//! 映射页（FR-3.6，docs/06 §4「按键映射」；11-21 实测反馈重构——功能键选择器）。
//!
//! ```text
//! 预设下拉（快速填充） + 直通开关（passthrough）
//! 功能键映射（主 UI）：5 行「主控按键 → 本机映射为」下拉
//!   Ctrl / Shift / Alt / Command(Win) —— 事件级修饰键映射（docs/03 §4.2 形态 2）
//!   Tab —— 单键但目标必须是修饰键的语义不被引擎支持 → 禁用 + 说明
//! 高级 · 自定义规则（折叠区，默认收起）：规则表（展示 + 删除） + 单条新增表单
//! ```
//!
//! 数据流（规则引擎/配置模型零改动——UI 生成的就是标准 mod 规则）：
//!
//! ```text
//! 预设下拉选择 → commit preset 名 → 选择器从 preset_rules 反解填充（render 期）
//! 用户改动任一功能键选择器 → commit：preset = "none" + 由 5 个选择器重建
//!   全部事件级修饰键规则（形态 2，左右手成对）写入 config.rules（其余形态保留）
//! ```
//!
//! 生效路径（docs/06 §4.1，11-21 反馈 4）：改动 → `commit_config` → TOML 落盘 +
//! `RuntimeConfigUpdate` 下发 → **即时生效**（运行中会话映射规则热替换——
//! `apply_hot_key_configs`；无活动会话时下会话构造点生效）。
//!
//! 预设反解（`selectors_from_config`）：`preset != "none"` 时选择器展示
//! `xcontrol_core::mapping::preset_rules` 的有效映射（预设规则先命中自定义，
//! docs/03 §4.4）；`preset == "none"` 时展示 config 中既有的形态 2 规则。
//! 每组取左手规则的映射方向（简化 UI 不分左右；右手规则可在高级区核对）。
//!
//! 新增规则校验（就地提示，docs/06 §4）：键名经 [`xcontrol_config::parse_key_name`]
//! 解析（语义键名文法，config v3）；trigger/action 必须非修饰键（修饰键映射由
//! 上方功能键选择器承载），action 支持 `drop`（形态 4 丢弃）。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, IndexPath, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, FontWeight, SharedString, Window, div, px};
use xcontrol_config::{Config, KeymapRule, RuleAction, RuleTrigger, parse_key_name};
use xcontrol_core::mapping::{PresetRule, preset_rules};
use xcontrol_proto::xkey::{
    KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_LEFT_SHIFT, KEY_RIGHT_ALT, KEY_RIGHT_CTRL,
    KEY_RIGHT_META, KEY_RIGHT_SHIFT, XKey,
};

use super::SettingsView;

/// 功能键组（左右手成对的修饰键；docs/03 §2 语义键名，不分左右的 UI 简化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuncKey {
    /// Ctrl（LCtrl/RCtrl）。
    Ctrl,
    /// Shift。
    Shift,
    /// Alt（mac Option 与之同 HID 键码）。
    Alt,
    /// Win / Command（XKey 语义 Meta）。
    Meta,
}

impl FuncKey {
    /// 组内键（序 = [左手, 右手]）。
    fn keys(self) -> [XKey; 2] {
        match self {
            Self::Ctrl => [KEY_LEFT_CTRL, KEY_RIGHT_CTRL],
            Self::Shift => [KEY_LEFT_SHIFT, KEY_RIGHT_SHIFT],
            Self::Alt => [KEY_LEFT_ALT, KEY_RIGHT_ALT],
            Self::Meta => [KEY_LEFT_META, KEY_RIGHT_META],
        }
    }

    /// 组内语义键名（config 规则用；docs/03 §2 规范全名）。
    fn names(self) -> [&'static str; 2] {
        match self {
            Self::Ctrl => ["LeftCtrl", "RightCtrl"],
            Self::Shift => ["LeftShift", "RightShift"],
            Self::Alt => ["LeftAlt", "RightAlt"],
            Self::Meta => ["LeftMeta", "RightMeta"],
        }
    }

    /// 全部组（下拉目标选项序 = [`TARGET_OPTIONS`] 去掉 0 位后的对齐序）。
    const ALL: [Self; 4] = [Self::Ctrl, Self::Shift, Self::Alt, Self::Meta];

    /// wire 值 → 组（反解用）。
    fn from_key(key: XKey) -> Option<Self> {
        Self::ALL.iter().copied().find(|group| {
            group
                .keys()
                .iter()
                .any(|candidate| candidate.raw() == key.raw())
        })
    }
}

/// 功能键行定义（渲染序 = 页面行序；`None` = Tab 特殊行——引擎不支持
/// Tab→修饰键映射（Tab 非修饰键，形态 2 不可用），该行禁用并说明）。
const FUNC_ROWS: [(&str, Option<FuncKey>); 5] = [
    ("mapping.row_ctrl", Some(FuncKey::Ctrl)),
    ("mapping.row_shift", Some(FuncKey::Shift)),
    ("mapping.row_alt", Some(FuncKey::Alt)),
    ("mapping.row_meta", Some(FuncKey::Meta)),
    ("mapping.row_tab", None),
];

/// 目标下拉选项（文案 key 序；0 位 = 不映射，其余与 [`FuncKey::ALL`] 对齐）。
const TARGET_OPTIONS: [&str; 5] = [
    "mapping.target_none",
    "mapping.target_ctrl",
    "mapping.target_shift",
    "mapping.target_alt",
    "mapping.target_meta",
];

/// 目标下拉选项文案（构造期快照；FR-6.5 文案表）。
fn target_options() -> Vec<String> {
    TARGET_OPTIONS
        .iter()
        .map(|key| xcontrol_i18n::tr(key).to_owned())
        .collect()
}

/// 选项下标 → 目标组（0 位 = 不映射）。
fn target_from_index(index: usize) -> Option<FuncKey> {
    FuncKey::ALL.get(index.wrapping_sub(1)).copied()
}

/// 目标组 → 选项下标（不映射 = 0）。
fn index_from_target(target: Option<FuncKey>) -> usize {
    target.map_or(0, |group| {
        FuncKey::ALL
            .iter()
            .position(|candidate| *candidate == group)
            .unwrap_or(0)
            + 1
    })
}

/// 映射页控件状态（视图构造期创建——SelectState/InputState 需要 window）。
pub(crate) struct MappingPage {
    /// 预设下拉（8 项，docs/03 §4.4；快速填充面——选中即 commit preset 名）。
    preset_select: Entity<SelectState<Vec<String>>>,
    /// 功能键映射下拉（5 行，选项 = [`TARGET_OPTIONS`]；行序 = [`FUNC_ROWS`]）。
    pub(crate) func_selects: [Entity<SelectState<Vec<String>>>; 5],
    /// 选择器填充基准（上次同步的 preset 名；render 期与 config 比对触发重填充
    /// ——预设切换/选择器重建后回填一致态）。
    applied_preset: String,
    /// 高级（自定义规则）折叠区展开态（默认收起——11-21 实测反馈）。
    pub(crate) advanced_open: bool,
    /// 新增规则：trigger 键名输入（测试经实体 setup 内容，交互面在按钮）。
    pub(crate) trigger_input: Entity<InputState>,
    /// 新增规则：action 键名输入（`drop` = 丢弃）。
    pub(crate) action_input: Entity<InputState>,
}

impl MappingPage {
    pub(super) fn new(
        config: &Config,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let presets: Vec<String> = xcontrol_config::KEYMAP_PRESETS
            .iter()
            .map(|preset| (*preset).to_owned())
            .collect();
        // 初始选中项 = 当前 config 预设（未知名回落 0 位——校验保证不会发生）。
        let selected = xcontrol_config::KEYMAP_PRESETS
            .iter()
            .position(|p| *p == config.keymap.preset)
            .unwrap_or(0);
        let preset_select = cx.new(|cx| {
            SelectState::new(
                presets,
                Some(gpui_kit::component::IndexPath::new(selected)),
                window,
                cx,
            )
        });
        let options = target_options();
        // 功能键选择器初始态 = config 有效映射（预设反解 / 自定义形态 2 反解）。
        let selection = selectors_from_config(config);
        let func_selects = FUNC_ROWS.map(|_| {
            cx.new(|cx| SelectState::new(options.clone(), Some(IndexPath::new(0)), window, cx))
        });
        for (index, select) in func_selects.iter().enumerate() {
            select.update(cx, |state, cx| {
                state.set_selected_index(
                    Some(IndexPath::new(index_from_target(selection[index]))),
                    window,
                    cx,
                );
            });
        }
        Self {
            preset_select,
            func_selects,
            applied_preset: config.keymap.preset.clone(),
            advanced_open: false,
            trigger_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("mapping.ph_trigger"))
            }),
            action_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("mapping.ph_action"))
            }),
        }
    }
}

/// 控件订阅（预设选择 → commit preset；功能键选择 → commit 规则重建）。
pub(super) fn subscribe(page: &MappingPage, cx: &mut Context<SettingsView>) {
    cx.subscribe(&page.preset_select, |view, _entity, event, cx| {
        let SelectEvent::Confirm(Some(preset)) = event else {
            return;
        };
        tracing::info!(target: "app::settings", preset, "映射预设变更（即时生效，docs/06 §4.1）");
        let preset = preset.clone();
        view.commit_config(xcontrol_i18n::tr("label.mapping_preset"), |config| {
            config.keymap.preset = preset;
            Ok(())
        });
        cx.notify();
    })
    .detach();
    // 功能键选择器（5 行）：任一改动 → 选择器全集重建规则 + 预设切 none
    //（自定义语义——docs/03 §4.4「需覆盖预设请用 preset = none」由 UI 自动落）。
    for (index, select) in page.func_selects.iter().enumerate() {
        cx.subscribe(select, move |view, _entity, event, cx| {
            // SelectEvent 恒为 Confirm（单变体枚举）；match 表达意图并避免
            // irrefutable let 告警。
            match event {
                SelectEvent::Confirm(_) => {}
            }
            tracing::info!(target: "app::settings", row = index,
                "功能键映射变更（预设切 none + 修饰键规则重建，即时生效）");
            commit_func_selection(view, cx);
            cx.notify();
        })
        .detach();
    }
}

/// 从页面选择器读出目标矩阵（行序 = [`FUNC_ROWS`]；显示名 → 组，
/// layout.rs `parse_direction` 同款显示名解析模式）。
///
/// 语言敏感性边界（勿破坏）：选项串按构造期语言快照存取，此处按**当前**语言
/// 反查——两者不一致时仅「不映射/No mapping」存在跨语言文案差，而未命中的
/// 回退值恰为 `None`（= 不映射语义）；其余选项文案 zh/en 全等（Ctrl/Shift/
/// Alt/Win / Command）。若未来把某个**非 none** 选项文案改成跨语言不同，
/// 必须改为按下标（IndexPath）存取而非显示串比对。
fn read_func_selection(page: &MappingPage, cx: &gpui_kit::App) -> [Option<FuncKey>; 5] {
    let mut selection = [None; 5];
    for (index, select) in page.func_selects.iter().enumerate() {
        let Some(value) = select.read(cx).selected_value() else {
            continue;
        };
        let value = value.clone();
        if let Some(position) = TARGET_OPTIONS
            .iter()
            .position(|key| xcontrol_i18n::tr(key) == value.as_str())
        {
            selection[index] = target_from_index(position);
        }
    }
    selection
}

/// 选择器改动提交（docs/06 §4.1 生效框架）：预设切 `none` + 修饰键规则重建
///（形态 1/3/4 自定义规则原样保留——选择器只拥有修饰键映射面）。
fn commit_func_selection(view: &mut SettingsView, cx: &mut Context<SettingsView>) {
    let selection = read_func_selection(&view.mapping, cx);
    view.commit_config(xcontrol_i18n::tr("mapping.func_keys_title"), |config| {
        config.keymap.preset = "none".to_owned();
        let kept: Vec<KeymapRule> = config
            .keymap
            .rules
            .drain(..)
            .filter(|rule| rule.trigger.mod_.is_none())
            .collect();
        config.keymap.rules = kept;
        config
            .keymap
            .rules
            .extend(func_selection_to_rules(&selection));
        Ok(())
    });
}

/// config 有效映射 → 功能键选择器矩阵（行序 = [`FUNC_ROWS`]；Tab 行恒不映射）。
///
/// `preset != "none"`：从 [`preset_rules`] 反解（预设规则先命中自定义，docs/03
/// §4.4）；`preset == "none"`：从 config 既有的形态 2 规则反解（每组取左手
/// 规则——简化 UI 不分左右）。规则中每组首个命中者生效（与引擎 first-match
/// 语义一致）。
#[must_use]
fn selectors_from_config(config: &Config) -> [Option<FuncKey>; 5] {
    let mut selection = [None; 5];
    if config.keymap.preset != "none" {
        if let Some(preset_table) = preset_rules(&config.keymap.preset) {
            for PresetRule { from, to } in preset_table {
                let Some(source) = FuncKey::from_key(*from) else {
                    continue;
                };
                let target = if from.raw() == to.raw() {
                    None
                } else {
                    FuncKey::from_key(*to)
                };
                set_selector(&mut selection, source, target);
            }
        }
        return selection;
    }
    // 自定义形态 2 规则反解：trigger.mod → 行；action.mod → 目标。
    for rule in &config.keymap.rules {
        let (Some(trigger_mod), Some(action_mod)) = (&rule.trigger.mod_, &rule.action.mod_) else {
            continue;
        };
        let (Some(trigger_raw), Some(action_raw)) =
            (parse_key_name(trigger_mod), parse_key_name(action_mod))
        else {
            continue;
        };
        let (Some(trigger_key), Some(action_key)) = (XKey::new(trigger_raw), XKey::new(action_raw))
        else {
            continue;
        };
        let Some(source) = FuncKey::from_key(trigger_key) else {
            continue;
        };
        let target = if trigger_raw == action_raw {
            None
        } else {
            FuncKey::from_key(action_key)
        };
        set_selector(&mut selection, source, target);
    }
    selection
}

/// 组 → 行槽写入选定目标（组内首条规则生效——不覆盖已写值）。
fn set_selector(selection: &mut [Option<FuncKey>; 5], source: FuncKey, target: Option<FuncKey>) {
    if let Some(row) = FUNC_ROWS
        .iter()
        .position(|(_, group)| *group == Some(source))
        && selection[row].is_none()
    {
        selection[row] = target;
    }
}

/// 选择器矩阵 → 事件级修饰键规则（docs/03 §4.1 形态 2，左右手成对）：
/// 目标与源同组（恒等）或不映射 → 不产出规则（恒等即默认行为）。
#[must_use]
fn func_selection_to_rules(selection: &[Option<FuncKey>; 5]) -> Vec<KeymapRule> {
    let mut rules = Vec::new();
    for (row, (_, source)) in FUNC_ROWS.iter().enumerate() {
        let Some(source) = source else {
            continue; // Tab 行：引擎不支持 Tab→修饰键（模块文档），不产规则。
        };
        let Some(target) = selection[row] else {
            continue;
        };
        if target == *source {
            continue; // 恒等映射 = 默认行为，无需规则。
        }
        for hand in 0..2 {
            rules.push(KeymapRule::new(
                RuleTrigger {
                    mod_: Some(source.names()[hand].to_owned()),
                    ..RuleTrigger::default()
                },
                RuleAction {
                    mod_: Some(target.names()[hand].to_owned()),
                    ..RuleAction::default()
                },
            ));
        }
    }
    rules
}

/// 规则摘要（规则表行文本；trigger → action 的人类可读形态；FR-6.5 文案表）。
fn rule_summary(rule: &KeymapRule) -> String {
    let trigger = if let Some(mod_) = &rule.trigger.mod_ {
        xcontrol_i18n::trf("mapping.modifier", &[("mod", mod_)])
    } else {
        match (&rule.trigger.key, &rule.trigger.mods) {
            (Some(key), Some(mods)) => format!("{}+{key}", mods.join("+")),
            (Some(key), None) => key.clone(),
            (None, Some(mods)) => mods.join("+"),
            (None, None) => xcontrol_i18n::tr("mapping.empty").to_owned(),
        }
    };
    let action = if rule.action.drop == Some(true) {
        xcontrol_i18n::tr("mapping.drop").to_owned()
    } else if let Some(mod_) = &rule.action.mod_ {
        xcontrol_i18n::trf("mapping.modifier", &[("mod", mod_)])
    } else {
        match (&rule.action.key, &rule.action.mods) {
            (Some(key), Some(mods)) => format!("{}+{key}", mods.join("+")),
            (Some(key), None) => key.clone(),
            (None, Some(mods)) => mods.join("+"),
            (None, None) => xcontrol_i18n::tr("mapping.empty").to_owned(),
        }
    };
    format!("{trigger} → {action}")
}

/// 渲染映射页。
pub(super) fn render(
    view: &mut SettingsView,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let keymap = view.config.keymap.clone();

    // 选择器与预设基准同步（render 期填充——set_selected_index 需要 window；
    // 预设切换/规则重建后回填一致态，set_selected_index 不发 Confirm 不成环）。
    if view.mapping.applied_preset != keymap.preset {
        let selection = selectors_from_config(&view.config);
        for (index, select) in view.mapping.func_selects.iter().enumerate() {
            let target_index = index_from_target(selection[index]);
            select.update(cx, |state, cx| {
                state.set_selected_index(Some(IndexPath::new(target_index)), window, cx);
            });
        }
        view.mapping.applied_preset = keymap.preset.clone();
    }

    // 预设下拉（快速填充面）。
    let preset_select = Select::new(&view.mapping.preset_select)
        .accessibility_label(xcontrol_i18n::tr("label.mapping_preset"))
        .placeholder(xcontrol_i18n::tr("mapping.ph_preset"));

    let mut page = v_flex()
        .gap_3()
        .child(
            div()
                .child(xcontrol_i18n::tr("mapping.intro"))
                .text_size(px(12.))
                .text_color(muted),
        )
        // 预设 + 直通行。
        .child(
            gpui_kit::component::h_flex()
                .gap_4()
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .child(xcontrol_i18n::tr("mapping.preset"))
                                .text_size(px(12.))
                                .text_color(muted),
                        )
                        .child(div().w(px(220.)).child(preset_select)),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .child(xcontrol_i18n::tr("mapping.passthrough_hint"))
                                .text_size(px(12.))
                                .text_color(muted),
                        )
                        .child(
                            Switch::new("mapping-passthrough")
                                .checked(keymap.passthrough)
                                .on_click({
                                    let passthrough = !keymap.passthrough;
                                    cx.listener(move |view: &mut SettingsView, _ev, _window, cx| {
                                        tracing::info!(target: "app::settings", passthrough,
                                            "直通开关变更（即时生效，docs/06 §4.1）");
                                        view.commit_config(
                                            xcontrol_i18n::tr("label.passthrough"),
                                            |config| {
                                                config.keymap.passthrough = passthrough;
                                                Ok(())
                                            },
                                        );
                                        cx.notify();
                                    })
                                }),
                        ),
                ),
        );

    // —— 功能键映射（主 UI，11-21 实测反馈：5 行左右下拉选择）——
    let mut func_section = v_flex()
        .gap_1()
        .child(
            div()
                .child(xcontrol_i18n::tr("mapping.func_keys_title"))
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM),
        )
        .child(
            div()
                .child(xcontrol_i18n::tr("mapping.func_keys_hint"))
                .text_size(px(12.))
                .text_color(muted),
        )
        .child(
            gpui_kit::component::h_flex()
                .gap_3()
                .child(
                    div()
                        .w(px(140.))
                        .child(xcontrol_i18n::tr("mapping.col_source"))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(
                    div()
                        .w(px(180.))
                        .child(xcontrol_i18n::tr("mapping.col_target"))
                        .text_size(px(12.))
                        .text_color(muted),
                ),
        );
    for (index, (label_key, source)) in FUNC_ROWS.iter().enumerate() {
        let row_select = Select::new(&view.mapping.func_selects[index])
            .accessibility_label(xcontrol_i18n::tr(label_key))
            .disabled(source.is_none());
        func_section = func_section.child(
            gpui_kit::component::h_flex()
                .gap_3()
                .items_center()
                .id(("mapping-func-row", index))
                .child(
                    div()
                        .w(px(140.))
                        .child(xcontrol_i18n::tr(label_key))
                        .text_size(px(13.)),
                )
                .child(div().child("→").text_color(muted))
                .child(div().w(px(180.)).child(row_select)),
        );
        if source.is_none() {
            func_section = func_section.child(
                div()
                    .child(xcontrol_i18n::tr("mapping.tab_unsupported"))
                    .text_size(px(11.))
                    .text_color(muted),
            );
        }
    }
    page = page.child(func_section);

    // —— 高级 · 自定义规则（折叠区，默认收起）——
    let advanced_open = view.mapping.advanced_open;
    let toggle_label = if advanced_open {
        format!("▾ {}", xcontrol_i18n::tr("mapping.advanced_title"))
    } else {
        format!("▸ {}", xcontrol_i18n::tr("mapping.advanced_title"))
    };
    let mut advanced = v_flex().gap_1().child(
        gpui_kit::component::h_flex()
            .gap_2()
            .child(
                Button::new("mapping-advanced-toggle")
                    .label(SharedString::from(toggle_label))
                    .ghost()
                    .compact()
                    .on_click(
                        cx.listener(move |view: &mut SettingsView, _ev, _window, cx| {
                            view.mapping.advanced_open = !view.mapping.advanced_open;
                            tracing::debug!(target: "app::settings",
                            open = view.mapping.advanced_open, "高级规则折叠区切换");
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .child(xcontrol_i18n::tr("mapping.advanced_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            ),
    );
    if advanced_open {
        advanced = advanced
            .child(advanced_rules_table(view, cx))
            .child(advanced_add_form(view, cx));
    }
    page = page.child(advanced);
    page
}

/// 高级区：规则表（展示 + 删除；形态 2 修饰键规则由功能键选择器重建，不在此删）。
fn advanced_rules_table(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let rules = view.config.keymap.rules.clone();
    let mut table = v_flex().gap_1().child(
        div()
            .child(xcontrol_i18n::trf(
                "mapping.rules_count",
                &[("n", &rules.len().to_string())],
            ))
            .text_size(px(12.))
            .text_color(muted),
    );
    for (index, rule) in rules.iter().enumerate() {
        table = table.child(
            gpui_kit::component::h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .id(("mapping-rule", index))
                        .child(SharedString::from(rule_summary(rule)))
                        .text_size(px(13.)),
                )
                .child(
                    Button::new(("mapping-rule-del", index))
                        .label(xcontrol_i18n::tr("common.delete"))
                        .danger()
                        .compact()
                        .on_click(cx.listener(move |view: &mut SettingsView, _ev, _window, cx| {
                            tracing::info!(target: "app::settings", index, "删除映射规则（即时生效）");
                            view.commit_config(xcontrol_i18n::tr("label.rule_delete"), |config| {
                                if index >= config.keymap.rules.len() {
                                    return Err(xcontrol_i18n::tr("mapping.err_stale_index").to_owned());
                                }
                                let _ = config.keymap.rules.remove(index);
                                Ok(())
                            });
                            cx.notify();
                        })),
                ),
        );
    }
    if rules.is_empty() {
        table = table.child(
            div()
                .child(xcontrol_i18n::tr("mapping.no_rules"))
                .text_size(px(12.))
                .text_color(muted),
        );
    }
    table
}

/// 高级区：单条新增表单（trigger → action / drop）。
fn advanced_add_form(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    v_flex()
        .gap_2()
        .p_3()
        .rounded(px(6.))
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .child(xcontrol_i18n::tr("mapping.add_hint"))
                .text_size(px(12.))
                .text_color(muted),
        )
        .child(
            gpui_kit::component::h_flex()
                .gap_2()
                .child(
                    div()
                        .w(px(200.))
                        .child(Input::new(&view.mapping.trigger_input)),
                )
                .child(div().child("→").text_color(muted))
                .child(
                    div()
                        .w(px(200.))
                        .child(Input::new(&view.mapping.action_input)),
                )
                .child(
                    Button::new("mapping-rule-add")
                        .label(xcontrol_i18n::tr("common.add"))
                        .on_click(cx.listener(|view: &mut SettingsView, _ev, window, cx| {
                            add_rule(view, window, cx);
                        })),
                ),
        )
}

/// 新增规则提交（就地校验：语义键名解析 + 非修饰键判定，docs/03 §4.1 形态 1/4）。
fn add_rule(view: &mut SettingsView, window: &mut Window, cx: &mut Context<SettingsView>) {
    let trigger = view
        .mapping
        .trigger_input
        .read(cx)
        .value()
        .trim()
        .to_owned();
    let action = view.mapping.action_input.read(cx).value().trim().to_owned();
    match build_rule(&trigger, &action) {
        Ok(rule) => {
            tracing::info!(target: "app::settings", "新增映射规则（即时生效）");
            view.commit_config(xcontrol_i18n::tr("label.rule_add"), |config| {
                if config.keymap.rules.len() >= xcontrol_config::KEYMAP_MAX_RULES {
                    return Err(xcontrol_i18n::trf(
                        "mapping.err_limit",
                        &[("n", &xcontrol_config::KEYMAP_MAX_RULES.to_string())],
                    ));
                }
                config.keymap.rules.push(rule);
                Ok(())
            });
            // 成功后清空表单（set_value 需要 window）。
            view.mapping.trigger_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
            view.mapping.action_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        Err(error) => {
            view.notify_text(error, true);
        }
    }
    cx.notify();
}

/// 表单输入 → 规则（形态 1 单键重映射 / 形态 4 丢弃）；非法键名就地报错。
fn build_rule(trigger: &str, action: &str) -> Result<KeymapRule, String> {
    if trigger.is_empty() {
        return Err(xcontrol_i18n::tr("mapping.err_trigger_empty").to_owned());
    }
    // 键名解析（语义键名文法，docs/03 §2）；修饰键名不用于单键形态——
    // 修饰键映射由功能键选择器承载（11-21 实测反馈重构）。
    if parse_key_name(trigger).is_none() {
        return Err(xcontrol_i18n::trf(
            "mapping.err_trigger_unknown",
            &[("k", &format!("{trigger:?}"))],
        ));
    }
    if parse_key_name(trigger).is_some_and(xcontrol_config::raw_is_modifier) {
        return Err(xcontrol_i18n::trf(
            "mapping.err_trigger_modifier",
            &[("k", &format!("{trigger:?}"))],
        ));
    }
    let rule_action = if action == "drop" {
        RuleAction {
            drop: Some(true),
            ..RuleAction::default()
        }
    } else {
        if action.is_empty() {
            return Err(xcontrol_i18n::tr("mapping.err_action_empty").to_owned());
        }
        if parse_key_name(action).is_none() {
            return Err(xcontrol_i18n::trf(
                "mapping.err_action_unknown",
                &[("k", &format!("{action:?}"))],
            ));
        }
        if parse_key_name(action).is_some_and(xcontrol_config::raw_is_modifier) {
            return Err(xcontrol_i18n::trf(
                "mapping.err_action_modifier",
                &[("k", &format!("{action:?}"))],
            ));
        }
        RuleAction {
            key: Some(action.to_owned()),
            ..RuleAction::default()
        }
    };
    Ok(KeymapRule::new(
        RuleTrigger {
            key: Some(trigger.to_owned()),
            ..RuleTrigger::default()
        },
        rule_action,
    ))
}

#[cfg(test)]
mod tests {
    use xcontrol_config::KEYMAP_PRESETS;

    use super::*;

    /// 规则摘要：单键 / 组合 / drop / 修饰键四形态可读文本。
    #[test]
    fn rule_summary_covers_forms() {
        let single = build_rule("CapsLock", "Escape").expect("合法单键规则");
        assert_eq!(rule_summary(&single), "CapsLock → Escape");
        let drop = build_rule("CK_MUTE", "drop").expect("合法 drop 规则");
        assert_eq!(rule_summary(&drop), "CK_MUTE → 丢弃");
    }

    /// build_rule 校验矩阵：合法键名 / 未知键名 / 修饰键拒绝 / drop / 空输入。
    #[test]
    fn build_rule_validates_key_names() {
        assert!(build_rule("CapsLock", "Escape").is_ok());
        assert!(build_rule("CK_MUTE", "drop").is_ok());
        assert!(build_rule("", "Escape").is_err(), "空触发键拒绝");
        assert!(build_rule("CapsLock", "").is_err(), "空目标键拒绝");
        assert!(
            build_rule("capslock", "Escape").is_err(),
            "未知键名拒绝（大小写敏感）"
        );
        assert!(build_rule("LeftCtrl", "Escape").is_err(), "修饰键触发拒绝");
        assert!(
            build_rule("CapsLock", "LeftMeta").is_err(),
            "修饰键目标拒绝"
        );
        assert!(build_rule("CapsLock", "NotAKey").is_err(), "未知目标键拒绝");
    }

    /// 功能键组常量：wire 锚点与语义名一一对应（docs/03 §2）。
    #[test]
    fn func_key_constants_are_aligned() {
        assert_eq!(FuncKey::Ctrl.keys()[0].raw(), 0xE0);
        assert_eq!(FuncKey::Ctrl.keys()[1].raw(), 0xE4);
        assert_eq!(FuncKey::Meta.keys()[0].raw(), 0xE3);
        assert_eq!(FuncKey::Meta.keys()[1].raw(), 0xE7);
        assert_eq!(FuncKey::Meta.names(), ["LeftMeta", "RightMeta"]);
        for group in FuncKey::ALL {
            for (key, name) in group.keys().iter().zip(group.names()) {
                assert_eq!(parse_key_name(name), Some(key.raw()), "{name}");
                assert!(xcontrol_config::raw_is_modifier(key.raw()));
            }
        }
    }

    /// 预设反解矩阵：mac2win = Meta→Ctrl（Alt 行恒等 → 不映射）；恒等预设全不映射。
    #[test]
    fn selectors_reverse_solve_presets() {
        let mut config = Config::default();
        config.keymap.preset = "mac2win".to_owned();
        let selection = selectors_from_config(&config);
        // 行序 = Ctrl / Shift / Alt / Meta / Tab。
        assert_eq!(selection, [None, None, None, Some(FuncKey::Ctrl), None]);

        config.keymap.preset = "win2mac".to_owned();
        let selection = selectors_from_config(&config);
        assert_eq!(
            selection,
            [Some(FuncKey::Meta), None, None, Some(FuncKey::Alt), None]
        );

        for preset in ["none", "mac2mac", "win2linux", "linux2win"] {
            config.keymap.preset = preset.to_owned();
            assert_eq!(
                selectors_from_config(&config),
                [None; 5],
                "{preset} 恒等预设全部不映射"
            );
        }
    }

    /// 自定义形态 2 规则反解（preset = none）：LMeta→LCtrl + RMeta→RCtrl
    /// = Meta 行 → Ctrl；左手规则优先（首条生效）。
    #[test]
    fn selectors_reverse_solve_custom_mod_rules() {
        let mut config = Config::default();
        config.keymap.preset = "none".to_owned();
        config.keymap.rules = vec![
            KeymapRule::new(
                RuleTrigger {
                    mod_: Some("LeftMeta".to_owned()),
                    ..RuleTrigger::default()
                },
                RuleAction {
                    mod_: Some("LeftCtrl".to_owned()),
                    ..RuleAction::default()
                },
            ),
            KeymapRule::new(
                RuleTrigger {
                    mod_: Some("RightMeta".to_owned()),
                    ..RuleTrigger::default()
                },
                RuleAction {
                    mod_: Some("RightCtrl".to_owned()),
                    ..RuleAction::default()
                },
            ),
        ];
        assert_eq!(
            selectors_from_config(&config),
            [None, None, None, Some(FuncKey::Ctrl), None]
        );
    }

    /// 选择器 → 规则生成：Meta→Ctrl 产出左右手两条形态 2 规则；
    /// 恒等（目标 = 源组）与不映射不产出规则；Tab 行不产出规则。
    #[test]
    fn func_selection_generates_mod_rules() {
        let rules = func_selection_to_rules(&[None, None, None, Some(FuncKey::Ctrl), None]);
        assert_eq!(rules.len(), 2, "左右手各一条");
        assert_eq!(rules[0].trigger.mod_.as_deref(), Some("LeftMeta"));
        assert_eq!(rules[0].action.mod_.as_deref(), Some("LeftCtrl"));
        assert_eq!(rules[1].trigger.mod_.as_deref(), Some("RightMeta"));
        assert_eq!(rules[1].action.mod_.as_deref(), Some("RightCtrl"));

        assert!(func_selection_to_rules(&[None; 5]).is_empty(), "全不映射");
        // 恒等：目标 = 源组。
        assert!(
            func_selection_to_rules(&[Some(FuncKey::Ctrl), None, None, None, None]).is_empty(),
            "Ctrl→Ctrl 恒等无规则"
        );
    }

    /// 数据流闭环：选择器 → 规则 → 反解回同一选择器（生成与反解互逆）。
    #[test]
    fn selector_round_trip_is_identity() {
        let cases: [[Option<FuncKey>; 5]; 3] = [
            [None, None, None, Some(FuncKey::Ctrl), None],
            [Some(FuncKey::Meta), None, None, None, None],
            [
                None,
                Some(FuncKey::Alt),
                Some(FuncKey::Shift),
                Some(FuncKey::Ctrl),
                None,
            ],
        ];
        for selection in cases {
            let mut config = Config::default();
            config.keymap.preset = "none".to_owned();
            config.keymap.rules = func_selection_to_rules(&selection);
            assert_eq!(selectors_from_config(&config), selection, "{selection:?}");
        }
    }

    /// 生成的规则形态合法（形态 2 独占 + 规范修饰键名——config 校验同表规则，
    /// 端到端落盘校验由 tests_ui 承载）。
    #[test]
    fn generated_rules_are_valid_form2() {
        let rules = func_selection_to_rules(&[None, None, None, Some(FuncKey::Ctrl), None]);
        for rule in &rules {
            assert!(rule.trigger.mod_.is_some(), "trigger.mod 独占");
            assert!(rule.trigger.key.is_none() && rule.trigger.mods.is_none());
            assert!(rule.action.mod_.is_some(), "action.mod 独占");
            assert!(
                rule.action.key.is_none()
                    && rule.action.mods.is_none()
                    && rule.action.drop.is_none()
            );
            for name in [rule.trigger.mod_.as_deref(), rule.action.mod_.as_deref()]
                .into_iter()
                .flatten()
            {
                assert!(
                    xcontrol_config::is_modifier_name(name),
                    "{name} 必须是规范修饰键名"
                );
            }
        }
    }

    /// 目标下标换算：0 位 = 不映射；其余与 FuncKey::ALL 对齐。
    #[test]
    fn target_index_round_trip() {
        assert_eq!(index_from_target(None), 0);
        for (position, group) in FuncKey::ALL.iter().enumerate() {
            assert_eq!(index_from_target(Some(*group)), position + 1);
            assert_eq!(target_from_index(position + 1), Some(*group));
        }
        assert_eq!(target_from_index(0), None);
    }

    /// 预设表常量与 UI 反解的耦合面：全部预设名都可反解（未知名回落空选择）。
    #[test]
    fn all_presets_reverse_solve() {
        for preset in KEYMAP_PRESETS {
            let mut config = Config::default();
            config.keymap.preset = (*preset).to_owned();
            // 不 panic 且行数恒 5。
            assert_eq!(selectors_from_config(&config).len(), 5, "{preset}");
        }
    }
}
