# M3: 结构化日志与 --status

## Goal

NFR-8 落地：tracing 滚动文件 + 日志级别配置 + 脱敏纪律机械化 + `xcontrol-cli --status`。

## Requirements

1. **滚动文件**（docs/08 §2"tracing-subscriber fmt + 滚动文件"）：`tracing-appender`（daily rolling）写入 data_dir/logs/（config crate 的 `default_data_dir()`）；保留策略（保留 N 天，清理旧文件——appender 自带 rotation，清理逻辑简单实现）；stdout fmt 层并存（`--status`/前台看 stdout，后台文件全量）。
2. **级别配置**：config `[general] log_level`（error/warn/info/debug/trace，默认 info）+ `--log-level` 覆盖；子系统过滤示例（`net::=debug` env 风格——EnvFilter）。
3. **脱敏纪律机械化**：现有纪律是"手写不打印"——加一层保险：tracing `Visit` 层对字段名含 code/secret/password 的字段值替换 `[REDACTED]`（防御纵深，防未来手滑）；配套测试。
4. **`xcontrol-cli --status`**：本地只读状态——读 config（`xcontrol-config`）+ 日志尾部（最后 N 行）+ 进程探测（本机是否已有 xcontrol-app 运行——pid 文件或端口探测）→ 结构化输出（人类可读 + `--json`）；**不联网**（纯本地查询，FR-6.4 的 CLI 对应物）。
5. **mac 子系统名一致性收尾**：audit 全仓 tracing target 命名（app::/session::/net::/input::/clipboard::）统一，建立命名表入 spec。

## Acceptance Criteria

- [ ] 滚动文件写入 + 老文件清理测试（TempDir 模拟跨日）
- [ ] 级别/EnvFilter 配置生效测试
- [ ] REDACTED Visit 层测试（敏感字段名全替换）
- [ ] `xcontrol-cli --status` 输出 config 摘要 + 日志尾部（本机实测）；`--json` 合法 JSON
- [ ] tracing target 命名审计表入 spec（rust-core 或 guides）
- [ ] check-deps/clippy/fmt/test 全绿；cli bin 冒烟

## Notes

- tracing-appender/tracing-subscriber(env-filter) 双登记；cli 顶层 anyhow。
- pid 文件 vs 端口探测：选端口探测（TCP connect 4243 + 握手前缀判断？——只判端口占用即可，伪造服务误报可接受，注释说明）。
