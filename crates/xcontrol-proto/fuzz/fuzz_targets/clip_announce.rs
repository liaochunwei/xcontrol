//! fuzz target：剪贴板消息解码与接收端限额校验（docs/02 §6，Deskflow
//! CVE-2026-65976 教训：伪造 clip_id / total_len / data 超限不得 panic）。
#![no_main]

use libfuzzer_sys::fuzz_target;
use xcontrol_proto::{decode_clip, ClipAnnounce, ClipChunk, kind};

fuzz_target!(|data: &[u8]| {
    // 五条剪贴板消息的统一分派路径（postcard 严格解码 + 逐消息校验）：
    // - ChannelBind：session_id 16B postcard 路径；
    // - ClipAnnounce：total_len ≤ 16MiB 默认上限（违规报错）；
    // - ClipChunk：data ≤ 64KiB（违规报错）；
    // - ClipAck / ClipEnd：u16/u32 postcard 路径。
    for clip_kind in [
        kind::CLIP_CHANNEL_BIND,
        kind::CLIP_ANNOUNCE,
        kind::CLIP_CHUNK,
        kind::CLIP_ACK,
        kind::CLIP_END,
    ] {
        let _ = decode_clip(clip_kind, data);
    }
    // 独立解码入口再走一遍（绕过 kind 分派的最底层强校验）
    let _ = ClipAnnounce::decode(data);
    let _ = ClipChunk::decode(data);
});
