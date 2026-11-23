# M2: 最小 gpui 窗口与 --loopback

## Goal

`xcontrol-app`：最小 gpui 窗口（连接状态 + 日志页）+ `--loopback` 进程内演示 + 安全码握手实战（FR-1.4 localhost）——M2 的集成收口，第一次能看到 XControl "长什么样"。

## Requirements

1. **gpui 接入**：`gpui-kit = "0.9"`（default-features 按需）——**gpui 依赖首次入主 workspace**：双登记 + xtask 白名单（app 条目）+ **GPUI 编译告警应消失**（M0 预置的 profile.dev.package.gpui opt-level=3 终于命中，验证）。Linux CI 无 Vulkan：app 的 gpui 编译在 linux job 跳过（CI 注明，与 M0 预案一致）——实现方式：linux job 的 build 步骤对 app crate `--exclude` 或 features 组合处理，具体机制实现时定并注释。
2. **最小窗口**（docs/06 §3 设置窗口的最小子集）：单窗口两页——状态页（角色/连接状态/对端名/会话时长）+ 日志页（滚动文本，tracing 接入）；`QuitMode::Explicit`（关窗不退，托盘 M4 前用 Ctrl+C/命令退出）+ `on_reopen` 注册（spike 已验证 API）。
3. **`--loopback` 模式**：进程内跑 M1 全链路（config 加载 → Session/Dispatcher → net 回环 → mock 输入脚本驱动 → 状态页实时显示事件计数与日志页滚动）——零网络依赖的演示模式，也是 CI 可跑的集成冒烟。
4. **安全码握手实战**（FR-1.4）：`--host`/`--client` 模式经真实 TCP（M2-TCP 任务交付）在 localhost 握手——两个终端分别跑 `xcontrol-app --host` / `xcontrol-app --client 127.0.0.1:PORT`，正确码连上（状态页显示已连接），错误码握手失败（日志记录、连接关闭）。安全码来源：config/`--code` 参数（测试便利优先，keychain 接线在 M4 完善）。
5. **架构红线**（docs/06 §6）：UI 是 `AppState` 快照纯投影——app 内建最小 `AppState` 快照流（watch channel 200ms 节流）+ `CoreHandle` 命令入口；UI 不直连 net/core 细节。
6. **日志**：tracing + tracing-subscriber（fmt 层起步，滚动文件延后 M3 NFR-8）；子系统命名 `app::`、`session::`、`net::`。

## Acceptance Criteria

- [ ] `cargo run -p xcontrol-app -- --loopback` 跑通：窗口出现、状态页计数随 mock 脚本推进、日志页滚动、进程可干净退出
- [ ] localhost 双实例握手成功/失败路径演示通过（附运行日志摘录）
- [ ] gpui profile 告警消失（`cargo build --workspace` 无 "did not match any packages"）
- [ ] check-deps 绿（gpui/gpui-kit/tracing 系入白名单）；clippy/fmt/test 全绿；`--loopback` 模式可做 CI 集成冒烟（headless 或自动退出）
- [ ] UI 无直连网络/平台层（代码审查确认快照流单向）

## Notes

- gpui 窗口冒烟参考 spike（examples/spike-tray）手法：AX 元素检查 + 进程退出码；本任务交付真实 app 不是 spike。
- tray 不在本任务（M4 FR-6.1）；`--headless` 也不在（FR-6.4 M6）。
- gpui-kit 组件库（Form/Sidebar）M4 再用——本任务最小窗口用 gpui 基础 API 即可，别过度搭建。
- 本任务规模大（UI + 集成），是 M2 最重的任务；若 gpui API 与预期偏差大，优先保 --loopback 与握手实战，窗口 UI 可最小化。
