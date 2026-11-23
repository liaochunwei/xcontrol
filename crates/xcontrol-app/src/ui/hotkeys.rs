//! 热键页（FR-2.5，docs/03 §4.6 全局热键的 M4 简化编辑面）。
//!
//! 四键（切下一台/切上一台/回主控/暂停恢复）逐行展示 + 组合键文本输入 + 应用
//! 按钮；校验复用 [`xcontrol_core::hotkey::HotkeySet::from_config`]（热键文法
//! 与运行时编译同一张表——非法组合就地拒绝并提示，docs/06 §4）。
//!
//! 录制控件（按键捕获）属 M5；Wayland 回退手势开关同批。
//!
//! 生效路径（docs/06 §4.1，11-21 反馈 3）：改动落盘下发 → **即时生效**（运行中
//! 会话热键表热替换；win/x11 平台注册源随改动重建；无活动会话时下会话构造
//! 点生效）。

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, SharedString, Window, div, px};
use xcontrol_config::{Config, HotkeysConfig};
use xcontrol_core::hotkey::HotkeySet;

use super::SettingsView;

/// 热键条目（页面渲染顺序 = docs/03 §4.6 四动作）；显示名经文案表
/// （FR-6.5）按 key 现取——渲染期读取，随语言切换。
const HOTKEY_FIELDS: [(&str, &str, HotkeyField); 4] = [
    ("hotkeys.next", "next", HotkeyField::Next),
    ("hotkeys.prev", "prev", HotkeyField::Prev),
    (
        "hotkeys.back_to_host",
        "back_to_host",
        HotkeyField::BackToHost,
    ),
    ("hotkeys.pause", "pause", HotkeyField::Pause),
];

/// 四键字段标识（读取当前值 / 写回新值共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyField {
    Next,
    Prev,
    BackToHost,
    Pause,
}

impl HotkeyField {
    /// 当前值（config 读取）。
    fn get(self, hotkeys: &HotkeysConfig) -> &str {
        match self {
            Self::Next => &hotkeys.next,
            Self::Prev => &hotkeys.prev,
            Self::BackToHost => &hotkeys.back_to_host,
            Self::Pause => &hotkeys.pause,
        }
    }

    /// 写回新值（commit mutate 用）。
    fn set(self, hotkeys: &mut HotkeysConfig, value: String) {
        match self {
            Self::Next => hotkeys.next = value,
            Self::Prev => hotkeys.prev = value,
            Self::BackToHost => hotkeys.back_to_host = value,
            Self::Pause => hotkeys.pause = value,
        }
    }
}

/// 热键页控件状态（每键一个输入框，预填当前值）。
pub(crate) struct HotkeysPage {
    inputs: [Entity<InputState>; 4],
}

impl HotkeysPage {
    pub(super) fn new(
        config: &Config,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let inputs = HOTKEY_FIELDS.map(|(_, _, field)| {
            let current = field.get(&config.hotkeys).to_owned();
            cx.new(|cx| InputState::new(window, cx).default_value(current))
        });
        Self { inputs }
    }
}

/// 控件订阅：输入变化即时清提示（校验与应用按钮承担主路径；订阅面留作回显，
/// 不做逐键提交——组合键中间态非法，逐键提交会噪声落盘）。
pub(super) fn subscribe(_page: &HotkeysPage, _cx: &mut Context<SettingsView>) {}

/// 渲染热键页。
pub(super) fn render(
    view: &mut SettingsView,
    _window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let mut page = v_flex().gap_3().child(
        div()
            .child(xcontrol_i18n::tr("hotkeys.intro"))
            .text_size(px(12.))
            .text_color(muted),
    );

    for (index, ((label, _id, field), input)) in
        HOTKEY_FIELDS.iter().zip(&view.hotkeys.inputs).enumerate()
    {
        let field = *field;
        let current = field.get(&view.config.hotkeys).to_owned();
        page = page.child(
            gpui_kit::component::h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .w(px(120.))
                        .child(SharedString::from(xcontrol_i18n::tr(label)))
                        .text_size(px(13.)),
                )
                .child(
                    div()
                        .w(px(140.))
                        .id(("hotkey-current", index))
                        .child(SharedString::from(current))
                        .text_size(px(12.))
                        .text_color(muted),
                )
                .child(div().w(px(200.)).child(Input::new(input)))
                .child(
                    Button::new(("hotkey-save", index))
                        .label(xcontrol_i18n::tr("common.save"))
                        .compact()
                        .on_click(
                            cx.listener(move |view: &mut SettingsView, _ev, window, cx| {
                                apply_hotkey(field, view, window, cx);
                            }),
                        ),
                ),
        );
    }
    page
}

/// 应用单键修改（校验复用 HotkeySet::from_config——与运行时同一文法表）。
fn apply_hotkey(
    field: HotkeyField,
    view: &mut SettingsView,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) {
    let raw = field.get_input_value(view, cx).trim().to_owned();
    let mut candidate = view.config.hotkeys.clone();
    field.set(&mut candidate, raw.clone());
    // 全四键整体编译校验（单键合法但与其它键冲突的组合同样拒绝——运行时语义）。
    if let Err(error) = HotkeySet::from_config(&candidate) {
        tracing::warn!(target: "app::settings", %error, "热键组合非法（就地拒绝）");
        view.notify_text(
            xcontrol_i18n::trf(
                "notice.not_saved",
                &[
                    ("label", xcontrol_i18n::tr("label.hotkeys")),
                    ("error", &error.to_string()),
                ],
            ),
            true,
        );
        cx.notify();
        return;
    }
    tracing::info!(target: "app::settings", value = %raw, "热键修改（即时生效，docs/06 §4.1）");
    view.commit_config(xcontrol_i18n::tr("label.hotkeys"), |config| {
        field.set(&mut config.hotkeys, raw.clone());
        Ok(())
    });
    // 成功后把输入框对齐已保存值（非法值就地保留供修正）。
    if let Some(input) = view.hotkey_input(field) {
        input.update(cx, |state, cx| state.set_value(raw, window, cx));
    }
}

impl HotkeyField {
    /// 从页面输入框读当前候选值。
    fn get_input_value(&self, view: &SettingsView, cx: &App) -> String {
        let slot = self.slot_index();
        view.hotkeys.inputs[slot].read(cx).value().to_string()
    }

    /// 在 HOTKEY_FIELDS 中的槽位（inputs 数组下标）。
    fn slot_index(self) -> usize {
        // 不变量：四字段恰为 HOTKEY_FIELDS 全集（测试钉死）；防御性回落 0。
        HOTKEY_FIELDS
            .iter()
            .position(|(_, _, field)| *field == self)
            .unwrap_or(0)
    }
}

impl SettingsView {
    /// 热键页输入框实体（按字段）。
    fn hotkey_input(&self, field: HotkeyField) -> Option<Entity<InputState>> {
        Some(self.hotkeys.inputs[field.slot_index()].clone())
    }
}
