# implement: Windows 输入层

1. `rustup target add x86_64-pc-windows-msvc` → windows crate 双登记 → check-deps + target check 绿
2. `winkeymap.rs` 表 + 锚点测试
3. `platform/windows.rs`：注入（SendInput）→ 捕获（hook + 消息循环）→ trait 接线
4. build.rs 缺口裁定（文档先行）+ 落地
5. 全量验证 + 移交清单

```bash
cargo check -p xcontrol-input --features windows --target x86_64-pc-windows-msvc
cargo test --workspace && cargo run -p xtask -- check-deps
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
```

回滚：`git checkout -- crates/xcontrol-input xtask Cargo.toml Cargo.lock docs/`。
