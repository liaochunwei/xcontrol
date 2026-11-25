# 分层红线与模块规则

来源：docs/08-engineering.md §1、docs/01-architecture.md。违反任何一条 = PR 打回。

## 依赖方向（唯一合法图）

```
xcontrol-app/cli → 所有 crate
xcontrol-input / xcontrol-clipboard / xcontrol-net → xcontrol-proto, xcontrol-config
xcontrol-input   → xcontrol-core（re-export core::sink 的 InputSource/InputSink；11-17-m2-input-macos 补录，docs/08 §1 同步）
xcontrol-tray    → xcontrol-i18n, gpui-tray, gpui-kit（`gpui` feature 门控；11-18-m4-tray 修订——状态投影与三平台
                   自启为纯 std，无 proto/config 依赖；gpui-tray 直连 ADR-0001，仅 xcontrol-app 开启；windows 目标
                   另 target 门控 windows-registry——HKCU Run 键自启，safe API；11-19-m6-release 增 xcontrol-i18n——
                   FR-6.5 菜单/状态头/tooltip 文案表，零依赖）
xcontrol-app     → xcontrol-i18n（FR-6.5 UI 层文案；启动读 config [general].language、RuntimeConfigUpdate 消费点
                   set_language 热切——与 log_level 同点，docs/06 §4.1）
xcontrol-i18n    → （零依赖；tr/trf 静态文案表 zh-CN/en，parity 测试钉两表一致；UI 层专用——错误 Display/
                   tracing/CLI 输出 v1 中文单语）
xcontrol-proto   → 仅 serde/postcard 等序列化依赖
xcontrol-core    → xcontrol-proto, xcontrol-config（无 IO、无平台 API）
```

## 红线

1. **`xcontrol-proto` 与 `xcontrol-core` 禁止**：平台 API（CGEvent/win32/x11/wayland）、文件/网络 I/O、tokio 运行时依赖。它们必须能在普通 `cargo test` 全量跑。
2. **平台代码只出现在 feature 门控模块**：`#[cfg(feature = "macos|windows|x11|wayland")]`；公共代码（trait 定义、类型）中禁止 `#[cfg(target_os)]` 判断。平台模块命名固定：`platform/{macos,windows,x11,wayland}.rs`。
3. **async 边界**：tokio 只用于网络/发现/剪贴板流；平台输入层（回调线程）禁止 async，经 `tokio::sync::mpsc` 桥接。
4. **UI 是纯投影**：xcontrol-app 只读 `AppState` 快照流 + 发命令（CoreHandle），禁止直接触碰网络/平台层。
5. **协议变更流程**：docs/02 → 版本号提升 → fixture 更新 → 代码。禁止"先写代码后补文档"。

## 命名

- 事件类型 `XxEvent`，配置节 `XxConfig`，协议消息 `CtrlMsg::Xxx` / 输入帧 kind 表（docs/02 §4）。
- 平台 trait：`InputSource`（捕获）/ `InputSink`（注入）/ `ClipboardWatch`（监听），定义在 xcontrol-input / xcontrol-clipboard。

## 工具链基线

Rust 1.99（rust-toolchain.toml 锁定；M0 spike 实测修订 2026-11-16：gpui-pre 0.3.12 在 1.92/1.95 编译失败、stable 1.99.1 通过，ADR-0001 更新记录）· edition 2024 · gpui 只经 `gpui-kit` 一条依赖线（ADR-0001）。

## Lint 与依赖闸门（M0 脚手架落地，CI 机械强制）

- **Lint 集中管理**：根 `Cargo.toml` 的 `[workspace.lints]`（`unsafe_code = deny`、`clippy::unwrap_used/expect_used/map_unwrap_or = warn`），crate 侧只写 `[lints] workspace = true` 继承；CI 以 `clippy -D warnings` 强制。纯逻辑 crate（proto/core/net/config）另加 `#![forbid(unsafe_code)]`。新增豁免项在根 `[workspace.lints]` 处注释理由（docs/08 §2）。
- **新增依赖必须两处登记**：① 根 `[workspace.dependencies]`；② `xtask` 的 `RED_LINE_ALLOWED_DEPS` 白名单。漏 ② 会被 CI 三平台的 `cargo run -p xtask -- check-deps` 闸门拦下（fail-closed：白名单未登记 + 黑名单命中都会 exit 1）。流程：新依赖先过 docs 评审 → 再登记白名单。
- **传递依赖会撞黑名单**：平台路径类 crate（如 `dirs`→`libc`/`windows-sys`）的传递树会命中黑名单。此类依赖用 **`dep:xxx` 可选依赖 + feature 门控**（先例：config 的 `std-paths`，core 不开该 feature 即保持零平台传递依赖）。给非红线 crate 的直接依赖过白名单走 `GATED_CRATES` 机制（xtask）；若未来 core 不再依赖 config，需让 GATED_CRATES 也跑全树黑名单扫描（防御纵深，见 11-15-xcontrol-config check 报告）。**xtask 白名单同 crate 只允许一个条目**（`allowed_deps_for` 取首个同名条目，新开条目被静默忽略）——追加依赖必须并入既有数组（M2-TCP check 发现）。
- **握手限速键必须按来源 IP**（docs/02 §3.2"每来源 IP"）：不能用 `SocketAddr`（含源端口）——每条 TCP 连接源端口互异，按 Addr 计键退避永不生效（M2-TCP P1 教训，回归测试 `server_rate_limit_accumulates_across_connections_per_ip` 钉死）。从机 connect 侧按拨号地址键控（服务端地址稳定）。
- **snow 0.10 特例（M1 net 先例）**：`default-features = false` + 仅 `use-blake2/use-chacha20poly1305/use-curve25519/use-getrandom` 四 feature——`std` feature 会连带激活 `ring`（上游 `std = [..., "ring/std"]`）。argon2 用 `["alloc"]` 最小组合（`hash_password_into` 要求 alloc，password-hash/base64ct 纯软可接受）。**XX 模式握手需显式本地静态密钥**（snow 不代生成，`generate_keypair()` 一次性密钥）。升级这些 crate 大版本可能改变 feature 语义，check-deps fail-closed 会拦截。
- **noise 报文线上长度**：XXpsk0 三报文实测 [48, 96, 64]B（msg2 最长 96B）；应用明文上限 `MAX_NOISE_PLAINTEXT = 65535 - 16(tag) = 65519`。握手失败唯一变体 `HandshakeFailed`（超时/解密失败/格式/断开全折叠）——**禁止向对端泄露失败原因**（WPS 教训，docs/02 §0）。
- **rustfmt nightly-only 选项**：`imports_granularity`/`group_imports` 仅 nightly 生效，stable `cargo fmt` 会忽略并告警（预期行为，rustfmt.toml 有注释），CI 用 `cargo +nightly fmt`。
- **gpui profile spec 按 registry 包名写**（11-17-m2-app 修订）：gpui 框架本体在依赖图中的包名是 `gpui-pre`（lib 名 `gpui`）、底座 `gpui-base`——M0 预置的 `profile.dev.package.gpui` 永不命中且持续报 `did not match any packages` 告警；现已改为 `gpui-pre` + `gpui-base` + `gpui-kit` 三条 spec（opt-level=3），gpui 入主 workspace 后告警归零。**profile package spec 匹配 registry 包名而非 lib 名**，从 spike 迁移正式依赖时务必按解析出的依赖图核对。
- **发现层事实（M3 m3-discovery 实测）**：mdns-sd 0.22 API 与旧文档漂移（`daemon.register()` 而非 register_service，升级查 registry 源码）；macOS 定向广播不自环（同机 beacon 接收不可测，同机发现走 mDNS/单播探测）；UDP 发现口双进程共存必须 socket2 预绑定 SO_REUSEADDR(+unix SO_REUSEPORT)，且**复用绑定≠数据报复制**（同机双角色单播探测可能被对端监听口吞掉，已文档化）。beacon/TXT 组装函数签名只收公开字段——类型层面杜绝安全码派生物（零派生物断言测 raw 字节 + hex 文本三重形态）。
- **feature 统一撞上游 compile_error（11-19-m6-release 实测）**：同一 workspace 内给同一个上游 crate 启用互斥 feature 会在线性编译时爆——ashpd 0.13 `compile_error!("You can't enable both async-io & tokio features at once")`：本仓 input/wayland 取 `tokio`、gpui-base（gpui-pre-linux）取 `async-io`，单 crate 构建各自通过，**app（打包面）首次把两者拉进同一依赖图才爆**。教训：给"已被 workspace 外成员（gpui）依赖"的上游选 feature 前，先看 gpui 侧启用了什么（`cargo tree -i <crate> -e features`）；xcontrol-app 是唯一多平台 feature 汇聚点，其构建矩阵 = 全部 feature 冲突的暴露面。对位修复见 platform-code.md ashpd 条目。
- **钥匙串统一后端的 feature 门控纪律（11-19-m6-security 先例）**：平台统一库（keyring）以 **optional 依赖 + crate feature**（config `keychain`）进树，红线 crate（proto/core）不启用该 feature——check-deps 默认 feature 面零增量。平台统一库选型必须查树比对对照组：linux Secret Service 有三条实现路径（dbus-secret-service 需系统 libdbus C 库——破坏 mac 侧跨编译检查；内核 keyutils 非 Secret Service 语义；secret-service+zbus 纯 Rust 无 tokio——取此），**"能从 mac 跨编译 check"是硬判据**。平台可用性探测（`is_available()`）以 `NotFound`（未存储）为健康信号、平台级错误为不可用，UI 据此回退 file 0600（不引入启动期硬失败）。
- **白名单/策略门放在握手与限速门前**（FR-5.4 serve 层先例）：本地策略拒绝 ≠ 握手失败——不计限速（退避语义只针对错码爆破）、对端只见断开（无协议反馈，docs/02 §0 同面）、观测面事件带 reason 区分（`HandshakeFailureReason::NotInAllowlist` vs `Handshake`）。限速键 = 来源 IP 的 M2 教训延续：白名单匹配同样按 IP（CIDR 纯函数复用 M3 discovery `parse_cidr`/`cidr_contains`）。
- **换码协议生效链（docs/07 §3.3）**：`store_runtime_config` 统一检测 `security_code_ref` 变化返回 `ConfigDelta`——会话内变化 → Quit 断开 + `SessionEnd::ConfigReload`；空闲循环 → 直接 ConfigReload；supervisor `RuntimeDeps.secrets` 携带解析时引用，每轮 `ensure_secrets` 比对——引用变化即作废缓存重解析重派生（PskCache 属发现循环局部，重建即失效）。**双故障收口顺序裁定：先写 store 后存 config，config 失败回滚 store**（读旧码快照 → 新码入 store → config 落盘 → 失败回滚），半更新态（store 新码 + config 旧引用）比"短暂旧码"更危险。
- **抓包证据的进程内等价形态**（docs/07 §8 第 3 项先例）：tcpdump 需 root（CI/子代理不可得）——用**窃听中继**（TCP relay 逐字节搬运并记录双向原始字节）替代，断言与发现层零派生物测试同纪律（原始字节 + 小写/大写 hex 三重形态零出现）。注意 relay 接线：`into_split()` 的读写半必须**交叉**（client_read→host_write、host_read→client_write），同 socket 的 write 半是回环不是转发。
- **headless = 运行时无 UI 而非编译面分离**（FR-6.4 裁定）：gpui 是 app 硬依赖（ADR-0001），feature 化 ui/tray 面收益不成比例；`--headless` 只跳过 `ui::run_window`/`run_headless`（gpui Application 不构造），纯 tokio 运行时 block_on supervisor + auto-quit 看门狗 drop CoreHandle 收束。headless 下向导不可用——命令行缺 config/码时**显式报错**而非静默挂起。app 的 linux 编译面检查被 gpui-pre C 依赖阻断（cc-rs 需跨编译 gcc，先例 CI 同因 exclude app）——config/net/keychain 各面独立跨编译 check 是 linux 证据的正确载体。
- **映射引擎语义（M4 定稿）**：事件级修饰键映射用 origin→mapped 对照 + **同目标引用计数**（docs/03 §6.2 不变式是 OS 状态配对，"每↓必有↑"事件级配对在引用计数下不成立）；EnterScreen 对齐合成事件**过映射**（Session 跟踪 raw 键空间，绕过会欠映射）；reset 与 ReleaseAll 原子绑定（五条断开路径 + LeaveScreen + Pause 全产 ReleaseAll，勿逐点挂钩）；pending 触发器重复 Down 幂等 no-op（否则 pending 期卡键）；缓存 KeyRepeat/事件在四条 flush 路径统一 resolve（勿吐 raw 码）。keymap schema v3：四形态规则 + keynames 解析表（config），v2 简式迁移按"键名是否修饰键"归类。

- **切换/回切语义（11-24 定稿，任务 11-24-edge-switch-latency-hotkey-return）**：
  - **接管路径必须产出同一个 `outer_edge`**：`LocalCmd::EdgeCrossing` 与 `SwitchAck{ok}` 接管都建立 `ControllingContext{target, outer_edge, taken_over_ms}`，`outer_edge` = 受控屏**面向主控的边** = 进入边。边缘路径 = `opposite_edge(主控滑出边)`；热键路径 = `layout::entry_edge_into(..)` 的返回值**直接就是**（它返回的已是进入边；再取一次对侧得到远离主控的边——热键切入后推回面向主控的边无效的根因，且当时无测试覆盖）。新增任何接管路径必须带状态机用例：切入后面向主控边的极值回报 + 同向移动 → 回切；远离主控边 → 不触发（范本 `switch_takeover_return_edge_faces_host`）。
  - **回切免疫窗按时间**：`RETURN_IMMUNITY_MS = 800`，自 `taken_over_ms` 起算；`now` 由 `step()` 经 `execute_input` / `execute_local` / `edge_return_actions` 下传（core 零 I/O：时间只经 `Clock`）。禁止按回报条数计——节拍一变窗口跟着变。测试用 `pass_immunity(&mut session)` 推进会话自带的 `TestClock`。
  - **角落判定单一出处**：`layout::CORNER_RATIO`（0.05）+ `layout::in_corner_band(v)`；`edge_crossing`（切入）与 `edge_return_actions`（回切）只调它，判定处不得出现第二个比例字面量。单位是比例不是像素——回切判定在主控侧，手里只有归一化坐标。
  - **sink 新增带默认实现的方法时三处透传缺一不可**：`Box<dyn InputSink + Send>`（`sink.rs`）、`MappingSink`、`HotkeyFilterSink`，漏一处平台覆写就不可达；用 `RecordingInputSink` 的计数断言透传（先例 `resync_cursor`）。
  - **"门控"与"节拍"是耦合参数**：受控期间主控边缘命中由 `edge_hit_allowed`（仅 `ControlDir::None` 放行）一律忽略，控制关系变化后另有 `EDGE_SWITCH_DEAD_TIME = 150ms` 防抖；检测节拍 `DEFAULT_POLL_INTERVAL = 10ms`。两者必须成组改，原因见 platform-code.md「macOS 受控期门控与节拍耦合」。
  - **回切行程补偿（11-25 定稿）**：回切时三段位移本来会丢——① 工作机顶边期间被屏幕边界截掉的推进量，② 主控最后转发、在工作机采样之后才到的一批，③ 触发后到主控恢复本地输入之间被本地拦截的位移。① 由被控端度量并随反向回报带回（取值越出 `[0,1]`，见 protocol-rules.md），core 用 `beyond_edge` 解出、经 `Notice::EdgeReturnedToHost.overflow` 交给 app；②③ 在 app：`in_flight_batch`（发出不足 3ms 的那批）+ 排空捕获队列的 `sum_mouse_deltas`，三段相加经 `compensated_release_point` 夹回屏内（留白与 `EDGE_RETURN_INSET` 同源）——但**不平移到落点**（11-25 两轮实测：一次性补偿是瞬移，"10ms 节拍 × 6 大步"每步仍 20 点也是跳）：光标先落未补偿的贴缘基点，夹取后的补偿量由 `replay_slices` 切成 **2ms 微步、匀速**（速度 = 位移 ÷ 60ms——快甩快回；步数 ≤ `RETURN_REPLAY_MAX_STEPS`，单步 ≥ MIN_STEP），回切触发处交给 `spawn_cursor_replay` 专用线程逐 2ms 经 `xcontrol_input::shift_cursor`（**进程级线程安全 warp，不依赖实例状态——`CaptureSource` 含不可克隆的 Receiver，不要试图 clone 它**）回放；重新进入受控或某步失败即停止。手感参数（步长/步数/总时长）集中在 `RETURN_REPLAY_*` 常量。触发回切的那一拍**先排空、随即翻拍**（`reconcile_local_input_suppress`），不等下一个输入拍；排空批仍过鼠标键位图与热键过滤，只是不转发。`Notice` 因带浮点字段不再派生 `Eq`。

## tokio select 取消安全（11-22 两机实测定案，大图断连根因）

- **select! 分支禁止直接 await 非取消安全原语**（`read_exact`/`read_frame`/任何多步收集读）——分支被取消时进度丢失即流错位。收集型读必须 **buffer 化状态机**：socket 读一律取消安全的单次 `read`，进度入调用方持有的缓冲，future 任意取消/重入不丢字节（范本：`TcpTransport::read_buf` + `recv`/`fill`，回归测试 `tcp_recv_cancelled_mid_frame_keeps_bytes`）。
- 钉死教训：「实测通过」对竞态 bug 不是证据——撞窗概率不足 100% 的路径会在复测翻车；排查时 diff 排除法 + 两端日志时间线对齐 + deps 残留取证三者并用。
