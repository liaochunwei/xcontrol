//! Noise 握手与传输态封装（docs/02 §3.2 逐行契约）。
//!
//! ```text
//! 模式    Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s   （snow Builder::psk(0, &psk)）
//! prologue b"xcontrol/1"                            （协议名+大版本哈进握手，防降级/跨协议反射）
//! 角色    从机 = initiator，主控 = responder        （docs/02 §3.2）
//! 超时    5s；失败即断开                            （同步：HandshakeDeadline；async：调用方 tokio::time::timeout）
//! 传输态  snow TransportState（内建单调 nonce/rekey/重放防护；应用层不自管 nonce）
//! ```
//!
//! 两个入口共享同一套状态构建（`build_handshake_state`）与失败折叠纪律：
//! - [`NoiseHandshake::run`]：同步，作用于 M1 [`crate::transport::Transport`]
//!   （回环测试/基准路径）；
//! - [`NoiseHandshake::run_async`]：async，作用于任意 `AsyncRead + AsyncWrite`
//!   （M2 真实 TCP 路径，[`crate::framing`] 分帧；5s 超时由调用方以
//!   `tokio::time::timeout` 包裹——design.md 决策）。
//!
//! # 会话标识（docs/02 §3.3）
//!
//! 握手完成后双方各自取 `hs_hash = get_handshake_hash()`（转入传输态前），
//! `session_id = BLAKE3(hs_hash)[0..16]`。本 crate 依赖红线（docs/08 §1：net →
//! 仅 proto + config）不允许调用 core 的 `derive_session_id`，故此处本地等价
//! 实现；`tests/chain.rs` 用 dev-dep core 断言两实现逐字节一致。
//!
//! # 与应用帧的关系（design.md §关键契约）
//!
//! snow 传输态报文自带 AEAD 认证边界，无需再叠加密外帧层；应用层帧
//! （docs/02 §4.1：seq/kind/len + payload）整体作为**明文**进入单条 Noise 报文。
//! 明文上限见 [`SecureTransport::max_plaintext`]（超出 = [`NetError::EncryptFailed`]，
//! M1 常规消息远小于该上限；剪贴板大块按 docs/02 §6 分块帧天然受控）。

use std::time::{Duration, Instant};

use snow::{Builder, HandshakeState, TransportState};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::NetError;
use crate::framing;
use crate::transport::Transport;

/// Noise 模式串（docs/02 §3.2；ADR-0002 决策 2：rustls/QUIC 均无外部 PSK，故选 snow）。
pub const NOISE_PATTERN: &str = "Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s";

/// 握手 prologue（docs/02 §3.2：协议名+大版本，防降级与跨协议反射）。
pub const PROLOGUE: &[u8] = b"xcontrol/1";

/// 握手超时（docs/02 §3.2：5s）。以 [`HandshakeDeadline::after`] 传入 [`NoiseHandshake::run`]。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 握手报文缓冲：XXpsk0 三报文（均空载荷）最大 96B —— msg1 = e(32B) + 空 payload
/// tag(16B)；msg2 = e(32B) + 加密静态钥(32B+16B) + 空 payload tag(16B)；msg3 =
/// 加密静态钥(48B) + 空 payload tag(16B)。取 1KiB 余量（snow 对过小缓冲报错 →
/// 折叠为 HandshakeFailed，可被测试发现）。
const HANDSHAKE_BUF_LEN: usize = 1024;

/// snow 传输态单条报文的最大密文长度（65535B，snow `MAXMSGLEN`）减 AEAD tag
/// （16B，snow `TAGLEN`）；两常量未公开导出，此处按 snow 0.10 源码钉死并注释。
/// 应用帧（7B 帧头 + payload ≤ 65535）整体必须 ≤ 此值才能单报文密封。
pub const MAX_NOISE_PLAINTEXT: usize = 65535 - 16;

/// 握手角色（docs/02 §3.2：从机为 initiator，主控为 responder）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeRole {
    /// 从机侧：主动发出首条握手报文。
    Initiator,
    /// 主控侧：等待首条握手报文。
    Responder,
}

/// 握手截止时刻（调用方时钟控制的超时：构造即起算，`expired`/`remaining` 供
/// 握手循环检查——net 层不自管 sleep，tests 用短超时演示同一超时路径）。
#[derive(Debug, Clone, Copy)]
pub struct HandshakeDeadline {
    deadline: Instant,
}

impl HandshakeDeadline {
    /// 从现在起 `timeout` 后到期（生产路径用 [`HANDSHAKE_TIMEOUT`]，测试可传短值）。
    #[must_use]
    pub fn after(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
        }
    }

    /// 是否已到期。
    #[must_use]
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// 剩余时间（已到期 → ZERO）。
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Noise 握手入口（docs/02 §3.2）。
///
/// 全部失败路径（超时 / AEAD 解密失败 / 报文格式违规 / 对端中途断开）折叠为
/// [`NetError::HandshakeFailed`]——对端只见断开，无部分匹配反馈（docs/02 §0 WPS 教训）。
pub struct NoiseHandshake;

impl NoiseHandshake {
    /// 在 `io` 上以 `role` 角色执行完整 XXpsk0 三报文握手。
    ///
    /// 成功返回 [`SecureTransport`]（含 `session_id` 与 `hs_hash`）。注意：
    /// 本方法不关闭失败连接——失败即断开是调用方职责（M2 真实 TCP close；
    /// 回环场景调用方 shutdown，见 tests/chain.rs）。
    ///
    /// # Errors
    /// [`NetError::HandshakeFailed`]（唯一变体，原因折叠，见模块文档泄露纪律）。
    pub fn run<T: Transport>(
        io: &mut T,
        role: HandshakeRole,
        psk: &[u8; 32],
        deadline: &HandshakeDeadline,
    ) -> Result<SecureTransport, NetError> {
        let mut state = build_handshake_state(role, psk)?;
        let mut tx = [0u8; HANDSHAKE_BUF_LEN];
        let mut rx = Vec::new();
        match role {
            HandshakeRole::Initiator => {
                // -> psk0, e（psk0 是 msg1 首个 token，snow apply_psk_modifier 首位插入；
                // psk 派生的密钥加密空载荷 → 错码在 responder 首个解密点即失败）
                let n = state.write_message(&[], &mut tx).map_err(fail)?;
                io.send(&tx[..n]).map_err(fold_transport)?;
                // <- e, ee, s, es
                wait_message(io, deadline, &mut rx)?;
                state.read_message(&rx, &mut tx).map_err(fail)?;
                // -> s, es
                let n = state.write_message(&[], &mut tx).map_err(fail)?;
                io.send(&tx[..n]).map_err(fold_transport)?;
            }
            HandshakeRole::Responder => {
                // -> psk0, e
                wait_message(io, deadline, &mut rx)?;
                state.read_message(&rx, &mut tx).map_err(fail)?;
                // <- e, ee, s, es
                let n = state.write_message(&[], &mut tx).map_err(fail)?;
                io.send(&tx[..n]).map_err(fold_transport)?;
                // -> s, es
                wait_message(io, deadline, &mut rx)?;
                state.read_message(&rx, &mut tx).map_err(fail)?;
            }
        }
        // 握手摘要须在转入传输态前取出（TransportState 不再暴露）。
        let mut hs_hash = [0u8; 32];
        hs_hash.copy_from_slice(state.get_handshake_hash());
        let transport = state.into_transport_mode().map_err(fail)?;
        Ok(SecureTransport::new(transport, hs_hash))
    }

    /// 在异步 IO（`AsyncRead + AsyncWrite`）上执行完整 XXpsk0 三报文握手
    /// （M2 真实 TCP 路径；报文经 [`crate::framing`] 长度前缀分帧）。
    ///
    /// 报文序与折叠纪律同 [`NoiseHandshake::run`]；超时不在此计时——调用方以
    /// `tokio::time::timeout([HANDSHAKE_TIMEOUT], ...)` 包裹整个 future
    /// （docs/02 §3.2：5s 到期即取消 → 连接丢弃 → 对端只见断开）。
    ///
    /// 注意：本方法不关闭失败连接——失败即断开是调用方职责（[`crate::serve`] 的
    /// connect/accept 路径 drop 流；见模块文档泄露纪律）。
    ///
    /// # Errors
    /// [`NetError::HandshakeFailed`]（唯一变体：帧错误 / EOF / I/O 失败 /
    /// AEAD 解密失败 / 报文格式违规全折叠，同同步版）。
    pub async fn run_async<S>(
        io: &mut S,
        role: HandshakeRole,
        psk: &[u8; 32],
    ) -> Result<SecureTransport, NetError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut state = build_handshake_state(role, psk)?;
        let mut tx = [0u8; HANDSHAKE_BUF_LEN];
        let mut rx = Vec::new();
        match role {
            HandshakeRole::Initiator => {
                // -> psk0, e（序与同步版逐条对应，见 run 内注释）
                let n = state.write_message(&[], &mut tx).map_err(fail)?;
                framing::write_frame(io, &tx[..n])
                    .await
                    .map_err(fold_transport)?;
                // <- e, ee, s, es
                framing::read_frame(io, &mut rx)
                    .await
                    .map_err(fold_transport)?;
                state.read_message(&rx, &mut tx).map_err(fail)?;
                // -> s, es
                let n = state.write_message(&[], &mut tx).map_err(fail)?;
                framing::write_frame(io, &tx[..n])
                    .await
                    .map_err(fold_transport)?;
            }
            HandshakeRole::Responder => {
                // -> psk0, e（错码在首个解密点即失败：psk0 混入 msg1）
                framing::read_frame(io, &mut rx)
                    .await
                    .map_err(fold_transport)?;
                state.read_message(&rx, &mut tx).map_err(fail)?;
                // <- e, ee, s, es
                let n = state.write_message(&[], &mut tx).map_err(fail)?;
                framing::write_frame(io, &tx[..n])
                    .await
                    .map_err(fold_transport)?;
                // -> s, es
                framing::read_frame(io, &mut rx)
                    .await
                    .map_err(fold_transport)?;
                state.read_message(&rx, &mut tx).map_err(fail)?;
            }
        }
        // 握手摘要须在转入传输态前取出（同同步版）。
        let mut hs_hash = [0u8; 32];
        hs_hash.copy_from_slice(state.get_handshake_hash());
        let transport = state.into_transport_mode().map_err(fail)?;
        Ok(SecureTransport::new(transport, hs_hash))
    }
}

/// snow 错误 → 统一握手失败（原因折叠，不携带 snow 内部细节）。
fn fail(_: snow::Error) -> NetError {
    NetError::HandshakeFailed
}

/// 传输错误 → 统一握手失败（Closed/限速等同样折叠：对端只见断开）。
fn fold_transport(_: NetError) -> NetError {
    NetError::HandshakeFailed
}

/// 构建握手状态（psk(0) + prologue + 角色 + 本地一次性静态密钥）。
///
/// XX 模式需要本地静态密钥参与（snow 0.10 不代生成，缺省即 `Prereq` 错误）；
/// 每次 [`NoiseHandshake::run`] 用 `generate_keypair` 生成**一次性**静态密钥：
/// M1 无长期身份体系，PFS 由 XX 的临时密钥交换保证。
fn build_handshake_state(role: HandshakeRole, psk: &[u8; 32]) -> Result<HandshakeState, NetError> {
    let params: snow::params::NoiseParams = NOISE_PATTERN.parse().map_err(fail)?;
    let base = Builder::new(params);
    let keypair = base.generate_keypair().map_err(fail)?;
    let builder = base
        .local_private_key(&keypair.private)
        .map_err(fail)?
        .psk(0, psk)
        .map_err(fail)?
        .prologue(PROLOGUE)
        .map_err(fail)?;
    match role {
        HandshakeRole::Initiator => builder.build_initiator(),
        HandshakeRole::Responder => builder.build_responder(),
    }
    .map_err(fail)
}

/// 有界等待一个完整握手报文；对端断开/超时统一折叠为 HandshakeFailed。
fn wait_message<T: Transport>(
    io: &mut T,
    deadline: &HandshakeDeadline,
    out: &mut Vec<u8>,
) -> Result<(), NetError> {
    loop {
        match io.recv_timeout(deadline.remaining(), out) {
            Ok(true) => return Ok(()),
            // recv_timeout 以 remaining 为界：返回 false 即已到期。
            Ok(false) => {}
            // 对端断开/传输错误：对端只见断开，本端不区分原因。
            Err(_) => return Err(NetError::HandshakeFailed),
        }
        if deadline.expired() {
            return Err(NetError::HandshakeFailed);
        }
    }
}

/// Noise 传输态封装：`encrypt`/`decrypt` + 会话标识。
///
/// 单调 nonce、rekey、重放防护由 snow `TransportState` 内建（docs/02 §3.2：
/// 应用层不自管 nonce）。解密失败一律 [`NetError::DecryptFailed`] → 调用方断开
/// （docs/02 §7：协议违规不重试）。
#[derive(Debug)]
pub struct SecureTransport {
    state: TransportState,
    session_id: [u8; 16],
    hs_hash: [u8; 32],
}

impl SecureTransport {
    /// 从已完成的握手状态封装（`hs_hash` 为 `get_handshake_hash` 的拷贝）。
    fn new(state: TransportState, hs_hash: [u8; 32]) -> Self {
        // session_id = BLAKE3(hs_hash)[0..16]（docs/02 §3.3 的 net 侧本地等价实现；
        // 与 core::derive_session_id 的一致性由 tests/chain.rs 断言）。
        let digest = blake3::hash(&hs_hash);
        let mut session_id = [0u8; 16];
        session_id.copy_from_slice(&digest.as_bytes()[..16]);
        Self {
            state,
            session_id,
            hs_hash,
        }
    }

    /// 会话标识（`BLAKE3(hs_hash)[0..16]`；双方一致，用于日志/剪贴板通道绑定）。
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        self.session_id
    }

    /// 握手摘要（core `Session` 的 `HandshakeReady` 事件载荷）。
    #[must_use]
    pub const fn handshake_hash(&self) -> [u8; 32] {
        self.hs_hash
    }

    /// 单条 Noise 报文的明文上限（[`MAX_NOISE_PLAINTEXT`]）。
    #[must_use]
    pub const fn max_plaintext() -> usize {
        MAX_NOISE_PLAINTEXT
    }

    /// 加密一帧（应用帧整体作为明文，docs/02 §4.1 在密文内），密文追加到 `out`。
    ///
    /// # Errors
    /// [`NetError::EncryptFailed`]：明文超限或 snow 状态错误（不携带细节）。
    pub fn encrypt_into(&mut self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), NetError> {
        if plaintext.len() > MAX_NOISE_PLAINTEXT {
            return Err(NetError::EncryptFailed);
        }
        let mut buf = vec![0u8; plaintext.len() + 16];
        let n = self
            .state
            .write_message(plaintext, &mut buf)
            .map_err(|_| NetError::EncryptFailed)?;
        out.extend_from_slice(&buf[..n]);
        Ok(())
    }

    /// 解密一个完整 Noise 报文，明文追加到 `out`。
    ///
    /// # Errors
    /// [`NetError::DecryptFailed`]：AEAD 认证失败（篡改/密钥失配/报文错位）或
    /// snow 状态错误 → 调用方一律断开（docs/02 §7）。
    pub fn decrypt_into(&mut self, ciphertext: &[u8], out: &mut Vec<u8>) -> Result<(), NetError> {
        let mut buf = vec![0u8; ciphertext.len()];
        let n = self
            .state
            .read_message(ciphertext, &mut buf)
            .map_err(|_| NetError::DecryptFailed)?;
        out.extend_from_slice(&buf[..n]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::psk::derive_psk;
    use crate::transport::LoopbackTransport;

    /// 测试共用假码派生（m=19MiB argon2，进程内仅派生一次）。
    fn test_psk() -> [u8; 32] {
        static PSK: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        *PSK.get_or_init(|| derive_psk("test-code", &[0u8; 16]).unwrap())
    }

    /// 双线程跑完整握手（回环零延迟），返回双方 SecureTransport。
    fn handshake_pair(psk: [u8; 32]) -> (SecureTransport, SecureTransport) {
        let (mut a, mut b, _control) = LoopbackTransport::pair(None);
        let deadline = HandshakeDeadline::after(HANDSHAKE_TIMEOUT);
        let peer = thread::spawn(move || {
            NoiseHandshake::run(&mut b, HandshakeRole::Responder, &psk, &deadline)
        });
        let initiator = NoiseHandshake::run(&mut a, HandshakeRole::Initiator, &psk, &deadline)
            .expect("initiator 握手成功");
        let responder = peer
            .join()
            .expect("responder 线程")
            .expect("responder 握手成功");
        (initiator, responder)
    }

    /// 锚点：模式串/prologue/超时常量与 docs/02 §3.2 逐字一致。
    #[test]
    fn constants_match_docs() {
        assert_eq!(NOISE_PATTERN, "Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s");
        assert_eq!(PROLOGUE, b"xcontrol/1");
        assert_eq!(HANDSHAKE_TIMEOUT, Duration::from_secs(5));
        assert_eq!(SecureTransport::max_plaintext(), 65519);
    }

    /// 握手成功：双方 session_id 一致且与 core::derive_session_id 等价
    /// （docs/02 §3.3；此处断言自一致，跨 crate 一致性在 tests/chain.rs）。
    #[test]
    fn handshake_success_symmetric_ids() {
        let (initiator, responder) = handshake_pair(test_psk());
        assert_eq!(initiator.session_id(), responder.session_id());
        assert_eq!(initiator.handshake_hash(), responder.handshake_hash());
        let digest = blake3::hash(&initiator.handshake_hash());
        assert_eq!(initiator.session_id(), digest.as_bytes()[..16]);
        // hs_hash 非零（BLAKE2s 握手链输出）
        assert_ne!(initiator.handshake_hash(), [0u8; 32]);
    }

    /// 传输态往返：加密 → 解密保真；篡改密文 → DecryptFailed。
    #[test]
    fn transport_roundtrip_and_tamper() {
        let (mut initiator, mut responder) = handshake_pair(test_psk());

        let mut ciphertext = Vec::new();
        initiator
            .encrypt_into(b"hello xcp", &mut ciphertext)
            .unwrap();
        let mut plaintext = Vec::new();
        responder.decrypt_into(&ciphertext, &mut plaintext).unwrap();
        assert_eq!(plaintext, b"hello xcp");

        // 反向同理（密钥双向派生自同一握手链）
        let mut back = Vec::new();
        responder.encrypt_into(b"ack", &mut back).unwrap();
        let mut plain_back = Vec::new();
        initiator.decrypt_into(&back, &mut plain_back).unwrap();
        assert_eq!(plain_back, b"ack");

        // 篡改密文 → DecryptFailed（非 HandshakeFailed：传输态违规语义）
        let mut tampered = ciphertext;
        tampered[0] ^= 0xFF;
        assert_eq!(
            responder.decrypt_into(&tampered, &mut Vec::new()),
            Err(NetError::DecryptFailed)
        );
    }

    /// 超长明文 → EncryptFailed（MAX_PLAINTEXT 上限锚定）。
    #[test]
    fn encrypt_rejects_oversized_plaintext() {
        let (mut initiator, _responder) = handshake_pair(test_psk());
        let oversized = vec![0u8; MAX_NOISE_PLAINTEXT + 1];
        assert_eq!(
            initiator.encrypt_into(&oversized, &mut Vec::new()),
            Err(NetError::EncryptFailed)
        );
        // 上限以内正常加密（65519B 单报文）
        let at_limit = vec![0xA5u8; MAX_NOISE_PLAINTEXT];
        let mut out = Vec::new();
        initiator.encrypt_into(&at_limit, &mut out).unwrap();
        assert!(!out.is_empty());
    }

    /// 安全码不匹配：双方均得 HandshakeFailed（无部分匹配反馈，docs/02 §0）。
    ///
    /// 双侧对称并发（不预设谁先在哪个报文上失败——psk0 混入使错码方在首个
    /// 解密点即失败）；先返回方 shutdown 唤醒仍在等待的对方，2s 短 deadline
    /// 作兜底（超时与解密失败折叠为同一变体，正是本测试要断言的语义）。
    #[test]
    fn handshake_psk_mismatch_folds_to_single_variant() {
        static PSK_A: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        static PSK_B: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        let a = *PSK_A.get_or_init(|| derive_psk("code-a", &[0u8; 16]).unwrap());
        let b = *PSK_B.get_or_init(|| derive_psk("code-b", &[0u8; 16]).unwrap());

        let (client_wire, host_wire, _control) = LoopbackTransport::pair(None);
        // 主线程保留 client_wire 克隆：先完成方 shutdown 唤醒仍阻塞的一方（失败即断开）。
        let mut wake_wire = client_wire.clone();
        let deadline = || HandshakeDeadline::after(Duration::from_secs(2));
        let client_handle = thread::spawn(move || {
            let mut wire = client_wire;
            NoiseHandshake::run(&mut wire, HandshakeRole::Initiator, &a, &deadline())
        });
        let host_handle = thread::spawn(move || {
            let mut wire = host_wire;
            NoiseHandshake::run(&mut wire, HandshakeRole::Responder, &b, &deadline())
        });
        let host_result = host_handle.join().unwrap();
        wake_wire.shutdown();
        let client_result = client_handle.join().unwrap();
        assert!(matches!(host_result, Err(NetError::HandshakeFailed)));
        assert!(matches!(client_result, Err(NetError::HandshakeFailed)));
    }

    /// 对端静默 → 真超时路径：链路健在但无报文，deadline 到期 → HandshakeFailed
    /// （deadline 由测试注入短值演示，与生产 5s 同一代码路径，docs/02 §3.2）。
    #[test]
    fn handshake_timeout_when_peer_silent() {
        let (mut a, mut _b, _control) = LoopbackTransport::pair(None);
        let started = Instant::now();
        let result = NoiseHandshake::run(
            &mut a,
            HandshakeRole::Initiator,
            &test_psk(),
            &HandshakeDeadline::after(Duration::from_millis(100)),
        );
        assert!(matches!(result, Err(NetError::HandshakeFailed)));
        assert!(
            started.elapsed() >= Duration::from_millis(90),
            "应真实等待 deadline 到期而非提前失败"
        );
    }

    /// 对端断开（链路切断）→ 立即 HandshakeFailed（不等 deadline，docs/02 §3.2 失败即断开）。
    #[test]
    fn handshake_fails_immediately_on_disconnected_link() {
        let (mut a, mut _b, control) = LoopbackTransport::pair(None);
        control.disconnect();
        let started = Instant::now();
        let result = NoiseHandshake::run(
            &mut a,
            HandshakeRole::Responder,
            &test_psk(),
            &HandshakeDeadline::after(Duration::from_secs(5)),
        );
        assert!(matches!(result, Err(NetError::HandshakeFailed)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "链路已断应立即失败，不得等待 deadline"
        );
    }

    /// HandshakeDeadline：expired/remaining 语义（短超时演示，不真实 sleep 5s）。
    #[test]
    fn deadline_expiry_semantics() {
        let short = HandshakeDeadline::after(Duration::from_millis(20));
        assert!(!short.expired());
        assert!(short.remaining() <= Duration::from_millis(20));
        thread::sleep(Duration::from_millis(40));
        assert!(short.expired());
        assert_eq!(short.remaining(), Duration::ZERO);
        // HANDSHAKE_TIMEOUT 构造（生产路径形态）
        let long = HandshakeDeadline::after(HANDSHAKE_TIMEOUT);
        assert!(!long.expired());
        assert!(long.remaining() > Duration::from_secs(4));
    }

    /// 编译期断言的运行时锚点（常量比较无运行时意义，clippy::assertions_on_constants）。
    #[test]
    fn handshake_buffer_headroom() {
        // 上界锚定最大握手报文 msg2 = e(32) + 加密静态钥(48) + 空 payload tag(16) = 96B。
        const BUF_HEADROOM_OK: () = assert!(HANDSHAKE_BUF_LEN >= 96, "缓冲容纳不下 msg2");
        let _: () = BUF_HEADROOM_OK;
    }

    // ===== M2：run_async（AsyncRead/AsyncWrite 路径）=====
    //
    // 测试载体 = tokio::io::duplex（内存双工流，无需真实 socket；真实 TCP
    // 全链路见 tests/tcp.rs 集成测试三路径）。

    /// 异步握手成功：双端 session_id 一致（docs/02 §3.3）。XX 每次握手用
    /// 一次性静态密钥，hs_hash 跨次必不同——不与同步版比对具体值，只断言
    /// 异步双端自洽 + session_id = BLAKE3(hs_hash)[0..16] 派生关系成立。
    #[tokio::test]
    async fn run_async_success_symmetric_ids() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let psk = test_psk();
        let responder = tokio::spawn(async move {
            NoiseHandshake::run_async(&mut server, HandshakeRole::Responder, &psk).await
        });
        let initiator =
            NoiseHandshake::run_async(&mut client, HandshakeRole::Initiator, &test_psk())
                .await
                .expect("async initiator 握手成功");
        let responder = responder
            .await
            .expect("async responder 任务")
            .expect("async responder 握手成功");
        assert_eq!(initiator.session_id(), responder.session_id());
        assert_eq!(initiator.handshake_hash(), responder.handshake_hash());
        let digest = blake3::hash(&initiator.handshake_hash());
        assert_eq!(initiator.session_id(), digest.as_bytes()[..16]);
        assert_ne!(initiator.handshake_hash(), [0u8; 32]);
    }

    /// 异步握手 + 错码：双方均折叠 HandshakeFailed（无部分匹配反馈，同同步版
    /// 泄露纪律）；2s timeout 兜底防测试挂死（错码本应在首个解密点即失败）。
    #[tokio::test]
    async fn run_async_psk_mismatch_folds() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let psk_a = crate::psk::derive_psk("async-code-a", &[0u8; 16]).unwrap();
        let psk_b = crate::psk::derive_psk("async-code-b", &[0u8; 16]).unwrap();
        let responder = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(2),
                NoiseHandshake::run_async(&mut server, HandshakeRole::Responder, &psk_b),
            )
            .await
        });
        let initiator = tokio::time::timeout(
            Duration::from_secs(2),
            NoiseHandshake::run_async(&mut client, HandshakeRole::Initiator, &psk_a),
        )
        .await
        .expect("错码握手应在 2s 内返回（兜底窗口）")
        .expect_err("错码握手必须失败");
        let responder = responder
            .await
            .expect("responder 任务正常结束")
            .expect("错码握手应在 2s 内返回（兜底窗口）")
            .expect_err("错码握手必须失败");
        assert_eq!(initiator, NetError::HandshakeFailed);
        assert_eq!(responder, NetError::HandshakeFailed);
    }

    /// 对端静默：timeout 包裹到期 → HandshakeFailed（5s 生产窗口的测试短值
    /// 演示，与 serve.rs 生产路径同一包裹形态）。
    #[tokio::test]
    async fn run_async_times_out_when_peer_silent() {
        let (mut client, _server) = tokio::io::duplex(1024);
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            NoiseHandshake::run_async(&mut client, HandshakeRole::Initiator, &test_psk()),
        )
        .await;
        assert!(
            matches!(result, Err(_elapsed)),
            "timeout 到期应返回 Elapsed 错误（调用方折叠 HandshakeFailed）"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(90),
            "应真实等待窗口到期"
        );
    }
}
