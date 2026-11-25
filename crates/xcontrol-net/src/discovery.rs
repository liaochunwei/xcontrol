//! 发现层纯逻辑（docs/02 §2）：XCD1 beacon 编解码、XCP1? 探测识别、定向广播
//! 地址计算、网段解析与网络接口过滤启发式。
//!
//! 本模块**零 I/O、零锁、零时钟读取**——announce.rs / browse.rs 的 async 层
//! 只做薄封装，全部决策逻辑集中在此，保证表驱动测试可覆盖（testing.md）。
//!
//! # beacon 负载（docs/02 §2.1 逐字）
//!
//! ```text
//! magic "XCD1" | proto u16 (LE) | port u16 (LE) | name_len u8 | name | id 16B
//! ```
//!
//! - `proto`：XCP 协议大版本（[`BEACON_PROTO`]）；解析端容忍任意 u16（前向
//!   兼容），消费端按版本过滤；
//! - `port`：主控 TCP 服务端口（发现载荷只做"指路"，不做传输）；
//! - `name`：显示名（UTF-8，≤ [`NAME_MAX_BYTES`] 字节——u8 长度前缀上限）；
//! - `id`：主控实例 UUID（16B，公开值——PSK 盐的输入，docs/02 §3.1）。
//!
//! # 安全红线（docs/02 §0、07-security.md §2：发现流量零安全码派生物）
//!
//! beacon / 探测应答 / mDNS TXT 的负载组装函数**只接收**公开字段（端口、显示名、
//! 实例 UUID、能力掩码），签名上不存在安全码入参——任何派生链值（盐/PSK）都
//! 无路径进入负载；`tests::discovery_payload_carries_no_secret_derivatives`
//! 用已知测试码派生后逐字节断言（防回归锚点）。
//!
//! # 畸形包纪律（FR-5.1 鲁棒性）
//!
//! [`Beacon::decode`] 对截断 / 坏 magic / 超长 name_len / 非 UTF-8 一律返回
//! `None`（丢弃），**永不 panic**——发现流量来自不可信局域网，解析失败是常态
//! 而非异常（error-handling.md 规则 3）。
//!
//! # 广播地址（docs/02 §2.1"定向广播"）
//!
//! [`directed_broadcast`]：接口 IP + 前缀长度 → 该子网定向广播地址
//! （10.0.0.5/24 → 10.0.0.255）；前缀未知时 /24 兜底（家庭/办公局域网主流
//! 掩码，广播打错网段的最坏情形是"发到本网段不存在的广播域"= 丢弃，无泄漏面）。
//! IPv6 无广播语义——beacon 层仅 IPv4（IPv6 发现由 mDNS 层覆盖）。

use std::net::Ipv4Addr;

/// beacon 帧魔数（docs/02 §2.1）。
pub const BEACON_MAGIC: [u8; 4] = *b"XCD1";
/// 探测负载（docs/02 §2.1：从机单播 `XCP1?` → 主控单播回应 beacon）。
pub const PROBE_MAGIC: [u8; 5] = *b"XCP1?";
/// beacon 内 `proto` 字段的当前值 = XCP 协议大版本（docs/02 §5）。
pub const BEACON_PROTO: u16 = 1;
/// 显示名字节上限（u8 长度前缀的天然上限；编码端按 UTF-8 字符边界截断）。
pub const NAME_MAX_BYTES: usize = u8::MAX as usize;
/// beacon 帧定长部分 = magic(4) + proto(2) + port(2) + name_len(1) + id(16)。
const FIXED_LEN: usize = 4 + 2 + 2 + 1 + 16;
/// 实例 UUID 字节数（docs/02 §3.1 salt 输入；公开值）。
const ID_LEN: usize = 16;
/// 前缀未知时的兜底掩码（家庭/办公局域网主流 /24，见模块文档"广播地址"节）。
pub const FALLBACK_PREFIX: u8 = 24;
/// 单轮扫描展开的探测目标上限（DoS 防护：配置失误如 /8 不至于展开 1600 万目标；
/// 截断在 [`cidr_hosts`] 内完成，超出部分丢弃——扫描本来就是尽力而为层）。
pub const PROBE_TARGET_CAP: usize = 4096;

/// 主控通告载荷（docs/02 §2.1 beacon 负载的结构化形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// XCP 协议大版本（消费端按版本过滤，解析端容忍任意值）。
    pub proto: u16,
    /// 主控 TCP 服务端口。
    pub port: u16,
    /// 显示名（UTF-8）。
    pub name: String,
    /// 主控实例 UUID（16B 公开值）。
    pub instance_id: [u8; 16],
}

impl Beacon {
    /// 通告 beacon 构造（proto 取当前协议大版本 [`BEACON_PROTO`]）。
    #[must_use]
    pub fn announce(port: u16, name: impl Into<String>, instance_id: [u8; 16]) -> Self {
        Self {
            proto: BEACON_PROTO,
            port,
            name: name.into(),
            instance_id,
        }
    }

    /// 编码进既有缓冲（docs/02 §2.1 逐字：全小端；name 超 [`NAME_MAX_BYTES`]
    /// 时按 UTF-8 字符边界截断——截断只损失显示名可读性，id/端口不受影响）。
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&BEACON_MAGIC);
        out.extend_from_slice(&self.proto.to_le_bytes());
        out.extend_from_slice(&self.port.to_le_bytes());
        let truncated = truncate_utf8(&self.name, NAME_MAX_BYTES);
        let name = truncated.as_bytes();
        out.push(u8::try_from(name.len()).unwrap_or(NAME_MAX_BYTES as u8));
        out.extend_from_slice(name);
        out.extend_from_slice(&self.instance_id);
    }

    /// 编码为新缓冲（便利封装，见 [`Beacon::encode_into`]）。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FIXED_LEN + self.name.len());
        self.encode_into(&mut out);
        out
    }

    /// 解析 beacon（畸形 → `None`，永不 panic；纪律见模块文档"畸形包纪律"）。
    ///
    /// 结构校验：总长 == 定长部分 + name_len（严丝合缝，拒绝尾部垃圾——
    /// 尾部字节可能携带未来扩展，按当前协议版本一概视为畸形）。
    #[must_use]
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < FIXED_LEN {
            return None;
        }
        if buf[..4] != BEACON_MAGIC {
            return None;
        }
        let proto = u16::from_le_bytes([buf[4], buf[5]]);
        let port = u16::from_le_bytes([buf[6], buf[7]]);
        let name_len = buf[8] as usize;
        if buf.len() != FIXED_LEN + name_len {
            return None;
        }
        let name = std::str::from_utf8(&buf[9..9 + name_len]).ok()?;
        let mut instance_id = [0u8; ID_LEN];
        instance_id.copy_from_slice(&buf[9 + name_len..9 + name_len + ID_LEN]);
        Some(Self {
            proto,
            port,
            name: name.to_owned(),
            instance_id,
        })
    }
}

/// 按 UTF-8 字符边界截断到 `max_bytes`（编码端兜底，避免多字节字符被劈半）。
fn truncate_utf8(name: &str, max_bytes: usize) -> &str {
    if name.len() <= max_bytes {
        return name;
    }
    let mut end = max_bytes;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// 探测识别（docs/02 §2.1：负载恰为 `XCP1?` 才应答；其余静默丢弃）。
#[must_use]
pub fn is_probe(buf: &[u8]) -> bool {
    buf == PROBE_MAGIC
}

/// 探测应答构造（纯函数：探测 → beacon 负载；非探测 → `None`）。
///
/// 应答 = 主控当前通告 beacon（docs/02 §2.1"主控单播回应 beacon"）。
#[must_use]
pub fn probe_reply(buf: &[u8], beacon: &Beacon) -> Option<Vec<u8>> {
    is_probe(buf).then(|| beacon.encode())
}

/// 定向广播地址计算：接口 IP + 前缀 → 该子网广播地址（docs/02 §2.1）。
///
/// - `prefix` = `None` 或 > 32（非法输入）→ [`FALLBACK_PREFIX`] 兜底；
/// - /0 → 255.255.255.255（受约束广播）；/32 → 主机自身（单机网段无广播域）。
///
/// IPv6 无广播——本函数仅接受 IPv4（见模块文档"广播地址"节）。
#[must_use]
pub fn directed_broadcast(ip: Ipv4Addr, prefix: Option<u8>) -> Ipv4Addr {
    let prefix = match prefix {
        Some(p) if p <= 32 => p,
        _ => FALLBACK_PREFIX,
    };
    let p = u32::from(prefix);
    // 掩码 = 前 p 位全 1。p=0 时 32-0=32 位左移溢出 → checked_shl 返回 None → 掩码 0。
    let mask = u32::MAX.checked_shl(32 - p).unwrap_or(0);
    Ipv4Addr::from(ip.to_bits() | !mask)
}

/// 解析 CIDR 记法（仅 IPv4：`a.b.c.d/p`，0 ≤ p ≤ 32）；无前缀/非法/IPv6 → `None`。
///
/// 配置里的 `net.cidrs`（docs/01 §5）逐条经此解析；失败条目由调用方记日志跳过。
#[must_use]
pub fn parse_cidr(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip_part, prefix_part) = cidr.split_once('/')?;
    let ip = ip_part.parse::<Ipv4Addr>().ok()?;
    let prefix = prefix_part.parse::<u8>().ok()?;
    (prefix <= 32).then_some((ip, prefix))
}

/// IP 是否落在 CIDR 网段内（FR-5.4 主控白名单匹配的核心谓词，纯函数）。
///
/// 掩码语义与 [`directed_broadcast`]/[`cidr_hosts`] 一致（前 p 位全 1）；
/// 主机位不参与比较（`10.0.0.5/24` 的 `5` 被掩掉——配置侧写网络地址或任意
/// 网段内地址等价）。
#[must_use]
pub fn cidr_contains(cidr: (Ipv4Addr, u8), ip: Ipv4Addr) -> bool {
    let (base, prefix) = cidr;
    let p = u32::from(prefix);
    let mask = u32::MAX.checked_shl(32 - p).unwrap_or(0);
    base.to_bits() & mask == ip.to_bits() & mask
}

/// CIDR → 可探测主机地址列表（排除网络地址与广播地址；/31 例外——RFC 3021
/// 点对点链路两端都是主机；/32 单机）。
///
/// 截断纪律：目标数超 [`PROBE_TARGET_CAP`] 时按序截断（DoS 防护，见常量文档）。
/// 展开用 u64 算术（/0 的 2^32 超出 u32；边界比较防回绕）。
#[must_use]
pub fn cidr_hosts(cidr: (Ipv4Addr, u8)) -> Vec<Ipv4Addr> {
    let (ip, prefix) = cidr;
    let p = u32::from(prefix);
    let mask = u32::MAX.checked_shl(32 - p).unwrap_or(0);
    let network = u64::from(ip.to_bits() & mask);
    // 网段大小 = 反掩码 + 1（/0 → 2^32）。
    let total = u64::from(!mask) + 1;
    // /31 与 /32 无"网络+广播"预留（RFC 3021 / 单机路由）；其余首末各让一位。
    let (first, last) = if prefix >= 31 {
        (network, network + total - 1)
    } else {
        (network + 1, network + total - 2)
    };
    let mut hosts = Vec::new();
    let mut cur = first;
    while hosts.len() < PROBE_TARGET_CAP && cur <= last {
        // cur ≤ last ≤ u32::MAX（last 由 u32 位运算推导），try_from 不可达失败。
        hosts.push(Ipv4Addr::from(u32::try_from(cur).unwrap_or(u32::MAX)));
        cur += 1;
    }
    hosts
}

/// 虚拟网卡名字启发式（docs/02 §2.1"跳过虚拟网卡"）：`lo*`（回环）、`utun*`
/// （macOS 隧道）、`docker*` / `veth*`（容器）、`bridge*`（网桥）、`vpn*`
/// （VPN 虚拟口）。匹配 = 小写前缀命中。
///
/// 广播经虚拟口发不出去或发进错误广播域，只会制造噪音——默认跳过；例外走
/// allowlist（[`interfaces_ipv4`] 入参，用户显式声明优先于启发式）。
#[must_use]
pub fn is_virtual_interface(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["lo", "utun", "docker", "veth", "bridge", "vpn"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// 过滤后的 IPv4 接口清单（announce 广播 / browse 自动 CIDR 共用的入口）。
///
/// - 仅 IPv4（beacon 无 IPv6 广播语义）；IPv6 发现由 mDNS 覆盖；
/// - 跳过 [`is_virtual_interface`] 命中的接口，`allowlist`（接口名精确匹配）
///   显式覆盖启发式；
/// - 同名接口多地址（如 en0 双 IP）逐地址保留——每个地址各有自己的广播域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceV4 {
    /// 接口名（如 `en0`）。
    pub name: String,
    /// 接口 IPv4 地址。
    pub ip: Ipv4Addr,
    /// 前缀长度（if-addrs 自带 netmask → prefixlen；[0, 32]）。
    pub prefix: u8,
    /// 该地址所在子网的定向广播地址（[`directed_broadcast`]）。
    pub broadcast: Ipv4Addr,
}

/// 枚举本机 IPv4 接口（虚拟口过滤 + allowlist 覆盖，见 [`InterfaceV4`]）。
///
/// # Errors
/// 系统 getifaddrs 失败（io）——调用方降级（无广播层/无自动 CIDR）不中断发现。
pub fn interfaces_ipv4(allowlist: &[String]) -> std::io::Result<Vec<InterfaceV4>> {
    let all = if_addrs::get_if_addrs()?;
    let mut out: Vec<InterfaceV4> = Vec::new();
    for iface in all {
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue; // beacon 仅 IPv4
        };
        let allowed = allowlist.contains(&iface.name);
        if !allowed && is_virtual_interface(&iface.name) {
            continue;
        }
        out.push(InterfaceV4 {
            name: iface.name,
            ip: v4.ip,
            prefix: v4.prefixlen,
            broadcast: directed_broadcast(v4.ip, Some(v4.prefixlen)),
        });
    }
    Ok(out)
}

/// 自动 CIDR：各 IPv4 接口所在真实前缀（docs/02 §2.2 第 2 层"默认=各接口所在
/// /24..等真实前缀"；`net.cidrs` 非空时调用方不用本函数）。
///
/// 去重（多接口同网段只探一轮）；失败 → 空（探测层静默降级，规则 4）。
#[must_use]
pub fn auto_cidrs(allowlist: &[String]) -> Vec<(Ipv4Addr, u8)> {
    let Ok(ifaces) = interfaces_ipv4(allowlist) else {
        return Vec::new();
    };
    let mut out: Vec<(Ipv4Addr, u8)> = Vec::new();
    for iface in ifaces {
        let cidr = (iface.ip, iface.prefix);
        if !out.contains(&cidr) {
            out.push(cidr);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // FR-5.x 验收对照表（docs/00 §4.5 × 本文件测试；PRD 验收标准映射）：
    //
    // | FR | 验收标准 | 本文件/相关文件锚点 |
    // | --- | --- | --- |
    // | FR-5.1 | beacon 负载 round-trip + 畸形负例不 panic | `beacon_round_trip` / `malformed_beacons_are_rejected_not_panicking` / `encode_truncates_name_on_char_boundary` |
    // | FR-5.1 | 安全码后 ≤5s 连上同网段主控 | `probe_detection_is_exact` + announce.rs 探测应答 / browse.rs 探测扫描（整体时延由本机双进程演练证明 ≈2s） |
    // | FR-5.1 | 发现流量零派生物 | `discovery_payload_carries_no_secret_derivatives`（盐/PSK/码原文逐字节断言） |
    // | FR-5.2 | 自动发现（UDP 广播探测）可用 | `probe_reply_is_beacon_payload` + announce.rs `responder_replies_to_probe_with_beacon`（真实 socket） |
    // | FR-5.2 | 手动 IP 直连并存 | M2 serve/connect（serve.rs 测试）+ app `--client <addr>`（main.rs 解析测试） |
    // | FR-5.3 | 退避 1s→30s 封顶/成功重置 | reconnect.rs 测试序列（本文件不涉） |
    use super::*;

    /// 测试通告（固定 uuid；docs/08 §4 假码纪律：uuid 非密、name 非密）。
    fn demo_beacon() -> Beacon {
        Beacon::announce(4243, "demo-host", [0xAB; 16])
    }

    /// round-trip：编 → 解逐字段相等（FR-5.1 验收项）。
    #[test]
    fn beacon_round_trip() {
        let beacon = demo_beacon();
        let decoded = Beacon::decode(&beacon.encode()).expect("合法编码必可解");
        assert_eq!(decoded, beacon);
    }

    /// 线格式逐字节断言（docs/02 §2.1 逐字：小端 + 字段序）。
    #[test]
    fn beacon_wire_format_is_little_endian_doc_order() {
        let beacon = Beacon::announce(0x0102, "ab", [0xCD; 16]);
        let wire = beacon.encode();
        assert_eq!(&wire[..4], b"XCD1", "magic 逐字");
        assert_eq!(wire[4], 0x01, "proto u16 小端低字节（=1）");
        assert_eq!(wire[5], 0x00, "proto u16 小端高字节");
        assert_eq!(wire[6], 0x02, "port u16 小端低字节");
        assert_eq!(wire[7], 0x01, "port u16 小端高字节");
        assert_eq!(wire[8], 2, "name_len u8 = 2");
        assert_eq!(&wire[9..11], b"ab", "name 原样");
        assert_eq!(&wire[11..27], &[0xCD; 16], "id 16B 收尾");
        assert_eq!(wire.len(), FIXED_LEN + 2);
    }

    /// 畸形负例矩阵：截断（各断点）/ 坏 magic / 超长 name_len / 非 UTF-8 /
    /// 尾部垃圾 / 空缓冲——一律 `None`，永不 panic（FR-5.1 验收项）。
    #[test]
    fn malformed_beacons_are_rejected_not_panicking() {
        let wire = demo_beacon().encode();
        // ① 逐字节截断（含 0 长度与全长度——全长度在上文 round-trip 已验）。
        for cut in 0..wire.len() {
            assert!(
                Beacon::decode(&wire[..cut]).is_none(),
                "截断到 {cut} 字节必须被拒绝"
            );
        }
        // ② 坏 magic（改首字节）。
        let mut bad_magic = wire.clone();
        bad_magic[0] = b'X';
        bad_magic[1] = b'C';
        bad_magic[2] = b'D';
        bad_magic[3] = b'2';
        assert!(Beacon::decode(&bad_magic).is_none(), "坏 magic 必须被拒绝");
        // ③ name_len 超过实际（声称 200 字节 name，缓冲只有 2 字节）。
        let mut overlong = wire.clone();
        overlong[8] = 200;
        assert!(
            Beacon::decode(&overlong).is_none(),
            "超长 name_len 必须被拒绝"
        );
        // ④ name 非 UTF-8（0xFF 不合法）。
        let mut non_utf8 = wire.clone();
        non_utf8[9] = 0xFF;
        assert!(
            Beacon::decode(&non_utf8).is_none(),
            "非 UTF-8 name 必须被拒绝"
        );
        // ⑤ 尾部垃圾（结构严丝合缝纪律）。
        let mut trailing = wire.clone();
        trailing.push(0x00);
        assert!(Beacon::decode(&trailing).is_none(), "尾部垃圾必须被拒绝");
        // ⑥ 空与超短缓冲。
        assert!(Beacon::decode(&[]).is_none());
        assert!(Beacon::decode(&[b'X'; 3]).is_none());
    }

    /// 显示名截断：255 字节上限按 UTF-8 字符边界（多字节字符不劈半），
    /// 截断后的编码仍可 round-trip。
    #[test]
    fn encode_truncates_name_on_char_boundary() {
        // 每个 '马' 3 字节：100 个 = 300 字节 > 255。
        let long_name = "马".repeat(100);
        let beacon = Beacon::announce(4243, long_name.as_str(), [1; 16]);
        let wire = beacon.encode();
        assert_eq!(wire[8], 255, "name_len 恰为 u8 上限");
        assert_eq!(wire.len(), FIXED_LEN + 255);
        let decoded = Beacon::decode(&wire).expect("截断后仍合法");
        assert_eq!(
            decoded.name.len(),
            255,
            "截断到字符边界（255 是 3 的倍数，'马' 整字符保留）"
        );
        assert_eq!(decoded.name, "马".repeat(85));
        // 中文字符跨边界的情形：255 不是 3 的倍数时回退到更小的边界。
        let mixed = "aa马".repeat(100); // 每组 5 字节 = 500 字节
        let beacon = Beacon::announce(4243, mixed.as_str(), [1; 16]);
        let wire = beacon.encode();
        let decoded = Beacon::decode(&wire).expect("仍合法");
        assert!(decoded.name.is_char_boundary(decoded.name.len()));
    }

    /// 边界：空显示名 round-trip（name_len = 0）。
    #[test]
    fn empty_name_round_trip() {
        let beacon = Beacon::announce(4243, "", [2; 16]);
        assert_eq!(Beacon::decode(&beacon.encode()), Some(beacon));
    }

    /// 定向广播表驱动（design.md：10.0.0.5/24 → 10.0.0.255 等）。
    #[test]
    fn directed_broadcast_table() {
        let cases = [
            ("10.0.0.5", Some(24u8), "10.0.0.255"),
            ("10.1.2.3", Some(8), "10.255.255.255"),
            ("172.16.5.9", Some(16), "172.16.255.255"),
            // /25 下半段（.0-.127）：.100 属 192.168.1.0/25，广播 .127。
            ("192.168.1.100", Some(25), "192.168.1.127"),
            // /25 上半段（.128-.255）：.200 属 192.168.1.128/25，广播 .255。
            ("192.168.1.200", Some(25), "192.168.1.255"),
            ("192.168.1.5", Some(30), "192.168.1.7"),
            ("10.0.0.4", Some(31), "10.0.0.5"), // RFC 3021：两端皆主机
            ("10.0.0.7", Some(32), "10.0.0.7"), // /32：广播 = 自身
            ("0.0.0.0", Some(0), "255.255.255.255"), // 受约束广播
            ("10.0.0.5", None, "10.0.0.255"),   // 前缀未知 → /24 兜底
            ("10.0.0.5", Some(33), "10.0.0.255"), // 非法前缀 → /24 兜底
            ("10.0.0.5", Some(255), "10.0.0.255"), // u8 溢出输入 → /24 兜底
        ];
        for (ip, prefix, expect) in cases {
            let ip: Ipv4Addr = ip.parse().expect("测试常量");
            assert_eq!(
                directed_broadcast(ip, prefix),
                expect.parse::<Ipv4Addr>().expect("测试常量"),
                "{ip}/{prefix:?} → {expect}"
            );
        }
    }

    /// CIDR 解析：合法/缺前缀/越界前缀/非法 IP/IPv6 → None。
    #[test]
    fn parse_cidr_cases() {
        assert_eq!(
            parse_cidr("10.0.0.0/24"),
            Some(("10.0.0.0".parse().expect("常量"), 24))
        );
        assert_eq!(
            parse_cidr("192.168.1.10/32"),
            Some(("192.168.1.10".parse().expect("常量"), 32))
        );
        assert!(parse_cidr("10.0.0.5").is_none(), "缺前缀");
        assert!(parse_cidr("10.0.0.0/33").is_none(), "前缀越界");
        assert!(parse_cidr("300.0.0.0/24").is_none(), "非法 IP");
        assert!(
            parse_cidr("::1/64").is_none(),
            "IPv6 不支持（beacon 仅 v4）"
        );
        assert!(parse_cidr("").is_none());
        assert!(parse_cidr("/24").is_none());
    }

    /// CIDR 包含判定表驱动（FR-5.4 白名单谓词）：网段内/外/主机位掩掉/单机
    /// /0 全含/32 精确。
    #[test]
    fn cidr_contains_table() {
        let ip = |raw: &str| raw.parse::<Ipv4Addr>().expect("测试常量");
        let cases = [
            (("10.0.0.0", 24u8), "10.0.0.55", true, "网段内主机"),
            (("10.0.0.0", 24), "10.0.1.55", false, "网段外"),
            // 主机位不参与比较：配置写网段内任意地址等价网络地址。
            (("10.0.0.5", 24), "10.0.0.200", true, "主机位被掩掉"),
            (("192.168.1.0", 32), "192.168.1.0", true, "/32 精确命中"),
            (("192.168.1.0", 32), "192.168.1.1", false, "/32 精确未命中"),
            (("0.0.0.0", 0), "203.0.113.9", true, "/0 全含"),
            (("127.0.0.1", 8), "127.0.0.2", true, "回环 /8（测试用）"),
        ];
        for ((base, prefix), probe, expect, why) in cases {
            assert_eq!(
                cidr_contains((ip(base), prefix), ip(probe)),
                expect,
                "{base}/{prefix} ∋ {probe}（{why}）"
            );
        }
    }

    /// CIDR 展开：/24 → 254 个主机（除网络+广播）；/31 → 2 个；/32 → 1 个；
    /// 超大网段截断到 [`PROBE_TARGET_CAP`]。
    #[test]
    fn cidr_hosts_expansion_and_cap() {
        let hosts = cidr_hosts(("10.0.0.0".parse::<Ipv4Addr>().expect("常量"), 24));
        assert_eq!(hosts.len(), 254);
        assert_eq!(
            hosts[0],
            "10.0.0.1".parse::<Ipv4Addr>().expect("常量"),
            "排除网络地址"
        );
        assert_eq!(
            hosts[253],
            "10.0.0.254".parse::<Ipv4Addr>().expect("常量"),
            "排除广播地址"
        );

        let hosts = cidr_hosts(("10.0.0.4".parse::<Ipv4Addr>().expect("常量"), 31));
        assert_eq!(hosts.len(), 2, "RFC 3021 两端皆主机");
        let hosts = cidr_hosts(("10.0.0.7".parse::<Ipv4Addr>().expect("常量"), 32));
        assert_eq!(hosts.len(), 1);

        let hosts = cidr_hosts(("10.0.0.0".parse::<Ipv4Addr>().expect("常量"), 8));
        assert_eq!(hosts.len(), PROBE_TARGET_CAP, "超大网段截断");
    }

    /// 虚拟网卡启发式（design.md 名单：lo/utun/docker/bridge/vpn + veth）。
    #[test]
    fn virtual_interface_heuristics() {
        for name in ["lo0", "utun3", "docker0", "bridge100", "vpn0", "veth1a2b"] {
            assert!(is_virtual_interface(name), "{name} 应被过滤");
        }
        for name in ["en0", "eth0", "wlan0", "en5", "Ethernet 2"] {
            assert!(!is_virtual_interface(name), "{name} 不应被过滤");
        }
        // 大小写不敏感（Windows 接口名大小写混杂）。
        assert!(is_virtual_interface("LO0"));
    }

    /// 本机接口枚举：不污染断言（环境相关），只断言结构不变式——
    /// 每条记录的 broadcast 与 (ip, prefix) 自洽；allowlist 能捞回虚拟口。
    /// 回环口断言仅 macOS/Linux（11-25 PR #1 windows job 实测：Windows 无
    /// `lo0`/`lo` 命名惯例——回环口不参与广播发现，allowlist 启发式按
    /// 平台命名约定，断言随平台走）。
    #[test]
    fn interface_enumeration_invariants() {
        let ifaces = interfaces_ipv4(&[]).expect("本机枚举可用");
        for iface in &ifaces {
            assert!(!is_virtual_interface(&iface.name), "虚拟口已过滤");
            assert_eq!(
                iface.broadcast,
                directed_broadcast(iface.ip, Some(iface.prefix)),
                "broadcast 字段与 ip/prefix 自洽"
            );
        }
        // allowlist 覆盖启发式：lo0 在 macOS/Linux 必存在（回环）。
        let with_lo = interfaces_ipv4(&["lo0".to_owned(), "lo".to_owned()]).expect("枚举可用");
        #[cfg(not(target_os = "windows"))]
        assert!(
            with_lo.iter().any(|i| i.name.starts_with("lo")),
            "allowlist 应放行回环口"
        );
        #[cfg(target_os = "windows")]
        assert!(
            with_lo.iter().any(|i| !is_virtual_interface(&i.name)),
            "allowlist 只过滤虚拟口（Windows 无 lo 命名，回环口若被枚举则放行）"
        );
    }

    /// 自动 CIDR：非空环境至少产出一条（有真实网卡）；同网段去重。
    #[test]
    fn auto_cidrs_non_empty_and_deduped() {
        let cidrs = auto_cidrs(&[]);
        assert!(!cidrs.is_empty(), "本机必有真实 IPv4 接口");
        let len_before = cidrs.len();
        let unique = cidrs.clone();
        let mut dedup = unique.clone();
        dedup.dedup();
        assert_eq!(len_before, dedup.len(), "无重复 CIDR");
    }

    /// 探测识别：恰为 `XCP1?` 才真（大小写/长度/前缀变体全假）。
    #[test]
    fn probe_detection_is_exact() {
        assert!(is_probe(b"XCP1?"));
        assert!(!is_probe(b"XCP1"));
        assert!(!is_probe(b"XCP1??"));
        assert!(!is_probe(b"xcp1?"));
        assert!(!is_probe(b"XCD1?"));
        assert!(!is_probe(&[]));
    }

    /// 探测应答 = 当前 beacon 负载；非探测 → None（docs/02 §2.1 应答语义）。
    #[test]
    fn probe_reply_is_beacon_payload() {
        let beacon = demo_beacon();
        let reply = probe_reply(b"XCP1?", &beacon).expect("探测必有应答");
        assert_eq!(reply, beacon.encode(), "应答逐字节等于 beacon 负载");
        assert!(probe_reply(b"garbage", &beacon).is_none());
    }

    /// **安全红线测试（docs/02 §0 / 07-security.md §2）**：beacon 负载与探测
    /// 应答**零安全码派生物**——用已知测试码走完整派生链（盐 → PSK），断言
    /// 派生值与码原文均不以任何形态出现在发现负载中。
    #[test]
    fn discovery_payload_carries_no_secret_derivatives() {
        let code = "test-code-zero-deriv-2026";
        let uuid: [u8; 16] = [0x5A; 16];
        // 完整派生链（docs/02 §3.1）：盐 = blake3(uuid 域分离)，PSK = argon2(code, 盐)。
        let salt = crate::psk::derive_salt(&uuid);
        let psk = crate::psk::derive_psk(code, &uuid).expect("固定输入派生成功");

        let beacon = Beacon::announce(4243, "sec-host", uuid);
        let mut wire = beacon.encode();
        // 探测应答与 beacon 同源（已由 probe_reply_is_beacon_payload 断言等价），
        // 这里再独立断言一次应答路径的输出。
        if let Some(reply) = probe_reply(PROBE_MAGIC.as_slice(), &beacon) {
            wire.extend_from_slice(&reply);
        }

        // 断言（三重形态）：码原文 / 盐(16B) / PSK(32B) 不以 ① 原始字节、
        // ② 小写 hex 文本、③ 大写 hex 文本出现在负载中——子串搜索对整段
        // wire 做，覆盖任意字段对齐/跨字段拼接；hex 形态覆盖"派生值被文本
        // 编码进 name/TXT"的回归面（本 crate 存在 hex 编码器，见 announce::hex）。
        let hex_lower = |bytes: &[u8]| crate::announce::hex(bytes);
        let hex_upper = |bytes: &[u8]| hex_lower(bytes).to_ascii_uppercase();
        let text_forms = |bytes: &[u8]| {
            [
                bytes.to_vec(),
                hex_lower(bytes).into_bytes(),
                hex_upper(bytes).into_bytes(),
            ]
        };
        for (label, forms) in [
            ("安全码原文", text_forms(code.as_bytes())),
            ("盐", text_forms(&salt)),
            ("PSK", text_forms(&psk)),
        ] {
            for form in &forms {
                assert!(
                    !contains_subslice(&wire, form),
                    "{label}（{} 字节形态）泄漏进发现负载（WPS 红线，docs/02 §0）",
                    form.len()
                );
            }
        }
        // 负载只含公开字段：magic、proto、port、name、uuid。
        assert!(wire.starts_with(&BEACON_MAGIC));
        assert_eq!(Beacon::decode(&wire[..beacon.encode().len()]), Some(beacon));
    }

    /// 子串搜索（测试辅助；标准库无 &[u8] contains）。
    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || haystack.len() < needle.len() {
            return false;
        }
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
