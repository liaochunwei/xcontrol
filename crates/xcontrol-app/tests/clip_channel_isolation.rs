//! FR-4.x 独立剪贴板通道集成测试（11-21 双连接裁定，docs/02 §0/§4.4）。
//!
//! 本任务根因的回归锚点：剪贴板大块流曾与输入/控制帧共用控制连接（同连接
//! 裁定）——队头阻塞使任何抖动断连 = 控制断连。回归双连接后：
//!
//! 1. **双连接并存**：控制连接（静默）与剪贴板通道（首帧 `ChannelBind`）经
//!    [`classify_incoming`] 分派为两个角色，同一监听端口、独立 Noise 握手；
//! 2. **剪贴板通道断开不影响控制**（核心验收）：host 侧关闭剪贴板通道后，
//!    控制连接的双向帧交换**立即继续成功**（承载层隔离——控制存活不依赖
//!    剪贴板通道）；
//! 3. **3s 独立重连自愈**：通道断开后 [`client_clip_channel_loop`] 按 3s
//!    退避独立重连，host 分派器再次产出剪贴板通道（控制连接全程未被触碰）。
//!
//! 限速纪律与握手测试同形（缩小版 [`ThrottleConfig`]）；`max_bytes` 侧
//! host=0（禁用壳：不写真实剪贴板）、client>0（通道拨号需启用面——只读
//! 基线，不产生落板写入）。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::watch;
use xcontrol_app::clip_channel::{
    CLIP_BIND_PEEK_WINDOW, CLIP_CHANNEL_RECONNECT_INTERVAL, ClipChannelGuard, IncomingClass,
    classify_incoming, client_clip_channel_loop,
};
use xcontrol_net::{Connection, HandshakeThrottle, ServerOptions, ThrottleConfig, serve};
use xcontrol_proto::Message as _;

/// 测试假码派生（docs/08 §4 假码纪律；每码进程内只派生一次）。
fn psk(code: &'static str) -> [u8; 32] {
    static PSK: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *PSK.get_or_init(|| match xcontrol_net::derive_psk(code, &[0x11; 16]) {
        Ok(psk) => psk,
        Err(error) => panic!("固定输入派生失败：{error}"),
    })
}

/// 测试辅助取值（非 `#[test]` 作用域，expect-in-tests 豁免不覆盖 helper——
/// 与 net serve tests / handshake_localhost 的 `must` 同形）。
#[track_caller]
fn must<T, E: std::fmt::Debug>(context: &str, result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"),
    }
}

/// 测试限速参数（毫秒级，与 net serve 测试同形）。
fn fast_throttle_config() -> ThrottleConfig {
    ThrottleConfig {
        max_failures: 5,
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        min_interval: Duration::from_millis(1),
    }
}

/// 起一个测试主控（随机端口）。
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

/// host 侧接受并分派一条连接（带 5s 兜底超时防挂死）。
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

/// 双连接并存 + 杀剪贴板通道不影响控制 + 3s 独立重连自愈（核心验收回归）。
#[tokio::test]
async fn clip_channel_kill_leaves_control_alive_and_channel_self_heals() {
    let (mut server, addr) = test_server().await;
    let session_id = [0x7Fu8; 16];

    // ① 控制连接（控制面形态：握手后静默等主控先发 Hello——本测试不驱动
    //    会话状态机，只验证承载层隔离）。
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
    let mut control_client = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("控制连接握手");

    // ② 从机剪贴板通道循环（client_clip_channel_loop：独立握手 + ChannelBind
    //    首帧 + 泵；失败 3s 独立重连）。
    let (clip_stop_tx, clip_stop_rx) = watch::channel(false);
    let clip_loop = tokio::spawn(client_clip_channel_loop(
        addr,
        psk("it-good"),
        fast_throttle_config(),
        session_id,
        1024, // >0 = 启用面（泵只读基线；host 侧禁用壳不写真实剪贴板）
        clip_stop_rx,
    ));

    // ③ host 分派：第一条 = 控制连接（静默 → Control）；第二条 = 剪贴板通道
    //    （ChannelBind 会话匹配 → ClipChannel）。双连接并存（同一端口）。
    let mut host_control: Box<Connection> = match accept_and_classify(&mut server, session_id).await
    {
        IncomingClass::Control(connection) => connection,
        other => panic!("静默连接应分派为控制连接：{other:?}"),
    };
    let clip_guard: ClipChannelGuard = match accept_and_classify(&mut server, session_id).await {
        IncomingClass::ClipChannel(guard) => guard,
        other => panic!("ChannelBind 连接应分派为剪贴板通道：{other:?}"),
    };

    // ④ 通道存活期：控制连接双向帧交换正常（双连接互不干扰的基线）。
    host_control
        .transport
        .send(b"control-before-kill")
        .await
        .expect("host → client 控制帧发送");
    let mut out = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        control_client.transport.recv(&mut out),
    )
    .await
    .expect("控制帧在 5s 内送达")
    .expect("控制连接存活");
    assert_eq!(out, b"control-before-kill");

    // ⑤ 核心验收：**杀剪贴板通道**（host 侧关闭——模拟通道任意失败面）→
    //    控制连接的双向帧交换立即继续成功（控制存活不依赖剪贴板通道）。
    clip_guard.close();
    control_client
        .transport
        .send(b"control-after-kill")
        .await
        .expect("通道被杀后 client → host 控制帧发送（控制连接未受影响）");
    let mut out = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        host_control.transport.recv(&mut out),
    )
    .await
    .expect("通道被杀后控制帧在 5s 内送达")
    .expect("host 控制连接存活");
    assert_eq!(out, b"control-after-kill");

    // ⑥ 3s 独立重连自愈：客户端通道循环按 CLIP_CHANNEL_RECONNECT_INTERVAL
    //    重拨 → host 分派器再次产出剪贴板通道（新通道替换语义的同型验证；
    //    期间控制连接全程未被触碰——⑦ 直接复验）。
    let healed = tokio::time::timeout(
        CLIP_CHANNEL_RECONNECT_INTERVAL + Duration::from_secs(5),
        accept_and_classify(&mut server, session_id),
    )
    .await
    .expect("剪贴板通道在退避窗口 + 5s 内自愈");
    let healed_guard = match healed {
        IncomingClass::ClipChannel(guard) => guard,
        other => panic!("重连连接应分派为剪贴板通道：{other:?}"),
    };

    // ⑦ 自愈后控制连接仍然存活（第三轮双向帧交换）。
    healed_guard.close();
    host_control
        .transport
        .send(b"control-after-heal")
        .await
        .expect("自愈后 host → client 控制帧发送");
    let mut out = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        control_client.transport.recv(&mut out),
    )
    .await
    .expect("自愈后控制帧在 5s 内送达")
    .expect("控制连接跨两次通道关闭存活");
    assert_eq!(out, b"control-after-heal");

    // ⑧ 收尾：停通道循环（stop 信号 + abort），控制连接交给 host 侧排空。
    let _ = clip_stop_tx.send(true);
    clip_loop.abort();
    let _ = clip_loop.await;
    host_control.transport.shutdown().await;
    control_client.transport.shutdown().await;
}

/// host 侧分派拒绝面：`ChannelBind` 会话不匹配 → 连接断开（Rejected），
/// 且**不影响**已建立的剪贴板通道与控制连接（后续客户端重连仍可接入）。
#[tokio::test]
async fn mismatched_bind_is_rejected_without_disturbing_existing_channel() {
    let (mut server, addr) = test_server().await;
    let session_id = [0x5Au8; 16];

    // 控制连接 + 正常剪贴板通道。
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
    let _control_client = serve::connect(addr, &psk("it-good"), &mut throttle)
        .await
        .expect("控制连接握手");
    let mut control: Box<Connection> = match accept_and_classify(&mut server, session_id).await {
        IncomingClass::Control(connection) => connection,
        other => panic!("静默连接应分派为控制连接：{other:?}"),
    };

    let (clip_stop_tx, clip_stop_rx) = watch::channel(false);
    let clip_loop = tokio::spawn(client_clip_channel_loop(
        addr,
        psk("it-good"),
        fast_throttle_config(),
        session_id,
        1024,
        clip_stop_rx,
    ));
    let _clip_guard: ClipChannelGuard = match accept_and_classify(&mut server, session_id).await {
        IncomingClass::ClipChannel(guard) => guard,
        other => panic!("ChannelBind 连接应分派为剪贴板通道：{other:?}"),
    };

    // 异会话 `ChannelBind`：握手合法但绑定校验不过 → Rejected（对端只见断开）。
    let mut throttle2 = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
    let mut rogue = serve::connect(addr, &psk("it-good"), &mut throttle2)
        .await
        .expect("异会话连接握手");
    let bad = xcontrol_proto::clip::ChannelBind {
        session_id: [0xEE; 16],
    };
    rogue
        .transport
        .send(&bad.encode_frame(0).expect("bind 编码"))
        .await
        .expect("bind 发送");
    let rogue_conn = tokio::time::timeout(Duration::from_secs(5), server.next_connection())
        .await
        .expect("连接交付")
        .expect("服务存活");
    let class = classify_incoming(rogue_conn, session_id, 0, CLIP_BIND_PEEK_WINDOW).await;
    assert!(
        matches!(class, IncomingClass::Rejected),
        "会话不匹配必须拒绝：{class:?}"
    );

    // 拒绝不波及既有通道：客户端通道存活（无需重连）→ 窗口内**无**新连接
    // 交付；控制连接仍可发送（存活面）。
    control
        .transport
        .send(b"control-still-alive")
        .await
        .expect("拒绝后控制帧发送");
    let quiet = tokio::time::timeout(Duration::from_millis(500), server.next_connection()).await;
    assert!(quiet.is_err(), "既有剪贴板通道存活：不得出现重连交付");

    let _ = control; // host 侧收尾（连接随 drop 关闭）
    drop(_clip_guard); // stop sender 随 guard drop → host 泵优雅退出
    let _ = clip_stop_tx.send(true);
    clip_loop.abort();
    let _ = clip_loop.await;
}
