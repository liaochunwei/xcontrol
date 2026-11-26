# ADR 索引（Architecture Decision Records）

格式：`NNNN-slug.md`，内容含 **背景 / 备选方案 / 决策 / 后果**。一旦接受不再改写历史 ADR——状态变更（如被取代）在原文件头部标注 `Superseded by ADR-XXXX`。

| # | 标题 | 状态 |
| --- | --- | --- |
| [0001](0001-gpui-and-tray.md) | gpui 依赖方式与系统托盘集成策略 | 已接受（2026-11-15） |
| [0002](0002-transport-and-discovery.md) | 传输层、加密与发现机制选型 | 已接受（2026-11-15） |
| [0003](0003-platform-input-paths.md) | 各平台输入捕获/注入/剪贴板实现路径 | 已接受（2026-11-15） |
| [0004](0004-file-transfer-channel-and-materialization.md) | 跨设备文件传输的通道拓扑与物化策略（FR-7.3） | 已接受（2026-11-26） |

## 模板

```markdown
# ADR-NNNN：<标题>

- 状态：<草案 | 已接受 | 已废弃 | Superseded by ADR-XXXX>
- 日期：YYYY-MM-DD
- 关联需求：FR-x / NFR-x

## 背景
（为什么现在要做这个决策；约束与调研事实，附来源）

## 备选方案
（列出认真考虑过的选项与放弃原因）

## 决策
（选定方案与落地要点）

## 后果
（正面收益、已知代价、需跟踪的验证项）
```
