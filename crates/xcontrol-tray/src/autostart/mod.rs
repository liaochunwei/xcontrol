//! 开机自启（FR-6.1）：[`Autostart`] trait + 三平台后端（feature 门控）。
//!
//! ```text
//! autostart/
//! ├── mod.rs      # trait + 错误类型 + 纯函数生成器（plist/desktop/Run 值）——跨平台可测
//! ├── macos.rs    # launchd agent（~/Library/LaunchAgents/com.xcontrol.plist）   [feature macos]
//! ├── windows.rs  # HKCU Run 键（Software\Microsoft\Windows\CurrentVersion\Run）[feature windows]
//! └── linux.rs    # XDG autostart（~/.config/autostart/com.xcontrol.desktop）
//!                 #                                                             [feature x11/wayland]
//! ```
//!
//! # 后端语义（三平台统一）
//!
//! - `enable`：写自启配置（路径 = 平台标准位置；可执行文件路径 =
//!   `std::env::current_exe`——**cargo run 与安装后路径不同**，托盘勾选时的
//!   当前二进制即配置对象，docs/06 §5 注记）；
//! - `disable`：删配置（幂等——不存在亦成功）；mac 侧额外 best-effort 注销
//!   已加载的 launchd job；
//! - `is_enabled`：配置存在性（持久化事实源；不查询运行中的 launchd/注册表
//!   会话态——托盘勾选读它，重启后一致）。
//!
//! # mac 生效时机裁定（M4）
//!
//! `enable` 只写 plist **不** `launchctl bootstrap`：bootstrap 会按 `RunAtLoad`
//! 立即拉起一个新实例（与运行中的应用双实例）。登录时 launchd 自动加载
//! LaunchAgents 目录，登录即生效；立即生效（bootstrap/bootout 集成）随 M6
//! 打包任务接入。`LaunchdAgent::bootout` 为 disable 的 best-effort 注销。
//!
//! # 平台门控纪律（architecture-rules 红线 2）
//!
//! 公共代码（trait/错误/纯函数生成器）零 `#[cfg(target_os)]`；平台实现收敛在
//! 各自 feature 门控模块内，家目录解析（mac/linux 读 `HOME`、windows 读
//! `USERPROFILE`）同属平台模块。纯函数生成器只接受注入路径/字符串——三平台
//! 生成物在任意 host 上单测。

use std::path::{Path, PathBuf};

// 平台后端模块公开（pub）：注入构造（with_plist/with_desktop）与 mac 运行态
// 钩子（bootstrap/bootout，M6 接入立即生效语义）是对外 API 的一部分——私有模块
// 会让它们成为 dead_code（-D warnings 下 mac/x11 feature 编译面红，11-18-m4-tray
// check 复现）；平台门控纪律不变（仍由各自 feature 编译，architecture-rules 红线 2）。
#[cfg(any(feature = "x11", feature = "wayland"))]
pub mod linux;
#[cfg(feature = "macos")]
pub mod macos;
#[cfg(feature = "windows")]
pub mod windows;

/// launchd job Label / plist 文件名主干 / .desktop 文件名主干（三平台统一标识）。
pub const LABEL: &str = "com.xcontrol";
/// mac LaunchAgents plist 文件名。
pub const PLIST_FILE_NAME: &str = "com.xcontrol.plist";
/// linux XDG autostart .desktop 文件名。
pub const DESKTOP_FILE_NAME: &str = "com.xcontrol.desktop";
/// windows HKCU Run 键子路径（`Software\Microsoft\Windows\CurrentVersion\Run`）。
pub const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// windows Run 键值名。
pub const RUN_VALUE_NAME: &str = "XControl";

/// 开机自启后端（FR-6.1；`Send + Sync`：app 层以 `Arc<dyn Autostart>` 持有并
/// 进 gpui 闭包）。
pub trait Autostart: Send + Sync {
    /// 启用开机自启（写平台配置；重复启用 = 覆盖写，幂等）。
    ///
    /// # Errors
    /// [`AutostartError`]：可执行文件路径/家目录不可得或配置写入失败。
    fn enable(&self) -> Result<(), AutostartError>;

    /// 停用开机自启（删平台配置；不存在亦成功——幂等）。
    ///
    /// # Errors
    /// [`AutostartError`]：配置删除失败（不存在不算失败）。
    fn disable(&self) -> Result<(), AutostartError>;

    /// 当前是否已启用（配置存在性；托盘菜单勾选态数据源）。
    fn is_enabled(&self) -> bool;
}

/// 自启配置错误（error-handling.md 规则 1：可编程区分的变体；文案不含敏感值）。
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    /// 无法定位当前可执行文件路径（`std::env::current_exe` 失败）。
    #[error("无法定位当前可执行文件路径：{source}")]
    CurrentExe {
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },
    /// 家目录不可得（mac/linux 无 `HOME` / windows 无 `USERPROFILE`）。
    #[error("无法定位家目录（HOME/USERPROFILE 环境变量缺失）")]
    HomeDir,
    /// 自启配置写入失败。
    #[error("自启配置写入失败（{}）：{source}", path.display())]
    Write {
        /// 目标配置路径。
        path: PathBuf,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },
    /// 自启配置删除失败。
    #[error("自启配置删除失败（{}）：{source}", path.display())]
    Remove {
        /// 目标配置路径。
        path: PathBuf,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },
    /// 平台后端命令/注册表操作失败（mac `launchctl` / win 注册表）。
    ///
    /// `op` 为可编程区分的操作标识（如 `"launchctl bootstrap"` /
    /// `"registry remove_value"`）；`detail` 为平台侧错误文本。
    #[error("自启后端操作失败（{op}）：{detail}")]
    Backend {
        /// 操作标识（可编程区分）。
        op: &'static str,
        /// 平台侧错误文本。
        detail: String,
    },
}

/// 本机平台后端（按 feature 编译期选定；macos 优先，windows 次之，linux 桌面
/// 会话（x11/wayland）再次之——同一构建同开多平台 feature 时取先命中者）。
///
/// 无任何平台 feature（默认编译面，如 linux CI 排除 app 的场景）返回 `None`。
#[must_use]
pub fn platform_autostart() -> Option<std::sync::Arc<dyn Autostart>> {
    macos_backend()
        .or_else(windows_backend)
        .or_else(linux_backend)
}

#[cfg(feature = "macos")]
fn macos_backend() -> Option<std::sync::Arc<dyn Autostart>> {
    Some(std::sync::Arc::new(macos::LaunchdAgent::current()))
}
#[cfg(not(feature = "macos"))]
fn macos_backend() -> Option<std::sync::Arc<dyn Autostart>> {
    None
}

#[cfg(feature = "windows")]
fn windows_backend() -> Option<std::sync::Arc<dyn Autostart>> {
    Some(std::sync::Arc::new(windows::RunKeyAgent::current()))
}
#[cfg(not(feature = "windows"))]
fn windows_backend() -> Option<std::sync::Arc<dyn Autostart>> {
    None
}

#[cfg(any(feature = "x11", feature = "wayland"))]
fn linux_backend() -> Option<std::sync::Arc<dyn Autostart>> {
    Some(std::sync::Arc::new(linux::XdgAgent::current()))
}
#[cfg(not(any(feature = "x11", feature = "wayland")))]
fn linux_backend() -> Option<std::sync::Arc<dyn Autostart>> {
    None
}

/// mac LaunchAgents 目录（`<home>/Library/LaunchAgents`；纯路径拼装——三平台
/// host 上均可单测）。
#[must_use]
pub fn launch_agents_dir(home: &Path) -> PathBuf {
    home.join("Library").join("LaunchAgents")
}

/// mac plist 落点（`<home>/Library/LaunchAgents/com.xcontrol.plist`）。
#[must_use]
pub fn plist_path(home: &Path) -> PathBuf {
    launch_agents_dir(home).join(PLIST_FILE_NAME)
}

/// linux XDG autostart 目录（`<home>/.config/autostart`）。
#[must_use]
pub fn desktop_dir(home: &Path) -> PathBuf {
    home.join(".config").join("autostart")
}

/// linux .desktop 落点（`<home>/.config/autostart/com.xcontrol.desktop`）。
#[must_use]
pub fn desktop_path(home: &Path) -> PathBuf {
    desktop_dir(home).join(DESKTOP_FILE_NAME)
}

/// launchd agent plist 内容（RunAtLoad：登录即启动；KeepAlive：false——用户
/// 经托盘「退出」后 launchd 不复活进程；`program` 路径做 XML 转义）。
#[must_use]
pub fn plist_xml(label: &str, program: &Path) -> String {
    let program = xml_escape(&program.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{label}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{program}</string>\n\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>KeepAlive</key>\n\
         \x20 <false/>\n\
         \x20 <key>ProcessType</key>\n\
         \x20 <string>Interactive</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// XDG autostart .desktop 内容（Desktop Entry Specification；`Exec` 路径含空格
/// 时按 spec 用双引号包裹）。
#[must_use]
pub fn desktop_entry(exec: &Path) -> String {
    let exec = exec.to_string_lossy();
    let exec = if exec.contains(' ') {
        format!("\"{exec}\"")
    } else {
        exec.into_owned()
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=XControl\n\
         Comment=XControl 键鼠共享（开机自启）\n\
         Exec={exec}\n\
         Terminal=false\n\
         Categories=Utility;\n"
    )
}

/// windows Run 键值（REG_SZ；含空格路径按 CreateProcess 约定整体加引号）。
#[must_use]
pub fn run_value_command(program: &Path) -> String {
    let program = program.to_string_lossy();
    if program.contains(' ') {
        format!("\"{program}\"")
    } else {
        program.into_owned()
    }
}

/// 最小 XML 文本转义（& < >——plist 值层的五个预设实体中路径可能命中的三个；
/// 引号在文本节点内无需转义）。
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯函数生成器在任意 host 可测（路径按组件断言，不依赖分隔符形态）。
    #[test]
    fn paths_follow_platform_conventions() {
        let home = Path::new("/Users/demo");
        let plist = plist_path(home);
        assert_eq!(
            plist.file_name().map(|s| s.to_str()),
            Some(Some(PLIST_FILE_NAME))
        );
        let plist_str = plist.to_string_lossy().replace('\\', "/");
        assert!(plist_str.contains("Library/LaunchAgents"), "{plist_str}");

        let desktop = desktop_path(home);
        assert_eq!(
            desktop.file_name().map(|s| s.to_str()),
            Some(Some(DESKTOP_FILE_NAME))
        );
        let desktop_str = desktop.to_string_lossy().replace('\\', "/");
        assert!(desktop_str.contains(".config/autostart"), "{desktop_str}");
    }

    /// plist 内容：Label / ProgramArguments / RunAtLoad / KeepAlive 齐全且转义。
    #[test]
    fn plist_xml_contains_label_program_and_lifecycle() {
        let xml = plist_xml(
            LABEL,
            Path::new("/Applications/XControl.app/Contents/MacOS/xcontrol"),
        );
        assert!(xml.contains("<string>com.xcontrol</string>"), "{xml}");
        assert!(
            xml.contains("<string>/Applications/XControl.app/Contents/MacOS/xcontrol</string>")
        );
        assert!(xml.contains("<key>RunAtLoad</key>"));
        assert!(xml.contains("<key>KeepAlive</key>"));
        assert!(xml.trim_end().ends_with("</plist>"));
        // XML 转义：& < > 不裸露
        let xml = plist_xml(LABEL, Path::new("/tmp/a&b<c> d"));
        assert!(xml.contains("/tmp/a&amp;b&lt;c&gt; d"), "{xml}");
        assert!(!xml.contains("/tmp/a&b"));
    }

    /// .desktop 内容：Type/Exec/Terminal 齐全；含空格 Exec 加引号（spec）。
    #[test]
    fn desktop_entry_quotes_spaces_and_lists_fields() {
        let entry = desktop_entry(Path::new("/usr/local/bin/xcontrol-app"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=/usr/local/bin/xcontrol-app\n"));
        assert!(entry.contains("Terminal=false"));

        let entry = desktop_entry(Path::new("/opt/My Apps/xcontrol"));
        assert!(
            entry.contains("Exec=\"/opt/My Apps/xcontrol\"\n"),
            "{entry}"
        );
    }

    /// Run 键值：含空格路径加引号（CreateProcess 约定）。
    #[test]
    fn run_value_quotes_spaces() {
        assert_eq!(
            run_value_command(Path::new("C:\\Apps\\xcontrol.exe")),
            "C:\\Apps\\xcontrol.exe"
        );
        assert_eq!(
            run_value_command(Path::new("C:\\My Apps\\xcontrol.exe")),
            "\"C:\\My Apps\\xcontrol.exe\""
        );
    }

    /// 常量与平台规范一致（Run 子路径 / 值名 / Label）。
    #[test]
    fn constants_match_platform_conventions() {
        assert_eq!(LABEL, "com.xcontrol");
        assert_eq!(
            RUN_SUBKEY,
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run"
        );
        assert_eq!(RUN_VALUE_NAME, "XControl");
    }

    /// 默认编译面（无平台 feature）工厂返回 None——本测试在 mac host 跑时
    /// macos feature 未开（cargo test -p xcontrol-tray 默认 features）。
    #[test]
    fn factory_yields_backend_matching_enabled_features() {
        let backend = platform_autostart();
        // 任何 host 上，至少与已启用的平台 feature 一致；未启用时为 None。
        if cfg!(any(feature = "macos", feature = "windows")) {
            assert!(backend.is_some());
        }
        // is_enabled 是无副作用纯读——断言与平台配置事实源**一致**而非恒 false：
        // 开发机可能处于合法的已启用状态（用户真开了自启），测试不得依赖机器
        // 状态（一致性断言在任意机器状态上都成立）。fs 后端（mac/linux）比对
        // 配置文件存在性；windows 注册表读取只验证不 panic（注入路径生命周期
        // 断言在各平台模块测试内）。
        if let Some(backend) = backend {
            if cfg!(feature = "macos") {
                let expected = std::env::var_os("HOME")
                    .is_some_and(|home| plist_path(Path::new(&home)).exists());
                assert_eq!(backend.is_enabled(), expected);
            } else if cfg!(any(feature = "x11", feature = "wayland")) {
                let expected = std::env::var_os("HOME")
                    .is_some_and(|home| desktop_path(Path::new(&home)).exists());
                assert_eq!(backend.is_enabled(), expected);
            } else {
                let _ = backend.is_enabled();
            }
        }
    }
}
