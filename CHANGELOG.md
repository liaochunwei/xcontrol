# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 惯例与
[语义化版本](https://semver.org/lang/zh-CN/)；`0.x` 期间协议 minor 版本可能不兼容，
兼容性以 XCP `XCP-VERSION` 协商为准（docs/02）。

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
