# M6: 安全加固与能力补全

## Goal

FR-5.4 白名单 / FR-5.5 多主控候选 / FR-6.4 headless / keychain 真接线 / 换码协议——安全相关能力收口 + docs/07 自查清单全绿。

## Requirements

1. **FR-5.4 白名单/网段限制**：主控只接受白名单 CIDR/instance-id 的连接（握手前 IP 检查——serve 层前置过滤）；config `[net] allowlist` 字段；UI 网段页已有（M3 连接页）接线。
2. **FR-5.5 多主控候选**：发现多个主控 → 候选列表（托盘子菜单/状态页）用户确认选择；记忆已确认主控（config）；错码候选排除语义（M3 软排除 → 用户可见）。
3. **FR-6.4 headless**：`--headless` 纯无 gpui 路径（xcontrol-cli 或 app headless 分支——docs/06 §2"Application::headless() 或纯无 gpui 路径"裁定）；服务器场景验证（SSH 远端无显示环境实测！）。
4. **keychain 真接线**：mac Keychain（security-framework crate 或 keyring crate——查树裁定）/win Credential Manager/linux Secret Service——M4 遗留收口；`security_code_ref = keychain:xcontrol` 生效路径；回退 file 0600 保持。
5. **换码协议**：改安全码 = 全部会话断开重握手（docs/07 §3）——断开/重派生/重连链路 + 原子性（M4 遗留 store 写成功 + config save 失败双故障收口）。
6. **docs/07 自查清单**：§5 五项逐条核验（发现流量无派生物✓M3/无部分反馈✓/全流量密文抓包——本机 tcpdump/ itrace 实测/钥匙串三平台——mac 实测+移交/超限乱序 fuzz——proto fuzz targets 实跑一次 M1 遗留收口）。

## Acceptance Criteria

- [ ] 白名单：非授权 IP 连接被拒（集成测试）；授权路径不受影响
- [ ] 多主控候选：模拟两主控发现 → 候选呈现 → 选择记忆
- [ ] headless：SSH 远端无显示实测 `--headless` 运行（日志/托盘缺失降级）
- [ ] keychain mac 实测（写入/读取/删除 + 0600 file 回退对照）
- [ ] 换码端到端：改码 → 旧会话断开 → 新码握手成功
- [ ] docs/07 §5 清单逐项核验记录（grep/实测证据）
- [ ] check-deps/clippy/fmt/test 全绿

## Notes

- keyring crate 选型查树（oo7 是 docs/06 提到的 linux 方向——keyring crate 统一接口更简；实现裁定）。
- headless 裁定报告（cli 复用 vs app headless 分支）。
