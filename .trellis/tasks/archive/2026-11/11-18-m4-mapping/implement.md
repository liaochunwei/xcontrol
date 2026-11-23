# implement: 按键映射引擎

1. config 侧裁定（passthrough 字段若加 → v3 迁移按先例；或并入现有字段——读 model.rs 后定）
2. `mapping.rs`：规则编译 → 事件级状态机（错序测试）→ 组合级（pending/flush）→ 预设表 → 直通
3. app client 接线：Dispatcher 注入动作 → MappingEngine → InputSink；reset 钩子接 EnterScreen/LeaveScreen/断开
4. 全量验证

```bash
cargo run -p xtask -- check-deps
cargo test --workspace && cargo test -p xcontrol-core
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
cargo llvm-cov -p xcontrol-core --summary-only   # ≥85%
```

回滚：`git checkout -- crates/xcontrol-core crates/xcontrol-app crates/xcontrol-config Cargo.toml Cargo.lock`。
