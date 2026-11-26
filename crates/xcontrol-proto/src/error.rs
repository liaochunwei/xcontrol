//! 统一协议错误（`.trellis/spec/rust-core/error-handling.md`）：
//!
//! - 单一 [`ProtoError`]，变体全部可编程区分（携带 kind / 字段名 / 期望与实际值等上下文）；
//! - 库代码零 `unwrap()`/`expect()`——外部输入（网络帧）一律走 `Result`；
//! - 错误信息只含长度/取值等诊断量，禁止出现安全码、剪贴板内容、密钥派生中间值。

/// XCP 协议编解码错误（docs/02 §4/§5/§6）。
///
/// 判定顺序约定：帧级错误（截断/越界）在 [`crate::frame`] 报出；payload 级
/// 错误（布局/值域/UTF-8/postcard）在消息解码层报出。任何变体都表示
/// 「对端或数据流违反 wire 契约」，调用方按 docs/02 §7 code 0x04 语义断开。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtoError {
    /// 帧头不完整（不足 [`crate::frame::FRAME_HEADER_LEN`] 字节）。
    #[error("帧头不完整：需要 {needed} 字节，实际 {got}")]
    FrameTruncated {
        /// 需要的字节数（帧头定长）。
        needed: usize,
        /// 实际可用的字节数。
        got: usize,
    },
    /// 声明的 `len` 超出缓冲内可用字节（docs/02 §4.1：len 越界必须报错）。
    #[error("payload 越界：len={declared} 超出剩余 {available} 字节")]
    PayloadOutOfBounds {
        /// 帧头声明的 payload 字节数。
        declared: u16,
        /// 帧头之后实际剩余的字节数。
        available: usize,
    },
    /// 编码侧：payload 超过帧头 `len: u16` 可表达的物理上限。
    #[error("payload 超长：{len} 字节超出帧上限 {max}")]
    PayloadTooLarge {
        /// 待编码 payload 长度。
        len: usize,
        /// 帧允许的 payload 上限（u16::MAX）。
        max: usize,
    },
    /// 定长消息 payload 与 wire 布局不符（多字节/少字节，docs/02 §4.2）。
    #[error("kind 0x{kind:02X} payload 定长不符：需要 {expected} 字节，实际 {got}")]
    LayoutMismatch {
        /// 帧 kind。
        kind: u8,
        /// wire 布局要求的字节数。
        expected: usize,
        /// 实际 payload 字节数。
        got: usize,
    },
    /// `Text` 事件超过 512B 上限（docs/02 §4.2 kind 0x07；主控应分段发送）。
    #[error("Text 事件超长：{len} 字节超出上限 {max}")]
    TextTooLong {
        /// 声明/实际长度。
        len: usize,
        /// 协议上限（512B）。
        max: usize,
    },
    /// `Text` payload 不是合法 UTF-8（docs/03 §4.5：必须整段 UTF-8）。
    #[error("Text payload 不是合法 UTF-8")]
    TextUtf8(#[source] core::str::Utf8Error),
    /// XKey 落在保留区（docs/03 §2：0x04–0xE7 / 0x100–0x1FF / 0x200–0x2FF 之外禁止使用）。
    #[error("XKey 0x{code:04X} 在保留区")]
    ReservedKey {
        /// 越界的原始键码值。
        code: u16,
    },
    /// 枚举字段取值不在文档定义域（KeyState/MouseButton/Axis/ScreenEdge 等）。
    #[error("kind 0x{kind:02X} 字段 {field} 取值 {value} 不在定义域")]
    FieldValue {
        /// 帧 kind。
        kind: u8,
        /// 违规字段名（wire 契约中的字段）。
        field: &'static str,
        /// 线上的非法取值。
        value: u8,
    },
    /// 浮点字段非法（NaN / inf；坐标/ratio/delta 必须是有限值）。
    #[error("kind 0x{kind:02X} 浮点字段 {field} 非有限值")]
    NonFiniteFloat {
        /// 帧 kind。
        kind: u8,
        /// 违规字段名。
        field: &'static str,
    },
    /// postcard 解码失败（控制/剪贴板消息，docs/02 §4.3/§4.4）。
    #[error("postcard 解码失败：{0}")]
    Postcard(String),
    /// 帧 kind 与消息类型不符（postcard 判别值 ≠ 帧 kind，docs/02 §4.3）。
    #[error("帧 kind 0x{kind:02X} 与消息类型不符（该消息期望 0x{expected:02X}）")]
    KindMismatch {
        /// 帧头携带的 kind。
        kind: u8,
        /// 解码出的消息实际对应的 kind。
        expected: u8,
    },
    /// 版本协商失败：major 不等（docs/02 §5.2：立即 Error{UnsupportedVersion} 断开）。
    #[error("版本不兼容：本地 {local_major}.{local_minor}，对端 {remote_major}.{remote_minor}")]
    VersionIncompatible {
        /// 本地协议版本。
        local_major: u8,
        /// 本地协议 minor。
        local_minor: u8,
        /// 对端协议 major。
        remote_major: u8,
        /// 对端协议 minor。
        remote_minor: u8,
    },
    /// `ClipAnnounce.total_len` 超过协商上限（docs/02 §6.1：超限即违规断开）。
    #[error("clip total_len {total_len} 超过上限 {max}")]
    ClipTotalLenExceeded {
        /// 宣称的剪贴板总长。
        total_len: u32,
        /// 协商上限（默认 16MiB，docs/02 §6）。
        max: u32,
    },
    /// `ClipChunk.data` 超过 64KiB 单块上限（docs/02 §6.2）。
    #[error("clip chunk 数据 {len} 字节超过单块上限 {max}")]
    ClipChunkTooLarge {
        /// 块数据实际长度。
        len: usize,
        /// 单块上限（64KiB）。
        max: usize,
    },
    /// 文件清单条目数超上限（docs/02 §4.6 防护规则 1：单帧承载，条目 ≤512）。
    #[error("文件清单条目数 {count} 超过上限 {max}")]
    FileEntriesExceeded {
        /// 实际条目数。
        count: usize,
        /// 条目数上限（512，docs/02 §4.6）。
        max: usize,
    },
    /// 文件条目名非法：含 `/`（分层只经目录条目表达）或超 256B UTF-8
    /// （docs/02 §4.6 防护规则 1；wire 只允许相对纯名）。
    #[error("文件条目名非法（含 '/' 或超 {max} 字节）")]
    FileNameInvalid {
        /// 名称长度上限（256B）。
        max: usize,
    },
    /// `FileRefAnnounce.total_bytes` 超过协商上限（docs/02 §4.6：默认 2GiB 可配，
    /// 独立于剪贴板 16MiB）。
    #[error("文件清单总字节 {total_bytes} 超过上限 {max}")]
    FileTotalBytesExceeded {
        /// 宣称的内容总字节。
        total_bytes: u64,
        /// 协商上限（默认 2GiB）。
        max: u64,
    },
    /// `FileChunk.data` 超过 64KiB 单块上限（docs/02 §4.6，与 ClipChunk 同源）。
    #[error("file chunk 数据 {len} 字节超过单块上限 {max}")]
    FileChunkTooLarge {
        /// 块数据实际长度。
        len: usize,
        /// 单块上限（64KiB）。
        max: usize,
    },
}

impl ProtoError {
    /// 把 postcard 错误转成携带 Display 文本的 [`ProtoError::Postcard`]
    /// （postcard::Error 不保证 `PartialEq`，取其 Display 串保持错误值可比较）。
    pub(crate) fn from_postcard_decode(error: postcard::Error) -> Self {
        Self::Postcard(error.to_string())
    }
}
