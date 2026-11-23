# M4: 按键映射引擎

## Goal

`xcontrol-core` 映射引擎（docs/03 §4 全量）——从机侧"收到主控事件 → 注入本机"之间的规则改写层。纯逻辑零 I/O，FR-3.1–3.4/3.6 的核心。接线后从机注入流水线成为 `接收 → 热键过滤 → 规则改写 → InputSink`。

## Requirements

1. **规则模型**（docs/03 §4.1）：`MappingRules` 从 config `[[keymap.rules]]` 加载；first-match-wins；上限 256（config 已校验）；规则四类（单键/修饰键事件级/组合级/drop）；trigger.mods 精确集合匹配；trigger 用主控原始（映射前）XKey。
2. **事件级修饰键映射**（§4.2）：`pressed_origin → mapped` 对照表状态机——任意释放顺序正确防卡键（如 LMeta↓ LCtrl↓（另一条规则）LMeta↑ 的错序释放）。
3. **组合级拦截状态机**（§4.3）：触发器修饰键按下 → pending 不注入；非修饰键到达查组合规则命中 → 注入 `action.mods 全↓ → key ↓/↑ → mods 全↑`；未命中 → flush pending（退回 4.2 语义）+ 当前键；修饰键全释放 → flush 释放；pending 缓存上限 16 条超限 flush；无超时依赖。
4. **平台预设**（§4.4）：8 内置（mac2win/mac2linux/win2mac/linux2mac/win2linux/linux2win/mac2mac/none）；自定义规则叠加在预设之后（预设先命中则不查自定义）；`preset = "none"` 纯自定义。
5. **直通模式**（FR-3.6）：`keymap.passthrough` 开关（config v2→v3 或并入 [keymap] 节——按 config 迁移约定裁定）绕过全部规则。
6. **热键前置**（§4.6）：热键匹配发生在映射之前（引擎入口提供 `is_hotkey(raw_event)` 钩子或分层由调用方先行——设计裁定），保证任意预设下热键一致。
7. **接线**：app client 会话运行时——Dispatcher 的注入动作经引擎改写后进 InputSink；`keymap.preset` 与规则从 config 加载；直通/改写切换即时生效（config 重载钩子，热加载延后则重启生效并注明）。
8. **修饰键快照协同**：EnterScreen 快照对齐动作（M1）在引擎管线中的语义——对齐产生的合成 Key 事件**是否过映射**？裁定并文档化（建议：对齐是"对端语义平移"，不过映射，否则双重映射）。

## Acceptance Criteria

- [ ] FR-3.1 AC 序列测试：`LMeta↓ C↓ C↑ LMeta↑` 经 mac2win → 注入 `LCtrl↓ C↓ C↑ LCtrl↑`
- [ ] 错序释放防卡键：混用两条修饰键规则 + 交错按下/释放 → 注入序列正确
- [ ] 组合级：`Cmd+C → Ctrl+Shift+C` 改写；`Cmd+X`（无规则）→ flush 退回 `LCtrl+X`；pending 16 条上限 flush
- [ ] 8 预设全量测试（规则表逐条 + 自定义叠加/覆盖语义）
- [ ] drop 动作：命中即吞（不产出注入）
- [ ] 直通模式：全规则绕过
- [ ] 行覆盖 ≥85%（core 合并口径）；check-deps/clippy/fmt 全绿；core 零新依赖（如需新依赖先报告）
- [ ] docs/03 §4 若实现中发现缺口/歧义先行修订（修订记录）

## Notes

- 映射引擎放 core（纯逻辑）还是独立模块？——放 core `mapping.rs`（它消费 config 的规则 + proto 的事件，都是 core 红线内依赖）。
- config 若加 passthrough 字段：v2→v3 迁移按 m3-status 先例。
- docs/03 §4.5 Text 事件不过映射（字符路径，映射是键码路径）——在引擎入口分流并文档化。
