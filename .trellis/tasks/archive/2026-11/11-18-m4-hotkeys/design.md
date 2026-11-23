# design: 全局热键与切换路由

```
core/src/hotkey.rs        # HotkeyId/HotkeySet 匹配纯逻辑（原始 XKey + Mods 精确）
input 平台模块            # win: RegisterHotKey（复用 clipboard 消息窗口模式）/ x11: grab_key
app/src/hotkey_rt.rs      # 两端路由：HotkeyId → LocalCmd/SwitchRequest/暂停 toggle
```

- **匹配纯逻辑**：`HotkeySet::match_raw(mods: Mods, key: XKey) -> Option<HotkeyId>`——Ctrl=LCtrl|RCtrl 任一（修饰键匹配语义：docs 用 Ctrl+Alt 泛指；实现按"任一侧修饰键命中即匹配"裁定并文档化）；未列出的额外修饰键按下 → 不命中（精确）。
- **mac**：捕获事件流过滤（M2 CaptureSource 输出 → app 捕获接线层 → HotkeySet.match_raw → 命中产 HotkeyId 不产出转发事件；未命中原样转发）。**mac 不吞**语义：ListenOnly tap 无法拦截——本地热键由识别后"不转发"实现（从机侧收到的是主控没转发的流），文档化与 win 吞键差异。
- **win**：RegisterHotKey 需要消息窗口线程——clipboard 的 listener 窗口模式复用（新窗口或同一窗口扩展 WM_HOTKEY 处理）；从机侧注入前过滤：hook 捕获流识别热键 → 可吞（不调 CallNextHookEx）→ 不进注入管线（本地动作）。
- **x11**：grab_key 全局被动抓取（x11rb）+ KeyPress 解码 → HotkeyId；Xwayland 下 grab 语义实测（M3 先例：XTEST 注入经 KWin 路由不可见——grab 全局键可能同样受 compositor 限制，实测记录移交 M5）。
- **路由**：Next/Prev → `SwitchRequest{target}`（target 计算：主控在线从机列表循环——app 会话层维护候选序）；Home → 主控侧 LocalCmd（释放受控）；Pause → toggle。从机收到 SwitchRequest → release_all + Ack（M1 已有）。
- config `[hotkeys]` 四键（next/prev/home/pause）——v3 内已有 next；扩三键是否 bump v4 按约定（新键 + 默认值——serde default 不破坏 v3 读取，**字段追加可不 bump**（旧程序解析忽略？不——Config 无 deny_unknown_fields，旧程序读新文件会忽略新字段继续运行，安全）→ 裁定 v3 内追加字段 + default，报告说明）。
