//! 编解码吞吐基准（docs/08 §4 性能层：criterion）。
//!
//! 运行：
//! ```text
//! cargo bench -p xcontrol-proto
//! # 快速冒烟（调小测量时长；必须 --bench codec 定向 criterion 目标，
//! 否则参数会转发给 lib 目标的 libtest 而报 Unrecognized option）：
//! cargo bench -p xcontrol-proto --bench codec -- --measurement-time 1 --warm-up-time 0.3 --sample-size 10
//! ```
//!
//! 分组：
//! - `input_encode` / `input_decode`：热路径定长手写编解码（Key / MouseMoveRel / Text / Wheel）；
//! - `ctrl_roundtrip`：postcard 控制消息（Hello 编解码，低频路径）；
//! - `frame_decode`：帧头解析 + 分派（混排流，模拟一次发送窗口）。

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use xcontrol_proto::xkey::{CK_VOLUME_UP, KEY_A};
use xcontrol_proto::{
    Axis, Caps, CtrlMsg, InputEvent, KeyState, Message, Options, ProtoError, ProtoVer, decode_ctrl,
    decode_frame, kind,
};

/// bench 内取值：编码/解码失败即 panic（bench 闭包不走 Result 链，保持基准体简洁）。
#[track_caller]
fn must<T>(result: Result<T, ProtoError>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("bench 编解码失败：{error}"),
    }
}

fn bench_input_encode(c: &mut Criterion) {
    let key = InputEvent::Key {
        code: KEY_A,
        state: KeyState::Down,
    };
    let rel = InputEvent::MouseMoveRel { dx: 1.25, dy: -2.5 };
    let text = InputEvent::Text {
        utf8: "héllo unicode 文本".repeat(8),
    };
    let mut group = c.benchmark_group("input_encode");
    group.throughput(criterion::Throughput::Bytes(
        must(key.encode_frame(0)).len() as u64,
    ));
    group.bench_function("key", |b| b.iter(|| must(key.encode_frame(1))));
    group.bench_function("mouse_move_rel", |b| b.iter(|| must(rel.encode_frame(1))));
    group.bench_function("text_128b", |b| b.iter(|| must(text.encode_frame(1))));
    group.finish();
}

fn bench_input_decode(c: &mut Criterion) {
    let key_frame = must(
        InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        }
        .encode_frame(1),
    );
    let rel_frame = must(InputEvent::MouseMoveRel { dx: 0.5, dy: 0.5 }.encode_frame(1));
    let text_frame = must(
        InputEvent::Text {
            utf8: "x".repeat(128),
        }
        .encode_frame(1),
    );
    let wheel_frame = must(
        InputEvent::Wheel {
            axis: Axis::Vertical,
            delta: -1.0,
        }
        .encode_frame(1),
    );
    let _ = CK_VOLUME_UP; // 消费者页常量哨兵（docs/03 §2 值域覆盖）
    let decode_bench =
        |group: &mut criterion::BenchmarkGroup<'_, _>, name: &'static str, bytes: &Vec<u8>| {
            group.bench_function(name, |b| {
                b.iter(|| {
                    let frame = match decode_frame(black_box(bytes)) {
                        Ok(frame) => frame,
                        Err(error) => panic!("{error}"),
                    };
                    let event = must(InputEvent::decode(frame.header.kind, frame.payload));
                    black_box(event);
                })
            });
        };
    let mut group = c.benchmark_group("input_decode");
    decode_bench(&mut group, "key", &key_frame);
    decode_bench(&mut group, "mouse_move_rel", &rel_frame);
    decode_bench(&mut group, "text_128b", &text_frame);
    decode_bench(&mut group, "wheel", &wheel_frame);
    group.finish();
}

fn bench_ctrl_roundtrip(c: &mut Criterion) {
    let hello = CtrlMsg::Hello {
        proto: ProtoVer::CURRENT,
        name: "bench-host".to_owned(),
        screen_name: "primary".to_owned(),
        instance_id: [0xAB; 16],
        caps: Caps::UNICODE | Caps::IMAGE | Caps::HTML | Caps::ABS_POINTER,
        salt: [0x5A; 16],
    };
    let frame_bytes = must(hello.encode_frame(1));
    let set_options = CtrlMsg::SetOptions {
        options: Options::defaults(),
    };
    let mut group = c.benchmark_group("ctrl_roundtrip");
    group.bench_function("hello_encode", |b| b.iter(|| must(hello.encode_frame(1))));
    group.bench_function("hello_decode", |b| {
        b.iter(|| {
            let frame = match decode_frame(black_box(&frame_bytes)) {
                Ok(frame) => frame,
                Err(error) => panic!("{error}"),
            };
            let msg = must(decode_ctrl(frame.header.kind, frame.payload));
            black_box(msg);
        })
    });
    group.bench_function("set_options_encode", |b| {
        b.iter(|| must(set_options.encode_frame(1)))
    });
    group.finish();
}

fn bench_frame_decode(c: &mut Criterion) {
    // 混排流：输入帧 ×8 + 控制帧 + 剪贴板帧，模拟一次发送窗口（合帧 ≤8 条）
    let mut stream = Vec::new();
    for i in 0..8u32 {
        InputEvent::MouseMoveRel { dx: 0.1, dy: 0.1 }
            .encode_frame_into(i, &mut stream)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    CtrlMsg::Ping { ts_ms: 42 }
        .encode_frame_into(8, &mut stream)
        .unwrap_or_else(|error| panic!("{error}"));
    xcontrol_proto::ClipAck { seq: 1 }
        .encode_frame_into(9, &mut stream)
        .unwrap_or_else(|error| panic!("{error}"));
    let mut group = c.benchmark_group("frame_decode");
    group.throughput(criterion::Throughput::Bytes(stream.len() as u64));
    group.bench_function("window_10_frames", |b| {
        b.iter(|| {
            let mut rest: &[u8] = black_box(&stream);
            let mut count = 0usize;
            while !rest.is_empty() {
                let frame = match decode_frame(rest) {
                    Ok(frame) => frame,
                    Err(error) => panic!("{error}"),
                };
                rest = &rest[frame.total_len()..];
                if frame.header.kind == kind::MOUSE_MOVE_REL {
                    let _ = InputEvent::decode(frame.header.kind, frame.payload);
                }
                count += 1;
            }
            count
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_input_encode,
    bench_input_decode,
    bench_ctrl_roundtrip,
    bench_frame_decode,
);
criterion_main!(benches);
