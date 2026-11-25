# tracing 子系统命名审计表与脱敏纪律

来源：docs/08 §2（日志）、NFR-8（docs/00）。11-17-m3-status 全仓审计建立
（`grep -rn 'target: "' crates/*/src` 实测，2026-11-18）；此后新增子系统必须
先改本表再落 `target:`，否则 check 评审打回。

## 规则

1. **发布点显式 `target:` 声明**，禁止裸宏（默认 target 会把模块路径泄进日志，
   破坏子系统过滤与审计面）。
2. **子系统前缀三段制**：`app::`（UI/CLI/装配）、`session::`（会话运行时）、
   `net::`（握手/传输）。前缀即过滤单元——EnvFilter 指令按 target **前缀匹配**
   （如 `net::=debug` 命中 `net::serve`/`net::connect`）。
3. **测试专用前缀 `app::test`**：不复用任何生产前缀，保证命名审计 grep 不被
   测试代码污染。
4. **平台/网络层零日志**：input / clipboard / net / core / proto 均不依赖
   tracing（依赖闸门见 architecture-rules.md）——平台层事件计数经 app 边缘
   传感器（app::edge）承载；net 的握手失败折叠经
   `xcontrol_net::Server::subscribe_handshake_failures` 观测面上抛，由
   app accept 循环落 `tracing::warn!`（FR-1.4"拒绝并记录日志"主控侧）。
   `cli::` 前缀预留（xcontrol-cli 当前只输出 stdout，无 tracing 事件）。

## 命名审计表（2026-11-18 全仓实测）

| target | 使用位置（crate/文件） | 用途 |
| --- | --- | --- |
| `app::main` | xcontrol-app main.rs | 进程装配、模式入口、会话任务收尾 |
| `app::config` | xcontrol-app main.rs | 配置加载/兜底 |
| `app::log` | xcontrol-app logging.rs | 日志系统自身（装配完成、滚动清理线程） |
| `app::ui` | xcontrol-app ui/mod.rs | 窗口/设置生命周期 |
| `app::settings` | xcontrol-app ui/{mod,layout,general,hotkeys,mapping,clipboard}.rs、session_rt.rs（RuntimeConfigUpdate 消费点） | 设置页配置提交/控件变更/运行时配置热更新留痕（11-21 UX 打磨轮表补录——target 此前已在用，漏登记） |
| `app::state` | xcontrol-app state.rs | 核心命令投递失败告警 |
| `app::edge` | xcontrol-app edge.rs、session_rt.rs | 边缘传感器/屏幕切换 |
| `app::loopback` | xcontrol-app loopback.rs | 回环演示装配 |
| `app::discover` | xcontrol-app main.rs | 通告器（mDNS/beacon）开启/降级 |
| `app::tray` | xcontrol-app tray_rt.rs | 托盘装配/动作分发/快照刷新循环（11-18-m4-tray） |
| `app::autostart` | xcontrol-app tray_rt.rs | 开机自启切换（enable/disable 结果留痕；11-18-m4-tray） |
| `app::test` | xcontrol-app tests | 测试专用 |
| `session::rt` | xcontrol-app session_rt.rs、main.rs | 会话运行时驱动（tokio 执行域）与循环退出告警 |
| `session::clip` | xcontrol-app clip_rt.rs、clip_channel.rs、session_rt.rs（防御剥离） | 剪贴板运行时（发送/交付观测）+ 独立剪贴板通道（接入/就绪/断开/重连；11-21 回归双连接） |
| `session::loopback` | xcontrol-app loopback.rs | 回环会话事件 |
| `net::serve` | xcontrol-app session_rt.rs（主控侧） | 监听就绪/从机接入/握手失败告警 |
| `net::connect` | xcontrol-app session_rt.rs（从机侧） | 拨号/重连/候选握手失败 |
| `net::transport` | xcontrol-net transport.rs（crate 内直接打，11-21 过审 tracing 依赖） | 流错位重同步锁定候选（warn）/传输态解密失败（warn）——大图断连排查盲区教训：net 层静默使错位第一因不可见 |

审计结论（2026-11-24 复核更正）：`input::` 前缀**已在用**——`input::macos`（platform/macos.rs：tap/锚定/warp 失败 warn、HID 位置探针 info）与 `input::wayland`（platform/wayland.rs：受控光标采样 debug、环境探测日志）合计约 48 处；原"无 target 使用"的结论自平台模块接入 tracing 起即过期。两段式 `input::<platform>` 与三段制并存（统一改名要动约 48 处调用，留待日志面整体整理时处理），新增平台日志沿用并在本表登记。`clipboard::` 仍无 target 使用（M1 遗留的 app::/session::/net:: 三前缀已在本表固化
（11-21 UX 打磨轮复验补录 `app::settings`——此前漏登记；11-21 大图断连排查
补录 `net::transport`——net crate 唯一直接日志点，此前全部经 app 层转打）。

## 脱敏纪律（NFR-8 机械化层）

- 发布点纪律（error-handling.md 规则 6）之外，`xcontrol-app/src/logging.rs`
  对**事件字段名**做机械脱敏：黑名单 `code / secret / password / token /
  passphrase / psk / key`（11-17-m3-status check 评审增补 psk/key——XXpsk0
  协议密钥材料字段名；含 key 字母的整段词如 keymap/hotkeys 不误伤），按
  "分段后逐段比对"命中（分隔符 `_-.`/空格 + 驼峰边界切词，
  大小写不敏感）→ 值替换 `[REDACTED]`；
- 覆盖全部 `record_*` 原语（str/数值/布尔/错误/Debug）；stdout、滚动文件、
  UI 环形缓冲三处输出共用（`RedactingVisitor` + `RedactedFormat`）；
- `message` 内容不做文本扫描（内嵌脱敏靠发布点纪律 + 本表审计——design.md
  拍板"仅拦截 struct field"）；
- **宁可过度脱敏**：`error_code` 等含 "code" 段的良性字段也命中（观测性损失
  可接受，泄露不可接受）；新增字段名若被误伤，优先改名而非放宽黑名单。
