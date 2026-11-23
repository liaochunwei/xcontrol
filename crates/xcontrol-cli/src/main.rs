//! `xcontrol-cli` — headless / 调试入口（无 gpui，docs/08 §1）。
//!
//! 当前能力（NFR-8，11-17-m3-status）：
//!
//! ```text
//! --status                本地只读状态查询：配置摘要 + 端口探测 + 滚动日志尾部
//!   --json                机器可读 JSON 输出（人类可读输出互斥的形态开关）
//!   --log-tail N          日志尾部行数（默认 20，上限 1000）
//! -h / --help             帮助
//! ```
//!
//! `--status` 是 FR-6.4（headless 场景）的 CLI 对应物：**不联网**——仅回环端口
//! 探测（TCP connect `127.0.0.1:<config 端口>`）与本地文件读取。端口探测语义：
//! connect 成功 = "本机有进程监听该端口"，判定为"检测到运行中的 XControl"；
//! **伪造服务会误报**（design.md 拍板可接受——不做协议握手前缀判断，避免对
//! 未知进程产生协议流量，探测保持纯传输层）。
//!
//! 配置读取走 [`xcontrol_config::load`] 完整流水线：文件缺失用默认配置并如实
//! 标注（`config_found=false`）；解析/校验错误**照常上抛**——--status 必须把
//! "配置坏了"响亮暴露，静默兜底会掩盖问题。
//!
//! 顶层错误处理按 docs/08 §2 用 anyhow（main 统一格式化输出）。
//!
//! docs/08 §2：unsafe 仅预期出现在 input / clipboard 平台绑定层，本 crate 禁用。
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde_json::json;
use xcontrol_config::{
    Config, LOG_FILE_PREFIX, Role, default_config_path, default_data_dir, load, logs_dir,
};

/// 端口探测超时（回环 connect 应毫秒级返回；500ms 为保守上界）。
const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// `--log-tail` 缺省行数（design.md：日志尾部 20 行）。
const DEFAULT_LOG_TAIL: usize = 20;

/// `--log-tail` 上限（防止把整份日志灌进终端/JSON）。
const MAX_LOG_TAIL: usize = 1000;

/// 帮助文案（中文）。
const USAGE: &str = "\
xcontrol-cli — XControl headless / 调试入口（无 UI）

用法：
  xcontrol-cli --status [--json] [--log-tail N]
  xcontrol-cli -h | --help

子命令：
  --status        本地只读状态查询（NFR-8，FR-6.4 headless 的 CLI 对应物）：
                  配置摘要 + 端口探测（是否已有进程监听）+ 滚动日志尾部。
                  不联网：仅回环 TCP connect 与本地文件读取。

--status 参数：
  --json          机器可读 JSON 输出（config/process/logs 三节）
  --log-tail N    日志尾部行数（默认 20，上限 1000）

示例：
  xcontrol-cli --status                    # 终端查看本地状态
  xcontrol-cli --status --json             # 供脚本消费
  xcontrol-cli --status --log-tail 50      # 尾部多看几行
";

/// 解析后的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Args {
    /// `--status`（含 `--json` / `--log-tail`）。
    Status {
        /// JSON 输出开关。
        json: bool,
        /// 日志尾部行数（已归一化：缺省 20）。
        log_tail: usize,
    },
}

fn main() -> anyhow::Result<()> {
    match parse_args(std::env::args_os().skip(1))? {
        None => Ok(()), // --help 已打印
        Some(Args::Status { json, log_tail }) => run_status(json, log_tail),
    }
}

/// `--status` 执行体：本地只读查询 + 结构化输出（人类可读或 JSON）。
fn run_status(as_json: bool, log_tail: usize) -> anyhow::Result<()> {
    let (config, config_path, config_found) = load_local_config()?;
    let listening = probe_port(config.net.port);
    let log_dir = default_data_dir().map(|data_dir| logs_dir(&data_dir));
    let (log_file, tail) = match &log_dir {
        Some(dir) => collect_log_tail_from(dir, log_tail),
        None => (None, Vec::new()),
    };
    let snapshot = StatusSnapshot {
        config: &config,
        config_path: config_path.as_deref(),
        config_found,
        log_dir: log_dir.as_deref(),
        log_file: log_file.as_deref(),
        tail: &tail,
        tail_requested: log_tail,
        listening,
    };
    if as_json {
        println!("{}", status_json(&snapshot)?);
    } else {
        print_human(&snapshot);
    }
    Ok(())
}

/// `--status` 输出快照（渲染入口的单一入参，人类可读/JSON 共用）。
struct StatusSnapshot<'a> {
    /// 已加载配置（默认配置兜底时 [`Config::default`]）。
    config: &'a Config,
    /// 实际读取的配置路径（None = 默认配置兜底）。
    config_path: Option<&'a Path>,
    /// 是否找到并读取了真实配置文件。
    config_found: bool,
    /// 日志目录（None = 数据目录不可得）。
    log_dir: Option<&'a Path>,
    /// 最新滚动日志文件（None = 没有日志）。
    log_file: Option<&'a Path>,
    /// 日志尾部行（旧→新顺序）。
    tail: &'a [String],
    /// 请求的尾部行数（--log-tail 归一化后）。
    tail_requested: usize,
    /// 端口探测结果（true = 本机有进程监听配置端口）。
    listening: bool,
}

/// 读默认配置（文件缺失 → 默认配置 + `config_found=false`；解析/校验错误照常
/// 上抛——见模块文档"配置读取"节）。
fn load_local_config() -> anyhow::Result<(Config, Option<PathBuf>, bool)> {
    match default_config_path() {
        Some(path) if path.exists() => {
            let config =
                load(&path).with_context(|| format!("读取配置失败（{}）", path.display()))?;
            Ok((config, Some(path), true))
        }
        _ => Ok((Config::default(), None, false)),
    }
}

/// 回环端口探测：TCP connect 成功 = 有进程监听（判为"检测到运行中的
/// XControl"）。**误报可接受**（design.md）：任何进程占用端口都算"监听中"；
/// 不做协议握手前缀判断——避免对未知进程产生协议流量，探测保持纯传输层
/// （`--status` 不联网：仅 127.0.0.1 回环，无外部地址访问）。
fn probe_port(port: u16) -> bool {
    let addr = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port);
    TcpStream::connect_timeout(&addr, PORT_PROBE_TIMEOUT).is_ok()
}

/// 最新滚动日志文件（文件名 `xcontrol.log.<YYYY-MM-DD>`——ISO 日期字典序 =
/// 时间序，取最大即最新；命名契约见 [`LOG_FILE_PREFIX`] 与 app logging.rs）。
fn newest_log_file(dir: &Path) -> Option<PathBuf> {
    let prefix = format!("{LOG_FILE_PREFIX}.");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .max_by_key(|entry| entry.file_name())
        .map(|entry| entry.path())
}

/// 日志尾部收集（测试可注入目录）：最新滚动文件 + 最后 n 行。
/// 返回 (文件路径, 行序列)；无日志文件 → (None, 空)。读取失败（文件被轮转/
/// 权限变化）按无内容报告——--status 是只读诊断，不为读日志失败而整体失败。
fn collect_log_tail_from(dir: &Path, n: usize) -> (Option<PathBuf>, Vec<String>) {
    let Some(file) = newest_log_file(dir) else {
        return (None, Vec::new());
    };
    match tail_lines(&file, n) {
        Ok(lines) => (Some(file), lines),
        Err(_) => (Some(file), Vec::new()),
    }
}

/// 读文件最后 n 行（逐日滚动文件量级为 MB 级，整读后取尾——简单可靠，
/// 内存压力可控；分块反向读取属 M6 观测打磨）。
fn tail_lines(path: &Path, n: usize) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取日志失败（{}）", path.display()))?;
    let total = content.lines().count();
    let skip = total.saturating_sub(n);
    Ok(content.lines().skip(skip).map(str::to_owned).collect())
}

/// 组装 `--json` 输出（结构契约：`config` / `process` / `logs` 三节；
/// 机器消费面——字段只增不改名，语义变化需升 `--json` 输出版本，当前 v1 隐含）。
fn status_json(snapshot: &StatusSnapshot<'_>) -> anyhow::Result<String> {
    let config = snapshot.config;
    let document = json!({
        "config": {
            "schema_version": config.version,
            "config_found": snapshot.config_found,
            "config_path": snapshot.config_path.map(|path| path.display().to_string()),
            "role": role_label(config.role),
            "security_code_ref": config.security_code_ref,
            "log_level": config.general.log_level.as_str(),
            "port": config.net.port,
            "discovery": &config.net.discovery,
            // FR-5.4/5.5（v3 内追加字段）：主控接入白名单与已记忆主控——
            // headless 场景的配置核对面（--status 只读）。
            "allowlist": &config.net.allowlist,
            "confirmed_host": &config.net.confirmed_host,
        },
        "process": {
            "port": config.net.port,
            "listening": snapshot.listening,
            "probe": "tcp-connect-127.0.0.1（成功即视为监听；伪造服务误报可接受）",
        },
        "logs": {
            "dir": snapshot.log_dir.map(|path| path.display().to_string()),
            "file": snapshot.log_file.map(|path| path.display().to_string()),
            "tail_requested": snapshot.tail_requested,
            "lines": snapshot.tail,
        },
    });
    serde_json::to_string_pretty(&document)
        .map_err(|error| anyhow::anyhow!("JSON 序列化失败：{error}"))
}

/// 人类可读输出（中文）。
fn print_human(snapshot: &StatusSnapshot<'_>) {
    let config = snapshot.config;
    println!("XControl 本地状态（--status 只读查询，不联网）");
    println!();
    match snapshot.config_path {
        Some(path) if snapshot.config_found => println!("配置（{}）：", path.display()),
        _ => println!("配置（默认配置——未找到配置文件，路径形态见 xcontrol-cli --help）："),
    }
    println!(
        "  schema 版本: v{} · 角色: {}（{}）",
        config.version,
        role_label(config.role),
        match config.role {
            Role::Host => "主控",
            Role::Client => "从机",
        }
    );
    println!(
        "  端口: {} · 发现: {}",
        config.net.port,
        config
            .net
            .discovery
            .iter()
            .map(|method| match method {
                xcontrol_config::DiscoveryMethod::Mdns => "mdns",
                xcontrol_config::DiscoveryMethod::Beacon => "beacon",
                xcontrol_config::DiscoveryMethod::Scan => "scan",
                xcontrol_config::DiscoveryMethod::ScanOff => "scan-off",
            })
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  日志级别: {} · 安全码引用: {}",
        config.general.log_level.as_str(),
        config.security_code_ref
    );
    println!(
        "  白名单: {} · 已记忆主控: {}",
        if config.net.allowlist.is_empty() {
            "未启用（接受全部来源）".to_owned()
        } else {
            config.net.allowlist.join(", ")
        },
        config
            .net
            .confirmed_host
            .as_deref()
            .unwrap_or("无（发现到多台主控时需确认）"),
    );
    println!();
    println!("进程探测（127.0.0.1:{}）：", config.net.port);
    if snapshot.listening {
        println!(
            "  检测到监听——本机可能有 XControl 运行中（任何占用该端口的进程都会命中；伪造服务误报可接受）"
        );
    } else {
        println!("  未检测到监听——本机没有进程占用端口 {}", config.net.port);
    }
    println!();
    match snapshot.log_file {
        Some(file) if snapshot.tail.is_empty() => {
            println!("日志尾部：无内容（{} 为空或不可读）", file.display());
        }
        Some(file) => {
            println!(
                "日志尾部（{}，最近 {} 行）：",
                file.display(),
                snapshot.tail.len()
            );
            for line in snapshot.tail {
                println!("  {line}");
            }
        }
        None => {
            println!(
                "日志尾部：无滚动日志文件（xcontrol-app 尚未在本机数据目录写过日志；目录 {}）",
                snapshot
                    .log_dir
                    .map_or("不可定位".to_owned(), |path| path.display().to_string())
            );
        }
    }
}

/// 角色字面值（与 config serde 字面值一致）。
fn role_label(role: Role) -> &'static str {
    match role {
        Role::Host => "host",
        Role::Client => "client",
    }
}

/// 手写参数解析（与 xcontrol-app 同款风格，免 clap 重依赖）。
///
/// 返回 `Ok(None)` = 已打印 `--help`（进程以 0 退出）。
fn parse_args(args: impl Iterator<Item = OsString>) -> anyhow::Result<Option<Args>> {
    let mut status = false;
    let mut json = false;
    let mut log_tail: Option<usize> = None;
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
            "--status" => status = true,
            "--json" => json = true,
            "--log-tail" => {
                let value = next_value(&mut iter, &arg)?;
                let parsed: usize = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--log-tail 需要正整数，收到 {value:?}"))?;
                if parsed == 0 {
                    anyhow::bail!("--log-tail 必须大于 0");
                }
                if parsed > MAX_LOG_TAIL {
                    anyhow::bail!("--log-tail 上限 {MAX_LOG_TAIL}（当前 {parsed}）");
                }
                log_tail = Some(parsed);
            }
            other => anyhow::bail!("未知参数 {other:?}；--help 查看用法"),
        }
    }

    if !status {
        if json {
            anyhow::bail!("--json 需要与 --status 一起使用（--help 查看用法）");
        }
        if log_tail.is_some() {
            anyhow::bail!("--log-tail 需要与 --status 一起使用（--help 查看用法）");
        }
        anyhow::bail!("缺少子命令：--status（--help 查看用法）");
    }
    Ok(Some(Args::Status {
        json,
        log_tail: log_tail.unwrap_or(DEFAULT_LOG_TAIL),
    }))
}

/// 取旗标的值参数（app 同款）。
fn next_value(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> anyhow::Result<String> {
    iter.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} 缺少值参数（--help 查看用法）"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("{flag} 的值含无效 UTF-8"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU32, Ordering};

    use xcontrol_config::CURRENT_CONFIG_VERSION;

    use super::*;

    /// 独立临时目录；Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "xcontrol-cli-test-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("临时目录创建");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn args<'a>(list: &'a [&'a str]) -> impl Iterator<Item = OsString> + 'a {
        list.iter().map(OsString::from)
    }

    /// 合法组合逐个解析通过；缺省 log-tail 归一化为 20。
    #[test]
    fn parses_status_flags() {
        let parsed = parse_args(args(&["--status"]))
            .expect("解析成功")
            .expect("非 help");
        assert_eq!(
            parsed,
            Args::Status {
                json: false,
                log_tail: DEFAULT_LOG_TAIL
            }
        );

        let parsed = parse_args(args(&["--status", "--json", "--log-tail", "50"]))
            .expect("解析成功")
            .expect("非 help");
        assert_eq!(
            parsed,
            Args::Status {
                json: true,
                log_tail: 50
            }
        );
    }

    /// 拒绝面：未知参数 / 缺子命令 / 孤立旗标 / 非法行数——中文错误文案。
    #[test]
    fn rejects_invalid_combinations_with_readable_errors() {
        let error = parse_args(args(&["--warp"])).expect_err("未知参数被拒");
        assert!(error.to_string().contains("未知参数"));

        let error = parse_args(args(&[])).expect_err("缺子命令被拒");
        assert!(error.to_string().contains("缺少子命令"));

        let error = parse_args(args(&["--json"])).expect_err("孤立 --json 被拒");
        assert!(error.to_string().contains("需要与 --status"));

        let error = parse_args(args(&["--log-tail", "5"])).expect_err("孤立 --log-tail 被拒");
        assert!(error.to_string().contains("需要与 --status"));

        let error = parse_args(args(&["--status", "--log-tail", "0"])).expect_err("0 被拒");
        assert!(error.to_string().contains("大于 0"));

        let error = parse_args(args(&["--status", "--log-tail", "1001"])).expect_err("超上限被拒");
        assert!(error.to_string().contains("上限"));

        let error = parse_args(args(&["--status", "--log-tail", "abc"])).expect_err("非数字被拒");
        assert!(error.to_string().contains("正整数"));

        let error = parse_args(args(&["--log-tail"])).expect_err("缺值被拒");
        assert!(error.to_string().contains("缺少值参数"));
    }

    /// --help 返回 None（进程 0 退出的信号）。
    #[test]
    fn help_returns_none() {
        assert!(parse_args(args(&["--help"])).expect("解析成功").is_none());
        assert!(parse_args(args(&["-h"])).expect("解析成功").is_none());
    }

    /// 尾部读取：恰好 n 行取尾；n 超总量取全量；空文件返回空。
    #[test]
    fn tail_lines_take_last_n() {
        let dir = TempDir::new("tail");
        let path = dir.path().join("xcontrol.log.2026-11-18");
        let content = (0..30)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content).expect("写入");

        let tail = tail_lines(&path, 20).expect("读取成功");
        assert_eq!(tail.len(), 20);
        assert_eq!(tail[0], "line-10", "前 10 行被跳过");
        assert_eq!(tail[19], "line-29");

        let tail = tail_lines(&path, 100).expect("读取成功");
        assert_eq!(tail.len(), 30, "n 超总量取全量");

        let empty = dir.path().join("xcontrol.log.2026-11-17");
        fs::write(&empty, "").expect("写入空文件");
        let tail = tail_lines(&empty, 20).expect("读取成功");
        assert!(tail.is_empty());
    }

    /// 最新文件选择：ISO 日期字典序 = 时间序；只认滚动文件前缀；目录跳过。
    #[test]
    fn newest_log_file_picks_latest_by_name() {
        let dir = TempDir::new("newest");
        for name in [
            "xcontrol.log.2026-11-01",
            "xcontrol.log.2026-11-17",
            "xcontrol.log.2026-11-09",
        ] {
            fs::write(dir.path().join(name), name).expect("写入");
        }
        fs::write(dir.path().join("other.txt"), "unrelated").expect("写入");
        fs::create_dir_all(dir.path().join("xcontrol.log.dir")).expect("子目录");

        let newest = newest_log_file(dir.path()).expect("存在滚动日志");
        assert_eq!(
            newest.file_name().expect("文件名"),
            "xcontrol.log.2026-11-17"
        );

        let empty = TempDir::new("newest-empty");
        assert_eq!(newest_log_file(empty.path()), None);
    }

    /// 尾部收集（注入目录）：最新文件的最后 n 行；目录缺日志 → (None, 空)。
    #[test]
    fn collect_log_tail_reads_newest_file() {
        let dir = TempDir::new("collect");
        let older = dir.path().join("xcontrol.log.2026-11-17");
        fs::write(&older, "old-1\nold-2\n").expect("写入");
        let newest = dir.path().join("xcontrol.log.2026-11-18");
        let content = (0..30)
            .map(|i| format!("fresh-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&newest, content).expect("写入");

        let (file, lines) = collect_log_tail_from(dir.path(), 5);
        assert_eq!(file.as_deref(), Some(newest.as_path()));
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "fresh-25");

        let none = TempDir::new("collect-none");
        let (file, lines) = collect_log_tail_from(none.path(), 20);
        assert_eq!(file, None);
        assert!(lines.is_empty());
    }

    /// 端口探测：占用中 → true；释放后 → false（回环、零联网面）。
    #[test]
    fn probe_port_detects_listener_state() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("占位监听");
        let port = listener.local_addr().expect("地址").port();
        assert!(probe_port(port), "有监听 → 探测成功");

        drop(listener);
        // close(2) 与后续 connect 之间存在竞态窗口（fd 尚未完全关闭时 connect
        // 可能仍成功）：轮询到探测转败为止（至多 2s）。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut released = false;
        while std::time::Instant::now() < deadline {
            if !probe_port(port) {
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(released, "无监听 → 探测失败（端口 {} 已释放）", port);
    }

    /// JSON 输出：三节结构 + 字段形态（默认配置快照 + 无日志路径）。
    #[test]
    fn status_json_structure() {
        let config = Config::default();
        let snapshot = StatusSnapshot {
            config: &config,
            config_path: None,
            config_found: false,
            log_dir: None,
            log_file: None,
            tail: &[],
            tail_requested: DEFAULT_LOG_TAIL,
            listening: false,
        };
        let text = status_json(&snapshot).expect("JSON 生成");
        let value: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        let config_json = &value["config"];
        assert_eq!(
            config_json["schema_version"], CURRENT_CONFIG_VERSION,
            "schema 版本跟随 crate 常量（v3）"
        );
        assert_eq!(config_json["config_found"], false);
        assert_eq!(config_json["role"], "client");
        assert_eq!(config_json["log_level"], "info");
        assert_eq!(config_json["port"], 4243);
        assert_eq!(
            config_json["discovery"],
            json!(["mdns", "beacon"]),
            "发现机制走 serde 字面值"
        );
        assert_eq!(
            config_json["allowlist"],
            json!([]),
            "白名单默认空表进 JSON（FR-5.4）"
        );
        assert_eq!(
            config_json["confirmed_host"],
            serde_json::Value::Null,
            "无记忆主控为 null（FR-5.5）"
        );
        assert_eq!(value["process"]["listening"], false);
        assert_eq!(value["logs"]["file"], serde_json::Value::Null);
        assert_eq!(value["logs"]["lines"], json!([]));
    }

    /// JSON 输出：带路径/尾部行的完整形态（config_found 与行序列进 JSON）。
    #[test]
    fn status_json_includes_paths_and_lines() {
        let config = Config::default();
        let tail = ["2026-11-18 10:00:00 INFO  [app::main] 启动".to_owned()];
        let snapshot = StatusSnapshot {
            config: &config,
            config_path: Some(Path::new("/tmp/xcontrol/config.toml")),
            config_found: true,
            log_dir: Some(Path::new("/tmp/xcontrol/logs")),
            log_file: Some(Path::new("/tmp/xcontrol/logs/xcontrol.log.2026-11-18")),
            tail: &tail,
            tail_requested: 20,
            listening: true,
        };
        let text = status_json(&snapshot).expect("JSON 生成");
        let value: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        assert_eq!(value["config"]["config_found"], true);
        assert_eq!(value["config"]["config_path"], "/tmp/xcontrol/config.toml");
        assert_eq!(value["process"]["listening"], true);
        assert_eq!(value["logs"]["dir"], "/tmp/xcontrol/logs");
        assert_eq!(
            value["logs"]["file"],
            "/tmp/xcontrol/logs/xcontrol.log.2026-11-18"
        );
        assert!(
            !value["logs"]["lines"][0]
                .as_str()
                .expect("字符串行")
                .is_empty(),
            "行序列进 JSON：{value}"
        );
    }

    /// 人类可读输出：关键信息全部出现（角色/端口/探测结论/尾部行）。
    #[test]
    fn human_output_contains_summary() {
        let config = Config::default();
        let snapshot = StatusSnapshot {
            config: &config,
            config_path: None,
            config_found: false,
            log_dir: None,
            log_file: None,
            tail: &[],
            tail_requested: DEFAULT_LOG_TAIL,
            listening: false,
        };
        // print_human 打到 stdout；此处只断言"不 panic + 无日志分支文案可生成"
        // ——完整输出走本机冒烟（implement.md 验证项）。
        print_human(&snapshot);
    }
}
