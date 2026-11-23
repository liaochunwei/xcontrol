//! 剪贴板发送计划生成（docs/02 §6.2 / docs/04 §3；纯逻辑、零 I/O）。
//!
//! 「内容字节 → `ClipAnnounce` 参数 + `ClipChunk` 序列」的通用拆解：从
//! mock 层（`crate::mock::clipboard::MockClipboard::transfer_plan`，M1）
//! 通用化到公开模块——app 剪贴板接线（11-20 两机实测收尾扩展）把本机剪贴板
//! 读出的文本经本计划转为线上帧序列。mock 版仍保留（原签名/语义不动，
//! 对拍测试继续可用），本模块是同一算法的唯一公开出处。
//!
//! - **发送端有效分块 ≤ 65472B**（64KiB − 64B：帧层 `len: u16` 物理上限
//!   65535 与 Noise 单报文明文 65519 的头部裕量，docs/02 §6.2 修订
//!   2026-11-17）；
//! - **超限不传输**：内容超过协商上限返回 `None`（FR-4.5 超限不传输——
//!   发送侧与接收端 `ClipRx` 的 total_len 校验同一口径）；
//! - blake3 摘要即 `ClipAnnounce.blake3`，接收端 `ClipEnd` 校验用。

/// 单块线上安全长度：`ClipChunk::MAX_DATA_LEN`（64KiB 解码语义上限）− 64B
/// 头部裕量 = 65472B（docs/02 §6.2 修订 2026-11-17）。
pub const WIRE_CHUNK_LEN: usize = xcontrol_proto::clip::ClipChunk::MAX_DATA_LEN - 64;

/// 一次传输的发送计划（`plan_chunks` 的产出）。
///
/// 调用方按 `chunks` 顺序编号 seq（0 起）经 [`crate::clip_tx::ClipTx`]
/// 信用窗口发送；`blake3`/`total_len` 进 `ClipAnnounce`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPlan {
    /// 内容总字节数（`ClipAnnounce.total_len`）。
    pub total_len: u32,
    /// 数据块序列（每块 ≤ [`WIRE_CHUNK_LEN`]，最后一块可为任意短）。
    pub chunks: Vec<Vec<u8>>,
    /// 全内容 BLAKE3 摘要（`ClipAnnounce.blake3`）。
    pub blake3: [u8; 32],
}

/// 把内容字节展开为发送计划（超协商上限 → `None`，FR-4.5 超限不传输）。
///
/// 空内容 → 零块计划（announce total_len=0 + end 直接交付空内容；与 mock
/// 路径同语义）。
#[must_use]
pub fn plan_chunks(content: &[u8], max_total_len: u32) -> Option<TransferPlan> {
    let total_len = u32::try_from(content.len()).ok()?;
    if total_len > max_total_len {
        return None;
    }
    let chunks = content.chunks(WIRE_CHUNK_LEN).map(<[u8]>::to_vec).collect();
    let mut blake3 = [0u8; 32];
    blake3.copy_from_slice(blake3::hash(content).as_bytes());
    Some(TransferPlan {
        total_len,
        chunks,
        blake3,
    })
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::Message;
    use xcontrol_proto::clip::{ClipAnnounce, ClipChunk};

    use super::*;

    /// 分块 ≤ 线上安全长度（65472B）、拼接还原、blake3 与内容一致；
    /// mock 版计划的同义性对拍（同一内容两路径产出相同计划）。
    #[cfg(feature = "mock-platform")]
    #[test]
    fn plan_chunks_matches_mock_semantics() {
        let content: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let plan =
            plan_chunks(&content, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN).expect("未超限应有计划");
        assert_eq!(plan.total_len, 70_000);
        assert_eq!(plan.chunks.len(), 2, "70KB → 65472B + 4528B 两块");
        assert_eq!(plan.chunks.concat(), content);
        // 对拍 mock（同一算法的 mock 出处）
        let mut mock = crate::mock::MockClipboard::new();
        mock.set_content(content.clone());
        let mock_plan = mock
            .transfer_plan(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
            .expect("mock 同内容应有计划");
        assert_eq!(plan, mock_plan, "公开模块与 mock 版同计划");
    }

    /// 恰好上限可传、超限不传输（FR-4.5）；每块整帧编码可发出。
    #[test]
    fn plan_respects_max_total_len() {
        let at_limit = vec![7u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize];
        let plan = plan_chunks(&at_limit, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN).expect("恰好上限");
        assert_eq!(plan.total_len, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN);
        assert_eq!(plan.chunks.len(), 257, "16MiB / 65472B = 256 块 + 余数块");
        for chunk in &plan.chunks {
            let msg = ClipChunk {
                clip_id: 1,
                seq: 0,
                data: chunk.clone(),
            };
            assert!(msg.encode_frame(0).is_ok(), "整块帧编码可发出");
        }

        let over = vec![0u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize + 1];
        assert!(
            plan_chunks(&over, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN).is_none(),
            "超限不传输"
        );
        // 协商缩限同样生效
        assert!(plan_chunks(&[0u8; 2048], 1024).is_none());
        // u32 域外的内容长度（usize > u32::MAX）防御性不传输
        let huge_len_hint = vec![0u8; 4];
        assert!(
            plan_chunks(&huge_len_hint, 3).is_none(),
            "协商缩限拒绝超限内容"
        );
    }

    /// 空内容 → 零块计划（announce total_len=0 + end 直接交付空内容）。
    #[test]
    fn empty_content_yields_empty_plan() {
        let plan = plan_chunks(&[], 1024).expect("空内容未超限");
        assert_eq!(plan.total_len, 0);
        assert!(plan.chunks.is_empty());
    }
}
