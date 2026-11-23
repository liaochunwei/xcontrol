# M2: macOS 输入捕获与注入

## Goal

`xcontrol-input/macos`：CGEventTap 捕获 + CGEventPost 注入 + 权限检测——M2 让 XControl 第一次"真的能动别人鼠标"的任务。平台绑定层是全仓唯一预期出现 unsafe 的地方，每处 `// SAFETY:` 强制。

## Requirements

1. **权限检测**（docs/05 §1 显式要求）：`AXIsProcessTrustedWithOptions(kAXTrustedCheckOptionPrompt)` ——返回三态 API：`Granted` / `Denied`（可选弹引导框）；未授权时 CGEventTap 回调被系统静默忽略（不报错）——检测必须独立显式做，tap 创建成功 ≠ 有权限。
2. **捕获**（docs/05 §1）：`CGEventTap::new(kCGSessionEventTap)` 监听 keyDown/Up、flagsChanged、mouseMoved/Dragged、leftMouse 等事件；CFRunLoop 独立线程运行；**回调线程禁 async**——事件经 `std::sync::mpsc` 桥接（architecture-rules 红线 3）；回调返回事件（不吞，M2 不拦截主控本地输入——拦截留给热键 M4）。
3. **事件转换**：CGEvent → proto `InputEvent`——kVK_* 键码经**静态映射表 → XKey(HID)**（docs/03 §2，按物理键位非字符）；鼠标 button/delta/wheel 转换；修饰键从 flags 提取 `Mods` 位掩码。
4. **注入**（docs/05 §1）：`CGEvent` 构造 + `CGEventPost(kCGHIDEventTap)`——`InputSink` 实现：Key/Mouse/Wheel/Text 全支持；`MouseMoveRel` 用 **f64 累加器**处理小数余量（docs/02 §4.2：Windows 整数化必需，mac 先行实现同语义）；修饰键合成经 flags attached；Unicode 文本经 `CGEventKeyboardSetUnicodeString`。
5. **`InputSource`/`InputSink` trait 实现**：core 定义的两 trait（M1 已定）；`release_all()` 释放全部跟踪中的按下键（防卡键收尾）。
6. **布局感知**：光标位置查询 `CGEventGetLocation`（无需权限）+ 屏幕几何（`CGGetDisplaysWithRect` 或 `NSScreen` 等价 API）→ `ScreenLayout` 构建辅助（FR-1.3 基础）。
7. **feature 门控**：全部实现 `#[cfg(feature = "macos")]`（crate 内 `platform/macos.rs` 模块，architecture-rules 红线 2）；依赖 `core-graphics`/`core-foundation` 仅 macos feature。

## Acceptance Criteria

- [ ] kVK→HID 映射表全量测试（锚点键：字母/数字/修饰键/方向键/F 区抽测；未知 kVK 拒绝）
- [ ] 权限检测三态测试（本机当前状态实测——未授权时应返回 Denied，报告注明本机实际状态）
- [ ] CGEventTap 创建 + 事件流冒烟（无权限时 tap 创建成功但事件静默——如实记录行为；有权限则收到真实事件——人工授权后补验）
- [ ] 注入冒烟：`CGEventPost` 发一个无害事件（如 MouseMoved）不崩溃（权限允许时生效；无权限记录行为）
- [ ] clippy/fmt/test 全绿（macos feature 组合）；`cargo tree -p xcontrol-input --features macos` 依赖面干净
- [ ] 每个 unsafe 块有 `// SAFETY:` 注释；回调线程无 async

## Notes

- 本机（macOS arm64）可编译可冒烟；辅助功能授权弹窗需要**用户手动点**——权限 Denied 路径是本机默认现实，代码必须优雅处理并在报告注明"人工授权后可补验的项"。
- `core-graphics`/`core-foundation` crate 版本对齐 docs/01 §6 基线（core-graphics/event API 细节以实际 crate 文档为准，不得凭记忆断言——不确定时查 docs.rs 并在代码注释注明版本）。
- tokio 禁入 input crate（红线 3）。
