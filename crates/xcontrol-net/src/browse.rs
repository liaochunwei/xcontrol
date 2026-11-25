//! 从机发现（docs/02 §2.2）：mDNS browse + UDP 并发探测（CIDR 展开 / 128 并发 /
//! 300ms 收集窗）+ 兜底 TCP connect 扫描（默认关）→ 按 instance id 去重 → 候选。
//!
//! # 三层（docs/02 §2.2 逐字）
//!
//! ```text
//! Scanning:
//!  1. mDNS browse(_xcontrol._tcp)     命中 → 候选（源标记 Mdns）
//!  2. 并发 UDP 探测：每个已配 CIDR（默认 = 各接口真实前缀）逐 IP 发 XCP1?，
//!     300ms 收集应答（被动 beacon 监听同窗并行；源标记 Probe/Beacon）
//!  3. 兜底 TCP connect 扫描（可选，默认关）：CIDR 并发 connect（128 上限/1s 超时）
//! 候选去重（按 instance id）→ 交给调用方逐个 Noise 握手（安全码不匹配 = 握手
//! 失败自动排除，docs/02 §2.2）
//! ```
//!
//! # 决策记录
//!
//! - **UDP 探测的"128 并发"**：无连接 `send_to` 内核开销微秒级（/24 = 254 发
//!   ≈ 毫秒级），并发上限落在**发送任务的 `Semaphore`(128)** 上（共享探测
//!   socket，`send_to` 是 `&self`）——语义与 docs/02 §2.2 一致且无每目标建
//!   socket 的开销；300ms 应答窗 + 1s 探测段硬上限双保险。
//! - **窗口归属**（docs/02 §2.2 的"超时 1s"属于 UDP 探测段）：mDNS browse
//!   自有收集窗（[`MDNS_WINDOW`] 1.2s），探测段 300ms + 1s 硬帽，两段独立——
//!   整轮 ≈1.6s，留足 FR-5.1"≤5s 连上"的握手预算。
//! - **兜底 TCP 扫描（默认关）**：docs/02 §2.2 要求"连上后发 SaltQuery 协议帧
//!   验证是否 XControl"——该消息**尚未进入 docs/02 §4 消息层定义**，协议红线
//!   （先文档后代码 + 本任务 proto 零改动）禁止新增；本层实现 connect 级
//!   可达性探测，产出 `instance_id` 全零的候选（去重按地址，[`dedupe_candidates`]），
//!   消费方以手动安全码路径握手（app 层 `session_rt`；生产语义 = M4 的
//!   per-device UUID 落地后由 SaltQuery 升级补全 id）。默认关：盲 connect 给
//!   对端留一次握手超时计数（5s 折叠 HandshakeFailed），连扫会触发对端限速
//!   （docs/02 §3.2）——只在 mDNS/beacon 双双空手时由调用方显式开启。
//! - **被动 beacon 监听**：同占发现端口（[`crate::announce::bind_discovery_socket`]
//!   复用绑定），扫描窗内收到的定向广播 beacon 进候选（源标记 Beacon）——
//!   mDNS 在 AP 隔离/组播受限网络失效时的兜底（docs/02 §0 发现三层动机）。
//!   周期性广播（5s）与短扫描窗天然异步：当轮没收到不表示主控不存在，重连
//!   循环逐轮刷新候选会自然覆盖。
//! - **去重**（[`dedupe_candidates`]）：有 id 候选按 instance id 首见保留
//!   （优先序 = 产出顺序：Mdns → Beacon → Probe → TcpProbe）；无 id 候选
//!   （TCP 兜底）按地址去重。
//! - **零日志纪律 / 错误纪律**（M1 先例 + error-handling.md 规则 4）：单层
//!   失败（mDNS 守护线程起不来、socket 绑定失败）降级为该层空产出，
//!   [`Browser::scan_once`] 恒成功返回候选列表——发现层失败不中断重连循环。

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::announce::{SERVICE_TYPE, bind_discovery_socket};
use crate::discovery::{self, Beacon};

/// UDP 探测并发上限（docs/02 §2.2：128）。
pub const PROBE_CONCURRENCY: usize = 128;
/// 探测应答收集窗（docs/02 §2.2：300ms）。
pub const PROBE_WINDOW: Duration = Duration::from_millis(300);
/// UDP 探测段硬上限（docs/02 §2.2"超时 1s"；覆盖发送 + 收集全程）。
pub const SCAN_DEADLINE: Duration = Duration::from_secs(1);
/// mDNS browse 收集窗（docs/02 §2.2 未定值；FR-5.1"≤5s 连上"预算下取 1.2s
/// ——首包解析通常 <200ms，1.2s 覆盖同网段一次问答往返有余）。
pub const MDNS_WINDOW: Duration = Duration::from_millis(1200);
/// 兜底 TCP 扫描单目标超时（docs/02 §2.2：1s）。
pub const TCP_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// 候选来源（docs/02 §2.2 三层 + TCP 兜底；去重优先序 = 枚举序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
    /// mDNS browse 命中（SRV/A 已解析）。
    Mdns,
    /// 被动收到的定向广播 beacon。
    Beacon,
    /// `XCP1?` 单播探测的应答。
    Probe,
    /// 兜底 TCP connect 可达（默认关；id 未知——见模块文档"决策记录"）。
    TcpProbe,
}

/// 一台候选主控（docs/02 §2.2"候选列表"元素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 主控 TCP 服务地址（mDNS 取解析结果；beacon/探测取报文源 IP + 负载内
    /// port 字段——报文源端口是发送方临时端口，不能作服务地址）。
    pub addr: SocketAddr,
    /// 主控实例 UUID（16B 公开值；去重键 + PSK 盐输入，docs/02 §3.1）。
    /// TCP 兜底候选为全零（id 未知，消费方走手动安全码路径）。
    pub instance_id: [u8; 16],
    /// 显示名（日志/UI 呈现，FR-5.5 候选列表 M4 完善）。
    pub name: String,
    /// 发现来源（去重优先序与诊断）。
    pub source: CandidateSource,
}

impl Candidate {
    /// beacon → 候选（addr = 报文源 IP + beacon.port；port 0 = 畸形通告过滤）。
    #[must_use]
    pub fn from_beacon(beacon: Beacon, source_ip: IpAddr, source: CandidateSource) -> Option<Self> {
        if beacon.port == 0 {
            return None;
        }
        Some(Self {
            addr: SocketAddr::new(source_ip, beacon.port),
            instance_id: beacon.instance_id,
            name: beacon.name,
            source,
        })
    }

    /// TCP 兜底候选（id/name 未知，占位全零——去重与消费语义见模块文档）。
    #[must_use]
    pub fn tcp_reachable(addr: SocketAddr) -> Self {
        Self {
            addr,
            instance_id: [0; 16],
            name: String::new(),
            source: CandidateSource::TcpProbe,
        }
    }
}

/// 浏览器选项（[`Browser`] 入参）。
#[derive(Debug, Clone)]
pub struct BrowserOptions {
    /// 目标 UDP 发现端口（主控通告口；docs/02 §2.1 同端口）。
    pub discovery_port: u16,
    /// 显式 CIDR 列表（`net.cidrs`，"a.b.c.d/p"；空 → 各接口真实前缀）。
    pub cidrs: Vec<String>,
    /// mDNS browse 层开关。
    pub mdns: bool,
    /// 被动 beacon 监听开关（绑定发现端口收定向广播）。
    pub beacon_listen: bool,
    /// 主动 UDP 探测开关（逐 IP 发 `XCP1?`，docs/02 §2.2 第 2 层；与
    /// `beacon_listen` 同属 UDP 发现层——config `net.discovery` 的 "beacon"
    /// 同时控制两者，docs/01 §5；默认开）。
    pub udp_probe: bool,
    /// mDNS browse 收集窗（默认 [`MDNS_WINDOW`]）。
    pub mdns_window: Duration,
    /// 探测应答收集窗（默认 [`PROBE_WINDOW`]）。
    pub probe_window: Duration,
    /// 并发上限（默认 [`PROBE_CONCURRENCY`]）。
    pub max_concurrent: usize,
    /// 接口 allowlist（自动 CIDR 的接口过滤）。
    pub allowlist: Vec<String>,
    /// 兜底 TCP connect 扫描（docs/02 §2.2 第 3 层；默认关）。
    pub tcp_fallback: bool,
}

impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            discovery_port: xcontrol_config::DEFAULT_PORT,
            cidrs: Vec::new(),
            mdns: true,
            beacon_listen: true,
            udp_probe: true,
            mdns_window: MDNS_WINDOW,
            probe_window: PROBE_WINDOW,
            max_concurrent: PROBE_CONCURRENCY,
            allowlist: Vec::new(),
            tcp_fallback: false,
        }
    }
}

/// 主控浏览器（docs/02 §2.2）。无状态：[`Browser::scan_once`] 每轮独立建
/// socket/任务（漫游安全、无长生命周期资源），可跨轮复用同一实例。
#[derive(Debug, Clone)]
pub struct Browser {
    opts: BrowserOptions,
}

impl Browser {
    /// 构造（选项归一：零值窗口/并发回退默认，防配置失误扫成全窗零时）。
    #[must_use]
    pub fn new(mut opts: BrowserOptions) -> Self {
        if opts.mdns_window.is_zero() {
            opts.mdns_window = MDNS_WINDOW;
        }
        if opts.probe_window.is_zero() {
            opts.probe_window = PROBE_WINDOW;
        }
        if opts.max_concurrent == 0 {
            opts.max_concurrent = PROBE_CONCURRENCY;
        }
        Self { opts }
    }

    /// 实际生效选项（诊断）。
    #[must_use]
    pub const fn options(&self) -> &BrowserOptions {
        &self.opts
    }

    /// 一轮完整扫描（docs/02 §2.2）。恒成功返回候选列表（可能为空）——
    /// 单层失败降级为空产出，不中断调用方的重连循环（规则 4）。
    pub async fn scan_once(&self) -> Vec<Candidate> {
        // ① 被动 beacon 监听（后台起，②③ 期间并行收割）。
        let listener = self.spawn_beacon_listener();

        // ② mDNS browse（docs/02 §2.2 第 1 层；自有收集窗）。
        let mut candidates = if self.opts.mdns {
            self.browse_mdns().await
        } else {
            Vec::new()
        };

        // ③ UDP 探测（第 2 层）+ 被动 beacon 收割（广播兜底并行同窗）。
        //    探测口 = 本机任选临时端口（应答按源地址回包，无需固定口）；
        //    `udp_probe` 关时不建探测口（config "beacon" 关 = UDP 层整体静默）。
        if self.opts.udp_probe {
            match bind_discovery_socket(SocketAddr::from((IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))) {
                Ok(socket) => candidates.extend(self.probe_cidrs(Arc::new(socket)).await),
                Err(_) => { /* 探测层降级（规则 4）：跳到 beacon 收割 */ }
            }
        }
        if let Some((_, mut rx, task)) = listener {
            let window = self.opts.probe_window;
            let _ = tokio::time::timeout(window, async {
                while let Some(candidate) = rx.recv().await {
                    candidates.push(candidate);
                }
            })
            .await;
            task.abort();
        }

        // ④ 兜底 TCP connect（第 3 层，默认关；跳过已有候选地址）。
        if self.opts.tcp_fallback {
            let found: HashSet<SocketAddr> = candidates.iter().map(|c| c.addr).collect();
            candidates.extend(self.tcp_probe(&found).await);
        }

        dedupe_candidates(candidates)
    }

    /// 被动 beacon 监听任务：返回 (监听端口, 候选流, 任务句柄)。
    /// 绑定失败（如系统无可用口）→ `None`（监听层降级，规则 4）。
    fn spawn_beacon_listener(&self) -> Option<(u16, mpsc::Receiver<Candidate>, JoinHandle<()>)> {
        if !self.opts.beacon_listen {
            return None;
        }
        let socket = bind_discovery_socket(SocketAddr::from((
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            self.opts.discovery_port,
        )))
        .ok()?;
        let port = socket.local_addr().ok()?.port();
        let (tx, rx) = mpsc::channel::<Candidate>(32);
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, src)) = socket.recv_from(&mut buf).await else {
                    continue; // 瞬时错误
                };
                if let Some(candidate) = Beacon::decode(&buf[..n]).and_then(|beacon| {
                    Candidate::from_beacon(beacon, src.ip(), CandidateSource::Beacon)
                }) {
                    // 满载丢弃：候选是尽力而为流（下一轮扫描会再收）。
                    let _ = tx.try_send(candidate);
                }
            }
        });
        Some((port, rx, task))
    }

    /// mDNS browse（docs/02 §2.2 第 1 层）：`_xcontrol._tcp.local.` 解析事件
    /// → 候选。守护线程/browse 失败降级空产出；仅取 IPv4 地址（与 beacon/
    /// 握手链路一致的地址族；IPv6 连接性属后续里程碑）。
    async fn browse_mdns(&self) -> Vec<Candidate> {
        let Ok(daemon) = ServiceDaemon::new() else {
            return Vec::new();
        };
        let Ok(rx) = daemon.browse(SERVICE_TYPE) else {
            let _ = daemon.shutdown();
            return Vec::new();
        };
        let window_end = tokio::time::Instant::now() + self.opts.mdns_window;
        let mut out = Vec::new();
        loop {
            let event = match tokio::time::timeout_at(window_end, rx.recv_async()).await {
                Ok(event) => event,
                Err(_) => break, // 收集窗到
            };
            let Ok(event) = event else {
                break; // 事件通道关闭（守护线程退出）
            };
            if let Some(candidate) = match event {
                ServiceEvent::ServiceResolved(info) => candidate_from_mdns(&info),
                _ => None,
            } {
                out.push(candidate);
            }
        }
        let _ = daemon.stop_browse(SERVICE_TYPE);
        let _ = daemon.shutdown();
        out
    }

    /// UDP 并发探测（docs/02 §2.2 第 2 层）：CIDR 展开 → `Semaphore`(128)
    /// 门控发送 `XCP1?` → [`PROBE_WINDOW`] 收集应答（1s 硬帽）→ beacon 候选。
    async fn probe_cidrs(&self, socket: Arc<tokio::net::UdpSocket>) -> Vec<Candidate> {
        let targets = self.probe_targets();
        if targets.is_empty() {
            return Vec::new();
        }
        let semaphore = Arc::new(Semaphore::new(self.opts.max_concurrent));
        let mut send_tasks = Vec::with_capacity(targets.len());
        for ip in targets {
            let permit = Arc::clone(&semaphore);
            let socket = Arc::clone(&socket);
            let dst = SocketAddr::from((IpAddr::V4(ip), self.opts.discovery_port));
            send_tasks.push(tokio::spawn(async move {
                let _permit = permit.acquire_owned().await; // 128 并发上限
                let _ = socket.send_to(discovery::PROBE_MAGIC.as_slice(), dst).await;
            }));
        }

        // 应答收集：300ms 窗 ∩ 1s 探测段硬帽（docs/02 §2.2）。
        let window_end = (tokio::time::Instant::now() + self.opts.probe_window)
            .min(tokio::time::Instant::now() + SCAN_DEADLINE);
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        while out.len() < discovery::PROBE_TARGET_CAP {
            let received = tokio::time::timeout_at(window_end, socket.recv_from(&mut buf)).await;
            let Ok(Ok((n, src))) = received else {
                break; // 窗到或瞬时收包错误（ICMP 拒绝等）
            };
            if let Some(candidate) = Beacon::decode(&buf[..n])
                .and_then(|beacon| Candidate::from_beacon(beacon, src.ip(), CandidateSource::Probe))
            {
                out.push(candidate);
            }
        }
        for task in send_tasks {
            task.abort(); // 窗到即收：滞留发送任务回收（permit 自动归还）
        }
        out
    }

    /// 探测目标集：显式 CIDR（`net.cidrs`）优先；空 → 各接口真实前缀
    /// （docs/02 §2.2"默认=各接口所在 /24..等真实前缀"）。失败条目跳过；
    /// 全局去重 + [`discovery::PROBE_TARGET_CAP`] 截断（DoS 防护）。
    fn probe_targets(&self) -> Vec<Ipv4Addr> {
        let mut cidrs: Vec<(Ipv4Addr, u8)> = Vec::new();
        for entry in &self.opts.cidrs {
            if let Some(cidr) = discovery::parse_cidr(entry) {
                cidrs.push(cidr);
            }
        }
        if cidrs.is_empty() {
            cidrs = discovery::auto_cidrs(&self.opts.allowlist);
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for cidr in cidrs {
            for host in discovery::cidr_hosts(cidr) {
                if out.len() >= discovery::PROBE_TARGET_CAP {
                    return out;
                }
                if seen.insert(host) {
                    out.push(host);
                }
            }
        }
        out
    }

    /// 兜底 TCP connect 扫描（docs/02 §2.2 第 3 层，默认关；connect 级可达性
    /// ——见模块文档"决策记录"）。`skip` = 已发现候选的服务地址（不重复扫）。
    async fn tcp_probe(&self, skip: &HashSet<SocketAddr>) -> Vec<Candidate> {
        let overall_end = tokio::time::Instant::now() + SCAN_DEADLINE + TCP_PROBE_TIMEOUT;
        let semaphore = Arc::new(Semaphore::new(self.opts.max_concurrent));
        let mut tasks = Vec::new();
        for ip in self.probe_targets() {
            let dst = SocketAddr::from((IpAddr::V4(ip), self.opts.discovery_port));
            if skip.contains(&dst) {
                continue;
            }
            let permit = Arc::clone(&semaphore);
            tasks.push(tokio::spawn(async move {
                let _permit = permit.acquire_owned().await;
                let attempt =
                    tokio::time::timeout(TCP_PROBE_TIMEOUT, tokio::net::TcpStream::connect(dst))
                        .await;
                matches!(attempt, Ok(Ok(_))).then(|| Candidate::tcp_reachable(dst))
            }));
        }
        let mut out = Vec::new();
        for task in tasks {
            match tokio::time::timeout_at(overall_end, task).await {
                Ok(Ok(candidate)) => out.extend(candidate),
                _ => { /* 窗到/任务异常：跳过剩余 */ }
            }
            if tokio::time::Instant::now() >= overall_end {
                break;
            }
        }
        out
    }
}

/// mDNS 解析结果 → 候选（TXT `id` 必须是 32 位 hex 实例 UUID——id 是去重键
/// 与 PSK 盐输入，缺/坏 → 跳过；端口 0 过滤；`name` 缺省空串）。
fn candidate_from_mdns(info: &mdns_sd::ResolvedService) -> Option<Candidate> {
    let id_hex = info.get_property_val_str("id")?;
    let instance_id = parse_hex16(id_hex)?;
    let port = info.get_port();
    if port == 0 {
        return None;
    }
    let name = info
        .get_property_val_str("name")
        .unwrap_or_default()
        .to_owned();
    let ip = *info.get_addresses_v4().iter().next()?;
    Some(Candidate {
        addr: SocketAddr::from((IpAddr::V4(ip), port)),
        instance_id,
        name,
        source: CandidateSource::Mdns,
    })
}

/// 32 位 hex → 16 字节（TXT `id` 解析；长度/字符非法 → `None`）。
#[must_use]
fn parse_hex16(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// 按 instance id 去重（docs/02 §2.2"候选去重"）：有 id 候选首见保留（产出
/// 顺序即优先序：Mdns → Beacon → Probe → TcpProbe）；id 全零（TCP 兜底）按
/// 服务地址去重保留——消费方以手动安全码路径握手（模块文档"决策记录"）。
#[must_use]
pub fn dedupe_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen_ids = HashSet::new();
    let mut seen_addrs = HashSet::new();
    let mut out = Vec::new();
    for candidate in candidates {
        if candidate.instance_id == [0u8; 16] {
            if !seen_addrs.insert(candidate.addr) {
                continue;
            }
        } else if !seen_ids.insert(candidate.instance_id) {
            continue;
        }
        out.push(candidate);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// 候选去重：有 id 按首见（优先层保留）、全零 id 按地址去重、同 id 异
    /// 地址（mDNS 双网段解析）只留首见。
    #[test]
    fn dedupe_by_id_then_addr() {
        let mk = |id: u8, port: u16, source: CandidateSource| Candidate {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            instance_id: [id; 16],
            name: format!("host-{id}"),
            source,
        };
        let candidates = vec![
            mk(1, 4243, CandidateSource::Mdns),
            mk(1, 9999, CandidateSource::Probe), // 重复 id：丢弃（mDNS 首见保留）
            mk(2, 4243, CandidateSource::Beacon),
            Candidate::tcp_reachable("10.0.0.9:4243".parse().expect("常量")),
            Candidate::tcp_reachable("10.0.0.9:4243".parse().expect("常量")), // 同址去重
            mk(0, 4244, CandidateSource::TcpProbe),                           // 全零 id 异址：保留
        ];
        let out = dedupe_candidates(candidates);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].source, CandidateSource::Mdns, "首见（高优先层）保留");
        assert_eq!(out[0].addr.port(), 4243);
        assert_eq!(out[2].source, CandidateSource::TcpProbe);
        assert_eq!(out[3].addr.port(), 4244, "零 id 异址候选保留");
    }

    /// TXT id hex 解析：32 位合法 / 长度错 / 非法字符 / 大小写混合。
    #[test]
    fn parse_hex16_cases() {
        assert_eq!(parse_hex16(&"ab".repeat(16)), Some([0xab; 16]));
        assert_eq!(
            parse_hex16("00000000000000000000000000000ffF"),
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0f, 0xff])
        );
        assert!(parse_hex16(&"ab".repeat(15)).is_none(), "长度不足");
        assert!(parse_hex16(&"ab".repeat(17)).is_none(), "长度超出");
        assert!(parse_hex16(&"zz".repeat(16)).is_none(), "非法字符");
        assert!(parse_hex16("").is_none());
    }

    /// beacon → 候选：addr = 源 IP + beacon.port（服务端口，非报文源端口）；
    /// port 0 过滤。TCP 兜底候选占位全零 id。
    #[test]
    fn candidate_constructors() {
        let beacon = Beacon::announce(4243, "host", [5; 16]);
        let candidate = Candidate::from_beacon(
            beacon,
            IpAddr::V4("10.1.2.3".parse().expect("常量")),
            CandidateSource::Beacon,
        )
        .expect("合法");
        assert_eq!(candidate.addr.port(), 4243);
        assert_eq!(
            candidate.addr.ip(),
            IpAddr::V4("10.1.2.3".parse().expect("常量"))
        );
        assert_eq!(candidate.instance_id, [5; 16]);

        let zero = Beacon::announce(0, "broken", [5; 16]);
        assert!(
            Candidate::from_beacon(
                zero,
                IpAddr::V4("10.1.2.3".parse().expect("常量")),
                CandidateSource::Beacon
            )
            .is_none()
        );

        let tcp = Candidate::tcp_reachable("10.0.0.9:4243".parse().expect("常量"));
        assert_eq!(tcp.source, CandidateSource::TcpProbe);
        assert_eq!(tcp.instance_id, [0; 16]);
        assert!(tcp.name.is_empty());
    }

    /// 探测目标：显式 CIDR 优先 + 去重；坏条目跳过；空配置回退接口前缀；
    /// 超帽截断。
    #[test]
    fn probe_targets_explicit_first_then_auto() {
        let opts = BrowserOptions {
            cidrs: vec!["127.0.0.0/30".to_owned(), "bad-entry".to_owned()],
            ..BrowserOptions::default()
        };
        let browser = Browser::new(opts);
        let targets = browser.probe_targets();
        assert_eq!(
            targets,
            vec![
                "127.0.0.1".parse::<Ipv4Addr>().expect("常量"),
                "127.0.0.2".parse::<Ipv4Addr>().expect("常量"),
            ],
            "127.0.0.0/30 主机位 + 坏条目跳过"
        );

        let browser = Browser::new(BrowserOptions::default());
        assert!(!browser.probe_targets().is_empty(), "空配置回退接口前缀");

        let huge = BrowserOptions {
            cidrs: vec!["1.0.0.0/8".to_owned()],
            ..BrowserOptions::default()
        };
        assert_eq!(
            Browser::new(huge).probe_targets().len(),
            discovery::PROBE_TARGET_CAP,
            "超大网段截断到目标帽"
        );
    }

    /// 选项归一：零窗口/零并发回退默认。
    #[test]
    fn zero_options_fall_back_to_defaults() {
        let browser = Browser::new(BrowserOptions {
            mdns_window: Duration::ZERO,
            probe_window: Duration::ZERO,
            max_concurrent: 0,
            ..BrowserOptions::default()
        });
        assert_eq!(browser.options().mdns_window, MDNS_WINDOW);
        assert_eq!(browser.options().probe_window, PROBE_WINDOW);
        assert_eq!(browser.options().max_concurrent, PROBE_CONCURRENCY);
        assert!(browser.options().udp_probe, "UDP 探测默认开");
    }

    /// `udp_probe` 关：假主控在线也不会产出 Probe 候选（config "beacon" 关 =
    /// UDP 层整体静默——被动监听与主动探测同一开关，docs/01 §5 映射语义）。
    #[tokio::test]
    async fn udp_probe_disabled_skips_probe_layer() {
        // 假主控在线：若探测层未关，127.0.0.0/30 定点探测必然命中。
        let responder = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("假主控");
        let port = responder.local_addr().expect("地址").port();
        let reply = Beacon::announce(port, "mock", [0x31; 16]).encode();
        let responder_task = tokio::spawn(async move {
            let mut buf = [0u8; 16];
            loop {
                let Ok((n, src)) = responder.recv_from(&mut buf).await else {
                    continue;
                };
                if discovery::is_probe(&buf[..n]) {
                    let _ = responder.send_to(&reply, src).await;
                }
            }
        });

        let opts = BrowserOptions {
            cidrs: vec!["127.0.0.0/30".to_owned()],
            discovery_port: port,
            mdns: false,
            beacon_listen: false,
            udp_probe: false,
            ..BrowserOptions::default()
        };
        let candidates = Browser::new(opts).scan_once().await;
        responder_task.abort();
        assert!(
            candidates.is_empty(),
            "探测层关闭时不得有候选：{candidates:?}"
        );
    }

    /// **UDP 探测端到端（真实 socket）**：假主控应答 `XCP1?` → probe 层产出
    /// 候选（127.0.0.0/30 定点探测，不触真实网段）。
    /// `#[ignore]` 理由（PR #1 windows job 实测）：探测路径经**定向广播**
    /// （`bind_discovery_socket` 的 SO_BROADCAST 面），Windows 的 UDP 栈对
    /// 回环段广播不回投（正向单播应答测试同环境通过，红灯只集中在广播面）
    /// ——GitHub runner 沙箱上不可靠。本机 `cargo test -p xcontrol-net` 照常跑。
    #[cfg_attr(
        target_os = "windows",
        ignore = "Windows 对回环段定向广播不回投（平台 UDP 行为差异）；本机验证"
    )]
    #[tokio::test]
    async fn probe_cidrs_finds_mock_responder() {
        let responder = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("假主控");
        let port = responder.local_addr().expect("地址").port();
        let beacon = Beacon::announce(port, "mock-host", [0x42; 16]);
        let reply = beacon.encode();
        let responder_task = tokio::spawn(async move {
            let mut buf = [0u8; 16];
            loop {
                let Ok((n, src)) = responder.recv_from(&mut buf).await else {
                    continue;
                };
                if discovery::is_probe(&buf[..n]) {
                    let _ = responder.send_to(&reply, src).await;
                }
            }
        });

        let probe_socket =
            bind_discovery_socket("127.0.0.1:0".parse().expect("常量")).expect("探测口");
        let opts = BrowserOptions {
            cidrs: vec!["127.0.0.0/30".to_owned()],
            discovery_port: port,
            mdns: false,
            beacon_listen: false,
            probe_window: Duration::from_millis(400),
            ..BrowserOptions::default()
        };
        let candidates = Browser::new(opts).probe_cidrs(Arc::new(probe_socket)).await;
        responder_task.abort();

        assert_eq!(candidates.len(), 1, "命中假主控（未监听项不产候选）");
        assert_eq!(candidates[0].source, CandidateSource::Probe);
        assert_eq!(candidates[0].instance_id, [0x42; 16]);
        assert_eq!(
            candidates[0].addr.port(),
            port,
            "候选端口 = beacon 载荷端口"
        );
        assert_eq!(candidates[0].name, "mock-host");
    }

    /// 畸形应答不进候选（截断 / 坏 magic：静默丢弃不 panic）。
    #[tokio::test]
    async fn probe_cidrs_ignores_malformed_replies() {
        let responder = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("假主控");
        let port = responder.local_addr().expect("地址").port();
        let truncated = {
            let mut wire = Beacon::announce(4243, "t", [1; 16]).encode();
            wire.truncate(wire.len() - 3);
            wire
        };
        let bad_magic = {
            let mut wire = Beacon::announce(4243, "t", [1; 16]).encode();
            wire[3] = b'2';
            wire
        };
        let responder_task = tokio::spawn(async move {
            let mut buf = [0u8; 16];
            loop {
                let Ok((n, src)) = responder.recv_from(&mut buf).await else {
                    continue;
                };
                if discovery::is_probe(&buf[..n]) {
                    let _ = responder.send_to(&truncated, src).await;
                    let _ = responder.send_to(&bad_magic, src).await;
                }
            }
        });

        let probe_socket =
            bind_discovery_socket("127.0.0.1:0".parse().expect("常量")).expect("探测口");
        let opts = BrowserOptions {
            cidrs: vec!["127.0.0.0/30".to_owned()],
            discovery_port: port,
            mdns: false,
            beacon_listen: false,
            probe_window: Duration::from_millis(400),
            ..BrowserOptions::default()
        };
        let candidates = Browser::new(opts).probe_cidrs(Arc::new(probe_socket)).await;
        responder_task.abort();
        assert!(candidates.is_empty(), "畸形应答全弃：{candidates:?}");
    }

    /// 被动 beacon 监听：向监听口发 beacon → 候选流入（广播兜底层）。
    #[tokio::test]
    async fn beacon_listener_collects_broadcasts() {
        let opts = BrowserOptions {
            mdns: false,
            discovery_port: 0, // 临时端口：不与真实发现口竞争
            ..BrowserOptions::default()
        };
        let browser = Browser::new(opts);
        let (port, mut rx, task) = browser.spawn_beacon_listener().expect("监听层可用");

        // 模拟主控广播：从任意口发 beacon 到监听口。
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("发送口");
        let beacon = Beacon::announce(4243, "bc-host", [0x77; 16]);
        sender
            .send_to(&beacon.encode(), ("127.0.0.1", port))
            .await
            .expect("beacon 发出");

        let candidate = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("2s 内收到")
            .expect("通道存活");
        assert_eq!(candidate.source, CandidateSource::Beacon);
        assert_eq!(candidate.instance_id, [0x77; 16]);
        assert_eq!(candidate.addr.port(), 4243, "候选端口 = beacon 载荷端口");
        task.abort();
    }

    /// `XCP1?` 应答全链路（假主控 ↔ 假从机，双进程同语义的最小复现）：
    /// 探测 → 应答 → 候选 →（外部）握手衔接由 app 的 session_rt 承担。
    /// `#[ignore]` 理由：同 [`probe_cidrs_finds_mock_responder`]——探测经
    /// 回环段定向广播，Windows 不回投（平台 UDP 行为差异，PR #1 windows job
    /// 实测）；本机 `cargo test -p xcontrol-net` 照常跑。
    #[cfg_attr(
        target_os = "windows",
        ignore = "Windows 对回环段定向广播不回投（平台 UDP 行为差异）；本机验证"
    )]
    #[tokio::test]
    async fn probe_then_candidate_end_to_end() {
        let responder = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("假主控");
        let port = responder.local_addr().expect("地址").port();
        let beacon = Beacon::announce(port, "e2e-host", [0x99; 16]);
        let reply = beacon.encode();
        tokio::spawn(async move {
            let mut buf = [0u8; 16];
            loop {
                let Ok((n, src)) = responder.recv_from(&mut buf).await else {
                    continue;
                };
                if discovery::is_probe(&buf[..n]) {
                    let _ = responder.send_to(&reply, src).await;
                }
            }
        });

        // scan_once 全路径（mDNS 关、beacon 监听开——监听口用同临时端口会与
        // 假主控无关，beacon_listen 关闭以保持测试最小）。
        let opts = BrowserOptions {
            cidrs: vec!["127.0.0.0/30".to_owned()],
            discovery_port: port,
            mdns: false,
            beacon_listen: false,
            probe_window: Duration::from_millis(400),
            ..BrowserOptions::default()
        };
        let candidates = Browser::new(opts).scan_once().await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].instance_id, [0x99; 16]);
    }

    /// mDNS 通告 ↔ 浏览闭环（[`crate::announce::Announcer`] 注册 →
    /// [`Browser::scan_once`] 第 1 层发现；11-17-m3-discovery 双进程演练
    /// 已在本机跑通同链路，本测试是可重复的进程内等价物）。
    ///
    /// `#[ignore]` 理由（testing.md 禁止无注释忽略）：① CI 三平台沙箱的组播
    /// 栈可用性不确定，红灯噪音；② 测试运行会向所在真实网络注册临时
    /// `_xcontrol` 服务（LAN 污染面）。本机验证：
    /// `cargo test -p xcontrol-net -- --ignored mdns_announce_browse`。
    #[tokio::test]
    #[ignore = "CI 沙箱组播不确定且会向真实 LAN 注册临时服务；本机 --ignored 验证"]
    async fn mdns_announce_browse_roundtrip_local() {
        let opts = crate::announce::AnnouncerOptions {
            discovery_port: 0,
            service_port: 4299,
            name: "mdns-loopback-test".to_owned(),
            instance_id: [0x5E; 16],
            caps: 0x9,
            mdns: true,
            beacon: false, // 不发真实广播（beacon 层由其它测试/演练覆盖）
            allowlist: Vec::new(),
            period_unpaired: Duration::from_secs(3600),
            period_paired: Duration::from_secs(3600),
        };
        let announcer = crate::announce::Announcer::start(opts)
            .await
            .expect("mDNS 通告启动（本机需要组播栈）");

        let browser = Browser::new(BrowserOptions {
            mdns: true,
            beacon_listen: false,
            ..BrowserOptions::default()
        });
        // mDNS 首问首答通常 <1s；6s 上限兜底（含守护线程启动）。
        let candidates = tokio::time::timeout(Duration::from_secs(6), browser.scan_once()).await;
        announcer.stop();
        let candidates = candidates.expect("扫描在 6s 内完成");
        let hit = candidates
            .iter()
            .find(|candidate| candidate.instance_id == [0x5E; 16])
            .expect("本机 mDNS 自见：注册的实例被 browse 命中");
        assert_eq!(hit.name, "mdns-loopback-test");
        assert_eq!(hit.addr.port(), 4299);
        assert_eq!(hit.source, CandidateSource::Mdns);
    }
}
