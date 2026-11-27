# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 惯例与
[语义化版本](https://semver.org/lang/zh-CN/)；`0.x` 期间协议 minor 版本可能不兼容，
兼容性以 XCP `XCP-VERSION` 协商为准（docs/02）。

## [Unreleased]

### Added

- **FR-7.3 目录复制（FileExpand 接线，切片 d R1）**：复制目录 = 整树落地——
  源端 PullReq 命中目录条目时有界枚举（深度 ≤4、单批 ≤512，非 UTF-8 名跳过）
  回 FileExpand；接收端下标空间增长、树形暂存（staging/{tid}/{相对路径}）、
  文件子条目自动续拉（子目录经再展开递归）；事件驱动收敛判定替代计数完成
  （队列/接收/服务/等待全空 → 收尾挂板，目录壳随顶层目录带出）。集成测试：
  目录树（含子目录）真 TCP 端到端逐字节断言 PASS。
- **FR-7.3 切片 d：会话收尾打磨**：部分成功策略（条目失败不废弃整单——已成功
  条目照常挂板 + 失败清单提示，全失败保留 pending 供重试）；托盘「取消文件
  传输」（拉取进行中出现，对端 FileCancel + 本地收尾）；配置 `[file_transfer]`
  （enabled / max_bytes=2GiB / confirm_threshold_bytes=512MiB——发送端超限不
  发布、接收端超限 Quota 退回、超阈值需托盘二次点击）；断点续传评估结论 = v1
  选重传（offset 协议面保留，docs/02 §4.6 注记）。
- **FR-7.3 切片 c：透明文件粘贴**：受控机上直接 Cmd+V/Ctrl+V 即物化——
  注入管线的 sink 包装器拦截粘贴组合（修饰键自跟踪，wire Key 无 mods），
  pending 槽有货时挂起组合键并触发拉流（复用切片 b 管线），挂板完成后自动
  回注挂起键——目标应用粘到的就是物化后的本地文件，"像本地一样"。单飞
  语义（物化中不重复触发）、一次性消费（挂板后 pending 清空，随后粘贴行为
  与无文件槽完全一致）、失败/超时（120s）丢弃挂起键并提示、零系统级 hook、
  文本粘贴路径零改动（pending 无货时逐字节透传，NFR-1 不回退）。
- **FR-7.3 切片 b/c 前置：文件拉流与托盘触发面（mac 实测，win/linux 平台面
  随实装补齐）**：独立文件 TCP 通道（FileBind 三通道分派、域隔离防御、协商
  门控 minor≥2、3s 独立退避）；真实拉流全链路——本机检测（changeCount 触发 →
  清单发布，挂板回环防线 2s 窗口）→ 元数据通告 → pending 槽 → **托盘
  「接收文件：N 项 / X MB」**（docs/06 §5.2 菜单新条，无货不渲染）→ 条目级
  顺序拉流（信用窗口 + blake3 边流边算）→ 暂存落盘 → 校验 → 挂板系统剪贴板
  （mac writeObjects(NSURL[]) 实测；其余平台文件已暂存并提示）。集成测试：
  真 TCP + Noise + 真实磁盘的端到端（暂存内容逐字节断言 + 挂板指纹登记）。
- **FR-7.3 切片 a：文件引用元数据面（协议与状态机层）**：XCP 1.2 文件域九消息实装
  （`xcontrol-proto::file`，kind 0x58–0x60，golden fixtures 26→35，解码层强制清单
  ≤512 条/名称 ≤256B 纯名禁 `/`·`.`·`..`·NUL/总字节 ≤协商上限/chunk ≤64KiB，
  清单单帧承载编码期校验）；`PROTO_MINOR` 1→2、`CAP_FILETRANSFER`（bit4）；
  core 新增 `file_ref` 模块——发送侧清单构建（空清单不广播、超限 fail-fast、
  total_bytes 聚合）+ 接收侧 pending 单槽（代次失效"最后复制者赢"、`(name,size)`
  blake3 回环指纹不含 mtime/路径、`(origin_id, seq)` 去重首实装）+ mock 剪贴板
  文件引用槽（`mock-platform`）。文件 TCP 通道与 app 接线随切片 b。
- **FR-7.3 跨设备文件复制粘贴立项（规范先行，代码未动）**：复制端仅同步文件元数据
  清单，接收端在用户粘贴时才经文件通道拉流物化——跨设备复制对齐本地体验。
  [ADR-0004](docs/adr/0004-file-transfer-channel-and-materialization.md) 定稿通道拓扑
  （独立第三条 TCP，与剪贴板通道同构隔离）与物化策略（统一 stage-then-mount，OS 原生
  惰性粘贴列为 P2 增强）；XCP 协议 minor 1.1 → 1.2 预留文件通道消息域
  （kind 0x58–0x60，[docs/02 §4.6](docs/02-protocol.md)）；剪贴板侧 pending 槽/代次
  失效/回环指纹规范形入 [docs/04 §8](docs/04-clipboard.md)；平台读写路径与实机复核项
  入 [docs/05 §0.2](docs/05-platform.md)；文件传输安全面（条目索引寻址红线、磁盘耗尽
  防护、粘贴即授权）入 [docs/07 §9](docs/07-security.md)。后续实装按切片推进
  （元数据面 → 通道拉流 → 透明粘贴 → 打磨）。

### Fixed

- **双机实测九连修复（FR-7.3 跨设备文件复制，mac↔Linux KDE Wayland 真实环境）**：
  ① 入槽 Quota 检查 + 收敛判定的同线程重入死锁；② 文件预览图误判（Finder 文件
  复制带 icns/tiff 预览被 clip 泵当图片同步）；③ 源端服务流从不开账（Chunk 零
  发出）；④ 文件通告指纹节流 hub 级共享（对端重启后 announce 发进死通道）；
  ⑤ detector 独立 wayland 连接互锁（file 泵永不启动）；⑥ 接收端 rel_paths 未
  初始化（挂板名 entry-N）；⑦ menu_equivalent 漏字段（托盘永不显示接收条目）；
  ⑧ TransferFiles action 漏声明/漏处理器（点击无反应）；⑨ 全进程双剪贴板实例
  （外部写入事件投递不可达 detector）。终态：**双向复制 + 挂板 + 托盘直驱真实
  环境验证通过（131MB 大文件 md5 一致，44 秒完成）**。
- **双机实测根因修复：文件通告入槽的同线程重入死锁**（FR-7.3，双机冒烟暴露）：
  `std::sync::Mutex` 不可重入——入槽 Quota 检查（嵌套两层 `lock_hub()`）与会话
  收敛判定（`&&` 链上两次取锁）在对端清单到达时同线程二次加锁永久卡死，拖死
  双端心跳（9s 互断、进程存活但会话沉默）。合并为单次持锁；全文件扫描无残留。
  实测：Finder 复制 → 通告 → 对端入槽后 35s+ 会话持续存活（修复前必死）。
- **macOS NSPasteboard 进程级访问互斥**（防御加固，同冒烟暴露）：剪贴板读取
  路径分布在多个后台线程（clip 泵 / 文件检测器 / 监听线程），AppKit 板非线程
  安全——全部访问点持进程级锁串行化。

- **`Layout` 下发门控改用特性级常量 `LAYOUT_MIN_MINOR`（1.1）**：原先错用
  `PROTO_MINOR` 作门控——文件域实装把 PROTO_MINOR 提到 1.2 后会误停 1.1 对端的
  布局下发（`ScreenInfo.owner` 属 1.1 形态，docs/02 §4.3 版本门控）。
  特性门控与实现级版本常量自此分离（任务 11-26-file-ref-announce 实装自查）。

- **热键切入后边缘回切失效**（FR-2.2/FR-2.5，任务 11-24-edge-switch-latency-hotkey-return）：
  热键/托盘切换接管后的回切触发边取反（取了进入边的对侧 = 远离主控的边）——改为与
  边缘路径同定义的**进入边**（受控屏面向主控的边），无布局时回退受控屏左缘
  （[docs/03 §5](docs/03-input.md)）。
- **快速过边停滞**（FR-2.1/FR-2.2）：主控边缘轮询与工作机受控光标回报节拍
  50ms → 10ms，最坏切换延迟约 50–60ms → 约 10ms。配套落地 docs/03 §5 的"死区"
  条款：受控期间主控边缘命中一律忽略（macOS 受控期光标靠锚定回拉冻结，两次回拉
  之间冲到屏边会被传感器重复命中，把工作机光标拉回进入点、释放按键、重置回切
  免疫窗）+ 控制关系变化后 150ms 防抖。**节拍提速与受控期门控同进同退。**
- **热键切入后 uinput 工作机的回切判定基于虚构位置**（FR-2.2）：`release_all` 不再
  把虚拟光标位置复位到屏幕中心；保持原位切入（`preserve_cursor=1`）后被控端经新增
  的 `InputSink::resync_cursor` 把真实光标校准到切出时的位置
  （[docs/05 §4.5](docs/05-platform.md)）。
- **热键回主控时光标跳到屏幕中央**（FR-2.5，macOS 主控）：回主控落点改为**切入
  锚点**（受控期间所见位置，原地解冻），锚点不可得时不做落点定位、光标随
  物理鼠标（[docs/03 §4.6](docs/03-input.md)）。
- **边缘回切后光标停在中途**（FR-2.2 体验；用户实机反馈"快速甩动回切仍卡顿"）：
  回切时有三段位移本来会丢失——工作机光标顶边期间被屏幕边界截掉的推进量、
  主控最后转发的一批晚于工作机采样、回切触发后到本地输入恢复之间被本地拦截的
  位移。现在全部补回并夹回屏内：uinput 工作机度量顶边行程并随回报
  带回（反向回报的取值范围扩展到端点之外）；主控补在途批与本地拦截段，并在
  回切触发处排空捕获队列后**立即恢复本地输入**，不再等下一个输入拍
  （[docs/03 §5](docs/03-input.md) 回切第 7 条、
  [docs/05 §4.5](docs/05-platform.md)）。补偿量不一次性加到落点上：光标先落在
  贴缘的未补偿位置，再切成 2ms 微步、匀速回放（步数 ≤ 30、总时长 ≤ 60ms）——
  用户实机反馈一次性补偿在快速移动时表现为直接跳跃、10ms 节拍 6 大步仍不顺滑
  （2026-11-25，两轮实测迭代）。
- **macOS 未授权辅助功能时没有引导、授权后必须重启应用**（FR-6.3）：应用启动
  （角色确定后）未授权即请求系统引导框并告警（`--headless` 无界面运行只告警，
  不弹系统框）；主控运行时每秒静默检测一次，授权后约 1s 自动重建捕获源（日志
  「主控捕获源已恢复」），热键与捕获转发随即可用。授权前若已靠边缘切换切入过
  工作机，恢复前先自动释放这次控制关系并提示重新切换（否则键鼠会在授权瞬间
  被转发出去）。捕获源启动失败的告警改为可执行的说法；会话内重建持续失败时
  告警退避（首次立即，之后至多 30s 一条）
  （[docs/05 §1.3](docs/05-platform.md)）。

### Added

- **macOS 应用图标**：`.app` 带应用图标（Finder / Dock 不再显示系统默认空白
  图标），图形与托盘图标同源；入库产物 `scripts/macos/AppIcon.icns`，打包不依赖
  swift（[docs/06 §5.1](docs/06-ui.md)、[docs/10 §1.1](docs/10-install.md)）。

### Changed

- **回切免疫窗按时间计**：接管后 800ms 内的回报不触发回切（原按回报条数计 16 条，
  窗口随采样频率变化）。
- **角落排除改为 5% 角落区**（FR-2.1/FR-2.2）：每屏四角、沿屏宽与屏高各取 5% 的
  矩形内贴边不切入也不回切（原为"两轴同为端点"，每角约 1 点见方）。切入与回切
  两处判定共用 core `layout::CORNER_RATIO`，判定全在主控侧，工作机无需升级。
- **无协议报文变更**（docs/02 的 wire 布局与版本不动；受控光标反向回报的**取值
  范围语义**扩展到端点之外——帧布局与 golden fixtures 不变）：新旧版本可互通
  ——旧主控 + 新工作机时位置校准生效，越界回报照常判极值、幅值被忽略；新主控
  + 旧工作机时回切边、受控期门控、角落区与主控侧节拍生效（工作机回报仍是旧
  节拍、行程为 0，落点只有主控侧两段补偿）。
- **安装说明更正：辅助功能授权按签名匹配**（macOS 部署实测）：ad-hoc 签名的
  应用换装新构建后授权即失效（与安装路径无关），需在系统设置里重新开启——
  此前"TCC 按路径记忆、固定路径安装即可保住授权"的说法已更正
  （[docs/10 §1.1](docs/10-install.md)）。

## [0.1.0] — 2026-11-20

首个发布里程碑：M0–M6 全部任务落地。局域网多机键鼠共享 + 剪贴板同步 + 按键映射工具，
Rust + gpui 实现，macOS / Windows / Linux X11 / Linux Wayland。

### Added — 协议与核心（M1）

- **XCP 协议**（`xcontrol-proto`）：7B 帧头 + 校验、12 种输入事件定长编解码、
  CtrlMsg/Clip* postcard 编解码、XKey 语义键码 131 常量、版本协商、26 个 golden
  fixtures、cargo-fuzz 脚手架（[docs/02](docs/02-protocol.md)）。
- **核心状态机**（`xcontrol-core`）：会话状态机（Idle→Handshaking→Running{paused}→Disconnected，
  转移表 200 组合白名单测试）、屏幕布局邻接图与边缘检测、修饰键同步、`ControlDir` 单向
  门控、帧 dispatcher、剪贴板重组/信用窗口状态机（[docs/03](docs/03-input.md)/[docs/04](docs/04-clipboard.md)）。
- **传输与加密**（`xcontrol-net`）：Noise_XXpsk0 握手（三报文 430µs）+ SecureTransport、
  PSK 派生链（blake3 域分离 + argon2id m=19MiB）、回环传输（延迟/断开注入）、
  握手限速（指数退避封顶 10min）、真实 TCP 长度前缀分帧（[ADR-0002](docs/adr/0002-transport-and-discovery.md)）。
- **mock 平台层**：进程内 host+client 端到端（FR-2.x 全部 12 事件类型）；criterion 基线
  回环 P50 ≈ 0.81µs（NFR-1 复核门槛 5ms 的 ~6000 倍余量，[docs/11](docs/11-benchmarks.md)）。

### Added — 平台层（M2/M3/M5）

- **macOS**：CGEventTap 捕获 + CGEventPost 注入（kVK→HID 全表 112 项对 Apple SDK 头文件
  核验）、辅助功能权限检测、剪贴板文本/图片/HTML（arboard + objc2-app-kit，零 unsafe 写入路径）、
  屏幕边缘切换与 CGWarp 定位语义。
- **Windows**：LL hook 捕获 + SendInput 注入（scancode 表 130 项 MS Learn 核验）、
  剪贴板消息窗口监听。
- **Linux X11**：XRecord 捕获 + XTEST 注入（evdev 表 145 项内核 v6.6 核验）+ XFixes 剪贴板。
- **Linux Wayland**：InputCapture / RemoteDesktop / GlobalShortcuts portal（ashpd）+
  uinput 注入（evdev）+ GestureSensor 手势回退 + CapabilityTable 按 compositor 降级
  （C-2，[docs/05 §4](docs/05-platform.md)）；zwlr_data_control 剪贴板（wl-clipboard-rs）。

### Added — 应用与 UI（M2/M4）

- **`xcontrol-app`**：gpui-kit 单一版本源窗口（QuitMode::Explicit 后台常驻）、状态页/
  布局/映射/热键/剪贴板/通用/日志/关于八设置页、四步首次运行向导（FR-6.3）、
  单实例探测、托盘（gpui-tray 直连，[ADR-0001](docs/adr/0001-gpui-and-tray.md)）与
  三平台开机自启。
- **映射引擎**：两级状态机 + 8 平台预设 + 直通模式 + config v3 四形态规则模型（FR-3.x）。
- **全局热键**：mac 捕获流识别 / win RegisterHotKey / x11 grab×8（FR-2.5）。
- **布局编辑器**（FR-1.3）：方位选择制 + 协议 1.1 Layout 协商（ScreenInfo.owner 跨机 id、
  PublishLayout 三重门控、Next-Prev 完整闭环）。
- **配置**：`xcontrol-config` TOML schema v3 + 迁移链 + 双侧校验 + 原子写；
  `[general].language` 界面语言（FR-6.5）。

### Added — 发现、安全与运维（M3/M6）

- **自动发现**（FR-5.x）：mDNS + UDP beacon + 手动 IP 三方式并存、断线指数退避重连
  （1s→30s）、同网段安全码匹配（≤5s 内连上，实测 2s）。
- **安全加固**（[docs/07](docs/07-security.md)）：主控接入白名单（FR-5.4）、多主控候选
  与记忆确认（FR-5.5）、headless 无 UI 模式（FR-6.4）、OS 钥匙串真接线（keyring 三平台）、
  换码协议（store→config 原子序 + 回滚收口）、进程内窃听中继抓包验证 + fuzz 实跑
  6.6M+5.0M runs 零 crash。
- **可观测性**（NFR-8）：tracing 结构化日志（子系统命名审计表）、滚动文件 + 14 天清理、
  字段级脱敏（REDACTED 层）、`xcontrol-cli --status/--json/--log-tail`。
- **i18n**（FR-6.5）：`xcontrol-i18n` 轻量文案表框架（零依赖 static table，tr/trf），
  zh-CN 完整 + en 全量；UI 层 + 托盘全量 key 化，`[general].language` 切换即时生效。

### Added — 发布工程（M6）

- **基准报告** [docs/11-benchmarks.md](docs/11-benchmarks.md)：criterion 基线 + NFR-1/2/7
  达标判定（回环 P50 0.81µs / 空闲 CPU ≤0.2% / 托盘可用 ≈1s；headless 从机 RSS 一项
  如实记录为 0.1.x 优化项）。
- **打包脚本**：`scripts/macos/dmg.sh`（.app bundle + hdiutil，本机实测 11MB + verify PASS）、
  `scripts/linux/deb.sh`（dpkg-deb，远端实测）、`scripts/linux/appimage.sh`（AppDir +
  appimagetool，远端实测）、`scripts/windows/msi/`（WiX v3 脚本交付，Windows 实跑移交；
  防火墙预配置 custom action 兑现 docs/10 §2.4 蓝本）。
- **CI/发布**：三平台质量闸门（fmt/clippy/test/llvm-cov/check-deps）+ release.yml
  tag 触发产物管线（`v*` tag push 生效）。

### 文档

- [docs/00–11](docs/) 全套规范 + [ADR-0001/0002/0003](docs/adr/)；FR/NFR 编号追踪体系。

### 已知限制（详见 [docs/10 §6](docs/10-install.md)）

- 三平台实机手动验收清单属发布门禁（[docs/05 §7](docs/05-platform.md)）——
  Windows/Wayland 实机运行时行为按清单移交人工回填。
- headless 从机 RSS ≈ 37MB > 30MB 目标（NFR-2 分量，[docs/11 §3](docs/11-benchmarks.md)）——0.1.x 优化项。
- macOS 正式签名/公证、WiX MSI Windows 实跑、CI linux runner gpui 系统库——移交项。
