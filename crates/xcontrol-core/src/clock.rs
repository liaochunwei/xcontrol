//! 毫秒时钟注入（docs/02 §4.5 心跳语义的可测性前提）。
//!
//! core 红线禁 I/O——[`Clock`] 把"当前时刻"抽象为 trait，由上层注入实现：
//!
//! - 生产路径：[`SystemClock`]（`std::time::Instant` 单调毫秒）；
//! - 测试路径：[`TestClock`]（虚拟时钟；`Clone` 句柄**共享**同一读数，
//!   会话持一份、测试持一份，测试手动推进即驱动会话计时）。

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

/// 毫秒时钟（单调）。core 内所有计时（Ping 周期 / 判死阈值 / ModifierSync 周期）
/// 都经本 trait 读取，禁止直接触碰系统时间。
pub trait Clock {
    /// 当前时刻（相对毫秒；不要求绝对纪元，只要求单调递增）。
    fn now_ms(&self) -> u64;
}

/// 系统单调时钟（生产实现）。
#[derive(Debug)]
pub struct SystemClock {
    epoch: Instant,
}

impl SystemClock {
    /// 以当前时刻为零点。
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// 虚拟时钟（测试注入）：内部读数为 `Rc<Cell<u64>>`，**Clone 共享同一读数**，
/// 供"会话持一份 + 测试持一份"的双句柄推进模式（`Rc` ⇒ 单线程测试即可）。
#[derive(Debug, Clone)]
pub struct TestClock {
    now_ms: Rc<Cell<u64>>,
}

impl TestClock {
    /// 零点虚拟时钟。
    pub fn new() -> Self {
        Self {
            now_ms: Rc::new(Cell::new(0)),
        }
    }

    /// 向前推进 `ms` 毫秒（饱和累加，不回退；共享句柄全部可见）。
    pub fn advance(&self, ms: u64) {
        self.now_ms.set(self.now_ms.get().saturating_add(ms));
    }

    /// 直接置位（测试便捷；常规路径用 [`TestClock::advance`]）。
    pub fn set_ms(&self, now_ms: u64) {
        self.now_ms.set(now_ms);
    }

    /// 读数（不经 trait，测试断言用）。
    #[must_use]
    pub fn peek(&self) -> u64 {
        self.now_ms.get()
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TestClock：零点起点 + advance 饱和累加；Clone 句柄共享同一读数。
    #[test]
    fn test_clock_clones_share_and_advance_saturates() {
        let clock = TestClock::new();
        assert_eq!(clock.peek(), 0);
        let handle = clock.clone();
        clock.advance(1500);
        assert_eq!(handle.peek(), 1500, "Clone 共享读数");
        handle.advance(1);
        assert_eq!(clock.peek(), 1501);
        let maxed = TestClock::new();
        maxed.set_ms(u64::MAX);
        maxed.advance(100); // 饱和而非回绕
        assert_eq!(maxed.peek(), u64::MAX);
    }

    /// SystemClock：作为 [`Clock`] 两次读取单调不减。
    #[test]
    fn system_clock_is_monotonic() {
        let clock = SystemClock::new();
        let first = Clock::now_ms(&clock);
        let second = Clock::now_ms(&clock);
        assert!(second >= first);
    }
}
