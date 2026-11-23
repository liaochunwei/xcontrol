# ADR-0002：传输层、加密与发现机制选型

- 状态：已接受
- 日期：2026-11-15
- 关联需求：FR-1.4、FR-4.5、FR-5.1/5.2/5.3、NFR-1、NFR-3、NFR-9

## 背景

需要自定义协议 XCP：主从间的发现、认证加密、输入事件与剪贴板传输。调研（2026-11，来源见文末）关键事实：

- Synergy 系（Deskflow 继承）协议：TCP 24800 + 4 字符消息 ID + 大端打包；明文起步、TLS 后补；Barrier 曾因 TLS 实现缺陷连曝 5 个 CVE（2021-42072~76）；Deskflow 2026 年又曝剪贴板分块重组不查累计上限的 DoS（CVE-2026-65976）。**C++ 系 20 年的坑清单是最好的反面教材**。
- **rustls 至 0.23.46 不支持外部 PSK**（issue #174 自 2018 open，两批实现 PR 关闭未合并；0.23 的 "PSK" 仅会话恢复）；quinn 只支持 rustls 后端 → QUIC 也无 PSK；quinn-noise 停更于 2022。
- lan-mouse：DTLS（webrtc-rs）+ 自签证书 + 人工指纹授权 + UDP 4242；无自动发现（源码核实，网传 mDNS 不实）。
- LAN 上 QUIC 的收益（多流抗丢包、0-RTT）在亚毫秒 RTT 有线网不成立；社区共识 TCP + NODELAY 足够。
- WPS 事件（CERT VU#723755）：广播中发现信号的可离线校验 = 低熵配对码灾难。
- mdns-sd 0.21.7 活跃（纯 Rust）；mDNS 在 AP 隔离/组播受限网络失效，需扫描兜底。

## 备选方案

| 备选 | 放弃原因 |
| --- | --- |
| TCP + rustls TLS-PSK | rustls 无外部 PSK（事实性不可行） |
| TCP + rustls 自签+指纹钉扎（lan-mouse TLS 变体） | 首次配对需人工比对指纹，与"安全码配对"的产品诉求冲突 |
| DTLS（webrtc-rs，lan-mouse 同款） | 引入整套 WebRTC 依赖树换一个传输层，重；UDP 自管重传/分片成本高 |
| QUIC（quinn） | 无 PSK（依赖链锁死 rustls）；LAN 收益不成立 |
| 自造 AEAD 层（XChaCha20+BLAKE3+计数器） | 原语没问题，组合层（nonce 管理/重放窗口/rekey/降级）是协议史翻车高发区；snow 的复杂度几乎相同但内建了这一切 |
| Noise IKpsk2 | 省一个往返，但要求预先分发改方静态公钥，配对体验倒退；记为未来"免输码配对"升级项 |
| PAKE（OPAQUE） | 学术最优，但对"已有共享口令的双机对等"场景过重 |
| 兼容 Synergy/Deskflow 协议 | 旧协议明文/无认证设计无法承载安全需求；Input-Leap 已归档、生态合流 Deskflow，兼容价值不抵约束 |

## 决策

（完整规范见 [02-protocol.md](../02-protocol.md)，此处仅记录选型）

1. **传输**：TCP × 2（输入/控制 + 剪贴板），`TCP_NODELAY` 强制；默认端口 4243（避开 24800/4242/1716；IANA 核查通过，2026-11-16，见 [02-protocol.md](../02-protocol.md) §0）。
2. **加密**：snow `Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s`，prologue `xcontrol/1`；PSK = argon2id(安全码, per-host salt) 经 blake3 域分离。从机发起握手。
3. **发现**：三层——mDNS（`_xcontrol._tcp.local.`）/ UDP 定向广播 beacon（明文、无安全码派生物）/ 并发 TCP 扫描（用户 CIDR，默认关）。安全码匹配只在在线握手中发生，主控按 IP 限速。
4. **序列化**：输入事件手写定长帧（未知 kind 按 len 跳过）；控制消息 postcard。
5. **剪贴板防护**：逐帧限额、声明即校验、10s 超时、信用窗口（直接吸收 Deskflow CVE-2026-65976 教训）。
6. **热路径纪律**：键盘事件严禁合帧/重排；鼠标 delta 可合并累加（f32 线上 / f64 端上累加）。

## 后果

- ✅ 无证书体系、"输码即配对"的体验；PFS（XX）；规范级加密栈；依赖树极小（snow/tokio/mdns-sd）。
- ⚠️ 低熵安全码存在离线穷举的理论面（录制握手）——已按 [07-security.md](../07-security.md) §3.2 诚实文档化并默认长口令。
- ⚠️ 自定义协议 = 自担兼容承诺（major=1 内只增不改）；为此协议库独立成 `xcontrol-proto` 并配 golden fuzz 套件。
- ⚠️ 双连接使会话管理略复杂（ChannelBind）；换来输入永不因剪贴板大传输卡顿（Synergy 系痛点根除）。
- 跟踪项：IANA 端口核查 ✅ 已完成（2026-11-16：4243/tcp、4243/udp 无 IANA 分配，结论可用，详见 [02-protocol.md](../02-protocol.md) §0）；mdns-sd 与 Windows 系统 mDNS 的 5353 共存实测（M3）。

## 来源（节选）

- https://github.com/rustls/rustls/issues/174 （PSK 状态跟踪）
- https://github.com/quinn-rs/quinn/issues/1389 （quinn 仅 rustls 后端）
- https://docs.rs/snow （psk0/XX 支持、TransportState）
- https://github.com/deskflow/deskflow/blob/master/docs/dev/protocol_reference.md （Synergy 系协议）
- https://github.com/debauchee/barrier/releases （CVE-2021-42072~76）
- https://docs.syncthing.net/specs/localdisco-v4.html （定向广播/多接口发现先例）
- https://www.kb.cert.org/vuls/id/723755 （WPS 离线 oracle 教训）
- https://github.com/keepsimple1/mdns-sd
- https://cheatsheetseries.owasp.org/ （argon2id 参数下限）
