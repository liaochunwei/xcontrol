# ADR-0003：各平台输入捕获/注入/剪贴板实现路径

- 状态：已接受
- 日期：2026-11-15
- 关联需求：FR-2.x、FR-4.x、C-2、C-3、NFR-4、NFR-6

## 背景

跨 4 个图形栈（macOS / Windows / X11 / Wayland）× 5 类能力（捕获、注入、光标、剪贴板、热键）选 API 与 crate。Wayland 因安全模型（无全局钩子、无全局坐标）必须按 compositor 分路径。完整调研矩阵见 [05-platform.md](../05-platform.md)（本 ADR 记录"为什么这么选"）。

关键事实（2026-11 调研，来源见文末）：

- rdev 维护弱（Linux 监听仅 X11、滚轮无 delta、已知映射缺陷）；enigo 活跃但后端**编译期**选择，Wayland 实验性。
- lan-mouse 的架构（捕获与注入后端分离、按环境自动选择）是同类最佳参照；其 wlroots 捕获用 layer-shell 1px 边缘窗口，GNOME/KDE 用 libei/portal。
- InputCapture portal（GNOME≥46 / KDE≥6.1，wlroots ❌）+ reis（纯 Rust libei）是 Wayland 主控的现代正道；GlobalShortcuts portal 在 sway 等 wlroots 环境不可用。
- Deskflow v1.27 也已转向 portal+libei 路线支持 Wayland（佐证方向正确）。
- X11 注入必须 XTEST（XSendEvent 被大量客户端按 ICCCM 忽略）；监听 XRecord + XFixes。
- rustdesk 用 arboard fork + clipboard-master + owner 标记防回环，是剪贴板工程先例。

## 备选方案（跨平台层面的关键取舍）

| 备选 | 放弃原因 |
| --- | --- |
| 全平台用 rdev（捕获+注入一站） | 维护弱；Linux 监听 X11-only；滚轮/布局缺陷；不可控 |
| 全平台用 enigo 做注入 | 成熟平台可用（保留为这些平台的**可选实现**），但 Wayland 后端实验性且编译期选择，无法按 compositor 运行时切换 |
| Wayland 注入只走 uinput | 需要 input 组/root，分发不友好且绕过 compositor 安全模型；仅作 opt-in 兜底（headless 从机场景反而合适） |
| Wayland 捕获只走 portal | wlroots（sway/Hyprland）无 InputCapture 后端 → 该人群不可用；layer-shell 补位 |
| 剪贴板全走 arboard | 无监听 API、无 HTML、GNOME data-control 支持存疑 → arboard 保留做基础读写，监听与扩展格式自建/混搭 |

## 决策

1. **自写平台捕获层**（唯一负责事件语义化与热键拦截的位置）：mac `core-graphics` CGEventTap / win `windows` LL-Hook / x11 `x11rb`(record+grab) / wayland `ashpd::input_capture`+`reis` 或 wlr-layer-shell（按 compositor 探测）。统一实现 `xcontrol-input` 的 `InputSource` trait；平台模块 feature 门控。
2. **注入层**：统一 `InputSink` trait。mac/win/x11 直接绑定原生 API（CGEventPost / SendInput / XTEST）——量可控且可精确实现 f64 累加、Unicode 直发；wayland 运行时探测：RemoteDesktop portal+reis → wlr-virtual-pointer+virtual-keyboard → uinput（显式 opt-in）。enigo 不引入主路径（避免编译期后端限制），但其实现作参考。
3. **剪贴板**：读写基础层 arboard（mac/win/x11）；HTML/扩展格式与监听自建（win32 `WM_CLIPBOARDUPDATE`、mac changeCount 轮询 250ms、X11 XFixes、Wayland `wl-clipboard-rs` Watcher + GNOME clipboard portal 降级）；防回环按 [04-clipboard.md](../04-clipboard.md) §5 三重防线。
4. **热键**：优先 OS API（RegisterEventHotKey/RegisterHotKey/XGrabKey/GlobalShortcuts portal），统一回退"事件流内识别 + 边缘双击 + 死角"（Synergy 语义）——因为主控天然持有全量输入流，零依赖回退几乎免费。
5. **能力协商**：所有平台差异收敛为 Caps 位掩码，连接时交换，UI 呈现降级（如 sway 从机提示"无 Unicode 直发，uinput 后端受限"）。
6. **Keymap 转换表**由 `xtask` 从各平台头文件生成（`docs`→代码，禁止手抄漂移）。

## 后果

- ✅ 每条路径都有活跃先例验证（lan-mouse/Deskflow/rustdesk）；Wayland 三路径覆盖全部主流 compositor；平台层完全可 mock（NFR-6）。
- ⚠️ 平台层是工作量主体（约 4×5 块 API 胶水），靠矩阵文档（05）+ feature 门控 + 手动清单控复杂度。
- ⚠️ portal 路径引入用户可见的授权对话框（restore token 缓解）；uinput 兜底需要文档引导改组。
- 跟踪项：M5 首周实测 sway 1.11 virtual-pointer 矛盾信息、GNOME data-control 可用性；结论回写 05 附录。

## 来源（节选）

- https://github.com/feschber/lan-mouse （Wayland 后端矩阵与架构）
- https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html
- https://docs.rs/ashpd/latest/ashpd/desktop/input_capture/ 、https://crates.io/crates/reis
- https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1
- https://www.x.org/releases/X11R7.7/doc/man/man3/XTestFakeKeyEvent.3.xhtml 、XSendEvent ICCCM 忽略问题
- https://github.com/Narsil/rdev 、https://github.com/enigo-rs/enigo （维护状态与限制）
- https://github.com/1Password/arboard 、https://github.com/YaLTeR/wl-clipboard-rs 、https://docs.rs/clipboard-master
- https://github.com/rustdesk/rustdesk/blob/master/src/clipboard.rs （防回环/轮询先例）
- https://github.com/deskflow/deskflow （v1.27 Wayland portal+libei 路线）
