//! fuzz target：帧解码全路径（docs/02 §8 恶意流）。
//!
//! 覆盖必跑路径：超长 len（越界）、截断帧头、垃圾 kind、未知 kind 跳过。
//! 断言目标：违规输入**报错/跳过而非 panic**（协议违规 → 调用方断开）。
#![no_main]

use libfuzzer_sys::fuzz_target;
use xcontrol_proto::{decode_ctrl, decode_clip, decode_frame, InputEvent};

fuzz_target!(|data: &[u8]| {
    // 逐帧解码直到耗尽或首个违规；任意字节流不得 panic。
    let mut rest = data;
    while !rest.is_empty() {
        match decode_frame(rest) {
            Ok(frame) => {
                rest = &rest[frame.total_len()..];
                // 已知 kind 的 payload 必须走各自的强校验路径（值域/布局/postcard）
                let _ = InputEvent::decode(frame.header.kind, frame.payload);
                let _ = decode_ctrl(frame.header.kind, frame.payload);
                let _ = decode_clip(frame.header.kind, frame.payload);
            }
            Err(_violation) => break, // 截断/越界：协议违规，调用方断开
        }
    }
});
