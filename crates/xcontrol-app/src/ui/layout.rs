//! 布局页（FR-1.3 屏幕布局编辑器，docs/06 §3 M5 简化裁定——**方位选择制**）。
//!
//! ```text
//! 本机屏幕（只读，会话建立时自动检测——运行时快照投影） +
//! 外接（从机）屏条目表（展示 + 删除） +
//! 新增条目表单（名称 + 锚点 + 上/下/左/右 方位下拉）
//! ```
//!
//! - 本机屏是布局图锚点（主控显示枚举序），运行时自动检测、页面只读展示
//!   （纯投影红线：数据来自 [`crate::state::AppState::local_screens`] 快照，
//!   UI 不触碰平台层）；像素级拖拽网格画布属 M6+（docs/06 §3 修订记录）。
//! - 条目模型 = config `[layout.screens]`（`{id, relative_to, direction}`，
//!   FR-1.3 方位制）：锚点空 = 主控主屏（11-19-m5-layout 最小模型语义，
//!   docs/01 `[layout]` 注）；锚点可填本机屏名或既有条目名（条目可相互引用，
//!   `merge_layout` 锚点解析契约）。
//! - 生效路径（docs/06 §4.1 `layout` 行）：改动 → `commit_config` → 落盘 +
//!   `RuntimeConfigUpdate` → **主控会话内即时**重装配 + 重新下发 `Layout`
//!   （`session_rt` 的 RuntimeConfigUpdate 消费点调
//!   [`crate::layout_rt::rebuild_and_publish`]；未协商时按会话快照在握手时
//!   装配——页内说明如实标注）。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{ActiveTheme, IndexPath, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, SharedString, Window, div, px};
use xcontrol_config::{Config, Direction, LayoutScreen};

use super::SettingsView;

/// 方位下拉选项（key 序 = [`DIRECTIONS`] 定义序；显示名经文案表现取——FR-6.5，
/// 下拉选项为构造期快照，语言切换后本控件重建对齐）。
const DIRECTION_KEYS: [&str; 4] = [
    "layout.dir_up",
    "layout.dir_down",
    "layout.dir_left",
    "layout.dir_right",
];

/// 方位定义序（下拉选项与 [`parse_direction`] 共用的唯一出处）。
const DIRECTIONS: [Direction; 4] = [
    Direction::Up,
    Direction::Down,
    Direction::Left,
    Direction::Right,
];

/// 布局页控件状态。
pub(crate) struct LayoutPage {
    /// 新增条目：屏幕/从机标识输入（测试经实体 setup 内容）。
    pub(crate) name_input: Entity<InputState>,
    /// 新增条目：锚点输入（空 = 主控主屏；可填本机屏名或既有条目名）。
    pub(crate) anchor_input: Entity<InputState>,
    /// 新增条目：方位下拉（选中态由 SelectState 自持，添加时读取）。
    direction_select: Entity<SelectState<Vec<String>>>,
}

impl LayoutPage {
    pub(super) fn new(
        _config: &Config,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let options: Vec<String> = DIRECTION_KEYS
            .iter()
            .map(|key| xcontrol_i18n::tr(key).to_owned())
            .collect();
        // 默认方位 = 右侧（M4「从机在主控右侧」开箱语义的延续）。
        let default_index = DIRECTION_KEYS
            .iter()
            .position(|key| *key == "layout.dir_right")
            .unwrap_or(0);
        Self {
            name_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("layout.ph_name"))
            }),
            anchor_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(xcontrol_i18n::tr("layout.ph_anchor"))
            }),
            direction_select: cx.new(|cx| {
                SelectState::new(options, Some(IndexPath::new(default_index)), window, cx)
            }),
        }
    }
}

/// 控件订阅：无（方位选择在「添加」时经 `selected_value` 读取）。
pub(super) fn subscribe(_page: &LayoutPage, _cx: &mut Context<SettingsView>) {}

/// 方位显示名（条目表/摘要共用；FR-6.5 文案表）。
#[must_use]
fn direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Up => xcontrol_i18n::tr("layout.dir_up"),
        Direction::Down => xcontrol_i18n::tr("layout.dir_down"),
        Direction::Left => xcontrol_i18n::tr("layout.dir_left"),
        Direction::Right => xcontrol_i18n::tr("layout.dir_right"),
    }
}

/// 显示名 → 方位（[`DIRECTION_KEYS`] 序的唯一解析点）。
#[must_use]
fn parse_direction(label: &str) -> Option<Direction> {
    DIRECTIONS
        .iter()
        .copied()
        .find(|direction| direction_label(*direction) == label)
}

/// 条目摘要（条目表行文本；锚点语义与 `merge_layout` 解析一致；FR-6.5 文案表）。
#[must_use]
fn entry_summary(entry: &LayoutScreen) -> String {
    let anchor = match &entry.relative_to {
        Some(name) => name.as_str(),
        None => xcontrol_i18n::tr("layout.default_anchor"),
    };
    let direction = entry.direction.map_or_else(
        || xcontrol_i18n::tr("layout.standalone_anchor").to_owned(),
        |d| direction_label(d).to_owned(),
    );
    xcontrol_i18n::trf(
        "layout.entry_summary",
        &[("id", &entry.id), ("anchor", anchor), ("dir", &direction)],
    )
}

/// 表单输入 → 条目（就地校验：名称非空；方位由下拉保证存在）。
fn build_entry(
    name: &str,
    anchor: &str,
    direction: Option<Direction>,
) -> Result<LayoutScreen, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(xcontrol_i18n::tr("layout.err_name_empty").to_owned());
    }
    Ok(LayoutScreen {
        id: name.to_owned(),
        relative_to: {
            let anchor = anchor.trim();
            if anchor.is_empty() {
                None // 缺省锚点 = 主控主屏（docs/01 [layout] 注）
            } else {
                Some(anchor.to_owned())
            }
        },
        direction,
    })
}

/// 条目加入（纯函数——单测覆盖；id 重复就地拒绝，save 校验兜底）。
fn add_entry(screens: &mut Vec<LayoutScreen>, entry: LayoutScreen) -> Result<(), String> {
    if screens.iter().any(|existing| existing.id == entry.id) {
        return Err(xcontrol_i18n::trf(
            "layout.err_duplicate",
            &[("id", &format!("{:?}", entry.id))],
        ));
    }
    screens.push(entry);
    Ok(())
}

/// 渲染布局页。
pub(super) fn render(
    view: &mut SettingsView,
    _window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> gpui_kit::Div {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let layout = &view.config.layout;

    let mut page = v_flex().gap_3().child(
        div()
            .child(xcontrol_i18n::tr("layout.intro"))
            .text_size(px(12.))
            .text_color(muted),
    );

    // 本机屏幕（只读列表——运行时快照投影，纯投影红线：不查平台）。
    let mut local = v_flex().gap_1().child(
        div()
            .child(xcontrol_i18n::tr("layout.local_screens"))
            .text_size(px(12.))
            .text_color(muted),
    );
    if view.state.local_screens.is_empty() {
        local = local.child(
            div()
                .child(xcontrol_i18n::tr("layout.no_session"))
                .text_size(px(12.))
                .text_color(muted),
        );
    }
    for (index, name) in view.state.local_screens.iter().enumerate() {
        local = local.child(
            div()
                .id(("layout-local-screen", index))
                .child(SharedString::from(format!("{name}（{index}）")))
                .text_size(px(13.)),
        );
    }
    page = page.child(local);

    // 外接（从机）屏条目表（展示 + 删除）。
    let mut table = v_flex().gap_1().child(
        div()
            .child(xcontrol_i18n::trf(
                "layout.entries",
                &[("n", &layout.screens.len().to_string())],
            ))
            .text_size(px(12.))
            .text_color(muted),
    );
    for (index, entry) in layout.screens.iter().enumerate() {
        table = table.child(
            gpui_kit::component::h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .id(("layout-entry", index))
                        .child(SharedString::from(entry_summary(entry)))
                        .text_size(px(13.)),
                )
                .child(
                    Button::new(("layout-entry-del", index))
                        .label(xcontrol_i18n::tr("common.delete"))
                        .danger()
                        .compact()
                        .on_click(
                            cx.listener(move |view: &mut SettingsView, _ev, _window, cx| {
                                tracing::info!(target: "app::settings", index,
                                "删除布局条目（主控会话内重新下发）");
                                view.commit_config(
                                    xcontrol_i18n::tr("label.layout_delete"),
                                    |config| {
                                        if index >= config.layout.screens.len() {
                                            return Err(xcontrol_i18n::tr(
                                                "layout.err_stale_index",
                                            )
                                            .to_owned());
                                        }
                                        let _ = config.layout.screens.remove(index);
                                        Ok(())
                                    },
                                );
                                cx.notify();
                            }),
                        ),
                ),
        );
    }
    if layout.screens.is_empty() {
        table = table.child(
            div()
                .child(xcontrol_i18n::tr("layout.no_entries"))
                .text_size(px(12.))
                .text_color(muted),
        );
    }
    page = page.child(table);

    // 新增条目表单（名称 + 锚点 + 方位下拉）。
    page = page.child(
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .child(xcontrol_i18n::tr("layout.add_hint"))
                    .text_size(px(12.))
                    .text_color(muted),
            )
            .child(
                gpui_kit::component::h_flex()
                    .gap_2()
                    .child(div().w(px(160.)).child(Input::new(&view.layout.name_input)))
                    .child(
                        div()
                            .w(px(200.))
                            .child(Input::new(&view.layout.anchor_input)),
                    )
                    .child(
                        div().w(px(140.)).child(
                            Select::new(&view.layout.direction_select)
                                .accessibility_label(xcontrol_i18n::tr("layout.direction"))
                                .placeholder(xcontrol_i18n::tr("layout.direction")),
                        ),
                    )
                    .child(
                        Button::new("layout-entry-add")
                            .label(xcontrol_i18n::tr("common.add"))
                            .on_click(cx.listener(|view: &mut SettingsView, _ev, window, cx| {
                                add_entry_from_form(view, window, cx);
                            })),
                    ),
            ),
    );
    page
}

/// 新增条目提交（就地校验：名称/重复 id；方位下拉恒有选中项）。
fn add_entry_from_form(
    view: &mut SettingsView,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) {
    let name = view.layout.name_input.read(cx).value().trim().to_owned();
    let anchor = view.layout.anchor_input.read(cx).value().trim().to_owned();
    let direction = view
        .layout
        .direction_select
        .read(cx)
        .selected_value()
        .and_then(|label| parse_direction(label))
        .or_else(|| {
            tracing::warn!(target: "app::settings", "方位下拉无选中项（防御分支：默认右侧）");
            Some(Direction::Right)
        });
    match build_entry(&name, &anchor, direction) {
        Ok(entry) => {
            let mut rejected = None;
            view.commit_config(
                xcontrol_i18n::tr("label.layout_add"),
                |config| match add_entry(&mut config.layout.screens, entry.clone()) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        rejected = Some(error.clone());
                        Err(error)
                    }
                },
            );
            if let Some(error) = rejected {
                // commit_config 已就地提示 save 校验失败；重复 id 的表单级原因
                // 这里补一条更具体的提示（同一提示条，覆盖文案）。
                view.notify_text(
                    xcontrol_i18n::trf("layout.err_not_saved", &[("error", &error)]),
                    true,
                );
            } else {
                tracing::info!(target: "app::settings", name = %entry.id,
                    "新增布局条目（主控会话内重新下发，docs/06 §4.1 layout 行）");
                // 成功后清空表单（set_value 需要 window）。
                view.layout.name_input.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                });
                view.layout.anchor_input.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                });
            }
        }
        Err(error) => {
            view.notify_text(
                xcontrol_i18n::trf("layout.err_not_saved", &[("error", &error)]),
                true,
            );
        }
    }
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 方位显示名与解析对拍（下拉选项的唯一出处契约）。
    #[test]
    fn direction_labels_round_trip() {
        for direction in DIRECTIONS {
            assert_eq!(parse_direction(direction_label(direction)), Some(direction));
        }
        assert_eq!(parse_direction("右侧"), Some(Direction::Right));
        assert_eq!(parse_direction("斜上方"), None);
        // 下拉选项 = 定义序（显示稳定性——AX/测试锚点；zh 默认语言下的字面量）。
        let labels: Vec<&str> = DIRECTION_KEYS
            .iter()
            .map(|key| xcontrol_i18n::tr(key))
            .collect();
        assert_eq!(labels, ["上方", "下方", "左侧", "右侧"]);
    }

    /// 条目摘要：缺省锚点（主控主屏）/显式锚点/独立锚点三种形态。
    #[test]
    fn entry_summary_covers_anchor_forms() {
        let default_anchor = build_entry("sofa", "", Some(Direction::Right)).expect("合法条目");
        assert_eq!(entry_summary(&default_anchor), "sofa：主控主屏 右侧");
        let explicit = build_entry("desk", "sofa", Some(Direction::Down)).expect("合法条目");
        assert_eq!(entry_summary(&explicit), "desk：sofa 下方");
        let mut standalone = explicit;
        standalone.direction = None;
        assert_eq!(
            entry_summary(&standalone),
            "desk：sofa （独立锚点，路由图跳过）"
        );
    }

    /// build_entry 校验矩阵：空名拒绝 / 名称与锚点 trim / 方位随下拉。
    #[test]
    fn build_entry_validates_name_and_trims() {
        assert!(build_entry("", "", Some(Direction::Right)).is_err());
        assert!(build_entry("   ", "", Some(Direction::Right)).is_err());
        let entry = build_entry(" sofa ", " desk ", Some(Direction::Left)).expect("合法");
        assert_eq!(entry.id, "sofa");
        assert_eq!(entry.relative_to.as_deref(), Some("desk"));
        // 无方位（下拉防御分支不会出现；config 校验允许的独立锚点形态）。
        let standalone = build_entry("sofa", "", None).expect("合法");
        assert_eq!(standalone.direction, None);
    }

    /// add_entry：唯一性 + 追加顺序（条目间唯一，FR-1.3 校验同款判定）。
    #[test]
    fn add_entry_rejects_duplicates_in_order() {
        let mut screens = Vec::new();
        add_entry(
            &mut screens,
            build_entry("sofa", "", Some(Direction::Right)).expect("合法"),
        )
        .expect("首条加入");
        add_entry(
            &mut screens,
            build_entry("desk", "sofa", Some(Direction::Down)).expect("合法"),
        )
        .expect("次条加入");
        assert_eq!(screens.len(), 2);
        let error = add_entry(
            &mut screens,
            build_entry("sofa", "", Some(Direction::Left)).expect("合法"),
        )
        .expect_err("重复 id 拒绝");
        assert!(error.contains("已存在"), "{error}");
        assert_eq!(screens.len(), 2, "拒绝不产生部分写入");
    }
}
