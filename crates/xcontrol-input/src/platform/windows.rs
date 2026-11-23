//! Windows 输入实现（docs/05 §2、ADR-0003 决策 1/2）：`WH_KEYBOARD_LL`/`WH_MOUSE_LL`
//! 捕获 + `SendInput` 注入 + `GetCursorPos`/`GetSystemMetrics` 光标与虚拟屏查询。
//!
//! # 权限现实（docs/05 §2 / §6：无强制权限；UIPI 是运行时边界）
//!
//! 安装 LL hook 与调用 `SendInput` **不需要管理员**；但 **UIPI**（用户界面特权隔离）
//! 决定真实可达范围：本进程为普通完整性级别时——
//!
//! - **注入**：对**提权进程**（任务管理器等）前台窗口的注入被系统静默丢弃
//!   （`SendInput` 返回值仍报告成功——失败不体现在返回值上）；
//! - **捕获**：LL hook 收不到定向到提权前台窗口的键盘输入（按键被 UIPI 隔离），
//!   鼠标钩子不受影响。
//!
//! 打包侧按 docs/05 §2 提供"以管理员运行"可选项；检测用 `IsUserAnAdmin` 分支提示
//! （FR-6.3，M4 向导接线）。运行时**不报错**与 mac 未授权时的静默语义同型：是否可用
//! 必须显式检测，不能以 hook 创建成功为凭。
//!
//! # 回调纪律（platform-code.md 回调纪律 2：LowLevelHooksTimeout）
//!
//! LL hook 回调运行在安装线程（专用捕获线程）上，受注册表
//! `LowLevelHooksTimeout`（默认 300ms 起）约束：**超时未返回的回调会被系统静默
//! 摘除**，之后本进程的捕获永久失效（不报错）。因此两个 `extern "system"` 回调内
//! **只做**：结构体字段读取 → 扫描码/消息转换（查静态表）→ `std::sync::mpsc`
//! 投递 → `CallNextHookEx`。零网络、零文件、零分配大对象、零 tokio
//! （architecture-rules 红线 3）；唯一的锁是 [`HOOK_SHARED`] 的 `Mutex`——热路径上
//! 恒无竞争（宿主线程仅在 start/stop 各碰一次该锁），无竞争 std Mutex 加锁 ≈ 一次
//! 原子 CAS，低于回调预算三个数量级。
//!
//! **不吞事件**（docs/05 §2"M2 语义同 mac"）：两个回调恒返回
//! [`CallNextHookEx`] 的结果，被过滤的事件（injected/未映射键）也照常透传——
//! M3 的捕获是 ListenOnly，拦截留给热键任务。
//!
//! # 防回环：过滤 injected 事件（与 mac 后端的语义差异）
//!
//! `SendInput` 注入的事件会**重新进入**本进程的 LL hook（`LLKHF_INJECTED`/
//! `LLMHF_INJECTED` 标记）。若不过滤，同一台机器捕获+注入并存时会把自身注入回灌
//! 进事件流（主从回环）。因此本模块**丢弃**一切 injected 事件——与 mac 后端
//! （CGEventTap ListenOnly 不过滤自注入，回环测试依赖此行为）有意不同：Windows
//! 侧的注入事件自带标记、过滤零成本且是 rustdesk/lan-mouse 同款防线。端到端回环
//! 测试（注入→捕获断言）在 Windows 上因此不可行，改由移交清单的实机两进程对拍
//! 覆盖（见模块尾部 tests 与任务报告移交清单）。
//!
//! # 线程模型
//!
//! [`WinCaptureSource::start`] 创建专用线程：安装两个 LL hook → 就绪握手（回传
//! 线程 id）→ `GetMessageW` 消息泵阻塞循环。停止 = 摘除静态共享（迟到的回调直接
//! 透传）→ `PostThreadMessageW(WM_QUIT)` → 泵退出 → `UnhookWindowsHookEx` × 2 →
//! join。回调线程零 async（红线 3）；与 tokio 的桥接由上层经
//! [`crate::InputSource::poll`] 拉取。
//!
//! # 全局热键（docs/05 §2 热键行，M4-hotkeys）
//!
//! [`WinHotkeySource`]：`RegisterHotKey`（message-only 窗口线程，剪贴板监听器
//! 同款线程/泵/静态桥模型）+ `WM_HOTKEY` → [`HotkeyId`] mpsc。两端同型注册
//! （FR-2.5）；组合被 OS 消费（不投递前台应用），LL hook 吞键**不采用**（与
//! `CallNextHookEx` 恒先行的超时纪律冲突且与 RegisterHotKey 语义重复——裁定
//! 记录见 [`WinHotkeySource`] doc）。主键 → VK 映射见 [`crate::winkeymap::
//! xkey_to_vk`]（覆盖范围注释钉死）。
//!
//! # API 版本依据（design.md：以 windows crate 实际源码为准）
//!
//! 绑定：`windows 0.62`（微软官方 rust bindings，纯 Rust 无 C 编译；docs/01 §6
//! 平台行 "`windows`（win）" 的具体化）。全部签名逐项对 registry 源码
//! `windows-0.62.2/src/Windows/Win32/...` 核实（2026-11-17）：
//!
//! - `SetWindowsHookExW(WINDOWS_HOOK_ID, HOOKPROC, Option<HINSTANCE>, u32)
//!   -> windows_core::Result<HHOOK>`；`HOOKPROC = Option<unsafe extern "system"
//!   fn(i32, WPARAM, LPARAM) -> LRESULT>`——真 unsafe FFI（无安全封装先例，本模块
//!   每个 unsafe 块附 `// SAFETY:` 不变量）；
//! - `GetMessageW(*mut MSG, Option<HWND>, u32, u32) -> windows_core::BOOL`
//!   （`BOOL(i32)`：0 = WM_QUIT/错误、-1 = 错误）；
//! - `PostThreadMessageW(u32, u32, WPARAM, LPARAM) -> windows_core::Result<()>`；
//! - `SendInput(&[INPUT], i32) -> u32`（0.62 已改切片形参）；
//! - `GetCursorPos(*mut POINT) -> windows_core::Result<()>`；
//! - `GetSystemMetrics(SYSTEM_METRICS_INDEX) -> i32`；
//! - LL 结构体：`KBDLLHOOKSTRUCT { vkCode: u32, scanCode: u32,
//!   flags: KBDLLHOOKSTRUCT_FLAGS(u32), .. }`（`LLKHF_EXTENDED=1`、
//!   `LLKHF_INJECTED=16`、`LLKHF_UP=128`）；`MSLLHOOKSTRUCT { pt: POINT,
//!   mouseData: u32, flags: u32, .. }`（`LLMHF_INJECTED=1`——0.62 值与
//!   MS Learn《MSLLHOOKSTRUCT》当前文档一致，与 winuser.h 历史宏 0x10 不同）；
//! - `INPUT { r#type: INPUT_TYPE, Anonymous: INPUT_0 { mi/ki/hi } }` 联合体构造
//!   为安全操作（仅写不读）。
//!
//! # 已知限制（M3 范围，报告/移交项）
//!
//! - **相对移动受系统鼠标加速影响**：`MOUSEEVENTF_MOVE` 相对注入会被"提高指针
//!   精确度"（Enhance Pointer Precision）加速/减速——f64 累加器保证协议侧余量
//!   不丢，但物理位移比例失真。精确定位路径一律走绝对注入（[`MOUSEEVENTF_ABSOLUTE`] +
//!   `VIRTUALDESK` 归一化）。移交清单含加速开关实测项。
//! - **KeyRepeat 不区分**：LL hook 的 `KBDLLHOOKSTRUCT` 无 autorepeat 标记（长按
//!   产生一串普通 down）——捕获侧恒发 [`InputEvent::Key`] Down，不发
//!   [`InputEvent::KeyRepeat`]（docs/03 §3：该事件可选，从机可忽略，mac 侧才产）。
//! - **ScreenId = 虚拟桌面单一表面**：注入/换算把 `ScreenId 0` 解释为整个虚拟桌面
//!   （`MOUSEEVENTF_VIRTUALDESK` 0..65535 归一化），非 0 拒绝。逐屏枚举
//!   （`EnumDisplayMonitors`）与逐屏几何留 M3 屏幕布局任务（mac 的
//!   `display_layout` 对应物届时补齐，docs/05 §2 光标行）。
//! - **滚轮轴语义与 mac 轴 2 相反**：`WM_MOUSEWHEEL` 正值 = 向上（与 mac 轴 1
//!   同向）；`WM_MOUSEHWHEEL` 正值 = 向右（mac 轴 2 正值 = 向左）——跨平台符号
//!   归一属映射引擎（M3）职责，本模块保持 Windows 原生语义并在捕获/注入两侧
//!   对称（Windows↔Windows 往返一致）。
//! - **ISO 国际键/消费键缺口**：见 [`crate::winkeymap`] 模块文档（有意不映射）。

#![allow(unsafe_code)] // docs/08 §2：平台绑定层是唯一预期 unsafe 的位置；SAFETY 注释逐块强制。

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOD_ALT,
    MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, RegisterHotKey, SendInput,
    UnregisterHotKey, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetMessageW,
    GetSystemMetrics, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP,
    LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WHEEL_DELTA, WM_HOTKEY,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, XBUTTON1, XBUTTON2,
};
use windows::core::{PCWSTR, w};
use xcontrol_core::hotkey::{HotkeyCombo, HotkeyId, HotkeySet, ModFamilies};
use xcontrol_core::sink::{InputSink, SinkError};
use xcontrol_proto::input::{Axis, InputEvent, KeyState, Mods, MouseButton, ScreenId};
use xcontrol_proto::xkey::XKey;

use crate::error::InputError;
use crate::winkeymap;

// ============================================================
// 共享状态（LL hook 回调是裸 FFI，不能捕获环境——经静态 Mutex 桥接）
// ============================================================

/// LL hook 回调可访问的共享投递状态（静态槽内；`start` 置入、`shutdown` 摘除）。
///
/// 字段即回调热路径的全部读写面：`sender`（mpsc 投递）、`mods_bits`（修饰键快照
/// 位，docs/02 §4.2 bit0-7）、`last_mouse_{x,y}`（WM_MOUSEMOVE 无 delta 字段，
/// 与上一事件坐标做差得到相对位移——AHK/rustdesk 同法）。
struct HookShared {
    sender: Sender<InputEvent>,
    mods_bits: Arc<AtomicU32>,
    last_mouse_x: AtomicI32,
    last_mouse_y: AtomicI32,
}

/// 回调 ↔ 宿主的静态桥（`Mutex<Option<_>>`：Sync by construction，无需 unsafe impl）。
///
/// 竞争面：回调每次事件加锁、宿主仅 start/stop 各一次——热路径恒无竞争
/// （回调纪律论证见模块文档）。`lock().ok()`：回调线程不 panic（无 unwrap），
/// 中毒态下静默透传事件（钩子不吞）。
static HOOK_SHARED: Mutex<Option<HookShared>> = Mutex::new(None);

/// 以共享状态执行 `f`（无共享 = 捕获已停止，返回 `None` 且调用方透传事件）。
fn with_shared<T>(f: impl FnOnce(&HookShared) -> T) -> Option<T> {
    let guard = HOOK_SHARED.lock().ok()?;
    guard.as_ref().map(f)
}

// ============================================================
// LL hook 回调（真 unsafe FFI：extern "system"，windows crate 裸 API）
// ============================================================

/// `WH_KEYBOARD_LL` 回调（签名 = windows 0.62 `HOOKPROC` 逐参一致，源码核实）。
///
/// # SAFETY（系统约束，Rust 侧无从保证、只能立约）
///
/// - 由系统在安装线程上以 (ncode, wparam, lparam) 调用：`ncode < 0` 时必须立即
///   `CallNextHookEx` 透传且**不得**读 `lparam`（Win32 LowLevelKeyboardProc 契约）；
/// - `ncode == HC_ACTION(0)` 时 `lparam` 指向系统所有的 `KBDLLHOOKSTRUCT`，
///   仅在本回调栈帧内读取、不存储指针、不另起线程引用（回调返回即失效）；
/// - 本函数不重入系统输入 API（不调用 SendInput/GetMessage 系），无死锁面；
/// - 返回值恒为 [`CallNextHookEx`] 结果（不吞事件，模块文档）。
unsafe extern "system" fn keyboard_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 先取透传值（任何分支都必须以它收尾——钩子超时摘除风险的另一半防线：
    // 转发先于一切可选工作）。
    // SAFETY: CallNextHookEx 按契约转发原参数（ncode/wparam/lparam 为系统原样
    // 传入）；LL hook 的 hhk 参数恒传 None（Win32 文档明确忽略该参数）。
    let pass = unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    if ncode < 0 {
        return pass; // 非 HC_ACTION：不得触碰 lparam
    }
    let msg = wparam.0 as u32;
    if matches!(msg, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP) {
        // SAFETY: ncode >= 0 时 lparam 是系统提供的 KBDLLHOOKSTRUCT 指针
        // （LowLevelKeyboardProc 契约），本回调栈帧内只读解引用、不外泄。
        let kbdll = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        translate_keyboard(kbdll);
    }
    pass
}

/// `WH_MOUSE_LL` 回调（约束同 [`keyboard_proc`]；lparam 指向 `MSLLHOOKSTRUCT`）。
///
/// # SAFETY
///
/// 同 [`keyboard_proc`]：ncode < 0 立即透传；结构体只在回调栈帧内只读；
/// 恒返回 [`CallNextHookEx`] 结果。
unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: 同 keyboard_proc——原参数契约转发；hhk 恒 None。
    let pass = unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    if ncode < 0 {
        return pass;
    }
    let msg = wparam.0 as u32;
    if matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    ) {
        // SAFETY: ncode >= 0 时 lparam 是系统提供的 MSLLHOOKSTRUCT 指针
        // （LowLevelMouseProc 契约），本回调栈帧内只读解引用、不外泄。
        let msll = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        translate_mouse(msll, msg);
    }
    pass
}

// ============================================================
// 事件转换（回调线程主体：只做语义转换 + mpsc 投递）
// ============================================================

/// 键盘 LL 事件 → 协议事件（回调线程；零慢操作）。
///
/// injected 事件直接丢弃（防回环，模块文档）；未映射扫描码拒绝（[`crate::winkeymap`]）；
/// 接收端关闭时 `send` 失败静默忽略（捕获正在关闭）。
fn translate_keyboard(kbdll: &KBDLLHOOKSTRUCT) {
    with_shared(|shared| {
        // injected：SendInput 事件回流（本进程或他进程合成）——丢弃防回环。
        if kbdll.flags.contains(LLKHF_INJECTED) {
            return;
        }
        let extended = kbdll.flags.contains(LLKHF_EXTENDED);
        let up = kbdll.flags.contains(LLKHF_UP);
        // scanCode 为单字节 set-1 码（E0 前缀走 extended 标志，winkeymap 模块文档）；
        // u32→u16 截断仅为防御，正常单字节值不截断，异常值查表不中即拒绝。
        let scan = kbdll.scanCode as u16;
        let Some(code) = winkeymap::scan_to_xkey(scan, extended) else {
            return;
        };
        let state = if up { KeyState::Up } else { KeyState::Down };
        if let Some(bit) = code.mods_bit() {
            let mask = 1_u32 << bit;
            if up {
                shared.mods_bits.fetch_and(!mask, Ordering::Relaxed);
            } else {
                shared.mods_bits.fetch_or(mask, Ordering::Relaxed);
            }
        }
        // 长按重复不区分（KBDLLHOOKSTRUCT 无 autorepeat 标记，模块文档已知限制）：
        // 重复 down 恒发 Key Down（docs/03 §3 KeyRepeat 可选，从机可忽略）。
        let _ = shared.sender.send(InputEvent::Key { code, state });
    });
}

/// 鼠标 LL 事件 → 协议事件（回调线程；零慢操作）。
fn translate_mouse(msll: &MSLLHOOKSTRUCT, msg: u32) {
    // injected 过滤（LLMHF_*：0.62 常量值与 MS Learn 当前文档一致，模块文档）。
    if msll.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0 {
        return;
    }
    with_shared(|shared| {
        let event = match msg {
            WM_MOUSEMOVE => {
                // LL 鼠标事件无 delta 字段：与上一事件坐标做差（AHK/rustdesk 同法）。
                let dx = msll.pt.x - shared.last_mouse_x.swap(msll.pt.x, Ordering::Relaxed);
                let dy = msll.pt.y - shared.last_mouse_y.swap(msll.pt.y, Ordering::Relaxed);
                if dx == 0 && dy == 0 {
                    return; // 静止事件（warp 后的确认移动等）不转发
                }
                Some(InputEvent::MouseMoveRel {
                    dx: dx as f32,
                    dy: dy as f32,
                })
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP => Some(mouse_button(MouseButton::Left, msg)),
            WM_RBUTTONDOWN | WM_RBUTTONUP => Some(mouse_button(MouseButton::Right, msg)),
            WM_MBUTTONDOWN | WM_MBUTTONUP => Some(mouse_button(MouseButton::Middle, msg)),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                // mouseData 低字 = XBUTTON 编号（高字保留）：1=后退、2=前进
                // （docs/02 §4.2 button 表 3/4；与 mac kCGMouseEventButtonNumber 对称）。
                let button = match (msll.mouseData & 0xFFFF) as u16 {
                    XBUTTON1 => MouseButton::Back,
                    XBUTTON2 => MouseButton::Forward,
                    _ => return,
                };
                Some(mouse_button(button, msg))
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = wheel_notches(msll.mouseData);
                if delta == 0.0 {
                    return;
                }
                Some(InputEvent::Wheel {
                    axis: if msg == WM_MOUSEHWHEEL {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    },
                    delta,
                })
            }
            _ => None,
        };
        // 接收端关闭（捕获停止中）时静默忽略——回调不得因投递失败做任何补救。
        if let Some(event) = event {
            let _ = shared.sender.send(event);
        }
    });
}

/// 按钮 down/up 事件（`msg` = WM_*DOWN/UP 之一；坐标不进协议——docs/02 §4.2
/// kind 0x05 仅 button+state）。
fn mouse_button(button: MouseButton, msg: u32) -> InputEvent {
    let is_up =
        msg == WM_LBUTTONUP || msg == WM_RBUTTONUP || msg == WM_MBUTTONUP || msg == WM_XBUTTONUP;
    InputEvent::MouseButton {
        button,
        state: if is_up { KeyState::Up } else { KeyState::Down },
    }
}

/// 滚轮 mouseData → 格（notch）数：高字 i16 ÷ WHEEL_DELTA（docs/02 §4.2 delta
/// 单位=格、保留小数以支持高分滚轮；120 = 1 格）。纯函数。
#[must_use]
fn wheel_notches(mouse_data: u32) -> f32 {
    let raw = (mouse_data >> 16) as u16;
    // WHEEL_DELTA(120) 是 f32 精确可表示的整数（From<u32> 不存在——非无损转换）。
    f32::from(raw as i16) / (WHEEL_DELTA as f32)
}

/// 注入用滚轮 mouseData：格数 × WHEEL_DELTA（i32 位型经补码回绕为 u32，系统侧
/// 按有符号解释）。纯函数。
#[must_use]
const fn wheel_raw(notches: i32) -> u32 {
    notches.wrapping_mul(WHEEL_DELTA as i32) as u32
}

// ============================================================
// 光标与虚拟屏几何（docs/05 §2 光标行；M2 edge 接线的查询面）
// ============================================================

/// 虚拟桌面几何快照（所有显示器并集的包围矩形，全局屏幕坐标，y 轴向下）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualScreenInfo {
    /// 左上角原点（可为负——左侧/上方副屏把并集矩形拉出第一象限）。
    pub origin: (i32, i32),
    /// 并集尺寸（像素；为 0 时查询无效）。
    pub size: (i32, i32),
}

/// 查询虚拟桌面几何（`GetSystemMetrics(SM_X/YVIRTUALSCREEN, SM_CX/CYVIRTUALSCREEN)`，
/// docs/05 §2"光标"行）。纯查询、无权限要求。
///
/// # Errors
/// [`InputError::Platform`]：尺寸非正（多屏会话理论必有正并集，防御性报错）。
pub fn virtual_screen_info() -> Result<VirtualScreenInfo, InputError> {
    let origin = (
        // SAFETY: GetSystemMetrics 是无副作用系统常量查询（win32 契约：任意线程可调）。
        unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
    );
    let size = (
        // SAFETY: 同上。
        unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
        unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
    );
    if size.0 <= 0 || size.1 <= 0 {
        return Err(InputError::Platform {
            action: "virtual-screen-query",
            reason: format!("GetSystemMetrics 虚拟桌面尺寸异常：{size:?}"),
        });
    }
    Ok(VirtualScreenInfo { origin, size })
}

/// 当前光标位置（全局屏幕坐标；docs/05 §2：`GetCursorPos` 无需权限）。
///
/// # Errors
/// [`InputError::Platform`]：坐标获取失败（会话断开等罕见态）。
pub fn cursor_location() -> Result<(f64, f64), InputError> {
    let mut point = POINT::default();
    // SAFETY: point 是栈上合法出参（POINT 默认零值），GetCursorPos 写入后即用；
    // 0.62 签名返回 Result<()>（失败已转 Err），无指针外泄。
    unsafe { GetCursorPos(&mut point) }.map_err(|error| InputError::Platform {
        action: "cursor-query",
        reason: format!("GetCursorPos 失败：{error}"),
    })?;
    Ok((f64::from(point.x), f64::from(point.y)))
}

// ============================================================
// 注入器（InputSink 实现）
// ============================================================

/// 把 [`f64`] 累加器与一条增量合并，产出整数注入量与保留的小数余量
/// （docs/02 §4.2：取整不丢余量，净移动量守恒；mac 后端同款纯函数）。
fn accumulate(acc: f64, delta: f32) -> (f64, i32) {
    let total = acc + f64::from(delta);
    let integral = total.trunc();
    // float→int `as` 转换自 Rust 1.45 起饱和；f32 增量路径下 i32 溢出仅出现在
    // 单轴 >2^31 单位的病态输入，饱和即合理上限。
    (total - integral, integral as i32)
}

/// 归一化坐标（0.0–1.0）→ `MOUSEEVENTF_ABSOLUTE|VIRTUALDESK` 的 0..65535 整数坐标
/// （越界收敛边界，docs/02 §4.2 kind 0x04）。纯函数。
fn abs_to_virtual(nx: f32, ny: f32) -> (i32, i32) {
    let ax = (f64::from(nx).clamp(0.0, 1.0) * 65535.0).round() as i32;
    let ay = (f64::from(ny).clamp(0.0, 1.0) * 65535.0).round() as i32;
    (ax, ay)
}

/// [`XKey`] → 按键注入单元（KEYEVENTF_SCANCODE 物理键位路径；未知键返回 `None`）。
///
/// 扩展键：扫描码取单字节 + `KEYEVENTF_EXTENDEDKEY` 标志（系统合成 E0 前缀，
/// [`crate::winkeymap`] 模块文档）。`wVk` 置 0——scancode 路径下 VK 由系统按布局
/// 解析（物理键位语义，FR-2.6）。
fn key_input(code: XKey, up: bool) -> Option<INPUT> {
    let (scan, extended) = winkeymap::xkey_to_scan(code)?;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    Some(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
}

/// 鼠标按键注入单元（XBUTTON 键号经 `mouseData` 低字承载）。
fn button_input(button: MouseButton, up: bool) -> INPUT {
    let (down_flag, up_flag, data) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0_u32),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        MouseButton::Back => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
        MouseButton::Forward => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
    };
    let flags = if up { up_flag } else { down_flag };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 相对移动注入单元（`MOUSEEVENTF_MOVE`；**受系统鼠标加速影响**，模块文档已知限制）。
fn move_rel_input(dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 绝对定位注入单元（`MOUSEEVENTF_ABSOLUTE|VIRTUALDESK`：0..65535 覆盖整个虚拟桌面，
/// 多屏并集坐标系，模块文档 ScreenId 限制）。
fn move_abs_input(ax: i32, ay: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: ax,
                dy: ay,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 滚轮注入单元（`MOUSEEVENTF_WHEEL/HWHEEL`，`mouseData` = 格 × WHEEL_DELTA）。
fn wheel_input(axis: Axis, notches: i32) -> INPUT {
    let flag = match axis {
        Axis::Vertical => MOUSEEVENTF_WHEEL,
        Axis::Horizontal => MOUSEEVENTF_HWHEEL,
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: wheel_raw(notches),
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Unicode 文本注入单元对（`KEYEVENTF_UNICODE`：UTF-16 单元经 `wScan` 承载，
/// `wVk` 置 0；**不经扫描码表**，docs/03 §4.5）。代理对每单元独立 down/up。
fn unicode_input(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Windows 注入器（[`InputSink`] 的 Windows 实现，docs/05 §2：`SendInput`）。
///
/// 状态：跟踪已注入按键/按钮集合（[`InputSink::release_all`] 防卡键收尾）与
/// 移动/滚轮 f64 累加器（docs/02 §4.2 Windows 整数化必需）。非阻塞快速返回
/// （trait 契约）；单次注入失败返回 [`SinkError::Failed`]，调用方（core route 层）
/// 计数并继续，不得中断事件流（error-handling.md §4）。**UIPI**：对提权进程的
/// 注入被系统静默丢弃且 `SendInput` 返回值不体现（模块文档）。
pub struct WinInjector {
    /// 虚拟桌面几何（绝对定位归一化基准；`refresh_layout` 随显示器热插拔刷新）。
    virtual_screen: VirtualScreenInfo,
    /// 已注入且未释放的按键（按下序；release_all 按逆序释放）。
    pressed_keys: Vec<XKey>,
    /// 已注入且未释放的鼠标按键（按下序）。
    pressed_buttons: Vec<MouseButton>,
    /// 相对移动累加器（x/y 小数余量，docs/02 §4.2）。
    move_x: f64,
    move_y: f64,
    /// 滚轮累加器（v/h 小数余量；整数格注入，余量保留）。
    wheel_v: f64,
    wheel_h: f64,
}

impl WinInjector {
    /// 构建注入器（查询虚拟桌面几何；无需任何权限——注入失败是运行时 UIPI 行为，
    /// 构建恒成功，模块文档）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：虚拟桌面几何查询失败。
    pub fn new() -> Result<Self, InputError> {
        let virtual_screen = virtual_screen_info()?;
        Ok(Self {
            virtual_screen,
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
            move_x: 0.0,
            move_y: 0.0,
            wheel_v: 0.0,
            wheel_h: 0.0,
        })
    }

    /// 重新查询虚拟桌面几何（显示器热插拔/分辨率变化后调用）。
    ///
    /// # Errors
    /// 同 [`WinInjector::new`]。
    pub fn refresh_layout(&mut self) -> Result<(), InputError> {
        self.virtual_screen = virtual_screen_info()?;
        Ok(())
    }

    /// 批量提交 `SendInput`（docs/05 §2：数组一次提交多事件，系统保证不被其他
    /// 输入穿插；`release_all` 的逆序释放序列同样整批提交）。
    ///
    /// # Errors
    /// [`SinkError::Failed`]：提交数 < 请求数（被 BlockInput 等机制拦截）。
    fn send_batch(&self, inputs: Vec<INPUT>, action: &'static str) -> Result<(), SinkError> {
        if inputs.is_empty() {
            return Ok(());
        }
        // SAFETY: inputs 为连续合法 INPUT 数组，生命周期覆盖本调用；cbsize 按
        // Win32 契约传 sizeof(INPUT)；SendInput 是线程安全的系统调用，注入事件
        // 不会重入本回调线程（捕获侧 injected 过滤兜底，模块文档）。
        let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
        if sent != inputs.len() as u32 {
            return Err(SinkError::Failed {
                action,
                reason: format!(
                    "SendInput 提交 {}/{}（被输入拦截机制阻断？）",
                    sent,
                    inputs.len()
                ),
            });
        }
        Ok(())
    }

    /// 注入一条按键事件并更新跟踪集合（先记账后注入，与 mac 后端一致）。
    fn inject_key(&mut self, code: XKey, state: KeyState) -> Result<(), SinkError> {
        let Some(input) = key_input(code, state == KeyState::Up) else {
            return Err(SinkError::Failed {
                action: "inject",
                reason: format!("XKey 0x{:04X} 无 Windows 扫描码对应", code.raw()),
            });
        };
        match state {
            KeyState::Down => {
                if !self.pressed_keys.contains(&code) {
                    self.pressed_keys.push(code);
                }
            }
            KeyState::Up => self.pressed_keys.retain(|pressed| *pressed != code),
        }
        self.send_batch(vec![input], "inject")
    }

    /// 注入滚轮事件（整数格注入 + 小数余量保留；正负号沿用 Windows 原生语义，
    /// 见模块文档轴 2 说明）。
    fn inject_wheel(&mut self, axis: Axis, delta: f32) -> Result<(), SinkError> {
        if !delta.is_finite() {
            return Err(SinkError::Failed {
                action: "inject",
                reason: "滚轮 delta 非有限值（协议违规不应到达注入层）".into(),
            });
        }
        let (acc, rest) = match axis {
            Axis::Vertical => (self.wheel_v, &mut self.wheel_v),
            Axis::Horizontal => (self.wheel_h, &mut self.wheel_h),
        };
        let (remainder, notches) = accumulate(acc, delta);
        *rest = remainder;
        if notches == 0 {
            return Ok(()); // 纯小数余量：无可注入整数格
        }
        self.send_batch(vec![wheel_input(axis, notches)], "inject")
    }
}

impl InputSink for WinInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        match event {
            InputEvent::Key { code, state } => self.inject_key(*code, *state),
            InputEvent::KeyRepeat { code } => {
                // 长按重复：仅当键仍在本机按下集合时补发 keyDown（docs/03 §3：
                // 从机可按平台能力忽略；Windows 合成 down 不自动重复，与 mac 同构）。
                if !self.pressed_keys.contains(code) {
                    return Ok(());
                }
                let Some(input) = key_input(*code, false) else {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: format!("XKey 0x{:04X} 无 Windows 扫描码对应", code.raw()),
                    });
                };
                self.send_batch(vec![input], "inject")
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: "相对移动 delta 非有限值（NaN 会沿累加器扩散，docs/02 §4.2）"
                            .into(),
                    });
                }
                let (rest_x, int_x) = accumulate(self.move_x, *dx);
                let (rest_y, int_y) = accumulate(self.move_y, *dy);
                self.move_x = rest_x;
                self.move_y = rest_y;
                if int_x == 0 && int_y == 0 {
                    return Ok(()); // 纯小数余量：无可注入整数位移
                }
                self.send_batch(vec![move_rel_input(int_x, int_y)], "inject")
            }
            InputEvent::MouseMoveAbs { screen, x, y } => self.warp(*screen, *x, *y),
            InputEvent::MouseButton { button, state } => {
                match state {
                    KeyState::Down => {
                        if !self.pressed_buttons.contains(button) {
                            self.pressed_buttons.push(*button);
                        }
                    }
                    KeyState::Up => self.pressed_buttons.retain(|pressed| pressed != button),
                }
                self.send_batch(
                    vec![button_input(*button, *state == KeyState::Up)],
                    "inject",
                )
            }
            InputEvent::Wheel { axis, delta } => self.inject_wheel(*axis, *delta),
            InputEvent::Text { utf8 } => {
                let bytes = utf8.as_bytes();
                if bytes.is_empty() {
                    return Ok(());
                }
                if bytes.len() > xcontrol_proto::input::MAX_TEXT_LEN {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: format!(
                            "Text 超过 512B 上限（{}B）；主控应分段（docs/03 §4.5）",
                            bytes.len()
                        ),
                    });
                }
                // UTF-16 单元序列（含代理对），每单元 down/up 成对，整批一次提交。
                let units = utf8.encode_utf16().collect::<Vec<u16>>();
                let mut inputs = Vec::with_capacity(units.len() * 2);
                for unit in units {
                    inputs.push(unicode_input(unit, false));
                    inputs.push(unicode_input(unit, true));
                }
                self.send_batch(inputs, "inject")
            }
            // —— 路由/会话层事件（docs/02 §4.2 kind 0x10–0x21）：非设备注入，
            //    由 core 会话对齐机制消费；到达本层属冗余投递，静默忽略不算错误
            //    （error-handling.md §4；mac 后端同构）。——
            InputEvent::EnterScreen { .. }
            | InputEvent::LeaveScreen { .. }
            | InputEvent::ModifierSync { .. }
            | InputEvent::SwitchRequest { .. }
            | InputEvent::SwitchAck { .. } => Ok(()),
        }
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(SinkError::Failed {
                action: "warp",
                reason: "warp 坐标非有限值".into(),
            });
        }
        // M3：ScreenId 0 = 整个虚拟桌面（模块文档已知限制；逐屏枚举属屏幕布局任务）。
        if screen != 0 {
            return Err(SinkError::Failed {
                action: "warp",
                reason: format!(
                    "未知 ScreenId {screen}（Windows M3 仅虚拟桌面单表面 = 0，{}×{}）",
                    self.virtual_screen.size.0, self.virtual_screen.size.1
                ),
            });
        }
        let (ax, ay) = abs_to_virtual(x, y);
        self.send_batch(vec![move_abs_input(ax, ay)], "warp")
    }

    /// 释放全部跟踪中的按键/按钮（切出/暂停/断开防卡键，docs/03 §3）。
    ///
    /// 按按下逆序释放（后按的先起），**整批一次 `SendInput` 提交**（系统保证
    /// 序列不穿插）；移动/滚轮累加器一并清零（防跨会话余量漂移）。
    ///
    /// # Errors
    /// [`SinkError::Failed`]：批量提交失败（仍会清空跟踪集合，下次 release_all
    /// 幂等）。
    fn release_all(&mut self) -> Result<(), SinkError> {
        let mut inputs = Vec::new();
        for code in self.pressed_keys.iter().rev().copied().collect::<Vec<_>>() {
            // 跟踪集合仅来自成功构建的注入（理论必有反查位）；防御跳过。
            if let Some(input) = key_input(code, true) {
                inputs.push(input);
            }
        }
        self.pressed_keys.clear();
        for button in self
            .pressed_buttons
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>()
        {
            inputs.push(button_input(button, true));
        }
        self.pressed_buttons.clear();
        self.move_x = 0.0;
        self.move_y = 0.0;
        self.wheel_v = 0.0;
        self.wheel_h = 0.0;
        self.send_batch(inputs, "release_all")
    }
}

// ============================================================
// 捕获源（InputSource 实现）
// ============================================================

/// Windows 捕获源（[`crate::InputSource`] 的 Windows 实现，docs/05 §2：
/// `WH_KEYBOARD_LL`/`WH_MOUSE_LL` + 专用线程消息泵）。
///
/// - 回调只做"扫描码→[`XKey`] / 消息→协议事件 → mpsc 投递"（LowLevelHooksTimeout
///   纪律，模块文档）；事件 FIFO 保序（单 channel 天然满足）；
/// - [`crate::InputSource::poll`] 非阻塞拉取；`mods()` 读捕获线程维护的快照位
///   （Relaxed：单写者回调线程 + 宿主读，位域合法即可）；
/// - 安装 hook **无需权限**；UIPI 限流见模块文档（普通完整性级别下收不到提权
///   前台窗口的键盘输入）。
pub struct WinCaptureSource {
    receiver: Receiver<InputEvent>,
    /// 修饰键快照位（与 [`HOOK_SHARED`] 内同一 `Arc`；宿主侧无锁读）。
    mods_bits: Arc<AtomicU32>,
    /// 捕获线程 id（`PostThreadMessageW(WM_QUIT)` 定址）。
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

/// 就绪握手载荷：`Ok(捕获线程 id)`（`PostThreadMessageW(WM_QUIT)` 定址用）或
/// `Err(安装失败原因)`。hook 句柄不上送宿主——关停路径在捕获线程内 unhook（泵
/// 退出后执行，句柄生命周期不出本线程）。
type ReadyPayload = Result<u32, String>;

impl WinCaptureSource {
    /// 启动捕获（创建消息泵线程并等待其就绪：`GetModuleHandleW` → 安装双 LL hook
    /// → 握手回传线程 id → 进入 `GetMessageW` 泵）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：线程创建失败 / 模块句柄获取失败 /
    /// `SetWindowsHookExW` 被拒（极少见：系统策略）/ 就绪线程意外退出。
    pub fn start() -> Result<Self, InputError> {
        let (sender, receiver) = channel::<InputEvent>();
        let mods_bits = Arc::new(AtomicU32::new(0));
        // 鼠标 delta 基点初始化为当前光标位置：首个 WM_MOUSEMOVE 不产生巨大位移
        // 伪事件。查询失败回退 (0,0)（首个移动事件可能偏大，防御性可接受）。
        let (last_x, last_y) = cursor_location().map_or((0, 0), |(x, y)| (x as i32, y as i32));
        *HOOK_SHARED.lock().map_err(|_| InputError::Platform {
            action: "capture-start",
            reason: "捕获共享状态锁中毒（上一会话 panic 残留）".into(),
        })? = Some(HookShared {
            sender,
            mods_bits: Arc::clone(&mods_bits),
            last_mouse_x: AtomicI32::new(last_x),
            last_mouse_y: AtomicI32::new(last_y),
        });

        let (ready_tx, ready_rx) = channel::<ReadyPayload>();
        let worker = std::thread::Builder::new()
            .name("xcontrol-capture-win".into())
            .spawn(move || {
                let ready_tx = ready_tx;
                // SAFETY: GetModuleHandleW(None) 取本进程映像模块句柄（Win32 惯例：
                // LL hook 的 hmod 参数要求进程模块句柄，线程安全无副作用）。
                let hmodule = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
                    Ok(module) => module,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("GetModuleHandleW 失败：{error}")));
                        return;
                    }
                };
                // HMODULE→HINSTANCE 同为模块句柄的语义别名（win32 契约可互换）。
                let hinstance = HINSTANCE(hmodule.0);
                // SAFETY: HOOKPROC 指向本模块静态函数（进程生命周期有效，无需卸载）；
                // dwthreadid=0 = 全局 LL hook（docs/05 §2 主控捕获语义）；hmod 为
                // 合法模块句柄。SetWindowsHookExW 无重入面，线程安全。
                let keyboard_hook = match unsafe {
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(hinstance), 0)
                } {
                    Ok(hook) => hook,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!(
                            "SetWindowsHookExW(WH_KEYBOARD_LL) 失败：{error}"
                        )));
                        return;
                    }
                };
                // SAFETY: 同上（mouse_proc 同为本模块静态函数）。
                let mouse_hook = match unsafe {
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hinstance), 0)
                } {
                    Ok(hook) => hook,
                    Err(error) => {
                        // 键盘 hook 已装上：回滚后报错（半装态不可交付）。
                        // SAFETY: keyboard_hook 为上一步 SetWindowsHookExW 成功返回的
                        // 合法句柄，UnhookWindowsHookEx 恰好一次。
                        let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
                        let _ = ready_tx
                            .send(Err(format!("SetWindowsHookExW(WH_MOUSE_LL) 失败：{error}")));
                        return;
                    }
                };
                // 就绪握手：宿主 start() 必须等到此步才认为捕获可用（否则 hook 尚未
                // 安装、静默无事件——mac 后端就绪握手同构）。
                // SAFETY: GetCurrentThreadId 是无副作用系统查询（当前线程标识），
                // 任意线程可调。
                let thread_id = unsafe { GetCurrentThreadId() };
                if ready_tx.send(Ok(thread_id)).is_err() {
                    // 宿主已放弃（start 出错/被丢弃）：卸载双 hook 后退出。
                    // SAFETY: 两 hook 均为本线程成功安装的合法句柄，各恰好一次卸载。
                    let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
                    let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
                    return;
                }
                // 消息泵：LL hook 回调由本线程的消息队列驱动（Win32 契约：装钩线程
                // 必须泵消息）。GetMessageW 返回 0 = WM_QUIT（关停）、-1 = 错误，
                // 两者均退出循环；正数继续（本线程无窗口，消息仅服务钩子派发）。
                let mut msg = MSG::default();
                loop {
                    // SAFETY: msg 为栈上合法出参；hwnd=None 接收本线程全部消息；
                    // 过滤区间 0..0 = 不过滤。阻塞语义即设计（关停靠 WM_QUIT 唤醒）。
                    let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                    if ret.0 <= 0 {
                        break;
                    }
                }
                // 泵退出：先卸载 hook（此后不再有回调）再退出线程。
                // SAFETY: 两 hook 均为本线程成功安装的合法句柄，各恰好一次卸载；
                // 卸载后系统保证不再派发回调（Win32 UnhookWindowsHookEx 契约）。
                let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
                let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
            })
            .map_err(|error| InputError::Platform {
                action: "capture-spawn",
                reason: format!("捕获线程创建失败：{error}"),
            })?;

        let thread_id = ready_rx
            .recv()
            .map_err(|_| {
                // 线程已带静态共享退出：摘共享防迟到回调写死通道。
                if let Ok(mut slot) = HOOK_SHARED.lock() {
                    *slot = None;
                }
                InputError::Platform {
                    action: "capture-start",
                    reason: "捕获线程在就绪前退出（panic 或提前 return）".into(),
                }
            })?
            .map_err(|reason| {
                if let Ok(mut slot) = HOOK_SHARED.lock() {
                    *slot = None;
                }
                InputError::Platform {
                    action: "hook-install",
                    reason,
                }
            })?;

        Ok(Self {
            receiver,
            mods_bits,
            thread_id,
            worker: Some(worker),
        })
    }

    /// 停止捕获：摘静态共享 → `PostThreadMessageW(WM_QUIT)` → 泵退出 → unhook →
    /// join（顺序保证迟到的回调只透传、不投递）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：回调线程 panic。
    pub fn stop(mut self) -> Result<(), InputError> {
        self.shutdown()
    }

    /// 关停公共路径（[`WinCaptureSource::stop`] 与 [`Drop`] 共用）。
    fn shutdown(&mut self) -> Result<(), InputError> {
        // 1) 先摘共享：此后任何迟到回调在 with_shared 处短路、事件只透传不投递。
        if let Ok(mut slot) = HOOK_SHARED.lock() {
            *slot = None;
        }
        // 2) 投 WM_QUIT 唤醒消息泵（线程可能已自行退出：错误静默，join 为准）。
        // SAFETY: thread_id 是就绪握手回传的合法线程标识；WM_QUIT 为 Win32 预留
        // 消息，参数恒 (0, 0)。PostThreadMessageW 不重入输入系统。
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        // 3) join：等待泵退出 + 双 unhook 完成（unhook 在线程内做，宿主不可达句柄）。
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| InputError::Platform {
                action: "capture-stop",
                reason: "捕获线程 panic".into(),
            })?;
        }
        Ok(())
    }
}

impl Drop for WinCaptureSource {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也关停线程与 hook（幂等；join 失败在
        // Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

impl crate::InputSource for WinCaptureSource {
    fn poll(&mut self, out: &mut Vec<InputEvent>) {
        while let Ok(event) = self.receiver.try_recv() {
            out.push(event);
        }
    }

    fn mods(&self) -> Mods {
        Mods::from_bits(self.mods_bits.load(Ordering::Relaxed))
    }
}

// ============================================================
// 全局热键（docs/05 §2 热键行：RegisterHotKey 消息窗口线程，M4-hotkeys）
// ============================================================

/// WNDPROC 可访问的共享投递状态（静态槽内；`register` 置入、`shutdown` 摘除）。
///
/// 单源约定与剪贴板监听器同款（`LISTENER_SHARED` 先例）：热键窗口是进程级
/// 单例资源（窗口类 + RegisterHotKey 的 hwnd 关联），同一时刻至多一个
/// [`WinHotkeySource`]——重复注册被 app 层会话生命周期串行化（每会话一个）。
struct HotkeyShared {
    sender: Sender<HotkeyId>,
}

/// 热键回调 ↔ 宿主的静态桥（剪贴板监听器同款 `Mutex<Option<_>>`）。
static HOTKEY_SHARED: Mutex<Option<HotkeyShared>> = Mutex::new(None);

/// 热键窗口类名（进程级唯一；与剪贴板监听类名区分）。
const HOTKEY_CLASS: PCWSTR = w!("xcontrol_hotkey_window");

/// 注册顺序 id（`RegisterHotKey` 的 id 形参；WM_HOTKEY wParam 原样回传）。
/// [`HotkeyId::ALL`] 的下标即注册 id（窗口过程回查 `ALL` 还原语义）。
fn hotkey_id_index(id: HotkeyId) -> i32 {
    HotkeyId::ALL
        .iter()
        .position(|candidate| *candidate == id)
        .map_or(0, |index| index as i32)
}

/// 热键窗口 WNDPROC（裸 FFI 回调：仅 `WM_HOTKEY` → id 还原 → mpsc 投递，
/// 其余全部走 `DefWindowProcW`）。
///
/// # SAFETY（系统约束，Rust 侧无从保证、只能立约）
///
/// - 由系统在**创建该窗口的线程**（泵线程）上调用；本函数不重入输入系统
///   （不 Register/Unregister/SendInput），无死锁面；
/// - 回调内零分配大对象、零 async（回调线程纪律，platform-code.md）——仅
///   wparam 还原 + mpsc send；
/// - `wparam` = 注册时传入的 id（`RegisterHotKey` 契约），越界 id（理论不可达）
///   静默丢弃；hwnd 有效性由系统保证，不存储句柄。
unsafe extern "system" fn hotkey_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_HOTKEY {
        // WM_HOTKEY：wparam = 注册 id，lparam = 修饰键|vk（低字 mods 高字 vk，
        // 诊断用——命中语义已由注册期确定，不在此复判）。
        let index = wparam.0; // WPARAM.0 本就是 usize（指针宽度），无 cast
        if let Some(id) = HotkeyId::ALL.get(index) {
            // 无共享 = 停止中/已停止：迟到的 WM_HOTKEY 丢弃（shutdown 先摘
            // 共享的顺序保证，与剪贴板监听器同款）。
            let _ = with_shared_hotkey(|shared| shared.sender.send(*id));
        }
    }
    // 全部消息走默认过程（WM_HOTKEY 无返回值语义）。
    // SAFETY: hwnd/msg/wparam/lparam 为系统原样传入的合法参数，DefWindowProcW
    // 按契约转发（Win32 WindowProc 默认处理）。
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 以热键共享状态执行 `f`（无共享 = 捕获已停止，返回 `None` 且丢弃事件）。
fn with_shared_hotkey<T>(f: impl FnOnce(&HotkeyShared) -> T) -> Option<T> {
    let guard = HOTKEY_SHARED.lock().ok()?;
    guard.as_ref().map(f)
}

/// 家族位 → `HOT_KEY_MODIFIERS`（`RegisterHotKey` 的 fsModifiers 形参）。
///
/// Meta/Super → `MOD_WIN`（Windows 键 = XKey Meta 家族的 win 侧语义）；
/// `MOD_NOREPEAT` 恒附加（按住不重复触发 WM_HOTKEY——与 core `HotkeyFilter`
/// 的「按住不重复路由」语义对齐；Vista+ 支持，不支持的历史系统上按住会重复
/// toggle，可接受降级）。
#[must_use]
fn hotkey_modifiers(combo: HotkeyCombo) -> HOT_KEY_MODIFIERS {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let families = combo.families;
    if families.contains(ModFamilies::CTRL) {
        mods |= MOD_CONTROL;
    }
    if families.contains(ModFamilies::SHIFT) {
        mods |= MOD_SHIFT;
    }
    if families.contains(ModFamilies::ALT) {
        mods |= MOD_ALT;
    }
    if families.contains(ModFamilies::META) {
        mods |= MOD_WIN;
    }
    mods | MOD_NOREPEAT
}

/// Windows 全局热键源（[`WinHotkeySource`]：`RegisterHotKey` + message-only
/// 窗口 + `WM_HOTKEY`，docs/05 §2 热键行）。
///
/// - **两端同型注册**（FR-2.5「热键在两端注册为本地全局热键」）：主控命中 =
///   本地路由不转发；从机命中 = 本地动作（Pause toggle 等，无对端切换信令）；
/// - **组合被系统消费**：`RegisterHotKey` 命中的按键不投递前台应用（OS 级
///   吞键）——「热键不被本地处理以外的路径消费」由 OS 语义直接保证，LL hook
///   无需吞键（win 捕获可吞的能力留作 M5 捕获链的备选，本任务不采用——
///   hook 吞键需先于 `CallNextHookEx` 判定，与 M3 超时纪律冲突且与
///   RegisterHotKey 语义重复）；
/// - 与 LL hook 捕获并存：hook 先于热键消费看到按键（M3 恒透传），主控侧的
///   转发过滤由 app 层 `HotkeyFilter` 承担（双保险，PRD 要求 3）；
/// - 生命周期：[`WinHotkeySource::stop`]（显式）或 `Drop`（兜底）都是摘共享 →
///   `PostThreadMessageW(WM_QUIT)` → join——线程内 `UnregisterHotKey` × N +
///   `DestroyWindow`（Win32 线程契约：窗口归创建线程所有）。
pub struct WinHotkeySource {
    receiver: Receiver<HotkeyId>,
    /// 泵线程 id（就绪握手回传；`PostThreadMessageW(WM_QUIT)` 定址）。
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

/// 注册线程的就绪载荷：`Ok(线程 id)` 或 `Err(原因)`。
type HotkeyReadyPayload = Result<u32, String>;

impl WinHotkeySource {
    /// 注册热键集合（线程：注册窗口类 → 建消息窗口 → 逐组合 `RegisterHotKey`
    /// → 就绪握手 → `GetMessageW` 泵）。
    ///
    /// 任一组合注册失败 → 全量回滚（已注册的先注销）后报错——半注册态会让
    /// 「部分热键可用」这种静默降级不可诊断（fail loud，调用方降级停用）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：窗口类/窗口创建失败、组合注册失败（被其他
    /// 程序占用）或主键无 VK 映射（[`crate::winkeymap::xkey_to_vk`] `None`）、
    /// 线程就绪前退出。
    pub fn register(set: &HotkeySet) -> Result<Self, InputError> {
        let (sender, receiver) = channel::<HotkeyId>();
        let (ready_tx, ready_rx) = channel::<HotkeyReadyPayload>();
        let set = set.clone(); // 所有权入线程（借用不可跨 spawn）

        let worker = std::thread::Builder::new()
            .name("xcontrol-hotkey-win".into())
            .spawn(move || run_hotkey_thread(ready_tx, sender, set))
            .map_err(|error| InputError::Platform {
                action: "hotkey-spawn",
                reason: format!("热键线程创建失败：{error}"),
            })?;

        let thread_id = ready_rx.recv().map_err(|_| {
            // 线程已带静态槽退出：摘共享防迟到回调写死通道。
            if let Ok(mut slot) = HOTKEY_SHARED.lock() {
                *slot = None;
            }
            InputError::Platform {
                action: "hotkey-start",
                reason: "热键线程在就绪前退出（panic 或提前 return）".into(),
            }
        })?;
        let thread_id = thread_id.map_err(|reason| {
            if let Ok(mut slot) = HOTKEY_SHARED.lock() {
                *slot = None;
            }
            InputError::Platform {
                action: "hotkey-register",
                reason,
            }
        })?;

        Ok(Self {
            receiver,
            thread_id,
            worker: Some(worker),
        })
    }

    /// 显式停止：摘共享 → `PostThreadMessageW(WM_QUIT)` → join（线程内随后
    /// `UnregisterHotKey` × N + `DestroyWindow`）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：热键线程 panic。
    pub fn stop(mut self) -> Result<(), InputError> {
        self.shutdown()
    }

    /// 非阻塞拉取自上次调用以来积累的命中（无事件时 `out` 不变）。
    pub fn drain(&mut self, out: &mut Vec<HotkeyId>) {
        while let Ok(id) = self.receiver.try_recv() {
            out.push(id);
        }
    }

    /// 关停公共路径（[`WinHotkeySource::stop`] 与 [`Drop`] 共用）。
    fn shutdown(&mut self) -> Result<(), InputError> {
        // 1) 先摘共享：迟到的 WM_HOTKEY 在 with_shared_hotkey 处短路丢弃。
        if let Ok(mut slot) = HOTKEY_SHARED.lock() {
            *slot = None;
        }
        // 2) 投 WM_QUIT 唤醒消息泵（线程可能已自行退出：错误静默，join 为准）。
        // SAFETY: thread_id 是就绪握手回传的合法线程标识；WM_QUIT 参数恒 (0,0)。
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        // 3) join：等待泵退出 + 全量注销 + 窗口销毁完成（都在线程内做）。
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| InputError::Platform {
                action: "hotkey-stop",
                reason: "热键线程 panic".into(),
            })?;
        }
        Ok(())
    }
}

impl Drop for WinHotkeySource {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也关停线程与热键注册（幂等；join
        // 失败在 Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

/// 热键线程主体：窗口类注册 → message-only 窗口 → 逐组合注册 → 就绪握手 →
/// `GetMessageW` 泵 → 收尾（全量注销 + 销毁窗口）。
///
/// 每步失败都经 `ready_tx.send(Err(...))` 上报后退出（宿主 register 以 Err
/// 收尾）；注册失败回滚已注册 id（函数 doc 的 fail-loud 裁定）。回调纪律见
/// [`hotkey_wndproc`]。
fn run_hotkey_thread(
    ready_tx: Sender<HotkeyReadyPayload>,
    sender: Sender<HotkeyId>,
    set: HotkeySet,
) {
    // SAFETY: GetModuleHandleW(None) 取本进程主模块句柄（Win32 惯例，线程安全）。
    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => HINSTANCE(module.0),
        Err(error) => {
            let _ = ready_tx.send(Err(format!("GetModuleHandleW 失败：{error}")));
            return;
        }
    };
    let class = WNDCLASSW {
        lpfnWndProc: Some(hotkey_wndproc),
        hInstance: hinstance,
        lpszClassName: HOTKEY_CLASS,
        ..WNDCLASSW::default()
    };
    // SAFETY: class 为栈上完整初始化的合法 WNDCLASSW；重复注册按
    // ERROR_CLASS_ALREADY_EXISTS 幂等放行（剪贴板监听器同款）。
    if unsafe { RegisterClassW(&class) } == 0 {
        // SAFETY: GetLastError 读取本线程最后错误码（紧跟失败的注册调用）。
        let last_error = unsafe { windows::Win32::Foundation::GetLastError() };
        if last_error != ERROR_CLASS_ALREADY_EXISTS {
            let _ = ready_tx.send(Err(format!(
                "RegisterClassW 失败（error={}）",
                last_error.0
            )));
            return;
        }
    }
    // message-only 窗口（剪贴板监听器同款：不可见、不参与 Z 序，仅收消息）。
    // SAFETY: 类名 w! 字面量（NUL 结尾）；父级 HWND_MESSAGE 为预定义句柄。
    let hwnd = match unsafe {
        CreateWindowExW(
            Default::default(),
            HOTKEY_CLASS,
            w!("xcontrol hotkey"),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("CreateWindowExW 失败：{error}")));
            return;
        }
    };

    // 逐组合注册（id = HotkeyId::ALL 下标）；任一失败 → 回滚已注册后报错。
    let mut registered: Vec<HotkeyId> = Vec::new();
    for id in HotkeyId::ALL {
        let Some(combo) = set.combo(id) else {
            continue; // 未配置的动作跳过（空集合合法）
        };
        let Some(vk) = crate::winkeymap::xkey_to_vk(combo.key) else {
            rollback_hotkeys(Some(hwnd), &registered);
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = ready_tx.send(Err(format!(
                "热键 {} 主键无 VK 映射（winkeymap::xkey_to_vk = None；见该模块覆盖范围）",
                id.label()
            )));
            return;
        };
        // SAFETY: hwnd 为本线程刚创建的合法窗口；id 唯一（ALL 下标）；vk/mods
        // 为表内合法值。RegisterHotKey 是进程级注册（无重入面）。
        if let Err(error) =
            unsafe { RegisterHotKey(Some(hwnd), hotkey_id_index(id), hotkey_modifiers(combo), vk) }
        {
            rollback_hotkeys(Some(hwnd), &registered);
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = ready_tx.send(Err(format!(
                "RegisterHotKey({}) 失败（组合被其他程序占用？）：{error}",
                id.label()
            )));
            return;
        }
        registered.push(id);
    }

    // 装静态共享（此后 WNDPROC 开始投递）→ 就绪握手 → 消息泵（监听器同款顺序：
    // 装槽在握手前——宿主 start 返回后按下的第一组合键必然可达）。
    if let Ok(mut slot) = HOTKEY_SHARED.lock() {
        *slot = Some(HotkeyShared { sender });
    }
    // SAFETY: GetCurrentThreadId 无参数（返回本线程 OS id）。
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready_tx.send(Ok(thread_id)).is_err() {
        // 宿主已放弃：全量清理后退出。
        rollback_hotkeys(Some(hwnd), &registered);
        // SAFETY: hwnd 为本线程创建的合法窗口（同线程销毁，Win32 契约）。
        let _ = unsafe { DestroyWindow(hwnd) };
        if let Ok(mut slot) = HOTKEY_SHARED.lock() {
            *slot = None;
        }
        return;
    }
    // 消息泵：GetMessageW 阻塞取消息；WM_HOTKEY 在 wndproc 内处理（剪贴板
    // 监听器同款标准泵序）。0 = WM_QUIT（关停）、-1 = 错误，均退出循环。
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg 为栈上合法出参；hwnd=None 接收本线程全部消息（message-only
        // 窗口的消息也进线程队列）。
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        // SAFETY: msg 为 GetMessageW 刚填充的合法消息结构；TranslateMessage/
        // DispatchMessageW 标准泵序（WM_HOTKEY 不产字符消息，Translate 无效果
        // 但保持与监听器同款泵形，减少两处消息循环的认知分叉）。
        unsafe {
            // TranslateMessage 返回值 = 是否产字符消息（热键路径恒 false，忽略）。
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }
    }
    // 泵退出：全量注销 + 销毁窗口（顺序无关紧要——同线程收尾）。
    rollback_hotkeys(Some(hwnd), &registered);
    // SAFETY: hwnd 为本线程创建的合法窗口。
    let _ = unsafe { DestroyWindow(hwnd) };
    if let Ok(mut slot) = HOTKEY_SHARED.lock() {
        *slot = None;
    }
}

/// 全量注销已注册热键（逐条尽力；失败仅影响系统残留——进程退出时系统
/// 自动回收该窗口的全部热键注册，Drop/stop 均不因单条失败中断）。
fn rollback_hotkeys(hwnd: Option<windows::Win32::Foundation::HWND>, registered: &[HotkeyId]) {
    for id in registered {
        // SAFETY: hwnd 为注册时传入的合法窗口；id 与注册一一对应。
        let _ = unsafe { UnregisterHotKey(hwnd, hotkey_id_index(*id)) };
    }
}

// ============================================================
// 测试（纯逻辑：坐标/滚轮/按钮换算与注入单元构成；Windows CI runner 跑
// `--features xcontrol-input/windows` 时执行，docs/08 §5）
// ============================================================

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT_FLAGS;
    use xcontrol_core::sink::{InputSink, InputSource};
    use xcontrol_proto::xkey::{
        KEY_A, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_RIGHT_ALT, KEY_RIGHT_CTRL, KEY_RIGHT_META,
        KEY_RIGHT_SHIFT, KEY_SPACE, KEY_UP,
    };

    use super::*;

    /// f64 累加器：整数提取 + 余量保留（docs/02 §4.2 语义；mac 同款锚点对拍）。
    #[test]
    fn accumulate_preserves_remainder() {
        let (rest, int) = accumulate(0.0, 1.5);
        assert_eq!((rest, int), (0.5, 1));
        let (rest, int) = accumulate(0.5, 0.6);
        assert_eq!(int, 1);
        assert!((rest - 0.1).abs() < 1e-6, "余量应为 0.1 ±1e-6，实测 {rest}");
        let (rest, int) = accumulate(-0.5, -0.6);
        assert!((rest + 0.1).abs() < 1e-6);
        assert_eq!(int, -1);
        let (mut rest, mut total) = (0.0_f64, 0_i64);
        for _ in 0..100 {
            let (r, i) = accumulate(rest, 0.15);
            rest = r;
            total += i as i64;
        }
        assert_eq!((rest * 1000.0).round() as i64, 0, "余量应耗尽");
        assert_eq!(total, 15, "100×0.15 = 15 格");
    }

    /// 归一化 → 0..65535 绝对坐标（越界收敛边界；docs/02 §4.2 kind 0x04）。
    #[test]
    fn abs_to_virtual_normalizes_and_clamps() {
        assert_eq!(abs_to_virtual(0.0, 0.0), (0, 0));
        assert_eq!(abs_to_virtual(1.0, 1.0), (65535, 65535));
        assert_eq!(abs_to_virtual(0.5, 0.5), (32768, 32768));
        // 越界收敛（协议侧越界值不得放大为注入错误）
        assert_eq!(abs_to_virtual(-0.5, 1.5), (0, 65535));
        // NaN 直透 clamp（f64::clamp 传播 NaN），由末位 `as i32` 饱和转换收敛到 0
        // （防御性；warp 入口已拒绝非有限值，此处只保证纯函数自身不放大 NaN）
        assert_eq!(abs_to_virtual(f32::NAN, f32::NAN), (0, 0));
    }

    /// 滚轮解码：高字 i16 ÷ 120 = 格（含小数）；捕获方向（正 = 上/右）。
    #[test]
    fn wheel_notches_decodes_high_word() {
        let one_notch = u32::from(120_u16) << 16; // WM_MOUSEWHEEL +1 格
        assert!((wheel_notches(one_notch) - 1.0).abs() < 1e-6);
        let half = u32::from(60_u16) << 16; // 高分滚轮半格
        assert!((wheel_notches(half) - 0.5).abs() < 1e-6);
        let neg = ((u16::MAX - 119) as u32) << 16; // -120（i16 补码）
        assert!((wheel_notches(neg) + 1.0).abs() < 1e-6);
        assert_eq!(wheel_notches(0), 0.0);
    }

    /// 注入滚轮编码：格 × 120，负数补码回绕（系统按有符号解释）。
    #[test]
    fn wheel_raw_encodes_notches() {
        assert_eq!(wheel_raw(1), 120);
        assert_eq!(wheel_raw(3), 360);
        assert_eq!(wheel_raw(-1), (-120_i32) as u32);
        assert_eq!(wheel_raw(0), 0);
    }

    /// XBUTTON 解码路径的对拍：XBUTTON1/2 常量值（windows 0.62 = 1/2）。
    #[test]
    fn xbutton_constants_match_wire() {
        assert_eq!(XBUTTON1, 1_u16);
        assert_eq!(XBUTTON2, 2_u16);
    }

    /// 按键注入单元构成：scancode 路径标志位（普通/扩展/抬起）逐位断言。
    #[test]
    fn key_input_flag_composition() {
        // 普通键 down：SCANCODE 单标志
        let down = key_input(KEY_A, false).expect("KEY_A 应有扫描码");
        assert_eq!(down.r#type, INPUT_KEYBOARD);
        // SAFETY: 读取刚按 ki 分支构造（union 活跃域明确）的 INPUT，仅本栈帧只读。
        let ki = unsafe { down.Anonymous.ki };
        assert_eq!(ki.wScan, 0x1E); // set-1 A（winkeymap 锚点）
        assert_eq!(ki.dwFlags, KEYEVENTF_SCANCODE);
        assert_eq!(ki.wVk, VIRTUAL_KEY(0));
        // 扩展键：+ EXTENDEDKEY（右 Alt = E0 38）
        let ralt = key_input(KEY_RIGHT_ALT, false).expect("右 Alt 应有扫描码");
        // SAFETY: 同上（ki 活跃域）。
        let ralt_ki = unsafe { ralt.Anonymous.ki };
        assert_eq!(ralt_ki.wScan, 0x38);
        assert_eq!(ralt_ki.dwFlags, KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY);
        // 抬起：+ KEYUP
        let up = key_input(KEY_RIGHT_ALT, true).expect("抬起单元应可构建");
        // SAFETY: 同上（ki 活跃域）。
        let up_ki = unsafe { up.Anonymous.ki };
        assert_eq!(
            up_ki.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        // 未知键拒绝（winkeymap 反向缺口键）
        assert!(key_input(xcontrol_proto::xkey::KEY_NONUS_HASH, false).is_none());
    }

    /// Unicode 注入单元构成：KEYEVENTF_UNICODE + wScan 承载 UTF-16 单元。
    #[test]
    fn unicode_input_carries_utf16_units() {
        // ASCII
        let a = unicode_input(u16::from(b'a'), false);
        // SAFETY: 读取刚按 ki 分支构造的 INPUT（活跃域明确），仅本栈帧只读。
        let a_ki = unsafe { a.Anonymous.ki };
        assert_eq!(a_ki.dwFlags, KEYEVENTF_UNICODE);
        assert_eq!(a_ki.wScan, u16::from(b'a'));
        // 抬起对
        let a_up = unicode_input(u16::from(b'a'), true);
        // SAFETY: 同上（ki 活跃域）。
        let a_up_ki = unsafe { a_up.Anonymous.ki };
        assert_eq!(a_up_ki.dwFlags, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
        // 中文 BMP
        let zh = unicode_input(0x63A7, false); // '控'
        // SAFETY: 同上（ki 活跃域）。
        let zh_ki = unsafe { zh.Anonymous.ki };
        assert_eq!(zh_ki.wScan, 0x63A7);
    }

    /// Text 事件的 UTF-16 序列（代理对展开：U+1F600 = D83D DE00 两单元）。
    #[test]
    fn text_surrogate_pairs_expand() {
        let units = "a控😀".encode_utf16().collect::<Vec<u16>>();
        assert_eq!(
            units,
            vec![u16::from(b'a'), 0x63A7, 0xD83D, 0xDE00],
            "代理对应展开为两单元（KEYEVENTF_UNICODE 逐单元注入）"
        );
    }

    /// 修饰键快照位与 mods bit0-7 对应（mods() 载荷合法性；回调写入路径的宿主读）。
    #[test]
    fn mods_bits_layout_matches_protocol() {
        let mods_bits = Arc::new(AtomicU32::new(0));
        for (key, bit) in [
            (KEY_LEFT_CTRL, 0_u8),
            (KEY_RIGHT_SHIFT, 5),
            (KEY_LEFT_META, 3),
            (KEY_RIGHT_META, 7),
        ] {
            let mask = 1_u32 << u32::from(bit);
            mods_bits.fetch_or(mask, Ordering::Relaxed);
            assert_eq!(Mods::from_key(key), Some(Mods::from_key_bit(bit)));
        }
        assert_eq!(
            Mods::from_bits(mods_bits.load(Ordering::Relaxed)).bits(),
            0b1000_1011,
            "LCtrl|LMeta|RShift|RMeta 位组"
        );
    }

    /// 无害注入冒烟（PRD AC：调用面成功不崩溃；默认可跑——无需权限，UIPI 只影响
    /// 事件到达率，不影响 API 成败）：零滚轮（纯余量路径）、空文本、空 release_all。
    #[test]
    fn inject_harmless_events_are_ok() {
        let mut injector = WinInjector::new().expect("注入器构建（无需权限）");
        injector
            .inject(&InputEvent::Wheel {
                axis: Axis::Vertical,
                delta: 0.0,
            })
            .expect("零滚轮应成功（不产生整数格）");
        injector
            .inject(&InputEvent::Text {
                utf8: String::new(),
            })
            .expect("空文本应成功");
        injector.release_all().expect("空 release_all 应成功");
        // 虚拟屏几何合理（会话内必有正并集）
        let vs = virtual_screen_info().expect("虚拟屏查询");
        assert!(vs.size.0 > 0 && vs.size.1 > 0);
        let (cx, cy) = cursor_location().expect("光标查询");
        assert!(cx.is_finite() && cy.is_finite());
    }

    /// 键盘/按钮注入 + 释放记账（#[ignore]：**会产生真实输入**——Space 键与中键
    /// 点击落到当前前台应用；人工运行
    /// `cargo test -p xcontrol-input --features windows -- --ignored`）。
    /// 事件实际送达由移交清单实机项核验（UIPI/前台应用依赖运行时环境）。
    #[test]
    #[ignore = "会产生真实输入（Space 键 + 中键点击）：确认前台应用无未保存内容后运行"]
    fn inject_key_and_button_round_trip() {
        let mut injector = WinInjector::new().expect("注入器构建");
        injector
            .inject(&InputEvent::Key {
                code: KEY_SPACE,
                state: KeyState::Down,
            })
            .expect("Space down 注入调用面应成功");
        injector
            .inject(&InputEvent::Key {
                code: KEY_SPACE,
                state: KeyState::Up,
            })
            .expect("Space up 注入调用面应成功");
        // 按钮往返（真实鼠标事件：光标原地点击）
        injector
            .inject(&InputEvent::MouseButton {
                button: MouseButton::Middle,
                state: KeyState::Down,
            })
            .expect("中键 down 注入调用面应成功");
        injector
            .inject(&InputEvent::MouseButton {
                button: MouseButton::Middle,
                state: KeyState::Up,
            })
            .expect("中键 up 注入调用面应成功");
        injector.release_all().expect("释放应成功");
    }

    /// 未知 ScreenId warp 拒绝（M3 单表面语义：非 0 一律 Failed；拒绝发生在
    /// SendInput 之前，无真实输入，默认可跑）。
    #[test]
    fn warp_unknown_screen_fails() {
        let mut injector = WinInjector::new().expect("注入器构建");
        let error = injector.warp(99, 0.5, 0.5).expect_err("非 0 屏应拒绝");
        assert!(matches!(error, SinkError::Failed { action: "warp", .. }));
    }

    /// ScreenId 0（虚拟桌面）warp（#[ignore]：**会移动真实光标**到虚拟桌面中心；
    /// 人工运行方式同 [`inject_key_and_button_round_trip`]）。
    #[test]
    #[ignore = "会移动真实光标到虚拟桌面中心"]
    fn warp_virtual_desktop_center_moves_cursor() {
        let mut injector = WinInjector::new().expect("注入器构建");
        injector.warp(0, 0.5, 0.5).expect("ScreenId 0 warp 应成功");
    }

    /// 路由层事件（EnterScreen 等）到注入层为静默忽略（mac 后端同构语义）。
    #[test]
    fn routing_events_are_ignored() {
        use xcontrol_proto::input::ScreenEdge;
        let mut injector = WinInjector::new().expect("注入器构建");
        for event in [
            InputEvent::EnterScreen {
                target: 1,
                edge: ScreenEdge::Right,
                ratio: 0.5,
                mods: Mods::LMETA,
                preserve_cursor: false,
            },
            InputEvent::LeaveScreen { mods: Mods::RALT },
            InputEvent::ModifierSync { mods: Mods::LCTRL },
            InputEvent::SwitchRequest { target: 1 },
            InputEvent::SwitchAck {
                target: 1,
                ok: true,
            },
        ] {
            injector.inject(&event).expect("路由事件应静默成功");
        }
    }

    /// 捕获源启停冒烟（默认可跑：LL hook 安装无需权限；PRD AC 编译级之外的
    /// 运行面验证由移交清单实机覆盖——CI runner 上用户键鼠事件不可预期，本测试
    /// 只断言生命周期不挂起、mods 位域合法）。
    #[test]
    fn capture_start_poll_stop_smoke() {
        let mut source = match WinCaptureSource::start() {
            Ok(source) => source,
            Err(error) => {
                // 极少数系统策略可拒装 hook：如实记录，不判定为回归。
                println!("本机 LL hook 安装失败（记录）：{error}");
                return;
            }
        };
        // 短窗口无注入 → 理论无事件（injected 过滤 + runner 无人为键鼠）。
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut events = Vec::new();
        source.poll(&mut events);
        println!("100ms 窗口捕获事件数：{}", events.len());
        assert!(source.mods().bits() & !0xFF == 0, "mods 位域应在 bit0-7 内");
        source.stop().expect("捕获应干净停止（join 无 panic）");
    }

    /// 二次启停（静态共享槽位的置入/摘除幂等：stop 后 HOOK_SHARED 回到 None，
    /// 新 start 不受上一会话残留影响）。
    #[test]
    fn capture_restart_after_stop_is_clean() {
        let first = WinCaptureSource::start().ok();
        if let Some(source) = first {
            source.stop().expect("第一次停止应成功");
        }
        assert!(
            HOOK_SHARED.lock().is_ok_and(|slot| slot.is_none()),
            "关停后共享槽应已摘除"
        );
        let second = WinCaptureSource::start().ok();
        if let Some(source) = second {
            assert!(
                HOOK_SHARED.lock().is_ok_and(|slot| slot.is_some()),
                "重启动后共享槽应已置入"
            );
            source.stop().expect("第二次停止应成功");
        }
    }

    /// 修饰键捕获语义的纯逻辑对拍（回调写路径的独立复算；不依赖真实键鼠事件）。
    /// 完整注入→捕获回环在 Windows 上不可行（本模块过滤 injected 事件，模块文档），
    /// 实机两进程对拍属移交清单。
    #[test]
    fn keyboard_translation_matches_winkeymap_anchors() {
        // 合成 LL 结构体（与系统填充字段同构）复算 translate_keyboard 的查表层：
        // scan 0x1E 无扩展 → KEY_A down；flags 带 LLKHF_UP → Up。
        let make = |scan: u32, flags: u32| KBDLLHOOKSTRUCT {
            vkCode: 0,
            scanCode: scan,
            flags: KBDLLHOOKSTRUCT_FLAGS(flags),
            time: 0,
            dwExtraInfo: 0,
        };
        let down = make(0x1E, 0); // 无 LLKHF_UP/EXTENDED/INJECTED
        assert_eq!(
            winkeymap::scan_to_xkey(down.scanCode as u16, false),
            Some(KEY_A)
        );
        let up = make(0x1E, LLKHF_UP.0); // 抬起位
        assert!(up.flags.contains(LLKHF_UP));
        // 方向键：extended 位分流（0x48 扩展 = ↑，非扩展 = KP8）
        let arrow = make(0x48, LLKHF_EXTENDED.0);
        assert_eq!(
            winkeymap::scan_to_xkey(arrow.scanCode as u16, true),
            Some(KEY_UP)
        );
        assert_eq!(
            winkeymap::scan_to_xkey(0x48, false),
            Some(xcontrol_proto::xkey::KEY_KP_8)
        );
        // injected 丢弃判据
        let injected = make(0x1E, LLKHF_INJECTED.0);
        assert!(injected.flags.contains(LLKHF_INJECTED));
        // 修饰键 mods 位（Key 事件进通道前的快照位写路径复算）
        let rctrl = make(0x1D, LLKHF_EXTENDED.0);
        assert_eq!(
            winkeymap::scan_to_xkey(rctrl.scanCode as u16, true),
            Some(KEY_RIGHT_CTRL)
        );
    }

    // —— 热键注册路径纯逻辑（M4-hotkeys；窗口/注册调用面仅 Windows 原机可测，
    //    CI runner 跑 --features windows 时以下纯函数断言执行） ——

    /// 家族位 → HOT_KEY_MODIFIERS：逐家族断言 + Meta→MOD_WIN + NOREPEAT 恒附加。
    #[test]
    fn hotkey_modifiers_map_families() {
        use xcontrol_proto::xkey::{KEY_P, KEY_RIGHT};
        let mk = |families| HotkeyCombo {
            families,
            key: KEY_RIGHT,
        };
        let base = hotkey_modifiers(mk(ModFamilies::CTRL.union(ModFamilies::ALT)));
        assert!(base.contains(MOD_CONTROL));
        assert!(base.contains(MOD_ALT));
        assert!(!base.contains(MOD_SHIFT));
        assert!(!base.contains(MOD_WIN));
        assert!(base.contains(MOD_NOREPEAT), "NOREPEAT 恒附加（按住不重复）");
        // Meta → MOD_WIN（Windows 键家族语义）
        let meta = hotkey_modifiers(mk(ModFamilies::META));
        assert!(meta.contains(MOD_WIN));
        assert!(!meta.contains(MOD_CONTROL));
        // Shift
        let shift = hotkey_modifiers(mk(ModFamilies::SHIFT));
        assert!(shift.contains(MOD_SHIFT));
        // 主键不进掩码（vk 独立形参）
        let _ = KEY_P;
    }

    /// 注册 id 还原：HotkeyId::ALL 下标 ↔ hotkey_id_index 一一对应。
    #[test]
    fn hotkey_id_index_roundtrip() {
        for (index, id) in HotkeyId::ALL.iter().enumerate() {
            assert_eq!(hotkey_id_index(*id), index as i32, "{id:?}");
        }
    }

    /// 默认热键集合注册要素齐备：四动作各有主键 VK（RegisterHotKey 前置条件）。
    #[test]
    fn default_set_has_vk_for_all_main_keys() {
        let set = HotkeySet::from_config(&xcontrol_config::HotkeysConfig::default())
            .expect("默认热键必解析");
        for id in HotkeyId::ALL {
            let combo = set.combo(id).expect("默认四键齐全");
            assert!(
                crate::winkeymap::xkey_to_vk(combo.key).is_some(),
                "{id:?} 主键 {:?} 应有 VK 映射",
                combo.key
            );
        }
    }
}
