//! 输入事件（docs/02 §4.2 kind 表，**定长手写、全部小端**——热路径不走通用序列化）。
//!
//! 布局逐字节对应 docs/02 §4.2 表格；语义模型见 docs/03 §3（冲突时以 docs/02 为准）。
//!
//! # 合帧规则（docs/02 §4.2）
//!
//! - 仅 `MouseMoveRel` 可在发送窗口内合并（delta 累加，最多 8 条/帧周期），
//!   见 [`RelMerger`]——**只提供合帧辅助，不做自动重排**；
//! - 键盘与按钮事件严禁合并或重排（因果顺序即注入顺序，docs/03 §3 顺序保证）。

use crate::error::ProtoError;
use crate::frame::{self, Frame};
use crate::xkey::XKey;

/// 屏幕标识（docs/02 §4.2 `target`/`screen` 字段，u16）。
pub type ScreenId = u16;

/// `Text` 事件 UTF-8 上限：512B/条，超长由主控分段（docs/02 §4.2 kind 0x07、docs/03 §4.5）。
pub const MAX_TEXT_LEN: usize = 512;

/// 按键状态（docs/02 §4.2：`0=down, 1=up`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyState {
    /// 按下（wire `0`）。
    Down,
    /// 抬起（wire `1`）。
    Up,
}

impl KeyState {
    const fn wire(self) -> u8 {
        match self {
            Self::Down => 0,
            Self::Up => 1,
        }
    }

    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Down),
            1 => Some(Self::Up),
            _ => None,
        }
    }
}

/// 鼠标按键（docs/02 §4.2 kind 0x05：`0=L, 1=M, 2=R, 3=Back, 4=Fwd`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// 左键（wire `0`）。
    Left,
    /// 中键（wire `1`）。
    Middle,
    /// 右键（wire `2`）。
    Right,
    /// 侧键-后退（wire `3`）。
    Back,
    /// 侧键-前进（wire `4`）。
    Forward,
}

impl MouseButton {
    const fn wire(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::Back => 3,
            Self::Forward => 4,
        }
    }

    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Left),
            1 => Some(Self::Middle),
            2 => Some(Self::Right),
            3 => Some(Self::Back),
            4 => Some(Self::Forward),
            _ => None,
        }
    }
}

/// 滚轮轴（docs/02 §4.2 kind 0x06：`0=V, 1=H`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    /// 垂直（wire `0`）。
    Vertical,
    /// 水平（wire `1`）。
    Horizontal,
}

impl Axis {
    const fn wire(self) -> u8 {
        match self {
            Self::Vertical => 0,
            Self::Horizontal => 1,
        }
    }

    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Vertical),
            1 => Some(Self::Horizontal),
            _ => None,
        }
    }
}

/// 屏幕边缘（docs/02 §4.2 kind 0x10：`0=L, 1=R, 2=T, 3=B`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenEdge {
    /// 左边缘（wire `0`）。
    Left,
    /// 右边缘（wire `1`）。
    Right,
    /// 上边缘（wire `2`）。
    Top,
    /// 下边缘（wire `3`）。
    Bottom,
}

impl ScreenEdge {
    const fn wire(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Top => 2,
            Self::Bottom => 3,
        }
    }

    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            2 => Some(Self::Top),
            3 => Some(Self::Bottom),
            _ => None,
        }
    }
}

/// 修饰键快照位掩码（docs/02 §4.2：bit0-7 = LCtrl/LShift/LAlt/LMeta/RCtrl/RShift/RAlt/RMeta，
/// 与 [`XKey`] 修饰键 `0xE0–0xE7` 一一对应）。
///
/// bit8-31 未定义：解码时原样保留（不判违规，宽容处理），但语义上恒为 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Mods(u32);

impl Mods {
    /// 左 Ctrl（bit0）。
    pub const LCTRL: Self = Self(1 << 0);
    /// 左 Shift（bit1）。
    pub const LSHIFT: Self = Self(1 << 1);
    /// 左 Alt（bit2）。
    pub const LALT: Self = Self(1 << 2);
    /// 左 Meta（bit3）。
    pub const LMETA: Self = Self(1 << 3);
    /// 右 Ctrl（bit4）。
    pub const RCTRL: Self = Self(1 << 4);
    /// 右 Shift（bit5）。
    pub const RSHIFT: Self = Self(1 << 5);
    /// 右 Alt（bit6）。
    pub const RALT: Self = Self(1 << 6);
    /// 右 Meta（bit7）。
    pub const RMETA: Self = Self(1 << 7);

    /// 原始位值。
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 从原始位值构造（未知位原样保留）。
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
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

    /// 修饰键 XKey → mods 位（docs/02 §4.2 bit0-7）；非修饰键返回 `None`。
    #[must_use]
    pub fn from_key(key: XKey) -> Option<Self> {
        key.mods_bit().map(|bit| Self(1 << bit))
    }

    /// mods 位序号（[`XKey::mods_bit`]）→ mods 位。
    #[must_use]
    pub const fn from_key_bit(bit: u8) -> Self {
        Self(1 << bit)
    }
}

impl core::ops::BitOr for Mods {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// 输入事件全量 12 种（docs/02 §4.2 kind 表 1:1；docs/03 §3 语义模型）。
///
/// 定长手写编解码（小端、不用 serde/postcard）；`Text` 为变长（≤512B）。
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// 按键按下/抬起（含修饰键本身；docs/02 §4.2 kind 0x01：`code u16 | state u8 | pad u8`）。
    Key {
        /// 抽象键码（保留区拒绝）。
        code: XKey,
        /// 按键状态。
        state: KeyState,
    },
    /// 长按重复（kind 0x02：`code u16 | pad u16`；从机可按平台能力忽略）。
    KeyRepeat {
        /// 抽象键码。
        code: XKey,
    },
    /// 相对指针移动（kind 0x03：`dx f32 | dy f32`；注入端以 f64 累加器取整）。
    MouseMoveRel {
        /// X 方向增量。
        dx: f32,
        /// Y 方向增量。
        dy: f32,
    },
    /// 绝对指针移动（kind 0x04：`screen u16 | x f32 | y f32`，目标屏内归一化 0.0–1.0）。
    MouseMoveAbs {
        /// 目标屏幕。
        screen: ScreenId,
        /// 归一化 X。
        x: f32,
        /// 归一化 Y。
        y: f32,
    },
    /// 鼠标按键（kind 0x05：`button u8 | state u8`）。
    MouseButton {
        /// 哪个键。
        button: MouseButton,
        /// 状态。
        state: KeyState,
    },
    /// 滚轮（kind 0x06：`axis u8 | delta f32`；delta 单位=格，保留小数）。
    Wheel {
        /// 滚动轴。
        axis: Axis,
        /// 格数（可含小数，触控板/高分滚轮）。
        delta: f32,
    },
    /// Unicode 文本直发（kind 0x07：`len u16 | utf8[..len]`，≤512B/条）。
    Text {
        /// 整段 UTF-8 文本（docs/03 §4.5）。
        utf8: String,
    },
    /// 切入屏幕（kind 0x10：`target u16 | edge u8 | ratio f32 | mods u32`）。
    EnterScreen {
        /// 目标屏幕。
        target: ScreenId,
        /// 命中的边缘。
        edge: ScreenEdge,
        /// 命中点在该边的归一化位置 0.0–1.0。
        ratio: f32,
        /// 修饰键快照（切入时对齐，防卡键）。
        mods: Mods,
        /// 光标保持原位（11-23）：1 = 被控端**不做落点定位**，光标留在当前
        /// 位置（热键/托盘切换路径——用户语义"I 切出时的位置"）；0 = 按
        /// edge/ratio 定位到进入点（边缘切换路径，空间连续必需）。
        /// 兼容：1.1 wire 追加字节；1.0 对端无此字段，双端需同版本部署。
        preserve_cursor: bool,
    },
    /// 切出屏幕（kind 0x11：`mods u32`；从机据此释放本机按键）。
    LeaveScreen {
        /// 离开时的修饰键状态。
        mods: Mods,
    },
    /// 修饰键快照对齐（kind 0x12：`mods u32`；周期 1s 与切入时全量对齐）。
    ModifierSync {
        /// 当前修饰键状态。
        mods: Mods,
    },
    /// 切换请求（kind 0x20：`target u16`；热键/托盘切换，FR-2.5，主控→目标从机）。
    SwitchRequest {
        /// 目标屏幕。
        target: ScreenId,
    },
    /// 切换确认（kind 0x21：`target u16 | ok u8`；从机确认可受控）。
    SwitchAck {
        /// 确认方屏幕。
        target: ScreenId,
        /// 是否可受控。
        ok: bool,
    },
}

impl InputEvent {
    /// 对应 docs/02 §4.2 kind 表中的帧 kind（InputEvent ↔ kind 转换集中于此）。
    #[must_use]
    pub const fn kind(&self) -> u8 {
        match self {
            Self::Key { .. } => frame::kind::KEY,
            Self::KeyRepeat { .. } => frame::kind::KEY_REPEAT,
            Self::MouseMoveRel { .. } => frame::kind::MOUSE_MOVE_REL,
            Self::MouseMoveAbs { .. } => frame::kind::MOUSE_MOVE_ABS,
            Self::MouseButton { .. } => frame::kind::MOUSE_BUTTON,
            Self::Wheel { .. } => frame::kind::WHEEL,
            Self::Text { .. } => frame::kind::TEXT,
            Self::EnterScreen { .. } => frame::kind::ENTER_SCREEN,
            Self::LeaveScreen { .. } => frame::kind::LEAVE_SCREEN,
            Self::ModifierSync { .. } => frame::kind::MODIFIER_SYNC,
            Self::SwitchRequest { .. } => frame::kind::SWITCH_REQUEST,
            Self::SwitchAck { .. } => frame::kind::SWITCH_ACK,
        }
    }

    /// payload 编码（小端，不含帧头），追加到 `out`。
    ///
    /// # Errors
    /// [`ProtoError::TextTooLong`]：`Text` 超过 [`MAX_TEXT_LEN`]（512B）。
    pub fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        match self {
            Self::Key { code, state } => {
                put_u16(out, code.raw());
                put_u8(out, state.wire());
                put_u8(out, 0); // pad
            }
            Self::KeyRepeat { code } => {
                put_u16(out, code.raw());
                put_u16(out, 0); // pad
            }
            Self::MouseMoveRel { dx, dy } => {
                put_f32(out, *dx);
                put_f32(out, *dy);
            }
            Self::MouseMoveAbs { screen, x, y } => {
                put_u16(out, *screen);
                put_f32(out, *x);
                put_f32(out, *y);
            }
            Self::MouseButton { button, state } => {
                put_u8(out, button.wire());
                put_u8(out, state.wire());
            }
            Self::Wheel { axis, delta } => {
                put_u8(out, axis.wire());
                put_f32(out, *delta);
            }
            Self::Text { utf8 } => {
                let bytes = utf8.as_bytes();
                if bytes.len() > MAX_TEXT_LEN {
                    return Err(ProtoError::TextTooLong {
                        len: bytes.len(),
                        max: MAX_TEXT_LEN,
                    });
                }
                put_u16(out, bytes.len() as u16);
                out.extend_from_slice(bytes);
            }
            Self::EnterScreen {
                target,
                edge,
                ratio,
                mods,
                preserve_cursor,
            } => {
                put_u16(out, *target);
                put_u8(out, edge.wire());
                put_f32(out, *ratio);
                put_u32(out, mods.bits());
                put_u8(out, u8::from(*preserve_cursor));
            }
            Self::LeaveScreen { mods } | Self::ModifierSync { mods } => {
                put_u32(out, mods.bits());
            }
            Self::SwitchRequest { target } => {
                put_u16(out, *target);
            }
            Self::SwitchAck { target, ok } => {
                put_u16(out, *target);
                put_u8(out, u8::from(*ok));
            }
        }
        Ok(())
    }

    /// payload 解码；未知 kind 返回 `None`（调用方按 docs/02 §4.1 按 len 跳过）。
    ///
    /// # Errors
    /// - 定长布局不符 → [`ProtoError::LayoutMismatch`]；
    /// - 值域违规（保留区键码 / 未知枚举值 / 非有限浮点）→ 对应变体；
    /// - `Text` 超长 / 非法 UTF-8 → [`ProtoError::TextTooLong`] / [`ProtoError::TextUtf8`]。
    pub fn decode(kind: u8, payload: &[u8]) -> Result<Option<Self>, ProtoError> {
        if !frame::is_input_kind(kind) {
            return Ok(None);
        }
        let event = match kind {
            frame::kind::KEY => {
                expect_len(kind, payload, 4)?;
                let code = read_key(payload, 0)?;
                let state = read_enum(kind, payload, 2, "state", KeyState::from_wire)?;
                Self::Key { code, state }
            }
            frame::kind::KEY_REPEAT => {
                expect_len(kind, payload, 4)?;
                Self::KeyRepeat {
                    code: read_key(payload, 0)?,
                }
            }
            frame::kind::MOUSE_MOVE_REL => {
                expect_len(kind, payload, 8)?;
                Self::MouseMoveRel {
                    dx: read_f32(kind, payload, 0, "dx")?,
                    dy: read_f32(kind, payload, 4, "dy")?,
                }
            }
            frame::kind::MOUSE_MOVE_ABS => {
                expect_len(kind, payload, 10)?;
                Self::MouseMoveAbs {
                    screen: read_u16(payload, 0)?,
                    x: read_f32(kind, payload, 2, "x")?,
                    y: read_f32(kind, payload, 6, "y")?,
                }
            }
            frame::kind::MOUSE_BUTTON => {
                expect_len(kind, payload, 2)?;
                let button = read_enum(kind, payload, 0, "button", MouseButton::from_wire)?;
                let state = read_enum(kind, payload, 1, "state", KeyState::from_wire)?;
                Self::MouseButton { button, state }
            }
            frame::kind::WHEEL => {
                expect_len(kind, payload, 5)?;
                let axis = read_enum(kind, payload, 0, "axis", Axis::from_wire)?;
                let delta = read_f32(kind, payload, 1, "delta")?;
                Self::Wheel { axis, delta }
            }
            frame::kind::TEXT => {
                // len 字段本身需要 2 字节：先兜底校验再读（恶意截断不得 panic）
                if payload.len() < 2 {
                    return Err(ProtoError::LayoutMismatch {
                        kind,
                        expected: 2,
                        got: payload.len(),
                    });
                }
                let declared = read_u16(payload, 0)? as usize;
                if declared > MAX_TEXT_LEN {
                    return Err(ProtoError::TextTooLong {
                        len: declared,
                        max: MAX_TEXT_LEN,
                    });
                }
                let expected = declared + 2;
                if payload.len() != expected {
                    return Err(ProtoError::LayoutMismatch {
                        kind,
                        expected,
                        got: payload.len(),
                    });
                }
                let utf8 = core::str::from_utf8(&payload[2..])
                    .map_err(ProtoError::TextUtf8)?
                    .to_owned();
                Self::Text { utf8 }
            }
            frame::kind::ENTER_SCREEN => {
                expect_len(kind, payload, 12)?;
                let edge = read_enum(kind, payload, 2, "edge", ScreenEdge::from_wire)?;
                Self::EnterScreen {
                    target: read_u16(payload, 0)?,
                    edge,
                    ratio: read_f32(kind, payload, 3, "ratio")?,
                    mods: Mods::from_bits(read_u32(payload, 7)?),
                    preserve_cursor: payload[11] != 0,
                }
            }
            frame::kind::LEAVE_SCREEN => {
                expect_len(kind, payload, 4)?;
                Self::LeaveScreen {
                    mods: Mods::from_bits(read_u32(payload, 0)?),
                }
            }
            frame::kind::MODIFIER_SYNC => {
                expect_len(kind, payload, 4)?;
                Self::ModifierSync {
                    mods: Mods::from_bits(read_u32(payload, 0)?),
                }
            }
            frame::kind::SWITCH_REQUEST => {
                expect_len(kind, payload, 2)?;
                Self::SwitchRequest {
                    target: read_u16(payload, 0)?,
                }
            }
            frame::kind::SWITCH_ACK => {
                expect_len(kind, payload, 3)?;
                let target = read_u16(payload, 0)?;
                let ok = read_enum(kind, payload, 2, "ok", wire_bool)?;
                Self::SwitchAck { target, ok }
            }
            // is_input_kind 已排除未知 kind；match 穷尽性兜底
            _ => return Ok(None),
        };
        Ok(Some(event))
    }

    /// 从已解出的帧解码输入事件（kind+payload 分派的便捷形式）。
    ///
    /// # Errors
    /// 同 [`InputEvent::decode`]。
    pub fn decode_frame(frame: &Frame<'_>) -> Result<Option<Self>, ProtoError> {
        Self::decode(frame.header.kind, frame.payload)
    }
}

impl crate::Message for InputEvent {
    fn kind(&self) -> u8 {
        self.kind()
    }

    fn encode_payload(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        self.encode_payload(out)
    }
}

// —— 定长小端编解码辅助（布局长度由 expect_len/read_* 的调用点先行校验）——

fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn expect_len(kind: u8, payload: &[u8], expected: usize) -> Result<(), ProtoError> {
    if payload.len() != expected {
        return Err(ProtoError::LayoutMismatch {
            kind,
            expected,
            got: payload.len(),
        });
    }
    Ok(())
}

fn read_u16(payload: &[u8], offset: usize) -> Result<u16, ProtoError> {
    let mut buf = [0u8; 2];
    buf.copy_from_slice(&payload[offset..offset + 2]);
    Ok(u16::from_le_bytes(buf))
}

fn read_u32(payload: &[u8], offset: usize) -> Result<u32, ProtoError> {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&payload[offset..offset + 4]);
    Ok(u32::from_le_bytes(buf))
}

fn read_f32(
    kind: u8,
    payload: &[u8],
    offset: usize,
    field: &'static str,
) -> Result<f32, ProtoError> {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&payload[offset..offset + 4]);
    let value = f32::from_le_bytes(buf);
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ProtoError::NonFiniteFloat { kind, field })
    }
}

fn read_key(payload: &[u8], offset: usize) -> Result<XKey, ProtoError> {
    let code = read_u16(payload, offset)?;
    XKey::new(code).ok_or(ProtoError::ReservedKey { code })
}

/// wire u8 → bool（`0=false, 1=true`；其余非法）。
fn wire_bool(value: u8) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn read_enum<T>(
    kind: u8,
    payload: &[u8],
    offset: usize,
    field: &'static str,
    from_wire: fn(u8) -> Option<T>,
) -> Result<T, ProtoError> {
    let value = payload[offset];
    from_wire(value).ok_or(ProtoError::FieldValue { kind, field, value })
}

/// `MouseMoveRel` 合帧辅助（docs/02 §4.2 合帧规则、docs/03 §3）。
///
/// 发送窗口内对 delta 做 f32 累加；单窗口最多合并 [`RelMerger::DEFAULT_MAX_EVENTS`]
/// 条（8 条 = ≤250Hz 鼠标 + 120Hz 发送周期下的上界）。**只合并相对移动，
/// 不做任何事件重排**；键盘/按钮事件严禁合并（调用方不要把它们交给本辅助）。
///
/// 线上仍是单条 `MouseMoveRel` f32 delta；注入端另有 f64 累加器处理小数余量
/// （docs/02 §4.2 kind 0x03 注）。
#[derive(Debug, Clone)]
pub struct RelMerger {
    dx: f32,
    dy: f32,
    count: u8,
    max: u8,
}

impl RelMerger {
    /// 单窗口合并上限：8 条（docs/03 §3）。
    pub const DEFAULT_MAX_EVENTS: u8 = 8;

    /// 默认上限（8 条/窗口）。
    #[must_use]
    pub const fn new() -> Self {
        Self::with_max(Self::DEFAULT_MAX_EVENTS)
    }

    /// 自定义窗口上限（≥1；0 视为 1，即逐条直发不合帧）。
    #[must_use]
    pub const fn with_max(max: u8) -> Self {
        let max = if max == 0 { 1 } else { max };
        Self {
            dx: 0.0,
            dy: 0.0,
            count: 0,
            max,
        }
    }

    /// 累加一条相对移动 delta。
    pub fn push(&mut self, dx: f32, dy: f32) {
        self.dx += dx;
        self.dy += dy;
        self.count += 1;
    }

    /// 累加并在窗口已满时先取出上一窗口累计（发送方循环使用）。
    /// 返回 `Some((dx, dy))` 表示上一窗口已满、应先发送该累计。
    pub fn push_and_flush_full(&mut self, dx: f32, dy: f32) -> Option<(f32, f32)> {
        let flushed = if self.count >= self.max {
            self.take()
        } else {
            None
        };
        self.push(dx, dy);
        flushed
    }

    /// 窗口内没有任何待发送累计。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 窗口已满（达到合并上限，应尽快 flush）。
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.count >= self.max
    }

    /// 窗口内已合并的事件条数。
    #[must_use]
    pub const fn pending_events(&self) -> u8 {
        self.count
    }

    /// 窗口内累计的 delta（不取出）。
    #[must_use]
    pub const fn pending_delta(&self) -> (f32, f32) {
        (self.dx, self.dy)
    }

    /// 取出累计 delta（发送后清空窗口）；空窗口返回 `None`。
    pub fn take(&mut self) -> Option<(f32, f32)> {
        if self.count == 0 {
            return None;
        }
        let delta = (self.dx, self.dy);
        self.dx = 0.0;
        self.dy = 0.0;
        self.count = 0;
        Some(delta)
    }
}

impl Default for RelMerger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use crate::xkey::{KEY_A, KEY_LEFT_CTRL, KEY_LEFT_META};

    /// docs/02 §4.2 定长布局锚点：kind 与 payload 字节数逐条对应。
    #[test]
    fn payload_layout_lengths() {
        let cases: [(u8, InputEvent, usize); 12] = [
            (
                frame::kind::KEY,
                InputEvent::Key {
                    code: KEY_A,
                    state: KeyState::Down,
                },
                4,
            ),
            (
                frame::kind::KEY_REPEAT,
                InputEvent::KeyRepeat { code: KEY_A },
                4,
            ),
            (
                frame::kind::MOUSE_MOVE_REL,
                InputEvent::MouseMoveRel { dx: 1.5, dy: -2.5 },
                8,
            ),
            (
                frame::kind::MOUSE_MOVE_ABS,
                InputEvent::MouseMoveAbs {
                    screen: 1,
                    x: 0.25,
                    y: 0.75,
                },
                10,
            ),
            (
                frame::kind::MOUSE_BUTTON,
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: KeyState::Up,
                },
                2,
            ),
            (
                frame::kind::WHEEL,
                InputEvent::Wheel {
                    axis: Axis::Vertical,
                    delta: -1.0,
                },
                5,
            ),
            (
                frame::kind::TEXT,
                InputEvent::Text {
                    utf8: "héllo".to_owned(),
                },
                2 + "héllo".len(),
            ),
            (
                frame::kind::ENTER_SCREEN,
                InputEvent::EnterScreen {
                    target: 2,
                    edge: ScreenEdge::Right,
                    ratio: 0.5,
                    mods: Mods::LMETA,
                    preserve_cursor: false,
                },
                12,
            ),
            (
                frame::kind::LEAVE_SCREEN,
                InputEvent::LeaveScreen { mods: Mods::RALT },
                4,
            ),
            (
                frame::kind::MODIFIER_SYNC,
                InputEvent::ModifierSync {
                    mods: Mods::LCTRL | Mods::LSHIFT,
                },
                4,
            ),
            (
                frame::kind::SWITCH_REQUEST,
                InputEvent::SwitchRequest { target: 3 },
                2,
            ),
            (
                frame::kind::SWITCH_ACK,
                InputEvent::SwitchAck {
                    target: 3,
                    ok: true,
                },
                3,
            ),
        ];
        for (want_kind, event, len) in cases {
            assert_eq!(event.kind(), want_kind, "kind 映射错误：{event:?}");
            let mut payload = Vec::new();
            event.encode_payload(&mut payload).unwrap();
            assert_eq!(payload.len(), len, "{event:?}");
            let decoded = InputEvent::decode(want_kind, &payload)
                .unwrap()
                .expect("已知 kind 必须可解");
            assert_eq!(decoded, event);
        }
    }

    /// Key 事件 wire 布局 golden：code 小端 + state + pad（docs/02 §4.2 kind 0x01）。
    #[test]
    fn key_wire_golden() {
        let mut payload = Vec::new();
        InputEvent::Key {
            code: KEY_A,
            state: KeyState::Down,
        }
        .encode_payload(&mut payload)
        .unwrap();
        assert_eq!(payload, vec![0x04, 0x00, 0x00, 0x00]); // code=0x0004, state=0(down), pad=0
        payload[1] = 0x00; // 高字节
        payload[2] = 0x01; // state=up
        payload[3] = 0x77; // pad 忽略
        let decoded = InputEvent::decode(frame::kind::KEY, &payload)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded,
            InputEvent::Key {
                code: KEY_A,
                state: KeyState::Up
            }
        );
    }

    /// MouseMoveAbs wire golden：screen 小端 + x/y f32 小端。
    #[test]
    fn mouse_abs_wire_golden() {
        let mut payload = Vec::new();
        InputEvent::MouseMoveAbs {
            screen: 0x0102,
            x: 0.5,
            y: 1.0,
        }
        .encode_payload(&mut payload)
        .unwrap();
        assert_eq!(payload[0], 0x02);
        assert_eq!(payload[1], 0x01);
        assert_eq!(&payload[2..6], &0.5f32.to_le_bytes());
        assert_eq!(&payload[6..10], &1.0f32.to_le_bytes());
    }

    /// Text：512B 边界内往返；513B 编码报错；声明超长解码报错；非法 UTF-8 报错。
    #[test]
    fn text_boundaries() {
        let exact = "a".repeat(MAX_TEXT_LEN);
        let event = InputEvent::Text {
            utf8: exact.clone(),
        };
        let mut payload = Vec::new();
        event.encode_payload(&mut payload).unwrap();
        assert_eq!(payload.len(), MAX_TEXT_LEN + 2);
        let decoded = InputEvent::decode(frame::kind::TEXT, &payload)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, event);

        // 编码侧超长
        let over = "a".repeat(MAX_TEXT_LEN + 1);
        let err = InputEvent::Text { utf8: over.clone() }
            .encode_payload(&mut Vec::new())
            .unwrap_err();
        assert_eq!(
            err,
            ProtoError::TextTooLong {
                len: over.len(),
                max: MAX_TEXT_LEN
            }
        );

        // 解码侧声明超长（恶意 len，u16 LE 编码）
        let over_declared = (MAX_TEXT_LEN + 1) as u16;
        let evil = over_declared.to_le_bytes().to_vec();
        let err = InputEvent::decode(frame::kind::TEXT, &evil).unwrap_err();
        assert_eq!(
            err,
            ProtoError::TextTooLong {
                len: MAX_TEXT_LEN + 1,
                max: MAX_TEXT_LEN
            }
        );

        // 非法 UTF-8
        let bad = [0x01, 0x00, 0xFF];
        let err = InputEvent::decode(frame::kind::TEXT, &bad).unwrap_err();
        assert!(matches!(err, ProtoError::TextUtf8(_)));

        // len 与实际字节不符
        let mismatch = [0x05, 0x00, b'a', b'b'];
        let err = InputEvent::decode(frame::kind::TEXT, &mismatch).unwrap_err();
        assert_eq!(
            err,
            ProtoError::LayoutMismatch {
                kind: frame::kind::TEXT,
                expected: 7,
                got: 4
            }
        );
    }

    /// 值域负例：保留区键码 / 未知枚举值 / 非有限浮点 / bool 非 0-1。
    #[test]
    fn value_domain_negatives() {
        // 保留区键码（0x00 / 0xE8 / 0x300；Key 布局 = code u16 + state u8 + pad u8）
        for code in [0x0000u16, 0xE8, 0x300] {
            let mut payload = code.to_le_bytes().to_vec();
            payload.extend_from_slice(&[0x00, 0x00]);
            let err = InputEvent::decode(frame::kind::KEY, &payload).unwrap_err();
            assert_eq!(err, ProtoError::ReservedKey { code });
        }
        // KeyState 非法
        let err = InputEvent::decode(frame::kind::KEY, &[0x04, 0x00, 0x02, 0x00]).unwrap_err();
        assert_eq!(
            err,
            ProtoError::FieldValue {
                kind: frame::kind::KEY,
                field: "state",
                value: 2
            }
        );
        // MouseButton 非法（5 不在 0-4）
        let err = InputEvent::decode(frame::kind::MOUSE_BUTTON, &[5, 0]).unwrap_err();
        assert!(matches!(
            err,
            ProtoError::FieldValue {
                field: "button",
                ..
            }
        ));
        // Axis 非法
        let err = InputEvent::decode(frame::kind::WHEEL, &[2, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, ProtoError::FieldValue { field: "axis", .. }));
        // ScreenEdge 非法
        let payload = {
            let mut p = Vec::new();
            p.extend_from_slice(&1u16.to_le_bytes());
            p.push(4);
            p.extend_from_slice(&0.5f32.to_le_bytes());
            p.extend_from_slice(&0u32.to_le_bytes());
            p.push(0); // preserve_cursor（11-23 追加字节）
            p
        };
        let err = InputEvent::decode(frame::kind::ENTER_SCREEN, &payload).unwrap_err();
        assert!(matches!(err, ProtoError::FieldValue { field: "edge", .. }));
        // 非有限浮点（NaN / inf）
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&bad.to_le_bytes());
            payload.extend_from_slice(&0f32.to_le_bytes());
            let err = InputEvent::decode(frame::kind::MOUSE_MOVE_REL, &payload).unwrap_err();
            assert!(matches!(
                err,
                ProtoError::NonFiniteFloat { field: "dx", .. }
            ));
        }
        // SwitchAck ok 非 0-1
        let err = InputEvent::decode(frame::kind::SWITCH_ACK, &[1, 0, 2]).unwrap_err();
        assert!(matches!(err, ProtoError::FieldValue { field: "ok", .. }));
    }

    /// 定长布局负例：多/少字节一律 LayoutMismatch（含空 payload）。
    #[test]
    fn layout_mismatch_negatives() {
        let cases = [
            (frame::kind::KEY, 3usize),
            (frame::kind::KEY, 5),
            (frame::kind::KEY_REPEAT, 0),
            (frame::kind::MOUSE_MOVE_REL, 7),
            (frame::kind::MOUSE_MOVE_ABS, 11),
            (frame::kind::MOUSE_BUTTON, 1),
            (frame::kind::MOUSE_BUTTON, 3),
            (frame::kind::WHEEL, 4),
            (frame::kind::WHEEL, 6),
            (frame::kind::TEXT, 0), // len 字段本身截断
            (frame::kind::TEXT, 1),
            (frame::kind::ENTER_SCREEN, 10),
            (frame::kind::ENTER_SCREEN, 13),
            (frame::kind::LEAVE_SCREEN, 3),
            (frame::kind::MODIFIER_SYNC, 5),
            (frame::kind::SWITCH_REQUEST, 1),
            (frame::kind::SWITCH_ACK, 2),
            (frame::kind::SWITCH_ACK, 4),
        ];
        for (kind, len) in cases {
            let payload = vec![0u8; len];
            let err = InputEvent::decode(kind, &payload).unwrap_err();
            assert!(
                matches!(err, ProtoError::LayoutMismatch { .. }),
                "kind 0x{kind:02X} len={len}"
            );
        }
    }

    /// 未知 / 非输入 kind → Ok(None)（调用方按 len 跳过）。
    #[test]
    fn unknown_kind_is_none() {
        for kind in [
            0x00,
            0x08,
            0x30,
            frame::kind::CTRL_HELLO,
            frame::kind::CLIP_CHUNK,
            0xFF,
        ] {
            let decoded = InputEvent::decode(kind, &[]).unwrap();
            assert_eq!(decoded, None, "kind 0x{kind:02X}");
        }
    }

    /// 完整帧往返：encode_frame → decode_frame → decode。
    #[test]
    fn frame_roundtrip() {
        let event = InputEvent::EnterScreen {
            target: 1,
            edge: ScreenEdge::Bottom,
            ratio: 0.9,
            mods: Mods::LCTRL | Mods::LMETA,
            preserve_cursor: false,
        };
        let bytes = event.encode_frame(42).unwrap();
        let frame = frame::decode_frame(&bytes).unwrap();
        assert_eq!(frame.header.seq, 42);
        assert_eq!(frame.header.kind, frame::kind::ENTER_SCREEN);
        let decoded = InputEvent::decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded, event);
    }

    /// Mods 位运算与 XKey 映射（docs/02 §4.2 bit0-7）。
    #[test]
    fn mods_ops() {
        assert_eq!(Mods::default(), Mods(0));
        assert!(Mods::default().is_empty());
        let both = Mods::LCTRL | Mods::RMETA;
        assert_eq!(both.bits(), 0x81);
        assert!(both.contains(Mods::LCTRL));
        assert!(both.contains(Mods::RMETA));
        assert!(!both.contains(Mods::RSHIFT));
        assert_eq!(
            both.union(Mods::RSHIFT),
            Mods::LCTRL | Mods::RMETA | Mods::RSHIFT
        );
        // XKey 修饰键 → mods
        assert_eq!(Mods::from_key(KEY_LEFT_CTRL), Some(Mods::LCTRL));
        assert_eq!(Mods::from_key(KEY_LEFT_META), Some(Mods::LMETA));
        assert_eq!(Mods::from_key(KEY_A), None);
        for bit in 0..8u8 {
            let mods = Mods::from_key_bit(bit);
            assert_eq!(mods.bits(), 1 << bit);
        }
        // 未知高位原样保留
        assert_eq!(Mods::from_bits(0xFFFF_FFFF).bits(), 0xFFFF_FFFF);
    }

    /// RelMerger：累加 / 上限 / flush / 空窗口语义。
    #[test]
    fn rel_merger_accumulates_and_caps() {
        let mut merger = RelMerger::new();
        assert!(merger.is_empty());
        assert_eq!(merger.take(), None);

        for i in 0..RelMerger::DEFAULT_MAX_EVENTS {
            assert!(!merger.is_full());
            merger.push(1.0, i as f32);
        }
        assert_eq!(merger.pending_events(), RelMerger::DEFAULT_MAX_EVENTS);
        assert!(merger.is_full());
        let (dx, dy) = merger.take().unwrap();
        assert!((dx - 8.0).abs() < f32::EPSILON);
        assert!((dy - 28.0).abs() < f32::EPSILON);
        assert!(merger.is_empty());

        // push_and_flush_full：满窗时自动先交出旧窗口
        let mut merger = RelMerger::with_max(2);
        assert_eq!(merger.pending_events(), 0);
        assert_eq!(merger.push_and_flush_full(1.0, 2.0), None);
        assert_eq!(merger.pending_events(), 1);
        assert_eq!(merger.push_and_flush_full(3.0, 4.0), None);
        assert_eq!(merger.pending_events(), 2);
        let flushed = merger.push_and_flush_full(5.0, 6.0).unwrap();
        assert_eq!(flushed, (4.0, 6.0)); // (1+3, 2+4)
        assert_eq!(merger.pending_delta(), (5.0, 6.0));
        assert_eq!(merger.pending_events(), 1);
    }

    /// RelMerger：自定义上限边界（0 → 逐条直发）与 Default。
    #[test]
    fn rel_merger_limits() {
        let mut merger = RelMerger::with_max(0);
        merger.push(1.0, 1.0);
        assert!(merger.is_full(), "max=0 视为 1：一条即满");
        assert_eq!(merger.take(), Some((1.0, 1.0)));
        let merger = RelMerger::default();
        assert_eq!(merger.pending_events(), 0);
    }
}
