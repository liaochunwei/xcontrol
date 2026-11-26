//! `--loopback` — 进程内 M1 全链路演示（FR-1.5 自回环调试模式）。
//!
//! 拓扑（与 `xcontrol-net` tests/scenarios.rs 同构，双角色同进程）：
//!
//! ```text
//! ScriptedInputSource（主控捕获）→ proto 编码 → Noise 加密 → LoopbackTransport
//!   → 解密 → 解码 → core Session/Dispatcher → RecordingInputSink（从机注入）
//! ```
//!
//! - 零网络、零平台依赖：host/client 两个 [`Session`](xcontrol_core::session::Session)
//!   都由本模块在一条 std 线程（`loopback-core`）上驱动，经 [`StateBus`] 上报快照、
//!   经 `mpsc::Receiver` 消费 UI 命令——与 TCP 模式（`session_rt`）共用 state.rs 的
//!   单向数据流契约，UI 完全不感知后端差异（design.md 关键决策）；
//! - 命令语义：`Pause`/`Resume` → 主控侧 `LocalCmd`（`CtrlMsg::Pause` 经链路同步对端）；
//!   `Disconnect`/`QuitApp` → 主控侧 `LocalCmd::Quit` 优雅断开；
//! - `auto_quit` 有值时驱动到时限即收尾（保持 Running 终态可见——headless CI 冒烟
//!   断言依赖它；gpui 侧用 `Application::headless()` 起进程回路，见 `ui::run_headless`）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use xcontrol_core::clock::SystemClock;
use xcontrol_core::mock::{RecordingInputSink, ScriptedInputSource};
use xcontrol_core::route::{ApplyReport, Dispatcher, apply_actions};
use xcontrol_core::session::{
    DisconnectReason, Identity, LocalCmd, Notice, Role as SessionRole, Session, SessionAction,
    SessionState, StateError,
};
use xcontrol_core::sink::InputSource;
use xcontrol_net::transport::{LoopbackTransport, Transport as _};
use xcontrol_net::{HandshakeDeadline, HandshakeRole, NoiseHandshake, SecureTransport};
use xcontrol_proto::Message as _;
use xcontrol_proto::input::{Axis, InputEvent, KeyState, MouseButton};
use xcontrol_proto::xkey::{KEY_A, KEY_LEFT_META};

use crate::state::{ConnState, CoreCommand, CoreHandle, StateBus};

/// 演示脚本的推进节奏（每 tick 从队列推一条事件进捕获源，模拟真实输入节奏）。
const DEMO_TICK_PERIOD: Duration = Duration::from_millis(250);

/// 主循环步进周期（收发泵 + 命令轮询 + auto-quit 检查）。
const PUMP_PERIOD: Duration = Duration::from_millis(20);

/// 心跳判定步进周期（Session 的 Timer 事件；3s ping / 1s modsync / 9s 判死
/// 都在会话内部按 [`SystemClock`] 判定，这里只负责喂 Timer）。
const TIMER_PERIOD: Duration = Duration::from_millis(200);

/// 回环握手窗口（进程内回环，5s 已远超需要；失败即报错收尾）。
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(5);

/// 回环演示身份（M2 演示用；M3 发现层落地后改为每设备真实 UUID）。
#[must_use]
pub fn loopback_identity(name: &str, instance_uuid: [u8; 16]) -> Identity {
    Identity {
        name: name.to_owned(),
        screen_name: format!("{name}-screen"),
        instance_id: instance_uuid,
        caps: xcontrol_proto::Caps::UNICODE.union(xcontrol_proto::Caps::ABS_POINTER),
        salt: xcontrol_net::derive_salt(&instance_uuid),
    }
}

/// 演示脚本：修饰键包裹的按键 / 鼠标相对移动 / 按下抬起 / 滚轮 / 文本，
/// 覆盖 docs/02 §4.2 常用 kind（产出顺序 = 状态页计数的推进顺序）。
fn demo_script() -> VecDeque<InputEvent> {
    VecDeque::from(vec![
        InputEvent::Key {
            code: KEY_LEFT_META,
            state: KeyState::Down,
        },
        InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        },
        InputEvent::Key {
            code: KEY_A,
            state: KeyState::Up,
        },
        InputEvent::Key {
            code: KEY_LEFT_META,
            state: KeyState::Up,
        },
        InputEvent::MouseMoveRel {
            dx: 24.0,
            dy: -12.0,
        },
        InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        },
        InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Up,
        },
        InputEvent::Wheel {
            axis: Axis::Vertical,
            delta: -2.0,
        },
        InputEvent::Text {
            utf8: "XControl M2 回环演示".to_owned(),
        },
    ])
}

/// 回环驱动器收尾原因（冒烟输出/日志的语义锚点）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopbackEnd {
    /// 到达 `auto_quit` 时限（headless 冒烟路径；Running 终态保持可见）。
    Deadline,
    /// 会话断开（命令退出/违规/心跳判死），携带人类可读原因。
    Disconnected(String),
}

/// 回环驱动结果（headless 冒烟打印用）。
#[derive(Debug, Clone)]
pub struct LoopbackSummary {
    /// 收尾原因。
    pub end: LoopbackEnd,
    /// 运行时长。
    pub ran_for: Duration,
}

/// 回环驱动的启动形态：常规 = 独立线程；线程资源枯竭（进程级故障）时退化为
/// 调用线程内同步驱动（快照/日志语义一致，只是 main 被阻塞到演示结束）。
#[must_use = "驱动在后台推进；headless 冒烟需 join_summary 收结果，窗口模式也应保留句柄"]
pub enum LoopbackRun {
    /// 独立线程（常规路径；headless 冒烟 join 它，窗口模式让它随进程存活）。
    Threaded(thread::JoinHandle<LoopbackSummary>),
    /// 同步回退（`Builder::spawn` 失败时的唯一无 panic 收尾——记录后原地驱动）。
    Inline(LoopbackSummary),
}

impl LoopbackRun {
    /// 收取驱动结果（Threaded 阻塞至线程结束；Inline 直接返回）。
    ///
    /// # Errors
    /// Threaded 分支线程 panic 时上抛（附 panic 载荷的 Debug 形态）。
    pub fn join_summary(self) -> anyhow::Result<LoopbackSummary> {
        match self {
            Self::Threaded(handle) => handle
                .join()
                .map_err(|payload| anyhow::anyhow!("回环驱动线程 panic：{payload:?}")),
            Self::Inline(summary) => Ok(summary),
        }
    }
}

/// 启动回环驱动（非阻塞；headless 冒烟 `join_summary`，窗口模式保留句柄即可）。
///
/// 线程内错误不上抛（窗口模式下 main 已无阻塞点可接）：记录 `last_error` 快照 +
/// `tracing::error!`，线程正常收尾。（返回的 [`LoopbackRun`] 自身已带 must_use。）
pub fn start(
    bus: Arc<StateBus>,
    rx_cmd: mpsc::Receiver<CoreCommand>,
    psk: [u8; 32],
    identity: Identity,
    auto_quit: Option<Duration>,
) -> LoopbackRun {
    // 回退需用的值先克隆（spawn 闭包按 move 捕获原件；spawn 失败时闭包被
    // 丢弃，原件不可恢复——Arc 克隆廉价、Identity 为 Clone）。
    let fallback_bus = Arc::clone(&bus);
    let fallback_identity = identity.clone();
    match thread::Builder::new()
        .name("loopback-core".to_owned())
        .spawn(move || drive(bus, rx_cmd, psk, identity, auto_quit))
    {
        Ok(handle) => LoopbackRun::Threaded(handle),
        Err(error) => {
            // Builder::spawn 仅在线程资源枯竭时失败（进程级故障，无恢复路径）：
            // 退化为主线程内同步驱动，保证快照/日志语义一致（命令通道降级为空）。
            tracing::error!(target: "app::loopback", %error, "回环线程创建失败，退化为同步驱动（命令通道降级为空）");
            let (_tx, rx) = mpsc::channel(1);
            LoopbackRun::Inline(drive(fallback_bus, rx, psk, fallback_identity, auto_quit))
        }
    }
}

/// 驱动器主体：建链（Noise + 协商）→ 脚本推进 → 泵收发 → 命令/时限收尾。
fn drive(
    bus: Arc<StateBus>,
    mut rx_cmd: mpsc::Receiver<CoreCommand>,
    psk: [u8; 32],
    identity: Identity,
    auto_quit: Option<Duration>,
) -> LoopbackSummary {
    let started = Instant::now();
    tracing::info!(target: "app::loopback", "回环演示启动：进程内 host↔client 全链路（Noise + Session/Dispatcher + mock 注入）");

    let end = match run_chain(&bus, &mut rx_cmd, psk, &identity, auto_quit, started) {
        Ok(end) => end,
        Err(error) => {
            tracing::error!(target: "app::loopback", %error, "回环链路异常收尾");
            bus.report(|state| state.last_error = Some(error.clone()));
            bus.report(|state| state.conn = ConnState::Disconnected { reason: error });
            bus.flush();
            LoopbackEnd::Disconnected("链路异常".to_owned())
        }
    };

    // 收尾 flush：节流窗口内的最后一次 report 不推送（合并语义），
    // 驱动结束后不会再有推送机会——这里强制推一次终值，保证 watch 流
    // （UI/冒烟观测端）与 current() 收敛。
    bus.flush();
    let summary = LoopbackSummary {
        end,
        ran_for: started.elapsed(),
    };
    tracing::info!(target: "app::loopback", end = ?summary.end, elapsed_ms = summary.ran_for.as_millis() as u64, "回环演示收尾");
    // 冒烟输出（CI 断言「输出含事件计数」）：在驱动线程内打印——
    // macOS 的 quit 走 NSApplication terminate（进程在 Application::run 内终结），
    // main 在 run 之后的代码不可依赖。
    println!(
        "回环演示结束（{}ms）：conn={} events_out={} events_in={} packets_out={} packets_in={} end={:?}",
        summary.ran_for.as_millis(),
        bus.current().conn.label(),
        bus.current().events_out,
        bus.current().events_in,
        bus.current().packets_out,
        bus.current().packets_in,
        summary.end,
    );
    summary
}

/// 链路驱动（错误以 String 收束——线程内无上抛点）。
fn run_chain(
    bus: &Arc<StateBus>,
    rx_cmd: &mut mpsc::Receiver<CoreCommand>,
    psk: [u8; 32],
    identity: &Identity,
    auto_quit: Option<Duration>,
    started: Instant,
) -> Result<LoopbackEnd, String> {
    bus.report(|state| state.conn = ConnState::Connecting);
    bus.flush();

    // —— Noise XXpsk0 握手（三报文交错，同步 API 需两条子线程并发；
    //    wire 传入克隆、原件留给会话——LoopbackTransport 克隆共享同一队列对，
    //    scenarios.rs handshake_pair_on 同形）——
    let (mut client_wire, host_wire, _control) = LoopbackTransport::pair(None);
    let host_join = {
        let mut wire = host_wire.clone();
        let deadline = HandshakeDeadline::after(HANDSHAKE_WINDOW);
        thread::spawn(move || {
            NoiseHandshake::run(&mut wire, HandshakeRole::Responder, &psk, &deadline)
        })
    };
    let client_st = NoiseHandshake::run(
        &mut client_wire,
        HandshakeRole::Initiator,
        &psk,
        &HandshakeDeadline::after(HANDSHAKE_WINDOW),
    )
    .map_err(|error| format!("回环握手（initiator）失败：{error}"))?;
    let host_st = host_join
        .join()
        .map_err(|_| "回环握手线程 panic".to_owned())?
        .map_err(|error| format!("回环握手（responder）失败：{error}"))?;

    // —— 会话组装（host = 主控，client = 从机；时钟 = SystemClock，真实节奏）——
    let mut host = PeerEnd::new(SessionRole::Host, identity.clone(), host_wire, host_st);
    let mut client = PeerEnd::new(
        SessionRole::Client,
        identity.clone(),
        client_wire,
        client_st,
    );

    // 就绪事件：携带握手摘要进入版本协商（docs/02 §3.3/§5）。
    host.on_handshake_ready()
        .map_err(|error| format!("host 就绪失败：{error}"))?;
    client
        .on_handshake_ready()
        .map_err(|error| format!("client 就绪失败：{error}"))?;

    bus.report(|state| state.conn = ConnState::Handshaking);
    bus.flush();

    // 协商序列（host Hello → client HelloBack → 双方 Running），回环队列有序，
    // 泵 4 轮必达（scenarios.rs run_negotiation 同构）。
    for _ in 0..4 {
        host.pump_in(bus)?;
        client.pump_in(bus)?;
        host.flush_out(bus);
        client.flush_out(bus);
        if host.is_running() && client.is_running() {
            break;
        }
        thread::sleep(PUMP_PERIOD);
    }
    if !host.is_running() || !client.is_running() {
        return Err("版本协商未在预期轮次内完成".to_owned());
    }
    tracing::info!(target: "app::loopback", "协商完成：双方 Running");
    report_running(bus, &host);
    report_running(bus, &client);

    // —— 稳态循环：脚本推进 + 双向泵 + 心跳 + 命令 ——
    let mut script = demo_script();
    let mut source = ScriptedInputSource::empty();
    let mut next_demo = Instant::now() + DEMO_TICK_PERIOD;
    let mut next_timer = Instant::now() + TIMER_PERIOD;

    loop {
        // auto-quit（headless 冒烟分支）：保持 Running 终态可见，直接收尾。
        if let Some(limit) = auto_quit
            && started.elapsed() >= limit
        {
            return Ok(LoopbackEnd::Deadline);
        }

        // 捕获路径：脚本事件逐条进源 → poll 出批 → 编码进出站缓冲。
        if Instant::now() >= next_demo {
            next_demo += DEMO_TICK_PERIOD;
            if let Some(event) = script.pop_front() {
                source.push(event);
            }
            let mut batch = Vec::new();
            source.poll(&mut batch);
            if !batch.is_empty() {
                host.capture(batch, bus);
            }
        }

        // 心跳判定（3s ping / 1s modsync / 9s 判死在会话内判定）。
        if Instant::now() >= next_timer {
            next_timer += TIMER_PERIOD;
            host.on_timer(bus)?;
            client.on_timer(bus)?;
        }

        // 双向泵：收报文 → 解密 → 喂 Dispatcher → 动作应用。
        host.pump_in(bus)?;
        client.pump_in(bus)?;
        host.flush_out(bus);
        client.flush_out(bus);

        // 命令轮询（UI / 托盘，docs/06 §6；M2 命令低频，逐条处理）。
        while let Some(command) = CoreHandle::try_recv(rx_cmd) {
            match command {
                CoreCommand::TransferFiles => {
                    crate::file_channel::lock_hub().request_pull_all_pending();
                }
                CoreCommand::Pause => {
                    host.on_local(LocalCmd::Pause, bus)?;
                    bus.report(|state| state.conn = ConnState::Running { paused: true });
                    bus.flush();
                }
                CoreCommand::Resume => {
                    host.on_local(LocalCmd::Resume, bus)?;
                    bus.report(|state| state.conn = ConnState::Running { paused: false });
                    bus.flush();
                }
                CoreCommand::Disconnect | CoreCommand::QuitApp => {
                    host.on_local(LocalCmd::Quit, bus)?;
                    client.on_local(LocalCmd::Quit, bus)?;
                }
                // 托盘动作的回环语义边界（FR-1.5 演示进程内双角色同进程）：
                // 角色切换/从机切换/候选确认在回环拓扑无意义——留痕忽略（演示继续）。
                CoreCommand::SwitchRole
                | CoreCommand::SwitchToSlave { .. }
                | CoreCommand::ConfirmHost { .. } => {
                    tracing::info!(target: "app::loopback", ?command,
                        "回环演示模式不支持该托盘动作（进程内双角色，无角色可切换）");
                }
                // 运行时配置更新（11-18-m4-ui，docs/06 §4.1）：回环演示无共享配置
                // 单元（码/角色来自 CLI），唯一立即生效项 = 日志级别热改；其余字段
                // 留痕（演示进程不落盘不转发）。
                CoreCommand::RuntimeConfigUpdate(config) => {
                    crate::logging::set_level(config.general.log_level);
                    // 界面语言（FR-6.5）：与 log_level 同消费点立即生效。
                    xcontrol_i18n::set_language(
                        xcontrol_i18n::Lang::parse(&config.general.language)
                            .unwrap_or(xcontrol_i18n::Lang::ZhCn),
                    );
                    tracing::info!(target: "app::loopback",
                        level = config.general.log_level.as_str(),
                        "回环演示收到配置更新：日志级别立即生效（其余字段演示模式不消费）");
                }
            }
        }

        // 断开检测（Quit/违规/心跳判死都会落 Disconnected 终态）。
        if let Some(reason) = host.disconnected_reason() {
            bus.report(|state| {
                state.conn = ConnState::Disconnected {
                    reason: reason.clone(),
                }
            });
            bus.flush();
            return Ok(LoopbackEnd::Disconnected(reason));
        }
        if let Some(reason) = client.disconnected_reason() {
            bus.report(|state| {
                state.conn = ConnState::Disconnected {
                    reason: reason.clone(),
                }
            });
            bus.flush();
            return Ok(LoopbackEnd::Disconnected(reason));
        }

        thread::sleep(PUMP_PERIOD);
    }
}

/// 断开原因的人类可读描述（不含安全码/密钥派生，error-handling.md 红线 6）。
fn describe_disconnect(reason: &DisconnectReason) -> String {
    match reason {
        DisconnectReason::Timeout => "心跳超时（9s 无帧）".to_owned(),
        DisconnectReason::VersionIncompatible => "协议版本不兼容".to_owned(),
        DisconnectReason::ProtocolViolation => "协议违规".to_owned(),
        DisconnectReason::PeerError { code, msg } => format!("对端错误（{code:?}）：{msg}"),
        DisconnectReason::LocalQuit => "本端主动退出".to_owned(),
    }
}

/// 推送 Running 快照（协商完成后；会话内取对端名与会话 id）。
fn report_running(bus: &Arc<StateBus>, peer: &PeerEnd) {
    let session_id_hex = peer.session.session_id().map(|id| crate::state::hex(&id));
    let peer_name = peer.session.peer_name().map(str::to_owned);
    bus.report(|state| {
        state.conn = ConnState::Running { paused: false };
        if state.session_id_hex.is_none() {
            state.session_id_hex = session_id_hex;
        }
        if state.peer_name.is_none() {
            state.peer_name = peer_name;
        }
    });
    bus.flush();
}

/// 回环一端：会话 + Noise 传输 + 回环线 + 注入记录 + 出站缓冲。
struct PeerEnd {
    session: Session<SystemClock>,
    st: SecureTransport,
    wire: LoopbackTransport,
    sink: RecordingInputSink,
    out: Vec<u8>,
    capture_seq: u32,
    /// 最近一次动作序列中的 Notice（`apply` 时缓存，`notice_to_bus` 消费）。
    last_notices: Vec<Notice>,
}

impl PeerEnd {
    fn new(
        role: SessionRole,
        identity: Identity,
        wire: LoopbackTransport,
        st: SecureTransport,
    ) -> Self {
        let session = Session::new(
            role,
            identity,
            SystemClock::new(),
            xcontrol_proto::Options::defaults(),
        );
        Self {
            session,
            st,
            wire,
            sink: RecordingInputSink::new(),
            out: Vec::new(),
            capture_seq: 0,
            last_notices: Vec::new(),
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.session.state(), SessionState::Running { .. })
    }

    fn disconnected_reason(&self) -> Option<String> {
        match self.session.state() {
            SessionState::Disconnected(reason) => Some(describe_disconnect(reason)),
            _ => None,
        }
    }

    /// HandshakeReady 就绪（携带握手摘要；host 侧产出 Hello 进出站缓冲）。
    fn on_handshake_ready(&mut self) -> Result<(), StateError> {
        let actions = {
            let mut dispatcher = Dispatcher::new(&mut self.session);
            dispatcher.on_handshake_ready(self.st.handshake_hash())?
        };
        self.apply(&actions);
        Ok(())
    }

    /// 捕获一批事件：逐条编码（键盘/按钮严禁合并，docs/02 §4.2）。
    fn capture(&mut self, batch: Vec<InputEvent>, bus: &Arc<StateBus>) {
        let count = batch.len() as u64;
        for event in batch {
            self.capture_seq = self.capture_seq.wrapping_add(1);
            if let Err(error) = event.encode_frame_into(self.capture_seq, &mut self.out) {
                let error = format!("捕获帧编码失败（proto 约束违规）：{error}");
                bus.report(|state| state.last_error = Some(error));
            }
        }
        bus.report(|state| state.events_out += count);
    }

    /// Timer 滴答（心跳判定）。
    fn on_timer(&mut self, bus: &Arc<StateBus>) -> Result<(), String> {
        let actions = {
            let mut dispatcher = Dispatcher::new(&mut self.session);
            dispatcher
                .on_timer()
                .map_err(|error| format!("Timer 事件被拒：{error}"))?
        };
        self.apply(&actions);
        self.notice_to_bus(bus);
        Ok(())
    }

    /// 本地命令直通。
    fn on_local(&mut self, cmd: LocalCmd, bus: &Arc<StateBus>) -> Result<(), String> {
        let actions = {
            let mut dispatcher = Dispatcher::new(&mut self.session);
            dispatcher
                .on_local(cmd)
                .map_err(|error| format!("本地命令被拒：{error}"))?
        };
        self.apply(&actions);
        self.notice_to_bus(bus);
        Ok(())
    }

    /// 收一帧（非阻塞）：解密 → 喂 Dispatcher → 动作应用。
    /// 违规/解密失败返回 Err（调用方收尾断开）。
    fn pump_in(&mut self, bus: &Arc<StateBus>) -> Result<(), String> {
        let mut ciphertext = Vec::new();
        match self.wire.try_recv(&mut ciphertext) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => return Err(format!("回环接收失败：{error}")),
        }
        let mut plaintext = Vec::new();
        self.st
            .decrypt_into(&ciphertext, &mut plaintext)
            .map_err(|error| format!("回环解密失败：{error}"))?;
        bus.report(|state| state.packets_in += 1);

        let outcome = {
            let mut dispatcher = Dispatcher::new(&mut self.session);
            dispatcher.feed(&plaintext)
        };
        if let Some(fatal) = &outcome.fatal {
            return Err(format!("链路违规断开：{fatal}"));
        }
        let report = self.apply(&outcome.actions);
        bus.report(|state| state.events_in += report.injected as u64);
        self.notice_to_bus(bus);
        Ok(())
    }

    /// 出站缓冲整批加密发出（一帧流一条 Noise 报文，docs/02 §4.1）。
    fn flush_out(&mut self, bus: &Arc<StateBus>) {
        let bytes = std::mem::take(&mut self.out);
        if bytes.is_empty() {
            return;
        }
        let mut ciphertext = Vec::new();
        match self.st.encrypt_into(&bytes, &mut ciphertext) {
            Ok(()) => match self.wire.send(&ciphertext) {
                Ok(()) => {
                    bus.report(|state| state.packets_out += 1);
                }
                Err(error) => {
                    let error = format!("回环发送失败：{error}");
                    bus.report(|state| state.last_error = Some(error));
                }
            },
            Err(error) => {
                let error = format!("回环加密失败：{error}");
                bus.report(|state| state.last_error = Some(error));
            }
        }
    }

    /// 动作应用：注入/warp/释放进 sink；Send 编码进出站缓冲；Notice 缓存待消费。
    fn apply(&mut self, actions: &[SessionAction]) -> ApplyReport {
        self.last_notices = actions
            .iter()
            .filter_map(|action| match action {
                SessionAction::Notice(notice) => Some(notice.clone()),
                _ => None,
            })
            .collect();
        let PeerEnd { sink, out, .. } = self;
        apply_actions(actions, sink, &mut |seq, msg| {
            if let Err(error) = msg.encode_frame_into(seq, out) {
                tracing::warn!(target: "app::loopback", %error, "出站帧编码失败（丢弃）");
            }
        })
    }

    /// Notice 分发：协商完成上报快照、断开记日志（docs/06 §6 状态跃迁走 flush）。
    fn notice_to_bus(&self, bus: &Arc<StateBus>) {
        for notice in &self.last_notices {
            match notice {
                Notice::HandshakeComplete { session_id, minor } => {
                    tracing::info!(target: "session::loopback",
                        session_id = crate::state::hex(session_id), minor,
                        "版本协商完成");
                    let peer_name = self.session.peer_name().map(str::to_owned);
                    let id_hex = crate::state::hex(session_id);
                    bus.report(|state| {
                        state.session_id_hex = Some(id_hex);
                        if state.peer_name.is_none() {
                            state.peer_name = peer_name;
                        }
                    });
                }
                Notice::Disconnected(reason) => {
                    tracing::warn!(target: "session::loopback", "会话断开：{}", describe_disconnect(reason));
                }
                Notice::EnteredControl { target } => {
                    tracing::info!(target: "session::loopback", target, "进入受控态");
                }
                Notice::LeftControl => {
                    tracing::info!(target: "session::loopback", "退出受控态");
                }
                Notice::EdgeReturnedToHost { target, overflow } => {
                    tracing::info!(target: "session::loopback", target, ?overflow,
                        "受控屏外侧滑出：边缘回切主控（docs/03 §5；overflow = 回报段回切行程）");
                }
                Notice::SwitchRequested { target } => {
                    tracing::info!(target: "session::loopback", target, "对端请求切换");
                }
                Notice::SwitchResult { target, ok } => {
                    tracing::info!(target: "session::loopback", target, ok, "切换结果");
                }
                Notice::LayoutReceived { screens } => {
                    tracing::info!(target: "session::loopback", screens,
                        "收到主控布局（映射表由 layout_rt 安装；回环仅记录）");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_config::Role;

    use super::*;
    use crate::state::AppState;

    /// 测试假码派生（docs/08 §4：fixture/测试永不用真实安全码；进程内一次）。
    fn test_psk() -> [u8; 32] {
        static PSK: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        *PSK.get_or_init(|| {
            xcontrol_net::derive_psk("loopback-test-code", &[0u8; 16]).expect("固定输入派生")
        })
    }

    fn test_identity() -> Identity {
        loopback_identity("loopback-demo", [7u8; 16])
    }

    /// 回环链路端到端冒烟（无 UI）：驱动到 auto-quit 时限，断言双方 Running
    /// 达成、脚本事件双向计数一致、会话 id 已派生、无错误。
    #[test]
    fn loopback_chain_reaches_running_and_counts_events() {
        let initial = AppState {
            role: Role::Host,
            ..AppState::default()
        };
        let bus = StateBus::new(initial);
        let mut rx = bus.subscribe();
        let (_tx, rx_cmd) = mpsc::channel(16);

        // 2600ms：脚本 9 条 × 250ms 间隔在 2250ms 内推完（并行测试负载下
        // 泵节奏会漂移，截止判定只截断观察窗，不承诺末帧送达）。
        let run = start(
            Arc::clone(&bus),
            rx_cmd,
            test_psk(),
            test_identity(),
            Some(Duration::from_millis(2600)),
        );
        let summary = run.join_summary().expect("驱动线程正常收尾");

        assert_eq!(summary.end, LoopbackEnd::Deadline, "按时限收尾");
        let final_state = bus.current();
        assert_eq!(
            final_state.conn,
            ConnState::Running { paused: false },
            "协商完成进入 Running：{:?}",
            final_state.conn
        );
        assert!(
            final_state.session_id_hex.is_some(),
            "会话 id 已派生（docs/02 §3.3）"
        );
        assert_eq!(
            final_state.peer_name.as_deref(),
            Some("loopback-demo"),
            "对端名来自 Hello/HelloBack"
        );
        assert_eq!(
            final_state.events_out, 9,
            "演示脚本全部捕获出站：{}",
            final_state.events_out
        );
        // 注入计数在截止前推进即可（末帧在截止边缘可能未达——无损保序的
        // 强断言由 net tests/scenarios.rs 全链路用例承载，此处是 app 冒烟口径）。
        assert!(
            final_state.events_in >= 5 && final_state.events_in <= final_state.events_out,
            "链路注入推进且无幻影计数：{} / {}",
            final_state.events_in,
            final_state.events_out
        );
        assert!(
            final_state.packets_out >= 1 && final_state.packets_in >= 1,
            "Noise 报文收发计数：out={} in={}",
            final_state.packets_out,
            final_state.packets_in
        );
        assert!(
            final_state.last_error.is_none(),
            "无错误：{:?}",
            final_state.last_error
        );

        // watch 流最终可见同一快照（UI 纯投影的数据源契约）：
        // 驱动收尾 flush 后，节流窗口内未推送的最后一次 report 已补推。
        let watched = rx.borrow_and_update().clone();
        assert_eq!(watched.events_in, final_state.events_in);
        assert_eq!(watched.events_out, final_state.events_out);
    }

    /// Pause 命令语义：UI 命令 → 主控 LocalCmd::Pause → CtrlMsg::Pause 经链路
    /// 同步对端 → 快照 Running{paused=true}。
    #[test]
    fn pause_command_propagates_across_loop() {
        let bus = StateBus::new(AppState {
            role: Role::Host,
            ..AppState::default()
        });
        let (tx, rx_cmd) = mpsc::channel(16);

        let run = start(
            Arc::clone(&bus),
            rx_cmd,
            test_psk(),
            test_identity(),
            Some(Duration::from_millis(900)),
        );
        // 命令先于稳态循环入队：驱动线程在协商完成后（Running）才消费，
        // Handshaking 期 Local(Pause) 会被状态机拒绝，故此处入队时机安全。
        tx.try_send(CoreCommand::Pause).expect("命令通道空闲");

        let summary = run.join_summary().expect("驱动线程正常收尾");
        assert_eq!(summary.end, LoopbackEnd::Deadline);
        let state = bus.current();
        assert_eq!(
            state.conn,
            ConnState::Running { paused: true },
            "Pause 经链路同步到双方：{:?}",
            state.conn
        );
        assert!(
            state.last_error.is_none(),
            "Pause 不应产生错误：{:?}",
            state.last_error
        );
    }

    /// 断开原因描述：人类可读且不含安全语义载荷。
    #[test]
    fn disconnect_reasons_are_human_readable() {
        use xcontrol_proto::ErrCode;
        assert_eq!(
            describe_disconnect(&DisconnectReason::Timeout),
            "心跳超时（9s 无帧）"
        );
        assert_eq!(
            describe_disconnect(&DisconnectReason::LocalQuit),
            "本端主动退出"
        );
        let described = describe_disconnect(&DisconnectReason::PeerError {
            code: ErrCode::PROTOCOL_VIOLATION,
            msg: "违规".to_owned(),
        });
        assert!(described.contains("对端错误"));
    }
}
