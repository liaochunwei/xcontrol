//! 剪贴板页（FR-4.4/4.5，docs/04 同步范围/上限/格式的 M4 编辑面）。
//!
//! ```text
//! 同步范围（current/all） + 大小上限（数值输入 + 校验） + 格式开关
//! （text/image/html 全部可开关——11-19-m5-clipboard-full 起三格式读写路径
//! 已落平台层，开关随配置回显；同步管线接线后于会话构造点读取）
//! ```
//!
//! 生效路径（docs/06 §4.1）：改动落盘下发 → **下会话生效**（剪贴板管线于会话
//! 构造点读取格式集合）。上限校验：`0 = 禁用`、不得超过
//! [`xcontrol_config::MAX_CLIPBOARD_BYTES_LIMIT`]（64 MiB，FR-4.5 兜底）。

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, Window, div, px};
use xcontrol_config::{
    ClipboardConfig, ClipboardFormat, ClipboardScope, Config, MAX_CLIPBOARD_BYTES_LIMIT,
};

use super::SettingsView;

/// 剪贴板页控件状态。
pub(crate) struct ClipboardPage {
    /// 大小上限输入（字节；0 = 禁用；测试经实体 setup 内容）。
    pub(crate) max_bytes_input: Entity<InputState>,
}

impl ClipboardPage {
    pub(super) fn new(
        config: &Config,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        Self {
            max_bytes_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(config.clipboard.max_bytes.to_string())
                    .placeholder(xcontrol_i18n::trf(
                        "clipboard.ph_max_bytes",
                        &[("n", &MAX_CLIPBOARD_BYTES_LIMIT.to_string())],
                    ))
            }),
        }
    }
}

/// 控件订阅：输入回显（应用按钮承担主路径——数值中间态不落盘）。
pub(super) fn subscribe(_page: &ClipboardPage, _cx: &mut Context<SettingsView>) {}

/// 渲染剪贴板页。
pub(super) fn render(
    view: &mut SettingsView,
    _window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let clipboard = &view.config.clipboard;
    let text_on = clipboard.formats.contains(&ClipboardFormat::Text);
    let image_on = clipboard.formats.contains(&ClipboardFormat::Image);
    let html_on = clipboard.formats.contains(&ClipboardFormat::Html);

    let mut page = v_flex()
        .gap_3()
        .child(
            div()
                .child(xcontrol_i18n::tr("clipboard.intro"))
                .text_size(px(12.))
                .text_color(muted),
        )
        // 同步范围。
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("clipboard.scope"))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(
                    gpui_kit::component::h_flex()
                        .gap_2()
                        .child(
                            Switch::new("clipboard-scope-current")
                                .label(xcontrol_i18n::tr("clipboard.scope_current"))
                                .checked(clipboard.scope == ClipboardScope::Current)
                                .on_click(cx.listener(
                                    |view: &mut SettingsView, _ev: &bool, _window, cx| {
                                        set_scope(view, ClipboardScope::Current, cx);
                                    },
                                )),
                        )
                        .child(
                            Switch::new("clipboard-scope-all")
                                .label(xcontrol_i18n::tr("clipboard.scope_all"))
                                .checked(clipboard.scope == ClipboardScope::All)
                                .on_click(cx.listener(
                                    |view: &mut SettingsView, _ev: &bool, _window, cx| {
                                        set_scope(view, ClipboardScope::All, cx);
                                    },
                                )),
                        ),
                ),
        )
        // 大小上限。
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::trf(
                            "clipboard.max_hint",
                            &[("n", &MAX_CLIPBOARD_BYTES_LIMIT.to_string())],
                        ))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(
                    gpui_kit::component::h_flex()
                        .gap_2()
                        .child(
                            div()
                                .w(px(220.))
                                .child(Input::new(&view.clipboard.max_bytes_input)),
                        )
                        .child(
                            Button::new("clipboard-max-save")
                                .label(xcontrol_i18n::tr("common.save"))
                                .compact()
                                .on_click(cx.listener(
                                    |view: &mut SettingsView, _ev, window, cx| {
                                        apply_max_bytes(view, window, cx);
                                    },
                                )),
                        ),
                ),
        );

    // 格式开关（text/image/html 全部可开关——11-19-m5-clipboard-full 三格式
    // 读写路径已落平台层；checked 随配置回显）。
    page = page.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .child(xcontrol_i18n::tr("clipboard.formats_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            )
            .child(
                gpui_kit::component::h_flex()
                    .gap_4()
                    .child(
                        Switch::new("clipboard-format-text")
                            .label(xcontrol_i18n::tr("clipboard.format_text"))
                            .checked(text_on)
                            .on_click(cx.listener(
                                |view: &mut SettingsView, ev: &bool, _window, cx| {
                                    toggle_format(view, ClipboardFormat::Text, *ev, cx);
                                },
                            )),
                    )
                    .child(
                        Switch::new("clipboard-format-image")
                            .label(xcontrol_i18n::tr("clipboard.format_image"))
                            .checked(image_on)
                            .on_click(cx.listener(
                                |view: &mut SettingsView, ev: &bool, _window, cx| {
                                    toggle_format(view, ClipboardFormat::Image, *ev, cx);
                                },
                            )),
                    )
                    .child(
                        Switch::new("clipboard-format-html")
                            .label("HTML")
                            .checked(html_on)
                            .on_click(cx.listener(
                                |view: &mut SettingsView, ev: &bool, _window, cx| {
                                    toggle_format(view, ClipboardFormat::Html, *ev, cx);
                                },
                            )),
                    ),
            ),
    );
    page
}

/// 范围切换（current/all 单选语义——switch 成对呈现，点中即提交）。
fn set_scope(view: &mut SettingsView, scope: ClipboardScope, cx: &mut Context<SettingsView>) {
    tracing::info!(target: "app::settings", scope = ?scope, "剪贴板范围变更（下会话生效）");
    view.commit_config(xcontrol_i18n::tr("label.clipboard_scope"), |config| {
        config.clipboard.scope = scope;
        Ok(())
    });
    cx.notify();
}

/// 格式开关提交（text/image/html；空列表 = 禁用同步，允许——FR-4.4 语义）。
fn toggle_format(
    view: &mut SettingsView,
    format: ClipboardFormat,
    enable: bool,
    cx: &mut Context<SettingsView>,
) {
    tracing::info!(target: "app::settings", format = ?format, enable,
        "剪贴板格式开关（下会话生效）");
    view.commit_config(xcontrol_i18n::tr("label.clipboard_format"), |config| {
        apply_format_toggle(&mut config.clipboard, format, enable);
        Ok(())
    });
    cx.notify();
}

/// 格式集合变更（纯函数——单测覆盖；空列表 = 禁用同步）。
fn apply_format_toggle(clipboard: &mut ClipboardConfig, format: ClipboardFormat, enable: bool) {
    let contains = clipboard.formats.contains(&format);
    if enable && !contains {
        clipboard.formats.push(format);
    } else if !enable && contains {
        clipboard.formats.retain(|item| *item != format);
    }
}

/// 上限应用（校验：数字、0..=MAX；非法就地拒绝，docs/06 §4）。
fn apply_max_bytes(view: &mut SettingsView, window: &mut Window, cx: &mut Context<SettingsView>) {
    let raw = view
        .clipboard
        .max_bytes_input
        .read(cx)
        .value()
        .trim()
        .to_owned();
    let parsed: Result<u64, _> = raw.parse();
    let value = match parsed {
        Ok(value) if value <= MAX_CLIPBOARD_BYTES_LIMIT => value,
        Ok(value) => {
            let error = xcontrol_i18n::trf(
                "clipboard.err_over_limit",
                &[
                    ("v", &value.to_string()),
                    ("n", &MAX_CLIPBOARD_BYTES_LIMIT.to_string()),
                ],
            );
            view.notify_text(error, true);
            cx.notify();
            return;
        }
        Err(_) => {
            view.notify_text(
                xcontrol_i18n::trf("clipboard.err_not_number", &[("raw", &format!("{raw:?}"))]),
                true,
            );
            cx.notify();
            return;
        }
    };
    tracing::info!(target: "app::settings", value, "剪贴板上限变更（下会话生效）");
    view.commit_config(xcontrol_i18n::tr("label.clipboard_max"), |config| {
        config.clipboard.max_bytes = value;
        Ok(())
    });
    // 成功后输入框对齐规范化值。
    view.clipboard.max_bytes_input.update(cx, |state, cx| {
        state.set_value(value.to_string(), window, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 格式开关纯函数：加入/移除/幂等；空列表 = 禁用语义允许。
    #[test]
    fn apply_format_toggle_is_idempotent() {
        let mut clipboard = ClipboardConfig::default();
        apply_format_toggle(&mut clipboard, ClipboardFormat::Text, false);
        assert!(!clipboard.formats.contains(&ClipboardFormat::Text));
        apply_format_toggle(&mut clipboard, ClipboardFormat::Text, false);
        assert!(!clipboard.formats.contains(&ClipboardFormat::Text), "幂等");
        apply_format_toggle(&mut clipboard, ClipboardFormat::Text, true);
        assert!(clipboard.formats.contains(&ClipboardFormat::Text));
        apply_format_toggle(&mut clipboard, ClipboardFormat::Text, true);
        assert_eq!(
            clipboard
                .formats
                .iter()
                .filter(|item| **item == ClipboardFormat::Text)
                .count(),
            1,
            "重复开启不产生重复项"
        );
        // 全关 = 禁用同步（合法形态）。
        for format in [
            ClipboardFormat::Text,
            ClipboardFormat::Image,
            ClipboardFormat::Html,
        ] {
            apply_format_toggle(&mut clipboard, format, false);
        }
        assert!(clipboard.formats.is_empty(), "空 formats = 禁用同步");
    }
}
