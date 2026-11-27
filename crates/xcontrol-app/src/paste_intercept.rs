//! `paste_intercept` — 透明文件粘贴拦截（FR-7.3 切片 c，docs/04 §8.4）。
//!
//! ```text
//! 从机注入管线（切片 c 后）：
//!   SessionAction::InjectInput
//!     → HotkeyFilterSink（热键吞）
//!     → MappingSink（规则改写）
//!     → PasteInterceptSink（本模块：粘贴组合 × pending 槽 → 挂起/放行）
//!     → 平台注入 sink
//! ```
//!
//! - **拦截点在自身注入路径**（KVM 架构天然钩子）：本机是受控方时，主控转发的
//!   粘贴组合经本包装器到达平台注入——pending 槽有货则挂起组合键、触发物化
//!   （拉流→暂存→挂板，复用切片 b 管线），挂板完成后由会话循环回注挂起键，
//!   目标应用粘到的就是物化后的本地文件（"像本地一样"）；零系统级 hook；
//! - **修饰键自跟踪**：wire Key 事件不携带 mods（修饰键以独立 Key 事件到达），
//!   包装器本地维护快照（inject 内对修饰键 down/up 增删）——粘贴组合 = V↓
//!   时快照含 META|CTRL 任一（mac `Cmd+V` / win·linux `Ctrl+V` 同判，docs/03
//!   §4 修饰键语义）；
//! - **单飞语义**：物化进行中（`materializing` 共享位）重复粘贴组合直接放行
//!   注入——不二次物化；
//! - **一次性消费**：挂板成功 → `consume_pending`（pending 槽清空，托盘
//!   「接收文件」随之消失）——随后的粘贴组合因无货放行，行为与无文件槽完全
//!   一致（回归保护，prd AC3）；
//! - **失败/超时**：挂起的键丢弃不注入 + bus 提示（prd R3——不得把挂起的键
//!   注入成错误行为；无货状态下用户手动重按即可）。
//!
//! 文本粘贴路径零改动：pending 无货时包装器逐字节透传（命中判定在 Key 事件
//! 的 V↓ 前置检查，非 V↓ 事件零开销）——NFR-1 输入延迟不回退。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use xcontrol_core::mods::ModifierTracker;
use xcontrol_core::sink::{InputSink, SinkError};
use xcontrol_proto::input::{InputEvent, KeyState, Mods, ScreenId};
use xcontrol_proto::xkey::KEY_V;

/// 粘贴组合修饰键掩码（META|CTRL 任一——mac Cmd+V / win·linux Ctrl+V 同判）。
const PASTE_MOD_MASK: u32 =
    (Mods::LMETA.bits() | Mods::RMETA.bits()) | (Mods::LCTRL.bits() | Mods::RCTRL.bits());

use crate::file_channel::lock_hub;

/// 物化等待超时：120s（覆盖 GB 级单文件在局域网的传输上限；超时 = 条目级
/// 失败未收敛——挂起的键丢弃，用户手动重试）。
pub const PASTE_MATERIALIZATION_TIMEOUT: Duration = Duration::from_secs(120);

/// 挂起的粘贴事件（物化完成后由会话循环回注；单事件——组合的修饰键已先行
/// 注入且目标 OS 仍保持按下态，只需补 V↓；V↑ 与修饰键 ↑ 照常透传）。
#[derive(Debug, Clone)]
pub struct HeldPaste {
    /// 挂起的 V↓ 事件（原样回注）。
    pub event: InputEvent,
    /// 拦截时的粘贴修饰键快照（回注重放——物化等待的数秒里用户可能已松开
    /// 物理 Meta/Ctrl，裸 V↓ = 单独 v 字母的幽灵键）。
    pub mods_snapshot: Mods,
}

impl HeldPaste {
    /// 回注的修饰键重放序列（拦截时快照里的粘贴修饰键 → Down 序列）。
    /// 物化等待的数秒里用户可能已松开物理修饰键——裸 V↓ = 单独 v 字母。
    #[must_use]
    pub fn modifier_replay(&self) -> Vec<(xcontrol_proto::xkey::XKey, KeyState)> {
        let mut out = Vec::new();
        for (mods_bit, key) in [
            (Mods::LMETA, xcontrol_proto::xkey::KEY_LEFT_META),
            (Mods::RMETA, xcontrol_proto::xkey::KEY_RIGHT_META),
            (Mods::LCTRL, xcontrol_proto::xkey::KEY_LEFT_CTRL),
            (Mods::RCTRL, xcontrol_proto::xkey::KEY_RIGHT_CTRL),
        ] {
            if self.mods_snapshot.contains(mods_bit) {
                out.push((key, KeyState::Down));
            }
        }
        out
    }
}

/// 物化结果（worker → 会话循环；Resume = 回注挂起键，Failed = 丢弃 + 提示）。
#[derive(Debug, Clone)]
pub enum PasteOutcome {
    /// 挂板成功：回注挂起的 V↓（此时 pending 已消费，透传直达平台 sink）。
    Resume(HeldPaste),
    /// 物化失败/超时：丢弃挂起键（原因上抛 bus 提示）。
    Failed {
        /// 失败原因（诊断文案）。
        reason: String,
    },
}

/// 单飞共享位（sink 包装器与物化 worker 各持一份 Arc——worker 收尾清位）。
pub type MaterializingFlag = Arc<AtomicBool>;

/// 新建单飞位（session_rt 装配点一处创建，双端共享）。
#[must_use]
pub fn new_materializing_flag() -> MaterializingFlag {
    Arc::new(AtomicBool::new(false))
}

/// 透明粘贴拦截 sink 包装器（会话循环持有；泛型内层 = 注入管线下游）。
pub struct PasteInterceptSink<S: InputSink> {
    inner: S,
    /// 本地修饰键跟踪（wire Key 无 mods——修饰键 down/up 在此应用；
    /// core 单一事实源 [`ModifierTracker`]，mods.rs）。
    tracker: ModifierTracker,
    /// 物化请求通道（拦截命中 → 会话循环 spawn 物化 worker）。
    job_tx: UnboundedSender<HeldPaste>,
    /// 单飞语义标志（worker 完成后清；intercept 检查非重入）。
    materializing: MaterializingFlag,
}

impl<S: InputSink> PasteInterceptSink<S> {
    /// 包装注入管线下游 sink。
    #[must_use]
    pub fn new(
        inner: S,
        job_tx: UnboundedSender<HeldPaste>,
        materializing: MaterializingFlag,
    ) -> Self {
        Self {
            inner,
            tracker: ModifierTracker::new(),
            job_tx,
            materializing,
        }
    }

    /// 粘贴修饰键判定：按下集合含 META|CTRL 任一（mac Cmd+V / win·linux
    /// Ctrl+V 同判——跨平台修饰键映射（docs/03 §6）作用于转发语义，到达
    /// 本机的组合已是本机形态）。
    #[must_use]
    fn paste_modifier_down(&self) -> bool {
        self.tracker.pressed().bits() & PASTE_MOD_MASK != 0
    }

    /// 拦截判定（prd R2）：V↓ + 粘贴修饰键按下 + pending 槽有货 + 协商
    /// minor ≥ 2 + 非物化中。
    #[must_use]
    fn should_intercept(&self) -> bool {
        let hub = lock_hub();
        hub.file_capable() && hub.pending().is_some() && !self.materializing.load(Ordering::Acquire)
    }
}

impl<S: InputSink> InputSink for PasteInterceptSink<S> {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        if let InputEvent::Key { code, state } = event {
            // 修饰键快照维护（core 单一事实源：apply_key 对修饰键 down/up
            // 增删、非修饰键无操作）。
            self.tracker.apply_key(*code, *state);
            // 粘贴组合 × 拦截判定（仅 V↓；其余键零开销透传）。
            if *code == KEY_V
                && *state == KeyState::Down
                && self.paste_modifier_down()
                && self.should_intercept()
            {
                // 单飞登记（worker 完成后清）；整单入拉取队列（复用切片 b 管线）。
                self.materializing.store(true, Ordering::Release);
                tracing::info!(target: "session::file", "【透明粘贴】拦截命中：挂起粘贴键并触发整单拉取");
                lock_hub().request_pull_all_pending();
                let _ = self.job_tx.send(HeldPaste {
                    event: event.clone(),
                    mods_snapshot: self.tracker.pressed(),
                });
                return Ok(()); // 挂起：不注入（物化完成后由会话循环回注）
            }
        }
        self.inner.inject(event)
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        self.inner.warp(screen, x, y)
    }

    fn release_all(&mut self) -> Result<(), SinkError> {
        // 释放序列（切出/暂停/断开防卡键）：修饰键快照随之清零（卡键面与
        // 快照面同源复位）；挂起键的回注由会话循环侧的 outcome 消息承担，
        // 通道随会话收尾自然失效（Resume 到达时 sink 已重建则照常注入）。
        self.tracker = ModifierTracker::new();
        self.inner.release_all()
    }

    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        self.inner.cursor_normalized()
    }

    fn resync_cursor(&mut self) -> Result<(), SinkError> {
        self.inner.resync_cursor()
    }
}

/// 物化等待（worker 主体，tokio 任务内调用）：100ms 拍对 hub 挂板指纹变化，
/// 超时上限 [`PASTE_MATERIALIZATION_TIMEOUT`]。
///
/// 返回 `true` = 挂板完成（回注挂起键）；`false` = 超时（清单飞位，挂起键
/// 丢弃——调用方上抛 bus 提示）。
pub async fn wait_materialized(baseline_mounted_fp: Option<[u8; 16]>) -> bool {
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // 首拍立即返回（interval 语义）
    let deadline = tokio::time::Instant::now() + PASTE_MATERIALIZATION_TIMEOUT;
    loop {
        if lock_hub().mounted_fp() != baseline_mounted_fp {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        ticker.tick().await;
    }
}

/// 单飞位清理（worker 收尾统一走此入口——共享 Arc，无 thread_local 面）。
pub fn clear_materializing(flag: &MaterializingFlag) {
    flag.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::unbounded_channel;
    use xcontrol_core::sink::InputSink;
    use xcontrol_proto::input::{InputEvent, KeyState};
    use xcontrol_proto::xkey::{KEY_A, KEY_LEFT_META, KEY_V};

    use super::*;
    use crate::file_channel::FILE_MIN_MINOR;

    /// 注入记录 sink（测试载体；非 mock-platform 依赖——本地小实现）。
    #[derive(Default)]
    struct VecSink {
        injected: Vec<InputEvent>,
    }

    impl InputSink for VecSink {
        fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
            self.injected.push(event.clone());
            Ok(())
        }

        fn warp(&mut self, _screen: ScreenId, _x: f32, _y: f32) -> Result<(), SinkError> {
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
    }

    fn key(code: xcontrol_proto::xkey::XKey, state: KeyState) -> InputEvent {
        InputEvent::Key { code, state }
    }

    fn slot_armed() {
        let mut hub = lock_hub();
        hub.reset_for_new_session();
        hub.set_session_minor(FILE_MIN_MINOR);
        hub.set_pending(xcontrol_core::file_ref::PendingSlot {
            transfer_id: 1,
            origin_id: [1; 16],
            origin_seq: 1,
            entries: vec![xcontrol_proto::FileEntry {
                name: "a.bin".to_owned(),
                size: 1,
                mtime_ms: 0,
                kind: xcontrol_proto::FileEntryKind::File,
                children: 0,
            }],
            total_bytes: 1,
            copied_at_ms: 0,
            fingerprint: [0; 16],
        });
    }

    fn reset_hub() {
        lock_hub().reset_for_new_session();
    }

    /// 命中面：Meta↓ → V↓（槽有货）= 挂起（不注入）+ 物化请求 + 队列入队；
    /// Meta↓ 本身透传。
    #[test]
    fn paste_combo_with_armed_slot_is_held() {
        let _guard = crate::test_support::HUB_TEST_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot_armed();
        let (job_tx, mut job_rx) = unbounded_channel();
        let flag = new_materializing_flag();
        let mut sink = PasteInterceptSink::new(VecSink::default(), job_tx, Arc::clone(&flag));

        sink.inject(&key(KEY_LEFT_META, KeyState::Down))
            .expect("Meta↓ 透传");
        sink.inject(&key(KEY_V, KeyState::Down))
            .expect("V↓ 挂起成功");
        // 挂起：注入日志无 V↓（Meta↓ 在）
        // （inner 已移入包装器——经 job/flag 断言替代直接观察）
        assert!(job_rx.try_recv().is_ok(), "物化请求已发出");
        assert!(flag.load(Ordering::Acquire), "单飞位已登记");
        let hub = lock_hub();
        assert_eq!(hub.pull_queue_len(), 1, "整单入拉取队列");
        drop(hub);
        reset_hub();
    }

    /// 无槽/无修饰键 → 透传（文本粘贴路径零改动，NFR-1 回归保护）。
    #[test]
    fn passthrough_without_slot_or_modifier() {
        let _guard = crate::test_support::HUB_TEST_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_hub(); // 无 pending
        let (job_tx, mut job_rx) = unbounded_channel();
        let flag = new_materializing_flag();
        let mut sink = PasteInterceptSink::new(VecSink::default(), job_tx, Arc::clone(&flag));

        sink.inject(&key(KEY_V, KeyState::Down))
            .expect("无修饰键 V↓ 透传");
        assert!(job_rx.try_recv().is_err(), "无槽不触发物化");
        assert!(!flag.load(Ordering::Acquire), "单飞位未登记");

        // 有修饰键但无槽：同样透传
        sink.inject(&key(KEY_LEFT_META, KeyState::Down))
            .expect("Meta↓");
        sink.inject(&key(KEY_V, KeyState::Down))
            .expect("无槽 V↓ 透传");
        assert!(job_rx.try_recv().is_err(), "无槽不触发物化（有修饰键）");
        reset_hub();
    }

    /// 单飞语义：物化中（标志为真）重复粘贴组合放行注入，不重复触发。
    #[test]
    fn materializing_flag_suppresses_retrigger() {
        let _guard = crate::test_support::HUB_TEST_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot_armed();
        let (job_tx, mut job_rx) = unbounded_channel();
        let flag = new_materializing_flag();
        flag.store(true, Ordering::Release); // 模拟物化进行中
        let mut sink = PasteInterceptSink::new(VecSink::default(), job_tx, Arc::clone(&flag));

        sink.inject(&key(KEY_LEFT_META, KeyState::Down))
            .expect("Meta↓");
        sink.inject(&key(KEY_V, KeyState::Down))
            .expect("物化中 V↓ 放行");
        assert!(job_rx.try_recv().is_err(), "单飞：不重复触发");
        reset_hub();
    }

    /// 修饰键快照维护：Up 释放后 V↓ 不再判为组合。
    #[test]
    fn modifier_release_clears_combo() {
        let _guard = crate::test_support::HUB_TEST_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot_armed();
        let (job_tx, mut job_rx) = unbounded_channel();
        let flag = new_materializing_flag();
        let mut sink = PasteInterceptSink::new(VecSink::default(), job_tx, Arc::clone(&flag));

        sink.inject(&key(KEY_LEFT_META, KeyState::Down))
            .expect("Meta↓");
        sink.inject(&key(KEY_LEFT_META, KeyState::Up))
            .expect("Meta↑");
        sink.inject(&key(KEY_V, KeyState::Down))
            .expect("释放后 V↓ 透传");
        assert!(job_rx.try_recv().is_err(), "修饰键已释放不构成组合");
        reset_hub();
    }

    /// Ctrl 变体（win/linux 形态）同判：Ctrl↓ + V↓ 命中。
    #[test]
    fn ctrl_variant_intercepts_too() {
        let _guard = crate::test_support::HUB_TEST_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot_armed();
        let (job_tx, mut job_rx) = unbounded_channel();
        let flag = new_materializing_flag();
        let mut sink = PasteInterceptSink::new(VecSink::default(), job_tx, Arc::clone(&flag));

        sink.inject(&key(xcontrol_proto::xkey::KEY_LEFT_CTRL, KeyState::Down))
            .expect("Ctrl↓");
        sink.inject(&key(KEY_V, KeyState::Down)).expect("V↓ 挂起");
        assert!(job_rx.try_recv().is_ok(), "Ctrl+V 同判命中");
        reset_hub();
    }

    /// 非 V↓ 键零开销透传（A↓ 任何状态不入拦截面）。
    #[test]
    fn non_paste_keys_pass_through() {
        let _guard = crate::test_support::HUB_TEST_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot_armed();
        let (job_tx, mut job_rx) = unbounded_channel();
        let flag = new_materializing_flag();
        let mut sink = PasteInterceptSink::new(VecSink::default(), job_tx, Arc::clone(&flag));

        sink.inject(&key(KEY_LEFT_META, KeyState::Down))
            .expect("Meta↓");
        sink.inject(&key(KEY_A, KeyState::Down)).expect("A↓ 透传");
        assert!(job_rx.try_recv().is_err(), "非粘贴键不触发");
        reset_hub();
    }
}
