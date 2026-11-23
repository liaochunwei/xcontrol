# implement: 设置编辑器与首次运行向导

1. gpui-kit 组件库开启（feature/编译面验证）+ 窗口骨架页路由
2. wizard.rs 四步 + 完成写 config + 运行时触发（AX 实测脚本）
3. 五设置页（每页：UI + save + 生效路径 + 测试）
4. RuntimeConfigUpdate 生效框架 + 各字段时点表（docs/06 §4 同步）
5. mac AX 全链路实测（向导四步/映射页改动/通用页）+ 全量验证

```bash
cargo run -p xtask -- check-deps && cargo test --workspace
cargo run -p xcontrol-app -- --loopback --auto-quit-ms 3000   # 回归
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
# AX 实测：无配置启动（临时 HOME 或 --config 指向不存在路径）→ 向导四步点击
```

回滚：`git checkout -- crates/xcontrol-app Cargo.toml Cargo.lock xtask docs`。
