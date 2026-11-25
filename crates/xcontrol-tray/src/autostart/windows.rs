//! HKCU Run 键自启后端（Windows，docs/05 §2 自启行、FR-6.1）。
//!
//! 键路径 `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`，
//! 值名 `XControl`（[`super::RUN_SUBKEY`] / [`super::RUN_VALUE_NAME`]），值 =
//! 当前可执行文件路径（含空格按 CreateProcess 约定加引号）。
//!
//! 经 `windows-registry`（微软官方 safe 封装）读写——本 crate 保持
//! `#![forbid(unsafe_code)]`，无需平台绑定层 unsafe 放行。

use windows_registry::CURRENT_USER;
use windows_result::Error;

use super::{Autostart, AutostartError, RUN_SUBKEY, RUN_VALUE_NAME, run_value_command};

/// HKCU Run 键自启代理（无状态——键路径/值名为常量）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RunKeyAgent;

impl RunKeyAgent {
    /// 生产构造（无状态）。
    #[must_use]
    pub fn current() -> Self {
        Self
    }
}

/// windows_registry 错误 → [`AutostartError::Backend`]（`op` 标识操作，
/// 平台错误文本进 `detail`——可编程区分 + 上下文保留）。错误本体 =
/// [`Error`]（`windows_result::Error`——`windows_registry::Error` 在 0.6 不可
/// 见，E0603，PR #1 windows job 实测；`pub use windows_result::Result` 未带
/// `Error`）。
fn registry_error(op: &'static str, error: Error) -> AutostartError {
    AutostartError::Backend {
        op,
        detail: error.to_string(),
    }
}

impl Autostart for RunKeyAgent {
    fn enable(&self) -> Result<(), AutostartError> {
        let program =
            std::env::current_exe().map_err(|source| AutostartError::CurrentExe { source })?;
        let key = CURRENT_USER
            .create(RUN_SUBKEY)
            .map_err(|error| registry_error("registry create", error))?;
        key.set_string(RUN_VALUE_NAME, run_value_command(&program))
            .map_err(|error| registry_error("registry set_string", error))
    }

    fn disable(&self) -> Result<(), AutostartError> {
        // Run 子键不存在 = 未注册（幂等目标态已达成）；存在则删值。
        // open 失败按"未注册"处理（HKCU per-user，Run 子键仅缺失去失败）。
        let key = match CURRENT_USER.open(RUN_SUBKEY) {
            Ok(key) => key,
            Err(_) => return Ok(()),
        };
        // 值不存在：remove_value 报错 → 幂等成功（目标态一致）。
        match key.remove_value(RUN_VALUE_NAME) {
            Ok(()) => Ok(()),
            Err(_) => Ok(()),
        }
    }

    fn is_enabled(&self) -> bool {
        CURRENT_USER
            .open(RUN_SUBKEY)
            .ok()
            .and_then(|key| key.get_string(RUN_VALUE_NAME).ok())
            .is_some()
    }
}

/// 值命令形态一致性（注册表侧无法在非 windows host 单测——生成器测试在
/// mod.rs 覆盖；此处钉死值名/子键常量与 Run 键规范一致）。
#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn run_key_constants_are_stable() {
        assert_eq!(
            RUN_SUBKEY,
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run"
        );
        assert_eq!(RUN_VALUE_NAME, "XControl");
        // 值形态生成器（mod.rs 纯函数）与 Run 键约定一致：REG_SZ + 引号包裹。
        assert_eq!(
            run_value_command(Path::new("C:\\Program Files\\XControl\\xcontrol-app.exe")),
            "\"C:\\Program Files\\XControl\\xcontrol-app.exe\""
        );
    }
}
