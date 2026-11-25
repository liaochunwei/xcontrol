//! 屏幕布局模型与边缘/坐标纯函数（FR-1.3 / FR-2.7，docs/03 §5，docs/02 §4.2 kind 0x10/0x04）。
//!
//! 布局模型：每屏是布局图中的节点，邻居关系由 [`ScreenInfo`](xcontrol_proto::ScreenInfo)
//! 的 `left/right/top/bottom` 显式声明（**不强制几何精确对齐**，几何模式为 P2）。
//! 因此边缘判定是纯图查询：光标归一化坐标触边（`≤0` / `≥1`）且该方向存在邻居 → 产生
//! [`EdgeCrossing`]（`EnterScreen` 帧所需的 target/edge/ratio 全部字段）；命中边的
//! 沿边坐标落在**角落带**（每屏四角、沿屏宽与屏高各取 [`CORNER_RATIO`] 的矩形，
//! 11-24）即排除——角落留给系统热区（docs/03 §5）。
//!
//! 坐标换算：[`to_abs`] 把归一化坐标换算为目标屏幕物理像素（含缩放提示
//! `scale = 逻辑/物理像素比`，1.0 = 无缩放）。
//!
//! 全部函数无状态、无 I/O、对非法输入（NaN/inf/非法 scale）返回 `None` 而非 panic。

use xcontrol_config::LayoutScreen;
use xcontrol_proto::ScreenInfo;
use xcontrol_proto::input::{ScreenEdge, ScreenId};

/// 热键切换闭环的进入点比例（docs/03 §5 热键闭环：居中进入；可配置重写属后续里程碑）。
pub const SWITCH_ENTRY_RATIO: f32 = 0.5;

/// 角落区比例（11-24，docs/03 §5 角落排除）：每屏四角、沿屏宽与屏高各取本比例
/// 的矩形为**角落区**。沿边坐标落在 `[0, CORNER_RATIO] ∪ [1 − CORNER_RATIO, 1]`
/// （含端点）即角落带。每条边两端各 5% 不可切换，中间 90% 可用（用户选定：
/// 4K 约 192×108px，Mac 约 76×49pt）。
///
/// 取比例而非固定像素：主控判定工作机回切时只有归一化坐标，不知道工作机
/// 分辨率。切入（[`edge_crossing`]）与回切（`session` 的 `edge_return_actions`）
/// 两处判定面**只经本常量**——改这里即两处同步，判定处不得另写比例。
pub const CORNER_RATIO: f32 = 0.05;

/// 沿边坐标是否落在角落带（11-24，docs/03 §5 角落排除）：`v ≤ CORNER_RATIO`
/// 或 `v ≥ 1 − CORNER_RATIO`（含端点）。非有限值（NaN）两比较皆假 → `false`
/// ——调用方已先行拒绝非有限输入，本函数不做防御性收敛。
#[must_use]
pub const fn in_corner_band(v: f32) -> bool {
    v <= CORNER_RATIO || v >= 1.0 - CORNER_RATIO
}

/// 屏幕布局图（`CtrlMsg::Layout` 的缓存形态）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScreenLayout {
    screens: Vec<ScreenInfo>,
}

impl ScreenLayout {
    /// 空布局（握手后、收到 `Layout` 前）。
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            screens: Vec::new(),
        }
    }

    /// 由 `CtrlMsg::Layout` 的 screens 构造。
    #[must_use]
    pub const fn new(screens: Vec<ScreenInfo>) -> Self {
        Self { screens }
    }

    /// 布局条目（含主控屏）。
    #[must_use]
    pub const fn screens(&self) -> &Vec<ScreenInfo> {
        &self.screens
    }

    /// 屏幕数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.screens.len()
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.screens.is_empty()
    }

    /// 按 id 查屏。
    #[must_use]
    pub fn screen(&self, id: ScreenId) -> Option<&ScreenInfo> {
        self.screens.iter().find(|s| s.id == id)
    }

    /// `id` 屏在 `edge` 方向的邻居（显式邻居图查询；无边/无此屏 → `None`）。
    #[must_use]
    pub fn neighbor(&self, id: ScreenId, edge: ScreenEdge) -> Option<ScreenId> {
        let screen = self.screen(id)?;
        match edge {
            ScreenEdge::Left => screen.left,
            ScreenEdge::Right => screen.right,
            ScreenEdge::Top => screen.top,
            ScreenEdge::Bottom => screen.bottom,
        }
    }

    /// 属于 `owner` 的屏 id（按布局条目出现序；从机映射表构建用，docs/02 §4.2
    /// 「ScreenId 跨机语义」：owner 匹配 + 出现序 → 本机显示枚举序）。
    #[must_use]
    pub fn ids_owned_by(&self, owner: &[u8; 16]) -> Vec<ScreenId> {
        self.screens
            .iter()
            .filter(|screen| &screen.owner == owner)
            .map(|screen| screen.id)
            .collect()
    }
}

/// 边缘穿越判定结果（`EnterScreen` 帧全部字段）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeCrossing {
    /// 目标屏幕（邻居 id）。
    pub target: ScreenId,
    /// 命中的边缘。
    pub edge: ScreenEdge,
    /// 命中点在该边的归一化位置（已 clamp 到 0.0–1.0）。
    pub ratio: f32,
}

/// 边缘穿越判定（纯函数，docs/03 §5）。
///
/// 给定当前屏 `current` 内的归一化光标 `(x, y)`：
/// - `x ≤ 0.0` 且左侧有邻居 → 左边缘触发；`x ≥ 1.0` 且右侧有邻居 → 右边缘；
///   `y` 同理（上/下）。触边即触发（±0px 语义），越界量不影响；
/// - **角落排除**（11-22 第三轮，docs/03 §5；11-24 由"两轴同为端点"改为角落区）：
///   命中边的**沿边坐标**落在角落带（[`in_corner_band`]，每屏四角、沿屏宽与
///   屏高各取 [`CORNER_RATIO`] 的矩形）时不触发——角落留给系统热区（KDE 触发角
///   专用，边缘=切换）。垂直边看沿边坐标 y，水平边看 x；角落区外的边照常触发。
///   原实现要求两轴同为端点，每个角落只有约 1 点见方，光标斜着进角落几乎必然
///   先在角落外贴到可切换的边；
/// - 同一位置多边越界时按 Left → Right → Top → Bottom 固定优先级取第一个
///   （角落排除后多边候选只剩角落——固定排除，优先级保留为防御顺序）；
/// - `ratio` 取沿边方向的另一坐标并 clamp 到 `[0, 1]`；
/// - 坐标非有限值（NaN/±inf）视为非法输入返回 `None`（协议对浮点字段的拒绝语义）；
/// - 无邻居 / 当前屏不在布局中 → `None`（边界临时摘除，FR-2.8 锁屏边界的图侧表达）。
#[must_use]
pub fn edge_crossing(
    layout: &ScreenLayout,
    current: ScreenId,
    x: f32,
    y: f32,
) -> Option<EdgeCrossing> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    // 角落区判定（11-24，docs/03 §5）：命中边的沿边坐标落在角落带 → 光标在屏角
    // 的角落区内贴边，不切换。垂直边（Left/Right）看 y、水平边（Top/Bottom）
    // 看 x；与 session 回切判定共用 in_corner_band（单一比例出处）。
    if x <= 0.0
        && !in_corner_band(y)
        && let Some(target) = layout.neighbor(current, ScreenEdge::Left)
    {
        return Some(EdgeCrossing {
            target,
            edge: ScreenEdge::Left,
            ratio: clamp01(y),
        });
    }
    if x >= 1.0
        && !in_corner_band(y)
        && let Some(target) = layout.neighbor(current, ScreenEdge::Right)
    {
        return Some(EdgeCrossing {
            target,
            edge: ScreenEdge::Right,
            ratio: clamp01(y),
        });
    }
    if y <= 0.0
        && !in_corner_band(x)
        && let Some(target) = layout.neighbor(current, ScreenEdge::Top)
    {
        return Some(EdgeCrossing {
            target,
            edge: ScreenEdge::Top,
            ratio: clamp01(x),
        });
    }
    if y >= 1.0
        && !in_corner_band(x)
        && let Some(target) = layout.neighbor(current, ScreenEdge::Bottom)
    {
        return Some(EdgeCrossing {
            target,
            edge: ScreenEdge::Bottom,
            ratio: clamp01(x),
        });
    }
    None
}

/// 物理像素坐标（逻辑像素 ÷ scale 后取整到最近像素）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelPoint {
    /// 物理像素 X。
    pub x: f32,
    /// 物理像素 Y。
    pub y: f32,
}

/// 归一化坐标 → 目标屏幕物理像素（纯函数，FR-2.7）。
///
/// 换算：`物理 = 归一化 × 逻辑像素 ÷ scale`（`scale` 为 docs/02 §4.3 的
/// 逻辑/物理像素比：Retina 2x 屏 `scale = 0.5`，无缩放 `scale = 1.0`）。
/// 输入 clamp 到 `[0, 1]`；结果取最近整数像素（平台 warp/注入按物理像素定位）。
///
/// 非法输入返回 `None`：坐标或 `scale` 非有限、`scale ≤ 0`。
#[must_use]
pub fn to_abs(
    x: f32,
    y: f32,
    width_logical_px: u32,
    height_logical_px: u32,
    scale: f32,
) -> Option<PixelPoint> {
    if !x.is_finite() || !y.is_finite() || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let (x, y) = (clamp01(x), clamp01(y));
    Some(PixelPoint {
        x: (x * width_logical_px as f32 / scale).round(),
        y: (y * height_logical_px as f32 / scale).round(),
    })
}

/// 热键切换闭环的进入边推导（docs/03 §5 热键闭环，FR-2.5）：布局图中邻接
/// `target` 的第一条边取**对侧边**（主控从该边进入 target——target 在锚点屏
/// 右侧即其左缘进入，下侧即其上缘进入）。无邻边 → `None`（调用方回退 Next
/// 语义 = target 左缘）。同 id 多处邻接时按布局条目出现序取第一条（确定性）。
#[must_use]
pub fn entry_edge_into(layout: &ScreenLayout, target: ScreenId) -> Option<ScreenEdge> {
    for screen in layout.screens() {
        if screen.right == Some(target) {
            return Some(ScreenEdge::Left);
        }
        if screen.left == Some(target) {
            return Some(ScreenEdge::Right);
        }
        if screen.bottom == Some(target) {
            return Some(ScreenEdge::Top);
        }
        if screen.top == Some(target) {
            return Some(ScreenEdge::Bottom);
        }
    }
    None
}

/// 外接（从机）屏布局合并（FR-1.3 方位制最小模型，docs/01 §5 `[layout]`）。
///
/// - 外接条目 id 从本机屏数起连续分配（ScreenId 全局空间，docs/02 §4.2），
///   `owner` 记 `externals_owner`（连接对端的 instance id）；本机条目 `owner`
///   统一记 `local_owner`（从机据此排除主控屏）；
/// - 邻接边：条目在锚点屏 `direction` 方向（`Direction::Left` = 条目在锚点
///   左侧）→ 锚点的该方向边连向条目，条目取**对侧边**回连锚点。锚点解析：
///   `relative_to` 匹配本机屏名 → 该屏；匹配既有外接条目名 → 该条目（外接
///   条目可相互引用）；缺省/不匹配 → 本机主屏（id 0）；
/// - 同一锚点同方向多条目按配置顺序**链式**排列（沿方向走到链尾再挂接），
///   避免「右侧已有邻居仍硬挂」覆盖既有边；
/// - 缺 `direction` 的条目（config 校验允许的独立锚点预留形态）不进路由图，
///   跳过不分配 id。
#[must_use]
pub fn merge_layout(
    local: &ScreenLayout,
    local_owner: [u8; 16],
    externals_owner: [u8; 16],
    externals: &[LayoutScreen],
) -> ScreenLayout {
    let mut screens = local.screens().clone();
    for screen in &mut screens {
        screen.owner = local_owner;
    }
    // 外接条目名 → 已分配 id（支持 relative_to 引用既有外接条目）。
    let mut assigned: Vec<(String, ScreenId)> = Vec::new();
    for entry in externals {
        // 屏数 << u16::MAX；饱和取 MAX 属防御（多屏 65535 块不可达）。
        let Some(direction) = entry.direction else {
            continue;
        };
        let id = u16::try_from(screens.len()).unwrap_or(ScreenId::MAX);
        let anchor_edge = direction_edge(direction);
        // 锚点解析（见函数文档）：本机屏名 → 外接条目名 → 主屏 0。
        let mut anchor = entry
            .relative_to
            .as_ref()
            .and_then(|name| {
                screens
                    .iter()
                    .find(|screen| &screen.name == name)
                    .map(|screen| screen.id)
            })
            .or_else(|| {
                entry.relative_to.as_ref().and_then(|name| {
                    assigned
                        .iter()
                        .rev()
                        .find(|(assigned_name, _)| assigned_name == name)
                        .map(|(_, id)| *id)
                })
            })
            .unwrap_or(0);
        // 同锚点同方向链式挂接：沿方向走到链尾。
        while let Some(next) = edge_of(&screens, anchor, anchor_edge) {
            anchor = next;
        }
        // 新条目：对侧边回连锚点（条目在锚点 direction 方向 → 回连边为对侧）。
        let entry_edge = opposite_edge(anchor_edge);
        screens.push(ScreenInfo {
            id,
            owner: externals_owner,
            name: entry.id.clone(),
            left: (entry_edge == ScreenEdge::Left).then_some(anchor),
            right: (entry_edge == ScreenEdge::Right).then_some(anchor),
            top: (entry_edge == ScreenEdge::Top).then_some(anchor),
            bottom: (entry_edge == ScreenEdge::Bottom).then_some(anchor),
            scale: 1.0,
        });
        set_edge(&mut screens, anchor, anchor_edge, id);
        assigned.push((entry.id.clone(), id));
    }
    ScreenLayout::new(screens)
}

/// `screens` 中 `id` 屏在 `edge` 方向的邻居（无此屏/无边 → `None`）。
fn edge_of(screens: &[ScreenInfo], id: ScreenId, edge: ScreenEdge) -> Option<ScreenId> {
    let screen = screens.iter().find(|screen| screen.id == id)?;
    match edge {
        ScreenEdge::Left => screen.left,
        ScreenEdge::Right => screen.right,
        ScreenEdge::Top => screen.top,
        ScreenEdge::Bottom => screen.bottom,
    }
}

/// `screens` 中 `id` 屏在 `edge` 方向写邻居边（无此屏为防御空操作）。
fn set_edge(screens: &mut [ScreenInfo], id: ScreenId, edge: ScreenEdge, neighbor: ScreenId) {
    let Some(screen) = screens.iter_mut().find(|screen| screen.id == id) else {
        return;
    };
    match edge {
        ScreenEdge::Left => screen.left = Some(neighbor),
        ScreenEdge::Right => screen.right = Some(neighbor),
        ScreenEdge::Top => screen.top = Some(neighbor),
        ScreenEdge::Bottom => screen.bottom = Some(neighbor),
    }
}

/// 配置方向 → 锚点侧的边（条目在锚点左侧 → 锚点的左边）。
const fn direction_edge(direction: xcontrol_config::Direction) -> ScreenEdge {
    match direction {
        xcontrol_config::Direction::Left => ScreenEdge::Left,
        xcontrol_config::Direction::Right => ScreenEdge::Right,
        xcontrol_config::Direction::Up => ScreenEdge::Top,
        xcontrol_config::Direction::Down => ScreenEdge::Bottom,
    }
}

/// 方向的对侧边（Left↔Right、Top↔Bottom）。
const fn opposite_edge(edge: ScreenEdge) -> ScreenEdge {
    match edge {
        ScreenEdge::Left => ScreenEdge::Right,
        ScreenEdge::Right => ScreenEdge::Left,
        ScreenEdge::Top => ScreenEdge::Bottom,
        ScreenEdge::Bottom => ScreenEdge::Top,
    }
}

/// clamp 到 [0, 1]（NaN 不会到达此处——调用方已先行过滤）。
const fn clamp01(v: f32) -> f32 {
    if v < 0.0 {
        0.0
    } else if v > 1.0 {
        1.0
    } else {
        v
    }
}

/// 2×1 双屏 fixture（docs/03 §5 路由测试最小组）：A 右邻 B，B 左邻 A。
#[cfg(test)]
fn layout_2x1() -> ScreenLayout {
    ScreenLayout::new(vec![
        ScreenInfo {
            id: 0,
            owner: [1; 16],
            name: "A".into(),
            left: None,
            right: Some(1),
            top: None,
            bottom: None,
            scale: 1.0,
        },
        ScreenInfo {
            id: 1,
            owner: [2; 16],
            name: "B".into(),
            left: Some(0),
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        },
    ])
}

/// L 形 3 屏 fixture：A(0) 右邻 B(1)、下邻 C(2)；B 左邻 A；C 上邻 A。
#[cfg(test)]
fn layout_l_shape() -> ScreenLayout {
    ScreenLayout::new(vec![
        ScreenInfo {
            id: 0,
            owner: [1; 16],
            name: "A".into(),
            left: None,
            right: Some(1),
            top: None,
            bottom: Some(2),
            scale: 1.0,
        },
        ScreenInfo {
            id: 1,
            name: "B".into(),
            owner: [1; 16],
            left: Some(0),
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        },
        ScreenInfo {
            id: 2,
            owner: [2; 16],
            name: "C".into(),
            left: None,
            right: None,
            top: Some(0),
            bottom: None,
            scale: 1.0,
        },
    ])
}

/// 十字 5 屏 fixture：A(0) 四边各有邻居——右 B(1)、左 C(2)、下 D(3)、上 E(4)
/// （切入侧四条边的角落带边界各验一遍用）。
#[cfg(test)]
fn layout_cross() -> ScreenLayout {
    ScreenLayout::new(vec![
        ScreenInfo {
            id: 0,
            owner: [1; 16],
            name: "A".into(),
            left: Some(2),
            right: Some(1),
            top: Some(4),
            bottom: Some(3),
            scale: 1.0,
        },
        ScreenInfo {
            id: 1,
            owner: [2; 16],
            name: "B".into(),
            left: Some(0),
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        },
        ScreenInfo {
            id: 2,
            owner: [2; 16],
            name: "C".into(),
            left: None,
            right: Some(0),
            top: None,
            bottom: None,
            scale: 1.0,
        },
        ScreenInfo {
            id: 3,
            owner: [2; 16],
            name: "D".into(),
            left: None,
            right: None,
            top: Some(0),
            bottom: None,
            scale: 1.0,
        },
        ScreenInfo {
            id: 4,
            owner: [2; 16],
            name: "E".into(),
            left: None,
            right: None,
            top: None,
            bottom: Some(0),
            scale: 1.0,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CORNER_RATIO` 与 `in_corner_band`（11-24，AC9）：比例 = 5%；角落带 =
    /// 沿边坐标两端各 5%（含端点），带外中段为 false。边界值相对常量书写
    /// （`± 0.001`），不测精确端点。
    #[test]
    fn corner_band_is_five_percent_at_each_end() {
        assert!((CORNER_RATIO - 0.05).abs() < 1e-6, "角落区比例 = 5%");
        // 带内：两端（含越界外推样本）。
        assert!(in_corner_band(0.0));
        assert!(in_corner_band(1.0));
        assert!(in_corner_band(CORNER_RATIO - 0.001));
        assert!(in_corner_band(1.0 - CORNER_RATIO + 0.001));
        assert!(in_corner_band(-0.5), "屏角外推样本同样算带内");
        assert!(in_corner_band(1.5), "屏角外推样本同样算带内");
        // 带外：中段。
        assert!(!in_corner_band(CORNER_RATIO + 0.001));
        assert!(!in_corner_band(1.0 - CORNER_RATIO - 0.001));
        assert!(!in_corner_band(0.25));
        assert!(!in_corner_band(0.5));
        assert!(!in_corner_band(0.75));
    }

    /// neighbor() 四方向查询：有边/无边/无此屏三态。
    #[test]
    fn neighbor_lookup_three_states() {
        let layout = layout_l_shape();
        assert_eq!(layout.neighbor(0, ScreenEdge::Right), Some(1));
        assert_eq!(layout.neighbor(0, ScreenEdge::Bottom), Some(2));
        assert_eq!(layout.neighbor(0, ScreenEdge::Left), None);
        assert_eq!(layout.neighbor(0, ScreenEdge::Top), None);
        assert_eq!(layout.neighbor(1, ScreenEdge::Left), Some(0));
        assert_eq!(layout.neighbor(2, ScreenEdge::Top), Some(0));
        assert_eq!(layout.neighbor(9, ScreenEdge::Right), None); // 无此屏
        assert_eq!(layout.len(), 3);
        assert!(ScreenLayout::empty().is_empty());
        assert_eq!(ScreenLayout::empty().len(), 0);
    }

    /// 2×1 fixture：四边穿越与精确触边边界（±0px 语义：0.0/1.0 触发，内侧不触发）。
    #[test]
    fn edge_crossing_2x1_boundaries() {
        let layout = layout_2x1();
        // A 右缘触发 → B，ratio = y
        let hit = edge_crossing(&layout, 0, 1.0, 0.5);
        assert_eq!(
            hit,
            Some(EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5
            })
        );
        // 内侧不触发
        assert_eq!(edge_crossing(&layout, 0, 0.999, 0.5), None);
        assert_eq!(edge_crossing(&layout, 0, 0.5, 0.999), None);
        assert_eq!(edge_crossing(&layout, 0, 0.5, 0.001), None);
        assert_eq!(edge_crossing(&layout, 0, 0.001, 0.5), None);
        // A 左/上/下无边 → 不触发
        assert_eq!(edge_crossing(&layout, 0, 0.0, 0.5), None);
        assert_eq!(edge_crossing(&layout, 0, 0.5, 0.0), None);
        assert_eq!(edge_crossing(&layout, 0, 0.5, 1.0), None);
        // B 左缘 → A
        assert_eq!(
            edge_crossing(&layout, 1, 0.0, 0.25),
            Some(EdgeCrossing {
                target: 0,
                edge: ScreenEdge::Left,
                ratio: 0.25
            })
        );
        // B 右/上/下无边
        assert_eq!(edge_crossing(&layout, 1, 1.0, 0.5), None);
        assert_eq!(edge_crossing(&layout, 1, 0.5, 0.0), None);
        assert_eq!(edge_crossing(&layout, 1, 0.5, 1.0), None);
    }

    /// L 形 3 屏：A→B/A→C/B→A/C→A 路由全部正确（沿边坐标取严格内侧值——
    /// 角落区已排除，见 edge_crossing_corner_exclusion）；死角（B 下缘、C 右缘）
    /// 无边不触发。
    #[test]
    fn edge_crossing_l_shape() {
        let layout = layout_l_shape();
        assert_eq!(
            edge_crossing(&layout, 0, 1.0, 0.25),
            Some(EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.25
            })
        );
        assert_eq!(
            edge_crossing(&layout, 0, 0.5, 1.0),
            Some(EdgeCrossing {
                target: 2,
                edge: ScreenEdge::Bottom,
                ratio: 0.5
            })
        );
        assert_eq!(
            edge_crossing(&layout, 1, 0.0, 0.75),
            Some(EdgeCrossing {
                target: 0,
                edge: ScreenEdge::Left,
                ratio: 0.75
            })
        );
        assert_eq!(
            edge_crossing(&layout, 2, 0.25, 0.0),
            Some(EdgeCrossing {
                target: 0,
                edge: ScreenEdge::Top,
                ratio: 0.25
            })
        );
        // 死角：B 上/下缘、C 左/右/下缘均无边
        assert_eq!(edge_crossing(&layout, 1, 0.5, 1.0), None);
        assert_eq!(edge_crossing(&layout, 1, 0.5, 0.0), None);
        assert_eq!(edge_crossing(&layout, 2, 0.0, 0.5), None);
        assert_eq!(edge_crossing(&layout, 2, 1.0, 0.5), None);
        assert_eq!(edge_crossing(&layout, 2, 0.5, 1.0), None);
    }

    /// 角落排除（11-22 第三轮，docs/03 §5；11-24 由"两轴同为端点"改为 5% 角落区）：
    /// 光标在角落区内贴边不触发任何边——角落留给系统热区（KDE 触发角专用，
    /// 边缘=切换）。四个角点各一；**角落带内但并非角点**的样本（如右缘
    /// y=0.03——旧实现下"y=0.001 未收敛 → 触发"）同样排除；沿边中段照常触发。
    /// 角落带边界（`CORNER_RATIO ± 0.001`）见
    /// `edge_crossing_corner_band_boundaries_on_all_four_edges`。
    #[test]
    fn edge_crossing_corner_exclusion() {
        let layout = layout_l_shape(); // A(0) 右邻 B(1)、下邻 C(2)
        // A 右上角（右缘有邻居）：角落 → None
        assert_eq!(edge_crossing(&layout, 0, 1.0, 0.0), None);
        // A 右下角（右/下缘均有邻居，Right 优先候选）：角落 → None
        assert_eq!(edge_crossing(&layout, 0, 1.0, 1.0), None);
        // A 左下角（下缘有邻居）：角落 → None
        assert_eq!(edge_crossing(&layout, 0, 0.0, 1.0), None);
        // A 左上角（两缘均无邻居）：角落 → None
        assert_eq!(edge_crossing(&layout, 0, 0.0, 0.0), None);
        // 右上角落带内的非角点：同样排除（旧实现下 0.001 未收敛会触发）。
        assert_eq!(
            edge_crossing(&layout, 0, 1.0, 0.001),
            None,
            "右上角带内（近角点）"
        );
        assert_eq!(edge_crossing(&layout, 0, 1.0, 0.03), None, "右上角带内中点");
        // 右下角带内 / 下缘的左右角落带内。
        assert_eq!(edge_crossing(&layout, 0, 1.0, 0.97), None, "右下角带内");
        assert_eq!(edge_crossing(&layout, 0, 0.04, 1.0), None, "下缘左角带内");
        assert_eq!(edge_crossing(&layout, 0, 0.96, 1.0), None, "下缘右角带内");
        // 角落区外回归：沿边中段 → 照常触发，ratio = 沿边坐标。
        assert_eq!(
            edge_crossing(&layout, 0, 1.0, 0.5),
            Some(EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5
            }),
            "右缘中点触发"
        );
        assert_eq!(
            edge_crossing(&layout, 0, 0.5, 1.0),
            Some(EdgeCrossing {
                target: 2,
                edge: ScreenEdge::Bottom,
                ratio: 0.5
            }),
            "下缘中点触发"
        );
    }

    /// 角落带边界（11-24，AC9）——切入侧**四条边各验**：沿边坐标落在角落带内
    /// （`CORNER_RATIO − 0.001`、`1 − CORNER_RATIO + 0.001`）贴边不触发；带外
    /// （`CORNER_RATIO + 0.001`、`1 − CORNER_RATIO − 0.001`）照常触发，ratio =
    /// 沿边坐标原值。边界值相对常量书写，不测精确端点。
    #[test]
    fn edge_crossing_corner_band_boundaries_on_all_four_edges() {
        let layout = layout_cross(); // A(0)：右 B(1)、左 C(2)、上 E(4)、下 D(3)
        let (in_low, out_low) = (CORNER_RATIO - 0.001, CORNER_RATIO + 0.001);
        let (out_high, in_high) = (1.0 - CORNER_RATIO - 0.001, 1.0 - CORNER_RATIO + 0.001);
        // A 屏 `edge` 边上、沿边坐标 = `along` 的样本 → (命中边, 目标, ratio)。
        let hit = |edge: ScreenEdge, along: f32| {
            match edge {
                ScreenEdge::Left => edge_crossing(&layout, 0, 0.0, along),
                ScreenEdge::Right => edge_crossing(&layout, 0, 1.0, along),
                ScreenEdge::Top => edge_crossing(&layout, 0, along, 0.0),
                ScreenEdge::Bottom => edge_crossing(&layout, 0, along, 1.0),
            }
            .map(|hit| (hit.edge, hit.target, hit.ratio))
        };

        for (edge, target) in [
            (ScreenEdge::Left, 2),
            (ScreenEdge::Right, 1),
            (ScreenEdge::Top, 4),
            (ScreenEdge::Bottom, 3),
        ] {
            // 角落带外：四条边照常触发。
            assert_eq!(
                hit(edge, out_low),
                Some((edge, target, out_low)),
                "{edge:?} 下端角落带外应触发"
            );
            assert_eq!(
                hit(edge, out_high),
                Some((edge, target, out_high)),
                "{edge:?} 上端角落带外应触发"
            );
            // 角落带内：四条边都不触发。
            assert_eq!(hit(edge, in_low), None, "{edge:?} 下端角落带内不触发");
            assert_eq!(hit(edge, in_high), None, "{edge:?} 上端角落带内不触发");
        }
    }

    /// ratio clamp：越界沿边坐标收敛到 [0,1]（角落区已排除——沿边坐标越界
    /// 即角落，不触发；clamp 保留为防御面）；非有限坐标拒绝。
    #[test]
    fn edge_crossing_clamps_ratio_and_rejects_non_finite() {
        let layout = layout_2x1();
        // 沿边坐标内侧：ratio = 原值
        let hit = edge_crossing(&layout, 0, 1.2, 0.25);
        assert_eq!(hit.map(|h| h.ratio), Some(0.25));
        // 非有限输入
        assert_eq!(edge_crossing(&layout, 0, f32::NAN, 0.5), None);
        assert_eq!(edge_crossing(&layout, 0, f32::INFINITY, 0.5), None);
        assert_eq!(edge_crossing(&layout, 0, 0.5, f32::NEG_INFINITY), None);
        // 多边同时越界 = 角落区：固定排除（Left > Right > Top > Bottom 优先级
        // 不再参与——任何双轴极值组合都不触发）
        assert_eq!(edge_crossing(&layout, 1, -0.1, 1.1), None);
    }

    /// to_abs：无缩放精确换算 + Retina（scale=0.5）翻倍 + 越界 clamp。
    #[test]
    fn to_abs_scales_and_clamps() {
        let p = to_abs(0.5, 0.25, 1920, 1080, 1.0);
        assert_eq!(p, Some(PixelPoint { x: 960.0, y: 270.0 }));
        // scale = 逻辑/物理：0.5 → 物理翻倍
        let p = to_abs(0.5, 0.5, 1920, 1080, 0.5);
        assert_eq!(
            p,
            Some(PixelPoint {
                x: 1920.0,
                y: 1080.0
            })
        );
        // 越界 clamp
        let p = to_abs(-1.0, 1.5, 100, 100, 1.0);
        assert_eq!(p, Some(PixelPoint { x: 0.0, y: 100.0 }));
        // 四舍五入到最近像素
        let p = to_abs(0.333, 0.0, 100, 100, 1.0);
        assert_eq!(p, Some(PixelPoint { x: 33.0, y: 0.0 }));
    }

    /// to_abs 非法输入：非有限坐标、非有限/非正 scale → None。
    #[test]
    fn to_abs_rejects_invalid_input() {
        assert_eq!(to_abs(f32::NAN, 0.5, 100, 100, 1.0), None);
        assert_eq!(to_abs(0.5, f32::INFINITY, 100, 100, 1.0), None);
        assert_eq!(to_abs(0.5, 0.5, 100, 100, 0.0), None);
        assert_eq!(to_abs(0.5, 0.5, 100, 100, -1.0), None);
        assert_eq!(to_abs(0.5, 0.5, 100, 100, f32::NAN), None);
    }

    /// ScreenLayout 的 PartialEq/Clone 可用（会话缓存 Layout 消息需要）。
    #[test]
    fn layout_equality() {
        assert_eq!(layout_2x1(), layout_2x1());
        assert_ne!(layout_2x1(), layout_l_shape());
        let copy = layout_2x1().clone();
        assert_eq!(copy.screen(1).map(|s| s.name.as_str()), Some("B"));
    }

    /// entry_edge_into（FR-2.5 热键闭环进入边推导）：四方向邻接取对侧边；
    /// 无邻接 → None；多屏邻接同 target 按条目出现序取第一条。
    #[test]
    fn entry_edge_into_opposite_of_link() {
        // 2×1：A 右邻 B → 进入 B = 左缘；B 左邻 A → 进入 A = 右缘。
        let layout = layout_2x1();
        assert_eq!(entry_edge_into(&layout, 1), Some(ScreenEdge::Left));
        assert_eq!(entry_edge_into(&layout, 0), Some(ScreenEdge::Right));
        assert_eq!(entry_edge_into(&layout, 9), None);
        // L 形：C 在 A 下方（A.bottom = C）→ 进入 C = 上缘。
        let layout = layout_l_shape();
        assert_eq!(entry_edge_into(&layout, 2), Some(ScreenEdge::Top));
        // 空布局恒 None。
        assert_eq!(entry_edge_into(&ScreenLayout::empty(), 0), None);
    }

    /// ids_owned_by：owner 过滤 + 出现序（docs/02 §4.2 从机映射表构建输入）。
    #[test]
    fn ids_owned_by_filters_and_preserves_order() {
        let layout = layout_l_shape();
        assert_eq!(layout.ids_owned_by(&[1; 16]), vec![0, 1]);
        assert_eq!(layout.ids_owned_by(&[2; 16]), vec![2]);
        assert!(layout.ids_owned_by(&[3; 16]).is_empty());
    }

    /// merge_layout（FR-1.3 方位制）：缺省锚点 = 本机主屏；方向语义 = 条目在
    /// 锚点的该方向（左条目 → 锚点左边连出、条目右缘回连）；id 连续分配；
    /// owner 双侧分别落值；无 direction 条目跳过。
    #[test]
    fn merge_layout_wires_directions_and_owners() {
        let local = ScreenLayout::new(vec![ScreenInfo {
            id: 0,
            owner: [1; 16],
            name: "Display 0".into(),
            left: None,
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        }]);
        let merged = merge_layout(
            &local,
            [1; 16],
            [2; 16],
            &[
                LayoutScreen {
                    id: "sofa".into(),
                    relative_to: None,
                    direction: Some(xcontrol_config::Direction::Right),
                },
                LayoutScreen {
                    id: "standalone".into(),
                    relative_to: None,
                    direction: None,
                },
                LayoutScreen {
                    id: "attic".into(),
                    relative_to: Some("sofa".into()),
                    direction: Some(xcontrol_config::Direction::Up),
                },
            ],
        );
        let screens = merged.screens();
        // standalone 无 direction：跳过不分配 id → attic = id 2。
        assert_eq!(screens.len(), 3, "{screens:?}");
        assert_eq!(screens[0].owner, [1; 16], "本机条目 owner = 主控");
        assert_eq!(screens[1].id, 1);
        assert_eq!(screens[1].name, "sofa");
        assert_eq!(screens[1].owner, [2; 16], "外接条目 owner = 从机");
        // 主屏右边连 sofa；sofa 左缘回连主屏。
        assert_eq!(screens[0].right, Some(1));
        assert_eq!(screens[1].left, Some(0));
        assert_eq!(screens[1].right, None);
        // attic 在 sofa 上方：sofa 上边连 attic；attic 下缘回连 sofa。
        assert_eq!(screens[2].id, 2);
        assert_eq!(screens[2].name, "attic");
        assert_eq!(screens[1].top, Some(2));
        assert_eq!(screens[2].bottom, Some(1));
        assert_eq!(screens[2].top, None);
        // 路由查询一致：进入 sofa（从主屏右侧）= 左缘。
        assert_eq!(entry_edge_into(&merged, 1), Some(ScreenEdge::Left));
        assert_eq!(entry_edge_into(&merged, 2), Some(ScreenEdge::Bottom));
    }

    /// merge_layout 链式挂接：同锚点同方向两条目按配置顺序串成链（不覆盖既有边）。
    #[test]
    fn merge_layout_chains_same_direction() {
        let local = ScreenLayout::new(vec![ScreenInfo {
            id: 0,
            owner: [1; 16],
            name: "Display 0".into(),
            left: None,
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        }]);
        let merged = merge_layout(
            &local,
            [1; 16],
            [2; 16],
            &[
                LayoutScreen {
                    id: "r1".into(),
                    relative_to: None,
                    direction: Some(xcontrol_config::Direction::Right),
                },
                LayoutScreen {
                    id: "r2".into(),
                    relative_to: None,
                    direction: Some(xcontrol_config::Direction::Right),
                },
            ],
        );
        let screens = merged.screens();
        assert_eq!(screens[0].right, Some(1), "主屏右边 = 链首");
        assert_eq!(screens[1].right, Some(2), "链首右边 = 第二条（链式）");
        assert_eq!(screens[1].left, Some(0));
        assert_eq!(screens[2].left, Some(1));
        assert_eq!(screens[2].right, None, "链尾右边无边");
    }

    /// 本机多屏 + 外接条目：锚点可按本机屏名引用（relative_to = 本机屏名）。
    #[test]
    fn merge_layout_anchors_by_local_screen_name() {
        let local = ScreenLayout::new(vec![
            ScreenInfo {
                id: 0,
                owner: [1; 16],
                name: "Display 0".into(),
                left: None,
                right: None,
                top: None,
                bottom: None,
                scale: 1.0,
            },
            ScreenInfo {
                id: 1,
                owner: [1; 16],
                name: "Display 1".into(),
                left: None,
                right: None,
                top: None,
                bottom: None,
                scale: 1.0,
            },
        ]);
        let merged = merge_layout(
            &local,
            [1; 16],
            [2; 16],
            &[LayoutScreen {
                id: "left-of-second".into(),
                relative_to: Some("Display 1".into()),
                direction: Some(xcontrol_config::Direction::Left),
            }],
        );
        let screens = merged.screens();
        assert_eq!(screens[1].left, Some(2), "Display 1 左边连外接条目");
        assert_eq!(screens[2].right, Some(1), "外接条目右缘回连 Display 1");
    }
}
