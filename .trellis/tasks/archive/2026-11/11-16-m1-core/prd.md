# M1: xcontrol-core 会话状态机与事件路由

## Goal

`xcontrol-core` 的会话/布局/路由纯逻辑层——协议帧进来后"怎么处理"的全部决策逻辑。零 I/O、零平台 API、零 tokio（红线 crate），全部可在 `cargo test` 覆盖。

## Requirements

1. **会话状态机**（docs/02 §4.5/§5）：状态 `Idle → Handshaking → Running ⇄ Paused → Disconnected`；事件驱动（收帧 / 定时器到期 / 本地命令）；转移非法时拒绝并返回错误（如 Paused 中收输入帧）。心跳语义：Ping 3s 周期、9s 无帧判死（可配，经 `Options`）；时钟注入（`Clock` trait），测试用虚拟时钟。
2. **版本协商集成**：用 proto 的 `negotiate()`；`Hello`/`HelloBack` 处理产生会话 minor；major 不等 → `Error{UnsupportedVersion}` 后断开。
3. **屏幕布局模型**（FR-1.3/FR-2.7 纯函数部分）：`ScreenInfo`（名称/几何/缩放提示）+ 布局网格；给定光标位置与布局 → 边缘穿越判定 `EnterScreen{target, edge, ratio}`（docs/02 §4.2 0x10）的纯函数；绝对坐标归一化换算。
4. **事件路由**：协议帧 → 本地动作的 dispatch 层；输出抽象 `InputSink` trait（docs/08 §2 命名：`InputSource` 捕获/`InputSink` 注入）——core 定义 trait，实现由平台 crate/mock 提供；未知 kind 跳过语义保持。
5. **修饰键对齐防卡键**（docs/03 §3 不变量）：`EnterScreen.mods` 快照对齐、`LeaveScreen` 释放全部、1s `ModifierSync` 周期校验——状态跟踪 + 差异计算纯逻辑。
6. **会话标识**：`session_id = BLAKE3(hs_hash)[0..16]` 的计算与生命周期管理（docs/02 §3.3）。

## Acceptance Criteria

- [ ] 状态机全部合法转移有单测；非法转移被拒绝并报错
- [ ] 心跳判死用虚拟时钟测试（3s/9s 语义）
- [ ] 版本协商：major 不等 → Error+断开路径；minor=min 生效
- [ ] 边缘检测纯函数：四边×比例边界用例覆盖；绝对坐标换算正确
- [ ] 修饰键：切入对齐/切出释放/周期同步三场景单测
- [ ] 行覆盖 ≥85%；零 I/O 零 tokio（依赖树证明）；check-deps 全绿
- [ ] `cargo build/test/clippy/fmt` 全绿

## Notes

- 红线：core → 仅 proto + config；本任务**不加任何新依赖**（blake3 由 net 任务引入？否——session_id 计算若需 blake3，评估：blake3 属纯逻辑库无 I/O，可入 core 白名单，但优先把 hash 计算放 net/core 之一并说明理由）。
- 与 net 边界：net 管传输+加密+握手报文；core 管握手成功后的会话语义。trait `InputSink`/`InputSource` 定义放 core（docs/08 §2），input crate 后续 re-export。
- `mock-platform` feature 在 T4 任务实现，本任务只留 feature 声明与 trait。
