//! `xcontrol-app` — gpui 设置窗口 + 进程装配（单二进制、单进程，Host/Client 角色，
//! docs/01 §1）。本 lib 目标承载全部模块，`main.rs` 只做 CLI 解析与模式分发；
//! 集成测试（`tests/`）经本 lib 直测 `session_rt`（FR-1.4）。
//!
//! 架构约束（docs/01 §2、`.trellis/spec/rust-platform/ui-tray.md`）：
//!
//! - gpui 依赖只走 `gpui-kit` 一条线（ADR-0001）；进程常驻 = `QuitMode::Explicit`，
//!   关闭全部窗口 ≠ 退出进程；
//! - UI 是 `AppState` 快照的纯投影（watch channel，200ms 合并节流），变更一律走
//!   `CoreHandle` 命令（mpsc），禁止直连网络/平台层；
//! - tokio 只进 `session_rt`（网络会话执行域，docs/01 §2）；回环演示在 std 线程，
//!   两执行域经 `state.rs` 的 watch/mpsc 单向桥接（tokio::sync 原语不依赖运行时）。
//!
//! # 模块导览
//!
//! | 模块 | 内容 | 语义锚点 |
//! | --- | --- | --- |
//! | [`state`] | `AppState` 快照流 + `StateBus`（200ms 节流）+ `CoreHandle` 命令 | docs/06 §6 |
//! | [`clip_rt`] | 剪贴板运行时接线：出站驱动 + ClipRx 驱动 + EchoFilter（FR-4.x） | docs/02 §6，docs/04 §3/§5 |
//! | [`clip_channel`] | 独立剪贴板 TCP 通道：首帧分派 + 双向泵 + 3s 独立重连（FR-4.x） | docs/02 §0/§4.4 |
//! | [`logging`] | tracing 装配：stdout fmt 层 + UI 日志页环形缓冲层 | docs/06 §6、M3 前日志 |
//! | [`loopback`] | `--loopback`：进程内 M1 全链路演示（FR-1.5） | docs/08 §4 |
//! | [`session_rt`] | TCP 会话运行时：serve/connect → 握手 → 会话循环（FR-1.4） | docs/02 §3.2 |
//! | [`edge`] | 边缘切换传感器：光标判定纯状态机 + macos 查询胶水（FR-2.1/2.2） | docs/03 §5 |
//! | [`layout_rt`] | 主控布局装配/下发 + 从机映射表安装（FR-1.3 协商，M5） | docs/02 §4.2 |
//! | [`hotkey_rt`] | 热键路由纯逻辑 + 从机注入前过滤 sink + 平台热键源助手（FR-2.5） | docs/03 §4.6 |
//! | [`tray_rt`] | 托盘接线：快照 → 图标/菜单投影 + action → CoreHandle（FR-6.1） | docs/06 §5 |
//! | [`ui`] | gpui 窗口：状态页 + 日志页 + QuitMode::Explicit/on_reopen | docs/06 §2/§3 |
//!
//! 平台 feature（`macos` / `windows` / `x11` / `wayland`）是本 crate 的转发聚合口
//! （docs/08 §3：逐一转发到 xcontrol-input / xcontrol-clipboard / xcontrol-tray）；
//! M2 阶段为显式 `--features` 启用（app 的平台接线自 M3 起，构建脚本自动按目标
//! 平台选择届时落地——当前 crate 无 build.rs，勿假设自动选择已生效）。
//!
//! docs/08 §2：unsafe 仅预期出现在 input / clipboard 平台绑定层，本 crate 禁用。
#![forbid(unsafe_code)]

pub mod clip_channel;
pub mod clip_rt;
pub mod edge;
pub mod file_channel;
pub mod hotkey_rt;
pub mod layout_rt;
pub mod logging;
pub mod loopback;
pub mod session_rt;
pub mod state;
pub mod tray_rt;
pub mod ui;

#[cfg(test)]
mod tests {
    use crate::{logging, state};

    /// crate 骨架冒烟：lib 目标可链接、模块齐全（M0 任务验收项的延续）。
    #[test]
    fn crate_skeleton_smoke() {
        assert_eq!(env!("CARGO_PKG_NAME"), "xcontrol-app");
        let _ = state::hex(&[1, 2, 3]);
        let _ = logging::LogBuffer::new();
    }
}
