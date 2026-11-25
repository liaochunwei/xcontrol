//! 会话状态机（docs/02 §4.5 心跳/§5 版本协商/§3.3 会话标识，docs/03 §3 修饰键不变量）。
//!
//! # 状态模型
//!
//! ```text
//! Idle ──HandshakeReady──► Handshaking ──Hello/HelloBack(协商成功)──► Running{paused}
//!                             │                                          │
//!                             ├─ Hello(major 不等) ─► Disconnected(VersionIncompatible)
//!                             └─ Error(对端) ───────► Disconnected(PeerError)
//! Running ──9s 无帧/Local(Quit)/协议违规──► Disconnected(reason)
//! ```
//!
//! - **转移表集中在 [`Session::transition_table`] 单函数**（纯、无 payload 依赖，
//!   测试对 角色×状态×暂停×事件 做全组合枚举）；`step` 只负责按表执行 payload。
//! - 心跳：进入 Running 时计时基准归零；任何收帧刷新活性（含未知 kind/被拒绝帧）；
//!   Timer 先判死（`dead_after_ms`，默认 9s）再按 `ping_interval_ms`（默认 3s）发
//!   `Ping`；周期 1s 的 `ModifierSync` 快照**单向**（[`ControlDir`]，docs/03 §3
//!   「快照由控制方发给被控方」）：仅本机控制对端（`LocalControlsPeer`）时外发，
//!   仅对端控制本机（`PeerControlsLocal`，即收到过 `EnterScreen`）时应用对齐——
//!   其余方向忽略（宽容处理，不违规断开）。时钟经 [`Clock`] 注入，
//!   测试用 [`TestClock`](crate::clock::TestClock)。
//! - 版本协商：复用 proto [`negotiate`]；major 不等 → 发
//!   `Error{UnsupportedVersion}` 后断开（docs/02 §5.2）。
//! - 会话标识：`HandshakeReady` 携带 Noise 握手摘要，[`derive_session_id`]
//!   计算 `BLAKE3(hs_hash)[0..16]`（docs/02 §3.3），断开后保留供日志/审计。
//! - 与 net 边界：net 管 Noise 传输与字节流；core 管握手成功后的会话语义
//!   （本模块只见已解密的 [`SessionEvent`]，产出 [`SessionAction`] 交执行层）。
//!
//! # 语义裁定（docs 未逐字定义处，本模块钉死并测试）
//!
//! - [`LocalCmd::EdgeCrossing`]（本侧光标穿越边缘，宿主侧检测后上报）：
//!   先 `ReleaseAll`（释放本机按键防卡键，docs/03 §5），再发
//!   `LeaveScreen{mods}` + `EnterScreen{target, edge, ratio, mods}`；同时置
//!   [`ControlDir::LocalControlsPeer`]（本机成为控制方）。
//! - 收到 `EnterScreen`：无条件快照对齐（受控开始语义）+ 置
//!   [`ControlDir::PeerControlsLocal`]；收到 `LeaveScreen`：释放全部已注入按键
//!   （`ReleaseAll`）+ 控制方向复位为 [`ControlDir::None`]。
//! - 收到 `CtrlMsg::Pause`：置暂停 + 释放已注入按键；Paused 期间收输入帧 =
//!   [`StateError::PausedReject`]（拒该帧、连接保持）；控制帧/心跳照常处理。
//! - `CtrlMsg::SetOptions` 的 0 值字段 = 「字段缺失」，保持现值（对端不可用
//!   0 关闭心跳——否则 0ms 判死会立即断连）。
//! - `SwitchRequest` 回 `ReleaseAll` + `SwitchAck{ok=true}`（M1 无能力否决逻辑，
//!   FR-2.5；能力校验随 Caps 位在后续里程碑补充。M4-hotkeys 补 ReleaseAll：
//!   热键切换路径没有 EdgeCrossing 前置的 LeaveScreen 兜底，受控切换前先清
//!   本机已注入按键防叠加卡键，PRD 要求 6）。
//! - `LocalCmd::ReleaseControl`（FR-2.5 Home，M4-hotkeys）：仅
//!   `LocalControlsPeer` 时生效——`ReleaseAll` + `LeaveScreen{mods}` + 控制方向
//!   复位；无受控为无操作。`LocalCmd::ForwardInput`（主控捕获转发）：Running
//!   未暂停才接受（Paused 下对端拒收输入帧）；control_dir 门控由调用方承担。
//! - **布局协商与 ScreenId 全局语义**（M5，docs/02 §4.2/§4.3 修订）：
//!   [`LocalCmd::PublishLayout`]（仅 Host、Running）按会话 minor 门控下发
//!   `CtrlMsg::Layout{screens}`（`ScreenInfo.owner` 属 1.1 wire 形态——协商
//!   minor < 1 不发，1.0 对端按旧结构解码会违规断开，docs/02 §5.3）；对端收
//!   `Layout` → 缓存 [`ScreenLayout`] + `Notice::LayoutReceived`。跨机 id 映射：
//!   [`Session::map_peer_screen`] 由执行层按「owner == 本机条目序 → 本机屏」
//!   注入（docs/02 §4.2 从机映射表）；`EnterScreen.target` / `MouseMoveAbs.screen`
//!   在产出注入/warp 动作前先查映射，未映射回退直通（1.0 语义兼容）。
//! - **切换闭环**（FR-2.5，docs/03 §5 热键闭环）：Host 收 `SwitchAck{ok=true}`
//!   → 立即执行 EdgeCrossing 同型接管（`ReleaseAll` + `LeaveScreen{mods}` +
//!   `EnterScreen{target, edge, ratio}`）：进入边从布局图推导
//!   （[`crate::layout::entry_edge_into`]，无邻边回退 target 左缘 = Next 语义），
//!   ratio 取 [`crate::layout::SWITCH_ENTRY_RATIO`]（0.5 居中）。被控中
//!   （`PeerControlsLocal`）不接管（切换门控的镜像防御）。**切入后的回切触发边
//!   = 进入边**（受控屏面向主控的边，11-24 更正）——与边缘路径同一定义
//!   （`ControllingContext.outer_edge`），任何新增接管路径都必须产出相同的
//!   `outer_edge`。
//! - **双向边缘回切**（11-21 两机联调反馈 5，docs/03 §5）：受控屏光标位置由
//!   被控方经 `MouseMoveAbs`（kind 0x04 反向语义，docs/02 §4.2 修订）回报；
//!   控制方（`LocalControlsPeer`）收到回报**消化为判定输入而非注入**（注入会
//!   破坏受控锚定光标）。判定全在本状态机：受控屏**面向主控的边**（= 主控
//!   切出边的对侧，11-22 语义改判）极值回报（对端容差带收敛契约：边缘驻留
//!   回报恰 0.0/1.0）**且**上次回报以来仍转发过同方向 `MouseMoveRel`（同向
//!   武装）→ 触发回切 = ReleaseControl 同型释放（`ReleaseAll` +
//!   `LeaveScreen{mods}`）+ 环绕落点 warp（主控面向受控屏的那条边内侧内缩
//!   [`EDGE_RETURN_INSET`]，ratio 保持）。**回切免疫窗按时间计**
//!   （[`RETURN_IMMUNITY_MS`]，接管后 800ms；11-24 起与回报频率解耦——原按
//!   回报条数计，节拍一变窗口跟着变）。**角落排除**（11-22 第三轮；11-24 改为
//!   角落区）：正交轴回报落在角落带（[`crate::layout::in_corner_band`]）不触发
//!   ——角落留给系统热区（KDE 触发角专用），与
//!   [`crate::layout::edge_crossing`] 的切入侧排除共用同一比例。**按键门控**
//!   （11-22 第三轮）：任意鼠标键按下（[`LocalCmd::MouseButtons`] 位图直通）
//!   不触发——拖拽中不切换；热键回切不经此判定面。本轮范围裁定：**单跳**
//!   （受控屏面向主控边 → 主控）；多屏链回邻屏移交后续里程碑（当前部署 2 屏）。
//!   **回切行程补偿**（11-24，R8）：受控屏回报的取值可到端点之外（docs/02 §4.2
//!   「反向回报取值范围扩展」）——被控光标顶边期间被屏幕边界截掉的位移；触发
//!   回切时这段行程经 [`Notice::EdgeReturnedToHost`] 的 `overflow` 字段随通知带出
//!   （[`beyond_edge`]），序列里的落点 warp 不含补偿，加到落点上由执行层完成
//!   （主控侧另补在途/丢弃的两段位移，docs/03 §5 回切第 3 条）。
//! - **Home 的落点**（[`LocalCmd::ReleaseControl`]）：core 产出主屏内侧落点
//!   warp；macOS 主控在 app 层以切入锚点替换此落点（11-24，docs/03 §4.6）。
//! - 任何进入 `Disconnected` 的转移统一产出 `ReleaseAll` + 断开通知：断开即
//!   控制关系终结，已注入按键必须释放（docs/03 §3 防卡键不变量的断开分支）。

use xcontrol_proto::input::{InputEvent, Mods, ScreenEdge, ScreenId};
use xcontrol_proto::{Caps, CtrlMsg, ErrCode, Options, ProtoVer, negotiate};

use crate::clock::Clock;
use crate::layout::ScreenLayout;
use crate::mods::ModifierTracker;

/// ModifierSync 周期：1s（docs/03 §3「心跳内周期性（1s）校验状态一致」）。
pub const MODIFIER_SYNC_INTERVAL_MS: u64 = 1000;

/// 会话角色（docs/01：Host = 主控，Client = 从机；决定 Hello 方向）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 主控：握手完成后先发 `Hello`，等待 `HelloBack`。
    Host,
    /// 从机：等待 `Hello`，回 `HelloBack`。
    Client,
}

/// 控制方向（docs/03 §3：修饰键快照**由控制方发给被控方**——单向语义，
/// 防止无人在控时周期快照在对端制造幻按键注入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControlDir {
    /// 无人在控（常态）：不外发周期快照，收到 `ModifierSync` 宽容忽略。
    #[default]
    None,
    /// 本机控制对端（本机侧 `EdgeCrossing` 切出）：周期 1s 外发本地快照。
    LocalControlsPeer,
    /// 对端控制本机（收到 `EnterScreen`）：周期快照应用对齐。
    PeerControlsLocal,
}

/// 本端身份（`Hello`/`HelloBack` 载荷来源；来自配置/平台枚举，由上层填充）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// 显示名（`Hello.name` / `HelloBack.name`）。
    pub name: String,
    /// 本机屏幕显示名（仅 `Hello.screen_name`）。
    pub screen_name: String,
    /// 实例 UUID（16B；发现层通告的同一 id）。
    pub instance_id: [u8; 16],
    /// 能力掩码。
    pub caps: Caps,
    /// PSK 派生盐（16B，docs/02 §3.1；公开值，非机密）。
    pub salt: [u8; 16],
}

/// 断开原因（`Disconnected` 子态的载荷；日志与重连策略的输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// 9s（`dead_after_ms`）无任何帧判死（docs/02 §4.5）。
    Timeout,
    /// 版本协商 major 不等（docs/02 §5.2）。
    VersionIncompatible,
    /// 帧解码/非法转移等协议违规（docs/02 §7 code 0x04）。
    ProtocolViolation,
    /// 对端发来 `Error` 帧后断开。
    PeerError {
        /// 对端给出的错误码（docs/02 §7）。
        code: ErrCode,
        /// 对端给出的人类可读说明。
        msg: String,
    },
    /// 本地主动退出。
    LocalQuit,
}

/// 会话状态（`Running` 的 `paused` 为共享暂停子态，FR-2.8）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// 初始态（Noise 握手未完成）。
    Idle,
    /// 握手摘要已就绪、等待版本协商帧。
    Handshaking,
    /// 会话运行中；`paused` = 共享暂停。
    Running {
        /// 是否共享暂停。
        paused: bool,
    },
    /// 已断开（终态；任何事件被拒绝）。
    Disconnected(DisconnectReason),
}

/// 本地命令（`SessionEvent::Local`）。
#[derive(Debug, Clone, PartialEq)]
pub enum LocalCmd {
    /// 本地共享暂停（FR-2.8；通知对端 + 释放本机已注入按键）。
    Pause,
    /// 本地共享恢复。
    Resume,
    /// 热键/托盘切换控制权（FR-2.5；发 `SwitchRequest`）。
    Switch {
        /// 目标屏幕。
        target: ScreenId,
    },
    /// 本地退出。
    Quit,
    /// 本侧光标穿越屏幕边缘（宿主侧经
    /// [`edge_crossing`](crate::layout::edge_crossing) 检测后上报）。
    EdgeCrossing {
        /// 目标屏幕。
        target: ScreenId,
        /// 命中边缘。
        edge: ScreenEdge,
        /// 命中点沿边归一化比例。
        ratio: f32,
    },
    /// 本地修饰键快照更新（捕获侧 [`ModifierTracker`] 的输出直通）。
    LocalMods(Mods),
    /// 本地鼠标按键状态更新（11-22 第三轮按键门控，docs/03 §5）：任意鼠标键
    /// 按下 = true、全部释放 = false（跟踪面 = 主控捕获流 MouseDown/Up 位图，
    /// 受控与未受控都过捕获回调——同一位图）。消费面：被控回切门控
    /// （[`Session`] `edge_return_actions` 判定前过滤——回报照常消化）；主控
    /// 切入门控在 app 层（drive_session 丢弃 EdgeCrossing 上报，不经状态机）。
    /// 全状态可接受（Idle/Handshaking/Running/Paused 跟踪照常——恢复后状态
    /// 正确）。
    MouseButtons {
        /// 任意鼠标键按下。
        any_down: bool,
    },
    /// 回主控（FR-2.5 热键「Home」；主控侧自切）：释放当前受控的从机——
    /// 仅 `ControlDir::LocalControlsPeer` 时生效（`ReleaseAll` + `LeaveScreen{mods}`
    /// + 控制方向复位）；无受控时为无操作（调用方记日志）。
    ReleaseControl,
    /// 主控捕获事件转发（FR-2.x 主控链路，M4 接线）：捕获层过滤热键后的
    /// 未命中事件经会话状态机出站（seq 计数/暂停判定统一在此收口）。
    /// 调用方门控：仅 Running 且未共享暂停（Paused 下出站输入帧会被对端拒绝）。
    ForwardInput(InputEvent),
    /// 受控屏光标位置回报（双向边缘回切，11-21 反馈 5；docs/02 §4.2 kind 0x04
    /// 反向语义）：被控方把受控屏光标归一化位置（容差带收敛端点值契约；光标
    /// 被推出屏边时为**端点之外**的值，幅值 = 顶边行程——docs/02 §4.2「反向回报
    /// 取值范围扩展」，11-24）回报控制方——判定/回切决策全在对端状态机。内部
    /// 门控：仅 `PeerControlsLocal` 且受控目标已知时出站 `MouseMoveAbs{screen:
    /// 目标}`，其余无操作；非有限坐标防御丢弃。
    ReportControlledCursor {
        /// 受控屏光标归一化 X（对端受控屏空间 0..1）。
        x: f32,
        /// 受控屏光标归一化 Y。
        y: f32,
    },
    /// 主控下发当前布局（FR-1.3 协商，M5；`Session::set_layout` 预装后由执行层
    /// 在握手完成/布局配置变更时触发）：出站 `CtrlMsg::Layout{screens}`。
    /// 内部门控：仅 Host、布局非空、协商 minor ≥ 1（`ScreenInfo.owner` 属 1.1
    /// wire 形态，docs/02 §4.3 修订记录/§5.3 版本门控）——不满足时无动作。
    PublishLayout,
}

/// 帧载荷（已按 kind 大类解码；[`crate::route::Dispatcher`] 的产出）。
#[derive(Debug, Clone, PartialEq)]
pub enum FramePayload {
    /// 输入事件（docs/02 §4.2）。
    Input(InputEvent),
    /// 控制消息（docs/02 §4.3）。
    Ctrl(CtrlMsg),
    /// 剪贴板通道 kind 出现在输入/控制连接（错误通道 → 非法转移拒绝）。
    Clip {
        /// 帧 kind。
        kind: u8,
    },
    /// 未知 kind（前向兼容：按 len 跳过，仅计活性，docs/02 §4.1）。
    Unknown {
        /// 帧 kind。
        kind: u8,
    },
}

/// 会话事件（`step` 的输入；net 层解密后构造）。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// Noise 握手完成，携带握手摘要（docs/02 §3.3 的 hs_hash）。
    HandshakeReady {
        /// 握手摘要（32B）。
        hs_hash: [u8; 32],
    },
    /// 收到一帧（已解码分类）。
    Frame(FramePayload),
    /// 定时器滴答（心跳判定内部读 [`Clock`]）。
    Timer,
    /// 本地命令。
    Local(LocalCmd),
}

/// 状态机错误（`step` 拒绝事件的全部形态）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// Paused 子态拒绝输入帧（拒该帧、连接保持；控制帧不受影响）。
    #[error("Paused 状态拒绝输入帧（kind 0x{kind:02X}）")]
    PausedReject {
        /// 被拒帧的 kind。
        kind: u8,
    },
    /// 当前状态不接受该事件（协议违规，调用方断开）。
    #[error("状态 {state} 不接受事件 {event}")]
    UnexpectedEvent {
        /// 当前状态标签。
        state: &'static str,
        /// 事件标签。
        event: &'static str,
    },
    /// 会话已断开（终态，任何事件被拒绝）。
    #[error("会话已断开")]
    AlreadyDisconnected,
}

/// 出站消息（core 产出的待编码帧载荷；net 层负责密封与加密）。
#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    /// 控制消息。
    Ctrl(CtrlMsg),
    /// 输入事件。
    Input(InputEvent),
}

impl Outbound {
    /// 帧 kind（docs/02 §4.2/§4.3 表）。
    #[must_use]
    pub const fn kind(&self) -> u8 {
        match self {
            Self::Ctrl(msg) => msg.kind(),
            Self::Input(event) => event.kind(),
        }
    }

    /// 编码完整帧（帧头 + payload）追加到 `out`。
    ///
    /// # Errors
    /// proto 编码约束违规（如 `Text` 超长）。
    pub fn encode_frame_into(
        &self,
        seq: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), xcontrol_proto::ProtoError> {
        match self {
            Self::Ctrl(msg) => xcontrol_proto::Message::encode_frame_into(msg, seq, out),
            Self::Input(event) => xcontrol_proto::Message::encode_frame_into(event, seq, out),
        }
    }
}

/// 会话通知（状态迁移的对外可见事件；UI/日志/重连策略消费）。
///
/// 只派生 `PartialEq`：[`Notice::EdgeReturnedToHost`] 的回切行程是浮点字段
/// （11-24），浮点不满足 `Eq`；全仓没有以 `Notice` 作集合键或要求 `Eq` 的用法。
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    /// 版本协商完成（含会话 id 与协商 minor）。
    HandshakeComplete {
        /// 会话标识（`BLAKE3(hs_hash)[0..16]`）。
        session_id: [u8; 16],
        /// 会话 minor（双方 min）。
        minor: u8,
    },
    /// 会话断开。
    Disconnected(DisconnectReason),
    /// 本机进入受控态（收到 `EnterScreen`）。
    EnteredControl {
        /// 本机屏幕 id（`EnterScreen.target`，对端布局中指向本机的目标屏）。
        target: ScreenId,
    },
    /// 本机退出受控态（收到 `LeaveScreen`）。
    LeftControl,
    /// 控制方经受控屏外侧边缘滑出回切主控（双向边缘回切触发，11-21 反馈 5；
    /// docs/03 §5）——对端 `LeaveScreen` 已随行发出，环绕落点 warp 已产出。
    EdgeReturnedToHost {
        /// 被释放的受控屏（全局 id）。
        target: ScreenId,
        /// 回切行程补偿的**回报段**（11-24，R8；docs/02 §4.2「反向回报取值范围
        /// 扩展到端点之外」、docs/03 §5 回切第 3 条）：触发回切的那条回报里端点
        /// 之外的行程 `(dx, dy)`——光标顶在受控屏边期间被屏幕边界截掉的位移，
        /// 单位 = 本机转发的 `MouseMoveRel` delta 单位（主控的点），符号 = 推出
        /// 方向（右/下为正，与本机屏幕坐标轴同向）。无越界、或被控端不度量行程
        /// （macOS / X11）时为 `(0.0, 0.0)`。序列里的落点 warp **不含**这段补偿
        /// ——执行层把它（连同主控侧另补的在途位移）加到落点上。
        overflow: (f32, f32),
    },
    /// 对端请求切换到本机（FR-2.5）。
    SwitchRequested {
        /// 发起方屏幕。
        target: ScreenId,
    },
    /// 切换请求的结果（对端 `SwitchAck`）。
    SwitchResult {
        /// 确认方屏幕。
        target: ScreenId,
        /// 是否可受控。
        ok: bool,
    },
    /// 收到 `CtrlMsg::Layout`（M5 布局协商）：布局已缓存为 [`ScreenLayout`]。
    /// 执行层据此构建 ScreenId → 本机屏映射表（[`Session::map_peer_screen`]，
    /// docs/02 §4.2「ScreenId 跨机语义」）。
    LayoutReceived {
        /// 布局条目数。
        screens: usize,
    },
}

/// 会话动作（`step` 的产出；执行层按序应用到 sink / 出站发送 / 通知）。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionAction {
    /// 发送出站帧（`seq` 由会话内单调计数器分配）。
    Send {
        /// 帧序号（诊断/延迟统计）。
        seq: u32,
        /// 出站消息。
        msg: Outbound,
    },
    /// 注入一条输入事件（合成修饰键事件也走此通道）。
    InjectInput(InputEvent),
    /// 指针 warp 到目标屏幕归一化坐标（`EnterScreen` 到达边位置）。
    WarpPointer {
        /// 目标屏幕。
        screen: ScreenId,
        /// 归一化 X。
        x: f32,
        /// 归一化 Y。
        y: f32,
    },
    /// 释放全部已注入按键/按钮（切出/暂停防卡键）。
    ReleaseAll,
    /// 会话通知。
    Notice(Notice),
}

/// 会话标识派生：`BLAKE3(hs_hash)[0..16]`（docs/02 §3.3）。
///
/// `hs_hash` 为 Noise 握手完成后的通道绑定摘要（net 层提供）。
#[must_use]
pub fn derive_session_id(hs_hash: &[u8; 32]) -> [u8; 16] {
    let digest = blake3::hash(hs_hash);
    let bytes = digest.as_bytes();
    let mut id = [0u8; 16];
    id.copy_from_slice(&bytes[..16]);
    id
}

/// 转移表纯函数的结果（接受时的粗粒度效果，供全组合枚举测试断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// 忽略（仅计活性/快照更新）。
    Ignore,
    /// Idle → Handshaking（Host 另发 `Hello`）。
    EnterHandshake,
    /// Handshaking → Running（Client 回 `HelloBack`）。
    EnterRunning,
    /// 对端 `Error` 帧 → 断开（PeerError）。
    PeerFatal,
    /// Running 内透传处理（输入/配置帧）。
    Passthrough,
    /// 心跳应答。
    Ping,
    /// 暂停/恢复翻转。
    SetPaused(bool),
    /// Timer 心跳判定（判死 → Ping → ModifierSync）。
    Heartbeat,
    /// 本地切换请求。
    Switch,
    /// 本地边缘穿越。
    EdgeCrossing,
    /// 本地回主控（释放受控从机，FR-2.5 Home）。
    ReleaseControl,
    /// 主控捕获事件转发。
    ForwardInput,
    /// 本地退出。
    Quit,
}

/// 转移拒绝（`transition_table` 的 Err 形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reject {
    /// 事件在该状态无意义（协议违规）。
    Unexpected,
    /// Paused 拒绝输入帧（可容忍：拒帧不断连）。
    PausedInput,
    /// 终态拒绝。
    Disconnected,
}

/// 转移表键：事件大类（payload 无关，可全组合枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    HandshakeReady,
    Frame(FrameClass),
    Timer,
    Local(LocalKind),
}

/// 转移表键：帧大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameClass {
    Input,
    Ctrl(CtrlKind),
    Clip,
    Unknown,
}

/// 转移表键：控制消息分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlKind {
    Hello,
    HelloBack,
    Layout,
    SetOptions,
    Error,
    Ping,
    Pong,
    Pause,
    Resume,
}

impl From<&CtrlMsg> for CtrlKind {
    fn from(msg: &CtrlMsg) -> Self {
        match msg {
            CtrlMsg::Hello { .. } => Self::Hello,
            CtrlMsg::HelloBack { .. } => Self::HelloBack,
            CtrlMsg::Layout { .. } => Self::Layout,
            CtrlMsg::SetOptions { .. } => Self::SetOptions,
            CtrlMsg::Error { .. } => Self::Error,
            CtrlMsg::Ping { .. } => Self::Ping,
            CtrlMsg::Pong { .. } => Self::Pong,
            CtrlMsg::Pause => Self::Pause,
            CtrlMsg::Resume => Self::Resume,
        }
    }
}

/// 转移表键：本地命令分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalKind {
    Pause,
    Resume,
    Switch,
    Quit,
    EdgeCrossing,
    LocalMods,
    MouseButtons,
    ReleaseControl,
    ForwardInput,
    ReportControlledCursor,
    PublishLayout,
}

impl From<&LocalCmd> for LocalKind {
    fn from(cmd: &LocalCmd) -> Self {
        match cmd {
            LocalCmd::Pause => Self::Pause,
            LocalCmd::Resume => Self::Resume,
            LocalCmd::Switch { .. } => Self::Switch,
            LocalCmd::Quit => Self::Quit,
            LocalCmd::EdgeCrossing { .. } => Self::EdgeCrossing,
            LocalCmd::LocalMods(_) => Self::LocalMods,
            LocalCmd::MouseButtons { .. } => Self::MouseButtons,
            LocalCmd::ReleaseControl => Self::ReleaseControl,
            LocalCmd::ForwardInput(_) => Self::ForwardInput,
            LocalCmd::ReportControlledCursor { .. } => Self::ReportControlledCursor,
            LocalCmd::PublishLayout => Self::PublishLayout,
        }
    }
}

/// 转移表键：状态大类（`Running` 的 paused 单独作维度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateTag {
    Idle,
    Handshaking,
    Running,
    Disconnected,
}

impl StateTag {
    const fn label(self, paused: bool) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Handshaking => "Handshaking",
            Self::Running => {
                if paused {
                    "Running(Paused)"
                } else {
                    "Running"
                }
            }
            Self::Disconnected => "Disconnected",
        }
    }
}

impl EventKind {
    const fn label(self) -> &'static str {
        match self {
            Self::HandshakeReady => "HandshakeReady",
            Self::Timer => "Timer",
            Self::Frame(class) => match class {
                FrameClass::Input => "Frame(Input)",
                FrameClass::Clip => "Frame(Clip)",
                FrameClass::Unknown => "Frame(Unknown)",
                FrameClass::Ctrl(kind) => match kind {
                    CtrlKind::Hello => "Frame(Ctrl(Hello))",
                    CtrlKind::HelloBack => "Frame(Ctrl(HelloBack))",
                    CtrlKind::Layout => "Frame(Ctrl(Layout))",
                    CtrlKind::SetOptions => "Frame(Ctrl(SetOptions))",
                    CtrlKind::Error => "Frame(Ctrl(Error))",
                    CtrlKind::Ping => "Frame(Ctrl(Ping))",
                    CtrlKind::Pong => "Frame(Ctrl(Pong))",
                    CtrlKind::Pause => "Frame(Ctrl(Pause))",
                    CtrlKind::Resume => "Frame(Ctrl(Resume))",
                },
            },
            Self::Local(kind) => match kind {
                LocalKind::Pause => "Local(Pause)",
                LocalKind::Resume => "Local(Resume)",
                LocalKind::Switch => "Local(Switch)",
                LocalKind::Quit => "Local(Quit)",
                LocalKind::EdgeCrossing => "Local(EdgeCrossing)",
                LocalKind::LocalMods => "Local(LocalMods)",
                LocalKind::MouseButtons => "Local(MouseButtons)",
                LocalKind::ReleaseControl => "Local(ReleaseControl)",
                LocalKind::ForwardInput => "Local(ForwardInput)",
                LocalKind::ReportControlledCursor => "Local(ReportControlledCursor)",
                LocalKind::PublishLayout => "Local(PublishLayout)",
            },
        }
    }
}

impl SessionEvent {
    /// 转移表键提取（payload 无关）。
    fn classify(&self) -> EventKind {
        match self {
            Self::HandshakeReady { .. } => EventKind::HandshakeReady,
            Self::Frame(payload) => EventKind::Frame(match payload {
                FramePayload::Input(_) => FrameClass::Input,
                FramePayload::Ctrl(msg) => FrameClass::Ctrl(CtrlKind::from(msg)),
                FramePayload::Clip { .. } => FrameClass::Clip,
                FramePayload::Unknown { .. } => FrameClass::Unknown,
            }),
            Self::Timer => EventKind::Timer,
            Self::Local(cmd) => EventKind::Local(LocalKind::from(cmd)),
        }
    }
}

/// 受控屏上下文（控制方视角；双向边缘回切判定输入，11-21 反馈 5）。
///
/// 随 `EdgeCrossing` / `SwitchAck` 接管建立（边缘/热键两条路径都汇于此），随
/// 释放（回切/Home/断开/对端 LeaveScreen）清除。`outer_edge` = **受控屏面向
/// 主控的边**：
/// - 边缘路径 = 主控滑出边的对侧（`opposite_edge(切出边)`）；
/// - 热键/托盘路径 = 进入边本身（`entry_edge_into` 返回的已是受控屏的进入边，
///   11-24 更正：原再取一次对侧，得到的是远离主控的边——热键切入后推回面向
///   主控的边无效、推远离主控的边反而回切的根因）。
///
/// 几何语义（11-22 用户实测改判）：回切走空间连续穿行——主控左缘滑出（向左
/// 移动）进入受控屏后，用户在其**右缘**（面向主控侧）向右滑出即穿回主控，与
/// "来时方向相反、同一条边界穿越"的桌面连续直觉一致；原"外侧边绕回"模型
/// （滑出远离主控的边）与直觉相悖，废弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ControllingContext {
    /// 受控屏（全局 id，`EdgeCrossing.target` / `SwitchAck.target`）。
    target: ScreenId,
    /// 受控屏面向主控的边（极值判定与环绕落点共用）。
    outer_edge: ScreenEdge,
    /// 接管时刻（会话时钟毫秒；回切免疫窗的计时起点，11-24 按时间计）。
    taken_over_ms: u64,
}

/// 回切免疫窗（按时间：接管后 800ms；11-24 起与回报频率解耦——原为 16 条
/// 回报 × 50ms 采样的等价换算，节拍一变窗口跟着变）：接管后窗内的受控屏回报
/// 不触发回切——**死区减半历程下的慢速切入回弹修复**（11-22 用户实测；第三轮
/// 1%→0.5% 后切入惯性行程更短，本窗是防"切入即回切"的主防线）：切入惯性
/// 只需 1% 行程（2-3 帧 delta）即达极值，慢速切入的持续同向动量直接触发
/// "切入即回切"。免疫窗内动量自然消散；刻意回切在稳定期后推缘即可（手感
/// 代价 = 切入后短暂稳定期，与切换防抖 150ms 的放大版同语义）。
///
/// 判定用回报**到达时刻**（[`Session::step`] 读时钟）与 `taken_over_ms` 之差
/// （饱和）：差值 < 本常量 = 窗内，≥ 本常量 = 窗外可触发。
const RETURN_IMMUNITY_MS: u64 = 800;

/// XCP 会话状态机（角色感知；零 I/O，时钟注入，全部产出 [`SessionAction`]）。
pub struct Session<C: Clock> {
    role: Role,
    identity: Identity,
    clock: C,
    options: Options,
    state: SessionState,
    session_id: Option<[u8; 16]>,
    negotiated: Option<ProtoVer>,
    peer_name: Option<String>,
    peer_instance_id: Option<[u8; 16]>,
    peer_caps: Option<Caps>,
    last_frame_ms: Option<u64>,
    last_ping_ms: Option<u64>,
    last_modsync_ms: Option<u64>,
    /// 本地物理修饰键快照（出站 LeaveScreen/EnterScreen/ModifierSync 载荷）。
    local_mods: ModifierTracker,
    /// 本地任意鼠标键按下（11-22 第三轮按键门控：拖拽中不切入/不回切，
    /// docs/03 §5）。跟踪面 = `LocalCmd::MouseButtons`（主控捕获流位图直通，
    /// 受控/未受控/暂停全态更新）。
    local_mouse_down: bool,
    /// 已注入修饰键基准（入站对齐防卡键）。
    injected_mods: ModifierTracker,
    /// 控制方向（周期 ModifierSync 单向收发的依据，docs/03 §3）。
    control_dir: ControlDir,
    /// 受控屏上下文（本机为控制方时；双向边缘回切判定，见 [`ControllingContext`]）。
    controlling: Option<ControllingContext>,
    /// 受控目标屏（本机被控时 `EnterScreen.target` 全局 id；受控光标回报的出站
    /// 屏 id 来源，docs/02 §4.2 kind 0x04 反向语义）。随控制关系建立/清除。
    controlled_target: Option<ScreenId>,
    /// 同向武装：上次回报以来仍转发过受控屏滑出方向的 `MouseMoveRel`
    /// （「仍有同方向移动事件」判定输入；随每条受控屏回报复位）。
    return_armed: bool,
    layout: ScreenLayout,
    /// ScreenId 全局 → 本机屏映射表（docs/02 §4.2「ScreenId 跨机语义」；执行层
    /// 收 Layout 后按 `owner == 本机` 条目序注入，[`Session::map_peer_screen`]）。
    /// 查不到的 id 回退直通（1.0 语义兼容）。
    screen_map: Vec<(ScreenId, ScreenId)>,
    next_seq: u32,
}

impl<C: Clock> std::fmt::Debug for Session<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 手写 Debug：clock 实现不进日志。
        f.debug_struct("Session")
            .field("role", &self.role)
            .field("state", &self.state)
            .field("session_id", &self.session_id)
            .field("negotiated", &self.negotiated)
            .field("options", &self.options)
            .field("next_seq", &self.next_seq)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> Session<C> {
    /// 构造 Idle 会话（`options` 一般取 [`Options::defaults()`]，对端
    /// `SetOptions` 到达后覆盖）。
    #[must_use]
    pub fn new(role: Role, identity: Identity, clock: C, options: Options) -> Self {
        Self {
            role,
            identity,
            clock,
            options,
            state: SessionState::Idle,
            session_id: None,
            negotiated: None,
            peer_name: None,
            peer_instance_id: None,
            peer_caps: None,
            last_frame_ms: None,
            last_ping_ms: None,
            last_modsync_ms: None,
            local_mods: ModifierTracker::new(),
            local_mouse_down: false,
            injected_mods: ModifierTracker::new(),
            control_dir: ControlDir::None,
            controlling: None,
            controlled_target: None,
            return_armed: false,
            layout: ScreenLayout::empty(),
            screen_map: Vec::new(),
            next_seq: 0,
        }
    }

    // —— 只读访问器 ——

    /// 角色。
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// 当前状态。
    #[must_use]
    pub const fn state(&self) -> &SessionState {
        &self.state
    }

    /// 会话标识（`HandshakeReady` 后为 `Some`；断开保留）。
    #[must_use]
    pub const fn session_id(&self) -> Option<[u8; 16]> {
        self.session_id
    }

    /// 协商结果（major 同、minor 取 min；docs/02 §5.2）。
    #[must_use]
    pub const fn negotiated(&self) -> Option<ProtoVer> {
        self.negotiated
    }

    /// 是否处于暂停子态（非 Running 恒 false）。
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        match self.state {
            SessionState::Running { paused } => paused,
            _ => false,
        }
    }

    /// 当前连接参数（`SetOptions` 更新后可见）。
    #[must_use]
    pub const fn options(&self) -> Options {
        self.options
    }

    /// 屏幕布局缓存（`CtrlMsg::Layout` / [`Session::set_layout`] 更新）。
    #[must_use]
    pub const fn layout(&self) -> &ScreenLayout {
        &self.layout
    }

    /// 主控侧直接安装布局（Host 上线时本地配置优先，后续仍可被对端 `Layout` 覆盖）。
    pub fn set_layout(&mut self, layout: ScreenLayout) {
        self.layout = layout;
    }

    /// 注入 ScreenId 全局 → 本机屏映射条目（docs/02 §4.2「ScreenId 跨机语义」）。
    ///
    /// 执行层收到 [`Notice::LayoutReceived`] 后按「`owner == 本机 instance_id`
    /// 条目序 → 本机显示枚举序」逐条调用；同 id 重复注入以最新为准（布局更新
    /// 重灌）。`EnterScreen.target` / `MouseMoveAbs.screen` 在产出注入/warp 动作
    /// 前查表换算，未命中回退直通。
    pub fn map_peer_screen(&mut self, peer: ScreenId, local: ScreenId) {
        match self.screen_map.iter_mut().find(|(id, _)| *id == peer) {
            Some(entry) => entry.1 = local,
            None => self.screen_map.push((peer, local)),
        }
    }

    /// 清空 ScreenId 全局 → 本机屏映射表（布局更新重灌前置——映射表语义是
    /// 「本次 `Layout` 的全量换算」，重装必须替换而非合并，否则已从布局摘除的
    /// 旧 id 残留旧映射，越界帧被错误换算；docs/02 §4.2「从机映射表」）。
    pub fn clear_screen_map(&mut self) {
        self.screen_map.clear();
    }

    /// ScreenId 全局 → 本机屏映射表（诊断/测试；条目序即注入序）。
    #[must_use]
    pub fn screen_map(&self) -> &[(ScreenId, ScreenId)] {
        &self.screen_map
    }

    /// 映射解析（未命中回退原值——1.0 无 Layout 会话与本机屏 id 直通）。
    fn resolve_local(&self, peer: ScreenId) -> ScreenId {
        self.screen_map
            .iter()
            .find(|(id, _)| *id == peer)
            .map_or(peer, |&(_, local)| local)
    }

    /// 本地修饰键快照（出站快照载荷来源）。
    #[must_use]
    pub const fn local_mods(&self) -> Mods {
        self.local_mods.pressed()
    }

    /// 已注入修饰键基准（入站对齐结果）。
    #[must_use]
    pub const fn injected_mods(&self) -> Mods {
        self.injected_mods.pressed()
    }

    /// 控制方向（决定周期 ModifierSync 的收发；见 [`ControlDir`]）。
    #[must_use]
    pub const fn control_dir(&self) -> ControlDir {
        self.control_dir
    }

    /// 受控目标屏（本机被控时为 `Some`，`EnterScreen.target` 全局 id）。
    /// 执行层据此把本机屏换算回全局 id 采样回报（双向边缘回切，docs/03 §5）。
    #[must_use]
    pub const fn controlled_target(&self) -> Option<ScreenId> {
        self.controlled_target
    }

    /// 对端显示名（协商成功后为 `Some`）。
    #[must_use]
    pub fn peer_name(&self) -> Option<&str> {
        self.peer_name.as_deref()
    }

    /// 对端实例 id（协商成功后为 `Some`）。
    #[must_use]
    pub const fn peer_instance_id(&self) -> Option<[u8; 16]> {
        self.peer_instance_id
    }

    /// 对端能力掩码（协商成功后为 `Some`）。
    #[must_use]
    pub const fn peer_caps(&self) -> Option<Caps> {
        self.peer_caps
    }

    // —— 事件入口 ——

    /// 处理一个会话事件：先按转移表判定，合法则执行 payload 并产出动作。
    ///
    /// # Errors
    /// [`StateError::PausedReject`]（拒帧不断连）/
    /// [`StateError::UnexpectedEvent`]（协议违规，调用方断开并可用
    /// [`Session::violation_actions`] 生成断开序列）/ [`StateError::AlreadyDisconnected`]。
    pub fn step(&mut self, event: SessionEvent) -> Result<Vec<SessionAction>, StateError> {
        let now = self.clock.now_ms();
        let kind = event.classify();
        // 活性：任何收到的帧都刷新基准（docs/02 §4.5「9s 无任何帧判死」——
        // 含未知 kind 与被 Paused 拒绝的帧；终态不刷新）。
        if matches!(event, SessionEvent::Frame(_)) && self.tag() != StateTag::Disconnected {
            self.last_frame_ms = Some(now);
        }
        let paused = self.is_paused();
        let tag = self.tag();
        match Self::transition_table(self.role, tag, paused, kind) {
            Ok(_outcome) => Ok(self.execute(event, now)),
            Err(Reject::PausedInput) => Err(StateError::PausedReject {
                kind: input_frame_kind(&event),
            }),
            Err(Reject::Disconnected) => Err(StateError::AlreadyDisconnected),
            Err(Reject::Unexpected) => Err(StateError::UnexpectedEvent {
                state: tag.label(paused),
                event: kind.label(),
            }),
        }
    }

    /// 协议违规断开序列（帧解码失败 / 非法转移时由上层调用）：发
    /// `Error{ProtocolViolation}` + 断开 + 通知。已断开则为空。
    pub fn violation_actions(&mut self, reason: &str) -> Vec<SessionAction> {
        if self.tag() == StateTag::Disconnected {
            return Vec::new();
        }
        let mut actions = vec![self.send_ctrl(CtrlMsg::Error {
            code: ErrCode::PROTOCOL_VIOLATION,
            msg: reason.to_string(),
        })];
        actions.extend(self.disconnect_actions(DisconnectReason::ProtocolViolation));
        actions
    }

    // —— 转移表（全部合法转移集中于此，测试全组合枚举） ——

    fn transition_table(
        role: Role,
        tag: StateTag,
        paused: bool,
        kind: EventKind,
    ) -> Result<Outcome, Reject> {
        use CtrlKind as CK;
        use EventKind as EK;
        use FrameClass as FC;
        use LocalKind as LK;
        use Outcome as O;
        use Reject as R;
        use Role as RO;
        use StateTag as ST;
        match (tag, kind) {
            // —— 终态：拒绝一切 ——
            (ST::Disconnected, _) => Err(R::Disconnected),
            // —— Idle：仅握手启动/心跳驱动/退出/修饰键快照 ——
            (ST::Idle, EK::HandshakeReady) => Ok(O::EnterHandshake),
            (ST::Idle, EK::Timer) => Ok(O::Ignore),
            (ST::Idle, EK::Local(LK::Quit)) => Ok(O::Quit),
            (ST::Idle, EK::Local(LK::LocalMods)) => Ok(O::Ignore),
            (ST::Idle, EK::Local(LK::MouseButtons)) => Ok(O::Ignore),
            // —— Handshaking：版本协商 + 宽容的早期控制帧 ——
            (ST::Handshaking, EK::Frame(FC::Ctrl(CK::Hello))) if role == RO::Client => {
                Ok(O::EnterRunning)
            }
            (ST::Handshaking, EK::Frame(FC::Ctrl(CK::HelloBack))) if role == RO::Host => {
                Ok(O::EnterRunning)
            }
            (ST::Handshaking, EK::Frame(FC::Ctrl(CK::Error))) => Ok(O::PeerFatal),
            (ST::Handshaking, EK::Frame(FC::Unknown))
            | (
                ST::Handshaking,
                EK::Frame(FC::Ctrl(CK::Ping | CK::Pong | CK::Layout | CK::SetOptions)),
            ) => Ok(O::Ignore),
            (ST::Handshaking, EK::Timer) => Ok(O::Ignore),
            (ST::Handshaking, EK::Local(LK::Quit)) => Ok(O::Quit),
            (ST::Handshaking, EK::Local(LK::LocalMods)) => Ok(O::Ignore),
            (ST::Handshaking, EK::Local(LK::MouseButtons)) => Ok(O::Ignore),
            // —— Running：Paused 只拒输入帧，控制帧照常 ——
            (ST::Running, EK::Frame(FC::Input)) if paused => Err(R::PausedInput),
            (ST::Running, EK::Frame(FC::Input)) => Ok(O::Passthrough),
            (ST::Running, EK::Frame(FC::Unknown)) => Ok(O::Ignore),
            (ST::Running, EK::Frame(FC::Ctrl(CK::Error))) => Ok(O::PeerFatal),
            (ST::Running, EK::Frame(FC::Ctrl(CK::Ping))) => Ok(O::Ping),
            (ST::Running, EK::Frame(FC::Ctrl(CK::Pong))) => Ok(O::Ignore),
            (ST::Running, EK::Frame(FC::Ctrl(CK::Layout | CK::SetOptions))) => Ok(O::Passthrough),
            (ST::Running, EK::Frame(FC::Ctrl(CK::Pause))) => Ok(O::SetPaused(true)),
            (ST::Running, EK::Frame(FC::Ctrl(CK::Resume))) => Ok(O::SetPaused(false)),
            (ST::Running, EK::Timer) => Ok(O::Heartbeat),
            (ST::Running, EK::Local(LK::Pause)) => Ok(O::SetPaused(true)),
            (ST::Running, EK::Local(LK::Resume)) => Ok(O::SetPaused(false)),
            (ST::Running, EK::Local(LK::Switch)) => Ok(O::Switch),
            (ST::Running, EK::Local(LK::EdgeCrossing)) => Ok(O::EdgeCrossing),
            // 回主控（FR-2.5 Home）：仅 LocalControlsPeer 时产生动作（execute 层判定），
            // 无受控 = 无操作；paused 下调用方门控拦截，表内仍放行（防御性无操作）。
            (ST::Running, EK::Local(LK::ReleaseControl)) => Ok(O::ReleaseControl),
            // 捕获转发：仅未暂停（Paused 下对端拒收输入帧，docs/02 §4.5）；
            // control_dir 门控（仅 LocalControlsPeer 转发）由调用方承担。
            (ST::Running, EK::Local(LK::ForwardInput)) if !paused => Ok(O::ForwardInput),
            // 受控光标位置回报（双向边缘回切，docs/02 §4.2 kind 0x04 反向语义）：
            // Running 内均可（Paused 下对端对回报宽容消化——共享暂停不打断观测
            // 面）；被控方向与目标门控在 execute 层判定（非被控 = 无操作）。
            (ST::Running, EK::Local(LK::ReportControlledCursor)) => Ok(O::Passthrough),
            // 布局下发（M5）：Running 内均可（控制帧，Paused 不拒；角色/门控在
            // execute 层判定——非 Host/空布局/协商 minor < 1 为无操作）。
            (ST::Running, EK::Local(LK::PublishLayout)) => Ok(O::Passthrough),
            (ST::Running, EK::Local(LK::Quit)) => Ok(O::Quit),
            (ST::Running, EK::Local(LK::LocalMods)) => Ok(O::Ignore),
            // 鼠标按键门控状态（11-22 第三轮）：Running 内均可（Paused 照常
            // 跟踪——恢复后状态正确）；纯本地观测更新，无动作产出。
            (ST::Running, EK::Local(LK::MouseButtons)) => Ok(O::Ignore),
            // —— 其余全部非法（含：未握手收帧、重复 HandshakeReady、
            //    Running 中 Hello/HelloBack、Handshaking 中输入帧、Clip 错通道等） ——
            _ => Err(R::Unexpected),
        }
    }

    // —— payload 执行层 ——

    fn execute(&mut self, event: SessionEvent, now: u64) -> Vec<SessionAction> {
        match event {
            SessionEvent::HandshakeReady { hs_hash } => self.execute_handshake_ready(hs_hash, now),
            SessionEvent::Frame(payload) => self.execute_frame(payload, now),
            SessionEvent::Timer => self.execute_timer(now),
            SessionEvent::Local(cmd) => self.execute_local(cmd, now),
        }
    }

    fn execute_handshake_ready(&mut self, hs_hash: [u8; 32], now: u64) -> Vec<SessionAction> {
        self.session_id = Some(derive_session_id(&hs_hash));
        self.state = SessionState::Handshaking;
        self.last_frame_ms = Some(now);
        match self.role {
            Role::Host => vec![self.send_ctrl(CtrlMsg::Hello {
                proto: ProtoVer::CURRENT,
                name: self.identity.name.clone(),
                screen_name: self.identity.screen_name.clone(),
                instance_id: self.identity.instance_id,
                caps: self.identity.caps,
                salt: self.identity.salt,
            })],
            Role::Client => Vec::new(),
        }
    }

    fn execute_frame(&mut self, payload: FramePayload, now: u64) -> Vec<SessionAction> {
        match payload {
            // 未知 kind：仅计活性（已在 step 记录），按 len 跳过（docs/02 §4.1）。
            FramePayload::Unknown { .. } => Vec::new(),
            // 不达：transition 拒绝 Clip；防御性空。
            FramePayload::Clip { .. } => Vec::new(),
            FramePayload::Input(event) => self.execute_input(event, now),
            FramePayload::Ctrl(msg) => self.execute_ctrl(msg, now),
        }
    }

    fn execute_input(&mut self, event: InputEvent, now: u64) -> Vec<SessionAction> {
        match event {
            InputEvent::Key { code, state } => {
                self.injected_mods.apply_key(code, state);
                vec![SessionAction::InjectInput(InputEvent::Key { code, state })]
            }
            InputEvent::MouseMoveAbs { screen, x, y } => {
                // 受控光标位置回报（11-21 反馈 5 双向边缘，docs/02 §4.2 kind 0x04
                // 反向语义）：本机为控制方时，对端把受控屏光标归一化位置以本帧
                // 回报——消化为回切判定输入，**不注入**（注入外来 ABS 会破坏受控
                // 锚定光标）。判定命中 → 回切动作；未命中按回报消化（同向武装随
                // 受控屏回报复位——「仍有同向移动」窗口 = 上一回报以来）；不匹配
                // 屏的回报静默消化（武装不动，非受控屏不参与判定）。
                if self.control_dir == ControlDir::LocalControlsPeer {
                    // 回切免疫窗按时间计（11-24）：判定取回报到达时刻（`now`）
                    // 与接管时刻之差，回报本身不再计数（foreign 屏回报不参与判定）。
                    return match self.edge_return_actions(screen, x, y, now) {
                        Some(actions) => actions,
                        None => {
                            if self.controlling.is_some_and(|ctx| ctx.target == screen) {
                                self.return_armed = false;
                            }
                            Vec::new()
                        }
                    };
                }
                // 控制方向之外：既有 ABS 注入语义不变——ScreenId 全局 → 本机屏
                // 映射（docs/02 §4.2；未命中直通，FR-2.7 绝对坐标的跨机 id 收口
                // ——注入层只看本机屏）。
                let screen = self.resolve_local(screen);
                vec![SessionAction::InjectInput(InputEvent::MouseMoveAbs {
                    screen,
                    x,
                    y,
                })]
            }
            InputEvent::EnterScreen {
                target,
                edge,
                ratio,
                mods,
                preserve_cursor,
            } => {
                // 受控开始：快照对齐无条件保留（docs/03 §3），控制方向置为被控；
                // 受控目标屏记录（受控光标回报的出站屏 id 来源，docs/02 §4.2）。
                self.control_dir = ControlDir::PeerControlsLocal;
                self.controlled_target = Some(target);
                let mut actions = self.align_actions(mods);
                // 光标保持原位（11-23）：热键/托盘切换路径不定位——被控端光标
                // 留在切出时的位置（用户语义）。边缘切换路径（preserve=0）照常
                // 按 edge/ratio 定位到进入点（空间连续穿行必需）。
                if !preserve_cursor {
                    let ratio = sanitize_ratio(ratio);
                    let (x, y) = edge_point(edge, ratio);
                    // warp 目标屏经映射表换算（docs/02 §4.2；未命中直通）。
                    let screen = self.resolve_local(target);
                    actions.push(SessionAction::WarpPointer { screen, x, y });
                }
                actions.push(SessionAction::Notice(Notice::EnteredControl { target }));
                actions
            }
            InputEvent::LeaveScreen { .. } => {
                // 切出：释放全部已注入按键并回本地控制（docs/03 §3）；
                // 控制方向复位（周期快照随控制关系终止）+ 受控目标/回切上下文清除。
                self.injected_mods.clear();
                self.control_dir = ControlDir::None;
                self.controlled_target = None;
                self.controlling = None;
                self.return_armed = false;
                vec![
                    SessionAction::ReleaseAll,
                    SessionAction::Notice(Notice::LeftControl),
                ]
            }
            InputEvent::ModifierSync { mods } => {
                // 单向语义（docs/03 §3）：仅被控方应用周期快照；其余方向宽容忽略
                // （不违规断开）——无人在控时对端的 sync 不得制造幻按键注入。
                if self.control_dir == ControlDir::PeerControlsLocal {
                    self.align_actions(mods)
                } else {
                    Vec::new()
                }
            }
            InputEvent::SwitchRequest { target } => {
                // M4-hotkeys（FR-2.5 PRD 要求 6）：受控切换前先释放本机已注入按键
                // （防"旧会话残留 + 新会话注入"叠加卡键；M1 仅 Ack，释放由
                // LeaveScreen 承接——热键切换路径无 EdgeCrossing 前置，故在此补齐）。
                // M1 无能力否决逻辑，默认可受控即确认。
                self.injected_mods.clear();
                vec![
                    SessionAction::ReleaseAll,
                    self.send_input(InputEvent::SwitchAck { target, ok: true }),
                    SessionAction::Notice(Notice::SwitchRequested { target }),
                ]
            }
            InputEvent::SwitchAck { target, ok } => {
                let mut actions = vec![SessionAction::Notice(Notice::SwitchResult { target, ok })];
                // M5 切换闭环（FR-2.5，docs/03 §5 热键闭环）：Ack 确认可受控 →
                // 主控立即 EdgeCrossing 同型接管（ReleaseAll + LeaveScreen +
                // EnterScreen）；进入边从布局图推导（无邻边回退 target 左缘 =
                // Next 语义），ratio 居中。被控中不接管（切换门控的镜像防御）；
                // 非 Host（理论不可达：SwitchRequest 主控→从机单向）不接管。
                // 热键/托盘切换（11-23 用户语义）：被控端光标保持原位——不进
                // EdgeCrossing（那会走边缘落点定位），改为直接接管 + EnterScreen
                // preserve_cursor=true（被控端跳过 warp）。边缘切换路径不受影响。
                if ok
                    && self.role == Role::Host
                    && self.control_dir != ControlDir::PeerControlsLocal
                {
                    self.control_dir = ControlDir::LocalControlsPeer;
                    // 回切触发边 = 受控屏面向主控的边 = **进入边**（11-24 更正，
                    // docs/03 §5）：`entry_edge_into` 返回的已是受控屏的进入边，
                    // 即 `ControllingContext.outer_edge` 的定义，与边缘路径
                    // （`opposite_edge(主控滑出边)` 恰为进入边）同值——原再取一次
                    // 对侧得到的是远离主控的边，热键切入后推回面向主控的边无效。
                    let entry_edge = crate::layout::entry_edge_into(&self.layout, target)
                        .unwrap_or(ScreenEdge::Left);
                    self.controlling = Some(ControllingContext {
                        target,
                        outer_edge: entry_edge,
                        taken_over_ms: now,
                    });
                    self.return_armed = false;
                    let mods = self.local_mods.pressed();
                    actions.push(SessionAction::ReleaseAll);
                    actions.push(self.send_input(InputEvent::LeaveScreen { mods }));
                    actions.push(self.send_input(InputEvent::EnterScreen {
                        target,
                        edge: entry_edge,
                        ratio: crate::layout::SWITCH_ENTRY_RATIO,
                        mods,
                        // 热键切换：光标原地不动（11-23 用户定案）。
                        preserve_cursor: true,
                    }));
                }
                actions
            }
            other => vec![SessionAction::InjectInput(other)],
        }
    }

    fn execute_ctrl(&mut self, msg: CtrlMsg, now: u64) -> Vec<SessionAction> {
        match msg {
            CtrlMsg::Hello {
                proto,
                name,
                instance_id,
                caps,
                ..
            } => match negotiate(ProtoVer::CURRENT, proto) {
                Ok(ver) => {
                    self.complete_handshake(ver, Some(name), Some(instance_id), Some(caps), now);
                    let mut actions = match self.role {
                        Role::Client => vec![self.send_ctrl(CtrlMsg::HelloBack {
                            proto: ProtoVer::CURRENT,
                            name: self.identity.name.clone(),
                            instance_id: self.identity.instance_id,
                            caps: self.identity.caps,
                        })],
                        Role::Host => Vec::new(),
                    };
                    actions.push(self.handshake_complete_notice());
                    actions
                }
                Err(_) => self.fail_version(proto),
            },
            CtrlMsg::HelloBack {
                proto,
                name,
                instance_id,
                caps,
            } => match negotiate(ProtoVer::CURRENT, proto) {
                Ok(ver) => {
                    self.complete_handshake(ver, Some(name), Some(instance_id), Some(caps), now);
                    vec![self.handshake_complete_notice()]
                }
                Err(_) => self.fail_version(proto),
            },
            CtrlMsg::Layout { screens } => {
                // M5 布局协商（FR-1.3）：缓存 + 通知（执行层据此构建 ScreenId
                // → 本机屏映射表，docs/02 §4.2）。
                let count = screens.len();
                self.layout = ScreenLayout::new(screens);
                vec![SessionAction::Notice(Notice::LayoutReceived {
                    screens: count,
                })]
            }
            CtrlMsg::SetOptions { options } => {
                self.apply_options(options);
                Vec::new()
            }
            CtrlMsg::Error { code, msg } => {
                let reason = DisconnectReason::PeerError { code, msg };
                self.disconnect_actions(reason)
            }
            CtrlMsg::Ping { ts_ms } => vec![self.send_ctrl(CtrlMsg::Pong { ts_ms })],
            CtrlMsg::Pong { .. } => Vec::new(),
            CtrlMsg::Pause => {
                self.set_paused(true);
                self.injected_mods.clear();
                vec![SessionAction::ReleaseAll]
            }
            CtrlMsg::Resume => {
                self.set_paused(false);
                Vec::new()
            }
        }
    }

    fn execute_timer(&mut self, now: u64) -> Vec<SessionAction> {
        // 判死优先（docs/02 §4.5：9s 无任何帧 → 断开）。
        match self.last_frame_ms {
            None => return Vec::new(),
            Some(last_frame) => {
                if now.saturating_sub(last_frame) >= u64::from(self.options.dead_after_ms) {
                    return self.disconnect_actions(DisconnectReason::Timeout);
                }
            }
        }
        let mut actions = Vec::new();
        if now.saturating_sub(self.last_ping_ms.unwrap_or(now))
            >= u64::from(self.options.ping_interval_ms)
        {
            actions.push(self.send_ctrl(CtrlMsg::Ping { ts_ms: now }));
            self.last_ping_ms = Some(now);
        }
        // 周期快照单向（docs/03 §3）：仅本机控制对端时外发，防止无人在控时
        // 对端被注入幻按键（P1 回归修复）。
        if self.control_dir == ControlDir::LocalControlsPeer
            && now.saturating_sub(self.last_modsync_ms.unwrap_or(now)) >= MODIFIER_SYNC_INTERVAL_MS
        {
            actions.push(self.send_input(InputEvent::ModifierSync {
                mods: self.local_mods.pressed(),
            }));
            self.last_modsync_ms = Some(now);
        }
        actions
    }

    fn execute_local(&mut self, cmd: LocalCmd, now: u64) -> Vec<SessionAction> {
        match cmd {
            LocalCmd::Pause => {
                self.set_paused(true);
                self.injected_mods.clear();
                // 先释放本机已注入按键，再通知对端（防卡键顺序，docs/03 §5）。
                vec![SessionAction::ReleaseAll, self.send_ctrl(CtrlMsg::Pause)]
            }
            LocalCmd::Resume => {
                self.set_paused(false);
                vec![self.send_ctrl(CtrlMsg::Resume)]
            }
            LocalCmd::Switch { target } => {
                vec![self.send_input(InputEvent::SwitchRequest { target })]
            }
            LocalCmd::Quit => self.disconnect_actions(DisconnectReason::LocalQuit),
            LocalCmd::EdgeCrossing {
                target,
                edge,
                ratio,
            } => {
                // docs/03 §5：主控释放本机按键（防卡键）→ 发 EnterScreen；
                // LeaveScreen{mods} 一并通知（对端释放/对齐依据，docs/02 §4.2 kind 0x11）。
                // 本机切出成为控制方：周期 ModifierSync 由此开启（docs/03 §3 单向语义）。
                //
                // edge 语义转换（11-20 实测修订，docs/02 §4.2 kind 0x10）：
                // 上游 `EdgeHit.edge`/热键闭环的 `entry_edge_into` 传的是**主控
                // 滑出边**（mac 右缘 = Right）；而 EnterScreen 的 edge 按 wire
                // 契约是**从机进入边**（从机左缘 = Left）——对侧转换在此收口，
                // 从机 warp 落点即正确（mac 右滑 → 从机左缘进入）。
                self.control_dir = ControlDir::LocalControlsPeer;
                // 双向边缘回切上下文（11-21 反馈 5 / 11-22 语义改判）：回切触发
                // 边 = 受控屏**面向主控的边** = 主控滑出边的对侧（空间连续穿行，
                // 见 ControllingContext 文档）；同向武装复位——免疫窗（慢速切入
                // 回弹修复，按时间计）自接管起计。
                self.controlling = Some(ControllingContext {
                    target,
                    outer_edge: opposite_edge(edge),
                    taken_over_ms: now,
                });
                self.return_armed = false;
                let mods = self.local_mods.pressed();
                vec![
                    SessionAction::ReleaseAll,
                    self.send_input(InputEvent::LeaveScreen { mods }),
                    self.send_input(InputEvent::EnterScreen {
                        target,
                        edge: opposite_edge(edge),
                        ratio: sanitize_ratio(ratio),
                        mods,
                        // 边缘切换：空间连续穿行，定位到进入边（11-23）。
                        preserve_cursor: false,
                    }),
                ]
            }
            LocalCmd::LocalMods(mods) => {
                self.local_mods.set(mods);
                Vec::new()
            }
            LocalCmd::MouseButtons { any_down } => {
                // 按键门控状态直通（11-22 第三轮，docs/03 §5）：全状态更新
                // （Paused 照常——恢复后状态正确），无动作产出。
                self.local_mouse_down = any_down;
                Vec::new()
            }
            LocalCmd::ReleaseControl => {
                // FR-2.5 Home（主控侧自切）：仅本机控制对端时生效——释放本机
                // 已注入按键（防卡键，docs/03 §5 顺序）+ 发 LeaveScreen（对端
                // 释放本机按键）+ 控制方向复位（周期 ModifierSync 随控制关系终止）。
                if self.control_dir != ControlDir::LocalControlsPeer {
                    return Vec::new(); // 无受控：无操作（调用方记日志）
                }
                self.control_dir = ControlDir::None;
                self.controlling = None;
                self.return_armed = false;
                let mods = self.local_mods.pressed();
                // 11-20 回切落点修复：Home 回主控时光标 warp 回主屏**内侧**
                // （0.5/0.6 略偏下，避开边缘容差带——与 edge_point 死区同源，
                // 防回切落点立即再触发边缘切换）。macOS 主控在 app 层以切入
                // 锚点替换此落点（11-24，docs/03 §4.6：受控期间光标锚定在切入
                // 处，回主控即原地解冻）。
                vec![
                    SessionAction::ReleaseAll,
                    self.send_input(InputEvent::LeaveScreen { mods }),
                    SessionAction::WarpPointer {
                        screen: 0,
                        x: 0.5,
                        y: 0.6,
                    },
                ]
            }
            LocalCmd::ForwardInput(event) => {
                // 主控捕获转发（FR-2.x 链路）：热键过滤后的未命中事件出站。
                // seq 计数与普通出站同源（Outbound::Input）。
                // 双向边缘回切同向武装（11-21 反馈 5，docs/03 §5）：转发受控屏
                // 滑出方向的相对移动 = 用户仍在向外推——与受控屏回报极值共同
                // 构成回切触发（驻留边缘无移动不触发）。
                if let (Some(ctx), InputEvent::MouseMoveRel { dx, dy }) = (self.controlling, &event)
                    && is_outward(ctx.outer_edge, *dx, *dy)
                {
                    self.return_armed = true;
                }
                vec![self.send_input(event)]
            }
            LocalCmd::ReportControlledCursor { x, y } => {
                // 受控光标位置回报（docs/02 §4.2 kind 0x04 反向语义）：仅被控方
                // 且受控目标已知时出站（屏 id = EnterScreen.target 全局空间）；
                // 非有限坐标防御丢弃（协议浮点拒绝语义，不违规断开——回报是
                // 观测面而非注入路径）。
                if self.control_dir != ControlDir::PeerControlsLocal {
                    return Vec::new();
                }
                let Some(target) = self.controlled_target else {
                    return Vec::new();
                };
                if !x.is_finite() || !y.is_finite() {
                    return Vec::new();
                }
                vec![self.send_input(InputEvent::MouseMoveAbs {
                    screen: target,
                    x,
                    y,
                })]
            }
            LocalCmd::PublishLayout => {
                // M5 布局下发（FR-1.3 协商）：仅 Host、布局非空、协商 minor ≥
                // 本实现 wire 级（ScreenInfo.owner 属 1.1 形态——1.0 对端按旧
                // 结构解码会违规断开，docs/02 §4.3 修订记录/§5.3 版本门控）。
                // negotiated minor = min(双方) ≤ 本端 minor：≥ 即两端同级。
                if self.role != Role::Host || self.layout.is_empty() {
                    return Vec::new();
                }
                if !self
                    .negotiated
                    .is_some_and(|ver| ver.minor >= xcontrol_proto::PROTO_MINOR)
                {
                    return Vec::new();
                }
                vec![self.send_ctrl(CtrlMsg::Layout {
                    screens: self.layout.screens().clone(),
                })]
            }
        }
    }

    // —— 内部辅助 ——

    /// 受控屏外侧滑出回切判定（双向边缘回切，11-21 反馈 5，docs/03 §5）。
    ///
    /// 输入 = 被控方回报的受控屏光标位置（`MouseMoveAbs` 反向语义）与回报到达
    /// 时刻 `now`。触发条件（全部满足）：
    /// 1. 回报到达时刻已在回切免疫窗外（[`RETURN_IMMUNITY_MS`]，自接管起按
    ///    时间计——与回报频率解耦，11-24）；
    /// 2. 本机为控制方且回报屏 = 受控屏（上下文匹配）；
    /// 3. 坐标有限（协议浮点拒绝语义的防御面）；
    /// 4. 落在受控屏**外侧边**极值（对端容差带收敛契约：边缘驻留回报恰
    ///    0.0/1.0——内侧不触发，非极值不触发）；
    /// 5. **角落排除**（11-22 第三轮，docs/03 §5；11-24 改为 5% 角落区）：正交轴
    ///    回报落在角落带（[`crate::layout::in_corner_band`]，与切入侧共用同一
    ///    比例——光标在屏角的角落区内贴边）→ 不触发——角落留给系统热区（KDE
    ///    触发角专用），回报照常消化、武装照常复位；
    /// 6. **按键门控**（11-22 第三轮，docs/03 §5）：任意鼠标键按下（拖拽中）
    ///    → 不触发——回报照常消化；松开后的下一次同向移动再武装；
    /// 7. 同向武装（上次回报以来仍转发过滑出方向 `MouseMoveRel`——「仍有同
    ///    方向移动事件」；边缘驻留无移动不触发）。
    ///
    /// 触发产出 = ReleaseControl 同型释放序列（防卡键顺序 docs/03 §5：
    /// `ReleaseAll` → `LeaveScreen{mods}`）+ 环绕落点 warp + [`Notice::EdgeReturnedToHost`]
    /// （携带回报段回切行程，[`beyond_edge`]；11-24），并清除控制关系（周期快照
    /// 随控制关系终止）。**回切行程补偿不改变触发条件**：越界的回报值照常按
    /// `≤ 0` / `≥ 1` 判极值，角落带仍只看沿边坐标。
    ///
    /// 范围裁定（11-21）：**单跳**——外侧边即使有邻居（多屏链 [主控][受控][C]）
    /// 也回主控，链式回邻屏移交后续里程碑（当前部署 2 屏）。
    fn edge_return_actions(
        &mut self,
        screen: ScreenId,
        x: f32,
        y: f32,
        now: u64,
    ) -> Option<Vec<SessionAction>> {
        let ctx = self.controlling?;
        // 免疫窗（11-22 慢速切入回弹修复；11-24 改按时间）：接管后 800ms 内
        // 到达的回报不触发——判定基准 = 回报到达时刻（step 读时钟）− 接管时刻，
        // 与回报条数/采样频率无关。
        if now.saturating_sub(ctx.taken_over_ms) < RETURN_IMMUNITY_MS {
            return None;
        }
        if ctx.target != screen || !x.is_finite() || !y.is_finite() || !self.return_armed {
            return None;
        }
        if !at_outer_extreme(ctx.outer_edge, x, y) {
            return None;
        }
        // 角落排除（11-22 第三轮，docs/03 §5；11-24 改为 5% 角落区）：正交轴
        // 回报落在角落带 = 光标在屏角的角落区内贴边（用户明确意图：角落留给
        // 系统热区，边缘=切换）——回报照常消化（下方 None 路径随条复位武装，
        // 键松开/离角后的下一次同向移动再武装）。
        if orthogonal_in_corner_band(ctx.outer_edge, x, y) {
            return None;
        }
        // 按键门控（11-22 第三轮，docs/03 §5）：任意鼠标键按下（拖文件/拖滚动
        // 条）→ 不回切——回报照常消化；热键回切（ReleaseControl）不经此判定，
        // 不受门控限制。
        if self.local_mouse_down {
            return None;
        }
        // ratio = 滑出点沿边坐标（水平外侧边取 y、垂直取 x），环绕保持。
        let ratio = match ctx.outer_edge {
            ScreenEdge::Left | ScreenEdge::Right => sanitize_ratio(y),
            ScreenEdge::Top | ScreenEdge::Bottom => sanitize_ratio(x),
        };
        // 回切行程补偿（11-24，R8；docs/02 §4.2 反向回报取值范围扩展）：触发
        // 回切的回报里端点之外的值 = 被控方顶边期间被屏幕边界截掉的位移，解出
        // 后随通知带出（落点 warp 本身不含补偿，由执行层加到落点上）。上方的
        // 角落带与落点 ratio 判定不受越界值影响（前者带内即不触发，后者已钳回
        // [0,1]）；无越界（含被控端不度量行程的端点值回报）时行程为 0。
        let overflow = (beyond_edge(x), beyond_edge(y));
        let (landing_screen, landing_x, landing_y) = self.edge_return_landing(ctx, ratio);
        self.control_dir = ControlDir::None;
        self.controlling = None;
        self.return_armed = false;
        let mods = self.local_mods.pressed();
        Some(vec![
            SessionAction::ReleaseAll,
            self.send_input(InputEvent::LeaveScreen { mods }),
            SessionAction::WarpPointer {
                screen: landing_screen,
                x: landing_x,
                y: landing_y,
            },
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: ctx.target,
                overflow,
            }),
        ])
    }

    /// 环绕落点（11-21 环绕闭合裁定）：受控屏外侧边滑出 → 主控**面向受控屏
    /// 的那条边**内侧内缩点（[受控][主控] 链：右缘滑出 → 主控左缘内侧
    /// [`EDGE_RETURN_INSET`]；[主控][受控] 链对称：左缘滑出 → 主控右缘内侧）。
    /// 落点死区与进入落点同源——内缩在边缘传感器容差带之外，回切后静置不可能
    /// 立即再触发；持续朝受控屏方向外推的再触发由 app 层切入防抖（session_rt
    /// `EDGE_SWITCH_DEAD_TIME`）与传感器闩锁承接。
    ///
    /// 落点屏解析：沿 `facing_edge`（= 主控的切出边，即 `outer_edge` 的反向）找
    /// 受控屏的邻居，且 `owner` = 本机 →
    /// 该屏；缺失回退任一条边的主控邻屏（L 形多邻接），再回退主屏 0（布局
    /// 缺失/热点 Next 语义无邻边的接管路径）。
    fn edge_return_landing(&self, ctx: ControllingContext, ratio: f32) -> (ScreenId, f32, f32) {
        let facing_edge = opposite_edge(ctx.outer_edge);
        let facing = self
            .layout
            .neighbor(ctx.target, facing_edge)
            .filter(|id| {
                self.layout
                    .screen(*id)
                    .is_some_and(|s| s.owner == self.identity.instance_id)
            })
            .or_else(|| {
                [
                    ScreenEdge::Left,
                    ScreenEdge::Right,
                    ScreenEdge::Top,
                    ScreenEdge::Bottom,
                ]
                .into_iter()
                .find_map(|edge| {
                    self.layout.neighbor(ctx.target, edge).filter(|id| {
                        self.layout
                            .screen(*id)
                            .is_some_and(|s| s.owner == self.identity.instance_id)
                    })
                })
            })
            .unwrap_or(0);
        // 落点内缩用**回切落点死区**（EDGE_RETURN_INSET = 0.5%，贴缘衔接）而非
        // 进入落点死区——两死区语义分离见 EDGE_ENTRY_INSET 文档。
        let inset = EDGE_RETURN_INSET;
        let (x, y) = match facing_edge {
            ScreenEdge::Left => (inset, ratio),
            ScreenEdge::Right => (1.0 - inset, ratio),
            ScreenEdge::Top => (ratio, inset),
            ScreenEdge::Bottom => (ratio, 1.0 - inset),
        };
        (facing, x, y)
    }

    /// 进入 `Disconnected` 的统一收尾（语义裁定：断开即控制关系终结，docs/03 §3
    /// 防卡键不变量的断开分支）：释放全部已注入按键 + 清基准 + 断开通知。
    fn disconnect_actions(&mut self, reason: DisconnectReason) -> Vec<SessionAction> {
        self.injected_mods.clear();
        // 双向边缘回切上下文随断开终结（control_dir 保留原值供诊断——终态后
        // 不再参与任何判定，docs/03 §3 断开即控制关系终结）。
        self.controlling = None;
        self.controlled_target = None;
        self.return_armed = false;
        self.state = SessionState::Disconnected(reason.clone());
        vec![
            SessionAction::ReleaseAll,
            SessionAction::Notice(Notice::Disconnected(reason)),
        ]
    }

    /// 转移表键：当前状态大类。
    const fn tag(&self) -> StateTag {
        match self.state {
            SessionState::Idle => StateTag::Idle,
            SessionState::Handshaking => StateTag::Handshaking,
            SessionState::Running { .. } => StateTag::Running,
            SessionState::Disconnected(_) => StateTag::Disconnected,
        }
    }

    fn complete_handshake(
        &mut self,
        ver: ProtoVer,
        peer_name: Option<String>,
        peer_instance_id: Option<[u8; 16]>,
        peer_caps: Option<Caps>,
        now: u64,
    ) {
        self.negotiated = Some(ver);
        self.peer_name = peer_name;
        self.peer_instance_id = peer_instance_id;
        self.peer_caps = peer_caps;
        self.state = SessionState::Running { paused: false };
        self.last_frame_ms = Some(now);
        self.last_ping_ms = Some(now);
        self.last_modsync_ms = Some(now);
        // 新会话从无人在控开始（控制方向由 EdgeCrossing/EnterScreen 重新建立；
        // 双向边缘回切上下文一并复位——新会话不带旧控制关系的判定状态）。
        self.control_dir = ControlDir::None;
        self.controlling = None;
        self.controlled_target = None;
        self.return_armed = false;
    }

    fn handshake_complete_notice(&self) -> SessionAction {
        SessionAction::Notice(Notice::HandshakeComplete {
            session_id: self.session_id.unwrap_or([0; 16]),
            minor: self.negotiated.map_or(0, |ver| ver.minor),
        })
    }

    fn fail_version(&mut self, remote: ProtoVer) -> Vec<SessionAction> {
        let mut actions = vec![self.send_ctrl(CtrlMsg::Error {
            code: ErrCode::UNSUPPORTED_VERSION,
            msg: format!(
                "版本不兼容：本地 {}.{}, 对端 {}.{}",
                ProtoVer::CURRENT.major,
                ProtoVer::CURRENT.minor,
                remote.major,
                remote.minor
            ),
        })];
        actions.extend(self.disconnect_actions(DisconnectReason::VersionIncompatible));
        actions
    }

    /// 快照对齐（切入/周期同步共用）：合成 Up(extra)/Down(missing) 注入事件，
    /// 基准更新为快照（docs/03 §3 防卡键）。
    fn align_actions(&mut self, snapshot: Mods) -> Vec<SessionAction> {
        let mut actions = Vec::new();
        for (code, state) in self.injected_mods.sync_actions(snapshot) {
            actions.push(SessionAction::InjectInput(InputEvent::Key { code, state }));
        }
        self.injected_mods.set(snapshot);
        actions
    }

    fn apply_options(&mut self, options: Options) {
        // 0 值视为「字段缺失」，保持现值（对端不可用 0 关闭心跳）。
        if options.ping_interval_ms != 0 {
            self.options.ping_interval_ms = options.ping_interval_ms;
        }
        if options.dead_after_ms != 0 {
            self.options.dead_after_ms = options.dead_after_ms;
        }
        if options.clipboard_max_bytes != 0 {
            self.options.clipboard_max_bytes = options.clipboard_max_bytes;
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if let SessionState::Running { .. } = self.state {
            self.state = SessionState::Running { paused };
        }
    }

    fn send_ctrl(&mut self, msg: CtrlMsg) -> SessionAction {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        SessionAction::Send {
            seq,
            msg: Outbound::Ctrl(msg),
        }
    }

    fn send_input(&mut self, event: InputEvent) -> SessionAction {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        SessionAction::Send {
            seq,
            msg: Outbound::Input(event),
        }
    }
}

/// Paused 拒绝错误所需的帧 kind（仅 Input 帧会触发；其余防御性 0）。
fn input_frame_kind(event: &SessionEvent) -> u8 {
    match event {
        SessionEvent::Frame(FramePayload::Input(input)) => input.kind(),
        _ => 0,
    }
}

/// 对侧边（11-20 EdgeCrossing 滑出边 → EnterScreen 进入边转换用；
/// 与 layout::opposite_edge 同语义，session 私有内联避免改上游可见性）。
const fn opposite_edge(edge: ScreenEdge) -> ScreenEdge {
    match edge {
        ScreenEdge::Left => ScreenEdge::Right,
        ScreenEdge::Right => ScreenEdge::Left,
        ScreenEdge::Top => ScreenEdge::Bottom,
        ScreenEdge::Bottom => ScreenEdge::Top,
    }
}

/// `EnterScreen` 到达点的**内缩死区**（11-20 两机实测回弹修复）：落点若恰好
/// 压在边上（0.0/1.0），从机侧任一微小回移立刻反向触发切换（边界抖动）。
/// 进入落点从边内缩 5%——docs/03 §5 死区语义的落点侧收口。
///
/// **与回切落点死区分离（11-22 两轮实测裁定）**：切入落点与回切落点消费面
/// 不同（被控端 EnterScreen warp vs 主控端释放 warp），语义不共享。
/// **1% → 0.5%（11-22 第三轮回调）**：切入贴缘衔接与回切落点对称；原 5% 的
/// 防弹回职责已由回切免疫窗（`RETURN_IMMUNITY_MS`，按时间 800ms）承接——切入
/// 惯性在免疫窗内自然消散，死区本身不再承担防弹回，贴缘安全。0.5% = mac
/// 逻辑宽轴 7.5pt（1512）/ Linux 实机 4K 宽轴 19px（3840，docs/03 §5 KDE
/// 触发角适配节），距容差带（±1 逻辑点）仍有 7.5 倍余量（mac），不触碰
/// 传感器判定面。
const EDGE_ENTRY_INSET: f32 = 0.005;

/// 回切落点的**贴缘内缩**（11-22 用户实测：回切落点要求贴缘衔接，5% 感知
/// "离边缘太远"）：主控面向受控屏的那条边内侧内缩——防弹回的真实防线是边缘传感
/// 器容差带（±1 逻辑点）+ 切换防抖 + 回切免疫窗（`RETURN_IMMUNITY_MS`）。
/// **1% → 0.5%（11-22 第三轮）**：与切入落点死区同源减半（感知 ~0.5cm），
/// mac 7.5pt 仍 7.5 倍于容差带，安全余量保持。
///
/// 11-24 起公开：主控侧的回切落点补偿（app 层把回切行程加到落点上，docs/03 §5
/// 回切第 3 条）夹回屏内时以**同一内缩量**作四周留白——补偿后的落点不得落到
/// 别条边的容差带或角落区里。内缩量只此一处定义，执行层不另写比例。
pub const EDGE_RETURN_INSET: f32 = 0.005;

/// `EnterScreen` 到达点：边上坐标内缩 [`EDGE_ENTRY_INSET`]（Left/Right → x 固定，
/// y=ratio；Top/Bottom 对称）。
const fn edge_point(edge: ScreenEdge, ratio: f32) -> (f32, f32) {
    let inset = EDGE_ENTRY_INSET;
    match edge {
        ScreenEdge::Left => (inset, ratio),
        ScreenEdge::Right => (1.0 - inset, ratio),
        ScreenEdge::Top => (ratio, inset),
        ScreenEdge::Bottom => (ratio, 1.0 - inset),
    }
}

/// 本地命令的 ratio 清洗：非有限 → 0.0，越界 → clamp [0,1]。
fn sanitize_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 受控屏外侧边极值判定（双向边缘回切触发条件之一，docs/03 §5）：回报坐标
/// 是否落在 `outer` 边的极值端点。对端回报为容差带收敛值（边缘驻留恰 0.0/1.0，
/// docs/02 §4.2 kind 0x04 反向语义修订）——精确比较即采样语义，与
/// [`crate::layout::edge_crossing`] 的 `≤0`/`≥1` 触边同款。
const fn at_outer_extreme(outer: ScreenEdge, x: f32, y: f32) -> bool {
    match outer {
        ScreenEdge::Left => x <= 0.0,
        ScreenEdge::Right => x >= 1.0,
        ScreenEdge::Top => y <= 0.0,
        ScreenEdge::Bottom => y >= 1.0,
    }
}

/// 受控屏回报单轴的**端点之外行程**（11-24 回切行程补偿，纯函数；docs/02 §4.2
/// 「反向回报取值范围扩展到端点之外」）：`v < 0` → `v`（推出左/上边界的行程，
/// 负值）；`v > 1` → `v − 1`（推出右/下边界的行程，正值）；`[0, 1]` 内 → 0
/// （无越界——含被控端不度量行程时的端点值回报 0.0/1.0）。单位 = 控制方转发的
/// `MouseMoveRel` delta 单位，符号与屏幕坐标轴同向，两轴分量可直接相加到落点上。
/// 非有限值由调用方先行拒绝（[`Session::edge_return_actions`]），本函数不另设防。
fn beyond_edge(v: f32) -> f32 {
    if v < 0.0 {
        v
    } else if v > 1.0 {
        v - 1.0
    } else {
        0.0
    }
}

/// 角落区判定（11-22 第三轮角落排除，docs/03 §5；11-24 由"正交轴恰为端点"
/// 改为 5% 角落区）：外侧边极值命中时，**正交轴**回报落在角落带
/// （[`crate::layout::in_corner_band`]——与切入侧
/// [`crate::layout::edge_crossing`] 共用同一比例 [`crate::layout::CORNER_RATIO`]，
/// 判定处不另写比例）→ 光标在屏角的角落区内贴边。角落留给系统热区（KDE 触发
/// 角专用，边缘=切换），角落驻留不回切。垂直外侧边（Left/Right）看沿边坐标 y，
/// 水平外侧边（Top/Bottom）看 x；非有限值已由调用方先行拒绝。
const fn orthogonal_in_corner_band(outer: ScreenEdge, x: f32, y: f32) -> bool {
    match outer {
        ScreenEdge::Left | ScreenEdge::Right => crate::layout::in_corner_band(y),
        ScreenEdge::Top | ScreenEdge::Bottom => crate::layout::in_corner_band(x),
    }
}

/// 相对移动是否朝 `outer` 边方向（同向武装判定）：归一化坐标 y 向下（Top =
/// y 减小侧），与捕获层 delta 轴向一致（mac 正 delta = 右/下）。
const fn is_outward(outer: ScreenEdge, dx: f32, dy: f32) -> bool {
    match outer {
        ScreenEdge::Left => dx < 0.0,
        ScreenEdge::Right => dx > 0.0,
        ScreenEdge::Top => dy < 0.0,
        ScreenEdge::Bottom => dy > 0.0,
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::ScreenInfo;
    use xcontrol_proto::input::{KeyState, ScreenEdge};
    use xcontrol_proto::xkey::{KEY_A, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_LEFT_SHIFT};

    use super::*;
    use crate::clock::TestClock;

    /// 公共夹具：Client 会话（虚拟时钟、默认心跳参数），返回（会话, 时钟句柄）。
    fn client_session(clock: TestClock) -> Session<TestClock> {
        Session::new(
            Role::Client,
            Identity {
                name: "client".into(),
                screen_name: "client-screen".into(),
                instance_id: [1; 16],
                caps: Caps::UNICODE,
                salt: [7; 16],
            },
            clock,
            Options::defaults(),
        )
    }

    /// 公共夹具：Host 会话。
    fn host_session(clock: TestClock) -> Session<TestClock> {
        Session::new(
            Role::Host,
            Identity {
                name: "host".into(),
                screen_name: "host-screen".into(),
                instance_id: [2; 16],
                caps: Caps::UNICODE.union(Caps::ABS_POINTER),
                salt: [9; 16],
            },
            clock,
            Options::defaults(),
        )
    }

    fn hello_msg(major: u8, minor: u8) -> CtrlMsg {
        CtrlMsg::Hello {
            proto: ProtoVer { major, minor },
            name: "peer".into(),
            screen_name: "peer-screen".into(),
            instance_id: [3; 16],
            caps: Caps::UNICODE,
            salt: [5; 16],
        }
    }

    /// 推进 Client 会话到 Running（握手完成于当前时刻），返回 HandshakeComplete 通知。
    fn ready_running(session: &mut Session<TestClock>) -> Notice {
        let actions = session
            .step(SessionEvent::HandshakeReady {
                hs_hash: [0xAB; 32],
            })
            .unwrap();
        assert!(actions.is_empty(), "client 就绪无出站");
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(hello_msg(1, 0))))
            .unwrap();
        assert_eq!(actions.len(), 2, "HelloBack + HandshakeComplete");
        match &actions[1] {
            SessionAction::Notice(notice) => notice.clone(),
            other => panic!("期望 Notice，实际 {other:?}"),
        }
    }

    /// 推进 Host 会话到 Running（就绪发 Hello = seq0，回 HelloBack），返回通知。
    fn ready_running_host(session: &mut Session<TestClock>) -> Notice {
        let actions = session
            .step(SessionEvent::HandshakeReady {
                hs_hash: [0xCD; 32],
            })
            .unwrap();
        assert_eq!(actions.len(), 1, "Host 就绪发 Hello");
        let back = CtrlMsg::HelloBack {
            proto: ProtoVer::CURRENT,
            name: "client".into(),
            instance_id: [1; 16],
            caps: Caps::UNICODE,
        };
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(back)))
            .unwrap();
        assert_eq!(actions.len(), 1, "仅 HandshakeComplete 通知");
        match &actions[0] {
            SessionAction::Notice(notice) => notice.clone(),
            other => panic!("期望 Notice，实际 {other:?}"),
        }
    }

    /// 收集动作里的 Send seq。
    fn collect_seq(action: &SessionAction, out: &mut Vec<u32>) {
        if let SessionAction::Send { seq, .. } = action {
            out.push(*seq);
        }
    }

    /// warp 记录 sink（闭环用例断言 `EnterScreen` 到达点；mock 模块 feature 门控
    /// 不可用，本地最小实现——route.rs tests 的 MockSink 同形）。
    #[derive(Default)]
    struct WarpRecorder {
        warps: Vec<(ScreenId, f32, f32)>,
    }

    impl crate::sink::InputSink for WarpRecorder {
        fn inject(&mut self, _event: &InputEvent) -> Result<(), crate::sink::SinkError> {
            Ok(())
        }

        fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), crate::sink::SinkError> {
            self.warps.push((screen, x, y));
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), crate::sink::SinkError> {
            Ok(())
        }
    }

    // —— session_id ——

    /// docs/02 §3.3：派生确定性 + 16B + 输入敏感；生命周期 None→Some→断开保留。
    #[test]
    fn session_id_derivation_and_lifecycle() {
        let h1 = [1u8; 32];
        let h2 = [2u8; 32];
        assert_eq!(derive_session_id(&h1), derive_session_id(&h1));
        assert_ne!(derive_session_id(&h1), derive_session_id(&h2));
        let id = derive_session_id(&h1);
        assert_eq!(id.len(), 16);

        let mut session = client_session(TestClock::new());
        assert_eq!(session.session_id(), None);
        session
            .step(SessionEvent::HandshakeReady { hs_hash: h1 })
            .unwrap();
        assert_eq!(session.session_id(), Some(id));
        // 断开后保留（日志/审计）
        session.step(SessionEvent::Local(LocalCmd::Quit)).unwrap();
        assert_eq!(session.session_id(), Some(id));
    }

    // —— 转移表全组合 ——

    /// 转移表键全集（25 种事件）。
    fn all_event_kinds() -> Vec<EventKind> {
        use CtrlKind as CK;
        use FrameClass as FC;
        use LocalKind as LK;
        vec![
            EventKind::HandshakeReady,
            EventKind::Timer,
            EventKind::Frame(FC::Input),
            EventKind::Frame(FC::Clip),
            EventKind::Frame(FC::Unknown),
            EventKind::Frame(FC::Ctrl(CK::Hello)),
            EventKind::Frame(FC::Ctrl(CK::HelloBack)),
            EventKind::Frame(FC::Ctrl(CK::Layout)),
            EventKind::Frame(FC::Ctrl(CK::SetOptions)),
            EventKind::Frame(FC::Ctrl(CK::Error)),
            EventKind::Frame(FC::Ctrl(CK::Ping)),
            EventKind::Frame(FC::Ctrl(CK::Pong)),
            EventKind::Frame(FC::Ctrl(CK::Pause)),
            EventKind::Frame(FC::Ctrl(CK::Resume)),
            EventKind::Local(LK::Pause),
            EventKind::Local(LK::Resume),
            EventKind::Local(LK::Switch),
            EventKind::Local(LK::Quit),
            EventKind::Local(LK::EdgeCrossing),
            EventKind::Local(LK::LocalMods),
            EventKind::Local(LK::MouseButtons),
            EventKind::Local(LK::ReleaseControl),
            EventKind::Local(LK::ForwardInput),
            EventKind::Local(LK::ReportControlledCursor),
            EventKind::Local(LK::PublishLayout),
        ]
    }

    /// 全组合枚举（2 角色 × 5 态暂停组合 × 25 事件 = 250）：接受集 = 白名单
    /// （逐条显式声明 + 期望 Outcome），其余必须拒绝。
    #[test]
    fn transition_table_full_matrix() {
        type Entry = (Role, StateTag, bool, EventKind, Outcome);

        fn accept(
            roles: &[Role],
            tag: StateTag,
            paused: bool,
            kind: EventKind,
            outcome: Outcome,
            out: &mut Vec<Entry>,
        ) {
            for &role in roles {
                out.push((role, tag, paused, kind, outcome));
            }
        }

        let both = [Role::Host, Role::Client];
        let mut whitelist: Vec<Entry> = Vec::new();

        // —— Idle（角色无关）——
        for (kind, outcome) in [
            (EventKind::HandshakeReady, Outcome::EnterHandshake),
            (EventKind::Timer, Outcome::Ignore),
            (EventKind::Local(LocalKind::Quit), Outcome::Quit),
            (EventKind::Local(LocalKind::LocalMods), Outcome::Ignore),
            (EventKind::Local(LocalKind::MouseButtons), Outcome::Ignore),
        ] {
            accept(&both, StateTag::Idle, false, kind, outcome, &mut whitelist);
        }
        // —— Handshaking（Hello 仅 Client、HelloBack 仅 Host；其余角色无关）——
        accept(
            &[Role::Client],
            StateTag::Handshaking,
            false,
            EventKind::Frame(FrameClass::Ctrl(CtrlKind::Hello)),
            Outcome::EnterRunning,
            &mut whitelist,
        );
        accept(
            &[Role::Host],
            StateTag::Handshaking,
            false,
            EventKind::Frame(FrameClass::Ctrl(CtrlKind::HelloBack)),
            Outcome::EnterRunning,
            &mut whitelist,
        );
        for (kind, outcome) in [
            (
                EventKind::Frame(FrameClass::Ctrl(CtrlKind::Error)),
                Outcome::PeerFatal,
            ),
            (EventKind::Frame(FrameClass::Unknown), Outcome::Ignore),
            (
                EventKind::Frame(FrameClass::Ctrl(CtrlKind::Ping)),
                Outcome::Ignore,
            ),
            (
                EventKind::Frame(FrameClass::Ctrl(CtrlKind::Pong)),
                Outcome::Ignore,
            ),
            (
                EventKind::Frame(FrameClass::Ctrl(CtrlKind::Layout)),
                Outcome::Ignore,
            ),
            (
                EventKind::Frame(FrameClass::Ctrl(CtrlKind::SetOptions)),
                Outcome::Ignore,
            ),
            (EventKind::Timer, Outcome::Ignore),
            (EventKind::Local(LocalKind::Quit), Outcome::Quit),
            (EventKind::Local(LocalKind::LocalMods), Outcome::Ignore),
            (EventKind::Local(LocalKind::MouseButtons), Outcome::Ignore),
        ] {
            accept(
                &both,
                StateTag::Handshaking,
                false,
                kind,
                outcome,
                &mut whitelist,
            );
        }
        // —— Running（paused 两态同白名单，唯 Frame(Input) 仅未暂停接受）——
        for paused in [false, true] {
            for (kind, outcome) in [
                (EventKind::Frame(FrameClass::Unknown), Outcome::Ignore),
                (
                    EventKind::Frame(FrameClass::Ctrl(CtrlKind::Error)),
                    Outcome::PeerFatal,
                ),
                (
                    EventKind::Frame(FrameClass::Ctrl(CtrlKind::Ping)),
                    Outcome::Ping,
                ),
                (
                    EventKind::Frame(FrameClass::Ctrl(CtrlKind::Pong)),
                    Outcome::Ignore,
                ),
                (
                    EventKind::Frame(FrameClass::Ctrl(CtrlKind::Layout)),
                    Outcome::Passthrough,
                ),
                (
                    EventKind::Frame(FrameClass::Ctrl(CtrlKind::SetOptions)),
                    Outcome::Passthrough,
                ),
                (
                    EventKind::Frame(FrameClass::Ctrl(CtrlKind::Pause)),
                    Outcome::SetPaused(true),
                ),
                (
                    EventKind::Frame(FrameClass::Ctrl(CtrlKind::Resume)),
                    Outcome::SetPaused(false),
                ),
                (EventKind::Timer, Outcome::Heartbeat),
                (EventKind::Local(LocalKind::Pause), Outcome::SetPaused(true)),
                (
                    EventKind::Local(LocalKind::Resume),
                    Outcome::SetPaused(false),
                ),
                (EventKind::Local(LocalKind::Switch), Outcome::Switch),
                (EventKind::Local(LocalKind::Quit), Outcome::Quit),
                (
                    EventKind::Local(LocalKind::EdgeCrossing),
                    Outcome::EdgeCrossing,
                ),
                (
                    EventKind::Local(LocalKind::ReleaseControl),
                    Outcome::ReleaseControl,
                ),
                // 受控光标回报（双向边缘回切）：Running 内均可（Paused 宽容），
                // 被控门控在 execute 层。
                (
                    EventKind::Local(LocalKind::ReportControlledCursor),
                    Outcome::Passthrough,
                ),
                // 布局下发（M5）：控制帧语义，Paused 下照常（execute 层门控）。
                (
                    EventKind::Local(LocalKind::PublishLayout),
                    Outcome::Passthrough,
                ),
                (EventKind::Local(LocalKind::LocalMods), Outcome::Ignore),
                // 鼠标按键门控状态（11-22 第三轮）：Running 内均可（Paused
                // 照常跟踪），纯本地观测更新。
                (EventKind::Local(LocalKind::MouseButtons), Outcome::Ignore),
            ] {
                accept(
                    &both,
                    StateTag::Running,
                    paused,
                    kind,
                    outcome,
                    &mut whitelist,
                );
            }
            if !paused {
                accept(
                    &both,
                    StateTag::Running,
                    paused,
                    EventKind::Frame(FrameClass::Input),
                    Outcome::Passthrough,
                    &mut whitelist,
                );
                // 捕获转发仅未暂停（Paused 下对端拒收输入帧）
                accept(
                    &both,
                    StateTag::Running,
                    paused,
                    EventKind::Local(LocalKind::ForwardInput),
                    Outcome::ForwardInput,
                    &mut whitelist,
                );
            }
        }

        // 全组合断言：接受集精确等于白名单。
        let mut accepted = 0;
        for role in [Role::Host, Role::Client] {
            for (tag, paused) in [
                (StateTag::Idle, false),
                (StateTag::Handshaking, false),
                (StateTag::Running, false),
                (StateTag::Running, true),
                (StateTag::Disconnected, false),
            ] {
                for kind in all_event_kinds() {
                    let actual = Session::<TestClock>::transition_table(role, tag, paused, kind);
                    match whitelist.iter().find(|(r, t, p, k, _)| {
                        *r == role && *t == tag && *p == paused && *k == kind
                    }) {
                        Some((_, _, _, _, outcome)) => {
                            assert_eq!(
                                actual,
                                Ok(*outcome),
                                "{role:?} {tag:?} paused={paused} {kind:?}"
                            );
                            accepted += 1;
                        }
                        None => {
                            assert!(
                                actual.is_err(),
                                "意外接受：{role:?} {tag:?} paused={paused} {kind:?}"
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(
            accepted, 112,
            "白名单应精确覆盖接受集（250 组合中的合法 112 组：M4 ReleaseControl×4 + ForwardInput×2；M5 PublishLayout×4；11-21 ReportControlledCursor×4；11-22 MouseButtons×8〔Idle×2 + Handshaking×2 + Running 未暂停/暂停×2 角色×2〕）"
        );
    }

    /// 关键拒绝形态的具体错误变体。
    #[test]
    fn rejection_error_variants() {
        // Idle + 收帧 = 未握手违规
        let mut session = client_session(TestClock::new());
        let err = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(hello_msg(1, 0))))
            .unwrap_err();
        assert_eq!(
            err,
            StateError::UnexpectedEvent {
                state: "Idle",
                event: "Frame(Ctrl(Hello))"
            }
        );
        // Host 在 Handshaking 收到 Hello = 角色错位
        let mut host = host_session(TestClock::new());
        host.step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .unwrap();
        let err = host
            .step(SessionEvent::Frame(FramePayload::Ctrl(hello_msg(1, 0))))
            .unwrap_err();
        assert_eq!(
            err,
            StateError::UnexpectedEvent {
                state: "Handshaking",
                event: "Frame(Ctrl(Hello))"
            }
        );
        // Running + 重复 HandshakeReady
        let mut client = client_session(TestClock::new());
        ready_running(&mut client);
        let err = client
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .unwrap_err();
        assert_eq!(
            err,
            StateError::UnexpectedEvent {
                state: "Running",
                event: "HandshakeReady"
            }
        );
        // Running + Clip 帧（错误通道）
        let err = client
            .step(SessionEvent::Frame(FramePayload::Clip { kind: 0x50 }))
            .unwrap_err();
        assert_eq!(
            err,
            StateError::UnexpectedEvent {
                state: "Running",
                event: "Frame(Clip)"
            }
        );
        // 终态：一切拒绝为 AlreadyDisconnected
        client.step(SessionEvent::Local(LocalCmd::Quit)).unwrap();
        for event in [
            SessionEvent::Timer,
            SessionEvent::Local(LocalCmd::Resume),
            SessionEvent::Frame(FramePayload::Unknown { kind: 0x60 }),
        ] {
            assert_eq!(client.step(event), Err(StateError::AlreadyDisconnected));
        }
    }

    // —— 握手与版本协商 ——

    /// Client 快路径：HandshakeReady 无出站 → Hello → HelloBack + 通知，minor 取 min。
    #[test]
    fn client_handshake_happy_path() {
        let mut session = client_session(TestClock::new());
        assert_eq!(session.state(), &SessionState::Idle);
        assert_eq!(session.session_id(), None);
        let actions = session
            .step(SessionEvent::HandshakeReady {
                hs_hash: [0xAB; 32],
            })
            .unwrap();
        assert!(actions.is_empty());
        assert_eq!(session.state(), &SessionState::Handshaking);
        assert_eq!(session.session_id(), Some(derive_session_id(&[0xAB; 32])));

        // minor 1.1 ↔ 1.7 → 会话 minor = 1
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(hello_msg(1, 7))))
            .unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0],
            SessionAction::Send {
                seq: 0,
                msg: Outbound::Ctrl(CtrlMsg::HelloBack {
                    proto: ProtoVer::CURRENT,
                    name: "client".into(),
                    instance_id: [1; 16],
                    caps: Caps::UNICODE,
                })
            }
        );
        assert_eq!(
            actions[1],
            SessionAction::Notice(Notice::HandshakeComplete {
                session_id: derive_session_id(&[0xAB; 32]),
                minor: 1,
            })
        );
        assert_eq!(session.state(), &SessionState::Running { paused: false });
        assert_eq!(session.negotiated(), Some(ProtoVer { major: 1, minor: 1 }));
        assert_eq!(session.peer_name(), Some("peer"));
        assert_eq!(session.peer_instance_id(), Some([3; 16]));
        assert_eq!(session.peer_caps(), Some(Caps::UNICODE));
    }

    /// Host 快路径：HandshakeReady 先发 Hello → HelloBack（1.7）→ Running，无回帧。
    #[test]
    fn host_handshake_happy_path() {
        let mut session = host_session(TestClock::new());
        let actions = session
            .step(SessionEvent::HandshakeReady {
                hs_hash: [0xCD; 32],
            })
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Send {
                seq: 0,
                msg: Outbound::Ctrl(CtrlMsg::Hello {
                    proto: ProtoVer::CURRENT,
                    name: "host".into(),
                    screen_name: "host-screen".into(),
                    instance_id: [2; 16],
                    caps: Caps::UNICODE.union(Caps::ABS_POINTER),
                    salt: [9; 16],
                })
            }]
        );
        let back = CtrlMsg::HelloBack {
            proto: ProtoVer { major: 1, minor: 7 },
            name: "peer".into(),
            instance_id: [3; 16],
            caps: Caps::UNICODE,
        };
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(back)))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Notice(Notice::HandshakeComplete {
                session_id: derive_session_id(&[0xCD; 32]),
                minor: 1, // min(1, 7)
            })]
        );
        assert_eq!(session.state(), &SessionState::Running { paused: false });
        assert_eq!(session.negotiated(), Some(ProtoVer { major: 1, minor: 1 }));
    }

    /// major 不等 → Error{UnsupportedVersion} + 断开（docs/02 §5.2）。
    #[test]
    fn version_mismatch_sends_error_and_disconnects() {
        let mut session = client_session(TestClock::new());
        session
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .unwrap();
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(hello_msg(2, 0))))
            .unwrap();
        assert_eq!(actions.len(), 3, "Error 帧 + ReleaseAll + 断开通知");
        assert_eq!(
            actions[0],
            SessionAction::Send {
                seq: 0,
                msg: Outbound::Ctrl(CtrlMsg::Error {
                    code: ErrCode::UNSUPPORTED_VERSION,
                    msg: "版本不兼容：本地 1.1, 对端 2.0".into(),
                })
            }
        );
        assert_eq!(actions[1], SessionAction::ReleaseAll);
        assert_eq!(
            actions[2],
            SessionAction::Notice(Notice::Disconnected(DisconnectReason::VersionIncompatible))
        );
        assert_eq!(
            session.state(),
            &SessionState::Disconnected(DisconnectReason::VersionIncompatible)
        );
    }

    // —— 心跳（虚拟时钟） ——

    /// 3s Ping / 1s ModifierSync 周期（docs/02 §4.5 + docs/03 §3）。
    /// 前置：本机为控制方（`EdgeCrossing` 切出，[`ControlDir::LocalControlsPeer`]）——
    /// 单向语义下只有控制方才外发周期快照。
    #[test]
    fn heartbeat_ping_and_modifier_sync_periods() {
        let clock = TestClock::new();
        let mut session = host_session(clock.clone());
        ready_running_host(&mut session); // t=0：Hello = seq0
        session
            .step(SessionEvent::Local(LocalCmd::LocalMods(
                Mods::LCTRL.union(Mods::LMETA),
            )))
            .unwrap();
        // 本机切出 → 成为控制方（seq1 LeaveScreen + seq2 EnterScreen）
        session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            }))
            .unwrap();

        // t=1000：仅 ModifierSync（未到 3s Ping）
        clock.advance(1000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Send {
                seq: 3,
                msg: Outbound::Input(InputEvent::ModifierSync {
                    mods: Mods::LCTRL.union(Mods::LMETA),
                })
            }]
        );
        // t=3000：Ping + ModifierSync（上次 sync t=1000，间隔 2000 ≥ 1000）
        clock.advance(2000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                seq: 4,
                msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 3000 })
            }
        ));
        assert!(matches!(
            &actions[1],
            SessionAction::Send {
                seq: 5,
                msg: Outbound::Input(InputEvent::ModifierSync { .. })
            }
        ));
        // t=6000：Ping 到期（上次 t=3000）+ ModifierSync（间隔 3000 ≥ 1000）
        clock.advance(3000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                seq: 6,
                msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 6000 })
            }
        ));
    }

    /// 9s 无帧判死（docs/02 §4.5）；无人在控（None 方向）时周期快照静默。
    #[test]
    fn heartbeat_dead_after() {
        let clock = TestClock::new();
        let mut session = client_session(clock.clone());
        ready_running(&mut session); // last_frame = 0
        // t=8999：仍活，Ping 周期已到；None 方向 → 无 ModifierSync
        clock.advance(8999);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(actions.len(), 1, "仅 Ping，不断开");
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 8999 }),
                ..
            }
        ));
        assert_eq!(session.state(), &SessionState::Running { paused: false });
        // t=9000：判死（ReleaseAll 兜底 + 通知）
        clock.advance(1);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(
            actions,
            vec![
                SessionAction::ReleaseAll,
                SessionAction::Notice(Notice::Disconnected(DisconnectReason::Timeout)),
            ]
        );
        assert_eq!(
            session.state(),
            &SessionState::Disconnected(DisconnectReason::Timeout)
        );
    }

    /// 对端 Ping 刷新活性：t=4000 收 Ping → t=12000 仍活（8000 < 9000）→ t=13000 判死。
    #[test]
    fn incoming_frame_resets_death_timer() {
        let clock = TestClock::new();
        let mut session = client_session(clock.clone());
        ready_running(&mut session);
        clock.advance(4000);
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(CtrlMsg::Ping {
                ts_ms: 4000,
            })))
            .unwrap();
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Pong { ts_ms: 4000 }),
                ..
            }
        ));
        clock.advance(8000); // t=12000，距 last_frame=4000 有 8000
        session.step(SessionEvent::Timer).unwrap();
        assert!(!matches!(session.state(), SessionState::Disconnected(_)));
        clock.advance(1000); // t=13000，9000 达阈值
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert!(matches!(
            actions.last(),
            Some(SessionAction::Notice(Notice::Disconnected(
                DisconnectReason::Timeout
            )))
        ));
    }

    /// 未知 kind 计活性：t=8000 收未知帧 → 判死点推迟到 17000。
    #[test]
    fn unknown_kind_counts_as_liveness() {
        let clock = TestClock::new();
        let mut session = client_session(clock.clone());
        ready_running(&mut session);
        clock.advance(8000);
        session
            .step(SessionEvent::Frame(FramePayload::Unknown { kind: 0x60 }))
            .unwrap();
        clock.advance(8999); // t=16999
        session.step(SessionEvent::Timer).unwrap();
        assert!(!matches!(session.state(), SessionState::Disconnected(_)));
        clock.advance(1); // t=17000
        session.step(SessionEvent::Timer).unwrap();
        assert!(matches!(
            session.state(),
            SessionState::Disconnected(DisconnectReason::Timeout)
        ));
    }

    /// SetOptions 更新心跳周期；0 值字段忽略（对端不可用 0 关闭心跳）。
    #[test]
    fn set_options_updates_timers() {
        let clock = TestClock::new();
        let mut session = client_session(clock.clone());
        ready_running(&mut session);
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(
                CtrlMsg::SetOptions {
                    options: Options {
                        ping_interval_ms: 15000,
                        dead_after_ms: 60000, // 放宽判死，聚焦 Ping 周期
                        clipboard_max_bytes: 1024,
                    },
                },
            )))
            .unwrap();
        assert_eq!(
            session.options(),
            Options {
                ping_interval_ms: 15000,
                dead_after_ms: 60000,
                clipboard_max_bytes: 1024,
            }
        );
        // 0 值字段忽略（追加一条 SetOptions 验证）
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(
                CtrlMsg::SetOptions {
                    options: Options {
                        ping_interval_ms: 0,
                        dead_after_ms: 0,
                        clipboard_max_bytes: 0,
                    },
                },
            )))
            .unwrap();
        assert_eq!(
            session.options(),
            Options {
                ping_interval_ms: 15000,
                dead_after_ms: 60000,
                clipboard_max_bytes: 1024,
            }
        );
        // t=3000：默认周期已改 15s → 无 Ping；None 方向 → 无 ModifierSync
        clock.advance(3000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert!(actions.is_empty(), "None 方向周期快照静默");
        // t=15000：Ping 到期
        clock.advance(12000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 15000 }),
                ..
            }
        ));
    }

    // —— 暂停语义 ——

    /// Paused 拒绝输入帧（拒帧不断连）；控制帧/恢复照常。
    #[test]
    fn paused_rejects_input_frames() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(CtrlMsg::Pause)))
            .unwrap();
        assert_eq!(actions, vec![SessionAction::ReleaseAll]);
        assert_eq!(session.state(), &SessionState::Running { paused: true });

        let key = InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        };
        let err = session
            .step(SessionEvent::Frame(FramePayload::Input(key.clone())))
            .unwrap_err();
        assert_eq!(err, StateError::PausedReject { kind: 0x01 });
        assert_eq!(session.state(), &SessionState::Running { paused: true });

        // 恢复后同样事件被注入
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(CtrlMsg::Resume)))
            .unwrap();
        assert_eq!(session.state(), &SessionState::Running { paused: false });
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(key)))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::InjectInput(InputEvent::Key {
                code: KEY_A,
                state: KeyState::Down,
            })]
        );
    }

    /// 本地暂停：释放 + 发 Pause 帧；恢复对称。
    #[test]
    fn local_pause_sends_and_releases() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        let actions = session.step(SessionEvent::Local(LocalCmd::Pause)).unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0], SessionAction::ReleaseAll);
        assert!(matches!(
            &actions[1],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Pause),
                ..
            }
        ));
        assert_eq!(session.state(), &SessionState::Running { paused: true });
        let actions = session.step(SessionEvent::Local(LocalCmd::Resume)).unwrap();
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Resume),
                ..
            }
        ));
        assert_eq!(session.state(), &SessionState::Running { paused: false });
    }

    // —— 修饰键三场景（docs/03 §3） ——

    /// 场景 1（切入对齐）：残留 LALT，快照 LCTRL → 合成 Up/Down + warp + 通知。
    #[test]
    fn enter_screen_aligns_stale_mods_and_warps() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        // 先注入 LALT（残留）
        session
            .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
                code: KEY_LEFT_ALT,
                state: KeyState::Down,
            })))
            .unwrap();
        assert_eq!(session.injected_mods(), Mods::LALT);

        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Left,
                    ratio: 0.6,
                    mods: Mods::LCTRL,
                    preserve_cursor: false,
                },
            )))
            .unwrap();
        assert_eq!(
            actions,
            vec![
                SessionAction::InjectInput(InputEvent::Key {
                    code: KEY_LEFT_ALT,
                    state: KeyState::Up,
                }),
                SessionAction::InjectInput(InputEvent::Key {
                    code: KEY_LEFT_CTRL,
                    state: KeyState::Down,
                }),
                SessionAction::WarpPointer {
                    screen: 1,
                    x: 0.005, // 进入落点死区 0.5%（11-22 第三轮，防弹回由免疫窗承接）——该测试喂 EnterScreen{Left}
                    y: 0.6,
                },
                SessionAction::Notice(Notice::EnteredControl { target: 1 }),
            ]
        );
        assert_eq!(session.injected_mods(), Mods::LCTRL);
        assert_eq!(session.control_dir(), ControlDir::PeerControlsLocal);
    }

    /// 场景 2（切出释放）：LeaveScreen → ReleaseAll + 基准清空 + 控制方向复位。
    #[test]
    fn leave_screen_releases_everything() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        // 先建立被控方向
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Left,
                    ratio: 0.5,
                    mods: Mods::default(),
                    preserve_cursor: false,
                },
            )))
            .unwrap();
        session
            .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
                code: KEY_LEFT_SHIFT,
                state: KeyState::Down,
            })))
            .unwrap();
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::LeaveScreen { mods: Mods::LSHIFT },
            )))
            .unwrap();
        assert_eq!(
            actions,
            vec![
                SessionAction::ReleaseAll,
                SessionAction::Notice(Notice::LeftControl),
            ]
        );
        assert_eq!(session.injected_mods(), Mods::default());
        assert_eq!(session.control_dir(), ControlDir::None);
    }

    /// 场景 3（周期同步）：被控方向（PeerControlsLocal）下 ModifierSync 全量校验，
    /// 差异合成 Up。
    #[test]
    fn modifier_sync_periodic_validation() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        // 受控开始（EnterScreen 建立方向 + 对齐基准为 LSHIFT）
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Left,
                    ratio: 0.5,
                    mods: Mods::LSHIFT,
                    preserve_cursor: false,
                },
            )))
            .unwrap();
        assert_eq!(session.control_dir(), ControlDir::PeerControlsLocal);
        for code in [KEY_LEFT_SHIFT, KEY_LEFT_META] {
            session
                .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
                    code,
                    state: KeyState::Down,
                })))
                .unwrap();
        }
        assert_eq!(session.injected_mods(), Mods::LSHIFT.union(Mods::LMETA));
        // 对端快照只剩 LSHIFT → 合成 LMETA Up
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::ModifierSync { mods: Mods::LSHIFT },
            )))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::InjectInput(InputEvent::Key {
                code: KEY_LEFT_META,
                state: KeyState::Up,
            })]
        );
        assert_eq!(session.injected_mods(), Mods::LSHIFT);
    }

    /// Host 边缘穿越：释放 + LeaveScreen/EnterScreen 成对发出（mods 快照随行；
    /// 越界 ratio clamp）。
    #[test]
    fn host_edge_crossing_emits_release_and_pair() {
        let mut session = host_session(TestClock::new());
        ready_running_host(&mut session); // Hello = seq0
        session
            .step(SessionEvent::Local(LocalCmd::LocalMods(Mods::LCTRL)))
            .unwrap();
        let actions = session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Left,
                ratio: 0.4,
            }))
            .unwrap();
        assert_eq!(
            actions,
            vec![
                SessionAction::ReleaseAll,
                SessionAction::Send {
                    seq: 1,
                    msg: Outbound::Input(InputEvent::LeaveScreen { mods: Mods::LCTRL }),
                },
                SessionAction::Send {
                    seq: 2,
                    msg: Outbound::Input(InputEvent::EnterScreen {
                        target: 1,
                        edge: ScreenEdge::Right,
                        ratio: 0.4,
                        mods: Mods::LCTRL,
                        preserve_cursor: false,
                    }),
                },
            ]
        );
        // 本机切出 → 成为控制方（周期快照外发的依据）
        assert_eq!(session.control_dir(), ControlDir::LocalControlsPeer);
        // 越界 ratio 清洗到 [0,1]
        let actions = session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Top,
                ratio: 2.5,
            }))
            .unwrap();
        assert!(matches!(
            actions.last(),
            Some(SessionAction::Send { msg: Outbound::Input(InputEvent::EnterScreen { ratio, .. }), .. }) if *ratio == 1.0
        ));
    }

    // —— 切换请求（FR-2.5） ——

    /// Switch 本地命令 → SwitchRequest；对端「ReleaseAll + SwitchAck(ok=true)」
    /// （M4-hotkeys 补释放，PRD 要求 6）；结果通知。
    #[test]
    fn switch_request_flow() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        let actions = host
            .step(SessionEvent::Local(LocalCmd::Switch { target: 1 }))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Send {
                seq: 1,
                msg: Outbound::Input(InputEvent::SwitchRequest { target: 1 }),
            }]
        );
        let mut client = client_session(TestClock::new());
        ready_running(&mut client);
        // 受控切换前先注入按键（模拟旧会话残留）→ SwitchRequest 到达必须先释放
        client
            .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
                code: KEY_LEFT_SHIFT,
                state: KeyState::Down,
            })))
            .unwrap();
        let actions = client
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::SwitchRequest { target: 1 },
            )))
            .unwrap();
        assert_eq!(actions.len(), 3, "ReleaseAll + SwitchAck + Notice");
        assert_eq!(actions[0], SessionAction::ReleaseAll);
        assert!(matches!(
            &actions[1],
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::SwitchAck {
                    target: 1,
                    ok: true
                }),
                ..
            }
        ));
        assert_eq!(
            actions[2],
            SessionAction::Notice(Notice::SwitchRequested { target: 1 })
        );
        assert_eq!(
            client.injected_mods(),
            Mods::default(),
            "残留注入基准已清空"
        );
        // Host 收到确认（无布局：进入边回退 target 左缘 = Next 语义，docs/03 §5）
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::SwitchAck {
                    target: 1,
                    ok: true,
                },
            )))
            .unwrap();
        assert_eq!(actions.len(), 4, "通知 + 接管序列（ReleaseAll + 2×Send）");
        assert_eq!(
            actions[0],
            SessionAction::Notice(Notice::SwitchResult {
                target: 1,
                ok: true,
            })
        );
        assert_eq!(actions[1], SessionAction::ReleaseAll, "接管前置释放");
        assert!(matches!(
            &actions[2],
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::LeaveScreen { .. }),
                ..
            }
        ));
        assert!(
            matches!(
                &actions[3],
                SessionAction::Send {
                    msg: Outbound::Input(InputEvent::EnterScreen {
                        target: 1,
                        // 11-20 语义链：无布局回退 = 主控右滑（滑出边 Right）
                        // → EnterScreen 进入边 Left（对侧转换收口在 EdgeCrossing）。
                        edge: ScreenEdge::Left,
                        ratio,
                        .. }),
                    ..
                }
                if *ratio == crate::layout::SWITCH_ENTRY_RATIO
            ),
            "无布局回退左缘 + 居中 ratio（实际 {actions:?}）"
        );
        assert_eq!(
            host.control_dir(),
            ControlDir::LocalControlsPeer,
            "接管后主控为控制方"
        );
    }

    /// 切换闭环（M5，docs/03 §5 热键闭环）全链路：Host 装布局（主屏右邻从机屏）
    /// → PublishLayout 下发 → 从机映射注入 → Switch → Ack → 接管 EnterScreen
    /// 进入边从布局推导（target 在主屏右侧 = 左缘进入）。
    #[test]
    fn switch_closed_loop_with_layout_and_entry_edge() {
        use xcontrol_config::LayoutScreen;

        use crate::layout::{ScreenLayout, merge_layout};

        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host.set_layout(merge_layout(
            &ScreenLayout::new(vec![ScreenInfo {
                id: 0,
                owner: [2; 16],
                name: "Display 0".into(),
                left: None,
                right: None,
                top: None,
                bottom: None,
                scale: 1.0,
            }]),
            [2; 16],
            [1; 16],
            &[LayoutScreen {
                id: "slave-main".into(),
                relative_to: None,
                direction: Some(xcontrol_config::Direction::Right),
            }],
        ));
        // 协商 1.1（两端 CURRENT）→ Layout 下发（Hello=seq0 后 seq1）
        let actions = host
            .step(SessionEvent::Local(LocalCmd::PublishLayout))
            .unwrap();
        assert_eq!(actions.len(), 1, "PublishLayout 单帧出站");
        let mut stream = Vec::new();
        let SessionAction::Send { seq, msg } = &actions[0] else {
            panic!("期望 Send，实际 {actions:?}");
        };
        msg.encode_frame_into(*seq, &mut stream).unwrap();

        // 从机收 Layout：缓存 + 通知（执行层据以建映射表）
        let mut client = client_session(TestClock::new());
        ready_running(&mut client);
        let outcome = {
            let mut dispatcher = crate::route::Dispatcher::new(&mut client);
            dispatcher.feed(&stream)
        };
        assert_eq!(outcome.fatal, None);
        assert_eq!(
            outcome.actions,
            vec![SessionAction::Notice(Notice::LayoutReceived { screens: 2 })],
            "{:?}",
            outcome.actions
        );
        assert_eq!(client.layout().len(), 2);
        // 从机映射注入（执行层按 owner 条目序：id1 → 本机屏 0）
        client.map_peer_screen(1, 0);

        // Host Switch → 从机 Ack（布局推导进入边：主屏右邻 slave-main → 左缘）
        host.step(SessionEvent::Local(LocalCmd::Switch { target: 1 }))
            .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::SwitchAck {
                    target: 1,
                    ok: true,
                },
            )))
            .unwrap();
        assert!(
            matches!(
                actions.last(),
                Some(SessionAction::Send {
                    msg: Outbound::Input(InputEvent::EnterScreen {
                        target: 1,
                        edge: ScreenEdge::Left,
                        ratio: 0.5,
                        ..
                    }),
                    ..
                })
            ),
            "布局推导进入边 = target 左缘（实际 {actions:?}）"
        );

        // 把接管序列喂给从机：EnterScreen 的 warp 已映射到本机屏 0
        let mut wire = Vec::new();
        for action in &actions[2..] {
            if let SessionAction::Send { seq, msg } = action {
                msg.encode_frame_into(*seq, &mut wire).unwrap();
            }
        }
        let outcome = {
            let mut dispatcher = crate::route::Dispatcher::new(&mut client);
            dispatcher.feed(&wire)
        };
        assert_eq!(outcome.fatal, None);
        let mut sink = WarpRecorder::default();
        crate::route::apply_actions(&outcome.actions, &mut sink, &mut |_, _| {});
        // 热键切换（11-23 用户定案）：preserve_cursor=true → 被控端**不定位**
        // ——光标留在切出时的位置（无 warp 动作）。
        assert_eq!(
            sink.warps,
            Vec::<(u16, f32, f32)>::new(),
            "热键切换 preserve_cursor=true：被控端跳过落点定位（光标原地）"
        );
        assert!(outcome.actions.iter().any(|action| matches!(
            action,
            SessionAction::Notice(Notice::EnteredControl { target: 1 })
        )));
        // MouseMoveAbs 同样映射（FR-2.7）：全局屏 1 → 本机屏 0
        client.map_peer_screen(1, 0);
        let actions = client
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 0.25,
                    y: 0.75,
                },
            )))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::InjectInput(InputEvent::MouseMoveAbs {
                screen: 0,
                x: 0.25,
                y: 0.75,
            })],
            "MouseMoveAbs.screen 经映射表换算（实际 {actions:?}）"
        );
    }

    // —— 双向边缘回切（11-21 反馈 5，docs/03 §5） ——

    /// 2×1 布局 fixture（主控 [2;16] 屏 0 右邻从机屏 1，owner=[1;16]）：
    /// 边缘回切全组用例的判定图（环绕落点查面向屏 owner）。
    fn host_layout_2x1() -> crate::layout::ScreenLayout {
        crate::layout::ScreenLayout::new(vec![
            ScreenInfo {
                id: 0,
                owner: [2; 16],
                name: "host-main".into(),
                left: None,
                right: Some(1),
                top: None,
                bottom: None,
                scale: 1.0,
            },
            ScreenInfo {
                id: 1,
                owner: [1; 16],
                name: "slave-main".into(),
                left: Some(0),
                right: None,
                top: None,
                bottom: None,
                scale: 1.0,
            },
        ])
    }

    /// Host 接管（EdgeCrossing 切出）+ 同向武装的公共前置（含 2×1 布局）。
    ///
    /// `exit_edge` = **主控切出边**（EdgeCrossing 语义；11-22 起回切触发边 =
    /// 其对侧 = 受控屏面向主控的边）。
    fn host_controlling_with_outward(
        session: &mut Session<TestClock>,
        exit_edge: ScreenEdge,
        dx: f32,
        dy: f32,
    ) {
        session.set_layout(host_layout_2x1());
        session_takeover(session, exit_edge);
        // 免疫窗预热（11-22 慢速切入回弹修复；11-24 按时间计）：时钟越过
        // RETURN_IMMUNITY_MS 后才可触发。
        pass_immunity(session);
        session
            .step(SessionEvent::Local(LocalCmd::ForwardInput(
                InputEvent::MouseMoveRel { dx, dy },
            )))
            .unwrap();
    }

    /// 免疫窗预热（11-24，`edge_return_actions` 按时间计）：推进会话时钟越过
    /// [`RETURN_IMMUNITY_MS`]——接管后到达的极值回报自此可触发回切。直接推进
    /// 会话持有的 [`TestClock`] 句柄（子模块可见私有字段），调用方无需另持句柄。
    fn pass_immunity(session: &mut Session<TestClock>) {
        session.clock.advance(RETURN_IMMUNITY_MS);
    }

    /// Host 接管前置（无武装、无布局注入）：EdgeCrossing 切出建立控制方上下文。
    fn session_takeover(session: &mut Session<TestClock>, exit_edge: ScreenEdge) {
        session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: exit_edge,
                ratio: 0.5,
            }))
            .unwrap();
    }

    /// 热键/托盘切换接管前置（11-24，R1）：`Switch` → `SwitchAck{ok}` 两步接管
    /// （无布局注入；`[主控 | 工作机]` 布局由调用方先行 `set_layout`）。
    fn switch_takeover(session: &mut Session<TestClock>) {
        session
            .step(SessionEvent::Local(LocalCmd::Switch { target: 1 }))
            .unwrap();
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::SwitchAck {
                    target: 1,
                    ok: true,
                },
            )))
            .unwrap();
        assert_eq!(
            session.control_dir(),
            ControlDir::LocalControlsPeer,
            "Ack 确认后主控接管"
        );
    }

    /// AC1（11-24，R1）：热键/托盘切入后的回切触发边 = 受控屏**面向主控的边**
    /// （= 进入边），与边缘路径一致（docs/03 §5）。`[主控 | 工作机]` 布局经
    /// `Switch` + `SwitchAck` 接管：工作机**左缘**（面向主控）极值回报 + 同向移动
    /// → 回切，落主控右缘内侧 0.5%；**远离主控**的边（工作机右缘）极值回报不触发。
    #[test]
    fn switch_takeover_return_edge_faces_host() {
        // —— 面向主控的边（工作机左缘）：极值回报 + 朝主控方向移动 → 回切 ——
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host.set_layout(host_layout_2x1());
        switch_takeover(&mut host);
        // 免疫窗预热（11-24 按时间计）：接管后 800ms 内的回报不触发。
        pass_immunity(&mut host);
        // 朝主控方向（左）移动 → 同向武装。
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: -3.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 0.0,
                    y: 0.4,
                },
            )))
            .unwrap();
        assert_eq!(
            actions.len(),
            4,
            "面向主控边（工作机左缘）极值回报应触发回切（实际 {actions:?}）"
        );
        assert_eq!(actions[0], SessionAction::ReleaseAll);
        assert!(matches!(
            &actions[1],
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::LeaveScreen { .. }),
                ..
            }
        ));
        assert_eq!(
            actions[2],
            SessionAction::WarpPointer {
                screen: 0,
                x: 1.0 - EDGE_RETURN_INSET,
                y: 0.4
            },
            "落点 = 主控右缘内侧 EDGE_RETURN_INSET，ratio 保持"
        );
        assert_eq!(
            actions[3],
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (0.0, 0.0),
            })
        );
        assert_eq!(host.control_dir(), ControlDir::None, "控制关系已终结");

        // —— 远离主控的边（工作机右缘）：极值回报 + 同向移动 → 不触发 ——
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host.set_layout(host_layout_2x1());
        switch_takeover(&mut host);
        pass_immunity(&mut host);
        // 朝远离主控方向（右）移动 → 同向武装。
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 3.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.4,
                },
            )))
            .unwrap();
        assert!(
            actions.is_empty(),
            "远离主控的边（工作机右缘）极值回报不触发回切（实际 {actions:?}）"
        );
        assert_eq!(
            host.control_dir(),
            ControlDir::LocalControlsPeer,
            "控制关系保持"
        );
    }

    /// 面向主控边滑出触发回切：极值回报 + 同向武装 → ReleaseControl 同型释放 +
    /// 环绕落点 warp（主控左缘切出 → 受控屏右缘（面向主控）滑出 → 主控左缘内侧
    /// 0.005 落点，ratio 保持）+ 通知；控制方向复位。回报本身不注入。
    #[test]
    fn edge_return_triggers_release_and_wrap_landing() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
        assert_eq!(host.control_dir(), ControlDir::LocalControlsPeer);

        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.4,
                },
            )))
            .unwrap();
        assert_eq!(actions.len(), 4, "ReleaseAll + LeaveScreen + warp + 通知");
        assert_eq!(actions[0], SessionAction::ReleaseAll);
        assert!(matches!(
            &actions[1],
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::LeaveScreen { .. }),
                ..
            }
        ));
        assert_eq!(
            actions[2],
            SessionAction::WarpPointer {
                screen: 0,
                x: 0.005,
                y: 0.4
            },
            "环绕落点：主控面向屏（屏 0）左缘内缩 EDGE_RETURN_INSET，ratio 保持"
        );
        assert_eq!(
            actions[3],
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (0.0, 0.0),
            })
        );
        assert_eq!(host.control_dir(), ControlDir::None, "控制关系已终结");
    }

    /// 边缘驻留不触发（死区防误触）：极值回报但无同向移动（未武装）→ 仅消化；
    /// 随后同向移动到达 → 下一回报触发（「仍有同方向移动事件」语义）。
    #[test]
    fn edge_return_requires_outward_delta_since_last_report() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host.set_layout(host_layout_2x1());
        session_takeover(&mut host, ScreenEdge::Left);
        // 免疫窗预热（11-24 按时间计）：先越过免疫窗——本用例考察的是"未武装"
        // 这一条独立防线（窗内的极值回报本就因免疫窗不触发，分不清是谁挡的）。
        pass_immunity(&mut host);

        // 驻留极值（cursor 已在边缘但用户未再向外推）：仅消化，不注入不触发。
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(
            actions.is_empty(),
            "未武装的极值回报不触发（实际 {actions:?}）"
        );

        // 同向移动 → 武装 → 下一回报触发。
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 2.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert_eq!(actions.len(), 4, "武装后极值回报触发回切");
    }

    /// 非极值回报不触发（容差带外/内侧位置）：同向武装下的 0.97 回报仅消化
    /// 并复位武装——随后极值回报需再次武装（武装窗口 = 上一回报以来）。
    #[test]
    fn edge_return_requires_extreme_position() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);

        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 0.97,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(
            actions.is_empty(),
            "近边非极值不触发（对端容差带收敛契约外）"
        );

        // 武装已被上一回报复位：极值回报不触发；再武装后才触发。
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(actions.is_empty(), "武装随回报复位（极值但未再武装）");
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 1.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert_eq!(actions.len(), 4, "再武装 + 极值 → 触发");
    }

    /// 回切免疫窗按时间（11-24，AC6）：接管后 **799ms** 到达的极值回报不触发，
    /// **800ms** 起可触发；与回报条数无关（窗内到达多少条回报都不缩短窗口）。
    #[test]
    fn edge_return_immunity_window_is_time_based() {
        assert_eq!(RETURN_IMMUNITY_MS, 800, "免疫窗 = 接管后 800ms");
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host.set_layout(host_layout_2x1());
        session_takeover(&mut host, ScreenEdge::Left); // t=0 接管
        assert_eq!(host.clock.peek(), 0, "接管时刻 = 会话时钟零点");
        // 窗内灌 20 条非极值回报（超过原按条数计的 16 条窗）：窗口不随条数推进。
        for _ in 0..20 {
            host.step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 0.5,
                    y: 0.5,
                },
            )))
            .unwrap();
        }
        assert_eq!(
            host.clock.peek(),
            0,
            "回报不推进会话时钟（判定基准 = 回报到达时刻）"
        );

        // t=799：窗内（799 < RETURN_IMMUNITY_MS）——已同向武装的极值回报不触发。
        host.clock.advance(RETURN_IMMUNITY_MS - 1);
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 3.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(actions.is_empty(), "799ms 仍在免疫窗内（实际 {actions:?}）");
        assert_eq!(
            host.control_dir(),
            ControlDir::LocalControlsPeer,
            "控制关系保持"
        );

        // t=800：窗外——上条回报已复位武装，再武装后极值回报触发。
        host.clock.advance(1);
        assert_eq!(host.clock.peek(), RETURN_IMMUNITY_MS);
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 3.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert_eq!(actions.len(), 4, "800ms 起免疫窗失效，回切触发");
        assert_eq!(host.control_dir(), ControlDir::None, "控制关系已终结");
    }

    /// 远离主控的边滑出不触发（11-22 语义改判：回切只认面向主控的边；原
    /// "外侧边绕回"模型废弃——空间连续穿行才是用户直觉）：主控左缘切出 →
    /// 触发边 = 受控屏右缘；左缘（远离主控）极值回报无动作。
    #[test]
    fn edge_return_far_edge_never_triggers() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);

        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 0.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(
            actions.is_empty(),
            "远离主控的左缘极值不触发回切（实际 {actions:?}）"
        );
        assert_eq!(
            host.control_dir(),
            ControlDir::LocalControlsPeer,
            "控制关系保持"
        );
    }

    /// 纵向 2×1 布局 fixture（主控 [2;16] 屏 0 下邻从机屏 1，owner=[1;16]）：
    /// 水平外侧边回切用例的判定图。
    fn host_layout_vertical() -> crate::layout::ScreenLayout {
        crate::layout::ScreenLayout::new(vec![
            ScreenInfo {
                id: 0,
                owner: [2; 16],
                name: "host-main".into(),
                left: None,
                right: None,
                top: None,
                bottom: Some(1),
                scale: 1.0,
            },
            ScreenInfo {
                id: 1,
                owner: [1; 16],
                name: "slave-main".into(),
                left: None,
                right: None,
                top: Some(0),
                bottom: None,
                scale: 1.0,
            },
        ])
    }

    /// 水平外侧边接管前置：主控下缘切出 → 触发边 = 受控屏上缘（面向主控）。
    /// 含纵向布局、免疫窗预热与同向（上）武装。
    fn host_controlling_upward(session: &mut Session<TestClock>) {
        session.set_layout(host_layout_vertical());
        session_takeover(session, ScreenEdge::Bottom);
        pass_immunity(session);
        session
            .step(SessionEvent::Local(LocalCmd::ForwardInput(
                InputEvent::MouseMoveRel { dx: 0.0, dy: -2.5 },
            )))
            .unwrap();
    }

    /// 受控屏（id 1）的一条回报 → 动作序列（空 = 仅消化）。
    fn slave_report(session: &mut Session<TestClock>, x: f32, y: f32) -> Vec<SessionAction> {
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs { screen: 1, x, y },
            )))
            .unwrap()
    }

    /// 角落排除（11-22 第三轮，docs/03 §5；11-24 由"正交轴恰为端点"改为 5%
    /// 角落区）：外侧边极值命中且**正交轴**回报落在角落带 → 不回切（角落留给
    /// 系统热区——KDE 触发角专用，边缘=切换）；沿边坐标中段（角落区外）照常
    /// 触发。四个角点、以及**角落带内的非角点**（旧实现下会触发）各验一遍；
    /// 垂直与水平外侧边各一次。带边界见 `edge_return_corner_band_boundaries`。
    #[test]
    fn edge_return_corner_exclusion() {
        use crate::layout::CORNER_RATIO;
        let band_mid = CORNER_RATIO / 2.0;
        // (沿边坐标, 是否回切)：角点 → 否；带内非角点 → 否（11-24）；中段 → 是。
        for (along, returns) in [
            (0.0, false),
            (band_mid, false),
            (1.0 - band_mid, false),
            (1.0, false),
            (0.5, true),
        ] {
            // —— 垂直外侧边（主控左缘切出 → 触发边 = 受控屏右缘）：回报 (1.0, along) ——
            let mut host = host_session(TestClock::new());
            ready_running_host(&mut host);
            host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
            let actions = slave_report(&mut host, 1.0, along);
            assert_eq!(
                actions.len() == 4,
                returns,
                "垂直外侧边 沿边坐标 {along} 的回切判定（实际 {actions:?}）"
            );
            if returns {
                assert_eq!(
                    actions[2],
                    SessionAction::WarpPointer {
                        screen: 0,
                        x: EDGE_RETURN_INSET,
                        y: along
                    },
                    "落点 = 主控左缘内缩，ratio = 沿边坐标保持"
                );
                assert_eq!(host.control_dir(), ControlDir::None, "回切后控制关系终结");
            } else {
                assert_eq!(
                    host.control_dir(),
                    ControlDir::LocalControlsPeer,
                    "角落驻留不回切，控制关系保持"
                );
            }

            // —— 水平外侧边（主控下缘切出 → 触发边 = 受控屏上缘）：回报 (along, 0.0) ——
            let mut host = host_session(TestClock::new());
            ready_running_host(&mut host);
            host_controlling_upward(&mut host);
            let actions = slave_report(&mut host, along, 0.0);
            assert_eq!(
                actions.len() == 4,
                returns,
                "水平外侧边 沿边坐标 {along} 的回切判定（实际 {actions:?}）"
            );
            if returns {
                assert_eq!(
                    actions[2],
                    SessionAction::WarpPointer {
                        screen: 0,
                        x: along,
                        y: 1.0 - EDGE_RETURN_INSET
                    },
                    "落点 = 主控下缘内缩，ratio = 沿边坐标保持"
                );
            }
        }
    }

    /// 角落带边界（11-24，AC9）——回切侧**垂直与水平外侧边各验**：沿边坐标落在
    /// 角落带内（`CORNER_RATIO − 0.001`、`1 − CORNER_RATIO + 0.001`）的极值回报
    /// 不回切；带外（`CORNER_RATIO + 0.001`、`1 − CORNER_RATIO − 0.001`）照常
    /// 回切，落点 ratio = 沿边坐标原值。边界值相对常量书写，不测精确端点。
    #[test]
    fn edge_return_corner_band_boundaries() {
        use crate::layout::CORNER_RATIO;
        let (in_low, out_low) = (CORNER_RATIO - 0.001, CORNER_RATIO + 0.001);
        let (out_high, in_high) = (1.0 - CORNER_RATIO - 0.001, 1.0 - CORNER_RATIO + 0.001);
        for (along, returns) in [
            (in_low, false),
            (out_low, true),
            (out_high, true),
            (in_high, false),
        ] {
            // 垂直外侧边：回报 (1.0, along)。
            let mut host = host_session(TestClock::new());
            ready_running_host(&mut host);
            host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
            let actions = slave_report(&mut host, 1.0, along);
            assert_eq!(
                actions.len() == 4,
                returns,
                "垂直外侧边 沿边坐标 {along}（实际 {actions:?}）"
            );
            if returns {
                assert_eq!(
                    actions[2],
                    SessionAction::WarpPointer {
                        screen: 0,
                        x: EDGE_RETURN_INSET,
                        y: along
                    }
                );
            }
            // 水平外侧边：回报 (along, 0.0)。
            let mut host = host_session(TestClock::new());
            ready_running_host(&mut host);
            host_controlling_upward(&mut host);
            let actions = slave_report(&mut host, along, 0.0);
            assert_eq!(
                actions.len() == 4,
                returns,
                "水平外侧边 沿边坐标 {along}（实际 {actions:?}）"
            );
            if returns {
                assert_eq!(
                    actions[2],
                    SessionAction::WarpPointer {
                        screen: 0,
                        x: along,
                        y: 1.0 - EDGE_RETURN_INSET
                    }
                );
            }
        }
    }

    /// 按键门控（11-22 第三轮，docs/03 §5）：任意鼠标键按下（拖文件/拖滚动条）
    /// → 极值回报不回切（回报照常消化——免疫窗计时/武装复位语义不变）；
    /// 松开后再武装 → 恢复触发。热键回切（ReleaseControl）不经此判定面。
    #[test]
    fn edge_return_gated_while_mouse_button_down() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);

        // 左键按下（LocalCmd::MouseButtons 直通，无动作产出）。
        let actions = host
            .step(SessionEvent::Local(LocalCmd::MouseButtons {
                any_down: true,
            }))
            .unwrap();
        assert!(actions.is_empty(), "按键状态更新无动作产出");

        // 按键按下期间的极值回报：不回切（回报照常消化）。
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(actions.is_empty(), "鼠标键按下不回切（实际 {actions:?}）");
        assert_eq!(
            host.control_dir(),
            ControlDir::LocalControlsPeer,
            "控制关系保持"
        );

        // 松开（任意键全释放）→ 再武装（同向移动）→ 极值回报恢复触发。
        host.step(SessionEvent::Local(LocalCmd::MouseButtons {
            any_down: false,
        }))
        .unwrap();
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 2.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert_eq!(actions.len(), 4, "松开后恢复回切触发");
        assert_eq!(host.control_dir(), ControlDir::None, "控制关系已终结");
    }

    /// 按键门控全态跟踪（11-22 第三轮 Pause 语义交互）：暂停期 MouseButtons
    /// 照常更新（恢复后状态正确——暂停中按下的按键在恢复后仍门控）。
    #[test]
    fn mouse_button_tracking_persists_through_pause() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        // 暂停期按下按键：状态更新被接受（Running+Paused → Ignore，不拒帧）。
        host.step(SessionEvent::Local(LocalCmd::Pause)).unwrap();
        host.step(SessionEvent::Local(LocalCmd::MouseButtons {
            any_down: true,
        }))
        .unwrap();
        host.step(SessionEvent::Local(LocalCmd::Resume)).unwrap();
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
        // 恢复后按键状态保持按下：极值回报不回切。
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(
            actions.is_empty(),
            "暂停期按下的按键在恢复后仍门控（实际 {actions:?}）"
        );
    }

    /// 垂直对称（主控下缘切出 → 受控屏上缘（面向主控）滑出 → 主控下缘内侧
    /// 落点，沿边坐标 x = ratio 保持）。
    #[test]
    fn edge_return_vertical_wrap_landing() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        // 主控屏 0 下邻从机屏 1（B.top=0）：切出边 Bottom → 触发边 = 受控屏
        // 上缘（面向主控）。
        host.set_layout(crate::layout::ScreenLayout::new(vec![
            ScreenInfo {
                id: 0,
                owner: [2; 16],
                name: "host-main".into(),
                left: None,
                right: None,
                top: None,
                bottom: Some(1),
                scale: 1.0,
            },
            ScreenInfo {
                id: 1,
                owner: [1; 16],
                name: "slave-main".into(),
                left: None,
                right: None,
                top: Some(0),
                bottom: None,
                scale: 1.0,
            },
        ]));
        host.step(SessionEvent::Local(LocalCmd::EdgeCrossing {
            target: 1,
            edge: ScreenEdge::Bottom,
            ratio: 0.5,
        }))
        .unwrap();
        // 免疫窗预热（11-24 按时间计）。
        pass_immunity(&mut host);
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 0.0, dy: -2.5 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 0.3,
                    y: 0.0,
                },
            )))
            .unwrap();
        assert_eq!(
            actions[2],
            SessionAction::WarpPointer {
                screen: 0,
                x: 0.3,
                y: 0.995
            },
            "上缘滑出 → 主控下缘内缩落点，ratio = x 保持"
        );
    }

    /// 无布局回退（热点 Next 语义接管路径）：面向屏解析失败 → 落主屏 0
    /// 面向受控屏的那条边内侧内缩（语义与布局可用时一致，仅屏 id 兜底）。
    #[test]
    fn edge_return_landing_falls_back_to_main_screen_without_layout() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        session_takeover(&mut host, ScreenEdge::Left);
        // 免疫窗预热（11-24 按时间计）。
        pass_immunity(&mut host);
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 2.0, dy: 0.0 },
        )))
        .unwrap();
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.6,
                },
            )))
            .unwrap();
        assert_eq!(
            actions[2],
            SessionAction::WarpPointer {
                screen: 0,
                x: 0.005,
                y: 0.6
            },
            "无布局：主屏 0 + 左缘内缩落点（环绕方向不变）"
        );
    }

    /// 防御面：非有限回报坐标、不匹配屏的回报——均不触发；不匹配屏不消费
    /// 武装（受控屏回报才参与判定）。
    #[test]
    fn edge_return_rejects_non_finite_and_foreign_screen_reports() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);

        // 非有限坐标：消化不触发（武装随受控屏回报复位——防御面吞武装可接受）。
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: f32::NAN,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(actions.is_empty(), "非有限回报不触发");
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 2.0, dy: 0.0 },
        )))
        .unwrap();
        // 不匹配屏（id 5 非受控屏）：静默消化且武装保持 → 随后受控屏极值触发。
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 5,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert!(actions.is_empty(), "非受控屏回报不触发");
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 1.0,
                    y: 0.5,
                },
            )))
            .unwrap();
        assert_eq!(actions.len(), 4, "武装保持 + 受控屏极值 → 触发");
    }

    /// 被控方回报出站（docs/02 §4.2 kind 0x04 反向语义）：EnterScreen 建立
    /// 受控目标 → ReportControlledCursor 出站 MouseMoveAbs（屏 = 全局目标 id）；
    /// 非被控/切出后为无操作。
    #[test]
    fn controlled_cursor_report_outbound_gating() {
        let mut client = client_session(TestClock::new());
        ready_running(&mut client);
        // 非被控：无操作。
        assert!(
            client
                .step(SessionEvent::Local(LocalCmd::ReportControlledCursor {
                    x: 0.9,
                    y: 0.4
                }))
                .unwrap()
                .is_empty()
        );
        // 收 EnterScreen → 受控目标记录（屏幕 id = 全局 target，未映射原值）。
        client
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Left,
                    ratio: 0.5,
                    mods: Mods::default(),
                    preserve_cursor: false,
                },
            )))
            .unwrap();
        assert_eq!(client.controlled_target(), Some(1), "受控目标已记录");
        let seq_next = 1; // HelloBack(seq0)；EnterScreen 处理只产注入/warp/通知（不占 seq）
        let actions = client
            .step(SessionEvent::Local(LocalCmd::ReportControlledCursor {
                x: 0.9,
                y: 0.4,
            }))
            .unwrap();
        assert!(
            matches!(
                &actions[..],
                [SessionAction::Send {
                    seq,
                    msg: Outbound::Input(InputEvent::MouseMoveAbs { screen: 1, x: 0.9, y: 0.4 }),
                }] if *seq == seq_next
            ),
            "回报出站 MouseMoveAbs（全局屏 id，实际 {actions:?}）"
        );
        // 收 LeaveScreen → 受控目标清除 → 回报无操作。
        client
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::LeaveScreen {
                    mods: Mods::default(),
                },
            )))
            .unwrap();
        assert_eq!(client.controlled_target(), None);
        assert!(
            client
                .step(SessionEvent::Local(LocalCmd::ReportControlledCursor {
                    x: 0.9,
                    y: 0.4
                }))
                .unwrap()
                .is_empty(),
            "切出后回报无操作"
        );
    }

    /// 回报帧 wire 级回环（mac 冒烟同链路口径）：被控方回报出站帧经真实帧编码
    /// → 主控 `Dispatcher` 解码喂状态机 → 回切动作产出。证明 kind 0x04 反向
    /// 语义在帧层闭环（编码/解码/状态机三方契约一致）。
    #[test]
    fn edge_return_wire_roundtrip_via_dispatcher() {
        // —— 被控方：收 EnterScreen → 受控目标 → 回报出站（编码成帧）——
        let mut client = client_session(TestClock::new());
        ready_running(&mut client);
        client
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Left,
                    ratio: 0.5,
                    mods: Mods::default(),
                    preserve_cursor: false,
                },
            )))
            .unwrap();
        let actions = client
            .step(SessionEvent::Local(LocalCmd::ReportControlledCursor {
                x: 1.0,
                y: 0.4,
            }))
            .unwrap();
        let mut wire = Vec::new();
        let SessionAction::Send { seq, msg } = &actions[0] else {
            panic!("期望 Send，实际 {actions:?}");
        };
        msg.encode_frame_into(*seq, &mut wire).unwrap();

        // —— 主控：接管 + 同向武装（回切前置）→ Dispatcher 喂回报帧 ——
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
        let outcome = {
            let mut dispatcher = crate::route::Dispatcher::new(&mut host);
            dispatcher.feed(&wire)
        };
        assert_eq!(outcome.fatal, None);
        assert_eq!(
            outcome.actions.len(),
            4,
            "帧级回切：ReleaseAll + LeaveScreen + warp + 通知"
        );
        assert_eq!(outcome.actions[0], SessionAction::ReleaseAll);
        assert!(matches!(
            &outcome.actions[1],
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::LeaveScreen { .. }),
                ..
            }
        ));
        assert_eq!(
            outcome.actions[2],
            SessionAction::WarpPointer {
                screen: 0,
                x: 0.005,
                y: 0.4
            },
            "环绕落点随帧级路径产出"
        );
        assert_eq!(
            outcome.actions[3],
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (0.0, 0.0),
            }),
            "端点值回报（无越界 / 被控端不度量行程）：行程为 0"
        );
    }

    /// `beyond_edge`（11-24，AC10）：端点之外的值解出行程（左/上为负、右/下为
    /// 正且超出 1 的部分才是行程），`[0, 1]` 内（含端点值 0.0/1.0）恒 0。
    #[test]
    fn beyond_edge_extracts_travel_outside_unit_range() {
        assert_eq!(beyond_edge(-37.5), -37.5, "左/上边界：负值原样");
        assert_eq!(beyond_edge(1.25), 0.25, "右/下边界：超出 1 的部分");
        for inside in [0.0, 1.0, 0.5, 0.001, 0.999] {
            assert_eq!(beyond_edge(inside), 0.0, "{inside} 在 [0,1] 内无行程");
        }
        // 回报精度下的极小越界也是行程（不做容差收敛——收敛起被控端职责）。
        assert_eq!(beyond_edge(-0.001), -0.001);
        assert!((beyond_edge(1.001) - 0.001).abs() < 1e-6);
    }

    /// 回切行程补偿（11-24，AC10 / R8；docs/02 §4.2 反向回报取值范围扩展）：
    /// 端点之外的回报照常触发回切，越界幅值解出行程并随
    /// [`Notice::EdgeReturnedToHost`] 的 `overflow` 带出；**落点 warp 不含补偿**
    /// （仍为主控面向受控屏那条边的内侧内缩点，ratio = 沿边坐标——补偿由执行层
    /// 加到落点上）。四条外侧边各验一次；无越界的端点值回报行程为 0。
    #[test]
    fn edge_return_carries_reported_travel_in_notice() {
        // (主控切出边, 同向武装 delta, 回报 (x, y), 期望行程, 期望落点 (x, y))
        let cases = [
            // 受控屏右缘（面向主控）滑出：x 越过 1 → 行程 = x − 1。
            (
                ScreenEdge::Left,
                (3.0, 0.0),
                (1.25, 0.4),
                (0.25, 0.0),
                (EDGE_RETURN_INSET, 0.4),
            ),
            // 受控屏左缘滑出：x 越过 0 → 行程为负。
            (
                ScreenEdge::Right,
                (-3.0, 0.0),
                (-40.0, 0.6),
                (-40.0, 0.0),
                (1.0 - EDGE_RETURN_INSET, 0.6),
            ),
        ];
        for (exit_edge, (dx, dy), (x, y), travel, (landing_x, landing_y)) in cases {
            let mut host = host_session(TestClock::new());
            ready_running_host(&mut host);
            host_controlling_with_outward(&mut host, exit_edge, dx, dy);
            let actions = slave_report(&mut host, x, y);
            assert_eq!(actions.len(), 4, "越界回报照常触发回切（实际 {actions:?}）");
            assert_eq!(
                actions[2],
                SessionAction::WarpPointer {
                    screen: 0,
                    x: landing_x,
                    y: landing_y
                },
                "落点 warp 不含补偿"
            );
            assert_eq!(
                actions[3],
                SessionAction::Notice(Notice::EdgeReturnedToHost {
                    target: 1,
                    overflow: travel,
                }),
                "回报段行程随通知带出"
            );
        }

        // 水平外侧边（主控下缘切出 → 受控屏上缘）：y 越过 0 → 行程在 y 轴。
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_upward(&mut host);
        let actions = slave_report(&mut host, 0.3, -0.125);
        assert_eq!(
            actions[3],
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (0.0, -0.125),
            }),
            "受控屏上缘滑出：行程在 y 轴（x 轴无越界为 0）"
        );
        // 受控屏下缘（主控上缘切出）：y 越过 1。
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host.set_layout(host_layout_vertical());
        session_takeover(&mut host, ScreenEdge::Top);
        pass_immunity(&mut host);
        host.step(SessionEvent::Local(LocalCmd::ForwardInput(
            InputEvent::MouseMoveRel { dx: 0.0, dy: 2.5 },
        )))
        .unwrap();
        let actions = slave_report(&mut host, 0.3, 1.5);
        assert_eq!(
            actions.len(),
            4,
            "受控屏下缘越界回报触发回切（实际 {actions:?}）"
        );
        assert_eq!(
            actions[3],
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (0.0, 0.5),
            })
        );

        // 无越界（端点值回报——旧工作机 / macOS / X11 被控端）：行程为 0。
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
        let actions = slave_report(&mut host, 1.0, 0.4);
        assert_eq!(
            actions[3],
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (0.0, 0.0),
            })
        );

        // 触发条件不因越界值放宽：角落带内的越界回报（沿边坐标 y=0.02）不触发，
        // 控制关系保持——角落带仍只看沿边坐标。
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
        let actions = slave_report(&mut host, 1.25, 0.02);
        assert!(
            actions.is_empty(),
            "角落带内的越界回报不回切（实际 {actions:?}）"
        );
        assert_eq!(
            host.control_dir(),
            ControlDir::LocalControlsPeer,
            "控制关系保持"
        );
    }

    /// 越界回报经真实帧编解码仍解出行程（11-24，AC10）：解码只校验有限性，
    /// 端点之外的值原样通过——被控方回报 `x = 1.25` 的帧经 `Dispatcher` 喂入
    /// 主控状态机，通知携带 0.25 的行程（兼容性依据：旧主控忽略幅值、新主控对
    /// 旧工作机的端点值回报算出 0 行程）。
    #[test]
    fn edge_return_travel_survives_wire_roundtrip() {
        use xcontrol_proto::Message as _;
        let mut wire = Vec::new();
        InputEvent::MouseMoveAbs {
            screen: 1,
            x: 1.25,
            y: 0.4,
        }
        .encode_frame_into(7, &mut wire)
        .expect("越界回报帧可编码");

        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host_controlling_with_outward(&mut host, ScreenEdge::Left, 3.0, 0.0);
        let outcome = {
            let mut dispatcher = crate::route::Dispatcher::new(&mut host);
            dispatcher.feed(&wire)
        };
        assert_eq!(outcome.fatal, None, "越界值不构成协议违规");
        assert_eq!(outcome.actions.len(), 4, "帧级回切触发");
        assert_eq!(
            outcome.actions[3],
            SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (0.25, 0.0),
            }),
            "行程经 wire 往返保持"
        );
    }

    /// PublishLayout 门控矩阵：非 Host / 空布局 / 协商 minor < 1 → 无动作。
    #[test]
    fn publish_layout_gating() {
        // 非 Host：无动作
        let mut client = client_session(TestClock::new());
        ready_running(&mut client);
        client.set_layout(crate::layout::ScreenLayout::new(vec![ScreenInfo {
            id: 0,
            owner: [1; 16],
            name: "A".into(),
            left: None,
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        }]));
        assert!(
            client
                .step(SessionEvent::Local(LocalCmd::PublishLayout))
                .unwrap()
                .is_empty(),
            "从机不下发布局"
        );
        // Host + 布局 + minor < 1（对端 1.0）：无动作（wire 门控）
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        host.set_layout(crate::layout::ScreenLayout::new(vec![ScreenInfo {
            id: 0,
            owner: [2; 16],
            name: "A".into(),
            left: None,
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        }]));
        // ready_running_host 走 CURRENT 握手（1.1）；重开会话用 1.0 对端模拟：
        let mut host10 = host_session(TestClock::new());
        host10
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .unwrap();
        host10
            .step(SessionEvent::Frame(FramePayload::Ctrl(
                CtrlMsg::HelloBack {
                    proto: ProtoVer { major: 1, minor: 0 },
                    name: "client".into(),
                    instance_id: [1; 16],
                    caps: Caps::UNICODE,
                },
            )))
            .unwrap();
        host10.set_layout(crate::layout::ScreenLayout::new(vec![ScreenInfo {
            id: 0,
            owner: [2; 16],
            name: "A".into(),
            left: None,
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        }]));
        assert!(
            host10
                .step(SessionEvent::Local(LocalCmd::PublishLayout))
                .unwrap()
                .is_empty(),
            "协商 minor=0 不发 Layout（1.0 对端解码 1.1 wire 会违规断开）"
        );
        // Host + 布局 + 协商 1.1：出站 Layout（同 switch_closed_loop 用例）
        assert_eq!(
            host.step(SessionEvent::Local(LocalCmd::PublishLayout))
                .unwrap()
                .len(),
            1
        );
        // Host 空布局：无动作
        let mut host_empty = host_session(TestClock::new());
        ready_running_host(&mut host_empty);
        assert!(
            host_empty
                .step(SessionEvent::Local(LocalCmd::PublishLayout))
                .unwrap()
                .is_empty()
        );
    }

    /// SwitchAck ok=false / 被控中：不接管（闭环的否定分支）。
    #[test]
    fn switch_ack_negative_branches_do_not_take_over() {
        // ok=false：仅通知
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::SwitchAck {
                    target: 1,
                    ok: false,
                },
            )))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Notice(Notice::SwitchResult {
                target: 1,
                ok: false,
            })]
        );
        assert_eq!(host.control_dir(), ControlDir::None);
        // 被控中（PeerControlsLocal）：ok=true 也不接管
        host.step(SessionEvent::Frame(FramePayload::Input(
            InputEvent::EnterScreen {
                target: 0,
                edge: ScreenEdge::Left,
                ratio: 0.5,
                mods: Mods::default(),
                preserve_cursor: false,
            },
        )))
        .unwrap();
        assert_eq!(host.control_dir(), ControlDir::PeerControlsLocal);
        let actions = host
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::SwitchAck {
                    target: 1,
                    ok: true,
                },
            )))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Notice(Notice::SwitchResult {
                target: 1,
                ok: true,
            })]
        );
    }

    /// `LocalCmd::ReleaseControl`（FR-2.5 Home）：受控中 → ReleaseAll + LeaveScreen
    /// + 控制方向复位；无受控 → 无操作。
    #[test]
    fn release_control_releases_peer_and_resets_direction() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        // 无受控：无操作
        let actions = host
            .step(SessionEvent::Local(LocalCmd::ReleaseControl))
            .unwrap();
        assert!(actions.is_empty(), "无受控时 Home 无操作");

        // 切出建立控制方向（mods 快照随行）
        host.step(SessionEvent::Local(LocalCmd::LocalMods(Mods::LCTRL)))
            .unwrap();
        host.step(SessionEvent::Local(LocalCmd::EdgeCrossing {
            target: 1,
            edge: ScreenEdge::Right,
            ratio: 0.5,
        }))
        .unwrap();
        assert_eq!(host.control_dir(), ControlDir::LocalControlsPeer);
        let actions = host
            .step(SessionEvent::Local(LocalCmd::ReleaseControl))
            .unwrap();
        assert_eq!(
            actions,
            vec![
                SessionAction::ReleaseAll,
                SessionAction::Send {
                    seq: 3, // Hello(0) + LeaveScreen(1) + EnterScreen(2) 之后
                    msg: Outbound::Input(InputEvent::LeaveScreen { mods: Mods::LCTRL }),
                },
                // 11-20 回切落点：warp 回主屏内侧（防边缘回弹）。
                SessionAction::WarpPointer {
                    screen: 0,
                    x: 0.5,
                    y: 0.6
                },
            ]
        );
        assert_eq!(host.control_dir(), ControlDir::None);
        // 复位后周期 ModifierSync 静默（控制关系终止）
        let actions = host.step(SessionEvent::Timer).unwrap();
        assert!(
            !actions.iter().any(|action| matches!(
                action,
                SessionAction::Send {
                    msg: Outbound::Input(InputEvent::ModifierSync { .. }),
                    ..
                }
            )),
            "无受控方向不再外发周期快照"
        );
    }

    /// `LocalCmd::ForwardInput`：Running 未暂停 → 出站 Input 帧（seq 同源）；
    /// Paused 下被转移表拒绝（调用方门控的防御兜底）。
    #[test]
    fn forward_input_outbound_and_paused_reject() {
        let mut host = host_session(TestClock::new());
        ready_running_host(&mut host);
        let key = InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        };
        let actions = host
            .step(SessionEvent::Local(LocalCmd::ForwardInput(key.clone())))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Send {
                seq: 1,
                msg: Outbound::Input(key.clone()),
            }]
        );
        // 暂停后拒绝（UnexpectedEvent——协议违规形态，调用方断开前有门控拦截）
        host.step(SessionEvent::Local(LocalCmd::Pause)).unwrap();
        let error = host
            .step(SessionEvent::Local(LocalCmd::ForwardInput(key)))
            .unwrap_err();
        assert_eq!(
            error,
            StateError::UnexpectedEvent {
                state: "Running(Paused)",
                event: "Local(ForwardInput)",
            }
        );
    }

    // —— 布局与退出 ——

    /// Layout 帧更新缓存（Host 本地安装同样生效）+ LayoutReceived 通知 +
    /// map_peer_screen 映射注入/查询。
    #[test]
    fn layout_update() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        assert!(session.layout().is_empty());
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(CtrlMsg::Layout {
                screens: vec![ScreenInfo {
                    id: 1,
                    owner: [1; 16],
                    name: "B".into(),
                    left: Some(0),
                    right: None,
                    top: None,
                    bottom: None,
                    scale: 1.0,
                }],
            })))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Notice(Notice::LayoutReceived { screens: 1 })],
            "布局缓存后通知（执行层据以建映射表）"
        );
        assert_eq!(session.layout().len(), 1);
        assert_eq!(session.layout().neighbor(1, ScreenEdge::Left), Some(0));
        assert_eq!(session.layout().ids_owned_by(&[1; 16]), vec![1]);
        // Host 本地安装
        let mut host = host_session(TestClock::new());
        host.set_layout(crate::layout::ScreenLayout::new(vec![ScreenInfo {
            id: 0,
            owner: [2; 16],
            name: "A".into(),
            left: None,
            right: Some(1),
            top: None,
            bottom: None,
            scale: 1.0,
        }]));
        assert_eq!(host.layout().neighbor(0, ScreenEdge::Right), Some(1));
        // 映射注入：同 id 覆盖、未命中直通；重灌前置清空（布局更新 = 替换语义）
        host.map_peer_screen(1, 3);
        host.map_peer_screen(1, 4);
        assert_eq!(host.screen_map(), &[(1, 4)], "同 id 重复注入以最新为准");
        host.clear_screen_map();
        assert!(host.screen_map().is_empty(), "重灌前置清空（docs/02 §4.2）");
    }

    /// 对端 Error 帧与本地 Quit 断开路径。
    #[test]
    fn peer_error_and_local_quit() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Ctrl(CtrlMsg::Error {
                code: ErrCode::SERVER_FULL,
                msg: "full".into(),
            })))
            .unwrap();
        assert_eq!(
            actions,
            vec![
                SessionAction::ReleaseAll,
                SessionAction::Notice(Notice::Disconnected(DisconnectReason::PeerError {
                    code: ErrCode::SERVER_FULL,
                    msg: "full".into(),
                })),
            ]
        );
        assert_eq!(
            session.state(),
            &SessionState::Disconnected(DisconnectReason::PeerError {
                code: ErrCode::SERVER_FULL,
                msg: "full".into(),
            })
        );
        // Local Quit
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        let actions = session.step(SessionEvent::Local(LocalCmd::Quit)).unwrap();
        assert_eq!(
            actions,
            vec![
                SessionAction::ReleaseAll,
                SessionAction::Notice(Notice::Disconnected(DisconnectReason::LocalQuit)),
            ]
        );
        assert_eq!(
            session.state(),
            &SessionState::Disconnected(DisconnectReason::LocalQuit)
        );
    }

    /// 断开即释放：受控中已注入的修饰键在任意断开路径（此处 LocalQuit）
    /// 都被 ReleaseAll 兜底 + 基准清空（语义裁定：防卡键不变量的断开分支）。
    #[test]
    fn disconnect_releases_injected_keys() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        session
            .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
                code: KEY_LEFT_SHIFT,
                state: KeyState::Down,
            })))
            .unwrap();
        assert_eq!(session.injected_mods(), Mods::LSHIFT);
        let actions = session.step(SessionEvent::Local(LocalCmd::Quit)).unwrap();
        assert_eq!(actions[0], SessionAction::ReleaseAll);
        assert_eq!(
            session.injected_mods(),
            Mods::default(),
            "断开后注入基准清空"
        );
    }

    /// 出站 seq 单调递增（HelloBack/Ping/ModifierSync 共用计数器）。
    #[test]
    fn outbound_seq_monotonic() {
        let clock = TestClock::new();
        let mut session = client_session(clock.clone());
        let mut seqs = Vec::new();
        assert!(
            session
                .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
                .unwrap()
                .is_empty()
        );
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(hello_msg(1, 0))))
            .unwrap()
            .iter()
            .for_each(|a| collect_seq(a, &mut seqs));
        clock.advance(3000);
        session
            .step(SessionEvent::Timer)
            .unwrap()
            .iter()
            .for_each(|a| collect_seq(a, &mut seqs));
        clock.advance(3000);
        session
            .step(SessionEvent::Timer)
            .unwrap()
            .iter()
            .for_each(|a| collect_seq(a, &mut seqs));
        assert_eq!(
            seqs,
            vec![0, 1, 2],
            "HelloBack + 两次 Ping（None 方向无 ModifierSync）"
        );
        // Host 侧从 Hello 起算
        let mut host = host_session(TestClock::new());
        let mut seqs = Vec::new();
        host.step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .unwrap()
            .iter()
            .for_each(|a| collect_seq(a, &mut seqs));
        assert_eq!(seqs, vec![0]);
    }

    // —— ModifierSync 单向语义回归（P1：幻按键注入修复） ——

    /// 回归①：无人在控（ControlDir::None）时本地 mods 不外发周期快照；
    /// 收到对端 ModifierSync 宽容忽略（不注入、不断开）。
    #[test]
    fn modifier_sync_silent_without_control_direction() {
        let clock = TestClock::new();
        let mut session = client_session(clock.clone());
        ready_running(&mut session);
        assert_eq!(session.control_dir(), ControlDir::None);
        session
            .step(SessionEvent::Local(LocalCmd::LocalMods(Mods::LSHIFT)))
            .unwrap();

        // 1s tick：无 ModifierSync（修复前此处会外发并注入对端）
        clock.advance(1000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert!(
            actions.iter().all(|a| !matches!(
                a,
                SessionAction::Send {
                    msg: Outbound::Input(InputEvent::ModifierSync { .. }),
                    ..
                }
            )),
            "None 方向不得外发周期快照：{actions:?}"
        );
        // 3s tick：仅 Ping
        clock.advance(2000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Ping { .. }),
                ..
            }
        ));

        // 收到对端 sync：None 方向 → 忽略（不注入、不违规断开）
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::ModifierSync { mods: Mods::LSHIFT },
            )))
            .unwrap();
        assert!(actions.is_empty(), "None 方向收到 sync 必须忽略");
        assert_eq!(
            session.injected_mods(),
            Mods::default(),
            "不得产生幻按键注入"
        );
        assert_eq!(session.state(), &SessionState::Running { paused: false });
    }

    /// 回归②：被控方向（PeerControlsLocal）收到 sync 才产生合成注入。
    #[test]
    fn modifier_sync_applies_only_when_peer_controls_local() {
        let mut session = client_session(TestClock::new());
        ready_running(&mut session);
        // 受控开始：EnterScreen 建立方向 + 无条件对齐
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Right,
                    ratio: 0.5,
                    mods: Mods::LCTRL,
                    preserve_cursor: false,
                },
            )))
            .unwrap();
        assert_eq!(session.control_dir(), ControlDir::PeerControlsLocal);
        assert_eq!(session.injected_mods(), Mods::LCTRL);
        // 对端新增 LMETA：本地补按下
        session
            .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
                code: KEY_LEFT_META,
                state: KeyState::Down,
            })))
            .unwrap();
        // 周期校验：对端快照 LCTRL|LMETA → 已一致，无动作
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::ModifierSync {
                    mods: Mods::LCTRL.union(Mods::LMETA),
                },
            )))
            .unwrap();
        assert!(actions.is_empty());
        assert_eq!(session.injected_mods(), Mods::LCTRL.union(Mods::LMETA));
        // 对端快照回落 LCTRL → 合成 LMETA Up（被控方才应用）
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::ModifierSync { mods: Mods::LCTRL },
            )))
            .unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::InjectInput(InputEvent::Key {
                code: KEY_LEFT_META,
                state: KeyState::Up,
            })]
        );
    }

    /// 回归③：本机控制对端（LocalControlsPeer，经 EdgeCrossing 建立）时，
    /// 本地 mods 变化 → 1s tick 产出 ModifierSync；LeaveScreen 复位后停止。
    #[test]
    fn modifier_sync_sent_only_when_local_controls_peer() {
        let clock = TestClock::new();
        let mut session = host_session(clock.clone());
        ready_running_host(&mut session);
        session
            .step(SessionEvent::Local(LocalCmd::LocalMods(Mods::LCTRL)))
            .unwrap();
        // t=1000：None 方向 → 静默
        clock.advance(1000);
        assert!(session.step(SessionEvent::Timer).unwrap().is_empty());

        // 本机切出 → LocalControlsPeer → 周期快照开启
        session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            }))
            .unwrap();
        assert_eq!(session.control_dir(), ControlDir::LocalControlsPeer);
        // t=2000：距上次基准（t=0）2000 ≥ 1000 → 外发
        clock.advance(1000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert_eq!(
            actions,
            vec![SessionAction::Send {
                seq: 3, // Hello=0, LeaveScreen=1, EnterScreen=2
                msg: Outbound::Input(InputEvent::ModifierSync { mods: Mods::LCTRL }),
            }]
        );

        // 对端切回（LeaveScreen）→ 方向复位 → 周期快照停止
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::LeaveScreen {
                    mods: Mods::default(),
                },
            )))
            .unwrap();
        assert_eq!(session.control_dir(), ControlDir::None);
        clock.advance(1000);
        let actions = session.step(SessionEvent::Timer).unwrap();
        assert!(
            actions.iter().all(|a| !matches!(
                a,
                SessionAction::Send {
                    msg: Outbound::Input(InputEvent::ModifierSync { .. }),
                    ..
                }
            )),
            "方向复位后不得再外发周期快照：{actions:?}"
        );
    }
}
