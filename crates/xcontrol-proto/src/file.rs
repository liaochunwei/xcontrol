//! 文件通道消息（docs/02 §4.6，XCP 1.2+，FR-7.3；postcard 编码）。
//!
//! 决策依据 [ADR-0004](../../../docs/adr/0004-file-transfer-channel-and-materialization.md)：
//! 文件域走独立第三条 TCP（与剪贴板通道同构），首帧 [`FileBind`] 绑定控制会话。
//! 传输模型两种流：
//!
//! 1. **元数据面**：复制端 [`FileRefAnnounce`]（仅清单，无内容字节）；
//! 2. **内容面**（接收端发起）：[`FilePullReq`] → [`FileChunk`] → [`FileAck`] →
//!    [`FileEnd`]（条目级 blake3）；失败 [`FileError`]；随时 [`FileCancel`]；
//!    目录条目先 [`FileExpand`] 展开子清单。
//!
//! # 信任边界在接收端（docs/02 §4.6 防护规则，同 §6 原则）
//!
//! 解码层强制（发送端自律不作数）：
//! - 清单单帧承载：条目 ≤ [`FileRefAnnounce::MAX_ENTRIES`]、名称 ≤
//!   [`FileEntry::MAX_NAME_BYTES`] 且不含 `/`、`.`/`..`、NUL；
//! - [`FileRefAnnounce::total_bytes`] 超协商上限 → [`ProtoError::FileTotalBytesExceeded`]；
//! - [`FileChunk::data`] 超 64KiB → [`ProtoError::FileChunkTooLarge`]。
//!
//! **寻址红线**（docs/07 §9）：拉取请求只含 `transfer_id + entry 下标 (+offset)`，
//! 任何帧不得携带路径——下标 → 本机路径映射由源端私有维护。
//!
//! 其余会话级校验（未知 transfer_id/越界下标/seq 单调/信用窗口/超时）依赖连接态，
//! 由 core/net 层执行。

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;
use crate::frame::{self, MAX_FRAME_PAYLOAD};

/// 清单条目种类（docs/02 §4.6 wire 钉死；postcard 按变体声明序编码，File=0/Dir=1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileEntryKind {
    /// 普通文件。
    File,
    /// 目录（`children` 为子树条目总数，可为 0 = 未统计；拉取时经 [`FileExpand`] 展开）。
    Dir,
}

/// 清单条目（docs/02 §4.6 wire 钉死）。
///
/// `name` 为**相对纯名**：不含 `/`、不为 `.`/`..`、不含 NUL、≤ 256B UTF-8
/// （[`FileEntry::MAX_NAME_BYTES`]）——分层只经目录条目表达，目录穿越分量在
/// 协议层就地禁止（docs/02 §4.6 防护规则 1）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileEntry {
    /// 条目名（UTF-8 相对纯名）。
    pub name: String,
    /// 条目字节数（文件 = 内容大小；目录 = 子树聚合估计）。
    pub size: u64,
    /// 修改时间（Unix 毫秒；仅展示用，**不参与回环指纹**——docs/04 §8.3）。
    pub mtime_ms: u64,
    /// 条目种类。
    pub kind: FileEntryKind,
    /// 目录 = 子树条目总数（未统计为 0）；文件恒 0。
    pub children: u32,
}

impl FileEntry {
    /// 名称字节上限（docs/02 §4.6 防护规则 1）。
    pub const MAX_NAME_BYTES: usize = 256;

    /// 名称合法性（编码与解码两侧行为一致）：
    /// 非空、≤ 256B、不含 `/`、不为 `.`/`..`、不含 NUL。
    #[must_use]
    pub fn is_valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= Self::MAX_NAME_BYTES
            && !name.contains('/')
            && name != "."
            && name != ".."
            && !name.contains('\0')
    }

    /// 校验名称，违规返回 [`ProtoError::FileNameInvalid`]。
    fn validate_name(&self) -> Result<(), ProtoError> {
        if Self::is_valid_name(&self.name) {
            Ok(())
        } else {
            Err(ProtoError::FileNameInvalid {
                max: Self::MAX_NAME_BYTES,
            })
        }
    }
}

/// 文件连接首帧（docs/02 §4.6）：绑定控制连接的会话（同 [`crate::clip::ChannelBind`] 语义）。
///
/// 对端校验 session 存在且绑定唯一，否则断开；`session_id` 由握手摘要派生
/// （docs/02 §3.3，16B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileBind {
    /// 控制连接会话标识（16B）。
    pub session_id: [u8; 16],
}

impl FileBind {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for FileBind {
    fn kind(&self) -> u8 {
        frame::kind::FILE_BIND
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 文件引用元数据清单（docs/02 §4.6；FR-7.3 元数据面——复制端在剪贴板出现文件
/// 引用时发送，**无内容字节**）。
///
/// `origin_id`/`origin_seq` 为来源去重字段（docs/04 §8.2 代次失效，`origin_id`
/// 即来源 instance_id）。**清单必须单帧承载**（物理上限先行，docs/02 §4.6 防护
/// 规则 1）：条目 ≤ [`Self::MAX_ENTRIES`]，编码后 ≤ [`Self::MAX_ENCODED_LEN`]，
/// 编码期超限即拒绝（`PayloadTooLarge`），不产生分帧。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileRefAnnounce {
    /// 本次传输标识（会话内唯一；后续拉取按此寻址）。
    pub transfer_id: u32,
    /// 来源实例 UUID（16B，docs/04 §8.2）。
    pub origin_id: [u8; 16],
    /// 来源序列号（每源递增；接收端按 `(origin_id, seq)` 去重）。
    pub origin_seq: u32,
    /// 顶层条目清单（≤ [`Self::MAX_ENTRIES`]；子树经 [`FileExpand`] 惰性展开）。
    pub entries: Vec<FileEntry>,
    /// 内容总字节数（解码层按协商上限校验，默认 2GiB，独立于剪贴板 16MiB）。
    pub total_bytes: u64,
    /// 复制时刻（Unix 毫秒；仅展示/诊断用）。
    pub copied_at_ms: u64,
}

impl FileRefAnnounce {
    /// 清单条目数上限（docs/02 §4.6 防护规则 1：单帧承载）。
    pub const MAX_ENTRIES: usize = 512;
    /// 默认总字节上限：2GiB（docs/02 §4.6；可经协商上限收紧）。
    pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    /// 清单编码后单帧承载预算（字节）。
    ///
    /// 推导同 docs/02 §6.2（物理上限先行）：帧头 7B + payload 必须 ≤ Noise 单报文
    /// 明文 65519B（snow 65535 − 16B tag）→ payload ≤ 65512B；帧头 `len: u16`
    /// 物理上限 65535 不构成额外约束。
    pub const MAX_ENCODED_LEN: usize = 65519 - frame::FRAME_HEADER_LEN;

    /// payload 编码（postcard），追加到 `out`。
    ///
    /// 编码期校验（发送端自律 + 物理上限先行）：条目数/名称合法性、编码后
    /// ≤ [`Self::MAX_ENCODED_LEN`]（单帧承载，违规拒绝不产生分帧）。
    ///
    /// # Errors
    /// [`ProtoError::FileEntriesExceeded`] / [`ProtoError::FileNameInvalid`] /
    /// [`ProtoError::PayloadTooLarge`] / [`ProtoError::Postcard`]。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.validate()?;
        let mut scratch = Vec::new();
        frame::encode_postcard_into(self, &mut scratch)?;
        if scratch.len() > Self::MAX_ENCODED_LEN {
            return Err(ProtoError::PayloadTooLarge {
                len: scratch.len(),
                max: Self::MAX_ENCODED_LEN,
            });
        }
        out.extend_from_slice(&scratch);
        Ok(())
    }

    /// 解码并以**默认上限**（2GiB）校验 `total_bytes`。
    ///
    /// # Errors
    /// [`ProtoError::FileTotalBytesExceeded`] / [`ProtoError::Postcard`] /
    /// 条目校验失败（同 [`Self::validate`] 对应变体）。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        Self::decode_with_limit(payload, Self::DEFAULT_MAX_TOTAL_BYTES)
    }

    /// 解码并以**协商上限**校验 `total_bytes`（docs/02 §4.6：信任边界在接收端）。
    ///
    /// # Errors
    /// - `entries.len() > MAX_ENTRIES` → [`ProtoError::FileEntriesExceeded`]；
    /// - 任一条目名非法 → [`ProtoError::FileNameInvalid`]；
    /// - `total_bytes > max_total_bytes` → [`ProtoError::FileTotalBytesExceeded`]；
    /// - postcard 解码失败 / 尾部残留 → [`ProtoError::Postcard`]。
    pub fn decode_with_limit(
        payload: &[u8],
        max_total_bytes: u64,
    ) -> Result<Self, ProtoError> {
        let announce: Self = frame::decode_postcard(payload)?;
        announce.validate()?;
        if announce.total_bytes > max_total_bytes {
            return Err(ProtoError::FileTotalBytesExceeded {
                total_bytes: announce.total_bytes,
                max: max_total_bytes,
            });
        }
        Ok(announce)
    }

    /// 结构校验（条目数 + 名称），编码/解码共用。
    fn validate(&self) -> Result<(), ProtoError> {
        validate_entries(&self.entries)
    }
}

impl crate::Message for FileRefAnnounce {
    fn kind(&self) -> u8 {
        frame::kind::FILE_REF_ANNOUNCE
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 拉取请求（docs/02 §4.6；**条目索引寻址**——任何帧不得携带路径，docs/07 §9）。
///
/// 会话级校验（transfer_id 已通告、下标越界、offset ≤ size）依赖连接态，
/// 由 core/net 层执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FilePullReq {
    /// 目标传输。
    pub transfer_id: u32,
    /// 条目全局下标（清单 + FileExpand 展开分配的连续空间）。
    pub entry: u32,
    /// 起始偏移（≤ 条目 size；断点续传预留）。
    pub offset: u64,
}

impl FilePullReq {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for FilePullReq {
    fn kind(&self) -> u8 {
        frame::kind::FILE_PULL_REQ
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 文件数据块（docs/02 §4.6；64KiB 单块上限，发送有效分块 65472B 推导同 §6.2）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileChunk {
    /// 所属传输。
    pub transfer_id: u32,
    /// 块序号（单调递增不回绕；连接态校验）。
    pub seq: u32,
    /// 块数据（**解码层校验 ≤ 64KiB**）。
    pub data: Vec<u8>,
}

impl FileChunk {
    /// 单块数据上限：64KiB（解码语义上限；发送有效分块 65472B，推导同
    /// [`crate::clip::ClipChunk`]）。
    pub const MAX_DATA_LEN: usize = MAX_FRAME_PAYLOAD;

    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }

    /// 解码并校验 `data` ≤ 64KiB（docs/02 §4.6 防护规则 4）。
    ///
    /// # Errors
    /// [`ProtoError::FileChunkTooLarge`] / [`ProtoError::Postcard`]。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        let chunk: Self = frame::decode_postcard(payload)?;
        if chunk.data.len() > Self::MAX_DATA_LEN {
            return Err(ProtoError::FileChunkTooLarge {
                len: chunk.data.len(),
                max: Self::MAX_DATA_LEN,
            });
        }
        Ok(chunk)
    }
}

impl crate::Message for FileChunk {
    fn kind(&self) -> u8 {
        frame::kind::FILE_CHUNK
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 信用归还（docs/02 §4.6 防护规则 5：初始 16 块，每 ack 归还一块）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileAck {
    /// 归还信用的块序号。
    pub seq: u32,
}

impl FileAck {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for FileAck {
    fn kind(&self) -> u8 {
        frame::kind::FILE_ACK
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 条目传输结束（docs/02 §4.6）：接收端 blake3 校验，失败按条目级错误处理
/// （可重试——与剪贴板"不重传"语义相反，用户在粘贴处等待）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileEnd {
    /// 所属传输。
    pub transfer_id: u32,
    /// 结束的条目下标。
    pub entry: u32,
    /// 条目内容 BLAKE3 摘要（源端边流边算，docs/02 §4.6 防护规则 8）。
    pub blake3: [u8; 32],
}

impl FileEnd {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for FileEnd {
    fn kind(&self) -> u8 {
        frame::kind::FILE_END
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 文件传输错误（docs/02 §4.6 防护规则 6：条目级**可重试**，由接收端决策）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileError {
    /// 所属传输。
    pub transfer_id: u32,
    /// 失败条目（`None` = 整单失败，如清单过期）。
    pub entry: Option<u32>,
    /// 失败原因（未知值原样透传——major=1 内只增，docs/02 §4.6）。
    pub code: FileErrCode,
}

impl FileError {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for FileError {
    fn kind(&self) -> u8 {
        frame::kind::FILE_ERROR
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 用户取消（docs/02 §4.6；接收端/源端均可发，对端终止该 transfer）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileCancel {
    /// 取消的传输。
    pub transfer_id: u32,
}

impl FileCancel {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for FileCancel {
    fn kind(&self) -> u8 {
        frame::kind::FILE_CANCEL
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 目录子树展开（docs/02 §4.6 防护规则 2）：子清单条目自 `base` 起在全局下标
/// 空间**连续分配**；每批 ≤ [`FileRefAnnounce::MAX_ENTRIES`]（同单帧承载约束）。
///
/// 展开仅允许由 `FilePullReq{entry = Dir 下标}` 触发；`base` 必须等于源端当前
/// 下标空间上界（接收端以此校验下标连续性，连接态执行）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileExpand {
    /// 所属传输。
    pub transfer_id: u32,
    /// 被展开的目录条目下标。
    pub parent: u32,
    /// 本批首条目的全局下标（= 源端当前下标空间上界）。
    pub base: u32,
    /// 子清单条目（≤ [`FileRefAnnounce::MAX_ENTRIES`]，校验同 announce）。
    pub entries: Vec<FileEntry>,
}

impl FileExpand {
    /// payload 编码（postcard），追加到 `out`。
    ///
    /// 编码期校验同 [`FileRefAnnounce::encode_payload`]（单帧承载 + 条目合法性）。
    ///
    /// # Errors
    /// [`ProtoError::FileEntriesExceeded`] / [`ProtoError::FileNameInvalid`] /
    /// [`ProtoError::PayloadTooLarge`] / [`ProtoError::Postcard`]。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        validate_entries(&self.entries)?;
        let mut scratch = Vec::new();
        frame::encode_postcard_into(self, &mut scratch)?;
        if scratch.len() > FileRefAnnounce::MAX_ENCODED_LEN {
            return Err(ProtoError::PayloadTooLarge {
                len: scratch.len(),
                max: FileRefAnnounce::MAX_ENCODED_LEN,
            });
        }
        out.extend_from_slice(&scratch);
        Ok(())
    }

    /// 解码并校验条目（条目数 ≤ 512 + 名称合法性）。
    ///
    /// # Errors
    /// [`ProtoError::FileEntriesExceeded`] / [`ProtoError::FileNameInvalid`] /
    /// [`ProtoError::Postcard`]。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        let expand: Self = frame::decode_postcard(payload)?;
        validate_entries(&expand.entries)?;
        Ok(expand)
    }
}

impl crate::Message for FileExpand {
    fn kind(&self) -> u8 {
        frame::kind::FILE_EXPAND
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// 文件域错误码（docs/02 §4.6 wire 钉死；**未知值原样透传**——major=1 内只增，
/// 同 [`crate::ctrl::ErrCode`] 纪律）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileErrCode(pub u8);

impl FileErrCode {
    /// 条目不存在（复制后被删除）。
    pub const NOT_FOUND: Self = Self(1);
    /// 权限不足。
    pub const PERMISSION_DENIED: Self = Self(2);
    /// 源不可达（源机离线/清单过期）。
    pub const SOURCE_GONE: Self = Self(3);
    /// 大小不符（拉取时 re-stat 与清单不一致）。
    pub const SIZE_MISMATCH: Self = Self(4);
    /// blake3 校验失败。
    pub const HASH_MISMATCH: Self = Self(5);
    /// 用户取消。
    pub const CANCELLED: Self = Self(6);
    /// 配额（接收端磁盘/阈值拒绝）。
    pub const QUOTA: Self = Self(7);
    /// 清单条目超限。
    pub const TOO_MANY_ENTRIES: Self = Self(8);
}

/// kind+payload → 文件域消息（统一分派入口，文件连接专用）。
///
/// - kind 不在文件表（`0x58–0x60`）→ `Ok(None)`（调用方按 docs/02 §4.1 跳过）；
/// - 各消息解码执行接收端校验（信任边界在接收端，docs/02 §4.6 防护规则）。
///
/// # Errors
/// postcard 解码失败 / 传输约束违规（[`crate::ProtoError`] 各文件域变体）。
pub fn decode_file(kind: u8, payload: &[u8]) -> Result<Option<FileMsg>, ProtoError> {
    if !frame::is_file_kind(kind) {
        return Ok(None);
    }
    let msg = match kind {
        frame::kind::FILE_BIND => FileMsg::Bind(frame::decode_postcard::<FileBind>(payload)?),
        frame::kind::FILE_REF_ANNOUNCE => {
            FileMsg::RefAnnounce(FileRefAnnounce::decode(payload)?)
        }
        frame::kind::FILE_PULL_REQ => {
            FileMsg::PullReq(frame::decode_postcard::<FilePullReq>(payload)?)
        }
        frame::kind::FILE_CHUNK => FileMsg::Chunk(FileChunk::decode(payload)?),
        frame::kind::FILE_ACK => FileMsg::Ack(frame::decode_postcard::<FileAck>(payload)?),
        frame::kind::FILE_END => FileMsg::End(frame::decode_postcard::<FileEnd>(payload)?),
        frame::kind::FILE_ERROR => FileMsg::Error(frame::decode_postcard::<FileError>(payload)?),
        frame::kind::FILE_CANCEL => FileMsg::Cancel(frame::decode_postcard::<FileCancel>(payload)?),
        frame::kind::FILE_EXPAND => FileMsg::Expand(FileExpand::decode(payload)?),
        // is_file_kind 已排除未知 kind
        _ => return Ok(None),
    };
    Ok(Some(msg))
}

/// 文件域消息统一枚举（[`decode_file`] 的返回载体；变体与 kind 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileMsg {
    /// 连接绑定（[`FileBind`]，kind 0x58）。
    Bind(FileBind),
    /// 元数据清单（[`FileRefAnnounce`]，kind 0x59）。
    RefAnnounce(FileRefAnnounce),
    /// 拉取请求（[`FilePullReq`]，kind 0x5A）。
    PullReq(FilePullReq),
    /// 数据块（[`FileChunk`]，kind 0x5B）。
    Chunk(FileChunk),
    /// 信用归还（[`FileAck`]，kind 0x5C）。
    Ack(FileAck),
    /// 条目结束（[`FileEnd`]，kind 0x5D）。
    End(FileEnd),
    /// 错误（[`FileError`]，kind 0x5E）。
    Error(FileError),
    /// 取消（[`FileCancel`]，kind 0x5F）。
    Cancel(FileCancel),
    /// 子树展开（[`FileExpand`]，kind 0x60）。
    Expand(FileExpand),
}

impl FileMsg {
    /// 按变体分发到对应消息的 [`Message`](crate::Message) 实现（编码侧便捷入口）。
    #[must_use]
    pub fn as_message(&self) -> &dyn crate::Message {
        match self {
            Self::Bind(m) => m,
            Self::RefAnnounce(m) => m,
            Self::PullReq(m) => m,
            Self::Chunk(m) => m,
            Self::Ack(m) => m,
            Self::End(m) => m,
            Self::Error(m) => m,
            Self::Cancel(m) => m,
            Self::Expand(m) => m,
        }
    }
}

/// 清单结构校验（条目数 + 名称），announce/expand 编码解码共用。
fn validate_entries(entries: &[FileEntry]) -> Result<(), ProtoError> {
    if entries.len() > FileRefAnnounce::MAX_ENTRIES {
        return Err(ProtoError::FileEntriesExceeded {
            count: entries.len(),
            max: FileRefAnnounce::MAX_ENTRIES,
        });
    }
    for entry in entries {
        entry.validate_name()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性样本。
    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            size: 1024,
            mtime_ms: 1_700_000_000_000,
            kind: FileEntryKind::File,
            children: 0,
        }
    }

    fn dir(name: &str, children: u32) -> FileEntry {
        FileEntry {
            kind: FileEntryKind::Dir,
            children,
            ..entry(name)
        }
    }

    fn announce() -> FileRefAnnounce {
        FileRefAnnounce {
            transfer_id: 7,
            origin_id: [0x22; 16],
            origin_seq: 42,
            entries: vec![entry("a.txt"), dir("docs", 3), entry("b.png")],
            total_bytes: 2048,
            copied_at_ms: 1_700_000_000_000,
        }
    }

    /// 九条消息全 round-trip + kind 映射 + decode_file 分派。
    #[test]
    fn roundtrip_and_dispatch() {
        let samples = [
            (
                frame::kind::FILE_BIND,
                FileMsg::Bind(FileBind {
                    session_id: [0x11; 16],
                }),
            ),
            (frame::kind::FILE_REF_ANNOUNCE, FileMsg::RefAnnounce(announce())),
            (
                frame::kind::FILE_PULL_REQ,
                FileMsg::PullReq(FilePullReq {
                    transfer_id: 7,
                    entry: 2,
                    offset: 4096,
                }),
            ),
            (
                frame::kind::FILE_CHUNK,
                FileMsg::Chunk(FileChunk {
                    transfer_id: 7,
                    seq: 3,
                    data: b"file payload".to_vec(),
                }),
            ),
            (frame::kind::FILE_ACK, FileMsg::Ack(FileAck { seq: 3 })),
            (
                frame::kind::FILE_END,
                FileMsg::End(FileEnd {
                    transfer_id: 7,
                    entry: 2,
                    blake3: [0x33; 32],
                }),
            ),
            (
                frame::kind::FILE_ERROR,
                FileMsg::Error(FileError {
                    transfer_id: 7,
                    entry: Some(2),
                    code: FileErrCode::NOT_FOUND,
                }),
            ),
            (
                frame::kind::FILE_CANCEL,
                FileMsg::Cancel(FileCancel { transfer_id: 7 }),
            ),
            (
                frame::kind::FILE_EXPAND,
                FileMsg::Expand(FileExpand {
                    transfer_id: 7,
                    parent: 1,
                    base: 3,
                    entries: vec![entry("nested.txt")],
                }),
            ),
        ];
        for (kind, msg) in samples {
            let mut payload = Vec::new();
            msg.as_message().encode_payload(&mut payload).unwrap();
            let decoded = decode_file(kind, &payload).unwrap().unwrap();
            assert_eq!(decoded, msg, "kind 0x{kind:02X}");
        }
    }

    /// 非文件域 kind → None（含剪贴板/控制 kind 与保留空档）。
    #[test]
    fn unknown_kind_is_none() {
        let payload = [0u8; 4];
        for kind in [
            frame::kind::KEY,
            frame::kind::CTRL_PING,
            frame::kind::CLIP_ANNOUNCE,
            0x00,
            0x55,
            0x57,
            0x61,
            0xFF,
        ] {
            assert_eq!(decode_file(kind, &payload).unwrap(), None);
        }
    }

    /// 条目数上限：512 恰好通过、513 拒绝（编码与解码两侧一致）。
    #[test]
    fn entries_limit() {
        let many: Vec<FileEntry> = (0..FileRefAnnounce::MAX_ENTRIES)
            .map(|i| entry(&format!("f{i:04}.txt")))
            .collect();
        let mut ok = announce();
        ok.entries = many;
        let mut payload = Vec::new();
        ok.encode_payload(&mut payload).unwrap();
        assert!(FileRefAnnounce::decode(&payload).is_ok());

        // 解码侧：postcard 构造 513 条目（绕过编码校验，模拟恶意流）
        let over = FileRefAnnounce {
            entries: (0..=FileRefAnnounce::MAX_ENTRIES)
                .map(|i| entry(&format!("f{i:04}.txt")))
                .collect(),
            ..announce()
        };
        let mut payload = Vec::new();
        frame::encode_postcard_into(&over, &mut payload).unwrap();
        let err = FileRefAnnounce::decode(&payload).unwrap_err();
        assert_eq!(
            err,
            ProtoError::FileEntriesExceeded {
                count: FileRefAnnounce::MAX_ENTRIES + 1,
                max: FileRefAnnounce::MAX_ENTRIES
            }
        );

        // 编码侧：同样拒绝（发送端自律 + fail-fast）
        let mut out = Vec::new();
        assert!(over.encode_payload(&mut out).is_err());
    }

    /// 名称合法性：含 `/`、`.`/`..`、NUL、超 256B、空名 → FileNameInvalid。
    #[test]
    fn name_validation() {
        let bad_names = ["a/b.txt", ".", "..", "a\0b", "", &"x".repeat(257)];
        for name in bad_names {
            let bad = FileRefAnnounce {
                entries: vec![entry(name)],
                ..announce()
            };
            let mut payload = Vec::new();
            frame::encode_postcard_into(&bad, &mut payload).unwrap();
            assert!(
                matches!(
                    FileRefAnnounce::decode(&payload),
                    Err(ProtoError::FileNameInvalid { max: 256 })
                ),
                "名 {name:?} 应拒绝"
            );
            let mut out = Vec::new();
            assert!(bad.encode_payload(&mut out).is_err(), "名 {name:?} 编码期拒绝");
        }
        // 恰好 256B 合法
        let ok = FileRefAnnounce {
            entries: vec![entry(&"n".repeat(256))],
            ..announce()
        };
        let mut payload = Vec::new();
        ok.encode_payload(&mut payload).unwrap();
        assert!(FileRefAnnounce::decode(&payload).is_ok());
    }

    /// total_bytes 上限：默认 2GiB + 自定义协商上限（decode_with_limit）。
    #[test]
    fn total_bytes_limit() {
        let mut payload = Vec::new();
        announce().encode_payload(&mut payload).unwrap();
        assert!(FileRefAnnounce::decode(&payload).is_ok());

        let at_limit = FileRefAnnounce {
            total_bytes: FileRefAnnounce::DEFAULT_MAX_TOTAL_BYTES,
            ..announce()
        };
        let mut payload = Vec::new();
        at_limit.encode_payload(&mut payload).unwrap();
        assert!(FileRefAnnounce::decode(&payload).is_ok());

        let over = FileRefAnnounce {
            total_bytes: FileRefAnnounce::DEFAULT_MAX_TOTAL_BYTES + 1,
            ..announce()
        };
        let mut payload = Vec::new();
        frame::encode_postcard_into(&over, &mut payload).unwrap();
        let err = FileRefAnnounce::decode(&payload).unwrap_err();
        assert_eq!(
            err,
            ProtoError::FileTotalBytesExceeded {
                total_bytes: FileRefAnnounce::DEFAULT_MAX_TOTAL_BYTES + 1,
                max: FileRefAnnounce::DEFAULT_MAX_TOTAL_BYTES
            }
        );

        // 协商收紧（如接收端磁盘余量小）
        let mut payload = Vec::new();
        announce().encode_payload(&mut payload).unwrap();
        let err = FileRefAnnounce::decode_with_limit(&payload, 1024).unwrap_err();
        assert!(matches!(err, ProtoError::FileTotalBytesExceeded { .. }));
    }

    /// 清单单帧承载：合法上限字段的 512×长名清单编码期拒绝（物理上限先行）。
    #[test]
    fn manifest_single_frame_budget() {
        let mut big = announce();
        big.entries = (0..FileRefAnnounce::MAX_ENTRIES)
            .map(|i| entry(&format!("{:0190}{}", i, "n".repeat(60))))
            .collect(); // 每条 ~250B 名 → 总编码远超 65512B
        let mut out = Vec::new();
        let err = big.encode_payload(&mut out).unwrap_err();
        assert!(
            matches!(err, ProtoError::PayloadTooLarge { max: FileRefAnnounce::MAX_ENCODED_LEN, .. }),
            "超预算应编码期拒绝：{err}"
        );
        // 常规清单远小于预算
        let mut out = Vec::new();
        announce().encode_payload(&mut out).unwrap();
        assert!(out.len() < FileRefAnnounce::MAX_ENCODED_LEN);
    }

    /// FileChunk：64KiB 单块上限（边界值 + 超限负例）。
    #[test]
    fn chunk_data_limit() {
        let at_limit = FileChunk {
            transfer_id: 1,
            seq: 9,
            data: vec![0u8; FileChunk::MAX_DATA_LEN],
        };
        let mut payload = Vec::new();
        at_limit.encode_payload(&mut payload).unwrap();
        let decoded = FileChunk::decode(&payload).unwrap();
        assert_eq!(decoded.data.len(), FileChunk::MAX_DATA_LEN);

        let over = FileChunk {
            data: vec![0u8; FileChunk::MAX_DATA_LEN + 1],
            transfer_id: 1,
            seq: 9,
        };
        let mut payload = Vec::new();
        frame::encode_postcard_into(&over, &mut payload).unwrap();
        assert_eq!(
            FileChunk::decode(&payload).unwrap_err(),
            ProtoError::FileChunkTooLarge {
                len: FileChunk::MAX_DATA_LEN + 1,
                max: FileChunk::MAX_DATA_LEN
            }
        );
        assert_eq!(FileChunk::MAX_DATA_LEN, 64 * 1024);
    }

    /// FileExpand：条目校验同 announce（超限 + 坏名）。
    #[test]
    fn expand_validation() {
        let over = FileExpand {
            transfer_id: 1,
            parent: 0,
            base: 3,
            entries: (0..=FileRefAnnounce::MAX_ENTRIES)
                .map(|i| entry(&format!("f{i:04}")))
                .collect(),
        };
        let mut payload = Vec::new();
        frame::encode_postcard_into(&over, &mut payload).unwrap();
        assert!(matches!(
            FileExpand::decode(&payload),
            Err(ProtoError::FileEntriesExceeded { .. })
        ));

        let bad = FileExpand {
            entries: vec![entry("../escape")],
            transfer_id: 1,
            parent: 0,
            base: 3,
        };
        let mut payload = Vec::new();
        frame::encode_postcard_into(&bad, &mut payload).unwrap();
        assert!(matches!(
            FileExpand::decode(&payload),
            Err(ProtoError::FileNameInvalid { .. })
        ));
    }

    /// FileErrCode 未知值原样透传（major=1 内只增，docs/02 §4.6）。
    #[test]
    fn err_code_passthrough() {
        let unknown = FileError {
            transfer_id: 1,
            entry: None,
            code: FileErrCode(0xDD),
        };
        let mut payload = Vec::new();
        unknown.encode_payload(&mut payload).unwrap();
        let decoded = decode_file(frame::kind::FILE_ERROR, &payload)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, FileMsg::Error(unknown));
    }

    /// 恶意 payload：垃圾 / 截断 / 残留 → 报错不 panic。
    #[test]
    fn malformed_payloads() {
        for payload in [&b""[..], &[0xFF, 0xFF, 0xFF][..], &[0x00, 0x01][..]] {
            for kind in [
                frame::kind::FILE_BIND,
                frame::kind::FILE_REF_ANNOUNCE,
                frame::kind::FILE_PULL_REQ,
                frame::kind::FILE_CHUNK,
                frame::kind::FILE_ACK,
                frame::kind::FILE_END,
                frame::kind::FILE_ERROR,
                frame::kind::FILE_CANCEL,
                frame::kind::FILE_EXPAND,
            ] {
                assert!(
                    decode_file(kind, payload).is_err(),
                    "kind 0x{kind:02X} payload {payload:?} 应报错"
                );
            }
        }
        // 尾部残留
        let mut payload = Vec::new();
        let ack = FileAck { seq: 1 };
        frame::encode_postcard_into(&ack, &mut payload).unwrap();
        payload.push(0);
        assert!(decode_file(frame::kind::FILE_ACK, &payload).is_err());
    }

    /// 帧级完整往返。
    #[test]
    fn frame_roundtrip() {
        let msg = FileMsg::RefAnnounce(announce());
        let bytes = msg.as_message().encode_frame(5).unwrap();
        let frame = frame::decode_frame(&bytes).unwrap();
        assert_eq!(frame.header.seq, 5);
        assert_eq!(frame.header.kind, frame::kind::FILE_REF_ANNOUNCE);
        let decoded = decode_file(frame.header.kind, frame.payload)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, msg);
    }
}
