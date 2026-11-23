# 测试规范

来源：docs/08-engineering.md §4。

## 分层要求

| 层 | 手段 | 门槛 |
| --- | --- | --- |
| 单元（proto/core/config） | `cargo test`，全平台 | 行覆盖 ≥85%（cargo-llvm-cov，CI 报告） |
| 回环集成 | `xcontrol-core --features mock-platform`：进程内 host+client 全流程 | 覆盖 FR-2.x 全部事件类型 |
| 协议一致性 | golden fixtures 比对 + 版本协商矩阵（1.0↔1.1↔2.0） | 变更必过 |
| fuzz | cargo-fuzz：恶意帧/剪贴板流 | 违规输入断开而非崩溃 |
| 性能 | criterion：编解码吞吐、回环延迟 | 回环 P50 < 5ms（NFR-1 分量）；基线与 NFR-1/2/7 判定见 docs/11-benchmarks.md |
| 平台手动 | docs/05-platform.md §7 清单 | 发布门禁 |

## 必测清单（新增代码时自查）

- 新协议消息 → golden fixture + postcard 往返测试
- 新核心状态转移（会话/路由/映射）→ 单测覆盖每个转移分支
- 映射引擎改动 → proptest"修饰键配对性"不变式 + 拦截 flush 分支覆盖
- 剪贴板改动 → 回环三重防线各关闭一项的故障注入测试
- 平台层 → 不设覆盖率指标，靠手动清单；公共逻辑必须下沉到可测的核心层

## 禁止

- 测试里硬编码安全码进断言输出；fixture 永远用假码（`test-code-****`）
- `#[ignore]` 无注释理由
- 跳过的测试不算过（CI 绿 ≠ 手动清单绿）
