//! `xcontrol-clipboard` — 平台剪贴板抽象：`ClipboardWatch`（监听）trait、读写与
//! 防回环纯逻辑，以及 feature 门控的平台实现。
//!
//! # Trait 形状（M1 mock 语义，docs/04 §4）
//!
//! [`ClipboardWatch`] 以 core `MockClipboard` 的接口形状为准
//! （`change_count` + `has_changes_since`，即「变化方主动推送」的触发判定抽象）；
//! 平台实现（mac `NSPasteboard.changeCount`）与 mock 对拍验证语义一致
//! （core mock 只依赖 proto/std，无法反向依赖本 crate——对拍用 dev-dep 测试完成）。
//!
//! # 平台实现（docs/05 §0/§1/§3/§4、docs/08 §3）
//!
//! 经 feature 启用：`macos` / `windows` / `x11` / `wayland`，模块路径固定
//! `platform/{macos,windows,x11,wayland}.rs`。监听路径：mac `changeCount` 轮询
//! 250ms（mac 无系统剪贴板通知 API）/ win `WM_CLIPBOARDUPDATE` / X11 `XFixes` /
//! Wayland `wl-clipboard-rs` Watcher（0.9.5 首含 watch 模块）。
//!
//! 读写面三格式（M5 FR-4.1/4.2/4.3，docs/04 §2 线格式）：
//! - **文本**（FR-4.1，M2 起）：线格式 LF 文本（Windows 平台层 CRLF 转换，
//!   [`newline`]）；mac/x11/win 经 arboard，wayland 经 wl-clipboard-rs。
//! - **图片**（FR-4.2，11-19-m5-clipboard-full 起）：wire 格式恒为 PNG 字节
//!   （[`ClipFormatPayload::ImagePng`]）；平台本地格式（mac NSImage/TIFF、win
//!   CF_DIB、x11 `image/png` target）与 wire 的 RGBA↔PNG 转换收敛在公共
//!   [`png_codec`]（PNG 编解码裁定记录见 workspace Cargo.toml png 条目）。
//!   wayland 为 mime 字节直通（`image/png` 原样）。
//! - **HTML**（FR-4.3，11-19-m5-clipboard-full 起）：wire 格式为 UTF-8 HTML
//!   fragment（[`ClipFormatPayload::Html`]）；mac 写入走 public.html 原生挂载
//!   （objc2-app-kit 安全绑定，**零 unsafe**，platform/macos.rs 模块文档）、
//!   win CF_HTML（arboard clipboard-win）、x11 `text/html` target（arboard）、
//!   wayland `text/html` mime（wl-clipboard-rs 直通）。
//!
//! # 防回环（docs/04 §5 防线 1 + 防线 3，本机回声抑制）
//!
//! 写入方记录「内容指纹（blake3 短哈希）+ 写入后 changeCount 基准 + 时刻」
//! （[`WrittenMark`]）；监听检测到变化后读取内容、经
//! [`fingerprint_content`] 规范化取指纹，交 [`EchoFilter::classify`] 判定：
//! 命中回声则不外发。防线 2（origin 序列号）在协议层（`ClipAnnounce.origin_*`）。
//!
//! 指纹规范形（**payload 类型扩展的关键裁定**）：文本取 LF 归一化字节
//! （Windows 指纹字节教训，[`newline`]）；图片取**解码后 RGBA 像素**（PNG 是
//! 可重编码容器——mac NSImage/TIFF 读回重编码的字节必然不同，像素才是
//! round-trip 不动点，[`png_codec`] 模块文档）；HTML 取 UTF-8 字节。判据不变量：
//! 指纹必须取**读回形态的 round-trip 不动点**。
//!
//! # 线程纪律（architecture-rules 红线 3）
//!
//! 轮询/监听线程只做「变化判定 → `std::sync::mpsc` 投递」，**零 async、零
//! tokio**；剪贴板读写不在监听线程做（回调线程纪律，platform-code.md）——由
//! 消费方在收到变化事件后自行调用平台读写。
//!
//! # unsafe 纪律（docs/08 §2）
//!
//! 本 crate 是预期出现 unsafe 的平台绑定层之一（workspace `unsafe_code = deny`
//! 在平台模块内文件级放行 + 逐块 `// SAFETY:`）；公共代码（trait/纯逻辑区）零
//! unsafe。M5 实况：windows.rs 承载全部 unsafe（FFI 窗口回调）；mac 的
//! public.html 挂载经 objc2 0.6 安全绑定、x11/wayland 后端零 unsafe——mac/x11/
//! wayland 平台段当前实际零 unsafe（文件级放行保留为平台层纪律）。

pub mod error;
pub mod newline;
pub mod platform;
pub mod png_codec;

use std::time::{Duration, Instant};

pub use error::ClipboardError;
#[cfg(feature = "macos")]
pub use platform::macos::{ClipChange, ClipboardWatcher, DEFAULT_POLL_INTERVAL, MacClipboard};
#[cfg(feature = "wayland")]
pub use platform::wayland::{WaylandClipboard, WaylandWatcher};
#[cfg(feature = "windows")]
pub use platform::windows::{ClipChange, ClipboardWatcher, WinClipboard};
#[cfg(feature = "x11")]
pub use platform::x11::{ClipChange, X11Clipboard, X11Watcher};

// ============================================================
// 平台 feature 互斥断言（design.md：单 feature 多选一；同时开两个平台 feature
// 是编译错误——编译期断言。arboard 是跨平台共享依赖，双开只会在"工厂选哪个
// 实现"处产生歧义，fail loud 于编译期而非运行时静默择一。）
// ============================================================

#[cfg(all(feature = "macos", feature = "windows"))]
compile_error!(
    "xcontrol-clipboard 平台 feature 互斥：macos 与 windows 同时开启\
（设计约束：单平台多选一，见 design.md 与 lib.rs 断言注释）"
);
#[cfg(all(feature = "macos", feature = "x11"))]
compile_error!(
    "xcontrol-clipboard 平台 feature 互斥：macos 与 x11 同时开启\
（设计约束：单平台多选一，见 design.md 与 lib.rs 断言注释）"
);
#[cfg(all(feature = "macos", feature = "wayland"))]
compile_error!(
    "xcontrol-clipboard 平台 feature 互斥：macos 与 wayland 同时开启\
（设计约束：单平台多选一，见 design.md 与 lib.rs 断言注释）"
);
#[cfg(all(feature = "windows", feature = "x11"))]
compile_error!(
    "xcontrol-clipboard 平台 feature 互斥：windows 与 x11 同时开启\
（设计约束：单平台多选一，见 design.md 与 lib.rs 断言注释）"
);
#[cfg(all(feature = "windows", feature = "wayland"))]
compile_error!(
    "xcontrol-clipboard 平台 feature 互斥：windows 与 wayland 同时开启\
（设计约束：单平台多选一，见 design.md 与 lib.rs 断言注释）"
);
#[cfg(all(feature = "x11", feature = "wayland"))]
compile_error!(
    "xcontrol-clipboard 平台 feature 互斥：x11 与 wayland 同时开启\
（设计约束：单平台多选一，见 design.md 与 lib.rs 断言注释）"
);

// ============================================================
// ClipboardBackend 工厂（design.md：按 feature 编译对应平台实现——调用方以
// `PlatformClipboard` 统一类型名 + `open_platform_clipboard()` 工厂取本机后端，
// 平台选择由 feature 层承载（architecture-rules 红线 2），公共代码零
// `#[cfg(target_os)]`。读写面（new/write_text/read_text/write_image_png/
// read_image_png/write_html/read_html）四平台同形；ClipboardWatch trait 各平台
// 各自实现（mac/win 以全局计数器轮询可独立于监听线程读数；x11/wayland 计数 =
// 监听线程观测事件数，见 platform/x11.rs 与 platform/wayland.rs 模块文档）。
// ============================================================

/// 平台剪贴板后端（按 feature 编译期选定的 [`MacClipboard`] / [`WinClipboard`] /
/// [`X11Clipboard`] / [`WaylandClipboard`]）。
///
/// [`MacClipboard`]: crate::platform::macos::MacClipboard
/// [`WinClipboard`]: crate::platform::windows::WinClipboard
/// [`X11Clipboard`]: crate::platform::x11::X11Clipboard
/// [`WaylandClipboard`]: crate::platform::wayland::WaylandClipboard
#[cfg(feature = "macos")]
pub type PlatformClipboard = crate::platform::macos::MacClipboard;
#[cfg(feature = "windows")]
pub type PlatformClipboard = crate::platform::windows::WinClipboard;
#[cfg(feature = "x11")]
pub type PlatformClipboard = crate::platform::x11::X11Clipboard;
#[cfg(feature = "wayland")]
pub type PlatformClipboard = crate::platform::wayland::WaylandClipboard;

/// 打开本机平台剪贴板后端（读写统一入口；监听经各平台 watcher——mac
/// `ClipboardWatcher::start(interval)` / win `ClipboardWatcher::start()` /
/// x11 `X11Clipboard::watch()` / wayland `WaylandClipboard::watch()`）。
///
/// # Errors
/// [`ClipboardError::Platform`]：平台剪贴板会话创建失败（mac 会话 / win32 会话 /
/// X11 连接 / Wayland 连接）。
#[cfg(feature = "macos")]
pub fn open_platform_clipboard() -> Result<PlatformClipboard, ClipboardError> {
    MacClipboard::new()
}
#[cfg(feature = "windows")]
pub fn open_platform_clipboard() -> Result<PlatformClipboard, ClipboardError> {
    WinClipboard::new()
}
#[cfg(feature = "x11")]
pub fn open_platform_clipboard() -> Result<PlatformClipboard, ClipboardError> {
    X11Clipboard::new()
}
#[cfg(feature = "wayland")]
pub fn open_platform_clipboard() -> Result<PlatformClipboard, ClipboardError> {
    WaylandClipboard::new()
}

// ============================================================
// ClipboardWatch trait（M1 mock 接口形状；docs/04 §4「变化方主动推送」）
// ============================================================

/// 剪贴板变化监听抽象（docs/04 §4 平台监听路径的语义契约）。
///
/// 接口形状以 core `MockClipboard`（M1）为准：变化计数单调不减，`seen` 之后
/// 发生过写入即触发。平台实现（mac 为 `NSPasteboard.changeCount` 轮询的原语）
/// 与 mock 在同一语义下可互换，供上层「变化方主动推送」逻辑复用。
pub trait ClipboardWatch {
    /// 当前累计变化计数（平台 changeCount / mock 计数；单调不减）。
    fn change_count(&self) -> u64;

    /// `seen` 之后发生过写入（docs/04 §4 触发判定；`seen` 取自上次消费的计数）。
    fn has_changes_since(&self, seen: u64) -> bool;
}

// ============================================================
// 写入指纹（docs/04 §5 防线 1）
// ============================================================

/// 内容指纹：`blake3(规范形字节)[0..16]`（128 位，碰撞概率对本用途足够；
/// docs/04 §5 防线 1 的「最近一次自写哈希」）。
pub type ContentFingerprint = [u8; 16];

/// 计算字节内容的 blake3 短哈希指纹。
///
/// 文本路径传 LF 归一化后的 UTF-8 字节（[`crate::newline`] round-trip 不动点）；
/// 图片路径传 [`png_codec::rgba_canonical_fingerprint_input`] 的规范形（PNG 是
/// 可重编码容器，指纹取像素不动点而非 PNG 字节，[`png_codec`] 模块文档）。
#[must_use]
pub fn fingerprint_bytes(bytes: &[u8]) -> ContentFingerprint {
    let mut fp = [0u8; 16];
    let digest = blake3::hash(bytes);
    fp.copy_from_slice(&digest.as_bytes()[..16]);
    fp
}

/// 计算文本内容指纹（LF 归一化后取短哈希；M2 文本路径的既有语义）。
#[must_use]
pub fn fingerprint(text: &str) -> ContentFingerprint {
    fingerprint_bytes(crate::newline::normalize_to_lf(text).as_bytes())
}

// ============================================================
// 单格式读取判定（FR-4.1 空剪贴板常态 + FR-4.5 超限信号 + FR-4.2/4.3 多格式）
// ============================================================

/// 单格式载荷（平台读取的原始产物；形态 = docs/04 §2 线格式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipFormatPayload {
    /// `text/plain`：UTF-8 文本，换行统一 LF（平台层保证，docs/04 §2）。
    Text(String),
    /// `image/png`：PNG 字节流（图片交换唯一必选格式，FR-4.2）。
    ImagePng(Vec<u8>),
    /// `text/html`：UTF-8 HTML fragment（FR-4.3；换行 LF）。
    Html(String),
}

impl ClipFormatPayload {
    /// 载荷字节数（FR-4.5 上限判定的口径 = 线格式字节数）。
    #[must_use]
    pub fn len(&self) -> u64 {
        match self {
            Self::Text(text) | Self::Html(text) => u64::try_from(text.len()).unwrap_or(u64::MAX),
            Self::ImagePng(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        }
    }

    /// 载荷是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) | Self::Html(text) => text.is_empty(),
            Self::ImagePng(bytes) => bytes.is_empty(),
        }
    }
}

/// 单格式读取结果（纯逻辑判定产物；跨平台共用）。
///
/// 「无内容」与「超限」都是**常态/信号而非错误**：前者是空剪贴板/无该格式的
/// 常态处理（FR-4.1），后者是 FR-4.5 的超限不传输语义（上层托盘气泡提示，
/// 本层只报信号）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatRead {
    /// 无内容（空剪贴板 / 无该格式 / 空载荷）——不外发、不报错。
    NoContent,
    /// 内容载荷（≤ 上限；形态即线格式，docs/04 §2）。
    Payload(ClipFormatPayload),
    /// 超限信号（携带实际字节长度）：**不截断、不外发**（FR-4.5 超限不传输，
    /// 提示由上层 UI 做）。原内容在此丢弃（只读一次，不二次入内存）。
    Oversize {
        /// 实际内容字节数（供上层提示「剪贴板内容 N MB 超过上限」）。
        actual_len: u64,
    },
}

/// 把平台读取的原始载荷归一化为 [`FormatRead`]（超限判定 + 有效性判定）。
///
/// - `None`（空剪贴板 / 无该格式）→ [`FormatRead::NoContent`]（FR-4.1 常态）；
/// - 空载荷（空串/零字节）→ [`FormatRead::NoContent`]（空内容同步无意义，
///   语义与空剪贴板一致）；
/// - 字节数 > `max_bytes` → [`FormatRead::Oversize`]（FR-4.5，不截断）；
/// - 其余 → [`FormatRead::Payload`]。
///
/// 协商上限取 `ClipboardConfig::max_bytes`（FR-4.5 可配；协议默认 16MiB 见
/// `ClipAnnounce::DEFAULT_MAX_TOTAL_LEN`）。
#[must_use]
pub fn format_read_from(raw: Option<ClipFormatPayload>, max_bytes: u64) -> FormatRead {
    let Some(payload) = raw else {
        return FormatRead::NoContent;
    };
    if payload.is_empty() {
        return FormatRead::NoContent;
    }
    if payload.len() > max_bytes {
        return FormatRead::Oversize {
            actual_len: payload.len(),
        };
    }
    FormatRead::Payload(payload)
}

// ============================================================
// 多格式内容容器（FR-4.2/4.3 线格式形态；协议层 announce 组装的输入）
// ============================================================

/// 平台无关的多格式内容容器（docs/04 §2：一次同步可多格式并存）。
///
/// 各字段形态即线格式（text=LF 文本、image=PNG 字节、html=fragment）；
/// `None` = 该格式不存在。协议层组装 `ClipAnnounce.formats` 与分段载荷时
/// 由此取用（proto 侧布局见 `xcontrol_proto::clip::encode_format_segments`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardContent {
    /// `text/plain`（LF 归一化由平台读取路径保证）。
    pub text: Option<String>,
    /// `image/png`（wire PNG 字节）。
    pub image_png: Option<Vec<u8>>,
    /// `text/html`（UTF-8 fragment）。
    pub html: Option<String>,
}

impl ClipboardContent {
    /// 全部载荷字节和（FR-4.5 上限口径；协议 `total_len` 的组装基准）。
    #[must_use]
    pub fn total_len(&self) -> u64 {
        let mut total = 0u64;
        if let Some(text) = &self.text {
            total = total.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
        }
        if let Some(png) = &self.image_png {
            total = total.saturating_add(u64::try_from(png.len()).unwrap_or(u64::MAX));
        }
        if let Some(html) = &self.html {
            total = total.saturating_add(u64::try_from(html.len()).unwrap_or(u64::MAX));
        }
        total
    }

    /// 是否三个格式全部为空/缺省（同步无意义的空内容）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.as_ref().is_none_or(String::is_empty)
            && self.image_png.as_ref().is_none_or(Vec::is_empty)
            && self.html.as_ref().is_none_or(String::is_empty)
    }

    /// 按载荷类型归一化为容器（[`FormatRead`] → 容器的装配步骤）。
    #[must_use]
    pub fn with_payload(mut self, payload: ClipFormatPayload) -> Self {
        match payload {
            ClipFormatPayload::Text(text) => self.text = Some(text),
            ClipFormatPayload::ImagePng(png) => self.image_png = Some(png),
            ClipFormatPayload::Html(html) => self.html = Some(html),
        }
        self
    }
}

/// 内容指纹的规范形输入（[`fingerprint_content`] 的判据字节；docs/04 §5）。
///
/// 布局：每个存在且非空的格式按 text → image → html 固定顺序取
/// `tag(u8) + len(u32 BE) + 规范形字节` 拼接（tag 隔离跨格式碰撞——同字节
/// 不同格式不碰撞；len 消除拼接歧义）。规范形：
/// - text：LF 归一化 UTF-8 字节（Windows 指纹字节教训，round-trip 不动点）；
/// - image：解码 PNG 后的 RGBA 像素规范形（[`png_codec::png_canonical_fingerprint_input`]
///   ——PNG 可重编码，像素是 round-trip 不动点；解码失败时**降级为 PNG 字节
///   原样**，防线从"判定回声"退化为"可能误判外部变化"的有界假阳性，不 panic
///   不阻塞同步）；
/// - html：UTF-8 字节原样。
///
/// 空/缺省格式不进哈希；全空内容的判据字节为空（指纹恒定，无意义内容不会与
/// 真实内容碰撞——tag+len 布局保证）。写入侧标记与读出侧判定必须同经本函数
/// （[`fingerprint_content`] 单一入口）。
#[must_use]
pub fn content_fingerprint_input(content: &ClipboardContent) -> Vec<u8> {
    // 各格式的规范形字节（存在且非空才参与）。
    let text_canonical = content
        .text
        .as_ref()
        .filter(|text| !text.is_empty())
        .map(|text| crate::newline::normalize_to_lf(text).into_bytes());
    let image_canonical = content
        .image_png
        .as_ref()
        .filter(|png| !png.is_empty())
        .map(|png| {
            // 图片规范形优先取像素（round-trip 不动点）；解码失败降级 PNG 原样
            // 字节（畸形 PNG 不应阻塞指纹——假阳性后果有界，见模块文档）。
            png_codec::png_canonical_fingerprint_input(png).unwrap_or_else(|_| png.clone())
        });
    let html_canonical = content
        .html
        .as_ref()
        .filter(|html| !html.is_empty())
        .map(|html| html.as_bytes().to_vec());

    let mut out = Vec::new();
    let parts = [
        text_canonical.as_deref(),
        image_canonical.as_deref(),
        html_canonical.as_deref(),
    ];
    for (tag, part) in [1u8, 2, 3].into_iter().zip(parts) {
        if let Some(part) = part {
            out.push(tag);
            out.extend_from_slice(&u32::try_from(part.len()).unwrap_or(u32::MAX).to_be_bytes());
            out.extend_from_slice(part);
        }
    }
    out
}

/// 计算多格式内容指纹（blake3 短哈希；防线 1 的「最近一次自写哈希」）。
#[must_use]
pub fn fingerprint_content(content: &ClipboardContent) -> ContentFingerprint {
    fingerprint_bytes(&content_fingerprint_input(content))
}

// ============================================================
// 回声抑制（docs/04 §5 防线 1 + 防线 3；防线 2 在协议层 ClipAnnounce.origin_*）
// ============================================================

/// 写后静默窗（防线 3）：本地写入后窗口内的任何变化事件一律视为自身写入
/// 的回声，仅保基线不外发（docs/04 §5）。
pub const WRITE_SILENCE_WINDOW: Duration = Duration::from_millis(500);

/// 自写回声窗（防线 1）：窗口内读出内容与最后写入内容一致 → 回声（docs/04 §5）。
pub const SELF_ECHO_WINDOW: Duration = Duration::from_millis(2_000);

/// 一次本地写入的记录（回声抑制基线；[`EchoFilter::note_written`] 的入参）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenMark {
    /// 内容指纹（blake3 短哈希；防线 1 主判据——按
    /// [`fingerprint_content`] 的规范形计算）。
    pub fingerprint: ContentFingerprint,
    /// 写入后立即采样的变化计数基准（平台 changeCount）。
    ///
    /// 记录但不参与 [`EchoFilter::classify`] 判定——「基准后的第 N 次计数推进」
    /// 不可靠（一次写入可能推进多次、窗口间也可能被外部写入穿插），仅作诊断
    /// 与消费方轮询基准的接线数据；回声判定以内容一致为准（见 classify 注释）。
    pub change_count: u64,
    /// 写入时刻（本机单调时钟；时间窗判定用）。
    pub written_at: Instant,
}

/// 回声判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoVerdict {
    /// 自身写入的回声（防线 1/3 命中）：更新本地基线、**不外发**。
    Echo,
    /// 外部变化：读取内容走「读取 → 上限判定 → 外发」流程。
    External,
}

/// 本机回声抑制器（docs/04 §5 防线 1 + 防线 3；本 crate 的公共代码，
/// 平台无关——mock 集成测试与平台实现共用）。
///
/// 单实例对应单台机器的监听器；写入方在每次本地写入后调
/// [`EchoFilter::note_written`]，监听回调在变化事件到达时调
/// [`EchoFilter::classify`]（入参为读出内容的指纹——文本/图片/HTML 统一走
/// [`fingerprint_content`] 规范形，payload 类型扩展不改判定逻辑）。
#[derive(Debug, Default)]
pub struct EchoFilter {
    last_written: Option<WrittenMark>,
}

impl EchoFilter {
    /// 记录一次本地写入（覆盖既有基线）。
    pub fn note_written(&mut self, mark: WrittenMark) {
        self.last_written = Some(mark);
    }

    /// 最后一次写入记录（消费方核对轮询基准用）。
    #[must_use]
    pub const fn last_written(&self) -> Option<&WrittenMark> {
        self.last_written.as_ref()
    }

    /// 判定一次变化事件是否为自身写入的回声（判定逻辑与 M2 文本路径一致，
    /// 指纹入参泛化到内容指纹——**防线语义不变**，payload 类型扩展仅扩展
    /// 指纹规范形，docs/04 §5）。
    ///
    /// 判定顺序与权衡（docs/04 §5）：
    ///
    /// 1. **防线 3（写后静默窗 500ms）**：窗口内一律 [`EchoVerdict::Echo`]，
    ///    **不看指纹**——NSPasteboard/macOS 剪贴板写入是惰性服务（数据提供方
    ///    机制），写入后 changeCount 的推进不保证单次完成，窗口期是「我刚写过，
    ///    接下来第一波事件大概率是回声」的时间证据。代价：用户在 500ms 内紧接着
    ///    复制的新内容也被抑制一次（可接受假阴性；该内容再次复制仍会同步，
    ///    docs/04 §5 边界说明同款语义）。
    /// 2. **防线 1（内容指纹 + 2s 窗）**：窗口内读出内容指纹与最后写入指纹
    ///    一致 → [`EchoVerdict::Echo`]。**主判据是内容一致**，changeCount 只作
    ///    触发源不参与判定——「写入基准后的第 N 次计数推进」可能是多次推进/
    ///    外部穿插，无法构成回声的充分证据（设计权衡，任务
    ///    11-17-m2-clipboard-macos）；指纹一致即「这就是我刚写的那份内容」的
    ///    直接证据。
    /// 3. 其余 → [`EchoVerdict::External`]。
    ///
    /// `fingerprint` 取 `None` 表示「无内容可指纹化」（空剪贴板/读取失败）——
    /// 窗口外无内容证据只能放行为外部变化（与 M2 非文本语义一致）。
    ///
    /// 边界（docs/04 §5 已接受的假阴性）：用户在 2s 窗内复制**完全相同内容**
    /// 被防线 1 拦截——内容相同，同步与否无差别。窗口外（>2s）的相同内容照常
    /// 外发。
    #[must_use]
    pub fn classify(&self, now: Instant, fingerprint: Option<ContentFingerprint>) -> EchoVerdict {
        let Some(mark) = &self.last_written else {
            return EchoVerdict::External;
        };
        let elapsed = now.saturating_duration_since(mark.written_at);
        // 防线 3：写后静默窗内不看指纹（见 docstring 权衡 1）
        if elapsed <= WRITE_SILENCE_WINDOW {
            return EchoVerdict::Echo;
        }
        // 防线 1：内容指纹一致（见 docstring 权衡 2；无指纹证据在窗口外无法
        // 比对——窗口内已被防线 3 覆盖，窗口外只能放行为外部变化）
        if elapsed <= SELF_ECHO_WINDOW
            && let Some(fingerprint) = fingerprint
            && fingerprint == mark.fingerprint
        {
            return EchoVerdict::Echo;
        }
        EchoVerdict::External
    }

    /// [`EchoFilter::classify`] 的内容便捷入口：对读出内容取规范形指纹后判定
    /// （读出侧的规范化与写入侧同源，保证 round-trip 不动点）。
    #[must_use]
    pub fn classify_content(
        &self,
        now: Instant,
        content: Option<&ClipboardContent>,
    ) -> EchoVerdict {
        let fingerprint = content.map(fingerprint_content);
        self.classify(now, fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use xcontrol_config::DEFAULT_CLIPBOARD_MAX_BYTES;
    use xcontrol_core::mock::MockClipboard;
    use xcontrol_proto::clip::ClipAnnounce;

    use super::*;

    /// 写入指纹：同文本同指纹、异文本异指纹、短哈希 16B；CRLF 与 LF 输入**同
    /// 指纹**（M5 起 fingerprint 经 normalize_to_lf 规范形——round-trip 不动点
    /// 判据上提到公共入口，Windows 指纹字节教训的全平台统一落地）。
    #[test]
    fn fingerprint_is_stable_and_discriminating() {
        let a = fingerprint("hello 剪贴板\n");
        assert_eq!(a, fingerprint("hello 剪贴板\n"), "同内容指纹一致");
        assert_ne!(a, fingerprint("hello 剪贴板"), "差一个字节指纹不同");
        assert_eq!(
            a,
            fingerprint("hello 剪贴板\r\n"),
            "CRLF/LF 同指纹（规范形 round-trip 不动点）"
        );
        assert_eq!(a.len(), 16, "短哈希 16 字节");

        // fingerprint_bytes 与 fingerprint 的关系：LF 文本的字节指纹一致。
        assert_eq!(
            fingerprint("hello 剪贴板\n"),
            fingerprint_bytes("hello 剪贴板\n".as_bytes()),
            "文本指纹 = 规范形字节指纹"
        );
    }

    /// 无内容（None / 空载荷）→ NoContent 常态而非错误（FR-4.1/FR-4.2/FR-4.3）。
    #[test]
    fn missing_or_empty_payload_is_no_content() {
        assert_eq!(
            format_read_from(None, 1024),
            FormatRead::NoContent,
            "空剪贴板/无该格式"
        );
        assert_eq!(
            format_read_from(Some(ClipFormatPayload::Text(String::new())), 1024),
            FormatRead::NoContent,
            "空文本"
        );
        assert_eq!(
            format_read_from(Some(ClipFormatPayload::ImagePng(Vec::new())), 1024),
            FormatRead::NoContent,
            "空图片载荷"
        );
    }

    /// 超限信号路径（FR-4.5）：16MiB+1 拒绝并携带实际长度、恰好 16MiB 通过、
    /// 协商缩限同样生效；图片载荷同口径；不截断。
    #[test]
    fn oversize_payload_yields_signal_not_truncation() {
        let max = u64::from(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN);
        let at_limit = "a".repeat(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize);
        assert_eq!(
            format_read_from(Some(ClipFormatPayload::Text(at_limit.clone())), max),
            FormatRead::Payload(ClipFormatPayload::Text(at_limit)),
            "恰好上限可同步"
        );

        let over = "b".repeat(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize + 1);
        match format_read_from(Some(ClipFormatPayload::Text(over.clone())), max) {
            FormatRead::Oversize { actual_len } => {
                assert_eq!(actual_len, max + 1, "信号携带实际字节长度");
            }
            other => panic!("16MiB+1 应返回超限信号，得到 {other:?}"),
        }

        // 图片载荷同口径（FR-4.5 对 image/png 适用，16MiB 复核结论见 docs/04 §2）。
        let png_over = vec![0x89u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize + 1];
        match format_read_from(Some(ClipFormatPayload::ImagePng(png_over)), max) {
            FormatRead::Oversize { actual_len } => assert_eq!(actual_len, max + 1),
            other => panic!("图片 16MiB+1 应返回超限信号，得到 {other:?}"),
        }

        let small = "x".repeat(2048);
        match format_read_from(Some(ClipFormatPayload::Text(small.clone())), 1024) {
            FormatRead::Oversize { actual_len } => assert_eq!(actual_len, 2048),
            other => panic!("协商缩限（1KiB）应同样拒绝，得到 {other:?}"),
        }
    }

    /// 上限基准一致性：config 默认（FR-4.5 可配项的默认值）与协议默认
    /// （ClipAnnounce 解码校验）不得漂移（都为 16MiB）。
    #[test]
    fn default_max_bytes_matches_proto_default() {
        assert_eq!(
            DEFAULT_CLIPBOARD_MAX_BYTES,
            u64::from(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN),
            "config 默认上限与协议默认上限应同为 16MiB"
        );
    }

    /// 多格式容器：total_len 各格式求和、is_empty 全空判定、with_payload 装配。
    #[test]
    fn clipboard_content_totals_and_assembly() {
        let mut content = ClipboardContent::default();
        assert!(content.is_empty());
        assert_eq!(content.total_len(), 0);

        content = content.with_payload(ClipFormatPayload::Text("hello".into()));
        assert!(!content.is_empty());
        assert_eq!(content.total_len(), 5);
        content = content.with_payload(ClipFormatPayload::ImagePng(vec![0u8; 100]));
        content = content.with_payload(ClipFormatPayload::Html("<b>x</b>".into()));
        assert_eq!(content.total_len(), 5 + 100 + 8);
        assert_eq!(
            content,
            ClipboardContent {
                text: Some("hello".into()),
                image_png: Some(vec![0u8; 100]),
                html: Some("<b>x</b>".into()),
            }
        );
    }

    /// 内容指纹规范形（docs/04 §5 M5 扩展）：同内容同指纹、跨格式不碰撞、
    /// 图片指纹取像素不动点（PNG 字节不同、指纹一致）、文本规范形含 LF 归一化
    /// （CRLF/LF 同指纹）。
    #[test]
    fn fingerprint_content_is_canonical_and_format_aware() {
        let text_only = ClipboardContent {
            text: Some("内容".into()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            fingerprint_content(&ClipboardContent {
                text: Some("内容\n".into()),
                ..ClipboardContent::default()
            }),
            fingerprint_content(&ClipboardContent {
                text: Some("内容\r\n".into()),
                ..ClipboardContent::default()
            }),
            "LF 归一化进规范形（CRLF/LF 同指纹）"
        );
        assert_eq!(
            fingerprint_content(&text_only),
            fingerprint_bytes(&content_fingerprint_input(&text_only)),
            "fingerprint_content 与 content_fingerprint_input 入口一致"
        );

        // PNG 重编码（字节不同）→ 同指纹（像素不动点，png_codec 模块文档）。
        let (width, height, rgba) = (2u32, 1u32, vec![1u8, 2, 3, 255, 4, 5, 6, 0]);
        let png_a = png_codec::encode_rgba_to_png(&rgba, width, height).expect("编码");
        let mut rgba_b = rgba.clone();
        rgba_b[0] ^= 1; // 像素不同
        let png_b = png_codec::encode_rgba_to_png(&rgba_b, width, height).expect("编码");
        let fp_a = fingerprint_content(&ClipboardContent {
            image_png: Some(png_a.clone()),
            ..ClipboardContent::default()
        });
        let fp_b = fingerprint_content(&ClipboardContent {
            image_png: Some(png_b.clone()),
            ..ClipboardContent::default()
        });
        assert_ne!(fp_a, fp_b, "像素不同指纹不同");
        let fp_a2 = fingerprint_content(&ClipboardContent {
            image_png: Some(png_a),
            ..ClipboardContent::default()
        });
        assert_eq!(fp_a, fp_a2, "同内容同指纹");

        // 跨格式不碰撞：text="x" 与 html="x" 指纹不同（tag+len 布局）。
        let as_text = fingerprint_content(&ClipboardContent {
            text: Some("x".into()),
            ..ClipboardContent::default()
        });
        let as_html = fingerprint_content(&ClipboardContent {
            html: Some("x".into()),
            ..ClipboardContent::default()
        });
        assert_ne!(as_text, as_html, "同字节不同格式不碰撞");

        // 多格式内容指纹 ≠ 任何单格式子集（组合参与哈希）。
        let combined = ClipboardContent {
            text: Some("x".into()),
            html: Some("x".into()),
            ..ClipboardContent::default()
        };
        assert_ne!(fingerprint_content(&combined), as_text);
        assert_ne!(fingerprint_content(&combined), as_html);
    }

    /// 回声判定矩阵（docs/04 §5 防线 1 + 防线 3）。
    ///
    /// 窗口常量值一并钉死（500ms / 2s，docs/04 §5 数值依据），防止后续调参
    /// 无记录漂移；边界含端点（<=）也用异内容逐毫秒钉死。
    #[test]
    fn echo_filter_classifies_by_window_then_fingerprint() {
        assert_eq!(
            WRITE_SILENCE_WINDOW,
            Duration::from_millis(500),
            "防线 3 静默窗 = 500ms（docs/04 §5）"
        );
        assert_eq!(
            SELF_ECHO_WINDOW,
            Duration::from_millis(2_000),
            "防线 1 指纹窗 = 2s（docs/04 §5）"
        );
        let mut filter = EchoFilter::default();
        let text = "machine A 的文本";
        let other = "另一段内容";
        let content = ClipboardContent {
            text: Some(text.into()),
            ..ClipboardContent::default()
        };
        let other_content = ClipboardContent {
            text: Some(other.into()),
            ..ClipboardContent::default()
        };
        let t0 = Instant::now();
        filter.note_written(WrittenMark {
            fingerprint: fingerprint_content(&content),
            change_count: 7,
            written_at: t0,
        });
        assert_eq!(filter.last_written().map(|m| m.change_count), Some(7));

        // 防线 3（500ms 静默窗）：一律回声，不看指纹
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(
            filter.classify(at(100), Some(fingerprint_content(&content))),
            EchoVerdict::Echo,
            "静默窗内同内容"
        );
        assert_eq!(
            filter.classify(at(100), Some(fingerprint_content(&other_content))),
            EchoVerdict::Echo,
            "静默窗内不同内容也视为回声（可接受假阴性）"
        );
        assert_eq!(
            filter.classify(at(100), None),
            EchoVerdict::Echo,
            "静默窗内无指纹证据同样视为回声"
        );
        assert_eq!(
            filter.classify(at(500), Some(fingerprint_content(&other_content))),
            EchoVerdict::Echo,
            "静默窗含右端点（elapsed == 500ms 仍判回声）"
        );
        assert_eq!(
            filter.classify(at(501), Some(fingerprint_content(&other_content))),
            EchoVerdict::External,
            "501ms 起静默窗失效（异内容即外部变化）"
        );

        // 防线 1（2s 指纹窗）：主判据 = 内容一致
        assert_eq!(
            filter.classify(at(600), Some(fingerprint_content(&content))),
            EchoVerdict::Echo,
            "静默窗外、2s 窗内同内容 → 回声"
        );
        assert_eq!(
            filter.classify(at(600), Some(fingerprint_content(&other_content))),
            EchoVerdict::External,
            "静默窗外、2s 窗内不同内容 → 外部变化"
        );
        assert_eq!(
            filter.classify(at(600), None),
            EchoVerdict::External,
            "静默窗外、无指纹证据 → 外部变化"
        );

        // 2s 窗外：即便内容一致也放行（docs/04 §5 已接受的反向假阴性只覆盖窗内）
        assert_eq!(
            filter.classify(at(2_000), Some(fingerprint_content(&content))),
            EchoVerdict::Echo,
            "指纹窗含右端点（elapsed == 2s 同内容仍判回声）"
        );
        assert_eq!(
            filter.classify(at(2_001), Some(fingerprint_content(&content))),
            EchoVerdict::External,
            "2s+1ms 同内容即外部变化（用户再复制应正常同步）"
        );
        assert_eq!(
            filter.classify(at(2_100), Some(fingerprint_content(&content))),
            EchoVerdict::External,
            "2s 窗外同内容 → 外部变化（用户再次复制应正常同步）"
        );

        // classify_content 便捷入口与 classify 指纹入口等价。
        assert_eq!(
            filter.classify_content(at(600), Some(&content)),
            EchoVerdict::Echo,
            "classify_content 同内容判回声"
        );
        assert_eq!(
            filter.classify_content(at(600), Some(&other_content)),
            EchoVerdict::External,
            "classify_content 异内容判外部"
        );
        assert_eq!(
            filter.classify_content(at(600), None),
            EchoVerdict::External,
            "classify_content 无内容判外部"
        );
    }

    /// 无基线时一切变化都是外部（监听器启动后尚未本地写入的常态）。
    #[test]
    fn no_baseline_means_external() {
        let filter = EchoFilter::default();
        assert_eq!(
            filter.classify(Instant::now(), Some(fingerprint("anything"))),
            EchoVerdict::External
        );
        assert_eq!(filter.last_written(), None);
    }

    /// 每次本地写入覆盖基线（连续两次自写场景）。
    #[test]
    fn note_written_replaces_baseline() {
        let mut filter = EchoFilter::default();
        let t0 = Instant::now();
        filter.note_written(WrittenMark {
            fingerprint: fingerprint("第一份"),
            change_count: 1,
            written_at: t0,
        });
        filter.note_written(WrittenMark {
            fingerprint: fingerprint("第二份"),
            change_count: 2,
            written_at: t0,
        });
        let mark = filter.last_written().expect("应有基线");
        assert_eq!(mark.fingerprint, fingerprint("第二份"));
        assert_eq!(mark.change_count, 2);
        assert_ne!(
            filter.classify(t0 + Duration::from_millis(600), Some(fingerprint("第一份"))),
            EchoVerdict::Echo,
            "旧基线内容不再判回声（已被新写入覆盖）"
        );
    }

    /// trait 接线对拍（design.md 测试策略）：core `MockClipboard` 的语义与
    /// [`ClipboardWatch`] 契约一致——实现方（mac 平台 / 未来其他平台 / mock）
    /// 在同一接口下可互换（mock 只依赖 proto/std，impl 落在本 crate 测试侧，
    /// docs/08 §1 依赖图不新增 core → clipboard 方向）。
    #[test]
    fn mock_clipboard_satisfies_clipboard_watch_semantics() {
        struct MockWatch(MockClipboard);
        impl ClipboardWatch for MockWatch {
            fn change_count(&self) -> u64 {
                self.0.change_count()
            }
            fn has_changes_since(&self, seen: u64) -> bool {
                self.0.has_changes_since(seen)
            }
        }

        let mut mock = MockClipboard::new();
        let mut watch = MockWatch(mock.clone());
        assert_eq!(watch.change_count(), 0, "初始无变更");
        assert!(!watch.has_changes_since(0));

        mock.set_text("hello");
        watch = MockWatch(mock.clone());
        assert_eq!(watch.change_count(), 1, "一次写入递增一次");
        assert!(watch.has_changes_since(0), "seen 之前有写入 → 触发");
        assert!(!watch.has_changes_since(1), "已消费的变更不再触发");

        mock.set_content(vec![1, 2, 3]);
        watch = MockWatch(mock);
        assert_eq!(watch.change_count(), 2);
        assert!(watch.has_changes_since(1));
    }
}
