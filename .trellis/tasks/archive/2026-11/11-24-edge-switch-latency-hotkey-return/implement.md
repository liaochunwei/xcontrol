# implement: 热键切入后边缘回切失效与快速过边停滞

设计见 [design.md](./design.md)（D1–D9），验收见 [prd.md](./prd.md)（AC1–AC9）。五个改动组的回滚约束见 design §9：**G3 的门控与节拍必须同进同退**。

## 前置

- [ ] 工作树干净；基线 `cargo test -p xcontrol-core --lib` 全绿（任务规划时 139 通过）。

## 1. 规范先行（docs/03）

- [ ] 按 design §6 更新 docs/03：§4.6 切换路由语义的 Home 补落点；§5 热键闭环补回切边与位置校准一句；"角落排除"条目改写为 5% 角落区；回切第 1 条采样周期改 10ms、第 2 条角落判定随之改写、第 4 条免疫窗注明按时间；"死区"条目改写为实现事实；追加 2026-11-24 修订记录。

## 2. G1 core：回切边与免疫窗（D1 + D2 → AC1、AC6）

- [ ] 先写失败用例 `switch_takeover_return_edge_faces_host`：`[主控 | 工作机]` 布局经 `Switch` + `SwitchAck` 接管，工作机左缘极值回报加同向移动应回切、落主控右缘内侧；工作机右缘极值回报不触发。运行确认**修复前失败**。
- [ ] `session.rs:1138` 改 `outer_edge: entry_edge`，同步 `SwitchAck` 分支与 `ControllingContext` 的注释。
- [ ] 免疫窗改按时间：`taken_over_ms` 字段、`RETURN_IMMUNITY_MS = 800`、`execute_input` / `execute_local` / `edge_return_actions` 下传 `now`；删除 `reports_since_takeover` 与 `RETURN_IMMUNITY_REPORTS`。
- [ ] 测试助手 `pass_immunity(&mut session)`（推进 `TestClock`），替换全部旧常量引用；新增边界用例 799ms 不触发 / 800ms 触发。逐个检查回切用例里是否在推进时钟后又步进 `SessionEvent::Timer`（心跳时序会被 800ms 推进影响）。
- [ ] 验证：`cargo test -p xcontrol-core --lib`

## 3. G2 位置模型校准（D3 + D4 + D5 → AC2、AC3）

- [ ] `InputSink::resync_cursor` 默认方法；`Box<dyn InputSink + Send>` 与 `MappingSink` 透传；`RecordingInputSink` 覆写为计数并暴露 `resync_count()`。补透传单测。
- [ ] `wayland.rs`：`release_all` 删除虚拟位置复位；实现 `resync_cursor`（序列同 `warp`）；更新两处注释。
- [ ] `hotkey_rt.rs`：`HotkeyFilterSink::resync_cursor` 透传 + 单测。
- [ ] `session_rt.rs`：`is_preserved_entry` 纯函数 + 单测；收帧分支 `apply_and_send` 成功后调用校准，失败 `warn!`（target `session::rt`）。
- [ ] 验证：core 单测；`cargo check -p xcontrol-input --features xcontrol-input/wayland --target x86_64-unknown-linux-gnu`（依赖无法交叉构建时记录原因，改由 CI linux job 覆盖）；`cargo test -p xcontrol-app --features macos --lib`。

## 4. G3 门控与节拍（D6 + D7 → AC4、AC7）

- [ ] `edge_hit_allowed` 改为仅 `ControlDir::None` 放行；新增 `EDGE_SWITCH_DEAD_TIME` 与 `edge_dead_time_elapsed`；循环顶部记录控制关系变化沿；边缘分支判定顺序改为"本机屏过滤 → 门控 → 防抖 → EdgeCrossing"。
- [ ] `edge.rs:59` `DEFAULT_POLL_INTERVAL` 改 10ms；更新 `edge.rs`、`session_rt.rs` 里所有写着 50ms 的注释与模块文档（`grep -n "50ms" crates/xcontrol-app/src/edge.rs crates/xcontrol-app/src/session_rt.rs` 清零或改对）。
- [ ] 测试：更新 `edge_gate_truth_table` 与 `edge_gate_follows_control_dir_lifecycle`；新增防抖用例（窗内拒绝、窗外放行）与节拍常量断言。
- [ ] 验证：`cargo test -p xcontrol-app --features macos --lib`

**不允许只落地本组的一半。**

## 5. G4 热键回主控落锚点（D8 → AC5、AC7）

- [ ] `macos.rs`：`CaptureSource::anchor_point()` 只读访问器（读 `pin_point`，锁中毒处理同文件既有写法）。
- [ ] `session_rt.rs`：`strip_home_warp` 纯函数 + 单测；热键分支 `ReleaseControl` 接线（有捕获源即剥离；`last_suppress` 为真时设 `pending_release_warp`）；删除热键分支里因此失效的 WarpPointer 缓存块（约 1933–1944 行）。
- [ ] `session.rs:1334-1336` 的落点注释补一句"macOS 主控在 app 层以切入锚点替换"。
- [ ] 验证：`cargo clippy -p xcontrol-input --features xcontrol-input/macos --all-targets -- -D warnings`；`cargo test -p xcontrol-input --features xcontrol-input/macos`；`cargo test -p xcontrol-app --features macos --lib`。

## 6. G5 角落区 5%（D9 → AC9）

- [ ] `layout.rs`：新增 `CORNER_RATIO` 与 `in_corner_band`；`edge_crossing` 改用角落带判定。
- [ ] `session.rs`：`at_orthogonal_extreme` 改为 `in_corner_band(正交轴回报)`，函数改名并更新注释。
- [ ] 改写三处钉死旧行为的测试：`layout.rs` 的 `edge_crossing_corner_exclusion`（删掉"y=0.001 未收敛 → 触发"）、`session.rs` 的 `edge_return_corner_exclusion`、`edge.rs` 的 `corner_excluded_and_interior_ratio_preserved`（删掉"998.5 → 触发"）。传感器的判定代码不动，它调用的是 core `edge_crossing`。
- [ ] 新增边界用例，取 `CORNER_RATIO ± 0.001`：切入侧四条边各验带内不触发、带外触发；回切侧垂直与水平外侧边各验一次。
- [ ] 排查同文件其余用例里落在角落带内的沿边坐标（`0.0x`、`0.9x` 一类的 ratio 或回报坐标），逐一确认预期。
- [ ] 验证：`cargo test -p xcontrol-core --lib`；`cargo test -p xcontrol-app --features macos --lib`。

## 6b. G6 回切行程补偿（D10–D13 → AC10；用户 2026-11-24 实机反馈后追加）

- [ ] 先改 docs/02 §4.2：反向回报取值范围扩展到端点之外（design D11）。wire 布局不变，不动 fixtures。
- [ ] `wayland.rs`：`overflow_x / overflow_y`、纯函数 `advance_axis` + 单测；`inject` / `warp` / `release_all` / `cursor_normalized` 接线。`sink.rs` 的 `cursor_normalized` 契约文档同步。
- [ ] core `session.rs`：纯函数 `beyond_edge` + 单测；`Notice::EdgeReturnedToHost` 增加 `overflow`，同步 `loopback.rs` 与全部既有用例；新增"越界回报解出行程并随通知带出"的用例。
- [ ] app `session_rt.rs` / `edge.rs`：`compensated_release_point`、`in_flight_batch`、`sum_mouse_deltas` 纯函数 + 单测；收帧分支落点补偿接线；输入拍记录在途批；把输入拍里的翻拍代码抽成 `reconcile_local_input_suppress`，回切触发处排空后立即翻拍。
- [ ] D14 文档更正：`scripts/macos/dmg.sh` 头注、docs/10 §1.1、docs/05 §1。
- [ ] docs/03 §5 回切第 3 条、docs/05 §4.5、CHANGELOG。
- [ ] 验证：core 与 app 单测；wayland 交叉 check 与 `--all-targets` clippy；workspace 与 macos feature clippy；fmt。

回滚点：`git checkout -- crates/xcontrol-input/src/platform/wayland.rs docs/02-protocol.md scripts/macos/dmg.sh docs/10-install.md`；`session.rs`、`session_rt.rs`、`edge.rs`、`sink.rs`、`loopback.rs` 按代码块还原。

## 6c. G7 未授权引导与应用图标（D15 + D16 → AC11、AC12；用户 2026-11-24 提出）

- [ ] `main.rs`：角色确定后的启动引导（未授权 → `check_accessibility(true)` + `warn!`），仅 macOS 编译面。
- [ ] `session_rt.rs`：输入拍内的捕获源自动恢复（`CAPTURE_RETRY_INTERVAL` 1s，macOS、Host、`capture.is_none()`）；`hotkey_rt.rs` 的失败告警改写。
- [ ] `dmg.sh`：拷入 `scripts/macos/AppIcon.icns`，`Info.plist` 加 `CFBundleIconFile`。图标生成脚本与 `.icns` 已由主会话加入仓库（`scripts/macos/icon.swift`、`icon.sh`、`AppIcon.icns`）。
- [ ] docs/05 §1（未授权引导与自动恢复）、docs/06 §5.1（应用图标）、docs/10 §1.1（图标来源与重新生成）、CHANGELOG。
- [ ] 验证：app 单测、macos feature clippy（app + input）、fmt；`scripts/macos/dmg.sh` 产物含 `Contents/Resources/AppIcon.icns` 且 `Info.plist` 有 `CFBundleIconFile`。

## 7. 文档与规则卡收尾（R6）

- [ ] docs/05 §1.2（锚定周期 10ms、受控期门控、锚点是热键回主控落点）与 §4.5（uinput 位置模型、转录 11-22 REL 实测记录）。
- [ ] CHANGELOG 新增 `## [Unreleased]` 的 Fixed / Changed。
- [ ] `.trellis/spec`：`rust-core/architecture-rules.md` 加"切换/回切语义（11-24 定稿）"，含"角落判定只经 `layout::in_corner_band` 一处"；`rust-platform/platform-code.md` 修正 ABS 虚拟光标条目并加 uinput 位置模型纪律与 macOS 受控期门控；核对后更正 `rust-core/logging-targets.md` 的 `input::` 审计结论。

## 8. 全量验证（CI macOS job 同款）

```bash
cargo run -p xtask -- check-deps
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p xcontrol-input -p xcontrol-clipboard --features xcontrol-input/macos,xcontrol-clipboard/macos --all-targets -- -D warnings
cargo test -p xcontrol-input -p xcontrol-clipboard --features xcontrol-input/macos,xcontrol-clipboard/macos
cargo test --workspace
cargo +nightly fmt --all -- --check
# 回归冒烟（CI 不跑，本机补）
cargo run -p xcontrol-app -- --loopback --auto-quit-ms 3000
```

- [ ] 逐条对照 prd 的 AC1、AC2、AC4、AC5、AC6、AC8、AC9 勾选；`git diff --stat` 确认只动了计划内文件。
- [ ] 复核未触碰研究笔记里的未验证假设 H1–H3（mac 注入器、捕获 tap 重启、受控期 delta 语义）。

## 9. 实机验收（用户执行，两端部署本次构建 → AC3、AC7、AC9）

- [ ] 边缘切入后在工作机上朝主控方向推回：回切（基线回归）。
- [ ] 热键切入：工作机光标落到上次受控结束的位置（进程启动后首次为屏幕中心）；朝主控方向推边：回切。
- [ ] 两个方向各快速甩动过边 10 次：无明显撞边停顿。
- [ ] 切入后立即朝工作机方向继续快速移动：工作机光标不回跳、按键不丢。
- [ ] 慢速连续移动切入：不回弹。
- [ ] 热键回主控：主控光标停在受控期间所见位置，不再跳到屏幕中央；边缘切入后用热键回主控同样如此。
- [ ] 把光标斜推进主控与工作机的四个角：不发生切换，系统热角可用；沿边中间一段贴边照常切换。
- [ ] 抽查主控日志：受控期间没有重复的"光标触边：上报边缘切换"。

## 回滚

按组还原，约束见 design §9。可整文件 checkout 的：`layout.rs`、`sink.rs`、`mapping.rs`、`mock/sink.rs`、`wayland.rs`、`macos.rs`、`hotkey_rt.rs`、`edge.rs`。**`session.rs` 与 `session_rt.rs` 被多组共用，按组回滚时按代码块还原，不要整文件 checkout。**

## 风险文件

- **`session_rt.rs` 的 `drive_session`**：G2–G4 都在改这个文件。不得改变 select 分支内任何 await 的形态（取消安全纪律，architecture-rules.md 末节）；变化沿记录放在循环顶部。
- **`session.rs`**：G1、G4（注释）、G5 都在改。不触碰 `transition_table`；`now` 下传靠编译器兜底，但语义上要确认免疫窗用的是回报到达时刻。
- **`wayland.rs`**：本机无法运行，只有交叉 check；改动严格限定在 `release_all` 与 `resync_cursor` 两处。
- **`macos.rs`**：只加一个只读访问器，不碰捕获与锚定逻辑。
- **角落带内的既有用例**：规划时已排查，受影响的只有 G5 列出的三处断言；`xcontrol-net` 的 `tests/chain.rs` 与 `tests/scenarios.rs` 直接调用 `LocalCmd::EdgeCrossing`，不经角落判定。全量单测仍需跑一遍兜底。
