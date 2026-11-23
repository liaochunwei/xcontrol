# M4: 托盘与开机自启

## Goal

`xcontrol-tray` + app 集成（FR-6.1）——M0 spike 已验证 gpui-tray 直连路径，本任务交付真实托盘：图标状态机 + 固定顺序菜单 + 三平台开机自启。mac 本机全链路实测。

## Requirements

1. **图标状态机**（docs/06 §5.1）：五态（主控在线 N≥1/搜索中/暂停/错误 + 从机已连接/搜索中/暂停/错误）——图标源内嵌 SVG→渲染；状态只由 AppState 快照驱动（纯投影红线）；状态变化 ≤1s 反映（FR-6.1 AC）。
2. **菜单**（docs/06 §5.2 固定顺序）：状态头（不可点）→ 暂停/恢复共享（勾选态随状态刷新）→ 切换角色 → 分隔 → 在线从机子菜单（主控）/断开并重扫（从机）→ 分隔 → 打开设置 → 开机自启 ☑ → 分隔 → 退出。菜单 action 走 gpui-tray 原生 MenuItem+action 分发（M0 spike 定稿语义），action 处理器经 CoreHandle。
3. **gpui-tray 集成**：`xcontrol-tray` crate 实现（M0 空壳）——icon 渲染（状态→图像）+ gpui-tray TrayIcon 生命周期（app 启动创建/退出销毁）；依赖 gpui-tray 0.3（M0 spike 验证的 feature 组合：gpui-kit/menu-state）——入主 workspace（app 依赖 tray）。
4. **菜单动作接线**：暂停/恢复 → LocalCmd::Pause/Resume；切换角色 → 角色翻转（重启会话语义——app 层裁定：停当前会话按新角色重启，报告设计）；打开设置 → 窗口唤起（M2 窗口）；退出 → Quit。**在线从机子菜单点击直接切换**（FR-2.5 路由复用——切换到指定从机）。
5. **开机自启**（三平台）：mac launchd agent（~/Library/LaunchAgents plist 生成/删除）；win 注册表 Run 键（HKCU\Software\Microsoft\Windows\CurrentVersion\Run）；linux XDG autostart（~/.config/autostart .desktop）；命令行路径处理（当前二进制路径解析）；托盘菜单勾选态与实际状态同步；`autostart` 相关 config 字段（或不持久化——运行时状态，裁定）。
6. **tooltip**：`XControl · 主控 · 2 台在线 · 延迟 8ms` 格式（§5.1）。

## Acceptance Criteria

- [ ] mac 本机全链路实测：托盘图标出现 → 菜单项点击（暂停/恢复/切换角色/退出）经 osascript AX 自动化实测（M0 spike 手法）→ 状态图标随 AppState 变化（≤1s 断言）
- [ ] 菜单顺序与 docs/06 §5.2 逐项一致
- [ ] 自启三平台：mac 本机实测（plist 写入/删除 + launchctl 验证）；win/x11 编译级 + 脚本逻辑测试
- [ ] FR-6.1 AC 对照（状态 ≤1s/菜单项/tooltip）
- [ ] check-deps/clippy/fmt/test 全绿；CI macos 步骤（tray feature 若门控）
- [ ] docs/06 §5 若实现缺口先行修订

## Notes

- gpui-tray 是 M0 spike 验证过的直连路径（同版本源 gpui-pre 0.3.12）——examples/spike-tray 源码是 API 锚。
- xcontrol-tray 语义收敛为"核心状态快照 → 图标/菜单投影的适配层"（M0 spike 后的 docs/06 §1 定稿）——不含平台胶水。
- 自启路径处理：cargo run 与安装后路径不同——mac plist 用当前 executable 路径；文档注明。
