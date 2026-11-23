//! 全局热键模型与匹配纯逻辑（docs/03 §4.6，FR-2.5）——映射引擎**之前**的前置过滤层。
//!
//! # 流水线位置（docs/03 §4.1 修订）
//!
//! ```text
//! 主控：捕获流 ──► [本模块 HotkeyFilter] ──命中──► 本地路由（hotkey_rt，不转发）
//!                        │ 未命中
//!                        ▼                    从机：收到的流 ──► [HotkeyFilter]
//!                    转发（ForwardInput）          │ 命中 ──► 本地路由（不注入本机 OS）
//!                                                  ▼ 未命中 ──► 映射引擎（mapping.rs）→ 注入
//! ```
//!
//! 热键匹配用**原始 XKey**（映射之前，docs/03 §4.6）——任意预设下热键行为一致；
//! 引擎只改写已转发的原始键码（`MappingEngine` 与本模块零耦合）。
//!
//! # 匹配语义（design.md 裁定，钉死并测试）
//!
//! - **修饰键「任一侧命中」**：docs 用 `Ctrl+Alt` 泛指两侧（用户不区分左右手）——
//!   配置写 `Ctrl` 即左/右 Ctrl 任一按下都满足；[`ModFamilies`] 把左右手位集折叠
//!   为四家族位（Ctrl/Shift/Alt/Meta），家族语义在匹配层生效、不影响事件流的
//!   左右手保真（快照与转发仍是逐键位集，docs/03 §3）。
//! - **精确集合**：未列出的修饰键家族按下 → 不命中（`Ctrl+Alt+Right` 不会在
//!   `Ctrl+Alt+Shift+Right` 时触发）——与 keymap 规则的 `trigger.mods` 精确语义
//!   一致（docs/03 §4.1）。
//! - **仅主键 Down 匹配**：修饰键事件与主键 Up 不触发（避免重复路由）；主键为
//!   修饰键的组合在 config 校验层拒绝（`crate::config validate` 同纪律）。
//! - **同组合先到先得**：多键配置成同一组合时按 [`HotkeySet`] 条目序（config
//!   字段序 next/prev/back_to_host/pause）取首个命中。
//! - **命中吞键范围**：主键 Down（路由）+ 同键 Up/KeyRepeat（尾巴，防对端收到
//!   无 Down 的 Up）；组合的修饰键事件**照常转发**——命中的判定点在主键 Down，
//!   修饰键先于判定点已转发，事后追回需要拦截缓存（复杂度不成比例）；对端收到的
//!   「修饰键按下又抬起、无主键」序列无注入副作用（无键可注）。XKey 空间左右手
//!   分键（0xE0–0xE7），转发语义不受家族折叠影响。
//!
//! # 平台注册路径对照（docs/05 §2/§3 热键行 + §5 优先级，M4 裁定）
//!
//! | 平台 | 主控检测 | 从机检测 |
//! | --- | --- | --- |
//! | mac | 捕获流识别（CaptureSource + [`HotkeyFilter`]，PRD 裁定不用 RegisterEventHotKey） | 收到的流注入前过滤（本模块）；本机物理热键 M4 无注册路径（移交 M5） |
//! | win | RegisterHotKey（消息窗口，WM_HOTKEY）+ 捕获流过滤双保险 | 收到的流注入前过滤 + RegisterHotKey（本机物理热键） |
//! | x11 | grab_key 被动抓取（服务器精确匹配 = 本模块「精确」语义的 OS 层实现） | 同左（两端注册）+ 收到的流注入前过滤 |

use xcontrol_config::HotkeysConfig;
use xcontrol_proto::input::{InputEvent, KeyState, Mods};
use xcontrol_proto::xkey::XKey;

use crate::mods::ModifierTracker;

/// 热键动作标识（docs/03 §4.6 四默认键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyId {
    /// 切到下一台（默认 `Ctrl+Alt+Right`）。
    Next,
    /// 切到上一台（默认 `Ctrl+Alt+Left`）。
    Prev,
    /// 回主控（默认 `Ctrl+Alt+Home`；config 字段 `back_to_host`）。
    BackToHost,
    /// 暂停/恢复 toggle（默认 `Ctrl+Alt+P`）。
    Pause,
}

impl HotkeyId {
    /// 全量（config 字段序；[`HotkeySet::from_config`] 条目序与此一致）。
    pub const ALL: [HotkeyId; 4] = [
        HotkeyId::Next,
        HotkeyId::Prev,
        HotkeyId::BackToHost,
        HotkeyId::Pause,
    ];

    /// 日志/UI 标签（config 字段名同源）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            HotkeyId::Next => "next",
            HotkeyId::Prev => "prev",
            HotkeyId::BackToHost => "back_to_host",
            HotkeyId::Pause => "pause",
        }
    }
}

/// 修饰键家族集合（左右手折叠的四位掩码；匹配语义「任一侧命中 + 精确集合」）。
///
/// 位序：bit0=Ctrl、bit1=Shift、bit2=Alt、bit3=Meta（Super 并入 Meta——XKey 空间
/// 两者同为 0xE3/0xE7 语义键，config 校验层同表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModFamilies(u8);

impl ModFamilies {
    /// Ctrl（LCtrl|RCtrl 任一）。
    pub const CTRL: Self = Self(1 << 0);
    /// Shift（LShift|RShift 任一）。
    pub const SHIFT: Self = Self(1 << 1);
    /// Alt（LAlt|RAlt 任一）。
    pub const ALT: Self = Self(1 << 2);
    /// Meta/Super（LMeta|RMeta 任一）。
    pub const META: Self = Self(1 << 3);

    /// 原始位（诊断/测试断言用）。
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// 左右手修饰键位集（docs/02 §4.2 mods bit0-7）→ 家族折叠：同族任一侧按下
    /// 即置位（「任一侧命中」的折叠点）。
    #[must_use]
    pub const fn from_mods(mods: Mods) -> Self {
        let bits = mods.bits();
        let ctrl = Mods::LCTRL.bits() | Mods::RCTRL.bits();
        let shift = Mods::LSHIFT.bits() | Mods::RSHIFT.bits();
        let alt = Mods::LALT.bits() | Mods::RALT.bits();
        let meta = Mods::LMETA.bits() | Mods::RMETA.bits();
        Self(
            (((bits & ctrl) != 0) as u8)
                | ((((bits & shift) != 0) as u8) << 1)
                | ((((bits & alt) != 0) as u8) << 2)
                | ((((bits & meta) != 0) as u8) << 3),
        )
    }

    /// 并集。
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// 是否包含 `other` 的全部家族位。
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// 一条热键组合（修饰键家族集合 + 主键；主键必非修饰键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyCombo {
    /// 修饰键家族（精确匹配：未列出的家族按下则不命中）。
    pub families: ModFamilies,
    /// 主键（原始 XKey；映射前的判定键）。
    pub key: XKey,
}

/// 热键集合：四动作 → 组合的有序表（config `[hotkeys]` 解析产物）。
///
/// 条目序 = [`HotkeyId::ALL`]（config 字段序 next/prev/back_to_host/pause）；
/// 同组合多键时先命中先得（match_raw 按序扫描）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeySet {
    entries: Vec<(HotkeyId, HotkeyCombo)>,
}

/// 热键配置解析错误（error-handling.md：可编程区分、无安全语义）。
///
/// config 加载期校验（`xcontrol_config::validate::validate_hotkey`）先于本错误
/// 报出同型问题并附字段路径；本错误防御绕过校验的构造路径（如测试直连）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HotkeyError {
    /// 组合串格式非法（空段/无修饰键/主键为修饰键/未知修饰键/重复修饰键）。
    #[error("热键 {field} 格式非法：{reason}")]
    Invalid {
        /// config 字段路径（如 `hotkeys.next`）。
        field: &'static str,
        /// 非法原因（中文，供日志/UI）。
        reason: String,
    },
    /// 主键名无法解析为 XKey（docs/03 §4.1 键名文法之外的名字）。
    #[error("热键 {field} 主键 {key:?} 无法解析为 XKey")]
    UnknownKey {
        /// config 字段路径。
        field: &'static str,
        /// 无法解析的键名。
        key: String,
    },
}

/// config 修饰键家族名 → 家族位（`xcontrol_config::validate` 的
/// `HOTKEY_MODIFIERS` 同表：Ctrl/Alt/Shift/Meta/Super）。
fn family_from_name(name: &str) -> Option<ModFamilies> {
    match name {
        "Ctrl" => Some(ModFamilies::CTRL),
        "Shift" => Some(ModFamilies::SHIFT),
        "Alt" => Some(ModFamilies::ALT),
        "Meta" | "Super" => Some(ModFamilies::META),
        _ => None,
    }
}

impl HotkeySet {
    /// 从 config `[hotkeys]` 四字段解析（字段序 = [`HotkeyId::ALL`]）。
    ///
    /// 解析规则与 config 校验对齐：`修饰键+主键`（`+` 分隔、去空白）；
    /// 修饰键限家族名（Ctrl/Alt/Shift/Meta/Super——「任一侧」语义无左右手形态，
    /// 左右手细分规格属后续扩展，docs/03 §4.6）；主键经
    /// `xcontrol_config::parse_key_name`（键名文法全表）且必非修饰键。
    ///
    /// # Errors
    /// [`HotkeyError`]（字段路径随错误返回；加载期 `validate` 已拦截同型问题，
    /// 此处防御绕过校验的构造路径）。
    pub fn from_config(config: &HotkeysConfig) -> Result<Self, HotkeyError> {
        let fields = [
            (HotkeyId::Next, "hotkeys.next", config.next.as_str()),
            (HotkeyId::Prev, "hotkeys.prev", config.prev.as_str()),
            (
                HotkeyId::BackToHost,
                "hotkeys.back_to_host",
                config.back_to_host.as_str(),
            ),
            (HotkeyId::Pause, "hotkeys.pause", config.pause.as_str()),
        ];
        let mut entries = Vec::with_capacity(fields.len());
        for (id, field, raw) in fields {
            let combo = parse_combo(field, raw)?;
            entries.push((id, combo));
        }
        Ok(Self { entries })
    }

    /// 空集合（无任何热键；平台注册路径的「未配置」形态）。
    #[must_use]
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 查询某动作的组合（平台注册路径读用；未配置 → `None`）。
    #[must_use]
    pub fn combo(&self, id: HotkeyId) -> Option<HotkeyCombo> {
        self.entries
            .iter()
            .find(|(entry_id, _)| *entry_id == id)
            .map(|(_, combo)| *combo)
    }

    /// 条目数（测试/诊断）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空集合。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 原始（映射前）语义匹配：`mods` = 当前按下修饰键快照（左右手位集，
    /// docs/02 §4.2 bit0-7），`key` = 本次事件主键。
    ///
    /// 匹配条件（全满足）：`key` 非修饰键且等于条目主键；家族位**精确相等**
    /// （列出的家族任一侧命中、未列出的家族一个都没按）。按条目序取首个。
    ///
    /// 调用方约定：仅对 `KeyState::Down` 的非修饰键事件调用（Up/KeyRepeat 由
    /// [`HotkeyFilter`] 的尾巴机制承接）。
    #[must_use]
    pub fn match_raw(&self, mods: Mods, key: XKey) -> Option<HotkeyId> {
        if key.is_modifier() {
            return None;
        }
        let pressed = ModFamilies::from_mods(mods);
        self.entries
            .iter()
            .find_map(|(id, combo)| (combo.key == key && combo.families == pressed).then_some(*id))
    }
}

/// 单条组合串解析（`修饰键+修饰键+主键`；规则见 [`HotkeySet::from_config`]）。
fn parse_combo(field: &'static str, raw: &str) -> Result<HotkeyCombo, HotkeyError> {
    let value = raw.trim();
    let invalid = |reason: String| HotkeyError::Invalid { field, reason };
    if value.is_empty() {
        return Err(invalid(
            "热键不能为空（应为 修饰键+主键，如 Ctrl+Alt+Right）".into(),
        ));
    }
    let parts: Vec<&str> = value.split('+').map(str::trim).collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(invalid(format!(
            "存在空段（多余或缺失的 '+'；当前 {value:?}）"
        )));
    }
    let Some((main_name, modifier_names)) = parts.split_last() else {
        return Err(invalid("热键不能为空".into()));
    };
    let mut families = ModFamilies::default();
    for name in modifier_names {
        let Some(family) = family_from_name(name) else {
            return Err(invalid(format!(
                "未知修饰键 {name:?}（可用：Ctrl、Alt、Shift、Meta、Super——家族名泛指左右任一侧）"
            )));
        };
        if families.contains(family) {
            return Err(invalid(format!("重复的修饰键 {name:?}")));
        }
        families = families.union(family);
    }
    if families.bits() == 0 {
        return Err(invalid(format!(
            "必须包含修饰键（仅主键 {main_name:?} 无法与普通输入区分）"
        )));
    }
    let Some(raw_code) = xcontrol_config::parse_key_name(main_name) else {
        return Err(HotkeyError::UnknownKey {
            field,
            key: (*main_name).to_owned(),
        });
    };
    let Some(key) = XKey::new(raw_code) else {
        return Err(invalid(format!("主键 {main_name:?} 落在 XKey 保留区")));
    };
    if key.is_modifier() {
        return Err(invalid(format!(
            "热键主键不能是修饰键（当前 {main_name:?}）"
        )));
    }
    Ok(HotkeyCombo { families, key })
}

/// 流过滤结果（[`HotkeyFilter::feed`] 的产出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOutcome {
    /// 未命中/无关事件：原样转发（主控转发 / 从机照常注入）。
    Forward,
    /// 命中热键（仅主键 Down 事件产出）：该事件不转发，调用方路由 [`HotkeyId`]。
    Hit(HotkeyId),
    /// 命中键的尾巴（同键 Up/KeyRepeat）：吞下不转发、不产出新路由。
    HitTail,
}

/// 热键流过滤器：修饰键跟踪 + [`HotkeySet`] 匹配的有状态机（双端复用）。
///
/// 输入 = 捕获流（主控）或收到的注入前流（从机）的 [`InputEvent`] 序列；
/// 输出 = 转发/命中/吞尾。会话重置（ReleaseAll/切出/暂停）时调用
/// [`HotkeyFilter::reset`] 对齐释放语义（修饰键跟踪与吞尾一并清零）。
///
/// 运行中会话的热键改动（docs/06 §4.1 即时生效行）经 [`HotkeyFilter::
/// set_hotkeys`] 原位替换匹配表——被动捕获架构（mac CGEventTap ListenOnly /
/// x11 XRecord 全量捕获 + 本模块匹配）无 OS 注册语义，替换即生效。
#[derive(Debug)]
pub struct HotkeyFilter {
    set: HotkeySet,
    /// 流内修饰键跟踪（匹配快照来源；仅 Key 事件驱动）。
    tracker: ModifierTracker,
    /// 命中后待吞尾巴的主键（Up 到达即清；KeyRepeat 连吞）。
    swallowing: Option<XKey>,
}

impl HotkeyFilter {
    /// 以热键集合构建（空集合 = 恒 Forward 的直通过滤器）。
    #[must_use]
    pub fn new(set: HotkeySet) -> Self {
        Self {
            set,
            tracker: ModifierTracker::new(),
            swallowing: None,
        }
    }

    /// 热键集合（平台注册路径复用同一份配置）。
    #[must_use]
    pub const fn set(&self) -> &HotkeySet {
        &self.set
    }

    /// 热替换匹配表（运行中会话收到 `RuntimeConfigUpdate` 的路径，docs/06 §4.1
    /// 热键行——即时生效）。
    ///
    /// 替换语义（注释钉死）：
    /// - **匹配表立即生效**：其后所有 Down 判定按新表执行；
    /// - **修饰键跟踪保留**：`tracker` 反映物理键盘当前按下态，与新集合无关
    ///   ——替换瞬间正按着的修饰键快照必须延续（否则新表首匹配会丢修饰键）；
    /// - **吞尾状态保留**：旧表命中主键的 Up/KeyRepeat 尾巴继续吞到物理释放
    ///   为止——防「Down 已吞、Up 却按新表转发」产生无 Down 的孤立注入；
    ///   新表不含该组合时，尾巴随物理 Up 自然清零（下一段该键恢复正常）。
    pub fn set_hotkeys(&mut self, set: HotkeySet) {
        self.set = set;
    }

    /// 修饰键跟踪快照（诊断/测试）。
    #[must_use]
    pub fn pressed_mods(&self) -> Mods {
        self.tracker.pressed()
    }

    /// 喂入一条事件 → 转发或吞（语义见模块文档「命中吞键范围」）。
    pub fn feed(&mut self, event: &InputEvent) -> FilterOutcome {
        match event {
            InputEvent::Key { code, state } => {
                if code.is_modifier() {
                    // 修饰键恒转发（跟踪 + 放行——命中组合的修饰键先于判定点已转发，
                    // 见模块文档；从机侧同理：修饰键事件照常进映射引擎的事件级路径）。
                    self.tracker.apply_key(*code, *state);
                    return FilterOutcome::Forward;
                }
                if self.swallowing == Some(*code) {
                    // 吞尾期：主键 Up 清状态；按住期间的重复 Down（win/x11 LL 捕获
                    // 长按恒产 Down，无 autorepeat 区分）与 mac KeyRepeat 一并吞且
                    // **不重复路由**——防按住热键连环切换；释放（Up）后再次按下才
                    // 重新命中（见下方 Down 匹配分支）。
                    if *state == KeyState::Up {
                        self.swallowing = None;
                    }
                    return FilterOutcome::HitTail;
                }
                if *state == KeyState::Down
                    && let Some(id) = self.set.match_raw(self.tracker.pressed(), *code)
                {
                    self.swallowing = Some(*code);
                    return FilterOutcome::Hit(id);
                }
                FilterOutcome::Forward
            }
            InputEvent::KeyRepeat { code } => {
                if self.swallowing == Some(*code) {
                    FilterOutcome::HitTail
                } else {
                    FilterOutcome::Forward
                }
            }
            // 鼠标/滚轮/Text/路由层事件与热键无关，恒转发。
            _ => FilterOutcome::Forward,
        }
    }

    /// 会话级重置（ReleaseAll 对齐）：修饰键跟踪与吞尾清零。
    pub fn reset(&mut self) {
        self.tracker.clear();
        self.swallowing = None;
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::input::ScreenEdge;
    use xcontrol_proto::xkey::{
        KEY_A, KEY_END, KEY_HOME, KEY_LEFT, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_SHIFT, KEY_P,
        KEY_RIGHT, KEY_RIGHT_ALT,
    };

    use super::*;

    /// 默认四键集合（docs/03 §4.6 默认表）。
    fn default_set() -> HotkeySet {
        HotkeySet::from_config(&HotkeysConfig::default()).expect("默认热键必解析")
    }

    // —— config 解析 ——

    /// 默认四键解析：组合与 docs/03 §4.6 默认表一致。
    #[test]
    fn default_config_parses_all_four() {
        let set = default_set();
        assert_eq!(set.len(), 4);
        let ctrl_alt = ModFamilies::CTRL.union(ModFamilies::ALT);
        assert_eq!(
            set.combo(HotkeyId::Next),
            Some(HotkeyCombo {
                families: ctrl_alt,
                key: KEY_RIGHT,
            })
        );
        assert_eq!(
            set.combo(HotkeyId::Prev),
            Some(HotkeyCombo {
                families: ctrl_alt,
                key: KEY_LEFT,
            })
        );
        assert_eq!(
            set.combo(HotkeyId::BackToHost),
            Some(HotkeyCombo {
                families: ctrl_alt,
                key: KEY_HOME,
            })
        );
        assert_eq!(
            set.combo(HotkeyId::Pause),
            Some(HotkeyCombo {
                families: ctrl_alt,
                key: KEY_P,
            })
        );
    }

    /// 自定义组合解析：家族别名（Super=Meta）与 proto 全名主键（KEY_END）。
    #[test]
    fn custom_combo_and_super_alias_parse() {
        let config = HotkeysConfig {
            next: "Ctrl+Super+KEY_END".to_owned(),
            ..HotkeysConfig::default()
        };
        let set = HotkeySet::from_config(&config).expect("解析成功");
        assert_eq!(
            set.combo(HotkeyId::Next),
            Some(HotkeyCombo {
                families: ModFamilies::CTRL.union(ModFamilies::META),
                key: KEY_END,
            })
        );
    }

    /// 非法组合逐形态拒绝（与 config validate 同纪律：空/空段/无修饰键/
    /// 主键为修饰键/未知修饰键/重复修饰键/保留区）。
    #[test]
    fn invalid_combos_are_rejected_per_form() {
        let cases: [(&str, &str); 6] = [
            ("", "不能为空"),
            ("Ctrl++Right", "空段"),
            ("Right", "必须包含修饰键"),
            ("Ctrl+Foo+Right", "未知修饰键"),
            ("Ctrl+Ctrl+Right", "重复的修饰键"),
            ("Ctrl+LeftCtrl", "主键不能是修饰键"),
        ];
        for (raw, keyword) in cases {
            let config = HotkeysConfig {
                next: raw.to_owned(),
                ..HotkeysConfig::default()
            };
            let error = HotkeySet::from_config(&config).expect_err(raw);
            let message = error.to_string();
            assert!(message.contains(keyword), "{raw:?} → {message}");
            assert!(message.contains("hotkeys.next"), "字段路径缺失：{message}");
        }
    }

    /// 未知主键名 → UnknownKey（键名文法之外）。
    #[test]
    fn unknown_main_key_is_rejected() {
        let config = HotkeysConfig {
            pause: "Ctrl+Alt+Bogus".to_owned(),
            ..HotkeysConfig::default()
        };
        let error = HotkeySet::from_config(&config).expect_err("未知键名");
        assert_eq!(
            error,
            HotkeyError::UnknownKey {
                field: "hotkeys.pause",
                key: "Bogus".to_owned(),
            }
        );
    }

    // —— match_raw 矩阵（PRD AC：四键 × 命中/不命中/修饰键多余拒绝） ——

    /// 四默认键左手命中矩阵（任一侧语义：右手同型，见下一测）。
    #[test]
    fn match_matrix_left_hand_hits() {
        let set = default_set();
        let ctrl_alt = Mods::LCTRL.union(Mods::LALT);
        assert_eq!(set.match_raw(ctrl_alt, KEY_RIGHT), Some(HotkeyId::Next));
        assert_eq!(set.match_raw(ctrl_alt, KEY_LEFT), Some(HotkeyId::Prev));
        assert_eq!(
            set.match_raw(ctrl_alt, KEY_HOME),
            Some(HotkeyId::BackToHost)
        );
        assert_eq!(set.match_raw(ctrl_alt, KEY_P), Some(HotkeyId::Pause));
    }

    /// 「任一侧命中」：右手 Ctrl/Alt 组合同样命中（docs 用 Ctrl+Alt 泛指两侧）。
    #[test]
    fn right_hand_modifiers_hit_too() {
        let set = default_set();
        let ctrl_alt = Mods::RCTRL.union(Mods::RALT);
        assert_eq!(set.match_raw(ctrl_alt, KEY_RIGHT), Some(HotkeyId::Next));
        // 左右混搭同命中
        let mixed = Mods::LCTRL.union(Mods::RALT);
        assert_eq!(set.match_raw(mixed, KEY_RIGHT), Some(HotkeyId::Next));
    }

    /// 不命中矩阵：缺修饰键 / 错修饰键 / 多余修饰键（精确拒绝）/ 错主键 /
    /// 修饰键作主键 / 空集合。
    #[test]
    fn match_matrix_misses() {
        let set = default_set();
        let ctrl_alt = Mods::LCTRL.union(Mods::LALT);
        // 缺一侧修饰键
        assert_eq!(set.match_raw(Mods::LCTRL, KEY_RIGHT), None);
        assert_eq!(set.match_raw(Mods::LALT, KEY_RIGHT), None);
        assert_eq!(set.match_raw(Mods::default(), KEY_RIGHT), None);
        // 错修饰键家族
        assert_eq!(
            set.match_raw(Mods::LCTRL.union(Mods::LSHIFT), KEY_RIGHT),
            None
        );
        // 多余修饰键（精确语义）：+Shift / +Meta 均拒绝
        assert_eq!(set.match_raw(ctrl_alt.union(Mods::LSHIFT), KEY_RIGHT), None);
        assert_eq!(set.match_raw(ctrl_alt.union(Mods::LMETA), KEY_RIGHT), None);
        // 错主键（同修饰键按别的键）
        assert_eq!(set.match_raw(ctrl_alt, KEY_A), None);
        assert_eq!(set.match_raw(ctrl_alt, KEY_END), None);
        // 修饰键本身不作主键匹配
        assert_eq!(set.match_raw(ctrl_alt, KEY_LEFT_CTRL), None);
        // 空集合恒不命中
        assert_eq!(HotkeySet::empty().match_raw(ctrl_alt, KEY_RIGHT), None);
    }

    /// 同组合多键先到先得（条目序 = config 字段序）。
    #[test]
    fn duplicate_combo_first_wins() {
        let config = HotkeysConfig {
            prev: "Ctrl+Alt+Right".to_owned(), // 与 next 同组合
            ..HotkeysConfig::default()
        };
        let set = HotkeySet::from_config(&config).expect("解析成功");
        let ctrl_alt = Mods::LCTRL.union(Mods::LALT);
        assert_eq!(set.match_raw(ctrl_alt, KEY_RIGHT), Some(HotkeyId::Next));
    }

    /// 家族折叠：ModFamilies::from_mods 的左右手归并与空集。
    #[test]
    fn family_folding() {
        assert_eq!(ModFamilies::from_mods(Mods::default()).bits(), 0);
        assert_eq!(ModFamilies::from_mods(Mods::LCTRL), ModFamilies::CTRL);
        assert_eq!(ModFamilies::from_mods(Mods::RCTRL), ModFamilies::CTRL);
        assert_eq!(ModFamilies::from_mods(Mods::LALT), ModFamilies::ALT);
        assert_eq!(ModFamilies::from_mods(Mods::RMETA), ModFamilies::META);
        let both_ctrl = ModFamilies::from_mods(Mods::LCTRL.union(Mods::RCTRL));
        assert_eq!(both_ctrl, ModFamilies::CTRL, "同族两侧同折叠一位");
        assert_eq!(
            ModFamilies::from_mods(Mods::LSHIFT.union(Mods::RALT)).bits(),
            ModFamilies::SHIFT.bits() | ModFamilies::ALT.bits()
        );
    }

    // —— HotkeyFilter 流过滤 ——

    /// 主控捕获流全序列：修饰键转发 → 主键 Down 命中 → KeyRepeat 吞 → Up 吞。
    #[test]
    fn master_stream_hit_and_tail_swallow() {
        let mut filter = HotkeyFilter::new(default_set());
        // 修饰键：转发 + 跟踪
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Down,
            }),
            FilterOutcome::Forward
        );
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT_ALT,
                state: KeyState::Down,
            }),
            FilterOutcome::Forward
        );
        assert_eq!(
            filter.pressed_mods(),
            Mods::LCTRL.union(Mods::RALT),
            "左右手混合跟踪"
        );
        // 主键 Down：命中（右手 Alt 亦命中——任一侧语义）
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Hit(HotkeyId::Next)
        );
        // 长按重复：吞尾
        assert_eq!(
            filter.feed(&InputEvent::KeyRepeat { code: KEY_RIGHT }),
            FilterOutcome::HitTail
        );
        // 同键 Up：吞尾 + 清吞尾状态
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Up,
            }),
            FilterOutcome::HitTail
        );
        // 吞尾已清：同键再次 Down → 再次命中（释放后重按 = 重新路由）
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Hit(HotkeyId::Next)
        );
        // 按住期间的重复 Down（win/x11 捕获无 autorepeat 区分）：吞且不重复路由
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::HitTail
        );
        // 修饰键 Up：转发（命中组合的修饰键事件照常放行，模块文档）
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT_ALT,
                state: KeyState::Up,
            }),
            FilterOutcome::Forward
        );
        // 吞尾未清（主键尚未抬起）：继续吞
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::HitTail
        );
        // 主键 Up：吞尾 + 清状态
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Up,
            }),
            FilterOutcome::HitTail
        );
        // Alt 已释放：同键 Down 不再命中（缺修饰键）→ Forward
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Forward
        );
    }

    /// 未命中流全转发：修饰键不足/多余/错主键，事件逐条 Forward。
    #[test]
    fn non_matching_stream_forwards_everything() {
        let mut filter = HotkeyFilter::new(default_set());
        let events = [
            InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Down,
            },
            InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }, // 缺 Alt
            InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Up,
            },
            InputEvent::Key {
                code: KEY_LEFT_ALT,
                state: KeyState::Down,
            },
            InputEvent::Key {
                code: KEY_LEFT_SHIFT,
                state: KeyState::Down,
            }, // 多余 Shift
            InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            },
            InputEvent::Key {
                code: KEY_A,
                state: KeyState::Down,
            }, // 错主键
        ];
        for event in &events {
            assert_eq!(filter.feed(event), FilterOutcome::Forward, "{event:?}");
        }
    }

    /// 鼠标/滚轮/Text/路由事件恒 Forward（与热键无关）。
    #[test]
    fn non_key_events_always_forward() {
        let mut filter = HotkeyFilter::new(default_set());
        let events = [
            InputEvent::MouseMoveRel { dx: 1.0, dy: -1.0 },
            InputEvent::MouseButton {
                button: xcontrol_proto::input::MouseButton::Left,
                state: KeyState::Down,
            },
            InputEvent::Wheel {
                axis: xcontrol_proto::input::Axis::Vertical,
                delta: 1.5,
            },
            InputEvent::Text {
                utf8: "热键".to_owned(),
            },
            InputEvent::EnterScreen {
                target: 0,
                edge: ScreenEdge::Right,
                ratio: 0.5,
                mods: Mods::default(),
                preserve_cursor: false,
            },
        ];
        for event in &events {
            assert_eq!(filter.feed(event), FilterOutcome::Forward, "{event:?}");
        }
    }

    /// reset：跟踪与吞尾清零（ReleaseAll 对齐）。
    #[test]
    fn reset_clears_tracker_and_tail() {
        let mut filter = HotkeyFilter::new(default_set());
        filter.feed(&InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        });
        filter.feed(&InputEvent::Key {
            code: KEY_RIGHT,
            state: KeyState::Down, // 命中 Next，进入吞尾
        });
        filter.reset();
        assert!(filter.pressed_mods().is_empty());
        // 吞尾已清：Up 事件不再被吞（此时它只是普通未按下的 Up——恒 Forward）
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Up,
            }),
            FilterOutcome::Forward
        );
    }

    /// 空集合过滤器恒直通（未配置热键的平台路径）。
    #[test]
    fn empty_set_filter_is_passthrough() {
        let mut filter = HotkeyFilter::new(HotkeySet::empty());
        let ctrl_alt = Mods::LCTRL.union(Mods::LALT);
        filter.feed(&InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        });
        filter.feed(&InputEvent::Key {
            code: KEY_LEFT_ALT,
            state: KeyState::Down,
        });
        assert_eq!(filter.pressed_mods(), ctrl_alt);
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Forward
        );
    }

    // —— 热替换（docs/06 §4.1 热键即时生效行，11-21 反馈 3）——

    /// set_hotkeys：新表立即生效（旧组合失活、新组合激活），修饰键跟踪延续。
    #[test]
    fn set_hotkeys_swaps_matching_table_immediately() {
        let mut filter = HotkeyFilter::new(default_set());
        // 旧表：Ctrl+Alt+Right 命中 Next。
        filter.feed(&InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        });
        filter.feed(&InputEvent::Key {
            code: KEY_LEFT_ALT,
            state: KeyState::Down,
        });
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Hit(HotkeyId::Next)
        );
        filter.feed(&InputEvent::Key {
            code: KEY_RIGHT,
            state: KeyState::Up,
        });
        // 热替换：next 改为 Ctrl+Alt+End（修饰键跟踪延续——Ctrl/Alt 仍按着）。
        let config = HotkeysConfig {
            next: "Ctrl+Alt+KEY_END".to_owned(),
            ..HotkeysConfig::default()
        };
        filter.set_hotkeys(HotkeySet::from_config(&config).expect("新表解析"));
        // 旧组合失活（主键已换）。
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Forward,
            "旧组合应立即失活"
        );
        filter.feed(&InputEvent::Key {
            code: KEY_RIGHT,
            state: KeyState::Up,
        });
        // 新组合激活（同一按着的修饰键快照）。
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_END,
                state: KeyState::Down,
            }),
            FilterOutcome::Hit(HotkeyId::Next),
            "新组合应立即生效"
        );
    }

    /// set_hotkeys：替换瞬间旧命中主键的吞尾延续到物理 Up（防孤立注入），
    /// 尾巴清零后新表恢复正常判定。
    #[test]
    fn set_hotkeys_keeps_tail_swallow_until_release() {
        let mut filter = HotkeyFilter::new(default_set());
        filter.feed(&InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        });
        filter.feed(&InputEvent::Key {
            code: KEY_LEFT_ALT,
            state: KeyState::Down,
        });
        // 旧表命中 Next（进入吞尾）。
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Hit(HotkeyId::Next)
        );
        // 热替换为不含 Right 的新表。
        let config = HotkeysConfig {
            next: "Ctrl+Alt+KEY_END".to_owned(),
            ..HotkeysConfig::default()
        };
        filter.set_hotkeys(HotkeySet::from_config(&config).expect("新表解析"));
        // 替换瞬间仍按着的 Right：KeyRepeat/Down 尾巴继续吞（无 Down 的孤立
        // 注入防线不因替换丢失）。
        assert_eq!(
            filter.feed(&InputEvent::KeyRepeat { code: KEY_RIGHT }),
            FilterOutcome::HitTail
        );
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::HitTail
        );
        // 物理 Up：吞尾清零。
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Up,
            }),
            FilterOutcome::HitTail
        );
        // 清零后同键恢复普通事件语义。
        assert_eq!(
            filter.feed(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            }),
            FilterOutcome::Forward
        );
    }
}
