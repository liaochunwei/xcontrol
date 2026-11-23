//! mock 平台层（`feature = "mock-platform"`，docs/08 §4 回环集成测试入口）。
//!
//! 内存实现 [`crate::sink`] 的捕获/注入契约 + 剪贴板 watch 语义的脚本化替身，
//! 供进程内 host↔client 全链路测试消费（`xcontrol-net` tests/scenarios.rs：
//! proto 编解码 → Noise 加密 → 回环传输 → core Session/Dispatcher → mock 注入）。
//!
//! - [`ScriptedInputSource`]：脚本化
//!   [`InputSource`](crate::sink::InputSource)——预设序列产出按键/鼠标/滚轮/
//!   文本事件（因果顺序即脚本顺序）；`long_text` 演示超长 Text 分段
//!   （docs/02 §4.2 kind 0x07「超长主控分段多条」的发送侧职责）。
//!   未知 kind 帧属线层边界场景（`InputEvent` 无法表达），由测试经
//!   `xcontrol_proto::encode_frame_into` 直接注入出站流。
//! - [`RecordingInputSink`]：记录全部注入动作（事件/warp/释放）供链路断言
//!   （T3 tests/chain.rs 的简版 RecordingSink 迁移至此）。
//! - [`MockClipboard`]：内容 + 变更计数（watch 触发语义）+ 传输计划脚本化
//!   （超限不传输，docs/04 §3）；`ClipboardWatch` trait 随 xcontrol-clipboard
//!   平台 crate 落地后对接（`.trellis/spec/rust-core/architecture-rules.md`）。
//!
//! mock 只依赖 proto/std/blake3（blake3 为 core 既有白名单依赖；零平台 API、
//! 零 I/O），不进默认 features。

pub mod clipboard;
pub mod input;
pub mod sink;

pub use clipboard::{MockClipboard, TransferPlan};
pub use input::ScriptedInputSource;
pub use sink::RecordingInputSink;
