//! 关于页（docs/06 §4「关于」M4 集：版本 / 协议版本 / 许可 / 诊断）。
//!
//! 诊断包导出（zip）M5+——M4 裁定为**路径展示 + reveal**：日志目录
//! （`<data_dir>/logs/`，NFR-8 布局契约）展示 + 「在 Finder 中打开」按钮
//! （macOS `open` 命令；win `explorer` / linux `xdg-open`——std 层 cfg 门控，
//! secret.rs 同款先例）。

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, FontWeight, SharedString, div, px};

use super::SettingsView;

/// 渲染关于页。
pub(super) fn render(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let logs_dir = view
        .data_dir
        .as_ref()
        .map(|dir| xcontrol_config::logs_dir(dir));

    v_flex()
        .gap_4()
        .child(
            div()
                .child(xcontrol_i18n::tr("about.title"))
                .text_size(px(16.))
                .font_weight(FontWeight::MEDIUM),
        )
        .child(
            div()
                .child(xcontrol_i18n::tr("about.subtitle"))
                .text_size(px(12.))
                .text_color(muted),
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("about.version"))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(div().child(SharedString::from(xcontrol_i18n::trf(
                    "about.version_value",
                    &[
                        ("v", env!("CARGO_PKG_VERSION")),
                        ("major", &xcontrol_proto::PROTO_MAJOR.to_string()),
                        ("minor", &xcontrol_proto::PROTO_MINOR.to_string()),
                    ],
                )))),
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("about.license"))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(div().child(xcontrol_i18n::tr("about.license_value"))),
        )
        // 日志目录（诊断包导出 M5——M4 路径展示 + reveal，docs/06 §4）。
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("about.logs_hint"))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(div().child(SharedString::from(match &logs_dir {
                    Some(dir) => dir.display().to_string(),
                    None => xcontrol_i18n::tr("about.no_data_dir").to_owned(),
                })))
                .child(if logs_dir.is_some() {
                    Button::new("about-reveal-logs")
                        .label(xcontrol_i18n::tr("about.open_logs"))
                        .on_click(|_ev, _window, _cx| reveal_logs_dir())
                        .into_any_element()
                } else {
                    div()
                        .child(xcontrol_i18n::tr("about.no_logs_dir"))
                        .text_size(px(12.))
                        .text_color(muted)
                        .into_any_element()
                }),
        )
        // 清空日志（11-21 实测反馈）：破坏性操作 → 确认模态（docs/06 §4）。
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("about.clear_logs_hint"))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(if logs_dir.is_some() {
                    Button::new("about-clear-logs")
                        .label(xcontrol_i18n::tr("about.clear_logs"))
                        .on_click(cx.listener(|view: &mut SettingsView, _ev, _window, cx| {
                            tracing::info!(target: "app::ui", "清空日志请求：进入确认模态（删除不可撤销）");
                            view.confirm = Some(super::ConfirmRequest::ClearLogs);
                            cx.notify();
                        }))
                        .into_any_element()
                } else {
                    div()
                        .child(xcontrol_i18n::tr("about.no_logs_dir"))
                        .text_size(px(12.))
                        .text_color(muted)
                        .into_any_element()
                }),
        )
}

/// 清空日志执行（确认模态出栈路径）：删全部滚动文件（含当日）+ 清页面环形缓冲 + 就地
/// 提示（结果含删除文件数——诚实呈现；失败提示错误）。
pub(super) fn clear_logs(view: &mut SettingsView, cx: &mut Context<SettingsView>) {
    let Some(data_dir) = view.data_dir.clone() else {
        view.notify_text(xcontrol_i18n::tr("about.no_data_dir"), true);
        return;
    };
    let dir = xcontrol_config::logs_dir(&data_dir);
    match crate::logging::clear_log_files(&dir) {
        Ok(removed) => {
            view.logs.clear();
            tracing::info!(target: "app::log", removed, dir = %dir.display(),
                "日志已清空（滚动文件全部删除 + 页面缓冲重置；次日轮转前新日志只进页面/stdout）");
            view.notify_text(
                xcontrol_i18n::trf("notice.logs_cleared", &[("n", &removed.to_string())]),
                false,
            );
        }
        Err(error) => {
            tracing::warn!(target: "app::log", %error, dir = %dir.display(), "日志清空失败");
            view.notify_text(
                xcontrol_i18n::trf("notice.logs_clear_failed", &[("error", &error.to_string())]),
                true,
            );
        }
    }
    cx.notify();
}

/// 用系统文件管理器打开日志目录（平台命令：mac `open` / win `explorer` /
/// linux `xdg-open`；std 层 cfg 门控——secret.rs `#[cfg(unix)]` 同款纪律，
/// architecture-rules 红线 2 针对的是 feature 门控层的平台判断）。
fn reveal_logs_dir() {
    let Some(data_dir) = xcontrol_config::default_data_dir() else {
        return;
    };
    let dir = xcontrol_config::logs_dir(&data_dir);
    let _ = std::fs::create_dir_all(&dir);
    let command = reveal_command();
    match std::process::Command::new(command).arg(&dir).spawn() {
        Ok(_) => tracing::info!(target: "app::ui", dir = %dir.display(), "已打开日志目录"),
        Err(error) => tracing::warn!(target: "app::ui", %error, dir = %dir.display(),
            "打开日志目录失败（文件管理器不可用？）"),
    }
}

/// reveal 命令（按平台）。
fn reveal_command() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "open"
    }
    #[cfg(target_os = "windows")]
    {
        "explorer"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "xdg-open"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// reveal 命令非空（平台矩阵各分支产出合法程序名）。
    #[test]
    fn reveal_command_is_nonempty() {
        assert!(!reveal_command().is_empty());
    }
}
