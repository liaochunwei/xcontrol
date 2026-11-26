//! 帧层（docs/02 §4.1）：Noise 传输态密文内的应用层帧封装。
//!
//! ```text
//! 帧头 7 字节定长（全部小端）：seq: u32 | kind: u8 | len: u16 | payload: [u8; len]
//! ```
//!
//! 规则（docs/02 §4.1/§5.3）：
//!
//! - **未知 kind 按 len 跳过**（前向兼容）；接收方拿到 `(kind, payload)` 自行分派；
//! - **len 越界必须报错**（[`decode_frame`] 对声明的 payload 超出可用字节即返回
//!   [`ProtoError::PayloadOutOfBounds`]，调用方按协议违规断开，docs/02 §7 code 0x04）；
//! - 接收方按 TCP 顺序处理，不做重排（seq 仅用于诊断/延迟统计）。

use crate::error::ProtoError;

/// 帧头定长（`seq u32 + kind u8 + len u16` = 7 字节，docs/02 §4.1）。
pub const FRAME_HEADER_LEN: usize = 7;

/// payload 语义上限：64KiB（与 `ClipChunk` 单块上限对齐，docs/02 §6）。
/// 帧头 `len: u16` 的物理上限为 65535（≤ 本常量），编码侧以 u16 上限校验。
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

/// docs/02 §4.2/§4.3/§4.4/§4.6 kind 表（**1:1 对应，改表先改 docs/02**）。
///
/// kind 分段：输入事件 `0x01–0x21`（定长手写）；控制消息 `0x40–0x48`（postcard）；
/// 剪贴板通道 `0x50–0x54`（postcard）；文件通道 `0x58–0x60`（postcard，XCP 1.2+，
/// 仅出现在文件连接——docs/02 §4.6 域隔离）。每条消息独立占用一个 kind——新消息
/// 随 minor 提升新增 kind，老对端才能靠"未知 kind 按 len 跳过"无损降级（docs/02 §5.3）。
pub mod kind {
    // —— 输入事件（定长手写，docs/02 §4.2）——
    /// `Key`：按键按下/抬起。
    pub const KEY: u8 = 0x01;
    /// `KeyRepeat`：长按重复。
    pub const KEY_REPEAT: u8 = 0x02;
    /// `MouseMoveRel`：相对指针移动。
    pub const MOUSE_MOVE_REL: u8 = 0x03;
    /// `MouseMoveAbs`：绝对指针移动。
    pub const MOUSE_MOVE_ABS: u8 = 0x04;
    /// `MouseButton`：鼠标按键。
    pub const MOUSE_BUTTON: u8 = 0x05;
    /// `Wheel`：滚轮。
    pub const WHEEL: u8 = 0x06;
    /// `Text`：Unicode 文本直发。
    pub const TEXT: u8 = 0x07;
    /// `EnterScreen`：切入从机屏幕。
    pub const ENTER_SCREEN: u8 = 0x10;
    /// `LeaveScreen`：切出屏幕。
    pub const LEAVE_SCREEN: u8 = 0x11;
    /// `ModifierSync`：修饰键快照对齐。
    pub const MODIFIER_SYNC: u8 = 0x12;
    /// `SwitchRequest`：热键/托盘切换请求。
    pub const SWITCH_REQUEST: u8 = 0x20;
    /// `SwitchAck`：切换确认。
    pub const SWITCH_ACK: u8 = 0x21;

    // —— 控制消息（postcard，docs/02 §4.3）——
    /// `CtrlMsg::Hello`（主控→从机第一条）。
    pub const CTRL_HELLO: u8 = 0x40;
    /// `CtrlMsg::HelloBack`。
    pub const CTRL_HELLO_BACK: u8 = 0x41;
    /// `CtrlMsg::Layout`。
    pub const CTRL_LAYOUT: u8 = 0x42;
    /// `CtrlMsg::SetOptions`。
    pub const CTRL_SET_OPTIONS: u8 = 0x43;
    /// `CtrlMsg::Error`（随后断开）。
    pub const CTRL_ERROR: u8 = 0x44;
    /// `CtrlMsg::Ping`。
    pub const CTRL_PING: u8 = 0x45;
    /// `CtrlMsg::Pong`。
    pub const CTRL_PONG: u8 = 0x46;
    /// `CtrlMsg::Pause`。
    pub const CTRL_PAUSE: u8 = 0x47;
    /// `CtrlMsg::Resume`。
    pub const CTRL_RESUME: u8 = 0x48;

    // —— 剪贴板通道（postcard，docs/02 §4.4/§6）——
    /// `ChannelBind`：剪贴板连接首帧。
    pub const CLIP_CHANNEL_BIND: u8 = 0x50;
    /// `ClipAnnounce`。
    pub const CLIP_ANNOUNCE: u8 = 0x51;
    /// `ClipChunk`。
    pub const CLIP_CHUNK: u8 = 0x52;
    /// `ClipAck`。
    pub const CLIP_ACK: u8 = 0x53;
    /// `ClipEnd`。
    pub const CLIP_END: u8 = 0x54;

    // —— 文件通道（postcard，docs/02 §4.6，XCP 1.2+）——
    /// `FileBind`：文件连接首帧。
    pub const FILE_BIND: u8 = 0x58;
    /// `FileRefAnnounce`：文件引用元数据清单（FR-7.3）。
    pub const FILE_REF_ANNOUNCE: u8 = 0x59;
    /// `FilePullReq`：条目索引拉取请求。
    pub const FILE_PULL_REQ: u8 = 0x5A;
    /// `FileChunk`：文件数据块。
    pub const FILE_CHUNK: u8 = 0x5B;
    /// `FileAck`：信用归还。
    pub const FILE_ACK: u8 = 0x5C;
    /// `FileEnd`：条目传输结束（blake3 校验）。
    pub const FILE_END: u8 = 0x5D;
    /// `FileError`：条目级/整单失败。
    pub const FILE_ERROR: u8 = 0x5E;
    /// `FileCancel`：用户取消。
    pub const FILE_CANCEL: u8 = 0x5F;
    /// `FileExpand`：目录子树展开。
    pub const FILE_EXPAND: u8 = 0x60;
}

/// kind 大类（帧解出后的分派依据；未知 kind 归入 [`KindClass::Unknown`]，
/// 调用方按 docs/02 §4.1 跳过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum KindClass {
    /// 输入事件（定长手写，docs/02 §4.2）。
    Input = 0,
    /// 控制消息（postcard，docs/02 §4.3）。
    Ctrl = 1,
    /// 剪贴板通道消息（postcard，docs/02 §4.4/§6）。
    Clip = 2,
    /// 文件通道消息（postcard，docs/02 §4.6，XCP 1.2+）。
    File = 3,
    /// 未知 kind（按 len 跳过）。
    Unknown = 4,
}

/// kind → 大类（与 [`kind`] 表一致）。
#[must_use]
pub const fn kind_class(kind: u8) -> KindClass {
    match kind {
        kind::KEY..=kind::TEXT
        | kind::ENTER_SCREEN..=kind::MODIFIER_SYNC
        | kind::SWITCH_REQUEST..=kind::SWITCH_ACK => KindClass::Input,
        kind::CTRL_HELLO..=kind::CTRL_RESUME => KindClass::Ctrl,
        kind::CLIP_CHANNEL_BIND..=kind::CLIP_END => KindClass::Clip,
        kind::FILE_BIND..=kind::FILE_EXPAND => KindClass::File,
        _ => KindClass::Unknown,
    }
}

/// kind 是否属于输入事件表（docs/02 §4.2）。
#[must_use]
pub const fn is_input_kind(kind: u8) -> bool {
    kind_class(kind) as u8 == KindClass::Input as u8
}

/// kind 是否属于控制消息表（docs/02 §4.3）。
#[must_use]
pub const fn is_ctrl_kind(kind: u8) -> bool {
    kind_class(kind) as u8 == KindClass::Ctrl as u8
}

/// kind 是否属于剪贴板通道表（docs/02 §4.4）。
#[must_use]
pub const fn is_clip_kind(kind: u8) -> bool {
    kind_class(kind) as u8 == KindClass::Clip as u8
}

/// kind 是否属于文件通道表（docs/02 §4.6，XCP 1.2+）。
#[must_use]
pub const fn is_file_kind(kind: u8) -> bool {
    kind_class(kind) as u8 == KindClass::File as u8
}

/// 帧头（docs/02 §4.1）：`seq u32 LE | kind u8 | len u16 LE`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// 递增序号（诊断/延迟统计；接收方不做重排）。
    pub seq: u32,
    /// 消息 kind（[`kind`] 表）。
    pub kind: u8,
    /// payload 字节数。
    pub len: u16,
}

impl FrameHeader {
    /// 编码字节数（定长帧头）。
    pub const SIZE: usize = FRAME_HEADER_LEN;

    /// 构造帧头（`len` 为 payload 字节数）。
    #[must_use]
    pub const fn new(seq: u32, kind: u8, len: u16) -> Self {
        Self { seq, kind, len }
    }

    /// 小端编码，追加到 `out`。
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.push(self.kind);
        out.extend_from_slice(&self.len.to_le_bytes());
    }

    /// 小端编码为独立缓冲。
    #[must_use]
    pub fn encode_to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        self.encode_into(&mut out);
        out
    }

    /// 从缓冲解码帧头；返回 `(帧头, 消费字节数=7)`。
    ///
    /// # Errors
    /// 缓冲不足 7 字节时返回 [`ProtoError::FrameTruncated`]。
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), ProtoError> {
        if buf.len() < Self::SIZE {
            return Err(ProtoError::FrameTruncated {
                needed: Self::SIZE,
                got: buf.len(),
            });
        }
        let mut seq = [0u8; 4];
        seq.copy_from_slice(&buf[0..4]);
        let mut len = [0u8; 2];
        len.copy_from_slice(&buf[5..7]);
        Ok((
            Self {
                seq: u32::from_le_bytes(seq),
                kind: buf[4],
                len: u16::from_le_bytes(len),
            },
            Self::SIZE,
        ))
    }
}

/// 解码出的单帧视图：payload 借用输入缓冲（零拷贝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    /// 帧头。
    pub header: FrameHeader,
    /// payload（`len` 字节；未知 kind 时调用方直接丢弃此切片即可完成"按 len 跳过"）。
    pub payload: &'a [u8],
}

impl Frame<'_> {
    /// 帧总字节数（帧头 + payload），流式解析时用于推进读指针。
    #[must_use]
    pub const fn total_len(&self) -> usize {
        FRAME_HEADER_LEN + self.header.len as usize
    }
}

/// 从缓冲解出一个完整帧。允许尾部携带后续帧字节（流式解析）；
/// payload 借用输入切片，总消费字节数为 [`Frame::total_len`]。
///
/// # Errors
/// - 帧头不足 7 字节 → [`ProtoError::FrameTruncated`]；
/// - **len 越界**（声明的 payload 超出缓冲内可用字节）→
///   [`ProtoError::PayloadOutOfBounds`]（docs/02 §4.1 必须报错，不得静默截断）。
pub fn decode_frame(buf: &[u8]) -> Result<Frame<'_>, ProtoError> {
    let (header, header_len) = FrameHeader::decode(buf)?;
    let declared = header.len as usize;
    let available = buf.len() - header_len;
    if declared > available {
        return Err(ProtoError::PayloadOutOfBounds {
            declared: header.len,
            available,
        });
    }
    Ok(Frame {
        header,
        payload: &buf[header_len..header_len + declared],
    })
}

/// 编码一帧（帧头 + payload），追加到 `out`。
///
/// # Errors
/// payload 超过 `u16::MAX`（帧头 `len` 物理上限）→ [`ProtoError::PayloadTooLarge`]。
pub fn encode_frame_into(
    seq: u32,
    kind: u8,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), ProtoError> {
    if payload.len() > u16::MAX as usize {
        return Err(ProtoError::PayloadTooLarge {
            len: payload.len(),
            max: u16::MAX as usize,
        });
    }
    FrameHeader::new(seq, kind, payload.len() as u16).encode_into(out);
    out.extend_from_slice(payload);
    Ok(())
}

/// 编码一帧到独立缓冲（[`encode_frame_into`] 的便捷形式）。
///
/// # Errors
/// 同 [`encode_frame_into`]。
pub fn encode_frame(seq: u32, kind: u8, payload: &[u8]) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    encode_frame_into(seq, kind, payload, &mut out)?;
    Ok(out)
}

/// postcard 载荷解码辅助：[`postcard::take_from_bytes`] + **严格消费全部字节**
/// （帧 payload 长度由帧头钉死，残留字节 = 布局不符，报错而非静默忽略）。
pub(crate) fn decode_postcard<T: serde::de::DeserializeOwned>(
    payload: &[u8],
) -> Result<T, ProtoError> {
    let (value, rest) =
        postcard::take_from_bytes(payload).map_err(ProtoError::from_postcard_decode)?;
    if !rest.is_empty() {
        return Err(ProtoError::Postcard(format!(
            "payload 有 {} 字节残留（严格解码要求恰好消费全部字节）",
            rest.len()
        )));
    }
    Ok(value)
}

/// postcard 载荷编码辅助：追加到 `out`（[`postcard::to_extend`] 以值进出，
/// 用 `mem::take` 归还以保留缓冲容量）。
pub(crate) fn encode_postcard_into<T: serde::Serialize + ?Sized>(
    value: &T,
    out: &mut Vec<u8>,
) -> Result<(), ProtoError> {
    let taken = std::mem::take(out);
    match postcard::to_extend(value, taken) {
        Ok(filled) => {
            *out = filled;
            Ok(())
        }
        Err(error) => Err(ProtoError::Postcard(format!("postcard 编码失败：{error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// kind 表与 docs/02 §4.2/§4.3/§4.4 逐字对应（golden 锚点）。
    #[test]
    fn kind_table_matches_docs() {
        // 输入事件（docs/02 §4.2）
        assert_eq!(kind::KEY, 0x01);
        assert_eq!(kind::KEY_REPEAT, 0x02);
        assert_eq!(kind::MOUSE_MOVE_REL, 0x03);
        assert_eq!(kind::MOUSE_MOVE_ABS, 0x04);
        assert_eq!(kind::MOUSE_BUTTON, 0x05);
        assert_eq!(kind::WHEEL, 0x06);
        assert_eq!(kind::TEXT, 0x07);
        assert_eq!(kind::ENTER_SCREEN, 0x10);
        assert_eq!(kind::LEAVE_SCREEN, 0x11);
        assert_eq!(kind::MODIFIER_SYNC, 0x12);
        assert_eq!(kind::SWITCH_REQUEST, 0x20);
        assert_eq!(kind::SWITCH_ACK, 0x21);
        // 控制消息（docs/02 §4.3）
        assert_eq!(kind::CTRL_HELLO, 0x40);
        assert_eq!(kind::CTRL_HELLO_BACK, 0x41);
        assert_eq!(kind::CTRL_LAYOUT, 0x42);
        assert_eq!(kind::CTRL_SET_OPTIONS, 0x43);
        assert_eq!(kind::CTRL_ERROR, 0x44);
        assert_eq!(kind::CTRL_PING, 0x45);
        assert_eq!(kind::CTRL_PONG, 0x46);
        assert_eq!(kind::CTRL_PAUSE, 0x47);
        assert_eq!(kind::CTRL_RESUME, 0x48);
        // 剪贴板通道（docs/02 §4.4/§6）
        assert_eq!(kind::CLIP_CHANNEL_BIND, 0x50);
        assert_eq!(kind::CLIP_ANNOUNCE, 0x51);
        assert_eq!(kind::CLIP_CHUNK, 0x52);
        assert_eq!(kind::CLIP_ACK, 0x53);
        assert_eq!(kind::CLIP_END, 0x54);
        // 文件通道（docs/02 §4.6，XCP 1.2+）
        assert_eq!(kind::FILE_BIND, 0x58);
        assert_eq!(kind::FILE_REF_ANNOUNCE, 0x59);
        assert_eq!(kind::FILE_PULL_REQ, 0x5A);
        assert_eq!(kind::FILE_CHUNK, 0x5B);
        assert_eq!(kind::FILE_ACK, 0x5C);
        assert_eq!(kind::FILE_END, 0x5D);
        assert_eq!(kind::FILE_ERROR, 0x5E);
        assert_eq!(kind::FILE_CANCEL, 0x5F);
        assert_eq!(kind::FILE_EXPAND, 0x60);
    }

    /// kind 大类划分覆盖每张表的全部 kind（含边界值）。
    #[test]
    fn kind_class_covers_tables() {
        for k in [
            kind::KEY,
            kind::KEY_REPEAT,
            kind::MOUSE_MOVE_REL,
            kind::MOUSE_MOVE_ABS,
            kind::MOUSE_BUTTON,
            kind::WHEEL,
            kind::TEXT,
            kind::ENTER_SCREEN,
            kind::LEAVE_SCREEN,
            kind::MODIFIER_SYNC,
            kind::SWITCH_REQUEST,
            kind::SWITCH_ACK,
        ] {
            assert_eq!(kind_class(k), KindClass::Input);
            assert!(is_input_kind(k));
            assert!(!is_ctrl_kind(k));
            assert!(!is_clip_kind(k));
        }
        for k in [
            kind::CTRL_HELLO,
            kind::CTRL_HELLO_BACK,
            kind::CTRL_LAYOUT,
            kind::CTRL_SET_OPTIONS,
            kind::CTRL_ERROR,
            kind::CTRL_PING,
            kind::CTRL_PONG,
            kind::CTRL_PAUSE,
            kind::CTRL_RESUME,
        ] {
            assert_eq!(kind_class(k), KindClass::Ctrl);
            assert!(is_ctrl_kind(k));
        }
        for k in [
            kind::CLIP_CHANNEL_BIND,
            kind::CLIP_ANNOUNCE,
            kind::CLIP_CHUNK,
            kind::CLIP_ACK,
            kind::CLIP_END,
        ] {
            assert_eq!(kind_class(k), KindClass::Clip);
            assert!(is_clip_kind(k));
            assert!(!is_file_kind(k));
        }
        for k in [
            kind::FILE_BIND,
            kind::FILE_REF_ANNOUNCE,
            kind::FILE_PULL_REQ,
            kind::FILE_CHUNK,
            kind::FILE_ACK,
            kind::FILE_END,
            kind::FILE_ERROR,
            kind::FILE_CANCEL,
            kind::FILE_EXPAND,
        ] {
            assert_eq!(kind_class(k), KindClass::File);
            assert!(is_file_kind(k));
            assert!(!is_clip_kind(k));
            assert!(!is_ctrl_kind(k));
        }
        for k in [
            0x00, 0x08, 0x0F, 0x13, 0x1F, 0x22, 0x30, 0x3F, 0x49, 0x4F, 0x55, 0x57, 0x61, 0xFF,
        ] {
            assert_eq!(kind_class(k), KindClass::Unknown);
            assert!(!is_input_kind(k) && !is_ctrl_kind(k) && !is_clip_kind(k) && !is_file_kind(k));
        }
    }

    /// 帧头小端逐字节 golden：`seq=0x01020304 kind=0x01 len=0x0304`。
    #[test]
    fn header_wire_golden() {
        let header = FrameHeader::new(0x0102_0304, 0x01, 0x0304);
        let bytes = header.encode_to_vec();
        assert_eq!(bytes, vec![0x04, 0x03, 0x02, 0x01, 0x01, 0x04, 0x03]);
        let (decoded, consumed) = FrameHeader::decode(&bytes).unwrap();
        assert_eq!(consumed, FRAME_HEADER_LEN);
        assert_eq!(decoded, header);
    }

    /// encode_frame_into 产物 = 帧头 + payload；decode_frame 还原。
    #[test]
    fn frame_roundtrip() {
        let payload = b"hello";
        let frame_bytes = encode_frame(7, kind::TEXT, payload).unwrap();
        assert_eq!(frame_bytes.len(), FRAME_HEADER_LEN + payload.len());
        let frame = decode_frame(&frame_bytes).unwrap();
        assert_eq!(frame.header.seq, 7);
        assert_eq!(frame.header.kind, kind::TEXT);
        assert_eq!(frame.header.len, payload.len() as u16);
        assert_eq!(frame.payload, payload);
        assert_eq!(frame.total_len(), frame_bytes.len());
    }

    /// 帧头截断（<7 字节）与 payload 越界（len 超出可用字节）必须报错。
    #[test]
    fn truncated_and_overrun() {
        let full = encode_frame(1, kind::KEY, &[0x04, 0x00, 0x00, 0x00]).unwrap();
        for cut in 0..FRAME_HEADER_LEN {
            let err = decode_frame(&full[..cut]).unwrap_err();
            assert_eq!(
                err,
                ProtoError::FrameTruncated {
                    needed: 7,
                    got: cut
                }
            );
        }
        // 修改 len 字段为越界值（覆盖头后无足够字节）
        let mut evil = full.clone();
        evil[5] = 0xFF;
        evil[6] = 0xFF;
        let err = decode_frame(&evil).unwrap_err();
        assert_eq!(
            err,
            ProtoError::PayloadOutOfBounds {
                declared: 0xFFFF,
                available: 4
            }
        );
    }

    /// len 恰好等于剩余字节 = 合法；尾部多字节留给下一帧（流式）。
    #[test]
    fn exact_and_trailing() {
        let mut buf = encode_frame(1, kind::KEY, &[1, 2, 3, 4]).unwrap();
        buf.extend_from_slice(&[0xAA, 0xBB]); // 下一帧的开头（不完整）
        let frame = decode_frame(&buf).unwrap();
        assert_eq!(frame.payload, &[1, 2, 3, 4]);
        assert_eq!(frame.total_len(), 11);
        // 推进后剩余是残帧 → 截断错误
        let rest = &buf[frame.total_len()..];
        assert!(decode_frame(rest).is_err());
    }

    /// encode 超过 u16 上限 → PayloadTooLarge（MAX_FRAME_PAYLOAD 为语义上限常量）。
    #[test]
    fn payload_too_large_on_encode() {
        let big = vec![0u8; u16::MAX as usize + 1];
        let err = encode_frame(0, kind::TEXT, &big).unwrap_err();
        assert_eq!(
            err,
            ProtoError::PayloadTooLarge {
                len: big.len(),
                max: u16::MAX as usize
            }
        );
        // 语义上限常量与 docs/02 §6 对齐
        assert_eq!(MAX_FRAME_PAYLOAD, 64 * 1024);
    }

    /// postcard 辅助：严格消费全部字节。
    #[test]
    fn postcard_strict_trailing() {
        let mut buf = Vec::new();
        encode_postcard_into(&42u32, &mut buf).unwrap();
        assert!(!buf.is_empty());
        let value: u32 = decode_postcard(&buf).unwrap();
        assert_eq!(value, 42);
        buf.push(0xFF);
        let err = decode_postcard::<u32>(&buf).unwrap_err();
        assert!(matches!(err, ProtoError::Postcard(_)));
        // 非法 varint 也要报错不 panic
        assert!(decode_postcard::<u32>(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).is_err());
    }
}
