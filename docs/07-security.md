# 07 · 安全模型

> 状态：草案 v0.1 · 与 [02-protocol.md](02-protocol.md) §3 配套；发布前按 §8 清单自查。

## 1. 威胁模型

| 攻击者 | 能力 | 目标 | 对策 |
| --- | --- | --- | --- |
| LAN 被动嗅探 | 抓包（WiFi 同网段/集线器场景） | 窃取按键内容（口令！）、剪贴板 | 全流量 Noise 加密（含剪贴板） |
| LAN 主动中间人 | 重定向/伪造 TCP、伪造发现应答 | 劫持输入通道、注入按键 | Noise `XXpsk0`：无正确安全码无法完成握手；prologue 防跨协议/降级 |
| 伪造主控 | 扫描网段诱连从机 | 接收从机键盘流（窃取口令） | 同上：从机只与安全码匹配者完成握手；多主控候选列表需用户确认（FR-5.5） |
| 伪造从机 | 连接主控 | 控制主控转发、塞恶意剪贴板/超长帧 | 握手限速；帧限额/超时/断开（[02](02-protocol.md) §6）；主控白名单（FR-5.4，P1） |
| 离线爆破者 | 录制握手机会数据，穷举安全码 | 恢复安全码 | §3 安全码策略 + §2 无 oracle 原则的**诚实边界**（见 3.2） |
| 本机恶意软件 | 同用户权限读配置/日志 | 窃取安全码 | 钥匙串存储 + 日志脱敏；明确不防御 root 级恶意软件（超出范围） |

**明确不防御**：root/管理员级本地恶意软件、侧信道时序（lan-mouse 亦明确不缓解）、物理接触设备。

## 2. 无离线 Oracle 原则

1. **发现流量（mDNS TXT / UDP beacon / 扫描探测）不携带任何安全码派生物**（无 HMAC 前缀、无"加密通告"）——低熵码下任何可离线校验的派生物都是字典攻击入口（WPS / CERT VU#723755 教训）。
2. 安全码正确性**只能**通过在线 Noise 握手验证，而主控对每 IP 失败握手限速退避（[02](02-protocol.md) §3.2）。
3. 协议任何报文不得包含"部分匹配"信息。

## 3. 安全码策略

### 3.1 两种模式

| 模式 | 形态 | 适用 | 派生 |
| --- | --- | --- | --- |
| 默认：**长口令** | ≥16 字符随机（首启生成 base32，UI 支持二维码/文件导入导出到从机） | 长期部署 | argon2id(m=19MiB,t=2,p=1) + blake3 域分离（[02](02-protocol.md) §3.1） |
| 易用：6 位数字码 | 一次性配对场景 | 用户显式开启 | 同上；UI 明示风险提示（§3.2） |

### 3.2 诚实的强度边界（写入 UI 文案）

- **任何 PSK 协议（含 TLS-PSK/Noise-PSK）对低熵口令都无法抵御"录制握手 + 离线穷举"**：攻击者录制一次完整握手，可对候选码离线重放密钥链验证。6 位数字 ≈ 20 bit 熵，argon2id 只提高每次尝试成本（~40ms → 单线程约 11 小时穷尽），不改变可行性结论。
- 因此：6 位模式的安全承诺 = "抵御 opportunistic 同网段闲人 + 在线爆破（限速）"，**不承诺抵御录制流量后定向离线攻击**。UI 开启该模式时展示此提示；默认长口令模式无此弱化。
- 不引入 PAKE（OPAQUE）：双机对等 + 已有 PSK 通道的场景下复杂度收益不成比例（ADR-0002 备选记录）。

### 3.3 存储与展示

- 安全码存 OS 钥匙串（mac Keychain / win Credential Manager / linux Secret Service）；不可用（headless 无 DBus 等）回退 `0600` 配置文件 + 启动日志警告。
  **实装（11-19-m6-security）**：`keyring` crate 统一三平台（xcontrol-config `keychain` feature，默认关——proto/core 红线树零增量；选型/查树记录见根 workspace.dependencies `keyring` 条目）。linux 走 Secret Service（secret-service + zbus 纯 Rust，async-io 执行器，免 libdbus C 依赖）；headless 无 DBus 会话时后端探测失败 → UI/向导回退 file 0600。`security_code_ref = "keychain:xcontrol"`（M0 定义的前缀路由）即生效路径；向导安全码步在钥匙串可用时默认 keychain。
  **mac ACL 弹窗语义（11-19-m6-release 实测补录）**：条目按 (service, user) 建立，ACL 绑定
  首次写入的二进制签名/路径——**新构建/新安装的二进制首次读取既有条目时，SecKeychain
  层会弹"允许访问"授权框并同步等待用户放行**（sample 实证 `SecKeychainFindGenericPassword`
  mach_msg 阻塞）。运行时配套：无配置向导流（FR-6.3）跳过存储预读（码由向导提供），
  窗口/托盘不被弹窗阻塞；配置存在路径的首次读取保持弹窗语义（用户放行一次后系统记忆）。
  无人值守场景（CI/自动化）用 `--code` 或 `file:` 引用规避。
- UI 永远掩码显示，点击可见 5s；日志/诊断包自动脱敏（安全码、session_id、IP 可保留）。
- 修改安全码 = 全部会话断开重握手。
  **换码原子性（双故障收口裁定，11-19-m6-security）**：写入顺序 = ① 读旧码快照 → ② 新码写 store → ③ config 落盘 + 下发 → ④ config 落盘失败时**回滚 store**（旧码写回；旧码本不存在则删除新条目）——避免"store 已是新码、config 仍指旧引用"的半更新态。生效链：config 引用变化经 `RuntimeConfigUpdate` 传播，各会话循环检测 `security_code_ref` 变更立即断开全部会话（含会话内变更——本端 Quit 信令断开，对端按心跳超时收尾），supervisor 按新引用重读 SecretStore + 重派生 PSK；发现循环的 `PskCache` 属循环局部状态，重建即失效（psk.rs `psk_cache_hits_and_invalidation` 钉死换码新键新派生）。

## 4. 传输安全属性

| 属性 | 机制 |
| --- | --- |
| 机密性/完整性 | Noise XXpsk0 + ChaChaPoly（snow TransportState，内建 nonce/rekey） |
| 前向保密 | XX 模式握手含 ephemeral DH（psk0 不牺牲 PFS） |
| 双向认证 | PSK 混入首消息——"知道安全码"即双向证明 |
| 防降级/跨协议 | prologue `xcontrol/1` |
| 防重放 | TCP 有连接边界 + Noise 单调 nonce；剪贴板 chunk 16bit seq 窗口（[02](02-protocol.md) §6.2） |
| 会话绑定 | 剪贴板通道 `ChannelBind(session_id)` 防跨会话注入 |

## 5. 输入面风险（本工具特有）

- **被控 = 全键盘权限**：伪造主控的后果等价于"攻击者可打字"。故主控伪造防护（§1）与白名单（FR-5.4）是从机侧第一优先；从机 UI 常驻显示"受控中"状态（托盘图标变化），受控态切换有可选提示音（默认关）。
- **剪贴板内容敏感**：密码管理器剪贴板通常有时效清理，XControl 同步会把它带走——提供"敏感格式过滤"：剪贴板来源标记为 password-manager（mac `org.nspasteboard.ConcealedType`、win `ExcludeClipboardContentFromMonitorProcessing`）时**不同步**（P1，FR-4 补充项）。
- **热键冲突**：默认热键避免与系统常用组合（已选 Ctrl+Alt+方向/Home/P，见 [03-input.md](03-input.md) §4.6）。

## 6. DoS 防护（网络面）

- 连接数上限（默认 16）、每 IP 握手失败退避、帧长度 u16 硬上限、剪贴板逐帧限额与 10s 超时、未握手 5s 断开（[02](02-protocol.md)）。
- 监听端口可改 + 可绑定指定网卡（多宿主机只暴露内网口）。
- **主控接入白名单（FR-5.4 实装，11-19-m6-security）**：`[net].allowlist`（CIDR 数组）非空时，serve 层 accept 后、**握手与限速门前**按来源 IP 匹配（CIDR 纯函数复用 M3 discovery 解析）；白名单外来源立即断开——对端只见连接消失（与限速门同面，无任何协议反馈），不进握手、不计握手失败限速（策略拒绝 ≠ 错码爆破）；拒绝事件经观测面上抛 app 日志（reason = `not-in-allowlist`）。空白名单 = 不启用。IPv6 来源在启用白名单时一律拒绝（白名单当前仅 IPv4，与发现层 beacon 同面）。

## 7. 供应链与构建

- 依赖审查 cargo-deny（license/重复/漏洞 advisory）进 CI（[08-engineering.md](08-engineering.md) §5）。
- 发布产物签名与校验和：mac 签名+公证（起步可 ad-hoc，发布必须正式签名）；win MSI 签名；Linux 二进制 SHA256 +（可选）sigstore。
- 密钥/口令永不进 fixtures、示例、截图与日志（AGENTS.md 禁止事项）。

## 8. 发布自查清单（M6 门禁）

11-19-m6-security 逐项核验记录（2026-11-20，mac 本机 + 测试证据；完整证据见任务报告）：

- [x] 无安全码派生物出现在发现流量（抓包验证）——**测试证据**：`xcontrol-net/src/discovery.rs` `discovery_payload_carries_no_secret_derivatives`（beacon/探测应答负载逐字节断言码原文/盐/PSK 的原始字节 + hex 文本三重形态零出现；beacon/TXT 组装函数签名只收公开字段——类型层面杜绝）。
- [x] 错误安全码握手失败无部分反馈、限速生效——**测试证据**：serve.rs `wrong_code_rejected_and_folded`（唯一折叠变体 + 无交付 + 服务无污染）、`sixth_rapid_attempt_rejected_before_dialing`（第 6 次**触网前**被本地退避拒绝）、`server_rate_limit_accumulates_across_connections_per_ip`（限速键 = 来源 IP 跨连接累计）。
- [x] 抓包确认全流量密文（含剪贴板通道与 beacon 之外的 TCP）——**实测证据**：`xcontrol-net/tests/capture.rs` `wire_capture_carries_no_plaintext_or_derivatives`——进程内窃听中继抓取握手 + 应用帧全部双向字节（实测 52/100/68B 握手三报文 + 应用帧），断言码原文/盐/PSK 的原始字节与 hex 文本零出现；tcpdump 需 root（CI 不可得），窃听中继为等价替代（字节级完整），发现流量（UDP）由第 1 项单独覆盖。
- [x] 安全码钥匙串存储三平台实测；日志/诊断包脱敏抽查——**实测证据**：mac 本机 `cargo test -p xcontrol-config --features keychain -- --ignored keychain_live` PASS（真实 login keychain 写入/读取/覆盖写/删除 + 幂等 + file 后端对照一致）；win/linux 编译级（`cargo check --target` 两 target 查树无 tokio/无红线树增量）+ linux 实测移交（lsdark-eos 本次不可达，探测配方：`busctl --user status org.freedesktop.secrets`，Secret Service 兼容层 = kwalletd/gnome-keyring）。日志脱敏：`SecretCode` Debug 恒 `[REDACTED]`（secret.rs 红线测试）、错误类型不携带秘密（error-handling.md 红线 6）。
- [x] 剪贴板超限/超时/乱序 fuzz 用例全部断开而非崩溃——**实测证据**：`cargo +nightly fuzz run frame_decode / clip_announce -- -max_total_time=30`（M1 遗留 fuzz 实跑收口，2026-11-20；两 target 各 30s 零 crash/零 hang，覆盖率/执行统计见 fuzz 输出）。
- [x] 威胁模型表逐行复核（§1）与 ADR-0002/0003 无冲突——复核记录：被动嗅探（Noise 全流量密文，第 3 项证据）；主动中间人（XXpsk0 无正确码不完成握手）；伪造主控（多主控候选 FR-5.5 用户确认 + 记忆——本任务实装，错码软排除用户可见）；伪造从机（白名单 FR-5.4 本任务实装 + 握手限速）；离线爆破者（无 oracle 原则 §2 不变，限速键 = 来源 IP 回归测试在列）；本机恶意软件（钥匙串实装后 mac 凭据受 ACL 保护，file 回退 0600）。ADR-0002（传输/发现选型）与 ADR-0003（平台输入路径）无新增冲突；keyring 引入经 xtask 白名单登记（config `keychain` feature 门控）。

## 9. 文件传输面安全（FR-7.3，XCP 1.2+，规划——[ADR-0004](adr/0004-file-transfer-channel-and-materialization.md)）

文件通道（[02-protocol.md](02-protocol.md) §4.6）把"对端可触达本机文件系统"变成协议能力，
威胁面与剪贴板本质不同，红线如下：

1. **条目索引寻址（最高红线）**：拉取请求只含 `transfer_id + entry 下标 (+offset)`，
   任何帧不得携带路径；下标 → 本机路径映射完全由源端私有维护，源端只服务**本会话
   已通告/已展开**的下标，未知 `transfer_id` / 越界下标 = 协议违规断开——杜绝对端
   构造请求读取任意文件（等价于把全盘读权限交给对端）。
2. **元数据最小化**：清单只含条目名（相对名）/大小/mtime/树形计数，不含绝对路径；
   接收端用户界面呈现的来源路径仅限清单内相对结构。
3. **清单与条目上限**（接收端校验，信任边界在接收端，§6 原则沿用）：条目数/清单
   字节/总字节/名称长度上限（[02-protocol.md](02-protocol.md) §4.6 防护规则 1）；
   `FileExpand` 下标连续性校验。
4. **磁盘耗尽防护**：接收端拉流前做剩余空间预检；总字节超用户阈值（可配，默认
   512MiB）需显式确认（UI），拒绝即不发起拉取；暂存区专用子目录，物化完成/取消
   后清理，不与用户目录混淆。
5. **协议面防护沿用**：文件域帧同样受 Noise 加密、帧长上限、逐帧校验约束；
   transfer 并发上限（≤4/连接）、分块空闲超时与总时长上限防资源占用（§4.6 防护
   规则 4/6）；文件通道故障只影响文件域（§4.6 域隔离）。
6. **敏感文件不外发**：源端剪贴板文件引用若来自密码管理器等 Concealed 标记上下文
   ——文件引用格式本身无此标记（剪贴板敏感格式过滤为剪贴板域 P1 项，§5），v1 不
   做源端文件级敏感判定，依赖"接收端粘贴动作显式发起"的同意模型：**粘贴即授权**
   （元数据清单到达 ≠ 内容传输；无粘贴动作零字节出站）。
7. **发布自查**：本节随 FR-7.3 实装切片纳入 §8 门禁（新增"越界下标 fuzz 断开 +
   索引寻址红线测试"两项），实装前不勾选。
