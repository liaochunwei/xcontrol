# 02 · XCP 协议规范（XControl Protocol v1）

> 状态：草案 v0.1 · 实现 FR-1.x / FR-2.x / FR-5.x。**协议即契约：任何报文变更先改本文，版本号随之提升。** 选型依据见 [ADR-0002](adr/0002-transport-and-discovery.md)。

## 0. 设计决策摘要（Why）

| 决策 | 理由（调研结论 2026-11） |
| --- | --- |
| TCP（而非 UDP/QUIC）+ `TCP_NODELAY` | 局域网亚毫秒 RTT 下 QUIC 无收益（用户态栈每包开销反而高）；Synergy 系 20 年实践证明 TCP+NODELAY 足够（Input-Leap 源码注释明确此点） |
| **两条 TCP 连接**：`输入/控制` + `剪贴板` | 单流时大块剪贴板传输会队头阻塞输入事件（Synergy 系固有痛点）；QUIC 多流在 quinn 上不可行（见下） |
| 加密 = **Noise `XXpsk0_25519_ChaChaPoly_BLAKE2s`**（snow） | rustls 至 0.23.46 **不支持外部 PSK**（issue #174，两批 PR 均关闭未合并）；quinn 仅支持 rustls 后端 → 无 PSK；DTLS 自签+指纹（lan-mouse 模式）需人工比对，体验差；自造 AEAD 层风险在组合层（nonce/rekey/重放窗口）。Noise 有规范、snow 内建 nonce 管理与 rekey，复杂度与自写相当 |
| 发现三层：mDNS → UDP beacon → 子网扫描 | mDNS 在 AP 隔离/组播受限网络失效，需兜底；扫描型发现允许用户配置 CIDR（FR-5.1） |
| **发现流量不携带任何安全码派生物** | 教训：WPS 8 位 PIN 因广播中存在可离线验证的信号被打穿（CERT VU#723755）。无论 HMAC 前缀还是"加密通告"，低熵安全码都可对捕获流量离线穷举。安全码匹配**只在**在线 Noise 握手中完成（可限速） |
| 输入事件手写定长帧；控制/协商消息 postcard | 同类项目无人在热路径用通用序列化；postcard 1.x 有稳定 wire format，体积最小，适合演进较快的控制消息 |
| 默认端口 TCP/UDP **4243** | 避开 24800(Synergy 系)、4242(lan-mouse)、1716(KDE Connect)。IANA 核查通过（见下行），可配置 |
| **IANA 核查**：4243 可用（2026-11-16） | [IANA 注册表 CSV](https://www.iana.org/assignments/service-names-port-numbers/service-names-port-numbers.csv) 中 4243/tcp、4243/udp **均无精确分配**，仅落在遗留非排他性多用途段 `vrml-multi-use`（4200-4299，VRML Multi User Systems）内。惯例冲突面均为跨域服务器/企业软件（Docker 旧版 remote API、Qlik Sense、Osmocom VTY、VirtualGL vglclient 兼容监听），局域网桌面 KVM 场景无同域占用；企业内网 IDS 可能因 Docker 关联对 4243 产生告警噪音，端口可配置兜底。冲突调查来源：[Docker 2375/4243 惯例](https://stackoverflow.com/questions/43399998/how-does-docker-use-ports-2375-and-4243) |

## 1. 端口与连接模型

```
从机                                        主控机
 │ ①发现: mDNS / UDP beacon / TCP 扫描       │
 │ ──────────── TCP 4243 (control) ────────► │  输入+控制+心跳
 │ ──────────── TCP 4243 (clipboard) ─────► │  剪贴板大块数据（握手后按需建立）
```

- 每台从机与主控维持至多 2 条 TCP；主控支持 N 台从机并发（FR-1.2）。
- 两条连接分别独立做 Noise 握手；剪贴板连接首帧 `ChannelBind` 绑定控制连接的会话（§4.4）。
- 所有帧在 Noise 传输态密文内传输（§3）；明文链路上只有 Noise 握手报文与发现 beacon。

## 2. 发现（Discovery）

### 2.1 主控通告（三层并存，全部可配置开关）

| 层 | 机制 | 内容 |
| --- | --- | --- |
| mDNS | `mdns-sd` crate 注册 `_xcontrol._tcp.local.`（服务名 8 字符，RFC 6763 合规） | TXT：`proto=1.0`、`id=<instance-uuid>`、`name=<显示名>`、`caps=<hex 位掩码>`、`port=4243` |
| UDP beacon | 每网络接口（跳过虚拟网卡，可配 allowlist）向**该接口子网的定向广播地址**发送（须 `SO_BROADCAST` 且 bind 到接口 IP）；周期 5s（未配对时）→ 60s（已满配对）；负载：`magic "XCD1" | proto u16 | port u16 | name_len u8 | name | id 16B` | **明文、无安全码派生物**（§0） |
| 应答探测 | 从机可发单播 `XCP1?`（同端口 UDP），主控单播回应 beacon —— 供"扫描指定 CIDR"路径（§2.2 第 3 层配合） | 同 beacon |

### 2.2 从机发现流程（FR-5.1/5.2）

```
Scanning:
 1. mDNS browse(_xcontrol._tcp) ──── 命中 → 候选列表
 2. 并发 UDP 探测：对每个已配 CIDR（默认=各接口所在 /24..等真实前缀）逐 IP 发 XCP1?，300ms 收集回应
    （并发上限 128，超时 1s）
 3. 兜底 TCP connect 扫描（可选，默认关）：对 CIDR 并发 connect 4243（Semaphore 128 / timeout 1s），
    连上后发送 SaltQuery 协议帧验证是否 XControl（防误扫）
    （M3 实现注记 11-17-m3-discovery：SaltQuery 消息尚未进入 §4 消息层——协议
    红线"先文档后代码"禁止先行加消息，当前实现为 connect 级可达性探测（产
    id 未知候选、走手动派生路径、默认关），SaltQuery 落地后升级补全）
候选去重（按 instance id）→ 逐个尝试 Noise 握手（安全码不匹配=握手失败，自动排除）
→ 单个成功：连接；多个成功：列候选由用户选（FR-5.5）
```

- **重连**：指数退避 1s→30s（FR-5.3）；发现层与连接层独立，重连期间每轮都先刷新候选。
- 从机侧同样以节流方式探测（每候选每轮至多 1 次握手尝试），避免对主控造成握手风暴。

### 2.3 防火墙注意（实现与文档要求）

- Windows：安装器必须预加防火墙入站规则（TCP+UDP 4243，Private profile；注意网卡须为"专用网络"）。预配置脚本与安装指引见 [10-install.md](10-install.md) §2（`scripts/windows/firewall.ps1`）。
- macOS：正式/Ad-hoc 签名，否则应用防火墙反复拦截；"阻止所有传入连接"开启时 mDNS/UDP 均不可用——UI 诊断提示。
- UDP 广播必须 bind 到具体接口 IP（否则可能静默发不出去）。

## 3. 加密与密钥派生

### 3.1 PSK 派生

```
salt = BLAKE3-derive-key("xcontrol/salt-v1", instance_uuid)[0..16]   // instance_uuid 公开，per-host 唯一，防跨用户彩虹表
p1   = Argon2id(code, salt, m=19MiB, t=2, p=1)                        // OWASP 最低档；口令类输入必须内存困难 KDF
psk  = BLAKE3-derive-key("xcontrol/psk-v1", p1)                       // 域分离，输出 32B
```

- argon2 结果**缓存**（派生一次，启动时后台执行，不阻塞发现）。
- 安全码策略（长度、易用模式风险）见 [07-security.md](07-security.md)。

### 3.2 Noise 握手

- 模式：`Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s`（snow `Builder::psk(0, psk)`）。
- `prologue = b"xcontrol/1"`（协议名+大版本哈进握手，防降级与跨协议反射）。
- 角色：**从机为 initiator，主控为 responder**。
- 握手超时 5s；失败即断开 TCP。
- 传输态：使用 snow `TransportState`（内建单调 nonce 与 rekey）；应用层不再自管 nonce。
- 在线防爆破：主控对**每来源 IP** 的失败握手计数——5 次失败 → 指数退避（10s 起，封顶 10min）+ 日志告警；从机侧自我限速（每候选 ≥2s/次）。**协议不提供任何"部分匹配"反馈**（WPS 之鉴）。

### 3.3 会话标识

握手完成后，双方由握手摘要派生 `session_id = BLAKE3(hs_hash)[0..16]`，用于日志、剪贴板通道绑定（§4.4）与生命周期管理。

## 4. 消息层

### 4.1 帧封装（Noise 密文内）

**输入/控制连接**：

```
帧头:  seq: u32        # 递增序号（诊断/延迟统计用；接收方按 TCP 顺序处理，不做重排）
       kind: u8
       len: u16        # payload 字节数
       payload: [u8; len]
```

- kind ∈ 输入事件表（§4.2，定长手写）或控制消息（§4.3，postcard）。
- **未知 kind：按 len 跳过**（前向兼容，lan-mouse 同款策略）；仅当双方协商的 minor 版本明确引入新 kind 时发送方才可发（§5）。

**剪贴板连接**：分块流式（§6 / [04-clipboard.md](04-clipboard.md)），帧头同上。

### 4.2 输入事件（kind 表，定长，全部小端）

| kind | 消息 | payload 布局 |
| --- | --- | --- |
| 0x01 | `Key` | `code: u16 (XKey)` `state: u8 (0=down,1=up)` `pad: u8` |
| 0x02 | `KeyRepeat` | `code: u16` `pad: u16` |
| 0x03 | `MouseMoveRel` | `dx: f32` `dy: f32`（注入端以 f64 累加器处理小数余量，Windows SendInput 整数化必需） |
| 0x04 | `MouseMoveAbs` | `screen: u16` `x: f32` `y: f32`（归一化 0..1；反向 = 受控光标回报，越界时取端点之外的值，见下方修订） |
| 0x05 | `MouseButton` | `button: u8 (0=L,1=M,2=R,3=Back,4=Fwd)` `state: u8` |
| 0x06 | `Wheel` | `axis: u8 (0=V,1=H)` `delta: f32`（单位=格，保留小数） |
| 0x07 | `Text` | `len: u16` `utf8[..len]`（≤512B，超长主控分段多条） |
| 0x10 | `EnterScreen` | `target: u16` `edge: u8 (0=L,1=R,2=T,3=B)` `ratio: f32`（命中点在边的位置 0..1）`mods: u32`（修饰键快照位掩码）`preserve_cursor: u8`（0=按 edge/ratio 定位；1=被控端不定位、光标保持原位——2026-11-23 追加） |
| 0x11 | `LeaveScreen` | `mods: u32`（离开时的修饰键状态，供对端对齐/释放） |
| 0x12 | `ModifierSync` | `mods: u32`（周期 1s 与切入时全量对齐；防卡键） |
| 0x20 | `SwitchRequest` | `target: u16`（热键/托盘切换，FR-2.5；主控→目标从机） |
| 0x21 | `SwitchAck` | `target: u16` `ok: u8`（从机确认可受控；失败含原因） |

`mods` 位掩码：bit0-7 = LCtrl/LShift/LAlt/LMeta/RCtrl/RShift/RAlt/RMeta（与 [03-input.md](03-input.md) XKey 修饰键对应）。

**ScreenId 跨机语义**（2026-11-19 修订，任务 11-19-m5-layout）：输入帧中引用屏幕的字段——`MouseMoveAbs.screen`（kind 0x04）、`EnterScreen.target`（kind 0x10）、`SwitchRequest.target`（kind 0x20）与 `SwitchAck.target`（kind 0x21）——一律使用 **`CtrlMsg::Layout` 定义的 ScreenId 全局空间（主控分配）**，不再是各端本机显示列表下标：

- **分配权在主控**：主控本机屏 id 从 0 起按本机显示枚举序连续分配；外接（从机）屏条目 id 紧随其后，按主控布局配置顺序分配（无 `direction` 的独立锚点条目为预留形态：不分配 id、不进路由图）。`CtrlMsg::Layout.screens` 的条目 `id` 即全局 id，全拓扑唯一。
- **从机映射表**：从机收到 `Layout` 后，把 `owner == 本机 instance_id` 的条目按出现顺序映射到本机显示枚举序（第 k 个条目 → 本机第 k 块屏；越出本机屏数的条目不映射，该 id 的帧按「未映射」处理）。重收 `Layout`（布局更新）时映射表**全量替换**——已摘除条目的旧映射随之失效。`EnterScreen` / `MouseMoveAbs` 到达时先查映射表换算到本机屏，再交注入层。
- **未映射回退**：target/screen 不在映射表内时按原值直通（兼容无 Layout 的旧会话与主控本机屏 id）。
- **1.0 对端**：协商 minor < 1 的会话不发送 `Layout`（见 §4.3 修订记录），两端保持 1.0 语义（screen/target = 接收端本机下标，0 = 主屏）。

浮点字段（`dx`/`dy`/`x`/`y`/`ratio`/`delta`）必须为**有限值**：NaN/±inf 视为协议违规，接收端拒绝（§7 code 0x04）——NaN 会沿注入端 f64 累加器扩散。（2026-11-16 补录，任务 11-16-m1-proto，随 v1 实装钉死。）

**EnterScreen 光标保持语义（2026-11-23 修订，两机联调体验轮）**：`preserve_cursor` 字节（0x10 载荷第 12 字节，1.1 修订系列）——`1` = 被控端**不做落点定位**，光标留在切出时的位置（热键/托盘切换路径：文件拖拽中途切换、用户上次停驻位置保持等语义）；`0` = 按 `edge`/`ratio` 定位到进入点（边缘切换路径，空间连续穿行必需）。兼容性：1.0 无此字节（载荷 11B），双端需同版本部署；1.1 起载荷 12B。

**受控光标回报（kind 0x04 反向语义，2026-11-22 修订，两机联调反馈轮 2——双向边缘回切）**：`MouseMoveAbs` 帧方向**双向可用**。除主控→从机的注入语义（FR-2.7）外，**被控方**（收到 `EnterScreen` 进入受控态后）把受控屏光标位置以同帧回报主控：`screen` = 受控屏全局 id（与 `EnterScreen.target` 同源），`x`/`y` = 光标在受控屏的归一化坐标，且经**边缘容差带收敛**（光标驻留屏缘 ±1 逻辑点内的样本收敛到端点值——回报契约：边缘驻留恒报 0.0/1.0，供主控极值判定）。回报节拍量级 ~10ms（2026-11-24 起；原 50ms），仅受控期间发送。

- **主控侧处理**：本机为控制方（收到过本端 `EnterScreen` 出站、`ControlDir=LocalControlsPeer`）时，收到的 `MouseMoveAbs` **消化为受控光标位置观测**（双向边缘回切判定输入，docs/03 §5「双向边缘回切」），**不注入本地**——注入会破坏主控受控期的光标锚定。其余状态按既有注入语义处理。
- **兼容性**：帧格式不变、无新 kind（非兼容性破坏）。1.0 实装无任何 `MouseMoveAbs` 发送路径（核查记录：三平台捕获源均产 `MouseMoveRel`），故不存在会把回报误注入的旧接收端；回报仅随 minor ≥ 1 双端实装出现。

**反向回报取值范围扩展到端点之外（2026-11-24 修订，回切行程补偿，任务 11-24-edge-switch-latency-hotkey-return）**：光标被推出受控屏边界期间，`x`/`y` 取端点之外的值，幅值即被屏幕边界截掉的累计行程——此前恒钳在端点值 0.0/1.0，顶边期间的推进量没有任何回报渠道，回切后这段行程丢失（快速甩动时几十到上百点）：

- `x < 0`：光标被推出受控屏**左边界**，`|x|` 是自到达该边以来的累计行程；`x > 1`：推出**右边界**，行程 = `x − 1`。`y` 同理对应**上**边界（`y < 0`）与**下**边界（`y > 1`）。
- 行程单位是控制方转发的 `MouseMoveRel` delta 单位（即主控的点）；光标严格回到屏内即清零。
- 无越界时取值仍在 `[0, 1]`，与 2026-11-22 契约一致（边缘驻留、且被控端不度量行程时恒报 0.0/1.0）。
- **兼容性**：帧布局与版本不变（解码只校验有限性，越界值原样通过；golden fixtures 不变）。旧主控按 `x ≤ 0` / `x ≥ 1` 判极值，越界值照常触发回切、幅值被忽略；旧工作机只发 `[0, 1]`，新主控算出的行程为 0。
- 平台矩阵：uinput 被控端度量行程（[05-platform.md](05-platform.md) §4.5）；macOS / X11 被控端不度量，回报仍为端点值，回切落点没有这部分补偿。

**合帧规则**：`MouseMoveRel` 可在发送窗口内合并（delta 累加，最多 8 条/帧周期）；键盘与按钮事件**严禁合并或重排**。发送侧 `TCP_NODELAY` 强制开启。

### 4.3 控制消息（postcard 编码的枚举）

```rust
enum CtrlMsg {
    Hello        { proto: ProtoVer, name: String, screen_name: String,
                   instance_id: [u8;16], caps: Caps, salt: [u8;16] },   // 主控→从机（第一条）
    HelloBack    { proto: ProtoVer, name: String, instance_id: [u8;16], caps: Caps },
    Layout       { screens: Vec<ScreenInfo> },        // 主控下发屏幕布局（名称/几何/缩放提示）
    SetOptions   { options: Options },                 // 心跳周期等
    Error        { code: ErrCode, msg: String },       // 随后断开
    Ping         { ts_ms: u64 },                       // 3s 周期；9s 无任何消息判死（Synergy 语义）
    Pong         { ts_ms: u64 },
    Pause        {}, Resume {},                        // 共享暂停/恢复（FR-2.8）
}
// ProtoVer = { major: u8, minor: u8 }；Caps = 按位置掩码（Unicode 直发/图片/HTML/绝对映射/…，见 05-platform §0）
```

**控制消息的帧 kind 分配**：每条控制消息独立占用一个 kind——新控制消息随 minor 提升新增 kind，老对端才能靠"未知 kind 按 len 跳过"（§4.1）无损降级。postcard 枚举判别值作为第二层类型标签：kind 与判别值不一致 = 协议违规（§7 code 0x04）。

| kind | 消息 |
| --- | --- |
| 0x40 | `Hello` |
| 0x41 | `HelloBack` |
| 0x42 | `Layout` |
| 0x43 | `SetOptions` |
| 0x44 | `Error` |
| 0x45 | `Ping` |
| 0x46 | `Pong` |
| 0x47 | `Pause` |
| 0x48 | `Resume` |

> 修订记录（2026-11-16，任务 11-16-m1-proto）：本节 kind 分配表与下方"关联类型定义"为 v1 首次实装补录（v0.1 草案仅有消息枚举、未分配 kind）。属 §5.4 兼容承诺内的"只增"，且 v0.1 草案期无已部署对端，无兼容性破坏。Ping/Pong 按 `CtrlMsg` 枚举声明顺序分配（Ping=0x45、Pong=0x46），kind 段内单调递增，避免两栏排版引起错行。

**关联类型定义**（v1 实装钉死 wire 类型；postcard 按 Rust 类型编码）：

```rust
struct ProtoVer  { major: u8, minor: u8 }
struct ScreenInfo { id: u16, owner: [u8; 16], name: String,
                    left: Option<u16>, right: Option<u16>,       // 布局图邻居边（docs/03 §5）；None = 无邻居
                    top: Option<u16>,  bottom: Option<u16>,
                    scale: f32 }                                  // 缩放提示（逻辑/物理像素比，1.0 = 无缩放）；几何精确对齐为 P2
// owner = 该屏所属机器的 instance id（公开值，Hello.instance_id 同源）：
// 主控本机屏 = 主控 instance id；从机屏 = 该从机 instance id。从机构建
// ScreenId → 本机屏映射表的归属判定字段（§4.2 ScreenId 跨机语义）。
struct Options   { ping_interval_ms: u32,    // 默认 3000（§4.5）
                   dead_after_ms: u32,       // 默认 9000（§4.5）
                   clipboard_max_bytes: u32 } // 默认 16 MiB（FR-4.5；§6 上限协商值）
struct Caps(u32)   // 位定义：05-platform §0；接收方必须容忍未知 bit（§5.3 无损降级）
struct ErrCode(u8) // §7；未知 code 原样透传，不得拒收（major=1 内错误码只增）
```

> 修订记录（2026-11-19，任务 11-19-m5-layout）：`ScreenInfo` 新增末位字段
> `owner: [u8; 16]`（屏幕归属机器），协议 minor 1.0 → 1.1。动机：ScreenId
> 跨机语义定稿（§4.2「ScreenId 跨机语义」段）——从机需要机器级归属字段才能把
> 主控全局 id 映射回本机屏；`name` 匹配在同名屏/改名场景不成立，不能用。
> 兼容性：postcard 结构体加字段改变 wire 布局，按 §5.3 版本门控——协商
> minor < 1 的会话**不得发送 `Layout`**（1.0 对端按旧结构解码会以尾部字节
> 违规断开）；帧 kind 集不变、无新 kind。golden fixture `ctrl_layout.bin`
> 同步刷新。
> 接收侧对称说明：postcard 非自描述，**加字段无旧形态宽容解码**——1.1 接收端
> 在 minor=0 会话收到旧形态（无 `owner`）`Layout` 时按新结构解码必然失败
> （错位读 `owner` 后尾部字节违规），按协议违规断开（fail-closed，不产生静默
> 误映射）。该方向实际不可达：1.0 实装（M1–M4 代码核查）无 `Layout` 发送
> 路径；第三方 1.0 发送端属违规面，1.1 对端以断开回应。

### 4.4 剪贴板通道

- 从机（或主控）需传剪贴板时，**另建 TCP → 完整 Noise 握手 → 首帧 `ChannelBind { session_id }`**；对端校验 session 存在且绑定唯一，否则断开。
- 剪贴板消息同为 postcard 编码，帧 kind 分配：

  | kind | 消息 |
  | --- | --- |
  | 0x50 | `ChannelBind` |
  | 0x51 | `ClipAnnounce` |
  | 0x52 | `ClipChunk` |
  | 0x53 | `ClipAck` |
  | 0x54 | `ClipEnd` |

  > 修订记录（2026-11-16，任务 11-16-m1-proto）：剪贴板 kind 分配为 v1 首次实装补录，属"只增"，无兼容性破坏。

- 传输模型：`ClipAnnounce → ClipChunk* → ClipEnd`，窗口信用流控，详见 [04-clipboard.md](04-clipboard.md) §3。

  > 修订记录（2026-11-21，两机实机联调回归）：**回归本节原设计（双连接）**，
  > **取代** 2026-11-20 的同连接裁定——同连接实测暴露队头阻塞：大图分块流与
  > 键鼠/心跳帧互为队头阻塞，任何抖动断连 = 控制断连 = 控制被切回主控（用户
  > 不可接受）。落地语义：从机在控制会话建立后**另拨一条 TCP** 到主控同端口
  > → 独立 Noise 握手（独立 nonce 流）→ 首帧 `ChannelBind{session_id}` 绑定
  > 控制会话（对端校验会话一致，不一致断开；绑定确认单工静默，无响应帧）→
  > kind 0x50–0x54 全部走该连接。剪贴板通道任何失败（解密/分帧/超时/违规）
  > **只影响剪贴板**——通道独立重连（3s 退避），控制会话与输入转发完全不受
  > 影响；协议违规处置恢复「违规断开剪贴板连接」（§6.6 原语义）。控制连接上
  > 收到 clip 帧为防御面：剥出丢弃告警，不断连（app 层 `split_clip_frames`）。
  > 接收端状态机（core `ClipRx`）与 `on_bind` 入口原样复用；帧格式无任何变更
  > （本节 wire 类型不变，非兼容性破坏）。
  >
  > 修订记录（2026-11-20，两机实机联调收尾扩展）：**1.0 实装的剪贴板传输走与
  > 输入/控制相同的控制连接**（app 层裁定，`xcontrol-app` clip_rt 模块文档）——
  > kind 0x50–0x54 与输入/控制 kind 同帧流、按 kind 分派互不干扰，协议上合法
  > （帧格式不变、心跳活性同路共享、Noise 会话即信任根）；`ChannelBind` 不进入
  > 该路径（同连接内对端已持有 session_id）。**该裁定已被 2026-11-21 修订取代**
  > （见上，回归双连接原设计）。

### 4.5 心跳与超时

| 项 | 值 |
| --- | --- |
| Ping 周期 | 3s（空闲时；输入流量存在时可视作活性，可降频至 15s） |
| 判死 | 9s 无任何帧 → 断开、从机回本地控制（NFR-5） |
| 剪贴板通道空闲 30s 自动关闭，按需重建 |

## 5. 版本协商与兼容策略

1. Noise 握手成功后，主控先发 `Hello{proto=major.minor}`。
2. 从机回 `HelloBack{proto=自身}`。双方取 `min(minor)` 为会话 minor；`major` 不等 → 立即 `Error{UnsupportedVersion}` 断开。
3. 会话 minor 之下的消息集才允许发送；新 kind/新 Caps 位只能随 minor 提升，老对端靠"未知 kind 跳过 + caps 掩码"无损降级。
4. **兼容承诺**：major=1 内保持帧格式向后兼容（只增不改不删）；破坏性变更必须 major+1 并双栈并存至少一个 major 周期。

## 6. 剪贴板分帧与防护（吸取 Deskflow CVE-2026-65976 教训）

1. `ClipAnnounce{clip_id, origin_id, origin_seq, formats[], total_len, blake3}`：**total_len 在此校验**——超过协商上限（默认 16MiB，NFR/FR-4.5 可配）立即断开。`origin_id`/`origin_seq` 为来源去重字段（[04-clipboard.md](04-clipboard.md) §5 防回环第二道防线），`origin_id` 即来源 instance_id（16B）。（修订 2026-11-16，任务 11-16-m1-proto：补录 origin 字段以对齐 docs/04 §5 防线 2——来源去重需要线上字段，v0.1 草案期无已部署对端，非兼容性破坏。）
2. `ClipChunk{clip_id, seq, data≤64KiB}`：接收端**每帧**检查 `累计长度 ≤ total_len`，违规断开；16bit seq 窗口去重（乱序=违规断开）。
   - **发送端有效分块 ≤ 65472B（64KiB−64B）**：64KiB 是接收端解码校验的语义上限，不可直接作为发送分块值——满 64KiB 数据 + postcard 头（clip_id/seq/长度 varint 最坏 11B）必然超过帧层 `len: u16` 物理上限 65535（§4.1），且整帧（7B 帧头 + payload）还须 ≤ Noise 传输态单报文明文 65519B（snow MAXMSGLEN 65535 − 16B tag，§3.2）。发送侧自律按 65472B 分块（64B 头部裕量同时满足两种物理上限），接收端校验上限保持 64KiB 不变。（修订 2026-11-17，任务 11-16-m1-mock-chain：v1 剪贴板全链路实装时发现 64KiB 满块与帧封装物理上限不相容，补录发送端有效分块上限；仅收紧发送侧取值，接收端语义不变，非兼容性破坏。）
3. 信用窗口：初始 4 块，`ClipAck{seq}` 归还信用；发送端窗口空则等待（避免撑爆对端）。
4. 超时：单 clip 10s 未完成 → 丢弃重组状态并告警（防慢速占用）。
5. `ClipEnd{clip_id}` → blake3 校验，失败丢弃并告警（不重传，剪贴板语义允许失败）。
6. 重组缓冲**每连接每 clip 独立**；任何越界（seq 回绕异常、未 announce 先 chunk）→ 断开。
7. 原则：**信任边界在接收端**，发送端自律不作数。

**v1 wire 类型钉死**（postcard 按 Rust 类型编码；修订 2026-11-16，任务 11-16-m1-proto——v1 实装时把线格式从散文钉死为 Rust 类型，后续 postcard 升级不得改变本布局）：

```rust
struct ChannelBind  { session_id: [u8; 16] }              // §3.3 session_id 16B
struct ClipAnnounce { clip_id: u32, origin_id: [u8; 16], origin_seq: u32,
                      formats: Vec<ClipFormat>, total_len: u32, blake3: [u8; 32] }
struct ClipChunk    { clip_id: u32, seq: u16, data: Vec<u8> }   // data.len() ≤ 64KiB
struct ClipAck      { seq: u16 }                                 // 信用归还（单活动 clip，无需 clip_id）
struct ClipEnd      { clip_id: u32 }
enum ClipFormat     { Text = 0, Png = 1, Html = 2, UriList = 3 } // docs/04 §2 线格式；新格式随 minor + Caps 位新增
```

**多格式内容段布局**（修订 2026-11-19，任务 11-19-m5-clipboard-full——`formats.len() ≥ 2` 时 announce→chunk→end 重组出的 content 字节与 `formats[]` 的对应关系，接收端按 formats 长度确定性选择、无嗅探）：

```rust
// formats.len() == 1 → content 即该格式原始字节（无包裹，单格式零开销）
// formats.len() >= 2 → content = postcard 编码的 Vec<Vec<u8>>（v1 布局钉死同上）
//   segments[i] 对应 formats[i] 载荷；段数 == formats.len()（不符 = 违规断开）
// 实现：xcontrol_proto::clip::{encode_content_segments, decode_content_segments}
```

## 7. 错误码（部分）

| code | 含义 |
| --- | --- |
| 0x01 | 版本不兼容 |
| 0x02 | 安全码错误（Noise 握手失败不携带此码；仅控制层语义错误使用） |
| 0x03 | 能力不满足（如要求绝对映射而对端不支持） |
| 0x04 | 协议违规（帧越界、未知通道等） |
| 0x05 | 服务端满载（超过 FR-1.2 上限） |

## 8. 测试要求（引用 [08-engineering.md](08-engineering.md) §4）

- 全消息 golden fixtures（`tests/fixtures/xcp/*.bin`），CI 比对。
- 版本协商矩阵测试（1.0↔1.1↔2.0 组合）。
- 恶意流 fuzz（cargo-fuzz）：超长 len、伪造 clip_id、seq 回绕、未握手发帧。
- 回环延迟基准：mock 传输 P50 < 5ms（NFR-1 分量）。
