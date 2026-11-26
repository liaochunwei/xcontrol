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

## 2026-11-26 · 任务 11-26-file-transfer-spec（FR-7.3 规范先行：跨设备文件复制粘贴）

**结果**：分支 `feat/file-transfer-spec`，5 个提交（ADR / 需求入册 / 协议 / 剪贴板+平台+安全 / trellis）。FR-7.3 立项——复制端仅同步文件元数据清单，接收端在用户粘贴时才经文件通道拉流物化。父任务 11-26-file-transfer + 4 个实装切片子任务已建（planning，待启动）。

**写了什么**：
- ADR-0004：独立第三条 TCP（文件通道，与剪贴板通道同构隔离）+ 统一 stage-then-mount 物化；否决"复用剪贴板 TCP"与"控制连接复用"（11-21 队头阻塞实测教训）；OS 原生惰性粘贴（file promise / FILEDESCRIPTOR）列 P2 增强
- docs/00：FR-7.3 入 §4.7；§6.3 非目标改有界表述；FR-4.6 备注为 FR-7.3 的降级形态
- docs/02 §4.6：XCP minor 1.1→1.2，kind 0x58–0x60 九个（FileExpand 随 1.2 一并钉死 wire，避免目录切片"只增"二次协商）；防护规则——条目索引寻址红线（任何帧不带路径）、分块空闲超时（固定 10s 不可用）、FileError 条目级可重试、2GiB 上限独立于剪贴板 16MiB
- docs/04 §8：pending 槽、代次失效（最后复制者赢）、指纹规范形 = (name,size) 序列 blake3（不含 mtime/绝对路径——物化必变两者）、§8.5 与 §3 推送决策辨析（拉模式否决针对剪贴板字节，不冲突但必须明说）
- docs/05：Caps bit4 `CAP_FILETRANSFER`、§0.2 四平台读写矩阵（全标注未实装）、§8 三条实机复核项（mac 文件贴文本应用行为 / NSFilePromiseReceiver 粘贴路径 / CF_HDROP 长路径 Unicode）
- docs/07 §9：索引寻址最高红线、磁盘耗尽预检+阈值确认、"粘贴即授权"同意模型（元数据到达 ≠ 内容出站）
- 规则卡 rust-core/protocol-rules.md：kind 分配约定补文件域 + 文件域红线条目

**经验教训**：
1. 大段插入文档用章节标题做锚点会把标题吞掉（§4.6 插入时吞了 §5 标题）——插入后必须回读前后结构确认。
2. 新设计触碰既有否决决策时，写专门的"辨析"小节（docs/04 §8.5），否则规范自相矛盾会被后续实现当成借口。
3. 规范先行让实装切片的 prd 可以直接引用条文章节号，切片任务 Planning 成本大幅下降（a–d 的 prd 已带引用）。

**遗留/后续**：切片 a–d 待启动（子任务已挂上下文种子）；分支未合 main（待评审/PR）；golden fixtures 与 fuzz 随切片 a 实装补。
