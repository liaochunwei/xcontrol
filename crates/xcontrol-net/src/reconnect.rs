//! 断线重连退避状态机（FR-5.3：3s 起指数退避、30s 封顶、成功重置、每轮刷新候选）。
//!
//! # 纯逻辑可测（throttle.rs 同形纪律）
//!
//! 时间由调用方以 [`std::time::Instant`] 注入（`record_failure`/`ready` 的
//! `now` 参数），本模块不读系统时钟、不 sleep——测试用合成时刻序列断言退避
//! 曲线（`tests::backoff_sequence_doubles_and_caps_at_30s` 等）。async 侧的
//! "睡到可重试"由消费方（app 的 `session_rt`）用 `tokio::time::sleep` 落实，
//! 睡眠时长即 [`ReconnectPlanner::wait_remaining`]。
//!
//! # 与 throttle 的分工（docs/02 §3.2 vs FR-5.3）
//!
//! - [`crate::throttle::HandshakeThrottle`]：**单次握手层**的在线防爆破退避
//!   （5 失败 → 10s 起退避封顶 10min，per-peer 键控，docs/02 §3.2）；
//! - [`ReconnectPlanner`]：**会话层**的断线重连节奏（3s 起、30s 封顶，
//!   FR-5.3——拔网线 30s 再插回 ≤10s 恢复的重连预算）。两层叠加生效：
//!   planner 决定"什么时候发起下一轮（发现→握手）"，throttle 决定"这一轮里
//!   对每个候选是否真的触网"。
//!
//! # 每轮刷新候选（docs/02 §2.2"重连期间每轮都先刷新候选"）
//!
//! planner 只管节奏；候选刷新是消费方循环的结构（每轮 sleep 醒来后重新
//! scan → 拿新候选），不进本模块——状态机保持纯时序，零候选快照持有。

use std::time::{Duration, Instant};

/// 退避基数：首次失败后等 3s（FR-5.3 的 1s 在两机实测中被重连风暴放大——
/// 大内容传输断连后 1s 重连与对端旧会话收尾窗口重叠，新旧连接帧交织；
/// 3s 给对端会话 teardown 留出余量，11-21 定稿）。
pub const RECONNECT_BASE: Duration = Duration::from_secs(3);
/// 退避封顶：30s（FR-5.3"上限 30s"；封顶后每轮固定 30s 节奏重试）。
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// 重连节奏参数（默认 = [`RECONNECT_BASE`]/[`RECONNECT_MAX`]；测试可缩小）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectConfig {
    /// 首次失败后的等待。
    pub base: Duration,
    /// 封顶。
    pub max: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            base: RECONNECT_BASE,
            max: RECONNECT_MAX,
        }
    }
}

/// 重连节奏状态机（FR-5.3）。消费循环：
///
/// ```text
/// loop {
///     if planner.ready(now) {
///         候选 = scan()（每轮刷新，docs/02 §2.2）;
///         match try_connect(候选) {
///             成功 → planner.reset(); 进入会话;
///             失败 → let wait = planner.record_failure(now); sleep(wait);
///         }
///     } else { sleep(planner.wait_remaining(now)); }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ReconnectPlanner {
    config: ReconnectConfig,
    /// 自上次成功以来的连续失败轮数。
    attempts: u32,
    /// 下次允许尝试的时刻（`None` = 立即可试：首次启动或已重置）。
    next_at: Option<Instant>,
}

impl ReconnectPlanner {
    /// 新 planner：首轮立即可试（启动时不停顿——网络本来就通的场景零等待）。
    #[must_use]
    pub const fn new(config: ReconnectConfig) -> Self {
        Self {
            config,
            attempts: 0,
            next_at: None,
        }
    }

    /// 自上次成功以来的连续失败轮数（诊断/测试锚点）。
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// `now` 是否已到可重试时刻（首轮/已重置 → 恒真）。
    #[must_use]
    pub fn ready(&self, now: Instant) -> bool {
        self.next_at.is_none_or(|at| now >= at)
    }

    /// 距可重试还差多少（已可试 → [`Duration::ZERO`]——消费方直接进入本轮）。
    #[must_use]
    pub fn wait_remaining(&self, now: Instant) -> Duration {
        match self.next_at {
            Some(at) => at.saturating_duration_since(now),
            None => Duration::ZERO,
        }
    }

    /// 记一次失败轮：连续失败 +1，排定下次时刻，返回应等待的时长
    /// （`base × 2^(n-1)`，饱和封顶 `max`——FR-5.3 序列 3s→6s→…→30s）。
    pub fn record_failure(&mut self, now: Instant) -> Duration {
        self.attempts = self.attempts.saturating_add(1);
        // 指数因子 2^(n-1)：n 超过 ~31 时 u32 溢出 → checked_pow 返回 None，
        // 用 u32::MAX 代入（saturating_mul + min 封顶兜住，等待值只会更大不会 panic）。
        let factor = u32::checked_pow(2, self.attempts.saturating_sub(1)).unwrap_or(u32::MAX);
        let wait = self.config.base.saturating_mul(factor).min(self.config.max);
        self.next_at = Some(now.checked_add(wait).unwrap_or(now));
        wait
    }

    /// 会话成功（连接建立）：清零连续失败与等待（FR-5.3"成功重置"）——
    /// 下次断线从 3s 重新爬坡。
    pub fn reset(&mut self) {
        self.attempts = 0;
        self.next_at = None;
    }
}

#[cfg(test)]
mod tests {
    // FR-5.3 验收对照（docs/00 §4.5"断线自动重连：指数退避（3s 起，上限 30s；11-21 实测定稿），
    // 网络恢复后 ≤10s 重连"；PRD 验收标准）：`backoff_sequence_doubles_and_caps_at_30s`
    // 钉死 1s→2s→…→30s 封顶序列，`success_resets_sequence` 钉死成功重置；
    // 封顶 30s 意味着断线恢复后至多等一个 30s 轮（AC"拔网线 30s 再插回，自动
    // 恢复"的时序预算），重连实际发生由 app 双进程演练日志证明。
    use std::time::Duration;

    use super::*;

    /// t0 起点的合成时刻（测试零点）。
    fn t0() -> Instant {
        Instant::now()
    }

    /// FR-5.3 序列：1s→2s→4s→8s→16s→30s（第 6 轮封顶）→30s…（封顶保持）。
    #[test]
    fn backoff_sequence_doubles_and_caps_at_30s() {
        let mut planner = ReconnectPlanner::new(ReconnectConfig::default());
        let t = t0();
        let expect = [
            Duration::from_secs(3),
            Duration::from_secs(6),
            Duration::from_secs(12),
            Duration::from_secs(24),
            Duration::from_secs(30),
            Duration::from_secs(30), // 32s → 封顶 30s
            Duration::from_secs(30), // 保持封顶
            Duration::from_secs(30),
        ];
        let mut at = t;
        for (round, wait) in expect.iter().enumerate() {
            assert_eq!(planner.record_failure(at), *wait, "第 {} 轮失败", round + 1);
            at += *wait;
        }
        assert_eq!(planner.attempts(), 8);
    }

    /// 成功重置：爬坡后成功 → 下次失败重新从 1s 开始（FR-5.3 验收"成功重置"；11-21 基数 3s）。
    #[test]
    fn success_resets_sequence() {
        let mut planner = ReconnectPlanner::new(ReconnectConfig::default());
        let t = t0();
        assert_eq!(planner.record_failure(t), Duration::from_secs(3));
        assert_eq!(planner.record_failure(t), Duration::from_secs(6));
        planner.reset();
        assert_eq!(planner.attempts(), 0);
        assert_eq!(
            planner.record_failure(t),
            Duration::from_secs(3),
            "重置后从基数重新爬坡"
        );
    }

    /// 首轮零等待：新 planner / 刚重置的 planner 立即可试（启动不睡）。
    #[test]
    fn fresh_planner_is_immediately_ready() {
        let mut planner = ReconnectPlanner::new(ReconnectConfig::default());
        let t = t0();
        assert!(planner.ready(t));
        assert_eq!(planner.wait_remaining(t), Duration::ZERO);

        planner.record_failure(t);
        assert!(!planner.ready(t), "失败后进入等待");
        assert_eq!(planner.wait_remaining(t), Duration::from_secs(3));
        assert!(planner.ready(t + Duration::from_secs(3)), "边界时刻可试");
        assert!(planner.ready(t + Duration::from_secs(4)), "越界时刻可试");
        planner.reset();
        assert!(planner.ready(t), "重置后立即恢复");
    }

    /// 等待剩余量随时间衰减（消费方据此 sleep：不满一轮时醒来继续等）。
    #[test]
    fn wait_remaining_decays() {
        let mut planner = ReconnectPlanner::new(ReconnectConfig::default());
        let t = t0();
        planner.record_failure(t); // 排定 t+3s
        assert_eq!(
            planner.wait_remaining(t + Duration::from_millis(750)),
            Duration::from_millis(2_250)
        );
        assert_eq!(
            planner.wait_remaining(t + Duration::from_secs(10)),
            Duration::ZERO,
            "已过期 → 零等待"
        );
    }

    /// 缩小配置的同型行为（测试可缩参数，throttle.rs 同形纪律）：
    /// base 10ms、max 40ms → 10/20/40/40。
    #[test]
    fn config_scaling_preserves_shape() {
        let cfg = ReconnectConfig {
            base: Duration::from_millis(10),
            max: Duration::from_millis(40),
        };
        let mut planner = ReconnectPlanner::new(cfg);
        let t = t0();
        for wait in [
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(40),
            Duration::from_millis(40),
        ] {
            assert_eq!(planner.record_failure(t), wait);
        }
    }

    /// 极端失败计数不 panic（saturating / checked 路径）：爬坡到封顶后，
    /// 万次失败全部保持 30s。
    #[test]
    fn saturates_under_extreme_repetition() {
        let cfg = ReconnectConfig {
            base: Duration::from_secs(3),
            max: Duration::from_secs(30),
        };
        let mut planner = ReconnectPlanner::new(cfg);
        let t = t0();
        // 先正常爬坡到封顶（3,6,12,24,30）。
        while planner.record_failure(t) < Duration::from_secs(30) {}
        // 极深失败：u32::checked_pow 溢出路径 → 因子 u32::MAX → 仍封顶。
        for round in 0..10_000 {
            let wait = planner.record_failure(t);
            assert_eq!(wait, Duration::from_secs(30), "第 {round} 轮极深失败仍封顶");
        }
        assert_eq!(planner.attempts(), 10_005);
    }
}
