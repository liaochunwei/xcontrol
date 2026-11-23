//! 主控通告层（docs/02 §2.1）：mDNS 注册（`_xcontrol._tcp.local.`）+ UDP beacon
//! 周期广播（逐接口定向广播）+ `XCP1?` 探测应答。
//!
//! # 结构（design.md 决策）
//!
//! ```text
//! Announcer::start(opts)
//! ├── mDNS（可关）  mdns-sd 守护线程注册服务（TXT：proto/id/name/caps/port）
//! ├── 应答任务      UDP 口 bind 0.0.0.0:<discovery_port> → 收 `XCP1?` → 单播回应 beacon
//! └── 广播任务      每 period：枚举 IPv4 接口 → 逐接口 bind 接口 IP + SO_BROADCAST
//!                   → 发向该接口子网定向广播地址；period 随"满配对"切换 5s ↔ 60s
//! ```
//!
//! - **周期切换**（docs/02 §2.1：5s 未满配对 → 60s 已满）：[`Announcer::set_all_paired`]
//!   由会话层在配对状态变化时调用；广播任务下一拍生效。
//! - **UDP 发现端口**：应答口与被动监听口（browse.rs）同占 `<discovery_port>`
//!   （docs/02 §2.1"同端口"）。绑定走 [`bind_discovery_socket`] 的复用选项——
//!   双进程同机（演练/同机双角色）也能共存；不同机部署无冲突。
//! - **接口纪律**：仅 IPv4（beacon 无 IPv6 广播语义），虚拟口跳过、allowlist
//!   覆盖（[`crate::discovery::interfaces_ipv4`]）；接口列表**每拍重枚举**
//!   （Wi-Fi 漫游换接口不丢通告）。
//! - **零派生物**（docs/02 §0 / 07-security.md §2）：mDNS TXT（[`txt_properties`]
//!   纯函数）与 beacon（[`crate::discovery::Beacon`]）只接收公开字段，签名上无
//!   安全码入参；测试 `txt_properties_carry_only_public_fields` 逐字节断言。
//! - **错误纪律**（error-handling.md 规则 4）：单接口广播失败、瞬时收包错误等
//!   可忽略错误**跳过不中断**（通告循环存活）；启动级失败（UDP 端口绑定 /
//!   mDNS 守护线程）经 `Result` 上抛，由 app 侧降级（告警后继续服务）。
//! - net 层零日志纪律（M1 先例，docs/02 §2）：本模块不引 tracing；可观测性
//!   由 app/cli 顶层接入点承载。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::discovery::{self, Beacon};
use crate::error::NetError;

/// mDNS 服务类型（docs/02 §2.1：`_xcontrol._tcp.local.`，服务名 8 字符合规）。
pub const SERVICE_TYPE: &str = "_xcontrol._tcp.local.";

/// beacon 广播周期：未满配对（docs/02 §2.1：5s）。
pub const BEACON_PERIOD_UNPAIRED: Duration = Duration::from_secs(5);
/// beacon 广播周期：已满配对（docs/02 §2.1：60s——子网广播降噪）。
pub const BEACON_PERIOD_PAIRED: Duration = Duration::from_secs(60);

/// 通告层选项（[`Announcer::start`] 入参）。
#[derive(Debug, Clone)]
pub struct AnnouncerOptions {
    /// UDP 发现端口（收 `XCP1?` / 发 beacon 的目标端口；= config `net.port`）。
    pub discovery_port: u16,
    /// 主控 TCP 服务端口（beacon/TXT 载荷里的"指路"端口）。
    pub service_port: u16,
    /// 显示名（beacon name / TXT `name`）。
    pub name: String,
    /// 本机实例 UUID（公开值，docs/02 §3.1）。
    pub instance_id: [u8; 16],
    /// 能力位掩码（TXT `caps`，hex）。
    pub caps: u32,
    /// 是否启用 mDNS 通告层（docs/02 §2.1 三层并存、全部可配开关）。
    pub mdns: bool,
    /// 是否启用 UDP beacon 层（含 `XCP1?` 应答）。
    pub beacon: bool,
    /// 接口 allowlist（覆盖虚拟口启发式，精确匹配接口名）。
    pub allowlist: Vec<String>,
    /// 未满配对广播周期（默认 [`BEACON_PERIOD_UNPAIRED`]，测试可缩小）。
    pub period_unpaired: Duration,
    /// 已满配对广播周期（默认 [`BEACON_PERIOD_PAIRED`]，测试可缩小）。
    pub period_paired: Duration,
}

/// mDNS TXT 属性（docs/02 §2.1：`proto`/`id`/`name`/`caps`/`port`）。
///
/// 纯函数（无 mdns-sd 类型）便于零派生物断言；注册时转 `HashMap` 喂给
/// [`ServiceInfo::new`]。值全部公开字段：协议版本字符串、实例 hex、显示名、
/// 能力 hex、端口十进制——签名上不存在安全码入参。
#[must_use]
pub fn txt_properties(
    name: &str,
    instance_id: &[u8; 16],
    caps: u32,
    port: u16,
) -> Vec<(String, String)> {
    vec![
        ("proto".to_owned(), "1.0".to_owned()),
        ("id".to_owned(), hex(instance_id)),
        ("name".to_owned(), name.to_owned()),
        ("caps".to_owned(), format!("{caps:x}")),
        ("port".to_owned(), port.to_string()),
    ]
}

/// 字节序列小写 hex（TXT id/caps 的线上形态；无第三方 hex 依赖）。
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        // String 写入只可能因 OOM 失败（容量已预留），无外部输入路径。
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// mDNS 服务实例名：显示名 + 实例短 ID（前 2 字节 hex）——同网段两台主控
/// 同显示名时实例名仍唯一（mDNS 实例名冲突会注册失败，提前避让）。
#[must_use]
pub fn mdns_instance_name(name: &str, instance_id: &[u8; 16]) -> String {
    format!("{name}-{}", hex(&instance_id[..2]))
}

/// mDNS 主机名（SRV target）：`xcontrol-<短id>.local.`（与实例名同短 ID）。
#[must_use]
pub fn mdns_hostname(instance_id: &[u8; 16]) -> String {
    format!("xcontrol-{}.local.", hex(&instance_id[..2]))
}

/// 发现 UDP 口绑定（复用选项 + 非阻塞 → tokio socket）。
///
/// SO_REUSEADDR（全平台）+ SO_REUSEPORT（unix 家族）：主控应答口与从机被动
/// 监听口同占发现端口——双进程同机（演练/同机双角色）绑定不冲突；跨机部署
/// 无端口竞争，选项无害。`cfg(unix)` 仅因 socket2 的 `set_reuse_port`
/// 是 unix-only API（std 逐族可移植性，非平台 feature 判断——architecture-rules
/// 红线 2 针对的是 macos/windows/x11/wayland 四路平台模块）。
///
/// **投递语义（诚实边界）**：复用绑定只保证 bind 成功，**不复制数据报**——
/// 同机多 socket 竞争同一发现口时，单播包只交付其中一个 socket（Linux 按
/// 流哈希分散；macOS/BSD 倾向交付后绑定者），广播/组播则各 socket 均收。
/// 因此同机双角色下 `XCP1?` 单播探测可能落进从机自己的被动监听口（静默
/// 丢弃，探测路径失效）；广播 beacon 与 mDNS 不受影响——同机演练/部署以
/// mDNS 与广播路径为准，探测路径跨机才有完整语义。
pub(crate) fn bind_discovery_socket(bind: SocketAddr) -> Result<tokio::net::UdpSocket, NetError> {
    let domain = match bind {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
        .map_err(|_| NetError::IoFailed)?;
    // 复用选项尽力而为：设置失败时 bind 仍可能成功（单进程场景），失败则由
    // 下方 bind 返回 IoFailed 收敛。
    let _ = socket.set_reuse_address(true);
    #[cfg(unix)]
    {
        let _ = socket.set_reuse_port(true);
    }
    socket.bind(&bind.into()).map_err(|_| NetError::IoFailed)?;
    socket
        .set_nonblocking(true)
        .map_err(|_| NetError::IoFailed)?;
    tokio::net::UdpSocket::from_std(std::net::UdpSocket::from(socket))
        .map_err(|_| NetError::IoFailed)
}

/// 主控通告器（docs/02 §2.1）。`Drop` 即停止（任务 abort + mDNS 反注册）。
pub struct Announcer {
    paired_tx: watch::Sender<bool>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    daemon: Option<ServiceDaemon>,
    fullname: Option<String>,
    responder_port: Option<u16>,
}

impl Announcer {
    /// 启动通告（按 `opts.mdns`/`opts.beacon` 分层启用）。
    ///
    /// 启动级失败（发现 UDP 端口绑定失败、mDNS 守护线程不可用）返回 `Err`——
    /// 调用方（app）告警降级，不中断会话（规则 4）。
    ///
    /// # Errors
    /// [`NetError::IoFailed`]（发现端口绑定失败）、
    /// [`NetError::DiscoveryUnavailable`]（mDNS 守护线程创建/注册失败）。
    pub async fn start(opts: AnnouncerOptions) -> Result<Self, NetError> {
        let beacon = Beacon::announce(opts.service_port, opts.name.clone(), opts.instance_id);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (paired_tx, paired_rx) = watch::channel(false);
        let mut tasks = Vec::new();
        let mut daemon = None;
        let mut fullname = None;
        let mut responder_port = None;

        if opts.beacon {
            // 应答口：bind 0.0.0.0:<discovery_port>（同端口收探测，docs/02 §2.1）。
            let socket = bind_discovery_socket(SocketAddr::from((
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                opts.discovery_port,
            )))?;
            responder_port = socket.local_addr().ok().map(|addr| addr.port());

            // ① XCP1? 应答任务：收探测 → 单播回应 beacon（畸形静默丢弃）。
            let mut probe_shutdown = shutdown_rx.clone();
            tasks.push(tokio::spawn(async move {
                let mut probe_buf = [0u8; discovery::PROBE_MAGIC.len() + 64];
                loop {
                    tokio::select! {
                        received = socket.recv_from(&mut probe_buf) => {
                            let Ok((n, src)) = received else { continue }; // 瞬时错误跳过
                            if let Some(reply) = discovery::probe_reply(&probe_buf[..n], &beacon) {
                                let _ = socket.send_to(&reply, src).await;
                            }
                        }
                        _ = probe_shutdown.changed() => break,
                    }
                }
            }));

            // ② 广播任务：首拍立即 → 每 period 逐接口定向广播；period 随
            //    满配对切换（docs/02 §2.1：5s ↔ 60s）。接口每拍重枚举（漫游安全）。
            let mut bc_shutdown = shutdown_rx;
            let mut bc_paired = paired_rx;
            let allowlist = opts.allowlist.clone();
            let bytes =
                Beacon::announce(opts.service_port, opts.name.clone(), opts.instance_id).encode();
            let (unpaired, paired) = (opts.period_unpaired, opts.period_paired);
            tasks.push(tokio::spawn(async move {
                let mut period = unpaired;
                broadcast_interfaces(&bytes, opts.discovery_port, &allowlist).await;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(period) => {
                            broadcast_interfaces(&bytes, opts.discovery_port, &allowlist).await;
                            period = if *bc_paired.borrow_and_update() { paired } else { unpaired };
                        }
                        _ = bc_paired.changed() => { /* 拍间生效：下拍取新周期 */ }
                        _ = bc_shutdown.changed() => break,
                    }
                }
            }));
        }

        if opts.mdns {
            let (mdns_daemon, mdns_fullname) = register_mdns(&opts)?;
            daemon = Some(mdns_daemon);
            fullname = Some(mdns_fullname);
        }

        Ok(Self {
            paired_tx,
            shutdown_tx,
            tasks,
            daemon,
            fullname,
            responder_port,
        })
    }

    /// 会话层上报"全部配对已满"：true = 切 60s 周期，false = 回 5s
    /// （docs/02 §2.1 周期切换；拍间调用、下拍生效）。
    pub fn set_all_paired(&self, paired: bool) {
        let _ = self.paired_tx.send(paired); // 无接收方（beacon 关）时静默
    }

    /// 应答口实际绑定端口（测试锚点；绑定失败/未启用 beacon → `None`）。
    #[must_use]
    pub const fn responder_port(&self) -> Option<u16> {
        self.responder_port
    }

    /// 停止通告：abort 任务 + mDNS 反注册 + 守护线程关停（幂等；`Drop` 同路径）。
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
        for task in &self.tasks {
            task.abort();
        }
        if let (Some(daemon), Some(fullname)) = (&self.daemon, &self.fullname) {
            // 反注册尽力而为：接收端直接丢弃（不等 UnregisterStatus）。
            let _ = daemon.unregister(fullname);
            let _ = daemon.shutdown();
        }
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 手写 Debug（`ServiceDaemon` 未实现 Debug；只投影可诊断字段，不持锁）。
impl std::fmt::Debug for Announcer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Announcer")
            .field("tasks", &self.tasks.len())
            .field("mdns_registered", &self.fullname.is_some())
            .field("responder_port", &self.responder_port)
            .finish_non_exhaustive()
    }
}

/// 逐接口定向广播一轮 beacon（docs/02 §2.1：bind 接口 IP + SO_BROADCAST；
/// 单接口失败跳过不中断——规则 4）。
async fn broadcast_interfaces(bytes: &[u8], dst_port: u16, allowlist: &[String]) {
    let Ok(ifaces) = discovery::interfaces_ipv4(allowlist) else {
        return; // 枚举失败：本轮无广播，下拍重试
    };
    for iface in ifaces {
        // 每接口独立临时 socket：bind 接口 IP（否则广播可能静默发不出去，
        // docs/02 §2.3）；SO_BROADCAST 必须显式开（内核默认拒发广播）。
        let bind = SocketAddr::from((IpAddr::V4(iface.ip), 0));
        let Ok(socket) = tokio::net::UdpSocket::bind(bind).await else {
            continue;
        };
        if socket.set_broadcast(true).is_err() {
            continue;
        }
        let dst = SocketAddr::from((IpAddr::V4(iface.broadcast), dst_port));
        let _ = socket.send_to(bytes, dst).await; // 失败（广播域黑洞等）跳过
    }
}

/// mDNS 注册（docs/02 §2.1）：TXT 属性来自 [`txt_properties`]（纯公开字段）。
///
/// A 记录地址 = 本机 IPv4 接口地址（allowlist 同广播层）；无接口时回退
/// 127.0.0.1（mDNS 在受限环境降级——同机回环仍可发现，docs/02 §2.3 注意事项）。
fn register_mdns(opts: &AnnouncerOptions) -> Result<(ServiceDaemon, String), NetError> {
    let daemon = ServiceDaemon::new().map_err(|_| NetError::DiscoveryUnavailable)?;
    let instance = mdns_instance_name(&opts.name, &opts.instance_id);
    let host = mdns_hostname(&opts.instance_id);
    let props: HashMap<String, String> =
        txt_properties(&opts.name, &opts.instance_id, opts.caps, opts.service_port)
            .into_iter()
            .collect();
    let ips = discovery::interfaces_ipv4(&opts.allowlist)
        .ok()
        .map(|ifaces| {
            ifaces
                .iter()
                .map(|iface| iface.ip.to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|joined| !joined.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &host,
        ips.as_str(),
        opts.service_port,
        props,
    )
    .map_err(|_| NetError::DiscoveryUnavailable)?;
    let fullname = info.get_fullname().to_owned();
    daemon
        .register(info)
        .map_err(|_| NetError::DiscoveryUnavailable)?;
    Ok((daemon, fullname))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// 测试通告选项（mdns 关——单测不污染真实网络；beacon 周期拉长保证
    /// 广播任务在测试窗内不发真实广播包）。
    fn test_opts(discovery_port: u16) -> AnnouncerOptions {
        AnnouncerOptions {
            discovery_port,
            service_port: 4243,
            name: "test-host".to_owned(),
            instance_id: [0x11; 16],
            caps: 0x9,
            mdns: false,
            beacon: true,
            allowlist: Vec::new(),
            period_unpaired: Duration::from_secs(3600),
            period_paired: Duration::from_secs(3600),
        }
    }

    /// TXT 属性内容契约（docs/02 §2.1 五键逐一对值）。
    #[test]
    fn txt_properties_carry_documented_keys() {
        let props = txt_properties("my-mac", &[0xAB; 16], 0x9, 4243);
        let get = |key: &str| -> String {
            match props.iter().find(|(k, _)| k == key) {
                Some((_, v)) => v.clone(),
                None => String::new(),
            }
        };
        assert_eq!(get("proto"), "1.0");
        assert_eq!(get("id"), hex(&[0xAB; 16]));
        assert_eq!(get("name"), "my-mac");
        assert_eq!(get("caps"), "9");
        assert_eq!(get("port"), "4243");
        assert_eq!(props.len(), 5, "文档未定义的键不得出现");
    }

    /// **安全红线测试**：mDNS TXT 只含公开字段——完整派生链（盐/PSK）与
    /// 安全码原文都不在 TXT 任何键值中（docs/02 §0 WPS 红线）。断言覆盖
    /// 原始字节 + 大小写 hex 文本三种形态（TXT 值是字符串，hex 编码形态的
    /// 泄漏不会被原始字节子串断言捕获）。
    #[test]
    fn txt_properties_carry_only_public_fields() {
        let code = "txt-code-zero-deriv";
        let uuid: [u8; 16] = [0x3C; 16];
        let salt = crate::psk::derive_salt(&uuid);
        let psk = crate::psk::derive_psk(code, &uuid).expect("固定输入派生");

        let hex_lower = |bytes: &[u8]| hex(bytes);
        let hex_upper = |bytes: &[u8]| hex(bytes).to_ascii_uppercase();
        let text_forms = |bytes: &[u8]| {
            [
                bytes.to_vec(),
                hex_lower(bytes).into_bytes(),
                hex_upper(bytes).into_bytes(),
            ]
        };
        let props = txt_properties("sec-mac", &uuid, 0x9, 4243);
        for (key, value) in &props {
            for (label, forms) in [
                ("安全码原文", text_forms(code.as_bytes())),
                ("盐", text_forms(&salt)),
                ("PSK", text_forms(&psk)),
            ] {
                for form in &forms {
                    assert!(
                        !value
                            .as_bytes()
                            .windows(form.len())
                            .any(|window| window == form.as_slice()),
                        "TXT {key} 值泄漏 {label}（hex 文本形态）"
                    );
                }
            }
        }
    }

    /// 实例名/主机名带短 ID：同显示名不同实例 → 不同实例名（冲突避让）。
    #[test]
    fn mdns_names_embed_short_id() {
        let a = mdns_instance_name("mac", &[0x01; 16]);
        let b = mdns_instance_name("mac", &[0x02; 16]);
        assert_ne!(a, b);
        assert!(a.starts_with("mac-"));
        assert_eq!(a.len(), 3 + 1 + 4);
        assert!(mdns_hostname(&[0x07; 16]).ends_with(".local."));
        assert!(mdns_hostname(&[0x07; 16]).starts_with("xcontrol-"));
    }

    /// `XCP1?` 应答全链路（真实 socket）：探测 → 单播 beacon 应答 → 可解码
    /// 且字段与通告一致（FR-5.1 探测路径；docs/02 §2.1"同端口"）。
    #[tokio::test]
    async fn responder_replies_to_probe_with_beacon() {
        // discovery_port = 0：内核分配临时端口（测试不占真实发现端口）。
        let announcer = Announcer::start(test_opts(0)).await.expect("通告器启动");
        let port = announcer.responder_port().expect("应答口已绑定");

        let prober = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("探测 socket");
        prober
            .send_to(discovery::PROBE_MAGIC.as_slice(), ("127.0.0.1", port))
            .await
            .expect("探测发出");
        let mut buf = [0u8; 512];
        let (n, src) = tokio::time::timeout(Duration::from_secs(2), prober.recv_from(&mut buf))
            .await
            .expect("2s 内收到应答")
            .expect("应答可读");
        // 应答源 = 应答口（同端口语义：源端口 == 发现端口）。
        assert_eq!(src.port(), port);
        let beacon = Beacon::decode(&buf[..n]).expect("应答是合法 beacon");
        assert_eq!(beacon.port, 4243, "载荷 port = TCP 服务端口");
        assert_eq!(beacon.name, "test-host");
        assert_eq!(beacon.instance_id, [0x11; 16]);
        assert_eq!(beacon.proto, discovery::BEACON_PROTO);

        // 非探测负载（如对端 beacon 误入应答口）：静默，不回应。
        prober
            .send_to(b"garbage-not-probe", ("127.0.0.1", port))
            .await
            .expect("垃圾发出");
        let nothing =
            tokio::time::timeout(Duration::from_millis(300), prober.recv_from(&mut buf)).await;
        assert!(nothing.is_err(), "非探测不得触发应答");

        announcer.stop();
    }

    /// stop 后应答口关闭：探测不再有应答（幂等 stop / Drop 路径的活性验证）。
    #[tokio::test]
    async fn stopped_announcer_no_longer_replies() {
        let announcer = Announcer::start(test_opts(0)).await.expect("启动");
        let port = announcer.responder_port().expect("端口");
        announcer.stop();

        let prober = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("探测 socket");
        prober
            .send_to(discovery::PROBE_MAGIC.as_slice(), ("127.0.0.1", port))
            .await
            .expect("探测发出");
        let mut buf = [0u8; 512];
        let nothing =
            tokio::time::timeout(Duration::from_millis(400), prober.recv_from(&mut buf)).await;
        assert!(nothing.is_err(), "停止后不得应答");
        // 幂等：重复 stop 不 panic。
        announcer.stop();
    }

    /// 满配对切换的 API 活性（beacon 关闭时发送方缺失 → send 失败被吞，不 panic）。
    #[test]
    fn set_all_paired_without_beacon_is_noop() {
        // start 需要异步（mdns 关、beacon 关 → 零任务启动）。
        let opts = AnnouncerOptions {
            beacon: false,
            mdns: false,
            ..test_opts(0)
        };
        let announcer = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("运行时")
            .block_on(async { Announcer::start(opts).await.expect("启动") });
        announcer.set_all_paired(true);
        announcer.set_all_paired(false);
        assert!(announcer.responder_port().is_none(), "beacon 关 → 无应答口");
    }
}
