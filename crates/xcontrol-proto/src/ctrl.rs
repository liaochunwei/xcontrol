//! 控制消息（docs/02 §4.3）：postcard 编码的 [`CtrlMsg`] 枚举，
//! 以及 [`ProtoVer`]（re-export）/ [`Caps`] / [`ScreenInfo`] / [`Options`] / [`ErrCode`]。
//!
//! wire 契约（docs/02 §4.3）：
//! - 每条控制消息独立占用一个帧 kind（[`crate::kind`] `0x40–0x48`），
//!   postcard 枚举判别值作为第二层类型标签——kind 与判别值不符 = 协议违规
//!   （[`crate::ProtoError::KindMismatch`]）；
//! - 枚举**变体顺序即 wire 判别值顺序**（postcard 外部标签格式），只增不改；
//! - 结构体字段顺序即 wire 字段顺序，与 docs/02 §4.3 伪代码一致。

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;
use crate::frame;
use crate::input::ScreenId;
use crate::version::ProtoVer;

/// 能力位掩码（docs/05 §0.1；`u32`，bit0-4 已定义，major=1 内只增）。
///
/// 接收方**必须容忍未知 bit**（新 minor 引入的位按 0 处理，docs/02 §5.3）；
/// 发送端只发对端掩码声明过的能力（如剪贴板格式协商，docs/04 §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
pub struct Caps(u32);

impl Caps {
    /// Unicode 文本直发（bit0，docs/05 §0.1）：可注入 `Text` 事件。
    pub const UNICODE: Self = Self(1 << 0);
    /// 剪贴板图片 `image/png`（bit1，docs/04 §2）。
    pub const IMAGE: Self = Self(1 << 1);
    /// 剪贴板 HTML `text/html`（bit2，docs/04 §2）。
    pub const HTML: Self = Self(1 << 2);
    /// 绝对指针映射（bit3，docs/05 §0.1）：可接收 `MouseMoveAbs`。
    pub const ABS_POINTER: Self = Self(1 << 3);
    /// 文件传输（bit4，docs/05 §0.1，FR-7.3）：文件通道（docs/02 §4.6）可建立；
    /// 对端未声明时不发送 `FileRefAnnounce`。
    pub const FILETRANSFER: Self = Self(1 << 4);

    /// 已定义位掩码（bit0-4）；未知位用 [`Caps::known`] 过滤。
    pub const KNOWN: Self = Self(
        Self::UNICODE.0 | Self::IMAGE.0 | Self::HTML.0 | Self::ABS_POINTER.0 | Self::FILETRANSFER.0,
    );

    /// 原始位值。
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 从原始位值构造（未知位原样保留——对端可能来自更高 minor）。
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// 过滤掉未定义位（对未知位必须容忍的默认路径是 [`Caps::bits`] 直用）。
    #[must_use]
    pub const fn known(self) -> Self {
        Self(self.0 & Self::KNOWN.0)
    }

    /// 包含 `other` 的全部位。
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 取并集。
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl core::ops::BitOr for Caps {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// 屏幕条目（docs/02 §4.3 `Layout`；布局图邻居见 docs/03 §5，几何精确对齐为 P2）。
///
/// `id` 属**主控分配的全局 ScreenId 空间**（docs/02 §4.2「ScreenId 跨机语义」）；
/// `owner` 为从机侧「全局 id → 本机屏」映射表的归属判定字段（1.1 起）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenInfo {
    /// 屏幕标识（与输入事件的 `screen`/`target` 同域；主控全局空间）。
    pub id: ScreenId,
    /// 该屏所属机器的 instance id（公开值，`Hello.instance_id` 同源；主控本机屏
    /// = 主控 id，从机屏 = 该从机 id）。
    pub owner: [u8; 16],
    /// 显示名。
    pub name: String,
    /// 左邻居屏幕（`None` = 该方向无边）。
    pub left: Option<ScreenId>,
    /// 右邻居屏幕。
    pub right: Option<ScreenId>,
    /// 上邻居屏幕。
    pub top: Option<ScreenId>,
    /// 下邻居屏幕。
    pub bottom: Option<ScreenId>,
    /// 缩放提示：逻辑/物理像素比（1.0 = 无缩放）。
    pub scale: f32,
}

/// 连接参数（docs/02 §4.3 `SetOptions`；默认值对齐 §4.5 心跳表与 FR-4.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Options {
    /// 心跳周期 ms（默认 3000 = 3s）。
    pub ping_interval_ms: u32,
    /// 判死阈值 ms（默认 9000 = 9s 无任何帧即断开）。
    pub dead_after_ms: u32,
    /// 剪贴板单次传输上限字节（默认 16MiB，FR-4.5 可配；见 [`crate::clip::ClipAnnounce`]）。
    pub clipboard_max_bytes: u32,
}

impl Options {
    /// 心跳周期默认值（docs/02 §4.5）。
    pub const DEFAULT_PING_INTERVAL_MS: u32 = 3000;
    /// 判死阈值默认值（docs/02 §4.5）。
    pub const DEFAULT_DEAD_AFTER_MS: u32 = 9000;
    /// 剪贴板上限默认值（docs/02 §6：16MiB）。
    pub const DEFAULT_CLIPBOARD_MAX_BYTES: u32 = 16 * 1024 * 1024;

    /// 文档默认值（对端未协商 `SetOptions` 前的本地假设）。
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            ping_interval_ms: Self::DEFAULT_PING_INTERVAL_MS,
            dead_after_ms: Self::DEFAULT_DEAD_AFTER_MS,
            clipboard_max_bytes: Self::DEFAULT_CLIPBOARD_MAX_BYTES,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::defaults()
    }
}

/// 协议错误码（docs/02 §7；`u8` 新类型——major=1 内只增，未知码原样透传不拒收）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErrCode(pub u8);

impl ErrCode {
    /// 0x01 版本不兼容。
    pub const UNSUPPORTED_VERSION: Self = Self(0x01);
    /// 0x02 安全码错误（仅控制层语义错误；Noise 握手失败不携带此码，docs/02 §7）。
    pub const BAD_SECURITY_CODE: Self = Self(0x02);
    /// 0x03 能力不满足（如要求绝对映射而对端不支持）。
    pub const CAPABILITY_MISSING: Self = Self(0x03);
    /// 0x04 协议违规（帧越界、未知通道等）。
    pub const PROTOCOL_VIOLATION: Self = Self(0x04);
    /// 0x05 服务端满载（超过 FR-1.2 上限）。
    pub const SERVER_FULL: Self = Self(0x05);

    /// 原始码值。
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }
}

/// 控制消息全量（docs/02 §4.3，postcard 编码；**变体顺序 = wire 判别值**）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CtrlMsg {
    /// 主控→从机第一条（握手成功后；docs/02 §5.1）。
    Hello {
        /// 主控协议版本。
        proto: ProtoVer,
        /// 主控显示名。
        name: String,
        /// 主控屏幕显示名。
        screen_name: String,
        /// 实例 UUID（16B；发现层通告的同一 id）。
        instance_id: [u8; 16],
        /// 能力掩码（docs/05 §0.1）。
        caps: Caps,
        /// PSK 派生盐（16B，docs/02 §3.1；`BLAKE3-derive-key` 的 instance_uuid 派生值）。
        salt: [u8; 16],
    },
    /// 从机→主机应答（docs/02 §5.2）。
    HelloBack {
        /// 从机协议版本。
        proto: ProtoVer,
        /// 从机显示名。
        name: String,
        /// 实例 UUID。
        instance_id: [u8; 16],
        /// 能力掩码。
        caps: Caps,
    },
    /// 主控下发屏幕布局（名称/邻居图/缩放提示，docs/03 §5）。
    Layout {
        /// 屏幕条目（含主控屏）。
        screens: Vec<ScreenInfo>,
    },
    /// 连接参数（心跳周期等）。
    SetOptions {
        /// 参数集。
        options: Options,
    },
    /// 协议层错误（**随后断开**，docs/02 §4.3）。
    Error {
        /// 错误码（docs/02 §7）。
        code: ErrCode,
        /// 人类可读说明（不得含安全码/剪贴板内容，error-handling.md）。
        msg: String,
    },
    /// 心跳（3s 周期；9s 无任何消息判死，docs/02 §4.5）。
    Ping {
        /// 发送时刻毫秒（对端回显即可）。
        ts_ms: u64,
    },
    /// 心跳应答。
    Pong {
        /// 回显的 [`CtrlMsg::Ping`] 时间戳。
        ts_ms: u64,
    },
    /// 共享暂停（FR-2.8）。
    Pause,
    /// 共享恢复。
    Resume,
}

impl CtrlMsg {
    /// 对应 docs/02 §4.3 kind 分配表（`0x40–0x48`；变体 ↔ kind 集中于此）。
    #[must_use]
    pub const fn kind(&self) -> u8 {
        match self {
            Self::Hello { .. } => frame::kind::CTRL_HELLO,
            Self::HelloBack { .. } => frame::kind::CTRL_HELLO_BACK,
            Self::Layout { .. } => frame::kind::CTRL_LAYOUT,
            Self::SetOptions { .. } => frame::kind::CTRL_SET_OPTIONS,
            Self::Error { .. } => frame::kind::CTRL_ERROR,
            Self::Ping { .. } => frame::kind::CTRL_PING,
            Self::Pong { .. } => frame::kind::CTRL_PONG,
            Self::Pause => frame::kind::CTRL_PAUSE,
            Self::Resume => frame::kind::CTRL_RESUME,
        }
    }

    /// payload 编码（postcard，含枚举判别字节），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::Postcard`]：序列化失败（理论上仅超长字符串等极端输入）。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        frame::encode_postcard_into(self, out)
    }
}

impl crate::Message for CtrlMsg {
    fn kind(&self) -> u8 {
        self.kind()
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

/// kind+payload → 控制消息。
///
/// - kind 不在控制消息表（`0x40–0x48`）→ `Ok(None)`（调用方按 docs/02 §4.1 跳过）；
/// - postcard 判别值与帧 kind 不符 → [`ProtoError::KindMismatch`]（协议违规）；
/// - postcard 解码失败 → [`ProtoError::Postcard`]（严格消费全部 payload 字节）。
///
/// # Errors
/// 见函数说明。
pub fn decode_ctrl(kind: u8, payload: &[u8]) -> Result<Option<CtrlMsg>, ProtoError> {
    if !frame::is_ctrl_kind(kind) {
        return Ok(None);
    }
    let msg: CtrlMsg = frame::decode_postcard(payload)?;
    let expected = msg.kind();
    if expected != kind {
        return Err(ProtoError::KindMismatch { kind, expected });
    }
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use crate::xkey::CK_VOLUME_UP;

    /// 确定性样本（fixture 与 round-trip 共用）。
    pub(crate) fn samples() -> Vec<CtrlMsg> {
        vec![
            CtrlMsg::Hello {
                proto: ProtoVer::CURRENT,
                name: "host".to_owned(),
                screen_name: "desk".to_owned(),
                instance_id: [0xAB; 16],
                caps: Caps::UNICODE | Caps::IMAGE | Caps::ABS_POINTER,
                salt: [0x5A; 16],
            },
            CtrlMsg::HelloBack {
                proto: ProtoVer { major: 1, minor: 1 },
                name: "client".to_owned(),
                instance_id: [0xCD; 16],
                caps: Caps::UNICODE,
            },
            CtrlMsg::Layout {
                screens: vec![
                    ScreenInfo {
                        id: 0,
                        owner: [0xAA; 16],
                        name: "primary".to_owned(),
                        left: None,
                        right: Some(1),
                        top: None,
                        bottom: None,
                        scale: 2.0,
                    },
                    ScreenInfo {
                        id: 1,
                        owner: [0xBB; 16],
                        name: "lap".to_owned(),
                        left: Some(0),
                        right: None,
                        top: None,
                        bottom: None,
                        scale: 1.0,
                    },
                ],
            },
            CtrlMsg::SetOptions {
                options: Options::defaults(),
            },
            CtrlMsg::Error {
                code: ErrCode::UNSUPPORTED_VERSION,
                msg: "peer major mismatch".to_owned(),
            },
            CtrlMsg::Ping { ts_ms: 1_234_567 },
            CtrlMsg::Pong { ts_ms: 1_234_567 },
            CtrlMsg::Pause,
            CtrlMsg::Resume,
        ]
    }

    /// 全变体 round-trip + kind 映射（docs/02 §4.3 kind 分配表）。
    #[test]
    fn roundtrip_and_kind_mapping() {
        let expected_kinds = [
            frame::kind::CTRL_HELLO,
            frame::kind::CTRL_HELLO_BACK,
            frame::kind::CTRL_LAYOUT,
            frame::kind::CTRL_SET_OPTIONS,
            frame::kind::CTRL_ERROR,
            frame::kind::CTRL_PING,
            frame::kind::CTRL_PONG,
            frame::kind::CTRL_PAUSE,
            frame::kind::CTRL_RESUME,
        ];
        for (msg, want_kind) in samples().into_iter().zip(expected_kinds) {
            assert_eq!(msg.kind(), want_kind);
            let mut payload = Vec::new();
            msg.encode_payload(&mut payload).unwrap();
            let decoded = decode_ctrl(want_kind, &payload).unwrap().unwrap();
            assert_eq!(decoded, msg);
        }
    }

    /// 非控制 kind → None；判别值/kind 错配 → KindMismatch。
    #[test]
    fn kind_mismatch() {
        let payload = {
            let mut p = Vec::new();
            CtrlMsg::Pause.encode_payload(&mut p).unwrap();
            p
        };
        // 非控制 kind
        for kind in [frame::kind::KEY, frame::kind::CLIP_CHUNK, 0x00, 0xFF] {
            assert_eq!(decode_ctrl(kind, &payload).unwrap(), None);
        }
        // Pause 的 payload 配了 Pong 的 kind：postcard 可解但类型不符
        let err = decode_ctrl(frame::kind::CTRL_PONG, &payload).unwrap_err();
        assert_eq!(
            err,
            ProtoError::KindMismatch {
                kind: frame::kind::CTRL_PONG,
                expected: frame::kind::CTRL_PAUSE
            }
        );
    }

    /// 帧级完整往返（encode_frame → decode_frame → decode_ctrl）。
    #[test]
    fn frame_roundtrip() {
        let msg = CtrlMsg::SetOptions {
            options: Options {
                ping_interval_ms: 5000,
                dead_after_ms: 15000,
                clipboard_max_bytes: 1024,
            },
        };
        let bytes = msg.encode_frame(9).unwrap();
        let frame = frame::decode_frame(&bytes).unwrap();
        assert_eq!(frame.header.seq, 9);
        let decoded = decode_ctrl(frame.header.kind, frame.payload)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, msg);
    }

    /// 恶意 payload：垃圾字节 / 空输入 / 尾部残留 → 报错不 panic。
    #[test]
    fn malformed_payloads() {
        for payload in [&b""[..], &[0xFF, 0xFF][..], &[0x00, 0x01, 0x02][..]] {
            assert!(
                decode_ctrl(frame::kind::CTRL_PING, payload).is_err(),
                "payload {payload:?} 应报错"
            );
        }
        // Ping 判别值 5 + ts varint，尾部残留拒绝
        let mut payload = Vec::new();
        CtrlMsg::Ping { ts_ms: 1 }
            .encode_payload(&mut payload)
            .unwrap();
        payload.push(0x00);
        assert!(decode_ctrl(frame::kind::CTRL_PING, &payload).is_err());
    }

    /// Caps：位定义（docs/05 §0.1）与容忍未知位语义。
    #[test]
    fn caps_bits() {
        assert_eq!(Caps::UNICODE.bits(), 1 << 0);
        assert_eq!(Caps::IMAGE.bits(), 1 << 1);
        assert_eq!(Caps::HTML.bits(), 1 << 2);
        assert_eq!(Caps::ABS_POINTER.bits(), 1 << 3);
        assert_eq!(Caps::FILETRANSFER.bits(), 1 << 4);
        assert_eq!(Caps::KNOWN.bits(), 0b1_1111);
        assert!(Caps::default().bits() == 0);
        let all = Caps::UNICODE | Caps::IMAGE | Caps::HTML | Caps::ABS_POINTER | Caps::FILETRANSFER;
        assert!(all.contains(Caps::UNICODE));
        assert!(all.contains(Caps::FILETRANSFER));
        assert!(!all.contains(Caps::from_bits(1 << 9)));
        // 未知位保留 + known 过滤（docs/05 §0.1：接收方必须容忍未知 bit）
        let with_unknown = Caps::from_bits(all.bits() | (1 << 5) | (1 << 31));
        assert_eq!(with_unknown.bits(), all.bits() | (1 << 5) | (1 << 31));
        assert_eq!(with_unknown.known(), all);
        assert_eq!(with_unknown.union(Caps::HTML), with_unknown);
    }

    /// ErrCode：已知码锚点（docs/02 §7）+ 未知码透传。
    #[test]
    fn err_code_passthrough() {
        assert_eq!(ErrCode::UNSUPPORTED_VERSION.raw(), 0x01);
        assert_eq!(ErrCode::BAD_SECURITY_CODE.raw(), 0x02);
        assert_eq!(ErrCode::CAPABILITY_MISSING.raw(), 0x03);
        assert_eq!(ErrCode::PROTOCOL_VIOLATION.raw(), 0x04);
        assert_eq!(ErrCode::SERVER_FULL.raw(), 0x05);
        // 未知码 wire 透传（major=1 内只增，不得拒收）
        let msg = CtrlMsg::Error {
            code: ErrCode(0x99),
            msg: "future code".to_owned(),
        };
        let mut payload = Vec::new();
        msg.encode_payload(&mut payload).unwrap();
        let decoded = decode_ctrl(msg.kind(), &payload).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    /// Options 默认值对齐 docs/02 §4.5 / §6。
    #[test]
    fn options_defaults() {
        let options = Options::defaults();
        assert_eq!(options.ping_interval_ms, 3000);
        assert_eq!(options.dead_after_ms, 9000);
        assert_eq!(options.clipboard_max_bytes, 16 * 1024 * 1024);
        assert_eq!(Options::default(), options);
    }

    /// CtrlMsg 与输入 kind 互不串扰（decode 分派边界）。
    #[test]
    fn decode_does_not_cross_channels() {
        let mut payload = Vec::new();
        CtrlMsg::Ping { ts_ms: 0 }
            .encode_payload(&mut payload)
            .unwrap();
        // 输入通道收到控制 payload：输入解码返回 None（未知 kind），不误解析
        assert_eq!(
            crate::input::InputEvent::decode(frame::kind::CTRL_PING, &payload).unwrap(),
            None
        );
        // 消费者页键常量仍可用（防 import 漂移的哨兵）
        assert!(CK_VOLUME_UP.raw() >= 0x100);
    }
}
