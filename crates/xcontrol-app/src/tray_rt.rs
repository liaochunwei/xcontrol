//! `tray_rt` — 托盘运行时接线（FR-6.1，docs/06 §5）。
//!
//! ```text
//! StateBus 快照 ──(200ms 循环)──► AppState → TraySnapshot ──► TrayController.update
//!                                                  （图标/角标/tooltip/菜单 ≤1s 反映，FR-6.1 AC）
//! 托盘菜单 action（gpui-tray 原生分发，GPUI 主线程）
//!   ├─ TogglePause       → CoreCommand::Pause/Resume（按当前暂停态取反）
//!   ├─ SwitchRole        → CoreCommand::SwitchRole（运行时 supervisor 翻转重建）
//!   ├─ SwitchToSlave     → CoreCommand::SwitchToSlave（FR-2.5 直接切换）
//!   ├─ DisconnectRescan  → CoreCommand::Disconnect（从机回环退避重扫）
//!   ├─ OpenSettings      → ui::open_main_or_quit（M2 窗口唤起）
//!   ├─ ToggleAutostart   → Autostart enable/disable（OS 级，不经核心命令）
//!   └─ QuitApp           → TrayController.close + cx.quit()
//! ```
//!
//! 纯投影红线（architecture-rules.md 红线 4 / ui-tray.md）：本模块只读
//! [`StateBus`] 快照（`current()`，非阻塞纯读）与发 [`CoreCommand`]；图标/菜单
//! 不持有核心状态（xcontrol-tray 投影层为纯函数）；action 处理器是唯一允许
//! 触碰命令通道的地方。
//!
//! `Tray` 非 `Send`（gpui 主线程绑定）——托盘构建/更新/销毁全部发生在 gpui
//! executor 主线程（菜单 action 天然落主线程，快照循环经实体 update 回主线程，
//! ui::RootView 刷新循环同形——`WeakEntity::update` 返回 `Result`，app 释放
//! 后循环自然退出，规避 `AsyncApp::update` 的 released-panic 路径）。托盘装配
//! 失败（状态栏资源不可用等）告警降级为无托盘运行（error-handling.md 规则 4
//! ——窗口路径不因托盘中断）。

use std::sync::Arc;

use gpui_kit::{App, AppContext, Entity, Global};
use xcontrol_config::Role;
use xcontrol_tray::project::{SlaveInfo, TrayRole, TraySnapshot};
use xcontrol_tray::{Autostart, SnapshotProvider, TrayController};

use crate::logging::LogBuffer;
use crate::state::{AppState, ConnState, CoreCommand, CoreHandle, SNAPSHOT_PERIOD, StateBus};
use crate::ui;

/// 托盘全局存档（gpui 主线程单例）：
///
/// - `controller`：quit 路径 close 用；
/// - `refresher`：快照刷新实体的**强引用**——实体无窗口/视图持有，必须有人
///   持强引用保活（11-18-m4-tray AX 实测教训：强引用只活在 install 栈帧时，
///   实体在首拍前即释放，刷新循环立刻以 "entity released" 收敛——RootView
///   无此问题因其被窗口持有）。
///
/// 快照刷新实体持有另一个控制器 Clone 作唯一 update 写者（xcontrol-tray
/// tray.rs 单写者纪律）；实体随 app 释放——刷新循环经 `WeakEntity::update`
/// 的 `Err` 分支自然收敛，RootView 同形。
struct TrayGlobal {
    controller: TrayController,
    /// 保活强引用（有意不读——实体仅凭持有存活；释放语义 = app 退出时全局
    /// 随之释放，刷新循环经 `WeakEntity::update` 的 `Err` 收敛）。
    #[expect(dead_code)]
    refresher: Entity<TrayRefresher>,
}

impl Global for TrayGlobal {}

/// 快照刷新实体：托盘控制器的存活宿主（强引用存于 [`TrayGlobal`]）。
struct TrayRefresher {
    controller: TrayController,
}

/// 装配托盘（窗口模式主回路调用）：action 处理器注册 + 控制器构建 + 快照
/// 刷新循环。`params` = 窗口装配参数（「打开设置…」复用同一开窗策略——向导态
/// 打开的是向导窗，11-18-m4-ui）。
pub fn install(
    cx: &mut App,
    bus: &Arc<StateBus>,
    core: &CoreHandle,
    logs: &LogBuffer,
    params: &crate::ui::WindowParams,
) {
    // 开机自启后端（feature 门控平台实现；无平台 feature 的编译面返回 None
    // ——菜单勾选态恒 false 且切换动作留痕忽略）。
    let autostart: Option<Arc<dyn Autostart>> = xcontrol_tray::platform_autostart();
    if autostart.is_none() {
        tracing::warn!(target: "app::tray", "当前编译面无开机自启后端（未启用平台 feature）");
    }

    register_action_handlers(
        cx,
        Arc::clone(bus),
        core.clone(),
        logs.clone(),
        params.clone(),
        autostart.clone(),
    );

    // 快照提供器：菜单构建器与 update 共用（StateBus current 非阻塞纯读）。
    let provider: SnapshotProvider = {
        let bus = Arc::clone(bus);
        let autostart = autostart.clone();
        Arc::new(move || to_tray_snapshot(&bus.current(), autostart.as_ref()))
    };

    match TrayController::build(provider, cx) {
        Ok(controller) => {
            // 快照刷新实体（强引用入全局存档保活）+ 200ms 循环（FR-6.1 AC：
            // 状态变化 ≤1s 反映——200ms 循环 + tokio 侧 200ms 节流合并，最坏
            // ~400ms 内可见）。
            let refresher = cx.new(|_cx| TrayRefresher {
                controller: controller.clone(),
            });
            // 全局存档（quit close 用 + 刷新实体保活）+ 首次投影。
            cx.set_global(TrayGlobal {
                controller: controller.clone(),
                refresher: refresher.clone(),
            });
            if let Err(error) = controller.update(cx) {
                tracing::warn!(target: "app::tray", %error, "托盘首次投影失败");
            }
            refresher.update(cx, |_refresher, cx| {
                cx.spawn(async move |refresher, cx| loop {
                    cx.background_executor().timer(SNAPSHOT_PERIOD).await;
                    let refreshed =
                        refresher.update(cx, |refresher, cx| refresher.controller.update(cx));
                    match refreshed {
                        Ok(Ok(update)) => {
                            // 变更摘要落 debug 日志（观测面：菜单重建/角标/tooltip
                            // 的应用时刻——FR-6.1 AC 的 ≤1s 断言经日志时间戳核对）。
                            if update.any() {
                                tracing::debug!(target: "app::tray",
                                    tooltip = update.tooltip, badge = update.badge,
                                    icon = update.icon, menu = update.menu,
                                    "托盘投影已应用");
                            }
                        }
                        Ok(Err(error)) => {
                            // 托盘已 close / 平台错误：停止刷新（退出路径正常收敛）。
                            tracing::debug!(target: "app::tray", %error, "托盘刷新终止（已销毁？）");
                            return;
                        }
                        Err(_) => {
                            tracing::debug!(target: "app::tray", "gpui 应用已退出，托盘刷新循环停止");
                            return;
                        }
                    }
                })
                .detach();
            });
            tracing::info!(target: "app::tray",
                "托盘已创建（图标 + docs/06 §5.2 固定顺序菜单；快照刷新 {SNAPSHOT_PERIOD:?}）");
        }
        Err(error) => {
            tracing::warn!(target: "app::tray", %error, "托盘创建失败（降级为无托盘运行）");
        }
    }
}

/// 托盘动作处理器注册（menu.rs 声明的 action 类型 → CoreHandle / 窗口 / OS）。
/// 各处理器独立克隆共享件（`CoreHandle`/`StateBus`/`LogBuffer`/`WindowParams`
/// 均可廉价 Clone）。
fn register_action_handlers(
    cx: &mut App,
    bus: Arc<StateBus>,
    core: CoreHandle,
    logs: LogBuffer,
    params: crate::ui::WindowParams,
    autostart: Option<Arc<dyn Autostart>>,
) {
    // 暂停/恢复共享（FR-2.8）：按当前暂停态取反（菜单标签即目标语义）。
    let (bus_paused, core_paused) = (Arc::clone(&bus), core.clone());
    cx.on_action(move |_action: &xcontrol_tray::TogglePause, _cx: &mut App| {
        let paused = matches!(
            bus_paused.current().conn,
            ConnState::Running { paused: true }
        );
        tracing::info!(target: "app::tray", paused, "托盘动作：暂停/恢复共享（FR-2.8）");
        core_paused.send(if paused {
            CoreCommand::Resume
        } else {
            CoreCommand::Pause
        });
    });

    // 切换角色（FR-1.1）：停当前会话 → 角色翻转 → 运行时重建（supervisor）。
    let core_role = core.clone();
    cx.on_action(move |_action: &xcontrol_tray::SwitchRole, _cx: &mut App| {
        tracing::info!(target: "app::tray", "托盘动作：切换角色（FR-1.1，运行时重建语义）");
        core_role.send(CoreCommand::SwitchRole);
    });

    // 在线从机子菜单「直接切换」（FR-2.5）。
    let core_slave = core.clone();
    cx.on_action(
        move |action: &xcontrol_tray::SwitchToSlave, _cx: &mut App| {
            tracing::info!(target: "app::tray", target = action.target,
            "托盘动作：直接切换到在线从机（FR-2.5）");
            core_slave.send(CoreCommand::SwitchToSlave {
                target: action.target,
            });
        },
    );

    // 主控候选子菜单「确认直连」（FR-5.5）：发现循环立即按确认目标直连；
    // 持久化（config net.confirmed_host）由状态页提交路径承载——托盘路径只
    // 进程内生效（下次启动回退自动语义，docs/06 §4.1）。
    let core_confirm = core.clone();
    cx.on_action(move |action: &xcontrol_tray::ConfirmHost, _cx: &mut App| {
        tracing::info!(target: "app::tray", instance_id = %action.instance_id,
            "托盘动作：确认候选主控（FR-5.5，进程内立即直连）");
        core_confirm.send(CoreCommand::ConfirmHost {
            instance_id: action.instance_id.clone(),
        });
    });

    // 断开并重扫（从机）：命令通道 → 会话优雅断开 → 发现/重连循环接手。
    let core_rescan = core.clone();
    cx.on_action(
        move |_action: &xcontrol_tray::DisconnectRescan, _cx: &mut App| {
            tracing::info!(target: "app::tray", "托盘动作：断开并重扫");
            core_rescan.send(CoreCommand::Disconnect);
        },
    );

    // 打开设置…：窗口唤起（单窗口语义经 ui::open_main_or_quit 一处维护）；
    // 向导态打开的是向导窗（11-18-m4-ui：无配置时设置未就绪）。
    cx.on_action(move |_action: &xcontrol_tray::OpenSettings, cx: &mut App| {
        tracing::info!(target: "app::tray", "托盘动作：打开设置窗口");
        ui::open_main_or_quit(cx, Arc::clone(&bus), logs.clone(), params.clone());
    });

    // 开机自启 ☑（FR-6.1）：OS 级开关（enable/disable 幂等；菜单勾选态经
    // 快照循环的 is_enabled 读数收敛——gpui-tray 在 action 分发后自带菜单
    // 刷新，provider 现读现映）。
    cx.on_action(move |_action: &xcontrol_tray::ToggleAutostart, _cx: &mut App| {
        let Some(autostart) = autostart.as_ref() else {
            tracing::warn!(target: "app::autostart", "无自启后端（平台 feature 未启用），忽略切换");
            return;
        };
        if autostart.is_enabled() {
            match autostart.disable() {
                Ok(()) => tracing::info!(target: "app::autostart", "开机自启已停用"),
                Err(error) => {
                    tracing::warn!(target: "app::autostart", %error, "开机自启停用失败")
                }
            }
        } else {
            match autostart.enable() {
                Ok(()) => tracing::info!(target: "app::autostart",
                    "开机自启已启用（当前二进制路径生效；cargo run 与安装后路径不同）"),
                Err(error) => {
                    tracing::warn!(target: "app::autostart", %error, "开机自启启用失败")
                }
            }
        }
    });

    // 退出：销毁托盘（状态栏图标即时消失）+ 进程退出（QuitMode::Explicit 下
    // 显式退出路径；会话侧由进程终止收尾，macOS terminate 语义）。
    cx.on_action(move |_action: &xcontrol_tray::QuitApp, cx: &mut App| {
        tracing::info!(target: "app::tray", "托盘动作：退出");
        // 先克隆出控制器（全局借用随即释放），再销毁 + 退出。
        let controller = cx.try_global::<TrayGlobal>().map(|g| g.controller.clone());
        if let Some(controller) = controller
            && let Err(error) = controller.close(cx)
        {
            tracing::warn!(target: "app::tray", %error, "托盘销毁失败（继续退出）");
        }
        cx.quit();
    });
}

/// `AppState` → 托盘快照转换（纯投影的 app 侧映射层；无核心状态写入）。
///
/// - 角色：config `Role` → `TrayRole`；
/// - 五态映射（docs/06 §5.1）：`Disconnected` → 错误；`Running{paused:true}` →
///   暂停；主控在线 = 有从机接入（单会话拓扑 0/1 台）；从机已连接 =
///   `Running{paused:false}`；
/// - 在线从机条目：对端名（版本协商 `Hello`）；延迟 M4 无指标来源恒 `None`；
/// - 自启勾选：`Autostart::is_enabled` 实时读数。
fn to_tray_snapshot(state: &AppState, autostart: Option<&Arc<dyn Autostart>>) -> TraySnapshot {
    let role = match state.role {
        Role::Host => TrayRole::Host,
        Role::Client => TrayRole::Client,
    };
    let paused = matches!(state.conn, ConnState::Running { paused: true });
    let running = matches!(state.conn, ConnState::Running { .. });
    let error = matches!(state.conn, ConnState::Disconnected { .. });

    // 在线从机（主控）：单会话拓扑——Running 中带对端名即 1 台在线。
    let slaves = if role == TrayRole::Host && running && !paused {
        state
            .peer_name
            .as_deref()
            .map(|name| {
                vec![SlaveInfo {
                    name: name.to_owned(),
                    latency_ms: None, // 延迟指标聚合属 M5；投影层已支持
                }]
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // 候选主控（FR-5.5 从机）：发现循环投影原样透传（软排除/记忆标注随行）。
    let host_candidates = state
        .host_candidates
        .iter()
        .map(|candidate| xcontrol_tray::HostCandidateInfo {
            name: candidate.name.clone(),
            addr: candidate.addr.clone(),
            instance_id_hex: candidate.instance_id_hex.clone(),
            last_handshake_failed: candidate.last_handshake_failed,
            confirmed: candidate.confirmed,
        })
        .collect();

    // 对端文件清单（FR-7.3）投影直读 hub（进程级单例——托盘 200ms 刷新即反映；
    // 体积 humanize：MiB/KB 二段式，与 pending 槽字节量对齐）。
    let hub = crate::file_channel::lock_hub();
    let transfer_cancelable = hub.session().is_some();
    let incoming_files = hub.pending().map(|slot| {
        let mib = slot.total_bytes as f64 / (1024.0 * 1024.0);
        let size_label = if mib >= 1.0 {
            format!("{mib:.1} MB")
        } else {
            format!("{:.0} KB", slot.total_bytes as f64 / 1024.0)
        };
        xcontrol_tray::IncomingFilesInfo {
            count: slot.entries.len(),
            size_label,
        }
    });

    TraySnapshot {
        role,
        connected: running && !paused && role == TrayRole::Client,
        paused,
        error,
        slaves,
        host_candidates,
        incoming_files,
        transfer_cancelable,
        latency_ms: None,
        autostart_enabled: autostart.is_some_and(|backend| backend.is_enabled()),
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_config::Role as ConfigRole;

    use super::*;

    /// 快照转换矩阵：ConnState × 角色 → 托盘快照五态字段（FR-6.1 数据面）。
    #[test]
    fn app_state_maps_to_tray_snapshot_matrix() {
        // 主控：Idle/Connecting/Handshaking → 搜索中（无从机、未连接）。
        for conn in [
            ConnState::Idle,
            ConnState::Connecting,
            ConnState::Handshaking,
        ] {
            let snapshot = to_tray_snapshot(
                &AppState {
                    role: ConfigRole::Host,
                    conn: conn.clone(),
                    ..AppState::default()
                },
                None,
            );
            assert_eq!(snapshot.role, TrayRole::Host);
            assert!(!snapshot.connected);
            assert!(!snapshot.paused);
            assert!(!snapshot.error);
            assert!(snapshot.slaves.is_empty(), "{conn:?} 无在线从机");
        }

        // 主控：Running 未暂停 + 对端名 → 1 台在线（单会话拓扑）。
        let snapshot = to_tray_snapshot(
            &AppState {
                role: ConfigRole::Host,
                conn: ConnState::Running { paused: false },
                peer_name: Some("mac-book".to_owned()),
                ..AppState::default()
            },
            None,
        );
        assert_eq!(snapshot.slaves.len(), 1);
        assert_eq!(snapshot.slaves[0].name, "mac-book");
        assert!(snapshot.slaves[0].latency_ms.is_none());

        // 主控：Running 未暂停无对端 → 搜索中；暂停 → paused（slaves 清空）。
        let snapshot = to_tray_snapshot(
            &AppState {
                role: ConfigRole::Host,
                conn: ConnState::Running { paused: false },
                ..AppState::default()
            },
            None,
        );
        assert!(snapshot.slaves.is_empty());
        let snapshot = to_tray_snapshot(
            &AppState {
                role: ConfigRole::Host,
                conn: ConnState::Running { paused: true },
                peer_name: Some("mac-book".to_owned()),
                ..AppState::default()
            },
            None,
        );
        assert!(snapshot.paused);
        assert!(snapshot.slaves.is_empty(), "暂停时在线从机不参与投影");

        // 主控：Disconnected → 错误。
        let snapshot = to_tray_snapshot(
            &AppState {
                role: ConfigRole::Host,
                conn: ConnState::Disconnected {
                    reason: "心跳超时".to_owned(),
                },
                ..AppState::default()
            },
            None,
        );
        assert!(snapshot.error);

        // 从机：Running 未暂停 → 已连接；暂停/搜索/错误各态。
        let snapshot = to_tray_snapshot(
            &AppState {
                role: ConfigRole::Client,
                conn: ConnState::Running { paused: false },
                ..AppState::default()
            },
            None,
        );
        assert!(snapshot.connected);
        assert!(snapshot.slaves.is_empty(), "从机角色无从机子菜单数据");
        let snapshot = to_tray_snapshot(
            &AppState {
                role: ConfigRole::Client,
                conn: ConnState::Running { paused: true },
                ..AppState::default()
            },
            None,
        );
        assert!(snapshot.paused);
        assert!(!snapshot.connected);
    }

    /// 自启勾选态：无后端（默认测试编译面）恒 false（有后端时经 is_enabled
    /// 实时读数——托盘实测覆盖）。
    #[test]
    fn autostart_checkstate_follows_backend() {
        let snapshot = to_tray_snapshot(&AppState::default(), None);
        assert!(!snapshot.autostart_enabled);
    }

    /// CoreHandle 仍是动作唯一出口（快照转换不触碰命令通道——编译期由
    /// to_tray_snapshot 签名保证；此处冒烟通道可用性）。
    #[tokio::test]
    async fn core_handle_remains_command_outlet() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(crate::state::COMMAND_CHANNEL_BUFFER);
        let core = CoreHandle::new(tx);
        core.send(CoreCommand::SwitchRole);
        core.send(CoreCommand::SwitchToSlave { target: 0 });
        assert_eq!(CoreHandle::try_recv(&mut rx), Some(CoreCommand::SwitchRole));
        assert_eq!(
            CoreHandle::try_recv(&mut rx),
            Some(CoreCommand::SwitchToSlave { target: 0 })
        );
    }
}
