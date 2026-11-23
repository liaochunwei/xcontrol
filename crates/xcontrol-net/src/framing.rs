//! 长度前缀分帧适配器（11-17-m2-tcp，design.md：u32 BE len + Noise 报文）。
//!
//! TCP 是字节流：snow 传输态报文自带 AEAD 认证边界，但流上无消息边界——
//! 接收端必须整报文送入 `read_message`（M1 transport.rs"消息边界契约"），
//! 故每条 Noise 报文外再包一层长度前缀帧：
//!
//! ```text
//! wire: [magic: u16 BE = 0xC1A1][len: u16 BE][Noise 报文（≤ 65535B，含 16B tag）]
//! ```
//!
//! - **同步标记（11-21 用户建议定稿）**：原 u32 len 的高 2 字节恒 0x0000（len
//!   ≤ 65535 只用低 16 位）——改作 magic 常量，**零开销**获得错位检测与确定性
//!   重同步能力（[`read_frame_resync`] 逐字节扫 magic，扫到即对齐）；
//! - **读侧 `read_exact` 两段式**（4B 头 + len 载荷）：粘包（一次 read 到多帧拼接）
//!   与拆包（一帧分多次到达）天然正确——头定界后载荷按 `read_exact` 凑满为止；
//! - **对端优雅关闭检测**：帧头起点读到 0 字节 = FIN → [`NetError::Closed`]
//!   （与 M1 回环"对端 shutdown → recv 得 Closed"对齐）；帧中途截断属异常断开
//!   → [`NetError::IoFailed`]。两类错误调用方语义相同：断开（docs/02 §7）。
//!
//! # 帧校验（信任边界在接收端，docs/02 §6 原则）
//!
//! `magic != 0xC1A1`（错位/垃圾）→ [`NetError::FrameInvalid`] → 调用方触发
//! 重同步扫描；`len == 0`（Noise 报文最小为 16B tag，0 长度必非合法报文）与
//! `len > 65535`（snow `MAXMSGLEN`）同上。`1..=15` 的长度交给解密层判伪
//! （必然 [`NetError::DecryptFailed`]），不在此处二次猜边界。握手期一切帧
//! 错误折叠进 [`NetError::HandshakeFailed`]（[`crate::noise`] 的 async 握手
//! 负责），传输期原样上抛。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::NetError;

/// 长度前缀头长（magic u16 + len u16）。
pub const HEADER_LEN: usize = 4;

/// 传输帧同步标记（每帧 4B 头的前 2 字节；错位检测与重同步锚点）。
pub const TRANSPORT_MAGIC: u16 = 0xC1A1;

/// 单帧载荷上限 = snow `TransportState` 单条报文上限（65535B，`MAXMSGLEN`，
/// 见 [`crate::noise`] 模块文档；未公开导出，按 snow 0.10 源码钉死并注释）。
pub const MAX_PACKET_LEN: usize = 65535;

/// 编码一帧：`[magic: u16][len: u16][packet]` 追加到 `out`（send 路径每帧
/// 独立调用）。
///
/// # Errors
/// [`NetError::FrameInvalid`]：`packet` 为空或超过 [`MAX_PACKET_LEN`]。
pub fn encode_into(packet: &[u8], out: &mut Vec<u8>) -> Result<(), NetError> {
    let len = packet.len();
    if len == 0 || len > MAX_PACKET_LEN {
        return Err(NetError::FrameInvalid);
    }
    out.reserve(HEADER_LEN + len);
    out.extend_from_slice(&TRANSPORT_MAGIC.to_be_bytes());
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(packet);
    Ok(())
}

/// 解码帧头：返回载荷长度。
///
/// # Errors
/// [`NetError::FrameInvalid`]：magic 不匹配（流错位——调用方应触发
/// [`read_frame_resync`]）或 `len == 0`（空帧）。
pub fn decode_header(header: &[u8; HEADER_LEN]) -> Result<usize, NetError> {
    let magic = u16::from_be_bytes([header[0], header[1]]);
    if magic != TRANSPORT_MAGIC {
        return Err(NetError::FrameInvalid);
    }
    let len = u16::from_be_bytes([header[2], header[3]]) as usize;
    if len == 0 {
        return Err(NetError::FrameInvalid);
    }
    Ok(len)
}

/// 从字节流读取一个完整帧，载荷写入 `out`（覆盖内容：先清空再填充），返回载荷长度。
///
/// 两段式：先探测帧头（首 `read` 得 0 字节 = 对端优雅关闭），再 `read_exact` 凑满
/// 头与载荷——粘包/拆包天然正确（见模块文档）。
///
/// # Errors
/// - [`NetError::Closed`]：帧头起点读到 EOF（对端 FIN，语义对齐 M1 回环）；
/// - [`NetError::FrameInvalid`]：帧头长度非法（调用方应断开，docs/02 §7）；
/// - [`NetError::IoFailed`]：帧中途截断或 socket 读写错误。
///
/// 出错时 `out` 内容未定义（可能已被清空/部分填充）。
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> Result<usize, NetError> {
    let mut header = [0u8; HEADER_LEN];
    // 帧头探测：read 可能少读，但不返回 0 即流未到 EOF（EOF → 对端优雅关闭）。
    let first = reader.read(&mut header).await.map_err(io_failed)?;
    if first == 0 {
        return Err(NetError::Closed);
    }
    if first < HEADER_LEN {
        reader
            .read_exact(&mut header[first..])
            .await
            .map_err(io_failed)?;
    }
    let len = decode_header(&header)?;
    out.clear();
    out.resize(len, 0);
    reader.read_exact(out).await.map_err(io_failed)?;
    Ok(len)
}

/// 编码并写出一个完整帧（encode_into + write_all；send 路径单入口）。
///
/// # Errors
/// [`NetError::FrameInvalid`]（载荷非法）或 [`NetError::IoFailed`]（写失败）。
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    packet: &[u8],
) -> Result<(), NetError> {
    let mut frame = Vec::with_capacity(HEADER_LEN + packet.len());
    encode_into(packet, &mut frame)?;
    writer.write_all(&frame).await.map_err(io_failed)?;
    Ok(())
}

/// I/O 错误归一：传输期统一 [`NetError::IoFailed`]（不含 io::Error 细节，
/// docs/08 §2 错误信息纪律；断开决策由调用方执行，docs/02 §7）。
/// 对端优雅关闭（EOF）在 [`read_frame`] 内单独映射 [`NetError::Closed`]。
///
/// 流错位重同步（11-21 两机实测：控制随大图断连不可接受）：从错位流中
/// **逐字节滑动**找到下一个可信帧头（len ∈ 1..=[`MAX_PACKET_LEN`]），读出
/// 该帧载荷。扫描上限 [`RESYNC_SCAN_LIMIT`] 字节——超出即放弃（真链路故障，
/// 上抛 `FrameInvalid` 由调用方断开）。
pub async fn read_frame_resync<R: AsyncRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> Result<usize, NetError> {
    let mut scanned: usize = 0;
    let mut byte_buf = [0u8; 1];
    let mut header = [0u8; HEADER_LEN];
    let mut header_filled = 0usize;
    while scanned < RESYNC_SCAN_LIMIT {
        // 逐字节滑动：每次读 1 字节并入滑动帧头窗口（从 `first < HEADER_LEN`
        // 的 read 分片逻辑复用语义）。
        let n = reader.read(&mut byte_buf).await.map_err(io_failed)?;
        if n == 0 {
            return Err(NetError::Closed);
        }
        scanned += n;
        header[header_filled] = byte_buf[0];
        header_filled += 1;
        if header_filled < HEADER_LEN {
            continue;
        }
        header_filled = 0;
        // 候选帧头：len ∈ 1..=MAX_PACKET_LEN（0 与越界都不可信）。
        if let Ok(len) = decode_header(&header) {
            // 可信候选：读出该帧载荷并返回（解密失败由调用方处置——
            // DecryptFailed 才是真错位）。
            out.clear();
            out.resize(len, 0);
            reader.read_exact(out).await.map_err(io_failed)?;
            return Ok(len);
        }
    }
    Err(NetError::FrameInvalid)
}

/// 重同步扫描上限（字节）：超过即放弃——错位超过一整帧的垃圾意味着
/// 链路层故障而非单帧残帧。
pub const RESYNC_SCAN_LIMIT: usize = MAX_PACKET_LEN;
fn io_failed(_: std::io::Error) -> NetError {
    NetError::IoFailed
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, ReadBuf};

    use super::*;

    /// 测试辅助取值（helpers 非 `#[test]` 作用域，clippy 豁免不覆盖——与
    /// tests/chain.rs 的 `must` 同形）。
    #[track_caller]
    fn must<T, E: std::fmt::Debug>(context: &'static str, result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{context}: {error:?}"),
        }
    }

    /// 定长切块 reader：每次 `poll_read` 至多吐 `chunk` 字节——确定性模拟
    /// 拆包（chunk=1 即逐字节到达）；数据耗尽后返回 EOF。
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunk: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk,
            }
        }
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let remaining = self.data.len() - self.pos;
            let n = remaining.min(self.chunk).min(buf.remaining());
            // n == 0 且数据已尽 → EOF（put_slice 空切片 + Ready(Ok) 之外的
            // 显式路径，语义与 socket EOF 一致）。
            if n == 0 && remaining == 0 {
                return Poll::Ready(Ok(()));
            }
            buf.put_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    /// 三条测试报文（长度互异，覆盖头部/载荷对齐差异）。
    fn sample_packets() -> Vec<Vec<u8>> {
        vec![
            vec![0xA1; 16],                            // 最小合法 Noise 报文（仅 tag）
            vec![0xB2; 95],                            // 接近握手 msg2 形态
            (0..=255_u8).cycle().take(1024).collect(), // 1KiB 载荷
        ]
    }

    /// 编码多帧拼接为一条字节流（模拟发送端）。
    fn concat_frames(packets: &[Vec<u8>]) -> Vec<u8> {
        let mut wire = Vec::new();
        for packet in packets {
            must("编码帧", encode_into(packet, &mut wire));
        }
        wire
    }

    /// 锚点：帧头 4B + u32 BE + 上限常量（design.md 分帧决策）。
    #[test]
    fn constants_match_design() {
        assert_eq!(HEADER_LEN, 4);
        assert_eq!(MAX_PACKET_LEN, 65535);
    }

    /// 单帧 round-trip：编码 → 解码还原等价。
    #[tokio::test]
    async fn single_frame_roundtrip() {
        let packet = vec![0x5A; 500];
        let mut wire = Vec::new();
        must("编码帧", encode_into(&packet, &mut wire));
        assert_eq!(wire.len(), HEADER_LEN + 500, "线上 = 4B 头 + 载荷");
        assert_eq!(
            &wire[..HEADER_LEN],
            &[0xC1, 0xA1, 0x01, 0xF4],
            "magic + u16 BE 长度前缀"
        );

        let mut reader = ChunkedReader::new(wire, usize::MAX); // 一次全到（粘包极限）
        let mut out = Vec::new();
        let len = must("读帧", read_frame(&mut reader, &mut out).await);
        assert_eq!(len, 500);
        assert_eq!(out, packet);
    }

    /// 粘包：多帧一次 write 全部到达 → 逐帧解码还原（顺序、内容双保真）。
    #[tokio::test]
    async fn coalesced_frames_decode_in_order() {
        let packets = sample_packets();
        let wire = concat_frames(&packets);
        let mut reader = ChunkedReader::new(wire, usize::MAX);

        let mut out = Vec::new();
        for expected in &packets {
            let len = must("粘包读帧", read_frame(&mut reader, &mut out).await);
            assert_eq!(len, expected.len());
            assert_eq!(&out, expected, "粘包流逐帧还原必须等价");
        }
        // 全部帧消费完 → EOF → Closed（对端优雅关闭语义）。
        assert!(
            matches!(
                read_frame(&mut reader, &mut out).await,
                Err(NetError::Closed)
            ),
            "流排空后应得 Closed"
        );
    }

    /// 拆包：同一帧流按 1B/2B/7B/64B 切块分批到达 → 解码还原等价。
    /// chunk=1 覆盖最坏拆包（帧头逐字节、载荷逐字节）。
    #[tokio::test]
    async fn split_frames_reassemble() {
        let packets = sample_packets();
        let wire = concat_frames(&packets);
        for chunk in [1, 2, 7, 64] {
            let mut reader = ChunkedReader::new(wire.clone(), chunk);
            let mut out = Vec::new();
            for expected in &packets {
                let len = must("拆包读帧", read_frame(&mut reader, &mut out).await);
                assert_eq!(len, expected.len(), "chunk={chunk}");
                assert_eq!(&out, expected, "chunk={chunk}");
            }
        }
    }

    /// 边界穷举：第一条帧在字节流每个可能位置切一刀（半帧 write + 余下 write），
    /// 全部位置解码结果一致——design.md 测试设计的"分两次 write 半帧"场景。
    #[tokio::test]
    async fn every_split_point_reassembles_identically() {
        let packet = vec![0xC3; 200];
        let mut wire = Vec::new();
        must("编码帧", encode_into(&packet, &mut wire));
        for split in 1..wire.len() {
            let (head, tail) = wire.split_at(split);
            let mut reader = ChunkedReader::new([head, tail].concat(), usize::MAX);
            let mut out = Vec::new();
            let len = must("半帧读法", read_frame(&mut reader, &mut out).await);
            assert_eq!(len, 200, "split={split}");
            assert_eq!(out, packet, "split={split}");
        }
    }

    /// 半帧等待：前半到达后流暂停（reader 数据耗尽 → EOF）不误判——拆包场景
    /// 的"等待后半帧"由真实 socket 的 Pending 承担；本测试断言 reader 层 EOF
    /// 与帧头探测的行为边界（载荷不完整 + 数据尽 = IoFailed 而非 Closed）。
    #[tokio::test]
    async fn truncated_payload_is_io_failed_not_closed() {
        let mut wire = Vec::new();
        must("编码帧", encode_into(&[0xD4; 64], &mut wire));
        // 只喂 4B 头 + 10B 载荷，随后 EOF：帧中途断开 ≠ 优雅关闭。
        let mut reader = ChunkedReader::new(wire[..14].to_vec(), usize::MAX);
        let mut out = Vec::new();
        assert!(matches!(
            read_frame(&mut reader, &mut out).await,
            Err(NetError::IoFailed)
        ));
    }

    /// 非法帧头：magic 不匹配（错位/垃圾）与 len=0（空帧）→ FrameInvalid。
    #[test]
    fn invalid_headers_rejected() {
        // magic 不匹配（11-21 同步标记格式）。
        let bad_magic = [0x00, 0x00, 0x00, 0x64]; // 旧 u32 len 形态 + 错 magic
        assert_eq!(
            decode_header(&bad_magic),
            Err(NetError::FrameInvalid),
            "magic 不匹配 = 流错位"
        );
        // magic 对 + len=0（空帧必非合法 Noise 报文，最小 16B tag）。
        let zero_len = [0xC1, 0xA1, 0x00, 0x00];
        assert_eq!(decode_header(&zero_len), Err(NetError::FrameInvalid));
        // magic 对 + len 合法。
        let ok = [0xC1, 0xA1, 0x01, 0xF4];
        assert_eq!(decode_header(&ok), Ok(500));

        assert_eq!(
            encode_into(&[], &mut Vec::new()),
            Err(NetError::FrameInvalid)
        );
        let oversized_packet = vec![0u8; MAX_PACKET_LEN + 1];
        assert_eq!(
            encode_into(&oversized_packet, &mut Vec::new()),
            Err(NetError::FrameInvalid)
        );
        // 上限帧可编码（65535 载荷 + 4B 头）
        let mut out = Vec::new();
        let at_limit_packet = vec![0u8; MAX_PACKET_LEN];
        must("上限帧编码", encode_into(&at_limit_packet, &mut out));
        assert_eq!(out.len(), HEADER_LEN + MAX_PACKET_LEN);
    }

    /// write_frame → read_frame 往返等价（send/recv 单入口组合路径）。
    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let packet = b"noise-packet-payload".to_vec();
        let mut wire = Vec::new();
        must("写帧", write_frame(&mut wire, &packet).await);
        let mut reader = ChunkedReader::new(wire, 3); // 3B 切块（拆包）
        let mut out = Vec::new();
        let len = must("读帧", read_frame(&mut reader, &mut out).await);
        assert_eq!(len, packet.len());
        assert_eq!(out, packet);
    }

    /// 空 reader：无任何字节即 EOF → Closed（对端未发先关）。
    #[tokio::test]
    async fn empty_stream_is_closed() {
        let mut reader = ChunkedReader::new(Vec::new(), 4);
        let mut out = Vec::new();
        assert!(matches!(
            read_frame(&mut reader, &mut out).await,
            Err(NetError::Closed)
        ));
    }

    /// out 复用：read_frame 覆盖式填充（M1 Transport recv 语义对齐）。
    #[tokio::test]
    async fn read_frame_overwrites_out() {
        let packets = sample_packets();
        let wire = concat_frames(&packets);
        let mut reader = ChunkedReader::new(wire, usize::MAX);
        let mut out = vec![0xEE; 4096]; // 预置脏数据
        must("首帧", read_frame(&mut reader, &mut out).await);
        assert_eq!(out, packets[0], "首帧覆盖脏数据");
        must("次帧", read_frame(&mut reader, &mut out).await);
        assert_eq!(out, packets[1], "次帧覆盖首帧（无拼接残留）");
    }
}
