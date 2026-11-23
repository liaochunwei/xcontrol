# M3: Windows 输入层

## Goal

`xcontrol-input/windows`：LL hook 捕获 + SendInput 注入 + scancode→XKey 映射表——mac 后端的 Windows 对应物。本机无 Windows：**编译级验证**（`cargo check --target x86_64-pc-windows-msvc`）+ 跨平台纯逻辑测试；运行时验证移交有 Windows 机器时。

## Requirements

1. **捕获**（docs/05 §2）：`SetWindowsHookExW(WH_KEYBOARD_LL/WH_MOUSE_LL)` + 专用线程消息循环（GetMessage）；钩子回调内**零慢操作**（LowLevelHooksTimeout）——转换 + mpsc 投递即返回 `CallNextHookEx`（不吞事件，M2 语义同 mac）；UIPI/钩子超时注意事项进模块文档。
2. **注入**：`SendInput`（KEYBDINPUT：scan code + KEYEVENTF_UNICODE 文本路径；MOUSEINPUT：绝对/相对/滚轮/按钮）；`InputSink` 实现 + f64 累加器（docs/02 §4.2 Windows 整数化必需）+ `release_all`。
3. **映射表**：scancode set 1 → HID usage 静态表（docs/03 §2：按物理键位）——**表准确性核验方法**写明（公开 scancode 表来源），锚点测试；`KEYEVENTF_UNICODE` 路径不经映射表。
4. **build.rs 平台 feature 自动选择**（docs/08 声明的缺口，M2 check 遗留）：input/clipboard/tray 三个 crate 加 build.rs——按 `CARGO_CFG_TARGET_OS` 自动 `cargo:rustc-cfg=has_windows` 类输出或 emit feature 提示？**设计裁定**：build.rs 无法给本 crate 开 feature（circular）——落地方式改为 `platform/` 模块按 target_os cfg 选择 + app 的 feature 转发仍显式；或修订 docs/08 措辞。实现时裁定并同步 docs/08（文档先行）。
5. **测试**：映射表全量锚点（跨平台可跑）；转换逻辑纯函数测试；windows target `cargo check` 编译验证（`rustup target add x86_64-pc-windows-msvc`，check 不需要 MSVC linker）。

## Acceptance Criteria

- [ ] `cargo check -p xcontrol-input --features windows --target x86_64-pc-windows-msvc` 通过（本机）
- [ ] scancode 表锚点测试跨平台可跑（非门控纯数据区）
- [ ] build.rs 缺口裁定落地（代码或 docs 修订，报告说明选择）
- [ ] unsafe 全部 `// SAFETY:`；回调零慢操作纪律注释；check-deps/clippy/fmt 全绿
- [ ] 运行时验证清单文档化（移交 Windows 机器：捕获/注入/UIPI 提权场景）

## Notes

- `windows` crate 版本按 docs/01 §6 基线；feature 门控（windows）+ target cfg 双门控（macos crate 先例）。
- SendInput 的 UIPI 限制（注入提权进程失败）是运行时行为，模块文档 + 移交清单说明。
