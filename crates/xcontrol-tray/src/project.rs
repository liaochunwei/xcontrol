//! `TrayAdapter` — 核心状态快照 → 图标/菜单投影（docs/06 §5.1 五态状态机）。
//!
//! 纯逻辑：输入 [`TraySnapshot`]（app 层从 `AppState` 转换的轻量类型，crate 内
//! 定义、不依赖 gpui/proto——可测性优先），输出 [`TrayState`]（图标形态/角标/
//! tooltip/菜单勾选态与菜单数据）。单测覆盖全矩阵（主控 0/1/N 台在线 × 暂停 ×
//! 错误 × 从机已连接/搜索中/暂停/错误）。
//!
//! 五态（docs/06 §5.1）：
//!
//! ```text
//! 主控: [在线(N≥1)] [搜索中(0)] [暂停] [错误]      从机: [已连接] [搜索中] [暂停] [错误]
//! ```
//!
//! 状态优先级（从高到低）：错误 > 暂停 > 在线/搜索。暂停/错误是会话级覆盖态
//! ——共享暂停（FR-2.8）时即使有从机在线也显示暂停；断开错误未恢复前显示错误。

/// 本机角色（docs/06 §5.1 双列状态机；app 层从 config `Role` 转换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrayRole {
    /// 主控（共享方）。
    #[default]
    Host,
    /// 从机（被控方）。
    Client,
}

impl TrayRole {
    /// 角色名（tooltip/状态头用；FR-6.5 文案表——随当前语言，zh-CN 默认）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Host => xcontrol_i18n::tr("tray.role_host"),
            Self::Client => xcontrol_i18n::tr("tray.role_client"),
        }
    }
}

/// 在线从机条目（主控子菜单数据，docs/06 §5.2「名称/延迟/版本」中 M4 可得的
/// 两项；版本快照字段 M4 尚无——M5 会话元数据扩展时补入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlaveInfo {
    /// 从机显示名（会话协商 `Hello` 对端名）。
    pub name: String,
    /// 平均往返延迟（ms；M4 无指标来源恒 `None`——投影层支持，指标聚合 M5）。
    pub latency_ms: Option<u32>,
}

/// 候选主控条目（FR-5.5 从机"主控候选"子菜单数据；docs/06 §5.2 M6 增项——
/// 复用在线从机子菜单模式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCandidateInfo {
    /// 主控显示名（beacon/mDNS TXT；TCP 兜底候选为空串——条目文案回退地址）。
    pub name: String,
    /// 主控服务地址（`ip:port`）。
    pub addr: String,
    /// 主控 instance id（32 位小写 hex；确认动作载荷原值回传）。
    pub instance_id_hex: String,
    /// 上次握手失败标注（FR-5.5 错码软排除的用户可见面——条目文案附段）。
    pub last_handshake_failed: bool,
    /// 已记忆（config `net.confirmed_host` 命中——文案前缀 ✓）。
    pub confirmed: bool,
}

/// 图标形态（图标方案裁定，docs/06 §5.1 M4 注记：2 张程序化生成模板图起步——
/// 常态环形 + 错误感叹号；多态预渲染图标 M6 打磨）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    /// 常态（在线/搜索中/暂停共用——状态由 tooltip + 角标表达）。
    Normal,
    /// 错误（断开未恢复）。
    Error,
}

/// 投影后的托盘状态（docs/06 §5.1 五态的归一形态；主控 Online = N≥1、从机
/// Online = 已连接）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStatus {
    /// 主控在线（N≥1）/ 从机已连接。
    Online,
    /// 主控搜索中（N=0）/ 从机搜索中。
    Searching,
    /// 共享暂停（FR-2.8）。
    Paused,
    /// 错误（断开/失败未恢复）。
    Error,
}

/// 菜单勾选态（docs/06 §5.2：暂停共享勾选随状态刷新；开机自启勾选与实际
/// 状态同步——app 层读 `Autostart::is_enabled` 填入快照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuCheckstates {
    /// 暂停共享勾选（true = 已暂停）。
    pub pause_checked: bool,
    /// 开机自启勾选。
    pub autostart_checked: bool,
}

/// 投影输入快照（app 层从 `AppState` + 自启实测状态转换；crate 内轻量类型）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraySnapshot {
    /// 本机角色。
    pub role: TrayRole,
    /// 会话运行中且未暂停（主控不读此字段——在线以 `slaves` 计；从机 = 已连接）。
    pub connected: bool,
    /// 共享暂停（FR-2.8；`ConnState::Running { paused: true }`）。
    pub paused: bool,
    /// 错误态（连接断开/失败未恢复；`ConnState::Disconnected`）。
    pub error: bool,
    /// 在线从机（主控；单会话拓扑 M4 至多 1 条）。
    pub slaves: Vec<SlaveInfo>,
    /// 候选主控（FR-5.5 从机；发现循环投影——空 = 无候选/非发现模式）。
    pub host_candidates: Vec<HostCandidateInfo>,
    /// 平均往返延迟（ms；`None` = 无指标——tooltip 省略延迟段）。
    pub latency_ms: Option<u32>,
    /// 开机自启当前生效状态（`Autostart::is_enabled`，菜单勾选态）。
    pub autostart_enabled: bool,
}

/// 投影输出（图标 + 角标 + tooltip + 菜单投影数据；菜单构建仅读本结构）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    /// 本机角色（菜单角色分支：在线从机子菜单 / 断开并重扫）。
    pub role: TrayRole,
    /// 五态归一状态（docs/06 §5.1）。
    pub status: TrayStatus,
    /// 图标形态（2 张起步方案，见 [`IconKind`]）。
    pub icon_kind: IconKind,
    /// 角标文本（docs/06 §5.1：主控在线 = 台数；错误 = 感叹号；其余 `None`。
    /// mac 经 status item title 呈现，Linux SNI 同字段，Windows 忽略）。
    pub badge: Option<String>,
    /// 悬停 tooltip（docs/06 §5.1 格式：`XControl · 主控 · 2 台在线 · 延迟 8ms`）。
    pub tooltip: String,
    /// 菜单状态头（不可点；= tooltip 去掉 `XControl · ` 前缀，docs/06 §5.2）。
    pub header: String,
    /// 共享暂停（菜单项标签 暂停共享/恢复共享 与勾选态）。
    pub paused: bool,
    /// 菜单勾选态。
    pub menu_checkstates: MenuCheckstates,
    /// 在线从机子菜单数据（主控；从机角色为空）。
    pub slaves: Vec<SlaveInfo>,
    /// 候选主控子菜单数据（FR-5.5 从机；其余角色为空）。
    pub host_candidates: Vec<HostCandidateInfo>,
}

/// 托盘投影器（无状态；`project` 为纯函数）。
#[derive(Debug, Clone, Copy, Default)]
pub struct TrayAdapter;

impl TrayAdapter {
    /// 快照 → 托盘状态投影（docs/06 §5.1 状态机；优先级 错误 > 暂停 > 在线/搜索）。
    #[must_use]
    pub fn project(snapshot: &TraySnapshot) -> TrayState {
        let role = snapshot.role;
        let status = if snapshot.error {
            TrayStatus::Error
        } else if snapshot.paused {
            TrayStatus::Paused
        } else {
            match role {
                TrayRole::Host if snapshot.slaves.is_empty() => TrayStatus::Searching,
                TrayRole::Host => TrayStatus::Online,
                TrayRole::Client if snapshot.connected => TrayStatus::Online,
                TrayRole::Client => TrayStatus::Searching,
            }
        };

        // 在线台数（主控 = 在线从机数；从机 = 0/1，仅用于文案）。
        let online = match role {
            TrayRole::Host => snapshot.slaves.len(),
            TrayRole::Client => usize::from(status == TrayStatus::Online),
        };

        // 状态摘要段（docs/06 §5.1 五态文案；FR-6.5 文案表——投影每 200ms
        // 重建，语言切换 ≤1s 反映到 tooltip/菜单头）。
        let summary = match status {
            TrayStatus::Paused => xcontrol_i18n::tr("tray.state_paused").to_owned(),
            TrayStatus::Error => xcontrol_i18n::tr("tray.state_error").to_owned(),
            TrayStatus::Online => match role {
                TrayRole::Host => {
                    xcontrol_i18n::trf("tray.state_n_online", &[("n", &online.to_string())])
                }
                TrayRole::Client => xcontrol_i18n::tr("tray.state_connected").to_owned(),
            },
            TrayStatus::Searching => match role {
                TrayRole::Host => xcontrol_i18n::tr("tray.searching_slaves").to_owned(),
                TrayRole::Client => xcontrol_i18n::tr("tray.searching_host").to_owned(),
            },
        };
        // 延迟段：仅在线态且有指标时附加（docs/06 §5.1 格式示例）。
        let mut header = format!("{} · {summary}", role.label());
        if status == TrayStatus::Online
            && let Some(ms) = snapshot.latency_ms
        {
            header.push_str(&xcontrol_i18n::trf(
                "tray.latency_segment",
                &[("ms", &ms.to_string())],
            ));
        }

        let badge = match (status, role) {
            (TrayStatus::Error, _) => Some("!".to_owned()),
            (TrayStatus::Online, TrayRole::Host) => Some(online.to_string()),
            _ => None,
        };

        TrayState {
            role,
            status,
            icon_kind: if status == TrayStatus::Error {
                IconKind::Error
            } else {
                IconKind::Normal
            },
            badge,
            tooltip: format!("XControl · {header}"),
            header,
            paused: snapshot.paused,
            menu_checkstates: MenuCheckstates {
                pause_checked: snapshot.paused,
                autostart_checked: snapshot.autostart_enabled,
            },
            slaves: snapshot.slaves.clone(),
            host_candidates: snapshot.host_candidates.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主控快照构造（全矩阵用）。
    fn host(slaves: usize, paused: bool, error: bool, latency_ms: Option<u32>) -> TraySnapshot {
        TraySnapshot {
            role: TrayRole::Host,
            connected: false, // 主控不读此字段（在线以 slaves 计）
            paused,
            error,
            slaves: (0..slaves)
                .map(|i| SlaveInfo {
                    name: format!("slave-{i}"),
                    latency_ms,
                })
                .collect(),
            host_candidates: Vec::new(),
            latency_ms,
            autostart_enabled: false,
        }
    }

    /// 从机快照构造。
    fn client(connected: bool, paused: bool, error: bool) -> TraySnapshot {
        TraySnapshot {
            role: TrayRole::Client,
            connected,
            paused,
            error,
            ..TraySnapshot::default()
        }
    }

    /// 主控全矩阵：0/1/N 台 × 暂停 × 错误 → 五态投影（docs/06 §5.1 左列）。
    #[test]
    fn host_matrix_projects_five_states() {
        // 在线 1 台
        let state = TrayAdapter::project(&host(1, false, false, None));
        assert_eq!(state.status, TrayStatus::Online);
        assert_eq!(state.icon_kind, IconKind::Normal);
        assert_eq!(state.badge.as_deref(), Some("1"));
        assert_eq!(state.tooltip, "XControl · 主控 · 1 台在线");
        assert_eq!(state.header, "主控 · 1 台在线");
        assert!(!state.paused);
        assert_eq!(state.slaves.len(), 1);

        // 在线 N 台（2/5）：角标 = 台数
        for n in [2_usize, 5] {
            let state = TrayAdapter::project(&host(n, false, false, None));
            assert_eq!(state.badge.as_deref(), Some(n.to_string().as_str()));
            assert_eq!(state.tooltip, format!("XControl · 主控 · {n} 台在线"));
        }

        // 搜索中（0 台）
        let state = TrayAdapter::project(&host(0, false, false, None));
        assert_eq!(state.status, TrayStatus::Searching);
        assert_eq!(state.tooltip, "XControl · 主控 · 搜索工作机中");
        assert_eq!(state.badge, None);
        assert_eq!(state.icon_kind, IconKind::Normal);
        assert!(state.slaves.is_empty());

        // 暂停（有从机在线也被覆盖）
        let state = TrayAdapter::project(&host(2, true, false, None));
        assert_eq!(state.status, TrayStatus::Paused);
        assert_eq!(state.tooltip, "XControl · 主控 · 已暂停");
        assert!(state.paused);
        assert!(state.menu_checkstates.pause_checked);
        assert_eq!(state.badge, None, "暂停非在线态，无台数角标");

        // 错误（最高优先级）
        let state = TrayAdapter::project(&host(2, true, true, None));
        assert_eq!(state.status, TrayStatus::Error);
        assert_eq!(state.tooltip, "XControl · 主控 · 错误");
        assert_eq!(state.icon_kind, IconKind::Error);
        assert_eq!(state.badge.as_deref(), Some("!"));
        let state = TrayAdapter::project(&host(0, false, true, None));
        assert_eq!(state.icon_kind, IconKind::Error);
    }

    /// 主控在线 + 延迟指标：tooltip 附加「 · 延迟 8ms」段（docs/06 §5.1 格式示例）。
    #[test]
    fn host_tooltip_appends_latency_when_online() {
        let state = TrayAdapter::project(&host(2, false, false, Some(8)));
        assert_eq!(state.tooltip, "XControl · 主控 · 2 台在线 · 延迟 8ms");
        // 搜索中不附加延迟段
        let state = TrayAdapter::project(&host(0, false, false, Some(8)));
        assert_eq!(state.tooltip, "XControl · 主控 · 搜索工作机中");
    }

    /// 从机全矩阵：已连接/搜索中/暂停/错误（docs/06 §5.1 右列）。
    #[test]
    fn client_matrix_projects_four_states() {
        let state = TrayAdapter::project(&client(true, false, false));
        assert_eq!(state.status, TrayStatus::Online);
        assert_eq!(state.tooltip, "XControl · 工作机 · 已连接");
        assert_eq!(state.icon_kind, IconKind::Normal);
        assert_eq!(state.badge, None, "从机已连接无角标（台数角标仅主控）");

        let state = TrayAdapter::project(&client(false, false, false));
        assert_eq!(state.status, TrayStatus::Searching);
        assert_eq!(state.tooltip, "XControl · 工作机 · 搜索主控中");

        let state = TrayAdapter::project(&client(true, true, false));
        assert_eq!(state.status, TrayStatus::Paused);
        assert_eq!(state.tooltip, "XControl · 工作机 · 已暂停");
        assert!(state.menu_checkstates.pause_checked);

        let state = TrayAdapter::project(&client(false, false, true));
        assert_eq!(state.status, TrayStatus::Error);
        assert_eq!(state.tooltip, "XControl · 工作机 · 错误");
        assert_eq!(state.icon_kind, IconKind::Error);
        assert_eq!(state.badge.as_deref(), Some("!"));
    }

    /// 从机角色不产台数角标（台数角标仅主控）；延迟段在线态通用（RTT 对称
    /// ——从机到主控的往返延迟同形附加）；子菜单数据原样透传（app 层从机角色
    /// 不填充；投影层不吞数据）。
    #[test]
    fn client_snapshot_ignores_host_only_fields() {
        let mut snapshot = client(true, false, false);
        snapshot.slaves = vec![SlaveInfo {
            name: "misleading".to_owned(),
            latency_ms: None,
        }];
        snapshot.latency_ms = Some(8);
        let state = TrayAdapter::project(&snapshot);
        assert_eq!(state.tooltip, "XControl · 工作机 · 已连接 · 延迟 8ms");
        assert_eq!(state.badge, None);
        assert_eq!(state.slaves.len(), 1);
    }

    /// 开机自启勾选态透传（菜单 ☑ 与实际状态同步，FR-6.1）。
    #[test]
    fn autostart_checkstate_passes_through() {
        let mut snapshot = TraySnapshot::default();
        assert!(
            !TrayAdapter::project(&snapshot)
                .menu_checkstates
                .autostart_checked
        );
        snapshot.autostart_enabled = true;
        assert!(
            TrayAdapter::project(&snapshot)
                .menu_checkstates
                .autostart_checked
        );
    }

    /// 菜单数据完备性：状态头/暂停标签/子菜单条目一次投影齐全（menu.rs 消费面）。
    #[test]
    fn projected_state_carries_menu_payload() {
        let mut snapshot = host(1, false, false, Some(8));
        snapshot.autostart_enabled = true;
        let state = TrayAdapter::project(&snapshot);
        assert_eq!(state.header, "主控 · 1 台在线 · 延迟 8ms");
        assert_eq!(state.slaves[0].name, "slave-0");
        assert_eq!(state.slaves[0].latency_ms, Some(8));
        assert!(state.menu_checkstates.autostart_checked);
        assert!(!state.menu_checkstates.pause_checked);
    }
}
