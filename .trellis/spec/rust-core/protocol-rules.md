# XCP 协议实现规则

来源：docs/02-protocol.md（唯一事实源）、ADR-0002。

## Wire format 规则

1. 输入事件**手写定长帧**（小端），布局严格按 docs/02 §4.2 表格；改任何字段 = minor 版本提升 + fixture 重生成。
2. 控制消息用 **postcard** 序列化的 `CtrlMsg` 枚举；禁止在输入通道夹带 postcard 消息。
3. 帧 = `seq u32 | kind u8 | len u16 | payload`；**未知 kind 按 len 跳过**（不报错不断连）。
4. **键盘事件严禁合帧/重排**；`MouseMoveRel` 可在发送窗口内 delta 累加合并（≤8 条/周期）。
5. 鼠标 delta 线上 f32、注入端 f64 累加器（Windows SendInput 整数化必需）。
6. 剪贴板通道：`ChannelBind(session_id)` 绑定；**接收端逐帧限额校验**（total_len 声明即校验、累计检查、10s 超时、seq 窗口去重）——信任边界在接收端（Deskflow CVE-2026-65976 教训）。**发送端有效分块 ≤ 65472B**（64KiB − postcard 头余量，docs/02 §6.2 修订 2026-11-17）——`ClipChunk::MAX_DATA_LEN`(65536) 是解码语义上限，不是发送分块值。**剪贴板走独立 TCP 连接（2026-11-21 回归 docs/02 §0/§4.4 双连接原设计，取代 2026-11-20 同连接裁定——同连接实测大图分块流与键鼠/心跳帧队头阻塞，任何抖动断连 = 控制断连）**：从机在控制会话建立后另拨一条 TCP 到主控同端口（独立 Noise 握手）→ 首帧 `ChannelBind{session_id}`（host 校验与当前控制会话一致，不一致断开；绑定确认单工静默）→ kind 0x50–0x54 全走该通道（app 层 clip_channel.rs：host 首帧分派 + 双端泵 + 3s 独立重连）；通道任何失败只影响剪贴板，违规处置恢复「违规断开剪贴板连接」（docs/02 §6.6）；控制连接上收到 clip 帧为防御面：`split_clip_frames` 剥出丢弃告警、不断连。

## 加密与安全红线

1. 只用 snow `Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s`，prologue `xcontrol/1`；**禁止自造 AEAD 层**（ADR-0002）。
2. PSK 派生链：argon2id(m=19MiB,t=2,p=1) → blake3 域分离；派生一次缓存，不阻塞发现。
3. **发现流量（mDNS/UDP beacon/扫描）禁止携带任何安全码派生物**；安全码匹配只发生在在线握手。
4. 无任何"部分匹配"反馈；主控按 IP 失败限速。

## 版本与 fixture

1. `ProtoVer{major,minor}`；major=1 内只增不改不删；会话取 `min(minor)`，只发送会话 minor 之下的消息集。
2. 每个消息 kind 必须有 `tests/fixtures/xcp/*.bin` golden 样本（`tests/fixtures.rs` 的 `#[ignore]` 生成测试产出，默认只跑比对；M1 定稿，原"xtask 生成"措辞作废），CI 比对。
3. fuzz（cargo-fuzz）必跑路径：超长 len、伪造 clip_id 属 proto 层（targets 已建 `fuzz/frame_decode`、`fuzz/clip_announce`）；seq 回绕、未握手发帧属连接态语义，在 core/net 层任务覆盖（M1 分层边界）。**fuzz/target 是 cargo-fuzz 自建构建目录，必须显式 gitignore**（同 spike 教训：独立目录 target 不受根 `/target` 覆盖；曾误入库 275 文件）。
4. **kind 分配约定（M1 定稿）**：控制消息 kind `0x40–0x48`、剪贴板 `0x50–0x54`、文件域 `0x58–0x60`（XCP 1.2，FR-7.3 立项预留，2026-11-26——11-26-file-transfer-spec），按 `CtrlMsg`/`ClipMsg`/`FileMsg` 枚举声明顺序单调分配；postcard 判别值 = 帧 kind 的第二层类型标签，错配 = `KindMismatch` 协议违规。新消息只追加到枚举尾部并分配下一个 kind（docs/02 §4.3/§4.6 修订记录）。
5. **文件域红线（XCP 1.2，未实装——实装切片先补 fixtures/fuzz）**：文件通道 = 独立第三条 TCP + `FileBind{session_id}` 首帧绑定（同构剪贴板通道，docs/02 §4.6）；文件域 kind 仅出现在文件连接，控制/剪贴板连接收到一律剥出丢弃告警；`FileRefAnnounce` 仅元数据（清单 ≤1MiB、条目 ≤4096、总字节 ≤协商上限，独立于剪贴板 16MiB）；**拉取请求只含 transfer_id + 条目下标，任何帧不得携带路径**（任意读红线，docs/07 §9）；流超时 = 分块空闲超时 + 总时长上限（不用固定 10s），失败条目级可重试；消息集随 minor ≥ 2 门控发送。
5. **ModifierSync 单向语义（M1 定稿，P1 教训）**：周期性快照仅**控制方**发送、仅**被控方**应用对齐——Session 用 `ControlDir`（None/LocalControlsPeer/PeerControlsLocal）门控，由 `EdgeCrossing`/`EnterScreen`/`LeaveScreen` 维护。双向无条件收发会让未受控机器被幻按键注入（防卡键机制反制卡键）。`EnterScreen` 进入时的无条件快照对齐保留。收到方向不符的 sync 宽容忽略（不断连）。

## 帧传输边界（11-20 两机实测定稿）

- **1 应用帧 = 1 Noise 报文**（M2 design 契约）：`transport.send(packet)` 把整个 packet 加密为**一条** Noise 报文——多帧编码进同一 buffer 一次 send = 粘包（接收端只认第 1 帧，余下错位→"帧格式违规"断连）。发送路径**逐帧独立 send**（输入/剪贴板双路径统一，剪贴板大图分块帧必现粘包）。
- **剪贴板读路径顺序定稿**：图片优先（PNG magic 校验——macOS arboard 对纯图片给 `get_text` 返回 TIFF 乱码文本、对文本剪贴板给 `get_image` fallback 成位图；双向误判面都要 magic 挡）→ 文本次之。magic = `[0x89, b'P', b'N', b'G']`。
- **剪贴板独立连接传输**（2026-11-21 回归 docs/02 §4.4，取代同连接裁定）：Clip 帧（0x50-0x54）走独立剪贴板 TCP（`clip_channel.rs` 泵），接收端状态机即通道泵自身（无需剥离）；控制连接上的 `split_clip_frames` 保留为防御面（对端旧版本/残留 clip 帧 → 剥出丢弃告警，不断连）；逐帧独立 send 契约在通道上继续生效。

## 双连接裁定回归（11-21 两机实测定稿）

- **剪贴板通道必须独立 TCP**（回归 docs/02 §0 原设计）：同连接裁定（11-20）导致大图分块流与控制帧互为队头阻塞——任何抖动断连=控制断。host 首帧分派器：ChannelBind=剪贴板通道 / 窥探窗无首帧=控制连接。
- **传输帧带 magic 同步标记**：`[magic:u16=0xC1A1][len:u16]`——原 u32 len 高 2 字节恒 0 的浪费空间改作 magic，零开销获得错位检测+确定性重同步（FrameInvalid→逐字节扫 magic 自愈，控制不断；DecryptFailed 仍断开）。
- **EchoFilter 基线进程级共享**（OnceLock）：跨重连/跨通道存活——否则重连后对端交付被误判新内容→回声乒乓→断连风暴。
- **剪贴板通道独立重连**：3s 固定退避、新连接优先旧通道关闭、失败只影响剪贴板（控制/输入转发完全不受扰）。

## 会话级配置热替换与双向回报（11-22 两机实测定稿）

- 热键/映射**即时生效**：`HotkeyFilter::set_hotkeys` / `MappingEngine::set_rules` 整体编译成功才替换（失败保旧表），修饰键跟踪/吞尾状态与 origin 引用计数/pending 保留——防「Down 已吞 Up 新表转发」孤立注入。win/x11 `PlatformHotkeySource::update` 必须**先 Drop 旧源（全量注销+摘桥）再注册新集合**（先注册后注销 = 进程级独占冲突全灭，11-22 check P1）。
- `MouseMoveAbs`（kind 0x04）**双向语义**（docs/02 §4.2）：被控方按 ~10ms 节拍反向回报受控屏光标（容差带收敛 0.0/1.0，仅受控期间；节拍不是 wire 契约——2026-11-24 由 50ms 提速，旧工作机仍按 50ms 回报，互通不受影响）；**越界取值 = 顶边行程**（2026-11-25：`x < 0` / `x > 1`、`y` 同理，幅值是被屏幕边界截掉的累计位移，单位 = 控制方 delta；解码只校验有限性，旧主控按 `≤ 0` / `≥ 1` 照常判极值、旧工作机只发 `[0, 1]`——wire 布局、golden fixtures、版本号均不变。主控侧只允许 core `beyond_edge` 解行程，不得先把回报值 clamp 回 `[0, 1]`），主控 `LocalControlsPeer` 下消化不注入——边缘回切判定收在主控侧（触发边 = 受控屏**面向主控的边**；极值 + 同向武装 + 角落排除〔正交轴回报落在角落带 `layout::in_corner_band`（`CORNER_RATIO` = 5%，2026-11-24；原"两轴同为端点"只有约 1 点见方）= 角落区不回切〕；热键切入（`SwitchAck` 接管）与边缘切入共用同一回切判定，回切触发边均为进入边+ 鼠标按键门控〔`LocalCmd::MouseButtons` 位图直通，拖拽中不切入/不回切〕多条件，docs/03 §5）。新增事件必须过 session 转移表全组合矩阵（250 组合白名单精确等价；11-22 第三轮起 25 事件 × 2 角色 × 5 态）。
