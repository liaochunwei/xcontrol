# Rust Core 规范（xcontrol-proto / core / net / config）

> 完整规范：[docs/08-engineering.md](../../../docs/08-engineering.md)、[docs/01-architecture.md](../../../docs/01-architecture.md)。本目录是会话注入用的精简规则卡；与 docs/ 冲突时以 docs/ 为准并回改此处。

## Pre-Development Checklist（写代码前逐条确认）

- [ ] 我知道这个变更对应的 FR/NFR/ADR 编号（需求追踪，docs/00 §8）
- [ ] 涉及协议报文？先改 [docs/02-protocol.md](../../../docs/02-protocol.md) 再写代码（协议即契约）
- [ ] 我的代码在允许的分层内（见 architecture-rules.md 红线）
- [ ] 新增依赖是否重量级（>10 直接依赖或底层 unsafe 库）？是 → 先写 ADR
- [ ] 依赖的工具链/平台断言（MSRV、API 可用性）写入 docs 基线前必须实测编译验证（先例：调研报告"MSRV 1.92"误判，M0 托盘 spike 实测 gpui-pre 0.3.12 需 1.99，ADR-0001）

## Guidelines Index

| 文件 | 内容 |
| --- | --- |
| [architecture-rules.md](./architecture-rules.md) | 分层红线、crate 依赖方向、feature 门控、async 边界 |
| [error-handling.md](./error-handling.md) | thiserror/anyhow 边界、unwrap 禁令、错误传播 |
| [protocol-rules.md](./protocol-rules.md) | XCP wire format 规则、golden fixtures、版本兼容承诺 |
| [testing.md](./testing.md) | 单测/回环/golden/fuzz/基准要求与覆盖率门槛 |
| [logging-targets.md](./logging-targets.md) | tracing 子系统命名审计表、脱敏纪律（NFR-8） |

## Quality Check（提交前逐条确认）

- [ ] `cargo fmt` + `clippy`（含 unwrap_used 检查）零警告
- [ ] 新协议消息有 golden fixture；新状态机转移有单测
- [ ] `xcontrol-proto` / `xcontrol-core` 无平台 API、无 I/O（红线可机械检查）
- [ ] 日志走 tracing 子系统命名规范；无安全码/密钥入日志
