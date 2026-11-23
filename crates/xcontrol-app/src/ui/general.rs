//! 通用页（docs/06 §4「通用」M4 集）。
//!
//! ```text
//! 日志级别下拉（五值，**立即生效**——EnvFilter 热重建） + 界面语言下拉
//! （FR-6.5，M6：zh-CN/en——落盘 + RuntimeConfigUpdate 消费点热切） +
//! 开机自启开关（Autostart trait 直调——OS 级配置，11-18-m4-tray 先例） +
//! 安全码修改（确认模态） + 主题（跟随系统 v1）
//! ```
//!
//! 生效路径（docs/06 §4.1）：日志级别 = 立即（`logging::set_level` 于命令消费点
//! 执行，本页提示即生效）；语言 = 立即（`xcontrol_i18n::set_language` 同消费点，
//! 静态文案随下次渲染切换——控件构造期快照在下个窗口生命周期对齐，v1 边界）；
//! 安全码 = 落盘后下次启动/重建生效（PSK 派生进程内缓存）；自启 = 立即
//! （OS 配置，不经核心命令）。

use std::sync::Arc;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, Window, div, px};
use xcontrol_config::{LANGUAGE_VARIANTS, LogLevel};
use xcontrol_tray::Autostart;

use super::{ConfirmRequest, SettingsView};

/// 日志级别下拉选项（docs/01 §5 五值字面值）。
const LOG_LEVELS: [LogLevel; 5] = [
    LogLevel::Error,
    LogLevel::Warn,
    LogLevel::Info,
    LogLevel::Debug,
    LogLevel::Trace,
];

/// 语言下拉选项（config 字面值序 = [`LANGUAGE_VARIANTS`]）。语言名以自身文字
/// 呈现（自描述惯例，不随 UI 语言翻译）。
const LANGUAGE_OPTIONS: [&str; 2] = ["简体中文（zh-CN）", "English (en)"];

/// 通用页控件状态。
pub(crate) struct GeneralPage {
    /// 日志级别下拉（测试经 Confirm 事件驱动确认路径——组件弹层未开观察面）。
    pub(crate) log_level_select: Entity<SelectState<Vec<String>>>,
    /// 语言下拉（FR-6.5：选项 = config 字面值，选中项经 `LANGUAGE_OPTIONS` 序
    /// 映射回 `general.language`——构造期快照，语言切换后本控件重建对齐）。
    pub(crate) language_select: Entity<SelectState<Vec<String>>>,
    /// 新安全码输入（确认模态承担破坏性确认——输入在此暂存）。
    new_code_input: Entity<InputState>,
}

impl GeneralPage {
    pub(super) fn new(
        config: &xcontrol_config::Config,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        // 初始选中 = config [general].log_level（--log-level 为启动期覆盖，
        // 不回显进本控件——UI 改动落盘 config 并热改生效，口径见 docs/06 §4.1）。
        let selected = LOG_LEVELS
            .iter()
            .position(|level| *level == config.general.log_level)
            .unwrap_or(2);
        // 语言选中项 = config [general].language（未知值由校验层拒绝；此处按
        // 位置映射失败回落 zh-CN 位——默认语言）。
        let lang_selected = LANGUAGE_VARIANTS
            .iter()
            .position(|variant| config.general.language.trim() == *variant)
            .unwrap_or(0);
        Self {
            log_level_select: cx.new(|cx| {
                SelectState::new(
                    LOG_LEVELS
                        .iter()
                        .map(|level| level.as_str().to_owned())
                        .collect(),
                    Some(gpui_kit::component::IndexPath::new(selected)),
                    window,
                    cx,
                )
            }),
            language_select: cx.new(|cx| {
                SelectState::new(
                    LANGUAGE_OPTIONS
                        .iter()
                        .map(|label| (*label).to_owned())
                        .collect(),
                    Some(gpui_kit::component::IndexPath::new(lang_selected)),
                    window,
                    cx,
                )
            }),
            new_code_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("general.ph_new_code"))
            }),
        }
    }
}

/// 控件订阅（日志级别选择 → 立即提交；语言选择 → 落盘 + 热切）。
pub(super) fn subscribe(page: &GeneralPage, cx: &mut Context<SettingsView>) {
    cx.subscribe(&page.log_level_select, |view, _entity, event, cx| {
        let SelectEvent::Confirm(Some(level)) = event else {
            return;
        };
        let Some(level) = LogLevel::parse(level) else {
            return;
        };
        tracing::info!(target: "app::settings", level = level.as_str(),
            "日志级别变更（立即生效：EnvFilter 热重建，docs/06 §4.1）");
        view.commit_config(xcontrol_i18n::tr("label.log_level"), |config| {
            config.general.log_level = level;
            Ok(())
        });
        cx.notify();
    })
    .detach();
    // 语言（FR-6.5）：落盘 config → RuntimeConfigUpdate 消费点 set_language
    // （与 log_level 同点）→ tr() 下一次渲染即新语言；托盘 ≤1s 随刷新循环。
    cx.subscribe(&page.language_select, |view, _entity, event, cx| {
        let SelectEvent::Confirm(Some(option)) = event else {
            return;
        };
        let Some(index) = LANGUAGE_OPTIONS.iter().position(|label| label == option) else {
            return;
        };
        let language = LANGUAGE_VARIANTS[index].to_owned();
        tracing::info!(target: "app::settings", language = %language,
            "界面语言变更（即时生效：RuntimeConfigUpdate 消费点 set_language，FR-6.5）");
        view.commit_config(xcontrol_i18n::tr("label.language"), |config| {
            config.general.language = language;
            Ok(())
        });
        cx.notify();
    })
    .detach();
}

/// 渲染通用页。
pub(super) fn render(
    view: &mut SettingsView,
    _window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let autostart_enabled = view
        .autostart
        .as_ref()
        .is_some_and(|backend| backend.is_enabled());

    let mut page = v_flex().gap_4().child(
        div()
            .child(xcontrol_i18n::tr("general.subtitle"))
            .text_size(px(12.))
            .text_color(muted),
    );

    // 日志级别（立即生效）。
    page = page.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .child(xcontrol_i18n::tr("general.log_level_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            )
            .child(
                div().w(px(200.)).child(
                    Select::new(&view.general.log_level_select)
                        .accessibility_label(xcontrol_i18n::tr("label.log_level")),
                ),
            ),
    );

    // 界面语言（FR-6.5：切换即时生效——落盘 + RuntimeConfigUpdate 消费点热切；
    // 控件构造期快照，完整对齐在下个窗口生命周期）。
    page = page.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .child(xcontrol_i18n::tr("general.language_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            )
            .child(
                div().w(px(200.)).child(
                    Select::new(&view.general.language_select)
                        .accessibility_label(xcontrol_i18n::tr("label.language")),
                ),
            ),
    );

    // 开机自启（OS 级，立即）。
    page = page.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .child(xcontrol_i18n::tr("general.autostart_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            )
            .child(if view.autostart.is_some() {
                gpui_kit::component::h_flex()
                    .gap_2()
                    .child(
                        Switch::new("general-autostart")
                            .label(xcontrol_i18n::tr("general.autostart_switch"))
                            .checked(autostart_enabled)
                            .on_click(cx.listener(
                                |view: &mut SettingsView, _ev: &bool, _window, cx| {
                                    toggle_autostart(view, cx);
                                },
                            )),
                    )
                    .into_any_element()
            } else {
                div()
                    .child(xcontrol_i18n::tr("general.no_autostart_backend"))
                    .text_size(px(12.))
                    .text_color(muted)
                    .into_any_element()
            }),
    );

    // 安全码修改（破坏性确认模态，docs/06 §4）。
    page = page.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .child(xcontrol_i18n::tr("general.code_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            )
            .child(
                gpui_kit::component::h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(280.))
                            .child(Input::new(&view.general.new_code_input).mask_toggle()),
                    )
                    .child(
                        Button::new("general-code-change")
                            .label(xcontrol_i18n::tr("general.change_btn"))
                            .on_click(cx.listener(|view: &mut SettingsView, _ev, _window, cx| {
                                request_code_change(view, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .child(xcontrol_i18n::trf(
                        "general.code_ref_hint",
                        &[("ref", &view.config.security_code_ref)],
                    ))
                    .text_size(px(11.))
                    .text_color(muted),
            ),
    );

    // 主题（v1 固定——跟随系统；主题切换 M6+ 打磨）。
    page = page.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .child(xcontrol_i18n::tr("general.theme_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            )
            .child(
                div()
                    .child(xcontrol_i18n::tr("general.theme_follow_system"))
                    .text_size(px(13.)),
            ),
    );

    page
}

/// 自启开关切换（Autostart trait 直调——OS 级配置不经核心命令，11-18-m4-tray
/// 先例；失败就地提示）。
fn toggle_autostart(view: &mut SettingsView, cx: &mut Context<SettingsView>) {
    let Some(autostart): Option<Arc<dyn Autostart>> = view.autostart.clone() else {
        return;
    };
    if autostart.is_enabled() {
        match autostart.disable() {
            Ok(()) => {
                tracing::info!(target: "app::autostart", "开机自启已停用（设置页）");
                view.notify_text(xcontrol_i18n::tr("general.autostart_disabled"), false);
            }
            Err(error) => {
                tracing::warn!(target: "app::autostart", %error, "开机自启停用失败");
                view.notify_text(
                    xcontrol_i18n::trf(
                        "general.autostart_disable_failed",
                        &[("error", &error.to_string())],
                    ),
                    true,
                );
            }
        }
    } else {
        match autostart.enable() {
            Ok(()) => {
                tracing::info!(target: "app::autostart",
                    "开机自启已启用（当前二进制路径生效；cargo run 与安装后路径不同）");
                view.notify_text(xcontrol_i18n::tr("general.autostart_enabled"), false);
            }
            Err(error) => {
                tracing::warn!(target: "app::autostart", %error, "开机自启启用失败");
                view.notify_text(
                    xcontrol_i18n::trf(
                        "general.autostart_enable_failed",
                        &[("error", &error.to_string())],
                    ),
                    true,
                );
            }
        }
    }
    cx.notify();
}

/// 安全码修改请求（校验后进入确认模态——docs/06 §4 破坏性操作）。
fn request_code_change(view: &mut SettingsView, cx: &mut Context<SettingsView>) {
    let code = view
        .general
        .new_code_input
        .read(cx)
        .value()
        .trim()
        .to_owned();
    if code.chars().count() < 8 {
        view.notify_text(xcontrol_i18n::tr("general.err_code_too_short"), true);
        cx.notify();
        return;
    }
    tracing::info!(target: "app::settings", "安全码修改请求：进入确认模态（破坏性操作）");
    view.confirm = Some(ConfirmRequest::ChangeCode {
        code: xcontrol_config::SecretCode::new(code),
    });
    cx.notify();
}
