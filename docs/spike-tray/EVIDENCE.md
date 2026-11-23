# 托盘 Spike 证据记录（M0 硬门禁，ADR-0001 决策 3）

- 日期：2026-11-16
- 机器：macOS（darwin 25.5.0，arm64，Retina 3456x2234）
- spike crate：曾位于 `examples/spike-tray/`（standalone，非主 workspace 成员）——11-22 实验使命完成后代码移出版本管理，证据（本文件与 evidence/）归档至 `docs/spike-tray/`
- 结论速览：**Spike A ✔（需 Rust ≥1.96..1.99 区间，1.92 锁定编译失败）· Spike B ✔（gpui-tray 直连路线可用）· Spike C 不需要**

## A0 · 关键前置发现：gpui-kit 0.9.x 在锁定的 1.92 工具链下编译失败

gpui-kit 0.9.1 宣称 MSRV 1.92（docs/01 §6、根 rust-toolchain.toml 依据），但其锁定的
gpui-pre 0.3.12 快照使用了 1.92 尚未稳定的 std API：

| 工具链 | gpui-kit 版本 | 结果 | 失败点 |
| --- | --- | --- | --- |
| 1.92.0 | 0.9.1（gpui-pre =0.3.12） | ✘ 编译失败 | `gpui-pre-util`: `slice_as_array`（rust-lang#133508，E0658） |
| 1.95.0 | 0.9.1（gpui-pre =0.3.12） | ✘ 编译失败 | `gpui-pre-macos`: `result_option_map_or_default`（rust-lang#138099，E0658，`Option::map_or_default`） |
| **1.99.1（stable）** | 0.9.1（gpui-pre =0.3.12） | **✔ 全树编译通过** | — |
| 1.92.0 | 0.9.0（gpui-pre =0.3.11，降级备选） | ✘ 编译失败 | 同 `slice_as_array`（gpui-pre-util 0.3.11 同样代码） |

命令与输出（节选）：

```
$ cargo check --bin spike-a                      # spike crate rust-toolchain=1.92 时
error[E0658]: use of unstable library feature `slice_as_array`
   --> ~/.cargo/registry/src/.../gpui-pre-util-0.3.12/src/lib.rs:575:18
error: could not compile `gpui-pre-util` (lib) due to 1 previous error    # EXIT=101

$ cargo +1.95 check --bin spike-a
error[E0658]: use of unstable library feature `result_option_map_or_default`
   --> ~/.cargo/registry/src/.../gpui-pre-macos-0.3.12/src/window.rs:957:14
error: could not compile `gpui-pre-macos` (lib) due to 3 previous errors  # EXIT=101

$ cargo +stable check --bin spike-a              # stable = 1.99.1
    Checking gpui-kit v0.9.1
    Finished `dev` profile ...                   # EXIT=0
```

1.92 + gpui-kit 0.9.0 复现：`evidence/toolchain-192-gpui-kit-0.9.0-fail.log`。

→ **工具链红线结论：走 gpui-kit 线必须把工具链升到 stable（≥1.99 实测可用；1.95 仍不够，
未对 1.96–1.98 逐版二分）。主 workspace 的 rust-toolchain.toml（1.92）与 docs/01 §6 的
"MSRV 1.92" 断言需要主会话决策修订（本任务无权改这两处，已回写 ADR-0001 跟踪项）。**

## A · Spike A：QuitMode::Explicit（可用）

实现：`src/bin/spike-a.rs`（timeline 全自动：t=0 开窗口 → 每秒心跳 → t≈3s 程序化关全部窗口
→ 继续心跳 → t≈11s 自杀退出）。

1. **API 编译验证 ✔**：`application().with_quit_mode(QuitMode::Explicit)`、`cx.on_reopen(...)`
   在 gpui-pre 0.3.12 中存在且编译通过（app.rs:291/526/369）。
   注意：`Application::new()` 在该快照中**不存在**，平台构造入口是
   `gpui_kit::application()`（re-export 自 gpui-fast-platform::application()，
   内部 `Application::with_platform(current_platform(false))`）；`on_reopen` 注册在
   **Application 构建器**上（App 上无此方法）——与 PRD 中 `Application::new()` 的写法有出入，
   语义等价。附带发现：快照还提供 `Application::headless()`（FR-6.4 直接可用）。
2. **关窗存活实测 ✔**（macOS 实测；日志 `evidence/spike-a-run.log`）：

```
[t=0.0s] spike-a 启动：application().with_quit_mode(QuitMode::Explicit)
[t=0.1s] on_reopen 注册成功（macOS，Application 构建器）
[spike-a] 设置窗口已打开
[t=4.0s] 程序化关闭全部 1 个窗口（window.remove_window()）
[t=5.0s] 心跳 #4: 进程存活, 窗口数=0
...
[t=12.4s] 心跳 #11: 进程存活, 窗口数=0
[t=12.4s] 结论：窗口关闭后进程存活 8s（心跳连续无中断），QuitMode::Explicit 生效 ✔
[spike-a] run() 已返回，进程正常退出            # EXIT=0，无残留进程（pgrep 复核为空）
```

3. **on_reopen 注册 ✔**（触发待人工：`cargo run --bin spike-a` 后点击 Dock 图标，日志应出
   `[t=…]s] on_reopen 触发`。Dock 点击无法自动化，macOS 人工项）。

Linux/Wayland 侧未实测（本机无 Linux 桌面），仅代码路径同构，gpui 官方 Wayland 后端在列。

## B · Spike B：gpui-tray 0.3.0 共存（可用，选定路线：gpui-tray 直连）

Cargo 特性：`gpui-tray = { default-features = false, features = ["gpui-kit", "menu-state"] }`。

1. **同版本源统一 ✔**（`evidence/gpui-pre-unification.txt`）：
   gpui-tray 的 `gpui-kit` feature 依赖 `gpui-pre ^0.3.4`（包名 gpui-kit-backend），
   gpui-kit 0.9.1 依赖 `gpui-pre =0.3.12`，cargo 统一到 **0.3.12 单一版本**，类型共享无冲突：

```
gpui-pre v0.3.12
├── gpui-base v0.9.1 ─── gpui-kit v0.9.1 ─── spike-tray
├── gpui-kit v0.9.1                        （同上）
├── gpui-pre-apple ─ gpui-pre-macos ─ gpui-pre-platform ─ gpui-kit v0.9.1
└── gpui-tray v0.3.0 ─── spike-tray        （同一 gpui-pre 0.3.12）
```

2. **编译 ✔**：`cargo check/build --features tray --bin spike-b` 通过（gpui-tray 首次编译 2.6s 增量）。
   代码需 `extern crate gpui_kit as gpui;` 别名（gpui-tray 宏展开要求 `::gpui::` 路径可解析）。
3. **托盘图标出现 ✔**（AX 证据，screencapture 因宿主缺 Screen Recording TCC 权限不可用，
   System Events 可达性查询为更强证据——元素真实存在）：

```
$ osascript -e 'tell application "System Events" to tell process "spike-b" to
    get {name, description, position, size} of every menu bar item of menu bar 1'
→ "XControl, status menu, 1008, 0, 109, 34"
```

4. **菜单结构渲染 ✔**（枚举结果与 docs/06 §5.2 菜单规范同构）：

```
→ "XControl · spike-b, <分隔线>, 暂停共享, 打开设置…, <分隔线>, 退出"
```

5. **菜单事件回传 GPUI executor ✔（自动化证实，非人工项）**
   （日志 `evidence/spike-b-run.log`，三次点击全部到达）：

```
[menu-event] OpenSettings 到达 GPUI executor
[spike-b] 设置窗口已打开（托盘与窗口共存 ✔）
[menu-event] TogglePause 到达 GPUI executor：paused=true（托盘菜单 → 动作分发 ✔）
[menu-event] QuitApp 到达 GPUI executor，退出
```

   事件链：System Events 点击 → NSMenuItem → gpui-tray → `cx.on_action` 分发 → handler
   运行在 GPUI 主线程。**点击菜单项是本机 osascript（System Events）真实点击**，非代码伪造。

6. **状态驱动菜单刷新 ✔**：TogglePause 后 `refresh_menu()`，菜单项变为"恢复共享"且带
   勾选标记（AXMenuItemMarkChar="✓"）——支持 docs/06 §5.2 的勾选态语义（menu-state feature）。
7. **退出干净 ✔**：托盘菜单"退出" → `tray.close()` + `cx.quit()` → EXIT=0，pgrep 复核无残留。
   90s 超时兜底逻辑未触发（78s 时人工路径已退出）。

## C · Spike C：Ropy 模式（未触发）

PRD 规定"仅当 B 失败才做 C"。B 全项通过，C 不执行；Cargo.toml 中 `ropys` 特性（tray-icon）
保留占位未启用。Linux ksni 路径仍为未验证项（本机无 Linux 桌面），由后续 Linux 实测任务覆盖。

## 环境备注

- spike-b 在 Cargo.toml 声明 `[[bin]] required-features = ["tray"]`：未启用 feature 时
  `cargo check --bins` 自动跳过该 target（gpui-tray 可选依赖不阻塞 Spike A 复跑）。
- gpui 系依赖全量 `opt-level = 3`（`[profile.dev.package."*"]`）：全树 codegen 构建 ~3m11s（M2 级 arm64）。
- 主 workspace 未改动：spike crate 独立 target 目录（`examples/spike-tray/target/`），
  根 Cargo.lock / members / CI 矩阵不受影响（复核命令与输出见回写后的任务报告）。
- 截图路径：`evidence/spike-b-menubar.png` 未生成（宿主无 Screen Recording 权限，
  "could not create image from display"）——以 System Events AX 查询替代，证据强度更高。
