//! tracing 装配：stdout fmt 层 + UI 日志页环形缓冲层 + 滚动文件层（docs/06 §6、
//! NFR-8"结构化日志（tracing），连接/延迟/错误计数暴露为 --status 输出与日志"）。
//!
//! - 子系统命名纪律：`app::`（UI/CLI/loopback 装配）、`session::`（会话运行时）、
//!   `net::`（握手/传输）——发布点用显式 `target:` 声明；命名审计表见
//!   `.trellis/spec/rust-core/logging-targets.md`；
//! - 级别过滤（NFR-8）：`EnvFilter` 全局指令 = config `[general].log_level`
//!   （`--log-level` 覆盖）+ `XCONTROL_LOG` 追加指令（子系统微调，env 风格如
//!   `net::=debug`——EnvFilter target 前缀匹配）。RUST_LOG **有意不读**：级别
//!   口径必须单一（config + 显式覆盖），环境残留不得改变进程行为；
//!   **运行时改级**（11-18-m4-ui，docs/06 §4.1 生效时点表）：EnvFilter 包一层
//!   `tracing_subscriber::reload`，句柄存进程级单例——设置页日志级别改动经
//!   [`set_level`] 立即重建过滤（`RuntimeConfigUpdate` 消费点调用）；
//! - 滚动文件（NFR-8）：`tracing-appender` daily rotation 写 `<data_dir>/logs/`
//!   （[`xcontrol_config::logs_dir`]，前缀 [`xcontrol_config::LOG_FILE_PREFIX`]，
//!   实际文件形如 `xcontrol.log.2026-11-18`）；stdout fmt 层并存（前台看 stdout，
//!   后台文件全量）。appender 不带保留策略——自写清理（启动 + 每日：删
//!   [`LOG_RETENTION_DAYS`] 天前的滚动文件，design.md 拍板 N=14）；
//! - 脱敏纪律（error-handling.md 规则 6）：本模块对**事件字段名**做机械脱敏
//!   （[`SENSITIVE_FIELD_WORDS`] 命中 → 值替换 `[REDACTED]`，大小写/分隔符/驼峰
//!   不敏感）——安全码不入日志由 `SecretCode` 类型纪律保证，此层是防未来手滑的
//!   防御纵深；`message` 内容不做文本扫描（内嵌脱敏靠纪律 + 命名审计，
//!   design.md 拍板"仅拦截 struct field"）。

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer, reload};
use xcontrol_config::{LOG_FILE_PREFIX, LogLevel, logs_dir as logs_dir_of};

/// UI 日志页环形缓冲容量（最近 N 条；超过淘汰最旧）。
pub const RING_CAPACITY: usize = 500;

/// 敏感字段名黑名单（design.md 拍板五词 + 11-17-m3-status check 评审增补
/// `psk`/`key`：本仓库协议即 XXpsk0，`psk`/`key` 是最可能的未来密钥字段名，
/// 且 `psk` 段零误伤面、`key` 误伤面受"宁可过度脱敏"纪律覆盖）。
/// 按"字段名分段后逐段比对"命中（大小写不敏感，见 [`is_sensitive_field_name`]）；
/// 宁可过度脱敏（如 `error_code` 也命中 "code" 段）不可漏报——观测性损失可接受，
/// 泄露不可接受。
const SENSITIVE_FIELD_WORDS: [&str; 7] = [
    "code",
    "secret",
    "password",
    "token",
    "passphrase",
    "psk",
    "key",
];

/// 滚动日志保留天数（design.md：删 N 天前的 `xcontrol.log.*` 文件；config
/// 可选覆盖留待后续任务，当前为常量）。
const LOG_RETENTION_DAYS: u64 = 14;

/// 每日清理周期（启动时先立即清一次，此后每 24h 一清）。
const CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// 日志装配设置（main 解析 config + CLI 覆盖后传入；logging.rs 只消费不解析）。
#[derive(Debug, Clone)]
pub struct LogSettings {
    /// 全局级别（config `[general].log_level`；`--log-level` 覆盖后传入）。
    pub level: LogLevel,
    /// 滚动文件根目录（`xcontrol_config::default_data_dir()` 的结果；`None` =
    /// 不落盘，仅 stdout/UI——测试与无数据目录环境）。
    pub file_dir: Option<PathBuf>,
}

/// 日志装配句柄：持有滚动文件 worker guard——进程生命周期内必须存活
/// （提前 drop 会丢掉 worker 缓冲中未落盘的日志；main 以绑定持有至退出）。
#[derive(Debug)]
pub struct LoggingGuard {
    _file_worker: Option<WorkerGuard>,
}

/// 共享环形日志缓冲（UI 日志页数据源；`snapshot()` 每次渲染拉取）。
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl LogBuffer {
    /// 空缓冲。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一行（超容量淘汰最旧）。
    pub fn push(&self, line: String) {
        let mut lines = self.lock();
        if lines.len() >= RING_CAPACITY {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// 全部行（渲染用快照；旧→新顺序）。
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    /// 清空缓冲（关于页「清空日志文件」按钮同步清页面显示——11-21 实测反馈）。
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// 行数（UI 变更检测用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<String>> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 捕获 tracing 事件进环形缓冲的 Layer（与 stdout fmt 层并联注册）。
struct UiLogLayer {
    buffer: LogBuffer,
}

/// 字段访问器：拼装 `message` + `key=value` 序列（渲染共用；敏感字段由
/// [`RedactingVisitor`] 在外层替换后再进入本 visitor）。
struct LineVisitor {
    parts: Vec<String>,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.parts.push(format!("{value:?}"));
        } else {
            self.parts.push(format!("{}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.parts.push(value.to_owned());
        } else {
            self.parts.push(format!("{}={value}", field.name()));
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_str(field, &value.to_string());
    }
}

/// 字段名分段：按非字母数字分隔符（`_`/`-`/`.`/空格等）与驼峰边界（小写→大写）
/// 切词——`security_code` / `security-code` / `securityCode` / `SecurityCode`
/// 都拆出 `security` 与 `code` 两段，逐段比对黑名单。
fn name_segments(name: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut prev_lowercase = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lowercase && !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            current.push(ch);
            prev_lowercase = ch.is_lowercase();
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
            prev_lowercase = false;
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// 字段名是否命中敏感黑名单（大小写不敏感；分段规则见 [`name_segments`]）。
fn is_sensitive_field_name(name: &str) -> bool {
    name_segments(name)
        .iter()
        .any(|segment| SENSITIVE_FIELD_WORDS.contains(&segment.to_lowercase().as_str()))
}

/// 字段访问器装饰器：敏感字段 → 值替换 `[REDACTED]` 后转发内层 visitor
/// （防御纵深——发布点纪律之外的一层机械保险，NFR-8；覆盖全部
/// `record_*` 原语，数值/布尔/错误路径同样拦截）。
struct RedactingVisitor<'a> {
    inner: &'a mut dyn Visit,
}

impl Visit for RedactingVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_debug(field, value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_str(field, value);
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_error(field, value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_i64(field, value);
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_u64(field, value);
        }
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_i128(field, value);
        }
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_u128(field, value);
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_bool(field, value);
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if is_sensitive_field_name(field.name()) {
            self.inner.record_str(field, "[REDACTED]");
        } else {
            self.inner.record_f64(field, value);
        }
    }
}

/// 事件渲染：与 UI 日志页同构的紧凑行（`HH:MM:SS LEVEL [target] 字段…`），
/// 脱敏内建（[`RedactingVisitor`]）——stdout 层与滚动文件层共用，保证三处
/// 输出口径一致（UI 环形缓冲 / stdout / 文件）。
struct RedactedFormat;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for RedactedFormat
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = LineVisitor { parts: Vec::new() };
        event.record(&mut RedactingVisitor {
            inner: &mut visitor,
        });
        writeln!(
            writer,
            "{} {:<5} [{}] {}",
            timestamp_hms(),
            event.metadata().level(),
            event.metadata().target(),
            visitor.parts.join(" ")
        )
    }
}

impl<S: Subscriber> Layer<S> for UiLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = LineVisitor { parts: Vec::new() };
        event.record(&mut RedactingVisitor {
            inner: &mut visitor,
        });
        let message = visitor.parts.join(" ");
        self.buffer.push(format!(
            "{} {:<5} [{}] {}",
            timestamp_hms(),
            event.metadata().level(),
            event.metadata().target(),
            message
        ));
    }
}

/// `HH:MM:SS` UTC 墙钟（日志行前缀；无 chrono 依赖——UNIX 秒手写取模换算）。
/// 注：M2 刻意为 UTC 而非本地时区（不引时区依赖）；stdout/文件/UI 三处口径
/// 一致。跨日可追溯性由滚动文件名（`xcontrol.log.<日期>`）承载。
fn timestamp_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

/// 组装 EnvFilter 指令串：全局级别指令（config/`--log-level`）+ 追加指令。
///
/// `extra` 来自环境变量 `XCONTROL_LOG`（子系统微调，env 风格逗号分隔，如
/// `net::=debug,app::edge=trace`——EnvFilter target 前缀匹配，`net::` 命中
/// `net::serve`/`net::connect`）。纯函数以便单测；RUST_LOG 有意不读（口径
/// 单一，见模块文档）。非法指令的处置（严格解析失败 → 告警 + 丢无效部分）
/// 见 [`init`]——本函数只负责拼串。
fn directive_string(level: LogLevel, extra: Option<&str>) -> String {
    let mut directives = level.as_str().to_owned();
    if let Some(extra) = extra.map(str::trim).filter(|value| !value.is_empty()) {
        directives.push(',');
        directives.push_str(extra);
    }
    directives
}

/// 删除 `dir` 下超过 `retention` 时长的滚动日志文件（文件名前缀
/// `xcontrol.log.`——[`LOG_FILE_PREFIX`] + tracing-appender 自动补的点；
/// 其余文件/目录一律不动）。返回删除数。
///
/// 时间由调用方注入（`now`）以便测试模拟跨日。目录不可读时返回 Err；
/// 单文件失败（权限/竞态）跳过继续——清理是尽力而为的维护操作，不因个别
/// 文件中断。
pub fn clean_old_logs(dir: &Path, now: SystemTime, retention: Duration) -> std::io::Result<usize> {
    let prefix = format!("{LOG_FILE_PREFIX}.");
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue, // 竞态：目录项在读期间消失，跳过
        };
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue; // 非滚动日志文件（用户放什么都不动）
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue; // 平台不支持 mtime：保守保留
        };
        let Ok(age) = now.duration_since(modified) else {
            continue; // 未来时间戳（时钟回拨）：保守保留
        };
        if age > retention && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// 启动后台清理线程：立即清一次，此后每 [`CLEANUP_INTERVAL`] 一清。
/// 失败只告警不中断（旧文件回收是维护性操作，规则 4）；进程退出随进程终止。
fn spawn_daily_cleaner(logs_dir: &Path) {
    let dir = logs_dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("xcontrol-log-cleaner".to_owned())
        .spawn(move || {
            let retention = Duration::from_secs(LOG_RETENTION_DAYS * 24 * 60 * 60);
            loop {
                match clean_old_logs(&dir, SystemTime::now(), retention) {
                    Ok(0) => {}
                    Ok(removed) => tracing::info!(target: "app::log", removed,
                        days = LOG_RETENTION_DAYS, "滚动日志清理完成"),
                    Err(error) => tracing::warn!(target: "app::log", %error,
                        "滚动日志清理失败（目录不可读？下一周期重试）"),
                }
                std::thread::sleep(CLEANUP_INTERVAL);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(target: "app::log", %error, "日志清理线程创建失败（跳过周期清理）");
    }
}

/// 清空全部滚动日志文件——关于页「清空日志文件」按钮的执行体。
///
/// 复用 [`clean_old_logs`] 以 **retention = 0** 调用：前缀匹配
/// （`xcontrol.log.*`，命名契约由 `rolling_appender_uses_documented_file_name`
/// 测试钉死）的滚动文件全部删除，**含当日**（mtime 在过去的文件 age > 0 恒成立；
/// 时钟回拨产生的未来时间戳保守保留）。
///
/// 含当日的裁定（11-21）：保留当日文件会让"清空后磁盘日志仍在"（实测场景——
/// 旧版本会话把当日文件写满观测噪音，用户点清空的诉求就是它们消失）。代价：
/// 滚动 appender 的写句柄指向被删文件，删除后到次日轮转前的新日志只落
/// stdout/UI 缓冲（不落盘）——确认框文案如实告知。
pub fn clear_log_files(dir: &Path) -> std::io::Result<usize> {
    clean_old_logs(dir, SystemTime::now(), Duration::ZERO)
}

/// EnvFilter 热重建句柄（11-18-m4-ui：设置页日志级别立即生效，docs/06 §4.1）。
///
/// `init` 把 EnvFilter 包进 `reload::Layer` 后将句柄存入进程级单例；
/// [`set_level`] 经句柄原子替换过滤层（registry 顶层并发读安全——reload 内部
/// 有锁）。`init` 未跑（纯单测环境）时句柄为 `None`，[`set_level`] 告警降级。
static RELOAD_HANDLE: OnceLock<reload::Handle<EnvFilter, tracing_subscriber::Registry>> =
    OnceLock::new();

/// 运行时改变全局日志级别（docs/06 §4.1 生效时点表：`general.log_level` 立即）。
///
/// 指令串按 [`directive_string`] 重建（XCONTROL_LOG 追加指令保留——改级别不改
/// 子系统微调口径）。`init` 未装配（单测/异常路径）时仅返回 `false` 并告警，
/// 不 panic。返回是否已应用（调用方据此记"立即生效"或降级日志）。
pub fn set_level(level: LogLevel) -> bool {
    let directives = directive_string(level, std::env::var("XCONTROL_LOG").ok().as_deref());
    let Some(handle) = RELOAD_HANDLE.get() else {
        tracing::warn!(target: "app::log", level = level.as_str(),
            "日志级别热改不可用（subscriber 未装配 reload 层），级别维持启动值");
        return false;
    };
    let new_filter = match EnvFilter::try_new(&directives) {
        Ok(filter) => filter,
        Err(error) => {
            tracing::warn!(target: "app::log", %error, level = level.as_str(),
                "日志级别指令解析失败（维持原级别）");
            return false;
        }
    };
    let applied = handle.modify(|filter| *filter = new_filter).is_ok();
    if applied {
        tracing::info!(target: "app::log", level = level.as_str(),
            "日志级别已热改（EnvFilter 重建，XCONTROL_LOG 追加指令保留）");
    } else {
        tracing::warn!(target: "app::log", level = level.as_str(),
            "日志级别热改失败（reload 句柄失效——subscriber 已回收？）");
    }
    applied
}

/// 装配全局 subscriber：stdout fmt 层 + UI 环形缓冲层（进程内一次）；
/// `settings.file_dir` 有值时叠加滚动文件层（daily rotation）+ 启动清理线程。
/// EnvFilter 经 `reload::Layer` 包裹（句柄入 [`RELOAD_HANDLE`] 单例，供
/// [`set_level`] 热改）。
///
/// # Errors
/// 全局 subscriber 已被设置（重复初始化）或日志目录创建失败时返回错误——
/// main 直接上抛（bin 顶层 anyhow，error-handling.md 规则 2）。
pub fn init(buffer: LogBuffer, settings: &LogSettings) -> Result<LoggingGuard, anyhow::Error> {
    let directives = directive_string(
        settings.level,
        std::env::var("XCONTROL_LOG").ok().as_deref(),
    );
    // 先严格解析：全部指令合法 → 直接用；有非法指令（多来自 XCONTROL_LOG 手写
    // 指令）→ 暂存告警，subscriber 装配完成后经 tracing::warn! 落 stdout/文件/UI
    // 三处（见 init 尾部）——"丢弃无效并告警"在此落地，非法输入不致命（级别
    // 基线指令恒合法，过滤面永不因 env 手误而失效）。
    let mut filter_warning: Option<String> = None;
    let env_filter = match EnvFilter::try_new(&directives) {
        Ok(filter) => filter,
        Err(error) => {
            filter_warning = Some(format!(
                "XCONTROL_LOG 含非法过滤指令，已忽略无效部分（{error}）：{directives}"
            ));
            EnvFilter::builder().parse_lossy(&directives)
        }
    };
    // reload 包装：级别热改的运行时通道（句柄跨线程 Send，注册进进程级单例）。
    let (reload_filter, reload_handle) = reload::Layer::new(env_filter);
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .event_format(RedactedFormat);
    let ui_layer = UiLogLayer { buffer };

    let guard = match settings.file_dir.as_deref().map(logs_dir_of) {
        Some(logs_dir) => {
            std::fs::create_dir_all(&logs_dir).map_err(|error| {
                anyhow::anyhow!("日志目录创建失败（{}）：{error}", logs_dir.display())
            })?;
            let appender = tracing_appender::rolling::daily(&logs_dir, LOG_FILE_PREFIX);
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(writer)
                .with_ansi(false)
                .event_format(RedactedFormat);
            tracing_subscriber::registry()
                .with(reload_filter)
                .with(stdout_layer)
                .with(file_layer)
                .with(ui_layer)
                .try_init()
                .map_err(|error| anyhow::anyhow!("tracing subscriber 初始化失败：{error}"))?;
            spawn_daily_cleaner(&logs_dir);
            LoggingGuard {
                _file_worker: Some(guard),
            }
        }
        None => {
            tracing_subscriber::registry()
                .with(reload_filter)
                .with(stdout_layer)
                .with(ui_layer)
                .try_init()
                .map_err(|error| anyhow::anyhow!("tracing subscriber 初始化失败：{error}"))?;
            LoggingGuard { _file_worker: None }
        }
    };
    let _ = RELOAD_HANDLE.set(reload_handle);
    // 订阅者已就绪：非法指令告警走正常管道落 stdout/文件/UI 三处（受当前级别
    // 过滤——显式 error 级别下不可见，属级别口径的既定语义）。
    if let Some(warning) = filter_warning {
        tracing::warn!(target: "app::log", "{warning}");
    }
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// 测试捕获层：把事件行（LEVEL target）推入共享 sink（级别过滤行为断言用）。
    struct CaptureLayer {
        sink: Arc<Mutex<Vec<String>>>,
    }

    impl<S: Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            self.sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!(
                    "{} {}",
                    event.metadata().level(),
                    event.metadata().target()
                ));
        }
    }

    /// 独立临时目录；Drop 时清理（滚动文件/清理测试的沙箱）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "xcontrol-app-log-{tag}-{}-{}",
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

    /// 环形缓冲：push 按序累积、超容量淘汰最旧、snapshot 返回全量。
    #[test]
    fn ring_buffer_capacity_and_order() {
        let buffer = LogBuffer::new();
        assert!(buffer.is_empty());

        for i in 0..(RING_CAPACITY + 20) {
            buffer.push(format!("line-{i}"));
        }
        assert_eq!(buffer.len(), RING_CAPACITY);
        let lines = buffer.snapshot();
        assert_eq!(lines.len(), RING_CAPACITY);
        // 最旧 20 行被淘汰：首行是 line-20，末行是 line-(CAP+19)。
        assert_eq!(lines[0], format!("line-{}", 20));
        assert_eq!(
            lines[RING_CAPACITY - 1],
            format!("line-{}", RING_CAPACITY + 19)
        );
    }

    /// 墙钟格式：HH:MM:SS 三段两位数。
    #[test]
    fn timestamp_shape() {
        let ts = timestamp_hms();
        assert_eq!(ts.len(), 8);
        assert_eq!(ts.as_bytes()[2], b':');
        assert_eq!(ts.as_bytes()[5], b':');
    }

    /// UI Layer 事件捕获：字段拼装进环形缓冲（info! → snapshot 可见）。
    #[test]
    fn ui_layer_captures_events() {
        let buffer = LogBuffer::new();
        let layer = UiLogLayer {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "app::test", count = 3, "回环演示事件");
        });
        let lines = buffer.snapshot();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("app::test"),
            "target 进日志：{}",
            lines[0]
        );
        assert!(lines[0].contains("回环演示事件"), "message 进日志");
        assert!(lines[0].contains("count=3"), "字段进日志：{}", lines[0]);
    }

    /// REDACTED 命中面：命中/不命中/大小写/分隔符/驼峰逐项断言。
    /// 已知代价：`error_code` 等含 "code" 段的良性字段也脱敏（过度脱敏可接受，
    /// 见 [`SENSITIVE_FIELD_WORDS`] 文档）。
    #[test]
    fn sensitive_field_name_matrix() {
        let hits = [
            "code",
            "Code",
            "SECRET",
            "security_code",
            "security-code",
            "security.code",
            "SecurityCode",
            "securityCode",
            "authToken",
            "access_token",
            "passphrase",
            "password_hash",
            "secretRef",
            "error_code",
            // psk/key 增补词（XXpsk0 协议——密钥材料字段名）：
            "psk",
            "psk_salt",
            "session_key",
            "derivedPsk",
        ];
        for name in hits {
            assert!(is_sensitive_field_name(name), "{name} 应命中黑名单");
        }
        let misses = [
            "decoder",
            "encode_result",
            "user_name",
            "count",
            "elapsed_ms",
            "peer",
            "tokener",
            "recode",
            // 含 key 字母但不是独立分段（整段词不误伤）：
            "keymap",
            "hotkeys",
            "monkey",
        ];
        for name in misses {
            assert!(!is_sensitive_field_name(name), "{name} 不应命中黑名单");
        }
    }

    /// REDACTED Visit 层（端到端）：敏感字段值全替换，良性字段原样保留；
    /// str / 数值 / 错误三类 record 路径全覆盖。
    #[test]
    fn sensitive_fields_are_redacted_in_ui_buffer() {
        let buffer = LogBuffer::new();
        let layer = UiLogLayer {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "app::test",
                security_code = "hunter2-demo-code",
                PASSWORD = "hunter2-password",
                auth_token = "hunter2-token",
                passphrase = "hunter2-passphrase",
                secret = 42_u64,
                username = "alice",
                count = 3,
                "脱敏演示事件");
        });
        let lines = buffer.snapshot();
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.contains("脱敏演示事件"), "{line}");
        assert!(line.contains("username=alice"), "良性字段保留：{line}");
        assert!(line.contains("count=3"), "良性字段保留：{line}");
        assert_eq!(
            line.matches("[REDACTED]").count(),
            5,
            "五个敏感字段全部替换：{line}"
        );
        for leaked in [
            "hunter2-demo-code",
            "hunter2-password",
            "hunter2-token",
            "hunter2-passphrase",
        ] {
            assert!(!line.contains(leaked), "泄露 {leaked}：{line}");
        }
        assert!(
            line.contains("secret=[REDACTED]"),
            "数值字段同样替换：{line}"
        );
    }

    /// REDACTED 覆盖 record_error 路径（字段值为 `&dyn Error` 时 tracing 走
    /// `Visit::record_error`——tracing-core 对 `dyn Error` 的 `Value` 实现）。
    #[test]
    fn error_fields_are_redacted_when_sensitive() {
        let buffer = LogBuffer::new();
        let layer = UiLogLayer {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let token_error: &(dyn std::error::Error + 'static) = &std::io::Error::other("boom-detail");
        let plain_error: &(dyn std::error::Error + 'static) =
            &std::io::Error::other("visible-detail");
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "app::test", token_error, plain_error, "错误演示");
        });
        let line = &buffer.snapshot()[0];
        assert!(line.contains("token_error=[REDACTED]"), "{line}");
        assert!(!line.contains("boom-detail"), "{line}");
        assert!(line.contains("plain_error=visible-detail"), "{line}");
    }

    /// EnvFilter 级别指令：全局级别生效（Info 下 debug 不放行、info/warn 放行）。
    #[test]
    fn env_filter_applies_global_level() {
        fn captured_lines(level: LogLevel) -> Vec<String> {
            let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let capture = CaptureLayer {
                sink: Arc::clone(&lines),
            };
            let filter = EnvFilter::builder().parse_lossy(directive_string(level, None));
            let subscriber = tracing_subscriber::registry().with(filter).with(capture);
            tracing::subscriber::with_default(subscriber, || {
                tracing::debug!(target: "app::test", "debug 事件");
                tracing::info!(target: "app::test", "info 事件");
                tracing::warn!(target: "app::test", "warn 事件");
            });
            let guard = lines.lock().unwrap_or_else(PoisonError::into_inner);
            guard.clone()
        }

        let info_lines = captured_lines(LogLevel::Info);
        assert_eq!(
            info_lines.len(),
            2,
            "info/warn 放行、debug 拦截：{info_lines:?}"
        );
        assert!(info_lines.iter().all(|line| !line.contains("DEBUG")));

        let debug_lines = captured_lines(LogLevel::Debug);
        assert_eq!(debug_lines.len(), 3, "debug 全放行：{debug_lines:?}");
    }

    /// 指令串组装：级别在前、追加指令可叠加、空/空白追加忽略。
    #[test]
    fn directive_string_combines_level_and_extra() {
        assert_eq!(directive_string(LogLevel::Info, None), "info");
        assert_eq!(
            directive_string(LogLevel::Warn, Some("net::=debug")),
            "warn,net::=debug"
        );
        assert_eq!(
            directive_string(LogLevel::Debug, Some("  net::=debug,app::edge=trace  ")),
            "debug,net::=debug,app::edge=trace"
        );
        assert_eq!(
            directive_string(LogLevel::Trace, Some("   ")),
            "trace",
            "空白追加不产出悬空逗号"
        );
    }

    /// EnvFilter 子系统指令前缀匹配（PRD 示例 `net::=debug`）：`net::` 命中
    /// `net::serve`/`net::connect`，其他子系统仍按基线级别过滤。
    #[test]
    fn subsystem_directive_matches_target_prefix() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let capture = CaptureLayer {
            sink: Arc::clone(&lines),
        };
        let filter =
            EnvFilter::builder().parse_lossy(directive_string(LogLevel::Info, Some("net::=debug")));
        let subscriber = tracing_subscriber::registry().with(filter).with(capture);
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "net::serve", "net 调试");
            tracing::debug!(target: "net::connect", "net 调试二");
            tracing::debug!(target: "app::edge", "app 调试（应被 info 基线拦截）");
            tracing::info!(target: "session::rt", "会话信息（基线放行）");
        });
        let guard = lines.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            guard.len(),
            3,
            "net:: 两条 debug + session:: 一条 info 放行：{:?}",
            *guard
        );
        assert!(
            guard.iter().all(|line| !line.contains("app::edge")),
            "app::edge 的 debug 被基线拦截：{:?}",
            *guard
        );
    }

    /// 滚动文件命名契约：daily appender 落盘文件形如 `xcontrol.log.<YYYY-MM-DD>`
    /// （清理与 `--status` 日志尾部都按此前缀检索——命名漂移会同时打断两者）。
    #[test]
    fn rolling_appender_uses_documented_file_name() {
        let dir = TempDir::new("rolling-name");
        let mut appender = tracing_appender::rolling::daily(dir.path(), LOG_FILE_PREFIX);
        appender
            .write_all(b"naming-contract-line\n")
            .expect("滚动文件写入");
        drop(appender);

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .expect("目录可读")
            .map(|entry| {
                entry
                    .expect("目录项")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        let name = &entries[0];
        assert!(
            name.starts_with(&format!("{LOG_FILE_PREFIX}.")),
            "文件名前缀约定：{name}"
        );
        assert_eq!(
            name.len(),
            LOG_FILE_PREFIX.len() + 1 + "2026-11-18".len(),
            "后缀为 10 位日期：{name}"
        );
        let content = std::fs::read_to_string(dir.path().join(name)).expect("内容可读");
        assert_eq!(content, "naming-contract-line\n");
    }

    /// 文件层端到端：RedactedFormat + NonBlocking 落盘，内容与 UI/stdout 同构
    /// 且敏感字段已替换（guard drop = worker flush 后断言）。
    #[test]
    fn file_layer_writes_redacted_compact_lines() {
        let dir = TempDir::new("file-layer");
        let appender = tracing_appender::rolling::daily(dir.path(), LOG_FILE_PREFIX);
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_writer(writer)
            .with_ansi(false)
            .event_format(RedactedFormat);
        let filter = EnvFilter::builder().parse_lossy("info");
        let subscriber = tracing_subscriber::registry().with(filter).with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "app::test", security_code = "file-hunter2", "写盘事件");
        });
        drop(guard); // worker 收尾 + flush

        let mut entries = std::fs::read_dir(dir.path())
            .expect("目录可读")
            .map(|entry| entry.expect("目录项").path())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1, "仅一个滚动文件");
        let content = std::fs::read_to_string(entries.pop().expect("非空")).expect("内容可读");
        assert!(content.contains("写盘事件"), "{content}");
        assert!(content.contains("[app::test]"), "target 进行：{content}");
        assert!(!content.contains("file-hunter2"), "脱敏：{content}");
        assert!(
            content.contains("security_code=[REDACTED]"),
            "敏感字段替换：{content}"
        );
    }

    /// 清理策略：删超期滚动文件、保留新鲜/边界内文件与无关文件/目录，
    /// now 由参数注入（模拟跨日），mtime 用 `File::set_modified` 回拨。
    #[test]
    fn clean_old_logs_respects_retention_prefix_and_type() {
        let dir = TempDir::new("clean");
        let now = SystemTime::now();
        let retention = Duration::from_secs(LOG_RETENTION_DAYS * 24 * 60 * 60);
        let day = Duration::from_secs(24 * 60 * 60);

        let set_mtime = |path: &Path, age: Duration| {
            let mtime = now.checked_sub(age).expect("合法过去时间戳");
            let file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("重开文件");
            file.set_modified(mtime).expect("mtime 可写");
        };
        let make_file = |name: &str, age: Option<Duration>| -> PathBuf {
            let path = dir.path().join(name);
            std::fs::write(&path, name).expect("占位写入");
            if let Some(age) = age {
                set_mtime(&path, age);
            }
            path
        };

        // 40 天前（远超 14 天）→ 删；保留期内差 1s → 留；1s 前 → 留；
        // 前缀不符 → 不动；同名前缀目录 → 不动。
        let stale = make_file("xcontrol.log.2026-01-01", Some(day * 40));
        let boundary_kept = make_file(
            "xcontrol.log.2026-11-05",
            Some(retention - Duration::from_secs(1)),
        );
        let fresh = make_file("xcontrol.log.today", Some(Duration::from_secs(1)));
        let unrelated = make_file("other.log.2026-01-01", Some(day * 40));
        let subdir = dir.path().join("xcontrol.log.subdir");
        std::fs::create_dir_all(&subdir).expect("子目录");

        let removed = clean_old_logs(dir.path(), now, retention).expect("清理成功");
        assert_eq!(removed, 1, "仅超期滚动文件被删");
        assert!(!stale.exists(), "超期滚动文件应被删除");
        assert!(boundary_kept.exists(), "恰在保留期内（差 1s）必须保留");
        assert!(fresh.exists());
        assert!(unrelated.exists(), "前缀不符的文件一律不动");
        assert!(subdir.exists(), "目录一律不动");

        // 边界外 1s 即删除（age > retention 语义）。
        set_mtime(&boundary_kept, retention + Duration::from_secs(1));
        let removed = clean_old_logs(dir.path(), now, retention).expect("清理成功");
        assert_eq!(removed, 1, "超期 1s 即删");
        assert!(!boundary_kept.exists());

        // 幂等：再跑一次无删除。
        let removed = clean_old_logs(dir.path(), now, retention).expect("清理成功");
        assert_eq!(removed, 0);
        assert!(fresh.exists());
    }

    /// 日志级别热改（11-18-m4-ui）：subscriber 未装配（纯单测进程，init 未跑）
    /// 时 `set_level` 告警降级返回 false、不 panic——生效路径由运行时实测覆盖
    /// （docs/06 §4.1 生效时点表"日志级别=立即"）。
    #[test]
    fn set_level_degrades_gracefully_without_subscriber() {
        // RELOAD_HANDLE 在测试进程内从未被 init 填充（init 需要全局 subscriber
        // 唯一性，单测不触发）；无论何种进程时序，返回 false 都是安全语义。
        assert!(!set_level(LogLevel::Debug));
    }

    /// 清空日志文件（retention = 0）：删**全部**前缀匹配滚动文件（含当日——
    /// 实测场景：旧版本观测噪音写满当日文件，清空诉求就是它们消失）、保留
    /// 无关文件与目录；目录不可读报错（关于页「清空日志文件」按钮的执行体）。
    #[test]
    fn clear_log_files_removes_all_rotating_files() {
        let dir = TempDir::new("clear-logs");
        let make = |name: &str| {
            std::fs::write(dir.path().join(name), name).expect("占位写入");
        };
        make(&format!("{LOG_FILE_PREFIX}.2020-01-01"));
        make(&format!("{LOG_FILE_PREFIX}.2026-11-21"));
        make(&format!("{LOG_FILE_PREFIX}.not-a-date"));
        make("other.log.2020-01-01");
        let subdir = dir.path().join(format!("{LOG_FILE_PREFIX}.2019-01-01"));
        std::fs::create_dir_all(&subdir).expect("子目录");

        let removed = clear_log_files(dir.path()).expect("清空成功");
        assert_eq!(removed, 3, "全部滚动文件（含当日/非日期后缀）被删");
        assert!(
            !dir.path()
                .join(format!("{LOG_FILE_PREFIX}.2020-01-01"))
                .exists()
        );
        assert!(
            !dir.path()
                .join(format!("{LOG_FILE_PREFIX}.2026-11-21"))
                .exists(),
            "当日文件同样删除（清空语义，代价见 clear_log_files 文档）"
        );
        assert!(
            dir.path().join("other.log.2020-01-01").exists(),
            "无关文件不动"
        );
        assert!(subdir.exists(), "目录一律不动");

        // 幂等：再跑一次无删除。
        let removed = clear_log_files(dir.path()).expect("清空成功");
        assert_eq!(removed, 0);
    }

    /// 清空日志文件：目录不存在 → Err（调用方就地提示）。
    #[test]
    fn clear_log_files_errors_on_missing_dir() {
        let dir = TempDir::new("clear-logs-missing");
        let missing = dir.path().join("no-such-subdir");
        assert!(clear_log_files(&missing).is_err());
    }

    /// 环形缓冲清空（关于页清空按钮同步清页面显示）。
    #[test]
    fn ring_buffer_clear_empties_snapshot() {
        let buffer = LogBuffer::new();
        buffer.push("line-1".to_owned());
        buffer.push("line-2".to_owned());
        assert_eq!(buffer.len(), 2);
        buffer.clear();
        assert!(buffer.is_empty());
        assert!(buffer.snapshot().is_empty());
    }
}
