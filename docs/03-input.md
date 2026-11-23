# 03 · 输入事件模型与按键映射规范

> 状态：草案 v0.1 · 实现 FR-2.x / FR-3.x。本文定义与平台无关的输入事件抽象、键码体系、映射引擎语义与平台预设。

## 1. 设计原则

1. **键码语义化**：协议中传输的键码是**与平台无关的抽象键码**（USB HID usage），主控的键盘布局（Dvorak、非 US 布局、输入法）不得影响从机输出（FR-2.6）。
2. **修饰键忠实转发**：修饰键（Ctrl/Shift/Alt/Meta）作为普通按键事件转发，从机本地维护修饰键状态机；**不**由主控聚合成"快捷键对象"——这样映射引擎才有改写空间。
3. **映射只在从机侧生效**：主控转发前不做任何改写；从机注入前经过映射引擎（FR-3.4）。例外：主控侧热键（FR-2.5）在捕获层拦截、不转发。
4. **文本兜底**：键码无法表达的内容（死键组合、输入法组字结果、emoji 面板）走 Unicode 文本事件直发。

## 2. 键码体系：XKey

`XKey = u16`，取值空间采用 **USB HID Usage ID**（键盘页 `0x07` + 消费者页 `0x0C`），与 QMK/KMonad/kanata 等工具的选型一致，转换表成熟。

| 取值范围 | 含义 |
| --- | --- |
| `0x04–0xE7` | HID 键盘页（`A`=0x04 … 修饰键 `LeftCtrl`=0xE0, `LeftShift`=0xE1, `LeftAlt`=0xE2, `LeftMeta`=0xE3, `RightCtrl`=0xE4, `RightShift`=0xE5, `RightAlt`=0xE6, `RightMeta`=0xE7） |
| `0x100–0x1FF` | HID 消费者页（媒体键、音量、亮度等），基址偏移 `0x100`（如 `VolumeUp`=0x100+0xE9） |
| `0x200–0x2FF` | 指针键（鼠标侧键等超集，自定义扩展区） |
| 其余 | 保留，禁止使用 |

命名约定：常量名 `KEY_A`、`KEY_LEFT_META`、`CK_VOLUME_UP`（`CK_` 前缀 = consumer key）。`xcontrol-proto` 提供 `XKey` 新类型 + 全量常量表 + 各平台转换表：

| 平台 | 源键码 | 转换 |
| --- | --- | --- |
| macOS | `CGKeyCode`（kVK_*） | 静态映射表（well-known 表，QWERTY 物理位 → HID） |
| Windows | Set/Get 视角 scancode（make code） | 静态映射表（scancode set 1 → HID） |
| X11 | `keycode - 8`（= evdev `KEY_*` 编号，libinput/xf86-evdev 直传 +8） | `KEY_*` ↔ HID 全表（与 Wayland(uinput) 行共用；`keycode = KEY_* + 8` 全量成立，xkb `keycodes/evdev` 逐行背书） |
| Wayland(uinput) | `linux/input-event-codes.h` `KEY_*` | 线性表（`KEY_*` ↔ HID，两者编号不同需全表） |

> 修订记录（2026-11-17，任务 11-17-m3-input-x11）：原 X11 行"keycode - 8（evdev 驱动下即
> HID usage）| 直接等值（校验 + 异常表）"有误——`keycode - 8` 等于的是 **evdev `KEY_*`
> 编号**（`linux/input-event-codes.h`），而非 HID usage（两者编号体系不同：`KEY_A` = 30
> 而 HID `A` = 0x04；内核 `drivers/hid/hid-input.c` 的 `hid_keyboard[]` 表才是
> usage → `KEY_*` 的官方翻译源）。X11 与 Wayland(uinput) 行因此共用同一张
> `KEY_*` ↔ HID 全表，X11 侧只需叠加固定的 `+8`（该关系全量成立、无例外，
> 见 xkb `keycodes/evdev` 文件每一行 `<K> = KEY_* + 8` 的自注释）。

> 布局无关性的责任边界：转换表按**物理键位**而非字符；字符由从机自身布局解释。`Text` 事件用于需要显式字符的场景（§4.5）。

## 3. 事件模型（协议层）

```rust
/// 输入事件。语义模型如下；wire 编码为定长手写帧（docs/02 §4.2 kind 表，小端），
/// 本节不做序列化规范——帧格式冲突时以 docs/02 为准。帧内一次一条。
enum InputEvent {
    /// 按键按下/抬起（含修饰键本身）。code: XKey。
    Key { code: u16, state: KeyState },            // KeyState = Down | Up
    /// 长按重复（可选发送；从机若平台支持本地 repeat 可忽略）。
    KeyRepeat { code: u16 },
    /// 相对指针移动（高 DPI 下保留小数，从机累加取整注入）。
    MouseMoveRel { dx: f32, dy: f32 },
    /// 绝对指针移动（目标屏幕内归一化坐标 0.0–1.0，FR-2.7）。
    MouseMoveAbs { screen: ScreenId, x: f32, y: f32 },
    /// 鼠标按键。
    MouseButton { button: MouseButton, state: KeyState }, // Left/Middle/Right/Side(Back/SideFwd)
    /// 滚轮。delta 单位=「格」（notch，保留小数以支持高分滚轮/触控板）。
    Wheel { axis: Axis, delta: f32 },               // Axis = Vertical | Horizontal
    /// Unicode 文本直发（§4.5），仅在键码路径不可行时使用。
    Text { utf8: String },
}
```

不变量：

- **顺序保证**：同一物理设备的因果顺序（如 `Meta↓, C↓, Meta↑, C↑`）在网络流中严格保序（单 TCP/QUIC 流天然满足；多流时输入事件固定走 stream 0）。
- **修饰键状态归属**：主控在**切入屏幕时**发送全量修饰键快照（`ModifierSet` 消息），从机注入前先对齐，防止"卡键"；切出时从机负责释放本机全部按键。心跳内周期性（1s）校验状态一致。
- **拖拽**：不设专门消息——`MouseButton{Left,Down}` + 连续 `MouseMoveRel` 序列即构成拖拽（FR-2.4）。
- **合帧**：单帧最多聚合 8 个 `MouseMoveRel`（≤250Hz 鼠标 + 120Hz 发送周期下的上界），键事件不合帧。

## 4. 按键映射引擎（从机侧，FR-3.x）

### 4.1 规则模型

配置节 `[[keymap.rules]]`，自上而下**首条命中**（first-match-wins），规则数上限 256：

```toml
# 1) 单键重映射
[[keymap.rules]]
trigger.key = "CapsLock"
action.key  = "Escape"

# 2) 修饰键映射（事件级，见 4.2）
[[keymap.rules]]
trigger.mod = "LeftMeta"          # 主控侧的修饰键
action.mod  = "LeftCtrl"          # 注入到本机（从机）的修饰键

# 3) 完整快捷键改写（组合级，见 4.3）
[[keymap.rules]]
trigger = { mods = ["LeftMeta"], key = "C" }
action  = { mods = ["LeftCtrl", "LeftShift"], key = "C" }

# 4) 特殊动作
[[keymap.rules]]
trigger = { mods = ["LeftMeta","LeftShift"], key = "Digit3" }
action.drop = true                # 吞掉不注入
```

- `trigger.mods` 为**精确集合匹配**（列出的修饰键全部按下、未列出的不得按下）。
- 规则中的修饰键名使用 XKey 语义名（`LeftMeta`/`RightAlt`…），左右手分开。
- 键名文法（config v3 落地，`xcontrol_config::parse_key_name` 解析为 XKey）：修饰键全名
  `LeftCtrl/LeftShift/LeftAlt/LeftMeta` 及右手对应（简写 `LCtrl/LOpt/LWin` 等 v2 别名兼容）；
  字母 `A`–`Z`；数字 `Digit0`–`Digit9`（兼容裸 `0`–`9` 别名）；`F1`–`F24`；常用功能名（`CapsLock`/`Escape`/`Enter`/
  `Space`/`Tab`/导航键等）；以及 proto 常量全名（`KEY_A`/`KEY_LEFT_META`/`CK_VOLUME_UP`）。
  未知名在配置加载期报错（`keymap.rules[i]` 字段路径）。
- 规则匹配作用于**映射后的语义空间**之外——即 trigger 用主控发来的原始（映射前）XKey 判定，action 产出映射后的注入序列。整条流水线：`接收 → 热键过滤（4.6，映射之前）→ 规则改写 → HID→本机键码 → 注入`。引擎落点为从机 `InputSink` 包装层（`MappingSink`），热键在主控捕获层过滤（不转发），从机侧注入前过滤由调用方在引擎之前完成（M4-hotkeys）。

### 4.2 修饰键事件级映射（默认路径）

对第 2 类规则采用**事件级直映射**：主控 `LeftMeta↓` 到达 → 注入 `LeftCtrl↓`；`↑` 同理。状态机为每个修饰键维护 `pressed_origin → mapped` 对照表，保证任意释放顺序正确（防卡键）；并按 mapped 目标维护**引用计数**——多个 origin（规则映射或恒等登记）压到同一个 mapped 修饰键时，最后一个 origin 释放才注入 `↑`（防"过早释放"：如 mac2linux 下 `Cmd+Ctrl` 同落客户端 `Ctrl`，先松 `Cmd` 不得提前释放仍在按住的 `Ctrl`）。重复 Down/Up 事件按 origin 幂等（不重复登记、不产出注入）。

主控按 `Cmd+C`（mac）：发 `LeftMeta↓, C↓, C↑, LeftMeta↑` → 从机注入 `LeftCtrl↓, C↓, C↑, LeftCtrl↑` = `Ctrl+C` ✔（FR-3.1 AC）。

### 4.3 组合级改写（拦截状态机）

当 modifier 数量在两侧不同（如 `Cmd+C → Ctrl+Shift+C`）时，事件级映射无法表达，需要**拦截**：

1. 命中触发器修饰键（如 `LeftMeta`）按下时，**暂不注入**，进入 pending 态（触发器集合 = 所有组合/drop 规则 trigger.mods 中出现过的修饰键；非触发器修饰键照常走 4.2 事件级注入）；
2. 后续非修饰键按下到达：查组合规则表（`trigger.mods` 精确等于当前按下的修饰键全集合 + `trigger.key` 相符），命中 → 注入**原子弦** `action.mods 全部↓ → action.key ↓ → action.key ↑ → action.mods 全部↑`（一次性 tap，不随物理按住延续；命中后触发器修饰键保持 withheld——其后重复按键可再次命中，或未命中时 flush 补注入，见 3'）；drop 类命中 → 全部吞掉不注入；
3. 触发器修饰键**全部释放**仍无非修饰键按下 → flush：pending 修饰键的 Down 从未注入，故**不产出任何注入**（避免虚假 `↑` 误放真实按下的同目标键），仅按序吐出缓存事件；
3'. 非修饰键按下**未命中**任何组合规则 → flush（退回 4.2 语义）：注入已映射的 pending 修饰键（登记 4.2 对照表）→ 缓存事件 → 当前键；flush 后触发器修饰键保持物理按下（4.2 语义已注入）——其后重复按键仍可再次命中组合（弦对已注入的同目标修饰键跳过 ↓/↑，不触碰 OS 已按住的键）；缓存事件在 flush 时按各自路径语义解析后吐出（`KeyRepeat` 同样走单键 drop/重映射换算，与直通路径一致）；
4. 无超时依赖（按键必然释放；掉线时由「修饰键快照对齐」清理，§3）。

约束：pending 期间缓存**非解析事件**（`KeyRepeat`/鼠标/滚轮；非修饰键按下立即解析不缓存；`Text` 走字符路径直通不缓存，见 4.5）上限 16 条，超限立即 flush（注入已映射 pending 修饰键 + 已缓存事件，当前事件按新状态重新处理），防异常序列撑爆。

### 4.4 平台预设

预设 = 一组规则 + 默认开关。内置（`keymap.preset = "mac2win"` 等），自定义规则叠加在预设之后（先命中预设则不查自定义——需覆盖时用 `preset = "none"`）：

| 预设 | 规则摘要 | 说明 |
| --- | --- | --- |
| `mac2win` / `mac2linux` | `LMeta→LCtrl`, `RMeta→RCtrl`, `LOpt→LAlt`, `ROpt→RAlt`, `LCtrl→LWin`(win)/直通(linux) | Mac 主控 → Win/Linux 从机 |
| `win2mac` / `linux2mac` | `LCtrl→LMeta`, `RCtrl→RMeta`, `LWin/RWin→LOpt/ROpt`(win), `LMeta/RMeta→LOpt/ROpt`(linux), `LAlt` 直通 | Win/Linux 主控 → Mac 从机 |
| `win2linux` / `linux2win` | `LWin ↔ LMeta` 直通语义不变 | 默认仅 Super 命名差异 |
| `mac2mac` / `none` | 恒等 | 直通 |

> `mac2win` 中 mac `Ctrl` 的去向存在用户偏好分歧（映射为 `Win` 或保持 `Ctrl`）：预设默认**保持直通**，`LCtrl→LWin` 作为注释示例给出，用户可启用（在设置 UI 中以开关呈现，避免手写 TOML）。

媒体键（`CK_*`）默认直通，不做映射。

> 修订记录（2026-11-18，任务 11-18-m4-mapping）：§4 全节实现前修订——
> ① §4.2 对照表补**同目标引用计数**（多 origin 落同一 mapped 修饰键，最后一个释放才注入
> `↑`，防过早释放/卡键的两难取"永不提前释放"侧；重复 Down/Up 按 origin 幂等）；
> ② §4.3 定稿 flush 语义——路径 3（全释放）**不产出注入**（pending Down 从未注入，虚假
> `↑` 会误放真实按下的同目标键），路径 2'（未命中）补注入 pending 映射 + 缓存 + 当前键；
> 组合命中为**原子弦**（一次性 tap，不随物理按住延续，命中后触发器保持 withheld 使重复
> 按键可再次命中）；缓存范围定稿（非修饰键按下立即解析；`Text` 直通；`KeyRepeat`/鼠标/
> 滚轮缓存，上限 16 超限即 flush）；③ §4.4 表 `win2mac`/`linux2mac` 补右手机规则
> （`RWin→ROpt` / `RMeta→ROpt`，原表只有左手——对称缺口）；④ §4.1 补键名文法与
> 流水线落点（`InputSink` 包装层，热键在映射之前）；⑤ 实现载体：config schema v3
> （`keymap.passthrough` 开关 + `[[keymap.rules]]` trigger/action 四形态，docs/01 §5 同步）。
>
> 修订记录（2026-11-18，任务 11-18-m4-mapping check 补录）：① §4.1 键名文法补裸
> `0`–`9` 别名（实现 `keynames.rs` 已含，文档漏写）；② §4.3 3' 补 flush 后触发器
> 保持按下可再次命中、缓存事件按路径语义解析（`KeyRepeat` 走 drop/重映射）两处
> 实现定稿语义；③ §6.2 proptest 在 core 零新依赖红线下以确定性种子伪随机
> （64 种子 × 120 操作）驱动同一不变式（`xcontrol-core/src/mapping.rs`
> `randomized_modifier_pairing_invariant_holds`），proptest 引入需先过依赖闸门。

### 4.5 文本直发（Text 事件）

发送侧（主控）触发条件：

1. 按键事件携带的字符与"当前布局下该物理键应产生字符"不符（输入法/死键/组合键产生）；
2. 主控平台回调直接给出 Unicode（如 macOS `CGEventKeyboardGetUnicodeString` 修饰组合无法映射时）。

接收侧（从机）注入路径：

| 平台 | 注入 API |
| --- | --- |
| macOS | `CGEventKeyboardSetUnicodeString` + 合成 keyDown/keyUp |
| Windows | `SendInput` `KEYEVENTF_UNICODE` |
| X11 | XTEST keysym 反查注入（M3 实装 **latin-1 子集**：keysym 0x20–0xFF 与 Unicode 码点直通，经服务器 keymap `get_keyboard_mapping` 快照反查 keycode，Shift 级字符补修饰键包裹；CJK/emoji 等超 latin-1 字符**整段拒绝**报能力不支持，不注入半个词——完整 keysym 合成（XIM/XChangeKeyboardMapping 动态键位）侵入性强，M5 评估后落地，见 [05-platform.md](05-platform.md) §3.1 item 3） |
| Wayland | uinput 无 Unicode 入口 → 经 evdev 键序列不可行时，**上报能力不支持**，UI 提示该从机建议改走"键码路径"（见 05-platform.md 能力矩阵） |

`Text` 事件必须整段 UTF-8、≤ 512 字节/条；超长由主控分段。

> 修订记录（2026-11-18，任务 11-17-m3-input-x11 check 补录）：X11 行原文"XIM/XTEST
> keysym 合成（经 `Xutf8LookupString` 反查 keysym；CJK 走多 keysym 序列）"描述的是
> 平台理论能力，与 M3 实际交付不符——`Xutf8LookupString` 是输入法查询 API 非注入
> 路径，M3 实装为 **latin-1 子集**（keysym 0x20–0xFF 经 `get_keyboard_mapping`
> 反查 keycode），CJK 不走"多 keysym 序列"而是**整段拒绝**（`SinkError::Unsupported`，
> 远端实测确认拒绝路径）。能力矩阵（[05-platform.md](05-platform.md) §0）的
> "X11 ✅ keysym 合成"仍指平台能力上限，完整合成路径 M5 评估。

### 4.6 热键（切换控制权，FR-2.5）

- 热键在**两端**都注册为本地全局热键，命中即本地处理、**不转发**（主控在捕获层过滤，从机在注入前过滤）。
- 默认热键（可在设置改）：

| 动作 | 默认 | config 字段 |
| --- | --- | --- |
| 切到下一台 | `Ctrl+Alt+→` | `[hotkeys].next` |
| 切到上一台 | `Ctrl+Alt+←` | `[hotkeys].prev` |
| 回主控 | `Ctrl+Alt+Home` | `[hotkeys].back_to_host` |
| 暂停/恢复 | `Ctrl+Alt+P` | `[hotkeys].pause` |

- 热键匹配发生在**映射之前**（用原始 XKey 判定），保证任意预设下热键行为一致。
- **匹配语义（11-18-m4-hotkeys 实现定稿）**：
  - **修饰键「任一侧命中」**：`Ctrl`/`Alt`/`Shift`/`Meta`（含 `Super` 别名）泛指左右任一侧——`Ctrl+Alt+→` 在右 Ctrl + 左 Alt 下同样命中。匹配层把左右手位集折叠为四家族位（`xcontrol-core::hotkey::ModFamilies`）；事件流快照与转发仍是逐键左右手位集（§3 保真不变）。
  - **精确集合**：未列出的修饰键家族按下 → 不命中（`Ctrl+Alt+→` 不在 `Ctrl+Alt+Shift+→` 时触发）——与 §4.1 规则 `trigger.mods` 的精确语义一致。
  - **仅主键 Down 匹配**；按住期间的重复 Down/KeyRepeat 吞尾且**不重复路由**（防按住连环切换）；释放后再按重新命中。主键 Up/KeyRepeat 连同 Down 一并吞（对端不得收到无 Down 的 Up）。
  - **命中吞键范围**：只吞主键 Down/Up/重复；组合的修饰键事件**照常转发/注入**（命中判定点在主键 Down，修饰键先于判定点已转发；对端收到的「修饰键按下又抬起、无主键」序列无注入副作用）。
  - 同组合多键配置按 config 字段序先到先得。
- **平台实现路径与「不转发」语义（两端）**：
  - **mac**：捕获流识别（CGEventTap ListenOnly + `HotkeyFilter`，RegisterEventHotKey 不采用——捕获识别已覆盖且少一条 Carbon 依赖）。ListenOnly **无法吞事件**，「不转发」的落点是 **app 层过滤不产出转发事件**（`session_rt` 捕获 poll → `HotkeyFilter` 命中 → 本地路由，未命中原样出站）；合成事件方向判定需设备位（§2 表 mac 行、`platform/macos.rs` 已知限制）。
  - **win**：`RegisterHotKey`（消息窗口线程，clipboard message-only 窗口先例）+ `WM_HOTKEY` → `HotkeyId`；命中组合由 OS 消费（不投递前台应用）。**LL hook 吞键（不调 `CallNextHookEx`）不采用**：与 hook 超时纪律「`CallNextHookEx` 恒先于一切」（docs/05 §2）冲突，且与 RegisterHotKey 的 OS 级消费语义重复——主控侧转发过滤由 app 层 `HotkeyFilter` 承担（双保险），从机侧「不注入」由注入前过滤承担。主键→VK 映射表（`winkeymap::xkey_to_vk`）覆盖字母/数字/F1–F24/导航/符号/小键盘；媒体键（`CK_*`）无 VK 语义不映射（热键注册路径不支持，事件流识别路径可用）。
  - **x11**：`grab_key` 被动抓取（root 窗口，`owner_events=false`）——**服务器侧精确匹配**即「精确集合」语义的 OS 层实现，grab 的按键只上报抓取客户端（焦点窗口收不到 = OS 级不投递）；修饰掩码不分左右手（天然「任一侧」）；CapsLock/NumLock/Mod5 锁定位按 {0, LOCK, M2, M5} 全 8 变体抓取（i3 同款），KeyPress 剥离锁定位后查表；按住的主键 autorepeat 重复 KeyPress 由事件循环 held 位集吞（与 win `MOD_NOREPEAT` / mac 流过滤吞尾同语义——「按住不重复路由」的三平台落点）。Xwayland（rootless）下 compositor 先行处理按键，X 客户端 grab 对 Wayland 原生焦点可能不触发（M4 实测记录见 docs/05 §3.1）。
  - **从机注入前过滤（全平台统一）**：收到的事件流 → `HotkeyFilterSink`（包装映射引擎的 sink 适配层，**热键在映射之前**）→ 命中不注入本机 OS + 本地路由（Pause toggle 可用；Next/Prev/Home 无对端信令路径——`SwitchRequest` 主控→从机单向（docs/02 §4.2），反向信令属后续里程碑评估）。
- **切换路由语义（M4）**：Next/Prev → `SwitchRequest{target}` 信令握手（从机 `ReleaseAll` + `SwitchAck`；控制转移的完成——Ack 后的 `EnterScreen` 选点——依赖布局语义，随布局协商里程碑收口）；Home → 主控侧 `ReleaseControl`（`ReleaseAll` + `LeaveScreen{mods}` + 控制方向复位，完整闭环）；Pause → 暂停/恢复 toggle（FR-2.8）。切换类动作门控与 §5 边缘切换一致（仅 Running 未暂停、未被控）。
- Wayland 无全局热键 API 时的回退：GlobalShortcuts portal（compositor 支持）→ 否则启用"边缘双击"与"屏幕角落悬停"手势（设置中可关）。

## 5. 屏幕布局与跨屏路由（FR-1.3 / FR-2.1/2.2）

- 布局模型：每屏（含主控屏）是网格中的节点，记录 `left/right/up/down` 邻居（显式图，不强制几何精确对齐；几何模式为 P2）。ScreenId 全局空间与从机映射表见 [02-protocol.md](02-protocol.md) §4.2「ScreenId 跨机语义」。
- 边缘判定：光标本坐标到达主控屏边缘 ±0px 且存在该方向邻居 → 触发 `ScreenSwitch`：
  1. 主控：释放本机修饰键/按键（防卡键）→ 发 `EnterScreen{target, entry_pos}`（entry_pos 为边缘命中点在目标边的归一化比例）；
  2. 从机收到后 warp 指针到对应边缘位置（`target` 经映射表换算为本机屏），进入受控态（开始接收输入流）。
- **角落排除**（2026-11-22 第三轮补录）：光标同时贴**两条边**（x 极值带 ∩ y 极值带 = "角落区"）不触发任何方向的切换——**角落留给系统热区**（用户明确意图：角落 = KDE 触发角专用，边缘 = 切换）。判定面两处对称：主控切入（`edge_crossing` 纯函数——垂直边命中要求沿边坐标 y 严格内侧，水平边命中要求 x 严格内侧）与被控回切（主控状态机回报判定——正交轴回报也处于极值端点 → 角落驻留不回切，回报照常消化）。判定与触边同款精确语义（≤0 / ≥1）：容差带已把贴边样本收敛到端点值，角落驻留必然两轴同时命中端点；沿边坐标未收敛（非端点）= 非角落，照常触发。
- **鼠标按键门控**（2026-11-22 第三轮补录）：**任意鼠标键按下 → 不触发切入/回切**——拖文件/拖滚动条时滑到边缘被切换会中断拖拽。跟踪面 = 主控捕获流 MouseDown/Up 位图（受控与未受控事件都过捕获回调，同一位图；多键任一按下即门控；Paused 期跟踪照常，恢复后状态正确）：
  - 切入门控：传感器边缘命中上报在主控运行时丢弃（app 层判定面；传感器已闩锁——松开后须回内侧再推，防按键释放瞬间残留贴边误触发）；
  - 回切门控：主控状态机在回切判定前过滤（回报照常消化，免疫窗计数/武装复位语义不变；松开后的下一次同向移动再武装）；
  - 热键切换（SwitchRequest/ReleaseControl）不经此判定面——用户明确意图不受限。
- **KDE 触发角适配说明**（2026-11-22 第三轮补录，命中交用户实测）：进入落点死区 0.5% 使被控光标 warp 落点距屏角约 19px（Linux 实机 4K **宽轴**，3840×0.5%；左右缘切入内缩沿 x）/ 7.5pt（mac **宽轴**，1512 逻辑点×0.5%）——原 1% 死区下被控光标距角落恒差 38px（4K 宽轴×1%），kwin 触发角（通常要求进入屏幕角落 ~几 px 的角区）在被控端不生效的用户反馈由此缓解减半。角落排除（切入/回切两判定面）保证角落驻留不会被切换打断；被控光标在免疫窗后**持续推入**仍可到达虚拟极值（virt 0.0/1.0，回报探针日志可分辨 virt 是否到 max）——kwin 角落区是否命中以实测为准。
- **热键/托盘切换闭环**（FR-2.5，2026-11-19 补录，任务 11-19-m5-layout）：主控发 `SwitchRequest{target}` → 从机 `ReleaseAll` + `SwitchAck{ok}` → 主控收到 ok 确认后立即执行与边缘切换同型的接管序列（`ReleaseAll` + `LeaveScreen{mods}` + `EnterScreen{target, edge, ratio}`）：进入边 `edge` 从布局图推导（布局中邻接 target 的方向取对侧边；无邻边回退 target 左缘），`ratio` 默认 0.5（居中进入，后续里程碑可配）。
- **双向边缘回切**（2026-11-22 补录，两机联调反馈轮 2——取代上方原"从机侧反向移出边缘同理"的未实装占位；**同日语义改判**：触发边从"外侧边绕回"改为**面向主控的边**，见下）：受控屏（工作机）把光标滑出其**面向主控的边**即穿回主控——**空间连续穿行**：主控左缘滑出（向左移动）进入受控屏，用户在其**右缘**（面向主控侧）向右滑出即回到主控，与桌面连续移动的直觉一致（原"外侧边绕回"模型与直觉相悖，用户实测推翻后废弃）。**判定与决策全在主控侧会话状态机**（协议零新增，`MouseMoveAbs` kind 0x04 反向语义见 docs/02 §4.2 修订）：
  1. **回报**：被控方按 ~50ms 采样受控屏光标，经边缘容差带（±1 逻辑点）归一化后以 `MouseMoveAbs{screen: 受控屏全局 id}` 回报——边缘驻留回报端点值（0.0/1.0）；仅受控期间发送；
  2. **触发判定**（主控）：回报落在受控屏面向主控边的**极值**（恰 0.0/1.0）**且**上一回报周期内主控仍转发过朝该边方向 `MouseMoveRel`（"仍有同方向移动事件"）——边缘驻留无移动不触发、非极值不触发（防误触的双条件；同向武装随每条回报复位；切入惯性 1-2 帧 delta 行程远不足极值，天然不误触）。**角落排除**（2026-11-22 第三轮）：正交轴回报也处于极值端点（角落区驻留）→ 不触发——角落留给系统热区（KDE 触发角专用），见上"角落排除"。**按键门控**（2026-11-22 第三轮）：任意鼠标键按下 → 不触发（回报照常消化），见上"鼠标按键门控"；
  3. **触发动作** = ReleaseControl 同型释放（`ReleaseAll` + `LeaveScreen{mods}`，防卡键顺序同 §5 边缘切换）+ **空间连续落点** warp：光标落在主控上与穿出方向衔接的边缘内侧（受控屏面向边滑出 → 主控**背向受控屏的边缘**内缩，与进入落点死区同源内缩 0.5%（11-22 第三轮：1%→0.5%——5% 感知距缘太远、1% 仍偏远的用户回调延续；防弹回真实防线是容差带+防抖+回切免疫窗，mac 7.5pt 仍 7.5 倍于容差带）；沿边 ratio 保持滑出点位置）；
  4. **死区交互**：进入落点死区恰在回切触发边（面向主控边）内侧 0.5%——切入后无移动不武装、惯性行程不足极值，不误触；主动推向该边滑出 = 明确回切意图；落点内缩保证回切后不立即再触发（背向边缘无邻边时恒安全；免疫窗 ~800ms 承接切入惯性）；
  5. **范围裁定**：**单跳**——受控屏面向主控边滑出恒回主控；多屏链受控屏回邻屏（内侧/外侧多跳路由）移交后续里程碑（当前部署 2 屏）。受控屏**远离主控的边**滑出不触发；
  6. **平台矩阵**：回报采样面随主控边缘轮询（macOS 实装 + Linux uinput/X11；win 采样源移交后续里程碑——非采样平台无回报，单向边缘切换不受影响）。
- **死区**：切换后 150ms 内忽略同方向再次触发（防抖）；`锁屏边界`（FR-2.8）= 布图上临时摘除该方向边。

## 6. 测试要求

1. 键码转换表：每平台全表 golden 单测（fixture: `keytables/<platform>.csv`，来自各平台头文件生成，脚本入库 `xtask`）。
2. 映射引擎：属性测试（proptest；实现期以零新依赖的确定性种子伪随机驱动同一不变式，见 §4 修订记录 check 补录）——随机事件序列经映射后，**注入流的修饰键 OS 状态配对性不变式**（任一 mapped 修饰键的注入 ↑ 前必处于按下态——不存在"无中生有的抬起"；序列终点全部键抬起不卡键）恒成立；4.2 同目标引用计数下事件级"每 ↓ 必有 ↑"不成立（抑制中间 ↑ 是设计语义，见 §4.2 修订）；4.3 拦截状态机的 flush 分支全覆盖。
3. 跨屏路由：布局图遍历测试（随机布局 + 随机边缘序列，断言目标屏与 entry_pos）。
4. 手动验收：映射规则三平台各执行 [05-platform.md](05-platform.md) 的清单 §"映射"。
