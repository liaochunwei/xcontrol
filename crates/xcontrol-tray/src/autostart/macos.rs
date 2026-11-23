//! launchd agent 自启后端（macOS，docs/05 §1 自启行、FR-6.1）。
//!
//! plist 落点 `~/Library/LaunchAgents/com.xcontrol.plist`（Label =
//! [`super::LABEL`]）；`ProgramArguments = std::env::current_exe`——**cargo run
//! 与安装后路径不同**，勾选时的当前二进制即配置对象（docs/06 §5 注记）。
//!
//! 生效时机裁定（mod.rs 模块文档）：`enable` 只写 plist（登录时 launchd 自动
//! 加载 LaunchAgents 目录，登录即生效），**不** bootstrap——避免按 RunAtLoad
//! 立即拉起第二个实例。`disable` 只删 plist；运行态注销（bootout）为独立的
//! [`LaunchdAgent::bootout`] 方法——对注入路径实例的 disable 无差别 bootout 固定
//! Label 会误伤真实 job（11-18-m4-tray 实测教训），M6 打包任务再接入立即生效。
//!
//! 全程 std（fs + `launchctl` Command），无 unsafe（crate 级 forbid）。

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Autostart, AutostartError, LABEL, plist_path, plist_xml};

/// launchd LaunchAgent（plist 落点注入构造——生产走 [`LaunchdAgent::current`]，
/// 测试可注入临时路径验证内容与生命周期）。
#[derive(Debug, Clone)]
pub struct LaunchdAgent {
    /// plist 绝对路径（家目录不可得 → `None`：enable/is_enabled 报错/停用语义）。
    plist: Option<PathBuf>,
}

impl LaunchdAgent {
    /// 生产构造：`$HOME/Library/LaunchAgents/com.xcontrol.plist`。
    #[must_use]
    pub fn current() -> Self {
        Self {
            plist: std::env::var_os("HOME")
                .map(PathBuf::from)
                .as_deref()
                .map(plist_path),
        }
    }

    /// 注入构造（测试/诊断：任意 plist 路径）。
    #[must_use]
    pub fn with_plist(plist: PathBuf) -> Self {
        Self { plist: Some(plist) }
    }

    /// 当前登录会话的 gui domain（`gui/<uid>`；`id -u` 取 uid——std 无 uid API，
    /// 进程级一次性查询，失败返回 `None` 由调用方降级）。
    fn gui_domain() -> Option<String> {
        let output = Command::new("id").arg("-u").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if uid.is_empty() {
            return None;
        }
        Some(format!("gui/{uid}"))
    }

    /// `launchctl bootstrap gui/<uid> <plist>`（加载并按 RunAtLoad 立即拉起——
    /// M4 仅测试/诊断路径使用；生产 enable 不调用，mod.rs 裁定）。
    ///
    /// # Errors
    /// launchctl 非零退出（含"已 bootstrap"——调用方按需幂等化）。
    pub fn bootstrap(&self) -> Result<(), AutostartError> {
        let Some(plist) = self.plist.as_deref() else {
            return Err(AutostartError::HomeDir);
        };
        let Some(domain) = Self::gui_domain() else {
            return Err(AutostartError::Backend {
                op: "launchctl bootstrap",
                detail: "无法解析 gui domain（id -u 失败）".to_owned(),
            });
        };
        let output = Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(plist)
            .output()
            .map_err(|source| AutostartError::Backend {
                op: "launchctl bootstrap",
                detail: source.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(AutostartError::Backend {
            op: "launchctl bootstrap",
            detail: format!(
                "exit {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }

    /// `launchctl bootout gui/<uid>/com.xcontrol`（注销已加载 job 并终止其进程；
    /// 未加载/已注销同样成功——幂等）。
    ///
    /// # Errors
    /// domain 解析失败或 launchctl 进程无法启动（launchctl 报"未加载"算成功）。
    pub fn bootout(&self) -> Result<(), AutostartError> {
        let Some(domain) = Self::gui_domain() else {
            return Err(AutostartError::Backend {
                op: "launchctl bootout",
                detail: "无法解析 gui domain（id -u 失败）".to_owned(),
            });
        };
        Command::new("launchctl")
            .arg("bootout")
            .arg(format!("{domain}/{LABEL}"))
            .output()
            .map_err(|source| AutostartError::Backend {
                op: "launchctl bootout",
                detail: source.to_string(),
            })?;
        // 退出码忽略：bootout 是幂等收尾（"未加载/已注销"即目标态；launchctl 对
        // 未加载 job 报非零——按目标态已达成处理）。
        Ok(())
    }
}

impl Autostart for LaunchdAgent {
    fn enable(&self) -> Result<(), AutostartError> {
        let plist = self.plist.as_ref().ok_or(AutostartError::HomeDir)?;
        let program =
            std::env::current_exe().map_err(|source| AutostartError::CurrentExe { source })?;
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AutostartError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let xml = plist_xml(LABEL, &program);
        std::fs::write(plist, xml).map_err(|source| AutostartError::Write {
            path: plist.clone(),
            source,
        })
    }

    fn disable(&self) -> Result<(), AutostartError> {
        // M4 裁定（模块文档）：只删 plist（登录自启的持久化事实源）。**不**在此
        // bootout 已加载 job——对注入路径的实例（测试/诊断）无差别 bootout 固定
        // Label 会误伤真实 job（11-18-m4-tray 实测教训）；运行态注销经
        // [`LaunchdAgent::bootout`] 显式调用（M6 打包任务接入立即生效语义）。
        let Some(plist) = self.plist.as_deref() else {
            return Ok(());
        };
        match std::fs::remove_file(plist) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(AutostartError::Remove {
                path: plist.to_path_buf(),
                source,
            }),
        }
    }

    fn is_enabled(&self) -> bool {
        self.plist.as_deref().is_some_and(Path::exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未启用：临时路径上 is_enabled=false；enable 后 true；disable 幂等归零
    /// （纯 fs 生命周期，零 launchctl 调用——无 gui domain 的 CI/测试环境安全）。
    #[test]
    fn enable_disable_is_enabled_lifecycle() {
        let plist =
            std::env::temp_dir().join(format!("xcontrol-tray-test-{}.plist", std::process::id()));
        let agent = LaunchdAgent::with_plist(plist.clone());
        let _ = std::fs::remove_file(&plist); // 清残留

        assert!(!agent.is_enabled());
        agent.enable().expect("enable 成功（temp 可写）");
        assert!(agent.is_enabled());
        let written = std::fs::read_to_string(&plist).expect("plist 可读");
        assert!(written.contains(&format!("<string>{LABEL}</string>")));
        assert!(written.contains("<key>RunAtLoad</key>"));

        agent.disable().expect("disable 成功");
        assert!(!agent.is_enabled());
        agent.disable().expect("幂等：再删不存在配置仍成功");
    }

    /// enable 落盘内容 = 纯函数生成器产物（current_exe 实路径入 ProgramArguments）。
    #[test]
    fn enable_writes_current_exe_into_program_arguments() {
        let plist =
            std::env::temp_dir().join(format!("xcontrol-tray-exe-{}.plist", std::process::id()));
        let agent = LaunchdAgent::with_plist(plist.clone());
        agent.enable().expect("enable 成功");
        let written = std::fs::read_to_string(&plist).expect("plist 可读");
        let exe = std::env::current_exe().expect("测试进程路径可得");
        assert!(
            written.contains(&format!("<string>{}</string>", exe.to_string_lossy())),
            "ProgramArguments 应含 current_exe：{written}"
        );
        let _ = std::fs::remove_file(&plist);
    }

    /// 家目录缺失语义：无 HOME 时 enable 报 HomeDir、disable/is_enabled 无害。
    #[test]
    fn missing_home_degrades_gracefully() {
        let agent = LaunchdAgent { plist: None };
        assert!(!agent.is_enabled());
        assert!(agent.disable().is_ok());
        assert!(matches!(agent.enable(), Err(AutostartError::HomeDir)));
    }
}

/// mac launchd 全链路实测（AC：plist 写入 + launchctl 验证 + 删除恢复）。
///
/// 手动运行：`cargo test -p xcontrol-tray --features macos -- --ignored --nocapture`
/// 流程：enable 写 plist（ProgramArguments = 测试进程自身）→ launchctl bootstrap
/// → `launchctl print gui/<uid>/com.xcontrol` 验证 job 已加载 → bootout → print
/// 复核已注销 → disable 删 plist 恢复。
#[cfg(test)]
#[ignore = "本机 launchd 实测（AC 手动验收；写入 ~/Library/LaunchAgents 并短暂加载 job）"]
#[test]
fn macos_launchd_live_cycle() {
    // 生产路径（真实 LaunchAgents 目录）——验证 launchd 对真实落点的接受度。
    let agent = LaunchdAgent::current();
    let plist = plist_path(
        &std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("实测需要 HOME"),
    );
    assert_eq!(
        agent.is_enabled(),
        plist.exists(),
        "is_enabled=plist 存在性"
    );

    // 0. 预清理：若历史残留则先恢复（测试前后状态一致）。
    let _ = agent.disable();
    assert!(!agent.is_enabled(), "预清理后应未启用");

    // 1. enable：写入 plist（内容 = 本测试进程路径）。
    agent.enable().expect("enable 写入 plist");
    assert!(plist.exists(), "plist 已落盘：{}", plist.display());
    println!("[live] plist 写入：{}", plist.display());
    println!(
        "[live] plist 内容：\n{}",
        std::fs::read_to_string(&plist).expect("可读")
    );

    // 2. bootstrap：加载 job（RunAtLoad 会拉起测试进程一次——短命进程，无副作用）。
    agent.bootstrap().expect("launchctl bootstrap");
    println!("[live] bootstrap 完成");

    // 3. print 验证：job 已在 gui domain。
    let domain = LaunchdAgent::gui_domain().expect("gui domain 可解析");
    let print = Command::new("launchctl")
        .arg("print")
        .arg(format!("{domain}/{LABEL}"))
        .output()
        .expect("launchctl print 可执行");
    assert!(
        print.status.success(),
        "launchctl print 失败：{}",
        String::from_utf8_lossy(&print.stderr)
    );
    let stdout = String::from_utf8_lossy(&print.stdout);
    // darwin 25 的 print 头部为 `gui/<uid>/com.xcontrol = {`（无 "label =" 行）。
    assert!(stdout.contains(&format!("/{LABEL} = {{")), "{stdout}");
    println!("[live] launchctl print 验证通过（job 已加载）");

    // 4. bootout：注销 job；print 复核已消失。
    agent.bootout().expect("bootout");
    let print = Command::new("launchctl")
        .arg("print")
        .arg(format!("{domain}/{LABEL}"))
        .output()
        .expect("launchctl print 可执行");
    assert!(
        !print.status.success(),
        "bootout 后 print 应失败（job 已注销）"
    );
    println!("[live] bootout 后 print 确认 job 已注销");

    // 5. disable：删 plist 恢复原状。
    agent.disable().expect("disable 删除 plist");
    assert!(!plist.exists(), "plist 已删除恢复");
    assert!(!agent.is_enabled());
    println!("[live] plist 已删除，环境恢复");
}
