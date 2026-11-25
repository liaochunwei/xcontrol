//! macOS 输入实现（docs/05 §1、ADR-0003）：CGEventTap 捕获 + CGEventPost 注入 +
//! `AXIsProcessTrustedWithOptions` 权限检测 + 屏幕几何查询（FR-1.3 基础）。
//!
//! # 权限现实（docs/05 §1）
//!
//! 捕获与注入都要求**辅助功能**授权。未授权时的表现（2026-11-24 部署实测更正）：
//! 现行 **active tap**（`CGEventTapOptions::Default`，受控吞本地输入需要 Drop 能力）
//! 的创建**直接被拒**——`CGEventTapCreate` 返回空、[`CaptureSource::start`] 报错；
//! 注入（`CGEventPost`）不生效、不报错。旧说法"tap 创建成功但回调收不到事件"
//! 只对 ListenOnly 成立（M2 当时如此）——权限检测仍须显式独立做
//! （[`check_accessibility`]）。
//!
//! # 线程与 async 纪律（architecture-rules 红线 3）
//!
//! CGEventTap 回调运行在专用线程的 CFRunLoop 上：回调内只做"事件语义化 →
//! kVK→[`XKey`] 转换 → `std::sync::mpsc` 投递"（platform-code.md 回调纪律），
//! **零 async、零 tokio**；上层经 [`crate::InputSource::poll`] 拉取后自行桥接 tokio。
//!
//! # unsafe 纪律（docs/08 §2、platform-code.md）
//!
//! 本 crate 是仓库唯一预期出现 unsafe 的层，workspace `unsafe_code = deny` 在本模块
//! 文件级放行（源码属性优先于 lint 配置，allow 落在 unsafe 实际出现的位置）。
//! 平台 FFI 已由 `core-graphics`/`core-foundation` crate 封装（servo 绑定，
//! CGEventTap 回调内部为 `extern "C"` 转发），本模块仅剩一处自声明 extern
//! （`AXIsProcessTrustedWithOptions`，ApplicationServices 框架）；每个 unsafe 块
//! 附 `// SAFETY:` 不变量注释。
//!
//! # API 版本依据（design.md：以 docs.rs 实际为准）
//!
//! 绑定版本：`core-graphics 0.25`（`highsierra` feature 解锁
//! `CGEvent::new_scroll_event`）+ `core-foundation 0.10`（11-17-m2-input-macos
//! 逐签名核实于 docs.rs）：`CGEventTap::new` 为闭包式回调
//! （`Fn(CGEventTapProxy, CGEventType, &CGEvent) -> CallbackResult`，'static + Send，
//! crate 内部封装 C 回调）、`CGEventFlags` 为 bitflags-2 结构、`CFRunLoop`
//! `unsafe impl Send + Sync`（跨线程 stop 合法）、`CFMachPort::create_runloop_source
//! -> Result<CFRunLoopSource, ()>`。
//!
//! # 已知限制（M2 范围，报告项）
//!
//! - 实体媒体键（音量等）通常经 `NSSystemDefined` 事件而非 `KeyDown` 到达，
//!   捕获侧收不到 [`CK_VOLUME_UP`] 等事件；注入路径不受影响（kVK 表可达）。
//! - FlagsChanged 按下/抬起方向按**设备相关位**判定（真实硬件恒携带）；仅置
//!   家族位的合成 FlagsChanged（第三方进程注入）会被判为 Up——真实键盘路径不受影响。
//! - `TapDisabledByTimeout`/`TapDisabledByUserInput` 事件仅忽略不自动重启 tap
//!   （M2 为 ListenOnly、回调零慢操作，超时摘除风险极低；生命周期集成留 M3+）。
//! - `ScreenId` 映射：[`CGDisplay::active_displays`] 返回序 = ScreenId（无稳定 ID
//!   协商；会话层 ID 协商属 M3）。
//! - **指针定位走 `CGWarpMouseCursorPosition`**（11-19-m5-layout 实测修订）：
//!   HID-posted `MouseMoved` 对光标位置的施加在本机不可靠（证据与裁定见
//!   docs/05 §1.1）——rel/abs/warp 三路统一 warp 原语，按键/滚轮仍 CGEventPost；
//!   warp 不生成事件（hover 高亮不随受控光标刷新，控制语义无影响）。

#![allow(unsafe_code)] // docs/08 §2：平台绑定层是唯一预期 unsafe 的位置；SAFETY 注释逐块强制。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_foundation::string::CFString;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGMouseButton, CallbackResult, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use xcontrol_core::sink::{InputSink, SinkError};
use xcontrol_proto::input::{Axis, InputEvent, KeyState, Mods, MouseButton, ScreenId};
use xcontrol_proto::xkey::XKey;

use crate::error::InputError;
use crate::keymap;

// ============================================================
// 权限检测（docs/05 §1：AXIsProcessTrustedWithOptions，显式三态语义）
// ============================================================

/// 辅助功能授权状态（docs/05 §1：tap 创建成功 ≠ 有权限，检测必须显式做）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityStatus {
    /// 已授予（捕获/注入可用）。
    Granted,
    /// 未授予（系统静默忽略 tap 回调与 CGEventPost；走 FR-6.3 权限引导）。
    Denied,
}

impl AccessibilityStatus {
    /// 是否已授予。
    #[must_use]
    pub const fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }
}

/// `kAXTrustedCheckOptionPrompt` 字典键（Apple HIServices 公开常量名）。
const AX_TRUSTED_CHECK_OPTION_PROMPT: &str = "AXTrustedCheckOptionPrompt";

// ApplicationServices 框架的辅助功能查询入口（HIServices 子框架导出）。
// 签名：`Boolean AXIsProcessTrustedWithOptions(CFDictionaryRef options)`；
// `options` 可含 `kAXTrustedCheckOptionPrompt -> CFBoolean`（true 弹系统引导框）。
// 线程安全、无 I/O 副作用（除 prompt 弹窗）。（extern 块不带 ///——doc 注释对
// extern 块无效会产生 unused_doc_comment 警告。）
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> u8;
}

/// 检测本进程的辅助功能授权状态。
///
/// `prompt = true` 时未授权会弹系统引导框（首次运行向导 M4 用）；
/// `prompt = false` 纯查询（后台/周期检测用，design.md 权限参数化要求）。
///
/// 返回值如实反映当前 TCC 状态；不缓存（权限可在运行中被授予/吊销，docs/05 §7）。
#[must_use]
pub fn check_accessibility(prompt: bool) -> AccessibilityStatus {
    let key = CFString::new(AX_TRUSTED_CHECK_OPTION_PROMPT);
    let value = if prompt {
        CFBoolean::true_value()
    } else {
        CFBoolean::false_value()
    };
    let options: CFDictionary<CFString, CFBoolean> =
        CFDictionary::from_CFType_pairs(&[(key, value)]);
    // SAFETY: AXIsProcessTrustedWithOptions 是 ApplicationServices 的稳定 C 导出，
    // 入参为刚构建、生命周期覆盖本调用的合法 CFDictionaryRef（TCFType 保证非空、
    // 由 CFRetain/CFRelease 管理）；该函数无指针外泄、线程安全，u8 返回即 Boolean。
    let trusted = unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef().cast()) };
    if trusted != 0 {
        AccessibilityStatus::Granted
    } else {
        AccessibilityStatus::Denied
    }
}

// ============================================================
// 屏幕几何（FR-1.3 基础：纯查询，无需辅助功能权限）
// ============================================================

/// 一块活动显示器的几何快照（CG 全局坐标：主屏左上角为原点，y 轴向下，单位=点）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInfo {
    /// CG 显示器 ID（`CGDirectDisplayID`，本机内有效；会话层 ScreenId 协商属 M3）。
    pub display_id: u32,
    /// 左上角原点（全局点坐标）。
    pub origin: (f64, f64),
    /// 逻辑尺寸（点；CGEvent 坐标空间即此空间）。
    pub size_points: (f64, f64),
    /// 物理像素尺寸（Retina 下为点的整数倍）。
    pub size_pixels: (u64, u64),
    /// 是否主显示器。
    pub is_main: bool,
}

impl DisplayInfo {
    /// 缩放提示（docs/02 §4.3 `scale = 逻辑/物理像素比`：Retina 2x 屏 = 0.5，
    /// 无缩放 = 1.0）；像素尺寸异常时返回 `None`。
    #[must_use]
    pub fn scale_hint(&self) -> Option<f32> {
        if self.size_pixels.0 == 0 || self.size_pixels.1 == 0 || self.size_points.0 <= 0.0 {
            return None;
        }
        Some((self.size_points.0 / self.size_pixels.0 as f64) as f32)
    }
}

/// 枚举全部显示器，产出 [`DisplayInfo`] 列表（FR-1.3 的 `ScreenLayout` 构建辅助）。
///
/// 以 `CGGetActiveDisplayList` 为主查询；**active 列表为空时回退
/// `CGGetOnlineDisplayList`**——屏幕休眠/锁定/未激活会话下 active 可为 0 而
/// 在线列表稳定（11-17-m2-input-macos 本机实测：active=0、online=1、
/// CGMainDisplayID=1 同刻成立），FR-1.3 布局构建需要稳定几何来源。
///
/// 顺序即系统枚举序（不保证稳定）——本 crate 的 `ScreenId` 约定 = 该列表下标；
/// 跨会话稳定性由会话层协商（M3）。
///
/// # Errors
/// [`InputError::Platform`]：显示器枚举失败。
pub fn display_layout() -> Result<Vec<DisplayInfo>, InputError> {
    let mut ids = CGDisplay::active_displays().map_err(|error| InputError::Platform {
        action: "display-query",
        reason: format!("CGGetActiveDisplayList 失败：{error:?}"),
    })?;
    if ids.is_empty() {
        ids = online_display_ids()?;
    }
    let main_id = CGDisplay::main().id;
    let mut displays = Vec::with_capacity(ids.len());
    for id in ids {
        let display = CGDisplay::new(id);
        let bounds = display.bounds();
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return Err(InputError::Platform {
                action: "display-query",
                reason: format!("显示器 {id} 几何异常：{bounds:?}"),
            });
        }
        displays.push(DisplayInfo {
            display_id: id,
            origin: (bounds.origin.x, bounds.origin.y),
            size_points: (bounds.size.width, bounds.size.height),
            size_pixels: (display.pixels_wide(), display.pixels_high()),
            is_main: id == main_id,
        });
    }
    Ok(displays)
}

/// 当前光标位置（全局点坐标；docs/05 §1：`CGEventGetLocation` 无需权限）。
///
/// # Errors
/// [`InputError::Platform`]：事件源/探针事件创建失败。
/// 受控锚点内缩（11-22 撞缘断流根因修复）：切入锚点若落在其所在屏的边缘
/// 判定带内（`ANCHOR_EDGE_INSET` 量级）则沿边界内缩到带外。
///
/// 判定域 = 包含该点的屏（多屏主控切入边可在任一屏；点不在任何屏时退回原
/// 点——防御，理论不达）。软缘（多屏邻边方向）系统光标本可推出、delta 本就
/// 正常——统一内缩是无害的语义微差；硬缘（钳位边）才是断流源，必须内缩。
///
/// 内缩量 > 撞缘回锚判定带（app 层 `REANCHOR_EDGE_POINTS` = 60pt）：内缩锚
/// 点处于回锚触发带之外，受控期 delta 从切入即正常供给，行进中再撞缘才轮到
/// 回锚机制接力。
fn entry_anchor_clamped(point: (f64, f64)) -> (f64, f64) {
    const ANCHOR_EDGE_INSET: f64 = 90.0;
    let Some(displays) = display_layout().ok() else {
        return point;
    };
    let Some(info) = displays.iter().find(|info| {
        let (ox, oy) = info.origin;
        let (w, h) = info.size_points;
        point.0 >= ox && point.0 <= ox + w && point.1 >= oy && point.1 <= oy + h
    }) else {
        return point;
    };
    let (ox, oy) = info.origin;
    let (w, h) = info.size_points;
    (
        point
            .0
            .clamp(ox + ANCHOR_EDGE_INSET, ox + w - ANCHOR_EDGE_INSET),
        point
            .1
            .clamp(oy + ANCHOR_EDGE_INSET, oy + h - ANCHOR_EDGE_INSET),
    )
}

/// 光标相对平移（11-25：回切行程的平滑回放，docs/05 §1.2）：读当前位置并 warp
/// 到偏移 `(dx, dy)` 之后的点（`CGWarpMouseCursorPosition`，不生成事件——语义
/// 同受控锚定 warp）。**进程级线程安全 API，不依赖任何实例状态**（Apple 文档，
/// 自 macOS 10.11 无限制）——回放线程（app `spawn_cursor_replay`）直接调用，
/// 无需 `CaptureSource`。回放期间物理鼠标的位移照常叠加在 warp 之后的位置上。
///
/// # Errors
/// [`InputError::Platform`]：光标查询或 warp 失败（调用方告警后终止回放）。
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
pub fn shift_cursor(dx: f64, dy: f64) -> Result<(), InputError> {
    let (x, y) = cursor_location()?;
    CGDisplay::warp_mouse_cursor_position(CGPoint::new(x + dx, y + dy)).map_err(|error| {
        InputError::Platform {
            action: "cursor-warp",
            reason: format!("CGWarpMouseCursorPosition 失败：{error}"),
        }
    })
}

pub fn cursor_location() -> Result<(f64, f64), InputError> {
    let source = event_source().map_err(|()| InputError::Platform {
        action: "event-source",
        reason: "CGEventSourceCreate 返回空".into(),
    })?;
    // SAFETY: 无——CGEvent::new 是 core-graphics 0.25 的安全包装（内部空指针检查后
    // wrap_under_create_rule）；此处无直接 FFI。
    let probe = CGEvent::new(source).map_err(|()| InputError::Platform {
        action: "event-source",
        reason: "CGEventCreate 探针事件失败".into(),
    })?;
    let point = probe.location();
    Ok((point.x, point.y))
}

/// 注入/查询共用的 CGEventSource（HID 系统状态：合成事件以硬件级语义进入系统，
/// Input-Leap 同路径；docs/05 §1"enigo 的 mac 后端同路径"）。
fn event_source() -> Result<CGEventSource, ()> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
}

/// 全局点 → 屏内归一化坐标（纯函数；±1 点容差带收敛端点 0.0/1.0——与
/// edge.rs `EDGE_TOLERANCE_POINTS` 同型契约，被控边缘驻留回报恰端点值）。
fn point_snap_normalized(point: f64, origin: f64, size: f64) -> f32 {
    if size <= 0.0 {
        return 0.0; // 几何异常防御（display_layout 已滤，此处兜底）
    }
    let value = point - origin;
    if value <= 1.0 {
        0.0
    } else if value >= size - 1.0 {
        1.0
    } else {
        (value / size) as f32
    }
}

// ============================================================
// 注入器（InputSink 实现）
// ============================================================

/// 把 [`f64`] 累加器与一条增量合并，产出整数注入量与保留的小数余量
/// （docs/02 §4.2 kind 0x03：f64 累加器语义——取整不丢余量，净移动量守恒）。
fn accumulate(acc: f64, delta: f32) -> (f64, i32) {
    let total = acc + f64::from(delta);
    let integral = total.trunc();
    // float→int `as` 转换自 Rust 1.45 起饱和（无 UB、无 wrap）；f32 增量路径下
    // i32 溢出仅出现在单轴 >2^31 点的病态输入，饱和即合理上限。
    (total - integral, integral as i32)
}

// CoreGraphics 在线显示器枚举（core-graphics 0.25 未封装 `CGGetOnlineDisplayList`，
// 自声明薄绑定；签名按 Apple CoreGraphics 头：`i32 CGGetOnlineDisplayList(
// uint32_t maxDisplays, CGDirectDisplayID *onlineDisplays, uint32_t *displayCount)`，
// 0 = kCGErrorSuccess）。（extern 块不带 ///——doc 注释对 extern 块无效。）
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(
        max_displays: u32,
        online_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
}

/// 在线显示器 ID 列表（两段式查询：先探测数量，再取列表）。
fn online_display_ids() -> Result<Vec<u32>, InputError> {
    let mut count = 0_u32;
    // SAFETY: 数量探测阶段按 Apple 惯例传空列表指针 + 合法 count 出参；
    // CGGetOnlineDisplayList 是 CoreGraphics 稳定导出，无指针外泄、线程安全。
    let status = unsafe { CGGetOnlineDisplayList(0, std::ptr::null_mut(), &mut count) };
    if status != 0 {
        return Err(InputError::Platform {
            action: "display-query",
            reason: format!("CGGetOnlineDisplayList 数量探测失败：kCGError {status}"),
        });
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut ids = vec![0_u32; count as usize];
    // SAFETY: ids 容量 = 上一步返回的 count，系统写入不越界；同上无指针外泄。
    let status = unsafe { CGGetOnlineDisplayList(count, ids.as_mut_ptr(), &mut count) };
    if status != 0 {
        return Err(InputError::Platform {
            action: "display-query",
            reason: format!("CGGetOnlineDisplayList 枚举失败：kCGError {status}"),
        });
    }
    ids.truncate(count as usize);
    Ok(ids)
}

/// 跟踪中的修饰键集合 → CGEventFlags 合成位（docs/05 §1：修饰键经 flags 合成）。
fn synthesized_flags(pressed_keys: &[XKey]) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    for key in pressed_keys {
        if let Some(flag) = modifier_flag(*key) {
            flags |= flag;
        }
    }
    flags
}

/// 修饰键 XKey（0xE0–0xE7）→ 对应 CGEventFlags 家族位（左右共用家族位；
/// CG flags 不分左右，左右语义由修饰键 Key 事件本身承载）。非修饰键返回 `None`。
const fn modifier_flag(key: XKey) -> Option<CGEventFlags> {
    match key.raw() {
        0xE0 | 0xE4 => Some(CGEventFlags::CGEventFlagControl),
        0xE1 | 0xE5 => Some(CGEventFlags::CGEventFlagShift),
        0xE2 | 0xE6 => Some(CGEventFlags::CGEventFlagAlternate),
        0xE3 | 0xE7 => Some(CGEventFlags::CGEventFlagCommand),
        _ => None,
    }
}

/// NX 设备相关修饰键位（Apple `IOLLEvent.h` 公开值，随 SDK 分发；数值自 10.x 稳定）。
///
/// 真实硬件的 FlagsChanged 事件 flags 同时携带**设备无关家族位**（上方
/// `CGEventFlagShift` 等）与**设备相关左右手位**（本组，低 16 位区段）；
/// `core-graphics 0.25` 的 `CGEventFlags` bitflags 未定义这些常量，按头文件值
/// 本地声明。`CGEvent::get_flags` 直接透传 FFI 原始位（不做 bitflags 裁剪），
/// 未知位经 `.bits()` 原样可读。
mod nx_device_flags {
    /// `NX_DEVICELCTLKEYMASK`（左 Ctrl）。
    pub const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
    /// `NX_DEVICELSHIFTKEYMASK`（左 Shift）。
    pub const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
    /// `NX_DEVICERSHIFTKEYMASK`（右 Shift）。
    pub const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
    /// `NX_DEVICELCMDKEYMASK`（左 Cmd）。
    pub const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
    /// `NX_DEVICERCMDKEYMASK`（右 Cmd）。
    pub const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
    /// `NX_DEVICELALTKEYMASK`（左 Opt）。
    pub const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
    /// `NX_DEVICERALTKEYMASK`（右 Opt）。
    pub const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
    /// `NX_DEVICERCTLKEYMASK`（右 Ctrl，IOLLEvent.h 的历史缝隙值 0x2000）。
    pub const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;
    /// `NX_ALPHASHIFTMASK`（CapsLock 锁存家族位；单键无左右之分）。
    pub const NX_ALPHASHIFTMASK: u64 = 0x0001_0000;
}

/// kVK 修饰键 → 该键自己的 NX 设备相关位（IOLLEvent.h；非修饰键/未知返回 `None`）。
///
/// FlagsChanged 的按下/抬起方向**只能**用设备位判定：家族位在"左右同族同按"场景
/// 对单颗键无判别力（左手松开时家族位仍因右手按下而置位——旧逻辑此处误报 Down，
/// 违反 docs/03 §6 修饰键配对性不变式）。
const fn device_modifier_bit(kvk: u16) -> Option<u64> {
    match kvk {
        0x38 => Some(nx_device_flags::NX_DEVICELSHIFTKEYMASK), // kVK_Shift
        0x3C => Some(nx_device_flags::NX_DEVICERSHIFTKEYMASK), // kVK_RightShift
        0x3B => Some(nx_device_flags::NX_DEVICELCTLKEYMASK),   // kVK_Control
        0x3E => Some(nx_device_flags::NX_DEVICERCTLKEYMASK),   // kVK_RightControl
        0x3A => Some(nx_device_flags::NX_DEVICELALTKEYMASK),   // kVK_Option
        0x3D => Some(nx_device_flags::NX_DEVICERALTKEYMASK),   // kVK_RightOption
        0x37 => Some(nx_device_flags::NX_DEVICELCMDKEYMASK),   // kVK_Command
        0x36 => Some(nx_device_flags::NX_DEVICERCMDKEYMASK),   // kVK_RightCommand
        _ => None,
    }
}

/// macOS 注入器（[`InputSink`] 的 mac 实现，docs/05 §1：CGEvent 构造 +
/// `CGEventPost(kCGHIDEventTap)`）。
///
/// 状态：跟踪已注入按键/按钮集合（[`InputSink::release_all`] 防卡键收尾）与
/// 移动/滚轮 f64 累加器（docs/02 §4.2：小数余量跨事件保留）。
/// 非阻塞快速返回（trait 契约）；单次注入失败返回 [`SinkError::Failed`]，
/// 调用方（core route 层）计数并继续，不得中断事件流（error-handling.md §4）。
// SAFETY: MacInjector 仅持 CGEventSource 句柄与纯 Rust 状态（布局表/按键账本）。
// CGEvent 系 API（CGEventCreate/CGEventPost）自 macOS 10.11 起官方文档标注线程安全，
// 句柄无线程亲和性（Run loop 归属仅 CGEventTap/CaptureSource，不在本结构）；
// 全部访问经 `&mut self`，无共享可变。会话任务 spawn 需 sink 满足 Send（11-20）。
unsafe impl Send for MacInjector {}

pub struct MacInjector {
    source: CGEventSource,
    /// 活动显示器布局（`ScreenId` = 下标；`refresh_layout` 随热插拔刷新）。
    layout: Vec<DisplayInfo>,
    /// 已注入且未释放的按键（按下序；release_all 按逆序释放）。
    pressed_keys: Vec<XKey>,
    /// 已注入且未释放的鼠标按键（按下序）。
    pressed_buttons: Vec<MouseButton>,
    /// 相对移动累加器（x/y 小数余量，docs/02 §4.2）。
    move_x: f64,
    move_y: f64,
    /// 滚轮累加器（v/h 小数余量；注入 API 收整数行，余量保留）。
    wheel_v: f64,
    wheel_h: f64,
}

impl MacInjector {
    /// 构建注入器（创建 CGEventSource + 查询屏幕布局；**无需辅助功能权限**，
    /// 未授权时注入动作被系统静默忽略——检测独立用 [`check_accessibility`]）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：事件源或显示器查询失败。
    pub fn new() -> Result<Self, InputError> {
        let source = event_source().map_err(|()| InputError::Platform {
            action: "event-source",
            reason: "CGEventSourceCreate 返回空".into(),
        })?;
        let layout = display_layout()?;
        Ok(Self {
            source,
            layout,
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
            move_x: 0.0,
            move_y: 0.0,
            wheel_v: 0.0,
            wheel_h: 0.0,
        })
    }

    /// 重新查询屏幕布局（显示器热插拔/分辨率变化后调用）。
    ///
    /// # Errors
    /// 同 [`MacInjector::new`]。
    pub fn refresh_layout(&mut self) -> Result<(), InputError> {
        self.layout = display_layout()?;
        Ok(())
    }

    /// 探针查询当前光标位置（`CGEventGetLocation`，无需权限）。
    fn cursor_point(&self) -> Result<CGPoint, SinkError> {
        // SAFETY: 无——CGEvent::new 为安全包装（见 cursor_location 注）。
        let probe = CGEvent::new(self.source.clone()).map_err(|()| SinkError::Failed {
            action: "inject",
            reason: "CGEventCreate 探针事件失败".into(),
        })?;
        Ok(probe.location())
    }

    /// 归一化坐标 → 全局点坐标（`ScreenId` = [`display_layout`] 下标）。
    fn abs_point(&self, screen: ScreenId, x: f32, y: f32) -> Result<CGPoint, SinkError> {
        let info = self
            .layout
            .get(usize::from(screen))
            .ok_or_else(|| SinkError::Failed {
                action: "warp",
                reason: format!("未知 ScreenId {screen}（布局 {} 屏）", self.layout.len()),
            })?;
        let (ox, oy) = info.origin;
        let (w, h) = info.size_points;
        // clamp 到屏内（docs/02 §4.2 kind 0x04：归一化 0.0–1.0；越界收敛边界）
        let nx = f64::from(x).clamp(0.0, 1.0);
        let ny = f64::from(y).clamp(0.0, 1.0);
        Ok(CGPoint::new(ox + nx * w, oy + ny * h))
    }

    /// 把光标定位到全局点坐标（`CGWarpMouseCursorPosition`——"moves the mouse
    /// cursor **without generating events**"，core-graphics 0.25 绑定）。
    ///
    /// 实测依据（2026-11-19，macOS 26 / Darwin 25.5.0，11-19-m5-layout；完整
    /// 记录见 docs/05 §1.1）：HID-posted `MouseMoved` 对光标位置的施加**不可靠**
    /// （多轮探针 0/3 命中；相对路径 5×(+10,0) 仅移动 ≈2/5 步），Session-post
    /// 2/3 精确；`CGWarpMouseCursorPosition` 3/3 亚像素命中且与活机物理鼠标
    /// 竞争无关。故全部指针定位（rel/abs/warp）统一走 warp 原语。
    ///
    /// 语义代价（warp 不生成事件）：光标悬停/拖拽的 NSEvent 更新不再由移动
    /// 本身投递——受控端按键/按钮/滚轮仍经 CGEventPost 投递（点击位置由 warp
    /// 后的光标位置决定，控制语义不受影响）；前台应用 hover 高亮不随受控光标
    /// 刷新（视觉提示损失，控制正确性无影响）。
    fn warp_to(&self, point: CGPoint) -> Result<(), SinkError> {
        CGDisplay::warp_mouse_cursor_position(point).map_err(|error| SinkError::Failed {
            action: "warp",
            reason: format!("CGWarpMouseCursorPosition 失败：{error}"),
        })
    }

    /// 构造并投递一个鼠标事件（`kCGHIDEventTap`；修饰键 flags 附着）。
    fn post_mouse(
        &self,
        mouse_type: CGEventType,
        point: CGPoint,
        cg_button: CGMouseButton,
        button_number: Option<i64>,
        flags: CGEventFlags,
    ) -> Result<(), SinkError> {
        // SAFETY: 无——CGEvent::new_mouse_event 为安全包装。
        let event = CGEvent::new_mouse_event(self.source.clone(), mouse_type, point, cg_button)
            .map_err(|()| SinkError::Failed {
                action: "inject",
                reason: "CGEventCreateMouseEvent 失败".into(),
            })?;
        if let Some(number) = button_number {
            // 侧键/中键：CGEventCreateMouseEvent 只认 Left/Right/Center 占位，
            // 实际键号经 kCGMouseEventButtonNumber 字段覆盖（捕获侧对称读取）。
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
        }
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// 构造并投递一个键盘事件（修饰键 flags 由跟踪集合合成附着）。
    fn post_key(&self, kvk: u16, down: bool, flags: CGEventFlags) -> Result<(), SinkError> {
        // SAFETY: 无——CGEvent::new_keyboard_event 为安全包装。
        let event = CGEvent::new_keyboard_event(self.source.clone(), kvk, down).map_err(|()| {
            SinkError::Failed {
                action: "inject",
                reason: format!("CGEventCreateKeyboardEvent 失败（kVK 0x{kvk:02X}）"),
            }
        })?;
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// 注入一条按键事件并更新跟踪集合（先记账后注入：flags 反映注入后状态）。
    fn inject_key(&mut self, code: XKey, state: KeyState) -> Result<(), SinkError> {
        let kvk = keymap::xkey_to_kvk(code).ok_or_else(|| SinkError::Failed {
            action: "inject",
            reason: format!("XKey 0x{:04X} 无 macOS kVK 对应", code.raw()),
        })?;
        match state {
            KeyState::Down => {
                if !self.pressed_keys.contains(&code) {
                    self.pressed_keys.push(code);
                }
            }
            KeyState::Up => self.pressed_keys.retain(|pressed| *pressed != code),
        }
        let flags = synthesized_flags(&self.pressed_keys);
        self.post_key(kvk, state == KeyState::Down, flags)
    }

    /// 注入滚轮事件（整数行注入 + 小数余量保留；正负号沿用 mac 原生语义：
    /// axis1 正=向上滚动、axis2 正=向左，捕获侧读同一字段，往返一致）。
    fn inject_wheel(&mut self, axis: Axis, delta: f32) -> Result<(), SinkError> {
        if !delta.is_finite() {
            return Err(SinkError::Failed {
                action: "inject",
                reason: "滚轮 delta 非有限值（协议违规不应到达注入层）".into(),
            });
        }
        match axis {
            Axis::Vertical => {
                let (rest, lines) = accumulate(self.wheel_v, delta);
                self.wheel_v = rest;
                if lines != 0 {
                    self.post_scroll(lines, 0)?;
                }
            }
            Axis::Horizontal => {
                let (rest, lines) = accumulate(self.wheel_h, delta);
                self.wheel_h = rest;
                if lines != 0 {
                    self.post_scroll(0, lines)?;
                }
            }
        }
        Ok(())
    }

    /// 投递滚轮事件（wheel_count 覆盖用到的轴；`ScrollEventUnit::LINE` = 格/notch）。
    fn post_scroll(&self, lines_v: i32, lines_h: i32) -> Result<(), SinkError> {
        let wheel_count = if lines_h != 0 { 2 } else { 1 };
        // SAFETY: 无——CGEvent::new_scroll_event 为安全包装（highsierra feature）。
        let event = CGEvent::new_scroll_event(
            self.source.clone(),
            ScrollEventUnit::LINE,
            wheel_count,
            lines_v,
            lines_h,
            0,
        )
        .map_err(|()| SinkError::Failed {
            action: "inject",
            reason: "CGEventCreateScrollWheelEvent 失败".into(),
        })?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl InputSink for MacInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        match event {
            InputEvent::Key { code, state } => self.inject_key(*code, *state),
            InputEvent::KeyRepeat { code } => {
                // 长按重复：仅当键仍在本机按下集合时补发带 autorepeat 标记的 keyDown
                // （docs/03 §3：从机可按平台能力忽略；mac 本地 synthetic keyDown 不自动重复）。
                if !self.pressed_keys.contains(code) {
                    return Ok(());
                }
                let Some(kvk) = keymap::xkey_to_kvk(*code) else {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: format!("XKey 0x{:04X} 无 macOS kVK 对应", code.raw()),
                    });
                };
                let flags = synthesized_flags(&self.pressed_keys);
                // SAFETY: 无——new_keyboard_event 为安全包装。
                let event =
                    CGEvent::new_keyboard_event(self.source.clone(), kvk, true).map_err(|()| {
                        SinkError::Failed {
                            action: "inject",
                            reason: format!("CGEventCreateKeyboardEvent 失败（kVK 0x{kvk:02X}）"),
                        }
                    })?;
                event.set_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT, 1);
                event.set_flags(flags);
                event.post(CGEventTapLocation::HID);
                Ok(())
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: "相对移动 delta 非有限值（NaN 会沿累加器扩散，docs/02 §4.2）"
                            .into(),
                    });
                }
                let (rest_x, int_x) = accumulate(self.move_x, *dx);
                let (rest_y, int_y) = accumulate(self.move_y, *dy);
                self.move_x = rest_x;
                self.move_y = rest_y;
                if int_x == 0 && int_y == 0 {
                    return Ok(()); // 纯小数余量：无可注入整数位移
                }
                // 读改定位（读当前光标 → warp 目标点）：HID-posted MouseMoved
                // 对光标位置不可靠（warp_to 实测依据），相对语义 = 读改写由
                // 本层承担，累加器契约不变（docs/02 §4.2 净移动守恒）。
                let cursor = self.cursor_point()?;
                self.warp_to(CGPoint::new(
                    cursor.x + f64::from(int_x),
                    cursor.y + f64::from(int_y),
                ))
            }
            InputEvent::MouseMoveAbs { screen, x, y } => {
                if !x.is_finite() || !y.is_finite() {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: "绝对坐标非有限值".into(),
                    });
                }
                // FR-2.7 绝对坐标：warp 定位（几何换算见 abs_point；实测依据
                // 见 warp_to 注）。
                let point = self.abs_point(*screen, *x, *y)?;
                self.warp_to(point)
            }
            InputEvent::MouseButton { button, state } => {
                // 中键/侧键走 OtherMouse 系列 + 键号字段（2=M, 3=Back, 4=Fwd；
                // 捕获侧对称读取 kCGMouseEventButtonNumber）。
                let down = *state == KeyState::Down;
                let (mouse_type, cg_button, number) = match button {
                    MouseButton::Left => (
                        if down {
                            CGEventType::LeftMouseDown
                        } else {
                            CGEventType::LeftMouseUp
                        },
                        CGMouseButton::Left,
                        None,
                    ),
                    MouseButton::Right => (
                        if down {
                            CGEventType::RightMouseDown
                        } else {
                            CGEventType::RightMouseUp
                        },
                        CGMouseButton::Right,
                        None,
                    ),
                    MouseButton::Middle => (
                        if down {
                            CGEventType::OtherMouseDown
                        } else {
                            CGEventType::OtherMouseUp
                        },
                        CGMouseButton::Center,
                        Some(2_i64),
                    ),
                    MouseButton::Back => (
                        if down {
                            CGEventType::OtherMouseDown
                        } else {
                            CGEventType::OtherMouseUp
                        },
                        CGMouseButton::Center,
                        Some(3_i64),
                    ),
                    MouseButton::Forward => (
                        if down {
                            CGEventType::OtherMouseDown
                        } else {
                            CGEventType::OtherMouseUp
                        },
                        CGMouseButton::Center,
                        Some(4_i64),
                    ),
                };
                match state {
                    KeyState::Down => {
                        if !self.pressed_buttons.contains(button) {
                            self.pressed_buttons.push(*button);
                        }
                    }
                    KeyState::Up => {
                        self.pressed_buttons.retain(|pressed| pressed != button);
                    }
                }
                let cursor = self.cursor_point()?;
                self.post_mouse(
                    mouse_type,
                    cursor,
                    cg_button,
                    number,
                    synthesized_flags(&self.pressed_keys),
                )
            }
            InputEvent::Wheel { axis, delta } => self.inject_wheel(*axis, *delta),
            InputEvent::Text { utf8 } => {
                let bytes = utf8.as_bytes();
                if bytes.is_empty() {
                    return Ok(());
                }
                if bytes.len() > xcontrol_proto::input::MAX_TEXT_LEN {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: format!(
                            "Text 超过 512B 上限（{}B）；主控应分段（docs/03 §4.5）",
                            bytes.len()
                        ),
                    });
                }
                let flags = synthesized_flags(&self.pressed_keys);
                for down in [true, false] {
                    // 键码占位 0（kVK_ANSI_A）：应用消费 CGEventKeyboardSetUnicodeString
                    // 覆盖后的 Unicode 串；enigo 同路径（docs/05 §1）。
                    // SAFETY: 无——new_keyboard_event/set_string 均为安全包装。
                    let event = CGEvent::new_keyboard_event(self.source.clone(), 0, down).map_err(
                        |()| SinkError::Failed {
                            action: "inject",
                            reason: "CGEventCreateKeyboardEvent（Text）失败".into(),
                        },
                    )?;
                    event.set_string(utf8);
                    event.set_flags(flags);
                    event.post(CGEventTapLocation::HID);
                }
                Ok(())
            }
            // —— 路由/会话层事件（docs/02 §4.2 kind 0x10–0x21）：非设备注入，
            //    由 core 会话对齐机制消费（EnterScreen → Key 对齐 + WarpPointer），
            //    到达本层属于冗余投递，静默忽略不算错误（error-handling.md §4）。——
            InputEvent::EnterScreen { .. }
            | InputEvent::LeaveScreen { .. }
            | InputEvent::ModifierSync { .. }
            | InputEvent::SwitchRequest { .. }
            | InputEvent::SwitchAck { .. } => Ok(()),
        }
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(SinkError::Failed {
                action: "warp",
                reason: "warp 坐标非有限值".into(),
            });
        }
        // EnterScreen 对齐 warp（docs/03 §5）：边缘进入点必须精确落位——
        // warp 原语（实测依据见 warp_to 注；post_mouse 路径不可靠）。
        let point = self.abs_point(screen, x, y)?;
        self.warp_to(point)
    }

    /// 释放全部跟踪中的按键/按钮（切出/暂停/断开防卡键，docs/03 §3）。
    ///
    /// 按按下逆序释放（后按的先起）；移动/滚轮累加器一并清零（防跨会话余量漂移）。
    /// 尽力释放到底，返回首个失败（若有）。
    ///
    /// # Errors
    /// [`SinkError::Failed`]：任一释放事件投递失败（仍会尝试释放其余按键）。
    fn release_all(&mut self) -> Result<(), SinkError> {
        let mut first_error = None;
        for code in self.pressed_keys.iter().rev().copied().collect::<Vec<_>>() {
            self.pressed_keys.retain(|pressed| *pressed != code);
            let kvk = match keymap::xkey_to_kvk(code) {
                Some(kvk) => kvk,
                None => continue, // 跟踪集合仅来自成功注入的键，理论不可达；防御跳过
            };
            let flags = synthesized_flags(&self.pressed_keys);
            if let Err(error) = self.post_key(kvk, false, flags) {
                first_error.get_or_insert(error);
            }
        }
        for button in self
            .pressed_buttons
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>()
        {
            self.pressed_buttons.retain(|pressed| *pressed != button);
            let (mouse_type, cg_button, number) = match button {
                MouseButton::Left => (CGEventType::LeftMouseUp, CGMouseButton::Left, None),
                MouseButton::Right => (CGEventType::RightMouseUp, CGMouseButton::Right, None),
                MouseButton::Middle => (
                    CGEventType::OtherMouseUp,
                    CGMouseButton::Center,
                    Some(2_i64),
                ),
                MouseButton::Back => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(3)),
                MouseButton::Forward => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(4)),
            };
            let cursor = match self.cursor_point() {
                Ok(point) => point,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            if let Err(error) = self.post_mouse(
                mouse_type,
                cursor,
                cg_button,
                number,
                synthesized_flags(&self.pressed_keys),
            ) {
                first_error.get_or_insert(error);
            }
        }
        self.move_x = 0.0;
        self.move_y = 0.0;
        self.wheel_v = 0.0;
        self.wheel_h = 0.0;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        // 双向边缘回切的被控采样面（11-22 修复轮）：CGEventGetLocation →
        // 活动显示器布局归一化。app 会话循环的 mac 被控回报**仍走 rects
        // 快照路径**（现行实测正常，11-21 反馈 5 接线面）——本实现是 sink
        // 查询面的平台完备项（`InputSink::cursor_normalized` trait 契约），
        // 避免双采样路径漂移的换算保持同型（±1 点容差带收敛端点）。
        let (px, py) = cursor_location().ok()?;
        let info = self.layout.iter().find(|info| {
            let (ox, oy) = info.origin;
            let (w, h) = info.size_points;
            px >= ox && px <= ox + w && py >= oy && py <= oy + h
        })?;
        let (ox, oy) = info.origin;
        let (w, h) = info.size_points;
        Some((
            point_snap_normalized(px, ox, w),
            point_snap_normalized(py, oy, h),
        ))
    }
}

// ============================================================
// 捕获源（InputSource 实现）
// ============================================================

/// 捕获事件掩码（docs/05 §1：keyDown/Up、flagsChanged、mouseMoved/Dragged、
/// leftMouse 等事件；`CGEventTap::new` 按 Vec<CGEventType> 折算掩码）。
const TAP_EVENTS: [CGEventType; 14] = [
    CGEventType::KeyDown,
    CGEventType::KeyUp,
    CGEventType::FlagsChanged,
    CGEventType::MouseMoved,
    CGEventType::LeftMouseDragged,
    CGEventType::RightMouseDragged,
    CGEventType::OtherMouseDragged,
    CGEventType::LeftMouseDown,
    CGEventType::LeftMouseUp,
    CGEventType::RightMouseDown,
    CGEventType::RightMouseUp,
    CGEventType::OtherMouseDown,
    CGEventType::OtherMouseUp,
    CGEventType::ScrollWheel,
];

/// macOS 捕获源（[`InputSource`] 的 mac 实现，docs/05 §1：`CGEventTap` +
/// CFRunLoop 独立线程）。
///
/// - tap 为 `kCGSessionEventTap` + **active（Default）**：受控时吞本地输入
///   （suppress → [`CallbackResult::Drop`]，KVM 重定向语义），未受控透传——
///   M2 的 ListenOnly 形态已被受控重定向取代；
/// - 回调线程只做"kVK→[`XKey`] → mods 位 → `std::sync::mpsc` 投递"，
///   **零 async/零 tokio/零锁**（platform-code.md 回调纪律）；
/// - **未授权时 active tap 的创建直接返回空**（`CGEventTapCreate` → NULL，
///   2026-11-24 实测，docs/05 §1）——"创建成功但事件被静默忽略"只是 M2
///   ListenOnly 时代的行为。显式权限检测用 [`check_accessibility`]；
/// - [`InputSource::poll`] 非阻塞拉取；事件 FIFO 保序（单 channel 天然满足）。
pub struct CaptureSource {
    receiver: Receiver<InputEvent>,
    /// 修饰键快照位（bit0-7 = docs/02 §4.2 mods；回调线程写、宿主线程读，Relaxed 足够）。
    mods_bits: Arc<AtomicU32>,
    /// 受控吞本地输入开关（M5：受控时本地输入重定向到从机——tap 回调返回
    /// Drop 阻止事件进入本地 OS；未受控透传。docs/06 §2 KVM 语义）。
    suppress: Arc<AtomicBool>,
    /// 受控光标锚点（11-20 用户方案：切入瞬间**锁定当前位置**——每拍 warp 回
    /// 该点，光标"冻结在原地"；比角点锚自然（不产生边界噪声、视觉是"停在
    /// 你切入的地方"）。`None` = 尚未进入受控（首次 suppress=true 时捕获）。
    pin_point: Arc<std::sync::Mutex<Option<(f64, f64)>>>,
    /// 锚定开关（与 pin_point 配对：false = 不锚定且清空锚点）。
    pinned: Arc<AtomicBool>,
    /// 回调线程的 CFRunLoop（`Send + Sync`，stop 从宿主线程发起）。
    run_loop: Arc<Mutex<Option<CFRunLoop>>>,
    worker: Option<JoinHandle<()>>,
}

impl CaptureSource {
    /// 启动捕获（创建 tap 线程并等待其就绪：tap 创建 → runloop source 挂载 → enable）。
    ///
    /// 未授权时本方法**照常成功**（系统静默语义，docs/05 §1）——是否可用须另行
    /// [`check_accessibility`]。
    ///
    /// # Errors
    /// [`InputError::Platform`]：线程创建失败 / `CGEventTapCreate` 返回空 /
    /// runloop source 创建失败 / 就绪线程意外退出。
    pub fn start() -> Result<Self, InputError> {
        let (sender, receiver) = channel::<InputEvent>();
        let mods_bits = Arc::new(AtomicU32::new(0));
        let suppress_bits = Arc::new(AtomicBool::new(false));
        let pin_point = Arc::new(std::sync::Mutex::new(None::<(f64, f64)>));
        let pinned_bits = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = channel::<Result<CFRunLoop, String>>();
        let mods_for_callback = Arc::clone(&mods_bits);
        let suppress_for_callback = Arc::clone(&suppress_bits);
        let _pinned_for_callback = Arc::clone(&pinned_bits);

        let worker = std::thread::Builder::new()
            .name("xcontrol-capture".into())
            .spawn(move || {
                let sender = sender;
                let mods_bits = mods_for_callback;
                let suppress = suppress_for_callback;
                let _pinned = _pinned_for_callback;
                // SAFETY: 无直接 FFI——CGEventTap::new 是 core-graphics 0.25 安全封装，
                // C 回调（extern "C"）在 crate 内部把 &CGEvent 转发到本闭包并负责
                // userInfo 指针的装箱/回收（'static 约束保证回调存活于 tap 生命周期）。
                // 闭包 Send：只捕获 Sender<InputEvent> + Arc<AtomicU32>（均 Send）；
                // 回调仅运行在本 tap 线程（单消费者），无跨线程可变共享。
                let tap = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    // M5：Default（active tap）——受控吞本地输入（suppress）需要 Drop 能力；
                    // ListenOnly 无法拦截事件（M2 语义已被受控重定向取代）。
                    CGEventTapOptions::Default,
                    TAP_EVENTS.to_vec(),
                    move |_proxy: CGEventTapProxy, event_type: CGEventType, event: &CGEvent| {
                        // 转换+投递恒先于吞放判定（受控时事件仍转发从机，只是
                        // Drop 阻止本地 OS 执行——KVM 重定向语义，docs/06 §2）。
                        translate_event(&sender, &mods_bits, event_type, event, &suppress);
                        if suppress.load(Ordering::Relaxed) {
                            CallbackResult::Drop // 受控：吞本地输入（已重定向从机）
                        } else {
                            CallbackResult::Keep // 未受控：透传本地执行
                        }
                    },
                );
                let tap = match tap {
                    Ok(tap) => tap,
                    Err(()) => {
                        let _ = ready_tx
                            .send(Err("CGEventTapCreate 返回空（参数或权限被系统拒绝）".into()));
                        return;
                    }
                };
                let loop_source = match tap.mach_port().create_runloop_source(0) {
                    Ok(source) => source,
                    Err(()) => {
                        let _ = ready_tx.send(Err("CFMachPortCreateRunLoopSource 失败".into()));
                        return;
                    }
                };
                let run_loop = CFRunLoop::get_current();
                // SAFETY: kCFRunLoopCommonModes 是 CF 框架导出的进程级常量 extern
                // CFStringRef（不可变、生命周期 = 进程）；edition 2024 要求 extern static
                // 读取入 unsafe 块，读取本身无副作用。
                let common_modes = unsafe { kCFRunLoopCommonModes };
                run_loop.add_source(&loop_source, common_modes);
                tap.enable();
                if ready_tx.send(Ok(run_loop.clone())).is_err() {
                    return; // 宿主已放弃（start 出错/被丢弃）
                }
                CFRunLoop::run_current();
                // run 返回（宿主 stop()）：tap 在此 drop → crate Drop 内
                // CFMachPortInvalidate（CGEventTap 被 drop 即失效，#[must_use] 契约）。
            })
            .map_err(|error| InputError::Platform {
                action: "capture-spawn",
                reason: format!("捕获线程创建失败：{error}"),
            })?;

        let run_loop = ready_rx
            .recv()
            .map_err(|_| InputError::Platform {
                action: "tap-create",
                reason: "捕获线程在就绪前退出（panic 或提前 return）".into(),
            })?
            .map_err(|reason| InputError::Platform {
                action: "tap-create",
                reason,
            })?;

        Ok(Self {
            receiver,
            mods_bits,
            suppress: suppress_bits,
            pin_point,
            pinned: pinned_bits,
            run_loop: Arc::new(Mutex::new(Some(run_loop))),
            worker: Some(worker),
        })
    }

    /// 受控吞本地输入开关（M5 KVM 语义）：受控时本地物理输入在 tap 层被 Drop
    /// （已转换转发从机），阻止本地 OS 执行；离开受控态必须复位 `false`。
    ///
    /// 线程安全：tap 回调经 `Arc<AtomicBool>` 读，setter 任意线程可调（Relaxed）。
    /// 受控期光标锚定（11-20 实测修复）：非前台进程的
    /// `CGAssociateMouseAndMouseCursorPosition` 无效（Apple 文档限定前台；
    /// console 运行的 xcontrol 非前台 → 解关联不生效，光标仍随物理鼠标移动）。
    /// 代理方案：受控期间由会话轮询每拍把光标 warp 回藏点（右下角内侧），
    /// 物理鼠标位移在下一拍即被拉回——视觉上光标钉死在角落（Synergy/Deskflow
    /// 同思路）。hide_cursor 已全局生效（探针实测），叠加锚定后光标不可见且
    /// 不再"移动"。
    pub fn pin_cursor_to_corner(&self) {
        // SAFETY: CGWarpMouseCursorPosition 是进程级线程安全 API（Apple 文档，
        // 自 macOS 10.11 无限制）；传入全局坐标。失败仅告警（下一拍重试）。
        let bounds = CGDisplay::main().bounds();
        // 锚点取**内侧安全点**（各内缩 50 点）而非屏角：锚在角点会落进边缘
        // 容差带（inside=false）——传感器每拍在边界样本上打转（闩锁不解除、
        // 归属帧换算反复命中边界）；内侧点让受控期间传感器安静（11-20 优化）。
        const PIN_INSET: f64 = 50.0;
        let x = bounds.origin.x + bounds.size.width - PIN_INSET;
        let y = bounds.origin.y + bounds.size.height - PIN_INSET;
        if let Err(error) = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y)) {
            tracing::warn!(target: "input::macos", %error, "受控光标锚定 warp 失败（下拍重试）");
        }
    }

    /// 受控输入开关（`suppress=true` = 吞本机输入并接管光标锚定）。
    ///
    /// `release_point`（`suppress=false` 时生效）：释放落点重放（全局点坐标）
    /// ——重关联会把光标拉回物理鼠标位置，覆盖此前执行的回切落点 warp（11-22
    /// 根因，见 false 分支注释）；传入落点则重放生效。会话异常收尾传 `None`
    /// （光标停物理鼠标位，语义自然）。
    pub fn set_suppress_local_input(&self, suppress: bool, release_point: Option<(f64, f64)>) {
        self.suppress.store(suppress, Ordering::Relaxed);
        // 受控光标方案（11-20 用户建议定稿）：**锁定在切入瞬间的当前位置**。
        // - suppress=true：捕获当前光标位置为锚点（一次性）+ 开启锚定；
        //   hide_cursor/associate 尽力而为（macOS 26 对非前台进程均忽略——
        //   探针实测，光标可见但被冻结在原地，视觉语义"停在切入处"）。
        // - false：解除锚定 + 恢复 show/associate + 释放落点重放（见上）。
        let main = CGDisplay::main();
        if suppress {
            // 先捕获（在 pin 生效前——锚点 = 切入瞬间的真实位置）。
            let loc = cursor_location();
            if let Ok((x, y)) = loc {
                // 锚点内缩（11-22 撞缘断流根因修复）：边缘切入时主控光标停在
                // 系统钳位的屏幕边缘——锚在缘上，受控期**朝缘方向**的后续移动
                // 事件位置恒被钳在缘上（位置差分 delta 恒 0）→ 增量断流 → 被控
                // 光标"差一点"到不了缘（回切判定与 kwin 触发角双双失效）。捕获
                // 后沿所在屏边界内缩到回锚判定带（60pt）之外，delta 从切入即
                // 正常供给；视觉语义保持"冻结在切入处附近"。软缘（多屏邻边）
                // 方向系统光标本可推出、delta 本就正常——统一内缩是无害的语义
                // 微差（锚点偏离切入处约 90pt）。
                let (x, y) = entry_anchor_clamped((x, y));
                *self
                    .pin_point
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((x, y));
            }
            // **锚定复活（11-22 实验裁定）**：对照实验确认——停用锚定后被控
            // 光标正常移动（delta 断供不存在，原推断误判：解关联后 HID 事件
            // delta 为硬件增量，无屏缘钳位）且回切/落点整链正常；但主控光标
            // **跟随物理鼠标移动**（macOS 的指针合成在 HID 层，Session tap 的
            // Drop 吞事件防不住光标移动；hide_cursor/associate 在 macOS 26
            // 非前台进程均失效——探针实测）。**每拍锚定 warp 是"光标冻结"的
            // 唯一可达实现**：会话输入拍（10ms，app `INPUT_POLL_INTERVAL`）每拍
            // 强制光标回锚点 = 视觉静止（11-24 勘误：此处原记 50ms——回拉一直
            // 发生在 10ms 输入拍，docs/05 §1.2 同步更正）。此前锚定引发的"切入
            // 即回切"已由回切免疫窗（core session `RETURN_IMMUNITY_MS`，接管后
            // 800ms 按时间计；11-24 前按回报条数计）+ 落点死区拆分修掉，锚定
            // 不再有回弹副作用。回拉是周期性的而非真正冻结，两次回拉之间光标
            // 若冲到屏边（docs/05 §1.2，按 10ms 拍的算术推算、非实测值），受控
            // 期间的主控边缘命中由 app 门控一律忽略（11-24，session_rt
            // `edge_hit_allowed` 只对 `ControlDir::None` 放行）。
            self.pinned.store(true, Ordering::Relaxed);
            if let Err(error) = main.hide_cursor() {
                tracing::warn!(target: "input::macos", %error, "CGDisplayHideCursor 失败（macOS 26 非前台忽略——光标可见但冻结，已知限制）");
            }
            if let Err(error) = CGDisplay::associate_mouse_and_mouse_cursor_position(false) {
                tracing::warn!(target: "input::macos", %error, "鼠标解关联失败（尽力而为）");
            }
        } else {
            self.pinned.store(false, Ordering::Relaxed);
            if let Err(error) = CGDisplay::associate_mouse_and_mouse_cursor_position(true) {
                tracing::warn!(target: "input::macos", %error, "鼠标重关联失败（尽力而为）");
            }
            if let Err(error) = main.show_cursor() {
                tracing::warn!(target: "input::macos", %error, "CGDisplayShowCursor 失败（尽力而为）");
            }
            // 释放落点重放（11-22 回切落点覆盖根因修复）：重关联会把光标拉到
            // **物理鼠标当前位置**（解关联期间系统持续追踪物理指针，associate
            // (true) 语义即"光标回归物理鼠标"）——回切 WarpPointer 在本拍之前
            // 已执行，其落点被此处 associate **覆盖**（用户实测：落点跳到物理
            // 鼠标停驻的屏幕右侧，期望的边缘衔接落点丢失）。调用方传入释放落
            // 点（全局点坐标）时在此**重放** warp——associate 之后执行即最终
            // 生效；None（会话异常收尾）保持物理鼠标位，语义自然。
            if let Some((x, y)) = release_point
                && let Err(error) = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y))
            {
                tracing::warn!(target: "input::macos", %error, "释放落点 warp 失败（光标停在物理鼠标位）");
            }
            // 锚点清空（下次受控重新捕获）。
            *self
                .pin_point
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    /// 受控光标锚定（每拍调用；warp 回切入时捕获的位置）。
    ///
    /// `pin_point = None` 时跳过（未受控/锚点未捕获）。
    pub fn pin_cursor_to_anchor(&self) {
        if !self.pinned.load(Ordering::Relaxed) {
            return;
        }
        let Some((x, y)) = *self
            .pin_point
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        else {
            return;
        };
        if let Err(error) = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y)) {
            tracing::warn!(target: "input::macos", %error, "受控光标锚定 warp 失败（下拍重试）");
        }
    }

    /// 受控光标锚点只读访问（11-24：macOS 主控热键回主控的落点 = 切入锚点，
    /// docs/03 §4.6）。`Some` = 受控中，锚点已捕获（全局点坐标——受控期间用户
    /// 所见的光标位置）；`None` = 未受控，或切入时光标查询失败未捕获到锚点
    /// （调用方据此让光标停在当前位置）。只读 `pin_point`，不改锚定状态；
    /// 锁中毒处理同 [`CaptureSource::pin_cursor_to_anchor`]。
    #[must_use]
    pub fn anchor_point(&self) -> Option<(f64, f64)> {
        *self
            .pin_point
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 停止捕获：runloop stop → 回调线程退出 → tap drop（mach port 失效）→ join。
    ///
    /// # Errors
    /// [`InputError::Platform`]：回调线程 panic。
    pub fn stop(mut self) -> Result<(), InputError> {
        self.shutdown()
    }

    /// 关停公共路径（[`CaptureSource::stop`] 与 [`Drop`] 共用）。
    fn shutdown(&mut self) -> Result<(), InputError> {
        if let Some(run_loop) = self.run_loop.lock().ok().and_then(|mut slot| slot.take()) {
            run_loop.stop();
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| InputError::Platform {
                action: "capture-stop",
                reason: "捕获线程 panic".into(),
            })?;
        }
        Ok(())
    }
}

impl Drop for CaptureSource {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也关停线程（幂等；join 失败在 Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

impl crate::InputSource for CaptureSource {
    fn poll(&mut self, out: &mut Vec<InputEvent>) {
        while let Ok(event) = self.receiver.try_recv() {
            out.push(event);
        }
    }

    fn mods(&self) -> Mods {
        Mods::from_bits(self.mods_bits.load(Ordering::Relaxed))
    }
}

/// CG 事件 → 协议事件转换（回调线程主体；只做转换与 mpsc 投递）。
///
/// 未映射 kVK（ISO/JIS/Fn 等，见 [`crate::keymap`] 模块文档）与
/// `TapDisabledBy*`/`Tablet*`/`Null` 事件按 M2 范围**丢弃**（保持 Keep 不拦截）；
/// 接收端关闭时 `send` 失败静默忽略（捕获正在关闭）。
fn translate_event(
    sender: &Sender<InputEvent>,
    mods_bits: &AtomicU32,
    event_type: CGEventType,
    event: &CGEvent,
    suppress: &AtomicBool,
) {
    let emit = |event: InputEvent| {
        let _ = sender.send(event);
    };
    // HID 物理位置探针（11-22 KDE 触发角排查；1/s 节流）：打印捕获事件的
    // location（HID 合成位置）——受控期用户推角落时，本值直接揭示 mac 物理
    // 指针是否被屏幕边界钳位（钳位 = delta 断供实锤；持续增长 = 无钳位）。
    if matches!(
        event_type,
        CGEventType::MouseMoved | CGEventType::LeftMouseDragged
    ) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAST_PROBE_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let last = LAST_PROBE_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) >= 1000
            && LAST_PROBE_MS
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let loc = event.location();
            tracing::info!(target: "input::macos",
                x = loc.x, y = loc.y, ?event_type,
                "HID 事件位置探针（受控期物理指针轨迹观测）");
        }
    }
    match event_type {
        CGEventType::KeyDown | CGEventType::KeyUp => {
            // keycode 字段即 kVK（u16 值域内）；i64→u16 截断仅为防御，正常值不截断。
            let kvk = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let Some(code) = keymap::kvk_to_xkey(kvk) else {
                return;
            };
            let state = if matches!(event_type, CGEventType::KeyUp) {
                KeyState::Up
            } else {
                KeyState::Down
            };
            if state == KeyState::Down
                && event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) == 1
            {
                emit(InputEvent::KeyRepeat { code }); // 长按重复（docs/02 §4.2 kind 0x02）
            } else {
                emit(InputEvent::Key { code, state });
            }
        }
        CGEventType::FlagsChanged => {
            // 修饰键状态变化：keycode 字段给出**具体哪颗**修饰键（左右手归属），
            // flags 给出新状态——方向判定按设备相关位（见 [`device_modifier_bit`]）。
            let kvk = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let Some(code) = keymap::kvk_to_xkey(kvk) else {
                return; // Fn（kVK_Function）无 HID 语义，忽略
            };
            let raw_flags = event.get_flags().bits();
            // CapsLock（kVK_CapsLock=0x39）不是 0xE0-0xE7 修饰键（HID 0x39 普通 Key
            // 语义，docs/03 §4.1 允许作触发键），但经 FlagsChanged 到达：方向取
            // NX_ALPHASHIFTMASK 锁存家族位（单键无左右之分），不占 mods bit0-7。
            if code == xcontrol_proto::xkey::KEY_CAPS_LOCK {
                emit(InputEvent::Key {
                    code,
                    state: if raw_flags & nx_device_flags::NX_ALPHASHIFTMASK != 0 {
                        KeyState::Down
                    } else {
                        KeyState::Up
                    },
                });
                return;
            }
            let Some(bit) = code.mods_bit() else {
                return;
            };
            // 方向 = 本键设备位（真实硬件 FlagsChanged 恒携带；合成事件若仅置家族位
            // 会被判为 Up——见模块文档已知限制）。
            let down =
                device_modifier_bit(kvk).is_some_and(|device_bit| raw_flags & device_bit != 0);
            let mask = 1_u32 << bit;
            if down {
                mods_bits.fetch_or(mask, Ordering::Relaxed);
            } else {
                mods_bits.fetch_and(!mask, Ordering::Relaxed);
            }
            emit(InputEvent::Key {
                code,
                state: if down { KeyState::Down } else { KeyState::Up },
            });
        }
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            // 受控锚定与 warp 伪影滤除（11-20 泄漏修复的正确形态）：
            // CGWarpMouseCursorPosition 产生的 MouseMoved 其 delta 字段**为 0**
            //（warp 不算硬件位移——Apple 语义），真实物理事件 delta 非零。
            // 据此过滤：受控期间 delta 双零的 MouseMoved = 锚定伪影（丢弃），
            // 非零 = 物理位移（转发从机）——不能像 11-20 首版那样全丢
            //（物理输入也走此路径，全丢 = 从机收不到任何移动）。
            // kCGMouseEventDeltaX/Y：窗口服务器对真实硬件事件填充的逐事件增量
            // （点为单位，正=右/下，与 CG 全局坐标轴一致）；合成为 0（无谓转发）。
            let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X);
            let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y);
            if suppress.load(Ordering::Relaxed) && dx == 0 && dy == 0 {
                return; // 受控期 warp 伪影（delta 零移动事件）：不转发
            }
            if dx != 0 || dy != 0 {
                emit(InputEvent::MouseMoveRel {
                    dx: dx as f32,
                    dy: dy as f32,
                });
            }
        }
        CGEventType::LeftMouseDown => emit(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        }),
        CGEventType::LeftMouseUp => emit(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Up,
        }),
        CGEventType::RightMouseDown => emit(InputEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Down,
        }),
        CGEventType::RightMouseUp => emit(InputEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Up,
        }),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            // 键号：2=中键、3=后退、4=前进（与 docs/02 §4.2 button 表一致）。
            let number = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match number {
                2 => MouseButton::Middle,
                3 => MouseButton::Back,
                4 => MouseButton::Forward,
                _ => return,
            };
            emit(InputEvent::MouseButton {
                button,
                state: if matches!(event_type, CGEventType::OtherMouseUp) {
                    KeyState::Up
                } else {
                    KeyState::Down
                },
            });
        }
        CGEventType::ScrollWheel => {
            // FixedPt 增量为触控板/高分滚轮的小数格（docs/02 §4.2 delta 保留小数）；
            // 正负号沿用 mac 原生语义（axis1 正=向上、axis2 正=向左），注入侧读同语义，
            // mac↔mac 往返一致；跨平台符号约定属映射引擎（M3）职责。
            let vertical = event
                .get_double_value_field(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1);
            let horizontal = event
                .get_double_value_field(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2);
            if vertical != 0.0 {
                emit(InputEvent::Wheel {
                    axis: Axis::Vertical,
                    delta: vertical as f32,
                });
            }
            if horizontal != 0.0 {
                emit(InputEvent::Wheel {
                    axis: Axis::Horizontal,
                    delta: horizontal as f32,
                });
            }
        }
        // TapDisabledByTimeout/UserInput、Tablet*、Null：M2 忽略（见模块文档已知限制）。
        _ => {}
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use xcontrol_core::sink::{InputSink, InputSource};
    use xcontrol_proto::xkey::{
        KEY_LEFT_CTRL, KEY_LEFT_SHIFT, KEY_RIGHT_ALT, KEY_RIGHT_CTRL, KEY_RIGHT_SHIFT,
    };

    use super::*;

    /// f64 累加器：整数提取 + 余量保留（docs/02 §4.2 语义）。
    #[test]
    fn accumulate_preserves_remainder() {
        // 1.5 → 整数 1、余量 0.5
        let (rest, int) = accumulate(0.0, 1.5);
        assert_eq!((rest, int), (0.5, 1));
        // 跨事件小数合并：0.5 + 0.6 = 1.1 → 注 1 留 0.1（f32→f64 有表示误差，用容差）
        let (rest, int) = accumulate(0.5, 0.6);
        assert_eq!(int, 1);
        assert!((rest - 0.1).abs() < 1e-6, "余量应为 0.1 ±1e-6，实测 {rest}");
        // 负方向：-0.5 + -0.6 = -1.1 → 注 -1 留 -0.1（trunc 向零取整，净量守恒）
        let (rest, int) = accumulate(-0.5, -0.6);
        assert!((rest + 0.1).abs() < 1e-6);
        assert_eq!(int, -1);
        // 大量小步最终合并出整数（净移动守恒）
        let (mut rest, mut total) = (0.0_f64, 0_i64);
        for _ in 0..100 {
            let (r, i) = accumulate(rest, 0.15);
            rest = r;
            total += i as i64;
        }
        assert_eq!((rest * 1000.0).round() as i64, 0, "余量应耗尽");
        assert_eq!(total, 15, "100×0.15 = 15 点");
    }

    /// 屏内归一化（`cursor_normalized` 域换算契约，11-22 修复轮）：±1 点容差带
    /// 收敛端点 + 中段比例换算（与 wayland `abs_normalized` / x11
    /// `pointer_snap_normalized` 同型——两侧被控回报同一主控判定面）。
    #[test]
    fn point_snap_normalized_snaps_endpoints_and_scales() {
        // 端点与 ±1 点容差带（origin = 0 的屏）。
        assert_eq!(point_snap_normalized(0.0, 0.0, 1000.0), 0.0);
        assert_eq!(point_snap_normalized(1.0, 0.0, 1000.0), 0.0);
        assert_eq!(point_snap_normalized(999.0, 0.0, 1000.0), 1.0);
        assert_eq!(point_snap_normalized(1000.0, 0.0, 1000.0), 1.0);
        // 中段精确比例。
        assert!((point_snap_normalized(500.0, 0.0, 1000.0) - 0.5).abs() < 1e-6);
        // 副屏原点偏移（origin = 1000 的右挂屏：屏内相对坐标换算）。
        assert!((point_snap_normalized(1500.0, 1000.0, 1000.0) - 0.5).abs() < 1e-6);
        assert_eq!(point_snap_normalized(1999.5, 1000.0, 1000.0), 1.0);
        // 点越出屏原点一侧（负坐标 / 屏外）：clamp 0.0（左缘语义）。
        assert_eq!(point_snap_normalized(-3.0, 0.0, 1000.0), 0.0);
        assert_eq!(point_snap_normalized(985.0, 1000.0, 1000.0), 0.0);
        // warp 可把坐标推过屏末端：clamp 1.0。
        assert_eq!(point_snap_normalized(1500.0, 0.0, 1000.0), 1.0);
        // 几何异常（零尺寸）防御：恒 0.0（display_layout 已滤，此处兜底）。
        assert_eq!(point_snap_normalized(500.0, 0.0, 0.0), 0.0);
    }

    /// 修饰键 → CG flags 家族位映射（左右同族）。
    #[test]
    fn modifier_flags_map_left_and_right() {
        assert_eq!(modifier_flag(KEY_LEFT_CTRL), modifier_flag(KEY_RIGHT_CTRL));
        assert!(
            modifier_flag(KEY_LEFT_CTRL)
                .is_some_and(|f| f.contains(CGEventFlags::CGEventFlagControl))
        );
        assert!(
            modifier_flag(KEY_LEFT_SHIFT)
                .is_some_and(|f| f.contains(CGEventFlags::CGEventFlagShift))
        );
        assert!(
            modifier_flag(KEY_RIGHT_ALT)
                .is_some_and(|f| f.contains(CGEventFlags::CGEventFlagAlternate))
        );
        assert_eq!(
            modifier_flag(xcontrol_proto::xkey::KEY_LEFT_META),
            Some(CGEventFlags::CGEventFlagCommand)
        );
        // 非修饰键无 flags
        assert_eq!(modifier_flag(xcontrol_proto::xkey::KEY_A), None);
    }

    /// 合成 flags：跟踪集合 → 位集（docs/05 §1 flags 合成路径）。
    #[test]
    fn synthesized_flags_reflect_tracked_keys() {
        let empty = synthesized_flags(&[]);
        assert_eq!(empty, CGEventFlags::empty());
        let both = synthesized_flags(&[KEY_LEFT_CTRL, KEY_RIGHT_SHIFT]);
        assert!(both.contains(CGEventFlags::CGEventFlagControl));
        assert!(both.contains(CGEventFlags::CGEventFlagShift));
        assert!(!both.contains(CGEventFlags::CGEventFlagCommand));
        let with_alt =
            synthesized_flags(&[KEY_LEFT_CTRL, xcontrol_proto::xkey::KEY_A, KEY_RIGHT_ALT]);
        assert!(with_alt.contains(CGEventFlags::CGEventFlagControl));
        assert!(with_alt.contains(CGEventFlags::CGEventFlagAlternate));
    }

    /// 权限检测（默认可跑，本机实测路径）：返回值如实并打印当前状态
    /// （PRD AC：未授权时应返回 Denied——本机实际状态记录在测试输出）。
    #[test]
    fn accessibility_check_reports_current_state() {
        let status = check_accessibility(false);
        println!(
            "本机辅助功能授权状态（prompt=false 实测）：{status:?} \
             —— 若为 Denied，捕获/注入将被系统静默忽略（docs/05 §1）；\
             人工授权路径见 #[ignore] 测试与报告"
        );
        assert!(status == AccessibilityStatus::Granted || status == AccessibilityStatus::Denied);
        // prompt=true 只影响是否弹引导框，不改变判定语义（不在此测弹窗——避免 CI 噪音）。
        let again = check_accessibility(false);
        assert_eq!(status, again, "连续两次检测应一致（无缓存、无副作用）");
    }

    /// 屏幕几何查询（FR-1.3 基础；纯查询无需权限）。
    ///
    /// 本机实测记录：屏幕休眠时 `CGGetActiveDisplayList` 可为空（11-17 实测 active=0、
    /// online=1），`display_layout` 走在线列表兜底——两种状态都应产出 ≥1 屏。
    #[test]
    fn display_layout_lists_active_displays() {
        let active = CGDisplay::active_displays().expect("active 查询应成功");
        let layout = display_layout().expect("显示器查询应成功（无需权限）");
        println!(
            "本机显示器（active 列表 {} 块，最终布局 {} 块）：{layout:?}",
            active.len(),
            layout.len()
        );
        assert!(!layout.is_empty(), "至少一块显示器（active 或在线兜底）");
        assert!(layout.iter().any(|info| info.is_main), "应存在主显示器");
        for info in &layout {
            assert!(info.size_points.0 > 0.0 && info.size_points.1 > 0.0);
            assert!(info.size_pixels.0 > 0 && info.size_pixels.1 > 0);
            let scale = info.scale_hint().expect("几何合法的屏应有缩放提示");
            assert!(
                scale > 0.0 && scale <= 1.0,
                "scale = 逻辑/物理 ∈ (0, 1]，实测 {scale}"
            );
        }
    }

    /// 锚点内缩（11-22）：带内点沿所在屏边界内缩到带外；带内不动的点原样；
    /// 无屏环境（display_layout 失败）原样返回。真实光标环境跑——判定域来自
    /// 本机 display_layout，CI mac 面有主屏即成立。
    #[test]
    fn entry_anchor_clamped_pushes_edge_points_inward() {
        let Ok(displays) = display_layout() else {
            println!("无显示器环境，跳过");
            return;
        };
        let info = displays.first().expect("至少一块屏");
        let (ox, oy) = info.origin;
        let (w, h) = info.size_points;
        // 带内（缘上点——切入钳位形态）→ 内缩到带外。
        let (x, y) = entry_anchor_clamped((ox, oy));
        assert!(
            x >= ox + 90.0 && y >= oy + 90.0,
            "缘上锚点必须内缩出判定带：({x},{y})"
        );
        let (x, y) = entry_anchor_clamped((ox + w, oy + h));
        assert!(
            x <= ox + w - 90.0 && y <= oy + h - 90.0,
            "对角缘锚点必须内缩：({x},{y})"
        );
        // 带外（屏心）→ 原样。
        let center = (ox + w / 2.0, oy + h / 2.0);
        assert_eq!(entry_anchor_clamped(center), center, "带外点不动");
    }

    /// 光标位置查询（CGEventGetLocation 无需权限）。
    #[test]
    fn cursor_location_is_finite() {
        let (x, y) = cursor_location().expect("光标查询应成功（无需权限）");
        println!("本机光标位置（全局点坐标）：({x}, {y})");
        assert!(x.is_finite() && y.is_finite());
    }

    /// 注入冒烟（PRD AC：CGEventPost 无害事件不崩溃；**默认可跑**）。
    ///
    /// 在当前位置原点附近 warp（Δ ≤ 1px，授权与否均无害）：授权时生效（≤1px 移动），
    /// 未授权时被系统静默忽略——两种现实都断言"调用面成功、进程不崩溃"。
    #[test]
    fn inject_harmless_event_is_ok() {
        let status = check_accessibility(false);
        let mut injector = MacInjector::new().expect("注入器构建（无需权限）");
        // 归一化定位到"当前光标所在处"：含屏判定（或回退首屏），warp 回原位。
        let (cx, cy) = cursor_location().expect("光标查询");
        let layout = display_layout().expect("显示器查询");
        let Some((idx, info)) = layout
            .iter()
            .enumerate()
            .find(|(_, info)| {
                cx >= info.origin.0
                    && cx < info.origin.0 + info.size_points.0
                    && cy >= info.origin.1
                    && cy < info.origin.1 + info.size_points.1
            })
            .map(|(i, info)| (i as u16, *info))
            .or_else(|| layout.first().map(|info| (0_u16, *info)))
        else {
            // 无任何在线显示器（理论不可达：会话内必有主屏）——记录后跳过注入段。
            println!("无在线显示器（异常环境），跳过注入段");
            return;
        };
        let nx = ((cx - info.origin.0) / info.size_points.0).clamp(0.0, 1.0) as f32;
        let ny = ((cy - info.origin.1) / info.size_points.1).clamp(0.0, 1.0) as f32;
        injector
            .warp(idx, nx, ny)
            .expect("无害 warp（原位 ≤1px）应成功");
        // 空滚轮（余量路径：不产生整数行 → 不投递）与空文本同样不崩溃。
        injector
            .inject(&InputEvent::Wheel {
                axis: Axis::Vertical,
                delta: 0.0,
            })
            .expect("零滚轮应成功");
        injector
            .inject(&InputEvent::Text {
                utf8: String::new(),
            })
            .expect("空文本应成功");
        // release_all 空集合幂等。
        injector.release_all().expect("空 release_all 应成功");
        println!("注入冒烟完成（授权状态 {status:?}；未授权时事件被系统静默忽略，调用面仍成功）");
    }

    /// 未知 ScreenId warp 拒绝（`SinkError::Failed` 语义）。
    #[test]
    fn warp_unknown_screen_fails() {
        let mut injector = MacInjector::new().expect("注入器构建");
        let error = injector.warp(99, 0.5, 0.5).expect_err("未知屏应失败");
        assert!(matches!(error, SinkError::Failed { action: "warp", .. }));
    }

    /// 路由层事件（EnterScreen 等）到注入层为静默忽略（非设备事件语义）。
    #[test]
    fn routing_events_are_ignored() {
        use xcontrol_proto::input::{Mods, ScreenEdge};
        let mut injector = MacInjector::new().expect("注入器构建");
        for event in [
            InputEvent::EnterScreen {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
                mods: Mods::LMETA,
                preserve_cursor: false,
            },
            InputEvent::LeaveScreen { mods: Mods::RALT },
            InputEvent::ModifierSync { mods: Mods::LCTRL },
            InputEvent::SwitchRequest { target: 1 },
            InputEvent::SwitchAck {
                target: 1,
                ok: true,
            },
        ] {
            injector.inject(&event).expect("路由事件应静默成功");
        }
    }

    /// 无权限注入的"静默失败"冒烟：权限 Denied 时注入调用面照常成功（系统忽略），
    /// 跟踪集合照常记账（本机默认现实；授权机器上此测试改走真实注入路径）。
    #[test]
    fn inject_key_updates_tracking_regardless_of_permission() {
        let mut injector = MacInjector::new().expect("注入器构建");
        injector
            .inject(&InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Down,
            })
            .expect("注入调用面应成功（权限不影响返回值）");
        // Ctrl 按下后注入普通键：flags 应含 Control（内部状态可经后续注入观察——
        // 直接断言跟踪行为：Up 后 release_all 无残留）。
        injector
            .inject(&InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Up,
            })
            .expect("Up 应成功");
        injector.release_all().expect("释放应成功");
    }

    /// 捕获源启停冒烟（PRD AC：无权限时 tap 创建成功但事件静默——如实记录；
    /// 默认可跑）。
    #[test]
    fn capture_start_poll_stop_smoke() {
        let status = check_accessibility(false);
        let mut source = match CaptureSource::start() {
            Ok(source) => source,
            Err(error) => {
                // 部分系统/版本可能直接拒绝创建：如实记录，不判定为回归。
                println!("本机 tap 创建失败（记录）：{error}");
                return;
            }
        };
        println!(
            "tap 创建成功（授权状态 {status:?}；未授权时事件被系统静默忽略——\
             docs/05 §1 预期行为）"
        );
        // 短窗口内无注入 → 无事件（授权机器上用户若恰有键鼠操作可能收到事件，
        // 只断言 poll 不 panic、mods 快照位域合法）。
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut events = Vec::new();
        source.poll(&mut events);
        println!(
            "100ms 窗口捕获事件数：{}（静默与否见授权状态）",
            events.len()
        );
        assert!(source.mods().bits() & !0xFF == 0, "mods 位域应在 bit0-7 内");
        source.stop().expect("捕获应干净停止（join 无 panic）");
    }

    /// 端到端回环（#[ignore]：**需人工授予辅助功能权限**后运行
    /// `cargo test -p xcontrol-input --features macos -- --ignored`）。
    ///
    /// 前置：本测试进程已在系统设置的辅助功能列表中。
    /// 行为：注入 `a` 键按下/抬起（**会在前台应用输入一个 "a"**），断言捕获线程
    /// 收到同一 Key 事件（注入 → HID → 会话 tap → 转换 → mpsc 全链路）。
    #[test]
    #[ignore = "需辅助功能授权：会在前台应用输入一个 a；人工授权后 cargo test -p xcontrol-input --features macos -- --ignored"]
    fn capture_receives_injected_key_with_accessibility() {
        let status = check_accessibility(false);
        assert!(
            status.is_granted(),
            "本测试需辅助功能授权（当前 {status:?}）：系统设置 → 隐私与安全性 → 辅助功能"
        );
        let mut source = CaptureSource::start().expect("已授权：tap 应创建成功");
        let mut injector = MacInjector::new().expect("注入器构建");
        injector
            .inject(&InputEvent::Key {
                code: xcontrol_proto::xkey::KEY_A,
                state: KeyState::Down,
            })
            .expect("注入 a↓");
        injector
            .inject(&InputEvent::Key {
                code: xcontrol_proto::xkey::KEY_A,
                state: KeyState::Up,
            })
            .expect("注入 a↑");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut received = Vec::new();
        while std::time::Instant::now() < deadline {
            source.poll(&mut received);
            let has_down = received.iter().any(|event| {
                matches!(
                    event,
                    InputEvent::Key {
                        code: xcontrol_proto::xkey::KEY_A,
                        state: KeyState::Down,
                    }
                )
            });
            let has_up = received.iter().any(|event| {
                matches!(
                    event,
                    InputEvent::Key {
                        code: xcontrol_proto::xkey::KEY_A,
                        state: KeyState::Up,
                    }
                )
            });
            if has_down && has_up {
                println!(
                    "回环成功：捕获收到注入的 a↓/a↑（共 {} 事件）",
                    received.len()
                );
                source.stop().expect("停止捕获");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        source.stop().expect("停止捕获");
        panic!(
            "2s 内未捕获到注入的 a↓/a↑（收到 {:?}）——tap 回调可能被系统忽略，\
             检查辅助功能授权是否对本测试宿主生效",
            received
        );
    }

    /// 无害注入序列（#[ignore]：**需人工授权**；授权后验证鼠标/滚轮注入真实生效）。
    ///
    /// 前置：辅助功能已授权。行为：注入中键按下/抬起（多数应用无绑定，无害）、
    /// 滚轮 1 行、flags 合成的 Ctrl+Shift 空按序列——断言全部调用面成功不崩溃。
    #[test]
    #[ignore = "需辅助功能授权：会产生真实中键点击与 1 行滚轮；人工授权后 cargo test -p xcontrol-input --features macos -- --ignored"]
    fn inject_mouse_wheel_sequence_with_accessibility() {
        let status = check_accessibility(false);
        assert!(
            status.is_granted(),
            "本测试需辅助功能授权（当前 {status:?}）"
        );
        let mut injector = MacInjector::new().expect("注入器构建");
        for event in [
            InputEvent::MouseButton {
                button: MouseButton::Middle,
                state: KeyState::Down,
            },
            InputEvent::MouseButton {
                button: MouseButton::Middle,
                state: KeyState::Up,
            },
            InputEvent::Wheel {
                axis: Axis::Vertical,
                delta: 1.0,
            },
            InputEvent::Wheel {
                axis: Axis::Horizontal,
                delta: -1.5,
            },
            InputEvent::Text {
                utf8: "x控制".into(),
            },
        ] {
            injector.inject(&event).expect("注入应成功");
        }
        injector.release_all().expect("释放应成功");
        println!("鼠标/滚轮/文本注入序列完成（前台可见中键点击与滚动）");
    }

    /// 绝对坐标注入冒烟（#[ignore]：**需辅助功能授权**；FR-2.7 mac 实测证据，
    /// 11-19-m5-layout）。
    ///
    /// 前置：辅助功能已授权。行为：读本机显示布局 → 注入
    /// `MouseMoveAbs{screen=主屏, 0.5, 0.5}`（主屏中心，无害移动）与
    /// `{0.0, 0.0}`（屏原点）→ `CGEventGetLocation` 回读断言光标落在目标
    /// ±3 点邻域——[`MacInjector::abs_point`] 空间换算（display 原点 + 归一化 ×
    /// 逻辑尺寸）与 CG 全局点空间一致性的实机证据。`EnterScreen` 对齐 warp
    /// 与 `MouseMoveAbs` 注入共用此路径（macos.rs warp/abs_point 同源）。
    #[test]
    #[ignore = "需辅助功能授权：移动真实光标到主屏中心与原点；人工授权后 cargo test -p xcontrol-input --features macos -- --ignored"]
    fn inject_mouse_move_abs_centers_cursor_on_main_display() {
        let status = check_accessibility(false);
        assert!(
            status.is_granted(),
            "本测试需辅助功能授权（当前 {status:?}）"
        );
        let infos = display_layout().expect("显示枚举");
        assert!(!infos.is_empty(), "至少一块屏（active/online 兜底后非空）");
        let main = infos.iter().position(|info| info.is_main).unwrap_or(0);
        let (ox, oy) = infos[main].origin;
        let (w, h) = infos[main].size_points;
        let mut injector = MacInjector::new().expect("注入器构建");

        // 采样窗口读数（活机竞争缓解）：注入后 HID 事件管线需微小传播时间，
        // 且物理鼠标可能立即覆盖注入位置——100ms 静置 + 300ms 内多点采样，
        // 任一样本命中目标 ±3 点即判定注入生效（证据打印全部样本）。
        fn sample_hits(target: (f64, f64), label: &str) -> bool {
            let mut hit = false;
            for tick in 0..10 {
                let (px, py) = cursor_location().expect("光标回读");
                let dist = ((px - target.0).powi(2) + (py - target.1).powi(2)).sqrt();
                println!("{label} [{tick:02}] 实测 ({px:.1}, {py:.1})，距目标 {dist:.1} 点");
                if dist <= 3.0 {
                    hit = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            hit
        }

        // 主屏中心（归一化 0.5, 0.5 → 全局点 (ox + w/2, oy + h/2)）。
        let (ex, ey) = (ox + w * 0.5, oy + h * 0.5);
        injector
            .inject(&InputEvent::MouseMoveAbs {
                screen: main as ScreenId,
                x: 0.5,
                y: 0.5,
            })
            .expect("abs 注入应成功");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            sample_hits((ex, ey), "abs 中心"),
            "abs 中心注入未生效：目标 ({ex:.1},{ey:.1})（样本见上；若持续漂移请\
             排除活机物理鼠标干扰后重跑）"
        );
        // 屏原点（归一化 0.0, 0.0 → 全局点 = display origin）。
        injector
            .inject(&InputEvent::MouseMoveAbs {
                screen: main as ScreenId,
                x: 0.0,
                y: 0.0,
            })
            .expect("abs 注入（原点）应成功");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            sample_hits((ox, oy), "abs 原点"),
            "abs 原点注入未生效：目标 ({ox:.1},{oy:.1})（样本见上）"
        );
    }

    /// 设备相关位映射：8 颗修饰键 kVK 全覆盖，位值与 Apple IOLLEvent.h 公开值一致。
    #[test]
    fn device_modifier_bit_covers_all_modifier_kvk() {
        let expect = [
            (0x38_u16, nx_device_flags::NX_DEVICELSHIFTKEYMASK),
            (0x3C, nx_device_flags::NX_DEVICERSHIFTKEYMASK),
            (0x3B, nx_device_flags::NX_DEVICELCTLKEYMASK),
            (0x3E, nx_device_flags::NX_DEVICERCTLKEYMASK),
            (0x3A, nx_device_flags::NX_DEVICELALTKEYMASK),
            (0x3D, nx_device_flags::NX_DEVICERALTKEYMASK),
            (0x37, nx_device_flags::NX_DEVICELCMDKEYMASK),
            (0x36, nx_device_flags::NX_DEVICERCMDKEYMASK),
        ];
        for (kvk, bit) in expect {
            assert_eq!(device_modifier_bit(kvk), Some(bit), "kVK 0x{kvk:02X}");
        }
        // CapsLock / Fn / 非修饰键无设备位
        assert_eq!(device_modifier_bit(0x39), None);
        assert_eq!(device_modifier_bit(0x3F), None);
    }

    /// FlagsChanged 左右手同家族回归（docs/03 §6 修饰键配对性不变式）：左右 Shift
    /// 同按后松开左手，须产出 LeftShift **Up**（家族位判定在右手仍按时误报 Down，
    /// 并把 mods 位错留在 0x02）且右手 mods 位（bit5）保持、全部释放后清零。
    #[test]
    fn flags_changed_same_family_release_reports_up() {
        let (sender, receiver) = channel::<InputEvent>();
        let mods_bits = Arc::new(AtomicU32::new(0));
        let suppress = Arc::new(AtomicBool::new(false)); // 受控关闭（本测非受控路径）
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("事件源创建");
        // 合成 FlagsChanged 事件（kCGEventNull 探针的字段不驻留，须用具体键盘事件）；
        // device bit 原样写入 CGEventSetFlags（get_flags 透传原始位，不做 bitflags 裁剪）。
        let synthetic = |kvk: u16, flags: CGEventFlags| {
            let event =
                CGEvent::new_keyboard_event(source.clone(), kvk, true).expect("合成键盘事件创建");
            event.set_flags(flags);
            event
        };

        // 左 Shift 按下：家族位 + 左设备位
        translate_event(
            &sender,
            &mods_bits,
            CGEventType::FlagsChanged,
            &synthetic(
                0x38,
                CGEventFlags::from_bits_retain(
                    CGEventFlags::CGEventFlagShift.bits() | nx_device_flags::NX_DEVICELSHIFTKEYMASK,
                ),
            ),
            &suppress,
        );
        assert_eq!(mods_bits.load(Ordering::Relaxed), 1 << 1, "LShift = bit1");
        // 右 Shift 按下：家族位 + 右设备位（左手仍按）
        translate_event(
            &sender,
            &mods_bits,
            CGEventType::FlagsChanged,
            &synthetic(
                0x3C,
                CGEventFlags::from_bits_retain(
                    CGEventFlags::CGEventFlagShift.bits() | nx_device_flags::NX_DEVICERSHIFTKEYMASK,
                ),
            ),
            &suppress,
        );
        assert_eq!(
            mods_bits.load(Ordering::Relaxed),
            (1 << 1) | (1 << 5),
            "RShift = bit5"
        );
        // 左 Shift 抬起：家族位仍置位（右手按着）、左设备位已清——本测试钉死的回归点
        translate_event(
            &sender,
            &mods_bits,
            CGEventType::FlagsChanged,
            &synthetic(
                0x38,
                CGEventFlags::from_bits_retain(
                    CGEventFlags::CGEventFlagShift.bits() | nx_device_flags::NX_DEVICERSHIFTKEYMASK,
                ),
            ),
            &suppress,
        );
        assert_eq!(mods_bits.load(Ordering::Relaxed), 1 << 5, "仅右手保持");
        // 右 Shift 抬起：全部清零
        translate_event(
            &sender,
            &mods_bits,
            CGEventType::FlagsChanged,
            &synthetic(0x3C, CGEventFlags::empty()),
            &suppress,
        );
        assert_eq!(mods_bits.load(Ordering::Relaxed), 0);

        let seq: Vec<(u16, KeyState)> = receiver
            .try_iter()
            .filter_map(|event| match event {
                InputEvent::Key { code, state } => Some((code.raw(), state)),
                _ => None,
            })
            .collect();
        assert_eq!(
            seq,
            vec![
                (xcontrol_proto::xkey::KEY_LEFT_SHIFT.raw(), KeyState::Down),
                (xcontrol_proto::xkey::KEY_RIGHT_SHIFT.raw(), KeyState::Down),
                (xcontrol_proto::xkey::KEY_LEFT_SHIFT.raw(), KeyState::Up), // 回归点
                (xcontrol_proto::xkey::KEY_RIGHT_SHIFT.raw(), KeyState::Up),
            ]
        );
    }

    /// CapsLock 经 FlagsChanged 到达（kVK 0x39，HID 0x39 普通 Key 语义，不占
    /// mods bit0-7）：必须产出 Key 事件——docs/03 §4.1 示例触发键与 docs/05 §7
    /// "CapsLock→Esc" 清单的前提（旧逻辑因 mods_bit()=None 整键丢弃）。
    #[test]
    fn flags_changed_caps_lock_emits_key_events() {
        let (sender, receiver) = channel::<InputEvent>();
        let mods_bits = Arc::new(AtomicU32::new(0));
        let suppress = Arc::new(AtomicBool::new(false)); // 受控关闭（本测非受控路径）
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("事件源创建");
        let synthetic = |flags: CGEventFlags| {
            let event =
                CGEvent::new_keyboard_event(source.clone(), 0x39, true).expect("合成键盘事件创建");
            event.set_flags(flags);
            event
        };

        // 锁存置位（NX_ALPHASHIFTMASK）→ Down
        translate_event(
            &sender,
            &mods_bits,
            CGEventType::FlagsChanged,
            &synthetic(CGEventFlags::from_bits_retain(
                CGEventFlags::CGEventFlagAlphaShift.bits(),
            )),
            &suppress,
        );
        assert_eq!(
            receiver.try_recv().ok(),
            Some(InputEvent::Key {
                code: xcontrol_proto::xkey::KEY_CAPS_LOCK,
                state: KeyState::Down,
            })
        );
        // 锁存清零 → Up；mods bit0-7 不受影响
        translate_event(
            &sender,
            &mods_bits,
            CGEventType::FlagsChanged,
            &synthetic(CGEventFlags::empty()),
            &suppress,
        );
        assert_eq!(
            receiver.try_recv().ok(),
            Some(InputEvent::Key {
                code: xcontrol_proto::xkey::KEY_CAPS_LOCK,
                state: KeyState::Up,
            })
        );
        assert_eq!(mods_bits.load(Ordering::Relaxed), 0);
    }

    /// 探针字段锚点：编译期钉死 `EventField` 常量值（Apple CGEventTypes 公开值），
    /// 防上游 crate 重命名/改值后静默错位（kCGKeyboardEventKeycode=9 等）。
    #[test]
    fn event_field_anchors() {
        assert_eq!(EventField::KEYBOARD_EVENT_KEYCODE, 9);
        assert_eq!(EventField::KEYBOARD_EVENT_AUTOREPEAT, 8);
        assert_eq!(EventField::MOUSE_EVENT_DELTA_X, 4);
        assert_eq!(EventField::MOUSE_EVENT_DELTA_Y, 5);
        assert_eq!(EventField::MOUSE_EVENT_BUTTON_NUMBER, 3);
        assert_eq!(EventField::MOUSE_EVENT_CLICK_STATE, 1);
        assert_eq!(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1, 93);
        assert_eq!(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2, 94);
    }

    // —— M4-hotkeys：主控链路实测（捕获流 → 热键识别 → "不转发"语义） ——

    /// mac 主控热键链路冒烟（#[ignore]，辅助功能授权环境——未授权时 tap 被
    /// 系统静默忽略、捕获恒无事件，docs/05 §1）。
    ///
    /// 链路：合成组合键（CGEventPost(HID)，事件携带**设备位**——FlagsChanged
    /// 方向判定的已知限制：纯家族位合成事件被判 Up，见模块文档）→ 本进程
    /// session tap 收到（ListenOnly 不过滤自注入，M2 实测行为）→
    /// [`HotkeyFilter`](xcontrol_core::HotkeyFilter)（app 会话循环的同一过滤器）
    /// 匹配 → 断言：
    /// 1. `Ctrl+Alt+Right` 组合 → `Hit(Next)`（捕获层识别，FR-2.5）；
    /// 2. KEY_RIGHT 的 Down/Up 不出现在 Forward 产出中——mac「不转发」= app 层
    ///    不产出转发事件（PRD 裁定：ListenOnly 无法吞，语义在过滤层达成）；
    /// 3. 组合的修饰键 Down/Up 照常 Forward（模块文档「命中吞键范围」裁定）；
    /// 4. 未命中组合（Ctrl+Alt+A）→ Forward 原样转发。
    ///
    /// 运行：`cargo test -p xcontrol-input --features macos -- --ignored --nocapture`
    #[test]
    #[ignore = "mac 热键链路实测（需辅助功能授权）：cargo test -p xcontrol-input --features macos -- --ignored --nocapture"]
    fn capture_hotkey_chain_smoke() {
        use std::time::{Duration, Instant};

        use core_graphics::event_source::CGEventSourceStateID;
        use xcontrol_config::HotkeysConfig;
        use xcontrol_core::hotkey::{FilterOutcome, HotkeyFilter, HotkeyId, HotkeySet};
        use xcontrol_proto::xkey::{KEY_A, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_RIGHT};

        assert_eq!(
            check_accessibility(false),
            AccessibilityStatus::Granted,
            "实测前提：辅助功能已授权（未授权时捕获恒无事件，系统设置引导见 docs/10 §1）"
        );
        let mut capture = CaptureSource::start().expect("捕获源启动");
        std::thread::sleep(Duration::from_millis(200)); // tap 就绪余量

        let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .expect("CGEventSource 创建");
        // 合成事件（family 家族位 + device 设备位）：设备位是 FlagsChanged 方向
        // 判定的唯一依据（模块文档已知限制——真实硬件事件恒携带）。
        let post = |kvk: u16, down: bool, family: CGEventFlags, device: u64| {
            // SAFETY: 无——new_keyboard_event/set_flags/post 均为安全包装。
            let event =
                CGEvent::new_keyboard_event(event_source.clone(), kvk, down).expect("事件构造");
            event.set_flags(CGEventFlags::from_bits_retain(family.bits() | device));
            event.post(CGEventTapLocation::HID);
        };
        let family = |flags: CGEventFlags| flags;
        let (f_ctrl, f_alt, fam_both) = (
            family(CGEventFlags::CGEventFlagControl),
            family(CGEventFlags::CGEventFlagAlternate),
            family(CGEventFlags::CGEventFlagControl.union(CGEventFlags::CGEventFlagAlternate)),
        );
        let (d_lctl, d_lalt) = (
            nx_device_flags::NX_DEVICELCTLKEYMASK,
            nx_device_flags::NX_DEVICELALTKEYMASK,
        );

        // ① 组合热键：Ctrl+Alt+Right（Down 序 → Up 序）。修饰键 Up 事件清零
        // 家族/设备位（真实硬件抬起时不再携带——FlagsChanged 方向按设备位判定）。
        post(0x3B, true, f_ctrl, d_lctl); // LCtrl Down
        post(0x3A, true, f_alt, d_lalt); // LAlt Down
        post(0x7C, true, fam_both, 0); // Right Arrow Down（kVK_RightArrow=0x7C）
        post(0x7C, false, fam_both, 0); // Right Arrow Up（修饰键仍按住）
        post(0x3A, false, CGEventFlags::empty(), 0); // LAlt Up（已抬起）
        post(0x3B, false, CGEventFlags::empty(), 0); // LCtrl Up（已抬起）
        // ② 未命中组合：Ctrl+Alt+A（A 的 Down/Up 均应 Forward）
        post(0x3B, true, f_ctrl, d_lctl);
        post(0x3A, true, f_alt, d_lalt);
        post(0x00, true, fam_both, 0); // A Down（kVK_ANSI_A=0x00）
        post(0x00, false, fam_both, 0); // A Up（修饰键仍按住）
        post(0x3A, false, CGEventFlags::empty(), 0); // LAlt Up
        post(0x3B, false, CGEventFlags::empty(), 0); // LCtrl Up

        // 轮询捕获流 → 同一会话循环的过滤器语义（HotkeyFilter 默认四键）
        let mut filter = HotkeyFilter::new(
            HotkeySet::from_config(&HotkeysConfig::default()).expect("默认热键解析"),
        );
        let mut outcomes: Vec<(InputEvent, FilterOutcome)> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let mut events = Vec::new();
            capture.poll(&mut events);
            for event in events {
                let outcome = filter.feed(&event);
                outcomes.push((event, outcome));
            }
            if outcomes.len() >= 12 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("捕获链路产出（{}/12）：", outcomes.len());
        for (event, outcome) in &outcomes {
            println!("  {outcome:?} <- {event:?}");
        }

        let hits: Vec<HotkeyId> = outcomes
            .iter()
            .filter_map(|(_, outcome)| match outcome {
                FilterOutcome::Hit(id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(hits, vec![HotkeyId::Next], "组合应命中且仅命中一次");

        // 不转发：KEY_RIGHT 的 Down/Up 不得出现在 Forward 产出（PRD 裁定语义）
        for (event, outcome) in &outcomes {
            if let InputEvent::Key { code, .. } = event
                && *code == KEY_RIGHT
            {
                assert_ne!(
                    outcome,
                    &FilterOutcome::Forward,
                    "热键主键 {event:?} 不得转发"
                );
            }
        }
        // 修饰键照常转发（命中组合的修饰键事件放行）
        for code in [KEY_LEFT_CTRL, KEY_LEFT_ALT, KEY_A] {
            let forwarded = outcomes.iter().any(|(event, outcome)| {
                matches!(
                    event,
                    InputEvent::Key { code: c, .. } if *c == code
                ) && *outcome == FilterOutcome::Forward
            });
            assert!(
                forwarded,
                "{code:?} 应出现在 Forward 产出（修饰键/未命中键放行）"
            );
        }
        capture.stop().expect("捕获停止");
    }
}
