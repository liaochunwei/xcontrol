//! 剪贴板接收端重组状态机（docs/02 §6 / docs/04 §3·§6；纯逻辑、零 I/O）。
//!
//! 位置与边界（design.md）：剪贴板消息走独立连接（docs/02 §4.4），不经过
//! [`Session`](crate::session::Session) 的控制通道转移表——本模块与发送端
//! [`ClipTx`](crate::clip_tx::ClipTx) 由连接接线层持有（M2 接入真实 TCP；
//! M1 由 mock 回环集成测试全流程驱动）。信任边界在接收端（docs/02 §6.7）：
//!
//! | 校验 | 违规处置 | 依据 |
//! | --- | --- | --- |
//! | `ChannelBind` 首帧 + 会话匹配 + 绑定唯一 | `Violation`（断开剪贴板连接） | docs/02 §4.4 |
//! | `total_len` ≤ 协商上限 | `Violation` | docs/02 §6.1 |
//! | 每帧累计长度 ≤ `total_len` | `Violation` | docs/02 §6.2 |
//! | seq 连续（跳号=乱序违规；重复=去重忽略） | 跳号 `Violation` | docs/02 §6.2 |
//! | 未 announce 先 chunk / 先 end | `Violation` | docs/02 §6.6 |
//! | announce 后 10s 未完成 | `Drop`（丢弃重组状态并告警，不断连） | docs/02 §6.4 |
//! | `ClipEnd` blake3 校验失败 / 不完整 | `Drop`（剪贴板语义允许失败，不重传） | docs/02 §6.5 |
//!
//! `ClipAck` 无 `clip_id` ⇒ **单连接单活动 clip**（docs/02 §6.3 proto 注）；
//! 活动传输未结束又收到 announce 属违规。防回环三防线（docs/04 §5）中
//! 防线 1/3 属写入侧（平台剪贴板 crate）、防线 2 的 origin 去重属上层
//! 通知语义，均不在本接收重组状态机内。
//!
//! 超时判定 Clock 驱动：接线层周期调用 [`ClipRx::on_tick`]（与会话 Timer
//! 同节奏）；消息入口自身不判超时——慢速但守约的传输只在 tick 点被清理。

use xcontrol_proto::clip::{ChannelBind, ClipAnnounce, ClipChunk, ClipEnd};

use crate::clock::Clock;

/// 单 clip 完成时限：10s（docs/02 §6.4；自 announce 起算，Clock 驱动判定）。
pub const CLIP_TIMEOUT_MS: u64 = 10_000;

/// seq 半窗判定阈值（u16 回绕安全）：`seq - next_seq`（wrapping）小于此值
/// 视为"跳号"（乱序违规），否则视为"已收过的重复块"（去重忽略）。
const SEQ_HALF_WINDOW: u16 = 0x8000;

/// 接收端配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRxConfig {
    /// 协商剪贴板上限（`Options.clipboard_max_bytes`；默认 16MiB，docs/02 §6.1）。
    pub max_total_len: u32,
}

impl Default for ClipRxConfig {
    fn default() -> Self {
        Self {
            max_total_len: ClipAnnounce::DEFAULT_MAX_TOTAL_LEN,
        }
    }
}

/// 单条剪贴板消息的处理产出（调用方按序执行：回 Ack 帧 / 交付内容 / 告警 / 断开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipRxAction {
    /// 信用归还（每个被接受的 chunk 一条，docs/02 §6.3；重复块不重复归还）。
    SendAck {
        /// 被接受块的 seq。
        seq: u16,
    },
    /// `ClipEnd` 且 blake3 校验通过：交付完整内容（docs/02 §6.5）。
    Deliver {
        /// 传输标识。
        clip_id: u32,
        /// 重组完成的完整内容（与 announce.total_len / blake3 一致）。
        content: Vec<u8>,
    },
    /// 软失败：丢弃当前重组状态并告警（**不断连**，docs/02 §6.4/§6.5——
    /// 超时 / blake3 校验失败；剪贴板语义允许失败）。
    Drop {
        /// 告警原因（不含内容本身，docs/04 §6 泄露纪律）。
        reason: String,
    },
    /// 协议违规：调用方必须**断开剪贴板连接**（docs/02 §6.6；docs/02 §7 code 0x04）。
    Violation {
        /// 违规原因（诊断用，不含内容）。
        reason: String,
    },
}

/// 重组中的传输（单连接单活动 clip）。
struct ActiveClip {
    clip_id: u32,
    total_len: u32,
    blake3: [u8; 32],
    received: Vec<u8>,
    /// 下一期望 seq（严格连续递增，u16 回绕；docs/02 §6.2「16bit seq 窗口去重」）。
    next_seq: u16,
    /// 完成时限（announce 时刻 + [`CLIP_TIMEOUT_MS`]）。
    deadline_ms: u64,
}

/// 剪贴板接收状态机（每条剪贴板连接一个实例；时钟注入）。
///
/// 消息入口按 docs/02 §4.4 顺序：`ChannelBind` 必须为连接首帧；
/// `ClipAnnounce → ClipChunk* → ClipEnd`；任何违规入口产出
/// [`ClipRxAction::Violation`]，由调用方断开（协议违规不重试）。
pub struct ClipRx<C: Clock> {
    config: ClipRxConfig,
    clock: C,
    /// 期望的控制连接会话标识（`ChannelBind.session_id` 必须匹配，docs/02 §3.3）。
    session_id: [u8; 16],
    /// 是否已收到合法 `ChannelBind`（连接首帧语义）。
    bound: bool,
    active: Option<ActiveClip>,
}

impl<C: Clock> ClipRx<C> {
    /// 构造接收端；`session_id` 为本连接绑定的控制会话标识
    /// （来自控制连接握手派生 `BLAKE3(hs_hash)[0..16]`，接线层持有）。
    #[must_use]
    pub fn new(config: ClipRxConfig, clock: C, session_id: [u8; 16]) -> Self {
        Self {
            config,
            clock,
            session_id,
            bound: false,
            active: None,
        }
    }

    /// `ChannelBind`（docs/02 §4.4）：必须是首帧、会话匹配且绑定唯一，
    /// 否则违规断开。
    pub fn on_bind(&mut self, bind: ChannelBind) -> Vec<ClipRxAction> {
        if self.bound {
            return vec![violation("重复 ChannelBind（绑定必须唯一）")];
        }
        if bind.session_id != self.session_id {
            return vec![violation("ChannelBind 会话不匹配")];
        }
        self.bound = true;
        Vec::new()
    }

    /// `ClipAnnounce`（docs/02 §6.1）：协商上限校验 + 单活动 clip 占用校验；
    /// 通过后开启重组缓冲（10s 时限自此起算）。
    pub fn on_announce(&mut self, announce: ClipAnnounce) -> Vec<ClipRxAction> {
        if let Some(action) = self.precheck_unbound() {
            return vec![action];
        }
        if announce.total_len > self.config.max_total_len {
            return vec![violation(&format!(
                "total_len {} 超协商上限 {}",
                announce.total_len, self.config.max_total_len
            ))];
        }
        if self.active.is_some() {
            return vec![violation("上一传输未结束又收到 announce（单活动 clip）")];
        }
        // 重组缓冲容量按上限预留（total_len 即精确上界，超出即违规）
        let cap = usize::try_from(announce.total_len.min(64 * 1024)).unwrap_or(0);
        self.active = Some(ActiveClip {
            clip_id: announce.clip_id,
            total_len: announce.total_len,
            blake3: announce.blake3,
            received: Vec::with_capacity(cap),
            next_seq: 0,
            deadline_ms: self.clock.now_ms().saturating_add(CLIP_TIMEOUT_MS),
        });
        Vec::new()
    }

    /// `ClipChunk`（docs/02 §6.2）：clip_id 匹配 → seq 窗口判定 → 累计长度
    /// 检查，全部通过才入缓冲并归还信用（`SendAck`）。
    pub fn on_chunk(&mut self, chunk: ClipChunk) -> Vec<ClipRxAction> {
        if let Some(action) = self.precheck_unbound() {
            return vec![action];
        }
        let Some(active) = self.active.as_mut() else {
            return vec![violation("未 announce 先 chunk")];
        };
        if chunk.clip_id != active.clip_id {
            return vec![violation("chunk.clip_id 与活动传输不匹配")];
        }
        // seq 判定（u16 回绕安全，docs/02 §6.2「seq 窗口去重（乱序=违规断开）」）：
        // ahead = seq - next_seq（wrapping）：
        //   ahead == 0            期望块 → 接受；
        //   0 < ahead < 半窗      跳号（乱序/回绕异常）→ 违规断开；
        //   ahead ≥ 半窗          已收过的重复块 → 去重忽略（不重复归还信用）。
        let ahead = chunk.seq.wrapping_sub(active.next_seq);
        if ahead >= SEQ_HALF_WINDOW {
            return Vec::new();
        }
        if ahead > 0 {
            return vec![violation(&format!(
                "seq 乱序：期望 {} 收到 {}（跳号）",
                active.next_seq, chunk.seq
            ))];
        }
        // 每帧累计长度检查（docs/02 §6.2：累计 ≤ total_len，违规断开）
        let new_len = active.received.len().saturating_add(chunk.data.len());
        if new_len > active.total_len as usize {
            return vec![violation(&format!(
                "累计长度 {} 超 total_len {}",
                new_len, active.total_len
            ))];
        }
        active.received.extend_from_slice(&chunk.data);
        active.next_seq = active.next_seq.wrapping_add(1);
        vec![ClipRxAction::SendAck { seq: chunk.seq }]
    }

    /// `ClipEnd`（docs/02 §6.5）：完整性 + blake3 校验；失败/不完整 →
    /// 软失败丢弃（告警不重传不断连），校验通过 → 交付完整内容。
    pub fn on_end(&mut self, end: ClipEnd) -> Vec<ClipRxAction> {
        if let Some(action) = self.precheck_unbound() {
            return vec![action];
        }
        let Some(active) = self.active.as_ref() else {
            return vec![violation("未 announce 先 end")];
        };
        if end.clip_id != active.clip_id {
            return vec![violation("end.clip_id 与活动传输不匹配")];
        }
        // 借用检查完毕，取走所有权收尾
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        if active.received.len() != active.total_len as usize {
            return vec![drop_action(format!(
                "clip {} 传输不完整（{}/{} 字节即收到 end），丢弃",
                active.clip_id,
                active.received.len(),
                active.total_len
            ))];
        }
        let mut computed = [0u8; 32];
        computed.copy_from_slice(blake3::hash(&active.received).as_bytes());
        if computed != active.blake3 {
            return vec![drop_action(format!(
                "clip {} blake3 校验失败，丢弃（不重传，docs/02 §6.5）",
                active.clip_id
            ))];
        }
        vec![ClipRxAction::Deliver {
            clip_id: active.clip_id,
            content: active.received,
        }]
    }

    /// 定时器滴答（Clock 驱动，docs/02 §6.4）：活动传输超 10s 未完成 →
    /// 丢弃重组状态并告警（软失败，连接保持；防慢速占用）。
    pub fn on_tick(&mut self) -> Vec<ClipRxAction> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if self.clock.now_ms() < active.deadline_ms {
            return Vec::new();
        }
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        vec![drop_action(format!(
            "clip {} 超时（10s 未完成），丢弃 {} 字节重组缓冲",
            active.clip_id,
            active.received.len()
        ))]
    }

    /// 绑定前置校验：`ChannelBind` 未到即收到其他剪贴板消息 = 违规
    /// （docs/02 §4.4「首帧 ChannelBind」）。
    fn precheck_unbound(&self) -> Option<ClipRxAction> {
        if self.bound {
            return None;
        }
        Some(violation("未绑定先传剪贴板消息（ChannelBind 必须为首帧）"))
    }
}

fn violation(reason: &str) -> ClipRxAction {
    ClipRxAction::Violation {
        reason: reason.to_owned(),
    }
}

fn drop_action(reason: String) -> ClipRxAction {
    ClipRxAction::Drop { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    /// 确定性测试内容。
    fn make_content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// 内容对应的合法 announce（blake3 与 total_len 一致）。
    fn announce_for(content: &[u8], clip_id: u32) -> ClipAnnounce {
        let mut blake3 = [0u8; 32];
        blake3.copy_from_slice(blake3::hash(content).as_bytes());
        ClipAnnounce {
            clip_id,
            origin_id: [1; 16],
            origin_seq: 1,
            formats: Vec::new(),
            total_len: u32::try_from(content.len()).expect("测试内容远小于 u32"),
            blake3,
        }
    }

    fn chunk(clip_id: u32, seq: u16, data: &[u8]) -> ClipChunk {
        ClipChunk {
            clip_id,
            seq,
            data: data.to_vec(),
        }
    }

    /// 已完成合法 ChannelBind 的接收端。
    fn bound_rx(clock: TestClock) -> ClipRx<TestClock> {
        let mut rx = ClipRx::new(ClipRxConfig::default(), clock, [7; 16]);
        assert!(
            rx.on_bind(ChannelBind {
                session_id: [7; 16]
            })
            .is_empty()
        );
        rx
    }

    fn chunk_len(content: &[u8], per: usize) -> Vec<&[u8]> {
        content.chunks(per).collect()
    }

    /// 快路径：announce → 3 块（每块 Ack）→ end = 完整交付；随后可开新传输。
    #[test]
    fn happy_path_reassembles_and_delivers() {
        let content = make_content(10);
        let mut rx = bound_rx(TestClock::new());
        let announce = announce_for(&content, 3);
        assert!(rx.on_announce(announce.clone()).is_empty());

        for (seq, part) in chunk_len(&content, 4).into_iter().enumerate() {
            let seq = u16::try_from(seq).expect("块数 < u16");
            let actions = rx.on_chunk(chunk(3, seq, part));
            assert_eq!(actions, vec![ClipRxAction::SendAck { seq }]);
        }
        let actions = rx.on_end(ClipEnd { clip_id: 3 });
        assert_eq!(
            actions,
            vec![ClipRxAction::Deliver {
                clip_id: 3,
                content: content.clone()
            }]
        );
        // 传输收尾后可开下一个（active 已回收）
        assert!(rx.on_announce(announce).is_empty());
    }

    /// bind 语义：会话不匹配违规（之后仍可正确绑定）；重复绑定违规；
    /// 绑定前 announce/chunk/end 全部违规。
    #[test]
    fn bind_semantics() {
        let mut rx = ClipRx::new(ClipRxConfig::default(), TestClock::new(), [7; 16]);
        let actions = rx.on_bind(ChannelBind {
            session_id: [9; 16],
        });
        assert!(
            matches!(actions[0], ClipRxAction::Violation { .. }),
            "会话不匹配"
        );
        assert!(
            rx.on_bind(ChannelBind {
                session_id: [7; 16]
            })
            .is_empty()
        );
        let actions = rx.on_bind(ChannelBind {
            session_id: [7; 16],
        });
        assert!(
            matches!(actions[0], ClipRxAction::Violation { .. }),
            "重复绑定"
        );

        let mut rx = ClipRx::new(ClipRxConfig::default(), TestClock::new(), [7; 16]);
        for actions in [
            rx.on_announce(announce_for(&make_content(4), 1)),
            rx.on_chunk(chunk(1, 0, &[0; 4])),
            rx.on_end(ClipEnd { clip_id: 1 }),
        ] {
            assert!(
                matches!(actions[0], ClipRxAction::Violation { .. }),
                "未绑定先传剪贴板消息必须违规：{actions:?}"
            );
        }
    }

    /// 活动传输未结束又收到 announce → 违规（单活动 clip，docs/02 §6.3）。
    #[test]
    fn announce_while_active_violates() {
        let content = make_content(8);
        let mut rx = bound_rx(TestClock::new());
        assert!(rx.on_announce(announce_for(&content, 1)).is_empty());
        let actions = rx.on_announce(announce_for(&content, 2));
        assert!(matches!(actions[0], ClipRxAction::Violation { .. }));
    }

    /// 每帧累计长度检查（docs/02 §6.2）：累计超 total_len 违规；恰好等于合法。
    #[test]
    fn cumulative_length_over_total_violates() {
        let content = make_content(8);
        let mut rx = bound_rx(TestClock::new());
        assert!(rx.on_announce(announce_for(&content, 1)).is_empty());
        // 恰好 total_len（8B 单块）：合法
        assert_eq!(
            rx.on_chunk(chunk(1, 0, &content)),
            vec![ClipRxAction::SendAck { seq: 0 }]
        );
        // 新传输：单块超 total_len
        let mut rx = bound_rx(TestClock::new());
        assert!(rx.on_announce(announce_for(&[0u8; 4], 2)).is_empty());
        let actions = rx.on_chunk(chunk(2, 0, &[0u8; 8]));
        assert!(matches!(actions[0], ClipRxAction::Violation { .. }));
    }

    /// seq 去重（docs/02 §6.2）：重复块静默忽略、不重复归还信用；跳号违规。
    #[test]
    fn duplicate_seq_dedup_and_gap_violates() {
        let content = make_content(8);
        let mut rx = bound_rx(TestClock::new());
        assert!(rx.on_announce(announce_for(&content, 1)).is_empty());
        assert_eq!(
            rx.on_chunk(chunk(1, 0, &content[..4])),
            vec![ClipRxAction::SendAck { seq: 0 }]
        );
        // 重复 seq 0：去重忽略（无 Ack）
        assert!(rx.on_chunk(chunk(1, 0, &content[..4])).is_empty());
        // 跳号（期望 1 收到 2）：乱序违规
        let actions = rx.on_chunk(chunk(1, 2, &content[4..]));
        assert!(matches!(actions[0], ClipRxAction::Violation { .. }));
        // 违规后状态机不动（断开由调用方执行）；seq 1 仍按协议接受
        assert_eq!(
            rx.on_chunk(chunk(1, 1, &content[4..])),
            vec![ClipRxAction::SendAck { seq: 1 }]
        );
    }

    /// 未 announce 先 chunk / 先 end、clip_id 不匹配 → 违规（docs/02 §6.6）。
    #[test]
    fn out_of_order_entry_points_violate() {
        let mut rx = bound_rx(TestClock::new());
        let actions = rx.on_chunk(chunk(1, 0, &[0; 4]));
        assert!(
            matches!(actions[0], ClipRxAction::Violation { .. }),
            "未 announce 先 chunk"
        );
        let actions = rx.on_end(ClipEnd { clip_id: 1 });
        assert!(
            matches!(actions[0], ClipRxAction::Violation { .. }),
            "未 announce 先 end"
        );

        // clip_id 不匹配（chunk / end）
        let content = make_content(8);
        let mut rx = bound_rx(TestClock::new());
        assert!(rx.on_announce(announce_for(&content, 1)).is_empty());
        let actions = rx.on_chunk(chunk(2, 0, &content[..4]));
        assert!(
            matches!(actions[0], ClipRxAction::Violation { .. }),
            "chunk clip_id 不匹配"
        );
        let actions = rx.on_end(ClipEnd { clip_id: 2 });
        assert!(
            matches!(actions[0], ClipRxAction::Violation { .. }),
            "end clip_id 不匹配"
        );
    }

    /// 协商缩限（docs/02 §6.1 可配）：16MiB 内但超协商值 → 违规断开。
    #[test]
    fn negotiated_limit_violates_below_default() {
        let mut rx = ClipRx::new(
            ClipRxConfig {
                max_total_len: 1024,
            },
            TestClock::new(),
            [7; 16],
        );
        assert!(
            rx.on_bind(ChannelBind {
                session_id: [7; 16]
            })
            .is_empty()
        );
        let announce = ClipAnnounce {
            clip_id: 1,
            origin_id: [1; 16],
            origin_seq: 1,
            formats: Vec::new(),
            total_len: 2048,
            blake3: [0; 32],
        };
        let actions = rx.on_announce(announce);
        assert!(matches!(actions[0], ClipRxAction::Violation { .. }));
    }

    /// 软失败（不断连）：不完整 end / blake3 校验失败 → Drop + 状态回收，
    /// 随后可开新传输。
    #[test]
    fn soft_failures_drop_state_but_keep_connection() {
        let content = make_content(8);
        // 不完整 end
        let mut rx = bound_rx(TestClock::new());
        assert!(rx.on_announce(announce_for(&content, 1)).is_empty());
        assert_eq!(
            rx.on_chunk(chunk(1, 0, &content[..4])),
            vec![ClipRxAction::SendAck { seq: 0 }]
        );
        let actions = rx.on_end(ClipEnd { clip_id: 1 });
        assert!(
            matches!(actions[0], ClipRxAction::Drop { .. }),
            "4/8 字节即 end"
        );
        assert!(rx.active.is_none(), "状态已回收");
        assert!(
            rx.on_announce(announce_for(&content, 1)).is_empty(),
            "连接保持可重开"
        );

        // blake3 校验失败（内容与声明摘要不符）
        let mut rx = bound_rx(TestClock::new());
        let mut announce = announce_for(&content, 2);
        announce.blake3 = [0xFF; 32];
        assert!(rx.on_announce(announce).is_empty());
        assert_eq!(
            rx.on_chunk(chunk(2, 0, &content)),
            vec![ClipRxAction::SendAck { seq: 0 }]
        );
        let actions = rx.on_end(ClipEnd { clip_id: 2 });
        assert!(
            matches!(actions[0], ClipRxAction::Drop { .. }),
            "摘要不符丢弃"
        );
        assert!(rx.active.is_none());
    }

    /// 超时清理（docs/02 §6.4，Clock 驱动）：10s 未完成 → Drop + 状态回收；
    /// 清理后到达的 chunk 视为未 announce（违规）。
    #[test]
    fn timeout_drops_partial_reassembly() {
        let clock = TestClock::new();
        let content = make_content(8);
        let mut rx = bound_rx(clock.clone());
        assert!(rx.on_announce(announce_for(&content, 5)).is_empty());
        let _ = rx.on_chunk(chunk(5, 0, &content[..4]));
        assert_eq!(
            rx.active.as_ref().map(|active| active.received.len()),
            Some(4)
        );

        clock.advance(CLIP_TIMEOUT_MS - 1);
        assert!(rx.on_tick().is_empty(), "未到期不清理");

        clock.advance(1);
        let actions = rx.on_tick();
        assert!(
            matches!(actions[0], ClipRxAction::Drop { .. }),
            "10s 未完成丢弃重组状态"
        );
        assert!(rx.active.is_none(), "防慢速占用达成");

        // 清理后到达的 chunk = 未 announce → 违规
        let actions = rx.on_chunk(chunk(5, 1, &content[4..]));
        assert!(matches!(actions[0], ClipRxAction::Violation { .. }));
        // 无活动传输时 tick 空转
        assert!(rx.on_tick().is_empty());
    }
}
