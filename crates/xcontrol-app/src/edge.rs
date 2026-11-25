//! `edge` — 边缘切换传感器（FR-2.1/2.2 代码侧闭环的判定与轮询解耦层，docs/03 §5）。
//!
//! # 职责切分
//!
//! - **判定（纯逻辑，本模块默认区）**：[`EdgeSensor`] 持有屏幕矩形几何 +
//!   邻接图（[`ScreenLayout`]，core 纯函数复用）+ 上次状态（归属帧 + 去抖闩锁），
//!   `feed(光标点) -> Option<EdgeHit>` 是纯状态机——无 I/O、无平台依赖，
//!   默认 `cargo test` 全量可测（PRD 要求 4：轮询与判定解耦）。
//! - **轮询（app 编排职责）**：本模块放 app 而非 core 的原因——光标读取是平台
//!   I/O（core 零 I/O 红线），10ms 轮询节奏（11-24 起；原 50ms——光标撞边后
//!   最坏要等 50–60ms 才切换，快速移动表现为撞边冻住）是会话运行时的编排决策；
//!   session_rt 经 [`poll_edge_sensor` 语义]（见 [`EdgeSensor::poll_cursor`]）把
//!   平台查询喂进纯判定。
//! - **平台查询（macos feature 门控）**：复用 xcontrol-input 既有安全封装
//!   [`xcontrol_input::cursor_location`]（CGEventGetLocation，无需辅助功能权限）
//!   与 [`xcontrol_input::display_layout`]（CGGetActiveDisplayList，active 空时
//!   online 兜底）——**不新增** input 平台 API（两者在 11-17-m2-input-macos 已是
//!   pub 安全函数）。
//!
//! # 判定管线（docs/03 §5「边缘判定」的采样实现）
//!
//! ```text
//! CG 全局点坐标 ──normalize（含屏定位 + 归一化 + 边缘容差带）──► (屏, nx, ny)
//!              ──frame 归属（最近一次严格内侧所在屏）──► core::layout::edge_crossing
//!              ──去抖闩锁（触发后须先回内侧）──► Option<EdgeHit>
//! ```
//!
//! - **归属帧**：边界样本（nx/ny 恰为 0.0/1.0）同时落在相邻两屏的闭区间内，
//!   方向歧义用「最近一次严格内侧所在屏」消解——从 A 滑到边界判 A 的帧
//!   （x=1.0 → A→B），从 B 滑回来判 B 的帧（x=0.0 → B→A）。
//! - **边缘容差带**（[`EDGE_TOLERANCE_POINTS`]）：CG 光标顶在屏边时坐标落在
//!   边界 ≤1 点内（点坐标采样粒度），docs/03 §5 的 ±0px 触发语义在采样层
//!   靠此容差带稳定命中；1 逻辑点远小于交互可感知距离，不改变「内侧不触发」。
//! - **去抖（PRD 要求 2 + docs/03 §5 死区）**：触发后闩锁，光标回严格内侧
//!   才重新武装（当拍不触发）——同一条边的连续命中由闩锁抑制。控制关系变化
//!   （切入/回切/Home）后的 150ms 防抖在 session_rt 门控层另计
//!   （`edge_dead_time_elapsed`，11-24）：回切落点贴缘，快速甩动时回内侧再推
//!   可能远快于 150ms，闩锁单独覆盖不了。
//!
//! # 已知限制（M2 范围，M3 布局协商收口）
//!
//! - ScreenId 仍是**各端本机**显示列表下标（macos.rs 限制项）：本模块产出的
//!   `EdgeHit.target` 在 M2 演示语义 = 对端应 warp 的目标屏下标；跨机 id 空间
//!   协商（`CtrlMsg::Layout` 下发）属 M3。多屏主控在本机屏间移动会产出
//!   本机→本机的命中（日志/冒烟可见），单屏主控无邻边、永不触发。
//! - 布局快照在会话启动时取一次（PRD 要求 1）；显示器热插拔重取属 M3+
//!   （MacInjector::refresh_layout 同期接线）。

use std::time::Duration;

use xcontrol_config::LayoutScreen;
use xcontrol_core::layout::{ScreenLayout, edge_crossing};
use xcontrol_proto::ScreenInfo;
use xcontrol_proto::input::ScreenId;

/// 边缘命中（`EnterScreen` 帧全部字段；直接复用 core 判定结果的类型，
/// 避免 app 侧重复定义同形结构）。
pub type EdgeHit = xcontrol_core::layout::EdgeCrossing;

/// 默认轮询间隔（11-24 起 10ms；[`EdgeSensor::poll_interval`] 可读，会话循环
/// 据此建 ticker）。主控边缘轮询与工作机受控光标回报共用本常量：原 50ms 下
/// 光标撞边后最坏要等 50–60ms 才切换（快速移动表现为撞边停住），10ms 把最坏
/// 切换延迟降到约 10ms。每次是微秒级光标查询（`CGEventGetLocation`）+ 一个
/// 约 35 字节的回报报文。**节拍与受控期门控必须同进同退**（docs/03 §5 死区）：
/// macOS 受控期间光标靠每 10ms 输入拍的锚定 warp 冻结，两次回拉之间会冲到屏
/// 边——轮询提到 10ms 后与回拉近乎同相位，误触发概率上升，故受控期间的主控
/// 边缘命中由 session_rt 门控一律忽略。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 边缘容差（全局点坐标）：贴边 ±1 点内的归一化样本收敛到精确边缘 0.0/1.0。
///
/// 依据：CG 光标坐标是点空间连续值，顶边驻留时落在边界 ≤1 点内；无容差时
/// 归一化 x = (w-1)/w ≈ 0.9997 < 1.0，`edge_crossing` 的 `≥1.0` 判定永不命中
/// （FR-2.1 实机不可用）。容差 0.0 退化为精确判定（单测对照）。
pub const EDGE_TOLERANCE_POINTS: f64 = 1.0;

/// 邻接判定容差（全局点坐标）：两屏「贴边」的共享边距离上限。
/// CG 排列坐标为整数点，1 点容差吸收浮点误差；带间隙（>1 点）的排列不建边。
const EDGE_TOUCH_EPS_POINTS: f64 = 1.0;

// ============================================================
// 屏幕几何（平台无关快照；macos 侧从 DisplayInfo 折算）
// ============================================================

/// 一块屏的矩形几何（CG 全局坐标：主屏左上为原点、y 向下、单位=点；
/// 平台无关快照——判定逻辑不感知 `DisplayInfo` 等 macos 类型）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    /// 左上角原点（全局点坐标）。
    pub origin: (f64, f64),
    /// 逻辑尺寸（点）。
    pub size: (f64, f64),
}

/// 由矩形快照推导邻接图（FR-1.3 本机布局：显式邻居 = 几何贴边 + 沿边重叠）。
///
/// 判定：B 是 A 的 `dir` 邻居 ⇔ 共享边距离 ≤ [`EDGE_TOUCH_EPS_POINTS`]
/// **且**沿边方向重叠长度 > 0（角对角零重叠不建边）；重叠最大者当选
/// （错位双屏：一条边对着两块屏时取重叠更长的）。无几何对齐强制
/// （docs/03 §5 图模型），此处只是本机布局的图构建启发式。
#[must_use]
pub fn screen_layout_from_rects(rects: &[ScreenRect]) -> ScreenLayout {
    let screens: Vec<ScreenInfo> = rects
        .iter()
        .enumerate()
        .map(|(i, _)| ScreenInfo {
            // 下标即 ScreenId（macos.rs 约定）；屏数 << u16::MAX，截断不可达。
            id: i as ScreenId,
            // owner 占位零值：本图只用于本机边缘判定（`edge_crossing` 不读
            // owner）与主控装配输入（`merge_layout` 统一覆写 local_owner，
            // core/layout.rs merge_layout 契约）——真实归属 identity 在装配点注入。
            owner: [0; 16],
            name: format!("Display {i}"),
            left: adjacent_neighbor(rects, i, Dir::Left),
            right: adjacent_neighbor(rects, i, Dir::Right),
            top: adjacent_neighbor(rects, i, Dir::Top),
            bottom: adjacent_neighbor(rects, i, Dir::Bottom),
            scale: 1.0,
        })
        .collect();
    ScreenLayout::new(screens)
}

/// 邻接查询方向（私有；与 [`ScreenEdge`] 一一对应但分离，保持几何层自洽）。
#[derive(Debug, Clone, Copy)]
enum Dir {
    /// 左（x 较小一侧）。
    Left,
    /// 右（x 较大一侧）。
    Right,
    /// 上（CG 全局坐标 y 向下：Top = y 较小一侧）。
    Top,
    /// 下（y 较大一侧）。
    Bottom,
}

/// `from` 屏在 `dir` 方向的邻居下标（无贴边重叠候选 → `None`）。
fn adjacent_neighbor(rects: &[ScreenRect], from: usize, dir: Dir) -> Option<ScreenId> {
    let (ax0, ay0) = rects[from].origin;
    let (aw, ah) = rects[from].size;
    let (ax1, ay1) = (ax0 + aw, ay0 + ah);
    let mut best: Option<(f64, usize)> = None; // (沿边重叠长度, 候选下标)
    for (j, rect) in rects.iter().enumerate() {
        if j == from {
            continue;
        }
        let (bx0, by0) = rect.origin;
        let (bw, bh) = rect.size;
        let (bx1, by1) = (bx0 + bw, by0 + bh);
        let overlap = match dir {
            Dir::Left | Dir::Right => ay1.min(by1) - ay0.max(by0),
            Dir::Top | Dir::Bottom => ax1.min(bx1) - ax0.max(bx0),
        };
        if overlap <= 0.0 {
            continue; // 零重叠（角对角）不建边
        }
        let touching = match dir {
            Dir::Right => (bx0 - ax1).abs() <= EDGE_TOUCH_EPS_POINTS,
            Dir::Left => (bx1 - ax0).abs() <= EDGE_TOUCH_EPS_POINTS,
            Dir::Bottom => (by0 - ay1).abs() <= EDGE_TOUCH_EPS_POINTS,
            Dir::Top => (by1 - ay0).abs() <= EDGE_TOUCH_EPS_POINTS,
        };
        if touching && best.is_none_or(|(best_overlap, _)| overlap > best_overlap) {
            best = Some((overlap, j));
        }
    }
    best.map(|(_, j)| j as ScreenId)
}

/// 含屏定位（纯函数）：闭区间包含（含边界）的首块命中屏。
///
/// 边界样本会同时落在相邻两屏的闭区间内——按索引序归属首块，方向歧义由
/// [`EdgeSensor`] 的归属帧（最近严格内侧屏）消解。非有限坐标（NaN/±inf）
/// 比较恒 false → 自然返回 `None`（协议对浮点字段的拒绝语义，core 同）。
#[must_use]
fn locate_screen(rects: &[ScreenRect], point: (f64, f64)) -> Option<ScreenId> {
    let (px, py) = point;
    for (i, rect) in rects.iter().enumerate() {
        let (ox, oy) = rect.origin;
        let (w, h) = rect.size;
        if w <= 0.0 || h <= 0.0 {
            continue; // 几何异常屏（macos display_layout 已滤，防御跳过）
        }
        if px >= ox && px <= ox + w && py >= oy && py <= oy + h {
            return Some(i as ScreenId);
        }
    }
    None
}

/// 全局点 → `screen` 帧的归一化坐标（纯函数）：`point` 相对该屏矩形换算、
/// clamp [0,1]、边缘容差带收敛。
///
/// 容差带：`归一化坐标 ≤ tolerance/尺寸` 收敛到 0.0，`≥ 1 - tolerance/尺寸`
/// 收敛到 1.0（收敛阈值再 clamp 到 ≤0.5，防窄屏全带退化）。点在屏外时被
/// clamp 到边界——调用方保证只在「边界样本换算到归属帧」路径使用（该路径
/// 下点必在归属帧的边界邻域）。
#[must_use]
fn normalized_in(
    rects: &[ScreenRect],
    screen: ScreenId,
    point: (f64, f64),
    tolerance: f64,
) -> (f32, f32) {
    let (px, py) = point;
    let (ox, oy) = rects[usize::from(screen)].origin;
    let (w, h) = rects[usize::from(screen)].size;
    let mut nx = ((px - ox) / w).clamp(0.0, 1.0) as f32;
    let mut ny = ((py - oy) / h).clamp(0.0, 1.0) as f32;
    let tx = (tolerance / w).clamp(0.0, 0.5) as f32;
    let ty = (tolerance / h).clamp(0.0, 0.5) as f32;
    if nx <= tx {
        nx = 0.0;
    } else if nx >= 1.0 - tx {
        nx = 1.0;
    }
    if ny <= ty {
        ny = 0.0;
    } else if ny >= 1.0 - ty {
        ny = 1.0;
    }
    (nx, ny)
}

/// 受控屏光标采样（双向边缘回切的被控方采样面，11-21 反馈 5；docs/03 §5）：
/// 全局点坐标 → `screen` 屏归一化坐标（边缘容差带收敛——边缘驻留回报恰
/// 0.0/1.0 端点值，控制方极值判定的回报契约）。
///
/// 防御：`screen` 越界、点不在该屏闭区间内（多屏从机上光标在另一块屏）→
/// `None`（本拍跳过——把别的屏上的点 clamp 到受控屏边缘会造成假极值回报）。
/// 非有限坐标：包含判定恒 false → `None`（协议浮点拒绝语义同源）。
#[must_use]
pub fn controlled_cursor_normalized(
    rects: &[ScreenRect],
    screen: ScreenId,
    point: (f64, f64),
) -> Option<(f32, f32)> {
    let rect = rects.get(usize::from(screen))?;
    let (px, py) = point;
    let (ox, oy) = rect.origin;
    let (w, h) = rect.size;
    if w <= 0.0 || h <= 0.0 || !(px >= ox && px <= ox + w && py >= oy && py <= oy + h) {
        return None;
    }
    Some(normalized_in(rects, screen, point, EDGE_TOLERANCE_POINTS))
}

// ============================================================
// EdgeSensor（纯状态机）
// ============================================================

/// 去抖闩锁态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Latch {
    /// 在内侧（或从未触发）：允许触发。
    Armed,
    /// 已触发未回内侧：一律不触发。
    Triggered,
}

/// 边缘切换传感器：布局 + 上次状态 + 轮询配置的纯状态机（FR-2.1）。
///
/// `feed` 逐样本推进：内部→越界命中 [`EdgeHit`]；已触发未回内侧 → 去抖
/// `None`；回严格内侧重新武装（当拍不触发）。线程模型：单属主（会话循环
/// select 分支独占 `&mut`），无锁（architecture-rules 红线 3 的轮询纪律）。
#[derive(Debug)]
pub struct EdgeSensor {
    /// 矩形快照（归一化定位用；与 `layout` 同序同 id）。
    rects: Vec<ScreenRect>,
    /// 邻接图（core `edge_crossing` 查询用）。
    layout: ScreenLayout,
    /// 归属帧：最近一次光标严格内侧所在屏（初始 = 本机主屏下标）。
    frame: ScreenId,
    /// 去抖闩锁。
    latch: Latch,
    /// 轮询间隔（会话循环建 ticker 用；默认 [`DEFAULT_POLL_INTERVAL`] 10ms）。
    poll_interval: Duration,
    /// 边缘容差（点；默认 [`EDGE_TOLERANCE_POINTS`]，单测可置 0 对照）。
    tolerance: f64,
}

impl EdgeSensor {
    /// 本机矩形快照（`layout` 同序同 id；释放落点换算用——`WarpPointer` 的
    /// 全局 id 即本 rects 下标，见 [`host_edge_sensor`] 与 macos.rs 约定）。
    #[must_use]
    pub fn rects(&self) -> &[ScreenRect] {
        &self.rects
    }
}

impl EdgeSensor {
    /// 构建传感器（`local` = 本机主屏下标，作为初始归属帧）。
    ///
    /// 无屏幕或 `local` 越界 → `None`（PRD：非 macos/无屏幕 → 跳过轮询）。
    #[must_use]
    pub fn new(rects: Vec<ScreenRect>, local: ScreenId, poll_interval: Duration) -> Option<Self> {
        let layout = screen_layout_from_rects(&rects);
        Self::with_layout(rects, layout, local, poll_interval)
    }

    /// 显式布局构造（11-20 边缘切换接线）：`layout` 为**合并布局**（本机物理屏
    ///       + config 外接从机条目）——传感器邻接图必须含从机屏，否则光标滑出最右
    ///       本机屏边缘时无从机邻居可命中（FR-2.1 不触发的实测根因）。`rects` 仍为
    ///       本机物理几何（光标定位用；从机条目无本机几何）。
    #[must_use]
    pub fn with_layout(
        rects: Vec<ScreenRect>,
        layout: ScreenLayout,
        local: ScreenId,
        poll_interval: Duration,
    ) -> Option<Self> {
        if rects.is_empty() || usize::from(local) >= rects.len() {
            return None;
        }
        Some(Self {
            layout,
            rects,
            frame: local,
            latch: Latch::Armed,
            poll_interval,
            tolerance: EDGE_TOLERANCE_POINTS,
        })
    }

    /// 覆盖边缘容差（点；0.0 = 精确判定，单测对照用）。
    #[must_use]
    pub fn with_tolerance(mut self, tolerance_points: f64) -> Self {
        self.tolerance = tolerance_points;
        self
    }

    /// 轮询间隔（会话循环建 ticker 用）。
    #[must_use]
    pub const fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// 邻接图（诊断/测试）。
    #[must_use]
    pub const fn layout(&self) -> &ScreenLayout {
        &self.layout
    }

    /// 喂入一个光标样本（全局点坐标）→ 去抖后的边缘命中。
    ///
    /// - 样本不在任何屏（异常间隙/非有限值）→ `None`，闩锁与归属帧**保持**；
    /// - 严格内侧（0 < nx < 1 且 0 < ny < 1，在包含屏帧内判定）→ 刷新归属帧，
    ///   闩锁态则重新武装；边界样本沿用上一归属帧并**换算到该帧**判定
    ///   （方向语义：从哪块屏滑入边界，就判哪块屏的越界方向）；
    /// - 武装态越界 → core 判定（Left > Right > Top > Bottom 固定优先级），
    ///   命中即闩锁。
    #[must_use]
    pub fn feed(&mut self, point: (f64, f64)) -> Option<EdgeHit> {
        let containing = locate_screen(&self.rects, point)?;
        let (cx, cy) = normalized_in(&self.rects, containing, point, self.tolerance);
        let inside = cx > 0.0 && cx < 1.0 && cy > 0.0 && cy < 1.0;
        if inside {
            self.frame = containing;
        }
        if self.latch == Latch::Triggered {
            if inside {
                self.latch = Latch::Armed; // 回内侧重新武装（当拍不触发）
            }
            return None; // 去抖：触发后未回内侧一律 None（docs/03 §5 死区的强语义）
        }
        // 边界样本换算到归属帧（containing 帧的 1.0 可能是归属帧的 0.0——
        // 共享边界两侧语义相反，必须按「从哪块屏滑来」取帧）。
        let (nx, ny) = if inside {
            (cx, cy)
        } else {
            normalized_in(&self.rects, self.frame, point, self.tolerance)
        };
        let hit = edge_crossing(&self.layout, self.frame, nx, ny);
        if hit.is_some() {
            self.latch = Latch::Triggered;
        }
        hit
    }

    /// 本机物理屏数（11-20 策略分层：传感器纯机制返回一切邻居命中；
    /// 「本机屏间移动 ≠ 切换」的策略过滤在 drive_session 集成层，用本数判别）。
    #[must_use]
    pub fn local_screen_count(&self) -> usize {
        self.rects.len()
    }

    /// 读取平台光标并喂入判定（macos：`CGEventGetLocation`，无需辅助功能权限）。
    ///
    /// # Platform
    ///
    /// - `macos`：[`xcontrol_input::cursor_location`]（既有安全封装，零 unsafe）；
    ///   单拍查询失败（事件源创建失败等）→ `None` 跳过本拍，不中断轮询
    ///   （error-handling.md 规则 4：可忽略错误不打断实时路径）。
    /// - 其余平台：M2 无实现，恒 `None`（且 [`host_edge_sensor`] 恒 `None`，
    ///   会话循环不建轮询分支——此桩仅为编译完整）。
    #[must_use]
    #[cfg(feature = "macos")]
    pub fn poll_cursor(&mut self) -> Option<EdgeHit> {
        match xcontrol_input::cursor_location() {
            Ok(point) => self.feed(point),
            Err(error) => {
                tracing::debug!(target: "app::edge", %error, "光标查询失败（跳过本拍）");
                None
            }
        }
    }

    /// 非 macos 平台的编译桩（见 [`Self::poll_cursor`] 平台说明）。
    #[must_use]
    #[cfg(not(feature = "macos"))]
    pub fn poll_cursor(&mut self) -> Option<EdgeHit> {
        None
    }
}

/// 主控侧传感器构建（会话启动时一次，PRD 要求 1）。
///
/// # Platform
///
/// - `macos`：[`xcontrol_input::display_layout`] 查询本机屏幕几何
///   （active 空 → online 兜底），主显下标作归属帧；查询失败/无屏幕 → `None`。
/// - 非 `macos`：恒 `None`（会话循环跳过边缘轮询分支）。
#[must_use]
#[cfg(feature = "macos")]
pub fn local_display_rects() -> Option<Vec<ScreenRect>> {
    let infos = xcontrol_input::display_layout().ok()?;
    let rects: Vec<ScreenRect> = infos
        .iter()
        .map(|info| ScreenRect {
            origin: info.origin,
            size: info.size_points,
        })
        .collect();
    (!rects.is_empty()).then_some(rects)
}

/// 释放落点换算（11-22 回切落点覆盖修复）：`WarpPointer{screen, x, y}` 归一
/// 化坐标 → 本机全局点坐标（rect origin + 归一化×尺寸，clamp 屏内）。`screen`
/// = 本机 rects 下标（`host_edge_sensor` 与 macos.rs 的 id 约定）；越界屏/非
/// 有限坐标 → `None`（不重放，光标停物理鼠标位）。
#[must_use]
pub fn release_warp_point(
    rects: &[ScreenRect],
    screen: ScreenId,
    x: f32,
    y: f32,
) -> Option<(f64, f64)> {
    let rect = rects.get(usize::from(screen))?;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let (ox, oy) = rect.origin;
    let (w, h) = rect.size;
    let nx = f64::from(x).clamp(0.0, 1.0);
    let ny = f64::from(y).clamp(0.0, 1.0);
    Some((ox + nx * w, oy + ny * h))
}

/// 回切落点补偿（11-24 回切行程补偿，纯函数；docs/03 §5 回切第 3 条）：
/// [`release_warp_point`] 的落点加上 `travel`（主控点；`(dx, dy)`，符号与屏幕
/// 坐标轴同向——右/下为正），再夹回**落点所在屏**内，四周各留
/// [`xcontrol_core::session::EDGE_RETURN_INSET`] 换算的点数。
///
/// `travel` = 三段在回切时本来会丢失的位移之和：工作机顶边期间被屏幕边界截掉的
/// 推进量（随回报带回）+ 主控最后转发、在工作机采样之后才到的一批 + 回切触发后
/// 到本地输入恢复之间被本地拦截的位移。加到落点上，快速甩动回切后光标落点与
/// 手势连续（原实现落点恒贴缘停在中途，慢速时几像素、快速时几十到上百点）。
///
/// 四周留白与 core 的回切落点内缩同源：补偿后的落点不得落到别条边的容差带或
/// 角落区里（否则回切后立即再触发切换）。`travel` 为 `(0.0, 0.0)` 即原落点。
///
/// 防御：屏下标越界 / 非有限归一化坐标 → `None`（同 [`release_warp_point`]，
/// 不重放，光标停物理鼠标位）；`travel` 非有限或屏几何非正（无法安全夹取）→
/// 退回**未补偿**的原落点（宁可少补一段，不让落点变成 NaN 或 panic）。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
pub fn compensated_release_point(
    rects: &[ScreenRect],
    screen: ScreenId,
    x: f32,
    y: f32,
    travel: (f32, f32),
) -> Option<(f64, f64)> {
    let base = release_warp_point(rects, screen, x, y)?;
    let rect = rects.get(usize::from(screen))?;
    let (tx, ty) = (f64::from(travel.0), f64::from(travel.1));
    let (ox, oy) = rect.origin;
    let (w, h) = rect.size;
    // `f64::clamp` 在 min/max 为 NaN 或 min > max 时 panic——几何非有限/非正一律
    // 不夹取，退回未补偿的原落点。
    if !tx.is_finite()
        || !ty.is_finite()
        || !(ox.is_finite() && oy.is_finite())
        || !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0)
    {
        return Some(base);
    }
    let inset = f64::from(xcontrol_core::session::EDGE_RETURN_INSET);
    let clamp_axis = |value: f64, origin: f64, size: f64| {
        value.clamp(origin + inset * size, origin + (1.0 - inset) * size)
    };
    Some((
        clamp_axis(base.0 + tx, ox, w),
        clamp_axis(base.1 + ty, oy, h),
    ))
}

/// 非 `macos` 平台的空实现（受控光标回报仅 mac 部署面有采样源——与
/// [`host_edge_sensor`] 的平台矩阵一致）。
#[must_use]
#[cfg(not(feature = "macos"))]
pub fn local_display_rects() -> Option<Vec<ScreenRect>> {
    None
}

/// 主控侧传感器构建（会话启动时一次，PRD 要求 1）。
///
/// # Platform
///
/// - `macos`：[`xcontrol_input::display_layout`] 查询本机屏幕几何
///   （active 空 → online 兜底），主显下标作归属帧；查询失败/无屏幕 → `None`。
/// - 非 `macos`：恒 `None`（会话循环跳过边缘轮询分支）。
#[must_use]
#[cfg(feature = "macos")]
pub fn host_edge_sensor(poll_interval: Duration, externals: &[LayoutScreen]) -> Option<EdgeSensor> {
    let infos = xcontrol_input::display_layout().ok()?;
    let main = infos.iter().position(|info| info.is_main).unwrap_or(0);
    let rects: Vec<ScreenRect> = infos
        .iter()
        .map(|info| ScreenRect {
            origin: info.origin,
            size: info.size_points,
        })
        .collect();
    // 合并布局（11-20）：本机物理屏 + config 外接从机条目（链式挂接到该方向
    // 链尾——双 mac 屏时从机挂最右屏右侧，merge_layout 链式语义已保证）。
    let local_layout = screen_layout_from_rects(&rects);
    let layout = xcontrol_core::layout::merge_layout(&local_layout, [0; 16], [0; 16], externals);
    tracing::info!(
        target: "app::edge",
        screens = rects.len(),
        externals = externals.len(),
        main,
        "边缘传感器就绪（合并布局快照）"
    );
    EdgeSensor::with_layout(rects, layout, main as ScreenId, poll_interval)
}

/// 非 macos 平台的空实现（见 [`host_edge_sensor`] 平台说明）。
#[must_use]
#[cfg(not(feature = "macos"))]
pub fn host_edge_sensor(
    _poll_interval: Duration,
    _externals: &[LayoutScreen],
) -> Option<EdgeSensor> {
    None
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use xcontrol_config::{Direction, LayoutScreen};
    use xcontrol_core::layout::merge_layout;
    use xcontrol_proto::input::ScreenEdge;

    use super::*;

    /// 边缘切换接线（11-20）：2 物理屏 + config 从机右挂链尾 →
    /// 光标滑出最右屏右缘命中从机（id=2，FR-2.1 传感器合并布局回归）。
    #[test]
    fn edge_crossing_hits_slave_chained_after_displays() {
        let rects = rects_2x1();
        let local = screen_layout_from_rects(&rects);
        let layout = merge_layout(
            &local,
            [0; 16],
            [0; 16],
            &[LayoutScreen {
                id: "lsdark-eos".into(),
                relative_to: None,
                direction: Some(Direction::Right),
            }],
        );
        let Some(mut sensor) =
            EdgeSensor::with_layout(rects.clone(), layout, 0, Duration::from_millis(50))
        else {
            panic!("双屏 fixture 传感器构造应成功");
        };
        // 主屏内移动：不触发。
        assert!(sensor.feed((500.0, 500.0)).is_none());
        // 主屏右缘：右邻 = 本机第二屏（机制层命中；「本机屏间 ≠ 切换」策略在
        // drive_session 用 local_screen_count 过滤）。
        let hit = sensor.feed((2000.0, 500.0)).expect("机制层应命中右邻");
        assert_eq!(hit.target, 1);
        assert_eq!(hit.edge, ScreenEdge::Right);
        // 进入第二屏内侧：去抖重武装。
        assert!(sensor.feed((1500.0, 500.0)).is_none());
        // 第二屏右缘：右邻 = 从机（合并布局链尾 id=2）——FR-2.1 从机切换命中。
        let hit = sensor.feed((2000.0, 500.0)).expect("第二屏右缘应命中从机");
        assert_eq!(hit.target, 2);
        assert_eq!(hit.edge, ScreenEdge::Right);
    }

    /// 2×1 双屏 fixture：A(0) 右邻 B(1)，各 1000×1000 点（数学干净）。
    fn rects_2x1() -> Vec<ScreenRect> {
        vec![
            ScreenRect {
                origin: (0.0, 0.0),
                size: (1000.0, 1000.0),
            },
            ScreenRect {
                origin: (1000.0, 0.0),
                size: (1000.0, 1000.0),
            },
        ]
    }

    /// L 形 3 屏 fixture：A(0) 右邻 B(1)、下邻 C(2)。
    fn rects_l_shape() -> Vec<ScreenRect> {
        vec![
            ScreenRect {
                origin: (0.0, 0.0),
                size: (1000.0, 1000.0),
            },
            ScreenRect {
                origin: (1000.0, 0.0),
                size: (1000.0, 1000.0),
            },
            ScreenRect {
                origin: (0.0, 1000.0),
                size: (1000.0, 1000.0),
            },
        ]
    }

    /// 2×1 传感器（主屏 A，默认容差）。
    fn sensor_2x1() -> EdgeSensor {
        EdgeSensor::new(rects_2x1(), 0, DEFAULT_POLL_INTERVAL).expect("合法布局")
    }

    // —— 邻接图构建 ——

    /// 贴边 + 重叠 → 建边；非贴边方向/角对角零重叠/带间隙 → 不建边。
    #[test]
    fn adjacency_from_rects_touch_overlap_and_gap() {
        let layout = screen_layout_from_rects(&rects_2x1());
        assert_eq!(layout.neighbor(0, ScreenEdge::Right), Some(1));
        assert_eq!(layout.neighbor(1, ScreenEdge::Left), Some(0));
        assert_eq!(layout.neighbor(0, ScreenEdge::Left), None);
        assert_eq!(layout.neighbor(0, ScreenEdge::Top), None);
        assert_eq!(layout.neighbor(0, ScreenEdge::Bottom), None);
        assert_eq!(layout.neighbor(1, ScreenEdge::Right), None);

        let l = screen_layout_from_rects(&rects_l_shape());
        assert_eq!(l.neighbor(0, ScreenEdge::Right), Some(1));
        assert_eq!(l.neighbor(0, ScreenEdge::Bottom), Some(2));
        assert_eq!(l.neighbor(1, ScreenEdge::Left), Some(0));
        assert_eq!(l.neighbor(2, ScreenEdge::Top), Some(0));
        // 角对角（B 右下角贴 C 右上角，零重叠）：B 下缘 / C 右缘不建边
        assert_eq!(l.neighbor(1, ScreenEdge::Bottom), None);
        assert_eq!(l.neighbor(2, ScreenEdge::Right), None);

        // 带间隙（> 1 点）不建边
        let gapped = vec![
            ScreenRect {
                origin: (0.0, 0.0),
                size: (1000.0, 1000.0),
            },
            ScreenRect {
                origin: (1002.0, 0.0),
                size: (1000.0, 1000.0),
            },
        ];
        let layout = screen_layout_from_rects(&gapped);
        assert_eq!(layout.neighbor(0, ScreenEdge::Right), None);
        assert_eq!(layout.neighbor(1, ScreenEdge::Left), None);
    }

    // —— 主判定路径（PRD 单测矩阵） ——

    /// 滑入右缘触发 → 继续外推不再触发 → 回到内侧重新武装 → 再次触发。
    #[test]
    fn slide_to_right_edge_triggers_latches_rearms() {
        let mut sensor = sensor_2x1();
        // 内侧（含容差带内但不贴边）：不触发，保持武装
        assert_eq!(sensor.feed((500.0, 500.0)), None);
        assert_eq!(sensor.feed((990.0, 500.0)), None);
        // 滑入右缘（容差带收敛 +1.0 点内）：命中 A→B，ratio = y
        assert_eq!(
            sensor.feed((999.5, 500.0)),
            Some(EdgeHit {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            })
        );
        // 继续外推（贴边/外推样本）：去抖 None
        assert_eq!(sensor.feed((1000.0, 500.0)), None);
        assert_eq!(sensor.feed((1000.0, 501.0)), None);
        assert_eq!(sensor.feed((999.8, 499.0)), None);
        // 越过边界进入 B（另一屏的严格内侧）：去抖放行（当拍 None）+ 重新武装
        assert_eq!(sensor.feed((1100.0, 500.0)), None);
        // 回到 A 内侧（归属帧刷新为 A）后再触发：新的命中
        assert_eq!(sensor.feed((500.0, 500.0)), None);
        assert_eq!(
            sensor.feed((999.5, 250.0)),
            Some(EdgeHit {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.25,
            })
        );
    }

    /// 角落排除（11-22 第三轮，docs/03 §5；11-24 由"两轴同为端点"改为 5% 角落区）：
    /// 传感器边界样本落在角落区（沿边坐标在角落带内）→ 不触发任何边——角落留给
    /// 系统热区（KDE 触发角专用，边缘=切换），且不闩锁（None 且武装保持，可连续
    /// 喂）；角落区外照常触发，ratio = 沿边归一化值。传感器的判定代码不改——它
    /// 调用的是 core `edge_crossing`。
    #[test]
    fn corner_excluded_and_interior_ratio_preserved() {
        // L 形布局（各屏 1000×1000 点）：A 右缘→B、下缘→C。四角逐一喂入。
        let mut sensor =
            EdgeSensor::new(rects_l_shape(), 0, DEFAULT_POLL_INTERVAL).expect("合法布局");
        // 右上角（1000, 0）：右缘有邻居也排除。
        assert_eq!(sensor.feed((1000.0, 0.0)), None, "右上角");
        // 右下角（1000, 1000）：右/下缘均有邻居（Right 优先候选）仍排除。
        assert_eq!(sensor.feed((1000.0, 1000.0)), None, "右下角");
        // 左上角（0, 0）：两缘均无邻居，排除同语义。
        assert_eq!(sensor.feed((0.0, 0.0)), None, "左上角");
        // 左下角（0, 1000）：下缘有邻居（→C）仍排除。
        assert_eq!(sensor.feed((0.0, 1000.0)), None, "左下角");
        // 角落带内的**非角点**（沿边坐标 3% / 97%——远在 1 点容差带之外、未收敛
        // 到端点）：同样排除。旧实现下"离角 1.5 点即触发"，斜着进角落几乎必然
        // 先在角落外贴到可切换的边。
        assert_eq!(
            sensor.feed((1000.0, 30.0)),
            None,
            "右缘上端角落带内（ny=0.03）"
        );
        assert_eq!(
            sensor.feed((1000.0, 970.0)),
            None,
            "右缘下端角落带内（ny=0.97）"
        );
        assert_eq!(
            sensor.feed((30.0, 1000.0)),
            None,
            "下缘左端角落带内（nx=0.03）"
        );
        assert_eq!(
            sensor.feed((970.0, 1000.0)),
            None,
            "下缘右端角落带内（nx=0.97）"
        );
        // 角落区外回归：右缘中段触发，ratio = y = 0.5。
        assert_eq!(
            sensor.feed((1000.0, 500.0)),
            Some(EdgeHit {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            })
        );
    }

    /// 传感器与 core 共用同一角落比例（11-24，AC9）：传感器判定面直接调用 core
    /// `edge_crossing`——沿边坐标取 `CORNER_RATIO ± 0.001` 处，传感器命中与否必须
    /// 与 core 纯函数逐点一致（带内不触发、带外触发）。容差置 0（归一化值精确
    /// 等于构造比例）；每样本新建传感器（命中会闩锁）。
    #[test]
    fn sensor_corner_band_matches_core_ratio() {
        use xcontrol_core::layout::CORNER_RATIO;
        let boundary = f64::from(CORNER_RATIO);
        for along in [
            CORNER_RATIO - 0.001,
            CORNER_RATIO + 0.001,
            1.0 - CORNER_RATIO - 0.001,
            1.0 - CORNER_RATIO + 0.001,
        ] {
            let along_f64 = f64::from(along);
            let in_band = along_f64 <= boundary || along_f64 >= 1.0 - boundary;
            for (point, (nx, ny)) in [
                // A 右缘：沿边坐标 = y。
                ((1000.0, along_f64 * 1000.0), (1.0, along)),
                // A 下缘：沿边坐标 = x。
                ((along_f64 * 1000.0, 1000.0), (along, 1.0)),
            ] {
                let mut sensor = EdgeSensor::new(rects_l_shape(), 0, DEFAULT_POLL_INTERVAL)
                    .expect("合法布局")
                    .with_tolerance(0.0);
                let via_sensor = sensor.feed(point);
                let via_core = edge_crossing(sensor.layout(), 0, nx, ny);
                assert_eq!(
                    via_sensor, via_core,
                    "沿边坐标 {along}：传感器判定必须与 core edge_crossing 一致"
                );
                assert_eq!(
                    via_sensor.is_none(),
                    in_band,
                    "沿边坐标 {along}：{}",
                    if in_band {
                        "角落带内不触发"
                    } else {
                        "角落带外照常触发"
                    }
                );
            }
        }
    }

    /// 归属帧：边界样本的方向按「最近一次严格内侧所在屏」消解——
    /// 从 B 内侧滑回左界 → 判 B 的帧（x=0.0）→ B→A 命中。
    #[test]
    fn boundary_attributed_by_last_inside_screen() {
        let mut sensor = sensor_2x1();
        // 初始就在 B 内侧：归属帧刷新为 B（无命中）
        assert_eq!(sensor.feed((1500.0, 500.0)), None);
        // 滑到 A/B 共享边界（闭区间首命中 = A，但归属帧 = B）：B→A
        assert_eq!(
            sensor.feed((1000.0, 500.0)),
            Some(EdgeHit {
                target: 0,
                edge: ScreenEdge::Left,
                ratio: 0.5,
            })
        );
    }

    /// 初始归属帧 = 本机主屏：首样本即贴边（从未在任何屏严格内侧）也按主屏帧判定。
    #[test]
    fn initial_frame_is_local_main_screen() {
        let mut sensor = sensor_2x1();
        assert_eq!(
            sensor.feed((1000.0, 300.0)),
            Some(EdgeHit {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.3,
            })
        );
    }

    /// 无邻居边缘不触发（FR-2.1 AC「无从机的边缘不切换」）且不改变武装态。
    #[test]
    fn edge_without_neighbor_never_triggers() {
        let mut sensor = sensor_2x1();
        // A 左/上/下无边；B 右/上/下无边
        assert_eq!(sensor.feed((0.0, 500.0)), None);
        assert_eq!(sensor.feed((500.0, 0.0)), None);
        assert_eq!(sensor.feed((500.0, 1000.0)), None);
        // 武装态未被消耗：随后右缘照常触发
        assert!(sensor.feed((1000.0, 500.0)).is_some());
    }

    // —— 容差带 ——

    /// 默认容差：边界 1 点内的样本收敛命中；容差 0：精确判定不命中（对照）。
    #[test]
    fn tolerance_band_snaps_near_edge_samples() {
        // 默认容差 1.0 点：999.5（0.5 点内）→ 命中
        let mut sensor = sensor_2x1();
        assert!(sensor.feed((999.5, 500.0)).is_some());
        // 容差 0：999.5 → nx=0.9995 < 1.0 → 不命中；1000.0 → 命中
        let mut exact = sensor_2x1().with_tolerance(0.0);
        assert_eq!(exact.feed((999.5, 500.0)), None);
        assert!(exact.feed((1000.0, 500.0)).is_some());
        // 容差带只影响贴边：990（10 点内内区）永不命中
        let mut sensor = sensor_2x1();
        assert_eq!(sensor.feed((990.0, 500.0)), None);
    }

    // —— 异常样本 ——

    /// 样本不在任何屏 / 非有限值 → None 且状态保持（闩锁不丢、武装不变）。
    #[test]
    fn unlocatable_sample_keeps_state() {
        let mut sensor = sensor_2x1();
        // 触发进入闩锁
        assert!(sensor.feed((1000.0, 500.0)).is_some());
        // 屏外样本：None 且闩锁保持（再贴边仍去抖）
        assert_eq!(sensor.feed((2500.0, 500.0)), None);
        assert_eq!(sensor.feed((1000.0, 500.0)), None);
        // 非有限值：None
        assert_eq!(sensor.feed((f64::NAN, 500.0)), None);
        assert_eq!(sensor.feed((500.0, f64::INFINITY)), None);
        // 回内侧 → 重新武装 → 可再触发
        assert_eq!(sensor.feed((500.0, 500.0)), None);
        assert!(sensor.feed((1000.0, 400.0)).is_some());
    }

    /// 构建校验：空布局 / 主屏下标越界 → None。
    #[test]
    fn rejects_empty_layout_and_out_of_range_local() {
        assert!(EdgeSensor::new(Vec::new(), 0, DEFAULT_POLL_INTERVAL).is_none());
        assert!(EdgeSensor::new(rects_2x1(), 2, DEFAULT_POLL_INTERVAL).is_none());
        let sensor = sensor_2x1();
        assert_eq!(sensor.poll_interval(), DEFAULT_POLL_INTERVAL);
        assert_eq!(sensor.layout().len(), 2);
    }

    // —— 节拍（11-24，D7） ——

    /// 默认节拍 10ms：主控边缘轮询与工作机受控光标回报共用
    /// `DEFAULT_POLL_INTERVAL`——原 50ms 下光标撞边后最坏等 50–60ms 才切换。
    /// 显式间隔照原值透传（`EdgeSensor::poll_interval` 接口不变）。
    #[test]
    fn default_poll_interval_is_10ms() {
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_millis(10));
        assert_eq!(
            sensor_2x1().poll_interval(),
            DEFAULT_POLL_INTERVAL,
            "默认构造的传感器读到的即默认节拍"
        );
        let custom = EdgeSensor::new(rects_2x1(), 0, Duration::from_millis(33)).expect("合法布局");
        assert_eq!(custom.poll_interval(), Duration::from_millis(33));
    }

    // —— 受控光标采样（双向边缘回切，docs/03 §5） ——

    /// 采样契约：屏内归一化 + 边缘容差带收敛端点值（驻留回报恰 0.0/1.0）。
    #[test]
    fn controlled_cursor_normalizes_with_tolerance_snap() {
        let rects = rects_2x1();
        // 屏内：比例归一化。
        assert_eq!(
            controlled_cursor_normalized(&rects, 1, (1500.0, 500.0)),
            Some((0.5, 0.5))
        );
        // 边缘驻留（右缘 ±1 点内）→ 收敛 1.0（主控极值判定的回报契约）。
        assert_eq!(
            controlled_cursor_normalized(&rects, 1, (1999.5, 500.0)),
            Some((1.0, 0.5))
        );
        // 左缘驻留 → 0.0。
        assert_eq!(
            controlled_cursor_normalized(&rects, 0, (0.0, 300.0)),
            Some((0.0, 0.3))
        );
    }

    /// 采样防御：越界屏 id、点在别的屏（多屏从机）、屏外点、非有限值 → None
    /// （假极值防护——把别屏的点 clamp 进受控屏会伪造边缘回报）。
    #[test]
    fn controlled_cursor_rejects_foreign_points_and_bad_input() {
        let rects = rects_2x1();
        // 越界屏 id。
        assert_eq!(
            controlled_cursor_normalized(&rects, 2, (500.0, 500.0)),
            None
        );
        // 点在另一块屏（受控屏 = 0，光标在屏 1 上）：不 clamp、跳过本拍。
        assert_eq!(
            controlled_cursor_normalized(&rects, 0, (1500.0, 500.0)),
            None
        );
        // 屏外点。
        assert_eq!(
            controlled_cursor_normalized(&rects, 1, (2500.0, 500.0)),
            None
        );
        // 非有限值。
        assert_eq!(
            controlled_cursor_normalized(&rects, 1, (f64::NAN, 500.0)),
            None
        );
        assert_eq!(
            controlled_cursor_normalized(&rects, 1, (500.0, f64::INFINITY)),
            None
        );
    }

    // —— 回切落点补偿（11-24 回切行程补偿，docs/03 §5 回切第 3 条） ——

    /// 浮点点比较：归一化落点经 f32 → f64 换算有末位误差（0.995f32 在 1000 点
    /// 屏上约 5e-6 点），容差取 1e-3 点。
    fn assert_point_close(actual: (f64, f64), expected: (f64, f64)) {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-3;
        assert!(
            close(actual.0, expected.0) && close(actual.1, expected.1),
            "落点 {actual:?} ≠ 期望 {expected:?}"
        );
    }

    /// 落点补偿：原落点 + 行程向量；零行程 = 原落点（与 `release_warp_point`
    /// 同值）。行程单位是主控点，符号与屏幕坐标轴同向（右/下为正）。
    #[test]
    fn compensated_release_point_adds_travel_to_landing() {
        let rects = rects_2x1();
        let inset = f64::from(xcontrol_core::session::EDGE_RETURN_INSET) * 1000.0;
        // 零行程：原落点不变（主控左缘内侧）。
        assert_point_close(
            compensated_release_point(&rects, 0, 0.005, 0.4, (0.0, 0.0)).expect("合法落点"),
            release_warp_point(&rects, 0, 0.005, 0.4).expect("合法落点"),
        );
        // 向右回切（工作机右缘滑出 → 主控左缘落点）+ 顶边行程 120 点、在途 8 点：
        // 落点沿行进方向前移。
        assert_point_close(
            compensated_release_point(&rects, 0, 0.005, 0.4, (128.0, 8.0)).expect("合法落点"),
            (inset + 128.0, 408.0),
        );
        // 向左回切（工作机左缘滑出 → 主控右缘落点）：行程为负，落点离缘内移；
        // 副屏（origin = 1000）按各自矩形换算。
        assert_point_close(
            compensated_release_point(&rects, 1, 0.995, 0.6, (-40.0, -12.5)).expect("合法落点"),
            (1000.0 + 1000.0 - inset - 40.0, 587.5),
        );
        // 横向外侧边：行程落在 y 轴。
        assert_point_close(
            compensated_release_point(&rects, 0, 0.3, 0.005, (0.0, 50.0)).expect("合法落点"),
            (300.0, inset + 50.0),
        );
    }

    /// 落点补偿夹回屏内：四周各留 `EDGE_RETURN_INSET` 换算的点数（1000 点屏 =
    /// 5 点），且**不越过落点所在屏**（即使邻屏在行进方向上）。
    #[test]
    fn compensated_release_point_clamps_inside_landing_screen() {
        let rects = rects_2x1();
        let inset = f64::from(xcontrol_core::session::EDGE_RETURN_INSET) * 1000.0;
        let far = (100_000.0, 100_000.0);
        let back = (-100_000.0, -100_000.0);
        // 四周留白 = 内缩量换算的点数（左、上）。
        assert_point_close(
            compensated_release_point(&rects, 0, 0.005, 0.4, back).expect("合法落点"),
            (inset, inset),
        );
        // 右、下。
        assert_point_close(
            compensated_release_point(&rects, 0, 0.005, 0.4, far).expect("合法落点"),
            (1000.0 - inset, 1000.0 - inset),
        );
        // 副屏：夹在副屏自己的矩形内（不越到主屏，也不出屏）。
        assert_point_close(
            compensated_release_point(&rects, 1, 0.995, 0.6, back).expect("合法落点"),
            (1000.0 + inset, inset),
        );
        // 落点在屏 0、邻屏 1 在右侧行进方向上：不越过屏 0 右缘。
        assert_point_close(
            compensated_release_point(&rects, 0, 0.005, 0.4, (2000.0, 0.0)).expect("合法落点"),
            (1000.0 - inset, 400.0),
        );
        // 留白与 core 回切落点内缩同源：常量改了这里跟着变（不写死 5）。
        assert_eq!(xcontrol_core::session::EDGE_RETURN_INSET, 0.005);
    }

    /// 落点补偿的防御面：屏下标越界 / 非有限归一化坐标 → `None`（不重放，同
    /// `release_warp_point`）；行程非有限 / 屏几何非正 → 退回**未补偿**的原落点
    /// （不 panic、不产生 NaN 夹取）。
    #[test]
    fn compensated_release_point_degrades_to_uncompensated_landing() {
        let rects = rects_2x1();
        // 越界屏 / 非有限坐标：None。
        assert_eq!(
            compensated_release_point(&rects, 2, 0.005, 0.4, (10.0, 0.0)),
            None
        );
        assert_eq!(
            compensated_release_point(&rects, 0, f32::NAN, 0.4, (10.0, 0.0)),
            None
        );
        assert_eq!(
            compensated_release_point(&rects, 0, 0.005, f32::INFINITY, (10.0, 0.0)),
            None
        );
        // 非有限行程：原落点（不补偿）。
        let base = release_warp_point(&rects, 0, 0.005, 0.4);
        assert_eq!(
            compensated_release_point(&rects, 0, 0.005, 0.4, (f32::NAN, 0.0)),
            base
        );
        assert_eq!(
            compensated_release_point(&rects, 0, 0.005, 0.4, (0.0, f32::INFINITY)),
            base
        );
        // 几何非正：原落点。
        let degenerate = vec![ScreenRect {
            origin: (0.0, 0.0),
            size: (0.0, 1000.0),
        }];
        assert_eq!(
            compensated_release_point(&degenerate, 0, 0.005, 0.4, (100.0, 100.0)),
            release_warp_point(&degenerate, 0, 0.005, 0.4),
            "零尺寸屏不夹取、不 panic"
        );
        let nan_origin = vec![ScreenRect {
            origin: (f64::NAN, 0.0),
            size: (1000.0, 1000.0),
        }];
        // NaN 原点：原落点本身 x = NaN（既有行为）——只断言未 panic 且未被夹取、
        // 未被补偿（y 仍是原落点的 0.4×1000，不是 0.4×1000+100）。
        let landed = compensated_release_point(&nan_origin, 0, 0.005, 0.4, (100.0, 100.0))
            .expect("非有限几何仍返回原落点");
        assert!(
            landed.0.is_nan() && (landed.1 - 400.0).abs() < 1e-3,
            "非有限原点不夹取、不补偿（f64::clamp 遇 NaN 边界会 panic）：{landed:?}"
        );
    }

    // —— 平台接线（macos；默认测试全绿不受影响） ——

    /// 本机屏幕布局 → 传感器构建冒烟（纯查询，无需辅助功能权限；默认可跑）。
    /// 断言「至少一屏 → Some」；多屏本机的邻边数打印留档（实机人工验收输入）。
    #[test]
    #[cfg(feature = "macos")]
    fn host_sensor_builds_from_real_displays() {
        let Some(sensor) = host_edge_sensor(DEFAULT_POLL_INTERVAL, &[]) else {
            panic!("本机显示器查询失败或无屏幕（active/online 兜底后仍为空）");
        };
        let layout = sensor.layout();
        println!(
            "本机边缘传感器：{} 屏，邻边 {:?}",
            layout.len(),
            layout.screens()
        );
        assert!(!layout.is_empty());
    }

    /// 真实光标轮询冒烟（#[ignore]，PRD AC：打印触边事件，不强制真实切换）。
    ///
    /// 运行：`cargo test -p xcontrol-app --features macos -- --ignored`
    /// 行为：10s 内按默认节拍（10ms，11-24 起）读真实光标并打印命中——人工把光标推向屏幕
    /// 邻边（多屏主控）观察触发/去抖/重新武装；单屏主控无邻边，仅验证轮询
    /// 与查询路径不崩溃。会话级 `EdgeCrossing` 上报见 session_rt（门控语义）。
    #[test]
    #[ignore = "真实光标冒烟（10s）：cargo test -p xcontrol-app --features macos -- --ignored"]
    fn real_cursor_smoke_prints_edge_hits() {
        let Some(mut sensor) = host_edge_sensor(DEFAULT_POLL_INTERVAL, &[]) else {
            println!("无屏幕（异常环境），跳过冒烟");
            return;
        };
        let ticks = 1000; // 1000 × 10ms（DEFAULT_POLL_INTERVAL）= 10s 观察窗
        for tick in 0..ticks {
            if let Some(hit) = sensor.poll_cursor() {
                println!("[{tick:03}] 触边：{hit:?}");
            }
            std::thread::sleep(DEFAULT_POLL_INTERVAL);
        }
        println!("冒烟完成（{ticks} 拍）——触边事件已实时打印");
    }
}
