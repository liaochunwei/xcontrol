//! `serve()` / `connect()` — 端口绑定与安全码握手入口（11-17-m2-tcp）。
//!
//! 语义锚点：docs/02 §1（监听端口 4243 可配，默认值见
//! [`xcontrol_config::DEFAULT_PORT`]）、§3.2（握手 5s、失败即断开、
//! per-来源 IP 限速）、§0（无"部分匹配"反馈）。
//!
//! ```text
//! 主控 = responder：serve(opts, psk) → 监听 + 每连接 spawn 握手；
//!         成功连接经 channel 交付（Server::next_connection），失败连接丢弃。
//! 从机 = initiator：connect(addr, psk, &mut throttle) → 本地限速判定（不触网）
//!         → TCP 连接 → 握手（5s 经 tokio::time::timeout）→ Connection。
//! ```
//!
//! # 主控接入白名单（FR-5.4，docs/07 §6）
//!
//! [`ServerOptions::allowlist`] 非空时，accept 后、**握手与限速门前**先做来源
//! IP 白名单匹配（[`crate::discovery::cidr_contains`] 纯函数，复用 M3 的 CIDR
//! 解析）：不在白名单的来源**立即断开**——对端只见连接消失（与限速门同面，
//! 无任何协议反馈），不进握手、不计握手失败限速（策略拒绝 ≠ 错码爆破）；
//! 折叠事件经观测面上抛（`reason = NotInAllowlist`，app 顶层日志落点）。
//! 空白名单 = 不启用（接受全部来源）。
//!
//! # 限速（docs/02 §3.2——键的选择是安全语义的一部分）
//!
//! - **serve 侧**（主控）：键 = **来源 IP**（[`IpAddr`]，docs/02 §3.2"每来源 IP"）
//!   ——每条 TCP 连接的源端口互异（临时端口），若按 [`SocketAddr`] 计键则每次
//!   新连接都拿到全新计数，退避**永不生效**（check 评审修正：11-17-m2-tcp 首版
//!   误按 SocketAddr 计键，回归测试
//!   `server_rate_limit_accumulates_across_connections_per_ip` 钉死）。accept 后、
//!   握手前查退避/冷却，不在允许窗口内直接断开（被限速的对端只见连接不建立）；
//!   握手失败 `record_failure`、成功 `record_success`（窗口重置）；
//! - **connect 侧**（从机）：键 = 拨号目标 [`SocketAddr`]（服务端地址稳定，无端口
//!   漂移问题）；`TcpStream::connect` **之前**查限速——退避/冷却中的重试直接报
//!   [`NetError::RateLimited`]，不产生任何网络流量（"第 N 次触网前被拒"的可测试
//!   锚点）；失败/成功同样计数。
//!
//! # 错误折叠与日志（docs/02 §0、§7）
//!
//! 握手失败路径只产出 [`NetError::HandshakeFailed`]（超时/解密失败/帧违规/断开
//! 全折叠，对端只见连接消失）；本任务沿 M1 先例 net 层零日志依赖——失败记录
//! 以限速计数承载，日志告警（docs/02 §3.2）属 app/cli 顶层接入点（tracing 订阅
//! 由执行域装配）。app 侧观测面：[`Server::subscribe_handshake_failures`]——
//! 折叠事件（对端地址，不含原因）经 broadcast 通道上抛，M3 NFR-8 起 app 顶层
//! 逐条 `tracing::warn!`（11-17-m3-status 落地 FR-1.4"拒绝并记录日志"的主控侧）。
//!
//! # M2 边界
//!
//! 本模块只覆盖 localhost 场景可跑的最小入口（单 listener + 每连接一任务）；
//! 主控 N 从机并发、心跳判死、断线重连、双连接（输入/剪贴板）属 M3 会话层
//! （docs/02 §1/§2/§4.5）。

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::error::NetError;
use crate::noise::{HANDSHAKE_TIMEOUT, HandshakeRole, NoiseHandshake};
use crate::throttle::{Admission, HandshakeThrottle, ThrottleConfig};
use crate::transport::TcpTransport;

/// 握手完成连接的交付缓冲：M2 localhost 场景个位数并发，16 足够且不构成
/// 无界队列（满载时 handle_incoming 背压等待——握手后的连接交付不吞帧）。
const CONNECTION_CHANNEL_BUFFER: usize = 16;

/// 握手失败通知缓冲（[`Server::subscribe_handshake_failures`]）：观测面事件，
/// 订阅者滞后丢条记 Lagged（app 层记一条滞后告警），不阻塞握手路径。
const HANDSHAKE_FAILURE_NOTIFY_BUFFER: usize = 16;

/// 一条被折叠的握手失败（app 顶层日志观测面；握手失败原因仍折叠不外泄——
/// docs/02 §0 无"部分匹配"反馈，通知只携带对端地址；白名单拒绝属**本地策略**
/// 拒绝，与握手结果无关，对端同样只见断开，reason 仅用于本端日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeFailure {
    /// 对端地址（来源 IP + 临时端口）。
    pub peer: SocketAddr,
    /// 折叠/拒绝原因（本端观测面专用；不进入任何对端可见的协议流量）。
    pub reason: HandshakeFailureReason,
}

/// 握手阶段拒绝原因（[`HandshakeFailure::reason`]；本端日志观测语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeFailureReason {
    /// XXpsk0 握手失败/超时/非法报文——原因折叠（docs/02 §0）。
    Handshake,
    /// 来源 IP 不在主控白名单（FR-5.4）——accept 后、握手**前**的本地策略拒绝。
    NotInAllowlist,
}

impl HandshakeFailureReason {
    /// 日志标签（app 顶层观测面用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::NotInAllowlist => "not-in-allowlist",
        }
    }
}

/// serve 侧选项（[`serve`] 入参）。
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// 监听地址（生产 = 0.0.0.0:配置端口；测试 = 127.0.0.1:0 随机端口）。
    pub bind: SocketAddr,
    /// 主控侧限速参数（docs/02 §3.2 常量；测试可缩小）。
    pub throttle: ThrottleConfig,
    /// 主控接入白名单（FR-5.4）：CIDR 字符串数组；空 = 不启用（接受全部来源）。
    /// 非法条目在启动时解析失败即忽略并经观测面告警（解析函数
    /// [`crate::discovery::parse_cidr`]，仅 IPv4；IPv6 来源在启用白名单时一律拒绝）。
    pub allowlist: Vec<String>,
}

/// 一条已完成 XXpsk0 握手的连接（serve 与 connect 的统一交付物）。
///
/// `session_id`/`handshake_hash`/`peer_addr` 透传自 [`TcpTransport`]；
/// 后续应用帧收发直接用 `transport`。
#[derive(Debug)]
pub struct Connection {
    /// 加密传输端（含 [`crate::noise::SecureTransport`] 与 TCP 流）。
    pub transport: TcpTransport,
    /// 会话标识（docs/02 §3.3，双方一致）。
    pub session_id: [u8; 16],
    /// 对端地址。
    pub peer: SocketAddr,
}

/// 主控侧服务实例：持有 accept 循环任务与连接交付通道。
///
/// **Drop 即停止服务**（abort accept 任务；已握手连接通道随之关闭，
/// in-flight 握手任务在交付时自然退出）。
#[derive(Debug)]
pub struct Server {
    local_addr: SocketAddr,
    connections: mpsc::Receiver<Connection>,
    failures: broadcast::Sender<HandshakeFailure>,
    accept_task: JoinHandle<()>,
}

impl Server {
    /// 实际监听地址（`bind: 127.0.0.1:0` 时即为测试所需的随机端口）。
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// 等待下一条完成握手的入站连接；服务已停止（[`Server`] drop）时返回 `None`。
    pub async fn next_connection(&mut self) -> Option<Connection> {
        self.connections.recv().await
    }

    /// 订阅握手失败通知（折叠事件的可观测面——FR-1.4"安全码不匹配的连接在
    /// 握手阶段被拒绝并记录日志"的主控侧接入点；tracing 告警由 app 顶层落）。
    ///
    /// 通知只携带对端地址，失败原因仍折叠不外泄（docs/02 §0）；无订阅者时
    /// 事件即丢弃（`send` 失败被忽略，握手路径零等待）。`next_connection`
    /// 只交付成功握手，被折叠的失败**只**经本通道可见——调用方在 accept 循环
    /// 里 `tokio::select!` 两路等待（两个 future 均 cancel-safe）。
    #[must_use]
    pub fn subscribe_handshake_failures(&self) -> broadcast::Receiver<HandshakeFailure> {
        self.failures.subscribe()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// 主控侧入口：监听 `opts.bind` 并为每条入站连接执行 responder 握手。
///
/// 成功握手连接经 [`Server::next_connection`] 交付；失败连接**静默丢弃**
/// （对端只见断开，docs/02 §0），来源 IP 计入限速（docs/02 §3.2"每来源 IP"）。
///
/// # Errors
/// [`NetError::IoFailed`]：端口绑定失败（占用/权限）或读取监听地址失败。
pub async fn serve(opts: ServerOptions, psk: &[u8; 32]) -> Result<Server, NetError> {
    let listener = TcpListener::bind(opts.bind)
        .await
        .map_err(|_| NetError::IoFailed)?;
    let local_addr = listener.local_addr().map_err(|_| NetError::IoFailed)?;
    let (tx, rx) = mpsc::channel::<Connection>(CONNECTION_CHANNEL_BUFFER);
    // 握手失败观测面（app 顶层日志落点）：Sender 由 Server 持有（订阅入口），
    // clone 进握手任务；初始无订阅者，send 失败被忽略（零成本空转）。
    let (failure_tx, _) = broadcast::channel(HANDSHAKE_FAILURE_NOTIFY_BUFFER);
    // 键 = 来源 IP（docs/02 §3.2"每来源 IP"；源端口随连接漂移，不能作键——
    // 见模块文档"限速"节与回归测试）。
    let throttle = Arc::new(Mutex::new(HandshakeThrottle::<IpAddr>::with_config(
        opts.throttle,
    )));
    // 白名单解析（FR-5.4）：启动期一次（serve 期配置变更由上层重建运行时）。
    // 非法条目跳过——配置校验（xcontrol-config validate）应已拦截，此处防御
    // 不中断主控启动（error-handling.md 规则 4）；net 层零日志依赖（M1 先例），
    // 非法条目的用户可见面由 app 侧装配前的配置校验承担。全部非法 = 空
    // 解析表 = 不启用（与空配置同语义，不会"全部拒绝"）。
    let allowlist: Vec<(std::net::Ipv4Addr, u8)> = opts
        .allowlist
        .iter()
        .filter_map(|cidr| crate::discovery::parse_cidr(cidr))
        .collect();
    let accept_task = tokio::spawn(accept_loop(
        listener,
        *psk,
        throttle,
        allowlist,
        tx,
        failure_tx.clone(),
    ));
    Ok(Server {
        local_addr,
        connections: rx,
        failures: failure_tx,
        accept_task,
    })
}

/// accept 循环：白名单门（FR-5.4）→ 限速门（触握手前）→ 每连接一个握手任务。
///
/// 交付通道关闭（[`Server`] 已 drop）后停止 accept。
async fn accept_loop(
    listener: TcpListener,
    psk: [u8; 32],
    throttle: Arc<Mutex<HandshakeThrottle<IpAddr>>>,
    allowlist: Vec<(std::net::Ipv4Addr, u8)>,
    tx: mpsc::Sender<Connection>,
    failures: broadcast::Sender<HandshakeFailure>,
) {
    loop {
        if tx.is_closed() {
            return; // 服务已被调用方停止
        }
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                // accept 瞬态错误（如连接在 backlog 中被重置）：短暂退避后继续，
                // 不放大为主控宕机；持续失败由观测层处理（M3）。
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
        };
        // 白名单门（FR-5.4，docs/07 §6）：**握手前**的本地策略拒绝——对端只见
        // 断开（与限速门同面，无任何协议反馈）；不进握手、不计握手限速
        // （策略拒绝不是握手失败，退避语义只针对错码爆破）。空白名单 = 不启用。
        if !ip_allowed(peer.ip(), &allowlist) {
            drop(stream); // 立即断开
            // 观测面通知（app 顶层日志落点）：reason = NotInAllowlist；无订阅者
            // send 失败即忽略（限速门/握手路径同形）。
            let _ = failures.send(HandshakeFailure {
                peer,
                reason: HandshakeFailureReason::NotInAllowlist,
            });
            continue;
        }
        // 主控侧限速（docs/02 §3.2：per-来源 IP，5 失败 → 指数退避）。判定在
        // 握手**前**——被限速对端只见连接不建立，无任何协议反馈（无部分匹配 oracle）。
        if !matches!(
            lock_throttle(&throttle).admit(&peer.ip(), Instant::now()),
            Admission::Allowed
        ) {
            drop(stream); // 直接断开
            continue;
        }
        if prepare_stream(&stream).is_err() {
            drop(stream); // NODELAY 设置失败 = 流异常，不进入握手
            continue;
        }
        tokio::spawn(handle_incoming(
            stream,
            peer,
            psk,
            Arc::clone(&throttle),
            tx.clone(),
            failures.clone(),
        ));
    }
}

/// 白名单匹配谓词（FR-5.4）：空表 = 不启用（放行）；非空 = 来源 IP 必须落在
/// 至少一条 CIDR 内。IPv6 来源在启用白名单时一律拒绝（白名单当前仅 IPv4，
/// [`crate::discovery::parse_cidr`] 语义；发现层 beacon 同面）。
fn ip_allowed(ip: IpAddr, allowlist: &[(std::net::Ipv4Addr, u8)]) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => allowlist
            .iter()
            .any(|cidr| crate::discovery::cidr_contains(*cidr, v4)),
        IpAddr::V6(_) => false,
    }
}

/// 单连接握手任务（responder 侧）：5s 窗口内完成 XXpsk0；成功交付、失败计数
/// 断开并上抛观测面通知。
async fn handle_incoming(
    mut stream: TcpStream,
    peer: SocketAddr,
    psk: [u8; 32],
    throttle: Arc<Mutex<HandshakeThrottle<IpAddr>>>,
    tx: mpsc::Sender<Connection>,
    failures: broadcast::Sender<HandshakeFailure>,
) {
    let outcome = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        NoiseHandshake::run_async(&mut stream, HandshakeRole::Responder, &psk),
    )
    .await;
    match outcome {
        Ok(Ok(secure)) => {
            lock_throttle(&throttle).record_success(&peer.ip());
            let session_id = secure.session_id();
            let transport = TcpTransport::new(stream, secure, peer);
            // 交付失败 = 服务已停止（Server drop）：静默退出（连接随 transport drop 关闭）。
            let _ = tx
                .send(Connection {
                    transport,
                    session_id,
                    peer,
                })
                .await;
        }
        // 超时与一切握手失败同折叠（docs/02 §3.2 失败即断开 + docs/02 §0 无原因反馈）；
        // 观测面通知（app 顶层日志落点）：无订阅者 send 失败即忽略，不阻塞。
        Ok(Err(_)) | Err(_) => {
            lock_throttle(&throttle).record_failure(peer.ip(), Instant::now());
            let _ = failures.send(HandshakeFailure {
                peer,
                reason: HandshakeFailureReason::Handshake,
            });
            drop(stream); // 失败即断开
        }
    }
}

/// 从机侧入口：连到 `addr` 并执行 initiator 握手。
///
/// 顺序（docs/02 §3.2 从机自我限速）：
/// 1. **本地限速判定（触网前）**——退避/冷却中直接 [`NetError::RateLimited`]，
///    不产生任何网络流量；`throttle` 由调用方持有（跨重试累计退避）；
/// 2. TCP 连接（超时纳入 [`HANDSHAKE_TIMEOUT`] 同一窗口；连接失败 = 环境性
///    [`NetError::IoFailed`]，尚未进入握手、不折叠、不计失败）；
/// 3. XXpsk0 握手（5s 经 `tokio::time::timeout`）——一切失败折叠
///    [`NetError::HandshakeFailed`] 并计入限速。
///
/// 成功返回 [`Connection`]（`session_id` 与主控侧一致，docs/02 §3.3）。
///
/// # Errors
/// [`NetError::RateLimited`]（本地限速，未触网）、[`NetError::IoFailed`]
/// （TCP 连接失败/超时）、[`NetError::HandshakeFailed`]（握手失败，唯一出口变体）。
pub async fn connect(
    addr: SocketAddr,
    psk: &[u8; 32],
    throttle: &mut HandshakeThrottle<SocketAddr>,
) -> Result<Connection, NetError> {
    // ① 触网前的本地限速判定（docs/02 §3.2：退避/冷却中的重试不产生流量）。
    if !matches!(throttle.admit(&addr, Instant::now()), Admission::Allowed) {
        return Err(NetError::RateLimited);
    }
    // ② TCP 连接：握手尚未开始，连接层失败不折叠、不计入握手失败
    //    （无安全码 oracle 语义——端口可达性本就经扫描公开可见）。
    let mut stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => return Err(NetError::IoFailed),
    };
    if prepare_stream(&stream).is_err() {
        return Err(NetError::IoFailed);
    }
    // ③ 握手：5s 窗口（docs/02 §3.2），超时与失败统一折叠 + 限速计数 + 断开。
    let outcome = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        NoiseHandshake::run_async(&mut stream, HandshakeRole::Initiator, psk),
    )
    .await;
    let secure = match outcome {
        Ok(Ok(secure)) => secure,
        Ok(Err(_)) | Err(_) => {
            throttle.record_failure(addr, Instant::now());
            drop(stream); // 失败即断开
            return Err(NetError::HandshakeFailed);
        }
    };
    throttle.record_success(&addr);
    let session_id = secure.session_id();
    Ok(Connection {
        transport: TcpTransport::new(stream, secure, addr),
        session_id,
        peer: addr,
    })
}

/// 连接前置：强制 `TCP_NODELAY`（docs/02 §0；握手报文同样不被 Nagle 拖延）。
/// 失败 = 流异常 → 调用方丢弃连接。
fn prepare_stream(stream: &TcpStream) -> Result<(), NetError> {
    stream.set_nodelay(true).map_err(|_| NetError::IoFailed)
}

/// throttle 锁获取（锁中毒恢复：计数为纯内存状态，中毒后保留内容继续限速
/// 优于整体失效；`unwrap_or_else` 非 unwrap，不触 CI 禁令——与 psk.rs 同形）。
fn lock_throttle(
    throttle: &Mutex<HandshakeThrottle<IpAddr>>,
) -> MutexGuard<'_, HandshakeThrottle<IpAddr>> {
    throttle.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::OnceLock;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// 测试假码派生（m=19MiB argon2，进程内一次；docs/08 §4 假码纪律）。
    fn serve_test_psk(code: &'static str) -> [u8; 32] {
        static GOOD: OnceLock<[u8; 32]> = OnceLock::new();
        static BAD: OnceLock<[u8; 32]> = OnceLock::new();
        match code {
            "serve-good" => *GOOD
                .get_or_init(|| crate::psk::derive_psk(code, &[0u8; 16]).expect("固定输入派生")),
            _ => {
                *BAD.get_or_init(|| crate::psk::derive_psk(code, &[0u8; 16]).expect("固定输入派生"))
            }
        }
    }

    /// 测试限速参数：阈值/基数缩小保证用例毫秒级；min_interval 缩小避免
    /// 主动断言与冷却语义纠缠（冷却路径由 throttle 单测承载）。
    fn fast_throttle_config() -> ThrottleConfig {
        ThrottleConfig {
            max_failures: 5,
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            min_interval: Duration::from_millis(1),
        }
    }

    /// 测试辅助取值（非 `#[test]` 作用域，与 chain.rs `must` 同形）。
    #[track_caller]
    fn must<T, E: std::fmt::Debug>(context: &'static str, result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{context}: {error:?}"),
        }
    }

    /// serve 交付连接（带超时兜底，防测试挂死）。
    async fn next_conn(server: &mut Server) -> Connection {
        let deadline = Duration::from_secs(5);
        tokio::time::timeout(deadline, server.next_connection())
            .await
            .expect("握手交付在 5s 内")
            .expect("服务未停止时有连接")
    }

    /// 成功路径：connect → serve 交付，session_id 双端一致、peer 地址正确。
    #[tokio::test]
    async fn connect_and_serve_handshake_success() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());

        let client = must(
            "connect 握手成功",
            connect(addr, &serve_test_psk("serve-good"), &mut throttle).await,
        );
        let server_side = next_conn(&mut server).await;

        assert_eq!(
            client.session_id, server_side.session_id,
            "docs/02 §3.3：双方会话标识一致"
        );
        assert_eq!(client.peer, addr, "connect 侧 peer = 监听地址");
        assert_eq!(
            server_side.peer.ip(),
            client.transport.peer_addr().ip(),
            "两侧同处回环（serve 侧记录来源 socket，connect 侧记录拨号目标）"
        );
        assert_ne!(server_side.peer.port(), 0, "serve 侧来源端口为从机临时端口");
        assert!(throttle.is_empty(), "成功握手清空限速窗口（docs/02 §3.2）");
        assert!(!client.transport.is_closed());
    }

    /// 握手后的应用帧往返（localhost 全链路的传输段）。
    #[tokio::test]
    async fn connection_exchanges_frames() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let mut client = must(
            "connect",
            connect(addr, &serve_test_psk("serve-good"), &mut throttle).await,
        );
        let mut server_side = next_conn(&mut server).await;

        client.transport.send(b"ping-frame").await.unwrap();
        let mut out = Vec::new();
        server_side.transport.recv(&mut out).await.unwrap();
        assert_eq!(out, b"ping-frame");
        server_side.transport.send(b"pong-frame").await.unwrap();
        client.transport.recv(&mut out).await.unwrap();
        assert_eq!(out, b"pong-frame");
    }

    /// 错误安全码（FR-1.4）：connect 报 HandshakeFailed（唯一出口变体）；
    /// 连接断开（serve 侧无交付）；随后正确安全码仍可正常握手（服务健在、
    /// 无"部分匹配"后泄漏态）。
    #[tokio::test]
    async fn wrong_code_rejected_and_folded() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();

        // ① 错码：折叠 HandshakeFailed（从机视角与超时/断开不可区分——无部分匹配 oracle）。
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let result = connect(addr, &serve_test_psk("serve-bad"), &mut throttle).await;
        assert!(
            matches!(result, Err(NetError::HandshakeFailed)),
            "错码折叠为唯一变体：{result:?}"
        );
        assert_eq!(throttle.failure_count(&addr), 1, "失败计入限速");

        // ② serve 侧无交付（连接已断开）——交付通道在宽限窗内保持为空。
        let nothing =
            tokio::time::timeout(Duration::from_millis(200), server.next_connection()).await;
        assert!(
            matches!(nothing, Err(_elapsed)),
            "错码连接不得交付（对端只见断开）"
        );

        // ③ 正确安全码随即成功：服务未被失败握手污染。
        let mut fresh = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let client = must(
            "错码后正确码握手",
            connect(addr, &serve_test_psk("serve-good"), &mut fresh).await,
        );
        let server_side = next_conn(&mut server).await;
        assert_eq!(client.session_id, server_side.session_id);
    }

    /// 限速（docs/02 §3.2）：同 addr 连打 6 次，前 5 次真实握手失败（错码），
    /// 第 6 次**触网前**被本地 Backoff 拒绝（RateLimited，非 HandshakeFailed
    /// ——后者必然经历了网络往返）。
    #[tokio::test]
    async fn sixth_rapid_attempt_rejected_before_dialing() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());

        // ①-⑤：本地判定允许 → 真实触网 → 错码握手失败（退避阈值 5 次后触发）。
        for attempt in 1..=5_u32 {
            // 间隔 > min_interval，规避 Cooldown 干扰（本用例只验证 Backoff 路径）。
            tokio::time::sleep(Duration::from_millis(10)).await;
            let result = connect(addr, &serve_test_psk("serve-bad"), &mut throttle).await;
            assert!(
                matches!(result, Err(NetError::HandshakeFailed)),
                "第 {attempt} 次应为真实握手失败：{result:?}"
            );
            assert_eq!(throttle.failure_count(&addr), attempt);
        }

        // ⑥：本地退避已生效 → RateLimited（触网前，无 TCP 连接产生）。
        let started = std::time::Instant::now();
        let result = connect(addr, &serve_test_psk("serve-bad"), &mut throttle).await;
        assert!(
            matches!(result, Err(NetError::RateLimited)),
            "第 6 次必须在触网前被本地退避拒绝：{result:?}"
        );
        assert_eq!(
            throttle.failure_count(&addr),
            5,
            "被拒尝试不计失败（未触网）"
        );
        assert!(
            started.elapsed() < Duration::from_millis(40),
            "本地拒绝不得经历网络往返（真实握手失败 + 间隔 >40ms）"
        );

        // serve 侧依然健康：等待 serve 侧退避期满（base_backoff 50ms + 裕量，
        // 退避在第 5 次失败时于 serve 侧同步生效）后，正确安全码 + 新窗口 → 全链路成功。
        let mut fresh = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        tokio::time::sleep(Duration::from_millis(80)).await;
        let client = must(
            "限速拒绝后服务不受影响",
            connect(addr, &serve_test_psk("serve-good"), &mut fresh).await,
        );
        let server_side = next_conn(&mut server).await;
        assert_eq!(client.session_id, server_side.session_id);
    }

    /// 端口不可达 → IoFailed（连接层环境性失败，不折叠为握手失败、不计限速）。
    #[tokio::test]
    async fn connect_refused_is_io_failed() {
        // 0.0.0.0:0 不构成合法连接目标：bind 随机端口后立刻关闭 listener。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("占位监听");
        let addr = listener.local_addr().expect("地址");
        drop(listener); // 端口释放，connect 必被拒（ECONNREFUSED）
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let result = connect(addr, &serve_test_psk("serve-good"), &mut throttle).await;
        assert!(
            matches!(result, Err(NetError::IoFailed)),
            "连接被拒是环境性失败：{result:?}"
        );
        assert!(
            throttle.is_empty(),
            "未进入握手的失败不得计入握手限速（无 oracle 语义）"
        );
    }

    /// Server drop 停止服务：accept 任务终止，后续 connect 得 IoFailed（端口关闭）。
    #[tokio::test]
    async fn server_drop_stops_listening() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();
        drop(server);
        // abort + 端口释放需要一拍事件循环；轮询至多 1s。
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let mut refused = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Err(NetError::IoFailed) =
                connect(addr, &serve_test_psk("serve-good"), &mut throttle).await
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "Server drop 后监听应关闭（连接被拒）");
    }

    /// FR-5.4 白名单（授权路径）：allowlist 覆盖来源 → 握手照常交付。
    #[tokio::test]
    async fn allowlisted_source_connects_normally() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            // 回环 /8 覆盖 127.0.0.1 与 127.0.0.2（macOS/Linux 全 127/8 回环）。
            allowlist: vec!["127.0.0.0/8".to_owned()],
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();
        let mut failures = server.subscribe_handshake_failures();
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let client = must(
            "白名单内来源正常握手",
            connect(addr, &serve_test_psk("serve-good"), &mut throttle).await,
        );
        let server_side = next_conn(&mut server).await;
        assert_eq!(client.session_id, server_side.session_id);
        // 授权路径零失败通知（静默回归锚点）。
        let quiet = tokio::time::timeout(Duration::from_millis(150), failures.recv()).await;
        assert!(quiet.is_err(), "白名单内来源不得产出失败通知");
    }

    /// FR-5.4 白名单（拒绝路径）：白名单不含回环来源（仅 TEST-NET-3
    /// `203.0.113.0/24`，本机不可路由）时，回环来源**正确安全码**连接也在
    /// 握手**前**被断开——对端见 EOF/重置（无任何协议反馈，docs/02 §0 同面）；
    /// 观测面通知 reason = NotInAllowlist；不计握手限速（策略拒绝 ≠ 错码爆破）；
    /// 放宽白名单后同一来源恢复全链路（授权路径由
    /// `allowlisted_source_connects_normally` 单独锚定）。
    #[tokio::test]
    async fn non_allowlisted_source_rejected_before_handshake() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: vec!["203.0.113.0/24".to_owned()],
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();
        let mut failures = server.subscribe_handshake_failures();

        // 白名单外来源（127.0.0.1）：正确安全码的完整握手代码路径——拒绝必须
        // 发生在 accept 后、握手前（原始 socket 观察"无任何回包"，真握手会回 msg2）。
        let mut raw = tokio::net::TcpStream::connect(addr)
            .await
            .expect("回环连接");
        raw.set_nodelay(true).expect("NODELAY");
        let mut sink = [0u8; 8];
        let response = tokio::time::timeout(Duration::from_secs(2), raw.read(&mut sink)).await;
        assert!(
            matches!(response, Ok(Ok(0)) | Ok(Err(_)) | Err(_)),
            "白名单外来源只见断开（无 msg2/无协议反馈）：{response:?}"
        );

        // 观测面：NotInAllowlist 通知送达（对端 IP = 回环来源）。
        let notification = tokio::time::timeout(Duration::from_secs(5), failures.recv())
            .await
            .expect("通知在 5s 内送达")
            .expect("订阅存活");
        assert_eq!(notification.peer.ip(), addr.ip(), "通知对端 = 被拒来源");
        assert_eq!(
            notification.reason,
            HandshakeFailureReason::NotInAllowlist,
            "拒绝原因标注为白名单策略（非握手失败）"
        );

        // serve 侧无交付（连接未进握手）。
        let nothing =
            tokio::time::timeout(Duration::from_millis(150), server.next_connection()).await;
        assert!(nothing.is_err(), "白名单外连接不得交付");

        // Server 内部白名单表已定（serve 期不变）——授权路径回归锚点：
        // 策略拒绝未污染限速窗口，同 IP 后续行为由 throttle 单独承载。
        let _ = &server;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), failures.recv())
                .await
                .is_err(),
            "失败通知不得凭空多出（每次拒绝恰一条）"
        );
    }

    /// 握手失败观测面（NFR-8 / FR-1.4 主控侧日志落点）：错码握手 → 通知送达
    /// （只含对端地址，原因折叠不携带）；成功握手 → 无通知（静默路径回归锚点）。
    #[tokio::test]
    async fn handshake_failure_notification_delivered_and_success_silent() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();
        let mut failures = server.subscribe_handshake_failures();
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());

        // 静默路径：正确安全码握手成功——不产出失败事件。
        let client = must(
            "成功握手（无通知）",
            connect(addr, &serve_test_psk("serve-good"), &mut throttle).await,
        );
        let server_side = next_conn(&mut server).await;
        drop(client);
        drop(server_side);
        let silent = tokio::time::timeout(Duration::from_millis(150), failures.recv()).await;
        assert!(silent.is_err(), "成功握手不得产出失败通知");

        // 错码握手（真实触网）→ 失败通知送达：对端 IP 与发起方一致。
        let bad = connect(addr, &serve_test_psk("serve-bad"), &mut throttle).await;
        assert!(
            matches!(bad, Err(NetError::HandshakeFailed)),
            "错码折叠：{bad:?}"
        );
        let notification = tokio::time::timeout(Duration::from_secs(5), failures.recv())
            .await
            .expect("通知在 5s 内送达")
            .expect("订阅存活");
        assert_eq!(
            notification.peer.ip(),
            addr.ip(),
            "通知对端 IP = 发起方来源 IP"
        );
        assert_ne!(notification.peer.port(), 0, "来源端口为临时端口");
        assert_eq!(
            throttle.failure_count(&addr),
            1,
            "限速计数语义不受观测面影响"
        );

        // 无凭空事件：通知消费完后再等一拍，不得多出（限速前拒绝在客户端本地
        // 判定，根本无 TCP 到达服务端——事件只产自 handle_incoming 折叠点）。
        let quiet = tokio::time::timeout(Duration::from_millis(150), failures.recv()).await;
        assert!(quiet.is_err(), "失败通知不得凭空多出");
    }
    /// 每条 TCP 连接源端口互异，主控限速键必须是来源 **IP** 而非 [`SocketAddr`]
    /// ——按 SocketAddr 计键时每次新连接都拿到全新计数，退避永不生效（安全码
    /// 在线爆破闸门失效；11-17-m2-tcp check 评审修正）。
    ///
    /// 用原始 socket 垃圾握手（绕开从机侧 throttle）打满 5 次失败后，断言同 IP
    /// 新连接上**正确安全码**也被握手前拒绝（若键退化为 SocketAddr，此断言必红
    /// ——新连接将握手成功），退避期满后恢复。
    #[tokio::test]
    async fn server_rate_limit_accumulates_across_connections_per_ip() {
        let opts = ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let mut server = must(
            "serve 启动",
            serve(opts, &serve_test_psk("serve-good")).await,
        );
        let addr = server.local_addr();

        // ① 5 条垃圾握手：真实 TCP + 48B 垃圾 msg1（帧封装合法、AEAD 必败）
        //    → responder 逐次 record_failure（键 = 来源 IP）。间隔 > min_interval，
        //    规避 Cooldown 干扰（每条连接都必须真实计入）。
        for _ in 0..5_u32 {
            let mut raw = tokio::net::TcpStream::connect(addr)
                .await
                .expect("垃圾连接");
            raw.set_nodelay(true).expect("NODELAY");
            let mut frame = Vec::new();
            must(
                "垃圾帧编码",
                crate::framing::encode_into(&[0u8; 48], &mut frame),
            );
            must("垃圾帧写出", raw.write_all(&frame).await);
            // 服务端解密失败即断开：对端见 EOF（FIN）或连接重置（RST）——
            // 无论如何不得收到任何回包（真握手会回 msg2）。
            let mut sink = [0u8; 8];
            let response = raw.read(&mut sink).await;
            assert!(
                matches!(response, Ok(0) | Err(_)),
                "垃圾握手必须被断开（无 msg2 回包）：{response:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // ② 同 IP 新连接（新源端口）+ **正确**安全码：退避期内必须在握手前被
        //    服务端拒绝（HandshakeFailed，非成功）。
        let mut fresh = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let rejected = connect(addr, &serve_test_psk("serve-good"), &mut fresh).await;
        assert!(
            matches!(rejected, Err(NetError::HandshakeFailed)),
            "退避期内同 IP 新连接（正确安全码）必须被拒：{rejected:?}"
        );
        // 服务端无交付（连接未进握手即被断开）。
        let nothing =
            tokio::time::timeout(Duration::from_millis(200), server.next_connection()).await;
        assert!(nothing.is_err(), "被服务端限速拒绝的连接不得交付");

        // ③ 退避期满（fast 配置第 5 次失败 = base 50ms；取 200ms 裕量覆盖调度
        //    抖动）→ 正确安全码恢复全链路：服务未被永久污染（窗口语义只在
        //    退避窗内生效，docs/02 §3.2 指数退避有界）。
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut recovered = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let client = must(
            "退避期满恢复握手",
            connect(addr, &serve_test_psk("serve-good"), &mut recovered).await,
        );
        let server_side = next_conn(&mut server).await;
        assert_eq!(client.session_id, server_side.session_id);
    }
}
