//! 配置模型：docs/01 §5 的 `config.toml` schema（顶层 `version` + 各配置节）。
//!
//! 命名约定（`.trellis/spec/rust-core/architecture-rules.md`）：配置节类型 `XxConfig`。
//! 所有节/字段都有 serde 默认值：旧文件缺节可解析（语义补齐在 [`crate::migrate`] 迁移链中）。
//! 默认值集中定义在本模块常量；`Default` 一律手写（禁止 derive 出"端口 0/热键为空"类无效默认）。

use serde::{Deserialize, Serialize};

use crate::migrate::CURRENT_CONFIG_VERSION;
use crate::secret::DEFAULT_SECURITY_CODE_REF;

/// 默认监听端口（docs/02 §0：IANA 核查通过，避开 Synergy/lan-mouse/KDE Connect）。
pub const DEFAULT_PORT: u16 = 4243;

/// 剪贴板默认大小上限：16 MiB（docs/00 FR-4.5：超限不同步并托盘提示）。
pub const DEFAULT_CLIPBOARD_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// 剪贴板大小校验上限：64 MiB（默认值的 4 倍，防 DoS 配置失误的兜底；
/// 协议侧另有逐帧限额与 10s 超时兜底，docs/02 §6）。
pub const MAX_CLIPBOARD_BYTES_LIMIT: u64 = 64 * 1024 * 1024;

/// keymap 自定义规则数量上限（PRD：rules ≤ 256）。
pub const KEYMAP_MAX_RULES: usize = 256;

/// 界面语言默认值（FR-6.5）：zh-CN（文案表 zh-CN 完整 + en 全量，docs/06 §7）。
/// 合法值集 = `zh-CN` | `en`（校验层 `LANGUAGE_VARIANTS`；解析语义见
/// `xcontrol-i18n::Lang::parse`）。
pub const DEFAULT_LANGUAGE: &str = "zh-CN";

/// 界面语言合法字面值（校验提示用）。
pub const LANGUAGE_VARIANTS: [&str; 2] = ["zh-CN", "en"];

/// 内置 keymap 预设（docs/03 §4.4）。`none` = 恒等直通。
pub const KEYMAP_PRESETS: [&str; 8] = [
    "mac2win",
    "mac2linux",
    "win2mac",
    "linux2mac",
    "win2linux",
    "linux2win",
    "mac2mac",
    "none",
];

// 默认热键（docs/03 §4.6：两端本地注册，命中即本地处理、不转发）。
/// 切到下一台（`Ctrl+Alt+→`）。
pub const DEFAULT_HOTKEY_NEXT: &str = "Ctrl+Alt+Right";
/// 切到上一台（`Ctrl+Alt+←`）。
pub const DEFAULT_HOTKEY_PREV: &str = "Ctrl+Alt+Left";
/// 回主控（`Ctrl+Alt+Home`）。
pub const DEFAULT_HOTKEY_BACK_TO_HOST: &str = "Ctrl+Alt+Home";
/// 暂停/恢复（`Ctrl+Alt+P`）。
pub const DEFAULT_HOTKEY_PAUSE: &str = "Ctrl+Alt+P";

/// 日志级别（NFR-8，docs/01 §5 `[general].log_level`）。
///
/// 五值枚举（serde 小写字面值）：未知值在解析期即被拒绝（与其余枚举同纪律，
/// 见 `unknown_enum_values_are_rejected` 测试）；`parse`/`as_str` 供 CLI
/// `--log-level` 覆盖参数与 EnvFilter 指令组装复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// 仅错误。
    Error,
    /// 警告及以上。
    Warn,
    /// 信息及以上（默认）。
    Info,
    /// 调试（含子系统诊断）。
    Debug,
    /// 追踪（全量，量最大）。
    Trace,
}

impl LogLevel {
    /// 全部合法字面值（校验提示与 CLI 错误文案用，docs/01 §5）。
    pub const VARIANTS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

    /// 解析字面值（CLI `--log-level` 覆盖参数用）；未知值 `None`，由调用方报错。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// 字面值（EnvFilter 全局指令与 `--status` 显示用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// `[general]` 配置节（docs/01 §5，v2 起；M3 NFR-8）。language 为 String
/// （FR-6.5，M6）故不再 derive Copy——字段级读取/赋值不受影响。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// 日志级别（NFR-8）：[`LogLevel`] 五值，默认 info。
    #[serde(default = "default_log_level")]
    pub log_level: LogLevel,
    /// 界面语言（FR-6.5，M6）：`zh-CN`（默认）| `en`。文案表 key 化见 docs/06 §7；
    /// 切换经设置页落盘 → `RuntimeConfigUpdate` 即时生效（与 log_level 同消费点）。
    #[serde(default = "default_language")]
    pub language: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            language: default_language(),
        }
    }
}

fn default_log_level() -> LogLevel {
    LogLevel::Info
}

fn default_language() -> String {
    DEFAULT_LANGUAGE.to_owned()
}

/// 顶层配置（`config.toml`，docs/01 §5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// schema 版本：读入时缺失视为 0（无版本号的旧格式），由迁移链逐步升级到
    /// [`CURRENT_CONFIG_VERSION`]；保存时必须等于当前版本（见 [`crate::io`]）。
    #[serde(default)]
    pub version: u32,
    /// 运行角色（FR-1.1）。默认从机：新装进程不主动控制他人（保守默认）。
    #[serde(default)]
    pub role: Role,
    /// 安全码引用（docs/07 §3.3）：`keychain:<条目名>`（默认）或 `file:<路径>`（回退）。
    #[serde(default = "default_security_code_ref")]
    pub security_code_ref: String,
    /// 通用配置节（v2 起，docs/01 §5；NFR-8 日志级别）。
    #[serde(default)]
    pub general: GeneralConfig,
    /// 网络配置节。
    #[serde(default)]
    pub net: NetConfig,
    /// 屏幕布局节（FR-1.3 最小模型；邻接图重建在 xcontrol-core::layout）。
    #[serde(default)]
    pub layout: LayoutConfig,
    /// 按键映射节（docs/03 §4）。
    #[serde(default)]
    pub keymap: KeymapConfig,
    /// 全局热键节（docs/03 §4.6，FR-2.5）。
    #[serde(default)]
    pub hotkeys: HotkeysConfig,
    /// 剪贴板同步节（docs/04，FR-4.4/4.5）。
    #[serde(default)]
    pub clipboard: ClipboardConfig,
}

impl Default for Config {
    /// 默认配置：当前 schema 版本 + 保守默认值（从机角色、4243 端口、
    /// mdns+beacon 发现、恒等 keymap、docs/03 §4.6 热键、16 MiB 剪贴板、
    /// 日志级别 info）。
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            role: Role::default(),
            security_code_ref: DEFAULT_SECURITY_CODE_REF.to_owned(),
            general: GeneralConfig::default(),
            net: NetConfig::default(),
            layout: LayoutConfig::default(),
            keymap: KeymapConfig::default(),
            hotkeys: HotkeysConfig::default(),
            clipboard: ClipboardConfig::default(),
        }
    }
}

/// 运行角色（FR-1.1）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 主控：接收从机连接，向焦点从机转发输入。
    Host,
    /// 从机（默认）：被动等待被发现/连接。
    #[default]
    Client,
}

/// `[net]` 配置节（docs/01 §5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetConfig {
    /// 监听/连接端口（TCP×2 与 UDP 发现共用号段，docs/02 §0）。校验：1–65535。
    #[serde(default = "default_port")]
    pub port: u16,
    /// 网段列表（CIDR）；空 = 自动取各接口真实前缀（docs/01 §5）。
    #[serde(default)]
    pub cidrs: Vec<String>,
    /// 发现机制列表（docs/02 §1 三层发现）。
    #[serde(default = "default_discovery")]
    pub discovery: Vec<DiscoveryMethod>,
    /// 主控接入白名单（FR-5.4，v3 内追加字段）：CIDR 数组；serve 层 accept 后、
    /// 握手**前**逐条匹配来源 IP（[`crate::model`] docs/07 §6）。空 = 不启用
    /// （接受全部来源）。非法条目由校验拒绝；IPv6 条目合法但当前 serve 层仅
    /// 匹配 IPv4（beacon/发现同面），见 docs/07 §6。
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// 已记忆主控（FR-5.5，v3 内追加字段）：instance id 的 32 位小写 hex。
    /// `Some` = 用户已确认过该主控，发现循环优先直连；`None` = 未确认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_host: Option<String>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            cidrs: Vec::new(),
            discovery: default_discovery(),
            allowlist: Vec::new(),
            confirmed_host: None,
        }
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_security_code_ref() -> String {
    DEFAULT_SECURITY_CODE_REF.to_owned()
}

fn default_discovery() -> Vec<DiscoveryMethod> {
    vec![DiscoveryMethod::Mdns, DiscoveryMethod::Beacon]
}

/// 发现机制开关项（docs/02 §1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscoveryMethod {
    /// mDNS（`_xcontrol._tcp.local.`，docs/02 §1）。
    #[serde(rename = "mdns")]
    Mdns,
    /// UDP beacon 广播（docs/02 §1）。
    #[serde(rename = "beacon")]
    Beacon,
    /// 主动 TCP connect 扫描（docs/02 §1，默认关）。
    #[serde(rename = "scan")]
    Scan,
    /// 显式关闭主动扫描的兼容写法（docs/01 §5 示例；与 [`DiscoveryMethod::Scan`]
    /// 同时出现由校验拒绝）。
    #[serde(rename = "scan-off")]
    ScanOff,
}

/// `[layout]` 配置节：屏幕邻接网格（FR-1.3）。
///
/// 最小模型（本任务范围）：每个条目描述一台从机屏幕相对另一屏幕的方向；
/// 主控屏是网格原点、不在此列。完整邻接表由 xcontrol-core::layout 重建（docs/01 §3.2）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LayoutConfig {
    /// 屏幕条目。
    #[serde(default)]
    pub screens: Vec<LayoutScreen>,
}

/// 单个屏幕条目（FR-1.3：相对某屏幕的上/下/左/右形成网格）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutScreen {
    /// 屏幕/从机标识（非空、条目间唯一）。
    pub id: String,
    /// 相对哪个屏幕；`None`（有 direction 时）= 锚定主控主屏（11-19-m5-layout
    /// 方位制最小模型）；两者皆缺 = 独立锚点（预留，路由图跳过）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_to: Option<String>,
    /// 相对方向；与 `relative_to` 缺一才合法（仅 `relative_to` 无 `direction` 校验拒绝）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
}

/// 相对方向（FR-1.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// 目标屏幕在本屏上方。
    Up,
    /// 目标屏幕在本屏下方。
    Down,
    /// 目标屏幕在本屏左侧。
    Left,
    /// 目标屏幕在本屏右侧。
    Right,
}

/// `[keymap]` 配置节（docs/03 §4：预设 + 自定义规则叠加，先命中预设则不查自定义）。
///
/// v3 起（docs/01 §5 修订）：新增 [`KeymapConfig::passthrough`] 直通开关（FR-3.6）；
/// `rules` 升级为 docs/03 §4.1 的 trigger/action 四形态模型（v2 的 `from`/`to`
/// 简式由迁移链自动转换，见 [`crate::migrate`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapConfig {
    /// 内置预设名（[`KEYMAP_PRESETS`]，docs/03 §4.4）。
    #[serde(default = "default_preset")]
    pub preset: String,
    /// FR-3.6 直通模式：true 时引擎绕过全部映射规则（键码原样注入；托盘/热键
    /// 一键切换的落点）。默认 false。
    #[serde(default)]
    pub passthrough: bool,
    /// 自定义规则（数量上限 [`KEYMAP_MAX_RULES`]）。
    #[serde(default)]
    pub rules: Vec<KeymapRule>,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            preset: default_preset(),
            passthrough: false,
            rules: Vec::new(),
        }
    }
}

fn default_preset() -> String {
    "none".to_owned()
}

/// 单条映射规则（docs/03 §4.1 四形态，v3 起）：
///
/// ```toml
/// # 1) 单键重映射
/// [[keymap.rules]]
/// trigger.key = "CapsLock"
/// action.key  = "Escape"
///
/// # 2) 修饰键映射（事件级，docs/03 §4.2）
/// [[keymap.rules]]
/// trigger.mod = "LeftMeta"
/// action.mod  = "LeftCtrl"
///
/// # 3) 组合级改写（docs/03 §4.3）
/// [[keymap.rules]]
/// trigger = { mods = ["LeftMeta"], key = "C" }
/// action  = { mods = ["LeftCtrl", "LeftShift"], key = "C" }
///
/// # 4) 特殊动作：丢弃
/// [[keymap.rules]]
/// trigger = { mods = ["LeftMeta", "LeftShift"], key = "Digit3" }
/// action.drop = true
/// ```
///
/// 形态合法性由 [`crate::validate`] 在加载期按字段路径报错；core 引擎编译
/// （`MappingEngine::new`）做同一套二次判定（防绕过校验的构造路径）。
///
/// `from`/`to` 为 **v2 遗留字段**（pub(crate)：仅迁移链可见）：v2 文件经 serde
/// 解析进这两个字段，[`crate::migrate::migrate_v2_to_v3`] 转换为 trigger/action
/// 形态后清空；v3 文件手写 `from`/`to` 会被校验拒绝（提示改用新形态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapRule {
    /// 触发条件（按下什么）。
    #[serde(default)]
    pub trigger: RuleTrigger,
    /// 注入动作（产出什么）。
    #[serde(default)]
    pub action: RuleAction,
    /// v2 遗留：源语义键名（迁移前形态；迁移后为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) from: Option<String>,
    /// v2 遗留：目标语义键名；`None` = 丢弃（迁移前形态；迁移后为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) to: Option<String>,
}

impl KeymapRule {
    /// 程序化构造 v3 形态规则（`from`/`to` 遗留字段恒为 `None`——那是迁移链
    /// 专用的解析期通道，见字段文档）。core 引擎测试与未来的设置 UI 规则
    /// 编辑走这里，不走裸结构体字面量（遗留字段对外不可见）。
    #[must_use]
    pub fn new(trigger: RuleTrigger, action: RuleAction) -> Self {
        Self {
            trigger,
            action,
            from: None,
            to: None,
        }
    }
}

/// 规则触发条件（docs/03 §4.1：单键 / 修饰键 / 组合三种 trigger 形态）。
///
/// 字段全部可选以镜像 TOML 形态；**合法组合**由 [`crate::validate`] 判定：
/// `mod_` 独占（形态 2）；`key` 可与 `mods` 同现（形态 3/4）或独占（形态 1）；
/// `mods` 无 `key`、多字段混用、全空均非法。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleTrigger {
    /// 单键/组合级 trigger 主键（语义键名，非修饰键）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 事件级修饰键映射 trigger（形态 2；语义键名，必须是修饰键）。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "mod")]
    pub mod_: Option<String>,
    /// 组合级 trigger 修饰键集合（**精确集合匹配**，docs/03 §4.1；全为修饰键、不重复、非空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<Vec<String>>,
}

/// 规则注入动作（docs/03 §4.1：单键 / 修饰键 / 组合 / drop 四种 action 形态）。
///
/// 合法组合：`drop = true` 独占（形态 4）；`mod_` 独占（形态 2）；`key` 可与
/// `mods` 同现（形态 3）或独占（形态 1）；其余混用/全空非法。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleAction {
    /// 注入目标键（语义键名，非修饰键；组合级可与 `mods` 同现）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 事件级修饰键映射目标（形态 2；必须是修饰键）。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "mod")]
    pub mod_: Option<String>,
    /// 组合级 action 修饰键集合（全为修饰键、不重复、非空；弦注入 docs/03 §4.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<Vec<String>>,
    /// 丢弃该事件（形态 4）：只接受 `true`；省略 = 不丢弃。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop: Option<bool>,
}

impl RuleTrigger {
    /// 是否三字段全空（v2 遗留规则迁移判定用）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key.is_none() && self.mod_.is_none() && self.mods.is_none()
    }
}

impl RuleAction {
    /// 是否四字段全空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key.is_none() && self.mod_.is_none() && self.mods.is_none() && self.drop.is_none()
    }
}

/// `[hotkeys]` 配置节（docs/03 §4.6，FR-2.5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeysConfig {
    /// 切到下一台（默认 [`DEFAULT_HOTKEY_NEXT`]）。
    #[serde(default = "default_hotkey_next")]
    pub next: String,
    /// 切到上一台（默认 [`DEFAULT_HOTKEY_PREV`]）。
    #[serde(default = "default_hotkey_prev")]
    pub prev: String,
    /// 回主控（默认 [`DEFAULT_HOTKEY_BACK_TO_HOST`]）。
    #[serde(default = "default_hotkey_back_to_host")]
    pub back_to_host: String,
    /// 暂停/恢复（默认 [`DEFAULT_HOTKEY_PAUSE`]）。
    #[serde(default = "default_hotkey_pause")]
    pub pause: String,
    /// Wayland 手势回退开关（docs/05 §5 优先级 3、docs/06 §4"Wayland 回退手势
    /// 开关"）：GlobalShortcuts portal 不可用时的边缘双击/角落悬停检测。
    /// 默认关——portal 热键可用时手势是多余触发面（误触风险）；11-19-m5-wayland。
    #[serde(default)]
    pub gesture_fallback: bool,
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self {
            next: DEFAULT_HOTKEY_NEXT.to_owned(),
            prev: DEFAULT_HOTKEY_PREV.to_owned(),
            back_to_host: DEFAULT_HOTKEY_BACK_TO_HOST.to_owned(),
            pause: DEFAULT_HOTKEY_PAUSE.to_owned(),
            gesture_fallback: false,
        }
    }
}

fn default_hotkey_next() -> String {
    DEFAULT_HOTKEY_NEXT.to_owned()
}

fn default_hotkey_prev() -> String {
    DEFAULT_HOTKEY_PREV.to_owned()
}

fn default_hotkey_back_to_host() -> String {
    DEFAULT_HOTKEY_BACK_TO_HOST.to_owned()
}

fn default_hotkey_pause() -> String {
    DEFAULT_HOTKEY_PAUSE.to_owned()
}

/// `[clipboard]` 配置节（docs/04，FR-4.4/4.5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardConfig {
    /// 同步范围（FR-4.4）。
    #[serde(default)]
    pub scope: ClipboardScope,
    /// 大小上限（字节，FR-4.5 默认 [`DEFAULT_CLIPBOARD_MAX_BYTES`]）；
    /// 0 = 禁用剪贴板同步。校验上限 [`MAX_CLIPBOARD_BYTES_LIMIT`]。
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// 同步格式；空列表 = 禁用同步。
    #[serde(default = "default_formats")]
    pub formats: Vec<ClipboardFormat>,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            scope: ClipboardScope::default(),
            max_bytes: DEFAULT_CLIPBOARD_MAX_BYTES,
            formats: default_formats(),
        }
    }
}

fn default_max_bytes() -> u64 {
    DEFAULT_CLIPBOARD_MAX_BYTES
}

fn default_formats() -> Vec<ClipboardFormat> {
    vec![
        ClipboardFormat::Text,
        ClipboardFormat::Image,
        ClipboardFormat::Html,
    ]
}

/// 同步范围（FR-4.4）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardScope {
    /// 仅主控 ↔ 当前受控从机（默认）。
    #[default]
    Current,
    /// 主控 ↔ 全部从机。
    All,
}

/// 剪贴板同步格式（docs/04 §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardFormat {
    /// 纯文本。
    Text,
    /// 位图图像。
    Image,
    /// HTML（富文本）。
    Html,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/01 §5 示例原文（无 version 字段 → 解析为 v0，经迁移链升级到当前版本）。
    const DOCS_EXAMPLE_TOML: &str = r#"
role = "host"                 # host | client
security_code_ref = "keychain:xcontrol"   # 或 file（回退模式）
[general]                     # 通用项（v2 起，NFR-8）
log_level = "info"            # error | warn | info | debug | trace（默认 info；--log-level 可覆盖）
[net]
port = 4243
cidrs = []                    # 空 = 自动取各接口真实前缀
discovery = ["mdns", "beacon", "scan-off"]
[layout]                      # screens 网格（FR-1.3）
[keymap]
preset = "mac2win"
rules = []
[hotkeys]
next = "Ctrl+Alt+Right"
[clipboard]
scope = "current"             # current | all
max_bytes = 16777216
formats = ["text", "image", "html"]
"#;

    #[test]
    fn default_config_uses_conservative_defaults() {
        let config = Config::default();
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.role, Role::Client);
        assert_eq!(config.security_code_ref, "keychain:xcontrol");
        assert_eq!(config.general.log_level, LogLevel::Info);
        // FR-6.5 默认语言：zh-CN（docs/06 §7 文案表 zh-CN 完整）。
        assert_eq!(config.general.language, DEFAULT_LANGUAGE);
        assert_eq!(config.net.port, 4243);
        assert!(config.net.cidrs.is_empty());
        // FR-5.4/5.5 默认：白名单不启用、无已记忆主控。
        assert!(config.net.allowlist.is_empty());
        assert_eq!(config.net.confirmed_host, None);
        assert_eq!(
            config.net.discovery,
            vec![DiscoveryMethod::Mdns, DiscoveryMethod::Beacon]
        );
        assert!(config.layout.screens.is_empty());
        assert_eq!(config.keymap.preset, "none");
        assert!(!config.keymap.passthrough, "FR-3.6 直通开关默认关");
        assert!(config.keymap.rules.is_empty());
        assert_eq!(config.hotkeys.next, "Ctrl+Alt+Right");
        assert_eq!(config.hotkeys.prev, "Ctrl+Alt+Left");
        assert_eq!(config.hotkeys.back_to_host, "Ctrl+Alt+Home");
        assert_eq!(config.hotkeys.pause, "Ctrl+Alt+P");
        assert_eq!(config.clipboard.scope, ClipboardScope::Current);
        assert_eq!(config.clipboard.max_bytes, 16 * 1024 * 1024);
        assert_eq!(
            config.clipboard.formats,
            vec![
                ClipboardFormat::Text,
                ClipboardFormat::Image,
                ClipboardFormat::Html
            ]
        );
    }

    #[test]
    fn docs_example_parses_field_by_field() {
        let config: Config = toml::from_str(DOCS_EXAMPLE_TOML).unwrap();
        assert_eq!(config.version, 0, "docs 示例无 version 字段，应为 v0");
        assert_eq!(config.role, Role::Host);
        assert_eq!(config.security_code_ref, "keychain:xcontrol");
        assert_eq!(config.general.log_level, LogLevel::Info);
        assert_eq!(config.net.port, 4243);
        assert!(config.net.cidrs.is_empty());
        assert_eq!(
            config.net.discovery,
            vec![
                DiscoveryMethod::Mdns,
                DiscoveryMethod::Beacon,
                DiscoveryMethod::ScanOff
            ]
        );
        assert!(config.layout.screens.is_empty());
        assert_eq!(config.keymap.preset, "mac2win");
        assert!(config.keymap.rules.is_empty());
        assert_eq!(config.hotkeys.next, "Ctrl+Alt+Right");
        assert_eq!(config.clipboard.scope, ClipboardScope::Current);
        assert_eq!(config.clipboard.max_bytes, 16_777_216);
        assert_eq!(
            config.clipboard.formats,
            vec![
                ClipboardFormat::Text,
                ClipboardFormat::Image,
                ClipboardFormat::Html
            ]
        );
    }

    /// 完整配置 round-trip：解析 → 序列化 → 再解析等价（PRD 验收项）。
    #[test]
    fn round_trip_full_config_is_equivalent() {
        let original = Config {
            version: CURRENT_CONFIG_VERSION,
            role: Role::Host,
            security_code_ref: "file:secrets/code".to_owned(),
            general: GeneralConfig {
                log_level: LogLevel::Debug,
                // FR-6.5（v3 内追加字段）：语言随 round-trip。
                language: "en".to_owned(),
            },
            net: NetConfig {
                port: 5000,
                cidrs: vec!["192.168.1.0/24".to_owned(), "fd00::/8".to_owned()],
                discovery: vec![DiscoveryMethod::Mdns, DiscoveryMethod::Scan],
                // FR-5.4/5.5（v3 内追加字段）：白名单 + 已记忆主控随 round-trip。
                allowlist: vec!["192.168.1.0/24".to_owned()],
                confirmed_host: Some("ab".repeat(16)),
            },
            layout: LayoutConfig {
                screens: vec![
                    LayoutScreen {
                        id: "desk-right".to_owned(),
                        relative_to: None,
                        direction: None,
                    },
                    LayoutScreen {
                        id: "sofa".to_owned(),
                        relative_to: Some("desk-right".to_owned()),
                        direction: Some(Direction::Up),
                    },
                ],
            },
            keymap: KeymapConfig {
                preset: "mac2win".to_owned(),
                passthrough: false,
                rules: vec![
                    // 形态 1：单键重映射
                    KeymapRule {
                        trigger: RuleTrigger {
                            key: Some("CapsLock".to_owned()),
                            ..RuleTrigger::default()
                        },
                        action: RuleAction {
                            key: Some("Escape".to_owned()),
                            ..RuleAction::default()
                        },
                        from: None,
                        to: None,
                    },
                    // 形态 2：修饰键事件级映射
                    KeymapRule {
                        trigger: RuleTrigger {
                            mod_: Some("LCtrl".to_owned()),
                            ..RuleTrigger::default()
                        },
                        action: RuleAction {
                            mod_: Some("LWin".to_owned()),
                            ..RuleAction::default()
                        },
                        from: None,
                        to: None,
                    },
                    // 形态 4：drop（CK_* 常量名直书）
                    KeymapRule {
                        trigger: RuleTrigger {
                            key: Some("CK_MUTE".to_owned()),
                            ..RuleTrigger::default()
                        },
                        action: RuleAction {
                            drop: Some(true),
                            ..RuleAction::default()
                        },
                        from: None,
                        to: None,
                    },
                ],
            },
            hotkeys: HotkeysConfig {
                next: "Ctrl+Alt+Right".to_owned(),
                prev: "Ctrl+Alt+Left".to_owned(),
                back_to_host: "Ctrl+Alt+Home".to_owned(),
                pause: "Ctrl+Alt+P".to_owned(),
                gesture_fallback: false,
            },
            clipboard: ClipboardConfig {
                scope: ClipboardScope::All,
                max_bytes: 1024,
                formats: vec![ClipboardFormat::Text],
            },
        };

        let serialized = toml::to_string_pretty(&original).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed, original);
    }

    /// 各字段默认值配置 round-trip：序列化 → 再解析等价（PRD 验收项）。
    #[test]
    fn round_trip_default_config_is_equivalent() {
        let original = Config::default();
        let serialized = toml::to_string_pretty(&original).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed, original);
    }

    /// `version` 序列化在首位（docs/01 §5 schema 可读性）。
    #[test]
    fn serialized_output_starts_with_version() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let first_line = text.lines().next().unwrap();
        assert_eq!(first_line, "version = 3");
        // `[general]` 节随默认配置序列化（字段声明顺序：security_code_ref 之后）
        assert!(text.contains("[general]\nlog_level = \"info\""), "{text}");
        // v3 [keymap]：passthrough 默认值随序列化落盘（round-trip 显式化）
        assert!(text.contains("passthrough = false"), "{text}");
        // FR-5.4/5.5：allowlist 默认空表落盘（显式形态）；confirmed_host None 省略。
        assert!(text.contains("allowlist = []"), "{text}");
        assert!(!text.contains("confirmed_host"), "{text}");
    }

    /// 未知枚举值解析失败（role / discovery / scope / format / log_level 类型安全）。
    #[test]
    fn unknown_enum_values_are_rejected() {
        for raw in [
            "role = \"banana\"",
            "[net]\ndiscovery = [\"carrier-pigeon\"]",
            "[clipboard]\nscope = \"everywhere\"",
            "[clipboard]\nformats = [\"rtf\"]",
            "[general]\nlog_level = \"verbose\"",
        ] {
            assert!(
                toml::from_str::<Config>(raw).is_err(),
                "应拒绝未知枚举值：{raw}"
            );
        }
    }

    /// `LogLevel` 字面值解析/回显与 VARIANTS 一致（CLI `--log-level` 复用面）。
    #[test]
    fn log_level_parse_and_as_str_round_trip() {
        for variant in LogLevel::VARIANTS {
            let level = LogLevel::parse(variant).unwrap();
            assert_eq!(level.as_str(), variant);
        }
        assert_eq!(LogLevel::parse("Info"), None, "字面值区分大小写");
        assert_eq!(LogLevel::parse(""), None);
        assert_eq!(LogLevel::parse("verbose"), None);
    }

    /// `scan-off` 兼容写法往返（docs/01 §5 示例字面值）。
    #[test]
    fn scan_off_survives_round_trip() {
        let raw = "[net]\ndiscovery = [\"scan-off\"]";
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.net.discovery, vec![DiscoveryMethod::ScanOff]);
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("\"scan-off\""), "{serialized}");
    }

    /// Option 字段为 None 时序列化省略键（round-trip 后仍为 None）；
    /// trigger/action 的空字段同样省略（v3 规则序列化只写命中的形态字段）。
    #[test]
    fn none_option_fields_are_omitted_and_reparse_as_none() {
        let config = Config {
            keymap: KeymapConfig {
                preset: "none".to_owned(),
                passthrough: false,
                rules: vec![KeymapRule {
                    trigger: RuleTrigger {
                        key: Some("CK_MUTE".to_owned()),
                        ..RuleTrigger::default()
                    },
                    action: RuleAction {
                        drop: Some(true),
                        ..RuleAction::default()
                    },
                    from: None,
                    to: None,
                }],
            },
            layout: LayoutConfig {
                screens: vec![LayoutScreen {
                    id: "solo".to_owned(),
                    relative_to: None,
                    direction: None,
                }],
            },
            ..Config::default()
        };
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(!serialized.contains("relative_to"), "{serialized}");
        assert!(!serialized.contains("direction"), "{serialized}");
        // v2 遗留字段与空 Option 字段不落盘
        assert!(!serialized.contains("from"), "{serialized}");
        assert!(!serialized.contains("to "), "{serialized}");
        assert!(!serialized.contains("mod "), "{serialized}");
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.keymap.rules[0].trigger.key.as_deref(),
            Some("CK_MUTE")
        );
        assert_eq!(reparsed.keymap.rules[0].action.drop, Some(true));
        assert_eq!(reparsed.layout.screens[0].relative_to, None);
    }

    /// docs/03 §4.1 四形态 TOML 原文逐条解析（v3 规则模型 1:1 对应）。
    #[test]
    fn docs_rule_forms_parse_field_by_field() {
        let raw = r#"
version = 3
[keymap]
preset = "none"
[[keymap.rules]]
trigger.key = "CapsLock"
action.key = "Escape"
[[keymap.rules]]
trigger.mod = "LeftMeta"
action.mod = "LeftCtrl"
[[keymap.rules]]
trigger = { mods = ["LeftMeta"], key = "C" }
action = { mods = ["LeftCtrl", "LeftShift"], key = "C" }
[[keymap.rules]]
trigger = { mods = ["LeftMeta", "LeftShift"], key = "Digit3" }
action.drop = true
"#;
        let config: Config = toml::from_str(raw).unwrap();
        let rules = &config.keymap.rules;
        assert_eq!(rules.len(), 4);
        // 形态 1：单键重映射
        assert_eq!(rules[0].trigger.key.as_deref(), Some("CapsLock"));
        assert_eq!(rules[0].action.key.as_deref(), Some("Escape"));
        // 形态 2：修饰键映射
        assert_eq!(rules[1].trigger.mod_.as_deref(), Some("LeftMeta"));
        assert_eq!(rules[1].action.mod_.as_deref(), Some("LeftCtrl"));
        // 形态 3：组合级
        assert_eq!(
            rules[2].trigger.mods.as_deref(),
            Some(["LeftMeta".to_owned()].as_slice())
        );
        assert_eq!(
            rules[2].action.mods.as_deref(),
            Some(["LeftCtrl".to_owned(), "LeftShift".to_owned()].as_slice())
        );
        // 形态 4：drop
        assert_eq!(rules[3].action.drop, Some(true));
        // 全部规则无 v2 遗留字段
        assert!(
            rules
                .iter()
                .all(|rule| rule.from.is_none() && rule.to.is_none())
        );
    }
}
