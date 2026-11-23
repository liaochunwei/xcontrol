# implement: 打包预配置与自签

1. firewall.ps1（幂等 + 语法检查）
2. codesign.sh + 本机实测（release 构建 + 签名 + 验证输出）
3. docs/10-install.md
4. CI release artifact（可选，评估 CI 时长后定）
5. 全量验证

```bash
pwsh -NoProfile -Command "[void][System.Management.Automation.Language.Parser]::ParseFile('scripts/windows/firewall.ps1')" 2>/dev/null || echo "pwsh 不可用，静态自查"
bash scripts/macos/codesign.sh && codesign -dv --verbose=2 target/release/xcontrol-app | head -5
cargo run -p xtask -- check-deps && cargo clippy --workspace --all-targets -- -D warnings
```

回滚：`git checkout -- scripts docs .github`。
