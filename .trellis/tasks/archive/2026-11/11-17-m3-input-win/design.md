# design: Windows 输入层

```
src/
├── platform/windows.rs   # 全部 windows API（#[cfg(feature = "windows")]）
├── winkeymap.rs          # scancode set1 → HID 表（非门控纯数据，跨平台测试）
└── (macos.rs / keymap.rs 既有)
```

## 关键点

- LL hook 回调签名 `extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT`——真正的 unsafe FFI（core-graphics 安全封装先例不适用，windows crate hook 是裸 API）；SAFETY 注释 + 静态 mpsc sender（OnceLock 静态通道）。
- 消息循环线程：`GetMessageW` 阻塞泵；stop = `PostThreadMessageW(WM_QUIT)` + join。
- SendInput 数组一次提交多事件（比逐个 post 高效）；绝对坐标归一化 0..65535 换算（虚拟屏指标 GetSystemMetrics）。
- scancode 表来源：公开的 "Scan Code Set 1" 表（Windows 文档/QMK 来源）；锚点（A=0x1E、0=0x0B、Esc=0x01、修饰键 0x1D/0x2A/0x38/0x5B E0 前缀等）；E0/E1 前缀键（方向键/右修饰）在 LL hook 的 LLKHF_EXTENDED 标志区分——映射表按 (scancode, extended) 二维。
- build.rs 裁定预案：build.rs 不能给自身 crate 开 feature → 方案 A：`platform/mod.rs` 用 `#[cfg(target_os)]`（违反红线 2！）→ 方案 B（推荐）：修订 docs/08 措辞为"app 按 --features 显式选择，单平台发布构建由打包脚本固定 feature 组合"；实现时定夺并文档先行。

## 验证

- 本机：`rustup target add x86_64-pc-windows-msvc` → `cargo check -p xcontrol-input --features windows --target x86_64-pc-windows-msvc`（纯 Rust bindings，无需 MSVC）。
- 移交清单：捕获真实键鼠/注入生效/UIPI 提权/钩子超时行为——Windows 机器实测。
