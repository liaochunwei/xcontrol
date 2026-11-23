# 05 · 平台能力矩阵与实现路径

> 状态：草案 v0.1 · 基于 2026-11-15 调研（来源见各节链接，ADR-0003 记录关键选型）。本文是 `xcontrol-input` / `xcontrol-clipboard` 平台实现路径的**唯一事实源**；平台行为存疑时先更新本文再写代码。

## 0. 总矩阵（能力 × 平台）

| 能力 | macOS | Windows | Linux X11 | Linux Wayland (GNOME≥46 / KDE≥6.1) | Linux Wayland (wlroots: sway/Hyprland 等) |
| --- | --- | --- |---|---|--- |
| 输入捕获（主控） | ✅ CGEventTap | ✅ LL Hook | ✅ XRecord + XGrab* | ✅ InputCapture portal + reis | ⚠️ layer-shell 边缘窗口（近似） |
| 输入注入（从机） | ✅ CGEventPost | ✅ SendInput | ✅ XTEST | ✅ RemoteDesktop portal (EIS) 或 wlr 协议 | ✅ wlr-virtual-pointer + virtual-keyboard；兜底 uinput |
| 屏幕边缘/光标 | ✅ CGEventGetLocation | ✅ GetCursorPos | ✅ XQueryPointer | ✅ portal barrier 自带（Activated.cursor_position） | ⚠️ layer-shell 窗口进入即触发（无全局坐标） |
| 剪贴板读写 | ✅ arboard + NSPasteboard | ✅ arboard + win32 | ✅ arboard（3 selections） | ✅ wl-clipboard-rs / arboard data-control；GNOME 备选 clipboard portal | ✅ wl-clipboard-rs（wlr-data-control） |
| 剪贴板监听 | ⚠️ changeCount 轮询 | ✅ WM_CLIPBOARDUPDATE | ✅ XFixes | ✅ data-control 协议事件 | ✅ data-control 协议事件 |
| 全局热键 | ✅ RegisterEventHotKey | ✅ RegisterHotKey | ✅ XGrabKey | ✅ GlobalShortcuts portal（GNOME≥47） | ❌ → 回退：事件流识别/边缘双击/死角 |
| Unicode 直发 | ✅ | ✅ | ✅（keysym 合成） | ✅（EIS 路径）；uinput ❌ | ⚠️ virtual-keyboard 可，uinput ❌ |
| 安全码钥匙串（docs/07 §3.3） | ✅ Keychain（security-framework） | ✅ Credential Manager（windows-sys） | ✅ Secret Service（zbus 纯 Rust；无 DBus 会话回退 file 0600） | ✅ Secret Service（kwalletd/gnome-keyring 兼容层） | ✅ 同左；headless 服务器常无 DBus → file 0600 回退 |
| 所需权限 | 辅助功能（主从都要） | 无（提权 UIPI 见 §2.2） | 可访问 $DISPLAY | portal 会话授权（restore token） | 无 root；uinput 兜底需 input 组 |

**降级原则**：能力缺失组合必须在首次连接协商时上报（`Hello`/`HelloBack` 的 `caps` 掩码字段，见 [02-protocol.md](02-protocol.md) §4.3/§5），UI 明确提示该从机/主控的受限项（如"该从机不支持 Unicode 直发，将退回键码路径"），禁止静默失败。

### 0.1 Caps 位掩码定义（协议协商）

`Caps` 是 `u32` 位掩码，bit 从 0 起编号，**major=1 内只增不改不删**（[02-protocol.md](02-protocol.md) §5）。接收方**必须容忍未知 bit**——新 minor 引入的位在老对端视角一律忽略（按 0 处理），不因此拒连；发送端只发对端掩码声明过的能力（如剪贴板只发对端 `CAP_IMAGE` 声明的格式，docs/04 §2）。

| bit | 常量名 | 含义 |
| --- | --- | --- |
| 0 | `CAP_UNICODE` | Unicode 文本直发：可注入 `Text` 事件（[03-input.md](03-input.md) §4.5；uinput 路径为 ❌） |
| 1 | `CAP_IMAGE` | 剪贴板图片：`image/png` 收发（[04-clipboard.md](04-clipboard.md) §2） |
| 2 | `CAP_HTML` | 剪贴板 HTML：`text/html` 收发（[04-clipboard.md](04-clipboard.md) §2） |
| 3 | `CAP_ABS_POINTER` | 绝对指针映射：可接收 `MouseMoveAbs`（[02-protocol.md](02-protocol.md) §4.2） |

bit4+ 保留，只能随 minor 提升追加（新增位须在本文表格补录后再实现，协议即契约）。

> 修订记录（2026-11-16，任务 11-16-m1-proto）：本节为 v1 实装新增——原 §0 只述能力矩阵、未钉死协商位；`CAP_UNICODE`/`CAP_ABS_POINTER` 的语义取自 §0 能力矩阵行，`CAP_IMAGE`/`CAP_HTML` 取自 [04-clipboard.md](04-clipboard.md) §2 格式协商，位值与 `xcontrol-proto::Caps` 常量一致。

## 1. macOS

| 功能 | API / crate | 要点 |
| --- | --- | --- |
| 捕获 | `core-graphics::event::CGEventTap::new`（kCGSessionEventTap）+ `core-foundation` CFRunLoop；回调返回 `Remove` 可吞事件 | 必须辅助功能授权；用 `AXIsProcessTrustedWithOptions(kAXTrustedCheckOptionPrompt)` 检测并引导；未授权时 tap 回调被**静默忽略**（不报错）——权限检测必须显式做 |
| 注入 | `CGEvent` 构造 + `CGEventPost(kCGHIDEventTap)`（`core-graphics`；enigo 的 mac 后端同路径）；**指针定位（rel/abs/warp）走 `CGWarpMouseCursorPosition`**（11-19-m5-layout 实测修订，见 §1.1） | 修饰键用 `CGEventPost` 辅助事件或 flags 合成；Unicode 用 `CGEventKeyboardSetUnicodeString`；鼠标按键/滚轮仍走 CGEventPost（HID） |
| 剪贴板 | `arboard`（text/image，`image-data` feature）+ `objc2-app-kit` public.html **原样挂载**（11-19-m5-clipboard-full：clearContents + setString_forType(html, NSPasteboardTypeHTML)，0.3.2 绑定为安全方法**零 unsafe**——M2 预留的 allow(unsafe_code) 未被消耗；不经 arboard `set_html`——其模板包装破坏 fragment 读回等价性）；图片 wire PNG↔RGBA 经 `png_codec`（NSImage/TIFF 读回重编码 → 等价判据像素级，docs/04 §2） | changeCount 轮询 250ms（无系统通知 API） |
| 热键 | 捕获流识别（CGEventTap ListenOnly + `HotkeyFilter`；M4-hotkeys 裁定，docs/03 §4.6） | Carbon `RegisterEventHotKey` **不采用**——捕获识别已覆盖且少一条 Carbon 依赖；ListenOnly 无法吞事件，「不转发」= app 层过滤不产出转发事件（实测冒烟 `capture_hotkey_chain_smoke` PASS，2026-11-18） |
| 光标 | `CGEventGetLocation`（无需权限）+ tap 事件坐标 | 多屏 `CGGetDisplaysWithRect` |
| 打包 | 签名（自签起步）+ 固定路径安装（.app 移动会破坏 TCC 授权） | 权限引导页检测 TCC 状态（ad-hoc 自签脚本 `scripts/macos/codesign.sh` 与安装指引见 [10-install.md](10-install.md) §1） |

### 1.1 macOS 指针定位原语实测记录（11-19-m5-layout，2026-11-19）

FR-2.7 绝对坐标注入实测时发现：**HID-posted `MouseMoved` 对光标位置的施加在
本机（macOS 26 / Darwin 25.5.0，Apple Silicon）不可靠**。辅助功能已授权
（`check_accessibility` = Granted）条件下，MacInjector 全路径多轮探针：

| 原语 | 结果 | 证据 |
| --- | --- | --- |
| `CGEventPost(kCGHIDEventTap)` MouseMoved（绝对定位 + 现有 rel 读改写路径） | **0/3 精确命中**；相对路径 5×(+10,0) 仅移动 ≈2/5 步（净移动大量丢失） | 注入后 400ms 多点采样，目标位置从未出现；同一轮内对照原语精确命中，排除活机物理鼠标干扰 |
| `CGEventPost(kCGSessionEventTap)` MouseMoved | 2/3 精确（一轮 y 偏差 ≈52 点） | 同上 |
| `CGWarpMouseCursorPosition` | **3/3 亚像素命中**（整点量化 ≤0.5 点） | 主屏中心 (864, 558.5) → 实测 (864.0, 558.0)；屏原点 (0,0) → (0.0, 0.0) 精确 |

**裁定**：指针定位（`MouseMoveRel` 读改写 / `MouseMoveAbs` / `WarpPointer`）
统一改走 `CGWarpMouseCursorPosition`（"moves the mouse cursor without
generating events"，core-graphics 0.25 绑定）；鼠标**按键/滚轮**维持
`CGEventPost(kCGHIDEventTap)`（点击位置由 warp 后光标决定，本表未发现按键
投递异常）。实测证据固化在
`platform/macos.rs::inject_mouse_move_abs_centers_cursor_on_main_display`
（#[ignore]，授权后可复跑）。

**语义代价（warp 不生成事件，有意接受）**：受控光标移动不向前台应用投递
hover/drag NSEvent——控制语义（点击落点、位置追踪）不受影响；前台 hover
高亮不随受控光标刷新（视觉提示损失，M6+ 若需可补 Session tap 辅助事件）。
M2 的「相对移动经 MouseMoved 投递」行为自本修订起变更（本机行为本就不可靠，
净移动守恒契约不变）。

### 1.2 macOS 受控光标冻结实测记录（11-22 两机联调，锚定复活裁定）

受控期主控光标冻结的三条路径实测（macOS 26 / Darwin 25.5.0）：

| 路径 | 结果 | 依据 |
| --- | --- | --- |
| `CGDisplayHideCursor` | **失效**（非前台进程忽略） | 探针实测，光标可见 |
| `CGAssociateMouseAndMouseCursorPosition(false)` | 调用成功但**光标仍跟随物理鼠标** | 实测（重关联时"跳回物理鼠标位"的行为另见回切落点覆盖修复——associate 后需重放落点 warp） |
| Session tap（active）吞 `MouseMoved` | **防不住光标移动**（macOS 指针合成在 HID 层，Session tap 下游的吞事件不影响光标合成） | 实测 |

**唯一可达实现 = 每拍锚定 warp**（50ms 周期 `CGWarpMouseCursorPosition` 回
切入锚点——warp 原语 3/3 亚像素命中见 §1.1）：光标被周期性拉回锚点 = 视觉
静止。锚点取切入位置**内缩 90pt**（避边缘容差带防传感器边界打转，11-20）。
光标可见（无法隐藏）为已知代价；锚定 warp 不生成事件（§1.1），无输入副作用。

**撞缘回锚已删除（11-22 实验证伪）**：其前提"受控 delta 随系统钳位断流"不
成立——解关联后 HID 事件 delta 为**硬件增量**（与合成光标位置无关，无屏缘
钳位），受控光标移动不受锚定影响（对照实验：停用锚定后被控光标正常移动）。
回锚的可见跳变与其存在必要性一并移除。

## 2. Windows

| 功能 | API / crate | 要点 |
| --- | --- | --- |
| 捕获 | `windows` crate：`SetWindowsHookExW(WH_KEYBOARD_LL / WH_MOUSE_LL)` + 线程消息循环 | 无需管理员；吞事件＝不调 `CallNextHookEx`；钩子超时（LowLevelHooksTimeout）内必须返回，慢操作投线程池 |
| 注入 | `SendInput`（KEYBDINPUT/MOUSEINPUT，支持绝对/相对/滚轮/`KEYEVENTF_UNICODE`） | **UIPI**：注入提权进程（任务管理器等）需本进程提权；打包建议提供"以管理员运行"可选项并在 UI 说明（实测项见 §2.1 item 3） |
| 剪贴板 | `arboard` + win32 补 `CF_HTML`/`CF_DIB` | 监听 `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE`（隐藏消息窗口）；被占用 3×33ms 重试 |
| 热键 | `RegisterHotKey`（消息窗口线程） | — |
| 光标 | `GetCursorPos` + `GetSystemMetrics(SM_C/XVIRTUALSCREEN…)` | hook 事件自带坐标 |
| 打包 | MSI + 预配置防火墙入站规则（xcontrol 监听端口） | 首监听弹窗会被用户误拒 → 安装器预授权（预配置脚本 `scripts/windows/firewall.ps1` 与安装指引见 [10-install.md](10-install.md) §2） |

### 2.1 Windows 输入层运行时验收清单（M3 接线移交，11-17-m3-input-win）

本机无 Windows 时仅完成**编译级验证**（`cargo check -p xcontrol-input --features
windows --target x86_64-pc-windows-msvc`；无 MSVC 交叉环境需
`CC_x86_64_pc_windows_msvc=nonexistent-cc` 前缀——使 blake3 构建脚本回退纯 Rust
路径，见 crates/xcontrol-input/Cargo.toml 注释）+ 跨平台纯逻辑测试。以下**运行时
行为**须在有 Windows 机器时逐项实测并把结果回填本节（发布门禁，§7 输入行的
Windows 细化）：

1. **捕获**（`WinCaptureSource`）：
   - 键盘全锚点实击：字母/数字/符号（QWERTY 与一种非 US 布局各一轮）、方向键、
     右 Ctrl/右 Alt/左右 Win、小键盘全键（NumLock 两态）——确认扫描码→XKey 按物理
     键位（与布局无关，FR-2.6）；
   - 易混键四条归一化行为实测（winkeymap 模块文档钉死、官方表背书，但 kbdhid 驱动
     归一化按机型存在历史差异）：Pause=(0x45,无扩展)、NumLock=(0x45,E0)、
     ScrollLock=0x46 无扩展、PrintScreen=E0 37、Ctrl+Pause=E0 46、Alt+PrintScreen=0x54；
   - 媒体键（音量/播放/切歌）经 `WH_KEYBOARD_LL` 到达确认；
   - 长按重复：确认持续产生 Key Down（本层不产 KeyRepeat，docs/03 §3 可选项）；
   - 修饰键快照位与事件流一致（含左右手同族修饰键）。
2. **注入**（`WinInjector`）：
   - 键码路径（KEYEVENTF_SCANCODE）：前台记事本打字 100 字符无错漏（docs/05 §7）；
   - 文本路径（KEYEVENTF_UNICODE）：ASCII + 中文 + emoji（代理对）各一段；
   - 鼠标：相对移动（**关闭/开启"提高指针精确度"各测一轮**——相对注入受系统鼠标
     加速影响，模块文档已知限制）、绝对 warp（虚拟桌面归一化）、五键按钮、滚轮
     纵横（含高分滚轮小数格累加）；
   - `release_all` 防卡键：注入按住修饰键后强切会话，确认全部抬起。
3. **UIPI 提权场景**（docs/05 §2"无（提权 UIPI 见 §2.2）"行）：
   - 普通完整性级别进程：注入到**提权前台**（任务管理器）被静默丢弃且
     `SendInput` 返回成功——确认模块文档描述与实测一致；
   - 提权运行后重测同场景（应可注入）；
   - 捕获侧：提权前台窗口的键盘输入是否到达 LL hook（UIPI 隔离范围确认）。
4. **钩子超时行为**（LowLevelHooksTimeout）：压测回调线程（挂起消息泵模拟慢回调）
   ——确认系统摘除钩子后的表现（静默无事件），为 M4 热键拦截的超时防线设计取数。
5. **生命周期**：锁屏/解锁、切换用户会话（LL hook 随会话失效）、显示器热插拔后
   `refresh_layout`、捕获源 stop/start 重启幂等（单测已覆盖静态槽位，实机重复启停）。
6. **两进程对拍（主控↔从机全链路，11-17-m3-input-win check 补录）**：windows 捕获层
   丢弃一切 injected 事件（`LLKHF_INJECTED`/`LLMHF_INJECTED`，防回环防线，见
   `platform/windows.rs` 模块文档），同进程"注入→捕获"回环测试不可行——须两台实机
   （或两进程主从配对）验证"主控捕获→转发→从机注入→从机前台生效"全链路。注入侧
   事件带 injected 标记、不进入从机自身捕获流属**预期行为**（从机事件流热键识别只
   响应该机物理键鼠；与 mac 后端 CGEventTap 不过滤自注入的语义差异同见模块文档）。
7. **全局热键（`WinHotkeySource` RegisterHotKey，11-18-m4-hotkeys；编译级已过
   `--target x86_64-pc-windows-msvc` check + clippy）**：有 Windows 机器时实测——
   ① 默认四键按下 → `WM_HOTKEY` → `HotkeyId` 到达 drain（消息窗口线程，两端
   同型）；② 组合被前台应用消费确认（焦点记事本输入 `Ctrl+Alt+P` 无字符落入——
   OS 级吞键语义）；③ 组合被其他程序占用时的注册失败报错路径（fail loud）；④
   `MOD_NOREPEAT` 按住不重复确认（历史系统降级：按住重复 toggle 可接受）；⑤
   与 LL hook 并存：热键组合在捕获流中到达 + app 层过滤不转发（双保险，docs/03
   §4.6 win 行裁定）。

实测完成后：结果写入本节（通过/偏差/驱动版本），偏差项回改 winkeymap.rs 表与
platform/windows.rs 文档（文档先行）。

### 2.2 Windows 剪贴板运行时验收清单（M3 接线移交，11-17-m3-clipboard-win-x11）

本机无 Windows 时完成**编译级验证**（`cargo check -p xcontrol-clipboard --features
windows --target x86_64-pc-windows-msvc`，blake3 交叉编译配方同 §2.1 前缀
`CC_x86_64_pc_windows_msvc=nonexistent-cc`）+ 跨平台纯逻辑测试（newline.rs 换行
规范化 / 重试策略常量，mac/linux 开发机与 CI 均可跑）。CI windows runner 已接线
`--features xcontrol-clipboard/windows`（clippy + test，监听生命周期测试在 runner
用户会话运行；写剪贴板冒烟 `#[ignore]`）。以下**运行时行为**须在有 Windows 机器
时逐项实测并把结果回填本节（发布门禁，§2 剪贴板行的细化）：

1. **监听（`WM_CLIPBOARDUPDATE`）**：外部应用复制文本 → 事件驱动投递（非轮询，
   延迟应显著低于 mac 250ms 轮询档）；自身进程写入 → 事件照常到达（防回环由
   `EchoFilter` 防线 1/3 承接，同 mac 语义）；监听窗口为 message-only（无任务栏/
   无前台窗口副作用）；前台应用复制期间（剪贴板被占用窗口）监听不丢事件。
2. **3×33ms 占用重试**（docs/04 §4 参数行）：并发占用者（剪贴板查看器/杀毒/
   输入法常驻类）存在时 `read_text`/`write_text` 经重试成功；持续占用 >100ms 的
   场景确认错误干净上抛（`ClipboardError::Platform`，无 panic/无重试风暴）。与
   arboard 内层 5×5ms 重试叠加后的总等待时长可接受（<150ms 量级）。
3. **换行语义**（docs/04 §2）：写 `\n`→`\r\n` 落板（记事本等 CRLF 敏感应用粘贴
   换行正确）；读 CRLF→LF 归一化（CRLF/LF/CR 混合、emoji、超长文本各一轮，与
   newline.rs 单测矩阵对拍）；**自写回声识别**：本进程写入 → 监听事件 →
   EchoFilter 判回声不外发（防线 1 指纹按归一化 LF 文本，round-trip 不动点，
   newline.rs 模块文档）——这是"Windows 指纹字节教训"的实测确认项。
4. **UIPI 与监听范围**：提权前台应用（任务管理器）复制 → 普通完整性监听进程
   收到 `WM_CLIPBOARDUPDATE`（高→低完整性方向不被过滤，模块文档预判）；
   普通应用向**提权**进程粘贴不受本层影响（剪贴板是内核对象非窗口消息）。
5. **生命周期**：锁屏/解锁、切换用户会话（监听窗口随会话失效 → 恢复后重建/
   降级表现）、远程桌面会话切换、watcher start/stop 重复启停幂等（实机重复
   起停，同 §2.1 item 5）。
6. **多格式**（M5 落地编译级，11-19-m5-clipboard-full）：图片/HTML 读写路径已
   实装（arboard `image-data`：CF_DIB↔RGBA 经本仓 png_codec；HTML 经 arboard
   clipboard-win CF_HTML 包装）——`cargo check/clippy -p xcontrol-clipboard
   --features windows --target x86_64-pc-windows-msvc` 绿（2026-11-19）。CF_HTML
   读回提取（StartFragment/EndFragment）与写入 fragment 的字节等价性、CF_DIB
   像素 round-trip 须实机复核并把结果回填（回合冒烟
   `cargo test -p xcontrol-clipboard --features windows -- --ignored`）。

## 3. Linux X11

| 功能 | API / crate | 要点 |
| --- | --- | --- |
| 捕获 | `x11rb`（feature `record`）XRecord 被动监听；受控切换期用 `grab_pointer/grab_keyboard` 隐藏本机输入 | X11 无安全边界：任何 DISPLAY 客户端都能监听全局输入（本项目依赖此特性，也是 Wayland 封杀它的原因） |
| 注入 | `x11rb`（feature `xtest`）`xtest::fake_input` | **必须 XTEST**；`XSendEvent` 合成事件带 `send_event=True` 标记，被大量工具包按 ICCCM 忽略；注意 fake_input 参数是 **keycode 非 keysym** |
| 剪贴板 | `arboard`（Clipboard/Primary/Secondary） | 监听 `XFixesSelectSelectionInput` → SelectionNotify；需常驻进程持有 selection 响应 paste（arboard `wait()`） |
| 热键 | `x11rb` `grab_key`（被动抓取） | — |
| 光标 | `XQueryPointer`（root） | — |
| 键码 | keycode = evdev `KEY_*` + 8（libinput/xf86-evdev 直传，全量无例外） | `KEY_*` ↔ HID 全表（与 Wayland(uinput) 行共用；见 [03-input.md](03-input.md) §2 修订记录——keycode-8 是 evdev 编号非 HID usage） |

### 3.1 X11 输入层运行时验收清单（M3 接线移交，11-17-m3-input-x11）

本机（macOS）完成**编译级验证**（`cargo check -p xcontrol-input --features x11
--target x86_64-unknown-linux-gnu`）+ 跨平台纯逻辑测试；**远端 Linux 实测已完成
第一轮**（2026-11-17，lsdark-eos：Arch x86_64，KDE Plasma Wayland 会话，Xorg :0 =
sddm greeter（root 持 xauth，SSH 不可连）+ Xwayland :1 rootless + 自建 rootful
Xwayland :2 作回环载体；SSH tty 无 DISPLAY，需显式 `DISPLAY=:…`；:1 还需
`XAUTHORITY=/run/user/1000/xauth_*`）。实测配方与证据见 platform/x11.rs 模块文档
"远端实测配方与结果"一节。**已实测项**：

- ✅ **回环（核心验收）**：rootful Xwayland :2 上 `capture_inject_loopback_smoke`
  PASS——XTEST 注入 F15 → XRecord 收到 Key{F15,Down/Up}，全链路（context 创建/
  enable/迭代解析/keycode→XKey）成立；注入按键/按钮往返、绝对 warp、`release_all`
  同轮通过；默认套件 65 项全绿（Xwayland :1 与 :2 双跑）；远端原生 clippy `-D
  warnings` 绿。
- ✅ **时序修正（实测发现并修复）**：start 后立即 stop 存在跨连接 enable/disable
  竞态——disable 先于 enable 到达服务器时"先禁后启"、context 保持启用、事件线程
  join 挂死；修正为就绪握手以服务器 **StartOfData 首应答**为凭证（platform/x11.rs
  `run_record_thread` 时序注释），`capture_restart_after_stop_is_clean` 压测通过。
- ⚠️ **Xwayland :1 行为差异（移交 M5 取数，item 5 已有首轮数据）**：XTEST 注入调用面
  全部成功（warp/key/button），但注入事件经 compositor（KWin）路由到当前聚焦的
  Wayland 原生表面——X 客户端与 XRecord 均收不到；xdotool 独立实现交叉验证同行为
  （排除本模块注入路径问题）。焦点在 Xwayland 窗口（compositor 激活）时预期与真
  Xorg 一致，待桌面端确认。

以下**运行时行为**须逐项实测并把结果回填本节（发布门禁，§7 输入行的 X11 细化）：

1. **grab 隐藏本机输入**（`hide_local_input`/`restore_local_input`，11-17-m3-input-x11
   最小实现——grab 本体 + 行为文档；**未实测**：无人值守跑会锁住用户键鼠，需操作员
   在场）：**XRecord 与 grab 的交互是核心未知项**：
   - grab_pointer（event_mask=0）+ grab_keyboard 后：本机本地应用是否确实收不到
     键鼠（隐藏语义验证）；
   - grab 期间 XRecord 事件流是否仍收到被吞事件——**决定主控受控期的事件来源架构**：
     若被吞事件不进 record 流，需补"从 grab 连接读事件"路径（Synergy/Barrier 做法，
     platform/x11.rs 模块文档已预留此设计分叉点）；
   - grab/ungrab 往返幂等、restore 后本机输入恢复正常、他客户端持有 grab 时的报错路径；
   - XGrabKeyboard 积压事件的内存上界实测（受控期持续打字，≈32B/事件）。
2. **捕获**（`X11CaptureSource`；链路已由回环验证，以下为物理键全锚点实击）：
   键盘全锚点实击（字母/数字/符号/方向键/左右修饰键/小键盘两态/媒体键）确认
   keycode→XKey 按物理键位（FR-2.6，x11keymap 全表实测对拍；异常键注意：真实 USB
   键盘 Menu 键落 keycode 138（KEY_PROPS）——捕获侧已按 Menu 语义上报，实测确认）；
   鼠标五键 + 滚轮纵/横（X 按钮 4-7）；修饰键快照位与事件流一致（左右手分开，
   X state 掩码不做此语义——模块文档）。
3. **注入**（`X11Injector`；调用面/回环已验证，以下为前台应用语义验证）：
   - 键码路径（XTEST keycode=evdev+8）：前台文本框打字 100 字符无错漏；多键盘布局
     （非 US）各一轮——XTEST keycode 语义物理键位，与布局无关；
   - 鼠标：相对移动（XTEST 相对增量 vs 系统加速——X11 无"提高指针精确度"等效项，
     确认比例失真有无）；绝对 warp（root 并集坐标，✅ 已实测）；五键按钮、滚轮纵横
     （4/5/6/7）；
   - Text 路径（latin-1 keysym 反查 keycode；keymap 表已在 Xwayland 构建成功）：
     ASCII + latin-1 扩展区（各布局可达性实测）+ CJK 确认整段拒绝
     （`SinkError::Unsupported`，✅ 已实测拒绝路径）；
   - `release_all` 防卡键：注入按住修饰键后强切会话，确认全部抬起。
4. **两进程对拍**（主控↔从机全链路）：X11 捕获不过滤自注入（X 协议无 injected 标记，
   与 Windows 后端有意不同，platform/x11.rs 模块文档）——同进程回环 ✅ 已实测通过，
   **全链路需两进程**（主控捕获→转发→从机注入→从机前台生效）验证，并实测**双角色
   同机**（同进程既捕获又注入）的回环风险边界（上层角色互斥是防线，docs/05 §3
   捕获行）。
5. **XWayland vs Xorg 行为差异**（移交 M5 Wayland 判断的取数；首轮见上 ⚠️ 项）：
   - XTEST 注入在 Xwayland（rootless）下对 Wayland 原生窗口的实际生效范围（✅ 首轮：
     调用面成功、事件达 Wayland 侧、X 侧不可见——具体到"聚焦 Xwayland 窗口时是否
     回到 X 侧"待桌面端确认）；
   - XRecord 在 Xwayland 下对 Wayland 原生应用的键盘事件可见性（rootless Xwayland
     只服务 X 客户端——首轮数据支持"不可见"，待聚焦场景确认）；
   - KDE Plasma Wayland 会话上 Xwayland 的 keycode 传递链（libinput→compositor→
     Xwayland）是否保持 evdev+8 不变（xmodmap 锚点已吻合——38=a/50=LShift/62=RShift/
     104=KPEnter/133=SuperL 等）。
6. **生命周期**：锁屏/解锁、$DISPLAY 断开重连（服务器死亡 → 捕获静默终止的降级
   表现）、显示器热插拔后 `refresh_layout`、捕获源 stop/start 重启幂等（✅ 已实测：
   立即 stop 压测 + 二次启停通过）。
7. **全局热键（`X11HotkeySource` grab_key，11-18-m4-hotkeys 首轮实测，2026-11-18）**：
   - ✅ **grab 回环（核心验收）**：rootful Xwayland :2 上 `hotkey_grab_loopback_smoke`
     PASS——注册默认四键（8 锁定变体/组合）→ XTEST 合成 Ctrl+Alt+Right → 被动
     grab 触发 → 事件线程收到 KeyPress → `HotkeyId::Next` 投递（全链路：连接/停机
     窗/atom/8 变体注册/状态剥离查表成立）；注册/注销往返 + 二次注册（窗口 id 段
     复用，双端 sync 纪律）通过；默认套件 71 项（:1/:2 双跑）+ ignored 5 项全绿；
     远端原生 clippy `-D warnings` 绿；
   - ✅ **按住不重复（check 补测，2026-11-18）**：XKB autorepeat 对 grab 按住的
     主键持续交付重复 KeyPress（:2 按住 1.5s 实测 22 次——首轮实装无防线，按住
     热键会连环路由，违 docs/03 §4.6「防按住连环切换」）。修复 = 事件循环 held
     位集去重（每 keycode 首次 KeyPress 投递、KeyRelease 清位——与 win
     `MOD_NOREPEAT` / mac 流过滤吞尾同语义）；回归防线 `hotkey_grab_hold_no_repeat`
     （按住 1.5s 断言恰好一次命中）实测 PASS；
   - 环境注意：SSH 会话连用户真实 Xwayland :1 需显式
     `XAUTHORITY=/run/user/1000/xauth_*`（kwin 会话 cookie 归该文件，仅 $DISPLAY
     不够——"Authorization required" 即此因）；:2 自建 rootful server 用
     `xauth -f /tmp/x2.auth add :2 . $(mcookie)` 生成 cookie。
   - ⚠️ **rootless Xwayland :1 限制（与 item 5 XTEST 注入不可见同源，移交 M5）**：
     compositor（KWin）先行处理键盘，X 客户端对 Wayland 原生焦点窗口的 passive
     grab 预期同样不触发——grab 载体为 X 客户端焦点（真 Xorg / rootful Xwayland /
     聚焦 Xwayland 窗口时预期与真 Xorg 一致，待桌面端确认）；Wayland 原生热键
     （GlobalShortcuts portal / 事件流识别回退）属 Wayland 里程碑路径；
   - 实测配方：`DISPLAY=:2 cargo test -p xcontrol-input --features x11 -- --ignored`
     （回环冒烟）+ 默认套件（`hotkey_register_stop_roundtrip` 随套件运行）；测后
     `pkill -f "Xwayland :2"` 清理。

实测完成后：结果写入本节（通过/偏差/环境版本），偏差项回改 x11keymap.rs 表与
platform/x11.rs 文档（文档先行）。

### 3.2 X11 剪贴板运行时验收清单（M3 远端实测已完成第一轮，11-17-m3-clipboard-win-x11）

本机（macOS）完成**编译级验证**（`cargo check/clippy -p xcontrol-clipboard
--features x11 --target x86_64-unknown-linux-gnu`，x11rb 纯 Rust 无系统库依赖）；
**远端 Linux 实测已完成第一轮**（2026-11-17，lsdark-eos：Arch x86_64，KDE Plasma
Wayland 会话，环境配方同 §3.1——Xwayland :1 rootless + 自建 rootful Xwayland :2
作回环载体，测后已清理 :2；SSH tty 无 DISPLAY 需显式 `DISPLAY=:…`）。实测配方
与证据见 crates/xcontrol-clipboard/src/platform/x11.rs 模块文档"远端实测配方"
一节。**已实测项**：

- ✅ **监听 round-trip（核心验收）**：rootful Xwayland :2 上
  `write_watch_echo_round_trip_smoke` PASS——本进程写入（arboard set owner）→
  XFixes `SelectionNotify` 到达监听线程（计数推进）→ 读回还原 → `EchoFilter`
  防线 3（静默窗内判回声）与防线 1（静默窗外指纹一致判回声、异内容判外部）
  全链路成立；
- ✅ **独立客户端持有语义**：`external_write_detected_by_watcher_smoke` PASS——
  xclip（独立进程）写入 CLIPBOARD → 监听线程收到外部 owner 变化 → 本进程经
  selection 请求协议读回 xclip payload（"常驻持有 selection 响应对端 paste"
  的独立客户端交叉验证）；
- ✅ **默认套件 21 项全绿**（Xwayland :1 与 :2 双跑；连接类测试按 $DISPLAY 容错
  跳过机制在 CI 无 X 环境下同样成立）；远端原生 clippy `-D warnings` 绿；
- ✅ **时序修正（实测发现并修复）**：连续 start/stop 压测（`watcher_restart_stress`
  ——停机连接 + 工作线程连接、X 服务器跨客户端请求无顺序保证）复现 join 挂死：
  工作线程 unsync 的 `destroy_window` 与下一监听器复用被回收窗口 id 段的
  `create_window` 竞态，destroy 后到连带销毁新窗口 → 下一次停机 ClientMessage
  落空（SendEvent BadWindow 静默）→ join 挂死。修正为**双端 sync**（工作线程
  destroy 后 sync 才退出 + 停机连接 send 后 sync 才 join），压测转绿；
- ⚠️ **Primary selection 不同步**（P2，docs/04 §4）：本层只走 CLIPBOARD，中键
  粘贴路径未监听未读写（设计如此，非缺陷）；
- ⚠️ **进程退出内容丢失提示（移交 M4/M5）**：无剪贴板管理器环境退出 XControl
  后 CLIPBOARD 无主——arboard Drop 的 `CLIPBOARD_MANAGER` 临终转交是尽力而为
  （管理器在场才生效，arboard 3.7 内建）；用户提示属 UI 层（docs/04 §4），
  转交加固评估留 M5（platform/x11.rs 模块文档"常驻持有语义"一节）。

以下**运行时行为**须逐项实测并把结果回填本节（发布门禁，§3 剪贴板行细化）：

1. **真 Xorg 会话对拍**（环境限制同 §3.1：Xorg :0 的 xauth 归 sddm，SSH 不可连，
   需桌面侧操作）：XFixes 监听与 selection 请求往返在真 Xorg 上复测（Xwayland
   :2 行为与真 Xorg 预期一致，待确认）；KDE Plasma Wayland 会话上经 :1 的
   CLIPBOARD 镜像与 Wayland 剪贴板（Klipper）的双向行为。
2. **剪贴板管理器在场场景**：Klipper/clipman 等在场时退出 XControl →
   CLIPBOARD_MANAGER 临终转交实际生效（内容存活）；不在场时确认内容丢失且
   UI 提示路径可挂接。
3. **多格式**（M5 落地 + 远端回归，11-19-m5-clipboard-full）：图片/HTML 读写已
   实装（arboard `image-data`：`image/png` target 经本仓 png_codec，`text/html`
   target 经 arboard `set().html()` **原样存储**无模板包装）；Xwayland :1 默认
   套件回归 27 项全绿（2026-11-19，M3 的 21 项 + 连接类新增），`image/png`/
   `text/html` 读写冒烟与 TARGETS 协商为 `#[ignore]` 项（写 CLIPBOARD，
   `cargo test -p xcontrol-clipboard --features x11 -- --ignored --nocapture`
   人工运行/远端配方见 platform/x11.rs 模块文档）。
4. **生命周期**：$DISPLAY 断开重连（服务器死亡 → 监听线程静默终止的降级表现、
   arboard 会话失效的恢复路径）、watcher 长跑（数小时挂监听无泄漏——线程数/
   fd 数稳定）。

实测完成后：结果写入本节（通过/偏差/环境版本），偏差项回改 platform/x11.rs
文档（文档先行）。

## 4. Linux Wayland（按 compositor 三路径）

### 4.1 GNOME ≥46 / KDE ≥6.1（portal 路径，首选）

| 功能 | 路径 |
| --- | --- |
| 捕获 | `ashpd`（feature `input_capture`）：`create_session`(KEYBOARD+POINTER) → `zones()` → `set_pointer_barriers()`（边缘 barrier，BarrierID≠0）→ `enable()`；光标撞 barrier → `Activated` 信号（带 cursor_position），事件经 `connect_to_eis()` 的 fd 用 **`reis`**（纯 Rust libei，`ContextType::Receiver`）接收。`release()` 回位时坐标向屏障**内侧偏 1px** |
| 注入 | `ashpd` `remote_desktop` `connect_to_eis` + `reis`（Sender 角色）；portal 对话框授权一次，**restore token 持久化**免重复授权 |
| 剪贴板 | `wl-clipboard-rs`（ext-data-control / wlr-data-control，Watcher 事件）；GNOME 对 data-control 支持存疑 → 运行时探测，失败退 **ashpd clipboard portal** |
| 热键 | `ashpd` `global_shortcuts`（KDE ✅；GNOME ≥47 ✅）；不支持则走 §5 回退 |

已知坑：GNOME 46.0 barrier 启用后不可更新（升级 zones 需重建会话）；GNOME<51 的 InputCapture 只有 v1、**不能共享剪贴板**（Deskflow 同样受限）→ 能力协商上报。

### 4.2 wlroots 系（sway / Hyprland / Wayfire / labwc）

| 功能 | 路径 |
| --- | --- |
| 捕获 | **layer-shell 1px 边缘窗口**（`wayland-client` + `wlr-layer-shell`，lan-mouse 同款）：指针进入边缘窗口即触发接管；无全局坐标语义，`entry_pos` 用进入事件的局部坐标近似 |
| 注入 | `wlr-virtual-pointer-unstable-v1` + `zwp_virtual_keyboard_v1`（`wayland-protocols` wlr feature / sctk），无需 root；sway 兼容性存疑点见下 |
| 剪贴板 | `wl-clipboard-rs`（wlr-data-control，协议发源于 wlroots，最稳） |
| 热键 | ❌ GlobalShortcuts 多数不支持 → §5 回退（主控事件流内识别 + 边缘双击 + 死角） |

实测要求：sway 1.11 对 virtual-pointer 的支持存在社区矛盾信息（wayland.app 矩阵标 ✗ 但 wayvnc/wlrctl 依赖它）——**M5 首周实测 sway/Hyprland 并把结果固化为本文附录的实测表**。

### 4.3 兜底：uinput（任意 compositor / 无桌面）

- `evdev` crate `evdev::uinput` 模块建虚拟键鼠，compositor 视为真实硬件；同时适用于 **headless 从机**（NFR-4）。
- 代价：需 `input` 组/root（安装器/文档配置 udev 规则）；绕过 compositor 安全模型；**无 Unicode 直发**（能力协商上报，Text 事件走键码近似或拒绝）。
- lan-mouse 未采用此路径作为默认，本项目仅作显式 opt-in 的兜底（设置项"注入后端：portal/wlr/uinput/auto"）。

### 4.4 Wayland 剪贴板实装记录（11-19-m5-clipboard-full，2026-11-19）

- **选型落地**：`wl-clipboard-rs` **0.9.5** 直依赖（非 arboard `wayland-data-control`
  ——mime 体系与 wire 同构，image/html 走 mime 字节直通零转换；arboard 无监听
  能力）。**版本硬依据**：watch 模块（`Watcher`/`ClipboardEvent`/`CancelHandle`）
  2026-09 落 master、0.9.5 首发——0.9.3 及以下无 watch，事件驱动监听不可达
  （docs.rs + crates.io 查证）。ext-data-control 优先、wlr-data-control 兜底
  （0.9.2 起）；纯 Rust Wayland 客户端（native_lib feature 不开），crate 内
  deny(unsafe_code)。
- **坑位钉死**：copy `trim_newline` 默认 true（对 Source::Bytes 文本裁尾换行 →
  指纹失配）——写路径一律 `Options::trim_newline(false)`（platform/wayland.rs
  模块文档）；copy 非 foreground 模式起 serve 线程（0.9.3 起无 fork，多线程进程
  安全）。
- **远端 KDE 实测（lsdark-eos，Arch x86_64 + KDE Plasma Wayland，KWin；
  `WAYLAND_DISPLAY=wayland-0`——注意本任务前登记的 wayland-1 已不存在，
  socket 以 `$XDG_RUNTIME_DIR` 实际为准；XDG_RUNTIME_DIR=/run/user/1000
  显式注入）**：
  - ✅ **文本 round-trip（核心验收）**：`write_watch_echo_round_trip_smoke`
    PASS——copy_multi 写入（text/plain;charset=utf-8）→ data-control selection
    变化事件（计数推进）→ paste 读回逐字节还原 → EchoFilter 防线 3/1 全链路
    成立；
  - ✅ **image/html 能力实测**：`image_html_round_trip_smoke` PASS——`image/png`
    与 `text/html` mime 直通读写**字节级等价**（零转换路径）；能力探测
    （`capability_probe_mime_types`）记录 offered mime = `["text/plain;charset=utf-8",
    "text/plain", "application/x-kde-onlyReplaceEmpty"]`（KDE Klipper 在场，
    data-control 协议可用凭证 = Watcher::new 成功）；
  - ✅ 默认套件 28 项全绿；远端原生 clippy `-D warnings` 绿；X11 face 回归
    27 项全绿（同机 :1）。
- **GNOME 降级**：data-control 缺失 → `Error::MissingProtocol` 干净上抛，上层
  自动降级提示（ashpd clipboard portal 路径为移交项，P2）。
- 实测配方：`ssh … 'XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0
  cargo test -p xcontrol-clipboard --features wayland [-- --ignored --nocapture]'`
  （platform/wayland.rs 模块文档同款）。

### 4.5 Wayland 输入实装记录（11-19-m5-wayland，2026-11-19）

- **选型落地**（xcontrol-input `platform/wayland.rs`，wayland feature + target=linux
  双门控）：
  - **注入主路径 = uinput（`evdev` 0.13.2）**：`VirtualDevice::builder()` 建虚拟
    键鼠（键盘全表 = x11keymap::EVDEV_TABLE 同源复用、REL_X/Y/WHEEL/HWHEEL、
    BTN 五键、ABS_X/Y warp 轴）；XKey→evdev 翻译复用 keycode = evdev+8 全表。
    **Text 无路径**（CAP_UNICODE ❌，能力协商上报）。PRD 提及的 "rei crate" 经
    docs.rs/crates.io 查证不存在对应 uinput 能力（docs/01 §6 的 "rei" 系
    **reis 之误**——libei 协议实现，非 uinput 封装），兜底路径按本节 §4.3 取
    `evdev`。
  - **注入备选 = RemoteDesktop portal（ashpd 0.13.13 + reis 0.7.1 Sender）**：
    授权一次 + restore token；EIS Text 接口 Unicode 直发（CAP_UNICODE ✅）。
    设备协商：`ei_seat.capability` (interface, mask) 映射 → Done 后裸代理
    `bind(mask)` + `request_device(mask)`（reis 高层 Seat 只暴露 bind，
    request_device 需裸 `ei::Seat`——源码核实裁定）→ `DeviceAdded` 后按
    `interface::<ei::Pointer|Keyboard|Button|Scroll|PointerAbsolute|Text>()` 发送，
    每条注入 start_emulating → 事件 → frame → stop_emulating 批次。
  - **捕获 = InputCapture portal（ashpd + reis Receiver）**：create_session →
    connect_to_eis → zones → 四边 barrier（每 zone）→ enable；Activated 信号带
    activation_id/cursor_position，release 回位坐标屏障内侧偏 1px；EIS 事件
    （KeyboardKey=Linux evdev 码 / Button=BTN 码 / PointerMotion / ScrollDiscrete）
    → proto InputEvent（evdev↔XKey 反查同表）。GNOME 46 disable 后不可
    re-enable 的坑位继承（docs/05 §8）。
  - **热键 = GlobalShortcuts portal**：四热键按 config 便携串
    （`Ctrl+Alt+Right`）注册 preferred_trigger；Activated 信号按 shortcut_id
    （next/prev/back_to_host/pause）路由。**手势回退**（docs/06 §2 设置开关
    `[hotkeys] gesture_fallback`）：GestureSensor 纯逻辑（边缘双击 500ms 时间窗
    + 角落悬停 700ms 位置窗，时间注入可全量单测）。
  - **C-2 能力探测表**：`CapabilityTable::probe()`——compositor 识别
    （XDG_CURRENT_DESKTOP）+ portal 代理非交互探测（代理创建 + 属性读取，
    不触发授权对话框）+ uinput 权限探测；KDE/GNOME/wlroots 三行 ×
    capture/inject(uinput/portal)/hotkey/clipboard 六列，三态
    Available/Unavailable/Untested（诚实标注未实测行，UI 降级提示依据 docs/06 §4）。
- **异步边界裁定**：portal DBus 调用只在模块内控制线程的阻塞执行器上泵动
  （mdns-sd 自带线程先例）；事件路径 reis 同步核心 API
  （ei_handshake_blocking + Context::read/pending_event）+ std mpsc——回调线程
  纪律（platform-code.md）零 async 保持。依赖增量：ashpd 0.13.13（gpui 传递树
  同版，零新增版本）+ reis 0.7.1 + evdev 0.13.2 + tokio/futures-util/tracing
  （均已在 lock 或纯门面）+ nix 0.29/bitvec 系（evdev 传递）。
  **执行器修订（11-19-m6-release，打包实测发现）**：portal 执行器由
  "tokio current-thread runtime + ashpd `tokio` feature" 切换为
  "`async_io::block_on` + ashpd `async-io` feature"——gpui-base（gpui-pre-linux）
  自身启用 ashpd `async-io`，feature 统一后两执行器并存触发 ashpd
  `compile_error!`；app 构建（打包面）首次把两者拉进同一依赖图而暴露。
  超时竞速/多路事件选择以 `race_timeout`/`futures_util::select!` 对位替换
  （Timeout 契约语义不变）；tokio 依赖保留（仅 mpsc 命令通道——运行时无关原语）。
- **远端环境探测（lsdark-eos，Arch + KDE Plasma 6.7.4 Wayland，实现前实测）**：
  `/dev/uinput` ACL `user:lsdark:rw-`（getfacl 实测——groups 无 input 组，ACL
  来自 udev 附加，通用机器不可假设）；xdg-desktop-portal 1.22.1 +
  xdg-desktop-portal-kde 6.7.4-2（kde.portal 声明 InputCapture/GlobalShortcuts/
  RemoteDesktop/Clipboard 四 impl 接口）；busctl 实测 InputCapture
  CreateSession2/ConnectToEIS/SetPointerBarriers + GlobalShortcuts
  BindShortcuts 齐备；libei 1.6.0。
- **实测结果（2026-11-19 回填，lsdark-eos 原生）**：
  - 默认套件 **70 项全绿**（纯逻辑 + 权限探测，`cargo test -p xcontrol-input
    --features wayland`）；
  - **uinput 注入冒烟 PASS**：ACL 授权下虚拟键鼠创建 + F15 down/up + 相对移动 +
    滚轮 + release_all 无错；`Text` 干净拒绝（`Unsupported`，能力协商依据）；
  - **C-2 探测表 KDE 行全 Available（实测回填）**：InputCapture portal v2
    （Keyboard|Pointer|Touchscreen）· RemoteDesktop portal v2 · GlobalShortcuts
    portal v2 · Clipboard portal v1 · uinput 可写；
  - **GlobalShortcuts = 应用身份门槛（新发现）**：无人值守绑定被 KDE portal
    拒绝——`NotAllowed: An app id is required`；KDE 要求调用方有匹配的
    `.desktop` 桌面身份（打包/安装前置，docs/10 面）——授权对话框流程随
    桌面身份安装后移交用户在场实测；
  - **InputCapture = 授权对话框交互项**：无人值守 create_session 超时（3s，
    对话框等待取证）；授权后全链路（激活→EIS 事件→release 回位）移交；
  - **RemoteDesktop 会话建立 = 授权对话框交互项（check 复跑修订
    2026-11-19）**：首测曾记录"create_session → select_devices → start →
    connect_to_eis 无人值守 3s 内全通"，check 复跑两次（并行/串行）均卡授权
    3s 超时——不阻断结论不可复现（或系首测时用户在场点击），按超时取证口径
    归移交项；**无人值守三 portal 项统一形态 = `PortalInitError::Timeout`**
    （复跑确认），授权后实际注入落屏验证移交用户在场。

## 5. 全局热键统一策略（FR-2.5）

优先级从高到低，运行时按能力自动选择，UI 显示当前生效方式：

1. **OS 热键 API**：mac RegisterEventHotKey / win RegisterHotKey / x11 grab_key / wayland GlobalShortcuts portal（可用时）。（M4-hotkeys 裁定：mac 实装直接取本表次优先级「事件流内识别」——捕获识别已覆盖且少一条 Carbon 依赖，docs/03 §4.6；win/x11 各取 RegisterHotKey / grab_key。）
2. **事件流内识别**（零依赖，主控与受控态从机天然可用）：本机捕获层按 [03-input.md](03-input.md) §4.6 的默认组合键匹配原始 XKey 流——注意这只覆盖"本机键盘事件流可达"的角色（主控恒可；从机在受控态可，非受控态不可）。
3. **手势回退**（Synergy 语义）：边缘双击切换（500ms 内两次撞同一边缘）、屏幕角落悬停（dead corner 触发锁定/解锁）。

### 5.1 安全码钥匙串实装记录（11-19-m6-security，docs/07 §3.3）

- **统一接口**：`keyring` crate（xcontrol-config `keychain` feature 门控，默认关——proto/core 红线树零增量；bin 侧 app 开启）。条目映射：keyring `service` = 配置条目名（默认 `xcontrol`）、`user` = 固定 `security-code`；`keychain:<名称>` 前缀路由（M0 定义）即生效路径。
- **平台后端（查树裁定，2026-11-20）**：mac = security-framework（apple-native）；win = windows-sys Credential Manager（windows-native）；linux = **Secret Service 经 secret-service + zbus 4（纯 Rust，async-io 执行器）**——对照组 dbus-secret-service 需系统 libdbus（破坏 mac 侧跨编译检查）、内核 keyutils（linux-native）非 Secret Service 语义，均不取；zbus 树无 tokio，xcontrol-config「无 tokio」边界在 feature 面内不破。
- **mac 本机实测 PASS**：`cargo test -p xcontrol-config --features keychain -- --ignored keychain_live`（真实 login keychain：写入/读取/覆盖写/删除/幂等 + file 后端对照一致，无用户交互弹窗）。
- **win/linux 编译级**：`cargo check --target` 两 target 全绿（feature 树不含 tokio/不触红线 crate）。**linux 运行时实测移交**（lsdark-eos 本次不可达）：探测配方 `busctl --user status org.freedesktop.secrets`（KDE 的 kwalletd 提供 Secret Service 兼容层；GNOME 为 gnome-keyring）；无 DBus 会话（headless）→ `KeychainSecretStore::is_available()` 探测失败 → UI/向导回退 file 0600 + 日志告警（docs/07 §3.3）。
- **换码**：覆盖写同一 (service, user) 条目；双故障收口（store/config 顺序与回滚）见 docs/07 §3.3。

## 6. 权限与安装对照表（FR-6.3 向导依据）

| 平台 × 角色 | 权限 | 检测方式 | 向导动作 |
| --- | --- | --- | --- |
| mac 主控 | 辅助功能 | `AXIsProcessTrusted()` | 深链系统设置 + 等待轮询 |
| mac 从机 | 辅助功能（注入需要） | 同上 | 同上 |
| win 主/从 | 无（提权可选：UIPI） | `IsUserAnAdmin` 分支提示 | 说明框（可选"以管理员运行"） |
| win 防火墙 | 监听端口入站 | `netsh` 查询规则 | 安装器预配置 |
| x11 主/从 | $DISPLAY 可用 | 连接测试 | 错误诊断页 |
| wayland 主控(portal) | InputCapture 会话授权 | ashpd 授权流程 | 系统对话框 + restore token |
| wayland 从机(portal) | RemoteDesktop 授权 | 同上 | 同上 |
| wayland 兜底 uinput | input 组 / udev 规则 | `/dev/uinput` 写探测 | 给出 `usermod -aG input` / udev 文案 |
| wayland 剪贴板 | data-control 或 clipboard portal | 协议探测 | 自动降级提示 |

## 7. 平台测试清单（发布门禁）

每平台发布前手动过一遍，结果记录在 release checklist：

- **输入**：打字 1000 字符无错漏；修饰键组合；拖拽；滚轮纵横；高 DPI 相对移动累积；5 台从机循环切换 30 分钟无卡键。
- **映射**：`mac2win` 预设下 Cmd+C/V/S/Tab；CapsLock→Esc；组合级改写（拦截 flush 路径）；直通开关。
- **剪贴板**：文本（含 CRLF/LF 混合、emoji、超长）；图片 mac↔win↔linux 六向；HTML 降级；16MiB 上限提示。
- **边缘**：四向切换 + 死区 + 锁定边界；分辨率不对称映射。
- **生命周期**：权限中途吊销的降级行为；睡眠/唤醒；断网重连；主控强杀后从机 3s 恢复本地控制（NFR-5）。
- **Wayland 专项**：GNOME/KDE/sway 三环境各跑：portal 授权→restore token 二次免授权；sway layer-shell 捕获；data-control 剪贴板；热键回退方式提示正确。

## 8. 已知风险与未确认项

| 项 | 状态 | 处置 |
| --- | --- | --- |
| sway 1.11 virtual-pointer 支持 | 矛盾信息 | M5 首周实测，结果写入本文附录 |
| GNOME data-control 剪贴板 | 未确认 | 运行时探测 + clipboard portal 降级（已设计） |
| GNOME <51 InputCapture v1 无法共享剪贴板 | 已知 | 能力协商上报 + UI 提示升级 |
| Windows LL hook 超时 | 已知机制 | 回调内零慢操作（投递线程池） |
| Mutter 对新 uinput 节点的 EACCES 竞态 | 历史 bug | uinput 路径重试 + 报告 |
