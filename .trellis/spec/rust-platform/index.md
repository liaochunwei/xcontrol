# Rust 平台层规范（xcontrol-input / clipboard / tray / app）

> 完整平台矩阵：[docs/05-platform.md](../../../docs/05-platform.md)（API 选型的唯一事实源）；UI/托盘：[docs/06-ui.md](../../../docs/06-ui.md)。

## Pre-Development Checklist

- [ ] 我要用的 API 路径在 docs/05 §0 矩阵中存在且是我平台的推荐项（不在 → 先改 docs/05）
- [ ] 平台差异已收敛为 Caps 位（能力协商），UI 有降级提示路径
- [ ] 回调线程内零慢操作（只转换 + 投递）
- [ ] 新平台 API 用法不确定时查证并把结论回写 docs/05（禁止凭记忆断言平台行为）

## Guidelines Index

| 文件 | 内容 |
| --- | --- |
| [platform-code.md](./platform-code.md) | feature 门控、unsafe 规则、回调线程纪律、各平台 API 速查 |
| [ui-tray.md](./ui-tray.md) | gpui 依赖红线、托盘事件流、AppState 投影模型 |
| [macos-input.md](./macos-input.md) | macOS 输入层事件语义坑（FlagsChanged 设备位/CapsLock/合成事件测试）、kVK 表纪律 |

## Quality Check

- [ ] 新平台模块在其余平台编译路径下不破坏（feature 组合矩阵编译过）
- [ ] 每个 `unsafe` 块有 `// SAFETY:` 不变量注释
- [ ] 权限缺失场景有检测 + 向导引导（docs/05 §6 对照表）
- [ ] UI 变更即时持久化；无"保存"按钮反模式
