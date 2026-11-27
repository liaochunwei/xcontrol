//! `clip_rt` — 剪贴板运行时接线（FR-4.1/4.5，docs/04 §3·§5/§6；11-20 两机实测
//! 收尾扩展：`xcontrol-clipboard` / core `ClipRx`/`ClipTx`/`clip_plan` 全部就绪
//! 但 app 运行时从未接线——本模块把四块拼起来）。
//!
//! # 传输裁定（回归双连接，2026-11-21）
//!
//! docs/02 §0/§4.4 的原设计：剪贴板走**独立 TCP 连接**（完整 Noise 握手 +
//! 首帧 `ChannelBind{session_id}` 绑定控制会话）。2026-11-20 曾按两机实测
//! 优先裁定「同连接」（clip 帧与输入/控制同帧流），实测暴露队头阻塞：大图
//! 分块流与键鼠/心跳帧互为队头阻塞，任何抖动断连 = 控制断连 = 控制被切回
//! 主控（用户不可接受）。2026-11-21 **回归双连接原设计**（docs/02 §4.4 修订
//! 记录同步）：剪贴板帧（kind 0x50–0x54）全部走独立连接（接线在
//! [`crate::clip_channel`]——连接生命周期/重连/首帧分派在那边，本模块仍是
//! 纯状态机）；剪贴板通道任何失败（解密/分帧/超时/违规）只影响剪贴板——
//! 通道独立重连（3s 退避），控制会话与输入转发完全不受影响。
//!
//! 接收侧按 session_id **本地预绑定**（[`ClipboardRuntime::new`]——
//! `ClipRx::on_bind` 入口原样复用，会话匹配/绑定唯一语义保留；线上
//! `ChannelBind` 首帧由 clip_channel 的分派器在建连时校验——会话一致才
//! 构造运行时，等价于按帧绑定）。违规处置恢复独立连接语义：[`ClipRx`]
//! 的 `Violation` → **断开剪贴板连接**（[`ClipboardRuntime::on_clip_msg`]
//! 返回违规标志，通道泵据此断连重连），不再降级丢弃。
//!
//! # 结构（纯驱动 + 显式 send 回调）
//!
//! ```text
//! 本机变化（watcher drain）→ read_text → format_read_from（FR-4.5 超限信号）
//!   → EchoFilter.classify（防线 1/3 回声抑制）→ clip_plan::plan_chunks
//!   → ClipAnnounce → ClipChunk*（ClipTx 信用窗口门控）→ ClipEnd
//!   → pump 产出 → clip_channel 编码 → 通道 transport.send（独立 TCP）
//!
//! 通道收帧（clip_channel 泵解出的 ClipMsg）→ on_clip_msg
//!   → ClipRx.on_announce/on_chunk/on_end → SendAck 出站 / Deliver
//!   → clipboard.write_text → EchoFilter.note_written（防回环）
//! ```
//!
//! - **不持 transport / 不持 session**：运行时只产出帧与动作；发送与状态机
//!   访问权在 clip_channel 的泵循环（与 hotkey_rt/edge 的分工同构，编排
//!   决策集中）；
//! - **watcher 线程纪律**：平台 watcher 只做「变化判定 → mpsc 投递」；
//!   剪贴板读写全部在通道泵的 ticker 拍内执行（非监听线程——
//!   platform-code.md 回调线程纪律；NSPasteboard 读写任意线程合法）；
//! - **容错红线（error-handling 规则 4）**：剪贴板失败（读/写/watcher 全部）
//!   只 `tracing::warn!` 跳过，绝不影响输入转发；平台构造失败降级为
//!   「剪贴板不同步但会话照常」；
//! - **EchoFilter 进程级共享**（[`shared_echo`]）：跨控制会话重连与跨剪贴板
//!   通道重建存活——通道重建后已交付内容的回声基线仍在，不会把对端刚交付
//!   的内容误判为新变化再发回去（11-20 回声乒乓修复的延续）；
//! - **观测日志**：发送/交付各一条 info（clip_id/字节数），两机实测两端
//!   可见（`session::clip` target）。

use std::time::Instant;

use xcontrol_clipboard::{
    ClipboardContent, ClipboardError, ContentFingerprint, EchoFilter, FormatRead, WrittenMark,
    fingerprint_bytes,
};
use xcontrol_core::clip_plan::plan_chunks;
use xcontrol_core::clip_rx::{ClipRx, ClipRxAction, ClipRxConfig};
use xcontrol_core::clip_tx::ClipTx;
use xcontrol_proto::clip::{ClipFormat, ClipMsg};
use xcontrol_proto::{ClipAck, ClipAnnounce, ClipChunk, ClipEnd};

/// 剪贴板运行时 logging target（logging-targets.md 审计表新增行）。
pub(crate) const TARGET: &str = "session::clip";

/// 剪贴板 ticker 周期：200ms（本机变化检测 + 出站补泵；用户复制后
/// <200ms+一拍 内对端可见——FR-4.1「复制后 <200ms 可粘贴」端到端预算内）。
pub(crate) const CLIP_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

// ============================================================
// EchoFilter 进程级共享（跨控制会话重连 + 跨剪贴板通道重建存活）
// ============================================================

/// 进程级共享的回声抑制基线（11-20 回声乒乓修复 + 11-21 双连接延续）：
/// 剪贴板运行时随通道/会话重建，`note_written` 基线若随实例丢失，重连后
/// 对端交付的回声会被误判为新内容 → 双端乒乓。静态 `OnceLock` 持有
/// `Arc<Mutex<EchoFilter>>`，全部运行时实例共享同一份（[`ClipboardRuntime::new`]
/// 经 [`shared_echo`] 注入）。
static SHARED_ECHO: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<EchoFilter>>> =
    std::sync::OnceLock::new();

/// 取进程级共享 EchoFilter 句柄（首次调用惰性构造；其后克隆同一 Arc）。
#[must_use]
/// 进程级共享平台剪贴板槽（根因 #5 深层修复，2026-11-27 双机实测）：同一进程
/// 第二次 `open_platform_clipboard()` 的 wayland 连接在 KWin 上互锁（detector
/// 卡死 → file 泵永不启动 → 心跳死）。clip 泵与文件检测器共用同一实例
/// （同一 counter / 同一 data-control 连接 / 同一 watcher 线程）。
#[cfg(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
))]
pub(crate) fn shared_clipboard()
-> Option<std::sync::Arc<std::sync::Mutex<xcontrol_clipboard::PlatformClipboard>>> {
    static SHARED: std::sync::OnceLock<
        Option<std::sync::Arc<std::sync::Mutex<xcontrol_clipboard::PlatformClipboard>>>,
    > = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| match open_platform_clipboard() {
            Ok(clip) => {
                #[cfg(feature = "wayland")]
                if let Err(error) = clip.watch() {
                    tracing::warn!(target: TARGET, %error, "共享剪贴板 Watcher 启动失败");
                }
                let _ = &clip; // wayland watch 是 &self（counter Arc 已克隆给 watcher 线程）
                Some(std::sync::Arc::new(std::sync::Mutex::new(clip)))
            }
            Err(error) => {
                tracing::warn!(target: TARGET, %error, "共享剪贴板实例创建失败");
                None
            }
        })
        .clone()
}

pub(crate) fn shared_echo() -> std::sync::Arc<std::sync::Mutex<EchoFilter>> {
    SHARED_ECHO
        .get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(EchoFilter::default())))
        .clone()
}

// ============================================================
// 收帧旁路（控制连接防御：clip 帧不应再出现在控制连接上）
// ============================================================

/// 从一段解密明文流中剥离剪贴板帧（kind 0x50–0x54）。
///
/// 双连接裁定（2026-11-21）下剪贴板帧走独立通道——控制连接**不应**再收到
/// clip 帧；会话状态机（core `Session::transition_table`）把 `FramePayload::Clip`
/// 判为违规断开。本函数保留为**防御层**：控制连接万一收到 clip 帧（对端旧
/// 版本/残留在途帧），在喂 dispatcher 之前剥出并**丢弃告警**（不旁路处理、
/// 不断连——控制连接的存活优先级高于帧处置），剩余流原样喂 dispatcher。
/// 剪贴板帧的正常处理路径在 [`crate::clip_channel`] 的通道泵（独立连接）。
///
/// 返回 `(剩余流, 解出的 ClipMsg)`。帧边界解析失败（半帧）属传输层不可能
/// 事件（dispatcher 才做流切帧，此处只剥完整帧），按剩余流兜底。
pub(crate) fn split_clip_frames(stream: &[u8]) -> (Vec<u8>, Vec<ClipMsg>) {
    use xcontrol_proto::frame::{FRAME_HEADER_LEN, decode_frame, is_clip_kind};
    let mut rest_stream = Vec::with_capacity(stream.len());
    let mut clips = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        if rest.len() < FRAME_HEADER_LEN {
            // 半帧头：传输层不可能（dispatcher 收到的是完整报文），兜底保留。
            rest_stream.extend_from_slice(rest);
            break;
        }
        match decode_frame(rest) {
            Ok(frame) => {
                let total = frame.total_len();
                if is_clip_kind(frame.header.kind) {
                    match xcontrol_proto::clip::decode_clip(frame.header.kind, frame.payload) {
                        Ok(Some(msg)) => clips.push(msg),
                        // 解码失败/未知：违规帧降级丢弃（同连接裁定不断连）。
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(target: TARGET, %error,
                                "剪贴板帧解码失败（丢弃该帧，不断连——同连接裁定）");
                        }
                    }
                } else {
                    rest_stream.extend_from_slice(&rest[..total]);
                }
                rest = &rest[total..];
            }
            // 帧边界异常：剩余字节整段保留交 dispatcher（由其做违规判定）。
            Err(_) => {
                rest_stream.extend_from_slice(rest);
                break;
            }
        }
    }
    (rest_stream, clips)
}

// ============================================================
// 出站驱动（ClipDriver：内容 → ClipMsg 流；信用窗口 + 单活动 clip）
// ============================================================

/// 出站剪贴板驱动：把「本机内容」变成 `ClipMsg` 序列的纯状态机（零平台、
/// 零 I/O——单测全量可跑）。
///
/// 状态：单活动传输（待发块 + 游标）+ [`ClipTx`] 信用窗口。每拍
/// [`ClipDriver::pump`] 产出当前窗口允许的全部帧；对端 `ClipAck{seq}` 经
/// [`ClipDriver::on_ack`] 归还信用并即时补泵（大内容的多拍延迟收敛）。
#[derive(Debug)]
pub(crate) struct ClipDriver {
    /// 信用窗口（初始 4 块，docs/02 §6.3）。
    tx: ClipTx,
    /// 单活动传输的待发计划（`None` = 无活动传输）。
    pending: Option<PendingPlan>,
    /// 下一个 clip_id（连接内自增，从 1 起）。
    next_clip_id: u32,
    /// 上次外发内容指纹（同内容重复复制的节流面——重复 announce 收益为零
    /// 且占满窗口；两机实测裁定的节流，防线 1 语义的发送侧延伸）。
    last_sent_fp: Option<ContentFingerprint>,
}

/// 单活动传输的待发计划。
#[derive(Debug)]
struct PendingPlan {
    clip_id: u32,
    /// 宣告帧（首拍随首块原子发出——对端状态机要求 announce 先于 chunk，
    /// 拆拍会违反 docs/02 §6.6）。
    announce: ClipAnnounce,
    /// 待发块队列（seq = 游标位置，从 0 起）。
    chunks: Vec<Vec<u8>>,
    /// 已发送块数（游标；重入 pump 从游标继续）。
    sent_chunks: usize,
    /// `ClipEnd` 是否已产出（End 无 ack 语义——发出即传输完成，防重复产出）。
    end_sent: bool,
}

impl ClipDriver {
    /// 无传输的初始态。
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            tx: ClipTx::new(),
            pending: None,
            next_clip_id: 1,
            last_sent_fp: None,
        }
    }

    /// 是否有未收完 ack 的传输（观测/暂缓判定）。
    #[must_use]
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// 提交一次外发内容（`plan_chunks` 展开）。
    ///
    /// - 窗口占用（上一传输未收完 ack）→ [`SubmitErr::Busy`]：内容不丢弃，
    ///   调用方保留待收尾后重提交；
    /// - 同内容重复 → [`SubmitErr::Duplicate`]（节流）；
    /// - 超协商上限 → [`SubmitErr::Oversize`]（FR-4.5 超限不传输）。
    ///
    /// 种子化基线。11-21 用户语义定稿：连接建立时既有剪贴板内容不外发——当前
    /// 指纹注入 `last_sent_fp`，首拍读取即回声跳过；新复制内容指纹不同则正常同步。
    pub(crate) fn seed_last_sent(&mut self, content: &[u8]) {
        self.last_sent_fp = Some(fingerprint_bytes(content));
    }

    pub(crate) fn submit(&mut self, content: &[u8], max_total_len: u32) -> Result<(), SubmitErr> {
        if self.pending.is_some() {
            return Err(SubmitErr::Busy);
        }
        let fp = fingerprint_bytes(content);
        if self.last_sent_fp == Some(fp) {
            return Err(SubmitErr::Duplicate);
        }
        let Some(plan) = plan_chunks(content, max_total_len) else {
            return Err(SubmitErr::Oversize);
        };
        let clip_id = self.next_clip_id;
        self.next_clip_id = self.next_clip_id.wrapping_add(1).max(1);
        self.pending = Some(PendingPlan {
            clip_id,
            // origin 防线 2（docs/04 §5）：同连接裁定下接收侧以内容指纹
            // （EchoFilter 防线 1）去重；origin 字段仍按协议类型填充——
            // app 层无 per-source 序号计数器，置常量 1（协议合法：接收端
            // 状态机不校验该值）。
            announce: ClipAnnounce {
                clip_id,
                origin_id: [0; 16],
                origin_seq: 1,
                // 11-20 图片扩展：PNG magic 判别（读路径只产 Text/ImagePng 两类）。
                formats: if content.starts_with(&[0x89, b'P', b'N', b'G']) {
                    vec![ClipFormat::Png]
                } else {
                    vec![ClipFormat::Text]
                },
                total_len: plan.total_len,
                blake3: plan.blake3,
            },
            chunks: plan.chunks,
            sent_chunks: 0,
            end_sent: false,
        });
        self.last_sent_fp = Some(fp);
        Ok(())
    }

    /// 产出当前信用窗口允许的全部帧到 `out`（无活动传输 → 无产出）。
    ///
    /// 首拍：announce 随首块同拍原子给出；全部块发出后随拍补 `ClipEnd`
    /// （End 不占信用——窗口满不阻塞 End 产出，否则全块已 ack 的收尾被
    /// 窗口挡死即死锁）。
    pub(crate) fn pump(&mut self, out: &mut Vec<ClipMsg>) {
        // announce 先行（游标为 0 = 尚未发出过任何块）。
        if let Some(pending) = self
            .pending
            .as_ref()
            .filter(|pending| pending.sent_chunks == 0)
        {
            out.push(ClipMsg::Announce(pending.announce.clone()));
        }
        // End 先行于本轮块（块全发完且 End 未产出——不占信用，立即给出）。
        if let Some(pending) = self
            .pending
            .as_ref()
            .filter(|pending| !pending.end_sent && pending.sent_chunks >= pending.chunks.len())
        {
            out.push(ClipMsg::End(ClipEnd {
                clip_id: pending.clip_id,
            }));
            if let Some(pending) = self.pending.as_mut() {
                pending.end_sent = true;
            }
        }
        // 逐块按信用窗口产出。
        while self.tx.can_send() {
            let Some(pending) = self.pending.as_mut() else {
                return;
            };
            let Some(seq) = u16::try_from(pending.sent_chunks).ok() else {
                break; // 块数超 u16（16MiB/65472B = 257 块，不可达；防御）
            };
            let Some(data) = pending.chunks.get(pending.sent_chunks) else {
                break; // 全部块已发出（补 End 检查在循环后）
            };
            let data = data.clone();
            let clip_id = pending.clip_id;
            out.push(ClipMsg::Chunk(ClipChunk { clip_id, seq, data }));
            self.tx.on_chunk_sent(seq);
            if let Some(pending) = self.pending.as_mut() {
                pending.sent_chunks += 1;
            }
        }
        // 信用窗口耗尽后仍可能补 End（窗口满但块已全部发完的情形）。
        if let Some(pending) = self
            .pending
            .as_ref()
            .filter(|pending| !pending.end_sent && pending.sent_chunks >= pending.chunks.len())
        {
            out.push(ClipMsg::End(ClipEnd {
                clip_id: pending.clip_id,
            }));
            if let Some(pending) = self.pending.as_mut() {
                pending.end_sent = true;
            }
        }
    }

    /// 对端 `ClipAck{seq}`：归还信用（`true` = 有效 ack；未知/重复静默）。
    ///
    /// 信用全部归还且 End 已发出 → 传输收尾（窗口完全空闲）。
    pub(crate) fn on_ack(&mut self, seq: u16) -> bool {
        if !self.tx.on_ack(seq) {
            return false;
        }
        if self.tx.in_flight() == 0
            && let Some(pending) = self.pending.take()
        {
            tracing::info!(target: TARGET,
                clip_id = pending.clip_id, total_len = pending.announce.total_len,
                "剪贴板出站传输完成（全部块已确认，docs/02 §6.3）");
        }
        true
    }

    /// 会话收尾：丢弃未完成传输（新连接用新实例；观测日志留痕）。
    pub(crate) fn reset(&mut self) {
        if self.pending.is_some() {
            tracing::debug!(target: TARGET, "会话收尾：丢弃未完成剪贴板出站传输");
        }
        self.tx = ClipTx::new();
        self.pending = None;
        self.next_clip_id = 1;
        self.last_sent_fp = None;
    }
}

/// [`ClipDriver::submit`] 的拒绝原因（调用方按语义日志/暂缓）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitErr {
    /// 上一传输未收完 ack（窗口占用）：内容保留，收尾后重提交。
    Busy,
    /// 与上次外发内容一致（同内容节流，不重复外发）。
    Duplicate,
    /// 超协商上限（FR-4.5 超限不传输）。
    Oversize,
}

// ============================================================
// 接收侧（ClipRx 驱动封装；同连接裁定下免 ChannelBind）
// ============================================================

/// 接收端时钟：`ClipRx` 的 10s 超时判定驱动（`Instant` 单调毫秒；语义与
/// core `SystemClock` 一致的本地薄实现——避免为命名泛型引入额外导出面）。
struct ClipRxInstantClock {
    epoch: Instant,
}

impl xcontrol_core::clock::Clock for ClipRxInstantClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

// ============================================================
// 平台读写/watcher 的 feature 门控装配（公共代码零 #[cfg(target_os)]）
// ============================================================

/// watcher drain 面的统一 trait（各平台 watcher 的 `drain(&mut Vec<T>)` 适配
/// ——调用方在 ticker 拍内拉取，监听线程只 mpsc 投递，回调线程纪律）。
trait ClipWatcherDrain: Send {
    /// 非阻塞拉取自上次以来的变化事件（计数快照投给 `sink`）。
    fn drain(&mut self, sink: &mut dyn FnMut(u64));
}

/// 无平台 feature 的占位（drain 恒空——编译面在无平台环境仍成立）。
struct NullWatcher;

impl ClipWatcherDrain for NullWatcher {
    fn drain(&mut self, _sink: &mut dyn FnMut(u64)) {}
}

// ============================================================
// ClipboardRuntime（session_rt 持有的双侧运行时）
// ============================================================

/// 剪贴板运行时（每条 Running 会话一个实例；断开即 drop——watcher 线程随
/// Drop 退出，xcontrol-clipboard 的防泄漏语义）。
///
/// 平台构造失败**不是错误**：降级为「本端不同步、输入照常转发」（error-
/// handling 规则 4 的平台版）；接收侧无落板路径时直接不 ack（对端 10s 软
/// 失败，语义更诚实——不假应答）。
pub(crate) struct ClipboardRuntime {
    /// 平台剪贴板后端（读写统一入口；构造失败 = `None`）。
    backend: Option<PlatformBackend>,
    /// 平台 watcher（构造失败 = `None`——本端不检测变化 = 不外发）。
    watcher: Box<dyn ClipWatcherDrain>,
    /// 本机回声抑制（防线 1 内容指纹 + 防线 3 写后静默窗，docs/04 §5）。
    echo: std::sync::Arc<std::sync::Mutex<EchoFilter>>,
    /// 出站驱动。
    driver: ClipDriver,
    /// 接收状态机（构造即按 session_id 本地预绑定，bind 语义原样保留）。
    rx: ClipRx<ClipRxInstantClock>,
    /// 出站内容上限（FR-4.5 协商口径）。
    max_total_len: u32,
    /// watcher 变化的已消费计数（`has_changes_since` 触发基准）。
    seen_count: u64,
    /// Busy 暂缓的待发内容（`None` = 无）。
    resubmit: Option<Vec<u8>>,
}

/// 平台后端封装（feature 门控 enum；公共类型零平台判断）。
struct PlatformBackend(PlatformBackendInner);

#[cfg(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
))]
struct PlatformBackendInner {
    /// 共享剪贴板实例（进程级单例——clip 泵与 file 检测器/挂板面共用，
    /// 根因 #9：独立实例 = 第二 wayland 连接，外部写入事件投递不可靠）。
    inner: Option<std::sync::Arc<std::sync::Mutex<xcontrol_clipboard::PlatformClipboard>>>,
}

#[cfg(not(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
)))]
struct PlatformBackendInner;

impl ClipboardRuntime {
    /// 构造运行时（平台后端 + watcher + EchoFilter + 出站驱动 + 接收状态机）。
    ///
    /// `max_bytes` = config `[clipboard].max_bytes` 与协议 16MiB 取小（FR-4.5
    /// 可配；`0` = 禁用同步，构造即空壳）。
    ///
    /// `session_id` = 控制连接握手摘要派生的会话标识（docs/02 §3.3）——双连接
    /// 裁定下接收状态机在构造时**本地预绑定**（线上 `ChannelBind` 首帧已在建连
    /// 分派时校验过会话一致；`ClipRx::on_bind` 入口原样复用，绑定唯一语义由
    /// 「此后收到的 Bind = 违规」承接）。
    #[must_use]
    pub(crate) fn new(
        max_bytes: u64,
        session_id: [u8; 16],
        shared_echo: std::sync::Arc<std::sync::Mutex<EchoFilter>>,
    ) -> Self {
        if max_bytes == 0 {
            tracing::info!(target: TARGET,
                "剪贴板同步已禁用（config clipboard.max_bytes = 0，FR-4.5）");
            return Self::disabled(session_id, shared_echo);
        }
        let capped = u32::try_from(max_bytes.min(u64::from(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)))
            .unwrap_or(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN);
        let (backend, watcher) = build_platform_clipboard();
        if backend.is_none() {
            tracing::warn!(target: TARGET,
                "平台剪贴板后端不可用：本端剪贴板不参与同步（输入转发不受影响）");
        }
        let mut rx = ClipRx::new(
            ClipRxConfig {
                max_total_len: capped,
            },
            ClipRxInstantClock {
                epoch: Instant::now(),
            },
            session_id,
        );
        // 本地预绑定（on_bind 一次性调用，不可能重复；违规只可能来自构造
        // 失败面——防御性日志）。
        for action in rx.on_bind(xcontrol_proto::clip::ChannelBind { session_id }) {
            if let ClipRxAction::Violation { reason } = action {
                tracing::warn!(target: TARGET, %reason, "接收端预绑定被拒");
            }
        }
        // 11-21 用户语义定稿：**连接成功时不发送既有剪贴板内容**——只有会话
        // 建立之后的新复制事件才同步。种子 = 当前剪贴板指纹注入 driver 基线
        // （首拍读取即回声跳过）。同时消除"连接瞬间双向大内容互发"的断连
        // 触发器（两机实测：1.38MB 既有截图在连接秒级外发 → DecryptFailed）。
        // 尽力而为：读取失败基线留空（最坏多一次初始同步，不阻塞建连）。
        let mut driver = ClipDriver::new();
        let mut backend_mut = backend;
        if backend_mut.is_some() {
            match read_platform_text(&mut backend_mut, u64::from(capped)) {
                Ok(FormatRead::Payload(ClipFormatPayload::Text(t))) => {
                    driver.seed_last_sent(&t.into_bytes());
                    tracing::info!(target: TARGET,
                        "连接基线：既有文本剪贴板已登记（不外发，新复制才同步）");
                }
                _ => {
                    // 文本不存在 → 试图片（mac 截图常驻剪贴板的形态）。
                    if let Ok(FormatRead::Payload(ClipFormatPayload::ImagePng(png))) =
                        read_platform_image(&mut backend_mut, u64::from(capped))
                    {
                        driver.seed_last_sent(&png);
                        tracing::info!(target: TARGET,
                            "连接基线：既有图片剪贴板已登记（不外发，新复制才同步）");
                    }
                }
            }
        }
        Self {
            backend: backend_mut,
            watcher,
            echo: shared_echo,
            driver,
            rx,
            max_total_len: capped,
            seen_count: 0,
            resubmit: None,
        }
    }

    /// 禁用/降级空壳（无平台后端；rx 仍按 session_id 预绑定——状态机语义
    /// 完整保留，仅无交付路径）。
    fn disabled(
        session_id: [u8; 16],
        shared_echo: std::sync::Arc<std::sync::Mutex<EchoFilter>>,
    ) -> Self {
        let mut rx = ClipRx::new(
            ClipRxConfig::default(),
            ClipRxInstantClock {
                epoch: Instant::now(),
            },
            session_id,
        );
        let _ = rx.on_bind(xcontrol_proto::clip::ChannelBind { session_id });
        Self {
            backend: None,
            watcher: Box::new(NullWatcher),
            echo: shared_echo,
            driver: ClipDriver::new(),
            rx,
            max_total_len: 0,
            seen_count: 0,
            resubmit: None,
        }
    }

    /// 本端是否具备落板路径（收帧 ack 门控：无落板 = 不 ack）。
    #[must_use]
    pub(crate) fn can_deliver(&self) -> bool {
        self.backend.is_some()
    }

    /// 消费 watcher 变化 → 读文本 → 回声判定 → 出站提交（ticker 拍内调用；
    /// 平台读写全在本拍执行，监听线程零剪贴板操作）。
    ///
    /// 产出直接进入 `out`（调用方编码发送）；回声/超限/失败只日志不外发。
    pub(crate) fn poll_local_changes(&mut self, out: &mut Vec<ClipMsg>, now: Instant) {
        // Busy 暂缓内容优先（窗口空闲即补发，不等下一次复制）。
        self.flush_resubmit(out);
        let mut counts = Vec::new();
        self.watcher.drain(&mut |count| counts.push(count));
        let changed = counts.iter().any(|&count| {
            if count > self.seen_count {
                self.seen_count = count;
                true
            } else {
                false
            }
        });
        if !changed || self.backend.is_none() {
            return;
        }
        // 读取（11-20 扩展：文本优先 → 图片次之——mac 截图剪贴板无 text，
        // arboard get_text 对纯图片返回 NoContent；FR-4.1/4.2/4.5）。超限/空/
        // 失败都是信号或常态，不外发不报错。
        // 【2026-11-27 双机实测修复】文件引用板优先探测：Finder 复制文件时板上
        // 带 public.file-url + icns/tiff/PNGf **文件预览图**——图片优先路径会把
        // 预览图误判为图片剪贴板内容同步出去（166KB PNG，与文件域通告并发出站）。
        // 板上有 file-url = 文件复制，本板归文件域接管，clip 泵静默跳过。
        // 【2026-11-27 双机实测修复】文件引用板优先探测：Finder 复制文件时板上
        // 带 public.file-url + icns/tiff/PNGf **文件预览图**——图片优先路径会把
        // 预览图误判为图片剪贴板内容同步出去（166KB PNG，与文件域通告并发出站）。
        // 板上有 file-url = 文件复制，本板归文件域接管，clip 泵静默跳过。
        #[cfg(any(feature = "macos", feature = "wayland"))]
        if let Some(inner_clip) = self.backend.as_mut().and_then(|b| b.0.inner.as_mut())
            && inner_clip
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_file_urls(512)
                .is_ok_and(|r| r.is_some())
        {
            tracing::debug!(target: TARGET, "板上有文件引用（文件域接管）：clip 泵跳过");
            return;
        }
        // 11-20 实测顺序修正：**图片优先**。macOS 剪贴板对纯图片（截图）会给
        // `get_text` 返回二进制乱码"文本"（TIFF 数据强转）——文本优先把它当
        // 文本发出（非 UTF-8/超 Noise 65519B 加密上限）。图片读取对纯文本剪贴
        // 板返回 NoContent，无误判面。
        let payload = match read_platform_image(&mut self.backend, u64::from(self.max_total_len)) {
            // PNG magic 校验（11-20 实测修复）：mac read_image_png 对**文本**
            // 剪贴板也返回"图片"（arboard fallback 把文本包装成位图）——不加
            // magic 校验会把文本误判为图片、真文本路径走不到。
            Ok(FormatRead::Payload(ClipFormatPayload::ImagePng(png)))
                if png.starts_with(&[0x89, b'P', b'N', b'G']) =>
            {
                Some(ClipFormatPayload::ImagePng(png))
            }
            Ok(FormatRead::Payload(ClipFormatPayload::ImagePng(fake))) => {
                tracing::debug!(target: TARGET, len = fake.len(),
                    "图片读取返回非 PNG 数据（arboard fallback）→ 走文本路径");
                None
            }
            Ok(FormatRead::Payload(_)) => None, // 图片路径返回文本/HTML（防御，走文本路径）
            Ok(FormatRead::Oversize { actual_len }) => {
                tracing::info!(target: TARGET, actual_len,
                        "剪贴板内容超上限（FR-4.5 超限不传输，本轮跳过）");
                return;
            }
            Ok(FormatRead::NoContent) | Err(_) => {
                // 无图片 → 试文本（FR-4.1 主路径）。
                match read_platform_text(&mut self.backend, u64::from(self.max_total_len)) {
                    Ok(FormatRead::Payload(ClipFormatPayload::Text(t))) => {
                        Some(ClipFormatPayload::Text(t))
                    }
                    Ok(FormatRead::Payload(ClipFormatPayload::Html(_))) => {
                        tracing::debug!(target: TARGET, "HTML 载荷本轮跳过");
                        None
                    }
                    Ok(FormatRead::Payload(ClipFormatPayload::ImagePng(_))) => {
                        tracing::debug!(target: TARGET, "文本路径返回图片（防御，跳过）");
                        None
                    }
                    Ok(FormatRead::NoContent) => None, // 真空
                    Ok(FormatRead::Oversize { actual_len }) => {
                        tracing::info!(target: TARGET, actual_len,
                                "剪贴板内容超上限（FR-4.5 超限不传输，本轮跳过）");
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(target: TARGET, %error, "剪贴板文本读取失败（跳过本轮）");
                        return;
                    }
                }
            }
        };
        let Some(payload) = payload else {
            // 空剪贴板：常态（FR-4.1），无外发。
            return;
        };
        // 回声判定（防线 3 静默窗 + 防线 1 内容指纹；docs/04 §5）。
        let content = match &payload {
            ClipFormatPayload::Text(text) => ClipboardContent {
                text: Some(text.clone()),
                ..ClipboardContent::default()
            },
            ClipFormatPayload::ImagePng(png) => ClipboardContent {
                image_png: Some(png.clone()),
                ..ClipboardContent::default()
            },
            ClipFormatPayload::Html(_) => return, // 防御（上方已滤）
        };
        if self
            .echo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .classify_content(now, Some(&content))
            == xcontrol_clipboard::EchoVerdict::Echo
        {
            tracing::debug!(target: TARGET, "剪贴板变化判定为自身回声（不外发）");
            return;
        }
        let bytes: Vec<u8> = match payload {
            ClipFormatPayload::Text(text) => text.into_bytes(),
            ClipFormatPayload::ImagePng(png) => png,
            ClipFormatPayload::Html(_) => return,
        };
        let len = bytes.len();
        match self.driver.submit(&bytes, self.max_total_len) {
            Ok(()) => {
                let kind_label = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                    "图片 PNG"
                } else {
                    "文本"
                };
                tracing::info!(target: TARGET, len,
                    "本机剪贴板变化 → 出站传输开始（{kind_label}）");
                self.driver.pump(out);
            }
            Err(SubmitErr::Busy) => {
                tracing::debug!(target: TARGET, "上一剪贴板传输未收完，本变化暂缓");
                self.resubmit = Some(bytes);
            }
            Err(SubmitErr::Duplicate) => {
                tracing::debug!(target: TARGET, "剪贴板内容与上次外发一致（跳过）");
            }
            Err(SubmitErr::Oversize) => {
                tracing::info!(target: TARGET, len, "剪贴板内容超上限（FR-4.5 超限不传输）");
            }
        }
    }

    /// Busy 暂缓内容的重提交（窗口空闲时随拍补发；失败丢弃本拍）。
    fn flush_resubmit(&mut self, out: &mut Vec<ClipMsg>) {
        if self.driver.has_pending() || self.resubmit.is_none() {
            return;
        }
        if let Some(bytes) = self.resubmit.take() {
            let len = bytes.len();
            match self.driver.submit(&bytes, self.max_total_len) {
                Ok(()) => {
                    tracing::info!(target: TARGET, len, "暂缓剪贴板内容已补发");
                    self.driver.pump(out);
                }
                Err(SubmitErr::Oversize) => {
                    tracing::info!(target: TARGET, len, "暂缓内容超上限（丢弃）");
                }
                // Duplicate/Busy：静默丢弃本拍（下次变化重新走全流程）。
                Err(_) => {}
            }
        }
    }

    /// `ClipAck`：归还信用并即时补泵（窗口空闲 → 下一批 chunk/End 立即出站，
    /// 不等下一拍）。
    pub(crate) fn on_ack(&mut self, seq: u16, out: &mut Vec<ClipMsg>) {
        if self.driver.on_ack(seq) {
            self.driver.pump(out);
            self.flush_resubmit(out);
        }
    }

    /// 收到的剪贴板帧（clip_channel 通道泵调用，独立连接）。
    ///
    /// 接收端在构造时按 session_id 本地预绑定（[`ClipboardRuntime::new`]）；
    /// 线上 `ChannelBind` 首帧已在建连时由分派器校验（会话一致才构造运行时），
    /// 此后到达的 `Bind` 变体 = 重复绑定违规（返回 `true`，通道泵断开重连）；
    /// 对端 `ClipAck` 直接归还本端出站信用。
    ///
    /// 返回是否发生协议违规（`true` = 调用方必须断开剪贴板通道——独立连接
    /// 语义恢复，docs/02 §6.6；控制会话不受影响）。
    pub(crate) fn on_clip_msg(&mut self, msg: ClipMsg, out: &mut Vec<ClipMsg>) -> bool {
        match msg {
            // 预绑定后对端再发 Bind = 绑定唯一被破坏（违规断开通道）。
            ClipMsg::Bind(_) => {
                tracing::warn!(target: TARGET,
                    "剪贴板通道收到重复 ChannelBind（绑定唯一违规，docs/02 §4.4）");
                true
            }
            // 对端归还我方出站信用（Ack 无 clip_id——单活动 clip 语义）。
            ClipMsg::Ack(ack) => {
                self.on_ack(ack.seq, out);
                false
            }
            ClipMsg::Announce(_) | ClipMsg::Chunk(_) | ClipMsg::End(_) => {
                if !self.can_deliver() {
                    // 无落板路径：不 ack 不交付（对端 10s 超时软失败丢弃，
                    // 语义诚实——不假应答）。
                    return false;
                }
                match msg {
                    ClipMsg::Announce(announce) => {
                        let actions = self.rx.on_announce(announce);
                        self.apply_rx_actions(actions, out)
                    }
                    ClipMsg::Chunk(chunk) => {
                        let actions = self.rx.on_chunk(chunk);
                        self.apply_rx_actions(actions, out)
                    }
                    ClipMsg::End(end) => {
                        let actions = self.rx.on_end(end);
                        self.apply_rx_actions(actions, out)
                    }
                    // 上一分支已覆盖（编译器要求完整 match）。
                    ClipMsg::Bind(_) | ClipMsg::Ack(_) => false,
                }
            }
        }
    }

    /// ticker 拍内的接收端超时判定（10s 未完成 → Drop，软失败不断连）。
    /// 返回是否发生协议违规（当前超时路径不产出违规，防御性传播）。
    pub(crate) fn on_tick(&mut self, out: &mut Vec<ClipMsg>) -> bool {
        let actions = self.rx.on_tick();
        self.apply_rx_actions(actions, out)
    }

    /// ClipRx 动作应用：SendAck → 出站；Deliver → 落板 + note_written；
    /// Drop → 日志；Violation → 日志 + 违规标志（独立连接语义：调用方断开
    /// 剪贴板通道重连，docs/02 §6.6）。返回是否发生违规。
    fn apply_rx_actions(&mut self, actions: Vec<ClipRxAction>, out: &mut Vec<ClipMsg>) -> bool {
        let mut violated = false;
        for action in actions {
            match action {
                ClipRxAction::SendAck { seq } => {
                    out.push(ClipMsg::Ack(ClipAck { seq }));
                }
                ClipRxAction::Deliver { clip_id, content } => {
                    self.deliver(clip_id, content);
                }
                ClipRxAction::Drop { reason } => {
                    tracing::warn!(target: TARGET, %reason, "剪贴板传输软失败丢弃（不断连）");
                }
                ClipRxAction::Violation { reason } => {
                    tracing::warn!(target: TARGET, %reason,
                        "剪贴板协议违规（独立连接语义：断开剪贴板通道，docs/02 §6.6）");
                    violated = true;
                }
            }
        }
        violated
    }

    /// 交付完整内容到本机剪贴板（UTF-8 文本；落板 + EchoFilter 基线）。
    fn deliver(&mut self, clip_id: u32, content: Vec<u8>) {
        // 内容形态（11-20 图片扩展）：PNG magic → write_image_png（FR-4.2）；
        // 否则 UTF-8 文本 → write_platform_text（FR-4.1）。
        if content.starts_with(&[0x89, b'P', b'N', b'G']) {
            match write_platform_image(&mut self.backend, &content) {
                Ok(mark) => {
                    self.echo
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .note_written(mark);
                    tracing::info!(target: TARGET, clip_id, len = content.len(),
                        "对端剪贴板已交付到本机（图片 PNG）");
                }
                Err(error) => {
                    tracing::warn!(target: TARGET, clip_id, %error, "剪贴板图片写入失败（丢弃交付）");
                }
            }
            return;
        }
        let text = match String::from_utf8(content) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(target: TARGET, clip_id, %error, "交付内容非 UTF-8（丢弃）");
                return;
            }
        };
        match write_platform_text(&mut self.backend, &text) {
            Ok(mark) => {
                self.echo
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .note_written(mark);
                tracing::info!(target: TARGET, clip_id, len = text.len(),
                    "对端剪贴板已交付到本机（文本）");
            }
            Err(error) => {
                tracing::warn!(target: TARGET, clip_id, %error, "剪贴板写入失败（丢弃交付）");
            }
        }
    }

    /// 会话收尾：丢弃未完成出站传输（rx 状态随实例 drop）。
    pub(crate) fn reset_outbound(&mut self) {
        self.driver.reset();
        self.resubmit = None;
    }
}

// ============================================================
// 平台构造与读写分派（feature 门控宏；公共路径零平台判断）
// ============================================================

/// 平台构造（读写后端 + watcher 两步独立可失败——监听不可用时仍可被动接收，
/// 仅不外发本机变化）。
///
/// - macos：arboard 读写 + `NSPasteboard.changeCount` 轮询线程（250ms，
///   docs/04 §4 mac 无系统剪贴板通知 API）；
/// - windows：arboard 读写 + `AddClipboardFormatListener` 消息窗口监听；
/// - x11：arboard X11 后端读写 + `XFixes` SelectionNotify 监听线程；
/// - wayland：wl-clipboard-rs 读写 + data-control 监听线程。
///
/// x11/wayland 的监听计数槽与读写句柄共享（Arc<AtomicU64>）——用独立工厂
/// 实例承担 watch（`watch()` 成功后 forget 读写侧：计数槽由 watcher 持有，
/// 读写走本次会话的另一个工厂实例；selection 语义下读写与监听不共享句柄
/// 状态，docs/05 §4）。
#[must_use]
fn build_platform_clipboard() -> (Option<PlatformBackend>, Box<dyn ClipWatcherDrain>) {
    #[cfg(any(
        feature = "macos",
        feature = "windows",
        feature = "x11",
        feature = "wayland"
    ))]
    {
        // 根因 #9（2026-11-27 双机实测）：改用进程级共享实例——clip 泵与
        // file 检测器/挂板面共用同一 PlatformClipboard（同一 wayland 连接 /
        // 同一 counter / 同一 watcher）。原 open_platform_backend 独立实例 =
        // 第二连接，外部写入事件投递不可靠（detector 失明）。
        match shared_clipboard() {
            Some(shared) => {
                let watcher = start_platform_watcher_from_shared(&shared);
                let boxed = Some(PlatformBackend(PlatformBackendInner {
                    inner: Some(shared),
                }));
                (boxed, watcher)
            }
            None => {
                tracing::debug!(target: TARGET, "共享剪贴板实例不可用");
                (None, Box::new(NullWatcher))
            }
        }
    }
    #[cfg(not(any(
        feature = "macos",
        feature = "windows",
        feature = "x11",
        feature = "wayland"
    )))]
    {
        (None, Box::new(NullWatcher))
    }
}

#[cfg(feature = "macos")]
#[cfg(any(feature = "windows", feature = "x11", feature = "wayland"))]
fn open_platform_backend() -> Result<xcontrol_clipboard::PlatformClipboard, ClipboardError> {
    xcontrol_clipboard::open_platform_clipboard()
}

/// 平台 watcher 启动（与打开成功的后端同拍启动；失败 = NullWatcher 降级）。
#[cfg(feature = "macos")]
fn start_platform_watcher_from_shared(
    _shared: &std::sync::Arc<std::sync::Mutex<xcontrol_clipboard::PlatformClipboard>>,
) -> Box<dyn ClipWatcherDrain> {
    // mac：changeCount 轮询线程独立于读写句柄（ns_change_count 原语全局）。
    match xcontrol_clipboard::ClipboardWatcher::start(xcontrol_clipboard::DEFAULT_POLL_INTERVAL) {
        Ok(watcher) => Box::new(PollWatcherAdapter(watcher)),
        Err(error) => {
            tracing::debug!(target: TARGET, %error, "mac watcher 启动失败（降级 NullWatcher）");
            Box::new(NullWatcher)
        }
    }
}

#[cfg(feature = "windows")]
fn start_platform_watcher_from_shared(
    _shared: &std::sync::Arc<std::sync::Mutex<xcontrol_clipboard::PlatformClipboard>>,
) -> Box<dyn ClipWatcherDrain> {
    match xcontrol_clipboard::ClipboardWatcher::start() {
        Ok(watcher) => Box::new(PollWatcherAdapter(watcher)),
        Err(error) => {
            tracing::debug!(target: TARGET, %error, "win watcher 启动失败（降级 NullWatcher）");
            Box::new(NullWatcher)
        }
    }
}

#[cfg(feature = "x11")]
fn start_platform_watcher(
    backend: &mut xcontrol_clipboard::PlatformClipboard,
) -> Box<dyn ClipWatcherDrain> {
    // X11：watcher 与读写句柄共享计数槽（Arc<AtomicU64>）——同一句柄上
    // `watch()` 启动监听线程（结构体内计数槽 Arc 克隆，非独占借用）。
    match backend.watch() {
        Ok(watcher) => Box::new(X11WatcherAdapter(watcher)),
        Err(error) => {
            tracing::debug!(target: TARGET, %error, "x11 watcher 启动失败（降级 NullWatcher）");
            Box::new(NullWatcher)
        }
    }
}

#[cfg(feature = "wayland")]
fn start_platform_watcher(
    backend: &mut xcontrol_clipboard::PlatformClipboard,
) -> Box<dyn ClipWatcherDrain> {
    match backend.watch() {
        Ok(watcher) => Box::new(WaylandWatcherAdapter(watcher)),
        Err(error) => {
            tracing::debug!(target: TARGET, %error, "wayland watcher 启动失败（降级 NullWatcher）");
            Box::new(NullWatcher)
        }
    }
}

#[cfg(not(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
)))]
// 完整编译矩阵守卫（app 生产恒带平台 feature，本分支实际不可达但保留使
// 无平台环境可编译；CI feature 矩阵不断重排该假设）。
#[allow(dead_code)]
fn start_platform_watcher() -> Box<dyn ClipWatcherDrain> {
    // 无平台 feature 编译面（app 恒带平台 feature；此路径保完整矩阵可编译）。
    Box::new(NullWatcher)
}

#[cfg(any(feature = "macos", feature = "windows"))]
struct PollWatcherAdapter(xcontrol_clipboard::ClipboardWatcher);

#[cfg(any(feature = "macos", feature = "windows"))]
impl ClipWatcherDrain for PollWatcherAdapter {
    fn drain(&mut self, sink: &mut dyn FnMut(u64)) {
        let mut events = Vec::new();
        self.0.drain(&mut events);
        for event in events {
            sink(event.change_count);
        }
    }
}

#[cfg(feature = "x11")]
struct X11WatcherAdapter(xcontrol_clipboard::platform::x11::X11Watcher);

#[cfg(feature = "x11")]
impl ClipWatcherDrain for X11WatcherAdapter {
    fn drain(&mut self, sink: &mut dyn FnMut(u64)) {
        let mut events = Vec::new();
        self.0.drain(&mut events);
        for event in events {
            sink(event.change_count);
        }
    }
}

#[cfg(feature = "wayland")]
struct WaylandWatcherAdapter(xcontrol_clipboard::platform::wayland::WaylandWatcher);

#[cfg(feature = "wayland")]
impl ClipWatcherDrain for WaylandWatcherAdapter {
    fn drain(&mut self, sink: &mut dyn FnMut(u64)) {
        let mut events = Vec::new();
        self.0.drain(&mut events);
        for event in events {
            sink(event.change_count);
        }
    }
}

/// 平台读文本（feature 分派；`FormatRead` 归一化面统一）。
#[cfg(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
))]
fn read_platform_text(
    backend: &mut Option<PlatformBackend>,
    max_bytes: u64,
) -> Result<FormatRead, ClipboardError> {
    let inner = backend
        .as_mut()
        .map(|PlatformBackend(inner)| inner.inner.take());
    match inner {
        Some(Some(clipboard)) => {
            let result = clipboard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_text(max_bytes);
            // 放回（下次拍继续用同一句柄；X11 常驻 serve 线程语义保留）。
            if let Some(backend) = backend.as_mut() {
                backend.0.inner = Some(clipboard);
            }
            result
        }
        _ => Err(ClipboardError::Platform {
            action: "clipboard-read",
            reason: "unavailable".into(),
        }),
    }
}

/// 平台读图片（feature 分派；PNG wire 字节归一化面统一）。
#[cfg(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
))]
fn read_platform_image(
    backend: &mut Option<PlatformBackend>,
    max_bytes: u64,
) -> Result<FormatRead, ClipboardError> {
    let inner = backend
        .as_mut()
        .map(|PlatformBackend(inner)| inner.inner.take());
    match inner {
        Some(Some(clipboard)) => {
            let result = clipboard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_image_png(max_bytes);
            if let Some(backend) = backend.as_mut() {
                backend.0.inner = Some(clipboard);
            }
            result
        }
        _ => Err(ClipboardError::Platform {
            action: "clipboard-read-image",
            reason: "unavailable".into(),
        }),
    }
}

/// 平台写文本（feature 分派；`WrittenMark` 基线面统一）。
#[cfg(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
))]
fn write_platform_text(
    backend: &mut Option<PlatformBackend>,
    text: &str,
) -> Result<WrittenMark, ClipboardError> {
    let inner = backend
        .as_mut()
        .map(|PlatformBackend(inner)| inner.inner.take());
    match inner {
        Some(Some(clipboard)) => {
            let result = clipboard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .write_text(text);
            if let Some(backend) = backend.as_mut() {
                backend.0.inner = Some(clipboard);
            }
            result
        }
        _ => Err(ClipboardError::Platform {
            action: "clipboard-write",
            reason: "unavailable".into(),
        }),
    }
}

#[cfg(not(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
)))]
fn read_platform_image(
    _backend: &mut Option<PlatformBackend>,
    _max_bytes: u64,
) -> Result<FormatRead, ClipboardError> {
    let _ = _backend;
    Err(ClipboardError::Platform {
        action: "clipboard-read-image",
        reason: "no platform feature".into(),
    })
}

#[cfg(not(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
)))]
fn write_platform_image(
    _backend: &mut Option<PlatformBackend>,
    _png: &[u8],
) -> Result<WrittenMark, ClipboardError> {
    let _ = _backend;
    Err(ClipboardError::Platform {
        action: "clipboard-write-image",
        reason: "no platform feature".into(),
    })
}

#[cfg(not(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
)))]
fn read_platform_text(
    backend: &mut Option<PlatformBackend>,
    _max_bytes: u64,
) -> Result<FormatRead, ClipboardError> {
    let _ = backend;
    Err(ClipboardError::Platform {
        action: "clipboard-read",
        reason: "no platform feature".into(),
    })
}

/// 平台写图片（feature 分派；`WrittenMark` 基线面统一）。
#[cfg(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
))]
fn write_platform_image(
    backend: &mut Option<PlatformBackend>,
    png: &[u8],
) -> Result<WrittenMark, ClipboardError> {
    let inner = backend
        .as_mut()
        .map(|PlatformBackend(inner)| inner.inner.take());
    match inner {
        Some(Some(clipboard)) => {
            let result = clipboard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .write_image_png(png);
            if let Some(backend) = backend.as_mut() {
                backend.0.inner = Some(clipboard);
            }
            result
        }
        _ => Err(ClipboardError::Platform {
            action: "clipboard-write-image",
            reason: "unavailable".into(),
        }),
    }
}

#[cfg(not(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
)))]
fn write_platform_text(
    backend: &mut Option<PlatformBackend>,
    _text: &str,
) -> Result<WrittenMark, ClipboardError> {
    let _ = backend;
    Err(ClipboardError::Platform {
        action: "clipboard-write",
        reason: "no platform feature".into(),
    })
}

// ============================================================
// ClipFormatPayload / format_read_from 的短名 re-export（本模块使用面）
// ============================================================

#[allow(unused_imports)]
use _format_read_from as _;
#[cfg(any(
    feature = "macos",
    feature = "windows",
    feature = "x11",
    feature = "wayland"
))]
use xcontrol_clipboard::open_platform_clipboard;
use xcontrol_clipboard::{ClipFormatPayload, format_read_from as _format_read_from};

// ============================================================
// 纯逻辑单测（mac 可跑；分块/回声对接/超限跳过）
// ============================================================

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use xcontrol_proto::clip::ClipAnnounce;

    use super::*;

    /// 出站分块：短文本 → announce + 1 chunk + end（原子序列，announce
    /// 先于 chunk——docs/02 §6.6 入口顺序）。
    #[test]
    fn driver_short_text_emits_announce_chunk_end() {
        let mut driver = ClipDriver::new();
        let mut out = Vec::new();
        driver.submit(b"hello", u32::MAX).expect("未超限");
        driver.pump(&mut out);
        assert_eq!(out.len(), 3, "announce + 1 chunk + end");
        assert!(matches!(out[0], ClipMsg::Announce(_)), "首帧必为 announce");
        match &out[1] {
            ClipMsg::Chunk(chunk) => {
                assert_eq!(chunk.seq, 0);
                assert_eq!(chunk.data, b"hello");
            }
            other => panic!("第二帧应为 chunk，得到 {other:?}"),
        }
        assert!(matches!(out[2], ClipMsg::End(_)));
        // ack 全归还后传输收尾（End 无 ack 语义，发出即算）。
        assert!(driver.on_ack(0));
        assert!(!driver.has_pending(), "信用全还 → 收尾");
    }

    /// 大内容分块：信用窗口（初始 4 块）门控——首拍 announce + 4 chunk；
    /// ack 归还后补泵继续；重复 ack 不重复归还。
    #[test]
    fn driver_credit_window_gates_chunks() {
        let mut driver = ClipDriver::new();
        let content = vec![0xABu8; 65472 * 6]; // 6 块
        driver
            .submit(&content, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
            .expect("16MiB 内");
        let mut out = Vec::new();
        driver.pump(&mut out);
        assert_eq!(out.len(), 5, "announce + 4 chunk（信用窗满）");
        assert!(matches!(out[0], ClipMsg::Announce(_)));
        for (idx, msg) in out.iter().enumerate().skip(1) {
            match msg {
                ClipMsg::Chunk(chunk) => {
                    assert_eq!(chunk.seq, u16::try_from(idx - 1).expect("seq < 6"));
                }
                other => panic!("窗口内应全是 chunk，得到 {other:?}"),
            }
        }
        // 归还 2 个信用 → 补泵 2 块（此后块全部发完 → End 随拍补出不占信用）
        let mut out2 = Vec::new();
        assert!(driver.on_ack(0));
        assert!(driver.on_ack(1));
        driver.pump(&mut out2);
        let chunks2 = out2
            .iter()
            .filter(|msg| matches!(msg, ClipMsg::Chunk(_)))
            .count();
        assert_eq!(chunks2, 2, "归还 2 信用 → 补发 2 块");
        assert!(
            out2.iter().any(|msg| matches!(msg, ClipMsg::End(_))),
            "块全发完后 End 随拍补出（不占信用）"
        );
        // 重复 ack 不重复归还（无新块；End 已产出不重复）
        let mut out3 = Vec::new();
        assert!(!driver.on_ack(0), "重复 ack 静默");
        driver.pump(&mut out3);
        assert!(out3.is_empty(), "End 已产出 → 无重复产出");
    }

    /// 全流程：多块传输按信用窗口分多拍发完，End 在最后一批出现（不占
    /// 信用——窗口满时块发完也必须补 End，否则收尾死锁）。
    #[test]
    fn driver_completes_multi_batch_transfer() {
        let mut driver = ClipDriver::new();
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        driver
            .submit(&content, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
            .expect("16MiB 内");
        let total_chunks = u16::try_from(content.chunks(65472).count()).expect("< u16");
        let mut sent: Vec<ClipMsg> = Vec::new();
        // 迭代收敛：每拍 ack 全部未确认块（按 seq 去重）→ 补泵；End 出现即止。
        for _round in 0..=total_chunks {
            let mut batch = Vec::new();
            driver.pump(&mut batch);
            sent.append(&mut batch);
            if sent.iter().any(|msg| matches!(msg, ClipMsg::End(_))) {
                break;
            }
            // ack 全部已发未确认块（每 seq 至多一次；本轮新发的块由下一轮确认）。
            let acked: std::collections::HashSet<u16> = sent
                .iter()
                .filter_map(|msg| match msg {
                    ClipMsg::Chunk(chunk) => Some(chunk.seq),
                    _ => None,
                })
                .collect();
            for seq in acked {
                driver.on_ack(seq);
            }
        }
        assert!(
            sent.iter().any(|msg| matches!(msg, ClipMsg::End(_))),
            "迭代收敛后 End 必须出现（{sent:?} 概览）"
        );
        let chunk_count = sent
            .iter()
            .filter(|msg| matches!(msg, ClipMsg::Chunk(_)))
            .count();
        assert_eq!(chunk_count, usize::from(total_chunks), "全部块最终发出");
        // seq 连续且无重复（0..total 各一次）
        let mut seqs: Vec<u16> = sent
            .iter()
            .filter_map(|msg| match msg {
                ClipMsg::Chunk(chunk) => Some(chunk.seq),
                _ => None,
            })
            .collect();
        seqs.sort_unstable();
        let expected: Vec<u16> = (0..total_chunks).collect();
        assert_eq!(seqs, expected, "seq 连续无重复");
        assert!(
            sent.last()
                .is_some_and(|msg| matches!(msg, ClipMsg::End(_))),
            "End 在最后一批"
        );
    }

    /// 同内容节流：传输收尾后重复 submit 同内容 → Duplicate；不同内容放行。
    #[test]
    fn driver_dedups_identical_repeated_content() {
        let mut driver = ClipDriver::new();
        driver.submit(b"hello", u32::MAX).expect("首次提交");
        let mut out = Vec::new();
        driver.pump(&mut out); // 发出块（占用窗口）
        assert!(driver.on_ack(0), "块 ack 归还信用");
        assert!(!driver.has_pending(), "End 已随拍产出 → 传输收尾");
        let err = driver.submit(b"hello", u32::MAX).unwrap_err();
        assert_eq!(err, SubmitErr::Duplicate, "同内容重复提交被节流");
        driver.submit(b"world", u32::MAX).expect("异内容放行");
        driver.pump(&mut out);
        assert!(driver.on_ack(0));
        // reset 清节流基线：同内容再次放行
        driver.reset();
        driver.submit(b"hello", u32::MAX).expect("reset 后放行");
    }

    /// 超限与窗口占用：16MiB+1 → Oversize（不产生 pending）；窗口满 → Busy。
    #[test]
    fn driver_oversize_and_busy_paths() {
        let mut driver = ClipDriver::new();
        let over = vec![0u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize + 1];
        assert_eq!(
            driver.submit(&over, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN),
            Err(SubmitErr::Oversize),
            "超限不传输（FR-4.5）"
        );
        assert!(!driver.has_pending(), "超限无 pending");
        // 协商缩限同样生效
        let small = vec![0u8; 2048];
        assert_eq!(driver.submit(&small, 1024), Err(SubmitErr::Oversize));
        // 窗口占用：提交后未 ack，再次提交 → Busy
        driver.submit(b"first", u32::MAX).expect("首次");
        assert_eq!(driver.submit(b"second", u32::MAX), Err(SubmitErr::Busy));
    }

    /// `ClipboardRuntime::new(0, …)` = 禁用空壳（无后端/不交付；rx 预绑定
    /// 语义保留——构造即绑定本会话）。
    #[test]
    fn disabled_runtime_when_max_bytes_zero() {
        let session_id = [7u8; 16];
        let mut runtime = ClipboardRuntime::new(
            0,
            session_id,
            std::sync::Arc::new(std::sync::Mutex::new(EchoFilter::default())),
        );
        assert!(!runtime.can_deliver(), "禁用 = 无落板路径");
        // 出站帧被拒收（无落板直接丢帧）。
        let mut out = Vec::new();
        runtime.on_clip_msg(
            ClipMsg::Announce(ClipAnnounce {
                clip_id: 1,
                origin_id: [0; 16],
                origin_seq: 1,
                formats: vec![ClipFormat::Text],
                total_len: 5,
                blake3: [0; 32],
            }),
            &mut out,
        );
        assert!(out.is_empty(), "禁用态不 ack 不交付");
        // 本机变化不外发（NullWatcher 恒无事件；即便有事件也无后端读出）。
        let mut out = Vec::new();
        runtime.poll_local_changes(&mut out, Instant::now());
        assert!(out.is_empty());
    }

    /// EchoFilter 与运行时对接的纯逻辑面：note_written 后窗口内变化判回声
    /// （deliver 路径防回环的判据复验——不依赖平台剪贴板）。
    #[test]
    fn echo_filter_suppresses_written_content() {
        let mut echo = EchoFilter::default();
        let mark = WrittenMark {
            fingerprint: xcontrol_clipboard::fingerprint_content(&ClipboardContent {
                text: Some("写入的内容".into()),
                ..ClipboardContent::default()
            }),
            change_count: 1,
            written_at: Instant::now(),
        };
        echo.note_written(mark);
        // 静默窗内（500ms）：异内容也判回声（防线 3，不看指纹）
        let other = ClipboardContent {
            text: Some("另一段内容".into()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            echo.classify_content(Instant::now(), Some(&other)),
            xcontrol_clipboard::EchoVerdict::Echo
        );
        // 静默窗外、2s 指纹窗内：同内容判回声（防线 1）、异内容放行
        let later = Instant::now() + Duration::from_millis(600);
        let same = ClipboardContent {
            text: Some("写入的内容".into()),
            ..ClipboardContent::default()
        };
        assert_eq!(
            echo.classify_content(later, Some(&same)),
            xcontrol_clipboard::EchoVerdict::Echo
        );
        assert_eq!(
            echo.classify_content(later, Some(&other)),
            xcontrol_clipboard::EchoVerdict::External
        );
    }

    /// `ClipboardRuntime` 结构可跨线程移动（tokio 多线程执行域要求 Send）。
    #[test]
    fn runtime_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ClipboardRuntime>();
        assert_send::<ClipDriver>();
    }

    /// 收帧旁路剥离（同连接裁定）：clip 帧从明文流剥出、其余帧原样保留；
    /// 纯输入流零剥离；混合流顺序保持。
    #[test]
    fn split_clip_frames_peers_clip_kinds() {
        use xcontrol_proto::Message as _;
        use xcontrol_proto::frame::kind;
        use xcontrol_proto::input::{InputEvent, KeyState};
        use xcontrol_proto::xkey::KEY_A;

        // 纯输入流：零剥离
        let key = InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        }
        .encode_frame(1)
        .expect("输入帧编码");
        let (rest, clips) = split_clip_frames(&key);
        assert!(clips.is_empty(), "纯输入流无剥离");
        assert_eq!(rest, key, "输入帧原样保留");

        // clip 帧：剥出并解码为 ClipMsg
        let ack = ClipAck { seq: 3 }.encode_frame(2).expect("Ack 帧编码");
        let (rest, clips) = split_clip_frames(&ack);
        assert_eq!(clips.len(), 1, "clip 帧剥出");
        assert!(matches!(clips[0], ClipMsg::Ack(ClipAck { seq: 3 })));
        assert!(rest.is_empty(), "clip 帧不进剩余流");

        // 混合流：Key + Chunk + Ping 顺序保持、clip 帧全部剥出
        let chunk = ClipChunk {
            clip_id: 1,
            seq: 0,
            data: b"data".to_vec(),
        }
        .encode_frame(3)
        .expect("Chunk 帧编码");
        let ping = xcontrol_proto::encode_frame(4, kind::CTRL_PING, &[0x00]).expect("Ping 编码");
        let mut mixed = key.clone();
        mixed.extend_from_slice(&chunk);
        mixed.extend_from_slice(&ping);
        let (rest, clips) = split_clip_frames(&mixed);
        assert_eq!(clips.len(), 1, "仅 Chunk 被剥出");
        assert!(matches!(clips[0], ClipMsg::Chunk(_)));
        // 剩余流 = Key + Ping（帧层完整、字节序保持）
        let (rest_key_len, _) = split_clip_frames(&key);
        let _ = rest_key_len;
        assert!(rest.starts_with(&key), "剩余流以 Key 帧开头");
        assert!(rest.ends_with(&ping), "剩余流以 Ping 帧结尾");
        assert_eq!(rest.len(), key.len() + ping.len(), "剥离后长度守恒");

        // 多个 clip 帧连续剥出
        let mut many = ack.clone();
        many.extend_from_slice(&chunk);
        let (_, clips) = split_clip_frames(&many);
        assert_eq!(clips.len(), 2, "连续 clip 帧全部剥出");
    }
}
