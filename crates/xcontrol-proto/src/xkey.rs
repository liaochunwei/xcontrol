//! XKey 键码体系（docs/03 §2）：与平台无关的 `u16` 抽象键码，取值空间为
//! USB HID usage ID（键盘页 0x07 + 消费者页 0x0C，基址偏移 0x100）。
//!
//! 值域（docs/03 §2，保留区拒绝）：
//!
//! | 范围 | 含义 |
//! | --- | --- |
//! | `0x04–0xE7` | HID 键盘页（`KEY_A`=0x04 … `KEY_LEFT_CTRL`=0xE0 … `KEY_RIGHT_META`=0xE7） |
//! | `0x100–0x1FF` | HID 消费者页 + 0x100 基址偏移（`CK_VOLUME_UP`=0x100+0xE9=0x1E9） |
//! | `0x200–0x2FF` | 指针键扩展区（本项目自定义超集，暂无常量） |
//! | 其余 | 保留，禁止使用（[`XKey::new`] 拒绝、wire 解码报 [`crate::ProtoError::ReservedKey`]） |
//!
//! # 常量表覆盖范围
//!
//! 本表覆盖**键盘页核心区**（字母/数字/符号/修饰键/导航键/F1–F24/小键盘/常用编辑与音量键）
//! 与**消费者页常用键**（媒体/音量/亮度/电源）。键盘页 0x85–0x9F（international/lang/
//! locking 等零散区段）与指针区常量随各平台转换表任务（docs/03 §2 表）按需补齐——
//! 不为没有文档依据的自定义键发明语义；全表范围以 USB HID 官方 usage 表为准。

/// 值域边界（docs/03 §2 表）。
pub mod ranges {
    /// HID 键盘页（usage page 0x07）：`0x04–0xE7`。
    pub const KEYBOARD: (u16, u16) = (0x04, 0xE7);
    /// HID 消费者页（usage page 0x0C）+ 0x100 基址偏移：`0x100–0x1FF`。
    pub const CONSUMER: (u16, u16) = (0x100, 0x1FF);
    /// 指针键扩展区（自定义超集）：`0x200–0x2FF`。
    pub const POINTER: (u16, u16) = (0x200, 0x2FF);
}

/// 与平台无关的抽象键码（docs/03 §2）。
///
/// 构造即校验：保留区（[`ranges`] 之外）返回 `None`；wire 解码遇到保留区值
/// 报 [`crate::ProtoError::ReservedKey`]。键码按**物理键位**语义传输，
/// 字符由从机自身布局解释（FR-2.6，docs/03 §1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct XKey(u16);

impl XKey {
    /// 从 wire 值构造；保留区返回 `None`（docs/03 §2）。
    #[must_use]
    pub const fn new(code: u16) -> Option<Self> {
        if Self::is_valid_raw(code) {
            Some(Self(code))
        } else {
            None
        }
    }

    /// 原始 wire 值（docs/03 §2 值域）。
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// 值域校验：键盘页 / 消费者页 / 指针区之一（docs/03 §2）。
    #[must_use]
    pub const fn is_valid_raw(code: u16) -> bool {
        let (kb_lo, kb_hi) = ranges::KEYBOARD;
        let (ck_lo, ck_hi) = ranges::CONSUMER;
        let (pt_lo, pt_hi) = ranges::POINTER;
        (code >= kb_lo && code <= kb_hi)
            || (code >= ck_lo && code <= ck_hi)
            || (code >= pt_lo && code <= pt_hi)
    }

    /// 是否修饰键（`0xE0–0xE7`；docs/02 §4.2 mods 位掩码的对应键）。
    #[must_use]
    pub const fn is_modifier(self) -> bool {
        self.0 >= 0xE0 && self.0 <= 0xE7
    }

    /// 修饰键 → mods 位序号（docs/02 §4.2：bit0-7 = LCtrl/LShift/LAlt/LMeta/RCtrl/RShift/RAlt/RMeta）。
    /// 非修饰键返回 `None`；位序号可经 [`crate::input::Mods::from_key_bit`] 展开。
    #[must_use]
    pub const fn mods_bit(self) -> Option<u8> {
        if !self.is_modifier() {
            return None;
        }
        Some((self.0 - 0xE0) as u8)
    }

    /// 常量构造（保留区在 const 求值期直接编译失败）。
    const fn const_key(code: u16) -> Self {
        if !Self::is_valid_raw(code) {
            panic!("XKey 常量越界（保留区）");
        }
        Self(code)
    }
}

/// 键盘页 / 消费者页常量表（docs/03 §2；覆盖范围见模块文档）。
macro_rules! define_keys {
    ($($(#[$meta:meta])* $name:ident = $raw:literal),* $(,)?) => {
        $(
            #[doc = concat!("`", stringify!($name), "`（wire 值 `", stringify!($raw), "`）。")]
            $(#[$meta])*
            pub const $name: XKey = XKey::const_key($raw);
        )*
    };
}

define_keys! {
        // —— 字母（HID 键盘页 0x04–0x1D）——
        KEY_A = 0x04, KEY_B = 0x05, KEY_C = 0x06, KEY_D = 0x07,
        KEY_E = 0x08, KEY_F = 0x09, KEY_G = 0x0A, KEY_H = 0x0B,
        KEY_I = 0x0C, KEY_J = 0x0D, KEY_K = 0x0E, KEY_L = 0x0F,
        KEY_M = 0x10, KEY_N = 0x11, KEY_O = 0x12, KEY_P = 0x13,
        KEY_Q = 0x14, KEY_R = 0x15, KEY_S = 0x16, KEY_T = 0x17,
        KEY_U = 0x18, KEY_V = 0x19, KEY_W = 0x1A, KEY_X = 0x1B,
        KEY_Y = 0x1C, KEY_Z = 0x1D,
        // —— 数字行（0x1E–0x27，1 在前 0 在后）——
        KEY_1 = 0x1E, KEY_2 = 0x1F, KEY_3 = 0x20, KEY_4 = 0x21,
        KEY_5 = 0x22, KEY_6 = 0x23, KEY_7 = 0x24, KEY_8 = 0x25,
        KEY_9 = 0x26, KEY_0 = 0x27,
        // —— 控制与符号键 ——
        KEY_ENTER = 0x28, KEY_ESCAPE = 0x29, KEY_BACKSPACE = 0x2A, KEY_TAB = 0x2B,
        KEY_SPACE = 0x2C, KEY_MINUS = 0x2D, KEY_EQUAL = 0x2E,
        KEY_LEFTBRACKET = 0x2F, KEY_RIGHTBRACKET = 0x30, KEY_BACKSLASH = 0x31,
        KEY_NONUS_HASH = 0x32, KEY_SEMICOLON = 0x33, KEY_APOSTROPHE = 0x34,
        KEY_GRAVE = 0x35, KEY_COMMA = 0x36, KEY_DOT = 0x37, KEY_SLASH = 0x38,
        KEY_CAPS_LOCK = 0x39,
        // —— F 区（0x3A–0x45）与系统键 ——
        KEY_F1 = 0x3A, KEY_F2 = 0x3B, KEY_F3 = 0x3C, KEY_F4 = 0x3D,
        KEY_F5 = 0x3E, KEY_F6 = 0x3F, KEY_F7 = 0x40, KEY_F8 = 0x41,
        KEY_F9 = 0x42, KEY_F10 = 0x43, KEY_F11 = 0x44, KEY_F12 = 0x45,
        KEY_PRINT_SCREEN = 0x46, KEY_SCROLL_LOCK = 0x47, KEY_PAUSE = 0x48,
        // —— 导航键 ——
        KEY_INSERT = 0x49, KEY_HOME = 0x4A, KEY_PAGE_UP = 0x4B,
        KEY_DELETE = 0x4C, KEY_END = 0x4D, KEY_PAGE_DOWN = 0x4E,
        KEY_RIGHT = 0x4F, KEY_LEFT = 0x50, KEY_DOWN = 0x51, KEY_UP = 0x52,
        // —— 小键盘 ——
        KEY_NUM_LOCK = 0x53, KEY_KP_DIVIDE = 0x54, KEY_KP_MULTIPLY = 0x55,
        KEY_KP_MINUS = 0x56, KEY_KP_PLUS = 0x57, KEY_KP_ENTER = 0x58,
        KEY_KP_1 = 0x59, KEY_KP_2 = 0x5A, KEY_KP_3 = 0x5B,
        KEY_KP_4 = 0x5C, KEY_KP_5 = 0x5D, KEY_KP_6 = 0x5E,
        KEY_KP_7 = 0x5F, KEY_KP_8 = 0x60, KEY_KP_9 = 0x61,
        KEY_KP_0 = 0x62, KEY_KP_DOT = 0x63,
        KEY_NONUS_BACKSLASH = 0x64, KEY_APPLICATION = 0x65, KEY_POWER = 0x66,
        KEY_KP_EQUAL = 0x67,
        // —— F13–F24 ——
        KEY_F13 = 0x68, KEY_F14 = 0x69, KEY_F15 = 0x6A, KEY_F16 = 0x6B,
        KEY_F17 = 0x6C, KEY_F18 = 0x6D, KEY_F19 = 0x6E, KEY_F20 = 0x6F,
        KEY_F21 = 0x70, KEY_F22 = 0x71, KEY_F23 = 0x72, KEY_F24 = 0x73,
        // —— 编辑/常用动作键 ——
        KEY_EXECUTE = 0x74, KEY_HELP = 0x75, KEY_MENU = 0x76, KEY_SELECT = 0x77,
        KEY_STOP = 0x78, KEY_AGAIN = 0x79, KEY_UNDO = 0x7A, KEY_CUT = 0x7B,
        KEY_COPY = 0x7C, KEY_PASTE = 0x7D, KEY_FIND = 0x7E,
        KEY_MUTE = 0x7F, KEY_VOLUME_UP = 0x80, KEY_VOLUME_DOWN = 0x81,
        // —— 修饰键（0xE0–0xE7，与 mods 位掩码 bit0-7 对应）——
        KEY_LEFT_CTRL = 0xE0, KEY_LEFT_SHIFT = 0xE1,
        KEY_LEFT_ALT = 0xE2, KEY_LEFT_META = 0xE3,
        KEY_RIGHT_CTRL = 0xE4, KEY_RIGHT_SHIFT = 0xE5,
        KEY_RIGHT_ALT = 0xE6, KEY_RIGHT_META = 0xE7,
        // —— 消费者页常用键（0x100 + HID 消费者页 usage，`CK_` 前缀）——
        CK_BRIGHTNESS_UP = 0x16F, CK_BRIGHTNESS_DOWN = 0x170,
        CK_POWER = 0x130, CK_RESET = 0x131, CK_SLEEP = 0x132, CK_WAKE_UP = 0x183,
        CK_PLAY = 0x1B0, CK_PAUSE = 0x1B1, CK_RECORD = 0x1B2,
        CK_FAST_FORWARD = 0x1B3, CK_REWIND = 0x1B4,
        CK_SCAN_NEXT_TRACK = 0x1B5, CK_SCAN_PREV_TRACK = 0x1B6,
        CK_STOP = 0x1B7, CK_EJECT = 0x1B8, CK_PLAY_PAUSE = 0x1CD,
        CK_MUTE = 0x1E2, CK_VOLUME_UP = 0x1E9, CK_VOLUME_DOWN = 0x1EA,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 值域三段各自接受边界值。
    #[test]
    fn valid_ranges() {
        assert_eq!(XKey::new(0x04).map(XKey::raw), Some(0x04));
        assert_eq!(XKey::new(0xE7).map(XKey::raw), Some(0xE7));
        assert_eq!(XKey::new(0x100).map(XKey::raw), Some(0x100));
        assert_eq!(XKey::new(0x1FF).map(XKey::raw), Some(0x1FF));
        assert_eq!(XKey::new(0x200).map(XKey::raw), Some(0x200));
        assert_eq!(XKey::new(0x2FF).map(XKey::raw), Some(0x2FF));
    }

    /// 保留区拒绝（docs/03 §2：段间缝隙与越界值）。
    #[test]
    fn reserved_rejected() {
        for code in [0x00, 0x03, 0xE8, 0xFF, 0x1_00 + 0x200, 0x300, 0xFFFF] {
            assert!(XKey::new(code).is_none(), "0x{code:04X} 应在保留区");
            assert!(!XKey::is_valid_raw(code));
        }
    }

    /// 值域无缝校验：三段区间内全部合法、区间外全部非法。
    #[test]
    fn range_boundaries_exhaustive() {
        let (kb_lo, kb_hi) = ranges::KEYBOARD;
        let (ck_lo, ck_hi) = ranges::CONSUMER;
        let (pt_lo, pt_hi) = ranges::POINTER;
        for code in kb_lo..=kb_hi {
            assert!(XKey::is_valid_raw(code));
        }
        for code in ck_lo..=ck_hi {
            assert!(XKey::is_valid_raw(code));
        }
        for code in pt_lo..=pt_hi {
            assert!(XKey::is_valid_raw(code));
        }
        // 段间与段外缝隙（消费者页与指针区相邻：0x1FF+1 = 0x200 均合法）
        for code in [kb_lo - 1, kb_hi + 1, ck_lo - 1, pt_hi + 1] {
            assert!(!XKey::is_valid_raw(code), "0x{code:04X} 应非法");
        }
    }

    /// docs/03 §2 钉死的锚点常量。
    #[test]
    fn doc_anchors() {
        assert_eq!(KEY_A.raw(), 0x04);
        assert_eq!(KEY_1.raw(), 0x1E);
        assert_eq!(KEY_Z.raw(), 0x1D);
        assert_eq!(KEY_LEFT_CTRL.raw(), 0xE0);
        assert_eq!(KEY_LEFT_META.raw(), 0xE3);
        assert_eq!(KEY_RIGHT_META.raw(), 0xE7);
        // docs/03 §2：VolumeUp = 0x100 + 0xE9
        assert_eq!(CK_VOLUME_UP.raw(), 0x100 + 0xE9);
    }

    /// 修饰键判定与 mods 位序号（docs/02 §4.2 bit0-7）。
    #[test]
    fn modifier_bits() {
        let expect = [
            (KEY_LEFT_CTRL, 0u8),
            (KEY_LEFT_SHIFT, 1),
            (KEY_LEFT_ALT, 2),
            (KEY_LEFT_META, 3),
            (KEY_RIGHT_CTRL, 4),
            (KEY_RIGHT_SHIFT, 5),
            (KEY_RIGHT_ALT, 6),
            (KEY_RIGHT_META, 7),
        ];
        for (key, bit) in expect {
            assert!(key.is_modifier());
            assert_eq!(key.mods_bit(), Some(bit));
        }
        assert!(!KEY_A.is_modifier());
        assert_eq!(KEY_A.mods_bit(), None);
        assert!(!CK_VOLUME_UP.is_modifier());
    }

    /// 全部常量必须落在合法值域（const 构造已有编译期保障，此处作运行期冗余断言）。
    #[test]
    fn all_constants_in_range() {
        for key in [
            KEY_A,
            KEY_B,
            KEY_C,
            KEY_D,
            KEY_E,
            KEY_F,
            KEY_G,
            KEY_H,
            KEY_I,
            KEY_J,
            KEY_K,
            KEY_L,
            KEY_M,
            KEY_N,
            KEY_O,
            KEY_P,
            KEY_Q,
            KEY_R,
            KEY_S,
            KEY_T,
            KEY_U,
            KEY_V,
            KEY_W,
            KEY_X,
            KEY_Y,
            KEY_Z,
            KEY_1,
            KEY_2,
            KEY_3,
            KEY_4,
            KEY_5,
            KEY_6,
            KEY_7,
            KEY_8,
            KEY_9,
            KEY_0,
            KEY_ENTER,
            KEY_ESCAPE,
            KEY_BACKSPACE,
            KEY_TAB,
            KEY_SPACE,
            KEY_MINUS,
            KEY_EQUAL,
            KEY_LEFTBRACKET,
            KEY_RIGHTBRACKET,
            KEY_BACKSLASH,
            KEY_NONUS_HASH,
            KEY_SEMICOLON,
            KEY_APOSTROPHE,
            KEY_GRAVE,
            KEY_COMMA,
            KEY_DOT,
            KEY_SLASH,
            KEY_CAPS_LOCK,
            KEY_F1,
            KEY_F2,
            KEY_F3,
            KEY_F4,
            KEY_F5,
            KEY_F6,
            KEY_F7,
            KEY_F8,
            KEY_F9,
            KEY_F10,
            KEY_F11,
            KEY_F12,
            KEY_PRINT_SCREEN,
            KEY_SCROLL_LOCK,
            KEY_PAUSE,
            KEY_INSERT,
            KEY_HOME,
            KEY_PAGE_UP,
            KEY_DELETE,
            KEY_END,
            KEY_PAGE_DOWN,
            KEY_RIGHT,
            KEY_LEFT,
            KEY_DOWN,
            KEY_UP,
            KEY_NUM_LOCK,
            KEY_KP_DIVIDE,
            KEY_KP_MULTIPLY,
            KEY_KP_MINUS,
            KEY_KP_PLUS,
            KEY_KP_ENTER,
            KEY_KP_1,
            KEY_KP_2,
            KEY_KP_3,
            KEY_KP_4,
            KEY_KP_5,
            KEY_KP_6,
            KEY_KP_7,
            KEY_KP_8,
            KEY_KP_9,
            KEY_KP_0,
            KEY_KP_DOT,
            KEY_NONUS_BACKSLASH,
            KEY_APPLICATION,
            KEY_POWER,
            KEY_KP_EQUAL,
            KEY_F13,
            KEY_F14,
            KEY_F15,
            KEY_F16,
            KEY_F17,
            KEY_F18,
            KEY_F19,
            KEY_F20,
            KEY_F21,
            KEY_F22,
            KEY_F23,
            KEY_F24,
            KEY_EXECUTE,
            KEY_HELP,
            KEY_MENU,
            KEY_SELECT,
            KEY_STOP,
            KEY_AGAIN,
            KEY_UNDO,
            KEY_CUT,
            KEY_COPY,
            KEY_PASTE,
            KEY_FIND,
            KEY_MUTE,
            KEY_VOLUME_UP,
            KEY_VOLUME_DOWN,
            KEY_LEFT_CTRL,
            KEY_LEFT_SHIFT,
            KEY_LEFT_ALT,
            KEY_LEFT_META,
            KEY_RIGHT_CTRL,
            KEY_RIGHT_SHIFT,
            KEY_RIGHT_ALT,
            KEY_RIGHT_META,
            CK_BRIGHTNESS_UP,
            CK_BRIGHTNESS_DOWN,
            CK_POWER,
            CK_RESET,
            CK_SLEEP,
            CK_WAKE_UP,
            CK_PLAY,
            CK_PAUSE,
            CK_RECORD,
            CK_FAST_FORWARD,
            CK_REWIND,
            CK_SCAN_NEXT_TRACK,
            CK_SCAN_PREV_TRACK,
            CK_STOP,
            CK_EJECT,
            CK_PLAY_PAUSE,
            CK_MUTE,
            CK_VOLUME_UP,
            CK_VOLUME_DOWN,
        ] {
            assert!(XKey::is_valid_raw(key.raw()), "0x{:04X}", key.raw());
        }
    }
}
