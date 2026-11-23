//! `hotkey_rt` — 热键路由与接线（FR-2.5，docs/03 §4.6）。
//!
//! # 职责切分
//!
//! - **路由（纯逻辑，本模块默认区）**：[`route_host`] / [`route_slave`] 把
//!   [`HotkeyId`] 翻译为会话层动作（[`HotkeyAction`]）——无 I/O、无平台依赖、
//!   `cargo test` 全量可测。门控语义与边缘切换门控同型：仅 Running 未暂停、
//!   未被控时允许切换类动作；Pause 在 Running 内 toggle（paused 态按压 = 恢复）。
//! - **切换语义（M4 裁定，M5 已闭环）**：
//!   - **Next/Prev** → `LocalCmd::Switch{target}` = 出站 `SwitchRequest`
//!     （docs/02 §4.2：主控→目标从机）→ 从机 `ReleaseAll` + `SwitchAck` →
//!     主控收 ok 即接管（core session M5：`ReleaseAll` + `LeaveScreen` +
//!     `EnterScreen{target, edge, ratio}`，进入边从布局图推导——docs/03 §5
//!     热键闭环）。**target = 布局装配的从机屏全局 id**（`LayoutPlan::
//!     slave_targets`，session_rt 注入 `route_host` 的 `peers` 参数）。
//!   - **Home** → `LocalCmd::ReleaseControl`（M4 新增 core 命令）：释放当前
//!     受控从机（ReleaseAll + LeaveScreen + 控制方向复位）——完整闭环动作。
//!   - **Pause** → `LocalCmd::Pause/Resume` toggle（FR-2.8 既有语义）。
//!   - 目标序（M5）：协商完成（HandshakeComplete）时由布局装配产出——空
//!     config `[layout]` 也有默认拓扑（单从机屏锚主控主屏右侧，M4 语义延续），
//!     热键 Next 开箱即用；空表仅出现在未协商态（门控本就拒绝切换）。
//!     托盘「直接切换」的下标经 `layout_rt::resolve_slave_target` 解析到
//!     全局 id（越界回退原值 = 1.0 语义兼容）。
//! - **从机侧路由**：Pause toggle 可用（两端同义）；Next/Prev/Home 无信令路径
//!   （`SwitchRequest` 主控→从机单向，docs/02 §4.2）——吞键不注入 + 日志留痕
//!   （FR-2.5 "不注入本机 OS"语义；反向信令协议扩展属 M5 评估）。
//! - **检测路径（平台矩阵，docs/05 §5 优先级的 M4 落点）**：
//!
//! | 端 | mac | win | x11 |
//! | --- | --- | --- | --- |
//! | 主控 | 捕获流识别（`CaptureSource` + core `HotkeyFilter`；PRD 裁定不采用
//!   RegisterEventHotKey） | `RegisterHotKey`（消息窗口）+ 捕获流过滤双保险 | `grab_key`（被动抓取） |
//! | 从机（收到的流） | 注入前过滤（[`HotkeyFilterSink`]，全平台统一） | 同左 | 同左 |
//! | 从机（本机物理热键） | M4 无注册路径（移交 M5） | `RegisterHotKey` | `grab_key` |
//!
//! - **捕获转发链（FR-2.x 主控侧，M4 接通）**：`CaptureSource` 产出
//!   `InputEvent` 流 → core `HotkeyFilter`（命中 → 路由 + 吞键；未命中 →
//!   `LocalCmd::ForwardInput` 经会话状态机出站）。转发门控
//!   [`capture_forward_allowed`]：仅 Running 未暂停**且** `LocalControlsPeer`
//!   （无人在控时不转发本机输入）。win/x11 主控的捕获转发接线移交 M5
//!   （M4 win/x11 热键走 OS 注册路径，无需捕获链）。
//! - **从机注入前过滤**：[`HotkeyFilterSink`] 包装 `MappingSink`（docs/03
//!   §4.1 流水线：热键过滤在映射之前），命中 → 事件不注入 + [`HotkeyId`]
//!   投通道（会话循环路由）；`release_all` 同步重置过滤器（防卡键对齐）。
//!
//! # 与 session_rt 的分工
//!
//! 本模块只提供纯决策与包装件；select 循环编排（capture poll ticker、热键源
//! drain、通道消费）在 session_rt（编排决策 + 状态机访问权集中，与 edge 接线
//! 同构）。

use xcontrol_core::hotkey::{FilterOutcome, HotkeyFilter, HotkeyId, HotkeySet};
use xcontrol_core::session::{ControlDir, SessionState};
use xcontrol_core::sink::{InputSink, SinkError};
use xcontrol_proto::input::{InputEvent, ScreenId};

/// M4 在线从机序（保留作测试 fixture：单会话单从机，目标 = 从机主屏 0）。
/// 生产路径的切换目标序自 M5 起由布局装配产出（`LayoutPlan::slave_targets`，
/// 全局 ScreenId 空间，docs/02 §4.2）——session_rt 不再读本常量。
pub const ONLINE_PEERS_M4: [ScreenId; 1] = [0];

/// 会话输入抽头轮询周期（捕获流/平台热键源 drain 共用节拍）：10ms = 100Hz，
/// 捕获转发链的延迟上界（鼠标移动的用户可感知阈值量级；mac CaptureSource
/// 是非阻塞 `poll`，热键源 drain 同为非阻塞）。
pub const INPUT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// 热键路由决策（会话循环执行的动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    /// 切到在线从机（出站 `SwitchRequest{target}`；M4 信令握手，模块文档）。
    Switch {
        /// 目标屏（M4 = 从机主屏 0）。
        target: ScreenId,
    },
    /// 回主控：释放当前受控从机（`LocalCmd::ReleaseControl`）。
    ReleaseControl,
    /// 暂停/恢复 toggle（目标态；FR-2.8）。
    SetPaused {
        /// 目标态（true = 暂停）。
        paused: bool,
    },
    /// 无操作（原因随日志留痕；门控拒绝/无信令路径/无在线从机）。
    None(&'static str),
}

/// 切换类动作门控（与 [`crate::session_rt::edge_hit_allowed`] 同语义）：
/// 仅 Running 未暂停、未被控。热键路由与托盘「直接切换」（FR-2.5 子菜单）
/// 共用同一门控——切换语义单一出处。
pub(crate) fn switch_allowed(state: &SessionState, dir: ControlDir) -> bool {
    matches!(state, SessionState::Running { paused: false }) && dir != ControlDir::PeerControlsLocal
}

/// 主控侧路由（FR-2.5 四动作；M4 单从机环语义见模块文档）。
#[must_use]
pub fn route_host(
    id: HotkeyId,
    state: &SessionState,
    control_dir: ControlDir,
    peers: &[ScreenId],
) -> HotkeyAction {
    match id {
        // Next：受控中 → 环绕回主控（N=1：受控的"下一台"即主控自己）；
        // 无受控 → 第一台在线从机。
        HotkeyId::Next => {
            if !switch_allowed(state, control_dir) {
                return HotkeyAction::None("门控拒绝（未运行/暂停/被控中）");
            }
            if control_dir == ControlDir::LocalControlsPeer {
                HotkeyAction::ReleaseControl
            } else if let Some(first) = peers.first() {
                HotkeyAction::Switch { target: *first }
            } else {
                HotkeyAction::None("无在线从机")
            }
        }
        // Prev：受控中 → 环绕回主控；无受控 → 无"上一台"（主控即环起点）。
        HotkeyId::Prev => {
            if !switch_allowed(state, control_dir) {
                return HotkeyAction::None("门控拒绝（未运行/暂停/被控中）");
            }
            if control_dir == ControlDir::LocalControlsPeer {
                HotkeyAction::ReleaseControl
            } else {
                HotkeyAction::None("无受控从机（Prev 环起点为主控）")
            }
        }
        // Home：主控侧自切——仅受控中生效。
        HotkeyId::BackToHost => {
            if !switch_allowed(state, control_dir) {
                return HotkeyAction::None("门控拒绝（未运行/暂停/被控中）");
            }
            if control_dir == ControlDir::LocalControlsPeer {
                HotkeyAction::ReleaseControl
            } else {
                HotkeyAction::None("当前无受控从机")
            }
        }
        // Pause：Running 内 toggle（paused 态按压 = 恢复）。
        HotkeyId::Pause => match state {
            SessionState::Running { paused } => HotkeyAction::SetPaused { paused: !paused },
            _ => HotkeyAction::None("非 Running 状态无暂停语义"),
        },
    }
}

/// 从机侧路由（吞键不注入后的本地动作；信令路径限制见模块文档）。
#[must_use]
pub fn route_slave(id: HotkeyId, state: &SessionState) -> HotkeyAction {
    match id {
        // Pause toggle 两端同义（FR-2.8）。
        HotkeyId::Pause => match state {
            SessionState::Running { paused } => HotkeyAction::SetPaused { paused: !paused },
            _ => HotkeyAction::None("非 Running 状态无暂停语义"),
        },
        // Next/Prev/Home：SwitchRequest 主控→从机单向（docs/02 §4.2），
        // 从机无反向信令路径——吞键语义已达成（不注入本机 OS），动作留痕。
        HotkeyId::Next | HotkeyId::Prev | HotkeyId::BackToHost => HotkeyAction::None(
            "从机侧无对端切换信令路径（SwitchRequest 主控→从机单向；吞键不注入）",
        ),
    }
}

/// 主控捕获转发门控：仅 Running 未暂停**且**本机控制对端（无人在控时不转发
/// 本机输入——转发了也是把主控本地打字注入从机）。
#[must_use]
pub fn capture_forward_allowed(state: &SessionState, control_dir: ControlDir) -> bool {
    matches!(state, SessionState::Running { paused: false })
        && control_dir == ControlDir::LocalControlsPeer
}

// ============================================================
// 从机注入前过滤 sink（docs/03 §4.1 流水线：热键在映射之前）
// ============================================================

/// 热键过滤 sink：包装内层注入 sink（`MappingSink`），命中热键的事件
/// **不注入本机 OS**（FR-2.5 从机语义）并投 `HotkeyId` 通道（会话循环路由）。
///
/// 会话重置（ReleaseAll/切出/暂停/断开）经 [`InputSink::release_all`] 传导：
/// 过滤器修饰键跟踪与吞尾同步清零（与 `MappingSink::release_all` 的引擎
/// 重置同构——运行态与"已注入 OS 的键"一一对应）。
pub struct HotkeyFilterSink<S> {
    filter: HotkeyFilter,
    inner: S,
    hits: tokio::sync::mpsc::UnboundedSender<HotkeyId>,
}

impl<S> HotkeyFilterSink<S> {
    /// 包装内层 sink（热键集合 + 命中通道）。
    pub fn new(
        set: HotkeySet,
        inner: S,
        hits: tokio::sync::mpsc::UnboundedSender<HotkeyId>,
    ) -> Self {
        Self {
            filter: HotkeyFilter::new(set),
            inner,
            hits,
        }
    }

    /// 热替换热键匹配表（docs/06 §4.1 热键行即时生效——会话循环在
    /// `RuntimeConfigUpdate` 消费点调用；语义见 [`HotkeyFilter::set_hotkeys`]）。
    pub fn set_hotkeys(&mut self, set: HotkeySet) {
        self.filter.set_hotkeys(set);
    }

    /// 内层 sink 的可变借用（映射规则热替换经此到达 [`MappingSink`]）。
    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<S: InputSink> InputSink for HotkeyFilterSink<S> {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        match self.filter.feed(event) {
            // 未命中：照常进映射引擎（docs/03 §4.1 流水线顺序）。
            FilterOutcome::Forward => self.inner.inject(event),
            // 命中：吞（不注入本机 OS）+ 投路由通道。投递失败仅发生在通道
            // 关闭（会话退出中）——吞键语义不受影响，静默降级。
            FilterOutcome::Hit(id) => {
                let _ = self.hits.send(id);
                Ok(())
            }
            // 命中键的尾巴（Up/重复 Down）：吞，无路由。
            FilterOutcome::HitTail => Ok(()),
        }
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        self.inner.warp(screen, x, y)
    }

    fn release_all(&mut self) -> Result<(), SinkError> {
        self.filter.reset();
        self.inner.release_all()
    }

    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        // 只读查询：热键过滤只作用于注入事件流（"命中吞 + 路由"），对光标
        // 位置语义无涉——直通内层（11-22 被控光标采样走 sink 查询的链条面）。
        self.inner.cursor_normalized()
    }
}

// ============================================================
// 平台热键源构建（feature 门控；会话循环 drain）
// ============================================================

/// 主控捕获源构建（macos：`CaptureSource::start`，PRD 裁定的捕获流识别路径）。
///
/// 启动失败告警降级 `None`（输入管线不因捕获不可用中断——error-handling.md
/// 规则 4；辅助功能未授权时系统静默无事件，docs/05 §1，可用性检测属
/// `check_accessibility` 的 UI 引导域）。
#[must_use]
#[cfg(feature = "macos")]
pub fn host_capture_source() -> Option<xcontrol_input::CaptureSource> {
    match xcontrol_input::CaptureSource::start() {
        Ok(source) => Some(source),
        Err(error) => {
            tracing::warn!(target: "app::hotkey", %error,
                "主控捕获源启动失败（热键识别与捕获转发降级停用）");
            None
        }
    }
}

/// 非 macos 平台的编译桩（win/x11 主控热键走 OS 注册路径，无需捕获链）。
#[must_use]
#[cfg(not(feature = "macos"))]
pub fn host_capture_source() -> Option<std::convert::Infallible> {
    None
}

/// 平台热键注册源聚合（win `RegisterHotKey` / x11 `grab_key`；两端同型注册）。
///
/// mac 无独立注册源（主控识别经捕获流，[`HotkeyFilter`] 在 session_rt）——
/// 本结构在 mac 上为空壳、`drain` 恒产出空。注册失败逐源告警降级（热键停用
/// 不中断会话，error-handling.md 规则 4）。
#[derive(Debug, Default)]
pub struct PlatformHotkeySource {
    #[cfg(feature = "windows")]
    win: Option<xcontrol_input::WinHotkeySource>,
    #[cfg(feature = "x11")]
    x11: Option<xcontrol_input::X11HotkeySource>,
}

impl PlatformHotkeySource {
    /// 按编译期启用的平台注册热键（docs/05 §2/§3 热键行）。
    #[must_use]
    pub fn register(set: &HotkeySet) -> Self {
        // 无注册源平台（mac 等）：参数保留接口形面一致，显式置未用。
        #[cfg(not(any(feature = "windows", feature = "x11")))]
        let _ = set;
        Self {
            #[cfg(feature = "windows")]
            win: Self::register_win(set),
            #[cfg(feature = "x11")]
            x11: Self::register_x11(set),
        }
    }

    /// 热重建注册（docs/06 §4.1 热键行即时生效的 win/x11 路径）：**先 Drop 旧
    /// 源**（win 线程内 UnregisterHotKey×N + 摘共享事件桥 / x11 解除 grab），
    /// **再按新集合重注册**——OS 注册语义不同于被动捕获（mac），必须重建而非
    /// 改表；且注册面是进程/服务器级独占，新旧源并存窗口内未变更的组合会让
    /// 新注册失败（win：`RegisterHotKey` 跨线程重复注册直接拒绝并触发新源的
    /// fail-loud 全量回滚；x11：旧连接仍持有 grab，新连接同组合 `XGrabKey`
    /// 产 BadAccess 同样全量回滚），win 还会因旧源 Drop 收尾清 `HOTKEY_SHARED`
    /// 静态桥而吞掉新源的全部命中事件——「先注销、后注册」顺序是正确性前提。
    ///
    /// 重注册失败逐源告警降级为无平台注册（热键停用不中断会话，error-handling
    /// .md 规则 4）；捕获流/注入前过滤路径（core `HotkeyFilter`）仍按新表生效。
    pub fn update(&mut self, set: &HotkeySet) {
        // 先 Drop 旧源（`None` 覆写即触发 `Drop` = 全量注销 + 摘桥）。
        #[cfg(feature = "windows")]
        {
            self.win = None;
            self.win = Self::register_win(set);
        }
        #[cfg(feature = "x11")]
        {
            self.x11 = None;
            self.x11 = Self::register_x11(set);
        }
        #[cfg(not(any(feature = "windows", feature = "x11")))]
        {
            let _ = set;
        }
    }

    /// win RegisterHotKey 注册（失败告警降级 `None`）。
    #[cfg(feature = "windows")]
    fn register_win(set: &HotkeySet) -> Option<xcontrol_input::WinHotkeySource> {
        match xcontrol_input::WinHotkeySource::register(set) {
            Ok(source) => Some(source),
            Err(error) => {
                tracing::warn!(target: "app::hotkey", %error,
                    "RegisterHotKey 注册失败（热键停用；组合被占用或主键无 VK 映射）");
                None
            }
        }
    }

    /// x11 grab_key 注册（失败告警降级 `None`）。
    #[cfg(feature = "x11")]
    fn register_x11(set: &HotkeySet) -> Option<xcontrol_input::X11HotkeySource> {
        match xcontrol_input::X11HotkeySource::register(set) {
            Ok(source) => Some(source),
            Err(error) => {
                tracing::warn!(target: "app::hotkey", %error,
                    "grab_key 注册失败（热键停用；组合被他客户端持有或主键不可达）");
                None
            }
        }
    }

    /// 非阻塞拉取自上次调用以来积累的命中（无事件时 `out` 不变）。
    pub fn drain(&mut self, out: &mut Vec<HotkeyId>) {
        #[cfg(feature = "windows")]
        if let Some(source) = self.win.as_mut() {
            source.drain(out);
        }
        #[cfg(feature = "x11")]
        if let Some(source) = self.x11.as_mut() {
            source.drain(out);
        }
        #[cfg(not(any(feature = "windows", feature = "x11")))]
        {
            let _ = out;
        }
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_config::HotkeysConfig;
    use xcontrol_core::hotkey::HotkeySet;
    use xcontrol_core::mock::RecordingInputSink;
    use xcontrol_proto::input::{KeyState, ScreenEdge};
    use xcontrol_proto::xkey::{KEY_A, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_P, KEY_RIGHT};

    use super::*;

    fn running(paused: bool) -> SessionState {
        SessionState::Running { paused }
    }

    /// 主控路由真值表（M4 单从机环语义）。
    #[test]
    fn host_routing_truth_table() {
        let peers = ONLINE_PEERS_M4;
        // Next：无受控 → 第一台
        assert_eq!(
            route_host(HotkeyId::Next, &running(false), ControlDir::None, &peers),
            HotkeyAction::Switch { target: 0 }
        );
        // Next：受控中 → 环绕回主控
        assert_eq!(
            route_host(
                HotkeyId::Next,
                &running(false),
                ControlDir::LocalControlsPeer,
                &peers
            ),
            HotkeyAction::ReleaseControl
        );
        // Prev：无受控 → 无上一台
        assert_eq!(
            route_host(HotkeyId::Prev, &running(false), ControlDir::None, &peers),
            HotkeyAction::None("无受控从机（Prev 环起点为主控）")
        );
        // Prev：受控中 → 回主控
        assert_eq!(
            route_host(
                HotkeyId::Prev,
                &running(false),
                ControlDir::LocalControlsPeer,
                &peers
            ),
            HotkeyAction::ReleaseControl
        );
        // Home：无受控 → 无操作；受控中 → 释放
        assert_eq!(
            route_host(
                HotkeyId::BackToHost,
                &running(false),
                ControlDir::None,
                &peers
            ),
            HotkeyAction::None("当前无受控从机")
        );
        assert_eq!(
            route_host(
                HotkeyId::BackToHost,
                &running(false),
                ControlDir::LocalControlsPeer,
                &peers
            ),
            HotkeyAction::ReleaseControl
        );
        // Pause：toggle 两态
        assert_eq!(
            route_host(HotkeyId::Pause, &running(false), ControlDir::None, &peers),
            HotkeyAction::SetPaused { paused: true }
        );
        assert_eq!(
            route_host(HotkeyId::Pause, &running(true), ControlDir::None, &peers),
            HotkeyAction::SetPaused { paused: false }
        );
    }

    /// 门控矩阵：非 Running / 共享暂停 / 被控中拒绝切换类动作；Pause 不受门控
    /// （Running 内任意态可 toggle，FR-2.8 共享暂停的退出路径）。
    #[test]
    fn host_gate_matrix() {
        let peers = ONLINE_PEERS_M4;
        let blocked_states = [
            SessionState::Idle,
            SessionState::Handshaking,
            running(true),
            SessionState::Disconnected(xcontrol_core::session::DisconnectReason::LocalQuit),
        ];
        for state in &blocked_states {
            for id in [HotkeyId::Next, HotkeyId::Prev, HotkeyId::BackToHost] {
                assert!(
                    matches!(
                        route_host(id, state, ControlDir::None, &peers),
                        HotkeyAction::None(_)
                    ),
                    "{state:?} × {id:?} 应被门控拒绝"
                );
            }
        }
        // 被控中的主控（链式场景）：切换拒绝
        assert!(matches!(
            route_host(
                HotkeyId::Next,
                &running(false),
                ControlDir::PeerControlsLocal,
                &peers
            ),
            HotkeyAction::None(_)
        ));
        // Pause 在被控/暂停态仍可 toggle（Running 内）
        assert_eq!(
            route_host(
                HotkeyId::Pause,
                &running(true),
                ControlDir::PeerControlsLocal,
                &peers
            ),
            HotkeyAction::SetPaused { paused: false }
        );
    }

    /// 空在线从机序：Next/Prev → 无操作。
    #[test]
    fn empty_peers_yields_none() {
        assert_eq!(
            route_host(HotkeyId::Next, &running(false), ControlDir::None, &[]),
            HotkeyAction::None("无在线从机")
        );
    }

    /// 从机路由：Pause toggle 可用；切换类动作无信令路径（留痕）。
    #[test]
    fn slave_routing() {
        assert_eq!(
            route_slave(HotkeyId::Pause, &running(false)),
            HotkeyAction::SetPaused { paused: true }
        );
        assert_eq!(
            route_slave(HotkeyId::Pause, &running(true)),
            HotkeyAction::SetPaused { paused: false }
        );
        for id in [HotkeyId::Next, HotkeyId::Prev, HotkeyId::BackToHost] {
            assert!(
                matches!(route_slave(id, &running(false)), HotkeyAction::None(_)),
                "{id:?} 从机侧应无信令路径"
            );
        }
    }

    /// 捕获转发门控真值表：仅 Running 未暂停 + LocalControlsPeer。
    #[test]
    fn capture_forward_gate_truth_table() {
        assert!(capture_forward_allowed(
            &running(false),
            ControlDir::LocalControlsPeer
        ));
        assert!(!capture_forward_allowed(&running(false), ControlDir::None));
        assert!(!capture_forward_allowed(
            &running(false),
            ControlDir::PeerControlsLocal
        ));
        assert!(!capture_forward_allowed(
            &running(true),
            ControlDir::LocalControlsPeer
        ));
        assert!(!capture_forward_allowed(
            &SessionState::Idle,
            ControlDir::LocalControlsPeer
        ));
    }

    /// HotkeyFilterSink：命中不注入（记录 sink 无事件）+ 命中通道投递；
    /// 未命中原样注入。
    #[tokio::test]
    async fn filter_sink_swallows_hits_and_forwards_misses() {
        let set = HotkeySet::from_config(&HotkeysConfig::default()).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = HotkeyFilterSink::new(set, RecordingInputSink::new(), tx);
        // 修饰键：未命中 → 注入
        sink.inject(&InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        })
        .unwrap();
        sink.inject(&InputEvent::Key {
            code: KEY_LEFT_ALT,
            state: KeyState::Down,
        })
        .unwrap();
        // 主键命中：吞（记录 sink 无事件）+ 通道投递
        sink.inject(&InputEvent::Key {
            code: KEY_RIGHT,
            state: KeyState::Down,
        })
        .unwrap();
        assert_eq!(rx.recv().await, Some(HotkeyId::Next));
        // 尾巴：吞
        sink.inject(&InputEvent::Key {
            code: KEY_RIGHT,
            state: KeyState::Up,
        })
        .unwrap();
        // 未命中普通键：注入
        sink.inject(&InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        })
        .unwrap();
        sink.inject(&InputEvent::Key {
            code: KEY_P,
            state: KeyState::Down,
        })
        .unwrap(); // Ctrl+Alt+P 已按着 Ctrl+Alt → 命中 Pause
        assert_eq!(rx.recv().await, Some(HotkeyId::Pause));
        // 记录 sink 事件 = Ctrl↓ + Alt↓ + A↓（命中主键与尾巴未注入）
        let recorded = sink.inner.events();
        assert_eq!(
            recorded,
            vec![
                InputEvent::Key {
                    code: KEY_LEFT_CTRL,
                    state: KeyState::Down,
                },
                InputEvent::Key {
                    code: KEY_LEFT_ALT,
                    state: KeyState::Down,
                },
                InputEvent::Key {
                    code: KEY_A,
                    state: KeyState::Down,
                },
            ]
        );
    }

    /// HotkeyFilterSink::release_all 重置过滤器（ReleaseAll 后旧修饰键不再参与
    /// 匹配——防跨会话误命中）。
    #[tokio::test]
    async fn filter_sink_release_all_resets_tracking() {
        let set = HotkeySet::from_config(&HotkeysConfig::default()).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = HotkeyFilterSink::new(set, RecordingInputSink::new(), tx);
        sink.inject(&InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        })
        .unwrap();
        sink.inject(&InputEvent::Key {
            code: KEY_LEFT_ALT,
            state: KeyState::Down,
        })
        .unwrap();
        sink.release_all().unwrap();
        // 重置后无修饰键跟踪：Right 不再命中
        sink.inject(&InputEvent::Key {
            code: KEY_RIGHT,
            state: KeyState::Down,
        })
        .unwrap();
        assert!(rx.try_recv().is_err(), "重置后不应命中");
    }

    /// 非键事件（路由层/鼠标）穿透 sink 直达内层。
    #[tokio::test]
    async fn filter_sink_passes_non_key_events() {
        let set = HotkeySet::from_config(&HotkeysConfig::default()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = HotkeyFilterSink::new(set, RecordingInputSink::new(), tx);
        sink.inject(&InputEvent::MouseMoveRel { dx: 1.0, dy: 2.0 })
            .unwrap();
        let recorded = sink.inner.events();
        assert_eq!(
            recorded,
            vec![InputEvent::MouseMoveRel { dx: 1.0, dy: 2.0 }]
        );
        // WarpPointer 透传
        sink.warp(0, 0.5, 0.5).unwrap();
        assert_eq!(sink.inner.warps(), &[(0, 0.5, 0.5)]);
        // 路由层事件到达 inject 也透传（会话层不产路由帧 InjectInput，防御路径）
        sink.inject(&InputEvent::EnterScreen {
            target: 1,
            edge: ScreenEdge::Right,
            ratio: 0.5,
            mods: xcontrol_proto::input::Mods::default(),
            preserve_cursor: false,
        })
        .unwrap();
    }

    /// XKey 语义（热键主键非修饰键）与 HotkeySet 默认键主键一致性冒烟
    /// （捕获流识别路径的锚点对拍）。
    #[test]
    fn default_hotkeys_use_expected_raw_keys() {
        let set = HotkeySet::from_config(&HotkeysConfig::default()).unwrap();
        let next = set.combo(HotkeyId::Next).unwrap();
        assert_eq!(next.key.raw(), KEY_RIGHT.raw());
        assert!(!next.key.is_modifier());
    }
}
