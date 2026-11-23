# design: 设置编辑器与首次运行向导

```
app/src/ui/
├── mod.rs         # 窗口骨架 + 页面路由（Sidebar 或顶部 Tab——gpui-kit 组件裁定）+ 向导窗口分流
├── wizard.rs      # 四步向导流（状态机 + 每步校验 + 完成写 config）
├── status.rs      # 已有，补从机列表
├── mapping.rs     # 预设下拉/直通开关/规则表（增删简化）
├── hotkeys.rs     # 四键 + 组合键文本输入
├── clipboard.rs   # 范围/上限/格式
├── general.rs     # 语言/主题/日志级别/自启
└── about.rs       # 版本/许可/诊断
```

- **组件库**：gpui-kit default features 开启（Form/Input/Switch/Sidebar/List）——spike 当时 default-features=false；版本源一致性红线（同 gpui-pre 0.3.12，开启 feature 不换源）。
- **向导状态机**：`WizardStep { Role, Code, Network, Permissions, Done }`——完成时构建 Config（role/security_code_ref=file 回退或 keychain（file 0600 先行——keychain 接线 M5）/net 默认）→ save → 触发运行时。`security_code` 存 SecretStore（FileSecretStore 0600）。
- **配置生效框架（最小钩子）**：设置页改动 → `config.save()` → `RuntimeConfigUpdate` 命令（CoreHandle 新枚举项携带新 Config）→ supervisor 分发（日志级别=立即 EnvFilter 重建；直通/映射/热键=会话重启生效标注；自启=立即 Autostart 调用）。**不做文件监听**；文档化各字段生效时点表。
- **单实例**：最简裁定 = 启动时端口探测（--status 同款）+ mac on_reopen 已有；完整 lock+IPC 留 M5/M6（报告实际交付）。
- **AX**：向导每步一个 AX 容器（title/label 稳定），osascript 按 description 定位（spike 手法扩展）。
