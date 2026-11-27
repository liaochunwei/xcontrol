//! 帧流与恶意输入负例（docs/02 §8：恶意流负例不 panic、未知 kind 跳过、版本协商矩阵
//! 的多帧流语义）。
//!
//! fuzz（cargo-fuzz，`fuzz/`）覆盖更深的随机路径；本文件用**确定性**字节流做回归。

use xcontrol_proto::xkey::KEY_A;
use xcontrol_proto::{
    Caps, CtrlMsg, InputEvent, KeyState, KindClass, Message, Options, ProtoError, ProtoVer,
    decode_clip, decode_ctrl, decode_frame, encode_frame, frame, kind_class,
};

/// 构造一条完整帧并拼接到流。
fn push_event(stream: &mut Vec<u8>, seq: u32, event: &InputEvent) {
    let _ = event.encode_frame_into(seq, stream);
}

/// 多帧流解码循环：输入帧 + 控制帧 + 剪贴板帧 + 未知 kind 帧混排，
/// 全部正确分派；未知 kind 按 len 跳过（docs/02 §4.1/§5.3 前向兼容）。
#[test]
fn mixed_stream_dispatch_and_skip() {
    let mut stream = Vec::new();
    push_event(
        &mut stream,
        1,
        &InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        },
    );
    // 未知 kind（未来 minor 才有的 0x61——文件域止于 0x60）：len=4
    stream.extend_from_slice(&encode_frame(2, 0x61, &[1, 2, 3, 4]).unwrap());
    let ctrl = CtrlMsg::SetOptions {
        options: Options::defaults(),
    };
    ctrl.encode_frame_into(3, &mut stream).unwrap();
    let clip = xcontrol_proto::ClipMsg::Ack(xcontrol_proto::ClipAck { seq: 9 });
    clip.as_message().encode_frame_into(4, &mut stream).unwrap();
    push_event(
        &mut stream,
        5,
        &InputEvent::MouseMoveRel { dx: 1.0, dy: 2.0 },
    );

    let mut rest: &[u8] = &stream;
    let mut seen = Vec::new();
    while !rest.is_empty() {
        let frame = decode_frame(rest).unwrap();
        rest = &rest[frame.total_len()..];
        seen.push(frame.header.kind);
        match frame.header.kind {
            frame::kind::KEY | frame::kind::MOUSE_MOVE_REL => {
                let event = InputEvent::decode(frame.header.kind, frame.payload)
                    .unwrap()
                    .expect("输入 kind 必须可解");
                assert_eq!(event.kind(), frame.header.kind);
            }
            frame::kind::CTRL_SET_OPTIONS => {
                let msg = decode_ctrl(frame.header.kind, frame.payload)
                    .unwrap()
                    .expect("控制 kind 必须可解");
                assert_eq!(msg, ctrl);
            }
            frame::kind::CLIP_ACK => {
                let msg = decode_clip(frame.header.kind, frame.payload)
                    .unwrap()
                    .expect("剪贴板 kind 必须可解");
                assert!(matches!(msg, xcontrol_proto::ClipMsg::Ack(_)));
                assert_eq!(msg.as_message().kind(), frame::kind::CLIP_ACK);
            }
            0x61 => {
                assert_eq!(frame.payload, &[1, 2, 3, 4]); // 未知 kind 保留 payload 供上层自行跳过
                assert_eq!(kind_class(0x61), KindClass::Unknown);
            }
            other => panic!("意外 kind 0x{other:02X}"),
        }
    }
    assert_eq!(
        seen,
        vec![
            frame::kind::KEY,
            0x61,
            frame::kind::CTRL_SET_OPTIONS,
            frame::kind::CLIP_ACK,
            frame::kind::MOUSE_MOVE_REL
        ]
    );
    assert!(rest.is_empty());
}

/// 恶意流负例：截断 / len 越界——必须报错且错误值精确（docs/02 §4.1）。
#[test]
fn malicious_frames_error_precisely() {
    // 截断帧头
    let err = decode_frame(&[0x01, 0x02, 0x03]).unwrap_err();
    assert_eq!(err, ProtoError::FrameTruncated { needed: 7, got: 3 });
    // len 越界：声明 65535 但只有 2 字节 payload
    let mut evil = Vec::new();
    evil.extend_from_slice(&7u32.to_le_bytes());
    evil.push(frame::kind::KEY);
    evil.extend_from_slice(&u16::MAX.to_le_bytes());
    evil.extend_from_slice(&[0xAA, 0xBB]);
    let err = decode_frame(&evil).unwrap_err();
    assert_eq!(
        err,
        ProtoError::PayloadOutOfBounds {
            declared: u16::MAX,
            available: 2
        }
    );
    // kind 声明为控制消息但 payload 是垃圾 → postcard 报错（不是 panic）
    let mut evil = Vec::new();
    evil.extend_from_slice(&1u32.to_le_bytes());
    evil.push(frame::kind::CTRL_HELLO);
    evil.extend_from_slice(&4u16.to_le_bytes());
    evil.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    let mut rest: &[u8] = &evil;
    let frame = decode_frame(rest).unwrap();
    rest = &rest[frame.total_len()..];
    let err = decode_ctrl(frame.header.kind, frame.payload).unwrap_err();
    assert!(matches!(err, ProtoError::Postcard(_)));
    assert!(rest.is_empty());
}

/// xorshift64 确定性伪随机流：解码器对任意字节流必须报错/跳过而非 panic
/// （fuzz 的确定性回归版，docs/02 §8 恶意流要求）。
#[test]
fn pseudorandom_stream_never_panics() {
    let mut state = 0x9E37_7997_97F6_7C1Bu64;
    let mut next_byte = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 24) as u8
    };
    for round in 0..64u8 {
        let mut stream = Vec::new();
        if round % 2 == 0 {
            // 一半轮次前置一条合法帧，检验解析器在合法/垃圾边界的行为
            push_event(
                &mut stream,
                1,
                &InputEvent::Key {
                    code: KEY_A,
                    state: KeyState::Up,
                },
            );
        }
        for _ in 0..96 {
            stream.push(next_byte());
        }
        let mut rest: &[u8] = &stream;
        while !rest.is_empty() {
            match decode_frame(rest) {
                Ok(frame) => {
                    rest = &rest[frame.total_len()..];
                    let _ = InputEvent::decode(frame.header.kind, frame.payload);
                    let _ = decode_ctrl(frame.header.kind, frame.payload);
                    let _ = decode_clip(frame.header.kind, frame.payload);
                    let _ = xcontrol_proto::decode_file(frame.header.kind, frame.payload);
                }
                Err(_) => break, // 截断/越界：调用方断开（协议违规）
            }
        }
    }
}

/// 协议版本协商矩阵在「连接」语义下的使用（docs/02 §8：1.0↔1.1↔2.0）：
/// Hello/HelloBack 携带各自版本 → 协商 → 会话 minor 决定 HelloBack 的 wire 版本值。
#[test]
fn version_handshake_matrix() {
    let local = ProtoVer::CURRENT; // 1.2
    let peers = [
        (ProtoVer { major: 1, minor: 0 }, true),
        (ProtoVer { major: 1, minor: 1 }, true),
        (ProtoVer { major: 1, minor: 2 }, true),
        (ProtoVer { major: 2, minor: 0 }, false),
    ];
    for (peer, compatible) in peers {
        let hello = CtrlMsg::Hello {
            proto: peer,
            name: String::new(),
            screen_name: String::new(),
            instance_id: [0; 16],
            caps: Caps::UNICODE,
            salt: [0; 16],
        };
        let bytes = hello.encode_frame(0).unwrap();
        let frame = decode_frame(&bytes).unwrap();
        let decoded = decode_ctrl(frame.header.kind, frame.payload)
            .unwrap()
            .unwrap();
        let peer_ver = match decoded {
            CtrlMsg::Hello { proto, .. } => proto,
            other => panic!("期望 Hello，得到 {other:?}"),
        };
        let negotiated = xcontrol_proto::negotiate(local, peer_ver);
        assert_eq!(negotiated.is_ok(), compatible, "peer {peer_ver:?}");
        if compatible {
            let session = negotiated.unwrap();
            let hello_back = CtrlMsg::HelloBack {
                proto: session,
                name: "client".to_owned(),
                instance_id: [0; 16],
                caps: Caps::UNICODE,
            };
            // 会话 minor 之下的消息集才允许发送——HelloBack 声明的就是协商后版本
            let bytes = hello_back.encode_frame(1).unwrap();
            let frame = decode_frame(&bytes).unwrap();
            let decoded = decode_ctrl(frame.header.kind, frame.payload)
                .unwrap()
                .unwrap();
            assert_eq!(
                decoded,
                CtrlMsg::HelloBack {
                    proto: session,
                    name: "client".to_owned(),
                    instance_id: [0; 16],
                    caps: Caps::UNICODE
                }
            );
        } else {
            // major 不等 → Error{UnsupportedVersion} 后断开（docs/02 §5.2）
            let error = CtrlMsg::Error {
                code: xcontrol_proto::ErrCode::UNSUPPORTED_VERSION,
                msg: "unsupported major".to_owned(),
            };
            let bytes = error.encode_frame(2).unwrap();
            let frame = decode_frame(&bytes).unwrap();
            let decoded = decode_ctrl(frame.header.kind, frame.payload)
                .unwrap()
                .unwrap();
            match decoded {
                CtrlMsg::Error { code, .. } => {
                    assert_eq!(code, xcontrol_proto::ErrCode::UNSUPPORTED_VERSION);
                }
                other => panic!("期望 Error，得到 {other:?}"),
            }
            let err = xcontrol_proto::negotiate(local, peer_ver).unwrap_err();
            assert!(matches!(err, ProtoError::VersionIncompatible { .. }));
        }
    }
}
