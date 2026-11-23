//! 平台标准目录解析（docs/01 §5），`std-paths` feature 门控。
//!
//! `dirs` 的传递依赖（unix `libc` / windows `windows-sys`）在 xtask
//! `FORBIDDEN_PACKAGES` 黑名单内，不得进入依赖 xcontrol-config 的红线 crate
//! （xcontrol-proto / xcontrol-core）全依赖树——故默认关闭，
//! 由 bin（xcontrol-app/cli）按需开启：`features = ["std-paths"]`。
//!
//! `--config` 覆盖：调用方解析 CLI 后把显式路径传给 [`crate::load`]/[`crate::save`]，
//! 本模块只提供"未覆盖时"的默认值。

use std::path::{Path, PathBuf};

/// 日志目录名（NFR-8 数据布局契约：滚动日志统一写 `<data_dir>/logs/`；
/// app 写入与 cli `--status` 读取共用此约定，单一事实源避免双处漂移）。
pub const LOGS_DIR_NAME: &str = "logs";

/// 滚动日志文件名前缀（tracing-appender daily rotation 自动补 `.` 与日期：
/// 实际文件形如 `xcontrol.log.2026-11-18`；清理与 `--status` 日志尾部按
/// 此前缀检索）。文档口径：docs/08 §2"tracing-subscriber fmt + 滚动文件"。
pub const LOG_FILE_PREFIX: &str = "xcontrol.log";

/// 数据目录下的日志目录：`<data_dir>/logs/`（NFR-8 滚动文件写入位置）。
#[must_use]
pub fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(LOGS_DIR_NAME)
}

/// 默认数据目录（安全码 file 回退等，docs/01 §5 / docs/07 §3.3）：
/// mac `~/Library/Application Support/XControl`、
/// win `%APPDATA%\XControl`、
/// linux `$XDG_DATA_HOME|~/.local/share` 下的 `XControl`。
/// 家目录不可得时返回 `None`（调用方应报错并提示显式指定路径）。
pub fn default_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("XControl"))
}

/// 默认配置文件路径：平台配置目录下 `XControl/config.toml`
/// （mac `~/Library/Application Support/`、win `%APPDATA%`、
/// linux `$XDG_CONFIG_HOME|~/.config`）。家目录不可得时返回 `None`。
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("XControl").join("config.toml"))
}

#[cfg(all(test, feature = "std-paths"))]
mod tests {
    use super::*;

    /// 日志目录布局：`<data_dir>/logs/`，前缀 `xcontrol.log`（app/cli 共用契约）。
    #[test]
    fn logs_dir_layout_contract() {
        let data_dir = default_data_dir().unwrap();
        let logs = logs_dir(&data_dir);
        assert_eq!(logs.file_name().unwrap(), LOGS_DIR_NAME);
        assert_eq!(
            logs.parent().unwrap().file_name().unwrap(),
            "XControl",
            "日志目录挂在数据目录下"
        );
    }

    /// 默认配置路径位于 `<config_dir>/XControl/` 下，文件名 `config.toml`。
    #[test]
    fn default_config_path_ends_with_xcontrol_config() {
        let path = default_config_path().unwrap();
        assert_eq!(path.file_name().unwrap(), "config.toml");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "XControl");
    }

    /// 默认数据目录名为 `XControl`（三平台一致的后缀布局）。
    #[test]
    fn default_data_dir_ends_with_xcontrol() {
        let dir = default_data_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "XControl");
    }

    /// 两个默认目录都基于 dirs 解析（家目录不可得时 None，不 panic）。
    #[test]
    fn default_paths_are_absolute_when_available() {
        if let Some(dir) = default_data_dir() {
            assert!(dir.is_absolute(), "{dir:?}");
        }
        if let Some(path) = default_config_path() {
            assert!(path.is_absolute(), "{path:?}");
        }
    }
}
