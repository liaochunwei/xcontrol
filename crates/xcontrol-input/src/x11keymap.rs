//! Linux evdev `KEY_*` → [`XKey`]（HID usage）静态映射表（docs/03 §2 表 X11 行）。
//!
//! **非门控纯数据区**：本模块零 unsafe、零平台 API、零 `x11rb` 依赖，任意平台默认
//! 编译——CI 无 Linux 也可全量测试（design.md 模块结构，mac [`crate::keymap`] /
//! win [`crate::winkeymap`] 先例）。锚点测试跨平台可跑。
//!
//! # 两段式编号链（2026-11-17 实测核验，docs/03 §2 修订记录）
//!
//! X11 keycode 的编号链是**物理键 → HID usage → 内核翻译 → evdev `KEY_*` → X keycode**，
//! 其中只有最后一段是线性关系：
//!
//! ```text
//! X keycode = evdev KEY_* + 8      ← 全量成立、无例外（本仓核验）
//! evdev KEY_* ↔ HID usage          ← 完全不同编号体系，需本全表
//! ```
//!
//! **`+8` 关系的证据**（2026-11-17，lsdark-eos Arch x86_64 实测 + 公开文件核对）：
//!
//! - `linux/input-event-codes.h`（内核 6.x，`/usr/include/linux/`）定义 `KEY_*` 值；
//! - xkb `keycodes/evdev`（xkeyboard-config）每一行都是 `<K> = KEY_* + 8` 的自注释
//!   映射（`minimum = 8`，`<ESC> = 9`、`<AC01> = 38`、`<FK01> = 67`、`<I183> = 191`
//!   ……全文件逐行 +8，无一处例外）；
//! - 运行时锚点：Xwayland `xmodmap -pke` 实测 keycode 38 = `a`、50 = `Shift_L`、
//!   62 = `Shift_R`、104 = `KP_Enter`、133 = `Super_L`——与 +8 全表逐项吻合。
//!
//! **`KEY_*` ↔ HID usage 全表的证据**：内核 `drivers/hid/hid-input.c` 的
//! `hid_keyboard[256]` 表（HID 键盘页 usage → `KEY_*` 的官方翻译）+ 同文件
//! `HID_UP_CONSUMER` switch（消费者页 usage → `KEY_*`）。两值域无关的直接例证：
//! `KEY_A` = 30 而 HID `A` = 0x04；`KEY_ESC` = 1 而 HID `Escape` = 0x29。本表按
//! 内核 v6.6 源码逐项转录（2026-11-17 自 torvalds/linux v6.6 核对），XKey 侧取
//! `xcontrol-proto` 常量（HID usage 语义）。
//!
//! # 覆盖范围（M3）
//!
//! 字母 A–Z、数字 0–9、F1–F24、方向键与导航键、修饰键全集（左右手分开）、
//! Space/Tab/Return/Escape/Backspace、常用符号键、小键盘全键（含 KP Enter/KP Equals）、
//! 媒体键子集（音量/播放/切歌/刻录/亮度/电源，经消费者页 usage 对 CK_* 常量）、
//! 编辑动作键（Undo/Cut/Copy/Paste/Find/Again/Stop）、Help/Menu/Compose(Application)。
//!
//! **有意不映射**（查表不中一律拒绝，捕获调用方丢弃事件；注入返回 `None` 计数）：
//!
//! - 国际/语言键（`KEY_ZENKAKUHANKAKU`(85)、`KEY_RO`(89)、`KEY_KATAKANA`(90)、
//!   `KEY_HIRAGANA`(91)、`KEY_HENKAN`(92)、`KEY_MUHENKAN`(94)、`KEY_KPJPCOMMA`(95)、
//!   `KEY_HANGEUL`(122)、`KEY_HANJA`(123)、`KEY_YEN`(124)、`KEY_LINEFEED`(101)、
//!   `KEY_KPCOMMA`(121)、`KEY_SCALE`(120)、84 缝隙）：XKey 侧无国际键常量（proto 留待
//!   布局扩展统一补录——mac [`crate::keymap`] 的 ISO/JIS 取舍一致，不为没有常量语义的
//!   键发明映射）；
//! - **`KEY_NONUS_HASH`（HID 0x32）**：内核 `hid_keyboard[0x32]` 与 Backslash（0x31）
//!   同落 `KEY_BACKSLASH`(43)——Linux 下无独立 evdev 编号（ISO #~ 键与 ANSI \ 键共享
//!   物理位），反向表按 canonical 首见归 [`xcontrol_proto::xkey::KEY_BACKSLASH`]；
//! - **键盘页音量三键**（HID 0x7F `Mute` / 0x80 `VolumeUp` / 0x81 `VolumeDown`）：内核
//!   与消费者页（0xE2/0xE9/0xEA → `KEY_MUTE`(113)/`KEY_VOLUMEUP`(115)/
//!   `KEY_VOLUMEDOWN`(114)）映射到相同 evdev 码，反向表取**消费者页语义**
//!   （`CK_MUTE`/`CK_VOLUME_UP`/`CK_VOLUME_DOWN`）——物理键盘媒体键走消费者页、跨平台
//!   对齐 mac（`CK_VOLUME_UP` 才有 kVK）与 win（winkeymap 媒体键全为 CK_*）后端；
//!   键盘页语义的 `KEY_MUTE`/`KEY_VOLUME_UP`/`KEY_VOLUME_DOWN` 三个 proto 常量因此
//!   无注入位（文档化缺口）；
//! - **`KEY_PAUSE` vs `CK_PAUSE`**：两者内核映射同为 `KEY_PAUSE`(119)（键盘页 0x48 与
//!   消费者页 0xB1），反向表按 win 后端先例取 [`xcontrol_proto::xkey::KEY_PAUSE`]；
//!   **`CK_POWER`** 同理（消费者页 0x30 与键盘页 0x66 同落 `KEY_POWER`(116)，取
//!   [`xcontrol_proto::xkey::KEY_POWER`]）；
//! - **`KEY_EXECUTE`（0x74）/`KEY_SELECT`（0x77）**：内核键盘页怪癖映射
//!   （0x74→`KEY_OPEN`(134)、0x77→`KEY_FRONT`(132)，xkb 符号对应 XF86Open/XF86Front
//!   媒体动作）——语义错位，注入会触发错误应用行为，宁缺毋滥（同
//!   [`crate::winkeymap`] "不为没有常量语义的键发明映射"的精神）；
//! - evdev ≥ 248 的键（`KEY_MICMUTE`(248) 等）：+8 后超出 X keycode u8 上限（xkb
//!   `keycodes/evdev` `maximum = 255`），X11 编号空间不可达，无映射意义。
//!
//! # 反向表（注入路径）
//!
//! [`xkey_to_keycode`] 产出 X keycode（= evdev + 8），配合 XTEST `fake_input` 的
//! **keycode 非 keysym** 语义按物理键位注入（docs/05 §3 注入行红线）。同一 XKey 有
//! 多个 evdev 来源时反向表取**首个（canonical）条目**——canonical 按语义对应排布，
//! 非 canonical 别名（内核 Menu→Props 怪癖产生的 `KEY_PROPS`(130)）集中在表尾
//! （winkeymap 同法）。唯一重复项 `KEY_MENU` 见表尾注释。

use std::collections::HashMap;
use std::sync::LazyLock;

use xcontrol_proto::xkey::{self, XKey};

/// X keycode 的有效下界（X 协议保留 0–7；xkb `keycodes/evdev` `minimum = 8`）。
pub const MIN_KEYCODE: u8 = 8;

/// evdev `KEY_*` → [`XKey`] 全量表（canonical 条目在前、按语义对应排布；非 canonical
/// 别名殿后）。
///
/// 数值来源：内核 `drivers/hid/hid-input.c` v6.6 `hid_keyboard[]` + `HID_UP_CONSUMER`
/// switch（HID usage → `KEY_*` 的官方翻译，模块文档）；`KEY_*` 值与
/// `linux/input-event-codes.h` 对照核实。注释列内核 `KEY_*` 名与 xkb 键名对照。
pub const EVDEV_TABLE: &[(u8, XKey)] = &[
    // —— 控制键与数字行（KEY_ESC=1 … KEY_0=11；hid_keyboard[0x29]=1、[0x1E..0x27]=2..11）——
    (1, xkey::KEY_ESCAPE),     // KEY_ESC（HID 0x29；xkb <ESC> = 9）
    (2, xkey::KEY_1),          // KEY_1（xkb <AE01> = 10）
    (3, xkey::KEY_2),          // KEY_2
    (4, xkey::KEY_3),          // KEY_3
    (5, xkey::KEY_4),          // KEY_4
    (6, xkey::KEY_5),          // KEY_5
    (7, xkey::KEY_6),          // KEY_6
    (8, xkey::KEY_7),          // KEY_7
    (9, xkey::KEY_8),          // KEY_8
    (10, xkey::KEY_9),         // KEY_9
    (11, xkey::KEY_0),         // KEY_0（dispatch 锚点）
    (12, xkey::KEY_MINUS),     // KEY_MINUS（xkb <AE11> = 20）
    (13, xkey::KEY_EQUAL),     // KEY_EQUAL（xkb <AE12> = 21）
    (14, xkey::KEY_BACKSPACE), // KEY_BACKSPACE（HID 名 Delete；xkb <BKSP> = 22）
    (15, xkey::KEY_TAB),       // KEY_TAB（xkb <TAB> = 23）
    // —— 字母 QWERTY 位图（KEY_Q=16 … KEY_M=50；hid_keyboard[0x04..0x1D] 逐项）——
    (16, xkey::KEY_Q),
    (17, xkey::KEY_W),
    (18, xkey::KEY_E),
    (19, xkey::KEY_R),
    (20, xkey::KEY_T),
    (21, xkey::KEY_Y),
    (22, xkey::KEY_U),
    (23, xkey::KEY_I),
    (24, xkey::KEY_O),
    (25, xkey::KEY_P),
    (26, xkey::KEY_LEFTBRACKET),  // KEY_LEFTBRACE（xkb <AD11> = 34）
    (27, xkey::KEY_RIGHTBRACKET), // KEY_RIGHTBRACE（xkb <AD12> = 35）
    (28, xkey::KEY_ENTER),        // KEY_ENTER（主键区；KP Enter = 96；xkb <RTRN> = 36）
    (29, xkey::KEY_LEFT_CTRL),    // KEY_LEFTCTRL（dispatch 锚点；右 Ctrl = 97；xkb <LCTL> = 37）
    (30, xkey::KEY_A),            // KEY_A（dispatch 锚点；xkb <AC01> = 38，xmodmap 实测 a）
    (31, xkey::KEY_S),
    (32, xkey::KEY_D),
    (33, xkey::KEY_F),
    (34, xkey::KEY_G),
    (35, xkey::KEY_H),
    (36, xkey::KEY_J),
    (37, xkey::KEY_K),
    (38, xkey::KEY_L),
    (39, xkey::KEY_SEMICOLON),  // KEY_SEMICOLON
    (40, xkey::KEY_APOSTROPHE), // KEY_APOSTROPHE
    (41, xkey::KEY_GRAVE),      // KEY_GRAVE（xkb <TLDE> = 49）
    (42, xkey::KEY_LEFT_SHIFT), // KEY_LEFTSHIFT（keycode 50，PRD 锚点；xkb <LFSH> = 50）
    (43, xkey::KEY_BACKSLASH),  // KEY_BACKSLASH（HID 0x31；0x32 NonUS# 内核同落此码，模块文档；
    //  xkb <BKSL> = 51）
    (44, xkey::KEY_Z),
    (45, xkey::KEY_X),
    (46, xkey::KEY_C),
    (47, xkey::KEY_V),
    (48, xkey::KEY_B),
    (49, xkey::KEY_N),
    (50, xkey::KEY_M),
    (51, xkey::KEY_COMMA),       // KEY_COMMA
    (52, xkey::KEY_DOT),         // KEY_DOT
    (53, xkey::KEY_SLASH),       // KEY_SLASH
    (54, xkey::KEY_RIGHT_SHIFT), // KEY_RIGHTSHIFT（keycode 62，PRD 锚点；xkb <RTSH> = 62）
    (55, xkey::KEY_KP_MULTIPLY), // KEY_KPASTERISK（xkb <KPMU> = 63）
    (56, xkey::KEY_LEFT_ALT),    // KEY_LEFTALT（xkb <LALT> = 64）
    (57, xkey::KEY_SPACE),       // KEY_SPACE（xkb <SPCE> = 65，xmodmap 实测 space）
    (58, xkey::KEY_CAPS_LOCK),   // KEY_CAPSLOCK（xkb <CAPS> = 66，xmodmap 实测 Caps_Lock）
    // —— F1–F12（KEY_F1=59…KEY_F12=88，F11/F12 跳到 87/88）——
    (59, xkey::KEY_F1), // xkb <FK01> = 67
    (60, xkey::KEY_F2),
    (61, xkey::KEY_F3),
    (62, xkey::KEY_F4),
    (63, xkey::KEY_F5),
    (64, xkey::KEY_F6),
    (65, xkey::KEY_F7),
    (66, xkey::KEY_F8),
    (67, xkey::KEY_F9),
    (68, xkey::KEY_F10),
    (69, xkey::KEY_NUM_LOCK),    // KEY_NUMLOCK（xkb <NMLK> = 77）
    (70, xkey::KEY_SCROLL_LOCK), // KEY_SCROLLLOCK（xkb <SCLK> = 78）
    // —— 小键盘（KEY_KP7=71…KEY_KPDOT=83）——
    (71, xkey::KEY_KP_7),
    (72, xkey::KEY_KP_8),
    (73, xkey::KEY_KP_9),
    (74, xkey::KEY_KP_MINUS),
    (75, xkey::KEY_KP_4),
    (76, xkey::KEY_KP_5),
    (77, xkey::KEY_KP_6),
    (78, xkey::KEY_KP_PLUS),
    (79, xkey::KEY_KP_1),
    (80, xkey::KEY_KP_2),
    (81, xkey::KEY_KP_3),
    (82, xkey::KEY_KP_0),
    (83, xkey::KEY_KP_DOT),
    // 84 缝隙、85 KEY_ZENKAKUHANKAKU、89 KEY_RO、90-95 日韩语言键：无 proto 常量（模块文档）
    (86, xkey::KEY_NONUS_BACKSLASH), // KEY_102ND（HID 0x64 NonUS Backslash；xkb <LSGT> = 94）
    (87, xkey::KEY_F11),             // KEY_F11（xkb <FK11> = 95）
    (88, xkey::KEY_F12),             // KEY_F12（xkb <FK12> = 96）
    // —— 导航与系统键（KEY_KPENTER=96 …）——
    (96, xkey::KEY_KP_ENTER), // KEY_KPENTER（xkb <KPEN> = 104，xmodmap 实测 KP_Enter）
    (97, xkey::KEY_RIGHT_CTRL), // KEY_RIGHTCTRL（xkb <RCTL> = 105，xmodmap 实测 Control_R）
    (98, xkey::KEY_KP_DIVIDE), // KEY_KPSLASH（xkb <KPDV> = 106）
    (99, xkey::KEY_PRINT_SCREEN), // KEY_SYSRQ（HID 0x46 PrintScreen → SYSRQ；xkb <PRSC> = 107）
    (100, xkey::KEY_RIGHT_ALT), // KEY_RIGHTALT（xkb <RALT> = 108，xmodmap 实测 Alt_R）
    // 101 KEY_LINEFEED：无 proto 常量（模块文档）
    (102, xkey::KEY_HOME),      // KEY_HOME（xkb <HOME> = 110）
    (103, xkey::KEY_UP),        // KEY_UP（xkb <UP> = 111）
    (104, xkey::KEY_PAGE_UP),   // KEY_PAGEUP（xkb <PGUP> = 112）
    (105, xkey::KEY_LEFT),      // KEY_LEFT（xkb <LEFT> = 113）
    (106, xkey::KEY_RIGHT),     // KEY_RIGHT（xkb <RGHT> = 114）
    (107, xkey::KEY_END),       // KEY_END（xkb <END> = 115）
    (108, xkey::KEY_DOWN),      // KEY_DOWN（xkb <DOWN> = 116）
    (109, xkey::KEY_PAGE_DOWN), // KEY_PAGEDOWN（xkb <PGDN> = 117）
    (110, xkey::KEY_INSERT),    // KEY_INSERT（xkb <INS> = 118）
    (111, xkey::KEY_DELETE),    // KEY_DELETE（xkb <DELE> = 119）
    // —— 媒体三键（消费者页语义优先，模块文档；KEY_MUTE/KEY_VOLUMEUP/KEY_VOLUMEDOWN
    //     键盘页常量无注入位）——
    (113, xkey::CK_MUTE), // KEY_MUTE ← 消费者页 0xE2（CK_MUTE = 0x100+0xE2；xkb <MUTE> = 121）
    (114, xkey::CK_VOLUME_DOWN), // KEY_VOLUMEDOWN ← 消费者页 0xEA（xkb <VOL-> = 122）
    (115, xkey::CK_VOLUME_UP), // KEY_VOLUMEUP ← 消费者页 0xE9（xkb <VOL+> = 123）
    (116, xkey::KEY_POWER), // KEY_POWER（键盘页 0x66 与消费者页 0x30 同码；win 后端先例取键盘页；
    //  xkb <POWR> = 124）
    (117, xkey::KEY_KP_EQUAL), // KEY_KPEQUAL（xkb <KPEQ> = 125）
    (119, xkey::KEY_PAUSE), // KEY_PAUSE（键盘页 0x48 与消费者页 0xB1 同码；win 后端先例取键盘页；
    //  xkb <PAUS> = 127）
    // 118 KEY_KPPLUSMINUS、120 KEY_SCALE、121 KEY_KPCOMMA、122/123 韩文、124 KEY_YEN：无 proto 常量
    (125, xkey::KEY_LEFT_META), // KEY_LEFTMETA（xkb <LWIN> = 133，xmodmap 实测 Super_L）
    (126, xkey::KEY_RIGHT_META), // KEY_RIGHTMETA（xkb <RWIN> = 134，xmodmap 实测 Super_R）
    (127, xkey::KEY_APPLICATION), // KEY_COMPOSE（HID 0x65 Application；xkb <COMP> = 135）
    // —— 编辑动作键（hid_keyboard[0x74..0x7E] + 消费者页 0x40；STOP/AGAIN/UNDO/COPY/
    //     PASTE/FIND/CUT/HELP 内核直落同义 evdev 码）——
    (128, xkey::KEY_STOP), // KEY_STOP ← HID 键盘页 0x78 Stop（KEY_STOP = AC Stop；xkb <STOP> = 136）
    (129, xkey::KEY_AGAIN), // KEY_AGAIN ← HID 0x79 Again（xkb <AGAI> = 137）
    (131, xkey::KEY_UNDO), // KEY_UNDO ← HID 0x7A Undo（xkb <UNDO> = 139）
    // 132 KEY_FRONT ← HID 0x77 Select（内核 Select→Front 怪癖，语义错位有意不映射，模块文档）
    (133, xkey::KEY_COPY), // KEY_COPY ← HID 0x7C Copy（xkb <COPY> = 141）
    // 134 KEY_OPEN ← HID 0x74 Execute（内核 Execute→Open 怪癖，有意不映射，模块文档）
    (135, xkey::KEY_PASTE), // KEY_PASTE ← HID 0x7D Paste（xkb <PAST> = 143）
    (136, xkey::KEY_FIND),  // KEY_FIND ← HID 0x7E Find（xkb <FIND> = 144）
    (137, xkey::KEY_CUT),   // KEY_CUT ← HID 0x7B Cut（xkb <CUT> = 145）
    (138, xkey::KEY_HELP),  // KEY_HELP ← HID 0x75 Help（xkb <HELP> = 146）
    (139, xkey::KEY_MENU),  // KEY_MENU ← 消费者页 0x40 Menu（canonical：xkb <I147> = 147 符号
    //  为 XF86Menu；130 的 KEY_PROPS 来源同义，见表尾别名块）
    (142, xkey::CK_SLEEP), // KEY_SLEEP ← 消费者页 0x32（CK_SLEEP = 0x100+0x32；win 后端先例；
    //  xkb <I150> = 150）
    (143, xkey::CK_WAKE_UP), // KEY_WAKEUP ← GD SystemControl 0x83（CK_WAKE_UP = 0x100+0x83；
    //  内核自 GD 页翻译，proto 取消费者页偏移语义——注释存疑点；
    //  xkb <I151> = 151）
    // 144-160 文件/浏览器/程序键（KEY_FILE/KEY_WWW/KEY_MAIL 等）：proto 消费者页未收录
    (161, xkey::CK_EJECT), // KEY_EJECTCD ← 消费者页 0xB8（xkb <I169> = 169）
    (163, xkey::CK_SCAN_NEXT_TRACK), // KEY_NEXTSONG ← 消费者页 0xB5（xkb <I171> = 171）
    (164, xkey::CK_PLAY_PAUSE), // KEY_PLAYPAUSE ← 消费者页 0xCD（xkb <I172> = 172）
    (165, xkey::CK_SCAN_PREV_TRACK), // KEY_PREVIOUSSONG ← 消费者页 0xB6（xkb <I173> = 173）
    (166, xkey::CK_STOP), // KEY_STOPCD ← 消费者页 0xB7（与 KEY_STOP(128) 分码并存；xkb <I174> = 174）
    (167, xkey::CK_RECORD), // KEY_RECORD ← 消费者页 0xB2（xkb <I175> = 175）
    (168, xkey::CK_REWIND), // KEY_REWIND ← 消费者页 0xB4（xkb <I176> = 176）
    // —— F13–F24（KEY_F13=183 … KEY_F24=194）——
    (183, xkey::KEY_F13), // xkb <FK13> = 191
    (184, xkey::KEY_F14),
    (185, xkey::KEY_F15),
    (186, xkey::KEY_F16),
    (187, xkey::KEY_F17),
    (188, xkey::KEY_F18),
    (189, xkey::KEY_F19),
    (190, xkey::KEY_F20),
    (191, xkey::KEY_F21),
    (192, xkey::KEY_F22),
    (193, xkey::KEY_F23),
    (194, xkey::KEY_F24), // xkb <FK24> = 202
    // —— 播放控制（KEY_PLAY=207 / KEY_FASTFORWARD=208）——
    (207, xkey::CK_PLAY),         // KEY_PLAY ← 消费者页 0xB0
    (208, xkey::CK_FAST_FORWARD), // KEY_FASTFORWARD ← 消费者页 0xB3
    // —— 亮度（KEY_BRIGHTNESSDOWN=224 / KEY_BRIGHTNESSUP=225——降在前、升在后，与直觉
    //     相反，逐项对 input-event-codes.h）——
    (224, xkey::CK_BRIGHTNESS_DOWN), // KEY_BRIGHTNESSDOWN ← 消费者页 0x70
    (225, xkey::CK_BRIGHTNESS_UP),   // KEY_BRIGHTNESSUP ← 消费者页 0x6F
    // —— 非 canonical 别名（**只服务捕获方向**；反查表首见优先取上方 canonical 位，
    //    本块必须保持在表尾——见模块文档与 table_has_no_duplicate_xkey 测试）——
    (130, xkey::KEY_MENU), // KEY_PROPS ← HID 键盘页 0x76 Menu（内核 Menu→Props 怪癖：真实 USB
                           //  键盘的 Menu 键落 evdev 130；捕获按 Menu 语义上报、注入走 139 的
                           //  XF86Menu 符号路径）
];

/// X keycode → [`XKey`]（捕获路径；keycode < 8 或表外 evdev 码返回 `None`——未知键拒绝
/// 语义，[`crate::platform::x11`] 捕获调用方丢弃事件）。
///
/// 捕获热路径（每次按键一次）：线性扫 ~145 项 u8 比较 ≈ 百纳秒级，远低于回调纪律
/// 预算（platform-code.md：回调内只做转换与投递）。XRecord 事件线程零 async。
#[must_use]
pub fn keycode_to_xkey(keycode: u8) -> Option<XKey> {
    let evdev = keycode.checked_sub(MIN_KEYCODE)?;
    EVDEV_TABLE
        .iter()
        .find(|(code, _)| *code == evdev)
        .map(|(_, key)| *key)
}

/// evdev → keycode 反查表（懒构建自 [`EVDEV_TABLE`]；XTEST 注入路径用）。
///
/// 同一 XKey 多条目时取**首个**（canonical）——非 canonical 条目（`KEY_PROPS`(130)
/// 来源的 Menu）只服务捕获方向。表内每个 XKey 至多一个注入位（tests 断言）。
static EVDEV_BY_XKEY: LazyLock<HashMap<u16, u8>> = LazyLock::new(|| {
    let mut map = HashMap::with_capacity(EVDEV_TABLE.len());
    for (evdev, key) in EVDEV_TABLE {
        // 多来源键首见即 canonical（模块文档；winkeymap 反查表同法）。
        map.entry(key.raw()).or_insert(*evdev);
    }
    map
});

/// [`XKey`] → X keycode（注入路径；= evdev + 8，无 Linux 对应返回 `None`）。
///
/// 消费者页 XKey（`CK_*`）经内核消费者页翻译后的 `KEY_*` 码 +8；键盘页音量键等
/// 反向缺口见模块文档（注入按 [`xcontrol_core::sink::SinkError::Failed`] 计数，
/// error-handling.md §4 不中断事件流）。
#[must_use]
pub fn xkey_to_keycode(key: XKey) -> Option<u8> {
    EVDEV_BY_XKEY
        .get(&key.raw())
        .map(|evdev| evdev + MIN_KEYCODE)
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::xkey::{
        CK_BRIGHTNESS_DOWN, CK_BRIGHTNESS_UP, CK_EJECT, CK_FAST_FORWARD, CK_MUTE, CK_PLAY,
        CK_PLAY_PAUSE, CK_RECORD, CK_REWIND, CK_SCAN_NEXT_TRACK, CK_SCAN_PREV_TRACK, CK_SLEEP,
        CK_STOP, CK_VOLUME_DOWN, CK_VOLUME_UP, CK_WAKE_UP, KEY_A, KEY_AGAIN, KEY_APPLICATION,
        KEY_B, KEY_BACKSLASH, KEY_BACKSPACE, KEY_CAPS_LOCK, KEY_COMMA, KEY_COPY, KEY_CUT,
        KEY_DELETE, KEY_DOT, KEY_DOWN, KEY_END, KEY_ENTER, KEY_EQUAL, KEY_F1, KEY_F2, KEY_F3,
        KEY_F4, KEY_F5, KEY_F6, KEY_F7, KEY_F8, KEY_F9, KEY_F10, KEY_F11, KEY_F12, KEY_F13,
        KEY_F14, KEY_F15, KEY_F16, KEY_F17, KEY_F18, KEY_F19, KEY_F20, KEY_F21, KEY_F22, KEY_F23,
        KEY_F24, KEY_FIND, KEY_GRAVE, KEY_HELP, KEY_HOME, KEY_INSERT, KEY_KP_0, KEY_KP_1, KEY_KP_2,
        KEY_KP_3, KEY_KP_4, KEY_KP_5, KEY_KP_6, KEY_KP_7, KEY_KP_8, KEY_KP_9, KEY_KP_DIVIDE,
        KEY_KP_DOT, KEY_KP_ENTER, KEY_KP_EQUAL, KEY_KP_MINUS, KEY_KP_MULTIPLY, KEY_KP_PLUS,
        KEY_LEFT, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_LEFT_SHIFT, KEY_LEFTBRACKET,
        KEY_MENU, KEY_MINUS, KEY_NONUS_BACKSLASH, KEY_NUM_LOCK, KEY_PAGE_DOWN, KEY_PAGE_UP,
        KEY_PASTE, KEY_PAUSE, KEY_POWER, KEY_PRINT_SCREEN, KEY_RIGHT, KEY_RIGHT_ALT,
        KEY_RIGHT_CTRL, KEY_RIGHT_META, KEY_RIGHT_SHIFT, KEY_RIGHTBRACKET, KEY_SCROLL_LOCK,
        KEY_SEMICOLON, KEY_SLASH, KEY_SPACE, KEY_STOP, KEY_TAB, KEY_UNDO, KEY_UP,
    };

    use super::*;

    /// 表覆盖量级（≥140 项；抽删键会在此先失败）。
    #[test]
    fn table_size_sanity() {
        assert!(
            EVDEV_TABLE.len() >= 140,
            "evdev 全量表应 ≥140 项，实际 {}",
            EVDEV_TABLE.len()
        );
    }

    /// evdev 码无重复（捕获查表唯一性的前提；表主体按语义对应排布、别名块殿后，
    /// 不做整体升序断言——winkeymap 同法）。
    #[test]
    fn table_has_no_duplicate_evdev_codes() {
        let mut seen: Vec<u8> = EVDEV_TABLE.iter().map(|(code, _)| *code).collect();
        seen.sort_unstable();
        let len = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), len, "evdev 码在表内重复");
    }

    /// 表内 XKey 重复仅限模块文档钉死的一项（KEY_MENU：KEY_PROPS(130) capture-only +
    /// KEY_MENU(139) canonical）；其余每个 XKey 至多一个注入位。
    #[test]
    fn table_has_no_duplicate_xkey_beyond_documented_sources() {
        let mut seen: Vec<u16> = EVDEV_TABLE.iter().map(|(_, key)| key.raw()).collect();
        seen.sort_unstable();
        let len = seen.len();
        seen.dedup();
        assert_eq!(
            len - seen.len(),
            1,
            "多来源键应为模块文档钉死的 KEY_MENU 一键"
        );
    }

    /// **`+8` 关系全量断言**（本模块的核心不变式）：正查 keycode = evdev + 8、
    /// 反查 evdev = keycode - 8，对表内每一项双向成立。
    #[test]
    fn keycode_is_evdev_plus_eight_for_every_entry() {
        for (evdev, key) in EVDEV_TABLE {
            let keycode = evdev + MIN_KEYCODE;
            assert_eq!(
                keycode_to_xkey(keycode),
                Some(*key),
                "正查 keycode {keycode}（evdev {evdev}）"
            );
            assert!(xkey_to_keycode(*key).is_some(), "反查 {key:?} 应有注入位");
        }
    }

    /// 反查取 evdev + 8；canonical 首见优先（KEY_MENU 取 139 而非表尾别名 130）。
    #[test]
    fn reverse_prefers_canonical_entries() {
        assert_eq!(xkey_to_keycode(KEY_A), Some(30 + 8));
        assert_eq!(xkey_to_keycode(KEY_KP_ENTER), Some(96 + 8));
        assert_eq!(xkey_to_keycode(KEY_MENU), Some(139 + 8));
    }

    /// 锚点：字母（QWERTY 位图，dispatch 锚点 A=30 起逐项）。
    #[test]
    fn anchor_letters() {
        let expect = [
            (30u8, KEY_A),
            (31, xkey::KEY_S),
            (32, xkey::KEY_D),
            (48, KEY_B),
            (16, xkey::KEY_Q),
            (17, xkey::KEY_W),
            (44, xkey::KEY_Z),
            (45, xkey::KEY_X),
            (50, xkey::KEY_M),
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 锚点：数字行（2–11，dispatch 锚点 0=11；8=9、9=10）。
    #[test]
    fn anchor_digits() {
        let expect = [
            (2u8, xkey::KEY_1),
            (3, xkey::KEY_2),
            (9, xkey::KEY_8),
            (10, xkey::KEY_9),
            (11, xkey::KEY_0), // dispatch 锚点
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 锚点：修饰键（PRD 实测锚点 keycode 50/62 = 左/右 Shift；docs/02 §4.2 bit0-7 对应；
    /// xmodmap 运行时锚点逐项）。
    #[test]
    fn anchor_modifiers() {
        let expect = [
            (29u8, KEY_LEFT_CTRL), // keycode 37（xmodmap 实测 Control_L）
            (42, KEY_LEFT_SHIFT),  // keycode 50（PRD 锚点）
            (56, KEY_LEFT_ALT),    // keycode 64（xmodmap 实测 Alt_L）
            (125, KEY_LEFT_META),  // keycode 133（xmodmap 实测 Super_L）
            (97, KEY_RIGHT_CTRL),  // keycode 105（xmodmap 实测 Control_R）
            (54, KEY_RIGHT_SHIFT), // keycode 62（PRD 锚点）
            (100, KEY_RIGHT_ALT),  // keycode 108（xmodmap 实测 Alt_R）
            (126, KEY_RIGHT_META), // keycode 134（xmodmap 实测 Super_R）
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert!(key.is_modifier(), "{key:?} 应在 0xE0-0xE7 修饰键区");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
        // CapsLock 是普通键语义（HID 0x39，不在 mods bit0-7 对应区）
        assert_eq!(keycode_to_xkey(58 + 8), Some(KEY_CAPS_LOCK));
        assert!(!KEY_CAPS_LOCK.is_modifier());
    }

    /// 锚点：F 区（59-68 连续 + F11/F12 跳 87/88 + F13-F24 连续，xkb <FK11>/<FK13> 锚点）。
    #[test]
    fn anchor_function_keys() {
        let expect = [
            (59u8, KEY_F1),
            (60, KEY_F2),
            (61, KEY_F3),
            (62, KEY_F4),
            (63, KEY_F5),
            (64, KEY_F6),
            (65, KEY_F7),
            (66, KEY_F8),
            (67, KEY_F9),
            (68, KEY_F10),
            (87, KEY_F11),  // xkb <FK11> = 95（89-95 为日韩语言键段）
            (88, KEY_F12),  // xkb <FK12> = 96
            (183, KEY_F13), // xkb <FK13> = 191
            (184, KEY_F14),
            (185, KEY_F15),
            (186, KEY_F16),
            (187, KEY_F17),
            (188, KEY_F18),
            (189, KEY_F19),
            (190, KEY_F20),
            (191, KEY_F21),
            (192, KEY_F22),
            (193, KEY_F23),
            (194, KEY_F24), // xkb <FK24> = 202
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 锚点：导航键（xkb keycodes/evdev 逐行 +8 的对照锚点）。
    #[test]
    fn anchor_navigation() {
        let expect = [
            (102u8, KEY_HOME),    // <HOME> = 110
            (103, KEY_UP),        // <UP> = 111
            (104, KEY_PAGE_UP),   // <PGUP> = 112
            (105, KEY_LEFT),      // <LEFT> = 113
            (106, KEY_RIGHT),     // <RGHT> = 114
            (107, KEY_END),       // <END> = 115
            (108, KEY_DOWN),      // <DOWN> = 116
            (109, KEY_PAGE_DOWN), // <PGDN> = 117
            (110, KEY_INSERT),    // <INS> = 118
            (111, KEY_DELETE),    // <DELE> = 119
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 锚点：小键盘（KEY_KP7=71 … KEY_KPDOT=83 与 KPENTER/KPSLASH 的 HID→evdev 错位）。
    #[test]
    fn anchor_keypad() {
        let expect = [
            (71u8, KEY_KP_7),
            (72, KEY_KP_8),
            (73, KEY_KP_9),
            (74, KEY_KP_MINUS),
            (75, KEY_KP_4),
            (76, KEY_KP_5),
            (77, KEY_KP_6),
            (78, KEY_KP_PLUS),
            (79, KEY_KP_1),
            (80, KEY_KP_2),
            (81, KEY_KP_3),
            (82, KEY_KP_0),
            (83, KEY_KP_DOT),
            (96, KEY_KP_ENTER),    // <KPEN> = 104
            (98, KEY_KP_DIVIDE),   // <KPDV> = 106
            (55, KEY_KP_MULTIPLY), // <KPMU> = 63
            (117, KEY_KP_EQUAL),   // <KPEQ> = 125
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 锚点：符号键（hid_keyboard[0x2D..0x38] 与 QWERTY 物理位对齐）。
    #[test]
    fn anchor_punctuation() {
        let expect = [
            (39u8, KEY_SEMICOLON),
            (40, xkey::KEY_APOSTROPHE),
            (41, KEY_GRAVE),     // <TLDE> = 49
            (43, KEY_BACKSLASH), // <BKSL> = 51
            (51, KEY_COMMA),
            (52, KEY_DOT),
            (53, KEY_SLASH),
            (26, KEY_LEFTBRACKET),  // <AD11> = 34
            (27, KEY_RIGHTBRACKET), // <AD12> = 35
            (12, KEY_MINUS),
            (13, KEY_EQUAL),
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 锚点：控制/符号/媒体键（含 xmodmap 运行时锚点 65=space、66=Caps）。
    #[test]
    fn anchor_control_symbols_media() {
        let expect = [
            (28u8, KEY_ENTER),         // <RTRN> = 36（xmodmap 实测 Return）
            (96, KEY_KP_ENTER),        // <KPEN> = 104（xmodmap 实测 KP_Enter）
            (57, KEY_SPACE),           // <SPCE> = 65（xmodmap 实测 space）
            (58, KEY_CAPS_LOCK),       // <CAPS> = 66（xmodmap 实测 Caps_Lock）
            (41, KEY_GRAVE),           // <TLDE> = 49
            (14, KEY_BACKSPACE),       // <BKSP> = 22
            (15, KEY_TAB),             // <TAB> = 23
            (12, KEY_MINUS),           // <AE11> = 20
            (13, KEY_EQUAL),           // <AE12> = 21
            (26, KEY_LEFTBRACKET),     // <AD11> = 34
            (27, KEY_RIGHTBRACKET),    // <AD12> = 35
            (43, KEY_BACKSLASH),       // <BKSL> = 51
            (86, KEY_NONUS_BACKSLASH), // <LSGT> = 94（ISO 102nd 键）
            (99, KEY_PRINT_SCREEN),    // <PRSC> = 107
            (70, KEY_SCROLL_LOCK),     // <SCLK> = 78
            (119, KEY_PAUSE),          // <PAUS> = 127
            (69, KEY_NUM_LOCK),        // <NMLK> = 77
            (98, KEY_KP_DIVIDE),       // <KPDV> = 106
            (55, KEY_KP_MULTIPLY),     // <KPMU> = 63
            (117, KEY_KP_EQUAL),       // <KPEQ> = 125
            (127, KEY_APPLICATION),    // <COMP> = 135（HID Application → Compose 码）
            (113, CK_MUTE),            // <MUTE> = 121
            (114, CK_VOLUME_DOWN),     // <VOL-> = 122
            (115, CK_VOLUME_UP),       // <VOL+> = 123
            (116, KEY_POWER),          // <POWR> = 124
            (142, CK_SLEEP),           // <I150> = 150
            (143, CK_WAKE_UP),         // <I151> = 151
            (161, CK_EJECT),           // <I169> = 169
            (163, CK_SCAN_NEXT_TRACK), // <I171> = 171
            (164, CK_PLAY_PAUSE),      // <I172> = 172
            (165, CK_SCAN_PREV_TRACK), // <I173> = 173
            (166, CK_STOP),            // <I174> = 174
            (167, CK_RECORD),          // <I175> = 175
            (168, CK_REWIND),          // <I176> = 176
            (207, CK_PLAY),            // KEY_PLAY = 207
            (208, CK_FAST_FORWARD),    // KEY_FASTFORWARD = 208
            (224, CK_BRIGHTNESS_DOWN), // KEY_BRIGHTNESSDOWN = 224 → keycode 232
            (225, CK_BRIGHTNESS_UP),   // KEY_BRIGHTNESSUP = 225 → keycode 233
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 锚点：编辑动作键（hid_keyboard[0x74..0x7E] 直落同义 evdev 码）。
    #[test]
    fn anchor_edit_action_keys() {
        let expect = [
            (128u8, KEY_STOP), // <STOP> = 136
            (129, KEY_AGAIN),  // <AGAI> = 137
            (131, KEY_UNDO),   // <UNDO> = 139
            (133, KEY_COPY),   // <COPY> = 141
            (135, KEY_PASTE),  // <PAST> = 143
            (136, KEY_FIND),   // <FIND> = 144
            (137, KEY_CUT),    // <CUT> = 145
            (138, KEY_HELP),   // <HELP> = 146
        ];
        for (evdev, key) in expect {
            assert_eq!(keycode_to_xkey(evdev + 8), Some(key), "evdev {evdev}");
            assert_eq!(xkey_to_keycode(key), Some(evdev + 8), "{key:?}");
        }
    }

    /// 未知 keycode 拒绝：X 保留区（0–7）与表外 evdev 码（语言键段、内核怪癖段、缝隙）。
    #[test]
    fn unknown_keycodes_rejected() {
        let expect = [
            0u8, // X 保留区
            3,   // X 保留区
            7,   // X 保留区上界
            92,  // evdev 84 缝隙
            93,  // KEY_ZENKAKUHANKAKU（无 proto 常量）
            109, // KEY_LINEFEED（无 proto 常量）
            129, // KEY_KPCOMMA（无 proto 常量）
            140, // KEY_FRONT ← Select 怪癖（有意不映射）
            142, // KEY_OPEN ← Execute 怪癖（有意不映射）
            255, // evdev 247 = KEY_RFKILL（proto 未收录）
        ];
        for keycode in expect {
            assert!(
                keycode_to_xkey(keycode).is_none(),
                "keycode {keycode} 应不映射"
            );
        }
    }

    /// 反向表缺口（有意不映射，模块文档逐项）：注入时按 Failed 计数。
    #[test]
    fn reverse_gaps_are_intentional() {
        for key in [
            xkey::KEY_NONUS_HASH, // 内核与 KEY_BACKSLASH 同落 43
            xkey::KEY_MUTE,       // 键盘页音量三键：反向取消费者页 CK_*（模块文档）
            xkey::KEY_VOLUME_UP,
            xkey::KEY_VOLUME_DOWN,
            xkey::KEY_EXECUTE, // 内核 Execute→KEY_OPEN 怪癖
            xkey::KEY_SELECT,  // 内核 Select→KEY_FRONT 怪癖
            xkey::CK_PAUSE,    // KEY_PAUSE(0x48) 已占 KEY_PAUSE(119) 码
            xkey::CK_POWER,    // KEY_POWER(0x66) 已占 KEY_POWER(116) 码
            xkey::CK_RESET,    // 内核消费者页 0x31 → KEY_RESTART（proto 未收录对应）
        ] {
            assert!(xkey_to_keycode(key).is_none(), "{key:?} 应无 keycode 对应");
        }
    }

    /// 全表 XKey 值域合法（XKey::new 校验语义在 proto 侧，此处冗余断言防表写错区段）。
    #[test]
    fn all_xkeys_in_valid_ranges() {
        for (_, key) in EVDEV_TABLE {
            assert!(XKey::is_valid_raw(key.raw()), "0x{:04X} 越界", key.raw());
        }
    }

    /// 消费者页偏移语义抽检（CK_* = 0x100 + usage，docs/03 §2）。
    #[test]
    fn consumer_offsets_match_proto() {
        assert_eq!(xkey::CK_VOLUME_UP.raw(), 0x100 + 0xE9);
        assert_eq!(xkey::CK_MUTE.raw(), 0x100 + 0xE2);
        assert_eq!(xkey::CK_PLAY_PAUSE.raw(), 0x100 + 0xCD);
        assert_eq!(xkey::CK_SCAN_NEXT_TRACK.raw(), 0x100 + 0xB5);
    }
}
