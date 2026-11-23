//! 错误类型（docs/08 §2：库 crate 用 `thiserror` 定义错误枚举，变体携带可编程区分的语义）。
//!
//! 红线（`.trellis/spec/rust-core/error-handling.md`）：
//! - 错误信息禁止包含安全码明文（`SecretCode` 的 Debug/Display 已在 [`crate::secret`] 脱敏）；
//!   字段路径、端口、CIDR、热键文本、存储路径均非敏感信息，可回显；
//! - 错误链保留源错误（`#[source]`），bin 顶层可用 anyhow 完整展开。

use std::path::PathBuf;

/// 单条配置校验错误：字段路径 + 面向用户的中文提示（PRD 验收项）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct ValidationError {
    /// 字段路径（TOML 键路径，如 `net.port`、`hotkeys.next`、`layout.screens[0].id`）。
    pub path: String,
    /// 中文提示（可回显当前值；被校验字段均非敏感信息）。
    pub message: String,
}

impl ValidationError {
    pub(crate) fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// 配置读写全流程错误（IO / 解析 / 序列化 / 校验 / 迁移 / SecretStore 六类）。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 读取配置文件失败（文件不存在、权限不足等）。
    #[error("读取配置文件失败（{path}）")]
    Io {
        /// 配置文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },
    /// TOML 解析失败（语法错误、类型不匹配、未知枚举值等，含 toml 原始错误链）。
    #[error("配置文件解析失败（{path}）：{source}")]
    Parse {
        /// 配置文件路径。
        path: PathBuf,
        /// toml 解析错误（保留原始定位信息）。
        #[source]
        source: toml::de::Error,
    },
    /// 序列化失败（合法 `Config` 理论上不会触发；防御性保留错误分类）。
    #[error("配置文件序列化失败：{source}")]
    Serialize {
        /// toml 序列化错误。
        #[source]
        source: toml::ser::Error,
    },
    /// 校验失败（携带**全部**问题，一次修复多项；Display 逐条列出）。
    #[error("配置校验失败：{}", format_validation_errors(.0))]
    Validation(Vec<ValidationError>),
    /// 迁移：配置 schema 版本高于当前支持（拒绝运行，防止未知字段静默丢失）。
    #[error("配置 schema 版本 {found} 高于当前支持版本 {supported}，请升级 XControl")]
    VersionTooNew {
        /// 配置文件中的版本。
        found: u32,
        /// 本程序支持的版本。
        supported: u32,
    },
    /// 保存：配置版本落后于当前 schema（应先 `migrate` 再 `save`）。
    #[error("配置 schema 版本 {found} 低于当前支持版本 {supported}，请先调用 migrate() 升级")]
    VersionStale {
        /// 配置中的版本。
        found: u32,
        /// 本程序支持的版本。
        supported: u32,
    },
    /// 安全码存取失败（`SecretStore`）。
    #[error("安全码存取失败：{0}")]
    Secret(#[from] SecretStoreError),
}

/// 把多条校验错误拼成单行 Display（`；` 分隔）。
fn format_validation_errors(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("；")
}

/// `SecretStore` 操作错误（Display/Debug 不得包含安全码明文；存储路径可回显）。
#[derive(Debug, thiserror::Error)]
pub enum SecretStoreError {
    /// keychain feature 未启用的编译面（proto/core/最小构建）：钥匙串后端为
    /// 占位——一律返回此错误（启用 `keychain` feature 即真实现，docs/07 §3.3）。
    #[error("本构建未启用系统钥匙串后端，请改用 file: 回退模式（docs/07 §3.3）")]
    KeychainUnavailable,
    /// 钥匙串后端运行时失败（无 DBus 会话/存储锁定/平台错误等——keychain
    /// feature 面）。描述来自平台错误链，**不含秘密内容**（keyring 错误只在
    /// 属性/状态层取词）；调用方按 docs/07 §3.3 引导 file: 回退。
    #[error("系统钥匙串操作失败：{0}")]
    KeychainBackend(String),
    /// 安全码尚未存储。
    #[error("安全码尚未存储")]
    NotFound,
    /// 安全码引用格式非法（引用是"指向安全码的指针"，回显不泄露明文）。
    #[error("安全码引用格式非法（应为 keychain:<名称> 或 file:<路径>）：{0}")]
    InvalidRef(String),
    /// 文件后端 IO 失败。
    #[error("安全码文件读写失败（{path}）")]
    Io {
        /// 安全码文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },
}
