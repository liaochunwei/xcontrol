# implement: TCP 传输层

## 步骤

1. tokio 双登记 → check-deps 绿（观察传递树）
2. `framing.rs`（round-trip + 粘包/拆包测试先行）
3. `TcpTransport` + `serve.rs`（localhost 集成测试三路径）
4. CI 收尾（llvm-cov 步骤 + fuzz 冒烟 job）
5. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo test --workspace && cargo test -p xcontrol-net
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
cargo llvm-cov -p xcontrol-net --summary-only          # ≥85%
cargo bench -p xcontrol-net --bench loopback -- --measurement-time 1 --warm-up-time 0.3 --sample-size 10  # 回归不劣化
python3 -c "import yaml,yaml" 2>/dev/null || ruby -ryaml -e "YAML.load_file('.github/workflows/ci.yml')"  # yaml 合法性
```

## 回滚点

net 内新增 + CI 文件；失败 `git checkout -- crates/xcontrol-net .github xtask Cargo.toml Cargo.lock`。
