//! M1 端到端场景测试（11-16-m1-mock-chain，prd Requirements 3/4）。
//!
//! 拓扑（design.md）：
//! ```text
//! ScriptedInputSource → proto 编码 → Noise 加密 → LoopbackTransport → 解密
//!   → 解码 → core Session/Dispatcher → RecordingInputSink 断言（双端对称；
//!   剪贴板走第二对 Loopback + 独立 Noise 握手 + ChannelBind）
//! ```
//!
//! # FR-2.x 端到端覆盖清单（12 种 kind → 测试函数，docs/02 §4.2）
//!
//! | kind | 消息          | 端到端测试函数 |
//! | 0x01 | Key           | `keyboard_causal_order_strictly_preserved`、`modifier_alignment_leave_release_and_periodic_sync` |
//! | 0x02 | KeyRepeat     | `keyboard_causal_order_strictly_preserved` |
//! | 0x03 | MouseMoveRel  | `drag_synthesis_full_chain` |
//! | 0x04 | MouseMoveAbs  | `drag_synthesis_full_chain` |
//! | 0x05 | MouseButton   | `drag_synthesis_full_chain` |
//! | 0x06 | Wheel         | `text_wheel_and_long_text_segmentation` |
//! | 0x07 | Text          | `text_wheel_and_long_text_segmentation` |
//! | 0x10 | EnterScreen   | `modifier_alignment_leave_release_and_periodic_sync` |
//! | 0x11 | LeaveScreen   | `modifier_alignment_leave_release_and_periodic_sync` |
//! | 0x12 | ModifierSync  | `modifier_alignment_leave_release_and_periodic_sync` |
//! | 0x20 | SwitchRequest | `switch_request_flow_end_to_end` |
//! | 0x21 | SwitchAck     | `switch_request_flow_end_to_end` |
//!
//! # 场景组（prd Requirements 3）
//!
//! 1. 键盘因果保序 —— `keyboard_causal_order_strictly_preserved`（含未知 kind
//!    帧边界注入，docs/02 §4.1 按 len 跳过）；
//! 2. 拖拽合成 —— `drag_synthesis_full_chain`（MouseLeft↓ + MoveRel*（RelMerger
//!    合帧 ≤8 条）+ ↑ + MoveAbs 全链到达）；
//! 3. 修饰键全场景 —— `modifier_alignment_leave_release_and_periodic_sync`
//!    （EnterScreen 快照对齐注入 + LeaveScreen 全释放 + ControlDir 门控 1s
//!    周期 sync 差异修正）；
//! 4. 剪贴板全流程 + 三防护 —— `clipboard_full_flow_credit_window_and_blake3_verify`
//!    / `clipboard_oversize_announce_disconnects` / `clipboard_out_of_order_seq_disconnects`
//!    / `clipboard_chunk_before_announce_disconnects`（超限/乱序/未 announce 先
//!    chunk 全部按协议断开剪贴板连接）；
//! 5. 心跳判死 —— `heartbeat_dead_after_nine_seconds_silent_peer`（虚拟时钟
//!    9s 无帧 → Disconnected + ReleaseAll）；
//! 6. 版本协商 —— `version_negotiation_minor_downgrade_coexists`（1.0↔1.1
//!    降级共存双向跑通）、`version_negotiation_major_mismatch_rejected`
//!    （1.0↔2.0 拒绝）。
//!
//! 测试假码纪律（docs/08 §4）：fixture/测试永不用真实安全码；argon2 m=19MiB
//! 每码只派生一次（进程内 OnceLock 缓存）。

use std::thread;
use std::time::Duration;

use xcontrol_core::clip_rx::{ClipRx, ClipRxAction, ClipRxConfig};
use xcontrol_core::clip_tx::ClipTx;
use xcontrol_core::clock::TestClock;
use xcontrol_core::mock::{MockClipboard, RecordingInputSink, ScriptedInputSource};
use xcontrol_core::route::{Dispatcher, apply_actions};
use xcontrol_core::session::{
    FramePayload, Identity, LocalCmd, Notice, Outbound, Role, Session, SessionAction, SessionEvent,
    SessionState, StateError,
};
use xcontrol_core::sink::InputSource;
use xcontrol_net::transport::Transport;
use xcontrol_net::{
    HandshakeDeadline, HandshakeRole, LoopbackControl, LoopbackTransport, NetError, NoiseHandshake,
    SecureTransport,
};
use xcontrol_proto::clip::{
    ChannelBind, ClipAck, ClipAnnounce, ClipChunk, ClipEnd, ClipFormat, ClipMsg, decode_clip,
};
use xcontrol_proto::frame::{decode_frame, encode_frame_into};
use xcontrol_proto::input::{Axis, InputEvent, KeyState, Mods, MouseButton, RelMerger, ScreenEdge};
use xcontrol_proto::xkey::{KEY_C, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_LEFT_SHIFT};
use xcontrol_proto::{Caps, CtrlMsg, Message, Options, ProtoError, ProtoVer, negotiate};

// ———————————————————————————————————— 夹具 ————————————————————————————————————

/// 测试辅助取值（helpers 非 `#[test]` 作用域，与 chain.rs 的 `must` 同形：
/// panic 携带上下文）。
#[track_caller]
fn must<T, E: std::fmt::Debug>(context: &'static str, result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"),
    }
}

/// 测试假码派生（docs/08 §4；进程内一次）。
fn good_psk() -> [u8; 32] {
    static PSK: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *PSK.get_or_init(|| {
        must(
            "PSK 派生",
            xcontrol_net::derive_psk("test-code", &[0u8; 16]),
        )
    })
}

/// 测试身份（从机侧）。
fn client_identity() -> Identity {
    Identity {
        name: "client".into(),
        screen_name: "client-screen".into(),
        instance_id: [1; 16],
        caps: Caps::UNICODE,
        salt: [7; 16],
    }
}

/// 测试身份（主控侧）。
fn host_identity() -> Identity {
    Identity {
        name: "host".into(),
        screen_name: "host-screen".into(),
        instance_id: [2; 16],
        caps: Caps::UNICODE.union(Caps::ABS_POINTER),
        salt: [9; 16],
    }
}

/// 双线程完成一次握手；任一侧失败即 panic（测试快速失败）。
fn handshake_pair_on(
    client_wire: LoopbackTransport,
    host_wire: LoopbackTransport,
    psk: &[u8; 32],
) -> (SecureTransport, SecureTransport) {
    let psk = *psk;
    let client = thread::spawn(move || {
        let mut wire = client_wire;
        NoiseHandshake::run(
            &mut wire,
            HandshakeRole::Initiator,
            &psk,
            &HandshakeDeadline::after(Duration::from_secs(2)),
        )
    });
    let host = thread::spawn(move || {
        let mut wire = host_wire;
        NoiseHandshake::run(
            &mut wire,
            HandshakeRole::Responder,
            &psk,
            &HandshakeDeadline::after(Duration::from_secs(2)),
        )
    });
    let client_st = must("client 握手成功", must("client 线程", client.join()));
    let host_st = must("host 握手成功", must("host 线程", host.join()));
    (client_st, host_st)
}

/// 加密发送一整段明文帧流（应用帧整体进单条 Noise 报文，docs/02 §4.1）。
fn send_encrypted(st: &mut SecureTransport, wire: &mut LoopbackTransport, plaintext: &[u8]) {
    let mut ciphertext = Vec::new();
    must(
        "加密成功（帧流在 MAX_PLAINTEXT 内）",
        st.encrypt_into(plaintext, &mut ciphertext),
    );
    must("回环发送成功", wire.send(&ciphertext));
}

/// 接收一个报文并解密为明文帧流。
fn recv_decrypted(wire: &mut LoopbackTransport, st: &mut SecureTransport) -> Vec<u8> {
    let mut ciphertext = Vec::new();
    assert!(
        must(
            "回环接收成功",
            wire.recv_timeout(Duration::from_secs(2), &mut ciphertext)
        ),
        "应有报文到达"
    );
    let mut plaintext = Vec::new();
    must("解密成功", st.decrypt_into(&ciphertext, &mut plaintext));
    plaintext
}

/// 全双工回环上的一端：会话 + 传输 + 注入记录 + 出站缓冲。
struct Peer {
    label: &'static str,
    wire: LoopbackTransport,
    st: SecureTransport,
    session: Session<TestClock>,
    clock: TestClock,
    sink: RecordingInputSink,
    /// 出站帧缓冲（Session 产出动作的编码目标；`flush_out` 一次性发出）。
    out: Vec<u8>,
    /// 捕获路径（mock 源事件）的帧 seq 计数（诊断字段，与会话 seq 独立）。
    wire_seq: u32,
}

impl Peer {
    fn new(
        label: &'static str,
        role: Role,
        identity: Identity,
        wire: LoopbackTransport,
        st: SecureTransport,
    ) -> Self {
        let clock = TestClock::new();
        let session = Session::new(role, identity, clock.clone(), Options::defaults());
        Self {
            label,
            wire,
            st,
            session,
            clock,
            sink: RecordingInputSink::new(),
            out: Vec::new(),
            wire_seq: 0,
        }
    }

    fn new_host(wire: LoopbackTransport, st: SecureTransport) -> Self {
        Self::new("host", Role::Host, host_identity(), wire, st)
    }

    fn new_client(wire: LoopbackTransport, st: SecureTransport) -> Self {
        Self::new("client", Role::Client, client_identity(), wire, st)
    }

    /// 出站帧缓冲加密发送（空缓冲跳过）。
    fn flush_out(&mut self) {
        let bytes = std::mem::take(&mut self.out);
        if bytes.is_empty() {
            return;
        }
        send_encrypted(&mut self.st, &mut self.wire, &bytes);
    }

    /// 接收一个报文 → 解密 → 喂 Dispatcher → 动作应用到 sink/出站缓冲。
    /// 返回动作（Notice 断言用；fatal 即 panic）。
    fn pump_in(&mut self) -> Vec<SessionAction> {
        let plaintext = recv_decrypted(&mut self.wire, &mut self.st);
        let outcome = {
            let mut dispatcher = Dispatcher::new(&mut self.session);
            dispatcher.feed(&plaintext)
        };
        assert!(
            outcome.fatal.is_none(),
            "{}: 链路 fatal：{:?}",
            self.label,
            outcome.fatal
        );
        self.apply(&outcome.actions);
        outcome.actions
    }

    /// 动作应用：注入进 sink 记录；Send 编码进出站缓冲（Notice 由断言消费）。
    fn apply(&mut self, actions: &[SessionAction]) {
        let Peer { sink, out, .. } = self;
        apply_actions(actions, sink, &mut |seq, msg| {
            must("出站帧编码", msg.encode_frame_into(seq, out));
        });
    }

    /// 定时器滴答（Clock 驱动的心跳判定/周期快照）。
    fn tick(&mut self) -> Vec<SessionAction> {
        let actions = {
            let mut dispatcher = Dispatcher::new(&mut self.session);
            must("timer", dispatcher.on_timer())
        };
        self.apply(&actions);
        actions
    }

    /// 本地命令直通。
    fn local(&mut self, cmd: LocalCmd) -> Vec<SessionAction> {
        let actions = {
            let mut dispatcher = Dispatcher::new(&mut self.session);
            must("local cmd", dispatcher.on_local(cmd))
        };
        self.apply(&actions);
        actions
    }

    /// 捕获路径：mock 源事件逐条编码进出站缓冲（键盘/按钮严禁合并，docs/02 §4.2；
    /// `MouseMoveRel` 合帧由调用方按需经 `RelMerger` 后自行编码）。
    fn pump_capture(&mut self, source: &mut ScriptedInputSource) {
        let mut batch = Vec::new();
        source.poll(&mut batch);
        for event in batch {
            self.wire_seq = self.wire_seq.wrapping_add(1);
            must(
                "捕获帧编码",
                event.encode_frame_into(self.wire_seq, &mut self.out),
            );
        }
    }

    /// 线层边界注入：未知 kind 帧（docs/02 §4.1 按 len 跳过）。
    fn inject_unknown_frame(&mut self, kind: u8, payload: &[u8]) {
        self.wire_seq = self.wire_seq.wrapping_add(1);
        must(
            "未知 kind 帧编码",
            encode_frame_into(self.wire_seq, kind, payload, &mut self.out),
        );
    }

    /// 合帧后的单条 MoveRel 编码（发送窗口产物）。
    fn send_rel(&mut self, dx: f32, dy: f32) {
        self.wire_seq = self.wire_seq.wrapping_add(1);
        let event = InputEvent::MouseMoveRel { dx, dy };
        must(
            "MoveRel 帧编码",
            event.encode_frame_into(self.wire_seq, &mut self.out),
        );
    }
}

/// 标准拓扑：握手 + 协商完成的双端（双方 Running、时钟各自独立虚拟推进）。
fn setup_pair() -> (Peer, Peer, LoopbackControl) {
    let (client_wire, host_wire, control) = LoopbackTransport::pair(None);
    let (client_st, host_st) =
        handshake_pair_on(client_wire.clone(), host_wire.clone(), &good_psk());
    let mut host = Peer::new_host(host_wire, host_st);
    let mut client = Peer::new_client(client_wire, client_st);
    run_negotiation(&mut host, &mut client);
    (host, client, control)
}

/// 协商序列（chain.rs 同款）：Host 就绪发 Hello → Client 回 HelloBack → 双方
/// Running + HandshakeComplete 通知（minor = min(0,0)）。与 proto 层
/// [`xcontrol_proto::negotiate`] 同名故取 run_negotiation（本文件用的是会话级序列）。
fn run_negotiation(host: &mut Peer, client: &mut Peer) {
    let actions = {
        let mut dispatcher = Dispatcher::new(&mut client.session);
        must(
            "Client 接受 HandshakeReady",
            dispatcher.on_handshake_ready(client.st.handshake_hash()),
        )
    };
    assert!(actions.is_empty(), "从机就绪无出站");
    let actions = {
        let mut dispatcher = Dispatcher::new(&mut host.session);
        must(
            "Host 接受 HandshakeReady",
            dispatcher.on_handshake_ready(host.st.handshake_hash()),
        )
    };
    assert_eq!(actions.len(), 1, "Host 就绪发 Hello");
    host.apply(&actions);
    host.flush_out();

    let actions = client.pump_in();
    assert!(
        actions.iter().any(|action| matches!(
            action,
            SessionAction::Notice(Notice::HandshakeComplete { .. })
        )),
        "协商完成通知"
    );
    client.flush_out();
    let actions = host.pump_in();
    assert!(
        actions.iter().any(|action| matches!(
            action,
            SessionAction::Notice(Notice::HandshakeComplete { .. })
        )),
        "协商完成通知"
    );
    assert_eq!(
        host.session.state(),
        &SessionState::Running { paused: false }
    );
    assert_eq!(
        client.session.state(),
        &SessionState::Running { paused: false }
    );
}

// ————————————————————————— 场景 1：键盘因果保序 —————————————————————————

/// 场景 1（FR-2.2）：`Meta↓ C↓ (KeyRepeat) C↑ Meta↑` 经全链路严格保序到达；
/// 事件流中夹杂未知 kind 帧（未来 minor 新增消息）被按 len 跳过且不影响顺序。
#[test]
fn keyboard_causal_order_strictly_preserved() {
    let (mut host, mut client, _control) = setup_pair();

    // 批次 1：Meta↓ C↓
    let mut source = ScriptedInputSource::new([
        InputEvent::Key {
            code: KEY_LEFT_META,
            state: KeyState::Down,
        },
        InputEvent::Key {
            code: KEY_C,
            state: KeyState::Down,
        },
    ]);
    host.pump_capture(&mut source);
    // 边界注入：未知 kind 帧（0x61，文件域止于 0x60 后的下一空档）按 len 跳过（docs/02 §4.1）
    host.inject_unknown_frame(0x61, &[0xDE, 0xAD, 0xBE, 0xEF]);
    // 批次 2：KeyRepeat C → C↑ → Meta↑（因果顺序 = 脚本顺序）
    source.push(InputEvent::KeyRepeat { code: KEY_C });
    source.push(InputEvent::Key {
        code: KEY_C,
        state: KeyState::Up,
    });
    source.push(InputEvent::Key {
        code: KEY_LEFT_META,
        state: KeyState::Up,
    });
    host.pump_capture(&mut source);
    host.flush_out();

    // 一个 Noise 报文携带全部 6 帧 → 客户端按 TCP 顺序逐帧处理（docs/02 §4.1）
    let actions = client.pump_in();
    assert!(
        actions
            .iter()
            .all(|action| matches!(action, SessionAction::InjectInput(_))),
        "纯输入流只产生注入动作：{actions:?}"
    );
    let events = client.sink.events();
    assert_eq!(
        events,
        &[
            InputEvent::Key {
                code: KEY_LEFT_META,
                state: KeyState::Down
            },
            InputEvent::Key {
                code: KEY_C,
                state: KeyState::Down
            },
            InputEvent::KeyRepeat { code: KEY_C },
            InputEvent::Key {
                code: KEY_C,
                state: KeyState::Up
            },
            InputEvent::Key {
                code: KEY_LEFT_META,
                state: KeyState::Up
            },
        ],
        "键盘因果序列严格保序（未知 kind 帧被跳过）"
    );
    assert_eq!(events.len(), 5);
    assert_eq!(
        client.session.state(),
        &SessionState::Running { paused: false }
    );
    assert_eq!(
        host.session.state(),
        &SessionState::Running { paused: false }
    );
}

// ————————————————————————— 场景 2：拖拽合成 —————————————————————————

/// 场景 2（FR-2.4）：拖拽 = `MouseButton{Left,↓} + MoveRel* + MouseButton{Left,↑}`
/// 全链到达（docs/03 §3「不设专门消息」）；MoveRel 经发送侧 RelMerger 合帧
/// （≤8 条/窗口，docs/02 §4.2），delta 守恒；MoveAbs 直发直达。
#[test]
fn drag_synthesis_full_chain() {
    let (mut host, mut client, _control) = setup_pair();

    // MouseLeft↓
    let mut source = ScriptedInputSource::new([InputEvent::MouseButton {
        button: MouseButton::Left,
        state: KeyState::Down,
    }]);
    host.pump_capture(&mut source);

    // 6 条 MoveRel 经 RelMerger 合帧（6 ≤ 8 → 单条累计 delta 直发）
    let mut merger = RelMerger::new();
    let mut merged_frames = 0usize;
    for _ in 0..6u32 {
        if let Some((dx, dy)) = merger.push_and_flush_full(0.5, 0.25) {
            host.send_rel(dx, dy);
            merged_frames += 1;
        }
    }
    let (dx, dy) = merger.take().expect("窗口内应有累计");
    host.send_rel(dx, dy);
    merged_frames += 1;
    assert_eq!(merged_frames, 1, "6 条相对移动合并为 1 条（≤8 条/窗口）");

    // MouseLeft↑ + MoveAbs（回中）
    source.push(InputEvent::MouseButton {
        button: MouseButton::Left,
        state: KeyState::Up,
    });
    source.push(InputEvent::MouseMoveAbs {
        screen: 1,
        x: 0.5,
        y: 0.5,
    });
    host.pump_capture(&mut source);
    host.flush_out();
    client.pump_in();

    assert_eq!(
        client.sink.events(),
        &[
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down
            },
            InputEvent::MouseMoveRel { dx: 3.0, dy: 1.5 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Up
            },
            InputEvent::MouseMoveAbs {
                screen: 1,
                x: 0.5,
                y: 0.5
            },
        ],
        "拖拽全链：按钮夹持移动序列保序，合并 delta 守恒（0.5×6=3.0 / 0.25×6=1.5）"
    );
}

// —————————————————————— 场景 3（Text/Wheel + 超长分段）——————————————————————

/// 场景 3a（FR-2.3/FR-2.6）：超长 Text（>512B、多字节跨边界）经 mock 源
/// 按 512B 上限分段（docs/03 §4.5），全链到达后拼接严格还原；Wheel 直达。
#[test]
fn text_wheel_and_long_text_segmentation() {
    let (mut host, mut client, _control) = setup_pair();

    // 超长文本：ASCII 边界 + 多字节字符 + emoji（跨分段边界）
    let text = format!("{}中文文本✂️", "x".repeat(1200));
    let segments = ScriptedInputSource::segment_text(&text);
    assert!(segments.len() > 1, "超长文本必须分段");
    assert!(
        segments
            .iter()
            .all(|segment| segment.len() <= xcontrol_proto::MAX_TEXT_LEN)
    );
    assert_eq!(segments.concat(), text, "分段拼接严格还原原文");

    let mut source = ScriptedInputSource::long_text(&text);
    host.pump_capture(&mut source);
    // Wheel（FR-2.3）
    source.push(InputEvent::Wheel {
        axis: Axis::Vertical,
        delta: -3.5,
    });
    source.push(InputEvent::Wheel {
        axis: Axis::Horizontal,
        delta: 1.0,
    });
    host.pump_capture(&mut source);
    host.flush_out();
    client.pump_in();

    assert_eq!(
        client.sink.joined_text(),
        text,
        "分段 Text 全链重组与原文一致（docs/02 §4.2 kind 0x07 ≤512B/条）"
    );
    let tail = &client.sink.events()[segments.len()..];
    assert_eq!(
        tail,
        &[
            InputEvent::Wheel {
                axis: Axis::Vertical,
                delta: -3.5
            },
            InputEvent::Wheel {
                axis: Axis::Horizontal,
                delta: 1.0
            },
        ],
        "Wheel 事件直达（垂直/水平轴）"
    );
    assert_eq!(
        client.session.state(),
        &SessionState::Running { paused: false }
    );
}

// ————————————————————————— 场景 4：修饰键全场景 —————————————————————————

/// 场景 4（FR-2.2/docs-03 §3）：修饰键三段式 ——
/// ① EnterScreen 快照对齐（被控端合成注入 + warp）；
/// ② 控制方 1s 周期 ModifierSync（ControlDir 门控）+ 快照差异修正（合成 Up）；
/// ③ 被控端切出：LeaveScreen 全释放 + 对端转入受控态（对齐 + warp）。
#[test]
fn modifier_alignment_leave_release_and_periodic_sync() {
    let (mut host, mut client, _control) = setup_pair();

    // ① Host 修饰键按住（Ctrl+Shift）时切出 → ReleaseAll + LeaveScreen + EnterScreen
    host.local(LocalCmd::LocalMods(Mods::LCTRL.union(Mods::LSHIFT)));
    host.local(LocalCmd::EdgeCrossing {
        target: 1,
        edge: ScreenEdge::Right,
        ratio: 0.5,
    });
    assert_eq!(host.sink.release_count(), 1, "本机切出先释放（docs/03 §5）");
    host.flush_out();

    let actions = client.pump_in();
    assert!(
        actions.iter().any(|action| matches!(
            action,
            SessionAction::Notice(Notice::EnteredControl { target: 1 })
        )),
        "受控开始通知"
    );
    assert_eq!(client.sink.release_count(), 1, "LeaveScreen → 全释放");
    assert_eq!(
        client.sink.events(),
        &[
            InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Down
            },
            InputEvent::Key {
                code: KEY_LEFT_SHIFT,
                state: KeyState::Down
            },
        ],
        "EnterScreen 快照对齐：空基准 → 合成 LCTRL↓ LSHIFT↓（docs/03 §3 防卡键）"
    );
    assert_eq!(
        client.sink.warps(),
        &[(1, 0.005, 0.5)],
        "滑出 Right → 进入边 Left → warp(1, 0.005, 0.5)——11-20 进入边语义+死区（11-22 第三轮：0.5% 贴缘，防弹回由免疫窗承接）"
    );

    // ② 周期 sync：1s tick（本机为控制方 LocalControlsPeer → 快照外发）
    host.clock.advance(1000);
    let actions = host.tick();
    assert!(
        actions.iter().any(|action| matches!(
            action,
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::ModifierSync { mods }),
                ..
            } if *mods == Mods::LCTRL.union(Mods::LSHIFT)
        )),
        "周期快照随 EdgeCrossing 开启（单向语义，docs/03 §3）：{actions:?}"
    );
    host.flush_out();
    let before = client.sink.events().len();
    client.pump_in();
    assert_eq!(
        client.sink.events().len(),
        before,
        "快照与已注入基准一致 → 无合成注入"
    );

    // ③ 差异修正：Host 释放 Shift → 下一周期快照只有 LCTRL → 客户端合成 LSHIFT↑
    host.local(LocalCmd::LocalMods(Mods::LCTRL));
    host.clock.advance(1000);
    let actions = host.tick();
    assert!(
        actions.iter().any(|action| matches!(
            action,
            SessionAction::Send {
                msg: Outbound::Input(InputEvent::ModifierSync { mods }),
                ..
            } if *mods == Mods::LCTRL
        )),
        "1s 周期差异修正"
    );
    host.flush_out();
    client.pump_in();
    assert_eq!(
        client.sink.events().last(),
        Some(&InputEvent::Key {
            code: KEY_LEFT_SHIFT,
            state: KeyState::Up
        }),
        "快照差异 → 合成释放（防卡键）"
    );

    // ④ 反向切回：客户端修饰键按住（LCTRL）穿越回主控屏
    client.local(LocalCmd::LocalMods(Mods::LCTRL));
    client.local(LocalCmd::EdgeCrossing {
        target: 0,
        edge: ScreenEdge::Left,
        ratio: 0.5,
    });
    assert_eq!(
        client.sink.release_count(),
        2,
        "被控端切出：释放全部已注入按键（LCTRL/LSHIFT）"
    );
    assert_eq!(
        client.session.control_dir(),
        xcontrol_core::session::ControlDir::LocalControlsPeer,
        "客户端成为控制方"
    );
    client.flush_out();

    let actions = host.pump_in();
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, SessionAction::Notice(Notice::LeftControl))),
        "主控收到 LeaveScreen → 退出受控"
    );
    assert!(
        actions.iter().any(|action| matches!(
            action,
            SessionAction::Notice(Notice::EnteredControl { target: 0 })
        )),
        "主控随后转入受控态（EnterScreen target 0）"
    );
    assert_eq!(host.sink.release_count(), 2, "主控侧 LeaveScreen → 全释放");
    assert_eq!(
        host.sink.events(),
        &[InputEvent::Key {
            code: KEY_LEFT_CTRL,
            state: KeyState::Down
        }],
        "主控转入受控态：按客户端快照对齐注入 LCTRL↓"
    );
    assert_eq!(
        host.sink.warps(),
        &[(0, 0.995, 0.5)],
        "从机滑出 Left 边 → 主控进入边 Right → warp(0, 0.995, 0.5)——11-20 进入边语义"
    );
    assert_eq!(
        client.session.state(),
        &SessionState::Running { paused: false }
    );
    assert_eq!(
        host.session.state(),
        &SessionState::Running { paused: false }
    );
}

// ———————————————————————— 场景 5：切换请求（FR-2.5）————————————————————————

/// 场景 5（FR-2.5）：主控 `Switch` 本地命令 → SwitchRequest 全链到达从机 →
/// 从机回 SwitchAck{ok:true} → 主控收到 SwitchResult 通知。
#[test]
fn switch_request_flow_end_to_end() {
    let (mut host, mut client, _control) = setup_pair();

    host.local(LocalCmd::Switch { target: 1 });
    host.flush_out();

    let actions = client.pump_in();
    assert_eq!(
        actions,
        vec![
            // M4-hotkeys（PRD 要求 6）：受控切换前先释放本机已注入按键（防叠加卡键）。
            SessionAction::ReleaseAll,
            SessionAction::Send {
                seq: 1, // HelloBack（协商期）= seq 0，SwitchAck 延续单调计数
                msg: Outbound::Input(InputEvent::SwitchAck {
                    target: 1,
                    ok: true
                })
            },
            SessionAction::Notice(Notice::SwitchRequested { target: 1 }),
        ],
        "SwitchRequest 到达：ReleaseAll + 默认可受控即确认（M1 无能力否决，FR-2.5）"
    );
    client.flush_out();

    let actions = host.pump_in();
    assert_eq!(
        actions,
        vec![
            SessionAction::Notice(Notice::SwitchResult {
                target: 1,
                ok: true
            }),
            // M5 切换闭环（docs/03 §5 热键闭环）：Ack ok → 主控立即 EdgeCrossing
            // 同型接管；本场景无布局图（空布局）→ 进入边回退 target 左缘 =
            // Next 语义，ratio 居中（entry_edge_into 回退契约）。
            SessionAction::ReleaseAll,
            SessionAction::Send {
                seq: 2,
                msg: Outbound::Input(InputEvent::LeaveScreen {
                    mods: Mods::default()
                })
            },
            SessionAction::Send {
                seq: 3,
                msg: Outbound::Input(InputEvent::EnterScreen {
                    target: 1,
                    edge: ScreenEdge::Left,
                    ratio: 0.5,
                    mods: Mods::default(),
                    preserve_cursor: true,
                })
            },
        ],
        "主控收到切换确认 → Ack ok 立即接管（M5 闭环：ReleaseAll + LeaveScreen + EnterScreen）"
    );
}

// ————————————————————————— 场景 6：心跳判死 —————————————————————————

/// 场景 6（docs/02 §4.5）：3s Ping/Pong 双向保活；随后对端静默 9s（虚拟时钟
/// 无任何入帧）→ 会话判死 Disconnected(Timeout) + ReleaseAll 防卡键兜底。
#[test]
fn heartbeat_dead_after_nine_seconds_silent_peer() {
    let (mut host, mut client, _control) = setup_pair();

    // 保活路径：host 3s tick → Ping；client（虚拟时钟 t=1000）收到 → Pong
    host.clock.advance(3000);
    let actions = host.tick();
    assert!(matches!(
        actions[0],
        SessionAction::Send {
            msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 3000 }),
            ..
        }
    ));
    host.flush_out();

    client.clock.advance(1000);
    let actions = client.pump_in();
    assert!(
        matches!(
            actions[0],
            SessionAction::Send {
                msg: Outbound::Ctrl(CtrlMsg::Pong { ts_ms: 3000 }),
                ..
            }
        ),
        "Ping → Pong"
    );
    client.flush_out();
    assert!(host.pump_in().is_empty(), "Pong 无动作，连接保持");

    // 静默判死：距 client 上一次收帧（t=1000）8999ms 仍活（Ping 周期已到 → 发 Ping）
    client.clock.advance(8999);
    let actions = client.tick();
    assert!(matches!(
        actions[0],
        SessionAction::Send {
            msg: Outbound::Ctrl(CtrlMsg::Ping { ts_ms: 9999 }),
            ..
        }
    ));
    assert_eq!(
        client.session.state(),
        &SessionState::Running { paused: false }
    );

    // t=10000：9s 无任何帧 → 判死（ReleaseAll + 通知）
    client.clock.advance(1);
    let actions = client.tick();
    assert_eq!(
        actions,
        vec![
            SessionAction::ReleaseAll,
            SessionAction::Notice(Notice::Disconnected(
                xcontrol_core::session::DisconnectReason::Timeout
            )),
        ],
        "9s 无帧判死（docs/02 §4.5）"
    );
    assert_eq!(
        client.session.state(),
        &SessionState::Disconnected(xcontrol_core::session::DisconnectReason::Timeout)
    );
    assert_eq!(
        client.sink.release_count(),
        1,
        "断开即释放已注入按键（防卡键兜底；tick 已应用 ReleaseAll）"
    );
}

// ————————————————————————— 场景 7：版本协商 —————————————————————————

/// 场景 7a（docs/02 §5）：1.0 ↔ 本端 CURRENT 降级共存双向跑通（本端 CURRENT
/// 随 PROTO_MINOR 锚点走——11-19-m5-layout 时为 1.1，11-26 文件域起为 1.2；
/// 模拟对端恒为 1.0）——
/// ① 真实主控会话接受模拟 1.0 从机的 HelloBack{1,0}（协商 minor=0）并按
///    minor 0 消息集完成 Ping/Pong 往返；
/// ② 真实从机会话接受模拟 1.0 主控的 Hello{1,0}（HelloBack 仍为本端
///    CURRENT，模拟 1.0 侧 negotiate 接受）。
#[test]
fn version_negotiation_minor_downgrade_coexists() {
    // —— ① 真实 host（1.1）↔ 模拟 1.0 从机 ——
    let (mut client_wire, host_wire, _control) = LoopbackTransport::pair(None);
    let (mut sim_st, host_st) =
        handshake_pair_on(client_wire.clone(), host_wire.clone(), &good_psk());
    let mut host = Peer::new_host(host_wire, host_st);

    let actions = {
        let mut dispatcher = Dispatcher::new(&mut host.session);
        must(
            "Host 就绪",
            dispatcher.on_handshake_ready(host.st.handshake_hash()),
        )
    };
    assert_eq!(actions.len(), 1);
    host.apply(&actions);
    host.flush_out();

    // 模拟 1.0 从机收到 Hello{CURRENT}：negotiate(1.0, CURRENT) 接受 → 回 HelloBack{1,0}
    let plaintext = recv_decrypted(&mut client_wire, &mut sim_st);
    let frame = must("帧解码", decode_frame(&plaintext));
    let msg = must(
        "控制消息解码",
        xcontrol_proto::decode_ctrl(frame.header.kind, frame.payload),
    );
    let Some(CtrlMsg::Hello { proto, .. }) = msg else {
        panic!("应为 Hello：{msg:?}");
    };
    assert_eq!(proto, ProtoVer::CURRENT, "Hello 恒为本端 CURRENT");
    assert_eq!(
        negotiate(ProtoVer { major: 1, minor: 0 }, proto),
        Ok(ProtoVer { major: 1, minor: 0 }),
        "1.0 从机接受 CURRENT 主控（降级共存：会话 minor 取 min）"
    );
    let hello_back = CtrlMsg::HelloBack {
        proto: ProtoVer { major: 1, minor: 0 },
        name: "sim-client-1.0".into(),
        instance_id: [9; 16],
        caps: Caps::UNICODE,
    };
    send_encrypted(
        &mut sim_st,
        &mut client_wire,
        &must("HelloBack 编码", hello_back.encode_frame(0)),
    );

    let actions = host.pump_in();
    assert!(
        actions.iter().any(|action| matches!(
            action,
            SessionAction::Notice(Notice::HandshakeComplete { minor: 0, .. })
        )),
        "主控完成协商：minor = min(0, CURRENT) = 0"
    );
    assert_eq!(
        host.session.negotiated(),
        Some(ProtoVer { major: 1, minor: 0 })
    );

    // 降级共存实际跑通：模拟 1.1 侧按 minor 0 消息集发 Ping → 主控回 Pong
    let ping = CtrlMsg::Ping { ts_ms: 5000 };
    send_encrypted(
        &mut sim_st,
        &mut client_wire,
        &must("Ping 编码", ping.encode_frame(1)),
    );
    let actions = host.pump_in();
    assert!(matches!(
        actions[0],
        SessionAction::Send {
            msg: Outbound::Ctrl(CtrlMsg::Pong { ts_ms: 5000 }),
            ..
        }
    ));
    host.flush_out();
    let plaintext = recv_decrypted(&mut client_wire, &mut sim_st);
    let frame = must("Pong 帧解码", decode_frame(&plaintext));
    let msg = must(
        "Pong 解码",
        xcontrol_proto::decode_ctrl(frame.header.kind, frame.payload),
    );
    assert_eq!(msg, Some(CtrlMsg::Pong { ts_ms: 5000 }));

    // —— ② 真实 client（1.1）↔ 模拟 1.0 主控 ——
    let (client_wire, mut host_wire, _control) = LoopbackTransport::pair(None);
    let (client_st, mut sim_st) =
        handshake_pair_on(client_wire.clone(), host_wire.clone(), &good_psk());
    let mut client = Peer::new_client(client_wire, client_st);
    let actions = {
        let mut dispatcher = Dispatcher::new(&mut client.session);
        must(
            "Client 就绪",
            dispatcher.on_handshake_ready(client.st.handshake_hash()),
        )
    };
    assert!(actions.is_empty());

    let hello = CtrlMsg::Hello {
        proto: ProtoVer { major: 1, minor: 0 },
        name: "sim-host-1.0".into(),
        screen_name: "sim-screen".into(),
        instance_id: [8; 16],
        caps: Caps::UNICODE,
        salt: [3; 16],
    };
    send_encrypted(
        &mut sim_st,
        &mut host_wire,
        &must("Hello 编码", hello.encode_frame(0)),
    );

    let actions = client.pump_in();
    assert_eq!(
        client.session.negotiated(),
        Some(ProtoVer { major: 1, minor: 0 }),
        "min(0,1)"
    );
    let hello_back = actions.iter().find_map(|action| match action {
        SessionAction::Send {
            msg: Outbound::Ctrl(msg),
            ..
        } => Some(msg.clone()),
        _ => None,
    });
    let Some(CtrlMsg::HelloBack { proto, .. }) = hello_back else {
        panic!("从机应回 HelloBack：{actions:?}");
    };
    assert_eq!(
        proto,
        ProtoVer::CURRENT,
        "HelloBack 恒为本端 CURRENT（1.1；模拟 1.0 对端按 negotiate 接受）"
    );
    client.flush_out();

    // 模拟 1.0 主控收到 HelloBack{1,1}：negotiate(1.0, 1.1) 接受（共存成立）
    let plaintext = recv_decrypted(&mut host_wire, &mut sim_st);
    let frame = must("HelloBack 帧解码", decode_frame(&plaintext));
    let msg = must(
        "HelloBack 解码",
        xcontrol_proto::decode_ctrl(frame.header.kind, frame.payload),
    );
    let Some(CtrlMsg::HelloBack { proto, .. }) = msg else {
        panic!("应为 HelloBack：{msg:?}");
    };
    assert_eq!(
        negotiate(ProtoVer { major: 1, minor: 0 }, proto),
        Ok(ProtoVer { major: 1, minor: 0 }),
        "1.0 主控接受 1.1 从机：会话 minor 0，消息集回落 1.0"
    );
}

/// 场景 7b（docs/02 §5.2）：major 不等（1.0 ↔ 2.0）→ `Error{UnsupportedVersion}`
/// + 断开（VersionIncompatible）；对端收到断开前最后的 Error 帧。
#[test]
fn version_negotiation_major_mismatch_rejected() {
    let (client_wire, mut host_wire, _control) = LoopbackTransport::pair(None);
    let (client_st, mut sim_st) =
        handshake_pair_on(client_wire.clone(), host_wire.clone(), &good_psk());
    let mut client = Peer::new_client(client_wire, client_st);
    let actions = {
        let mut dispatcher = Dispatcher::new(&mut client.session);
        must(
            "Client 就绪",
            dispatcher.on_handshake_ready(client.st.handshake_hash()),
        )
    };
    assert!(actions.is_empty());

    let hello = CtrlMsg::Hello {
        proto: ProtoVer { major: 2, minor: 0 },
        name: "sim-host-2.0".into(),
        screen_name: "sim-screen".into(),
        instance_id: [8; 16],
        caps: Caps::UNICODE,
        salt: [3; 16],
    };
    send_encrypted(
        &mut sim_st,
        &mut host_wire,
        &must("Hello 编码", hello.encode_frame(0)),
    );

    let actions = client.pump_in();
    assert_eq!(actions.len(), 3, "Error 帧 + ReleaseAll + 断开通知");
    assert!(matches!(
        &actions[0],
        SessionAction::Send {
            msg: Outbound::Ctrl(CtrlMsg::Error { code, .. }),
            ..
        } if *code == xcontrol_proto::ErrCode::UNSUPPORTED_VERSION
    ));
    assert_eq!(actions[1], SessionAction::ReleaseAll);
    assert_eq!(
        actions[2],
        SessionAction::Notice(Notice::Disconnected(
            xcontrol_core::session::DisconnectReason::VersionIncompatible
        ))
    );
    assert_eq!(
        client.session.state(),
        &SessionState::Disconnected(xcontrol_core::session::DisconnectReason::VersionIncompatible)
    );
    client.flush_out();

    // 模拟 2.0 主控收到断开前最后的 Error 帧（明确拒绝原因，docs/02 §5.2）
    let plaintext = recv_decrypted(&mut host_wire, &mut sim_st);
    let frame = must("Error 帧解码", decode_frame(&plaintext));
    let msg = must(
        "Error 解码",
        xcontrol_proto::decode_ctrl(frame.header.kind, frame.payload),
    );
    assert_eq!(
        msg,
        Some(CtrlMsg::Error {
            code: xcontrol_proto::ErrCode::UNSUPPORTED_VERSION,
            msg: format!(
                "版本不兼容：本地 1.{}, 对端 2.0",
                xcontrol_proto::PROTO_MINOR
            ),
        })
    );

    // 会话终态：任何后续事件被拒绝
    let err = client
        .session
        .step(SessionEvent::Frame(FramePayload::Input(InputEvent::Key {
            code: KEY_C,
            state: KeyState::Down,
        })))
        .expect_err("终态拒绝");
    assert_eq!(err, StateError::AlreadyDisconnected);
}

// ————————————————————————— 场景 8：剪贴板全链路 —————————————————————————

/// 剪贴板链路：第二对回环 + 独立 Noise 握手 + [`ClipRx`]/[`ClipTx`] 全流程驱动
/// （docs/02 §4.4：握手后首帧 ChannelBind 绑定控制连接会话）。
struct ClipChannel {
    control: LoopbackControl,
    /// 推送方（announce 端）。
    tx_wire: LoopbackTransport,
    tx_st: SecureTransport,
    tx: ClipTx,
    /// 接收方（重组端）。
    rx_wire: LoopbackTransport,
    rx_st: SecureTransport,
    rx: ClipRx<TestClock>,
    /// 接收方待回帧（Ack）缓冲。
    rx_out: Vec<u8>,
    /// 帧 seq 计数（诊断字段）。
    seq: u32,
}

impl ClipChannel {
    /// 以默认协商上限（16MiB）建立剪贴板链路。
    fn connect(control_session_id: [u8; 16]) -> Self {
        Self::connect_with_limit(control_session_id, Options::defaults().clipboard_max_bytes)
    }

    /// 以自定义协商上限建立剪贴板链路（`Options.clipboard_max_bytes` 缩限）。
    fn connect_with_limit(control_session_id: [u8; 16], max_total_len: u32) -> Self {
        let (tx_wire, rx_wire, control) = LoopbackTransport::pair(None);
        let (tx_st, rx_st) = handshake_pair_on(tx_wire.clone(), rx_wire.clone(), &good_psk());
        Self {
            control,
            tx_wire,
            tx_st,
            tx: ClipTx::new(),
            rx_wire,
            rx_st,
            rx: ClipRx::new(
                ClipRxConfig { max_total_len },
                TestClock::new(),
                control_session_id,
            ),
            rx_out: Vec::new(),
            seq: 0,
        }
    }

    /// 推送方发出一条剪贴板消息。
    fn send(&mut self, msg: &dyn Message) {
        let mut frame = Vec::new();
        must("剪贴板帧编码", msg.encode_frame_into(self.seq, &mut frame));
        self.seq = self.seq.wrapping_add(1);
        self.send_frame(&frame);
    }

    /// 加密发送原始帧（违规场景构造用）。
    fn send_frame(&mut self, frame: &[u8]) {
        must("剪贴板发送", self.try_send_frame(frame));
    }

    /// 可失败发送（断开后的发送必须 Closed）。
    fn try_send_frame(&mut self, frame: &[u8]) -> Result<(), NetError> {
        let mut ciphertext = Vec::new();
        self.tx_st.encrypt_into(frame, &mut ciphertext)?;
        self.tx_wire.send(&ciphertext)
    }

    /// 接收方回帧（Ack）加密发送。
    fn flush_rx_out(&mut self) {
        let bytes = std::mem::take(&mut self.rx_out);
        if bytes.is_empty() {
            return;
        }
        send_encrypted(&mut self.rx_st, &mut self.rx_wire, &bytes);
    }

    /// 接收方向取一帧并解码（解码错误原样返回——违规场景断言用）。
    fn rx_try_recv(&mut self) -> Result<Option<ClipMsg>, ProtoError> {
        let mut ciphertext = Vec::new();
        match self.rx_wire.try_recv(&mut ciphertext) {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(error) => panic!("剪贴板链路意外关闭：{error:?}"),
        }
        let mut plaintext = Vec::new();
        must(
            "剪贴板解密",
            self.rx_st.decrypt_into(&ciphertext, &mut plaintext),
        );
        let frame = must("剪贴板帧解码", decode_frame(&plaintext));
        decode_clip(frame.header.kind, frame.payload)
    }

    /// 推送方向取一帧（Ack 回程）。
    fn tx_try_recv(&mut self) -> Option<ClipMsg> {
        let mut ciphertext = Vec::new();
        match self.tx_wire.try_recv(&mut ciphertext) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => panic!("剪贴板链路意外关闭：{error:?}"),
        }
        let mut plaintext = Vec::new();
        must(
            "Ack 解密",
            self.tx_st.decrypt_into(&ciphertext, &mut plaintext),
        );
        let frame = must("Ack 帧解码", decode_frame(&plaintext));
        must(
            "Ack 消息解码",
            decode_clip(frame.header.kind, frame.payload),
        )
    }

    /// 双向泵：接收端处理全部到达消息（`SendAck` 即刻回帧），推送方消费全部
    /// Ack；循环至双向队列空。返回全部接收端动作。
    fn sync(&mut self) -> Vec<ClipRxAction> {
        let mut all = Vec::new();
        loop {
            let mut progressed = false;
            while let Some(msg) = must("剪贴板消息解码", self.rx_try_recv()) {
                progressed = true;
                let actions = match msg {
                    ClipMsg::Bind(bind) => self.rx.on_bind(bind),
                    ClipMsg::Announce(announce) => self.rx.on_announce(announce),
                    ClipMsg::Chunk(chunk) => self.rx.on_chunk(chunk),
                    ClipMsg::End(end) => self.rx.on_end(end),
                    ClipMsg::Ack(_) => panic!("接收端不得收到 Ack"),
                };
                for action in &actions {
                    if let ClipRxAction::SendAck { seq } = action {
                        self.seq = self.seq.wrapping_add(1);
                        must(
                            "Ack 帧编码",
                            ClipAck { seq: *seq }.encode_frame_into(self.seq, &mut self.rx_out),
                        );
                    }
                }
                all.extend(actions);
            }
            if !self.rx_out.is_empty() {
                progressed = true;
                self.flush_rx_out();
            }
            while let Some(msg) = self.tx_try_recv() {
                progressed = true;
                match msg {
                    ClipMsg::Ack(ack) => assert!(
                        self.tx.on_ack(ack.seq),
                        "Ack 必须对应对途块（seq {}）",
                        ack.seq
                    ),
                    other => panic!("推送方只消费 Ack，实际 {other:?}"),
                }
            }
            if !progressed {
                break;
            }
        }
        all
    }

    /// 断开注入（违规处置，docs/02 §6「立即断开」）。
    fn disconnect(&self) {
        self.control.disconnect();
    }
}

/// 确定性剪贴板内容。
fn clip_content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// 场景 8a（docs/02 §6.3/§6.5、docs/04 §3）：mock 剪贴板内容变化 →
/// `ChannelBind → ClipAnnounce → ClipChunk×N（信用窗口 4）→ ClipAck 回程 →
/// ClipEnd → blake3 校验`全流程跑通，内容逐字节还原；控制连接不受影响。
#[test]
fn clipboard_full_flow_credit_window_and_blake3_verify() {
    let (host, client, _control) = setup_pair();
    let session_id = client.session.session_id().expect("协商完成有会话标识");
    let mut clip = ClipChannel::connect(session_id);

    // mock 剪贴板变化（主控复制 ~300KB → 5 块，必然触发信用窗口等待）
    let mut clipboard = MockClipboard::new();
    let content = clip_content(300_000);
    clipboard.set_content(content.clone());
    assert!(clipboard.has_changes_since(0), "写入即待同步变更");
    let plan = clipboard
        .transfer_plan(Options::defaults().clipboard_max_bytes)
        .expect("未超限应有传输计划");
    assert_eq!(plan.chunks.len(), 5, "300KB → 5×64KiB 内分块");

    // 首帧 ChannelBind + ClipAnnounce
    clip.send(&ChannelBind { session_id });
    clip.send(&ClipAnnounce {
        clip_id: 7,
        origin_id: host_identity().instance_id,
        origin_seq: 1,
        formats: vec![ClipFormat::Text],
        total_len: plan.total_len,
        blake3: plan.blake3,
    });

    // Chunk×N（发送端信用窗口：窗口空即泵 Ack 回程）
    let mut max_in_flight = 0usize;
    let mut all_actions = Vec::new();
    for (idx, data) in plan.chunks.iter().enumerate() {
        while !clip.tx.can_send() {
            all_actions.extend(clip.sync());
        }
        let seq = u16::try_from(idx).expect("块数 < u16");
        max_in_flight = max_in_flight.max(clip.tx.in_flight() + 1);
        assert!(clip.tx.can_send(), "第 {idx} 块应有信用（窗口 4）");
        clip.send(&ClipChunk {
            clip_id: 7,
            seq,
            data: data.clone(),
        });
        clip.tx.on_chunk_sent(seq);
    }
    all_actions.extend(clip.sync());

    assert_eq!(
        max_in_flight,
        xcontrol_core::clip_tx::INITIAL_CREDIT_CHUNKS,
        "信用窗口峰值 = 初始 4 块（docs/02 §6.3）"
    );
    let acks = all_actions
        .iter()
        .filter(|action| matches!(action, ClipRxAction::SendAck { .. }))
        .count();
    assert_eq!(acks, 5, "每块恰好一条信用归还");

    // ClipEnd → blake3 校验通过 → 交付
    clip.send(&ClipEnd { clip_id: 7 });
    let actions = clip.sync();
    let delivered = actions
        .iter()
        .find_map(|action| match action {
            ClipRxAction::Deliver { clip_id, content } => Some((*clip_id, content.clone())),
            _ => None,
        })
        .expect("ClipEnd 后应交付完整内容");
    assert_eq!(delivered.0, 7);
    assert_eq!(
        delivered.1, content,
        "重组内容逐字节一致（blake3 校验通过）"
    );

    // 控制连接不受剪贴板通道影响
    assert_eq!(
        host.session.state(),
        &SessionState::Running { paused: false }
    );
    assert_eq!(
        client.session.state(),
        &SessionState::Running { paused: false }
    );
}

/// 场景 8b（docs/02 §6.1）：total_len 超限按协议断开 —— ① 超 16MiB 协议默认
/// 上限在解码层即拒绝；② 超协商缩限在接收状态机违规断开。
#[test]
fn clipboard_oversize_announce_disconnects() {
    let (_host, client, _control) = setup_pair();
    let session_id = client.session.session_id().expect("协商完成有会话标识");

    // ① 16MiB + 1B：proto 解码层拒绝（信任边界在接收端，docs/02 §6.1）
    let mut clip = ClipChannel::connect(session_id);
    let over = ClipAnnounce {
        clip_id: 1,
        origin_id: [0; 16],
        origin_seq: 1,
        formats: vec![ClipFormat::Text],
        total_len: ClipAnnounce::DEFAULT_MAX_TOTAL_LEN + 1,
        blake3: [0; 32],
    };
    let frame = must("编码侧自律不校验 total_len", over.encode_frame(1));
    clip.send_frame(&frame);
    let error = clip.rx_try_recv().expect_err("解码层必须拒绝超限 announce");
    assert!(
        matches!(error, ProtoError::ClipTotalLenExceeded { .. }),
        "解码层违规：{error:?}"
    );
    clip.disconnect();
    assert!(
        matches!(clip.try_send_frame(&frame), Err(NetError::Closed)),
        "违规即断开剪贴板连接"
    );

    // ② 协商缩限（1024B）：16MiB 内但超协商值 → 接收状态机违规断开
    let mut clip = ClipChannel::connect_with_limit(session_id, 1024);
    clip.send(&ChannelBind { session_id });
    assert!(clip.sync().is_empty(), "合法绑定无动作");
    let within_default = ClipAnnounce {
        clip_id: 2,
        origin_id: [0; 16],
        origin_seq: 2,
        formats: vec![ClipFormat::Text],
        total_len: 2048,
        blake3: [0; 32],
    };
    clip.send(&within_default);
    let actions = clip.sync();
    assert_eq!(actions.len(), 1);
    assert!(
        matches!(&actions[0], ClipRxAction::Violation { .. }),
        "超协商上限违规：{actions:?}"
    );
    let frame = must("帧编码", within_default.encode_frame(9));
    clip.disconnect();
    assert!(matches!(clip.try_send_frame(&frame), Err(NetError::Closed)));
}

/// 场景 8c（docs/02 §6.2）：seq 乱序（跳号）= 违规断开；合法块照常 Ack。
#[test]
fn clipboard_out_of_order_seq_disconnects() {
    let (_host, client, _control) = setup_pair();
    let session_id = client.session.session_id().expect("协商完成有会话标识");
    let mut clip = ClipChannel::connect(session_id);

    clip.send(&ChannelBind { session_id });
    clip.send(&ClipAnnounce {
        clip_id: 3,
        origin_id: [0; 16],
        origin_seq: 1,
        formats: vec![ClipFormat::Text],
        total_len: 4,
        blake3: [0; 32],
    });

    // seq 0：合法（Ack）
    clip.send(&ClipChunk {
        clip_id: 3,
        seq: 0,
        data: vec![1, 2],
    });
    clip.tx.on_chunk_sent(0);
    let actions = clip.sync();
    assert_eq!(actions, vec![ClipRxAction::SendAck { seq: 0 }]);

    // seq 2（跳过 1）：乱序违规断开（docs/02 §6.2）
    let evil = ClipChunk {
        clip_id: 3,
        seq: 2,
        data: vec![3, 4],
    };
    clip.send(&evil);
    let actions = clip.sync();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], ClipRxAction::Violation { .. }));

    clip.disconnect();
    let frame = must("帧编码", evil.encode_frame(9));
    assert!(
        matches!(clip.try_send_frame(&frame), Err(NetError::Closed)),
        "乱序违规断开剪贴板连接"
    );
}

/// 场景 8d（docs/02 §6.6）：未 announce 先 chunk = 越界违规断开。
#[test]
fn clipboard_chunk_before_announce_disconnects() {
    let (_host, client, _control) = setup_pair();
    let session_id = client.session.session_id().expect("协商完成有会话标识");
    let mut clip = ClipChannel::connect(session_id);

    // 绑定合法，但 announce 未到
    clip.send(&ChannelBind { session_id });
    assert!(clip.sync().is_empty());

    let evil = ClipChunk {
        clip_id: 5,
        seq: 0,
        data: vec![0; 8],
    };
    clip.send(&evil);
    let actions = clip.sync();
    assert_eq!(actions.len(), 1);
    assert!(
        matches!(&actions[0], ClipRxAction::Violation { .. }),
        "未 announce 先 chunk 必须违规：{actions:?}"
    );

    clip.disconnect();
    let frame = must("帧编码", evil.encode_frame(9));
    assert!(matches!(clip.try_send_frame(&frame), Err(NetError::Closed)));
}
