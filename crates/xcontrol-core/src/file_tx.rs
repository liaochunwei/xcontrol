//! 文件域源端发送编排：条目流 + 信用窗口（docs/02 §4.6 防护规则 5；纯逻辑、零 I/O）。
//!
//! 与剪贴板 [`crate::clip_tx::ClipTx`] 的差异：文件域**每连接多 transfer、
//! 每 transfer 串行多条目**（单连接同时一条目流，多文件顺序服务），seq 为
//! u32（GB 级流），信用窗口初始 16 块。文件字节由 app 层按
//! [`crate::file_serve::FileServe`] 的 [`crate::file_serve::ServeVerdict::Grant`]
//! 读取；本模块只管「seq 分配 / 信用记账 / 已发字节累计 / 发满即 end」。
//!
//! blake3 摘要由 app 层读块时边流边算（docs/02 §4.6 防护规则 8：复制瞬间
//! 不预哈希），`FileEnd.blake3` 由 app 提供——本模块不做摘要组装。

/// 单连接信用窗口初始块数（docs/02 §4.6 防护规则 5）。
pub const INITIAL_FILE_CREDIT_CHUNKS: usize = 16;

/// 单条目流的发送状态（单连接同时一条目流）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryStream {
    transfer_id: u32,
    entry: u32,
    /// 条目尺寸（来自 serve Grant 的 remaining；拉全量时即条目 size）。
    size: u64,
    /// 已发送字节（含在途未确认——信用窗口保证接收端可承受）。
    bytes_sent: u64,
    /// 下一块 seq（u32 单调递增不回绕，docs/02 §4.6 防护规则 4）。
    next_seq: u32,
}

/// 源端发送编排（每条文件连接一个实例）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTx {
    window: usize,
    /// 已发送未确认的块 seq（FIFO 登记；上限即窗口大小）。
    unacked: Vec<u32>,
    stream: Option<EntryStream>,
}

impl FileTx {
    /// 默认窗口（16 块，docs/02 §4.6 防护规则 5）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_window(INITIAL_FILE_CREDIT_CHUNKS)
    }

    /// 自定义窗口（0 视为 1——恒允许至少一块在途，防死锁）。
    #[must_use]
    pub fn with_window(window: usize) -> Self {
        Self {
            window: window.max(1),
            unacked: Vec::new(),
            stream: None,
        }
    }

    /// 开始服务一条目（上一条目必须已收尾）。`size` 来自
    /// [`crate::file_serve::ServeVerdict::Grant`] 的 `remaining`。
    ///
    /// # Errors
    /// 上一条目未收尾（`should_end` 未确认）→ `Err(进行中条目描述)`。
    pub fn begin_entry(&mut self, transfer_id: u32, entry: u32, size: u64) -> Result<(), String> {
        if let Some(current) = &self.stream {
            return Err(format!(
                "上一条目未收尾：transfer {} entry {}（{}/{} 字节）",
                current.transfer_id, current.entry, current.bytes_sent, current.size
            ));
        }
        self.stream = Some(EntryStream {
            transfer_id,
            entry,
            size,
            bytes_sent: 0,
            next_seq: 0,
        });
        Ok(())
    }

    /// 是否可发送下一块（窗口未满且有条目流）。
    #[must_use]
    pub fn can_send(&self) -> bool {
        self.stream.is_some() && self.unacked.len() < self.window
    }

    /// 下一块 seq（组帧用；`on_chunk_sent(seq, len)` 的 `seq` 必须取自此）。
    #[must_use]
    pub fn next_seq(&self) -> Option<u32> {
        self.stream.as_ref().map(|stream| stream.next_seq)
    }

    /// 登记已发送块（占用一个信用并累计字节）。
    ///
    /// 调用方契约：发送前先以 [`FileTx::can_send`] 确认窗口有余量；
    /// `len` 为本块字节数（累计不得超条目 size——app 读块按 Grant 剩余量截断）。
    ///
    /// # Errors
    /// 无条目流 / 累计超 size → `Err`（调用方 bug，不产线上帧）。
    pub fn on_chunk_sent(&mut self, seq: u32, len: usize) -> Result<(), String> {
        let Some(stream) = &mut self.stream else {
            return Err("无进行中条目流".to_owned());
        };
        if seq != stream.next_seq {
            return Err(format!("seq 失序：期望 {} 收到 {seq}", stream.next_seq));
        }
        let new_len = stream.bytes_sent.saturating_add(len as u64);
        if new_len > stream.size {
            return Err(format!("累计发送 {new_len} 超条目 size {}", stream.size));
        }
        stream.bytes_sent = new_len;
        stream.next_seq = stream.next_seq.wrapping_add(1);
        self.unacked.push(seq);
        Ok(())
    }

    /// `FileAck` 归还信用；未知 / 重复 seq → `false`（信用不重复归还）。
    pub fn on_ack(&mut self, seq: u32) -> bool {
        match self.unacked.iter().position(|&sent| sent == seq) {
            Some(pos) => {
                self.unacked.remove(pos);
                true
            }
            None => false,
        }
    }

    /// 当前条目是否已发满（调用方此时产出 `FileEnd` 并以
    /// [`FileTx::take_stream`] 收尾）。
    #[must_use]
    pub fn should_end(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|stream| stream.bytes_sent == stream.size)
    }

    /// 收尾当前条目流（产出 `FileEnd` 后调用；信用窗口保留——跨条目连续记账）。
    ///
    /// # Errors
    /// 条目未发满 → `Err(当前进度)`。
    pub fn take_stream(&mut self) -> Result<(u32, u32), String> {
        match self.stream.take() {
            Some(stream) if stream.bytes_sent == stream.size => {
                Ok((stream.transfer_id, stream.entry))
            }
            other => {
                // 未发满：归还流状态（调用方修正后重试）
                self.stream = other;
                match &self.stream {
                    Some(stream) => Err(format!(
                        "条目未发满：{}/{} 字节",
                        stream.bytes_sent, stream.size
                    )),
                    None => Err("无进行中条目流".to_owned()),
                }
            }
        }
    }

    /// 用户取消 / 错误终止：丢弃条目流与在途记账（信用窗口同清——连接级
    /// 重建时由调用方整体重置）。
    pub fn reset(&mut self) {
        self.unacked.clear();
        self.stream = None;
    }

    /// 在途未确认块数。
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.unacked.len()
    }

    /// 窗口大小。
    #[must_use]
    pub const fn window(&self) -> usize {
        self.window
    }

    /// 当前条目流（诊断/测试）。
    #[must_use]
    pub fn stream(&self) -> Option<(u32, u32, u64, u64)> {
        self.stream.as_ref().map(|stream| {
            (
                stream.transfer_id,
                stream.entry,
                stream.bytes_sent,
                stream.size,
            )
        })
    }
}

impl Default for FileTx {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 条目流编排：begin → 发满 → take；窗口满后 can_send 阻塞直到 ack。
    #[test]
    fn entry_stream_lifecycle_and_credit_window() {
        let mut tx = FileTx::new();
        tx.begin_entry(7, 0, 100).unwrap();

        // 16 块窗口：每块 10B，10 块发满
        for seq in 0..10u32 {
            assert!(tx.can_send(), "窗口 16 远大于块数，第 {seq} 块应可发");
            tx.on_chunk_sent(seq, 10).unwrap();
        }
        assert!(tx.should_end(), "100/100 字节发满");
        let (transfer_id, entry) = tx.take_stream().unwrap();
        assert_eq!((transfer_id, entry), (7, 0));
        assert!(tx.stream().is_none(), "条目流已收尾");
        assert_eq!(tx.in_flight(), 10, "信用窗口跨条目保留，ack 前 10 块在途");

        // 信用归还应答后窗口回落
        for seq in 0..10u32 {
            assert!(tx.on_ack(seq));
        }
        assert_eq!(tx.in_flight(), 0);
    }

    /// 窗口阻塞：16 块在途后 can_send = false；ack 归还放行。
    #[test]
    fn credit_window_blocks_17th_chunk() {
        let mut tx = FileTx::new();
        tx.begin_entry(1, 0, 1000).unwrap();
        for seq in 0..16u32 {
            tx.on_chunk_sent(seq, 10).unwrap();
        }
        assert!(!tx.can_send(), "窗口满（16 块在途）");
        assert!(tx.on_ack(3));
        assert!(tx.can_send());
        tx.on_chunk_sent(16, 10).unwrap();
        assert!(!tx.can_send());
    }

    /// 防御面：无流发送、seq 失序、累计超 size、未发满 take、begin 重入。
    #[test]
    fn defensive_errors() {
        let mut tx = FileTx::new();
        assert!(tx.on_chunk_sent(0, 1).is_err(), "无流发送");

        tx.begin_entry(1, 0, 100).unwrap();
        assert!(tx.on_chunk_sent(1, 10).is_err(), "seq 失序（期望 0）");
        tx.on_chunk_sent(0, 10).unwrap();
        assert!(
            tx.begin_entry(2, 1, 50).is_err(),
            "begin 重入（上一条目进行中）"
        );
        assert!(
            tx.on_chunk_sent(1, 95).is_err(),
            "累计 105 > 100 超条目尺寸"
        );
        tx.on_chunk_sent(1, 90).unwrap();
        assert!(tx.should_end(), "100/100 发满");

        // 未发满 take 的负例：另起一条目发一半再 take
        let mut partial = FileTx::new();
        partial.begin_entry(3, 5, 100).unwrap();
        partial.on_chunk_sent(0, 40).unwrap();
        let err = partial.take_stream().unwrap_err();
        assert!(err.contains("未发满"), "{err}");
        assert!(partial.stream().is_some(), "take 失败后流保留");
    }

    /// 取消重置：条目流与在途记账同清，窗口恒可重用。
    #[test]
    fn reset_clears_stream_and_unacked() {
        let mut tx = FileTx::new();
        tx.begin_entry(1, 0, 100).unwrap();
        for seq in 0..16u32 {
            tx.on_chunk_sent(seq, 5).unwrap();
        }
        tx.reset();
        assert_eq!(tx.in_flight(), 0);
        assert!(tx.stream().is_none());
        assert!(!tx.can_send(), "无流时不可发送（窗口语义以流为前提）");
        tx.begin_entry(2, 3, 10).unwrap();
        assert!(tx.can_send(), "重置后新流窗口恢复");
        tx.on_chunk_sent(0, 10).unwrap();
        assert!(tx.should_end());
    }

    /// 自定义窗口；0 视为 1（恒允许一块在途，防死锁）。
    #[test]
    fn custom_window_minimum_one() {
        let tx = FileTx::with_window(2);
        assert_eq!(tx.window(), 2);
        let degenerate = FileTx::with_window(0);
        assert_eq!(degenerate.window(), 1);
    }
}
