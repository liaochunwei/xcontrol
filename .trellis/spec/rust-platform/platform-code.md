# 平台代码规则

来源：docs/05-platform.md、ADR-0003。

## 结构

1. 平台实现只通过 trait 接入：`InputSource` / `InputSink`（xcontrol-input）、`ClipboardWatch`（xcontrol-clipboard）、`TrayBackend`（xcontrol-tray）。
2. feature 名与模块名固定：`macos` / `windows` / `x11` / `wayland`。平台自动选择**不用 crate 自带 build.rs 推断**（build.rs 无法为自身 crate 开 feature——feature 解析先于构建脚本；其 rustc-cfg 输出会让平台 cfg 漏进公共代码，违红线 2；11-17-m3-input-win 裁定、docs/08 §3 同步）——由 xcontrol-app 的 target-specific optional dependency 声明（cargo 原生按目标平台解析）或打包脚本固定 feature 组合承载，也允许显式组合（Wayland 上叠加 x11 兜底）。
3. **Wayland 一律运行时探测后端**（portal → wlr 协议 → uinput opt-in），禁止编译期绑死一条路径。

## 回调线程纪律（正确性关键）

1. 系统回调（CGEventTap / LL-Hook / x11rb 事件 / reis 事件）内只做：事件→XKey 语义转换 → mpsc 投递。**禁止**：剪贴板读写、网络、锁竞争、分配大对象。
2. Windows LL-hook 回调受 LowLevelHooksTimeout 约束，超时会被系统摘除钩子——慢操作必须投线程池。
3. 事件顺序即因果顺序：同一物理设备事件在任何队列/锁面前保持 FIFO。

## unsafe 规则

- 每个 `unsafe` 块必须有 `// SAFETY:` 注释说明不变量（参数合法性、生命周期、线程约束）。
- FFI 边界类型用 `-sys` 风格薄封装；禁止在业务代码直接散写 extern 调用。

## API 速查（详情以 docs/05 为准）

| 平台 | 捕获 | 注入 |
| --- | --- | --- |
| macOS | CGEventTap（需辅助功能权限，`AXIsProcessTrusted` 检测） | CGEventPost |
| Windows | WH_*_LL hook | SendInput（UIPI：提权进程需管理员） |
| X11 | x11rb record + grab | **XTEST**（禁止 XSendEvent——会被忽略） |
| Wayland(GNOME≥46/KDE≥6.1) | ashpd InputCapture + reis | ashpd RemoteDesktop(EIS) + reis |
| Wayland(wlroots) | wlr-layer-shell 边缘窗口 | wlr-virtual-pointer + virtual-keyboard |

剪贴板监听：mac changeCount 轮询 250ms / win WM_CLIPBOARDUPDATE / X11 XFixes / Wayland wl-clipboard-rs Watcher。
热键回退链：OS 热键 API → 事件流内识别 → 边缘双击/死角（docs/05 §5）。

## macOS 剪贴板要点（M2 m2-clipboard 实测，11-17）

- **防回环判据**：内容指纹（blake3[0..16]）为主判据、changeCount 仅作触发源不作判定（一次写入可推进多次计数，不可靠）；写入后 500ms 静默窗不看指纹一律判回声（docs/04 §5 防线 3，基线**保持**不更新——漂移会误吞用户后续复制）。窗口常量有测试钉死。
- **NSPasteboard 后台线程合法**（非 MainThreadOnly，Apple Thread Safety Summary；rustdesk 先例）；objc2-app-kit 0.3 的 generalPasteboard/changeCount 是安全绑定（零 unsafe）；watcher 裸线程暂无 autorelease pool（objc_retainAutoreleasedReturnValue TLS 快路径不依赖），M5 加 ObjC 调用时必须包 `objc2::rc::autoreleasepool`。
- **arboard 3.7 feature 语义**：`default = ["image-data"]`——关默认 features 只裁 image 栈，`get_text/set_text` 无条件编译；M5 图片/HTML 再开 feature 过评审。
- **测试写系统剪贴板必须 RestoreGuard**（Drop trait 恢复，panic 路径也生效）+ `#[ignore]` 标注 + 冒烟间 Mutex 串行化（并发 NSPasteboard 会 SIGTRAP）。
- **Windows 任务带入**：指纹判据必须与**读回路径的归一化形态**一致（M3 落地裁定：LF↔CRLF 平台上取归一化 LF 文本——round-trip 不动点——而非落板 CRLF 字节，见下方"X11 剪贴板要点"的"Windows 指纹字节教训的落点"；M2 时按落板字节的理解已修正）。
- CI 已加 macos feature clippy/test 步骤（m2-clipboard check 发现平台代码此前不在 CI 编译面）。

## Windows 输入层要点（M3 m3-input-win 实测）

- **LL hook 纪律**：回调里 CallNextHookEx 恒先于一切可选工作调用（超时纪律"转发先于一切"）；静态桥 `Mutex<Option<HookShared>>`（Sender: Send ⇒ Sync，无需 unsafe impl）；stop = 先摘共享再 PostThreadMessageW(WM_QUIT) 再 join。
- **injected 过滤的平台不对称**：Windows 捕获侧滤 LLKHF_INJECTED/LLMHF_*（自注入不回流），mac ListenOnly 不过滤——跨平台语义不对称是预期，实机两进程对拍验证（docs/05 §2.1 item 6）。
- **scancode 表源**：MS Learn《Keyboard Input Overview》官方表（NumLock=E045 与 Pause=0045 同码分流、RShift=0x36 无扩展、PrintScreen=E037 双形态）；反查首见优先取 canonical（entry().or_insert 表序构建）。
- **windows crate 0.62**：SetWindowsHookExW 返回 Result<HHOOK>、SendInput 切片形参——签名查 registry 源码，勿凭记忆。blake3 交叉编译 windows-msvc 需 ml64.exe，本机 check 用 `CC_x86_64_pc_windows_msvc=nonexistent-cc` 前缀回退纯 Rust（Cargo.toml 注释配方）。

## X11 输入层要点（M3 m3-input-x11 远端实测）

- **keycode 语义（重要勘误）**：X keycode = evdev `KEY_*` + 8，**≠ HID usage+8**（KEY_A=30 vs HID 0x04）——全表翻译源 = 内核 drivers/hid/hid-input.c 的 hid_keyboard[] + HID_UP_CONSUMER（PRD 假设错误已修 docs/03/05）。表用 torvalds/linux v6.6 源码逐项核对，勿信二手文档。
- **XRecord 双连接时序竞态**：enable/disable 经不同连接到达服务器的顺序不保证——start 后立即 stop 会"先禁后启"挂死。防线：以服务器 StartOfData 首应答为 enable 凭证后才放行宿主（`run_record_thread`）。
- **XRecord 只录已投递事件**：需要 X 焦点目标（Xwayland 下注入事件经 compositor 路由，X 客户端/XRecord 不可见——xdotool 同行为，移交 M5 Wayland 路径）。远端实测纪律：自建 rootful Xvfb 式 server（DISPLAY=:2）做确定性回环，不动用户真实会话。
- x11rb 0.13 `forbid(unsafe_code)` 默认开启（lib.rs cfg_attr）——平台模块零 unsafe 达成；record/xtest/xfixes features 组合。
- Text latin-1（0x20-0xFF）经 keysym 直通；CJK/emoji 整段预扫描拒绝（SinkError::Unsupported），M5 走 Wayland unicode 路径。

## X11 剪贴板要点（M3 m3-clipboard-win-x11 远端实测，2026-11-17）

- **X11 无全局 changeCount**：监听计数 = 本进程观测到的 SelectionNotify 事件数（`Arc<AtomicU64>` 剪贴板句柄与 watcher 共享）；不监听时恒 0（trait"单调不减"仍成立）。Windows 有 `GetClipboardSequenceNumber`（无需开剪贴板、任意线程可读）——三平台 changeCount 对应物：mac NSPasteboard.changeCount / win 序列号 / x11 观测计数。
- **XFixes 双连接停机的双端 sync（实测教训）**：X 服务器对跨客户端连接的请求无顺序保证，flush 只到 socket——工作线程 unsync 的 `destroy_window` 与下一监听器复用被回收窗口 id 段的 `create_window` 竞态（destroy 后到连带销毁新窗口），下一次停机 ClientMessage 落空（SendEvent 到不存在窗口 = BadWindow，unchecked 静默）→ join 挂死（`watcher_restart_stress` 复现）。防线：工作线程 destroy 后 **sync** 才退出 + 停机连接 send_event 后 **sync** 才 join。
- **SendEvent 空掩码投递**：`event_mask = NO_EVENT` 时事件投递给**创建 destination 窗口的客户端**（X11 protocol spec §8）——跨连接停机信号的合法通道；窗口必须由应接收事件的连接创建（XFixes 事件送创建者客户端）。
- **arboard X11 常驻语义**（3.7.0 源码核实）：`Clipboard::new()` 起专属 serve 线程持有 selection 响应 paste（进程存活期），高层 `set_text` 立即返回；`SetExtLinux::wait()` 是短命进程 daemonize 模式、长驻进程**禁用**（会挂死调用线程）；Drop 最后一个句柄 → arboard 自动向 CLIPBOARD_MANAGER 临终转交（尽力而为）。
- **Windows 指纹字节教训的落点**：LF↔CRLF 平台上，指纹取**归一化 LF 文本**（读回形态的 round-trip 不动点），不是落板 CRLF 字节——否则防线 1 自回声识别必然失效（newline.rs 模块文档）。
- 默认套件含 `watcher_restart_stress`（10 次 start/stop 复用 id 段）作为停机时序回归防线；写 CLIPBOARD 的冒烟一律 `#[ignore]` 且只在自建 rootful X server（:2）跑。

## Wayland 输入层要点（M5 m5-wayland 实测，2026-11-19）

- **ashpd feature 组合**：`remote_desktop` 模块**硬依赖 `screencast` feature**（0.13.13 `desktop/remote_desktop.rs` 顶部 `use super::screencast::Stream` 无 feature 门控，Cargo.toml `[features]` 也没声明这条依赖——只开 remote_desktop 不开 screencast 直接编译错误）。portal 面的特征组 = `async-io + input_capture + global_shortcuts + remote_desktop + screencast (+clipboard)`（**11-19-m6-release 修订：`tokio` 不可取**——gpui-base（gpui-pre-linux）启用 ashpd `async-io`，feature 统一两执行器并存 → ashpd `compile_error!`；打包面 app 构建暴露），workspace.dependencies 登记注释同录。
- **uinput 只能注入已声明的码位**：`VirtualDeviceBuilder` 的声明集 = 注入能力集（内核 `input_inject_event` 按设备 keybit 过滤未声明码——静默丢弃或写错误）。`x11keymap::EVDEV_TABLE` 是纯键盘表（无 BTN 0x110+）——鼠标键 BTN_* 必须在 `with_keys` 里单独声明，否则按钮注入静默失效且默认冒烟测不出来（11-19 check 补修；冒烟自此覆盖鼠标键）。
- **"rei" 系 "reis" 之误**（docs/05 §4.5 勘误）：调研曾引用 "rei crate（uinput 封装）"——不存在；`reis` 是 libei 协议实现（EIS fd 事件通道，需 portal 会话才可用），uinput 兜底路径用 `evdev` crate（docs/05 §4.3）。X keycode = evdev+8 与 EVDEV_TABLE 同源复用，不建第二张表。
- **GlobalShortcuts KDE 应用身份门槛（实测）**：无人值守 bind 被拒 `NotAllowed: An app id is required`——KDE 要求调用方有匹配的 `.desktop` 桌面身份（打包/安装前置项，非代码缺陷）；InputCapture/RemoteDesktop 会话建立在 KDE 6.7.4 无人值守 3s 内就绪，授权后全链路移交用户在场。
- **异步边界**：portal DBus 调用只在模块内控制线程的 `async_io::block_on` 上泵动（11-19-m6-release 从 tokio current-thread runtime 切换——理由见 ashpd feature 条目）；超时竞速/多路事件选择 = `race_timeout`/`futures_util::select!`（**操作数须 `.fuse()`**，宏要求 FusedFuture/FusedStream；宏内部自行 stack-pin，非 Unpin future 可直接传）；事件路径 reis 同步核心 API（`ei_handshake_blocking` + `read`/`pending_event`）+ std mpsc——回调线程纪律零 async 保持；tokio 依赖保留（仅 mpsc 命令通道——运行时无关原语，`tokio::time::*`/`tokio::select!` 在无 reactor 的 block_on 下会 panic，不可混用）。

## 热键要点（M4 m4-hotkeys 实测）

- **XKB autorepeat 陷阱**：passive grab 下按住热键会收到「同时间戳 Release+Press 对」×N（XKB repeat）——必须 held 位集 + pending_release 同戳判定，否则连环路由。docs/03 §4.6"按住不重复路由"在 x11 靠这个防线（win 用 MOD_NOREPEAT，mac 捕获层原生无 autorepeat 重复 Down）。
- **热键实现选型**：mac 走捕获流识别（不采用 Carbon RegisterEventHotKey，少一条依赖）；win 用 RegisterHotKey + message-only 窗口（OS 消费组合，与 hook 吞键语义重复且冲突超时纪律）；x11 用 grab_key×8 锁定变体（i3 同款）。三平台"不转发"语义落点不同（mac app 层不产出转发 / win OS 消费 / x11 grab 消费）——都写在 core hotkey.rs 文档。
- **受控事实源**：ControlDir 是唯一受控状态——不加独立"当前受控指针"注册表（避免与 session 状态机双份漂移）。

## 剪贴板完整版要点（M5 m5-clipboard-full 实测）

- **Wayland 实测环境**：SSH 非交互 shell 不加载 PATH（`export PATH=$HOME/.cargo/bin:$PATH`）；KDE 的 WAYLAND_DISPLAY socket 名**要探测不要假设**（实测 `wayland-0`，假设 wayland-1 会得到 "Couldn't connect to the Wayland compositor"）；`XDG_RUNTIME_DIR=/run/user/1000` 必设。wl-clipboard-rs 在 KDE Plasma（zwlr_data_control）文本/图片/HTML round-trip 实测通过。
- **图片 wire 格式统一 PNG**：平台解码到 RGBA（arboard image-data）→ PNG 编码上 wire → 对端解码回 RGBA 落板。PNG encoder 选轻量 `png` crate（image crate 依赖树过重）。
- **HTML 平台格式**：arboard 3.7 **有** html 支持（`get().html()`/`set().html()`）——win CF_HTML（clipboard-win `wrap_html` 偏移头）与 x11 `text/html` target 直接用 arboard；**mac 写入不走 arboard**（`set_html` 的 `<html><head>…</body></html>` 模板包装破坏 fragment 读回等价性 → 回声指纹失配），改 objc2-app-kit `clearContents` + `setString_forType(html, NSPasteboardTypeHTML)` public.html 原样挂载——objc2 0.6 全安全绑定、零 unsafe。教训：依赖能力以 docs.rs 实查为准；HTML 回声指纹的判据 = 读回形态 round-trip 不动点（win CF_HTML 读回提取行为实机复核项，若读回 ≠ 写入则指纹按读回形态对齐）。

## 两机 KVM 实测要点（11-20 实机联调定稿）

- **受控注入必须 ABS 虚拟光标**（KDE/Wayland；**11-24 勘误**：11-22 起现行代码改回 REL 注入——加速由 libinput quirks 对本设备关闭，见下方「uinput 位置模型纪律」；本条余下内容保留为 11-20 的实测历史）：REL 位移被 compositor 指针加速放大数倍（小幅移动光标狂奔）。主控维护 virt_x/virt_y 累加 + ABS 直发；**warp 必须同步 virt 坐标**（否则 MoveRel 从陈旧位置起跳——进入后跳变元凶）；uinput ABS 轴 `AbsInfo.resolution` 必填（=0 时 libinput 分类为触屏类，触发角/手势管线不喂——1000 units/mm 惯例）。
- **受控光标策略定稿**：切入瞬间**锁定当前位置**为锚点（每拍 warp 回该点，光标"冻结在切入处"）——比角点锚自然（无边界噪声/无跳底部/视觉语义对）。锚定与触边采样解耦：锚点在屏内侧则传感器安静。
- **macOS 26 非前台限制（探针实测）**：`CGDisplayHideCursor`/`CGAssociateMouseAndMouseCursorPosition` 对非前台进程**均被忽略**（返回 0 但无效果）——cargo run 裸进程受控时光标可见但冻结。正式 .app 打包（GUI 身份）后预计生效，0.1.x 验证项。
- **探针方法论**：平台语义怀疑时先写最小 ObjC 探针实测（`clang -framework CoreGraphics probe.m`）——本轮三个"理论正确"的方案（associate 解关联/warp 无事件论/hide 全局生效论）全被探针修正。warp 实测**不产生 MouseMoved 事件**（tap 零输出）。
- **调试经验**：从机收帧 dump（全帧 head hex）比猜偏移快——第一版观测偏移错（seq 低字节≠kind），第二版才对上；SSH 非交互环境的 PATH/WAYLAND_DISPLAY/XDG_RUNTIME_DIR 三件套每次都要显式。

- **uinput 位置模型纪律（11-24）**：现行代码（`wayland.rs`，11-22）受控移动走 **REL 注入**——代码注释的实测记录：加速由 libinput quirks 对本设备单独关闭（flat）后 REL 不再被放大；kwin 边缘/角落判定对 ABS 注入不响应、对 REL 响应。`virt_x / virt_y` 是位置模型的**唯一事实源**（回报 `cursor_normalized` 与 MoveRel 的起算点都取自它）：`warp` 同步 virt；`release_all` **保留** virt（复位回中心 = `preserve_cursor=1` 的热键切入之后回报基于虚构位置、边缘回切失效）；`InputSink::resync_cursor` 把真实光标 ABS 定位回 virt（保持原位切入后 app 调一次；默认无操作，仅 uinput 覆写）。**已知不对称**：`inject(MouseMoveAbs)` 分支只发 ABS、不同步 virt——当前转发链不产该事件（mac 捕获只产 MoveRel），reis 捕获源会产，接入转发之前必须先补同步。
- **macOS 受控期门控与节拍耦合（11-24）**：锚点内缩 90pt 只能让**慢速**移动下的传感器安静；快速移动时光标在两次锚定回拉之间照样冲到屏边。因此受控期间的主控边缘命中由 app 门控一律忽略，控制关系变化后另有 150ms 防抖（`session_rt.rs` 的 `edge_hit_allowed` / `edge_dead_time_elapsed`）。边缘轮询与受控回报节拍 `DEFAULT_POLL_INTERVAL`（10ms）与锚定回拉（会话输入拍 `INPUT_POLL_INTERVAL`，同为 10ms）近同相位——**只提节拍会让受控期误触发更频繁，只收紧门控则撞边停滞不变，两者必须同进同退**。锚点同时是热键回主控的落点：`CaptureSource::anchor_point()` 只读，经 `pending_release_warp` 在重关联**之后**重放（先 warp 再重关联会被覆盖，11-22 教训）。

- **uinput 顶边行程度量（11-25）**：`advance_axis`（越界累加被截掉的推进量、回到屏内即清零、贴边且该轴无位移时保留）+ `axis_report`（`overflow < 0` 回报负值、`> 0` 回报 `1.0 + overflow`）；`warp` / `release_all` 清零。回报值因此可以越出 `[0, 1]`。既有取整：`warp` 取 x=1.0 时 `virt = max + 1`，`advance_axis` 对域外起点按边界起算，那 1 格不算行程。
- **macOS 辅助功能授权按签名匹配（11-24 实测）**：ad-hoc 签名的 .app 重新打包后授权即失效——同一路径、同一 bundle id，`CGEventTapCreate` 直接返回空（"未授权时 tap 创建成功但静默忽略"的旧说法只对 ListenOnly 成立）。引导与恢复：启动时 `guide_accessibility_at_startup`（未授权 → `check_accessibility(true)` 请求系统引导框；`--headless` 只告警不弹框）+ 会话内每秒静默探测、已授权即重建捕获源；恢复前若名义上处于受控态，先 `ReleaseControl`（剥离落点 warp，光标不动），避免授权瞬间本地输入被突然转发到工作机。

## SSH 部署红线（2026-11-21 实测轮，两坑实踩）

1. **构建漏平台 feature = 静默空壳**：`cargo build -p xcontrol-app` 不带 `--features <macos/windows/x11>` 时进程照常启动、端口照常监听，但捕获/注入/剪贴板/托盘全为空壳，唯一线索是日志 `平台剪贴板后端不可用` warn。部署成功的判定标准是功能日志（`剪贴板通道就绪` 且无后端 warn + 连接建立）而非进程存活。配方：docs/10 §3.3.1。
2. **KDE Wayland SSH 启动五件套**：`PATH` + `systemctl --user show-environment` 四变量（WAYLAND_DISPLAY/DISPLAY/XDG_SESSION_TYPE/XDG_RUNTIME_DIR）+ `XAUTHORITY=/run/user/1000/xauth_*`（kwin cookie 随机名文件；仅 DISPLAY 报 "Authorization required"）。缺 XAUTHORITY 时 X11 后端（arboard 等）构造失败。
3. **远端 pkill 必须用 `pkill -x xcontrol-app`**（进程名精确匹配）——`pkill -f` 模式出现在 ssh 命令行字符串里会把会话 shell 自身杀掉（ssh exit 255，命令半途死）。
4. **ssh 长命令静默期会被断开**（构建 `| tail -5` 期间无流量，连接被掐 exit 255）——长构建放远端 `nohup ... > log 2>&1 &` 后台 + 轮询日志；ssh 加 `-o ServerAliveInterval=15`。

5. **feature 按会话类型选**：Linux 部署命令的 feature 跟随桌面会话（`XDG_SESSION_TYPE`）——Wayland 会话用 `--features wayland`（wl-clipboard-rs 原生，image/png 可读），Xorg 才用 x11。x11 变体在 KDE Wayland 下走 Xwayland 桥：文本通、**图片 MIME 同步缺口**（实测症状：文字正常截图发不回 + 启动缺 `连接基线：既有剪贴板已登记` 行）。剪贴板 crate x11/wayland 互斥门禁意味着选错即静默降级，判定读图能力看基线行而非进程存活（docs/10 §3.3.1）。
