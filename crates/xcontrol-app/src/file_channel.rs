//! `file_channel` — 独立文件 TCP 通道（FR-7.3，docs/02 §4.6，XCP 1.2+，
//! [ADR-0004](../../../docs/adr/0004-file-transfer-channel-and-materialization.md)）。
//!
//! ```text
//! 客户端（从机）                                     主控（host）
//!   控制连接 + 剪贴板通道（已有，clip_channel）
//!   再拨一条 TCP（同端口）→ 独立 Noise 握手（独立 nonce 流）
//!     → 首帧 FileBind{session_id} ──────────────────► 分派器：会话校验
//!     ►► 通道泵（双向）：元数据面 announce ⇄ 拉流面 ◄◄
//!   通道任何失败 → 3s 退避独立重连（控制会话/剪贴板通道不受影响）
//! ```
//!
//! - **协商门控（docs/02 §4.6）**：会话 minor < 2 不拨号、不发送文件域帧——
//!   协商 minor 由 session_rt 写入 [`FileHub`]（HandshakeComplete 处理点），
//!   泵发送侧与客户端拨号循环共用该门控；
//! - **域隔离（docs/02 §4.6）**：文件域 kind 仅出现在文件连接——剪贴板通道
//!   收到文件域帧 = 剥出丢弃告警（`clip_channel` 解码剥离）；控制连接上由
//!   [`split_file_frames`] 在分派前剥离；
//! - **hub 状态跨重连存活**（同 clip 的 EchoFilter 进程级共享先例）：pending
//!   槽/在途拉取不随通道重建丢失；`FileRefRx` 去重防跨重连重复通知；
//! - **b-net 阶段边界**：announce/pending/拉取请求/软失败全链路已通；源端
//!   文件字节读取未接线（入站 `FilePullReq` 回 `FileError{SOURCE_GONE}`，
//!   待 b-app 落盘与读取面）；拉取触发面（托盘/粘贴拦截）同样待 b-app。

use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::watch;
use xcontrol_core::clock::SystemClock;
use xcontrol_core::file_ref::{FileRefRx, FileRefRxConfig, FileRefVerdict, manifest_fingerprint};
use xcontrol_core::file_rx::{FileRx, FileRxAction, FileRxConfig};
use xcontrol_core::file_serve::{FileServe, ServeVerdict};
use xcontrol_core::file_tx::FileTx;
use xcontrol_net::transport::TcpTransport;
use xcontrol_net::{HandshakeThrottle, NetError, serve};
use xcontrol_proto::file::{FileAck, FileBind, FileErrCode, FileError, FileMsg, FilePullReq};
use xcontrol_proto::{FileRefAnnounce, Message as _, frame};

/// 观测日志 target（文件域独立命名，诊断可分辨于剪贴板流量）。
pub const TARGET: &str = "session::file";

/// 文件通道独立重连退避：固定 3s（同剪贴板通道——通道失败只影响文件域）。
pub const FILE_CHANNEL_RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

/// 文件域所需的会话 minor（docs/02 §4.6：XCP 1.2 引入，随文件域 kind 门控）。
pub const FILE_MIN_MINOR: u8 = 2;

// ============================================================
// hub：跨重连/双端共享的文件域状态（进程级单例）
// ============================================================

/// 文件域共享状态（进程级单例，[`file_hub`] 取用；同 clip 的 shared_echo 先例）。
#[derive(Debug, Default)]
pub struct FileHub {
    /// 当前会话协商 minor（session_rt 在 HandshakeComplete 写入；None = 未协商）。
    session_minor: Option<u8>,
    /// 本机文件清单（b-app 在本机剪贴板出现文件引用时发布；泵发送后登记指纹节流）。
    outbound: Option<FileRefAnnounce>,
    outbound_fp: Option<[u8; 16]>,
    /// 对端最近通告的 pending 槽（代次失效/去重在泵内 [`FileRefRx`] 承接，
    /// docs/04 §8.2；本字段是托盘投影源，b-app 消费）。
    pending: Option<xcontrol_core::file_ref::PendingSlot>,
    /// 待发拉取请求（b-app 粘贴动作/测试写入；泵消费发出）。
    pull_request: Option<(u32, u32)>,
    /// 拉取落盘累积（b-net 无磁盘面：WriteChunk 数据在 hub 暂存供测试/检查；
    /// b-app 改为直写暂存文件）。
    pub staged: Vec<(u32, u32, Vec<u8>)>,
    /// 已校验完成的条目（transfer_id, entry）。
    pub verified: Vec<(u32, u32)>,
    /// 条目级软失败记录（可重试语义，docs/02 §4.6 防护规则 6）。
    pub failures: Vec<String>,
}

impl FileHub {
    /// 当前会话协商 minor。
    #[must_use]
    pub fn session_minor(&self) -> Option<u8> {
        self.session_minor
    }

    /// 会话协商完成（session_rt 写入；新会话覆盖旧值）。
    pub fn set_session_minor(&mut self, minor: u8) {
        self.session_minor = Some(minor);
    }

    /// 协商门控（docs/02 §4.6：minor ≥ 2 才允许文件域）。
    #[must_use]
    pub fn file_capable(&self) -> bool {
        self.session_minor
            .is_some_and(|minor| minor >= FILE_MIN_MINOR)
    }

    /// 本机清单（泵读取发送）。
    #[must_use]
    pub fn outbound(&self) -> Option<&FileRefAnnounce> {
        self.outbound.as_ref()
    }

    /// 发布本机清单（b-app 调用；指纹节流基线重置——新清单必发）。
    pub fn publish_manifest(&mut self, announce: FileRefAnnounce) {
        self.outbound_fp = None;
        self.outbound = Some(announce);
    }

    /// 发送指纹登记（泵发送后写，防每拍重发）。
    pub fn mark_outbound_sent(&mut self, fingerprint: [u8; 16]) {
        self.outbound_fp = Some(fingerprint);
    }

    /// 已发送指纹（泵节流判定）。
    #[must_use]
    pub fn outbound_sent_fp(&self) -> Option<[u8; 16]> {
        self.outbound_fp
    }

    /// 对端 pending 槽（托盘投影源）。
    #[must_use]
    pub fn pending(&self) -> Option<&xcontrol_core::file_ref::PendingSlot> {
        self.pending.as_ref()
    }

    /// pending 槽写入（泵在合法 announce 应用后调用）。
    pub fn set_pending(&mut self, slot: xcontrol_core::file_ref::PendingSlot) {
        self.pending = Some(slot);
    }

    /// 请求拉取一条目（b-app 粘贴动作/测试入口；offset 恒 0——断点续传切片 d）。
    pub fn request_pull(&mut self, transfer_id: u32, entry: u32) {
        self.pull_request = Some((transfer_id, entry));
    }

    /// 取走待发拉取请求（泵消费）。
    pub fn take_pull_request(&mut self) -> Option<(u32, u32)> {
        self.pull_request.take()
    }

    /// 清空会话期状态（控制会话结束随行清理；跨会话不得残留旧对端清单）。
    pub fn reset_for_new_session(&mut self) {
        self.pending = None;
        self.pull_request = None;
        self.staged.clear();
        self.verified.clear();
        self.failures.clear();
        self.outbound = None;
        self.outbound_fp = None;
        self.session_minor = None;
    }
}

/// 进程级 hub 单例（同 [`crate::clip_rt::shared_echo`] 的 OnceLock 先例——
/// 跨通道重连/跨会话重建存活，防重连后状态丢失）。
pub fn file_hub() -> &'static Mutex<FileHub> {
    static HUB: OnceLock<Mutex<FileHub>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(FileHub::default()))
}

/// hub 锁取用（poison 视为不可恢复——同仓库 `unwrap_or_else(PoisonError)` 惯例）。
pub fn lock_hub() -> std::sync::MutexGuard<'static, FileHub> {
    file_hub()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ============================================================
// host 侧：新连接首帧分派（FileBind）——由 clip_channel::classify_incoming 调用
// ============================================================

/// host 侧文件通道接入（`FileBind` 会话校验通过后由分派器调用）：spawn 泵任务，
/// 返回 guard（调用方持有；新通道替换旧通道时 close）。
pub fn accept_file_channel(
    transport: TcpTransport,
    control_session_id: [u8; 16],
    peer: SocketAddr,
) -> FileChannelGuard {
    tracing::info!(target: TARGET, peer = %peer,
        "文件通道接入（独立 TCP，FileBind 会话校验通过，docs/02 §4.6）");
    let (stop_tx, stop_rx) = watch::channel(false);
    let handle = tokio::spawn(run_file_pump(transport, control_session_id, stop_rx));
    FileChannelGuard { stop_tx, handle }
}

/// 文件通道泵任务的句柄（host 侧持有；客户端循环内泵直接 await 无需 guard）。
#[derive(Debug)]
pub struct FileChannelGuard {
    stop_tx: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl FileChannelGuard {
    /// 关闭通道：stop 信号 + abort 兜底（同 [`crate::clip_channel::ClipChannelGuard::close`]）。
    pub fn close(self) {
        let _ = self.stop_tx.send(true);
        self.handle.abort();
    }
}

// ============================================================
// 通道泵（双端共用）
// ============================================================

/// 通道明文流 → `FileMsg` 序列（通道上只允许文件域帧，docs/02 §4.6 域隔离）。
fn decode_file_channel_frames(stream: &[u8]) -> Result<Vec<FileMsg>, String> {
    use xcontrol_proto::frame::{FRAME_HEADER_LEN, decode_frame};
    let mut msgs = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        if rest.len() < FRAME_HEADER_LEN {
            return Err(format!("半帧头（{} 字节，违反一帧一报文契约）", rest.len()));
        }
        let frame = decode_frame(rest).map_err(|error| format!("帧解析失败：{error}"))?;
        let total = frame.total_len();
        match xcontrol_proto::file::decode_file(frame.header.kind, frame.payload) {
            Ok(Some(msg)) => msgs.push(msg),
            // kind 不在文件表：控制/剪贴板帧出现在文件通道 = 误路由（违规）。
            Ok(None) => {
                return Err(format!(
                    "非文件域帧 kind=0x{:02X}（误路由或违规，docs/02 §4.6）",
                    frame.header.kind
                ));
            }
            Err(error) => return Err(format!("文件域帧解码失败：{error}")),
        }
        rest = &rest[total..];
    }
    Ok(msgs)
}

/// 单条已建立文件通道的泵循环（双端共用）：入站（announce → pending 槽 /
/// PullReq → serve 校验 → 文件服务 / Chunk → 组装）+ 出站拍（200ms：本机清单
/// 推送、待发拉取、组装产出 Ack）+ 停止信号。
///
/// 任何通道失败 → 泵退出（客户端 3s 退避重连，host 等客户端重拨）；hub 状态
/// 跨重连存活（pending 槽与去重基线不随重建丢失）。
async fn run_file_pump(
    mut transport: TcpTransport,
    session_id: [u8; 16],
    mut stop: watch::Receiver<bool>,
) {
    if *stop.borrow_and_update() {
        return; // 通道建立即被要求停止（控制会话已收尾）
    }
    let _ = session_id; // 绑定由分派器首帧校验承接（同 clip 的本地预绑定语义）
    let mut serve = FileServe::new();
    let mut tx = FileTx::new();
    let mut rx = FileRx::new(FileRxConfig::default(), SystemClock::new());
    let mut ref_rx = FileRefRx::new(FileRefRxConfig::default());
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut wire_buf = Vec::new();
    // 帧序号（独立连接独立计数，帧层 seq 仅诊断语义，docs/02 §4.1）。
    let mut wire_seq: u32 = 0;
    let peer = transport.peer();
    tracing::info!(target: TARGET, %peer, "文件通道就绪（独立 TCP，元数据/拉流面开始）");
    loop {
        tokio::select! {
            // 停止信号（控制会话收尾 / host 换新通道）。
            _ = stop.changed() => break,

            // 入站：解密 → 全量解帧（通道上只允许文件域帧）→ 状态机。
            received = transport.recv(&mut wire_buf) => match received {
                Ok(()) => match decode_file_channel_frames(&wire_buf) {
                    Ok(msgs) => {
                        let mut out = Vec::new();
                        let mut violated = false;
                        for msg in msgs {
                            violated |= on_file_msg(
                                msg, &mut serve, &mut tx, &mut rx, &mut ref_rx, &mut out,
                            );
                        }
                        if violated {
                            // 违规断开文件通道（独立连接语义；控制/剪贴板不受影响）。
                            break;
                        }
                        if !out.is_empty()
                            && send_file_frames(&mut transport, out, &mut wire_seq)
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(target: TARGET, reason, %peer,
                            "文件通道帧违规（断开通道，3s 后重连）");
                        break;
                    }
                },
                Err(error) => {
                    tracing::info!(target: TARGET, %error, %peer,
                        "文件通道收帧失败（通道断开，待独立重连）");
                    break;
                }
            },

            // 出站拍：超时判定 + 本机清单推送 + 待发拉取。
            _ = ticker.tick() => {
                let mut out = Vec::new();
                // ① 接收端超时判定（空闲 10s / 总时长 30min → 条目软失败；
                // on_tick 不产 Violation——记录面即可）
                for action in rx.on_tick() {
                    record_rx_action(action, &mut out, &mut serve, &mut tx);
                }
                // ② 本机清单推送（协商门控 + 指纹节流；announce 与 serve 注册
                // 同拍——后续 PullReq 校验依据；hub 清单保留，重连后按指纹不重发，
                // 新会话 reset 后重发）
                let announce = {
                    let hub = lock_hub();
                    hub.outbound()
                        .cloned()
                        .filter(|_| hub.file_capable())
                };
                if let Some(announce) = announce {
                    let fingerprint = manifest_fingerprint(&announce.entries);
                    let already_sent =
                        lock_hub().outbound_sent_fp() == Some(fingerprint);
                    if !already_sent {
                        serve.register(&announce);
                        lock_hub().mark_outbound_sent(fingerprint);
                        out.push(FileMsg::RefAnnounce(announce));
                    }
                }
                // ③ 待发拉取请求（size 取自 pending 槽清单；协商门控同面）
                let pull = {
                    let mut hub = lock_hub();
                    hub.take_pull_request().filter(|_| hub.file_capable())
                };
                if let Some((transfer_id, entry)) = pull {
                    let size = lock_hub()
                        .pending()
                        .and_then(|slot| slot.entries.get(entry as usize))
                        .map(|entry| entry.size);
                    match size {
                        Some(size) => {
                            rx.begin_pull(transfer_id, entry, size);
                            // 发送侧条目流同拍开账（b-app 阶段承接真实分块发送）
                            let _ = tx.begin_entry(transfer_id, entry, size);
                            out.push(FileMsg::PullReq(FilePullReq {
                                transfer_id,
                                entry,
                                offset: 0,
                            }));
                        }
                        None => lock_hub().failures.push(format!(
                            "拉取请求越界：transfer {transfer_id} entry {entry}（pending 槽无此条目）"
                        )),
                    }
                }
                if !out.is_empty()
                    && send_file_frames(&mut transport, out, &mut wire_seq)
                        .await
                        .is_err()
                {
                    break;
                }
            },
        }
    }
    transport.shutdown().await;
    tracing::info!(target: TARGET, %peer, "文件通道关闭（重连由调用方按 3s 退避承接）");
}

/// 单条入站文件域消息处理（返回违规标志：true = 断开文件通道）。
fn on_file_msg(
    msg: FileMsg,
    serve: &mut FileServe,
    tx: &mut FileTx,
    rx: &mut FileRx<SystemClock>,
    ref_rx: &mut FileRefRx,
    out: &mut Vec<FileMsg>,
) -> bool {
    match msg {
        // 元数据面：入 pending 槽（代次失效/去重在 FileRefRx 内承接，docs/04 §8.2）。
        FileMsg::RefAnnounce(announce) => match ref_rx.on_announce(announce) {
            FileRefVerdict::Applied { .. } => {
                if let Some(slot) = ref_rx.slot() {
                    lock_hub().set_pending(slot.clone());
                }
                false
            }
            FileRefVerdict::Duplicate => false,
            FileRefVerdict::Rejected { reason } => {
                tracing::warn!(target: TARGET, reason, "文件清单被拒绝（策略面，不断连）");
                false
            }
        },
        // 拉流面（源端角色）：PullReq 校验 → 文件服务。b-net 阶段无磁盘读取面
        // ——回条目级软错误（可重试），待 b-app 接入读取/分块。
        FileMsg::PullReq(req) => match serve.on_pull(&req) {
            ServeVerdict::Grant {
                transfer_id, entry, ..
            } => {
                tracing::warn!(target: TARGET, transfer_id, entry,
                    "文件服务未接线（b-app 待落地）：回 FileError{{SOURCE_GONE}}");
                out.push(FileMsg::Error(FileError {
                    transfer_id,
                    entry: Some(entry),
                    code: FileErrCode::SOURCE_GONE,
                }));
                serve.release(transfer_id);
                false
            }
            ServeVerdict::Violation { reason } => {
                tracing::warn!(target: TARGET, reason,
                    "PullReq 违规（断开文件通道，docs/07 §9 索引寻址红线）");
                true
            }
        },
        // 拉流面（接收端角色）：组装/校验/软失败。
        FileMsg::Chunk(chunk) => {
            let mut violated = false;
            for action in rx.on_chunk(&chunk) {
                violated |= record_rx_action(action, out, serve, tx);
            }
            violated
        }
        FileMsg::End(end) => {
            let mut violated = false;
            for action in rx.on_end(&end) {
                violated |= record_rx_action(action, out, serve, tx);
            }
            violated
        }
        FileMsg::Error(error) => {
            let mut violated = false;
            for action in rx.on_error(&error) {
                violated |= record_rx_action(action, out, serve, tx);
            }
            violated
        }
        FileMsg::Cancel(cancel) => {
            let mut violated = false;
            for action in rx.on_cancel(&cancel) {
                violated |= record_rx_action(action, out, serve, tx);
            }
            violated
        }
        // FileBind：连接首帧由分派器消费，泵内再现 = 违规（绑定唯一）。
        FileMsg::Bind(_) => {
            tracing::warn!(target: TARGET, "文件通道内重复 FileBind（绑定唯一，违规）");
            true
        }
        // Expand：子树展开（切片 d 目录面）——登记校验面已备（serve），本阶段
        // 不会被对端发出；收到按登记处理（base 不连续 = 违规断开）。
        FileMsg::Expand(expand) => {
            if let Err(reason) = serve.register_expand(&expand) {
                tracing::warn!(target: TARGET, reason, "FileExpand 下标不连续（违规）");
                true
            } else {
                false
            }
        }
        // 信用归还（源端角色）——b-net 无出站块，多余 Ack 无状态影响（on_ack
        // 未知 seq 返回 false，忽略）。
        FileMsg::Ack(ack) => {
            let _ = tx.on_ack(ack.seq);
            false
        }
    }
}

/// 接收端动作落位：Ack → 出站；WriteChunk/Verified/Failed → hub 记录；
/// Violation → 返回 true（调用方断开文件通道）。
fn record_rx_action(
    action: FileRxAction,
    out: &mut Vec<FileMsg>,
    serve: &mut FileServe,
    tx: &mut FileTx,
) -> bool {
    match action {
        FileRxAction::SendAck { seq } => {
            out.push(FileMsg::Ack(FileAck { seq }));
            false
        }
        FileRxAction::WriteChunk {
            transfer_id,
            entry,
            data,
        } => {
            lock_hub().staged.push((transfer_id, entry, data));
            false
        }
        FileRxAction::EntryVerified { transfer_id, entry } => {
            lock_hub().verified.push((transfer_id, entry));
            serve.release(transfer_id);
            // 条目流收尾（发满才 Verified；take 失败即 reset 兜底）
            if tx.should_end() {
                let _ = tx.take_stream();
            } else {
                tx.reset();
            }
            false
        }
        FileRxAction::EntryFailed {
            transfer_id,
            reason,
            ..
        } => {
            lock_hub()
                .failures
                .push(format!("transfer {transfer_id}：{reason}"));
            serve.release(transfer_id);
            tx.reset();
            false
        }
        FileRxAction::Violation { reason } => {
            tracing::warn!(target: TARGET, reason, "文件通道接收端违规（断开）");
            true
        }
    }
}

/// 文件域帧编码 + 逐帧独立发送（每帧一个 Noise 报文——11-20 帧粘包修复语义；
/// 背压让步同 clip 通道：每 4 帧让 TCP 缓冲排空一拍）。
async fn send_file_frames(
    transport: &mut TcpTransport,
    msgs: Vec<FileMsg>,
    wire_seq: &mut u32,
) -> Result<(), NetError> {
    let mut count = 0usize;
    for msg in msgs {
        let kind = msg.as_message().kind();
        let mut single = Vec::new();
        if let Err(error) = msg.as_message().encode_frame_into(*wire_seq, &mut single) {
            tracing::warn!(target: TARGET, kind, %error, "文件域出站帧编码失败（丢弃）");
            continue;
        }
        tracing::debug!(target: TARGET, kind, len = single.len(), "文件域出站帧");
        *wire_seq = wire_seq.wrapping_add(1);
        if let Err(error) = transport.send(&single).await {
            tracing::warn!(target: TARGET, %error, "文件域帧发送失败（通道断开）");
            return Err(error);
        }
        count += 1;
        if count.is_multiple_of(4) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    Ok(())
}

// ============================================================
// 客户端侧：通道拨号 + 独立重连循环
// ============================================================

/// 客户端侧文件通道循环：控制会话存活期间独立维持一条文件 TCP（docs/02 §4.6）。
/// 协商 minor < 2 时不拨号（静默等 stop / 等 minor 就绪）；任何失败 → 3s 退避
/// 独立重连；控制会话结束 → 退出。
pub async fn client_file_channel_loop(
    addr: SocketAddr,
    psk: [u8; 32],
    throttle_config: xcontrol_net::ThrottleConfig,
    session_id: [u8; 16],
    mut stop: watch::Receiver<bool>,
) {
    if *stop.borrow_and_update() {
        return;
    }
    // 独立限速实例：通道拨号的成败不与控制拨号互记（同 clip 通道纪律）。
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(throttle_config);
    loop {
        if *stop.borrow_and_update() {
            break;
        }
        // 协商门控（docs/02 §4.6）：minor 未就绪或 < 2 → 等待（200ms 拍），
        // 不拨号（1.0/1.1 对端不建文件连接）。
        if !lock_hub().file_capable() {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
                _ = stop.changed() => break,
            }
        }
        match dial_file_channel(addr, psk, &mut throttle, session_id).await {
            Ok(transport) => {
                run_file_pump(transport, session_id, stop.clone()).await;
                if *stop.borrow_and_update() {
                    break;
                }
                tracing::info!(target: TARGET, addr = %addr,
                    "文件通道断开：3s 后独立重连（控制会话不受影响）");
            }
            Err(error) => {
                tracing::warn!(target: TARGET, addr = %addr, %error,
                    "文件通道拨号失败（3s 后重试）");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(FILE_CHANNEL_RECONNECT_INTERVAL) => {}
            _ = stop.changed() => break,
        }
    }
    tracing::info!(target: TARGET, addr = %addr, "文件通道循环退出（控制会话收尾）");
}

/// 单次文件通道拨号：TCP + 独立 Noise 握手 → 首帧 `FileBind{session_id}`
/// （host 校验会话一致后静默——单工绑定确认，同 ChannelBind）。
async fn dial_file_channel(
    addr: SocketAddr,
    psk: [u8; 32],
    throttle: &mut HandshakeThrottle<SocketAddr>,
    session_id: [u8; 16],
) -> anyhow::Result<TcpTransport> {
    let mut connection = serve::connect(addr, &psk, throttle)
        .await
        .map_err(|error| anyhow::anyhow!("文件通道握手失败：{error}"))?;
    let bind = FileBind { session_id };
    let frame = bind
        .encode_frame(0)
        .map_err(|error| anyhow::anyhow!("FileBind 编码失败：{error}"))?;
    connection
        .transport
        .send(&frame)
        .await
        .map_err(|error| anyhow::anyhow!("FileBind 发送失败：{error}"))?;
    Ok(connection.transport)
}

// ============================================================
// 控制连接防御面：文件域帧剥离（docs/02 §4.6 域隔离）
// ============================================================

/// 控制连接明文流中的文件域帧剥离（出现即对端误路由/残留——剥出丢弃告警，
/// **不断连**；同 [`crate::clip_rt::split_clip_frames`] 防御面）。返回剥离
/// 文件域帧后的流（原样交会话分派器）与剥离帧 kind 列表（告警用）。
#[must_use]
pub fn split_file_frames(stream: &[u8]) -> (Vec<u8>, Vec<u8>) {
    use xcontrol_proto::frame::{FRAME_HEADER_LEN, decode_frame};
    let mut rest_stream = Vec::with_capacity(stream.len());
    let mut dropped = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        if rest.len() < FRAME_HEADER_LEN {
            rest_stream.extend_from_slice(rest);
            break;
        }
        let Ok(frame) = decode_frame(rest) else {
            rest_stream.extend_from_slice(rest);
            break;
        };
        let total = frame.total_len();
        if total > rest.len() {
            rest_stream.extend_from_slice(rest);
            break;
        }
        if frame::is_file_kind(frame.header.kind) {
            dropped.push(frame.header.kind);
        } else {
            rest_stream.extend_from_slice(&rest[..total]);
        }
        rest = &rest[total..];
    }
    (rest_stream, dropped)
}

// ============================================================
// 单测（纯逻辑面：通道帧解码 / 控制连接剥离）
// ============================================================

#[cfg(test)]
mod tests {
    use xcontrol_proto::FileRefAnnounce;

    use super::*;

    fn announce() -> FileRefAnnounce {
        FileRefAnnounce {
            transfer_id: 7,
            origin_id: [1; 16],
            origin_seq: 1,
            entries: vec![xcontrol_proto::FileEntry {
                name: "a.bin".to_owned(),
                size: 10,
                mtime_ms: 0,
                kind: xcontrol_proto::FileEntryKind::File,
                children: 0,
            }],
            total_bytes: 10,
            copied_at_ms: 0,
        }
    }

    /// 通道帧解码：合法文件域序列全解出（保序）。
    #[test]
    fn decode_channel_frames_accepts_file_sequence() {
        let bind = FileBind {
            session_id: [1; 16],
        }
        .encode_frame(0)
        .expect("bind 编码");
        let announce = announce().encode_frame(1).expect("announce 编码");
        let req = FilePullReq {
            transfer_id: 7,
            entry: 0,
            offset: 0,
        }
        .encode_frame(2)
        .expect("pull 编码");
        let ack = FileAck { seq: 0 }.encode_frame(3).expect("ack 编码");
        let mut stream = bind;
        stream.extend_from_slice(&announce);
        stream.extend_from_slice(&req);
        stream.extend_from_slice(&ack);
        let msgs = decode_file_channel_frames(&stream).expect("合法序列");
        assert_eq!(msgs.len(), 4, "四帧全解出");
        assert!(matches!(msgs[0], FileMsg::Bind(_)));
        assert!(matches!(msgs[1], FileMsg::RefAnnounce(_)));
        assert!(matches!(msgs[2], FileMsg::PullReq(_)));
        assert!(matches!(msgs[3], FileMsg::Ack(_)));
    }

    /// 通道帧解码：剪贴板/控制帧 = 误路由违规（文件通道只收文件域）。
    #[test]
    fn decode_channel_frames_rejects_non_file_frame() {
        use xcontrol_proto::frame::encode_frame;
        let clip_ack = xcontrol_proto::ClipAck { seq: 1 }
            .encode_frame(1)
            .expect("clip ack 编码");
        let error = decode_file_channel_frames(&clip_ack).expect_err("剪贴板帧必须违规");
        assert!(error.contains("非文件域帧"), "{error}");
        let ping =
            encode_frame(1, xcontrol_proto::frame::kind::CTRL_PING, &[0x00]).expect("ping 编码");
        assert!(decode_file_channel_frames(&ping).is_err(), "控制帧违规");
    }

    /// 通道帧解码：半帧 / 垃圾字节 = 违规。
    #[test]
    fn decode_channel_frames_rejects_malformed_stream() {
        assert!(decode_file_channel_frames(&[0x00, 0x01]).is_err(), "半帧头");
        assert!(
            decode_file_channel_frames(b"garbage!!").is_err(),
            "垃圾字节"
        );
    }

    /// 控制连接剥离（域隔离防御面）：文件域帧剥出、其余帧原样保留、
    /// 半帧/垃圾尾部原样交回（分派器自有防御面处理）。
    #[test]
    fn split_file_frames_strips_file_domain_only() {
        let key = xcontrol_proto::InputEvent::Key {
            code: xcontrol_proto::xkey::KEY_A,
            state: xcontrol_proto::KeyState::Down,
        }
        .encode_frame(1)
        .expect("key 编码");
        let announce = announce().encode_frame(2).expect("announce 编码");
        let ping =
            xcontrol_proto::frame::encode_frame(3, xcontrol_proto::frame::kind::CTRL_PING, &[0])
                .expect("ping 编码");
        let mut mixed = key.clone();
        mixed.extend_from_slice(&announce);
        mixed.extend_from_slice(&ping);
        let (rest, dropped) = split_file_frames(&mixed);
        let _ = &key;
        assert_eq!(
            dropped,
            vec![xcontrol_proto::frame::kind::FILE_REF_ANNOUNCE]
        );
        let mut expected = key.clone();
        expected.extend_from_slice(&ping);
        assert_eq!(rest, expected, "非文件域帧原样保留");
        // 半帧尾：原样交回
        let mut truncated = key;
        truncated.extend_from_slice(&[0x01, 0x02]);
        let (rest, dropped) = split_file_frames(&truncated);
        assert!(dropped.is_empty());
        assert_eq!(rest, truncated);
    }
}
