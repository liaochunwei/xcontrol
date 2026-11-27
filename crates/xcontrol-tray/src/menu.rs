//! 托盘菜单（`gpui` feature 门控）：docs/06 §5.2 固定顺序逐项构建。
//!
//! ```text
//! 状态头（不可点）：角色 + 连接摘要
//! ─
//! 暂停共享 / 恢复共享        （FR-2.8，勾选态随状态刷新）
//! 切换角色：主控 ↔ 从机      （FR-1.1）
//! ─
//! [主控] 在线从机子菜单：名称/延迟，点击直接切换（FR-2.5）
//! [从机] 断开并重扫
//! ─
//! 打开设置…
//! 开机自启 ☑
//! ─
//! 退出
//! ```
//!
//! 动作分发（M0 spike 定稿语义，docs/06 §5.2）：原生 `MenuItem` + action——
//! 本模块声明 action 类型，处理器在 app 层经 `CoreHandle` 调用核心服务 API；
//! 禁止在 action 处理器外触碰核心状态。菜单勾选态经 `menu-state` feature 的
//! `MenuItem::checked`；Linux（zbus SNI 后端）对复选框的呈现降级由后端承载
//! （docs/06 §5.2 注记；Linux 侧实测列 ADR-0001 跟踪项）。
//!
//! 菜单项数据全部来自 [`crate::project::TrayState`]（纯投影——菜单构建不读
//! 任何核心状态，刷新时机由 app 层的快照循环承载）。

use gpui::{Menu, MenuItem, NoAction};

use crate::project::{SlaveInfo, TrayRole, TrayState};

// —— 托盘动作（docs/06 §5.2 固定顺序逐项；处理器在 app 层）——
gpui::actions!(
    xcontrol_tray,
    [
        TogglePause,
        SwitchRole,
        DisconnectRescan,
        TransferFiles,
        CancelFileTransfer,
        OpenSettings,
        ToggleAutostart,
        QuitApp
    ]
);

/// 切换到指定在线从机（FR-2.5「直接切换」的菜单路径；处理器复用 M4-T2 的
/// `LocalCmd::Switch` 路由）。载荷 = **在线从机序下标**（M4 单会话单从机拓扑
/// 恒 0；11-19-m5-layout 起会话循环经 `layout_rt::resolve_slave_target` 换算到
/// 布局装配的全局 ScreenId，越界回退原值——tray 层只投影在线序，不做布局
/// 语义，docs/02 §4.2 ScreenId 全局空间）。
#[derive(Clone, Debug, PartialEq, Eq, gpui::Action)]
#[action(namespace = xcontrol_tray, no_json)]
pub struct SwitchToSlave {
    /// 在线从机序下标（会话循环负责换算到全局 ScreenId）。
    pub target: u16,
}

/// 确认候选主控（FR-5.5「主控候选」子菜单路径；处理器发
/// `CoreCommand::ConfirmHost`——发现循环立即直连；持久化经 UI 提交路径）。
#[derive(Clone, Debug, PartialEq, Eq, gpui::Action)]
#[action(namespace = xcontrol_tray, no_json)]
pub struct ConfirmHost {
    /// 被确认主控的 instance id（32 位 hex，候选投影原值）。
    pub instance_id: String,
}

/// 在线从机子菜单标题（FR-6.5 文案表；菜单构建每 200ms 投影重建，随语言切换）。
#[must_use]
fn slaves_submenu_title() -> &'static str {
    xcontrol_i18n::tr("menu.slaves_submenu")
}
/// 无在线从机时的子菜单占位项（不可点）。
#[must_use]
fn no_slaves_placeholder() -> &'static str {
    xcontrol_i18n::tr("menu.no_slaves")
}
/// 候选主控子菜单标题（FR-5.5，M6 增项；空候选时子菜单不出现——占位项省略）。
#[must_use]
fn host_candidates_submenu_title() -> &'static str {
    xcontrol_i18n::tr("menu.host_candidates")
}
/// 切换角色菜单项文案（docs/06 §5.2 固定文案）。
#[must_use]
fn switch_role_label() -> &'static str {
    xcontrol_i18n::tr("menu.switch_role")
}

/// docs/06 §5.2 固定顺序构建菜单（输入仅 [`TrayState`]——纯投影）。
#[must_use]
pub fn build_menu(state: &TrayState) -> Vec<MenuItem> {
    let mut items = vec![
        // 状态头（不可点）：角色 + 连接摘要。
        MenuItem::action(state.header.clone(), NoAction).disabled(true),
        MenuItem::separator(),
        // 暂停/恢复共享（FR-2.8）：标签随状态、勾选态随状态（spike-b 已验证
        // checked 标记经 menu-state feature 原生呈现）。
        MenuItem::action(
            if state.paused {
                xcontrol_i18n::tr("menu.resume")
            } else {
                xcontrol_i18n::tr("menu.pause")
            },
            TogglePause,
        )
        .checked(state.paused),
        MenuItem::action(switch_role_label(), SwitchRole),
        MenuItem::separator(),
    ];
    match state.role {
        // 主控：在线从机子菜单——名称/延迟，点击直接切换（FR-2.5）。
        TrayRole::Host => items.push(slaves_submenu(&state.slaves)),
        // 从机：断开并重扫（回环退避重扫候选，FR-5.3 语义）+ 主控候选子菜单
        //（FR-5.5，M6 增项——仅在有候选时出现，空表不占菜单位）。
        TrayRole::Client => {
            items.push(MenuItem::action(
                xcontrol_i18n::tr("menu.disconnect_rescan"),
                DisconnectRescan,
            ));
            if !state.host_candidates.is_empty() {
                items.push(host_candidates_submenu(&state.host_candidates));
            }
        }
    }
    // 接收文件（FR-7.3）：对端 pending 有货才渲染（无货不占菜单位，docs/06 §5.2）。
    if let Some(files) = &state.incoming_files {
        items.push(MenuItem::action(
            xcontrol_i18n::trf(
                "menu.receive_files",
                &[
                    ("count", &files.count.to_string()),
                    ("size", &files.size_label),
                ],
            ),
            TransferFiles,
        ));
    }
    // 传输进度（FR-7.3）：拉取进行中在接收文件条目下方渲染不可点进度行
    //（原生菜单无进度条控件——文案进度与百分比，docs/06 §5.2 降级口径）。
    if let Some(progress) = &state.transfer_progress {
        items.push(
            MenuItem::action(
                xcontrol_i18n::trf(
                    "menu.transfer_progress",
                    &[
                        ("percent", &progress.percent.to_string()),
                        ("received", &progress.received_label),
                        ("total", &progress.total_label),
                    ],
                ),
                NoAction,
            )
            .disabled(true),
        );
    }
    items.extend([
        MenuItem::separator(),
        MenuItem::action(xcontrol_i18n::tr("menu.open_settings"), OpenSettings),
        MenuItem::action(xcontrol_i18n::tr("menu.autostart"), ToggleAutostart)
            .checked(state.menu_checkstates.autostart_checked),
        MenuItem::separator(),
        MenuItem::action(xcontrol_i18n::tr("menu.quit"), QuitApp),
    ]);
    items
}

/// 在线从机子菜单（名称/延迟；M4 无版本快照字段——条目文案不含版本段，
/// docs/06 §5.2 的"版本"随会话元数据 M5 补入）。
fn slaves_submenu(slaves: &[SlaveInfo]) -> MenuItem {
    let items = if slaves.is_empty() {
        vec![MenuItem::action(no_slaves_placeholder(), NoAction).disabled(true)]
    } else {
        slaves
            .iter()
            .map(|slave| MenuItem::action(slave_label(slave), SwitchToSlave { target: 0 }))
            .collect()
    };
    MenuItem::submenu(Menu::new(slaves_submenu_title()).items(items))
}

/// 候选主控子菜单（FR-5.5，复用在线从机子菜单模式）：每台候选一条目，点击
/// 确认直连；条目文案带软排除/记忆标注（"上次握手失败"为错码候选的用户可见
/// 排除标记——docs/02 §2.2 软排除语义的呈现面）。
fn host_candidates_submenu(candidates: &[crate::project::HostCandidateInfo]) -> MenuItem {
    let items: Vec<MenuItem> = candidates
        .iter()
        .map(|candidate| {
            MenuItem::action(
                candidate_label(candidate),
                ConfirmHost {
                    instance_id: candidate.instance_id_hex.clone(),
                },
            )
        })
        .collect();
    MenuItem::submenu(Menu::new(host_candidates_submenu_title()).items(items))
}

/// 候选条目文案：`[✓ ]名称/地址（来源）[ · 上次握手失败]`。
fn candidate_label(candidate: &crate::project::HostCandidateInfo) -> String {
    let mut label = String::new();
    if candidate.confirmed {
        label.push_str("✓ ");
    }
    if candidate.name.is_empty() {
        label.push_str(&candidate.addr);
    } else {
        label.push_str(&candidate.name);
        label.push_str(&xcontrol_i18n::trf(
            "menu.candidate_addr_wrap",
            &[("addr", &candidate.addr)],
        ));
    }
    if candidate.last_handshake_failed {
        label.push_str(xcontrol_i18n::tr("common.last_handshake_failed"));
    }
    label
}

/// 从机条目文案：`名称` 或 `名称 · 延迟 Nms`（延迟未知省略段）。
fn slave_label(slave: &SlaveInfo) -> String {
    match slave.latency_ms {
        Some(ms) => xcontrol_i18n::trf(
            "menu.slave_latency",
            &[("name", &slave.name), ("ms", &ms.to_string())],
        ),
        None => slave.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use gpui::Action as _;

    use super::*;
    use crate::project::{MenuCheckstates, TrayStatus};

    /// 测试投影构造（project.rs 全矩阵已覆盖投影语义——此处只造菜单输入）。
    fn state(
        role: TrayRole,
        status: TrayStatus,
        paused: bool,
        autostart: bool,
        slaves: Vec<SlaveInfo>,
    ) -> TrayState {
        let summary = match status {
            TrayStatus::Paused => "已暂停".to_owned(),
            TrayStatus::Error => "错误".to_owned(),
            TrayStatus::Online => match role {
                TrayRole::Host => format!("{} 台在线", slaves.len()),
                TrayRole::Client => "已连接".to_owned(),
            },
            TrayStatus::Searching => match role {
                TrayRole::Host => "搜索从机中".to_owned(),
                TrayRole::Client => "搜索主控中".to_owned(),
            },
        };
        TrayState {
            incoming_files: None,
            transfer_cancelable: false,
            transfer_progress: None,
            role,
            status,
            icon_kind: if status == TrayStatus::Error {
                crate::project::IconKind::Error
            } else {
                crate::project::IconKind::Normal
            },
            badge: None,
            tooltip: format!("XControl · {} · {summary}", role.label()),
            header: format!("{} · {summary}", role.label()),
            paused,
            menu_checkstates: MenuCheckstates {
                pause_checked: paused,
                autostart_checked: autostart,
            },
            slaves,
            host_candidates: Vec::new(),
        }
    }

    /// 菜单结构 = docs/06 §5.2 固定顺序（主控 × 从机两形态）。
    ///
    /// MenuItem 无公开标签读取 API——经 action 类型断言顺序（compile_menu 的
    /// 同构断言；spike-b 已验证标签以 `MenuItem::action(name, ..)` 落 NSMenu）。
    #[test]
    fn menu_order_matches_docs_06_section_5_2() {
        // 主控（有从机）：
        let slaves = vec![SlaveInfo {
            name: "mac-book".to_owned(),
            latency_ms: Some(8),
        }];
        let menu = build_menu(&state(
            TrayRole::Host,
            TrayStatus::Online,
            false,
            true,
            slaves,
        ));
        assert_eq!(
            menu.len(),
            11,
            "状态头 + 分隔 + 暂停 + 切换 + 分隔 + 子菜单 + 分隔 + 设置 + 自启 + 分隔 + 退出"
        );
        // 逐项顺序：0 状态头(NoAction, disabled) → 1 分隔 → 2 TogglePause →
        // 3 SwitchRole → 4 分隔 → 5 子菜单 → 6 分隔 → 7 OpenSettings →
        // 8 ToggleAutostart → 9 分隔 → 10 QuitApp
        let kinds: Vec<&'static str> = menu
            .iter()
            .map(|item| match item {
                MenuItem::Separator => "separator",
                MenuItem::Action { action, .. } => match action
                    .as_ref()
                    .as_any()
                    .downcast_ref::<QuitApp>()
                {
                    Some(_) => "quit",
                    None => match action.as_ref().as_any().downcast_ref::<OpenSettings>() {
                        Some(_) => "settings",
                        None => match action.as_ref().as_any().downcast_ref::<ToggleAutostart>() {
                            Some(_) => "autostart",
                            None => match action.as_ref().as_any().downcast_ref::<TogglePause>() {
                                Some(_) => "pause",
                                None => match action.as_ref().as_any().downcast_ref::<SwitchRole>()
                                {
                                    Some(_) => "switch-role",
                                    None => match action
                                        .as_ref()
                                        .as_any()
                                        .downcast_ref::<SwitchToSlave>()
                                    {
                                        Some(_) => "switch-slave",
                                        None => "other",
                                    },
                                },
                            },
                        },
                    },
                },
                MenuItem::Submenu(_) => "submenu",
                MenuItem::SystemMenu(_) => "system",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "other", // 状态头（NoAction 不可点）
                "separator",
                "pause",
                "switch-role",
                "separator",
                "submenu", // 在线从机子菜单
                "separator",
                "settings",
                "autostart",
                "separator",
                "quit",
            ]
        );

        // 从机：子菜单位置换成 断开并重扫。
        let menu = build_menu(&state(
            TrayRole::Client,
            TrayStatus::Online,
            false,
            false,
            vec![],
        ));
        let kinds: Vec<&'static str> = menu
            .iter()
            .map(|item| match item {
                MenuItem::Separator => "separator",
                MenuItem::Action { action, .. } => {
                    match action.as_ref().as_any().downcast_ref::<DisconnectRescan>() {
                        Some(_) => "rescan",
                        None => "action",
                    }
                }
                MenuItem::Submenu(_) => "submenu",
                MenuItem::SystemMenu(_) => "system",
            })
            .collect();
        assert!(
            kinds.contains(&"rescan"),
            "从机形态含 断开并重扫：{kinds:?}"
        );
        assert!(!kinds.contains(&"submenu"), "从机形态无子菜单：{kinds:?}");
    }

    /// 无在线从机：子菜单内是占位不可点项（提示性，不误导点击）。
    #[test]
    fn slaves_submenu_placeholder_when_empty() {
        let menu = build_menu(&state(
            TrayRole::Host,
            TrayStatus::Searching,
            false,
            false,
            vec![],
        ));
        let submenu = menu.iter().find_map(|item| match item {
            MenuItem::Submenu(menu) => Some(menu),
            _ => None,
        });
        let submenu = submenu.expect("主控形态含在线从机子菜单");
        assert_eq!(submenu.items.len(), 1);
        assert!(matches!(
            &submenu.items[0],
            MenuItem::Action { action, .. } if action.as_ref().as_any().downcast_ref::<NoAction>().is_some()
        ));
    }

    /// 从机条目文案：延迟段随数据（名称/延迟/版本中 M4 可得两项）。
    #[test]
    fn slave_labels_render_name_and_latency() {
        assert_eq!(
            slave_label(&SlaveInfo {
                name: "dev-box".to_owned(),
                latency_ms: Some(12),
            }),
            "dev-box · 延迟 12ms"
        );
        assert_eq!(
            slave_label(&SlaveInfo {
                name: "dev-box".to_owned(),
                latency_ms: None,
            }),
            "dev-box"
        );
    }

    /// 候选条目文案（FR-5.5）：记忆前缀 ✓ / 无名回退地址 / 软排除标注段。
    #[test]
    fn candidate_labels_render_flags() {
        use crate::project::HostCandidateInfo;
        let base = HostCandidateInfo {
            name: "mac-studio".to_owned(),
            addr: "192.168.1.10:4243".to_owned(),
            instance_id_hex: "ab".repeat(16),
            last_handshake_failed: false,
            confirmed: false,
        };
        assert_eq!(
            candidate_label(&base),
            "mac-studio（192.168.1.10:4243）",
            "普通候选：名称（地址）"
        );
        let failed = HostCandidateInfo {
            last_handshake_failed: true,
            ..base.clone()
        };
        assert!(
            candidate_label(&failed).ends_with(" · 上次握手失败"),
            "错码软排除标注：{}",
            candidate_label(&failed)
        );
        let confirmed = HostCandidateInfo {
            confirmed: true,
            ..base.clone()
        };
        assert!(candidate_label(&confirmed).starts_with("✓ "), "记忆前缀");
        let unnamed = HostCandidateInfo {
            name: String::new(),
            ..base
        };
        assert_eq!(
            candidate_label(&unnamed),
            "192.168.1.10:4243",
            "无名回退地址"
        );
    }

    /// 从机角色 + 有候选：菜单含「主控候选」子菜单（FR-5.5 呈现面）；
    /// 无候选时子菜单不出现（不占菜单位）。
    #[test]
    fn client_menu_shows_candidates_submenu_only_when_present() {
        use crate::project::HostCandidateInfo;
        let mut st = state(
            TrayRole::Client,
            TrayStatus::Searching,
            false,
            false,
            vec![],
        );
        assert!(
            !build_menu(&st)
                .iter()
                .any(|item| matches!(item, MenuItem::Submenu(_))),
            "无候选：无子菜单"
        );
        st.host_candidates = vec![HostCandidateInfo {
            name: "mac-studio".to_owned(),
            addr: "192.168.1.10:4243".to_owned(),
            instance_id_hex: "ab".repeat(16),
            last_handshake_failed: false,
            confirmed: false,
        }];
        let menu = build_menu(&st);
        let submenu = menu.iter().find_map(|item| match item {
            MenuItem::Submenu(menu) => Some(menu),
            _ => None,
        });
        let submenu = submenu.expect("有候选：含子菜单");
        assert_eq!(submenu.name, host_candidates_submenu_title());
        assert_eq!(submenu.items.len(), 1);
        assert!(
            matches!(
                &submenu.items[0],
                MenuItem::Action { action, .. }
                    if action.as_ref().as_any().downcast_ref::<ConfirmHost>().is_some()
            ),
            "候选条目携带 ConfirmHost 动作"
        );
    }

    /// SwitchToSlave 动作：载荷目标屏（M4 恒 0）+ action 元数据可读。
    #[test]
    fn switch_to_slave_carries_target() {
        let action = SwitchToSlave { target: 0 };
        assert_eq!(action.target, 0);
        // gpui Action 元数据（name 带命名空间）——gpui-tray 菜单事件按类型分发。
        assert_eq!(
            SwitchToSlave::name_for_type(),
            "xcontrol_tray::SwitchToSlave"
        );
    }

    /// 接收文件条目（FR-7.3）：incoming_files 有货时插入第 7 位（角色段之后、
    /// 设置段之前），计数 12；无货不渲染（既有 11 项 golden 不变）。
    #[test]
    fn menu_incoming_files_item_position() {
        let mut state = state(
            TrayRole::Host,
            TrayStatus::Online,
            false,
            true,
            vec![SlaveInfo {
                name: "mac-book".to_owned(),
                latency_ms: Some(8),
            }],
        );
        // 无货：11 项（既有 golden 形态）
        assert_eq!(build_menu(&state).len(), 11, "无 pending 时不渲染接收文件");
        // 有货：12 项，位次 6（0 状态头 1 分隔 2 暂停 3 切换 4 分隔 5 子菜单
        // 6 接收文件 7 分隔 8 设置 9 自启 10 分隔 11 退出）
        state.incoming_files = Some(crate::project::IncomingFilesInfo {
            count: 3,
            size_label: "128.0 MB".to_owned(),
        });
        let menu = build_menu(&state);
        assert_eq!(menu.len(), 12, "有 pending 时插入接收文件条目");
        let has_transfer = menu.iter().any(|item| match item {
            MenuItem::Action { action, .. } => action
                .as_ref()
                .as_any()
                .downcast_ref::<TransferFiles>()
                .is_some(),
            _ => false,
        });
        assert!(has_transfer, "接收文件条目携带 TransferFiles 动作");
    }

    /// 进度行（FR-7.3）：transfer_progress 有货时渲染不可点进度行（百分比 +
    /// 双体积文案）；无货不渲染（golden 计数不变）。
    #[test]
    fn menu_transfer_progress_row() {
        let mut state = state(
            TrayRole::Host,
            TrayStatus::Online,
            false,
            true,
            vec![SlaveInfo {
                name: "mac-book".to_owned(),
                latency_ms: Some(8),
            }],
        );
        // 空闲：11 项 golden 形态，无进度行
        assert_eq!(build_menu(&state).len(), 11, "空闲不渲染进度行");

        state.transfer_progress = Some(crate::project::TransferProgressInfo {
            percent: 45,
            received_label: "60.0 MB".to_owned(),
            total_label: "131.0 MB".to_owned(),
        });
        let menu = build_menu(&state);
        assert_eq!(menu.len(), 12, "进度行在接收文件条目下方 +1 项");
        let has_progress = menu.iter().any(|item| match item {
            MenuItem::Action { name, disabled, .. } => {
                *disabled && name.contains("45%") && name.contains("60.0 MB")
            }
            _ => false,
        });
        assert!(has_progress, "进度行不可点且文案含百分比与体积");
    }
}
