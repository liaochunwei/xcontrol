//! `xcontrol-input` — 平台输入抽象：`InputSource`（捕获）与 `InputSink`（注入），
//! 以及 feature 门控的平台实现。
//!
//! # Trait 归属与 re-export
//!
//! 两 trait 定义在 `xcontrol-core::sink`（M1 起供 mock-platform 回环集成测试消费；
//! docs/08 §1 依赖图 11-17-m2-input-macos 补录 `input → core` 方向），本 crate
//! re-export 并提供平台实现——`xcontrol-app` 可把平台实现直接当作
//! `dyn InputSink` 交给 core 会话层（`route::apply_actions`）。
//!
//! # 平台实现（docs/08 §3、platform-code.md）
//!
//! 经 feature 启用：`macos` / `windows` / `x11` / `wayland`，模块路径固定
//! `platform/{macos,windows,x11,wayland}.rs`，由 xcontrol-app 按目标平台自动选择，
//! 也允许显式组合（Wayland 上叠加 x11 兜底）。平台路径（ADR-0003、docs/05 §0）：
//!
//! - 捕获：mac `CGEventTap` / win `WH_*_LL` hook / x11 `x11rb` record+grab /
//!   wayland `ashpd::input_capture` + `reis`（或 wlr-layer-shell 边缘窗口）；
//! - 注入：mac `CGEventPost` / win `SendInput` / x11 `XTEST`（禁用 XSendEvent）/
//!   wayland `reis` / wlr-virtual-pointer + virtual-keyboard。
//!
//! # 线程纪律（docs/01 §2、architecture-rules 红线 3）
//!
//! 平台回调线程禁止 async，只做"事件语义化 → `XKey` 转换 → `std::sync::mpsc`
//! 投递"；与 tokio 运行时的桥接由上层（app 会话循环）经 `tokio::sync::mpsc` 完成。
//!
//! # unsafe 纪律（docs/08 §2）
//!
//! 本 crate 是仓库唯一预期出现 unsafe 的层之一；workspace `unsafe_code = deny`
//! 在平台模块内文件级 `#![allow(unsafe_code)]` 放行，每个 unsafe 块强制
//! `// SAFETY:` 不变量注释。非门控区（[`keymap`]、[`winkeymap`]、[`x11keymap`]、
//! [`error`]）保持零 unsafe；x11 平台模块同样零 unsafe（x11rb 纯 Rust bindings，
//! `allow-unsafe-code` feature 未开——platform/x11.rs 模块文档）。

pub mod error;
pub mod keymap;
pub mod platform;
pub mod winkeymap;
pub mod x11keymap;

pub use error::InputError;
#[cfg(feature = "macos")]
pub use platform::macos::{
    AccessibilityStatus, CaptureSource, DisplayInfo, MacInjector, check_accessibility,
    cursor_location, display_layout,
};
#[cfg(feature = "wayland")]
pub use platform::wayland::{
    CapState, CapStatus, CapabilityTable, CaptureCmd, Compositor, CompositorCaps, GestureConfig,
    GestureCorner, GestureHit, GestureSensor, PortalCaptureSource, PortalHotkeys, PortalInitError,
    PortalInjector, UinputInjector, UinputStatus, evdev_to_mouse_button, evdev_to_xkey,
    mouse_button_to_evdev, portal_trigger_map, uinput_available, uinput_status, xkey_to_evdev,
};
#[cfg(feature = "windows")]
pub use platform::windows::{
    VirtualScreenInfo, WinCaptureSource, WinHotkeySource, WinInjector, cursor_location,
    virtual_screen_info,
};
#[cfg(feature = "x11")]
pub use platform::x11::{
    ScreenGeometry, X11CaptureSource, X11HotkeySource, X11Injector, cursor_location,
    display_geometry,
};
pub use xcontrol_core::sink::{InputSink, InputSource, SinkError};
