//! 设置窗口 UI 集成测试（11-18-m4-ui，gpui_kit::test 框架级交互；lib 内测试
//! 模块——可访问 pub(crate) 面，行为面与外部用户一致）。
//!
//! 外部 AX 缺口背景（任务报告）：gpui-pre 0.3.12 快照的窗口内容对**外部** AX
//! 客户端不可达（AXWindows 属性返回 app 元素——实测证据见任务报告），托盘菜单
//! 经真实 AX query + AXPress 实测通过。本模块用 gpui_kit::test 在进程内以
//! **真实渲染 + 真实事件派发**（move/down/up 走命中测试）驱动窗口内容，并以
//! `find_by_label`（与 AccessKit 同源的可达名）断言 AX 标签面——向导四步、
//! 映射页增删规则、布局页增删条目（11-19-m5-layout）、通用页日志级别
//! （生效框架全链路）、剪贴板页校验负例。
//!
//! 配置隔离：`config_path` 与 `data_dir` 注入临时目录（SettingsView DI 参数）；
//! 命令通道接收端留在测试内，断言 `RuntimeConfigUpdate` 下发（docs/06 §4.1）。

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::IndexPath;
use gpui_kit::test::{TestQueryExt, TestWindowExt};
use gpui_kit::{
    AppContext, Bounds, Entity, Point, Size, TestAppContext, WindowBounds, WindowOptions, px,
};
use tokio::sync::mpsc;
use xcontrol_config::{Config, Role};

use crate::state::{AppState, CoreCommand, CoreHandle, StateBus};
use crate::ui::SettingsView;

/// 侧栏导航到指定页（角色过滤语义——页必须在当前快照角色的可见集中，
/// docs/06 §4 信息架构角色归属）。
///
/// SidebarMenuItem 的外部容器带 `.test_support()`（观察面 id = 组件内部按序生成的
/// `Name("0-{index}")`），其内部 `aria_label` 层未开观察——故按**可见页序号**
/// 计算稳定 id 点击（gpui-component 0.9.1 内部 id 方案，任务报告记录该耦合）；
/// 页面切换正确性经 `view.page` 状态断言（纯投影路由的可观测面）。
fn nav_to(
    window: &mut gpui_kit::Window,
    page: &str,
    view: &Entity<SettingsView>,
    cx: &mut gpui_kit::App,
) {
    let role = view.read(cx).state.role;
    let index = crate::ui::Page::visible_pages(role)
        .iter()
        .position(|p| p.title() == page)
        .unwrap_or_else(|| panic!("页 {page} 在角色 {role:?} 下不可见"));
    window.render_frame(cx);
    window.click(format!("0-{index}"), cx);
    window.render_frame(cx);
    assert_eq!(
        view.read(cx).page,
        crate::ui::page_by_title(page),
        "路由到 {page}"
    );
}

/// 指定角色快照的状态总线（角色过滤测试用——SettingsView 构造期取 initial
/// 快照，构造后 200ms 刷新循环在测试面不可靠）。
fn bus_with_role(role: Role) -> Arc<StateBus> {
    StateBus::new(AppState {
        role,
        ..AppState::default()
    })
}

/// 按可达名点击（Select 触发器/弹层菜单项等无显式 id 的控件）。
fn click_by_label(window: &mut gpui_kit::Window, label: &str, cx: &mut gpui_kit::App) {
    window.render_frame(cx);
    let snap = window
        .try_find_by_label(label)
        .unwrap_or_else(|| panic!("缺可达元素 {label:?}"));
    window.click(snap.path().last().unwrap().clone(), cx);
    window.render_frame(cx);
}

/// 与 ui/mod.rs open_main_with_bus 同款开窗（生产入口 gpui_kit::open_window，
/// 包裹 base Root——组件 popover/弹层挂载点一致）。
fn open_settings(
    cx: &mut TestAppContext,
    config: &Config,
    config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    wizard: bool,
    core: CoreHandle,
    bus: Arc<StateBus>,
) -> (gpui_kit::AnyWindowHandle, Entity<SettingsView>) {
    let config = config.clone();
    cx.update(|cx| {
        // 组件库初始化（主题/输入/列表全局状态）——生产 run_window 同序。
        gpui_kit::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: Size {
                width: px(760.),
                height: px(620.),
            },
        };
        let (handle, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    SettingsView::new(
                        bus,
                        crate::logging::LogBuffer::new(),
                        core,
                        config,
                        config_path,
                        data_dir,
                        wizard,
                        wizard,
                        window,
                        cx,
                    )
                })
            },
        )
        .expect("open settings window");
        (handle, view)
    })
}

/// 临时目录守卫（Drop 清理；config/secrets 均落其中）。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "xcontrol-ui-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// FR-6.3 验收：无配置 → 向导自动弹出 → 四步走完 → config.toml 生成且字段
/// 正确 → RuntimeConfigUpdate 携带所选角色下发（等待相语义：不叠加 SwitchRole）。
#[gpui_kit::test]
fn wizard_four_steps_write_config_and_notify_runtime(cx: &mut TestAppContext) {
    let dir = TempDir::new("wizard-four-steps");
    let config_path = dir.path().join("xdg").join("config.toml");
    let data_dir = dir.path().join("data");
    let bus = Arc::new(StateBus::new(Default::default()));
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &Config::default(),
        Some(config_path.clone()),
        Some(data_dir.clone()),
        true,
        core,
        Arc::clone(&bus),
    );

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 步骤 1：角色选择——默认从机（保守默认），选「主控（Host）」验证选择面。
        window.click("wizard-role-host", cx);
        window.render_frame(cx);
        window.click("wizard-next", cx);
        window.render_frame(cx);
        // 步骤 2：安全码——空输入先被就地拒绝（docs/06 §4）。
        window.click("wizard-next", cx);
        window.render_frame(cx);
        let wizard = view.read(cx).wizard.as_ref().expect("向导存活");
        assert_eq!(wizard.step_index_for_test(), 1, "校验失败不推进步骤");
        assert!(
            wizard
                .error_text_for_test()
                .is_some_and(|text| text == "安全码不能为空（两端需一致）"),
            "空安全码必须就地拒绝"
        );
        // 生成建议填入（真实按钮路径；非 CSPRNG 建议码，向导页可编辑）。
        window.click("wizard-gen-code", cx);
        window.render_frame(cx);
        window.click("wizard-next", cx);
        window.render_frame(cx);
        // 步骤 3：网段默认（端口 4243、CIDR 自动）。
        window.click("wizard-next", cx);
        window.render_frame(cx);
        // 步骤 4：权限引导（测试编译面无 macos feature → M5 提示分支）→ 完成。
        window.click("wizard-done", cx);
        // notify 后需要一帧完成重绘（侧栏替换向导布局）。
        window.render_frame(cx);
        window.render_frame(cx);
        // 向导完成：视图回到设置（向导收起——状态页侧栏可达）；运行时已唤醒，
        // 状态页"待命中"提示退场（11-18-m4-ui check 修复的诚实呈现）。
        assert_eq!(view.read(cx).page, crate::ui::Page::Status, "回到状态页");
        assert!(view.read(cx).wizard.is_none(), "向导状态应已清空");
        assert!(
            !view.read(cx).runtime_pending,
            "完成后运行时不再处于等待相（状态页待命提示应退场）"
        );
    })
    .expect("window alive");

    // config.toml 落盘 diff 验证（AC：字段正确；load 流水线含校验）。
    let saved: Config =
        xcontrol_config::load(&config_path).expect("wizard config.toml loads against schema");
    assert_eq!(
        saved.role,
        xcontrol_config::Role::Host,
        "角色 = 向导所选（主控）"
    );
    assert_eq!(saved.net.port, xcontrol_config::DEFAULT_PORT);
    assert!(saved.net.cidrs.is_empty(), "网段默认自动");
    assert_eq!(
        saved.security_code_ref,
        format!("file:{}", xcontrol_config::DEFAULT_SECRET_FILE_RELATIVE),
        "安全码引用 = file 回退后端（测试编译面探测恒 false；真实现 keychain 优先，\
         本机实测见 xcontrol-config keychain_live_round_trip）"
    );
    // 安全码已写入 file 后端（0600，docs/07 §3.3）。
    let secret_path = data_dir.join(xcontrol_config::DEFAULT_SECRET_FILE_RELATIVE);
    let stored = std::fs::read_to_string(&secret_path).expect("security code stored");
    assert!(!stored.trim().is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secret_path)
            .expect("secret metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "安全码文件必须 0600");
    }

    // RuntimeConfigUpdate 下发验证（等待相语义：仅此一令，等待相按 config.role 定角色）。
    let command = rx.try_recv().expect("RuntimeConfigUpdate dispatched");
    let CoreCommand::RuntimeConfigUpdate(config) = command else {
        panic!("应为 RuntimeConfigUpdate，收到 {command:?}")
    };
    assert_eq!(config.role, xcontrol_config::Role::Host);
    assert!(rx.try_recv().is_err(), "等待相完成不应叠加 SwitchRole");
}

/// 向导跳过按钮（CLI 用户）：向导收起、不下发任何运行时命令。
#[gpui_kit::test]
fn wizard_skip_dismisses_without_runtime_commands(cx: &mut TestAppContext) {
    let dir = TempDir::new("wizard-skip");
    let config_path = dir.path().join("xdg").join("config.toml");
    let bus = Arc::new(StateBus::new(Default::default()));
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &Config::default(),
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        true,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("wizard-skip", cx);
        // notify 后需要一帧完成重绘（侧栏替换向导布局）。
        window.render_frame(cx);
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(view.read(cx).wizard.is_none(), "跳过应收取向导");
        assert_eq!(view.read(cx).page, crate::ui::Page::Status, "回到状态页");
        assert!(
            view.read(cx).runtime_pending,
            "跳过后运行时保持等待相（状态页待命提示可见）"
        );
    })
    .expect("window alive");
    assert!(!config_path.exists(), "跳过不落盘");
    assert!(rx.try_recv().is_err(), "跳过不下发运行时命令");
}

/// 映射页（11-21 重构后）：功能键选择器改动 → preset 切 none + 修饰键规则
/// 重建落盘（预设 mac2win 反解初始态 → 用户改动 Ctrl 行 → TOML diff 验证）。
#[gpui_kit::test]
fn mapping_func_selector_writes_mod_rules(cx: &mut TestAppContext) {
    let dir = TempDir::new("mapping-func-selector");
    let config_path = dir.path().join("xdg").join("config.toml");
    let mut config = Config::default();
    config.keymap.preset = "mac2win".to_owned();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Client); // 映射页仅工作机可见
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "按键映射", &view, cx);
        // 预设反解初始态：mac2win → Meta 行应选中 Ctrl（preset_rules 反解，
        // render 期经 set_selected_index 填充）。
        window.render_frame(cx);
        let meta_select = view.read(cx).mapping.func_selects[3].clone();
        let selected = meta_select.read(cx).selected_value().cloned();
        assert_eq!(
            selected.as_deref(),
            Some("Ctrl"),
            "mac2win 反解：Command/Win → Ctrl"
        );

        // 用户改动 Ctrl 行 → Win（命令示例的反向：Ctrl → Win/Command）。
        let ctrl_select = view.read(cx).mapping.func_selects[0].clone();
        ctrl_select.update(cx, |state, cx| {
            state.set_selected_index(Some(IndexPath::new(4)), window, cx); // Win / Command
        });
        // 用户确认事件（真实菜单选中路径的 Confirm；组件弹层交互由 gpui-kit 覆盖）。
        ctrl_select.update(cx, |_state, cx| {
            cx.emit(gpui_kit::component::select::SelectEvent::Confirm(Some(
                "Win / Command".to_owned(),
            )));
        });
        window.render_frame(cx);
    })
    .expect("window alive");

    // TOML diff：预设切 none + 左右手成对的形态 2 规则（保留 Meta→Ctrl 与
    // 新增 Ctrl→Win——选择器全集重建）。
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert_eq!(saved.keymap.preset, "none", "选择器改动后预设自动切自定义");
    assert_eq!(saved.keymap.rules.len(), 4, "左右手成对 × 2 行");
    assert_eq!(
        saved.keymap.rules[0].trigger.mod_.as_deref(),
        Some("LeftCtrl")
    );
    assert_eq!(
        saved.keymap.rules[0].action.mod_.as_deref(),
        Some("LeftMeta")
    );
    assert_eq!(
        saved.keymap.rules[1].trigger.mod_.as_deref(),
        Some("RightCtrl")
    );
    assert_eq!(
        saved.keymap.rules[1].action.mod_.as_deref(),
        Some("RightMeta")
    );
    assert_eq!(
        saved.keymap.rules[2].trigger.mod_.as_deref(),
        Some("LeftMeta")
    );
    assert_eq!(
        saved.keymap.rules[2].action.mod_.as_deref(),
        Some("LeftCtrl")
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(CoreCommand::RuntimeConfigUpdate(_))
    ));
}

/// 映射页高级区：新增规则（就地校验 + 落盘）→ 删除规则（TOML diff 验证）。
#[gpui_kit::test]
fn mapping_page_add_and_delete_rule_updates_toml(cx: &mut TestAppContext) {
    let dir = TempDir::new("mapping-add-del");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Client);
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "按键映射", &view, cx);
        // 高级（自定义规则）折叠区默认收起——先展开（11-21 重构裁定）。
        window.click("mapping-advanced-toggle", cx);
        window.render_frame(cx);
        // 新增规则：CapsLock → Escape（形态 1；触发键/目标键经实体 setup，
        // 交互面 = 「添加」按钮点击 + 落盘断言）。
        view.update(cx, |view, cx| {
            view.mapping.trigger_input.update(cx, |state, cx| {
                state.set_value("CapsLock", window, cx);
            });
            view.mapping.action_input.update(cx, |state, cx| {
                state.set_value("Escape", window, cx);
            });
        });
        window.click("mapping-rule-add", cx);
        window.render_frame(cx);
    })
    .expect("window alive");

    // TOML diff：规则落盘 + RuntimeConfigUpdate 下发（即时生效链路上游——
    // 会话内热替换由 apply_hot_key_configs 承接，docs/06 §4.1）。
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert_eq!(saved.keymap.rules.len(), 1, "规则已落盘");
    assert_eq!(
        saved.keymap.rules[0].trigger.key.as_deref(),
        Some("CapsLock")
    );
    assert_eq!(saved.keymap.rules[0].action.key.as_deref(), Some("Escape"));
    assert!(matches!(
        rx.try_recv(),
        Ok(CoreCommand::RuntimeConfigUpdate(_))
    ));

    // 删除规则（折叠区在首次展开后保持展开——advanced_open 是页面级状态）。
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "按键映射", &view, cx);
        window.click(("mapping-rule-del", 0_usize), cx);
        window.render_frame(cx);
    })
    .expect("window alive");
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert!(saved.keymap.rules.is_empty(), "规则已删除");
}

/// 映射页高级区就地校验负例：未知键名被拒绝（TOML 不变，docs/06 §4）。
#[gpui_kit::test]
fn mapping_page_rejects_unknown_key_name(cx: &mut TestAppContext) {
    let dir = TempDir::new("mapping-negative");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Client);
    let (tx, _rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "按键映射", &view, cx);
        window.click("mapping-advanced-toggle", cx);
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            view.mapping.trigger_input.update(cx, |state, cx| {
                state.set_value("NotAKeyName", window, cx);
            });
            view.mapping.action_input.update(cx, |state, cx| {
                state.set_value("Escape", window, cx);
            });
        });
        window.click("mapping-rule-add", cx);
        window.render_frame(cx);
        // 就地错误提示（docs/06 §4：校验失败就地提示，禁止静默吞错；纯文本提示
        // 不注册可达名——断言视图提示态，提示条 div 同帧渲染）。
        let view = view.read(cx);
        let notice = view.notice.as_ref().expect("未知键名必须就地提示");
        assert!(notice.is_error);
        assert!(
            notice
                .text
                .contains("触发键 \"NotAKeyName\" 不是已知语义键名"),
            "提示文本含字段级原因：{}",
            notice.text
        );
    })
    .expect("window alive");
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert!(saved.keymap.rules.is_empty(), "非法输入不得落盘");
}

/// 通用页：日志级别下拉 → debug（生效框架全链路：落盘 + RuntimeConfigUpdate；
/// EnvFilter 立即生效由命令消费点完成——loopback 集成测试覆盖）。
#[gpui_kit::test]
fn general_page_log_level_change_commits_and_notifies(cx: &mut TestAppContext) {
    let dir = TempDir::new("log-level");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = Arc::new(StateBus::new(Default::default()));
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "通用设置", &view, cx);
        // 日志级别变更（docs/06 §4.1 生效链全链路）。
        //
        // 组件内部弹层（Select 菜单）未开 test_support 观察面——菜单项点击不可
        // 在本 harness 驱动（与侧栏标签同款组件级缺口，任务报告记录）。此处按
        // Confirm 事件语义直接驱动 SelectState 的选择确认（与用户在菜单中选中
        // 同一事件路径——subscribe → commit_config → save → RuntimeConfigUpdate）；
        // 弹层交互本身由 gpui-kit 组件测试覆盖。
        click_by_label(window, "日志级别", cx);
        let select_state = view.read(cx).general.log_level_select.clone();
        select_state.update(cx, |_state, cx| {
            cx.emit(gpui_kit::component::select::SelectEvent::Confirm(Some(
                "debug".to_owned(),
            )));
        });
    })
    .expect("window alive");

    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert_eq!(
        saved.general.log_level,
        xcontrol_config::LogLevel::Debug,
        "日志级别已落盘（EnvFilter 热改由命令消费点执行，docs/06 §4.1）"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(CoreCommand::RuntimeConfigUpdate(_))
    ));
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).config.general.log_level,
            xcontrol_config::LogLevel::Debug,
            "UI 工作副本同步"
        );
    });
}

/// 剪贴板页上限改动校验负例（AC）：超限值就地拒绝、TOML 不变。
#[gpui_kit::test]
fn clipboard_page_rejects_over_limit_bytes(cx: &mut TestAppContext) {
    let dir = TempDir::new("clipboard-negative");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = Arc::new(StateBus::new(Default::default()));
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "剪贴板同步", &view, cx);
        view.update(cx, |view, cx| {
            view.clipboard.max_bytes_input.update(cx, |state, cx| {
                state.set_value(
                    (xcontrol_config::MAX_CLIPBOARD_BYTES_LIMIT + 1).to_string(),
                    window,
                    cx,
                );
            });
        });
        window.click("clipboard-max-save", cx);
        window.render_frame(cx);
        // 超限值就地拒绝（提示态断言；TOML 不变断言在下方）。
        let view = view.read(cx);
        let notice = view.notice.as_ref().expect("超限值必须就地提示");
        assert!(notice.is_error);
        assert!(
            notice.text.contains("超过校验上限"),
            "提示文本含上限说明：{}",
            notice.text
        );
    })
    .expect("window alive");
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert_eq!(
        saved.clipboard.max_bytes,
        xcontrol_config::DEFAULT_CLIPBOARD_MAX_BYTES,
        "超限输入不得落盘"
    );
    assert!(rx.try_recv().is_err(), "校验失败不下发命令");
}

/// 关于页「清空日志文件」（11-21 实测反馈）：按钮 → 确认模态 → 执行——
/// 历史滚动文件删除、当日保留、就地提示（logging::clear_log_files 契约的 UI 面）。
#[gpui_kit::test]
fn about_page_clear_logs_confirm_and_execute(cx: &mut TestAppContext) {
    let dir = TempDir::new("about-clear-logs");
    let config_path = dir.path().join("xdg").join("config.toml");
    let data_dir = dir.path().join("data");
    let logs_dir = xcontrol_config::logs_dir(&data_dir);
    std::fs::create_dir_all(&logs_dir).expect("logs dir");
    std::fs::write(logs_dir.join("xcontrol.log.2020-01-01"), "old").expect("old log file");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = Arc::new(StateBus::new(AppState::default()));
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(data_dir.clone()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "关于", &view, cx);
        window.click("about-clear-logs", cx);
        window.render_frame(cx);
        // 确认模态出现（docs/06 §4 破坏性操作）。
        assert!(
            matches!(
                view.read(cx).confirm,
                Some(crate::ui::ConfirmRequest::ClearLogs)
            ),
            "清空日志必须先确认"
        );
        window.click("confirm-ok", cx);
        window.render_frame(cx);
        assert!(view.read(cx).confirm.is_none(), "确认后模态收起");
        let notice = view.read(cx).notice.clone().expect("清空结果就地提示");
        assert!(!notice.is_error);
        assert!(notice.text.contains("日志已清空"), "{}", notice.text);
    })
    .expect("window alive");
    // 文件面：历史滚动文件已删除（当日文件保留的契约由 logging 单测承载）。
    assert!(!logs_dir.join("xcontrol.log.2020-01-01").exists());
    // 清空日志不走 config 提交框架（无 RuntimeConfigUpdate 下发）。
    assert!(rx.try_recv().is_err(), "清空日志不产生配置命令");
}

/// 设置页全部可达——主控角色（侧栏路由 + 各页切换无 panic；页面集合按角色过滤）。
#[gpui_kit::test]
fn all_settings_pages_are_reachable_for_host(cx: &mut TestAppContext) {
    let dir = TempDir::new("pages-reachable-host");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Host);
    let (tx, _rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        for page in [
            "运行状态",
            "屏幕布局",
            "全局热键",
            "剪贴板同步",
            "通用设置",
            "运行日志",
            "关于",
        ] {
            nav_to(window, page, &view, cx);
        }
    })
    .expect("window alive");
}

/// 设置页全部可达——工作机角色（无屏幕布局/全局热键，有按键映射）。
#[gpui_kit::test]
fn all_settings_pages_are_reachable_for_client(cx: &mut TestAppContext) {
    let dir = TempDir::new("pages-reachable-client");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Client);
    let (tx, _rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        for page in [
            "运行状态",
            "按键映射",
            "剪贴板同步",
            "通用设置",
            "运行日志",
            "关于",
        ] {
            nav_to(window, page, &view, cx);
        }
    })
    .expect("window alive");
}

/// 布局页（FR-1.3，M5 方位选择制）：新增从机屏条目（名称/锚点/方位）→ 落盘 +
/// RuntimeConfigUpdate（主控会话内重新下发的生效链上游）→ 删除条目。
#[gpui_kit::test]
fn layout_page_add_and_delete_entry_updates_toml(cx: &mut TestAppContext) {
    let dir = TempDir::new("layout-add-del");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Host); // 布局页仅主控可见
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "屏幕布局", &view, cx);
        // 新增条目：sofa 锚主控主屏右侧（下拉默认项——不触碰 Select 弹层，
        // 与 mapping 页同款「实体 setup 输入 + 按钮交互」路径）。
        view.update(cx, |view, cx| {
            view.layout.name_input.update(cx, |state, cx| {
                state.set_value("sofa", window, cx);
            });
            // 锚点留空 = 主控主屏（docs/01 [layout] 最小模型语义）。
        });
        window.click("layout-entry-add", cx);
        window.render_frame(cx);
    })
    .expect("window alive");

    // TOML diff：条目落盘（load 流水线含校验）+ RuntimeConfigUpdate 下发。
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert_eq!(saved.layout.screens.len(), 1, "布局条目已落盘");
    assert_eq!(saved.layout.screens[0].id, "sofa");
    assert_eq!(
        saved.layout.screens[0].relative_to, None,
        "空锚点 = 主控主屏"
    );
    assert_eq!(
        saved.layout.screens[0].direction,
        Some(xcontrol_config::Direction::Right),
        "下拉默认方位 = 右侧"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(CoreCommand::RuntimeConfigUpdate(_))
    ));

    // 重复 id 就地拒绝（条目间唯一，FR-1.3；TOML 不变）。
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.layout.name_input.update(cx, |state, cx| {
                state.set_value("sofa", window, cx);
            });
        });
        window.click("layout-entry-add", cx);
        window.render_frame(cx);
        let view = view.read(cx);
        let notice = view.notice.as_ref().expect("重复 id 必须就地提示");
        assert!(notice.is_error);
        assert!(
            notice.text.contains("已存在"),
            "提示文本含唯一性原因：{}",
            notice.text
        );
    })
    .expect("window alive");
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert_eq!(saved.layout.screens.len(), 1, "重复 id 不得落盘");

    // 删除条目。
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "屏幕布局", &view, cx);
        window.click(("layout-entry-del", 0_usize), cx);
        window.render_frame(cx);
    })
    .expect("window alive");
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert!(saved.layout.screens.is_empty(), "条目已删除");
}

/// 布局页空名就地拒绝（docs/06 §4：校验失败就地提示；TOML 不变）。
#[gpui_kit::test]
fn layout_page_rejects_empty_name(cx: &mut TestAppContext) {
    let dir = TempDir::new("layout-negative");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Host); // 布局页仅主控可见
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        nav_to(window, "屏幕布局", &view, cx);
        // 名称留空直接点「添加」。
        window.click("layout-entry-add", cx);
        window.render_frame(cx);
        let view = view.read(cx);
        let notice = view.notice.as_ref().expect("空名必须就地提示");
        assert!(notice.is_error);
        assert!(
            notice.text.contains("屏幕名不能为空"),
            "提示文本含字段级原因：{}",
            notice.text
        );
    })
    .expect("window alive");
    let saved: Config = xcontrol_config::load(&config_path).expect("parses");
    assert!(saved.layout.screens.is_empty(), "非法输入不得落盘");
    assert!(rx.try_recv().is_err(), "校验失败不下发命令");
}

/// 状态页无「切换角色」控件（11-21 实测反馈 1：角色切换只走托盘菜单——
/// FR-1.1 `CoreCommand::SwitchRole` 命令链路保留，设置页移除按钮与确认模态）。
#[gpui_kit::test]
fn status_page_has_no_role_switch_control(cx: &mut TestAppContext) {
    let dir = TempDir::new("status-no-role-switch");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Host);
    let (tx, mut rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        // 状态页 = 默认首页（无需导航）。
        window.render_frame(cx);
        let labels: Vec<String> = window
            .elements()
            .into_iter()
            .filter_map(|element| element.label().map(str::to_owned))
            .collect();
        assert!(
            !labels.iter().any(|label| label.contains("切换角色")),
            "状态页不得有角色切换控件（可达名全集：{labels:?}）"
        );
        assert!(view.read(cx).confirm.is_none(), "无破坏性确认模态待处理");
    })
    .expect("window alive");
    // 页面不产生任何配置命令（按钮移除后无 SwitchRole 下发路径）。
    assert!(rx.try_recv().is_err(), "状态页渲染不下发命令");
}

/// 日志页滚动（11-21 实测反馈 2）：超可视区内容可滚 + 底部自动跟随（新日志
/// 到达时在底部则跟随、上滚查看历史不抢滚动、滚回底部恢复跟随）。
#[gpui_kit::test]
fn log_page_scrolls_and_follows_bottom(cx: &mut TestAppContext) {
    let dir = TempDir::new("log-scroll");
    let config_path = dir.path().join("xdg").join("config.toml");
    let config = Config::default();
    xcontrol_config::save(&config, &config_path).expect("seed config");
    let bus = bus_with_role(Role::Host);
    let (tx, _rx) = mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
    let core = CoreHandle::new(tx);

    let (handle, view) = open_settings(
        cx,
        &config,
        Some(config_path.clone()),
        Some(dir.path().to_path_buf()),
        false,
        core,
        Arc::clone(&bus),
    );
    cx.update_window(handle, |_, window, cx| {
        // 预填超一屏的日志行（页面层快照直写——200ms 刷新循环在测试面不可靠，
        // 与既有 harness 的直接状态驱动同形）。
        view.update(cx, |view, cx| {
            view.log_lines = (0..200).map(|i| format!("line-{i}")).collect();
            cx.notify();
        });
        nav_to(window, "运行日志", &view, cx);
        window.render_frame(cx);
        let scroll = view.read(cx).log_scroll.clone();
        // ① 内容溢出 + 底部跟随：首帧（在底部判定）滚动到最新行。
        let max = scroll.max_offset();
        assert!(
            max.y > gpui_kit::px(0.),
            "内容超出可视区（max_offset = {max:?}）"
        );
        let bottom = scroll.offset();
        assert!(
            bottom.y <= -max.y + gpui_kit::px(1.),
            "新内容到达后应跟随到底部（offset = {bottom:?}，max = {max:?}）"
        );
        // ② 用户上滚：离开底部。
        window.scroll(
            "log-scroll-area",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(400.))),
            cx,
        );
        let at_history = scroll.offset();
        assert!(
            at_history.y > bottom.y,
            "上滚后滚动位置应离开底部（{at_history:?} vs {bottom:?}）"
        );
        // ③ 上滚历史中新日志到达：不抢滚动（位置保持）。
        view.update(cx, |view, cx| {
            view.log_lines.push("late-line".to_owned());
            cx.notify();
        });
        window.render_frame(cx);
        assert_eq!(
            scroll.offset().y,
            at_history.y,
            "用户上滚查看历史时新日志不得抢滚动"
        );
        // ④ 滚回底部（大幅向下滚动）→ 恢复跟随。
        window.scroll(
            "log-scroll-area",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                gpui_kit::px(0.),
                gpui_kit::px(-30_000.),
            )),
            cx,
        );
        view.update(cx, |view, cx| {
            view.log_lines.push("late-line-2".to_owned());
            cx.notify();
        });
        window.render_frame(cx);
        let max2 = scroll.max_offset();
        assert!(
            scroll.offset().y <= -max2.y + gpui_kit::px(1.),
            "滚回底部后新日志恢复自动跟随（offset = {:?}，max = {max2:?}）",
            scroll.offset()
        );
    })
    .expect("window alive");
}
