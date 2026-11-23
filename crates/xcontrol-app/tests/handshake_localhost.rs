//! FR-1.4 localhost 握手集成测试（不经 UI 直测 `session_rt`）。
//!
//! 覆盖（PRD Requirement 4 / FR-1.4 验收标准）：
//! 1. 正确安全码：host/client 双实例经真实 TCP + XXpsk0 握手成功——双方进入
//!    `Running`、会话标识一致（docs/02 §3.3）、对端名可见；
//! 2. 错误安全码：握手在握手阶段被拒绝（唯一折叠变体 [`NetError::HandshakeFailed`]），
//!    连接关闭、主控无交付；随后正确安全码仍可正常握手（服务健在、无污染）。
//!
//! 限速纪律：测试用缩小版 [`ThrottleConfig`]（阈值内不触发退避；min_interval
//! 缩小避免 Cooldown 干扰），与 `xcontrol-net` serve 测试同形。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use xcontrol_app::session_rt::{
    BoundHost, ClientOptions, HostOptions, SessionEnd, client_dial_once, client_session,
    host_listen,
};
use xcontrol_app::state::{AppState, ConnState, CoreCommand, StateBus};
use xcontrol_config::Role;
use xcontrol_core::session::Identity;
use xcontrol_net::{HandshakeThrottle, NetError, ThrottleConfig};

/// 测试假码派生（docs/08 §4：fixture/测试永不用真实安全码；argon2 m=19MiB，
/// 每码进程内只派生一次）。
fn psk(code: &'static str) -> [u8; 32] {
    static GOOD: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    static BAD: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let slot = match code {
        "it-good" => &GOOD,
        _ => &BAD,
    };
    // 注：本 helper 非 `#[test]` 函数（clippy allow-expect-in-tests 不覆盖），
    // 失败路径用显式 `panic!`（与 net tests 的 `must` helper 同形）。
    *slot.get_or_init(|| match xcontrol_net::derive_psk(code, &[0x11; 16]) {
        Ok(psk) => psk,
        Err(error) => panic!("固定输入派生失败：{error}"),
    })
}

/// 测试限速参数（阈值/基数缩小保证用例毫秒级；min_interval=1ms 避免
/// 主动断言与冷却语义纠缠）。
fn fast_throttle() -> ThrottleConfig {
    ThrottleConfig {
        max_failures: 5,
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        min_interval: Duration::from_millis(1),
    }
}

/// 演示实例 UUID（M2 双实例共享同一 UUID 派生 PSK——与 main.rs 的
/// DEMO_INSTANCE_UUID 同值语义；测试内独立常量避免依赖 bin 私有项）。
fn identity(name: &str) -> Identity {
    Identity {
        name: name.to_owned(),
        screen_name: format!("{name}-screen"),
        instance_id: [0x11; 16],
        caps: xcontrol_proto::Caps::UNICODE.union(xcontrol_proto::Caps::ABS_POINTER),
        salt: xcontrol_net::derive_salt(&[0x11; 16]),
    }
}

fn bus_for(role: Role) -> Arc<StateBus> {
    // StateBus::new 已返回 Arc（UI/核心两侧共享同一总线）。
    StateBus::new(AppState {
        role,
        ..AppState::default()
    })
}

/// 轮询快照流直到谓词成立（带总超时兜底，防测试挂死）。
async fn wait_for(
    rx: &mut watch::Receiver<AppState>,
    deadline: Duration,
    mut pred: impl FnMut(&AppState) -> bool,
) -> AppState {
    let started = std::time::Instant::now();
    loop {
        {
            let state = rx.borrow();
            if pred(&state) {
                return state.clone();
            }
        }
        assert!(
            started.elapsed() < deadline,
            "等待快照状态超时（当前 {:?}）",
            rx.borrow().conn
        );
        // 200ms 节流的推送节拍：changed() 等下一次推送，超时一拍再查。
        let _ = tokio::time::timeout(Duration::from_millis(300), rx.changed()).await;
    }
}

/// FR-1.4 成功路径：localhost 双实例（真实 TCP + XXpsk0）握手成功，
/// 双方 Running、会话标识一致、对端名可见、报文计数推进。
#[tokio::test]
async fn correct_code_handshake_reaches_running() {
    let good = psk("it-good");
    let host_bus = bus_for(Role::Host);
    let client_bus = bus_for(Role::Client);
    let mut host_rx = host_bus.subscribe();
    let mut client_rx = client_bus.subscribe();
    let (_host_tx, host_cmd_rx) = mpsc::channel(16);
    let (_client_tx, client_cmd_rx) = mpsc::channel(16);

    let bound = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: good,
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: Vec::new(),
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let addr: SocketAddr = bound.local_addr;
    tokio::spawn(bound.run_session(
        Arc::clone(&host_bus),
        host_cmd_rx,
        None,
        Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default())),
    ));
    tokio::spawn(client_session(
        ClientOptions {
            addr,
            psk: good,
            identity: identity("it-client"),
            throttle: fast_throttle(),
            keymap: xcontrol_config::KeymapConfig::default(),
            hotkeys: xcontrol_config::HotkeysConfig::default(),
        },
        Arc::clone(&client_bus),
        client_cmd_rx,
        Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default())),
    ));

    let host_state = wait_for(&mut host_rx, Duration::from_secs(10), |state| {
        matches!(state.conn, ConnState::Running { paused: false })
    })
    .await;
    let client_state = wait_for(&mut client_rx, Duration::from_secs(10), |state| {
        matches!(state.conn, ConnState::Running { paused: false })
    })
    .await;

    // 会话标识一致（docs/02 §3.3）且非空。
    let host_id = host_state.session_id_hex.expect("host 会话 id");
    let client_id = client_state.session_id_hex.expect("client 会话 id");
    assert_eq!(host_id, client_id, "双方会话标识一致");
    assert_eq!(host_id.len(), 32, "16B hex");

    // 对端名经 Hello/HelloBack 交换可见。
    assert_eq!(client_state.peer_name.as_deref(), Some("it-host"));
    assert_eq!(host_state.peer_name.as_deref(), Some("it-client"));

    // 报文计数推进（协商帧 + 心跳已过线）。
    assert!(host_state.packets_out >= 1 && host_state.packets_in >= 1);
    assert!(client_state.packets_out >= 1 && client_state.packets_in >= 1);
    assert!(host_state.last_error.is_none());
    assert!(client_state.last_error.is_none());
}

/// FR-1.4 拒绝路径：错误安全码在握手阶段被拒（折叠变体 + 连接关闭 +
/// 主控无交付），快照记录断开原因；随后正确安全码正常握手（服务健在）。
#[tokio::test]
async fn wrong_code_rejected_then_correct_code_recovers() {
    let good = psk("it-good");
    let host_bus = bus_for(Role::Host);
    let host_rx = host_bus.subscribe();
    let (_host_tx, host_cmd_rx) = mpsc::channel(16);

    let bound: BoundHost = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: good,
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: Vec::new(),
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let addr = bound.local_addr;
    tokio::spawn(bound.run_session(
        Arc::clone(&host_bus),
        host_cmd_rx,
        None,
        Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default())),
    ));

    // ① 错码从机：握手失败（唯一折叠变体），根因保留 NetError 可 downcast。
    //    （单次拨号锚点：M3 起手动模式包 FR-5.3 重连循环——错码断言用轮内
    //    原子操作，见 session_rt::client_dial_once 文档。）
    let bad_bus = bus_for(Role::Client);
    let mut bad_rx = bad_bus.subscribe();
    let mut bad_throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());
    let result = client_dial_once(
        &ClientOptions {
            addr,
            psk: psk("it-bad"),
            identity: identity("it-bad-client"),
            throttle: fast_throttle(),
            keymap: xcontrol_config::KeymapConfig::default(),
            hotkeys: xcontrol_config::HotkeysConfig::default(),
        },
        &mut bad_throttle,
        &bad_bus,
    )
    .await;
    let error = result.expect_err("错码握手必须失败");
    assert!(
        error
            .chain()
            .any(|cause| cause.downcast_ref::<NetError>() == Some(&NetError::HandshakeFailed)),
        "折叠为唯一握手失败变体：{error:#}"
    );

    // 错码从机快照：进入 Disconnected 且携带握手失败文案（FR-1.4 记录日志）。
    let bad_state = wait_for(&mut bad_rx, Duration::from_secs(2), |state| {
        matches!(state.conn, ConnState::Disconnected { .. })
    })
    .await;
    let ConnState::Disconnected { reason } = &bad_state.conn else {
        panic!("应为 Disconnected：{:?}", bad_state.conn);
    };
    assert!(reason.contains("握手失败"), "断开原因含握手失败：{reason}");

    // ② 主控无交付：宽限窗内未进入 Running（连接已断开，未进入会话层）。
    let quiet = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let state = host_rx.borrow().clone();
            if matches!(state.conn, ConnState::Running { .. }) {
                panic!("错码连接不得交付（对端只见断开）");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(quiet.is_err(), "宽限窗内主控保持非 Running");

    // ③ 正确安全码随即成功：服务未被失败握手污染。
    let good_bus = bus_for(Role::Client);
    let mut good_rx = good_bus.subscribe();
    let (_good_tx, good_cmd_rx) = mpsc::channel(16);
    tokio::spawn(client_session(
        ClientOptions {
            addr,
            psk: good,
            identity: identity("it-client"),
            throttle: fast_throttle(),
            keymap: xcontrol_config::KeymapConfig::default(),
            hotkeys: xcontrol_config::HotkeysConfig::default(),
        },
        Arc::clone(&good_bus),
        good_cmd_rx,
        Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default())),
    ));
    let good_state = wait_for(&mut good_rx, Duration::from_secs(10), |state| {
        matches!(state.conn, ConnState::Running { paused: false })
    })
    .await;
    assert_eq!(
        good_state.peer_name.as_deref(),
        Some("it-host"),
        "恢复路径协商完成"
    );
}

/// 空闲期 `RuntimeConfigUpdate` 的 net 段重建判定（11-18-m4-ui check 补测，
/// docs/06 §4.1）：发现层开关变化 → `SessionEnd::ConfigReload` 让位 supervisor
/// 重建；端口/发现层均不变 → 留在 accept 循环（通道关闭收尾）。
#[tokio::test]
async fn idle_config_update_reloads_on_discovery_change_only() {
    // ① 发现层变化（端口同）→ ConfigReload。
    let bound = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: psk("it-good"),
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: Vec::new(),
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let port = bound.local_addr.port();
    let bus = bus_for(Role::Host);
    let (tx, rx) = mpsc::channel(16);
    let config_cell = Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default()));
    let handle =
        tokio::spawn(bound.run_session(Arc::clone(&bus), rx, None, Arc::clone(&config_cell)));

    let mut changed = xcontrol_config::Config::default();
    changed.net.port = port;
    changed.net.discovery = vec![xcontrol_config::DiscoveryMethod::Scan];
    tx.send(CoreCommand::RuntimeConfigUpdate(Box::new(changed)))
        .await
        .expect("命令通道存活");
    drop(tx); // ConfigReload 返回后无其他收尾路径依赖本发送端
    let (end, _rx_back) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("accept 循环在超时内让位")
        .expect("run_session 任务不 panic")
        .expect("run_session 不上抛错误");
    assert_eq!(end, SessionEnd::ConfigReload, "发现层变化触发重建判定");

    // ①' 白名单变化（FR-5.4）→ 同样上抛 ConfigReload（serve 层白名单表在
    // listen 时解析固定——变更必须重新绑定）。
    let bound = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: psk("it-good"),
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: Vec::new(),
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let port = bound.local_addr.port();
    let bus = bus_for(Role::Host);
    let (tx, rx) = mpsc::channel(16);
    let config_cell = Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default()));
    let handle =
        tokio::spawn(bound.run_session(Arc::clone(&bus), rx, None, Arc::clone(&config_cell)));
    let mut allowlisted = xcontrol_config::Config::default();
    allowlisted.net.port = port;
    allowlisted.net.allowlist = vec!["127.0.0.0/8".to_owned()];
    tx.send(CoreCommand::RuntimeConfigUpdate(Box::new(allowlisted)))
        .await
        .expect("命令通道存活");
    drop(tx);
    let (end, _rx_back) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("accept 循环在超时内让位")
        .expect("run_session 任务不 panic")
        .expect("run_session 不上抛错误");
    assert_eq!(end, SessionEnd::ConfigReload, "白名单变化触发重新绑定判定");

    // ①'' 安全码引用变化（docs/07 §3.3 换码协议）→ 上抛 ConfigReload
    //（server 内嵌旧 PSK，必须让位 supervisor 按新码重建）。
    let bound = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: psk("it-good"),
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: Vec::new(),
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let port = bound.local_addr.port();
    let bus = bus_for(Role::Host);
    let (tx, rx) = mpsc::channel(16);
    let config_cell = Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default()));
    let handle = tokio::spawn(bound.run_session(Arc::clone(&bus), rx, None, config_cell));
    let mut recoded = xcontrol_config::Config::default();
    recoded.net.port = port;
    recoded.security_code_ref = "keychain:xcontrol-rotated".to_owned();
    tx.send(CoreCommand::RuntimeConfigUpdate(Box::new(recoded)))
        .await
        .expect("命令通道存活");
    drop(tx);
    let (end, _rx_back) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("accept 循环在超时内让位")
        .expect("run_session 任务不 panic")
        .expect("run_session 不上抛错误");
    assert_eq!(end, SessionEnd::ConfigReload, "换码触发全量重建判定");

    // ② 端口/发现层均不变 → 留在 accept 循环（通道关闭 → SessionClosed）。
    let bound = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: psk("it-good"),
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: Vec::new(),
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let port = bound.local_addr.port();
    let bus = bus_for(Role::Host);
    let (tx, rx) = mpsc::channel(16);
    let config_cell = Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default()));
    let handle = tokio::spawn(bound.run_session(Arc::clone(&bus), rx, None, config_cell));

    let unchanged = xcontrol_config::NetConfig::default();
    let mut same = xcontrol_config::Config::default();
    same.net.port = port;
    same.net.discovery = unchanged.discovery;
    tx.send(CoreCommand::RuntimeConfigUpdate(Box::new(same)))
        .await
        .expect("命令通道存活");
    // 命令已被消费（留在循环）：关通道收尾，断言非 ConfigReload。
    drop(tx);
    let (end, _rx_back) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("accept 循环在超时内收尾")
        .expect("run_session 任务不 panic")
        .expect("run_session 不上抛错误");
    assert_eq!(end, SessionEnd::SessionClosed, "net 段不变不触发重建判定");
}

/// FR-5.4 白名单（app 会话层集成）：allowlist 不含回环来源 → 正确安全码的
/// 从机连接在握手前被 serve 层断开（折叠 [`NetError::HandshakeFailed`]——
/// 对端语义与错码一致，docs/02 §0）；主控无交付；放宽白名单后同一来源恢复
/// 全链路（授权路径）。net 层拒绝路径细粒度断言（NotInAllowlist 观测面/
/// 不计限速）见 xcontrol-net serve.rs 测试，此处锚定 host_listen 装配链。
#[tokio::test]
async fn allowlist_blocks_non_matching_source_and_allows_matching() {
    // ① 白名单仅 TEST-NET-3（本机不可路由）→ 回环来源被拒。
    let bound = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: psk("it-good"),
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: vec!["203.0.113.0/24".to_owned()],
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let addr = bound.local_addr;
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());
    let result = client_dial_once(
        &client_options(addr, "it-client"),
        &mut throttle,
        &bus_for(Role::Client),
    )
    .await;
    assert!(
        matches!(result.as_ref(), Err(error) if error.to_string().contains("握手失败") || error.chain().any(|c| c.downcast_ref::<NetError>().is_some_and(|e| matches!(e, NetError::HandshakeFailed)))),
        "白名单外来源被拒（折叠语义）：{result:?}"
    );

    // ② 白名单覆盖回环 → 正确安全码全链路成功（授权路径不受影响）。
    let bound = host_listen(HostOptions {
        bind: "127.0.0.1:0".parse().expect("常量地址"),
        psk: psk("it-good"),
        identity: identity("it-host"),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
        allowlist: vec!["127.0.0.0/8".to_owned()],
        discovery: xcontrol_config::NetConfig::default().discovery,
    })
    .await
    .expect("主控监听就绪");
    let addr = bound.local_addr;
    let bus = bus_for(Role::Host);
    let mut host_rx = bus.subscribe();
    let (_host_tx, host_cmd_rx) = mpsc::channel(16);
    let config_cell = Arc::new(std::sync::Mutex::new(xcontrol_config::Config::default()));
    let session = tokio::spawn(bound.run_session(
        Arc::clone(&bus),
        host_cmd_rx,
        None,
        Arc::clone(&config_cell),
    ));
    let client_bus = bus_for(Role::Client);
    let mut client_rx = client_bus.subscribe();
    let (_client_tx, client_cmd_rx) = mpsc::channel(16);
    let client = tokio::spawn(client_session(
        client_options(addr, "it-client"),
        Arc::clone(&client_bus),
        client_cmd_rx,
        Arc::clone(&config_cell),
    ));
    let host_state = wait_for(&mut host_rx, Duration::from_secs(5), |state| {
        matches!(state.conn, ConnState::Running { .. })
    })
    .await;
    let client_state = wait_for(&mut client_rx, Duration::from_secs(5), |state| {
        matches!(state.conn, ConnState::Running { .. })
    })
    .await;
    assert_eq!(
        host_state.session_id_hex, client_state.session_id_hex,
        "白名单内来源：双方会话标识一致"
    );
    // 收尾（任务在命令通道关闭后自然退出）。
    client.abort();
    session.abort();
}

/// 从机侧选项装配（测试同构 helper：直连路径 + 缩小限速）。
fn client_options(addr: SocketAddr, name: &str) -> ClientOptions {
    ClientOptions {
        addr,
        psk: psk("it-good"),
        identity: identity(name),
        throttle: fast_throttle(),
        keymap: xcontrol_config::KeymapConfig::default(),
        hotkeys: xcontrol_config::HotkeysConfig::default(),
    }
}
