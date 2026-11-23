# M5: 剪贴板完整版（图片/HTML/Wayland）

## Goal

FR-4.2 图片 + FR-4.3 HTML 全平台 + Wayland zwlr_data_control——剪贴板从"文本 only"到完整版。mac 本机 + SSH 远端 KDE Wayland 实测。

## Requirements

1. **图片剪贴板**（FR-4.2）：arboard 开 image-data feature（mac NSPasteboard TIFF/PNG、win CF_DIB、x11 image/png）；ClipFormat::Image 编解码路径（proto 已有格式枚举）；mac/win/x11 读写 round-trip；防回环/超限/信用窗口复用 M3 状态机（payload 换 bytes）。
2. **HTML 剪贴板**（FR-4.3）：mac `public.html`（objc2-app-kit 补充——M2 预留的 allow(unsafe_code) 终于用上）、win CF_HTML 格式（头格式注册）、x11 `text/html` target；arboard html 支持情况查证（arboard 3.7 html feature 状态——查 docs.rs，不支持则平台手写）；写读 round-trip。
3. **Wayland 剪贴板**：`wl-clipboard-rs`（zwlr_data_control/ext-data-control）——x11 feature 的 wayland 对应物；SSH 远端 KDE 实测（WAYLAND_DISPLAY=wayland-1 文本 round-trip + 监听）；GNOME ashpd portal 路径探测（portal 可用性检测 + 降级提示）。
4. **协议**：ClipAnnounce formats 含 image/html 的端到端（M3 状态机扩展测试——图片 payload 分块/信用窗口/超限）；16MiB 上限对图片的适用性复核（FR-4.5）。
5. **UI**：剪贴板设置页 image/html 开关从灰置转可用（M4 灰置项收口）。

## Acceptance Criteria

- [ ] mac 图片 round-trip 实测（NSPasteboard 写 PNG → 读回字节等价）+ HTML 实测
- [ ] SSH 远端 KDE Wayland：wl-clipboard-rs 文本 round-trip 实测；image/html 能力探测记录
- [ ] 端到端：图片 announce→chunk→end 全链路测试（M3 状态机扩展）
- [ ] 三平台新 unsafe 全 SAFETY（mac HTML 路径）；check-deps/clippy/fmt/test 全绿
- [ ] docs/04/05 缺口先行修订；移交清单更新

## Notes

- arboard html 支持：查 crate 文档——若 arboard 无 html 支持，mac 用 objc2（M2 预留）、win 用 windows crate clipboard API 手写 CF_HTML、x11 用 x11rb selection 传 text/html——实现裁定报告。
- wl-clipboard-rs feature 选择（纯 Rust Wayland 客户端）。
