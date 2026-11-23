# implement: 自动发现与断线重连

## 步骤

1. 依赖选型（查树）→ 双登记 → check-deps 绿
2. `discovery.rs` beacon 编解码 + 广播地址纯函数（测试先行）
3. `reconnect.rs` 退避状态机（TestClock 测试）
4. `announce.rs` / `browse.rs`（async 层）
5. app 接线（--host 通告 / --client 发现模式）+ 本机双进程实测
6. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo test --workspace && cargo test -p xcontrol-net
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
cargo llvm-cov -p xcontrol-net --summary-only   # ≥85%
# 本机双进程演练（host 终端 A / client 终端 B）——日志摘录进报告
```

## 回滚点

net/app 增量；`git checkout -- crates/xcontrol-net crates/xcontrol-app Cargo.toml Cargo.lock xtask`。
