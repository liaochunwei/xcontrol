//! `session_rt` — TCP 会话运行时（tokio 执行域，docs/01 §2）。
//!
//! ```text
//! host: serve(opts, psk) → 逐条接受 Server::next_connection() → Connection（循环）
//! client(手动): connect(addr, psk, throttle) → Connection → 断开后退避重连（FR-5.3）
//! client(发现): Browser::scan_once → 候选逐个 connect → 断开/失败后退避 + 刷新候选
//! 双端: Dispatcher::on_handshake_ready(hs_hash) → select! {
//!   transport.recv → 解密明文喂 Dispatcher → SessionAction
//!   interval.tick  → Dispatcher::on_timer（3s ping / 9s 判死）
//!   rx_cmd.recv    → LocalCmd（Pause/Resume/Quit）
//!   edge 轮询(10ms, host+macos) → EdgeSensor::feed → 门控（暂停/受控中/鼠标键
//!   按下，11-22 第三轮；控制关系变化后 150ms 防抖，11-24）→ LocalCmd::EdgeCrossing
//!   受控光标回报(10ms, client) → 归一化采样（mac=rects 快照；其余=sink 查询，
//!   11-22）→ LocalCmd::ReportControlledCursor
//! } → SessionAction::Send 编码 → transport.send（Noise 加密 + 分帧）
//! ```
//!
//! - 核心状态机在 tokio 侧驱动（[`Session`] + [`SystemClock`]），与
//!   `loopback.rs` 共享同一套 core 语义；状态经 [`StateBus`] 推 UI 快照，
//!   命令经 `mpsc::Receiver` 消费——UI 不感知 TCP 细节（docs/06 §6 纯投影）；
//! - 握手语义（FR-1.4）：serve/connect 全部复用 M2-TCP 交付的
//!   [`xcontrol_net::serve`]——错码折叠 [`NetError::HandshakeFailed`]、
//!   per-来源 IP 限速，本模块只做会话层组装与状态上报；
//! - 重连语义（FR-5.3，11-17-m3-discovery）：M2"直连失败即退出"升级为
//!   [`ReconnectPlanner`] 退避循环（1s→2s→…→30s 封顶、成功重置）；发现模式
//!   每轮退避醒来先刷新候选（docs/02 §2.2"重连期间每轮都先刷新候选"）；
//!   节流（每候选每轮至多 1 次握手）由循环结构 + connect 侧 throttle 承担；
//! - 发现模式 PSK：候选携带主控 instance id（公开，docs/02 §3.1 盐输入），
//!   逐候选 `PskCache` 派生（同码同 id 只派生一次）；id 全零的 TCP 兜底候选
//!   回退手动派生（browse.rs"决策记录"）；
//! - 泄露纪律：错误文案不含安全码/派生中间值；对端只见连接消失。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use xcontrol_config::{
    Config, DiscoveryMethod, HotkeysConfig, KeymapConfig, LayoutConfig, NetConfig,
};
use xcontrol_core::clock::SystemClock;
// HotkeyId/HotkeySet 双端使用；FilterOutcome/HotkeyFilter 仅 mac 主控捕获链
// （cfg 门控，非 macos 编译面不引入——CI 三平台编译面纪律）。
#[cfg(feature = "macos")]
use xcontrol_core::hotkey::{FilterOutcome, HotkeyFilter};
use xcontrol_core::hotkey::{HotkeyId, HotkeySet};
use xcontrol_core::layout::ScreenLayout;
use xcontrol_core::mapping::{MappingEngine, MappingSink};
use xcontrol_core::mock::RecordingInputSink;
use xcontrol_core::route::{Dispatcher, apply_actions};
use xcontrol_core::session::{
    ControlDir, DisconnectReason, Identity, LocalCmd, Notice, Role, Session, SessionAction,
    SessionState, StateError,
};
use xcontrol_core::sink::InputSink;
use xcontrol_net::browse::{Browser, BrowserOptions, Candidate};
use xcontrol_net::reconnect::{ReconnectConfig, ReconnectPlanner};
use xcontrol_net::serve::{self};
use xcontrol_net::transport::TcpTransport;
use xcontrol_net::{Connection, HandshakeThrottle, NetError, PskCache, Server, ServerOptions};
use xcontrol_proto::Options;
#[cfg(feature = "macos")]
use xcontrol_proto::input::Mods;
use xcontrol_proto::input::{InputEvent, KeyState, MouseButton, ScreenId};

use crate::edge::{DEFAULT_POLL_INTERVAL, EdgeHit, EdgeSensor, host_edge_sensor};
use crate::hotkey_rt::{HotkeyAction, HotkeyFilterSink};
use crate::state::{ConnState, CoreCommand, SharedConfig, StateBus};

/// 会话循环的 Timer 步进周期（会话内部按 Options 自行判定 ping/判死时刻）。
const TIMER_PERIOD: Duration = Duration::from_millis(500);

/// `RuntimeConfigUpdate` 的统一消费点（11-18-m4-ui 生效框架，docs/06 §4.1）：
/// 新配置写入共享单元 + 日志级别立即重建（`logging::set_level`）+ 观测日志。
///
/// 返回变化摘要（[`ConfigDelta`]）：换码协议（docs/07 §3.3）消费方据此断开
/// 全部会话——旧 PSK 派生的会话在换码后必须终止重握手；net 段是否需要重建
/// 由调用方按自身基线判定（accept 循环只关心端口、发现循环关心整段 net）。
fn store_runtime_config(cell: &SharedConfig, config: &Config) -> ConfigDelta {
    let code_ref_changed = {
        let mut cell = cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = cell.security_code_ref != config.security_code_ref;
        *cell = config.clone();
        changed
    };
    // 日志级别立即生效（docs/06 §4.1 生效时点表第一行）。
    crate::logging::set_level(config.general.log_level);
    // 界面语言立即生效（FR-6.5，11-19-m6-release）：与 log_level 同消费点——
    // tr() 读进程全局语言，UI 下一次渲染 / 托盘下一轮投影即新语言。
    xcontrol_i18n::set_language(
        xcontrol_i18n::Lang::parse(&config.general.language).unwrap_or(xcontrol_i18n::Lang::ZhCn),
    );
    if code_ref_changed {
        // 换码协议（docs/07 §3.3）：全部会话断开 + PSK 重建——调用方消费
        // [`ConfigDelta::code_ref_changed`]；PskCache 属发现循环局部（重建即
        // 失效，psk.rs `psk_cache_hits_and_invalidation` 钉死换码新键新派生）。
        tracing::warn!(target: "app::settings",
            "安全码引用已变更（docs/07 §3.3 换码协议）：全部会话将断开，运行时按新码重新派生 PSK");
    }
    tracing::info!(target: "app::settings",
        "运行时配置已更新（日志级别/语言/热键/映射立即生效——热键映射走会话热更新路径；剪贴板下会话生效，net 段空闲重建时生效）");
    ConfigDelta { code_ref_changed }
}

/// `store_runtime_config` 的变化摘要（消费方按需反应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigDelta {
    /// 安全码引用变化（docs/07 §3.3 换码协议）：消费方断开会话/上抛重建，
    /// supervisor 侧按新引用重新解析安全码并派生 PSK。
    code_ref_changed: bool,
}

/// 布局配置读取（M5 布局协商随帧路径，docs/06 §4.1 `[layout]` 生效时点）：
/// 锁定共享单元后克隆 `[layout]` 段——锁粒度只覆盖克隆本身，会话循环
/// `select` 分支内不长时间持锁。
fn locked_layout(cell: &SharedConfig) -> LayoutConfig {
    let config = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    config.layout.clone()
}

/// 会话级配置刷新（`SessionConfigs` 构造点读最新值，docs/06 §4.1）：cell 与本地
/// 副本不同则整体替换 keymap/hotkeys。返回是否刷新（观测日志用）。
///
/// 11-21 起运行中会话的热键/映射改动走热更新路径即时生效（`drive_session` 的
/// `RuntimeConfigUpdate` 分支）——本函数只承担**无活动会话**期间的构造点取值
/// （下一次会话按新值编译）。
fn refresh_session_configs(
    cell: &SharedConfig,
    keymap: &mut KeymapConfig,
    hotkeys: &mut HotkeysConfig,
) -> bool {
    let latest = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if latest.keymap != *keymap || latest.hotkeys != *hotkeys {
        *keymap = latest.keymap;
        *hotkeys = latest.hotkeys;
        tracing::info!(target: "app::settings",
            "会话级配置已刷新（会话构造点读取最新 keymap/hotkeys；运行中会话已由热更新路径即时生效，docs/06 §4.1）");
        true
    } else {
        false
    }
}

/// 会话运行体（accept 循环 / 重连循环 / 发现循环）的收束信号（M4 托盘角色切换
/// 的传播通道；main.rs 运行时 supervisor 据此重建或收尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// 循环按自身语义正常收束（命令通道关闭 / 监听停止）：accept 循环退出、
    /// 从机循环退出、supervisor 收尾——不含角色切换语义。
    SessionClosed,
    /// 托盘请求角色切换（FR-1.1，docs/06 §5.2）：会话已优雅断开，调用方必须
    /// **停止自身循环并原样上抛**——supervisor 收到后翻转角色重建运行时。
    RoleSwitch,
    /// 空闲期收到 `RuntimeConfigUpdate` 且 net 段变化（11-18-m4-ui 生效框架，
    /// docs/06 §4.1）：调用方让位给 supervisor 按新配置重建（端口/CIDR/发现层
    /// 需重新绑定——keymap/hotkeys 等会话级配置已入共享单元，重建后自然生效；
    /// 11-21 起运行中会话另有热更新路径即时生效）。
    /// 会话内到达的更新**不**产生此信号（设计裁定：会话不中断；net 段外的
    /// 改动在会话内即时生效或按 §4.1 时点表落地）。
    ConfigReload,
}

/// 会话级配置集（每会话编译一次；会话重建/重连生效。运行中会话的改动走
/// `apply_hot_key_configs` 热更新即时生效——11-21 反馈 3/4，docs/06 §4.1）。
#[derive(Debug, Clone, Copy)]
pub struct SessionConfigs<'a> {
    /// keymap 配置（FR-3.x 映射引擎；docs/03 §4）。
    pub keymap: &'a KeymapConfig,
    /// 热键配置（FR-2.5；docs/03 §4.6：两端本地注册，命中不转发）。
    pub hotkeys: &'a HotkeysConfig,
}

/// 主控侧配置（[`host_listen`] 入参）。
#[derive(Debug, Clone)]
pub struct HostOptions {
    /// 监听地址（生产 = `0.0.0.0:配置端口`；测试 = `127.0.0.1:0`）。
    pub bind: SocketAddr,
    /// PSK（安全码派生；由调用方经 `derive_psk` 得到，不在本模块持有明文码）。
    pub psk: [u8; 32],
    /// 本端身份（`Hello` 载荷来源）。
    pub identity: Identity,
    /// 主控侧限速参数（docs/02 §3.2；生产用 [`Default`]，测试可缩小）。
    pub throttle: xcontrol_net::ThrottleConfig,
    /// keymap 配置（FR-3.x 映射引擎；本端被控时——多主控链式场景——注入同样
    /// 经过映射，docs/03 §4）。
    pub keymap: KeymapConfig,
    /// 热键配置（FR-2.5，docs/03 §4.6：两端本地注册，命中不转发）。
    pub hotkeys: HotkeysConfig,
    /// 主控接入白名单（FR-5.4）：config `net.allowlist` 直传 serve 层——accept
    /// 后、握手前按来源 IP 匹配（docs/07 §6）；空 = 不启用。
    pub allowlist: Vec<String>,
    /// 发现层开关基线（11-18-m4-ui check 补：docs/06 §4.1 net 段空闲重建判定的
    /// 对照基线——主控侧消费 mdns/beacon 两层（scan 属从机扫描层），基线取
    /// 启动通告器时的 config `net.discovery` 原表；显式 `--no-discover` 时通告
    /// 不启动，此基线比较仅造成一次无害重建）。supervisor 侧入参 = 装配本轮
    /// HostOptions 时的 config 快照，与本轮通告器开关同源。
    pub discovery: Vec<DiscoveryMethod>,
}

/// 已绑定监听的主控端（[`host_listen`] 产出；测试据此取随机端口）。
#[derive(Debug)]
pub struct BoundHost {
    /// 实际监听地址（`bind` 端口为 0 时为内核分配端口）。
    pub local_addr: SocketAddr,
    server: Server,
    identity: Identity,
    keymap: KeymapConfig,
    hotkeys: HotkeysConfig,
    discovery_baseline: Vec<DiscoveryMethod>,
    /// 白名单基线（FR-5.4 空闲重建判定：allowlist 变化需重新绑定——serve 层
    /// 白名单表在 listen 时解析固定）。
    allowlist_baseline: Vec<String>,
}

impl BoundHost {
    /// 逐条接受完成握手的连接并驱动会话（M3：会话结束后回到 accept 继续
    /// ——从机断线重连（FR-5.3）与新从机接入都由本循环承接）。
    ///
    /// `announcer` = 主控通告器（可选）：会话运行期间上报"满配对"（beacon
    /// 周期 5s→60s 切换，docs/02 §2.1），会话结束回未满（回到密集通告）。
    ///
    /// # Errors
    /// 监听已停止（无连接可交付 = [`Server`] 已 drop）或会话循环内部错误
    /// （单条会话失败不终止 accept 循环——规则 4，主控保持在线）。
    ///
    /// `rx_cmd` 所有权随调用移入、随返回值交还（M4 运行时 supervisor 跨角色
    /// 重建复用同一命令通道——UI/托盘的 `CoreHandle` 恒指向同一发送端）。
    ///
    /// `config` = 共享配置单元（11-18-m4-ui 生效框架）：accept 循环每轮在
    /// `SessionConfigs` 构造点读最新 keymap/hotkeys（无活动会话期间的下一次
    /// 会话生效；运行中会话另有热更新路径，docs/06 §4.1）；空闲分支消费
    /// `RuntimeConfigUpdate`——端口或发现层开关变化时上抛
    /// [`SessionEnd::ConfigReload`] 让 supervisor 重新绑定/重启通告器。
    pub async fn run_session(
        self,
        bus: Arc<StateBus>,
        mut rx_cmd: mpsc::Receiver<CoreCommand>,
        announcer: Option<Arc<xcontrol_net::Announcer>>,
        config: SharedConfig,
    ) -> anyhow::Result<(SessionEnd, mpsc::Receiver<CoreCommand>)> {
        let Self {
            mut server,
            identity,
            local_addr,
            mut keymap,
            mut hotkeys,
            discovery_baseline,
            allowlist_baseline,
        } = self;
        // FR-1.4 主控侧日志落点（NFR-8，11-17-m3-status）：错码/非法报文的握手
        // 在 net 层折叠断开（docs/02 §0 无原因反馈），折叠事件经观测面上抛——
        // app 顶层逐条告警，主控操作者可见"有人拿错码尝试配对"这一安全信号。
        let mut handshake_failures = server.subscribe_handshake_failures();
        // 会话期间到达的控制连接（drive_session 窥探后暂存）：本会话结束后
        // 优先驱动——串行 accept 语义保留（控制连接排队、不并发多控制会话，
        // 11-21 双连接裁定：剪贴板连接在会话内分派，控制连接仍走本排队面）。
        let mut pending: Option<Connection> = None;
        loop {
            // SessionConfigs 构造点读最新（docs/06 §4.1：映射/热键改动在无活动会话
            // 时下一次从机接入即按新配置编译引擎；运行中会话走热更新路径）。
            let _ = refresh_session_configs(&config, &mut keymap, &mut hotkeys);
            set_connecting(&bus);
            // 三路等待：成功握手连接交付 / 折叠失败通知 / 空闲期命令（三个
            // future 均 cancel-safe，docs/02 §3.2；会话期间到达的失败进缓冲，
            // 下轮取走）。空闲期命令分支为 M4 托盘角色切换新增——无从机接入时
            // accept 循环是唯一命令消费者（drive_session 仅在会话内运行，二者
            // 不并发轮询同一通道）：角色切换立即生效；其余命令（暂停/切换/断开）
            // 在无从机时无作用语义，留痕忽略。
            let connection = if let Some(connection) = pending.take() {
                // 上一会话期间暂存的控制连接优先驱动（重连风暴残余——客户端
                // 已在等待 Hello，立即续上；暂存连接失活则本轮驱动即失败，
                // 回环继续 accept，无害）。
                tracing::info!(target: "session::rt", peer = %connection.peer,
                    "驱动会话期间暂存的控制连接（串行 accept 排队语义）");
                connection
            } else {
                tokio::select! {
                    connection = server.next_connection() => {
                        let Some(connection) = connection else {
                            // 服务已停止（Server drop / 进程退出路径）：正常收尾。
                            return Ok((SessionEnd::SessionClosed, rx_cmd));
                        };
                        tracing::info!(target: "net::serve", peer = %connection.peer, listener = %local_addr, "从机已接入（XXpsk0 握手完成）");
                        connection
                    }
                    failure = handshake_failures.recv() => {
                        match failure {
                            Ok(event) => match event.reason {
                                // 握手失败：原因折叠不外泄（docs/02 §0），来源已计入限速。
                                xcontrol_net::HandshakeFailureReason::Handshake => {
                                    tracing::warn!(target: "net::serve", peer = %event.peer,
                                        "握手失败（安全码不匹配或非法报文；原因折叠不外泄 docs/02 §0，来源已计入限速）");
                                }
                                // 白名单策略拒绝（FR-5.4，docs/07 §6）：非握手失败，
                                // 未进握手未计限速——日志按 reason 区分（观测面语义）。
                                xcontrol_net::HandshakeFailureReason::NotInAllowlist => {
                                    tracing::warn!(target: "net::serve", peer = %event.peer,
                                        reason = event.reason.as_str(),
                                        "连接被白名单拒绝（FR-5.4：来源不在允许网段；未进握手、未计入限速）");
                                }
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                // 观测面滞后：丢条不失效，告警一条滞后说明。
                                tracing::warn!(target: "net::serve", skipped,
                                    "握手失败通知积压丢弃（观测滞后，限速计数不受影响）");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                // Sender 由 Server 持有，run_session 存活期内不可达；兜底收尾。
                                return Ok((SessionEnd::SessionClosed, rx_cmd));
                            }
                        }
                        continue;
                    }
                    command = rx_cmd.recv() => match command {
                        // 空闲期角色切换：立即让位给 supervisor 翻转重建（FR-1.1）。
                        Some(CoreCommand::SwitchRole) => return Ok((SessionEnd::RoleSwitch, rx_cmd)),
                        // 空闲期配置更新（docs/06 §4.1）：写入共享单元 + 日志级别立即；
                        // 端口/白名单变化需重新绑定、发现层开关变化需重启通告器、
                        // 安全码引用变化需重派生 PSK → ConfigReload 让位 supervisor
                        // （其余 net 字段随下轮重建自然生效，无需打断监听；CIDR 主控侧
                        // 不消费，随从机发现循环判定）。
                        Some(CoreCommand::RuntimeConfigUpdate(new_config)) => {
                            let delta = store_runtime_config(&config, &new_config);
                            if delta.code_ref_changed
                                || new_config.net.port != local_addr.port()
                                || new_config.net.discovery != discovery_baseline
                                || new_config.net.allowlist != allowlist_baseline
                            {
                                tracing::info!(target: "session::rt",
                                    old_port = local_addr.port(), new_port = new_config.net.port,
                                    discovery_changed = new_config.net.discovery != discovery_baseline,
                                    allowlist_changed = new_config.net.allowlist != allowlist_baseline,
                                    code_ref_changed = delta.code_ref_changed,
                                    "主控空闲期配置更新：net/码段变化，让位 supervisor 重新绑定");
                                return Ok((SessionEnd::ConfigReload, rx_cmd));
                            }
                            continue;
                        }
                        // 通道关闭（UI 已退出）：accept 循环一并收尾。
                        None => return Ok((SessionEnd::SessionClosed, rx_cmd)),
                        Some(other) => {
                            tracing::debug!(target: "session::rt", ?other,
                                "主控空闲期收到无会话命令（忽略；无从机时无作用语义）");
                            continue;
                        }
                    },
                }
            };
            // 边缘传感器每会话重建（去抖闩锁随会话复位；macos feature 下查询
            // 真实几何，失败/无屏 → None——轮询分支永挂起，零开销跳过）。
            // 合并布局（本机 + config 外接从机屏）——传感器邻接图含从机
            // 才能触发 FR-2.1 边缘切换（11-20 实测接线缺口）。
            let edge = host_edge_sensor(DEFAULT_POLL_INTERVAL, &locked_layout(&config).screens);
            // 满配对上报（beacon 5s→60s 降噪，docs/02 §2.1）。
            if let Some(announcer) = &announcer {
                announcer.set_all_paired(true);
            }
            // 单条会话驱动至断开；错误只记日志不退出 accept 循环
            // （NFR-5 故障隔离：从机崩溃/掉线不影响主控在线）。
            // host 侧传入 accept 通道源：会话期间到达的剪贴板连接在会话内
            // 分派（docs/02 §4.4），控制连接窥探后随返回值暂存。
            let mut end = drive_session(
                Role::Host,
                identity.clone(),
                connection,
                Arc::clone(&bus),
                &mut rx_cmd,
                edge,
                SessionConfigs {
                    keymap: &keymap,
                    hotkeys: &hotkeys,
                },
                &config,
                Some(&mut server),
            )
            .await;
            pending = match &mut end {
                Ok((_, session_pending)) => session_pending.take(),
                Err(_) => None,
            };
            if let Err(error) = &end {
                tracing::warn!(target: "session::rt", %error, "主控侧会话结束（继续等待下一从机）");
            }
            if let Some(announcer) = &announcer {
                announcer.set_all_paired(false);
            }
            if matches!(end, Ok((SessionEnd::RoleSwitch, _))) {
                // 托盘角色切换：会话已优雅断开，accept 循环让位给
                // supervisor 重建（run_session 返回后 server 随之 drop，
                // 监听端口释放供重建复用）。命令通道交还 supervisor。
                return Ok((SessionEnd::RoleSwitch, rx_cmd));
            }
            if matches!(end, Ok((SessionEnd::ConfigReload, _))) {
                // 换码协议（docs/07 §3.3）：会话内安全码引用变更 → 全部
                // 会话断开。server 内嵌旧 PSK，accept 循环不得带着旧监听
                // 继续接客——让位 supervisor 重建（新码派生 + 重新绑定）。
                return Ok((SessionEnd::ConfigReload, rx_cmd));
            }
            if rx_cmd.is_closed() {
                // 命令通道关闭（UI 已退出）：accept 循环一并收尾。
                return Ok((SessionEnd::SessionClosed, rx_cmd));
            }
        }
    }
}

/// 主控侧入口①：绑定监听并启动 accept 循环（每连接 responder 握手）。
///
/// 握手成功连接经 [`BoundHost::run_session`] 交付；错码连接被静默丢弃
/// （docs/02 §0），来源 IP 计入限速。
///
/// # Errors
/// 端口绑定失败（占用/权限）。
pub async fn host_listen(opts: HostOptions) -> anyhow::Result<BoundHost> {
    let server_opts = ServerOptions {
        bind: opts.bind,
        throttle: opts.throttle,
        allowlist: opts.allowlist.clone(),
    };
    let server = serve::serve(server_opts, &opts.psk)
        .await
        .map_err(|error| anyhow::anyhow!("监听 {} 失败：{error}", opts.bind))?;
    let local_addr = server.local_addr();
    tracing::info!(target: "net::serve", %local_addr, "主控监听就绪（等待从机握手）");
    Ok(BoundHost {
        local_addr,
        server,
        identity: opts.identity,
        keymap: opts.keymap,
        hotkeys: opts.hotkeys,
        discovery_baseline: opts.discovery,
        allowlist_baseline: opts.allowlist,
    })
}

/// 从机侧配置（[`client_session`] 入参）。
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// 主控地址（`ip:port`）。
    pub addr: SocketAddr,
    /// PSK（安全码派生）。
    pub psk: [u8; 32],
    /// 本端身份。
    pub identity: Identity,
    /// 从机侧限速参数（docs/02 §3.2 同一套常量，键 = 拨号地址；测试可缩小）。
    pub throttle: xcontrol_net::ThrottleConfig,
    /// keymap 配置（FR-3.x：映射引擎每会话从本配置编译，docs/03 §4）。
    pub keymap: KeymapConfig,
    /// 热键配置（FR-2.5：两端本地注册，命中不转发；docs/03 §4.6）。
    pub hotkeys: HotkeysConfig,
}

/// 从机侧入口（手动地址，FR-5.2 手动路径）：拨号 + initiator 握手 + 会话循环；
/// 断线/失败后按 FR-5.3 退避自动重连（M2"失败即退出"升级为重连循环）。
///
/// 退避序列（[`ReconnectPlanner`]）：1s→2s→4s→…→30s 封顶，会话成功即重置；
/// 每轮重试走 connect 前置限速（docs/02 §3.2 从机自我限速，退避中的重试
/// 不触网）。命令通道关闭（UI 退出）时收尾退出。
///
/// `config` = 共享配置单元（docs/06 §4.1）：每轮重连前刷新 keymap/hotkeys
/// （无会话期间的下次会话生效）；手动路径不感知 net 段变化（地址来自 CLI），不上抛
/// ConfigReload。
///
/// # Errors
/// 仅命令通道关闭时收尾返回（[`SessionEnd::SessionClosed`]）；连接失败不向上抛
/// （转入退避重连，规则 4）；托盘角色切换（[`SessionEnd::RoleSwitch`]）原样上抛。
pub async fn client_session(
    mut opts: ClientOptions,
    bus: Arc<StateBus>,
    mut rx_cmd: mpsc::Receiver<CoreCommand>,
    config: SharedConfig,
) -> anyhow::Result<(SessionEnd, mpsc::Receiver<CoreCommand>)> {
    let mut planner = ReconnectPlanner::new(ReconnectConfig::default());
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(opts.throttle);
    loop {
        if rx_cmd.is_closed() {
            return Ok((SessionEnd::SessionClosed, rx_cmd)); // UI 已退出：不再重连
        }
        // SessionConfigs 构造点读最新（docs/06 §4.1：映射/热键改动于每次重连生效；
        // 运行中会话另有热更新路径即时生效）。
        let _ = refresh_session_configs(&config, &mut opts.keymap, &mut opts.hotkeys);
        if let Some(connection) = dial_with_backoff(&opts, &mut throttle, &bus, &mut planner).await
        {
            tracing::info!(target: "net::connect", addr = %opts.addr, "已连上主控（XXpsk0 握手完成）");
            planner.reset(); // FR-5.3 成功重置：下次断线从 1s 重新爬坡
            // 独立剪贴板通道（11-21 双连接裁定，docs/02 §0/§4.4）：控制会话建立
            // 后另拨一条 TCP 到主控同端口（独立 Noise 握手 + ChannelBind 首帧），
            // 剪贴板大块流与键鼠/心跳帧不再队头阻塞；通道任何失败 3s 独立重连，
            // 控制会话结束时随行关闭（stop 信号）。
            let clip_session_id = connection.session_id;
            let (clip_stop_tx, clip_stop_rx) = tokio::sync::watch::channel(false);
            let clip_task = tokio::spawn(crate::clip_channel::client_clip_channel_loop(
                opts.addr,
                opts.psk,
                opts.throttle,
                clip_session_id,
                crate::clip_channel::clip_max_bytes(&config),
                clip_stop_rx,
            ));
            // 独立文件通道（FR-7.3，docs/02 §4.6，第三条 TCP）：协商 minor ≥ 2
            // 才拨号（泵内门控）；通道失败 3s 独立重连，控制会话结束随行关闭。
            let (file_stop_tx, file_stop_rx) = tokio::sync::watch::channel(false);
            let file_task = tokio::spawn(crate::file_channel::client_file_channel_loop(
                opts.addr,
                opts.psk,
                opts.throttle,
                clip_session_id,
                file_stop_rx,
            ));
            // 从机不轮询本机光标（边缘切换是主控侧语义，docs/03 §5；反向移出由
            // 对端 EnterScreen/LeaveScreen 路由，M3 布局协商收口）。
            // 会话结束（干净断开/错误断开均在函数内投影+日志）→ 回环退避重连。
            let identity = opts.identity.clone();
            let end = drive_session(
                Role::Client,
                identity,
                connection,
                Arc::clone(&bus),
                &mut rx_cmd,
                None,
                SessionConfigs {
                    keymap: &opts.keymap,
                    hotkeys: &opts.hotkeys,
                },
                &config,
                None,
            )
            .await
            .map(|(end, _pending)| end); // client 侧无暂存控制连接
            // 控制会话结束：剪贴板/文件通道随行关闭（独立通道生命周期锚定
            // 控制会话；stop 信号优雅退出 + abort 兜底，await 回收 watcher 线程）。
            let _ = clip_stop_tx.send(true);
            clip_task.abort();
            let _ = clip_task.await;
            let _ = file_stop_tx.send(true);
            file_task.abort();
            let _ = file_task.await;
            crate::file_channel::lock_hub().reset_for_new_session();
            // 错误断开的 tracing 留痕（与主控侧会话结束 warn 同纪律；11-21 观测
            // 日志瘦身移除逐帧【观测】后，错误详情由本条承载——快照 last_error
            // 之外仍有日志可查）。干净断开（Ok）不告警。
            if let Err(error) = &end {
                tracing::warn!(target: "session::rt", addr = %opts.addr, %error,
                    "从机侧会话结束（退避重连，FR-5.3）");
            }
            if matches!(end, Ok(SessionEnd::RoleSwitch)) {
                // 托盘角色切换：不再重连，命令通道交还 supervisor 重建运行时。
                return Ok((SessionEnd::RoleSwitch, rx_cmd));
            }
        }
        if rx_cmd.is_closed() {
            return Ok((SessionEnd::SessionClosed, rx_cmd));
        }
        let wait = planner.record_failure(Instant::now());
        tracing::info!(target: "session::rt", addr = %opts.addr, wait_ms = wait.as_millis() as u64,
            attempts = planner.attempts(), "断线退避：{wait:?} 后重连（FR-5.3）");
        if backoff_wait(&mut rx_cmd, wait, &config).await == BackoffOutcome::RoleSwitch {
            return Ok((SessionEnd::RoleSwitch, rx_cmd));
        }
    }
}

/// 退避等待的收束（M4 修订；11-18-m4-ui 增补配置更新语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum BackoffOutcome {
    /// 等满退避时长（或通道关闭——循环首部 is_closed 收尾）。
    Elapsed,
    /// 退避期收到角色切换：调用方立即上抛 [`SessionEnd::RoleSwitch`]。
    RoleSwitch,
    /// 退避期收到 `RuntimeConfigUpdate`（docs/06 §4.1）：已入共享单元 + 日志级别
    /// 立即。调用方按自身基线判定 net 段是否需要上抛 ConfigReload；会话级字段
    /// 随下次会话构造点自然生效。
    ConfigUpdated,
    /// 退避期收到候选主控确认（FR-5.5）：立即结束等待进入下一轮——发现循环
    /// 按携带的 instance id 直连。
    HostConfirmed {
        /// 被确认主控的 instance id（32 位 hex，命令原值）。
        instance_id: String,
    },
}

/// 退避等待（M4 修订）：sleep 与命令通道并行 select——角色切换（托盘）在退避
/// 期即可生效（11-18-m4-tray AX 实测发现：否则 SwitchRole 滞留缓冲直到下次
/// 会话建立才被消费，从机搜索期角色切换"失灵"）；其余命令在无会话的退避期
/// 无作用语义（暂停/切换需 Running 会话，断开已无连接），留痕忽略防滞留。
///
/// `config` = 共享配置单元（11-18-m4-ui）：退避期到达的 `RuntimeConfigUpdate`
/// 在此消费（写入 + 日志级别立即），不滞留到下个会话。
async fn backoff_wait(
    rx_cmd: &mut mpsc::Receiver<CoreCommand>,
    wait: std::time::Duration,
    config: &SharedConfig,
) -> BackoffOutcome {
    tokio::select! {
        _ = tokio::time::sleep(wait) => BackoffOutcome::Elapsed,
        command = rx_cmd.recv() => match command {
            Some(CoreCommand::SwitchRole) => {
                tracing::info!(target: "session::rt", "退避期收到角色切换：立即翻转（FR-1.1）");
                BackoffOutcome::RoleSwitch
            }
            Some(CoreCommand::RuntimeConfigUpdate(new_config)) => {
                let _delta = store_runtime_config(config, &new_config);
                BackoffOutcome::ConfigUpdated
            }
            Some(CoreCommand::ConfirmHost { instance_id }) => {
                // 确认候选主控（FR-5.5）：退避/等待期到达——立即唤醒下一轮
                // （Elapsed 即重扫/重连），由发现循环按 override 直连。
                tracing::info!(target: "session::rt", instance_id = %instance_id,
                    "退避期收到候选主控确认（FR-5.5）：立即按记忆目标重连");
                BackoffOutcome::HostConfirmed { instance_id }
            }
            None => BackoffOutcome::Elapsed, // 通道关闭：循环首部 is_closed 收尾
            Some(other) => {
                tracing::debug!(target: "session::rt", ?other, "退避期收到无会话命令（忽略）");
                BackoffOutcome::Elapsed
            }
        },
    }
}

/// 退避就绪时拨号一次（FR-5.3）；未就绪先补睡剩余时长。成功返回连接，
/// 失败记快照投影后返回 `None`（等待由调用方按 planner 落实）。
async fn dial_with_backoff(
    opts: &ClientOptions,
    throttle: &mut HandshakeThrottle<SocketAddr>,
    bus: &Arc<StateBus>,
    planner: &mut ReconnectPlanner,
) -> Option<Connection> {
    let remaining = planner.wait_remaining(Instant::now());
    if !remaining.is_zero() {
        tokio::time::sleep(remaining).await;
    }
    set_connecting(bus);
    match client_dial_once(opts, throttle, bus).await {
        Ok(connection) => Some(connection),
        Err(error) => {
            tracing::warn!(target: "net::connect", addr = %opts.addr, %error, "连接失败（进入退避重连）");
            None
        }
    }
}

/// 单次拨号 + 握手（重连循环的轮内原子操作；错误文案与快照投影集中于此）。
///
/// 公开锚点：错误码折叠语义（FR-1.4 唯一变体 [`NetError::HandshakeFailed`]）
/// 可在本层直接断言（tests/handshake_localhost.rs），不被 FR-5.3 重连循环
/// 包裹（循环只属于 [`client_session`]）。
///
/// # Errors
/// [`NetError::RateLimited`]（本地限速）、连接失败、握手失败（唯一折叠变体）
/// ——anyhow 包装的中文文案，根因保留 [`NetError`]（测试可 downcast）。
pub async fn client_dial_once(
    opts: &ClientOptions,
    throttle: &mut HandshakeThrottle<SocketAddr>,
    bus: &Arc<StateBus>,
) -> anyhow::Result<Connection> {
    connect(opts, throttle, bus).await
}

/// 发现模式选项（[`client_discover`] 入参；`--client` 无显式地址时启用）。
#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    /// UDP 发现端口（= config `net.port`，docs/02 §2.1 同端口）。
    pub discovery_port: u16,
    /// 显式 CIDR（`net.cidrs`；空 = 各接口真实前缀，docs/02 §2.2）。
    pub cidrs: Vec<String>,
    /// 接口 allowlist（接口过滤覆盖）。
    pub allowlist: Vec<String>,
    /// mDNS browse 层开关（config `net.discovery` 含 "mdns"，docs/01 §5）。
    pub mdns: bool,
    /// UDP 发现层开关（被动 beacon 监听 + 主动 `XCP1?` 探测；config 含
    /// "beacon"——两层同属 UDP 发现层，一起开关）。
    pub udp: bool,
    /// 兜底 TCP connect 扫描（config 含 "scan"；默认关，docs/02 §2.2 第 3 层；
    /// "scan-off" 为显式关闭写法，不含 "scan" 即关）。
    pub tcp_fallback: bool,
    /// 手动派生 PSK（PSK 盐回退路径：TCP 兜底候选 id 未知 → 按 M2 演示
    /// UUID 派生；browse.rs"决策记录"）。
    pub fallback_psk: [u8; 32],
    /// 从机侧限速参数（docs/02 §3.2；生产默认，测试可缩小）。
    pub throttle: xcontrol_net::ThrottleConfig,
    /// 本端身份（会话协商 `Hello` 载荷）。
    pub identity: Identity,
    /// keymap 配置（FR-3.x：映射引擎每会话从本配置编译，docs/03 §4）。
    pub keymap: KeymapConfig,
    /// 热键配置（FR-2.5：两端本地注册，命中不转发；docs/03 §4.6）。
    pub hotkeys: HotkeysConfig,
    /// 已记忆主控基线（FR-5.5）：config `net.confirmed_host`（32 位 hex）。发现
    /// 循环每轮另从共享配置单元读最新值（`RuntimeConfigUpdate` 传播持久化确认）
    /// ——本字段只是装配时的初值快照（重建语义），非每轮事实源。
    pub confirmed_host: Option<String>,
}

/// 从机侧入口（自动发现，FR-5.1/5.2/5.3/5.5 全链路）：
///
/// ```text
/// loop {
///     候选 = Browser::scan_once()          // mDNS + UDP 探测 + beacon 监听
///     选路（FR-5.5）：
///       confirmed_host 命中候选   → 只连它（已记忆主控优先直连）
///       候选恰 1 台               → 直连（单一主控无歧义）
///       候选 >1 台且无记忆        → 不自动选：候选投影到状态页/托盘子菜单
///                                   等用户确认（ConfirmHost），本轮退避重扫
///     逐选中候选: PskCache 派生(id) → connect（throttle 门控）
///             成功 → 会话；错码失败 → "上次握手失败"标注（软排除可见面）
///             断开后回环 → 重新扫描（刷新候选，docs/02 §2.2）
///     整轮未建立 → 退避 1s→30s → 下一轮刷新候选
/// }
/// ```
///
/// 发现层任何失败（mDNS 不可用/绑定失败）降级空候选不中断循环（规则 4）；
/// 命令通道关闭（UI 退出）时收尾。
///
/// # Errors
/// 仅命令通道关闭时收尾返回（[`SessionEnd::SessionClosed`]）；发现/连接失败均
/// 转退避重连；托盘角色切换（[`SessionEnd::RoleSwitch`]）原样上抛；空闲期
/// `RuntimeConfigUpdate` 且 net 段或安全码引用变化时上抛
/// [`SessionEnd::ConfigReload`]（docs/06 §4.1：端口/CIDR/发现层变更需重建
/// Browser 与层开关；换码需重派生 PSK——docs/07 §3.3）。
pub async fn client_discover(
    mut opts: DiscoverOptions,
    code: xcontrol_config::SecretCode,
    bus: Arc<StateBus>,
    mut rx_cmd: mpsc::Receiver<CoreCommand>,
    config: SharedConfig,
) -> anyhow::Result<(SessionEnd, mpsc::Receiver<CoreCommand>)> {
    let browser = Browser::new(BrowserOptions {
        discovery_port: opts.discovery_port,
        cidrs: opts.cidrs.clone(),
        mdns: opts.mdns,
        beacon_listen: opts.udp,
        udp_probe: opts.udp,
        allowlist: opts.allowlist.clone(),
        tcp_fallback: opts.tcp_fallback,
        ..BrowserOptions::default()
    });
    // net 段基线（docs/06 §4.1 ConfigReload 判定）：本循环由这组发现参数构建。
    let net_baseline = NetConfig {
        port: opts.discovery_port,
        cidrs: opts.cidrs.clone(),
        discovery: net_discovery_flags(opts.mdns, opts.udp, opts.tcp_fallback),
        allowlist: Vec::new(),
        confirmed_host: opts.confirmed_host.clone(),
    };
    // 换码基线（docs/07 §3.3）：本循环的 PSK 由装配时的码派生——引用变化即
    // 上抛 ConfigReload（supervisor 重解析 + 重派生）。
    let code_ref_baseline = config
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .security_code_ref
        .clone();
    let mut planner = ReconnectPlanner::new(ReconnectConfig::default());
    let mut throttle = HandshakeThrottle::<SocketAddr>::with_config(opts.throttle);
    let psk_cache = PskCache::new();
    // FR-5.5：进程内确认（`ConfirmHost` 命令，即时生效——持久化走
    // `RuntimeConfigUpdate` 的 config 落盘传播）。
    let mut confirmed_override: Option<String> = None;
    // FR-5.5 软排除标注集：错码握手失败的候选键（id hex / 地址）——"上次握手
    // 失败"标注的数据源；候选握手成功即撤销（`handshake_failed.remove`）。
    let mut handshake_failed: std::collections::HashSet<String> = std::collections::HashSet::new();
    tracing::info!(target: "session::rt", port = opts.discovery_port,
        mdns = opts.mdns, udp = opts.udp, tcp_fallback = opts.tcp_fallback,
        confirmed = ?opts.confirmed_host,
        "发现模式启动（层开关取 config net.discovery，docs/01 §5 / docs/02 §2.2）");

    loop {
        if rx_cmd.is_closed() {
            return Ok((SessionEnd::SessionClosed, rx_cmd)); // UI 已退出：不再扫描
        }
        // SessionConfigs 构造点读最新（docs/06 §4.1：映射/热键改动于下次会话生效；
        // 运行中会话另有热更新路径即时生效）。
        let _ = refresh_session_configs(&config, &mut opts.keymap, &mut opts.hotkeys);
        let candidates = browser.scan_once().await;
        if candidates.is_empty() {
            tracing::debug!(target: "session::rt", "本轮无候选（mDNS/探测/beacon 均未命中）");
        }

        // —— FR-5.5 选路（docs/00 FR-5.5：多主控候选让用户选择，可记住选择）——
        let confirmed_now = confirmed_override.clone().or_else(|| {
            config
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .net
                .confirmed_host
                .clone()
        });
        let confirmed_candidate = confirmed_now.as_ref().and_then(|id| {
            candidates
                .iter()
                .find(|candidate| candidate_matches_id(candidate, id))
        });
        project_candidates(
            &bus,
            &candidates,
            confirmed_now.as_deref(),
            &failed_keys(&candidates, &handshake_failed),
        );
        let selected: Option<&xcontrol_net::Candidate> = match confirmed_candidate {
            // 已记忆主控在线：只连它（其余候选忽略——记忆语义优先直连）。
            Some(candidate) => Some(candidate),
            None if candidates.len() > 1 => {
                // 多主控无记忆：**不自动选第一个**（FR-5.5）——候选列表已投影，
                // 等用户确认（状态页按钮 / 托盘子菜单），本轮退避重扫保持列表新鲜。
                tracing::info!(target: "session::rt", count = candidates.len(),
                    "发现多台候选主控且无记忆（FR-5.5）：等待用户确认（状态页/托盘子菜单），本轮不自动连接");
                bus.report(|state| {
                    state.conn = ConnState::Disconnected {
                        reason: format!(
                            "发现 {} 台候选主控：请在状态页或托盘选择（FR-5.5）",
                            candidates.len()
                        ),
                    };
                });
                bus.flush();
                None
            }
            None => candidates.first(),
        };

        if let Some(candidate) = selected {
            tracing::info!(target: "session::rt",
                addr = %candidate.addr, name = %candidate.name,
                source = ?candidate.source, id = %crate::state::hex(&candidate.instance_id),
                confirmed = confirmed_candidate.is_some(),
                "选定候选主控");
            let psk = candidate_psk(candidate, &code, &opts.fallback_psk, &psk_cache)?;
            set_connecting(&bus);
            match serve::connect(candidate.addr, &psk, &mut throttle).await {
                Ok(connection) => {
                    tracing::info!(target: "net::connect", addr = %candidate.addr, "已连上主控（XXpsk0 握手完成，安全码在线匹配）");
                    planner.reset();
                    // 错码软排除撤销：该候选握手成功（此前标注不再误导用户）。
                    handshake_failed.remove(&candidate_key(candidate));
                    // 独立剪贴板通道（11-21 双连接裁定，docs/02 §0/§4.4）：与
                    // client_session 同构——控制会话建立后另拨一条 TCP（独立
                    // 握手 + ChannelBind 首帧），通道失败 3s 独立重连，控制会话
                    // 结束随行关闭。PSK = 该候选的派生值（与控制连接同源）。
                    let clip_session_id = connection.session_id;
                    let (clip_stop_tx, clip_stop_rx) = tokio::sync::watch::channel(false);
                    let clip_task = tokio::spawn(crate::clip_channel::client_clip_channel_loop(
                        candidate.addr,
                        psk,
                        opts.throttle,
                        clip_session_id,
                        crate::clip_channel::clip_max_bytes(&config),
                        clip_stop_rx,
                    ));
                    // 独立文件通道（FR-7.3，docs/02 §4.6）：与 client_session 同构。
                    let (file_stop_tx, file_stop_rx) = tokio::sync::watch::channel(false);
                    let file_task = tokio::spawn(crate::file_channel::client_file_channel_loop(
                        candidate.addr,
                        psk,
                        opts.throttle,
                        clip_session_id,
                        file_stop_rx,
                    ));
                    // 会话结束（干净断开/错误断开均在函数内投影+日志）→ 下一轮
                    // 重新扫描（刷新候选，docs/02 §2.2）。
                    let identity = opts.identity.clone();
                    let end = drive_session(
                        Role::Client,
                        identity,
                        connection,
                        Arc::clone(&bus),
                        &mut rx_cmd,
                        None,
                        SessionConfigs {
                            keymap: &opts.keymap,
                            hotkeys: &opts.hotkeys,
                        },
                        &config,
                        None,
                    )
                    .await
                    .map(|(end, _pending)| end); // client 侧无暂存控制连接
                    // 控制会话结束：剪贴板/文件通道随行关闭（stop 信号 + abort 兜底）。
                    let _ = clip_stop_tx.send(true);
                    clip_task.abort();
                    let _ = clip_task.await;
                    let _ = file_stop_tx.send(true);
                    file_task.abort();
                    let _ = file_task.await;
                    crate::file_channel::lock_hub().reset_for_new_session();
                    // 错误断开的 tracing 留痕（同 client_session 手动路径——
                    // 快照 last_error 之外仍有日志可查）。干净断开不告警。
                    if let Err(error) = &end {
                        tracing::warn!(target: "session::rt", addr = %candidate.addr, %error,
                            "从机侧会话结束（重新扫描候选，FR-5.3）");
                    }
                    if matches!(end, Ok(SessionEnd::RoleSwitch)) {
                        // 托盘角色切换：不再扫描，命令通道交还 supervisor 重建运行时。
                        return Ok((SessionEnd::RoleSwitch, rx_cmd));
                    }
                }
                // 本地限速（该候选上一轮刚失败）：跳过，本轮试下一候选。
                Err(NetError::RateLimited) => {
                    tracing::debug!(target: "net::connect", addr = %candidate.addr, "候选在限速退避中，本轮跳过");
                    bus.report(|state| {
                        state.conn = ConnState::Disconnected {
                            reason: "候选限速退避中".to_owned(),
                        };
                    });
                    bus.flush();
                }
                Err(error) => {
                    // 错码软排除的用户可见标注（FR-5.5）：安全码不匹配或不可达
                    // 都折叠 HandshakeFailed（docs/02 §0）——统一标注"上次握手
                    // 失败"，候选列表/托盘子菜单可见。
                    if matches!(error, NetError::HandshakeFailed) {
                        handshake_failed.insert(candidate_key(candidate));
                    }
                    let error = connect_error(candidate.addr, error);
                    tracing::warn!(target: "net::connect", addr = %candidate.addr, %error, "候选握手失败（安全码不匹配或不可达，已标注软排除）");
                }
            }
            if rx_cmd.is_closed() {
                return Ok((SessionEnd::SessionClosed, rx_cmd));
            }
        }
        let wait = planner.record_failure(Instant::now());
        // 退避进度是搜索态常规节奏（非关键节点）：debug——避免无主控时按轮刷屏
        //（连接建立/断开等关键节点仍为 info，docs/06 §6 噪音纪律，11-21 实测反馈）。
        tracing::debug!(target: "session::rt", wait_ms = wait.as_millis() as u64,
            attempts = planner.attempts(), "本轮未建立会话：退避 {wait:?} 后刷新候选（FR-5.3）");
        match backoff_wait(&mut rx_cmd, wait, &config).await {
            BackoffOutcome::RoleSwitch => return Ok((SessionEnd::RoleSwitch, rx_cmd)),
            // net 段与基线不同：上抛 ConfigReload 让 supervisor 按新端口/CIDR/
            // 发现层重建本循环（docs/06 §4.1）；安全码引用变化同理（换码协议，
            // docs/07 §3.3——重派生 PSK）。会话级字段已在共享单元，重建后
            // 构造点自然读到。
            BackoffOutcome::ConfigUpdated => {
                let latest = config
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if latest.net != net_baseline || latest.security_code_ref != code_ref_baseline {
                    tracing::info!(target: "session::rt", old_port = net_baseline.port,
                        new_port = latest.net.port,
                        code_ref_changed = latest.security_code_ref != code_ref_baseline,
                        "发现循环空闲期 net/码段变更：让位 supervisor 重建");
                    return Ok((SessionEnd::ConfigReload, rx_cmd));
                }
            }
            // 用户确认候选（FR-5.5）：跳过剩余退避立即重扫——下一轮按
            // confirmed_override 直连目标。
            BackoffOutcome::HostConfirmed { instance_id } => {
                confirmed_override = Some(instance_id);
                planner.reset();
            }
            BackoffOutcome::Elapsed => {}
        }
    }
}

/// 候选实例 id 与记忆 hex 是否匹配（FR-5.5；大小写不敏感——配置侧手写宽容）。
/// 全零 id（TCP 兜底候选）永不匹配记忆（id 未知，记忆无从建立）。
fn candidate_matches_id(candidate: &xcontrol_net::Candidate, confirmed: &str) -> bool {
    candidate.instance_id != [0u8; 16]
        && crate::state::hex(&candidate.instance_id).eq_ignore_ascii_case(confirmed.trim())
}

/// 候选的记忆/软排除键（FR-5.5）：有 id 用 id hex；全零 id（TCP 兜底）退回
/// 地址串——同一主控跨轮的失败标注保持稳定。
fn candidate_key(candidate: &xcontrol_net::Candidate) -> String {
    if candidate.instance_id == [0u8; 16] {
        candidate.addr.to_string()
    } else {
        crate::state::hex(&candidate.instance_id)
    }
}

/// 软排除集合中被 `candidates` 命中的键（投影辅助；集合可能积累历史地址键，
/// 只投影本轮可见候选的标注）。
fn failed_keys(
    candidates: &[xcontrol_net::Candidate],
    failed: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    candidates
        .iter()
        .map(candidate_key)
        .filter(|key| failed.contains(key))
        .collect()
}

/// 候选列表投影（FR-5.5）：全部候选进快照（含软排除/记忆标注），状态页与
/// 托盘子菜单共用。空候选清空投影（不残留上一轮列表）。
fn project_candidates(
    bus: &Arc<StateBus>,
    candidates: &[xcontrol_net::Candidate],
    confirmed: Option<&str>,
    failed: &std::collections::HashSet<String>,
) {
    let projected: Vec<crate::state::HostCandidate> = candidates
        .iter()
        .map(|candidate| {
            let key = candidate_key(candidate);
            crate::state::HostCandidate {
                name: candidate.name.clone(),
                addr: candidate.addr.to_string(),
                instance_id_hex: crate::state::hex(&candidate.instance_id),
                source: source_label(candidate.source),
                last_handshake_failed: failed.contains(&key),
                confirmed: confirmed.is_some_and(|id| candidate_matches_id(candidate, id)),
            }
        })
        .collect();
    bus.report(|state| state.host_candidates = projected);
}

/// 候选来源的状态页/托盘标签（[`CandidateSource`] 的稳定字面值）。
fn source_label(source: xcontrol_net::CandidateSource) -> &'static str {
    match source {
        xcontrol_net::CandidateSource::Mdns => "mdns",
        xcontrol_net::CandidateSource::Beacon => "beacon",
        xcontrol_net::CandidateSource::Probe => "probe",
        xcontrol_net::CandidateSource::TcpProbe => "tcp",
    }
}

/// 发现循环基线的 `net.discovery` 复原（层开关 → 枚举列表，与 main.rs 的
/// DiscoverOptions 装配同构；仅用于 [`SessionEnd::ConfigReload`] 判定）。
fn net_discovery_flags(mdns: bool, udp: bool, tcp_fallback: bool) -> Vec<DiscoveryMethod> {
    let mut flags = Vec::new();
    if mdns {
        flags.push(DiscoveryMethod::Mdns);
    }
    if udp {
        flags.push(DiscoveryMethod::Beacon);
    }
    if tcp_fallback {
        flags.push(DiscoveryMethod::Scan);
    }
    flags
}

/// 候选 → PSK：有 id 走逐候选派生缓存（docs/02 §3.1 盐 = 公开 instance id，
/// 同 (码,id) 只 argon2 一次）；id 全零（TCP 兜底）回退手动派生路径。
fn candidate_psk(
    candidate: &Candidate,
    code: &xcontrol_config::SecretCode,
    fallback_psk: &[u8; 32],
    cache: &PskCache,
) -> anyhow::Result<[u8; 32]> {
    if candidate.instance_id == [0u8; 16] {
        return Ok(*fallback_psk);
    }
    cache
        .get_or_derive(code.expose(), &candidate.instance_id)
        .map_err(|error| anyhow::anyhow!("PSK 派生失败：{error}"))
}

/// 连接失败 → 人类可读（不含安全码/派生值；FR-1.4 折叠语义保留）。
fn connect_error(addr: SocketAddr, error: NetError) -> anyhow::Error {
    match error {
        NetError::RateLimited => anyhow::anyhow!("握手被本地限速拒绝（退避中）"),
        NetError::IoFailed => anyhow::anyhow!("无法连接主控 {addr}（端口不可达或超时）"),
        NetError::HandshakeFailed => anyhow::anyhow!("安全码握手失败（原因已折叠，docs/02 §0）"),
        other => anyhow::Error::new(other).context("连接主控失败"),
    }
}

/// 拨号 + 握手（错误文案与快照上报集中在这一处）。
async fn connect(
    opts: &ClientOptions,
    throttle: &mut HandshakeThrottle<SocketAddr>,
    bus: &Arc<StateBus>,
) -> anyhow::Result<Connection> {
    match serve::connect(opts.addr, &opts.psk, throttle).await {
        Ok(connection) => Ok(connection),
        Err(NetError::RateLimited) => {
            let error = anyhow::anyhow!("握手被本地限速拒绝（退避中），请稍后重试");
            report_connect_failure(bus, &error);
            Err(error)
        }
        Err(NetError::IoFailed) => {
            let error =
                anyhow::anyhow!("无法连接主控 {addr}（端口不可达或超时）", addr = opts.addr);
            report_connect_failure(bus, &error);
            Err(error)
        }
        // 唯一折叠变体（docs/02 §0）：不区分码不匹配/超时/对端断开。
        Err(error @ NetError::HandshakeFailed) => {
            let error =
                anyhow::Error::new(error).context("安全码握手失败（原因已折叠，见 docs/02 §0）");
            report_connect_failure(bus, &error);
            Err(error)
        }
        Err(error) => {
            let error = anyhow::Error::new(error).context("连接主控失败");
            report_connect_failure(bus, &error);
            Err(error)
        }
    }
}

/// 会话异常收尾上报：快照落 Disconnected（已断开则保留原原因不覆盖）+
/// `last_error` 留痕。预备段/循环收尾共用，保证任何退出路径都有断开投影。
fn report_session_error(bus: &Arc<StateBus>, error: &anyhow::Error) {
    bus.report(|state| {
        if !matches!(state.conn, ConnState::Disconnected { .. }) {
            state.conn = ConnState::Disconnected {
                reason: error.to_string(),
            };
        }
        state.last_error = Some(error.to_string());
    });
    bus.flush();
}

/// 连接失败上报：快照 Disconnected + warn 日志（FR-1.4 拒绝路径可观测）。
fn report_connect_failure(bus: &Arc<StateBus>, error: &anyhow::Error) {
    tracing::warn!(target: "net::connect", %error, "连接失败");
    bus.report(|state| {
        state.conn = ConnState::Disconnected {
            reason: error.to_string(),
        };
        state.last_error = Some(error.to_string());
    });
    bus.flush();
}

/// 会话循环（双端共用）：握手摘要就绪 → select 收帧/心跳/命令/边缘轮询 → 断开收尾。
///
/// `edge` = 主控侧边缘传感器（Host 传 [`host_edge_sensor`] 产出，Client 传
/// `None`）：并入本 select 循环而非独立任务——ControlDir/暂停门控需要会话
/// 状态机的当前值（`session.control_dir()` 就在手边，免共享原子/watch 桥接），
/// 且循环退出（断开）时轮询自然停止，无任务泄漏面。
///
/// `keymap` = 映射配置（FR-3.x，docs/03 §4）：每会话编译一次 [`MappingEngine`]
/// （会话重建/重连生效；运行中改动经 `apply_hot_key_configs` 热替换即时生效，
/// docs/06 §4.1）。注入管线：
///
/// ```text
/// Dispatcher 产出 SessionAction::InjectInput → MappingSink（规则改写）→ 注入 sink
/// ```
///
/// 集成方式（design.md 裁定）：引擎包在 sink 适配层（[`MappingSink`] 包装
/// [`RecordingInputSink`]），`apply_actions` 只见 `&mut dyn InputSink`——client/host
/// 双端共用本循环无需分支，未来平台 sink（M5）同样只换内层。
///
/// EnterScreen 对齐语义（模块内裁定，docs/03 §4 修订记录）：对齐合成 Key 事件与
/// wire Key 事件在 sink 边界同为 `InjectInput(Key)` 不可区分，且两者承载的都是
/// 主控**原始（raw）语义**（Session `injected_mods` 与快照均在 raw 空间）——统一
/// 过映射才是"对端语义平移"（非恒等预设下绕过映射会注入 raw 键 = 欠映射）。
/// reset 时机与 ReleaseAll 原子绑定（`MappingSink::release_all` 同时重置引擎）：
/// 切出/暂停/断开/违规路径都产出 ReleaseAll，无需逐点挂钩；EnterScreen 不做额外
/// reset——此时引擎态应已为空（上次切出已 reset），若此时清表反而丢失活 origin
/// 使随后的修饰键 ↑ 查不到映射（卡键）。
///
/// 热键接线（FR-2.5，M4-hotkeys；检测路径矩阵见 hotkey_rt 模块文档）：
///
/// ```text
/// 注入管线（从机）：SessionAction::InjectInput → HotkeyFilterSink（命中吞 + 投路由）
///                  → MappingSink（规则改写）→ 注入 sink          ← 热键在映射之前（docs/03 §4.1）
/// 主控识别（mac）：CaptureSource.poll → HotkeyFilter（命中 → 路由通道）
///                  → 未命中 + 门控 → LocalCmd::ForwardInput → 出站  ← FR-2.x 主控链路
/// 平台注册（win/x11）：RegisterHotKey / grab_key drain → 路由通道（两端注册）
/// ```
///
/// 命中的 [`HotkeyId`] 统一经 `hotkey_rx` 通道进入路由分支（host/client 各自
/// 语义，hotkey_rt::route_host/route_slave）；`hotkeys` 为 config `[hotkeys]`
/// 节（编译失败降级空集合 + 告警——输入管线不因热键配置错误中断）。
#[allow(clippy::too_many_arguments)]
// 会话驱动入参矩阵（双端共用循环）——refactor 成
// 参数结构体的收益在 M5 布局/剪贴板注入时一并做（此处先收敛 lint）。
///
/// `incoming` = host 侧 accept 通道源（[`Server`]）：11-21 双连接裁定下剪贴板
/// 通道只在控制会话存活期存在，客户端的剪贴板拨入必须随会话循环接受——host
/// 在 select 循环内轮询新连接并按首帧分派（[`crate::clip_channel::classify_incoming`]）；
/// 会话期间到达的**控制**连接（重连风暴残余/第二从机）窥探后暂存于返回值，
/// 由 accept 循环在本会话结束后优先驱动（串行 accept 语义保留）。client 侧
/// 传 `None`（该分支零唤醒）。
///
/// 返回 `(会话收束, 会话期间暂存的控制连接)`——client 侧恒为 `None`。
async fn drive_session(
    role: Role,
    identity: Identity,
    connection: Connection,
    bus: Arc<StateBus>,
    rx_cmd: &mut mpsc::Receiver<CoreCommand>,
    mut edge: Option<EdgeSensor>,
    configs: SessionConfigs<'_>,
    config: &SharedConfig,
    mut incoming: Option<&mut Server>,
) -> anyhow::Result<(SessionEnd, Option<Connection>)> {
    let Connection {
        mut transport,
        session_id,
        peer,
    } = connection;

    // M5 布局协商（docs/02 §4.2）的执行层素材：本机身份副本（从机映射表归属
    // 判定）与本机布局快照（主控装配锚点，会话启动时一次）。
    // 11-22 幻影屏修复：装配锚点必须是**纯本机图**（rects 几何直接建图）——
    // `sensor.layout()` 是 merge 过 externals 的合并图（传感器判定用），拿它
    // 当装配输入会**二次 merge**：Linux 条目重复入图（幻影屏，screens=1+1+1
    // 的第三块）+ 邻接污染（幻影条目挂真实条目的对侧边）——从机映射表越界
    // 告警与回切落点解析的污染源。
    let identity_for_layout = identity.clone();
    let local_layout: Option<ScreenLayout> = edge
        .as_ref()
        .map(|sensor| crate::edge::screen_layout_from_rects(sensor.rects()));
    // 外接（从机）屏全局 id 序（热键/托盘切换目标空间；协商完成后由布局装配填充）。
    let mut slave_targets: Vec<ScreenId> = Vec::new();
    // 受控光标回报采样面（双向边缘回切，11-21 反馈 5；docs/03 §5）：从机侧
    // （Client）按节拍采样本机光标、归一化后回报主控（判定/回切决策全在对端
    // 状态机）。11-22 修复轮起双路径共存：
    // - macos：本机显示矩形快照 + CG 光标查询（现行实测正常，路径不动）；
    // - Linux/其他平台：注入 sink 查询（`InputSink::cursor_normalized`——
    //   uinput ABS 域换算 / X11 QueryPointer。此前非 macos 恒 None = Linux
    //   从机从不回报、主控回切判定永远等不到数据的根因）。
    // Host 恒不回报（被控回报是从机语义）。
    #[cfg(feature = "macos")]
    let controlled_rects: Option<Vec<crate::edge::ScreenRect>> = if role == Role::Client {
        crate::edge::local_display_rects()
    } else {
        None
    };

    // 剪贴板出站上限（FR-4.5）：config `[clipboard].max_bytes` 与协议 16MiB
    // 取小（0=禁用）。11-21 双连接裁定：剪贴板运行时不再驻留会话循环——移交
    // 独立剪贴板通道（[`crate::clip_channel`] 泵任务，通道建立时构造，
    // EchoFilter 进程级共享基线跨重建存活）；本值仅用于会话期间接入的
    // 剪贴板通道分派。
    let clip_max_bytes = crate::clip_channel::clip_max_bytes(config);
    // 独立剪贴板通道句柄（host）：会话期间接入的剪贴板连接（首帧 ChannelBind）
    // 泵任务——会话收尾 / 新通道替换时关闭（新连接优先，防堆积）。
    let mut clip_channel: Option<crate::clip_channel::ClipChannelGuard> = None;
    // 独立文件通道 guard（FR-7.3，docs/02 §4.6；生命周期同剪贴板通道）。
    let mut file_channel: Option<crate::file_channel::FileChannelGuard> = None;
    // 会话期间到达的控制连接（重连风暴残余/第二从机）：窥探后暂存，本会话
    // 结束后随返回值交还 accept 循环优先驱动（串行 accept 语义保留）。
    let mut pending_control: Option<Connection> = None;

    bus.report(|state| {
        state.conn = ConnState::Handshaking;
        state.session_id_hex = Some(crate::state::hex(&session_id));
        // 本机屏幕快照（FR-1.3 布局页只读列表；与主控装配同源——edge 传感器
        // 几何图，缺省单锚 = screen_name，与 build_host_layout 兜底一致）。
        state.local_screens = local_layout.as_ref().map_or_else(
            || vec![identity_for_layout.screen_name.clone()],
            |layout| layout.screens().iter().map(|s| s.name.clone()).collect(),
        );
    });
    bus.flush();

    let mut session = Session::new(role, identity, SystemClock::new(), Options::defaults());
    // —— 热键装配（FR-2.5）——
    // 配置编译：加载期校验兜底，失败降级空集合 + 告警（恒直通过滤器，输入管线
    // 不因热键配置错误中断，error-handling.md 规则 4）。
    let hotkey_set = match HotkeySet::from_config(configs.hotkeys) {
        Ok(set) => set,
        Err(error) => {
            tracing::warn!(target: "app::hotkey", %error,
                "热键配置编译失败：降级为无热键（请检查 config [hotkeys]）");
            HotkeySet::empty()
        }
    };
    // 命中路由通道（捕获 filter / sink filter / 平台注册源三路汇入）。
    let (hotkey_tx, mut hotkey_rx) = mpsc::unbounded_channel::<HotkeyId>();
    // 注入管线：HotkeyFilterSink（热键在映射之前，docs/03 §4.1）→ MappingSink
    // → 平台注入 sink（11-20 生产接线：真实 OS 注入，mock 仅编译面回退）。
    let mut sink = HotkeyFilterSink::new(
        hotkey_set.clone(),
        build_mapping_sink(configs.keymap, build_platform_sink()),
        hotkey_tx.clone(),
    );
    // 平台注册源（win RegisterHotKey / x11 grab_key，两端同型注册；mac 无独立源）。
    let mut hotkey_source = crate::hotkey_rt::PlatformHotkeySource::register(&hotkey_set);
    // 主控捕获源（macos + Host：捕获流热键识别 + FR-2.x 转发链）。
    #[cfg(feature = "macos")]
    let mut capture: Option<xcontrol_input::CaptureSource> = if role == Role::Host {
        crate::hotkey_rt::host_capture_source()
    } else {
        None
    };
    #[cfg(feature = "macos")]
    let mut capture_filter = HotkeyFilter::new(hotkey_set.clone());
    #[cfg(feature = "macos")]
    let mut last_local_mods = Mods::default();
    #[cfg(feature = "macos")]
    let mut last_suppress = false;
    // 释放落点缓存（11-22 回切落点覆盖修复）：受控态的回主控落点先缓存，
    // `suppress=false` 重关联**之后**重放——
    // `CGAssociateMouseAndMouseCursorPosition(true)` 会把光标拉到物理鼠标
    // 当前位置，覆盖 warp 落点（用户实测：回切落点跳到屏幕右侧）；重关联后
    // 重放即最终生效。异常收尾不消费（光标停物理鼠标位，语义自然）。
    // 来源两路（11-24）：边缘回切 = core `WarpPointer` 换算的全局点**加上回切
    // 行程补偿**（收帧分支，`compensated_release_point`——回切触发处排空捕获队列
    // 后立即翻拍重放）；热键回主控 = 切入锚点（热键分支 `apply_home_landing`——
    // core 的落点 warp 已被 `strip_home_warp` 剥离，不进本缓存的换算）。
    #[cfg(feature = "macos")]
    let mut pending_release_warp: Option<(f64, f64)> = None;
    // 在途批（11-24 回切行程补偿，R8；docs/03 §5 回切第 3 条）：输入拍每成功发出
    // 一批，记下发出时刻与批内 `MouseMoveRel` 之和。回切触发时距发出不足
    // `IN_FLIGHT_WINDOW` 的那一批可能在工作机采样**之后**才到——没算进触发回切的
    // 那条回报，由主控补到落点上（纯函数 `in_flight_batch`）。
    #[cfg(feature = "macos")]
    let mut last_sent_batch: Option<(Instant, (f32, f32))> = None;
    // 捕获源自动恢复的最近一次探测时刻（11-24，R9）：主控捕获源缺席时每
    // `CAPTURE_RETRY_INTERVAL` 静默查询一次辅助功能授权，已授权即重建捕获源。
    #[cfg(feature = "macos")]
    let mut capture_probe_at: Option<Instant> = None;
    // 捕获源重建失败告警的退避（11-25）：已授权但捕获源启动持续失败时重试仍每秒
    // 一次，告警首次立即、之后至多 `CAPTURE_FAILURE_LOG_INTERVAL` 一条；恢复成功
    // 复位（下次再失败时重新立即告警）。
    #[cfg(feature = "macos")]
    let mut capture_failure_logged_at: Option<Instant> = None;
    // 回切行程的平滑回放队列（11-25 第二轮，R11；macOS 主控）：边缘回切时三段
    // 丢失行程不再一次性加到落点——快速甩动时一百多点的瞬移，用户实测"直接跳跃"
    //（第一轮的"10ms 节拍 × 6 大步"也不行：每步 20 点仍是跳）。改为光标先落在
    // 未补偿的贴缘基点（与它刚才在工作机屏边的位置连续），补偿量经
    // [`replay_slices`] 切成 2ms 微步（匀速），回切触发处直接交给
    // [`spawn_cursor_replay`] 专用线程高频回放；期间物理鼠标位移照常叠加。
    // 回切行程的平滑回放队列（11-25 第二轮，R11；macOS 主控）：边缘回切时三段
    // 丢失行程不再一次性加到落点——快速甩动时一百多点的瞬移，用户实测"直接跳跃"
    //（第一轮的"10ms 节拍 × 6 大步"也不行：每步 20 点仍是跳）。改为光标先落在
    // 未补偿的贴缘基点（与它刚才在工作机屏边的位置连续），补偿量经
    // [`replay_slices`] 切成 2ms 微步（匀速），回切触发处直接交给
    // [`spawn_cursor_replay`] 专用线程高频回放；期间物理鼠标位移照常叠加。
    #[cfg(feature = "macos")]
    #[allow(unused_assignments)]
    // 初始值仅在触发分支被覆盖前存在（clippy 无法跨分支看到 `take()` 消费）
    let mut cursor_replay: Vec<(f64, f64)> = Vec::new();
    // 本机矩形快照（释放落点换算：`WarpPointer` 全局 id → 全局点；sensor 同
    // 源同序，见 edge.rs release_warp_point）。
    #[cfg(feature = "macos")]
    let local_rects: Vec<crate::edge::ScreenRect> = edge
        .as_ref()
        .map(|sensor| sensor.rects().to_vec())
        .unwrap_or_default();
    // 受控回报探针的节流时刻（11-22；主控侧 1/s info，见 observe_controlled_reports）。
    let mut report_probe_logged: Option<Instant> = None;
    // 鼠标按键门控状态（11-22 第三轮，docs/03 §5）：`mouse_buttons` = 捕获流
    // 位图（Down 置位/Up 清位，macos Host 才有捕获源）；`mouse_buttons_down`
    // = 任意键按下（切入门控读数 + 状态机回切门控的 LocalCmd 镜像——变化沿
    // 才发，全态跟踪含 Paused）。跟踪位置 = input 抽头分支的 capture poll
    // 消费处（tap 全态透传：受控/未受控事件都路过）。非 macos 无捕获源恒
    // false——门控判定面与 host_edge_sensor 平台矩阵一致（边缘轮询本就不建）。
    #[cfg(feature = "macos")]
    let mut mouse_buttons: u8 = 0;
    #[cfg(feature = "macos")]
    let mut mouse_buttons_down = false;
    #[cfg(not(feature = "macos"))]
    let mouse_buttons_down = false;
    let mut wire_buf = Vec::new();
    let mut out_buf = Vec::new();

    // 就绪：携带握手摘要进入版本协商（host 侧产出 Hello，client 侧无出站）。
    // 预备段与循环内同一错误纪律：显式走「停传输 + 断开投影 + 上抛」，
    // 禁止 `?` 早退——早退会跳过收尾，UI 快照冻结在旧状态（见下方循环注释）。
    let ready_actions = {
        let mut dispatcher = Dispatcher::new(&mut session);
        match dispatcher
            .on_handshake_ready(transport.handshake_hash())
            .map_err(|error| state_error("HandshakeReady 事件被会话状态机拒绝", error))
        {
            Ok(actions) => actions,
            Err(error) => {
                transport.shutdown().await;
                report_session_error(&bus, &error);
                return Err(error);
            }
        }
    };
    if let Err(error) = apply_and_send(
        &mut transport,
        &mut sink,
        &ready_actions,
        &mut out_buf,
        &bus,
    )
    .await
    {
        transport.shutdown().await;
        report_session_error(&bus, &error);
        return Err(error);
    }

    // 状态跃迁 + 对端名在协商帧往返后经 Notice 更新（下方循环内处理）。
    let mut ticker = tokio::time::interval(TIMER_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 边缘轮询节拍（FR-2.1：传感器自带可配间隔，默认 10ms——11-24 起由 50ms
    // 提速，最坏切换延迟约 50–60ms → 约 10ms；无传感器时 ticker 空转——轮询
    // 分支在 poll_edge_sensor 内永挂起）。
    let mut edge_ticker = tokio::time::interval(
        edge.as_ref()
            .map_or(DEFAULT_POLL_INTERVAL, EdgeSensor::poll_interval),
    );
    edge_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 受控光标回报节拍（双向边缘回切，11-21 反馈 5；docs/03 §5）：与主控边缘
    // 轮询共用 DEFAULT_POLL_INTERVAL（10ms，11-24）；分支条件含采样面存在性——
    // 零唤醒。11-22 修复轮：
    // mac = rects 快照在位（路径不变）；非 macos = sink 查询恒在位（无实现的
    // 后端 cursor_normalized 返回默认 None，查询零动作但节拍照走——uinput/
    // X11 有实现）。
    let mut report_ticker = tokio::time::interval(DEFAULT_POLL_INTERVAL);
    report_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    #[cfg(feature = "macos")]
    let controlled_sampling = role == Role::Client && controlled_rects.is_some();
    #[cfg(not(feature = "macos"))]
    let controlled_sampling = role == Role::Client;

    // 会话输入抽头节拍（FR-2.5/2.x：捕获流 poll + 平台热键源 drain，10ms/100Hz；
    // 全部非阻塞——poll/drain 无事件即返回，无 I/O 等待）。
    let mut input_ticker = tokio::time::interval(crate::hotkey_rt::INPUT_POLL_INTERVAL);
    input_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 控制关系变化沿（11-24，切入防抖的输入，docs/03 §5 死区）：每轮循环顶部
    // 比对 `session.control_dir()` 与上一轮的值，变化即记 `Instant::now()`。
    // 放在循环顶部而非各分支内——select 的任一分支（收帧/边缘/热键/托盘/命令）
    // 引起的变化，以及分支内 `continue` 跳到下一轮的情形，都会在下一轮顶部被
    // 记到；边缘分支读到的恒是最新值。None = 尚无变化沿（空闲常态，不防抖）。
    let mut last_control_dir = session.control_dir();
    let mut control_changed_at: Option<Instant> = None;

    // 循环内错误一律 `break Err` 收敛到循环尾部（shutdown + 断开投影上报），
    // 禁止 `?` 早退——早退会跳过传输收尾与 Disconnected 投影，UI 将冻结在
    // 旧状态（如 TCP 写坏后仍显示 Running）。
    let result = loop {
        // —— 循环顶部：控制关系变化沿（见上）——同步快路径，无 await ——
        let control_dir_now = session.control_dir();
        if control_dir_now != last_control_dir {
            last_control_dir = control_dir_now;
            control_changed_at = Some(Instant::now());
        }
        tokio::select! {
            // 收帧：解密 → 喂 Dispatcher（可含多帧）。
            received = transport.recv(&mut wire_buf) => match received {
                Ok(()) => {
                    bus.report(|state| state.packets_in += 1);
                    // 受控光标回报探针（11-22 修复轮验证面）：主控在控时收到的
                    // MouseMoveAbs 反向回报在 core 层被消化为回切判定输入（无
                    // 注入、无动作产出）——app 层扫帧观察、1/s 节流 info 打印
                    // 归一化值，"回报到达但回切不触发"与"回报根本没来"的实测
                    // 分辨入口。
                    if session.control_dir() == ControlDir::LocalControlsPeer {
                        observe_controlled_reports(&wire_buf, &mut report_probe_logged);
                    }
                    let outcome = {
                        // 剪贴板帧防御剥离（11-21 双连接裁定）：clip 帧（0x50–0x54）
                        // 不应再出现在控制连接上（正常流量走独立剪贴板通道，
                        // crate::clip_channel 泵处理）——出现即对端旧版本/残留，
                        // 在喂 dispatcher 前剥出丢弃告警（会话状态机把 Clip 判
                        // 违规断开；控制连接存活优先，不因防御面断连）。
                        let (rest_stream, clips) = crate::clip_rt::split_clip_frames(&wire_buf);
                        if !clips.is_empty() {
                            tracing::warn!(target: crate::clip_rt::TARGET, count = clips.len(),
                                "控制连接收到剪贴板帧（双连接裁定下不应出现；剥出丢弃，不断连）");
                        }
                        // 文件域帧防御剥离（FR-7.3 域隔离，docs/02 §4.6）：文件帧
                        // 正常流量走独立文件通道——控制连接上出现即误路由/残留，
                        // 剥出丢弃告警，不断连（同剪贴板帧防御面语义）。
                        let (rest_stream, file_kinds) =
                            crate::file_channel::split_file_frames(&rest_stream);
                        if !file_kinds.is_empty() {
                            tracing::warn!(target: crate::file_channel::TARGET, count = file_kinds.len(),
                                "控制连接收到文件域帧（域隔离下不应出现；剥出丢弃，不断连）");
                        }
                        let mut dispatcher = Dispatcher::new(&mut session);
                        dispatcher.feed(&rest_stream)
                    };
                    if let Some(fatal) = &outcome.fatal {
                        break Err(anyhow::anyhow!("协议违规断开：{fatal}"));
                    }
                    if let Err(error) = apply_and_send(
                        &mut transport,
                        &mut sink,
                        &outcome.actions,
                        &mut out_buf,
                        &bus,
                    )
                    .await
                    {
                        break Err(error);
                    }
                    // 位置模型校准（11-24，R2）：保持原位切入（`preserve_cursor=1`，
                    // 无落点 warp）后，位置模型为虚拟的后端（uinput）把真实光标
                    // 定位回其虚拟位置——否则回切判定基于一个与真实光标无关的
                    // 虚构位置。上方动作已应用完毕（`SwitchRequest` 的 ReleaseAll
                    // 在前一帧、EnterScreen 在本帧，分两次循环迭代），校准时序
                    // 正确。位置模型即真实光标的后端默认无操作；失败只告警，不
                    // 打断会话（error-handling.md 规则 4）。
                    if is_preserved_entry(&outcome.actions)
                        && let Err(error) = sink.resync_cursor()
                    {
                        tracing::warn!(target: "session::rt", %error,
                            "保持原位切入后的光标校准失败（位置模型可能与真实光标不一致，回切判定或受影响）");
                    }
                    // 释放落点缓存（11-22）：受控态产出的 WarpPointer（收帧
                    // 路径上只有边缘回切——热键回主控的落点在热键分支剥离并
                    // 改写为切入锚点，11-24）换算成全局点缓存，suppress=false
                    // 拍在重关联后重放（重关联会覆盖落点，见
                    // pending_release_warp）。
                    //
                    // 回切行程补偿（11-24，R8；docs/03 §5 回切第 3 条）：三段位移在
                    // 回切时本来会丢失，全部加到落点上——① 工作机顶边期间被屏幕
                    // 边界截掉的推进量（随回报带回，`Notice::EdgeReturnedToHost`）；
                    // ② 最后转发的一批若在工作机采样之后才到，没算进触发回切的
                    // 那条回报（在途批）；③ 触发后到本地输入恢复之间被本地拦截的
                    // 位移（下方排空的尾批）。落点夹回屏内（四周留
                    // `EDGE_RETURN_INSET`）。
                    //
                    // 排空后立即翻拍：关闭本地输入拦截不再等下一个输入拍（≤10ms）
                    // ——先把捕获队列里翻拍前被本地拦截的事件排空（位移计入③；
                    // 事件仍过按键位图与热键过滤，只是不转发），随即翻拍并在
                    // 重关联之后重放贴缘基点，补偿量由回放队列在后续输入拍
                    // 逐拍补入（平滑回放）。翻拍之后的新事件原生送达本机。
                    #[cfg(feature = "macos")]
                    if last_suppress
                        && let Some(landing) = edge_return_of(&outcome.actions)
                    {
                        let in_flight = in_flight_batch(last_sent_batch.take(), Instant::now());
                        let mut drained_sum = (0.0, 0.0);
                        if let Some(capture) = capture.as_mut() {
                            use xcontrol_core::sink::InputSource as _;
                            let mut drained = Vec::new();
                            capture.poll(&mut drained);
                            drained_sum = sum_mouse_deltas(&drained);
                            absorb_unforwarded_events(
                                &drained,
                                &mut capture_filter,
                                &hotkey_tx,
                                &mut mouse_buttons,
                                &mut mouse_buttons_down,
                                &mut session,
                            );
                        }
                        let travel = (
                            landing.report_travel.0 + in_flight.0 + drained_sum.0,
                            landing.report_travel.1 + in_flight.1 + drained_sum.1,
                        );
                        // 平滑回放（11-25 第二轮，R11）：落点取**未补偿**的贴缘基点
                        // （翻拍后重放的就是它），补偿量（夹取后的终点 − 基点）切成
                        // 2ms 微步，交给专用线程高频回放——不等输入拍（10ms 一拍
                        // 的"6 大步"实测仍不顺滑：每步 20 点依然是跳）。失败只告警
                        // ——回放是观感优化，不打断输入路径。
                        let base_release = crate::edge::release_warp_point(
                            &local_rects,
                            landing.screen,
                            landing.x,
                            landing.y,
                        );
                        pending_release_warp = base_release;
                        cursor_replay = replay_slices(
                            crate::edge::compensated_release_point(
                                &local_rects,
                                landing.screen,
                                landing.x,
                                landing.y,
                                travel,
                            ),
                            base_release,
                        );
                        tracing::info!(target: "app::edge",
                            report = ?landing.report_travel,
                            in_flight = ?in_flight,
                            drained = ?drained_sum,
                            landing = ?pending_release_warp,
                            replay_steps = cursor_replay.len(),
                            replay_total = ?cursor_replay
                                .iter()
                                .fold((0.0, 0.0), |(x, y), &(dx, dy)| (x + dx, y + dy)),
                            "边缘回切落点补偿（回报行程 + 在途批 + 排空批，单位=点；补偿量高频微步回放）");
                        if let Some(capture) = capture.as_ref() {
                            // 控制关系已终结 → want = false：翻拍并以补偿落点重放。
                            // 正常路径下 control_dir 已是 None；万一同一批动作里
                            // 又建立了新的控制关系，这里按其现值对账（不翻拍）。
                            reconcile_local_input_suppress(
                                capture,
                                &mut pending_release_warp,
                                &mut last_suppress,
                                session.control_dir() == ControlDir::LocalControlsPeer,
                            );
                        }
                        if last_suppress {
                            // 未翻拍（仍在受控）：本次的释放落点作废，不留给下一次
                            // 释放重放；回放队列同理清空。
                            pending_release_warp = None;
                            cursor_replay.clear();
                        } else {
                            // 已翻拍（正常路径）：队列交给专用线程高频回放。此后的
                            // 重新切换、热键切走或会话结束都让光标回到受控锚定 /
                            // 原生控制，无需此队列（当前设计的收尾路径不残留旧
                            // 队列——切换前先回主控，回放早已完成）。
                            spawn_cursor_replay(std::mem::take(&mut cursor_replay));
                        }
                    }
                    consume_notices(&session, &outcome.actions, &bus);
                    // M5 布局协商接线（docs/02 §4.2）：布局类通知只随帧到达——
                    // Host 协商完成 → 装配 + 下发（出站动作立即发送）；Client 收
                    // Layout → 安装 ScreenId → 本机屏映射表。
                    let (followups, targets) = {
                        let cfg = locked_layout(config);
                        crate::layout_rt::handle_layout_notices(
                            &mut session,
                            &outcome.actions,
                            &identity_for_layout,
                            local_layout.as_ref(),
                            &cfg,
                        )
                    };
                    if !targets.is_empty() {
                        slave_targets = targets;
                    }
                    if !followups.is_empty()
                        && let Err(error) = apply_and_send(
                            &mut transport,
                            &mut sink,
                            &followups,
                            &mut out_buf,
                            &bus,
                        )
                        .await
                    {
                        break Err(error);
                    }
                }
                Err(error) => {
                    // net 层错误（流错位/解密失败）：快照投影由循环尾部
                    // report_session_error 承载，tracing 留痕由调用方循环尾部
                    // warn 承载（主控 accept 循环 / 从机两条客户端循环）——
                    // 逐帧【观测】dump 已移除（11-21 实测反馈）。
                    break Err(disconnect_error("收帧失败", &error));
                }
            },

            // 心跳判定（会话内部按 SystemClock 判定 ping/判死）。
            _ = ticker.tick() => {
                let actions = {
                    let mut dispatcher = Dispatcher::new(&mut session);
                    match dispatcher.on_timer() {
                        Ok(actions) => actions,
                        Err(error) => break Err(anyhow::anyhow!("Timer 事件被拒：{error}")),
                    }
                };
                if let Err(error) =
                    apply_and_send(&mut transport, &mut sink, &actions, &mut out_buf, &bus).await
                {
                    break Err(error);
                }
                consume_notices(&session, &actions, &bus);
            },

            // UI / 托盘命令。
            command = rx_cmd.recv() => match command {
                Some(CoreCommand::Pause) | Some(CoreCommand::Resume) => {
                    let paused = matches!(command, Some(CoreCommand::Pause));
                    let actions = {
                        let mut dispatcher = Dispatcher::new(&mut session);
                        match dispatcher.on_local(if paused { LocalCmd::Pause } else { LocalCmd::Resume }) {
                            Ok(actions) => actions,
                            Err(error) => break Err(anyhow::anyhow!("本地命令被拒：{error}")),
                        }
                    };
                    if let Err(error) =
                        apply_and_send(&mut transport, &mut sink, &actions, &mut out_buf, &bus)
                            .await
                    {
                        break Err(error);
                    }
                    bus.report(|state| {
                        if matches!(state.conn, ConnState::Running { .. }) {
                            state.conn = ConnState::Running { paused };
                        }
                    });
                    bus.flush();
                }
                Some(CoreCommand::Disconnect) | Some(CoreCommand::QuitApp) => {
                    let actions = {
                        let mut dispatcher = Dispatcher::new(&mut session);
                        match dispatcher.on_local(LocalCmd::Quit) {
                            Ok(actions) => actions,
                            Err(error) => break Err(anyhow::anyhow!("本地命令被拒：{error}")),
                        }
                    };
                    if let Err(error) =
                        apply_and_send(&mut transport, &mut sink, &actions, &mut out_buf, &bus)
                            .await
                    {
                        break Err(error);
                    }
                    consume_notices(&session, &actions, &bus);
                }
                // 运行时配置更新（11-18-m4-ui，docs/06 §4.1）：会话内到达 → 写
                // 共享单元 + 日志级别立即；会话不中断。
                // 11-21 实测反馈（反馈 3/4）：热键/映射**即时热更新**——编译新表
                // 原位替换（热键匹配表/映射规则，被动捕获架构无 OS 重注册；win/
                // x11 平台注册源重建），失败保留旧表留痕。
                // M5 例外（docs/06 §4.1 layout 行）：`[layout]` 段 Host 会话内
                // 即时重装配 + 重新下发（协商完成后才可执行；未协商时空动作，
                // 握手时按会话快照装配）。
                Some(CoreCommand::RuntimeConfigUpdate(new_config)) => {
                    let delta = store_runtime_config(config, &new_config);
                    if delta.code_ref_changed {
                        // 换码协议（docs/07 §3.3）：会话内改码 → 本会话立即断开
                        //（对端收 Quit 信令；supervisor 收 ConfigReload 后按新码
                        // 重派生 PSK 重建运行时——旧会话一律不延续）。
                        tracing::warn!(target: "session::rt",
                            "会话内安全码变更（docs/07 §3.3 换码协议）：断开当前会话，运行时按新码重建");
                        let actions = run_local(&mut session, LocalCmd::Quit);
                        if let Err(error) = apply_and_send(
                            &mut transport,
                            &mut sink,
                            &actions,
                            &mut out_buf,
                            &bus,
                        )
                        .await
                        {
                            tracing::warn!(target: "session::rt", %error,
                                "换码断开的 Quit 发送失败（对端将按心跳超时收尾；本端继续换码收尾）");
                        }
                        consume_notices(&session, &actions, &bus);
                        break Ok(SessionEnd::ConfigReload);
                    }
                    // 热键/映射即时生效（11-21 反馈 3/4，docs/06 §4.1）：编译新
                    // 配置并原位替换运行中的匹配表/规则（详见 apply_hot_key_configs）。
                    apply_hot_key_configs(
                        &new_config,
                        &mut sink,
                        #[cfg(feature = "macos")]
                        &mut capture_filter,
                        &mut hotkey_source,
                    );
                    let (followups, targets) = crate::layout_rt::rebuild_and_publish(
                        &mut session,
                        &identity_for_layout,
                        local_layout.as_ref(),
                        &locked_layout(config),
                    );
                    if !targets.is_empty() {
                        slave_targets = targets;
                    }
                    if !followups.is_empty()
                        && let Err(error) = apply_and_send(
                            &mut transport,
                            &mut sink,
                            &followups,
                            &mut out_buf,
                            &bus,
                        )
                        .await
                    {
                        break Err(error);
                    }
                }
                // 托盘「切换角色」（FR-1.1）：优雅断开当前会话并上抛 RoleSwitch
                // ——accept/重连/发现循环各自让位，main.rs supervisor 翻转角色重建。
                Some(CoreCommand::ConfirmHost { instance_id }) => {
                    // 候选确认（FR-5.5）在会话内到达：会话已建立，确认语义属
                    // 发现循环——留痕忽略；持久化效果经 RuntimeConfigUpdate
                    // 的共享单元传播（下一轮发现循环读最新 confirmed_host）。
                    tracing::debug!(target: "session::rt", instance_id = %instance_id,
                        "会话内收到候选确认（FR-5.5）：会话运行中，留痕忽略（发现循环/配置路径承接）");
                }
                Some(CoreCommand::SwitchRole) => {
                    let actions = {
                        let mut dispatcher = Dispatcher::new(&mut session);
                        match dispatcher.on_local(LocalCmd::Quit) {
                            Ok(actions) => actions,
                            // 会话已断开（被拒）时无出站动作——切换继续（目标态
                            // 本就是"当前会话不存在"）。
                            Err(error) => {
                                tracing::warn!(target: "session::rt", %error,
                                    "角色切换的 Quit 被状态机拒绝（会话已断开？），继续切换");
                                Vec::new()
                            }
                        }
                    };
                    if let Err(error) =
                        apply_and_send(&mut transport, &mut sink, &actions, &mut out_buf, &bus)
                            .await
                    {
                        tracing::warn!(target: "session::rt", %error,
                            "角色切换的断开发送失败（对端将按超时收尾；本端继续切换）");
                    }
                    break Ok(SessionEnd::RoleSwitch);
                }
                // 托盘在线从机子菜单「直接切换」（FR-2.5）：与热键 Next 的 Switch
                // 动作同门控语义（hotkey_rt::switch_allowed）。target = 在线从机序
                // 下标（托盘 M4 拓扑投影），经布局装配的全局 id 解析（docs/02 §4.2；
                // 越界回退原值 = 1.0 语义兼容）。
                Some(CoreCommand::SwitchToSlave { target }) => {
                    let actions = if crate::hotkey_rt::switch_allowed(
                        session.state(),
                        session.control_dir(),
                    ) {
                        let target =
                            crate::layout_rt::resolve_slave_target(&slave_targets, target);
                        tracing::info!(target: "app::hotkey", target,
                            "托盘直接切换（SwitchRequest → Ack → EnterScreen 接管闭环，docs/03 §5）");
                        run_local(&mut session, LocalCmd::Switch { target })
                    } else {
                        tracing::info!(target: "app::hotkey", target,
                            "托盘直接切换被门控拒绝（未运行/共享暂停/被控中）");
                        Vec::new()
                    };
                    if let Err(error) =
                        apply_and_send(&mut transport, &mut sink, &actions, &mut out_buf, &bus)
                            .await
                    {
                        break Err(error);
                    }
                    consume_notices(&session, &actions, &bus);
                }
                None => {
                    // 命令通道关闭（UI 侧已退出）：尽力优雅断开。Quit 被状态机
                    // 拒绝（会话已断开）则动作留空并记日志，不吞错。
                    let actions = {
                        let mut dispatcher = Dispatcher::new(&mut session);
                        match dispatcher.on_local(LocalCmd::Quit) {
                            Ok(actions) => actions,
                            Err(error) => {
                                tracing::warn!(target: "session::rt", %error, "Quit 被状态机拒绝（会话已断开？），跳过出站");
                                Vec::new()
                            }
                        }
                    };
                    if let Err(error) =
                        apply_and_send(&mut transport, &mut sink, &actions, &mut out_buf, &bus)
                            .await
                    {
                        break Err(error);
                    }
                    break Err(anyhow::anyhow!("命令通道关闭，会话退出"));
                }
            },

            // 边缘轮询（FR-2.1/2.2）：读光标 → 判定（含去抖）→ ControlDir/暂停
            // 门控 → 控制关系变化后 150ms 防抖（11-24）→ LocalCmd::EdgeCrossing。
            // 无传感器（非 macos/无屏/从机）时该分支永挂起（poll_edge_sensor 内
            // pending），零唤醒开销。
            hit = poll_edge_sensor(edge.as_mut(), &mut edge_ticker) => {
                if let Some(hit) = hit {
                    // 本机物理屏间移动 ≠ 切换（11-20 策略分层）：传感器机制层
                    // 返回一切邻居命中，本机屏（id < 本机屏数）的命中在此忽略
                    // ——正常多屏行为，只有外接从机屏（id ≥ 本机屏数）触发切换。
                    if usize::from(hit.target) < edge.as_ref().map_or(0, |sensor| sensor.local_screen_count()) {
                        continue;
                    }
                    // 判定顺序（11-24）：本机屏过滤 → 门控 → 防抖 → EdgeCrossing；
                    // 两类拒绝都走 debug 留痕（被拒命中不重发：传感器已闩锁，须回
                    // 内侧再推）。
                    if !edge_hit_allowed(session.state(), session.control_dir(), mouse_buttons_down) {
                        // 门控拒绝属常态（暂停/受控中贴边/鼠标键按下）：debug——
                        // 状态页/托盘已表达暂停态，info 级会在悬停边缘时刷屏
                        //（11-21 实测反馈）；按键门控 11-22 第三轮（拖拽中不切换）；
                        // 受控中一律拒绝 11-24（主控光标被锚定，受控期命中无有意
                        // 滑边——见 edge_hit_allowed 文档）。
                        tracing::debug!(target: "app::edge", target = hit.target,
                            edge = ?hit.edge, control_dir = ?session.control_dir(), mouse_buttons_down,
                            "触边被门控忽略（未运行/共享暂停/受控中/鼠标键按下）");
                    } else if !edge_dead_time_elapsed(control_changed_at, Instant::now()) {
                        // 防抖窗内（控制关系变化后不足 150ms）：防回切落点贴缘时
                        // 快速甩动造成的回弹（docs/03 §5 死区）。
                        tracing::debug!(target: "app::edge", target = hit.target,
                            edge = ?hit.edge, "触边被防抖忽略（控制关系变化后 150ms 内）");
                    } else {
                        tracing::info!(target: "app::edge", target = hit.target,
                            edge = ?hit.edge, ratio = hit.ratio, "光标触边：上报边缘切换");
                        let actions = {
                            let mut dispatcher = Dispatcher::new(&mut session);
                            // 门控已保证 Running 才进入；被拒属防御分支：记日志
                            // 跳过不断连（error-handling.md 规则 4，不打断实时路径）。
                            match dispatcher.on_local(LocalCmd::EdgeCrossing {
                                target: hit.target,
                                edge: hit.edge,
                                ratio: hit.ratio,
                            }) {
                                Ok(actions) => actions,
                                Err(error) => {
                                    tracing::warn!(target: "app::edge", %error,
                                        "EdgeCrossing 被状态机拒绝（跳过）");
                                    Vec::new()
                                }
                            }
                        };
                        if let Err(error) = apply_and_send(
                            &mut transport,
                            &mut sink,
                            &actions,
                            &mut out_buf,
                            &bus,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                }
            },

            // 受控光标回报（双向边缘回切，11-21 反馈 5；docs/03 §5）：被控方按
            // 10ms 节拍（11-24 起，原 50ms）采样本机光标 → 归一化 → MouseMoveAbs
            // 出站回报——判定
            // （外侧边极值 + 同向武装）与回切决策全在主控状态机。采样双路径
            // （11-22 修复轮）：mac 走 rects 快照 + CG 查询（现行路径不动），
            // Linux/其他平台走注入 sink 查询（cursor_normalized——此前 Linux
            // 被控从不回报、主控回切等不到数据的根因）。未受控/采样失败 →
            // 本拍跳过（回报是纯观测面，任何失败不打断会话，error-handling.md
            // 规则 4）。
            _ = report_ticker.tick(), if controlled_sampling => {
                if session.control_dir() == ControlDir::PeerControlsLocal {
                    #[cfg(feature = "macos")]
                    let sampled = sample_controlled_cursor(&session, controlled_rects.as_deref());
                    // 非 macos：sink 链只读查询（无实现的实现返回默认 None →
                    // 本拍跳过——回报分支零动作）。
                    #[cfg(not(feature = "macos"))]
                    let sampled = sink.cursor_normalized();
                    if let Some((x, y)) = sampled {
                        let actions =
                            run_local(&mut session, LocalCmd::ReportControlledCursor { x, y });
                        if !actions.is_empty()
                            && let Err(error) = apply_and_send(
                                &mut transport,
                                &mut sink,
                                &actions,
                                &mut out_buf,
                                &bus,
                            )
                            .await
                        {
                            break Err(error);
                        }
                    }
                }
            },

            // 会话输入抽头（FR-2.5/2.x）：平台热键源 drain + mac 主控捕获流
            // 热键识别/转发。全非阻塞（poll/drain 无事件即返回）。
            _ = input_ticker.tick() => {
                // 捕获源自动恢复（11-24，R9；docs/05 §1.3）：macOS 主控的捕获源只在
                // 会话装配时创建一次，未授权辅助功能时创建失败（热键识别与捕获
                // 转发停用）——此前授权后必须重启应用或重建会话。此处每
                // `CAPTURE_RETRY_INTERVAL` 静默查询一次授权状态（`prompt=false`，
                // 不弹系统框），已授权即重建捕获源，后续输入拍照常消费。仅 Host
                // （从机不建捕获源）且 `capture` 缺席时探测。
                //
                // 恢复前先解除名义上的受控态（11-25）：未授权期间没有捕获源，但
                // 边缘传感器不需要权限——主控仍可靠它切入（`LocalControlsPeer`，
                // 本地输入照常、只是不转发）。捕获源一恢复，本拍随后的对账就会
                // 打开本地输入拦截并把键鼠转发给工作机，而用户并没有在此刻发起
                // 切换。所以先经 `ReleaseControl` 释放（`ReleaseAll` 与
                // `LeaveScreen` 照常下发，落点 warp 剥离、光标不动），再重建
                // （纯决策 `capture_recovery_step`：共享暂停中推迟到恢复之后）。
                //
                // 重建失败的告警退避（11-25）：已授权但启动持续失败时每秒重试
                // 一次，告警首次立即、之后至多 `CAPTURE_FAILURE_LOG_INTERVAL` 一条
                // （纯函数 `capture_failure_log_due`）；恢复成功 `info!` 一次。
                #[cfg(feature = "macos")]
                if role == Role::Host
                    && capture.is_none()
                    && capture_probe_due(capture_probe_at, Instant::now())
                {
                    capture_probe_at = Some(Instant::now());
                    // 未授权：静默返回（每秒一条"仍未授权"只是噪音，启动引导已
                    // 留痕）。已授权才往下走。
                    if xcontrol_input::check_accessibility(false).is_granted() {
                        match capture_recovery_step(session.control_dir(), session.is_paused()) {
                            // 共享暂停中且名义上受控：对端此刻拒收全部输入帧
                            // （LeaveScreen 会被丢弃，控制关系在两端失配）——本拍
                            // 不重建，恢复之后的下个探测间隔再走"释放 + 重建"。
                            // 暂停期间捕获源缺席，本就什么都不转发。
                            CaptureRecoveryStep::DeferWhilePaused => {
                                tracing::debug!(target: "session::rt",
                                    "辅助功能已授权，但共享暂停中且名义上处于受控态：推迟到恢复后释放控制关系并重建捕获源");
                            }
                            step => {
                                if step == CaptureRecoveryStep::ReleaseThenRebuild {
                                    let actions =
                                        release_control_before_capture_recovery(&mut session);
                                    if !actions.is_empty() {
                                        if let Err(error) = apply_and_send(
                                            &mut transport,
                                            &mut sink,
                                            &actions,
                                            &mut out_buf,
                                            &bus,
                                        )
                                        .await
                                        {
                                            break Err(error);
                                        }
                                        consume_notices(&session, &actions, &bus);
                                        tracing::info!(target: "session::rt",
                                            "授权前已切入：已先释放控制关系，请重新切换");
                                    }
                                }
                                rebuild_capture_source(&mut capture, &mut capture_failure_logged_at);
                            }
                        }
                    }
                }

                // 平台注册源（win/x11）→ 路由通道。
                let mut hits = Vec::new();
                hotkey_source.drain(&mut hits);
                for id in hits {
                    tracing::info!(target: "app::hotkey", hotkey = id.label(),
                        source = "platform", "热键命中（OS 注册路径）");
                    let _ = hotkey_tx.send(id);
                }

                // mac 主控捕获流（FR-2.5 捕获层识别 + FR-2.x 转发链）。
                #[cfg(feature = "macos")]
                if let Some(capture) = capture.as_mut() {
                    use xcontrol_core::sink::InputSource as _;
                    // 修饰键快照对齐（EnterScreen/LeaveScreen/ModifierSync 载荷
                    // 来源；仅变化时上报，Idle 状态恒被宽容忽略——转移表 Ignore）。
                    let mods_now = capture.mods();
                    if mods_now != last_local_mods {
                        last_local_mods = mods_now;
                        let mut dispatcher = Dispatcher::new(&mut session);
                        if let Err(error) = dispatcher.on_local(LocalCmd::LocalMods(mods_now)) {
                            tracing::warn!(target: "app::hotkey", %error, "LocalMods 被状态机拒绝（跳过）");
                        }
                    }
                    // 捕获 poll → 热键 filter → 命中路由 / 未命中转发（门控）。
                    let mut events = Vec::new();
                    capture.poll(&mut events);
                    // 鼠标按键位图跟踪（11-22 第三轮按键门控，docs/03 §5）：全部
                    // 事件先过位图（热键命中/丢弃的分支不影响物理按键观测——
                    // Down 置位/Up 清位，受控与未受控同一位图），任意键按下即
                    // 门控切入（edge 分支读 `mouse_buttons_down`）与回切
                    // （`LocalCmd::MouseButtons` 镜像进状态机，变化沿才发；
                    // Paused 照常更新——恢复后状态正确）。
                    for event in &events {
                        track_mouse_buttons(&mut mouse_buttons, event);
                    }
                    let now_mouse_down = mouse_buttons != 0;
                    if now_mouse_down != mouse_buttons_down {
                        mouse_buttons_down = now_mouse_down;
                        let mut dispatcher = Dispatcher::new(&mut session);
                        if let Err(error) =
                            dispatcher.on_local(LocalCmd::MouseButtons { any_down: now_mouse_down })
                        {
                            tracing::warn!(target: "session::rt", %error,
                                "MouseButtons 被状态机拒绝（跳过）");
                        }
                    }
                    let forward_ok = crate::hotkey_rt::capture_forward_allowed(
                        session.state(),
                        session.control_dir(),
                    );
                    // 受控吞本地输入（KVM 重定向语义，docs/06 §2）：ControlDir 变化
                    // 即同步 tap 的 suppress 开关（受控 Drop 本地执行；已转换仍转发）。
                    // 翻拍与释放落点重放的判定抽在 reconcile_local_input_suppress
                    // （11-24：边缘回切在收帧分支排空后立即翻拍，与本拍共用）。
                    let want_suppress =
                        session.control_dir() == ControlDir::LocalControlsPeer;
                    reconcile_local_input_suppress(
                        capture,
                        &mut pending_release_warp,
                        &mut last_suppress,
                        want_suppress,
                    );
                    // 回切行程回放（11-25，R11）：队列非空时每拍相对 warp 一步。
                    // 放在对账之后——已翻到受控（want_suppress）即停止回放：光标
                    // 已交还给受控锚定，回放不得再推动它。
                    // 回切行程回放（11-25 第二轮）：回切触发处已把队列交给专用
                    // 线程高频回放（spawn_cursor_replay），输入拍不再参与——
                    // 10ms 节拍的"6 大步"实测仍不顺滑。
                    // 受控光标维护（11-20 切入锚定；11-22 撞缘回锚**删除**）：
                    // 每拍 warp 回切入锚点——mac 光标冻结在切入处（macOS 指针
                    // 合成在 HID 层，Session tap 吞事件防不住光标移动；锚定
                    // warp 是"光标冻结"唯一可达实现——11-22 对照实验裁定）。
                    // 回锚的前提（受控 delta 随系统钳位断流）经实验证伪——
                    // 解关联后 HID 事件 delta 为硬件增量，无钳位无断供，回锚
                    // 与其可见跳变一并删除。物理位移全部转发从机。
                    if want_suppress {
                        capture.pin_cursor_to_anchor();
                    }
                    // 整拍合帧：批内所有出站事件一次 send（docs/02 §4.1 合帧精神，
                    // 防 10ms ticker 逐事件一包）。
                    let mut outbound: Vec<SessionAction> = Vec::new();
                    {
                        let mut dispatcher = Dispatcher::new(&mut session);
                        for event in events {
                            match capture_filter.feed(&event) {
                                // 命中：不转发（PRD 裁定的 mac "不转发"语义——
                                // ListenOnly 无法吞事件，app 层不产出 InputEvent）。
                                FilterOutcome::Hit(id) => {
                                    tracing::info!(target: "app::hotkey", hotkey = id.label(),
                                        source = "capture", "热键命中（捕获层识别，事件不转发）");
                                    let _ = hotkey_tx.send(id);
                                }
                                // 命中键尾巴：吞。
                                FilterOutcome::HitTail => {}
                                // 未命中：门控放行 → 转发；门控拒绝（无人在控/
                                // 暂停）→ 丢弃（本机打字不外发）。
                                FilterOutcome::Forward => {
                                    if forward_ok {
                                        match dispatcher.on_local(LocalCmd::ForwardInput(event)) {
                                            Ok(actions) => outbound.extend(actions),
                                            Err(error) => tracing::warn!(
                                                target: "session::rt", %error,
                                                "ForwardInput 被状态机拒绝（跳过该事件）"),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // 在途批（11-24 回切行程补偿）：本批实际发出的 `MouseMoveRel`
                    // 之和——发出成功后连同发出时刻记下（下方 await 语句原样保留，
                    // 失败即 break，记录只发生在成功之后）。
                    let batch_move_sum = sum_mouse_deltas(outbound.iter().filter_map(|action| {
                        match action {
                            SessionAction::Send {
                                msg: xcontrol_core::session::Outbound::Input(event),
                                ..
                            } => Some(event),
                            _ => None,
                        }
                    }));
                    if !outbound.is_empty()
                        && let Err(error) = apply_and_send(
                            &mut transport,
                            &mut sink,
                            &outbound,
                            &mut out_buf,
                            &bus,
                        )
                        .await
                    {
                        break Err(error);
                    }
                    if !outbound.is_empty() {
                        last_sent_batch = Some((Instant::now(), batch_move_sum));
                    }
                }
            },

            // 会话期间新连接分派（host，docs/02 §0/§4.4 双连接）：剪贴板通道
            // 连接（首帧 ChannelBind）在此接入并 spawn 泵任务——剪贴板流量从此
            // 与控制连接完全隔离（队头阻塞根因修复）；控制连接（重连风暴残余/
            // 第二从机）窥探后暂存，本会话结束后由 accept 循环优先驱动（串行
            // accept 语义保留）。暂存就绪后停止轮询（多余连接留在交付通道缓冲）。
            // 分派期间的窥探窗口（300ms）只在新连接到达时发生，输入热路径无感。
            connection = poll_incoming(incoming.as_deref_mut()),
                if incoming.is_some() && pending_control.is_none() =>
            {
                let Some(connection) = connection else {
                    // Server 已停止（进程收尾路径）：本会话照常运行至自然断开。
                    continue;
                };
                match crate::clip_channel::classify_incoming(
                    connection,
                    session_id,
                    clip_max_bytes,
                    crate::clip_channel::CLIP_BIND_PEEK_WINDOW,
                )
                .await
                {
                    crate::clip_channel::IncomingClass::ClipChannel(guard) => {
                        // 新通道优先：关闭旧通道（绑定唯一，docs/02 §4.4）。
                        if let Some(old) = clip_channel.replace(guard) {
                            old.close();
                        }
                    }
                    crate::clip_channel::IncomingClass::FileChannel(guard) => {
                        // 新文件通道优先：关闭旧通道（绑定唯一，docs/02 §4.6）。
                        if let Some(old) = file_channel.replace(guard) {
                            old.close();
                        }
                    }
                    crate::clip_channel::IncomingClass::Control(conn) => {
                        tracing::info!(target: "session::rt", peer = %conn.peer,
                            "会话期间到达控制连接（窥探无首帧；暂存，本会话结束后驱动）");
                        pending_control = Some(*conn);
                    }
                    crate::clip_channel::IncomingClass::Rejected => {}
                }
            },

            // 热键路由（FR-2.5）：命中 → 角色语义 → 会话动作。主控切换目标 =
            // 布局装配的从机屏全局 id 序（M5，docs/02 §4.2；协商完成后非空——
            // 默认拓扑保证至少一条，空表 = 未协商，门控即拒绝）。
            Some(id) = hotkey_rx.recv() => {
                let action = match role {
                    Role::Host => crate::hotkey_rt::route_host(
                        id,
                        session.state(),
                        session.control_dir(),
                        &slave_targets,
                    ),
                    Role::Client => crate::hotkey_rt::route_slave(id, session.state()),
                };
                #[cfg_attr(not(feature = "macos"), allow(unused_mut))]
                let mut actions = match action {
                    HotkeyAction::Switch { target } => {
                        tracing::info!(target: "app::hotkey", hotkey = id.label(), target,
                            "热键切换（SwitchRequest → Ack → EnterScreen 接管闭环，docs/03 §5）");
                        run_local(&mut session, LocalCmd::Switch { target })
                    }
                    HotkeyAction::ReleaseControl => {
                        tracing::info!(target: "app::hotkey", hotkey = id.label(),
                            "热键回主控（释放当前受控从机）");
                        run_local(&mut session, LocalCmd::ReleaseControl)
                    }
                    HotkeyAction::SetPaused { paused } => {
                        tracing::info!(target: "app::hotkey", hotkey = id.label(),
                            paused, "热键暂停/恢复 toggle");
                        run_local(&mut session, if paused { LocalCmd::Pause } else { LocalCmd::Resume })
                    }
                    HotkeyAction::None(reason) => {
                        tracing::info!(target: "app::hotkey", hotkey = id.label(), reason,
                            "热键无动作");
                        Vec::new()
                    }
                };
                // 热键回主控的落点 = 切入锚点（11-24，R4；docs/03 §4.6）：macOS
                // 主控受控期间光标锚定在切入处，core 序列里的落点 warp（主屏内侧
                // 0.5/0.6）会把光标从锚点拽到屏幕中央——有捕获源即剥离；suppress
                // 尚未翻开（`last_suppress`）时把锚点交给 suppress=false 拍，在
                // 重关联**之后**重放（重关联会把光标拉到物理鼠标位、覆盖此前执行
                // 的 warp——11-22 回切落点覆盖修复已验证的路径）。锚点不可得 →
                // 重放点为空，光标停物理鼠标位；suppress 尚未翻开 → 只剥离不重放。
                #[cfg(feature = "macos")]
                if matches!(action, HotkeyAction::ReleaseControl)
                    && let Some(capture) = capture.as_ref()
                {
                    let stripped = strip_home_warp(&mut actions);
                    apply_home_landing(
                        &mut pending_release_warp,
                        stripped,
                        last_suppress,
                        capture.anchor_point(),
                    );
                }
                if let Err(error) = apply_and_send(
                    &mut transport,
                    &mut sink,
                    &actions,
                    &mut out_buf,
                    &bus,
                )
                .await
                {
                    break Err(error);
                }
                if let HotkeyAction::SetPaused { paused } = action {
                    bus.report(|state| {
                        if matches!(state.conn, ConnState::Running { .. }) {
                            state.conn = ConnState::Running { paused };
                        }
                    });
                    bus.flush();
                }
                consume_notices(&session, &actions, &bus);
            },
        }

        // 终态检测：Notice::Disconnected 落地即收尾（ReleaseAll 已由动作应用执行）。
        if let SessionState::Disconnected(reason) = session.state() {
            let reason = describe_disconnect(reason);
            tracing::warn!(target: "session::rt", peer = %peer, "会话断开：{reason}");
            bus.report(|state| {
                state.conn = ConnState::Disconnected {
                    reason: reason.clone(),
                }
            });
            bus.flush();
            break Ok(SessionEnd::SessionClosed);
        }
    };

    // 受控态收尾复位（11-20）：会话以任何方式结束（断开/切换/换码/退出）都
    // 必须恢复本地输入与光标——否则断开后 mac 键鼠永久被吞、光标永久隐藏。
    // last_suppress=true = 会话内曾进入受控态（10ms 拍内跟随 ControlDir）。
    #[cfg(feature = "macos")]
    if last_suppress && let Some(capture) = capture.as_ref() {
        // 收尾传 None：异常断开时光标停物理鼠标位（用户手在哪就在哪，语义
        // 自然）——释放落点重放仅用于回切/热键回主控的正常路径。
        capture.set_suppress_local_input(false, None);
        tracing::info!(target: "session::rt", "会话收尾：本地输入/光标已恢复（受控态复位）");
    }

    // 剪贴板通道收尾（11-21 双连接）：通道生命周期锚定控制会话——会话以任何
    // 方式结束都关闭通道（stop 信号 + abort；泵任务内未完成出站传输随运行时
    // drop 丢弃，watcher 线程随 Drop 退出）。暂存的控制连接交还 accept 循环。
    if let Some(guard) = file_channel.take() {
        guard.close();
    }
    crate::file_channel::lock_hub().reset_for_new_session();
    if let Some(guard) = clip_channel.take() {
        guard.close();
    }

    transport.shutdown().await;
    match result {
        Ok(end) => Ok((end, pending_control)),
        Err(error) => {
            report_session_error(&bus, &error);
            Err(error)
        }
    }
}

/// 会话期间的新连接轮询（host）：[`Server::next_connection`] cancel-safe
/// （mpsc recv）；client（无 server）永挂起——select 分支零唤醒。
async fn poll_incoming(server: Option<&mut Server>) -> Option<Connection> {
    match server {
        Some(server) => server.next_connection().await,
        None => std::future::pending().await,
    }
}

/// 平台注入 sink 工厂（11-20 生产接线修复：生产运行时曾恒用 RecordingInputSink——
/// 端到端测试断言到 mock 收到为止，最后一步从不落地 OS，FR-2.x 实机"没反应"根因）。
///
/// 优先级：wayland（uinput 内核级注入，KDE Wayland 实测）→ x11（XTEST）→
/// macos（CGEvent）→ windows（SendInput）；构造失败逐级回退，全败 → 记录型
/// sink + warn（输入管线不因平台错误中断，error-handling.md 规则 4）。
/// uinput ABS 轴标定 = [`UinputInjector::auto`]（wl_output 枚举真实分辨率，
/// 11-22 边缘回切修复轮替换 (1920,1080) 硬编码——实机 4K 屏下硬编码使
/// delta 粒度粗一倍；探测不可用走 1920×1080 兜底 + warn）。相对移动
/// 与边缘进入（ratio 0/1）不受标定值影响。
#[must_use]
fn build_platform_sink() -> Box<dyn InputSink + Send> {
    #[cfg(feature = "wayland")]
    if let Ok(sink) = xcontrol_input::UinputInjector::auto() {
        tracing::info!(target: "session::rt", backend = "uinput", "平台注入 sink 就绪");
        return Box::new(sink);
    } else {
        tracing::warn!(target: "session::rt", "uinput 注入器不可用（检查 /dev/uinput 权限/组成员）：降级下一后端");
    }
    #[cfg(feature = "x11")]
    if let Ok(sink) = xcontrol_input::X11Injector::new() {
        tracing::info!(target: "session::rt", backend = "xtest", "平台注入 sink 就绪");
        return Box::new(sink);
    }
    #[cfg(feature = "macos")]
    if let Ok(sink) = xcontrol_input::MacInjector::new() {
        tracing::info!(target: "session::rt", backend = "cgevent", "平台注入 sink 就绪");
        return Box::new(sink);
    } else {
        tracing::warn!(target: "session::rt", "MacInjector 不可用（辅助功能授权？）：降级下一后端");
    }
    #[cfg(feature = "windows")]
    if let Ok(sink) = xcontrol_input::WinInjector::new() {
        tracing::info!(target: "session::rt", backend = "sendinput", "平台注入 sink 就绪");
        return Box::new(sink);
    }
    tracing::warn!(target: "session::rt",
        "无可用平台注入后端：回退记录型 sink（注入不落地 OS——测试/无平台 feature 编译面）");
    Box::new(RecordingInputSink::new())
}

/// keymap 配置 → 会话注入 sink（映射引擎 + 平台 sink 包装；FR-3.x 接线点）。
///
/// 规则编译失败降级恒等并告警——config 校验应已拦截坏规则名，此处防御
/// （输入管线不因映射配置错误中断，error-handling.md 规则 4）。
fn build_mapping_sink(
    keymap: &KeymapConfig,
    inner: Box<dyn InputSink + Send>,
) -> MappingSink<Box<dyn InputSink + Send>> {
    let engine = match MappingEngine::new(&keymap.preset, &keymap.rules, keymap.passthrough) {
        Ok(engine) => engine,
        Err(error) => {
            tracing::warn!(target: "session::rt", %error,
                "keymap 规则编译失败：降级为恒等映射（请检查 config [keymap.rules]）");
            MappingEngine::identity()
        }
    };
    MappingSink::new(engine, inner)
}

/// 热键/映射即时生效（11-21 实测反馈 3/4，docs/06 §4.1 生效时点表热键/keymap
/// 行）：会话内收到 `RuntimeConfigUpdate` 后把新配置编译并**原位替换**运行中
/// 的检测面——
///
/// ```text
/// 新 [hotkeys] ─编译→ HotkeySet ─┬→ HotkeyFilterSink（从机注入前过滤，全平台）
///                                ├→ HotkeyFilter（mac 主控捕获流识别）
///                                └→ PlatformHotkeySource::update（win/x11
///                                   OS 注册路径——重建注册；mac 无注册源）
/// 新 [keymap]  ─编译→ set_rules → MappingSink（注入改写规则热替换）
/// ```
///
/// 编译失败语义：热键 = 保留旧表 + 告警（下会话构造点会重试新配置）；映射 =
/// `set_rules` 内部保证旧规则原样 + 告警。任一路径都不中断输入管线
/// （error-handling.md 规则 4）。被动捕获架构（mac CGEventTap ListenOnly /
/// Linux XRecord 全量捕获 + core 层匹配）无 grab 重注册需求——替换匹配表即生效。
fn apply_hot_key_configs<S: InputSink>(
    config: &Config,
    sink: &mut HotkeyFilterSink<MappingSink<S>>,
    #[cfg(feature = "macos")] capture_filter: &mut HotkeyFilter,
    hotkey_source: &mut crate::hotkey_rt::PlatformHotkeySource,
) {
    // 热键表（FR-2.5）。
    match HotkeySet::from_config(&config.hotkeys) {
        Ok(set) => {
            sink.set_hotkeys(set.clone());
            #[cfg(feature = "macos")]
            capture_filter.set_hotkeys(set.clone());
            hotkey_source.update(&set);
            tracing::info!(target: "app::hotkey",
                next = %config.hotkeys.next, prev = %config.hotkeys.prev,
                back_to_host = %config.hotkeys.back_to_host, pause = %config.hotkeys.pause,
                "热键表已热更新（会话内即时生效，docs/06 §4.1）");
        }
        Err(error) => {
            tracing::warn!(target: "app::hotkey", %error,
                "热键配置编译失败：保留旧热键表继续生效（下次会话构造点重试，docs/06 §4.1）");
        }
    }
    // 映射规则（FR-3.x；编译失败由 set_rules 保持旧规则原样）。
    if let Err(error) = sink.inner_mut().set_rules(
        &config.keymap.preset,
        &config.keymap.rules,
        config.keymap.passthrough,
    ) {
        tracing::warn!(target: "session::rt", %error,
            "keymap 规则热编译失败：保留旧映射规则继续生效（请检查 config [keymap.rules]）");
    } else {
        tracing::info!(target: "session::rt", preset = %config.keymap.preset,
            passthrough = config.keymap.passthrough,
            "映射规则已热更新（会话内即时生效，docs/06 §4.1）");
    }
}

/// 本地命令执行（实时路径容错形态，error-handling.md 规则 4）：被状态机拒绝
/// 记日志跳过（返回空动作），不中断会话循环——与 edge 分支的拒绝处理同构。
fn run_local<C: xcontrol_core::clock::Clock>(
    session: &mut Session<C>,
    cmd: LocalCmd,
) -> Vec<SessionAction> {
    let mut dispatcher = Dispatcher::new(session);
    match dispatcher.on_local(cmd) {
        Ok(actions) => actions,
        Err(error) => {
            tracing::warn!(target: "session::rt", %error, "本地命令被状态机拒绝（跳过）");
            Vec::new()
        }
    }
}

/// 识别"保持原位切入"（11-24，R2；docs/03 §5 热键闭环，纯函数）：动作序列产出
/// [`Notice::EnteredControl`]（本机进入受控）且**不含**落点 warp——即
/// `EnterScreen{preserve_cursor=1}` 路径（热键/托盘切换：被控端光标留在切出时
/// 的位置）。边缘切入的序列含 `WarpPointer`（定位到进入点），不属此类。
///
/// 消费面：`drive_session` 收帧分支在动作应用成功后据此调用一次
/// [`InputSink::resync_cursor`]（位置模型校准）。
#[must_use]
fn is_preserved_entry(actions: &[SessionAction]) -> bool {
    let entered = actions
        .iter()
        .any(|action| matches!(action, SessionAction::Notice(Notice::EnteredControl { .. })));
    let warped = actions
        .iter()
        .any(|action| matches!(action, SessionAction::WarpPointer { .. }));
    entered && !warped
}

/// 剥离动作序列里的落点 warp（11-24，R4 / design D8；纯函数）：热键回主控
/// （`ReleaseControl`）序列 = `ReleaseAll` + `LeaveScreen` + core 的落点 warp
/// （主屏内侧 0.5/0.6）。macOS 主控以切入锚点替换该落点——先剥离（否则下方
/// `apply_and_send` 会立即把光标从锚点拽到屏幕中央），锚点留到 suppress=false
/// 拍重放。返回是否剥离到（序列不含 warp = 状态机未产出回主控序列）。
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn strip_home_warp(actions: &mut Vec<SessionAction>) -> bool {
    let before = actions.len();
    actions.retain(|action| !matches!(action, SessionAction::WarpPointer { .. }));
    actions.len() != before
}

/// 热键回主控的落点重放判定（11-24，纯函数；[`strip_home_warp`] 的配套）：决定
/// 是否改写 suppress=false 拍的重放缓存 `pending`，以及改写成什么。
///
/// - 未剥离到（动作序列不含落点 warp）→ 不动；
/// - suppress 尚未翻开（`suppress_open == false`：接管后一个输入拍内就按了回
///   主控，没有翻拍可承接重放）→ 不动——只剥离，光标原地不动；
/// - 其余 → `pending` 置为 `anchor`：`Some` = 切入锚点（重关联后重放，光标
///   停在受控期间所见位置）；`None` = 锚点不可得（切入时光标查询失败）——
///   缓存清空，光标停物理鼠标位。
///
/// 返回是否改写了缓存。
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn apply_home_landing(
    pending: &mut Option<(f64, f64)>,
    stripped: bool,
    suppress_open: bool,
    anchor: Option<(f64, f64)>,
) -> bool {
    if !stripped || !suppress_open {
        return false;
    }
    *pending = anchor;
    true
}

// —— 回切行程补偿（11-24，R8 / AC10；design D13） ——

/// 边缘回切的落点素材（收帧动作序列解出）。
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
struct EdgeReturnLanding {
    /// core 落点 warp 的屏（本机 rects 下标）。
    screen: ScreenId,
    /// core 落点 warp 的归一化 X（未含补偿）。
    x: f32,
    /// core 落点 warp 的归一化 Y（未含补偿）。
    y: f32,
    /// 回报段行程（[`Notice::EdgeReturnedToHost`] 的 `overflow`，主控点）。
    report_travel: (f32, f32),
}

/// 从收帧动作序列识别**边缘回切**并取出落点素材（纯函数）：序列须同时含
/// [`Notice::EdgeReturnedToHost`]（回切的确凿标志——同批的 `WarpPointer` 就是它的
/// 环绕落点）与 `WarpPointer`；缺一不算（热键回主控的落点 warp 已在热键分支
/// 剥离，不会误入）。消费面：`drive_session` 收帧分支的落点补偿与立即翻拍。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn edge_return_of(actions: &[SessionAction]) -> Option<EdgeReturnLanding> {
    let report_travel = actions.iter().find_map(|action| match action {
        SessionAction::Notice(Notice::EdgeReturnedToHost { overflow, .. }) => Some(*overflow),
        _ => None,
    })?;
    let (screen, x, y) = actions.iter().find_map(|action| match action {
        SessionAction::WarpPointer { screen, x, y } => Some((*screen, *x, *y)),
        _ => None,
    })?;
    Some(EdgeReturnLanding {
        screen,
        x,
        y,
        report_travel,
    })
}

/// 在途批窗口（design D13）：主控每 10ms 转发一批，工作机每 10ms 采样一次并回报。
/// 回切触发时（回报到达主控的时刻），距发出不足本窗口的那一批可能是在工作机
/// 采样**之后**才到并注入的——没算进触发回切的那条回报。窗口取往返时延量级
/// （局域网单程约 1ms，3ms 覆盖到约 1.5ms 单程）；一批之内至多一批落窗（批间隔
/// 10ms 远大于窗口）。宁多补不漏补：实际已计入的那批最多让落点多走一批的位移
/// （10ms 的移动量），漏补则落点停在中途。
const IN_FLIGHT_WINDOW: Duration = Duration::from_millis(3);

/// 在途批判定（11-24，纯函数）：最近一次成功发出的批（发出时刻，批内鼠标位移
/// 和）中，发出时刻距 `now` 不足 [`IN_FLIGHT_WINDOW`] 的计入补偿，否则不计。
/// `None`（本会话尚未发出过批）→ `(0.0, 0.0)`。发出时刻晚于 `now`（不应出现）
/// 时 `Instant::duration_since` 饱和为零，不 panic。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn in_flight_batch(last: Option<(Instant, (f32, f32))>, now: Instant) -> (f32, f32) {
    match last {
        Some((sent_at, sum)) if now.duration_since(sent_at) < IN_FLIGHT_WINDOW => sum,
        _ => (0.0, 0.0),
    }
}

/// 一批捕获/转发事件里的鼠标位移求和（11-24，纯函数）：逐条累加 `MouseMoveRel`
/// 的 `(dx, dy)`（主控点，符号与屏幕坐标轴同向），其余事件不计。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn sum_mouse_deltas<'a>(events: impl IntoIterator<Item = &'a InputEvent>) -> (f32, f32) {
    let (mut dx, mut dy) = (0.0_f32, 0.0_f32);
    for event in events {
        if let InputEvent::MouseMoveRel {
            dx: event_dx,
            dy: event_dy,
        } = event
        {
            dx += *event_dx;
            dy += *event_dy;
        }
    }
    (dx, dy)
}

/// 高频微步回放（11-25 第二轮，R11；用户实测"6 步低频回放仍不顺滑"后改为
/// 惯性滑动式）：每步 2ms、匀速——积压行程是"被钳住之前正在发生的运动"，
/// 恢复交付时按原始速度分小步，每步只有 1 点上下，视觉上是连续运动而非跳变。
const RETURN_REPLAY_STEP: Duration = Duration::from_millis(2);

/// 单步的位移下限（点，防饿死）：行程极小时步长不至于小到无法感知；
/// 上限由总步数封顶推出（`RETURN_REPLAY_MAX_STEPS` × 步长 ≥ 行程）。
const RETURN_REPLAY_MIN_STEP: f64 = 1.0;

/// 回放总步数上限（11-25 第二轮，R11）：总时长 = 步数 × [`RETURN_REPLAY_STEP`] ≤
/// 60ms。行程超出"原始速度 × 总时长"时，步长等比例放大（仍匀速——位移越大
/// 说明被钳住之前甩得越快，恢复交付也越快，方向一致）。
const RETURN_REPLAY_MAX_STEPS: usize = 30;

/// 回放总时长（11-25 第二轮）：回放在这个量级内完成——原始速度由
/// "总位移 ÷ 本时长"定义，时长越长、回放越慢、越像惯性。
const RETURN_REPLAY_DURATION: Duration = Duration::from_millis(60);

/// 回切行程的回放切片（11-25；纯函数）：把"补偿终点 − 贴缘基点"的位移按
/// **匀速**切成微步（速度 = 总位移 ÷ [`RETURN_REPLAY_DURATION`]，即"被钳住之前
/// 那一下"的平均速度——快甩快回，慢推慢回）。三个约束同时成立：步数 ≤
/// [`RETURN_REPLAY_MAX_STEPS`]（总时长 ≤ 60ms）、单步 ≥ [`RETURN_REPLAY_MIN_STEP`]、
/// 各步之和恒等于总位移（望远镜求和，无舍入漂移）。
/// **逆序存放**——队列以 `pop()` 出步，弹出顺序即回放顺序。
///
/// 返回空队列（不回放）：终点或基点缺失（屏下标越界 / 非有限坐标）、总位移为零
/// 或非有限。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn replay_slices(compensated: Option<(f64, f64)>, base: Option<(f64, f64)>) -> Vec<(f64, f64)> {
    let (Some((end_x, end_y)), Some((base_x, base_y))) = (compensated, base) else {
        return Vec::new();
    };
    let (total_x, total_y) = (end_x - base_x, end_y - base_y);
    if !total_x.is_finite() || !total_y.is_finite() || (total_x == 0.0 && total_y == 0.0) {
        return Vec::new();
    }
    let magnitude = (total_x * total_x + total_y * total_y).sqrt();
    if magnitude <= 0.0 {
        return Vec::new();
    }
    // 名义步长 = 原始速度 × 单步时长（快甩大步、慢推小步）；夹进 [MIN_STEP,
    // 行程/最大步数]——下限防饿死，上限同时封住步数（≤ MAX_STEPS）与总时长。
    let step_len = ((magnitude / RETURN_REPLAY_DURATION.as_secs_f64())
        * RETURN_REPLAY_STEP.as_secs_f64())
    .clamp(
        RETURN_REPLAY_MIN_STEP,
        (magnitude / RETURN_REPLAY_MAX_STEPS as f64).max(RETURN_REPLAY_MIN_STEP),
    );
    let steps = ((magnitude / step_len).ceil() as usize).clamp(1, RETURN_REPLAY_MAX_STEPS);
    let mut slices = Vec::with_capacity(steps);
    let mut reached = 0.0;
    for step in 1..=steps {
        let now = (step as f64 / steps as f64 * magnitude).min(magnitude);
        let t = now - reached;
        slices.push((total_x * (t / magnitude), total_y * (t / magnitude)));
        reached = now;
    }
    slices.reverse();
    slices
}

/// 回放消费线程（11-25 第二轮，R11）：每 [`RETURN_REPLAY_STEP`] 相对 warp 一步，
/// 队列空即退出（生命周期 = 一次回放，约 60ms）。warp 是进程级线程安全 API
/// （`xcontrol_input::shift_cursor`，不依赖任何实例状态），线程只做"warp +
/// sleep"，无锁无共享可变状态（回调线程纪律的同型约束）；warp 失败即终止
/// （语义：光标停在当前位置）。
#[cfg(feature = "macos")]
fn spawn_cursor_replay(replay: Vec<(f64, f64)>) {
    if replay.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("xcontrol-return-replay".into())
        .spawn(move || {
            for (dx, dy) in replay {
                if let Err(error) = xcontrol_input::shift_cursor(dx, dy) {
                    tracing::warn!(target: "app::edge", %error,
                        "回切行程回放中止（本步光标平移失败）：光标停在当前位置");
                    break;
                }
                std::thread::sleep(RETURN_REPLAY_STEP);
            }
        });
}

/// 本地输入拦截开关对账 + 释放落点重放（11-24 自输入拍抽出，纯接线函数；边缘
/// 回切的"排空后立即翻拍"与输入拍两处共用——翻拍语义单一出处）。
///
/// `want_suppress`（= 当前是否受控中）与 `last_suppress`（tap 的现态）不同才动作：
///
/// - 翻到受控（`true`）：开本地输入拦截（`CaptureSource` 内部捕获锚点、解关联）；
/// - 翻回本地（`false`）：**释放落点重放**（11-22 回切落点覆盖修复）——重关联会把
///   光标拉到物理鼠标当前位置、覆盖此前执行的落点 warp，所以把缓存的落点
///   （`pending_release_warp`，边缘回切 = 补偿后的落点 / 热键回主控 = 切入锚点）
///   交给 `set_suppress_local_input` 在重关联**之后**重放；缓存为空 = 不做落点
///   定位，光标停物理鼠标位。
///
/// 状态一致时不动（幂等）：输入拍每 10ms 调一次，只有控制关系变化的那一拍翻拍。
#[cfg(feature = "macos")]
fn reconcile_local_input_suppress(
    capture: &xcontrol_input::CaptureSource,
    pending_release_warp: &mut Option<(f64, f64)>,
    last_suppress: &mut bool,
    want_suppress: bool,
) {
    if want_suppress == *last_suppress {
        return;
    }
    let release_point = if want_suppress {
        None
    } else {
        pending_release_warp.take()
    };
    capture.set_suppress_local_input(want_suppress, release_point);
    tracing::info!(target: "session::rt", suppress = want_suppress,
        "本地输入拦截开关更新（受控重定向）");
    *last_suppress = want_suppress;
}

/// 回切触发后排空的那批捕获事件的善后（11-24，design D13；macOS 主控）：这些
/// 事件在翻拍前已被 tap 本地拦截（Drop），而控制关系已终结——**不转发**，但照常
/// 过两道与转发无关的面，语义与输入拍一致：
///
/// - 鼠标按键位图（切入门控读数 + 状态机回切门控的 `LocalCmd::MouseButtons`
///   镜像，变化沿才发）——位图不因排空而漏记一次按下/抬起；
/// - 捕获流热键过滤（命中 → 投路由通道；尾巴吞）——与不排空、留到下一输入拍
///   处理的结果相同（那时同样不转发），只是提前。
///
/// 鼠标位移之和由调用方经 [`sum_mouse_deltas`] 计入落点补偿，这里不消费。
#[cfg(feature = "macos")]
fn absorb_unforwarded_events(
    events: &[InputEvent],
    capture_filter: &mut HotkeyFilter,
    hotkey_tx: &mpsc::UnboundedSender<HotkeyId>,
    mouse_buttons: &mut u8,
    mouse_buttons_down: &mut bool,
    session: &mut Session<SystemClock>,
) {
    for event in events {
        track_mouse_buttons(mouse_buttons, event);
    }
    let now_mouse_down = *mouse_buttons != 0;
    if now_mouse_down != *mouse_buttons_down {
        *mouse_buttons_down = now_mouse_down;
        let mut dispatcher = Dispatcher::new(session);
        if let Err(error) = dispatcher.on_local(LocalCmd::MouseButtons {
            any_down: now_mouse_down,
        }) {
            tracing::warn!(target: "session::rt", %error,
                "MouseButtons 被状态机拒绝（跳过）");
        }
    }
    for event in events {
        match capture_filter.feed(event) {
            FilterOutcome::Hit(id) => {
                tracing::info!(target: "app::hotkey", hotkey = id.label(),
                    source = "capture", "热键命中（捕获层识别，回切排空批，事件不转发）");
                let _ = hotkey_tx.send(id);
            }
            FilterOutcome::HitTail => {}
            FilterOutcome::Forward => {}
        }
    }
}

/// 捕获源自动恢复的探测间隔（11-24，R9；design D15）：未授权期间每秒静默查询
/// 一次授权状态，授权后约 1s 内恢复。
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
const CAPTURE_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// 捕获源恢复探测是否到期（11-24，纯函数）：距上次探测满
/// [`CAPTURE_RETRY_INTERVAL`]（或本会话尚未探测过）→ 到期。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn capture_probe_due(last_probe: Option<Instant>, now: Instant) -> bool {
    last_probe.is_none_or(|at| now.duration_since(at) >= CAPTURE_RETRY_INTERVAL)
}

/// 捕获源重建失败告警的退避间隔（11-25）：**重试本身仍每秒一次**（恢复要快，
/// 未授权 → 已授权的转换、或一次性的启动失败都要尽快接上），只让**告警**退避
/// ——已授权但捕获源启动持续失败时，否则每个探测间隔一条。
const CAPTURE_FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// 捕获源重建失败是否该留一条告警（11-25，纯函数）：首次失败立即告警；此后距上次
/// 告警满 [`CAPTURE_FAILURE_LOG_INTERVAL`] 才再告警一次。`None` = 尚未告警过（或
/// 上次恢复成功后已复位）→ 到期。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn capture_failure_log_due(last_logged: Option<Instant>, now: Instant) -> bool {
    last_logged.is_none_or(|at| now.duration_since(at) >= CAPTURE_FAILURE_LOG_INTERVAL)
}

/// 捕获源恢复前释放名义上的控制关系（11-25；macOS 主控，`drive_session` 输入拍的
/// 捕获源自动恢复调用）。未授权期间没有捕获源，但边缘传感器不需要权限——主控
/// 仍可靠它切入（`LocalControlsPeer`，本地输入照常、只是不转发）。授权后捕获源
/// 一恢复，同一拍的本地输入拦截与转发立即生效，键鼠被送到工作机，而用户并没有
/// 在此刻发起切换——所以恢复前先经 `ReleaseControl` 释放：`ReleaseAll` 与
/// `LeaveScreen` 照常产出，**落点 warp 剥离**（光标不动，做法同热键回主控的
/// [`strip_home_warp`]）。未被控（或状态机拒绝）时产出空序列，调用方不必发送。
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn release_control_before_capture_recovery<C: xcontrol_core::clock::Clock>(
    session: &mut Session<C>,
) -> Vec<SessionAction> {
    let mut actions = run_local(session, LocalCmd::ReleaseControl);
    strip_home_warp(&mut actions);
    actions
}

/// 捕获源自动恢复在重建之前要做的事（11-25；纯决策
/// [`capture_recovery_step`] 的结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
enum CaptureRecoveryStep {
    /// 未被控：直接重建。
    Rebuild,
    /// 名义上处于受控态：先释放控制关系（[`release_control_before_capture_recovery`]），
    /// 再重建。
    ReleaseThenRebuild,
    /// 名义上受控且**共享暂停中**：本拍不重建，推迟到恢复之后——暂停期间对端拒收
    /// 全部输入帧（`LeaveScreen` 会被丢弃，core `PausedReject`：拒帧不断连），
    /// 此时释放只会让两端控制状态失配；而捕获源仍缺席，暂停期间本就什么都不转发。
    DeferWhilePaused,
}

/// 捕获源自动恢复在重建之前要做的事（11-25，纯函数）：未被控直接重建；名义上受控
/// 先释放；名义上受控且共享暂停中推迟。
#[must_use]
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn capture_recovery_step(control_dir: ControlDir, paused: bool) -> CaptureRecoveryStep {
    match (control_dir == ControlDir::LocalControlsPeer, paused) {
        (false, _) => CaptureRecoveryStep::Rebuild,
        (true, false) => CaptureRecoveryStep::ReleaseThenRebuild,
        (true, true) => CaptureRecoveryStep::DeferWhilePaused,
    }
}

/// 重建主控捕获源并留痕（11-25；捕获源自动恢复用——调用方已确认辅助功能已授权、
/// 且名义上的受控态已解除或已推迟）：成功 → `info!` 一次、失败告警的退避复位；
/// 失败 → 告警按 [`capture_failure_log_due`] 退避（重试仍每个探测间隔一次）。
#[cfg(feature = "macos")]
fn rebuild_capture_source(
    capture: &mut Option<xcontrol_input::CaptureSource>,
    failure_logged_at: &mut Option<Instant>,
) {
    match crate::hotkey_rt::try_host_capture_source() {
        Ok(source) => {
            *capture = Some(source);
            *failure_logged_at = None;
            tracing::info!(target: "session::rt",
                "主控捕获源已恢复（辅助功能已授权）：热键识别与捕获转发可用，无需重启应用或重建会话");
        }
        Err(error) => {
            if capture_failure_log_due(*failure_logged_at, Instant::now()) {
                *failure_logged_at = Some(Instant::now());
                tracing::warn!(target: "session::rt", %error,
                    "主控捕获源重建失败（辅助功能已授权，但捕获源启动不了）：\
                     热键识别与捕获转发停用；每秒静默重试，恢复后留痕\
                     （本告警至多 30s 一条）");
            }
        }
    }
}

/// 边缘命中是否允许上报（ControlDir 门控 + 暂停语义 + 按键门控，FR-2.1 + PRD
/// 收敛项 3 + 11-22 第三轮 + 11-24 死区落地）：仅 **Running 且未共享暂停、且
/// 无人在控、且无鼠标键按下**时允许。
///
/// - `control_dir == None`（11-24 收紧：原为 `!= PeerControlsLocal`，对
///   `LocalControlsPeer` 放行）：
///   - `PeerControlsLocal`（收到对端 `EnterScreen`，本机被控）→ 拒绝：被控中
///     本机光标不驱动切换；对端 `LeaveScreen` 复位 `ControlDir::None` 后恢复；
///   - `LocalControlsPeer`（本机已切出、正在控制对端）→ **拒绝**：受控期间
///     主控光标被锚定在切入处（macOS，docs/05 §1.2），不存在有意滑边；两次
///     锚定回拉之间光标会冲到屏边，被传感器命中后重复发出的 `EdgeCrossing`
///     会让状态机重发 `LeaveScreen` + `EnterScreen`——工作机光标被拉回进入
///     点、按键被释放、回切免疫窗被重置。原放行的理由是"受控中链式滑边"
///     （FR-2.2 的 M3 路由基础），链式切换尚未实现；将来若实现，应走显式
///     意图（热键/托盘），或只在光标能被真正冻结的平台上另开入口；
/// - `mouse_buttons_down`（11-22 第三轮，docs/03 §5）→ 拒绝：拖文件/拖滚动条
///   时滑到边缘被切换会中断拖拽——任意鼠标键按下不切入（上报被丢弃，传感
///   器已闩锁：松开后须回内侧再推，防按键释放瞬间残留贴边误触发）。热键切换
///   不经此判定面（用户明确意图不受限）；
/// - 共享暂停（FR-2.8）/非 Running（协商中/已断开）→ 拒绝：暂停下切换会
///   建立一条收端拒收输入帧的控制链，无意义。
#[must_use]
fn edge_hit_allowed(
    state: &SessionState,
    control_dir: ControlDir,
    mouse_buttons_down: bool,
) -> bool {
    matches!(state, SessionState::Running { paused: false })
        && control_dir == ControlDir::None
        && !mouse_buttons_down
}

/// 控制关系变化后的切入防抖窗（11-24，docs/03 §5"死区"条款落地）：任意变化沿
/// （切入/回切/热键回主控/对端 LeaveScreen）之后本窗内的主控边缘命中一律忽略。
///
/// 背景：回切落点贴着主控屏缘（内缩 0.5%，mac 约 7.5pt），回切瞬间用户仍在朝
/// 同方向快速甩动——落点再一次触边会把刚回到的主控立即切回工作机（回弹）。
/// 传感器的去抖闩锁覆盖不了这一段（落点在严格内侧，闩锁已重新武装）。
const EDGE_SWITCH_DEAD_TIME: Duration = Duration::from_millis(150);

/// 防抖窗是否已过（11-24，纯函数）：`control_changed_at` 为最近一次控制关系
/// 变化沿的时刻；距该沿不足 [`EDGE_SWITCH_DEAD_TIME`] → 未过（命中忽略），
/// 达到即过。`None` = 本会话尚未观测到变化沿（空闲常态）→ 恒过。
#[must_use]
fn edge_dead_time_elapsed(control_changed_at: Option<Instant>, now: Instant) -> bool {
    control_changed_at.is_none_or(|changed| now.duration_since(changed) >= EDGE_SWITCH_DEAD_TIME)
}

/// 鼠标按键位图位（11-22 第三轮按键门控的跟踪面，docs/03 §5）：协议按钮
/// 枚举 → 位图位（任意键按下 = 位图非零）。位序与 docs/02 §4.2 button 表
/// wire 值同构（0=左、1=中、2=右、3=后退、4=前进——位图只用相对序，不传
/// 线）。runtime 调用点在 macos 门控块内（捕获源仅 mac Host）——非 macos
/// 编译面仅测试消费，豁免 dead_code。
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
#[must_use]
fn mouse_button_bit(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1 << 0,
        MouseButton::Right => 1 << 1,
        MouseButton::Middle => 1 << 2,
        MouseButton::Back => 1 << 3,
        MouseButton::Forward => 1 << 4,
    }
}

/// 捕获流事件的鼠标按键位图推进（纯函数）：Down 置位 / Up 清位，多键任一
/// 按下即门控；非按键事件不改动位图。跟踪位置 = capture poll 消费处（tap
/// 全态透传——受控与未受控的事件都在此路过，同一位图覆盖切入〔未受控态
/// 判定〕与回切〔受控态判定〕两个门控面）。dead_code 豁免理由同上。
#[cfg_attr(not(feature = "macos"), allow(dead_code))]
fn track_mouse_buttons(bits: &mut u8, event: &InputEvent) {
    if let InputEvent::MouseButton { button, state } = event {
        let bit = mouse_button_bit(*button);
        match state {
            KeyState::Down => *bits |= bit,
            KeyState::Up => *bits &= !bit,
        }
    }
}

/// 边缘轮询未来（select 分支体）：有传感器 → 等下一拍 → 读光标 + 判定；
/// 无传感器 → 永挂起（select 零唤醒）。
///
/// 每次循环重建该未来，ticker 状态在 `&mut Interval` 内跨拍保持（节奏稳定）；
/// 光标读取是 CG 快速查询（微秒级、非阻塞 I/O），无需 spawn_blocking。
async fn poll_edge_sensor(
    sensor: Option<&mut EdgeSensor>,
    ticker: &mut tokio::time::Interval,
) -> Option<EdgeHit> {
    match sensor {
        Some(sensor) => {
            ticker.tick().await;
            sensor.poll_cursor()
        }
        None => std::future::pending().await,
    }
}

/// 受控光标采样（select 回报分支体，macos——采样面矩阵与主控边缘轮询一致，
/// docs/03 §5）：受控目标（全局 id）经映射表反查本机屏 → 平台光标查询 →
/// 容差带归一化。任一环节缺失（非被控 / 目标未映射 / 查询失败 / 光标不在
/// 受控屏）→ `None` 跳过本拍。
///
/// # Platform
///
/// - `macos`：[`xcontrol_input::cursor_location`]（CG 快速查询，无需辅助功能
///   权限）+ 会话启动时快照的本机显示矩形（`local_display_rects`）。
/// - 其余平台：本函数不编译——回报分支直接走注入 sink 查询
///   （[`InputSink::cursor_normalized`]，uinput ABS 域 / XQueryPointer，
///   11-22 边缘回切修复轮起；此前非 macos 恒 None = Linux 从机从不回报的
///   根因）。
#[must_use]
#[cfg(feature = "macos")]
fn sample_controlled_cursor(
    session: &Session<SystemClock>,
    rects: Option<&[crate::edge::ScreenRect]>,
) -> Option<(f32, f32)> {
    let rects = rects?;
    let target = session.controlled_target()?;
    let local = session
        .screen_map()
        .iter()
        .find(|(global, _)| *global == target)
        .map(|&(_, local)| local)?;
    let point = xcontrol_input::cursor_location().ok()?;
    crate::edge::controlled_cursor_normalized(rects, local, point)
}

/// 受控光标回报观察（探针，11-22 修复轮）：扫描解密明文流的完整帧，解出
/// `MouseMoveAbs`（kind 0x04）反向回报，按 1/s 节流 info 打印归一化值。
/// 回报在 core 层被消化为回切判定输入（无注入、无动作产出）——本函数是
/// 唯一的到达观测面；回报只进不出的"静默面"此前无法从日志分辨"没来"与
/// "来了但判定不触发"。解析面 [`scan_controlled_report`] 纯函数可单测。
///
/// 防御：帧解析失败/半帧 → 停止扫描（后续字节仍照常喂 dispatcher——本函数
/// 只读观察，不影响喂入流）；`last_logged` 节流 1/s 防刷屏。
fn observe_controlled_reports(stream: &[u8], last_logged: &mut Option<Instant>) {
    let Some((screen, x, y)) = scan_controlled_report(stream) else {
        return;
    };
    let now = Instant::now();
    if last_logged.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
        *last_logged = Some(now);
        tracing::info!(target: "app::edge", screen, x, y,
            "受控光标回报（1/s 节流探针；主控回切判定输入）");
    }
}

/// 扫描明文流中**最后一条** `MouseMoveAbs` 回报（纯函数；观察探针的解析面）。
///
/// 帧边界解析失败/半帧/未知 kind → 该处停止（与 split_clip_frames 同款防御：
/// 收到的是完整报文，解析失败属不可能事件，剩余字节交 dispatcher 判定）。
#[must_use]
fn scan_controlled_report(stream: &[u8]) -> Option<(ScreenId, f32, f32)> {
    use xcontrol_proto::frame::{FRAME_HEADER_LEN, decode_frame};
    let mut rest = stream;
    let mut latest: Option<(ScreenId, f32, f32)> = None;
    while rest.len() >= FRAME_HEADER_LEN {
        let Ok(frame) = decode_frame(rest) else {
            break;
        };
        let total = frame.total_len();
        if total == 0 {
            break; // 解析器防御：零长度不推进（防死循环；不可能事件）
        }
        if let Ok(Some(InputEvent::MouseMoveAbs { screen, x, y })) =
            InputEvent::decode(frame.header.kind, frame.payload)
        {
            latest = Some((screen, x, y));
        }
        rest = &rest[total..];
    }
    latest
}

/// 出站动作应用 + 发送（Send 帧整批一条 Noise 报文）。
///
/// `sink` 泛型接收（[`MappingSink`] 包装后仍是同一 trait 实现链——映射改写在
/// sink 边界内完成，调用方无感知；泛型而非 `dyn` 是因为 sink 持有跨 `.await`，
/// tokio 多线程执行域要求 `Send`）。
async fn apply_and_send<S: InputSink>(
    transport: &mut TcpTransport,
    sink: &mut S,
    actions: &[SessionAction],
    out_buf: &mut Vec<u8>,
    bus: &Arc<StateBus>,
) -> anyhow::Result<()> {
    // 11-20 帧粘包统一修复：**逐帧独立 send**（与 send_clip_frames 同款）。
    // 此前多帧编码进同一 out_buf 一次 send——transport.send 把整个 buffer 当
    // 一个 Noise 报文加密（1 报文粘 N 帧），接收端解密后按帧头只认第 1 帧，
    // 余下字节错位（输入路径靠 dispatcher 的 while 循环侥幸消化，但帧边界
    // 语义已破——剪贴板剥离路径即炸）。逐帧发送 = 每帧一个 Noise 报文，
    // 与 M1 协议的 1 应用帧 = 1 Noise 报文边界契约一致（M2 design 定稿）。
    // 同步回调只**收集**单帧，循环外统一 await 逐帧 send。
    let mut frames: Vec<(u32, Vec<u8>)> = Vec::new();
    let report = apply_actions(actions, sink, &mut |seq, msg| {
        let mut single = Vec::new();
        if let Err(error) = msg.encode_frame_into(seq, &mut single) {
            tracing::warn!(target: "session::rt", %error, "出站帧编码失败（丢弃）");
            return;
        }
        frames.push((seq, single));
    });
    bus.report(|state| state.events_in += report.injected as u64);
    out_buf.clear();
    for (_seq, frame) in frames {
        if let Err(error) = transport.send(&frame).await {
            return Err(disconnect_error("发送失败", &error));
        }
        bus.report(|state| state.packets_out += 1);
    }
    Ok(())
}

/// Notice 消费：协商完成上报快照 + 切换信令日志（Sink/Send 之外的对外可见
/// 事件）。切换通知（FR-2.5 热键信令握手）仅日志留痕——控制转移完成（Ack 后
/// 的 EnterScreen 选点）属 M5 布局语义，见 hotkey_rt 模块文档。
fn consume_notices(session: &Session<SystemClock>, actions: &[SessionAction], bus: &Arc<StateBus>) {
    for action in actions {
        match action {
            SessionAction::Notice(Notice::HandshakeComplete { minor, .. }) => {
                let peer_name = session.peer_name().map(str::to_owned);
                // 文件域协商门控基线（docs/02 §4.6）：minor ≥ 2 才允许文件通道。
                crate::file_channel::lock_hub().set_session_minor(*minor);
                tracing::info!(target: "session::rt", minor, "版本协商完成（会话运行中）");
                bus.report(|state| {
                    state.conn = ConnState::Running { paused: false };
                    if let Some(peer_name) = peer_name {
                        state.peer_name = Some(peer_name);
                    }
                });
                bus.flush();
            }
            SessionAction::Notice(Notice::SwitchRequested { target }) => {
                tracing::info!(target: "app::hotkey", target,
                    "对端请求切换到本机（SwitchRequest；已回 ReleaseAll + Ack）");
            }
            SessionAction::Notice(Notice::SwitchResult { target, ok }) => {
                tracing::info!(target: "app::hotkey", target, ok,
                    "切换请求结果（对端 SwitchAck）");
            }
            SessionAction::Notice(Notice::EdgeReturnedToHost { target, overflow }) => {
                tracing::info!(target: "app::edge", target, ?overflow,
                    "受控屏外侧滑出：边缘回切主控（环绕落点已执行，docs/03 §5；overflow = 回报段回切行程，单位=点）");
            }
            _ => {}
        }
    }
}

/// 断开错误归一（文案不含安全码/派生值）。
fn disconnect_error(context: &str, error: &NetError) -> anyhow::Error {
    match error {
        NetError::Closed => anyhow::anyhow!("{context}：对端已关闭连接"),
        NetError::DecryptFailed => anyhow::anyhow!("{context}：解密失败（流错位，协议违规）"),
        NetError::FrameInvalid => anyhow::anyhow!("{context}：帧格式违规"),
        NetError::IoFailed => anyhow::anyhow!("{context}：网络 I/O 错误"),
        NetError::HandshakeFailed => anyhow::anyhow!("{context}：安全码握手失败"),
        other => anyhow::anyhow!("{context}：{other}"),
    }
}

/// 断开原因的人类可读描述。
fn describe_disconnect(reason: &DisconnectReason) -> String {
    match reason {
        DisconnectReason::Timeout => "心跳超时（9s 无帧）".to_owned(),
        DisconnectReason::VersionIncompatible => "协议版本不兼容".to_owned(),
        DisconnectReason::ProtocolViolation => "协议违规".to_owned(),
        DisconnectReason::PeerError { code, msg } => format!("对端错误（{code:?}）：{msg}"),
        DisconnectReason::LocalQuit => "本端主动退出".to_owned(),
    }
}

/// 连接建立中快照（两入口共用）。
fn set_connecting(bus: &Arc<StateBus>) {
    bus.report(|state| state.conn = ConnState::Connecting);
    bus.flush();
}

/// StateError → anyhow（本地命令/Timer 拒绝路径）。
fn state_error(context: &str, error: StateError) -> anyhow::Error {
    anyhow::anyhow!("{context}：{error}")
}

#[cfg(test)]
mod tests {
    use xcontrol_config::Role as ConfigRole;
    use xcontrol_proto::input::{InputEvent, KeyState};
    use xcontrol_proto::xkey::{KEY_C, KEY_LEFT_CTRL, KEY_LEFT_META};

    use super::*;
    use crate::state::AppState;

    /// 映射接线（FR-3.1/3.6）：`build_mapping_sink` 按配置编译——mac2win 改写
    /// `LMeta↓` → `LCtrl↓`；passthrough 直通；坏预设降级恒等（输入不中断）。
    #[test]
    fn mapping_sink_builds_maps_passes_through_and_falls_back() {
        // mac2win：事件级改写
        let cfg = KeymapConfig {
            preset: "mac2win".to_owned(),
            ..KeymapConfig::default()
        };
        let engine = MappingEngine::new(&cfg.preset, &cfg.rules, cfg.passthrough)
            .expect("mac2win 预设编译应成功");
        let mut sink = MappingSink::new(engine, RecordingInputSink::new());
        sink.inject(&InputEvent::Key {
            code: KEY_LEFT_META,
            state: KeyState::Down,
        })
        .unwrap();
        assert_eq!(
            sink.inner().events(),
            &[InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Down,
            }]
        );

        // 直通模式（FR-3.6）：原样通过
        let passthrough = KeymapConfig {
            preset: "mac2win".to_owned(),
            passthrough: true,
            ..KeymapConfig::default()
        };
        let engine = MappingEngine::new(
            &passthrough.preset,
            &passthrough.rules,
            passthrough.passthrough,
        )
        .expect("直通编译应成功");
        let mut sink = MappingSink::new(engine, RecordingInputSink::new());
        sink.inject(&InputEvent::Key {
            code: KEY_LEFT_META,
            state: KeyState::Down,
        })
        .unwrap();
        assert_eq!(
            sink.inner().events(),
            &[InputEvent::Key {
                code: KEY_LEFT_META,
                state: KeyState::Down,
            }]
        );

        // 坏配置：降级恒等（会话继续、无 panic）
        let _bogus = KeymapConfig {
            preset: "mac2dos".to_owned(),
            ..KeymapConfig::default()
        };
        // 坏预设降级恒等的语义在 build_mapping_sink 内部——这里直接验证恒等引擎
        let engine = MappingEngine::identity();
        let mut sink = MappingSink::new(engine, RecordingInputSink::new());
        sink.inject(&InputEvent::Key {
            code: KEY_C,
            state: KeyState::Down,
        })
        .unwrap();
        assert_eq!(
            sink.inner().events(),
            &[InputEvent::Key {
                code: KEY_C,
                state: KeyState::Down,
            }]
        );
    }

    /// FR-5.5 选路 helpers：id 匹配（大小写宽容、全零 id 永不匹配）、
    /// 候选键（id 优先、地址回退）、来源标签（稳定字面值）。
    #[test]
    fn candidate_selection_helpers() {
        use xcontrol_net::CandidateSource;
        let candidate = Candidate {
            addr: "192.168.1.10:4243".parse().expect("常量地址"),
            instance_id: [0xAB; 16],
            name: "mac-studio".to_owned(),
            source: CandidateSource::Mdns,
        };
        // 记忆匹配：大小写宽容（配置侧手写宽容语义）。
        assert!(candidate_matches_id(
            &candidate,
            &crate::state::hex(&[0xAB; 16])
        ));
        assert!(candidate_matches_id(
            &candidate,
            &crate::state::hex(&[0xAB; 16]).to_ascii_uppercase()
        ));
        assert!(candidate_matches_id(
            &candidate,
            &format!("  {}  ", crate::state::hex(&[0xAB; 16]))
        ));
        assert!(
            !candidate_matches_id(&candidate, &"00".repeat(16)),
            "异 id 不匹配"
        );
        // 全零 id（TCP 兜底候选）永不匹配——记忆无从建立。
        let zero = Candidate {
            instance_id: [0u8; 16],
            ..candidate.clone()
        };
        assert!(!candidate_matches_id(&zero, &"00".repeat(16)));
        // 候选键：有 id 用 id hex；全零回退地址串（跨轮标注稳定性）。
        assert_eq!(candidate_key(&candidate), crate::state::hex(&[0xAB; 16]));
        assert_eq!(candidate_key(&zero), "192.168.1.10:4243");
        // 来源标签：四值稳定（状态页/托盘呈现面）。
        for (source, label) in [
            (CandidateSource::Mdns, "mdns"),
            (CandidateSource::Beacon, "beacon"),
            (CandidateSource::Probe, "probe"),
            (CandidateSource::TcpProbe, "tcp"),
        ] {
            assert_eq!(source_label(source), label);
        }
    }

    /// FR-5.5 候选投影：标注（软排除/记忆）随快照可见；空候选清空投影。
    #[test]
    fn candidate_projection_carries_flags() {
        let bus = StateBus::new(AppState {
            role: ConfigRole::Client,
            ..AppState::default()
        });
        let candidate = Candidate {
            addr: "192.168.1.10:4243".parse().expect("常量地址"),
            instance_id: [0xAB; 16],
            name: "mac-studio".to_owned(),
            source: xcontrol_net::CandidateSource::Probe,
        };
        let mut failed = std::collections::HashSet::new();
        failed.insert(candidate_key(&candidate));
        project_candidates(
            &bus,
            std::slice::from_ref(&candidate),
            Some(&candidate_key(&candidate)),
            &failed,
        );
        let state = bus.current();
        assert_eq!(state.host_candidates.len(), 1);
        let projected = &state.host_candidates[0];
        assert_eq!(projected.name, "mac-studio");
        assert_eq!(projected.addr, "192.168.1.10:4243");
        assert_eq!(projected.instance_id_hex, candidate_key(&candidate));
        assert_eq!(projected.source, "probe");
        assert!(projected.last_handshake_failed, "软排除标注可见");
        assert!(projected.confirmed, "记忆标注可见");

        // 空候选清空投影（不残留上一轮列表）。
        project_candidates(&bus, &[], None, &failed);
        assert!(bus.current().host_candidates.is_empty());
    }

    /// 换码协议（docs/07 §3.3）：store_runtime_config 检测 security_code_ref
    /// 变化并产出 ConfigDelta——会话循环据此断开、supervisor 据此重派生。
    #[test]
    fn store_runtime_config_detects_code_ref_change() {
        let cell = Arc::new(std::sync::Mutex::new(Config::default()));
        let new_config = Config {
            security_code_ref: "keychain:xcontrol-rotated".to_owned(),
            ..Config::default()
        };
        let delta = store_runtime_config(&cell, &new_config);
        assert!(delta.code_ref_changed, "引用变化必须被检测");
        assert_eq!(
            cell.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .security_code_ref,
            "keychain:xcontrol-rotated",
            "共享单元已更新"
        );
        // 引用不变 → 无换码信号。
        let delta = store_runtime_config(&cell, &new_config);
        assert!(!delta.code_ref_changed);
    }

    /// 断开原因描述不含安全语义载荷。
    #[test]
    fn disconnect_reasons_are_human_readable() {
        use xcontrol_proto::ErrCode;
        assert_eq!(
            describe_disconnect(&DisconnectReason::Timeout),
            "心跳超时（9s 无帧）"
        );
        let described = describe_disconnect(&DisconnectReason::PeerError {
            code: ErrCode::PROTOCOL_VIOLATION,
            msg: "违规".to_owned(),
        });
        assert!(described.contains("对端错误"));
    }

    /// 断开错误归一：Closed → 对端关闭文案。
    #[test]
    fn net_error_mapping() {
        assert!(
            disconnect_error("收帧失败", &NetError::Closed)
                .to_string()
                .contains("对端已关闭连接")
        );
    }

    /// set_connecting / report_connect_failure 的快照语义。
    #[tokio::test]
    async fn connecting_then_failure_reports_disconnected() {
        let bus = StateBus::new(AppState {
            role: ConfigRole::Client,
            ..AppState::default()
        });
        let mut rx = bus.subscribe();
        set_connecting(&bus);
        assert_eq!(rx.borrow().conn, ConnState::Connecting);

        let error = anyhow::anyhow!("安全码握手失败（原因已折叠）");
        report_connect_failure(&bus, &error);
        let state = rx.borrow_and_update().clone();
        assert!(matches!(state.conn, ConnState::Disconnected { .. }));
        assert!(state.last_error.is_some());
    }

    /// 边缘命中门控真值表（AC4，11-24 收紧）：仅 Running 未暂停 + **无人在控**
    /// （`ControlDir::None`）+ 无鼠标键按下才允许。`LocalControlsPeer`（本机正在
    /// 控制对端）由原放行改为**拒绝**——受控期间主控光标被锚定在切入处，屏边
    /// 命中无有意滑边，重复的 `EdgeCrossing` 会把工作机光标拉回进入点、释放按键、
    /// 重置回切免疫窗（docs/03 §5 死区，FR-2.1）；按键门控 = 11-22 第三轮拖拽
    /// 防护。
    #[test]
    fn edge_gate_truth_table() {
        let running = SessionState::Running { paused: false };
        let paused = SessionState::Running { paused: true };
        assert!(edge_hit_allowed(&running, ControlDir::None, false));
        assert!(
            !edge_hit_allowed(&running, ControlDir::LocalControlsPeer, false),
            "受控中（本机控制对端）主控边缘命中一律忽略"
        );
        assert!(
            !edge_hit_allowed(&running, ControlDir::PeerControlsLocal, false),
            "本机被控时不切入"
        );
        // 按键门控（11-22 第三轮）：任意鼠标键按下 → 不切入（上报被丢弃）。
        assert!(
            !edge_hit_allowed(&running, ControlDir::None, true),
            "鼠标键按下（拖拽中）不切换"
        );
        assert!(
            !edge_hit_allowed(&running, ControlDir::LocalControlsPeer, true),
            "受控中 + 按键按下：两道门控都是拒绝"
        );
        assert!(
            !edge_hit_allowed(&paused, ControlDir::None, false),
            "共享暂停不切换（FR-2.8）：切了也只是一条收端拒收输入的控制链"
        );
        assert!(!edge_hit_allowed(
            &SessionState::Idle,
            ControlDir::None,
            false
        ));
        assert!(!edge_hit_allowed(
            &SessionState::Handshaking,
            ControlDir::None,
            false
        ));
        assert!(!edge_hit_allowed(
            &SessionState::Disconnected(DisconnectReason::LocalQuit),
            ControlDir::None,
            false
        ));
    }

    /// 控制关系变化后的切入防抖（AC4，11-24；docs/03 §5 死区）：变化沿后
    /// 不足 150ms 的命中被拒绝，达到 150ms 起放行；无变化沿（会话从未发生过
    /// 控制关系变化）恒放行。
    #[test]
    fn edge_dead_time_rejects_hits_within_window_after_control_change() {
        const WINDOW: Duration = Duration::from_millis(150);
        assert_eq!(
            EDGE_SWITCH_DEAD_TIME, WINDOW,
            "防抖窗 = 控制关系变化后 150ms"
        );

        // 无变化沿：恒放行（空闲常态不防抖）。
        let now = Instant::now();
        assert!(
            edge_dead_time_elapsed(None, now),
            "本会话尚无控制关系变化沿 → 不防抖"
        );

        // 变化沿时刻起：窗内拒绝。
        let changed = Instant::now();
        assert!(
            !edge_dead_time_elapsed(Some(changed), changed),
            "变化当拍拒绝"
        );
        assert!(
            !edge_dead_time_elapsed(Some(changed), changed + WINDOW - Duration::from_millis(1)),
            "149ms：仍在窗内"
        );
        // 达到 150ms：放行。
        assert!(
            edge_dead_time_elapsed(Some(changed), changed + WINDOW),
            "150ms：窗口到期放行"
        );
        assert!(
            edge_dead_time_elapsed(Some(changed), changed + WINDOW + Duration::from_millis(1)),
            "151ms：窗外放行"
        );
    }

    /// 鼠标按键位图跟踪（11-22 第三轮按键门控的跟踪面）：Down 置位/Up 清位、
    /// 多键任一按下即门控、非按键事件不改动位图。
    #[test]
    fn track_mouse_buttons_down_up_bitmap() {
        use xcontrol_proto::input::KeyState as KS;
        let btn = |button, state| InputEvent::MouseButton { button, state };
        let mut bits = 0_u8;
        // 非按键事件：位图不动。
        track_mouse_buttons(&mut bits, &InputEvent::MouseMoveRel { dx: 1.0, dy: 0.0 });
        assert_eq!(bits, 0);
        // 左键按下 → 置位（门控生效）。
        track_mouse_buttons(&mut bits, &btn(MouseButton::Left, KS::Down));
        assert_eq!(bits, mouse_button_bit(MouseButton::Left));
        // 右键按下 → 双键位图（任一按下即门控）。
        track_mouse_buttons(&mut bits, &btn(MouseButton::Right, KS::Down));
        assert_eq!(
            bits,
            mouse_button_bit(MouseButton::Left) | mouse_button_bit(MouseButton::Right)
        );
        // 左键抬起 → 位图仅剩右键（多键语义：全释放才解除门控）。
        track_mouse_buttons(&mut bits, &btn(MouseButton::Left, KS::Up));
        assert_eq!(bits, mouse_button_bit(MouseButton::Right));
        // 全部抬起 → 位图清零（门控解除）。
        track_mouse_buttons(&mut bits, &btn(MouseButton::Right, KS::Up));
        assert_eq!(bits, 0);
        // 中键/后退/前进同面（docs/02 §4.2 button 表）。
        for button in [MouseButton::Middle, MouseButton::Back, MouseButton::Forward] {
            let mut bits = 0_u8;
            track_mouse_buttons(&mut bits, &btn(button, KS::Down));
            assert_eq!(bits, mouse_button_bit(button), "{button:?} 置位");
            track_mouse_buttons(&mut bits, &btn(button, KS::Up));
            assert_eq!(bits, 0, "{button:?} 清位");
        }
    }

    // —— 位置模型校准（11-24，R2/AC2） ——

    /// `is_preserved_entry`：只有"产出 `EnteredControl` 且无 `WarpPointer`"的
    /// 序列算保持原位切入。期望形态不手写——用真实会话状态机对
    /// `EnterScreen{preserve_cursor}` 两态产出的动作序列对拍；边缘切入（含
    /// 落点 warp）与切出序列不触发。
    #[test]
    fn preserved_entry_detection_matches_session_actions() {
        use xcontrol_core::clock::TestClock;
        use xcontrol_core::session::{FramePayload, SessionEvent};
        use xcontrol_proto::input::{Mods, ScreenEdge};
        use xcontrol_proto::{Caps, CtrlMsg, ProtoVer};

        let mut session = Session::new(
            Role::Client,
            Identity {
                name: "client".into(),
                screen_name: "client-screen".into(),
                instance_id: [1; 16],
                caps: Caps::UNICODE,
                salt: [7; 16],
            },
            TestClock::new(),
            Options::defaults(),
        );
        session
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .expect("就绪被接受");
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(CtrlMsg::Hello {
                proto: ProtoVer::CURRENT,
                name: "host".into(),
                screen_name: "host-screen".into(),
                instance_id: [2; 16],
                caps: Caps::UNICODE,
                salt: [9; 16],
            })))
            .expect("协商被接受");

        let enter = |preserve_cursor| InputEvent::EnterScreen {
            target: 1,
            edge: ScreenEdge::Left,
            ratio: 0.5,
            mods: Mods::default(),
            preserve_cursor,
        };

        // 边缘切入（preserve_cursor=0）：序列含落点 warp → 不触发校准。
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(enter(false))))
            .expect("EnterScreen 被接受");
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, SessionAction::WarpPointer { .. })),
            "边缘切入含落点 warp（实际 {actions:?}）"
        );
        assert!(!is_preserved_entry(&actions), "边缘切入不是保持原位切入");

        // 切出序列：无 EnteredControl → 不触发校准。
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::LeaveScreen {
                    mods: Mods::default(),
                },
            )))
            .expect("LeaveScreen 被接受");
        assert!(!is_preserved_entry(&actions), "切出序列不触发校准");

        // 保持原位切入（preserve_cursor=1）：有 EnteredControl、无 warp → 触发。
        let actions = session
            .step(SessionEvent::Frame(FramePayload::Input(enter(true))))
            .expect("EnterScreen 被接受");
        assert_eq!(
            actions,
            vec![SessionAction::Notice(Notice::EnteredControl { target: 1 })],
            "保持原位切入：仅对齐（本例无差异）+ 通知，无 warp（实际 {actions:?}）"
        );
        assert!(is_preserved_entry(&actions), "保持原位切入触发校准");

        assert!(!is_preserved_entry(&[]), "空序列不触发");
    }

    /// 位置模型校准钩子贯通**生产同构**注入链（11-24，AC2）：`drive_session`
    /// 的 sink 形态是 HotkeyFilterSink → MappingSink → Box<dyn InputSink + Send>
    /// ——`resync_cursor` 须经三层透传到达底层实现。保持原位切入的动作序列
    /// 触发一次校准，边缘切入（含 warp）不触发。
    #[test]
    fn resync_cursor_reaches_platform_sink_through_production_chain() {
        use std::sync::atomic::{AtomicU32, Ordering};

        /// 共享计数的底层 sink（装箱成 trait 对象后仍可读计数）。
        #[derive(Default)]
        struct SharedCountSink(Arc<AtomicU32>);

        impl InputSink for SharedCountSink {
            fn inject(
                &mut self,
                _event: &InputEvent,
            ) -> Result<(), xcontrol_core::sink::SinkError> {
                Ok(())
            }

            fn warp(
                &mut self,
                _screen: ScreenId,
                _x: f32,
                _y: f32,
            ) -> Result<(), xcontrol_core::sink::SinkError> {
                Ok(())
            }

            fn release_all(&mut self) -> Result<(), xcontrol_core::sink::SinkError> {
                Ok(())
            }

            fn resync_cursor(&mut self) -> Result<(), xcontrol_core::sink::SinkError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        let counter = Arc::new(AtomicU32::new(0));
        let boxed: Box<dyn InputSink + Send> = Box::new(SharedCountSink(Arc::clone(&counter)));
        let (hotkey_tx, _hotkey_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = HotkeyFilterSink::new(
            HotkeySet::empty(),
            build_mapping_sink(&KeymapConfig::default(), boxed),
            hotkey_tx,
        );

        // drive_session 收帧分支的同款判定：保持原位切入 → 校准一次。
        let preserved = vec![SessionAction::Notice(Notice::EnteredControl { target: 1 })];
        if is_preserved_entry(&preserved) {
            sink.resync_cursor().expect("生产链透传成功");
        }
        // 边缘切入（含落点 warp）→ 不校准。
        let edge_entry = vec![
            SessionAction::WarpPointer {
                screen: 0,
                x: 0.005,
                y: 0.5,
            },
            SessionAction::Notice(Notice::EnteredControl { target: 1 }),
        ];
        if is_preserved_entry(&edge_entry) {
            sink.resync_cursor().expect("生产链透传成功");
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "保持原位切入校准一次，边缘切入零次"
        );
    }

    // —— 热键回主控落点 = 切入锚点（11-24，R4/AC5） ——

    /// 真实 `ReleaseControl` 序列（core 状态机产出，期望形态不手写）：
    /// `ReleaseAll` + `LeaveScreen` + 落点 warp。
    fn host_release_control_actions() -> Vec<SessionAction> {
        use xcontrol_core::clock::TestClock;
        use xcontrol_core::session::{FramePayload, SessionEvent};
        use xcontrol_proto::input::ScreenEdge;
        use xcontrol_proto::{Caps, CtrlMsg, ProtoVer};

        let mut session = Session::new(
            Role::Host,
            Identity {
                name: "host".into(),
                screen_name: "host-screen".into(),
                instance_id: [2; 16],
                caps: Caps::UNICODE,
                salt: [9; 16],
            },
            TestClock::new(),
            Options::defaults(),
        );
        session
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .expect("就绪被接受");
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(
                CtrlMsg::HelloBack {
                    proto: ProtoVer::CURRENT,
                    name: "client".into(),
                    instance_id: [1; 16],
                    caps: Caps::UNICODE,
                },
            )))
            .expect("协商被接受");
        session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            }))
            .expect("EdgeCrossing 被接受");
        assert_eq!(session.control_dir(), ControlDir::LocalControlsPeer);
        session
            .step(SessionEvent::Local(LocalCmd::ReleaseControl))
            .expect("ReleaseControl 被接受")
    }

    /// macOS 主控热键回主控（AC5）：core 序列里的落点 warp 被剥离；替代落点 =
    /// 切入锚点。锚点不可得时替代落点为空（光标停在当前位置）；suppress 尚未
    /// 翻开（接管后一个输入拍内就按了回主控）或未剥离到时不动重放缓存。
    #[test]
    fn home_hotkey_strips_core_landing_and_replays_anchor() {
        let actions = host_release_control_actions();
        // core 序列形态自证：含且仅含一条落点 warp（主屏内侧 0.5/0.6）。
        assert_eq!(
            actions,
            vec![
                SessionAction::ReleaseAll,
                actions[1].clone(),
                SessionAction::WarpPointer {
                    screen: 0,
                    x: 0.5,
                    y: 0.6
                },
            ],
            "core 的 ReleaseControl 序列（实际 {actions:?}）"
        );
        assert!(matches!(
            actions[1],
            SessionAction::Send {
                msg: xcontrol_core::session::Outbound::Input(_),
                ..
            }
        ));

        // 剥离：只剩释放序列（ReleaseAll + LeaveScreen），不残留 warp。
        let mut stripped = actions.clone();
        assert!(strip_home_warp(&mut stripped), "剥离到落点 warp");
        assert_eq!(stripped, vec![actions[0].clone(), actions[1].clone()]);
        assert!(
            stripped
                .iter()
                .all(|action| !matches!(action, SessionAction::WarpPointer { .. }))
        );
        // 序列已无 warp / 空序列：未剥离到，序列不变。
        assert!(!strip_home_warp(&mut stripped), "无 warp 可剥");
        assert_eq!(stripped.len(), 2);
        assert!(!strip_home_warp(&mut Vec::new()));

        // 替代落点 = 切入锚点（受控期间所见位置）。
        let mut pending = None;
        assert!(apply_home_landing(
            &mut pending,
            true,
            true,
            Some((120.0, 340.0))
        ));
        assert_eq!(pending, Some((120.0, 340.0)), "重放点 = 锚点");

        // 锚点不可得 → 替代落点为空（清掉残留值）：光标停物理鼠标位。
        let mut pending = Some((7.0, 8.0));
        assert!(apply_home_landing(&mut pending, true, true, None));
        assert_eq!(pending, None, "无锚点：重放点为空");

        // suppress 尚未翻开：只剥离，不动重放缓存（没有翻拍承接重放）。
        let mut pending = None;
        assert!(!apply_home_landing(
            &mut pending,
            true,
            false,
            Some((120.0, 340.0))
        ));
        assert_eq!(pending, None);

        // 未剥离到：不动重放缓存。
        let mut pending = Some((1.0, 2.0));
        assert!(!apply_home_landing(
            &mut pending,
            false,
            true,
            Some((120.0, 340.0))
        ));
        assert_eq!(pending, Some((1.0, 2.0)));
    }

    // —— 回切行程补偿（11-24，R8 / AC10） ——

    /// 真实边缘回切序列（core 状态机产出，期望形态不手写）：`[主控 | 工作机]`
    /// 布局，主控右缘切出（回切触发边 = 工作机左缘）→ 免疫窗过后朝主控方向武装
    /// → 工作机回报（`report_x` 可为端点之外的负值）触发回切。
    fn host_edge_return_actions(report_x: f32) -> Vec<SessionAction> {
        use xcontrol_core::clock::TestClock;
        use xcontrol_core::session::{FramePayload, SessionEvent};
        use xcontrol_proto::input::ScreenEdge;
        use xcontrol_proto::{Caps, CtrlMsg, ProtoVer, ScreenInfo};

        let host_id = [2; 16];
        // 虚拟时钟双句柄：会话持一份、本测试持一份（回切免疫窗按时间计，推进
        // 测试句柄即越过 800ms 窗）。
        let clock = TestClock::new();
        let mut session = Session::new(
            Role::Host,
            Identity {
                name: "host".into(),
                screen_name: "host-screen".into(),
                instance_id: host_id,
                caps: Caps::UNICODE,
                salt: [9; 16],
            },
            clock.clone(),
            Options::defaults(),
        );
        session
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .expect("就绪被接受");
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(
                CtrlMsg::HelloBack {
                    proto: ProtoVer::CURRENT,
                    name: "client".into(),
                    instance_id: [1; 16],
                    caps: Caps::UNICODE,
                },
            )))
            .expect("协商被接受");
        let screen = |id, owner, left, right| ScreenInfo {
            id,
            owner,
            name: format!("screen-{id}"),
            left,
            right,
            top: None,
            bottom: None,
            scale: 1.0,
        };
        session.set_layout(ScreenLayout::new(vec![
            screen(0, host_id, None, Some(1)),
            screen(1, [1; 16], Some(0), None),
        ]));
        session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            }))
            .expect("EdgeCrossing 被接受");
        // 回切免疫窗（接管后 800ms，按时间计）：推进虚拟时钟越过。
        clock.advance(800);
        // 朝主控方向（左）移动 → 同向武装。
        session
            .step(SessionEvent::Local(LocalCmd::ForwardInput(
                InputEvent::MouseMoveRel { dx: -3.0, dy: 0.0 },
            )))
            .expect("ForwardInput 被接受");
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: report_x,
                    y: 0.4,
                },
            )))
            .expect("回报帧被接受")
    }

    /// `edge_return_of` + `compensated_release_point` 端到端（AC10）：真实回切序列
    /// 解出落点素材；落点补偿 = 原落点 + （回报行程 + 在途批 + 排空批）。工作机
    /// 顶边 120 点、在途 8 点、排空 2 点 → 回主控后落点从右缘内侧向行进方向（左）
    /// 前移 130 点，与手势连续；旧工作机（端点值回报，行程 0）落点不变。
    #[test]
    fn edge_return_landing_is_compensated_by_lost_travel() {
        use xcontrol_core::session::EDGE_RETURN_INSET;

        use crate::edge::ScreenRect;

        // 主控双屏各 1000×1000 点；落点在屏 0（面向工作机的那块）。
        let rects = [
            ScreenRect {
                origin: (0.0, 0.0),
                size: (1000.0, 1000.0),
            },
            ScreenRect {
                origin: (1000.0, 0.0),
                size: (1000.0, 1000.0),
            },
        ];
        let base_x = f64::from(1.0 - EDGE_RETURN_INSET) * 1000.0;

        // 越界回报（工作机左缘被推出 120 点）：行程为负（向左）。
        let actions = host_edge_return_actions(-120.0);
        let landing = edge_return_of(&actions).expect("边缘回切序列应被识别");
        assert_eq!(
            landing,
            EdgeReturnLanding {
                screen: 0,
                x: 1.0 - EDGE_RETURN_INSET,
                y: 0.4,
                report_travel: (-120.0, 0.0),
            }
        );
        // 在途批（2ms 前发出，窗内）计入；排空批 2 点。
        let in_flight = in_flight_batch(Some((Instant::now(), (-8.0, 1.0))), Instant::now());
        let drained = sum_mouse_deltas(&[
            InputEvent::MouseMoveRel { dx: -2.0, dy: 0.0 },
            InputEvent::Key {
                code: KEY_C,
                state: KeyState::Down,
            },
        ]);
        assert_eq!((in_flight, drained), ((-8.0, 1.0), (-2.0, 0.0)));
        let travel = (
            landing.report_travel.0 + in_flight.0 + drained.0,
            landing.report_travel.1 + in_flight.1 + drained.1,
        );
        let (px, py) = crate::edge::compensated_release_point(
            &rects,
            landing.screen,
            landing.x,
            landing.y,
            travel,
        )
        .expect("合法落点");
        assert!((px - (base_x - 130.0)).abs() < 1e-3, "落点 x = {px}");
        assert!((py - 401.0).abs() < 1e-3, "落点 y = {py}");

        // 旧工作机 / macOS / X11 工作机（端点值回报）：行程 0，无在途/排空批 →
        // 落点与未补偿的原落点一致（行为与 11-24 之前相同）。
        let actions = host_edge_return_actions(0.0);
        let landing = edge_return_of(&actions).expect("边缘回切序列应被识别");
        assert_eq!(landing.report_travel, (0.0, 0.0));
        let (px, py) = crate::edge::compensated_release_point(
            &rects,
            landing.screen,
            landing.x,
            landing.y,
            (0.0, 0.0),
        )
        .expect("合法落点");
        assert!((px - base_x).abs() < 1e-3 && (py - 400.0).abs() < 1e-3);
    }

    /// `edge_return_of` 的识别面：只有同时含回切通知与落点 warp 的序列才算边缘
    /// 回切——热键回主控序列（有 warp、无通知）、切入序列、空序列均返回 `None`
    /// （热键回主控的落点策略是切入锚点，不得被当成边缘回切补偿）。
    #[test]
    fn edge_return_detection_requires_notice_and_warp() {
        // 热键回主控（真实序列）：ReleaseAll + LeaveScreen + 落点 warp，无回切通知。
        let home = host_release_control_actions();
        assert!(
            home.iter()
                .any(|action| matches!(action, SessionAction::WarpPointer { .. }))
        );
        assert_eq!(edge_return_of(&home), None, "热键回主控不是边缘回切");
        // 只有通知、没有 warp（防御：不完整的序列不补偿）。
        assert_eq!(
            edge_return_of(&[SessionAction::Notice(Notice::EdgeReturnedToHost {
                target: 1,
                overflow: (5.0, 0.0),
            })]),
            None
        );
        // 只有 warp、没有通知。
        assert_eq!(
            edge_return_of(&[SessionAction::WarpPointer {
                screen: 0,
                x: 0.5,
                y: 0.5
            }]),
            None
        );
        assert_eq!(edge_return_of(&[]), None);
    }

    /// 在途批判定（AC10）：发出时刻距回切触发不足 3ms 的批计入，达到 3ms 不计；
    /// 尚未发出过批 → 零位移。
    #[test]
    fn in_flight_batch_counts_only_batches_sent_within_window() {
        assert_eq!(
            IN_FLIGHT_WINDOW,
            Duration::from_millis(3),
            "在途批窗口 = 3ms"
        );
        assert_eq!(in_flight_batch(None, Instant::now()), (0.0, 0.0));
        let now = Instant::now();
        // 窗内（2ms 前）：整批位移计入。
        assert_eq!(
            in_flight_batch(Some((now - Duration::from_millis(2), (12.5, -3.0))), now),
            (12.5, -3.0)
        );
        // 窗口边界：不足 3ms 计入，达到 3ms 不计。
        assert_eq!(
            in_flight_batch(Some((now - Duration::from_millis(3), (12.5, -3.0))), now),
            (0.0, 0.0)
        );
        assert_eq!(
            in_flight_batch(Some((now - Duration::from_millis(10), (12.5, -3.0))), now),
            (0.0, 0.0),
            "上一个 10ms 拍发出的批早已到达，不计"
        );
        // 零位移批（纯键盘批）在窗内也计入——计出来的就是零。
        assert_eq!(
            in_flight_batch(Some((now - Duration::from_millis(1), (0.0, 0.0))), now),
            (0.0, 0.0)
        );
    }

    /// 回放切片（11-25 第二轮）：各步之和等于"补偿终点 − 基点"；匀速微步、
    /// 步数有界（≤ 30）；末步补齐余量；`pop()` 出步顺序即回放顺序。
    #[test]
    fn replay_slices_sum_to_travel_uniform_steps() {
        let base = Some((100.0, 200.0));
        let compensated = Some((220.0, 176.0));
        let mut queue = replay_slices(compensated, base);
        assert!(!queue.is_empty(), "非零位移应有切片");
        let steps: Vec<(f64, f64)> = core::iter::from_fn(|| queue.pop()).collect();
        let (sum_x, sum_y) = steps
            .iter()
            .fold((0.0, 0.0), |(x, y), &(dx, dy)| (x + dx, y + dy));
        assert!(
            (sum_x - 120.0).abs() < 1e-9,
            "x 各步之和 = 120，实际 {sum_x}"
        );
        assert!(
            (sum_y + 24.0).abs() < 1e-9,
            "y 各步之和 = −24，实际 {sum_y}"
        );
        // 匀速：各步同向、长度相等；末步把剩余量补齐，误差即浮点量级。
        for pair in steps.windows(2) {
            assert!(
                (pair[0].0 - pair[1].0).abs() < 1e-6 && pair[0].0 > 0.0,
                "除末步外步长应相等且同向：{pair:?}"
            );
        }
        // 步数 ≤ MAX_STEPS（总时长 ≤ 60ms）。
        assert!(
            steps.len() <= RETURN_REPLAY_MAX_STEPS,
            "步数有界：{}",
            steps.len()
        );
    }

    /// 大行程：步数贴着上限（步长等比例放大），总时长仍有界，各步之和守恒。
    #[test]
    fn replay_slices_large_travel_capped_step_count() {
        let mut queue = replay_slices(Some((10_060.0, 200.0)), Some((100.0, 200.0)));
        let steps: Vec<(f64, f64)> = core::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(
            steps.len(),
            RETURN_REPLAY_MAX_STEPS,
            "大行程步数贴上限（60ms 总时长有界）"
        );
        let (sum_x, _) = steps
            .iter()
            .fold((0.0, 0.0), |(x, y), &(dx, dy)| (x + dx, y + dy));
        assert!((sum_x - 9_960.0).abs() < 1e-6, "各步之和守恒：{sum_x}");
        // 除末步外步长相等（匀速放大形态）。
        for pair in steps.windows(2) {
            assert!((pair[0].0 - pair[1].0).abs() < 1e-6, "匀速：{pair:?}");
        }
        // 单步 ≥ MIN_STEP（防饿死下限）。
        for &(dx, _) in &steps {
            assert!(dx >= RETURN_REPLAY_MIN_STEP - 1e-9, "步长下限：{dx}");
        }
    }

    /// 回放切片的不回放情形：零位移、终点或基点缺失、非有限位移。
    #[test]
    fn replay_slices_empty_when_nothing_to_replay() {
        assert!(replay_slices(Some((50.0, 50.0)), Some((50.0, 50.0))).is_empty());
        assert!(replay_slices(None, Some((50.0, 50.0))).is_empty());
        assert!(replay_slices(Some((60.0, 50.0)), None).is_empty());
        assert!(replay_slices(Some((f64::NAN, 50.0)), Some((50.0, 50.0))).is_empty());
    }

    /// 滚轮/其他事件不计；空批为零。
    #[test]
    fn sum_mouse_deltas_accumulates_only_relative_moves() {
        let key = || InputEvent::Key {
            code: KEY_C,
            state: KeyState::Down,
        };
        let button = || InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        };
        let wheel = || InputEvent::Wheel {
            axis: xcontrol_proto::input::Axis::Vertical,
            delta: 2.0,
        };
        let batch = [
            InputEvent::MouseMoveRel { dx: 3.0, dy: -1.0 },
            key(),
            InputEvent::MouseMoveRel {
                dx: -10.5,
                dy: 0.25,
            },
            button(),
            wheel(),
            InputEvent::MouseMoveRel { dx: 0.0, dy: 4.0 },
        ];
        let sum = sum_mouse_deltas(&batch);
        assert!(
            (sum.0 - -7.5).abs() < 1e-6 && (sum.1 - 3.25).abs() < 1e-6,
            "{sum:?}"
        );
        assert_eq!(sum_mouse_deltas(&batch[..1]), (3.0, -1.0));
        assert_eq!(sum_mouse_deltas(&batch[..0]), (0.0, 0.0), "空批零位移");
        let no_moves = [key(), button(), wheel()];
        assert_eq!(
            sum_mouse_deltas(&no_moves),
            (0.0, 0.0),
            "纯键盘/按键/滚轮批零位移"
        );
    }

    /// 捕获源恢复探测的节流（11-24，R9）：未探测过或距上次探测满 1s → 到期；
    /// 间隔内不到期（授权后约 1s 内恢复，未授权期间不刷日志）。
    #[test]
    fn capture_probe_is_throttled_to_retry_interval() {
        assert_eq!(
            CAPTURE_RETRY_INTERVAL,
            Duration::from_secs(1),
            "恢复探测间隔 = 1s"
        );
        let now = Instant::now();
        assert!(capture_probe_due(None, now), "首次探测立即到期");
        assert!(
            !capture_probe_due(Some(now - Duration::from_millis(999)), now),
            "999ms：间隔未满"
        );
        assert!(
            capture_probe_due(Some(now - Duration::from_millis(1000)), now),
            "1000ms：到期"
        );
    }

    /// 捕获源重建失败告警的退避（11-25）：首次失败立即告警；之后不足 30s 不再告警
    /// （重试仍每秒一次，只是不刷日志），满 30s 再告警一条。
    #[test]
    fn capture_failure_log_backs_off_to_thirty_seconds() {
        assert_eq!(
            CAPTURE_FAILURE_LOG_INTERVAL,
            Duration::from_secs(30),
            "重建失败告警的退避间隔 = 30s"
        );
        let now = Instant::now();
        assert!(capture_failure_log_due(None, now), "首次失败立即告警");
        assert!(
            !capture_failure_log_due(Some(now - Duration::from_secs(1)), now),
            "已授权但持续失败：第 2 秒的探测不重复告警"
        );
        assert!(
            !capture_failure_log_due(Some(now - Duration::from_secs(29)), now),
            "29s：仍不告警"
        );
        assert!(
            capture_failure_log_due(Some(now - Duration::from_secs(30)), now),
            "30s：再告警一条"
        );
        // 探测间隔远小于告警间隔：告警退避期间每秒仍在重试。
        assert!(CAPTURE_RETRY_INTERVAL < CAPTURE_FAILURE_LOG_INTERVAL);
    }

    /// 捕获源恢复的步骤决策（11-25）：未被控直接重建；名义上受控先释放；名义上
    /// 受控且共享暂停中推迟（暂停期间对端拒收输入帧，此时释放会让两端失配）。
    #[test]
    fn capture_recovery_releases_first_and_defers_while_paused() {
        assert_eq!(
            capture_recovery_step(ControlDir::None, false),
            CaptureRecoveryStep::Rebuild,
            "未被控：直接重建"
        );
        assert_eq!(
            capture_recovery_step(ControlDir::PeerControlsLocal, false),
            CaptureRecoveryStep::Rebuild,
            "本机被控与主控捕获源恢复无关"
        );
        assert_eq!(
            capture_recovery_step(ControlDir::LocalControlsPeer, false),
            CaptureRecoveryStep::ReleaseThenRebuild,
            "名义上受控：先释放再重建"
        );
        assert_eq!(
            capture_recovery_step(ControlDir::LocalControlsPeer, true),
            CaptureRecoveryStep::DeferWhilePaused,
            "名义上受控 + 共享暂停：推迟"
        );
        assert_eq!(
            capture_recovery_step(ControlDir::None, true),
            CaptureRecoveryStep::Rebuild,
            "未被控时暂停不影响重建"
        );
    }

    /// 捕获源恢复前释放名义上的控制关系（11-25）：真实状态机的 `ReleaseControl`
    /// 序列保留 `ReleaseAll` 与 `LeaveScreen`（照常下发），**落点 warp 被剥离**
    /// （光标不动），控制关系终结；未被控时产出空序列。
    #[test]
    fn capture_recovery_releases_nominal_control_without_landing_warp() {
        use xcontrol_core::clock::TestClock;
        use xcontrol_core::session::{FramePayload, Outbound, SessionEvent};
        use xcontrol_proto::input::ScreenEdge;
        use xcontrol_proto::{Caps, CtrlMsg, ProtoVer};

        let mut session = Session::new(
            Role::Host,
            Identity {
                name: "host".into(),
                screen_name: "host-screen".into(),
                instance_id: [2; 16],
                caps: Caps::UNICODE,
                salt: [9; 16],
            },
            TestClock::new(),
            Options::defaults(),
        );
        session
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .expect("就绪被接受");
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(
                CtrlMsg::HelloBack {
                    proto: ProtoVer::CURRENT,
                    name: "client".into(),
                    instance_id: [1; 16],
                    caps: Caps::UNICODE,
                },
            )))
            .expect("协商被接受");

        // 未被控（授权前没有切入）：空序列，调用方不必发送。
        assert_eq!(session.control_dir(), ControlDir::None);
        assert!(
            release_control_before_capture_recovery(&mut session).is_empty(),
            "未被控时无动作"
        );

        // 未授权期间靠边缘传感器切入（不需要权限）：名义上处于受控态。
        session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            }))
            .expect("EdgeCrossing 被接受");
        assert_eq!(session.control_dir(), ControlDir::LocalControlsPeer);

        // 恢复前释放：释放序列照常，落点 warp 剥离。
        let actions = release_control_before_capture_recovery(&mut session);
        assert_eq!(
            actions.len(),
            2,
            "ReleaseAll + LeaveScreen（实际 {actions:?}）"
        );
        assert_eq!(actions[0], SessionAction::ReleaseAll);
        assert!(
            matches!(
                &actions[1],
                SessionAction::Send {
                    msg: Outbound::Input(InputEvent::LeaveScreen { .. }),
                    ..
                }
            ),
            "LeaveScreen 照常产出（实际 {:?}）",
            actions[1]
        );
        assert!(
            actions
                .iter()
                .all(|action| !matches!(action, SessionAction::WarpPointer { .. })),
            "落点 warp 被剥离——光标不动"
        );
        assert_eq!(session.control_dir(), ControlDir::None, "控制关系已终结");
    }

    /// ControlDir 生命周期 × 门控（PRD AC「被控时不轮询触发，LeaveScreen 恢复」）：
    /// 用真实 host 会话状态机走 EdgeCrossing → 对端 EnterScreen → LeaveScreen
    /// → Pause，每步断言门控结果与 ControlDir 一致。
    #[test]
    fn edge_gate_follows_control_dir_lifecycle() {
        use xcontrol_core::session::{FramePayload, SessionEvent};
        use xcontrol_proto::input::{InputEvent, Mods, ScreenEdge};
        use xcontrol_proto::{Caps, CtrlMsg, ProtoVer};

        let mut session = Session::new(
            Role::Host,
            Identity {
                name: "host".into(),
                screen_name: "host-screen".into(),
                instance_id: [2; 16],
                caps: Caps::UNICODE,
                salt: [9; 16],
            },
            SystemClock::new(),
            Options::defaults(),
        );
        session
            .step(SessionEvent::HandshakeReady { hs_hash: [0; 32] })
            .expect("就绪被接受");
        let back = CtrlMsg::HelloBack {
            proto: ProtoVer::CURRENT,
            name: "client".into(),
            instance_id: [1; 16],
            caps: Caps::UNICODE,
        };
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(back)))
            .expect("协商被接受");

        // ① Running + None：允许（常态滑边）
        assert_eq!(session.control_dir(), ControlDir::None);
        assert!(edge_hit_allowed(
            session.state(),
            session.control_dir(),
            false
        ));

        // ② 本机切出（ReleaseAll + LeaveScreen + EnterScreen 出站）：
        //    LocalControlsPeer —— **拒绝**（11-24：受控期间主控光标被锚定在切入
        //    处，屏边命中无有意滑边——重复 EdgeCrossing 会把工作机光标拉回进入
        //    点、释放按键、重置回切免疫窗；回切走状态机的回报判定，不经此门）。
        let actions = session
            .step(SessionEvent::Local(LocalCmd::EdgeCrossing {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
            }))
            .expect("EdgeCrossing 被接受");
        assert!(matches!(actions.first(), Some(SessionAction::ReleaseAll)));
        assert_eq!(session.control_dir(), ControlDir::LocalControlsPeer);
        assert!(
            !edge_hit_allowed(session.state(), session.control_dir(), false),
            "受控中（本机控制对端）主控边缘命中一律忽略"
        );

        // ③ 对端 EnterScreen → 本机被控（PeerControlsLocal）：门控拒绝
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::EnterScreen {
                    target: 0,
                    edge: ScreenEdge::Left,
                    ratio: 0.5,
                    mods: Mods::default(),
                    preserve_cursor: false,
                },
            )))
            .expect("EnterScreen 被接受");
        assert_eq!(session.control_dir(), ControlDir::PeerControlsLocal);
        assert!(!edge_hit_allowed(
            session.state(),
            session.control_dir(),
            false
        ));

        // ④ 对端 LeaveScreen → 复位 None：门控恢复（PRD 收敛项 3）
        session
            .step(SessionEvent::Frame(FramePayload::Input(
                InputEvent::LeaveScreen {
                    mods: Mods::default(),
                },
            )))
            .expect("LeaveScreen 被接受");
        assert_eq!(session.control_dir(), ControlDir::None);
        assert!(edge_hit_allowed(
            session.state(),
            session.control_dir(),
            false
        ));

        // ⑤ 共享暂停：门控拒绝（FR-2.8）
        session
            .step(SessionEvent::Local(LocalCmd::Pause))
            .expect("暂停被接受");
        assert!(!edge_hit_allowed(
            session.state(),
            session.control_dir(),
            false
        ));
    }

    // —— 热生效接线（11-21 反馈 3/4，docs/06 §4.1）——

    /// `apply_hot_key_configs`（会话内 `RuntimeConfigUpdate` 消费点的热替换）：
    /// 新热键表立即生效（旧组合失活/新组合命中路由通道）+ 映射规则热替换
    /// （mac2win 改写到达注入层）。
    #[test]
    fn apply_hot_key_configs_swaps_hotkeys_and_mapping_rules() {
        use xcontrol_core::hotkey::HotkeyId;
        use xcontrol_proto::xkey::{KEY_END, KEY_LEFT_ALT, KEY_RIGHT};

        let (hotkey_tx, mut hotkey_rx) = tokio::sync::mpsc::unbounded_channel::<HotkeyId>();
        // 注入链与 drive_session 同构（HotkeyFilterSink → MappingSink → 记录型
        // sink），内层用具体 RecordingInputSink 便于断言到达事件。
        let default_keymap = KeymapConfig::default();
        let engine = MappingEngine::new(
            &default_keymap.preset,
            &default_keymap.rules,
            default_keymap.passthrough,
        )
        .expect("默认 keymap 编译");
        let mut sink = HotkeyFilterSink::new(
            HotkeySet::from_config(&HotkeysConfig::default()).expect("默认热键编译"),
            MappingSink::new(engine, RecordingInputSink::new()),
            hotkey_tx,
        );
        #[cfg(feature = "macos")]
        let mut capture_filter = xcontrol_core::hotkey::HotkeyFilter::new(
            HotkeySet::from_config(&HotkeysConfig::default()).expect("默认热键编译"),
        );
        let mut source = crate::hotkey_rt::PlatformHotkeySource::register(
            &HotkeySet::from_config(&HotkeysConfig::default()).expect("默认热键编译"),
        );

        // 新配置：next → Ctrl+Alt+End（替换默认 Ctrl+Alt+Right）；keymap → mac2win。
        let mut config = Config::default();
        config.hotkeys.next = "Ctrl+Alt+KEY_END".to_owned();
        config.keymap.preset = "mac2win".to_owned();
        apply_hot_key_configs(
            &config,
            &mut sink,
            #[cfg(feature = "macos")]
            &mut capture_filter,
            &mut source,
        );

        // ① 旧组合失活：Ctrl+Alt+Right 未命中（转发进映射层记录）。
        let down = |code| InputEvent::Key {
            code,
            state: KeyState::Down,
        };
        let up = |code| InputEvent::Key {
            code,
            state: KeyState::Up,
        };
        for event in [down(KEY_LEFT_CTRL), down(KEY_LEFT_ALT), down(KEY_RIGHT)] {
            sink.inject(&event).expect("注入成功");
        }
        assert!(
            hotkey_rx.try_recv().is_err(),
            "旧组合 Ctrl+Alt+Right 应已失活"
        );
        // ② 新组合命中：Ctrl+Alt+End → Next 路由通道。
        sink.inject(&down(KEY_END)).expect("注入成功");
        assert_eq!(hotkey_rx.try_recv(), Ok(HotkeyId::Next), "新组合立即生效");
        for event in [up(KEY_END), up(KEY_LEFT_ALT), up(KEY_LEFT_CTRL)] {
            sink.inject(&event).expect("注入成功");
        }
        // ③ 映射规则已替换：新注入 LMeta↓ 经 mac2win 改写为 LCtrl↓（替换前
        // 默认恒等预设原样记录）。
        sink.inject(&down(KEY_LEFT_META)).expect("注入成功");
        let recorded = sink.inner_mut().inner().events();
        assert_eq!(
            recorded.last(),
            Some(&InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Down,
            }),
            "mac2win 改写即时生效（记录末条 = LCtrl↓）；全部记录：{recorded:?}"
        );
        // ④ 编译失败保留旧表：坏热键配置 → Ctrl+Alt+End 仍命中（旧表延续）。
        // 先释放 ③ 按下的 LMeta（修饰键跟踪延续——精确集合语义下须排空）。
        sink.inject(&up(KEY_LEFT_META)).expect("注入成功");
        let mut broken = Config::default();
        broken.hotkeys.next = "Ctrl++Bogus".to_owned();
        apply_hot_key_configs(
            &broken,
            &mut sink,
            #[cfg(feature = "macos")]
            &mut capture_filter,
            &mut source,
        );
        sink.inject(&down(KEY_LEFT_CTRL)).expect("注入成功");
        sink.inject(&down(KEY_LEFT_ALT)).expect("注入成功");
        sink.inject(&down(KEY_END)).expect("注入成功");
        assert_eq!(
            hotkey_rx.try_recv(),
            Ok(HotkeyId::Next),
            "坏配置保留旧热键表（输入管线不中断，error-handling.md 规则 4）"
        );
    }

    // —— 受控回报探针（11-22 边缘回切修复轮） ——

    /// `scan_controlled_report`：解出流中最后一条 MouseMoveAbs 回报（归一化
    /// 值原样）；混杂其他 kind / 半帧尾不干扰；无回报 → None。
    #[test]
    fn scan_controlled_report_decodes_last_abs() {
        use xcontrol_proto::Message;
        let abs = |screen: ScreenId, x: f32, y: f32| InputEvent::MouseMoveAbs { screen, x, y };
        let mut stream = Vec::new();
        // 首条回报 + 一条无关 Key 帧 + 次条回报（应取次条）。
        abs(7, 0.25, 0.75)
            .encode_frame_into(1, &mut stream)
            .expect("回报帧编码");
        InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        }
        .encode_frame_into(2, &mut stream)
        .expect("Key 帧编码");
        abs(7, 0.9985, 0.5)
            .encode_frame_into(3, &mut stream)
            .expect("次条回报帧编码");
        assert_eq!(
            scan_controlled_report(&stream),
            Some((7, 0.9985, 0.5)),
            "应取流中最后一条回报"
        );
        // 半帧尾（截断，尾帧不可解）：最后一条**可解**回报仍在——前导完整帧
        // 照常解出（剩余字节交 dispatcher 判定的同款防御语义）。
        let truncated = &stream[..stream.len() - 2];
        assert_eq!(scan_controlled_report(truncated), Some((7, 0.25, 0.75)));
        // 无回报（仅 Key 帧）→ None。
        let mut keys_only = Vec::new();
        InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down,
        }
        .encode_frame_into(1, &mut keys_only)
        .expect("Key 帧编码");
        assert_eq!(scan_controlled_report(&keys_only), None);
        // 空流 → None。
        assert_eq!(scan_controlled_report(&[]), None);
    }
}
