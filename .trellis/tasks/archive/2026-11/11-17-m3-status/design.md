# design: 结构化日志与 --status

```
xcontrol-app/src/logging.rs   # 扩展：rolling file layer + EnvFilter + REDACTED Visit
xcontrol-cli/src/main.rs      # --status / --json
```

- **REDACTED Visit**：`tracing_subscriber::layer::Layer` 实现，包在内层 fmt 之前——遍历 fields，字段名命中黑名单（code/secret/password/token/passphrase，大小写不敏感）→ 值替换；仅拦截 struct field（message 内嵌脱敏靠纪律+审计）。
- **滚动清理**：tracing-appender daily 无保留策略——自写清理任务（启动时 + 每日：删 data_dir/logs/ 下 N 天前文件，N=14 常量 + config 可选覆盖）；文件名前缀 `xcontrol.log.` 约定。
- **--status**：无 app 进程时输出 config 摘要 + "未检测到运行中的 XControl"；有进程时 + 端口探测结果 + 日志尾部 20 行。`--json`：serde_json（已在 config 传递树？config 用 toml——json 需新增 serde_json 双登记）或手写紧凑 JSON（避免新依赖）——实现时定，倾向 serde_json（配置机器可读性，量小）。
- **命名审计**：写脚本进 xtask？——轻量：`grep -rn "tracing::" crates/*/src | grep target:` 输出表人工核对后入 spec 卡（自动化留 M6 打磨）。
