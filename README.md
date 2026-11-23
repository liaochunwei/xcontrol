# XControl

局域网多机键鼠共享与剪贴板同步工具 —— 用一套键盘鼠标控制多台电脑（macOS / Windows / Linux X11 / Linux Wayland）。

- **语言**：Rust
- **UI**：[gpui](https://github.com/zed-industries/zed)（Zed 编辑器 UI 框架）
- **形态**：系统托盘常驻 + 按需弹出设置窗口
- **灵感来源**：Deskflow / Synergy / Barrier / lan-mouse，但为 Rust 原生实现，内建加密、自动发现与按键映射；后续规划共享文件夹（见 [FR-7.1](docs/00-requirements.md)）

## 核心特性

| 特性 | 说明 |
| --- | --- |
| 键鼠共享 | 光标移到屏幕边缘即切换到相邻机器，键盘/鼠标/滚轮事件实时转发 |
| 剪贴板同步 | 文本 / 图片 / HTML 富文本在主控与从机间自动同步 |
| 按键映射 | 从机侧可配置映射（如主控 `Cmd` → 从机 `Ctrl`），带平台预设 |
| 安全码 | 主从通过共享安全码认证，传输全程加密 |
| 网段设置 | 可配置允许的网段（CIDR），限制发现与连接范围 |
| 自动发现 | 从机未连接时自动扫描同网段内安全码匹配的主控机，断线自动重连 |
| 快捷键切换 | 全局热键循环 / 定向切换受控电脑 |
| 跨平台托盘 | macOS / Windows / Linux(X11) / Linux(Wayland) 托盘常驻 |
| 共享文件夹 🗺️ | 规划中（v0.2+，FR-7.1）：主从目录互挂与拖放传输，协议已预留扩展点 |

## 术语

| 术语 | 英文 | 说明 |
| --- | --- | --- |
| 主控机 | Host / Server | 物理键盘鼠标所连接、负责捕获输入的电脑 |
| 从机 | Client | 接收输入注入、上报剪贴板变化的电脑 |
| 屏幕 | Screen | 一台机器对应的逻辑屏幕，具有相对布局（左/右/上/下） |
| 安全码 | Security Code | 主从共享的配对口令，用于发现过滤与传输加密（PSK） |
| XCP | XControl Protocol | 本项目自定义通信协议，见 [02-protocol.md](docs/02-protocol.md) |

## 文档索引（规范集）

| 文档 | 内容 |
| --- | --- |
| [00-requirements.md](docs/00-requirements.md) | 需求分析：用户故事、功能/非功能需求、验收标准、非目标 |
| [01-architecture.md](docs/01-architecture.md) | 系统架构：角色模型、线程模型、crate 工作区布局、数据流 |
| [02-protocol.md](docs/02-protocol.md) | XCP 协议规范：发现、握手、加密、帧格式、消息定义 |
| [03-input.md](docs/03-input.md) | 输入事件模型与按键映射规范 |
| [04-clipboard.md](docs/04-clipboard.md) | 剪贴板同步规范 |
| [05-platform.md](docs/05-platform.md) | 平台能力矩阵：各平台输入/剪贴板/热键实现路径与所需权限 |
| [06-ui.md](docs/06-ui.md) | gpui UI 与系统托盘规范 |
| [07-security.md](docs/07-security.md) | 安全模型：威胁模型、安全码、网段隔离、防重放 |
| [08-engineering.md](docs/08-engineering.md) | 工程规范：代码风格、测试策略、CI、提交规范 |
| [09-roadmap.md](docs/09-roadmap.md) | 里程碑与任务分解 |
| [10-install.md](docs/10-install.md) | 安装与运行前配置指南：三平台安装、防火墙预配置、权限引导与验证 |
| [11-benchmarks.md](docs/11-benchmarks.md) | 性能基准报告：criterion 基线、NFR-1/2/7 达标判定与方法论 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录（0.1.0 全里程碑摘要） |

## 开发工作流（Trellis）

本仓库用 [Trellis](https://www.npmjs.com/package/@mindfoldhq/trellis)（`@mindfoldhq/trellis` CLI + ZCode 项目钩子）管理开发：

- **任务系统**：`.trellis/tasks/<MM-DD-slug>/`，每个任务带 PRD 与上下文清单（implement/check 注入规范到子代理）。
- **规范注入**：`.trellis/spec/rust-core`、`.trellis/spec/rust-platform`（规则卡，回链 `docs/` 完整规范）。
- **日常**：ZCode 会话里 `/trellis:continue` 继续/领取任务，`/trellis:finish-work` 收尾；或 CLI：
  ```bash
  python3 .trellis/scripts/task.py list        # 查看任务
  python3 .trellis/scripts/task.py start 11-15-tray-spike
  ```
- **开发者身份**：`python3 .trellis/scripts/init_developer.py <你的名字>`（生成 gitignored 的 `.trellis/.developer`）。

## 状态

**v0.1.0（2026-11-20）**——M0–M6 里程碑全部完成：XCP 协议与回环（P50 0.81µs，[基准报告](docs/11-benchmarks.md)）、macOS/Windows/X11/Wayland 输入与剪贴板、发现与断线重连、映射/热键/托盘/向导/设置、布局协商、安全加固（白名单/多主控候选/headless/钥匙串）、i18n（zh-CN/en）。打包：macOS dmg（本机实测）/ Linux deb+AppImage（实测）/ Windows MSI 脚本（移交实跑）。实机手动验收清单按 [docs/05 §7](docs/05-platform.md) 移交人工（见 [docs/10-install.md](docs/10-install.md) §6）。里程碑明细见 [09-roadmap.md](docs/09-roadmap.md)。

### 快速开始

```bash
# macOS：从源码构建并 ad-hoc 签名（或用 scripts/macos/dmg.sh 出安装包）
# 平台 feature 必传（macos/windows/x11/wayland，默认全关——漏传时进程可启动但无
# 捕获/注入后端，输入转发不可用；详见 docs/10-install.md §1.1）
cargo build --release -p xcontrol-app --features macos
scripts/macos/codesign.sh

# 运行（主控 / 从机 / 回环演示）
cargo run -p xcontrol-app --features macos --            # 无配置启动 → 首次运行向导
cargo run -p xcontrol-app --features macos -- --host --code <两端一致的口令>
cargo run -p xcontrol-app --features macos -- --client <主控ip>:4243 --code <口令>

# 无 UI 的服务器场景 / 诊断
cargo run -p xcontrol-cli -- --status
```

安装包与防火墙/权限预配置见 [docs/10-install.md](docs/10-install.md)；默认端口 TCP+UDP 4243（可配置）。
