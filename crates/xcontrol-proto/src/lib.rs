//! `xcontrol-proto` — XCP 协议层：帧编解码与全消息定义（docs/02 §4–§7）。
//!
//! 分层红线（docs/08 §1、`.trellis/spec/rust-core/architecture-rules.md`）：
//!
//! - 仅允许序列化类依赖（postcard/serde，docs/08 §2）；
//! - 禁止平台 API、文件/网络 I/O、tokio 运行时——本 crate 不做任何 I/O，
//!   编解码全部在内存字节切片上完成；
//! - `#![forbid(unsafe_code)]`；非测试代码零 `unwrap()`/`expect()`；
//! - 必须可在普通 `cargo test` 中全量运行（CI 三平台矩阵保证）。
//!
//! # 模块导览
//!
//! | 模块 | 内容 | wire 契约 |
//! | --- | --- | --- |
//! | [`frame`] | 7 字节定长帧头 + [`kind`] 表 + 流式解码辅助 | docs/02 §4.1 |
//! | [`input`] | 12 种输入事件（**定长手写**小端）+ [`RelMerger`] 合帧辅助 | docs/02 §4.2 |
//! | [`ctrl`] | [`CtrlMsg`] 控制消息（postcard）+ Caps/ScreenInfo/Options/ErrCode | docs/02 §4.3 |
//! | [`clip`] | 剪贴板通道五消息（postcard）+ 接收端限额校验 | docs/02 §4.4/§6 |
//! | [`version`] | [`negotiate`] 版本协商（major 不等即 Err、minor 取 min） | docs/02 §5 |
//! | [`xkey`] | [`XKey`] 抽象键码 + HID 值域常量表（保留区拒绝） | docs/03 §2 |
//!
//! 协议变更流程：先改 docs/02 → minor+1 → 更新 golden fixtures → 改代码
//! （`.trellis/spec/rust-core/protocol-rules.md`）。
//!
//! # 接收端分派模式（docs/02 §4.1/§5.3）
//!
//! ```rust
//! use xcontrol_proto::{decode_frame, InputEvent, KeyState, KindClass, Message};
//!
//! // stream: Noise 解密后的明文流切片（此处用一帧 Key 事件演示）
//! let mut stream = Vec::new();
//! let key = InputEvent::Key {
//!     code: xcontrol_proto::xkey::KEY_A,
//!     state: KeyState::Down,
//! };
//! key.encode_frame_into(1, &mut stream).unwrap();
//! // 混入一个未知 kind 帧（未来 minor 新增的消息）——接收方按 len 跳过
//! stream.extend_from_slice(&xcontrol_proto::encode_frame(2, 0x60, &[1, 2, 3]).unwrap());
//!
//! let mut rest = &stream[..];
//! while !rest.is_empty() {
//!     let frame = decode_frame(rest).unwrap();          // len 越界在此报错（断开语义）
//!     rest = &rest[frame.total_len()..];
//!     match xcontrol_proto::kind_class(frame.header.kind) {
//!         KindClass::Input => {
//!             let event = InputEvent::decode(frame.header.kind, frame.payload).unwrap();
//!             assert!(matches!(event, Some(InputEvent::Key { .. })));
//!         }
//!         KindClass::Ctrl => {
//!             let _ = xcontrol_proto::decode_ctrl(frame.header.kind, frame.payload);
//!         }
//!         KindClass::Clip => {
//!             let _ = xcontrol_proto::decode_clip(frame.header.kind, frame.payload);
//!         }
//!         KindClass::Unknown => {}                      // 未知 kind：按 len 跳过（前向兼容）
//!     }
//! }
//! ```
//!
//! 协议常量锚点：XCP-VERSION = `major 1 / minor 0`（[`version::PROTO_MAJOR`] /
//! [`version::PROTO_MINOR`]）。
#![forbid(unsafe_code)]

pub mod clip;
pub mod ctrl;
pub mod error;
pub mod frame;
pub mod input;
pub mod version;
pub mod xkey;

pub use clip::{
    ChannelBind, ClipAck, ClipAnnounce, ClipChunk, ClipEnd, ClipFormat, ClipMsg, decode_clip,
};
pub use ctrl::{Caps, CtrlMsg, ErrCode, Options, ScreenInfo, decode_ctrl};
pub use error::ProtoError;
pub use frame::{
    FRAME_HEADER_LEN, Frame, FrameHeader, KindClass, MAX_FRAME_PAYLOAD, decode_frame, encode_frame,
    encode_frame_into, is_clip_kind, is_ctrl_kind, is_input_kind, kind, kind_class,
};
pub use input::{
    Axis, InputEvent, KeyState, MAX_TEXT_LEN, Mods, MouseButton, RelMerger, ScreenEdge, ScreenId,
};
pub use version::{PROTO_MAJOR, PROTO_MINOR, ProtoVer, negotiate};

/// 可上线的协议消息公共接口：知道自己的帧 [`Message::kind`] 与 payload 编码。
///
/// 解码不入 trait——各通道签名不同（输入事件按 kind 返回 `Option`、
/// postcard 消息走 [`decode_ctrl`]/[`decode_clip`] 且带接收端校验）。
pub trait Message {
    /// docs/02 §4.2/§4.3/§4.4 kind 表中的帧 kind。
    fn kind(&self) -> u8;

    /// payload 编码（不含帧头），追加到 `out`。
    ///
    /// # Errors
    /// 编码约束违规（如 `Text` 超过 512B）。
    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError>;

    /// 完整帧编码（帧头 + payload），追加到 `out`。
    ///
    /// # Errors
    /// payload 编码失败，或 payload 超过帧头 `len: u16` 上限。
    fn encode_frame_into(&self, seq: u32, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        let mut payload = Vec::new();
        self.encode_payload(&mut payload)?;
        frame::encode_frame_into(seq, self.kind(), &payload, out)
    }

    /// 完整帧编码到独立缓冲。
    ///
    /// # Errors
    /// 同 [`Message::encode_frame_into`]。
    fn encode_frame(&self, seq: u32) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::new();
        self.encode_frame_into(seq, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xkey::{CK_VOLUME_UP, KEY_A, KEY_LEFT_META};

    /// crate 级锚点：version / frame 常量经 lib re-export 可达。
    #[test]
    fn anchors() {
        assert_eq!(PROTO_MAJOR, 1);
        assert_eq!(ProtoVer::CURRENT.minor, PROTO_MINOR);
        assert_eq!(FRAME_HEADER_LEN, 7);
        assert_eq!(MAX_TEXT_LEN, 512);
        assert_eq!(kind::KEY, 0x01);
        // 值域哨兵：消费者页基址偏移（docs/03 §2）
        assert!(CK_VOLUME_UP.raw() >= 0x100);
        assert!(KEY_LEFT_META.is_modifier());
    }

    /// Message trait 对三类消息统一可用（encode_frame → decode_frame）。
    #[test]
    fn message_trait_covers_all_classes() {
        let input = InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        };
        let ctrl = CtrlMsg::Ping { ts_ms: 7 };
        let clip = ChannelBind {
            session_id: [0; 16],
        };
        let messages: [&dyn Message; 3] = [&input, &ctrl, &clip];
        for (i, message) in messages.iter().enumerate() {
            let bytes = message.encode_frame(i as u32).unwrap();
            let frame = decode_frame(&bytes).unwrap();
            assert_eq!(frame.header.kind, message.kind());
            assert_eq!(frame.header.seq, i as u32);
        }
    }

    /// Caps/Mods 位运算经 re-export 可用。
    #[test]
    fn reexports_usable() {
        let caps = Caps::UNICODE | Caps::IMAGE;
        assert!(caps.contains(Caps::UNICODE));
        assert!(!caps.contains(Caps::HTML));
        let mods = Mods::LCTRL | Mods::LMETA;
        assert_eq!(mods.bits(), 0b1001);
    }
}
