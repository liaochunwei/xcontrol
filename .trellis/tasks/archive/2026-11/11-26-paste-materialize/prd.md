# 切片c：粘贴拦截物化（透明跨设备粘贴）

> 父任务：11-26-file-transfer；依赖切片 b（通道拉流）。本切片达成「像本地一样」的最终体验。

## Goal

注入路径拦截粘贴键：pending 槽有货且未失效时，先拉流物化挂板，再注入粘贴键——
用户全程 Cmd+C / Cmd+V，无托盘动作。

## Requirements（2026-11-26 开工定稿）

- R1 拦截点 = 从机注入管线的 sink 包装器（`HotkeyFilterSink → MappingSink → 平台 sink` 链内侧再加一层 `PasteInterceptSink`）——KVM 注入路径天然钩子，零系统级 hook；包装器自带修饰键跟踪（wire Key 无 mods，Meta/Ctrl 上下由包装器自维护快照），粘贴组合 = V↓ 时快照含 META|CTRL（mac Cmd+V / win·linux Ctrl+V 同判）。
- R2 触发条件：pending 槽有货 + 协商 minor ≥ 2 + 非物化中；命中 → 粘贴键**挂起不注入**，整单入拉取队列（复用切片 b 管线），物化完成（挂板成功）后补注入挂起的 V↓（此后 V↑ 与修饰键 ↑ 照常透传）。
- R3 物化失败/超时（120s）→ 挂起的键**丢弃不注入** + bus 提示（不得把挂起的键注入成错误行为）；物化中重复粘贴组合不重复触发（单飞语义）。
- R4 物化完成 → pending 槽消费清空（托盘「接收文件」随之消失，一次性语义）+ 挂板回环防线登记（切片 b 既有）。
- R5 防回环：切片 b 挂板防线复用（指纹 + 2s 窗口），无新增面。

## Acceptance Criteria

- [x] AC1 单测（sink 包装器）：Meta↓ 透传；V↓+Meta+槽有货 → 挂起并发出物化请求（注入日志无 V↓）；Ctrl+V 同判；无槽/无修饰键 → 全透传；物化中不重复触发。
- [x] AC2 会话接线：客户端注入路径挂包装器；物化完成经通道回注挂起键（async worker → select 臂）；超时/失败丢弃 + bus 提示。
- [x] AC3 一次性语义：挂板后 pending 清空，随后粘贴行为与无文件槽完全一致（回归保护）；挂板回环防线登记（指纹 + 2s 窗口）。
- [x] AC4 质量门全绿（clippy 双 feature、fmt、workspace 与 macos feature 测试）。
- [ ] AC5 双机实测：A 复制文件 → 控制着 B 直接 Cmd+V → 文件落地（移交人工；依赖 A/B 双机与目标应用行为）。

## Out of Scope

- 本机手动粘贴的拦截（系统级 hook 面——不做，切片 b 的挂板+手动粘贴已覆盖本机场景）。
- 进度条 UI（传输时长可见性走托盘 tooltip 文案，进度条属切片 d）。
- 物化期间的取消入口（超时兜底已覆盖；主动取消属切片 d）。

## Out of Scope

- OS 原生惰性粘贴（mac file promise / win FILEDESCRIPTOR，P2 增强）；目录递归（切片 d）。
