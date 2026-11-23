# implement: xcontrol-net

## 步骤

1. 依赖双登记（snow/argon2/blake3 复用/criterion dev）→ check-deps 绿
2. `psk.rs`（向量测试先行）→ `transport.rs`（trait + Loopback）→ `noise.rs`（回环双端握手）→ `throttle.rs`
3. `tests/chain.rs` 全链路 + xtask dev 白名单条目
4. `benches/loopback.rs` 延迟基准 → 实测 P50 记录进任务 notes
5. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo build --workspace && cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo llvm-cov -p xcontrol-net --summary-only   # ≥85%
cargo bench -p xcontrol-net -- --quick          # P50 < 5ms 断言或记录
```

## 回滚点

- 纯 net 内新增 + 白名单条目；任一步失败 `git checkout -- crates/xcontrol-net xtask`。
