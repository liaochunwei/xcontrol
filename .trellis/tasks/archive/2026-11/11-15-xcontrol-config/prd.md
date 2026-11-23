# xcontrol-config：配置模型与 TOML 读写

## Goal

实现 docs/01-architecture.md §5 的配置模型——一切功能的先行依赖（M0 项）。

## Requirements

1. 结构与 docs/01 §5 示例一致：`role` / `security_code_ref` / `[net]`(port, cidrs, discovery) / `[layout]` / `[keymap]`(preset, rules) / `[hotkeys]` / `[clipboard]`(scope, max_bytes, formats)。
2. serde + toml 读写；顶层 `version` 字段 + 迁移函数链（v1 起步，预留旧版 fixture 升级测试）。
3. 校验：端口 1–65535、CIDR 合法、hotkey 组合格式、keymap 规则数 ≤256、max_bytes 上限；错误信息面向用户可读（中文，含字段路径）。
4. `SecretStore` trait（keychain / file 两实现）：file 实现强制 0600 权限；keyring 真实现可延后占位。
5. 配置路径按平台标准目录（dirs crate），支持 `--config` 覆盖。

## Acceptance Criteria

- [ ] round-trip 单测：解析→序列化→再解析等价
- [ ] 迁移单测：旧版本 fixture 升级正确
- [ ] 每个校验字段至少一个负例，错误信息含字段路径与中文提示
- [ ] 行覆盖 ≥85%（rust-core spec `testing.md` 门槛）

## Notes

- 安全码禁止明文进日志/错误信息（rust-core spec `error-handling.md` 红线）。
- 热加载（ConfigReload）延后到核心任务，本任务只做静态读写。
