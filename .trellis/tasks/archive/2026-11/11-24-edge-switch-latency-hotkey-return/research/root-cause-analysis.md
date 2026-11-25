# 根因分析：热键切入后边缘回切失效 / 快速过边停滞

任务创建时的代码走读结论。行号对应 commit `1b02abb`。证据等级分三类：

- **走读确认**：由代码与规范文档直接得出，可用单测锁死。
- **项目实测记录**：引用 docs/05 §1.2 的探针实测，本任务未重新实测。
- **未验证假设**：本任务不改，仅留档。

## 1. 切换链路时序（修复前）

**切入（主控 → 工作机）**

1. 光标到达主控屏边（T0）。边缘轮询周期 50ms（`edge.rs:59`），在 `[T0, T0+50ms]` 内命中。
2. `EdgeCrossing` → 出站 `LeaveScreen` + `EnterScreen{preserve_cursor=0}`；工作机 warp 到进入点。
3. 主控下一个输入拍（≤10ms）把 tap 切到 suppress，锚点取切入位置内缩 90pt（`macos.rs:224`），此后每拍 warp 回锚点。

检测窗内的物理位移全部丢弃（`capture_forward_allowed` 在接管前为 false）。

**回切（工作机 → 主控）**

1. 工作机光标到达面向主控的边（T0）。回报节拍周期 50ms（`session_rt.rs:1340`），在 `[T0, T0+50ms]` 内采样，贴边样本经 ±1 点容差带收敛为 0.0/1.0。
2. 主控状态机判定：极值 + 同向武装（上一回报以来转发过朝该边的 `MouseMoveRel`）+ 非角落 + 无鼠标键按下 + 免疫窗外 → 回切，立即执行 `WarpPointer`。
3. 主控下一个输入拍重放落点（`set_suppress_local_input(false, point)`）。

快速移动时，光标在 T0 到切换完成之间被钳在边缘不动，这就是用户描述的"撞边停住"。慢速移动时同样的时延只对应几像素，不可感知。

## 2. 成因证据

### C1 热键路径回切边取反（走读确认）

- 边缘路径：`session.rs:1295` `outer_edge: opposite_edge(edge)`，`edge` 是**主控滑出边**；对侧恰为受控屏的进入边（面向主控）。
- 热键路径：`session.rs:1134-1138` `entry_edge = entry_edge_into(..)`（`layout.rs:212`，返回的**已是进入边**），随后 `outer_edge: opposite_edge(entry_edge)` 得到远离主控的边。
- 规范：docs/03 §5 双向边缘回切第 5 条"受控屏面向主控边滑出恒回主控……远离主控的边滑出不触发"。
- 实际表现（`[主控 | 工作机]`）：热键切入后朝主控方向（工作机左缘）推边无效；往工作机右缘推反而触发，落点算到主控**左缘**（环绕语义，11-22 已废弃的旧模型）。
- 测试缺口：`edge_return_*` 全组用例只经 `EdgeCrossing` 接管；`switch_closed_loop_with_layout_and_entry_edge` 止于 warp 断言，未验证回切边。

### C2 uinput 位置模型脱节（走读确认）

- `wayland.rs:360` 构造时 `virt = (w/2, h/2)`；`:572-576` `release_all` 复位回中心；`:558` `warp` 同步 virt；`:471` `MouseMoveRel` 累加 virt；`:589-606` `cursor_normalized` 上报 virt。
- 热键切换时序：`SwitchRequest` → `ReleaseAll`（virt 回中心）→ `SwitchAck` → `EnterScreen{preserve_cursor=1}`（core `session.rs:1072` 跳过 warp）。
- 结果：回报位置 = 中心起算的虚构轨迹，真实光标在别处。真实光标到边时回报未到极值，回切不触发。
- mac / X11 后端：`cursor_normalized` 读真实光标（`macos.rs:883`、`x11.rs:863`），不受影响。

### C3 两处 50ms 节拍（走读确认）

- 主控边缘轮询：`edge.rs:59` `DEFAULT_POLL_INTERVAL`，`session_rt.rs:1329` 建 ticker。
- 工作机回报：`session_rt.rs:1340` 同一常量建 ticker。
- 工作机采样成本：uinput 是纯内存读取（`wayland.rs:589`）；mac 是一次 `CGEventGetLocation`。10ms 节拍无成本障碍。
- 传输层已强制 `TCP_NODELAY`（`transport.rs:375`、`serve.rs:398`），不是延迟来源。

### C4 受控期间主控误触发（门控走读确认 + 冻结行为引自项目实测记录）

- 门控：`session_rt.rs:2164-2172` 条件是 `control_dir != PeerControlsLocal`，`LocalControlsPeer` 放行；`execute_local(EdgeCrossing)`（`session.rs:1274`）对已受控无防护，会重发 `LeaveScreen` + `EnterScreen` 并重置免疫窗。
- 工作机侧后果：`LeaveScreen` → `ReleaseAll`；`EnterScreen` → warp 回进入点（`session.rs:1082-1094`、`:1057-1081`）。
- 冻结行为：docs/05 §1.2 记录 macOS 26 非前台进程下 hide / associate 均无效，"唯一可达实现 = 每拍锚定 warp"；代码里回拉发生在 10ms 输入拍（`session_rt.rs:1813-1815`），文档写的是 50ms，需同步。
- 触发条件：两次回拉之间光标位移超过锚点到屏边的 90pt，即朝该方向速度约 9000pt/s 以上；轮询恰好落在光标贴边的时刻。
- **与 C3 的耦合**：边缘轮询改 10ms 后，与回拉同周期且相位接近，轮询时刻接近每周期位移最大点，误触发概率上升。两者必须同一次修。

### C5 回主控落点假设屏 0（走读确认）

- `session.rs:1340-1344` `ReleaseControl` 产出 `WarpPointer{screen: 0, x: 0.5, y: 0.6}`。
- `macos.rs:175` 文档："顺序即系统枚举序（不保证稳定）"；`host_edge_sensor`（`edge.rs:469`）自己也是查 `is_main` 的下标而非假设 0。
- 仅影响主屏不在枚举首位的 macOS 多屏主控。

### C6 热键回主控落点跳到屏幕中央（追加需求，走读确认）

- core `session.rs:1323-1346`：`ReleaseControl` 产出 `WarpPointer{0, 0.5, 0.6}`，注释写明是 11-20 的"Home 回主屏内侧"修复。
- app：`apply_and_send` 立即把该 warp 应用到 sink；`session_rt.rs:1933-1944` 再把它缓存为 `pending_release_warp`，在 suppress=false 拍重放。光标因此从锚点跳到屏幕中央。
- 锚点：`macos.rs:947` `pin_point`，`set_suppress_local_input(true)` 时捕获（`:1103-1117`，经 `entry_anchor_clamped` 内缩 90pt），false 时清空（`:1156-1159`）。没有对外访问器。
- 文档：docs/03 §4.6 只写了 `ReleaseControl` 的释放序列，从未规定落点。
- `ReleaseControl` 的唯一产出点是热键分支（`session_rt.rs:1917-1921`）；托盘"直接切换"只产出 `SwitchRequest`。

**为什么走"重关联之后重放"而不是"不 warp、让光标自己停在原地"**：仓库里对 `CGAssociateMouseAndMouseCursorPosition` 的两处记录互相冲突。docs/05 §1.2 记录解关联后"光标仍跟随物理鼠标"；`macos.rs:1143-1149` 的注释记录重关联"会把光标拉到物理鼠标当前位置"，并以之为 11-22 落点覆盖修复的依据。两种语义下，"把锚点作为 `release_point` 传入、在重关联之后重放"都能落到锚点；"传 `None`"只在其中一种语义下成立。选前者，不需要先裁决平台语义。

### C7 角落排除范围过小（追加需求，走读确认）

- 判定：主控切入 `layout.rs:128-129`（`x_extreme` / `y_extreme` 互斥）；工作机回切 `session.rs:1725` `at_orthogonal_extreme`（正交轴恰为 0.0 / 1.0）。
- 容差带：传感器 `edge.rs:66` ±1 点；回报侧 `wayland.rs:627` `abs_normalized`（≤1 → 0.0，≥ max−1 → 1.0）、`macos.rs:271` `point_snap_normalized`、`x11.rs:327` `pointer_snap_normalized`。合起来每个角落只有约 1 点见方。
- 钉死旧行为的断言：`layout.rs:551-552`（"y=0.001 未收敛 → 触发"）、`edge.rs:687-691`（"998.5 → 触发"）。`session.rs:3288` 起的角落用例只喂精确角点，结论不变但需要补带内用例。
- 影响面排查（规划时做）：`layout.rs`、`session.rs`、`edge.rs` 三个测试模块里，除上述断言外的沿边坐标都落在 5% 角落带外。`xcontrol-net` 的 `tests/chain.rs` 与 `tests/scenarios.rs` 直接调用 `LocalCmd::EdgeCrossing`，不经角落判定。
- 决策：用户选定 5%。单位取比例而非固定像素，因为回切判定在主控侧，主控手里只有归一化坐标。两处判定都在主控侧，工作机无需改动。

## 3. 未验证假设（本任务不改）

- **H1 mac↔mac 场景注入器突发丢增量**。`MacInjector::inject(MouseMoveRel)`（`macos.rs:655-678`）逐事件"读光标 → warp"。同一突发内连续多条时，若 warp 对紧随其后的读不是立即可见，会丢增量。当前部署的工作机是 Linux（uinput REL 路径），不受影响。验证方法：实机探针"读 → warp(+N) → 立即读"。
- **H2 受控期主控光标顶到硬边缘时 delta 是否断流**。docs/05 §1.2 记录"解关联后 HID 事件 delta 为硬件增量，无屏缘钳位"，但该对照实验是在**停用锚定**下做的；`macos.rs:1105-1113` 的注释记录过相反观察（"位置差分 delta 恒 0"）。两处记录冲突，未实测。
- **H3 捕获 tap 被系统禁用后不自动重启**（`macos.rs:41`、`:1402`）。已知限制。

## 4. 否决的备选方案

| 备选 | 否决理由 |
|---|---|
| 事件驱动回报（仅贴边时回报，基线仍 50ms） | 免疫窗按条计数时贴边驻留会让窗口缩到约 160ms；改按时间后与均匀 10ms 等价，取更简单的均匀 10ms |
| 边缘轮询并入输入拍成单一快节拍 | 省唤醒、时序确定，但要重排 select 分支体；收益是功耗，与本任务目标无关，留作后续 |
| 主控侧用转发增量外推工作机光标、提前判定回切 | 需要工作机屏幕尺寸与"增量↔点"比例，都不在协议里，会引入报文变更 |
| uinput 的位置校准下沉到 sink 内（首条 MoveRel 时隐式 warp） | 键盘/点击先于移动到达时位置仍不一致；显式钩子语义更清晰 |
| 增大锚点内缩量掩盖 C4 | 只把触发阈值从约 9000pt/s 抬高，不消除问题 |
| 改 mac 注入器为纯跟踪目标、不回读光标 | 屏幕边缘钳位会制造"死区"，比现状更差 |

## 5. 按需阅读指引

- `docs/05-platform.md` 约 48KB，超过上下文注入上限（32KB）会被截断。需要原文时直接读两段：§1.2 macOS 受控光标冻结实测记录（约 73–91 行）、§4.5 Wayland 输入实装记录（约 425–500 行）。
- `crates/xcontrol-app/src/session_rt.rs` 的 `drive_session` 约 900 行，收帧 / 边缘 / 回报 / 输入 / 热键五个分支在约 1355–1966 行。

## 6. 验证能力边界

- **本机可验证**：core / app / input(macos) 的单测与 clippy；uinput 改动对 `x86_64-unknown-linux-gnu` 的交叉 check（目标已安装，依赖能否交叉构建待执行时确认）。
- **本机不可验证**：真实鼠标移动下的平台事件语义；Linux 工作机上的实际落点与回切。这两项交实机验收（prd AC3、AC7）。

## 7. 实现后记（2026-11-24，实现与质量检查之后）

- **交叉 check 结果**：`xcontrol-input` 对 `x86_64-unknown-linux-gnu`（wayland、x11 两个 feature）与 `x86_64-pc-windows-msvc`（windows feature，`CC_x86_64_pc_windows_msvc=nonexistent-cc` 配方）的 check 均通过。
- **未跑的测试**：三个非 `#[ignore]` 的 macOS 默认测试会注入真实输入——`inject_key_updates_tracking_regardless_of_permission`（注入真实 Ctrl 按键）、`inject_harmless_event_is_ok`（移动光标）、`capture_start_poll_stop_smoke`（创建事件 tap）。实现与检查两轮都用 `--skip` 排除；`cargo test --workspace` 不启用 macos feature，所以也没跑到它们。它们不覆盖本次改动的代码。用户不在用机器时补跑 `cargo test -p xcontrol-input --features xcontrol-input/macos`。
- **CI 缺口**：macOS job 的 workspace 步骤不带 feature，`xcontrol-app` 里 `#[cfg(feature = "macos")]` 的代码（本任务 G2–G4 的接线）在 CI 里不编译。本机 `cargo clippy -p xcontrol-app --features macos --all-targets -- -D warnings` 通过。建议 CI 的 macos feature 步骤补这一条。
- **已知行为，非回归**：边缘回切后 uinput 的虚拟位置停在受控屏面向主控的那条边上；之后用热键切入，`resync_cursor` 会把真实光标定位到这条边上，免疫窗（800ms）之后任何朝主控方向的小幅移动都会立即回切。macOS 工作机在同样场景下光标本就停在边上，两端行为一致。要改善需要在保持原位切入时把落在回切边上的落点内缩，属后续。
- **已知不对称，既有**：`UinputInjector::inject(MouseMoveAbs)` 只发 ABS、不同步虚拟位置。当前转发链不产该事件（mac 捕获只产 MoveRel）；接入 reis 捕获源的 ABS 转发之前必须先补同步。
- **150ms 防抖的覆盖面**：任意控制关系变化沿都算，包括对端释放本机（`PeerControlsLocal → None`）。变化沿记在变化后的下一轮循环顶部，实际窗口略长于 150ms，方向安全。
- **与设计的偏离**：多拆出纯函数 `apply_home_landing`（捕获源在测试里构造不出来，不拆无法单测落点策略）；`edge_return_requires_outward_delta_since_last_report` 的免疫窗预热挪到首条极值回报之前，否则分不清是"未武装"还是"免疫窗"挡的；`#[ignore]` 冒烟 `real_cursor_smoke_prints_edge_hits` 拍数 200 → 1000，保持 10s 观察窗。
- **措辞统一**："锚点不可得"一律说"不做落点定位，光标随物理鼠标"——代码是把 `None` 传给 `set_suppress_local_input`，落点由系统决定。
- **主控辅助功能授权（2026-11-24 部署实测）**：同一 `/Applications/XControl.app` 路径换上新构建后，主控日志出现 `主控捕获源启动失败 … CGEventTapCreate 返回空（参数或权限被系统拒绝）`；旧包当天下午在同一路径正常。结论：ad-hoc 签名的应用按签名匹配授权，重新打包即失效，与路径无关。`dmg.sh` 头注与 docs/10 §1.1"TCC 按路径记忆"的说法不准确，由 design D14 更正。
- **两端共用同一个 `instance_id`（既有缺陷，未修）**：`main.rs:57` 的 `DEMO_INSTANCE_UUID` 是写死的常量，并被用作真实运行时身份（`main.rs:693`、`:918`）和 PSK 派生盐（`:936`）。工作机日志里"布局条目越出本机屏数（1）… peer_id=1"由此而来：`ids_owned_by` 把主控屏也算成本机屏，占掉唯一的本地屏下标，真正属于工作机的条目反而越界不映射。11-22 起的日志都有这条告警。uinput 后端忽略屏下标，所以现在不受影响；macOS 工作机上进入落点的 warp 会因"未知 ScreenId"失败，多工作机时主控按 owner 找落点屏也会失准。

## 8. 追加：边缘回切仍卡顿的根因（用户 2026-11-24 实机反馈，R8 / C8）

用户在新构建上实测：功能项全部通过，但边缘回切到主控时快速滑动仍有卡顿。代码走读定位到三段行程在回切时丢失，慢速移动时合计只有几像素，快速甩动时是几十到上百点：

1. **顶边期间被截掉的位移**：工作机光标到达屏边后，主控继续转发过去的 `MouseMoveRel` 在注入时被屏幕边界截掉（uinput 的 `virt` 被钳在边界）。回报采样最多晚 10ms，这段时间的推进量没有任何回报渠道——回报坐标被钳在端点值 0.0 / 1.0。
2. **触发后到恢复本地输入之间被丢弃的位移**：回切在收帧分支触发后，关闭本地输入拦截要等下一个输入拍（≤10ms）。这期间捕获到的鼠标位移被本地拦截、又不转发（已不在受控），直接丢弃。
3. **在途的一批**：主控每 10ms 转发一批。最后一批若在工作机采样之后才到达并注入，就不会体现在触发回切的那条回报里。

方案取舍（design §10）：由工作机精确度量第 1 段并随回报带回（D10 / D11），主控补第 2、3 段并立即恢复本地输入（D13）。未采用"主控侧用转发增量外推工作机光标"：需要从连续回报里学习工作机的屏幕尺度，在途批会让学习值失真，误差不可控。

未覆盖：切入方向（主控 → 工作机）的同类丢失——主控光标顶在本机屏边时物理位移是否还能从捕获流观测到，属未验证假设 H2，本次不动。macOS / X11 工作机不度量行程，回报仍为端点值。

## 9. G6 / G7 实现后记（2026-11-25）

**与设计的出入**（实现子代理报告，均经主会话复核）

- `Notice` 不再派生 `Eq`（`overflow` 是浮点字段）；全仓没有依赖 `Notice: Eq` 的用法。
- core 的 `EDGE_RETURN_INSET` 改为 `pub`，app 夹取补偿落点时用同一个常量。
- app 侧纯函数在所有平台编译（非 macos 加 `allow(dead_code)`），让单测在不带 feature 的 CI 里也跑；接线仍在 `cfg(macos)` 内。
- 启动引导放在 `supervise_runtime` 的向导等待相之后、主循环之前，每进程一次；托盘切换角色和配置重建不重复弹框。
- `advance_axis` 对域外起点先按边界起算（`warp` 取 x=1.0 时 `virt = max + 1`，多出的 1 格不算行程）。
- 落点补偿遇到非有限行程或非正屏几何时，退回未补偿的原落点。
- `dmg.sh` 在图标文件缺失时直接失败，不出没有图标的包。
- docs/03 的落点补偿写成回切第 7 条，第 3 条指向它。

**实现与检查中发现、已交给质量检查处理的项**

- 授权瞬间的接管：未授权时仍可靠边缘传感器切入，授权后捕获源恢复会立刻把本地输入转发出去。改为恢复前先释放控制关系。
- `--headless` 不应请求系统引导框。
- 捕获源重建持续失败时的告警每秒一条，改为退避。
- 过时表述：`macos.rs` 模块文档"未授权时 tap 创建成功但静默忽略"、docs/03 §4.6 的"tap 是 ListenOnly"、向导 i18n 的"主控转发无需本权限"。

**已知未修的既有问题**

- 迟到的回报被当作绝对坐标注入：回切之后才到达的 `MouseMoveAbs` 回报，因控制关系已终结，会走既有的"绝对坐标注入"分支。`resolve_local` 对未映射的全局 id 原值直通，单屏主控上因"未知 ScreenId"注入失败而无害；多屏主控上会把光标 warp 到下标对应的那块屏。回报提速到 10ms 后撞上的概率更高，但局域网时延下仍属小概率。根治方向：未受控时丢弃不属于本机的 `MouseMoveAbs`，或回切后短窗内忽略回报。

**残留的精度缺口**

- 在途批窗口取 3ms，按局域网时延估计；Wi-Fi 下会少补。
- 工作机 `virt` 与合成器真实光标若因取整相差 1–2px，顶边行程会少算这个量级。

**验证数字**（实现子代理在最终代码上报告，质量检查会复跑）：core 150、app 141、workspace 682（带三个 `--skip`），wayland 交叉 check 与 clippy 通过。wayland 的三个新单测在本机只能交叉编译，实现方把纯函数与用例抽到仓库外用宿主工具链跑过。
