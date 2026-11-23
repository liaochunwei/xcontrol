//! Windows 剪贴板实现（docs/05 §2 剪贴板行）：**arboard 读写** + **隐藏消息窗口
//! `WM_CLIPBOARDUPDATE` 事件驱动监听**（FR-4.1 文本路径）。
//!
//! # API 分工（design.md + 11-19-m5-clipboard-full 扩展）
//!
//! - **读写**（文本，FR-4.1）：[`arboard`]——win32 路径内部经 clipboard-win
//!   （CF_UNICODETEXT）；空剪贴板/非文本 → `Error::ContentNotAvailable` →
//!   [`FormatRead::NoContent`] 常态（FR-4.1），不上升到错误。
//! - **图片读写**（FR-4.2，M5）：arboard `image-data` feature（RGBA 内存形态，
//!   win 内部经 clipboard-win CF_DIB）；wire PNG ↔ RGBA 转换走公共
//!   [`crate::png_codec`]（与 mac/x11 同一规范形判据——CF_DIB 转换后重编码，
//!   读回 PNG 字节 ≠ wire 写入字节、像素无损）。
//! - **HTML 读写**（FR-4.3，M5）：arboard `get().html()` / `set().html()`——
//!   win 内部经 clipboard-win `get_html`/CF_HTML 注册格式（写侧 arboard
//!   `wrap_html` 构造 `Version/StartHTML/…` 偏移头包住 fragment）。**写读
//!   round-trip 字节等价待实机复核**（docs/05 §2.2 item 6——clipboard-win
//!   `get_html` 的 StartFragment/EndFragment 提取行为须实测确认；若读回形态
//!   ≠ 写入 fragment，回声指纹取读回形态重新对齐，防线 1 语义不变）。
//! - **监听**（docs/04 §4）：`AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE`
//!   ——事件驱动（**非轮询**，docs/05 §2 明示），隐藏消息窗口接收（message-only
//!   窗口，`HWND_MESSAGE` 父级，不可见/无任务栏/Z 序副作用）。监听线程只做
//!   「`WM_CLIPBOARDUPDATE` → 序列号采样 → `std::sync::mpsc` 投递」，**剪贴板
//!   读写不在监听线程做**（回调线程纪律，platform-code.md）——消费方收到
//!   [`ClipChange`] 后自行调用 [`WinClipboard::read_text`]。
//!
//! # 被占用重试：3×33ms（docs/05 §2"被占用 3×33ms 重试（rustdesk 同款）"）
//!
//! Windows 剪贴板是全局互斥对象：任意时刻至多一个进程持有打开状态，其他进程的
//! `OpenClipboard` 立即失败（前台应用复制动作期间常用剪贴板查看器/杀毒/输入法
//! 持锁数毫秒）。本模块在 arboard 读写外层做 [`OPEN_RETRY_ATTEMPTS`] 次重试、
//! 间隔 [`OPEN_RETRY_INTERVAL`]（初试 + 3 次重试，每次失败后睡 33ms；
//! [`retry_occupied`]）。arboard 内部另有 5×5ms 的 OpenClipboard 重试
//! （arboard 3.7.0 platform/windows.rs `DEFAULT_OPEN_ATTEMPTS`，Chromium 同款）——
//! 两层叠加：arboard 内层吸收亚毫秒级竞争，本层 33ms×3 承接 docs/05 明示的
//! rustdesk 量级重试；重试仅针对 `Error::ClipboardOccupied`（其余错误立即返回）。
//!
//! # 换行与指纹接线（docs/04 §2 + .trellis/spec "Windows 指纹字节教训"）
//!
//! - 写：线格式 LF 文本 → [`crate::newline::normalize_to_lf`]（防御性收敛孤 CR）
//!   → [`crate::newline::lf_to_crlf`] → arboard 落板（Windows 惯例 CRLF）；
//!   指纹取**归一化 LF 文本**（不是落板 CRLF 字节）——回声读回经同一归一化后
//!   指纹逐字节一致（round-trip 不动点，newline.rs 模块文档），防线 1 判据可用；
//!   直接按落板 CRLF 字节计算会让读回指纹必然不同、自写回声被误判为外部变化。
//! - 读：arboard 取回（CRLF）→ [`crate::newline::normalize_to_lf`] → 线格式 LF
//!   → 上限判定（FR-4.5，按归一化后字节数 = 线格式字节口径）。
//!
//! # ClipboardWatch 原语（docs/04 §4；mac changeCount 对应物）
//!
//! `GetClipboardSequenceNumber`：全局 32 位序列号，剪贴板内容变化/清空即递增
//! （MS Learn《GetClipboardSequenceNumber》）。无需打开剪贴板、无需窗口，任意
//! 线程可读——[`WinClipboard::change_count`] 的实现原语，与事件流的
//! [`ClipChange::change_count`] 同源（同一全局计数器，事件只携带采样快照）。
//! u32 回绕（2^32 次变化后归零）在 trait"单调不减"契约下是接受的记录性偏差
//! （现实不可达；真回绕窗口内 `has_changes_since` 一次假阴性，防线 2 origin 序列
//! 号在协议层兜底）。
//!
//! # 线程模型
//!
//! [`ClipboardWatcher::start`] 创建专用线程：`RegisterClassW`（已注册则幂等复用，
//! `ERROR_CLASS_ALREADY_EXISTS` 放行）→ `CreateWindowExW`（message-only）→
//! `AddClipboardFormatListener` → 就绪握手（回传线程 id）→ `GetMessageW` 泵。
//! 停止 = 摘静态共享（迟到的通知直接丢弃）→ `PostThreadMessageW(WM_QUIT)` →
//! 泵退出 → `RemoveClipboardFormatListener` + `DestroyWindow` → join。
//! **DestroyWindow 必须在创建线程调用**（Win32 契约）——窗口销毁在泵线程收尾，
//! 宿主只投递 WM_QUIT。
//!
//! # 静态桥与单监听器约定
//!
//! WNDPROC 是裸 FFI 回调（不可捕获环境），经静态 `Mutex<Option<Shared>>` 桥接
//! （input 后端同款，`Sender: Send ⇒ Sync` 无 unsafe impl）。竞争面：通知每次
//! 事件加锁、宿主仅 start/stop 各碰一次——热路径恒无竞争。**单监听器约定**：
//! 同一进程并发启动两个 watcher 会覆盖静态槽（第二个写入者持有 sender，第一个
//! 的事件被投给后者）——上层会话层保证单实例（input 角色互斥同款约定）。
//!
//! # API 版本依据（design.md：以 windows crate 实际源码为准）
//!
//! 绑定：`windows 0.62`（微软官方 bindings，纯 Rust；workspace 登记见根
//! Cargo.toml）。全部签名逐项对 registry 源码 `windows-0.62.2/src/Windows/Win32/...`
//! 核实（2026-11-17）：
//!
//! - `AddClipboardFormatListener(HWND) -> windows_core::Result<()>` /
//!   `RemoveClipboardFormatListener(HWND) -> Result<()>` /
//!   `GetClipboardSequenceNumber() -> u32`——`Win32::System::DataExchange`
//!   （0.62 剪贴板 API 归属该模块）；
//! - `RegisterClassW(*const WNDCLASSW) -> u16`（0 = 失败，ATOM 非 Result）；
//!   `WNDCLASSW` 结构体整体 `#[cfg(feature = "Win32_Graphics_Gdi")]` 门控
//!   （`hbrBackground: HBRUSH` 字段类型归属 Gdi 模块——Cargo.toml feature 集因此
//!   含 Graphics_Gdi，本模块不调用 Gdi API）；
//! - `CreateWindowExW(WINDOW_EX_STYLE, P1: Param<PCWSTR>, P2, WINDOW_STYLE,
//!   x,y,w,h: i32, Option<HWND>, Option<HMENU>, Option<HINSTANCE>,
//!   Option<*const c_void>) -> windows_core::Result<HWND>`；
//! - `DestroyWindow(HWND) -> Result<()>`；`DefWindowProcW(HWND, u32, WPARAM,
//!   LPARAM) -> LRESULT`；`GetMessageW(*mut MSG, Option<HWND>, u32, u32) ->
//!   windows_core::BOOL`（0 = WM_QUIT、-1 = 错误）；`DispatchMessageW(*const MSG)
//!   -> LRESULT`；`TranslateMessage(*const MSG) -> BOOL`；
//! - `PostThreadMessageW(u32, u32, WPARAM, LPARAM) -> Result<()>`；
//!   `WM_CLIPBOARDUPDATE: u32 = 0x031D`（`Win32::UI::WindowsAndMessaging`）；
//!   `WNDPROC = Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM)
//!   -> LRESULT>`——真 unsafe FFI，本模块每个 unsafe 块附 `// SAFETY:` 不变量；
//! - `GetLastError() -> WIN32_ERROR`（Foundation）；宽字符串经
//!   `windows::core::w!`（windows-strings 经 windows-core 的 `#[cfg(windows)]`
//!   glob 重导出，编译期展开 UTF-16 字面量 → PCWSTR）。
//!
//! # 已知限制（M3 范围，报告/移交项）
//!
//! - **UIPI 与监听范围**：剪贴板通知不受 UIPI 消息过滤约束（消息过滤只拦低→高
//!   完整性方向；监听窗口通常普通完整性，提权进程写入是高→低方向，照常到达）。
//!   提权写入的实测项见 docs/05 §2.2 移交清单。
//! - **通知 ≠ 内容可读**：`WM_CLIPBOARDUPDATE` 到达时写入方可能尚未 `CloseClipboard`
//!   （通知在写入者持有打开状态期间投递）——读路径的 3×33ms 重试即为此设计
//!   （docs/05 §2 同款依据）；消费方收到事件后调用 `read_text` 走重试。
//! - **多格式读取**（M5 已实装，11-19-m5-clipboard-full）：文本（CF_UNICODETEXT）/
//!   图片（CF_DIB，arboard image-data）/HTML（CF_HTML，arboard clipboard-win）三
//!   格式读写路径已落（本模块 read/write_image_png、read/write_html）；CF_HTML
//!   读回提取与 CF_DIB 像素 round-trip 的实机复核仍属移交项（docs/05 §2.2 item 6）。
//! - **序列号回绕**：见 ClipboardWatch 一节（接受的记录性偏差）。
//! - **单监听器约定**：见"静态桥"一节（上层保证单实例；并发双 watcher 覆盖
//!   静态槽是文档化约定而非运行时错误）。

#![allow(unsafe_code)] // docs/08 §2：平台绑定层是唯一预期 unsafe 的位置；SAFETY 注释逐块强制。

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber, RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostThreadMessageW, RegisterClassW, TranslateMessage, WINDOW_STYLE, WM_CLIPBOARDUPDATE,
    WM_QUIT, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::error::ClipboardError;
use crate::{
    ClipFormatPayload, ClipboardContent, ClipboardWatch, FormatRead, WrittenMark,
    fingerprint_content, format_read_from,
};

// ============================================================
// 被占用重试（docs/05 §2"被占用 3×33ms 重试（rustdesk 同款）"）
// ============================================================

/// 被占用重试次数（初试之外；docs/05 §2）。
pub const OPEN_RETRY_ATTEMPTS: usize = 3;

/// 被占用重试间隔（rustdesk 同款 33ms；docs/05 §2）。
pub const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(33);

/// 以固定间隔重试被占用（`ClipboardOccupied`）的剪贴板操作：初试 + 至多
/// [`OPEN_RETRY_ATTEMPTS`] 次重试（每次失败后睡 `interval`）；其余错误立即返回。
///
/// `interval` 形参仅供测试注入（生产传 [`OPEN_RETRY_INTERVAL`]）；arboard 的
/// `Error` 是 `#[non_exhaustive]`，其余变体走通配分支原样上抛。
fn retry_occupied<T>(
    interval: Duration,
    mut op: impl FnMut() -> Result<T, arboard::Error>,
) -> Result<T, arboard::Error> {
    let mut attempt = 0usize;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(arboard::Error::ClipboardOccupied) if attempt < OPEN_RETRY_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(interval);
            }
            Err(error) => return Err(error),
        }
    }
}

// ============================================================
// 变化计数原语（GetClipboardSequenceNumber；ClipboardWatch 的 Windows 对应物）
// ============================================================

/// 当前剪贴板全局序列号（MS Learn《GetClipboardSequenceNumber》；内容变化/清空
/// 即递增，无需打开剪贴板、无需窗口、任意线程可读）。
///
/// 返回 u32（Win32 DWORD）；本模块以 u64 承载——2^32 次变化后的回绕是接受的
/// 记录性偏差（见模块文档 ClipboardWatch 一节）。
#[must_use]
pub fn clipboard_sequence_number() -> u64 {
    // SAFETY: GetClipboardSequenceNumber 无参数、无前置状态（MSDN：不需要剪贴板
    // 处于打开状态）；返回值 0 仅在无法确定时出现（未初始化窗口站），按 0 作
    // 基线语义处理（与 mac ns_change_count 失败兜底同型）。
    u64::from(unsafe { GetClipboardSequenceNumber() })
}

// ============================================================
// WinClipboard：读写（arboard + LF↔CRLF）+ ClipboardWatch trait 实现
// ============================================================

/// Windows 剪贴板句柄：arboard 读写 + 序列号监听原语（FR-4.1 文本路径）。
///
/// 换行语义（docs/04 §2）：写 `\n`→`\r\n`、读去 `\r`（Windows 行，转换见
/// [`crate::newline`]）；macOS/Linux 侧原生 LF 直通。
pub struct WinClipboard {
    clipboard: arboard::Clipboard,
}

impl WinClipboard {
    /// 初始化（arboard 内部建立 win32 会话；无系统授权要求，docs/05 §6）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：剪贴板会话创建失败。
    pub fn new() -> Result<Self, ClipboardError> {
        let clipboard = arboard::Clipboard::new().map_err(|error| ClipboardError::Platform {
            action: "clipboard-init",
            reason: error.to_string(),
        })?;
        Ok(Self { clipboard })
    }

    /// 写入线格式 LF 文本并返回回声抑制基线（docs/04 §5 防线 1/3 的写入自标记）。
    ///
    /// 落板前归一化 + CRLF 转换（模块文档"换行与指纹接线"一节）；被占用按
    /// [`OPEN_RETRY_ATTEMPTS`]×[`OPEN_RETRY_INTERVAL`] 重试。
    ///
    /// 消费方应把返回的 [`WrittenMark`] 交给 [`crate::EchoFilter::note_written`]：
    /// 写入后系统必然向监听窗口投递 `WM_CLIPBOARDUPDATE`（`AddClipboardFormatListener`
    /// 契约），基线缺失会把自身回声当外部变化外发（回环风暴的起点）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：落板失败（重试耗尽等）。
    pub fn write_text(&mut self, text: &str) -> Result<WrittenMark, ClipboardError> {
        // 指纹与落板字节的关系见模块文档：指纹 = 归一化 LF 文本（读回形态）。
        let normalized = crate::newline::normalize_to_lf(text);
        let stored = crate::newline::lf_to_crlf(&normalized);
        retry_occupied(OPEN_RETRY_INTERVAL, || {
            self.clipboard.set_text(stored.as_str())
        })
        .map_err(|error| ClipboardError::Platform {
            action: "clipboard-write",
            reason: error.to_string(),
        })?;
        // 写入后立即采样序列号基准（序列号在写入方 CloseClipboard 时递增；
        // arboard 写路径返回即已关闭，此采样 ≥ 落板时刻的计数）。
        let change_count = clipboard_sequence_number();
        Ok(WrittenMark {
            // 指纹统一走 fingerprint_content 规范形（M5 单一入口；text-only 的
            // 规范形 = normalize_to_lf 字节，与 M3 指纹语义一致）。
            fingerprint: fingerprint_content(&ClipboardContent {
                text: Some(normalized),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取文本并按上限归一化（FR-4.1 空剪贴板常态 + FR-4.5 超限信号）。
    ///
    /// 读回 CRLF 经 [`crate::newline::normalize_to_lf`] 收敛为线格式 LF；超过
    /// `max_bytes`（协商上限，FR-4.5 可配）→ [`FormatRead::Oversize`]（携带实际
    /// 长度，**不截断不外发**，提示由上层 UI 做）；被占用按重试策略重试。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：读取真实失败（`ContentNotAvailable` 已归一化
    /// 为 [`FormatRead::NoContent`]，不在此列）。
    pub fn read_text(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        let raw = match retry_occupied(OPEN_RETRY_INTERVAL, || self.clipboard.get_text()) {
            Ok(raw) => raw,
            // 空剪贴板 / 非文本（如图片）→ 常态（FR-4.1），归一化为 NoContent
            Err(arboard::Error::ContentNotAvailable) => return Ok(FormatRead::NoContent),
            Err(error) => {
                return Err(ClipboardError::Platform {
                    action: "clipboard-read",
                    reason: error.to_string(),
                });
            }
        };
        // 读侧归一化（docs/04 §2 Windows 行"读时去 \r"）；上限按归一化后字节
        // （= 线格式字节数，与 16MiB 协商口径一致）。
        let normalized = crate::newline::normalize_to_lf(&raw);
        Ok(format_read_from(
            Some(ClipFormatPayload::Text(normalized)),
            max_bytes,
        ))
    }

    /// 写入图片（FR-4.2）：wire PNG 字节 → 解码 RGBA → arboard `set().image()`
    /// （win 内部 CF_DIB 落板）并返回回声抑制基线。
    ///
    /// 读回路径经 CF_DIB → RGBA → 重编码 PNG（字节必然不同、像素无损——
    /// [`crate::png_codec`] 模块文档）；指纹取 RGBA 规范形。被占用按重试策略。
    ///
    /// # Errors
    /// [`ClipboardError::ImageFormat`]：PNG 解码失败；
    /// [`ClipboardError::Platform`]：落板失败（重试耗尽等）。
    pub fn write_image_png(&mut self, png: &[u8]) -> Result<WrittenMark, ClipboardError> {
        let (width, height, rgba) = crate::png_codec::decode_png_to_rgba(png)?;
        let image = arboard::ImageData {
            width: usize::try_from(width).map_err(|error| ClipboardError::ImageFormat {
                reason: format!("宽度 usize 转换失败：{error}"),
            })?,
            height: usize::try_from(height).map_err(|error| ClipboardError::ImageFormat {
                reason: format!("高度 usize 转换失败：{error}"),
            })?,
            bytes: std::borrow::Cow::Owned(rgba),
        };
        retry_occupied(OPEN_RETRY_INTERVAL, || {
            self.clipboard.set().image(image.clone())
        })
        .map_err(|error| ClipboardError::Platform {
            action: "clipboard-write-image",
            reason: error.to_string(),
        })?;
        let change_count = clipboard_sequence_number();
        Ok(WrittenMark {
            fingerprint: fingerprint_content(&ClipboardContent {
                image_png: Some(png.to_vec()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取图片（FR-4.2）：arboard `get().image()`（CF_DIB → RGBA）→ 编码 PNG
    /// 并按上限归一化。被占用按重试策略。
    ///
    /// # Errors
    /// [`ClipboardError::ImageFormat`]：RGBA → PNG 编码失败；
    /// [`ClipboardError::Platform`]：读取真实失败（`ContentNotAvailable` 已归一化
    /// 为 [`FormatRead::NoContent`]）。
    pub fn read_image_png(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        let image = match retry_occupied(OPEN_RETRY_INTERVAL, || self.clipboard.get().image()) {
            Ok(image) => image,
            Err(arboard::Error::ContentNotAvailable) => return Ok(FormatRead::NoContent),
            Err(error) => {
                return Err(ClipboardError::Platform {
                    action: "clipboard-read-image",
                    reason: error.to_string(),
                });
            }
        };
        let png = crate::png_codec::encode_rgba_to_png(
            &image.bytes,
            u32::try_from(image.width).map_err(|error| ClipboardError::ImageFormat {
                reason: format!("宽度 u32 转换失败：{error}"),
            })?,
            u32::try_from(image.height).map_err(|error| ClipboardError::ImageFormat {
                reason: format!("高度 u32 转换失败：{error}"),
            })?,
        )?;
        Ok(format_read_from(
            Some(ClipFormatPayload::ImagePng(png)),
            max_bytes,
        ))
    }

    /// 写入 HTML fragment（FR-4.3）：arboard `set().html()`（win 内部 clipboard-win
    /// CF_HTML 注册格式 + 偏移头包装——模块文档 round-trip 复核注记）并返回回声
    /// 抑制基线。被占用按重试策略。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：落板失败（重试耗尽等）。
    pub fn write_html(&mut self, html: &str) -> Result<WrittenMark, ClipboardError> {
        retry_occupied(OPEN_RETRY_INTERVAL, || {
            self.clipboard.set().html(html, None)
        })
        .map_err(|error| ClipboardError::Platform {
            action: "clipboard-write-html",
            reason: error.to_string(),
        })?;
        let change_count = clipboard_sequence_number();
        Ok(WrittenMark {
            fingerprint: fingerprint_content(&ClipboardContent {
                html: Some(html.to_owned()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取 HTML fragment（FR-4.3）：arboard `get().html()`（clipboard-win
    /// `get_html`，CF_HTML 偏移头解析）并按上限归一化。被占用按重试策略。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：读取真实失败（`ContentNotAvailable` 已归一化
    /// 为 [`FormatRead::NoContent`]）。
    pub fn read_html(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        let html = match retry_occupied(OPEN_RETRY_INTERVAL, || self.clipboard.get().html()) {
            Ok(html) => html,
            Err(arboard::Error::ContentNotAvailable) => return Ok(FormatRead::NoContent),
            Err(error) => {
                return Err(ClipboardError::Platform {
                    action: "clipboard-read-html",
                    reason: error.to_string(),
                });
            }
        };
        Ok(format_read_from(
            Some(ClipFormatPayload::Html(html)),
            max_bytes,
        ))
    }
}

impl ClipboardWatch for WinClipboard {
    /// 当前剪贴板全局序列号（docs/04 §4 监听原语的 Windows 对应物）。
    fn change_count(&self) -> u64 {
        clipboard_sequence_number()
    }

    /// `seen` 之后发生过写入（M1 mock 语义同款判定）。
    fn has_changes_since(&self, seen: u64) -> bool {
        clipboard_sequence_number() > seen
    }
}

// ============================================================
// ClipboardWatcher：WM_CLIPBOARDUPDATE 事件驱动监听线程（非轮询，docs/05 §2）
// ============================================================

/// 一次剪贴板变化事件（监听线程投递；docs/04 §4「变化方主动推送」触发源）。
///
/// 只带计数快照不带内容：剪贴板读写不在监听线程做（回调线程纪律，
/// platform-code.md），消费方凭计数走「读文本 → [`crate::EchoFilter`] 回声判定
/// → 上限判定 → 外发」流程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipChange {
    /// 通知到达时采样的 `GetClipboardSequenceNumber` 快照。
    pub change_count: u64,
}

/// WNDPROC 可访问的共享投递状态（静态槽内；`start` 置入、`shutdown` 摘除）。
///
/// 单槽约定见模块文档"静态桥与单监听器约定"（input 后端同款）。
struct ListenerShared {
    sender: Sender<ClipChange>,
}

/// 通知回调 ↔ 宿主的静态桥（`Mutex<Option<_>>`：Sync by construction，无需
/// unsafe impl；input 后端同款）。
static LISTENER_SHARED: Mutex<Option<ListenerShared>> = Mutex::new(None);

/// 以共享状态执行 `f`（无共享 = 监听已停止，通知丢弃）。
fn with_shared<T>(f: impl FnOnce(&ListenerShared) -> T) -> Option<T> {
    let guard = LISTENER_SHARED.lock().ok()?;
    guard.as_ref().map(f)
}

/// 隐藏窗口类名（进程级唯一；NUL 结尾由 `w!` 保证）。
const LISTENER_CLASS: PCWSTR = w!("xcontrol_clipboard_listener");

/// 监听窗口 WNDPROC（裸 FFI 回调：仅 `WM_CLIPBOARDUPDATE` → 序列号采样 → mpsc
/// 投递，其余全部走 `DefWindowProcW`）。
///
/// # SAFETY（系统约束，Rust 侧无从保证、只能立约）
///
/// - 由系统在**创建该窗口的线程**（泵线程）上调用（窗口过程绑定线程消息队列）；
///   本函数不重入系统剪贴板 API（不 Open/Close/读写），无死锁面；
/// - 回调内零剪贴板读写、零分配大对象、零 async（回调线程纪律，
///   platform-code.md）——仅 `GetClipboardSequenceNumber` 采样 + mpsc send；
/// - 参数 (hwnd, msg, wparam, lparam) 原样转交 `DefWindowProcW`（通知消息不吞）；
///   `hwnd` 有效性由系统保证（进程窗口存在期间），本函数不存储该句柄。
unsafe extern "system" fn listener_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        // 通知回调纪律：只采样计数 + 投递（零读写）；无共享 = 停止中/已停止，
        // 通知直接丢弃（迟到的通知不外发——shutdown 先摘共享的顺序保证）。
        let change = ClipChange {
            change_count: clipboard_sequence_number(),
        };
        let _ = with_shared(|shared| shared.sender.send(change));
    }
    // 全部消息（含 WM_CLIPBOARDUPDATE）走默认过程：通知消息无自定义返回语义。
    // SAFETY: hwnd/msg/wparam/lparam 为系统原样传入的合法参数，DefWindowProcW
    // 按契约转发（Win32 WindowProc 默认处理）。
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// `WM_CLIPBOARDUPDATE` 监听器：专用线程 + 隐藏消息窗口 + `GetMessageW` 泵
/// （事件驱动，docs/05 §2；与 tokio 的桥接由上层拉取）。
///
/// 生命周期：[`ClipboardWatcher::stop`]（显式）或 `Drop`（兜底）都是摘共享 →
/// `PostThreadMessageW(WM_QUIT)` → join——**drop 后线程必然退出，无泄漏**；
/// 窗口类与窗口在泵线程内创建与销毁（`DestroyWindow` 的线程契约）。
pub struct ClipboardWatcher {
    receiver: Receiver<ClipChange>,
    /// 泵线程 id（就绪握手回传；`PostThreadMessageW(WM_QUIT)` 定址）。
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

impl ClipboardWatcher {
    /// 启动监听线程（注册窗口类 → 建消息窗口 → 加监听 → 就绪握手 → 消息泵）。
    ///
    /// 启动前的既有变化不回放（监听自 `AddClipboardFormatListener` 成功后开始；
    /// 历史内容由消费方启动同步时经 [`WinClipboard::read_text`] 全量拉取语义
    /// 覆盖，与 mac watcher"基准同步采样"同等语义层面）。线程 id 经就绪握手
    /// 回传。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：窗口类注册/窗口创建/监听注册失败或线程
    /// 在就绪前退出。
    pub fn start() -> Result<Self, ClipboardError> {
        let (sender, receiver) = channel::<ClipChange>();
        let (ready_tx, ready_rx) = channel::<Result<u32, String>>();

        let worker = std::thread::Builder::new()
            .name("xcontrol-clipboard-watch".into())
            .spawn(move || run_listener_thread(ready_tx, sender))
            .map_err(|error| ClipboardError::Platform {
                action: "watcher-spawn",
                reason: format!("监听线程创建失败：{error}"),
            })?;

        // 就绪握手：线程 id（WM_QUIT 定址用）。失败时线程已清理窗口与静态槽
        // （见线程体）。
        let thread_id = ready_rx.recv().map_err(|_| ClipboardError::Platform {
            action: "watcher-start",
            reason: "监听线程在就绪前退出（panic 或提前 return）".into(),
        })?;
        let thread_id = thread_id.map_err(|reason| ClipboardError::Platform {
            action: "listener-setup",
            reason,
        })?;

        Ok(Self {
            receiver,
            thread_id,
            worker: Some(worker),
        })
    }

    /// 显式停止：摘共享 → `PostThreadMessageW(WM_QUIT)` → join（线程内随后
    /// `RemoveClipboardFormatListener` + `DestroyWindow`）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：监听线程 panic。
    pub fn stop(mut self) -> Result<(), ClipboardError> {
        self.shutdown()
    }

    /// 非阻塞拉取自上次调用以来积累的变化事件（无事件时 `out` 不变）。
    pub fn drain(&mut self, out: &mut Vec<ClipChange>) {
        while let Ok(event) = self.receiver.try_recv() {
            out.push(event);
        }
    }

    /// 关停公共路径（[`ClipboardWatcher::stop`] 与 [`Drop`] 共用）。
    fn shutdown(&mut self) -> Result<(), ClipboardError> {
        // 1) 先摘共享：此后任何迟到通知在 with_shared 处短路、直接丢弃。
        if let Ok(mut slot) = LISTENER_SHARED.lock() {
            *slot = None;
        }
        // 2) 投 WM_QUIT 唤醒消息泵（线程可能已自行退出：错误静默，join 为准）。
        // SAFETY: thread_id 是就绪握手回传的合法线程标识；WM_QUIT 为 Win32 预留
        // 消息，参数恒 (0, 0)。PostThreadMessageW 不重入剪贴板系统。
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        // 3) join：等待泵退出 + 监听移除 + 窗口销毁完成（都在线程内做）。
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| ClipboardError::Platform {
                action: "watcher-stop",
                reason: "监听线程 panic".into(),
            })?;
        }
        Ok(())
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也关停线程与监听窗口（幂等；join
        // 失败在 Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

/// 监听线程主体：窗口类注册 → 消息窗口 → `AddClipboardFormatListener` →
/// 就绪握手 → `GetMessageW` 泵 → 收尾（移除监听 + 销毁窗口）。
///
/// 每步失败都经 `ready_tx.send(Err(...))` 上报后退出（宿主 start 以 Err 收尾）；
/// 成功路径在泵退出后清理并返回。回调（窗口过程）纪律见 [`listener_wndproc`]。
fn run_listener_thread(ready_tx: Sender<Result<u32, String>>, sender: Sender<ClipChange>) {
    // SAFETY: GetModuleHandleW(None) 取本进程主模块句柄（Win32 文档明确 None
    // 语义）；失败仅见于加载器异常态，上抛给宿主（不回退零句柄——窗口类归属
    // 零实例句柄是不同注册域，宁 fail loud）。
    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => HINSTANCE(module.0),
        Err(error) => {
            let _ = ready_tx.send(Err(format!("GetModuleHandleW 失败：{error}")));
            return;
        }
    };

    // 窗口类注册：lpszClassName 指向编译期 UTF-16 字面量（w! 保证 NUL 结尾、
    // 'static 生命周期，覆盖 RegisterClassW 的引用要求）。
    let class = WNDCLASSW {
        lpfnWndProc: Some(listener_wndproc), // WNDPROC Option 形参，Some 装裸回调
        hInstance: hinstance,
        lpszClassName: LISTENER_CLASS,
        ..WNDCLASSW::default() // style/icon/cursor/background/menu 全零默认
    };
    // SAFETY: class 为栈上完整初始化的合法 WNDCLASSW（回调指针与类名如上）；
    // RegisterClassW 进程级副作用——重复注册返回 0，此时查 GetLastError：
    // ERROR_CLASS_ALREADY_EXISTS 视为幂等成功（复用既有类，多 watcher/重启场景）。
    if unsafe { RegisterClassW(&class) } == 0 {
        // SAFETY: GetLastError 读取本线程的最后错误码（紧跟失败的 RegisterClassW，
        // 中间无其他 Win32 调用覆盖）。
        let last_error = unsafe { GetLastError() };
        if last_error != ERROR_CLASS_ALREADY_EXISTS {
            let _ = ready_tx.send(Err(format!(
                "RegisterClassW 失败（error={}）",
                last_error.0
            )));
            return;
        }
    }

    // message-only 窗口：父级 HWND_MESSAGE（Win32 契约——不可见、不参与 Z 序/
    // 枚举，仅接收本线程投递的消息）；样式全零、尺寸全零。
    // SAFETY: 类名 w! 字面量（NUL 结尾 PCWSTR）；父级 HWND_MESSAGE 为 Win32
    // 预定义句柄常量；hinstance 与注册类一致；窗口名/菜单/参数全 None/空。
    let hwnd = match unsafe {
        CreateWindowExW(
            Default::default(),       // WINDOW_EX_STYLE(0)
            LISTENER_CLASS,           // 已注册类名
            w!("xcontrol clipboard"), // 窗口名（message-only 不显示）
            WINDOW_STYLE(0),          // message-only 无需样式位
            0,                        // x（无效值，message-only 忽略）
            0,                        // y
            0,                        // w
            0,                        // h
            Some(HWND_MESSAGE),       // 父级 = message-only 语义
            None,                     // hmenu
            Some(hinstance),          // 与窗口类一致的实例句柄
            None,                     // lpParam
        )
    } {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("CreateWindowExW 失败：{error}")));
            return;
        }
    };

    // 注册剪贴板变化监听（WM_CLIPBOARDUPDATE 从此投递到本线程消息队列）。
    // SAFETY: hwnd 是本线程刚创建的合法窗口句柄（AddClipboardFormatListener
    // 契约：窗口归调用线程所有，通知投递到该线程队列）。
    if let Err(error) = unsafe { AddClipboardFormatListener(hwnd) } {
        // SAFETY: hwnd 为本线程创建的合法窗口；失败路径清理，避免泄漏
        // message-only 窗口（窗口类注册是进程级资源，类不销毁——幂等复用见上）。
        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = ready_tx.send(Err(format!("AddClipboardFormatListener 失败：{error}")));
        return;
    }

    // 装静态共享（此后 WNDPROC 开始投递）→ 就绪握手（线程 id）→ 消息泵。
    // 装槽在握手前：宿主 start 返回后系统投递的第一条通知必然可达。
    if let Ok(mut slot) = LISTENER_SHARED.lock() {
        *slot = Some(ListenerShared { sender });
    }
    // SAFETY: GetCurrentThreadId 无参数、无前置条件（返回本线程 OS id；线程 id
    // 全进程有效，供宿主 PostThreadMessageW 定址）。
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready_tx.send(Ok(thread_id)).is_err() {
        // 宿主已放弃（recv 端关闭）：清理监听与窗口后退出。
        // SAFETY: hwnd 为本线程创建的合法窗口（同线程销毁，Win32 契约）。
        let _ = unsafe { RemoveClipboardFormatListener(hwnd) };
        let _ = unsafe { DestroyWindow(hwnd) };
        if let Ok(mut slot) = LISTENER_SHARED.lock() {
            *slot = None;
        }
        return;
    }

    // 消息泵：GetMessageW 阻塞取消息；WM_CLIPBOARDUPDATE 在 wndproc 内处理。
    // 返回 0 = WM_QUIT（关停）、-1 = 错误——两者均退出循环（BOOL.0 <= 0）；
    // 正数继续。TranslateMessage/DispatchMessageW 标准泵序（WM_CLIPBOARDUPDATE
    // 无字符翻译需求，TranslateMessage 保留为泵的完整形态）。
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg 为栈上合法出参；hwnd=None 接收本线程全部消息（含
        // message-only 窗口与线程消息）；过滤区间 0..0 = 不过滤。阻塞语义即
        // 设计（关停靠 WM_QUIT 唤醒）。
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        // SAFETY: msg 为 GetMessageW 刚填充的合法消息结构；TranslateMessage/
        // DispatchMessageW 标准泵序（按 Win32 消息循环契约）。
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 泵退出收尾：先移除监听（不再产生通知）再销毁窗口（同线程契约）。
    // SAFETY: hwnd 为本线程创建的合法窗口（Remove/Destroy 契约见上）。
    let _ = unsafe { RemoveClipboardFormatListener(hwnd) };
    let _ = unsafe { DestroyWindow(hwnd) };
    if let Ok(mut slot) = LISTENER_SHARED.lock() {
        *slot = None;
    }
}

// ============================================================
// 测试（纯逻辑：重试策略/常量钉死/序列号只读；Windows CI runner 跑
// `--features xcontrol-clipboard/windows` 时执行，docs/08 §5）
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::PoisonError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::{ClipFormatPayload, EchoFilter, EchoVerdict, FormatRead};

    /// 重试策略常量钉死（docs/05 §2"被占用 3×33ms"）：3 次重试、33ms 间隔。
    #[test]
    fn retry_constants_match_docs() {
        assert_eq!(
            OPEN_RETRY_ATTEMPTS, 3,
            "重试次数（docs/05 §2 rustdesk 同款）"
        );
        assert_eq!(OPEN_RETRY_INTERVAL, Duration::from_millis(33), "重试间隔");
    }

    /// retry_occupied：占用错误重试至初试+3 次（共 4 次尝试），随后放行错误；
    /// 非占用错误立即返回零重试；成功不重试。
    #[test]
    fn retry_occupied_attempts_then_surfaces_error() {
        let interval = Duration::ZERO; // 测试零间隔（生产 33ms 由常量钉死测试覆盖）

        // 恒占用：初试 + 3 重试 = 4 次尝试后上抛 ClipboardOccupied。
        let calls = AtomicUsize::new(0);
        let result = retry_occupied(interval, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(arboard::Error::ClipboardOccupied)
        });
        assert!(matches!(result, Err(arboard::Error::ClipboardOccupied)));
        assert_eq!(calls.load(Ordering::Relaxed), 1 + OPEN_RETRY_ATTEMPTS);

        // 非占用错误：立即上抛、零重试。
        let calls = AtomicUsize::new(0);
        let result = retry_occupied(interval, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(arboard::Error::ContentNotAvailable)
        });
        assert!(matches!(result, Err(arboard::Error::ContentNotAvailable)));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // 第二次成功：共 2 次调用（1 次重试）。
        let calls = AtomicUsize::new(0);
        let result = retry_occupied::<()>(interval, || {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                Err(arboard::Error::ClipboardOccupied)
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// 序列号只读不写：两次采样单调不减、无授权要求（docs/05 §6）；ClipboardWatch
    /// 判定面与 mac `ns_change_count_is_readonly_and_monotonic` 同构（arboard
    /// 初始化在 runner 用户会话恒可用）。
    #[test]
    fn sequence_number_is_readonly_monotonic() {
        let clipboard = WinClipboard::new().expect("arboard 初始化");
        let first = clipboard_sequence_number();
        let second = clipboard_sequence_number();
        assert!(second >= first, "序列号单调不减（{first} -> {second}）");
        assert!(
            !clipboard.has_changes_since(second),
            "无写入时 seen=最新计数 不触发（读取路径零副作用）"
        );
    }

    /// 冒烟测试互斥：写**同一个系统剪贴板**的 ignored 测试串行（macos 后端
    /// CLIPBOARD_SMOKE_LOCK 同款理由）。
    static CLIPBOARD_SMOKE_LOCK: Mutex<()> = Mutex::new(());

    /// 原剪贴板恢复守卫：drop 即恢复备份（panic unwind 路径同样执行）——
    /// 测试 payload 不留在用户剪贴板（macos 后端 RestoreGuard 同款）。
    struct RestoreGuard {
        clipboard: arboard::Clipboard,
        original: FormatRead,
    }

    impl Drop for RestoreGuard {
        fn drop(&mut self) {
            let result = match &self.original {
                FormatRead::Payload(ClipFormatPayload::Text(text)) => {
                    self.clipboard.set_text(text.as_str())
                }
                _ => self.clipboard.clear(),
            };
            match (&self.original, result) {
                (FormatRead::Payload(ClipFormatPayload::Text(_)), Ok(())) => {
                    println!("已恢复原剪贴板文本内容")
                }
                (FormatRead::Payload(ClipFormatPayload::Text(_)), Err(error)) => {
                    println!("恢复原剪贴板失败：{error}")
                }
                (_, Ok(())) => {
                    println!("原剪贴板为空/非文本：已清空剪贴板（非文本无法按原样恢复）")
                }
                (_, Err(error)) => println!("清空剪贴板失败（原内容为空/非文本）：{error}"),
            }
        }
    }

    /// 端到端 round-trip + 监听检测 + 回声判定全链路（#[ignore]：**写系统剪贴板**，
    /// 人工运行 `cargo test -p xcontrol-clipboard --features windows -- --ignored
    /// --nocapture`；实机移交项见 docs/05 §2.2）。
    ///
    /// 写入（LF→CRLF 落板）→ 监听线程收到自身写入的 WM_CLIPBOARDUPDATE（序列号
    /// 推进）→ 读取还原（CRLF→LF）→ EchoFilter 防线 3/1 回声判定；任何退出路径
    /// （含断言 panic）经守卫恢复原内容。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features windows -- --ignored --nocapture"]
    fn write_watch_echo_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut clipboard = WinClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        // 守卫先行：此后任何路径离开作用域（含 unwind）都恢复原内容
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let mut watcher = ClipboardWatcher::start().expect("监听线程启动");
        let baseline = clipboard_sequence_number();

        // 写入载荷为 LF 形式（线格式）；读回应归一化还原（CRLF 落板 → LF）。
        let payload = "xcontrol Windows 冒烟 αβγ\nsecond line";
        let mark = clipboard.write_text(payload).expect("写入成功");
        assert!(
            mark.change_count >= baseline,
            "写入后序列号相对写入前基准不回退"
        );

        // 写入 → 系统投递 WM_CLIPBOARDUPDATE → 监听线程投递事件（宽限 2s）。
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while events.is_empty() && Instant::now() < deadline {
            watcher.drain(&mut events);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!events.is_empty(), "自身写入应触发 WM_CLIPBOARDUPDATE 事件");
        assert!(
            events.iter().all(|event| event.change_count >= baseline),
            "事件携带序列号快照（≥ 写入前基准）"
        );

        // 读取还原（round-trip，FR-4.1；读侧 CRLF→LF 归一化）。
        match clipboard.read_text(u64::MAX) {
            Ok(FormatRead::Payload(ClipFormatPayload::Text(text))) => {
                assert_eq!(text, payload, "round-trip 还原（LF 归一化）")
            }
            other => panic!("写入后应读到同文本，得到 {other:?}"),
        }

        // EchoFilter 复用判定（验收点：纯逻辑区跨平台复用——Windows 平台层直接喂；
        // 指纹 = 归一化 LF 文本规范形，与读回形态一致，newline.rs 不动点）。
        let payload_content = crate::ClipboardContent {
            text: Some(payload.to_owned()),
            ..crate::ClipboardContent::default()
        };
        let mut filter = EchoFilter::default();
        filter.note_written(mark.clone());
        assert_eq!(
            filter.classify_content(Instant::now(), Some(&payload_content)),
            EchoVerdict::Echo,
            "静默窗内自身写入判回声（防线 3）"
        );
        assert_eq!(
            filter.classify_content(
                Instant::now() + crate::WRITE_SILENCE_WINDOW + Duration::from_millis(100),
                Some(&payload_content)
            ),
            EchoVerdict::Echo,
            "静默窗外同内容仍判回声（防线 1 指纹一致）"
        );
        let other_content = crate::ClipboardContent {
            text: Some("不同的内容".into()),
            ..crate::ClipboardContent::default()
        };
        assert_eq!(
            filter.classify_content(
                Instant::now() + crate::WRITE_SILENCE_WINDOW + Duration::from_millis(100),
                Some(&other_content)
            ),
            EchoVerdict::External,
            "静默窗外不同内容判外部变化（轮询会真实外发的路径）"
        );

        watcher.stop().expect("显式 stop 干净 join");
    }

    /// 图片/HTML round-trip（#[ignore]：**写系统剪贴板**，人工运行
    /// `cargo test -p xcontrol-clipboard --features windows -- --ignored
    /// --nocapture`；FR-4.2/4.3 实机复核项 docs/05 §2.2 item 6，11-19-m5-clipboard-full
    /// check 补录——原移交配方引用的 ignored 套件缺本测试）。
    ///
    /// 图片：wire PNG → CF_DIB（arboard image-data）→ 读回重编码 PNG → 解码像素
    /// 逐字节等价（PNG 字节必然不同，等价判据像素级——mac/x11 同款）。HTML：
    /// arboard set().html（clipboard-win CF_HTML 偏移头包装）→ 读回提取
    /// （StartFragment/EndFragment）——**字节等价性即 docs/05 §2.2 item 6 待答
    /// 问题**：读回形态打印在断言前（实机跑一次即得答案）；若读回 ≠ 写入
    /// fragment，按 docs/04 §2 裁定回声指纹对齐读回形态（防线 1 语义不变）。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features windows -- --ignored --nocapture"]
    fn image_html_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut clipboard = WinClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        // 图片：像素级 round-trip（8×6 确定性不透明像素，mac 后端同款）。
        let (width, height) = (8u32, 6u32);
        let rgba: Vec<u8> = (0..width * height)
            .flat_map(|i| {
                [
                    (i % 251) as u8,
                    (i / 3 % 251) as u8,
                    (i / 7 % 251) as u8,
                    255u8,
                ]
            })
            .collect();
        let wire_png = crate::png_codec::encode_rgba_to_png(&rgba, width, height).expect("编码");
        clipboard.write_image_png(&wire_png).expect("图片写入成功");
        match clipboard.read_image_png(u64::MAX).expect("图片读取失败") {
            FormatRead::Payload(ClipFormatPayload::ImagePng(read_png)) => {
                let (w2, h2, rgba2) =
                    crate::png_codec::decode_png_to_rgba(&read_png).expect("读回 PNG 解码");
                assert_eq!((w2, h2), (width, height), "读回尺寸一致");
                assert_eq!(
                    rgba2, rgba,
                    "CF_DIB round-trip 像素级等价（8bit RGBA 无损）"
                );
            }
            other => panic!("写入图片后应读到 PNG 载荷，得到 {other:?}"),
        }

        // HTML：CF_HTML 偏移头包装 → 读回提取形态实测（实机复核项的取证入口）。
        let payload = "<b>xcontrol Windows 冒烟</b> <i>αβγ</i><p>第二段</p>";
        clipboard.write_html(payload).expect("HTML 写入成功");
        match clipboard.read_html(u64::MAX).expect("HTML 读取失败") {
            FormatRead::Payload(ClipFormatPayload::Html(html)) => {
                println!("写入 fragment：{payload:?}");
                println!("读回 fragment：{html:?}");
                assert_eq!(
                    html, payload,
                    "CF_HTML 读回与写入 fragment 字节等价（实机复核）"
                );
            }
            other => panic!("写入 HTML 后应读到同 fragment，得到 {other:?}"),
        }
    }
}
