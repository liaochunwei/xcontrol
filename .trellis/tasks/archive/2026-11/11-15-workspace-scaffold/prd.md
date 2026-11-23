# Workspace 脚手架与三平台 CI 矩阵

## Goal

把 docs/08-engineering.md §1 的仓库布局落成可编译的 Cargo workspace，并让三平台 CI 从第一天起跑通（M0 出口条件）。

## Requirements

1. workspace 根：`Cargo.toml`（members = `crates/*` + `xtask`；profile 为 gpui 系依赖设 `opt-level = 3`）。
2. 空 crate 骨架（每个含 crate 级文档注释 + feature 声明 + 占位单测，无业务代码）：
   `xcontrol-proto / core / net / input / clipboard / tray / config`（lib）+ `xcontrol-app / xcontrol-cli`（bin）+ `xtask`。
3. 工具链与配置：`rust-toolchain.toml`（1.92）、`rustfmt.toml`、`clippy.toml`、`deny.toml`。
4. CI：`.github/workflows/ci.yml` 矩阵 macOS-14(arm64) / ubuntu-24.04 / windows-2022，步骤 = fmt(仅 linux) + clippy + test + build；linux 附加 cargo-deny（llvm-cov 可并入或延后 M1，注明即可）。
5. 依赖方向自检：`cargo tree` 脚本断言 proto/core 无平台依赖（CI 审查步骤）。

## Acceptance Criteria

- [ ] `cargo build --workspace && cargo test --workspace` 本地绿
- [ ] 三平台 CI 全绿（linux 首跑若无 Vulkan 允许跳过 gpui 相关编译，注明后续修复）
- [ ] crate 命名 / feature 名与 docs/08 §1/§3 及 rust-core spec `architecture-rules.md` 完全一致
- [ ] 每个空 crate 有占位单测，CI test 步骤非空跑

## Notes

- 本任务不实现任何功能逻辑——空壳 + CI 即完成。
- Windows 需 MSVC 工具链；不要在 CI 里装无关全局依赖。
