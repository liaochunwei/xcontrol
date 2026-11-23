//! 托盘图标（`gpui` feature 门控）：2 张程序化生成模板图（docs/06 §5.1 M4 图标
//! 方案裁定）。
//!
//! 裁定（docs/06 §5.1 M4 注记）：M0/docs 原"SVG 源 → 各平台 PNG/模板图"的多态
//! 图标集收敛为 **2 张程序化生成的模板图**——常态为环形加中心点、错误为环形加
//! 感叹号；搜索中/暂停等状态由 tooltip（[`crate::project::TrayState::tooltip`]）
//! 与角标（mac status item title）表达。
//!
//! 多态预渲染图标（SVG 源 → 状态全覆盖）M6 打磨期落地——避免本任务引入 SVG
//! 资产管线；状态可辨识性由 tooltip/角标兜底（FR-6.1 AC 的可辨识主体是 tooltip
//! 与菜单）。
//!
//! macOS：Template Image（alpha 即形状，系统按菜单栏深浅色自动着色）；其余平台
//! 使用固定色（任务栏/通知区自绘着色）。

use gpui_tray::Icon;

use crate::project::IconKind;

/// 图标位图边长（px）。mac 以 `scale_factor(2.0)` 呈现为 18pt——菜单栏模板图
/// 常规尺寸；其余平台按 36px 原始像素呈现（通知区 DPI 适配 M6 随预渲染统一）。
pub const ICON_SIZE: u32 = 36;

/// 非模板平台的常态色（品牌蓝，与 M0 spike 同源）。
const NORMAL_COLOR: [u8; 3] = [45, 105, 220];
/// 非模板平台的错误色（警示红）。
const ERROR_COLOR: [u8; 3] = [214, 69, 69];

/// 常态图标 RGBA（环形 + 中心点）。
#[must_use]
pub fn normal_rgba(size: u32) -> Vec<u8> {
    draw(size, false, NORMAL_COLOR)
}

/// 错误图标 RGBA（环形 + 感叹号）。
#[must_use]
pub fn error_rgba(size: u32) -> Vec<u8> {
    draw(size, true, ERROR_COLOR)
}

/// 图标形态 → gpui-tray 图标（mac 附加 template + 2x scale）。
///
/// # Errors
/// 位图尺寸/字节量校验失败（固定常量下不可达——防御 gpui-tray 契约）。
pub fn icon(kind: IconKind) -> gpui_tray::Result<Icon> {
    let rgba = match kind {
        IconKind::Normal => normal_rgba(ICON_SIZE),
        IconKind::Error => error_rgba(ICON_SIZE),
    };
    let builder = Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE);
    // macOS Template Image：alpha 即形状，深浅色菜单栏自动适配（docs/06 §5.1）。
    // （cfg(target_os) 胶水先例：xcontrol-app ui/mod.rs 的 on_reopen 同形——
    // 渲染呈现细节，不涉平台 API 依赖。）
    #[cfg(target_os = "macos")]
    let builder = builder.scale_factor(2.0).template(true);
    builder.build()
}

/// 形状绘制（归一化坐标；`error` = 感叹号替换中心点）。
fn draw(size: u32, error: bool, color: [u8; 3]) -> Vec<u8> {
    let s = size as f32;
    let c = s / 2.0;
    let (ring_out, ring_in) = (0.44 * s, 0.30 * s);
    let mut rgba = vec![0_u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - c;
            let dy = y as f32 + 0.5 - c;
            let d2 = dx * dx + dy * dy;
            let in_ring = (ring_in * ring_in)..=(ring_out * ring_out);
            let hit = if error {
                // 感叹号：竖条（上段）+ 圆点（下段），全部落在环形内径之内。
                let bar = dx.abs() <= 0.055 * s && (-0.26 * s..=(0.02 * s)).contains(&dy);
                let dot_center_dy = 0.17 * s;
                let dot = dy > 0.0
                    && (dx * dx + (dy - dot_center_dy) * (dy - dot_center_dy))
                        <= (0.06 * s) * (0.06 * s);
                in_ring.contains(&d2) || bar || dot
            } else {
                // 中心点（"已连接"视觉锚；搜索中/暂停共用同一常态图标，状态由
                // tooltip/角标表达）。
                let dot = 0.10 * s;
                in_ring.contains(&d2) || d2 <= dot * dot
            };
            if hit {
                let offset = ((y * size + x) * 4) as usize;
                rgba[offset] = color[0];
                rgba[offset + 1] = color[1];
                rgba[offset + 2] = color[2];
                rgba[offset + 3] = 255;
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(rgba: &[u8], size: u32, x: u32, y: u32) -> u8 {
        let offset = ((y * size + x) * 4) as usize;
        rgba[offset + 3]
    }

    /// 字节量与尺寸契约（gpui-tray `Icon::from_rgba` 校验前置）。
    #[test]
    fn rgba_buffers_match_size_contract() {
        let normal = normal_rgba(ICON_SIZE);
        let error = error_rgba(ICON_SIZE);
        let expected = (ICON_SIZE * ICON_SIZE * 4) as usize;
        assert_eq!(normal.len(), expected);
        assert_eq!(error.len(), expected);
    }

    /// 常态：环上有形、中心有形、环与中心点之间透明、环外透明。
    #[test]
    fn normal_shape_hits_ring_and_dot() {
        let size = 36;
        let rgba = normal_rgba(size);
        let c = 18; // 中心
        assert_eq!(alpha_at(&rgba, size, c, 2), 255, "环外径上有形");
        assert_eq!(alpha_at(&rgba, size, c, c), 255, "中心点有形");
        assert_eq!(
            alpha_at(&rgba, size, c, 11),
            0,
            "环内径(≈10.8)与中心点(≈3.6)之间透明"
        );
        assert_eq!(alpha_at(&rgba, size, 2, 2), 0, "角落透明");
    }

    /// 错误：环上有形、竖条与圆点有形、中心点位移后不再是实心点。
    #[test]
    fn error_shape_hits_ring_bar_and_dot() {
        let size = 36;
        let rgba = error_rgba(size);
        let c = 18;
        assert_eq!(alpha_at(&rgba, size, c, 18 - 16), 255, "环上有形");
        assert_eq!(alpha_at(&rgba, size, c, c - 8), 255, "竖条有形");
        assert_eq!(alpha_at(&rgba, size, c, c + 6), 255, "圆点有形");
        // 竖条与圆点之间的空隙（感叹号的可辨识结构）
        assert_eq!(alpha_at(&rgba, size, c, c + 3), 0, "条点之间留隙");
    }

    /// 常态与错误位图确实不同（图标切换有视觉效果）。
    #[test]
    fn normal_and_error_bitmaps_differ() {
        assert_ne!(normal_rgba(ICON_SIZE), error_rgba(ICON_SIZE));
    }
}
