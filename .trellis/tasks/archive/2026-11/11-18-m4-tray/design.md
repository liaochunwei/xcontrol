# design: 托盘与开机自启

```
crates/xcontrol-tray/src/
├── lib.rs          # TrayAdapter：AppState 快照 → 图标/菜单投影（纯逻辑：状态→五态+tooltip 文本，可测）
├── icons.rs        # SVG 源内嵌 + 状态→渲染（简化：单图标 + 角标文字合成或预渲染 PNG 集合——实现裁定）
└── autostart/{macos,windows,linux}.rs   # 三平台自启（feature 门控）
app 侧              # gpui-tray TrayIcon 生命周期 + menu action → CoreHandle
```

- **状态→图标映射**纯函数（TrayAdapter::project(snapshot) -> TrayState{icon_kind, badge, tooltip, menu_checkstates}）——单测全矩阵（主控 0/1/N 台、暂停、错误 × 从机态）。
- **图标资源**：M0 spike 用过 gpui-tray 的 icon API——本任务简化起步：单色模板图标 1-2 张 + 状态用 tooltip/角标表达（预渲染多态图标 M6 打磨）；裁定写明。
- **gpui-tray API 锚**：examples/spike-tray/src/bin/spike-b.rs（menu-state feature 的 MenuState/MenuItem 用法）——菜单构建/事件回传照搬 spike 模式。
- **自启**：三平台各自 trait Autostart{enable/disable/is_enabled}；mac plist：Label com.xcontrol.agent + ProgramArguments[exe_path]；win Run 键；linux .desktop（Exec=exe_path）。路径解析：std::env::current_exe。持久化裁定：不进 config（运行时状态），托盘勾选读 is_enabled。
- **角色切换语义**：停当前会话（disconnect_actions）→ 角色翻转 → 重启运行时（serve or discover）——app 层函数 `switch_role()`（复用 main.rs 的模式分发），实测验证。
- **依赖**：xcontrol-tray 依赖 gpui-tray（+ gpui-kit 传递）——tray crate 首次有重依赖；xcontrol-tray 进 GATED_CRATES 白名单（gpui-tray/gpui-kit/thiserror…）；app 依赖 xcontrol-tray。**linux CI**：gpui 系在 linux 编译问题同 app——tray 依赖 gpui 后 tray crate 也要 exclude（CI 检查现有 exclude 覆盖）。
