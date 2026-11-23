# implement: 结构化日志与 --status

1. tracing-appender/serde_json(如需) 双登记 → check-deps 绿
2. logging.rs 扩展（rolling + EnvFilter + REDACTED Visit）+ 测试
3. cli --status（--json）+ 本机实测
4. 命名审计 + spec 卡
5. 全量验证

```bash
cargo run -p xtask -- check-deps
cargo test --workspace && cargo run -p xcontrol-cli -- --status && cargo run -p xcontrol-cli -- --status --json
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
```

回滚：`git checkout -- crates/xcontrol-app crates/xcontrol-cli Cargo.toml Cargo.lock xtask`。
