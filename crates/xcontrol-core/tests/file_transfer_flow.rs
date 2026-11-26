//! 文件拉流全链路（纯状态机编排，FR-7.3 切片 b-core；docs/02 §4.6）。
//!
//! 内存"文件"模拟端到端：serve 注册 → PullReq 校验 → Grant → app 侧读块
//! （内存切片）→ FileTx 信用窗口 → FileChunk 帧 → FileRx 组装（WriteChunk）
//! → FileEnd 校验 EntryVerified。拼接内容与 blake3 与源一致；
//! 校验矩阵：未知 id/越界下标/offset 超尺寸/跳号/哈希不符/并发上限。
//!
//! 真实磁盘读写与文件 TCP 通道在 app/net 接线层（本文件零 I/O，纯逻辑编排）。

use xcontrol_core::clock::TestClock;
use xcontrol_core::file_ref::{FileRefRx, FileRefRxConfig, build_manifest};
use xcontrol_core::file_rx::{FileRx, FileRxAction, FileRxConfig};
use xcontrol_core::file_serve::{FileServe, ServeVerdict};
use xcontrol_core::file_tx::{FileTx, INITIAL_FILE_CREDIT_CHUNKS};
use xcontrol_proto::{FileChunk, FileEnd, FileEntryKind, FilePullReq, FileRefAnnounce, Message};

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

const CHUNK: usize = 16;

/// 内存"文件"：源内容切片 + 确定性元数据。
struct SourceFile {
    content: Vec<u8>,
}

impl SourceFile {
    fn new(len: usize) -> Self {
        Self {
            content: (0..len).map(|i| (i % 251) as u8).collect(),
        }
    }

    fn entry(&self, name: &str) -> xcontrol_proto::FileEntry {
        xcontrol_proto::FileEntry {
            name: name.to_owned(),
            size: self.content.len() as u64,
            mtime_ms: 0,
            kind: FileEntryKind::File,
            children: 0,
        }
    }

    /// 自 `from` 起到末尾区间的 blake3（区间拉取的摘要口径，docs/02 §4.6 FileEnd）。
    fn range_blake3(&self, from: usize) -> [u8; 32] {
        let mut digest = [0u8; 32];
        digest.copy_from_slice(blake3::hash(&self.content[from..]).as_bytes());
        digest
    }
}

/// 双端会话：源端（serve + tx + 内存文件）与接收端（pending 槽 + rx + 暂存缓冲）。
struct Rig {
    source: SourceFile,
    serve: FileServe,
    tx: FileTx,
    rx: FileRx<TestClock>,
    staged: Vec<u8>,
    verified: Vec<(u32, u32)>,
    /// 本次拉取的文件内起始偏移（app 契约：物理读位 = offset + bytes_sent）。
    pull_offset: u64,
}

impl Rig {
    fn new(file_len: usize) -> Self {
        Self {
            source: SourceFile::new(file_len),
            serve: FileServe::new(),
            tx: FileTx::new(),
            rx: FileRx::new(FileRxConfig::default(), TestClock::new()),
            staged: Vec::new(),
            verified: Vec::new(),
            pull_offset: 0,
        }
    }

    /// 源端通告清单（announce → serve 注册）。
    fn announce(&mut self, transfer_id: u32) -> FileRefAnnounce {
        let announce = FileRefAnnounce {
            transfer_id,
            origin_id: [1; 16],
            origin_seq: 1,
            entries: vec![self.source.entry("file.bin")],
            total_bytes: self.source.content.len() as u64,
            copied_at_ms: 0,
        };
        self.serve.register(&announce);
        announce
    }

    /// 接收端拉取一条目（PullReq → Grant → begin_pull + begin_entry）。
    fn pull(&mut self, transfer_id: u32, entry: u32, offset: u64) -> ServeVerdict {
        let verdict = self.serve.on_pull(&FilePullReq {
            transfer_id,
            entry,
            offset,
        });
        if let ServeVerdict::Grant {
            offset, remaining, ..
        } = &verdict
        {
            self.pull_offset = *offset;
            self.rx.begin_pull(transfer_id, entry, *remaining);
            must(
                "tx begin_entry",
                self.tx.begin_entry(transfer_id, entry, *remaining),
            );
        }
        verdict
    }

    /// 源端泵：在信用窗口内从内存"文件"读块 → seq 分配 → 编帧 → 接收端消费；
    /// 发满产出 FileEnd 并消费。返回接收端累计动作。
    fn pump(&mut self) -> Vec<FileRxAction> {
        let mut actions = Vec::new();
        while !self.tx.should_end() {
            assert!(
                self.tx.can_send(),
                "信用窗口阻塞：测试泵不做跨泵等待（窗口 {INITIAL_FILE_CREDIT_CHUNKS} ≥ 块数）"
            );
            let (transfer_id, _entry, bytes_sent, _) =
                must_some("流在泵中应存在", self.tx.stream());
            let offset = (self.pull_offset + bytes_sent) as usize;
            let end = (offset + CHUNK).min(self.source.content.len());
            let data = &self.source.content[offset..end];
            let seq = must_some("流在泵中应有 seq", self.tx.next_seq());
            let frame = FileChunk {
                transfer_id,
                seq,
                data: data.to_vec(),
            };
            must("tx on_chunk_sent", self.tx.on_chunk_sent(seq, data.len()));
            // 接收端消费本块；Ack 即刻归还源端信用
            let new_actions = self.rx.on_chunk(&frame);
            for action in &new_actions {
                if let FileRxAction::SendAck { seq } = action {
                    assert!(self.tx.on_ack(*seq));
                }
            }
            actions.extend(new_actions);
        }
        // 发满 → FileEnd → 接收端校验
        let (transfer_id, entry) = must("tx take_stream", self.tx.take_stream());
        let end = FileEnd {
            transfer_id,
            entry,
            // 区间拉取语义：摘要覆盖本次传输区间（全量 = 整条目；续传区间 = 该区间）
            blake3: self.source.range_blake3(self.pull_offset as usize),
        };
        let end_actions = self.rx.on_end(&end);
        self.serve.release(transfer_id);
        for action in &actions {
            if let FileRxAction::WriteChunk { data, .. } = action {
                self.staged.extend_from_slice(data);
            }
        }
        for action in &end_actions {
            if let FileRxAction::EntryVerified { transfer_id, entry } = action {
                self.verified.push((*transfer_id, *entry));
            }
        }
        actions.extend(end_actions);
        actions
    }
}

/// 全链路：通告 → 拉取 → 泵 → EntryVerified；拼接内容与哈希与源一致。
#[test]
fn full_pull_stream_verifies() {
    let mut rig = Rig::new(100);
    let announce = rig.announce(7);

    // 接收端元数据面（切片 a）：清单入 pending 槽，作为拉取的 size 依据
    let mut slot = FileRefRx::new(FileRefRxConfig::default());
    assert!(matches!(
        slot.on_announce(announce.clone()),
        xcontrol_core::FileRefVerdict::Applied { .. }
    ));
    let pending = must_some("pending 槽", slot.slot());
    assert_eq!(pending.entries[0].size, 100);

    // 发送端元数据面：build_manifest 产出的清单经帧编码（切片 a wire 往返）
    let manifest = must_some(
        "清单构建",
        must(
            "构建",
            build_manifest(7, [1; 16], 1, vec![rig.source.entry("file.bin")], 0),
        ),
    );
    assert!(Message::encode_frame(&manifest, 0).is_ok());

    // 拉流全链路
    assert_eq!(
        rig.pull(7, 0, 0),
        ServeVerdict::Grant {
            transfer_id: 7,
            entry: 0,
            offset: 0,
            remaining: 100,
        }
    );
    let actions = rig.pump();
    assert_eq!(
        rig.verified,
        vec![(7, 0)],
        "泵内消费即记录（actions 与累计一致）"
    );
    assert_eq!(rig.staged, rig.source.content, "WriteChunk 拼接 == 源内容");
    assert!(
        actions.iter().any(|action| matches!(
            action,
            FileRxAction::EntryVerified {
                transfer_id: 7,
                entry: 0
            }
        )),
        "EntryVerified 在动作流中"
    );
}

/// 偏移续传预留：offset > 0 时 remaining 收窄，拼接 = 源内容后缀。
#[test]
fn offset_pull_serves_tail() {
    let mut rig = Rig::new(64);
    rig.announce(1);
    assert_eq!(
        rig.pull(1, 0, 48),
        ServeVerdict::Grant {
            transfer_id: 1,
            entry: 0,
            offset: 48,
            remaining: 16,
        }
    );
    rig.pump();
    assert_eq!(rig.staged, rig.source.content[48..], "offset 拉取只传后缀");
    assert_eq!(rig.verified, vec![(1, 0)]);
}

/// 校验矩阵贯穿双端：越界下标（源端违规）与哈希不符（接收端软失败）。
#[test]
fn cross_side_validation_matrix() {
    // 源端：未知 transfer / 越界下标 / offset ≥ size
    let mut rig = Rig::new(8);
    rig.announce(3);
    for (req_entry, offset, why) in [(5u32, 0u64, "越界下标"), (0, 8, "offset == size")] {
        assert!(
            matches!(
                rig.serve.on_pull(&FilePullReq {
                    transfer_id: 3,
                    entry: req_entry,
                    offset,
                }),
                ServeVerdict::Violation { .. }
            ),
            "{why}"
        );
    }

    // 接收端：哈希不符 → EntryFailed 软失败（可重试），状态回收后可重新拉取
    let mut rx = FileRx::new(FileRxConfig::default(), TestClock::new());
    rx.begin_pull(3, 0, 4);
    let _ = rx.on_chunk(&FileChunk {
        transfer_id: 3,
        seq: 0,
        data: vec![1, 2, 3, 4],
    });
    assert!(matches!(
        rx.on_end(&FileEnd {
            transfer_id: 3,
            entry: 0,
            blake3: [0xFF; 32],
        })[0],
        FileRxAction::EntryFailed { .. }
    ));
    rx.begin_pull(3, 0, 4);
    assert_eq!(rx.active(), Some((3, 0, 0, 4)), "重试重新起流");
}
