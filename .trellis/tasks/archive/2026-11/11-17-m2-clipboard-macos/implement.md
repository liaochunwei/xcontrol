# implement: macOS 文本剪贴板

## 步骤

1. 依赖登记（arboard 全平台 / objc2-app-kit 仅 macos）→ check-deps 绿（重点看 objc2 传递树）
2. 回声判定/超限判定纯逻辑 + 测试
3. `platform/macos.rs`：读写（arboard）→ changeCount 轮询线程（objc2-app-kit）→ 防回环接线
4. ClipboardWatch trait 接线 + 冒烟测试（#[ignore] 本机项）
5. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo test --workspace && cargo test -p xcontrol-clipboard --features macos
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
cargo tree -p xcontrol-clipboard --features macos -e normal
```

## 回滚点

clipboard crate + 白名单；`git checkout -- crates/xcontrol-clipboard xtask Cargo.toml Cargo.lock`。
