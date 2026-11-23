# implement: mock 平台层与全链路回环验证

## 步骤

1. mock 模块（input/sink/clipboard）+ feature 组合测试
2. core 侧剪贴板重组/信用窗口纯逻辑（若 T3 缺口）+ 单测
3. 端到端场景测试 ×6 组（键盘保序/拖拽/修饰键/剪贴板防护/心跳判死/版本协商）
4. FR-2.x 覆盖清单核对（12 种 kind）
5. 基准运行 + 数据记录
6. 全量验证

## 验证命令

```bash
cargo run -p xtask -- check-deps
cargo test --workspace
cargo test -p xcontrol-core --features mock-platform
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
cargo bench -p xcontrol-proto -- --quick && cargo bench -p xcontrol-net -- --quick
cargo llvm-cov -p xcontrol-core -p xcontrol-net --summary-only   # ≥85%
```

## 回滚点

- mock 模块独立可 revert；端到端测试逐文件独立。
