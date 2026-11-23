# UI 与托盘规则

来源：docs/06-ui.md、ADR-0001。

## gpui 红线

1. **依赖只走 `gpui-kit` 一条线**：`gpui-kit = "0.9"`，gpui API 一律经其 re-export；禁止同时出现 gpui-pre/gpui-ce/git 直依赖（类型不共享，编译期冲突）。
2. 后台常驻 = `QuitMode::Explicit`（✔ M0 spike 已验证，ADR-0001 更新记录）；关闭全部窗口 ≠ 退出进程。
3. UI 是 `AppState` 快照的纯投影（watch channel，200ms 节流）；一切变更走 `CoreHandle` 命令；禁止 UI 层直连网络/平台层。
4. Wayland 不承诺托盘点击抢焦点（xdg-activation 由 compositor 决定）——best-effort：已开则展示，未开则新建。
5. Linux **禁止引入任何 GTK 主循环依赖**（与 gpui 自建循环冲突）。

## 托盘事件流（唯一合法路径）

```
gpui-tray 菜单 action（原生 MenuItem+action 分发）→ GPUI executor（天然主线程）→ CoreHandle 命令
```

- M0 spike 选定 gpui-tray 直连（与 gpui-kit 0.9.1 统一到 gpui-pre 0.3.12 单一版本源，证据 `examples/spike-tray/EVIDENCE.md` §B）；原"平台回调 → TrayCommand → channel → executor"桥接仅作 Ropy 备胎（gpui-tray 不可用时启用，ADR-0001）。
- 禁止在 action 处理器外触碰核心状态。
- 图标状态只由核心状态快照驱动（状态机见 docs/06 §5.1），UI/托盘不得各自拉取造成漂移。

## 组件与交互

- 表单用 gpui-kit Form/Input/Switch；简单开关类配置**即时生效并持久化**（TOML）；批量编辑类页面（热键/剪贴板范围/映射）走确认式提交，按钮统一叫「保存」（曾叫「应用」，后统一）——新页面按交互形态二选一，不要混用。破坏性操作（改安全码/清空日志）必须确认框。
- 权限缺失 → 状态页可点击的修复指引（跳系统设置/文档）。
- 文案 key 化 `tr("menu.pause")`；v1 至少 zh-CN 完整；zh/en 双表 parity 由测试钉死。
- 指标刷新 500ms 节流，避免高频重绘。

## 术语与文案约定（2025-11 实测反馈定稿）

- 用户可见面一律称**工作机**（曾用"从机"），en 用 client；**内部标识不改**：i18n key 名、config `role="client"`、日志/代码 slave 命名、tracing target 保持原样——只改显示层。
- 设置页按角色过滤：`Page::visible_for(role)`（Host→布局/热键；Client→映射）；当前页因切角色被隐藏时 render 期回落状态页。
- 语言敏感反查显示串（下拉选中文本→语义值）依赖不变量：该组选项文案 zh/en 全等、未命中回退值恰为"默认语义"；改文案必须换下标存取，否则两语言下反查行为分裂。

## i18n 测试竞态

- `tr()` 读进程全局语言态，i18n/UI 测试并行执行会互相翻语言导致断言漂移——i18n 测试模块内用 `LANG_LOCK`（Mutex）串行化一切改语言/读文案的测试，新语言敏感测试必须持锁。

## 构建注意

- MSRV 1.99（M0 spike 实测：gpui-pre 0.3.12 在 1.92/1.95 编译失败；rust-toolchain.toml 已锁定 1.99，勿降回）。
- gpui 系 crate 必须 `opt-level = 3`（根 Cargo.toml profile 已配置，勿删）。
- Linux 需要 Vulkan 驱动 + fontconfig/wayland/xkbcommon 系统库；Windows 需 MSVC + Windows SDK + CMake。
