# IANA 4243 端口占用核查

## Goal

docs/02-protocol.md §0 遗留待办：确认 TCP/UDP 4243 可作为 XControl 默认端口。

## Requirements

1. 查 IANA Service Name and Transport Protocol Port Number Registry：4243/tcp、4243/udp 注册状态。
2. 查常见软件端口惯例冲突（局域网工具/游戏/数据库等）。
3. 结论回写 docs/02 §0 决策表（附来源 URL 与核查日期）；若冲突 → 提出新端口并在 docs/02、docs/01 §5 及配置默认值中全文替换。

## Acceptance Criteria

- [ ] docs/02 §0 新增"IANA 核查"行：结论 + 来源 URL + 日期
- [ ] 无冲突：登记后关闭；有冲突：完成替换并注明变更原因

## Notes

- 轻量文档任务；若要正式向 IANA 注册端口，另开任务（非 M0 必须）。
