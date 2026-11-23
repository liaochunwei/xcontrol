# M5: 屏幕布局编辑器与控制转移闭环

## Goal

FR-1.3 布局编辑器 + FR-2.7 绝对坐标 + Next/Prev 信令闭环（Ack→EnterScreen 选点）+ CtrlMsg::Layout 协商——M4 遗留的"控制转移完成"收口。

## Requirements

1. **Layout 协商**：主控 `CtrlMsg::Layout{screens}` 下发（M1 proto 已有）→ 从机 ScreenLayout 更新；ScreenId 跨机语义定稿（主控枚举 → 从机映射表——M3 遗留 ScreenId=本机下标 问题收口）。
2. **Next/Prev 闭环**：SwitchRequest Ack 后 → EnterScreen 选点（target 屏幕边缘进入点 ratio 计算——进入侧 edge 语义：从主控切出方向推断）——M4 信令移交项收口。
3. **布局编辑器 UI**（FR-1.3）：设置内嵌页网格画布——本机屏幕自动检测 + 从机屏拖拽排列（简化：网格拖放 + 位置保存到 config `[layout]`）；编辑结果 → 主控下发 Layout。
4. **绝对坐标**（FR-2.7）：MouseMoveAbs 的注入路径（mac CGEvent 空间换算 /win 虚拟屏 65535/x11 warp——M2/M3 注入器补 abs 路径）+ core `to_abs` 已有换算接线。
5. **测试**：Layout 下发→从机布局更新→EdgeCrossing 到 target 的端到端（mock）；编辑器 UI 测试（harness）。

## Acceptance Criteria

- [ ] Layout 协商端到端测试（下发→更新→切换路由命中 target）
- [ ] Next/Prev 完成闭环（Ack→EnterScreen→被控）测试
- [ ] 布局编辑器 AX/harness 实测（拖放简化版 + 保存）
- [ ] abs 注入三平台编译绿（mac 实测）
- [ ] check-deps/clippy/fmt/test 全绿；ScreenId 语义文档化（docs/02 修订如有）

## Notes

- 编辑器简化裁定权：网格拖放最小可用即可（像素级对齐/缩放提示 M6）。
- ScreenId 语义修订属协议文档变更（docs/02 先行）。
