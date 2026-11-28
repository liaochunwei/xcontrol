//! macOS 剪贴板实现（docs/05 §1、ADR-0003）：**arboard 读写** + **objc2-app-kit
//! `NSPasteboard.changeCount` 原生轮询与 public.html 挂载**两条 API 线并存
//! （FR-4.1 文本 / FR-4.2 图片 / FR-4.3 HTML）。
//!
//! # API 分工（design.md + 11-19-m5-clipboard-full 扩展）
//!
//! - **文本读写**（FR-4.1，M2 起）：[`arboard`]——高层封装稳定，mac 内部即
//!   NSPasteboard（惰性数据提供机制）；空剪贴板/非文本 →
//!   `Error::ContentNotAvailable` → [`FormatRead::NoContent`] 常态（FR-4.1），
//!   不上升到错误。
//! - **图片读写**（FR-4.2，M5）：arboard `image-data` feature（RGBA 内存形态），
//!   wire PNG 字节 ↔ RGBA 的转换在公共 [`crate::png_codec`]：写入 =
//!   decode PNG → RGBA → arboard `set().image()`（内部构造 NSImage，
//!   writeObjects 落 TIFF 表示）；读取 = arboard `get().image()`（TIFF →
//!   RGBA）→ encode PNG。**PNG 字节经 NSImage/TIFF round-trip 必然重编码**
//!   （压缩器非规范）——图片 round-trip 的等价判据是**像素级**（8bit RGBA
//!   逐字节一致），不是 PNG 字节级；回声指纹因此取 RGBA 规范形（docs/04 §5，
//!   [`crate::png_codec`] 模块文档）。透明度例外：NSImage/TIFF 对 alpha 的
//!   预乘处理可能改变透明像素值（不透明图片恒无损）——防线 1 在该场景可能
//!   放行一次自回声（防线 2 origin 去重兜底，有界冗余，docs/04 §5）。
//! - **HTML 写入**（FR-4.3，M5）：**不经 arboard**（其 `set_html` 会把
//!   fragment 包进 `<html><head><meta …></head><body>…</body></html>` 模板，
//!   读回形态 ≠ 写入 fragment，破坏防线 1 的 round-trip 不动点）——直接
//!   objc2-app-kit `clearContents()` + `setString_forType(html,
//!   NSPasteboardTypeHTML)`，fragment 原样落板（public.html，UTF-8）。
//! - **HTML 读取**（FR-4.3，M5）：arboard `get().html()`（内部
//!   `stringForType(NSPasteboardTypeHTML)`）——本进程写入时读到原样 fragment
//!   （写入未包模板），第三方应用复制时读到其原始 HTML。
//! - **监听**（docs/04 §4）：`NSPasteboard.changeCount` 轮询——mac 无系统剪贴板
//!   变化通知 API，arboard 也不暴露 changeCount，只能原生读计数（rustdesk 同款
//!   333ms 量级，本仓取 250ms 可配）。轮询线程只做「计数采样 → 变化判定 →
//!   `std::sync::mpsc` 投递」，**剪贴板读写不在轮询线程做**（回调线程纪律，
//!   platform-code.md）——消费方收到 [`ClipChange`] 后自行调用
//!   [`MacClipboard::read_text`] 等读取路径。
//!
//! # 权限现实
//!
//! 剪贴板读写与 changeCount 采样**无需辅助功能授权**（与输入层不同，docs/05 §6
//! 权限对照表不含剪贴板行）——本模块全部 API 在未授权环境照常工作。
//!
//! # 防回环接线（docs/04 §5 防线 1 + 防线 3）
//!
//! [`MacClipboard::write_text`] / [`MacClipboard::write_image_png`] /
//! [`MacClipboard::write_html`] 返回 [`WrittenMark`]（内容指纹 + 写入后
//! changeCount 基准 + 时刻），消费方交给公共代码的 [`crate::EchoFilter`] 判定
//! 回声；指纹按 [`crate::fingerprint_content`] 规范形（图片 = RGBA 像素不动点，
//! 见上）。本模块只负责"标记与采样"，判定逻辑在平台无关区（mock 集成测试可复用）。
//!
//! # API 版本依据（design.md：以 docs.rs 实际为准）
//!
//! 绑定版本：`arboard 3.7`（default-features off + `image-data`；`Clipboard::new ->
//! Result<Self, Error>`、`get_text/set_text/get().html()/get().image()/
//! set().image()`——html/image 签名对 arboard 3.7.0 源码逐项核实）+
//! `objc2-app-kit 0.3`（default-features off + 仅 `std`/`NSPasteboard` feature；
//! 签名 2026-11-17 逐项核实于 docs.rs：`generalPasteboard() ->
//! Retained<NSPasteboard>` **无 MainThreadMarker 参数**、
//! `changeCount(&self) -> NSInteger`、NSPasteboard 的 `extern_class!` **未标记
//! MainThreadOnly**——后台轮询线程合法；M5 补核实 `clearContents() -> NSInteger`、
//! `setString_forType(&NSString, &NSPasteboardType) -> bool`、
//! `NSPasteboardTypeHTML: &NSPasteboardType` 均为**安全绑定**（objc2 0.6 生成，
//! 无 unsafe fn）——public.html 挂载零 unsafe，M2 预留的 `allow(unsafe_code)`
//! 实际未被 M5 消耗（更优裁定）。`NSString::from_str` 经 objc2-foundation
//! （NSString feature）。
//! objc2 系为纯 Rust ObjC bindings（无 C 编译步骤）；本模块零自声明 extern。
//!
//! # unsafe 纪律（docs/08 §2）
//!
//! workspace `unsafe_code = deny` 在本模块文件级放行（源码属性优先于 lint 配置，
//! 同 input 先例）；每个 unsafe 块附 `// SAFETY:` 不变量注释。M5 实况：本模块
//! 当前实际零 unsafe（objc2 0.6 安全绑定覆盖全部所需 selector）——放行保留为
//! 平台层纪律（后续平台 API 若只能 unsafe 到达时使用）。

#![allow(unsafe_code)] // docs/08 §2：平台绑定层是唯一预期 unsafe 的位置；SAFETY 注释逐块强制。
// M2/M5 实况零 unsafe（objc2 0.6 将本模块用到的 selector 全部绑定为安全方法；
// M2 曾预留给 M5 HTML 挂载的 msg_send unsafe 未被消耗——objc2-app-kit 0.3.2 的
// setString_forType 是安全绑定，见模块文档 API 版本依据）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use objc2::ClassType;
use objc2::rc::autoreleasepool;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeHTML, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::error::ClipboardError;
use crate::{
    ClipFormatPayload, ClipboardContent, ClipboardWatch, FormatRead, WrittenMark,
    fingerprint_bytes, fingerprint_content, format_read_from,
};

/// 默认轮询间隔：250ms（docs/04 §4；macOS 无剪贴板通知 API，rustdesk 同款
/// 333ms 量级——250ms 满足 FR-4.1「复制后 <200ms 可粘贴」的端到端预算中
/// 监听环节的份额，可经 [`ClipboardWatcher::start`] 配置）。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 进程级 NSPasteboard 访问互斥（2026-11-27 双机实测教训）：AppKit 的
/// `NSPasteboard` **非线程安全**——本 crate 的读路径分布在多个后台线程
/// （clip 泵的 arboard 读取 / file 检测器的 read_file_urls / 监听线程的
/// changeCount 采样），Finder 大混合板（文件 + PNG 预览 + 20 余种格式）上
/// 并发读取触发 AppKit 内部死锁（实测：双端 9s 心跳互死，进程存活但全线程
/// park）。全部 NSPasteboard 访问点持此锁串行化——临界区为毫秒级读写。
pub(crate) fn pasteboard_gate() -> &'static std::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Mutex::new(()))
}

// ============================================================
// changeCount 采样（docs/04 §4 监听原语）
// ============================================================

/// 采样系统通用剪贴板的变化计数（`NSPasteboard.generalPasteboard.changeCount`）。
///
/// 计数单调不减（任何进程写入都会递增）；失败兜底返回 0（轮询线程以此保持
/// 上次基准，不误报变化）。剪贴板采样无需任何系统授权（docs/05 §6）。
#[must_use]
pub fn ns_change_count() -> u64 {
    // objc2-app-kit 0.3.2（objc2 0.6 生成绑定，签名对 docs.rs 核实）把这两个
    // selector 绑定为**安全方法**——消息分发带 nil 兜底、`changeCount` 返回
    // NSInteger 整数、`generalPasteboard` 返回进程级单例的 Retained 对象
    // （Apple 文档契约恒非 nil），均无内存安全隐患，故无需 unsafe 块。
    // 线程约束：NSPasteboard 非 MainThreadOnly（extern_class! 未标注），
    // 本函数可在任意线程调用（轮询线程，docs/04 §4）。
    // method_family = none（绑定声明），Retained 所有权语义正确。
    let _gate = pasteboard_gate()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ns_change_count_locked()
}

/// changeCount 采样（**调用方已持 [`pasteboard_gate`] 的内部形态**）：
/// `write_file_urls` 持 gate 后取写入基线必须走本变体——原实现直接调
/// [`ns_change_count`] 同线程二次 lock 同一把非重入 Mutex = 无条件自死锁
/// （2026-11-28 macos 集成测试卡死采样定位：mount_list → write_file_urls →
/// ns_change_count → __psynch_mutexwait；真机 Mac 作为**接收方**挂板同样
/// 必死，近期未暴露只因测试都是 Mac 当发送方）。
fn ns_change_count_locked() -> u64 {
    let count = NSPasteboard::generalPasteboard().changeCount();
    // NSInteger = isize；changeCount 恒 >= 0（内部版本号语义），负值只可能是
    // 平台异常——兜底 0 保持基准不动。
    u64::try_from(count).unwrap_or(0)
}

// ============================================================
// MacClipboard：读写（arboard）+ ClipboardWatch trait 实现
// ============================================================

/// macOS 剪贴板句柄：arboard 读写 + changeCount 监听原语（FR-4.1 文本路径）。
///
/// 换行语义（docs/04 §2）：macOS 原生 LF 直通，读写均不做 CRLF 转换
/// （Windows 平台层才需要）；内容指纹按 UTF-8 字节计算，与线格式一致。
pub struct MacClipboard {
    clipboard: arboard::Clipboard,
}

impl MacClipboard {
    /// 初始化（arboard 内部建立 NSPasteboard 会话）。
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

    /// 写入 UTF-8 文本并返回回声抑制基线（docs/04 §5 防线 1/3 的写入自标记）。
    ///
    /// 消费方应把返回的 [`WrittenMark`] 交给
    /// [`crate::EchoFilter::note_written`]：本进程写入后轮询线程必然检测到
    /// changeCount 推进（写入同步递增），基线缺失会把自身回声当外部变化外发
    /// （回环风暴的起点）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：NSPasteboard 写入失败。
    pub fn write_text(&mut self, text: &str) -> Result<WrittenMark, ClipboardError> {
        self.clipboard
            .set_text(text)
            .map_err(|error| ClipboardError::Platform {
                action: "clipboard-write",
                reason: error.to_string(),
            })?;
        // 写入后立即采样基准（NSPasteboard 写入同步递增 changeCount；
        // arboard 的 mac 写入路径是 declareTypes + setString，返回即计数已推进）。
        let change_count = ns_change_count();
        Ok(WrittenMark {
            // 指纹统一走 fingerprint_content 规范形（M5 起三格式单一入口；
            // LF 归一化语义与 M2 fingerprint(text) 一致——normalize_to_lf 对
            // LF 原生文本恒等，孤 CR 输入在读回侧同形收敛，round-trip 不动点）。
            fingerprint: fingerprint_content(&ClipboardContent {
                text: Some(text.to_owned()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取文本并按上限归一化（FR-4.1 空剪贴板常态 + FR-4.5 超限信号）。
    ///
    /// 空剪贴板 / 非文本内容 → [`FormatRead::NoContent`]（**常态不是错误**，
    /// FR-4.1）；超过 `max_bytes`（协商上限，FR-4.5 可配）→
    /// [`FormatRead::Oversize`]（携带实际长度，**不截断不外发**，提示由上层 UI
    /// 做）；内容只读一次。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：读取被占用等真实失败（`ContentNotAvailable`
    /// 已归一化为 [`FormatRead::NoContent`]，不在此列）。
    pub fn read_text(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        match self.clipboard.get_text() {
            Ok(text) => Ok(format_read_from(
                Some(ClipFormatPayload::Text(text)),
                max_bytes,
            )),
            // 空剪贴板 / 非文本（如图片）→ 常态（FR-4.1），归一化为 NoContent
            Err(arboard::Error::ContentNotAvailable) => Ok(FormatRead::NoContent),
            Err(error) => Err(ClipboardError::Platform {
                action: "clipboard-read",
                reason: error.to_string(),
            }),
        }
    }

    /// 写入图片（FR-4.2）：wire PNG 字节 → 解码 RGBA → arboard `set().image()`
    /// （NSImage 落板）并返回回声抑制基线。
    ///
    /// 读回路径（[`MacClipboard::read_image_png`]）经 TIFF → RGBA → 重编码 PNG，
    /// **PNG 字节必然不同、像素 round-trip 无损**（不透明图片；透明像素预乘例外
    /// 见模块文档）——指纹取 RGBA 规范形（[`crate::fingerprint_content`]，
    /// [`crate::png_codec`] 模块文档）。
    ///
    /// 消费方应把返回的 [`WrittenMark`] 交给 [`crate::EchoFilter::note_written`]。
    ///
    /// # Errors
    /// [`ClipboardError::ImageFormat`]：PNG 解码失败；
    /// [`ClipboardError::Platform`]：NSPasteboard 写入失败。
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
        self.clipboard
            .set()
            .image(image)
            .map_err(|error| ClipboardError::Platform {
                action: "clipboard-write-image",
                reason: error.to_string(),
            })?;
        let change_count = ns_change_count();
        Ok(WrittenMark {
            fingerprint: fingerprint_content(&ClipboardContent {
                image_png: Some(png.to_vec()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取图片（FR-4.2）：arboard `get().image()`（TIFF → RGBA）→ 编码 PNG
    /// 并按上限归一化。
    ///
    /// 空剪贴板 / 无图片内容 → [`FormatRead::NoContent`] 常态；超过 `max_bytes`
    /// → [`FormatRead::Oversize`]（携带 PNG 字节长度，不截断不外发）。
    ///
    /// # Errors
    /// [`ClipboardError::ImageFormat`]：RGBA → PNG 编码失败；
    /// [`ClipboardError::Platform`]：读取真实失败（`ContentNotAvailable` 已归一化
    /// 为 [`FormatRead::NoContent`]）。
    pub fn read_image_png(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        let image = match self.clipboard.get().image() {
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

    /// 写入 HTML fragment（FR-4.3）：public.html 原生挂载（**不经 arboard**，
    /// 模板包装问题见模块文档）并返回回声抑制基线。
    ///
    /// 落板路径：objc2-app-kit `clearContents()` + `setString_forType(fragment,
    /// NSPasteboardTypeHTML)`——fragment 原样落板（UTF-8，LF 直通），读回
    /// [`MacClipboard::read_html`] 逐字节还原（round-trip 不动点，防线 1 判据
    /// 直接可用）。
    ///
    /// ObjC 调用纪律：整段包 `objc2::rc::autoreleasepool`（platform-code.md
    /// "macOS 剪贴板要点"：M5 起 ObjC 调用必须包池）。
    ///
    /// 消费方应把返回的 [`WrittenMark`] 交给 [`crate::EchoFilter::note_written`]。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：NSPasteboard 写入失败。
    pub fn write_html(&mut self, html: &str) -> Result<WrittenMark, ClipboardError> {
        // NSPasteboard 写入（changeCount 原生轮询同款线程语义：NSPasteboard 非
        // MainThreadOnly，任意线程合法）。autoreleasepool 包整段（ObjC 调用纪律）。
        let success = autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            let html_ns = NSString::from_str(html);
            // SAFETY: NSPasteboardTypeHTML 是 objc2-app-kit 0.3.2 生成绑定的
            // extern static（`&'static NSPasteboardType` 共享引用）——extern
            // static 访问在本工具链属 unsafe（地址稳定性由 AppKit 保证：UTI
            // 常量进程生命周期恒有效）；setString_forType 本身为安全方法。
            // 返回 false 表示类型声明失败（fail loud 上抛）。
            pasteboard.setString_forType(&html_ns, unsafe { NSPasteboardTypeHTML })
        });
        if !success {
            return Err(ClipboardError::Platform {
                action: "clipboard-write-html",
                reason: "NSPasteboard setString_forType(public.html) 返回 false".into(),
            });
        }
        let change_count = ns_change_count();
        Ok(WrittenMark {
            fingerprint: fingerprint_content(&ClipboardContent {
                html: Some(html.to_owned()),
                ..ClipboardContent::default()
            }),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取 HTML fragment（FR-4.3）：arboard `get().html()`（内部
    /// `stringForType(NSPasteboardTypeHTML)`）并按上限归一化。
    ///
    /// 本进程写入（[`MacClipboard::write_html`]）读到原样 fragment；空剪贴板 /
    /// 无 HTML 内容 → [`FormatRead::NoContent`] 常态；超过 `max_bytes` →
    /// [`FormatRead::Oversize`]（不截断不外发）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：读取真实失败（`ContentNotAvailable` 已归一化
    /// 为 [`FormatRead::NoContent`]）。
    pub fn read_html(&mut self, max_bytes: u64) -> Result<FormatRead, ClipboardError> {
        match self.clipboard.get().html() {
            Ok(html) => Ok(format_read_from(
                Some(ClipFormatPayload::Html(html)),
                max_bytes,
            )),
            Err(arboard::Error::ContentNotAvailable) => Ok(FormatRead::NoContent),
            Err(error) => Err(ClipboardError::Platform {
                action: "clipboard-read-html",
                reason: error.to_string(),
            }),
        }
    }

    /// 写入文件引用清单（FR-7.3 挂板面，docs/05 §0.2「挂板」列；2026-11-26
    /// b-平台）：`clearContents()` + `writeObjects(NSURL[])`——NSURL 符合
    /// `NSPasteboardWriting`（objc2-app-kit 0.3.2 生成 `unsafe impl`，见
    /// nspasteboard.rs 的 NSURLNSPasteboardSupport 类目），落板形态 = Finder
    /// 语义的 `public.file-url`（多选 = 多 item），目标应用按本地文件处理。
    ///
    /// [`WrittenMark`] 指纹 = 路径+目录位规范形哈希（文件域独立指纹，与 clip
    /// 三格式指纹互不干扰，docs/04 §8.3 同板多域语义；本 crate 不依赖 core，
    /// 算法本地实现 [`fingerprint_bytes`]）。剪贴板域读取路径（text/png/html）
    /// 读不到文件引用 → 挂板不会触发 clip 域回声，域隔离天然成立。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：空清单 / `writeObjects` 返回 false。
    pub fn write_file_urls(&mut self, paths: &[PathBuf]) -> Result<WrittenMark, ClipboardError> {
        if paths.is_empty() {
            return Err(ClipboardError::Platform {
                action: "clipboard-write-file-urls",
                reason: "空清单（无文件引用可挂板）".into(),
            });
        }
        let _gate = pasteboard_gate()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 规范形（写入基线指纹输入）：路径 lossy 文本 + 目录位，len 前缀帧式
        // 布局防拼接碰撞（同 core 清单指纹惯例）。
        let mut canonical: Vec<(String, bool)> = Vec::with_capacity(paths.len());
        let success = autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            let urls: Vec<_> = paths
                .iter()
                .map(|path| {
                    canonical.push((path.to_string_lossy().into_owned(), path.is_dir()));
                    let ns = NSString::from_str(&path.to_string_lossy());
                    NSURL::fileURLWithPath_isDirectory(&ns, path.is_dir())
                })
                .collect();
            let objects: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = urls
                .iter()
                .map(|url| {
                    let url_ref: &NSURL = url;
                    ProtocolObject::from_ref(url_ref)
                })
                .collect();
            let array = NSArray::from_slice(&objects);
            // writeObjects 为安全绑定（objc2 0.6 生成；返回 false = 落板失败
            // fail loud 上抛，同 write_html 纪律）。
            pasteboard.writeObjects(&array)
        });
        if !success {
            return Err(ClipboardError::Platform {
                action: "clipboard-write-file-urls",
                reason: "NSPasteboard writeObjects 返回 false".into(),
            });
        }
        let mut digest_input = Vec::new();
        for (path, is_dir) in &canonical {
            digest_input.extend_from_slice(&path.len().to_le_bytes());
            digest_input.extend_from_slice(path.as_bytes());
            digest_input.push(u8::from(*is_dir));
        }
        // 本函数已持 gate（389 行）——写入基线必须走 locked 变体（自死锁防线，
        // 见 ns_change_count_locked 文档）。
        let change_count = ns_change_count_locked();
        Ok(WrittenMark {
            fingerprint: fingerprint_bytes(&digest_input),
            change_count,
            written_at: Instant::now(),
        })
    }

    /// 读取文件引用清单（FR-7.3 检测面，docs/05 §0.2「检测」列）：
    /// `readObjectsForClasses(NSURL)` 读出全部 file URL（多选 = 多条目，顺序 =
    /// Finder 复制顺序的 Apple 契约）。无文件引用（空剪贴板 / 文本/图片/HTML
    /// 剪贴板）→ `Ok(None)` 常态——按类读取一步到位，无需类型探测
    /// （`pasteboardTypes` 未进 0.3.2 绑定，且按类过滤语义等价）。
    ///
    /// 超过 `max_entries` 截断（调用方按清单上限语义告警，docs/02 §4.6 防护
    /// 规则 1 的平台侧配合）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：下标越界（usize→u64 防御面）。
    pub fn read_file_urls(
        &mut self,
        max_entries: usize,
    ) -> Result<Option<Vec<PathBuf>>, ClipboardError> {
        let _gate = pasteboard_gate()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            let classes = [NSURL::class()];
            let class_array = NSArray::from_slice(&classes);
            // SAFETY: class_array 仅含 NSURL 类对象（objc2-app-kit 0.3.2 声明
            // `unsafe impl NSPasteboardReading for NSURL`——Apple 契约 NSURL 符合
            // NSPasteboardReading 协议，readObjectsForClasses 按类过滤返回元素
            // 恒为 NSURL 实例）；options = None（默认读取选项，Apple 文档语义）。
            // readObjectsForClasses_options 为 unsafe 绑定（生成器标注泛型要求
            // 无法静态证明）；本调用点的不变量由上述类数组构成保证。
            let read = unsafe { pasteboard.readObjectsForClasses_options(&class_array, None) };
            let Some(array) = read else {
                return Ok(None);
            };
            let count = array.count();
            let take = count.min(max_entries);
            let mut paths = Vec::with_capacity(take);
            for index in 0..take {
                let object = array.objectAtIndex(index);
                let Ok(url) = object.downcast::<NSURL>() else {
                    continue; // 按类过滤后理论不可达；防御面跳过
                };
                let Some(path) = url.path() else {
                    continue; // 非 file 形态 URL（fileURLWithPath 构造恒有 path）
                };
                paths.push(PathBuf::from(path.to_string()));
            }
            Ok(Some(paths))
        })
    }
}

impl ClipboardWatch for MacClipboard {
    /// 当前 `NSPasteboard.changeCount`（docs/04 §4 监听原语）。
    fn change_count(&self) -> u64 {
        ns_change_count()
    }

    /// `seen` 之后发生过写入（M1 mock 语义同款判定）。
    fn has_changes_since(&self, seen: u64) -> bool {
        ns_change_count() > seen
    }
}

// ============================================================
// ClipboardWatcher：changeCount 轮询线程（250ms 可配；std::sync::mpsc 桥接）
// ============================================================

/// 一次剪贴板变化事件（轮询线程投递；docs/04 §4「变化方主动推送」触发源）。
///
/// 只带计数不带内容：剪贴板读写不在轮询线程做（回调线程纪律，platform-code.md），
/// 消费方凭计数走「读文本 → [`crate::EchoFilter`] 回声判定 → 上限判定 → 外发」流程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipChange {
    /// 检测到变化时的 `NSPasteboard.changeCount` 快照。
    pub change_count: u64,
}

/// changeCount 轮询器：后台线程独立运行（250ms 可配），变化事件经
/// `std::sync::mpsc` 投递（回调线程禁 async/tokio，architecture-rules 红线 3，
/// 同 input 捕获线程模式）。
///
/// 生命周期：[`ClipboardWatcher::stop`]（显式）或 `Drop`（兜底）都是
/// 置 stop 标志 → join——**drop 后线程必然退出，无泄漏**。
pub struct ClipboardWatcher {
    receiver: Receiver<ClipChange>,
    /// 轮询线程退出标志（宿主线程写、轮询线程读；单 bool，Relaxed 足够）。
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ClipboardWatcher {
    /// 启动轮询线程。
    ///
    /// `poll_interval` 即变化检测的最大延迟（docs/04 §4 默认 250ms，
    /// [`DEFAULT_POLL_INTERVAL`]）；启动前的既有变化不回放（基准取**本函数
    /// 返回前**的计数——基准在宿主线程同步采样，而非线程内采样：线程调度
    /// 延迟会让"start 返回后立即写入"与首次基准采样竞争，漏报该事件）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：线程创建失败。
    pub fn start(poll_interval: Duration) -> Result<Self, ClipboardError> {
        let (sender, receiver) = channel::<ClipChange>();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop_flag);
        // 基准同步采样（见 docstring）：此行之后宿主线程的写入必然 > 基准
        let seen = ns_change_count();
        let worker = std::thread::Builder::new()
            .name("xcontrol-clipboard-watch".into())
            .spawn(move || watch_loop(sender, stop_for_thread, poll_interval, seen))
            .map_err(|error| ClipboardError::Platform {
                action: "watcher-spawn",
                reason: format!("轮询线程创建失败：{error}"),
            })?;
        Ok(Self {
            receiver,
            stop_flag,
            worker: Some(worker),
        })
    }

    /// 显式停止：置标志 → join（线程最多再跑一个 poll_interval 后退出）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：轮询线程 panic。
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
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| ClipboardError::Platform {
                action: "watcher-stop",
                reason: "轮询线程 panic".into(),
            })?;
        }
        Ok(())
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也关停线程（幂等；join 失败在 Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

/// 轮询线程主体：只做「计数采样 → 变化判定 → mpsc 投递」（零剪贴板读写、
/// 零 async；docs/04 §4 + platform-code.md 回调线程纪律）。
///
/// `seen` 基准由宿主线程在 spawn 前同步采样（[`ClipboardWatcher::start`]），
/// 保证"start 返回之后"的变化必然 > 基准、不因线程调度延迟漏报。
fn watch_loop(
    sender: Sender<ClipChange>,
    stop_flag: Arc<AtomicBool>,
    poll_interval: Duration,
    mut seen: u64,
) {
    while !stop_flag.load(Ordering::Relaxed) {
        // 先睡后查：poll_interval 即变化检测的最大延迟（docs/04 §4）
        std::thread::sleep(poll_interval);
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let current = ns_change_count();
        if current > seen {
            seen = current;
            // 接收端已关闭（监听器被丢弃/关闭中）→ 线程退出（send 失败静默）
            if sender
                .send(ClipChange {
                    change_count: current,
                })
                .is_err()
            {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use xcontrol_proto::clip::ClipAnnounce;

    use super::*;
    use crate::{ClipFormatPayload, EchoFilter, EchoVerdict, FormatRead};

    const SMOKE_NOTE: &str = "本测试会写系统剪贴板；无论成功失败（含断言 panic）都会恢复原内容（原内容非文本时清空并说明）";

    /// 冒烟测试互斥：所有 ignored 测试都写**同一个系统剪贴板**，并行 harness
    /// 下会内容串台（断言互读对方 payload），并发 NSPasteboard 操作还可能触发
    /// ObjC 异常 SIGTRAP（2026-11-17 实测）。锁序列化执行——无论 `--ignored`
    /// 是否带 `--test-threads=1` 都安全。中毒锁直接放行（冒烟语境，前一个
    /// 测试 panic 不应阻塞后续测试的恢复逻辑）。
    static CLIPBOARD_SMOKE_LOCK: Mutex<()> = Mutex::new(());

    /// 原剪贴板恢复守卫：drop 即恢复备份（**panic unwind 路径同样执行**）——
    /// 任何失败都不把测试 payload 留在用户系统剪贴板（2026-11-17 实测教训：
    /// 断言 panic 留下 payload，后续测试备份到脏值链式传播，且占住用户剪贴板）。
    /// Drop 内不 panic：恢复失败只打印说明（冒烟输出即报告；原为非文本时
    /// macOS 无法按原样恢复，清空并说明）。
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

    /// changeCount 采样只读不写：两次采样单调不减、无授权要求（docs/05 §6）。
    #[test]
    fn ns_change_count_is_readonly_and_monotonic() {
        let first = ns_change_count();
        let second = ns_change_count();
        assert!(
            second >= first,
            "changeCount 单调不减（{first} -> {second}）"
        );
        assert!(
            !MacClipboard::new()
                .expect("arboard 初始化")
                .has_changes_since(second),
            "无写入时 seen=最新计数 不触发（读取路径零副作用）"
        );
    }

    /// 默认轮询间隔为 250ms（docs/04 §4）。
    #[test]
    fn default_poll_interval_is_250ms() {
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_millis(250));
    }

    /// 轮询线程生命周期：启动 → 无事件静默 → 显式 stop join（无泄漏）。
    /// 只读剪贴板计数，无需授权、不写剪贴板——默认可跑。
    #[test]
    fn watcher_start_drain_stop_lifecycle() {
        let mut watcher = ClipboardWatcher::start(Duration::from_millis(25))
            .expect("轮询线程应启动（线程创建无授权要求）");
        // 两个短窗口内无本地写入：事件只可能来自本测试进程外的剪贴板活动
        // （并发 CI 极小概率；即便收到也只影响计数快照，不影响生命周期断言）。
        std::thread::sleep(Duration::from_millis(60));
        let mut events = Vec::new();
        watcher.drain(&mut events);
        watcher.stop().expect("显式 stop 应干净 join");
    }

    /// Drop 兜底关停：start 后直接丢弃，线程必须退出（防泄漏，PRD 验收项）。
    #[test]
    fn watcher_drop_joins_thread() {
        let watcher = ClipboardWatcher::start(Duration::from_millis(25)).expect("轮询线程应启动");
        drop(watcher); // Drop::shutdown 置标志 + join；panic 会体现在测试失败
    }

    /// 端到端 round-trip（#[ignore]：**写系统剪贴板**，人工运行
    /// `cargo test -p xcontrol-clipboard --features macos -- --ignored`）。
    ///
    /// 写入 → changeCount 推进（trait 判定）→ 读取还原全链路；任何退出路径
    /// （含断言 panic）经守卫恢复原内容。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features macos -- --ignored"]
    fn write_read_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        println!("{SMOKE_NOTE}");
        let mut clipboard = MacClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        // 守卫先行：此后任何路径离开作用域（含 unwind）都恢复原内容
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let payload = "xcontrol 冒烟 αβγ\nsecond line";
        let mark = clipboard.write_text(payload).expect("写入成功");
        assert!(
            clipboard.has_changes_since(mark.change_count.saturating_sub(1)),
            "写入后计数相对写入前基准应推进"
        );
        match clipboard.read_text(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64) {
            Ok(FormatRead::Payload(ClipFormatPayload::Text(text))) => {
                assert_eq!(text, payload, "round-trip 还原")
            }
            other => panic!("写入后应读到同文本，得到 {other:?}"),
        }
    }

    /// 图片 round-trip（#[ignore]：写系统剪贴板，人工运行同上；FR-4.2 M5 验收项
    /// "mac 图片 round-trip 实测"）。
    ///
    /// wire PNG 写入 → arboard NSImage/TIFF → 读回重编码 PNG → **解码像素逐字节
    /// 等价**（PNG 字节经 NSImage 必然重编码——等价判据是像素级，模块文档）；
    /// 读回 PNG 是合法可再解码的 wire 格式；EchoFilter 指纹判据命中（RGBA
    /// 规范形 round-trip 不动点）。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features macos -- --ignored"]
    fn image_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        println!("{SMOKE_NOTE}");
        let mut clipboard = MacClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        // 8×6 确定性像素（含渐变；不透明——透明像素预乘例外见模块文档）。
        let (width, height) = (8u32, 6u32);
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

        let mark = clipboard.write_image_png(&wire_png).expect("图片写入成功");
        assert!(
            ns_change_count() > mark.change_count.saturating_sub(1),
            "写入后计数相对写入前基准应推进"
        );

        match clipboard
            .read_image_png(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64)
            .expect("图片读取失败")
        {
            FormatRead::Payload(ClipFormatPayload::ImagePng(read_png)) => {
                let (w2, h2, rgba2) =
                    crate::png_codec::decode_png_to_rgba(&read_png).expect("读回 PNG 解码");
                assert_eq!((w2, h2), (width, height), "读回尺寸一致");
                assert_eq!(rgba2, rgba, "像素级 round-trip 等价（8bit RGBA 无损）");
                // 读回 PNG 与写入 PNG 字节不同属预期（NSImage/TIFF 重编码），
                // 两者解码像素一致——指纹规范形判据的直接证据。
                let write_canonical =
                    crate::png_codec::png_canonical_fingerprint_input(&wire_png).expect("规范形");
                let read_canonical =
                    crate::png_codec::png_canonical_fingerprint_input(&read_png).expect("规范形");
                assert_eq!(
                    crate::fingerprint_bytes(&write_canonical),
                    crate::fingerprint_bytes(&read_canonical),
                    "重编码 PNG 的指纹规范形一致（防线 1 判据可用）"
                );
            }
            other => panic!("写入图片后应读到 PNG 载荷，得到 {other:?}"),
        }

        // EchoFilter 判据（图片路径）：写入标记 vs 读回内容指纹一致。
        let mut filter = EchoFilter::default();
        filter.note_written(mark.clone());
        std::thread::sleep(crate::WRITE_SILENCE_WINDOW + Duration::from_millis(100));
        let read_for_echo = clipboard.read_image_png(u64::MAX).expect("回声读取失败");
        let read_content = match read_for_echo {
            FormatRead::Payload(ClipFormatPayload::ImagePng(png)) => ClipboardContent {
                image_png: Some(png),
                ..ClipboardContent::default()
            },
            other => panic!("回声读取应得 PNG 载荷，得到 {other:?}"),
        };
        assert_eq!(
            filter.classify_content(Instant::now(), Some(&read_content)),
            EchoVerdict::Echo,
            "静默窗外图片内容指纹一致判回声（RGBA 规范形）"
        );
    }

    /// HTML round-trip（#[ignore]：写系统剪贴板，人工运行同上；FR-4.3 M5 验收项
    /// "HTML 实测"）。
    ///
    /// fragment 经 public.html 原样落板（objc2 路径，不经 arboard 模板包装——
    /// 模块文档）→ 读回字节等价；EchoFilter 指纹判据命中；另验证 arboard
    /// `get_text` 读同一内容为空（HTML-only 内容不泄漏到文本路径，格式隔离）。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features macos -- --ignored"]
    fn html_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        println!("{SMOKE_NOTE}");
        let mut clipboard = MacClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let payload = "<b>xcontrol 冒烟</b> <i>αβγ</i><p>第二段</p>";
        let mark = clipboard.write_html(payload).expect("HTML 写入成功");
        assert!(
            ns_change_count() > mark.change_count.saturating_sub(1),
            "写入后计数相对写入前基准应推进"
        );

        // 读回字节等价（public.html 原样落板的 round-trip 不动点）。
        match clipboard
            .read_html(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64)
            .expect("HTML 读取失败")
        {
            FormatRead::Payload(ClipFormatPayload::Html(html)) => {
                assert_eq!(html, payload, "HTML fragment 读回字节等价")
            }
            other => panic!("写入 HTML 后应读到同 fragment，得到 {other:?}"),
        }

        // 格式隔离：HTML-only 内容不出现在文本路径（FR-4.1 空剪贴板常态语义）。
        assert_eq!(
            clipboard.read_text(u64::MAX).expect("文本路径读取"),
            FormatRead::NoContent,
            "HTML-only 内容不泄漏到 text/plain 路径"
        );

        // EchoFilter 判据（HTML 路径）。
        let mut filter = EchoFilter::default();
        filter.note_written(mark.clone());
        std::thread::sleep(crate::WRITE_SILENCE_WINDOW + Duration::from_millis(100));
        let html_content = ClipboardContent {
            html: Some(payload.to_owned()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            filter.classify_content(Instant::now(), Some(&html_content)),
            EchoVerdict::Echo,
            "静默窗外 HTML 指纹一致判回声"
        );
        let other_content = ClipboardContent {
            html: Some("<p>别的</p>".into()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            filter.classify_content(Instant::now(), Some(&other_content)),
            EchoVerdict::External,
            "静默窗外异 HTML 判外部变化"
        );
    }

    /// 防回环自标记（#[ignore]：写系统剪贴板，人工运行同上）。
    ///
    /// 本进程写入 → 回声判定命中（不外发语义）；等过静默窗后指纹判据单独验证。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features macos -- --ignored"]
    fn self_write_is_suppressed_as_echo_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        println!("{SMOKE_NOTE}");
        let mut clipboard = MacClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let payload = "xcontrol 回环冒烟（防线 1/3）";
        let mark = clipboard.write_text(payload).expect("写入成功");

        let mut filter = EchoFilter::default();
        filter.note_written(mark.clone());

        // 写入后立即判定：防线 3 静默窗内，无论读到什么都判回声（不外发）
        let payload_content = ClipboardContent {
            text: Some(payload.to_owned()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            filter.classify_content(Instant::now(), Some(&payload_content)),
            EchoVerdict::Echo,
            "静默窗内自身写入判回声"
        );
        // 轮询线程视角：写入推进了计数（seen 取写入前基准即触发）
        assert!(
            ns_change_count() > mark.change_count.saturating_sub(1),
            "计数已推进"
        );
        // 读出的内容与写入一致（防线 1 主判据的证据链）
        match clipboard.read_text(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64) {
            Ok(FormatRead::Payload(ClipFormatPayload::Text(text))) => assert_eq!(text, payload),
            other => panic!("读回应得同文本，得到 {other:?}"),
        }

        // 过静默窗（600ms > 500ms）后：同内容仍判回声（防线 1 指纹判据），
        // 异内容判外部（轮询会真实外发的路径）
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
            "静默窗外不同内容判外部变化"
        );
    }

    /// 轮询检测外部写入（#[ignore]：写系统剪贴板，人工运行同上）。
    ///
    /// watcher 启动（50ms 加速档，验证间隔可配）→ 本进程写入（模拟外部应用
    /// 的真实写入路径相同：任何进程写都推进同一 changeCount）→ drain 收到事件
    /// → stop 干净 join。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features macos -- --ignored"]
    fn watcher_detects_write_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        println!("{SMOKE_NOTE}");
        let mut clipboard = MacClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let mut watcher = ClipboardWatcher::start(Duration::from_millis(50)).expect("轮询线程启动");

        let payload = "xcontrol 轮询冒烟";
        clipboard.write_text(payload).expect("写入成功");

        // 写入 → 轮询线程在一个 interval 内应投递事件（宽限 2s 覆盖调度抖动）
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while events.is_empty() && Instant::now() < deadline {
            watcher.drain(&mut events);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!events.is_empty(), "写入后轮询线程应投递变化事件");
        assert!(
            events.iter().all(|event| event.change_count > 0),
            "事件应携带计数快照"
        );

        watcher.stop().expect("显式 stop 干净 join");
    }

    /// 文件引用 round-trip（#[ignore]：**写系统剪贴板**，人工运行同上；FR-7.3
    /// b-平台验收项「mac 自往返」）。
    ///
    /// 临时文件 write_file_urls → read_file_urls 还原路径全链路；经守卫恢复
    /// 原剪贴板内容。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features macos -- --ignored"]
    fn file_urls_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        println!("{SMOKE_NOTE}");
        let mut clipboard = MacClipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        // 两个临时文件（真实存在——writeObjects 落板不校验存在性，但 Finder
        // 语义要求；read 路径也不校验，round-trip 只看 URL 字符串还原）。
        let dir = std::env::temp_dir().join("xcontrol-file-urls-smoke");
        std::fs::create_dir_all(&dir).expect("临时目录创建");
        let first = dir.join("xcontrol-冒烟-α.txt");
        let second = dir.join("second-β.pdf");
        std::fs::write(&first, b"smoke").expect("临时文件一");
        std::fs::write(&second, b"smoke2").expect("临时文件二");

        let paths = vec![first.clone(), second.clone()];
        let mark = clipboard.write_file_urls(&paths).expect("文件引用写入");
        assert!(
            clipboard.has_changes_since(mark.change_count.saturating_sub(1)),
            "写入后计数相对写入前基准应推进"
        );
        let read_back = clipboard
            .read_file_urls(64)
            .expect("读取不失败")
            .expect("有文件引用");
        assert_eq!(read_back.len(), 2, "多 item 读回");
        // URL path 还原（%20 解码由 NSURL.path 承担；中文文件名直接比对）
        assert_eq!(
            read_back,
            vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned()
            ],
            "round-trip 路径还原"
        );

        // 清理临时面（剪贴板恢复由守卫承接）
        let _ = std::fs::remove_dir_all(&dir);
    }
}
