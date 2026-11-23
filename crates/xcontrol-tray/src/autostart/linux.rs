//! XDG autostart 后端（linux 桌面会话——X11 与 Wayland 共用，docs/05 §3 自启行、
//! FR-6.1）。
//!
//! .desktop 落点 `~/.config/autostart/com.xcontrol.desktop`（Desktop Entry
//! Specification；GNOME/KDE/sway 等遵循 XDG autostart 的会话管理器在登录时
//! 拉起）。`Exec = std::env::current_exe`（含空格路径按 spec 加引号）。
//!
//! 全程 std（fs），无 unsafe（crate 级 forbid）；X11 与 Wayland feature 同一
//! 实现（XDG autostart 与窗口系统无关）。

use std::path::{Path, PathBuf};

use super::{Autostart, AutostartError, desktop_entry, desktop_path};

/// XDG autostart 代理（.desktop 落点注入构造——生产走 [`XdgAgent::current`]）。
#[derive(Debug, Clone)]
pub struct XdgAgent {
    /// .desktop 绝对路径（家目录不可得 → `None`：enable/is_enabled 报错/停用语义）。
    desktop: Option<PathBuf>,
}

impl XdgAgent {
    /// 生产构造：`$HOME/.config/autostart/com.xcontrol.desktop`。
    #[must_use]
    pub fn current() -> Self {
        Self {
            desktop: std::env::var_os("HOME")
                .map(PathBuf::from)
                .as_deref()
                .map(desktop_path),
        }
    }

    /// 注入构造（测试/诊断：任意 .desktop 路径）。
    #[must_use]
    pub fn with_desktop(desktop: PathBuf) -> Self {
        Self {
            desktop: Some(desktop),
        }
    }
}

impl Autostart for XdgAgent {
    fn enable(&self) -> Result<(), AutostartError> {
        let desktop = self.desktop.as_ref().ok_or(AutostartError::HomeDir)?;
        let program =
            std::env::current_exe().map_err(|source| AutostartError::CurrentExe { source })?;
        if let Some(parent) = desktop.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AutostartError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let entry = desktop_entry(&program);
        std::fs::write(desktop, entry).map_err(|source| AutostartError::Write {
            path: desktop.clone(),
            source,
        })
    }

    fn disable(&self) -> Result<(), AutostartError> {
        let Some(desktop) = self.desktop.as_deref() else {
            return Ok(());
        };
        match std::fs::remove_file(desktop) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(AutostartError::Remove {
                path: desktop.to_path_buf(),
                source,
            }),
        }
    }

    fn is_enabled(&self) -> bool {
        self.desktop.as_deref().is_some_and(Path::exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生命周期：enable → is_enabled → disable → 幂等（纯 fs，任意 host 可跑）。
    #[test]
    fn enable_disable_is_enabled_lifecycle() {
        let desktop =
            std::env::temp_dir().join(format!("xcontrol-tray-test-{}.desktop", std::process::id()));
        let agent = XdgAgent::with_desktop(desktop.clone());
        let _ = std::fs::remove_file(&desktop); // 清残留

        assert!(!agent.is_enabled());
        agent.enable().expect("enable 成功（temp 可写）");
        assert!(agent.is_enabled());
        let written = std::fs::read_to_string(&desktop).expect("desktop 可读");
        assert!(written.starts_with("[Desktop Entry]"));
        let exe = std::env::current_exe().expect("测试进程路径可得");
        assert!(
            written.contains(&format!("Exec={}", exe.to_string_lossy())),
            "Exec 应含 current_exe：{written}"
        );

        agent.disable().expect("disable 成功");
        assert!(!agent.is_enabled());
        agent.disable().expect("幂等：再删不存在配置仍成功");
    }

    /// 家目录缺失语义：enable 报 HomeDir；disable/is_enabled 无害。
    #[test]
    fn missing_home_degrades_gracefully() {
        let agent = XdgAgent { desktop: None };
        assert!(!agent.is_enabled());
        assert!(agent.disable().is_ok());
        assert!(matches!(agent.enable(), Err(AutostartError::HomeDir)));
    }
}
