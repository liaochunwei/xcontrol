# implement: 托盘与开机自启

1. xcontrol-tray：TrayAdapter 纯逻辑（状态投影矩阵测试）→ icons 简化起步 → gpui-tray 集成（spike API 锚）
2. app 接线：TrayIcon 生命周期 + menu action → CoreHandle + tooltip 刷新 ≤1s
3. 自启三平台 + mac launchd 实测
4. 角色切换/从机子菜单路由
5. mac 全链路 AX 自动化实测 + 全量验证 + CI 检查

```bash
cargo run -p xtask -- check-deps && cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
# mac 实测：托盘 AX 元素 + osascript 菜单点击（spike 手法）+ launchctl getenv/列表验证
```

回滚：`git checkout -- crates/xcontrol-tray crates/xcontrol-app Cargo.toml Cargo.lock xtask docs`。
