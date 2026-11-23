//! gpui 窗口装配：设置主窗口（左侧导航 + 右侧内容，docs/06 §3/§4 信息架构）、
//! 首次运行向导分流（FR-6.3）、`QuitMode::Explicit`、`on_reopen`。
//!
//! ```text
//! ui/
//! ├── mod.rs       # 本模块：窗口骨架 + 页面路由（按角色过滤，docs/06 §4）+ 提交/确认框架
//! ├── wizard.rs    # 四步向导状态机（角色 → 安全码 → 网段 → 权限引导）
//! ├── status.rs    # 状态页（角色/连接/对端/计数 + 在线工作机切换；角色切换只走托盘）
//! ├── layout.rs    # 布局页〔主控〕（FR-1.3 方位选择制编辑器：本机屏只读 + 工作机屏条目增删）
//! ├── log.rs       # 日志页（环形缓冲滚动显示）
//! ├── mapping.rs   # 映射页〔工作机〕（功能键 5 行选择器 + 预设反解 + 高级规则折叠区）
//! ├── hotkeys.rs   # 热键页〔主控〕（四键组合键文本输入 + 校验）
//! ├── clipboard.rs # 剪贴板页（范围/上限/格式）
//! ├── general.rs   # 通用页（日志级别/自启/安全码修改/语言主题）
//! └── about.rs     # 关于页（版本/协议/许可/日志目录 reveal + 清空日志）
//! ```
//!
//! 组件库裁定（11-18-m4-ui）：gpui-kit default features 开启（`component` +
//! `assets`，即 gpui-component 0.9.1 + 内嵌图标；gpui-pre 0.3.12 版本源单一，
//! `cargo tree` 断言见任务报告）。表单控件用 Input/Switch/Select/Button；
//! 左侧导航用 Sidebar/SidebarMenu。**破坏性确认**与**就地校验提示**用本窗口
//! 自绘模态/文本（gpui-kit Dialog 为 trigger 组合件、Form 无独立校验状态面
//! ——自绘保持 AX 可达且状态机在视图内，组件裁定记录见任务报告）。
//!
//! 生效框架（docs/06 §4.1）：所有配置改动经 [`SettingsView::commit_config`]——
//! 校验 → `xcontrol_config::save` 落盘 → `CoreCommand::RuntimeConfigUpdate` 下发；
//! UI 不直改运行时状态（纯投影红线）；`general.log_level` 立即生效由命令消费点
//! 完成（`logging::set_level`），本层只提示生效时点。
//!
//! API 锚定（**勿凭记忆改写**，examples/spike-tray/EVIDENCE.md 实测记录）：
//!
//! - 进程构造入口是 `gpui_kit::application()`（快照无 `Application::new()`）；
//! - `on_reopen` 注册在 **Application 构建器**上（`App` 无此方法）；
//! - 组件库初始化：`gpui_kit::init(cx)` 必须在 `app.run` 内、开窗前调用一次；
//! - 图标资产：`app.with_assets(gpui_kit::assets::Assets::new(endpoint))`（组件
//!   默认图标包；未注册时组件内建图标渲染为空，不致命）；
//! - 窗口打开：`gpui_kit::open_window(options, cx, |window, cx| ...)`（包裹
//!   `gpui_base::Root`——组件 popover/toast 的挂载点），返回 `(AnyWindowHandle,
//!   Entity<V>)`；
//! - 视图刷新：`Context::spawn(async move |weak, cx| …)` + `WeakEntity::update`
//!   + `cx.notify()`（实体释放后 update 报错 → 刷新循环自然退出）。
//!
//! 纯投影红线（architecture-rules.md 红线 4 / ui-tray.md 红线 3）：视图只持有
//! [`StateBus`] 订阅 / [`LogBuffer`] / 配置编辑副本，每 [`SNAPSHOT_PERIOD`] 拉取
//! 快照渲染；不触碰 net/core/平台层，一切变更走 `CoreHandle` 命令或 OS 级
//! `Autostart` trait（11-18-m4-tray 先例：OS 配置非核心运行时状态）。

#[cfg(test)]
mod tests_ui;

pub mod about;
pub mod clipboard;
pub mod general;
pub mod hotkeys;
pub mod layout;
pub mod log;
pub mod mapping;
pub mod status;
pub mod wizard;

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Application, AsyncApp, Bounds, Context, FontWeight, Global, QuitMode, Render,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, size,
};
use xcontrol_config::{Config, Role};
use xcontrol_tray::Autostart;

use crate::logging::LogBuffer;
use crate::state::{AppState, CoreCommand, CoreHandle, SNAPSHOT_PERIOD, StateBus};

/// 设置窗口标题（无显式模式/向导流；角色快照在状态页呈现）。
/// FR-6.5 文案表——窗口创建时取当前语言（标题为创建期快照）。
#[must_use]
pub fn title_settings() -> &'static str {
    xcontrol_i18n::tr("win.settings")
}

/// 窗口装配参数（模式标题 + auto-quit 看门狗 + 托盘命令入口 + 配置面）。
#[derive(Clone)]
pub struct WindowParams {
    /// 窗口标题（含模式提示，如「XControl · 主控」）。
    pub title: &'static str,
    /// 有值时启动后到点自动退出进程（窗口冒烟/演示用）。
    pub auto_quit: Option<Duration>,
    /// 托盘动作/设置页的命令入口（docs/06 §6：一切变更走命令）。
    pub core: CoreHandle,
    /// 启动时加载的配置（设置页编辑基线；向导完成后被新配置覆盖）。
    pub config: Config,
    /// 配置文件路径（`None` = 默认路径不可得——设置改动仅下发命令不落盘并提示）。
    pub config_path: Option<std::path::PathBuf>,
    /// 首次运行向导（FR-6.3：无配置 + 无模式参数启动 → 主窗首屏为向导）。
    /// `true` 时运行时 supervisor 处于等待相：向导完成只发
    /// `RuntimeConfigUpdate`（等待相按 config.role 定角色，勿叠加 SwitchRole）。
    pub wizard: bool,
    /// 启动即开窗（docs/06 §2：设置窗口默认不自动弹出；向导/auto-quit 冒烟/
    /// 回环演示例外——见 main.rs run_tcp 开窗策略）。
    pub open_at_start: bool,
}

/// 设置窗口页面路由（docs/06 §4 信息架构；M5 新增布局页——连接页仍属 M6+）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    /// 状态（默认首页）。
    Status,
    /// 屏幕布局（FR-1.3，M5 方位选择制编辑器）。
    Layout,
    /// 按键映射（FR-3.6）。
    Mapping,
    /// 全局热键（FR-2.5）。
    Hotkeys,
    /// 剪贴板同步（FR-4.4/4.5）。
    Clipboard,
    /// 通用（日志级别/自启/安全码/语言主题）。
    General,
    /// 日志（环形缓冲）。
    Log,
    /// 关于（版本/协议/许可/诊断）。
    About,
}

impl Page {
    /// 侧栏条目顺序（docs/06 §4 信息架构顺序；连接页未实装跳过）。
    const ALL: [Self; 8] = [
        Self::Status,
        Self::Layout,
        Self::Mapping,
        Self::Hotkeys,
        Self::Clipboard,
        Self::General,
        Self::Log,
        Self::About,
    ];

    /// 侧栏显示名（AX label 与一致——osascript 实测按 description 定位）。
    /// FR-6.5 文案表：随当前语言渲染（zh-CN 默认；tests_ui 断言依赖 zh 值）。
    fn title(self) -> &'static str {
        match self {
            Self::Status => xcontrol_i18n::tr("page.status"),
            Self::Layout => xcontrol_i18n::tr("page.layout"),
            Self::Mapping => xcontrol_i18n::tr("page.mapping"),
            Self::Hotkeys => xcontrol_i18n::tr("page.hotkeys"),
            Self::Clipboard => xcontrol_i18n::tr("page.clipboard"),
            Self::General => xcontrol_i18n::tr("page.general"),
            Self::Log => xcontrol_i18n::tr("page.log"),
            Self::About => xcontrol_i18n::tr("page.about"),
        }
    }

    /// 角色可见性（docs/06 §4 信息架构的角色归属；11-21 实测反馈裁定——
    /// 设置页不放角色切换控件，页面集合按快照角色过滤）：
    ///
    /// - 主控（Host）：屏幕布局、全局热键（布局下发/热键切换是主控语义）；
    /// - 工作机（Client）：按键映射（映射引擎在工作机侧生效，docs/03 §4）；
    /// - 状态/剪贴板/通用/日志/关于：双角色共有。
    fn visible_for(self, role: Role) -> bool {
        match self {
            Self::Layout | Self::Hotkeys => role == Role::Host,
            Self::Mapping => role == Role::Client,
            Self::Status | Self::Clipboard | Self::General | Self::Log | Self::About => true,
        }
    }

    /// 当前角色可见的侧栏页序列（保持 [`Self::ALL`] 信息架构顺序）。
    fn visible_pages(role: Role) -> Vec<Self> {
        Self::ALL
            .iter()
            .copied()
            .filter(|page| page.visible_for(role))
            .collect()
    }
}

/// 破坏性操作确认请求（docs/06 §4：改安全码必须确认；本窗口自绘模态，
/// 组件裁定见模块文档。11-21 实测反馈：角色切换不再是设置页操作——控件
/// 移除，`CoreCommand::SwitchRole` 只由托盘菜单直接下发，不经确认模态）。
#[derive(Debug, Clone)]
pub(crate) enum ConfirmRequest {
    /// 修改安全码：写入 SecretStore（file 0600）+ 更新 config 引用（确认后执行）。
    ChangeCode {
        /// 新安全码（不可回显——文案只提示"已输入 N 个字符"）。
        code: xcontrol_config::SecretCode,
    },
    /// 清空日志（11-21 实测反馈，关于页）：删全部滚动文件（含当日，代价见
    /// `logging::clear_log_files` 文档）+ 清页面环形缓冲（确认后执行——不可撤销）。
    ClearLogs,
}

/// 提交反馈条内容（就地提示，docs/06 §4：校验失败就地提示、禁止静默吞错）。
#[derive(Debug, Clone)]
pub(crate) struct Notice {
    /// 提示文本（含生效时点说明）。
    pub(crate) text: String,
    /// 是否错误（错误红、成功灰）。
    pub(crate) is_error: bool,
    /// 出现时刻（6s 自动消退）。
    pub(crate) at: Instant,
}

/// 设置主窗口视图（纯投影 + 配置编辑面；页面渲染分发到各模块）。
pub(crate) struct SettingsView {
    // —— 共享件 ——
    /// 状态总线（快照投影源）。
    pub(crate) bus: Arc<StateBus>,
    /// 日志环形缓冲（日志页数据源）。
    pub(crate) logs: LogBuffer,
    /// 命令入口（一切变更走命令）。
    pub(crate) core: CoreHandle,
    /// 配置文件路径（commit 落盘用；`None` 提示只下发不落盘）。
    pub(crate) config_path: Option<std::path::PathBuf>,
    /// 开机自启后端（feature 门控；`None` = 无平台后端，开关禁用并提示）。
    pub(crate) autostart: Option<Arc<dyn Autostart>>,
    /// 数据目录（关于页路径展示 / 向导与安全码落盘用）。
    pub(crate) data_dir: Option<std::path::PathBuf>,
    /// 运行时等待相（FR-6.3 向导流）：向导完成只发 RuntimeConfigUpdate。
    pub(crate) runtime_pending: bool,

    // —— 投影快照 ——
    /// 连接状态快照（200ms 刷新）。
    pub(crate) state: AppState,
    /// 日志行快照（200ms 刷新）。
    pub(crate) log_lines: Vec<String>,
    /// 日志页滚动句柄（滚动容器 + 自动跟随底部——11-21 反馈 2；随窗口生命
    /// 周期，页面切换不重置滚动位置）。
    pub(crate) log_scroll: gpui_kit::ScrollHandle,

    // —— 配置编辑副本（UI 工作基线；commit 后与磁盘/运行时一致）——
    /// 配置编辑副本。
    pub(crate) config: Config,

    // —— 路由与页面状态 ——
    /// 当前页面。
    pub(crate) page: Page,
    /// 向导状态（`Some` = 首屏向导未完成；跳过后可经侧栏「状态」回到设置）。
    pub(crate) wizard: Option<wizard::WizardState>,
    /// 映射页控件状态。
    pub(crate) mapping: mapping::MappingPage,
    /// 布局页控件状态（FR-1.3，M5）。
    pub(crate) layout: layout::LayoutPage,
    /// 热键页控件状态。
    pub(crate) hotkeys: hotkeys::HotkeysPage,
    /// 剪贴板页控件状态。
    pub(crate) clipboard: clipboard::ClipboardPage,
    /// 通用页控件状态。
    pub(crate) general: general::GeneralPage,

    // —— 提示与确认 ——
    /// 提示条（就地提示）。
    pub(crate) notice: Option<Notice>,
    /// 破坏性确认请求（模态展示中）。
    pub(crate) confirm: Option<ConfirmRequest>,
}

impl SettingsView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        bus: Arc<StateBus>,
        logs: LogBuffer,
        core: CoreHandle,
        config: Config,
        config_path: Option<std::path::PathBuf>,
        data_dir: Option<std::path::PathBuf>,
        with_wizard: bool,
        runtime_pending: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_state = bus.current();
        let mut view = Self {
            mapping: mapping::MappingPage::new(&config, window, cx),
            layout: layout::LayoutPage::new(&config, window, cx),
            hotkeys: hotkeys::HotkeysPage::new(&config, window, cx),
            clipboard: clipboard::ClipboardPage::new(&config, window, cx),
            general: general::GeneralPage::new(&config, window, cx),
            bus,
            logs,
            core,
            config_path,
            autostart: xcontrol_tray::platform_autostart(),
            data_dir: data_dir.or_else(xcontrol_config::default_data_dir),
            runtime_pending,
            state: initial_state,
            log_lines: Vec::new(),
            log_scroll: gpui_kit::ScrollHandle::new(),
            // bus.current() 在 bus 移入结构体前调用（上方字段序）——此处直接
            // 从结构体克隆快照不可行，改在构造后由 start_refresh 首拍补齐；
            config,
            page: Page::Status,
            wizard: None,
            notice: None,
            confirm: None,
        };
        if with_wizard {
            view.wizard = Some(wizard::WizardState::new(&view.config, window, cx));
        }
        // 输入与选择控件的变更订阅（就地校验/即时提交）；页面控件在构造期创建，
        // 订阅在此统一挂接。
        mapping::subscribe(&view.mapping, cx);
        layout::subscribe(&view.layout, cx);
        hotkeys::subscribe(&view.hotkeys, cx);
        clipboard::subscribe(&view.clipboard, cx);
        general::subscribe(&view.general, cx);
        view.start_refresh(cx);
        view
    }

    /// 200ms 刷新循环（docs/06 §6 节流投影）：拉取最新快照与日志行 → notify
    /// 重绘；实体释放（窗口关闭）后 WeakEntity::update 报错，循环自然退出。
    fn start_refresh(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(SNAPSHOT_PERIOD).await;
                let alive = view.update(cx, |view, cx| {
                    view.state = view.bus.current();
                    view.log_lines = view.logs.snapshot();
                    // 提示条 6s 自动消退（就地提示不常驻遮挡）。
                    if view
                        .notice
                        .as_ref()
                        .is_some_and(|notice| notice.at.elapsed() > Duration::from_secs(6))
                    {
                        view.notice = None;
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    tracing::debug!(target: "app::ui", "根视图已释放，刷新循环退出");
                    return;
                }
            }
        })
        .detach();
    }

    /// 就地提示（docs/06 §4：校验失败就地提示；成功也提示生效时点——诚实呈现，
    /// 时点表见 docs/06 §4.1）。
    pub(crate) fn notify_text(&mut self, text: impl Into<String>, is_error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error,
            at: Instant::now(),
        });
    }

    /// 配置改动提交框架（docs/06 §4 交互规范 + §4.1 生效时点表）：
    ///
    /// ```text
    /// mutate 校验（Err = 就地提示，不落盘不下发）
    ///   → xcontrol_config::save 落盘（config_path 缺失 → 仅下发并提示）
    ///   → CoreCommand::RuntimeConfigUpdate 下发（命令消费点分级生效）
    ///   → 更新 UI 工作副本 + 反馈条
    /// ```
    ///
    /// UI 不直改运行时状态（纯投影红线）；自启开关不走此框架（OS 级配置，
    /// general.rs 直调 Autostart trait——11-18-m4-tray 先例）。
    pub(crate) fn commit_config(
        &mut self,
        label: &str,
        mutate: impl FnOnce(&mut Config) -> Result<(), String>,
    ) {
        let mut next = self.config.clone();
        if let Err(error) = mutate(&mut next) {
            tracing::warn!(target: "app::settings", label, "{error}");
            self.notify_text(
                xcontrol_i18n::trf("notice.not_saved", &[("label", label), ("error", &error)]),
                true,
            );
            return;
        }
        match &self.config_path {
            Some(path) => match xcontrol_config::save(&next, path) {
                Ok(()) => {
                    tracing::info!(target: "app::settings", label, path = %path.display(),
                        "配置改动已落盘并下发运行时（docs/06 §4.1 生效时点表）");
                    self.notify_text(
                        xcontrol_i18n::trf("notice.saved", &[("label", label)]),
                        false,
                    );
                }
                Err(error) => {
                    tracing::warn!(target: "app::settings", label, %error, "配置落盘失败");
                    self.notify_text(
                        xcontrol_i18n::trf(
                            "notice.save_failed",
                            &[("label", label), ("error", &error.to_string())],
                        ),
                        true,
                    );
                    return;
                }
            },
            None => {
                tracing::warn!(target: "app::settings", label,
                    "配置路径不可得：改动仅下发运行时（不落盘，重启后丢失）");
                self.notify_text(
                    xcontrol_i18n::trf("notice.dispatched_no_path", &[("label", label)]),
                    true,
                );
            }
        }
        self.core
            .send(CoreCommand::RuntimeConfigUpdate(Box::new(next.clone())));
        self.config = next;
    }

    /// 确认模态出栈执行（确认按钮回调）。
    fn run_confirmed(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.confirm.take() else {
            return;
        };
        match request {
            ConfirmRequest::ChangeCode { code } => {
                self.apply_new_code(&code);
            }
            ConfirmRequest::ClearLogs => {
                about::clear_logs(self, cx);
            }
        }
        cx.notify();
    }

    /// 安全码写入（确认后路径，docs/07 §3.3 存储与换码协议）：
    ///
    /// 原子顺序（双故障收口裁定——**先写 store 后存 config，失败回滚 store**）：
    /// 1. 读旧码（`NotFound` 也记下——回滚时删除而非覆盖）；
    /// 2. 新码写 store（keychain 可用且引用为 keychain → 钥匙串；否则 file 0600）；
    /// 3. config 落盘 + 下发（`commit_config`）；
    /// 4. config 落盘失败 → 回滚：旧码写回 store（旧码本不存在则删除新条目）
    ///    ——避免"store 已是新码、config 仍指旧引用"的半更新态。
    ///
    /// 换码生效语义（docs/07 §3.3）：引用变化经 `RuntimeConfigUpdate` 传播，
    /// 各会话循环断开全部旧会话、supervisor 按新码重派生 PSK（session_rt
    /// `store_runtime_config` 的 code_ref_changed 消费链）。
    fn apply_new_code(&mut self, code: &xcontrol_config::SecretCode) {
        let Some(data_dir) = self.data_dir.clone() else {
            self.notify_text(xcontrol_i18n::tr("notice.data_dir_missing"), true);
            return;
        };
        // 引用策略（keychain 真接线，11-19-m6-security）：当前引用为 keychain
        // → 尊重（写钥匙串；后端探测不可用回退 file + 告警）；file → 保持原
        // 路径；其他（未解析）→ 默认 file 路径。
        let current_ref = xcontrol_config::SecretRef::parse(&self.config.security_code_ref);
        let target_ref = match current_ref {
            Some(xcontrol_config::SecretRef::Keychain { name }) => {
                let keychain = xcontrol_config::KeychainSecretStore::new(name.clone());
                if keychain.is_available() {
                    xcontrol_config::SecretRef::Keychain { name }
                } else {
                    tracing::warn!(target: "app::settings",
                        "钥匙串不可用（docs/07 §3.3）：本次写入回退 file 0600（引用随本次提交切换）");
                    file_ref_default()
                }
            }
            Some(reference @ xcontrol_config::SecretRef::File { .. }) => reference,
            None => file_ref_default(),
        };
        let store = xcontrol_config::build_secret_store(&target_ref, &data_dir);
        // ① 旧码快照（回滚依据）。
        let old_code = store.retrieve().ok();
        // ② 新码入 store。
        if let Err(error) = store.store(code) {
            tracing::warn!(target: "app::settings", %error, "安全码写入失败");
            self.notify_text(
                xcontrol_i18n::trf("common.code_write_failed", &[("error", &error.to_string())]),
                true,
            );
            return;
        }
        // ③ config 落盘 + 下发（成功判定：工作副本引用已更新——commit_config
        // 在落盘失败时不更新副本）。
        let new_ref_value = target_ref.to_config_value();
        self.commit_config(xcontrol_i18n::tr("label.code"), |config| {
            config.security_code_ref = new_ref_value;
            Ok(())
        });
        if self.config.security_code_ref == target_ref.to_config_value() {
            tracing::info!(target: "app::settings", ref = %target_ref.to_config_value(),
                "安全码已写入存储后端，config 引用更新（换码协议：全部会话断开重握手，docs/07 §3.3）");
        } else {
            // ④ config 落盘失败 → 回滚 store（旧码写回 / 无旧码则删除新条目）。
            tracing::warn!(target: "app::settings",
                "config 落盘失败（docs/07 §3.3 双故障收口）：回滚存储后端到旧码状态");
            let rollback = match &old_code {
                Some(old) => store.store(old),
                None => store.delete(),
            };
            match rollback {
                Ok(()) => self.notify_text(xcontrol_i18n::tr("notice.code_rollback_ok"), true),
                Err(error) => self.notify_text(
                    xcontrol_i18n::trf(
                        "notice.code_rollback_failed",
                        &[("error", &error.to_string())],
                    ),
                    true,
                ),
            }
        }
    }
}

/// 默认 file 引用（`file:secrets/security_code`，docs/01 §5 回退路径）。
fn file_ref_default() -> xcontrol_config::SecretRef {
    xcontrol_config::SecretRef::File {
        path: std::path::PathBuf::from(xcontrol_config::DEFAULT_SECRET_FILE_RELATIVE),
    }
}

impl Render for SettingsView {
    // 返回统一 AnyElement（向导分支与设置布局异构；Render trait 允许 impl trait
    // 收敛——本实现恒为 AnyElement，lint 提示签名细化有意为之）。
    #[allow(refining_impl_trait)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        // 向导首屏（FR-6.3）：全窗向导流，无侧栏。
        if let Some(mut wizard_state) = self.wizard.take() {
            let rendered = wizard::render(self, &mut wizard_state, window, cx);
            self.wizard = Some(wizard_state);
            return rendered;
        }

        let theme = cx.theme().clone();
        // 角色过滤（docs/06 §4 信息架构角色归属）：侧栏页集合随快照角色刷新
        //（托盘切换角色 → 快照 role 变化 → 下一拍侧栏重排）；当前页因角色切换
        // 不可见时回落状态页（避免渲染隐藏页）。
        let role = self.state.role;
        if !self.page.visible_for(role) {
            self.page = Page::Status;
        }
        // 左侧导航（gpui-kit Sidebar/SidebarMenu）。
        let mut menu = SidebarMenu::new();
        for page in Page::visible_pages(role) {
            menu = menu.child(
                SidebarMenuItem::new(page.title())
                    .accessibility_label(page.title())
                    .active(self.page == page)
                    .on_click(cx.listener(move |view: &mut Self, _ev, _window, cx| {
                        tracing::info!(target: "app::ui", page = page.title(), "切换设置页");
                        view.page = page;
                        cx.notify();
                    })),
            );
        }
        let sidebar = Sidebar::new("settings-nav").child(menu);

        // 右侧内容：页面 + 提示条 + 确认模态。
        let content = match self.page {
            Page::Status => status::render(self, window, cx).into_any_element(),
            Page::Layout => layout::render(self, window, cx).into_any_element(),
            Page::Mapping => mapping::render(self, window, cx).into_any_element(),
            Page::Hotkeys => hotkeys::render(self, window, cx).into_any_element(),
            Page::Clipboard => clipboard::render(self, window, cx).into_any_element(),
            Page::General => general::render(self, window, cx).into_any_element(),
            Page::Log => log::render(&self.log_lines, &self.log_scroll).into_any_element(),
            Page::About => about::render(self, cx).into_any_element(),
        };

        let mut body = v_flex()
            .flex_1()
            .h_full()
            .gap_2()
            .p_4()
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(content);
        if let Some(notice) = &self.notice {
            let color = if notice.is_error {
                theme.danger
            } else {
                theme.muted_foreground
            };
            body = body.child(
                div()
                    .id("notice-bar")
                    .child(SharedString::from(notice.text.clone()))
                    .text_size(px(12.))
                    .text_color(color),
            );
        }

        let mut layout = h_flex().size_full().child(sidebar).child(body);
        // 破坏性确认模态（docs/06 §4；自绘 overlay——组件裁定见模块文档）。
        if self.confirm.is_some() {
            layout = layout.child(self.render_confirm_modal(cx));
        }
        layout.into_any_element()
    }
}

impl SettingsView {
    /// 确认模态（居中卡片 + 全屏遮罩；确认/取消双按钮，AX 标签稳定）。
    fn render_confirm_modal(&self, cx: &mut Context<Self>) -> gpui_kit::Stateful<gpui_kit::Div> {
        let theme = cx.theme().clone();
        let (title, description) = match &self.confirm {
            Some(ConfirmRequest::ChangeCode { code }) => (
                xcontrol_i18n::tr("confirm.code_title"),
                xcontrol_i18n::trf(
                    "confirm.code_body",
                    &[("n", &code.expose().chars().count().to_string())],
                ),
            ),
            Some(ConfirmRequest::ClearLogs) => (
                xcontrol_i18n::tr("confirm.clear_logs_title"),
                xcontrol_i18n::tr("confirm.clear_logs_body").to_owned(),
            ),
            None => ("", String::new()),
        };
        div()
            .id("confirm-modal")
            .absolute()
            .inset_0()
            .bg(theme.background.opacity(0.55))
            .flex()
            .justify_center()
            .items_center()
            .child(
                v_flex()
                    .w(px(400.))
                    .p_4()
                    .gap_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(
                        div()
                            .child(SharedString::from(title.to_owned()))
                            .text_size(px(15.))
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        div()
                            .child(SharedString::from(description))
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("confirm-cancel")
                                    .label(xcontrol_i18n::tr("common.cancel"))
                                    .on_click(cx.listener(|_view: &mut Self, _ev, _window, cx| {
                                        tracing::info!(target: "app::ui", "破坏性操作已取消");
                                        _view.confirm = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-ok")
                                    .label(xcontrol_i18n::tr("common.confirm"))
                                    .danger()
                                    .on_click(cx.listener(|view, _ev, _window, cx| {
                                        view.run_confirmed(cx);
                                    })),
                            ),
                    ),
            )
    }
}

/// 按显示名取页面（UI 测试辅助：tests_ui 侧栏序号 ↔ 页面对齐）。
#[cfg(test)]
pub(crate) fn page_by_title(title: &str) -> Page {
    Page::ALL
        .iter()
        .find(|p| p.title() == title)
        .copied()
        .expect("页面标题必在 ALL 中")
}

/// 主窗口句柄全局（macOS Dock reopen / 托盘唤起时激活/重建窗口用）。
struct MainWindowHandle(gpui_kit::AnyWindowHandle);

impl Global for MainWindowHandle {}

/// 向导流终结记录（11-18-m4-ui check 修复）：窗口重建（托盘「打开设置…」/
/// Dock reopen）时的向导新鲜度判据——向导已完成（运行时已唤醒）或已被显式
/// 跳过（运行时保持等待相）后，重建窗口不再复现向导。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardFlowOutcome {
    /// 向导完成：config 已落盘、`RuntimeConfigUpdate` 已下发（等待相退出）。
    Completed,
    /// 向导被跳过：无落盘无下发，运行时保持等待相。
    Skipped,
}

struct WizardFlowDone(WizardFlowOutcome);

impl Global for WizardFlowDone {}

/// 窗口重建时的向导/等待相判定（纯函数便于单测）：返回
/// `(是否进向导, 运行时是否等待相)`。
///
/// - 本进程内向导已完成/跳过过 → 不再复现向导；等待相按终结方式取值
///   （完成 = 运行时已唤醒；跳过 = 仍待命）；
/// - 未终结过但 config 已落盘（异常时序兜底）→ 不复现向导（不覆盖既有配置），
///   等待相保守取 true（状态页提示可见，运行时实际状态以快照为准）。
fn wizard_rebuild_flags(
    startup_wizard: bool,
    flow_done: Option<WizardFlowOutcome>,
    config_exists: bool,
) -> (bool, bool) {
    let wizard = startup_wizard && flow_done.is_none() && !config_exists;
    let runtime_pending = match flow_done {
        Some(WizardFlowOutcome::Completed) => false,
        Some(WizardFlowOutcome::Skipped) => true,
        None => startup_wizard,
    };
    (wizard, runtime_pending)
}

/// 打开（或激活）设置主窗口（`pub(crate)`：启动装配 / 托盘「打开设置…」/
/// Dock reopen 复用同一入口——单窗口语义一处维护）。
pub(crate) fn open_main_with_bus(
    cx: &mut App,
    bus: &Arc<StateBus>,
    logs: &LogBuffer,
    params: &WindowParams,
) -> anyhow::Result<()> {
    // 已开窗：置前激活，不重复开窗（docs/06 §2 on_reopen 语义：已开则展示）。
    // 11-20 实测修复：托盘动作时本进程非前台，单 `window.activate_window()`
    // 被 macOS 忽略——先 `cx.activate(true)`（app 级前台化，ignoring_other_apps
    // 绕过其他 app 的前台锁），再 activate 窗口；窗口首次打开路径同理。
    cx.activate(true);
    let existing = cx.try_global::<MainWindowHandle>().map(|g| g.0);
    if let Some(handle) = existing {
        let activated = handle
            .update(cx, |_view, window, _app| window.activate_window())
            .is_ok();
        if activated {
            // 句柄有效性二次校验（11-20 双开窗修复）：update Ok ≠ 窗口活着
            //（gpui 对已关窗口的 update 可能仍返回 Ok）——用窗口列表核验。
            if cx
                .windows()
                .iter()
                .any(|w| w.window_id() == handle.window_id())
            {
                tracing::info!(target: "app::ui", "设置窗口已存在，置前激活（app 级激活已先行）");
                return Ok(());
            }
            tracing::info!(target: "app::ui", "句柄指向已关闭窗口，清缓存重建");
            cx.remove_global::<MainWindowHandle>();
        } else {
            tracing::info!(target: "app::ui", "窗口句柄更新失败，清缓存重建");
            cx.remove_global::<MainWindowHandle>();
        }
    }

    let bounds = Bounds::centered(None, size(px(760.), px(620.)), cx);
    // 向导新鲜度（11-18-m4-ui check 修复）：重建窗口仅在向导流仍未终结且
    // config 未落盘时复现向导（向导完成 = `WizardFlowDone(Completed)`，跳过 =
    // `WizardFlowDone(Skipped)`——两处终结点见 wizard.rs）。
    let flow_done = cx.try_global::<WizardFlowDone>().map(|done| done.0);
    let config_exists = params
        .config_path
        .as_deref()
        .is_some_and(std::path::Path::exists);
    let (wizard, runtime_pending) = wizard_rebuild_flags(params.wizard, flow_done, config_exists);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from(if wizard {
                xcontrol_i18n::tr("win.wizard")
            } else {
                params.title
            })),
            ..Default::default()
        }),
        ..Default::default()
    };
    let bus = Arc::clone(bus);
    let logs = logs.clone();
    let core = params.core.clone();
    let config = params.config.clone();
    let config_path = params.config_path.clone();
    let (handle, _view) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| {
            SettingsView::new(
                bus,
                logs,
                core,
                config,
                config_path,
                None,
                wizard,
                runtime_pending,
                window,
                cx,
            )
        })
    })
    .map_err(|error| anyhow::anyhow!("打开设置窗口失败：{error}"))?;
    cx.set_global(MainWindowHandle(handle));
    // 关闭即清句柄缓存（11-20 双开窗修复）：QuitMode::Explicit 下用户可关窗，
    // 残留的全局句柄指向死窗口会让下次「打开设置」误判"已存在"而跳过重建。
    let closed_id = handle.window_id();
    // detach：进程生命周期订阅（drop = 退订——每次开窗都建新订阅也无妨，
    // 旧订阅随窗口关闭自然失效，回调内的 id 比对保证只处理自己的窗口）。
    cx.on_window_closed(move |cx, closed| {
        if closed == closed_id {
            // 真清缓存（&mut App 支持 set/remove global）：下次「打开设置」
            // 走重建分支而非被死句柄短路。
            if cx.try_global::<MainWindowHandle>().is_some() {
                cx.remove_global::<MainWindowHandle>();
            }
            tracing::info!(target: "app::ui", "设置窗口已关闭（句柄缓存清除）");
        }
    })
    .detach();
    tracing::info!(target: "app::ui",
        wizard, "设置窗口已打开（{}）", if wizard { "首次运行向导" } else { "设置" });
    Ok(())
}

/// 打开窗口失败兜底：记录错误并退出进程（无窗口的常驻进程没有意义）。
/// `pub(crate)`：托盘「打开设置…」action 复用同一入口。
pub(crate) fn open_main_or_quit(
    cx: &mut App,
    bus: Arc<StateBus>,
    logs: LogBuffer,
    params: WindowParams,
) {
    // 11-20 双开窗真根因（gpui-pre 源码 + 实测日志定位）：托盘 action 回调运行在
    // gpui 的 action 分发上下文内，此时 `AnyWindowHandle::update` →
    // `update_window_id` 的窗口栈重入校验失败——**恒返回 Err("window not
    // found")**，即使窗口活着。表现：每次托盘点开都误判"句柄更新失败"→ 重建
    // → 叠窗。正解：**defer 到事件循环外**再走激活/重建逻辑（defer 闭包在
    // 当前 update 结束后执行，无重入）。参数收所有权（defer 闭包需 move）。
    cx.defer(move |cx| {
        if let Err(error) = open_main_with_bus(cx, &bus, &logs, &params) {
            tracing::error!(target: "app::ui", "{error}");
            cx.quit();
        }
    });
}

/// 窗口模式主回路（阻塞至进程退出）。
///
/// `QuitMode::Explicit`：关窗不退进程（后台常驻语义，ADR-0001；退出经托盘菜单
/// 或 Ctrl+C）。M4 同回路装配托盘（FR-6.1）与组件库初始化（`gpui_kit::init`，
/// docs/06 §1 组件裁定；图标资产随 default features 内嵌）。
pub fn run_window(bus: Arc<StateBus>, logs: LogBuffer, params: WindowParams) {
    let app: Application = gpui_kit::application();
    let app = app
        .with_quit_mode(QuitMode::Explicit)
        .with_assets(gpui_kit::assets::Assets::new(""));

    // macOS Dock reopen（docs/06 §2）：Application 构建器上注册（spike 实测）。
    // on_reopen 借用 &self 返回 &Self——按语句调用（spike-a 同形），app 所有权不变。
    #[cfg(target_os = "macos")]
    {
        let bus = Arc::clone(&bus);
        let logs = logs.clone();
        let params = params.clone();
        app.on_reopen(move |cx: &mut App| {
            tracing::info!(target: "app::ui", "on_reopen 触发：Dock reopen → 展示设置窗口");
            open_main_or_quit(cx, Arc::clone(&bus), logs.clone(), params.clone());
        });
    }

    app.run(move |cx: &mut App| {
        // 组件库初始化（gpui_kit::init = gpui_component::init：主题/输入/列表/
        // 弹层等全局状态）——必须在任何组件渲染前调用（gpui-kit 文档契约）。
        gpui_kit::init(cx);

        // 开窗策略（docs/06 §2）：设置窗口默认不自动弹出；向导（首次运行）、
        // auto-quit 窗口冒烟与回环演示例外（main.rs run_tcp 已算好 open_at_start）。
        if params.open_at_start {
            open_main_or_quit(cx, Arc::clone(&bus), logs.clone(), params.clone());
        } else {
            tracing::info!(target: "app::ui",
                "设置窗口不自动弹出（docs/06 §2）：托盘「打开设置…」/Dock reopen 唤起");
        }

        // 托盘装配（FR-6.1）：图标 + 固定顺序菜单 + action → CoreHandle；失败
        // 告警降级为无托盘运行（规则 4——窗口路径不因托盘不可用中断）。
        crate::tray_rt::install(cx, &bus, &params.core, &logs, &params);

        // auto-quit 看门狗（窗口冒烟/演示）：到点退出进程。
        if let Some(limit) = params.auto_quit {
            let bg = cx.background_executor().clone();
            cx.spawn(async move |cx: &mut AsyncApp| {
                bg.timer(limit).await;
                tracing::info!(target: "app::ui", "auto-quit 到点，进程退出");
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
}

/// headless 模式主回路（FR-6.4 的 gpui 分支；`--loopback --auto-quit-ms` CI 冒烟用）。
///
/// 无窗口、无 Dock 图标（macOS Accessory 策略），仅维持 gpui 事件回路到点退出。
pub fn run_headless(auto_quit: Duration) {
    let app: Application = gpui_kit::platform::headless();
    app.run(move |cx: &mut App| {
        tracing::info!(target: "app::ui", "headless 回路启动（{}ms 后退出）", auto_quit.as_millis());
        let bg = cx.background_executor().clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            bg.timer(auto_quit).await;
            tracing::info!(target: "app::ui", "headless auto-quit 到点，进程退出");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

/// 回环模式窗口标题。
#[must_use]
pub fn title_loopback() -> &'static str {
    xcontrol_i18n::tr("win.loopback")
}
/// 主控模式窗口标题。
#[must_use]
pub fn title_host() -> &'static str {
    xcontrol_i18n::tr("win.host")
}
/// 从机模式窗口标题。
#[must_use]
pub fn title_client() -> &'static str {
    xcontrol_i18n::tr("win.client")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窗口标题与模式语义对齐（冒烟输出断言依赖 zh 默认语言下的字面量）。
    #[test]
    fn titles_are_stable() {
        assert_eq!(title_loopback(), "XControl · 回环演示");
        assert_eq!(title_host(), "XControl · 主控");
        assert_eq!(title_client(), "XControl · 工作机");
        assert_eq!(title_settings(), "XControl · 设置");
    }

    /// WindowParams 是简单值对象（Clone——含非 Copy 的 CoreHandle 与 Config）。
    #[test]
    fn window_params_is_clone() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let params = WindowParams {
            title: title_host(),
            auto_quit: Some(Duration::from_millis(100)),
            core: CoreHandle::new(tx),
            config: Config::default(),
            config_path: None,
            wizard: false,
            open_at_start: false,
        };
        let copied = params.clone();
        assert_eq!(copied.title, params.title);
        assert_eq!(copied.auto_quit, params.auto_quit);
        assert!(!copied.wizard);
    }

    /// 页面路由：顺序与 docs/06 §4 信息架构一致，title 稳定且互异（AX 锚点）。
    #[test]
    fn pages_are_ordered_and_stable() {
        let titles: Vec<&str> = Page::ALL.iter().map(|p| p.title()).collect();
        assert_eq!(
            titles,
            [
                "运行状态",
                "屏幕布局",
                "按键映射",
                "全局热键",
                "剪贴板同步",
                "通用设置",
                "运行日志",
                "关于"
            ]
        );
        let mut labels: Vec<&str> = Page::ALL.iter().map(|p| p.title()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Page::ALL.len(), "页面标题必须互异（AX 锚点）");
    }

    /// 页面角色过滤矩阵（11-21 实测反馈裁定，docs/06 §4 角色归属）：
    /// 主控独占 布局/热键，工作机独占 映射，其余双角色共有。
    #[test]
    fn pages_filter_by_role() {
        use xcontrol_config::Role;
        let host: Vec<&str> = Page::visible_pages(Role::Host)
            .iter()
            .map(|page| page.title())
            .collect();
        assert_eq!(
            host,
            [
                "运行状态",
                "屏幕布局",
                "全局热键",
                "剪贴板同步",
                "通用设置",
                "运行日志",
                "关于"
            ],
            "主控可见页（无按键映射）"
        );
        let client: Vec<&str> = Page::visible_pages(Role::Client)
            .iter()
            .map(|page| page.title())
            .collect();
        assert_eq!(
            client,
            [
                "运行状态",
                "按键映射",
                "剪贴板同步",
                "通用设置",
                "运行日志",
                "关于"
            ],
            "工作机可见页（无屏幕布局/全局热键）"
        );
        assert!(
            !Page::Layout.visible_for(Role::Client) && !Page::Hotkeys.visible_for(Role::Client),
            "工作机不见主控专属页"
        );
        assert!(!Page::Mapping.visible_for(Role::Host), "主控不见映射页");
    }

    /// 窗口重建的向导/等待相判定（11-18-m4-ui check 修复）：完成/跳过后向导
    /// 不复现，等待相按终结方式取值；config 已落盘的异常时序不复现向导。
    #[test]
    fn wizard_rebuild_flags_matrix() {
        // 首开（未终结、无 config）：进向导，运行时待命。
        assert_eq!(
            wizard_rebuild_flags(true, None, false),
            (true, true),
            "向导流首开"
        );
        // 完成后重建：不再复现向导，运行时已唤醒。
        assert_eq!(
            wizard_rebuild_flags(true, Some(WizardFlowOutcome::Completed), false),
            (false, false),
            "向导完成后重建回设置页"
        );
        // 跳过后重建：不再复现向导，运行时仍待命（状态页提示可见）。
        assert_eq!(
            wizard_rebuild_flags(true, Some(WizardFlowOutcome::Skipped), false),
            (false, true),
            "跳过后重建：运行时待命提示保留"
        );
        // 未终结但 config 已落盘（异常时序兜底）：不覆盖既有配置，保守待命。
        assert_eq!(
            wizard_rebuild_flags(true, None, true),
            (false, true),
            "config 已存在不复现向导"
        );
        // 非向导流启动：恒 (false, false)。
        assert_eq!(wizard_rebuild_flags(false, None, false), (false, false));
    }
}
