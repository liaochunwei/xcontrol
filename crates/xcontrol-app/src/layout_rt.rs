//! `layout_rt` — 主控布局装配与从机映射表构建（FR-1.3 协商的 app 侧执行层，
//! docs/02 §4.2「ScreenId 跨机语义」、docs/03 §5）。
//!
//! ```text
//! Host：本机布局（edge 传感器几何图）+ config [layout] 外接条目
//!       ──merge_layout──► 全局 ScreenLayout（本机 id 0..H，从机 id H..）
//!       ──set_layout + LocalCmd::PublishLayout──► CtrlMsg::Layout 下发
//! Client：收 Layout（core 缓存）→ owner==本机 条目序 → 本机屏映射表
//!       ──map_peer_screen──► EnterScreen/MouseMoveAbs 注入前换算
//! ```
//!
//! - 纯装配逻辑（[`build_host_layout`] / [`slave_screen_ids`]）无 I/O、可单测；
//!   平台查询（本机显示枚举）只在 [`install_slave_screen_map`] 内一次，失败
//!   降级单表面语义（error-handling.md 规则 4）；
//! - 默认拓扑（config `[layout]` 空）：单从机屏条目锚定主控主屏右侧——M4 语义
//!   保持（热键 Next 开箱即用，无需先配置布局）；
//! - ScreenId 语义：主控全局空间（docs/02 §4.2）——热键/托盘切换目标、
//!   SwitchRequest/EnterScreen 的 target 一律取 [`LayoutPlan::slave_targets`]
//!   中的全局 id；从机经映射表换算到本机屏。

use xcontrol_config::LayoutConfig;
use xcontrol_core::layout::{ScreenLayout, merge_layout};
use xcontrol_core::session::{Role, Session};
use xcontrol_proto::ScreenInfo;
use xcontrol_proto::input::ScreenId;

/// 主控布局装配产物（下发载荷 + 切换目标序）。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutPlan {
    /// 全局布局（`CtrlMsg::Layout` 载荷来源）。
    pub layout: ScreenLayout,
    /// 外接（从机）屏条目 id（配置顺序；热键/托盘切换的目标空间）。
    pub slave_targets: Vec<ScreenId>,
}

/// 主控布局装配（FR-1.3 方位制最小模型）。
///
/// - `local` = 本机布局（mac：edge 传感器几何图；None/空 = 非 macos 或无屏 →
///   装配单屏锚点 `{id: 0, name: screen_name}`，保证布局图有锚）；
/// - `local_owner`/`externals_owner` = 主控/从机 instance id（`ScreenInfo.owner`
///   归属字段，docs/02 §4.3）；
/// - `config` = `[layout]` 条目；**空表时取默认拓扑**：单条目
///   `{id: 对端名, direction: Right}` 锚定主控主屏（M4 单从机右侧语义延续）。
#[must_use]
pub fn build_host_layout(
    local: Option<&ScreenLayout>,
    local_owner: [u8; 16],
    local_screen_name: &str,
    externals_owner: [u8; 16],
    peer_name: Option<&str>,
    config: &LayoutConfig,
) -> LayoutPlan {
    // 本机布局为空（非 macos/查询失败）→ 单屏锚点（主控至少有一块屏才能当主控）。
    let fallback = ScreenLayout::new(vec![ScreenInfo {
        id: 0,
        owner: local_owner,
        name: local_screen_name.to_owned(),
        left: None,
        right: None,
        top: None,
        bottom: None,
        scale: 1.0,
    }]);
    let local = match local {
        Some(layout) if !layout.is_empty() => layout,
        _ => &fallback,
    };
    // 默认拓扑（FR-1.3 最小可用）：未配置布局 → 从机屏在主控主屏右侧。
    let default_topo = [xcontrol_config::LayoutScreen {
        id: peer_name.unwrap_or("slave").to_owned(),
        relative_to: None,
        direction: Some(xcontrol_config::Direction::Right),
    }];
    let externals: &[xcontrol_config::LayoutScreen] = if config.screens.is_empty() {
        &default_topo
    } else {
        &config.screens
    };
    let layout = merge_layout(local, local_owner, externals_owner, externals);
    let local_len = local.len();
    let slave_targets = (local_len as ScreenId..layout.len() as ScreenId).collect();
    LayoutPlan {
        layout,
        slave_targets,
    }
}

/// 从机映射表安装（docs/02 §4.2：`owner == 本机` 条目序 → 本机显示枚举序）。
///
/// 先清空旧表再按本次 `Layout` 全量重灌（布局更新 = 映射表替换语义；增量合并
/// 会残留已从布局摘除的旧 id 映射——`Session::clear_screen_map` 契约）。
/// 本机屏数查询失败降级 1（单表面语义——win/x11/wayland 注入层本就单表面；
/// mac 为真实枚举数）。越出本机屏数的条目不映射（该 id 的帧按「未映射」直通）。
/// 返回安装的映射条数（观测日志用）。
pub fn install_slave_screen_map<C: xcontrol_core::clock::Clock>(
    session: &mut Session<C>,
    local_instance_id: [u8; 16],
) -> usize {
    session.clear_screen_map();
    let local_count = local_display_count();
    let own = session.layout().ids_owned_by(&local_instance_id);
    let mut installed = 0;
    for (index, peer_id) in own.into_iter().enumerate() {
        let local_id = index as ScreenId;
        if index >= local_count {
            tracing::warn!(target: "session::rt", peer_id,
                "布局条目越出本机屏数（{local_count}），不映射（该 id 帧按未映射直通）");
            break;
        }
        session.map_peer_screen(peer_id, local_id);
        installed += 1;
    }
    installed
}

/// 本机显示枚举数（macos：真实查询，失败按 1；其余平台恒 1 = 单表面语义）。
fn local_display_count() -> usize {
    #[cfg(feature = "macos")]
    {
        xcontrol_input::display_layout().map_or(1, |infos| infos.len().max(1))
    }
    #[cfg(not(feature = "macos"))]
    {
        1
    }
}

/// 会话内布局接线（drive_session 调用；按角色分流）：
///
/// - [`Notice::HandshakeComplete`](xcontrol_core::session::Notice)（Host）：装配
///   布局 + `LocalCmd::PublishLayout`（出站动作交调用方发送）；
/// - [`Notice::LayoutReceived`](xcontrol_core::session::Notice)（Client）：安装
///   ScreenId → 本机屏映射表（无出站）。
///
/// 返回 (Host 待发动作, 更新后的从机目标序)；无相关通知 → 空。
pub fn handle_layout_notices<C: xcontrol_core::clock::Clock>(
    session: &mut Session<C>,
    actions: &[xcontrol_core::session::SessionAction],
    identity: &xcontrol_core::session::Identity,
    local_layout: Option<&ScreenLayout>,
    config: &LayoutConfig,
) -> (Vec<xcontrol_core::session::SessionAction>, Vec<ScreenId>) {
    use xcontrol_core::session::{Notice, SessionAction};

    let mut followups: Vec<SessionAction> = Vec::new();
    let mut slave_targets = Vec::new();
    for action in actions {
        match action {
            SessionAction::Notice(Notice::HandshakeComplete { .. }) => {
                if session.role() == Role::Host {
                    let plan = build_host_layout(
                        local_layout,
                        identity.instance_id,
                        &identity.screen_name,
                        session.peer_instance_id().unwrap_or(identity.instance_id),
                        session.peer_name(),
                        config,
                    );
                    tracing::info!(target: "session::rt",
                        screens = plan.layout.len(),
                        slaves = plan.slave_targets.len(),
                        "主控布局已装配（本机 + 从机屏，ScreenId 全局空间）");
                    session.set_layout(plan.layout);
                    slave_targets = plan.slave_targets;
                    followups = run_publish(session);
                }
            }
            SessionAction::Notice(Notice::LayoutReceived { screens }) => {
                tracing::info!(target: "session::rt", screens,
                    "收到主控布局：安装 ScreenId → 本机屏映射表（docs/02 §4.2）");
                let installed = install_slave_screen_map(session, identity.instance_id);
                tracing::debug!(target: "session::rt", installed, "映射表条目安装完成");
            }
            _ => {}
        }
    }
    (followups, slave_targets)
}

/// 会话内布局重装配 + 下发（`RuntimeConfigUpdate` 到达的 Host 分支）。
///
/// 返回 (待发动作, 从机目标序)；未握手/无对端时空（握手时按下会话快照装配）。
pub fn rebuild_and_publish<C: xcontrol_core::clock::Clock>(
    session: &mut Session<C>,
    identity: &xcontrol_core::session::Identity,
    local_layout: Option<&ScreenLayout>,
    config: &LayoutConfig,
) -> (Vec<xcontrol_core::session::SessionAction>, Vec<ScreenId>) {
    if session.role() != Role::Host || session.peer_instance_id().is_none() {
        return (Vec::new(), Vec::new());
    }
    let plan = build_host_layout(
        local_layout,
        identity.instance_id,
        &identity.screen_name,
        session.peer_instance_id().unwrap_or(identity.instance_id),
        session.peer_name(),
        config,
    );
    tracing::info!(target: "session::rt", screens = plan.layout.len(),
        slaves = plan.slave_targets.len(),
        "布局配置变更：重装配并重新下发（docs/06 §4.1 layout 段生效时点）");
    session.set_layout(plan.layout);
    (run_publish(session), plan.slave_targets)
}

/// `LocalCmd::PublishLayout` 容错执行（被状态机拒绝记日志跳过——error-handling.md
/// 规则 4，布局下发不中断会话）。
fn run_publish<C: xcontrol_core::clock::Clock>(
    session: &mut Session<C>,
) -> Vec<xcontrol_core::session::SessionAction> {
    use xcontrol_core::session::LocalCmd;
    let mut dispatcher = xcontrol_core::route::Dispatcher::new(session);
    match dispatcher.on_local(LocalCmd::PublishLayout) {
        Ok(actions) => actions,
        Err(error) => {
            tracing::warn!(target: "session::rt", %error,
                "PublishLayout 被状态机拒绝（协商未完成或已断开；握手/下会话重试）");
            Vec::new()
        }
    }
}

/// 从机目标 id 的守卫直通（`SwitchToSlave.target` = 在线从机序下标；越界回退
/// 原值——M4 直通语义兼容，docs/02 §4.2 ScreenId 全局空间的目标解析点）。
#[must_use]
pub fn resolve_slave_target(slave_targets: &[ScreenId], index: ScreenId) -> ScreenId {
    slave_targets
        .get(usize::from(index))
        .copied()
        .unwrap_or(index)
}

#[cfg(test)]
mod tests {
    use xcontrol_config::LayoutScreen;
    use xcontrol_core::clock::TestClock;
    use xcontrol_core::session::{Identity, Role};
    use xcontrol_proto::input::ScreenEdge;
    use xcontrol_proto::{Caps, Options, ScreenInfo};

    use super::*;

    fn local_single(owner: [u8; 16]) -> ScreenLayout {
        ScreenLayout::new(vec![ScreenInfo {
            id: 0,
            owner,
            name: "Display 0".into(),
            left: None,
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        }])
    }

    fn identity(instance: u8) -> Identity {
        Identity {
            name: "self".into(),
            screen_name: "self-screen".into(),
            instance_id: [instance; 16],
            caps: Caps::UNICODE,
            salt: [0; 16],
        }
    }

    fn host_session(instance: u8) -> Session<TestClock> {
        Session::new(
            Role::Host,
            identity(instance),
            TestClock::new(),
            Options::defaults(),
        )
    }

    /// 默认拓扑：config 空 → 单从机条目锚主控主屏右侧（M4 语义延续）。
    #[test]
    fn default_topology_places_slave_right() {
        let plan = build_host_layout(
            Some(&local_single([1; 16])),
            [1; 16],
            "self-screen",
            [2; 16],
            Some("sofa"),
            &LayoutConfig::default(),
        );
        assert_eq!(plan.slave_targets, vec![1]);
        assert_eq!(plan.layout.neighbor(0, ScreenEdge::Right), Some(1));
        assert_eq!(plan.layout.screen(1).map(|s| s.name.as_str()), Some("sofa"));
        assert_eq!(
            plan.layout.screen(1).map(|s| s.owner),
            Some([2; 16]),
            "外接条目 owner = 从机 instance id"
        );
    }

    /// config 条目驱动 + peer 名缺省；目标序 = 本机屏之后的连续 id。
    #[test]
    fn config_entries_drive_topology() {
        let config = LayoutConfig {
            screens: vec![LayoutScreen {
                id: "desk-right".into(),
                relative_to: None,
                direction: Some(xcontrol_config::Direction::Right),
            }],
        };
        let plan = build_host_layout(
            Some(&local_single([1; 16])),
            [1; 16],
            "self-screen",
            [2; 16],
            None,
            &config,
        );
        assert_eq!(plan.slave_targets, vec![1]);
        assert_eq!(
            plan.layout.screen(1).map(|s| s.name.as_str()),
            Some("desk-right")
        );
        // 本机双屏：目标序从 2 起
        let two = ScreenLayout::new(vec![
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
        let plan = build_host_layout(Some(&two), [1; 16], "s", [2; 16], None, &config);
        assert_eq!(plan.slave_targets, vec![2]);
        // 条目 relative_to 缺省 → 锚 = 主控主屏 id 0（merge_layout 锚点解析契约，
        // core/layout.rs：缺省/不匹配 → 主屏 0）；非主屏的本机屏 1 无右邻。
        assert_eq!(plan.layout.neighbor(0, ScreenEdge::Right), Some(2));
        assert_eq!(plan.layout.neighbor(1, ScreenEdge::Right), None);
    }

    /// 本机布局缺失（非 macos/无屏）→ 单屏锚点兜底（布局图恒有锚）。
    #[test]
    fn missing_local_layout_falls_back_to_single_anchor() {
        let plan = build_host_layout(
            None,
            [1; 16],
            "self-screen",
            [2; 16],
            Some("peer"),
            &LayoutConfig::default(),
        );
        assert_eq!(plan.layout.len(), 2, "单屏锚点 + 默认拓扑从机条目");
        assert_eq!(plan.slave_targets, vec![1]);
    }

    /// resolve_slave_target：下标解析 + 越界回退原值（M4 直通兼容）。
    #[test]
    fn slave_target_resolution_and_fallback() {
        let targets = [3u16, 7];
        assert_eq!(resolve_slave_target(&targets, 0), 3);
        assert_eq!(resolve_slave_target(&targets, 1), 7);
        assert_eq!(resolve_slave_target(&targets, 2), 2, "越界回退原值");
    }

    /// 布局更新重灌 = 映射表替换语义（clear_screen_map 前置）：已从布局摘除的
    /// 旧 id 不得残留旧映射（否则越界帧被错误换算；docs/02 §4.2 从机映射表）。
    /// 安装条数随本机实机屏数浮动（mac 真实枚举/CI 单表面），旧映射用受控种子
    /// 复现，重灌断言与实机屏数无关。
    #[test]
    fn reinstall_replaces_stale_mappings() {
        let mut session = host_session(2);
        let owned = |id, name: &str, left| ScreenInfo {
            id,
            owner: [3; 16],
            name: name.to_owned(),
            left,
            right: None,
            top: None,
            bottom: None,
            scale: 1.0,
        };
        // 首次布局：从机条目 id 2、3（本机屏 id 0 起，越出本机屏数不映射）。
        session.set_layout(ScreenLayout::new(vec![
            ScreenInfo {
                id: 0,
                owner: [2; 16],
                name: "Display 0".into(),
                left: None,
                right: Some(2),
                top: None,
                bottom: None,
                scale: 1.0,
            },
            owned(2, "sofa", Some(0)),
            owned(3, "desk", Some(2)),
        ]));
        let installed = install_slave_screen_map(&mut session, [3; 16]);
        assert!(installed >= 1, "本机屏数 ≥1，首块从机屏必可映射");
        assert_eq!(
            session.screen_map().first(),
            Some(&(2, 0)),
            "首条 owner 匹配条目 → 本机屏 0"
        );
        // 受控种子：模拟首次布局含 id 3 的安装结果（id 3 → 本机屏 1）。
        session.map_peer_screen(3, 1);

        // 布局更新：摘除 desk（旧 id 3）——重灌后旧映射必须消失，而非残留 (3,1)。
        session.set_layout(ScreenLayout::new(vec![
            ScreenInfo {
                id: 0,
                owner: [2; 16],
                name: "Display 0".into(),
                left: None,
                right: Some(2),
                top: None,
                bottom: None,
                scale: 1.0,
            },
            owned(2, "sofa", Some(0)),
        ]));
        assert_eq!(install_slave_screen_map(&mut session, [3; 16]), 1);
        assert_eq!(
            session.screen_map(),
            &[(2, 0)],
            "旧 id 3 的映射已随重灌清除（替换语义，非合并）"
        );
    }

    /// handle_layout_notices（Host）：协商完成通知 → 装配布局 + PublishLayout
    /// 出站动作 + 目标序返回；PublishLayout 被状态机拒（未协商）时容错为空。
    #[test]
    fn host_layout_notices_install_and_publish() {
        use xcontrol_core::session::{FramePayload, SessionAction, SessionEvent};
        use xcontrol_proto::{CtrlMsg, ProtoVer};

        let mut session = host_session(1);
        // 未协商：HandshakeComplete 装配后 PublishLayout 被拒 → 容错空动作。
        let (actions, targets) = handle_layout_notices(
            &mut session,
            &[SessionAction::Notice(
                xcontrol_core::session::Notice::HandshakeComplete {
                    session_id: [0; 16],
                    minor: 1,
                },
            )],
            &identity(1),
            Some(&local_single([1; 16])),
            &LayoutConfig::default(),
        );
        assert!(actions.is_empty(), "未协商 PublishLayout 容错为空");
        assert_eq!(targets, vec![1], "目标序已装配");

        // 完成协商（Hello→HelloBack）后再投同一通知：PublishLayout 出站 Layout 帧。
        session
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .unwrap();
        let back = CtrlMsg::HelloBack {
            proto: ProtoVer::CURRENT,
            name: "client".into(),
            instance_id: [2; 16],
            caps: Caps::UNICODE,
        };
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(back)))
            .unwrap();
        let (actions, targets) = handle_layout_notices(
            &mut session,
            &[SessionAction::Notice(
                xcontrol_core::session::Notice::HandshakeComplete {
                    session_id: [0; 16],
                    minor: 1,
                },
            )],
            &identity(1),
            Some(&local_single([1; 16])),
            &LayoutConfig::default(),
        );
        assert_eq!(targets, vec![1]);
        assert_eq!(actions.len(), 1, "协商完成：PublishLayout 出站单帧");
        assert!(matches!(
            &actions[0],
            SessionAction::Send {
                msg: xcontrol_core::session::Outbound::Ctrl(CtrlMsg::Layout { screens }),
                ..
            } if screens.len() == 2
        ));
        // Peer 元数据已被 HelloBack 设置：默认拓扑条目名 = 对端名。
        assert_eq!(
            session.layout().screen(1).map(|s| s.name.as_str()),
            Some("client")
        );
    }
}
