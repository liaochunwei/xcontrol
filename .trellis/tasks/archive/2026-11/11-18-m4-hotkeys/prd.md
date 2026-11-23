# M4: 全局热键与切换路由

## Goal

三平台全局热键（FR-2.5）——热键在两端注册为本地全局热键，命中本地处理**不转发**（主控捕获层过滤 / 从机注入前过滤）。四默认键可配（docs/03 §4.6）。

## Requirements

1. **热键模型**：`HotkeySet`（next=Ctrl+Alt+Right / prev=Ctrl+Alt+Left / home=Ctrl+Alt+Home / pause=Ctrl+Alt+P 默认）从 config `[hotkeys]` 加载；匹配用**原始 XKey**（映射之前，§4.6）；`HotkeyId` 枚举（Next/Prev/Home/Pause）。
2. **mac**（docs/05 §1"或直接在事件流识别"）：捕获层（CaptureSource 的事件流）加热键识别——原始 Key 事件 + Mods 快照匹配 HotkeySet → 命中即产出 HotkeyId 且**不转发**该事件（mac 捕获是 ListenOnly 不吞，"不转发"指 app 层不产生 InputEvent 输出——按此语义实现并文档化）。Carbon RegisterEventHotKey 备选不采用（捕获层识别已覆盖，少一条 Carbon 依赖）。
3. **win**（docs/05 §2 热键行）：`RegisterHotKey`（消息窗口线程——clipboard 的 message-only 窗口先例）+ `WM_HOTKEY` → HotkeyId；从机侧"注入前过滤"：hook 捕获流中识别热键组合 → 拦截不注入（win 捕获可吞：不调 CallNextHookEx——FR-2.5 热键不被转发语义在从机侧是"不注入到本机 OS"）。**读 docs/05 §2 热键行与 §4.6 语义后裁定各侧实现**，报告说明。
4. **x11**（docs/05 §3 热键行）：`grab_key`（被动抓取，x11rb）→ KeyPress 事件 → HotkeyId；ungrab 生命周期管理。
5. **切换路由**：HotkeyId → 动作——Next/Prev → 会话层 SwitchRequest（对端切换）；Home → 回主控（主控侧自切：释放当前受控从机）；Pause → 暂停/恢复 toggle。路由接线 app 两端运行时。
6. **从机侧热键处理**：从机收到 SwitchRequest（M1 已有 Ack 语义）→ 释放本机按键 + 回 Ack。
7. 测试：HotkeySet 匹配纯函数全矩阵（四键 × 命中/不命中/修饰键多余拒绝）；config 加载；路由逻辑单测。平台层编译级验证（windows target/x11——x11 可 SSH 远端 grab_key 实测）。

## Acceptance Criteria

- [ ] HotkeySet 匹配矩阵测试（原始 XKey + 精确 mods）
- [ ] 三平台捕获/注册路径编译绿；x11 grab_key SSH 远端实测（注册→合成热键→收到）
- [ ] 路由单测（四动作语义）；不转发语义文档化（两端）
- [ ] check-deps/clippy/fmt/test 全绿；CI 平台步骤（win/x11 feature 化）
- [ ] docs/03 §4.6 / docs/05 热键行若有缺口先行修订

## Notes

- 热键配置 v3 已有 [hotkeys]（docs/01 §5 示例）——读 model.rs 确认字段（M2 时只有 next）——需扩成四键（v3 内扩展还是 v4？按迁移约定裁定）。
- mac 捕获层识别依赖 M2 CaptureSource 事件流——接线点在 app（capture → hotkey filter → 其余转发）。
