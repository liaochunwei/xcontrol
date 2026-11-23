# implement: 全局热键与切换路由

1. config [hotkeys] 扩四键（字段追加裁定）→ model/validate 测试
2. core hotkey.rs 匹配纯逻辑 + 矩阵测试
3. mac 捕获层接线（app capture → filter）+ win RegisterHotKey + x11 grab_key
4. 路由接线两端（hotkey_rt）+ SwitchRequest 语义贯通
5. SSH 远端 x11 grab_key 实测（注册→XTEST 合成热键→收到）
6. 全量验证 + CI 步骤

```bash
cargo run -p xtask -- check-deps && cargo test --workspace
cargo check -p xcontrol-input --features windows --target x86_64-pc-windows-msvc
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
# SSH 远端：cargo test -p xcontrol-input --features x11（含 grab 实测）
```

回滚：`git checkout -- crates Cargo.toml Cargo.lock xtask .github docs`。
