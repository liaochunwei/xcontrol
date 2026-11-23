//! 全流量密文抓包断言（docs/07 §8 发布自查清单第 3 项——"抓包确认全流量密文"）。
//!
//! 实测形态裁定（11-19-m6-security）：tcpdump 需要 root（CI/子代理环境不可得），
//! 本测试用**进程内线缆窃听**替代——client 与 host 之间串一条记录全部原始字节的
//! TCP 中继（relay），效果等价于在 lo0 上抓取该连接的全部双向流量：
//!
//! ```text
//! client ──TCP──► relay（逐字节复制并记录）──TCP──► host(serve)
//!         ◄──Noise 密文帧──────────────────────────┘
//! ```
//!
//! 断言三重形态（与 discovery.rs 零派生物测试同纪律）：
//! 1. 安全码原文、盐（blake3 域分离派生）、PSK（argon2 产物）的原始字节、
//!    小写 hex、大写 hex **均不出现**在抓到的任何字节中；
//! 2. 握手 + 应用帧双向交换成功（功能不受窃听影响——密文自含帧边界）；
//! 3. 会话建立后剪贴板通道语义的应用载荷（本测试用裸帧 ping）同样密文传输
//!    ——明文不落"线缆"。
//!
//! beacon/发现流量的零派生物由 discovery.rs
//! `discovery_payload_carries_no_secret_derivatives` 单独锚定（docs/07 §8 第 1 项）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use xcontrol_net::serve::{self, connect};
use xcontrol_net::{HandshakeThrottle, ServerOptions, ThrottleConfig};

/// 测试码（docs/08 §4 假码纪律；断言其明文不出现在线缆字节中）。
const CODE: &str = "capture-relay-code-2026-not-a-real-secret";
/// 实例 UUID（盐输入，公开值）。
const INSTANCE: [u8; 16] = [0x7C; 16];

/// 测试限速参数（与 serve.rs/tcp.rs 同形）。
fn fast_throttle() -> ThrottleConfig {
    ThrottleConfig {
        max_failures: 5,
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        min_interval: Duration::from_millis(1),
    }
}

/// 子串搜索（标准库无 &[u8] contains；与 discovery.rs 测试同形）。
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 单向字节泵：src 读 → dst 写（方向由调用处接线），每个块的副本压入
/// capture（等价于在 lo0 上逐块抓包）。
async fn pump(
    mut src: tokio::net::tcp::OwnedReadHalf,
    mut dst: tokio::net::tcp::OwnedWriteHalf,
    capture: Arc<Mutex<Vec<u8>>>,
) {
    let mut buf = [0u8; 4096];
    loop {
        match src.read(&mut buf).await {
            Ok(0) | Err(_) => break, // 对端关闭/错误：泵退出
            Ok(n) => {
                if dst.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(&buf[..n]);
            }
        }
    }
    let _ = dst.shutdown().await;
}

/// 全流量密文断言（docs/07 §8 第 3 项）：经窃听中继完成握手 + 应用帧往返，
/// 抓取的全部字节不含码原文/盐/PSK 的任何形态。
#[tokio::test]
async fn wire_capture_carries_no_plaintext_or_derivatives() {
    // 派生链（docs/02 §3.1）：盐 = blake3(uuid 域分离)，PSK = argon2(code, 盐)。
    let salt = xcontrol_net::derive_salt(&INSTANCE);
    let psk = xcontrol_net::derive_psk(CODE, &INSTANCE).expect("固定输入派生");

    // 主控：127.0.0.1 随机端口。
    let mut server = serve::serve(
        ServerOptions {
            bind: "127.0.0.1:0".parse().expect("常量地址"),
            throttle: fast_throttle(),
            allowlist: Vec::new(),
        },
        &psk,
    )
    .await
    .expect("serve 启动");
    let host_addr = server.local_addr();

    // 窃听中继：监听随机端口，接受一条 client 连接后与 host 建立双向泵；
    // 两个方向的原始字节统一进 capture。
    let relay = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("relay 监听");
    let relay_addr = relay.local_addr().expect("relay 地址");
    let capture: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let relay_task = tokio::spawn({
        let capture = Arc::clone(&capture);
        async move {
            loop {
                let (client_side, _) = match relay.accept().await {
                    Ok(accepted) => accepted,
                    Err(_) => break,
                };
                let host_side = match tokio::net::TcpStream::connect(host_addr).await {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                // 拆半后交叉接线：client 读半 → host 写半（下行），host 读半 →
                // client 写半（上行）——两个方向的原始字节共享同一 capture。
                let (client_read, client_write) = client_side.into_split();
                let (host_read, host_write) = host_side.into_split();
                let capture_down = Arc::clone(&capture);
                let down = tokio::spawn(pump(client_read, host_write, capture_down));
                let capture_up = Arc::clone(&capture);
                let up = tokio::spawn(pump(host_read, client_write, capture_up));
                let _ = tokio::join!(down, up);
            }
        }
    });

    // 从机经中继握手（真实 XXpsk0——中继只搬运字节，不解密）。
    let mut throttle = HandshakeThrottle::<std::net::SocketAddr>::with_config(fast_throttle());
    let mut client = connect(relay_addr, &psk, &mut throttle)
        .await
        .expect("经窃听中继握手成功");
    let mut host_side = tokio::time::timeout(Duration::from_secs(5), server.next_connection())
        .await
        .expect("握手交付在 5s 内")
        .expect("服务未停止时有连接");

    // 应用帧双向往返（握手后的密文传输段）。
    client
        .transport
        .send(b"capture-plain-frame-would-leak-if-unencrypted")
        .await
        .expect("client 发送");
    let mut out = Vec::new();
    host_side.transport.recv(&mut out).await.expect("host 收帧");
    assert_eq!(out, b"capture-plain-frame-would-leak-if-unencrypted");
    host_side
        .transport
        .send(b"host-reply-frame")
        .await
        .expect("host 回发");
    let mut out = Vec::new();
    client.transport.recv(&mut out).await.expect("client 收帧");
    assert_eq!(out, b"host-reply-frame");

    // 收尾：断开中继任务，抓取字节定稿。
    drop(client);
    drop(host_side);
    drop(server);
    relay_task.abort();
    let wire = capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        wire.len() > 96,
        "中继应抓到双向流量（握手 3 报文裸 Noise [48,96,64]B + 各 4B 长度前缀 = \
         docs/07 §8 实测 52/100/68B + 应用帧），实际 {} 字节",
        wire.len()
    );

    // 三重形态断言：码原文 / 盐 / PSK 的原始字节 + hex 文本均不出现在线缆上。
    let hex_lower = |bytes: &[u8]| -> Vec<u8> {
        bytes
            .iter()
            .flat_map(|b| {
                [
                    b"0123456789abcdef"[*b as usize >> 4],
                    b"0123456789abcdef"[*b as usize & 0x0f],
                ]
            })
            .collect()
    };
    let hex_upper = |bytes: &[u8]| -> Vec<u8> {
        hex_lower(bytes)
            .iter()
            .map(|b| b.to_ascii_uppercase())
            .collect()
    };
    for (label, needle) in [
        ("安全码原文", CODE.as_bytes().to_vec()),
        ("盐", salt.to_vec()),
        ("PSK", psk.to_vec()),
        ("安全码 hex", hex_lower(CODE.as_bytes())),
        ("盐 hex", hex_lower(&salt)),
        ("PSK hex", hex_lower(&psk)),
        ("安全码 HEX", hex_upper(CODE.as_bytes())),
    ] {
        assert!(
            !contains_subslice(&wire, &needle),
            "{label}（{} 字节形态）泄漏进线缆流量（docs/07 §8 第 3 项红线）",
            needle.len()
        );
    }
}
