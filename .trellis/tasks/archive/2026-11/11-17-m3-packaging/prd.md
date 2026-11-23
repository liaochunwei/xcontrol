# M3: 打包预配置与自签

## Goal

Windows 防火墙/安装器预配置 + macOS 自签起步——三平台"能被装上并跑通网络"的发布前置（docs/02 §2.3 防火墙注意 + docs/05 §2 打包行）。

## Requirements

1. **Windows 防火墙预配置**（docs/02 §2.3：安装器必须预授权，避免首监听弹窗误拒）：`netsh advfirewall firewall add rule` 脚本（TCP+UDP 4243，Private profile，入站）——`scripts/windows/firewall.ps1`（管理员运行）+ MSI 安装器集成说明文档（实际 MSI 制作工具选型延后 M6，本任务交付脚本 + 说明）。
2. **macOS 自签**（docs/05 §1"正式/Ad-hoc 签名，否则应用防火墙反复拦截"）：`scripts/macos/codesign.sh`——ad-hoc 签名（`codesign --force --deep -s -`）release 产物 + 说明（正式 Developer ID 证书 M6）；本机实测对 xcontrol-app 二进制自签成功。
3. **安装文档**：`docs/10-install.md`（新）——三平台安装/防火墙/权限指引（mac 辅助功能授权、win 专用网络、Linux 后续）；与 README 打通。
4. **CI 产物**：release build job（可选轻量：三平台 `cargo build --release` artifact 上传）——本任务可选，若 CI 时长顾虑则注明延后 M6。

## Acceptance Criteria

- [ ] firewall.ps1 幂等（重复运行无害——`-ErrorAction SilentlyContinue` + 检查已有规则）+ 语法验证
- [ ] codesign.sh 本机实测对 debug/release 产物签名成功（`codesign -dv` 验证）+ 未签名对照说明
- [ ] docs/10-install.md 三平台指引（引用 FR/权限文档编号体系）
- [ ] CI 变更（如有）yaml 合法
- [ ] docs 新增符合编号体系（docs/10 编号确认与既有目录不冲突）

## Notes

- 本任务以脚本 + 文档为主，不含完整安装器工程（MSI/DMG 工具链 M6）。
- Windows 脚本无法本机执行验证——语法静态检查（PowerShell parser 若可用）+ 逻辑自查 + 移交清单。
