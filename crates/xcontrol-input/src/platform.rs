//! 平台实现模块（docs/08 §3、.trellis/spec/rust-platform/platform-code.md）。
//!
//! 模块路径固定 `platform/{macos,windows,x11,wayland}.rs`，由 feature 门控
//! （architecture-rules 红线 2）：公共代码（trait/类型）中禁止 `#[cfg(target_os)]`，
//! 平台选择只发生在 feature 层。各平台模块是本 crate 唯一允许 unsafe 的位置
//! （docs/08 §2，文件级 `#![allow(unsafe_code)]` + 逐块 `// SAFETY:`）。

#[cfg(feature = "macos")]
pub mod macos;

#[cfg(feature = "windows")]
pub mod windows;

#[cfg(feature = "x11")]
pub mod x11;

#[cfg(feature = "wayland")]
pub mod wayland;
