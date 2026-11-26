//! 全链路回环集成测试（docs/02 §3/§4、implement.md 步骤 3）：
//!
//! ```text
//! mock 本地命令 → proto 编码 → Noise 加密 → 回环传输 → 解密 → 解码
//!   → core Session/Dispatcher → RecordingSink 断言（事件到达）
//! ```
//!
//! 覆盖场景（prd 验收）：
//! 1. 握手成功全链路（Noise 握手 + Hello/HelloBack 协商 + EdgeCrossing 输入注入 +
//!    Ping/Pong 心跳）；
//! 2. 握手超时（deadline 注入短值演示 5s 同路径；会话保持 Idle）；
//! 3. 安全码不匹配（双方 HandshakeFailed，无部分匹配反馈）；
//! 4. 密文无明文特征（回环捕获断言，docs/02 §0）；
//! 5. 防爆破限速（5 失败 → 退避拒绝第 6 次 + 成功重置窗口，docs/02 §3.2）。
//!
//! dev-dep 依赖说明：`xcontrol-core` 仅本集成测试消费（Session/Dispatcher/
//! InputSink/derive_session_id）；生产依赖方向仍为 net → proto + config
//! （docs/08 §1；xtask 白名单已登记 "chain integration tests only"）。

use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use xcontrol_core::clock::TestClock;
use xcontrol_core::mock::RecordingInputSink;
use xcontrol_core::route::{Dispatcher, apply_actions};
use xcontrol_core::session::{
    Identity, LocalCmd, Notice, Outbound, Role, Session, SessionAction, SessionState,
};
use xcontrol_net::noise::HANDSHAKE_TIMEOUT;
use xcontrol_net::transport::Transport;
use xcontrol_net::{
    Admission, HandshakeDeadline, HandshakeRole, HandshakeThrottle, LoopbackTransport, NetError,
    NoiseHandshake, SecureTransport,
};
use xcontrol_proto::input::{InputEvent, KeyState, ScreenEdge};
use xcontrol_proto::{Caps, CtrlMsg, Message, Options, ProtoVer, xkey};

/// 测试假码派生（docs/08 §4：fixture/测试永不用真实安全码）。
/// argon2 m=19MiB 每码只派生一次，全测试文件共享（缓存语义实测）。
fn good_psk() -> [u8; 32] {
    static PSK: OnceLock<[u8; 32]> = OnceLock::new();
    *PSK.get_or_init(|| {
        must(
            "PSK 派生",
            xcontrol_net::derive_psk("test-code", &[0u8; 16]),
        )
    })
}

/// 不匹配的假码（安全码不匹配场景用）。
fn bad_psk() -> [u8; 32] {
    static PSK: OnceLock<[u8; 32]> = OnceLock::new();
    *PSK.get_or_init(|| {
        must(
            "PSK 派生",
            xcontrol_net::derive_psk("wrong-code", &[0u8; 16]),
        )
    })
}

/// 测试超时兜底（握手正常 <100ms；2s 足够宽）。
fn test_deadline() -> HandshakeDeadline {
    HandshakeDeadline::after(Duration::from_secs(2))
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
        NoiseHandshake::run(&mut wire, HandshakeRole::Initiator, &psk, &test_deadline())
    });
    let host = thread::spawn(move || {
        let mut wire = host_wire;
        NoiseHandshake::run(&mut wire, HandshakeRole::Responder, &psk, &test_deadline())
    });
    let client_st = must("client 握手成功", must("client 线程", client.join()));
    let host_st = must("host 握手成功", must("host 线程", host.join()));
    (client_st, host_st)
}

/// 测试辅助取值（非 `#[test]` 作用域的 helpers 无法被 clippy.toml 的
/// allow-expect-in-tests 覆盖——与 proto benches 的 `must` 同形：panic 携带上下文）。
#[track_caller]
fn must<T, E: std::fmt::Debug>(context: &'static str, result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"),
    }
}

/// 记录型注入 sink：core mock 平台层的 [`RecordingInputSink`]
/// （11-16-m1-mock-chain：T3 简版实现迁移至 `xcontrol-core` mock 模块）。
type RecordingSink = RecordingInputSink;

/// 出站动作编码缓冲：`apply_actions` 的 on_send 回调目标（动作 → XCP 帧）。
#[derive(Default)]
struct OutboundBuffer {
    bytes: Vec<u8>,
}

impl OutboundBuffer {
    fn on_send(&mut self) -> impl FnMut(u32, &Outbound) + '_ {
        |seq, outbound| {
            outbound
                .encode_frame_into(seq, &mut self.bytes)
                .unwrap_or_else(|error| panic!("出站帧编码失败（测试输入合法）：{error:?}"));
        }
    }

    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

/// 加密发送一整段明文帧流（应用帧整体进单条 Noise 报文，docs/02 §4.1 在密文内）。
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

/// 协商最小序列（握手后的同一回环双端）：Host 就绪发 Hello → Client 回
/// HelloBack → 双方 Running + HandshakeComplete 通知。
fn run_negotiation(
    client_wire: &mut LoopbackTransport,
    host_wire: &mut LoopbackTransport,
    client_st: &mut SecureTransport,
    host_st: &mut SecureTransport,
    client_session: &mut Session<TestClock>,
    host_session: &mut Session<TestClock>,
    sink: &mut RecordingSink,
) {
    // 双方就绪（net → core 边界：双侧 HandshakeReady；Client 侧无出站动作）。
    let client_ready = must(
        "Client 接受 HandshakeReady",
        Dispatcher::new(client_session).on_handshake_ready(client_st.handshake_hash()),
    );
    assert!(client_ready.is_empty(), "从机就绪无出站");
    assert_eq!(client_session.state(), &SessionState::Handshaking);

    let mut host_out = OutboundBuffer::default();
    let actions = must(
        "Host 接受 HandshakeReady",
        Dispatcher::new(host_session).on_handshake_ready(host_st.handshake_hash()),
    );
    assert_eq!(actions.len(), 1, "Host 就绪即发 Hello（seq 0）");
    assert_eq!(host_session.state(), &SessionState::Handshaking);
    apply_actions(&actions, sink, &mut host_out.on_send());
    send_encrypted(host_st, host_wire, &host_out.take());

    let mut client_out = OutboundBuffer::default();
    {
        let plaintext = recv_decrypted(client_wire, client_st);
        let mut dispatcher = Dispatcher::new(client_session);
        let outcome = dispatcher.feed(&plaintext);
        assert!(outcome.fatal.is_none(), "Hello 解码与转移合法");
        assert_eq!(
            client_session.state(),
            &SessionState::Running { paused: false }
        );
        assert!(
            outcome.actions.iter().any(|action| matches!(
                action,
                SessionAction::Notice(Notice::HandshakeComplete {
                    minor: xcontrol_proto::PROTO_MINOR,
                    ..
                })
            )),
            "协商完成通知（minor = min(双端 CURRENT)，随 PROTO_MINOR 锚点走）"
        );
        apply_actions(&outcome.actions, sink, &mut client_out.on_send());
    }
    send_encrypted(client_st, client_wire, &client_out.take());

    {
        let plaintext = recv_decrypted(host_wire, host_st);
        let mut dispatcher = Dispatcher::new(host_session);
        let outcome = dispatcher.feed(&plaintext);
        assert!(outcome.fatal.is_none());
        assert_eq!(
            host_session.state(),
            &SessionState::Running { paused: false }
        );
        assert!(outcome.actions.iter().any(|action| matches!(
            action,
            SessionAction::Notice(Notice::HandshakeComplete { .. })
        )));
    }
    assert_eq!(
        client_session.negotiated(),
        Some(ProtoVer {
            major: 1,
            minor: xcontrol_proto::PROTO_MINOR
        })
    );
    assert_eq!(
        host_session.negotiated(),
        Some(ProtoVer {
            major: 1,
            minor: xcontrol_proto::PROTO_MINOR
        })
    );
}

/// 场景 1：全链路 —— Noise 握手 → 版本协商 → EdgeCrossing 输入注入（EnterScreen
/// 到达被控侧 sink）→ Ping/Pong 心跳；全程同一回环双端。
#[test]
fn full_chain_handshake_negotiation_input_and_heartbeat() {
    let (mut client_wire, mut host_wire, _control) = LoopbackTransport::pair(None);
    let (mut client_st, mut host_st) =
        handshake_pair_on(client_wire.clone(), host_wire.clone(), &good_psk());

    // 会话标识：双方一致且与 core 派生等价（docs/02 §3.3 跨 crate 契约）。
    assert_eq!(client_st.session_id(), host_st.session_id());
    assert_eq!(
        client_st.session_id(),
        xcontrol_core::derive_session_id(&client_st.handshake_hash())
    );

    let mut client_session = Session::new(
        Role::Client,
        client_identity(),
        TestClock::new(),
        Options::defaults(),
    );
    let host_clock = TestClock::new();
    let mut host_session = Session::new(
        Role::Host,
        host_identity(),
        host_clock.clone(),
        Options::defaults(),
    );
    let mut client_sink = RecordingSink::default();
    let mut host_sink = RecordingSink::default();

    run_negotiation(
        &mut client_wire,
        &mut host_wire,
        &mut client_st,
        &mut host_st,
        &mut client_session,
        &mut host_session,
        &mut host_sink,
    );

    // —— 输入链路：Host 本地边缘穿越 → LeaveScreen + EnterScreen → Client 注入 ——
    let mut host_out = OutboundBuffer::default();
    let actions = Dispatcher::new(&mut host_session)
        .on_local(LocalCmd::EdgeCrossing {
            target: 1,
            edge: ScreenEdge::Right,
            ratio: 0.5,
        })
        .expect("Running 接受 EdgeCrossing");
    assert_eq!(
        actions.len(),
        3,
        "ReleaseAll + Send(LeaveScreen) + Send(EnterScreen)"
    );
    apply_actions(&actions, &mut host_sink, &mut host_out.on_send());
    assert_eq!(
        host_sink.release_count(),
        1,
        "切出先释放本机按键（docs/03 §5）"
    );
    send_encrypted(&mut host_st, &mut host_wire, &host_out.take());

    {
        let plaintext = recv_decrypted(&mut client_wire, &mut client_st);
        let mut client_out = OutboundBuffer::default();
        let mut dispatcher = Dispatcher::new(&mut client_session);
        let outcome = dispatcher.feed(&plaintext);
        assert!(outcome.fatal.is_none());
        // EnterScreen 到达：ReleaseAll（防卡键）+ LeftControl + WarpPointer + 受控通知。
        assert!(
            outcome.actions.iter().any(|action| matches!(
                action,
                SessionAction::Notice(Notice::EnteredControl { target: 1 })
            )),
            "受控开始通知"
        );
        apply_actions(
            &outcome.actions,
            &mut client_sink,
            &mut client_out.on_send(),
        );
        assert!(client_out.take().is_empty(), "被控侧无回程帧");
    }
    assert_eq!(
        client_sink.warps(),
        &[(1, 0.005, 0.5)],
        "滑出 Right 边 → 进入边 Left → warp(1, x=0.005, y=0.5)——11-20 进入边语义+死区"
    );
    assert_eq!(
        client_sink.release_count(),
        1,
        "LeaveScreen 到达也执行防卡键释放"
    );

    // —— 心跳：Host 3s Timer → Ping(seq1) + ModifierSync(seq2，本机为控制方) → Client Pong ——
    host_clock.advance(3000);
    let mut host_out = OutboundBuffer::default();
    let actions = Dispatcher::new(&mut host_session)
        .on_timer()
        .expect("Running 接受 Timer");
    assert_eq!(
        actions.len(),
        2,
        "Ping + ModifierSync（EdgeCrossing 后本机为控制方，单向快照开启）"
    );
    assert!(
        matches!(&actions[0],
            SessionAction::Send { seq: 3, msg }
            if matches!(msg, Outbound::Ctrl(CtrlMsg::Ping { ts_ms: _ }))),
        "心跳 Ping 延续单调 seq（Hello=0、LeaveScreen/EnterScreen=1/2 → Ping=3）"
    );
    assert!(
        matches!(&actions[1],
            SessionAction::Send { seq: 4, msg }
            if matches!(msg, Outbound::Input(InputEvent::ModifierSync { mods }) if mods.is_empty())),
        "周期快照仅控制方外发（docs/03 §3 单向语义）"
    );
    apply_actions(&actions, &mut host_sink, &mut host_out.on_send());
    send_encrypted(&mut host_st, &mut host_wire, &host_out.take());

    let mut client_out = OutboundBuffer::default();
    {
        let plaintext = recv_decrypted(&mut client_wire, &mut client_st);
        let mut dispatcher = Dispatcher::new(&mut client_session);
        let outcome = dispatcher.feed(&plaintext);
        assert!(outcome.fatal.is_none());
        assert_eq!(outcome.actions.len(), 1, "仅 Send(Pong)");
        assert!(
            matches!(&outcome.actions[0], SessionAction::Send { msg, .. }
                if matches!(msg, Outbound::Ctrl(CtrlMsg::Pong { ts_ms: _ })))
        );
        apply_actions(
            &outcome.actions,
            &mut client_sink,
            &mut client_out.on_send(),
        );
    }
    send_encrypted(&mut client_st, &mut client_wire, &client_out.take());

    {
        let plaintext = recv_decrypted(&mut host_wire, &mut host_st);
        let mut dispatcher = Dispatcher::new(&mut host_session);
        let outcome = dispatcher.feed(&plaintext);
        assert!(outcome.fatal.is_none());
        assert!(outcome.actions.is_empty(), "Pong 无动作，连接保持");
    }
    assert_eq!(
        client_session.state(),
        &SessionState::Running { paused: false }
    );
    assert_eq!(
        host_session.state(),
        &SessionState::Running { paused: false }
    );
}

/// 场景 2：握手超时 —— 对端静默，deadline 到期 HandshakeFailed；会话保持 Idle
/// （net 失败不通知 core；生产 5s 常量与测试短值同一路径，docs/02 §3.2）。
#[test]
fn handshake_timeout_keeps_session_idle() {
    let (mut client_wire, _host_wire, _control) = LoopbackTransport::pair(None);
    let client_session = Session::new(
        Role::Client,
        client_identity(),
        TestClock::new(),
        Options::defaults(),
    );

    let started = std::time::Instant::now();
    let result = NoiseHandshake::run(
        &mut client_wire,
        HandshakeRole::Initiator,
        &good_psk(),
        &HandshakeDeadline::after(Duration::from_millis(100)),
    );
    assert!(
        matches!(result, Err(NetError::HandshakeFailed)),
        "超时折叠为 HandshakeFailed"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(90),
        "真实等待 deadline"
    );
    assert_eq!(
        client_session.state(),
        &SessionState::Idle,
        "失败不产生 HandshakeReady"
    );
    // 生产常量锚点（集成演示 5s 语义：同路径、不同参数）
    assert_eq!(HANDSHAKE_TIMEOUT, Duration::from_secs(5));
}

/// 场景 3：安全码不匹配 —— 双方 HandshakeFailed，会话未建立（无部分匹配反馈，
/// docs/02 §0：对端只见断开）。
#[test]
fn handshake_psk_mismatch_on_chain() {
    let (client_wire, host_wire, _control) = LoopbackTransport::pair(None);
    // 双端克隆：先失败方（responder 在 m1 解密即败）shutdown 唤醒仍在等待的对端
    // （模拟"失败即断开 TCP"，docs/02 §3.2）。
    let mut wake_client = client_wire.clone();
    let mut wake_host = host_wire.clone();

    let client = thread::spawn(move || {
        let mut wire = client_wire;
        NoiseHandshake::run(
            &mut wire,
            HandshakeRole::Initiator,
            &good_psk(),
            &test_deadline(),
        )
    });
    let host = thread::spawn(move || {
        let mut wire = host_wire;
        NoiseHandshake::run(
            &mut wire,
            HandshakeRole::Responder,
            &bad_psk(),
            &test_deadline(),
        )
    });

    let host_result = host.join().expect("host 线程");
    wake_host.shutdown();
    wake_client.shutdown();
    let client_result = client.join().expect("client 线程");

    assert!(
        matches!(host_result, Err(NetError::HandshakeFailed)),
        "主控侧（m1 解密失败）"
    );
    assert!(
        matches!(client_result, Err(NetError::HandshakeFailed)),
        "从机侧（对端断开折叠）"
    );

    let session = Session::new(
        Role::Client,
        client_identity(),
        TestClock::new(),
        Options::defaults(),
    );
    assert_eq!(session.state(), &SessionState::Idle, "不匹配不会话");
}

/// 场景 4：密文无明文特征 —— 同一链路自握手起全程捕获，断言密文不含身份名/
/// prologue/明文帧字节，且符合 Noise 报文形态（明文 + 16B AEAD tag）。
#[test]
fn ciphertext_leaks_no_plaintext_markers() {
    let (mut client_wire, mut host_wire, control) = LoopbackTransport::pair(None);
    control.set_capture(true);
    let (mut client_st, mut host_st) =
        handshake_pair_on(client_wire.clone(), host_wire.clone(), &good_psk());

    let mut client_session = Session::new(
        Role::Client,
        client_identity(),
        TestClock::new(),
        Options::defaults(),
    );
    let mut host_session = Session::new(
        Role::Host,
        host_identity(),
        TestClock::new(),
        Options::defaults(),
    );
    let mut sink = RecordingSink::default();
    run_negotiation(
        &mut client_wire,
        &mut host_wire,
        &mut client_st,
        &mut host_st,
        &mut client_session,
        &mut host_session,
        &mut sink,
    );

    // 额外发一帧已知明文（Key 事件）供比对
    let key_frame = InputEvent::Key {
        code: xkey::KEY_A,
        state: KeyState::Down,
    }
    .encode_frame(42)
    .expect("Key 帧编码");
    send_encrypted(&mut host_st, &mut host_wire, &key_frame);
    let roundtrip = recv_decrypted(&mut client_wire, &mut client_st);
    assert_eq!(roundtrip, key_frame, "解密保真");

    let captured = control.take_captured();
    // 握手 m1/m2/m3（3）+ Hello + HelloBack（2）+ Key 帧（1）
    assert_eq!(
        captured.len(),
        6,
        "全程捕获：3 握手报文 + 2 协商帧 + 1 输入帧"
    );

    // 明文特征集：双方身份名/屏名、prologue——密文中一处都不许出现
    for marker in [
        &b"host"[..],
        b"client",
        b"host-screen",
        b"client-screen",
        b"xcontrol/1",
    ] {
        assert!(
            captured
                .iter()
                .all(|packet| find_subslice(packet, marker).is_none()),
            "捕获密文不得包含明文特征 {marker:?}"
        );
    }
    // 完整明文帧不得以任何形态出现在密文中
    for packet in &captured {
        assert!(
            find_subslice(packet, &key_frame).is_none(),
            "密文不得包含明文帧字节"
        );
    }
    // Noise 报文形态：密文 = 明文 + 16B AEAD tag（ChaChaPoly）
    let key_packet = captured.last().expect("Key 帧密文已捕获");
    assert_eq!(key_packet.len(), key_frame.len() + 16, "ChaChaPoly tag 16B");
}

/// 场景 5：防爆破限速 —— 5 次真实失败握手后，第 6 次尝试在触网前被本地退避
/// 拒绝；per-peer 独立；成功重置窗口（docs/02 §3.2）。
#[test]
fn rate_limit_blocks_sixth_attempt_after_five_failures() {
    let mut throttle: HandshakeThrottle<&'static str> = HandshakeThrottle::new();
    let base = std::time::Instant::now();
    // 合成时刻序列：每次失败间隔 2.5s（避开 2s Cooldown，聚焦退避语义）
    let attempt_at =
        |attempt: usize| base + Duration::from_millis(u64::try_from(attempt).unwrap_or(0) * 2500);

    for attempt in 0..5_usize {
        // 真实触网：错误安全码 → 双方 HandshakeFailed（失败即断开由 shutdown 模拟）
        let (client_wire, host_wire, _control) = LoopbackTransport::pair(None);
        let mut wake_client = client_wire.clone();
        let mut wake_host = host_wire.clone();
        let client = thread::spawn(move || {
            let mut wire = client_wire;
            NoiseHandshake::run(
                &mut wire,
                HandshakeRole::Initiator,
                &good_psk(),
                &test_deadline(),
            )
        });
        let host = thread::spawn(move || {
            let mut wire = host_wire;
            NoiseHandshake::run(
                &mut wire,
                HandshakeRole::Responder,
                &bad_psk(),
                &test_deadline(),
            )
        });
        let host_result = host.join().expect("host 线程");
        wake_host.shutdown();
        wake_client.shutdown();
        let client_result = client.join().expect("client 线程");
        assert!(matches!(host_result, Err(NetError::HandshakeFailed)));
        assert!(matches!(client_result, Err(NetError::HandshakeFailed)));

        throttle.record_failure("client-1", attempt_at(attempt));
        if attempt < 4 {
            assert_eq!(
                throttle.admit(&"client-1", attempt_at(attempt + 1)),
                Admission::Allowed,
                "第 {} 次失败后尚未达阈值（5）",
                attempt + 1
            );
        }
    }

    // 5 次失败 → 退避：第 6 次尝试触网前被本地拒绝（对端无感知，无部分匹配反馈）
    let sixth = attempt_at(5);
    assert!(
        matches!(
            throttle.admit(&"client-1", sixth),
            Admission::Backoff { .. }
        ),
        "第 5 次失败后进入指数退避（10s 起）"
    );
    assert_eq!(throttle.failure_count(&"client-1"), 5);

    // per-peer 键独立：其他候选不受牵连
    assert_eq!(throttle.admit(&"client-2", sixth), Admission::Allowed);

    // 成功握手 + record_success → 窗口重置（docs/02 §3.2）
    let (client_wire, host_wire, _control) = LoopbackTransport::pair(None);
    let (client_st, host_st) = handshake_pair_on(client_wire, host_wire, &good_psk());
    assert_eq!(client_st.session_id(), host_st.session_id());
    throttle.record_success(&"client-1");
    assert_eq!(
        throttle.admit(&"client-1", base + Duration::from_secs(30)),
        Admission::Allowed,
        "成功即窗口重置"
    );
}

/// 子串查找（捕获断言用）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
