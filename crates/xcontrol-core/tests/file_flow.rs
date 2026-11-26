#![cfg(feature = "mock-platform")]
//! mock 虚拟剪贴板 × 文件引用清单全流程（FR-7.3 切片 a；docs/04 §8，
//! `cargo test -p xcontrol-core --features mock-platform`）。
//!
//! 覆盖（prd AC2/AC3）：
//! 1. mock 端到端：文件引用写入 → 变更触发 → 清单构建 → **帧级 wire 往返**
//!    （编码 → 解帧 → 分派解码）→ 接收端入槽（Applied + 指纹一致）；
//! 2. 代次失效矩阵：同源重放去重、跨来源覆盖（最后复制者赢）、invalidate 清槽；
//! 3. 指纹规范形跨 wire 稳定（`(name, size)` 不含 mtime——docs/04 §8.3）；
//! 4. 本机新剪贴板同步 → invalidate（调用方语义锚点，docs/04 §8.2）。
//!
//! 本文件直连纯逻辑状态机（无传输）；文件 TCP 通道全链路随切片 b 落地。

use xcontrol_core::file_ref::{
    FileRefRx, FileRefRxConfig, FileRefVerdict, build_manifest, manifest_fingerprint,
};
use xcontrol_core::mock::MockClipboard;
use xcontrol_proto::{
    FileEntry, FileEntryKind, FileMsg, FileRefAnnounce, Message, decode_file, decode_frame,
};

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

fn entry(name: &str, size: u64) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        size,
        mtime_ms: 1_700_000_000_000,
        kind: FileEntryKind::File,
        children: 0,
    }
}

/// 帧级 wire 往返：announce → 编码 → 解帧 → 分派解码（消息层校验全过）。
fn roundtrip(announce: &FileRefAnnounce) -> FileRefAnnounce {
    let bytes = must("announce 帧编码", Message::encode_frame(announce, 1));
    let frame = must("帧解码", decode_frame(&bytes));
    assert_eq!(
        frame.header.kind,
        xcontrol_proto::frame::kind::FILE_REF_ANNOUNCE
    );
    let msg = must_some(
        "文件域分派",
        must("消息解码", decode_file(frame.header.kind, frame.payload)),
    );
    match msg {
        FileMsg::RefAnnounce(announce) => announce,
        other => panic!("期望 RefAnnounce，得到 {other:?}"),
    }
}

/// mock 端到端：set_file_refs → 变更触发 → 清单构建 → wire 往返 → 入槽。
#[test]
fn mock_end_to_end_ref_announce_applies() {
    let mut clipboard = MockClipboard::new();
    clipboard.set_file_refs(vec![entry("报告.pdf", 2048), entry("photo.png", 1_048_576)]);
    assert!(
        clipboard.has_changes_since(0),
        "文件引用写入触发变更（watch 语义同真实平台监听）"
    );

    // 变化方（发送端）构建清单：total_bytes 由条目聚合
    let announce = must_some(
        "非空引用应产出清单",
        must(
            "清单构建",
            build_manifest(7, [0xAB; 16], 1, clipboard.file_refs().to_vec(), 42),
        ),
    );
    assert_eq!(announce.total_bytes, 2048 + 1_048_576);

    // 帧级 wire 往返 → 接收端入槽；指纹与发送端本地计算一致
    let fingerprint = manifest_fingerprint(&announce.entries);
    let decoded = roundtrip(&announce);
    let mut rx = FileRefRx::new(FileRefRxConfig::default());
    assert_eq!(
        rx.on_announce(decoded),
        FileRefVerdict::Applied {
            entry_count: 2,
            total_bytes: 2048 + 1_048_576,
            fingerprint,
        }
    );
    let slot = must_some("槽应有值", rx.slot());
    assert_eq!(slot.transfer_id, 7);
    assert_eq!(slot.entries.len(), 2);
}

/// 代次失效矩阵 + 指纹跨 wire 稳定。
#[test]
fn generation_invalidation_matrix_and_fingerprint_stability() {
    let mut clipboard = MockClipboard::new();
    clipboard.set_file_refs(vec![entry("a.txt", 1), entry("b.txt", 2)]);
    let announce = must_some(
        "清单",
        must(
            "构建",
            build_manifest(9, [1; 16], 1, clipboard.file_refs().to_vec(), 0),
        ),
    );
    let fingerprint = manifest_fingerprint(&announce.entries);

    // wire 往返后指纹不变（规范形不依赖 wire 外字段）
    let decoded = roundtrip(&announce);
    assert_eq!(manifest_fingerprint(&decoded.entries), fingerprint);

    // mtime（wire 内但不参与指纹）变化不改指纹——物化落盘的等价场景
    let mut touched = decoded.clone();
    for item in &mut touched.entries {
        item.mtime_ms += 999;
    }
    assert_eq!(manifest_fingerprint(&touched.entries), fingerprint);

    // 接收端矩阵：应用 → 同源重放去重 → 跨源覆盖 → invalidate 清槽 → 重新入槽
    let mut rx = FileRefRx::new(FileRefRxConfig::default());
    assert_eq!(
        rx.on_announce(decoded.clone()),
        FileRefVerdict::Applied {
            entry_count: 2,
            total_bytes: 3,
            fingerprint,
        }
    );
    assert_eq!(rx.on_announce(decoded.clone()), FileRefVerdict::Duplicate);

    let mut from_other = decoded.clone();
    from_other.origin_id = [2; 16];
    from_other.entries = vec![entry("c.txt", 5)];
    from_other.total_bytes = 5;
    assert!(
        matches!(rx.on_announce(from_other), FileRefVerdict::Applied { .. }),
        "跨来源覆盖（最后复制者赢）"
    );

    let evicted = must_some("失效应返回旧槽", rx.invalidate());
    assert_eq!(evicted.entries[0].name, "c.txt");
    assert!(rx.slot().is_none(), "槽已清空");
    assert!(rx.invalidate().is_none(), "空槽失效为 None");
    assert!(
        matches!(rx.on_announce(decoded), FileRefVerdict::Applied { .. }),
        "失效后新到达的同源清单重新入槽（新复制语义）"
    );
}

/// 本机新剪贴板文本同步 → invalidate（调用方语义锚点，docs/04 §8.2）。
#[test]
fn local_clipboard_write_invalidates_pending_slot() {
    let mut rx = FileRefRx::new(FileRefRxConfig::default());
    let mut clipboard = MockClipboard::new();
    clipboard.set_file_refs(vec![entry("pending.bin", 10)]);
    let announce = must_some(
        "清单",
        must(
            "构建",
            build_manifest(3, [1; 16], 1, clipboard.file_refs().to_vec(), 0),
        ),
    );
    assert!(matches!(
        rx.on_announce(announce),
        FileRefVerdict::Applied { .. }
    ));

    // 用户在本机复制了文本 → 文件槽失效（"最后复制者赢"）
    clipboard.set_text("新的文本复制");
    assert!(
        clipboard.has_changes_since(1),
        "文本写入同样触发剪贴板变化（调用方据此 invalidate）"
    );
    assert!(
        clipboard.file_refs().is_empty(),
        "文本写入清空文件引用（同板单写者语义，mock 与真实平台行为一致）"
    );
    assert!(rx.invalidate().is_some(), "调用方在剪贴板变化处失效文件槽");
    assert!(rx.slot().is_none());
}
