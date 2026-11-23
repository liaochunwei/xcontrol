#![cfg(feature = "mock-platform")]
//! mock 虚拟剪贴板 × 剪贴板状态机全流程（docs/08 §4：`mock-platform` feature
//! 组合测试，`cargo test -p xcontrol-core --features mock-platform`）。
//!
//! 覆盖（prd Requirement 2）：
//! 1. 变更触发（watch 语义）→ 传输计划 → 信用窗口（4 块）全流程 → blake3 交付；
//! 2. 超限不传输（16MiB 协议默认上限 +1B，docs/04 §3）；
//! 3. 超时场景（发送方停滞 → 10s Clock 驱动清理重组状态，docs/02 §6.4）。
//!
//! 本文件直连纯逻辑状态机（无传输）；经 Noise/回环传输的全链路版本在
//! `xcontrol-net` tests/scenarios.rs。

use xcontrol_core::clip_rx::{CLIP_TIMEOUT_MS, ClipRx, ClipRxAction, ClipRxConfig};
use xcontrol_core::clip_tx::{ClipTx, INITIAL_CREDIT_CHUNKS};
use xcontrol_core::clock::TestClock;
use xcontrol_core::mock::MockClipboard;
use xcontrol_proto::Options;
use xcontrol_proto::clip::{ChannelBind, ClipAnnounce, ClipChunk, ClipEnd, ClipFormat};

const SESSION_ID: [u8; 16] = [0x77; 16];

/// 测试辅助取值（helpers 非 `#[test]` 作用域，allow-expect-in-tests 不覆盖——
/// 与 net tests/chain.rs 的 `must` 同形：panic 携带上下文）。
#[track_caller]
fn must<T, E: std::fmt::Debug>(context: &'static str, result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"),
    }
}

#[track_caller]
fn must_some<T>(context: &'static str, value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => panic!("{context}"),
    }
}

/// 确定性测试内容。
fn make_content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// 已绑定的接收端。
fn bound_rx(clock: TestClock) -> ClipRx<TestClock> {
    let mut rx = ClipRx::new(ClipRxConfig::default(), clock, SESSION_ID);
    let actions = rx.on_bind(ChannelBind {
        session_id: SESSION_ID,
    });
    assert!(actions.is_empty());
    rx
}

/// mock 剪贴板内容 → announce + 全部块（信用窗口 4：窗口空即消化对端 Ack）。
/// 返回全部接收端动作（SendAck/Deliver）。
fn run_scripted_transfer(
    clipboard: &MockClipboard,
    rx: &mut ClipRx<TestClock>,
    tx: &mut ClipTx,
    clip_id: u32,
) -> Vec<ClipRxAction> {
    let plan = must_some(
        "未超限应有传输计划",
        clipboard.transfer_plan(Options::defaults().clipboard_max_bytes),
    );

    let mut all = Vec::new();
    assert!(
        rx.on_announce(ClipAnnounce {
            clip_id,
            origin_id: [1; 16],
            origin_seq: 1,
            formats: vec![ClipFormat::Text],
            total_len: plan.total_len,
            blake3: plan.blake3,
        })
        .is_empty()
    );

    let mut pending_acks: Vec<u16> = Vec::new();
    for (idx, data) in plan.chunks.iter().enumerate() {
        let seq = must("块数 < u16", u16::try_from(idx));
        while !tx.can_send() {
            // 信用窗口空：消化接收端回程 Ack（对端每块必 Ack）
            let Some(ack) = pending_acks.pop() else {
                panic!("信用窗口空但无待消化 Ack（接收端未按块 Ack）");
            };
            assert!(tx.on_ack(ack), "Ack 必须对应对途块");
        }
        assert!(
            tx.in_flight() < INITIAL_CREDIT_CHUNKS,
            "信用窗口从未超过 4 块"
        );
        let actions = rx.on_chunk(ClipChunk {
            clip_id,
            seq,
            data: data.clone(),
        });
        for action in &actions {
            if let ClipRxAction::SendAck { seq: acked } = action {
                pending_acks.push(*acked);
            }
        }
        tx.on_chunk_sent(seq);
        all.extend(actions);
    }
    all.extend(rx.on_end(ClipEnd { clip_id }));
    all
}

/// 变更触发 → 全流程 → 完整交付（内容逐字节一致、信用窗口 ≤4）。
#[test]
fn mock_clipboard_full_flow_with_credit_window() {
    let mut clipboard = MockClipboard::new();
    let content = make_content(200_000);
    clipboard.set_content(content.clone());
    assert!(clipboard.has_changes_since(0), "写入即待同步变更");

    let clock = TestClock::new();
    let mut rx = bound_rx(clock);
    let mut tx = ClipTx::new();
    let actions = run_scripted_transfer(&clipboard, &mut rx, &mut tx, 42);

    let acks = actions
        .iter()
        .filter(|action| matches!(action, ClipRxAction::SendAck { .. }))
        .count();
    assert_eq!(
        acks, 4,
        "200KB → 4 块（3×64KiB + 1×3392B），每块恰好一条信用归还"
    );
    let delivered = actions
        .iter()
        .find_map(|action| match action {
            ClipRxAction::Deliver { clip_id, content } => Some((*clip_id, content.clone())),
            _ => None,
        })
        .expect("ClipEnd 后应交付");
    assert_eq!(delivered.0, 42);
    assert_eq!(delivered.1, content, "重组内容逐字节一致");
}

/// 超限不传输（docs/04 §3）：16MiB 协议默认上限 +1B → 无传输计划。
#[test]
fn oversize_content_is_not_transferred() {
    let mut clipboard = MockClipboard::new();
    clipboard.set_content(vec![0u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize + 1]);
    assert!(
        clipboard
            .transfer_plan(Options::defaults().clipboard_max_bytes)
            .is_none(),
        "超 16MiB 协议默认上限不传输（托盘提示语义由上层呈现）"
    );
}

/// 超时场景（docs/02 §6.4）：发送方停滞（只发 announce + 前缀块）→
/// 10s Clock 驱动清理重组状态；迟到的 chunk 视为未 announce（违规）。
#[test]
fn timeout_script_discards_partial_transfer() {
    let mut clipboard = MockClipboard::new();
    let content = make_content(200_000);
    clipboard.set_content(content.clone());
    let plan = must_some(
        "未超限应有传输计划",
        clipboard.transfer_plan(Options::defaults().clipboard_max_bytes),
    );
    assert!(plan.chunks.len() > 1, "多块内容才能演示中途停滞");

    let clock = TestClock::new();
    let mut rx = bound_rx(clock.clone());
    assert!(
        rx.on_announce(ClipAnnounce {
            clip_id: 7,
            origin_id: [1; 16],
            origin_seq: 1,
            formats: vec![ClipFormat::Text],
            total_len: plan.total_len,
            blake3: plan.blake3,
        })
        .is_empty()
    );

    // 发送前缀块后停滞（超时脚本：发送方不再产出；Ack 归还属发送端记账，本场景不涉）
    for (idx, data) in plan.chunks[..2].iter().enumerate() {
        let seq = u16::try_from(idx).expect("块数 < u16");
        let actions = rx.on_chunk(ClipChunk {
            clip_id: 7,
            seq,
            data: data.clone(),
        });
        assert!(
            actions
                .iter()
                .all(|action| matches!(action, ClipRxAction::SendAck { .. }))
        );
    }

    // 10s 未完成 → 丢弃重组状态（软失败，连接保持）
    clock.advance(CLIP_TIMEOUT_MS);
    let actions = rx.on_tick();
    assert!(
        matches!(actions[0], ClipRxAction::Drop { .. }),
        "超时清理并告警"
    );

    // 迟到的 chunk = 未 announce → 违规断开
    let actions = rx.on_chunk(ClipChunk {
        clip_id: 7,
        seq: 2,
        data: plan.chunks[2].clone(),
    });
    assert!(matches!(actions[0], ClipRxAction::Violation { .. }));
}

/// M5 图片端到端（11-19-m5-clipboard-full，FR-4.2；PRD 验收"图片
/// announce→chunk→end 全链路"）：wire PNG 大载荷（500KB，多块）经
/// `formats=[Png]` announce → 分块 → 信用窗口 → blake3 交付全链路。
#[test]
fn image_payload_full_flow_with_credit_window() {
    // 确定性"图片"载荷：500KB（8 块量级，信用窗口翻转多轮）。
    let png_wire: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let mut clipboard = MockClipboard::new();
    clipboard.set_content(png_wire.clone());
    assert!(clipboard.has_changes_since(0), "图片写入即待同步变更");

    let clock = TestClock::new();
    let mut rx = bound_rx(clock);
    let mut tx = ClipTx::new();
    let actions = run_scripted_transfer(&clipboard, &mut rx, &mut tx, 100);

    let acks = actions
        .iter()
        .filter(|action| matches!(action, ClipRxAction::SendAck { .. }))
        .count();
    assert_eq!(
        acks, 8,
        "500KB / 65472B = 8 块，每块恰好一条信用归还（窗口 ≤4 多轮翻转）"
    );
    let delivered = actions
        .iter()
        .find_map(|action| match action {
            ClipRxAction::Deliver { clip_id, content } => Some((*clip_id, content.clone())),
            _ => None,
        })
        .expect("ClipEnd 后应交付");
    assert_eq!(delivered.0, 100);
    assert_eq!(delivered.1, png_wire, "wire PNG 载荷逐字节一致");
}

/// M5 HTML 端到端（FR-4.3）：fragment 载荷经 `formats=[Html]` 全链路交付。
#[test]
fn html_payload_full_flow() {
    let fragment = "<b>xcontrol</b> <i>αβγ</i>".repeat(500);
    let mut clipboard = MockClipboard::new();
    clipboard.set_content(fragment.as_bytes().to_vec());

    let clock = TestClock::new();
    let mut rx = bound_rx(clock);
    let mut tx = ClipTx::new();
    let actions = run_scripted_transfer(&clipboard, &mut rx, &mut tx, 101);

    let delivered = actions
        .iter()
        .find_map(|action| match action {
            ClipRxAction::Deliver { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("ClipEnd 后应交付");
    assert_eq!(delivered, fragment.as_bytes(), "HTML fragment 逐字节一致");
}

/// M5 多格式并存端到端（docs/04 §2"一次 ClipAnnounce 可携带多格式"）：
/// `formats=[Text, Png, Html]` 三段布局（docs/02 §6 v1 补充）——分段编码 →
/// 单传输分块交付 → 接收端按段数解码还原（段序 == formats 序）。
#[test]
fn multi_format_announce_segment_layout_full_flow() {
    use xcontrol_proto::clip::{decode_content_segments, encode_content_segments};

    let text = "hello 剪贴板\n".repeat(400);
    let png_wire: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let html = "<p>fragment</p>".repeat(200);

    // 三段布局编码（单传输的 content = postcard Vec<Vec<u8>>，docs/02 §6）。
    let content = encode_content_segments(&[text.as_bytes(), &png_wire, html.as_bytes()])
        .expect("三段布局编码");
    let mut clipboard = MockClipboard::new();
    clipboard.set_content(content.clone());

    let formats = vec![ClipFormat::Text, ClipFormat::Png, ClipFormat::Html];
    let clock = TestClock::new();
    let mut rx = bound_rx(clock);
    let mut tx = ClipTx::new();
    let plan = must_some(
        "未超限应有传输计划",
        clipboard.transfer_plan(Options::defaults().clipboard_max_bytes),
    );
    assert!(
        rx.on_announce(ClipAnnounce {
            clip_id: 102,
            origin_id: [1; 16],
            origin_seq: 1,
            formats,
            total_len: plan.total_len,
            blake3: plan.blake3,
        })
        .is_empty()
    );

    let mut delivered = None;
    let mut pending_acks: Vec<u16> = Vec::new();
    for (idx, data) in plan.chunks.iter().enumerate() {
        let seq = must("块数 < u16", u16::try_from(idx));
        while !tx.can_send() {
            let Some(ack) = pending_acks.pop() else {
                panic!("信用窗口空但无待消化 Ack");
            };
            assert!(tx.on_ack(ack));
        }
        for action in rx.on_chunk(ClipChunk {
            clip_id: 102,
            seq,
            data: data.clone(),
        }) {
            match action {
                ClipRxAction::SendAck { seq: acked } => pending_acks.push(acked),
                ClipRxAction::Deliver { content, .. } => delivered = Some(content),
                other => panic!("多格式全链路不应出现 {other:?}"),
            }
        }
        tx.on_chunk_sent(seq);
    }
    for action in rx.on_end(ClipEnd { clip_id: 102 }) {
        if let ClipRxAction::Deliver { content, .. } = action {
            delivered = Some(content);
        }
    }
    let delivered = delivered.expect("ClipEnd 后应交付");
    assert_eq!(delivered, content, "多格式 content 逐字节一致");

    // 按段数解码（接收端按 announce.formats.len() 选择布局，docs/02 §6）。
    let segments = decode_content_segments(&delivered, 3).expect("三段解码");
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0], text.as_bytes(), "段 0 = text/plain");
    assert_eq!(segments[1], png_wire, "段 1 = image/png");
    assert_eq!(segments[2], html.as_bytes(), "段 2 = text/html");
}

/// M5 图片 16MiB 上限复核（FR-4.5）：PNG 载荷同受协议默认上限约束——恰好
/// 16MiB 可传、16MiB+1 拒绝（与文本同口径，上限按 wire PNG 字节计）。
#[test]
fn image_oversize_limit_matches_text_semantics() {
    let at_limit: Vec<u8> = vec![0x89u8; ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize];
    let mut clipboard = MockClipboard::new();
    clipboard.set_content(at_limit);
    let plan = clipboard
        .transfer_plan(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
        .expect("恰好 16MiB 图片可传输");
    assert_eq!(plan.total_len, ClipAnnounce::DEFAULT_MAX_TOTAL_LEN);

    let mut clipboard = MockClipboard::new();
    clipboard.set_content(vec![
        0x89u8;
        ClipAnnounce::DEFAULT_MAX_TOTAL_LEN as usize + 1
    ]);
    assert!(
        clipboard
            .transfer_plan(ClipAnnounce::DEFAULT_MAX_TOTAL_LEN)
            .is_none(),
        "16MiB+1 图片超限不传输（FR-4.5 复核结论：16MiB 对图片适用，docs/04 §2）"
    );
}
