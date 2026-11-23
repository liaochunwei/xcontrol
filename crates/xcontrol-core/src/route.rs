//! 帧 → 动作 dispatcher（docs/02 §4.1 接收端分派 + docs/03 §3 注入语义的执行层）。
//!
//! 职责：
//! 1. 把解密后的明文字节流切帧（[`decode_frame`]，len 越界 = 违规）；
//! 2. 按 kind 大类解码 payload 并喂给 [`Session`]（转移表判定）；
//! 3. 未知 kind 按 len 跳过（前向兼容语义保持，docs/02 §4.1）；
//! 4. 解码失败 / 非法转移 → 追加违规断开序列（`Error{ProtocolViolation}` + 断开），
//!    调用方只管执行动作并关闭传输；
//! 5. [`apply_actions`] 把 [`SessionAction`] 应用到 [`InputSink`] / 出站发送——
//!    单次注入失败计数并继续（error-handling.md §4：不打断实时路径）。

use xcontrol_proto::frame::{Frame, decode_frame, is_clip_kind, is_ctrl_kind, is_input_kind};
use xcontrol_proto::input::InputEvent;
use xcontrol_proto::{ProtoError, decode_ctrl};

use crate::clock::Clock;
use crate::session::{
    FramePayload, LocalCmd, Outbound, Session, SessionAction, SessionEvent, StateError,
};
use crate::sink::InputSink;

/// dispatcher 处理一段流的最终结论。
#[derive(Debug, Clone, PartialEq)]
pub struct FeedOutcome {
    /// 已处理帧产生的动作（含违规断开序列——有 fatal 时必含断开动作）。
    pub actions: Vec<SessionAction>,
    /// 致命错误：解码失败或非法转移。调用方执行完全部动作后必须关闭传输。
    pub fatal: Option<RouteError>,
}

/// 路由层错误（`FeedOutcome::fatal` 的载荷）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// 帧解码失败（帧头截断 / len 越界 / payload 布局违规，docs/02 §7 code 0x04）。
    #[error("帧解码失败：{0}")]
    Decode(#[from] ProtoError),
    /// 状态机拒绝（非法转移；Paused 拒帧不算致命，dispatcher 内部消化）。
    #[error("状态机拒绝：{0}")]
    State(#[from] StateError),
}

/// 帧路由器：包装会话，把明文字节流转成会话事件与动作序列。
#[derive(Debug)]
pub struct Dispatcher<'s, C: Clock> {
    session: &'s mut Session<C>,
}

impl<'s, C: Clock> Dispatcher<'s, C> {
    /// 包装会话。
    #[must_use]
    pub const fn new(session: &'s mut Session<C>) -> Self {
        Self { session }
    }

    /// 喂入一段解密后的明文流（可含多帧；尾部截断的半帧按剩余字节报错）。
    ///
    /// 处理中途遇会话断开（如 `Error` 帧生效）即停止，剩余字节不再消费；
    /// 违规路径自动追加断开序列。
    pub fn feed(&mut self, stream: &[u8]) -> FeedOutcome {
        let mut actions = Vec::new();
        let mut rest = stream;
        while !rest.is_empty() {
            let frame = match decode_frame(rest) {
                Ok(frame) => frame,
                Err(error) => {
                    return self.fatal(actions, RouteError::Decode(error), "帧解码失败");
                }
            };
            rest = &rest[frame.total_len()..];
            match self.dispatch_frame(&frame) {
                Ok(mut produced) => actions.append(&mut produced),
                Err(RouteError::State(StateError::AlreadyDisconnected)) => {
                    // 会话已在本流内断开：停止消费，不再追加违规动作。
                    return FeedOutcome {
                        actions,
                        fatal: None,
                    };
                }
                Err(RouteError::State(StateError::PausedReject { .. })) => {
                    // Paused 拒帧：丢弃该帧（活性已在 step 记录），连接保持，继续处理。
                    continue;
                }
                Err(error) => return self.fatal(actions, error, "非法转移"),
            }
        }
        FeedOutcome {
            actions,
            fatal: None,
        }
    }

    /// 定时器滴答直通（心跳判定在会话内）。
    pub fn on_timer(&mut self) -> Result<Vec<SessionAction>, StateError> {
        self.session.step(SessionEvent::Timer)
    }

    /// 本地命令直通。
    pub fn on_local(&mut self, cmd: LocalCmd) -> Result<Vec<SessionAction>, StateError> {
        self.session.step(SessionEvent::Local(cmd))
    }

    /// Noise 握手完成直通（携带握手摘要，docs/02 §3.3）。
    pub fn on_handshake_ready(
        &mut self,
        hs_hash: [u8; 32],
    ) -> Result<Vec<SessionAction>, StateError> {
        self.session.step(SessionEvent::HandshakeReady { hs_hash })
    }

    /// 违规收尾：追加 `Error{ProtocolViolation}` 断开序列并给出致命错误。
    fn fatal(
        &mut self,
        mut actions: Vec<SessionAction>,
        error: RouteError,
        reason: &str,
    ) -> FeedOutcome {
        actions.extend(self.session.violation_actions(reason));
        FeedOutcome {
            actions,
            fatal: Some(error),
        }
    }

    /// 单帧分类解码 → 会话事件。
    fn dispatch_frame(&mut self, frame: &Frame<'_>) -> Result<Vec<SessionAction>, RouteError> {
        let kind = frame.header.kind;
        let payload = if is_input_kind(kind) {
            match InputEvent::decode(kind, frame.payload) {
                Ok(Some(event)) => FramePayload::Input(event),
                // is_input_kind 保证 Some；防御性按未知跳过。
                Ok(None) => FramePayload::Unknown { kind },
                Err(error) => return Err(RouteError::Decode(error)),
            }
        } else if is_ctrl_kind(kind) {
            match decode_ctrl(kind, frame.payload) {
                Ok(Some(msg)) => FramePayload::Ctrl(msg),
                Ok(None) => FramePayload::Unknown { kind },
                Err(error) => return Err(RouteError::Decode(error)),
            }
        } else if is_clip_kind(kind) {
            // 剪贴板消息属独立通道；出现在输入/控制连接 = 错误通道（会话按违规处理）。
            FramePayload::Clip { kind }
        } else {
            FramePayload::Unknown { kind }
        };
        Ok(self.session.step(SessionEvent::Frame(payload))?)
    }
}

/// [`apply_actions`] 的执行统计（注入失败按 error-handling.md §4 计数不打断）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// 成功注入的事件数。
    pub injected: usize,
    /// 成功执行的 warp 次数。
    pub warped: usize,
    /// 成功执行的释放次数。
    pub released: usize,
    /// 交给出站发送的消息数。
    pub sent: usize,
    /// sink 失败次数（调用方按需告警；不中断后续动作）。
    pub sink_errors: usize,
}

/// 按序执行会话动作：sink 动作应用到 `sink`，`Send` 动作交给 `on_send` 回调。
///
/// 单次 sink 失败只计数（[`ApplyReport::sink_errors`]），继续处理后续动作
/// （error-handling.md §4：注入类错误可忽略，不得中断事件流）。
/// `Notice` 动作由调用方自行消费（本函数只管 sink 与出站）。
pub fn apply_actions(
    actions: &[SessionAction],
    sink: &mut dyn InputSink,
    on_send: &mut dyn FnMut(u32, &Outbound),
) -> ApplyReport {
    let mut report = ApplyReport::default();
    for action in actions {
        match action {
            SessionAction::Send { seq, msg } => {
                on_send(*seq, msg);
                report.sent += 1;
            }
            SessionAction::InjectInput(event) => {
                if sink.inject(event).is_ok() {
                    report.injected += 1;
                } else {
                    report.sink_errors += 1;
                }
            }
            SessionAction::WarpPointer { screen, x, y } => {
                if sink.warp(*screen, *x, *y).is_ok() {
                    report.warped += 1;
                } else {
                    report.sink_errors += 1;
                }
            }
            SessionAction::ReleaseAll => match sink.release_all() {
                Ok(()) => report.released += 1,
                Err(_) => report.sink_errors += 1,
            },
            SessionAction::Notice(_) => {}
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::input::{KeyState, Mods, ScreenEdge};
    use xcontrol_proto::xkey::{KEY_A, KEY_LEFT_ALT};
    use xcontrol_proto::{Caps, CtrlMsg, ErrCode, Message, Options, ProtoVer};

    use super::*;
    use crate::clock::TestClock;
    use crate::session::{DisconnectReason, Identity, Notice, Role, SessionState};

    /// 注入可控失败的 mock sink。
    struct MockSink {
        fail_all: bool,
        events: Vec<InputEvent>,
        warps: Vec<(u16, f32, f32)>,
        releases: usize,
    }

    impl MockSink {
        fn working() -> Self {
            Self {
                fail_all: false,
                events: Vec::new(),
                warps: Vec::new(),
                releases: 0,
            }
        }

        fn failing() -> Self {
            Self {
                fail_all: true,
                ..Self::working()
            }
        }
    }

    impl InputSink for MockSink {
        fn inject(&mut self, event: &InputEvent) -> Result<(), crate::sink::SinkError> {
            if self.fail_all {
                return Err(crate::sink::SinkError::Failed {
                    action: "inject",
                    reason: "mock".into(),
                });
            }
            self.events.push(event.clone());
            Ok(())
        }

        fn warp(&mut self, screen: u16, x: f32, y: f32) -> Result<(), crate::sink::SinkError> {
            if self.fail_all {
                return Err(crate::sink::SinkError::Failed {
                    action: "warp",
                    reason: "mock".into(),
                });
            }
            self.warps.push((screen, x, y));
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), crate::sink::SinkError> {
            if self.fail_all {
                return Err(crate::sink::SinkError::Failed {
                    action: "release_all",
                    reason: "mock".into(),
                });
            }
            self.releases += 1;
            Ok(())
        }
    }

    fn client_session(clock: TestClock) -> Session<TestClock> {
        Session::new(
            Role::Client,
            Identity {
                name: "client".into(),
                screen_name: "client-screen".into(),
                instance_id: [1; 16],
                caps: Caps::UNICODE,
                salt: [7; 16],
            },
            clock,
            Options::defaults(),
        )
    }

    /// 到 Running 态的 Client（完成与 host 同参的握手）。
    fn running_client(clock: TestClock) -> Session<TestClock> {
        let mut session = client_session(clock);
        session
            .step(SessionEvent::HandshakeReady {
                hs_hash: [0xAB; 32],
            })
            .unwrap();
        let hello = CtrlMsg::Hello {
            proto: ProtoVer::CURRENT,
            name: "host".into(),
            screen_name: "host-screen".into(),
            instance_id: [2; 16],
            caps: Caps::UNICODE,
            salt: [9; 16],
        };
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(hello)))
            .unwrap();
        session
    }

    fn key_frame(code: u16) -> Vec<u8> {
        InputEvent::Key {
            code: xcontrol_proto::xkey::XKey::new(code).unwrap(),
            state: KeyState::Down,
        }
        .encode_frame(1)
        .unwrap()
    }

    /// 多帧流：Key + 未知 kind + Ping → 注入 + 跳过 + Pong。
    #[test]
    fn feed_mixed_stream_decodes_and_skips_unknown() {
        let clock = TestClock::new();
        let mut session = running_client(clock.clone());
        let mut stream = key_frame(0x04);
        // 未知 kind（未来 minor 新增）：按 len 跳过
        stream.extend_from_slice(&xcontrol_proto::encode_frame(2, 0x60, &[1, 2, 3]).unwrap());
        CtrlMsg::Ping { ts_ms: 42 }
            .encode_frame_into(3, &mut stream)
            .unwrap();

        let outcome = {
            let mut dispatcher = Dispatcher::new(&mut session);
            dispatcher.feed(&stream)
        };
        assert_eq!(outcome.fatal, None);
        assert_eq!(outcome.actions.len(), 2, "Inject(Key) + Send(Pong)");
        assert!(matches!(
            &outcome.actions[0],
            SessionAction::InjectInput(InputEvent::Key {
                code: KEY_A,
                state: KeyState::Down
            })
        ));
        assert!(matches!(
            &outcome.actions[1],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Pong { ts_ms: 42 }),
                ..
            }
        ));
    }

    /// len 越界 → 致命 Decode + 违规断开序列（Error 帧 + Notice + 会话终态）。
    #[test]
    fn feed_truncated_stream_violates_and_disconnects() {
        let clock = TestClock::new();
        let mut session = running_client(clock.clone());
        // 帧头声明 len=4 但只带 2 字节
        let mut stream = Vec::new();
        stream.extend_from_slice(&1u32.to_le_bytes()); // seq
        stream.push(0x01); // kind = Key
        stream.extend_from_slice(&4u16.to_le_bytes()); // len = 4
        stream.extend_from_slice(&[0x04, 0x00]); // 只有 2 字节 payload

        let outcome = {
            let mut dispatcher = Dispatcher::new(&mut session);
            dispatcher.feed(&stream)
        };
        assert_eq!(
            outcome.fatal,
            Some(RouteError::Decode(ProtoError::PayloadOutOfBounds {
                declared: 4,
                available: 2,
            }))
        );
        assert_eq!(outcome.actions.len(), 3, "Error 帧 + ReleaseAll + 断开通知");
        assert!(matches!(
            &outcome.actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Error { code, .. }),
                ..
            } if *code == ErrCode::PROTOCOL_VIOLATION
        ));
        assert_eq!(outcome.actions[1], SessionAction::ReleaseAll);
        assert_eq!(
            outcome.actions[2],
            SessionAction::Notice(Notice::Disconnected(DisconnectReason::ProtocolViolation))
        );
        assert_eq!(
            session.state(),
            &SessionState::Disconnected(DisconnectReason::ProtocolViolation)
        );
    }

    /// payload 布局违规（Key payload 长度不符）→ 致命 Decode + 断开序列。
    #[test]
    fn feed_bad_payload_violates() {
        let clock = TestClock::new();
        let mut session = running_client(clock.clone());
        let stream = xcontrol_proto::encode_frame(1, 0x01, &[0x04, 0x00, 0x00]).unwrap(); // 3B，需 4B
        let outcome = {
            let mut dispatcher = Dispatcher::new(&mut session);
            dispatcher.feed(&stream)
        };
        assert!(matches!(
            outcome.fatal,
            Some(RouteError::Decode(ProtoError::LayoutMismatch { .. }))
        ));
        assert!(matches!(
            session.state(),
            SessionState::Disconnected(DisconnectReason::ProtocolViolation)
        ));
    }

    /// Paused 拒帧不算致命：帧被丢弃、连接保持、无注入。
    #[test]
    fn feed_paused_reject_is_tolerated() {
        let clock = TestClock::new();
        let mut session = running_client(clock.clone());
        session
            .step(SessionEvent::Frame(FramePayload::Ctrl(CtrlMsg::Pause)))
            .unwrap();
        let stream = key_frame(0x04);
        let outcome = {
            let mut dispatcher = Dispatcher::new(&mut session);
            dispatcher.feed(&stream)
        };
        assert_eq!(outcome.fatal, None);
        assert!(outcome.actions.is_empty(), "拒帧无动作");
        assert_eq!(session.state(), &SessionState::Running { paused: true });
    }

    /// 空流安全；会话断开后喂流：停止消费、无 fatal、无动作。
    #[test]
    fn feed_empty_and_after_disconnect() {
        let clock = TestClock::new();
        let mut session = running_client(clock.clone());
        let mut dispatcher = Dispatcher::new(&mut session);
        let outcome = dispatcher.feed(&[]);
        assert_eq!(
            outcome,
            FeedOutcome {
                actions: Vec::new(),
                fatal: None,
            }
        );
        // 本地退出（经 dispatcher）
        dispatcher.on_local(LocalCmd::Quit).unwrap();
        // 断开后喂帧
        let stream = CtrlMsg::Ping { ts_ms: 1 }.encode_frame(1).unwrap();
        let outcome = dispatcher.feed(&stream);
        assert_eq!(outcome.fatal, None);
        assert!(outcome.actions.is_empty());
    }

    /// on_timer / on_handshake_ready 直通（时钟推进驱动 Ping）。
    #[test]
    fn passthrough_entrypoints() {
        let clock = TestClock::new();
        let mut session = client_session(clock.clone());
        let hello = CtrlMsg::Hello {
            proto: ProtoVer::CURRENT,
            name: "host".into(),
            screen_name: "host-screen".into(),
            instance_id: [2; 16],
            caps: Caps::UNICODE,
            salt: [9; 16],
        };
        let (ready_outcome, hello_outcome, timer_actions) = {
            let mut dispatcher = Dispatcher::new(&mut session);
            let ready = dispatcher.on_handshake_ready([0x11; 32]).unwrap();
            assert!(ready.is_empty(), "client 就绪无出站");
            let hello_result = dispatcher.feed(&hello.encode_frame(0).unwrap());
            assert_eq!(hello_result.fatal, None);
            assert_eq!(
                hello_result.actions.len(),
                2,
                "HelloBack + HandshakeComplete"
            );
            clock.advance(3000);
            let tick = dispatcher.on_timer().unwrap();
            (ready, hello_result, tick)
        };
        assert_eq!(session.state(), &SessionState::Running { paused: false });
        assert!(!timer_actions.is_empty());
        assert!(matches!(
            &timer_actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 3000 }),
                ..
            }
        ));
        let _ = (ready_outcome, hello_outcome);
    }

    /// Host↔Client 回环冒烟：host 的 SwitchRequest 经编码帧喂给 client，得到 SwitchAck。
    #[test]
    fn host_client_loopback_smoke() {
        let client_clock = TestClock::new();
        let mut host = {
            let clock = TestClock::new();
            Session::new(
                Role::Host,
                Identity {
                    name: "host".into(),
                    screen_name: "host-screen".into(),
                    instance_id: [2; 16],
                    caps: Caps::UNICODE,
                    salt: [9; 16],
                },
                clock,
                Options::defaults(),
            )
        };
        let mut client = running_client(client_clock.clone());

        // Host 握手：就绪发 Hello（seq0），喂 client 的 HelloBack → Running
        let mut sent: Vec<(u32, Outbound)> = Vec::new();
        {
            let mut host_dispatcher = Dispatcher::new(&mut host);
            let actions = host_dispatcher.on_handshake_ready([0xAB; 32]).unwrap();
            apply_actions(&actions, &mut MockSink::working(), &mut |seq, msg| {
                sent.push((seq, msg.clone()));
            });
            assert_eq!(sent.len(), 1, "Host 先发 Hello");
            let hello_back = CtrlMsg::HelloBack {
                proto: ProtoVer::CURRENT,
                name: "client".into(),
                instance_id: [1; 16],
                caps: Caps::UNICODE,
            };
            let outcome = host_dispatcher.feed(&hello_back.encode_frame(0).unwrap());
            assert_eq!(outcome.fatal, None);
            assert_eq!(outcome.actions.len(), 1, "仅 HandshakeComplete 通知");
        }
        assert_eq!(host.state(), &SessionState::Running { paused: false });

        // Host 发 SwitchRequest → 编码 → 喂 client
        let mut host_dispatcher = Dispatcher::new(&mut host);
        let actions = host_dispatcher
            .on_local(LocalCmd::Switch { target: 1 })
            .unwrap();
        sent.clear();
        apply_actions(&actions, &mut MockSink::working(), &mut |seq, msg| {
            sent.push((seq, msg.clone()));
        });
        assert_eq!(sent.len(), 1);
        let mut stream = Vec::new();
        sent[0].1.encode_frame_into(sent[0].0, &mut stream).unwrap();

        let outcome = {
            let mut client_dispatcher = Dispatcher::new(&mut client);
            client_dispatcher.feed(&stream)
        };
        assert_eq!(outcome.fatal, None);
        assert_eq!(
            outcome.actions.len(),
            3,
            "ReleaseAll + SwitchAck + SwitchRequested（M4-hotkeys 切换前释放）"
        );
        assert_eq!(outcome.actions[0], SessionAction::ReleaseAll);
        assert!(matches!(
            &outcome.actions[1],
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::SwitchAck {
                    target: 1,
                    ok: true
                }),
                ..
            }
        ));
    }

    /// apply_actions：注入/warp/释放计数 + sink 失败计数不打断 + Send 回调。
    #[test]
    fn apply_actions_reports_and_continues_on_sink_error() {
        let actions = vec![
            SessionAction::InjectInput(InputEvent::Key {
                code: KEY_A,
                state: KeyState::Down,
            }),
            SessionAction::WarpPointer {
                screen: 1,
                x: 1.0,
                y: 0.5,
            },
            SessionAction::ReleaseAll,
            SessionAction::Send {
                seq: 7,
                msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 1 }),
            },
            SessionAction::Notice(Notice::LeftControl),
        ];
        let mut sink = MockSink::working();
        let mut sent = Vec::new();
        let report = apply_actions(&actions, &mut sink, &mut |seq, msg| {
            sent.push((seq, msg.kind()));
        });
        assert_eq!(
            report,
            ApplyReport {
                injected: 1,
                warped: 1,
                released: 1,
                sent: 1,
                sink_errors: 0,
            }
        );
        assert_eq!(sink.events.len(), 1);
        assert_eq!(sink.warps, vec![(1, 1.0, 0.5)]);
        assert_eq!(sink.releases, 1);
        assert_eq!(sent, vec![(7, 0x45)]); // CtrlMsg::Ping kind

        // 全失败 sink：逐条计数、不 panic、继续
        let mut sink = MockSink::failing();
        let mut sent = Vec::new();
        let report = apply_actions(&actions, &mut sink, &mut |seq, msg| {
            sent.push((seq, msg.kind()));
        });
        assert_eq!(report.injected, 0);
        assert_eq!(
            report.sink_errors, 3,
            "inject + warp + release_all 各计一次"
        );
        assert_eq!(report.sent, 1, "Send 不经 sink，不受影响");
        assert_eq!(sent.len(), 1);
    }

    /// EnterScreen → apply_actions 端到端：对齐注入 + warp 坐标（Right 边）。
    #[test]
    fn enter_screen_end_to_end_through_sink() {
        let clock = TestClock::new();
        let mut session = running_client(clock.clone());
        // 残留 LALT
        session
            .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
                code: KEY_LEFT_ALT,
                state: KeyState::Down,
            })))
            .unwrap();
        let outcome = {
            let mut dispatcher = Dispatcher::new(&mut session);
            dispatcher.feed(
                &InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Right,
                    ratio: 0.25,
                    mods: Mods::LCTRL,
                    preserve_cursor: false,
                }
                .encode_frame(5)
                .unwrap(),
            )
        };
        assert_eq!(outcome.fatal, None);
        assert_eq!(outcome.actions.len(), 4);
        let mut sink = MockSink::working();
        let report = apply_actions(&outcome.actions, &mut sink, &mut |_, _| {});
        assert_eq!(report.injected, 2, "LALT Up + LCTRL Down");
        assert_eq!(report.warped, 1);
        assert_eq!(sink.warps, vec![(1, 0.995, 0.25)]);
        assert_eq!(sink.events.len(), 2);
    }
}
