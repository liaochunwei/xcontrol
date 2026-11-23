# 08 · 工程规范

> 状态：草案 v0.1 · 适用于本仓库全部 Rust 代码与 CI/发布流程。

## 1. 仓库结构

```
XControl/
├── README.md
├── AGENTS.md               # AI 辅助开发的行为约定
├── Cargo.toml              # workspace 根
├── rust-toolchain.toml     # 锁定工具链
├── rustfmt.toml
├── clippy.toml
├── deny.toml               # cargo-deny 依赖审查
├── docs/                   # 规范集（本目录）
│   └── adr/                # 架构决策记录
├── crates/
│   ├── xcontrol-proto/     # XCP 协议：消息定义、序列化、帧编解码（零平台依赖）
│   ├── xcontrol-core/      # 无 IO 的核心状态机：会话、屏幕布局、事件路由、映射引擎
│   ├── xcontrol-net/       # 发现（UDP 广播/mDNS）、传输（TCP/QUIC）、加密
│   ├── xcontrol-input/     # 平台输入抽象 trait + 平台实现（feature 门控）
│   ├── xcontrol-clipboard/ # 平台剪贴板抽象 + 同步策略
│   ├── xcontrol-tray/      # 托盘适配层：状态投影 + 图标/菜单 + 三平台自启（gpui-tray 直连，ADR-0001）
│   ├── xcontrol-config/    # 配置模型（TOML）、迁移、校验
│   ├── xcontrol-i18n/      # 轻量文案表框架（FR-6.5）：tr(key) 静态表 zh-CN/en，零依赖
│   ├── xcontrol-app/       # gpui UI + 进程入口（bin）
│   └── xcontrol-cli/       # headless / 调试入口（bin）
├── xtask/                  # 构建/打包/基准等开发任务
├── scripts/                # 平台打包/预配置脚本（windows/firewall.ps1、macos/codesign.sh；见 docs/10）
└── .github/workflows/      # CI（ci.yml 质量闸门；release.yml 产物管线，tag/manual 触发）
```

分层依赖规则（`→` 表示"可依赖"）：

```
xcontrol-app/cli → 所有 crate
xcontrol-input / xcontrol-clipboard / xcontrol-net → xcontrol-proto, xcontrol-config
xcontrol-input → xcontrol-core                    # re-export core 的 InputSource/InputSink trait（11-17-m2-input-macos 补录：
                                                  #   M1 已把两 trait 定义在 core::sink 供 mock-platform 回环测试用，
                                                  #   input 实现并 re-export；core 仍零平台依赖，方向不破坏红线）
xcontrol-tray → xcontrol-i18n, gpui-tray, gpui-kit（gpui feature）
                                                  # 托盘适配层（11-18-m4-tray 修订）：状态投影与三平台自启为纯 std（无 proto/config
                                                  #   依赖——投影输入为 crate 内轻量 TraySnapshot）；gpui-tray 直连（ADR-0001）
                                                  #   经 `gpui` feature 门控，仅 xcontrol-app 开启——linux CI 排除 app 后
                                                  #   本 crate 保持纯 std 编译面；windows 目标另 target 门控 windows-registry
                                                  #   （HKCU Run 键自启，safe API，crate 级 forbid(unsafe_code)）
                                                  #   11-19-m6-release 增补：xcontrol-i18n（FR-6.5 菜单/状态头/tooltip 文案表）
xcontrol-proto → (仅 serde/postcard 等序列化依赖)
xcontrol-core → xcontrol-proto, xcontrol-config   # 无 IO、无平台 API
```

**红线**：`xcontrol-proto` 与 `xcontrol-core` 不得依赖任何平台 API（no I/O，可在 CI 的普通单元测试中全量运行）；平台代码只出现在 `xcontrol-input` / `xcontrol-clipboard` / `xcontrol-tray`（自启后端与 gpui 集成，feature 门控）/ `xcontrol-app` 的 feature 门控模块中。

## 2. Rust 约定

- **Edition**：2024。
- **MSRV**：**1.99**（M0 托盘 spike 实测修订 2026-11-16：gpui-pre 0.3.12 在 1.92/1.95 编译失败、stable 1.99.1 全树通过——调研期"MSRV 1.92（gpui-kit 0.9.1 要求）"断言有误，见 [01-architecture.md](01-architecture.md) §6 与 ADR-0001 更新记录）；`rust-toolchain.toml` 与 CI 三平台锁定同一版本。
- **格式化**：`cargo +nightly fmt`（rustfmt.toml：`imports_granularity=Module` 等），CI 强制。
- **Lint**：`clippy` 默认全开 + `pedantic` 子集。豁免与 lint 声明统一放根 `Cargo.toml` 的 `[workspace.lints]`（`unsafe_code = deny`、`clippy::unwrap_used/expect_used/map_unwrap_or = warn` 等），各 crate 以 `[lints] workspace = true` 继承，CI 以 `clippy -D warnings` 强制；每个 crate 再按需在源码加 `#![forbid(unsafe_code)]` 等收紧项。禁止全局 `#![allow(clippy::pedantic)]`；新增豁免项必须在根 `[workspace.lints]` 处注释说明理由（M0 脚手架落地时定稿的机制，取代逐 crate `#![deny]` 属性写法）。
- **unsafe 规则**：每个 `unsafe` 块必须有 `// SAFETY:` 注释说明不变量；平台绑定层（input/clipboard）是唯一预期出现 unsafe 的地方。
- **错误处理**：
  - 库 crate 用 `thiserror` 定义错误类型；
  - bin（app/cli）顶层用 `anyhow`；
  - 禁止 `unwrap()/expect()` 出现在任何可能由外部输入触发的路径（测试代码除外）；CI 用 `clippy::unwrap_used` 限制非测试代码。
- **日志**：统一 `tracing` + `tracing-subscriber`（fmt + 滚动文件），子系统和 span 命名规范见 `xcontrol-app` 模板（子系统 target 命名审计表：`.trellis/spec/rust-core/logging-targets.md`，新子系统先改表）；禁止 `println!` 调试输出入库。级别口径（NFR-8，11-17-m3-status）：`config [general].log_level` + `--log-level` 覆盖 + `XCONTROL_LOG` 追加子系统指令（env 风格如 `net::=debug`；RUST_LOG 有意不读——口径单一）；滚动文件写 `<data_dir>/logs/`（前缀 `xcontrol.log.`，daily rotation，保留 14 天自清理）；事件字段名命中黑名单（code/secret/password/token/passphrase/psk/key）机械替换 `[REDACTED]`（防御纵深，宁可过度脱敏）。
- **异步运行时**：统一 `tokio`（网络、发现）；**输入平台层不用 async**（系统回调线程模型），与 tokio 之间用 `tokio::sync::mpsc` 桥接。
- **unsafe 与 lints**：workspace `unsafe_code = deny` 对平台模块的放行方式（11-17-m2-input-macos 定稿）：crate 继承 `[lints] workspace = true` 不变，平台模块文件（`platform/macos.rs` 等）文件级 `#![allow(unsafe_code)]` 显式放行——源码内属性优先于 lint 配置，allow 落在 unsafe 实际出现的位置（局部、可见、可审查），每个 unsafe 块仍强制 `// SAFETY:`。
- **序列化**：协议一律用 `postcard`（紧凑、no_std 兼容）；配置文件用 `toml` + `serde`。

## 3. 命名与代码组织

- crate/模块：`snake_case`；平台模块统一命名：`platform/macos.rs`、`platform/windows.rs`、`platform/x11.rs`、`platform/wayland.rs`。
- 平台实现统一实现 `xcontrol-input`/`xcontrol-clipboard` 定义的 trait，通过 feature 启用：`--features macos|windows|x11|wayland`。**平台自动选择不做 crate 自带 build.rs 推断**（11-17-m3-input-win 裁定，方案 B）：cargo 的 feature 解析先于构建脚本执行，build.rs 无法为自身 crate 启用 feature（循环）；它只能输出 `rustc-cfg`，而平台 cfg 判断出现在公共代码违反架构红线 2。自动选择由两个显式面承载：① `xcontrol-app` 的 **target-specific optional dependency**——cargo 原生按目标平台解析 `[target.'cfg(target_os = "…")'.dependencies]`，声明即选择（M4 app 接线平台 feature 时采用）；② 单平台发布构建由打包脚本固定 `--features` 组合。也允许显式组合（Wayland 上叠加 x11 兜底）。
- 公共类型命名：事件类型 `XxEvent`，配置节 `XxConfig`，协议消息 `MsgXxx`（详见 [02-protocol.md](02-protocol.md)）。

## 4. 测试策略

| 层级 | 内容 | 工具 |
| --- | --- | --- |
| 单元测试 | proto 帧编解码（golden bytes）、core 状态机（会话/路由/布局/映射）、config | `cargo test`，全平台跑 |
| 回环集成测试 | 进程内 host+client 对接 mock 平台层，跑完整握手→事件→剪贴板流程 | `cargo test -p xcontrol-core --features mock-platform` |
| 协议一致性 | 固化样本帧（`tests/fixtures/*.bin`）防破坏性变更；版本协商兼容表 | 脚本生成 + 校验 |
| 性能基准 | 事件编码/解码吞吐、端到端延迟（mock 传输） | `criterion`，`cargo bench` |
| 手动验收 | 每平台按 [05-platform.md](05-platform.md) 的测试清单执行，发布前必过 | checklist 模板 |

覆盖率要求：`xcontrol-proto`、`xcontrol-core` 行覆盖 ≥ 85%（`cargo-llvm-cov`，CI 报告）；平台层不设数字指标，以手动清单为准。

## 5. CI / CD

GitHub Actions，`build-test` 矩阵：

| 平台 | 目标 | 步骤 |
| --- | --- | --- |
| macOS 14 (arm64) | `aarch64-apple-darwin` | fmt(仅 lint job) + clippy + test + build |
| Ubuntu 24.04 (x64) | `x86_64-unknown-linux-gnu` | 同上 + `cargo-deny check` + llvm-cov |
| Windows 2022 (x64) | `x86_64-pc-windows-msvc` | clippy + test + build |

附加：PR 必须过三平台 green；nightly job 跑 `cargo bench` 记录延迟基线；release 流程见 §7。

## 6. 提交与分支

- **分支模型**：trunk-based。`main` 始终可构建；feature 分支命名 `feat/FR-3.1-keymap-presets`、`fix/...`（引用需求/issue 编号）。
- **提交信息**：Conventional Commits（`feat: / fix: / docs: / refactor: / test: / chore:`），scope 用 crate 名或需求域（如 `feat(proto): FR-2.3 key repeat event`）。
- **PR 规范**：≤ 400 行 diff 优先；必须关联需求编号；包含测试说明；平台相关改动标注"已在 <平台> 实测"。
- **文档同步**：改协议/架构行为必须同 PR 更新对应规范文档与 ADR。

## 7. 版本与发布

- 版本：semver；`0.x` 期间协议 minor 即可能不兼容，协议兼容性以 `XCP-VERSION` 协商为准（见 [02-protocol.md](02-protocol.md)）。
- 产物：`dmg`（macOS，universal）、`msi`（Windows）、`deb` + `AppImage`（Linux）；由 `xtask` 统一驱动。
- 发布 checklist：三平台手动验收清单全绿 → changelog → tag → 构建 → 校验和 → GitHub Release。

## 8. ADR（架构决策记录）

凡引入新依赖、改变分层规则、更换平台实现路径的决策，写 `docs/adr/NNNN-<slug>.md`（模板：背景 / 备选 / 决策 / 后果）。已有 ADR 索引见 [adr/README.md](adr/README.md)（首条 ADR 将记录 gpui 与传输层选型，规范冻结时补齐）。
