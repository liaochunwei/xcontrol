# implement: xcontrol-core

## 步骤

1. blake3 双登记（根 deps + xtask core 白名单条目 + 注释理由）→ check-deps 绿
2. `clock.rs` → `mods.rs` → `layout.rs`（纯函数先行，单测随写）
3. `session.rs` 状态机 + 转移表 + 心跳语义（虚拟时钟测试）
4. `sink.rs` trait + `route.rs` dispatcher（未知 kind 跳过、Paused 拒绝）
5. `derive_session_id` + 生命周期单测
6. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo build --workspace && cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo llvm-cov -p xcontrol-core --summary-only   # ≥85%
cargo tree -p xcontrol-core -e normal            # 零平台/零 tokio 证明
```

## 回滚点

- 纯 core 内新增，任一步失败可 `git checkout -- crates/xcontrol-core` 回退。
