//! Linux Wayland 输入实现（docs/05 §4 三路径、ADR-0003）：portal 捕获（ashpd
//! InputCapture + reis）+ uinput 注入（evdev 兜底）+ RemoteDesktop portal 注入
//! 备选 + GlobalShortcuts portal 热键 + 边缘手势回退（纯逻辑）+ C-2 能力探测表。
//!
//! # 平台纪律（platform-code.md 红线）
//!
//! - **Wayland 一律运行时探测后端**：本模块零编译期路径绑定——`CapabilityTable::
//!   probe()` 是唯一的后端决策入口（portal → uinput opt-in，UI 按 docs/06 §4
//!   降级提示），禁止调用方按编译期 cfg 假设某条路径可用。
//! - **回调线程纪律**：portal DBus 调用只发生在模块内控制线程（async_io::block_on
//!   runtime 的**控制线程**（mdns-sd 自带守护线程先例；docs/01 §2 网络执行域待遇）；
//!   事件路径是**同步零 async**——reis 同步核心 API（`ei_handshake_blocking` +
//!   `Context::read/pending_event`）+ `std::sync::mpsc` 投递。控制线程与事件
//!   线程职责分离：DBus 生命周期归控制线程，EIS 字节流归工作线程。
//! - **零 unsafe**：ashpd/reis/evdev 均为纯 Rust 实现且自带 unsafe 封装，本模块
//!   无 FFI（workspace `unsafe_code = deny` 直接生效，x11.rs 先例同款）。
//!
//! # 注入方案裁定（PRD 要求 2 的裁决记录）
//!
//! 1. **主路径 = uinput（evdev 0.13）**：`/dev/uinput` 节点建虚拟键鼠，compositor
//!    视为真实硬件（docs/05 §4.3 docs 指定 crate）。PRD 提到的 "rei crate" 经
//!    docs.rs/crates.io 查证**不存在对应 uinput 能力**（docs/01 §6 基线行的
//!    "rei" 系 "reis" 之误——reis 是 libei 协议实现，需 portal 会话才能用，不是
//!    uinput 封装），故兜底路径按 docs/05 §4.3 取 `evdev`。XKey→evdev 码翻译
//!    复用 `crate::x11keymap::EVDEV_TABLE`（同源全表，keycode = evdev + 8），
//!    不建第二张表。**限制**：uinput 无 Unicode 直发（`Text` → `Unsupported`，
//!    docs/05 §0 能力矩阵 Wayland(uinput) 行）。
//! 2. **备选 = RemoteDesktop portal（ashpd + reis Sender）**：交互授权一次 +
//!    restore token 免重复（docs/05 §6）；EIS Text 接口可 Unicode 直发。授权
//!    对话框需用户在场——实现 + 探测级测试 + 移交（见模块尾部测试）。
//!
//! # 热键方案（docs/05 §5 优先级 1/3 的 Wayland 落点）
//!
//! - **GlobalShortcuts portal**（优先级 1）：`bind_shortcuts` 注册四热键，
//!   trigger 串沿用 config 的 `Ctrl+Alt+Right` 便携格式（与
//!   `xcontrol_config::DEFAULT_HOTKEY_*` 同形；KDE 按 `QKeySequence::PortableText`
//!   解析——GNOME 的 gtk_accelerator_parse 格式差异为已知兼容项，见移交清单）。
//! - **边缘手势回退**（优先级 3，docs/06 §4 设置项"Wayland 回退手势开关"）：
//!   [`GestureSensor`] 纯
//!   逻辑——边缘双击（500ms 时间窗）+ 角落悬停（700ms 位置窗），时间由调用方
//!   注入（`feed(elapsed, point)`），零 I/O 可全量单测（edge.rs 思路：判定与
//!   轮询解耦）。
//!
//! # 远端环境探测报告（2026-11-19，lsdark-eos，写实现前的实测依据）
//!
//! ```text
//! SSH:  ssh -i ~/.ssh/gitssh lsdark@xwork.leopon-corn.ts.net
//! 会话: KDE Plasma Wayland（kwin_wayland --version → kwin 6.7.4；plasma-desktop
//!       6.7.4-1）；内核 7.2.3-arch1-3；rustup 1.99 工具链在位（rust-toolchain.toml
//!       自动选择）
//! uinput: /dev/uinput = crw-rw----+ root:root，getfacl 实测 ACL
//!         `user:lsdark:rw-` → **本用户直接可写**（uinput 注入冒烟可实测）。
//!         注意 groups 无 input 组（ACL 来自 udev 规则附加，40-kdeconnect/
//!         90-brltty-uinput.rules 在场）——通用机器不能假设 ACL，权限检测
//!         [`uinput_status`] 按 docs/06 §4 给 `usermod -aG input`/udev 文案。
//! portals: xdg-desktop-portal 1.22.1 + xdg-desktop-portal-kde 6.7.4-2 + gtk
//!         1.15.3 在运行（pgrep 实测）；kde.portal 声明
//!         org.freedesktop.impl.portal.{InputCapture, GlobalShortcuts,
//!         RemoteDesktop, Clipboard} 四接口齐备；libei 1.6.0-1。
//! busctl 实测（org.freedesktop.portal.Desktop）：
//!         InputCapture: CreateSession/CreateSession2/ConnectToEIS/Enable/
//!         Disable/Release/SetPointerBarriers/GetZones + Disabled 信号；
//!         GlobalShortcuts: CreateSession/BindShortcuts/ListShortcuts +
//!         Activated/Deactivated 信号。
//! ```
//!
//! # 模块清单（PRD 五需求的落点）
//!
//! | 需求 | 落点 |
//! | --- | --- |
//! | 捕获 | [`PortalCaptureSource`]（InputCapture 会话 + barriers + EIS 事件转换） |
//! | 注入 | [`UinputInjector`]（主）+ [`PortalInjector`]（RemoteDesktop 备选） |
//! | 热键 | [`PortalHotkeys`]（GlobalShortcuts）+ [`GestureSensor`]（回退） |
//! | 探测 | [`uinput_status`] + [`CapabilityTable`]（C-2 三 compositor 行） |
//!
//! # 远端实测配方（本机非 Linux 时的验证路径）
//!
//! ```bash
//! KEY=~/.ssh/gitssh; H=lsdark@xwork.leopon-corn.ts.net
//! R=/media/lsdark/gns/sources/com.aixhc/XControl
//! rsync -a --exclude target --exclude .git --exclude .trellis ./ $H:$R/
//! # 默认套件（纯逻辑 + 权限探测，无交互）：
//! ssh -i $KEY $H "export PATH=\$HOME/.cargo/bin:\$PATH; cd $R && cargo test \
//!   -p xcontrol-input --features wayland"
//! # uinput 注入冒烟（ACL 允许时实测；写 F15 键事件）：
//! ssh -i $KEY $H "cd $R && cargo test -p xcontrol-input --features wayland \
//!   -- --ignored --nocapture"
//! # 交互授权项（对话框需用户在场；无人值守时超时返回 PortalInitError::Timeout
//! # = 移交证据）。XDG_CURRENT_DESKTOP 必须显式给（SSH 非交互 shell 不继承桌面
//! # env，compositor 识别退化为 Unknown）；三交互项并行会互相挤占 portal 对话框
//! # 通道——串行跑（--test-threads=1）：
//! ssh -i $KEY $H "export PATH=\$HOME/.cargo/bin:\$PATH XDG_CURRENT_DESKTOP=KDE \
//!   WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000; cd $R && \
//!   cargo test -p xcontrol-input --features wayland -- --ignored --nocapture \
//!   --test-threads=1"
//! ```
//!
//! # 远端实测结论（2026-11-19，lsdark-eos 原生运行；docs/05 §4.5 同录）
//!
//! - **默认套件 70 项全绿**（纯逻辑 + 权限探测；`cargo test -p xcontrol-input
//!   --features wayland`）。
//! - **uinput 注入冒烟 PASS**：`/dev/uinput`（ACL 授权）创建虚拟键鼠 + F15
//!   down/up + 相对移动 + 滚轮 + release_all 全链路无错；`Text` 干净拒绝
//!   （`Unsupported`）。
//! - **C-2 能力探测表实测**：KDE 行全 `Available`——InputCapture portal **v2**
//!   （caps: Keyboard|Pointer|Touchscreen）、RemoteDesktop portal **v2**、
//!   GlobalShortcuts portal **v2**、Clipboard portal **v1**、uinput 可写。
//! - **GlobalShortcuts 注册 = 应用身份硬门槛（实测发现）**：无人值守绑定被
//!   portal 拒绝——`org.freedesktop.portal.Error.NotAllowed: An app id is
//!   required`。KDE 的 GlobalShortcuts 要求调用方有桌面身份（进程名匹配的
//!   `.desktop` 文件；打包/安装路径交付，docs/10 安装面）——非代码缺陷，
//!   属部署前置项；授权对话框流程（用户在场）随桌面身份安装后一并移交。
//! - **InputCapture 会话 = 授权对话框交互**：无人值守 create_session 3s 超时
//!   （`authorize` 超时即对话框等待的取证）——授权后全链路（激活→EIS 事件
//!   →release 回位）移交用户在场实测。
//! - **RemoteDesktop 会话 = 授权对话框交互（check 复跑修订）**：首测曾记录
//!   "create_session → select_devices → start → connect_to_eis 无人值守 3s 内
//!   全通"，check 复跑两次（并行/串行）均卡授权 3s 超时——按超时取证口径归
//!   移交项（不阻断结论不可复现，或系首测时用户在场点击）；授权后 fd 交 EIS
//!   sender 线程（reis 握手 + 设备协商）与实际注入落屏验证移交用户在场。
//!   **无人值守三 portal 项的统一形态**：`PortalInitError::Timeout`（复跑确认，
//!   契约类型）。

use std::collections::BTreeSet;
use std::future::Future;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use wayland_client::{Connection, Proxy as _, WEnum};
use xcontrol_config::HotkeysConfig;
use xcontrol_core::hotkey::{HotkeyId, HotkeySet};
use xcontrol_core::sink::{InputSink, SinkError};
use xcontrol_proto::input::{Axis, InputEvent, KeyState, Mods, MouseButton, ScreenEdge, ScreenId};
use xcontrol_proto::xkey::XKey;

use crate::InputError;
use crate::x11keymap::{keycode_to_xkey, xkey_to_keycode};

// ============================================================
// §1 权限探测（docs/05 §6 "wayland 兜底 uinput" 行 / docs/06 §4 引导语义）
// ============================================================

/// `/dev/uinput` 节点路径。
const UINPUT_NODE: &str = "/dev/uinput";

/// uinput 权限状态（docs/06 §4：权限缺失 → 可点击的修复指引文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UinputStatus {
    /// `/dev/uinput` 可写——uinput 注入器可创建。
    Available,
    /// 节点不存在（内核无 CONFIG_UINPUT / uinput 模块未加载）。
    ///
    /// 引导：`sudo modprobe uinput` + 开机自动加载（/etc/modules-load.d/uinput.conf）。
    NodeMissing,
    /// 节点存在但无写权限（未入 `input` 组且无 udev/ACL 授权）。
    ///
    /// 引导文案（[`UinputStatus::guidance`]）：`usermod -aG input` + 重新登录，
    /// 或 udev 规则（docs/05 §4.3 安装器职责）。
    Denied,
}

impl UinputStatus {
    /// 是否可用（探测表与 UI 降级判断用）。
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// 修复指引文案（docs/06 §4 "权限缺失显示可点击的修复指引"；不含安全码）。
    #[must_use]
    pub fn guidance(&self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::NodeMissing => Some(
                "未找到 /dev/uinput：执行 `sudo modprobe uinput`，并创建 \
                 /etc/modules-load.d/uinput.conf（内容 `uinput`）开机自动加载",
            ),
            Self::Denied => Some(
                "无 /dev/uinput 写权限：`sudo usermod -aG input $USER` 后重新登录，\
                 或按安装文档添加 udev 规则（SUBSYSTEM==\"uinput\", MODE=\"0660\", \
                 GROUP=\"input\"）",
            ),
        }
    }
}

/// uinput 权限探测（非破坏性：以只写打开 `/dev/uinput` 后立即关闭——与真实
/// 创建设备所需的同一权限面）。
///
/// 开节点失败的 `PermissionDenied` → [`UinputStatus::Denied`]；`NotFound` →
/// [`UinputStatus::NodeMissing`]；其余 I/O 错误按 Denied 处理（错误文案保守：
/// 权限面问题按不可用引导，不中断）。
#[must_use]
pub fn uinput_status() -> UinputStatus {
    match std::fs::OpenOptions::new().write(true).open(UINPUT_NODE) {
        // SAFETY 无涉：文件句柄随 Drop 关闭（RAII）。
        Ok(_file) => UinputStatus::Available,
        Err(error) if error.kind() == ErrorKind::NotFound => UinputStatus::NodeMissing,
        Err(_) => UinputStatus::Denied,
    }
}

/// [`uinput_status`] 的布尔便捷封装。
#[must_use]
pub fn uinput_available() -> bool {
    uinput_status().is_available()
}

// ============================================================
// §2 uinput 注入器（docs/05 §4.3 兜底；InputSink 实现）
// ============================================================

/// 协议鼠标键 → evdev BTN 码（uinput 与 EIS Button 事件共用；Linux
/// input-event-codes.h：BTN_LEFT=0x110 … BTN_EXTRA=0x114）。
#[must_use]
pub const fn mouse_button_to_evdev(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0x110,    // BTN_LEFT
        MouseButton::Middle => 0x112,  // BTN_MIDDLE
        MouseButton::Right => 0x111,   // BTN_RIGHT
        MouseButton::Back => 0x113,    // BTN_SIDE
        MouseButton::Forward => 0x114, // BTN_EXTRA
    }
}

/// evdev BTN 码 → 协议鼠标键（捕获/EIS 事件方向的反查；未知码 → `None`）。
#[must_use]
pub const fn evdev_to_mouse_button(code: u32) -> Option<MouseButton> {
    match code {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
        0x113 => Some(MouseButton::Back),
        0x114 => Some(MouseButton::Forward),
        _ => None,
    }
}

/// XKey → evdev 键码（复用 `x11keymap` 全表：X keycode = evdev + 8）。
///
/// 全量无例外（docs/03 §2：内核 hid-input.c 逐项核对表）；不在表内的键
/// （`None`）由调用方报 `Unsupported`。
#[must_use]
pub fn xkey_to_evdev(key: XKey) -> Option<u16> {
    xkey_to_keycode(key).map(|keycode| u16::from(keycode - crate::x11keymap::MIN_KEYCODE))
}

/// evdev 键码 → XKey（捕获方向反查）。
#[must_use]
pub fn evdev_to_xkey(code: u32) -> Option<XKey> {
    let code = u8::try_from(code).ok()?;
    keycode_to_xkey(code.saturating_add(crate::x11keymap::MIN_KEYCODE))
}

/// uinput 虚拟设备注入器（[`InputSink`]）。
///
/// - 键盘：XKey 全表（[`xkey_to_evdev`]）；
/// - 指针：相对移动（REL_X/REL_Y）+ 滚轮（REL_WHEEL/REL_HWHEEL）+ 五键
///   （BTN_*）+ 绝对定位（ABS_X/ABS_Y，[`UinputInjector::warp`] 用——uinput
///   的绝对轴覆盖构造时声明的单屏几何）；
/// - `release_all`：重放已按下集合的 Release（切出/暂停防卡键，docs/03 §3）；
///   按下集合在本对象内跟踪（uinput 设备无内核侧可查询状态）。
///
/// # 已知限制（文档化）
///
/// - 单屏几何：绝对轴按构造时 `screen_size` 标定（KDE 单 zone 会话即整桌面）；
///   多屏 union 几何协商（M5 布局收口）后由调用方传 union 尺寸。
/// - `Text` 无注入路径（docs/05 §0 能力矩阵 Wayland(uinput) 行 CAP_UNICODE ❌）
///   → `SinkError::Unsupported`（上层按能力协商上报，协议不静默失败）。
pub struct UinputInjector {
    device: evdev::uinput::VirtualDevice,
    /// 虚拟光标位置（ABS 轴坐标，单位 = ABS 声明域；warp/采样回报的定位
    /// 与判定语义源）。11-22 受控**移动**注入改回 REL（kwin 边缘/角落判定
    /// 对 ABS 注入路径不响应——REL 路径实验确认响应，物理鼠标同源）；加速
    /// 由 libinput quirks 对本设备单独关闭（flat，手感 = ABS 直通等价），
    /// 原 11-20 的"REL 跳数百像素"前提（compositor 加速）随 quirks 消除。
    /// `warp`/`MouseMoveAbs` 仍走 ABS 绝对定位（落点语义不经加速曲线）。
    virt_x: i32,
    virt_y: i32,
    /// 受控移动 REL 小数格累积器（[水平, 垂直]；uinput REL 轴只收整数格，
    /// 协议 delta 可含小数——余量滚存防丢步，语义同 wheel_accum）。
    move_accum: [f32; 2],
    screen_w: f64,
    screen_h: f64,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u16>,
    /// 滚轮小数格累积器（[水平, 垂直]；docs/02 wheel delta 单位 = 格，可含
    /// 小数——uinput REL_WHEEL 只收整数格，余量滚存）。
    wheel_accum: [f32; 2],
}

impl UinputInjector {
    /// 创建虚拟键鼠设备（需要 `/dev/uinput` 可写——先探 [`uinput_status`]）。
    ///
    /// # Errors
    /// [`SinkError::Failed`]：设备创建（open/uinput ioctl）失败——权限面问题
    /// 建议先调 [`uinput_status`] 走引导。
    pub fn new(screen_size: (u32, u32)) -> Result<Self, SinkError> {
        let mut keys = evdev::AttributeSet::<evdev::KeyCode>::new();
        for &(evdev_code, xkey) in crate::x11keymap::EVDEV_TABLE {
            // 表内 (evdev, XKey) 对已核对（x11keymap 模块测试）；逐项注册键位。
            let _ = xkey;
            keys.insert(evdev::KeyCode::new(u16::from(evdev_code)));
        }
        // 鼠标键 BTN_*（EV_KEY 域 0x110-0x114）：uinput 只能注入**已声明**的
        // 码位（input_inject_event 按设备 keybit 过滤未声明码——静默丢弃或写
        // 入报错，内核版本而异），EVDEV_TABLE 不含 BTN 码（那是纯键盘表），
        // 必须在此单独声明（11-19-m5-wayland check 补修；冒烟含鼠标键注入回归）。
        for button in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::Back,
            MouseButton::Forward,
        ] {
            keys.insert(evdev::KeyCode::new(mouse_button_to_evdev(button)));
        }
        let mut rel = evdev::AttributeSet::<evdev::RelativeAxisCode>::new();
        rel.insert(evdev::RelativeAxisCode::REL_X);
        rel.insert(evdev::RelativeAxisCode::REL_Y);
        rel.insert(evdev::RelativeAxisCode::REL_WHEEL);
        rel.insert(evdev::RelativeAxisCode::REL_HWHEEL);
        let (w, h) = (
            i32::try_from(screen_size.0).unwrap_or(i32::MAX).max(1),
            i32::try_from(screen_size.1).unwrap_or(i32::MAX).max(1),
        );
        let device = evdev::uinput::VirtualDevice::builder()
            .and_then(|builder| {
                let name = "xcontrol uinput bridge";
                let builder = builder
                    .name(name)
                    .with_keys(&keys)?
                    .with_relative_axes(&rel)?;
                // 绝对轴（warp + 受控 ABS 注入）：resolution 必填（11-20 实测
                // 修复——resolution=0 时 libinput 把设备分类为触屏类，compositor
                // 不喂触发角/边缘手势管线；1000 units/mm 是 libinput 对指针设备
                // 的标准分辨率惯例，让 KDE 把虚拟设备当"真实鼠标"对待）。
                // 1000 units/mm → 屏幕 mm 数 ≈ 逻辑分辨率/4（96dpi 惯例）。
                let res_x = (w / 4).max(1);
                let res_y = (h / 4).max(1);
                let builder = builder.with_absolute_axis(&evdev::UinputAbsSetup::new(
                    evdev::AbsoluteAxisCode::ABS_X,
                    evdev::AbsInfo::new(0, 0, w, 0, 0, res_x),
                ))?;
                builder.with_absolute_axis(&evdev::UinputAbsSetup::new(
                    evdev::AbsoluteAxisCode::ABS_Y,
                    evdev::AbsInfo::new(0, 0, h, 0, 0, res_y),
                ))
            })
            .and_then(evdev::uinput::VirtualDeviceBuilder::build)
            .map_err(|error| SinkError::Failed {
                action: "inject",
                reason: format!("uinput 设备创建失败：{error}"),
            })?;
        Ok(Self {
            device,
            virt_x: w / 2,
            virt_y: h / 2,
            screen_w: f64::from(w),
            screen_h: f64::from(h),
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            wheel_accum: [0.0; 2],
            move_accum: [0.0; 2],
        })
    }

    /// ABS 轴声明域上界（与构造 `AbsInfo` max 一致；虚拟光标 clamp 用）。
    fn abs_max(&self) -> (i32, i32) {
        (
            Self::f64_to_i32_floor(self.screen_w) - 1,
            Self::f64_to_i32_floor(self.screen_h) - 1,
        )
    }

    /// f64 → i32 下取整（负值安全；NaN → 0。ABS 轴 clamp 域计算用）。
    fn f64_to_i32_floor(v: f64) -> i32 {
        if v.is_finite() { v.floor() as i32 } else { 0 }
    }

    /// 单条 evdev 事件 + SYN（[`evdev::VirtualDevice::emit`] 自动补 SYN_REPORT）。
    fn emit(&mut self, kind: u16, code: u16, value: i32) -> Result<(), SinkError> {
        let event = evdev::InputEvent::new(kind, code, value);
        self.device
            .emit(&[event])
            .map_err(|error| SinkError::Failed {
                action: "inject",
                reason: format!("uinput 写入失败：{error}"),
            })
    }

    /// REL 相对移动（11-22 KDE 触发角机制实验专用；生产移动注入走 ABS——
    /// docs/05 §2）。实验判据：REL 路径推到角落若 kwin 触发角响应 = 边缘
    /// 判定对 ABS 注入路径不响应（修法定向：移动注入改 REL/混合设备）。
    ///
    /// # Errors
    /// [`SinkError::Failed`]：uinput 写入失败。
    pub fn probe_rel(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
        self.emit(
            evdev::EventType::RELATIVE.0,
            evdev::RelativeAxisCode::REL_X.0,
            dx,
        )?;
        self.emit(
            evdev::EventType::RELATIVE.0,
            evdev::RelativeAxisCode::REL_Y.0,
            dy,
        )
    }

    /// 自动标定构造（11-22 边缘回切修复轮）：wl_output 枚举真实分辨率作 ABS
    /// 域上界（[`probe_screen_pixels`]），不可用时回退 1920×1080 兜底标定。
    ///
    /// 背景：此前 `build_platform_sink` 硬编码 (1920,1080)，实机 4K 屏
    /// （3840×2160）下 ABS 域仅为真实像素一半——kwin 域缩放映射虽仍可达缘，
    /// 但 1 delta = 2px 粒度、边缘驻留换算不精确。本构造在会话装配点自查。
    ///
    /// # Errors
    /// 同 [`UinputInjector::new`]。
    pub fn auto() -> Result<Self, SinkError> {
        Self::with_calibration(calibration_size(probe_screen_pixels()))
    }

    /// 显式标定尺寸构造（[`Self::auto`] 的探测注入面；测试不依赖运行环境）。
    fn with_calibration(size: (u32, u32)) -> Result<Self, SinkError> {
        Self::new(size)
    }
}

impl InputSink for UinputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        match *event {
            InputEvent::Key { code, state } => {
                let Some(evdev_code) = xkey_to_evdev(code) else {
                    return Err(SinkError::Unsupported("键位无 evdev 翻译（uinput 路径）"));
                };
                let down = state == KeyState::Down;
                match down {
                    true => self.pressed_keys.insert(evdev_code),
                    false => self.pressed_keys.remove(&evdev_code),
                };
                self.emit(evdev::EventType::KEY.0, evdev_code, i32::from(down))
            }
            // X11/后端捕获层不发 KeyRepeat（x11.rs 已知限制同语义）；收到按
            // Key Down 处理（compositor 自行做长按重复，libinput 语义）。
            InputEvent::KeyRepeat { code } => {
                let Some(evdev_code) = xkey_to_evdev(code) else {
                    return Err(SinkError::Unsupported("键位无 evdev 翻译（uinput 路径）"));
                };
                self.pressed_keys.insert(evdev_code);
                self.emit(evdev::EventType::KEY.0, evdev_code, 1)
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                // 11-22 受控移动注入改回 REL（kwin 边缘/角落判定对 ABS 注入
                // 路径不响应——REL 路径（物理鼠标同源）实验确认响应，11-22
                // KDE 触发角排查）。加速由 libinput quirks 对本设备单独关闭
                // （flat，docs/05 §2）——原 11-20 改 ABS 的前提（REL 被加速
                // 放大跳底）随 quirks 消除。virt 累积保留：采样回报（回切判
                // 定）与 warp 的位置语义源；flat 下 virt = kwin 实际光标。
                // 小数格累积器防丢步（语义同 wheel_accum）。
                self.move_accum[0] += dx;
                self.move_accum[1] += dy;
                let step_x = self.move_accum[0].trunc() as i32;
                let step_y = self.move_accum[1].trunc() as i32;
                self.move_accum[0] -= step_x as f32;
                self.move_accum[1] -= step_y as f32;
                let (x_max, y_max) = self.abs_max();
                self.virt_x = (self.virt_x + step_x).clamp(0, x_max);
                self.virt_y = (self.virt_y + step_y).clamp(0, y_max);
                if step_x != 0 {
                    self.emit(
                        evdev::EventType::RELATIVE.0,
                        evdev::RelativeAxisCode::REL_X.0,
                        step_x,
                    )?;
                }
                if step_y != 0 {
                    self.emit(
                        evdev::EventType::RELATIVE.0,
                        evdev::RelativeAxisCode::REL_Y.0,
                        step_y,
                    )?;
                }
                Ok(())
            }
            InputEvent::MouseMoveAbs { x, y, .. } => {
                let px = (f64::from(x) * self.screen_w).round() as i32;
                let py = (f64::from(y) * self.screen_h).round() as i32;
                self.emit(
                    evdev::EventType::ABSOLUTE.0,
                    evdev::AbsoluteAxisCode::ABS_X.0,
                    px,
                )?;
                self.emit(
                    evdev::EventType::ABSOLUTE.0,
                    evdev::AbsoluteAxisCode::ABS_Y.0,
                    py,
                )
            }
            InputEvent::MouseButton { button, state } => {
                let btn = mouse_button_to_evdev(button);
                let down = state == KeyState::Down;
                match down {
                    true => self.pressed_buttons.insert(btn),
                    false => self.pressed_buttons.remove(&btn),
                };
                self.emit(evdev::EventType::KEY.0, btn, i32::from(down))
            }
            InputEvent::Wheel { axis, delta } => {
                // 符号约定：delta > 0 = 滚轮上/右（REL_WHEEL +1 = 远离用户），
                // 与 docs/02 "滚轮方向与主控一致"的 wire 语义取直通映射；小数
                // 格累积，整数部分入队（触控板细粒度滚动不丢格）。
                let slot = match axis {
                    Axis::Vertical => 1,
                    Axis::Horizontal => 0,
                };
                self.wheel_accum[slot] += delta;
                let steps = self.wheel_accum[slot].trunc();
                if steps != 0.0 {
                    self.wheel_accum[slot] -= steps;
                    let code = match axis {
                        Axis::Vertical => evdev::RelativeAxisCode::REL_WHEEL,
                        Axis::Horizontal => evdev::RelativeAxisCode::REL_HWHEEL,
                    };
                    self.emit(evdev::EventType::RELATIVE.0, code.0, steps as i32)?;
                }
                Ok(())
            }
            // Unicode 直发无路径（docs/03 §4.5；能力协商上报，不静默近似）。
            InputEvent::Text { .. } => Err(SinkError::Unsupported(
                "uinput 路径无 Unicode 直发（CAP_UNICODE ❌，docs/05 §0）",
            )),
            // 会话层事件（EnterScreen/LeaveScreen/ModifierSync/Switch*）不是
            // 注入载荷——route 层不会下发（core::route::apply_actions 只转
            // 注入类事件）；防御性拒绝并说明。
            InputEvent::EnterScreen { .. }
            | InputEvent::LeaveScreen { .. }
            | InputEvent::ModifierSync { .. }
            | InputEvent::SwitchRequest { .. }
            | InputEvent::SwitchAck { .. } => {
                Err(SinkError::Unsupported("会话控制事件不进入注入路径"))
            }
        }
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        // uinput 单设备绝对轴 = 构造时声明的单屏几何；ScreenId 非 0 按参数
        // 透传语义（多屏协商前恒 0；docs/03 §5 布局协商 M5 收口）。
        let _ = screen;
        let px = (f64::from(x) * self.screen_w).round() as i32;
        let py = (f64::from(y) * self.screen_h).round() as i32;
        // 虚拟光标同步（11-20 跳变根因修复）：warp 不同步 virt 坐标的话，
        // 后续第一条 MoveRel 的 ABS 注入会从**陈旧的 virt 位置**（构造默认 =
        // 屏幕中心）起跳——进入落点"先正确、一动就跳屏中央/底部"的元凶。
        self.virt_x = px;
        self.virt_y = py;
        self.emit(
            evdev::EventType::ABSOLUTE.0,
            evdev::AbsoluteAxisCode::ABS_X.0,
            px,
        )?;
        self.emit(
            evdev::EventType::ABSOLUTE.0,
            evdev::AbsoluteAxisCode::ABS_Y.0,
            py,
        )
    }

    fn release_all(&mut self) -> Result<(), SinkError> {
        // 虚拟光标复位回中（11-20：ABS 模式下位置随会话收尾重置）。
        let (cx, cy) = self.abs_max();
        self.virt_x = (cx + 1) / 2;
        self.virt_y = (cy + 1) / 2;
        // 先收后发：克隆快照再清集合，防止注入失败时的半释放状态歧义。
        let keys = std::mem::take(&mut self.pressed_keys);
        for code in keys {
            self.emit(evdev::EventType::KEY.0, code, 0)?;
        }
        let buttons = std::mem::take(&mut self.pressed_buttons);
        for code in buttons {
            self.emit(evdev::EventType::KEY.0, code, 0)?;
        }
        Ok(())
    }

    fn cursor_normalized(&self) -> Option<(f32, f32)> {
        // 双向边缘回切的被控采样面（11-22 修复轮；docs/03 §5）：虚拟光标 ABS
        // 域值 → 归一化。uinput 无"查询真实光标"入口——ABS 设备即域缩放映射
        // （kwin/libinput：[min,max] → [0,屏宽]），注入的 ABS 值就是光标位置
        // 的唯一事实源，`virt / max`（max = 末像素坐标）即归一化坐标；端点
        // 容差带收敛与 edge.rs EDGE_TOLERANCE_POINTS 同型（1 ABS 单位 = 1 像素）。
        let (x_max, y_max) = self.abs_max();
        if x_max <= 0 || y_max <= 0 {
            return None;
        }
        let x = abs_normalized(self.virt_x, x_max);
        let y = abs_normalized(self.virt_y, y_max);
        // 探针（验证面）：原始域值随行——回切"差一点不到缘"的实测分辨入口。
        tracing::debug!(target: "input::wayland",
            x = self.virt_x, y = self.virt_y, x_max, y_max, nx = x, ny = y,
            "受控光标采样（uinput ABS 域）");
        Some((x, y))
    }
}

// ============================================================
// §2.1 屏幕像素探测（uinput ABS 域标定；11-22 边缘回切修复轮）
// ============================================================

/// 探测失败时的兜底标定尺寸（CI 无 GUI / SSH 非交互 shell 无
/// `WAYLAND_DISPLAY` 的必然路径；warn 由 [`UinputInjector::auto`] 留痕）。
const FALLBACK_SCREEN_SIZE: (u32, u32) = (1920, 1080);

/// 探针结果 → 标定尺寸（纯函数）：`None` → 兜底 1920×1080（fallback 决策面，
/// 单测覆盖；真实构造仍需 `/dev/uinput`，故决策与设备创建分离）。
fn calibration_size(probe: Option<(u32, u32)>) -> (u32, u32) {
    probe.unwrap_or(FALLBACK_SCREEN_SIZE)
}

/// ABS 域值 → 归一化坐标（纯函数）：`virt / max`（max = 末像素坐标，与
/// [`UinputInjector::abs_max`] 同源），±1 单位容差带收敛到端点 0.0/1.0——
/// 与 edge.rs `EDGE_TOLERANCE_POINTS` 同型（1 ABS 单位 = 1 像素），主控
/// 极值判定的回报契约（边缘驻留回报恰 0.0/1.0）。
fn abs_normalized(virt: i32, max: i32) -> f32 {
    if virt <= 1 {
        0.0
    } else if virt >= max - 1 {
        1.0
    } else {
        (f64::from(virt) / f64::from(max)) as f32
    }
}

/// 单输出几何快照（wl_output `geometry` + 当前 `mode`；并集计算输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputGeometry {
    /// 输出左上角在全局桌面布局中的位置（`wl_output.geometry` 的 x/y）。
    position: (i32, i32),
    /// 当前模式的像素尺寸（`wl_output.mode` current 位；物理像素）。
    size: (u32, u32),
}

/// 多输出像素尺寸并集（纯函数，mock 数据单测面）：全部输出矩形的包围盒
/// 尺寸（多屏 = 桌面 union——uinput ABS 域覆盖整桌面语义）。零尺寸输出
/// 跳过（几何异常防御）；无有效输出/任一维度 ≤ 0 → `None`（调用方走兜底）。
fn outputs_union_pixels(outputs: &[OutputGeometry]) -> Option<(u32, u32)> {
    let mut bounds: Option<(i32, i32, i32, i32)> = None; // (min_x, min_y, max_x, max_y)
    for output in outputs {
        let (w, h) = (
            i32::try_from(output.size.0).unwrap_or(0),
            i32::try_from(output.size.1).unwrap_or(0),
        );
        if w <= 0 || h <= 0 {
            continue;
        }
        let (ox, oy) = output.position;
        let (max_x, max_y) = (ox.saturating_add(w), oy.saturating_add(h));
        bounds = Some(match bounds {
            None => (ox, oy, max_x, max_y),
            Some((bx0, by0, bx1, by1)) => {
                (bx0.min(ox), by0.min(oy), bx1.max(max_x), by1.max(max_y))
            }
        });
    }
    let (bx0, by0, bx1, by1) = bounds?;
    let (w, h) = (bx1.saturating_sub(bx0), by1.saturating_sub(by0));
    let size = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// wl_output 枚举状态（一次性查询；connection 随函数返回断开，无生命周期负担）。
#[derive(Default)]
struct OutputProbeState {
    /// 逐输出几何（key = 输出对象 id；geometry 与 mode 事件分两拍到达）。
    outputs: std::collections::HashMap<wayland_client::backend::ObjectId, ProbeOutput>,
}

/// 单输出的分片收集态（geometry 与 mode 事件先后到达后拼装）。
#[derive(Default)]
struct ProbeOutput {
    position: Option<(i32, i32)>,
    size: Option<(u32, u32)>,
}

impl wayland_client::Dispatch<wayland_client::protocol::wl_registry::WlRegistry, ()>
    for OutputProbeState
{
    fn event(
        _state: &mut Self,
        registry: &wayland_client::protocol::wl_registry::WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &wayland_client::QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "wl_output"
        {
            // 绑定 v1 面（geometry/mode 均为 v1 事件；更高版本的 scale/done
            // 本查询不需要——绑定低版本永远是合法的，compositor 按绑定版本发事件）。
            let _ = registry.bind::<wayland_client::protocol::wl_output::WlOutput, (), Self>(
                name,
                version.min(1),
                qh,
                (),
            );
        }
    }
}

impl wayland_client::Dispatch<wayland_client::protocol::wl_output::WlOutput, ()>
    for OutputProbeState
{
    fn event(
        state: &mut Self,
        output: &wayland_client::protocol::wl_output::WlOutput,
        event: wayland_client::protocol::wl_output::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event as OutputEvent;
        let entry = state.outputs.entry(output.id()).or_default();
        match event {
            OutputEvent::Geometry { x, y, .. } => {
                entry.position = Some((x, y));
            }
            // current 位过滤并入模式分支（wl_output.mode 的 flags 是位掩码，
            // 非 current 模式（备选分辨率）不参与标定）。
            OutputEvent::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wayland_client::protocol::wl_output::Mode::Current) => {
                // 协议面 width/height 为 i32（负值 = 几何异常，并集层防御跳过）。
                entry.size = Some((
                    u32::try_from(width).unwrap_or(0),
                    u32::try_from(height).unwrap_or(0),
                ));
            }
            _ => {}
        }
    }
}

/// 真实分辨率探测（wl_output 枚举；uinput ABS 域标定的事实源，11-22 修复轮）。
///
/// 连接-枚举-断开的一次性阻塞查询（构造点调用，非事件热路径）：两次
/// roundtrip 收齐 registry globals（触发输出绑定）与 geometry/mode 事件，
/// 尺寸 = 全部输出当前模式的并集包围盒。无 wayland 环境（CI / SSH 无
/// `WAYLAND_DISPLAY`）/ 连接失败 / 零有效输出 → `None`（调用方兜底
/// 1920×1080 + warn，[`UinputInjector::auto`]）。
#[must_use]
pub fn probe_screen_pixels() -> Option<(u32, u32)> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue::<OutputProbeState>();
    let qh = queue.handle();
    let mut state = OutputProbeState::default();
    // 注册表绑定（第一拍收 globals 并发出 bind 请求）。
    conn.display().get_registry(&qh, ());
    queue.roundtrip(&mut state).ok()?;
    // 输出几何（bind 已随第一拍发出，第二拍收 geometry/mode 事件）。
    queue.roundtrip(&mut state).ok()?;
    let outputs: Vec<OutputGeometry> = state
        .outputs
        .values()
        .filter_map(|output| {
            Some(OutputGeometry {
                position: output.position?,
                size: output.size?,
            })
        })
        .collect();
    let size = outputs_union_pixels(&outputs);
    if let Some((w, h)) = size {
        tracing::debug!(target: "input::wayland", outputs = outputs.len(), w, h,
            "wl_output 枚举成功（uinput ABS 域标定）");
    }
    size
}

// ============================================================
// §3 C-2 能力探测表（docs/05 §0 矩阵行 → 运行时结构化输出）
// ============================================================

/// Compositor 家族（C-2 三行；docs/05 §0 能力矩阵按 GNOME/KDE/wlroots 分列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    /// KDE Plasma（portal 路径全支持，docs/05 §4.1）。
    Kde,
    /// GNOME（portal 路径，InputCapture v1 限制见 docs/05 §8）。
    Gnome,
    /// wlroots 系（sway/Hyprland 等，docs/05 §4.2 非 portal 路径）。
    Wlroots,
    /// 未识别（探测表按 Unknown 行降级提示）。
    Unknown,
}

impl Compositor {
    /// 按会话环境变量识别（`XDG_CURRENT_DESKTOP` 冒号分隔列表）。
    #[must_use]
    pub fn detect() -> Self {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        let desktop = desktop.to_ascii_uppercase();
        if desktop.contains("KDE") {
            Self::Kde
        } else if desktop.contains("GNOME") {
            Self::Gnome
        } else if desktop.split(':').any(|token| {
            matches!(
                token,
                "SWAY" | "HYPRLAND" | "WAYFIRE" | "LABWC" | "RIVER" | "WLROOTS"
            )
        }) {
            Self::Wlroots
        } else {
            Self::Unknown
        }
    }

    /// 行序（C-2 表固定三行 + Unknown 行冗余展示）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Kde => "KDE",
            Self::Gnome => "GNOME",
            Self::Wlroots => "wlroots",
            Self::Unknown => "unknown",
        }
    }
}

/// 单项能力状态（诚实三态：实测可用 / 实测不可用 / 未实测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapState {
    /// 三态。
    pub status: CapStatus,
    /// 判定依据（探测错误摘要或文档行引用；不含敏感信息）。
    pub detail: String,
}

/// 能力三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapStatus {
    /// 探测确认可用。
    Available,
    /// 探测确认不可用（detail 说明原因）。
    Unavailable,
    /// 本机未实测（detail 引用 docs/05 §0/§4 文档预期，UI 提示"未实测"）。
    Untested,
}

impl CapState {
    const AVAILABLE: Self = Self {
        status: CapStatus::Available,
        detail: String::new(),
    };

    fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            status: CapStatus::Unavailable,
            detail: detail.into(),
        }
    }

    /// 是否可用（严格三态语义：Untested 不算可用）。
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self.status, CapStatus::Available)
    }
}

/// 单 compositor 行（capture/inject/hotkey/clipboard 四列，docs/05 §0 行拆解）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositorCaps {
    /// 行标识。
    pub compositor: Compositor,
    /// 捕获：InputCapture portal 可用性（wlroots 行为 layer-shell 路径，portal
    /// 探测不可代表——M5 wlroots 实现为移交项，行恒 Untested）。
    pub capture: CapState,
    /// 注入（uinput 兜底）：`/dev/uinput` 写权限。
    pub inject_uinput: CapState,
    /// 注入（portal 备选）：RemoteDesktop portal 接口在场。
    pub inject_portal: CapState,
    /// 热键：GlobalShortcuts portal 接口在场（实际绑定还需授权，探测不含）。
    pub hotkey_portal: CapState,
    /// 剪贴板：Clipboard portal 接口在场（data-control 探测在 xcontrol-clipboard
    /// 的 wayland 后端，docs/05 §4.4——两探测合并成完整行）。
    pub clipboard_portal: CapState,
}

/// C-2 支持矩阵（KDE/GNOME/wlroots 三行；docs/05 §0）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityTable {
    /// 三行（顺序 = KDE、GNOME、wlroots）。
    pub rows: [CompositorCaps; 3],
}

impl CapabilityTable {
    /// 本机实测行填充：portal 代理探测 + uinput 权限探测。
    ///
    /// 探测是**非交互**的：代理创建 + version/属性读取不触发授权对话框
    /// （对话框只在 CreateSession/BindShortcuts 等会话级调用出现）；接口缺席
    /// 时 zbus 返回错误 → `Unavailable` + 错误摘要。
    ///
    /// # Errors
    /// 运行时（DBUS session bus 连接失败等）——探测表构造失败时 UI 走"未知"
    /// 降级提示，不阻塞主流程。
    pub async fn probe_async() -> Result<Self, String> {
        let detect = Compositor::detect();
        let mut rows = [
            Self::reference_row(Compositor::Kde),
            Self::reference_row(Compositor::Gnome),
            Self::reference_row(Compositor::Wlroots),
        ];
        // 本机实测行（检出行覆盖为实测值；未检出行保持文档预期）。
        let idx = match detect {
            Compositor::Kde => 0,
            Compositor::Gnome => 1,
            Compositor::Wlroots => 2,
            // Unknown 行：三行全不覆盖（检出行不存在的环境逐行 Untested）。
            Compositor::Unknown => usize::MAX,
        };
        let uinput = uinput_status();
        let inject_uinput = match &uinput {
            UinputStatus::Available => CapState::AVAILABLE,
            other => CapState::unavailable(other.guidance().unwrap_or("uinput 不可用")),
        };
        let probe = live_portal_probe().await;
        if let Some(row) = rows.get_mut(idx) {
            row.capture = probe.capture;
            row.inject_uinput = inject_uinput;
            row.inject_portal = probe.inject_portal;
            row.hotkey_portal = probe.hotkey_portal;
            row.clipboard_portal = probe.clipboard_portal;
        }
        Ok(Self { rows })
    }

    /// [`Self::probe_async`] 的阻塞封装（async_io::block_on——UI/CLI 同步调用
    /// 入口；app 的 tokio 域内请用 async 版。执行器选择依据见
    /// [`portal_block_on`] 文档）。
    #[must_use]
    pub fn probe() -> Self {
        async_io::block_on(Self::probe_async()).unwrap_or_else(|error| {
            tracing::warn!(target: "input::wayland", %error, "能力探测失败（全行降级）");
            Self {
                rows: [
                    Self::reference_row(Compositor::Kde),
                    Self::reference_row(Compositor::Gnome),
                    Self::reference_row(Compositor::Wlroots),
                ],
            }
        })
    }

    /// 文档预期行（未实测环境的降级展示：docs/05 §0/§4 静态预期 + Untested）。
    fn reference_row(compositor: Compositor) -> CompositorCaps {
        let untested = |detail: &'static str| CapState {
            status: CapStatus::Untested,
            detail: detail.to_owned(),
        };
        match compositor {
            Compositor::Kde => CompositorCaps {
                compositor,
                capture: untested("InputCapture portal（KDE ≥6.1 预期可用，docs/05 §4.1）"),
                inject_uinput: untested("/dev/uinput 权限待探测"),
                inject_portal: untested("RemoteDesktop portal（KDE 预期可用）"),
                hotkey_portal: untested("GlobalShortcuts portal（KDE ✅，docs/05 §4.1）"),
                clipboard_portal: untested("Clipboard portal + data-control（docs/05 §4.4）"),
            },
            Compositor::Gnome => CompositorCaps {
                compositor,
                capture: untested("InputCapture portal（GNOME ≥46；barrier 更新限制 docs/05 §8）"),
                inject_uinput: untested("/dev/uinput 权限待探测"),
                inject_portal: untested("RemoteDesktop portal（GNOME ≥46）"),
                hotkey_portal: untested("GlobalShortcuts portal（GNOME ≥47 ✅）"),
                clipboard_portal: untested("data-control 存疑（docs/05 §8；portal 降级移交 P2）"),
            },
            Compositor::Wlroots => CompositorCaps {
                compositor,
                capture: untested("layer-shell 边缘窗口路径（M5 未实现，docs/05 §4.2）"),
                inject_uinput: untested("/dev/uinput 权限待探测"),
                inject_portal: untested("portal 一般缺席（wlroots 无 xdg-desktop-portal 后端时）"),
                hotkey_portal: untested("GlobalShortcuts 多数不支持（docs/05 §4.2）"),
                clipboard_portal: untested("data-control（wl-clipboard-rs，docs/05 §4.4）"),
            },
            // Unknown 行不在 C-2 三行内（probe 不构造；match 完备性要求）。
            Compositor::Unknown => CompositorCaps {
                compositor,
                capture: untested("compositor 未识别（运行时探测为准）"),
                inject_uinput: untested("/dev/uinput 权限待探测"),
                inject_portal: untested("portal 存在性未知"),
                hotkey_portal: untested("热键路径未知（docs/05 §5 回退链）"),
                clipboard_portal: untested("剪贴板路径未知（docs/04 §4）"),
            },
        }
    }
}

/// 本机 portal 接口探测（异步；非交互——仅代理创建与属性读取）。
struct LivePortalProbe {
    capture: CapState,
    inject_portal: CapState,
    hotkey_portal: CapState,
    clipboard_portal: CapState,
}

async fn live_portal_probe() -> LivePortalProbe {
    let capture = match ashpd::desktop::input_capture::InputCapture::new().await {
        Ok(portal) => match portal.supported_capabilities().await {
            Ok(caps) => CapState {
                status: CapStatus::Available,
                detail: format!("InputCapture portal v{}（caps: {caps}）", portal.version()),
            },
            Err(error) => {
                CapState::unavailable(format!("supported_capabilities 查询失败：{error}"))
            }
        },
        Err(error) => CapState::unavailable(format!("InputCapture 代理创建失败：{error}")),
    };
    let inject_portal = match ashpd::desktop::remote_desktop::RemoteDesktop::new().await {
        Ok(portal) => match portal.available_device_types().await {
            Ok(types) => CapState {
                status: CapStatus::Available,
                detail: format!(
                    "RemoteDesktop portal v{}（devices: {types}）",
                    portal.version()
                ),
            },
            Err(error) => {
                CapState::unavailable(format!("available_device_types 查询失败：{error}"))
            }
        },
        Err(error) => CapState::unavailable(format!("RemoteDesktop 代理创建失败：{error}")),
    };
    let hotkey_portal = match ashpd::desktop::global_shortcuts::GlobalShortcuts::new().await {
        Ok(portal) => CapState {
            status: CapStatus::Available,
            detail: format!("GlobalShortcuts portal v{}", portal.version()),
        },
        Err(error) => CapState::unavailable(format!("GlobalShortcuts 代理创建失败：{error}")),
    };
    let clipboard_portal = match ashpd::desktop::clipboard::Clipboard::new().await {
        Ok(portal) => CapState {
            status: CapStatus::Available,
            detail: format!("Clipboard portal v{}", portal.version()),
        },
        Err(error) => CapState::unavailable(format!("Clipboard 代理创建失败：{error}")),
    };
    LivePortalProbe {
        capture,
        inject_portal,
        hotkey_portal,
        clipboard_portal,
    }
}

// ============================================================
// §4 边缘手势回退（docs/05 §5 优先级 3、docs/06 §4 设置开关；纯逻辑）
// ============================================================

/// 手势判定参数（时间窗 + 位置窗；默认值 = docs/05 §5 的 Synergy 语义量级）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GestureConfig {
    /// 边缘双击判定窗（同边两次撞边的时间上限；docs/05 §5：500ms）。
    pub double_tap_window: Duration,
    /// 角落悬停保持时长（持续驻留才触发；docs/05 §5 死角量级 700ms）。
    pub corner_hold: Duration,
    /// 边缘判定带宽度（px；贴边多少像素算"撞边"）。
    pub edge_band_px: f64,
    /// 角落判定半径（px；角落双轴邻域）。
    pub corner_radius_px: f64,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            double_tap_window: Duration::from_millis(500),
            corner_hold: Duration::from_millis(700),
            edge_band_px: 12.0,
            corner_radius_px: 16.0,
        }
    }
}

/// 四角（角落悬停命中；docs/05 §5 死角悬停 = 锁定/解锁语义，动作映射归
/// 上层——本模块只报事件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureCorner {
    /// 左上。
    TopLeft,
    /// 右上。
    TopRight,
    /// 左下。
    BottomLeft,
    /// 右下。
    BottomRight,
}

/// 手势命中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureHit {
    /// 边缘双击（500ms 内两次撞同一边缘；docs/05 §5 切换语义）。
    EdgeDoubleTap {
        /// 命中的边缘。
        edge: ScreenEdge,
    },
    /// 角落悬停（死角驻留 hold 时长后触发一次）。
    CornerHover {
        /// 命中的角落。
        corner: GestureCorner,
    },
}

/// 边缘手势传感器（纯状态机，edge.rs 思路：判定与轮询/平台读取解耦）。
///
/// 时间由调用方注入（`feed(elapsed, point)` 的 `elapsed` = 传感器启动起的
/// 单调时长）——零时钟依赖，时间窗全量单测（无需 sleep）。
///
/// 语义：
/// - **撞边** = 样本进入边缘带（距边 ≤ [`GestureConfig::edge_band_px`]）；
///   进入带沿即"一击"，离开带重新武装；
/// - **双击** = 同边两击间隔 ≤ [`GestureConfig::double_tap_window`]；异边
///   不链（换边重置序列）；窗口过期重置；
/// - **角落** = 距角点 ≤ [`GestureConfig::corner_radius_px`]（角落优先于
///   边缘带——角落必然同时在边缘带内）；驻留 hold 触发一次，离开重置；
/// - 样本越界/非有限值忽略并保持状态（与 edge.rs 的容错一致）。
#[derive(Debug)]
pub struct GestureSensor {
    screen_w: f64,
    screen_h: f64,
    config: GestureConfig,
    /// 上一次撞边：(边缘, 时刻)；与 in_band 配合构成双击序列。
    last_tap: Option<(ScreenEdge, Duration)>,
    /// 当前是否在某边缘带内（进入沿记一击，重复样本不记）。
    in_band: Option<ScreenEdge>,
    /// 角落驻留状态：(角落, 进入时刻)。
    corner_enter: Option<(GestureCorner, Duration)>,
    /// 角落已触发（驻留期内只报一次）。
    corner_fired: bool,
}

impl GestureSensor {
    /// 构建传感器（单屏几何 px；Wayland 手势回退作用于主控单屏语义）。
    ///
    /// 几何非法（零/负尺寸）→ `None`。
    #[must_use]
    pub fn new(screen_size: (u32, u32), config: GestureConfig) -> Option<Self> {
        let (w, h) = (f64::from(screen_size.0), f64::from(screen_size.1));
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        Some(Self {
            screen_w: w,
            screen_h: h,
            config,
            last_tap: None,
            in_band: None,
            corner_enter: None,
            corner_fired: false,
        })
    }

    /// 喂入一个光标样本 → 去抖后的手势命中。
    ///
    /// 单拍最多产出一条命中（角落优先）；无命中返回 `None`。
    #[must_use]
    pub fn feed(&mut self, elapsed: Duration, point: (f64, f64)) -> Option<GestureHit> {
        let (px, py) = point;
        // 非有限值忽略（协议浮点拒绝语义；状态保持）。
        if !px.is_finite() || !py.is_finite() {
            return None;
        }
        // 角落判定（优先于边缘带）。
        let r = self.config.corner_radius_px;
        let (w, h) = (self.screen_w, self.screen_h);
        let corner = if px <= r && py <= r {
            Some((GestureCorner::TopLeft, (0.0, 0.0)))
        } else if px >= w - r && py <= r {
            Some((GestureCorner::TopRight, (w, 0.0)))
        } else if px <= r && py >= h - r {
            Some((GestureCorner::BottomLeft, (0.0, h)))
        } else if px >= w - r && py >= h - r {
            Some((GestureCorner::BottomRight, (w, h)))
        } else {
            None
        };
        if let Some((which, _)) = corner {
            let hit = match self.corner_enter {
                // 驻留到达 hold 且未触发过 → 触发一次。
                Some((entered, at))
                    if entered == which
                        && !self.corner_fired
                        && elapsed.saturating_sub(at) >= self.config.corner_hold =>
                {
                    self.corner_fired = true;
                    Some(GestureHit::CornerHover { corner: which })
                }
                Some(_) => None,
                None => {
                    self.corner_enter = Some((which, elapsed));
                    self.corner_fired = false;
                    None
                }
            };
            // 角落驻留同时在边缘带内——按语义角落优先，撞边序列不推进。
            self.in_band = None;
            return hit;
        }
        // 离开角落：重置驻留。
        self.corner_enter = None;
        self.corner_fired = false;
        // 边缘带判定。
        let band = self.config.edge_band_px;
        let edge = if px <= band {
            Some(ScreenEdge::Left)
        } else if px >= w - band {
            Some(ScreenEdge::Right)
        } else if py <= band {
            Some(ScreenEdge::Top)
        } else if py >= h - band {
            Some(ScreenEdge::Bottom)
        } else {
            None
        };
        match (edge, self.in_band) {
            (Some(which), None) => {
                // 进入带沿 = 一击（进入沿记，重复样本不重复记）。
                self.in_band = Some(which);
                match self.last_tap {
                    Some((same, at))
                        if same == which
                            && elapsed.saturating_sub(at) <= self.config.double_tap_window =>
                    {
                        self.last_tap = None; // 命中后重置序列（三击不连环）
                        Some(GestureHit::EdgeDoubleTap { edge: which })
                    }
                    _ => {
                        self.last_tap = Some((which, elapsed));
                        None
                    }
                }
            }
            (Some(_), Some(_)) => None, // 带内继续移动：不推进
            (None, Some(_)) => {
                self.in_band = None; // 离带重新武装（双击序列保持——带外逗号
                None // 不清 window，快速来回仍算双击的合法路径）
            }
            (None, None) => None,
        }
    }
}

// ============================================================
// §5 portal 公共件（错误类型 + 触发串映射 + 线程骨架）
// ============================================================

/// portal 会话构建错误（start 类入口共用；文案不含安全码）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PortalInitError {
    /// portal DBus 调用失败（接口缺席/拒绝/响应错误）。
    #[error("portal 会话失败（{action}）：{reason}")]
    Portal {
        /// 失败的调用（create-session/bind-shortcuts/connect-eis 等）。
        action: &'static str,
        /// portal 侧原因（诊断用）。
        reason: String,
    },
    /// 授权对话框等待超时（无人值守的必然结果；重试需用户在场）。
    #[error("portal 授权等待超时（{0:.0}s）——对话框需用户在场确认")]
    Timeout(f64),
    /// 平台不支持（非 Linux / feature 未开等编译期不可能到达的运行时桩）。
    #[error("平台不支持：{0}")]
    Unsupported(&'static str),
}

impl PortalInitError {
    fn portal(action: &'static str, reason: impl Into<String>) -> Self {
        Self::Portal {
            action,
            reason: reason.into(),
        }
    }
}

/// 四热键 → GlobalShortcuts 触发串映射（config 直通 + core 解析校验）。
///
/// config 组合串（`Ctrl+Alt+Right` 便携格式）与 GlobalShortcuts 规范的
/// preferred_trigger 同形——先经 [`HotkeySet::from_config`] 全量校验（保证
/// 四条都能被核心解析，portal 侧拒绝留给实测），再按
/// [`HotkeyId::label()`] 对应 config 字段产出 `(id, 描述, trigger)`。
///
/// # Errors
/// [`InputError::Platform`]：任一组合串非法（core 解析失败）。
pub fn portal_trigger_map(
    config: &HotkeysConfig,
) -> Result<[(HotkeyId, &'static str, String); 4], InputError> {
    let _set = HotkeySet::from_config(config).map_err(|error| InputError::Platform {
        action: "hotkey-parse",
        reason: error.to_string(),
    })?;
    Ok([
        (HotkeyId::Next, "xcontrol: 切到下一台", config.next.clone()),
        (HotkeyId::Prev, "xcontrol: 切到上一台", config.prev.clone()),
        (
            HotkeyId::BackToHost,
            "xcontrol: 回主控",
            config.back_to_host.clone(),
        ),
        (HotkeyId::Pause, "xcontrol: 暂停/恢复", config.pause.clone()),
    ])
}

/// portal 控制线程的阻塞执行器（`async_io::block_on`——portal futures 全链路
/// 挂在 ashpd/zbus 的 async-io 面上，reactor 在 block_on 泵动期间被调度）。
///
/// 执行器裁定（11-19-m6-release）：原实现为 tokio current-thread runtime +
/// ashpd "tokio" feature；gpui-base（gpui-pre-linux）自身启用 ashpd
/// "async-io"，feature 统一后两执行器并存触发 ashpd `compile_error!`——
/// app 构建（打包面）首次把两者拉进同一依赖图而暴露。portal 调用面相应整体
/// 切换 async-io 执行器（超时竞速 = [`race_timeout`]、多路事件选择 =
/// `futures_util::select!`，均为 tokio 同款 API 的执行器无关对位替换）。
fn portal_block_on<F: Future>(fut: F) -> F::Output {
    async_io::block_on(fut)
}

/// 超时竞速的"到点"标记（对位 tokio Elapsed；Zero-sized）。
mod race_timeout {
    /// 超时到点（仅作类型标记）。
    #[derive(Debug)]
    pub(super) struct Elapsed;
}

/// portal 调用的超时竞速（tokio::time::timeout 的对位替换——授权对话框等待
/// 上限，超时 = [`PortalInitError::Timeout`] 契约的执行器无关承载）。
async fn race_timeout<F: Future>(
    timeout: Duration,
    fut: F,
) -> Result<F::Output, race_timeout::Elapsed> {
    let fut = std::pin::pin!(fut);
    let timer = async_io::Timer::after(timeout);
    match futures_util::future::select(fut, timer).await {
        futures_util::future::Either::Left((output, _)) => Ok(output),
        futures_util::future::Either::Right(_) => Err(race_timeout::Elapsed),
    }
}

// ============================================================
// §6 GlobalShortcuts portal 热键源（docs/05 §5 优先级 1）
// ============================================================

/// GlobalShortcuts portal 热键源：注册四热键 + Activated 事件轮询。
///
/// 线程模型：[`PortalHotkeys::start`] 起一个控制线程（`portal_block_on` 泵动，
/// 11-19-m6-release 执行器裁定）完成会话建立（**受 `timeout` 约束**——BindShortcuts 会弹一次性
/// 授权对话框，无人值守时超时返回 [`PortalInitError::Timeout`]），就绪握手
/// 后进入 select 事件循环；会话循环经 [`PortalHotkeys::poll`] 非阻塞拉取
/// [`HotkeyId`]（回调纪律：DBus 归控制线程，路由侧零 async）。
#[derive(Debug)]
pub struct PortalHotkeys {
    rx: Receiver<HotkeyId>,
    ctrl_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    control: Option<JoinHandle<()>>,
}

impl PortalHotkeys {
    /// 注册四热键（`timeout` = 授权对话框等待上限；无人值守探测传小值）。
    ///
    /// start 会**阻塞至就绪握手**（绑定确认或超时）——与 x11 后端 start 等
    /// StartOfData 凭证同款纪律（start 后立即 stop 不悬空会话）。
    ///
    /// # Errors
    /// [`PortalInitError`]：portal 失败 / 解析失败 / 授权超时。
    pub fn start(config: &HotkeysConfig, timeout: Duration) -> Result<Self, PortalInitError> {
        let triggers = portal_trigger_map(config).map_err(|error| PortalInitError::Portal {
            action: "hotkey-parse",
            reason: error.to_string(),
        })?;
        let (event_tx, event_rx) = channel();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (ready_tx, ready_rx) = channel::<Result<(), PortalInitError>>();
        let mut ctrl_rx = ctrl_rx;
        let control = std::thread::Builder::new()
            .name("wayland-hotkey-portal".into())
            .spawn(move || {
                portal_block_on(async move {
                    // —— 会话建立（受 timeout 管：BindShortcuts 的授权对话框
                    //    在无人值守时永远不来，必须能超时脱身）——
                    let setup = async {
                        let portal = ashpd::desktop::global_shortcuts::GlobalShortcuts::new()
                            .await
                            .map_err(|error| ("proxy", error.to_string()))?;
                        let session = portal
                            .create_session(ashpd::desktop::CreateSessionOptions::default())
                            .await
                            .map_err(|error| ("create-session", error.to_string()))?;
                        let shortcuts: Vec<ashpd::desktop::global_shortcuts::NewShortcut> =
                            triggers
                                .iter()
                                .map(|(id, description, trigger)| {
                                    ashpd::desktop::global_shortcuts::NewShortcut::new(
                                        id.label(),
                                        *description,
                                    )
                                    .preferred_trigger(trigger.as_str())
                                })
                                .collect();
                        let bound = portal
                            .bind_shortcuts(
                                &session,
                                &shortcuts,
                                None,
                                ashpd::desktop::global_shortcuts::BindShortcutsOptions::default(),
                            )
                            .await
                            .map_err(|error| ("bind-shortcuts", error.to_string()))?;
                        let bound = bound
                            .response()
                            .map_err(|error| ("bind-shortcuts-response", error.to_string()))?;
                        // 回读平台实际解释的绑定（trigger_description = 实测
                        // 凭证；preferred_trigger 与平台解析差异在此显形）。
                        for shortcut in bound.shortcuts() {
                            tracing::info!(
                                target: "input::wayland",
                                id = shortcut.id(),
                                trigger = shortcut.trigger_description(),
                                "GlobalShortcuts 注册完成"
                            );
                        }
                        // portal 持活必须（信号流借用其连接）——随外层 future。
                        Ok::<_, (&'static str, String)>((portal, session))
                    };
                    use futures_util::{FutureExt, StreamExt};
                    let (portal, session) = match race_timeout(timeout, setup).await {
                        Ok(Ok(ready)) => ready,
                        Ok(Err((action, reason))) => {
                            let _ = ready_tx.send(Err(PortalInitError::portal(action, reason)));
                            return;
                        }
                        // 超时 = 授权对话框等待上限——契约类型 Timeout（无人值守
                        // 移交语义，调用方按此分支识别"需用户在场"）。
                        Err(_elapsed) => {
                            let _ =
                                ready_tx.send(Err(PortalInitError::Timeout(timeout.as_secs_f64())));
                            return;
                        }
                    };
                    // 信号流在同一外层 future 内创建（borrow portal 不跨边界）。
                    let mut activated = match portal.receive_activated().await {
                        Ok(stream) => std::pin::pin!(stream),
                        Err(error) => {
                            let _ = ready_tx.send(Err(PortalInitError::portal(
                                "receive-activated",
                                error.to_string(),
                            )));
                            return;
                        }
                    };
                    if ready_tx.send(Ok(())).is_err() {
                        // 调用方已放弃（start 的握手 recv 出错）——收尾退出。
                        let _ = session.close().await;
                        return;
                    }
                    // —— 事件循环（不受 timeout 管；Stop 或流终止退出；
                    //    futures_util::select! 对位 tokio::select!——宏内部
                    //    自行 stack-pin，借用流可跨迭代轮询）——
                    loop {
                        futures_util::select! {
                            item = activated.next().fuse() => match item {
                                Some(event) => {
                                    let id = match event.shortcut_id() {
                                        "next" => Some(HotkeyId::Next),
                                        "prev" => Some(HotkeyId::Prev),
                                        "back_to_host" => Some(HotkeyId::BackToHost),
                                        "pause" => Some(HotkeyId::Pause),
                                        _ => None,
                                    };
                                    if let Some(id) = id {
                                        // 满即丢弃（热路径背压策略；动作仍由
                                        // 下一拍重复触发补偿）。
                                        let _ = event_tx.send(id);
                                    } else {
                                        tracing::debug!(
                                            target: "input::wayland",
                                            id = event.shortcut_id(),
                                            "未识别的热键 id（忽略）"
                                        );
                                    }
                                }
                                None => break, // 信号流终止（portal 断开）
                            },
                            _ = ctrl_rx.recv().fuse() => break, // Stop（发送端全丢弃）
                        }
                    }
                    let _ = session.close().await;
                });
            })
            .map_err(|error| PortalInitError::portal("thread", error.to_string()))?;
        // 就绪握手（绑定确认或失败原因；此处阻塞是有界的——线程侧有 timeout）。
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx: event_rx,
                ctrl_tx: Some(ctrl_tx),
                control: Some(control),
            }),
            Ok(Err(error)) => {
                let _ = control.join();
                Err(error)
            }
            Err(recv_error) => Err(PortalInitError::portal(
                "thread",
                format!("热键控制线程崩溃：{recv_error}"),
            )),
        }
    }

    /// 拉取一个已激活的热键（非阻塞；无事件返回 `None`）。
    #[must_use]
    pub fn poll(&mut self) -> Option<HotkeyId> {
        self.rx.try_recv().ok()
    }

    /// 停止会话（close + join 控制线程）。
    ///
    /// # Errors
    /// [`PortalInitError::Portal`]：控制线程 join 失败（panic 传播）。
    pub fn stop(mut self) -> Result<(), PortalInitError> {
        self.stop_impl();
        match self.control.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| PortalInitError::portal("join", "热键控制线程 panic".to_owned())),
            None => Ok(()),
        }
    }

    /// 停止信号（Drop 与 stop 共用；丢发送端 → 控制线程 recv None → 退出）。
    fn stop_impl(&mut self) {
        self.ctrl_tx = None;
    }
}

impl Drop for PortalHotkeys {
    fn drop(&mut self) {
        // 尽力停止：丢发送端触发线程退出（不 join——Drop 不阻塞；显式 stop()
        // 才等待）。
        self.stop_impl();
    }
}

// ============================================================
// §7 InputCapture portal 捕获源（docs/05 §4.1 首选捕获；InputSource 实现）
// ============================================================

/// 捕获会话控制命令（调用方 → 控制线程；非阻塞投递）。
#[derive(Debug, Clone, Copy)]
pub enum CaptureCmd {
    /// 释放控制（`edge` = 撞入方向；回位坐标按该方向自屏障**内侧偏 1px**，
    /// docs/05 §4.1 release 行）。
    Release {
        /// 撞入的边缘（决定回位偏移方向）。
        edge: ScreenEdge,
    },
    /// 暂停捕获（disable；GNOME 46 disable 后不可 re-enable 的坑位见 docs/05
    /// §8——恢复用新会话，控制线程对 re-enable 返回错误）。
    Disable,
    /// 恢复捕获（enable；GNOME 旧版可能失败——错误经日志留痕）。
    Enable,
}

/// InputCapture portal 捕获源。
///
/// 线程模型（与 [`PortalHotkeys`] 同骨架）：
/// - **控制线程**（`portal_block_on` 泵动）：会话建立（create_session →
///   connect_to_eis → zones → set_pointer_barriers → enable，全程受 `timeout`
///   管理授权对话框等待）→ 就绪握手 → select 循环（Activated/Deactivated/
///   Disabled 信号 + 控制命令）；
/// - **EIS 工作线程**（纯同步）：`ei_handshake_blocking(Receiver)` →
///   `SeatAdded` 时 bind 能力 → 事件转换（见下）→ `std::mpsc` 投递。
///
/// EIS 事件转换表（docs/05 §4.1 "cursor 位置流 → proto InputEvent"）：
///
/// | EiEvent | proto InputEvent |
/// | --- | --- |
/// | `PointerMotion{dx,dy}` | `MouseMoveRel` |
/// | `PointerMotionAbsolute{dx_absolute,dy_absolute}` | `MouseMoveAbs`（主 zone 归一化） |
/// | `KeyboardKey{key,state}`（key = Linux evdev 码） | `Key`（`evdev_to_xkey` 反查）|
/// | `Button{button,state}`（BTN 码） | `MouseButton` |
/// | `ScrollDiscrete{discrete_dx,discrete_dy}` | `Wheel`（整数格直通） |
/// | `ScrollDelta{dx,dy}` | `Wheel`（连续量近似格；docs/05 §4.1 注记） |
/// | `KeyboardModifiers` | mods 快照原子更新（键事件同源维护） |
///
/// # 已知限制（M5 范围）
///
/// - 绝对位置按 EIS `DeviceAdded.dimensions()` 归一化（缺席时 1920×1080 兜底）；
///   多 zone 屏幕布局协商（M5 布局收口）后由会话层映射。
/// - wlroots 无 portal → 构造失败即上层降级提示（[`CapabilityTable`] 是决策
///   入口，docs/06 §4）。
#[derive(Debug)]
pub struct PortalCaptureSource {
    rx: Receiver<InputEvent>,
    mods: std::sync::Arc<AtomicU8>,
    ctrl_tx: Option<tokio::sync::mpsc::UnboundedSender<CaptureCmd>>,
    stop: std::sync::Arc<AtomicBool>,
    control: Option<JoinHandle<()>>,
    eis: Option<JoinHandle<()>>,
}

impl PortalCaptureSource {
    /// 建立捕获会话（`timeout` = 授权等待上限）。
    ///
    /// Barriers：对每个 zone 的四条边缘各建一条（M5 单屏语义；多屏邻居边
    /// 筛选随布局协商收口——docs/05 §4.1 barrier 行）。被 compositor 拒绝的
    /// barrier 记日志不致命。
    ///
    /// # Errors
    /// [`PortalInitError`]：portal 失败 / 授权超时。
    pub fn start(timeout: Duration) -> Result<Self, PortalInitError> {
        let (event_tx, event_rx) = channel();
        let (fd_tx, fd_rx) = channel::<std::os::fd::OwnedFd>();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::unbounded_channel::<CaptureCmd>();
        let mut ctrl_rx = ctrl_rx;
        let (ready_tx, ready_rx) = channel::<Result<(), PortalInitError>>();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let mods = std::sync::Arc::new(AtomicU8::new(0));

        // —— EIS 工作线程（纯同步；先起，fd 由控制线程就绪后投递）——
        let stop_for_eis = std::sync::Arc::clone(&stop);
        let mods_for_eis = std::sync::Arc::clone(&mods);
        let eis = std::thread::Builder::new()
            .name("wayland-capture-eis".into())
            .spawn(move || {
                run_eis_receiver(fd_rx, &event_tx, &mods_for_eis, &stop_for_eis);
            })
            .map_err(|error| PortalInitError::portal("thread", error.to_string()))?;

        // —— 控制线程（portal DBus）——
        let stop_for_ctrl = std::sync::Arc::clone(&stop);
        let control = std::thread::Builder::new()
            .name("wayland-capture-portal".into())
            .spawn(move || {
                portal_block_on(async move {
                    use futures_util::{FutureExt, StreamExt};
                    // —— 会话建立（受 timeout 管）：async 块只跨边界返回**自有
                    //    值**（portal/session/fd）；信号流在同一外层 future 内
                    //    创建（borrow portal 不跨界——async future 的局部借用
                    //    跨 await 合法，跨函数边界才违规）。——
                    let setup = async {
                        let portal = ashpd::desktop::input_capture::InputCapture::new()
                            .await
                            .map_err(|error| ("proxy", error.to_string()))?;
                        let (session, _caps) = portal
                            .create_session(
                                None,
                                ashpd::desktop::input_capture::CreateSessionOptions::default()
                                    .set_capabilities(
                                        ashpd::desktop::input_capture::Capabilities::Keyboard
                                            | ashpd::desktop::input_capture::Capabilities::Pointer,
                                    ),
                            )
                            .await
                            .map_err(|error| ("create-session", error.to_string()))?;
                        let eis_fd = portal
                            .connect_to_eis(&session, Default::default())
                            .await
                            .map_err(|error| ("connect-to-eis", error.to_string()))?;
                        let zones = portal
                            .zones(&session, Default::default())
                            .await
                            .map_err(|error| ("zones", error.to_string()))?
                            .response()
                            .map_err(|error| ("zones-response", error.to_string()))?;
                        tracing::info!(
                            target: "input::wayland",
                            zones = zones.regions().len(),
                            zone_set = zones.zone_set(),
                            "InputCapture zones 取得"
                        );
                        // 四边 barriers（每 zone 一组；BarrierID 从 1 起且非零）。
                        let mut barriers = Vec::new();
                        for region in zones.regions() {
                            let (x, y) = (region.x_offset(), region.y_offset());
                            let (w, h) = (
                                i32::try_from(region.width()).unwrap_or(i32::MAX),
                                i32::try_from(region.height()).unwrap_or(i32::MAX),
                            );
                            for position in [
                                ashpd::desktop::input_capture::BarrierPosition::new(x, y, x, y + h - 1),
                                ashpd::desktop::input_capture::BarrierPosition::new(x + w, y, x + w, y + h - 1),
                                ashpd::desktop::input_capture::BarrierPosition::new(x, y, x + w - 1, y),
                                ashpd::desktop::input_capture::BarrierPosition::new(x, y + h, x + w - 1, y + h),
                            ] {
                                let id = ashpd::desktop::input_capture::BarrierID::new(
                                    barriers.len() as u32 + 1,
                                )
                                .ok_or_else(|| ("barrier-id", "id 为零".to_owned()))?;
                                barriers.push(ashpd::desktop::input_capture::Barrier::new(id, position));
                            }
                        }
                        let set = portal
                            .set_pointer_barriers(&session, &barriers, zones.zone_set(), Default::default())
                            .await
                            .map_err(|error| ("set-pointer-barriers", error.to_string()))?
                            .response()
                            .map_err(|error| ("set-pointer-barriers-response", error.to_string()))?;
                        let failed = set.failed_barriers();
                        if !failed.is_empty() {
                            tracing::warn!(
                                target: "input::wayland",
                                failed = failed.len(),
                                "部分 barrier 被 compositor 拒绝（对应边缘不触发切换）"
                            );
                        }
                        portal
                            .enable(&session, Default::default())
                            .await
                            .map_err(|error| ("enable", error.to_string()))?;
                        Ok::<_, (&'static str, String)>((portal, session, eis_fd))
                    };
                    let Ok(Ok((portal, session, eis_fd))) = race_timeout(timeout, setup).await
                    else {
                        // 超时 = 授权对话框等待上限——契约类型 Timeout（无人值守
                        // 移交语义，调用方按此分支识别"需用户在场"）。
                        let _ = ready_tx.send(Err(PortalInitError::Timeout(timeout.as_secs_f64())));
                        stop_for_ctrl.store(true, Ordering::Release);
                        return;
                    };
                    // —— 信号流（同一 future 内创建；borrow portal 不跨边界）——
                    let activated = match portal.receive_activated().await {
                        Ok(stream) => stream,
                        Err(error) => {
                            let _ = ready_tx.send(Err(PortalInitError::portal(
                                "receive-activated",
                                error.to_string(),
                            )));
                            stop_for_ctrl.store(true, Ordering::Release);
                            return;
                        }
                    };
                    let deactivated = match portal.receive_deactivated().await {
                        Ok(stream) => stream,
                        Err(error) => {
                            let _ = ready_tx.send(Err(PortalInitError::portal(
                                "receive-deactivated",
                                error.to_string(),
                            )));
                            stop_for_ctrl.store(true, Ordering::Release);
                            return;
                        }
                    };
                    let disabled = match portal.receive_disabled().await {
                        Ok(stream) => stream,
                        Err(error) => {
                            let _ = ready_tx.send(Err(PortalInitError::portal(
                                "receive-disabled",
                                error.to_string(),
                            )));
                            stop_for_ctrl.store(true, Ordering::Release);
                            return;
                        }
                    };
                    if fd_tx.send(eis_fd).is_err() {
                        // EIS 线程已死（异常路径）——收尾。
                        let _ = ready_tx.send(Err(PortalInitError::portal(
                            "eis-thread",
                            "线程提前退出",
                        )));
                        stop_for_ctrl.store(true, Ordering::Release);
                        return;
                    }
                    if ready_tx.send(Ok(())).is_err() {
                        let _ = session.close().await;
                        stop_for_ctrl.store(true, Ordering::Release);
                        return;
                    }
                    // 最近一次激活（release 回位坐标来源；docs/05 §4.1）。
                    let mut last_activation: Option<(u32, (f32, f32))> = None;
                    let mut pinned_activated = std::pin::pin!(activated);
                    let mut pinned_deactivated = std::pin::pin!(deactivated);
                    let mut pinned_disabled = std::pin::pin!(disabled);
                    loop {
                        futures_util::select! {
                            item = pinned_activated.next().fuse() => match item {
                                Some(event) => {
                                    let id = event.activation_id().unwrap_or_default();
                                    let pos = event.cursor_position().unwrap_or((0.0, 0.0));
                                    tracing::info!(
                                        target: "input::wayland",
                                        activation_id = id, cursor = ?pos,
                                        "InputCapture 激活（受控中）"
                                    );
                                    last_activation = Some((id, pos));
                                }
                                None => break,
                            },
                            item = pinned_deactivated.next().fuse() => match item {
                                Some(_) => {
                                    tracing::info!(target: "input::wayland", "InputCapture 去激活（回本机）");
                                    last_activation = None;
                                }
                                None => break,
                            },
                            item = pinned_disabled.next().fuse() => match item {
                                Some(_) => {
                                    tracing::info!(target: "input::wayland", "InputCapture 被禁用（用户/compositor 侧）");
                                }
                                None => break,
                            },
                            cmd = ctrl_rx.recv().fuse() => match cmd {
                                Some(CaptureCmd::Release { edge }) => {
                                    let Some((id, (x, y))) = last_activation else {
                                        tracing::debug!(target: "input::wayland", "无激活记录的 release（忽略）");
                                        continue;
                                    };
                                    // 回位坐标：屏障内侧偏 1px（docs/05 §4.1）。
                                    let (rx, ry) = match edge {
                                        ScreenEdge::Left => (x + 1.0, y),
                                        ScreenEdge::Right => (x - 1.0, y),
                                        ScreenEdge::Top => (x, y - 1.0),
                                        ScreenEdge::Bottom => (x, y + 1.0),
                                    };
                                    let options = ashpd::desktop::input_capture::ReleaseOptions::default()
                                        .set_activation_id(id)
                                        .set_cursor_position((f64::from(rx), f64::from(ry)));
                                    if let Err(error) = portal.release(&session, options).await {
                                        tracing::warn!(target: "input::wayland", %error, "release 调用失败");
                                    }
                                    last_activation = None;
                                }
                                Some(CaptureCmd::Disable) => {
                                    if let Err(error) = portal.disable(&session, Default::default()).await {
                                        tracing::warn!(target: "input::wayland", %error, "disable 调用失败");
                                    }
                                }
                                Some(CaptureCmd::Enable) => {
                                    // GNOME 46 disable 后 re-enable 失败的坑位
                                    // （docs/05 §8）——KDE 6.7 支持，失败留日志。
                                    if let Err(error) = portal.enable(&session, Default::default()).await {
                                        tracing::warn!(target: "input::wayland", %error, "re-enable 失败（GNOME 46 已知限制；需重建会话）");
                                    }
                                }
                                None => break,
                            },
                        }
                    }
                    stop_for_ctrl.store(true, Ordering::Release);
                    let _ = portal.disable(&session, Default::default()).await;
                    let _ = session.close().await;
                });
            })
            .map_err(|error| PortalInitError::portal("thread", error.to_string()))?;
        // 就绪握手。
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx: event_rx,
                mods,
                ctrl_tx: Some(ctrl_tx),
                stop,
                control: Some(control),
                eis: Some(eis),
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = control.join();
                let _ = eis.join();
                Err(error)
            }
            Err(recv_error) => Err(PortalInitError::portal(
                "thread",
                format!("捕获控制线程崩溃：{recv_error}"),
            )),
        }
    }

    /// 发送控制命令（非阻塞；满/关闭即丢弃并留日志）。
    fn send_cmd(&self, cmd: CaptureCmd) {
        if let Some(Err(error)) = self.ctrl_tx.as_ref().map(|tx| tx.send(cmd)) {
            tracing::debug!(target: "input::wayland", %error, "捕获控制命令投递失败（会话已停？）");
        }
    }

    /// 释放控制回主控（受控侧边缘反向移动时由会话层调用）。
    pub fn release(&self, edge: ScreenEdge) {
        self.send_cmd(CaptureCmd::Release { edge });
    }

    /// 暂停捕获。
    pub fn disable(&self) {
        self.send_cmd(CaptureCmd::Disable);
    }

    /// 恢复捕获（GNOME 46 失败已文档化）。
    pub fn enable(&self) {
        self.send_cmd(CaptureCmd::Enable);
    }

    /// 停止（disable + close + 双线程 join）。
    ///
    /// # Errors
    /// [`PortalInitError::Portal`]：线程 join 失败。
    pub fn stop(mut self) -> Result<(), PortalInitError> {
        self.stop.store(true, Ordering::Release);
        self.ctrl_tx = None;
        let control_result = match self.control.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| PortalInitError::portal("join", "捕获控制线程 panic".to_owned())),
            None => Ok(()),
        };
        let eis_result = match self.eis.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| PortalInitError::portal("join", "EIS 工作线程 panic".to_owned())),
            None => Ok(()),
        };
        control_result.and(eis_result)
    }
}

impl Drop for PortalCaptureSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.ctrl_tx = None;
    }
}

impl crate::InputSource for PortalCaptureSource {
    fn poll(&mut self, out: &mut Vec<InputEvent>) {
        while let Ok(event) = self.rx.try_recv() {
            out.push(event);
        }
    }

    fn mods(&self) -> Mods {
        Mods::from_bits(u32::from(self.mods.load(Ordering::Acquire)))
    }
}

/// EIS 接收线程主循环（纯同步；fd 到达 → 握手 → 事件转换 → mpsc）。
fn run_eis_receiver(
    fd_rx: Receiver<std::os::fd::OwnedFd>,
    event_tx: &Sender<InputEvent>,
    mods: &AtomicU8,
    stop: &AtomicBool,
) {
    // 读超时 = 停机轮询节拍（50ms）：SO_RCVTIMEO 超时以 WouldBlock 冒出，
    // 每拍检查 stop 标志——不引入 rustix/libc 轮询面。
    let Ok(fd) = fd_rx.recv() else { return };
    let stream = std::os::unix::net::UnixStream::from(fd);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let context = match reis::ei::Context::new(stream) {
        Ok(context) => context,
        Err(error) => {
            tracing::error!(target: "input::wayland", %error, "EIS context 创建失败");
            return;
        }
    };
    if context.flush().is_err() {
        tracing::error!(target: "input::wayland", "EIS 初始 flush 失败");
        return;
    }
    let resp = match reis::handshake::ei_handshake_blocking(
        &context,
        "xcontrol-input",
        reis::ei::handshake::ContextType::Receiver,
    ) {
        Ok(resp) => resp,
        Err(error) => {
            tracing::error!(target: "input::wayland", %error, "EIS 握手失败");
            return;
        }
    };
    let mut converter = reis::event::EiEventConverter::new(&context, resp);
    loop {
        match context.read() {
            Ok(_read) => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Err(error) => {
                tracing::info!(target: "input::wayland", %error, "EIS 连接关闭（退出捕获线程）");
                break;
            }
        }
        let mut protocol_error = false;
        while let Some(result) = context.pending_event() {
            match result {
                reis::PendingRequestResult::Request(event) => {
                    if let Err(error) = converter.handle_event(event) {
                        tracing::warn!(target: "input::wayland", %error, "EIS 事件处理错误");
                        protocol_error = true;
                        break;
                    }
                }
                reis::PendingRequestResult::ParseError(error) => {
                    tracing::warn!(target: "input::wayland", %error, "EIS 协议解析错误");
                    protocol_error = true;
                    break;
                }
                reis::PendingRequestResult::InvalidObject(id) => {
                    tracing::warn!(target: "input::wayland", object = id, "EIS 未知对象");
                    protocol_error = true;
                    break;
                }
            }
        }
        if protocol_error {
            break;
        }
        // 产出的事件逐条转换（回调纪律：查静态表 + 位运算 + mpsc）。
        let mut disconnect = false;
        while let Some(event) = converter.next_event() {
            match event {
                reis::event::EiEvent::SeatAdded(seat) => {
                    seat.seat.bind_capabilities(
                        reis::event::DeviceCapability::Pointer
                            | reis::event::DeviceCapability::PointerAbsolute
                            | reis::event::DeviceCapability::Keyboard
                            | reis::event::DeviceCapability::Scroll
                            | reis::event::DeviceCapability::Button,
                    );
                    if context.flush().is_err() {
                        disconnect = true;
                        break;
                    }
                }
                reis::event::EiEvent::DeviceAdded(device) => {
                    let dev = &device.device;
                    tracing::info!(
                        target: "input::wayland",
                        name = dev.name().unwrap_or("?"),
                        dims = ?dev.dimensions(),
                        "EIS 设备就绪（捕获开始）"
                    );
                }
                reis::event::EiEvent::KeyboardKey(key_event) => {
                    let Some(xkey) = evdev_to_xkey(key_event.key) else {
                        tracing::debug!(
                            target: "input::wayland",
                            key = key_event.key,
                            "键码无 XKey 翻译（跳过）"
                        );
                        continue;
                    };
                    if let Some(mod_bit) = Mods::from_key(xkey) {
                        match key_event.state {
                            reis::ei::keyboard::KeyState::Press => {
                                mods.fetch_or(mod_bit.bits() as u8, Ordering::AcqRel);
                            }
                            reis::ei::keyboard::KeyState::Released => {
                                mods.fetch_and(!mod_bit.bits() as u8, Ordering::AcqRel);
                            }
                        }
                    }
                    let _ = event_tx.send(InputEvent::Key {
                        code: xkey,
                        state: match key_event.state {
                            reis::ei::keyboard::KeyState::Press => KeyState::Down,
                            reis::ei::keyboard::KeyState::Released => KeyState::Up,
                        },
                    });
                }
                reis::event::EiEvent::Button(button_event) => {
                    let Some(button) = evdev_to_mouse_button(button_event.button) else {
                        tracing::debug!(target: "input::wayland", btn = button_event.button, "按键无翻译（跳过）");
                        continue;
                    };
                    let _ = event_tx.send(InputEvent::MouseButton {
                        button,
                        state: match button_event.state {
                            reis::ei::button::ButtonState::Press => KeyState::Down,
                            reis::ei::button::ButtonState::Released => KeyState::Up,
                        },
                    });
                }
                reis::event::EiEvent::PointerMotion(motion) => {
                    let _ = event_tx.send(InputEvent::MouseMoveRel {
                        dx: motion.dx,
                        dy: motion.dy,
                    });
                }
                reis::event::EiEvent::PointerMotionAbsolute(motion) => {
                    // 归一化：EIS 绝对坐标空间 = 设备 dimensions（缺席 1080p
                    // 兜底；M5 布局协商后由会话层修正映射）。
                    let (w, h) = dims_of(&motion.device);
                    let _ = event_tx.send(InputEvent::MouseMoveAbs {
                        screen: 0,
                        x: motion.dx_absolute / w,
                        y: motion.dy_absolute / h,
                    });
                }
                reis::event::EiEvent::ScrollDiscrete(scroll) => {
                    // 垂直轴：discrete_dy 正 = 上（wire 语义同向）。
                    if scroll.discrete_dy != 0 {
                        let _ = event_tx.send(InputEvent::Wheel {
                            axis: Axis::Vertical,
                            delta: scroll.discrete_dy as f32,
                        });
                    }
                    if scroll.discrete_dx != 0 {
                        let _ = event_tx.send(InputEvent::Wheel {
                            axis: Axis::Horizontal,
                            delta: scroll.discrete_dx as f32,
                        });
                    }
                }
                reis::event::EiEvent::ScrollDelta(scroll) => {
                    // 连续滚动量近似格（KWin 以像素值投递；docs/02 delta 允许
                    // 小数，直通保真）。
                    if scroll.dy.abs() > f32::EPSILON {
                        let _ = event_tx.send(InputEvent::Wheel {
                            axis: Axis::Vertical,
                            delta: scroll.dy,
                        });
                    }
                    if scroll.dx.abs() > f32::EPSILON {
                        let _ = event_tx.send(InputEvent::Wheel {
                            axis: Axis::Horizontal,
                            delta: scroll.dx,
                        });
                    }
                }
                reis::event::EiEvent::KeyboardModifiers(_) => {
                    // mods 快照由键事件流维护（x11 后端同语义）；xkb 全量对齐
                    // 属 ModifierSync 周期（1s，docs/02 §4.2），M5 不引入 xkb。
                }
                reis::event::EiEvent::Disconnected(disconnected) => {
                    tracing::info!(
                        target: "input::wayland",
                        reason = ?disconnected.reason,
                        "EIS 断开"
                    );
                    disconnect = true;
                    break;
                }
                reis::event::EiEvent::Frame(_)
                | reis::event::EiEvent::DeviceRemoved(_)
                | reis::event::EiEvent::DevicePaused(_)
                | reis::event::EiEvent::DeviceResumed(_)
                | reis::event::EiEvent::SeatRemoved(_)
                | reis::event::EiEvent::DeviceStartEmulating(_)
                | reis::event::EiEvent::DeviceStopEmulating(_)
                | reis::event::EiEvent::ScrollStop(_)
                | reis::event::EiEvent::ScrollCancel(_)
                | reis::event::EiEvent::TouchDown(_)
                | reis::event::EiEvent::TouchUp(_)
                | reis::event::EiEvent::TouchMotion(_)
                | reis::event::EiEvent::TouchCancel(_)
                | reis::event::EiEvent::TextKeysym(_)
                | reis::event::EiEvent::TextUtf8(_) => {
                    // 触控/文本/帧事件 M5 不消费（能力协商不含 Touch）。
                }
            }
        }
        if disconnect || stop.load(Ordering::Acquire) {
            break;
        }
    }
}

/// EIS 设备的绝对坐标空间尺寸（缺席 1080p 兜底；除零防护）。
fn dims_of(device: &reis::event::Device) -> (f32, f32) {
    match device.dimensions() {
        Some((w, h)) if w > 0 && h > 0 => (w as f32, h as f32),
        _ => (1920.0, 1080.0),
    }
}

// ============================================================
// §8 RemoteDesktop portal 注入器（docs/05 §4.1 备选；InputSink 实现）
// ============================================================

/// 注入命令（调用方 → EIS 工作线程；非阻塞投递）。
#[derive(Debug)]
enum InjectCmd {
    /// 注入一条事件（已映射到 evdev/BTN 语义）。
    Event(InjectEvent),
    /// 绝对定位（warp；zone 像素坐标）。
    Warp { x: f32, y: f32 },
    /// 结束（断开 EIS）。
    Stop,
}

/// EIS 发送侧的语义事件（键码已翻译为 Linux evdev/BTN 码）。
#[derive(Debug)]
enum InjectEvent {
    /// 键（evdev 码）。
    Key { code: u32, down: bool },
    /// 鼠标键（BTN 码）。
    Button { code: u32, down: bool },
    /// 相对移动。
    Motion { dx: f32, dy: f32 },
    /// 滚轮（离散格；[水平, 垂直]）。
    Wheel { horizontal: i32, vertical: i32 },
    /// Unicode 直发（EIS Text 接口；docs/05 §0 portal 注入行 CAP_UNICODE ✅）。
    Text { utf8: String },
}

/// RemoteDesktop portal 注入器（[`InputSink`]）。
///
/// 与 [`UinputInjector`] 的分工：portal 路径需要**用户在场授权一次**
/// （select_devices/start 弹对话框）+ restore token 免重复授权（docs/05 §6
/// wayland 从机行）；换来的是**不走 compositor 安全模型之外**的通道 + EIS
/// Text 接口的 Unicode 直发。授权后与 uinput 注入器同一 [`InputSink`] 面供
/// 会话层无感切换（docs/05 §4.1 注入行）。
///
/// 线程模型：控制线程（tokio）建立会话（受 `timeout` 管）→ fd 交 EIS 工作线程
/// （同步 Sender 握手：bind 能力 → `request_device` → 等 `DeviceAdded`）→
/// 命令循环（`blocking_recv`）逐条发送 + `frame()` 批次语义（每条注入 =
/// start_emulating → 事件 → frame → stop_emulating——libei 的逻辑批次约定，
/// compositor 端聚合处理）。
#[derive(Debug)]
pub struct PortalInjector {
    cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<InjectCmd>>,
    screen_w: f64,
    screen_h: f64,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u16>,
    wheel_accum: [f32; 2],
    /// EIS Text 接口可用性（工作线程在设备协商后置位；初值 unknown=true，
    /// 已知不可用后 `Text` 注入直接返回 `Unsupported` 不排队）。
    text_supported: std::sync::Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    control: Option<JoinHandle<()>>,
    stop: std::sync::Arc<AtomicBool>,
}

impl PortalInjector {
    /// 建立 RemoteDesktop 注入会话（`timeout` = 授权等待上限；无人值守探测
    /// 传小值——授权对话框需用户在场）。
    ///
    /// # Errors
    /// [`PortalInitError`]：portal 失败 / 授权超时。
    pub fn start(screen_size: (u32, u32), timeout: Duration) -> Result<Self, PortalInitError> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<InjectCmd>();
        let (fd_tx, fd_rx) = channel::<std::os::fd::OwnedFd>();
        let (ready_tx, ready_rx) = channel::<Result<(), PortalInitError>>();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let text_supported = std::sync::Arc::new(AtomicBool::new(true));

        // —— EIS 工作线程（同步 Sender）——
        let stop_for_eis = std::sync::Arc::clone(&stop);
        let text_for_eis = std::sync::Arc::clone(&text_supported);
        let worker = std::thread::Builder::new()
            .name("wayland-inject-eis".into())
            .spawn(move || {
                run_eis_sender(fd_rx, cmd_rx, &stop_for_eis, &text_for_eis);
            })
            .map_err(|error| PortalInitError::portal("thread", error.to_string()))?;

        // —— 控制线程（portal DBus）——
        let stop_for_ctrl = std::sync::Arc::clone(&stop);
        let control = std::thread::Builder::new()
            .name("wayland-inject-portal".into())
            .spawn(move || {
                portal_block_on(async move {
                    let setup = async {
                        let portal = ashpd::desktop::remote_desktop::RemoteDesktop::new()
                            .await
                            .map_err(|error| ("proxy", error.to_string()))?;
                        let devices = portal
                            .available_device_types()
                            .await
                            .map_err(|error| ("available-device-types", error.to_string()))?;
                        tracing::info!(
                            target: "input::wayland",
                            portal_version = portal.version(),
                            devices = %devices,
                            "RemoteDesktop 能力（键鼠授权前提）"
                        );
                        let session = portal
                            .create_session(Default::default())
                            .await
                            .map_err(|error| ("create-session", error.to_string()))?;
                        portal
                            .select_devices(
                                &session,
                                ashpd::desktop::remote_desktop::SelectDevicesOptions::default()
                                    .set_devices(
                                        ashpd::desktop::remote_desktop::DeviceType::Keyboard
                                            | ashpd::desktop::remote_desktop::DeviceType::Pointer,
                                    ),
                            )
                            .await
                            .map_err(|error| ("select-devices", error.to_string()))?
                            .response()
                            .map_err(|error| ("select-devices-response", error.to_string()))?;
                        // start = 授权对话框挂起点（无人值守超时处）。
                        portal
                            .start(&session, None, Default::default())
                            .await
                            .map_err(|error| ("start", error.to_string()))?
                            .response()
                            .map_err(|error| ("start-response", error.to_string()))?;
                        let eis_fd = portal
                            .connect_to_eis(&session, Default::default())
                            .await
                            .map_err(|error| ("connect-to-eis", error.to_string()))?;
                        Ok::<_, (&'static str, String)>((portal, session, eis_fd))
                    };
                    let Ok(Ok((portal, session, eis_fd))) = race_timeout(timeout, setup).await
                    else {
                        // 超时 = 授权对话框等待上限——契约类型 Timeout（无人值守
                        // 移交语义，调用方按此分支识别"需用户在场"）。
                        let _ = ready_tx.send(Err(PortalInitError::Timeout(timeout.as_secs_f64())));
                        stop_for_ctrl.store(true, Ordering::Release);
                        return;
                    };
                    if fd_tx.send(eis_fd).is_err() || ready_tx.send(Ok(())).is_err() {
                        stop_for_ctrl.store(true, Ordering::Release);
                        return;
                    }
                    // 控制线程驻留会话生命周期（连接存活 = runtime 存活），
                    // Stop 时清理。
                    while !stop_for_ctrl.load(Ordering::Acquire) {
                        async_io::Timer::after(Duration::from_millis(200)).await;
                    }
                    let _ = session.close().await;
                    drop(portal);
                });
            })
            .map_err(|error| PortalInitError::portal("thread", error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cmd_tx: Some(cmd_tx),
                screen_w: f64::from(screen_size.0.max(1)),
                screen_h: f64::from(screen_size.1.max(1)),
                pressed_keys: BTreeSet::new(),
                pressed_buttons: BTreeSet::new(),
                wheel_accum: [0.0; 2],
                text_supported,
                worker: Some(worker),
                control: Some(control),
                stop,
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                let _ = control.join();
                Err(error)
            }
            Err(recv_error) => Err(PortalInitError::portal(
                "thread",
                format!("注入控制线程崩溃：{recv_error}"),
            )),
        }
    }

    /// 投递命令（非阻塞；队列关闭即丢弃留日志——热路径背压策略）。
    fn queue(&self, cmd: InjectCmd) {
        if let Some(Err(error)) = self.cmd_tx.as_ref().map(|tx| tx.send(cmd)) {
            tracing::debug!(target: "input::wayland", %error, "注入命令投递失败（会话已停？）");
        }
    }
}

impl InputSink for PortalInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), SinkError> {
        match *event {
            InputEvent::Key { code, state } => {
                let Some(evdev_code) = xkey_to_evdev(code) else {
                    return Err(SinkError::Unsupported(
                        "键位无 evdev 翻译（portal 注入路径）",
                    ));
                };
                match state == KeyState::Down {
                    true => self.pressed_keys.insert(evdev_code),
                    false => self.pressed_keys.remove(&evdev_code),
                };
                self.queue(InjectCmd::Event(InjectEvent::Key {
                    code: u32::from(evdev_code),
                    down: state == KeyState::Down,
                }));
                Ok(())
            }
            InputEvent::KeyRepeat { code } => {
                let Some(evdev_code) = xkey_to_evdev(code) else {
                    return Err(SinkError::Unsupported(
                        "键位无 evdev 翻译（portal 注入路径）",
                    ));
                };
                self.pressed_keys.insert(evdev_code);
                self.queue(InjectCmd::Event(InjectEvent::Key {
                    code: u32::from(evdev_code),
                    down: true,
                }));
                Ok(())
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                self.queue(InjectCmd::Event(InjectEvent::Motion { dx, dy }));
                Ok(())
            }
            InputEvent::MouseMoveAbs { x, y, .. } => {
                self.queue(InjectCmd::Warp {
                    x: (f64::from(x) * self.screen_w) as f32,
                    y: (f64::from(y) * self.screen_h) as f32,
                });
                Ok(())
            }
            InputEvent::MouseButton { button, state } => {
                let btn = u32::from(mouse_button_to_evdev(button));
                match state == KeyState::Down {
                    true => self.pressed_buttons.insert(mouse_button_to_evdev(button)),
                    false => self.pressed_buttons.remove(&mouse_button_to_evdev(button)),
                };
                self.queue(InjectCmd::Event(InjectEvent::Button {
                    code: btn,
                    down: state == KeyState::Down,
                }));
                Ok(())
            }
            InputEvent::Wheel { axis, delta } => {
                let slot = match axis {
                    Axis::Vertical => 1,
                    Axis::Horizontal => 0,
                };
                self.wheel_accum[slot] += delta;
                let steps = self.wheel_accum[slot].trunc();
                if steps != 0.0 {
                    self.wheel_accum[slot] -= steps;
                    let (horizontal, vertical) = match axis {
                        Axis::Vertical => (0, steps as i32),
                        Axis::Horizontal => (steps as i32, 0),
                    };
                    self.queue(InjectCmd::Event(InjectEvent::Wheel {
                        horizontal,
                        vertical,
                    }));
                }
                Ok(())
            }
            InputEvent::Text { ref utf8 } => {
                if !self.text_supported.load(Ordering::Acquire) {
                    return Err(SinkError::Unsupported(
                        "RemoteDesktop 会话无 EIS Text 接口（Unicode 直发不可用）",
                    ));
                }
                self.queue(InjectCmd::Event(InjectEvent::Text { utf8: utf8.clone() }));
                Ok(())
            }
            InputEvent::EnterScreen { .. }
            | InputEvent::LeaveScreen { .. }
            | InputEvent::ModifierSync { .. }
            | InputEvent::SwitchRequest { .. }
            | InputEvent::SwitchAck { .. } => {
                Err(SinkError::Unsupported("会话控制事件不进入注入路径"))
            }
        }
    }

    fn warp(&mut self, screen: ScreenId, x: f32, y: f32) -> Result<(), SinkError> {
        let _ = screen; // 单 zone 语义（M5；多屏协商后由会话层换算）
        self.queue(InjectCmd::Warp {
            x: (f64::from(x) * self.screen_w) as f32,
            y: (f64::from(y) * self.screen_h) as f32,
        });
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), SinkError> {
        for code in std::mem::take(&mut self.pressed_keys) {
            self.queue(InjectCmd::Event(InjectEvent::Key {
                code: u32::from(code),
                down: false,
            }));
        }
        for code in std::mem::take(&mut self.pressed_buttons) {
            self.queue(InjectCmd::Event(InjectEvent::Button {
                code: u32::from(code),
                down: false,
            }));
        }
        Ok(())
    }
}

impl PortalInjector {
    /// 停止会话（断 EIS + 关会话 + 双线程 join）。
    ///
    /// # Errors
    /// [`PortalInitError::Portal`]：线程 join 失败。
    pub fn stop(mut self) -> Result<(), PortalInitError> {
        self.stop.store(true, Ordering::Release);
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(InjectCmd::Stop);
        }
        let worker_result = match self.worker.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| PortalInitError::portal("join", "注入 EIS 线程 panic".to_owned())),
            None => Ok(()),
        };
        let control_result = match self.control.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| PortalInitError::portal("join", "注入控制线程 panic".to_owned())),
            None => Ok(()),
        };
        worker_result.and(control_result)
    }
}

impl Drop for PortalInjector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(InjectCmd::Stop);
        }
    }
}

/// EIS 发送线程（同步）：Sender 握手 → 设备协商 → 命令循环。
///
/// Sender 侧设备协商（协议语义，源码核实 reis 0.7.1）：`ei_seat.capability`
/// 事件给出 (interface, mask) 映射 → Done 后 `bind(mask 并集)` +
/// `request_device(mask 并集)` → 设备经 `ei_seat.device` 事件到达（高层
/// `EiEvent::DeviceAdded`）。高层 `Seat` 封装只暴露 `bind_capabilities`（其
/// capability_map 由 Capability 事件建立），**`request_device` 仅存在于裸
/// `ei::Seat` 代理**——本线程拦截 `ei::Event::Seat` 原样事件记录 (cap → mask)
/// 与 proto seat，bind/request_device 走裸代理（语义同高层 bind，文档化裁定）。
fn run_eis_sender(
    fd_rx: Receiver<std::os::fd::OwnedFd>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<InjectCmd>,
    stop: &AtomicBool,
    text_supported: &AtomicBool,
) {
    use reis::event::DeviceCapability;
    // 发送侧声明的能力集（键 + 相对/绝对指针 + 滚轮 + 键；Text 仅探测上报）。
    const WANTED: [DeviceCapability; 5] = [
        DeviceCapability::Keyboard,
        DeviceCapability::Pointer,
        DeviceCapability::PointerAbsolute,
        DeviceCapability::Scroll,
        DeviceCapability::Button,
    ];
    let Ok(fd) = fd_rx.recv() else { return };
    let stream = std::os::unix::net::UnixStream::from(fd);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let context = match reis::ei::Context::new(stream) {
        Ok(context) => context,
        Err(error) => {
            tracing::error!(target: "input::wayland", %error, "EIS sender context 创建失败");
            return;
        }
    };
    if context.flush().is_err() {
        return;
    }
    let resp = match reis::handshake::ei_handshake_blocking(
        &context,
        "xcontrol-input",
        reis::ei::handshake::ContextType::Sender,
    ) {
        Ok(resp) => resp,
        Err(error) => {
            tracing::error!(target: "input::wayland", %error, "EIS sender 握手失败");
            return;
        }
    };
    let mut converter = reis::event::EiEventConverter::new(&context, resp);
    // —— 设备协商循环：capability 映射 → bind + request_device → DeviceAdded ——
    let mut cap_masks: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut proto_seat: Option<reis::ei::Seat> = None;
    let mut device: Option<reis::event::Device> = None;
    let mut bound = false;
    while device.is_none() {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match context.read() {
            Ok(_) => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(_) => {
                tracing::info!(target: "input::wayland", "EIS sender 断开（协商期）");
                return;
            }
        }
        let mut protocol_error = false;
        while let Some(result) = context.pending_event() {
            let event = match result {
                reis::PendingRequestResult::Request(event) => event,
                reis::PendingRequestResult::ParseError(error) => {
                    tracing::error!(target: "input::wayland", %error, "EIS sender 协议解析错误");
                    protocol_error = true;
                    break;
                }
                reis::PendingRequestResult::InvalidObject(id) => {
                    tracing::error!(target: "input::wayland", object = id, "EIS sender 未知对象");
                    protocol_error = true;
                    break;
                }
            };
            // 拦截 Seat 原始事件：记录 proto seat 与 (interface → mask)。
            if let reis::ei::Event::Seat(seat_proto, seat_event) = &event {
                proto_seat = Some(seat_proto.clone());
                if let reis::ei::seat::Event::Capability { mask, interface } = seat_event {
                    cap_masks.insert(interface.clone(), *mask);
                }
            }
            if let Err(error) = converter.handle_event(event) {
                tracing::error!(target: "input::wayland", %error, "EIS sender 事件处理错误");
                protocol_error = true;
                break;
            }
        }
        if protocol_error {
            return;
        }
        while let Some(event) = converter.next_event() {
            match event {
                reis::event::EiEvent::SeatAdded(_) => {
                    // SeatAdded（= seat Done）：bind + request_device（裸代理）。
                    let Some(seat) = proto_seat.clone() else {
                        continue;
                    };
                    let mut union_mask = 0u64;
                    for cap in WANTED {
                        // reis 的 interface_name() 是 pub(crate)——按接口名常量
                        // 自行映射（ei.xml 固定命名，源码核实）。
                        let name = match cap {
                            DeviceCapability::Pointer => "ei_pointer",
                            DeviceCapability::PointerAbsolute => "ei_pointer_absolute",
                            DeviceCapability::Keyboard => "ei_keyboard",
                            DeviceCapability::Scroll => "ei_scroll",
                            DeviceCapability::Button => "ei_button",
                            DeviceCapability::Touch => "ei_touchscreen",
                            DeviceCapability::Text => "ei_text",
                        };
                        if let Some(mask) = cap_masks.get(name) {
                            union_mask |= mask;
                        }
                    }
                    if union_mask == 0 {
                        tracing::error!(target: "input::wayland", "seat 无可用能力掩码（协商失败）");
                        return;
                    }
                    seat.bind(union_mask);
                    seat.request_device(union_mask);
                    if context.flush().is_err() {
                        tracing::info!(target: "input::wayland", "EIS sender flush 失败（协商期）");
                        return;
                    }
                    bound = true;
                }
                reis::event::EiEvent::DeviceAdded(added) => {
                    let dev = &added.device;
                    tracing::info!(
                        target: "input::wayland",
                        name = dev.name().unwrap_or("?"),
                        has_text = dev.has_capability(DeviceCapability::Text),
                        "EIS 注入设备就绪"
                    );
                    device = Some(dev.clone());
                }
                reis::event::EiEvent::Disconnected(_) => {
                    tracing::info!(target: "input::wayland", "EIS sender 断开（协商期）");
                    return;
                }
                _ => {}
            }
        }
    }
    let Some(device) = device else { return };
    let _ = bound;
    // Text 接口可用性上报（KWin 是否暴露 ei_text 为实测项）。
    let has_text = device.interface::<reis::ei::Text>().is_some();
    text_supported.store(has_text, Ordering::Release);
    tracing::info!(target: "input::wayland", has_text, "EIS Text 接口状态");
    let proto = device.device();
    // —— 注入命令循环（每条命令 = start_emulating → 事件 → frame → stop——
    // libei 逻辑批次约定，compositor 端聚合处理）——
    while let Some(cmd) = cmd_rx.blocking_recv() {
        if matches!(cmd, InjectCmd::Stop) || stop.load(Ordering::Acquire) {
            break;
        }
        match cmd {
            InjectCmd::Event(InjectEvent::Key { code, down }) => {
                let Some(keyboard) = device.interface::<reis::ei::Keyboard>() else {
                    continue;
                };
                proto.start_emulating(0, 0);
                keyboard.key(
                    code,
                    match down {
                        true => reis::ei::keyboard::KeyState::Press,
                        false => reis::ei::keyboard::KeyState::Released,
                    },
                );
                proto.frame(0, now_micros());
                proto.stop_emulating(0);
            }
            InjectCmd::Event(InjectEvent::Button { code, down }) => {
                let Some(button) = device.interface::<reis::ei::Button>() else {
                    continue;
                };
                proto.start_emulating(0, 0);
                button.button(
                    code,
                    match down {
                        true => reis::ei::button::ButtonState::Press,
                        false => reis::ei::button::ButtonState::Released,
                    },
                );
                proto.frame(0, now_micros());
                proto.stop_emulating(0);
            }
            InjectCmd::Event(InjectEvent::Motion { dx, dy }) => {
                let Some(pointer) = device.interface::<reis::ei::Pointer>() else {
                    continue;
                };
                proto.start_emulating(0, 0);
                pointer.motion_relative(dx, dy);
                proto.frame(0, now_micros());
                proto.stop_emulating(0);
            }
            InjectCmd::Event(InjectEvent::Wheel {
                horizontal,
                vertical,
            }) => {
                let Some(scroll) = device.interface::<reis::ei::Scroll>() else {
                    continue;
                };
                proto.start_emulating(0, 0);
                if vertical != 0 {
                    scroll.scroll_discrete(0, vertical);
                }
                if horizontal != 0 {
                    scroll.scroll_discrete(horizontal, 0);
                }
                proto.frame(0, now_micros());
                proto.stop_emulating(0);
            }
            InjectCmd::Event(InjectEvent::Text { ref utf8 }) => {
                let Some(text) = device.interface::<reis::ei::Text>() else {
                    continue;
                };
                proto.start_emulating(0, 0);
                text.utf8(utf8);
                proto.frame(0, now_micros());
                proto.stop_emulating(0);
            }
            InjectCmd::Warp { x, y } => {
                let Some(pointer) = device.interface::<reis::ei::PointerAbsolute>() else {
                    continue;
                };
                proto.start_emulating(0, 0);
                pointer.motion_absolute(x, y);
                proto.frame(0, now_micros());
                proto.stop_emulating(0);
            }
            InjectCmd::Stop => break,
        }
        if context.flush().is_err() {
            tracing::info!(target: "input::wayland", "EIS sender flush 失败（断开）");
            break;
        }
    }
}

/// 当前时刻（微秒；EIS frame 时间戳单位）。
fn now_micros() -> u64 {
    use std::time::SystemTime;
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_micros() as u64,
        Err(_) => 0,
    }
}

// ============================================================
// §9 测试（纯逻辑默认套件 + 交互/实机项 #[ignore] 远端配方见模块文档）
// ============================================================

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// KDE 触发角机制实验（#[ignore]，KDE Wayland 实机 + **人工观察**）：
    /// REL 相对移动把光标推向**左上角**并驻留 3s。判据：kwin 触发角（左上
    /// 角动作）在 REL 路径下触发 = kwin 边缘判定对 REL 响应、对受控 ABS
    /// 注入路径不响应（修法定向：受控移动注入改 REL/混合双设备）；不触发
    /// = kwin 触发角另有条件（回 0.1.x 池）。
    ///
    /// ssh 会话可跑（uinput 写内核无需桌面环境）；观察在用户桌面。
    /// 运行：`cargo test -p xcontrol-input --features wayland -- --ignored
    /// kwin_corner_rel_probe --nocapture`
    #[test]
    #[ignore = "KDE 触发角实验：需实机人工观察 kwin 触发角"]
    fn kwin_corner_rel_probe() {
        let mut inj = super::UinputInjector::auto().expect("uinput 设备");
        std::thread::sleep(Duration::from_millis(300));
        // 先大步长推向右下（远离角落，制造"从内侧接近"的轨迹）。
        for _ in 0..40 {
            let _ = inj.probe_rel(60, 60);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(500));
        // 渐进加大负推左上角（REL 过 kwin 加速曲线，递增步长保底穿出）。
        for i in 0..60u32 {
            let step = -(100i32 + (i as i32) * 80);
            let _ = inj.probe_rel(step, step);
            std::thread::sleep(Duration::from_millis(30));
        }
        println!("已推向左上角，驻留 3s——请观察 kwin 触发角是否触发");
        std::thread::sleep(Duration::from_secs(3));
        println!("实验结束");
    }

    use xcontrol_config::HotkeysConfig;
    use xcontrol_proto::input::{KeyState, MouseButton, ScreenEdge};
    use xcontrol_proto::xkey::{
        KEY_A, KEY_F15, KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_SHIFT, KEY_RIGHT,
    };

    use super::*;

    // —— §1 权限探测 ——

    /// 权限探测不 panic 且三态语义自洽（可用 ⇔ 无引导文案；本机非 Linux 时
    /// 节点缺失 → NodeMissing 是合法结果）。
    #[test]
    fn uinput_status_trichotomy() {
        let status = uinput_status();
        match &status {
            UinputStatus::Available => assert!(status.guidance().is_none()),
            UinputStatus::NodeMissing | UinputStatus::Denied => {
                assert!(status.guidance().is_some());
            }
        }
        assert_eq!(
            status.is_available(),
            matches!(status, UinputStatus::Available)
        );
    }

    /// 引导文案不含安全码/密钥（docs/07 红线；文案可读）。
    #[test]
    fn guidance_has_no_secrets() {
        for status in [UinputStatus::NodeMissing, UinputStatus::Denied] {
            let text = status.guidance().expect("不可用状态必有引导");
            assert!(!text.to_lowercase().contains("password"));
            assert!(!text.contains("secret"));
        }
    }

    // —— §2 键码/按键映射（纯函数） ——

    /// XKey → evdev 全表抽样：字母/修饰键/功能键（x11keymap 同源）。
    #[test]
    fn xkey_to_evdev_table_samples() {
        assert_eq!(xkey_to_evdev(KEY_A), Some(30)); // evdev KEY_A = 30
        assert_eq!(xkey_to_evdev(KEY_LEFT_CTRL), Some(29));
        assert_eq!(xkey_to_evdev(KEY_LEFT_ALT), Some(56));
        assert_eq!(xkey_to_evdev(KEY_F15), Some(185)); // EVDEV_TABLE：F13=183 → F15=185
        // 反查回环（捕获方向）。
        assert_eq!(evdev_to_xkey(30), Some(KEY_A));
        assert_eq!(evdev_to_xkey(0xFFFF), None); // 超出 u8 → None
    }

    /// 鼠标键映射：协议 wire 序（Left=0, Middle=1, Right=2…）→ BTN 码。
    #[test]
    fn mouse_button_maps() {
        assert_eq!(mouse_button_to_evdev(MouseButton::Left), 0x110);
        assert_eq!(mouse_button_to_evdev(MouseButton::Right), 0x111);
        assert_eq!(mouse_button_to_evdev(MouseButton::Middle), 0x112);
        assert_eq!(mouse_button_to_evdev(MouseButton::Back), 0x113);
        assert_eq!(mouse_button_to_evdev(MouseButton::Forward), 0x114);
        // 反查全回环。
        for button in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::Back,
            MouseButton::Forward,
        ] {
            assert_eq!(
                evdev_to_mouse_button(u32::from(mouse_button_to_evdev(button))),
                Some(button)
            );
        }
        assert_eq!(evdev_to_mouse_button(0x118), None);
    }

    // —— §2.1 屏幕像素探测（11-22 边缘回切修复轮） ——

    /// ABS 域归一化：端点容差带收敛（±1 单位 → 0.0/1.0）+ 中段比例换算。
    /// uinput `cursor_normalized` 的域换算契约（与 edge.rs
    /// `EDGE_TOLERANCE_POINTS` 同型——1 ABS 单位 = 1 像素）。
    #[test]
    fn abs_normalization_snaps_endpoints_and_scales() {
        // 端点与 ±1 单位容差带。
        assert_eq!(abs_normalized(0, 1919), 0.0);
        assert_eq!(abs_normalized(1, 1919), 0.0);
        assert_eq!(abs_normalized(1919, 1919), 1.0);
        assert_eq!(abs_normalized(1918, 1919), 1.0);
        // 中段：精确比例换算、不触发容差带（4K 域 max = 3839：1920 ≈ 0.50013）。
        assert!((abs_normalized(1920, 3839) - 1920.0 / 3839.0).abs() < 1e-6);
        assert!((abs_normalized(960, 1919) - 960.0 / 1919.0).abs() < 1e-6);
        // 1 单位 = 1 像素：max-2 不收敛（容差带内边界精确）。
        assert!((abs_normalized(1917, 1919) - 1917.0 / 1919.0).abs() < 1e-6);
        // warp 可把 virt 推过域上界（x=1.0 → px = w > max）：clamp 到 1.0。
        assert_eq!(abs_normalized(1920, 1919), 1.0);
        assert_eq!(abs_normalized(-5, 1919), 0.0);
    }

    /// 兜底标定决策：探针 None → 1920×1080（无 wayland 环境的 fallback 路径
    /// ——CI 无 GUI / SSH 非交互 shell；探针 Some 原样透传）。
    #[test]
    fn calibration_size_falls_back_without_probe() {
        assert_eq!(calibration_size(None), (1920, 1080));
        assert_eq!(calibration_size(Some((3840, 2160))), (3840, 2160));
    }

    /// 多输出并集：单输出原样、双输出包围盒、负坐标（左挂副屏）包围盒、
    /// 零尺寸输出跳过、空/全零输出 → None（fallback 入口）。
    #[test]
    fn outputs_union_pixels_bounding_box() {
        let geo = |x: i32, y: i32, w: u32, h: u32| OutputGeometry {
            position: (x, y),
            size: (w, h),
        };
        // 单输出（4K 实机）原样。
        assert_eq!(
            outputs_union_pixels(&[geo(0, 0, 3840, 2160)]),
            Some((3840, 2160))
        );
        // 双输出横排（主屏右侧副屏）：并集宽 = 3840 + 1920。
        assert_eq!(
            outputs_union_pixels(&[geo(0, 0, 3840, 2160), geo(3840, 0, 1920, 1080)]),
            Some((5760, 2160))
        );
        // 负坐标（主屏左侧副屏）：包围盒含负区间。
        assert_eq!(
            outputs_union_pixels(&[geo(-1920, 108, 1920, 1080), geo(0, 0, 2560, 1440)]),
            Some((4480, 1440))
        );
        // 垂直堆叠。
        assert_eq!(
            outputs_union_pixels(&[geo(0, 0, 1920, 1080), geo(0, 1080, 1920, 1080)]),
            Some((1920, 2160))
        );
        // 零尺寸输出跳过（几何异常防御）。
        assert_eq!(
            outputs_union_pixels(&[geo(0, 0, 0, 1080), geo(0, 0, 1920, 1080)]),
            Some((1920, 1080))
        );
        // 空 / 全零 → None（调用方走兜底标定）。
        assert_eq!(outputs_union_pixels(&[]), None);
        assert_eq!(outputs_union_pixels(&[geo(0, 0, 0, 0)]), None);
    }

    // —— §3 能力探测表 ——

    /// 探测表结构恒三行（KDE/GNOME/wlroots，C-2）；本机非 Wayland 时全行
    /// Untested（探测失败走降级——不 panic）。
    #[test]
    fn capability_table_shape() {
        let table = CapabilityTable::probe();
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.rows[0].compositor, Compositor::Kde);
        assert_eq!(table.rows[1].compositor, Compositor::Gnome);
        assert_eq!(table.rows[2].compositor, Compositor::Wlroots);
        // 未实测行必须是 Untested（诚实三态；docs/06 §4 降级提示依据）。
        for row in &table.rows {
            if row.compositor != Compositor::detect() {
                assert_eq!(row.capture.status, CapStatus::Untested);
            }
        }
    }

    /// compositor 识别（env 注入不安全——只测 Unknown 兜底与 label 映射）。
    #[test]
    fn compositor_labels() {
        assert_eq!(Compositor::Kde.label(), "KDE");
        assert_eq!(Compositor::Wlroots.label(), "wlroots");
    }

    // —— §4 边缘手势（纯逻辑全量） ——

    /// 1000×1000 屏 + 默认参数的传感器。
    fn sensor() -> GestureSensor {
        GestureSensor::new((1000, 1000), GestureConfig::default()).expect("合法几何")
    }

    /// 边缘双击：同边 500ms 内两击触发一次，重置后需再来一轮。
    #[test]
    fn edge_double_tap_within_window() {
        let mut s = sensor();
        let t = |ms: u64| Duration::from_millis(ms);
        // 第一击：进带沿（右缘带 12px：x ≥ 988）。
        assert_eq!(s.feed(t(0), (995.0, 500.0)), None);
        // 带内继续移动：不推进。
        assert_eq!(s.feed(t(50), (999.0, 500.0)), None);
        // 出带重新武装。
        assert_eq!(s.feed(t(100), (500.0, 500.0)), None);
        // 第二击（同边，间隔 300ms ≤ 500ms）：触发。
        assert_eq!(
            s.feed(t(400), (993.0, 500.0)),
            Some(GestureHit::EdgeDoubleTap {
                edge: ScreenEdge::Right
            })
        );
        // 命中后序列重置：新一轮双击照常触发（进带→出带→进带）。
        assert_eq!(s.feed(t(450), (500.0, 500.0)), None);
        assert_eq!(s.feed(t(500), (999.0, 500.0)), None); // 新基线
        assert_eq!(s.feed(t(550), (500.0, 500.0)), None);
        assert_eq!(
            s.feed(t(600), (993.0, 500.0)),
            Some(GestureHit::EdgeDoubleTap {
                edge: ScreenEdge::Right
            }) // 间隔 100ms
        );
    }

    /// 双击窗口过期：间隔 > 500ms → 第二击只重置基线，不触发。
    #[test]
    fn edge_double_tap_window_expires() {
        let mut s = sensor();
        let t = |ms: u64| Duration::from_millis(ms);
        assert_eq!(s.feed(t(0), (999.0, 500.0)), None);
        assert_eq!(s.feed(t(100), (500.0, 500.0)), None);
        assert_eq!(s.feed(t(700), (995.0, 500.0)), None); // 700ms > 500ms：过期，重置基线
        assert_eq!(s.feed(t(750), (500.0, 500.0)), None);
        assert_eq!(
            s.feed(t(800), (995.0, 500.0)),
            Some(GestureHit::EdgeDoubleTap {
                edge: ScreenEdge::Right
            }) // 新基线 + 100ms 第二击
        );
    }

    /// 异边不链：左击后换右击，双击不触发（换边重置序列）。
    #[test]
    fn edge_double_tap_requires_same_edge() {
        let mut s = sensor();
        let t = |ms: u64| Duration::from_millis(ms);
        assert_eq!(s.feed(t(0), (5.0, 500.0)), None); // 左缘一击（基线 = Left）
        assert_eq!(s.feed(t(100), (500.0, 500.0)), None); // 出带
        assert_eq!(s.feed(t(200), (995.0, 500.0)), None); // 右缘击：异边 → 重置基线为 Right，不触发
        assert_eq!(s.feed(t(250), (500.0, 500.0)), None); // 出带
        assert_eq!(
            s.feed(t(300), (995.0, 500.0)),
            Some(GestureHit::EdgeDoubleTap {
                edge: ScreenEdge::Right
            }) // 同边（Right）窗口内第二击
        );
    }

    /// 角落悬停：驻留 ≥700ms 触发一次；驻留期内不重复；离开重置。
    #[test]
    fn corner_hover_hold_and_rearm() {
        let mut s = sensor();
        let t = |ms: u64| Duration::from_millis(ms);
        // 左上角（半径 16px）内驻留。
        assert_eq!(s.feed(t(0), (3.0, 4.0)), None); // 进入
        assert_eq!(s.feed(t(300), (5.0, 5.0)), None); // 未达 hold
        assert_eq!(
            s.feed(t(700), (2.0, 8.0)),
            Some(GestureHit::CornerHover {
                corner: GestureCorner::TopLeft
            })
        );
        // 驻留继续：不再触发（corner_fired 闩锁）。
        assert_eq!(s.feed(t(1000), (1.0, 1.0)), None);
        // 离开角落重置 → 重新驻留可再触发。
        assert_eq!(s.feed(t(1100), (500.0, 500.0)), None);
        assert_eq!(s.feed(t(1200), (4.0, 4.0)), None);
        assert_eq!(
            s.feed(t(1900), (4.0, 4.0)),
            Some(GestureHit::CornerHover {
                corner: GestureCorner::TopLeft
            })
        );
    }

    /// 角落四象限各自命中；角落优先于边缘带（角落样本不推进撞边序列）。
    #[test]
    fn corner_quadrants_take_precedence() {
        let mut s = sensor();
        let t = |ms: u64| Duration::from_millis(ms);
        // 右下角。
        assert_eq!(s.feed(t(0), (995.0, 997.0)), None);
        assert_eq!(
            s.feed(t(700), (999.0, 995.0)),
            Some(GestureHit::CornerHover {
                corner: GestureCorner::BottomRight
            })
        );
        // 角落样本不推进撞边序列：角落进出不影响撞边基线的准确性。
        let mut s = sensor();
        assert_eq!(s.feed(t(0), (995.0, 995.0)), None); // 右下角进入（角落优先）
        assert_eq!(s.feed(t(100), (999.0, 500.0)), None); // 出角落 → 右缘一击（基线 R,100）
        assert_eq!(s.feed(t(200), (995.0, 997.0)), None); // 回右下角（角落优先，不推进边）
        assert_eq!(
            s.feed(t(300), (999.0, 500.0)),
            Some(GestureHit::EdgeDoubleTap {
                edge: ScreenEdge::Right
            }) // 出角落进带 = 第二击（间隔 200ms）
        );
        assert_eq!(s.feed(t(400), (500.0, 500.0)), None); // 出带（命中后基线已重置）
        assert_eq!(s.feed(t(450), (999.0, 500.0)), None); // 新基线第一击
    }

    /// 异常样本：非有限值/越界屏外忽略且状态保持。
    #[test]
    fn gesture_ignores_invalid_samples() {
        let mut s = sensor();
        let t = |ms: u64| Duration::from_millis(ms);
        // 先制造一击。
        assert_eq!(s.feed(t(0), (999.0, 500.0)), None);
        // 非有限值。
        assert_eq!(s.feed(t(100), (f64::NAN, 500.0)), None);
        assert_eq!(s.feed(t(100), (f64::INFINITY, 500.0)), None);
        // 越界（屏外远点）。
        assert_eq!(s.feed(t(100), (5000.0, 500.0)), None);
        // 回内 + 再击（时间窗内）→ 仍触发（状态未被异常样本破坏）。
        assert_eq!(s.feed(t(150), (500.0, 500.0)), None);
        assert_eq!(
            s.feed(t(200), (995.0, 500.0)),
            Some(GestureHit::EdgeDoubleTap {
                edge: ScreenEdge::Right
            })
        );
    }

    /// 零/负尺寸几何拒绝。
    #[test]
    fn gesture_rejects_bad_geometry() {
        assert!(GestureSensor::new((0, 100), GestureConfig::default()).is_none());
        assert!(GestureSensor::new((100, 0), GestureConfig::default()).is_none());
    }

    // —— §5 触发串映射 ——

    /// 默认 config → 四热键映射（label 与 core HotkeyId::label 同源）。
    #[test]
    fn trigger_map_defaults() {
        let config = HotkeysConfig::default();
        let map = portal_trigger_map(&config).expect("默认热键必过解析");
        assert_eq!(map.len(), 4);
        assert_eq!(map[0].0, HotkeyId::Next);
        assert_eq!(map[0].2, "Ctrl+Alt+Right");
        assert_eq!(map[1].0, HotkeyId::Prev);
        assert_eq!(map[2].0, HotkeyId::BackToHost);
        assert_eq!(map[3].0, HotkeyId::Pause);
        assert_eq!(map[3].2, "Ctrl+Alt+P");
    }

    /// 非法组合串 → core 解析校验拦截（错误可读）。
    #[test]
    fn trigger_map_rejects_invalid() {
        let config = HotkeysConfig {
            pause: "Ctrl+".into(), // 空段
            ..HotkeysConfig::default()
        };
        let error = portal_trigger_map(&config).expect_err("非法串必拒");
        assert!(error.to_string().contains("hotkey-parse") || error.to_string().contains("热键"));
    }

    /// KeyState 语义对齐（EIS Press/Released ↔ wire Down/Up 的映射依据）。
    #[test]
    fn key_state_wire_semantics() {
        assert_ne!(KeyState::Down, KeyState::Up);
        assert_eq!(xkey_to_evdev(KEY_RIGHT), Some(106)); // evdev KEY_RIGHT = 106
        assert_eq!(xkey_to_evdev(KEY_LEFT_SHIFT), Some(42));
    }

    // —— 实机/交互项（#[ignore]；远端配方见模块文档） ——

    /// uinput 注入冒烟：权限允许时创建设备 + 注入 F15 down/up + release_all。
    ///
    /// 运行（lsdark-eos 实测，ACL `user:lsdark:rw-`）：
    /// `cargo test -p xcontrol-input --features wayland -- --ignored --nocapture`
    #[test]
    #[ignore = "uinput 实机冒烟（写真实虚拟设备事件）"]
    fn uinput_inject_smoke() {
        let status = uinput_status();
        assert_eq!(
            status,
            UinputStatus::Available,
            "uinput 不可用：{:?}",
            status.guidance()
        );
        let mut injector = UinputInjector::new((1920, 1080)).expect("设备创建");
        injector
            .inject(&InputEvent::Key {
                code: KEY_F15,
                state: KeyState::Down,
            })
            .expect("F15 down 注入");
        injector
            .inject(&InputEvent::Key {
                code: KEY_F15,
                state: KeyState::Up,
            })
            .expect("F15 up 注入");
        injector
            .inject(&InputEvent::MouseMoveRel { dx: 5.0, dy: -3.0 })
            .expect("相对移动注入");
        injector
            .inject(&InputEvent::Wheel {
                axis: Axis::Vertical,
                delta: 1.0,
            })
            .expect("滚轮注入");
        // 鼠标键（BTN_* 码位声明回归——check 11-19：EVDEV_TABLE 无 BTN 码，
        // 未声明时内核按 keybit 过滤注入，按钮必然无效）。
        injector
            .inject(&InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down,
            })
            .expect("左键 down 注入");
        injector
            .inject(&InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Up,
            })
            .expect("左键 up 注入");
        injector.release_all().expect("release_all");
        println!("uinput 注入冒烟完成（F15 + 相对移动 + 滚轮 + 左键；KDE 侧应为无感事件）");
    }

    /// Text 事件在 uinput 路径必须干净拒绝（CAP_UNICODE ❌ 能力协商依据）。
    #[test]
    #[ignore = "依赖 uinput 实机（同冒烟环境）"]
    fn uinput_text_rejected() {
        let mut injector = UinputInjector::new((1920, 1080)).expect("设备创建");
        let error = injector
            .inject(&InputEvent::Text {
                utf8: "汉字".into(),
            })
            .expect_err("Text 必拒");
        assert!(matches!(error, SinkError::Unsupported(_)));
    }

    /// 能力探测表实机（KDE 会话内运行）：KDE 行回填实测值。
    #[test]
    #[ignore = "DBus 会话内实机探测（Wayland 会话）"]
    fn capability_probe_live_kde() {
        let table = CapabilityTable::probe();
        let row = &table.rows[0];
        println!("KDE 行：{row:?}");
        println!("GNOME 行：{:?}", table.rows[1]);
        println!("wlroots 行：{:?}", table.rows[2]);
        assert_eq!(Compositor::detect(), Compositor::Kde, "非 KDE 会话环境");
        assert!(row.inject_uinput.is_available(), "uinput 权限探测");
        assert!(row.capture.is_available(), "InputCapture portal 探测");
        assert!(
            row.hotkey_portal.is_available(),
            "GlobalShortcuts portal 探测"
        );
    }

    /// uinput 自动标定构造实机（Wayland 会话 + `/dev/uinput` 可写；11-22 修复
    /// 轮）：ABS 域 = wl_output 真实分辨率并集；`cursor_normalized` 端点换算
    /// 冒烟（右缘 warp → 1.0、上缘 warp → 0.0——主控极值判定的回报契约）。
    #[test]
    #[ignore = "uinput 实机（Wayland 会话 + /dev/uinput 可写）"]
    fn uinput_auto_calibration_live() {
        let mut injector = UinputInjector::auto().expect("auto 构造（探测 + 设备创建）");
        injector.warp(0, 0.5, 0.5).expect("中心 warp");
        let (cx, cy) = injector.cursor_normalized().expect("中心采样");
        println!("中心采样：({cx}, {cy})（域 4K 时 ≈ 0.5001）");
        injector.warp(0, 1.0, 0.0).expect("右缘 warp");
        let (x, y) = injector.cursor_normalized().expect("端点采样");
        assert_eq!(x, 1.0, "右缘 warp 后归一化 x = 1.0（端点契约）");
        assert_eq!(y, 0.0, "上缘 warp 后归一化 y = 0.0（端点契约）");
        injector.release_all().expect("release_all");
        println!("uinput 自动标定冒烟完成（真实分辨率 ABS 域 + 端点采样）");
    }

    /// wl_output 枚举实机（Wayland 会话内运行；11-22 修复轮）：真实分辨率
    /// 并集打印——uinput ABS 域标定事实源的实测取证（4K 屏预期 (3840, 2160)）。
    #[test]
    #[ignore = "wl_output 实机探测（Wayland 会话；WAYLAND_DISPLAY/XDG_RUNTIME_DIR 在位）"]
    fn screen_probe_live_wayland() {
        let size = probe_screen_pixels();
        println!("wl_output 枚举结果：{size:?}");
        assert!(
            size.is_some(),
            "Wayland 会话内应枚举成功（SSH 非交互 shell 需显式导出 WAYLAND_DISPLAY/XDG_RUNTIME_DIR）"
        );
    }

    /// GlobalShortcuts 注册实测：KDE 弹授权对话框（在场即点；无人值守超时）。
    ///
    /// 实测配方见模块文档；`--ignored --nocapture`。
    #[test]
    #[ignore = "交互授权（对话框需用户在场）——无人值守传 3s 超时取证"]
    fn portal_hotkeys_bind_smoke() {
        let config = HotkeysConfig::default();
        match PortalHotkeys::start(&config, Duration::from_secs(3)) {
            Ok(mut hotkeys) => {
                println!("四热键注册成功（授权完成）；等待 5s 观察触发…");
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if let Some(id) = hotkeys.poll() {
                        println!("热键触发：{id:?}");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                hotkeys.stop().expect("停止");
            }
            Err(error @ PortalInitError::Timeout(_)) => {
                println!("授权超时（无人值守预期）——对话框移交用户在场测试：{error}");
            }
            Err(error) => panic!("portal 失败：{error}"),
        }
    }

    /// InputCapture 会话实测：zones + barriers + enable（授权对话框移交项）。
    #[test]
    #[ignore = "交互授权（对话框需用户在场）——无人值守传 3s 超时取证"]
    fn portal_capture_probe() {
        match PortalCaptureSource::start(Duration::from_secs(3)) {
            Ok(mut source) => {
                println!("捕获会话就绪（zones/barriers/enable 完成）；空转 1s 观察事件…");
                std::thread::sleep(Duration::from_millis(1000));
                let mut out = Vec::new();
                crate::InputSource::poll(&mut source, &mut out);
                println!("捕获事件数（空转应为 0）：{}", out.len());
                println!("mods 快照：{:?}", crate::InputSource::mods(&source));
                source.release(ScreenEdge::Right);
                source.stop().expect("停止");
            }
            Err(error @ PortalInitError::Timeout(_)) => {
                println!("授权超时（无人值守预期）：{error}");
            }
            Err(error) => panic!("portal 失败：{error}"),
        }
    }

    /// RemoteDesktop 注入实测：select_devices/start/connect_to_eis（授权移交）。
    #[test]
    #[ignore = "交互授权（对话框需用户在场）——无人值守传 3s 超时取证"]
    fn remote_desktop_probe() {
        match PortalInjector::start((1920, 1080), Duration::from_secs(3)) {
            Ok(mut injector) => {
                println!("注入会话就绪（授权完成）；释放资源…");
                injector.release_all().expect("release_all");
                injector.stop().expect("停止");
            }
            Err(error @ PortalInitError::Timeout(_)) => {
                println!("授权超时（无人值守预期）：{error}");
            }
            Err(error) => panic!("portal 失败：{error}"),
        }
    }
}
