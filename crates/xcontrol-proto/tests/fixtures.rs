//! golden fixtures（docs/02 §8、`.trellis/spec/rust-core/protocol-rules.md`）：
//! 每条消息 kind 一个确定性样本帧，落盘 `tests/fixtures/xcp/*.bin` 并提交入库。
//!
//! - 比对测试（默认运行）：当前编码 == golden 字节；同时把 golden 解码回消息判等。
//!   wire 变更属有意时 → docs/02 先行 + minor+1 + 本地重跑 `--ignored` 生成测试刷新 fixtures。
//! - 生成测试（`--ignored`）：仅本地手动运行，把确定性样本编码写盘：
//!   `cargo test -p xcontrol-proto --test fixtures -- --ignored generate_fixtures`
//!
//! 样本值全部为确定性的假数据（无真实安全码/剪贴板内容，testing.md 红线）。

use std::fs;
use std::path::{Path, PathBuf};

use xcontrol_proto::xkey::{CK_VOLUME_UP, KEY_A};
use xcontrol_proto::{
    Axis, Caps, ChannelBind, ClipAck, ClipAnnounce, ClipChunk, ClipEnd, ClipFormat, ClipMsg,
    CtrlMsg, ErrCode, FileAck, FileBind, FileCancel, FileChunk, FileEnd, FileEntry, FileEntryKind,
    FileErrCode, FileError, FileExpand, FileMsg, FilePullReq, FileRefAnnounce, InputEvent, KeyState,
    KindClass, Mods, MouseButton, Options, ProtoError, ProtoVer, ScreenEdge, decode_clip,
    decode_ctrl, decode_file, decode_frame, kind_class,
};

const FIXTURE_DIR: &str = "tests/fixtures/xcp";

/// 解码后的期望值（四类消息统一载体）。
#[derive(Debug, PartialEq)]
enum Sample {
    Input(InputEvent),
    Ctrl(CtrlMsg),
    Clip(ClipMsg),
    File(FileMsg),
}

/// kind+payload → 期望样本（与 lib.rs 接收端分派模式一致）。
fn decode_sample(kind: u8, payload: &[u8]) -> Result<Option<Sample>, ProtoError> {
    Ok(match kind_class(kind) {
        KindClass::Input => InputEvent::decode(kind, payload)?.map(Sample::Input),
        KindClass::Ctrl => decode_ctrl(kind, payload)?.map(Sample::Ctrl),
        KindClass::Clip => decode_clip(kind, payload)?.map(Sample::Clip),
        KindClass::File => decode_file(kind, payload)?.map(Sample::File),
        KindClass::Unknown => None,
    })
}

fn input(code: &xcontrol_proto::xkey::XKey, state: KeyState) -> Sample {
    Sample::Input(InputEvent::Key { code: *code, state })
}

/// 全部消息的确定性样本帧：`(文件名, 帧字节, 期望解码值)`。
/// **每条消息 kind 恰好一个样本**（docs/02 §8：每消息 ≥1 个 golden fixture）。
fn sample_frames() -> Vec<(&'static str, Vec<u8>, Sample)> {
    let key_down = input(&KEY_A, KeyState::Down);
    let key_up = input(&KEY_A, KeyState::Up);
    let rel = Sample::Input(InputEvent::MouseMoveRel { dx: 3.5, dy: -7.25 });
    let abs = Sample::Input(InputEvent::MouseMoveAbs {
        screen: 1,
        x: 0.25,
        y: 0.75,
    });
    let button = Sample::Input(InputEvent::MouseButton {
        button: MouseButton::Right,
        state: KeyState::Down,
    });
    let wheel = Sample::Input(InputEvent::Wheel {
        axis: Axis::Horizontal,
        delta: -1.5,
    });
    let text = Sample::Input(InputEvent::Text {
        utf8: "héllo ×中".to_owned(),
    });
    let enter = Sample::Input(InputEvent::EnterScreen {
        target: 1,
        edge: ScreenEdge::Right,
        ratio: 0.5,
        mods: Mods::LCTRL | Mods::LMETA,
        preserve_cursor: false,
    });
    let leave = Sample::Input(InputEvent::LeaveScreen { mods: Mods::RMETA });
    let sync = Sample::Input(InputEvent::ModifierSync {
        mods: Mods::LSHIFT | Mods::RALT,
    });
    let switch_req = Sample::Input(InputEvent::SwitchRequest { target: 2 });
    let switch_ack = Sample::Input(InputEvent::SwitchAck {
        target: 2,
        ok: true,
    });

    let hello = Sample::Ctrl(CtrlMsg::Hello {
        proto: ProtoVer::CURRENT,
        name: "host".to_owned(),
        screen_name: "desk".to_owned(),
        instance_id: [0xAB_u8; 16],
        caps: Caps::UNICODE | Caps::IMAGE | Caps::HTML | Caps::ABS_POINTER,
        salt: [0x5A_u8; 16],
    });
    let hello_back = Sample::Ctrl(CtrlMsg::HelloBack {
        proto: ProtoVer { major: 1, minor: 1 },
        name: "client".to_owned(),
        instance_id: [0xCD_u8; 16],
        caps: Caps::UNICODE | Caps::ABS_POINTER,
    });
    let layout = Sample::Ctrl(CtrlMsg::Layout {
        screens: vec![
            xcontrol_proto::ScreenInfo {
                id: 0,
                owner: [0xAB_u8; 16],
                name: "primary".to_owned(),
                left: None,
                right: Some(1),
                top: None,
                bottom: None,
                scale: 2.0,
            },
            xcontrol_proto::ScreenInfo {
                id: 1,
                owner: [0xCD_u8; 16],
                name: "lap".to_owned(),
                left: Some(0),
                right: None,
                top: None,
                bottom: None,
                scale: 1.0,
            },
        ],
    });
    let set_options = Sample::Ctrl(CtrlMsg::SetOptions {
        options: Options::defaults(),
    });
    let error = Sample::Ctrl(CtrlMsg::Error {
        code: ErrCode::UNSUPPORTED_VERSION,
        msg: "peer major mismatch".to_owned(),
    });
    let ping = Sample::Ctrl(CtrlMsg::Ping { ts_ms: 1_234_567 });
    let pong = Sample::Ctrl(CtrlMsg::Pong { ts_ms: 1_234_567 });
    let pause = Sample::Ctrl(CtrlMsg::Pause);
    let resume = Sample::Ctrl(CtrlMsg::Resume);

    let bind = Sample::Clip(ClipMsg::Bind(ChannelBind {
        session_id: [0x11_u8; 16],
    }));
    let announce = Sample::Clip(ClipMsg::Announce(ClipAnnounce {
        clip_id: 7,
        origin_id: [0x22_u8; 16],
        origin_seq: 42,
        formats: vec![ClipFormat::Text, ClipFormat::Png],
        total_len: 2048,
        blake3: [0x33_u8; 32],
    }));
    let chunk = Sample::Clip(ClipMsg::Chunk(ClipChunk {
        clip_id: 7,
        seq: 0,
        data: b"deterministic clip payload".to_vec(),
    }));
    let ack = Sample::Clip(ClipMsg::Ack(ClipAck { seq: 3 }));
    let end = Sample::Clip(ClipMsg::End(ClipEnd { clip_id: 7 }));

    let file_entry = FileEntry {
        name: "report.pdf".to_owned(),
        size: 2048,
        mtime_ms: 1_700_000_000_000,
        kind: FileEntryKind::File,
        children: 0,
    };
    let file_dir = FileEntry {
        kind: FileEntryKind::Dir,
        children: 3,
        ..file_entry.clone()
    };
    let file_bind = Sample::File(FileMsg::Bind(FileBind {
        session_id: [0x11_u8; 16],
    }));
    let file_announce = Sample::File(FileMsg::RefAnnounce(FileRefAnnounce {
        transfer_id: 7,
        origin_id: [0x22_u8; 16],
        origin_seq: 42,
        entries: vec![file_entry.clone(), file_dir],
        total_bytes: 2048,
        copied_at_ms: 1_700_000_000_000,
    }));
    let file_pull = Sample::File(FileMsg::PullReq(FilePullReq {
        transfer_id: 7,
        entry: 2,
        offset: 4096,
    }));
    let file_chunk = Sample::File(FileMsg::Chunk(FileChunk {
        transfer_id: 7,
        seq: 3,
        data: b"deterministic file payload".to_vec(),
    }));
    let file_ack = Sample::File(FileMsg::Ack(FileAck { seq: 3 }));
    let file_end = Sample::File(FileMsg::End(FileEnd {
        transfer_id: 7,
        entry: 2,
        blake3: [0x33_u8; 32],
    }));
    let file_error = Sample::File(FileMsg::Error(FileError {
        transfer_id: 7,
        entry: Some(2),
        code: FileErrCode::NOT_FOUND,
    }));
    let file_cancel = Sample::File(FileMsg::Cancel(FileCancel { transfer_id: 7 }));
    let file_expand = Sample::File(FileMsg::Expand(FileExpand {
        transfer_id: 7,
        parent: 1,
        base: 3,
        entries: vec![file_entry],
    }));

    let samples = [
        ("key", 1u32, key_down),
        (
            "key_repeat",
            2,
            Sample::Input(InputEvent::KeyRepeat { code: KEY_A }),
        ),
        ("mouse_move_rel", 3, rel),
        ("mouse_move_abs", 4, abs),
        ("mouse_button", 5, button),
        ("wheel", 6, wheel),
        ("text", 7, text),
        ("enter_screen", 8, enter),
        ("leave_screen", 9, leave),
        ("modifier_sync", 10, sync),
        ("switch_request", 11, switch_req),
        ("switch_ack", 12, switch_ack),
        ("ctrl_hello", 20, hello),
        ("ctrl_hello_back", 21, hello_back),
        ("ctrl_layout", 22, layout),
        ("ctrl_set_options", 23, set_options),
        ("ctrl_error", 24, error),
        ("ctrl_ping", 25, ping),
        ("ctrl_pong", 26, pong),
        ("ctrl_pause", 27, pause),
        ("ctrl_resume", 28, resume),
        ("clip_channel_bind", 30, bind),
        ("clip_announce", 31, announce),
        ("clip_chunk", 32, chunk),
        ("clip_ack", 33, ack),
        ("clip_end", 34, end),
        ("file_bind", 40, file_bind),
        ("file_ref_announce", 41, file_announce),
        ("file_pull_req", 42, file_pull),
        ("file_chunk", 43, file_chunk),
        ("file_ack", 44, file_ack),
        ("file_end", 45, file_end),
        ("file_error", 46, file_error),
        ("file_cancel", 47, file_cancel),
        ("file_expand", 48, file_expand),
    ];
    let _ = (key_up, CK_VOLUME_UP); // 常量哨兵：保持 import 对齐 docs/03 §2
    samples
        .into_iter()
        .map(|(name, seq, sample)| {
            let message: &dyn xcontrol_proto::Message = match &sample {
                Sample::Input(event) => event,
                Sample::Ctrl(msg) => msg,
                Sample::Clip(msg) => msg.as_message(),
                Sample::File(msg) => msg.as_message(),
            };
            let bytes = message
                .encode_frame(seq)
                .unwrap_or_else(|error| panic!("样本编码必须成功：{error}"));
            (name, bytes, sample)
        })
        .collect()
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(format!("{name}.bin"))
}

/// golden 比对：编码字节逐文件一致 + golden 可解码回等值消息（docs/02 §8）。
#[test]
fn fixtures_match_golden() {
    let samples = sample_frames();
    assert_eq!(
        samples.len(),
        35,
        "12 输入 + 9 控制 + 5 剪贴板 + 9 文件 = 35 条消息 kind"
    );
    for (name, bytes, expected) in samples {
        let path = fixture_path(name);
        let golden = fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "缺少 fixture {:?}：{error}（--ignored generate_fixtures 重新生成）",
                path
            )
        });
        assert_eq!(
            golden, bytes,
            "fixture {name} 与当前编码不一致：若 wire 变更属有意，先改 docs/02 → minor+1 → 重跑生成测试"
        );
        let decoded = decode_frame(&golden).expect("golden 帧必须可解");
        let sample = decode_sample(decoded.header.kind, decoded.payload)
            .expect("golden payload 必须可解")
            .unwrap_or_else(|| panic!("golden kind 0x{:02X} 无解码分派", decoded.header.kind));
        assert_eq!(sample, expected, "golden 解码值不等：{name}");
    }
}

/// 生成/刷新 golden fixtures（仅本地手动运行，产物提交入库）。
#[test]
#[ignore = "重新生成 fixtures：cargo test -p xcontrol-proto --test fixtures -- --ignored generate_fixtures（仅 wire 有意变更时，docs/02 先行）"]
fn generate_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    fs::create_dir_all(&dir).expect("创建 fixtures 目录");
    for (name, bytes, _) in sample_frames() {
        let path = fixture_path(name);
        fs::write(&path, &bytes).unwrap_or_else(|error| panic!("写 {path:?} 失败：{error}"));
        println!("wrote {} ({} bytes)", path.display(), bytes.len());
    }
}
