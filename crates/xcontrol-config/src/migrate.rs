//! schema 版本迁移链（docs/01 §5：顶层 `version` 字段 + 迁移函数链）。
//!
//! 约定：
//! - [`CURRENT_CONFIG_VERSION`] 为当前 schema 版本；
//! - `MIGRATIONS[i]` 把版本 i 升级到 i+1；**只追加、不修改历史步骤**
//!   （旧 fixture 必须永远可升级到最新）；
//! - 新增版本时：写 `migrate_vN_to_vN+1` → 追加到 `MIGRATIONS` 尾部 →
//!   [`CURRENT_CONFIG_VERSION`] +1 → 补旧 fixture 升级单测（rust-core spec `testing.md`）。

use crate::error::ConfigError;
use crate::keynames::is_modifier_name;
use crate::model::{ClipboardFormat, Config, DiscoveryMethod, RuleAction, RuleTrigger};
use crate::secret::DEFAULT_SECURITY_CODE_REF;

/// 当前 schema 版本。
///
/// v3 = 扩 `[keymap]` 节（新增 `passthrough` 直通开关 FR-3.6；`rules` 从 v2 的
/// `from`/`to` 简式升级为 docs/03 §4.1 的 trigger/action 四形态模型——旧简式由
/// [`migrate_v2_to_v3`] 按键名是否修饰键自动归类转换）；
/// v2 = 新增 `[general]` 节（`log_level`，NFR-8；v1→v2 仅扩节，语义默认 info）；
/// v1 = 首个带版本号的 schema（字段与 docs/01 §5 示例一致）；
/// v0 = 无 `version` 字段的初版格式（docs/01 §5 示例文本即 v0 形态）。
pub const CURRENT_CONFIG_VERSION: u32 = 3;

/// 迁移链：`MIGRATIONS[0]` = v0→v1，依此类推。
const MIGRATIONS: &[fn(&mut Config)] = &[migrate_v0_to_v1, migrate_v1_to_v2, migrate_v2_to_v3];

/// 把配置升级到 [`CURRENT_CONFIG_VERSION`]。
///
/// - 版本高于当前支持 → [`ConfigError::VersionTooNew`]（拒绝运行，
///   防止新版本字段被旧程序静默丢弃）；
/// - 版本低于当前 → 逐步应用迁移链；
/// - 已是当前版本 → 原样返回（幂等，load 每次都会调用）。
pub fn migrate(config: &mut Config) -> Result<(), ConfigError> {
    if config.version > CURRENT_CONFIG_VERSION {
        return Err(ConfigError::VersionTooNew {
            found: config.version,
            supported: CURRENT_CONFIG_VERSION,
        });
    }
    while let Some(step) = MIGRATIONS.get(config.version as usize) {
        step(config);
        config.version += 1;
    }
    Ok(())
}

/// v0 → v1：无版本号初版格式的升级步骤。
///
/// v0 缺省字段的历史语义是"未配置"（新装用户手写的最小文件）；升级规则：
/// 1. 空/缺失 `security_code_ref` → 默认钥匙串引用（docs/01 §5 默认形态）；
/// 2. 显式空 `discovery` / `formats` 列表 → 填充默认值（v0 用户未表达过
///    "显式关闭"，这是 v1 起才有的语义；v1 文件 version=1 不会再被本步骤触碰）。
fn migrate_v0_to_v1(config: &mut Config) {
    if config.security_code_ref.trim().is_empty() {
        config.security_code_ref = DEFAULT_SECURITY_CODE_REF.to_owned();
    }
    if config.net.discovery.is_empty() {
        config.net.discovery = vec![DiscoveryMethod::Mdns, DiscoveryMethod::Beacon];
    }
    if config.clipboard.formats.is_empty() {
        config.clipboard.formats = vec![
            ClipboardFormat::Text,
            ClipboardFormat::Image,
            ClipboardFormat::Html,
        ];
    }
    // 其余字段：v0 与 v1 布局一致；缺失节已由 serde 默认值补齐
    // （port 默认 4243、热键默认 docs/03 §4.6 四键等），无需显式迁移。
}

/// v1 → v2：新增 `[general]` 节（NFR-8 日志级别，docs/01 §5）。
///
/// v1 文件无 `[general]`——`general.log_level` 由 serde 默认值补齐为 info，
/// 本步骤仅推进版本号：显式标记 schema 边界，使旧程序读到 v2 文件时报
/// [`ConfigError::VersionTooNew`] 拒绝运行（防新字段被静默丢弃——模块文档
/// 约定），而不是带着 version=1 静默吞掉用户配置的日志级别。
fn migrate_v1_to_v2(_config: &mut Config) {
    // 无需改写字段：缺省值补齐在 serde `#[serde(default)]`（load 解析期）完成。
}

/// v2 → v3：`[[keymap.rules]]` 从 `from`/`to` 简式升级为 trigger/action 四形态
/// （docs/03 §4.1）；并补齐 `passthrough` 直通开关（FR-3.6，serde 默认 false，
/// v2 用户未表达过该开关）。
///
/// 简式归类规则：`from` 解析为修饰键名（[`crate::keynames`] 文法，含 v2 简写
/// `LMeta`/`LCtrl` 与习惯名 `LOpt`/`LWin`）→ 形态 2（`trigger.mod`/`action.mod`）；
/// 否则 → 形态 1（`trigger.key`/`action.key`）。`to = None`（v2 语义 = 丢弃）
/// 转为 `action.drop = true`。解析失败的键名保留原名转形态 1——交给校验层按
/// 字段路径报错（迁移不得静默丢弃用户规则，fail-closed）。trigger 已有内容
/// 却仍带 `from`/`to` 的混合写法不做转换——校验层对遗留字段残留统一报错
/// （迁移只处理纯 v2 形态，不猜测混合意图）。
fn migrate_v2_to_v3(config: &mut Config) {
    for rule in &mut config.keymap.rules {
        if rule.trigger.is_empty() {
            let Some(from) = rule.from.take() else {
                continue;
            };
            let to = rule.to.take();
            rule.trigger = if is_modifier_name(&from) {
                RuleTrigger {
                    mod_: Some(from),
                    ..RuleTrigger::default()
                }
            } else {
                RuleTrigger {
                    key: Some(from),
                    ..RuleTrigger::default()
                }
            };
            rule.action = match to {
                None => RuleAction {
                    drop: Some(true),
                    ..RuleAction::default()
                },
                Some(target) if rule.trigger.mod_.is_some() => RuleAction {
                    mod_: Some(target),
                    ..RuleAction::default()
                },
                Some(target) => RuleAction {
                    key: Some(target),
                    ..RuleAction::default()
                },
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DEFAULT_CLIPBOARD_MAX_BYTES, DEFAULT_PORT, NetConfig, Role};
    use crate::testing::load_str;

    /// 旧 fixture（手写 TOML 文本，PRD 验收项）：v0 形态——无 version 字段、
    /// 无 security_code_ref、显式空 discovery/formats（v0 "未配置"语义）。
    const LEGACY_V0_TOML: &str = r#"
role = "client"

[net]
port = 5000
cidrs = ["192.168.1.0/24"]
discovery = []

[layout]

[keymap]
preset = "none"

[hotkeys]
next = "Ctrl+Alt+Right"

[clipboard]
max_bytes = 1048576
formats = []
"#;

    #[test]
    fn legacy_v0_fixture_upgrades_to_current_version() {
        let mut config: Config = toml::from_str(LEGACY_V0_TOML).unwrap();
        assert_eq!(config.version, 0);

        migrate(&mut config).unwrap();

        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        // 迁移语义 1：缺失 security_code_ref → 默认钥匙串引用
        assert_eq!(config.security_code_ref, "keychain:xcontrol");
        // 迁移语义 2：v0 "未配置"空列表 → v1 默认值
        assert_eq!(
            config.net.discovery,
            vec![DiscoveryMethod::Mdns, DiscoveryMethod::Beacon]
        );
        assert_eq!(
            config.clipboard.formats,
            vec![
                ClipboardFormat::Text,
                ClipboardFormat::Image,
                ClipboardFormat::Html
            ]
        );
        // 未涉及字段原样保留（迁移不得静默改写用户配置）
        assert_eq!(config.role, Role::Client);
        assert_eq!(config.net.port, 5000);
        assert_eq!(config.net.cidrs, vec!["192.168.1.0/24"]);
        assert_eq!(config.clipboard.max_bytes, 1024 * 1024);
        // v2 新增节由 serde 默认补齐（NFR-8 日志级别）
        assert_eq!(
            config.general.log_level,
            crate::model::LogLevel::Info,
            "v0 文件缺 [general]，日志级别应补默认 info"
        );
    }

    /// 走完整 load 流水线的同一 fixture（解析 → 迁移 → 校验）。
    #[test]
    fn legacy_v0_fixture_loads_cleanly() {
        let config = load_str(LEGACY_V0_TOML).unwrap();
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.security_code_ref, DEFAULT_SECURITY_CODE_REF);
        assert_eq!(config.net.port, 5000);
    }

    /// v1 文件升级到 v2：版本推进 + 补 `[general]` 默认值，但 v1 的显式语义
    /// （空列表 = 关闭）必须原样保留——迁移只扩节，不改历史语义。
    #[test]
    fn v1_fixture_upgrades_with_empty_lists_preserved() {
        let raw = "version = 1\n[net]\ndiscovery = []\n[clipboard]\nformats = []\n";
        let mut config: Config = toml::from_str(raw).unwrap();
        migrate(&mut config).unwrap();
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert!(config.net.discovery.is_empty(), "v1 空列表必须保留");
        assert!(config.clipboard.formats.is_empty(), "v1 空列表必须保留");
        assert_eq!(
            config.general.log_level,
            crate::model::LogLevel::Info,
            "v1 文件缺 [general]，日志级别应补默认 info"
        );
    }

    /// v1 文件里显式写了 `[general]`（宽容解析）：值原样保留、版本照常推进
    /// （迁移不覆盖用户显式配置）。
    #[test]
    fn v1_fixture_with_explicit_log_level_keeps_value() {
        let raw = "version = 1\n[general]\nlog_level = \"trace\"\n";
        let mut config: Config = toml::from_str(raw).unwrap();
        migrate(&mut config).unwrap();
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.general.log_level, crate::model::LogLevel::Trace);
    }

    /// 高于当前支持的版本拒绝运行（防新字段被旧程序静默丢弃）。
    #[test]
    fn version_too_new_is_rejected() {
        let mut config: Config = toml::from_str("version = 4\nrole = \"host\"").unwrap();
        let error = migrate(&mut config).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::VersionTooNew {
                found: 4,
                supported: CURRENT_CONFIG_VERSION,
            }
        ));
        // load 流水线同样拒绝
        let error = load_str("version = 99").unwrap_err();
        assert!(matches!(
            error,
            ConfigError::VersionTooNew { found: 99, .. }
        ));
    }

    /// v2 文件的 `from`/`to` 简式规则升级到 v3 trigger/action 形态（任务 11-18-m4-mapping）。
    /// 覆盖：修饰键简式归类（形态 2）、普通键归类（形态 1）、`to = None` 丢弃
    /// （形态 4 drop）、v2 习惯名 `LOpt`/`LWin` 别名、passthrough 缺省补 false。
    #[test]
    fn v2_fixture_rules_convert_to_trigger_action() {
        let raw = r#"
version = 2
[keymap]
preset = "mac2win"
[[keymap.rules]]
from = "LMeta"
to = "LCtrl"
[[keymap.rules]]
from = "LOpt"
to = "LAlt"
[[keymap.rules]]
from = "CapsLock"
to = "Escape"
[[keymap.rules]]
from = "CK_MUTE"
"#;
        let mut config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.version, 2);
        migrate(&mut config).unwrap();
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);

        let rules = &config.keymap.rules;
        assert_eq!(rules.len(), 4, "规则数量不变（迁移不得丢弃）");
        // 修饰键简式 → 形态 2
        assert_eq!(rules[0].trigger.mod_.as_deref(), Some("LMeta"));
        assert_eq!(rules[0].action.mod_.as_deref(), Some("LCtrl"));
        // LOpt（mac 习惯名 = LeftAlt）→ 形态 2
        assert_eq!(rules[1].trigger.mod_.as_deref(), Some("LOpt"));
        assert_eq!(rules[1].action.mod_.as_deref(), Some("LAlt"));
        // 普通键 → 形态 1
        assert_eq!(rules[2].trigger.key.as_deref(), Some("CapsLock"));
        assert_eq!(rules[2].action.key.as_deref(), Some("Escape"));
        // to = None（v2 丢弃语义）→ 形态 4 drop
        assert_eq!(rules[3].trigger.key.as_deref(), Some("CK_MUTE"));
        assert_eq!(rules[3].action.drop, Some(true));
        // 遗留字段清空 + passthrough 缺省补 false
        assert!(
            rules
                .iter()
                .all(|rule| rule.from.is_none() && rule.to.is_none())
        );
        assert!(!config.keymap.passthrough);
        // 迁移后的配置过完整校验（load 流水线）
        let loaded = load_str(raw).unwrap();
        assert_eq!(loaded.keymap.rules, *rules);
    }

    /// v2 规则里解析失败的键名：保留原名转形态 1，交校验层按字段路径报错
    /// （迁移 fail-closed，不静默丢弃）。
    #[test]
    fn v2_rule_with_unknown_name_survives_migration_for_validation() {
        let raw = "version = 2\n[keymap]\n[[keymap.rules]]\nfrom = \"Bogus\"\nto = \"Escape\"\n";
        let mut config: Config = toml::from_str(raw).unwrap();
        migrate(&mut config).unwrap();
        let rule = &config.keymap.rules[0];
        assert_eq!(rule.trigger.key.as_deref(), Some("Bogus"), "原名保留待校验");
        assert_eq!(rule.action.key.as_deref(), Some("Escape"));
        let error = crate::validate::validate(&config).unwrap_err();
        assert!(
            error
                .iter()
                .any(|e| e.path.starts_with("keymap.rules[0]") && e.message.contains("Bogus")),
            "{error:?}"
        );
    }

    /// v3 文件手写 v2 遗留字段（version 已是 3，迁移不触发）：校验拒绝并提示新形态。
    #[test]
    fn v3_file_with_legacy_rule_fields_is_rejected() {
        let raw = "version = 3\n[keymap]\n[[keymap.rules]]\nfrom = \"LMeta\"\nto = \"LCtrl\"\n";
        let error = load_str(raw).unwrap_err();
        assert!(
            error.to_string().contains("from"),
            "应提示 v2 简式不再接受：{error}"
        );
    }

    /// 迁移幂等：重复调用结果一致（load 每次都会跑迁移）。
    #[test]
    fn migrate_is_idempotent() {
        let mut first: Config = toml::from_str(LEGACY_V0_TOML).unwrap();
        migrate(&mut first).unwrap();
        let mut second = first.clone();
        migrate(&mut second).unwrap();
        assert_eq!(first, second);
    }

    /// v0 最小文件（仅 role）：缺失节由 serde 默认补齐，迁移只补版本与语义修正。
    #[test]
    fn minimal_v0_file_upgrade_uses_defaults() {
        let mut minimal: Config = toml::from_str("role = \"host\"").unwrap();
        assert_eq!(minimal.net.port, DEFAULT_PORT);
        assert_eq!(minimal.hotkeys.next, "Ctrl+Alt+Right");
        assert_eq!(minimal.clipboard.max_bytes, DEFAULT_CLIPBOARD_MAX_BYTES);
        assert_eq!(
            minimal.clipboard.scope,
            crate::model::ClipboardScope::Current
        );
        migrate(&mut minimal).unwrap();
        assert_eq!(minimal.version, CURRENT_CONFIG_VERSION);
        assert_eq!(minimal.security_code_ref, DEFAULT_SECURITY_CODE_REF);
        assert_eq!(
            minimal.net.discovery,
            vec![DiscoveryMethod::Mdns, DiscoveryMethod::Beacon]
        );
    }

    /// 扩展点自检：迁移链步数 = 当前版本（未来加 v2 追加步骤后自动成立）。
    #[test]
    fn migration_chain_covers_every_version_step() {
        assert_eq!(MIGRATIONS.len() as u32, CURRENT_CONFIG_VERSION);
    }

    /// NetConfig serde 默认 = Default impl（缺节解析与手写 Default 一致）。
    #[test]
    fn section_serde_defaults_match_default_impl() {
        let parsed: NetConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, NetConfig::default());
    }
}
