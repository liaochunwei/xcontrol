//! 配置校验：每个负例给出「字段路径 + 中文提示」（PRD 验收项）。
//!
//! 在 load 与 save 双侧执行（[`crate::io`]）。风格：收集**全部**问题一次返回，
//! 便于用户一次修复多项；被校验字段均非敏感信息，当前值可回显。

use std::net::IpAddr;

use crate::error::ValidationError;
use crate::keynames::{is_modifier_name, parse_key_name};
use crate::model::{
    Config, DEFAULT_HOTKEY_NEXT, DiscoveryMethod, KEYMAP_MAX_RULES, KEYMAP_PRESETS, KeymapRule,
    LANGUAGE_VARIANTS, MAX_CLIPBOARD_BYTES_LIMIT, RuleAction, RuleTrigger,
};
use crate::secret::SecretRef;

/// 可用修饰键（docs/03 §4.6 默认热键用 Ctrl/Alt；Meta/Super 供平台差异命名）。
const HOTKEY_MODIFIERS: [&str; 5] = ["Ctrl", "Alt", "Shift", "Meta", "Super"];

/// 校验整个配置；无问题时 `Ok(())`，否则按字段顺序返回全部 [`ValidationError`]。
pub fn validate(config: &Config) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    validate_security_code_ref(config, &mut errors);
    validate_general(config, &mut errors);
    validate_net(config, &mut errors);
    validate_layout(config, &mut errors);
    validate_keymap(config, &mut errors);
    validate_hotkeys(config, &mut errors);
    validate_clipboard(config, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// `[general]` 节校验：界面语言（FR-6.5）必须是合法字面值——写坏的语言值会让
/// UI 层静默回退默认（`xcontrol-i18n::Lang::parse` 失败回退 zh-CN），这里显式
/// 拒绝避免"配置了却没生效"的静默偏差。
fn validate_general(config: &Config, errors: &mut Vec<ValidationError>) {
    let language = config.general.language.trim();
    if !LANGUAGE_VARIANTS.contains(&language) {
        errors.push(ValidationError::new(
            "general.language",
            format!(
                "必须是 {} 之一（当前 {language:?}；切换语言在设置 → 通用页）",
                LANGUAGE_VARIANTS.join(" / ")
            ),
        ));
    }
}

fn validate_security_code_ref(config: &Config, errors: &mut Vec<ValidationError>) {
    let raw = config.security_code_ref.trim();
    if SecretRef::parse(raw).is_none() {
        errors.push(ValidationError::new(
            "security_code_ref",
            format!("安全码引用必须形如 keychain:<名称> 或 file:<路径>（当前 {raw:?}）"),
        ));
    }
}

fn validate_net(config: &Config, errors: &mut Vec<ValidationError>) {
    if config.net.port == 0 {
        errors.push(ValidationError::new(
            "net.port",
            format!(
                "端口必须在 1–65535 之间（当前 {}；docs/02 §0 默认 4243）",
                config.net.port
            ),
        ));
    }

    for (index, cidr) in config.net.cidrs.iter().enumerate() {
        validate_cidr(&format!("net.cidrs[{index}]"), cidr, errors);
    }

    // FR-5.4 主控接入白名单：条目形态与 net.cidrs 同一套 CIDR 校验（serve 层
    // 匹配语义见 xcontrol-net::serve——空列表 = 不启用）。
    for (index, cidr) in config.net.allowlist.iter().enumerate() {
        validate_cidr(&format!("net.allowlist[{index}]"), cidr, errors);
    }

    // FR-5.5 已记忆主控：instance id 的 32 位 hex（比较侧大小写不敏感，这里只
    // 验形态；空串/长度不符/非 hex 一律拒绝——写坏的记忆条目会让发现循环
    // 静默匹配不到任何候选）。
    if let Some(host) = &config.net.confirmed_host {
        let host = host.trim();
        let valid = host.len() == 32 && host.chars().all(|c| c.is_ascii_hexdigit());
        if !valid {
            errors.push(ValidationError::new(
                "net.confirmed_host",
                format!(
                    "必须是主控 instance id 的 32 位 hex（当前 {host:?}；状态页/托盘选择候选时自动写入）"
                ),
            ));
        }
    }

    let discovery = &config.net.discovery;
    if discovery.contains(&DiscoveryMethod::Scan) && discovery.contains(&DiscoveryMethod::ScanOff) {
        errors.push(ValidationError::new(
            "net.discovery",
            "scan 与 scan-off 不能同时启用（scan-off 是显式关闭主动扫描的写法）",
        ));
    }
    let mut seen: Vec<DiscoveryMethod> = Vec::new();
    for (index, method) in discovery.iter().enumerate() {
        if seen.contains(method) {
            errors.push(ValidationError::new(
                format!("net.discovery[{index}]"),
                format!("发现机制重复（{method:?} 已出现过）"),
            ));
        } else {
            seen.push(*method);
        }
    }
}

/// 单条 CIDR：`地址/前缀长度`（IPv4 前缀 ≤32，IPv6 ≤128）。
fn validate_cidr(path: &str, cidr: &str, errors: &mut Vec<ValidationError>) {
    let raw = cidr.trim();
    let Some((addr_part, prefix_part)) = raw.split_once('/') else {
        errors.push(ValidationError::new(
            path,
            format!("不是合法的 CIDR（应为 地址/前缀长度，如 192.168.1.0/24；当前 {raw:?}）"),
        ));
        return;
    };
    let parsed_addr = addr_part.trim().parse::<IpAddr>();
    let Ok(addr) = parsed_addr else {
        errors.push(ValidationError::new(
            path,
            format!("IP 地址部分非法（当前 {addr_part:?}；示例 192.168.1.0 或 fd00::1）"),
        ));
        return;
    };
    let parsed_prefix = prefix_part.trim().parse::<u8>();
    let Ok(prefix) = parsed_prefix else {
        errors.push(ValidationError::new(
            path,
            format!("前缀长度必须是数字（当前 {prefix_part:?}）"),
        ));
        return;
    };
    let max_prefix = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if prefix > max_prefix {
        errors.push(ValidationError::new(
            path,
            format!("前缀长度超出范围（IPv4 上限 32，IPv6 上限 128；当前 {prefix}）"),
        ));
    }
}

fn validate_layout(config: &Config, errors: &mut Vec<ValidationError>) {
    let screens = &config.layout.screens;
    for (index, screen) in screens.iter().enumerate() {
        let path = format!("layout.screens[{index}]");
        if screen.id.trim().is_empty() {
            errors.push(ValidationError::new(
                &path,
                "屏幕标识 id 不能为空（如 \"desk-right\"）",
            ));
        }
        match (&screen.relative_to, &screen.direction) {
            // 成对提供 / 两者皆缺（独立锚点，路由图跳过）均合法；
            // direction 单独提供 = 锚定主控主屏（11-19-m5-layout 方位制最小模型）。
            (None, None) | (Some(_), Some(_)) | (None, Some(_)) => {}
            (Some(_), None) => errors.push(ValidationError::new(
                &path,
                "direction 缺失：relative_to 与 direction 必须成对提供（FR-1.3）",
            )),
        }
    }
    // id 重复（O(n²)，条目量级为个位数屏幕）
    for (index, screen) in screens.iter().enumerate() {
        if screens
            .iter()
            .take(index)
            .any(|other| other.id == screen.id)
        {
            errors.push(ValidationError::new(
                format!("layout.screens[{index}].id"),
                format!("屏幕标识重复（{:?} 已出现过）", screen.id),
            ));
        }
    }
}

fn validate_keymap(config: &Config, errors: &mut Vec<ValidationError>) {
    if !KEYMAP_PRESETS.contains(&config.keymap.preset.as_str()) {
        errors.push(ValidationError::new(
            "keymap.preset",
            format!(
                "未知预设（当前 {:?}；可用：{}，docs/03 §4.4）",
                config.keymap.preset,
                KEYMAP_PRESETS.join("、")
            ),
        ));
    }
    if config.keymap.rules.len() > KEYMAP_MAX_RULES {
        errors.push(ValidationError::new(
            "keymap.rules",
            format!(
                "规则数量超上限（当前 {} 条 > 上限 {KEYMAP_MAX_RULES} 条）",
                config.keymap.rules.len()
            ),
        ));
    }
    for (index, rule) in config.keymap.rules.iter().enumerate() {
        validate_keymap_rule(index, rule, errors);
    }
}

/// 单条 v3 规则的形态校验（docs/03 §4.1 四形态；与 core `MappingEngine::new`
/// 的编译判定同语义——校验先报、给出字段路径与中文提示）。
fn validate_keymap_rule(index: usize, rule: &KeymapRule, errors: &mut Vec<ValidationError>) {
    let path = move |field: &str| format!("keymap.rules[{index}].{field}");
    // v2 遗留字段残留（v3 文件手写 from/to）：迁移链只转换纯 v2 形态
    if rule.from.is_some() || rule.to.is_some() {
        errors.push(ValidationError::new(
            path("from"),
            "使用了 v2 的 from/to 简式——v3 起规则为 trigger/action 形态（docs/03 §4.1）\
             ，请改写为 trigger.key/trigger.mod/trigger.mods + action；或把 version 降回 2 走迁移链",
        ));
    }

    validate_trigger(&path, &rule.trigger, errors);
    validate_action(&path, &rule.action, errors);
}

/// trigger 形态判定：`mod_` 独占（形态 2）；`key` 独占（形态 1）或与 `mods`
/// 同现（形态 3/4）；`mods` 无 `key`、混用、全空均非法。键名经 [`parse_key_name`]
/// 解析，修饰键性用 [`is_modifier_name`] 判定。
fn validate_trigger(
    path: &dyn Fn(&str) -> String,
    trigger: &RuleTrigger,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(mod_name) = trigger.mod_.as_deref() {
        if trigger.key.is_some() || trigger.mods.is_some() {
            errors.push(ValidationError::new(
                path("trigger.mod"),
                "trigger.mod 形态不接受 key/mods（修饰键映射是单键事件级规则，docs/03 §4.1 形态 2）",
            ));
        }
        if mod_name.trim().is_empty() {
            errors.push(ValidationError::new(
                path("trigger.mod"),
                "trigger.mod 不能为空",
            ));
        } else if !is_modifier_name(mod_name) {
            errors.push(ValidationError::new(
                path("trigger.mod"),
                format!(
                    "trigger.mod 必须是修饰键名（当前 {mod_name:?}；可用 LeftCtrl/…/RightMeta，\
                     简写 LCtrl/LMeta/LOpt/LWin 等；左右手分开，docs/03 §4.1）"
                ),
            ));
        }
        return;
    }
    match (trigger.key.as_deref(), trigger.mods.as_deref()) {
        (None, None) => {
            errors.push(ValidationError::new(
                path("trigger"),
                "trigger 缺少内容：单键用 trigger.key，修饰键映射用 trigger.mod，\
                 组合用 trigger = { mods = […], key = … }（docs/03 §4.1）",
            ));
        }
        (Some(key), Some(mods)) => {
            validate_mods_list(&path("trigger.mods"), mods, errors);
            validate_non_modifier_key(&path("trigger.key"), key, errors);
        }
        (Some(key), None) => {
            validate_non_modifier_key(&path("trigger.key"), key, errors);
        }
        (None, Some(_)) => {
            errors.push(ValidationError::new(
                path("trigger"),
                "trigger.mods 必须与 trigger.key 同现（组合级规则的触发键不可省略，docs/03 §4.1 形态 3/4）",
            ));
        }
    }
}

/// action 形态判定：`drop = true` 独占（形态 4）；`mod_` 独占（形态 2）；`key`
/// 独占（形态 1）或与 `mods` 同现（形态 3）；`drop = false`、混用、全空非法。
fn validate_action(
    path: &dyn Fn(&str) -> String,
    action: &RuleAction,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(drop) = action.drop {
        if action.key.is_some() || action.mod_.is_some() || action.mods.is_some() {
            errors.push(ValidationError::new(
                path("action.drop"),
                "action.drop 不接受与 key/mod/mods 同现（丢弃即不产出注入，docs/03 §4.1 形态 4）",
            ));
        }
        if !drop {
            errors.push(ValidationError::new(
                path("action.drop"),
                "action.drop 只接受 true（不丢弃请省略该字段）",
            ));
        }
        return;
    }
    if let Some(mod_name) = action.mod_.as_deref() {
        if action.key.is_some() || action.mods.is_some() {
            errors.push(ValidationError::new(
                path("action.mod"),
                "action.mod 形态不接受 key/mods（修饰键映射是单键事件级规则，docs/03 §4.1 形态 2）",
            ));
        }
        if mod_name.trim().is_empty() {
            errors.push(ValidationError::new(
                path("action.mod"),
                "action.mod 不能为空",
            ));
        } else if !is_modifier_name(mod_name) {
            errors.push(ValidationError::new(
                path("action.mod"),
                format!(
                    "action.mod 必须是修饰键名（当前 {mod_name:?}；可用 LeftCtrl/…/RightMeta，\
                     简写 LCtrl/LMeta/LOpt/LWin 等，docs/03 §4.1）"
                ),
            ));
        }
        return;
    }
    match (action.key.as_deref(), action.mods.as_deref()) {
        (None, None) => {
            errors.push(ValidationError::new(
                path("action"),
                "action 缺少内容：注入键用 action.key，修饰键用 action.mod，\
                 组合弦用 action = { mods = […], key = … }，丢弃用 action.drop = true（docs/03 §4.1）",
            ));
        }
        (Some(key), Some(mods)) => {
            validate_mods_list(&path("action.mods"), mods, errors);
            validate_non_modifier_key(&path("action.key"), key, errors);
        }
        (Some(key), None) => {
            validate_non_modifier_key(&path("action.key"), key, errors);
        }
        (None, Some(_)) => {
            errors.push(ValidationError::new(
                path("action"),
                "action.mods 必须与 action.key 同现（组合弦总有一个注入键，docs/03 §4.1 形态 3）",
            ));
        }
    }
}

/// 修饰键集合列表：非空、全部是修饰键名、不重复（`trigger.mods` 精确集合匹配
/// 要求集合内无冗余，docs/03 §4.1）。
fn validate_mods_list(path: &str, mods: &[String], errors: &mut Vec<ValidationError>) {
    if mods.is_empty() {
        errors.push(ValidationError::new(
            path,
            "mods 不能为空列表（组合级规则至少要有一个触发修饰键；无修饰键的单键规则请省略 mods）",
        ));
        return;
    }
    let mut seen: Vec<&str> = Vec::new();
    for name in mods {
        let name = name.as_str();
        if name.trim().is_empty() {
            errors.push(ValidationError::new(path, "mods 中的键名不能为空"));
        } else if !is_modifier_name(name) {
            errors.push(ValidationError::new(
                path,
                format!(
                    "mods 必须全是修饰键名（当前 {name:?}；可用 LeftCtrl/…/RightMeta，\
                     简写 LCtrl/LMeta/LOpt/LWin 等，docs/03 §4.1）"
                ),
            ));
        }
        if seen.contains(&name) {
            errors.push(ValidationError::new(
                path,
                format!("mods 中重复的修饰键 {name:?}（集合匹配语义下重复无意义）"),
            ));
        } else {
            seen.push(name);
        }
    }
}

/// 非修饰键名（`trigger.key`/`action.key`）：非空、可解析、不是修饰键
/// （修饰键规则请用 trigger.mod/action.mod 形态——两类规则的运行语义不同：
/// 事件级对照表 vs 单键重映射）。
fn validate_non_modifier_key(path: &str, key: &str, errors: &mut Vec<ValidationError>) {
    if key.trim().is_empty() {
        errors.push(ValidationError::new(path, "键名不能为空"));
        return;
    }
    if parse_key_name(key).is_none() {
        errors.push(ValidationError::new(
            path,
            format!(
                "无法解析的语义键名 {key:?}（docs/03 §4.1 键名文法：A–Z、Digit0–9、F1–F24、\
                 CapsLock/Escape/Enter 等功能名，或 proto 常量全名 KEY_*/CK_*）"
            ),
        ));
        return;
    }
    if is_modifier_name(key) {
        errors.push(ValidationError::new(
            path,
            format!(
                "{key:?} 是修饰键——修饰键规则请用 trigger.mod/action.mod 形态 \
                 （事件级映射语义与单键重映射不同，docs/03 §4.1/§4.2）"
            ),
        ));
    }
}

fn validate_hotkeys(config: &Config, errors: &mut Vec<ValidationError>) {
    let fields = [
        ("hotkeys.next", config.hotkeys.next.as_str()),
        ("hotkeys.prev", config.hotkeys.prev.as_str()),
        ("hotkeys.back_to_host", config.hotkeys.back_to_host.as_str()),
        ("hotkeys.pause", config.hotkeys.pause.as_str()),
    ];
    for (path, value) in fields {
        validate_hotkey(path, value, errors);
    }
}

/// 热键组合格式：`修饰键+主键`（如 `Ctrl+Alt+Right`）。
/// 主键任意非空词元（平台键名归 xcontrol-input 校验）；修饰键必须已知且不重复。
fn validate_hotkey(path: &str, raw: &str, errors: &mut Vec<ValidationError>) {
    let value = raw.trim();
    if value.is_empty() {
        errors.push(ValidationError::new(
            path,
            format!("热键不能为空（应为 修饰键+主键，如 {DEFAULT_HOTKEY_NEXT}）"),
        ));
        return;
    }
    let parts: Vec<&str> = value.split('+').map(str::trim).collect();
    if parts.iter().any(|part| part.is_empty()) {
        errors.push(ValidationError::new(
            path,
            format!("热键存在空段（多余或缺失的 '+'；当前 {value:?}）"),
        ));
        return;
    }
    let Some((main, modifiers)) = parts.split_last() else {
        // 上一分支已拦截空串，此处防御性兜底
        errors.push(ValidationError::new(path, "热键不能为空"));
        return;
    };
    if modifiers.is_empty() {
        errors.push(ValidationError::new(
            path,
            format!(
                "热键必须包含修饰键（仅主键 {main:?} 无法与普通输入区分；应为 修饰键+主键，如 {DEFAULT_HOTKEY_NEXT}）"
            ),
        ));
    }
    if is_modifier(main) {
        errors.push(ValidationError::new(
            path,
            format!("热键主键不能是修饰键（当前 {main:?}）"),
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for modifier in modifiers {
        if !HOTKEY_MODIFIERS.contains(modifier) {
            errors.push(ValidationError::new(
                path,
                format!(
                    "未知修饰键 {modifier:?}（可用：{}）",
                    HOTKEY_MODIFIERS.join("、")
                ),
            ));
        }
        if seen.contains(modifier) {
            errors.push(ValidationError::new(
                path,
                format!("重复的修饰键 {modifier:?}"),
            ));
        } else {
            seen.push(modifier);
        }
    }
}

fn is_modifier(key: &str) -> bool {
    HOTKEY_MODIFIERS.contains(&key)
}

fn validate_clipboard(config: &Config, errors: &mut Vec<ValidationError>) {
    if config.clipboard.max_bytes > MAX_CLIPBOARD_BYTES_LIMIT {
        errors.push(ValidationError::new(
            "clipboard.max_bytes",
            format!(
                "超过上限（当前 {} 字节 > 上限 {MAX_CLIPBOARD_BYTES_LIMIT} 字节 = 64 MiB；FR-4.5 默认 16 MiB）",
                config.clipboard.max_bytes
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::CURRENT_CONFIG_VERSION;
    use crate::model::{
        ClipboardConfig, ClipboardFormat, ClipboardScope, Direction, DiscoveryMethod,
        GeneralConfig, HotkeysConfig, KeymapConfig, KeymapRule, LayoutConfig, LayoutScreen,
        NetConfig, Role, RuleAction, RuleTrigger,
    };
    use crate::testing::load_str;

    /// v3 形态规则构造捷径（trigger/action 各只填一个字段）。
    fn rule(trigger: RuleTrigger, action: RuleAction) -> KeymapRule {
        KeymapRule {
            trigger,
            action,
            from: None,
            to: None,
        }
    }

    /// 合法基线配置（各节均显式给出合法值）。
    fn valid_config() -> Config {
        Config {
            version: CURRENT_CONFIG_VERSION,
            role: Role::Host,
            security_code_ref: "keychain:xcontrol".to_owned(),
            general: GeneralConfig::default(),
            net: NetConfig {
                port: 4243,
                cidrs: vec!["192.168.1.0/24".to_owned(), "fd00::/8".to_owned()],
                discovery: vec![DiscoveryMethod::Mdns, DiscoveryMethod::Beacon],
                allowlist: Vec::new(),
                confirmed_host: None,
            },
            layout: LayoutConfig {
                screens: vec![LayoutScreen {
                    id: "desk".to_owned(),
                    relative_to: Some("sofa".to_owned()),
                    direction: Some(Direction::Left),
                }],
            },
            keymap: KeymapConfig {
                preset: "mac2win".to_owned(),
                passthrough: false,
                rules: vec![rule(
                    RuleTrigger {
                        mod_: Some("LMeta".to_owned()),
                        ..RuleTrigger::default()
                    },
                    RuleAction {
                        mod_: Some("LCtrl".to_owned()),
                        ..RuleAction::default()
                    },
                )],
            },
            hotkeys: HotkeysConfig::default(),
            clipboard: ClipboardConfig {
                scope: ClipboardScope::Current,
                max_bytes: 16 * 1024 * 1024,
                formats: vec![ClipboardFormat::Text],
            },
        }
    }

    fn validation_errors(config: &Config) -> Vec<ValidationError> {
        validate(config).unwrap_err()
    }

    fn assert_has_error(errors: &[ValidationError], path: &str, keyword: &str) {
        assert!(
            errors
                .iter()
                .any(|e| e.path == path && e.message.contains(keyword)),
            "应包含 path={path:?} 且消息含 {keyword:?} 的错误；实际：{errors:?}"
        );
    }

    #[test]
    fn valid_config_passes() {
        assert_eq!(validate(&valid_config()), Ok(()));
        // 默认配置同样合法（所有默认值必须过校验）
        assert_eq!(validate(&Config::default()), Ok(()));
        // docs/01 §5 示例经迁移后合法（load 流水线）
        let docs_example = r#"
role = "host"
security_code_ref = "keychain:xcontrol"
[net]
port = 4243
[layout]
[keymap]
preset = "mac2win"
[hotkeys]
next = "Ctrl+Alt+Right"
[clipboard]
max_bytes = 16777216
"#;
        assert!(load_str(docs_example).is_ok());
    }

    #[test]
    fn unknown_language_is_rejected_with_field_path() {
        // FR-6.5：语言必须是 zh-CN | en——写坏的值显式拒绝（避免"配置了却
        // 静默回退默认"的偏差；运行时侧 Lang::parse 失败回退 zh-CN 为容错面）。
        let mut config = valid_config();
        config.general.language = "fr".to_owned();
        let errors = validation_errors(&config);
        assert_has_error(&errors, "general.language", "必须是 zh-CN / en 之一");
        let mut config = valid_config();
        config.general.language = "zh_CN".to_owned();
        assert!(validate(&config).is_err(), "zh_CN（下划线）不合法");
        // 合法值两面。
        for language in ["zh-CN", "en"] {
            let mut config = valid_config();
            config.general.language = language.to_owned();
            assert_eq!(validate(&config), Ok(()), "{language} 合法");
        }
    }

    #[test]
    fn port_zero_is_rejected_with_field_path() {
        let mut config = valid_config();
        config.net.port = 0;
        let errors = validation_errors(&config);
        assert_has_error(&errors, "net.port", "端口必须在 1–65535 之间");
        assert!(errors.iter().any(|e| e.message.contains("当前 0")));
    }

    #[test]
    fn malformed_cidrs_are_rejected_with_indexed_paths() {
        let cases = [
            ("192.168.1.0", "地址/前缀长度"),
            ("not-an-ip/24", "IP 地址部分非法"),
            ("192.168.1.0/abc", "前缀长度必须是数字"),
            ("192.168.1.0/33", "前缀长度超出范围"),
            ("fd00::/129", "前缀长度超出范围"),
            ("192.168.1.256/24", "IP 地址部分非法"),
        ];
        for (index, (cidr, keyword)) in cases.iter().enumerate() {
            let mut config = valid_config();
            config.net.cidrs = vec![(*cidr).to_owned()];
            let errors = validation_errors(&config);
            assert_has_error(&errors, "net.cidrs[0]", keyword);
            // 索引路径随列表位置变化（多项场景由下面的用例覆盖）
            assert!(!errors.is_empty(), "case {index}: {cidr} 应被拒绝");
        }

        // 多条 CIDR 时路径带索引
        let mut config = valid_config();
        config.net.cidrs = vec!["10.0.0.0/8".to_owned(), "bad/24".to_owned()];
        let errors = validation_errors(&config);
        assert_has_error(&errors, "net.cidrs[1]", "IP 地址部分非法");
        assert!(
            !errors.iter().any(|e| e.path == "net.cidrs[0]"),
            "合法条目不应报错：{errors:?}"
        );
    }

    #[test]
    fn discovery_scan_conflict_and_duplicates_are_rejected() {
        let mut config = valid_config();
        config.net.discovery = vec![DiscoveryMethod::Scan, DiscoveryMethod::ScanOff];
        let errors = validation_errors(&config);
        assert_has_error(&errors, "net.discovery", "不能同时启用");

        let mut config = valid_config();
        config.net.discovery = vec![DiscoveryMethod::Mdns, DiscoveryMethod::Mdns];
        let errors = validation_errors(&config);
        assert_has_error(&errors, "net.discovery[1]", "重复");
    }

    /// FR-5.4 白名单：合法 CIDR 通过；非法条目按索引路径拒绝（与 net.cidrs
    /// 同一套校验语义）。
    #[test]
    fn net_allowlist_entries_are_cidr_validated() {
        let mut config = valid_config();
        config.net.allowlist = vec!["192.168.1.0/24".to_owned(), "fd00::/8".to_owned()];
        assert_eq!(validate(&config), Ok(()), "合法白名单通过");

        let mut config = valid_config();
        config.net.allowlist = vec!["10.0.0.0/8".to_owned(), "not-a-cidr".to_owned()];
        let errors = validation_errors(&config);
        assert_has_error(&errors, "net.allowlist[1]", "地址/前缀长度");
        assert!(
            !errors.iter().any(|e| e.path == "net.allowlist[0]"),
            "合法条目不应报错：{errors:?}"
        );
    }

    /// FR-5.5 已记忆主控：32 位 hex 合法；空串/长度不符/非 hex 拒绝。
    #[test]
    fn net_confirmed_host_is_hex32_validated() {
        let mut config = valid_config();
        config.net.confirmed_host = Some("a".repeat(32));
        assert_eq!(validate(&config), Ok(()), "32 位 hex 通过");

        for bad in ["", "abc", "a".repeat(31).as_str(), &"z".repeat(32)] {
            let mut config = valid_config();
            config.net.confirmed_host = Some((*bad).to_owned());
            let errors = validation_errors(&config);
            assert_has_error(&errors, "net.confirmed_host", "32 位 hex");
        }
    }

    #[test]
    fn invalid_security_code_refs_are_rejected() {
        for raw in ["", "bogus", "keychain:", "file:", "ftp:host"] {
            let mut config = valid_config();
            config.security_code_ref = raw.to_owned();
            let errors = validation_errors(&config);
            assert_has_error(
                &errors,
                "security_code_ref",
                "keychain:<名称> 或 file:<路径>",
            );
        }
    }

    #[test]
    fn layout_screens_are_validated() {
        // relative_to 必须随 direction 成对（direction 单独提供 = 锚定主控主屏，
        // 11-19-m5-layout 方位制最小模型；两者皆缺 = 独立锚点，均合法）
        let mut config = valid_config();
        config.layout.screens[0].direction = None;
        let errors = validation_errors(&config);
        assert_has_error(&errors, "layout.screens[0]", "direction 缺失");

        // direction-only 条目合法（缺省锚点 = 主控主屏）
        let mut config = valid_config();
        config.layout.screens[0].relative_to = None;
        assert_eq!(validate(&config), Ok(()), "direction-only 应合法");

        // id 重复 / 为空
        let mut config = valid_config();
        config.layout.screens.push(config.layout.screens[0].clone());
        let errors = validation_errors(&config);
        assert_has_error(&errors, "layout.screens[1].id", "重复");

        let mut config = valid_config();
        config.layout.screens[0].id = "  ".to_owned();
        let errors = validation_errors(&config);
        assert_has_error(&errors, "layout.screens[0]", "id 不能为空");
    }

    #[test]
    fn unknown_keymap_preset_is_rejected() {
        let mut config = valid_config();
        config.keymap.preset = "mac2dos".to_owned();
        let errors = validation_errors(&config);
        assert_has_error(&errors, "keymap.preset", "未知预设");
        assert!(errors.iter().any(|e| e.message.contains("mac2win")));
    }

    #[test]
    fn keymap_rule_limit_is_rejected() {
        let mut config = valid_config();
        config.keymap.rules = vec![
            rule(
                RuleTrigger {
                    key: Some("CapsLock".to_owned()),
                    ..RuleTrigger::default()
                },
                RuleAction {
                    key: Some("Escape".to_owned()),
                    ..RuleAction::default()
                },
            );
            KEYMAP_MAX_RULES + 1
        ];
        let errors = validation_errors(&config);
        assert_has_error(&errors, "keymap.rules", "规则数量超上限");
        assert!(errors.iter().any(|e| e.message.contains("256")));
    }

    /// v3 规则形态负例全集：未知键名、修饰键形态误用、组合缺 key、drop=false、
    /// mods 空表/重复/含非修饰键、trigger/action 全空、v2 遗留字段残留。
    #[test]
    fn keymap_rule_shape_negatives_are_rejected_with_paths() {
        let t = |fields: RuleTrigger| fields;
        let a = |fields: RuleAction| fields;
        let cases: Vec<(KeymapRule, &str, &str)> = vec![
            // trigger 形态
            (
                rule(
                    t(RuleTrigger::default()),
                    a(RuleAction {
                        key: Some("Escape".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger",
                "trigger 缺少内容",
            ),
            (
                rule(
                    t(RuleTrigger {
                        key: Some("LeftMeta".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("Escape".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger.key",
                "是修饰键",
            ),
            (
                rule(
                    t(RuleTrigger {
                        key: Some("Bogus".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("Escape".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger.key",
                "无法解析的语义键名",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mod_: Some("CapsLock".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        mod_: Some("LCtrl".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger.mod",
                "必须是修饰键名",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mod_: Some("LMeta".to_owned()),
                        key: Some("C".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        mod_: Some("LCtrl".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger.mod",
                "不接受 key/mods",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mods: Some(vec!["LMeta".to_owned()]),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("C".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger",
                "必须与 trigger.key 同现",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mods: Some(vec![]),
                        key: Some("C".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("C".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger.mods",
                "mods 不能为空列表",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mods: Some(vec!["LMeta".to_owned(), "LMeta".to_owned()]),
                        key: Some("C".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("C".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger.mods",
                "重复的修饰键",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mods: Some(vec!["LMeta".to_owned(), "C".to_owned()]),
                        key: Some("C".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("C".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].trigger.mods",
                "必须全是修饰键名",
            ),
            // action 形态
            (
                rule(
                    t(RuleTrigger {
                        key: Some("CapsLock".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction::default()),
                ),
                "keymap.rules[0].action",
                "action 缺少内容",
            ),
            (
                rule(
                    t(RuleTrigger {
                        key: Some("CapsLock".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        drop: Some(false),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].action.drop",
                "只接受 true",
            ),
            (
                rule(
                    t(RuleTrigger {
                        key: Some("CapsLock".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        drop: Some(true),
                        key: Some("Escape".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].action.drop",
                "不接受与 key/mod/mods 同现",
            ),
            (
                rule(
                    t(RuleTrigger {
                        key: Some("CapsLock".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("LeftMeta".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].action.key",
                "是修饰键",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mod_: Some("LMeta".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        key: Some("LCtrl".to_owned()),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].action.key",
                "是修饰键",
            ),
            (
                rule(
                    t(RuleTrigger {
                        mods: Some(vec!["LMeta".to_owned()]),
                        key: Some("C".to_owned()),
                        ..RuleTrigger::default()
                    }),
                    a(RuleAction {
                        mods: Some(vec!["LCtrl".to_owned()]),
                        ..RuleAction::default()
                    }),
                ),
                "keymap.rules[0].action",
                "必须与 action.key 同现",
            ),
            // v2 遗留字段残留
            (
                KeymapRule {
                    trigger: RuleTrigger {
                        key: Some("CapsLock".to_owned()),
                        ..RuleTrigger::default()
                    },
                    action: RuleAction {
                        key: Some("Escape".to_owned()),
                        ..RuleAction::default()
                    },
                    from: Some("LMeta".to_owned()),
                    to: None,
                },
                "keymap.rules[0].from",
                "from/to 简式",
            ),
        ];
        for (index, (bad_rule, expected_path, keyword)) in cases.iter().enumerate() {
            let mut config = valid_config();
            config.keymap.rules = vec![bad_rule.clone()];
            let errors = validation_errors(&config);
            assert_has_error(&errors, expected_path, keyword);
            assert!(
                errors.iter().all(|e| e.path.starts_with("keymap.rules[0]")),
                "case {index}: 错误应聚焦本条规则：{errors:?}"
            );
        }
    }

    /// docs/03 §4.1 四形态正例 + `passthrough` 开关经 load 流水线全绿。
    #[test]
    fn keymap_v3_forms_pass_validation() {
        let raw = r#"
version = 3
[keymap]
preset = "mac2win"
passthrough = true
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
        let config = load_str(raw).unwrap();
        assert!(config.keymap.passthrough);
        assert_eq!(config.keymap.rules.len(), 4);
    }

    #[test]
    fn hotkey_format_negatives_are_rejected_with_field_path() {
        let cases = [
            // (坏值, 消息关键词)
            ("", "热键不能为空"),
            ("   ", "热键不能为空"),
            ("Ctrl+", "空段"),
            ("Right", "必须包含修饰键"),
            ("Ctrl", "主键不能是修饰键"),
            ("Ctrl+Alt+Foobar+Right", "未知修饰键"),
            ("Ctrl+Ctrl+Right", "重复的修饰键"),
        ];
        for (raw, keyword) in cases {
            let mut config = valid_config();
            config.hotkeys.next = raw.to_owned();
            let errors = validation_errors(&config);
            assert_has_error(&errors, "hotkeys.next", keyword);
        }

        // 每个热键字段都被校验（以 pause 为例）
        let mut config = valid_config();
        config.hotkeys.pause = "P".to_owned();
        let errors = validation_errors(&config);
        assert_has_error(&errors, "hotkeys.pause", "必须包含修饰键");
    }

    #[test]
    fn clipboard_max_bytes_cap_is_enforced() {
        let mut config = valid_config();
        config.clipboard.max_bytes = MAX_CLIPBOARD_BYTES_LIMIT;
        assert_eq!(validate(&config), Ok(()), "恰达上限应放行");

        config.clipboard.max_bytes = MAX_CLIPBOARD_BYTES_LIMIT + 1;
        let errors = validation_errors(&config);
        assert_has_error(&errors, "clipboard.max_bytes", "超过上限");
        assert!(errors.iter().any(|e| e.message.contains("64 MiB")));
    }

    /// 收集全部问题（而非首错即停）：同时配错两个字段，两条错误都在。
    #[test]
    fn all_errors_are_collected_at_once() {
        let mut config = valid_config();
        config.net.port = 0;
        config.hotkeys.next = "Right".to_owned();
        config.clipboard.max_bytes = MAX_CLIPBOARD_BYTES_LIMIT + 1;
        let errors = validation_errors(&config);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert_has_error(&errors, "net.port", "端口");
        assert_has_error(&errors, "hotkeys.next", "修饰键");
        assert_has_error(&errors, "clipboard.max_bytes", "超过上限");
    }

    /// ValidationError 的 Display 即「字段路径: 中文提示」（PRD 验收格式）。
    #[test]
    fn validation_error_display_contains_path_and_message() {
        let error = ValidationError::new("net.port", "端口必须在 1–65535 之间（当前 0）");
        assert_eq!(
            error.to_string(),
            "net.port: 端口必须在 1–65535 之间（当前 0）"
        );
    }
}
