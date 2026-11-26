//! FR-7.3 独立文件通道集成测试（docs/02 §4.6，XCP 1.2+；切片 b-net）。
//!
//! 1. **三通道并存**：控制连接（静默）/ 剪贴板通道（`ChannelBind`）/ 文件通道
//!    （`FileBind`）经 [`classify_incoming`] 分派为三个角色，同一监听端口、
//!    独立 Noise 握手；`FileBind` 会话不匹配 = 拒绝断开（不扰动已有通道）；
//! 2. **元数据面端到端**：对端通告 `FileRefAnnounce` → host 泵入 pending 槽
//!    （hub 投影，托盘数据源）；
//! 3. **拉流面端到端（软失败路径）**：host 发起 `FilePullReq` → 对端（b-net
//!    阶段无磁盘读取面）回 `FileError{SOURCE_GONE}` → host hub 记录条目级软
//!    失败（可重试语义，不断连）。
//!
//! hub 为进程级单例（同生产形态：host/client 各一个进程）——本测试以
//! raw-frame 对端驱动，host 侧 hub 单角色使用；两用例以 hub 测试互斥锁串行。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::Mutex;
use xcontrol_app::clip_channel::{CLIP_BIND_PEEK_WINDOW, IncomingClass, classify_incoming};
use xcontrol_app::file_channel::{FILE_MIN_MINOR, lock_hub};
use xcontrol_net::{HandshakeThrottle, ServerOptions, ThrottleConfig, serve};
use xcontrol_proto::file::{FileBind, FileErrCode, FileError, FilePullReq};
use xcontrol_proto::{FileEntry, FileEntryKind, FileMsg, FileRefAnnounce, Message as _};

/// hub 测试互斥（进程级单例的跨用例串行）。
static HUB_LOCK: Mutex<()> = Mutex::const_new(());

/// 测试假码派生（docs/08 §4 假码纪律；每码进程内只派生一次）。
fn psk(code: &'static str) -> [u8; 32] {
    static PSK: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *PSK.get_or_init(|| match xcontrol_net::derive_psk(code, &[0x11; 16]) {
        Ok(psk) => psk,
        Err(error) => panic!("固定输入派生失败：{error}"),
    })
}

#[track_caller]
fn must<T, E: std::fmt::Debug>(context: &str, result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"),
    }
}

fn fast_throttle_config() -> ThrottleConfig {
    ThrottleConfig {
        max_failures: 5,
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        min_interval: Duration::from_millis(1),
    }
}

async fn test_server() -> (xcontrol_net::Server, SocketAddr) {
    let opts = ServerOptions {
        bind: must("常量地址", "127.0.0.1:0".parse::<SocketAddr>()),
        throttle: fast_throttle_config(),
        allowlist: Vec::new(),
    };
    let server = must("serve 启动", serve::serve(opts, &psk("it-good")).await);
    let addr = server.local_addr();
    (server, addr)
}

async fn accept_and_classify(
    server: &mut xcontrol_net::Server,
    control_session_id: [u8; 16],
) -> IncomingClass {
    let connection = must(
        "连接在 5s 内交付",
        tokio::time::timeout(Duration::from_secs(5), server.next_connection()).await,
    );
    let Some(connection) = connection else {
        panic!("服务存活（next_connection 不得得 None）");
    };
    must(
        "分派在 5s 内完成",
        tokio::time::timeout(
            Duration::from_secs(5),
            classify_incoming(connection, control_session_id, 0, CLIP_BIND_PEEK_WINDOW),
        )
        .await,
    )
}

fn entry(name: &str, size: u64) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        size,
        mtime_ms: 0,
        kind: FileEntryKind::File,
        children: 0,
    }
}

fn announce(transfer_id: u32) -> FileRefAnnounce {
    FileRefAnnounce {
        transfer_id,
        origin_id: [0x22; 16],
        origin_seq: 1,
        entries: vec![entry("report.pdf", 2048)],
        total_bytes: 2048,
        copied_at_ms: 0,
    }
}

/// 三通道并存 + FileBind 会话校验（分派面全矩阵）。
#[tokio::test]
async fn classify_dispatches_control_clip_file_and_rejects() {
    let _hub_guard = HUB_LOCK.lock().await;
    let (mut server, addr) = test_server().await;
    let session_id = [0x3Fu8; 16];
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());

    // ① 静默连接（控制形态）。
    let _control = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("控制连接握手");
    // ② 剪贴板通道（ChannelBind）。
    let mut clip_conn = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("剪贴板连接握手");
    let bind = xcontrol_proto::clip::ChannelBind { session_id };
    clip_conn
        .transport
        .send(&bind.encode_frame(0).expect("bind 编码"))
        .await
        .expect("bind 发送");
    // ③ 文件通道（FileBind，会话匹配）。
    let mut file_conn = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("文件连接握手");
    let file_bind = FileBind { session_id };
    file_conn
        .transport
        .send(&file_bind.encode_frame(0).expect("file bind 编码"))
        .await
        .expect("file bind 发送");
    // ④ 文件通道（FileBind，会话不匹配）。
    let mut mismatch_conn = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("第四条连接握手");
    let bad_bind = FileBind {
        session_id: [0xEE; 16],
    };
    mismatch_conn
        .transport
        .send(&bad_bind.encode_frame(0).expect("file bind 编码"))
        .await
        .expect("file bind 发送");

    let first = accept_and_classify(&mut server, session_id).await;
    assert!(
        matches!(first, IncomingClass::Control(_)),
        "静默 = 控制连接"
    );

    let second = accept_and_classify(&mut server, session_id).await;
    match second {
        IncomingClass::ClipChannel(guard) => guard.close(),
        other => panic!("ChannelBind 应产出剪贴板通道：{other:?}"),
    }

    let third = accept_and_classify(&mut server, session_id).await;
    match third {
        IncomingClass::FileChannel(guard) => guard.close(),
        other => panic!("FileBind 应产出文件通道：{other:?}"),
    }

    let fourth = accept_and_classify(&mut server, session_id).await;
    assert!(
        matches!(fourth, IncomingClass::Rejected),
        "FileBind 会话不匹配 = 拒绝"
    );
}

/// 元数据面 + 拉流面端到端（软失败路径）。
#[tokio::test]
async fn announce_flows_to_pending_and_pull_soft_fails() {
    let _hub_guard = HUB_LOCK.lock().await;
    let (mut server, addr) = test_server().await;
    let session_id = [0x4Fu8; 16];

    // hub 复位 + 协商 minor 就绪（FILE_MIN_MINOR 门控的测试等价：host 泵的
    // 出站拍门控按 hub minor 判定；入站面不受门控——对端 1.2 客户端已 gate）。
    {
        let mut hub = lock_hub();
        hub.reset_for_new_session();
        hub.set_session_minor(FILE_MIN_MINOR);
    }

    // 对端（raw-frame 客户端形态）：独立握手 + FileBind 首帧。
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
    let mut peer = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("对端握手");
    let file_bind = FileBind { session_id };
    peer.transport
        .send(&file_bind.encode_frame(0).expect("file bind 编码"))
        .await
        .expect("file bind 发送");

    let guard = match accept_and_classify(&mut server, session_id).await {
        IncomingClass::FileChannel(guard) => guard,
        other => panic!("FileBind 应产出文件通道：{other:?}"),
    };

    // —— 元数据面：对端 announce → host pending 槽 ——
    let announce = announce(7);
    peer.transport
        .send(&announce.encode_frame(1).expect("announce 编码"))
        .await
        .expect("announce 发送");
    let pending = wait_for(Duration::from_secs(5), || {
        lock_hub()
            .pending()
            .is_some_and(|slot| slot.transfer_id == 7 && slot.entries.len() == 1)
    })
    .await;
    assert!(pending, "announce 应在 5s 内入 pending 槽");
    assert_eq!(
        lock_hub().pending().map(|slot| slot.total_bytes),
        Some(2048)
    );

    // —— 拉流面：host 发起拉取 → 对端回 SOURCE_GONE（b-net 无读取面）→
    //        条目级软失败记录（可重试语义）——
    {
        let mut hub = lock_hub();
        hub.request_pull(7, 0);
    }
    // 对端在 5s 内收到 PullReq（200ms 出站拍 + 传输延迟）
    let mut wire_buf = Vec::new();
    let received = must(
        "PullReq 在 5s 内到达对端",
        tokio::time::timeout(Duration::from_secs(5), peer.transport.recv(&mut wire_buf)).await,
    );
    received.expect("PullReq 读取");
    let msgs = decode_file_frames(&wire_buf);
    assert!(
        msgs.iter().any(|msg| matches!(
            msg,
            FileMsg::PullReq(FilePullReq {
                transfer_id: 7,
                entry: 0,
                ..
            })
        )),
        "对端应收到 PullReq{{7, 0}}：{msgs:?}"
    );

    // 对端回 FileError{SOURCE_GONE}（b-net 阶段：文件服务未接线的固定路径）
    let error = FileError {
        transfer_id: 7,
        entry: Some(0),
        code: FileErrCode::SOURCE_GONE,
    };
    peer.transport
        .send(&error.encode_frame(2).expect("error 编码"))
        .await
        .expect("error 发送");
    let failed = wait_for(Duration::from_secs(5), || !lock_hub().failures.is_empty()).await;
    assert!(failed, "软失败应在 5s 内记录到 hub");
    assert!(
        lock_hub().failures[0].contains("transfer 7"),
        "失败记录携带 transfer 上下文：{:?}",
        lock_hub().failures
    );

    guard.close();
    lock_hub().reset_for_new_session();
}

/// 轮询等待（50ms 拍，超时返回 false）。
async fn wait_for(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 明文流 → FileMsg（测试侧解码；与泵内同契约）。
fn decode_file_frames(stream: &[u8]) -> Vec<FileMsg> {
    use xcontrol_proto::frame::{decode_frame, is_file_kind};
    let mut msgs = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        let Ok(frame) = decode_frame(rest) else { break };
        let total = frame.total_len();
        if is_file_kind(frame.header.kind)
            && let Ok(Some(msg)) =
                xcontrol_proto::file::decode_file(frame.header.kind, frame.payload)
        {
            msgs.push(msg);
        }
        rest = &rest[total..];
    }
    msgs
}
