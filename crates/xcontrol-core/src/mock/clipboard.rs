//! mock 剪贴板（内容 + 变更触发 + 传输计划脚本化）。
//!
//! watch 语义（docs/04 §4 平台监听的抽象等价物）：`set_content`/`set_text`
//! 递增变更计数，[`MockClipboard::has_changes_since`] 即"有待同步变更"判定，
//! 对应 docs/04 §3「变化方主动推送」的触发源。
//!
//! 传输脚本化（docs/04 §3）：
//! - [`MockClipboard::transfer_plan`] 把当前内容展开为
//!   announce（total_len + blake3）→ chunk（≤64KiB）→ end 的发送计划；
//! - **超限不传输**：内容超过协商上限返回 `None`（上层托盘气泡提示语义）；
//! - **超时场景**：调用方只发送计划的前缀（announce + 部分 chunk）后停滞，
//!   接收端由 [`crate::clip_rx::ClipRx::on_tick`] 在 10s 时限清理重组状态。

// 计划生成已通用化到公开模块 [`crate::clip_plan`]（11-20 app 剪贴板接线：
// 本机剪贴板读出的文本经同一算法上 wire）——此处类型转发，`transfer_plan`
// 原签名/返回类型路径不变（mock 消费方与对拍测试零改动）。
use xcontrol_proto::FileEntry;

pub use crate::clip_plan::{TransferPlan, WIRE_CHUNK_LEN};

/// 内存剪贴板：内容 + 变更计数（watch 触发）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MockClipboard {
    content: Vec<u8>,
    file_refs: Vec<FileEntry>,
    change_count: u64,
}

impl MockClipboard {
    /// 空内容剪贴板。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入 UTF-8 文本（docs/04 §2 `text/plain`：换行统一 LF 由调用方保证）。
    pub fn set_text(&mut self, text: &str) {
        self.set_content(text.as_bytes().to_vec());
    }

    /// 写入任意线格式内容（递增变更计数）。
    pub fn set_content(&mut self, content: Vec<u8>) {
        self.change_count = self.change_count.saturating_add(1);
        self.content = content;
        self.file_refs.clear();
    }

    /// 写入文件引用（FR-7.3 元数据面源端；递增变更计数并清空文本/图片内容
    /// ——同板多域时以最后写入者为准，docs/04 §8.2）。
    pub fn set_file_refs(&mut self, refs: Vec<FileEntry>) {
        self.change_count = self.change_count.saturating_add(1);
        self.file_refs = refs;
        self.content.clear();
    }

    /// 当前文件引用（探测路径的读取口，docs/05 §0.2 检测列的 mock 等价物）。
    #[must_use]
    pub fn file_refs(&self) -> &[FileEntry] {
        &self.file_refs
    }

    /// 当前内容。
    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// 累计变更次数（watch 触发判定的基准）。
    #[must_use]
    pub fn change_count(&self) -> u64 {
        self.change_count
    }

    /// 变更触发器：`seen` 之后有过写入 → `true`（docs/04 §3「变化方主动推送」）。
    #[must_use]
    pub fn has_changes_since(&self, seen_change_count: u64) -> bool {
        self.change_count > seen_change_count
    }

    /// 传输计划（发送侧脚本化入口）。
    ///
    /// - 内容字节超过 `max_total_len`（协商上限，docs/04 §3「超限不传输」）
    ///   → `None`；
    /// - 否则按线上安全长度（65472B）分块 + BLAKE3 摘要展开完整计划；
    ///   空内容 → 零块计划。算法单一出处 = [`crate::clip_plan::plan_chunks`]。
    #[must_use]
    pub fn transfer_plan(&self, max_total_len: u32) -> Option<TransferPlan> {
        crate::clip_plan::plan_chunks(&self.content, max_total_len)
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::Message;
    use xcontrol_proto::clip::{ClipAnnounce, ClipChunk};

    use super::*;

    /// 变更计数与 watch 触发判定（docs/04 §3 变化方主动推送的触发源）。
    #[test]
    fn change_trigger_counts_writes() {
        let mut clipboard = MockClipboard::new();
        assert!(!clipboard.has_changes_since(0), "初始无变更");
        clipboard.set_text("hello 剪贴板");
        assert_eq!(clipboard.change_count(), 1);
        assert!(clipboard.has_changes_since(0));
        assert!(!clipboard.has_changes_since(1), "已消费的变更不再触发");
        clipboard.set_content(vec![1, 2, 3]);
        assert_eq!(clipboard.change_count(), 2);
        assert!(clipboard.has_changes_since(1));
        assert_eq!(clipboard.content(), &[1, 2, 3]);
    }

    /// 传输计划：分块 ≤ 线上安全长度（64KiB 数据上限 − postcard/帧头余量）、
    /// 拼接还原、blake3 与内容一致。
    #[test]
    fn transfer_plan_chunks_and_digest() {
        let mut clipboard = MockClipboard::new();
        let content: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        clipboard.set_content(content.clone());
        let plan = clipboard
            .transfer_plan(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
            .expect("未超限应有计划");
        assert_eq!(plan.total_len, 70_000);
        assert_eq!(plan.chunks.len(), 2, "70KB → 65472B + 4528B 两块");
        assert!(
            plan.chunks
                .iter()
                .all(|chunk| chunk.len() <= ClipChunk::MAX_DATA_LEN)
        );
        // 每块整帧编码必须可发出（u16 帧长物理上限内的自律自律分块）
        for chunk in &plan.chunks {
            let msg = ClipChunk {
                clip_id: 1,
                seq: 0,
                data: chunk.clone(),
            };
            assert!(msg.encode_frame(0).is_ok(), "整块帧编码可发出");
        }
        assert_eq!(plan.chunks.concat(), content);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(blake3::hash(&content).as_bytes());
        assert_eq!(plan.blake3, digest);
    }

    /// 超限不传输（docs/04 §3）：默认 16MiB 上限 +1B 拒绝；恰好上限可传；
    /// 协商缩限同样生效。
    #[test]
    fn oversize_content_refuses_to_transfer() {
        let mut clipboard = MockClipboard::new();
        clipboard.set_content(vec![0u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize + 1]);
        assert!(
            clipboard
                .transfer_plan(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
                .is_none(),
            "16MiB + 1B 超协议默认上限，不传输"
        );

        let mut at_limit = MockClipboard::new();
        at_limit.set_content(vec![7u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize]);
        let plan = at_limit
            .transfer_plan(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
            .expect("恰好上限可传输");
        assert_eq!(plan.total_len, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN);
        assert_eq!(
            plan.chunks.len(),
            257,
            "16MiB / 65472B = 256 块 + 余数块（线上安全分块）"
        );

        let mut small = MockClipboard::new();
        small.set_content(vec![0u8; 2048]);
        assert!(small.transfer_plan(1024).is_none(), "协商缩限同样拒绝");
    }

    /// 空内容 → 零块计划（announce total_len=0 + end 直接交付空内容）。
    #[test]
    fn empty_content_yields_empty_plan() {
        let mut clipboard = MockClipboard::new();
        clipboard.set_content(Vec::new());
        let plan = clipboard.transfer_plan(1024).expect("空内容未超限");
        assert_eq!(plan.total_len, 0);
        assert!(plan.chunks.is_empty());
    }
}
