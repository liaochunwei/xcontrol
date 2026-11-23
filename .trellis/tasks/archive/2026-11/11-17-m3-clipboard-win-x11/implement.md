# implement: Windows/X11 剪贴板

1. 依赖确认（arboard 既有；windows/x11 target）→ check-deps 绿
2. `platform/windows.rs`（监听窗口 + 重试）→ target check
3. `platform/x11.rs`（XFixes 监听 + wait 语义）→ target check
4. trait 统一 + EchoFilter 复用验证
5. 全量验证 + 移交清单

```bash
rustup target add x86_64-pc-windows-msvc x86_64-unknown-linux-gnu
cargo check -p xcontrol-clipboard --features windows --target x86_64-pc-windows-msvc
cargo check -p xcontrol-clipboard --features x11 --target x86_64-unknown-linux-gnu
cargo test --workspace && cargo run -p xtask -- check-deps
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
```

回滚：`git checkout -- crates/xcontrol-clipboard xtask Cargo.toml Cargo.lock`。
