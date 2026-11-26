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

## 2026-11-26 · 任务 11-26-file-ref-announce（FR-7.3 切片 a：文件引用元数据面）

**结果**：分支 `feat/file-transfer-spec`（未合 main，按用户指示验证后再合）。proto + core 两批提交全绿：文件域九消息（kind 0x58–0x60、fixtures 26→35、PROTO_MINOR 1→2、Caps bit4）+ `file_ref` 模块（清单构建/pending 槽/代次失效/回环指纹）+ mock 文件引用槽 + 三层测试（proto 内嵌 11 组 / file_flow 集成 3 条 / 既有套件回归）。质量门：clippy -D warnings 零警告、fmt 干净、workspace 32 套件全绿、check-deps PASS。

**关键设计落地**：
- 回环指纹 = (name, size) 有序序列 blake3 截断，len 前缀帧式布局防拼接碰撞；不含 mtime/绝对路径（docs/04 §8.3）——mtime/kind/children 不参与有单测钉死
- pending 单槽代次失效：同源同 seq 重放 Duplicate、低 seq 过期拒绝、跨来源覆盖（最后复制者赢）、invalidate 由调用方显式触发（模块不嗅探剪贴板）
- origin 去重（防线 2）在本模块首次真实落地（剪贴板域的同名机制一直是占位）

**经验教训**（已回写规则卡/CHANGELOG）：
1. **物理上限先行自查**：初稿"条目 ≤4096、清单 ≤1MiB"与帧层 64KiB 物理上限矛盾（65472B 教训同源）——实装前自查修正为 512 条/256B/单帧承载，写进 docs/02 修订注记。
2. **特性门控不得用实现级版本常量**：session.rs Layout 门控错用 PROTO_MINOR，minor 提到 1.2 会误停 1.1 对端布局下发——新增 LAYOUT_MIN_MINOR 常量分离两个概念；net 层测试同步改锚 PROTO_MINOR 消除 minor 硬编码涟漪。
3. **kind 占用会污染"未知 kind"探针**：0x60 被文件域占用后，stream/scenarios/session 三处用它当未知探针的测试全部失真——探针统一改 0x61（文件域后第一空档）。
4. 大段插入文档用标题作锚点会吞标题（§4.6 插入吞了 §5 标题，立即发现修复）——插入后必须回读前后结构。

**遗留/后续**：切片 b（文件 TCP 通道 + FileBind 绑定 + 拉流 + 真实平台检测 + app 泵接线 + 托盘提示）承接；会话 minor ≥ 2 发送门控随发送路径落地。

## 2026-11-26 · 任务 11-26-file-channel-pull（FR-7.3 切片 b：通道拉流三阶段推进）

**结果**：分支 `feat/file-transfer-spec` 三批提交（3fa018c b-core / 2679dd3 b-net / 8988359 b-app+b-平台）。文件拉流全链路打通：本机检测 → 元数据通告 → pending 槽 → 拉取 → 真实分块发送 → 暂存落盘 → blake3 校验 → **挂板系统剪贴板**（mac 真机实测）。质量门：clippy 双 feature 配置零警告、fmt 干净、workspace 无 feature + macos feature 测试全绿、check-deps PASS、mac 剪贴板自往返冒烟 PASS。

**关键设计落地**：
1. **FileTx 哈希下沉**：源端 blake3 边流边算放 core FileTx（on_chunk_sent 喂入）——app 层零 blake3 依赖（分层红线；最初设计让 app 持 Hasher，实装时发现会把 blake3 拉进 app 依赖树，下沉即解）。
2. **FileHub 进程级单例**（OnceLock，同 clip shared_echo 先例）：pending/清单/挂板防线跨重连存活；挂板回环防线 = 指纹 + 2s 窗口（docs/04 §8.3 同板多域语义的 app 层等价）。
3. **NSURL 绑定路径全部源码核实**（objc2-app-kit 0.3.2 nspasteboard.rs）：writeObjects 安全绑定 + NSURL 的 NSPasteboardWriting/Reading unsafe impl 声明——写路径零 unsafe，读路径一处 unsafe 绑定带 SAFETY。pasteboardTypes 未进绑定 → 检测面改按类过滤一步到位。

**经验教训**：
1. **绑定可用性先查本地 registry 源码再写代码**（~/.cargo/registry/src/...）：本次 5 个关键 selector（writeObjects/readObjectsForClasses/fileURLWithPath_isDirectory/path/NSPasteboardTypeFileURL）全部先 grep 核实签名再落笔，一次编译通过率高很多。
2. **python 补丁脚本锚点必须以当前文件实况为准**：三次 AssertionError 全是"锚点写了记忆中的版本而非 fmt 后实况"——先 grep 再 assert。
3. **大文件补丁用脚本文件而非 heredoc**：嵌套三引号在 heredoc 里炸过一次。
4. **集成测试要先消化协议的正常面**：host 泵会推自身 outbound announce——测试收帧循环不能假设第一帧就是目标帧。
5. **挂板/检测面按平台 cfg 门控 + 断言分叉**：无 feature workspace 测试（CI Linux 面）与 macos feature 测试都全绿的关键。

**遗留/后续**（切片 b 收尾 + c）：
- 托盘「传输文件到本机」动作（pull 触发面 UI；menu 顺序 golden + docs/06 §5.2 先行）+ i18n 文案
- 剩余平台（win CF_HDROP / x11 uri-list / wayland mime）检测与挂板——docs/05 §0.2 pending 实机复核
- 切片 c 透明粘贴（注入路径拦截物化）
- PendingSlot 过期 Clock（切片 d）；断点续传 offset 评估（切片 d）

## 2026-11-26 · 任务 11-26-file-channel-pull 收口（切片 b 完成 + 托盘触发面）

**结果**：切片 b 全部三阶段（b-core/b-net/b-app+b-平台）+ 托盘触发面完成归档。mac 上文件跨设备复制管线真机可走通：复制 → 对端通告 → 托盘「接收文件：N 项 / X MB」→ 点击 → 暂存落盘 + blake3 校验 + **挂板系统剪贴板** → Cmd+V。双机实测（mac↔win/linux）移交人工——win/linux 平台面按 docs/05 §0.2 pending。

**本轮要点**（托盘面）：
- docs/06 §5.2 菜单先行（红线）；menu 顺序 golden 扩展（有货 12 项/无货 11 项，既有 golden 不变）
- 投影直读 hub（tray_rt 200ms 刷新即反映）——避免了 AppState 桥接任务的第三处 spawn 点
- hub 拉取队列 + 泵「接收端空闲」门控：多文件顺序拉流、全条目完成统一挂板（对齐 Finder 多选语义）
- i18n trf 占位符键 zh/en 同表（parity 测试同约束）

**流程教训**（本轮反复踩的坑）：
1. **结构体加字段后所有构造点都要补**：TraySnapshot/TrayState 加 incoming_files，散布在 project/menu/tray 三处测试构造器 + loopback match 臂——clippy -D warnings 逐个揪出；先 grep 构造点清单再动手更省轮次。
2. **python 补丁断言失败的两种根因**：锚点写了记忆版而非 fmt 后实况；插入位置吞掉相邻结构体的 derive 行——插入后必须回读前后 20 行。
3. **tick 内返回类型陷阱**：select 臂返回 ()，深层违规不能 `return true`——用局部 violated 标志 + tick 收尾统一 break。

**功能剩余面**（后续任务）：切片 c 透明粘贴（注入拦截物化）；win/linux 平台面（实机复核）；切片 d 目录/进度/断点。

## 2026-11-26 · 任务 11-26-paste-materialize（FR-7.3 切片 c：透明文件粘贴）

**结果**：提交 3245e7c，切片 c 完成。「像本地一样」达成：受控机上直接 Cmd+V/Ctrl+V → 注入管线最内层 sink 包装器拦截粘贴组合（pending 槽有货时）→ 挂起组合键 + 触发拉流（复用切片 b 管线）→ 挂板成功后自动回注挂起键 → 目标应用粘到物化后的本地文件。零系统级 hook；文本粘贴路径零改动。

**关键设计落地**：
1. **拦截点 = 注入管线最内层 sink 包装器**（HotkeyFilterSink → MappingSink → PasteInterceptSink → 平台 sink）——KVM 注入路径天然钩子；修饰键自跟踪复用 core ModifierTracker（wire Key 无 mods，手写 match 是重复造轮子——mods.rs 早有 apply_key）。
2. **单飞 + 一次性消费**：共享 AtomicBool（物化中不重复触发）；挂板后 consume_pending（托盘条目消失、随后粘贴行为与无文件槽一致——回归保护内建）。
3. **async/sync 边界**：sink.inject 是同步热路径，物化是异步长任务——桥接面 = 两条 unbounded 通道（job → worker；outcome → 回注），worker 只轮询 hub 挂板指纹，回注在会话循环线程执行（sink 所有权约束）。

**流程教训**：
1. paste_intercept 首版又混入草稿垃圾（占位 SystemClock/thread_local/假事件）——写完立即自检"这段是设计还是草稿"，一次重写比事后修干净。
2. 结构体加字段（TrayState.incoming_files）的构造点散布三个文件——下次加字段先 grep 全部构造点清单。
3. 原子序 store 不能 AcqRel（编译器直接拦）——Release/Acquire 配对够用。

**遗留**：AC5 双机实测移交人工（A 复制 → 控制着 B 直接 Cmd+V）；切片 d（目录 FileExpand 接线/进度 UI/断点评估/配置面）；win/linux 平台面（docs/05 §0.2 pending）。

## 2026-11-27 · 任务 11-26-file-transfer-polish（FR-7.3 切片 d：收尾打磨）

**结果**：提交 dbd8416。部分成功策略（PullSession 记账，条目失败不废弃整单——已成功条目照常挂板，全失败保留 pending 供重试）；托盘「取消文件传输」（FileCancel 双端收尾：源端服务流停止 + 本地已成功条目挂板）；config `[file_transfer]`（enabled/max_bytes/confirm_threshold——发布门控、Quota 退回、二次点击确认）；断点续传评估入档（v1 选重传，offset 面保留）。

**经验**：
1. 结构体加字段的构造点清单纪律再次生效（TraySnapshot/TrayState/Config 三处测试构造器 + loopback 臂——grep 先行省一半轮次）。
2. 共享状态重构（record_progress/finalize_session）把散在 record_rx_action 两臂的记账收敛进 hub 单一出处——部分成功语义只改一处。
3. python 补丁断言失败=整个脚本不落盘（原子性反而是保护）——失败后先核对文件实况再重跑，不盲改锚点。

**功能总账（FR-7.3）**：单文件跨设备复制粘贴全链路完成（mac 实测）；剩余 = 目录（FileExpand 接线）、字节级进度、设置页可视化、win/linux 平台面、双机实测。
