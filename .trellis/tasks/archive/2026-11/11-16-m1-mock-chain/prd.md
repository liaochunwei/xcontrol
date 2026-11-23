# M1: mock 平台层与全链路回环验证

## Goal

`xcontrol-core` 的 `mock-platform` feature（虚拟输入源/虚拟剪贴板）+ 端到端场景测试，达成 M1 出口条件："回环集成测试覆盖 FR-2.x 全部事件类型；NFR-1 回环分量 P50 < 5ms"。

## Requirements

1. **虚拟输入源**（core `mock-platform` feature 下）：可脚本化的 `InputSource`——按预设序列产生按键/鼠标/滚轮/文本事件（含修饰键序列、拖拽合成序列）；可注入边界场景（超长 Text 分段、未知 kind 帧）。
2. **虚拟剪贴板**：内存实现 `ClipboardWatch` 语义——变更 → ClipAnnounce/Chunk/End 全流程可脚本化；含超限（>16MiB 协商上限）与超时（10s 未完成）场景。
3. **端到端场景测试**（`tests/` 集成测试，经 net 回环 + Noise 全加密路径）：
   - 键盘因果序列严格保序（`Meta↓ C↓ Meta↑ C↑` 类）
   - 拖拽合成：`MouseLeft↓ + MoveRel* + ↑` 全链到达
   - 修饰键对齐：EnterScreen 快照 → 对端 InputSink 收到对齐动作；LeaveScreen 全释放；1s ModifierSync 差异修正
   - 剪贴板：announce→chunk 信用窗口（4 块）→end→blake3 校验；超限断开；seq 乱序断开；未 announce 先 chunk 断开
   - 心跳判死：9s 无帧（虚拟时钟）→ Disconnected
   - 版本协商：1.0↔1.1 降级共存、1.0↔2.0 拒绝
4. **M1 出口验证**：FR-2.x 事件类型覆盖清单（Key/KeyRepeat/MoveRel/MoveAbs/Button/Wheel/Text/EnterScreen/LeaveScreen/ModifierSync/SwitchRequest/SwitchAck）逐一标记测试用例；P50 < 5ms 基准数据回填。
5. **基准报告**：`docs/` 或任务 notes 记录编解码吞吐 + 回环延迟数据（M6 性能验证的 M1 基线）。

## Acceptance Criteria

- [ ] FR-2.x 全部 12 种事件类型均有端到端用例（覆盖清单在测试文件头部注释）
- [ ] 剪贴板三防护场景（超限/乱序/越序）全部按协议断开
- [ ] 心跳判死 + 版本协商场景全过
- [ ] `cargo test --workspace` 全绿；mock-platform feature 组合 `cargo test -p xcontrol-core --features mock-platform` 绿
- [ ] P50 < 5ms 实测达标并记录
- [ ] check-deps / clippy / fmt 全绿；覆盖 ≥85%（core+net 合并口径）

## Notes

- 本任务不改 proto/core/net 的既有 API（只消费）；发现接口缺口先报主会话评估，不擅自改签名。
- 信用窗口/重组状态机若 T3 net 未实现（属会话层），在本任务落到 core route/session 层，但不得违反 core 零 I/O。
- 虚拟时钟复用 core `clock.rs` 的 TestClock。
