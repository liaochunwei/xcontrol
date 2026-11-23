# design: Windows/X11 剪贴板

```
src/platform/windows.rs   # AddClipboardFormatListener 隐藏窗口 + arboard
src/platform/x11.rs       # XFixes SelectionNotify + arboard（wait 语义）
（lib.rs EchoFilter 纯逻辑区不动——平台无关复用是本任务验收点）
```

- Windows：隐藏消息窗口（RegisterClassW + CreateWindowExW "static" 类 + GetMessage 泵）；`WM_CLIPBOARDUPDATE` → mpsc；重试：OpenClipboard 失败 33ms×3（rustdesk 先例，docs/05 明示）。
- X11：XFixes 扩展（x11rb `fixes` feature）select selection input（CLIPBOARD）；SelectionNotify → 读 arboard；**常驻**：进程持有 CLIPBOARD 所有权——arboard `set_contents` 后 Linux 需进程存活（`Clipboard::wait()` 语义）；"临终转交"（退出前 xclip 式转交）注释留 M5。
- 三平台 trait 统一：`ClipboardBackend` 工厂按 feature 编译对应实现（单 feature 多选一；同时开两个平台 feature 是编译错误——编译期断言）。
