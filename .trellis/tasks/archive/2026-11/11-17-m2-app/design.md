# design: 最小 gpui 窗口与 --loopback

## 模块结构（xcontrol-app）

```
src/
├── main.rs         # CLI 解析（--loopback/--host/--client/--code/--config）+ 模式分发
├── state.rs        # AppState 快照 + watch channel（200ms 节流）+ CoreHandle 命令枚举
├── loopback.rs     # --loopback：进程内 mock 输入脚本 → Session → Dispatcher → RecordingSink，状态上报
├── session_rt.rs   # TCP 会话运行时：serve/connect → 握手 → Session<Clock> 驱动循环（tokio select 心跳/收帧）
├── ui/
│   ├── mod.rs      # gpui Application 启动、QuitMode::Explicit、on_reopen
│   ├── status.rs   # 状态页（纯投影：订阅快照流）
│   └── log.rs      # 日志页（tracing Layer 桥接到 UI 环形缓冲）
└── logging.rs      # tracing-subscriber fmt + 自定义 UI Layer
```

## 关键决策

- **线程模型**：tokio runtime（核心/网络）+ gpui 主线程（UI）——经 watch channel 单向投影 + CoreHandle mpsc 命令，与 docs/06 §6 一致；session_rt 在 tokio 侧驱动 `Session<Clock>`（SystemClock），把 `SessionAction::Send` 写 TCP、把 Notice/状态变更推快照。
- **快照结构**：`AppState { role, conn: ConnState, peer_name, session_id_hex, events_in/out 计数, last_error }`——200ms watch 合并。
- **gpui 依赖**：`gpui-kit = { version = "0.9", default-features = false, features = [...] }`——以 spike（examples/spike-tray/Cargo.toml）已验证的组合为起点；app 需要 `application()`/window/watch 等基础能力。
- **--loopback 与握手共用 state.rs**：两种模式只是 CoreHandle 后端不同（LoopbackBackend vs TcpBackend）——UI 完全不感知。
- CLI 用 `pico-args` 或手写（避免 clap 重依赖）——选型实现时定，倾向手写（参数就 5 个）。

## CI / linux 处理

- gpui 仅 `#[cfg(any(target_os = "macos", target_os = "windows"))]`？——否，gpui 支持 linux（Wayland 后端 M5）；linux CI 问题是 Vulkan 缺失导致**编译**可能都失败。方案：linux job `cargo build --workspace --exclude xcontrol-app`（注释注明，gpui 系统库装机后移除 exclude）；clippy/test 同样 exclude 或允许 default feature 差异。实现时验证 linux 编译是否真的失败（若只是运行失败则不用 exclude）。

## 测试策略

- `--loopback` 冒烟：集成测试 spawn `xcontrol-app --loopback --auto-quit-ms 3000`（自动退出参数，测试用）→ 断言 exit 0 + 输出含事件计数；窗口创建在 CI 无显示环境会失败——`--auto-quit` 模式配合"无显示则跳过 UI"或 `headless-ish` 分支（gpui headless API spike 验证过存在 `Application::headless()`）。
- localhost 握手：两 tokio 任务跑 session_rt（不经 UI），断言状态迁移与 FR-1.4 拒绝路径。
