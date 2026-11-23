# M3: Windows/X11 剪贴板

## Goal

Windows/X11 文本剪贴板后端（FR-4.1 mac 对应物）——arboard 统一读写 + 平台各自监听机制。编译级验证；运行时移交。

## Requirements

1. **Windows**（docs/05 §2）：`AddClipboardFormatListener` + 隐藏消息窗口 `WM_CLIPBOARDUPDATE`（事件驱动，非轮询）；剪贴板被占用 3×33ms 重试（rustdesk 同款）；arboard 读写。
2. **X11**（docs/05 §3）：`XFixesSelectSelectionInput` → SelectionNotify 事件监听；**常驻持有 selection** 响应对端 paste（arboard `wait()` 语义——docs/04 §4"退出 XControl 会清空 Linux 剪贴板内容"的提示与"临终转交"尽力而为留注释）；arboard Clipboard 路径（Primary 不同步，P2）。
3. **统一接口**：与 macos 后端同一 trait 形状（EchoFilter 防回环复用——指纹判据平台无关，直接复用 xcontrol-clipboard 的纯逻辑区）；超限信号语义一致。
4. **测试**：EchoFilter 复用回归（mac 任务已有，确保跨平台复用不重复实现）；各平台 target check；监听生命周期（消息窗口/SelectionNotify 线程）编译级验证。

## Acceptance Criteria

- [ ] `cargo check -p xcontrol-clipboard --features windows --target x86_64-pc-windows-msvc` + `--features x11 --target x86_64-unknown-linux-gnu` 通过
- [ ] 防回环 EchoFilter 复用无平台分支（纯逻辑区零改动证明）
- [ ] Windows 重试策略（3×33ms）与 X11 常驻语义在模块文档钉死（docs/05 逐条）
- [ ] check-deps/clippy/fmt 全绿；移交清单文档化

## Notes

- macos 后端（M2）已定 trait 形状与 EchoFilter；本任务是"同形不同底"。
- arboard 的 windows/x11 后端传递树检查（image 栈已裁剪保持）。
