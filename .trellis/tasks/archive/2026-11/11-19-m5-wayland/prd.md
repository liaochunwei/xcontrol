# M5: Wayland 捕获与注入

## Goal

FR-2.x 的 Wayland 路径——InputCapture portal 捕获 + uinput 注入 + GlobalShortcuts portal 热键 + 边缘手势回退。SSH 远端 KDE（Plasma Wayland）可部分实测；portal 授权对话框需用户在场（移交项明确化）。

## Requirements

1. **捕获**：`ashpd` InputCapture portal（freeze/cursor 语义）——cursor 位置流 → proto InputEvent 转换（ Wayland pointer 事件 → MoveRel/Key）；portal 不可用降级路径；**C-2 支持矩阵**（KDE/GNOME/wlroots）探测函数（每 compositor 能力检测——实现为运行时探测 + 结果表）。
2. **注入**：uinput/evdev 路径（/dev/uinput 设备创建——权限检测 + 组成员提示；rei 或手写 uinput ioctl——用 `rei` crate（docs/01 §6 基线））；RemoteDesktop portal 备选路径（ashpd——需要交互授权，实现 + 移交）。
3. **热键**：GlobalShortcuts portal（ashpd GlobalShortcuts）——注册四热键；KDE 支持实测；**边缘手势回退**（docs/06 §2：边缘双击/角落悬停——设置开关）。
4. **实测边界诚实化**：portal 授权对话框 = 用户在场交互——SSH 可实测：portal 可用性探测/uinput 权限检测/手势纯逻辑；**授权后全链路**移交用户在场。
5. **KDE 实测**：SSH 远端 WAYLAND_DISPLAY 下——uinput 节点权限探测、portal service 探测（org.freedesktop.impl.portal 进程/DBus）、GlobalShortcuts KDE 支持查询。

## Acceptance Criteria

- [ ] linux target 编译 + clippy 绿（ashpd/rei/wl-clipboard-rs 白名单）
- [ ] SSH 远端：环境探测报告（uinput 权限/portal 服务/GlobalShortcuts KDE 版本支持）
- [ ] uinput 注入器：/dev/uinput 可写时本地注入冒烟（远端权限允许时实测；否则移交清单）
- [ ] 边缘手势纯逻辑测试（双击判定/角落悬停窗口）
- [ ] 能力探测表（C-2 三 compositor）实现 + KDE 实测行回填
- [ ] check-deps/clippy/fmt/test 全绿；移交清单（授权后全链路/GNOME/wlroots 实测）

## Notes

- ashpd token/授权流程在 KDE 的自动化程度实测为准——能脚本化（无对话框）就实测，不能就探测 + 移交。
- rei crate 版本与 uinput API 以 docs.rs 为准。
