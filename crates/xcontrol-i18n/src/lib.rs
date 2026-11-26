//! `xcontrol-i18n` — 轻量文案表框架（FR-6.5，docs/06 §7）。
//!
//! ```text
//! tr("menu.pause")            # 当前语言的静态文案（zh-CN 默认 / en）
//! trf("tray.state_n_online", &[("n", "2")])  # 带占位符替换（"{n} 台在线"）
//! ```
//!
//! 设计裁定（11-19-m6-release）：
//!
//! - **static table，零依赖**：两张 `&[(&str, &str)]` 常量表 + 线性查找——文案量
//!   ~200 条、查找发生在渲染/投影路径（托盘 200ms 节流），性能无关紧要；不引
//!   fluent/gettext 等重依赖（PRD 裁定）。
//! - **语言状态 = 进程全局**（`AtomicU8`）：`set_language` 在启动（读 config
//!   `[general].language`）与 `RuntimeConfigUpdate` 消费点（docs/06 §4.1，与
//!   `log_level` 同点）调用；`tr()` 读当前值。切换后静态文案随下一次渲染生效
//!   （托盘 ≤1s——刷新循环重投影；设置页随交互重绘），控件构造期快照（输入框
//!   占位/下拉选项）在下个窗口生命周期对齐——v1 边界，诚实呈现。
//! - **范围**：gpui UI 层（设置页/向导/确认模态/窗口标题）+ 托盘（菜单/状态头/
//!   tooltip）。错误信息（thiserror Display）与 CLI `--status` 输出 v1 保持
//!   中文单语（裁定记录见 docs/06 §7 修订注记——双语化成本不成比例，en 覆盖
//!   UI 层即满足 FR-6.5「UI 多语言」）。
//! - **缺 key 回退**：查不到时返回 key 本身（不 panic、不吞错——key 直接显示
//!   即回归信号，测试面由 `parity` 测试兜底两表一致性）。
//!
//! 表约定：key 用点分层命名空间（`menu.*` / `page.*` / `wizard.*` / ...）；
//! 占位符 `{name}` 只经 [`trf`] 替换；zh 表文案与既有 UI 字面量逐字一致
//! （tests_ui 断言依赖 zh 默认值）。

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU8, Ordering};

/// 界面语言（FR-6.5）：zh-CN 默认 / en 可选（config `[general].language`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    /// 简体中文（默认）。
    #[default]
    ZhCn,
    /// English。
    En,
}

impl Lang {
    /// config 字面值（`[general].language`，docs/01 §5）。
    #[must_use]
    pub const fn as_config_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }

    /// 解析 config 字面值；未知值 `None`（校验层拒绝，调用方回退默认）。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "zh-CN" => Some(Self::ZhCn),
            "en" => Some(Self::En),
            _ => None,
        }
    }
}

/// 当前语言（进程全局；0 = ZhCn、1 = En——[`Lang`] 二值编码）。
static CURRENT_LANG: AtomicU8 = AtomicU8::new(0);

/// 设置当前语言（启动读 config / `RuntimeConfigUpdate` 消费点调用）。
pub fn set_language(lang: Lang) {
    CURRENT_LANG.store(
        match lang {
            Lang::ZhCn => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

/// 当前语言。
#[must_use]
pub fn current() -> Lang {
    match CURRENT_LANG.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::ZhCn,
    }
}

/// 当前语言的文案（缺 key 回退 key 本身）。key 恒为 `'static` 字面量——
/// 回退路径返回 key 自身不需要借用输入的生命周期。
#[must_use]
pub fn tr(key: &'static str) -> &'static str {
    tr_with(key, current())
}

/// 指定语言的文案（测试/多语言对照面；缺 key 回退 key 本身）。
#[must_use]
pub fn tr_with(key: &'static str, lang: Lang) -> &'static str {
    let table = match lang {
        Lang::ZhCn => ZH,
        Lang::En => EN,
    };
    for (candidate, value) in table {
        if *candidate == key {
            return value;
        }
    }
    key
}

/// 带占位符替换的文案（`{name}` → args 对应值；简单顺序替换，无转义语义）。
#[must_use]
pub fn trf(key: &'static str, args: &[(&str, &str)]) -> String {
    let mut text = tr(key).to_owned();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

// —— 文案表（zh-CN 完整 + en 全量；key 按命名空间字母序排列便于对照）——
//
// zh 值与 M4/M5 落地的 UI 字面量逐字一致（tests_ui 断言依赖）；en 值为本任务
// 全量翻译。两表一致性（同 key 集、占位符集合一致）由 `parity` 测试钉死。

/// zh-CN 文案表（key, 值）。
const ZH: &[(&str, &str)] = &[
    ("about.clear_logs", "清空日志文件"),
    (
        "about.clear_logs_hint",
        "删除全部滚动日志文件（含当日）并清空本页日志缓冲",
    ),
    ("about.license", "许可"),
    (
        "about.license_value",
        "MIT OR Apache-2.0（gpui / gpui-kit / gpui-tray 同为 Apache-2.0 或 MIT，依赖许可见 deny.toml 审计）",
    ),
    ("about.logs_hint", "日志目录（滚动文件，自动保留 14 天）"),
    (
        "about.no_data_dir",
        "（数据目录不可得——滚动文件日志未启用）",
    ),
    ("about.no_logs_dir", "（无日志目录可打开）"),
    ("about.open_logs", "打开日志目录"),
    ("about.subtitle", "版本信息、开源许可与日志维护。"),
    ("about.title", "XControl — 跨设备键鼠共享"),
    ("about.version", "版本"),
    (
        "about.version_value",
        "XControl {v}（协议 XCP-VERSION {major}.{minor}）",
    ),
    (
        "clipboard.err_not_number",
        "剪贴板上限未保存：{raw} 不是非负整数",
    ),
    (
        "clipboard.err_over_limit",
        "剪贴板上限未保存：{v} 超过校验上限 {n}",
    ),
    ("clipboard.format_image", "图片"),
    ("clipboard.format_text", "文本"),
    ("clipboard.formats_hint", "同步格式（改动下会话生效）"),
    (
        "clipboard.intro",
        "主控复制的内容自动同步到工作机（文本/图片，格式见下）。改动**下会话生效**。",
    ),
    (
        "clipboard.max_hint",
        "大小上限（字节；0 = 禁用同步；校验上限 {n}）",
    ),
    ("clipboard.ph_max_bytes", "字节（0 = 禁用；上限 {n}）"),
    ("clipboard.scope", "同步范围"),
    ("clipboard.scope_all", "全部工作机"),
    ("clipboard.scope_current", "仅当前受控工作机"),
    ("common.add", "添加"),
    ("common.cancel", "取消"),
    ("common.check_suffix", "✓"),
    ("common.code_write_failed", "安全码写入失败：{error}"),
    ("common.confirm", "确认"),
    ("common.delete", "删除"),
    ("common.last_handshake_failed", " · 上次握手失败"),
    ("common.none", "无"),
    ("common.save", "保存"),
    (
        "confirm.code_body",
        "新安全码（{n} 个字符）将写入本机存储后端，config 引用同步更新；已配对的设备需用新码重新配对。",
    ),
    ("confirm.code_title", "确认修改安全码？"),
    (
        "confirm.clear_logs_body",
        "将删除日志目录中的全部滚动文件（含当日）并清空页面日志缓冲。删除后至次日轮转前的新日志只进页面显示，不落盘。此操作不可撤销。",
    ),
    ("confirm.clear_logs_title", "清空日志？"),
    (
        "general.autostart_disable_failed",
        "开机自启停用失败：{error}",
    ),
    ("general.autostart_disabled", "开机自启已停用"),
    (
        "general.autostart_enable_failed",
        "开机自启启用失败：{error}",
    ),
    (
        "general.autostart_enabled",
        "开机自启已启用（以当前二进制路径注册）",
    ),
    ("general.autostart_hint", "开机自启（OS 级，立即生效）"),
    ("general.autostart_switch", "登录时自动启动 XControl"),
    ("general.change_btn", "修改…"),
    ("general.code_hint", "安全码修改（破坏性操作，需确认）"),
    (
        "general.code_ref_hint",
        "当前引用：{ref}（keychain 引用写钥匙串；file 引用写 0600 文件；钥匙串不可用时自动回退 file——docs/07 §3.3。换码后全部会话断开重握手）",
    ),
    (
        "general.err_code_too_short",
        "安全码建议 8 位以上（当前过短，未提交）",
    ),
    ("general.language_hint", "界面语言（切换即时生效）"),
    ("general.log_level_hint", "日志级别（立即生效）"),
    (
        "general.subtitle",
        "界面语言、开机自启、安全码与日志级别等应用级设置。",
    ),
    (
        "general.no_autostart_backend",
        "当前编译面无自启后端（平台 feature 未启用）",
    ),
    ("general.ph_new_code", "新安全码（8 位以上，两端一致）"),
    ("general.theme_follow_system", "跟随系统"),
    ("general.theme_hint", "主题（v1 固定）"),
    ("hotkeys.back_to_host", "回主控"),
    (
        "hotkeys.intro",
        "在主控上全局生效的快捷键：切换目标、回主控、暂停/恢复共享。组合键形如 Ctrl+Alt+Right（修饰键 + 主键）。热键两端本地注册，命中即本地处理、不转发对端。改动**即时生效**（运行中会话热键表热替换；win/x11 平台注册随改动重建）。",
    ),
    ("hotkeys.next", "切到下一台"),
    ("hotkeys.pause", "暂停/恢复共享"),
    ("hotkeys.prev", "切到上一台"),
    ("label.clipboard_format", "剪贴板格式"),
    ("label.clipboard_max", "剪贴板上限"),
    ("label.clipboard_scope", "剪贴板范围"),
    ("label.code", "安全码"),
    ("label.confirmed_host", "已记忆主控"),
    ("label.hotkeys", "热键"),
    ("label.language", "语言"),
    ("label.layout_add", "新增布局条目"),
    ("label.layout_delete", "删除布局条目"),
    ("label.log_level", "日志级别"),
    ("label.mapping_preset", "映射预设"),
    ("label.passthrough", "直通模式"),
    ("label.rule_add", "新增规则"),
    ("label.rule_delete", "删除规则"),
    (
        "log.subtitle",
        "应用运行日志（滚动查看；页面缓冲最近 500 行，落盘文件见「关于」页）。新日志自动跟随到底部，上滚查看历史时不打断。",
    ),
    (
        "layout.add_hint",
        "新增条目（相对锚点屏的方位；锚点可填本机屏名或既有条目名）",
    ),
    ("layout.default_anchor", "主控主屏"),
    ("layout.dir_down", "下方"),
    ("layout.dir_left", "左侧"),
    ("layout.dir_right", "右侧"),
    ("layout.dir_up", "上方"),
    ("layout.direction", "方位"),
    ("layout.entries", "外接（工作机）屏条目（{n}）"),
    ("layout.entry_summary", "{id}：{anchor} {dir}"),
    (
        "layout.err_duplicate",
        "屏幕名 {id} 已存在（条目间唯一，FR-1.3）",
    ),
    ("layout.err_name_empty", "屏幕名不能为空"),
    ("layout.err_not_saved", "新增布局条目未保存：{error}"),
    (
        "layout.err_stale_index",
        "条目序号已过期（列表已变化），请刷新后重试",
    ),
    (
        "layout.intro",
        "定义工作机屏幕相对主控屏幕的方位（上/下/左/右），决定光标移出屏幕时的切换方向——双向生效：主控滑向工作机、工作机滑出外侧缘回主控，均按此方位路由。本机屏幕由运行时自动检测（只读）。改动落盘后**主控会话内即时重新下发布局**（未协商时下个会话握手生效）。",
    ),
    ("layout.local_screens", "本机屏幕（自动检测）"),
    (
        "layout.no_entries",
        "（无条目——运行时用默认拓扑：单工作机屏在主控主屏右侧）",
    ),
    (
        "layout.no_session",
        "（会话未建立——本机屏信息在会话建立后自动检测）",
    ),
    ("layout.ph_anchor", "锚点屏名（空 = 主控主屏）"),
    ("layout.ph_name", "屏幕名（如 sofa / desk）"),
    ("layout.standalone_anchor", "（独立锚点，路由图跳过）"),
    (
        "mapping.add_hint",
        "新增规则：触发键 → 目标键（单键重映射；目标键填 drop 表示丢弃）",
    ),
    (
        "mapping.advanced_hint",
        "单键重映射与丢弃规则；功能键映射之外的个性化面。",
    ),
    ("mapping.advanced_title", "高级 · 自定义规则"),
    ("mapping.col_source", "主控按键"),
    ("mapping.col_target", "本机映射为"),
    ("mapping.empty", "（空）"),
    (
        "mapping.err_action_empty",
        "目标键不能为空（或填 drop 表示丢弃）",
    ),
    (
        "mapping.err_action_modifier",
        "目标键 {k} 是修饰键——单键形态需非修饰键",
    ),
    (
        "mapping.err_action_unknown",
        "目标键 {k} 不是已知语义键名（docs/03 §2 文法）",
    ),
    ("mapping.err_limit", "规则数已达上限 {n}"),
    (
        "mapping.err_stale_index",
        "规则序号已过期（列表已变化），请刷新后重试",
    ),
    ("mapping.err_trigger_empty", "触发键不能为空"),
    (
        "mapping.err_trigger_modifier",
        "触发键 {k} 是修饰键——请使用上方功能键映射（单键形态需非修饰键）",
    ),
    (
        "mapping.err_trigger_unknown",
        "触发键 {k} 不是已知语义键名（docs/03 §2 文法）",
    ),
    (
        "mapping.func_keys_hint",
        "左列为主控侧按下的功能键（左右手均生效），右列选择本机注入的对应键。",
    ),
    ("mapping.func_keys_title", "功能键映射"),
    (
        "mapping.intro",
        "把主控发来的按键改写为本机注入的按键（映射在工作机侧生效）。改动落盘后**即时生效**（运行中会话的映射规则热替换，docs/06 §4.1）。",
    ),
    ("mapping.modifier", "修饰键 {mod}"),
    ("mapping.no_rules", "（无自定义规则）"),
    (
        "mapping.passthrough_hint",
        "直通模式（绕过全部映射，键码原样注入）",
    ),
    ("mapping.ph_action", "目标键（如 Escape）或 drop"),
    ("mapping.ph_preset", "选择预设（自动填充功能键映射）"),
    ("mapping.ph_trigger", "触发键（如 CapsLock / CK_MUTE）"),
    ("mapping.preset", "预设"),
    ("mapping.row_alt", "Alt / Option"),
    ("mapping.row_ctrl", "Ctrl"),
    ("mapping.row_meta", "Command / Win"),
    ("mapping.row_shift", "Shift"),
    ("mapping.row_tab", "Tab"),
    ("mapping.rules_count", "自定义规则（{n}/256）"),
    (
        "mapping.tab_unsupported",
        "Tab 不是修饰键：引擎仅支持把 Tab 重映射为非修饰键（可在高级规则中配置）",
    ),
    ("mapping.target_alt", "Alt"),
    ("mapping.target_ctrl", "Ctrl"),
    ("mapping.target_meta", "Win / Command"),
    ("mapping.target_none", "不映射"),
    ("mapping.target_shift", "Shift"),
    ("mapping.drop", "丢弃"),
    ("menu.autostart", "开机自启"),
    ("menu.candidate_addr_wrap", "（{addr}）"),
    ("menu.disconnect_rescan", "断开并重扫"),
    ("menu.host_candidates", "主控候选"),
    ("menu.no_slaves", "（无在线工作机）"),
    ("menu.open_settings", "打开设置…"),
    ("menu.pause", "暂停共享"),
    ("menu.quit", "退出"),
    ("menu.resume", "恢复共享"),
    ("menu.slave_latency", "{name} · 延迟 {ms}ms"),
    ("menu.slaves_submenu", "在线工作机"),
    ("menu.receive_files", "接收文件：{count} 项 / {size}"),
    ("menu.switch_role", "切换角色：主控 ↔ 工作机"),
    (
        "notice.code_rollback_failed",
        "config 保存失败且存储回滚失败（{error}）：请检查存储后端后重试或改用 file: 引用",
    ),
    (
        "notice.code_rollback_ok",
        "安全码未切换：config 保存失败，已回滚存储后端（存储与配置一致）",
    ),
    ("notice.data_dir_missing", "无法定位数据目录，安全码未写入"),
    (
        "notice.dispatched_no_path",
        "{label}已下发（配置路径不可得，未落盘）",
    ),
    ("notice.logs_clear_failed", "日志清空失败：{error}"),
    (
        "notice.logs_cleared",
        "日志已清空（删除 {n} 个日志文件；页面缓冲已重置）",
    ),
    ("notice.not_saved", "{label}未保存：{error}"),
    ("notice.save_failed", "{label}保存失败：{error}"),
    ("notice.saved", "{label}已保存（生效时点见页内说明）"),
    ("page.about", "关于"),
    ("page.clipboard", "剪贴板同步"),
    ("page.general", "通用设置"),
    ("page.hotkeys", "全局热键"),
    ("page.layout", "屏幕布局"),
    ("page.log", "运行日志"),
    ("page.mapping", "按键映射"),
    ("page.status", "运行状态"),
    (
        "status.candidates_hint",
        "候选主控（多台时点击选择并记住；标注为上次握手失败）",
    ),
    (
        "status.candidate_line",
        "{checked}{name}（{addr}，{source}）",
    ),
    ("status.connect_remember", "连接并记住"),
    ("status.confirmed_reconnect", "已记忆（重新连接）"),
    ("status.role_client", "工作机（Client）"),
    ("status.role_host", "主控（Host）"),
    ("status.row_conn", "连接"),
    ("status.row_events", "事件 出/入"),
    ("status.row_last_error", "最近错误"),
    ("status.row_packets", "报文 出/入"),
    ("status.row_peer", "对端"),
    ("status.row_role", "角色"),
    ("status.row_session", "会话 ID"),
    (
        "status.subtitle",
        "当前角色、连接状态与在线工作机；切换与连接操作在此完成。",
    ),
    (
        "status.runtime_pending",
        "运行时待命中：尚未提供安全码（向导被跳过）。完成向导或修改配置后自动进入运行时。",
    ),
    ("status.slaves_hint", "在线工作机（点击直接切换）"),
    ("status.switch_to", "切换到 {peer}"),
    ("tray.latency_segment", " · 延迟 {ms}ms"),
    ("tray.role_client", "工作机"),
    ("tray.role_host", "主控"),
    ("tray.searching_host", "搜索主控中"),
    ("tray.searching_slaves", "搜索工作机中"),
    ("tray.state_connected", "已连接"),
    ("tray.state_error", "错误"),
    ("tray.state_n_online", "{n} 台在线"),
    ("tray.state_paused", "已暂停"),
    ("win.client", "XControl · 工作机"),
    ("win.host", "XControl · 主控"),
    ("win.loopback", "XControl · 回环演示"),
    ("win.settings", "XControl · 设置"),
    ("win.wizard", "XControl · 首次运行向导"),
    (
        "wizard.allowlist",
        "主控接入白名单（FR-5.4，CIDR，逗号分隔；主控角色生效）",
    ),
    ("wizard.ax_granted", "辅助功能：已授权 ✓"),
    (
        "wizard.ax_not_granted",
        "辅助功能：未授权 ✗（完成向导后可在设置页再次检测）",
    ),
    ("wizard.cidr", "扫描网段（CIDR，逗号分隔；留空自动）"),
    (
        "wizard.code_hint",
        "两端使用同一安全码完成配对（Noise PSK 派生输入）。码不落明文日志；本机默认存入 OS 钥匙串，不可用时回退 0600 文件。",
    ),
    ("wizard.done", "完成并启动"),
    (
        "wizard.done_notice",
        "配置完成：运行时已按所选角色启动（状态页可查连接进展）",
    ),
    (
        "wizard.err_allowlist_invalid",
        "白名单条目需为 CIDR 形态（如 192.168.1.0/24），收到 {raw}",
    ),
    (
        "wizard.err_cidr_invalid",
        "网段需为 CIDR 形态（如 192.168.1.0/24），收到 {raw}",
    ),
    ("wizard.err_code_empty", "安全码不能为空（两端需一致）"),
    (
        "wizard.err_code_empty_back",
        "安全码不能为空（请回到第 2 步填写）",
    ),
    ("wizard.err_code_short", "安全码建议 8 位以上（当前过短）"),
    ("wizard.err_config_write", "配置写入失败：{error}"),
    (
        "wizard.err_data_dir",
        "无法定位数据目录（家目录不可得），安全码无法写入",
    ),
    (
        "wizard.err_no_config_path",
        "配置文件路径不可得，无法完成向导（可用 --config 显式指定）",
    ),
    (
        "wizard.err_port_back",
        "端口需为 1–65535 的整数（请回到第 3 步修正）",
    ),
    (
        "wizard.err_port_invalid",
        "端口需为 1–65535 的整数，收到 {raw}",
    ),
    ("wizard.err_port_range", "端口需为 1–65535 的整数"),
    ("wizard.gen_code", "生成建议"),
    (
        "wizard.network_hint",
        "服务端口与扫描网段。保持默认即可跨机配对；网段留空表示自动取各接口真实前缀（docs/01 §5）。改动在重建运行时后生效。",
    ),
    ("wizard.next", "下一步"),
    ("wizard.open_ax_settings", "打开系统设置 · 辅助功能"),
    (
        "wizard.perm_generic",
        "Windows 需允许应用通过防火墙；Linux 需将用户加入 uinput/input 组。完成步骤可先跳过本页提示。",
    ),
    (
        "wizard.perm_mac_hint",
        "工作机受控与 macOS 主控都需要「辅助功能」权限：工作机用它注入键鼠，macOS 主控用它捕获键鼠——热键识别与键鼠转发都依赖它，未授权的主控切入后无法转发输入。下方为实时检测结果。",
    ),
    (
        "wizard.ph_allowlist",
        "留空 = 接受全部来源；多段用逗号分隔（如 192.168.1.0/24）",
    ),
    (
        "wizard.ph_cidr",
        "留空 = 自动取各接口真实网段；多段用逗号分隔（如 192.168.1.0/24）",
    ),
    ("wizard.ph_code", "输入两端一致的安全码（建议 12 位以上）"),
    ("wizard.ph_port", "服务端口（1–65535）"),
    ("wizard.port", "服务端口"),
    ("wizard.prev", "上一步"),
    (
        "wizard.progress",
        "步骤 {n}/4 · XControl 首次配置（完成后可随时在设置中修改）",
    ),
    ("wizard.recheck", "重新检测"),
    ("wizard.role_client", "工作机（Client）"),
    (
        "wizard.role_hint",
        "主控：接收工作机连接，把键鼠输入转发给焦点工作机。\n工作机：被动等待被发现与连接（新装默认，保守）。",
    ),
    ("wizard.role_host", "主控（Host）"),
    (
        "wizard.role_question",
        "本机在键鼠共享中扮演什么角色？（托盘菜单可随时切换）",
    ),
    ("wizard.skip", "跳过（CLI 用户手动配置）"),
    (
        "wizard.skipped_notice",
        "已跳过向导（无配置时运行时保持待命）",
    ),
    ("wizard.step_code", "第 2 步 · 安全码"),
    ("wizard.step_network", "第 3 步 · 网段默认"),
    ("wizard.step_permissions", "第 4 步 · 权限引导"),
    ("wizard.step_role", "第 1 步 · 角色选择"),
];

/// en 文案表（key, 值；key 集与 zh 全等——parity 测试钉死）。
const EN: &[(&str, &str)] = &[
    ("about.clear_logs", "Clear log files"),
    (
        "about.clear_logs_hint",
        "Delete all rotating log files (today's included) and clear the on-page buffer",
    ),
    ("about.license", "License"),
    (
        "about.license_value",
        "MIT OR Apache-2.0 (gpui / gpui-kit / gpui-tray are Apache-2.0 or MIT as well; dependency licenses are audited in deny.toml)",
    ),
    (
        "about.logs_hint",
        "Log directory (rotating files, 14 days kept automatically)",
    ),
    (
        "about.no_data_dir",
        "(data directory unavailable — rotating file logging not enabled)",
    ),
    ("about.no_logs_dir", "(no log directory to open)"),
    ("about.open_logs", "Open log directory"),
    ("about.subtitle", "Version, license and log maintenance."),
    (
        "about.title",
        "XControl — keyboard & mouse sharing across machines",
    ),
    ("about.version", "Version"),
    (
        "about.version_value",
        "XControl {v} (protocol XCP-VERSION {major}.{minor})",
    ),
    (
        "clipboard.err_not_number",
        "Clipboard limit not saved: {raw} is not a non-negative integer",
    ),
    (
        "clipboard.err_over_limit",
        "Clipboard limit not saved: {v} exceeds the validation limit {n}",
    ),
    ("clipboard.format_image", "Image"),
    ("clipboard.format_text", "Text"),
    (
        "clipboard.formats_hint",
        "Sync formats (changes take effect in the next session)",
    ),
    (
        "clipboard.intro",
        "Content copied on the host is synced to this machine automatically (text/image; formats below). Changes take effect **in the next session**.",
    ),
    (
        "clipboard.max_hint",
        "Size limit (bytes; 0 = disable sync; validation limit {n})",
    ),
    ("clipboard.ph_max_bytes", "bytes (0 = disabled; limit {n})"),
    ("clipboard.scope", "Sync scope"),
    ("clipboard.scope_all", "All clients"),
    ("clipboard.scope_current", "Current controlled client only"),
    ("common.add", "Add"),
    ("common.cancel", "Cancel"),
    ("common.check_suffix", " ✓"),
    (
        "common.code_write_failed",
        "Failed to write the security code: {error}",
    ),
    ("common.confirm", "Confirm"),
    ("common.delete", "Delete"),
    ("common.last_handshake_failed", " · last handshake failed"),
    ("common.none", "None"),
    ("common.save", "Save"),
    (
        "confirm.code_body",
        "The new code ({n} chars) will be written to the local secret store and the config reference updated; paired devices must re-pair with the new code.",
    ),
    ("confirm.code_title", "Change the security code?"),
    (
        "confirm.clear_logs_body",
        "All rotating log files (today's included) will be deleted and the on-page log buffer cleared. Until the next daily rotation, new logs appear on the page/stdout only. This cannot be undone.",
    ),
    ("confirm.clear_logs_title", "Clear logs?"),
    (
        "general.autostart_disable_failed",
        "Failed to disable launch at startup: {error}",
    ),
    ("general.autostart_disabled", "Launch at startup disabled"),
    (
        "general.autostart_enable_failed",
        "Failed to enable launch at startup: {error}",
    ),
    (
        "general.autostart_enabled",
        "Launch at startup enabled (registered with the current binary path)",
    ),
    (
        "general.autostart_hint",
        "Launch at startup (OS-level, takes effect immediately)",
    ),
    (
        "general.autostart_switch",
        "Start XControl automatically at login",
    ),
    ("general.change_btn", "Change…"),
    (
        "general.code_hint",
        "Change security code (destructive, needs confirmation)",
    ),
    (
        "general.code_ref_hint",
        "Current reference: {ref} (keychain references write to the OS keychain; file references write a 0600 file; falls back to file automatically when the keychain is unavailable — docs/07 §3.3. Changing the code disconnects all sessions for re-handshake)",
    ),
    (
        "general.err_code_too_short",
        "Use at least 8 characters for the code (too short, not submitted)",
    ),
    (
        "general.language_hint",
        "Interface language (takes effect immediately)",
    ),
    (
        "general.log_level_hint",
        "Log level (takes effect immediately)",
    ),
    (
        "general.subtitle",
        "App-level settings: interface language, launch at startup, security code and log level.",
    ),
    (
        "general.no_autostart_backend",
        "No autostart backend in this build (platform feature not enabled)",
    ),
    (
        "general.ph_new_code",
        "New code (8+ chars, identical on both ends)",
    ),
    ("general.theme_follow_system", "Follow system"),
    ("general.theme_hint", "Theme (fixed in v1)"),
    ("hotkeys.back_to_host", "Back to host"),
    (
        "hotkeys.intro",
        "Global hotkeys effective on the host: switch target, back to host, pause/resume sharing. Combos look like Ctrl+Alt+Right (modifiers + main key). Hotkeys are registered locally on both ends; a hit is handled locally and never forwarded. Changes take effect **immediately** (the running session hot-swaps the hotkey table; win/x11 platform registrations are rebuilt on change).",
    ),
    ("hotkeys.next", "Next machine"),
    ("hotkeys.pause", "Pause/resume sharing"),
    ("hotkeys.prev", "Previous machine"),
    ("label.clipboard_format", "Clipboard formats"),
    ("label.clipboard_max", "Clipboard size limit"),
    ("label.clipboard_scope", "Clipboard scope"),
    ("label.code", "Security code"),
    ("label.confirmed_host", "Remembered host"),
    ("label.hotkeys", "Hotkeys"),
    ("label.language", "Language"),
    ("label.layout_add", "Add layout entry"),
    ("label.layout_delete", "Delete layout entry"),
    ("label.log_level", "Log level"),
    ("label.mapping_preset", "Mapping preset"),
    ("label.passthrough", "Passthrough mode"),
    ("label.rule_add", "Add rule"),
    ("label.rule_delete", "Delete rule"),
    (
        "log.subtitle",
        "Application logs (scroll to browse; the page buffer keeps the most recent 500 lines, files live on the About page). New lines auto-follow while you are at the bottom; scrolling up to read history is never interrupted.",
    ),
    (
        "layout.add_hint",
        "New entry (direction relative to the anchor screen; the anchor can be a local screen name or an existing entry name)",
    ),
    ("layout.default_anchor", "host primary screen"),
    ("layout.dir_down", "Below"),
    ("layout.dir_left", "Left"),
    ("layout.dir_right", "Right"),
    ("layout.dir_up", "Above"),
    ("layout.direction", "Direction"),
    ("layout.entries", "Client screen entries ({n})"),
    ("layout.entry_summary", "{id}: {anchor} {dir}"),
    (
        "layout.err_duplicate",
        "Screen name {id} already exists (entries must be unique, FR-1.3)",
    ),
    ("layout.err_name_empty", "Screen name cannot be empty"),
    ("layout.err_not_saved", "Layout entry not saved: {error}"),
    (
        "layout.err_stale_index",
        "Entry index is stale (the list changed); refresh and try again",
    ),
    (
        "layout.intro",
        "Define where each client screen sits relative to the host screens (above/below/left/right); this decides the cursor switch direction — in both directions: host to client at the shared edge, and client back to host across the client's outer edge. Local screens are auto-detected by the runtime (read-only). Changes are saved and **re-published immediately within a host session** (on the next handshake if not negotiated).",
    ),
    ("layout.local_screens", "Local screens (auto-detected)"),
    (
        "layout.no_entries",
        "(no entries — the runtime uses the default topology: one client screen to the right of the host primary screen)",
    ),
    (
        "layout.no_session",
        "(no session — local screens are detected after a session is established)",
    ),
    (
        "layout.ph_anchor",
        "Anchor screen name (empty = host primary screen)",
    ),
    ("layout.ph_name", "Screen name (e.g. sofa / desk)"),
    (
        "layout.standalone_anchor",
        "(standalone anchor, skipped by the routing graph)",
    ),
    (
        "mapping.add_hint",
        "New rule: trigger key → target key (single-key remap; use drop as the target to discard)",
    ),
    (
        "mapping.advanced_hint",
        "Single-key remaps and drop rules; personalization beyond the functional-key mapping.",
    ),
    ("mapping.advanced_title", "Advanced · custom rules"),
    ("mapping.col_source", "Key on host"),
    ("mapping.col_target", "Mapped to (this machine)"),
    ("mapping.empty", "(empty)"),
    (
        "mapping.err_action_empty",
        "Target key cannot be empty (or use drop to discard)",
    ),
    (
        "mapping.err_action_modifier",
        "Target key {k} is a modifier — the single-key form needs a non-modifier key",
    ),
    (
        "mapping.err_action_unknown",
        "Target key {k} is not a known semantic key name (docs/03 §2 grammar)",
    ),
    ("mapping.err_limit", "Rule limit reached ({n})"),
    (
        "mapping.err_stale_index",
        "Rule index is stale (the list changed); refresh and try again",
    ),
    ("mapping.err_trigger_empty", "Trigger key cannot be empty"),
    (
        "mapping.err_trigger_modifier",
        "Trigger key {k} is a modifier — use the functional-key mapping above (the single-key form needs a non-modifier key)",
    ),
    (
        "mapping.err_trigger_unknown",
        "Trigger key {k} is not a known semantic key name (docs/03 §2 grammar)",
    ),
    (
        "mapping.func_keys_hint",
        "The left column is the functional key pressed on the host (both hands), the right column picks the key injected on this machine.",
    ),
    ("mapping.func_keys_title", "Functional-key mapping"),
    (
        "mapping.intro",
        "Rewrite keys arriving from the host into the keys injected on this machine (mapping applies on the client side). Changes take effect **immediately** (the running session hot-swaps mapping rules; see docs/06 §4.1).",
    ),
    ("mapping.modifier", "modifier {mod}"),
    ("mapping.no_rules", "(no custom rules)"),
    (
        "mapping.passthrough_hint",
        "Passthrough mode (bypasses all mappings; key codes are injected as-is)",
    ),
    ("mapping.ph_action", "Target key (e.g. Escape) or drop"),
    (
        "mapping.ph_preset",
        "Choose a preset (fills the mapping below)",
    ),
    (
        "mapping.ph_trigger",
        "Trigger key (e.g. CapsLock / CK_MUTE)",
    ),
    ("mapping.preset", "Preset"),
    ("mapping.row_alt", "Alt / Option"),
    ("mapping.row_ctrl", "Ctrl"),
    ("mapping.row_meta", "Command / Win"),
    ("mapping.row_shift", "Shift"),
    ("mapping.row_tab", "Tab"),
    ("mapping.rules_count", "Custom rules ({n}/256)"),
    (
        "mapping.tab_unsupported",
        "Tab is not a modifier: the engine only supports remapping Tab to a non-modifier key (configure it in Advanced rules)",
    ),
    ("mapping.target_alt", "Alt"),
    ("mapping.target_ctrl", "Ctrl"),
    ("mapping.target_meta", "Win / Command"),
    ("mapping.target_none", "No mapping"),
    ("mapping.target_shift", "Shift"),
    ("mapping.drop", "drop"),
    ("menu.autostart", "Launch at startup"),
    ("menu.candidate_addr_wrap", " ({addr})"),
    ("menu.disconnect_rescan", "Disconnect and rescan"),
    ("menu.host_candidates", "Host candidates"),
    ("menu.no_slaves", "(no clients online)"),
    ("menu.open_settings", "Open Settings…"),
    ("menu.pause", "Pause sharing"),
    ("menu.quit", "Quit"),
    ("menu.resume", "Resume sharing"),
    ("menu.slave_latency", "{name} · {ms}ms latency"),
    ("menu.slaves_submenu", "Online clients"),
    (
        "menu.receive_files",
        "Receive files: {count} items / {size}",
    ),
    ("menu.switch_role", "Switch role: host ↔ client"),
    (
        "notice.code_rollback_failed",
        "Config save failed and the store rollback failed ({error}): check the store backend and retry, or switch to a file: reference",
    ),
    (
        "notice.code_rollback_ok",
        "Security code unchanged: config save failed, the store has been rolled back (store and config are consistent)",
    ),
    (
        "notice.data_dir_missing",
        "Cannot locate the data directory; the security code was not written",
    ),
    (
        "notice.dispatched_no_path",
        "{label} dispatched (config path unavailable, not persisted)",
    ),
    ("notice.logs_clear_failed", "Failed to clear logs: {error}"),
    (
        "notice.logs_cleared",
        "Logs cleared ({n} files deleted; the on-page buffer was reset)",
    ),
    ("notice.not_saved", "{label} not saved: {error}"),
    ("notice.save_failed", "{label} failed to save: {error}"),
    (
        "notice.saved",
        "{label} saved (see in-page notes for when it takes effect)",
    ),
    ("page.about", "About"),
    ("page.clipboard", "Clipboard Sync"),
    ("page.general", "General Settings"),
    ("page.hotkeys", "Global Hotkeys"),
    ("page.layout", "Screen Layout"),
    ("page.log", "Logs"),
    ("page.mapping", "Key Mapping"),
    ("page.status", "Status"),
    (
        "status.candidates_hint",
        "Host candidates (click to choose and remember when several; flagged entries failed the last handshake)",
    ),
    (
        "status.candidate_line",
        "{checked}{name} ({addr}, {source})",
    ),
    ("status.connect_remember", "Connect and remember"),
    ("status.confirmed_reconnect", "Remembered (reconnect)"),
    ("status.role_client", "Client"),
    ("status.role_host", "Host"),
    ("status.row_conn", "Connection"),
    ("status.row_events", "Events out/in"),
    ("status.row_last_error", "Last error"),
    ("status.row_packets", "Packets out/in"),
    ("status.row_peer", "Peer"),
    ("status.row_role", "Role"),
    ("status.row_session", "Session ID"),
    (
        "status.subtitle",
        "Current role, connection status and online clients; switching and connection happen here.",
    ),
    (
        "status.runtime_pending",
        "Runtime standing by: no security code provided yet (the setup wizard was skipped). It starts automatically after you complete the wizard or change settings.",
    ),
    ("status.slaves_hint", "Online clients (click to switch)"),
    ("status.switch_to", "Switch to {peer}"),
    ("tray.latency_segment", " · latency {ms}ms"),
    ("tray.role_client", "Client"),
    ("tray.role_host", "Host"),
    ("tray.searching_host", "Searching for host"),
    ("tray.searching_slaves", "Searching for clients"),
    ("tray.state_connected", "Connected"),
    ("tray.state_error", "Error"),
    ("tray.state_n_online", "{n} online"),
    ("tray.state_paused", "Paused"),
    ("win.client", "XControl · Client"),
    ("win.host", "XControl · Host"),
    ("win.loopback", "XControl · Loopback Demo"),
    ("win.settings", "XControl · Settings"),
    ("win.wizard", "XControl · First-Run Setup"),
    (
        "wizard.allowlist",
        "Host access allowlist (FR-5.4, CIDR, comma-separated; applies in host role)",
    ),
    ("wizard.ax_granted", "Accessibility: granted ✓"),
    (
        "wizard.ax_not_granted",
        "Accessibility: not granted ✗ (re-check on the Settings page after finishing the wizard)",
    ),
    (
        "wizard.cidr",
        "Scan ranges (CIDR, comma-separated; empty = automatic)",
    ),
    (
        "wizard.code_hint",
        "Both ends pair with the same security code (Noise PSK derivation input). The code never appears in plain-text logs; locally it is stored in the OS keychain by default, with a 0600 file as fallback.",
    ),
    ("wizard.done", "Finish and start"),
    (
        "wizard.done_notice",
        "Setup complete: the runtime has started with the chosen role (check connection progress on the Status page)",
    ),
    (
        "wizard.err_allowlist_invalid",
        "Allowlist entries must be CIDR notation (e.g. 192.168.1.0/24), got {raw}",
    ),
    (
        "wizard.err_cidr_invalid",
        "Ranges must be CIDR notation (e.g. 192.168.1.0/24), got {raw}",
    ),
    (
        "wizard.err_code_empty",
        "The security code cannot be empty (must match on both ends)",
    ),
    (
        "wizard.err_code_empty_back",
        "The security code cannot be empty (go back to step 2)",
    ),
    (
        "wizard.err_code_short",
        "Use at least 8 characters for the code (currently too short)",
    ),
    (
        "wizard.err_config_write",
        "Failed to write the config: {error}",
    ),
    (
        "wizard.err_data_dir",
        "Cannot locate the data directory (home dir unavailable); the security code cannot be written",
    ),
    (
        "wizard.err_no_config_path",
        "Config file path unavailable; cannot finish the wizard (use --config to specify it explicitly)",
    ),
    (
        "wizard.err_port_back",
        "The port must be an integer in 1–65535 (go back to step 3 to fix it)",
    ),
    (
        "wizard.err_port_invalid",
        "The port must be an integer in 1–65535, got {raw}",
    ),
    (
        "wizard.err_port_range",
        "The port must be an integer in 1–65535",
    ),
    ("wizard.gen_code", "Suggest a code"),
    (
        "wizard.network_hint",
        "Service port and scan ranges. The defaults are enough to pair across machines; an empty range list means each interface's real prefix is used automatically (docs/01 §5). Changes take effect when the runtime is rebuilt.",
    ),
    ("wizard.next", "Next"),
    (
        "wizard.open_ax_settings",
        "Open System Settings · Accessibility",
    ),
    (
        "wizard.perm_generic",
        "On Windows, allow the app through the firewall; on Linux, add your user to the uinput/input group. You can skip this page for now.",
    ),
    (
        "wizard.perm_mac_hint",
        "Both the client and the macOS host need the Accessibility permission: the client uses it to inject keyboard & mouse, the macOS host uses it to capture them. Hotkey recognition and capture-forwarding both rely on it, and an unauthorized host cannot forward input after switching in. The status below is detected live.",
    ),
    (
        "wizard.ph_allowlist",
        "Empty = accept all sources; separate ranges with commas (e.g. 192.168.1.0/24)",
    ),
    (
        "wizard.ph_cidr",
        "Empty = use each interface's real prefix; separate ranges with commas (e.g. 192.168.1.0/24)",
    ),
    (
        "wizard.ph_code",
        "Enter the code (identical on both ends, 12+ chars recommended)",
    ),
    ("wizard.ph_port", "Service port (1–65535)"),
    ("wizard.port", "Service port"),
    ("wizard.prev", "Back"),
    (
        "wizard.progress",
        "Step {n}/4 · XControl first-time setup (everything can be changed later in Settings)",
    ),
    ("wizard.recheck", "Re-check"),
    ("wizard.role_client", "Client"),
    (
        "wizard.role_hint",
        "Host: accepts client connections and forwards keyboard/mouse input to the focused client.\nClient: passively waits to be discovered and connected (conservative default for new installs).",
    ),
    ("wizard.role_host", "Host"),
    (
        "wizard.role_question",
        "What role does this machine play in keyboard & mouse sharing? (switchable any time from the tray menu)",
    ),
    ("wizard.skip", "Skip (configure manually for CLI use)"),
    (
        "wizard.skipped_notice",
        "Setup wizard skipped (the runtime stands by when no config exists)",
    ),
    ("wizard.step_code", "Step 2 · Security code"),
    ("wizard.step_network", "Step 3 · Network defaults"),
    ("wizard.step_permissions", "Step 4 · Permissions"),
    ("wizard.step_role", "Step 1 · Choose a role"),
];

/// 提取 `{name}` 占位符集合（parity 测试用；非贪婪匹配到第一个 `}`）。
#[cfg(test)]
fn placeholders(value: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        found.push(&rest[start + 1..start + end]);
        rest = &rest[start + end + 1..];
    }
    found
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// 语言敏感测试的串行锁：`CURRENT_LANG` 是进程全局（tr/trf 读当前值），
    /// 测试并行线程会互相翻语言——依赖当前语言的断言必须持锁串行
    /// （parity/lang_config_round_trip 用 tr_with 显式语言，不需要锁）。
    static LANG_LOCK: Mutex<()> = Mutex::new(());

    /// 持串行锁（毒锁恢复同快照纪律）。
    fn lang_lock() -> MutexGuard<'static, ()> {
        LANG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 两表 parity：同 key 集、各自无重复 key、占位符集合逐 key 一致。
    #[test]
    fn parity() {
        for &(key, zh_value) in ZH {
            let en_value = tr_with(key, Lang::En);
            assert_ne!(en_value, key, "en 表缺 key：{key}");
            assert!(!en_value.is_empty(), "en 值非空：{key}");
            let mut zh_ph = placeholders(zh_value);
            let mut en_ph = placeholders(en_value);
            zh_ph.sort_unstable();
            en_ph.sort_unstable();
            assert_eq!(zh_ph, en_ph, "占位符集合不一致：{key}");
        }
        for &(key, _) in EN {
            let zh_value = tr_with(key, Lang::ZhCn);
            assert_ne!(zh_value, key, "zh 表缺 key：{key}");
            assert!(!zh_value.is_empty(), "zh 值非空：{key}");
        }
        // 各表内无重复 key。
        for (table, name) in [(ZH, "zh"), (EN, "en")] {
            let mut keys: Vec<&str> = table.iter().map(|&(key, _)| key).collect();
            keys.sort_unstable();
            let count = keys.len();
            keys.dedup();
            assert_eq!(keys.len(), count, "{name} 表存在重复 key");
        }
    }

    /// tr 基础语义：默认 zh、显式 en、缺 key 回退 key 本身。
    #[test]
    fn tr_basics() {
        let _guard = lang_lock();
        set_language(Lang::ZhCn);
        assert_eq!(tr("menu.pause"), "暂停共享");
        assert_eq!(tr_with("menu.pause", Lang::En), "Pause sharing");
        assert_eq!(tr_with("no.such.key", Lang::ZhCn), "no.such.key");
        assert_eq!(tr_with("no.such.key", Lang::En), "no.such.key");
    }

    /// 语言切换全局生效（tr 读当前值）。
    #[test]
    fn language_switch_is_global() {
        let _guard = lang_lock();
        set_language(Lang::ZhCn);
        assert_eq!(current(), Lang::ZhCn);
        assert_eq!(tr("menu.quit"), "退出");
        set_language(Lang::En);
        assert_eq!(current(), Lang::En);
        assert_eq!(tr("menu.quit"), "Quit");
        set_language(Lang::ZhCn);
        assert_eq!(tr("menu.quit"), "退出");
    }

    /// trf 占位符替换：全部替换、无残留花括号、未知占位符原样保留。
    #[test]
    fn trf_substitutes_placeholders() {
        let _guard = lang_lock();
        set_language(Lang::ZhCn);
        let text = trf("tray.state_n_online", &[("n", "3")]);
        assert_eq!(text, "3 台在线");
        let text = trf(
            "about.version_value",
            &[("v", "0.1.0"), ("major", "1"), ("minor", "1")],
        );
        assert_eq!(text, "XControl 0.1.0（协议 XCP-VERSION 1.1）");
        // 未知占位符名：不替换（调用方拼写错误可见）。
        let text = trf("tray.state_n_online", &[("count", "3")]);
        assert_eq!(text, "{n} 台在线");
    }

    /// Lang 与 config 字面值往返（`[general].language`）。
    #[test]
    fn lang_config_round_trip() {
        for lang in [Lang::ZhCn, Lang::En] {
            assert_eq!(Lang::parse(lang.as_config_str()), Some(lang));
        }
        assert_eq!(Lang::parse("fr"), None);
        assert_eq!(Lang::parse("zh_CN"), None);
        assert_eq!(Lang::parse(" zh-CN "), Some(Lang::ZhCn), "容忍两端空白");
        assert_eq!(Lang::default(), Lang::ZhCn, "zh-CN 为默认语言");
    }
}
