# design: 热键切入后边缘回切失效与快速过边停滞

需求与验收见 [prd.md](./prd.md)，根因证据见 [research/root-cause-analysis.md](./research/root-cause-analysis.md)。行号对应 commit `1b02abb`。

## 1. 边界

| 层 | 改动 | 不变 |
| --- | --- | --- |
| core `layout.rs` | 角落区比例常量与判定函数；`edge_crossing` 的角落判定 | 邻接图、布局合并、落点换算 |
| core `session.rs` | `SwitchAck` 接管的回切边；免疫窗按时间；受控上下文字段；回切判定共用角落比例；`ReleaseControl` 落点注释 | 转移表、动作序列、wire |
| core `sink.rs` / `mapping.rs` / `mock/sink.rs` | `InputSink::resync_cursor` 默认方法、Box 与 MappingSink 透传、记录型 sink 计数 | 既有四个方法的契约 |
| input `platform/wayland.rs` | `release_all` 保留虚拟位置；`resync_cursor` 实现 | REL 注入、`warp`、`cursor_normalized` |
| input `platform/macos.rs` | `CaptureSource::anchor_point` 只读访问器 | 捕获、锚定、注入逻辑 |
| app `hotkey_rt.rs` | `HotkeyFilterSink::resync_cursor` 透传 | 过滤与路由逻辑 |
| app `edge.rs` | 默认节拍 10ms；角落相关测试改写 | 判定状态机 |
| app `session_rt.rs` | 门控收紧与防抖；保持原位切入的识别与校准调用；热键回主控落点改走锚点 | select 分支结构、取消安全形态 |

红线对照：core 零平台依赖、零 I/O（时间只经 `Clock` 注入）；平台改动全部在 feature 门控模块内；无新增依赖；无 `unsafe`；无协议报文变更。

## 2. core：回切边、免疫窗、角落区（R1 / R5 / R7）

### D1 热键路径回切边 = 进入边

`execute_input` 的 `SwitchAck{ok:true}` 接管分支（`session.rs:1116-1155`）：

```rust
let entry_edge = crate::layout::entry_edge_into(&self.layout, target).unwrap_or(ScreenEdge::Left);
self.controlling = Some(ControllingContext {
    target,
    outer_edge: entry_edge, // 原 opposite_edge(entry_edge)
    taken_over_ms: now,
});
```

`entry_edge_into` 返回的已是受控屏的进入边，即受控屏面向主控的边，正是 `ControllingContext.outer_edge` 的定义。落点换算 `edge_return_landing` 不动：`facing_edge = opposite(outer_edge)`，落到邻接主控屏面向受控屏的那条边内侧。无布局时回退 `Left`，与边缘路径的默认拓扑（主控右滑 → 受控屏左缘进入）一致。

### D2 免疫窗按时间

```rust
struct ControllingContext { target: ScreenId, outer_edge: ScreenEdge, taken_over_ms: u64 }
const RETURN_IMMUNITY_MS: u64 = 800; // 原 16 条回报 × 50ms 的等价换算

fn execute_input(&mut self, event: InputEvent, now: u64) -> Vec<SessionAction>;
fn execute_local(&mut self, cmd: LocalCmd, now: u64) -> Vec<SessionAction>;
fn edge_return_actions(&mut self, screen, x, y, now: u64) -> Option<Vec<SessionAction>> {
    // 免疫窗：now.saturating_sub(ctx.taken_over_ms) < RETURN_IMMUNITY_MS → None
}
```

`now` 已由 `step()`（`session.rs:861`）读取并传到 `execute_frame`，这里只是继续下传。`reports_since_takeover` 字段与 `RETURN_IMMUNITY_REPORTS` 常量删除。

### D9 角落区 5%（R7）

现状：两处角落判定都是"另一轴也到端点"（`layout.rs:128-129`、`session.rs:1725`），配合两侧 ±1 点的容差带，角落区只有约 1 点见方。

改法：core 新增单一常量与判定函数，两处共用。

```rust
// layout.rs
/// 角落区比例：沿边坐标落在 [0, RATIO] ∪ [1 − RATIO, 1] 即角落区（含端点）。
pub const CORNER_RATIO: f32 = 0.05;
pub const fn in_corner_band(v: f32) -> bool { v <= CORNER_RATIO || v >= 1.0 - CORNER_RATIO }
```

- `edge_crossing`：命中边的沿边坐标（垂直边取 `y`，水平边取 `x`）落在角落带则不触发，替换现有的 `x_extreme` / `y_extreme` 互斥判定。
- `edge_return_actions`：`at_orthogonal_extreme` 改为 `in_corner_band(正交轴回报)`，函数随之改名并更新注释。

两处判定的输入都是归一化坐标（主控传感器的 `normalized_in`、工作机回报），所以**改动全部在主控侧的 core**：工作机的四角由主控依据回报判定，工作机不需要改动，主控也不需要知道工作机分辨率。

不需要动的地方：传感器容差带（仍 ±1 点，只负责把贴边样本收敛到端点）、进入与回切落点的内缩（仍 0.5%）、`edge_point` 与 `edge_return_landing`。

现有测试里有三处把"离角 1.5 个点即触发"钉死了，需要改写：

- `layout.rs` 的 `edge_crossing_corner_exclusion`（含"y=0.001 未收敛 → 触发"）
- `session.rs` 的 `edge_return_corner_exclusion`
- `edge.rs` 的 `corner_excluded_and_interior_ratio_preserved`

新增边界用例取 `CORNER_RATIO ± 0.001`，相对常量书写，不测精确端点。

## 3. 位置模型校准钩子（R2）

### D3 trait 钩子与透传

`InputSink`（`sink.rs:51`）新增默认方法，形态与 `cursor_normalized` 一致：

```rust
/// 把真实光标校准到实现的位置模型（保持原位切入后调用一次，11-24）。
/// 默认无操作——位置模型即真实光标的实现（mac / X11 / win / mock）无需校准；
/// 位置模型为虚拟的后端（uinput）覆写。
fn resync_cursor(&mut self) -> Result<(), SinkError> { Ok(()) }
```

三处透传缺一不可，否则平台覆写不可达：`Box<dyn InputSink + Send>`（`sink.rs:14`）、`MappingSink`（`mapping.rs:923` 旁）、`HotkeyFilterSink`（`hotkey_rt.rs:233` 旁）。`RecordingInputSink`（`mock/sink.rs`）覆写为计数并暴露 `resync_count()`，供透传与调用点断言，做法同既有的 `releases` 计数。

### D4 uinput 实现

- `release_all`（`wayland.rs:572`）删除虚拟位置复位。该复位是 11-20 ABS 模式的遗留；现行 REL 注入下不产生任何可见效果，只破坏位置模型。
- `resync_cursor`：按 `warp` 的同款序列发 `ABS_X` / `ABS_Y` 到 `(virt_x, virt_y)`，不改 virt。

### D5 app 调用点

纯函数识别"保持原位切入"：

```rust
fn is_preserved_entry(actions: &[SessionAction]) -> bool {
    let entered = /* 含 Notice::EnteredControl */;
    let warped = /* 含 WarpPointer */;
    entered && !warped
}
```

`drive_session` 收帧分支在 `apply_and_send` 成功之后调用：`is_preserved_entry(&outcome.actions)` 为真则 `sink.resync_cursor()`，失败只 `warn!`（target `session::rt`），不打断会话。热键切换时 `SwitchRequest` 与 `EnterScreen` 是两帧、两次循环迭代，校准时 `ReleaseAll` 已经执行完。

## 4. app：门控、防抖、节拍（R3 / R5）

### D6 门控与防抖

```rust
fn edge_hit_allowed(state, control_dir, mouse_buttons_down) -> bool {
    matches!(state, SessionState::Running { paused: false })
        && control_dir == ControlDir::None   // 原 != PeerControlsLocal
        && !mouse_buttons_down
}
const EDGE_SWITCH_DEAD_TIME: Duration = Duration::from_millis(150);
fn edge_dead_time_elapsed(control_changed_at: Option<Instant>, now: Instant) -> bool;
```

`drive_session` 在每轮循环顶部比较 `session.control_dir()` 与上一轮的值，变化沿记录 `Instant::now()`。select 分支内的 `continue` 会跳到下一轮顶部，所以各分支（收帧、热键、托盘切换）引起的变化都能被记到。边缘分支的判定顺序：本机屏命中过滤 → 门控 → 防抖 → `EdgeCrossing`；被拒命中走 debug 留痕。

### D7 节拍

`edge.rs:59` `DEFAULT_POLL_INTERVAL` 50ms → 10ms，主控边缘轮询与工作机回报共用该常量，无需新增常量。`EdgeSensor::poll_interval` 接口不变。

## 5. 热键回主控落点 = 切入锚点（R4）

### D8

现状：core `execute_local(ReleaseControl)`（`session.rs:1323-1346`）产出 `WarpPointer{0, 0.5, 0.6}`，app 立即对 sink 应用（光标跳到主屏中心），随后 suppress=false 拍再把同一落点重放一次。

改法：core 不动，macOS 主控在 app 层覆盖落点，走既有的 `pending_release_warp` 重放链。

- `CaptureSource::anchor_point(&self) -> Option<(f64, f64)>`（`macos.rs`，只读 `pin_point`）。
- 纯函数 `strip_home_warp(actions: &mut Vec<SessionAction>) -> bool`：剥离序列里的 `WarpPointer`，返回是否剥离到。
- 热键分支 `HotkeyAction::ReleaseControl`：`run_local` 之后，有捕获源时一律剥离 core 的落点 warp；若 `last_suppress` 为真（suppress=false 拍即将到来），`pending_release_warp = capture.anchor_point()`。锚点在重关联**之后**重放才生效，这正是 11-22 回切落点覆盖修复已经验证过的路径。
- 删除热键分支原有的"受控态产出的 WarpPointer 缓存到 suppress=false 拍重放"代码块（`session_rt.rs:1933-1944`）：剥离后该分支不再出现 `WarpPointer`，此块成为死代码。
- core `ReleaseControl` 的注释补一句：macOS 主控在 app 层以切入锚点替换此落点。

边界：

| 情形 | 结果 |
| --- | --- |
| 锚点不可得（切入时光标查询失败） | `pending_release_warp = None`，光标停物理鼠标位 |
| 接管后 10ms 内就按了回主控（suppress 尚未翻开，`last_suppress` 为假） | 只剥离、不设重放；没有翻拍，光标原地不动 |
| 边缘切入的会话用热键回主控 | 同样落锚点，即受控期间所见位置 |
| 非 macOS 主控（无捕获源） | core 的落点 warp 照旧生效 |

## 6. 文档同步（R6）

- **docs/03 §4.6**："切换路由语义"的 Home 一句补上落点：光标回到切入锚点。
- **docs/03 §5**
  - "热键/托盘切换闭环"补一句：切入后的回切触发边 = 进入边，与边缘路径一致；`preserve_cursor=1` 切入后被控端把真实光标校准到切出时的位置。
  - "角落排除"条目改写：角落区 = 每屏四角、沿屏宽与屏高各取 5% 的矩形；两处判定面共用 core 的同一比例；判定在主控侧完成。
  - "双向边缘回切"第 1 条采样周期 `~50ms` → `~10ms`；第 2 条的角落判定随之改写；第 4 条免疫窗注明按时间 800ms。
  - "死区"条目改写为实现事实：受控期间主控边缘命中忽略；控制关系变化后 150ms 内忽略切入。
  - 追加 2026-11-24 修订记录。
- **docs/05 §1.2**：锚定周期 50ms → 10ms（会话输入拍）；注明受控期主控边缘命中由 app 门控忽略；注明锚点同时是热键回主控的落点。
- **docs/05 §4.5**：补 uinput 位置模型三条（`warp` 与 `resync_cursor` 同步 virt、`release_all` 保留 virt、回报取自 virt）；把 `wayland.rs:457-462` 注释里 11-22 "受控移动注入改回 REL" 的实测记录转录进文档，不新增平台断言。
- **CHANGELOG**：新增 `## [Unreleased]` 的 Fixed / Changed。
- **`.trellis/spec`**
  - `rust-core/architecture-rules.md`：新增"切换/回切语义（11-24 定稿）"——任何新增接管路径都必须产出与边缘路径相同的 `outer_edge`；回切免疫窗按时间、与回报频率解耦；切入与回切的角落判定只经 `layout::in_corner_band` 一处；macOS 主控的落点一律经 `pending_release_warp` 在重关联之后重放。
  - `rust-platform/platform-code.md`：修正"受控注入必须 ABS 虚拟光标"为现行代码事实；新增 uinput 位置模型纪律与 macOS 受控期边缘门控。
  - `rust-core/logging-targets.md`：`input::` "无 target 使用"的审计结论与现状不符（`macos.rs`、`wayland.rs` 已在用），核对后更正。

docs/02 不动：`preserve_cursor` 的 wire 定义与其"光标留在切出时的位置"的语义都不变，uinput 的校准正是让实现符合这句定义。

## 7. 兼容性

- **新旧版本互通**：无 wire 变更。旧主控 + 新工作机时 R2 生效；新主控 + 旧工作机时 R1 / R3 / R4 / R7 与主控侧的 R5 生效，工作机回报仍是 50ms，回切延迟不变但免疫窗按时间仍然正确。R7 的两处判定都在主控侧，升级主控后工作机的四角立即生效，与工作机版本无关。
- **配置**：无新增或变更的配置项。
- **平台编译面**：uinput 改动只在 `wayland` feature + linux target 下编译；`anchor_point` 只在 `macos` feature 下编译；其余平台经默认方法无操作。

## 8. 取舍

- **唤醒频率**：主控空闲时边缘轮询 20Hz → 100Hz；工作机受控期回报 20Hz → 100Hz。每次是微秒级查询与一个约 35 字节的报文。把边缘轮询并入输入拍可以省掉这部分唤醒，但要重排 select 分支体，不在本任务做。
- **放弃"受控中链式滑边"**：原门控注释把它列为 FR-2.2 的 M3 路由基础。链式切换尚未实现；主控光标被锚定时也不存在有意滑边。将来若实现，应走显式意图（热键 / 托盘），或只在光标能被真正冻结的平台上另开入口。
- **uinput 上"保持原位"的实现方式**：工作机自己的物理鼠标在未受控期间移动过光标时，热键切入瞬间光标会跳回上次受控结束的位置。这是没有"查询真实光标"入口的平台上的唯一可行解，且与 docs/02 的定义一致。
- **锚点是切入位置内缩 90pt 后的点**（`macos.rs:224`）：切入瞬间光标离屏边不足 90pt 时，回主控落点比切入时的真实位置更靠内，但与受控期间所见位置一致。
- **角落区 5%（用户选定）**：每条边两端各 5% 不能切换，中间 90% 可用。取比例而非固定像素，是因为回切判定在主控侧、只有归一化坐标；矩形按屏宽与屏高各取同一比例，在 16:9 屏上略扁。

## 9. 回滚

无数据迁移、无配置迁移，按组还原即可。`session.rs` 与 `session_rt.rs` 被多组共用，按组回滚时按代码块还原，不要整文件 checkout。

| 组 | 内容 | 约束 |
| --- | --- | --- |
| G1 | D1 + D2（core 回切边与免疫窗） | 独立 |
| G2 | D3 + D4 + D5（位置模型校准） | 独立 |
| G3 | D6 + D7（门控防抖 + 节拍） | **必须同进同退**：只提节拍会让受控期误触发更频繁，只收紧门控则停滞感不变 |
| G4 | D8（热键回主控落锚点） | 独立 |
| G5 | D9（角落区 5%） | 独立；回滚后三处改写的测试一并还原 |
| G6 | D10 + D11 + D12 + D13（回切行程补偿） | 独立于 G1–G5；D11 的文档必须先于 D10 / D12 / D13 落地 |

## 10. 回切行程补偿（R8）

### 背景

三段行程在回切时丢失（research §8）：① 工作机光标顶边期间被屏幕边界截掉的位移（采样最多晚 10ms）；② 回切触发后到主控恢复本地输入之间被丢弃的位移（现在要等下一个输入拍，≤10ms）；③ 主控最后转发的一批在工作机采样之后才到，没算进去。

### D11 协议语义（docs/02 先改）

反向回报 `MouseMoveAbs{screen, x, y}` 的取值范围扩展到端点之外，wire 布局不变：

- `x < 0`：光标被推出受控屏左边界，`|x|` 是自到达该边以来的累计行程；`x > 1`：推出右边界，行程 = `x − 1`。`y` 同理对应上、下边界。
- 单位是控制方转发的 `MouseMoveRel` delta 单位（即主控的点）。无越界时取值仍在 `[0, 1]`，与 11-22 契约一致。
- 兼容：旧主控按 `x ≤ 0` / `x ≥ 1` 判极值，越界值照常触发、幅值被忽略；旧工作机只发 `[0, 1]`，新主控算出的行程为 0。解码只校验有限性，越界值可以通过。golden fixtures 不变，不提升版本。

### D10 工作机：uinput 度量

- `UinputInjector` 新增 `overflow_x / overflow_y: f32`。
- 纯函数 `advance_axis(virt, step, max, overflow) -> i32`：越过上界累加超出量、越过下界累加负量、严格回到屏内即清零；贴边且该轴无位移时保留。
- `inject(MouseMoveRel)` 两轴都经它推进；`warp` 与 `release_all` 清零。
- `cursor_normalized`：`overflow < 0` 直接回报该负值，`> 0` 回报 `1.0 + overflow`，否则维持原端点收敛逻辑。
- `InputSink::cursor_normalized` 的契约文档同步。macOS / X11 后端不度量，回报仍为端点值。

### D12 core

- 纯函数 `beyond_edge(v) -> f32`（`v < 0 → v`，`v > 1 → v − 1`，否则 0）。
- `Notice::EdgeReturnedToHost` 增加 `overflow: (f32, f32)`，由 `edge_return_actions` 从回报解出。触回切判定的条件不变（角落带仍只看沿边坐标）。

### D13 app：主控侧补偿

- **落点补偿**：收帧分支缓存落点时，`pending_release_warp = compensated_release_point(...)`，即原落点加上（回报行程 + 在途批 + 尾批）的向量，再夹回本机屏内，四周至少留 `EDGE_RETURN_INSET` 换算的点数。
- **在途批**：输入拍每成功发出一批，记下发出时刻与批内 `MouseMoveRel` 之和；回切触发时距发出不足 3ms（`IN_FLIGHT_WINDOW`）的那批计入，纯函数 `in_flight_batch`。
- **排空后立即翻拍**：回切触发时（macOS）先把捕获队列里翻拍前被本地拦截的事件排空，鼠标位移之和计入落点；随即 `set_suppress_local_input(false, 落点)`，不再等下一个输入拍。排空的事件仍过按键位图与热键过滤，只是不转发。翻拍之后的新事件原生送达本机。输入拍里的翻拍代码抽成 `reconcile_local_input_suppress`，两处共用。
- 热键回主控（G4）路径不变，仍在输入拍翻拍。
- 全部在 `#[cfg(feature = "macos")]` 内；纯函数（落点补偿、在途批判定、位移求和）有单测，排空后翻拍的接线只有编译保证。

### D14 文档更正（实测事实）

2026-11-24 实测：同一 `/Applications/XControl.app` 路径下换上新构建后，主控日志出现 `CGEventTapCreate 返回空（参数或权限被系统拒绝）`，重新授权辅助功能后恢复。结论：ad-hoc 签名的应用，辅助功能授权按签名匹配，重新打包就会失效，与路径无关。更正 `scripts/macos/dmg.sh` 头注、docs/10 §1.1、docs/05 §1 中"TCC 按路径记忆"的说法。

### 文档同步

docs/02 §4.2（反向回报取值范围）、docs/03 §5 回切第 3 条（落点补偿）、docs/05 §4.5（uinput 行程度量）、CHANGELOG，以及 D14 列出的三处。

## 11. 未授权引导与应用图标（R9 / R10）

### D15 未授权时的系统引导与自动恢复（macOS）

现状：`check_accessibility(prompt)` 的弹框分支从未被调用；主控 `host_capture_source()` 失败只 `warn!` 后静默降级，且捕获源只在会话建立时创建一次，授权后必须重启应用。

- **启动引导**：角色确定后（`main.rs` 的运行时启动路径，进入 host / client 循环之前），`check_accessibility(false)` 为 `Denied` 时调用 `check_accessibility(true)` 触发系统引导框，并 `warn!` 留痕。两种角色都做：主控要捕获、从机要注入，都需要辅助功能。
- **自动恢复**：`drive_session` 输入拍内，角色为 Host 且 `capture.is_none()` 时，按 `CAPTURE_RETRY_INTERVAL`（1s）静默查询 `check_accessibility(false)`；变为 `Granted` 即 `capture = host_capture_source()`，`info!` 留痕。
- **失败日志**：`host_capture_source` 的告警改为可执行的说法（去系统设置授权，授权后自动恢复）。
- 待实测的平台行为：应用已在辅助功能列表里但未勾选时，系统引导框是否出现。部署时确认，结论回写 docs/05 §1。

### D16 应用图标

- `scripts/macos/icon.swift`：CoreGraphics 绘制 iconset（10 个尺寸），图形比例与 `xcontrol-tray` 的 `draw()` 同源。
- `scripts/macos/icon.sh`：调用上面的脚本并 `iconutil` 合成 `.icns`。
- `scripts/macos/AppIcon.icns`：入库产物，`dmg.sh` 只读取它，打包不依赖 swift。
- `dmg.sh`：拷入 `Contents/Resources/AppIcon.icns`，`Info.plist` 加 `CFBundleIconFile = AppIcon`。

## 12. 回切行程补偿的平滑回放（R11）

用户实测 R8 的一次性补偿在快速移动时表现为瞬移。改法（仅 macOS 主控；工作机不变）：

- 回切触发处落点改取**未补偿**的贴缘基点（`release_warp_point`），翻拍重放的就是它。
- 夹取后的补偿量（`compensated_release_point` 的终点减基点）由纯函数 `replay_slices` 按 ease-out（二次曲线）切成 `RETURN_REPLAY_STEPS`（6）步，逆序存入 `cursor_replay` 队列。
- 输入拍在对账之后调用 `replay_cursor_step`：每拍经新增的 `CaptureSource::shift_cursor`（读当前位置再相对 warp）补一步，约 60ms 完成；期间物理鼠标位移照常叠加。已翻到受控即清空队列；某步平移失败则告警并清空。
- 日志 `边缘回切落点补偿` 增加 `replay_steps` 与 `replay_total` 两个字段。

取舍：回放期间光标比手势"慢"最多约 60ms，换来过程连续。步数是手感参数，集中在 `RETURN_REPLAY_STEPS`。
