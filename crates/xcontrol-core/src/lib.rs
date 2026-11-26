//! `xcontrol-core` — 无 IO 的核心状态机：会话、屏幕布局、事件路由、映射引擎。
//!
//! 分层红线（docs/08 §1、`.trellis/spec/rust-core/architecture-rules.md`）：
//!
//! - 仅依赖 `xcontrol-proto` 与 `xcontrol-config`（+ 已过审的 thiserror/blake3：
//!   错误类型与 session_id 派生，均为纯逻辑、无 I/O）；
//! - 无 IO、无平台 API、无 tokio 运行时依赖；
//! - `#![forbid(unsafe_code)]`；非测试代码零 `unwrap()`/`expect()`；
//! - UI / 平台层均为其投影与实现，核心必须可在普通 `cargo test` 全量运行。
//!
//! # 模块导览
//!
//! | 模块 | 内容 | 语义锚点 |
//! | --- | --- | --- |
//! | [`session`] | [`Session`] 会话状态机（Idle→Handshaking→Running{paused}→Disconnected，转移表集中单函数）、心跳 3s/9s、版本协商、session_id | docs/02 §3.3/§4.5/§5，docs/03 §3 |
//! | [`clock`] | [`Clock`] 毫秒时钟注入 + [`SystemClock`](clock::SystemClock)/[`TestClock`](clock::TestClock) | docs/02 §4.5 可测性前提 |
//! | [`layout`] | [`ScreenLayout`] 布局图 + [`edge_crossing`](layout::edge_crossing)/[`to_abs`](layout::to_abs) 纯函数 | docs/03 §5，FR-1.3/FR-2.7 |
//! | [`route`] | [`Dispatcher`](route::Dispatcher) 帧→动作 + [`apply_actions`](route::apply_actions) 执行统计 | docs/02 §4.1 |
//! | [`sink`] | [`InputSink`]/[`InputSource`] 注入/捕获 trait（docs/08 §2 命名；实现由平台 crate/mock 提供） | docs/08 §2 |
//! | [`mods`] | [`ModifierTracker`] 修饰键跟踪 + 对齐/释放/周期校验差异计算 | docs/03 §3 |
//! | [`clip_rx`] | [`ClipRx`] 剪贴板接收重组状态机（bind/上限/累计/seq 窗口/10s 超时/blake3 交付） | docs/02 §6，docs/04 §3 |
//! | [`clip_plan`] | 剪贴板发送计划生成（内容 → 分块 + blake3；超限不传输） | docs/02 §6.2，docs/04 §3 |
//! | [`clip_tx`] | [`ClipTx`] 剪贴板发送端信用窗口（初始 4 块 + Ack 归还） | docs/02 §6.3 |
//! | [`file_ref`] | 文件引用清单构建 + pending 槽（代次失效 + `(name,size)` 回环指纹） | docs/02 §4.6，docs/04 §8 |
//!
//! # 分层位置
//!
//! ```text
//! net（Noise/TCP，解密出明文流） ──► core（本 crate：会话判定/布局/修饰键）
//!                                        │ SessionAction
//!                                        ▼
//!                    平台 crate（InputSink 注入）/ net（出站帧加密）
//! ```
//!
//! core 只消费 proto 类型（Frame/InputEvent/CtrlMsg），不重复定义 wire 结构；
//! net 管传输+加密+握手字节，core 管握手成功后的会话语义。
//!
//! # 最小用法（从机侧）
//!
//! ```rust
//! use xcontrol_core::clock::TestClock;
//! use xcontrol_core::session::{Identity, Role, Session, SessionEvent};
//! use xcontrol_proto::{Caps, Options};
//!
//! let clock = TestClock::new();
//! let mut session = Session::new(
//!     Role::Client,
//!     Identity {
//!         name: "client".into(),
//!         screen_name: "client-screen".into(),
//!         instance_id: [1; 16],
//!         caps: Caps::UNICODE,
//!         salt: [7; 16],
//!     },
//!     clock.clone(),
//!     Options::defaults(),
//! );
//! // Noise 握手完成（net 层回调）→ 携带握手摘要进入版本协商
//! let actions = session
//!     .step(SessionEvent::HandshakeReady { hs_hash: [0xAB; 32] })
//!     .unwrap();
//! assert!(actions.is_empty(), "从机就绪无出站");
//! assert!(session.session_id().is_some());
//! ```
//!
//! feature：`mock-platform` 供回环集成测试对接 mock 平台层
//! （docs/08 §4：`cargo test -p xcontrol-core --features mock-platform`）。
//! 进程内 host+client 全链路接线见 `xcontrol-net` tests/scenarios.rs
//! （经 proto 编解码 + Noise 加密 + 回环传输驱动 mock 源/汇与剪贴板状态机）。
#![forbid(unsafe_code)]

pub mod clip_plan;
pub mod clip_rx;
pub mod clip_tx;
pub mod clock;
pub mod file_ref;
pub mod file_rx;
pub mod file_serve;
pub mod file_tx;
pub mod hotkey;
pub mod layout;
pub mod mapping;
#[cfg(feature = "mock-platform")]
pub mod mock;
pub mod mods;
pub mod route;
pub mod session;
pub mod sink;

pub use clip_rx::{CLIP_TIMEOUT_MS, ClipRx, ClipRxAction, ClipRxConfig};
pub use clip_tx::{ClipTx, INITIAL_CREDIT_CHUNKS};
pub use clock::{Clock, SystemClock, TestClock};
pub use file_ref::{
    FileRefRx, FileRefRxConfig, FileRefVerdict, PendingSlot, build_manifest, manifest_fingerprint,
};
pub use file_rx::{
    FILE_IDLE_TIMEOUT_MS, FILE_TOTAL_DURATION_CAP_MS, FileRx, FileRxAction, FileRxConfig,
};
pub use file_serve::{FileServe, MAX_ACTIVE_TRANSFERS, ServeVerdict};
pub use file_tx::{FileTx, INITIAL_FILE_CREDIT_CHUNKS};
pub use hotkey::{
    FilterOutcome, HotkeyCombo, HotkeyError, HotkeyFilter, HotkeyId, HotkeySet, ModFamilies,
};
pub use layout::{EdgeCrossing, PixelPoint, ScreenLayout, edge_crossing, to_abs};
pub use mapping::{MappingEngine, MappingError, MappingSink, PresetRule, preset_rules};
#[cfg(feature = "mock-platform")]
pub use mock::{MockClipboard, RecordingInputSink, ScriptedInputSource};
pub use mods::{ModifierDiff, ModifierTracker};
pub use route::{ApplyReport, Dispatcher, FeedOutcome, RouteError, apply_actions};
pub use session::{
    ControlDir, DisconnectReason, FramePayload, Identity, LocalCmd, MODIFIER_SYNC_INTERVAL_MS,
    Notice, Outbound, Role, Session, SessionAction, SessionEvent, SessionState, StateError,
    derive_session_id,
};
pub use sink::{InputSink, InputSource, SinkError};
