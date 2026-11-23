//! 状态页（docs/06 §4「状态（默认首页）」）。
//!
//! 纯投影：入参只有 [`AppState`] 快照 + 命令入口——不持有、不回写任何会话状态。
//! 行 = 标签 + 当前值（角色/连接/对端/会话 id/事件与报文计数/最近错误）；
//! M4 补充（docs/06 §4 + PRD Requirement 2）：
//!
//! - **在线从机列表**（主控运行中显示对端名 + 「切换」按钮——FR-2.5「直接切换」，
//!   走 `CoreCommand::SwitchToSlave`，托盘子菜单同信令）；
//! - 运行时等待相提示（FR-6.3 向导流：无码待命状态可见）。
//!
//! 11-21 实测反馈：状态页**不设角色切换控件**（原「切换角色」按钮 + 确认模态
//! 已移除）——角色切换只走托盘菜单（FR-1.1，`CoreCommand::SwitchRole` 命令
//! 链路保留，docs/06 §4）。

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, FontWeight, SharedString, Window, div, px};
use xcontrol_config::Role;

use super::SettingsView;
use crate::state::AppState;

/// 标签列固定宽度（对齐右侧值列）。
const LABEL_WIDTH: f32 = 92.;

/// 渲染状态页。
pub(super) fn render(
    view: &mut SettingsView,
    _window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let mut page = v_flex()
        .gap_2()
        .child(
            div()
                .child(xcontrol_i18n::tr("page.status"))
                .text_size(px(15.))
                .font_weight(FontWeight::MEDIUM),
        )
        .child(
            div()
                .child(xcontrol_i18n::tr("status.subtitle"))
                .text_size(px(12.))
                .text_color(muted),
        );

    for (index, (label, value, is_error)) in rows(&view.state).into_iter().enumerate() {
        page = page.child(row(
            index,
            &label,
            value,
            if is_error {
                theme.danger
            } else {
                theme.foreground
            },
            muted,
        ));
    }

    // 在线从机（FR-2.5）：主控运行中且有对端名 → 一行一个「切换」按钮。
    if view.state.role == Role::Host
        && matches!(
            view.state.conn,
            crate::state::ConnState::Running { paused: false }
        )
        && let Some(peer) = view.state.peer_name.clone()
    {
        page = page.child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .child(xcontrol_i18n::tr("status.slaves_hint"))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(
                    gpui_kit::component::h_flex()
                        .gap_2()
                        .child(
                            Button::new("status-switch-slave")
                                .label(SharedString::from(xcontrol_i18n::trf(
                                    "status.switch_to",
                                    &[("peer", &peer)],
                                )))
                                .on_click(cx.listener(|view: &mut SettingsView, _ev, _window, cx| {
                                    tracing::info!(target: "app::ui", "状态页：直接切换到在线从机（FR-2.5）");
                                    view.core.send(crate::state::CoreCommand::SwitchToSlave { target: 0 });
                                    cx.notify();
                                })),
                        ),
                ),
        );
    }

    // 候选主控列表（FR-5.5）：发现循环投影（多主控待确认 / 错码软排除标注 /
    // 已记忆标记）。每台候选一个「连接并记住」按钮 → ConfirmHost 命令 + config
    // 落盘（net.confirmed_host，docs/00 FR-5.5"选择后优先连接记住的主控"）。
    if !view.state.host_candidates.is_empty() {
        let mut section = v_flex().gap_1().child(
            div()
                .child(xcontrol_i18n::tr("status.candidates_hint"))
                .text_size(px(12.))
                .text_color(muted),
        );
        for (index, candidate) in view.state.host_candidates.iter().enumerate() {
            let display_name = if candidate.name.is_empty() {
                candidate.addr.as_str()
            } else {
                candidate.name.as_str()
            };
            let mut label = xcontrol_i18n::trf(
                "status.candidate_line",
                &[
                    ("checked", if candidate.confirmed { "✓ " } else { "" }),
                    ("name", display_name),
                    ("addr", &candidate.addr),
                    ("source", candidate.source),
                ],
            );
            if candidate.last_handshake_failed {
                label.push_str(xcontrol_i18n::tr("common.last_handshake_failed"));
            }
            let instance_id = candidate.instance_id_hex.clone();
            section = section.child(
                gpui_kit::component::h_flex()
                    .gap_2()
                    .child(
                        div()
                            .child(SharedString::from(label))
                            .text_size(px(12.))
                            .text_color(if candidate.last_handshake_failed {
                                theme.danger
                            } else {
                                theme.foreground
                            }),
                    )
                    .child(
                        Button::new(("status-confirm-host", index))
                            .label(if candidate.confirmed {
                                xcontrol_i18n::tr("status.confirmed_reconnect")
                            } else {
                                xcontrol_i18n::tr("status.connect_remember")
                            })
                            .on_click(cx.listener(
                                move |view: &mut SettingsView, _ev, _window, cx| {
                                    tracing::info!(target: "app::ui", instance_id = %instance_id,
                                    "状态页：确认候选主控（FR-5.5）——立即直连 + 记忆落盘");
                                    view.core.send(crate::state::CoreCommand::ConfirmHost {
                                        instance_id: instance_id.clone(),
                                    });
                                    // 持久化（net.confirmed_host）：走 commit_config
                                    // 框架（落盘 + RuntimeConfigUpdate 传播共享单元）。
                                    view.commit_config(
                                        xcontrol_i18n::tr("label.confirmed_host"),
                                        |config| {
                                            config.net.confirmed_host = Some(instance_id.clone());
                                            Ok(())
                                        },
                                    );
                                    cx.notify();
                                },
                            )),
                    ),
            );
        }
        page = page.child(section);
    }

    // 运行时等待相提示（FR-6.3：向导跳过后进程待命状态可见）。
    if view.runtime_pending {
        page = page.child(
            div()
                .child(xcontrol_i18n::tr("status.runtime_pending"))
                .text_size(px(12.))
                .text_color(muted),
        );
    }
    page
}

/// 快照 → (标签, 值, 是否错误色) 行序列（渲染顺序 = 状态页信息顺序）。
fn rows(state: &AppState) -> Vec<(String, String, bool)> {
    let idle = SharedString::from("-");
    vec![
        (
            xcontrol_i18n::tr("status.row_role").to_owned(),
            role_label(state.role),
            false,
        ),
        (
            xcontrol_i18n::tr("status.row_conn").to_owned(),
            state.conn.label(),
            false,
        ),
        (
            xcontrol_i18n::tr("status.row_peer").to_owned(),
            state.peer_name.clone().unwrap_or_else(|| idle.to_string()),
            false,
        ),
        (
            xcontrol_i18n::tr("status.row_session").to_owned(),
            state
                .session_id_hex
                .clone()
                .unwrap_or_else(|| idle.to_string()),
            false,
        ),
        (
            xcontrol_i18n::tr("status.row_events").to_owned(),
            format!("{} / {}", state.events_out, state.events_in),
            false,
        ),
        (
            xcontrol_i18n::tr("status.row_packets").to_owned(),
            format!("{} / {}", state.packets_out, state.packets_in),
            false,
        ),
        (
            xcontrol_i18n::tr("status.row_last_error").to_owned(),
            state
                .last_error
                .clone()
                .unwrap_or_else(|| xcontrol_i18n::tr("common.none").to_owned()),
            state.last_error.is_some(),
        ),
    ]
}

/// 单行：固定宽标签 + 值。
fn row(
    index: usize,
    label: &str,
    value: String,
    value_color: gpui_kit::Hsla,
    label_color: gpui_kit::Hsla,
) -> gpui_kit::Div {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(
            div()
                .w(px(LABEL_WIDTH))
                .child(SharedString::from(label.to_owned()))
                .text_color(label_color),
        )
        .child(
            div()
                .id(("status-row", index))
                .child(SharedString::from(value))
                .text_color(value_color),
        )
}

/// 角色显示名（FR-1.1：Host = 主控，Client = 从机；FR-6.5 文案表）。
fn role_label(role: Role) -> String {
    match role {
        Role::Host => xcontrol_i18n::tr("status.role_host").to_owned(),
        Role::Client => xcontrol_i18n::tr("status.role_client").to_owned(),
    }
}

/// 状态页渲染的单元测试（角色显示名、快照字段 → 行序列全覆盖）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ConnState;

    /// 角色显示名稳定（状态页文案断言依赖；11-21 术语裁定：从机 → 工作机）。
    #[test]
    fn role_labels_are_stable() {
        assert_eq!(role_label(Role::Host), "主控（Host）");
        assert_eq!(role_label(Role::Client), "工作机（Client）");
    }

    /// 快照 → 行序列：字段全覆盖、断开原因进「连接」行、无错误显示「无」、
    /// 错误行带错误色标记。
    #[test]
    fn rows_cover_snapshot_fields() {
        let state = AppState {
            role: Role::Host,
            conn: ConnState::Disconnected {
                reason: "心跳超时（9s 无帧）".to_owned(),
            },
            peer_name: Some("demo".to_owned()),
            session_id_hex: Some("abcd".to_owned()),
            events_in: 3,
            events_out: 5,
            packets_in: 7,
            packets_out: 8,
            last_error: None,
            local_screens: vec!["Display 0".to_owned(), "Display 1".to_owned()],
            host_candidates: Vec::new(),
        };
        let rows = rows(&state);
        let find = |label: &str| -> (String, bool) {
            rows.iter()
                .find(|(k, _, _)| k == label)
                .map(|(_, v, err)| (v.clone(), *err))
                .expect("行存在")
        };
        assert_eq!(find("角色").0, "主控（Host）");
        assert_eq!(
            find("连接").0,
            "Disconnected(心跳超时（9s 无帧）)",
            "断开原因含在连接行"
        );
        assert_eq!(find("对端").0, "demo");
        assert_eq!(find("会话 ID").0, "abcd");
        assert_eq!(find("事件 出/入").0, "5 / 3");
        assert_eq!(find("报文 出/入").0, "8 / 7");
        let (value, is_error) = find("最近错误");
        assert_eq!(value, "无");
        assert!(!is_error, "无错误行不是错误色");
    }

    /// 错误行：last_error 有值 → 错误色标记（状态页错误可见性）。
    #[test]
    fn error_row_is_flagged() {
        let state = AppState {
            last_error: Some("连接失败".to_owned()),
            ..AppState::default()
        };
        let rows = rows(&state);
        let (_, _, is_error) = rows
            .iter()
            .find(|(k, _, _)| k == "最近错误")
            .expect("错误行存在");
        assert!(*is_error);
    }
}
