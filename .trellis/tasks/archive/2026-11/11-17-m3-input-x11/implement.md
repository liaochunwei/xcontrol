# implement: X11 输入层

1. linux target + x11rb 双登记 → check-deps + target check 绿
2. `x11keymap.rs`（+8 关系全量断言 + 异常表锚点）
3. `platform/x11.rs`：注入（XTEST）→ 捕获（XRecord）→ trait 接线 → Text 策略落地
4. 移交清单文档化
5. 全量验证

```bash
rustup target add x86_64-unknown-linux-gnu
cargo check -p xcontrol-input --features x11 --target x86_64-unknown-linux-gnu
cargo test --workspace && cargo run -p xtask -- check-deps
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
```

回滚：`git checkout -- crates/xcontrol-input xtask Cargo.toml Cargo.lock`。
