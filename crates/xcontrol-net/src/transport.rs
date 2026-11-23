//! 传输抽象与内存回环实现（design.md：M1 同步实现、零 tokio）
//! + M2 真实 TCP 封装（[`TcpTransport`]，tokio 异步——两条并存，见下）。
//!
//! # 同步/异步决策落点（design.md §同步 vs 异步）
//!
//! M1 无真实 socket，async 无收益；加解密 CPU-bound；同步使基准更纯净。
//! [`Transport`] trait 保持 poll-friendly 边界：读写方法分离、内部无固定 sleep
//! ——超时一律由调用方以参数/截止时刻控制。M2 的 [`TcpTransport`] 为 async
//! 实现，**不强行实现** M1 同步 trait（design.md：避免破坏 M1 测试；整体 async
//! 统一延后到真实需要时）。
//!
//! # 消息边界契约
//!
//! snow 传输态的 `write_message` 产出**自带 AEAD 认证边界的报文**，接收端必须
//! 整报文送入 `read_message`——故收发以报文为单位。M1 回环按队列条目天然保
//! 边界；M2 真实 TCP 由 [`TcpTransport`] 在密文外叠加长度前缀分帧
//! （[`crate::framing`]）恢复边界；应用层帧 docs/02 §4.1 在密文**内**，与传输
//! 分帧正交（见 [`crate::noise`]）。
//!
//! # 关闭语义（M1 回环与 M2 TCP 对齐口径）
//!
//! - [`Transport::shutdown`] / [`TcpTransport::shutdown`]：本端优雅关闭——已排队
//!   报文（TCP：内核缓冲中的在途字节）仍可被对端收妥，排空后对端收侧得
//!   [`NetError::Closed`]；**任一方向关闭即整条链路关闭**（单侧 shutdown 影响两端：
//!   此后另一端的 `send` 亦报 Closed）。回环直接以标志位实现；TCP 侧对端 FIN
//!   被 [`TcpTransport::recv`] 观测为 EOF 后同样置整链关闭标志（TCP 半关闭不外露）；
//! - [`LoopbackControl::disconnect`]：断开注入（链路切断）——双向关闭并**丢弃
//!   在途报文**，两端 `recv`/`send` 立即报 [`NetError::Closed`]。
//!
//! # 延迟/断开注入与捕获（测试钩子）
//!
//! [`LoopbackTransport::pair`] 构建双端 + [`LoopbackControl`] 控制句柄；
//! `delay` 参数给每个报文注入固定投递延迟（模拟 RTT/抖动），
//! [`LoopbackControl::set_capture`] 开启报文捕获（供链路测试断言
//! "密文无明文特征"，docs/02 §0）。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::NetError;
use crate::framing;
use crate::noise::SecureTransport;

/// 面向报文的传输抽象（同步；超时由调用方控制，见模块文档）。
pub trait Transport {
    /// 发送一个完整报文（排队即返回，不等待对端收妥）。
    ///
    /// # Errors
    /// [`NetError::Closed`]：链路任一方向已关闭（本端/对端 shutdown 或断开注入后
    /// ——回环为链路级关闭语义，非 TCP 半关闭，见模块文档"关闭语义"）。
    fn send(&mut self, packet: &[u8]) -> Result<(), NetError>;

    /// 非阻塞接收一个完整报文：写入 `out`（覆盖内容）并返回 `Ok(true)`；
    /// 当前无（已到期）报文返回 `Ok(false)`。
    ///
    /// # Errors
    /// [`NetError::Closed`]：对端方向已关闭且排队报文已排空。
    fn try_recv(&mut self, out: &mut Vec<u8>) -> Result<bool, NetError>;

    /// 有界等待接收：至多等待 `timeout`；其余语义同 [`Transport::try_recv`]。
    ///
    /// # Errors
    /// 同 [`Transport::try_recv`]。
    fn recv_timeout(&mut self, timeout: Duration, out: &mut Vec<u8>) -> Result<bool, NetError>;

    /// 关闭本端（幂等；语义见模块文档"关闭语义"）。
    fn shutdown(&mut self);

    /// 本端任一方向是否已关闭（shutdown 或断开注入后为 true）。
    fn is_closed(&self) -> bool;
}

/// 链路侧别（双队列寻址）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// A 端。
    A,
    /// B 端。
    B,
}

/// 单方向线：报文队列（带投递时刻，支持延迟注入）+ 关闭标志。
#[derive(Debug, Default)]
struct Wire {
    queue: VecDeque<(Instant, Vec<u8>)>,
    closed: bool,
}

/// 链路状态（互斥锁保护；`Condvar` 独立于锁外——`wait_timeout` 要求"守卫按值
/// 进入 + Condvar 引用存活"，同结构体互持无法通过借用检查）。
#[derive(Debug)]
struct Link {
    /// A → B 方向。
    forward: Wire,
    /// B → A 方向。
    reverse: Wire,
    /// 每报文投递延迟注入（None = 立即投递）。
    delay: Option<Duration>,
    /// 报文捕获缓冲（None = 不记录；`set_capture` 切换）。
    capture: Option<Vec<Vec<u8>>>,
}

/// 双端 + 控制句柄三方共享的同步原语。
#[derive(Debug)]
struct Shared {
    lock: Mutex<Link>,
    /// 新报文/关闭时唤醒等待中的 recv。
    signal: Condvar,
}

impl Shared {
    /// 锁获取（锁中毒恢复：报文队列是纯内存状态，中毒后保留内容继续服务，
    /// `unwrap_or_else` 非 unwrap，不触 CI 禁令）。
    fn lock(&self) -> MutexGuard<'_, Link> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `from` 端的出方向线。
    fn outbound(link: &mut Link, from: Side) -> &mut Wire {
        match from {
            Side::A => &mut link.forward,
            Side::B => &mut link.reverse,
        }
    }

    /// `to` 端的入方向线。
    fn inbound(link: &mut Link, to: Side) -> &mut Wire {
        match to {
            Side::A => &mut link.reverse,
            Side::B => &mut link.forward,
        }
    }

    /// 任一方向已关闭即视为链路关闭（双端共享线，单侧关闭影响两端）。
    fn side_closed(link: &Link) -> bool {
        link.forward.closed || link.reverse.closed
    }

    /// 到期报文出队（队首按投递时刻有序；到期才投递）。
    fn pop_due(link: &mut Link, to: Side, now: Instant) -> Option<Vec<u8>> {
        let wire = Self::inbound(link, to);
        if matches!(wire.queue.front(), Some(&(at, _)) if at <= now) {
            wire.queue.pop_front().map(|(_, packet)| packet)
        } else {
            None
        }
    }
}

/// 回环传输单端（[`Transport`] 实现；M1 用法为每线程持一端）。
#[derive(Debug, Clone)]
pub struct LoopbackTransport {
    shared: Arc<Shared>,
    side: Side,
}

/// 回环控制句柄：断开注入与报文捕获（测试钩子）。
#[derive(Debug, Clone)]
pub struct LoopbackControl {
    shared: Arc<Shared>,
}

impl LoopbackTransport {
    /// 构建一对互通的回环端。
    ///
    /// `delay` 为每个报文的固定投递延迟（None = 立即投递）；返回
    /// `(A 端, B 端, 控制句柄)`。A→B 与 B→A 延迟相同（模拟对称 RTT）。
    #[must_use]
    pub fn pair(delay: Option<Duration>) -> (Self, Self, LoopbackControl) {
        let link = Link {
            forward: Wire::default(),
            reverse: Wire::default(),
            delay,
            capture: None,
        };
        let shared = Arc::new(Shared {
            lock: Mutex::new(link),
            signal: Condvar::new(),
        });
        let control = LoopbackControl {
            shared: shared.clone(),
        };
        (
            Self {
                shared: shared.clone(),
                side: Side::A,
            },
            Self {
                shared,
                side: Side::B,
            },
            control,
        )
    }

    /// 收发公共实现：`timeout = ZERO` 即 [`Transport::try_recv`]。
    fn recv_impl(&mut self, timeout: Duration, out: &mut Vec<u8>) -> Result<bool, NetError> {
        let deadline = Instant::now() + timeout;
        let shared = self.shared.clone();
        let mut link = shared.lock();
        loop {
            let now = Instant::now();
            if let Some(packet) = Shared::pop_due(&mut link, self.side, now) {
                out.clear();
                out.extend_from_slice(&packet);
                return Ok(true);
            }
            // 排空后方向已关闭 → 断开（优雅关闭的"发完即关"语义，见模块文档）。
            let wire = Shared::inbound(&mut link, self.side);
            if wire.closed && wire.queue.is_empty() {
                return Err(NetError::Closed);
            }
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                return Ok(false);
            }
            // 等待至 {总超时, 队首投递时刻} 的较早者（队首必晚于 now，见 pop_due）。
            let wait_until = Shared::inbound(&mut link, self.side)
                .queue
                .front()
                .map_or(remaining, |&(at, _)| {
                    at.saturating_duration_since(now).min(remaining)
                });
            let (guard, _) = shared
                .signal
                .wait_timeout(link, wait_until)
                .unwrap_or_else(PoisonError::into_inner);
            link = guard;
        }
    }
}

impl Transport for LoopbackTransport {
    fn send(&mut self, packet: &[u8]) -> Result<(), NetError> {
        let shared = self.shared.clone();
        let mut link = shared.lock();
        if Shared::side_closed(&link) {
            return Err(NetError::Closed);
        }
        let deliver_at = match link.delay {
            Some(delay) => Instant::now() + delay,
            None => Instant::now(),
        };
        if let Some(capture) = link.capture.as_mut() {
            capture.push(packet.to_vec());
        }
        Shared::outbound(&mut link, self.side)
            .queue
            .push_back((deliver_at, packet.to_vec()));
        shared.signal.notify_all();
        Ok(())
    }

    fn try_recv(&mut self, out: &mut Vec<u8>) -> Result<bool, NetError> {
        self.recv_impl(Duration::ZERO, out)
    }

    fn recv_timeout(&mut self, timeout: Duration, out: &mut Vec<u8>) -> Result<bool, NetError> {
        self.recv_impl(timeout, out)
    }

    fn shutdown(&mut self) {
        let shared = self.shared.clone();
        let mut link = shared.lock();
        Shared::outbound(&mut link, self.side).closed = true;
        shared.signal.notify_all();
    }

    fn is_closed(&self) -> bool {
        let link = self.shared.lock();
        Shared::side_closed(&link)
    }
}

impl LoopbackControl {
    /// 断开注入：双向关闭并丢弃在途报文（模拟链路切断，非优雅 FIN）。
    /// 两端此后 `send`/`recv` 报 [`NetError::Closed`]、`is_closed()` 为 true。
    pub fn disconnect(&self) {
        let shared = self.shared.clone();
        {
            let mut link = shared.lock();
            link.forward.closed = true;
            link.forward.queue.clear();
            link.reverse.closed = true;
            link.reverse.queue.clear();
        }
        // 守卫先释放再 notify：状态变更受锁保护，等待方持守卫检查后原子挂起，
        // 此处 notify 不存在丢唤醒窗口（见 recv_impl 的等待模式）。
        shared.signal.notify_all();
    }

    /// 开关报文捕获。开启后每个 `send` 的报文副本按序记录（断言
    /// "密文无明文特征"用；默认关闭，基准场景勿开以免无界增长）。
    pub fn set_capture(&self, enabled: bool) {
        let shared = self.shared.clone();
        let mut link = shared.lock();
        if enabled {
            link.capture.get_or_insert_with(Vec::new);
        } else {
            link.capture = None;
        }
    }

    /// 取走全部已捕获报文（清空缓冲）。
    #[must_use]
    pub fn take_captured(&self) -> Vec<Vec<u8>> {
        let shared = self.shared.clone();
        let mut link = shared.lock();
        link.capture.take().unwrap_or_default()
    }
}

/// `TcpTransport::fill` 的 EOF 语义（对齐原 `framing::read_frame` /
/// `read_frame_resync` 口径）：
/// - [`EofPolicy::StartClosedMidIo`]：正常收帧——帧头起点 EOF = 对端优雅关闭
///   （Closed）；帧中途 EOF = 截断异常（IoFailed）。
/// - [`EofPolicy::AlwaysClosed`]：重同步扫描中——任一位置 EOF 均按对端优雅关闭
///   （Closed，原 `read_frame_resync` 口径）。
#[derive(Clone, Copy)]
enum EofPolicy {
    StartClosedMidIo,
    AlwaysClosed,
}

fn io_failed(_: std::io::Error) -> NetError {
    NetError::IoFailed
}

/// 真实 TCP 上的加密会话单端（11-17-m2-tcp，design.md：M2 async 实现，不并入
/// M1 同步 [`Transport`] trait）。
///
/// 语义锚点：docs/02 §0（`TCP_NODELAY` 强制）、§3.2（传输态）、§1（双连接）。
///
/// 收发单位 = 一条 Noise 报文（明文进、明文出）：`send` 先加密再经
/// [`crate::framing`] 长度前缀分帧写出；`recv` 读一个完整帧后解密。关闭语义
/// 与 M1 回环对齐（见模块文档"关闭语义"：单侧关闭 = 整链关闭，TCP 半关闭不外露）。
///
/// 超时不在此处计时：调用方以 `tokio::time::timeout` 包裹（与握手窗口同型，
/// docs/02 §4.5 的判死/心跳属 M3 会话层）。
#[derive(Debug)]
pub struct TcpTransport {
    stream: TcpStream,
    secure: SecureTransport,
    peer: SocketAddr,
    /// 链路级关闭标志：本端 shutdown、对端 EOF、或任一收发硬错误后置位；
    /// 置位后所有收发立即报 [`NetError::Closed`]（对齐 M1"任一方向关闭即整链关闭"）。
    closed: bool,
    /// 跨 `recv` 调用的读缓冲（取消安全核心，11-21 两机实测大图断连定案）：
    /// 泵循环以 `tokio::select!` 轮转收发，`recv` future 随时可能被取消（200ms
    /// 出站拍 tick 撞上 64KB 剪贴板块的分段读窗即触发）——tokio `read_exact`
    /// 取消时已读字节丢失 = 流错位 → `FrameInvalid` → resync 在大块随机数据
    /// （PNG 密文）里锁到假 magic → `DecryptFailed` 断链。修复：socket 读一律
    /// 走**取消安全**的单次 `read`（挂起中无字节交付，取消不丢），进度全程留
    /// 在本缓冲——`recv` future 任意取消/重入不丢字节（回归测试
    /// `tcp_recv_cancelled_mid_frame_keeps_bytes`）。
    read_buf: Vec<u8>,
}

impl TcpTransport {
    /// 由已握手的 TCP 流 + 会话态组装单端。
    ///
    /// `TCP_NODELAY` 在此强制（docs/02 §0：输入热路径禁 Nagle 合帧）；设置失败
    /// 视为流不可用（`set_nodelay` 仅在流已死时失败）→ 立即置关闭标志，首次
    /// 收发报 [`NetError::Closed`]。[`crate::serve`] 的握手路径在握手前已设过
    /// 一次（握手报文同样不该被 Nagle 拖延），此处冗余设置幂等且兜底直构场景。
    #[must_use]
    pub fn new(stream: TcpStream, secure: SecureTransport, peer: SocketAddr) -> Self {
        // NODELAY 失败 = 流异常 → 预先标记整链关闭（构造函数不返回 Result，
        // 保持与握手成功路径的组装简单；错误在首次收发以 Closed 体现）。
        if stream.set_nodelay(true).is_err() {
            return Self {
                stream,
                secure,
                peer,
                closed: true,
                read_buf: Vec::new(),
            };
        }
        Self {
            stream,
            secure,
            peer,
            closed: false,
            read_buf: Vec::new(),
        }
    }

    /// 对端地址（诊断/日志标注用）。
    #[must_use]
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// 发送一个完整报文：加密 → 长度前缀分帧 → 写出（`write_all`）。
    ///
    /// # Errors
    /// - [`NetError::Closed`]：本端已 shutdown 或链路已被任一错误关闭；
    /// - [`NetError::EncryptFailed`]：明文超单条 Noise 报文上限；
    /// - [`NetError::IoFailed`]：socket 写失败（此后整链关闭）。
    pub async fn send(&mut self, packet: &[u8]) -> Result<(), NetError> {
        if self.closed {
            return Err(NetError::Closed);
        }
        // 发送端长度守卫（11-20 大图断连排查）：明文超 Noise 单报文上限时
        // **拒绝且不触碰流**——防止任何"长度头已写/载荷未写"的半写状态造成
        // 流错位（错位 = 后续全部 FrameInvalid）。本地错误不断链（既有测试
        // 契约：链路未受损，后续 send 合法）；调用方（clip 分块）保证单帧
        // ≤ 65519，此处是最后防线。
        if packet.len() > crate::noise::MAX_NOISE_PLAINTEXT {
            return Err(NetError::EncryptFailed);
        }
        let mut ciphertext = Vec::with_capacity(packet.len() + 16);
        self.secure.encrypt_into(packet, &mut ciphertext)?;
        framing::write_frame(&mut self.stream, &ciphertext)
            .await
            .inspect_err(|_| {
                // 写失败 = 链路不可用：对齐 M1 断开语义（docs/02 §7 违规/故障即断开）。
                self.closed = true;
            })
    }

    /// 阻塞接收一个完整报文：帧 → 解密，明文写入 `out`（覆盖内容）。
    ///
    /// # Errors
    /// - [`NetError::Closed`]：本端已 shutdown，或对端优雅关闭（帧头起点 EOF
    ///   ——与 M1 回环"对端 shutdown → recv 得 Closed"对齐）；
    /// - [`NetError::FrameInvalid`] / [`NetError::DecryptFailed`]：协议违规
    ///   （docs/02 §7：一律断开不重试——此后整链关闭）；
    /// - [`NetError::IoFailed`]：帧中途截断或 socket 读错误（此后整链关闭）。
    pub async fn recv(&mut self, out: &mut Vec<u8>) -> Result<(), NetError> {
        if self.closed {
            return Err(NetError::Closed);
        }
        // 取消安全（11-21 两机实测大图断连定案）：泵循环以 `tokio::select!` 轮转
        // 收发，本 future 随时可能被取消（出站拍 tick 撞上 64KB 剪贴板块的分段
        // 读窗）——因此**一切 socket 读必须走取消安全的单次 `read`，进度全程留
        // 在 `read_buf`**（挂起中无字节交付，取消不丢；`read_exact` 取消即丢，
        // 旧实现的错位源头）。帧解析状态机：缓冲够头即校验（错位尽早进重同步），
        // 载荷齐则整帧取出解密交付；EOF：缓冲空 = 帧头起点优雅关闭 → Closed，
        // 缓冲非空 = 帧中途截断 → IoFailed。
        loop {
            if self.read_buf.len() >= framing::HEADER_LEN {
                let mut header = [0u8; framing::HEADER_LEN];
                header.copy_from_slice(&self.read_buf[..framing::HEADER_LEN]);
                match framing::decode_header(&header) {
                    Ok(len) => {
                        let total = framing::HEADER_LEN + len;
                        if self.read_buf.len() >= total {
                            let frame: Vec<u8> = self.read_buf.drain(..total).collect();
                            out.clear();
                            return self
                                .secure
                                .decrypt_into(&frame[framing::HEADER_LEN..], out)
                                .inspect_err(|error| {
                                    // 解密失败 = 报文错位/密钥失配，流上位置已不可信：
                                    // 断开（docs/02 §7）。
                                    self.closed = true;
                                    tracing::warn!(target: "net::transport", %error, len,
                                        "传输态解密失败（断链，docs/02 §7）");
                                });
                        }
                    }
                    // 帧头非法 = 流错位（半写残帧/网络抖动，不是对端恶意）：丢弃
                    // 字节重同步而非断链——11-21 两机实测"控制随大图断连不可接受"
                    // 的裁定维持。DecryptFailed 保持断开（Noise nonce 失配 = 真错
                    // 位，无解——但根因（取消丢字节）修复后不应再抵达此处）。
                    Err(NetError::FrameInvalid) => return self.recv_resync(out).await,
                    Err(error) => {
                        self.closed = true;
                        return Err(error);
                    }
                }
            }
            self.fill(EofPolicy::StartClosedMidIo).await?;
        }
    }

    /// 帧头非法后的重同步（`framing::read_frame_resync` 的缓冲化版本）：从错位
    /// 点起逐字节滑窗（丢弃至多 [`framing::RESYNC_SCAN_LIMIT`] 字节）找下一个
    /// 合法帧头，读出该帧解密交付。扫描全程在 `read_buf` 内进行（不丢已读字
    /// 节）；锁定候选 warn 观测——DecryptFailed（假 magic 被当真头）的关键诊
    /// 断锚点。EOF 按 Closed（对齐原 `read_frame_resync`：对端优雅关闭）。
    async fn recv_resync(&mut self, out: &mut Vec<u8>) -> Result<(), NetError> {
        // scan_from = 已扫过（均不合法）的窗口起点；滑窗扫描在缓冲内进行。
        let mut scan_from = 0usize;
        loop {
            // 扫描窗口末端：最后一个完整 4B 窗口之后（不足一个头时为 0）。
            let last_window = self
                .read_buf
                .len()
                .checked_sub(framing::HEADER_LEN)
                .map_or(0, |end| end + 1);
            let scan_end = last_window.min(framing::RESYNC_SCAN_LIMIT + 1);
            let mut found = None;
            let mut pos = scan_from;
            while pos < scan_end {
                let mut header = [0u8; framing::HEADER_LEN];
                header.copy_from_slice(&self.read_buf[pos..pos + framing::HEADER_LEN]);
                if let Ok(len) = framing::decode_header(&header) {
                    found = Some((pos, len));
                    break;
                }
                pos += 1;
            }
            match found {
                // 候选锁定且载荷齐：丢弃错位前缀，整帧解密交付。
                Some((pos, len)) if self.read_buf.len() >= pos + framing::HEADER_LEN + len => {
                    let total = pos + framing::HEADER_LEN + len;
                    tracing::warn!(target: "net::transport", dropped = pos, len,
                        "传输流错位重同步：扫描锁定候选帧头（若此后 DecryptFailed 即假 magic 误锁）");
                    let frame: Vec<u8> = self.read_buf.drain(..total).collect();
                    out.clear();
                    return self
                        .secure
                        .decrypt_into(&frame[pos + framing::HEADER_LEN..], out)
                        .inspect_err(|_| {
                            self.closed = true;
                        });
                }
                // 候选锁定但载荷不足：从候选位置继续读。
                Some((pos, _)) => {
                    scan_from = pos;
                    self.fill(EofPolicy::AlwaysClosed).await?;
                }
                // 窗口扫尽无候选：超限 = 真链路故障；未超限 = 继续读。
                None => {
                    if scan_end > framing::RESYNC_SCAN_LIMIT {
                        self.closed = true;
                        return Err(NetError::FrameInvalid);
                    }
                    scan_from = scan_end;
                    self.fill(EofPolicy::AlwaysClosed).await?;
                }
            }
        }
    }

    /// 取消安全读入（单次 `read`，进度入 `read_buf`；挂起中无字节交付，取消不丢）。
    async fn fill(&mut self, policy: EofPolicy) -> Result<(), NetError> {
        let mut chunk = [0u8; 16 * 1024];
        let n = self.stream.read(&mut chunk).await.map_err(io_failed)?;
        if n == 0 {
            self.closed = true;
            return Err(match policy {
                EofPolicy::AlwaysClosed => NetError::Closed,
                EofPolicy::StartClosedMidIo => {
                    if self.read_buf.is_empty() {
                        NetError::Closed
                    } else {
                        NetError::IoFailed
                    }
                }
            });
        }
        self.read_buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// 本端优雅关闭（幂等）：置整链关闭 + 尽力发出 FIN（`AsyncWriteExt::shutdown`
    /// 会先冲刷内核缓冲——已排队数据仍可被对端收妥，排空后对端 `recv` 得
    /// [`NetError::Closed`]，对齐 M1 回环语义）。FIN 写失败不掩盖关闭意图
    /// （随后 drop 流强制断开），故不返回错误。
    pub async fn shutdown(&mut self) {
        self.closed = true;
        let _ = self.stream.shutdown().await;
    }

    /// 链路是否已关闭（shutdown / 对端 EOF / 任一收发错误后为 true）。
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// 会话标识（握手摘要派生，docs/02 §3.3；双方一致）。
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        self.secure.session_id()
    }

    /// 握手摘要（core `Session` 的 `HandshakeReady` 事件载荷）。
    #[must_use]
    pub const fn handshake_hash(&self) -> [u8; 32] {
        self.secure.handshake_hash()
    }

    /// 对端地址（serve 侧限速计数键，docs/02 §3.2 per-来源 IP）。
    #[must_use]
    pub const fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// 单条 Noise 报文明文上限（透传 [`SecureTransport::max_plaintext`]）。
    #[must_use]
    pub const fn max_plaintext() -> usize {
        SecureTransport::max_plaintext()
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::framing::TRANSPORT_MAGIC;

    /// 基础往返：立即投递、FIFO 保序、覆盖式写入 out。
    #[test]
    fn loopback_roundtrip_and_ordering() {
        let (mut a, mut b, _control) = LoopbackTransport::pair(None);
        assert!(!a.is_closed() && !b.is_closed());

        a.send(b"one").unwrap();
        a.send(b"two").unwrap();
        a.send(b"three").unwrap();

        let mut buf = Vec::new();
        buf.extend_from_slice(b"stale-bytes");
        assert!(b.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"one", "recv 覆盖 out 内容");
        assert!(b.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"two");
        assert!(b.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"three");
        assert!(!b.try_recv(&mut buf).unwrap(), "空队列非阻塞返回 false");
    }

    /// 反向独立：B 端可同时向 A 端发送。
    #[test]
    fn loopback_bidirectional() {
        let (mut a, mut b, _control) = LoopbackTransport::pair(None);
        a.send(b"ping").unwrap();
        b.send(b"pong").unwrap();
        let mut buf = Vec::new();
        assert!(b.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"ping");
        assert!(a.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"pong");
    }

    /// 延迟注入：投递前 try_recv 不可见，到期后可见。
    #[test]
    fn loopback_delay_injection() {
        let (mut a, mut b, _control) = LoopbackTransport::pair(Some(Duration::from_millis(50)));
        a.send(b"delayed").unwrap();
        let mut buf = Vec::new();
        assert!(!b.try_recv(&mut buf).unwrap(), "未到期不得投递");
        // 有界等待：到期报文在 timeout 内送达
        let started = Instant::now();
        assert!(b.recv_timeout(Duration::from_secs(2), &mut buf).unwrap());
        assert_eq!(buf, b"delayed");
        assert!(
            started.elapsed() >= Duration::from_millis(40),
            "recv_timeout 应等到投递时刻（注入 50ms）"
        );
    }

    /// recv_timeout 超时返回 false，不误报 Closed。
    #[test]
    fn loopback_recv_timeout_expires() {
        let (mut a, mut b, _control) = LoopbackTransport::pair(None);
        let mut buf = Vec::new();
        let started = Instant::now();
        assert!(!b.recv_timeout(Duration::from_millis(20), &mut buf).unwrap());
        assert!(
            started.elapsed() >= Duration::from_millis(15),
            "应真实等待超时时长"
        );
        // 期间对端仍可正常发送
        a.send(b"later").unwrap();
        assert!(b.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"later");
    }

    /// shutdown（优雅关闭）：在途报文仍可收妥，排空后 Closed；本端 send 报错。
    #[test]
    fn loopback_shutdown_graceful() {
        let (mut a, mut b, _control) = LoopbackTransport::pair(None);
        a.send(b"last").unwrap();
        a.shutdown();
        assert!(a.is_closed());
        let mut buf = Vec::new();
        assert!(
            b.try_recv(&mut buf).unwrap(),
            "在途报文仍可收（FIN 前已发）"
        );
        assert_eq!(buf, b"last");
        assert!(
            matches!(
                b.recv_timeout(Duration::from_millis(20), &mut buf),
                Err(NetError::Closed)
            ),
            "排空后对端 recv 应得 Closed"
        );
        assert!(
            matches!(a.send(b"nope"), Err(NetError::Closed)),
            "关闭后 send 报错"
        );
        a.shutdown(); // 幂等
        assert!(a.is_closed());
    }

    /// 断开注入：在途报文丢弃、两端立即 Closed。
    #[test]
    fn loopback_disconnect_drops_in_flight() {
        let (mut a, mut b, control) = LoopbackTransport::pair(None);
        a.send(b"lost").unwrap();
        control.disconnect();
        assert!(a.is_closed() && b.is_closed());
        let mut buf = Vec::new();
        assert!(
            matches!(b.try_recv(&mut buf), Err(NetError::Closed)),
            "断开注入应丢弃在途报文并立即 Closed"
        );
        assert!(matches!(a.send(b"x"), Err(NetError::Closed)));
        assert!(matches!(b.send(b"x"), Err(NetError::Closed)));
    }

    /// shutdown 唤醒对端阻塞中的 recv_timeout（对端得 Closed 而非等满超时）。
    #[test]
    fn loopback_shutdown_wakes_peer() {
        let (mut a, mut b, _control) = LoopbackTransport::pair(None);
        let handle = thread::spawn(move || {
            let mut buf = Vec::new();
            b.recv_timeout(Duration::from_secs(5), &mut buf)
        });
        thread::sleep(Duration::from_millis(30));
        a.shutdown();
        let started = Instant::now();
        assert!(matches!(handle.join().unwrap(), Err(NetError::Closed)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown 应即时唤醒对端"
        );
    }

    /// 延迟注入下优雅关闭的边界：已到期的在途报文仍可收，未到期的随关闭排空后 Closed。
    #[test]
    fn loopback_shutdown_with_delayed_queue() {
        let (mut a, mut b, _control) = LoopbackTransport::pair(Some(Duration::from_millis(40)));
        a.send(b"first").unwrap();
        a.send(b"second").unwrap();
        a.shutdown();
        thread::sleep(Duration::from_millis(80));
        let mut buf = Vec::new();
        assert!(b.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"first");
        assert!(b.try_recv(&mut buf).unwrap());
        assert_eq!(buf, b"second");
        assert!(matches!(b.try_recv(&mut buf), Err(NetError::Closed)));
    }

    /// 报文捕获：按 send 顺序记录；take_captured 取走并清空；关闭后不记录。
    #[test]
    fn loopback_capture_hook() {
        let (mut a, mut b, control) = LoopbackTransport::pair(None);
        a.send(b"before-capture").unwrap();
        control.set_capture(true);
        a.send(b"c1").unwrap();
        b.send(b"c2").unwrap();
        let captured = control.take_captured();
        assert_eq!(
            captured,
            vec![b"c1".to_vec(), b"c2".to_vec()],
            "按序记录双方报文"
        );
        // take 后清空
        assert!(control.take_captured().is_empty());
        control.set_capture(false);
        a.send(b"after").unwrap();
        assert!(control.take_captured().is_empty(), "关闭捕获后不记录");
        let _ = b; // b 仅证明双端共存
    }

    // ===== M2：TcpTransport（真实 TCP 回环 127.0.0.1:0）=====
    //
    // 握手用 crate::noise::NoiseHandshake::run_async（真实 socket 路径）；
    // serve/connect 全链路（含限速/错码）见 tests/tcp.rs。
    // 本模块用例均处于 #[cfg(test)] 作用域（clippy expect/unwrap 豁免覆盖），
    // 与 tests/chain.rs 的 must 模式不同——helpers 亦豁免。

    /// 测试共用假码派生（m=19MiB argon2，进程内仅派生一次）。
    fn tcp_test_psk() -> [u8; 32] {
        static PSK: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        *PSK.get_or_init(|| {
            crate::psk::derive_psk("tcp-transport-test", &[0u8; 16])
                .expect("测试假码派生（固定输入）")
        })
    }

    /// 建立一对已握手、NODELAY 已设的 [`TcpTransport`]（真实 TCP 双任务）。
    async fn tcp_pair() -> (TcpTransport, TcpTransport) {
        let (raw, secure, wrapped) = tcp_pair_raw_initiator().await;
        let peer = raw.peer_addr().expect("对端地址");
        (TcpTransport::new(raw, secure, peer), wrapped)
    }

    /// 非对称对：发起端保留**原始 socket + 传输态**（测试手工构造/篡改帧），
    /// 响应端为 [`TcpTransport`]。
    async fn tcp_pair_raw_initiator() -> (tokio::net::TcpStream, SecureTransport, TcpTransport) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定回环随机端口");
        let addr = listener.local_addr().expect("监听地址");
        let psk = tcp_test_psk();
        let responder = tokio::spawn(async move {
            let (mut stream, peer) = listener.accept().await.expect("接受连接");
            stream.set_nodelay(true).expect("NODELAY");
            let secure = crate::noise::NoiseHandshake::run_async(
                &mut stream,
                crate::noise::HandshakeRole::Responder,
                &psk,
            )
            .await
            .expect("responder 握手");
            TcpTransport::new(stream, secure, peer)
        });
        let stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("连接监听端");
        let mut stream = stream;
        stream.set_nodelay(true).expect("NODELAY");
        let secure = crate::noise::NoiseHandshake::run_async(
            &mut stream,
            crate::noise::HandshakeRole::Initiator,
            &tcp_test_psk(),
        )
        .await
        .expect("initiator 握手");
        let responder_side = responder.await.expect("responder 任务");
        (stream, secure, responder_side)
    }

    /// TCP 单帧往返：加密 → 分帧 → 解密还原；session_id 双端一致。
    #[tokio::test]
    async fn tcp_roundtrip_and_session_id() {
        let (mut a, mut b) = tcp_pair().await;
        assert_eq!(a.session_id(), b.session_id(), "握手摘要派生双方一致");
        assert!(!a.is_closed() && !b.is_closed());

        a.send(b"frame-one").await.unwrap();
        a.send(b"frame-two").await.unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(b"dirty"); // 覆盖式语义锚点
        b.recv(&mut out).await.unwrap();
        assert_eq!(out, b"frame-one");
        b.recv(&mut out).await.unwrap();
        assert_eq!(out, b"frame-two");

        // 反向同理
        b.send(b"reply").await.unwrap();
        a.recv(&mut out).await.unwrap();
        assert_eq!(out, b"reply");
    }

    /// shutdown 语义对齐 M1 回环：本端 shutdown 后在途数据仍可达对端，
    /// 对端排空后 recv 得 Closed；两端 send 均报 Closed（单侧关闭 = 整链关闭）。
    #[tokio::test]
    async fn tcp_shutdown_graceful_link_level() {
        let (mut a, mut b) = tcp_pair().await;
        a.send(b"last").await.unwrap();
        a.shutdown().await;
        assert!(a.is_closed());
        let mut out = Vec::new();
        b.recv(&mut out).await.unwrap();
        assert_eq!(out, b"last", "在途数据仍可收妥（内核缓冲排空）");
        assert!(
            matches!(b.recv(&mut out).await, Err(NetError::Closed)),
            "对端排空后 recv 得 Closed（FIN → EOF）"
        );
        assert!(b.is_closed(), "对端 EOF 置整链关闭");
        assert!(
            matches!(b.send(b"nope").await, Err(NetError::Closed)),
            "对端关闭后其 send 亦报 Closed（链路级语义，非 TCP 半关闭外露）"
        );
        assert!(
            matches!(a.send(b"nope").await, Err(NetError::Closed)),
            "本端 shutdown 后 send 报 Closed"
        );
        a.shutdown().await; // 幂等
        assert!(a.is_closed());
    }

    /// 明文超限 → EncryptFailed（本地错误，链路不受影响）。
    #[tokio::test]
    async fn tcp_send_rejects_oversized_plaintext() {
        let (mut a, mut b) = tcp_pair().await;
        let oversized = vec![0u8; TcpTransport::max_plaintext() + 1];
        assert_eq!(
            a.send(&oversized).await,
            Err(NetError::EncryptFailed),
            "超出 MAX_NOISE_PLAINTEXT 在加密层拒绝"
        );
        assert!(!a.is_closed(), "本地加密错误不断链");
        a.send(b"still-alive").await.unwrap();
        let mut out = Vec::new();
        b.recv(&mut out).await.unwrap();
        assert_eq!(out, b"still-alive");
    }

    /// 篡改密文帧 → 对端 DecryptFailed 且整链关闭（违规即断开，docs/02 §7）。
    /// 发起端持原始 socket + 传输态：加密真帧后翻转 1 字节，按帧格式手工写出。
    #[tokio::test]
    async fn tcp_tampered_frame_disconnects() {
        let (mut raw, mut secure, mut b) = tcp_pair_raw_initiator().await;
        let mut ciphertext = Vec::new();
        secure.encrypt_into(b"authentic", &mut ciphertext).unwrap();
        ciphertext[0] ^= 0xFF; // 篡改（AEAD 认证必失败）
        let mut frame = Vec::new();
        crate::framing::encode_into(&ciphertext, &mut frame).unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut raw, &frame)
            .await
            .unwrap();

        let mut out = Vec::new();
        assert_eq!(
            b.recv(&mut out).await,
            Err(NetError::DecryptFailed),
            "篡改帧在 AEAD 层拒绝（报文错位/密钥失配同一变体）"
        );
        assert!(b.is_closed(), "违规即断开：解密失败后整链关闭");
        assert!(
            matches!(b.recv(&mut out).await, Err(NetError::Closed)),
            "关闭后再 recv 报 Closed"
        );
    }

    /// 非法帧头（len=0）→ FrameInvalid 且整链关闭（帧层违规语义，
    /// docs/02 §7；握手期该错误折叠 HandshakeFailed，传输期原样上抛）。
    #[tokio::test]
    async fn tcp_zero_length_frame_disconnects() {
        let (mut raw, _secure, mut b) = tcp_pair_raw_initiator().await;
        // 手工写出 magic 对但 len=0 的帧头（11-21 magic 格式：合法 Noise 报文
        // 最小 16B tag，0 长度必违规）。旧格式手工写法在新 decode_header 下
        // magic 不匹配 → FrameInvalid 同样断开，但本测试锚定的是 len=0 路径。
        tokio::io::AsyncWriteExt::write_all(
            &mut raw,
            &[0xC1, 0xA1, 0x00, 0x00], // magic 对 + len=0（错误帧头）
        )
        .await
        .unwrap();
        // 错误帧头之后补一条合法帧——重同步扫描要有后继数据可扫
        //（resync 语义 = 吸收错位字节并对齐到下一帧，对端静默则超时属预期，
        // 生产中对端必然继续发后续帧/心跳）。
        let mut frame = Vec::new();
        frame.extend_from_slice(&TRANSPORT_MAGIC.to_be_bytes());
        frame.extend_from_slice(&16_u16.to_be_bytes());
        frame.extend_from_slice(&[0u8; 16]); // 16B 假 Noise 报文（解密必失败）
        tokio::io::AsyncWriteExt::write_all(&mut raw, &frame)
            .await
            .unwrap();
        let mut out = Vec::new();
        // 重同步语义：错误帧吸收 → 扫描对齐 → 假报文解密失败 → DecryptFailed
        //（nonce 失配真错位面）——Err 必达，链路处置为断开。
        assert!(matches!(
            b.recv(&mut out).await,
            Err(NetError::FrameInvalid) | Err(NetError::DecryptFailed)
        ));
        assert!(b.is_closed(), "帧违规即断开");
    }

    /// magic 不匹配（流错位形态）→ FrameInvalid 且断开（11-21 magic 格式回归）。
    #[tokio::test]
    async fn tcp_bad_magic_frame_disconnects() {
        let (mut raw, _secure, mut b) = tcp_pair_raw_initiator().await;
        tokio::io::AsyncWriteExt::write_all(&mut raw, &[0x00, 0x00, 0x00, 0x64])
            .await
            .unwrap();
        // 同①：错误头后补合法帧供扫描对齐（16B 假报文 → DecryptFailed）。
        let mut frame = Vec::new();
        frame.extend_from_slice(&TRANSPORT_MAGIC.to_be_bytes());
        frame.extend_from_slice(&16_u16.to_be_bytes());
        frame.extend_from_slice(&[0u8; 16]);
        tokio::io::AsyncWriteExt::write_all(&mut raw, &frame)
            .await
            .unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            b.recv(&mut out).await,
            Err(NetError::FrameInvalid) | Err(NetError::DecryptFailed)
        ));
        assert!(b.is_closed(), "帧违规即断开");
    }

    /// 取消安全回归（11-21 两机大图断连定案）：`recv` future 在半帧读入后被
    /// 取消（select! 出站拍 tick 撞上 64KB 块分段读窗的真实形态），已读字节
    /// 必须留在 `read_buf` 跨调用存活——重入 `recv` 交付完整帧明文，流不错位。
    /// 旧实现（`read_exact` 直读流）在此形态丢已读字节 → 流错位 → DecryptFailed。
    #[tokio::test]
    async fn tcp_recv_cancelled_mid_frame_keeps_bytes() {
        let (mut raw, mut secure, mut b) = tcp_pair_raw_initiator().await;
        let payload = vec![0xAB_u8; 32 * 1024];
        let mut ciphertext = Vec::new();
        secure
            .encrypt_into(&payload, &mut ciphertext)
            .expect("加密");
        let mut frame = Vec::new();
        framing::encode_into(&ciphertext, &mut frame).expect("分帧");
        // 发送端：先写 1000B 半帧，50ms 后补全——给接收端留出"半帧挂起被取消"窗口。
        let frame_head = frame[..1000].to_vec();
        let frame_tail = frame[1000..].to_vec();
        tokio::spawn(async move {
            tokio::io::AsyncWriteExt::write_all(&mut raw, &frame_head)
                .await
                .expect("半帧写出");
            tokio::time::sleep(Duration::from_millis(50)).await;
            tokio::io::AsyncWriteExt::write_all(&mut raw, &frame_tail)
                .await
                .expect("补帧写出");
        });
        // 等半帧入内核缓冲（10ms）再起 recv：确保首次 poll 读入半帧后挂起、
        // 20ms 到点时被 select 取消（50ms 补帧前）。
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut out = Vec::new();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            result = b.recv(&mut out) => panic!("半帧不应完成 recv：{result:?}"),
        }
        // 取消后重入：完整帧明文交付（取消不丢字节 = 流不错位）。
        b.recv(&mut out).await.expect("取消后重入 recv 完整交付");
        assert_eq!(out, payload, "取消不得丢已读字节");
        assert!(!b.is_closed());
    }

    /// 缓冲化重同步回归：非法帧头 + 后继合法帧 → 缓冲内滑窗锁定 → 真帧明文
    /// 交付（与 tcp_bad_magic 的 DecryptFailed 形态互补——此处后继是真报文）。
    #[tokio::test]
    async fn tcp_recv_resync_after_invalid_head_delivers() {
        let (mut raw, mut secure, mut b) = tcp_pair_raw_initiator().await;
        let payload = b"after-resync-delivery";
        let mut ciphertext = Vec::new();
        secure.encrypt_into(payload, &mut ciphertext).expect("加密");
        let mut stream_bytes = Vec::new();
        stream_bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x64]); // 非法头（magic 错）
        framing::encode_into(&ciphertext, &mut stream_bytes).expect("真帧编码");
        tokio::spawn(async move {
            tokio::io::AsyncWriteExt::write_all(&mut raw, &stream_bytes)
                .await
                .expect("写出");
        });
        let mut out = Vec::new();
        b.recv(&mut out).await.expect("重同步后交付真帧");
        assert_eq!(out, payload);
        assert!(!b.is_closed());
    }
}
