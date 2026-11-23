# design: 打包预配置与自签

```
scripts/windows/firewall.ps1   # netsh 防火墙规则（幂等）
scripts/macos/codesign.sh      # ad-hoc 自签 release/debug 产物
docs/10-install.md             # 三平台安装指引
```

- firewall.ps1：`netsh advfirewall firewall add rule name="XControl" dir=in action=allow protocol=TCP localport=4243 profile=private`（UDP 同型）；幂等 = 先 `Get-NetFirewallRule -Name` 判断或 add 时忽略已存在错误；注释"需管理员；MSI 集成时由安装器以提升权限执行"。
- codesign.sh：`cargo build --release -p xcontrol-app` 后 `codesign --force -s - target/release/xcontrol-app`；`codesign -dv --verbose=2` 输出作验证；说明 ad-hoc 与 Developer ID 差异（M6）。
- docs/10-install.md 结构：每平台 安装方式 → 权限引导（链接 docs/05 §6 权限对照）→ 防火墙 → 验证步骤（`--status`）。编号 docs/10 与既有 00-09 不冲突。
