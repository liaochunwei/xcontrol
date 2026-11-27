//! 托盘控制器（`gpui` feature 门控）：gpui-tray `TrayIcon` 生命周期 + 状态驱动
//! 的图标/tooltip/角标/菜单刷新（docs/06 §5、spike-b 实测 API 锚）。
//!
//! 生命周期（app 启动创建 / 退出销毁，FR-6.1）：
//!
//! ```text
//! TrayController::build(provider, cx)   # 图标 + 固定顺序菜单 + title/tooltip 初值
//! controller.update(cx)                 # 快照循环逐拍调用：diff → set_tooltip/set_title/
//!                                       #   set_icon/refresh_menu（≤1s 反映，FR-6.1 AC）
//! controller.close(cx)                  # 退出路径（托盘菜单「退出」action 处理器）
//! ```
//!
//! 纯投影红线：控制器只读快照——菜单构建器经 [`SnapshotProvider`] 拉最新
//! [`TraySnapshot`]（app 层由 StateBus 快照转换），图标/菜单不持有任何核心
//! 状态；动作经 gpui 原生 action 分发回 app 层（`CoreHandle`）。
//!
//! 单写者纪律（本模块裁定）：`update` 只应由**一个**控制器实例承载（app 层的
//! 快照刷新循环持有一个 `Clone`——`Tray` 句柄共享同一原生托盘，`applied` 差分
//! 基线归该刷新循环独占；gpui-tray 在每次菜单 action 分发后自带
//! `refresh_menu`，勾选/标签的即时修正由下一拍差分收敛，≤200ms）。全局存档的
//! 控制器实例仅用于 `close`（退出路径）。
//!
//! `Tray` 非 `Send`（原生托盘与 gpui 主线程绑定）——控制器仅在 gpui 主线程
//! 使用（构建/更新/销毁全部在 `App` 上下文内）。

use std::cell::RefCell;
use std::sync::Arc;

use gpui::App;

use crate::icons;
use crate::project::{IconKind, TrayAdapter, TrayState};

/// 状态快照提供器（app 层实现：StateBus 快照 + 自启实测状态 → [`TraySnapshot`]）。
///
/// 调用时机：菜单构建（含 gpui-tray 在 action 分发后的自带刷新）与 [`TrayController::update`]
/// 的每拍投影——必须是非阻塞纯读（StateBus `current()` 语义）。
pub type SnapshotProvider = Arc<dyn Fn() -> crate::project::TraySnapshot + Send + Sync>;

/// 托盘 status item 常显标题（角标缺省时的稳定标识；mac AX 自动化以
/// description "status menu" 定位，title 仅兜底）。
pub const TRAY_TITLE: &str = "XControl";

/// 一次 [`TrayController::update`] 的变更摘要（差分结果；app 层据此落观测
/// 日志——本 crate 零日志依赖，事件交由消费方记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayUpdate {
    /// tooltip 变更（已应用）。
    pub tooltip: bool,
    /// 角标（title）变更（已应用）。
    pub badge: bool,
    /// 图标变更（已应用）。
    pub icon: bool,
    /// 菜单重建（已应用）。
    pub menu: bool,
}

impl TrayUpdate {
    /// 是否发生了任何变更。
    #[must_use]
    pub fn any(self) -> bool {
        self.tooltip || self.badge || self.icon || self.menu
    }
}

/// 托盘控制器（可 Clone——`Tray` 句柄共享同一原生托盘；`applied` 差分基线
/// 随 Clone 复制，单写者纪律见模块文档）。
#[derive(Clone)]
pub struct TrayController {
    tray: gpui_tray::Tray,
    provider: SnapshotProvider,
    /// 最近一次应用的状态（差分基线；`None` = 尚未应用过）。
    applied: RefCell<Option<TrayState>>,
}

impl TrayController {
    /// 构建托盘（图标 + 固定顺序菜单 + 初始 title/tooltip）。
    ///
    /// # Errors
    /// gpui-tray 托盘创建失败（平台状态栏资源不可用等）或图标构建失败。
    pub fn build(provider: SnapshotProvider, cx: &mut App) -> gpui_tray::Result<Self> {
        let menu_provider = Arc::clone(&provider);
        let icon = icons::icon(IconKind::Normal)?;
        let tray = gpui_tray::Tray::builder()
            .icon(icon)
            .title(TRAY_TITLE)
            .tooltip(TRAY_TITLE)
            .menu(move |_cx| {
                // 菜单构建器：拉最新快照 → 投影 → 固定顺序菜单（docs/06 §5.2）。
                // 纯读快照（StateBus current 语义），无阻塞、无核心状态写入。
                let snapshot = (menu_provider)();
                crate::menu::build_menu(&TrayAdapter::project(&snapshot))
            })
            .build(cx)?;
        Ok(Self {
            tray,
            provider,
            applied: RefCell::new(None),
        })
    }

    /// 按最新快照刷新托盘（图标/tooltip/角标/菜单按差分最小化更新——与上次
    /// 应用状态一致的部分不触碰原生对象）。
    ///
    /// # Errors
    /// gpui-tray 更新失败（托盘已 close / 跨线程调用）——调用方（快照循环）
    /// 收到错误即停止循环。
    pub fn update(&self, cx: &mut App) -> gpui_tray::Result<TrayUpdate> {
        let snapshot = (self.provider)();
        let state = TrayAdapter::project(&snapshot);
        let previous = self.applied.borrow().clone();

        // tooltip（docs/06 §5.1 格式，投影层产出）。
        let tooltip = previous
            .as_ref()
            .is_none_or(|prev| prev.tooltip != state.tooltip);
        if tooltip {
            self.tray.set_tooltip(Some(state.tooltip.as_str()), cx)?;
        }
        // 角标（mac title；None = 移除）。
        let badge = previous
            .as_ref()
            .is_none_or(|prev| prev.badge != state.badge);
        if badge {
            self.tray.set_title(state.badge.as_deref(), cx)?;
        }
        // 图标（2 张模板图二选一）。
        let icon = previous
            .as_ref()
            .is_none_or(|prev| prev.icon_kind != state.icon_kind);
        if icon {
            self.tray.set_icon(icons::icon(state.icon_kind)?, cx)?;
        }
        // 菜单（勾选态/标签/子菜单条目变化 → 重建；等价判定见 `menu_equivalent`）。
        let menu = previous
            .as_ref()
            .is_none_or(|prev| !menu_equivalent(prev, &state));
        if menu {
            self.tray.refresh_menu(cx)?;
        }

        *self.applied.borrow_mut() = Some(state);
        Ok(TrayUpdate {
            tooltip,
            badge,
            icon,
            menu,
        })
    }

    /// 销毁托盘（幂等——gpui-tray `close` 语义）。
    ///
    /// # Errors
    /// 托盘已 close（幂等成功）之外的失败（跨线程调用）。
    pub fn close(&self, cx: &mut App) -> gpui_tray::Result<()> {
        self.tray.close(cx)
    }
}

/// 菜单等价判定：影响菜单内容的字段集合（角色/状态头/暂停/勾选态/从机条目）。
/// tooltip/角标变化不触发菜单重建（菜单重建有原生 NSMenu 重组成本，按需最小化）。
fn menu_equivalent(previous: &TrayState, current: &TrayState) -> bool {
    previous.role == current.role
        && previous.header == current.header
        && previous.paused == current.paused
        && previous.menu_checkstates == current.menu_checkstates
        && previous.slaves == current.slaves
        && previous.host_candidates == current.host_candidates
        && previous.incoming_files == current.incoming_files
        && previous.transfer_cancelable == current.transfer_cancelable
        && previous.transfer_progress == current.transfer_progress
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{
        IconKind, MenuCheckstates, SlaveInfo, TrayRole, TraySnapshot, TrayStatus,
    };

    /// 测试投影构造。
    fn state(role: TrayRole, status: TrayStatus, autostart: bool) -> TrayState {
        let slaves = match (role, status) {
            (TrayRole::Host, TrayStatus::Online) => vec![SlaveInfo {
                name: "peer".to_owned(),
                latency_ms: None,
            }],
            _ => vec![],
        };
        let summary = match status {
            TrayStatus::Paused => "已暂停".to_owned(),
            TrayStatus::Error => "错误".to_owned(),
            TrayStatus::Online => match role {
                TrayRole::Host => "1 台在线".to_owned(),
                TrayRole::Client => "已连接".to_owned(),
            },
            TrayStatus::Searching => match role {
                TrayRole::Host => "搜索从机中".to_owned(),
                TrayRole::Client => "搜索主控中".to_owned(),
            },
        };
        TrayState {
            role,
            status,
            icon_kind: if status == TrayStatus::Error {
                IconKind::Error
            } else {
                IconKind::Normal
            },
            badge: None,
            tooltip: format!("XControl · {} · {summary}", role.label()),
            header: format!("{} · {summary}", role.label()),
            paused: status == TrayStatus::Paused,
            menu_checkstates: MenuCheckstates {
                pause_checked: status == TrayStatus::Paused,
                autostart_checked: autostart,
            },
            slaves,
            host_candidates: Vec::new(),
            incoming_files: None,
            transfer_cancelable: false,
            transfer_progress: None,
        }
    }

    /// 菜单等价判定：内容字段变化才重建；tooltip/角标变化不触发。
    #[test]
    fn menu_equivalence_ignores_tooltip_and_badge() {
        let base = state(TrayRole::Host, TrayStatus::Online, false);
        // 同内容（tooltip/角标不同——badge 在本构造恒 None，直接改字段比对）
        let mut same = base.clone();
        same.tooltip = "XControl · 别的文案".to_owned();
        assert!(menu_equivalent(&base, &same), "tooltip 变化不重建菜单");

        // 暂停翻转 → 重建
        let paused = state(TrayRole::Host, TrayStatus::Paused, false);
        assert!(!menu_equivalent(&base, &paused));

        // 自启勾选翻转 → 重建
        let toggled = state(TrayRole::Host, TrayStatus::Online, true);
        assert!(!menu_equivalent(&base, &toggled));

        // 从机条目变化 → 重建
        let mut with_slave = base.clone();
        with_slave.slaves.push(SlaveInfo {
            name: "second".to_owned(),
            latency_ms: None,
        });
        assert!(!menu_equivalent(&base, &with_slave));

        // 状态头（摘要文案）变化 → 重建
        let searching = state(TrayRole::Host, TrayStatus::Searching, false);
        assert!(!menu_equivalent(&base, &searching));
    }

    /// 快照默认值经投影可得（provider 契约的退化形态；全矩阵在 project.rs）。
    #[test]
    fn provider_contract_projects_default_snapshot() {
        let state = TrayAdapter::project(&TraySnapshot::default());
        assert_eq!(state.status, TrayStatus::Searching);
        assert_eq!(state.role, TrayRole::Host);
    }
}
