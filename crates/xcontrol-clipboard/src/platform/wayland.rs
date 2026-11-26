//! Linux Wayland 剪贴板实现（docs/05 §4 剪贴板行）：**wl-clipboard-rs 直依赖**——
//! copy（写入）/ paste（读取）/ watch（监听）三线 API（FR-4.1 文本 / FR-4.2 图片 /
//! FR-4.3 HTML；11-19-m5-clipboard-full）。
//!
//! # API 选型与版本依据（docs/05 §4 的落地裁定）
//!
//! - **wl-clipboard-rs 而非 arboard**：Wayland 的 mime 体系与 wire 格式天然同构
//!   （`image/png`/`text/html`/`text/plain;charset=utf-8` 原样字节承载）——直依赖
//!   使图片/HTML 走 **mime 字节直通**（无 RGBA↔PNG 转换、无 CF_HTML 式包装），
//!   只有回声指纹仍需 PNG 解码（RGBA 规范形，[`crate::png_codec`]）。arboard 的
//!   `wayland-data-control` feature 不采用：其读写 API 与 x11 后端共享 RGBA 形态
//!   （白引入 image crate 转换层），且无监听能力。
//! - **版本 = 0.9.5（docs.rs + crates.io 查证，2026-11-19）**：watch 模块
//!   （[`watch::Watcher`]，zwlr/ext_data_control 事件驱动监听）2026-09 落 master、
//!   0.9.5 首发——0.9.3 及以下**无 watch**，docs/05 §4 的事件驱动监听不可达。
//!   协议支持：ext-data-control 优先、wlr-data-control 兜底（0.9.2 起）。
//! - **纯 Rust Wayland 客户端**（`native_lib`/`dlopen` feature 不开——wayland-backend
//!   Rust 实现）；传递树 wayland-client/backend/protocols(-wlr) 与 gpui 传递树同版
//!   合并，零 unsafe（crate 内 `deny(unsafe_code)`）。
//! - 签名逐项对 registry 源码 0.9.5 核实：`copy::Options::copy_multi(Vec<MimeSource>)`
//!   （`Source::Bytes(Box<[u8]>)` + `MimeType::Specific(String)`）、
//!   `paste::get_contents(ClipboardType, Seat, MimeType) -> (PipeReader, String)`、
//!   `paste::get_mime_types_ordered(ClipboardType, Seat) -> Vec<String>`、
//!   `watch::Watcher::new(ClipboardType, Seat)` → `next_event() ->
//!   Result<Option<ClipboardEvent>, Error>`（首调报告当前 selection 状态）→
//!   `cancel_handle()`（跨线程停机）。
//!
//! # 关键坑位（实测/源码核实）
//!
//! - **`trim_newline` 默认 true**：copy 对 `Source::Bytes` 文本**默认裁尾换行**
//!   （wl-clipboard-rs 源码 make_source）——改变字节 → 回声指纹必然失配。写路径
//!   一律 `Options::trim_newline(false)`（字节直通语义）。
//! - **copy 非 foreground 模式起 serve 线程**（0.9.3 起内存数据、无 fork——
//!   多线程进程安全）：`copy_multi` 内部起线程持有 data source 响应 paste 请求
//!   （常驻语义由 compositor + serve 线程承载，进程退出后 KDE Klipper 等剪贴板
//!   管理器接管；wlroots 无管理器时内容随进程丢失——docs/04 §4 同 X11 的提示
//!   语义）。
//! - **`ClipboardEvent::Changed` 首调即达**：Watcher::new 后第一次 next_event
//!   报告当前 selection 状态（非"等待下一次变化"）——计数基线取 start 时刻，
//!   首事件计入观测（与 X11"不监听时计数恒 0"语义一致）。
//!
//! # 监听模型（docs/04 §4 Wayland 行）
//!
//! 专用线程：`watch::Watcher::new(Regular, Unspecified)` → 就绪握手（回传
//! [`watch::CancelHandle`]）→ `next_event()` 循环（`Changed{mime_types, offer}`/
//! `Cleared` → 计数递增 + mpsc 投递；`Ok(None)` = cancel 停机；`Err` = 连接死亡
//! → 退出）。停机 = [`WaylandWatcher::stop`]/Drop 置 cancel → join——**drop 后
//! 线程必然退出，无泄漏**。ClipboardWatch 计数 = 监听线程观测事件数（X11 后端
//! 同款语义；Wayland 无全局 changeCount 对应物）。
//!
//! # 线程纪律（architecture-rules 红线 3）
//!
//! 监听线程只做「事件过滤 → 计数递增 → mpsc 投递」；剪贴板读写（copy/paste 各
//! 自建 Wayland 连接）不在监听线程做——消费方收到 [`ClipChange`] 后自行调用
//! 读取路径。
//!
//! # 能力探测（SSH 远端实测入口，docs/05 §4 实测配方）
//!
//! [`clipboard_mime_types`] 列当前剪贴板 offered mime（paste 连接即探协议可用性，
//! 缺 data-control 协议时 `Error::MissingProtocol` 干净上抛）；[`WaylandWatcher::new`]
//! 成功即 data-control 监听能力凭证。GNOME（无 data-control）场景的 ashpd
//! clipboard portal 降级路径为移交项（docs/05 §4 GNOME 行）。
//!
//! # 远端实测配方与结果（本机非 Linux 时的验证路径）
//!
//! ```bash
//! # lsdark-eos（KDE Plasma Wayland 会话，WAYLAND_DISPLAY=wayland-1；rsync 同步法
//! # 同 platform/x11.rs 模块文档）
//! ssh … 'cd … && XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 \
//!   cargo test -p xcontrol-clipboard --features wayland'
//! # 写剪贴板冒烟（round-trip + 监听 + 回声判定；写系统剪贴板 #[ignore]）：
//! ssh … 'cd … && XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 \
//!   cargo test -p xcontrol-clipboard --features wayland -- --ignored --nocapture'
//! ```
//!
//! 实测结论见 docs/05 §4 修订记录（任务完成时回填）。
//!
//! # 已知限制（M5 范围，报告/移交项）
//!
//! - **Primary selection 不同步**（P2，docs/04 §4）：只走 Regular clipboard
//!   （与 X11 后端同语义）。
//! - **进程退出内容丢失提示**：KDE Klipper 在场时接管；wlroots 无管理器时丢失
//!   （docs/04 §4 用户提示属 UI 层）。
//! - **GNOME 降级**：data-control 不支持的 compositor → `MissingProtocol` 错误
//!   上抛，上层提示降级（ashpd portal 为移交项）。

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use url::Url;
use wl_clipboard_rs::copy::{
    self as copy_api, MimeSource, MimeType as CopyMimeType, Options as CopyOptions, Source,
};
use wl_clipboard_rs::paste::{
    self as paste_api, ClipboardType as PasteClipboardType, MimeType as PasteMimeType,
    Seat as PasteSeat,
};
use wl_clipboard_rs::watch::{self as watch_api, CancelHandle};

use crate::error::ClipboardError;
use crate::{
    ClipFormatPayload, ClipboardContent, ClipboardWatch, FormatRead, WrittenMark,
    fingerprint_bytes, fingerprint_content, format_read_from,
};

/// `text/plain;charset=utf-8`（docs/04 §2 线格式 mime；KDE/wlroots 通用）。
const MIME_TEXT: &str = "text/plain;charset=utf-8";
/// `image/png`（docs/04 §2 图片交换唯一必选格式）。
const MIME_PNG: &str = "image/png";
/// 文件引用清单 mime（FR-7.3，docs/05 §0.2；URI 列表 = 行分隔 `file://` URL，
/// 注释行以 `#` 开头——X11/GNOME/KDE 通用约定）。
const MIME_URI_LIST: &str = "text/uri-list";
/// `text/html`（docs/04 §2 HTML fragment）。
const MIME_HTML: &str = "text/html";

// ============================================================
// WaylandClipboard：读写（wl-clipboard-rs copy/paste，mime 字节直通）
// ============================================================

/// Wayland 剪贴板句柄：copy/paste 读写 + 变化计数共享槽（FR-4.1/4.2/4.3）。
///
/// 本结构体无内部连接（每次读写独立建 Wayland 连接——wl-clipboard-rs 的会话
/// 生命周期按操作粒度承载）；`new()` 仅为 trait 形状对齐（恒成功）。
/// 常驻持有语义由 copy 的 serve 线程（模块文档）+ compositor/剪贴板管理器承载。
pub struct WaylandClipboard {
    /// 变化事件计数（与 [`WaylandWatcher`] 共享；监听线程每次 selection 变化
    /// 递增。Wayland 无全局 changeCount，X11 后端同款语义）。
    counter: Arc<AtomicU64>,
}

impl WaylandClipboard {
    /// 初始化（wl-clipboard-rs 按操作建连接，此处无失败路径；失败集中在读写/
    /// watch 时上抛 [`ClipboardError::Platform`]）。
    ///
    /// # Errors
    /// 本实现恒成功（保留 Result 形状以对齐 [`crate::open_platform_clipboard`]
    /// 工厂签名）。
    pub fn new() -> Result<Self, ClipboardError> {
        Ok(Self {
            counter: Arc::new(AtomicU64::new(0)),
        })
    }

    /// 启动 data-control 监听线程（与 `self` 共享变化计数；docs/04 §4 Wayland
    /// 写入文件引用清单（FR-7.3 挂板面，docs/05 §0.2；2026-11-27 b-平台补齐）：
    /// URI 列表（行分隔 `file://` URL）→ `text/uri-list` mime **直通**（零转换，
    /// 同 PNG 路径——模块文档）。
    ///
    /// 路径 → `file://` URL 编码经 `Url::from_file_path`（percent-encoding 由
    /// url crate 承担——中文/空格名安全）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：空清单 / Wayland 复制失败。
    pub fn write_file_urls(&mut self, paths: &[PathBuf]) -> Result<WrittenMark, ClipboardError> {
        if paths.is_empty() {
            return Err(ClipboardError::Platform {
                action: "clipboard-write-file-urls",
                reason: "空清单（无文件引用可挂板）".into(),
            });
        }
        let mut uri_list = String::new();
        let mut canonical: Vec<(String, bool)> = Vec::with_capacity(paths.len());
        for path in paths {
            let url = Url::from_file_path(path).map_err(|()| ClipboardError::Platform {
                action: "clipboard-write-file-urls",
                reason: format!("非绝对路径无法转 file URL：{}", path.display()),
            })?;
            uri_list.push_str(url.as_str());
            uri_list.push('\n');
            canonical.push((path.to_string_lossy().into_owned(), path.is_dir()));
        }
        self.copy_bytes(
            uri_list.as_bytes(),
            MIME_URI_LIST,
            "clipboard-write-file-urls",
        )?;
        let mut digest_input = Vec::new();
        for (path, is_dir) in &canonical {
            digest_input.extend_from_slice(&path.len().to_le_bytes());
            digest_input.extend_from_slice(path.as_bytes());
            digest_input.push(u8::from(*is_dir));
        }
        let change_count = self.counter.load(Ordering::Relaxed);
        Ok(WrittenMark {
            fingerprint: fingerprint_bytes(&digest_input),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取文件引用清单（FR-7.3 检测面，docs/05 §0.2）：`text/uri-list` mime
    /// 直通读回 → 解析 `file://` URL → 本机路径。无该 mime → `Ok(None)` 常态。
    /// 超过 `max_entries` 截断（docs/02 §4.6 防护规则 1 的平台侧配合）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接失败 / URL 解析失败。
    pub fn read_file_urls(
        &mut self,
        max_entries: usize,
    ) -> Result<Option<Vec<PathBuf>>, ClipboardError> {
        let bytes = match Self::paste_bytes(
            PasteMimeType::Specific(MIME_URI_LIST),
            u64::MAX,
            "clipboard-read-file-urls",
        ) {
            Ok(bytes) => bytes,
            Err(ClipboardError::Platform { reason, .. }) if reason == "CLIPBOARD_EMPTY" => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        for line in String::from_utf8_lossy(&bytes.unwrap_or_default()).lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue; // 注释行（uri-list 规范允许）
            }
            let Ok(url) = Url::parse(line) else {
                continue; // 非 URL 行：跳过（uri-list 允许注释外文本行）
            };
            if url.scheme() != "file" {
                continue;
            }
            match url.to_file_path() {
                Ok(path) => {
                    paths.push(path);
                    if paths.len() >= max_entries {
                        break;
                    }
                }
                Err(()) => continue, // 非 file 形态（远程 URL 等）：跳过
            }
        }
        if paths.is_empty() {
            return Ok(None);
        }
        Ok(Some(paths))
    }

    /// 监听路径）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接失败 / data-control 协议缺失
    /// （GNOME 等降级场景）/ 无 seat / 监听线程创建失败。
    pub fn watch(&self) -> Result<WaylandWatcher, ClipboardError> {
        WaylandWatcher::start(Arc::clone(&self.counter))
    }

    /// 写入 UTF-8 文本到 Regular clipboard（`text/plain;charset=utf-8`）并返回
    /// 回声抑制基线（docs/04 §5 防线 1/3 的写入自标记）。
    ///
    /// 字节直通（LF 原生，docs/04 §2）；`trim_newline(false)` 钉死（模块文档
    /// 关键坑位）。消费方应把返回的 [`WrittenMark`] 交给
    /// [`crate::EchoFilter::note_written`]。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接/复制失败。
    pub fn write_text(&mut self, text: &str) -> Result<WrittenMark, ClipboardError> {
        self.copy_bytes(text.as_bytes(), MIME_TEXT, "clipboard-write-text")?;
        let change_count = self.counter.load(Ordering::Relaxed);
        Ok(WrittenMark {
            fingerprint: fingerprint_content(&ClipboardContent {
                text: Some(text.to_owned()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 写入图片（FR-4.2）：wire PNG 字节 → `image/png` mime **直通**（零转换
    /// ——Wayland mime 体系与 wire 同构，模块文档）并返回回声抑制基线。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接/复制失败。
    pub fn write_image_png(&mut self, png: &[u8]) -> Result<WrittenMark, ClipboardError> {
        self.copy_bytes(png, MIME_PNG, "clipboard-write-image")?;
        let change_count = self.counter.load(Ordering::Relaxed);
        Ok(WrittenMark {
            fingerprint: fingerprint_content(&ClipboardContent {
                image_png: Some(png.to_vec()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 写入 HTML fragment（FR-4.3）：`text/html` mime **直通**（零包装）并返回
    /// 回声抑制基线（fragment 字节 round-trip 等价）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接/复制失败。
    pub fn write_html(&mut self, html: &str) -> Result<WrittenMark, ClipboardError> {
        self.copy_bytes(html.as_bytes(), MIME_HTML, "clipboard-write-html")?;
        let change_count = self.counter.load(Ordering::Relaxed);
        Ok(WrittenMark {
            fingerprint: fingerprint_content(&ClipboardContent {
                html: Some(html.to_owned()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// copy 路径公共封装：单 mime 字节源（`trim_newline(false)` 钉死）。
    fn copy_bytes(
        &self,
        bytes: &[u8],
        mime: &str,
        action: &'static str,
    ) -> Result<(), ClipboardError> {
        let mut options = CopyOptions::new();
        // 字节直通（关键坑位：默认 true 会裁尾换行 → 指纹失配，模块文档）。
        options.trim_newline(false);
        copy_api::copy_multi(
            options,
            vec![MimeSource {
                source: Source::Bytes(bytes.to_vec().into_boxed_slice()),
                mime_type: CopyMimeType::Specific(mime.to_owned()),
            }],
        )
        .map_err(|error| ClipboardError::Platform {
            action,
            reason: error.to_string(),
        })
    }

    /// paste 路径公共封装：按 mime 读回并限长（超限即停读——**不缓冲无界内容**，
    /// docs/04 §6 资源纪律；`Take(max+1)` 使超限可判定且不截断外发）。
    fn paste_bytes(
        mime: PasteMimeType<'_>,
        max_bytes: u64,
        action: &'static str,
    ) -> Result<Option<Vec<u8>>, ClipboardError> {
        let (reader, _actual_mime) =
            paste_api::get_contents(PasteClipboardType::Regular, PasteSeat::Unspecified, mime)
                .map_err(|error| match error {
                    // 空剪贴板 / 无匹配 mime → 常态（FR-4.1），不上抛
                    paste_api::Error::ClipboardEmpty | paste_api::Error::NoMimeType => {
                        ClipboardError::Platform {
                            action,
                            reason: "CLIPBOARD_EMPTY".into(),
                        }
                    }
                    other => ClipboardError::Platform {
                        action,
                        reason: other.to_string(),
                    },
                })?;
        let mut limited = reader.take(max_bytes.saturating_add(1));
        let mut buffer = Vec::new();
        if limited.read_to_end(&mut buffer).is_err() {
            return Err(ClipboardError::Platform {
                action,
                reason: "mime 数据管道读取失败".into(),
            });
        }
        let actual_len = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if actual_len > max_bytes {
            // 超限信号走 FormatRead::Oversize（此处以 Err 哨兵上抛长度）
            return Err(ClipboardError::Platform {
                action,
                reason: format!("OVERSIZE:{actual_len}"),
            });
        }
        Ok(Some(buffer))
    }

    /// 读取文本并按上限归一化（FR-4.1 空剪贴板常态 + FR-4.5 超限信号）。
    ///
    /// `MimeType::Text`（UTF-8 优先的文本 mime 启发式，wl-clipboard-rs 内建）；
    /// 空剪贴板 / 无文本 mime → [`FormatRead::NoContent`] 常态；超过 `max_bytes`
    /// → [`FormatRead::Oversize`]（不截断不外发）。非 UTF-8 文本 mime 内容 →
    /// [`ClipboardError::Platform`]（线格式契约 UTF-8，docs/04 §2）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接失败 / 非 UTF-8 内容 / 超限哨兵。
    pub fn read_text(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        let bytes = match Self::paste_bytes(PasteMimeType::Text, max_bytes, "clipboard-read-text") {
            Ok(bytes) => bytes,
            Err(ClipboardError::Platform { reason, .. })
                if reason == "CLIPBOARD_EMPTY" || reason.starts_with("OVERSIZE:") =>
            {
                return Ok(Self::normalize_signal(reason, max_bytes));
            }
            Err(error) => return Err(error),
        };
        let Some(bytes) = bytes else {
            return Ok(FormatRead::NoContent);
        };
        let text = String::from_utf8(bytes).map_err(|error| ClipboardError::Platform {
            action: "clipboard-read-text",
            reason: format!("text mime 内容非 UTF-8：{error}"),
        })?;
        Ok(format_read_from(
            Some(ClipFormatPayload::Text(text)),
            max_bytes,
        ))
    }

    /// 读取图片（FR-4.2）：`image/png` mime 直通读回（wire 格式即原生 mime，
    /// **零转换**）并按上限归一化。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接失败 / 超限哨兵。
    pub fn read_image_png(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        let bytes = match Self::paste_bytes(
            PasteMimeType::Specific(MIME_PNG),
            max_bytes,
            "clipboard-read-image",
        ) {
            Ok(bytes) => bytes,
            Err(ClipboardError::Platform { reason, .. })
                if reason == "CLIPBOARD_EMPTY" || reason.starts_with("OVERSIZE:") =>
            {
                return Ok(Self::normalize_signal(reason, max_bytes));
            }
            Err(error) => return Err(error),
        };
        Ok(format_read_from(
            bytes.map(ClipFormatPayload::ImagePng),
            max_bytes,
        ))
    }

    /// 读取 HTML fragment（FR-4.3）：`text/html` mime 直通读回并按上限归一化。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接失败 / 非 UTF-8 内容 / 超限哨兵。
    pub fn read_html(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        let bytes = match Self::paste_bytes(
            PasteMimeType::Specific(MIME_HTML),
            max_bytes,
            "clipboard-read-html",
        ) {
            Ok(bytes) => bytes,
            Err(ClipboardError::Platform { reason, .. })
                if reason == "CLIPBOARD_EMPTY" || reason.starts_with("OVERSIZE:") =>
            {
                return Ok(Self::normalize_signal(reason, max_bytes));
            }
            Err(error) => return Err(error),
        };
        let Some(bytes) = bytes else {
            return Ok(FormatRead::NoContent);
        };
        let html = String::from_utf8(bytes).map_err(|error| ClipboardError::Platform {
            action: "clipboard-read-html",
            reason: format!("text/html 内容非 UTF-8：{error}"),
        })?;
        Ok(format_read_from(
            Some(ClipFormatPayload::Html(html)),
            max_bytes,
        ))
    }

    /// 超限哨兵归一化：`OVERSIZE:<len>` → [`FormatRead::Oversize`]，空态 →
    /// [`FormatRead::NoContent`]（paste_bytes 的哨兵协议还原为信号类型）。
    fn normalize_signal(reason: String, _max_bytes: u64) -> FormatRead {
        if let Some(actual_len) = reason
            .strip_prefix("OVERSIZE:")
            .and_then(|len| len.parse::<u64>().ok())
        {
            FormatRead::Oversize { actual_len }
        } else {
            FormatRead::NoContent
        }
    }
}

impl ClipboardWatch for WaylandClipboard {
    /// 观测到的变化事件计数（监听线程递增；无监听时恒 0，模块文档）。
    fn change_count(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    /// `seen` 之后观测到过变化（M1 mock 语义同款判定）。
    fn has_changes_since(&self, seen: u64) -> bool {
        self.change_count() > seen
    }
}

// ============================================================
// WaylandWatcher：data-control 事件线程（CancelHandle 停机）
// ============================================================

/// 一次剪贴板变化事件（监听线程投递；docs/04 §4「变化方主动推送」触发源）。
///
/// 只带计数快照不带内容：剪贴板读写不在监听线程做（回调线程纪律）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipChange {
    /// 事件投递时的变化计数快照（监听线程递增后的值）。
    pub change_count: u64,
}

/// data-control 监听器：专用线程 + `watch::Watcher::next_event` 循环（事件
/// 驱动，docs/05 §4；与 tokio 的桥接由上层拉取）。
///
/// 生命周期：[`WaylandWatcher::stop`]（显式）或 `Drop`（兜底）都是
/// `CancelHandle::cancel()` → join——**drop 后线程必然退出，无泄漏**；连接死亡
/// （compositor 退出）时 `next_event` 返回 Err，线程自行退出。
pub struct WaylandWatcher {
    receiver: Receiver<ClipChange>,
    cancel_handle: Option<CancelHandle>,
    worker: Option<JoinHandle<()>>,
}

impl WaylandWatcher {
    /// 启动监听线程（连接 → data-control 协议校验 → 就绪握手（回传
    /// CancelHandle）→ `next_event` 循环）。
    ///
    /// `counter` 与 [`WaylandClipboard`] 共享（[`WaylandClipboard::watch`] 接线）
    /// ——监听线程每次 selection 变化递增。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：Wayland 连接失败 / data-control 协议缺失
    /// / 无 seat / 线程在就绪前退出。
    pub fn start(counter: Arc<AtomicU64>) -> Result<Self, ClipboardError> {
        let (sender, receiver) = channel::<ClipChange>();
        let (ready_tx, ready_rx) = channel::<Result<CancelHandle, String>>();

        let worker = std::thread::Builder::new()
            .name("xcontrol-clipboard-watch".into())
            .spawn(move || run_watch_thread(ready_tx, sender, counter))
            .map_err(|error| ClipboardError::Platform {
                action: "watcher-spawn",
                reason: format!("监听线程创建失败：{error}"),
            })?;

        // 就绪握手：Watcher 构造成功（data-control 协议可用凭证）后才放行。
        let ready = ready_rx.recv().map_err(|_| ClipboardError::Platform {
            action: "watcher-start",
            reason: "监听线程在就绪前退出（panic 或提前 return）".into(),
        })?;
        let cancel_handle = ready.map_err(|reason| ClipboardError::Platform {
            action: "wayland-watch-setup",
            reason,
        })?;

        Ok(Self {
            receiver,
            cancel_handle: Some(cancel_handle),
            worker: Some(worker),
        })
    }

    /// 显式停止：`CancelHandle::cancel()` → `next_event` 返回 `Ok(None)` →
    /// 线程退出 → join。
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

    /// 关停公共路径（[`WaylandWatcher::stop`] 与 [`Drop`] 共用）。
    fn shutdown(&mut self) -> Result<(), ClipboardError> {
        // cancel → 阻塞中的 next_event 被唤醒并返回 Ok(None)（watch 0.9.5 契约）
        if let Some(handle) = self.cancel_handle.take() {
            handle.cancel();
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| ClipboardError::Platform {
                action: "watcher-stop",
                reason: "监听线程 panic".into(),
            })?;
        }
        Ok(())
    }
}

impl Drop for WaylandWatcher {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也 cancel + join（幂等；join 失败在
        // Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

/// 监听线程主体：`watch::Watcher::new` → 就绪握手 → `next_event` 循环 →
/// cancel 后退出。
///
/// 回调（迭代）纪律（platform-code.md）：线程内只做「事件过滤 → 计数递增 →
/// mpsc 投递」，零剪贴板读写、零 tokio、零大对象分配。`Changed`/`Cleared` 都
/// 计入变化（Cleared = 剪贴板清空，消费方读取得 NoContent 不外发——语义与
/// X11 owner 消失一致）。
fn run_watch_thread(
    ready_tx: Sender<Result<CancelHandle, String>>,
    sender: Sender<ClipChange>,
    counter: Arc<AtomicU64>,
) {
    let mut watcher =
        match watch_api::Watcher::new(watch_api::ClipboardType::Regular, PasteSeat::Unspecified) {
            Ok(watcher) => watcher,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
    // 就绪：data-control 协议可用（Watcher::new 成功 = 协议与 seat 均通过）。
    if ready_tx.send(Ok(watcher.cancel_handle())).is_err() {
        // 宿主已放弃：直接退出（watcher Drop 断开 Wayland 连接）。
        return;
    }

    // 事件循环：Changed/Cleared → 计数递增 + 投递；cancel → Ok(None) 退出；
    // 连接死亡 → Err 退出（join 不挂死）。
    loop {
        match watcher.next_event() {
            Ok(Some(_event)) => {
                let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
                // 接收端已关闭（监听器被丢弃/关闭中）→ 线程退出（send 失败静默）。
                if sender
                    .send(ClipChange {
                        change_count: count,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(None) => break, // cancel 停机（shutdown 契约）
            Err(_) => break,   // Wayland 连接死亡（compositor 退出）
        }
    }
}

// ============================================================
// 能力探测（SSH 远端实测入口；docs/05 §4 GNOME 降级探测）
// ============================================================

/// 列当前剪贴板 offered mime 列表（保持源端顺序，wl-clipboard-rs 0.9.5
/// `get_mime_types_ordered`）。
///
/// 探测语义：连接成功 = data-control 协议可用；协议缺失 →
/// `Error::MissingProtocol`（GNOME 降级场景的干净信号，docs/05 §4）。空剪贴板
/// → 空列表（连接仍成功）。
///
/// # Errors
/// [`ClipboardError::Platform`]：Wayland 连接失败 / 协议缺失。
pub fn clipboard_mime_types() -> Result<Vec<String>, ClipboardError> {
    paste_api::get_mime_types_ordered(PasteClipboardType::Regular, PasteSeat::Unspecified).map_err(
        |error| ClipboardError::Platform {
            action: "clipboard-probe",
            reason: error.to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use xcontrol_proto::clip::ClipAnnounce;

    use super::*;
    use crate::{EchoFilter, EchoVerdict};

    /// Wayland 会话可用性（连接类测试的容错前提；CI/无 Wayland 环境跳过而非
    /// 失败——X11 后端 display_available 同款）。
    fn wayland_available() -> bool {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            return false;
        };
        let display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_default();
        let display = if display.is_empty() {
            // 默认 socket 名（wayland-client 解析语义）
            std::ffi::OsString::from("wayland-0")
        } else {
            display
        };
        std::path::Path::new(&runtime).join(&display).exists()
    }

    /// 空态读取常量化（paste 哨兵还原）：无 Wayland 会话时哨兵路径的纯逻辑面。
    #[test]
    fn oversize_signal_normalization() {
        assert_eq!(
            WaylandClipboard::normalize_signal("OVERSIZE:2048".into(), 1024),
            FormatRead::Oversize { actual_len: 2048 },
            "超限哨兵还原为信号"
        );
        assert_eq!(
            WaylandClipboard::normalize_signal("CLIPBOARD_EMPTY".into(), 1024),
            FormatRead::NoContent,
            "空态哨兵还原为常态"
        );
        assert_eq!(
            WaylandClipboard::normalize_signal("别的错误".into(), 1024),
            FormatRead::NoContent,
            "未知哨兵保守归一化为无内容（真实错误不走此路径）"
        );
    }

    /// ClipboardWatch 判定面（计数共享槽语义，X11 counter_watch_semantics 同构）。
    #[test]
    fn counter_watch_semantics() {
        let clipboard = WaylandClipboard::new().expect("wayland 后端初始化（无连接路径）");
        assert_eq!(clipboard.change_count(), 0, "初始无变更");
        assert!(!clipboard.has_changes_since(0), "未监听时计数恒 0");

        // 模拟监听线程递增（run_watch_thread 的 fetch_add 语义）。
        clipboard.counter.store(1, Ordering::Relaxed);
        assert_eq!(clipboard.change_count(), 1);
        assert!(clipboard.has_changes_since(0));
        assert!(!clipboard.has_changes_since(1));
    }

    /// 监听生命周期（连接类）：start（协议可用凭证）→ cancel stop 干净 join。
    /// 无 Wayland 会话时跳过（CI 无 wayland socket）。
    #[test]
    fn watcher_start_stop_lifecycle() {
        if !wayland_available() {
            println!("跳过：无 WAYLAND_DISPLAY socket（CI/无 Wayland 环境）");
            return;
        }
        let clipboard = WaylandClipboard::new().expect("初始化");
        let mut watcher = clipboard
            .watch()
            .expect("监听线程应启动（data-control 可用）");
        // 首调报告当前 selection 状态（模块文档）——此处只验证线程存活与停机。
        std::thread::sleep(Duration::from_millis(80));
        let mut events = Vec::new();
        watcher.drain(&mut events);
        watcher.stop().expect("cancel 停机应干净 join");
    }

    /// Drop 兜底关停：start 后直接丢弃，线程必须退出（防泄漏，PRD 验收项）。
    #[test]
    fn watcher_drop_joins_thread() {
        if !wayland_available() {
            println!("跳过：无 WAYLAND_DISPLAY socket（CI/无 Wayland 环境）");
            return;
        }
        let clipboard = WaylandClipboard::new().expect("初始化");
        let watcher = clipboard.watch().expect("监听线程应启动");
        drop(watcher); // Drop::shutdown cancel + join；panic 会体现在测试失败
    }

    /// 能力探测（连接类）：mime 列表读取（空剪贴板 → 空列表；有内容 → 含
    /// 已知 mime）。远端实测的证据入口（模块文档配方）。
    #[test]
    fn capability_probe_mime_types() {
        if !wayland_available() {
            println!("跳过：无 WAYLAND_DISPLAY socket（CI/无 Wayland 环境）");
            return;
        }
        let mimes = clipboard_mime_types().expect("data-control 探测应成功");
        println!("当前剪贴板 offered mime（顺序保持）：{mimes:?}");
    }

    /// 冒烟测试互斥：写**同一个** Wayland 剪贴板的 ignored 测试串行（macos
    /// 后端 CLIPBOARD_SMOKE_LOCK 同款理由）。
    static CLIPBOARD_SMOKE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 原剪贴板恢复守卫：drop 即恢复备份（panic unwind 路径同样执行）——
    /// 测试 payload 不留在用户剪贴板（macos 后端同款）。原为非文本时清空并说明
    /// （Wayland 本层只回读文本 mime；图片/HTML 原样恢复不可达属已知限制）。
    struct RestoreGuard {
        clipboard: WaylandClipboard,
        original: FormatRead,
    }

    impl Drop for RestoreGuard {
        fn drop(&mut self) {
            let result = match &self.original {
                FormatRead::Payload(ClipFormatPayload::Text(text)) => {
                    self.clipboard.write_text(text)
                }
                _ => {
                    // Wayland 无"清空"直连 API（copy 空源不可达）——写空格规避
                    // 测试 payload 残留；真实场景由 compositor 清空语义承载。
                    self.clipboard.write_text(" ")
                }
            };
            match (&self.original, result) {
                (FormatRead::Payload(ClipFormatPayload::Text(_)), Ok(_)) => {
                    println!("已恢复原剪贴板文本内容")
                }
                (FormatRead::Payload(ClipFormatPayload::Text(_)), Err(error)) => {
                    println!("恢复原剪贴板失败：{error}")
                }
                (_, Ok(_)) => println!("原剪贴板为空/非文本：已写入占位空格（Wayland 无清空 API）"),
                (_, Err(error)) => println!("清理剪贴板失败（原内容为空/非文本）：{error}"),
            }
        }
    }

    /// 文本 round-trip + 监听检测 + 回声判定全链路（#[ignore]：**写系统剪贴板**，
    /// 人工运行 `cargo test -p xcontrol-clipboard --features wayland -- --ignored
    /// --nocapture`；远端实测配方见模块文档）。
    ///
    /// 写入（mime 直通，trim_newline(false)）→ 监听线程收到 selection 变化
    /// （计数推进）→ 读取还原 → EchoFilter 防线 3/1 回声判定。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features wayland -- --ignored --nocapture"]
    fn write_watch_echo_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut clipboard = WaylandClipboard::new().expect("初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: WaylandClipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let watcher = clipboard.watch().expect("监听线程启动");
        let before = clipboard.change_count();

        let payload = "xcontrol Wayland 冒烟 αβγ\nsecond line";
        let mark = clipboard.write_text(payload).expect("写入成功");

        // 写入 → compositor selection 变化 → data-control 事件 → 计数推进
        // （宽限 2s 覆盖 compositor 事件调度）。
        let deadline = Instant::now() + Duration::from_secs(2);
        while clipboard.change_count() <= before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            clipboard.change_count() > before,
            "自身写入应触发 selection 变化（计数 {} > 基线 {before}）",
            clipboard.change_count()
        );

        // 读取还原（round-trip，FR-4.1；mime 字节直通逐字节一致）。
        match clipboard.read_text(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64) {
            Ok(FormatRead::Payload(ClipFormatPayload::Text(text))) => {
                assert_eq!(text, payload, "round-trip 还原（LF 直通）")
            }
            other => panic!("写入后应读到同文本，得到 {other:?}"),
        }

        // EchoFilter 复用判定（验收点：纯逻辑区跨平台复用——wayland 平台层直接喂）。
        let mut filter = EchoFilter::default();
        filter.note_written(mark.clone());
        let payload_content = ClipboardContent {
            text: Some(payload.to_owned()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            filter.classify_content(Instant::now(), Some(&payload_content)),
            EchoVerdict::Echo,
            "静默窗内自身写入判回声（防线 3）"
        );
        std::thread::sleep(crate::WRITE_SILENCE_WINDOW + Duration::from_millis(100));
        let now = Instant::now();
        assert_eq!(
            filter.classify_content(now, Some(&payload_content)),
            EchoVerdict::Echo,
            "静默窗外同内容仍判回声（防线 1 指纹一致）"
        );
        let other_content = ClipboardContent {
            text: Some("不同的内容".into()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            filter.classify_content(now, Some(&other_content)),
            EchoVerdict::External,
            "静默窗外不同内容判外部变化（轮询会真实外发的路径）"
        );

        watcher.stop().expect("显式 stop 干净 join");
    }

    /// 图片/HTML round-trip（#[ignore]：写系统剪贴板，人工运行同上；FR-4.2/4.3
    /// Wayland 直通路径验证 + 远端 KDE 能力实测项）。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features wayland -- --ignored --nocapture"]
    fn image_html_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut clipboard = WaylandClipboard::new().expect("初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: WaylandClipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        // 图片：wire PNG 直通——**字节级等价**（mime 直通无重编码，与 mac/x11
        // 的像素级等价判据不同，模块文档）。
        let (width, height) = (4u32, 3u32);
        let rgba: Vec<u8> = (0..width * height)
            .flat_map(|i| {
                [
                    (i % 251) as u8,
                    (i / 3 % 251) as u8,
                    (i / 7 % 251) as u8,
                    255,
                ]
            })
            .collect();
        let wire_png = crate::png_codec::encode_rgba_to_png(&rgba, width, height).expect("编码");
        clipboard.write_image_png(&wire_png).expect("图片写入成功");
        match clipboard.read_image_png(u64::MAX).expect("图片读取失败") {
            FormatRead::Payload(ClipFormatPayload::ImagePng(read_png)) => {
                assert_eq!(read_png, wire_png, "image/png mime 直通字节级等价");
            }
            other => panic!("写入图片后应读到 PNG 载荷，得到 {other:?}"),
        }
        // 指纹规范形判据（RGBA 解码路径跨平台一致；tag+len 信封布局见
        // content_fingerprint_input——image-only 内容的规范形字节 = 2 || len ||
        // RGBA 规范形）。
        let canonical =
            crate::png_codec::png_canonical_fingerprint_input(&wire_png).expect("规范形");
        let mut expected_input = vec![2u8];
        expected_input.extend_from_slice(
            &u32::try_from(canonical.len())
                .expect("长度 u32")
                .to_be_bytes(),
        );
        expected_input.extend_from_slice(&canonical);
        assert_eq!(
            fingerprint_content(&ClipboardContent {
                image_png: Some(wire_png.clone()),
                ..ClipboardContent::default()
            }),
            crate::fingerprint_bytes(&expected_input),
            "image-only 指纹 = tag+len 信封包 RGBA 规范形"
        );

        // HTML：fragment 直通——字节级等价（零包装，模块文档）。
        let payload = "<b>xcontrol Wayland 冒烟</b> <i>αβγ</i>";
        clipboard.write_html(payload).expect("HTML 写入成功");
        match clipboard.read_html(u64::MAX).expect("HTML 读取失败") {
            FormatRead::Payload(ClipFormatPayload::Html(html)) => {
                assert_eq!(html, payload, "text/html mime 直通字节级等价")
            }
            other => panic!("写入 HTML 后应读到同 fragment，得到 {other:?}"),
        }
    }
}
