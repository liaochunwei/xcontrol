# Journal 1 — csdark

<!-- 按 session 追加；格式：日期 · 任务 · 结果 · 经验 -->

## 2026-11-24 / 11-25 · 任务 11-24-edge-switch-latency-hotkey-return（热键切入后边缘回切失效与快速过边停滞）

**结果**：分支 `fix/edge-switch-latency-hotkey-return`，5 个工作提交（core / input / app / 授权引导 / 图标+docs），用户实测确认全部问题解决。两端部署：主控 `/Applications/XControl.app`（19:18 构建），工作机 rsync 20 文件 + 远端 wayland 构建。

**修了什么**（7 个成因 + 2 轮手感迭代，全部有单测）：
- 热键切入后回切边取反（core `SwitchAck` 分支）→ 进入边；免疫窗按条数 → 按时间 800ms
- uinput `release_all` 复位虚拟位置 → 保留；保持原位切入后位置模型校准（`InputSink::resync_cursor` 新钩子，三处透传）
- 两处 50ms 轮询 → 10ms；受控期主控边缘命中一律忽略 + 控制关系变化后 150ms 防抖（两者必须成组改——节拍与锚定回拉近同相位）
- 热键回主控落点屏 0 → 切入锚点（`pending_release_warp` 重关联后重放）
- 角落排除约 1 点见方 → 5% 角落区（`layout::in_corner_band` 单一出处）
- 回切行程补偿（三段丢失位移）+ 2ms 微步匀速高频回放（专用线程；两轮迭代：一次性补偿=瞬移、10ms×6 大步仍不顺滑，都被用户实测推翻）
- 未授权辅助功能：启动弹系统引导 + 每秒探测授权自动恢复（`--headless` 只告警）；`.app` 应用图标补齐

**经验教训**（已回写 `.trellis/spec`，要点摘录）：
1. **对称语义改判必须全路径排查**：11-22 回切边语义改判只改了边缘路径，热键路径漏改——新增接管路径时必须带状态机用例（范本 `switch_takeover_return_edge_faces_host`）。
2. **周期性动作是耦合参数**：改轮询节拍前先找同频的其他周期动作（锚定回拉、防抖窗），一起评估。
3. **手感问题两轮实测才收敛**：想象中的"平滑"（6 大步 ease-out）被实测推翻，专用线程 2ms 微步匀速才对。光标手感参数集中在 `RETURN_REPLAY_*`。
4. **ad-hoc 签名的授权按签名匹配**：重新打包即失效，docs/10"按路径记忆"的说法已更正。
5. **未验证假设要显式记录**：`research/root-cause-analysis.md` §3 的 H1–H3 全程未被触碰；`spawn_cursor_replay` 的 `CaptureSource` 克隆设想因 Receiver 不可克隆而放弃，改为进程级 warp 自由函数。

**遗留**（建议后续任务）：
- 两端共用写死的 `DEMO_INSTANCE_UUID`（工作机布局告警的根因；macOS 工作机会因"未知 ScreenId"落点失败）
- CI 补 `xcontrol-app` macos feature 编译面（本次 G2–G4 接线 CI 不编译）
- 迟到的 `MouseMoveAbs` 回报在控制关系终结后被当绝对坐标注入（多屏主控有风险）
- 稳定签名身份（Developer ID）以止住每次重打包的重新授权
- 三个会注入真实输入的 macOS 默认测试未跑（用户空闲时补跑）
