//! 按键映射引擎（docs/03 §4 全量，FR-3.1–3.4/3.6）——从机侧"收到主控事件 → 注入本机"
//! 之间的规则改写层。纯逻辑、零 I/O、零平台依赖（core 红线）。
//!
//! # 流水线位置
//!
//! ```text
//! 接收 → 热键过滤（docs/03 §4.6，映射之前，主控捕获层/从机注入前）→ 规则改写（本模块）
//!      → HID→本机键码（平台层）→ 注入
//! ```
//!
//! 引擎的落点是 [`MappingSink`]——包装 [`InputSink`](crate::sink::InputSink) 的适配器：
//! `SessionAction::InjectInput` 经 [`apply_actions`](crate::route::apply_actions) 到达 sink
//! 边界时逐事件改写。client/host 双端可用，不触碰会话 select 循环。
//!
//! # 结构（design.md）
//!
//! - **规则编译**：config `[[keymap.rules]]`（v3 trigger/action 四形态）+ 预设
//!   （[`preset_rules`]，docs/03 §4.4 表逐行）编译为三类查找结构 + 单键 drop 集：
//!   `key_remap`（单键）、`mod_map`（事件级修饰键）、`combo`（组合级/drop，保序
//!   first-match）、`key_drop`（无修饰键的单键丢弃）。自定义规则叠加在预设之后
//!   （同源键先命中预设则不查自定义——docs/03 §4.4，需覆盖时用 `preset = "none"`）。
//! - **事件级状态机**（§4.2）：`origin → mapped` 对照表按按下登记、释放按 origin
//!   查 mapped（错序安全），并按 mapped 目标维护**引用计数**（多个 origin 落同一
//!   mapped 修饰键时最后一个释放才注入 ↑——防过早释放；重复 Down/Up 按 origin 幂等）。
//!   未被规则覆盖的修饰键同样按恒等登记（与规则映射共用一套防卡键机制）。
//! - **组合级拦截状态机**（§4.3）：触发器修饰键按下 → pending 不注入；非修饰键按下
//!   → 查组合表（`trigger.mods` 精确等于当前按下的修饰键全集合）命中 → 原子弦
//!   （action.mods ↓ → key ↓ → key ↑ → action.mods ↑，位序 0→7 下、7→0 上）；
//!   未命中 → flush 补注入 pending 映射 + 缓存 + 当前键（退回 §4.2 语义）；
//!   触发器全释放仍无非修饰键 → flush 仅吐缓存**不产出注入**（pending Down 从未
//!   注入，虚假 ↑ 会误放真实按下的同目标键——docs/03 §4.3 修订裁定）。
//!   pending 缓存上限 [`PENDING_CACHE_CAP`] = 16，超限立即 flush 后当前事件照常注入。
//!
//! # 实现裁定（docs/03 §4 修订记录 2026-11-18，任务 11-18-m4-mapping）
//!
//! 1. **EnterScreen 对齐合成事件过映射**（与设计草案建议相反）：对齐事件与 wire Key
//!    事件在 sink 边界同为 `InjectInput(Key)` 不可区分，且两者承载的都是主控
//!    **原始（raw）语义**——Session 的 `injected_mods` 跟踪器与快照均在 raw 空间
//!    （见 session.rs `execute_input`/`align_actions`）。绕过映射会让非恒等预设把
//!    raw 键直接注入 OS（欠映射：mac2win 下对齐注入 LMeta 而非 LCtrl）；
//!    经引擎事件级路径换算才是"对端语义平移"。设计草案担忧的"双重映射"不存在。
//!    有自定义组合规则时对齐的触发器修饰键可能短暂进入 pending，由后续事件/释放
//!    flush（≤16 缓存兜底）。
//! 2. **reset 时机与 release_all 同一原子语义**：引擎运行态（origin 表/引用计数/
//!    pending）与"已注入 OS 的修饰键"一一对应，只在 OS 侧整体释放时重置——
//!    [`MappingSink::release_all`] 原子完成 inner 释放 + [`MappingEngine::reset`]。
//!    切出/暂停/断开/违规路径都产出 ReleaseAll，无需逐点挂钩。EnterScreen 不做
//!    额外 reset：此时引擎态应已为空（上次切出已 reset），若在此时清表反而丢失
//!    活 origin，使随后的修饰键 ↑ 查不到映射（卡键）。
//! 3. **组合弦为一次性 tap**（§4.3 修订）：不随物理按住延续；命中后触发器修饰键
//!    保持 withheld——其后重复按键可再次命中，或未命中时 flush 补注入。弦的
//!    action.mods 若与引擎已注入的修饰键同键（flush 补注入后再次命中组合），
//!    则跳过该键的 ↓/↑（不触碰 OS 已按住的键——防弦的 ↑ 提前释放活修饰键）。
//! 4. **Text 事件不过映射**（§4.5）：字符路径与键码路径正交，pending 期间亦直通
//!    （不缓存不拦截）。鼠标/滚轮/KeyRepeat 在 pending 期间缓存保序（§4.3 约束）。
//! 5. **热键在映射管线之外**（§4.6）：主控侧捕获层拦截不转发；从机侧注入前过滤
//!    由调用方在引擎之前完成（M4-hotkeys）。引擎只改写已转发的原始键码，
//!    [`MappingEngine::passthrough`] 供调用方查询直通态。
//! 6. **keymap 变更生效时机**：引擎每会话从 config 编译（会话重建/重连生效）；
//!    运行中会话的改动经 [`MappingEngine::set_rules`] 即时热替换（docs/06
//!    §4.1 keymap 行，11-21 反馈；替换语义见该方法文档）。编译失败由调用方
//!    降级恒等引擎/保留旧规则并告警（输入管线不因映射配置错误中断）。

use std::collections::HashMap;

use xcontrol_config::{KEYMAP_PRESETS, KeymapRule, parse_key_name};
use xcontrol_proto::input::{InputEvent, KeyState, Mods};
use xcontrol_proto::xkey::{
    KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_RIGHT_ALT, KEY_RIGHT_CTRL, KEY_RIGHT_META, XKey,
};

use crate::mods::MOD_KEYS;
use crate::sink::{InputSink, SinkError};

/// pending 事件缓存上限（docs/03 §4.3：超限立即 flush，防异常序列撑爆）。
pub const PENDING_CACHE_CAP: usize = 16;

/// 映射引擎编译/构造错误（error-handling.md：可编程区分、不含敏感信息）。
///
/// config 校验（加载期）先于本错误报出同名问题并附字段路径；本错误防御
/// 绕过校验的构造路径（如直接调用 [`MappingEngine::new`]）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MappingError {
    /// 未知预设名（docs/03 §4.4 八内置）。
    #[error("未知 keymap 预设 {found:?}（可用：{}，docs/03 §4.4）", KEYMAP_PRESETS.join("、"))]
    UnknownPreset {
        /// 传入的预设名。
        found: String,
    },
    /// 语义键名无法解析（docs/03 §4.1 键名文法）。
    #[error("keymap.rules[{rule}] 键名 {name:?} 无法解析为 XKey（docs/03 §4.1 键名文法）")]
    UnknownKeyName {
        /// 规则在 custom 列表中的下标。
        rule: usize,
        /// 无法解析的键名。
        name: String,
    },
    /// 规则形态非法（trigger/action 组合不合法）。
    #[error("keymap.rules[{rule}] 规则形态非法：{reason}")]
    InvalidRule {
        /// 规则在 custom 列表中的下标。
        rule: usize,
        /// 非法原因（中文，供日志/UI）。
        reason: String,
    },
}

/// 预设事件级修饰键规则（docs/03 §4.4 表逐行；from/to 均为修饰键）。
///
/// `LOpt→LAlt`/`ROpt→RAlt` 在 XKey 空间是恒等（mac Option 与 win/linux Alt 是
/// 同一 HID 键码）——按表逐行显式保留，语义是"文档化该键已被预设覆盖"，
/// 行为上与无规则等价（恒等登记走同一防卡键机制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetRule {
    /// 主控侧（原始）修饰键。
    pub from: XKey,
    /// 注入本机（从机）的修饰键。
    pub to: XKey,
}

/// `mac2win` / `mac2linux`（docs/03 §4.4 行 1；`LCtrl→LWin` 为注释示例，默认直通）。
static MAC2WIN: [PresetRule; 4] = [
    PresetRule {
        from: KEY_LEFT_META,
        to: KEY_LEFT_CTRL,
    },
    PresetRule {
        from: KEY_RIGHT_META,
        to: KEY_RIGHT_CTRL,
    },
    // LOpt→LAlt / ROpt→RAlt：XKey 恒等（见表条目文档）
    PresetRule {
        from: KEY_LEFT_ALT,
        to: KEY_LEFT_ALT,
    },
    PresetRule {
        from: KEY_RIGHT_ALT,
        to: KEY_RIGHT_ALT,
    },
];

/// `win2mac` / `linux2mac`（docs/03 §4.4 行 2；win 的 LWin/RWin 与 linux 的
/// LMeta/RMeta 是同一 XKey——两预设规则集相同，仅感知命名不同）。
static WIN2MAC: [PresetRule; 4] = [
    PresetRule {
        from: KEY_LEFT_CTRL,
        to: KEY_LEFT_META,
    },
    PresetRule {
        from: KEY_RIGHT_CTRL,
        to: KEY_RIGHT_META,
    },
    PresetRule {
        from: KEY_LEFT_META,
        to: KEY_LEFT_ALT,
    },
    PresetRule {
        from: KEY_RIGHT_META,
        to: KEY_RIGHT_ALT,
    },
];

/// 内置预设的规则表（docs/03 §4.4 逐行）。
///
/// `win2linux`/`linux2win`（LWin↔LMeta 直通语义不变）、`mac2mac`、`none` 为恒等，
/// 规则集为空。
#[must_use]
pub fn preset_rules(preset: &str) -> Option<&'static [PresetRule]> {
    match preset {
        "mac2win" | "mac2linux" => Some(&MAC2WIN),
        "win2mac" | "linux2mac" => Some(&WIN2MAC),
        "win2linux" | "linux2win" | "mac2mac" | "none" => Some(&[]),
        _ => None,
    }
}

/// 组合级规则的注入动作（docs/03 §4.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ComboAction {
    /// 原子弦：action.mods ↓ → key ↓ → key ↑ → action.mods ↑（一次性 tap）。
    Chord { mods: Mods, key: XKey },
    /// drop：命中即吞，不产出注入（docs/03 §4.1 形态 4）。
    Drop,
}

/// 组合级规则（含 drop；trigger 用主控原始 XKey 判定，docs/03 §4.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ComboRule {
    /// 触发修饰键集合（精确匹配：列出的全部按下且未列出的不得按下）。
    trigger_mods: Mods,
    /// 触发键（非修饰键）。
    trigger_key: XKey,
    /// 命中后的动作。
    action: ComboAction,
}

/// 编译后的规则查找结构（design.md：单键 / 事件级修饰 / 组合级保序三类 + 单键 drop）。
#[derive(Debug, Default)]
struct CompiledRules {
    /// 单键重映射（docs/03 §4.1 形态 1；Down/Up 对称映射）。
    key_remap: HashMap<XKey, XKey>,
    /// 单键丢弃（形态 1 的 `action.drop`；Down/Up 均吞）。
    key_drop: Vec<XKey>,
    /// 事件级修饰键映射（形态 2；按下登记 origin→mapped，docs/03 §4.2）。
    mod_map: HashMap<XKey, XKey>,
    /// 组合级/drop 规则（形态 3/4；保序 first-match）。
    combo: Vec<ComboRule>,
    /// 所有组合规则触发修饰键的位并集（进入 pending 的判定）。
    trigger_mods: Mods,
}

/// 按键映射引擎（纯逻辑状态机；每会话实例化，断开即弃）。
///
/// 构造见 [`MappingEngine::new`]；事件入口 [`MappingEngine::step`]；接线经
/// [`MappingSink`]（或测试直接驱动引擎）。运行态见各字段文档。
#[derive(Debug)]
pub struct MappingEngine {
    /// 编译后的规则（预设 + 自定义）。
    rules: CompiledRules,
    /// FR-3.6 直通模式：true 时全部事件原样通过、状态机不参与。
    passthrough: bool,
    /// 已注入修饰键对照表：origin（主控原始键）→ mapped（注入键）。
    /// 按下登记、释放按 origin 查（错序防卡键，docs/03 §4.2）。
    origin: HashMap<XKey, XKey>,
    /// mapped 修饰键按 mods bit 的引用计数（同目标多 origin：最后一个释放才注入 ↑）。
    hold: [u8; 8],
    /// 主控侧当前按下的原始修饰键位集（组合精确匹配基准；对齐事件同样维护）。
    held_mods: Mods,
    /// 组合触发器修饰键：已按下但未注入（按下序；flush 时按事件级语义补注入）。
    withheld: Vec<XKey>,
    /// pending 期间缓存的事件（≤ [`PENDING_CACHE_CAP`]，docs/03 §4.3）。
    cache: Vec<InputEvent>,
}

impl MappingEngine {
    /// 从预设名 + 自定义规则 + 直通开关构造引擎（docs/03 §4 全量入口）。
    ///
    /// 自定义规则叠加在预设之后：`key_remap`/`mod_map` 同源键预设先命中
    /// （自定义被忽略——需覆盖预设请用 `preset = "none"`，docs/03 §4.4）；
    /// `combo` 规则保序追加（预设无组合规则，自定义序即 first-match 序）。
    ///
    /// # Errors
    /// [`MappingError::UnknownPreset`]（预设名不在 [`KEYMAP_PRESETS`](xcontrol_config::KEYMAP_PRESETS)）、
    /// [`MappingError::UnknownKeyName`] / [`MappingError::InvalidRule`]（自定义规则
    /// 键名/形态非法；config 校验本应先行拦截同名问题）。
    pub fn new(
        preset: &str,
        custom: &[KeymapRule],
        passthrough: bool,
    ) -> Result<Self, MappingError> {
        let preset_table = preset_rules(preset).ok_or_else(|| MappingError::UnknownPreset {
            found: preset.to_owned(),
        })?;
        let mut rules = CompiledRules::default();
        for rule in preset_table {
            rules.mod_map.insert(rule.from, rule.to);
        }
        for (index, rule) in custom.iter().enumerate() {
            compile_custom(rule, index, &mut rules)?;
        }
        Ok(Self {
            rules,
            passthrough,
            origin: HashMap::new(),
            hold: [0; 8],
            held_mods: Mods::default(),
            withheld: Vec::new(),
            cache: Vec::new(),
        })
    }

    /// 恒等引擎（接线方在规则编译失败时的降级：键码直通、不出错中断输入管线）。
    #[must_use]
    pub fn identity() -> Self {
        Self {
            rules: CompiledRules::default(),
            passthrough: false,
            origin: HashMap::new(),
            hold: [0; 8],
            held_mods: Mods::default(),
            withheld: Vec::new(),
            cache: Vec::new(),
        }
    }

    /// 热替换规则（运行中会话收到 `RuntimeConfigUpdate` 的路径，docs/06 §4.1
    /// keymap 行——即时生效；入参语义与 [`Self::new`] 相同）。
    ///
    /// 替换语义（简单正确方案，注释钉死）：
    /// - **编译失败保持旧规则不变**（返回 `Err`，调用方留痕降级——输入管线
    ///   不因坏配置中断，error-handling.md 规则 4）；
    /// - **规则表整体替换、立即生效**：其后的 Down/Up 解析按新表执行；
    /// - **origin 对照表与 hold 引用计数保留**：已注入 OS 的修饰键是替换前
    ///   旧规则产出的运行态，释放必须按 origin 查 mapped（对照表是防卡键的
    ///   事实源，与规则表无关）——替换不清运行态；
    /// - **组合级 pending（withheld/cache）保留**：withheld 触发器在其物理
    ///   Up 时经 `mod_up` 摘除并按既有路径 flush 缓存（状态机路径与规则表
    ///   无关），缓存事件的解析点天然使用替换后的新表——无需等待「无 pending
    ///   才替换」的安全点；新规则只影响其后的新按下判定；
    /// - **passthrough 短路的边界**：切换进 `passthrough = true` 时 `step`
    ///   不再走状态机，替换前已注入且未释放的修饰键将按原始码直通 Up（非
    ///   恒等映射下 OS 侧 mapped 键的释放延后会话收尾 ReleaseAll 兜底）——
    ///   「改配置瞬间正按着修饰键」的极小概率边界，不做补偿注入（复杂度
    ///   不成比例）。
    ///
    /// # Errors
    /// [`MappingError`]（同 [`Self::new`]；失败时引擎保持旧规则原样）。
    pub fn set_rules(
        &mut self,
        preset: &str,
        custom: &[KeymapRule],
        passthrough: bool,
    ) -> Result<(), MappingError> {
        let preset_table = preset_rules(preset).ok_or_else(|| MappingError::UnknownPreset {
            found: preset.to_owned(),
        })?;
        let mut rules = CompiledRules::default();
        for rule in preset_table {
            rules.mod_map.insert(rule.from, rule.to);
        }
        for (index, rule) in custom.iter().enumerate() {
            compile_custom(rule, index, &mut rules)?;
        }
        self.rules = rules;
        self.passthrough = passthrough;
        Ok(())
    }

    /// 是否直通模式（FR-3.6；热键层/调用方可查询，docs/03 §4.6）。
    #[must_use]
    pub const fn passthrough(&self) -> bool {
        self.passthrough
    }

    /// 状态机主入口：一条输入事件 → 注入事件序列（0..n 条，因果序）。
    ///
    /// 分流（docs/03 §4）：`Key` → 事件级/组合级状态机；`KeyRepeat`/鼠标/滚轮 →
    /// pending 时缓存、否则直通（键码按规则换算）；`Text` → 字符路径恒直通
    /// （§4.5）；其余（EnterScreen 族控制事件）不应到达注入管线，防御性直通。
    pub fn step(&mut self, event: &InputEvent) -> Vec<InputEvent> {
        if self.passthrough {
            return vec![event.clone()];
        }
        match event {
            InputEvent::Key { code, state } => self.step_key(*code, *state),
            InputEvent::KeyRepeat { code } => {
                if self.withheld.is_empty() {
                    self.resolve_repeat(*code).into_iter().collect()
                } else {
                    self.cache_or_overflow(event.clone())
                }
            }
            // 鼠标/滚轮：pending 期间缓存保序（docs/03 §4.3 约束），其余直通。
            event @ (InputEvent::MouseMoveRel { .. }
            | InputEvent::MouseMoveAbs { .. }
            | InputEvent::MouseButton { .. }
            | InputEvent::Wheel { .. }) => self.pass_or_cache(event),
            // Text：字符路径不过键码映射（docs/03 §4.5），pending 期间亦直通保活。
            event @ InputEvent::Text { .. } => vec![event.clone()],
            // 控制事件族不进注入管线（会话层转对齐/warp/释放）；防御性直通。
            other => vec![other.clone()],
        }
    }

    /// 排空 pending：未注入的触发器修饰键按事件级语义补注入（登记 origin 对照）
    /// → 缓存事件。断开/热重载收尾或测试用；正常释放路径由状态机内部 flush。
    pub fn flush_pending(&mut self) -> Vec<InputEvent> {
        self.flush_with_downs()
    }

    /// 清空全部运行态（与 [`MappingSink::release_all`] 同一原子语义：
    /// OS 侧整体释放时 origin 表/引用计数/pending 一并归零）。
    pub fn reset(&mut self) {
        self.origin.clear();
        self.hold = [0; 8];
        self.held_mods = Mods::default();
        self.withheld.clear();
        self.cache.clear();
    }

    /// 单键事件分流：修饰键走事件级/组合级状态机，普通键走组合解析/重映射。
    fn step_key(&mut self, code: XKey, state: KeyState) -> Vec<InputEvent> {
        if code.is_modifier() {
            match state {
                KeyState::Down => self.mod_down(code),
                KeyState::Up => self.mod_up(code),
            }
        } else {
            match state {
                KeyState::Down => self.key_down(code),
                // 单键 drop 双向吞（v2 `to = None` 语义：该键不存在于注入流）。
                KeyState::Up if self.rules.key_drop.contains(&code) => Vec::new(),
                // 非修饰键释放对称映射（无状态；组合弦已 tap 过的键再注入 ↑ 为
                // 平台侧无害 no-op——键已抬起）。释放不参与组合解析（docs/03 §4.3.2）。
                KeyState::Up => vec![InputEvent::Key {
                    code: self.map_non_mod_code(code),
                    state: KeyState::Up,
                }],
            }
        }
    }

    /// 修饰键按下：组合触发器 → pending 暂不注入（§4.3.1）；否则事件级注入
    /// （mod_map 换算 + origin 登记 + 引用计数，§4.2）。
    fn mod_down(&mut self, code: XKey) -> Vec<InputEvent> {
        let one = mod_bit(code);
        self.held_mods = self.held_mods.union(one);
        // 触发器修饰键：未注入（未 withheld、未登记 origin）→ 暂不注入进入
        // pending；已在 pending 或已注入（flush 补注入后）的重复 Down 幂等
        // 不产出注入（docs/03 §4.2 修订）——pending 触发器无 origin 登记，
        // 若放行到事件级路径会在 pending 中途注入，既破坏"pending 不注入"
        // 又使随后的首次 Up 走 flush③ 无产出（修饰键卡在 OS 按下态）。
        if self.is_trigger(code) {
            if self.withheld.contains(&code) || self.origin.contains_key(&code) {
                return Vec::new();
            }
            self.withheld.push(code);
            return Vec::new();
        }
        self.inject_mod_down(code)
    }

    /// 修饰键释放：pending 触发器 → 摘除（全释放时 flush 路径③：仅吐缓存）；
    /// 已注入的 → 按 origin 查 mapped + 引用计数归零才注入 ↑（§4.2 修订）。
    fn mod_up(&mut self, code: XKey) -> Vec<InputEvent> {
        self.held_mods = Mods::from_bits(self.held_mods.bits() & !mod_bit(code).bits());
        if let Some(position) = self.withheld.iter().position(|key| *key == code) {
            self.withheld.remove(position);
            if self.withheld.is_empty() {
                // flush 路径③（docs/03 §4.3 修订）：pending Down 从未注入，
                // 不产出任何注入（虚假 ↑ 会误放真实按下的同目标键），仅吐缓存。
                return self.resolve_cache();
            }
            return Vec::new();
        }
        self.inject_mod_up(code)
    }

    /// 事件级修饰键注入（§4.2）：mod_map 换算（无规则恒等）、origin 登记、
    /// 引用计数 +1；重复 Down（origin 已登记）幂等不注入。
    fn inject_mod_down(&mut self, origin: XKey) -> Vec<InputEvent> {
        if self.origin.contains_key(&origin) {
            return Vec::new();
        }
        let mapped = self.rules.mod_map.get(&origin).map_or(origin, |to| *to);
        self.origin.insert(origin, mapped);
        if let Some(bit) = mapped.mods_bit() {
            self.hold[usize::from(bit)] += 1;
        }
        vec![InputEvent::Key {
            code: mapped,
            state: KeyState::Down,
        }]
    }

    /// 事件级修饰键释放（§4.2）：origin 查 mapped、引用计数 -1，归零才注入 ↑
    /// （同目标多 origin 场景防过早释放）；无登记（重复 Up / 未按下）不注入。
    fn inject_mod_up(&mut self, origin: XKey) -> Vec<InputEvent> {
        let Some(mapped) = self.origin.remove(&origin) else {
            return Vec::new();
        };
        if let Some(bit) = mapped.mods_bit() {
            let slot = &mut self.hold[usize::from(bit)];
            *slot = slot.saturating_sub(1);
            if *slot > 0 {
                return Vec::new();
            }
        }
        vec![InputEvent::Key {
            code: mapped,
            state: KeyState::Up,
        }]
    }

    /// 非修饰键按下：组合级解析（§4.3.2）→ 单键 drop/重映射。
    fn key_down(&mut self, code: XKey) -> Vec<InputEvent> {
        // 存在组合规则且有修饰键按下 → 查组合表（trigger.mods 精确匹配 held_mods）。
        if !self.rules.combo.is_empty() && !self.held_mods.is_empty() {
            // 命中的动作先克隆（ComboAction 为小型 Copy 级数据），脱离 rules 借用
            // 后才能排空缓存（resolve_cache 需 &mut self）。
            let hit = self
                .rules
                .combo
                .iter()
                .find(|rule| rule.trigger_mods == self.held_mods && rule.trigger_key == code)
                .map(|rule| rule.action.clone());
            if let Some(action) = hit {
                let mut out = self.resolve_cache(); // 缓存事件按到达序先吐（因果序）
                match action {
                    ComboAction::Drop => {} // 命中即吞，不产出注入
                    ComboAction::Chord { mods, key } => {
                        // action.mods 与引擎已注入修饰键同键 → 跳过 ↓/↑
                        //（flush 补注入后再次命中组合：不触碰 OS 已按住的键）。
                        for (bit, mod_key) in MOD_KEYS.iter().enumerate() {
                            let one = Mods::from_key_bit(bit as u8);
                            if mods.contains(one) && self.hold[bit] == 0 {
                                out.push(InputEvent::Key {
                                    code: *mod_key,
                                    state: KeyState::Down,
                                });
                            }
                        }
                        out.push(InputEvent::Key {
                            code: key,
                            state: KeyState::Down,
                        });
                        out.push(InputEvent::Key {
                            code: key,
                            state: KeyState::Up,
                        });
                        for (bit, mod_key) in MOD_KEYS.iter().enumerate().rev() {
                            let one = Mods::from_key_bit(bit as u8);
                            if mods.contains(one) && self.hold[bit] == 0 {
                                out.push(InputEvent::Key {
                                    code: *mod_key,
                                    state: KeyState::Up,
                                });
                            }
                        }
                    }
                }
                return out;
            }
            // 未命中任何组合规则 → flush（退回 §4.2 语义：补注入 pending 映射 +
            // 缓存事件）+ 当前键。
            let mut out = self.flush_with_downs();
            if self.rules.key_drop.contains(&code) {
                return out; // 单键 drop 规则优先于注入
            }
            out.push(InputEvent::Key {
                code: self.map_non_mod_code(code),
                state: KeyState::Down,
            });
            return out;
        }
        // 无组合上下文：单键 drop / 单键重映射。
        if self.rules.key_drop.contains(&code) {
            return Vec::new();
        }
        vec![InputEvent::Key {
            code: self.map_non_mod_code(code),
            state: KeyState::Down,
        }]
    }

    /// flush 路径 2'（docs/03 §4.3 修订）：补注入 pending 修饰键（按事件级语义，
    /// 登记 origin 对照供后续释放正确换算）→ 吐出缓存事件。
    fn flush_with_downs(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        let withheld = std::mem::take(&mut self.withheld);
        for origin in withheld {
            out.extend(self.inject_mod_down(origin));
        }
        out.extend(self.resolve_cache());
        out
    }

    /// 非修饰键码换算（单键重映射；无规则恒等）。
    fn map_non_mod_code(&self, code: XKey) -> XKey {
        self.rules.key_remap.get(&code).map_or(code, |to| *to)
    }

    /// 鼠标/滚轮：pending 时缓存（含 16 上限溢出 flush），否则直通。
    fn pass_or_cache(&mut self, event: &InputEvent) -> Vec<InputEvent> {
        if self.withheld.is_empty() {
            return vec![event.clone()];
        }
        self.cache_or_overflow(event.clone())
    }

    /// KeyRepeat 的解析（与 Key 路径同语义：单键 drop 吞、单键重映射换算）。
    ///
    /// docs/03 §4.3 缓存定稿要求缓存事件与直通路径语义一致——pending 期间缓存的
    /// repeat 在各 flush 路径（③/2'/命中/超限）同样经本函数解析，否则 raw 码
    /// 漏改写（重映射规则下注错键）、drop 规则下漏吞。
    fn resolve_repeat(&self, code: XKey) -> Option<InputEvent> {
        if self.rules.key_drop.contains(&code) {
            return None;
        }
        Some(InputEvent::KeyRepeat {
            code: self.map_non_mod_code(code),
        })
    }

    /// 排空 pending 缓存：KeyRepeat 逐条解析（[`Self::resolve_repeat`]），
    /// 鼠标/滚轮原样（无键码语义，docs/03 §4.3 缓存范围）。
    fn resolve_cache(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.cache)
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::KeyRepeat { code } => self.resolve_repeat(code),
                other => Some(other),
            })
            .collect()
    }

    /// pending 缓存写入（含 16 上限溢出 flush）。
    fn cache_or_overflow(&mut self, event: InputEvent) -> Vec<InputEvent> {
        if self.cache.len() >= PENDING_CACHE_CAP {
            // 超限立即 flush（docs/03 §4.3）：补注入 pending 映射 + 吐缓存；
            // 当前事件按新状态重新解析（KeyRepeat 走 repeat 语义，鼠标/滚轮直通）。
            let mut out = self.flush_with_downs();
            match event {
                InputEvent::KeyRepeat { code } => out.extend(self.resolve_repeat(code)),
                other => out.push(other),
            }
            return out;
        }
        self.cache.push(event);
        Vec::new()
    }

    /// 该修饰键是否任一组合规则的触发器。
    fn is_trigger(&self, code: XKey) -> bool {
        !self.rules.combo.is_empty()
            && code
                .mods_bit()
                .is_some_and(|bit| self.rules.trigger_mods.contains(Mods::from_key_bit(bit)))
    }
}

/// 修饰键 → mods 位（单 bit）；调用方保证修饰键。
fn mod_bit(code: XKey) -> Mods {
    code.mods_bit().map_or(Mods::default(), Mods::from_key_bit)
}

/// 编译一条自定义规则（docs/03 §4.1 四形态；与 config 校验同语义的二次判定）。
fn compile_custom(
    rule: &KeymapRule,
    index: usize,
    out: &mut CompiledRules,
) -> Result<(), MappingError> {
    parse_trigger_action(rule, index, out)
}

/// trigger/action 形态解析与登记（`compile_custom` 主体；独立函数便于错误下标传递）。
fn parse_trigger_action(
    rule: &KeymapRule,
    index: usize,
    out: &mut CompiledRules,
) -> Result<(), MappingError> {
    let invalid = |reason: String| MappingError::InvalidRule {
        rule: index,
        reason,
    };
    let unknown = |name: &str| MappingError::UnknownKeyName {
        rule: index,
        name: name.to_owned(),
    };
    let parse = |name: &str| -> Result<XKey, MappingError> {
        parse_key_name(name)
            .and_then(XKey::new)
            .ok_or_else(|| unknown(name))
    };

    // —— trigger 形态判定 ——
    if let Some(mod_name) = rule.trigger.mod_.as_deref() {
        if rule.trigger.key.is_some() || rule.trigger.mods.is_some() {
            return Err(invalid(
                "trigger.mod 形态不接受 key/mods（修饰键映射是单键事件级规则，docs/03 §4.1 形态 2）".to_owned(),
            ));
        }
        let from = parse(mod_name)?;
        if !from.is_modifier() {
            return Err(invalid(format!(
                "trigger.mod 必须是修饰键（{mod_name:?} 不是修饰键名）"
            )));
        }
        let Some(to_name) = rule.action.mod_.as_deref() else {
            return Err(invalid(
                "trigger.mod 需要 action.mod 形态的注入目标".to_owned(),
            ));
        };
        if rule.action.key.is_some() || rule.action.mods.is_some() || rule.action.drop.is_some() {
            return Err(invalid(
                "action.mod 形态不接受 key/mods/drop（docs/03 §4.1 形态 2）".to_owned(),
            ));
        }
        let to = parse(to_name)?;
        if !to.is_modifier() {
            return Err(invalid(format!(
                "action.mod 必须是修饰键（{to_name:?} 不是修饰键名）"
            )));
        }
        // 自定义叠加在预设之后：同源键预设先命中（or_insert 保首条）
        out.mod_map.entry(from).or_insert(to);
        return Ok(());
    }

    let Some(key_name) = rule.trigger.key.as_deref() else {
        return Err(invalid(
            "trigger 缺少内容：单键用 trigger.key，修饰键映射用 trigger.mod，组合用 trigger = { mods, key }（docs/03 §4.1）"
                .to_owned(),
        ));
    };
    let key = parse(key_name)?;

    match rule.trigger.mods.as_deref() {
        Some(mod_names) => {
            // 组合级/drop（形态 3/4）：trigger.key 必须是非修饰键
            if key.is_modifier() {
                return Err(invalid(
                    "组合级 trigger.key 必须是非修饰键（修饰键请写进 trigger.mods）".to_owned(),
                ));
            }
            let trigger_mods = compile_mods(mod_names, index)?;
            match rule.action.drop {
                Some(true) => {
                    if rule.action.key.is_some()
                        || rule.action.mod_.is_some()
                        || rule.action.mods.is_some()
                    {
                        return Err(invalid(
                            "action.drop 不接受与 key/mod/mods 同现（丢弃即不产出注入）".to_owned(),
                        ));
                    }
                    out.combo.push(ComboRule {
                        trigger_mods,
                        trigger_key: key,
                        action: ComboAction::Drop,
                    });
                }
                Some(false) => {
                    return Err(invalid(
                        "action.drop 只接受 true（不丢弃请省略）".to_owned(),
                    ));
                }
                None => {
                    let Some(action_key_name) = rule.action.key.as_deref() else {
                        return Err(invalid(
                            "组合级规则需要 action = { mods, key } 或 action.drop = true（docs/03 §4.1 形态 3/4）"
                                .to_owned(),
                        ));
                    };
                    if rule.action.mod_.is_some() {
                        return Err(invalid(
                            "action.mod 不参与组合级规则（弦修饰键请写进 action.mods）".to_owned(),
                        ));
                    }
                    let action_key = parse(action_key_name)?;
                    if action_key.is_modifier() {
                        return Err(invalid(format!(
                            "组合级 action.key 必须是非修饰键（{action_key_name:?} 是修饰键）"
                        )));
                    }
                    let action_mods = match rule.action.mods.as_deref() {
                        Some(names) => compile_mods(names, index)?,
                        None => Mods::default(),
                    };
                    out.combo.push(ComboRule {
                        trigger_mods,
                        trigger_key: key,
                        action: ComboAction::Chord {
                            mods: action_mods,
                            key: action_key,
                        },
                    });
                }
            }
            out.trigger_mods = out.trigger_mods.union(trigger_mods);
            Ok(())
        }
        None => {
            // 单键（形态 1 / 单键 drop）：trigger.key 必须是非修饰键
            if key.is_modifier() {
                return Err(invalid(format!(
                    "{key_name:?} 是修饰键——修饰键规则请用 trigger.mod/action.mod 形态（事件级映射语义，docs/03 §4.2）"
                )));
            }
            if rule.action.mod_.is_some() || rule.action.mods.is_some() {
                return Err(invalid(
                    "单键规则的 action 不接受 mod/mods（组合改写请给 trigger.mods）".to_owned(),
                ));
            }
            match rule.action.drop {
                Some(true) => {
                    if rule.action.key.is_some() {
                        return Err(invalid(
                            "action.drop 不接受与 key 同现（丢弃即不产出注入）".to_owned(),
                        ));
                    }
                    out.key_drop.push(key);
                }
                Some(false) => {
                    return Err(invalid(
                        "action.drop 只接受 true（不丢弃请省略）".to_owned(),
                    ));
                }
                None => {
                    let Some(action_key_name) = rule.action.key.as_deref() else {
                        return Err(invalid(
                            "单键规则需要 action.key 或 action.drop = true（docs/03 §4.1 形态 1/4）"
                                .to_owned(),
                        ));
                    };
                    let to = parse(action_key_name)?;
                    if to.is_modifier() {
                        return Err(invalid(format!(
                            "单键重映射的 action.key 必须是非修饰键（{action_key_name:?} 是修饰键——请用 trigger.mod/action.mod 形态）"
                        )));
                    }
                    out.key_remap.entry(key).or_insert(to);
                }
            }
            Ok(())
        }
    }
}

/// 编译 trigger/action.mods 列表：非空、全为修饰键、无重复 → Mods 位集。
fn compile_mods(names: &[String], index: usize) -> Result<Mods, MappingError> {
    if names.is_empty() {
        return Err(MappingError::InvalidRule {
            rule: index,
            reason: "mods 不能为空列表（组合级规则至少要有一个触发修饰键）".to_owned(),
        });
    }
    let mut mods = Mods::default();
    for name in names {
        let Some(raw) = parse_key_name(name).and_then(XKey::new) else {
            return Err(MappingError::UnknownKeyName {
                rule: index,
                name: name.clone(),
            });
        };
        let Some(bit) = raw.mods_bit() else {
            return Err(MappingError::InvalidRule {
                rule: index,
                reason: format!("mods 必须全是修饰键名（{name} 不是修饰键）"),
            });
        };
        let one = Mods::from_key_bit(bit);
        if mods.contains(one) {
            return Err(MappingError::InvalidRule {
                rule: index,
                reason: format!("mods 中重复的修饰键 {name}（集合匹配语义下重复无意义）"),
            });
        }
        mods = mods.union(one);
    }
    Ok(mods)
}

/// 映射引擎的 sink 适配器（design.md 裁定的集成方式）：包装 [`InputSink`]，
/// 注入动作先经 [`MappingEngine`] 改写再下沉；warp 直通；`release_all` 原子完成
/// "释放 OS 按键 + 重置引擎运行态"（模块文档裁定 2）。
///
/// client/host 双端可用（`apply_actions` 只见 `&mut dyn InputSink`），不触碰
/// 会话 select 循环。
#[derive(Debug)]
pub struct MappingSink<S: InputSink> {
    engine: MappingEngine,
    inner: S,
}

impl<S: InputSink> MappingSink<S> {
    /// 包装引擎与底层 sink。
    #[must_use]
    pub fn new(engine: MappingEngine, inner: S) -> Self {
        Self { engine, inner }
    }

    /// 底层 sink 的只读借用（观测/测试：已到达底层的事件记录等）。
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// 排空引擎 pending 并注入产出（断开/收尾用；返回注入条数）。
    pub fn flush_pending(&mut self) -> usize {
        let produced = self.engine.flush_pending();
        let count = produced.len();
        for event in &produced {
            let _ = self.inner.inject(event);
        }
        count
    }

    /// 重置引擎运行态（不含底层 sink 的释放——与 `release_all` 配套使用）。
    pub fn reset(&mut self) {
        self.engine.reset();
    }

    /// 热替换映射规则（运行中会话收到 `RuntimeConfigUpdate` 的路径，docs/06
    /// §4.1 keymap 行——即时生效；语义见 [`MappingEngine::set_rules`]）。
    ///
    /// # Errors
    /// [`MappingError`]（编译失败时引擎保持旧规则原样）。
    pub fn set_rules(
        &mut self,
        preset: &str,
        custom: &[KeymapRule],
        passthrough: bool,
    ) -> Result<(), MappingError> {
        self.engine.set_rules(preset, custom, passthrough)
    }

    /// 取回底层 sink。
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: InputSink> InputSink for MappingSink<S> {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        let produced = self.engine.step(event);
        // 逐条注入，尽力投递全部产出；失败只回报首错（error-handling.md §4：
        // 单次注入失败不中断事件流，由 apply_actions 计数）。
        let mut first_error = None;
        for produced_event in &produced {
            if let Err(error) = self.inner.inject(produced_event) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn warp(&mut self, screen: u16, x: f32, y: f32) -> Result<(), SinkError> {
        self.inner.warp(screen, x, y)
    }

    fn release_all(&mut self) -> Result<(), SinkError> {
        // 引擎运行态与 OS 已注入键一一对应：整体释放即整体重置（同一原子语义）。
        self.engine.reset();
        self.inner.release_all()
    }

    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        // 只读查询：映射引擎只改写注入事件流，对光标位置语义无涉——直通。
        self.inner.cursor_normalized()
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_config::{RuleAction, RuleTrigger};
    use xcontrol_proto::input::{Axis, MouseButton};
    use xcontrol_proto::xkey::{
        CK_MUTE, KEY_3, KEY_A, KEY_C, KEY_CAPS_LOCK, KEY_ESCAPE, KEY_LEFT_ALT, KEY_LEFT_CTRL,
        KEY_LEFT_META, KEY_LEFT_SHIFT, KEY_RIGHT_CTRL, KEY_RIGHT_META, KEY_X,
    };

    use super::*;

    // —— 构造辅助 ——

    /// mac2win 引擎（含可选自定义规则）。
    fn mac2win(custom: &[KeymapRule]) -> MappingEngine {
        MappingEngine::new("mac2win", custom, false).unwrap()
    }

    fn key(code: XKey, state: KeyState) -> InputEvent {
        InputEvent::Key { code, state }
    }

    fn down(code: XKey) -> InputEvent {
        key(code, KeyState::Down)
    }

    fn up(code: XKey) -> InputEvent {
        key(code, KeyState::Up)
    }

    /// 逐步喂入事件序列并拼接全部产出。
    fn run(engine: &mut MappingEngine, events: &[InputEvent]) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for event in events {
            out.extend(engine.step(event));
        }
        out
    }

    /// v3 形态自定义规则构造捷径（trigger/action 各只填目标字段）。
    fn custom(trigger: RuleTrigger, action: RuleAction) -> KeymapRule {
        KeymapRule::new(trigger, action)
    }

    /// {LMeta}+KEY → {LCtrl,LSHIFT}+KEY 形态的组合规则。
    fn cmd_rule(key_name: &str, target: &str) -> KeymapRule {
        custom(
            RuleTrigger {
                mods: Some(vec!["LeftMeta".to_owned()]),
                key: Some(key_name.to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                mods: Some(vec!["LeftCtrl".to_owned(), "LeftShift".to_owned()]),
                key: Some(target.to_owned()),
                ..RuleAction::default()
            },
        )
    }

    /// 本地记录型 sink（route.rs MockSink 同风格；不依赖 mock-platform feature）。
    #[derive(Default)]
    struct RecordingSink {
        events: Vec<InputEvent>,
        releases: usize,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self::default()
        }
    }

    impl InputSink for RecordingSink {
        fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
            self.events.push(event.clone());
            Ok(())
        }

        fn warp(&mut self, _screen: u16, _x: f32, _y: f32) -> Result<(), SinkError> {
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), SinkError> {
            self.releases += 1;
            Ok(())
        }
    }

    // —— FR-3.1 AC ——

    /// FR-3.1 AC：mac2win 下 `LMeta↓ C↓ C↑ LMeta↑` → 注入 `LCtrl↓ C↓ C↑ LCtrl↑`。
    #[test]
    fn fr31_mac2win_cmd_c_sequence() {
        let mut engine = mac2win(&[]);
        let out = run(
            &mut engine,
            &[
                down(KEY_LEFT_META),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_META),
            ],
        );
        assert_eq!(
            out,
            vec![
                down(KEY_LEFT_CTRL),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_CTRL)
            ]
        );
    }

    /// 恒等预设（none）：修饰键/普通键流 1:1 通过（恒等登记不改写序列）。
    #[test]
    fn none_preset_stream_is_identity() {
        let mut engine = MappingEngine::new("none", &[], false).unwrap();
        let events = vec![
            down(KEY_LEFT_META),
            down(KEY_C),
            up(KEY_C),
            up(KEY_LEFT_META),
            down(KEY_CAPS_LOCK),
            up(KEY_CAPS_LOCK),
        ];
        assert_eq!(run(&mut engine, &events), events);
    }

    // —— 错序释放防卡键（FR-3.1 AC 第二条）——

    /// 混用两条修饰键规则 + 交错按下/释放：两种释放顺序注入均正确。
    #[test]
    fn out_of_order_release_with_two_mod_rules() {
        // LMeta↓ LOpt↓ LOpt↑ LMeta↑（先释放后按下的）
        let mut engine = mac2win(&[]);
        let out = run(
            &mut engine,
            &[
                down(KEY_LEFT_META),
                down(KEY_LEFT_ALT),
                up(KEY_LEFT_ALT),
                up(KEY_LEFT_META),
            ],
        );
        assert_eq!(
            out,
            vec![
                down(KEY_LEFT_CTRL),
                down(KEY_LEFT_ALT),
                up(KEY_LEFT_ALT),
                up(KEY_LEFT_CTRL),
            ]
        );
        // LMeta↓ LOpt↓ LMeta↑ LOpt↑（后按下的先释放——错序）
        let mut engine = mac2win(&[]);
        let out = run(
            &mut engine,
            &[
                down(KEY_LEFT_META),
                down(KEY_LEFT_ALT),
                up(KEY_LEFT_META),
                up(KEY_LEFT_ALT),
            ],
        );
        assert_eq!(
            out,
            vec![
                down(KEY_LEFT_CTRL),
                down(KEY_LEFT_ALT),
                up(KEY_LEFT_CTRL),
                up(KEY_LEFT_ALT),
            ]
        );
    }

    /// 同目标引用计数（docs/03 §4.2 修订）：mac2linux 下 Cmd 与 Ctrl（恒等）都落
    /// 客户端 LCtrl——先松 Cmd 不得提前释放仍在按住的 Ctrl（抑制中间 ↑）。
    #[test]
    fn same_target_refcount_suppresses_premature_release() {
        let mut engine = MappingEngine::new("mac2linux", &[], false).unwrap();
        let out = run(
            &mut engine,
            &[
                down(KEY_LEFT_META), // → LCtrl↓（规则映射，hold=1）
                down(KEY_LEFT_CTRL), // → LCtrl↓（恒等登记，hold=2）
                up(KEY_LEFT_META),   // hold=1 → 抑制 ↑
                up(KEY_LEFT_CTRL),   // hold=0 → LCtrl↑
            ],
        );
        assert_eq!(
            out,
            vec![down(KEY_LEFT_CTRL), down(KEY_LEFT_CTRL), up(KEY_LEFT_CTRL)]
        );
    }

    /// 同目标引用计数·自定义双规则变体：LMeta→LCtrl 与 RMeta→LCtrl 两条规则
    /// 同落客户端 LCtrl——先松一侧不提前释放（docs/03 §4.2 修订的规则形态无关性）。
    #[test]
    fn refcount_across_two_custom_rules_to_same_target() {
        let left = custom(
            RuleTrigger {
                mod_: Some("LeftMeta".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                mod_: Some("LeftCtrl".to_owned()),
                ..RuleAction::default()
            },
        );
        let right = custom(
            RuleTrigger {
                mod_: Some("RightMeta".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                mod_: Some("LeftCtrl".to_owned()),
                ..RuleAction::default()
            },
        );
        let mut engine = MappingEngine::new("none", &[left, right], false).unwrap();
        let out = run(
            &mut engine,
            &[
                down(KEY_LEFT_META),
                down(KEY_RIGHT_META),
                up(KEY_LEFT_META),
                up(KEY_RIGHT_META),
            ],
        );
        assert_eq!(
            out,
            vec![down(KEY_LEFT_CTRL), down(KEY_LEFT_CTRL), up(KEY_LEFT_CTRL)]
        );
    }

    /// 重复 Down/Up 按 origin 幂等（docs/03 §4.2 修订）；无登记的 Up 不注入。
    #[test]
    fn duplicate_and_unmatched_mod_events_are_idempotent() {
        let mut engine = mac2win(&[]);
        let out = run(
            &mut engine,
            &[
                down(KEY_LEFT_META),
                down(KEY_LEFT_META), // 重复 Down：幂等
                up(KEY_LEFT_META),
                up(KEY_LEFT_META), // 重复 Up：无登记 → 不注入
            ],
        );
        assert_eq!(out, vec![down(KEY_LEFT_CTRL), up(KEY_LEFT_CTRL)]);
        let mut engine = mac2win(&[]);
        assert!(engine.step(&up(KEY_LEFT_META)).is_empty(), "从未按下的 Up");
    }

    /// pending 触发器的重复 Down 幂等（修复回归：放行到事件级路径会在 pending
    /// 中途注入 mapped 键，随后首次 Up 走 flush③ 无产出——修饰键卡在 OS 按下态）。
    #[test]
    fn duplicate_down_of_pending_trigger_is_idempotent() {
        // 重复 Down + 未命中 flush：补注入恰好一次
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        assert!(engine.step(&down(KEY_LEFT_META)).is_empty());
        assert!(
            engine.step(&down(KEY_LEFT_META)).is_empty(),
            "重复 Down：仍 pending 无注入"
        );
        assert_eq!(
            engine.step(&down(KEY_X)),
            vec![down(KEY_LEFT_CTRL), down(KEY_X)],
            "flush 补注入一次（非两次）"
        );
        assert_eq!(engine.step(&up(KEY_LEFT_META)), vec![up(KEY_LEFT_CTRL)]);
        assert!(
            engine.step(&up(KEY_LEFT_META)).is_empty(),
            "对应重复 Up 幂等"
        );

        // 重复 Down + 直接释放：flush③ 无产出、无残留运行态（修复前卡键）
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        run(&mut engine, &[down(KEY_LEFT_META), down(KEY_LEFT_META)]);
        assert!(engine.step(&up(KEY_LEFT_META)).is_empty(), "flush③ 无产出");
        assert!(engine.flush_pending().is_empty(), "无残留 pending");
        assert_eq!(
            engine.step(&down(KEY_C)),
            vec![down(KEY_C)],
            "held_mods 已清：C 直通无组合上下文"
        );
    }

    // —— 组合级（FR-3.3 / AC 第三条）——

    /// `Cmd+C → Ctrl+Shift+C` 原子弦改写（docs/03 §4.3 修订：一次性 tap，
    /// mods 位序 0→7 下、7→0 上）。
    #[test]
    fn combo_chord_hit_is_atomic() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        let out = run(&mut engine, &[down(KEY_LEFT_META), down(KEY_C)]);
        assert_eq!(
            out,
            vec![
                down(KEY_LEFT_CTRL),
                down(KEY_LEFT_SHIFT),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_SHIFT),
                up(KEY_LEFT_CTRL),
            ]
        );
        // 弦后物理 C↑：普通键对称路径（重复 ↑ 为平台 no-op）
        assert_eq!(engine.step(&up(KEY_C)), vec![up(KEY_C)]);
    }

    /// 命中后触发器保持 withheld：重复按键可再次命中（每根弦独立完整）。
    #[test]
    fn combo_repeat_after_hit() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        run(&mut engine, &[down(KEY_LEFT_META)]);
        let out = run(&mut engine, &[down(KEY_C), down(KEY_C)]);
        assert_eq!(out.len(), 12, "两根独立的完整弦");
        // 触发器从未注入 → 释放走 flush③：无注入
        assert!(engine.step(&up(KEY_LEFT_META)).is_empty());
    }

    /// `Cmd+X`（无规则）→ flush 退回 4.2 语义：`LCtrl↓ X↓`；随后 `LMeta↑` →
    /// `LCtrl↑`（flush 登记的 origin 生效）；flush 后再次命中组合 → 弦跳过
    /// 已注入的 LCtrl（不触碰 OS 已按住的键，模块文档裁定 3）。
    #[test]
    fn combo_miss_flush_then_hit_skips_injected_mods() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        let out = run(&mut engine, &[down(KEY_LEFT_META), down(KEY_X)]);
        assert_eq!(
            out,
            vec![down(KEY_LEFT_CTRL), down(KEY_X)],
            "flush 退回 LCtrl+X"
        );
        assert_eq!(engine.step(&up(KEY_X)), vec![up(KEY_X)]);
        // flush 补注入后再次命中：弦只 tap LShift 与 C，LCtrl 保持按住
        let out = engine.step(&down(KEY_C));
        assert_eq!(
            out,
            vec![
                down(KEY_LEFT_SHIFT),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_SHIFT),
            ]
        );
        // 释放：origin 换算 LCtrl↑（弦未破坏引用计数）
        assert_eq!(engine.step(&up(KEY_LEFT_META)), vec![up(KEY_LEFT_CTRL)]);
    }

    /// flush 路径③（docs/03 §4.3 修订）：触发器全释放仍无非修饰键 → 不产出注入，
    /// 仅吐缓存。
    #[test]
    fn flush_on_all_released_emits_nothing_but_cache() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        let out = run(&mut engine, &[down(KEY_LEFT_META), up(KEY_LEFT_META)]);
        assert!(out.is_empty(), "纯 tap：无产出");
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        let move_ev = InputEvent::MouseMoveRel { dx: 1.0, dy: 2.0 };
        assert!(run(&mut engine, &[down(KEY_LEFT_META), move_ev.clone()]).is_empty());
        assert_eq!(
            engine.step(&up(KEY_LEFT_META)),
            vec![move_ev],
            "flush③ 仅吐缓存"
        );
    }

    /// flush 路径2'（未命中）因果序：先补注入 pending 映射，再吐缓存，再注入当前键。
    #[test]
    fn combo_miss_order_downs_then_cache_then_key() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        let move_ev = InputEvent::MouseMoveRel { dx: 3.0, dy: 4.0 };
        run(&mut engine, &[down(KEY_LEFT_META), move_ev]);
        assert_eq!(
            engine.step(&down(KEY_X)),
            vec![
                down(KEY_LEFT_CTRL),
                InputEvent::MouseMoveRel { dx: 3.0, dy: 4.0 },
                down(KEY_X),
            ]
        );
    }

    /// pending 缓存上限 16：第 17 条缓存事件触发 flush（补注入 + 吐缓存），
    /// 溢出事件与后续事件立即注入；释放时 origin 换算的 ↑ 正常产出。
    #[test]
    fn pending_cache_overflow_flushes_at_16() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        run(&mut engine, &[down(KEY_LEFT_META)]);
        let mv = |n: f32| InputEvent::MouseMoveRel { dx: n, dy: 0.0 };
        let mut out = Vec::new();
        for n in 1u8..=16 {
            out.extend(engine.step(&mv(f32::from(n))));
        }
        assert!(out.is_empty(), "前 16 条全部缓存");
        let out = engine.step(&mv(17.0));
        let mut expect = vec![down(KEY_LEFT_CTRL)];
        expect.extend((1u8..=17).map(|n| mv(f32::from(n))));
        assert_eq!(
            out, expect,
            "第 17 条触发 flush：补注入 + 吐 16 缓存 + 溢出事件立即注入"
        );
        assert_eq!(engine.step(&mv(18.0)), vec![mv(18.0)], "后续直通");
        assert_eq!(engine.step(&up(KEY_LEFT_META)), vec![up(KEY_LEFT_CTRL)]);
    }

    /// 精确集合匹配：按住未列出的修饰键（事件级注入）时组合不命中 → flush 退回。
    #[test]
    fn combo_exact_set_match_excludes_unlisted_mods() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        let out = run(
            &mut engine,
            &[down(KEY_LEFT_SHIFT), down(KEY_LEFT_META), down(KEY_C)],
        );
        assert_eq!(
            out,
            vec![down(KEY_LEFT_SHIFT), down(KEY_LEFT_CTRL), down(KEY_C)],
            "集合 LMeta+LShift ≠ LMeta（未列出的 Shift 按下）：flush 补注入 LCtrl + C"
        );
    }

    /// drop 规则（形态 4）：命中即吞（不产出注入）；触发器同为 pending。
    #[test]
    fn drop_rule_swallows_event() {
        let drop_rule = custom(
            RuleTrigger {
                mods: Some(vec!["LeftMeta".to_owned(), "LeftShift".to_owned()]),
                key: Some("Digit3".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                drop: Some(true),
                ..RuleAction::default()
            },
        );
        let mut engine = mac2win(&[drop_rule]);
        let out = run(
            &mut engine,
            &[down(KEY_LEFT_META), down(KEY_LEFT_SHIFT), down(KEY_3)],
        );
        assert!(out.is_empty(), "drop 命中：无注入");
        assert_eq!(
            engine.step(&up(KEY_3)),
            vec![up(KEY_3)],
            "stray ↑ 平台 no-op"
        );
        assert!(run(&mut engine, &[up(KEY_LEFT_SHIFT), up(KEY_LEFT_META)]).is_empty());
    }

    /// 单键 drop（形态 1 的 `action.drop`）：Down/Up 双向吞（v2 `to = None` 语义）。
    #[test]
    fn single_key_drop_swallows_both_directions() {
        let mute = custom(
            RuleTrigger {
                key: Some("CK_MUTE".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                drop: Some(true),
                ..RuleAction::default()
            },
        );
        let mut engine = MappingEngine::new("none", &[mute], false).unwrap();
        assert!(run(&mut engine, &[down(CK_MUTE), up(CK_MUTE)]).is_empty());
        assert_eq!(
            engine.step(&down(KEY_A)),
            vec![down(KEY_A)],
            "其他键不受影响"
        );
        // drop 键的 KeyRepeat 同吞（"该键不存在于注入流"的完整语义）
        assert!(
            engine
                .step(&InputEvent::KeyRepeat { code: CK_MUTE })
                .is_empty(),
            "drop 键的 repeat 不注入"
        );
    }

    // —— 预设（FR-3.2 AC）——

    /// 8 预设规则表逐条断言（docs/03 §4.4 表；win2mac/linux2mac 在 XKey 空间同集）。
    #[test]
    fn preset_tables_match_docs() {
        let mac2win_expect = [
            (KEY_LEFT_META, KEY_LEFT_CTRL),
            (KEY_RIGHT_META, KEY_RIGHT_CTRL),
            (KEY_LEFT_ALT, KEY_LEFT_ALT),
            (KEY_RIGHT_ALT, KEY_RIGHT_ALT),
        ];
        let win2mac_expect = [
            (KEY_LEFT_CTRL, KEY_LEFT_META),
            (KEY_RIGHT_CTRL, KEY_RIGHT_META),
            (KEY_LEFT_META, KEY_LEFT_ALT),
            (KEY_RIGHT_META, KEY_RIGHT_ALT),
        ];
        let to_pairs = |rules: &'static [PresetRule]| -> Vec<(XKey, XKey)> {
            rules.iter().map(|rule| (rule.from, rule.to)).collect()
        };
        assert_eq!(to_pairs(preset_rules("mac2win").unwrap()), mac2win_expect);
        assert_eq!(to_pairs(preset_rules("mac2linux").unwrap()), mac2win_expect);
        assert_eq!(to_pairs(preset_rules("win2mac").unwrap()), win2mac_expect);
        assert_eq!(to_pairs(preset_rules("linux2mac").unwrap()), win2mac_expect);
        for empty in ["win2linux", "linux2win", "mac2mac", "none"] {
            assert!(preset_rules(empty).unwrap().is_empty(), "{empty} 恒等");
        }
        assert!(preset_rules("mac2dos").is_none(), "未知预设 None");
        // 行为抽查：win2mac 的 Ctrl+C → Meta+C（FR-3.1 由 fr31 用例覆盖 mac 侧）
        let mut engine = MappingEngine::new("win2mac", &[], false).unwrap();
        let out = run(
            &mut engine,
            &[
                down(KEY_LEFT_CTRL),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_CTRL),
            ],
        );
        assert_eq!(
            out,
            vec![
                down(KEY_LEFT_META),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_META)
            ]
        );
    }

    /// 自定义叠加在预设之后：不同键并存生效；同源键预设先命中（自定义被忽略）。
    #[test]
    fn custom_rules_overlay_after_preset() {
        let caps = custom(
            RuleTrigger {
                key: Some("CapsLock".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("Escape".to_owned()),
                ..RuleAction::default()
            },
        );
        // 与 mac2win 的 LMeta→LCtrl 同源：预设先命中，自定义被忽略
        let ignored = custom(
            RuleTrigger {
                mod_: Some("LeftMeta".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                mod_: Some("LeftAlt".to_owned()),
                ..RuleAction::default()
            },
        );
        let mut engine = mac2win(&[caps, ignored]);
        let out = run(
            &mut engine,
            &[
                down(KEY_CAPS_LOCK),
                up(KEY_CAPS_LOCK),
                down(KEY_LEFT_META),
                up(KEY_LEFT_META),
            ],
        );
        assert_eq!(
            out,
            vec![
                down(KEY_ESCAPE),
                up(KEY_ESCAPE),
                down(KEY_LEFT_CTRL),
                up(KEY_LEFT_CTRL),
            ]
        );
    }

    /// `preset = "none"` 纯自定义：单键重映射 + 组合改写并存。
    #[test]
    fn none_preset_custom_only_semantics() {
        let remap = custom(
            RuleTrigger {
                key: Some("CapsLock".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("Escape".to_owned()),
                ..RuleAction::default()
            },
        );
        let mut engine = MappingEngine::new("none", &[remap, cmd_rule("C", "C")], false).unwrap();
        let out = run(
            &mut engine,
            &[
                down(KEY_CAPS_LOCK),
                up(KEY_CAPS_LOCK),
                down(KEY_LEFT_META),
                down(KEY_C),
            ],
        );
        assert_eq!(
            out,
            vec![
                down(KEY_ESCAPE),
                up(KEY_ESCAPE),
                down(KEY_LEFT_CTRL),
                down(KEY_LEFT_SHIFT),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_SHIFT),
                up(KEY_LEFT_CTRL),
            ]
        );
    }

    // —— 直通（FR-3.6 AC）——

    /// 直通模式：全规则绕过，事件原样通过、状态机不参与。
    #[test]
    fn passthrough_bypasses_all_rules() {
        let mut engine = MappingEngine::new("mac2win", &[cmd_rule("C", "C")], true).unwrap();
        assert!(engine.passthrough());
        let events = vec![
            down(KEY_LEFT_META),
            down(KEY_C),
            up(KEY_C),
            up(KEY_LEFT_META),
            InputEvent::Text {
                utf8: "hi".to_owned(),
            },
        ];
        assert_eq!(run(&mut engine, &events), events);
    }

    // —— Text 分流（docs/03 §4.5）——

    /// Text 恒直通：无 pending 直通；pending 期间亦直通（不缓存不拦截）。
    #[test]
    fn text_bypasses_mapping_even_during_pending() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        let text = InputEvent::Text {
            utf8: "字".to_owned(),
        };
        assert_eq!(engine.step(&text), vec![text.clone()]);
        run(&mut engine, &[down(KEY_LEFT_META)]);
        assert_eq!(engine.step(&text), vec![text]);
    }

    // —— KeyRepeat / 鼠标 / 滚轮 ——

    /// KeyRepeat 按规则换算（单键重映射）；无规则恒等；pending 期间缓存，
    /// flush 时按 repeat 语义解析（重映射换算与直通路径一致）。
    #[test]
    fn key_repeat_follows_remap_and_pending_cache() {
        let remap = custom(
            RuleTrigger {
                key: Some("CapsLock".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("Escape".to_owned()),
                ..RuleAction::default()
            },
        );
        let mut engine = MappingEngine::new("none", &[remap], false).unwrap();
        let repeat = InputEvent::KeyRepeat {
            code: KEY_CAPS_LOCK,
        };
        assert_eq!(
            engine.step(&repeat),
            vec![InputEvent::KeyRepeat { code: KEY_ESCAPE }]
        );
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        run(&mut engine, &[down(KEY_LEFT_META)]);
        assert!(engine.step(&repeat).is_empty(), "pending 缓存 KeyRepeat");
        // mac2win 无 CapsLock 规则 → flush 吐缓存原码；重映射变体见
        // cached_key_repeat_is_resolved_on_flush。
        assert_eq!(
            engine.step(&up(KEY_LEFT_META)),
            vec![repeat],
            "flush③ 吐缓存"
        );
    }

    /// 缓存 KeyRepeat 在各 flush 路径按 repeat 语义解析（重映射换算），与直通
    /// 路径一致（修复回归：flush 曾吐 raw 码漏改写）。
    #[test]
    fn cached_key_repeat_is_resolved_on_flush() {
        let remap = custom(
            RuleTrigger {
                key: Some("CapsLock".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("Escape".to_owned()),
                ..RuleAction::default()
            },
        );
        let repeat = InputEvent::KeyRepeat {
            code: KEY_CAPS_LOCK,
        };
        let resolved = InputEvent::KeyRepeat { code: KEY_ESCAPE };
        let make =
            || MappingEngine::new("mac2win", &[remap.clone(), cmd_rule("C", "C")], false).unwrap();

        // flush③（触发器全释放）：缓存 repeat → Escape
        let mut engine = make();
        run(&mut engine, &[down(KEY_LEFT_META), repeat.clone()]);
        assert_eq!(engine.step(&up(KEY_LEFT_META)), vec![resolved.clone()]);

        // flush 2'（未命中）：补注入 → 缓存 repeat（解析）→ 当前键
        let mut engine = make();
        run(&mut engine, &[down(KEY_LEFT_META), repeat.clone()]);
        assert_eq!(
            engine.step(&down(KEY_X)),
            vec![down(KEY_LEFT_CTRL), resolved.clone(), down(KEY_X)]
        );

        // 组合命中：缓存 repeat（解析）先吐，弦后到（因果序）
        let mut engine = make();
        run(&mut engine, &[down(KEY_LEFT_META), repeat.clone()]);
        assert_eq!(
            engine.step(&down(KEY_C)),
            vec![
                resolved,
                down(KEY_LEFT_CTRL),
                down(KEY_LEFT_SHIFT),
                down(KEY_C),
                up(KEY_C),
                up(KEY_LEFT_SHIFT),
                up(KEY_LEFT_CTRL),
            ]
        );

        // 超限 flush：溢出的 repeat 本身也按新状态解析
        let mut engine = make();
        run(&mut engine, &[down(KEY_LEFT_META)]);
        let mv = |n: f32| InputEvent::MouseMoveRel { dx: n, dy: 0.0 };
        for n in 1u8..=16 {
            engine.step(&mv(f32::from(n)));
        }
        let mut expect = vec![down(KEY_LEFT_CTRL)];
        expect.extend((1u8..=16).map(|n| mv(f32::from(n))));
        expect.push(InputEvent::KeyRepeat { code: KEY_ESCAPE });
        assert_eq!(
            engine.step(&repeat),
            expect,
            "第 17 条触发 flush：补注入 + 吐 16 缓存 + 溢出 repeat 解析"
        );
    }

    /// 鼠标/滚轮无 pending 直通；事件字段原样保留。
    #[test]
    fn mouse_and_wheel_passthrough_without_pending() {
        let mut engine = mac2win(&[]);
        let wheel = InputEvent::Wheel {
            axis: Axis::Vertical,
            delta: -1.5,
        };
        let button = InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        };
        let abs = InputEvent::MouseMoveAbs {
            screen: 1,
            x: 0.5,
            y: 0.5,
        };
        assert_eq!(engine.step(&wheel), vec![wheel]);
        assert_eq!(engine.step(&button), vec![button]);
        assert_eq!(engine.step(&abs), vec![abs]);
    }

    // —— MappingSink / reset 协同（模块文档裁定 2）——

    /// MappingSink：注入动作先改写再下沉；warp 直通；release_all 原子完成
    /// "释放 + 引擎重置"——其后修饰键 ↑ 无 origin 可查（不产出虚假注入）。
    #[test]
    fn mapping_sink_wraps_and_resets_on_release_all() {
        let mut sink = MappingSink::new(mac2win(&[]), RecordingSink::new());
        sink.inject(&down(KEY_LEFT_META)).unwrap();
        sink.warp(1, 0.5, 0.5).unwrap();
        assert_eq!(sink.inner().events, vec![down(KEY_LEFT_CTRL)], "改写后下沉");
        sink.release_all().unwrap();
        assert_eq!(sink.inner().releases, 1);
        sink.inject(&up(KEY_LEFT_META)).unwrap();
        assert_eq!(
            sink.inner().events,
            vec![down(KEY_LEFT_CTRL)],
            "重置后的 ↑ 未下沉"
        );
    }

    /// flush_pending 经 sink：排空 pending 并注入产出。
    #[test]
    fn sink_flush_pending_drains_into_inner() {
        let mut sink = MappingSink::new(mac2win(&[cmd_rule("C", "C")]), RecordingSink::new());
        sink.inject(&down(KEY_LEFT_META)).unwrap();
        assert!(sink.inner().events.is_empty(), "pending 期间不注入");
        assert_eq!(sink.flush_pending(), 1, "补注入 LCtrl↓");
        assert_eq!(sink.inner().events, vec![down(KEY_LEFT_CTRL)]);
    }

    // —— EnterScreen 对齐协同（模块文档裁定 1）——

    /// 对齐合成事件（raw 语义）与 wire 事件统一走映射：对齐 LMeta↓ → 注入 LCtrl↓，
    /// 其后 wire C↓ → C↓、释放 → LCtrl↑（mac2win 下"对端语义平移"成立）。
    #[test]
    fn alignment_events_flow_through_engine() {
        let mut sink = MappingSink::new(mac2win(&[]), RecordingSink::new());
        sink.inject(&down(KEY_LEFT_META)).unwrap(); // 对齐合成 Down（快照 missing）
        sink.inject(&down(KEY_C)).unwrap(); // 用户按键
        sink.inject(&up(KEY_LEFT_META)).unwrap(); // 对齐/用户释放
        assert_eq!(
            sink.inner().events,
            vec![down(KEY_LEFT_CTRL), down(KEY_C), up(KEY_LEFT_CTRL)]
        );
    }

    /// 对齐"多余释放"方向：引擎已登记 origin（wire LMeta↓），快照为空 → 对齐合成
    /// LMeta↑ → 按 origin 换算 LCtrl↑（不过映射会注入 raw LMeta↑ 卡住 LCtrl）。
    #[test]
    fn alignment_extra_release_resolves_via_origin() {
        let mut engine = mac2win(&[]);
        engine.step(&down(KEY_LEFT_META));
        assert_eq!(engine.step(&up(KEY_LEFT_META)), vec![up(KEY_LEFT_CTRL)]);
    }

    // —— 引擎构造错误 ——

    /// 未知预设/未知键名/非法形态三类错误可编程区分；identity 兜底恒等。
    #[test]
    fn constructor_errors_are_distinguishable() {
        assert!(matches!(
            MappingEngine::new("mac2dos", &[], false),
            Err(MappingError::UnknownPreset { .. })
        ));
        let bogus = custom(
            RuleTrigger {
                key: Some("Bogus".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("Escape".to_owned()),
                ..RuleAction::default()
            },
        );
        assert!(matches!(
            MappingEngine::new("none", &[bogus], false),
            Err(MappingError::UnknownKeyName { rule: 0, .. })
        ));
        let mod_as_key = custom(
            RuleTrigger {
                key: Some("LeftMeta".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("Escape".to_owned()),
                ..RuleAction::default()
            },
        );
        assert!(matches!(
            MappingEngine::new("none", &[mod_as_key], false),
            Err(MappingError::InvalidRule { rule: 0, .. })
        ));
        let empty_mods = custom(
            RuleTrigger {
                mods: Some(vec![]),
                key: Some("C".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("C".to_owned()),
                ..RuleAction::default()
            },
        );
        assert!(matches!(
            MappingEngine::new("none", &[empty_mods], false),
            Err(MappingError::InvalidRule { rule: 0, .. })
        ));
        let mut engine = MappingEngine::identity();
        assert!(!engine.passthrough());
        assert_eq!(engine.step(&down(KEY_LEFT_META)), vec![down(KEY_LEFT_META)]);
    }

    /// flush_pending 公开 API：补注入 pending 映射（登记 origin）→ 后续释放正确。
    #[test]
    fn flush_pending_public_api_registers_origin() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        assert!(engine.step(&down(KEY_LEFT_META)).is_empty());
        assert_eq!(engine.flush_pending(), vec![down(KEY_LEFT_CTRL)]);
        assert_eq!(engine.step(&up(KEY_LEFT_META)), vec![up(KEY_LEFT_CTRL)]);
        assert!(engine.flush_pending().is_empty(), "排空后再 flush 无产出");
    }

    /// reset 清空运行态：origin/pending/held 全归零（与 release_all 同语义的
    /// 直接调用路径）。
    #[test]
    fn reset_clears_runtime_state() {
        let mut engine = mac2win(&[cmd_rule("C", "C")]);
        engine.step(&down(KEY_LEFT_META)); // withheld
        engine.reset();
        assert!(engine.flush_pending().is_empty(), "pending 已清");
        // held_mods 已清 → C 直通（无组合上下文）
        assert_eq!(engine.step(&down(KEY_C)), vec![down(KEY_C)]);
    }

    // —— 属性不变式（docs/03 §6.2 的 proptest 替身：core 零新依赖红线，
    //     以种子确定性伪随机序列驱动同一不变式——配对性 + 无过早释放）——

    /// mulberry32 种子伪随机（仅测试；确定性可复现）。
    struct Rng(u32);

    impl Rng {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_add(0x6D2B_79F5);
            let mut t = self.0;
            t = t.wrapping_mul((t ^ (t >> 15)) | 1);
            t ^= t.wrapping_add(t.wrapping_mul((t ^ (t >> 7)) | 61));
            t ^ (t >> 14)
        }

        fn below(&mut self, n: u32) -> u32 {
            self.next_u32() % n
        }
    }

    /// 随机修饰键/按键序列经映射后：注入流的修饰键 **OS 状态配对性**不变式
    /// （docs/03 §6.2；同目标引用计数下事件级"每 ↓ 配一 ↑"不成立——抑制中间 ↑
    /// 是设计语义，OS 侧模型为布尔按下态）：每个注入 ↑ 之前该 mapped 键必须处于
    /// 按下态（不存在"无中生有的抬起"），序列终点全部键处于抬起态（不卡键）。
    #[test]
    fn randomized_modifier_pairing_invariant_holds() {
        const SEEDS: u32 = 64;
        const OPS: usize = 120;
        let mod_pool = [KEY_LEFT_META, KEY_RIGHT_META, KEY_LEFT_ALT, KEY_LEFT_CTRL];
        let key_pool = [KEY_C, KEY_X, KEY_A];
        let chord = cmd_rule("C", "C");
        for seed in 0..SEEDS {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9) | 1);
            let mut engine = mac2win(std::slice::from_ref(&chord));
            let mut pressed_mods: Vec<XKey> = Vec::new();
            let mut pressed_keys: Vec<XKey> = Vec::new();
            // OS 侧修饰键按下态（按 mapped 键 bit；布尔——OS 键态幂等，非计数）
            let mut os_down = [false; 8];
            let feed = |event: &InputEvent, engine: &mut MappingEngine, os_down: &mut [bool; 8]| {
                for out in engine.step(event) {
                    if let InputEvent::Key { code, state } = out
                        && let Some(bit) = code.mods_bit()
                    {
                        match state {
                            KeyState::Down => os_down[usize::from(bit)] = true,
                            KeyState::Up => {
                                assert!(
                                    os_down[usize::from(bit)],
                                    "seed {seed}: {code:?} 的 ↑ 无对应按下态（无中生有的抬起）"
                                );
                                os_down[usize::from(bit)] = false;
                            }
                        }
                    }
                }
            };
            for _ in 0..OPS {
                match rng.below(10) {
                    0..=3 if pressed_mods.len() < 4 => {
                        let code = mod_pool[rng.below(4) as usize];
                        if !pressed_mods.contains(&code) {
                            pressed_mods.push(code);
                            feed(&down(code), &mut engine, &mut os_down);
                        }
                    }
                    4 if !pressed_mods.is_empty() => {
                        let index = rng.below(pressed_mods.len() as u32) as usize;
                        let code = pressed_mods.remove(index);
                        feed(&up(code), &mut engine, &mut os_down);
                    }
                    5..=6 if pressed_keys.len() < 3 => {
                        let code = key_pool[rng.below(3) as usize];
                        if !pressed_keys.contains(&code) {
                            pressed_keys.push(code);
                            feed(&down(code), &mut engine, &mut os_down);
                        }
                    }
                    7 if !pressed_keys.is_empty() => {
                        let index = rng.below(pressed_keys.len() as u32) as usize;
                        let code = pressed_keys.remove(index);
                        feed(&up(code), &mut engine, &mut os_down);
                    }
                    _ => {
                        feed(
                            &InputEvent::MouseMoveRel { dx: 1.0, dy: 1.0 },
                            &mut engine,
                            &mut os_down,
                        );
                    }
                }
            }
            // 收尾：全部释放（随机顺序）后不变式终点归零
            while let Some(index) =
                (!pressed_keys.is_empty()).then(|| rng.below(pressed_keys.len() as u32) as usize)
            {
                let code = pressed_keys.remove(index);
                feed(&up(code), &mut engine, &mut os_down);
            }
            while let Some(index) =
                (!pressed_mods.is_empty()).then(|| rng.below(pressed_mods.len() as u32) as usize)
            {
                let code = pressed_mods.remove(index);
                feed(&up(code), &mut engine, &mut os_down);
            }
            assert!(
                os_down.iter().all(|is_down| !is_down),
                "seed {seed}: 序列终点仍有修饰键未释放：{os_down:?}"
            );
        }
    }

    // —— 热替换（docs/06 §4.1 keymap 即时生效行，11-21 反馈 4）——

    /// set_rules：规则立即替换（旧映射失活、新映射激活）且已注入修饰键的
    /// 释放仍按 origin 对照表正确换算（防卡键语义跨替换延续）。
    #[test]
    fn set_rules_swaps_table_and_keeps_origin_release() {
        let mut engine = mac2win(&[]);
        // 旧规则：LMeta 已按下 → 注入 LCtrl（origin 登记）。
        assert_eq!(
            run(&mut engine, &[down(KEY_LEFT_META)]),
            vec![down(KEY_LEFT_CTRL)]
        );
        // 热替换为 win2mac：LMeta → LAlt。
        engine
            .set_rules("win2mac", &[], false)
            .expect("win2mac 预设编译");
        // 新按下按新规则。
        assert_eq!(
            run(&mut engine, &[down(KEY_RIGHT_META)]),
            vec![down(KEY_RIGHT_ALT)]
        );
        // 旧按下的 LMeta 释放：按 origin 对照表换算回 LCtrl ↑（不卡键）。
        assert_eq!(
            run(&mut engine, &[up(KEY_LEFT_META)]),
            vec![up(KEY_LEFT_CTRL)]
        );
        // 替换后按下的 RMeta 释放：origin 表登记的 RMeta→RAlt（按下时刻的
        // 规则）换算 RAlt ↑。
        assert_eq!(
            run(&mut engine, &[up(KEY_RIGHT_META)]),
            vec![up(KEY_RIGHT_ALT)]
        );
    }

    /// set_rules：编译失败保持旧规则原样（输入管线不因坏配置中断）。
    #[test]
    fn set_rules_failure_keeps_old_rules() {
        let mut engine = mac2win(&[]);
        // 坏预设：Err 且引擎仍按旧规则映射。
        assert!(matches!(
            engine.set_rules("bogus-preset", &[], false),
            Err(MappingError::UnknownPreset { .. })
        ));
        assert_eq!(
            run(&mut engine, &[down(KEY_LEFT_META)]),
            vec![down(KEY_LEFT_CTRL)]
        );
        // 坏自定义键名：同语义（预设 none + 非法规则整体失败，不半替换）。
        let bad = custom(
            RuleTrigger {
                key: Some("NotAKeyName".to_owned()),
                ..RuleTrigger::default()
            },
            RuleAction {
                key: Some("Escape".to_owned()),
                ..RuleAction::default()
            },
        );
        assert!(matches!(
            engine.set_rules("none", std::slice::from_ref(&bad), false),
            Err(MappingError::UnknownKeyName { .. })
        ));
        // 旧规则（mac2win）原样：先释放再按下仍走 LMeta→LCtrl。
        assert_eq!(
            run(&mut engine, &[up(KEY_LEFT_META)]),
            vec![up(KEY_LEFT_CTRL)]
        );
        assert_eq!(
            run(&mut engine, &[down(KEY_LEFT_META)]),
            vec![down(KEY_LEFT_CTRL)]
        );
    }

    /// set_rules：passthrough 立即切换（step 短路回显；后续释放不再换算——
    /// 边界语义见 set_rules 文档）。
    #[test]
    fn set_rules_switches_passthrough() {
        let mut engine = mac2win(&[]);
        engine
            .set_rules("mac2win", &[], true)
            .expect("同预设直通切换");
        assert!(engine.passthrough());
        // 直通：原始事件回显，无改写。
        assert_eq!(
            run(&mut engine, &[down(KEY_LEFT_META), down(KEY_C)]),
            vec![down(KEY_LEFT_META), down(KEY_C)]
        );
    }

    /// set_rules：组合级 pending 期间替换——withheld 触发器保持未注入，物理
    /// Up 仍走既有 flush 路径③（仅吐缓存、不产出注入）；后续按键按新表判定。
    #[test]
    fn set_rules_during_pending_flushes_safely() {
        // 构造带组合规则的引擎：{LMeta}+C → 弦 {LCtrl,LSHIFT}+X。
        let mut engine = mac2win(&[cmd_rule("C", "X")]);
        // LMeta 是组合触发器 → pending（不注入）。
        assert_eq!(run(&mut engine, &[down(KEY_LEFT_META)]), Vec::new());
        // pending 中热替换为恒等预设（无组合规则）。
        engine.set_rules("none", &[], false).expect("none 预设编译");
        // C 走无组合路径按新表恒等注入；withheld 的 LMeta 保持未注入（其
        // flush 只在物理 Up 到达时发生）。
        assert_eq!(
            run(&mut engine, &[down(KEY_C)]),
            vec![down(KEY_C)],
            "替换后的按键按新表恒等注入"
        );
        // LMeta 释放：flush 路径③（缓存空）→ 无产出——pending Down 从未注入，
        // 不产虚假 ↑（docs/03 §4.3 修订裁定）。
        assert_eq!(run(&mut engine, &[up(KEY_LEFT_META)]), Vec::new());
        // 状态机运行态归零后恢复正常（重复按下走事件级恒等）。
        assert_eq!(
            run(&mut engine, &[down(KEY_LEFT_META)]),
            vec![down(KEY_LEFT_META)]
        );
    }
}
