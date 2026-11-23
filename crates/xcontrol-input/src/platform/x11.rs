//! Linux X11 输入实现（docs/05 §3、ADR-0003 决策 1）：XRecord 被动捕获 + XTEST 注入 +
//! `XQueryPointer`/warp 光标查询 + `grab_pointer`/`grab_keyboard` 隐藏本机输入。
//!
//! # 依赖与 unsafe（docs/08 §2）
//!
//! `x11rb` 0.13（纯 Rust bindings；default-features off 时连 libc 都不进传递树，
//! 未开 `allow-unsafe-code` 则 crate 内部 `#![forbid(unsafe_code)]`）——本模块因此
//! **零 unsafe**（无文件级 allow，workspace 级 `unsafe_code = deny` 直接生效）。
//! API 签名逐项对 registry 源码 0.13.2 核实（2026-11-17）：
//!
//! - `RustConnection::connect(Option<&str>) -> Result<(RustConnection, usize), ConnectError>`
//!   （读 `$DISPLAY` 与 `$XAUTHORITY`；返回 (连接, 屏幕 index)）；
//! - `xtest_fake_input(type_: u8, detail: u8, time: u32, root: Window, root_x: i16,
//!   root_y: i16, deviceid: u8) -> Result<VoidCookie, ConnectionError>`（`type_` = X 事件
//!   类型 2-6；KeyPress/KeyRelease 的 `detail` = **keycode 非 keysym**，MotionNotify 的
//!   `detail` 1=相对 / 0=绝对）；
//! - `record_create_context(context, element_header, client_specs: &[ClientSpec],
//!   ranges: &[Range]) -> VoidCookie` / `record_enable_context(context) ->
//!   RecordEnableContextCookie`（**阻塞式** `Iterator<Item = Result<EnableContextReply,
//!   ReplyError>>`，`category == 5`（EndOfData）时迭代终止）/
//!   `record_disable_context` / `record_free_context`；
//! - `grab_pointer(owner_events, grab_window, event_mask, pointer_mode, keyboard_mode,
//!   confine_to: impl Into<Window>, cursor: impl Into<Cursor>, time: impl Into<Timestamp>)`
//!   （0.13 泛型形参是 `Into<T>` 而非 `Into<Option<T>>`——None 语义按 X 惯例传 0）；
//! - `warp_pointer(src_window: impl Into<Window>, dst_window: impl Into<Window>, …)`、
//!   `query_pointer(window) -> Cookie<QueryPointerReply>`、
//!   `get_keyboard_mapping(first_keycode, count) -> Cookie<GetKeyboardMappingReply>`。
//!
//! # XSendEvent 禁令（docs/05 §3 注入行红线，逐条钉死）
//!
//! > "**必须 XTEST**；`XSendEvent` 合成事件带 `send_event=True` 标记，被大量工具包按
//! > ICCCM 忽略；注意 fake_input 参数是 **keycode 非 keysym**"
//!
//! 本模块实现路径即上述语义：注入一律走 `xtest_fake_input`（XTEST 事件由服务器按真实
//! 输入处理，无 `send_event` 标记）；**全文件不 import `send_event`**（XSendEvent 在
//! 本模块无调用面，后续改动引入它属违规）；按键注入的 `detail` 恒为 keycode
//! （[`crate::x11keymap::xkey_to_keycode`] 产出，keycode = evdev + 8）。
//!
//! # 捕获模型：XRecord 双连接（x11rb 官方 record 例同款）
//!
//! RECORD 扩展的官方通信模型（recordproto）："The typical communication model for a
//! recording client is to open two connections to the server and use one for RC control
//! and the other for reading protocol data." 本模块三连接：
//!
//! 1. **宿主控制连接**（[`X11CaptureSource::start`] 在宿主线程创建）：stop 时发
//!    `record_disable_context`（XRecord context 是服务器资源，任意连接可禁用）；
//! 2. **工作线程控制连接**：`record_create_context` + `.check()`（同步确认服务器已建
//!    context，再允许 data 连接 enable——x11rb 示例的时序要求）；
//! 3. **工作线程数据连接**：`record_enable_context` → 阻塞迭代 `EnableContextReply`
//!    → 解析 `reply.data` 内打包的 32B 核心事件 → mpsc 投递。disable 触发服务器发
//!    EndOfData（category 5）→ 迭代器自然结束 → 线程退出。
//!
//! **就绪握手时序（远端实测修正，2026-11-17）**：就绪信号在服务器 **StartOfData 首应答**
//! （category 4 = 服务器已处理 enable 的凭证）之后才发——stop() 的 disable 经宿主连接
//! 直达服务器，与工作线程 enable 分属两条连接、到达顺序无保证；若 start 后立即 stop，
//! disable 可能先于 enable 到达服务器 → "先禁后启" → context 保持启用 → 迭代器永远
//! 收不到 EndOfData → join 挂死（rootful Xwayland :2 实测复现并以此修正）。
//!
//! **XRecord 语义注记**：`device_events` 记录的是**已投递给 X 客户端的事件**
//! （recordproto 拦截点在事件派发）——键盘事件投递给焦点窗口所属客户端，因此捕获
//! 存在的前提是"存在 X 客户端焦点目标"；无 X 窗口持有焦点时（rootless Xwayland +
//! Wayland 原生窗口聚焦）XRecord 无事件可录（实测项，见 docs/05 §3.1 item 5）。
//!
//! 回调（迭代）线程纪律（platform-code.md 回调纪律 1）：线程内只做"事件语义化
//! （keycode→[`XKey`] 查静态表）→ mods 位 → `std::sync::mpsc` 投递"，零网络、零 tokio、
//! 零大对象分配；唯一的锁在宿主侧（grab 状态），事件路径零锁。
//!
//! XTEST 合成的事件在 XRecord 视角**与物理事件无区别**（X 协议核心事件无 injected
//! 标记——与 Windows `LLKHF_INJECTED` 的显式过滤位不同）。因此本模块**不过滤自注入**：
//!
//! - 与 mac 后端（CGEventTap ListenOnly 不过滤）语义一致；Windows 的 injected 过滤
//!   是该平台特有防线（docs/05 §2.1 item 6）；
//! - **回环风险由上层角色互斥承接**：同一台机器同一时刻只运行一个角色（受控态从机
//!   的主控捕获由 ControlDir 联动暂停 + [`X11CaptureSource::hide_local_input`] 隐藏
//!   本机输入），M3 会话层保证；
//! - 同进程"注入→捕获"回环测试因此**可行**（与 Windows 相反）——是本模块的核心验收
//!   手段，见模块尾部 `capture_inject_loopback_smoke` 测试与 docs/05 §3.1。
//!
//! # grab 隐藏本机输入（docs/05 §3 捕获行，ControlDir 联动钩子）
//!
//! 受控切换期主控不得让本机物理输入同时作用于本地应用（防双输入）：受控期对 root 窗口
//! `grab_pointer`（`owner_events=false` + `event_mask=0` → 指针事件被吞）+
//! `grab_keyboard`（键盘事件送达抓取连接的内部队列——XGrabKeyboard 无 event_mask
//! 形参）+ ASYNC 模式（不冻结事件流，仅重定向）。`restore_local_input` 时 ungrab 并
//! 排空抓取连接积压事件。
//!
//! **已知限制（M3 范围，移交实测）**：
//!
//! - grab 期间 XRecord 是否仍收到被吞事件：XRecord 在服务器事件处理链上的位置决定
//!   行为——若被 grab 吞掉的事件不进 record 流，主控受控期需要改为从 grab 连接读事件
//!   （Synergy/Barrier 的做法）。**M3 只交付 grab 本体 + 文档化行为，实测（docs/05
//!   §3.1 item 1）决定是否补 grab 连接事件读取路径**；
//! - 抓取连接是懒创建的独立第三连接（XGrabKeyboard 的键盘事件只能送达抓取客户端，
//!   不能吞掉——积压由 restore 时排空 + Drop 兜底释放；受控期持续打字的内存上界
//!   ≈ 32B/事件）。
//!
//! # Text 事件策略（docs/03 §4.5 / design.md 预案落地）
//!
//! XTEST 无 Unicode 直发入口（对比 Windows `KEYEVENTF_UNICODE` / mac
//! `CGEventKeyboardSetUnicodeString`）。M3 策略：**latin-1 可达子集**走
//! "每字符 keysym 经服务器 keymap 反查 keycode" 路径——keysym 0x20–0xFF 即 ISO
//! Latin-1（Unicode 码点直通），经 `get_keyboard_mapping` 建立 keysym→(keycode,
//! shift 级别) 映射后逐字符 XTEST 注入（shift 级字符补 LeftShift down/up 包裹）。
//! **限制（文档化）**：
//!
//! - 超出 latin-1 的字符（CJK、emoji 等）**无注入路径**：整段 Text 预扫描、任一字符
//!   不可达则整段拒绝（不注入半个词），报 [`SinkError::Unsupported`]，移交 M5
//!   Wayland reis/portal 路径（docs/05 §0 能力矩阵 Unicode 行：X11 ✅ keysym 合成——
//!   完整 keysym 合成（XChangeKeyboardMapping 动态键位）侵入性强，M5 评估后落地）；
//! - keymap 是构建注入器时的快照（`XKB` 布局热切换不跟踪——`refresh_layout` 重建）；
//! - `'\n'`/`'\t'` 特判为 Return/Tab 的 keycode 路径（非 latin-1 keysym）。
//!
//! # 已知限制（M3 范围，报告/移交项）
//!
//! - **ScreenId = root 窗口单表面**：Xinerama/XRandR 下 root 窗口即全部显示器的并集
//!   坐标系，归一化坐标（0.0–1.0）乘 root 尺寸即全局像素；逐屏几何（`XineramaQuery`）
//!   与 ScreenId 多屏语义留屏幕布局任务（mac `display_layout` 对应物届时补齐）。
//! - **KeyRepeat 不区分**：XRecord 的长按重复表现为连续 KeyPress（XKB autorepeat），
//!   捕获侧恒发 [`InputEvent::Key`] Down，不发 [`InputEvent::KeyRepeat`]（docs/03 §3
//!   可选项；win 后端同语义）。
//! - **修饰键快照取自 Key 事件而非事件 state 字段**：X `state` 掩码（KeyButMask）只
//!   有 any-Shift/Control/Alt/Meta 位（bit0-7 对应 X 语义 Shift/Lock/Ctrl/Mod1-5），
//!   **不区分左右手**——docs/02 §4.2 的 mods 位（bit0-7 = LCtrl/LShift/LAlt/LMeta/
//!   RCtrl/RShift/RAlt/RMeta）需要左右手区分，故快照位由修饰键 keycode（50/62 等，
//!   [`crate::x11keymap`] 全表）的 Down/Up 维护（win 后端同法）。
//! - **多屏 Xinerama 移交**：见上；XWayland 行为差异（XTEST 在 Xwayland 下与真 Xorg
//!   的偏差）属 docs/05 §3.1 实测项。
//! - **stop/Drop 的 join 无超时**：disable 经宿主控制连接送达，若控制连接先于数据连接
//!   死亡（服务器对单连接的协议错误踢除）而数据连接仍健康，事件线程收不到 EndOfData、
//!   join 将无限阻塞（XRecord context 归创建客户端资源，连接全断才释放）。概率极低
//!   （控制连接仅在 stop 时发请求），硬化为"迭代器超时/强制摘除"留待 M4 会话层统一
//!   处理（与断网重连降级同批，docs/05 §3.1 item 6）。
//!
//! # 远端实测配方与结果（本机非 Linux 时的验证路径，docs/05 §3.1）
//!
//! ```bash
//! # lsdark-eos（KDE Plasma Wayland 会话：Xorg :0 = sddm greeter（root 持 xauth，
//! # SSH 不可连）+ Xwayland :1 rootless；SSH tty 无 DISPLAY）
//! rsync -a --exclude target --exclude .git --exclude .trellis ./ \
//!   lsdark@xwork.leopon-corn.ts.net:/media/lsdark/gns/sources/com.aixhc/XControl/
//! # 默认套件（65 项全绿，Xwayland :1）：
//! ssh lsdark@… 'cd … && DISPLAY=:1 XAUTHORITY=/run/user/1000/xauth_wQtMsx \
//!   cargo test -p xcontrol-input --features x11'
//! # 回环冒烟载体（rootful Xwayland :2，xtest 焦点窗口语义完整）：
//! ssh lsdark@… 'XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
//!   setsid /usr/bin/Xwayland :2 -ac >/tmp/xwayland2.log 2>&1 &'
//! ssh lsdark@… 'cd … && DISPLAY=:2 cargo test -p xcontrol-input --features x11 \
//!   -- --ignored --nocapture'   # 回环/注入/warp 四项全绿
//! # 诊断探针（三路注入对照）：cargo test … capture_loopback_probe -- --ignored --nocapture
//! ```
//!
//! **实测结论（2026-11-17，lsdark-eos）**：
//!
//! - **回环 PASS（rootful Xwayland :2）**：XTEST 注入 F15 → XRecord 收到 Key{F15,Down/Up}
//!   全链路（context 创建/enable/迭代解析/keycode→XKey）成立；
//! - **rootless Xwayland :1 行为差异**：XTEST 注入调用面成功（warp/按钮/key 全部返回
//!   成功），但注入事件经 compositor（KWin）路由到当前聚焦的 Wayland 原生表面——X
//!   客户端与 XRecord 均收不到（xdotool 交叉验证同行为，排除本模块注入路径问题）。
//!   焦点在 Xwayland 窗口（compositor 激活）时行为预期与真 Xorg 一致——待桌面端实测
//!   （docs/05 §3.1 item 5）；
//! - `get_keyboard_mapping` keysym 表在 Xwayland 构建成功（Text latin-1 路径可用）；
//! - 环境限制：Xorg :0 的 xauth 文件归 sddm（root 0600），SSH 会话不可连——真 Xorg
//!   会话的对拍需桌面侧操作（移交项）。
//!
//! **M4-hotkeys 补测（2026-11-18，lsdark-eos，docs/05 §3.1 item 7）**：
//!
//! - **grab 回环 PASS（rootful Xwayland :2）**：`hotkey_grab_loopback_smoke`——注册
//!   默认四键（每组合 8 锁定变体，root 窗口被动 grab）→ XTEST 合成 Ctrl+Alt+Right
//!   → grab 触发 → `HotkeyId::Next` 到达（连接/停机窗/atom/变体注册/状态剥离查表
//!   全链路成立）；`hotkey_register_stop_roundtrip`（注册/注销往返 + 二次注册，停机
//!   双端 sync 纪律回归防线）随默认套件通过；默认 71 项（:1/:2 双跑）+ ignored 5 项
//!   全绿；远端原生 clippy `-D warnings` 绿；测后 `pkill -f "Xwayland :2"` 清理；
//! - **按住不重复（check 实测教训，2026-11-18）**：XKB autorepeat 对 grab 按住的
//!   主键以**同键同时间戳的 Release+Press 对**交付重复（:2 按住 1.5s 实测 21 对、
//!   周期 ~40ms——修复前按住热键会连环路由）。事件循环以 held 位集 + 同戳对判定
//!   去重（KeyRelease 挂起，下一事件为同键同戳 KeyPress = 合成对整体吞；真重按
//!   必有新戳）——与 win `MOD_NOREPEAT` / mac `HotkeyFilter` 吞尾同语义；回归防线
//!   `hotkey_grab_hold_no_repeat`（按住 1.5s 断言恰好一次命中）；
//! - **rootless Xwayland :1 限制（与上方注入不可见同源，移交 M5）**：compositor 先行
//!   处理按键，X 客户端 passive grab 对 Wayland 原生焦点预期不触发——grab 载体 =
//!   X 客户端焦点（真 Xorg / rootful Xwayland / 聚焦 Xwayland 窗口）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::cookie::VoidCookie;
use x11rb::errors::ConnectionError;
use x11rb::protocol::Event;
use x11rb::protocol::record::{self, ConnectionExt as RecordExt};
use x11rb::protocol::xproto::{
    self, Atom, ButtonPressEvent, ClientMessageEvent, ConnectionExt as XprotoExt, EventMask,
    GrabMode, GrabStatus, KeyPressEvent, ModMask, MotionNotifyEvent, Window,
};
use x11rb::protocol::xtest::{self, ConnectionExt as XtestExt};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperExt;
use x11rb::x11_utils::TryParse;
use xcontrol_core::hotkey::{HotkeyId, HotkeySet, ModFamilies};
use xcontrol_core::sink::{InputSink, SinkError};
use xcontrol_proto::input::{Axis, InputEvent, KeyState, Mods, MouseButton, ScreenId};
use xcontrol_proto::xkey::{self, XKey};

use crate::error::InputError;
use crate::x11keymap;

// ============================================================
// X 协议常量（docs/02 §4.2 语义 ↔ X 事件类型的映射锚点）
// ============================================================

/// X 核心事件类型：KeyPress（`xproto::KEY_PRESS_EVENT`，fake_input `type_` 同值）。
const X_EVENT_KEY_PRESS: u8 = xproto::KEY_PRESS_EVENT;
/// X 核心事件类型：KeyRelease。
const X_EVENT_KEY_RELEASE: u8 = xproto::KEY_RELEASE_EVENT;
/// X 核心事件类型：ButtonPress。
const X_EVENT_BUTTON_PRESS: u8 = xproto::BUTTON_PRESS_EVENT;
/// X 核心事件类型：ButtonRelease。
const X_EVENT_BUTTON_RELEASE: u8 = xproto::BUTTON_RELEASE_EVENT;
/// X 核心事件类型：MotionNotify。
const X_EVENT_MOTION_NOTIFY: u8 = xproto::MOTION_NOTIFY_EVENT;

/// XTEST 相对移动标记（FakeInput MotionNotify `detail` = 1：root_x/root_y 为增量）。
const XTEST_MOTION_RELATIVE: u8 = 1;

/// X 指针按钮：滚轮上（X 惯例 4-7 为滚轮/倾斜，按下+释放一对 = 一格）。
const X_BUTTON_WHEEL_UP: u8 = 4;
/// X 指针按钮：滚轮下。
const X_BUTTON_WHEEL_DOWN: u8 = 5;
/// X 指针按钮：滚轮左（倾斜）。
const X_BUTTON_WHEEL_LEFT: u8 = 6;
/// X 指针按钮：滚轮右（倾斜）。
const X_BUTTON_WHEEL_RIGHT: u8 = 7;
/// X 指针按钮：后退侧键（X 惯例 8/9；与 win XBUTTON1/2、mac button 3/4 语义对位）。
const X_BUTTON_BACK: u8 = 8;
/// X 指针按钮：前进侧键。
const X_BUTTON_FORWARD: u8 = 9;

/// XRecord reply category：来自服务器的打包事件数据（recordproto FromServer）。
const RECORD_FROM_SERVER: u8 = 0;

/// [`MouseButton`] → X 指针按钮号（注入路径；docs/02 §4.2 button 表 0-4 ↔ X 惯例）。
fn x_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => X_BUTTON_BACK,
        MouseButton::Forward => X_BUTTON_FORWARD,
    }
}

/// X 指针按钮号 → [`MouseButton`]（捕获路径；滚轮/倾斜按钮返回 `None`——滚轮经
/// [`Wheel`] 语义转发，release 侧 4-7 忽略）。
///
/// [`Wheel`]: xcontrol_proto::input::InputEvent::Wheel
#[must_use]
fn button_from_x(detail: u8) -> Option<MouseButton> {
    match detail {
        1 => Some(MouseButton::Left),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Right),
        8 => Some(MouseButton::Back),
        9 => Some(MouseButton::Forward),
        _ => None,
    }
}

/// X 滚轮按钮号 → (轴, 格数)（捕获路径；ButtonPress 4-7 各一格，符号 = X 惯例：
/// 4 上/5 下/6 左/7 右，与 win 原生语义"正值=上/右"对齐——跨平台符号归一属映射
/// 引擎职责，本模块保持原生方向并在捕获/注入两侧对称）。
#[must_use]
fn wheel_from_x(detail: u8) -> Option<(Axis, f32)> {
    match detail {
        X_BUTTON_WHEEL_UP => Some((Axis::Vertical, 1.0)),
        X_BUTTON_WHEEL_DOWN => Some((Axis::Vertical, -1.0)),
        X_BUTTON_WHEEL_LEFT => Some((Axis::Horizontal, -1.0)),
        X_BUTTON_WHEEL_RIGHT => Some((Axis::Horizontal, 1.0)),
        _ => None,
    }
}

/// 注入侧滚轮格数 → X 按钮号（正格 = 上/右；与 [`wheel_from_x`] 逆对称）。
#[must_use]
const fn x_wheel_button(axis: Axis, notches: i32) -> Option<u8> {
    match axis {
        Axis::Vertical if notches > 0 => Some(X_BUTTON_WHEEL_UP),
        Axis::Vertical => Some(X_BUTTON_WHEEL_DOWN),
        Axis::Horizontal if notches > 0 => Some(X_BUTTON_WHEEL_RIGHT),
        Axis::Horizontal => Some(X_BUTTON_WHEEL_LEFT),
    }
}

// ============================================================
// 注入辅助纯函数（可跨平台测试；与 win 后端 accumulate 同构）
// ============================================================

/// 把 [`f64`] 累加器与一条增量合并，产出整数注入量与保留的小数余量
/// （docs/02 §4.2：取整不丢余量，净移动量守恒；mac/win 后端同款纯函数）。
fn accumulate(acc: f64, delta: f32) -> (f64, i32) {
    let total = acc + f64::from(delta);
    let integral = total.trunc();
    // float→int `as` 转换自 Rust 1.45 起饱和；i32 溢出仅出现在病态输入，饱和即上限。
    (total - integral, integral as i32)
}

/// 归一化坐标（0.0–1.0）→ root 窗口像素坐标（越界收敛边界；docs/02 §4.2 kind 0x04）。
/// 纯函数。
fn abs_to_root(nx: f32, ny: f32, width: f64, height: f64) -> (i16, i16) {
    let px = (f64::from(nx).clamp(0.0, 1.0) * width).round();
    let py = (f64::from(ny).clamp(0.0, 1.0) * height).round();
    // root 尺寸 ≤ u16 上限（X 协议 65535），round 后的 i64→i16 截断防御性收敛。
    (px as i16, py as i16)
}

/// i32 注入量 → i16（XTEST root_x/root_y 是 i16；|Δ| > 32767 的病态增量饱和收敛）。
fn clamp_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// 指针坐标 → 归一化（纯函数；X11 `cursor_normalized` 与 wayland ABS 域
/// 同型契约）：`value / max`（max = 末像素坐标），±1 单位容差带收敛到端点
/// 0.0/1.0——与 edge.rs `EDGE_TOLERANCE_POINTS` 同型（1 单位 = 1 像素），
/// 边缘驻留回报恰 0.0/1.0，主控极值判定的回报契约。
fn pointer_snap_normalized(value: f64, max: f64) -> f32 {
    if value <= 1.0 {
        0.0
    } else if value >= max - 1.0 {
        1.0
    } else {
        (value / max) as f32
    }
}

/// latin-1 可达字符 → keysym（docs/03 §4.5：keysym 0x20–0xFF = ISO Latin-1，与
/// Unicode 码点直通）。`'\n'`/`'\t'` 走 Return/Tab 的 keycode 特判（调用方处理），
/// 其余控制字符返回 `None`。纯函数。
#[must_use]
fn keysym_for_char(c: char) -> Option<u32> {
    let code = u32::from(c);
    if (0x20..=0xFF).contains(&code) {
        Some(code)
    } else {
        None
    }
}

// ============================================================
// keysym → keycode 映射（Text 事件路径；服务器 keymap 快照）
// ============================================================

/// 服务器 keymap 快照：keysym → (keycode, 是否 shift 级)（[`X11Injector`] Text 路径）。
///
/// `get_keyboard_mapping` 逐 keycode 返回 `keysyms_per_keycode` 个 keysym（XKB 层级
/// 0 = 直接、1 = Shift、2+ = AltGr 等）；M3 只消费层级 0/1（AltGr 组合属布局依赖，
/// 文档化不覆盖——latin-1 扩展区在不同布局可达性不同，实测项移交）。
#[derive(Debug, Default)]
struct KeysymMap {
    plain: HashMap<u32, u8>,
    shifted: HashMap<u32, u8>,
}

impl KeysymMap {
    /// 从连接查询并构建快照（一次全 keymap roundtrip；`min/max_keycode` 来自 setup）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：连接失败或服务器 keymap 异常（keysyms_per_keycode = 0）。
    fn build(conn: &RustConnection) -> Result<Self, InputError> {
        let setup = conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
        let reply = conn
            .get_keyboard_mapping(first, count)
            .map_err(|error| InputError::Platform {
                action: "keymap-query",
                reason: format!("get_keyboard_mapping 失败：{error}"),
            })?
            .reply()
            .map_err(|error| InputError::Platform {
                action: "keymap-query",
                reason: format!("get_keyboard_mapping reply 失败：{error}"),
            })?;
        let per = usize::from(reply.keysyms_per_keycode);
        if per == 0 {
            return Err(InputError::Platform {
                action: "keymap-query",
                reason: "keysyms_per_keycode = 0（服务器 keymap 异常）".into(),
            });
        }
        let mut map = Self::default();
        for (index, chunk) in reply.keysyms.chunks(per).enumerate() {
            // keycode = first + index（u8 域内：first + count ≤ max_keycode ≤ 255）。
            let keycode = first + index as u8;
            // 同 keysym 多 keycode 时保留首个（canonical；布局下重复位罕见）。
            if let Some(&plain) = chunk.first()
                && plain != 0
            {
                map.plain.entry(plain).or_insert(keycode);
            }
            if let Some(&shifted) = chunk.get(1)
                && shifted != 0
            {
                map.shifted.entry(shifted).or_insert(keycode);
            }
        }
        Ok(map)
    }

    /// keysym → (keycode, 是否需要 Shift 包裹)（层级 0 优先于层级 1）。
    fn find(&self, keysym: u32) -> Option<(u8, bool)> {
        if let Some(&keycode) = self.plain.get(&keysym) {
            return Some((keycode, false));
        }
        self.shifted.get(&keysym).map(|&keycode| (keycode, true))
    }
}

// ============================================================
// 注入器（InputSink 实现）
// ============================================================

/// root 窗口几何快照（Xinerama/XRandR 并集坐标系，[`X11Injector::warp`] 归一化基准）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenGeometry {
    /// root 窗口宽（像素；多屏 = 全部显示器并集宽）。
    pub width: u16,
    /// root 窗口高（像素）。
    pub height: u16,
}

/// 查询 root 窗口几何（setup 的首屏 root；docs/05 §3"光标"行的查询面）。
///
/// # Errors
/// [`InputError::Platform`]：连接失败 / 屏幕列表为空。
pub fn display_geometry(conn: &RustConnection) -> Result<ScreenGeometry, InputError> {
    let screen = setup_screen(conn)?;
    Ok(ScreenGeometry {
        width: screen.width_in_pixels,
        height: screen.height_in_pixels,
    })
}

/// 当前光标位置（root 坐标系全局像素；docs/05 §3：`XQueryPointer` 无需权限）。
///
/// # Errors
/// [`InputError::Platform`]：连接失败 / `query_pointer` 失败（会话断开等）。
pub fn cursor_location(conn: &RustConnection) -> Result<(f64, f64), InputError> {
    let screen = setup_screen(conn)?;
    let reply = conn
        .query_pointer(screen.root)
        .map_err(|error| InputError::Platform {
            action: "cursor-query",
            reason: format!("query_pointer 失败：{error}"),
        })?
        .reply()
        .map_err(|error| InputError::Platform {
            action: "cursor-query",
            reason: format!("query_pointer reply 失败：{error}"),
        })?;
    Ok((f64::from(reply.root_x), f64::from(reply.root_y)))
}

/// setup 的首屏（ScreenId 0 的语义锚点；roots 空 = 防御性错误）。
fn setup_screen(conn: &RustConnection) -> Result<&xproto::Screen, InputError> {
    conn.setup().roots.first().ok_or(InputError::Platform {
        action: "screen-query",
        reason: "setup.roots 为空（X 连接异常）".into(),
    })
}

/// 建立 X 连接（[`RustConnection::connect`] 包装：$DISPLAY/$XAUTHORITY 由 x11rb 解析）。
fn connect() -> Result<RustConnection, InputError> {
    let (conn, _) = RustConnection::connect(None).map_err(|error| InputError::Platform {
        action: "connect",
        reason: format!("X11 连接失败（检查 $DISPLAY 与 $XAUTHORITY，docs/05 §6）：{error}"),
    })?;
    Ok(conn)
}

/// 校验扩展存在（X11_EXTENSION_NAME 查询；缺失 = 服务器不支持，fail loud）。
fn require_extension(conn: &RustConnection, name: &'static str) -> Result<(), InputError> {
    if conn
        .extension_information(name)
        .map_err(|error| InputError::Platform {
            action: "extension-query",
            reason: format!("{name} 扩展查询失败：{error}"),
        })?
        .is_none()
    {
        return Err(InputError::Platform {
            action: "extension-query",
            reason: format!("X 服务器不支持 {name} 扩展（Xwayland/Xorg 均内置，环境异常）"),
        });
    }
    Ok(())
}

/// void 请求统一收尾：发送失败（`ConnectionError`）与服务器拒绝（`check()` 的
/// `ReplyError`）都收敛到 [`SinkError::Failed`]（x11rb 0.13 无
/// `From<ReplyError> for ConnectionError`，两条路径逐分支处理）。
fn check_void(
    result: Result<VoidCookie<'_, RustConnection>, ConnectionError>,
    action: &'static str,
    api: &str,
) -> Result<(), SinkError> {
    match result {
        Ok(cookie) => cookie.check().map_err(|error| SinkError::Failed {
            action,
            reason: format!("{api} 失败：{error}"),
        }),
        Err(error) => Err(SinkError::Failed {
            action,
            reason: format!("{api} 失败：{error}"),
        }),
    }
}

/// X11 注入器（[`InputSink`] 的 X11 实现，docs/05 §3：XTEST `fake_input`）。
///
/// 状态：跟踪已注入按键/按钮集合（[`InputSink::release_all`] 防卡键收尾）与
/// 移动/滚轮 f64 累加器（docs/02 §4.2 取整不丢余量）。非阻塞快速返回（trait 契约）；
/// 单次注入失败返回 [`SinkError::Failed`]，调用方（core route 层）计数并继续，
/// 不得中断事件流（error-handling.md §4）。
pub struct X11Injector {
    conn: RustConnection,
    /// 首屏 root 窗口（fake_input 的 root 形参 / warp 目标坐标系）。
    root: Window,
    /// root 几何（归一化→像素换算；`refresh_layout` 随显示器热插拔刷新）。
    geometry: ScreenGeometry,
    /// 服务器 keymap 快照（Text 事件 keysym 路径；布局热切换不跟踪——`refresh_layout` 重建）。
    keysym_map: KeysymMap,
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

impl X11Injector {
    /// 构建注入器（建立连接 + 校验 XTEST 扩展 + 查询 root 几何 + keymap 快照）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：$DISPLAY 不可用 / XTEST 扩展缺失 / keymap 查询失败。
    pub fn new() -> Result<Self, InputError> {
        let conn = connect()?;
        require_extension(&conn, xtest::X11_EXTENSION_NAME)?;
        let screen = setup_screen(&conn)?;
        let root = screen.root;
        let geometry = display_geometry(&conn)?;
        let keysym_map = KeysymMap::build(&conn)?;
        Ok(Self {
            conn,
            root,
            geometry,
            keysym_map,
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
            move_x: 0.0,
            move_y: 0.0,
            wheel_v: 0.0,
            wheel_h: 0.0,
        })
    }

    /// 重建 keymap 快照与 root 几何（显示器热插拔 / XKB 布局切换后调用）。
    ///
    /// # Errors
    /// 同 [`X11Injector::new`]。
    pub fn refresh_layout(&mut self) -> Result<(), InputError> {
        self.geometry = display_geometry(&self.conn)?;
        self.keysym_map = KeysymMap::build(&self.conn)?;
        Ok(())
    }

    /// 提交一条 XTEST 请求（`check()` = 一次 GetInputFocus 回程同步：等待服务器处理
    /// 并把错误立即上抛——注入失败的语义即 [`SinkError::Failed`]；M5 若测出热路径
    /// 回程延迟超标可改 unchecked 批量 + 周期 sync，当前同步语义优先正确性）。
    fn fake_input(
        &self,
        type_: u8,
        detail: u8,
        root_x: i16,
        root_y: i16,
        action: &'static str,
    ) -> Result<(), SinkError> {
        // deviceid 恒 0（XTEST 虚拟设备；Xorg/Xwayland 忽略该形参的其它取值）。
        // time 恒 CURRENT_TIME(0)（服务器取当前时间）；root 恒首屏 root。
        let request = self.conn.xtest_fake_input(
            type_,
            detail,
            x11rb::CURRENT_TIME,
            self.root,
            root_x,
            root_y,
            0,
        );
        check_void(request, action, "xtest_fake_input")
    }

    /// 注入一条按键事件并更新跟踪集合（先记账后注入，与 mac/win 后端一致）。
    fn inject_key(&mut self, code: XKey, state: KeyState) -> Result<(), SinkError> {
        let Some(keycode) = x11keymap::xkey_to_keycode(code) else {
            return Err(SinkError::Failed {
                action: "inject",
                reason: format!(
                    "XKey 0x{:04X} 无 X11 keycode 对应（x11keymap 反查缺口）",
                    code.raw()
                ),
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
        let type_ = match state {
            KeyState::Down => X_EVENT_KEY_PRESS,
            KeyState::Up => X_EVENT_KEY_RELEASE,
        };
        self.fake_input(type_, keycode, 0, 0, "inject")
    }

    /// 注入滚轮事件（整数格注入 + 小数余量保留；每格 = 一次按钮按下+释放对）。
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
        let Some(button) = x_wheel_button(axis, notches) else {
            return Err(SinkError::Failed {
                action: "inject",
                reason: format!("滚轮格数 {notches} 无对应 X 按钮（axis {axis:?}）"),
            });
        };
        for _ in 0..notches.abs() {
            self.fake_input(X_EVENT_BUTTON_PRESS, button, 0, 0, "inject")?;
            self.fake_input(X_EVENT_BUTTON_RELEASE, button, 0, 0, "inject")?;
        }
        Ok(())
    }

    /// 注入 Text 事件（latin-1 keysym 路径；模块文档"Text 事件策略"）。
    ///
    /// 整段预扫描：任一字符不可达（非 latin-1、keymap 缺位、控制字符）→ 整段拒绝
    /// （不注入半个词）。`'\n'`/`'\t'` 特判为 Return/Tab keycode。
    fn inject_text(&mut self, utf8: &str) -> Result<(), SinkError> {
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
        // 预扫描：全部字符先解析成 (keycode, shift 级别) 序列，任一失败整段拒绝。
        let shift_keycode = x11keymap::xkey_to_keycode(xkey::KEY_LEFT_SHIFT);
        let mut plan: Vec<(u8, bool)> = Vec::with_capacity(utf8.chars().count());
        for c in utf8.chars() {
            let resolved = match c {
                // 控制字符特判（非 latin-1 keysym，走 keycode 直映）。
                '\n' => x11keymap::xkey_to_keycode(xkey::KEY_ENTER).map(|kc| (kc, false)),
                '\t' => x11keymap::xkey_to_keycode(xkey::KEY_TAB).map(|kc| (kc, false)),
                '\r' => Some((0, false)), // CR 由发送侧 LF/CRLF 归一处理（docs/04 §2）；此处跳过
                _ => keysym_for_char(c).and_then(|keysym| self.keysym_map.find(keysym)),
            };
            match resolved {
                Some(pair) if pair.0 != 0 => plan.push(pair),
                Some(_) => continue, // CR：跳过
                None => {
                    return Err(SinkError::Unsupported(
                        "Text 含 X11 latin-1 路径不可达字符（CJK/emoji/死键等）：XTEST 无 \
                         Unicode 直发，M5 经 reis/portal 路径注入（docs/05 §0 CAP_UNICODE）",
                    ));
                }
            }
        }
        // 逐字符注入（同一连接的请求流天然保序 = 因果顺序）。
        for (keycode, needs_shift) in plan {
            if needs_shift && let Some(shift) = shift_keycode {
                self.fake_input(X_EVENT_KEY_PRESS, shift, 0, 0, "inject")?;
            }
            self.fake_input(X_EVENT_KEY_PRESS, keycode, 0, 0, "inject")?;
            self.fake_input(X_EVENT_KEY_RELEASE, keycode, 0, 0, "inject")?;
            if needs_shift && let Some(shift) = shift_keycode {
                self.fake_input(X_EVENT_KEY_RELEASE, shift, 0, 0, "inject")?;
            }
        }
        Ok(())
    }
}

impl InputSink for X11Injector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        match event {
            InputEvent::Key { code, state } => self.inject_key(*code, *state),
            InputEvent::KeyRepeat { code } => {
                // 长按重复：仅当键仍在本机按下集合时补发 keyDown（docs/03 §3：从机可按
                // 平台能力忽略；XTEST 合成 down 不自动重复，与 win 后端同构）。
                if !self.pressed_keys.contains(code) {
                    return Ok(());
                }
                let Some(keycode) = x11keymap::xkey_to_keycode(*code) else {
                    return Err(SinkError::Failed {
                        action: "inject",
                        reason: format!("XKey 0x{:04X} 无 X11 keycode 对应", code.raw()),
                    });
                };
                self.fake_input(X_EVENT_KEY_PRESS, keycode, 0, 0, "inject")
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
                // detail=1（相对）：root_x/root_y 承载 i16 增量。
                self.fake_input(
                    X_EVENT_MOTION_NOTIFY,
                    XTEST_MOTION_RELATIVE,
                    clamp_i16(int_x),
                    clamp_i16(int_y),
                    "inject",
                )
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
                let type_ = match state {
                    KeyState::Down => X_EVENT_BUTTON_PRESS,
                    KeyState::Up => X_EVENT_BUTTON_RELEASE,
                };
                self.fake_input(type_, x_button(*button), 0, 0, "inject")
            }
            InputEvent::Wheel { axis, delta } => self.inject_wheel(*axis, *delta),
            InputEvent::Text { utf8 } => self.inject_text(utf8),
            // —— 路由/会话层事件（docs/02 §4.2 kind 0x10–0x21）：非设备注入，
            //    由 core 会话对齐机制消费；到达本层属冗余投递，静默忽略不算错误
            //    （error-handling.md §4；mac/win 后端同构）。——
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
        // M3：ScreenId 0 = root 窗口单表面（模块文档已知限制；逐屏几何属屏幕布局任务）。
        if screen != 0 {
            return Err(SinkError::Failed {
                action: "warp",
                reason: format!(
                    "未知 ScreenId {screen}（X11 M3 仅 root 单表面 = 0，{}×{}）",
                    self.geometry.width, self.geometry.height
                ),
            });
        }
        let (dst_x, dst_y) = abs_to_root(
            x,
            y,
            f64::from(self.geometry.width),
            f64::from(self.geometry.height),
        );
        // 绝对定位走 warp_pointer（design.md：src=0（无源矩形）、dst=root、坐标即 root
        // 像素——Xinerama 并集坐标系，模块文档）。
        let request = self
            .conn
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, dst_x, dst_y);
        check_void(request, "warp", "warp_pointer")
    }

    /// 释放全部跟踪中的按键/按钮（切出/暂停/断开防卡键，docs/03 §3）。
    ///
    /// 按按下逆序释放（后按的先起）；移动/滚轮累加器一并清零（防跨会话余量漂移）。
    /// 单请求失败返回 [`SinkError::Failed`] 但仍清空跟踪集合（下次 release_all 幂等）。
    fn release_all(&mut self) -> Result<(), SinkError> {
        let mut first_error: Option<SinkError> = None;
        for code in self.pressed_keys.iter().rev().copied().collect::<Vec<_>>() {
            if let Some(keycode) = x11keymap::xkey_to_keycode(code)
                && let Err(error) =
                    self.fake_input(X_EVENT_KEY_RELEASE, keycode, 0, 0, "release_all")
            {
                first_error.get_or_insert(error);
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
            if let Err(error) = self.fake_input(
                X_EVENT_BUTTON_RELEASE,
                x_button(button),
                0,
                0,
                "release_all",
            ) {
                first_error.get_or_insert(error);
            }
        }
        self.pressed_buttons.clear();
        self.move_x = 0.0;
        self.move_y = 0.0;
        self.wheel_v = 0.0;
        self.wheel_h = 0.0;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        // 双向边缘回切的被控采样面（11-22 修复轮；docs/03 §5）：XQueryPointer
        // 真实光标 → root 几何归一化（复用既有 [`cursor_location`] 查询语义，
        // M3 的 query_pointer 光标面；root 尺寸 ≤ u16，坐标域 [0, width-1]）。
        // 端点容差带与 edge.rs `EDGE_TOLERANCE_POINTS` 同型（1 X 坐标单位
        // = 1 像素）——边缘驻留回报恰 0.0/1.0，主控极值判定的回报契约。
        let (px, py) = cursor_location(&self.conn).ok()?;
        let max_x = f64::from(self.geometry.width.saturating_sub(1));
        let max_y = f64::from(self.geometry.height.saturating_sub(1));
        if max_x <= 0.0 || max_y <= 0.0 {
            return None;
        }
        let x = pointer_snap_normalized(px, max_x);
        let y = pointer_snap_normalized(py, max_y);
        // 探针（验证面）：原始坐标随行——回切"差一点不到缘"的实测分辨入口。
        tracing::debug!(target: "input::x11",
            px, py, max_x, max_y, nx = x, ny = y, "受控光标采样（XQueryPointer）");
        Some((x, y))
    }
}

// ============================================================
// 捕获源（InputSource 实现）
// ============================================================

/// XRecord 工作线程的就绪握手载荷：`Ok(context id)`（stop 定址用）或 `Err(原因)`。
type ReadyPayload = Result<u32, String>;

/// XRecord 事件迭代线程共享的鼠标基点（WM_MOUSEMOVE 同款做差法：XRecord 的
/// MotionNotify 携带绝对坐标，与上一事件坐标做差得到相对位移）。
struct MouseBaseline {
    x: AtomicI32,
    y: AtomicI32,
}

/// X11 捕获源（[`crate::InputSource`] 的 X11 实现，docs/05 §3：XRecord 被动监听 +
/// grab 隐藏本机输入）。
///
/// - 三连接模型（模块文档）：宿主控制连接（stop 定位）+ 工作线程控制/数据连接；
/// - 事件线程只做"keycode→[`XKey`] / 按钮号→协议事件 → mpsc 投递"（回调纪律）；
///   [`crate::InputSource::poll`] 非阻塞拉取；事件 FIFO 保序（单 channel 天然满足）；
/// - **不过滤自注入**（模块文档防回环语义——与 Windows 有意不同、与 mac 一致）；
/// - [`hide_local_input`](X11CaptureSource::hide_local_input) /
///   [`restore_local_input`](X11CaptureSource::restore_local_input)：受控期 grab 联动
///   钩子（ControlDir 调用点，M3 交付接口 + 行为文档）。
pub struct X11CaptureSource {
    receiver: Receiver<InputEvent>,
    /// 修饰键快照位（bit0-7 = docs/02 §4.2 mods；事件线程写、宿主线程读，Relaxed 足够）。
    mods_bits: Arc<AtomicU32>,
    /// 宿主控制连接（stop 时 `record_disable_context` 定位 + `record_free_context`）。
    ctrl_conn: Option<RustConnection>,
    /// XRecord context id（就绪握手回传；工作线程退出后由宿主 free）。
    context: Option<u32>,
    /// 抓取状态（懒创建；`Mutex` = hide/restore 可从宿主任意线程调用）。
    grab: Mutex<Option<GrabState>>,
    worker: Option<JoinHandle<()>>,
}

/// grab 隐藏本机输入的连接状态（模块文档 grab 一节）。
struct GrabState {
    conn: RustConnection,
    root: Window,
    grabbed: bool,
}

impl X11CaptureSource {
    /// 启动捕获（宿主控制连接 + 工作线程三步握手：建双连接 → create_context+check →
    /// enable_context 阻塞迭代）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：$DISPLAY 不可用 / RECORD 扩展缺失 / context 创建被拒 /
    /// 就绪线程意外退出。
    pub fn start() -> Result<Self, InputError> {
        let (sender, receiver) = channel::<InputEvent>();
        let mods_bits = Arc::new(AtomicU32::new(0));
        // 宿主控制连接：stop 路径的 disable/free 定位（同时用于鼠标基点初始化）。
        let ctrl_conn = connect()?;
        require_extension(&ctrl_conn, record::X11_EXTENSION_NAME)?;
        // 查询失败回退 (0,0)：首个移动事件可能偏大（win 后端同防御）。
        let (baseline_x, baseline_y) =
            cursor_location(&ctrl_conn).map_or((0, 0), |(x, y)| (x as i32, y as i32));
        let baseline = Arc::new(MouseBaseline {
            x: AtomicI32::new(baseline_x),
            y: AtomicI32::new(baseline_y),
        });

        let (ready_tx, ready_rx) = channel::<ReadyPayload>();
        let mods_for_worker = Arc::clone(&mods_bits);
        let baseline_for_worker = Arc::clone(&baseline);
        let worker = std::thread::Builder::new()
            .name("xcontrol-capture-x11".into())
            .spawn(move || {
                run_record_thread(ready_tx, sender, mods_for_worker, baseline_for_worker);
            })
            .map_err(|error| InputError::Platform {
                action: "capture-spawn",
                reason: format!("捕获线程创建失败：{error}"),
            })?;

        let context = ready_rx.recv().map_err(|_| {
            // 线程已带自身连接退出（panic 或提前 return）；join 收尸交由 Drop。
            InputError::Platform {
                action: "capture-start",
                reason: "XRecord 线程在就绪前退出（panic 或提前 return）".into(),
            }
        })?;
        let context = context.map_err(|reason| InputError::Platform {
            action: "record-context",
            reason,
        })?;

        Ok(Self {
            receiver,
            mods_bits,
            ctrl_conn: Some(ctrl_conn),
            context: Some(context),
            grab: Mutex::new(None),
            worker: Some(worker),
        })
    }

    /// 受控期隐藏本机输入（docs/05 §3 捕获行"受控切换期用 grab_pointer/grab_keyboard
    /// 隐藏本机输入"——ControlDir 联动钩子，会话层切入受控态时调用）。
    ///
    /// 幂等（已抓取时直接成功）；抓取连接懒创建、restore 后保留复用（模块文档 grab 一节）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：连接失败 / grab 被拒（`AlreadyGrabbed` 等——他客户端
    /// 持有抓取；重试语义由调用方决定）。
    pub fn hide_local_input(&self) -> Result<(), InputError> {
        let mut slot = self.grab.lock().map_err(|_| InputError::Platform {
            action: "grab",
            reason: "抓取状态锁中毒".into(),
        })?;
        if slot.as_ref().is_some_and(|state| state.grabbed) {
            return Ok(()); // 幂等
        }
        let (conn, root) = match slot.take() {
            // 复用上次 restore 后保留的连接（免重连）。
            Some(state) => (state.conn, state.root),
            None => {
                let conn = connect()?;
                let root = setup_screen(&conn)?.root;
                (conn, root)
            }
        };
        let state = grab_on(conn, root)?;
        *slot = Some(state);
        Ok(())
    }

    /// 恢复本机输入（会话层切出受控态时调用；ungrab + 排空抓取连接积压事件）。
    ///
    /// 幂等（未抓取时直接成功）；连接保留复用（下次 hide 免重连）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：ungrab 失败（连接死亡等）。
    pub fn restore_local_input(&self) -> Result<(), InputError> {
        let mut slot = self.grab.lock().map_err(|_| InputError::Platform {
            action: "ungrab",
            reason: "抓取状态锁中毒".into(),
        })?;
        if let Some(state) = slot.as_mut()
            && state.grabbed
        {
            // ungrab：time 恒 CURRENT_TIME。XGrabKeyboard 的积压键盘事件随后排空。
            state
                .conn
                .ungrab_pointer(x11rb::CURRENT_TIME)
                .map_err(|error| InputError::Platform {
                    action: "ungrab",
                    reason: format!("ungrab_pointer 失败：{error}"),
                })?;
            state
                .conn
                .ungrab_keyboard(x11rb::CURRENT_TIME)
                .map_err(|error| InputError::Platform {
                    action: "ungrab",
                    reason: format!("ungrab_keyboard 失败：{error}"),
                })?;
            state.grabbed = false;
            drain_events(&state.conn);
        }
        Ok(())
    }

    /// 停止捕获：宿主连接 `record_disable_context` → 服务器发 EndOfData → 事件迭代
    /// 结束 → 工作线程退出 → join → `record_free_context`。
    ///
    /// # Errors
    /// [`InputError::Platform`]：disable 失败（连接死亡等）/ 回调线程 panic。
    pub fn stop(mut self) -> Result<(), InputError> {
        self.shutdown()
    }

    /// 关停公共路径（[`X11CaptureSource::stop`] 与 [`Drop`] 共用）。
    fn shutdown(&mut self) -> Result<(), InputError> {
        // 1) 摘抓取（若未显式 restore）。
        if let Ok(mut slot) = self.grab.lock() {
            if let Some(state) = slot.as_mut()
                && state.grabbed
            {
                let _ = state.conn.ungrab_pointer(x11rb::CURRENT_TIME);
                let _ = state.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
                state.grabbed = false;
            }
            *slot = None; // Drop 释放抓取连接（积压事件随之释放）
        }
        // 2) disable context：服务器向 data 连接发 EndOfData → 迭代器结束 → 线程退出。
        let mut join_error: Option<InputError> = None;
        if let (Some(conn), Some(context)) = (self.ctrl_conn.take(), self.context.take()) {
            // disable + check（同步确认服务器已处理，data 连接随之收到 EndOfData）；
            // x11rb 0.13 无 From<ReplyError> for ConnectionError，两步分开收错误。
            let disabled = match conn.record_disable_context(context) {
                Ok(cookie) => cookie.check(),
                Err(error) => Err(error.into()),
            };
            if let Err(error) = disabled {
                join_error.get_or_insert(InputError::Platform {
                    action: "capture-stop",
                    reason: format!("record_disable_context 失败：{error}"),
                });
            }
            // 3) join（线程退出后 free context——context 是服务器资源，宿主连接定位）。
            if let Some(worker) = self.worker.take()
                && worker.join().is_err()
            {
                join_error.get_or_insert(InputError::Platform {
                    action: "capture-stop",
                    reason: "XRecord 事件线程 panic".into(),
                });
            }
            let _ = conn.record_free_context(context); // 服务器侧清理；失败仅影响服务器残留
            let _ = conn.sync();
        }
        // worker/ctrl_conn 已 take；若 disable 前就失败（连接死亡），仍要收线程。
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        match join_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// 在给定连接上执行双 grab（pointer：event_mask=0 吞事件；keyboard：事件送达本连接）。
fn grab_on(conn: RustConnection, root: Window) -> Result<GrabState, InputError> {
    // 指针抓取：owner_events=false + event_mask=0 = 抓取期指针事件全部吞掉（模块文档）；
    // confine_to=NONE（不限制）、cursor=NONE（沿用现光标）、time=CURRENT_TIME。
    let pointer_reply = conn
        .grab_pointer(
            false,
            root,
            EventMask::default(),
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )
        .map_err(|error| InputError::Platform {
            action: "grab",
            reason: format!("grab_pointer 失败：{error}"),
        })?
        .reply()
        .map_err(|error| InputError::Platform {
            action: "grab",
            reason: format!("grab_pointer reply 失败：{error}"),
        })?;
    if pointer_reply.status != GrabStatus::SUCCESS {
        // 指针抓取未生效（无回滚对象）；键盘未抓——直接报错。
        return Err(InputError::Platform {
            action: "grab",
            reason: format!(
                "grab_pointer 被拒（status={}；他客户端持有抓取？）",
                grab_status_name(pointer_reply.status)
            ),
        });
    }
    // 键盘抓取：ASYNC 模式重定向事件到本连接（XGrabKeyboard 无 event_mask 形参——
    // 积压事件由 restore 排空 / Drop 释放，模块文档）。
    let keyboard_reply = conn
        .grab_keyboard(
            false,
            root,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .map_err(|error| InputError::Platform {
            action: "grab",
            reason: format!("grab_keyboard 失败：{error}"),
        })?
        .reply()
        .map_err(|error| InputError::Platform {
            action: "grab",
            reason: format!("grab_keyboard reply 失败：{error}"),
        })?;
    if keyboard_reply.status != GrabStatus::SUCCESS {
        // 回滚双抓（键盘失败时指针仍持有）。
        let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
        return Err(InputError::Platform {
            action: "grab",
            reason: format!(
                "grab_keyboard 被拒（status={}；他客户端持有抓取？）",
                grab_status_name(keyboard_reply.status)
            ),
        });
    }
    Ok(GrabState {
        conn,
        root,
        grabbed: true,
    })
}

/// GrabStatus → 可读名（生成的类型无 Debug/Display；逐常量匹配诊断用）。
fn grab_status_name(status: GrabStatus) -> &'static str {
    match status {
        GrabStatus::SUCCESS => "Success(0)",
        GrabStatus::ALREADY_GRABBED => "AlreadyGrabbed(1)",
        GrabStatus::INVALID_TIME => "InvalidTime(2)",
        GrabStatus::NOT_VIEWABLE => "NotViewable(3)",
        GrabStatus::FROZEN => "Frozen(4)",
        _ => "Unknown",
    }
}

/// 排空连接内积压事件（restore 路径；grab_keyboard 送达本连接的键盘事件丢弃）。
fn drain_events(conn: &RustConnection) {
    // poll_for_event 返回 None = 队列空；连接错误也终止（随后的请求自会暴露）。
    while matches!(conn.poll_for_event(), Ok(Some(_))) {}
}

impl Drop for X11CaptureSource {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也关停线程与服务器 context（幂等；
        // join 失败在 Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

impl crate::InputSource for X11CaptureSource {
    fn poll(&mut self, out: &mut Vec<InputEvent>) {
        while let Ok(event) = self.receiver.try_recv() {
            out.push(event);
        }
    }

    fn mods(&self) -> Mods {
        Mods::from_bits(self.mods_bits.load(Ordering::Relaxed))
    }
}

/// XRecord 事件线程主体（回调纪律：只做语义转换 + mpsc 投递；零 async/零锁）。
fn run_record_thread(
    ready_tx: Sender<ReadyPayload>,
    sender: Sender<InputEvent>,
    mods_bits: Arc<AtomicU32>,
    baseline: Arc<MouseBaseline>,
) {
    // 工作线程双连接：ctrl（建 context）+ data（enable + 读事件流）——recordproto
    // 官方推荐模型，模块文档。
    let ctrl = match connect() {
        Ok(conn) => conn,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("工作线程 X 连接失败：{error}")));
            return;
        }
    };
    let data = match connect() {
        Ok(conn) => conn,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("工作线程 X 数据连接失败：{error}")));
            return;
        }
    };
    if let Err(error) = require_extension(&ctrl, record::X11_EXTENSION_NAME) {
        let _ = ready_tx.send(Err(format!("{error}")));
        return;
    }
    // 查询版本（recordproto 1.13；仅探测服务器支持，返回值不参与后续逻辑）。
    let versioned = match ctrl.record_query_version(
        record::X11_XML_VERSION.0 as u16,
        record::X11_XML_VERSION.1 as u16,
    ) {
        Ok(cookie) => cookie.reply().map(|_| ()),
        Err(error) => Err(error.into()), // ConnectionError → ReplyError（errors.rs 提供）
    };
    if let Err(error) = versioned {
        let _ = ready_tx.send(Err(format!("record_query_version 失败：{error}")));
        return;
    }
    // 建 context：捕获核心设备事件段 KeyPress(2)..MotionNotify(6)（含 KeyRelease/
    // ButtonPress/ButtonRelease），ALL_CLIENTS（服务器全体客户端的事件流）。
    let context = match ctrl.generate_id() {
        Ok(id) => id,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("generate_id 失败：{error}")));
            return;
        }
    };
    let empty8 = record::Range8 { first: 0, last: 0 };
    let range = record::Range {
        core_requests: empty8,
        core_replies: empty8,
        ext_requests: record::ExtRange {
            major: empty8,
            minor: record::Range16 { first: 0, last: 0 },
        },
        ext_replies: record::ExtRange {
            major: empty8,
            minor: record::Range16 { first: 0, last: 0 },
        },
        delivered_events: empty8,
        device_events: record::Range8 {
            first: X_EVENT_KEY_PRESS,
            last: X_EVENT_MOTION_NOTIFY,
        },
        errors: empty8,
        client_started: false,
        client_died: false,
    };
    let created =
        match ctrl.record_create_context(context, 0, &[record::CS::ALL_CLIENTS.into()], &[range]) {
            Ok(cookie) => cookie.check(),
            Err(error) => Err(error.into()),
        };
    if let Err(error) = created {
        let _ = ready_tx.send(Err(format!("record_create_context 失败：{error}")));
        return;
    }
    // enable + 就绪握手 + 阻塞迭代（disable 触发 EndOfData 终止；模块文档捕获模型）。
    //
    // **时序关键**：ready 信号必须在服务器的 StartOfData 首应答之后才发——stop() 的
    // record_disable_context 经宿主连接直达服务器，与工作线程的 enable 请求分属两条
    // 连接、到达顺序无保证；若宿主在 enable 到达服务器前就 disable（start 立即 stop
    // 的竞态），服务器会"先禁后启"→ context 保持启用 → 迭代器永远收不到 EndOfData
    // → join 挂死（远端 Xwayland :2 实测复现）。StartOfData 应答 = 服务器已处理
    // enable 的凭证，此后 disable 必然有序生效（11-17-m3-input-x11 实测修正）。
    let mut cookie = match data.record_enable_context(context) {
        Ok(cookie) => cookie,
        Err(_) => {
            // 连接死亡：无法 enable 也无法向宿主交付——发 Err 让 start 报错（而非
            // 挂死在 recv）。
            let _ = ready_tx.send(Err("record_enable_context 发送失败（连接死亡）".into()));
            return;
        }
    };
    match cookie.next() {
        // StartOfData(4) = 服务器确认 enable 的标准凭证；任何其它应答同样证明服务器
        // 已处理 enable（就绪排序成立）——放行宿主，异常分类不单独报错。
        Some(Ok(_)) => {}
        // enable 阶段错误/提前终止：报错退出（宿主 start 报 Err，不挂死）。
        Some(Err(error)) => {
            let _ = ready_tx.send(Err(format!("record_enable_context 失败：{error}")));
            return;
        }
        None => {
            let _ = ready_tx.send(Err("record_enable_context 提前终止（EndOfData）".into()));
            return;
        }
    }
    // 就绪握手：宿主 start() 等到此步才认为捕获可用（mac 后端就绪握手同构）。
    if ready_tx.send(Ok(context)).is_err() {
        return; // 宿主已放弃（start 出错/被丢弃）；shutdown 会 free context
    }
    for reply in cookie {
        let Ok(reply) = reply else {
            break; // 连接错误：终止事件流（不再补救，线程退出）
        };
        if reply.client_swapped {
            continue; // 字节序不合的客户端数据无法解析（x11rb 官方示例同款防御）
        }
        if reply.category != RECORD_FROM_SERVER {
            continue; // StartOfData(4)/EndOfData(5) 等控制分类无事件数据
        }
        let mut remaining = reply.data.as_slice();
        while !remaining.is_empty() {
            match parse_device_event(remaining, &sender, &mods_bits, &baseline) {
                Some(rest) => remaining = rest,
                None => break, // 未知/解析失败的数据块：丢弃剩余（防御，正常不发生）
            }
        }
    }
}

/// 解析 reply.data 内的下一个 32B 核心设备事件并投递（返回剩余字节；无法识别返回
/// `None`）。XRecord 打包的即 X 服务器广播的核心事件结构（x11rb `TryParse` 逐类型
/// 解析，官方示例同款）。
fn parse_device_event<'a>(
    data: &'a [u8],
    sender: &Sender<InputEvent>,
    mods_bits: &AtomicU32,
    baseline: &MouseBaseline,
) -> Option<&'a [u8]> {
    let response_type = *data.first()?;
    match response_type {
        X_EVENT_KEY_PRESS | X_EVENT_KEY_RELEASE => {
            let (event, rest) = KeyPressEvent::try_parse(data).ok()?;
            // keycode → XKey（x11keymap 全表；表外 keycode 拒绝丢弃——未知键语义）。
            if let Some(code) = x11keymap::keycode_to_xkey(event.detail) {
                let up = response_type == X_EVENT_KEY_RELEASE;
                let state = if up { KeyState::Up } else { KeyState::Down };
                // 修饰键快照位（左右手由 keycode 区分；state 字段无左右手语义——模块文档）。
                if let Some(bit) = code.mods_bit() {
                    let mask = 1_u32 << bit;
                    if up {
                        mods_bits.fetch_and(!mask, Ordering::Relaxed);
                    } else {
                        mods_bits.fetch_or(mask, Ordering::Relaxed);
                    }
                }
                // 长按重复不区分：重复 KeyPress 恒发 Key Down（模块文档已知限制）。
                let _ = sender.send(InputEvent::Key { code, state });
            }
            Some(rest)
        }
        X_EVENT_BUTTON_PRESS | X_EVENT_BUTTON_RELEASE => {
            let (event, rest) = ButtonPressEvent::try_parse(data).ok()?;
            let up = response_type == X_EVENT_BUTTON_RELEASE;
            let event_out = if up {
                // 滚轮 release（4-7）忽略：一格滚轮 = press+release 对，捕获只发一格。
                button_from_x(event.detail).map(|button| InputEvent::MouseButton {
                    button,
                    state: KeyState::Up,
                })
            } else {
                // 滚轮 press（4-7）→ Wheel 一格；普通按钮 1/2/3/8/9 → MouseButton Down。
                if let Some((axis, delta)) = wheel_from_x(event.detail) {
                    Some(InputEvent::Wheel { axis, delta })
                } else {
                    button_from_x(event.detail).map(|button| InputEvent::MouseButton {
                        button,
                        state: KeyState::Down,
                    })
                }
            };
            if let Some(event_out) = event_out {
                let _ = sender.send(event_out);
            }
            Some(rest)
        }
        X_EVENT_MOTION_NOTIFY => {
            let (event, rest) = MotionNotifyEvent::try_parse(data).ok()?;
            // 绝对坐标做差 → 相对位移（win 后端 LL 鼠标同款；静止事件不转发）。
            let dx = i32::from(event.root_x)
                - baseline.x.swap(i32::from(event.root_x), Ordering::Relaxed);
            let dy = i32::from(event.root_y)
                - baseline.y.swap(i32::from(event.root_y), Ordering::Relaxed);
            if dx != 0 || dy != 0 {
                let _ = sender.send(InputEvent::MouseMoveRel {
                    dx: dx as f32,
                    dy: dy as f32,
                });
            }
            Some(rest)
        }
        // device_events 范围限定 2..6，不应出现其它类型；防御性终止本块解析。
        _ => None,
    }
}

// ============================================================
// 全局热键（docs/05 §3 热键行：grab_key 被动抓取，M4-hotkeys）
// ============================================================

/// 热键停机 ClientMessage 的 atom 名（工作连接 intern、宿主连接同名字符串
/// intern——atom 是服务器全局资源，同名同值）。
const HOTKEY_STOP_ATOM: &[u8] = b"XCONTROL_HOTKEY_STOP";

/// 锁定修饰键变体集（i3 同款策略，raw 位值）：被动 grab 的 modifiers 字段做
/// **精确匹配**（X 协议语义——与 core `HotkeySet` 的「精确集合」裁定同构），
/// CapsLock（LOCK）/NumLock（M2，惯例）/Mod5 按下时基组合不命中。本模块对每
/// 组合额外抓取 {0, LOCK, M2, M5} 全部 8 个变体，锁定键状态不干扰热键；KeyPress
/// 到达后剥离锁定位再查表（变体抓取只扩大触发面，不改变语义）。
///
/// raw 位值锚点（ModMask 常量同值，测试断言）：LOCK=1<<1、M2=1<<4、M5=1<<7。
/// ModMask 无 const 构造/位运算（bitmask_binop 宏非 const），变体表取 raw 值 +
/// 逐条 `ModMask::from`。
const LOCK_VARIANT_BITS: [u16; 8] = [
    0x00,
    0x02,               // LOCK
    0x10,               // M2（NumLock 惯例位）
    0x80,               // M5
    0x02 | 0x10,        // LOCK|M2
    0x02 | 0x80,        // LOCK|M5
    0x10 | 0x80,        // M2|M5
    0x02 | 0x10 | 0x80, // LOCK|M2|M5
];

/// 锁定位剥离掩码（KeyPress.state → 注册 base 掩码比对前剥离；u16 域运算，
/// [`LOCK_VARIANT_BITS`] 同锚点）。
const LOCK_BITS: u16 = 0x02 | 0x10 | 0x80;

/// 家族位 → X 修饰键掩码（base 组合，不含锁定位）。
///
/// X 服务器的修饰掩码不区分左右手（KeyButMask 只有 any-Control/Mod1 位）——
/// **天然就是 core `ModFamilies` 的「任一侧」语义**，左右手任一 Ctrl/Alt 均满足；
/// Meta/Super → Mod4（惯例 Super/Win 位）。
#[must_use]
fn families_to_x_mask(families: ModFamilies) -> ModMask {
    let mut mask = ModMask::from(0u16);
    if families.contains(ModFamilies::CTRL) {
        mask |= ModMask::CONTROL;
    }
    if families.contains(ModFamilies::SHIFT) {
        mask |= ModMask::SHIFT;
    }
    if families.contains(ModFamilies::ALT) {
        mask |= ModMask::M1;
    }
    if families.contains(ModFamilies::META) {
        mask |= ModMask::M4;
    }
    mask
}

/// X11 全局热键源（[`X11HotkeySource`]：root 窗口被动 `grab_key` + 事件线程
/// `wait_for_event`，docs/05 §3 热键行）。
///
/// - **被动抓取语义 = FR-2.5 的 OS 层吞键**：grab 的按键只上报抓取客户端
///   （`owner_events=false` + root 窗口），焦点窗口收不到——「命中本地处理、
///   不投递本机应用」由服务器保证；modifiers 精确匹配 = core「精确集合」裁定
///   的服务器侧实现（多余修饰键按下时 grab 不触发、事件照常走焦点窗口）；
/// - **两端同型注册**（FR-2.5）：主控/从机各注册一份（与本机会话路由衔接）；
/// - **连接模型（capture/剪贴板 watcher 三连接先例）**：工作连接（抓取 + 事件
///   接收，被动 grab 的 KeyPress 只投抓取客户端）+ 宿主停机连接（stop 时
///   `send_event` ClientMessage 到停机窗口——`event_mask=NO_EVENT` 投递给窗口
///   创建客户端，X11 protocol spec §8，剪贴板 watcher 实测先例）；
/// - **双端 sync 时序**（剪贴板 watcher 实测教训）：工作线程退出前 destroy + sync、
///   停机连接 send + sync 才 join——防跨连接窗口 id 段复用竞态（X 服务器对跨
///   客户端请求无顺序保证）；
/// - **grab 生命周期**：passive grab 归抓取客户端所有，客户端断开时服务器自动
///   释放（泄漏兜底）；显式路径线程退出前逐条 `ungrab_key`（8 变体全量）。
///
/// # Xwayland 已知限制（M3 先例同源，移交 M5）
///
/// rootless Xwayland（compositor 会话）下键盘事件由 compositor 先行处理：
/// X 客户端的 passive grab 对 Wayland 原生焦点窗口的按键**可能不触发**
/// （KWin 把按键路由给 Wayland 表面）。M4 实测记录见模块文档实测一节；
/// Wayland 原生热键路径（GlobalShortcuts portal / 事件流识别回退）属 M5。
pub struct X11HotkeySource {
    receiver: Receiver<HotkeyId>,
    /// 停机目的地（工作连接创建的窗口 + 停机 atom；stop 时消费，Drop 兜底同）。
    stop_target: Option<(Window, Atom)>,
    worker: Option<JoinHandle<()>>,
}

// 手写 Debug（app 的 PlatformHotkeySource derive 需要；JoinHandle/工作连接
// 非 Debug——只投影可读面：事件接收端 + 停机目的地 + 工作线程存活性）。
impl std::fmt::Debug for X11HotkeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11HotkeySource")
            .field("receiver", &self.receiver)
            .field(
                "stop_target",
                &self
                    .stop_target
                    .as_ref()
                    .map(|(window, atom)| format!("{window}/{atom}")),
            )
            .field("worker_alive", &self.worker.is_some())
            .finish()
    }
}

/// 注册线程就绪载荷：`Ok((停机窗口, 停机 atom))` 或 `Err(原因)`。
type HotkeyReadyPayload = Result<(Window, Atom), String>;

impl X11HotkeySource {
    /// 注册热键集合（工作线程：连接 → 建停机窗 → intern atom → 逐组合
    /// grab 8 变体 → 就绪握手 → `wait_for_event` 循环）。
    ///
    /// 任一组合抓取失败 → 全量 ungrab 回滚后报错（fail loud，与 win
    /// `RegisterHotKey` 裁定一致——半注册态不可诊断）。
    ///
    /// # Errors
    /// [`InputError::Platform`]：$DISPLAY 不可用 / 窗口创建或 grab 被拒
    /// （他客户端持有）/ 主键无 keycode 映射 / 线程就绪前退出。
    pub fn register(set: &HotkeySet) -> Result<Self, InputError> {
        let (sender, receiver) = channel::<HotkeyId>();
        let (ready_tx, ready_rx) = channel::<HotkeyReadyPayload>();
        let set = set.clone(); // 所有权入线程

        let worker = std::thread::Builder::new()
            .name("xcontrol-hotkey-x11".into())
            .spawn(move || run_hotkey_grab_thread(ready_tx, sender, set))
            .map_err(|error| InputError::Platform {
                action: "hotkey-spawn",
                reason: format!("热键线程创建失败：{error}"),
            })?;

        let stop_target = ready_rx.recv().map_err(|_| InputError::Platform {
            action: "hotkey-start",
            reason: "X11 热键线程在就绪前退出（panic 或提前 return）".into(),
        })?;
        let stop_target = stop_target.map_err(|reason| InputError::Platform {
            action: "hotkey-register",
            reason,
        })?;

        Ok(Self {
            receiver,
            stop_target: Some(stop_target),
            worker: Some(worker),
        })
    }

    /// 显式停止：懒建停机连接 → ClientMessage（NO_EVENT → 创建者客户端）→
    /// flush + **sync** → join（线程随后 ungrab 全量 + destroy 停机窗 + sync）。
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

    /// 关停公共路径（[`X11HotkeySource::stop`] 与 [`Drop`] 共用）。
    ///
    /// 时序保证（剪贴板 watcher 同款）：停机信号发出后以 `sync()`（GetInputFocus
    /// 往返）确认服务器已处理 SendEvent 再 join——不经 sync 直接 join 时信号
    /// 投递时机不定，与工作线程状态竞态（`watcher_restart_stress` 实测教训）。
    fn shutdown(&mut self) -> Result<(), InputError> {
        // 停机信号：对停机窗发 ClientMessage（空 event_mask → 投递给创建该窗口
        // 的客户端 = 工作线程连接）。服务器已死时连接失败静默——工作线程的
        // wait_for_event 同时报错退出，join 为准。
        if let Some((window, atom)) = self.stop_target.take()
            && let Ok((conn, _)) = RustConnection::connect(None)
        {
            let event = ClientMessageEvent::new(32, window, atom, [0u32; 5]);
            // 停机是尽力而为信号：send 失败静默（窗口已不存在 = 连接即将回收）。
            let _ = conn.send_event(false, window, EventMask::NO_EVENT, event);
            // flush + sync：服务器处理完 SendEvent 才 join。
            let _ = conn.flush();
            let _ = WrapperExt::sync(&conn);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| InputError::Platform {
                action: "hotkey-stop",
                reason: "X11 热键线程 panic".into(),
            })?;
        }
        Ok(())
    }
}

impl Drop for X11HotkeySource {
    fn drop(&mut self) {
        // 防泄漏兜底：未显式 stop 即丢弃时也发停机信号并 join（幂等；join 失败
        // 在 Drop 中不可传播）。
        let _ = self.shutdown();
    }
}

/// 注册条目（查表用）：base 掩码（剥离锁定位后的比对键）+ 动作。
struct HotkeyGrab {
    base_mask: u16,
    id: HotkeyId,
}

/// 热键抓取线程主体：连接 → 建停机窗 + intern atom → 逐组合抓取 → 就绪握手 →
/// `wait_for_event` 循环 → 收尾（全量 ungrab + destroy + sync）。
///
/// 回调（迭代）纪律（platform-code.md）：线程内只做「KeyPress 解码 → 查表 →
/// mpsc 投递」，零 tokio、零大对象分配。
fn run_hotkey_grab_thread(
    ready_tx: Sender<HotkeyReadyPayload>,
    sender: Sender<HotkeyId>,
    set: HotkeySet,
) {
    let (conn, _) = match RustConnection::connect(None) {
        Ok(pair) => pair,
        Err(error) => {
            let _ = ready_tx.send(Err(format!(
                "X11 连接失败（检查 $DISPLAY 与 $XAUTHORITY，docs/05 §6）：{error}"
            )));
            return;
        }
    };
    let root = match setup_screen(&conn) {
        Ok(screen) => screen.root,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("屏幕查询失败：{error}")));
            return;
        }
    };

    // 停机窗（InputOnly 1×1；event_mask 空——ClientMessage 的目的地，自身不收
    // 事件）+ 停机 atom（工作连接 intern，宿主连接按同名 intern 同值）。
    let stop_window = match conn.generate_id() {
        Ok(window) => window,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("generate_id 失败：{error}")));
            return;
        }
    };
    if let Err(error) = conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        stop_window,
        root,
        -1,
        -1,
        1,
        1,
        0,
        xproto::WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &xproto::CreateWindowAux::new().event_mask(EventMask::NO_EVENT),
    ) {
        let _ = ready_tx.send(Err(format!("停机窗口创建失败：{error}")));
        return;
    }
    let stop_atom = match conn.intern_atom(false, HOTKEY_STOP_ATOM) {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => reply.atom,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("intern_atom reply 失败：{error}")));
                return;
            }
        },
        Err(error) => {
            let _ = ready_tx.send(Err(format!("intern_atom 失败：{error}")));
            return;
        }
    };

    // 逐组合抓取（8 锁定变体；查表按 (keycode, base_mask) 建——同键组合共享
    // keycode，KeyPress 的 state 剥离锁定位后区分）。
    let mut grabs: Vec<(u8, HotkeyGrab)> = Vec::new();
    let mut registered: Vec<(u8, ModMask)> = Vec::new(); // (keycode, 变体掩码) 回滚面
    let mut conflict_seen: Vec<(u8, u16)> = Vec::new();
    for id in HotkeyId::ALL {
        let Some(combo) = set.combo(id) else {
            continue;
        };
        let Some(keycode) = crate::x11keymap::xkey_to_keycode(combo.key) else {
            rollback_grabs(&conn, root, &registered);
            let _ = conn.destroy_window(stop_window);
            let _ = conn.sync();
            let _ = ready_tx.send(Err(format!(
                "热键 {} 主键无 keycode 映射（x11keymap 缺项）",
                id.label()
            )));
            return;
        };
        let base = families_to_x_mask(combo.families);
        let base_bits = u16::from(base);
        // 同 (keycode, base) 的组合先到先得（与 core match_raw 条目序语义一致）。
        let key_pair = (keycode, base_bits);
        if conflict_seen.contains(&key_pair) {
            continue;
        }
        conflict_seen.push(key_pair);
        for variant_bits in LOCK_VARIANT_BITS {
            let mask_bits = base_bits | variant_bits;
            let mask = ModMask::from(mask_bits);
            // SAFETY: 无——x11rb 安全绑定。grab 是服务器全局副作用：失败即
            // 回滚（下方统一 ungrab），fail loud。
            if let Err(error) =
                conn.grab_key(false, root, mask, keycode, GrabMode::ASYNC, GrabMode::ASYNC)
            {
                rollback_grabs(&conn, root, &registered);
                let _ = conn.destroy_window(stop_window);
                let _ = conn.sync();
                let _ = ready_tx.send(Err(format!(
                    "grab_key({} keycode={} mask={:#x}) 失败（他客户端持有？）：{error}",
                    id.label(),
                    keycode,
                    mask_bits
                )));
                return;
            }
            registered.push((keycode, mask));
        }
        grabs.push((
            keycode,
            HotkeyGrab {
                base_mask: base_bits,
                id,
            },
        ));
    }
    // 注册完毕：sync 确认服务器已处理全部 grab（就绪语义 = 抓取生效）。
    if let Err(error) = conn.sync() {
        let _ = ready_tx.send(Err(format!("注册后 sync 失败：{error}")));
        return;
    }

    if ready_tx.send(Ok((stop_window, stop_atom))).is_err() {
        // 宿主已放弃：全量清理后退出。
        rollback_grabs(&conn, root, &registered);
        let _ = conn.destroy_window(stop_window);
        let _ = conn.sync();
        return;
    }

    // 事件循环：KeyPress → 查表投递；ClientMessage(停机 atom) → 退出。
    // 连接死亡（服务器退出/stop 连接异常）→ wait_for_event Err → 循环自然终止
    // （grab 随断开自动释放）。
    //
    // 按住不重复（docs/03 §4.6「防按住连环切换」）：grab 只拦主键按下，**服务器
    // XKB autorepeat 以「同键同时间戳的 Release+Press 对」持续交付重复**（Xwayland
    // :2 实测：按住 1.5s = 21 对重复，周期 ~40ms，对内两事件 time 相同；真释放
    // 无随行同戳 Press）。与 win `MOD_NOREPEAT`（`WinHotkeySource`）和 mac 流过滤
    // 吞尾（core `HotkeyFilter`）同语义的三段防线：
    // 1. `held` 位集：每 keycode 首次 KeyPress 投递，重复吞；
    // 2. `pending_release` 挂起：KeyRelease 不立即清 held——下一事件若为**同键同
    //    时间戳** KeyPress = 合成对的 Press（整体吞，保持 held）；其余任何事件 =
    //    释放为真（清 held）。同戳判据是确定性的（对内同戳、真重按必有新戳）。
    // grab 只上报被抓取的组合键，held 集恒不含修饰键——修饰键 Up 不经过本循环。
    let mut held: std::collections::HashSet<u8> = std::collections::HashSet::new();
    let mut pending_release: Option<(u8, x11rb::protocol::xproto::Timestamp)> = None;
    while let Ok(event) = conn.wait_for_event() {
        // 结算挂起的 KeyRelease：只有「同键同时间戳的 KeyPress」是合成对的成员。
        let synthetic_pair = match (&event, pending_release) {
            (Event::KeyPress(key), Some((code, time))) => key.detail == code && key.time == time,
            _ => false,
        };
        if let Some((code, _)) = pending_release.take() {
            if synthetic_pair {
                // 合成对整体吞（Release 已过，Press 在此丢弃；held 保持）。
                continue;
            }
            held.remove(&code);
        }
        match event {
            Event::KeyPress(key) => {
                // KeyButMask → u16 域：剥离锁定位后与注册 base 掩码比对
                // （服务器已按变体精确匹配，此处只是还原 base 键）。
                let state_bits = u16::from(key.state) & !LOCK_BITS;
                let hit = grabs
                    .iter()
                    .find(|(keycode, grab)| *keycode == key.detail && grab.base_mask == state_bits);
                if let Some((_, grab)) = hit
                    && held.insert(key.detail)
                {
                    // 首次按下：投递（接收端关闭 = 捕获停止中，静默忽略——回调不得补救）。
                    let _ = sender.send(grab.id);
                }
                // held 已含该 keycode = 按住重复（autorepeat）：吞，不投递。
            }
            Event::KeyRelease(key) => {
                // 挂起待判定（下一事件结算真伪）；未 held 的 Release（防御形态）
                // 忽略。
                if held.contains(&key.detail) {
                    pending_release = Some((key.detail, key.time));
                }
            }
            // 停机信号（宿主连接 send_event，NO_EVENT 掩码 → 创建者客户端）。
            Event::ClientMessage(msg) if msg.type_ == stop_atom => break,
            _ => {} // 其余 ClientMessage / MappingNotify 等：忽略
        }
    }

    // 收尾：全量 ungrab（8 变体）+ destroy 停机窗 + sync（剪贴板 watcher 双端
    // sync 纪律：destroy 在服务器处理完毕才释放窗口 id 段）。
    rollback_grabs(&conn, root, &registered);
    let _ = conn.destroy_window(stop_window);
    let _ = conn.sync();
}

/// 全量 ungrab（尽力逐条；客户端断开时服务器自动释放剩余 grab——泄漏兜底）。
fn rollback_grabs(conn: &RustConnection, root: Window, registered: &[(u8, ModMask)]) {
    for (keycode, mask) in registered {
        let _ = conn.ungrab_key(*keycode, root, *mask);
    }
}

// ============================================================
// 测试（纯逻辑：坐标/滚轮/按钮换算与 keysym 映射；X 连接类测试按 $DISPLAY 容错跳过；
// 回环冒烟 #[ignore]——远端实测配方见模块文档）
// ============================================================

#[cfg(test)]
mod tests {
    use xcontrol_core::sink::{InputSink, InputSource};
    use xcontrol_proto::input::{KeyState, Mods, ScreenEdge};
    use xcontrol_proto::xkey::{
        CK_VOLUME_UP, KEY_A, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_META, KEY_LEFT_SHIFT, KEY_RIGHT,
        KEY_RIGHT_SHIFT, KEY_SPACE, KEY_UP,
    };

    use super::*;

    /// $DISPLAY 可用性（连接类测试的容错前提；CI/无 X 环境跳过而非失败）。
    fn display_available() -> bool {
        match std::env::var_os("DISPLAY") {
            Some(value) => !value.is_empty(),
            None => false,
        }
    }

    /// 热键 grab 冒烟互斥锁（两热键测试争抢同一组合键的被动 grab + XTEST 注入
    /// 状态是服务器全局的——并发互扰实测见 `hotkey_register_stop_roundtrip` doc；
    /// poison 透传——上一个测试 panic 不连锁击穿下一个）。
    static HOTKEY_SMOKE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 指针坐标归一化（`cursor_normalized` 域换算契约，11-22 修复轮）：
    /// ±1 单位容差带收敛端点 + 中段比例换算（与 wayland `abs_normalized`
    /// 同型——两侧被控回报同一主控判定面）。
    #[test]
    fn pointer_snap_normalized_snaps_endpoints_and_scales() {
        assert_eq!(pointer_snap_normalized(0.0, 1919.0), 0.0);
        assert_eq!(pointer_snap_normalized(1.0, 1919.0), 0.0);
        assert_eq!(pointer_snap_normalized(1919.0, 1919.0), 1.0);
        assert_eq!(pointer_snap_normalized(1918.0, 1919.0), 1.0);
        assert!((pointer_snap_normalized(960.0, 1919.0) - 960.0 / 1919.0) < 1e-6);
        // warp/绝对注入可把坐标推过域上界：clamp 到 1.0。
        assert_eq!(pointer_snap_normalized(2560.0, 1919.0), 1.0);
        assert_eq!(pointer_snap_normalized(-3.0, 1919.0), 0.0);
    }

    /// f64 累加器：整数提取 + 余量保留（docs/02 §4.2 语义；mac/win 同款锚点对拍）。
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

    /// 归一化 → root 像素坐标（越界收敛边界；docs/02 §4.2 kind 0x04）。
    #[test]
    fn abs_to_root_normalizes_and_clamps() {
        assert_eq!(abs_to_root(0.0, 0.0, 1920.0, 1080.0), (0, 0));
        assert_eq!(abs_to_root(1.0, 1.0, 1920.0, 1080.0), (1920, 1080));
        assert_eq!(abs_to_root(0.5, 0.5, 1920.0, 1080.0), (960, 540));
        // 越界收敛（协议侧越界值不得放大为注入错误）
        assert_eq!(abs_to_root(-0.5, 1.5, 1920.0, 1080.0), (0, 1080));
        // NaN 直透 clamp（由末位 `as i16` 饱和转换收敛到 0；warp 入口已拒绝非有限值）
        assert_eq!(abs_to_root(f32::NAN, f32::NAN, 1920.0, 1080.0), (0, 0));
    }

    /// i32 → i16 注入量饱和（XTEST root_x/root_y 形参域）。
    #[test]
    fn clamp_i16_saturates() {
        assert_eq!(clamp_i16(0), 0);
        assert_eq!(clamp_i16(40_000), i16::MAX);
        assert_eq!(clamp_i16(-40_000), i16::MIN);
        assert_eq!(clamp_i16(300), 300);
    }

    /// 按钮映射双向（注入 1/2/3/8/9；捕获滚轮 4-7 与 release 忽略语义）。
    #[test]
    fn button_mapping_roundtrip() {
        assert_eq!(x_button(MouseButton::Left), 1);
        assert_eq!(x_button(MouseButton::Middle), 2);
        assert_eq!(x_button(MouseButton::Right), 3);
        assert_eq!(x_button(MouseButton::Back), 8);
        assert_eq!(x_button(MouseButton::Forward), 9);
        for (detail, button) in [
            (1u8, MouseButton::Left),
            (2, MouseButton::Middle),
            (3, MouseButton::Right),
            (8, MouseButton::Back),
            (9, MouseButton::Forward),
        ] {
            assert_eq!(button_from_x(detail), Some(button));
        }
        // 滚轮/倾斜/未知按钮不映射为普通按钮
        for detail in [0u8, 4, 5, 6, 7, 10, 255] {
            assert!(button_from_x(detail).is_none(), "按钮 {detail} 应不映射");
        }
    }

    /// 滚轮按钮号 → (轴, 格)（捕获方向：4 上/5 下/6 左/7 右，win 原生符号语义）。
    #[test]
    fn wheel_mapping() {
        assert_eq!(wheel_from_x(4), Some((Axis::Vertical, 1.0)));
        assert_eq!(wheel_from_x(5), Some((Axis::Vertical, -1.0)));
        assert_eq!(wheel_from_x(6), Some((Axis::Horizontal, -1.0)));
        assert_eq!(wheel_from_x(7), Some((Axis::Horizontal, 1.0)));
        assert_eq!(wheel_from_x(3), None);
        // 注入方向逆对称
        assert_eq!(x_wheel_button(Axis::Vertical, 2), Some(X_BUTTON_WHEEL_UP));
        assert_eq!(
            x_wheel_button(Axis::Vertical, -1),
            Some(X_BUTTON_WHEEL_DOWN)
        );
        assert_eq!(
            x_wheel_button(Axis::Horizontal, 1),
            Some(X_BUTTON_WHEEL_RIGHT)
        );
        assert_eq!(
            x_wheel_button(Axis::Horizontal, -3),
            Some(X_BUTTON_WHEEL_LEFT)
        );
    }

    /// latin-1 keysym 直通（docs/03 §4.5：0x20–0xFF = Unicode 码点）与控制字符拒绝。
    #[test]
    fn keysym_for_char_latin1() {
        assert_eq!(keysym_for_char('a'), Some(0x61));
        assert_eq!(keysym_for_char('A'), Some(0x41));
        assert_eq!(keysym_for_char('控'), None); // U+63A7 超出 latin-1
        assert_eq!(keysym_for_char('😀'), None); // 代理对外区段
        assert_eq!(keysym_for_char('\n'), None); // 控制字符（inject_text 特判）
        assert_eq!(keysym_for_char('\u{01}'), None);
    }

    /// 修饰键快照位与 mods bit0-7 对应（事件线程写路径的宿主读复算；win 后端同款）。
    #[test]
    fn mods_bits_layout_matches_protocol() {
        let mods_bits = Arc::new(AtomicU32::new(0));
        for (key, bit) in [
            (KEY_LEFT_CTRL, 0_u8),
            (KEY_RIGHT_SHIFT, 5),
            (KEY_LEFT_META, 3),
        ] {
            let mask = 1_u32 << u32::from(bit);
            mods_bits.fetch_or(mask, Ordering::Relaxed);
            assert_eq!(Mods::from_key(key), Some(Mods::from_key_bit(bit)));
        }
        assert_eq!(
            Mods::from_bits(mods_bits.load(Ordering::Relaxed)).bits(),
            0b0010_1001,
            "LCtrl(bit0)|LMeta(bit3)|RShift(bit5) 位组"
        );
        // 左右手区分：KEY_LEFT_SHIFT(bit1) 与 KEY_RIGHT_SHIFT(bit5) 不同位——
        // X state 掩码做不到的语义（模块文档），keycode 路径保证。
        assert_ne!(
            Mods::from_key(KEY_LEFT_SHIFT).unwrap().bits(),
            Mods::from_key(KEY_RIGHT_SHIFT).unwrap().bits()
        );
    }

    /// Text 事件预扫描拒绝语义：CJK 字符在 latin-1 路径不可达（无 $DISPLAY 时
    /// 构建注入器失败则跳过——错误文案保留证据）。
    #[test]
    fn text_cjk_is_unsupported() {
        if !display_available() {
            println!("无 $DISPLAY：跳过 Text 语义测试（远端实测配方见模块文档）");
            return;
        }
        let mut injector = match X11Injector::new() {
            Ok(injector) => injector,
            Err(error) => {
                println!("注入器构建失败（记录）：{error}");
                return;
            }
        };
        let error = injector
            .inject(&InputEvent::Text {
                utf8: "ab控".into(),
            })
            .expect_err("CJK 字符应整段拒绝（不注入半个词）");
        assert!(matches!(error, SinkError::Unsupported(_)));
    }

    /// 路由层事件（EnterScreen 等）到注入层为静默忽略（mac/win 后端同构语义）。
    #[test]
    fn routing_events_are_ignored() {
        if !display_available() {
            println!("无 $DISPLAY：跳过（X11Injector 构建需要 X 连接）");
            return;
        }
        let mut injector = match X11Injector::new() {
            Ok(injector) => injector,
            Err(error) => {
                println!("注入器构建失败（记录）：{error}");
                return;
            }
        };
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

    /// 无害注入冒烟（默认可跑——$DISPLAY 存在时；零滚轮/空文本/空 release_all）。
    #[test]
    fn inject_harmless_events_are_ok() {
        if !display_available() {
            println!("无 $DISPLAY：跳过（远端实测配方见模块文档）");
            return;
        }
        let mut injector = match X11Injector::new() {
            Ok(injector) => injector,
            Err(error) => {
                println!("注入器构建失败（记录）：{error}");
                return;
            }
        };
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
        // root 几何合理（Xinerama 并集必有正尺寸）与光标查询可用
        let geometry = display_geometry(&injector.conn).expect("root 几何查询");
        assert!(geometry.width > 0 && geometry.height > 0);
        let (cx, cy) = cursor_location(&injector.conn).expect("光标查询");
        assert!(cx.is_finite() && cy.is_finite());
    }

    /// 未知 ScreenId warp 拒绝（M3 单表面语义：非 0 一律 Failed；拒绝发生在
    /// X 请求之前，无真实输入，$DISPLAY 存在时可跑）。
    #[test]
    fn warp_unknown_screen_fails() {
        if !display_available() {
            println!("无 $DISPLAY：跳过");
            return;
        }
        let mut injector = match X11Injector::new() {
            Ok(injector) => injector,
            Err(error) => {
                println!("注入器构建失败（记录）：{error}");
                return;
            }
        };
        let error = injector.warp(99, 0.5, 0.5).expect_err("非 0 屏应拒绝");
        assert!(matches!(error, SinkError::Failed { action: "warp", .. }));
    }

    /// 键盘/按钮注入 + 释放记账（#[ignore]：**会产生真实输入**——Space 键与中键
    /// 点击落到当前前台应用；远端运行
    /// `cargo test -p xcontrol-input --features x11 -- --ignored --nocapture`）。
    #[test]
    #[ignore = "会产生真实输入（Space 键 + 中键点击）：确认前台应用无未保存内容后运行"]
    fn inject_key_and_button_round_trip() {
        let mut injector = X11Injector::new().expect("注入器构建");
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

    /// ScreenId 0（root 并集）warp（#[ignore]：**会移动真实光标**到屏幕中心）。
    #[test]
    #[ignore = "会移动真实光标到 root 中心"]
    fn warp_root_center_moves_cursor() {
        let mut injector = X11Injector::new().expect("注入器构建");
        injector.warp(0, 0.5, 0.5).expect("ScreenId 0 warp 应成功");
    }

    /// 捕获源启停冒烟（默认可跑，$DISPLAY 存在时；XRecord context 生命周期验证：
    /// start → 100ms 静默窗口 → stop 干净退出）。
    #[test]
    fn capture_start_poll_stop_smoke() {
        if !display_available() {
            println!("无 $DISPLAY：跳过（远端实测配方见模块文档）");
            return;
        }
        let mut source = match X11CaptureSource::start() {
            Ok(source) => source,
            Err(error) => {
                // RECORD 扩展缺失等环境问题如实记录，不判定为回归。
                println!("XRecord 捕获启动失败（记录）：{error}");
                return;
            }
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut events = Vec::new();
        source.poll(&mut events);
        println!("100ms 窗口捕获事件数：{}", events.len());
        assert!(source.mods().bits() & !0xFF == 0, "mods 位域应在 bit0-7 内");
        source.stop().expect("捕获应干净停止（join 无 panic）");
    }

    /// **回环冒烟：XTEST 注入 → XRecord 收到**（#[ignore]；本任务核心验收——
    /// X11 捕获不过滤自注入使同进程回环可行，模块文档防回环语义）。
    ///
    /// 注入无害键 F15（keycode 193 = evdev KEY_F15(185)+8，桌面默认无绑定）→
    /// 800ms 内捕获应收到 Key{F15, Down} 与 Key{F15, Up}。XRecord 的 device_events
    /// 记录的是**已投递给 X 客户端的事件**（recordproto 语义）：键盘事件投递给焦点
    /// 窗口所属客户端——本测试自建一个映射窗口并 set_input_focus（focus 独立于 WM
    /// 的窗口激活；BadMatch 容错跳过），保证存在确定的投递目标。远端运行配方与
    /// Xwayland 行为差异记录见模块文档与 docs/05 §3.1（rootful Xwayland :2 全绿；
    /// rootless :1 需 compositor 激活 X 窗口）。
    #[test]
    #[ignore = "需要 X 会话且注入真实按键事件（无害键 F15）；远端实测配方见模块文档"]
    fn capture_inject_loopback_smoke() {
        let mut source = X11CaptureSource::start().expect("捕获启动");
        // 等事件线程就绪（enable_context 后服务器才开始投递；就绪握手已保证 context
        // 存在，事件流可能有亚毫秒级延迟——100ms 预热足够）。
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut injector = X11Injector::new().expect("注入器构建");
        // 焦点窗口：映射 + input focus（容错——真 Xorg 会话上通常恒成功；rootless
        // Xwayland 下 compositor 管理焦点、无 WM 激活的窗口可能 BadMatch/不投递，
        // 见 docs/05 §3.1 item 5 实测记录）。连接持有到测试结束（窗口随连接销毁）。
        let focus = focus_probe_window();
        println!(
            "焦点窗口：{}",
            match &focus {
                Ok(_) => "已建立".to_string(),
                Err(reason) => format!("建立失败（继续注入，记录）：{reason}"),
            }
        );
        let f15 = xcontrol_proto::xkey::KEY_F15;
        injector
            .inject(&InputEvent::Key {
                code: f15,
                state: KeyState::Down,
            })
            .expect("F15 down 注入");
        injector
            .inject(&InputEvent::Key {
                code: f15,
                state: KeyState::Up,
            })
            .expect("F15 up 注入");
        // 轮询窗口：XRecord 事件经服务器 → data 连接 → mpsc，800ms 富余。
        let mut saw_down = false;
        let mut saw_up = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
        while std::time::Instant::now() < deadline && !(saw_down && saw_up) {
            let mut events = Vec::new();
            source.poll(&mut events);
            for event in events {
                if let InputEvent::Key { code, state } = event
                    && code == f15
                {
                    match state {
                        KeyState::Down => saw_down = true,
                        KeyState::Up => saw_up = true,
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        source.stop().expect("捕获停止");
        drop(focus); // 连接断开 → 焦点窗口销毁（测试收尾）
        assert!(saw_down, "回环：应捕获 F15 Down（XTEST → XRecord）");
        assert!(saw_up, "回环：应捕获 F15 Up");
    }

    /// 测试用焦点窗口：映射一个 120×80 白底窗口并 `set_input_focus`（XRecord 的
    /// device_events 记录已投递事件——键盘需要确定的投递目标客户端）。连接随返回值
    /// 存活（X 客户端断开时其窗口被服务器销毁——调用方须持有返回的连接到测试结束）。
    /// 错误容错返回（rootless Xwayland 下 WM 未激活窗口可能 BadMatch，docs/05 §3.1）。
    fn focus_probe_window() -> Result<(RustConnection, Window), String> {
        let conn = connect().map_err(|e| e.to_string())?;
        let screen = setup_screen(&conn).map_err(|e| e.to_string())?;
        let (root, depth, visual, white) = (
            screen.root,
            screen.root_depth,
            screen.root_visual,
            screen.white_pixel,
        );
        let wid = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            depth,
            wid,
            root,
            -10,
            -10,
            120,
            80,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            visual,
            &xproto::CreateWindowAux::default().background_pixel(white),
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
        conn.map_window(wid)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;
        // map 处理完再设焦点（BadMatch = 窗口尚不可视；100ms 富余）。
        std::thread::sleep(std::time::Duration::from_millis(100));
        conn.set_input_focus(xproto::InputFocus::PARENT, wid, x11rb::CURRENT_TIME)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;
        let _ = conn.flush();
        Ok((conn, wid))
    }

    /// 回环诊断探针（#[ignore]；移交实测工具——三路注入对照，定位 Xwayland/Xorg 行为
    /// 差异：A) 无焦点注入；B) xdotool（独立实现的 XTEST）交叉验证注入面；C) 自建
    /// X 窗口 + set_input_focus 后注入（焦点假设验证——rootless Xwayland 的键盘路由经
    /// compositor，焦点在 Wayland 原生窗口时 X 侧可能收不到事件，docs/05 §3.1 item 5）。
    /// 全部事件原样打印，不做断言。
    ///
    /// 实测注记（2026-11-18 check 复跑，rootful :2）：A 相捕获 2 对 Down/Up（无明确
    /// 焦点态下事件投递倍数待查，WM-less 载体伪影嫌疑）；B 相 xdotool `key F15` 退出
    /// 码 0 但捕获 0 事件（载体 keymap 可能无 F15 符号绑定）——**xdotool 交叉验证的
    /// 结论只在 :1 环境**（docs/05 §3.1 ⚠️ 项），:2 载体的 B 相数据不作判定依据；
    /// C 相（显式焦点）恰 1 对，回环确定性以 `capture_inject_loopback_smoke` 为准。
    #[test]
    #[ignore = "回环诊断工具：需要 X 会话，事件打印仅人工分析"]
    fn capture_loopback_probe() {
        let mut source = X11CaptureSource::start().expect("捕获启动");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut injector = X11Injector::new().expect("注入器构建");
        let f15 = xcontrol_proto::xkey::KEY_F15;
        let seen = |phase: &str, source: &mut X11CaptureSource| {
            let mut events = Vec::new();
            source.poll(&mut events);
            for event in &events {
                println!("[{phase}] 收到 {event:?}");
            }
            events.len()
        };
        // A) 本模块 XTEST 注入（前台焦点不做假设）。
        injector
            .inject(&InputEvent::Key {
                code: f15,
                state: KeyState::Down,
            })
            .expect("A: F15 down");
        injector
            .inject(&InputEvent::Key {
                code: f15,
                state: KeyState::Up,
            })
            .expect("A: F15 up");
        std::thread::sleep(std::time::Duration::from_millis(300));
        println!(
            "A) 本模块 XTEST 注入后捕获 {} 个事件",
            seen("A", &mut source)
        );

        // B) xdotool 交叉验证（独立 XTEST 实现，排除本模块注入路径问题）。
        let xdotool = std::process::Command::new("xdotool")
            .args(["key", "--clearmodifiers", "F15"])
            .status();
        println!("B) xdotool key F15：{xdotool:?}");
        std::thread::sleep(std::time::Duration::from_millis(300));
        println!("B) xdotool 注入后捕获 {} 个事件", seen("B", &mut source));

        // C) 自建 X 窗口 + input focus 后注入（焦点假设验证）。
        let focus = focus_probe_window();
        println!(
            "C) 自建窗口+input focus：{}",
            match &focus {
                Ok(_) => "成功".to_string(),
                Err(reason) => format!("失败：{reason}"),
            }
        );
        injector
            .inject(&InputEvent::Key {
                code: f15,
                state: KeyState::Down,
            })
            .expect("C: F15 down");
        injector
            .inject(&InputEvent::Key {
                code: f15,
                state: KeyState::Up,
            })
            .expect("C: F15 up");
        std::thread::sleep(std::time::Duration::from_millis(300));
        println!("C) 窗口焦点注入后捕获 {} 个事件", seen("C", &mut source));
        drop(focus); // 连接断开 → 焦点窗口销毁（测试收尾）
        source.stop().expect("捕获停止");
    }

    /// 二次启停（连接与 context 生命周期幂等：stop 后资源干净，新 start 不受残留影响；
    /// 立即 stop 压测 enable/disable 时序竞态——就绪握手保证 StartOfData 先于任何
    /// disable，见 run_record_thread 时序注释）。
    #[test]
    fn capture_restart_after_stop_is_clean() {
        if !display_available() {
            println!("无 $DISPLAY：跳过");
            return;
        }
        let first = X11CaptureSource::start().ok();
        if let Some(source) = first {
            source.stop().expect("第一次停止应成功");
        }
        let second = X11CaptureSource::start().ok();
        if let Some(source) = second {
            source.stop().expect("第二次停止应成功");
        }
    }

    /// 捕获按键转换的纯逻辑对拍（parse_device_event 查表层独立复算；不依赖真实事件）。
    #[test]
    fn keyboard_translation_matches_x11keymap_anchors() {
        // keycode 38（xmodmap 实测 a）→ KEY_A；keycode 111（<UP>）→ KEY_UP。
        assert_eq!(x11keymap::keycode_to_xkey(38), Some(KEY_A));
        assert_eq!(x11keymap::keycode_to_xkey(111), Some(KEY_UP));
        assert_eq!(x11keymap::keycode_to_xkey(65), Some(KEY_SPACE));
        // 反查：KEY_SPACE → 65（<SPCE> 运行时锚点）
        assert_eq!(x11keymap::xkey_to_keycode(KEY_SPACE), Some(65));
        // 媒体键经消费者页语义（xmodmap 实测 <VOL+> = 123）
        assert_eq!(x11keymap::keycode_to_xkey(123), Some(CK_VOLUME_UP));
    }

    // —— 热键 grab（M4-hotkeys） ——

    /// 家族位 → X 掩码：逐家族 + Meta→M4；raw 锚点与 LOCK_VARIANT_BITS/
    /// LOCK_BITS 的 ModMask 常量同值断言（raw 值表不漂移）。
    #[test]
    fn families_to_x_mask_anchors() {
        assert_eq!(u16::from(families_to_x_mask(ModFamilies::CTRL)), 0x04);
        assert_eq!(u16::from(families_to_x_mask(ModFamilies::ALT)), 0x08);
        assert_eq!(u16::from(families_to_x_mask(ModFamilies::SHIFT)), 0x01);
        assert_eq!(u16::from(families_to_x_mask(ModFamilies::META)), 0x40);
        let ctrl_alt = families_to_x_mask(ModFamilies::CTRL.union(ModFamilies::ALT));
        assert_eq!(
            u16::from(ctrl_alt),
            0x0C,
            "默认热键 Ctrl+Alt = Control|Mod1"
        );
        // raw 值表 ↔ ModMask 常量同值（LOCK_VARIANT_BITS 注释锚点强制）
        assert_eq!(u16::from(ModMask::LOCK), 0x02);
        assert_eq!(u16::from(ModMask::M2), 0x10);
        assert_eq!(u16::from(ModMask::M5), 0x80);
        assert_eq!(LOCK_BITS, 0x02 | 0x10 | 0x80);
        assert_eq!(LOCK_VARIANT_BITS.len(), 8);
    }

    /// KeyPress.state 剥离锁定位的查表逻辑对拍（纯函数域：u16 掩码运算与
    /// grab 查表同构，不依赖 X 连接）。
    #[test]
    fn state_lock_stripping_lookup() {
        // 注册：Ctrl+Alt+Right → Next（base 0x0C；keycode 114 = <RIGHT> evdev 锚点）
        let base: u16 = 0x0C;
        let grabs = [(
            114u8,
            HotkeyGrab {
                base_mask: base,
                id: HotkeyId::Next,
            },
        )];
        // 正常命中（state = Ctrl|Alt = 0x0C）
        let state: u16 = 0x0C;
        let hit = grabs
            .iter()
            .find(|(kc, g)| *kc == 114 && g.base_mask == state & !LOCK_BITS);
        assert_eq!(hit.map(|(_, g)| g.id), Some(HotkeyId::Next));
        // NumLock 按下（state = 0x0C | 0x10）：剥离后命中（变体抓取的语义）
        let with_numlock: u16 = 0x0C | 0x10;
        assert_eq!(with_numlock & !LOCK_BITS, base);
        // 多余修饰键 Shift（0x0C | 0x01）：精确语义下不命中
        let with_shift: u16 = 0x0C | 0x01;
        let miss = grabs
            .iter()
            .find(|(kc, g)| *kc == 114 && g.base_mask == with_shift & !LOCK_BITS);
        assert!(miss.is_none(), "多余修饰键不命中（精确集合）");
    }

    /// grab 注册/注销往返（$DISPLAY 可用时）：start → stop 幂等（默认套件
    /// 可跑；无 DISPLAY 容错跳过）。
    ///
    /// 热键 grab 冒烟互斥（[`HOTKEY_SMOKE_LOCK`]）：两个热键 grab 测试争抢
    /// **同一组合键**的被动 grab（XGrabKey 同键同掩码跨客户端 = AlreadyGrabbed），
    /// 且 XTEST 注入的修饰键/主键状态是服务器全局的——并发运行会互扰（实测：
    /// 忽略套件并发跑时两测试轮流挂）。与 mac 剪贴板冒烟的 Mutex 串行化同纪律
    /// （platform-code.md）；默认套件内本测试无并发 grab 面（其余测试不 grab
    /// 组合键），串行锁无影响。
    #[test]
    fn hotkey_register_stop_roundtrip() {
        let _guard = HOTKEY_SMOKE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !display_available() {
            println!("无 $DISPLAY：跳过");
            return;
        }
        let set = HotkeySet::from_config(&xcontrol_config::HotkeysConfig::default())
            .expect("默认热键必解析");
        let source = X11HotkeySource::register(&set).expect("注册成功");
        source.stop().expect("停止成功");
        // 二次注册（窗口 id 段复用——停机双端 sync 纪律的回归防线）
        let second = X11HotkeySource::register(&set).expect("二次注册成功");
        second.stop().expect("二次停止成功");
    }

    /// grab 回环冒烟（#[ignore]，远端实测配方，docs/05 §3.1 同款纪律）：注册
    /// 默认热键 → XTEST 合成 Ctrl+Alt+Right（XTEST 事件被视为真实输入，服务
    /// 器修饰键状态随行）→ grab 触发 KeyPress → drain 见 HotkeyId::Next。
    ///
    /// 运行：`cargo test -p xcontrol-input --features x11 -- --ignored --nocapture`
    /// 载体：rootful Xwayland :2（M3 回环先例；rootless Xwayland 的 compositor
    /// 路由限制见模块文档"Xwayland 已知限制"）。
    #[test]
    #[ignore = "grab 回环冒烟（需要 X server）：cargo test -p xcontrol-input --features x11 -- --ignored --nocapture"]
    fn hotkey_grab_loopback_smoke() {
        let _guard = HOTKEY_SMOKE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !display_available() {
            println!("无 $DISPLAY：跳过");
            return;
        }
        let set = HotkeySet::from_config(&xcontrol_config::HotkeysConfig::default())
            .expect("默认热键必解析");
        let mut source = X11HotkeySource::register(&set).expect("注册成功");
        let mut injector = X11Injector::new().expect("注入器就绪");
        // 合成 Ctrl+Alt+Right 全序列（按下 → 抬起；XTEST 按物理键位注入，
        // keycode = evdev+8）
        for (code, state) in [
            (KEY_LEFT_CTRL, KeyState::Down),
            (KEY_LEFT_ALT, KeyState::Down),
            (KEY_RIGHT, KeyState::Down),
        ] {
            injector
                .inject(&InputEvent::Key { code, state })
                .expect("合成按下成功");
        }
        for (code, state) in [
            (KEY_RIGHT, KeyState::Up),
            (KEY_LEFT_ALT, KeyState::Up),
            (KEY_LEFT_CTRL, KeyState::Up),
        ] {
            injector
                .inject(&InputEvent::Key { code, state })
                .expect("合成抬起成功");
        }
        // 轮询等待 grab 事件（事件线程异步投递；2s 超时防线）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut hits = Vec::new();
        while std::time::Instant::now() < deadline && hits.is_empty() {
            source.drain(&mut hits);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        println!("grab 回环命中：{hits:?}");
        assert_eq!(hits, vec![HotkeyId::Next], "XTEST 合成组合应触发 grab");
        source.stop().expect("停止成功");
    }

    /// 按住不重复回归（docs/03 §4.6「防按住连环切换」；Xwayland :2 实测教训：
    /// XKB autorepeat 以同键**同时间戳** Release+Press 对交付重复——1.5s 实测
    /// 21 对，修复前按住热键会连环路由）。主键 Down 按住 1.5s（> 默认
    /// autorepeat delay）→ Up → drain 断言**恰好一次命中**——无论服务器是否
    /// autorepeat 都成立（不重复的服务器天然只产一次；重复的服务器被
    /// held + 同戳对判定吞）。
    #[test]
    #[ignore = "按住不重复回归（需要 X server）：cargo test -p xcontrol-input --features x11 -- --ignored --nocapture"]
    fn hotkey_grab_hold_no_repeat() {
        let _guard = HOTKEY_SMOKE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !display_available() {
            println!("无 $DISPLAY：跳过");
            return;
        }
        let set = HotkeySet::from_config(&xcontrol_config::HotkeysConfig::default())
            .expect("默认热键必解析");
        let mut source = X11HotkeySource::register(&set).expect("注册成功");
        let mut injector = X11Injector::new().expect("注入器就绪");
        injector
            .inject(&InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Down,
            })
            .expect("Ctrl 合成按下成功");
        injector
            .inject(&InputEvent::Key {
                code: KEY_LEFT_ALT,
                state: KeyState::Down,
            })
            .expect("Alt 合成按下成功");
        injector
            .inject(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Down,
            })
            .expect("主键合成按下成功");
        // 按住 1.5s（XKB autorepeat delay 默认 660ms——重复交付窗口必被覆盖）
        std::thread::sleep(std::time::Duration::from_millis(1500));
        injector
            .inject(&InputEvent::Key {
                code: KEY_RIGHT,
                state: KeyState::Up,
            })
            .expect("主键合成抬起成功");
        injector
            .inject(&InputEvent::Key {
                code: KEY_LEFT_ALT,
                state: KeyState::Up,
            })
            .expect("Alt 合成抬起成功");
        injector
            .inject(&InputEvent::Key {
                code: KEY_LEFT_CTRL,
                state: KeyState::Up,
            })
            .expect("Ctrl 合成抬起成功");
        // drain 3s（覆盖重复交付 + 抬起后余波）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut hits = Vec::new();
        while std::time::Instant::now() < deadline {
            source.drain(&mut hits);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        println!("按住不重复命中：{} 次", hits.len());
        assert_eq!(
            hits,
            vec![HotkeyId::Next],
            "按住 1.5s 必须恰好路由一次（held 位集吞 autorepeat 重复）"
        );
        source.stop().expect("停止成功");
    }
}
