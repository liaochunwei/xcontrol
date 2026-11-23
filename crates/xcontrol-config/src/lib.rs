//! `xcontrol-config` — 配置模型：`config.toml` 的模型/迁移/校验 + 安全码存取（`SecretStore`）。
//!
//! 对应 docs/01 §5：
//! - schema 顶层 `version` 字段 + 迁移函数链（[`migrate`]，当前版本 [`CURRENT_CONFIG_VERSION`]）；
//! - 平台标准目录解析（`std-paths` feature：[`default_config_path`] / [`default_data_dir`]）；
//! - 安全码默认存 OS 钥匙串，回退文件强制 0600（unix）+ UI 警告（docs/07 §3.3）；
//!   keyring 真接线（11-19-m6-security）：`keychain` feature 启用三平台后端
//!   （mac Keychain / win Credential Manager / linux Secret Service——查树裁定
//!   见根 workspace.dependencies `keyring` 条目）；feature 关闭时为占位后端
//!   （[`KeychainSecretStore`] 返回明确错误），红线树零增量；
//! - 热加载（`ConfigReload`）延后到核心任务，本 crate 只做静态 [`load`]/[`save`]。
//!
//! I/O 边界：本 crate 是**配置 I/O 边界**——仅 `std::fs` 同步读写，无网络、无 tokio；
//! 依赖它的 xcontrol-proto / xcontrol-core 保持零 I/O 红线（docs/08 §1，
//! `cargo run -p xtask -- check-deps` 机械断言）。`dirs` 为可选依赖
//! （`std-paths` feature），保证其传递依赖不进入红线 crate 依赖树。
//!
//! # 用法
//!
//! ```no_run
//! use std::path::Path;
//!
//! // `--config` 覆盖由调用方解析 CLI 后传入；无覆盖时可用
//! // xcontrol_config::default_config_path()（需 std-paths feature）取默认位置。
//! let path = Path::new("config.toml");
//!
//! // 读取（解析 → 迁移 → 校验一条龙）；首启无文件时以默认配置开始。
//! // 其余错误（解析/校验/版本）必须上抛给用户——禁止静默回退默认值
//! // （那会无声丢弃用户配置，如把拼错的字段悄悄重置）。
//! let mut config = match xcontrol_config::load(path) {
//!     Ok(config) => config,
//!     Err(xcontrol_config::ConfigError::Io { source, .. })
//!         if source.kind() == std::io::ErrorKind::NotFound =>
//!     {
//!         xcontrol_config::Config::default()
//!     }
//!     Err(error) => {
//!         eprintln!("配置加载失败：{error}");
//!         return;
//!     }
//! };
//! config.role = xcontrol_config::Role::Host;
//!
//! // 写入（版本检查 → 校验 → pretty TOML）
//! xcontrol_config::save(&config, path).unwrap();
//! ```
//!
//! # 错误处理约定
//!
//! 所有错误经 [`ConfigError`]（thiserror，变体可编程区分、保留 `#[source]` 链）；
//! 安全码明文由 [`SecretCode`] 包装——`Debug` 恒为 `[REDACTED]` 且无 `Display`，
//! 从机制上杜绝明文进日志/错误信息（`.trellis/spec/rust-core/error-handling.md`）。
#![forbid(unsafe_code)]

mod error;
mod io;
mod keynames;
mod migrate;
mod model;
#[cfg(feature = "std-paths")]
mod paths;
mod secret;
mod validate;

pub use error::{ConfigError, SecretStoreError, ValidationError};
// 注意：keychain feature 未启用时 KeychainSecretStore 为占位实现；启用后为
// keyring 三平台真实现（store/retrieve/delete + is_available 探测）。
pub use io::{load, save};
pub use keynames::{is_modifier_name, parse_key_name, raw_is_modifier};
pub use migrate::{CURRENT_CONFIG_VERSION, migrate};
pub use model::{
    ClipboardConfig, ClipboardFormat, ClipboardScope, Config, DEFAULT_CLIPBOARD_MAX_BYTES,
    DEFAULT_HOTKEY_BACK_TO_HOST, DEFAULT_HOTKEY_NEXT, DEFAULT_HOTKEY_PAUSE, DEFAULT_HOTKEY_PREV,
    DEFAULT_LANGUAGE, DEFAULT_PORT, Direction, DiscoveryMethod, GeneralConfig, HotkeysConfig,
    KEYMAP_MAX_RULES, KEYMAP_PRESETS, KeymapConfig, KeymapRule, LANGUAGE_VARIANTS, LayoutConfig,
    LayoutScreen, LogLevel, MAX_CLIPBOARD_BYTES_LIMIT, NetConfig, Role, RuleAction, RuleTrigger,
};
#[cfg(feature = "std-paths")]
pub use paths::{LOG_FILE_PREFIX, LOGS_DIR_NAME, default_config_path, default_data_dir, logs_dir};
pub use secret::{
    DEFAULT_SECRET_FILE_RELATIVE, DEFAULT_SECURITY_CODE_REF, FileSecretStore, KeychainSecretStore,
    SecretCode, SecretRef, SecretStore, build_secret_store,
};

/// 测试公共设施（仅 `cargo test` 编译）：临时目录守卫 + 文本版 load 流水线。
#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::error::ConfigError;
    use crate::migrate::migrate;
    use crate::model::Config;
    use crate::validate::validate;

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// 独立临时目录；Drop 时清理（断言失败也不残留安全码测试数据）。
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "xcontrol-config-test-{}-{}-{}",
                tag,
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 从 TOML 文本走完整 load 流水线（解析 → 迁移 → 校验），供迁移/校验测试复用。
    pub(crate) fn load_str(raw: &str) -> Result<Config, ConfigError> {
        let mut config: Config = toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<text>"),
            source,
        })?;
        migrate(&mut config)?;
        validate(&config).map_err(ConfigError::Validation)?;
        Ok(config)
    }
}
