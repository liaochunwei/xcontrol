//! `AppState` 快照流 + `CoreHandle` 命令入口（docs/06 §6 状态与数据流）。
//!
//! 数据流（单向，架构红线 architecture-rules.md 红线 4）：
//!
//! ```text
//! 核心（tokio / 回环线程）──report()/flush()──► StateBus（watch，200ms 节流）──► UI 纯投影
//! UI ──CoreHandle::send()（mpsc，尽力投递）──► 核心
//! ```
//!
//! - UI 只读 [`AppState`] 快照、发 [`CoreCommand`]；不持有连接/会话状态，
//!   不直连 net/core 细节（ui-tray.md gpui 红线 3）；
//! - `tokio::sync::watch`/`mpsc` 原语不依赖 tokio 运行时（纯原子 + waker），
//!   gpui 主线程与回环 std 线程均可安全使用——两种执行域经此模块桥接。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use xcontrol_config::{Config, Role};
use xcontrol_proto::input::ScreenId;

/// 快照推送节流周期（docs/06 §6：watch channel 200ms 合并更新节流）。
pub const SNAPSHOT_PERIOD: Duration = Duration::from_millis(200);

/// 字节序列小写 hex（会话标识等快照字段的人类可读形态；无第三方 hex 依赖）。
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // String 的 write! 只在容量预估失误时分配失败（OOM），
        // 无外部输入路径，不触 unwrap 禁令。
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 命令通道容量：M2 命令是个位数的低频事件（暂停/退出），16 提供足够余量；
/// 满载时 [`CoreHandle::send`] 尽力投递失败仅记日志（UI 不阻塞主线程）。
/// main.rs 建通道时使用同一常量（避免双处魔数漂移）。
pub const COMMAND_CHANNEL_BUFFER: usize = 16;

/// 共享运行时配置单元（11-18-m4-ui 生效框架，docs/06 §4.1）。
///
/// 运行时 supervisor 持有并派发给各循环（`RuntimeDeps.config`）；`RuntimeConfigUpdate`
/// 的消费点（supervisor 等待相 / 空闲 accept 分支 / 退避等待 / 会话内命令分支）
/// 写入，各 `SessionConfigs` 构造点与重建点读取最新值。std `Mutex`（临界区为
/// 整体 clone，无 await 持锁）——tokio 任务与 gpui 主线程两侧均可安全使用。
pub type SharedConfig = Arc<Mutex<Config>>;

/// 共享配置单元写入（临界区为整体 clone，无 await 持锁；毒锁恢复同快照纪律）。
pub fn store_shared_config(cell: &SharedConfig, config: &Config) {
    *cell.lock().unwrap_or_else(PoisonError::into_inner) = config.clone();
}

/// UI → 核心命令（docs/06 §6：一切变更走命令；M2 最小集 + M4 托盘动作 + M4 设置生效框架）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreCommand {
    /// 本地共享暂停（FR-2.8）。
    Pause,
    /// 本地共享恢复（FR-2.8）。
    Resume,
    /// 主动断开当前连接（保持进程与窗口）。
    Disconnect,
    /// 退出应用（断开 + 进程退出）。
    QuitApp,
    /// 切换角色（FR-1.1；托盘「切换角色」）：停当前会话 → 角色翻转 → 运行时
    /// 重建（会话循环收到后优雅断开并上抛 [`crate::session_rt::SessionEnd::RoleSwitch`]，
    /// main.rs 的运行时 supervisor 据此重建——进程内角色翻转，docs/06 §5.2）。
    SwitchRole,
    /// 切换到指定在线从机（FR-2.5「直接切换」；托盘子菜单路径，热键路径复用
    /// 同一 `LocalCmd::Switch` 信令）。`target` = 在线从机序下标（M4 单会话单
    /// 从机拓扑恒 0），会话循环经 `layout_rt::resolve_slave_target` 换算到
    /// 布局装配的全局 ScreenId（docs/02 §4.2，11-19-m5-layout）。
    SwitchToSlave {
        /// 目标屏下标（在线从机序；proto `ScreenId` 语义）。
        target: ScreenId,
    },
    /// 运行时配置更新（11-18-m4-ui 生效框架，docs/06 §4.1 生效时点表）：设置页/
    /// 向导改动经 `xcontrol_config::save` 落盘后整体携带新 [`Config`] 下发。
    /// 消费点（supervisor 等待相 / 空闲 accept 分支 / 退避等待 / 会话内命令分支）
    /// 按 docs/06 §4.1 分级生效：日志级别立即（`logging::set_level`）；keymap/
    /// hotkeys/clipboard 写入共享配置单元（下会话构造点读最新）；net 变化在空闲
    /// 重建时应用。`Copy` 因此取消（Config 非拷贝类型）；`Box` 控制枚举尺寸
    ///（其余变体均为小整数/标记——大变体入堆避免通道缓存放大）。
    RuntimeConfigUpdate(Box<Config>),
    /// 确认候选主控（FR-5.5）：`instance_id` = 主控 instance id 的 32 位 hex。
    /// 发现循环立即以其为直连目标（进程内记忆）；持久化（config
    /// `net.confirmed_host`）由 UI 提交路径（`commit_config`）完成——托盘路径
    /// 只立即生效不落盘（下次启动回退自动语义，docs/06 §4.1 net 行）。
    ConfirmHost {
        /// 被确认主控的 instance id（32 位小写 hex）。
        instance_id: String,
    },
}

/// 连接状态（[`AppState::conn`]；与 core `SessionState` 对齐，UI 不直接依赖 core 类型）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnState {
    /// 未开始（等待 serve/connect 或回环驱动启动）。
    #[default]
    Idle,
    /// 链路建立中（TCP 监听/拨号、回环配对）。
    Connecting,
    /// Noise 握手完成、版本协商进行中。
    Handshaking,
    /// 会话运行中（`paused` = 共享暂停，FR-2.8）。
    Running {
        /// 是否共享暂停。
        paused: bool,
    },
    /// 已断开（`reason` 人类可读；禁止包含安全码/密钥派生中间值，
    /// error-handling.md 规则 6）。
    Disconnected {
        /// 断开原因描述。
        reason: String,
    },
}

impl ConnState {
    /// 状态页/冒烟输出的单行标签。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "Idle".into(),
            Self::Connecting => "Connecting".into(),
            Self::Handshaking => "Handshaking".into(),
            Self::Running { paused } => {
                if *paused {
                    "Running(Paused)".into()
                } else {
                    "Running".into()
                }
            }
            Self::Disconnected { reason } => format!("Disconnected({reason})"),
        }
    }
}

/// 一台候选主控的投影（FR-5.5 多主控候选列表；状态页/托盘共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCandidate {
    /// 主控显示名（beacon/mDNS TXT 载荷；TCP 兜底候选为空串）。
    pub name: String,
    /// 主控服务地址（`ip:port`）。
    pub addr: String,
    /// 主控 instance id（32 位小写 hex；TCP 兜底候选全零）。
    pub instance_id_hex: String,
    /// 发现来源（`mdns` / `beacon` / `probe` / `tcp`）。
    pub source: &'static str,
    /// 上次握手是否失败（M3 错码软排除的用户可见标记——docs/02 §2.2
    /// "安全码不匹配 = 握手失败自动排除"，此处只标注不阻断重试）。
    pub last_handshake_failed: bool,
    /// 是否已被记忆（config `net.confirmed_host` 命中）。
    pub confirmed: bool,
}

/// UI 快照（docs/06 §6 单一事实源的投影载荷；UI 唯一可读物）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppState {
    /// 本机角色（FR-1.1；来自配置/CLI）。
    pub role: Role,
    /// 连接状态。
    pub conn: ConnState,
    /// 对端显示名（版本协商成功后为 `Some`）。
    pub peer_name: Option<String>,
    /// 会话标识 hex（docs/02 §3.3；`BLAKE3(hs_hash)[0..16]`）。
    pub session_id_hex: Option<String>,
    /// 已注入本机的输入事件数（RecordingSink 统计；M2 演示计数，M3 接平台 sink）。
    pub events_in: u64,
    /// 出站输入事件数（捕获/脚本产出）。
    pub events_out: u64,
    /// 已收到的密文报文数（传输层解密次数）。
    pub packets_in: u64,
    /// 已发送的密文报文数（传输层加密次数）。
    pub packets_out: u64,
    /// 最近一次错误（人类可读；无安全语义）。
    pub last_error: Option<String>,
    /// 本机屏幕名列表（会话建立时自动检测；布局页只读列表数据源，FR-1.3）。
    /// 空表 = 会话未建立（本机屏快照随会话装配产出，docs/06 §4 布局页说明）。
    pub local_screens: Vec<String>,
    /// 候选主控列表（FR-5.5）：发现循环产出的最新候选投影；多主控待确认、
    /// 错码软排除标注都在此呈现。空 = 无候选/非发现模式/已连接。
    pub host_candidates: Vec<HostCandidate>,
}

/// 锁中毒恢复：快照是纯内存状态，中毒后保留内容继续投影优于整体失效
/// （`unwrap_or_else` 非 unwrap，不触 CI 禁令；与 net serve.rs 同形）。
fn lock_snapshot(snapshot: &Mutex<AppState>) -> MutexGuard<'_, AppState> {
    snapshot.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock_instant(cell: &Mutex<Instant>) -> MutexGuard<'_, Instant> {
    cell.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 状态总线：核心侧写、UI 侧读（docs/06 §6 单一事实源）。
///
/// - [`StateBus::report`]：修改快照；距上次推送 ≥ [`SNAPSHOT_PERIOD`] 才推送
///   （突发更新 200ms 合并一次，避免高频重绘）；
/// - [`StateBus::flush`]：状态跃迁（连接/断开/握手完成）立即推送——频率受
///   跃迁次数天然限制，不构成高频源，且 FR-6.1 要求状态变化 ≤1s 可见。
pub struct StateBus {
    snapshot: Mutex<AppState>,
    tx: watch::Sender<AppState>,
    last_push: Mutex<Instant>,
}

impl StateBus {
    /// 以 `initial` 快照构造（UI 侧经 [`StateBus::subscribe`] 订阅）。
    #[must_use]
    pub fn new(initial: AppState) -> Arc<Self> {
        let (tx, _) = watch::channel(initial.clone());
        // 初始 last_push 取过去时刻：首个 report 立即可推送。
        Arc::new(Self {
            snapshot: Mutex::new(initial),
            tx,
            last_push: Mutex::new(Instant::now() - SNAPSHOT_PERIOD),
        })
    }

    /// 订阅快照流（UI / 冒烟观测入口）。
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<AppState> {
        self.tx.subscribe()
    }

    /// 当前快照（最新值；不等待推送节流）。
    #[must_use]
    pub fn current(&self) -> AppState {
        lock_snapshot(&self.snapshot).clone()
    }

    /// 修改快照（200ms 合并节流；急迫状态请随后调用 [`StateBus::flush`]）。
    pub fn report(&self, mutate: impl FnOnce(&mut AppState)) {
        {
            let mut snapshot = lock_snapshot(&self.snapshot);
            mutate(&mut snapshot);
        }
        self.push_if_due();
    }

    /// 立即推送当前快照（状态跃迁用）。
    pub fn flush(&self) {
        let snapshot = self.current();
        let _ = self.tx.send(snapshot);
        *lock_instant(&self.last_push) = Instant::now();
    }

    /// 距上次推送 ≥ [`SNAPSHOT_PERIOD`] 时推送一次。
    fn push_if_due(&self) {
        let due = {
            let last = lock_instant(&self.last_push);
            last.elapsed() >= SNAPSHOT_PERIOD
        };
        if due {
            self.flush();
        }
    }
}

/// UI 命令入口（可克隆；UI 只能拿到它——编译期即约束"变更全走命令"）。
#[derive(Clone)]
pub struct CoreHandle {
    tx: mpsc::Sender<CoreCommand>,
}

impl CoreHandle {
    /// 绑定命令通道发送端（接收端归核心执行域）。
    #[must_use]
    pub fn new(tx: mpsc::Sender<CoreCommand>) -> Self {
        Self { tx }
    }

    /// 尽力投递一条命令（非阻塞；通道满/关闭 = 核心繁忙或已退出，丢弃并记日志——
    /// UI 主线程不得阻塞，M2 命令均为低频可重试语义）。
    pub fn send(&self, command: CoreCommand) {
        if let Err(error) = self.tx.try_send(command.clone()) {
            tracing::warn!(target: "app::state", ?command, %error, "核心命令投递失败（丢弃）");
        }
    }

    /// 非阻塞取走一条命令（核心循环用；无命令返回 `None`）。
    pub(crate) fn try_recv(rx: &mut mpsc::Receiver<CoreCommand>) -> Option<CoreCommand> {
        rx.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 200ms 节流：窗口内的多次 report 只推送一次（borrow 看到的仍是上次推送值）；
    /// flush 无视窗口立即推送。
    #[test]
    fn throttle_merges_and_flush_pushes() {
        let bus = StateBus::new(AppState::default());
        let mut rx = bus.subscribe();

        // 初始 last_push 已过期 → 首个 report 立即推送。
        bus.report(|s| s.events_out = 1);
        assert_eq!(rx.borrow().events_out, 1);

        // 节流窗口内：改动可见于 current()，但 watch 不再推送（合并语义）。
        for n in 2..=5_u64 {
            bus.report(|s| s.events_out = n);
        }
        assert_eq!(bus.current().events_out, 5, "current 读内部快照");
        assert_eq!(
            rx.borrow().events_out,
            1,
            "窗口内的 report 被节流，watch 保持上次推送值"
        );

        // flush：无视窗口立即推送。
        bus.flush();
        assert_eq!(rx.borrow_and_update().events_out, 5);
    }

    /// 命令通道：try_send 尽力投递；容量满时丢弃不阻塞。
    #[test]
    fn core_handle_best_effort_send() {
        let (tx, mut rx) = mpsc::channel(COMMAND_CHANNEL_BUFFER);
        let handle = CoreHandle::new(tx);

        handle.send(CoreCommand::Pause);
        handle.send(CoreCommand::Resume);
        assert_eq!(CoreHandle::try_recv(&mut rx), Some(CoreCommand::Pause));
        assert_eq!(CoreHandle::try_recv(&mut rx), Some(CoreCommand::Resume));
        // 运行时配置更新（11-18-m4-ui）：Config 整体携带、原样取回（Clone 语义；
        // Box<Config> ——枚举尺寸控制）。
        let mut config = xcontrol_config::Config::default();
        config.general.log_level = xcontrol_config::LogLevel::Debug;
        handle.send(CoreCommand::RuntimeConfigUpdate(Box::new(config.clone())));
        assert_eq!(
            CoreHandle::try_recv(&mut rx),
            Some(CoreCommand::RuntimeConfigUpdate(Box::new(config)))
        );
        assert_eq!(CoreHandle::try_recv(&mut rx), None, "空通道返回 None");

        // 打满通道后继续 send 不阻塞、不 panic（仅记 warn 日志）。
        for _ in 0..COMMAND_CHANNEL_BUFFER {
            handle.send(CoreCommand::QuitApp);
        }
        handle.send(CoreCommand::QuitApp);
    }

    /// hex 编码：小写、定长、空输入为空串。
    #[test]
    fn hex_encodes_lowercase() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0xab, 0x0F]), "00ab0f");
        assert_eq!(hex(&[0xff; 16]).len(), 32);
    }

    /// ConnState::label：状态页/冒烟输出的可读标签。
    #[test]
    fn conn_state_labels() {
        assert_eq!(ConnState::Idle.label(), "Idle");
        assert_eq!(ConnState::Connecting.label(), "Connecting");
        assert_eq!(ConnState::Handshaking.label(), "Handshaking");
        assert_eq!(ConnState::Running { paused: false }.label(), "Running");
        assert_eq!(
            ConnState::Running { paused: true }.label(),
            "Running(Paused)"
        );
        assert_eq!(
            ConnState::Disconnected {
                reason: "测试".into()
            }
            .label(),
            "Disconnected(测试)"
        );
    }
}
