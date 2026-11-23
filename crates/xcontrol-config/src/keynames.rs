//! 语义键名 → XKey wire 值解析（docs/03 §2/§4.1 键名文法，config v3 起）。
//!
//! 映射规则的 trigger/action 用**语义键名**书写（如 `LeftMeta`、`CapsLock`、`C`、
//! `Digit3`）；本模块是把名字解析为 USB HID usage 值的唯一入口——config 校验
//! （加载期报错，字段路径精确到条目）与 core 映射引擎编译（`MappingEngine::new`）
//! 共用同一张表，避免两套文法漂移。
//!
//! # 文法（docs/03 §4.1 修订记录）
//!
//! - 修饰键：全名 `LeftCtrl/LeftShift/LeftAlt/LeftMeta` 及右手对应；简写别名
//!   `LCtrl/LShift/LAlt/LMeta`（v2 遗留）与 mac/win 习惯名 `LOpt/ROpt`（Option=Alt）、
//!   `LWin/RWin`（Win 键=Meta）。规则文法**不含**左右手无关的 `Ctrl/Meta/Super`
//!   裸名（那类名属于热键文法 docs/03 §4.6，映射规则必须左右手分开）。
//! - 字母 `A`–`Z`；数字 `Digit0`–`Digit9`（兼容裸 `0`–`9`）。
//! - 功能键与编辑/导航名（`F1`–`F24`、`CapsLock`、`Escape`、`Enter`、`Tab`、
//!   `Space`、`Backspace`、`Delete`、`Insert`、`Home`、`End`、`PageUp`、`PageDown`、
//!   方向键、`PrintScreen`、`ScrollLock`、`Pause`、`NumLock`、小键盘 `KP*`、
//!   `Application`、行内符号 `Minus/Equal/…/Slash`）。
//! - proto 常量全名（`KEY_A`/`KEY_LEFT_META`/`CK_VOLUME_UP`…）1:1 对应
//!   `xcontrol_proto::xkey` 常量表——媒体键等没有习惯名的键一律走常量名。
//!
//! 名字区分大小写、精确匹配；未知名返回 `None`（调用方按字段路径报错）。
//! 媒体/消费者页键无裸名别名（`Stop`/`Mute` 等与键盘页同名键有歧义，强制
//! `CK_` 前缀消歧，docs/03 §4.4"媒体键 CK_*"）。

/// HID 键盘页修饰键取值范围（XKey 0xE0–0xE7，docs/03 §2；与 proto 一致的本地常量
/// ——config 不依赖 proto，仅判定"名字是否修饰键"用于规则形态分类/校验）。
const MODIFIER_RAW: (u16, u16) = (0xE0, 0xE7);

/// 键名 → wire 值总表（线性查找；仅配置加载/引擎编译路径调用，非热路径）。
static KEY_NAMES: &[(&str, u16)] = &[
    // —— 修饰键（docs/03 §4.1：左右手分开；全名 + v2 简写 + mac/win 习惯名）——
    ("LeftCtrl", 0xE0),
    ("LCtrl", 0xE0),
    ("LeftShift", 0xE1),
    ("LShift", 0xE1),
    ("LeftAlt", 0xE2),
    ("LAlt", 0xE2),
    ("LOpt", 0xE2),
    ("LeftMeta", 0xE3),
    ("LMeta", 0xE3),
    ("LWin", 0xE3),
    ("RightCtrl", 0xE4),
    ("RCtrl", 0xE4),
    ("RightShift", 0xE5),
    ("RShift", 0xE5),
    ("RightAlt", 0xE6),
    ("RAlt", 0xE6),
    ("ROpt", 0xE6),
    ("RightMeta", 0xE7),
    ("RMeta", 0xE7),
    ("RWin", 0xE7),
    // —— 字母（HID 0x04–0x1D）——
    ("A", 0x04),
    ("B", 0x05),
    ("C", 0x06),
    ("D", 0x07),
    ("E", 0x08),
    ("F", 0x09),
    ("G", 0x0A),
    ("H", 0x0B),
    ("I", 0x0C),
    ("J", 0x0D),
    ("K", 0x0E),
    ("L", 0x0F),
    ("M", 0x10),
    ("N", 0x11),
    ("O", 0x12),
    ("P", 0x13),
    ("Q", 0x14),
    ("R", 0x15),
    ("S", 0x16),
    ("T", 0x17),
    ("U", 0x18),
    ("V", 0x19),
    ("W", 0x1A),
    ("X", 0x1B),
    ("Y", 0x1C),
    ("Z", 0x1D),
    // —— 数字行（0x1E–0x27）：Digit 前缀 + 裸数字别名 ——
    ("Digit1", 0x1E),
    ("1", 0x1E),
    ("Digit2", 0x1F),
    ("2", 0x1F),
    ("Digit3", 0x20),
    ("3", 0x20),
    ("Digit4", 0x21),
    ("4", 0x21),
    ("Digit5", 0x22),
    ("5", 0x22),
    ("Digit6", 0x23),
    ("6", 0x23),
    ("Digit7", 0x24),
    ("7", 0x24),
    ("Digit8", 0x25),
    ("8", 0x25),
    ("Digit9", 0x26),
    ("9", 0x26),
    ("Digit0", 0x27),
    ("0", 0x27),
    // —— 控制与符号 ——
    ("Enter", 0x28),
    ("Escape", 0x29),
    ("Backspace", 0x2A),
    ("Tab", 0x2B),
    ("Space", 0x2C),
    ("Minus", 0x2D),
    ("Equal", 0x2E),
    ("LeftBracket", 0x2F),
    ("RightBracket", 0x30),
    ("Backslash", 0x31),
    ("Semicolon", 0x33),
    ("Apostrophe", 0x34),
    ("Grave", 0x35),
    ("Comma", 0x36),
    ("Dot", 0x37),
    ("Slash", 0x38),
    ("CapsLock", 0x39),
    // —— F 区与系统键 ——
    ("F1", 0x3A),
    ("F2", 0x3B),
    ("F3", 0x3C),
    ("F4", 0x3D),
    ("F5", 0x3E),
    ("F6", 0x3F),
    ("F7", 0x40),
    ("F8", 0x41),
    ("F9", 0x42),
    ("F10", 0x43),
    ("F11", 0x44),
    ("F12", 0x45),
    ("PrintScreen", 0x46),
    ("ScrollLock", 0x47),
    ("Pause", 0x48),
    // —— 导航 ——
    ("Insert", 0x49),
    ("Home", 0x4A),
    ("PageUp", 0x4B),
    ("Delete", 0x4C),
    ("End", 0x4D),
    ("PageDown", 0x4E),
    ("Right", 0x4F),
    ("Left", 0x50),
    ("Down", 0x51),
    ("Up", 0x52),
    // —— 小键盘 ——
    ("NumLock", 0x53),
    ("KPDivide", 0x54),
    ("KPMultiply", 0x55),
    ("KPMinus", 0x56),
    ("KPPlus", 0x57),
    ("KPEnter", 0x58),
    ("KP1", 0x59),
    ("KP2", 0x5A),
    ("KP3", 0x5B),
    ("KP4", 0x5C),
    ("KP5", 0x5D),
    ("KP6", 0x5E),
    ("KP7", 0x5F),
    ("KP8", 0x60),
    ("KP9", 0x61),
    ("KP0", 0x62),
    ("KPDot", 0x63),
    ("Application", 0x65),
    ("KPEqual", 0x67),
    ("F13", 0x68),
    ("F14", 0x69),
    ("F15", 0x6A),
    ("F16", 0x6B),
    ("F17", 0x6C),
    ("F18", 0x6D),
    ("F19", 0x6E),
    ("F20", 0x6F),
    ("F21", 0x70),
    ("F22", 0x71),
    ("F23", 0x72),
    ("F24", 0x73),
    // —— proto 常量全名（与 xcontrol_proto::xkey 常量表 1:1；docs/03 §2）——
    ("KEY_A", 0x04),
    ("KEY_B", 0x05),
    ("KEY_C", 0x06),
    ("KEY_D", 0x07),
    ("KEY_E", 0x08),
    ("KEY_F", 0x09),
    ("KEY_G", 0x0A),
    ("KEY_H", 0x0B),
    ("KEY_I", 0x0C),
    ("KEY_J", 0x0D),
    ("KEY_K", 0x0E),
    ("KEY_L", 0x0F),
    ("KEY_M", 0x10),
    ("KEY_N", 0x11),
    ("KEY_O", 0x12),
    ("KEY_P", 0x13),
    ("KEY_Q", 0x14),
    ("KEY_R", 0x15),
    ("KEY_S", 0x16),
    ("KEY_T", 0x17),
    ("KEY_U", 0x18),
    ("KEY_V", 0x19),
    ("KEY_W", 0x1A),
    ("KEY_X", 0x1B),
    ("KEY_Y", 0x1C),
    ("KEY_Z", 0x1D),
    ("KEY_1", 0x1E),
    ("KEY_2", 0x1F),
    ("KEY_3", 0x20),
    ("KEY_4", 0x21),
    ("KEY_5", 0x22),
    ("KEY_6", 0x23),
    ("KEY_7", 0x24),
    ("KEY_8", 0x25),
    ("KEY_9", 0x26),
    ("KEY_0", 0x27),
    ("KEY_ENTER", 0x28),
    ("KEY_ESCAPE", 0x29),
    ("KEY_BACKSPACE", 0x2A),
    ("KEY_TAB", 0x2B),
    ("KEY_SPACE", 0x2C),
    ("KEY_MINUS", 0x2D),
    ("KEY_EQUAL", 0x2E),
    ("KEY_LEFTBRACKET", 0x2F),
    ("KEY_RIGHTBRACKET", 0x30),
    ("KEY_BACKSLASH", 0x31),
    ("KEY_NONUS_HASH", 0x32),
    ("KEY_SEMICOLON", 0x33),
    ("KEY_APOSTROPHE", 0x34),
    ("KEY_GRAVE", 0x35),
    ("KEY_COMMA", 0x36),
    ("KEY_DOT", 0x37),
    ("KEY_SLASH", 0x38),
    ("KEY_CAPS_LOCK", 0x39),
    ("KEY_F1", 0x3A),
    ("KEY_F2", 0x3B),
    ("KEY_F3", 0x3C),
    ("KEY_F4", 0x3D),
    ("KEY_F5", 0x3E),
    ("KEY_F6", 0x3F),
    ("KEY_F7", 0x40),
    ("KEY_F8", 0x41),
    ("KEY_F9", 0x42),
    ("KEY_F10", 0x43),
    ("KEY_F11", 0x44),
    ("KEY_F12", 0x45),
    ("KEY_PRINT_SCREEN", 0x46),
    ("KEY_SCROLL_LOCK", 0x47),
    ("KEY_PAUSE", 0x48),
    ("KEY_INSERT", 0x49),
    ("KEY_HOME", 0x4A),
    ("KEY_PAGE_UP", 0x4B),
    ("KEY_DELETE", 0x4C),
    ("KEY_END", 0x4D),
    ("KEY_PAGE_DOWN", 0x4E),
    ("KEY_RIGHT", 0x4F),
    ("KEY_LEFT", 0x50),
    ("KEY_DOWN", 0x51),
    ("KEY_UP", 0x52),
    ("KEY_NUM_LOCK", 0x53),
    ("KEY_KP_DIVIDE", 0x54),
    ("KEY_KP_MULTIPLY", 0x55),
    ("KEY_KP_MINUS", 0x56),
    ("KEY_KP_PLUS", 0x57),
    ("KEY_KP_ENTER", 0x58),
    ("KEY_KP_1", 0x59),
    ("KEY_KP_2", 0x5A),
    ("KEY_KP_3", 0x5B),
    ("KEY_KP_4", 0x5C),
    ("KEY_KP_5", 0x5D),
    ("KEY_KP_6", 0x5E),
    ("KEY_KP_7", 0x5F),
    ("KEY_KP_8", 0x60),
    ("KEY_KP_9", 0x61),
    ("KEY_KP_0", 0x62),
    ("KEY_KP_DOT", 0x63),
    ("KEY_NONUS_BACKSLASH", 0x64),
    ("KEY_APPLICATION", 0x65),
    ("KEY_POWER", 0x66),
    ("KEY_KP_EQUAL", 0x67),
    ("KEY_F13", 0x68),
    ("KEY_F14", 0x69),
    ("KEY_F15", 0x6A),
    ("KEY_F16", 0x6B),
    ("KEY_F17", 0x6C),
    ("KEY_F18", 0x6D),
    ("KEY_F19", 0x6E),
    ("KEY_F20", 0x6F),
    ("KEY_F21", 0x70),
    ("KEY_F22", 0x71),
    ("KEY_F23", 0x72),
    ("KEY_F24", 0x73),
    ("KEY_EXECUTE", 0x74),
    ("KEY_HELP", 0x75),
    ("KEY_MENU", 0x76),
    ("KEY_SELECT", 0x77),
    ("KEY_STOP", 0x78),
    ("KEY_AGAIN", 0x79),
    ("KEY_UNDO", 0x7A),
    ("KEY_CUT", 0x7B),
    ("KEY_COPY", 0x7C),
    ("KEY_PASTE", 0x7D),
    ("KEY_FIND", 0x7E),
    ("KEY_MUTE", 0x7F),
    ("KEY_VOLUME_UP", 0x80),
    ("KEY_VOLUME_DOWN", 0x81),
    ("KEY_LEFT_CTRL", 0xE0),
    ("KEY_LEFT_SHIFT", 0xE1),
    ("KEY_LEFT_ALT", 0xE2),
    ("KEY_LEFT_META", 0xE3),
    ("KEY_RIGHT_CTRL", 0xE4),
    ("KEY_RIGHT_SHIFT", 0xE5),
    ("KEY_RIGHT_ALT", 0xE6),
    ("KEY_RIGHT_META", 0xE7),
    ("CK_BRIGHTNESS_UP", 0x16F),
    ("CK_BRIGHTNESS_DOWN", 0x170),
    ("CK_POWER", 0x130),
    ("CK_RESET", 0x131),
    ("CK_SLEEP", 0x132),
    ("CK_WAKE_UP", 0x183),
    ("CK_PLAY", 0x1B0),
    ("CK_PAUSE", 0x1B1),
    ("CK_RECORD", 0x1B2),
    ("CK_FAST_FORWARD", 0x1B3),
    ("CK_REWIND", 0x1B4),
    ("CK_SCAN_NEXT_TRACK", 0x1B5),
    ("CK_SCAN_PREV_TRACK", 0x1B6),
    ("CK_STOP", 0x1B7),
    ("CK_EJECT", 0x1B8),
    ("CK_PLAY_PAUSE", 0x1CD),
    ("CK_MUTE", 0x1E2),
    ("CK_VOLUME_UP", 0x1E9),
    ("CK_VOLUME_DOWN", 0x1EA),
];

/// 语义键名 → wire 值（docs/03 §2 XKey 取值）；未知名 `None`。
///
/// 区分大小写、精确匹配；非空白截断不做（调用方传入的应是校验过的字面名）。
#[must_use]
pub fn parse_key_name(name: &str) -> Option<u16> {
    KEY_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, raw)| *raw)
}

/// 名字是否解析为修饰键（规则形态分类/校验用：`trigger.mod`/`action.mod` 只接受修饰键名）。
#[must_use]
pub fn is_modifier_name(name: &str) -> bool {
    parse_key_name(name).is_some_and(|raw| raw >= MODIFIER_RAW.0 && raw <= MODIFIER_RAW.1)
}

/// 名字解析出的 wire 值是否修饰键（migration 分类用）。
#[must_use]
pub const fn raw_is_modifier(raw: u16) -> bool {
    raw >= MODIFIER_RAW.0 && raw <= MODIFIER_RAW.1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/03 §2 钉死的锚点值与 proto 常量一致（表与 xkey.rs 漂移即失败）。
    #[test]
    fn doc_anchor_values_match_proto() {
        for (name, raw) in [
            ("A", 0x04),
            ("KEY_A", 0x04),
            ("Digit3", 0x20),
            ("CapsLock", 0x39),
            ("F24", 0x73),
            ("LeftCtrl", 0xE0),
            ("KEY_LEFT_CTRL", 0xE0),
            ("LMeta", 0xE3),
            ("LeftMeta", 0xE3),
            ("LWin", 0xE3),
            ("RightMeta", 0xE7),
            ("KEY_RIGHT_META", 0xE7),
            ("LOpt", 0xE2),
            ("ROpt", 0xE6),
            ("RWin", 0xE7),
            ("CK_VOLUME_UP", 0x100 + 0xE9),
            ("CK_MUTE", 0x1E2),
        ] {
            assert_eq!(parse_key_name(name), Some(raw), "键名 {name}");
        }
    }

    /// 未知名 / 大小写敏感 / 空白名拒绝。
    #[test]
    fn unknown_or_malformed_names_are_rejected() {
        for name in [
            "", "a", "ctrl", "Ctrl", "META", "Super", "Capslock", "VolumeUp", "Mute", "Stop",
            "KEY_", "CK_", "KEY_A ", " Digit3", "Digit10", "F25", "KPA",
        ] {
            assert_eq!(parse_key_name(name), None, "应拒绝 {name:?}");
        }
    }

    /// 名字表内不重名（同名不同值会让解析结果依赖表序）。
    #[test]
    fn table_names_are_unique() {
        for (index, (name, _)) in KEY_NAMES.iter().enumerate() {
            assert!(
                !KEY_NAMES[..index].iter().any(|(other, _)| other == name),
                "键名重复：{name:?}"
            );
        }
    }

    /// 全表值域合法（键盘页 0x04–0xE7 或消费者页 0x100–0x1FF；指针区无键名）。
    #[test]
    fn table_values_are_in_valid_ranges() {
        for (name, raw) in KEY_NAMES {
            let valid = (0x04..=0xE7).contains(raw) || (0x100..=0x1FF).contains(raw);
            assert!(valid, "键名 {name:?} 值 0x{raw:04X} 越界");
        }
    }

    /// is_modifier_name / raw_is_modifier 与 0xE0–0xE7 范围一致。
    #[test]
    fn modifier_classification() {
        for name in ["LeftCtrl", "RCtrl", "LMeta", "RWin", "KEY_LEFT_ALT", "ROpt"] {
            assert!(is_modifier_name(name), "{name} 应为修饰键名");
        }
        for name in ["A", "CapsLock", "CK_MUTE", "Digit3"] {
            assert!(!is_modifier_name(name), "{name} 非修饰键名");
        }
        assert!(raw_is_modifier(0xE0));
        assert!(raw_is_modifier(0xE7));
        assert!(!raw_is_modifier(0xDF));
        assert!(!raw_is_modifier(0xE8));
        assert!(!raw_is_modifier(0x04));
    }
}
