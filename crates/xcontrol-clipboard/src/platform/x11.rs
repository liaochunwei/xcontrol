//! Linux X11 剪贴板实现（docs/05 §3 剪贴板行）：**arboard 读写（CLIPBOARD
//! selection，常驻持有）** + **XFixes `SelectionNotify` 事件驱动监听**（FR-4.1
//! 文本 / FR-4.2 图片 / FR-4.3 HTML）。
//!
//! # API 分工（design.md + 11-19-m5-clipboard-full 扩展）
//!
//! - **文本读写**（FR-4.1，M3 起）：[`arboard`] X11 后端（UTF8_STRING target；
//!   空剪贴板/非文本 → `Error::ContentNotAvailable` → [`FormatRead::NoContent`]
//!   常态，FR-4.1）。
//! - **图片读写**（FR-4.2，M5）：arboard `image-data` feature——X11 后端直接以
//!   `image/png` target 承载（arboard 3.7 x11 后端 set/get image 内部经 image
//!   crate png 编解码，png feature 已在树）；本层 wire PNG ↔ RGBA 转换仍走公共
//!   [`crate::png_codec`]（与 mac/win 同一规范形判据——X11 路径 arboard 写入
//!   会重编码，读回 PNG 字节 ≠ wire 写入字节、像素无损，图片 round-trip 等价
//!   判据是像素级）。
//! - **HTML 读写**（FR-4.3，M5）：arboard `get().html()` / `set().html()`——
//!   X11 后端以 `text/html` target 承载且**原样存储**（arboard x11 set_html 落
//!   `html.into_owned().into_bytes()`，无模板包装）——fragment 字节 round-trip
//!   等价（与 mac write_html 的 public.html 原样语义一致）。
//! - **监听**（docs/04 §4）：`XFixesSelectSelectionInput`（x11rb `xfixes` feature）
//!   → `SelectionNotify` 事件——事件驱动，监听线程只做「事件过滤 → 本地计数递增
//!   → `std::sync::mpsc` 投递」，**剪贴板读写不在监听线程做**（回调线程纪律，
//!   platform-code.md）——消费方收到 [`ClipChange`] 后自行调用
//!   [`X11Clipboard::read_text`] 等读取路径。
//!
//! # 常驻持有语义（docs/04 §4"常驻持有 selection 响应对端 paste"——arboard 实现
//! 细节核实，2026-11-17 对 arboard 3.7.0 源码）
//!
//! X11 剪贴板是"所有者服务"模型：内容由**最后一个写入它的进程**持有并实时响应
//! 其他客户端的 SelectionRequest（paste 请求）；持有进程退出且无剪贴板管理器
//! 接管时内容即失效（docs/04 §4 的"退出 XControl 会清空 Linux 剪贴板内容"提示）。
//! arboard X11 后端（`platform/linux/x11.rs`）的实现链：
//!
//! 1. `Clipboard::new()` 起一个**专属 serve 线程**（`serve_requests`）：持有专用
//!    服务器窗口、响应全服务器的 SelectionRequest——本进程存活期间 selection
//!    持续可用（**常驻**由 arboard 内部线程承载，`set_text` 立即返回不阻塞）；
//! 2. 高层 `set_text`（`WaitConfig::None` 默认）只写数据 + `set_selection_owner`
//!    ——arboard 的 `SetExtLinux::wait()`（阻塞调用线程永久服务）是**短命进程
//!    的 daemonize 模式**（xclip 式"复制后驻留"），本进程长驻、serve 线程已存在，
//!    **不使用**（用了会把同步层调用线程永久挂死）；
//! 3. **临终转交（尽力而为，arboard 内建）**：最后一个 `Clipboard` 句柄 Drop 时
//!    arboard 调 `ask_clipboard_manager_to_request_our_data`——请求数据给
//!    `CLIPBOARD_MANAGER`（ICCCM 剪贴板管理器协议，KDE/GNOME 的 clipboard
//!    manager 即此），管理器在场则内容移交、否则仍丢失。M5 评估项：移交失败的
//!    提示与"退出前转交 xclip"加固是否必要（docs/04 §4"临终转交尽力而为"的
//!    语义已由 arboard Drop 承载，本模块不重复实现）。
//!
//! # 监听模型：XFixes 单 selection 订阅 + 双连接停机（input 后端先例同款）
//!
//! 1. **工作线程数据连接**（[`X11Watcher::start`] 的工作线程创建）：连接 → 校验
//!    XFIXES 扩展 → `xfixes_query_version`（XFixes 协议要求先协商版本；
//!    SelectSelectionInput 属 v1.0，本绑定按 XML 6.0 协商）→ `intern_atom("CLIPBOARD")`
//!    → 建监听窗口（InputOnly 1×1，不映射——XFixes 事件送客户端、窗口只是关联
//!    句柄）→ `xfixes_select_selection_input(window, CLIPBOARD, SET_SELECTION_OWNER)`
//!    `.check()`（服务器确认凭证）→ 就绪握手（回传窗口 id + 停机 atom）→
//!    `wait_for_event()` 阻塞迭代。
//! 2. **宿主停机连接**（stop 时懒建）：对监听窗口 `send_event` 一条 ClientMessage
//!    （`event_mask = NO_EVENT`——X 协议 SendEvent 契约：空掩码投递给**创建窗口
//!    的客户端**，即工作线程连接；"If the event-mask is the empty set, then the
//!    event is sent to the client that created the destination window"，X11
//!    protocol spec §8）→ 工作线程收到停机 atom → 循环退出 → 销毁窗口 → join。
//!    与 input 的 XRecord 双连接时序同型：XFixes 无"disable 后 EndOfData"的天然
//!    终止信号，ClientMessage 即终止信号；就绪握手在订阅被服务器 check 确认后才
//!    放行——宿主过早 stop 的信号先于订阅到达会丢失、join 挂死（input 后端
//!    XRecord 双连接竞态教训同款防线）。
//!
//!    **双端 sync 时序（远端实测修正，2026-11-17）**：X 服务器对各客户端连接的
//!    请求处理无跨连接顺序保证，flush 只保证写入 socket。两处 sync 钉死时序：
//!    (a) 工作线程退出前 destroy_window + **sync**——destroy 在服务器处理完毕
//!    才释放窗口 id 段，杜绝"下一个监听器连接复用同 id 段的 create_window 与
//!    旧 destroy 竞态"（destroy 后到会连带销毁新窗口，下一次 stop 的
//!    ClientMessage 落空 → join 挂死）；(b) 停机连接 send_event + **sync**——
//!    服务器处理完 SendEvent 才 join，信号投递确定性达成。restart 压测
//!    （[`X11Watcher`] 连续 10 次 start/stop，复用 id 段）远端通过。
//! 3. **订阅掩码 = `SET_SELECTION_OWNER` 单位**：`SELECTION_CLIENT_CLOSE`/
//!    `SELECTION_WINDOW_DESTROY` 位是"持有者死亡"信号——结果要么是剪贴板管理器
//!    接管（产生新 owner，`SET_SELECTION_OWNER` 已覆盖）要么内容清空（协议无
//!    clear 公告语义，[`FormatRead::NoContent`] 不外发），不订阅。
//!
//! # ClipboardWatch 原语（X11 无全局 changeCount 的对应物）
//!
//! X 协议没有"剪贴板全局变化计数"（mac changeCount / Windows 序列号的对应物
//! 不存在）——本模块计数 = **监听线程观测到的 SelectionNotify 事件数**
//! （`Arc<AtomicU64>`，[`X11Clipboard`] 与 [`X11Watcher`] 共享；[`X11Clipboard::watch`]
//! 接线）。不启动监听时计数恒 0（trait"单调不减"契约仍成立——"没有观测就不
//! 有变化"，历史内容由消费方启动同步时全量拉取覆盖）。**异步滞后**：本地写入
//! （arboard set owner）触发的 SelectionNotify 经服务器回环到达，计数推进晚于
//! `write_text` 返回——[`WrittenMark::change_count`] 是诊断字段（不参与
//! [`crate::EchoFilter::classify`] 判定），滞后无害。
//!
//! # Primary 不同步（P2，docs/04 §4）
//!
//! X11 有 Clipboard/Primary/Secondary 三 selection；跨机同步只走 **CLIPBOARD**
//! （显式复制粘贴路径，docs/04 §4"Primary selection（中键粘贴）：v1 不同步"）。
//! 本模块监听与读写均不触 Primary（arboard 默认 selection 即 Clipboard；
//! Primary 经 `GetExtLinux/SetExtLinux::clipboard(LinuxClipboardKind::Primary)`
//! 才可达，本模块不调用）。
//!
//! # 依赖与 unsafe（docs/08 §2）
//!
//! `x11rb` 0.13（纯 Rust bindings，`allow-unsafe-code` 未开则 crate 内
//! `#![forbid(unsafe_code)]`——workspace 登记见根 Cargo.toml；arboard 的 x11rb
//! 传递依赖无 features，并集后 xfixes 单独在树）——本模块**零 unsafe**（无文件级
//! allow，workspace 级 `unsafe_code = deny` 直接生效；arboard 同为安全封装）。
//! API 签名逐项对 registry 源码 0.13.2 核实（2026-11-17）：
//!
//! - `RustConnection::connect(Option<&str>) -> Result<(RustConnection, usize),
//!   ConnectError>`（读 `$DISPLAY` 与 `$XAUTHORITY`）；
//! - `xfixes_select_selection_input(window, selection: Atom, event_mask:
//!   SelectionEventMask) -> VoidCookie`（`SelectionEventMask::SET_SELECTION_OWNER
//!   = 1<<0`）；`xfixes_query_version(u32, u32) -> Cookie<QueryVersionReply>`；
//!   `Event::XfixesSelectionNotify(xfixes::SelectionNotifyEvent)` 变体
//!   （`RustConnection::wait_for_event()` 返回，`selection`/`subtype` 过滤）；
//! - `xproto::send_event(propagate, destination: Into<Window>, event_mask:
//!   EventMask, event: Into<[u8; 32]>)`（ClientMessage 停机信号；
//!   `ClientMessageEvent::new(format=32, window, type_, [u32; 5])`）；
//! - `intern_atom(only_if_exists, name: &[u8])`、`create_window(depth, wid,
//!   parent, ..., class: WindowClass, visual, &CreateWindowAux)`（InputOnly 要求
//!   depth=0/border=0）、`destroy_window`、`generate_id`。
//!
//! # 远端实测配方与结果（本机非 Linux 时的验证路径，m3-input-x11 先例）
//!
//! ```bash
//! # lsdark-eos（KDE Plasma Wayland 会话：Xwayland :1 rootless + 自建 rootful
//! # Xwayland :2；SSH tty 无 DISPLAY，需显式 DISPLAY=:…；:1 还需 XAUTHORITY）
//! rsync -a -e "ssh -i ~/.ssh/gitssh -o BatchMode=yes" \
//!   --exclude target --exclude .git --exclude .trellis ./ \
//!   lsdark@xwork.leopon-corn.ts.net:/media/lsdark/gns/sources/com.aixhc/XControl/
//! # 回环冒烟载体（rootful Xwayland :2，写剪贴板不触用户会话）：
//! ssh … 'XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
//!   setsid /usr/bin/Xwayland :2 -ac >/tmp/xwayland2.log 2>&1 &'
//! # 默认套件（21 项全绿，:1 与 :2 双跑）：
//! ssh … 'cd … && DISPLAY=:1 XAUTHORITY=/run/user/1000/xauth_<…> \
//!   cargo test -p xcontrol-clipboard --features x11'
//! # 写剪贴板冒烟（round-trip + EchoFilter 回声判定 + xclip 外部写入交叉验证；
//! # 只在 :2 跑——写 CLIPBOARD 会影响该 X 服务器内容）：
//! ssh … 'cd … && DISPLAY=:2 cargo test -p xcontrol-clipboard \
//!   --features x11 -- --ignored --nocapture'
//! # 停机时序压测（已在默认套件：watcher_restart_stress，10 次 start/stop 复用
//! # id 段）：
//! # 测后清理：ssh … 'pkill -x Xwayland（按 :2 精确匹配）'
//! ```
//!
//! **实测结论（2026-11-17，lsdark-eos）**：
//!
//! - **监听 round-trip PASS（rootful :2）**：本进程写入 → XFixes SelectionNotify
//!   → 计数推进 → 读回还原 → EchoFilter 防线 3/1 回声判定全链路成立；
//! - **独立客户端 PASS**：xclip 写入 → 外部 SelectionNotify → 本进程经 selection
//!   请求协议读回（"常驻持有响应 paste"的独立客户端交叉验证）；
//! - **时序修正（实测发现并修复）**：连续 start/stop 复用窗口 id 段复现 join
//!   挂死——unsync 的 destroy_window 与下一监听器 create_window 竞态（见上
//!   "双端 sync 时序"），`watcher_restart_stress` 修正后转绿；
//! - 真实 Xorg 会话与剪贴板管理器在场景的对拍属移交项（docs/05 §3.2）。
//!
//! 完整移交清单与实测记录见 docs/05 §3.2。
//!
//! # 已知限制（M3 范围，报告/移交项）
//!
//! - **进程退出内容丢失提示**：无剪贴板管理器的环境退出 XControl 后 CLIPBOARD
//!   无主（上文常驻一节）——docs/04 §4 的用户提示属 UI 层职责（M4/M5 接线）。
//! - **多格式读取**（M5 已实装，11-19-m5-clipboard-full）：文本（UTF8_STRING）/
//!   图片（`image/png` target，arboard image-data）/HTML（`text/html` target）
//!   三格式读写路径已落（本模块 read/write_image_png、read/write_html）；
//!   六向图片矩阵与剪贴板管理器在场的实机对拍仍属移交项（docs/05 §3.2）。
//! - **监听窗口属 data 连接**：窗口由工作线程连接创建（XFixes 事件送达创建者
//!   客户端），stop 的 ClientMessage 经宿主连接投递（双连接，上文）。
//! - **单监听器约定**：同一 [`X11Clipboard`] 并发 `watch()` 两次会对同一计数器
//!   双计——上层保证单实例（win 后端静态槽同款约定）。
//! - **`wait_for_event` join 无超时**：stop 的 ClientMessage 依赖宿主连接可达；
//!   X 服务器死亡时工作线程连接同时报错退出（socket 断开），join 不挂死。与
//!   input 后端"迭代器超时硬化留 M4"同批（docs/05 §3.1 item 6）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::cookie::Cookie;
use x11rb::errors::ConnectionError;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesExt, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, ClientMessageEvent, ConnectionExt as XprotoExt, CreateWindowAux, EventMask, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperExt;

use crate::error::ClipboardError;
use crate::{
    ClipFormatPayload, ClipboardContent, ClipboardWatch, FormatRead, WrittenMark,
    fingerprint_content, format_read_from,
};

// ============================================================
// X11Clipboard：读写（arboard X11 后端，CLIPBOARD selection 常驻持有）
// ============================================================

/// X11 剪贴板句柄：arboard 读写（CLIPBOARD selection）+ 变化计数共享槽（FR-4.1）。
///
/// 常驻语义（docs/04 §4）：持有 arboard `Clipboard` 句柄即持有其内部 serve 线程
/// ——本结构体存活期间 selection 持续响应 paste 请求；Drop（含进程退出）触发
/// arboard 对 `CLIPBOARD_MANAGER` 的临终转交（见模块文档"常驻持有语义"）。
pub struct X11Clipboard {
    clipboard: arboard::Clipboard,
    /// 变化事件计数（与 [`X11Watcher`] 共享；监听线程每次 CLIPBOARD owner 变化
    /// 递增。X11 无全局 changeCount，见模块文档 ClipboardWatch 一节）。
    counter: Arc<AtomicU64>,
}

impl X11Clipboard {
    /// 初始化（arboard X11 后端：建 X 连接 + 起 serve 线程；`$DISPLAY`/`$XAUTHORITY`
    /// 由 arboard → x11rb 解析，docs/05 §6 权限行"$DISPLAY 可用"）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：X 连接失败 / serve 线程创建失败。
    pub fn new() -> Result<Self, ClipboardError> {
        let clipboard = arboard::Clipboard::new().map_err(|error| ClipboardError::Platform {
            action: "clipboard-init",
            reason: error.to_string(),
        })?;
        Ok(Self {
            clipboard,
            counter: Arc::new(AtomicU64::new(0)),
        })
    }

    /// 启动 XFixes SelectionNotify 监听线程（与 `self` 共享变化计数；docs/04 §4
    /// X11 监听路径）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：X 连接失败 / XFIXES 扩展缺失 / 监听注册被拒 /
    /// 就绪握手失败。
    pub fn watch(&self) -> Result<X11Watcher, ClipboardError> {
        X11Watcher::start(Arc::clone(&self.counter))
    }

    /// 写入 UTF-8 文本到 CLIPBOARD selection 并返回回声抑制基线（docs/04 §5
    /// 防线 1/3 的写入自标记）。
    ///
    /// 换行语义（docs/04 §2）：Linux 原生 LF 直通，读写均不做 CRLF 转换（Windows
    /// 平台层才需要）；写入后 serve 线程常驻响应 paste（本结构体存活期间）。
    /// 消费方应把返回的 [`WrittenMark`] 交给 [`crate::EchoFilter::note_written`]。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：写入失败（serve 线程停止/协议失败）。
    pub fn write_text(&mut self, text: &str) -> Result<WrittenMark, ClipboardError> {
        self.clipboard
            .set_text(text)
            .map_err(|error| ClipboardError::Platform {
                action: "clipboard-write",
                reason: error.to_string(),
            })?;
        // 计数基准异步滞后（SelectionNotify 经服务器回环，模块文档）——诊断字段，
        // 不参与 EchoFilter 判定。
        let change_count = self.counter.load(Ordering::Relaxed);
        Ok(WrittenMark {
            // 指纹统一走 fingerprint_content 规范形（M5 单一入口；LF 原生文本
            // normalize_to_lf 恒等，语义与 M3 一致）。
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
    /// 内部是 selection 请求协议往返（UTF8_STRING target，arboard 数秒级超时）；
    /// 超过 `max_bytes`（协商上限，FR-4.5 可配）→ [`FormatRead::Oversize`]（**不截断
    /// 不外发**）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：读取真实失败（`ContentNotAvailable` 已归一化
    /// 为 [`FormatRead::NoContent`]，不在此列）。
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
    /// （X11 后端以 image/png target 承载、内部重编码）并返回回声抑制基线。
    ///
    /// 读回 PNG 字节 ≠ 写入 PNG 字节（arboard 内部 image crate 重编码，模块
    /// 文档），像素 round-trip 无损——指纹取 RGBA 规范形（[`crate::png_codec`]）。
    /// 计数基准异步滞后（同 write_text）。
    ///
    /// # Errors
    /// [`ClipboardError::ImageFormat`]：PNG 解码失败；
    /// [`ClipboardError::Platform`]：写入失败（serve 线程停止/协议失败）。
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

    /// 读取图片（FR-4.2）：arboard `get().image()`（image/png target → RGBA）→
    /// 编码 PNG 并按上限归一化。
    ///
    /// 空剪贴板 / 无图片内容 → [`FormatRead::NoContent`] 常态；超过 `max_bytes`
    /// → [`FormatRead::Oversize`]（PNG 字节口径，不截断不外发）。
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

    /// 写入 HTML fragment（FR-4.3）：arboard `set().html()`（X11 后端 `text/html`
    /// target **原样存储**，无模板包装——模块文档）并返回回声抑制基线。
    ///
    /// fragment 字节 round-trip 等价（防线 1 判据直接可用）；计数基准异步滞后
    /// （同 write_text）。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：写入失败（serve 线程停止/协议失败）。
    pub fn write_html(&mut self, html: &str) -> Result<WrittenMark, ClipboardError> {
        self.clipboard
            .set()
            .html(html, None)
            .map_err(|error| ClipboardError::Platform {
                action: "clipboard-write-html",
                reason: error.to_string(),
            })?;
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

    /// 读取 HTML fragment（FR-4.3）：arboard `get().html()`（`text/html` target
    /// selection 请求往返）并按上限归一化。
    ///
    /// 空剪贴板 / 无 HTML 内容 → [`FormatRead::NoContent`] 常态；超过 `max_bytes`
    /// → [`FormatRead::Oversize`]（不截断不外发）。
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
}

impl ClipboardWatch for X11Clipboard {
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
// X11Watcher：XFixes SelectionNotify 事件线程（双连接停机；input 后端先例同款）
// ============================================================

/// 一次剪贴板变化事件（监听线程投递；docs/04 §4「变化方主动推送」触发源）。
///
/// 只带计数快照不带内容：剪贴板读写不在监听线程做（回调线程纪律，
/// platform-code.md），消费方凭计数走「读文本 → [`crate::EchoFilter`] 回声判定
/// → 上限判定 → 外发」流程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipChange {
    /// 事件投递时的变化计数快照（监听线程递增后的值）。
    pub change_count: u64,
}

/// 就绪握手载荷：监听窗口 id（宿主停机 ClientMessage 的目的地）+ 停机 atom
/// （工作线程连接 intern；X atom 服务器全局共享，宿主连接可复用同一 id）。
struct ReadyPayload {
    window: Window,
    stop_atom: Atom,
}

/// XFixes 监听器：专用线程 + `xfixes_select_selection_input` + `wait_for_event`
/// 循环（事件驱动，docs/05 §3；与 tokio 的桥接由上层拉取）。
///
/// 生命周期：[`X11Watcher::stop`]（显式）或 `Drop`（兜底）都是发停机
/// ClientMessage → join——**drop 后线程必然退出，无泄漏**（连接死亡同样退出：
/// `wait_for_event` 在 socket 断开时返回 Err）。
pub struct X11Watcher {
    receiver: Receiver<ClipChange>,
    /// 停机目的地（监听窗口 + 停机 atom；stop 时消费，Drop 兜底同样消费）。
    stop_target: Option<(Window, Atom)>,
    worker: Option<JoinHandle<()>>,
}

impl X11Watcher {
    /// 启动监听线程（连接 → XFIXES 校验/版本协商 → 建窗 + 订阅 → 就绪握手 →
    /// 事件循环）。
    ///
    /// `counter` 与 [`X11Clipboard`] 共享（[`X11Clipboard::watch`] 接线）——监听
    /// 线程每次 CLIPBOARD owner 变化递增，[`X11Clipboard::change_count`] 读取。
    ///
    /// # Errors
    /// [`ClipboardError::Platform`]：连接/扩展/订阅失败或线程在就绪前退出。
    pub fn start(counter: Arc<AtomicU64>) -> Result<Self, ClipboardError> {
        let (sender, receiver) = channel::<ClipChange>();
        let (ready_tx, ready_rx) = channel::<Result<ReadyPayload, String>>();

        let worker = std::thread::Builder::new()
            .name("xcontrol-clipboard-watch".into())
            .spawn(move || run_xfixes_thread(ready_tx, sender, counter))
            .map_err(|error| ClipboardError::Platform {
                action: "watcher-spawn",
                reason: format!("监听线程创建失败：{error}"),
            })?;

        // 就绪握手：订阅经服务器 check 确认后才放行（input 后端时序教训——宿主
        // 过早 stop 的 ClientMessage 若先于订阅到达，停机信号丢失、join 挂死）。
        let ready = ready_rx.recv().map_err(|_| ClipboardError::Platform {
            action: "watcher-start",
            reason: "监听线程在就绪前退出（panic 或提前 return）".into(),
        })?;
        let ReadyPayload { window, stop_atom } =
            ready.map_err(|reason| ClipboardError::Platform {
                action: "xfixes-setup",
                reason,
            })?;

        Ok(Self {
            receiver,
            stop_target: Some((window, stop_atom)),
            worker: Some(worker),
        })
    }

    /// 显式停止：懒建停机连接 → ClientMessage → join（线程随后销毁监听窗口）。
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

    /// 关停公共路径（[`X11Watcher::stop`] 与 [`Drop`] 共用）。
    ///
    /// 时序保证（远端实测修正，2026-11-17——`watcher_restart_stress` 复现）：
    /// 停机信号发出后以 `sync()`（GetInputFocus 往返）确认**服务器已处理
    /// SendEvent**，再 join——X 服务器对各客户端连接的请求处理无跨连接顺序
    /// 保证，flush 只到 socket 缓冲；不经 sync 直接 join 时信号何时投递不定，
    /// 与工作线程状态（是否已进 wait_for_event）竞态。
    fn shutdown(&mut self) -> Result<(), ClipboardError> {
        // 停机信号：对监听窗口发 ClientMessage（空 event_mask → 投递给窗口创建
        // 客户端 = 工作线程连接，X 协议 SendEvent 契约："If the event-mask is
        // the empty set, then the event is sent to the client that created the
        // destination window"，X11 protocol spec §8）。X 服务器已死时连接失败
        // 静默——工作线程的 wait_for_event 同时报错退出，join 为准。
        if let Some((window, stop_atom)) = self.stop_target.take() {
            // 连接失败仅影响"优雅停机信号"：无法建连接说明服务器已死/环境已坏，
            // 工作线程将自行退出（不在此上抛——停机路径的失败以 join 结果为准）。
            if let Ok((conn, _)) = RustConnection::connect(None) {
                let event = ClientMessageEvent::new(32, window, stop_atom, [0u32; 5]);
                // 停机是尽力而为信号：send 失败静默（工作线程 socket 错误路径兜底；
                // 窗口已在服务器侧不存在 = 服务器即将回收该连接，事件线程随之退出）。
                let _ = conn.send_event(false, window, EventMask::NO_EVENT, event);
                // flush + sync：服务器处理完 SendEvent 才 join（见函数 doc 时序保证）。
                let _ = conn.flush();
                let _ = WrapperExt::sync(&conn);
            }
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

impl Drop for X11Watcher {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也发停机信号并 join（幂等；join 失败
        // 在 Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

/// 校验扩展存在（X11_EXTENSION_NAME 查询；缺失 = 服务器不支持，fail loud）。
/// （input 后端同款辅助；错误文案差异化。）
fn require_xfixes(conn: &RustConnection) -> Result<(), String> {
    if conn
        .extension_information(xfixes::X11_EXTENSION_NAME)
        .map_err(|error| format!("XFIXES 扩展查询失败：{error}"))?
        .is_none()
    {
        return Err("X 服务器不支持 XFIXES 扩展（Xorg/Xwayland 均内置，环境异常）".into());
    }
    Ok(())
}

/// `Cookie` 两步收敛（发送 + 应答）：x11rb 0.13 的应答错误是 `ReplyError`、
/// 发送错误是 `ConnectionError`（两者无互转 `From`，input 后端同款处理）——
/// 统一为 String 诊断文本上抛。
fn flatten_reply<R: x11rb::x11_utils::TryParse>(
    cookie: Result<Cookie<'_, RustConnection, R>, ConnectionError>,
    what: &'static str,
) -> Result<R, String> {
    match cookie {
        Ok(cookie) => cookie
            .reply()
            .map_err(|error| format!("{what} 失败：{error}")),
        Err(error) => Err(format!("{what} 发送失败：{error}")),
    }
}

/// 监听线程主体：连接 → XFIXES 校验/版本协商 → 建窗 + 订阅 → 就绪握手 →
/// `wait_for_event` 循环 → 收尾（销毁窗口）。
///
/// 回调（迭代）纪律（platform-code.md）：线程内只做"事件过滤 → 计数递增 →
/// mpsc 投递"，零剪贴板读写、零 tokio、零大对象分配。
fn run_xfixes_thread(
    ready_tx: Sender<Result<ReadyPayload, String>>,
    sender: Sender<ClipChange>,
    counter: Arc<AtomicU64>,
) {
    let (conn, _) = match RustConnection::connect(None) {
        Ok(pair) => pair,
        Err(error) => {
            let _ = ready_tx.send(Err(format!(
                "X11 连接失败（检查 $DISPLAY 与 $XAUTHORITY，docs/05 §6）：{error}"
            )));
            return;
        }
    };
    if let Err(reason) = require_xfixes(&conn) {
        let _ = ready_tx.send(Err(reason));
        return;
    }
    // XFixes 协议要求先 QueryVersion（协商本绑定 XML 版本 6.0）；SelectSelection
    // Input 属 v1.0，服务器 major >= 1 即满足（现实服务器均 >= 3）。
    let version = match flatten_reply(
        conn.xfixes_query_version(xfixes::X11_XML_VERSION.0, xfixes::X11_XML_VERSION.1),
        "xfixes_query_version",
    ) {
        Ok(reply) => reply,
        Err(reason) => {
            let _ = ready_tx.send(Err(reason));
            return;
        }
    };
    if version.major_version < 1 {
        let _ = ready_tx.send(Err(format!(
            "XFIXES 版本过低（major={}，SelectSelectionInput 需 >= 1）",
            version.major_version
        )));
        return;
    }

    // CLIPBOARD 与停机 atom（X atom 服务器全局——工作线程 intern，宿主复用 id）。
    let clipboard_atom = match flatten_reply(
        conn.intern_atom(false, b"CLIPBOARD"),
        "intern_atom(CLIPBOARD)",
    ) {
        Ok(reply) => reply.atom,
        Err(reason) => {
            let _ = ready_tx.send(Err(reason));
            return;
        }
    };
    let stop_atom = match flatten_reply(
        conn.intern_atom(false, b"XCONTROL_CLIP_WATCH_STOP"),
        "intern_atom(STOP)",
    ) {
        Ok(reply) => reply.atom,
        Err(reason) => {
            let _ = ready_tx.send(Err(reason));
            return;
        }
    };

    // 监听窗口：InputOnly 1×1、不映射（XFixes 事件送客户端，窗口仅关联句柄；
    // InputOnly 约束 depth=0/border=0，见 x11rb create_window 文档）。
    let window = match conn.generate_id() {
        Ok(id) => id,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("generate_id 失败：{error}")));
            return;
        }
    };
    let root = match conn.setup().roots.first() {
        Some(screen) => screen.root,
        None => {
            let _ = ready_tx.send(Err("setup.roots 为空（X 连接异常）".into()));
            return;
        }
    };
    if let Err(error) = conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT, // depth：InputOnly 必须 0
        window,
        root,
        0,
        0, // x/y（不映射，坐标无意义）
        1,
        1, // 1×1
        0, // border_width：InputOnly 必须 0
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT, // visual：CopyFromParent（u32 别名 Visualid）
        &CreateWindowAux::new(),
    ) {
        let _ = ready_tx.send(Err(format!("create_window 失败：{error}")));
        return;
    }

    // 订阅 CLIPBOARD 的 owner 变化（掩码选择依据见模块文档"监听模型"）。
    // check() = 服务器已处理本请求的凭证（就绪握手的核心语义：宿主 stop 的
    // ClientMessage 在此之后到达，停机信号不丢）。x11rb 0.13 无
    // From<ReplyError> for ConnectionError，两条错误路径统一为 ReplyError。
    let subscribed = match conn.xfixes_select_selection_input(
        window,
        clipboard_atom,
        SelectionEventMask::SET_SELECTION_OWNER,
    ) {
        Ok(cookie) => cookie.check(),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = subscribed {
        let _ = conn.destroy_window(window);
        let _ = conn.flush();
        let _ = ready_tx.send(Err(format!("xfixes_select_selection_input 失败：{error}")));
        return;
    }
    if conn.flush().is_err() {
        let _ = ready_tx.send(Err("flush 失败（连接已死）".into()));
        return;
    }

    if ready_tx
        .send(Ok(ReadyPayload { window, stop_atom }))
        .is_err()
    {
        // 宿主已放弃：清理监听窗口后退出。
        let _ = conn.destroy_window(window);
        let _ = conn.flush();
        return;
    }

    // 事件循环：CLIPBOARD owner 变化 → 计数递增 + 投递；停机 ClientMessage →
    // 退出；其余事件忽略。连接死亡（服务器退出）→ Err → 退出（join 不挂死）。
    loop {
        match conn.wait_for_event() {
            Ok(Event::XfixesSelectionNotify(event))
                if event.selection == clipboard_atom
                    && event.subtype == xfixes::SelectionEvent::SET_SELECTION_OWNER =>
            {
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
            Ok(Event::ClientMessage(message))
                if message.type_ == stop_atom && message.window == window =>
            {
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // 收尾：销毁监听窗口（X 窗口资源；连接 Drop 断开服务器侧兜底回收）。
    // destroy 后 **sync（往返）**：确保服务器已处理 destroy 才退出线程——X 服务
    // 器对跨客户端连接的请求无顺序保证，unsync 的 destroy 与"下一个监听器连接
    // 复用被回收的窗口 id 段（同 id create_window）"竞态：destroy 后到，新窗口
    // 被连带销毁 → 下一次 stop 的 ClientMessage 落空（BadWindow，静默）→ join
    // 挂死（`watcher_restart_stress` 远端复现，2026-11-17）。sync 后退出即把
    // destroy 钉进服务器时序，id 复用必在其后。
    let _ = conn.destroy_window(window);
    let _ = conn.flush();
    let _ = WrapperExt::sync(&conn);
}

// ============================================================
// 测试（纯逻辑：trait 判定面 + 生命周期；连接类测试按 $DISPLAY 容错跳过；
// 写剪贴板冒烟 #[ignore]——远端实测配方见模块文档与 docs/05 §3.2）
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use xcontrol_proto::clip::ClipAnnounce;

    use super::*;
    use crate::{ClipFormatPayload, EchoFilter, EchoVerdict, FormatRead};

    /// $DISPLAY 可用性（连接类测试的容错前提；CI/无 X 环境跳过而非失败——
    /// input 后端同款）。
    fn display_available() -> bool {
        match std::env::var_os("DISPLAY") {
            Some(value) => !value.is_empty(),
            None => false,
        }
    }

    /// 冒烟测试互斥：写系统剪贴板的 ignored 测试串行（macos 后端同款理由——
    /// 并行 harness 下内容串台；X11 selection 并发读写可能死锁）。
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

    /// 计数 trait 判定面（连接类：arboard 初始化 + 共享计数器语义，mac mock
    /// 判定同构——`mock_clipboard_satisfies_clipboard_watch_semantics` 的平台侧
    /// 对拍）。无 $DISPLAY 时跳过（CI linux runner 无 X 会话）。
    #[test]
    fn counter_watch_semantics() {
        if !display_available() {
            println!("跳过：无 $DISPLAY（CI/无 X 环境）");
            return;
        }
        let clipboard = X11Clipboard::new().expect("arboard 初始化");
        assert_eq!(clipboard.change_count(), 0, "初始无变更");
        assert!(
            !clipboard.has_changes_since(0),
            "未监听时计数恒 0（模块文档）"
        );

        // 模拟监听线程递增（run_xfixes_thread 的 fetch_add 语义；同模块内直访
        // 共享槽）。
        clipboard.counter.store(1, Ordering::Relaxed);
        assert_eq!(clipboard.change_count(), 1, "监听递增后计数推进");
        assert!(clipboard.has_changes_since(0), "seen 之前有变化 → 触发");
        assert!(!clipboard.has_changes_since(1), "已消费的变更不再触发");
    }

    /// 监听生命周期（连接类）：start → 无事件静默 → 显式 stop join（无泄漏）。
    #[test]
    fn watcher_start_drain_stop_lifecycle() {
        if !display_available() {
            println!("跳过：无 $DISPLAY（CI/无 X 环境）");
            return;
        }
        let clipboard = X11Clipboard::new().expect("arboard 初始化");
        let mut watcher = clipboard.watch().expect("监听线程应启动");
        // 两个短窗口内无本地写入：事件只可能来自本测试进程外的剪贴板活动
        // （并发 CI 极小概率；即便收到也只影响计数快照，不影响生命周期断言）。
        std::thread::sleep(Duration::from_millis(60));
        let mut events = Vec::new();
        watcher.drain(&mut events);
        watcher.stop().expect("显式 stop 应干净 join");
    }

    /// 重启压测（连接类，input 后端 `capture_restart_after_stop_is_clean` 同款回归
    /// 防线）：连续 start/stop 复用被回收的窗口 id 段——停机时序（destroy 后 sync
    /// + 停机信号后 sync）不干净会在此复现"ClientMessage 落空 → join 挂死"。
    #[test]
    fn watcher_restart_stress() {
        if !display_available() {
            println!("跳过：无 $DISPLAY（CI/无 X 环境）");
            return;
        }
        let clipboard = X11Clipboard::new().expect("arboard 初始化");
        for _ in 0..10u32 {
            let watcher = clipboard.watch().expect("监听线程应启动");
            std::thread::sleep(Duration::from_millis(10));
            watcher.stop().expect("stop 干净 join");
        }
    }

    /// Drop 兜底关停：start 后直接丢弃，线程必须退出（防泄漏，PRD 验收项）。
    #[test]
    fn watcher_drop_joins_thread() {
        if !display_available() {
            println!("跳过：无 $DISPLAY（CI/无 X 环境）");
            return;
        }
        let clipboard = X11Clipboard::new().expect("arboard 初始化");
        let watcher = clipboard.watch().expect("监听线程应启动");
        drop(watcher); // Drop::shutdown 停机信号 + join；panic 会体现在测试失败
    }

    /// 端到端 round-trip + 监听检测 + 回声判定全链路（#[ignore]：**写系统剪贴板**，
    /// 人工运行 `cargo test -p xcontrol-clipboard --features x11 -- --ignored
    /// --nocapture`；远端实测配方见模块文档）。
    ///
    /// 写入 → 监听线程收到自身写入的 SelectionNotify（计数推进）→ 读取还原
    /// → EchoFilter 回声判定命中（防线 3 静默窗）→ 静默窗外同内容判回声（防线 1
    /// 指纹）、异内容判外部；任何退出路径（含断言 panic）经守卫恢复原内容。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features x11 -- --ignored --nocapture"]
    fn write_watch_echo_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut clipboard = X11Clipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        // 守卫先行：此后任何路径离开作用域（含 unwind）都恢复原内容
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let watcher = clipboard.watch().expect("监听线程启动");

        let payload = "xcontrol X11 冒烟 αβγ\nsecond line";
        let mark = clipboard.write_text(payload).expect("写入成功");

        // 写入 → arboard set owner → XFixes SelectionNotify → 计数推进（宽限 2s
        // 覆盖 X 服务器回环延迟）。
        let deadline = Instant::now() + Duration::from_secs(2);
        while clipboard.change_count() <= mark.change_count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            clipboard.change_count() > mark.change_count,
            "自身写入应触发 SelectionNotify（计数 {} > 基线 {}）",
            clipboard.change_count(),
            mark.change_count
        );

        // 读取还原（round-trip，FR-4.1）。
        match clipboard.read_text(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64) {
            Ok(FormatRead::Payload(ClipFormatPayload::Text(text))) => {
                assert_eq!(text, payload, "round-trip 还原")
            }
            other => panic!("写入后应读到同文本，得到 {other:?}"),
        }

        // EchoFilter 复用判定（验收点：纯逻辑区跨平台复用——X11 平台层直接喂）。
        let mut filter = EchoFilter::default();
        filter.note_written(mark.clone());
        let payload_content = crate::ClipboardContent {
            text: Some(payload.to_owned()),
            ..crate::ClipboardContent::default()
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
        let other_content = crate::ClipboardContent {
            text: Some("不同的内容".into()),
            ..crate::ClipboardContent::default()
        };
        assert_eq!(
            filter.classify_content(now, Some(&other_content)),
            EchoVerdict::External,
            "静默窗外不同内容判外部变化（轮询会真实外发的路径）"
        );

        watcher.stop().expect("显式 stop 干净 join");
    }

    /// 外部客户端写入检测（#[ignore]：**写系统剪贴板**，人工运行同上）。
    ///
    /// 独立客户端（xclip）写入 CLIPBOARD → 监听线程收到外部 owner 变化的
    /// SelectionNotify（计数推进）→ 本进程读取还原（对端 paste 请求语义的
    /// 独立客户端交叉验证）。xclip 不在 PATH 时跳过（远端配方见模块文档）。
    #[test]
    #[ignore = "写系统剪贴板（xclip 独立客户端，无论成败均恢复原内容）"]
    fn external_write_detected_by_watcher_smoke() {
        if !display_available() {
            println!("跳过：无 $DISPLAY（CI/无 X 环境）");
            return;
        }
        // xclip 可用性（测试依赖独立客户端；缺失即跳过）。
        if std::env::var_os("PATH").is_none_or(|paths| {
            std::env::split_paths(&paths).all(|dir| !dir.join("xclip").is_file())
        }) {
            println!("跳过：xclip 不在 PATH（需独立 X 客户端交叉验证）");
            return;
        }

        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut clipboard = X11Clipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let watcher = clipboard.watch().expect("监听线程启动");
        let before = clipboard.change_count();

        // 独立客户端写入（xclip fork 常驻服务 selection）。
        let payload = "xcontrol 外部客户端写入冒烟";
        let status = std::process::Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                let _ = child
                    .stdin
                    .as_mut()
                    .expect("piped stdin")
                    .write_all(payload.as_bytes());
                child.wait()
            })
            .expect("xclip 启动");
        assert!(
            status.success(),
            "xclip 写入应成功（exit {:?}）",
            status.code()
        );

        // 外部 owner 变化 → SelectionNotify → 计数推进（宽限 2s）。
        let deadline = Instant::now() + Duration::from_secs(2);
        while clipboard.change_count() <= before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            clipboard.change_count() > before,
            "外部写入应触发 SelectionNotify（计数 {} > {}）",
            clipboard.change_count(),
            before
        );

        // 读取（selection 请求协议往返到 xclip 进程——独立客户端持有语义验证）。
        match clipboard.read_text(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64) {
            Ok(FormatRead::Payload(ClipFormatPayload::Text(text))) => {
                assert_eq!(text, payload, "外部内容读取还原")
            }
            other => panic!("外部写入后应读到 xclip payload，得到 {other:?}"),
        }

        // 无本地写入基线 → 外部变化判 External（防线 1/3 的反例面）。
        let filter = EchoFilter::default();
        assert_eq!(
            filter.classify_content(
                Instant::now(),
                Some(&crate::ClipboardContent {
                    text: Some(payload.to_owned()),
                    ..crate::ClipboardContent::default()
                })
            ),
            EchoVerdict::External,
            "无基线时外部写入判外部变化"
        );

        // 清理：xclip 的常驻进程移交（wait 已让主进程退出，服务子进程由守卫的
        // clear 覆盖——arboard clear 使本进程取回 owner 空态）。
        watcher.stop().expect("显式 stop 干净 join");
    }

    /// 图片 round-trip（#[ignore]：写系统剪贴板，人工运行同上；FR-4.2 M5 远端
    /// 实测项，docs/05 §3.2 item 3）。
    ///
    /// wire PNG 写入（arboard set().image → image/png target 内部重编码）→ 读回
    /// 重编码 PNG → 解码像素逐字节等价（PNG 字节经 arboard image crate 重编码
    /// 必然不同——等价判据是像素级，模块文档"图片读写"）。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features x11 -- --ignored --nocapture"]
    fn image_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut clipboard = X11Clipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let (width, height) = (6u32, 4u32);
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
        // X11 计数异步滞后（SelectionNotify 服务器回环）——等待计数推进后再读
        // （同 write_watch_echo_round_trip_smoke 的宽限模式）。
        let deadline = Instant::now() + Duration::from_secs(2);
        while clipboard.change_count() <= mark.change_count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        match clipboard
            .read_image_png(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64)
            .expect("图片读取失败")
        {
            FormatRead::Payload(ClipFormatPayload::ImagePng(read_png)) => {
                let (w2, h2, rgba2) =
                    crate::png_codec::decode_png_to_rgba(&read_png).expect("读回 PNG 解码");
                assert_eq!((w2, h2), (width, height), "读回尺寸一致");
                assert_eq!(rgba2, rgba, "像素级 round-trip 等价（8bit RGBA 无损）");
            }
            other => panic!("写入图片后应读到 PNG 载荷，得到 {other:?}"),
        }
    }

    /// HTML round-trip（#[ignore]：写系统剪贴板，人工运行同上；FR-4.3 M5 远端
    /// 实测项，docs/05 §3.2 item 3）。
    ///
    /// fragment 经 arboard set().html（text/html target 原样存储）→ 读回字节
    /// 等价（无模板包装——模块文档"HTML 读写"）。
    #[test]
    #[ignore = "写系统剪贴板（无论成败均恢复原内容）：cargo test -p xcontrol-clipboard --features x11 -- --ignored --nocapture"]
    fn html_round_trip_smoke() {
        let _serial = CLIPBOARD_SMOKE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut clipboard = X11Clipboard::new().expect("arboard 初始化");
        let original = clipboard.read_text(u64::MAX).expect("读取失败");
        let _restore = RestoreGuard {
            clipboard: arboard::Clipboard::new().expect("恢复守卫初始化"),
            original: original.clone(),
        };

        let payload = "<b>xcontrol X11 冒烟</b> <i>αβγ</i>";
        let mark = clipboard.write_html(payload).expect("HTML 写入成功");
        let deadline = Instant::now() + Duration::from_secs(2);
        while clipboard.change_count() <= mark.change_count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        match clipboard
            .read_html(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as u64)
            .expect("HTML 读取失败")
        {
            FormatRead::Payload(ClipFormatPayload::Html(html)) => {
                assert_eq!(html, payload, "HTML fragment 读回字节等价")
            }
            other => panic!("写入 HTML 后应读到同 fragment，得到 {other:?}"),
        }
    }
}
