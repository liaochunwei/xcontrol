//! 修饰键状态跟踪与差异计算（docs/03 §3「修饰键状态归属」不变量的纯逻辑层）。
//!
//! 不变量（docs/03 §3）：主控在**切入屏幕时**发送全量修饰键快照（`EnterScreen.mods`），
//! 从机注入前先对齐（缺的合成按下、多的合成抬起，防"卡键"）；切出时释放全部
//! （`LeaveScreen` → 本机全部释放）；周期 1s `ModifierSync` 全量校验（docs/02 §4.2 kind 0x12）。
//!
//! 本模块只提供无状态的位集运算与有状态的单侧跟踪（[`ModifierTracker`]）：
//! - 主控侧用一份 tracker 维护**本地物理修饰键**（EnterScreen/LeaveScreen/ModifierSync 的载荷来源）；
//! - 从机侧用另一份 tracker 维护**已注入修饰键**（对齐基准）。
//!
//! 合成事件序列由 [`ModifierTracker::sync_actions`] 产出（先释放多余、再按下缺失）。

use xcontrol_proto::input::{KeyState, Mods};
use xcontrol_proto::xkey::{
    KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_LEFT_SHIFT, KEY_RIGHT_ALT, KEY_RIGHT_CTRL,
    KEY_RIGHT_META, KEY_RIGHT_SHIFT, XKey,
};

/// mods 位序号（bit0-7，docs/02 §4.2）→ 修饰键 XKey 的固定表。
///
/// bit0-7 = LCtrl/LShift/LAlt/LMeta/RCtrl/RShift/RAlt/RMeta（与 XKey 0xE0–0xE7 一一对应）。
/// crate 内共享（mapping.rs 组合弦的位序展开复用同一张表，单一事实源）。
pub(crate) const MOD_KEYS: [XKey; 8] = [
    KEY_LEFT_CTRL,
    KEY_LEFT_SHIFT,
    KEY_LEFT_ALT,
    KEY_LEFT_META,
    KEY_RIGHT_CTRL,
    KEY_RIGHT_SHIFT,
    KEY_RIGHT_ALT,
    KEY_RIGHT_META,
];

/// 对齐差异：本机相对快照缺失/多余的修饰键。
///
/// - `missing`：快照有、本机未按下 → 需合成 `Down`；
/// - `extra`：本机按着、快照没有 → 需合成 `Up`（防卡键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModifierDiff {
    /// 快照有而本机缺的修饰键。
    pub missing: Mods,
    /// 本机有而快照没有的修饰键。
    pub extra: Mods,
}

impl ModifierDiff {
    /// 差异为空（两侧已一致）。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

/// 单侧修饰键状态跟踪（位集 + 事件应用）。
///
/// 用途二选一：主控本地物理修饰键快照，或从机已注入修饰键基准。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModifierTracker {
    pressed: Mods,
}

impl ModifierTracker {
    /// 空状态。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pressed: Mods::from_bits(0),
        }
    }

    /// 当前按下集合（快照载荷来源）。
    #[must_use]
    pub const fn pressed(&self) -> Mods {
        self.pressed
    }

    /// 整体覆写（周期同步/切入对齐后基准即快照）。
    pub fn set(&mut self, mods: Mods) {
        self.pressed = mods;
    }

    /// 全部清空（LeaveScreen/暂停/断开时释放所有已注入修饰键）。
    pub fn clear(&mut self) {
        self.pressed = Mods::default();
    }

    /// 应用一条修饰键 Key 事件（仅 `XKey::is_modifier()` 有效，其余忽略）。
    pub fn apply_key(&mut self, code: XKey, state: KeyState) {
        if let Some(bit) = code.mods_bit() {
            let one = Mods::from_key_bit(bit);
            self.pressed = match state {
                KeyState::Down => self.pressed.union(one),
                KeyState::Up => Mods::from_bits(self.pressed.bits() & !one.bits()),
            };
        }
    }

    /// 计算相对 `snapshot` 的对齐差异（不修改自身）。
    #[must_use]
    pub fn diff_from(&self, snapshot: Mods) -> ModifierDiff {
        let bits = self.pressed.bits();
        let snap = snapshot.bits();
        ModifierDiff {
            missing: Mods::from_bits(snap & !bits),
            extra: Mods::from_bits(bits & !snap),
        }
    }

    /// 生成对齐动作序列：**先释放 `extra`（Up），再按下 `missing`（Down）**，
    /// 同类内按 bit0→bit7 排序（确定性，便于测试与日志比对）。不修改自身。
    #[must_use]
    pub fn sync_actions(&self, snapshot: Mods) -> Vec<(XKey, KeyState)> {
        let diff = self.diff_from(snapshot);
        let mut actions = Vec::new();
        for (bit, key) in MOD_KEYS.iter().enumerate() {
            if diff.extra.contains(Mods::from_key_bit(bit as u8)) {
                actions.push((*key, KeyState::Up));
            }
        }
        for (bit, key) in MOD_KEYS.iter().enumerate() {
            if diff.missing.contains(Mods::from_key_bit(bit as u8)) {
                actions.push((*key, KeyState::Down));
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 八个修饰键逐位 Down/Up 往返，位集精确跟踪。
    #[test]
    fn tracks_all_eight_modifiers() {
        let mut tracker = ModifierTracker::new();
        let all = [
            Mods::LCTRL,
            Mods::LSHIFT,
            Mods::LALT,
            Mods::LMETA,
            Mods::RCTRL,
            Mods::RSHIFT,
            Mods::RALT,
            Mods::RMETA,
        ];
        for (key, m) in MOD_KEYS.iter().zip(all.iter()) {
            tracker.apply_key(*key, KeyState::Down);
            assert!(tracker.pressed().contains(*m), "bit 未登记: {m:?}");
        }
        assert_eq!(tracker.pressed(), Mods::from_bits(0xFF));
        // 依序抬起：位集精确收缩
        for (i, key) in MOD_KEYS.iter().enumerate() {
            tracker.apply_key(*key, KeyState::Up);
            let remaining = Mods::from_bits((0xFF << (i + 1)) & 0xFF);
            assert_eq!(tracker.pressed(), remaining);
        }
        assert!(tracker.pressed().is_empty());
    }

    /// 非修饰键（如 KEY_A）不影响位集；重复 Down 幂等。
    #[test]
    fn ignores_non_modifier_and_duplicate_down() {
        let mut tracker = ModifierTracker::new();
        tracker.apply_key(xcontrol_proto::xkey::KEY_A, KeyState::Down);
        assert!(tracker.pressed().is_empty());
        tracker.apply_key(xcontrol_proto::xkey::KEY_LEFT_META, KeyState::Down);
        tracker.apply_key(xcontrol_proto::xkey::KEY_LEFT_META, KeyState::Down);
        assert_eq!(tracker.pressed(), Mods::LMETA);
    }

    /// 切入对齐场景：本机按着 LALT（残留），快照 LCTRL → 释放 LALT、按下 LCTRL。
    #[test]
    fn enter_screen_aligns_stale_mods() {
        let mut injected = ModifierTracker::new();
        injected.apply_key(KEY_LEFT_ALT, KeyState::Down);
        let actions = injected.sync_actions(Mods::LCTRL);
        assert_eq!(
            actions,
            vec![
                (KEY_LEFT_ALT, KeyState::Up),
                (KEY_LEFT_CTRL, KeyState::Down)
            ]
        );
        // 应用后基准 = 快照
        injected.set(Mods::LCTRL);
        assert!(injected.diff_from(Mods::LCTRL).is_empty());
    }

    /// 周期同步场景：两侧一致 → 空动作序列；快照多出多位 → 按 bit 序合成 Down。
    #[test]
    fn periodic_sync_orders_deterministically() {
        let mut injected = ModifierTracker::new();
        injected.apply_key(KEY_LEFT_SHIFT, KeyState::Down);
        // 已一致：无动作
        assert!(injected.sync_actions(Mods::LSHIFT).is_empty());
        // 快照 LSHIFT|LMETA|RCTRL：missing 按 bit 升序（LMETA bit3 → RCTRL bit4）
        let snapshot = Mods::LSHIFT.union(Mods::LMETA).union(Mods::RCTRL);
        let actions = injected.sync_actions(snapshot);
        assert_eq!(
            actions,
            vec![
                (KEY_LEFT_META, KeyState::Down),
                (KEY_RIGHT_CTRL, KeyState::Down),
            ]
        );
    }

    /// 多余多位（本机按着快照没有的）→ 全部合成 Up，先于 missing 段。
    #[test]
    fn extras_released_before_missing() {
        let mut injected = ModifierTracker::new();
        injected.apply_key(KEY_RIGHT_META, KeyState::Down);
        injected.apply_key(KEY_LEFT_CTRL, KeyState::Down);
        let actions = injected.sync_actions(Mods::RSHIFT);
        assert_eq!(
            actions,
            vec![
                (KEY_LEFT_CTRL, KeyState::Up), // extra，bit0 先于 bit7
                (KEY_RIGHT_META, KeyState::Up),
                (KEY_RIGHT_SHIFT, KeyState::Down), // missing 段在后
            ]
        );
    }

    /// diff_from 纯计算：missing/extra 互斥且不修改自身。
    #[test]
    fn diff_from_is_pure() {
        let mut injected = ModifierTracker::new();
        injected.apply_key(KEY_LEFT_ALT, KeyState::Down);
        let diff = injected.diff_from(Mods::RMETA);
        assert_eq!(diff.missing, Mods::RMETA);
        assert_eq!(diff.extra, Mods::LALT);
        assert!(!diff.is_empty());
        assert_eq!(injected.pressed(), Mods::LALT); // 自身未变
        // 空对空
        assert!(ModifierTracker::new().diff_from(Mods::default()).is_empty());
    }

    /// 切出释放场景：clear 归零（会话层对应 ReleaseAll）。
    #[test]
    fn leave_screen_releases_all() {
        let mut injected = ModifierTracker::new();
        injected.apply_key(KEY_LEFT_META, KeyState::Down);
        injected.apply_key(KEY_RIGHT_ALT, KeyState::Down);
        injected.clear();
        assert!(injected.pressed().is_empty());
        assert!(injected.diff_from(Mods::default()).is_empty());
    }
}
