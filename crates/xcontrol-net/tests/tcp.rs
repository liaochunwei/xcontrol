//! TCP 传输与安全码握手集成测试（11-17-m2-tcp，docs/02 §1/§3.2、FR-1.4 可自动化部分）。
//!
//! 三路径（design.md 测试设计；`127.0.0.1:0` 随机端口，`tokio::spawn` 双端）：
//! 1. **握手成功全链路**：真实 TCP + 真实 XXpsk0 → `session_id` 两侧一致
//!    （docs/02 §3.3）+ 应用帧往返（docs/02 §4.1 帧在密文内）；
//! 2. **错误安全码**（FR-1.4 验收）：双方 `HandshakeFailed`、连接断开、
//!    无"部分匹配"泄漏（错误折叠为唯一出口变体，docs/02 §0）；
//! 3. **限速**：同 addr 连打 6 次，第 6 次**触网前**被 Backoff 拒绝
//!    （`RateLimited`，docs/02 §3.2）。
//!
//! 另含真实 TCP 字节流上的粘包/拆包场景：一次 `write` 全帧 vs 分两次 `write`
//! 半帧——解码还原等价（分帧适配器正确性，prd 验收项 2）。
//!
//! 公共 API 边界：本文件只走 `xcontrol_net` 公开入口（serve/connect/TcpTransport/
//! framing/noise），与未来 M3 会话层的调用形态一致。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use xcontrol_net::noise::{HandshakeRole, NoiseHandshake};
use xcontrol_net::serve::{self, ServerOptions};
use xcontrol_net::throttle::{HandshakeThrottle, ThrottleConfig};
use xcontrol_net::{Connection, NetError, Server, TcpTransport, framing};
use xcontrol_proto::input::{InputEvent, KeyState};
use xcontrol_proto::{Message, xkey};

/// 测试假码派生（m=19MiB argon2，每码进程内一次；docs/08 §4 假码纪律）。
fn test_psk(code: &'static str) -> [u8; 32] {
    static GOOD: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    static BAD: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let derived = || {
        must(
            "固定输入派生（测试假码）",
            xcontrol_net::derive_psk(code, &[0u8; 16]),
        )
    };
    match code {
        "tcp-good-code" => *GOOD.get_or_init(derived),
        _ => *BAD.get_or_init(derived),
    }
}

/// 测试辅助取值（helpers 非 `#[test]` 作用域，clippy 豁免不覆盖——与 chain.rs 同形）。
#[track_caller]
fn must<T, E: std::fmt::Debug>(context: &'static str, result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"),
    }
}

/// 测试限速参数：阈值保持 docs/02 §3.2 的 5 次，基数/封顶/间隔缩小到毫秒级
/// （同一逻辑路径，throttle.rs 单测已覆盖默认常量）。
fn fast_throttle() -> ThrottleConfig {
    ThrottleConfig {
        max_failures: 5,
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        min_interval: Duration::from_millis(1),
    }
}

/// `127.0.0.1:0` 字面量解析（测试地址恒合法，非 test 作用域走 must 模式）。
fn loopback_any_port() -> SocketAddr {
    must("常量地址解析", "127.0.0.1:0".parse())
}

/// 启动测试主控（`127.0.0.1:0` 随机端口），返回 (Server, 监听地址)。
async fn spawn_server() -> (Server, SocketAddr) {
    let opts = ServerOptions {
        bind: loopback_any_port(),
        throttle: fast_throttle(),
        allowlist: Vec::new(),
    };
    let server = must(
        "serve 启动",
        serve::serve(opts, &test_psk("tcp-good-code")).await,
    );
    let addr = server.local_addr();
    (server, addr)
}

/// 等待 serve 交付一条握手完成连接（5s 兜底）。
async fn next_conn(server: &mut Server) -> Connection {
    let delivered = tokio::time::timeout(Duration::from_secs(5), server.next_connection()).await;
    match must("握手交付在 5s 内", delivered) {
        Some(conn) => conn,
        None => panic!("服务未停止时应有连接交付"),
    }
}

/// 场景 1（FR-1.4 成功路径）：真实 TCP + 真实 XXpsk0 全链路——握手、会话标识
/// 双端一致、应用帧双向多帧往返（输入事件帧整体进单条 Noise 报文，docs/02 §4.1）。
#[tokio::test]
async fn localhost_handshake_full_path_and_app_frames() {
    let (mut server, addr) = spawn_server().await;
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());

    let mut client = must(
        "connect 握手成功",
        serve::connect(addr, &test_psk("tcp-good-code"), &mut throttle).await,
    );
    let mut server_side = next_conn(&mut server).await;

    // 会话标识（docs/02 §3.3）：双方一致；与握手摘要派生关系成立。
    assert_eq!(
        client.session_id, server_side.session_id,
        "session_id 双端一致"
    );
    let digest = blake3::hash(&client.transport.handshake_hash());
    assert_eq!(&client.session_id[..], &digest.as_bytes()[..16]);
    assert!(throttle.is_empty(), "record_success 清空限速窗口");

    // 应用帧（真实输入事件帧，7B 帧头 + payload）双向多帧往返。
    let key_down = InputEvent::Key {
        code: xkey::KEY_A,
        state: KeyState::Down,
    }
    .encode_frame(1)
    .expect("测试帧编码（合法输入）");
    let key_up = InputEvent::Key {
        code: xkey::KEY_A,
        state: KeyState::Up,
    }
    .encode_frame(2)
    .expect("测试帧编码（合法输入）");

    client.transport.send(&key_down).await.unwrap();
    client.transport.send(&key_up).await.unwrap();
    let mut out = Vec::new();
    server_side.transport.recv(&mut out).await.unwrap();
    assert_eq!(out, key_down, "帧序保真（TCP + Noise 均有序）");
    server_side.transport.recv(&mut out).await.unwrap();
    assert_eq!(out, key_up);

    server_side
        .transport
        .send(
            &InputEvent::Key {
                code: xkey::KEY_B,
                state: KeyState::Down,
            }
            .encode_frame(3)
            .expect("测试帧编码（合法输入）"),
        )
        .await
        .unwrap();
    client.transport.recv(&mut out).await.unwrap();
    assert_eq!(
        out[4], 0x01,
        "帧头小端 seq(4B) 后即 kind：Key=0x01（docs/02 §4.1/§4.2）"
    );
}

/// 场景 2（FR-1.4 拒绝路径）：错误安全码 → 双方 HandshakeFailed、连接断开、
/// 无部分匹配泄漏（唯一出口变体；错误码不回传、连接直接消失）；随后正确
/// 安全码握手成功——服务未被污染、无"半匹配"状态残留。
#[tokio::test]
async fn wrong_security_code_rejected_without_leakage() {
    let (mut server, addr) = spawn_server().await;

    // ① 错码连接：折叠 HandshakeFailed（从机侧无法区分错码/超时/断开——无 oracle）。
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());
    let started = Instant::now();
    let result = serve::connect(addr, &test_psk("tcp-bad-code"), &mut throttle).await;
    assert!(
        matches!(result, Err(NetError::HandshakeFailed)),
        "FR-1.4：错码必须折叠为握手失败：{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "错码在首个解密点即失败（psk0 混入 msg1），非超时兜底"
    );
    assert_eq!(throttle.failure_count(&addr), 1, "失败计入从机侧限速窗口");

    // ② 连接断开：serve 侧无交付（宽限窗内通道保持为空）。
    let nothing = tokio::time::timeout(Duration::from_millis(200), server.next_connection()).await;
    assert!(
        nothing.is_err(),
        "错码连接不得交付（docs/02 §0：对端只见断开）"
    );

    // ③ 正确安全码随即成功：无部分匹配泄漏的服务级佐证。
    let mut fresh = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());
    let client = must(
        "错码后正确码握手",
        serve::connect(addr, &test_psk("tcp-good-code"), &mut fresh).await,
    );
    let server_side = next_conn(&mut server).await;
    assert_eq!(client.session_id, server_side.session_id);
}

/// 场景 3（docs/02 §3.2 限速集成）：同 addr 连打 6 次——前 5 次真实触网失败
/// （错码握手，计入退避），第 6 次**触网前**被本地 Backoff 拒绝
/// （`RateLimited`；`HandshakeFailed` 必然经历过网络往返，变体即证明）。
#[tokio::test]
async fn rate_limit_rejects_sixth_attempt_before_dialing() {
    let (mut server, addr) = spawn_server().await;
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());

    for attempt in 1..=5_u32 {
        // 间隔 > min_interval：本用例只验证 Backoff 路径（Cooldown 归 throttle 单测）。
        tokio::time::sleep(Duration::from_millis(10)).await;
        let result = serve::connect(addr, &test_psk("tcp-bad-code"), &mut throttle).await;
        assert!(
            matches!(result, Err(NetError::HandshakeFailed)),
            "第 {attempt} 次应为真实握手失败：{result:?}"
        );
        assert_eq!(throttle.failure_count(&addr), attempt);
    }

    // 第 6 次：本地退避拒绝（触网前）。
    let started = Instant::now();
    let result = serve::connect(addr, &test_psk("tcp-bad-code"), &mut throttle).await;
    assert!(
        matches!(result, Err(NetError::RateLimited)),
        "第 6 次必须触网前被拒：{result:?}"
    );
    assert_eq!(
        throttle.failure_count(&addr),
        5,
        "被本地拒绝的尝试不计失败（未触网）"
    );
    assert!(
        started.elapsed() < Duration::from_millis(40),
        "本地拒绝零网络往返（对比：真实错码握手 >40ms 间隔）"
    );

    // serve 侧在自身退避（同参数，第 5 次失败触发 50ms）期满后仍全链路健康。
    tokio::time::sleep(Duration::from_millis(80)).await;
    let mut fresh = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());
    let client = must(
        "限速后服务不受影响",
        serve::connect(addr, &test_psk("tcp-good-code"), &mut fresh).await,
    );
    let server_side = next_conn(&mut server).await;
    assert_eq!(client.session_id, server_side.session_id);
}

/// 场景 4（prd 验收项 2）：真实 TCP 字节流上的粘包/拆包。
///
/// 发送端持**原始 socket**（握手后不走 `TcpTransport::send`，手工拼帧），接收端
/// 为 [`TcpTransport`]：
/// - **粘包**：两条完整帧一次 `write_all` 写出（TCP 单段交付多帧）→ 接收端
///   逐帧解码还原等价；
/// - **拆包**：一条帧分两次 `write`（半帧 + 余下，中间停顿）→ `read_exact`
///   两段式凑满后解码还原等价。
#[tokio::test]
async fn coalesced_and_split_frames_on_real_tcp() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定回环随机端口");
    let addr = listener.local_addr().expect("监听地址");
    let psk = test_psk("tcp-good-code");
    let responder = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("接受连接");
        stream.set_nodelay(true).expect("NODELAY");
        let secure = NoiseHandshake::run_async(&mut stream, HandshakeRole::Responder, &psk)
            .await
            .expect("responder 握手");
        TcpTransport::new(stream, secure, peer)
    });

    let (mut raw, mut secure) = {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("连接监听端");
        stream.set_nodelay(true).expect("NODELAY");
        let secure = NoiseHandshake::run_async(&mut stream, HandshakeRole::Initiator, &psk)
            .await
            .expect("initiator 握手");
        (stream, secure)
    };
    let mut receiver = responder.await.expect("responder 任务");
    assert_eq!(receiver.session_id(), secure.session_id());

    // 三个互异的应用帧（docs/02 §4.1：密文内）。
    let frames = [
        InputEvent::Key {
            code: xkey::KEY_A,
            state: KeyState::Down,
        }
        .encode_frame(1)
        .expect("帧编码"),
        InputEvent::Key {
            code: xkey::KEY_A,
            state: KeyState::Up,
        }
        .encode_frame(2)
        .expect("帧编码"),
        InputEvent::MouseMoveRel { dx: 1.5, dy: -2.5 }
            .encode_frame(3)
            .expect("帧编码"),
    ];

    // —— 粘包：帧 0 + 帧 1 各自加密成帧后拼接，一次 write 全部写出 ——
    let mut wire = Vec::new();
    for frame in &frames[..2] {
        let mut ciphertext = Vec::new();
        secure.encrypt_into(frame, &mut ciphertext).unwrap();
        framing::encode_into(&ciphertext, &mut wire).unwrap();
    }
    raw.write_all(&wire).await.unwrap();

    let mut out = Vec::new();
    receiver.recv(&mut out).await.unwrap();
    assert_eq!(out, frames[0], "粘包流逐帧还原（帧 0）");
    receiver.recv(&mut out).await.unwrap();
    assert_eq!(out, frames[1], "粘包流逐帧还原（帧 1）");

    // —— 拆包：帧 2 分两次 write（半帧 → 停顿 → 余下）——
    let mut ciphertext = Vec::new();
    secure.encrypt_into(&frames[2], &mut ciphertext).unwrap();
    let mut full = Vec::new();
    framing::encode_into(&ciphertext, &mut full).unwrap();
    let mid = full.len() / 2;
    raw.write_all(&full[..mid]).await.unwrap();
    // 半帧在途时接收端不得产出任何帧——真实 socket 上由 read_exact 阻塞等待；
    // 停顿一拍制造真实的分段到达。
    tokio::time::sleep(Duration::from_millis(50)).await;
    raw.write_all(&full[mid..]).await.unwrap();
    receiver.recv(&mut out).await.unwrap();
    assert_eq!(out, frames[2], "半帧 + 余下 → 解码还原等价");

    // 接收端无残留：流上恰好多余字节为零（下一条 recv 等 EOF——对端 shutdown）。
    raw.shutdown().await.unwrap();
    assert!(
        matches!(receiver.recv(&mut out).await, Err(NetError::Closed)),
        "流排空后 recv 得 Closed（无粘包残渣、无重复解码）"
    );
}

/// 场景 5：多连接并发交付（docs/02 §1：主控支持 N 台从机，FR-1.2 的传输层前提）。
/// 两条独立连接先后完成握手，session_id 互异（一次性静态密钥 → 握手摘要互异），
/// 各自帧流互不串扰。
#[tokio::test]
async fn two_connections_delivered_independently() {
    let (mut server, addr) = spawn_server().await;
    let psk = test_psk("tcp-good-code");

    let mut client_a = {
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());
        must("A 握手", serve::connect(addr, &psk, &mut throttle).await)
    };
    let mut client_b = {
        let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(fast_throttle());
        must("B 握手", serve::connect(addr, &psk, &mut throttle).await)
    };
    let mut server_a = next_conn(&mut server).await;
    let mut server_b = next_conn(&mut server).await;

    assert_ne!(
        client_a.session_id, client_b.session_id,
        "一次性静态密钥 → 会话标识互异（无跨连接串扰）"
    );
    client_a.transport.send(b"a->").await.unwrap();
    client_b.transport.send(b"b->").await.unwrap();
    let mut out = Vec::new();
    server_a.transport.recv(&mut out).await.unwrap();
    assert_eq!(out, b"a->", "连接 A 帧不串扰");
    server_b.transport.recv(&mut out).await.unwrap();
    assert_eq!(out, b"b->", "连接 B 帧不串扰");
}
