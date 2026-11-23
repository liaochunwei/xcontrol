//! Windows Set-1 扫描码 → [`XKey`]（HID usage）静态映射表（docs/03 §2 表 windows 行）。
//!
//! **非门控纯数据区**：本模块零 unsafe、零平台 API、零 `windows` crate 依赖，任意平台
//! 默认编译——CI 无 Windows 也可全量测试（design.md 模块结构，mac [`crate::keymap`]
//! 先例）。锚点测试跨平台可跑。
//!
//! # 表来源（PRD 要求写明核验方法）
//!
//! 数值来源：Microsoft Learn《[Keyboard Input Overview]》"Scan Codes" 节的官方全表
//! （"the set of Scan Codes as presently recognized by Windows"，HID Usage ↔ Scan 1
//! Make 逐行对应，2025-07 版本页面核实）——该表由 kbdhid/kbdclass 驱动的 HID usage→
//! 扫描码翻译表背书，比社区 QMK 表更权威（Windows 视角的 set-1 归一化值以它为准）。
//! 锚点双重核验：公开 Scan Code Set 1 文档（IBM PS/2 技术参考传承，Andries Brouwer
//! scancodes-1.html 等公开汇总一致）。
//!
//! [Keyboard Input Overview]: https://learn.microsoft.com/en-us/windows/win32/inputdev/about-keyboard-input
//!
//! # 二维键：(扫描码, extended 位)
//!
//! LL hook（[`KBDLLHOOKSTRUCT`]）的 `scanCode` 只含单字节 set-1 码，**E0 前缀不走
//! scanCode 而走 `LLKHF_EXTENDED` 标志**——方向键与主键区同码（如 0x48 既是小键盘 8
//! 又是 E0 扩展的上方向键）、左右修饰键同码（LCtrl=RCtrl=0x1D、LAlt=RAlt=0x38）。
//! 映射表因此按 `(扫描码, extended)` **二维键**区分（design.md 关键点），调用方
//! （`platform/windows.rs` 捕获路径）从 flags 取 extended 位。
//!
//! 官方表钉死的易混键归一化行为（与直觉不同，注释逐条标注）：
//!
//! - **NumLock = (0x45, extended)**，**Pause = (0x45, 非 extended)**——两键共享扫描码
//!   0x45，靠 extended 位区分（官方 Extended-Key Flag 节原文："the *Num Lock* key has
//!   a separate scan code from the *Pause* key (which uses the same scan code without
//!   the extended-key flag)"）。Pause 的原始 set-1 序列是 E1 开头（E1 14 45 …），驱动
//!   归一化为 legacy 消息视角的 0x45 无扩展位（官方表 Note 3）；Ctrl+Pause（Break）
//!   产生 E0 46 → 表中按 (0x46, extended) 归 Pause 语义（官方表 Note 2）。注意官方表
//!   NumLock 行同时列出 0x0045 与 0xE045 两种形态（后者为 kbdhid 现代翻译值）——本表
//!   取 E0 形态为 NumLock、0x45 无扩展位归 Pause；若实机对拍（docs/05 §2.1）发现旧式
//!   键盘/虚拟机把 NumLock 报成无扩展 0x45，按实测结果回改。
//! - **ScrollLock = (0x46, 非 extended)**——与 Break 的 E0 46 同码但无扩展位。
//! - **PrintScreen = (0x37, extended)**（E0 37；与小键盘 * 共用 0x37）；Alt+PrintScreen
//!   产生 SysRq 码 0x54 无扩展位（官方表 Note 1），同归 PrintScreen 语义。
//! - **右 Shift = (0x36, 非 extended)**——官方原文明确"has a separate scan code
//!   (0x36)"、**不**置扩展位（与右 Ctrl/右 Alt 相反）。
//!
//! [`KBDLLHOOKSTRUCT`]: https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct
//!
//! # 覆盖范围（M3）
//!
//! 字母 A–Z、数字 0–9、F1–F24、方向键与编辑导航键、修饰键全集（左右手分开）、
//! Space/Tab/Return/Escape/Backspace、常用符号键、小键盘全键（含 KP /、KP Enter
//! 扩展位与 KP Equals）、Non-US Backslash（0x56）、媒体七键（音量/播放/切歌等）、
//! 电源三键（Power/Sleep/WakeUp）。
//!
//! **有意不映射**（未知扫描码一律拒绝，捕获调用方丢弃事件）：
//!
//! - 国际/语言键（INT1–INT5 = 0x73/0x70/0x7D/0x79/0x7B、INT6 = 0x5C 非扩展形态、
//!   LANG1–LANG4 = 0x72/0x71/0x78/0x77）：XKey 侧尚无国际键常量（proto 留待布局
//!   扩展统一补录，与 mac [`crate::keymap`] 的 ISO/JIS 取舍一致——不为没有常量
//!   语义的键发明映射）；
//! - LANG5 与 F24 同码 0x76（官方表两行同值）：唯一形态归 F24（主键区优先），
//!   JIS 键盘的 LANG5 键会按 F24 上报——语义冲突无解，按消费者页取舍一致处理；
//! - 巴西小键盘逗号（0x7E）：同上（无 `KEY_KP_COMMA` 常量）；
//! - AC 消费键（浏览器前进/后退/搜索等，E0 32/E0 6A–6B…）与 ErrorRollOver（0xFF）：
//!   XKey 消费者页未收录对应 usage（proto 只保留媒体常用子集）。
//!
//! # 反向表（注入路径）
//!
//! [`xkey_to_scan`] 产出 `(扫描码, extended)`，配合 `SendInput` 的
//! `KEYEVENTF_SCANCODE`（+ 扩展键置 `KEYEVENTF_EXTENDEDKEY`）按**物理键位**注入
//! （docs/03 §1：布局无关性——scancode 路径不受从机键盘布局影响）。`KEYEVENTF_UNICODE`
//! 文本路径（docs/03 §4.5）**不经本表**。
//!
//! 同一 XKey 有多个扫描码来源时（PrintScreen 的 E0 37 与 SysRq 0x54、Pause 的
//! 0x45 与 Break 的 E0 46），反向表取**首个（canonical）条目**——canonical 排在
//! [`SCAN_TABLE`] 前部，构建函数按"首见优先"取值。

use std::collections::HashMap;
use std::sync::LazyLock;

use xcontrol_proto::xkey::{self, XKey};

/// Set-1 扫描码（单字节 make 码）+ extended 位 → XKey 全量表（按扫描码升序排布）。
///
/// 数值来源：Microsoft Learn《Keyboard Input Overview》官方扫描码表（模块文档）。
/// `extended = true` 表示原始 set-1 序列带 E0 前缀（LL hook 的 `LLKHF_EXTENDED` 位）。
pub const SCAN_TABLE: &[(u16, bool, XKey)] = &[
    // —— 控制键（0x01–0x0F）——
    (0x01, false, xkey::KEY_ESCAPE),    // Esc
    (0x02, false, xkey::KEY_1),         // 1
    (0x03, false, xkey::KEY_2),         // 2
    (0x04, false, xkey::KEY_3),         // 3
    (0x05, false, xkey::KEY_4),         // 4
    (0x06, false, xkey::KEY_5),         // 5
    (0x07, false, xkey::KEY_6),         // 6
    (0x08, false, xkey::KEY_7),         // 7
    (0x09, false, xkey::KEY_8),         // 8
    (0x0A, false, xkey::KEY_9),         // 9
    (0x0B, false, xkey::KEY_0),         // 0（dispatch 锚点）
    (0x0C, false, xkey::KEY_MINUS),     // - _
    (0x0D, false, xkey::KEY_EQUAL),     // = +
    (0x0E, false, xkey::KEY_BACKSPACE), // Backspace（HID 名 Delete）
    (0x0F, false, xkey::KEY_TAB),       // Tab
    // —— 字母 QWERTY 位图（0x10–0x2C，set-1 与 mac kVK 完全不同的位图，逐项对官方表）——
    (0x10, false, xkey::KEY_Q),
    (0x11, false, xkey::KEY_W),
    (0x12, false, xkey::KEY_E),
    (0x13, false, xkey::KEY_R),
    (0x14, false, xkey::KEY_T),
    (0x15, false, xkey::KEY_Y),
    (0x16, false, xkey::KEY_U),
    (0x17, false, xkey::KEY_I),
    (0x18, false, xkey::KEY_O),
    (0x19, false, xkey::KEY_P),
    (0x1A, false, xkey::KEY_LEFTBRACKET),  // [ {
    (0x1B, false, xkey::KEY_RIGHTBRACKET), // ] }
    (0x1C, false, xkey::KEY_ENTER),        // 主键区 Enter（小键盘 Enter = E0 1C）
    (0x1D, false, xkey::KEY_LEFT_CTRL),    // 左 Ctrl（dispatch 锚点；右 Ctrl = E0 1D）
    (0x1E, false, xkey::KEY_A),            // A（dispatch 锚点）
    (0x1F, false, xkey::KEY_S),
    (0x20, false, xkey::KEY_D),
    (0x21, false, xkey::KEY_F),
    (0x22, false, xkey::KEY_G),
    (0x23, false, xkey::KEY_H),
    (0x24, false, xkey::KEY_J),
    (0x25, false, xkey::KEY_K),
    (0x26, false, xkey::KEY_L),
    (0x27, false, xkey::KEY_SEMICOLON),  // ; :
    (0x28, false, xkey::KEY_APOSTROPHE), // ' "
    (0x29, false, xkey::KEY_GRAVE),      // ` ~
    (0x2A, false, xkey::KEY_LEFT_SHIFT), // 左 Shift（dispatch 锚点）
    (0x2B, false, xkey::KEY_BACKSLASH),  // \ |（ANSI 位；ISO 同物理键见模块文档）
    (0x2C, false, xkey::KEY_Z),
    (0x2D, false, xkey::KEY_X),
    (0x2E, false, xkey::KEY_C),
    (0x2F, false, xkey::KEY_V),
    (0x30, false, xkey::KEY_B),
    (0x31, false, xkey::KEY_N),
    (0x32, false, xkey::KEY_M),
    (0x33, false, xkey::KEY_COMMA),       // , <
    (0x34, false, xkey::KEY_DOT),         // . >
    (0x35, false, xkey::KEY_SLASH),       // / ?（小键盘 / = E0 35）
    (0x36, false, xkey::KEY_RIGHT_SHIFT), // 右 Shift：0x36 无扩展位（官方原文钉死）
    (0x37, false, xkey::KEY_KP_MULTIPLY), // 小键盘 *（PrintScreen = E0 37）
    (0x38, false, xkey::KEY_LEFT_ALT),    // 左 Alt（dispatch 锚点；右 Alt = E0 38）
    (0x39, false, xkey::KEY_SPACE),       // Space
    (0x3A, false, xkey::KEY_CAPS_LOCK),   // CapsLock（HID 0x39，非 0xE0-0xE7 修饰区）
    // —— F 区（0x3B–0x44 连续段 + 0x57/0x58，与 mac kVK 的离散位图不同）——
    (0x3B, false, xkey::KEY_F1),
    (0x3C, false, xkey::KEY_F2),
    (0x3D, false, xkey::KEY_F3),
    (0x3E, false, xkey::KEY_F4),
    (0x3F, false, xkey::KEY_F5),
    (0x40, false, xkey::KEY_F6),
    (0x41, false, xkey::KEY_F7),
    (0x42, false, xkey::KEY_F8),
    (0x43, false, xkey::KEY_F9),
    (0x44, false, xkey::KEY_F10),
    (0x45, false, xkey::KEY_PAUSE), // Pause（驱动归一化 legacy 视角 0x45 无扩展位）
    (0x45, true, xkey::KEY_NUM_LOCK), // NumLock：与 Pause 同码靠扩展位区分（模块文档）
    (0x46, false, xkey::KEY_SCROLL_LOCK), // ScrollLock（Ctrl+Pause Break = E0 46 在表尾别名块）
    (0x47, false, xkey::KEY_KP_7),  // 小键盘 7/Home
    (0x48, false, xkey::KEY_KP_8),  // 小键盘 8/↑（上方向键 = E0 48）
    (0x49, false, xkey::KEY_KP_9),  // 小键盘 9/PgUp
    (0x4A, false, xkey::KEY_KP_MINUS), // 小键盘 -
    (0x4B, false, xkey::KEY_KP_4),  // 小键盘 4/←（左方向键 = E0 4B）
    (0x4C, false, xkey::KEY_KP_5),  // 小键盘 5
    (0x4D, false, xkey::KEY_KP_6),  // 小键盘 6/→（右方向键 = E0 4D）
    (0x4E, false, xkey::KEY_KP_PLUS), // 小键盘 +
    (0x4F, false, xkey::KEY_KP_1),  // 小键盘 1/End
    (0x50, false, xkey::KEY_KP_2),  // 小键盘 2/↓（下方向键 = E0 50）
    (0x51, false, xkey::KEY_KP_3),  // 小键盘 3/PgDn
    (0x52, false, xkey::KEY_KP_0),  // 小键盘 0/Insert
    (0x53, false, xkey::KEY_KP_DOT), // 小键盘 ./Delete
    (0x56, false, xkey::KEY_NONUS_BACKSLASH), // ISO 102 键（官方表 HID 0x64 → 0x56）
    (0x57, false, xkey::KEY_F11),
    (0x58, false, xkey::KEY_F12),
    (0x59, false, xkey::KEY_KP_EQUAL), // 小键盘 =（官方表 0x59）
    // —— E0 扩展键（导航簇 / 右修饰 / Win 键 / 方向键）——
    (0x1C, true, xkey::KEY_KP_ENTER),     // 小键盘 Enter（E0 1C）
    (0x1D, true, xkey::KEY_RIGHT_CTRL),   // 右 Ctrl（E0 1D）
    (0x35, true, xkey::KEY_KP_DIVIDE),    // 小键盘 /（E0 35）
    (0x37, true, xkey::KEY_PRINT_SCREEN), // PrintScreen（E0 37，canonical 反向条目）
    (0x38, true, xkey::KEY_RIGHT_ALT),    // 右 Alt（E0 38）
    (0x47, true, xkey::KEY_HOME),         // Home（E0 47）
    (0x48, true, xkey::KEY_UP),           // ↑（E0 48，dispatch 锚点）
    (0x49, true, xkey::KEY_PAGE_UP),      // PgUp（E0 49）
    (0x4B, true, xkey::KEY_LEFT),         // ←（E0 4B，dispatch 锚点）
    (0x4D, true, xkey::KEY_RIGHT),        // →（E0 4D，dispatch 锚点）
    (0x4F, true, xkey::KEY_END),          // End（E0 4F）
    (0x50, true, xkey::KEY_DOWN),         // ↓（E0 50，dispatch 锚点）
    (0x51, true, xkey::KEY_PAGE_DOWN),    // PgDn（E0 51）
    (0x52, true, xkey::KEY_INSERT),       // Insert（E0 52）
    (0x53, true, xkey::KEY_DELETE),       // ForwardDelete（E0 53）
    (0x5B, true, xkey::KEY_LEFT_META),    // 左 Win（E0 5B）
    (0x5C, true, xkey::KEY_RIGHT_META),   // 右 Win（E0 5C，dispatch 锚点）
    (0x5D, true, xkey::KEY_APPLICATION),  // Application/Menu（E0 5D）
    // —— F13–F24（官方表 0x64–0x6E 连续段 + 0x76；mac 表只覆盖 F17–F20）——
    (0x64, false, xkey::KEY_F13),
    (0x65, false, xkey::KEY_F14),
    (0x66, false, xkey::KEY_F15),
    (0x67, false, xkey::KEY_F16),
    (0x68, false, xkey::KEY_F17),
    (0x69, false, xkey::KEY_F18),
    (0x6A, false, xkey::KEY_F19),
    (0x6B, false, xkey::KEY_F20),
    (0x6C, false, xkey::KEY_F21),
    (0x6D, false, xkey::KEY_F22),
    (0x6E, false, xkey::KEY_F23),
    (0x76, false, xkey::KEY_F24),
    // —— 媒体/消费键（E0 前缀；HID 消费者页 usage 与 CK_* 常量 0x100 偏移一致）——
    (0x10, true, xkey::CK_SCAN_PREV_TRACK), // Scan Previous Track（usage 0xB6）
    (0x19, true, xkey::CK_SCAN_NEXT_TRACK), // Scan Next Track（usage 0xB5）
    (0x20, true, xkey::CK_MUTE),            // Mute（usage 0xE2）
    (0x22, true, xkey::CK_PLAY_PAUSE),      // Play/Pause（usage 0xCD）
    (0x24, true, xkey::CK_STOP),            // Stop（usage 0xB7）
    (0x2E, true, xkey::CK_VOLUME_DOWN),     // Volume Decrement（usage 0xEA）
    (0x30, true, xkey::CK_VOLUME_UP),       // Volume Increment（usage 0xE9）
    // —— 电源键（官方表 GenericDesktop/Keyboard 行；XKey 侧对位见各常量文档）——
    (0x5E, true, xkey::KEY_POWER), // Keyboard Power（usage 0x66 → E0 5E）
    (0x5F, true, xkey::CK_SLEEP),  // System Sleep（consumer usage 0x32 → CK_SLEEP）
    (0x63, true, xkey::CK_WAKE_UP), // System Wake Up（consumer usage 0x83 → CK_WAKE_UP）
    // —— 非 canonical 别名（**只服务捕获方向**；反查表首见优先取上方 canonical 位，
    //    本块必须保持在表尾——见模块文档与 reverse_prefers_canonical_entries 测试）——
    (0x46, true, xkey::KEY_PAUSE), // Ctrl+Pause = Break 序列 E0 46（官方表 Note 2）
    (0x54, false, xkey::KEY_PRINT_SCREEN), // SysRq（Alt+PrintScreen 产生，官方表 Note 1）
];

/// `(扫描码, extended)` → [`XKey`]（捕获路径；表外扫描码返回 `None`——未知键拒绝语义）。
#[must_use]
pub fn scan_to_xkey(scan: u16, extended: bool) -> Option<XKey> {
    // 捕获热路径（每次按键一次）：线性扫 ~120 项 u16+bool 比较 ≈ 百纳秒级，
    // 远低于 LL hook 回调预算（LowLevelHooksTimeout；platform-code.md 回调纪律）。
    SCAN_TABLE
        .iter()
        .find(|(code, ext, _)| *code == scan && *ext == extended)
        .map(|(_, _, key)| *key)
}

/// HID → (扫描码, extended) 反查表（懒构建自 [`SCAN_TABLE`]；`SendInput` 注入路径用）。
///
/// 同一 XKey 多条目时取**首个**（canonical）——非 canonical 条目（SysRq 0x54、
/// Break E0 46）只服务捕获方向。表静态无 canonical 重复（tests 断言首见优先唯一性）。
static SCAN_BY_XKEY: LazyLock<HashMap<u16, (u16, bool)>> = LazyLock::new(|| {
    let mut map = HashMap::with_capacity(SCAN_TABLE.len());
    for (scan, ext, key) in SCAN_TABLE {
        // 非重复 XKey 的 canonical 条目（多来源键首见即 canonical，见模块文档）。
        map.entry(key.raw()).or_insert((*scan, *ext));
    }
    map
});

/// [`XKey`] → `(扫描码, extended)`（注入路径；无 Windows 对应返回 `None`）。
///
/// 扩展键注入时扫描码取**不带 E0 前缀的单字节**，扩展语义由 `SendInput` 的
/// `KEYEVENTF_EXTENDEDKEY` 标志承载（系统按该标志合成 E0 前缀）。
#[must_use]
pub fn xkey_to_scan(key: XKey) -> Option<(u16, bool)> {
    SCAN_BY_XKEY.get(&key.raw()).copied()
}

/// [`XKey`]（HID usage）→ Windows 虚拟键码（M4-hotkeys：`RegisterHotKey` 的
/// `vk` 形参，docs/05 §2 热键行）。无 VK 语义的键返回 `None`（注册期报错
/// 降级——热键停用，不中断会话）。
///
/// # 覆盖范围（M4）
///
/// 热键主键的现实取值域：字母/数字、F1–F24、导航键、符号键（VK_OEM 系）、
/// Space/Tab/Return/Escape/Backspace、CapsLock、小键盘全键、Non-US Backslash
/// （VK_OEM_102）、Application（VK_APPS）。
///
/// # 有意不映射（`None`）
///
/// - **消费者页媒体键**（`CK_*`）：虚拟键码体系无对应项（`RegisterHotKey` 无法
///   注册媒体键——那是 LL hook/事件流识别的领域，本表不越界发明映射）；
/// - **修饰键**：`RegisterHotKey` 的修饰键走 `MOD_*` 掩码（家族语义，无左右手
///   区分——与 core `ModFamilies` 的「任一侧」匹配同构），0xE0–0xE7 不作主键
///   （config 校验层已拒绝）；
/// - PrintScreen/ScrollLock/Pause（0x46–0x48）：系统保留键（热键注册现实不可用
///   ——PrintScreen 被截图管线消费），且无稳定 VK 对应；
/// - KEY_KP_ENTER：与 Return 同 VK 0x0D 且 `RegisterHotKey` 无 extended 位形参
///   （扩展语义无法表达，注册二者会互相覆盖）——按主键区 Return 语义跳过。
///
/// # 表来源
///
/// VK 数值 = WinUser.h 公开定义（`VK_A`=0x41 起字母连续段、`VK_F1`=0x70 起
/// F 区连续段、`VK_NUMPAD0`=0x60 起小键盘连续段、VK_OEM_* 零散段），与
/// Microsoft Learn《Virtual-Key Codes》页一致（0.62 `VK_*` 常量同源锚点见测试）。
#[must_use]
pub fn xkey_to_vk(key: XKey) -> Option<u32> {
    let raw = u32::from(key.raw());
    Some(match raw {
        // 字母 A–Z（VK 0x41–0x5A 连续段）
        0x04..=0x1D => raw - 0x04 + 0x41,
        // 数字行：1–9（VK 0x31–0x39）+ 0（VK 0x30 在段尾）
        0x1E..=0x26 => raw - 0x1E + 0x31,
        0x27 => 0x30,
        // 控制键
        0x28 => 0x0D, // Enter → VK_RETURN
        0x29 => 0x1B, // Escape → VK_ESCAPE
        0x2A => 0x08, // Backspace → VK_BACK
        0x2B => 0x09, // Tab → VK_TAB
        0x2C => 0x20, // Space → VK_SPACE
        // 符号键（VK_OEM 系，US 键位约定）
        0x2D => 0xBD, // Minus → VK_OEM_MINUS
        0x2E => 0xBB, // Equal → VK_OEM_PLUS
        0x2F => 0xDB, // LeftBracket → VK_OEM_4
        0x30 => 0xDD, // RightBracket → VK_OEM_6
        0x31 => 0xDC, // Backslash → VK_OEM_5
        0x33 => 0xBA, // Semicolon → VK_OEM_1
        0x34 => 0xDE, // Apostrophe → VK_OEM_7
        0x35 => 0xC0, // Grave → VK_OEM_3
        0x36 => 0xBC, // Comma → VK_OEM_COMMA
        0x37 => 0xBE, // Dot → VK_OEM_PERIOD
        0x38 => 0xBF, // Slash → VK_OEM_2
        0x39 => 0x14, // CapsLock → VK_CAPITAL
        // F1–F12（VK 0x70–0x7B 连续段）
        0x3A..=0x45 => raw - 0x3A + 0x70,
        // 导航键（编辑/方向）
        0x49 => 0x2D, // Insert → VK_INSERT
        0x4A => 0x24, // Home → VK_HOME
        0x4B => 0x21, // PageUp → VK_PRIOR
        0x4C => 0x2E, // Delete → VK_DELETE
        0x4D => 0x23, // End → VK_END
        0x4E => 0x22, // PageDown → VK_NEXT
        0x4F => 0x27, // Right → VK_RIGHT
        0x50 => 0x25, // Left → VK_LEFT
        0x51 => 0x28, // Down → VK_DOWN
        0x52 => 0x26, // Up → VK_UP
        // 小键盘（VK_NUMPAD0=0x60 起；运算键 VK_DIVIDE 系）
        0x53 => 0x90,                     // NumLock → VK_NUMLOCK
        0x54 => 0x6F,                     // KP_Divide → VK_DIVIDE
        0x55 => 0x6A,                     // KP_Multiply → VK_MULTIPLY
        0x56 => 0x6D,                     // KP_Minus → VK_SUBTRACT
        0x57 => 0x6B,                     // KP_Plus → VK_ADD
        0x59..=0x61 => raw - 0x59 + 0x61, // KP_1..KP_9 → VK_NUMPAD1..9
        0x62 => 0x60,                     // KP_0 → VK_NUMPAD0
        0x63 => 0x6E,                     // KP_Dot → VK_DECIMAL
        0x64 => 0xE2,                     // NonUSBackslash → VK_OEM_102
        0x65 => 0x5D,                     // Application → VK_APPS
        // F13–F24（VK 0x7C–0x87 连续段）
        0x68..=0x73 => raw - 0x68 + 0x7C,
        // 修饰键（MOD_* 掩码域）、PrintScreen/ScrollLock/Pause、KP_ENTER、
        // KP_EQUAL、CK_* 消费键等：无热键主键 VK 语义 → None
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::xkey::{
        CK_MUTE, CK_PLAY_PAUSE, CK_SCAN_NEXT_TRACK, CK_SCAN_PREV_TRACK, CK_SLEEP, CK_STOP,
        CK_VOLUME_DOWN, CK_VOLUME_UP, CK_WAKE_UP, KEY_A, KEY_APPLICATION, KEY_B, KEY_BACKSLASH,
        KEY_BACKSPACE, KEY_CAPS_LOCK, KEY_COMMA, KEY_DELETE, KEY_DOT, KEY_DOWN, KEY_END, KEY_ENTER,
        KEY_EQUAL, KEY_ESCAPE, KEY_F1, KEY_F2, KEY_F3, KEY_F4, KEY_F5, KEY_F6, KEY_F7, KEY_F8,
        KEY_F9, KEY_F10, KEY_F11, KEY_F12, KEY_F13, KEY_F14, KEY_F15, KEY_F16, KEY_F17, KEY_F18,
        KEY_F19, KEY_F20, KEY_F21, KEY_F22, KEY_F23, KEY_F24, KEY_GRAVE, KEY_HOME, KEY_INSERT,
        KEY_KP_0, KEY_KP_1, KEY_KP_2, KEY_KP_3, KEY_KP_4, KEY_KP_5, KEY_KP_6, KEY_KP_7, KEY_KP_8,
        KEY_KP_9, KEY_KP_DIVIDE, KEY_KP_DOT, KEY_KP_ENTER, KEY_KP_EQUAL, KEY_KP_MINUS,
        KEY_KP_MULTIPLY, KEY_KP_PLUS, KEY_LEFT, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META,
        KEY_LEFT_SHIFT, KEY_LEFTBRACKET, KEY_MINUS, KEY_NONUS_BACKSLASH, KEY_NUM_LOCK,
        KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_PAUSE, KEY_POWER, KEY_PRINT_SCREEN, KEY_RIGHT,
        KEY_RIGHT_ALT, KEY_RIGHT_CTRL, KEY_RIGHT_META, KEY_RIGHT_SHIFT, KEY_RIGHTBRACKET,
        KEY_SCROLL_LOCK, KEY_SEMICOLON, KEY_SLASH, KEY_SPACE, KEY_TAB, KEY_UP,
    };

    use super::*;

    /// 表覆盖量级（≥115 项；抽删键会在此先失败）。
    #[test]
    fn table_size_sanity() {
        assert!(
            SCAN_TABLE.len() >= 115,
            "Windows set-1 全量表应 ≥115 项，实际 {}",
            SCAN_TABLE.len()
        );
    }

    /// (扫描码, extended) 二维键无重复（捕获查表唯一性的前提）。
    #[test]
    fn table_has_no_duplicate_keys() {
        let mut seen: Vec<(u16, bool)> = SCAN_TABLE.iter().map(|(s, e, _)| (*s, *e)).collect();
        seen.sort_unstable();
        let len = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), len, "(扫描码, extended) 键在表内重复");
    }

    /// 表内 HID 无重复 canonical 条目（每个 XKey 至多一个注入位；非 canonical 多来源
    /// 仅限模块文档钉死的三键：PrintScreen/Pause 的 SysRq 与 Break 码）。
    #[test]
    fn table_has_no_duplicate_xkey_beyond_documented_sources() {
        let mut seen: Vec<u16> = SCAN_TABLE.iter().map(|(_, _, key)| key.raw()).collect();
        seen.sort_unstable();
        let len = seen.len();
        seen.dedup();
        // 2 个非 canonical 条目：SysRq(0x54) 与 Ctrl+Pause Break(E0 46)。
        assert_eq!(
            len - seen.len(),
            2,
            "多来源键应为模块文档钉死的 PrintScreen/Pause 两键"
        );
    }

    /// 反查首见优先：PrintScreen 取 E0 37（非 SysRq 0x54）、Pause 取 0x45（非 Break E0 46）。
    #[test]
    fn reverse_prefers_canonical_entries() {
        assert_eq!(xkey_to_scan(KEY_PRINT_SCREEN), Some((0x37, true)));
        assert_eq!(xkey_to_scan(KEY_PAUSE), Some((0x45, false)));
        assert_eq!(xkey_to_scan(KEY_SCROLL_LOCK), Some((0x46, false)));
        assert_eq!(xkey_to_scan(KEY_NUM_LOCK), Some((0x45, true)));
    }

    /// 锚点：字母（set-1 QWERTY 位图，dispatch 锚点 A=0x1E 起逐项）。
    #[test]
    fn anchor_letters() {
        let expect = [
            (0x10u16, false, xkey::KEY_Q),
            (0x11, false, xkey::KEY_W),
            (0x12, false, xkey::KEY_E),
            (0x1E, false, KEY_A), // dispatch 锚点
            (0x1F, false, xkey::KEY_S),
            (0x20, false, xkey::KEY_D),
            (0x2C, false, xkey::KEY_Z),
            (0x2D, false, xkey::KEY_X),
            (0x30, false, KEY_B),
            (0x32, false, xkey::KEY_M),
        ];
        for (scan, ext, key) in expect {
            assert_eq!(scan_to_xkey(scan, ext), Some(key), "scan 0x{scan:02X}");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
    }

    /// 锚点：数字行（0x02–0x0B，dispatch 锚点 0=0x0B；8=0x09、9=0x0A）。
    #[test]
    fn anchor_digits() {
        let expect = [
            (0x02u16, false, xkey::KEY_1),
            (0x03, false, xkey::KEY_2),
            (0x09, false, xkey::KEY_8),
            (0x0A, false, xkey::KEY_9),
            (0x0B, false, xkey::KEY_0), // dispatch 锚点
        ];
        for (scan, ext, key) in expect {
            assert_eq!(scan_to_xkey(scan, ext), Some(key), "scan 0x{scan:02X}");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
    }

    /// 锚点：修饰键（dispatch 锚点 LCtrl=0x1D / LShift=0x2A / LAlt=0x38；右修饰扩展位
    /// 与官方 Extended-Key Flag 节逐条对应——右 Shift 例外为非扩展 0x36）。
    #[test]
    fn anchor_modifiers() {
        let expect = [
            (0x1Du16, false, KEY_LEFT_CTRL),
            (0x1D, true, KEY_RIGHT_CTRL),
            (0x2A, false, KEY_LEFT_SHIFT),
            (0x36, false, KEY_RIGHT_SHIFT),
            (0x38, false, KEY_LEFT_ALT),
            (0x38, true, KEY_RIGHT_ALT),
            (0x5B, true, KEY_LEFT_META),
            (0x5C, true, KEY_RIGHT_META),
        ];
        for (scan, ext, key) in expect {
            assert_eq!(
                scan_to_xkey(scan, ext),
                Some(key),
                "scan 0x{scan:02X} ext={ext}"
            );
            assert!(key.is_modifier(), "{key:?} 应在 0xE0-0xE7 修饰键区");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
        // CapsLock 是普通键语义（HID 0x39，0x3A 为 Windows 扫描码）
        assert_eq!(scan_to_xkey(0x3A, false), Some(KEY_CAPS_LOCK));
        assert!(!KEY_CAPS_LOCK.is_modifier());
    }

    /// 锚点：方向键与导航簇（dispatch 锚点 E0 0x48/0x50/0x4B/0x4D；全部扩展位）。
    #[test]
    fn anchor_navigation_extended() {
        let expect = [
            (0x48u16, true, KEY_UP),
            (0x50, true, KEY_DOWN),
            (0x4B, true, KEY_LEFT),
            (0x4D, true, KEY_RIGHT),
            (0x47, true, KEY_HOME),
            (0x4F, true, KEY_END),
            (0x49, true, KEY_PAGE_UP),
            (0x51, true, KEY_PAGE_DOWN),
            (0x52, true, KEY_INSERT),
            (0x53, true, KEY_DELETE),
        ];
        for (scan, ext, key) in expect {
            assert_eq!(scan_to_xkey(scan, ext), Some(key), "scan 0x{scan:02X}");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
    }

    /// 锚点：RWin E0 0x5C（dispatch 锚点）与 Application 键。
    #[test]
    fn anchor_win_keys() {
        assert_eq!(scan_to_xkey(0x5C, true), Some(KEY_RIGHT_META));
        assert_eq!(scan_to_xkey(0x5B, true), Some(KEY_LEFT_META));
        assert_eq!(scan_to_xkey(0x5D, true), Some(KEY_APPLICATION));
        // 同码非扩展位是主键区键（Win 键无混淆来源，但确认 extended 位不可少）
        assert_eq!(scan_to_xkey(0x5C, false), None);
    }

    /// 锚点：小键盘全键（NumLock 与 Pause 同码 0x45 二维区分；KP / 与 KP Enter 扩展位）。
    #[test]
    fn anchor_numpad() {
        let expect = [
            (0x47u16, false, KEY_KP_7),
            (0x48, false, KEY_KP_8),
            (0x49, false, KEY_KP_9),
            (0x4B, false, KEY_KP_4),
            (0x4C, false, KEY_KP_5),
            (0x4D, false, KEY_KP_6),
            (0x4F, false, KEY_KP_1),
            (0x50, false, KEY_KP_2),
            (0x51, false, KEY_KP_3),
            (0x52, false, KEY_KP_0),
            (0x53, false, KEY_KP_DOT),
            (0x37, false, KEY_KP_MULTIPLY),
            (0x4A, false, KEY_KP_MINUS),
            (0x4E, false, KEY_KP_PLUS),
            (0x59, false, KEY_KP_EQUAL),
            (0x35, true, KEY_KP_DIVIDE),
            (0x1C, true, KEY_KP_ENTER),
        ];
        for (scan, ext, key) in expect {
            assert_eq!(scan_to_xkey(scan, ext), Some(key), "scan 0x{scan:02X}");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
        // 小键盘键与主键区同码时靠 extended 位分流（0x47 非 extended = KP7，扩展 = Home）
        assert_eq!(scan_to_xkey(0x47, true), Some(KEY_HOME));
        assert_eq!(scan_to_xkey(0x47, false), Some(KEY_KP_7));
    }

    /// 锚点：官方表钉死的易混键（模块文档四条归一化行为逐条钉死）。
    #[test]
    fn anchor_ambiguous_keys_from_official_table() {
        // NumLock/Pause 同码 0x45 靠 extended 位区分（官方 Extended-Key Flag 节原文）
        assert_eq!(scan_to_xkey(0x45, false), Some(KEY_PAUSE));
        assert_eq!(scan_to_xkey(0x45, true), Some(KEY_NUM_LOCK));
        // ScrollLock 0x46 无扩展位；Ctrl+Pause = Break = E0 46 归 Pause 语义
        assert_eq!(scan_to_xkey(0x46, false), Some(KEY_SCROLL_LOCK));
        assert_eq!(scan_to_xkey(0x46, true), Some(KEY_PAUSE));
        // PrintScreen E0 37（与小键盘 * 同码）；SysRq 0x54（Alt+PrintScreen）
        assert_eq!(scan_to_xkey(0x37, true), Some(KEY_PRINT_SCREEN));
        assert_eq!(scan_to_xkey(0x37, false), Some(KEY_KP_MULTIPLY));
        assert_eq!(scan_to_xkey(0x54, false), Some(KEY_PRINT_SCREEN));
    }

    /// 锚点：控制/符号键（Esc=0x01、符号区、Space/Tab/Enter/Backspace）。
    #[test]
    fn anchor_control_symbols() {
        let expect = [
            (0x01u16, false, KEY_ESCAPE), // dispatch 锚点
            (0x0C, false, KEY_MINUS),
            (0x0D, false, KEY_EQUAL),
            (0x0E, false, KEY_BACKSPACE),
            (0x0F, false, KEY_TAB),
            (0x1A, false, KEY_LEFTBRACKET),
            (0x1B, false, KEY_RIGHTBRACKET),
            (0x1C, false, KEY_ENTER),
            (0x27, false, KEY_SEMICOLON),
            (0x28, false, xkey::KEY_APOSTROPHE),
            (0x29, false, KEY_GRAVE),
            (0x2B, false, KEY_BACKSLASH),
            (0x33, false, KEY_COMMA),
            (0x34, false, KEY_DOT),
            (0x35, false, KEY_SLASH),
            (0x39, false, KEY_SPACE),
            (0x56, false, KEY_NONUS_BACKSLASH),
        ];
        for (scan, ext, key) in expect {
            assert_eq!(scan_to_xkey(scan, ext), Some(key), "scan 0x{scan:02X}");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
    }

    /// 锚点：F 区（0x3B–0x44 连续 + F11/F12 = 0x57/0x58 + F13–F24）。
    #[test]
    fn anchor_function_keys() {
        let expect = [
            (0x3Bu16, false, KEY_F1),
            (0x3C, false, KEY_F2),
            (0x3D, false, KEY_F3),
            (0x3E, false, KEY_F4),
            (0x3F, false, KEY_F5),
            (0x40, false, KEY_F6),
            (0x41, false, KEY_F7),
            (0x42, false, KEY_F8),
            (0x43, false, KEY_F9),
            (0x44, false, KEY_F10),
            (0x57, false, KEY_F11),
            (0x58, false, KEY_F12),
            (0x64, false, KEY_F13),
            (0x65, false, KEY_F14),
            (0x66, false, KEY_F15),
            (0x67, false, KEY_F16),
            (0x68, false, KEY_F17),
            (0x69, false, KEY_F18),
            (0x6A, false, KEY_F19),
            (0x6B, false, KEY_F20),
            (0x6C, false, KEY_F21),
            (0x6D, false, KEY_F22),
            (0x6E, false, KEY_F23),
            (0x76, false, KEY_F24),
        ];
        assert_eq!(expect.len(), 24);
        for (scan, ext, key) in expect {
            assert_eq!(scan_to_xkey(scan, ext), Some(key), "scan 0x{scan:02X}");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
    }

    /// 锚点：媒体/电源键（E0 前缀消费键；CK_* 常量 = 0x100 + HID 消费者页 usage）。
    #[test]
    fn anchor_media_power_keys() {
        let expect = [
            (0x10u16, true, CK_SCAN_PREV_TRACK), // usage 0xB6
            (0x19, true, CK_SCAN_NEXT_TRACK),    // usage 0xB5
            (0x20, true, CK_MUTE),               // usage 0xE2
            (0x22, true, CK_PLAY_PAUSE),         // usage 0xCD
            (0x24, true, CK_STOP),               // usage 0xB7
            (0x2E, true, CK_VOLUME_DOWN),        // usage 0xEA
            (0x30, true, CK_VOLUME_UP),          // usage 0xE9
            (0x5E, true, KEY_POWER),
            (0x5F, true, CK_SLEEP),
            (0x63, true, CK_WAKE_UP),
        ];
        for (scan, ext, key) in expect {
            assert_eq!(scan_to_xkey(scan, ext), Some(key), "scan 0x{scan:02X}");
            assert_eq!(xkey_to_scan(key), Some((scan, ext)), "{key:?}");
        }
        // 与主键区同码的媒体键靠 extended 位分流（0x19 非 extended = P 键，扩展 = 切歌）
        assert_eq!(scan_to_xkey(0x19, false), Some(xkey::KEY_P));
        assert_eq!(scan_to_xkey(0x19, true), Some(CK_SCAN_NEXT_TRACK));
    }

    /// 未知扫描码拒绝：缝隙值、国际键（模块文档有意不映射）、ErrorRollOver。
    #[test]
    fn unknown_scan_rejected() {
        for (scan, ext) in [
            (0x00u16, false),
            (0x55, false),
            (0x5A, false),
            (0x60, false),
            (0x70, false), // INT2（模块文档）
            (0x73, false), // INT1（模块文档）
            (0x7D, false), // INT3（模块文档）
            (0x7E, false), // 小键盘逗号（模块文档）
            (0xFF, false), // ErrorRollOver
            (0x36, true),  // 右 Shift 无扩展位（官方钉死，扩展位形式不存在）
            (0x5F, false), // Sleep 只有 E0 形式
            (0x63, false), // WakeUp 只有 E0 形式
        ] {
            assert!(
                scan_to_xkey(scan, ext).is_none(),
                "scan 0x{scan:02X} ext={ext} 应不映射"
            );
        }
    }

    /// 反向表缺口（有意不映射，见模块文档）：ISO 国际键与未收录消费键注入时返回 `None`。
    #[test]
    fn reverse_gaps_are_intentional() {
        for key in [
            xkey::KEY_NONUS_HASH, // ISO 布局与 Backslash 同物理键（0x2B 已归 Backslash）
            xkey::KEY_HELP,
            xkey::KEY_EXECUTE,
            xkey::KEY_MENU,
            xkey::KEY_SELECT,
            xkey::KEY_STOP,
            xkey::KEY_AGAIN,
            xkey::KEY_UNDO,
            xkey::KEY_CUT,
            xkey::KEY_COPY,
            xkey::KEY_PASTE,
            xkey::KEY_FIND,
            xkey::KEY_VOLUME_UP, // 键盘页音量键：Windows 无该扫描码（媒体走 CK_VOLUME_UP）
            xkey::KEY_MUTE,
        ] {
            assert!(
                xkey_to_scan(key).is_none(),
                "{key:?} 应无 Windows 扫描码对应"
            );
        }
    }

    /// 全表双向往返恒等：正查全表逐条；反查每个表内 XKey 取 canonical 位后仍回到同键。
    #[test]
    fn roundtrip_is_identity() {
        for (scan, ext, key) in SCAN_TABLE {
            assert_eq!(scan_to_xkey(*scan, *ext), Some(*key), "正查 0x{scan:02X}");
        }
        for (_, _, key) in SCAN_TABLE {
            let (rscan, rext) = xkey_to_scan(*key).expect("表内键必有 canonical 反查位");
            assert_eq!(
                scan_to_xkey(rscan, rext),
                Some(*key),
                "canonical 往返 {key:?}"
            );
        }
    }

    /// 全表 XKey 值域合法（防表写错区段，与 mac keymap 同样的冗余断言）。
    #[test]
    fn all_xkeys_in_valid_ranges() {
        for (_, _, key) in SCAN_TABLE {
            assert!(XKey::is_valid_raw(key.raw()), "0x{:04X} 越界", key.raw());
        }
    }

    /// 扩展位汇总：所有 E0 键（extended=true）都在表内正确分组（防表写错列）。
    #[test]
    fn extended_entries_are_e0_family() {
        for (scan, ext, _) in SCAN_TABLE {
            if *ext {
                // E0 键族：导航簇/右修饰/Win/App/方向键/KP 分流键/F13-24 以外的媒体与电源。
                // 数值锚点逐项来自官方表 E0 行。
                let e0_scans = [
                    0x10, 0x19, 0x1C, 0x1D, 0x20, 0x22, 0x24, 0x2E, 0x30, 0x35, 0x37, 0x38, 0x45,
                    0x46, 0x47, 0x48, 0x49, 0x4B, 0x4D, 0x4F, 0x50, 0x51, 0x52, 0x53, 0x5B, 0x5C,
                    0x5D, 0x5E, 0x5F, 0x63,
                ];
                assert!(
                    e0_scans.contains(scan),
                    "0x{scan:02X} 不在官方表 E0 行集合内（表写错 extended 列？）"
                );
            }
        }
    }

    // —— xkey_to_vk（M4-hotkeys：RegisterHotKey 的 vk 形参表） ——

    /// VK 表锚点：字母/数字/导航/小键盘/F 区/Symbol 连续段与零散段逐项。
    /// 期望值 = WinUser.h 公开定义（VK_A=0x41 起、VK_F1=0x70 起、VK_NUMPAD0=0x60 起）。
    #[test]
    fn vk_table_anchors() {
        use xcontrol_proto::xkey as xk;
        let expect = [
            (xk::KEY_A, 0x41),
            (xk::KEY_Z, 0x5A),
            (xk::KEY_1, 0x31),
            (xk::KEY_9, 0x39),
            (xk::KEY_0, 0x30),
            (xk::KEY_ENTER, 0x0D),
            (xk::KEY_ESCAPE, 0x1B),
            (xk::KEY_BACKSPACE, 0x08),
            (xk::KEY_TAB, 0x09),
            (xk::KEY_SPACE, 0x20),
            (xk::KEY_MINUS, 0xBD),
            (xk::KEY_EQUAL, 0xBB),
            (xk::KEY_LEFTBRACKET, 0xDB),
            (xk::KEY_RIGHTBRACKET, 0xDD),
            (xk::KEY_BACKSLASH, 0xDC),
            (xk::KEY_SEMICOLON, 0xBA),
            (xk::KEY_GRAVE, 0xC0),
            (xk::KEY_COMMA, 0xBC),
            (xk::KEY_DOT, 0xBE),
            (xk::KEY_SLASH, 0xBF),
            (xk::KEY_CAPS_LOCK, 0x14),
            (xk::KEY_F1, 0x70),
            (xk::KEY_F12, 0x7B),
            (xk::KEY_F13, 0x7C),
            (xk::KEY_F24, 0x87),
            (xk::KEY_INSERT, 0x2D),
            (xk::KEY_HOME, 0x24),
            (xk::KEY_PAGE_UP, 0x21),
            (xk::KEY_DELETE, 0x2E),
            (xk::KEY_END, 0x23),
            (xk::KEY_PAGE_DOWN, 0x22),
            (xk::KEY_RIGHT, 0x27), // 默认热键 next 主键
            (xk::KEY_LEFT, 0x25),  // 默认热键 prev 主键
            (xk::KEY_DOWN, 0x28),
            (xk::KEY_UP, 0x26),
            (xk::KEY_NUM_LOCK, 0x90),
            (xk::KEY_KP_DIVIDE, 0x6F),
            (xk::KEY_KP_MULTIPLY, 0x6A),
            (xk::KEY_KP_MINUS, 0x6D),
            (xk::KEY_KP_PLUS, 0x6B),
            (xk::KEY_KP_1, 0x61),
            (xk::KEY_KP_9, 0x69),
            (xk::KEY_KP_0, 0x60),
            (xk::KEY_KP_DOT, 0x6E),
            (xk::KEY_NONUS_BACKSLASH, 0xE2),
            (xk::KEY_APPLICATION, 0x5D),
        ];
        for (key, vk) in expect {
            assert_eq!(xkey_to_vk(key), Some(vk), "{key:?} → VK 0x{vk:02X}");
        }
        // 默认热键 back_to_host 主键 = Home
        assert_eq!(xkey_to_vk(xk::KEY_HOME), Some(0x24));
    }

    /// VK 表不映射形态：修饰键（MOD_* 掩码域）、系统保留键、KP_ENTER（VK 冲突）、
    /// 消费者页媒体键（RegisterHotKey 无法注册）。
    #[test]
    fn vk_table_rejections() {
        use xcontrol_proto::xkey as xk;
        for key in [
            xk::KEY_LEFT_CTRL,
            xk::KEY_RIGHT_META,
            xk::KEY_PRINT_SCREEN,
            xk::KEY_SCROLL_LOCK,
            xk::KEY_PAUSE,
            xk::KEY_KP_ENTER,
            xk::KEY_KP_EQUAL,
            CK_VOLUME_UP,
            CK_PLAY_PAUSE,
        ] {
            assert!(xkey_to_vk(key).is_none(), "{key:?} 应无 VK 映射");
        }
    }

    /// 字母/F 区连续段全查（防段内跳键）。
    #[test]
    fn vk_table_contiguous_ranges() {
        // A–Z 连续
        for i in 0..26u16 {
            let key = XKey::new(0x04 + i).unwrap();
            assert_eq!(xkey_to_vk(key), Some(u32::from(0x41 + i)));
        }
        // F1–F24 连续（两段拼接：F1-F12 0x70-0x7B、F13-F24 0x7C-0x87）
        for i in 0..12u16 {
            assert_eq!(
                xkey_to_vk(XKey::new(0x3A + i).unwrap()),
                Some(u32::from(0x70 + i))
            );
            assert_eq!(
                xkey_to_vk(XKey::new(0x68 + i).unwrap()),
                Some(u32::from(0x7C + i))
            );
        }
        // 小键盘 0–9 连续
        for i in 0..10u16 {
            let hid = if i == 0 { 0x62 } else { 0x58 + i }; // KP_0 单独，KP_1..9 = 0x59..0x61
            assert_eq!(
                xkey_to_vk(XKey::new(hid).unwrap()),
                Some(u32::from(0x60 + i)),
                "KP_{i}"
            );
        }
    }
}
