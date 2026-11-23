//! 剪贴板通道消息（docs/02 §4.4/§6 + docs/04 §3，postcard 编码）。
//!
//! 传输模型：`ClipAnnounce → ClipChunk* → ClipEnd`，窗口信用流控（[`ClipAck`]
//! 归还信用，初始 4 块）。剪贴板走独立 TCP 连接，首帧 [`ChannelBind`] 绑定控制
//! 连接的会话（docs/02 §4.4）。
//!
//! # 信任边界在接收端（docs/02 §6，Deskflow CVE-2026-65976 教训）
//!
//! 解码层强制（发送端自律不作数）：
//! - [`ClipAnnounce::total_len`] 超过协商上限 → [`crate::ProtoError::ClipTotalLenExceeded`]；
//! - [`ClipChunk::data`] 超过 64KiB 单块上限 → [`crate::ProtoError::ClipChunkTooLarge`]。
//!
//! 其余逐帧/会话级校验（累计长度 ≤ total_len、seq 窗口去重、10s 超时、
//! 未 announce 先 chunk）依赖连接态，由 core/net 层执行。

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;
use crate::frame::{self, MAX_FRAME_PAYLOAD};

/// 剪贴板线格式（docs/04 §2；值与 docs/02 §6 `ClipFormat` 枚举一致）。
///
/// 一次 [`ClipAnnounce`] 可携带多格式并存（text+png+html），接收端按自身
/// Caps 择优挂载（docs/04 §2）；发送端只发对端声明支持的格式。
/// 新格式随 minor 提升 + Caps 位新增（docs/05 §0.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClipFormat {
    /// `text/plain`（UTF-8、换行统一 LF）。
    Text,
    /// `image/png`（图片交换唯一必选格式）。
    Png,
    /// `text/html`（HTML fragment，UTF-8、LF）。
    Html,
    /// `text/uri-list`（P2，FR-4.6：仅文件路径列表，不传内容）。
    UriList,
}

/// 剪贴板连接首帧（docs/02 §4.4）：绑定控制连接的会话。
///
/// 对端校验 session 存在且绑定唯一，否则断开；`session_id` 由握手摘要派生
/// （docs/02 §3.3，16B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelBind {
    /// 控制连接会话标识（`BLAKE3(hs_hash)[0..16]`，docs/02 §3.3）。
    pub session_id: [u8; 16],
}

impl ChannelBind {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for ChannelBind {
    fn kind(&self) -> u8 {
        frame::kind::CLIP_CHANNEL_BIND
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 剪贴板传输宣告（docs/02 §6.1；多格式并存，docs/04 §2）。
///
/// **`total_len` 在此校验**：超过协商上限（默认 16MiB，FR-4.5 可配）立即断开。
/// `origin_id`/`origin_seq` 为防回环来源去重字段（docs/04 §5 防线 2，
/// `origin_id` 即来源 instance_id）；`blake3` 为全内容摘要，[`ClipEnd`] 后校验。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipAnnounce {
    /// 本次传输标识（同连接内唯一）。
    pub clip_id: u32,
    /// 来源实例 UUID（16B，docs/04 §5）。
    pub origin_id: [u8; 16],
    /// 来源序列号（每源递增；接收端按 `(origin_id, seq)` 去重，docs/04 §5）。
    pub origin_seq: u32,
    /// 并存格式列表（发送端只发对端 Caps 声明过的格式）。
    pub formats: Vec<ClipFormat>,
    /// 内容总字节数（**解码层按协商上限校验**）。
    pub total_len: u32,
    /// 全内容 BLAKE3 摘要（32B；ClipEnd 时校验，失败丢弃，docs/02 §6.5）。
    pub blake3: [u8; 32],
}

impl ClipAnnounce {
    /// 默认上限：16MiB（docs/02 §6；可经 `Options.clipboard_max_bytes` 协商）。
    pub const DEFAULT_MAX_TOTAL_LEN: u32 = 16 * 1024 * 1024;

    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }

    /// 解码并以**默认上限**（16MiB）校验 `total_len`。
    ///
    /// # Errors
    /// [`ProtoError::ClipTotalLenExceeded`] / [`ProtoError::Postcard`]。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        Self::decode_with_limit(payload, Self::DEFAULT_MAX_TOTAL_LEN)
    }

    /// 解码并以**协商上限**校验 `total_len`（docs/02 §6.1：信任边界在接收端）。
    ///
    /// # Errors
    /// - `total_len > max_total_len` → [`ProtoError::ClipTotalLenExceeded`]；
    /// - postcard 解码失败 / 尾部残留 → [`ProtoError::Postcard`]。
    pub fn decode_with_limit(payload: &[u8], max_total_len: u32) -> Result<Self, ProtoError> {
        let announce: Self = frame::decode_postcard(payload)?;
        if announce.total_len > max_total_len {
            return Err(ProtoError::ClipTotalLenExceeded {
                total_len: announce.total_len,
                max: max_total_len,
            });
        }
        Ok(announce)
    }
}

impl crate::Message for ClipAnnounce {
    fn kind(&self) -> u8 {
        frame::kind::CLIP_ANNOUNCE
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 剪贴板数据块（docs/02 §6.2；64KiB 单块上限）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipChunk {
    /// 所属传输。
    pub clip_id: u32,
    /// 块序号（16bit seq 窗口去重；乱序=违规断开，由连接态执行）。
    pub seq: u16,
    /// 块数据（**解码层校验 ≤ 64KiB**；接收端每帧检查累计 ≤ total_len）。
    pub data: Vec<u8>,
}

impl ClipChunk {
    /// 单块数据上限：64KiB（docs/02 §6.2，与 [`MAX_FRAME_PAYLOAD`] 对齐）。
    ///
    /// **这是接收端解码校验的语义上限，不是发送分块值**：满 64KiB 数据 +
    /// postcard 头必然超帧层 `len: u16`（65535）与 Noise 单报文明文（65519），
    /// 发送端有效分块 ≤ 65472B（64KiB−64B，docs/02 §6.2 修订 2026-11-17）。
    pub const MAX_DATA_LEN: usize = MAX_FRAME_PAYLOAD;

    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }

    /// 解码并校验 `data` ≤ 64KiB（docs/02 §6.2）。
    ///
    /// # Errors
    /// [`ProtoError::ClipChunkTooLarge`] / [`ProtoError::Postcard`]。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        let chunk: Self = frame::decode_postcard(payload)?;
        if chunk.data.len() > Self::MAX_DATA_LEN {
            return Err(ProtoError::ClipChunkTooLarge {
                len: chunk.data.len(),
                max: Self::MAX_DATA_LEN,
            });
        }
        Ok(chunk)
    }
}

impl crate::Message for ClipChunk {
    fn kind(&self) -> u8 {
        frame::kind::CLIP_CHUNK
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 信用归还（docs/02 §6.3：初始 4 块信用，每 ack 归还一块；窗口空则发送端等待）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipAck {
    /// 归还信用的块序号。
    pub seq: u16,
}

impl ClipAck {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for ClipAck {
    fn kind(&self) -> u8 {
        frame::kind::CLIP_ACK
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 传输结束（docs/02 §6.5）：对端做 blake3 校验，失败丢弃并告警（不重传）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipEnd {
    /// 结束的传输标识。
    pub clip_id: u32,
}

impl ClipEnd {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for ClipEnd {
    fn kind(&self) -> u8 {
        frame::kind::CLIP_END
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

// ============================================================
// 多格式内容段布局（docs/02 §6 v1 wire 类型补充，2026-11-19 M5）：
// announce→chunk→end 重组出的 content 字节与 announce.formats 的对应关系。
// formats.len() == 1 → content 即该格式原始字节（无包裹——单格式零开销）；
// formats.len() >= 2 → content = postcard 编码的 `Vec<Vec<u8>>`（段序 == formats
// 序，段数 == formats.len()）。接收端按 formats 长度选择布局（确定性、无嗅探）。
// ============================================================

/// 多格式内容的分段编码（发送侧）：`segments[i]` 对应 `formats[i]` 的载荷。
///
/// 单段（`formats.len() == 1` 用法）：返回该段原始字节；多段：postcard
/// `Vec<Vec<u8>>` 布局。调用方契约：`segments.len()` 必须与 announce.formats
/// 长度一致。
///
/// # Errors
/// [`ProtoError::Postcard`]：序列化失败。
pub fn encode_content_segments(segments: &[&[u8]]) -> Result<Vec<u8>, ProtoError> {
    match segments {
        // 单格式：content 即该格式原始字节（无包裹）
        [only] => Ok(only.to_vec()),
        // 多格式：postcard Vec<Vec<u8>>（varint 段数 + 每段 varint 长度前缀）
        _ => {
            let owned: Vec<Vec<u8>> = segments.iter().map(|seg| seg.to_vec()).collect();
            let mut out = Vec::new();
            frame::encode_postcard_into(&owned, &mut out)?;
            Ok(out)
        }
    }
}

/// 多格式内容的分段解码（接收侧）：`segment_count` 取 announce.formats.len()。
///
/// 单段：原样返回；多段：postcard 解码并校验段数一致（段数不符 = 违规）。
///
/// # Errors
/// [`ProtoError::Postcard`]：解码失败 / 段数与声明不符 / 尾部残留。
pub fn decode_content_segments(
    payload: &[u8],
    segment_count: usize,
) -> Result<Vec<Vec<u8>>, ProtoError> {
    match segment_count {
        1 => Ok(vec![payload.to_vec()]),
        0 => Err(ProtoError::Postcard(
            "多格式段解码：formats 为空（无格式内容不可达）".into(),
        )),
        _ => {
            let segments: Vec<Vec<u8>> = frame::decode_postcard(payload)?;
            if segments.len() != segment_count {
                return Err(ProtoError::Postcard(format!(
                    "多格式段数不符：声明 {segment_count} 实得 {}",
                    segments.len()
                )));
            }
            Ok(segments)
        }
    }
}

/// kind+payload → 剪贴板消息（统一分派入口）。
///
/// - kind 不在剪贴板表（`0x50–0x54`）→ `Ok(None)`（调用方按 docs/02 §4.1 跳过）；
/// - 各消息解码时执行 [`ClipAnnounce`] 上限校验（默认 16MiB）与
///   [`ClipChunk`] 64KiB 校验（信任边界在接收端，docs/02 §6）。
///
/// # Errors
/// postcard 解码失败 / 传输约束违规（[`crate::ProtoError`] 各剪贴板变体）。
pub fn decode_clip(kind: u8, payload: &[u8]) -> Result<Option<ClipMsg>, ProtoError> {
    if !frame::is_clip_kind(kind) {
        return Ok(None);
    }
    let msg = match kind {
        frame::kind::CLIP_CHANNEL_BIND => {
            ClipMsg::Bind(frame::decode_postcard::<ChannelBind>(payload)?)
        }
        frame::kind::CLIP_ANNOUNCE => ClipMsg::Announce(ClipAnnounce::decode(payload)?),
        frame::kind::CLIP_CHUNK => ClipMsg::Chunk(ClipChunk::decode(payload)?),
        frame::kind::CLIP_ACK => ClipMsg::Ack(frame::decode_postcard::<ClipAck>(payload)?),
        frame::kind::CLIP_END => ClipMsg::End(frame::decode_postcard::<ClipEnd>(payload)?),
        // is_clip_kind 已排除未知 kind
        _ => return Ok(None),
    };
    Ok(Some(msg))
}

/// 剪贴板消息统一枚举（[`decode_clip`] 的返回载体；变体与 kind 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClipMsg {
    /// 连接绑定（[`ChannelBind`]，kind 0x50）。
    Bind(ChannelBind),
    /// 传输宣告（[`ClipAnnounce`]，kind 0x51）。
    Announce(ClipAnnounce),
    /// 数据块（[`ClipChunk`]，kind 0x52）。
    Chunk(ClipChunk),
    /// 信用归还（[`ClipAck`]，kind 0x53）。
    Ack(ClipAck),
    /// 传输结束（[`ClipEnd`]，kind 0x54）。
    End(ClipEnd),
}

impl ClipMsg {
    /// 按变体分发到对应消息的 [`Message`](crate::Message) 实现（编码侧便捷入口）。
    #[must_use]
    pub fn as_message(&self) -> &dyn crate::Message {
        match self {
            Self::Bind(m) => m,
            Self::Announce(m) => m,
            Self::Chunk(m) => m,
            Self::Ack(m) => m,
            Self::End(m) => m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性样本。
    fn channel_bind() -> ChannelBind {
        ChannelBind {
            session_id: [0x11; 16],
        }
    }

    fn announce() -> ClipAnnounce {
        ClipAnnounce {
            clip_id: 7,
            origin_id: [0x22; 16],
            origin_seq: 42,
            formats: vec![ClipFormat::Text, ClipFormat::Png],
            total_len: 2048,
            blake3: [0x33; 32],
        }
    }

    fn chunk() -> ClipChunk {
        ClipChunk {
            clip_id: 7,
            seq: 0,
            data: b"clip payload".to_vec(),
        }
    }

    /// 五条消息全 round-trip + kind 映射 + decode_clip 分派。
    #[test]
    fn roundtrip_and_dispatch() {
        let samples = [
            (
                frame::kind::CLIP_CHANNEL_BIND,
                ClipMsg::Bind(channel_bind()),
            ),
            (frame::kind::CLIP_ANNOUNCE, ClipMsg::Announce(announce())),
            (frame::kind::CLIP_CHUNK, ClipMsg::Chunk(chunk())),
            (frame::kind::CLIP_ACK, ClipMsg::Ack(ClipAck { seq: 3 })),
            (frame::kind::CLIP_END, ClipMsg::End(ClipEnd { clip_id: 7 })),
        ];
        for (kind, msg) in samples {
            let mut payload = Vec::new();
            msg.as_message().encode_payload(&mut payload).unwrap();
            let decoded = decode_clip(kind, &payload).unwrap().unwrap();
            assert_eq!(decoded, msg, "kind 0x{kind:02X}");
        }
    }

    /// 非剪贴板 kind → None。
    #[test]
    fn unknown_kind_is_none() {
        let payload = [0u8; 4];
        for kind in [frame::kind::KEY, frame::kind::CTRL_PING, 0x00, 0x55, 0xFF] {
            assert_eq!(decode_clip(kind, &payload).unwrap(), None);
        }
    }

    /// ClipAnnounce：total_len 上限校验（默认 16MiB + 自定义协商上限）。
    #[test]
    fn announce_total_len_limit() {
        // 默认上限内
        let mut payload = Vec::new();
        announce().encode_payload(&mut payload).unwrap();
        assert!(ClipAnnounce::decode(&payload).is_ok());

        // 恰好在默认上限
        let at_limit = ClipAnnounce {
            total_len: ClipAnnounce::DEFAULT_MAX_TOTAL_LEN,
            ..announce()
        };
        let mut payload = Vec::new();
        at_limit.encode_payload(&mut payload).unwrap();
        assert!(ClipAnnounce::decode(&payload).is_ok());

        // 超默认上限 1 字节
        let over = ClipAnnounce {
            total_len: ClipAnnounce::DEFAULT_MAX_TOTAL_LEN + 1,
            ..announce()
        };
        let mut payload = Vec::new();
        over.encode_payload(&mut payload).unwrap();
        let err = ClipAnnounce::decode(&payload).unwrap_err();
        assert_eq!(
            err,
            ProtoError::ClipTotalLenExceeded {
                total_len: ClipAnnounce::DEFAULT_MAX_TOTAL_LEN + 1,
                max: ClipAnnounce::DEFAULT_MAX_TOTAL_LEN
            }
        );

        // 自定义协商上限（Options.clipboard_max_bytes 缩容后）
        let mut payload = Vec::new();
        announce().encode_payload(&mut payload).unwrap();
        let err = ClipAnnounce::decode_with_limit(&payload, 1024).unwrap_err();
        assert_eq!(
            err,
            ProtoError::ClipTotalLenExceeded {
                total_len: 2048,
                max: 1024
            }
        );
    }

    /// ClipChunk：64KiB 单块上限（边界值与超限负例）。
    #[test]
    fn chunk_data_limit() {
        // 恰好 64KiB 合法
        let at_limit = ClipChunk {
            clip_id: 1,
            seq: 9,
            data: vec![0u8; ClipChunk::MAX_DATA_LEN],
        };
        let mut payload = Vec::new();
        at_limit.encode_payload(&mut payload).unwrap();
        let decoded = ClipChunk::decode(&payload).unwrap();
        assert_eq!(decoded.data.len(), ClipChunk::MAX_DATA_LEN);

        // 超限负例：绕过帧层直接构造 postcard（模拟恶意流）
        let over = ClipChunk {
            data: vec![0u8; ClipChunk::MAX_DATA_LEN + 1],
            ..chunk()
        };
        let mut payload = Vec::new();
        over.encode_payload(&mut payload).unwrap();
        let err = ClipChunk::decode(&payload).unwrap_err();
        assert_eq!(
            err,
            ProtoError::ClipChunkTooLarge {
                len: ClipChunk::MAX_DATA_LEN + 1,
                max: ClipChunk::MAX_DATA_LEN
            }
        );
        assert_eq!(ClipChunk::MAX_DATA_LEN, 64 * 1024);
    }

    /// 恶意 payload：垃圾 / 截断 / 残留 → 报错不 panic。
    #[test]
    fn malformed_payloads() {
        for payload in [&b""[..], &[0xFF, 0xFF, 0xFF][..], &[0x00, 0x01][..]] {
            for kind in [
                frame::kind::CLIP_CHANNEL_BIND,
                frame::kind::CLIP_ANNOUNCE,
                frame::kind::CLIP_CHUNK,
                frame::kind::CLIP_ACK,
                frame::kind::CLIP_END,
            ] {
                assert!(
                    decode_clip(kind, payload).is_err(),
                    "kind 0x{kind:02X} payload {payload:?} 应报错"
                );
            }
        }
        // 尾部残留
        let mut payload = Vec::new();
        let ack = ClipAck { seq: 1 };
        frame::encode_postcard_into(&ack, &mut payload).unwrap();
        payload.push(0);
        assert!(decode_clip(frame::kind::CLIP_ACK, &payload).is_err());
    }

    /// 帧级完整往返。
    #[test]
    fn frame_roundtrip() {
        let msg = ClipMsg::Chunk(ClipChunk {
            clip_id: 3,
            seq: 1,
            data: vec![0xAB; 4096],
        });
        let bytes = msg.as_message().encode_frame(5).unwrap();
        let frame = frame::decode_frame(&bytes).unwrap();
        assert_eq!(frame.header.seq, 5);
        assert_eq!(frame.header.kind, frame::kind::CLIP_CHUNK);
        let decoded = decode_clip(frame.header.kind, frame.payload)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, msg);
    }

    /// 多格式内容段布局（docs/02 §6 v1 补充，2026-11-19 M5）：
    /// 单段 = 原始字节无包裹；多段 = postcard `Vec<Vec<u8>>`，段序 == formats 序；
    /// 解码段数校验 + 错误负例。
    #[test]
    fn content_segment_layout() {
        // 单格式：无包裹（与 formats.len()==1 的接收端选择一致）
        let png = vec![0x89u8, 0x50, 0x4E, 0x47, 1, 2, 3];
        let encoded = encode_content_segments(&[&png]).unwrap();
        assert_eq!(encoded, png, "单格式 content 即原始字节");
        assert_eq!(
            decode_content_segments(&encoded, 1).unwrap(),
            vec![png.clone()]
        );

        // 多格式：postcard 布局 + 段序一致 + 往返还原
        let text = b"hello".to_vec();
        let html = b"<b>hi</b>".to_vec();
        let encoded = encode_content_segments(&[&text, &png, &html]).unwrap();
        assert_ne!(encoded, text, "多格式有包裹（单段原样断言的反例面）");
        let segments = decode_content_segments(&encoded, 3).unwrap();
        assert_eq!(
            segments,
            vec![text.clone(), png.clone(), html],
            "段序与 formats 序一致"
        );

        // 段数不符负例
        let err = decode_content_segments(&encoded, 2).unwrap_err();
        assert!(
            err.to_string().contains("段数不符"),
            "声明与实际段数校验：{err}"
        );

        // 空段合法（空格式载荷）：段数一致即通过
        let empty: Vec<u8> = Vec::new();
        let encoded = encode_content_segments(&[&text, &empty]).unwrap();
        assert_eq!(
            decode_content_segments(&encoded, 2).unwrap(),
            vec![text, Vec::new()],
            "空段往返"
        );

        // 畸形多格式载荷
        assert!(decode_content_segments(b"garbage", 3).is_err());
        // formats 为空不可达
        assert!(decode_content_segments(&encoded, 0).is_err());
    }
}
