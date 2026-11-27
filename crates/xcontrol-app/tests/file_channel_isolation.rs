//! FR-7.3 独立文件通道集成测试（docs/02 §4.6，XCP 1.2+；切片 b-net/b-app）。
//!
//! 1. **三通道并存**：控制连接（静默）/ 剪贴板通道（`ChannelBind`）/ 文件通道
//!    （`FileBind`）经 [`classify_incoming`] 分派为三个角色，同一监听端口、
//!    独立 Noise 握手；`FileBind` 会话不匹配 = 拒绝断开（不扰动已有通道）；
//! 2. **真实文件拉流端到端**：host 发布真实临时文件 → 对端通告镜像清单 →
//!    host 拉取（PullReq）→ 对端按 serve 语义回 chunk/end → host 暂存落盘 +
//!    blake3 校验 + **挂板**（mac 实测路径；hub.verified / mounted 指纹 /
//!    暂存内容逐字节断言）。
//!
//! hub 为进程级单例（同生产形态：host/client 各一个进程）——本测试以
//! raw-frame 对端驱动，host 侧 hub 单角色使用；两用例以 hub 测试互斥锁串行。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::Mutex;
use xcontrol_app::clip_channel::{CLIP_BIND_PEEK_WINDOW, IncomingClass, classify_incoming};
use xcontrol_app::file_channel::{FILE_MIN_MINOR, lock_hub, staging_dir_for_test};
use xcontrol_core::file_tx::content_digest;
use xcontrol_net::{HandshakeThrottle, ServerOptions, ThrottleConfig, serve};
use xcontrol_proto::file::{FileBind, FileChunk, FileEnd, FilePullReq};
use xcontrol_proto::{FileEntry, FileEntryKind, FileMsg, FileRefAnnounce, Message as _};

/// hub 测试互斥（进程级单例的跨用例串行；异步锁——跨 await 持有）。
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

/// 三通道并存 + FileBind 会话校验（分派面全矩阵）。
#[tokio::test]
async fn classify_dispatches_control_clip_file_and_rejects() {
    let _hub_guard = HUB_LOCK.lock().await;
    let (mut server, addr) = test_server().await;
    let session_id = [0x3Fu8; 16];
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());

    // ① 静默连接（控制形态）。② 剪贴板通道（ChannelBind）。
    // ③ 文件通道（FileBind，会话匹配）。④ 文件通道（会话不匹配）。
    let _control = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("控制连接握手");
    let mut clip_conn = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("剪贴板连接握手");
    let bind = xcontrol_proto::clip::ChannelBind { session_id };
    clip_conn
        .transport
        .send(&bind.encode_frame(0).expect("bind 编码"))
        .await
        .expect("bind 发送");
    let mut file_conn = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("文件连接握手");
    let file_bind = FileBind { session_id };
    file_conn
        .transport
        .send(&file_bind.encode_frame(0).expect("file bind 编码"))
        .await
        .expect("file bind 发送");
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

/// 真实文件拉流端到端：host 发布临时文件 → 对端通告镜像 → 拉取 → chunk/end
/// 回传 → 暂存 + 校验 + 挂板。
#[tokio::test]
async fn real_file_pull_flow_stages_and_mounts() {
    let _hub_guard = HUB_LOCK.lock().await;
    let (mut server, addr) = test_server().await;
    let session_id = [0x4Fu8; 16];

    {
        let mut hub = lock_hub();
        hub.reset_for_new_session();
        hub.set_session_minor(FILE_MIN_MINOR);
    }

    // —— host 真实临时文件 → 本机清单发布（b-app 检测面的测试等价入口）——
    let content: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let dir = std::env::temp_dir().join("xcontrol-file-e2e");
    std::fs::create_dir_all(&dir).expect("临时目录");
    let path = dir.join("e2e-样本.bin");
    std::fs::write(&path, &content).expect("临时文件");
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .expect("文件名");
    let now_ms = 1_700_000_000_000;
    lock_hub().publish_local_manifest(
        vec![entry(&file_name, content.len() as u64)],
        vec![path.clone()],
        now_ms,
    );
    let transfer_id = lock_hub()
        .outbound()
        .map(|announce| announce.transfer_id)
        .expect("发布后有清单");

    // —— 对端（raw-frame）：握手 + FileBind → host 分派文件通道 ——
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

    // —— 元数据面：对端通告同清单（镜像 host 发布条目）→ host pending 槽 ——
    let mirror = FileRefAnnounce {
        transfer_id,
        origin_id: [0x22; 16],
        origin_seq: 1,
        entries: vec![entry(&file_name, content.len() as u64)],
        total_bytes: content.len() as u64,
        copied_at_ms: now_ms,
    };
    peer.transport
        .send(&mirror.encode_frame(1).expect("announce 编码"))
        .await
        .expect("announce 发送");
    let pending_ready = wait_for(Duration::from_secs(5), || {
        lock_hub()
            .pending()
            .is_some_and(|slot| slot.transfer_id == transfer_id)
    })
    .await;
    assert!(pending_ready, "announce 应在 5s 内入 pending 槽");

    // —— 拉流面：host 发起拉取 → 对端按 serve 语义回 chunk×2 + end ——
    lock_hub().request_pull(transfer_id, 0);
    // 收帧直到 PullReq（host 泵会先推自身 outbound announce——元数据面正常
    // 行为，测试侧消化；出站拍 200ms + 逐帧独立发送）
    let mut saw_pull_req = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !saw_pull_req && tokio::time::Instant::now() < deadline {
        let mut wire_buf = Vec::new();
        let received =
            tokio::time::timeout(Duration::from_secs(1), peer.transport.recv(&mut wire_buf)).await;
        let Ok(Ok(())) = received else { continue };
        let msgs = decode_file_frames(&wire_buf);
        saw_pull_req = msgs.iter().any(|msg| matches!(
            msg,
            FileMsg::PullReq(FilePullReq { transfer_id: tid, entry: 0, .. }) if *tid == transfer_id
        ));
    }
    assert!(saw_pull_req, "对端应在 5s 内收到 PullReq{{transfer, 0}}");

    // 按 65472B 有效分块回传（seq 单调 + 末尾 end，blake3 全量）
    let mut seq: u32 = 0;
    let mut chunk_frames = Vec::new();
    for chunk_data in content.chunks(65472) {
        chunk_frames.push(
            FileChunk {
                transfer_id,
                seq,
                data: chunk_data.to_vec(),
            }
            .encode_frame(seq + 10)
            .expect("chunk 编码"),
        );
        seq += 1;
    }
    let end_frame = FileEnd {
        transfer_id,
        entry: 0,
        blake3: content_digest(&content),
    }
    .encode_frame(seq + 10)
    .expect("end 编码");
    chunk_frames.push(end_frame);
    for frame in &chunk_frames {
        peer.transport.send(frame).await.expect("chunk/end 发送");
    }

    // —— host 端断言：verified + 暂存内容逐字节一致 + 挂板指纹登记 ——
    let verified = wait_for(Duration::from_secs(5), || {
        lock_hub()
            .verified
            .iter()
            .any(|(tid, entry)| *tid == transfer_id && *entry == 0)
    })
    .await;
    assert!(verified, "blake3 校验应在 5s 内通过并记录 verified");
    // 树形暂存（切片 d R1）：staging/{tid}/{rel}
    let staged_path = staging_dir_for_test()
        .join(transfer_id.to_string())
        .join(&file_name);
    let staged = std::fs::read(&staged_path).expect("暂存文件存在");
    assert_eq!(staged, content, "暂存内容与源逐字节一致");
    // 挂板按平台分叉（docs/05 §0.2）：mac = write_file_urls 实测路径 + 回环
    // 防线指纹登记；其余平台挂板未实装（文件已暂存，路径提示）。
    // 挂板发生在收敛拍（tick 200ms）——轮询等待（mac 实测路径）
    #[cfg(feature = "macos")]
    {
        let want = Some(xcontrol_core::file_ref::manifest_fingerprint(
            &mirror.entries,
        ));
        let mounted = wait_for(Duration::from_secs(5), || lock_hub().mounted_fp() == want).await;
        assert!(mounted, "全条目完成应在 5s 内触发挂板并登记回环防线指纹");
    }
    #[cfg(not(feature = "macos"))]
    assert_eq!(
        lock_hub().mounted_fp(),
        None,
        "非 mac 平台挂板未实装（docs/05 §0.2 pending）：文件已暂存，指纹不登记"
    );

    guard.close();
    lock_hub().reset_for_new_session();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&staged_path);
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

/// 目录树端到端（US-FT-4，切片 d R1 FileExpand 接线）：host 发布目录清单 →
/// 对端通告镜像 → 拉取 Dir 条目（PullReq → Expand → 子条目续拉 chunk/end）
/// → 暂存树逐字节断言 → 挂板（目录壳 + 子树随目录带出）。
#[tokio::test]
async fn directory_tree_pull_expands_and_mounts() {
    let _hub_guard = HUB_LOCK.lock().await;
    let (mut server, addr) = test_server().await;
    let session_id = [0x5Fu8; 16];

    {
        let mut hub = lock_hub();
        hub.reset_for_new_session();
        hub.set_session_minor(FILE_MIN_MINOR);
    }

    // —— host 真实目录树：dir/{file1.txt, sub/file2.txt} ——
    let content1: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let content2 = b"nested file content".to_vec();
    let tree = std::env::temp_dir().join("xcontrol-dir-e2e");
    let dir_path = tree.join("目录树");
    std::fs::create_dir_all(dir_path.join("sub")).expect("目录树建立");
    std::fs::write(dir_path.join("file1.txt"), &content1).expect("file1");
    std::fs::write(dir_path.join("sub").join("file2.txt"), &content2).expect("file2");
    let dir_name = dir_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .expect("目录名");

    // host 发布目录清单（检测面等价入口；Dir size=0——Expand 展开面）
    lock_hub().publish_local_manifest(
        vec![FileEntry {
            name: dir_name.clone(),
            size: 0,
            mtime_ms: 0,
            kind: FileEntryKind::Dir,
            children: 0,
        }],
        vec![dir_path.clone()],
        1_700_000_000_000,
    );
    let transfer_id = lock_hub()
        .outbound()
        .map(|announce| announce.transfer_id)
        .expect("发布后有清单");
    // Dir 条目登记进 serve（清单 announce 的条目 kind=Dir）
    lock_hub().set_session_minor(FILE_MIN_MINOR);

    // 对端（raw-frame）：握手 + FileBind → 分派
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

    // 对端通告镜像（含 Dir 条目，children=0 = 未统计）
    let mirror = FileRefAnnounce {
        transfer_id,
        origin_id: [0x22; 16],
        origin_seq: 1,
        entries: vec![FileEntry {
            name: dir_name.clone(),
            size: 0,
            mtime_ms: 0,
            kind: FileEntryKind::Dir,
            children: 0,
        }],
        total_bytes: 0,
        copied_at_ms: 1_700_000_000_000,
    };
    peer.transport
        .send(&mirror.encode_frame(1).expect("announce 编码"))
        .await
        .expect("announce 发送");
    let pending_ready = wait_for(Duration::from_secs(5), || {
        lock_hub()
            .pending()
            .is_some_and(|slot| slot.transfer_id == transfer_id)
    })
    .await;
    assert!(pending_ready, "announce 应在 5s 内入 pending 槽");

    // —— 拉取会话：host 拉目录条目（Expand 展开）→ 子条目续拉 ——
    lock_hub().request_pull(transfer_id, 0);

    // 对端（源端模拟）：收 PullReq → 回 Expand；收 PullReq(文件子条目) →
    // 回 chunk+end（读真实文件）
    let mut seq: u32 = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let mut wire_buf = Vec::new();
        match tokio::time::timeout(Duration::from_secs(1), peer.transport.recv(&mut wire_buf)).await
        {
            Ok(Ok(())) => {}
            _ => continue,
        }
        for msg in decode_file_frames(&wire_buf) {
            if let FileMsg::PullReq(FilePullReq { entry, .. }) = msg {
                // 条目 0 = 目录（镜像清单唯一条目）→ Expand（子树两文件）
                // 子条目 1 = 目录/file1.txt、2 = 目录/sub/file2.txt
                // 目录条目 → Expand（镜像树：0=目录、1=file1、2=sub、
                // 3=sub/file2——sub 的 Expand 在 PullReq(2) 时给出 base=4
                // 的 file2 副本条目，接收端 rel 由父链还原）
                let expand_for = |parent: u32, base: u32| xcontrol_proto::file::FileExpand {
                    transfer_id,
                    parent,
                    base,
                    entries: match (parent, base) {
                        (0, 1) => vec![
                            FileEntry {
                                name: "file1.txt".to_owned(),
                                size: content1.len() as u64,
                                mtime_ms: 0,
                                kind: FileEntryKind::File,
                                children: 0,
                            },
                            FileEntry {
                                name: "sub".to_owned(),
                                size: 0,
                                mtime_ms: 0,
                                kind: FileEntryKind::Dir,
                                children: 0,
                            },
                        ],
                        (2, 3) => vec![FileEntry {
                            name: "file2.txt".to_owned(),
                            size: content2.len() as u64,
                            mtime_ms: 0,
                            kind: FileEntryKind::File,
                            children: 0,
                        }],
                        _ => panic!("意外 Expand 请求 {parent}/{base}"),
                    },
                };
                if entry == 0 {
                    let expand = expand_for(0, 1);
                    peer.transport
                        .send(&expand.encode_frame(seq).expect("expand 编码"))
                        .await
                        .expect("expand 发送");
                    seq += 1;
                } else if entry == 2 {
                    let expand = expand_for(2, 3);
                    peer.transport
                        .send(&expand.encode_frame(seq).expect("expand 编码"))
                        .await
                        .expect("expand 发送");
                    seq += 1;
                } else {
                    // 文件子条目：下标 1 = file1、4 = sub/file2（镜像树）
                    let content = match entry {
                        1 => &content1,
                        3 => &content2,
                        other => panic!("意外拉取下标 {other}"),
                    };
                    for (chunk_seq, piece) in content.chunks(65472).enumerate() {
                        let chunk = FileChunk {
                            transfer_id,
                            seq: chunk_seq as u32,
                            data: piece.to_vec(),
                        };
                        peer.transport
                            .send(&chunk.encode_frame(seq).expect("chunk 编码"))
                            .await
                            .expect("chunk 发送");
                        seq += 1;
                    }
                    let end = FileEnd {
                        transfer_id,
                        entry,
                        blake3: content_digest(content),
                    };
                    peer.transport
                        .send(&end.encode_frame(seq).expect("end 编码"))
                        .await
                        .expect("end 发送");
                    seq += 1;
                }
            }
        }
        // host 侧收敛完成（挂板登记）即退出
        if lock_hub().mounted_fp().is_some() {
            break;
        }
    }

    // —— host 端断言：暂存树逐字节 + 挂板 ——
    let tree_root = staging_dir_for_test().join(transfer_id.to_string());
    let staged1 = tree_root.join(format!("{dir_name}/file1.txt"));
    let staged2 = tree_root.join(format!("{dir_name}/sub/file2.txt"));
    let staged1_ok = wait_for(Duration::from_secs(5), || {
        std::fs::read(&staged1).is_ok_and(|data| data == content1)
    })
    .await;
    assert!(staged1_ok, "file1 暂存内容逐字节一致");
    let staged2_ok = wait_for(Duration::from_secs(5), || {
        std::fs::read(&staged2).is_ok_and(|data| data == content2)
    })
    .await;
    assert!(staged2_ok, "file2（子目录）暂存内容逐字节一致");
    // 挂板按平台分叉（docs/05 §0.2）：mac 实测路径登记指纹；其余平台挂板
    // 未实装（文件已暂存）——收敛本身以暂存树断言承接。
    #[cfg(feature = "macos")]
    let mounted = wait_for(Duration::from_secs(5), || lock_hub().mounted_fp().is_some()).await;
    #[cfg(feature = "macos")]
    assert!(mounted, "全条目收敛应触发挂板");

    guard.close();
    lock_hub().reset_for_new_session();
    let _ = std::fs::remove_dir_all(&tree);
    let _ = std::fs::remove_dir_all(staging_dir_for_test().join(transfer_id.to_string()));
}
