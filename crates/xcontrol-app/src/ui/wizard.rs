//! 首次运行向导（FR-6.3，docs/06 §3 窗口清单「首次运行向导」）。
//!
//! ```text
//! 角色选择 → 安全码（输入/生成建议）→ 网段默认 → 权限引导 → 完成
//! ```
//!
//! 触发：无配置文件（`--config` 路径不存在）+ 无模式参数启动 → 主窗首屏即向导
//! （main.rs run_tcp 开窗策略；supervisor 处于等待相——FR-6.3「全程无命令行完成
//! 配对与使用」）。跳过按钮（CLI 用户）：向导收起、进程照常运行（等待相保留，
//! 设置页改动可唤醒运行时）。
//!
//! 完成 = 构建 [`Config`]（role/端口/CIDR/`security_code_ref=file:`）→ 安全码经
//! [`FileSecretStore`] 0600 落盘（keychain M5）→ `xcontrol_config::save` →
//! `RuntimeConfigUpdate` 下发 → supervisor 按所选角色启动（等待相退出即定角色，
//! 不叠加 SwitchRole——见 main.rs run_tcp 文档）。
//!
//! 每步一个稳定 id 容器 + 控件 AX 标签（osascript 实测锚点）；权限页按平台条件
//! 渲染（mac 辅助功能实时检测 `check_accessibility(prompt:false)`——xcontrol-input
//! macos feature 门控；win/Linux 引导 M5）。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, FontWeight, SharedString, Window, div, px};
use xcontrol_config::{Config, Role};

use super::SettingsView;

/// 向导默认钥匙串条目名（`keychain:xcontrol`——M0 定义的前缀路由默认值，
/// docs/01 §5 / docs/07 §3.3）。
const WIZARD_KEYCHAIN_NAME: &str = "xcontrol";

/// 钥匙串可用性探测（向导完成路径）：**测试编译面（cfg(test)）恒 false**——
/// 单测/CI 不触碰真实 OS 钥匙串（tests_ui 断言 file 回退面；真实现行为由
/// xcontrol-config 的 `#[ignore] keychain_live_round_trip` 本机实测承载）。
fn keychain_available(name: &str) -> bool {
    if cfg!(test) {
        return false;
    }
    xcontrol_config::KeychainSecretStore::new(name).is_available()
}

/// 向导步骤（design.md 状态机：`WizardStep`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardStep {
    /// 步骤 1：角色选择（FR-1.1）。
    Role,
    /// 步骤 2：安全码（输入/生成建议）。
    Code,
    /// 步骤 3：网段默认（端口/CIDR；空 CIDR = 自动取各接口真实前缀）。
    Network,
    /// 步骤 4：权限引导（按平台渲染，FR-6.3）。
    Permissions,
}

impl WizardStep {
    /// 步骤序号（1 起步；进度指示用）。
    const ORDER: [Self; 4] = [Self::Role, Self::Code, Self::Network, Self::Permissions];

    fn index(self) -> usize {
        // 不变量：WizardStep 四值恰为 ORDER 全集（测试钉死）；防御性回落 0。
        Self::ORDER
            .iter()
            .position(|step| *step == self)
            .unwrap_or(0)
    }

    /// 标题（步骤容器 AX 标签；FR-6.5 文案表——随当前语言渲染）。
    fn title(self) -> &'static str {
        match self {
            Self::Role => xcontrol_i18n::tr("wizard.step_role"),
            Self::Code => xcontrol_i18n::tr("wizard.step_code"),
            Self::Network => xcontrol_i18n::tr("wizard.step_network"),
            Self::Permissions => xcontrol_i18n::tr("wizard.step_permissions"),
        }
    }

    /// 稳定容器 id（AX/实测锚点：wizard-step-role 等）。
    fn id(self) -> &'static str {
        match self {
            Self::Role => "wizard-step-role",
            Self::Code => "wizard-step-code",
            Self::Network => "wizard-step-network",
            Self::Permissions => "wizard-step-permissions",
        }
    }
}

/// 向导状态（每步局部控件 + 校验错误；完成路径见 [`complete`]）。
pub(crate) struct WizardState {
    /// 当前步骤。
    step: WizardStep,
    /// 所选角色（默认从机——保守默认：新装不主动控制他人，config v3 同语义）。
    role: Role,
    /// 安全码输入。
    code_input: Entity<InputState>,
    /// 服务端口输入（默认 [`xcontrol_config::DEFAULT_PORT`]）。
    port_input: Entity<InputState>,
    /// CIDR 输入（逗号分隔；空 = 自动）。
    cidr_input: Entity<InputState>,
    /// 主控接入白名单输入（FR-5.4，逗号分隔 CIDR；空 = 不启用——accept 后
    /// 握手前按来源 IP 过滤，docs/07 §6）。
    allowlist_input: Entity<InputState>,
    /// 步内校验错误（就地提示，docs/06 §4）。
    error: Option<String>,
}

impl WizardState {
    /// 当前步骤序号（UI 测试断言用；步骤推进语义的可观测面）。
    #[cfg(test)]
    pub(crate) fn step_index_for_test(&self) -> usize {
        self.step.index()
    }

    /// 当前步内错误文案（UI 测试断言用——就地提示的视图状态面）。
    #[cfg(test)]
    pub(crate) fn error_text_for_test(&self) -> Option<&String> {
        self.error.as_ref()
    }

    /// 构造（控件状态需要 window）。
    pub(super) fn new(
        _config: &Config,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        Self {
            step: WizardStep::Role,
            role: Role::Client,
            code_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("wizard.ph_code"))
            }),
            port_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(xcontrol_config::DEFAULT_PORT.to_string())
                    .placeholder(xcontrol_i18n::tr("wizard.ph_port"))
            }),
            cidr_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("wizard.ph_cidr"))
            }),
            allowlist_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("wizard.ph_allowlist"))
            }),
            error: None,
        }
    }
}

/// 生成安全码建议（非 CSPRNG——`RandomState` 随机种子混叠时间；正式随机生成
/// 随 keychain 接线（M5）走系统 keystore。建议码仅提示，用户可编辑）。
fn suggest_code() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut a = RandomState::new().build_hasher();
    let mut b = RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_u64, |d| d.as_nanos() as u64);
    a.write_u64(nanos);
    b.write_u64(u64::from(std::process::id()));
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

/// 向导整体渲染（标题 + 进度 + 步骤内容 + 页脚按钮）。
pub(super) fn render(
    _view: &mut SettingsView,
    wizard: &mut WizardState,
    _window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> gpui_kit::AnyElement {
    let theme = cx.theme().clone();
    let step = wizard.step;
    let mut column = v_flex()
        .size_full()
        .p_6()
        .gap_4()
        .bg(theme.background)
        .text_color(theme.foreground)
        // 稳定容器 id（每步一个 AX 容器，docs/design.md「每步 AX 容器」）。
        .child(
            div()
                .id(step.id())
                .child(SharedString::from(step.title()))
                .text_size(px(20.))
                .font_weight(FontWeight::MEDIUM),
        )
        .child(
            div()
                .child(SharedString::from(xcontrol_i18n::trf(
                    "wizard.progress",
                    &[("n", &(step.index() + 1).to_string())],
                )))
                .text_size(px(12.))
                .text_color(theme.muted_foreground),
        );

    column = column.child(match step {
        WizardStep::Role => render_role_step(wizard, cx),
        WizardStep::Code => render_code_step(wizard, cx),
        WizardStep::Network => render_network_step(wizard, cx),
        WizardStep::Permissions => render_permissions_step(cx),
    });

    if let Some(error) = &wizard.error {
        column = column.child(
            div()
                .id("wizard-error")
                .child(SharedString::from(error.clone()))
                .text_size(px(12.))
                .text_color(theme.danger),
        );
    }

    // 页脚：跳过（CLI 用户）| 上一步 | 下一步/完成。
    let mut footer = gpui_kit::component::h_flex().justify_between();
    footer = footer.child(
        Button::new("wizard-skip")
            .label(xcontrol_i18n::tr("wizard.skip"))
            .ghost()
            .on_click(cx.listener(|view, _ev, _window, cx| {
                tracing::info!(target: "app::ui",
                    "向导被跳过（CLI 用户）：收取向导，运行时保持等待相（设置改动可唤醒）");
                view.wizard = None;
                // 向导流终结（11-18-m4-ui check 修复）：窗口重建不再复现向导；
                // 运行时保持等待相（状态页待命提示可见）。
                cx.set_global(super::WizardFlowDone(super::WizardFlowOutcome::Skipped));
                view.notify_text(xcontrol_i18n::tr("wizard.skipped_notice"), false);
                cx.notify();
            })),
    );
    let mut actions = gpui_kit::component::h_flex().gap_2();
    if step.index() > 0 {
        actions = actions.child(
            Button::new("wizard-prev")
                .label(xcontrol_i18n::tr("wizard.prev"))
                .on_click(cx.listener(|view, _ev, _window, cx| {
                    let Some(wizard) = view.wizard.as_mut() else {
                        return;
                    };
                    wizard.step = WizardStep::ORDER[wizard.step.index() - 1];
                    wizard.error = None;
                    tracing::info!(target: "app::ui", step = wizard.step.title(), "向导步骤回退（上一步）");
                    cx.notify();
                })),
        );
    }
    if step == WizardStep::Permissions {
        actions = actions.child(
            Button::new("wizard-done")
                .label(xcontrol_i18n::tr("wizard.done"))
                .primary()
                .on_click(cx.listener(|view, _ev, window, cx| {
                    let Some(wizard) = view.wizard.take() else {
                        return;
                    };
                    match complete(view, &wizard, window, cx) {
                        Ok(()) => {
                            // 向导流终结（11-18-m4-ui check 修复）：窗口重建不再
                            // 复现向导（运行时已唤醒，重建窗口落设置页）。
                            cx.set_global(super::WizardFlowDone(
                                super::WizardFlowOutcome::Completed,
                            ));
                            tracing::info!(target: "app::ui",
                                "向导完成：config 已写入并下发运行时（FR-6.3 全程无命令行）");
                        }
                        Err(error) => {
                            // 失败回填向导（就地提示，状态保留可重试）。
                            view.wizard = Some(wizard);
                            if let Some(wizard) = view.wizard.as_mut() {
                                wizard.error = Some(error);
                            }
                        }
                    }
                    cx.notify();
                })),
        );
    } else {
        actions = actions.child(
            Button::new("wizard-next")
                .label(xcontrol_i18n::tr("wizard.next"))
                .primary()
                .on_click(cx.listener(|view, _ev, _window, cx| {
                    let Some(wizard) = view.wizard.as_mut() else {
                        return;
                    };
                    if let Err(error) = validate_step(wizard, cx) {
                        tracing::info!(target: "app::ui", step = wizard.step.title(), "{error}");
                        wizard.error = Some(error);
                    } else {
                        wizard.error = None;
                        wizard.step = WizardStep::ORDER[wizard.step.index() + 1];
                        tracing::info!(target: "app::ui", step = wizard.step.title(),
                            "向导步骤推进（下一步）");
                    }
                    cx.notify();
                })),
        );
    }
    footer = footer.child(actions);
    column = column.child(footer);
    column.into_any_element()
}

/// 步骤 1：角色选择（FR-1.1）。
fn render_role_step(wizard: &WizardState, cx: &mut Context<SettingsView>) -> gpui_kit::AnyElement {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let host_selected = wizard.role == Role::Host;
    let client_selected = wizard.role == Role::Client;
    v_flex()
        .gap_3()
        .child(
            div()
                .child(xcontrol_i18n::tr("wizard.role_question"))
                .text_size(px(13.))
                .text_color(muted),
        )
        .child(
            gpui_kit::component::h_flex()
                .gap_3()
                .child(
                    Button::new("wizard-role-host")
                        .label(if host_selected {
                            format!(
                                "{}{}",
                                xcontrol_i18n::tr("wizard.role_host"),
                                xcontrol_i18n::tr("common.check_suffix")
                            )
                        } else {
                            xcontrol_i18n::tr("wizard.role_host").to_owned()
                        })
                        .on_click(cx.listener(|view: &mut SettingsView, _ev, _window, cx| {
                            if let Some(wizard) = view.wizard.as_mut() {
                                wizard.role = Role::Host;
                                tracing::info!(target: "app::ui", "向导角色选择：主控（Host）");
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("wizard-role-client")
                        .label(if client_selected {
                            format!(
                                "{}{}",
                                xcontrol_i18n::tr("wizard.role_client"),
                                xcontrol_i18n::tr("common.check_suffix")
                            )
                        } else {
                            xcontrol_i18n::tr("wizard.role_client").to_owned()
                        })
                        .on_click(cx.listener(|view: &mut SettingsView, _ev, _window, cx| {
                            if let Some(wizard) = view.wizard.as_mut() {
                                wizard.role = Role::Client;
                                tracing::info!(target: "app::ui", "向导角色选择：从机（Client）");
                            }
                            cx.notify();
                        })),
                ),
        )
        .child(role_hint())
        .into_any_element()
}

/// 角色语义提示（静态文案；FR-6.5 文案表）。
fn role_hint() -> gpui_kit::AnyElement {
    div()
        .child(xcontrol_i18n::tr("wizard.role_hint"))
        .text_size(px(12.))
        .into_any_element()
}

/// 步骤 2：安全码（输入 + 生成建议）。
fn render_code_step(wizard: &WizardState, cx: &mut Context<SettingsView>) -> gpui_kit::AnyElement {
    let theme = cx.theme().clone();
    v_flex()
        .gap_3()
        .child(
            div()
                .child(xcontrol_i18n::tr("wizard.code_hint"))
                .text_size(px(13.))
                .text_color(theme.muted_foreground),
        )
        .child(
            gpui_kit::component::h_flex()
                .gap_2()
                .w_full()
                .child(
                    div().flex_1().child(
                        Input::new(&wizard.code_input)
                            .mask_toggle()
                            .cleanable(false),
                    ),
                )
                .child(
                    Button::new("wizard-gen-code")
                        .label(xcontrol_i18n::tr("wizard.gen_code"))
                        .on_click(cx.listener(|view: &mut SettingsView, _ev, window, cx| {
                            let Some(wizard) = view.wizard.as_mut() else {
                                return;
                            };
                            let suggestion = suggest_code();
                            let state = wizard.code_input.clone();
                            let chars = suggestion.len();
                            state.update(cx, |state, cx| {
                                state.set_value(suggestion, window, cx);
                            });
                            tracing::info!(target: "app::ui", chars,
                                "向导安全码：已填入生成建议（非 CSPRNG，可编辑）");
                            cx.notify();
                        })),
                ),
        )
        .into_any_element()
}

/// 步骤 3：网段默认（端口 + CIDR；空 = 自动）。
fn render_network_step(wizard: &WizardState, cx: &Context<SettingsView>) -> gpui_kit::AnyElement {
    let theme = cx.theme().clone();
    v_flex()
        .gap_3()
        .child(
            div()
                .child(xcontrol_i18n::tr("wizard.network_hint"))
                .text_size(px(13.))
                .text_color(theme.muted_foreground),
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("wizard.port"))
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground),
                )
                .child(div().w(px(220.)).child(Input::new(&wizard.port_input))),
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("wizard.cidr"))
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground),
                )
                .child(div().w_full().child(Input::new(&wizard.cidr_input))),
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("wizard.allowlist"))
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground),
                )
                .child(div().w_full().child(Input::new(&wizard.allowlist_input))),
        )
        .into_any_element()
}

/// 步骤 4：权限引导（按平台条件渲染，FR-6.3）。
///
/// 门控 = app `macos` feature（xcontrol-input/macos 转发——CI/本机无平台 feature
/// 时编译面不含 check_accessibility，走 M5 提示分支；ui-tray.md 平台门控纪律）。
fn render_permissions_step(cx: &mut Context<SettingsView>) -> gpui_kit::AnyElement {
    #[cfg(feature = "macos")]
    {
        render_permissions_step_macos(cx)
    }
    #[cfg(not(feature = "macos"))]
    {
        let _ = cx;
        div()
            .child(xcontrol_i18n::tr("wizard.perm_generic"))
            .text_size(px(13.))
            .into_any_element()
    }
}

/// mac 权限页：辅助功能实时检测 + 打开系统设置 + 重新检测（FR-6.3 AC：真实状态
/// 显示——本机已授权绿色，未授权给引导按钮）。
#[cfg(feature = "macos")]
fn render_permissions_step_macos(cx: &mut Context<SettingsView>) -> gpui_kit::AnyElement {
    let theme = cx.theme().clone();
    // 实时状态（prompt:false——不弹系统框；渲染期每次现查，授权后重新检测即绿）。
    let status = xcontrol_input::check_accessibility(false);
    let granted = status.is_granted();
    let mut column = v_flex().gap_3().child(
        div()
            .child(xcontrol_i18n::tr("wizard.perm_mac_hint"))
            .text_size(px(13.))
            .text_color(theme.muted_foreground),
    );
    column = column.child(
        div()
            .id("wizard-ax-status")
            .child(SharedString::from(if granted {
                xcontrol_i18n::tr("wizard.ax_granted").to_owned()
            } else {
                xcontrol_i18n::tr("wizard.ax_not_granted").to_owned()
            }))
            .text_size(px(14.))
            .text_color(if granted { theme.primary } else { theme.danger }),
    );
    column = column.child(
        gpui_kit::component::h_flex()
            .gap_2()
            .child(
                Button::new("wizard-perm-open")
                    .label(xcontrol_i18n::tr("wizard.open_ax_settings"))
                    .on_click(|_ev, _window, _cx| {
                        // 跳转系统隐私面板（macOS 标准 deep link）。
                        let opened = std::process::Command::new("open")
                            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                            .spawn();
                        match opened {
                            Ok(_) => tracing::info!(target: "app::ui", "已打开系统设置（辅助功能面板）"),
                            Err(error) => tracing::warn!(target: "app::ui", %error, "打开系统设置失败"),
                        }
                    }),
            )
            .child(
                Button::new("wizard-perm-refresh")
                    .label(xcontrol_i18n::tr("wizard.recheck"))
                    .ghost()
                    .on_click(cx.listener(|_view: &mut SettingsView, _ev, _window, cx| {
                        let status = xcontrol_input::check_accessibility(false);
                        tracing::info!(target: "app::ui", granted = status.is_granted(),
                            "辅助功能状态重新检测");
                        cx.notify();
                    })),
            ),
    );
    column.into_any_element()
}

/// 步间校验（下一步/完成前；docs/06 §4 就地提示）。
fn validate_step(wizard: &WizardState, cx: &App) -> Result<(), String> {
    match wizard.step {
        WizardStep::Role => Ok(()),
        WizardStep::Code => {
            let code = wizard.code_input.read(cx).value().trim().to_owned();
            if code.is_empty() {
                return Err(xcontrol_i18n::tr("wizard.err_code_empty").to_owned());
            }
            if code.chars().count() < 8 {
                return Err(xcontrol_i18n::tr("wizard.err_code_short").to_owned());
            }
            Ok(())
        }
        WizardStep::Network => {
            let port = wizard.port_input.read(cx).value().trim().to_owned();
            let port: u16 = port.parse().map_err(|_| {
                xcontrol_i18n::trf("wizard.err_port_invalid", &[("raw", &format!("{port:?}"))])
            })?;
            if port == 0 {
                return Err(xcontrol_i18n::tr("wizard.err_port_range").to_owned());
            }
            // CIDR 宽松校验（正式校验在 save 的 validate 流水线，错误就地回显）。
            for raw in split_cidrs(&wizard.cidr_input.read(cx).value()) {
                if !raw.contains('/') {
                    return Err(xcontrol_i18n::trf(
                        "wizard.err_cidr_invalid",
                        &[("raw", &format!("{raw:?}"))],
                    ));
                }
            }
            // 白名单（FR-5.4）同套宽松校验。
            for raw in split_cidrs(&wizard.allowlist_input.read(cx).value()) {
                if !raw.contains('/') {
                    return Err(xcontrol_i18n::trf(
                        "wizard.err_allowlist_invalid",
                        &[("raw", &format!("{raw:?}"))],
                    ));
                }
            }
            Ok(())
        }
        WizardStep::Permissions => Ok(()),
    }
}

/// 逗号/空白分隔的 CIDR 列表（trim + 去空）。
fn split_cidrs(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// 完成：构建 Config → 存码 → 落盘 → 下发运行时（FR-6.3 出口）。
///
/// 成功后向导收起（`view.wizard = None`），状态页可查角色与连接状态。
fn complete(
    view: &mut SettingsView,
    wizard: &WizardState,
    _window: &mut Window,
    cx: &App,
) -> Result<(), String> {
    let code = wizard.code_input.read(cx).value().trim().to_owned();
    if code.is_empty() {
        return Err(xcontrol_i18n::tr("wizard.err_code_empty_back").to_owned());
    }
    let port: u16 = wizard
        .port_input
        .read(cx)
        .value()
        .trim()
        .parse()
        .map_err(|_| xcontrol_i18n::tr("wizard.err_port_back").to_owned())?;
    if port == 0 {
        return Err(xcontrol_i18n::tr("wizard.err_port_back").to_owned());
    }
    let cidrs = split_cidrs(&wizard.cidr_input.read(cx).value());
    let allowlist = split_cidrs(&wizard.allowlist_input.read(cx).value());

    // 配置构建（design.md：完成时构建 Config——role/端口/网段/白名单/net 默认）。
    // 安全码引用（keychain 真接线，11-19-m6-security，docs/07 §3.3）：钥匙串
    // 可用 → 默认 keychain 条目（`keychain:xcontrol`——M0 就定义的前缀路由）；
    // 不可用（headless 无 DBus / 平台限制）→ file 0600 回退 + 完成日志注明。
    let data_dir = view
        .data_dir
        .clone()
        .ok_or_else(|| xcontrol_i18n::tr("wizard.err_data_dir").to_owned())?;
    let (security_code_ref, store): (String, Box<dyn xcontrol_config::SecretStore>) =
        if keychain_available(WIZARD_KEYCHAIN_NAME) {
            tracing::info!(target: "app::ui", name = WIZARD_KEYCHAIN_NAME,
                "向导：钥匙串可用，安全码默认存入 OS 钥匙串（docs/07 §3.3）");
            (
                xcontrol_config::SecretRef::Keychain {
                    name: WIZARD_KEYCHAIN_NAME.to_owned(),
                }
                .to_config_value(),
                Box::new(xcontrol_config::KeychainSecretStore::new(
                    WIZARD_KEYCHAIN_NAME,
                )),
            )
        } else {
            tracing::info!(target: "app::ui",
                "向导：钥匙串不可用，安全码回退 file 0600（docs/07 §3.3）");
            (
                xcontrol_config::SecretRef::File {
                    path: std::path::PathBuf::from(xcontrol_config::DEFAULT_SECRET_FILE_RELATIVE),
                }
                .to_config_value(),
                Box::new(xcontrol_config::FileSecretStore::new(
                    data_dir.join(xcontrol_config::DEFAULT_SECRET_FILE_RELATIVE),
                )),
            )
        };
    let config = Config {
        role: wizard.role,
        security_code_ref,
        net: xcontrol_config::NetConfig {
            port,
            cidrs,
            allowlist,
            ..xcontrol_config::NetConfig::default()
        },
        ..Config::default()
    };

    // 安全码落存储后端（0600/钥匙串，docs/07 §3.3；错误不回显码内容）。
    store
        .store(&xcontrol_config::SecretCode::new(code))
        .map_err(|error| {
            xcontrol_i18n::trf("common.code_write_failed", &[("error", &error.to_string())])
        })?;

    // 落盘（校验在写盘前——save 内建 validate，错误就地回显）。
    let path = view
        .config_path
        .clone()
        .ok_or_else(|| xcontrol_i18n::tr("wizard.err_no_config_path").to_owned())?;
    xcontrol_config::save(&config, &path).map_err(|error| {
        xcontrol_i18n::trf("wizard.err_config_write", &[("error", &error.to_string())])
    })?;

    // 下发运行时：等待相（无码启动）按 config.role 定角色进入主循环；非等待相
    // （显式模式 + 缺配置）且角色不同时补发 SwitchRole（FIFO 保序：更新先于切换）。
    tracing::info!(target: "app::ui", path = %path.display(), role = ?config.role,
        port, cidrs = ?config.net.cidrs, allowlist = ?config.net.allowlist,
        ref = %config.security_code_ref,
        "向导完成：config 已落盘（安全码已按可用性入钥匙串或 file 0600），下发运行时");
    view.core
        .send(super::CoreCommand::RuntimeConfigUpdate(Box::new(
            config.clone(),
        )));
    if !view.runtime_pending {
        let current = view.bus.current().role;
        if current != config.role {
            tracing::info!(target: "app::ui", from = ?current, to = ?config.role,
                "非等待相完成向导：角色与运行时不一致，补发 SwitchRole");
            view.core.send(super::CoreCommand::SwitchRole);
        }
    }
    view.config = config;
    view.wizard = None;
    // 运行时已唤醒（等待相退出）：状态页不再显示"运行时待命中"提示
    //（11-18-m4-ui check 修复——完成后的诚实呈现）。
    view.runtime_pending = false;
    view.page = super::Page::Status;
    view.notify_text(xcontrol_i18n::tr("wizard.done_notice"), false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成建议：32 位 hex、两次生成不同（随机种子语义）。
    #[test]
    fn suggest_code_is_hex_and_varies() {
        let first = suggest_code();
        assert_eq!(first.len(), 32, "{first}");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_ne!(first, suggest_code(), "两次生成应不同");
    }

    /// CIDR 拆分：逗号/空白分隔、trim、去空。
    #[test]
    fn split_cidrs_handles_separators() {
        assert!(split_cidrs("").is_empty());
        assert!(split_cidrs("  ").is_empty());
        assert_eq!(
            split_cidrs("192.168.1.0/24, 10.0.0.0/8"),
            vec!["192.168.1.0/24".to_owned(), "10.0.0.0/8".to_owned()]
        );
        assert_eq!(split_cidrs(" fd00::/8 "), vec!["fd00::/8".to_owned()]);
    }

    /// 步骤序号与标题稳定（AX 锚点断言依赖字面量）。
    #[test]
    fn steps_are_stable() {
        assert_eq!(WizardStep::Role.index(), 0);
        assert_eq!(WizardStep::Permissions.index(), 3);
        assert_eq!(WizardStep::Role.id(), "wizard-step-role");
        assert_eq!(WizardStep::Permissions.id(), "wizard-step-permissions");
    }
}
