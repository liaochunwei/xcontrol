//! `clip_channel` — 独立剪贴板 TCP 通道（FR-4.x，docs/02 §0/§4.4 双连接设计，
//! 2026-11-21 回归——同连接裁定被取代，见 clip_rt 模块文档）。
//!
//! ```text
//! 客户端（从机）                                     主控（host）
//!   控制连接（已有，键鼠/控制/心跳） ──────────────────► drive_session
//!   另拨一条 TCP（同端口）→ 独立 Noise 握手（独立 nonce 流）
//!     → 首帧 ChannelBind{session_id} ────────────────► 分派器：会话校验
//!     ►► 通道泵（双向）：poll_local_changes ⇄ on_clip_msg ◄◄
//!   通道任何失败 → 3s 退避独立重连（控制会话与输入转发完全不受影响）
//! ```
//!
//! - **角色分派（host）**：剪贴板连接的客户端建连后**立即**发 `ChannelBind`
//!   首帧（docs/02 §4.4）；控制连接的客户端则**静默**等待主控先发 `Hello`
//!   （docs/02 §5）——[`classify_incoming`] 以窥探窗口内有无首帧判角色：
//!   `ChannelBind` 且会话匹配 → 剪贴板通道（spawn [`run_clip_pump`]）；窗口
//!   内无帧 → 控制连接（原样交付给会话驱动，首帧未被消费）；其余 → 违规
//!   断开（对端只见断开，docs/02 §0）；
//! - **会话校验**：`ChannelBind.session_id` 必须与当前控制会话一致（不一致
//!   断开）——接收状态机按控制会话 id 本地预绑定（`ClipboardRuntime::new`，
//!   `ClipRx::on_bind` 入口复用），绑定唯一由「此后收到 Bind = 违规」承接；
//! - **多条剪贴板连接**：新通道到来时关闭旧通道（新连接优先，防堆积）；
//! - **失败隔离（本任务根因修复）**：通道任何失败（解密/分帧/超时/违规/
//!   发送）只断开剪贴板通道——[`run_clip_pump`] 退出，客户端循环 3s 后独立
//!   重连；控制会话与输入转发不受任何影响（大图分块流不再与键鼠帧队头阻塞）；
//! - **重连语义**：固定 3s 退避（独立于控制会话的指数退避）；重连成功 =
//!   `ClipboardRuntime` 重建（EchoFilter 进程级共享基线保留——已交付内容
//!   不重发；`seed_last_sent` 在通道建立时登记当前剪贴板基线——连接后的
//!   新复制才同步）；
//! - **限速独立**：客户端通道拨号用独立 [`HandshakeThrottle`] 实例（与控制
//!   拨号互不计数——通道重连不拖累控制重连）；
//! - **观测日志**：通道接入/就绪/断开/重连各一条（`session::clip` target），
//!   出站/交付观测在 clip_rt（状态机层）。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use xcontrol_net::transport::TcpTransport;
use xcontrol_net::{Connection, HandshakeThrottle, NetError, serve};
use xcontrol_proto::Message as _;
use xcontrol_proto::clip::{ChannelBind, ClipMsg};

use crate::clip_rt::{self, TARGET};

/// 剪贴板通道独立重连退避：固定 3s（docs/02 §4.4 双连接语义——通道失败只
/// 影响剪贴板，3s 后自愈；控制会话的指数退避独立运行，互不耦合）。
pub const CLIP_CHANNEL_RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

/// host 侧新连接首帧窥探窗口：剪贴板连接的客户端建连后**立即**发送
/// `ChannelBind` 首帧，控制连接的客户端**静默**等 `Hello`——窗口内无帧即
/// 判定为控制连接。窗口权衡：过短 → 慢调度下剪贴板连接被误判为控制（客户端
/// 泵收到误发的 `Hello` 自愈断开重连——自愈路径见 clip_channel 模块文档）；
/// 过长 → 每条控制连接接入延迟增加。300ms 对局域网建连（握手后 ms 级发帧）
/// 有充分裕量，控制接入的额外延迟可忽略。
pub const CLIP_BIND_PEEK_WINDOW: Duration = Duration::from_millis(300);

// ============================================================
// host 侧：新连接首帧分派（剪贴板通道 / 控制连接 / 拒绝）
// ============================================================

/// host 侧新连接的角色分派结果（[`classify_incoming`] 产出）。
#[derive(Debug)]
pub enum IncomingClass {
    /// 剪贴板通道（`ChannelBind` 校验通过，泵任务已 spawn）。
    ClipChannel(ClipChannelGuard),
    /// 控制连接（首帧未消费——客户端静默等 `Hello`，原样交付给会话驱动；
    /// 会话期间到达时由调用方持有，当前会话结束后优先驱动）。
    /// `Box` 收敛变体尺寸差（`Connection` 携带 stream + 传输态）。
    Control(Box<Connection>),
    /// 已拒绝并断开（首帧违规 / 会话不匹配 / 读帧失败）。
    Rejected,
}

/// 首帧分类（窥探读出的明文恰为一个完整应用帧——一帧一 Noise 报文契约）。
enum FirstFrame {
    /// `ChannelBind`（剪贴板连接的合法首帧）。
    ChannelBind(ChannelBind),
    /// 其他剪贴板帧（0x51–0x54）——未 bind 先传，违规。
    ClipWithoutBind(u8),
    /// 非剪贴板帧——控制连接不应有首帧（客户端静默等 `Hello`），违规面。
    NonClip(u8),
}

/// 明文首帧分类（解析失败 = `None`，违规）。
fn first_frame_class(buf: &[u8]) -> Option<FirstFrame> {
    use xcontrol_proto::frame::{decode_frame, is_clip_kind, kind};
    let frame = decode_frame(buf).ok()?;
    let kind_value = frame.header.kind;
    if kind_value == kind::CLIP_CHANNEL_BIND {
        match xcontrol_proto::clip::decode_clip(kind_value, frame.payload) {
            Ok(Some(ClipMsg::Bind(bind))) => Some(FirstFrame::ChannelBind(bind)),
            // 声称 bind 却解码失败 = 违规面。
            _ => None,
        }
    } else if is_clip_kind(kind_value) {
        Some(FirstFrame::ClipWithoutBind(kind_value))
    } else {
        Some(FirstFrame::NonClip(kind_value))
    }
}

/// host 侧新连接首帧分派（docs/02 §4.4：剪贴板连接首帧 `ChannelBind`）。
///
/// - 窥探窗口内收到 `ChannelBind` 且会话与当前控制会话一致 → 剪贴板通道
///   （spawn 泵任务，[`IncomingClass::ClipChannel`]）；
/// - `ChannelBind` 会话不匹配 / 未 bind 先传 / 非剪贴板首帧 / 解析失败 →
///   违规断开（[`IncomingClass::Rejected`]，对端只见断开）；
/// - 窗口内无帧 → 控制连接（[`IncomingClass::Control`]——客户端握手后静默
///   等主控先发 `Hello`，超时取消发生在读到任何字节前，连接原样交付）。
pub async fn classify_incoming(
    connection: Connection,
    control_session_id: [u8; 16],
    max_bytes: u64,
    peek_window: Duration,
) -> IncomingClass {
    let Connection {
        mut transport,
        session_id: conn_session_id,
        peer,
    } = connection;
    let mut buf = Vec::new();
    match tokio::time::timeout(peek_window, transport.recv(&mut buf)).await {
        Ok(Ok(())) => match first_frame_class(&buf) {
            Some(FirstFrame::ChannelBind(bind)) => {
                if bind.session_id != control_session_id {
                    tracing::warn!(target: TARGET, peer = %peer,
                        "剪贴板通道 ChannelBind 会话不匹配（断开该连接，docs/02 §4.4）");
                    transport.shutdown().await;
                    return IncomingClass::Rejected;
                }
                tracing::info!(target: TARGET, peer = %peer,
                    "剪贴板通道接入（独立 TCP，ChannelBind 会话校验通过）");
                let (stop_tx, stop_rx) = watch::channel(false);
                let handle = tokio::spawn(run_clip_pump(
                    transport,
                    control_session_id,
                    max_bytes,
                    stop_rx,
                ));
                IncomingClass::ClipChannel(ClipChannelGuard { stop_tx, handle })
            }
            Some(FirstFrame::ClipWithoutBind(kind_value)) => {
                tracing::warn!(target: TARGET, peer = %peer,
                    kind = format!("0x{kind_value:02X}"),
                    "剪贴板连接首帧非 ChannelBind（未绑定先传，违规断开，docs/02 §4.4）");
                transport.shutdown().await;
                IncomingClass::Rejected
            }
            Some(FirstFrame::NonClip(kind_value)) => {
                tracing::warn!(target: TARGET, peer = %peer,
                    kind = format!("0x{kind_value:02X}"),
                    "新连接首帧为非剪贴板帧（控制连接静默等 Hello——不应有首帧；断开）");
                transport.shutdown().await;
                IncomingClass::Rejected
            }
            None => {
                tracing::warn!(target: TARGET, peer = %peer,
                    "新连接首帧解析失败（违规断开）");
                transport.shutdown().await;
                IncomingClass::Rejected
            }
        },
        // 窗口内无帧：控制连接（客户端握手后静默，等主控先发 Hello，docs/02 §5）。
        Err(_elapsed) => IncomingClass::Control(Box::new(Connection {
            transport,
            session_id: conn_session_id,
            peer,
        })),
        Ok(Err(error)) => {
            tracing::debug!(target: TARGET, peer = %peer, %error,
                "新连接首帧读取失败（断开）");
            transport.shutdown().await;
            IncomingClass::Rejected
        }
    }
}

// ============================================================
// 通道泵（双端共用）：单条已建立通道的双向驱动
// ============================================================

/// 剪贴板通道泵任务的句柄（host 侧持有）：控制会话收尾或新通道替换时关闭。
#[derive(Debug)]
pub struct ClipChannelGuard {
    stop_tx: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl ClipChannelGuard {
    /// 关闭通道：stop 信号 + abort 兜底（泵的 select 监听 stop，通常优雅
    /// 退出；abort 防卡死——所有 await 点可安全取消，平台剪贴板操作为同步
    /// 调用不受取消影响）。transport 随泵退出 drop → TCP FIN。
    pub fn close(self) {
        let _ = self.stop_tx.send(true);
        self.handle.abort();
    }
}

/// 通道明文流 → `ClipMsg` 序列（通道上只允许剪贴板帧，docs/02 §4.4）。
///
/// 非剪贴板帧（误路由：对端把本通道当控制连接用）/ 剪贴板帧解码失败 / 半帧
/// = 违规（`Err`，泵断开通道重连——独立连接语义：违规只断剪贴板）。
fn decode_clip_channel_frames(stream: &[u8]) -> Result<Vec<ClipMsg>, String> {
    use xcontrol_proto::frame::{FRAME_HEADER_LEN, decode_frame};
    let mut msgs = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        if rest.len() < FRAME_HEADER_LEN {
            return Err(format!("半帧头（{} 字节，违反一帧一报文契约）", rest.len()));
        }
        let frame = decode_frame(rest).map_err(|error| format!("帧解析失败：{error}"))?;
        let total = frame.total_len();
        match xcontrol_proto::clip::decode_clip(frame.header.kind, frame.payload) {
            Ok(Some(msg)) => msgs.push(msg),
            // kind 不在剪贴板表：控制/输入帧出现在剪贴板通道 = 误路由（违规）。
            Ok(None) => {
                return Err(format!(
                    "非剪贴板帧 kind=0x{:02X}（误路由或违规，docs/02 §4.4）",
                    frame.header.kind
                ));
            }
            Err(error) => return Err(format!("剪贴板帧解码失败：{error}")),
        }
        rest = &rest[total..];
    }
    Ok(msgs)
}

/// 单条已建立通道的泵循环（双端共用）：入站（recv → 状态机 → 交付/ack 出站）
/// + 出站（200ms 拍：watcher drain → 本机变化 → 出站帧）+ 停止信号。
///
/// 任何通道失败（收帧错误 / 帧违规 / 发送失败）→ 泵退出（transport 随之
/// 关闭；调用方断连重连——客户端 3s 退避，host 等客户端重拨）。运行时实例
/// 每次泵启动新建（watcher/后端随 Drop 回收；EchoFilter 进程级共享基线跨
/// 重建存活）。
pub(crate) async fn run_clip_pump(
    mut transport: TcpTransport,
    session_id: [u8; 16],
    max_bytes: u64,
    mut stop: watch::Receiver<bool>,
) {
    if *stop.borrow_and_update() {
        return; // 通道建立即被要求停止（控制会话已收尾）
    }
    let mut clip_rt = clip_rt::ClipboardRuntime::new(max_bytes, session_id, clip_rt::shared_echo());
    let mut ticker = tokio::time::interval(clip_rt::CLIP_TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut wire_buf = Vec::new();
    // 帧序号（与控制连接 seq 空间无关——独立连接独立计数，帧层 seq 仅诊断
    // 语义，docs/02 §4.1）。
    let mut clip_seq: u32 = 0;
    let peer = transport.peer();
    tracing::info!(target: TARGET, %peer, "剪贴板通道就绪（独立 TCP，双向同步开始）");
    loop {
        tokio::select! {
            // 停止信号（控制会话收尾 / host 换新通道；sender drop 同效）。
            _ = stop.changed() => break,

            // 入站：解密 → 全量解帧（通道上只允许剪贴板帧）→ 状态机。
            received = transport.recv(&mut wire_buf) => match received {
                Ok(()) => match decode_clip_channel_frames(&wire_buf) {
                    Ok(msgs) => {
                        let mut out = Vec::new();
                        let mut violated = false;
                        for msg in msgs {
                            violated |= clip_rt.on_clip_msg(msg, &mut out);
                        }
                        if violated {
                            // 违规断开剪贴板通道（docs/02 §6.6 独立连接语义恢复；
                            // 控制会话与输入转发不受影响）。
                            break;
                        }
                        if !out.is_empty()
                            && send_clip_frames(&mut transport, out, &mut clip_seq)
                                .await
                                .is_err()
                        {
                            break; // 发送失败 = 通道不可用（send_clip_frames 已告警）
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(target: TARGET, reason, %peer,
                            "剪贴板通道帧违规（断开通道，3s 后重连）");
                        break;
                    }
                },
                Err(error) => {
                    tracing::info!(target: TARGET, %error, %peer,
                        "剪贴板通道收帧失败（通道断开，待独立重连）");
                    break;
                }
            },

            // 出站拍：接收端 10s 超时判定 + 本机变化检测出站（平台读写全在
            // 本拍执行，监听线程零剪贴板操作——platform-code.md 回调线程纪律）。
            _ = ticker.tick() => {
                let mut out = Vec::new();
                if clip_rt.on_tick(&mut out) {
                    break;
                }
                clip_rt.poll_local_changes(&mut out, Instant::now());
                if !out.is_empty()
                    && send_clip_frames(&mut transport, out, &mut clip_seq)
                        .await
                        .is_err()
                {
                    break;
                }
            },
        }
    }
    clip_rt.reset_outbound();
    transport.shutdown().await;
    tracing::info!(target: TARGET, %peer, "剪贴板通道关闭（重连由调用方按 3s 退避承接）");
}

/// 剪贴板帧编码 + 逐帧独立发送（每帧一个 Noise 报文——11-20 帧粘包修复的
/// 语义保留：多帧合包会在接收端按帧头只认第 1 帧后错位）。
///
/// 编码失败单帧丢弃（告警）；传输失败上抛（泵断开通道）。背压让步保留：
/// 每 4 块让 TCP 缓冲排空一拍（10ms），防连续 65KB 帧 × 快速 send 造成
/// 对端 read 缓冲雪崩（独立通道上雪崩只影响剪贴板，仍不值得付出重连成本）。
async fn send_clip_frames(
    transport: &mut TcpTransport,
    msgs: Vec<ClipMsg>,
    clip_seq: &mut u32,
) -> Result<(), NetError> {
    let mut chunk_count = 0usize;
    for msg in msgs {
        let kind = msg.as_message().kind();
        let mut single = Vec::new();
        if let Err(error) = msg.as_message().encode_frame_into(*clip_seq, &mut single) {
            tracing::warn!(target: TARGET, kind, %error, "剪贴板出站帧编码失败（丢弃）");
            continue;
        }
        tracing::debug!(target: TARGET, kind, len = single.len(), "剪贴板出站帧");
        *clip_seq = clip_seq.wrapping_add(1);
        if let Err(error) = transport.send(&single).await {
            tracing::warn!(target: TARGET, %error, "剪贴板帧发送失败（通道断开）");
            return Err(error);
        }
        chunk_count += 1;
        if chunk_count.is_multiple_of(4) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    Ok(())
}

// ============================================================
// 客户端侧：通道拨号 + 独立重连循环
// ============================================================

/// 客户端侧剪贴板通道循环：控制会话存活期间独立维持一条剪贴板 TCP
/// （docs/02 §0 双连接）。任何失败（拨号/握手/`ChannelBind`/泵）→ 3s 退避
/// 独立重连；控制会话结束（stop 信号 / sender drop）→ 退出。
///
/// `max_bytes = 0`（FR-4.5 禁用）不建通道（静默等 stop——对端也不会收到
/// 本端出站，同步语义为「双向禁用」）。
pub async fn client_clip_channel_loop(
    addr: SocketAddr,
    psk: [u8; 32],
    throttle_config: xcontrol_net::ThrottleConfig,
    session_id: [u8; 16],
    max_bytes: u64,
    mut stop: watch::Receiver<bool>,
) {
    if *stop.borrow_and_update() {
        return;
    }
    if max_bytes == 0 {
        tracing::info!(target: TARGET,
            "剪贴板同步已禁用（max_bytes=0，FR-4.5）：不建立剪贴板通道");
        let _ = stop.changed().await;
        return;
    }
    // 独立限速实例：通道拨号的成败不与控制拨号互记（通道重连不拖累控制重连；
    // host 侧 per-IP 限速仍覆盖全部握手——3s 退避保证不构成爆破节奏）。
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(throttle_config);
    loop {
        if *stop.borrow_and_update() {
            break;
        }
        match dial_clip_channel(addr, psk, &mut throttle, session_id).await {
            Ok(transport) => {
                run_clip_pump(transport, session_id, max_bytes, stop.clone()).await;
                if *stop.borrow_and_update() {
                    break;
                }
                tracing::info!(target: TARGET, addr = %addr,
                    "剪贴板通道断开：3s 后独立重连（控制会话不受影响）");
            }
            Err(error) => {
                tracing::warn!(target: TARGET, addr = %addr, %error,
                    "剪贴板通道拨号失败（3s 后重试）");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(CLIP_CHANNEL_RECONNECT_INTERVAL) => {}
            _ = stop.changed() => break,
        }
    }
    tracing::info!(target: TARGET, addr = %addr,
        "剪贴板通道循环退出（控制会话收尾）");
}

/// 单次剪贴板通道拨号：TCP + **独立** Noise 握手（docs/02 §0：两条连接分别
/// 独立握手——独立 nonce 流，任何一侧的抖动不污染另一侧）→ 首帧
/// `ChannelBind{session_id}`（绑定控制会话，docs/02 §4.4；host 校验会话一致
/// 后静默——单工绑定确认，无响应帧）。
async fn dial_clip_channel(
    addr: SocketAddr,
    psk: [u8; 32],
    throttle: &mut HandshakeThrottle<SocketAddr>,
    session_id: [u8; 16],
) -> anyhow::Result<TcpTransport> {
    let mut connection = serve::connect(addr, &psk, throttle)
        .await
        .map_err(|error| anyhow::anyhow!("剪贴板通道握手失败：{error}"))?;
    let bind = ChannelBind { session_id };
    let frame = bind
        .encode_frame(0)
        .map_err(|error| anyhow::anyhow!("ChannelBind 编码失败：{error}"))?;
    connection
        .transport
        .send(&frame)
        .await
        .map_err(|error| anyhow::anyhow!("ChannelBind 发送失败：{error}"))?;
    Ok(connection.transport)
}

// ============================================================
// 配置取值（session_rt 装配点共用）
// ============================================================

/// config `[clipboard].max_bytes` 与协议 16MiB 取小（FR-4.5 可配；0=禁用）。
pub fn clip_max_bytes(config: &crate::state::SharedConfig) -> u64 {
    let cfg = config
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cfg.clipboard.max_bytes.min(u64::from(
        xcontrol_proto::clip::ClipAnnounce::DEFAULT_MAX_TOTAL_LEN,
    ))
}

// ============================================================
// 单测（纯逻辑面：帧解码 / 首帧分类 / 分派器 localhost 对拍）
// ============================================================

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use xcontrol_proto::frame::kind;
    use xcontrol_proto::{ClipAck, ClipAnnounce, ClipChunk, ClipEnd, ClipFormat};

    use super::*;

    /// 测试限速参数（与 serve.rs 测试同形；毫秒级）。
    fn fast_throttle_config() -> xcontrol_net::ThrottleConfig {
        xcontrol_net::ThrottleConfig {
            max_failures: 5,
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            min_interval: Duration::from_millis(1),
        }
    }

    /// 通道帧解码：合法剪贴板帧序列全解出（保序）。
    #[test]
    fn decode_channel_frames_accepts_clip_sequence() {
        let bind = ChannelBind {
            session_id: [1; 16],
        }
        .encode_frame(0)
        .expect("bind 编码");
        let announce = ClipAnnounce {
            clip_id: 1,
            origin_id: [0; 16],
            origin_seq: 1,
            formats: vec![ClipFormat::Text],
            total_len: 5,
            blake3: [0; 32],
        }
        .encode_frame(1)
        .expect("announce 编码");
        let chunk = ClipChunk {
            clip_id: 1,
            seq: 0,
            data: b"hello".to_vec(),
        }
        .encode_frame(2)
        .expect("chunk 编码");
        let ack = ClipAck { seq: 0 }.encode_frame(3).expect("ack 编码");
        let end = ClipEnd { clip_id: 1 }.encode_frame(4).expect("end 编码");
        let mut stream = bind;
        stream.extend_from_slice(&announce);
        stream.extend_from_slice(&chunk);
        stream.extend_from_slice(&ack);
        stream.extend_from_slice(&end);
        let msgs = decode_clip_channel_frames(&stream).expect("合法序列");
        assert_eq!(msgs.len(), 5, "五帧全解出");
        assert!(matches!(msgs[0], ClipMsg::Bind(_)));
        assert!(matches!(msgs[1], ClipMsg::Announce(_)));
        assert!(matches!(msgs[2], ClipMsg::Chunk(_)));
        assert!(matches!(msgs[3], ClipMsg::Ack(_)));
        assert!(matches!(msgs[4], ClipMsg::End(_)));
    }

    /// 通道帧解码：非剪贴板帧（控制帧）= 误路由违规。
    #[test]
    fn decode_channel_frames_rejects_non_clip_frame() {
        use xcontrol_proto::frame::encode_frame;
        let ping = encode_frame(1, kind::CTRL_PING, &[0x00]).expect("ping 编码");
        let error = decode_clip_channel_frames(&ping).expect_err("控制帧必须违规");
        assert!(error.contains("非剪贴板帧"), "{error}");
    }

    /// 通道帧解码：半帧 / 垃圾字节 = 违规。
    #[test]
    fn decode_channel_frames_rejects_malformed_stream() {
        assert!(decode_clip_channel_frames(&[0x00, 0x01]).is_err(), "半帧头");
        assert!(
            decode_clip_channel_frames(b"garbage!!").is_err(),
            "垃圾字节"
        );
    }

    /// 首帧分类：ChannelBind / 其他剪贴板帧 / 非剪贴板帧 / 解析失败。
    #[test]
    fn first_frame_class_covers_kinds() {
        let bind = ChannelBind {
            session_id: [2; 16],
        }
        .encode_frame(0)
        .expect("bind 编码");
        assert!(matches!(
            first_frame_class(&bind),
            Some(FirstFrame::ChannelBind(_))
        ));
        let ack = ClipAck { seq: 1 }.encode_frame(1).expect("ack 编码");
        assert!(matches!(
            first_frame_class(&ack),
            Some(FirstFrame::ClipWithoutBind(0x53))
        ));
        let ping =
            xcontrol_proto::frame::encode_frame(1, kind::CTRL_PING, &[0x00]).expect("ping 编码");
        assert!(matches!(
            first_frame_class(&ping),
            Some(FirstFrame::NonClip(0x45))
        ));
        assert!(first_frame_class(b"junk").is_none(), "解析失败");
    }

    /// 分派器 localhost 对拍：静默连接 = 控制连接（原样交付）；ChannelBind
    /// 会话匹配 = 剪贴板通道（泵任务产出）；会话不匹配 = 拒绝断开。
    #[tokio::test]
    async fn classify_dispatches_control_clip_and_rejects_mismatch() {
        let opts = xcontrol_net::ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle_config(),
            allowlist: Vec::new(),
        };
        let psk = [0x42u8; 32];
        let mut server = serve::serve(opts, &psk).await.expect("serve 启动");
        let addr = server.local_addr();
        let control_session_id = [7u8; 16];

        // ① 静默连接（控制连接形态：客户端握手后不发帧）。
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle_config());
        let _control_conn = serve::connect(addr, &psk, &mut throttle)
            .await
            .expect("控制连接握手");
        // ② 剪贴板连接：发 ChannelBind（会话匹配）。
        let mut clip_conn = serve::connect(addr, &psk, &mut throttle)
            .await
            .expect("剪贴板连接握手");
        let bind = ChannelBind {
            session_id: control_session_id,
        };
        clip_conn
            .transport
            .send(&bind.encode_frame(0).expect("bind 编码"))
            .await
            .expect("bind 发送");
        // ③ 剪贴板连接：发 ChannelBind（会话不匹配）。
        let mut mismatch_conn = serve::connect(addr, &psk, &mut throttle)
            .await
            .expect("第三条连接握手");
        let bad_bind = ChannelBind {
            session_id: [0xEE; 16],
        };
        mismatch_conn
            .transport
            .send(&bad_bind.encode_frame(0).expect("bind 编码"))
            .await
            .expect("bind 发送");

        // FIFO：静默连接最先拨号 → 最先交付。分派为控制连接（原样交付）。
        let first = server.next_connection().await.expect("第一条连接");
        let class =
            classify_incoming(first, control_session_id, 1024, Duration::from_millis(150)).await;
        assert!(
            matches!(class, IncomingClass::Control(_)),
            "静默 = 控制连接"
        );

        // 第二条：ChannelBind 会话匹配 → 剪贴板通道（泵已 spawn，关闭收尾）。
        let second = server.next_connection().await.expect("第二条连接");
        let class =
            classify_incoming(second, control_session_id, 1024, Duration::from_millis(150)).await;
        match class {
            IncomingClass::ClipChannel(guard) => guard.close(),
            other => panic!("匹配的 ChannelBind 应产出剪贴板通道：{other:?}"),
        }

        // 第三条：会话不匹配 → 拒绝断开。
        let third = server.next_connection().await.expect("第三条连接");
        let class =
            classify_incoming(third, control_session_id, 1024, Duration::from_millis(150)).await;
        assert!(
            matches!(class, IncomingClass::Rejected),
            "会话不匹配 = 拒绝"
        );
    }

    /// 客户端通道循环：max_bytes=0（禁用）不拨号，stop 信号即退出。
    #[tokio::test]
    async fn client_loop_disabled_exits_on_stop() {
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(client_clip_channel_loop(
            "127.0.0.1:1".parse().expect("常量地址"),
            [0u8; 32],
            fast_throttle_config(),
            [0u8; 16],
            0, // 禁用
            stop_rx,
        ));
        // 未拨号（禁用面）：短等待后 stop → 任务即刻退出。
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = stop_tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("禁用循环在 stop 后 2s 内退出")
            .expect("任务无 panic");
    }
}
