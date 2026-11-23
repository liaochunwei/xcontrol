# 托盘 spike：验证 QuitMode::Explicit 与 gpui-tray 兼容性

## Goal

M0 硬门禁（ADR-0001 §决策 3）：在写任何业务代码前，验证 gpui 技术栈对"托盘常驻 + 按需弹窗"形态的关键假设，并把结论回写文档。

## Requirements

1. **Spike A（QuitMode）**：最小 gpui app（依赖只写 `gpui-kit = "0.9"`）：
   - `Application::new().with_quit_mode(QuitMode::Explicit)` 能否编译（验证 gpui-pre 快照是否包含该 API）；
   - 关闭全部窗口后进程存活；`on_reopen()` 重开设置窗（macOS Dock）。
2. **Spike B（gpui-tray 共存）**：`gpui-tray` 的 `gpui-kit` feature 与 gpui-kit 0.9.x 同版本源编译共存：托盘图标出现、菜单点击事件回到 GPUI executor。
3. **Spike C（仅当 B 失败）**：Ropy 模式 demo——macOS 用 tray-icon、Linux 用 ksni，原生菜单 → `TrayCommand` 枚举 → channel → GPUI executor。
4. 结论回写 [docs/06-ui.md](../../../docs/06-ui.md) §1 决策表与 [ADR-0001](../../../docs/adr/0001-gpui-and-tray.md) 跟踪项（含失败细节与选定路径）。

## Acceptance Criteria

- [ ] Spike A 结论明确三选一（可用 / 需升级 gpui-kit / 需临时 patch），附编译产物证据
- [ ] 关窗后进程存活实测通过（macOS 必测，Linux Wayland 顺带）
- [ ] Spike B 或 C 至少一条路径跑通：托盘图标 + 菜单事件回传
- [ ] 结论（含截图/日志路径）已回写 docs/06 §1 + ADR-0001
- [ ] spike 代码放 `examples/spike-tray/` 或独立分支，不污染主 src

## Notes

- 若 A/B 两周内全败 → 触发 ADR-0001 备胎评审（iced/Tauri）；基于 Ropy 先例概率低。
- Linux 构建需 Vulkan 驱动；gpui 系 crate 必须 `opt-level=3`。

## Scope Extension（2026-11-16，用户批准）

spike 期间发现 gpui-pre 0.3.12 在 1.92/1.95 编译失败、stable 1.99.1 通过（EVIDENCE §A0）——docs/01 §6"MSRV 1.92（gpui-kit 0.9.1 要求）"系调研误判。经用户确认，**工具链基准升级 1.92 → 1.99 并入本任务**：根 `rust-toolchain.toml`、`clippy.toml` msrv、根 `Cargo.toml` rust-version、CI 安装步骤、docs/01 §6、docs/08 §2、docs/06 附录与 §5.2、规则卡（architecture-rules / ui-tray）同步修订。
