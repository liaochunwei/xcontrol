//! `xcontrol-tray` — 托盘适配层（docs/06 §5、ADR-0001）：核心状态快照 → 图标/菜单
//! 投影的适配层 + 三平台开机自启。
//!
//! 语义收敛（M0 spike 后的 docs/06 §1 定稿）：本 crate 是"快照 → 投影"的纯适配层，
//! **不含平台胶水**——gpui-tray 直连（ADR-0001），托盘事件流唯一合法路径：
//!
//! ```text
//! gpui-tray 菜单 action（原生 MenuItem+action 分发）→ GPUI executor（天然主线程）
//!   → app 层 action 处理器 → CoreHandle 命令
//! ```
//!
//! - 图标/菜单只读快照（纯投影红线）：[`project::TraySnapshot`] 由 app 层从
//!   `AppState` 转换，[`TrayAdapter::project`] 产出 [`project::TrayState`]（图标
//!   形态/角标/tooltip/菜单勾选态）——单测全矩阵（docs/06 §5.1 五态）；
//! - 菜单为 docs/06 §5.2 固定顺序（状态头不可点 → 暂停/恢复勾选 → 切换角色 →
//!   在线从机子菜单（主控）/断开并重扫（从机）→ 打开设置 → 开机自启 ☑ → 退出），
//!   构建于 [`menu`]（`gpui` feature 门控）；
//! - 动作经 gpui 原生 action 分发（[`menu`] 声明的 action 类型），处理器在 app 层
//!   经 `CoreHandle` 调用核心服务 API——禁止在 action 处理器外触碰核心状态；
//! - 自启（FR-6.1）：[`Autostart`] trait + 三平台后端（feature 门控，
//!   [`autostart`] 模块文档）。
//!
//! # feature 矩阵
//!
//! | feature | 门控内容 |
//! | --- | --- |
//! | （默认） | 纯逻辑：`project.rs`（状态投影）+ `autostart/` 纯函数与平台后端 |
//! | `macos` | launchd agent 自启后端 |
//! | `windows` | HKCU Run 键自启后端 |
//! | `x11` / `wayland` | linux 桌面会话 XDG autostart 后端（两 feature 同一实现） |
//! | `gpui` | gpui-tray 集成（icons/menu/tray + action 声明）——仅 xcontrol-app 开启 |
//!
//! `gpui` feature 关闭时（linux CI 排除 app 的编译面），本 crate 纯 std + thiserror，
//! 无 gpui 系依赖进树——CI linux 步骤无需 exclude 本 crate。
//!
//! docs/08 §2：本 crate 全程 `#![forbid(unsafe_code)]`（windows-registry 为 safe API，
//! mac/linux 自启纯 std）。

#![forbid(unsafe_code)]

pub mod autostart;
pub mod project;

// gpui-tray 的 gpui-kit 后端期望消费方以 `gpui` 之名引用 gpui-kit 类型（gpui 宏展开
// 产生 `::gpui::` 路径），同时保证本 crate 与 gpui-tray 共享同一批类型（gpui-pre
// 0.3.12 单一版本源）——examples/spike-tray/src/bin/spike-b.rs 实测锚同形。
#[cfg(feature = "gpui")]
extern crate gpui_kit as gpui;

#[cfg(feature = "gpui")]
pub mod icons;
#[cfg(feature = "gpui")]
pub mod menu;
#[cfg(feature = "gpui")]
pub mod tray;

// —— 纯逻辑区 re-export（默认编译面）——
pub use autostart::{Autostart, AutostartError, platform_autostart};
// —— gpui 集成区 re-export（gpui feature）——
#[cfg(feature = "gpui")]
pub use menu::{
    CancelFileTransfer, ConfirmHost, DisconnectRescan, OpenSettings, QuitApp, SwitchRole,
    SwitchToSlave, ToggleAutostart, TogglePause, TransferFiles,
};
pub use project::{
    HostCandidateInfo, IconKind, IncomingFilesInfo, MenuCheckstates, SlaveInfo,
    TransferProgressInfo, TrayAdapter, TrayRole, TraySnapshot, TrayState, TrayStatus,
};
#[cfg(feature = "gpui")]
pub use tray::{SnapshotProvider, TrayController, TrayUpdate};

#[cfg(test)]
mod tests {
    /// crate 骨架冒烟：lib 目标可链接、模块齐全（M0 任务验收项的延续）。
    #[test]
    fn crate_skeleton_smoke() {
        assert_eq!(env!("CARGO_PKG_NAME"), "xcontrol-tray");
        // 纯投影在默认编译面即可用（无 gpui 依赖）。
        let state = crate::project::TrayAdapter::project(&crate::project::TraySnapshot::default());
        assert_eq!(state.role, crate::project::TrayRole::Host);
    }
}
