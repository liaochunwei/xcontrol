//! 握手限速计数器（docs/02 §3.2 在线防爆破）。
//!
//! # 契约（docs/02 §3.2、07-security.md §3）
//!
//! - **失败退避**：per-peer 连续失败 ≥ `MAX_FAILURES`（5）次后进入指数退避——
//!   第 5 次失败后重试需等 `BASE_BACKOFF`（10s），此后每多失败一次翻倍，
//!   封顶 `MAX_BACKOFF`（10min）；主控侧键 = 来源 IP（M2/M3 绑定真实传输层，
//!   M1 回环以 peer id 作键）；
//! - **自我限速**：失败后 `MIN_ATTEMPT_INTERVAL`（2s）内的重试请求一律拒绝
//!   （从机侧"每候选 ≥2s/次"的自我约束；主控侧同样受益——快速重试不触网）；
//! - **窗口重置**：握手成功即清空该 peer 的计数与退避（`record_success`）；
//! - **无部分匹配反馈**：限速是本地判定（[`crate::NetError::RateLimited`]，不触网），
//!   被限速的对端只见连接不建立——与 docs/02 §0 WPS 教训一致。
//!
//! # 纯逻辑可测
//!
//! 时间由调用方以 [`Instant`] 注入（`admit`/`record_failure` 的 `now` 参数），
//! 不读系统时钟、不 sleep；测试用合成时刻验证退避序列与窗口重置。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 连续失败阈值：达到 `MAX_FAILURES`（5）次失败后进入退避（docs/02 §3.2）。
pub const MAX_FAILURES: u32 = 5;
/// 退避基数：第 5 次失败后的首次重试等待 10s（docs/02 §3.2）。
pub const BASE_BACKOFF: Duration = Duration::from_secs(10);
/// 退避封顶：10min（docs/02 §3.2）。
pub const MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);
/// 最小尝试间隔：失败后 2s 内的重试一律拒绝（docs/02 §3.2 从机自我限速 ≥2s/次）。
pub const MIN_ATTEMPT_INTERVAL: Duration = Duration::from_secs(2);

/// 限速参数（默认 = [`MAX_FAILURES`]/[`BASE_BACKOFF`]/[`MAX_BACKOFF`]/
/// [`MIN_ATTEMPT_INTERVAL`]；测试可缩小验证同一逻辑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleConfig {
    /// 连续失败阈值（达到即进入退避）。
    pub max_failures: u32,
    /// 退避基数。
    pub base_backoff: Duration,
    /// 退避封顶。
    pub max_backoff: Duration,
    /// 失败后最小重试间隔。
    pub min_interval: Duration,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            max_failures: MAX_FAILURES,
            base_backoff: BASE_BACKOFF,
            max_backoff: MAX_BACKOFF,
            min_interval: MIN_ATTEMPT_INTERVAL,
        }
    }
}

/// 限速判定结果（`admit` 的产出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// 允许发起握手。
    Allowed,
    /// 退避中（连续失败达阈值；`retry_after` = 距可重试还差多少）。
    Backoff {
        /// 建议等待时长。
        retry_after: Duration,
    },
    /// 距上次失败过近（自我限速；`retry_after` = 距最小间隔期满还差多少）。
    Cooldown {
        /// 建议等待时长。
        retry_after: Duration,
    },
}

/// per-peer 状态（`record_success` 后整体移除）。
#[derive(Debug, Clone, Copy, Default)]
struct PeerState {
    failures: u32,
    last_failure: Option<Instant>,
    /// 当前退避到期时刻（None = 不在退避中）。
    backoff_until: Option<Instant>,
}

/// per-peer 握手限速器（泛型键：M1 回环用 peer id；M2/M3 绑定来源 IP）。
#[derive(Debug)]
pub struct HandshakeThrottle<K: Eq + std::hash::Hash> {
    config: ThrottleConfig,
    peers: HashMap<K, PeerState>,
}

impl<K: Eq + std::hash::Hash> HandshakeThrottle<K> {
    /// 默认参数（docs/02 §3.2 常量）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(ThrottleConfig::default())
    }

    /// 自定义参数（测试缩小退避/间隔验证同一逻辑）。
    #[must_use]
    pub fn with_config(config: ThrottleConfig) -> Self {
        Self {
            config,
            peers: HashMap::new(),
        }
    }

    /// 判定 `peer` 在 `now` 时刻能否发起握手（纯读，不改变状态）。
    ///
    /// 判定序：退避中 → [`Admission::Backoff`]；距上次失败不足最小间隔 →
    /// [`Admission::Cooldown`]；否则 [`Admission::Allowed`]。
    /// 无失败史的 peer 永远 [`Admission::Allowed`]（合法新连接不受限）。
    #[must_use]
    pub fn admit(&self, peer: &K, now: Instant) -> Admission {
        let Some(state) = self.peers.get(peer) else {
            return Admission::Allowed;
        };
        if let Some(until) = state.backoff_until
            && now < until
        {
            return Admission::Backoff {
                retry_after: until.saturating_duration_since(now),
            };
        }
        if let Some(last) = state.last_failure {
            let interval_end = last + self.config.min_interval;
            if now < interval_end {
                return Admission::Cooldown {
                    retry_after: interval_end.saturating_duration_since(now),
                };
            }
        }
        Admission::Allowed
    }

    /// 记录一次失败握手：计数 +1；达到阈值起按 2^n 指数退避（封顶 [`MAX_BACKOFF`]）。
    pub fn record_failure(&mut self, peer: K, now: Instant) {
        let state = self.peers.entry(peer).or_default();
        state.failures = state.failures.saturating_add(1);
        state.last_failure = Some(now);
        if state.failures >= self.config.max_failures {
            // backoff_for 只取 config（字段级借用不相交，见函数文档）。
            let backoff = backoff_for(&self.config, state.failures);
            state.backoff_until = Some(now + backoff);
        }
    }

    /// 记录一次成功握手：清空该 peer 的全部状态（docs/02 §3.2 窗口重置）。
    pub fn record_success(&mut self, peer: &K) {
        self.peers.remove(peer);
    }

    /// 该 peer 当前连续失败计数（观测/测试用）。
    #[must_use]
    pub fn failure_count(&self, peer: &K) -> u32 {
        self.peers.get(peer).map_or(0, |state| state.failures)
    }

    /// 是否已无任何受限 peer（全部成功重置后为 true）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// 第 `failures` 次失败的退避时长：`base × 2^(failures - max)`，封顶 `max_backoff`。
/// 位移上限 16（基数 10s × 2^16 已远超封顶，防御性避免无谓大数）。
/// 独立函数（只取 `config`）：`record_failure` 内与 `peers` 可变借用保持字段级不相交。
fn backoff_for(config: &ThrottleConfig, failures: u32) -> Duration {
    let shift = failures.saturating_sub(config.max_failures).min(16);
    // shift ≤ 16 ⇒ checked_shl 恒成功；防御性 fallback 保持纯函数无 panic 路径。
    let factor = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
    config
        .base_backoff
        .saturating_mul(factor)
        .min(config.max_backoff)
}

impl<K: Eq + std::hash::Hash> Default for HandshakeThrottle<K> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// 合成时钟基准（测试内单调推进，不读系统时间）。
    fn tick(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    /// 默认参数下：前 4 次失败只触发最小间隔限速，第 5 次起进入 10s 退避。
    #[test]
    fn backoff_starts_at_fifth_failure() {
        let mut throttle: HandshakeThrottle<u32> = HandshakeThrottle::new();
        let t0 = Instant::now();
        assert_eq!(throttle.admit(&1, t0), Admission::Allowed, "无失败史不受限");

        // 4 次失败，每次间隔 >2s（避开 Cooldown）
        for i in 0..4_u32 {
            throttle.record_failure(1, tick(t0, u64::from(i) * 3));
            assert_eq!(throttle.failure_count(&1), i + 1);
        }
        assert_eq!(
            throttle.admit(&1, tick(t0, 12)),
            Admission::Allowed,
            "未达 5 次不退避"
        );

        // 第 5 次失败 → 退避 10s
        throttle.record_failure(1, tick(t0, 12));
        assert_eq!(
            throttle.admit(&1, tick(t0, 13)),
            Admission::Backoff {
                retry_after: Duration::from_secs(9)
            },
            "第 5 次失败后退避 10s 起（docs/02 §3.2）"
        );
        // 退避期内不随时间加速
        assert_eq!(
            throttle.admit(&1, tick(t0, 20)),
            Admission::Backoff {
                retry_after: Duration::from_secs(2)
            }
        );
        // 10s 期满（且 >2s 间隔）→ 允许重试
        assert_eq!(throttle.admit(&1, tick(t0, 22)), Admission::Allowed);
    }

    /// 指数翻倍：第 6/7 次失败退避 20s/40s，封顶 10min。
    #[test]
    fn backoff_doubles_and_caps() {
        let mut throttle: HandshakeThrottle<u32> = HandshakeThrottle::new();
        let t0 = Instant::now();
        // 5 次失败（间隔 3s）→ 退避 10s
        for i in 0..5_u32 {
            throttle.record_failure(7, tick(t0, u64::from(i) * 3));
        }
        assert_eq!(
            throttle.admit(&7, tick(t0, 13)),
            Admission::Backoff {
                retry_after: Duration::from_secs(9)
            },
            "失败 5：10s（第 5 次失败在 12s，退避至 22s）"
        );
        // 期满后再失败（第 6 次）→ 20s
        throttle.record_failure(7, tick(t0, 16));
        assert_eq!(
            throttle.admit(&7, tick(t0, 17)),
            Admission::Backoff {
                retry_after: Duration::from_secs(19)
            },
            "失败 6：20s（2× 基数）"
        );
        // 第 7 次 → 40s
        throttle.record_failure(7, tick(t0, 40));
        assert_eq!(
            throttle.admit(&7, tick(t0, 45)),
            Admission::Backoff {
                retry_after: Duration::from_secs(35)
            },
            "失败 7：40s"
        );
        // 大量失败后退避封顶 10min
        for i in 8..40_u32 {
            throttle.record_failure(7, tick(t0, u64::from(i) * 60));
        }
        let last = tick(t0, 39 * 60);
        assert_eq!(
            throttle.admit(&7, last + Duration::from_secs(599)),
            Admission::Backoff {
                retry_after: Duration::from_secs(10 * 60 - 599)
            },
            "退避封顶 10min（docs/02 §3.2）"
        );
    }

    /// 最小间隔自我限速：失败后 2s 内重试被拒（Cooldown），期满允许。
    #[test]
    fn min_interval_cooldown() {
        let mut throttle: HandshakeThrottle<u32> = HandshakeThrottle::new();
        let t0 = Instant::now();
        throttle.record_failure(1, t0);
        assert_eq!(
            throttle.admit(&1, t0 + Duration::from_millis(1500)),
            Admission::Cooldown {
                retry_after: Duration::from_millis(500)
            },
            "失败后 2s 内自我限速（docs/02 §3.2）"
        );
        assert_eq!(
            throttle.admit(&1, t0 + Duration::from_secs(2)),
            Admission::Allowed
        );
        // 1 次失败不进入退避（仅 Cooldown 生效）
        assert_eq!(throttle.failure_count(&1), 1);
    }

    /// 成功重置：清空计数与退避；新失败从 1 重新计。
    #[test]
    fn success_resets_window() {
        let mut throttle: HandshakeThrottle<u32> = HandshakeThrottle::new();
        let t0 = Instant::now();
        for i in 0..5_u32 {
            throttle.record_failure(9, tick(t0, u64::from(i) * 3));
        }
        assert!(!throttle.is_empty());
        assert!(matches!(
            throttle.admit(&9, tick(t0, 13)),
            Admission::Backoff { .. }
        ));
        throttle.record_success(&9);
        assert!(throttle.is_empty(), "成功即窗口重置（docs/02 §3.2）");
        assert_eq!(throttle.admit(&9, tick(t0, 14)), Admission::Allowed);
        assert_eq!(throttle.failure_count(&9), 0);
        // 重置后重新计数：再 4 次失败仍不退避
        for i in 0..4_u32 {
            throttle.record_failure(9, tick(t0, 20 + u64::from(i) * 3));
        }
        assert_eq!(throttle.admit(&9, tick(t0, 32)), Admission::Allowed);
        throttle.record_failure(9, tick(t0, 32));
        assert!(matches!(
            throttle.admit(&9, tick(t0, 34)),
            Admission::Backoff { .. }
        ));
    }

    /// peer 相互独立：A 被退避不影响 B（per-peer 键语义）。
    #[test]
    fn peers_are_independent() {
        let mut throttle: HandshakeThrottle<&str> = HandshakeThrottle::new();
        let t0 = Instant::now();
        for i in 0..5_u32 {
            throttle.record_failure("attacker", tick(t0, u64::from(i) * 3));
        }
        assert_eq!(
            throttle.admit(&"victim", t0 + Duration::from_secs(1)),
            Admission::Allowed
        );
        assert!(matches!(
            throttle.admit(&"attacker", t0 + Duration::from_secs(1)),
            Admission::Backoff { .. }
        ));
    }

    /// 缩小参数验证同一逻辑：阈值 2、基数 5s、封顶 6s、间隔 1s。
    #[test]
    fn custom_config_follows_same_logic() {
        let config = ThrottleConfig {
            max_failures: 2,
            base_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(6),
            min_interval: Duration::from_secs(1),
        };
        let mut throttle: HandshakeThrottle<u32> = HandshakeThrottle::with_config(config);
        let t0 = Instant::now();
        throttle.record_failure(3, t0);
        assert!(
            matches!(
                throttle.admit(&3, t0 + Duration::from_millis(500)),
                Admission::Cooldown { .. }
            ),
            "间隔 1s 内重试应被 Cooldown 拒绝"
        );
        throttle.record_failure(3, tick(t0, 1));
        assert_eq!(
            throttle.admit(&3, tick(t0, 2)),
            Admission::Backoff {
                retry_after: Duration::from_secs(4)
            },
            "自定义阈值 2 → 退避基数 5s（失败在 1s，退避至 6s）"
        );
        // 第 3 次失败：2× 基数 10s → 封顶 6s（失败在 8s，退避至 14s）
        throttle.record_failure(3, tick(t0, 8));
        assert_eq!(
            throttle.admit(&3, tick(t0, 9)),
            Admission::Backoff {
                retry_after: Duration::from_secs(5)
            },
            "退避封顶按自定义 max_backoff"
        );
    }

    /// 大失败数的位移防御：计数饱和、退避封顶不恐慌。
    #[test]
    fn extreme_failure_count_is_safe() {
        let mut throttle: HandshakeThrottle<u32> = HandshakeThrottle::new();
        let t0 = Instant::now();
        // 同一时刻连续 100 次失败（无重试，纯计数路径）
        for _ in 0..100_u32 {
            throttle.record_failure(11, t0);
        }
        assert_eq!(throttle.failure_count(&11), 100);
        assert!(matches!(
            throttle.admit(&11, t0 + Duration::from_secs(30)),
            Admission::Backoff { .. }
        ));
    }
}
