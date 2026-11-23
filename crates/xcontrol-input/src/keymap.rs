//! macOS `kVK_*` 物理键码 → [`XKey`]（HID usage）静态映射表（docs/03 §2 表 macos 行）。
//!
//! **非门控纯数据区**：本模块零 unsafe、零平台 API，任意平台默认编译——CI 无 macOS
//! 也可全量测试（design.md 模块结构）。
//!
//! # 来源与语义
//!
//! - `kVK_*` 值来自 Apple `HIToolbox/Events.h` 公开常量（框架头文件随 macOS SDK 分发，
//!   数值自 OS X 10.x 起保持稳定）。`core-graphics` crate 只提供 `CGKeyCode`（u16）
//!   类型别名、不含 kVK 常量，故按公开值硬编码（design.md 允许并要求注明来源）。
//! - 转换按**物理键位**而非字符（FR-2.6、docs/03 §1）：ANSI QWERTY 位图，
//!   字符由从机自身布局解释；Dvorak/输入法主控不受影响。
//!
//! # 覆盖范围（M2）
//!
//! 字母 A–Z、数字 0–9、F1–F20、方向键、导航键（Home/End/PgUp/PgDn/ForwardDelete/Help 位）、
//! 修饰键全集（含 CapsLock）、Space/Tab/Return/Escape/Backspace、常用符号键、
//! 小键盘全键（含 Clear→NumLock 对位）、媒体三键（音量加/减/静音）。
//!
//! **有意不映射**（未知 kVK 一律拒绝，调用方丢弃事件）：
//!
//! - `kVK_Function`（0x3F）：HID 键盘页无 Fn 键语义（Fn 是硬件层复合键）；
//! - ISO/JIS 专用键（`kVK_ISO_Section` 0x0A、`kVK_JIS_Yen` 0x5D、`kVK_JIS_Underscore`
//!   0x5E、`kVK_JIS_KeypadComma` 0x5F、`kVK_JIS_Eisu` 0x66、`kVK_JIS_Kana` 0x68）：
//!   HID international/language 区段对应关系待 M3 布局扩展时按 USB HID 官方 usage 表补录
//!   （docs/03 §2：不为没有文档依据的键发明语义；proto/xkey.rs 同一原则）；
//! - 未分配的缝隙值（如 0x0A、0x34、0x36 以外的 0x3F 附近等）。
//!
//! # 注意：F 区 kVK 值不连续
//!
//! kVK_F1–F24 的物理码**非连续**（F1=0x7A、F2=0x78、F3=0x63、F5=0x60、F12=0x6F…，
//! 来自 ADB 时代布局的遗产），锚点测试按真实公开值钉死。
//!
//! # 有意取舍：Help 键 → `KEY_INSERT`
//!
//! `kVK_Help`（0x72）在 Apple 扩展键盘上占据 PC 键盘 Insert 的物理位置
//! （docs/03 §1 物理键位原则）；HID `Help` usage（0x75）在 Windows 应用中几乎无响应，
//! 故映射 `KEY_INSERT`。HID 侧 [`xcontrol_proto::xkey::KEY_HELP`] 因此**无 kVK 对应**
//! （反向表不含），跨平台等价性由从机映射引擎（FR-3.x 预设）处理。

use std::collections::HashMap;
use std::sync::LazyLock;

use xcontrol_proto::xkey::{self, XKey};

/// kVK → XKey 全量表（按 kVK 升序；单数据源，正/反查均由此构建）。
///
/// 数值来源：Apple `HIToolbox/Events.h`（macOS 15 SDK 公开值，注释列 kVK 名）。
pub const KVK_TABLE: &[(u16, XKey)] = &[
    // —— 字母（ANSI 位图顺序，即 Events.h 顺序，非字母表序）——
    (0x00, xkey::KEY_A), // kVK_ANSI_A
    (0x01, xkey::KEY_S), // kVK_ANSI_S
    (0x02, xkey::KEY_D), // kVK_ANSI_D
    (0x03, xkey::KEY_F), // kVK_ANSI_F
    (0x04, xkey::KEY_H), // kVK_ANSI_H
    (0x05, xkey::KEY_G), // kVK_ANSI_G
    (0x06, xkey::KEY_Z), // kVK_ANSI_Z
    (0x07, xkey::KEY_X), // kVK_ANSI_X
    (0x08, xkey::KEY_C), // kVK_ANSI_C
    (0x09, xkey::KEY_V), // kVK_ANSI_V
    (0x0B, xkey::KEY_B), // kVK_ANSI_B
    (0x0C, xkey::KEY_Q), // kVK_ANSI_Q
    (0x0D, xkey::KEY_W), // kVK_ANSI_W
    (0x0E, xkey::KEY_E), // kVK_ANSI_E
    (0x0F, xkey::KEY_R), // kVK_ANSI_R
    (0x10, xkey::KEY_Y), // kVK_ANSI_Y
    (0x11, xkey::KEY_T), // kVK_ANSI_T
    // —— 数字行（Events.h 位图顺序：1 在 0x12、0 在 0x1D、5/6 交错）——
    (0x12, xkey::KEY_1),            // kVK_ANSI_1
    (0x13, xkey::KEY_2),            // kVK_ANSI_2
    (0x14, xkey::KEY_3),            // kVK_ANSI_3
    (0x15, xkey::KEY_4),            // kVK_ANSI_4
    (0x16, xkey::KEY_6),            // kVK_ANSI_6
    (0x17, xkey::KEY_5),            // kVK_ANSI_5
    (0x18, xkey::KEY_EQUAL),        // kVK_ANSI_Equal
    (0x19, xkey::KEY_9),            // kVK_ANSI_9
    (0x1A, xkey::KEY_7),            // kVK_ANSI_7
    (0x1B, xkey::KEY_MINUS),        // kVK_ANSI_Minus
    (0x1C, xkey::KEY_8),            // kVK_ANSI_8
    (0x1D, xkey::KEY_0),            // kVK_ANSI_0
    (0x1E, xkey::KEY_RIGHTBRACKET), // kVK_ANSI_RightBracket
    (0x1F, xkey::KEY_O),            // kVK_ANSI_O
    (0x20, xkey::KEY_U),            // kVK_ANSI_U
    (0x21, xkey::KEY_LEFTBRACKET),  // kVK_ANSI_LeftBracket
    (0x22, xkey::KEY_I),            // kVK_ANSI_I
    (0x23, xkey::KEY_P),            // kVK_ANSI_P
    (0x24, xkey::KEY_ENTER),        // kVK_Return
    (0x25, xkey::KEY_L),            // kVK_ANSI_L
    (0x26, xkey::KEY_J),            // kVK_ANSI_J
    (0x27, xkey::KEY_APOSTROPHE),   // kVK_ANSI_Quote
    (0x28, xkey::KEY_K),            // kVK_ANSI_K
    (0x29, xkey::KEY_SEMICOLON),    // kVK_ANSI_Semicolon
    (0x2A, xkey::KEY_BACKSLASH),    // kVK_ANSI_Backslash
    (0x2B, xkey::KEY_COMMA),        // kVK_ANSI_Comma
    (0x2C, xkey::KEY_SLASH),        // kVK_ANSI_Slash
    (0x2D, xkey::KEY_N),            // kVK_ANSI_N
    (0x2E, xkey::KEY_M),            // kVK_ANSI_M
    (0x2F, xkey::KEY_DOT),          // kVK_ANSI_Period
    // —— 控制键与符号 ——
    (0x30, xkey::KEY_TAB),       // kVK_Tab
    (0x31, xkey::KEY_SPACE),     // kVK_Space
    (0x32, xkey::KEY_GRAVE),     // kVK_ANSI_Grave
    (0x33, xkey::KEY_BACKSPACE), // kVK_Delete（mac 名 Delete = PC Backspace）
    (0x35, xkey::KEY_ESCAPE),    // kVK_Escape
    // —— 修饰键（左右手分开，docs/02 §4.2 mods bit0-7 对应）——
    (0x36, xkey::KEY_RIGHT_META),  // kVK_RightCommand
    (0x37, xkey::KEY_LEFT_META),   // kVK_Command
    (0x38, xkey::KEY_LEFT_SHIFT),  // kVK_Shift
    (0x39, xkey::KEY_CAPS_LOCK),   // kVK_CapsLock（HID 0x39，非 0xE0-0xE7 修饰键区）
    (0x3A, xkey::KEY_LEFT_ALT),    // kVK_Option
    (0x3B, xkey::KEY_LEFT_CTRL),   // kVK_Control
    (0x3C, xkey::KEY_RIGHT_SHIFT), // kVK_RightShift
    (0x3D, xkey::KEY_RIGHT_ALT),   // kVK_RightOption
    (0x3E, xkey::KEY_RIGHT_CTRL),  // kVK_RightControl
    // —— F17 与小键盘（0x40-0x5C 区段，kVK 严格升序混排）——
    (0x40, xkey::KEY_F17),         // kVK_F17
    (0x41, xkey::KEY_KP_DOT),      // kVK_ANSI_KeypadDecimal
    (0x43, xkey::KEY_KP_MULTIPLY), // kVK_ANSI_KeypadMultiply
    (0x45, xkey::KEY_KP_PLUS),     // kVK_ANSI_KeypadPlus
    (0x47, xkey::KEY_NUM_LOCK),    // kVK_ANSI_KeypadClear（对位 NumLock，Karabiner 同判）
    // —— 媒体键（HID 消费者页 0x100 偏移；注意：实体媒体键通常经 NSSystemDefined
    //    事件而非 KeyDown 到达，本表服务注入路径与事件内转换）——
    (0x48, xkey::CK_VOLUME_UP),   // kVK_VolumeUp
    (0x49, xkey::CK_VOLUME_DOWN), // kVK_VolumeDown
    (0x4A, xkey::CK_MUTE),        // kVK_Mute
    (0x4B, xkey::KEY_KP_DIVIDE),  // kVK_ANSI_KeypadDivide
    (0x4C, xkey::KEY_KP_ENTER),   // kVK_ANSI_KeypadEnter
    (0x4E, xkey::KEY_KP_MINUS),   // kVK_ANSI_KeypadMinus
    (0x4F, xkey::KEY_F18),        // kVK_F18
    (0x50, xkey::KEY_F19),        // kVK_F19
    (0x51, xkey::KEY_KP_EQUAL),   // kVK_ANSI_KeypadEquals
    (0x52, xkey::KEY_KP_0),       // kVK_ANSI_Keypad0
    (0x53, xkey::KEY_KP_1),       // kVK_ANSI_Keypad1
    (0x54, xkey::KEY_KP_2),       // kVK_ANSI_Keypad2
    (0x55, xkey::KEY_KP_3),       // kVK_ANSI_Keypad3
    (0x56, xkey::KEY_KP_4),       // kVK_ANSI_Keypad4
    (0x57, xkey::KEY_KP_5),       // kVK_ANSI_Keypad5
    (0x58, xkey::KEY_KP_6),       // kVK_ANSI_Keypad6
    (0x59, xkey::KEY_KP_7),       // kVK_ANSI_Keypad7
    (0x5A, xkey::KEY_F20),        // kVK_F20
    (0x5B, xkey::KEY_KP_8),       // kVK_ANSI_Keypad8
    (0x5C, xkey::KEY_KP_9),       // kVK_ANSI_Keypad9
    // —— F 区剩余（0x60-0x7A，Events.h 公开值，不连续！逐项见模块文档）——
    (0x60, xkey::KEY_F5),  // kVK_F5
    (0x61, xkey::KEY_F6),  // kVK_F6
    (0x62, xkey::KEY_F7),  // kVK_F7
    (0x63, xkey::KEY_F3),  // kVK_F3
    (0x64, xkey::KEY_F8),  // kVK_F8
    (0x65, xkey::KEY_F9),  // kVK_F9
    (0x67, xkey::KEY_F11), // kVK_F11
    (0x69, xkey::KEY_F13), // kVK_F13
    (0x6A, xkey::KEY_F16), // kVK_F16
    (0x6B, xkey::KEY_F14), // kVK_F14
    (0x6D, xkey::KEY_F10), // kVK_F10
    (0x6F, xkey::KEY_F12), // kVK_F12
    (0x71, xkey::KEY_F15), // kVK_F15
    // —— 导航键（docs/03 §2：Home/End/PgUp/PgDn/Del）——
    (0x72, xkey::KEY_INSERT), // kVK_Help（物理位 = PC Insert，取舍见模块文档）
    (0x73, xkey::KEY_HOME),   // kVK_Home
    (0x74, xkey::KEY_PAGE_UP), // kVK_PageUp
    (0x75, xkey::KEY_DELETE), // kVK_ForwardDelete（PC Delete/ForwardDelete）
    (0x76, xkey::KEY_F4),     // kVK_F4
    (0x77, xkey::KEY_END),    // kVK_End
    (0x78, xkey::KEY_F2),     // kVK_F2
    (0x79, xkey::KEY_PAGE_DOWN), // kVK_PageDown
    (0x7A, xkey::KEY_F1),     // kVK_F1
    (0x7B, xkey::KEY_LEFT),   // kVK_LeftArrow
    (0x7C, xkey::KEY_RIGHT),  // kVK_RightArrow
    (0x7D, xkey::KEY_DOWN),   // kVK_DownArrow
    (0x7E, xkey::KEY_UP),     // kVK_UpArrow
];

/// HID → kVK 反查表（懒构建自 [`KVK_TABLE`]；注入路径用）。
///
/// HID 侧无 mac 对应的键（PrintScreen/ScrollLock/Pause/NonUS 系列/F21–F24/
/// 消费者页非媒体键等）不在表内——注入时返回 `None`，调用方按
/// [`crate::SinkError::Failed`] 语义计数（error-handling.md §4：不中断事件流）。
static KVK_BY_XKEY: LazyLock<HashMap<u16, u16>> = LazyLock::new(|| {
    let mut map = HashMap::with_capacity(KVK_TABLE.len());
    for (kvk, key) in KVK_TABLE {
        // 表静态、无重复（tests::table_has_no_duplicates 断言），insert 必不覆盖。
        map.insert(key.raw(), *kvk);
    }
    map
});

/// kVK 物理键码 → [`XKey`]（捕获路径；表外 kVK 返回 `None`——未知键拒绝语义）。
#[must_use]
pub fn kvk_to_xkey(kvk: u16) -> Option<XKey> {
    // 捕获热路径（每次按键一次）：线性扫 112 项 u16 比较 ≈ 百纳秒级，
    // 远低于回调纪律预算（platform-code.md：回调内只做转换与投递）。
    KVK_TABLE
        .iter()
        .find(|(code, _)| *code == kvk)
        .map(|(_, key)| *key)
}

/// [`XKey`] → kVK 物理键码（注入路径；无 mac 对应返回 `None`）。
#[must_use]
pub fn xkey_to_kvk(key: XKey) -> Option<u16> {
    KVK_BY_XKEY.get(&key.raw()).copied()
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::xkey::{
        CK_MUTE, CK_VOLUME_DOWN, CK_VOLUME_UP, KEY_A, KEY_B, KEY_BACKSPACE, KEY_CAPS_LOCK,
        KEY_DELETE, KEY_DOWN, KEY_END, KEY_ESCAPE, KEY_F1, KEY_F2, KEY_F3, KEY_F4, KEY_F5, KEY_F6,
        KEY_F7, KEY_F8, KEY_F9, KEY_F10, KEY_F11, KEY_F12, KEY_F13, KEY_F14, KEY_F15, KEY_F16,
        KEY_F17, KEY_F18, KEY_F19, KEY_F20, KEY_F21, KEY_F24, KEY_GRAVE, KEY_HELP, KEY_HOME,
        KEY_INSERT, KEY_LEFT, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_LEFT_SHIFT,
        KEY_LEFTBRACKET, KEY_MINUS, KEY_NUM_LOCK, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_PAUSE,
        KEY_PRINT_SCREEN, KEY_RIGHT, KEY_RIGHT_ALT, KEY_RIGHT_CTRL, KEY_RIGHT_META,
        KEY_RIGHT_SHIFT, KEY_RIGHTBRACKET, KEY_SCROLL_LOCK, KEY_SPACE, KEY_TAB, KEY_UP,
        KEY_VOLUME_UP,
    };

    use super::*;

    /// 表覆盖量级（≥100 键；抽删键会在此先失败）。
    #[test]
    fn table_size_sanity() {
        assert!(
            KVK_TABLE.len() >= 100,
            "kVK 全量表应 ≥100 项，实际 {}",
            KVK_TABLE.len()
        );
    }

    /// 表按 kVK 升序且无重复键码（反查表构建正确性的前提）。
    #[test]
    fn table_is_sorted_and_unique_by_kvk() {
        for pair in KVK_TABLE.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "表未按 kVK 升序：0x{:02X} >= 0x{:02X}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    /// 表内 HID 无重复（每个 XKey 至多一个 kVK 位）。
    #[test]
    fn table_has_no_duplicate_xkey() {
        let mut seen: Vec<u16> = KVK_TABLE.iter().map(|(_, key)| key.raw()).collect();
        seen.sort_unstable();
        let len = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), len, "XKey 在表内重复");
    }

    /// 锚点：字母（Events.h 非字母表序的真实位图）。
    #[test]
    fn anchor_letters() {
        let expect = [
            (0x00, KEY_A),
            (0x01, xkey::KEY_S),
            (0x02, xkey::KEY_D),
            (0x07, xkey::KEY_X),
            (0x0B, KEY_B),
            (0x11, xkey::KEY_T),
            (0x1F, xkey::KEY_O),
            (0x23, xkey::KEY_P),
            (0x2E, xkey::KEY_M),
        ];
        for (kvk, key) in expect {
            assert_eq!(kvk_to_xkey(kvk), Some(key), "kVK 0x{kvk:02X}");
            assert_eq!(xkey_to_kvk(key), Some(kvk), "{key:?}");
        }
    }

    /// 锚点：数字行（Events.h 位图顺序：1 在 0x12、0 在 0x1D、5/6 交错）。
    #[test]
    fn anchor_digits() {
        let expect = [
            (0x12u16, xkey::KEY_1),
            (0x13, xkey::KEY_2),
            (0x15, xkey::KEY_4),
            (0x16, xkey::KEY_6),
            (0x17, xkey::KEY_5),
            (0x1A, xkey::KEY_7),
            (0x1C, xkey::KEY_8),
            (0x1D, xkey::KEY_0),
        ];
        for (kvk, key) in expect {
            assert_eq!(kvk_to_xkey(kvk), Some(key), "kVK 0x{kvk:02X}");
            assert_eq!(xkey_to_kvk(key), Some(kvk), "{key:?}");
        }
    }

    /// 锚点：修饰键（docs/02 §4.2 bit0-7 对应键；kVK_Control=0x3B 等）。
    #[test]
    fn anchor_modifiers() {
        let expect = [
            (0x36u16, KEY_RIGHT_META),
            (0x37, KEY_LEFT_META),
            (0x38, KEY_LEFT_SHIFT),
            (0x3A, KEY_LEFT_ALT),
            (0x3B, KEY_LEFT_CTRL),
            (0x3C, KEY_RIGHT_SHIFT),
            (0x3D, KEY_RIGHT_ALT),
            (0x3E, KEY_RIGHT_CTRL),
        ];
        for (kvk, key) in expect {
            assert_eq!(kvk_to_xkey(kvk), Some(key), "kVK 0x{kvk:02X}");
            assert!(key.is_modifier(), "{key:?} 应在 0xE0-0xE7 修饰键区");
            assert_eq!(xkey_to_kvk(key), Some(kvk), "{key:?}");
        }
        // CapsLock 是普通键语义（HID 0x39，不在 mods bit0-7 对应区）
        assert_eq!(kvk_to_xkey(0x39), Some(KEY_CAPS_LOCK));
        assert!(!KEY_CAPS_LOCK.is_modifier());
    }

    /// 锚点：F 区非连续真实值（模块文档钉死：F1=0x7A、F2=0x78、F3=0x63、F5=0x60、F12=0x6F）。
    #[test]
    fn anchor_function_keys_non_contiguous() {
        let expect = [
            (0x7Au16, KEY_F1),
            (0x78, KEY_F2),
            (0x63, KEY_F3),
            (0x76, KEY_F4),
            (0x60, KEY_F5),
            (0x61, KEY_F6),
            (0x62, KEY_F7),
            (0x64, KEY_F8),
            (0x65, KEY_F9),
            (0x6D, KEY_F10),
            (0x67, KEY_F11),
            (0x6F, KEY_F12),
            (0x69, KEY_F13),
            (0x6B, KEY_F14),
            (0x71, KEY_F15),
            (0x6A, KEY_F16),
            (0x40, KEY_F17),
            (0x4F, KEY_F18),
            (0x50, KEY_F19),
            (0x5A, KEY_F20),
        ];
        assert_eq!(expect.len(), 20);
        for (kvk, key) in expect {
            assert_eq!(kvk_to_xkey(kvk), Some(key), "kVK 0x{kvk:02X}");
            assert_eq!(xkey_to_kvk(key), Some(kvk), "{key:?}");
        }
    }

    /// 锚点：方向键与导航键。
    #[test]
    fn anchor_navigation() {
        let expect = [
            (0x7Bu16, KEY_LEFT),
            (0x7C, KEY_RIGHT),
            (0x7D, KEY_DOWN),
            (0x7E, KEY_UP),
            (0x73, KEY_HOME),
            (0x77, KEY_END),
            (0x74, KEY_PAGE_UP),
            (0x79, KEY_PAGE_DOWN),
            (0x75, KEY_DELETE),
        ];
        for (kvk, key) in expect {
            assert_eq!(kvk_to_xkey(kvk), Some(key), "kVK 0x{kvk:02X}");
            assert_eq!(xkey_to_kvk(key), Some(kvk), "{key:?}");
        }
    }

    /// 锚点：控制/符号/媒体键。
    #[test]
    fn anchor_control_symbols_media() {
        let expect = [
            (0x24u16, xkey::KEY_ENTER),
            (0x30, KEY_TAB),
            (0x31, KEY_SPACE),
            (0x33, KEY_BACKSPACE),
            (0x35, KEY_ESCAPE),
            (0x32, KEY_GRAVE),
            (0x18, xkey::KEY_EQUAL),
            (0x1B, KEY_MINUS),
            (0x21, KEY_LEFTBRACKET),
            (0x1E, KEY_RIGHTBRACKET),
            (0x48, CK_VOLUME_UP),
            (0x49, CK_VOLUME_DOWN),
            (0x4A, CK_MUTE),
            (0x47, KEY_NUM_LOCK),
            (0x72, KEY_INSERT),
        ];
        for (kvk, key) in expect {
            assert_eq!(kvk_to_xkey(kvk), Some(key), "kVK 0x{kvk:02X}");
            assert_eq!(xkey_to_kvk(key), Some(kvk), "{key:?}");
        }
    }

    /// 未知 kVK 拒绝：ISO/JIS 专用键、Fn、未分配缝隙。
    #[test]
    fn unknown_kvk_rejected() {
        for kvk in [
            0x0Au16, 0x34, 0x3F, 0x5D, 0x5E, 0x5F, 0x66, 0x68, 0x6C, 0x70, 0x7F,
        ] {
            assert!(kvk_to_xkey(kvk).is_none(), "kVK 0x{kvk:02X} 应不映射");
        }
    }

    /// 反向表缺口（有意不映射，见模块文档）：HID 独有键注入时按 Failed 计数。
    #[test]
    fn reverse_gaps_are_intentional() {
        for key in [
            KEY_PRINT_SCREEN,
            KEY_SCROLL_LOCK,
            KEY_PAUSE,
            KEY_HELP,
            xkey::KEY_NONUS_HASH,
            xkey::KEY_NONUS_BACKSLASH,
            xkey::KEY_APPLICATION,
            KEY_F21,
            KEY_F24,
            KEY_VOLUME_UP, // 键盘页音量键无 kVK；mac 侧走消费者页 CK_VOLUME_UP
        ] {
            assert!(xkey_to_kvk(key).is_none(), "{key:?} 应无 kVK 对应");
        }
    }

    /// 全表双向往返恒等（正查 ∘ 反查 = id）。
    #[test]
    fn roundtrip_is_identity() {
        for (kvk, key) in KVK_TABLE {
            assert_eq!(kvk_to_xkey(*kvk), Some(*key), "正查 0x{kvk:02X}");
            assert_eq!(xkey_to_kvk(*key), Some(*kvk), "反查 {key:?}");
        }
    }

    /// 全表 XKey 值域合法（XKey::new 校验语义在 proto 侧，此处冗余断言防表写错区段）。
    #[test]
    fn all_xkeys_in_valid_ranges() {
        for (_, key) in KVK_TABLE {
            assert!(XKey::is_valid_raw(key.raw()), "0x{:04X} 越界", key.raw());
        }
    }
}
