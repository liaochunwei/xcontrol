//! 安全码存取（docs/07 §3.3）：[`SecretStore`] trait + file / keychain 两实现。
//!
//! 红线（`.trellis/spec/rust-core/error-handling.md` 红线 6、AGENTS.md 禁止事项）：
//! - [`SecretCode`] 手写 `Debug` 输出 `[REDACTED]`，且**不实现 `Display`**——
//!   任何 `{}` 格式化都是编译错误，从机制上杜绝明文进日志/错误信息；
//! - [`SecretRef`] 只是"指向安全码的引用"（条目名/文件路径），可安全回显。
//!
//! keyring 真接线（11-19-m6-security，`keychain` feature 门控）：
//! [`KeychainSecretStore`] 经 `keyring` crate 统一三平台——mac Keychain
//! （security-framework）/ win Credential Manager（windows-sys）/ linux Secret
//! Service（secret-service + zbus 纯 Rust，async-io 执行器）。feature 关闭时
//! 保持占位行为（[`SecretStoreError::KeychainUnavailable`]）——proto/core/
//! 最小编译面零平台依赖；查树与选型裁定见根 workspace.dependencies `keyring`
//! 条目与 docs/07 §3.3。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use crate::error::SecretStoreError;

/// 默认安全码引用（docs/01 §5：keychain 默认条目）。
pub const DEFAULT_SECURITY_CODE_REF: &str = "keychain:xcontrol";

/// file 后端存储文件在 `data_dir` 下的默认位置。
pub const DEFAULT_SECRET_FILE_RELATIVE: &str = "secrets/security_code";

/// 安全码包装类型：内容不可通过 `Debug`/`Display` 泄露。
#[derive(Clone, PartialEq, Eq)]
pub struct SecretCode(String);

impl SecretCode {
    /// 包装明文安全码。
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    /// 显式取用明文（仅限 PSK 派生/握手等必要路径；调用方不得写入日志）。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 红线：Debug 一律脱敏（error-handling.md 红线 6）。
        f.write_str("SecretCode([REDACTED])")
    }
}

/// 安全码引用（`security_code_ref` 的结构化形态，docs/01 §5 / docs/07 §3.3）：
/// `keychain:<条目名>`（默认）或 `file:<路径>`（钥匙串不可用时的回退，0600）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// OS 钥匙串条目名（mac Keychain / win Credential Manager / linux Secret Service）。
    Keychain {
        /// 钥匙串条目名。
        name: String,
    },
    /// 文件路径：绝对路径，或相对 `data_dir` 的相对路径（[`build_secret_store`] 解析）。
    File {
        /// 安全码文件路径。
        path: PathBuf,
    },
}

impl SecretRef {
    /// 解析 `security_code_ref` 字符串；格式非法返回 `None`
    /// （配置侧由 [`crate::validate`] 给出含字段路径的中文错误）。
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (scheme, value) = raw.split_once(':')?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match scheme.trim() {
            "keychain" => Some(Self::Keychain {
                name: value.to_owned(),
            }),
            "file" => Some(Self::File {
                path: PathBuf::from(value),
            }),
            _ => None,
        }
    }

    /// 序列化回配置文件字符串形态（与 docs/01 §5 示例一致）。
    pub fn to_config_value(&self) -> String {
        match self {
            Self::Keychain { name } => format!("keychain:{name}"),
            Self::File { path } => format!("file:{}", path.display()),
        }
    }
}

/// 安全码存取抽象（docs/07 §3.3）。实现方必须保证明文不进任何错误信息/日志。
pub trait SecretStore {
    /// 写入（覆盖既有值）。
    fn store(&self, secret: &SecretCode) -> Result<(), SecretStoreError>;
    /// 读取；未存储时返回 [`SecretStoreError::NotFound`]。
    fn retrieve(&self) -> Result<SecretCode, SecretStoreError>;
    /// 删除；本就不存在时视为成功（幂等）。
    fn delete(&self) -> Result<(), SecretStoreError>;
}

/// 按 [`SecretRef`] 构建对应后端；`file:` 相对路径相对 `data_dir` 解析。
pub fn build_secret_store(secret_ref: &SecretRef, data_dir: &Path) -> Box<dyn SecretStore> {
    match secret_ref {
        SecretRef::Keychain { name } => Box::new(KeychainSecretStore::new(name.clone())),
        SecretRef::File { path } => {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                data_dir.join(path)
            };
            Box::new(FileSecretStore::new(resolved))
        }
    }
}

/// keychain 后端：OS 钥匙串统一存取（docs/07 §3.3 三平台语义；`keychain`
/// feature 经 keyring crate 落地——mac Keychain / win Credential Manager /
/// linux Secret Service，后端由 keyring 按 target 自选）。
///
/// 映射（keyring 的 (service, user) 二元组 → 配置 `keychain:<名称>`）：
/// service = 配置条目名（默认 `xcontrol`）、user = 固定
/// [`KEYCHAIN_USER`]——同一 service 下的唯一条目，第三方钥匙串工具可见
/// （label 为 keyring 默认，非秘密）。
///
/// 错误映射：`NoEntry` → [`SecretStoreError::NotFound`]（未存储语义，探测
/// 可用性时是"健康"结果）；其余（无 DBus 会话/存储锁定/平台错误）→
/// [`SecretStoreError::KeychainBackend`]（描述不含秘密内容）——调用方按
/// docs/07 §3.3 引导 file: 回退。
///
/// feature 关闭（默认编译面）时退回占位：所有操作返回
/// [`SecretStoreError::KeychainUnavailable`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainSecretStore {
    name: String,
}

/// keyring 条目的固定 user 段（service/user 二元组中的 user；非秘密）。
/// 仅 `keychain` feature 消费（SecretStore impl 同门控）——feature 关闭时
/// 不编译（clippy -D warnings 的 dead_code 面不放行占位常量）。
#[cfg(feature = "keychain")]
pub const KEYCHAIN_USER: &str = "security-code";

impl KeychainSecretStore {
    /// 指定钥匙串条目名。
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// 钥匙串条目名（非秘密，可回显）。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 钥匙串后端可用性探测（UI 向导/设置页选后端用）：`retrieve` 返回
    /// `NotFound`（条目未存——后端健康）或成功 → 可用；平台级失败
    /// （无 DBus 会话等）→ 不可用。feature 关闭恒 `false`。
    #[must_use]
    pub fn is_available(&self) -> bool {
        match self.retrieve() {
            Ok(_) | Err(SecretStoreError::NotFound) => true,
            Err(_) => false,
        }
    }
}

/// keyring 平台错误 → [`SecretStoreError`]（`NoEntry` 是"未存储"，非故障）。
#[cfg(feature = "keychain")]
fn map_keyring_error(error: keyring::Error) -> SecretStoreError {
    match error {
        keyring::Error::NoEntry => SecretStoreError::NotFound,
        other => SecretStoreError::KeychainBackend(other.to_string()),
    }
}

#[cfg(feature = "keychain")]
impl SecretStore for KeychainSecretStore {
    fn store(&self, secret: &SecretCode) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(&self.name, KEYCHAIN_USER).map_err(map_keyring_error)?;
        entry
            .set_password(secret.expose())
            .map_err(map_keyring_error)
    }

    fn retrieve(&self) -> Result<SecretCode, SecretStoreError> {
        let entry = keyring::Entry::new(&self.name, KEYCHAIN_USER).map_err(map_keyring_error)?;
        let password = entry.get_password().map_err(map_keyring_error)?;
        let trimmed = password.trim().to_owned();
        if trimmed.is_empty() {
            return Err(SecretStoreError::NotFound);
        }
        Ok(SecretCode::new(trimmed))
    }

    fn delete(&self) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(&self.name, KEYCHAIN_USER).map_err(map_keyring_error)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // 本就不存在视为成功（幂等，与 FileSecretStore::delete 同语义）。
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(map_keyring_error(error)),
        }
    }
}

#[cfg(not(feature = "keychain"))]
impl SecretStore for KeychainSecretStore {
    fn store(&self, _secret: &SecretCode) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::KeychainUnavailable)
    }

    fn retrieve(&self) -> Result<SecretCode, SecretStoreError> {
        Err(SecretStoreError::KeychainUnavailable)
    }

    fn delete(&self) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::KeychainUnavailable)
    }
}

/// file 后端：安全码明文落盘（docs/07 §3.3 回退模式）。
///
/// 权限策略（docs/01 §5 的 0600 强制要求）：
/// - unix：文件 `0600`（创建时经 `OpenOptionsExt::mode` 生效，避免"先建文件再收紧"
///   的竞态窗口；预存在文件再统一 `set_permissions` 收紧）、父目录 `0700`；
/// - windows：`std` 无 ACL API，不额外收紧——依赖用户 Profile 目录默认 ACL
///   （仅当前用户可读）。局限在发布前按 docs/05 §7 平台清单复核；
///   此处用 `#[cfg(unix)]`（std 层门控），不引入平台 feature
///   （architecture-rules 红线 2 的"公共代码禁止平台判断"针对 feature 门控层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    /// 指向存储文件路径（默认布局见 [`DEFAULT_SECRET_FILE_RELATIVE`]）。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 存储文件路径（非秘密，可回显）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 确保父目录存在；unix 下收紧到 0700（目录可遍历即等于可读文件名）。
    fn ensure_parent_dir(&self) -> Result<(), SecretStoreError> {
        let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) else {
            return Ok(());
        };
        fs::create_dir_all(parent).map_err(|source| SecretStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
                SecretStoreError::Io {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }
        Ok(())
    }

    /// 把 IO 错误包装为 [`SecretStoreError::Io`]（携带路径，不带文件内容）。
    fn io_error(&self, source: std::io::Error) -> SecretStoreError {
        SecretStoreError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

impl SecretStore for FileSecretStore {
    fn store(&self, secret: &SecretCode) -> Result<(), SecretStoreError> {
        self.ensure_parent_dir()?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 0600 必须在创建时生效（先建后收紧存在明文可读的竞态窗口）。
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|source| self.io_error(source))?;
        file.write_all(secret.expose().as_bytes())
            .map_err(|source| self.io_error(source))?;
        #[cfg(unix)]
        {
            // mode 只影响新建文件；预存在文件在此统一收紧。
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|source| self.io_error(source))?;
        }
        Ok(())
    }

    fn retrieve(&self) -> Result<SecretCode, SecretStoreError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(SecretStoreError::NotFound);
            }
            Err(source) => return Err(self.io_error(source)),
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SecretStoreError::NotFound);
        }
        Ok(SecretCode::new(trimmed))
    }

    fn delete(&self) -> Result<(), SecretStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(self.io_error(source)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn secret_code_debug_is_redacted() {
        let secret = SecretCode::new("test-code-6242-not-a-real-secret");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "SecretCode([REDACTED])");
        assert!(!rendered.contains("test-code"));
        // 嵌套容器（日志常见路径）同样脱敏
        let rendered = format!("{:?}", vec![&secret]);
        assert!(!rendered.contains("test-code"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn secret_ref_parses_both_schemes() {
        assert_eq!(
            SecretRef::parse("keychain:xcontrol"),
            Some(SecretRef::Keychain {
                name: "xcontrol".to_owned()
            })
        );
        assert_eq!(
            SecretRef::parse("file:/abs/path/code"),
            Some(SecretRef::File {
                path: PathBuf::from("/abs/path/code")
            })
        );
        assert_eq!(
            SecretRef::parse("file:secrets/code"),
            Some(SecretRef::File {
                path: PathBuf::from("secrets/code")
            })
        );
        // 宽容空白
        assert_eq!(
            SecretRef::parse("  keychain:xcontrol  "),
            Some(SecretRef::Keychain {
                name: "xcontrol".to_owned()
            })
        );
    }

    #[test]
    fn secret_ref_rejects_malformed_values() {
        assert_eq!(SecretRef::parse(""), None);
        assert_eq!(SecretRef::parse("bogus"), None);
        assert_eq!(SecretRef::parse("keychain:"), None);
        assert_eq!(SecretRef::parse("file:"), None);
        assert_eq!(SecretRef::parse("ftp:host"), None);
        assert_eq!(SecretRef::parse(":value"), None);
    }

    #[test]
    fn secret_ref_serializes_back_to_config_value() {
        for raw in ["keychain:xcontrol", "file:secrets/code", "file:/abs/code"] {
            let parsed = SecretRef::parse(raw).unwrap();
            assert_eq!(parsed.to_config_value(), raw);
        }
    }

    /// 占位编译面（`keychain` feature 关闭）：钥匙串操作显式不可用——
    /// 调用方引导 file: 回退。真实现行为由 `keychain_live_round_trip`
    /// （feature 面 + #[ignore] 本机实测）承载。
    #[test]
    #[cfg(not(feature = "keychain"))]
    fn keychain_placeholder_is_explicitly_unavailable() {
        let store = KeychainSecretStore::new("xcontrol");
        assert_eq!(store.name(), "xcontrol");
        assert!(!store.is_available(), "占位编译面探测恒不可用");
        let secret = SecretCode::new("test-code-not-a-real-secret");
        assert!(matches!(
            store.store(&secret),
            Err(SecretStoreError::KeychainUnavailable)
        ));
        assert!(matches!(
            store.retrieve(),
            Err(SecretStoreError::KeychainUnavailable)
        ));
        assert!(matches!(
            store.delete(),
            Err(SecretStoreError::KeychainUnavailable)
        ));
    }

    #[test]
    fn file_store_round_trip_store_retrieve_delete() {
        let dir = TempDir::new("secret-round-trip");
        let path = dir.path().join("nested").join("code");
        let store = FileSecretStore::new(path.clone());
        let secret = SecretCode::new("test-code-not-a-real-secret");

        // 未存储 → NotFound
        assert!(matches!(store.retrieve(), Err(SecretStoreError::NotFound)));

        store.store(&secret).unwrap();
        let retrieved = store.retrieve().unwrap();
        assert_eq!(retrieved, secret);
        assert_eq!(retrieved.expose(), secret.expose());

        // 覆盖写
        let rotated = SecretCode::new("test-code-rotated");
        store.store(&rotated).unwrap();
        assert_eq!(store.retrieve().unwrap(), rotated);

        store.delete().unwrap();
        assert!(matches!(store.retrieve(), Err(SecretStoreError::NotFound)));
        // 幂等
        store.delete().unwrap();
    }

    /// 文件内容直接落明文（后端定位），但权限必须 0600 / 目录 0700（unix，docs/01 §5）。
    #[cfg(unix)]
    #[test]
    fn file_store_enforces_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("secret-permissions");
        let path = dir.path().join("secrets").join("security_code");
        let store = FileSecretStore::new(path.clone());
        store
            .store(&SecretCode::new("test-code-not-a-real-secret"))
            .unwrap();

        let mode = path.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "安全码文件必须 0600");
        let dir_mode = path
            .parent()
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "安全码目录必须 0700");

        // 预存在的宽松权限文件被收紧（mode 只影响新建，这里验证 set_permissions 兜底）
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        store.store(&SecretCode::new("test-code-rotated")).unwrap();
        let mode = path.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "预存在文件也应被收紧到 0600");
    }

    /// 空文件按"未存储"处理（retrieve 不得返回空明文）。
    #[test]
    fn file_store_treats_empty_file_as_not_found() {
        let dir = TempDir::new("secret-empty-file");
        let path = dir.path().join("code");
        let store = FileSecretStore::new(path.clone());
        std::fs::write(&path, "   \n").unwrap();
        assert!(matches!(store.retrieve(), Err(SecretStoreError::NotFound)));
    }

    /// build_secret_store 路由：keychain → 钥匙串后端；file 相对路径相对 data_dir 解析。
    /// （keychain 分支只断言路由类型面——真实现行为见 `keychain_live_round_trip`。）
    #[test]
    fn build_secret_store_routes_by_prefix() {
        let data_dir = Path::new("/data");

        // keychain 分支：占位编译面下显式不可用（assert 占位错误）；keychain
        // feature 面下**不做任何钥匙串 I/O**——`cargo test --workspace` 的
        // feature 统一会启用本 feature（xcontrol-app 依赖），此处若 store()/retrieve()
        // 真实条目 `xcontrol` 将：① 撞 ACL 弹窗挂起测试（mac 实测复现），
        // ② 无既有条目时静默写入测试码（污染 app 生产条目）。真实行为由
        // `#[ignore]` 的 `keychain_live_round_trip`（pid 唯一测试条目 + 自清理）
        // 显式运行承载——两种面都只断言"路由可达"，不触碰钥匙串。
        let keychain_ref = SecretRef::parse("keychain:xcontrol").unwrap();
        let store = build_secret_store(&keychain_ref, data_dir);
        let secret = SecretCode::new("test-code-not-a-real-secret");
        #[cfg(not(feature = "keychain"))]
        assert!(matches!(
            store.store(&secret),
            Err(SecretStoreError::KeychainUnavailable)
        ));
        #[cfg(feature = "keychain")]
        let _ = &store;

        // file 相对路径 → 在 data_dir 下落盘（用真实临时目录验证落点）
        let dir = TempDir::new("secret-build-route");
        let file_ref = SecretRef::parse("file:secrets/code").unwrap();
        let store = build_secret_store(&file_ref, dir.path());
        store.store(&secret).unwrap();
        assert!(
            dir.path().join("secrets/code").exists(),
            "file: 相对路径必须相对 data_dir 解析"
        );
        assert_eq!(store.retrieve().unwrap(), secret);
    }

    /// **mac/平台本机实测**（`keychain` feature 面，`#[ignore]`——CI 不触碰
    /// 真实钥匙串；本机以 `cargo test -p xcontrol-config --features keychain
    /// -- --ignored keychain_live` 显式运行，docs/07 §8 第 4 项证据）：
    ///
    /// 唯一 service（含 pid 防撞）下 store → retrieve → 覆盖写 → delete →
    /// NotFound 幂等 delete 全链路 + 与 file 后端对照（同一码两侧一致）。
    /// 条目名带进程号，测试自清理（失败残留对钥匙串无害——名称唯一可手删）。
    #[test]
    #[cfg(feature = "keychain")]
    #[ignore = "本机钥匙串实测：写入/读取/删除真实 OS 钥匙串条目（勿进 CI）"]
    fn keychain_live_round_trip() {
        let service = format!("xcontrol-it-{}", std::process::id());
        let store = KeychainSecretStore::new(&service);
        let secret = SecretCode::new("test-code-keychain-live-not-a-real-secret");

        // 探测可用性（mac login keychain / 有 DBus 会话的 linux / win 凭据管理器）。
        assert!(store.is_available(), "钥匙串后端应可用（本机实测前提）");

        // 未存储 → NotFound。
        assert!(matches!(store.retrieve(), Err(SecretStoreError::NotFound)));

        // 写入 → 读取一致。
        store.store(&secret).expect("钥匙串写入");
        let retrieved = store.retrieve().expect("钥匙串读取");
        assert_eq!(retrieved.expose(), secret.expose());

        // 覆盖写（换码路径的核心形态）。
        let rotated = SecretCode::new("test-code-keychain-live-rotated");
        store.store(&rotated).expect("钥匙串覆盖写");
        assert_eq!(store.retrieve().expect("再读").expose(), rotated.expose());

        // 对照：同一码经 file 后端往返一致（两后端语义等价，docs/07 §3.3）。
        let dir = TempDir::new("secret-keychain-compare");
        let file_store = FileSecretStore::new(dir.path().join("code"));
        file_store.store(&rotated).expect("file 写入");
        assert_eq!(
            file_store.retrieve().expect("file 读取").expose(),
            rotated.expose(),
            "keychain 与 file 后端存取结果一致"
        );

        // 删除 → NotFound；幂等。
        store.delete().expect("钥匙串删除");
        assert!(matches!(store.retrieve(), Err(SecretStoreError::NotFound)));
        store.delete().expect("幂等删除");
    }
}
