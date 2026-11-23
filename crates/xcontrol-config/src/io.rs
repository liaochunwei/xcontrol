//! `config.toml` 静态读写（docs/01 §5）。热加载（`ConfigReload`）属核心任务，不在本 crate。
//!
//! 流水线：
//! - [`load`]：读取 → 解析 → 迁移 → 校验；
//! - [`save`]：版本检查 → 校验 → 序列化 → 写入（父目录不存在则创建）。
//!
//! `--config` 覆盖由调用方解析 CLI 后传入显式路径，本 crate 不做参数解析
//! （位置：平台标准目录，`std-paths` feature 的 [`crate::paths`] 可给出默认值）。

use std::fs;
use std::io::Write as _;
use std::path::Path;

use crate::error::ConfigError;
use crate::migrate::{CURRENT_CONFIG_VERSION, migrate};
use crate::model::Config;
use crate::validate::validate;

/// 从 `path` 读取并返回就绪配置（已迁移到当前版本且通过校验）。
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut config: Config = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    migrate(&mut config)?;
    validate(&config).map_err(ConfigError::Validation)?;
    Ok(config)
}

/// 校验后把 `config` 写入 `path`（人类可读的 pretty TOML）。
///
/// 版本必须等于 [`CURRENT_CONFIG_VERSION`]（旧版本先 `migrate`）；
/// 校验在写盘**之前**执行（load/save 双侧执行之一），失败的 `save` 不产生/不改动文件。
///
/// 落盘为**原子替换**（11-18-m4-ui check 收口 M0"save 非原子写"备忘）：写同目录
/// 临时文件（文件名带进程 id，防多进程并发互踩）→ `sync_all` → `rename` 整文件
/// 替换（std `fs::rename` 在 unix 为原子 syscall；Windows 对应
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，同样覆盖既有目标）。进程崩溃/并发
/// 读最多看到旧文件或完整新文件，不会出现半截 TOML。
pub fn save(config: &Config, path: &Path) -> Result<(), ConfigError> {
    if config.version != CURRENT_CONFIG_VERSION {
        return Err(ConfigError::VersionStale {
            found: config.version,
            supported: CURRENT_CONFIG_VERSION,
        });
    }
    validate(config).map_err(ConfigError::Validation)?;
    let text =
        toml::to_string_pretty(config).map_err(|source| ConfigError::Serialize { source })?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    write_atomic(path, &text)
}

/// 原子写：同目录临时文件 + rename 替换（见 [`save`] 文档）。
///
/// 临时文件命名 `<目标文件名>.<pid>.tmp`——同进程串行 save 复用同名临时文件
/// （创建即截断）；失败路径尽力清理临时文件，不残留垃圾。
fn write_atomic(path: &Path, text: &str) -> Result<(), ConfigError> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| ConfigError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("配置路径缺少文件名"),
        })?;
    let tmp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));
    let write_result = (|| -> Result<(), ConfigError> {
        let mut file = fs::File::create(&tmp).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp); // 尽力清理：清理失败不掩盖写盘错误
        return write_result;
    }
    if let Err(source) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp); // 尽力清理：rename 失败不残留临时文件
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::migrate::CURRENT_CONFIG_VERSION;
    use crate::testing::TempDir;

    fn config_path(dir: &TempDir) -> PathBuf {
        dir.path().join("config").join("config.toml")
    }

    #[test]
    fn save_then_load_round_trips_through_filesystem() {
        let dir = TempDir::new("io-round-trip");
        let path = config_path(&dir);
        let original = Config::default();

        save(&original, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = TempDir::new("io-parent-dirs");
        let path = config_path(&dir);
        assert!(!path.parent().unwrap().exists());
        save(&Config::default(), &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn load_missing_file_is_io_error_with_path() {
        let dir = TempDir::new("io-missing");
        let path = dir.path().join("no-such-config.toml");
        let error = load(&path).unwrap_err();
        // Display 含路径（非敏感信息，用户可定位）
        let rendered = error.to_string();
        assert!(rendered.contains("no-such-config.toml"), "{rendered}");
        match error {
            ConfigError::Io {
                path: error_path,
                source,
            } => {
                assert_eq!(error_path, path);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("应为 Io 变体：{other:?}"),
        }
    }

    #[test]
    fn load_malformed_toml_is_parse_error_with_path() {
        let dir = TempDir::new("io-parse-error");
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "role = [unclosed").unwrap();
        let error = load(&path).unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");
        assert!(error.to_string().contains("broken.toml"));
    }

    /// save 拒绝非法配置且不落盘（校验在写盘之前）。
    #[test]
    fn save_invalid_config_writes_nothing() {
        let dir = TempDir::new("io-invalid-save");
        let path = config_path(&dir);
        let mut config = Config::default();
        config.net.port = 0;
        let error = save(&config, &path).unwrap_err();
        let ConfigError::Validation(errors) = error else {
            panic!("应为 Validation 变体：{error:?}")
        };
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "net.port");
        assert!(!path.exists(), "校验失败不得写盘");
    }

    /// save 原子替换（11-18-m4-ui check 收口 M0"save 非原子写"备忘）：重复 save
    /// 覆盖生效、目标目录不残留 `.tmp` 临时文件（失败路径清理 + 成功路径 rename）。
    #[test]
    fn save_is_atomic_replace_without_tmp_residue() {
        let dir = TempDir::new("io-atomic-save");
        let path = config_path(&dir);

        let mut first = Config::default();
        first.general.log_level = crate::LogLevel::Warn;
        save(&first, &path).unwrap();
        assert_eq!(
            load(&path).unwrap().general.log_level,
            crate::LogLevel::Warn
        );

        // 重复 save（M4 设置页高频落盘路径）：整文件替换、无半截内容。
        let mut second = Config::default();
        second.general.log_level = crate::LogLevel::Debug;
        second.net.port = 4300;
        save(&second, &path).unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.general.log_level, crate::LogLevel::Debug);
        assert_eq!(reloaded.net.port, 4300);

        // 无临时文件残留（成功路径 rename 消费、失败路径尽力清理）。
        let leftovers: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            vec!["config.toml".to_owned()],
            "目标目录只应剩最终文件：{leftovers:?}"
        );
    }

    /// save 拒绝落后版本（旧配置必须先 migrate）。
    #[test]
    fn save_rejects_stale_version() {
        let dir = TempDir::new("io-stale-version");
        let path = dir.path().join("config.toml");
        let config = Config {
            version: 0,
            ..Config::default()
        };
        let error = save(&config, &path).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::VersionStale {
                found: 0,
                supported: CURRENT_CONFIG_VERSION,
            }
        ));
        assert!(!path.exists());
    }

    /// load 对高版本配置拒绝（VersionTooNew 优先于校验）。
    #[test]
    fn load_rejects_future_version() {
        let dir = TempDir::new("io-future-version");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 42\nrole = \"host\"\n").unwrap();
        let error = load(&path).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::VersionTooNew {
                found: 42,
                supported: CURRENT_CONFIG_VERSION,
            }
        ));
    }

    /// load 对校验失败返回 Validation（含字段路径）。
    #[test]
    fn load_rejects_invalid_values_with_validation_error() {
        let dir = TempDir::new("io-validation");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n[net]\nport = 0\n").unwrap();
        let error = load(&path).unwrap_err();
        let ConfigError::Validation(errors) = error else {
            panic!("应为 Validation 变体：{error:?}")
        };
        assert!(errors.iter().any(|e| e.path == "net.port"));
    }
}
