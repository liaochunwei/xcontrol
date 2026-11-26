//! 文件域接收端条目组装状态机（docs/02 §4.6 防护规则 3/4/5/6/8；纯逻辑、零 I/O）。
//!
//! 拉取方向：接收端发 `FilePullReq` 后在本模块登记期望（`begin_pull`，size 来自
//! pending 槽清单），随后按序消费源端 `FileChunk`/`FileEnd`/`FileError`/`FileCancel`。
//! 信任边界在接收端（docs/02 §6.7 原则沿用）：
//!
//! | 校验 | 处置 | 依据 |
//! | --- | --- | --- |
//! | seq 严格连续（跳号=乱序违规断开；重复=去重忽略） | 跳号 `Violation` | docs/02 §4.6 规则 4 |
//! | 每帧累计 ≤ 条目 size | `Violation` | docs/02 §4.6 规则 4 |
//! | 未 begin_pull 先 chunk / end | `Violation` | docs/02 §4.6（未请求先到） |
//! | `FileEnd`：received == size 且 blake3 一致 | `EntryVerified` / `EntryFailed` | docs/02 §4.6 规则 8 |
//! | 10s 无 chunk（分块空闲超时）/ 30min 总时长 | `EntryFailed`（软，可重试） | docs/02 §4.6 规则 5 |
//! | `FileError`/`FileCancel` | `EntryFailed` / teardown（不断连） | docs/02 §4.6 规则 6 |
//!
//! 与剪贴板 [`crate::clip_rx::ClipRx`] 的语义差异：文件条目失败**可重试**
//! （用户在粘贴处等待），只回收条目状态不断连；blake3 增量计算（每条目一个
//! Hasher），磁盘写入由调用方按 [`FileRxAction::WriteChunk`] 执行（core 零 I/O）。
//!
//! 超时判定 Clock 驱动：接线层周期调用 [`FileRx::on_tick`]；消息入口不判超时
//! ——慢速但守约的传输只在 tick 点被清理。

use xcontrol_proto::{FileCancel, FileChunk, FileEnd, FileError};

use crate::clock::Clock;

/// 分块空闲超时：10s 无 chunk 判死该条目（docs/02 §4.6 防护规则 5）。
pub const FILE_IDLE_TIMEOUT_MS: u64 = 10_000;
/// 单 transfer 总时长上限：30min（docs/02 §4.6 防护规则 5；Clock 驱动）。
pub const FILE_TOTAL_DURATION_CAP_MS: u64 = 30 * 60 * 1000;

/// 接收端配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRxConfig {
    /// 分块空闲超时（毫秒；默认 [`FILE_IDLE_TIMEOUT_MS`]）。
    pub idle_timeout_ms: u64,
    /// 单 transfer 总时长上限（毫秒；默认 [`FILE_TOTAL_DURATION_CAP_MS`]）。
    pub total_duration_cap_ms: u64,
}

impl Default for FileRxConfig {
    fn default() -> Self {
        Self {
            idle_timeout_ms: FILE_IDLE_TIMEOUT_MS,
            total_duration_cap_ms: FILE_TOTAL_DURATION_CAP_MS,
        }
    }
}

/// 单条目消息的处理产出（调用方按序执行：写盘 / 回 Ack / 上抛结果 / 断开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRxAction {
    /// 信用归还（每个被接受的 chunk 一条；重复块不重复归还）。
    SendAck {
        /// 被接受块的 seq。
        seq: u32,
    },
    /// 顺序写盘指令（seq 校验通过后按序产出；调用方写入暂存文件对应偏移）。
    WriteChunk {
        /// 所属传输。
        transfer_id: u32,
        /// 条目下标。
        entry: u32,
        /// 块数据（按接收顺序，拼接即条目内容）。
        data: Vec<u8>,
    },
    /// 条目完整且 blake3 校验通过：调用方把暂存文件转入目标位（挂板就绪）。
    EntryVerified {
        /// 所属传输。
        transfer_id: u32,
        /// 条目下标。
        entry: u32,
    },
    /// 条目失败（**可重试**——软失败，不断连；用户可重新发起拉取）。
    EntryFailed {
        /// 所属传输。
        transfer_id: u32,
        /// 失败条目（`None` = 整单失败，如总时长超限）。
        entry: Option<u32>,
        /// 失败原因（诊断用，不含内容）。
        reason: String,
    },
    /// 协议违规：调用方必须**断开文件连接**（docs/02 §7 code 0x04）。
    Violation {
        /// 违规原因（诊断用）。
        reason: String,
    },
}

/// 组装中的条目（单连接同时一条目流——拉取方串行消费）。
struct ActiveEntry {
    transfer_id: u32,
    entry: u32,
    size: u64,
    received: u64,
    hasher: blake3::Hasher,
    /// 下一期望 seq（严格连续递增；u32 单调不回绕）。
    next_seq: u32,
    /// 最近一次 chunk 到达时刻（空闲超时基准）。
    last_chunk_ms: u64,
}

/// 单 transfer 的总时长锚点（首次拉取该 transfer 时起算）。
struct TransferClock {
    started_ms: u64,
}

/// 文件接收状态机（每条文件连接一个实例；时钟注入）。
pub struct FileRx<C: Clock> {
    config: FileRxConfig,
    clock: C,
    active: Option<ActiveEntry>,
    /// transfer 总时长锚点（单连接同时活动 transfer ≤ 4 由源端约束；本端
    /// 串行拉取，单锚点足够）。
    transfer_clock: Option<TransferClock>,
    /// 当前活动 transfer_id（`begin_pull` 时登记）。
    transfer_id: Option<u32>,
}

impl<C: Clock> FileRx<C> {
    /// 构造接收端。
    #[must_use]
    pub fn new(config: FileRxConfig, clock: C) -> Self {
        Self {
            config,
            clock,
            active: None,
            transfer_clock: None,
            transfer_id: None,
        }
    }

    /// 登记拉取期望（发送 `FilePullReq` 的同时调用）：`size` 来自 pending 槽
    /// 清单条目。进行中的上一条目被静默顶掉（重试语义——新拉取顶掉旧流，
    /// 软失败语义、不断连）。
    pub fn begin_pull(&mut self, transfer_id: u32, entry: u32, size: u64) {
        let _ = self.active.take();
        if self.transfer_id != Some(transfer_id) {
            self.transfer_id = Some(transfer_id);
            self.transfer_clock = Some(TransferClock {
                started_ms: self.clock.now_ms(),
            });
        }
        self.active = Some(ActiveEntry {
            transfer_id,
            entry,
            size,
            received: 0,
            hasher: blake3::Hasher::new(),
            next_seq: 0,
            last_chunk_ms: self.clock.now_ms(),
        });
    }

    /// `FileChunk`：transfer 匹配（条目上下文由连接隐含——`FileChunk` 不带
    /// entry 字段，单连接串行一条目流）→ seq 判定 → 累计检查，全部通过才
    /// 产出 `WriteChunk` 并归还信用（`SendAck`）。
    pub fn on_chunk(&mut self, chunk: &FileChunk) -> Vec<FileRxAction> {
        let Some(active) = &mut self.active else {
            return vec![violation("未 begin_pull 先收到 chunk")];
        };
        if chunk.transfer_id != active.transfer_id {
            return vec![violation("chunk 的 transfer_id 与活动拉取不匹配")];
        }
        // seq 判定：期望=接受；重复=去重忽略；跳号=乱序违规（docs/02 §4.6 规则 4）
        let ahead = chunk.seq.wrapping_sub(active.next_seq);
        if ahead >= SEQ_HALF_WINDOW {
            return Vec::new();
        }
        if ahead > 0 {
            return vec![violation(format!(
                "seq 乱序：期望 {} 收到 {}（跳号）",
                active.next_seq, chunk.seq
            ))];
        }
        let new_len = active.received.saturating_add(chunk.data.len() as u64);
        if new_len > active.size {
            return vec![violation(format!(
                "累计长度 {new_len} 超条目 size {}",
                active.size
            ))];
        }
        active.hasher.update(&chunk.data);
        active.received = new_len;
        active.next_seq = active.next_seq.wrapping_add(1);
        active.last_chunk_ms = self.clock.now_ms();
        let entry = active.entry;
        let transfer_id = active.transfer_id;
        vec![
            FileRxAction::SendAck { seq: chunk.seq },
            FileRxAction::WriteChunk {
                transfer_id,
                entry,
                data: chunk.data.clone(),
            },
        ]
    }

    /// `FileEnd`：完整性 + blake3 校验；通过 → `EntryVerified`；
    /// 失败 → `EntryFailed`（可重试，不断连，docs/02 §4.6 规则 8）。
    pub fn on_end(&mut self, end: &FileEnd) -> Vec<FileRxAction> {
        let Some(active) = &self.active else {
            return vec![violation("未 begin_pull 先收到 end")];
        };
        if end.transfer_id != active.transfer_id || end.entry != active.entry {
            return vec![violation("end 的 transfer/entry 与活动拉取不匹配")];
        }
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        if active.received != active.size {
            return vec![entry_failed(
                active.transfer_id,
                Some(active.entry),
                format!(
                    "传输不完整（{}/{} 字节即收到 end），可重试",
                    active.received, active.size
                ),
            )];
        }
        let mut computed = [0u8; 32];
        computed.copy_from_slice(active.hasher.finalize().as_bytes());
        if computed != end.blake3 {
            return vec![entry_failed(
                active.transfer_id,
                Some(active.entry),
                "blake3 校验失败，条目作废（可重试）".to_owned(),
            )];
        }
        vec![FileRxAction::EntryVerified {
            transfer_id: active.transfer_id,
            entry: active.entry,
        }]
    }

    /// `FileError`（源端条目级失败：文件消失/权限/尺寸不符等）→ 软失败可重试。
    /// 与活动拉取无关的 `transfer_id` 忽略（不断连）。
    pub fn on_error(&mut self, error: &FileError) -> Vec<FileRxAction> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        if error.transfer_id != active.transfer_id {
            self.active = Some(active);
            return Vec::new();
        }
        vec![entry_failed(
            active.transfer_id,
            Some(active.entry),
            format!("源端错误（code {}）", error.code.0),
        )]
    }

    /// `FileCancel`（任一侧取消）→ 软失败回收（不断连）。
    pub fn on_cancel(&mut self, cancel: &FileCancel) -> Vec<FileRxAction> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        vec![entry_failed(
            active.transfer_id,
            Some(active.entry),
            format!("传输被取消（transfer {}）", cancel.transfer_id),
        )]
    }

    /// 定时器滴答（Clock 驱动）：分块空闲超时 → 条目软失败；
    /// transfer 总时长超限 → 整单失败（entry = None）。
    pub fn on_tick(&mut self) -> Vec<FileRxAction> {
        let now = self.clock.now_ms();
        if let Some(clock) = &self.transfer_clock
            && now.saturating_sub(clock.started_ms) >= self.config.total_duration_cap_ms
        {
            let transfer_id = self.transfer_id.take();
            self.active = None;
            self.transfer_clock = None;
            if let Some(id) = transfer_id {
                return vec![entry_failed(
                    id,
                    None,
                    "transfer 总时长超上限（30min）".to_owned(),
                )];
            }
        }
        let Some(active) = &self.active else {
            return Vec::new();
        };
        if now.saturating_sub(active.last_chunk_ms) < self.config.idle_timeout_ms {
            return Vec::new();
        }
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        vec![entry_failed(
            active.transfer_id,
            Some(active.entry),
            format!(
                "分块空闲超时（{}s 无 chunk），已收 {}/{} 字节，可重试",
                self.config.idle_timeout_ms / 1000,
                active.received,
                active.size
            ),
        )]
    }

    /// 当前活动条目（诊断/测试）。
    #[must_use]
    pub fn active(&self) -> Option<(u32, u32, u64, u64)> {
        self.active.as_ref().map(|active| {
            (
                active.transfer_id,
                active.entry,
                active.received,
                active.size,
            )
        })
    }
}

/// seq 半窗判定阈值（u32 回绕安全，语义同 [`crate::clip_rx`] 的 u16 版）。
const SEQ_HALF_WINDOW: u32 = 0x8000_0000;

fn violation(reason: impl Into<String>) -> FileRxAction {
    FileRxAction::Violation {
        reason: reason.into(),
    }
}

fn entry_failed(transfer_id: u32, entry: Option<u32>, reason: String) -> FileRxAction {
    FileRxAction::EntryFailed {
        transfer_id,
        entry,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::FileErrCode;

    use super::*;
    use crate::clock::TestClock;

    fn chunk(transfer_id: u32, seq: u32, data: &[u8]) -> FileChunk {
        FileChunk {
            transfer_id,
            seq,
            data: data.to_vec(),
        }
    }

    /// 快路径：begin_pull → 3 块（各 Ack+WriteChunk）→ end = EntryVerified；
    /// 拼接内容与哈希一致。
    #[test]
    fn happy_path_assembles_and_verifies() {
        let content: Vec<u8> = (0..30u8).collect();
        let mut rx = FileRx::new(FileRxConfig::default(), TestClock::new());
        rx.begin_pull(7, 2, content.len() as u64);

        for (seq, part) in content.chunks(10).enumerate() {
            let actions = rx.on_chunk(&chunk(7, seq as u32, part));
            assert_eq!(
                actions,
                vec![
                    FileRxAction::SendAck { seq: seq as u32 },
                    FileRxAction::WriteChunk {
                        transfer_id: 7,
                        entry: 2,
                        data: part.to_vec(),
                    },
                ]
            );
        }
        let mut blake3 = [0u8; 32];
        blake3.copy_from_slice(blake3::hash(&content).as_bytes());
        assert_eq!(
            rx.on_end(&FileEnd {
                transfer_id: 7,
                entry: 2,
                blake3,
            }),
            vec![FileRxAction::EntryVerified {
                transfer_id: 7,
                entry: 2,
            }]
        );
        assert!(rx.active().is_none(), "条目状态已回收");
    }

    /// 校验矩阵：未先 pull、transfer 不匹配、跳号违规、重复去重、
    /// 累计超 size。
    #[test]
    fn validation_matrix() {
        let mut rx = FileRx::new(FileRxConfig::default(), TestClock::new());
        assert!(
            matches!(
                rx.on_chunk(&chunk(1, 0, &[0]))[0],
                FileRxAction::Violation { .. }
            ),
            "未 begin_pull 先 chunk"
        );
        rx.begin_pull(1, 0, 10);
        assert!(
            matches!(
                rx.on_chunk(&chunk(2, 0, &[0]))[0],
                FileRxAction::Violation { .. }
            ),
            "transfer 不匹配"
        );
        // 首块正常接受（Ack + WriteChunk）
        assert_eq!(
            rx.on_chunk(&chunk(1, 0, &[1, 2])),
            vec![
                FileRxAction::SendAck { seq: 0 },
                FileRxAction::WriteChunk {
                    transfer_id: 1,
                    entry: 0,
                    data: vec![1, 2],
                },
            ]
        );
        // 重复 seq 0：去重忽略（无动作、不重复归还信用）
        assert_eq!(
            rx.on_chunk(&chunk(1, 0, &[1, 2])),
            Vec::<FileRxAction>::new(),
            "重复块去重忽略"
        );
        // 跳号（期望 1 收到 2）：违规
        assert!(
            matches!(
                rx.on_chunk(&chunk(1, 2, &[3]))[0],
                FileRxAction::Violation { .. }
            ),
            "跳号乱序违规"
        );
        // 违规后状态机不动：期望 seq 1 照常接受（断开由调用方执行）
        assert!(
            matches!(
                rx.on_chunk(&chunk(1, 1, &[3, 4]))[1],
                FileRxAction::WriteChunk { .. }
            ),
            "违规后合法块照常接受"
        );
        // 累计超 size（已收 4B，size 10B，来 7B → 11B 超限）
        assert!(
            matches!(
                rx.on_chunk(&chunk(1, 2, &[0u8; 7]))[0],
                FileRxAction::Violation { .. }
            ),
            "累计超条目 size 违规"
        );
        // end 不完整 → 软失败可重试
        assert!(matches!(
            rx.on_end(&FileEnd {
                transfer_id: 1,
                entry: 0,
                blake3: [0; 32],
            })[0],
            FileRxAction::EntryFailed { .. }
        ));
        assert!(rx.active().is_none(), "软失败回收条目状态");
    }

    /// blake3 不符 → EntryFailed（可重试，非违规）。
    #[test]
    fn hash_mismatch_fails_entry_softly() {
        let mut rx = FileRx::new(FileRxConfig::default(), TestClock::new());
        rx.begin_pull(1, 0, 4);
        let _ = rx.on_chunk(&chunk(1, 0, &[1, 2, 3, 4]));
        let actions = rx.on_end(&FileEnd {
            transfer_id: 1,
            entry: 0,
            blake3: [0xFF; 32],
        });
        assert!(
            matches!(actions[0], FileRxAction::EntryFailed { .. }),
            "摘要不符按条目失败（可重试）"
        );
    }

    /// FileError / FileCancel：软失败回收（可重试，不断连）。
    #[test]
    fn source_error_and_cancel_are_soft() {
        let mut rx = FileRx::new(FileRxConfig::default(), TestClock::new());
        rx.begin_pull(1, 0, 10);
        assert!(matches!(
            rx.on_error(&FileError {
                transfer_id: 1,
                entry: Some(0),
                code: FileErrCode::NOT_FOUND,
            })[0],
            FileRxAction::EntryFailed { .. }
        ));
        assert!(rx.active().is_none());

        rx.begin_pull(1, 1, 10);
        assert!(matches!(
            rx.on_cancel(&FileCancel { transfer_id: 1 })[0],
            FileRxAction::EntryFailed { .. }
        ));
        assert!(rx.active().is_none());
    }

    /// 双超时（TestClock 驱动）：10s 无 chunk 条目软失败；30min 总时长整单失败。
    #[test]
    fn idle_timeout_and_total_duration_cap() {
        let clock = TestClock::new();
        let mut rx = FileRx::new(FileRxConfig::default(), clock.clone());
        rx.begin_pull(1, 0, 10);
        let _ = rx.on_chunk(&chunk(1, 0, &[0; 4]));

        clock.advance(FILE_IDLE_TIMEOUT_MS - 1);
        assert!(rx.on_tick().is_empty(), "未到期不清理");
        clock.advance(1);
        assert!(
            matches!(
                rx.on_tick()[0],
                FileRxAction::EntryFailed { entry: Some(0), .. }
            ),
            "10s 无 chunk 条目软失败"
        );
        assert!(rx.active().is_none());

        // 总时长上限：挂一个条目不喂 chunk，快进 30min
        let mut rx = FileRx::new(FileRxConfig::default(), clock.clone());
        rx.begin_pull(2, 0, 10);
        clock.advance(FILE_TOTAL_DURATION_CAP_MS);
        assert!(
            matches!(
                rx.on_tick()[0],
                FileRxAction::EntryFailed { entry: None, .. }
            ),
            "总时长超限整单失败（entry = None）"
        );
        assert!(rx.active().is_none());
        assert!(rx.on_tick().is_empty(), "清理后 tick 空转");
    }

    /// 顶掉旧流：begin_pull 重入按软失败语义回收（重试语义，不断连）。
    #[test]
    fn begin_pull_supersedes_previous_stream() {
        let mut rx = FileRx::new(FileRxConfig::default(), TestClock::new());
        rx.begin_pull(1, 0, 10);
        let _ = rx.on_chunk(&chunk(1, 0, &[0; 4]));
        rx.begin_pull(1, 0, 10); // 重试同一条目：旧流直接被顶掉
        assert_eq!(
            rx.active(),
            Some((1, 0, 0, 10)),
            "新流从零开始（received 重置）"
        );
        // 新流 seq 从 0 重新计数
        assert!(matches!(
            rx.on_chunk(&chunk(1, 0, &[1, 2]))[1],
            FileRxAction::WriteChunk { .. }
        ));
    }
}
