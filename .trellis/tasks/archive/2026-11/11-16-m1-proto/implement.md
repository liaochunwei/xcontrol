# implement: xcontrol-proto

## 步骤

1. 依赖登记：根 Cargo.toml `[workspace.dependencies]` + xtask 白名单（postcard / serde 如需 / criterion(dev)）→ `cargo run -p xtask -- check-deps` 绿
2. `error.rs` → `xkey.rs`（常量表先出）→ `frame.rs`（帧头 + kind 常量）
3. `input.rs`：12 种事件编解码 + 合帧辅助（每实现一种即补 round-trip 测试）
4. `ctrl.rs` / `clip.rs` / `version.rs`
5. golden fixtures 生成：实现 fixture 生成测试（确定性样本编码落盘 `tests/fixtures/xcp/*.bin`），后续 CI 比对
6. 负例与矩阵测试（docs/02 §8 清单）
7. `fuzz/` 脚手架（2 targets + README 说明 nightly 运行方法，不进 CI）
8. `benches/` criterion 两组
9. 全量验证（见下）

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo build --workspace && cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo llvm-cov -p xcontrol-proto --summary-only   # ≥85%
cargo bench -p xcontrol-proto --bench codec -- --measurement-time 1 --warm-up-time 0.3 --sample-size 10  # 基准可跑（criterion 无 --quick）
```

## 回滚点

- 步骤 1-2 后：纯新增可整体 revert；步骤 3 起每模块独立提交粒度（但不实际分 commit，主会话统一收口）。
