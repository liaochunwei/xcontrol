//! 剪贴板发送端信用窗口（docs/02 §6.3；纯逻辑、零 I/O）。
//!
//! 初始 4 块信用：每发送一块 `ClipChunk` 占用一个信用（登记 seq），
//! 对端每条 `ClipAck{seq}` 归还一个。窗口空时发送端**等待**（不再产出
//! 新块），避免撑爆接收端重组缓冲（docs/02 §6.3「发送端窗口空则等待」）。
//!
//! 配套接收端状态机见 [`crate::clip_rx::ClipRx`]；两者由剪贴板连接接线层
//! 持有（M2 接入真实 TCP；M1 由 mock 回环集成测试全流程驱动）。

/// 单连接信用窗口初始块数（docs/02 §6.3）。
pub const INITIAL_CREDIT_CHUNKS: usize = 4;

/// 发送端信用窗口（单活动 clip；seq 由调用方分配并在此登记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipTx {
    window: usize,
    /// 已发送未确认的块 seq（FIFO 登记；上限即窗口大小）。
    unacked: Vec<u16>,
}

impl ClipTx {
    /// 默认窗口（4 块，docs/02 §6.3）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_window(INITIAL_CREDIT_CHUNKS)
    }

    /// 自定义窗口（0 视为 1——恒允许至少一块在途，防死锁）。
    #[must_use]
    pub fn with_window(window: usize) -> Self {
        Self {
            window: window.max(1),
            unacked: Vec::new(),
        }
    }

    /// 是否可发送下一块（窗口未满）。
    #[must_use]
    pub fn can_send(&self) -> bool {
        self.unacked.len() < self.window
    }

    /// 登记已发送块（占用一个信用）。
    ///
    /// 调用方契约：发送前先以 [`ClipTx::can_send`] 确认窗口有余量。
    pub fn on_chunk_sent(&mut self, seq: u16) {
        self.unacked.push(seq);
    }

    /// `ClipAck` 归还信用；未知 / 重复 seq → `false`（信用不重复归还）。
    pub fn on_ack(&mut self, seq: u16) -> bool {
        match self.unacked.iter().position(|&sent| sent == seq) {
            Some(pos) => {
                self.unacked.remove(pos);
                true
            }
            None => false,
        }
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
}

impl Default for ClipTx {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认窗口 4：第 5 块被窗口挡住；ack 归还信用后放行。
    #[test]
    fn credit_window_blocks_fifth_chunk() {
        let mut tx = ClipTx::new();
        assert_eq!(tx.window(), INITIAL_CREDIT_CHUNKS);
        for seq in 0..4u16 {
            assert!(tx.can_send(), "第 {seq} 块应有信用");
            tx.on_chunk_sent(seq);
        }
        assert!(!tx.can_send(), "窗口满（4 块在途）");
        assert_eq!(tx.in_flight(), INITIAL_CREDIT_CHUNKS);

        assert!(tx.on_ack(2), "ack 归还信用");
        assert!(tx.can_send());
        assert_eq!(tx.in_flight(), 3);
        tx.on_chunk_sent(4);
        assert!(!tx.can_send());

        // 全部归还后窗口清空
        for seq in [0u16, 1, 3, 4] {
            assert!(tx.on_ack(seq));
        }
        assert_eq!(tx.in_flight(), 0);
        assert!(tx.can_send());
    }

    /// 未知 / 重复 ack 不归还信用（信用不重复计）。
    #[test]
    fn unknown_and_duplicate_acks_rejected() {
        let mut tx = ClipTx::new();
        tx.on_chunk_sent(0);
        assert!(!tx.on_ack(7), "未知 seq");
        assert!(tx.on_ack(0));
        assert!(!tx.on_ack(0), "重复 ack 不二次归还");
        assert!(!tx.on_ack(0));
    }

    /// 自定义窗口；0 视为 1（恒允许一块在途，防死锁）。
    #[test]
    fn custom_window_minimum_one() {
        let mut tx = ClipTx::with_window(2);
        tx.on_chunk_sent(0);
        tx.on_chunk_sent(1);
        assert!(!tx.can_send());
        assert_eq!(tx.window(), 2);

        let mut degenerate = ClipTx::with_window(0);
        assert!(degenerate.can_send(), "0 视为 1，防死锁");
        degenerate.on_chunk_sent(0);
        assert!(!degenerate.can_send());
    }
}
