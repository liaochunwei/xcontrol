# implement: 最小 gpui 窗口与 --loopback

## 步骤

1. gpui/tracing 双登记 + 白名单 → check-deps 绿 → **验证 gpui profile 告警消失**
2. `state.rs`（快照/CoreHandle）+ `logging.rs`
3. `loopback.rs`（进程内 M1 链路驱动）+ CLI 骨架 → `--loopback --auto-quit` 冒烟先跑通（headless 分支）
4. `session_rt.rs`（TCP 会话运行时）+ localhost 双实例握手集成测试
5. `ui/`（窗口 + 状态页 + 日志页）→ 本机冒烟（AX 元素/退出码）
6. linux CI 处理（exclude 或 feature 组合，验证后注释定稿）
7. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo build --workspace 2>&1 | grep -c "did not match"   # 应为 0
cargo test --workspace && cargo test -p xcontrol-app
cargo run -p xcontrol-app -- --loopback --auto-quit-ms 3000   # exit 0
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
```

## 回滚点

app crate + 白名单 + CI；`git checkout -- crates/xcontrol-app xtask .github Cargo.toml Cargo.lock`。步骤 5 失败可回退到步骤 4 状态（UI 最小化继续交付 3/4 成果）。
