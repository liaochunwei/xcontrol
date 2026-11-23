# AGENTS.md — AI 辅助开发约定

本仓库的 AI 助手（Claude Code / Codex / ZCode 等）与人类协作者共同遵守：

## Trellis 工作流（本仓库的开发管理方式）

- 任务是唯一工作单元：`.trellis/tasks/<MM-DD-slug>/`（`prd.md` + `task.json` + `implement.jsonl`/`check.jsonl` 上下文清单）。
- 常用命令：
  ```bash
  python3 .trellis/scripts/task.py list                       # 看任务
  python3 .trellis/scripts/task.py start <name>               # 领任务（会话级 current）
  python3 .trellis/scripts/task.py finish                     # 结束当前任务
  python3 .trellis/scripts/task.py archive <name>             # 归档
  ```
  ZCode 会话内优先用 `/trellis:continue`、`/trellis:finish-work` 命令（钩子会自动注入当前任务与 workflow-state）。
- **规划任务时**：用 `add-context` 把相关 spec/docs 挂进 implement/check 清单（空清单不能 start）。
- **spec 与 docs 的关系**：`docs/` 是完整规范（事实源），`.trellis/spec/` 是注入子代理的规则卡。**改 docs 后必须检查对应规则卡是否需要同步**。
- 任务完成后：把经验教训回写 `.trellis/spec/`（新约定/防坑），并记 journal（`.trellis/workspace/<dev>/journal-*.md`）。

## 事实源（source of truth）

1. 需求：`docs/00-requirements.md`（FR/NFR 编号是唯一需求引用方式）
2. 协议：`docs/02-protocol.md`（任何报文变更先改文档再改代码）
3. 平台能力：`docs/05-platform.md`（不得凭记忆断言平台 API 行为，不确定时查证并在文档补录）
4. 架构决策：`docs/adr/`

## 工作规则

- 实现任何功能前，先确认对应的 FR 编号与验收标准；发现规范缺口时，先提文档 PR/修改再写代码。
- 遵守 `docs/08-engineering.md` 的分层红线：`xcontrol-proto` / `xcontrol-core` 零平台依赖、零 I/O。
- 平台相关代码必须 feature 门控（`#[cfg(feature = "...")]`），禁止在公共代码里出现平台判断。
- 提交信息引用 FR/ADR 编号；协议/架构变更的 PR 必须包含文档 diff。
- 不引入新的重量级依赖（> 直接依赖 10 个 crate 或 unsafe 底层库）而不写 ADR。
- 测试：新增协议消息必须同步 golden fixtures；新增 core 状态转移必须覆盖单测。

## 禁止事项

- 禁止 `unsafe` 无 `// SAFETY:` 注释。
- 禁止在库 crate 中 `unwrap()`/`expect()` 处理外部输入。
- 禁止跳过 CI 红灯合入；禁止 force-push `main`。
- 禁止把安全码/密钥写入日志、fixture 或示例配置。
