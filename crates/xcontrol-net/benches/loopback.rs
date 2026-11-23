//! 回环往返延迟基准（docs/08 §4 性能层；NFR-1 分量验收：回环 P50 < 5ms）。
//!
//! 运行（正确形态：`--bench loopback` 定向 criterion 目标，否则参数转发给
//! libtest 报 Unrecognized option）：
//! ```text
//! cargo bench -p xcontrol-net --bench loopback -- \
//!     --measurement-time 1 --warm-up-time 0.3 --sample-size 10
//! ```
//!
//! 分组：
//! - `loopback_roundtrip`：encrypt → 回环 send/recv → decrypt（含 Noise 加解密，
//!   同步实现零 tokio——基准最纯形态，design.md）；
//! - `crypto_only`：仅 encrypt/decrypt（隔离传输开销）；
//! - `handshake`：完整 XXpsk0 三报文握手（连接建立路径，非热路径，参考值）。

use std::hint::black_box;
use std::thread;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use xcontrol_net::transport::Transport;
use xcontrol_net::{
    HandshakeDeadline, HandshakeRole, LoopbackTransport, NoiseHandshake, SecureTransport,
};
use xcontrol_proto::input::{InputEvent, KeyState};
use xcontrol_proto::{Message, xkey};

/// bench 内取值：失败即 panic（bench 闭包/helpers 非 `#[test]` 作用域，
/// clippy expect/unwrap 豁免不覆盖——与 proto/codec.rs 的 `must` 同形）。
#[track_caller]
fn must<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("bench 操作失败：{error:?}"),
    }
}

/// 建立一对已握手的传输态 + 同链路回环双端（每次基准设置执行一次）。
/// 握手线程持有线端原身，返回给基准的是同链路克隆（握手后帧流继续走该链路）。
fn bench_pair(
    psk: &[u8; 32],
) -> (
    SecureTransport,
    SecureTransport,
    LoopbackTransport,
    LoopbackTransport,
) {
    let (client_wire, host_wire, _control) = LoopbackTransport::pair(None);
    let client_wire_clone = client_wire.clone();
    let host_wire_clone = host_wire.clone();
    let psk = *psk;
    let client = thread::spawn(move || {
        let mut wire = client_wire;
        NoiseHandshake::run(
            &mut wire,
            HandshakeRole::Initiator,
            &psk,
            &HandshakeDeadline::after(Duration::from_secs(10)),
        )
    });
    let host = thread::spawn(move || {
        let mut wire = host_wire;
        NoiseHandshake::run(
            &mut wire,
            HandshakeRole::Responder,
            &psk,
            &HandshakeDeadline::after(Duration::from_secs(10)),
        )
    });
    let client_st = must(must(client.join()));
    let host_st = must(must(host.join()));
    (client_st, host_st, client_wire_clone, host_wire_clone)
}

/// PSK 派生一次共享（argon2 m=19MiB ~ms 级，不进基准计时）。
fn bench_psk() -> [u8; 32] {
    static PSK: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *PSK.get_or_init(|| must(xcontrol_net::derive_psk("bench-code", &[0u8; 16])))
}

fn bench_loopback_roundtrip(c: &mut Criterion) {
    let psk = bench_psk();
    let (mut client_st, mut host_st, mut wire_a, mut wire_b) = bench_pair(&psk);

    let key_frame = must(
        InputEvent::Key {
            code: xkey::KEY_A,
            state: KeyState::Down,
        }
        .encode_frame(1),
    );
    let text_frame = must(
        InputEvent::Text {
            utf8: "x".repeat(512),
        }
        .encode_frame(2),
    );

    let mut group = c.benchmark_group("loopback_roundtrip");
    for (name, frame) in [("key_14b", &key_frame), ("text_519b", &text_frame)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut ciphertext = Vec::with_capacity(frame.len() + 16);
                must(client_st.encrypt_into(black_box(frame), &mut ciphertext));
                black_box(&ciphertext);
                must(wire_a.send(&ciphertext));
                let mut rx = Vec::new();
                must(wire_b.recv_timeout(Duration::from_secs(1), &mut rx));
                let mut plaintext = Vec::with_capacity(frame.len());
                must(host_st.decrypt_into(black_box(&rx), &mut plaintext));
                black_box(plaintext);
            })
        });
    }
    group.finish();
}

fn bench_crypto_only(c: &mut Criterion) {
    let psk = bench_psk();
    let frame = must(
        InputEvent::Key {
            code: xkey::KEY_A,
            state: KeyState::Down,
        }
        .encode_frame(1),
    );

    let mut group = c.benchmark_group("crypto_only");
    {
        // 独立链路：encrypt-only 推进 client 计数器，不得混用解密侧的期望 nonce
        // （snow 重放防护按序校验，见下方 encrypt_decrypt 组）。
        let (mut client_st, _host_st, _wire_a, _wire_b) = bench_pair(&psk);
        group.bench_function("encrypt_key_14b", |b| {
            b.iter(|| {
                let mut out = Vec::with_capacity(frame.len() + 16);
                must(client_st.encrypt_into(black_box(&frame), &mut out));
                black_box(out);
            })
        });
    }
    {
        // 密文计数器约束（snow 重放防护）：解密必须紧跟加密 nonce 序列——
        // 本基准为同迭代 encrypt→decrypt 对（隔离传输层后的纯加密栈往返）。
        let (mut client_st, mut host_st, _wire_a, _wire_b) = bench_pair(&psk);
        group.bench_function("encrypt_decrypt_key_14b", |b| {
            b.iter(|| {
                let mut ciphertext = Vec::with_capacity(frame.len() + 16);
                must(client_st.encrypt_into(black_box(&frame), &mut ciphertext));
                let mut plaintext = Vec::with_capacity(frame.len());
                must(host_st.decrypt_into(black_box(&ciphertext), &mut plaintext));
                black_box(plaintext);
            })
        });
    }
    group.finish();
}

fn bench_handshake(c: &mut Criterion) {
    let psk = bench_psk();
    let mut group = c.benchmark_group("handshake");
    group.bench_function("xxpsk0_full", |b| {
        b.iter(|| {
            let (client_wire, host_wire, _control) = LoopbackTransport::pair(None);
            let client = thread::spawn(move || {
                let mut wire = client_wire;
                NoiseHandshake::run(
                    &mut wire,
                    HandshakeRole::Initiator,
                    &psk,
                    &HandshakeDeadline::after(Duration::from_secs(10)),
                )
            });
            let host = thread::spawn(move || {
                let mut wire = host_wire;
                NoiseHandshake::run(
                    &mut wire,
                    HandshakeRole::Responder,
                    &psk,
                    &HandshakeDeadline::after(Duration::from_secs(10)),
                )
            });
            black_box(must(must(client.join())));
            black_box(must(must(host.join())));
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_loopback_roundtrip,
    bench_crypto_only,
    bench_handshake
);
criterion_main!(benches);
