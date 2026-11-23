//! `xcontrol-app` 进程入口：手写 CLI 解析 + 模式分发（bin 顶层 anyhow，
//! error-handling.md 规则 2——错误统一在此格式化输出）。
//!
//! 模式（三选一）：
//!
//! ```text
//! --loopback                 进程内 M1 全链路演示（FR-1.5；--auto-quit-ms 时走 headless）
//! --host                     主控：监听 + 自动通告（mDNS + beacon，FR-5.2 通告侧）
//! --client <ip:port>         从机：手动 IP 直连（FR-5.2 手动路径）
//! --client                   从机：自动发现模式（mDNS + beacon + UDP 探测，FR-5.1）
//!
//! --code <安全码>            安全码（测试便利优先；缺省走 config 的存储后端）
//! --config <path>            配置文件路径（缺省走默认配置路径；不存在则用默认配置）
//! --log-level <level>        日志级别覆盖（NFR-8；缺省取 config [general].log_level）
//! --name <显示名>            通告/协商显示名（缺省主机名；beacon/mDNS TXT/Hello 共用）
//! --no-discover              关闭主控侧自动通告（--host 默认开启）
//! --headless                 无 UI 运行时（FR-6.4）：跳过窗口/托盘初始化，纯 tokio
//!                            会话运行时 + 日志；--auto-quit-ms 到点干净退出
//! --auto-quit-ms <N>         启动 N 毫秒后自动退出（>0；回环+此参 = headless CI 冒烟）
//! -h / --help                帮助
//! ```
//!
//! 发现接线（11-17-m3-discovery，docs/02 §2）：
//! - 主控默认开 [`xcontrol_net::Announcer`]（mDNS 注册、beacon 逐接口定向广播、
//!   `XCP1?` 应答）；`net.discovery` 配置项逐层开关，`--no-discover` 全关。
//!   通告失败（端口占用/mDNS 不可用）告警降级，不中断服务（规则 4）；
//! - 从机 `--client` 无地址 = 自动发现（[`session_rt::client_discover`]：扫描 →
//!   候选 → 逐个握手，失败退避 FR-5.3）；显式地址 = 手动直连 + 断线退避重连；
//! - 安全码纪律（error-handling.md 红线 6）：`--code` 值进入 [`SecretCode`] 后不再
//!   落日志/报错文案（类型上堵死——`SecretCode` 的 `Debug`/`Display` 恒脱敏）。
//!
//! 退出方式（PRD Requirement 2）：`QuitMode::Explicit` 下关窗不退进程——
//! 窗口模式（无 `--auto-quit-ms`）以 Ctrl+C（SIGINT 默认处置）收尾；
//! 回环 + `--auto-quit-ms` 走 headless 回路自动退出，可作 CI 集成冒烟。
//!
//! docs/08 §2：unsafe 仅预期出现在 input / clipboard 平台绑定层，本 crate 禁用。
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use xcontrol_app::logging::{self, LogBuffer};
use xcontrol_app::state::{self, ConnState, StateBus};
use xcontrol_app::{loopback, session_rt, ui};
use xcontrol_config::{
    Config, DiscoveryMethod, LogLevel, Role, SecretCode, SecretRef, build_secret_store, load,
};

/// 演示用实例 UUID（16B）：PSK 盐与 `Identity.instance_id` 的来源。
///
/// M2 演示两实例必须派生同一 PSK，故 host/client 共用此 UUID；M3 发现层落地后
/// 改为每设备真实 UUID（发现通告 + 配置持久化），届时盐策略同步修订（docs/02 §3.1）。
const DEMO_INSTANCE_UUID: [u8; 16] = [
    0x58, 0x43, 0x54, 0x52, 0x4c, 0x2d, 0x4d, 0x32, 0x2d, 0x44, 0x45, 0x4d, 0x4f, 0x00, 0x00, 0x01,
];

/// 回环模式的缺省演示码（非机密、不入日志；`--code` 可覆盖以走真实派生链）。
const LOOPBACK_DEMO_CODE: &str = "loopback-demo-code";

/// 帮助文案（中文；退出语义显式说明）。
const USAGE: &str = "\
xcontrol-app — XControl 桌面进程（窗口 + 托盘 + 会话运行时）

用法：
  xcontrol-app                                          # 无参数启动（见下）
  xcontrol-app --loopback [--auto-quit-ms N] [--code CODE]
  xcontrol-app --host [--code CODE] [--config PATH] [--name NAME] [--no-discover] [--auto-quit-ms N]
  xcontrol-app --client [<ip:port>] [--code CODE] [--config PATH] [--name NAME] [--auto-quit-ms N]
  xcontrol-app --headless --host|--client [<ip:port>] [--code CODE] [--auto-quit-ms N]

模式（可选；缺省 = 配置驱动，FR-6.3 无命令行使用）：
  （无模式参数）      读配置启动：role=host → 主控监听；role=client → 从机自动发现；
                      配置文件不存在 → 启动首次运行向导（FR-6.3），向导完成后按
                      所选角色进入运行时
  --loopback          进程内 host↔client 全链路演示（Noise + 会话状态机 + mock 注入）
  --host              主控：监听 0.0.0.0:<端口> + 自动通告（mDNS + UDP beacon，
                      docs/02 §2.1；--no-discover 关闭通告；端口取 [net].port 默认 4243；
                      白名单 [net].allowlist 启用时握手前按来源 IP 过滤，FR-5.4）
  --client <ip:port>  从机（手动路径）：直连主控并握手；断线后按退避自动重连（FR-5.3）
  --client            从机（自动发现，FR-5.1）：mDNS browse + UDP 探测 + beacon 监听
                      → 候选主控逐个握手（安全码在线匹配，docs/02 §2.2）；多台候选
                      等用户确认并可记忆（FR-5.5，状态页/托盘选择）

通用参数：
  --code CODE         安全码（缺省读配置指向的存储后端：keychain 或 file 0600 回退）
  --config PATH       配置文件路径（缺省为系统默认路径；文件不存在时用默认配置）
  --log-level LEVEL   日志级别覆盖（error|warn|info|debug|trace；缺省取 config
                      [general].log_level 默认 info。子系统微调见环境变量
                      XCONTROL_LOG，env 风格如 net::=debug；设置页改级立即生效）
  --name NAME         显示名（beacon/mDNS TXT/会话协商共用；缺省主机名）
  --no-discover       关闭主控侧自动通告（--host 默认开启）
  --headless          无 UI 运行时（FR-6.4）：不初始化窗口/托盘/gpui 事件回路，
                      纯 tokio 会话运行时 + 日志输出；服务器/SSH 无显示场景用。
                      需已有配置或 --code（向导不可用）；状态查询配套
                      `xcontrol-cli --status`。与 --loopback 组合无增量
                      （回环 + --auto-quit-ms 本就是 headless 冒烟路径）
  --auto-quit-ms N    启动 N 毫秒后自动退出（>0）。回环 + 此参 = headless 冒烟（无窗口）；
                      窗口模式 + 此参 = 开窗冒烟（到点退出）；--headless + 此参 =
                      到点干净退出（FR-6.4 验收路径）
  -h, --help          本帮助

退出方式：
  窗口模式（无 --auto-quit-ms）：托盘菜单「退出」或 Ctrl+C（关闭窗口不退出进程
  ——QuitMode::Explicit）；--headless / 回环 + --auto-quit-ms：到点自动退出
  （CI/SSH 冒烟）；--headless 无 --auto-quit-ms：Ctrl+C 收尾。

示例：
  xcontrol-app                                  # 双击/无参数：按配置启动（或进向导）
  xcontrol-app --loopback --auto-quit-ms 3000   # headless 冒烟，exit 0
  xcontrol-app --host --code demo-code          # 终端 1：主控（通告开启）
  xcontrol-app --client --code demo-code        # 终端 2：从机自动发现 → 握手
  xcontrol-app --client 127.0.0.1:4243 --code demo-code   # 从机手动直连
  xcontrol-app --headless --host --code demo-code --auto-quit-ms 5000   # 无 UI 主控（FR-6.4）
";

/// 运行模式（FR-1.1：同进程一次只扮演一种角色）。
///
/// `None` = 无显式模式（FR-6.3 无命令行使用，11-18-m4-ui）：按 config.role 启动；
/// 配置不存在时进入向导流（supervisor 等待相），向导完成后按所选角色启动。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// 进程内回环演示。
    Loopback,
    /// 主控（监听 + 自动通告）。
    Host,
    /// 从机：`None` = 自动发现（FR-5.1）；`Some` = 手动地址（FR-5.2 手动路径）。
    Client(Option<SocketAddr>),
}

/// 解析后的 CLI 参数（`mode` 可缺省——配置驱动启动/向导流，见 [`Mode`] 文档）。
#[derive(Debug, Clone)]
struct Args {
    mode: Option<Mode>,
    code: Option<String>,
    config: Option<PathBuf>,
    log_level: Option<LogLevel>,
    name: Option<String>,
    no_discover: bool,
    /// 无 UI 运行时（FR-6.4）：跳过窗口/托盘初始化。
    headless: bool,
    auto_quit_ms: Option<u64>,
}

fn main() -> anyhow::Result<()> {
    let args = parse_args(std::env::args_os().skip(1))?;
    let Some(args) = args else {
        return Ok(()); // --help 已打印
    };

    // 配置先行于日志装配：级别取 config [general].log_level（NFR-8），--log-level
    // 覆盖——必须在 subscriber 装配前就绪（M2 的"日志装配先行"顺序随之反转）。
    let (config, config_path) = load_config(args.config.as_deref())?;
    let effective_level = args.log_level.unwrap_or(config.general.log_level);
    // 界面语言（FR-6.5）：启动读 config [general].language（校验层保证合法值；
    // 未知值回退默认 zh-CN——防御手改配置未过校验路径的容错面）。
    xcontrol_i18n::set_language(
        xcontrol_i18n::Lang::parse(&config.general.language).unwrap_or(xcontrol_i18n::Lang::ZhCn),
    );

    // 日志装配（stdout fmt 层 + UI 环形缓冲 + 滚动文件 + EnvFilter；
    // 子系统命名 app::/session::/net::，脱敏与清理见 logging.rs 模块文档）。
    let logs = LogBuffer::new();
    let data_dir = xcontrol_config::default_data_dir();
    let file_dir_missing = data_dir.is_none();
    let _logging = logging::init(
        logs.clone(),
        &logging::LogSettings {
            level: effective_level,
            file_dir: data_dir,
        },
    )
    .context("日志装配失败")?;
    // LoggingGuard 绑定存活到 main 末尾：滚动文件 worker 提前 drop 会丢缓冲日志。
    match &config_path {
        Some(path) => tracing::info!(target: "app::config", path = %path.display(), "配置已加载"),
        None => tracing::info!(target: "app::config", "配置文件不存在，使用默认配置"),
    }
    if args.log_level.is_some() {
        tracing::info!(target: "app::log", level = effective_level.as_str(),
            "--log-level 覆盖 config [general].log_level");
    }
    if file_dir_missing {
        tracing::warn!(target: "app::log",
            "无法定位数据目录（家目录不可得），滚动文件日志不启用（仅 stdout/UI）");
    }
    tracing::info!(target: "app::log", level = effective_level.as_str(),
        "日志装配完成（stdout + UI 环形缓冲 + 滚动文件 daily rotation）");

    // 无显式模式（FR-6.3 无命令行使用，11-18-m4-ui）：配置驱动——role=host 主控
    // 监听；role=client 从机自动发现；配置缺失 → 向导流（supervisor 等待相，
    // 向导完成写 config + 存码后经 RuntimeConfigUpdate 唤醒运行时）。
    let wizard = config_path.is_none() && args.mode.is_none();
    // FR-6.4：headless 下向导不可用（无 UI 输入面）——配置/码必须在命令行或
    // 既有 config 提供，否则明确报错而非静默挂起。
    if args.headless && wizard {
        anyhow::bail!(
            "--headless 需要已有配置或 --code（首次运行向导在无 UI 模式下不可用）；\
             先在桌面环境完成向导，或提供 --config/--code"
        );
    }
    let mode = match &args.mode {
        Some(mode) => mode.clone(),
        None if wizard => {
            tracing::info!(target: "app::main",
                "无配置且无模式参数：进入首次运行向导（FR-6.3），运行时等待向导完成");
            Mode::Client(None) // 等待相默认从机姿态（向导完成后按所选角色启动）
        }
        None => match config.role {
            Role::Host => Mode::Host,
            Role::Client => Mode::Client(None),
        },
    };
    if args.mode.is_none() && !wizard {
        tracing::info!(target: "app::main", role = ?config.role,
            "无模式参数：按 config.role 启动（FR-6.3 配置驱动语义）");
    }

    let role = match mode {
        Mode::Loopback | Mode::Host => Role::Host,
        Mode::Client(_) => Role::Client,
    };
    let display_name = resolve_display_name(args.name.as_deref());
    tracing::info!(target: "app::main", name = %display_name, "进程身份显示名就绪");
    let bus = StateBus::new(state::AppState {
        role,
        conn: ConnState::Idle,
        ..state::AppState::default()
    });

    match mode {
        Mode::Loopback => run_loopback(Arc::clone(&bus), logs, &args),
        Mode::Host | Mode::Client(_) => {
            run_tcp(Arc::clone(&bus), logs, config, config_path, &args, wizard)
        }
    }
}

/// `--loopback` 模式：进程内全链路演示（FR-1.5）。
///
/// `--auto-quit-ms` 有值 → headless 分支（`Application::headless()`，CI 冒烟）；
/// 无值 → 窗口分支（状态页计数随脚本推进、日志页滚动；Ctrl+C 退出）。
fn run_loopback(bus: Arc<StateBus>, logs: LogBuffer, args: &Args) -> anyhow::Result<()> {
    let code = match &args.code {
        Some(code) => SecretCode::new(code.clone()),
        None => SecretCode::new(LOOPBACK_DEMO_CODE),
    };
    let psk = xcontrol_net::derive_psk(code.expose(), &DEMO_INSTANCE_UUID)
        .map_err(|error| anyhow::anyhow!("安全码派生失败：{error}"))?;
    let identity = loopback::loopback_identity("xcontrol-demo", DEMO_INSTANCE_UUID);
    let auto_quit = args.auto_quit_ms.map(Duration::from_millis);

    // 命令通道（docs/06 §6）：发送端面向 UI/托盘（M4 托盘动作经此下发），
    // 接收端归回环驱动线程——通道契约由单测驱动验证。
    let (command_tx, command_rx) = tokio::sync::mpsc::channel::<state::CoreCommand>(16);
    let command_handle = state::CoreHandle::new(command_tx);
    let driver = loopback::start(Arc::clone(&bus), command_rx, psk, identity, auto_quit);
    match auto_quit {
        Some(limit) => {
            // headless 分支：gpui 仅维持事件回路到点退出（无窗口、无 Dock 图标）。
            // 看门狗比驱动截止晚 500ms：macOS quit = NSApplication terminate
            // （进程在 run 内终结），必须让驱动线程先完成收尾输出再退出。
            ui::run_headless(limit + Duration::from_millis(500));
            // join 仅用于退出码语义（线程 panic → 非零退出）；事件计数输出
            // 由驱动线程自身打印（run 之后的代码在 macOS quit 下不可达）。
            driver.join_summary()?;
            Ok(())
        }
        None => {
            tracing::info!(target: "app::main",
                "回环演示（窗口模式）：关闭窗口不退出进程（QuitMode::Explicit），托盘/Ctrl+C 退出");
            // 回环演示窗口：状态页 + 日志页 + 设置页路由均可用；配置面为默认值
            // （演示进程码/角色来自 CLI，RuntimeConfigUpdate 仅日志级别立即生效
            // ——loopback.rs 消费分支），不落盘（config_path None）。
            ui::run_window(
                bus,
                logs,
                ui::WindowParams {
                    title: ui::title_loopback(),
                    auto_quit: None,
                    core: command_handle,
                    config: Config::default(),
                    config_path: None,
                    wizard: false,
                    open_at_start: true,
                },
            );
            Ok(())
        }
    }
}

/// `--host` / `--client` / 无模式（配置驱动）TCP 会话运行时（tokio 执行域）+
/// gpui 窗口主线程。
///
/// 线程模型（docs/01 §2）：tokio 多线程运行时驱动 `session_rt`（网络/会话/发现），
/// gpui 主线程跑窗口；经 state.rs 的 watch/mpsc 单向桥接。
///
/// 运行时生命周期收敛进 [`supervise_runtime`]（tokio 任务）——托盘 `SwitchRole`
/// 命令经会话循环优雅断开并上抛 [`session_rt::SessionEnd::RoleSwitch`]，
/// supervisor 据此翻转角色重建运行时；`SessionEnd::ConfigReload` 同理按新配置
/// 重建（11-18-m4-ui 生效框架，docs/06 §4.1）。
///
/// `wizard` = 首次运行向导流（FR-6.3，无配置 + 无模式参数）：进程启动时无安全码
/// 可用（argon2 PSK 派生无法进行），supervisor 进入**等待相**——命令通道归其直接
/// 消费，向导完成（`RuntimeConfigUpdate` 携带新 config + 码已入 SecretStore）后
/// 解析码 → 派生 PSK → 按 config.role 进入正常循环。UI 侧向导完成时只发
/// `RuntimeConfigUpdate`（等待相退出即按 config.role 定角色，不再叠加 SwitchRole
/// ——双命令会造成二次翻转）。
fn run_tcp(
    bus: Arc<StateBus>,
    logs: LogBuffer,
    config: Config,
    config_path: Option<PathBuf>,
    args: &Args,
    wizard: bool,
) -> anyhow::Result<()> {
    // 共享配置单元（docs/06 §4.1）：supervisor/会话循环的配置事实源；初始值 =
    // 启动时加载的 config（向导完成后被 RuntimeConfigUpdate 覆盖）。
    let config_cell: state::SharedConfig = Arc::new(std::sync::Mutex::new(config.clone()));

    // 安全码解析：`--code` 优先；否则配置指向的存储后端。失败时的分流：
    // 向导流（wizard）→ 等待相（码由向导提供后重试）；CLI 路径 → 硬错误
    // （历史语义：明确提示用 --code，不留静默无码运行时）。
    // 向导流（无 config + 无 --code）**跳过存储探测**：等待相的码由向导提供，
    // 提前读默认 keychain 引用会在 macOS 钥匙串 ACL 授权弹窗上同步阻塞主线程
    // （SecKeychainFindGenericPassword 等待用户放行——11-19-m6-release 实测：
    // 既有 "xcontrol" 条目 + 新构建二进制 → 弹窗挂起，窗口/托盘永不装配；
    // 无码启动路径本就无需读存储）。配置存在时的 keychain 读取保持原路径——
    // 首次访问弹窗是 macOS 语义（用户放行一次后记忆）。
    let secrets = if wizard && args.code.is_none() {
        None
    } else {
        resolve_secrets(args.code.as_deref(), &config)
    };
    if secrets.is_none() && !wizard {
        anyhow::bail!(
            "读取安全码失败：config 指向的存储后端不可用且未提供 --code（首次使用请运行 {}",
            "xcontrol 进入向导，或用 --code 显式提供）"
        );
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<state::CoreCommand>(state::COMMAND_CHANNEL_BUFFER);
    let command_handle = state::CoreHandle::new(tx); // UI/托盘命令入口

    // 单实例探测（docs/06 §2 M4 最小裁定）：对本机服务端口做 TCP connect 探测；
    // 连上即判定已有实例——记 warn 后照常运行（锁 + IPC 唤起已有实例窗口移交
    // M5/M6；macOS Dock reopen 已由 on_reopen 覆盖）。
    if probe_existing_instance(config.net.port) {
        tracing::warn!(target: "app::main", port = config.net.port,
            "检测到本机端口已被占用（疑似已有 XControl 实例在运行）：本次启动继续，\
             完整单实例锁 + IPC 唤起移交 M5/M6（docs/06 §4.1）");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio 运行时创建失败")?;

    let auto_quit = args.auto_quit_ms.map(Duration::from_millis);
    // 向导流目标配置路径（11-18-m4-ui check 修复）：load_config 对缺失文件返回
    // None——向导完成需要知道写往哪里。`--config` 显式路径优先，缺省系统默认
    // 路径；不可得（无家目录）时保持 None——向导完成就地报错提示（与设置页
    // commit 同语义），不会误写。
    let window_config_path = if wizard {
        wizard_target_config_path(args.config.as_deref())
    } else {
        config_path
    };
    let title = match &args.mode {
        Some(Mode::Host) => ui::title_host(),
        Some(Mode::Client(_)) => ui::title_client(),
        Some(Mode::Loopback) => {
            return Err(anyhow::anyhow!("内部模式分发错误：run_tcp 收到回环模式"));
        }
        // 无模式（配置驱动/向导）：标题中性（角色快照在状态页呈现）。
        None => ui::title_settings(),
    };
    // config 驱动启动（args.mode=None 且非向导）按 config.role 定角色——
    // 兜底 Client(None) 只属于向导等待相（11-20 修复：配置过的主控重启
    // 曾掉进从机发现模式，run_tcp 的 args.mode 恒为 None）。
    let initial_mode = initial_runtime_mode(args.mode.as_ref(), wizard, config.role);

    // 运行时 supervisor（tokio 任务）：持有命令通道接收端与角色生命周期——
    // 角色切换/配置重建在任务内进行（ui 窗口/命令发送端不受影响）。
    let deps = RuntimeDeps {
        config: config_cell,
        args_code: args.code.clone(),
        secrets: secrets.map(|(code, psk)| (code, psk, config.security_code_ref.clone())),
        manual_addr: match &args.mode {
            Some(Mode::Client(addr)) => *addr,
            _ => None,
        },
        display_name: resolve_display_name(args.name.as_deref()),
        no_discover: args.no_discover,
    };
    let supervisor = runtime.spawn(supervise_runtime(Arc::clone(&bus), rx, deps, initial_mode));

    // FR-6.4 headless 分支（docs/06 §2 裁定：运行时无 UI 而非编译面分离——
    // gpui 仍编进二进制但**不初始化**）：纯 tokio 会话运行时 + 日志；auto-quit
    // 到点 drop CoreHandle（命令通道关闭 = supervisor SessionClosed 收尾）。
    // 服务器/SSH 无显示场景的验收形态：`--headless --host --code …
    // --auto-quit-ms 5000`（无 DISPLAY/WAYLAND_DISPLAY 环境运行 + 干净退出）。
    if args.headless {
        tracing::info!(target: "app::main", "headless 模式（FR-6.4）：跳过窗口/托盘/gpui 初始化，纯会话运行时");
        let quit_handle = command_handle;
        if let Some(limit) = auto_quit {
            // auto-quit 看门狗（std 线程）：到点关闭命令通道 → supervisor 收束
            // → block_on 返回 → 干净退出（exit 0）。
            std::thread::spawn(move || {
                std::thread::sleep(limit);
                tracing::info!(target: "app::main", "headless auto-quit 到点，退出进程");
                drop(quit_handle);
            });
        }
        // 无 auto-quit：command_handle 存活至函数返回（Ctrl+C SIGINT 默认处置
        // 终止进程，通道关闭路径不触发）。
        runtime
            .block_on(supervisor)
            .map_err(|error| anyhow::anyhow!("运行时 supervisor 异常退出：{error}"))?;
        tracing::info!(target: "app::main", "headless 运行时已收束，进程退出");
        runtime.shutdown_timeout(Duration::from_millis(500));
        return Ok(());
    }

    // 窗口开窗策略（docs/06 §2）：设置窗口默认不自动弹出；向导（首次运行）、
    // 回环演示与 auto-quit 冒烟除外。托盘「打开设置…」/Dock reopen 随时唤起。
    let open_at_start = wizard || auto_quit.is_some();
    ui::run_window(
        bus,
        logs,
        ui::WindowParams {
            title,
            auto_quit,
            core: command_handle,
            config,
            config_path: window_config_path,
            wizard,
            open_at_start,
        },
    );

    // 窗口回路退出（auto-quit / 显式 quit）：收运行时（supervisor 任务随通道
    // 关闭自然收束——Channel rx 归 supervisor，CoreHandle drop 即 is_closed）。
    // 注意：macOS 的 quit = NSApplication terminate，进程在 run 内终结，
    // 本行在 macOS 上通常不可达（best-effort 清理；Windows/Linux 正常执行）。
    runtime.shutdown_timeout(Duration::from_millis(500));
    Ok(())
}

/// 单实例探测（docs/06 §2 M4 最小裁定）：对本机 `port` 做一次 TCP connect 探测。
///
/// 连接成功 → 有监听者（既有实例的主控 socket）→ 返回 true 并立即断开（既有
/// 实例在握手失败观测面记录一次来源 127.0.0.1 的折叠事件——可接受噪音，docs/02
/// §0 语义不变；**不用 bind 探测**：macOS std TcpListener 默认 SO_REUSEADDR，
/// 与既有通配监听共存，恒假阴性）。连接被拒/超时 → 端口空闲。客户端实例（不监听
/// 该端口）探测不到——已知局限，完整单实例锁 + IPC 移交 M5/M6。
fn probe_existing_instance(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// 运行时模式（FR-1.1：同进程一次只扮演一种角色；托盘角色切换在此层翻转）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeMode {
    /// 主控（监听 + 自动通告）。
    Host,
    /// 从机：`None` = 自动发现（FR-5.1）；`Some` = 手动地址（FR-5.2 手动路径）。
    Client(Option<SocketAddr>),
}

impl RuntimeMode {
    /// 角色翻转（托盘「切换角色」语义；手动地址保留——切回从机时恢复原路径）。
    fn flipped(self) -> Self {
        match self {
            Self::Host => Self::Client(None), // 主控 → 从机：走自动发现（找到哪台主控连哪台）
            Self::Client(_) => Self::Host,
        }
    }
}

/// supervisor 的运行时依赖集（角色/配置重建共用）。
///
/// `config` = 共享配置单元（docs/06 §4.1 生效框架）：`RuntimeConfigUpdate` 消费
/// 点写入、每轮重建与会话构造点读取——配置事实源单一；`secrets` = 已解析的
/// （码, PSK, 解析时引用）——argon2 派生进程内只做一次（docs/02 §3.1"派生一次"；
/// 向导等待相为 `None`，向导完成后按新 config 的存储后端解析并缓存）。
/// 第三列引用是**换码协议**（docs/07 §3.3）的失效判据：config 引用变化 → 缓存
/// 作废 → 下轮 `ensure_secrets` 重解析重派生（PskCache 属发现循环局部，随重建
/// 一并失效）。
struct RuntimeDeps {
    config: state::SharedConfig,
    /// CLI `--code` 原值（优先于存储后端；每轮重试解析——向导等待相结束后
    /// 仍然生效）。
    args_code: Option<String>,
    /// 已解析（安全码, PSK, 解析时的 security_code_ref）：`None` = 无码可用
    /// （向导等待相 / 换码后重解析失败）。
    secrets: Option<(xcontrol_config::SecretCode, [u8; 32], String)>,
    /// CLI 手动从机地址（角色往返后恢复原路径）。
    manual_addr: Option<SocketAddr>,
    display_name: String,
    no_discover: bool,
}

impl RuntimeDeps {
    /// 当前配置快照（cell 克隆；临界区为纯 clone）。
    fn config_snapshot(&self) -> Config {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 确保安全码/PSK 就绪（每轮 supervisor 重建前调用）：
    ///
    /// - 缓存命中（`--code` 固定或引用未变）直接返回；
    /// - 引用变化（docs/07 §3.3 换码协议）→ 旧缓存作废，按新引用重新解析
    ///   （SecretStore 读出的是新码）+ 重派生 PSK；
    /// - 未就绪则按 `--code` → config 存储后端顺序解析并派生（成功写入
    ///   `secrets`）。失败返回 `false` 并告警（不 panic——等待相下由向导
    ///   完成后重试）。
    fn ensure_secrets(&mut self) -> bool {
        let config = self.config_snapshot();
        if let Some((_, _, resolved_ref)) = &self.secrets {
            let unchanged = self.args_code.is_some() || *resolved_ref == config.security_code_ref;
            if unchanged {
                return true;
            }
            tracing::info!(target: "app::main",
                old_ref = %resolved_ref, new_ref = %config.security_code_ref,
                "安全码引用变更（docs/07 §3.3 换码协议）：作废 PSK 缓存，按新引用重新解析派生");
            self.secrets = None;
        }
        match resolve_secrets(self.args_code.as_deref(), &config) {
            Some((code, psk)) => {
                tracing::info!(target: "app::main",
                    "安全码就绪：PSK 已派生（argon2 进程内一次，docs/02 §3.1）");
                self.secrets = Some((code, psk, config.security_code_ref.clone()));
                true
            }
            None => {
                tracing::warn!(target: "app::main",
                    "安全码不可用（--code 未提供且存储后端读取失败）；等待向导/设置提供");
                false
            }
        }
    }
}

/// 运行时 supervisor：角色生命周期循环 + 配置生效框架（docs/06 §4.1）。
///
/// ```text
/// 若 secrets 未就绪（向导等待相，FR-6.3）：
///     loop { recv 命令：
///         RuntimeConfigUpdate → 写单元 → 解析码/PSK → mode = config.role → 进入主循环
///         SwitchRole          → 等待相记账翻转（无运行时可断）
///         其余/关闭            → 留痕 / 收尾 }
/// 主循环 loop {
///     快照投影角色（翻转后 ≤1s 可见，FR-6.1）
///     Host    → 通告器按需启动 → host_listen → run_session（命令通道移入/交还）
///     Client  → 通告器停止 → client_session / client_discover
///     SessionEnd::RoleSwitch   → 翻转模式，下一轮重建
///     SessionEnd::ConfigReload → 同模式重建（配置单元已是新值，docs/06 §4.1）
///     SessionEnd::SessionClosed → 收尾退出（命令通道关闭 = UI 退出）
/// }
/// ```
async fn supervise_runtime(
    bus: Arc<StateBus>,
    mut rx: tokio::sync::mpsc::Receiver<state::CoreCommand>,
    mut deps: RuntimeDeps,
    mut mode: RuntimeMode,
) {
    // —— 向导等待相（FR-6.3：无配置无码 → UI 向导完成后经 RuntimeConfigUpdate
    // 唤醒；docs/06 §4.1「role/码 = 下次启动或重建生效」的本进程特例：等待相
    // 内生效即首次启动）——
    if deps.secrets.is_none() {
        tracing::info!(target: "app::main",
            "运行时等待相：无安全码可用，等待首次运行向导/设置页提供（FR-6.3）");
        loop {
            match rx.recv().await {
                Some(state::CoreCommand::RuntimeConfigUpdate(config)) => {
                    let config = *config;
                    state::store_shared_config(&deps.config, &config);
                    crate::logging::set_level(config.general.log_level);
                    // 界面语言（FR-6.5）：与 log_level 同消费点立即生效。
                    xcontrol_i18n::set_language(
                        xcontrol_i18n::Lang::parse(&config.general.language)
                            .unwrap_or(xcontrol_i18n::Lang::ZhCn),
                    );
                    if deps.ensure_secrets() {
                        // 等待相退出即按新 config 定角色（向导不再叠加 SwitchRole
                        // ——双命令语义会二次翻转，见 run_tcp 文档）。
                        mode = match config.role {
                            Role::Host => RuntimeMode::Host,
                            Role::Client => RuntimeMode::Client(None),
                        };
                        tracing::info!(target: "app::main", role = ?mode,
                            "向导/设置提供配置：安全码就绪，按所选角色进入运行时（FR-6.3 完成）");
                        break;
                    }
                }
                Some(state::CoreCommand::SwitchRole) => {
                    tracing::info!(target: "app::main", "等待相收到角色切换：记账翻转（运行时未启动）");
                    mode = mode.flipped();
                }
                Some(state::CoreCommand::ConfirmHost { instance_id }) => {
                    // 候选确认（FR-5.5）在等待相到达：无发现循环在跑——留痕忽略
                    //（发现循环启动后由共享配置/命令承接）。
                    tracing::debug!(target: "app::main", instance_id = %instance_id,
                        "等待相收到候选确认（FR-5.5）：运行时未启动，留痕忽略");
                }
                Some(other) => {
                    tracing::debug!(target: "app::main", ?other, "等待相收到命令（留痕忽略）");
                }
                None => {
                    tracing::info!(target: "app::main", "命令通道关闭（UI 已退出），等待相收尾");
                    return;
                }
            }
        }
    }

    // 通告器存活域：主控角色期间存活（host→client 切换即停；收尾兜底停）。
    let mut announcer: Option<Arc<xcontrol_net::Announcer>> = None;
    loop {
        // 角色投影：翻转后立即 flush（FR-6.1 状态 ≤1s 反映的角色面）。
        let role = match mode {
            RuntimeMode::Host => Role::Host,
            RuntimeMode::Client(_) => Role::Client,
        };
        bus.report(|state| state.role = role);
        bus.flush();

        // 本轮配置快照（docs/06 §4.1：每轮重建读最新单元——RuntimeConfigUpdate
        // / RoleSwitch / ConfigReload 后的下一轮都按新值装配）。
        let config = deps.config_snapshot();
        // 安全码/PSK 就绪判定（每轮重建前）：引用变化时按新引用重解析重派生
        // （docs/07 §3.3 换码协议——会话循环已断开旧会话，这里供给新 PSK）。
        // 解析失败（存储后端临时不可用）：等待后重试，不用旧 PSK 重建
        //（换码半途的 PSK 混用会话必败）。
        if !deps.ensure_secrets() {
            tracing::warn!(target: "app::main", "安全码未就绪：2s 后重试（等待向导/设置/存储后端恢复）");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        // 会话运行体：命令通道接收端随调用移入、随返回值交还（角色切换时
        // supervisor 复用同一通道——UI/托盘的 CoreHandle 恒指向同一发送端）。
        // Err 分支不可达（运行体自行消化会话级错误并日志化，规则 4）——
        // `unreachable_run_session_error` 兜底以 SessionClosed 收束类型面。
        let (end, rx_back) = match mode {
            RuntimeMode::Host => {
                // 自动通告（docs/02 §2.1，FR-5.2 通告侧）：按需启动（角色往返
                // 重建）；--no-discover 全关；层级开关取 config net.discovery。
                // 失败告警降级（仅监听不通告，规则 4）。
                let secrets = match &deps.secrets {
                    Some((_, psk, _)) => *psk,
                    None => {
                        tracing::error!(target: "app::main", "主控启动缺少 PSK（不可达：等待相已确保）");
                        return;
                    }
                };
                if announcer.is_none() && !deps.no_discover {
                    // service_port 依赖实际绑定结果——先监听再启动通告
                    //（与 M3 首装顺序一致：bound.local_addr.port()）。
                    let bind = SocketAddr::from(([0, 0, 0, 0], config.net.port));
                    let identity = runtime_identity(&deps.display_name);
                    match session_rt::host_listen(session_rt::HostOptions {
                        bind,
                        psk: secrets,
                        identity: identity.clone(),
                        throttle: xcontrol_net::ThrottleConfig::default(),
                        keymap: config.keymap.clone(),
                        hotkeys: config.hotkeys.clone(),
                        // FR-5.4 白名单直传 serve 层（accept 后握手前过滤）。
                        allowlist: config.net.allowlist.clone(),
                        discovery: config.net.discovery.clone(),
                    })
                    .await
                    {
                        Ok(bound) => {
                            tracing::info!(target: "app::main", addr = %bound.local_addr,
                                "主控就绪：等待从机握手（窗口关闭不退出进程，托盘/Ctrl+C 收尾）");
                            let opts = xcontrol_net::AnnouncerOptions {
                                discovery_port: config.net.port,
                                service_port: bound.local_addr.port(),
                                name: deps.display_name.clone(),
                                instance_id: DEMO_INSTANCE_UUID,
                                caps: identity.caps.bits(),
                                mdns: config.net.discovery.contains(&DiscoveryMethod::Mdns),
                                beacon: config.net.discovery.contains(&DiscoveryMethod::Beacon),
                                allowlist: Vec::new(),
                                period_unpaired: xcontrol_net::BEACON_PERIOD_UNPAIRED,
                                period_paired: xcontrol_net::BEACON_PERIOD_PAIRED,
                            };
                            announcer = match xcontrol_net::Announcer::start(opts).await {
                                Ok(started) => {
                                    tracing::info!(target: "app::discover",
                                        mdns = config.net.discovery.contains(&DiscoveryMethod::Mdns),
                                        beacon = config.net.discovery.contains(&DiscoveryMethod::Beacon),
                                        port = config.net.port,
                                        "通告开启：mDNS + beacon（5s→60s 满配对切换）");
                                    Some(Arc::new(started))
                                }
                                Err(error) => {
                                    tracing::warn!(target: "app::discover", %error,
                                        "通告启动失败（降级为仅监听；--no-discover 可显式关闭）");
                                    None
                                }
                            };
                            bound
                                .run_session(Arc::clone(&bus), rx, announcer.clone(), deps.config.clone())
                                .await
                                .unwrap_or_else(|error| {
                                    tracing::error!(target: "session::rt", %error, "主控会话循环退出");
                                    unreachable_run_session_error()
                                })
                        }
                        Err(error) => {
                            tracing::error!(target: "app::main", %error, "主控监听失败（运行时收尾）");
                            // 监听失败：通道未移入会话体，原样归还（外层收尾）。
                            (session_rt::SessionEnd::SessionClosed, rx)
                        }
                    }
                } else {
                    if deps.no_discover {
                        tracing::info!(target: "app::discover", "--no-discover：通告关闭（仅监听）");
                    }
                    let bind = SocketAddr::from(([0, 0, 0, 0], config.net.port));
                    let identity = runtime_identity(&deps.display_name);
                    match session_rt::host_listen(session_rt::HostOptions {
                        bind,
                        psk: secrets,
                        identity,
                        throttle: xcontrol_net::ThrottleConfig::default(),
                        keymap: config.keymap.clone(),
                        hotkeys: config.hotkeys.clone(),
                        allowlist: config.net.allowlist.clone(),
                        discovery: config.net.discovery.clone(),
                    })
                    .await
                    {
                        Ok(bound) => {
                            tracing::info!(target: "app::main", addr = %bound.local_addr,
                                "主控就绪：等待从机握手（窗口关闭不退出进程，托盘/Ctrl+C 收尾）");
                            bound
                                .run_session(Arc::clone(&bus), rx, announcer.clone(), deps.config.clone())
                                .await
                                .unwrap_or_else(|error| {
                                    tracing::error!(target: "session::rt", %error, "主控会话循环退出");
                                    unreachable_run_session_error()
                                })
                        }
                        Err(error) => {
                            tracing::error!(target: "app::main", %error, "主控监听失败（运行时收尾）");
                            (session_rt::SessionEnd::SessionClosed, rx)
                        }
                    }
                }
            }
            RuntimeMode::Client(addr) => {
                // 离开主控角色：停通告（mDNS 注册与 beacon 广播随 stop 撤销）。
                if let Some(started) = announcer.take() {
                    started.stop();
                }
                let identity = runtime_identity(&deps.display_name);
                let (secrets, code) = match &deps.secrets {
                    Some((code, psk, _)) => (*psk, code.clone()),
                    None => {
                        tracing::error!(target: "app::main", "从机启动缺少安全码（不可达：等待相已确保）");
                        return;
                    }
                };
                match addr.or(deps.manual_addr) {
                    Some(addr) => {
                        // 手动路径（FR-5.2 手动）：直连 + 断线退避重连（FR-5.3）。
                        tracing::info!(target: "app::main", %addr,
                            "从机手动直连（断线自动重连，FR-5.3；错误码握手将被拒绝并记录）");
                        session_rt::client_session(
                            session_rt::ClientOptions {
                                addr,
                                psk: secrets,
                                identity,
                                throttle: xcontrol_net::ThrottleConfig::default(),
                                keymap: config.keymap.clone(),
                                hotkeys: config.hotkeys.clone(),
                            },
                            Arc::clone(&bus),
                            rx,
                            deps.config.clone(),
                        )
                        .await
                        .unwrap_or_else(|error| {
                            tracing::error!(target: "session::rt", %error, "从机会话退出");
                            unreachable_run_session_error()
                        })
                    }
                    None => {
                        // 自动发现（FR-5.1/5.2/5.3）：扫描 → 候选 → 逐个握手。
                        // 层开关取 config net.discovery（docs/01 §5：mdns/beacon/
                        // scan 三值 → 各层开关；"scan-off" = 显式关扫描）。
                        let mdns = config.net.discovery.contains(&DiscoveryMethod::Mdns);
                        let udp = config.net.discovery.contains(&DiscoveryMethod::Beacon);
                        let tcp_fallback = config.net.discovery.contains(&DiscoveryMethod::Scan);
                        tracing::info!(target: "app::main", port = config.net.port,
                            mdns, udp, tcp_fallback,
                            "从机自动发现模式（层开关取 config net.discovery，docs/02 §2.2）");
                        session_rt::client_discover(
                            session_rt::DiscoverOptions {
                                discovery_port: config.net.port,
                                cidrs: config.net.cidrs.clone(),
                                allowlist: Vec::new(),
                                mdns,
                                udp,
                                tcp_fallback,
                                fallback_psk: secrets,
                                throttle: xcontrol_net::ThrottleConfig::default(),
                                identity,
                                keymap: config.keymap.clone(),
                                hotkeys: config.hotkeys.clone(),
                                // FR-5.5 已记忆主控基线（每轮另读共享单元最新值）。
                                confirmed_host: config.net.confirmed_host.clone(),
                            },
                            code,
                            Arc::clone(&bus),
                            rx,
                            deps.config.clone(),
                        )
                        .await
                        .unwrap_or_else(|error| {
                            tracing::error!(target: "session::rt", %error, "从机发现循环退出");
                            unreachable_run_session_error()
                        })
                    }
                }
            }
        };
        rx = rx_back;

        match end {
            session_rt::SessionEnd::SessionClosed => {
                tracing::info!(target: "app::main", "运行时收尾（命令通道关闭/监听停止）");
                break;
            }
            session_rt::SessionEnd::RoleSwitch => {
                tracing::info!(target: "app::main", from = ?mode,
                    "托盘角色切换：停当前运行时 → 翻转角色 → 重建（FR-1.1）");
                mode = mode.flipped();
            }
            session_rt::SessionEnd::ConfigReload => {
                // net 段变化的重建（docs/06 §4.1）：旧通告器按旧端口/层开关广播
                //——先停，下一轮主控分支按共享单元最新值重启（否则换端口后仍
                // 通告旧 service_port，从机按旧地址握手必败）。
                if let Some(started) = announcer.take() {
                    started.stop();
                }
                tracing::info!(target: "app::main", mode = ?mode,
                    "配置重建（docs/06 §4.1）：按共享单元最新值重建运行时（同模式，通告器随重建重启）");
            }
        }
    }
    // 收尾兜底：通告器停（SessionClosed 正常路径与错误收尾统一出口）。
    if let Some(started) = announcer.take() {
        started.stop();
    }
}

/// 会话运行体 `Err` 的类型面兜底（运行体不向上抛会话级错误——规则 4；此
/// 分支仅在未来新增 Err 路径时兜住类型，返回 SessionClosed 收束 supervisor）。
#[cold]
#[inline(never)]
fn unreachable_run_session_error() -> (
    session_rt::SessionEnd,
    tokio::sync::mpsc::Receiver<state::CoreCommand>,
) {
    // 运行体 Err 时命令通道随 future 释放（UI 退出路径语义一致）——此处
    // 建一个空通道占位，supervisor 的 SessionClosed 分支随即收尾不再复用。
    let (_tx, rx) = tokio::sync::mpsc::channel(state::COMMAND_CHANNEL_BUFFER);
    (session_rt::SessionEnd::SessionClosed, rx)
}

/// 显示名解析：`--name` 优先；缺省主机名（env `HOSTNAME`/`COMPUTERNAME` →
/// `hostname` 命令 → 常量兜底；零新增依赖）。发现通告（beacon name / mDNS
/// TXT / 会话协商 Hello）共用（docs/02 §2.1、PRD `--name`）。
#[must_use]
fn resolve_display_name(arg: Option<&str>) -> String {
    if let Some(name) = arg {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_owned();
        }
    }
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !name.is_empty() {
            return name;
        }
    }
    "xcontrol".to_owned()
}

/// 身份构造（显示名来自 `--name`/主机名；instance id 用演示 UUID，逐设备
/// UUID + 配置持久化属 M4，docs/02 §3.1）——运行时 supervisor 重建共用。
fn runtime_identity(display_name: &str) -> xcontrol_core::session::Identity {
    loopback::loopback_identity(display_name, DEMO_INSTANCE_UUID)
}

/// 安全码解析 + PSK 派生（`--code` 优先；否则走配置指向的存储后端）。
///
/// 返回 `None` = 无码可用（向导等待相重试）；文案不含码内容
/// （error-handling.md 红线 6）。PSK 派生（argon2）在此发生一次，结果由
/// [`RuntimeDeps`] 缓存（docs/02 §3.1"派生一次"）。
fn resolve_secrets(arg: Option<&str>, config: &Config) -> Option<(SecretCode, [u8; 32])> {
    let code = match arg {
        Some(code) => SecretCode::new(code),
        None => {
            let data_dir = xcontrol_config::default_data_dir()?;
            let secret_ref = SecretRef::parse(&config.security_code_ref)?;
            let store = build_secret_store(&secret_ref, &data_dir);
            store.retrieve().ok()?
        }
    };
    let psk = xcontrol_net::derive_psk(code.expose(), &DEMO_INSTANCE_UUID).ok()?;
    Some((code, psk))
}

/// 配置加载：`--config` 优先；文件不存在用默认配置（回环演示零配置可跑）。
/// 返回实际读取的配置路径（`None` = 默认配置兜底——默认路径不可得或文件不存在；
/// 具体告警在日志装配后由调用方记录，subscriber 未就绪时 tracing 事件无处可去）。
fn load_config(path: Option<&Path>) -> anyhow::Result<(Config, Option<PathBuf>)> {
    let resolved: Option<PathBuf> = match path {
        Some(p) => Some(p.to_path_buf()),
        None => xcontrol_config::default_config_path(),
    };
    match resolved {
        Some(path) if path.exists() => {
            let config = load(&path)
                .map_err(|error| anyhow::anyhow!("配置加载失败（{}）：{error}", path.display()))?;
            Ok((config, Some(path)))
        }
        _ => Ok((Config::default(), None)),
    }
}

/// 向导完成的目标配置路径（11-18-m4-ui check 修复）：向导流的判定依据是
/// "配置文件不存在"（[`load_config`] 此时返回 `None`），但向导完成必须把新
/// config 写到确定位置——`--config` 显式路径优先，缺省系统默认路径。
#[must_use]
/// config 驱动/向导/显式模式三路的初始运行时角色推导（11-20 修复：
/// 配置过的主控重启曾掉进从机发现模式——兜底 `Client(None)` 只属于向导等待相）。
fn initial_runtime_mode(mode: Option<&Mode>, wizard: bool, role: Role) -> RuntimeMode {
    match mode {
        Some(Mode::Host) => RuntimeMode::Host,
        Some(Mode::Client(addr)) => RuntimeMode::Client(*addr),
        Some(Mode::Loopback) => RuntimeMode::Client(None), // 回环不走此函数（防御）
        None if wizard => RuntimeMode::Client(None),       // 等待相默认从机姿态
        None => match role {
            Role::Host => RuntimeMode::Host,
            Role::Client => RuntimeMode::Client(None),
        },
    }
}

fn wizard_target_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    explicit
        .map(PathBuf::from)
        .or_else(xcontrol_config::default_config_path)
}

/// 手写参数解析（design.md：手写解析避免 clap 重依赖）。
///
/// 返回 `Ok(None)` = 已打印 `--help`（进程以 0 退出）。
fn parse_args(args: impl Iterator<Item = OsString>) -> anyhow::Result<Option<Args>> {
    let mut mode: Option<Mode> = None;
    let mut code: Option<String> = None;
    let mut config: Option<PathBuf> = None;
    let mut log_level: Option<LogLevel> = None;
    let mut name: Option<String> = None;
    let mut no_discover = false;
    let mut headless = false;
    let mut auto_quit_ms: Option<u64> = None;
    let mut iter = args.peekable();

    while let Some(raw) = iter.next() {
        let arg = raw
            .into_string()
            .map_err(|_| anyhow::anyhow!("参数含无效 UTF-8，无法解析"))?;
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--loopback" => set_mode(&mut mode, Mode::Loopback, &arg)?,
            "--host" => set_mode(&mut mode, Mode::Host, &arg)?,
            // `--client` 带可选值：下一个 token 是合法地址 → 手动路径（消费它，
            // FR-5.2）；旗标/缺值 → 发现模式（token 留给后续旗标解析，FR-5.1）；
            // 其余 token（非旗标非地址）按预期地址解析失败报错。
            "--client" => {
                let manual = match iter.peek() {
                    Some(next) => {
                        let text = next.to_string_lossy().into_owned();
                        match text.parse::<SocketAddr>() {
                            Ok(addr) => {
                                let _ = iter.next(); // 消费已识别的地址
                                Some(addr)
                            }
                            Err(_) if text.starts_with('-') => None, // 旗标：发现模式
                            Err(_) => anyhow::bail!(
                                "--client 需要合法地址 ip:port（或省略走自动发现），收到 {text:?}"
                            ),
                        }
                    }
                    None => None,
                };
                set_mode(&mut mode, Mode::Client(manual), &arg)?;
            }
            "--code" => code = Some(next_value(&mut iter, &arg)?),
            "--config" => config = Some(PathBuf::from(next_value(&mut iter, &arg)?)),
            "--log-level" => {
                let value = next_value(&mut iter, &arg)?;
                log_level = Some(LogLevel::parse(&value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--log-level 需要五值之一（{}），收到 {value:?}",
                        LogLevel::VARIANTS.join("/")
                    )
                })?);
            }
            "--name" => name = Some(next_value(&mut iter, &arg)?),
            "--no-discover" => no_discover = true,
            // FR-6.4 无 UI 运行时（docs/06 §2：运行时无 UI 而非编译面分离）。
            "--headless" => headless = true,
            "--auto-quit-ms" => {
                let value = next_value(&mut iter, &arg)?;
                let ms: u64 = value.parse().map_err(|_| {
                    anyhow::anyhow!("--auto-quit-ms 需要正整数毫秒，收到 {value:?}")
                })?;
                if ms == 0 {
                    anyhow::bail!("--auto-quit-ms 必须大于 0");
                }
                auto_quit_ms = Some(ms);
            }
            other => anyhow::bail!("未知参数 {other:?}；--help 查看用法"),
        }
    }

    // 模式可缺省（11-18-m4-ui，FR-6.3 配置驱动/向导流）：main 按 config.role
    // 决定运行姿态或进入向导等待相；--loopback/--host/--client 三者仍互斥。
    Ok(Some(Args {
        mode,
        code,
        config,
        log_level,
        name,
        no_discover,
        headless,
        auto_quit_ms,
    }))
}

/// 模式互斥（FR-1.5：同进程一次只扮演一种角色）。
fn set_mode(slot: &mut Option<Mode>, value: Mode, flag: &str) -> anyhow::Result<()> {
    if slot.is_some() {
        anyhow::bail!("模式参数冲突：{flag}（--loopback / --host / --client 只能三选一）");
    }
    *slot = Some(value);
    Ok(())
}

/// 取旗标的值参数。
fn next_value(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> anyhow::Result<String> {
    iter.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} 缺少值参数（--help 查看用法）"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("{flag} 的值含无效 UTF-8"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn args<'a>(list: &'a [&'a str]) -> impl Iterator<Item = OsString> + 'a {
        list.iter().map(OsString::from)
    }

    /// 合法组合逐个解析通过（含手动/发现两种 --client 形态与新旗标）。
    #[test]
    fn parses_all_modes_and_flags() {
        let parsed = parse_args(args(&["--loopback"]))
            .expect("解析成功")
            .expect("非 help");
        assert_eq!(parsed.mode, Some(Mode::Loopback));
        assert_eq!(parsed.auto_quit_ms, None);

        // 无模式（11-18-m4-ui FR-6.3）：可解析（main 按 config.role / 向导分流）。
        let parsed = parse_args(args(&[])).expect("解析成功").expect("非 help");
        assert_eq!(parsed.mode, None);
        assert_eq!(parsed.code, None);

        // 手动地址（FR-5.2）。
        let parsed = parse_args(args(&[
            "--client",
            "127.0.0.1:4243",
            "--code",
            "demo-code",
            "--auto-quit-ms",
            "3000",
        ]))
        .expect("解析成功")
        .expect("非 help");
        assert_eq!(
            parsed.mode,
            Some(Mode::Client(Some(
                "127.0.0.1:4243".parse().expect("常量地址")
            )))
        );
        assert_eq!(parsed.code.as_deref(), Some("demo-code"));
        assert_eq!(parsed.auto_quit_ms, Some(3000));

        // 自动发现（FR-5.1）：--client 后跟旗标 → 地址不消费，旗标正常解析。
        let parsed = parse_args(args(&["--client", "--code", "demo-code"]))
            .expect("解析成功")
            .expect("非 help");
        assert_eq!(parsed.mode, Some(Mode::Client(None)));
        assert_eq!(parsed.code.as_deref(), Some("demo-code"));

        // --client 收尾（无后续 token）→ 发现模式。
        let parsed = parse_args(args(&["--host", "--client"])).expect_err("互斥被拒");
        assert!(parsed.to_string().contains("只能三选一"));

        // --host + 新旗标。
        let parsed = parse_args(args(&[
            "--host",
            "--config",
            "/tmp/x.toml",
            "--name",
            "my-mac",
            "--no-discover",
        ]))
        .expect("解析成功")
        .expect("非 help");
        assert_eq!(parsed.mode, Some(Mode::Host));
        assert_eq!(parsed.config.as_deref(), Some(Path::new("/tmp/x.toml")));
        assert_eq!(parsed.name.as_deref(), Some("my-mac"));
        assert!(parsed.no_discover);

        // --log-level（NFR-8 覆盖参数）：五值可解析。
        let parsed = parse_args(args(&["--loopback", "--log-level", "debug"]))
            .expect("解析成功")
            .expect("非 help");
        assert_eq!(parsed.log_level, Some(LogLevel::Debug));
    }

    /// 模式互斥 / 未知参数 / 非法值 → 中文错误文案（缺模式自 11-18-m4-ui 起合法
    /// ——配置驱动/向导流，见 FR-6.3）。
    #[test]
    fn rejects_invalid_combinations_with_readable_errors() {
        let error = parse_args(args(&["--loopback", "--host"])).expect_err("互斥被拒");
        assert!(error.to_string().contains("只能三选一"));

        let error = parse_args(args(&["--warp"])).expect_err("未知参数被拒");
        assert!(error.to_string().contains("未知参数"));

        // 非法地址形态（非旗标、非地址）→ 明确报错（发现模式只由省略值触发）。
        let error = parse_args(args(&["--client", "not-an-addr"])).expect_err("非法地址被拒");
        assert!(error.to_string().contains("ip:port"));

        let error = parse_args(args(&["--auto-quit-ms", "0"])).expect_err("0 被拒");
        assert!(error.to_string().contains("大于 0"));

        let error = parse_args(args(&["--config"])).expect_err("缺值被拒");
        assert!(error.to_string().contains("缺少值参数"));

        let error =
            parse_args(args(&["--loopback", "--log-level", "verbose"])).expect_err("未知级别被拒");
        assert!(error.to_string().contains("五值之一"));
    }

    /// 显示名解析：--name 优先；空值回落主机名链；无环境时兜底常量。
    #[test]
    fn display_name_resolution() {
        assert_eq!(resolve_display_name(Some("  my-mac ")), "my-mac");
        // 空白 --name：回落主机名链（任何分支都产出非空名）。
        assert!(!resolve_display_name(Some("  ")).is_empty());
        // 无 --name：env/hostname 命令/常量兜底。
        assert!(!resolve_display_name(None).is_empty());
    }

    /// --help 返回 None（进程 0 退出的信号）。
    #[test]
    fn help_returns_none() {
        assert!(parse_args(args(&["--help"])).expect("解析成功").is_none());
        assert!(parse_args(args(&["-h"])).expect("解析成功").is_none());
    }

    /// 配置加载：不存在的路径回退默认配置（不报错；路径返回 None 供装配后告警）。
    #[test]
    fn missing_config_falls_back_to_default() {
        let (config, path) =
            load_config(Some(Path::new("/nonexistent/xcontrol-demo.toml"))).expect("回退默认");
        assert_eq!(path, None);
        assert_eq!(config.net.port, xcontrol_config::DEFAULT_PORT);
        assert_eq!(config.general.log_level, xcontrol_config::LogLevel::Info);
    }

    /// 向导目标配置路径（11-18-m4-ui check 修复）：显式路径优先于默认路径——
    /// 向导完成必须写到确定位置，否则 `complete` 在"配置文件路径不可得"处
    /// 中断（DI 测试注入掩盖的生产接线缺口）。
    #[test]
    fn wizard_target_path_prefers_explicit() {
        assert_eq!(
            wizard_target_config_path(Some(Path::new("/tmp/wizard-demo.toml"))),
            Some(PathBuf::from("/tmp/wizard-demo.toml"))
        );
    }

    /// 配置驱动重启的角色推导（11-20 P1 回归：配置过的主控重启曾掉进
    /// 从机发现模式——兜底 Client(None) 只属于向导等待相）。
    #[test]
    fn initial_runtime_mode_config_driven_respects_role() {
        use Mode as M;
        // 配置驱动（无显式模式、非向导）：按 config.role。
        assert_eq!(
            initial_runtime_mode(None, false, Role::Host),
            RuntimeMode::Host
        );
        assert_eq!(
            initial_runtime_mode(None, false, Role::Client),
            RuntimeMode::Client(None)
        );
        // 向导等待相：恒从机姿态（完成后再按所选角色）。
        assert_eq!(
            initial_runtime_mode(None, true, Role::Host),
            RuntimeMode::Client(None)
        );
        // 显式模式优先。
        assert_eq!(
            initial_runtime_mode(Some(&M::Host), false, Role::Client),
            RuntimeMode::Host
        );
        assert_eq!(
            initial_runtime_mode(Some(&M::Client(None)), false, Role::Host),
            RuntimeMode::Client(None)
        );
    }

    /// 单实例探测（docs/06 §2 M4 最小裁定）：有监听者 → true；空闲端口 → false
    /// （bind 后立即 drop 释放内核端口——极小概率被其他进程抢占，属测试沙箱
    /// 可接受窗口）。
    ///
    /// 释放判定带 deadline 重试（2026-11-19 check 实测 flake：drop 返回后内核
    /// close 处理存在竞态窗，紧随的 connect 偶发仍成功——轮询到被拒即视为释放，
    /// 2s 宽限与平台冒烟超时同量级）。
    #[test]
    fn probe_existing_instance_detects_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("测试监听");
        let port = listener.local_addr().expect("本地地址").port();
        assert!(probe_existing_instance(port), "有监听者的端口必须探得 true");
        drop(listener);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut freed = !probe_existing_instance(port);
        while !freed && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            freed = !probe_existing_instance(port);
        }
        assert!(freed, "drop 后端口应释放（连接被拒；deadline 内未释放）");
    }
}
