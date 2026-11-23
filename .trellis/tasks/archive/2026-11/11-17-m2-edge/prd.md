# M2: 边缘切换实时接线

## Goal

把 M1 就绪的 `edge_crossing` 纯函数 + M2 就绪的 macOS 光标查询/屏幕几何接进 xcontrol-app 会话运行时——FR-2.1/2.2 代码侧闭环。实机验收（光标真实滑边/无跳动）留人工。

## Requirements

1. 会话运行时（host 侧）持有 `ScreenLayout`（`input` 的 `display_layout()` 构建，窗口启动时获取一次 + 可选重取）。
2. 光标轮询任务（独立线程或 tokio 任务，50ms 量级可配）：`CGEventGetLocation` → 归一化 → `edge_crossing(layout, pos)` → 命中即发 `LocalCmd::EdgeCrossing`（去抖：同一边缘触发后需先回内侧才允许再触发）。
3. 收敛：对端 `EnterScreen` 进入受控时轮询暂停（本机光标不再驱动切换）；`LeaveScreen`/断开时恢复/停止按 ControlDir 语义。
4. 单测：合成布局 + 合成光标路径（滑入左缘/右缘/角落 ratio clamp/去抖窗口）全用例；无 macOS 依赖的纯逻辑测试（轮询与判定解耦）。
5. `--loopback` 模式下边缘切换用 mock 光标脚本演示（可选，不阻塞）。

## Acceptance Criteria

- [ ] 判定纯函数接线有单测（含去抖）
- [ ] host 模式真实光标轮询路径编译通过 + 冒烟（打印触边事件，不强制真实切换）
- [ ] check-deps/clippy/fmt/test 全绿
- [ ] ControlDir 暂停/恢复语义正确（被控时不轮询触发）

## Notes

- 实机验收（光标跨屏无跳动）属 M2 人工清单。
- 判定用本机屏幕布局；从机布局协商（CtrlMsg::Layout 下发）是 M3。
