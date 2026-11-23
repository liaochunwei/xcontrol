# implement: macOS 输入捕获与注入

## 步骤

1. 依赖登记（core-graphics/core-foundation，macos feature 门控）→ check-deps 绿
2. `keymap.rs` 全量表 + 锚点测试（纯数据先行，CI 可测）
3. `platform/macos.rs`：权限检测 → 注入器（f64 累加器）→ 捕获 tap（顺序：先注入后捕获，冒烟风险递增）
4. InputSink/InputSource trait 接线 + release_all
5. 屏幕几何辅助
6. 冒烟测试（#[ignore] 本机项 + 默认可跑的 Denied 路径）
7. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo test --workspace && cargo test -p xcontrol-input --features macos
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
cargo tree -p xcontrol-input --features macos -e normal
```

## 回滚点

input crate + 白名单；`git checkout -- crates/xcontrol-input xtask Cargo.toml Cargo.lock`。
