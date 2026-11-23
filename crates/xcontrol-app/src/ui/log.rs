//! 日志页渲染：tracing Layer 桥接的环形缓冲（[`LogBuffer`]）滚动显示。
//!
//! 纯投影：入参只有行快照（`&[String]`，旧→新序）+ 滚动句柄；渲染环形缓冲
//! 全量（上限 `RING_CAPACITY` = 500 行——滚动容器内超可视区部分滚轮可达，
//! 11-21 反馈 2：此前固定尾部 60 行 + 无滚动，下方内容不可见）。
//!
//! # 自动跟随底部（11-21 反馈 2 裁定）
//!
//! - 渲染前判「用户是否已在底部」（`offset().y` 已达 `-max_offset().y`，含
//!   1px 容差与未溢出态）——已在底部则本帧内容更新后 `scroll_to_bottom()`
//!   跟随新日志；用户上滚查看历史时不抢滚动（判定 false，不调用跟随）。
//! - 首帧（从未滚动、未溢出）判定为在底部 → 打开页面即展示最新行。
//! - `scroll_to_bottom` 是 prepaint 期消费的标志（gpui ScrollHandle 契约），
//!   渲染期调用即对本帧生效。

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::*;
use gpui_kit::{ScrollHandle, SharedString, TestSupportExt as _, div, px, rgb};

/// 分区标题色（弱化灰）。
const HEADER_COLOR: u32 = 0x8fa1b3;
/// 日志行文本色（暗底浅字）。
const LINE_COLOR: u32 = 0xaab4c2;

/// 「用户已在底部」判定（1px 容差吸收浮点取整；未溢出 max_offset = 0 时恒真）。
fn is_at_bottom(scroll: &ScrollHandle) -> bool {
    scroll.offset().y <= -scroll.max_offset().y + px(1.0)
}

/// 渲染日志页（根视图下半区；`lines` 旧→新序，全量入滚动容器）。
pub(super) fn render(lines: &[String], scroll: &ScrollHandle) -> gpui_kit::Div {
    let follow = is_at_bottom(scroll);
    let mut column = div().flex().flex_col();
    for line in lines {
        column = column.child(
            div()
                .child(SharedString::from(line.clone()))
                .text_size(px(11.))
                .text_color(rgb(LINE_COLOR)),
        );
    }
    // 滚动区（gpui ScrollHandle + 组件滚动条；结构对齐 gpui-kit
    // `Scrollable::render` 的 Vertical 形态——wrapper 承担布局收缩
    // （overflow_hidden 允许在 flex 列中缩到可视区），内层 area 跟踪滚动，
    // content flex_none + min_h_full（不足一屏时撑满、超出时滚动）。
    // area 带 `.test_support()` 观察注册（gpui-kit harness 按 id 定位滚动事件
    // 的靶元素；生产面 inert——TestSupportExt 无 test-support feature 时不生效）。
    let content = div()
        .id("log-content")
        .flex_none()
        .h_auto()
        .min_h_full()
        .flex()
        .flex_col();
    let content = content.child(column);
    let area = div()
        .id("log-scroll-area")
        .test_support()
        .size_full()
        .flex()
        .flex_col()
        .track_scroll(scroll)
        .overflow_y_scroll()
        .child(content);
    let scroller = div()
        .id("log-scroll")
        .flex_1()
        .min_h(px(0.))
        .overflow_hidden()
        .relative()
        .flex()
        .flex_col()
        .child(area)
        .vertical_scrollbar(scroll);
    let page = div()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .child(SharedString::from(xcontrol_i18n::tr("page.log")))
                .text_size(px(15.))
                .text_color(rgb(HEADER_COLOR)),
        )
        .child(
            div()
                .child(SharedString::from(xcontrol_i18n::tr("log.subtitle")))
                .text_size(px(12.))
                .text_color(rgb(HEADER_COLOR)),
        )
        .child(scroller);
    // 新日志到达且用户在底部 → 跟随（标志由本帧 prepaint 消费）。
    if follow {
        scroll.scroll_to_bottom();
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全量渲染（不再截尾——滚动容器承担超可视区；构造路径无 panic）。
    #[test]
    fn renders_all_lines_without_tail_cap() {
        let lines: Vec<String> = (0..600).map(|i| format!("line-{i}")).collect();
        let scroll = ScrollHandle::new();
        let _ = render(&lines, &scroll);
    }

    /// 空日志构造路径安全。
    #[test]
    fn empty_log_is_safe() {
        let scroll = ScrollHandle::new();
        let _ = render(&[], &scroll);
    }

    /// 「在底部」判定：未渲染句柄（offset = max = 0）视为在底部——首帧跟随。
    /// 真实滚动态（上滚不抢滚动/滚回底部恢复跟随）由 UI harness 测试覆盖
    ///（max_offset 仅在真实 paint 中更新，纯单测无布局面）。
    #[test]
    fn fresh_handle_counts_as_at_bottom() {
        let scroll = ScrollHandle::new();
        assert!(is_at_bottom(&scroll), "初始态视为在底部（跟随新日志）");
    }
}
