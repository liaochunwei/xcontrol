# ADR-0001：gpui 依赖方式与系统托盘集成策略

- 状态：已接受
- 日期：2026-11-15
- 关联需求：C-1、FR-6.1、FR-6.4、NFR-4

## 背景

用户指定 UI 用 gpui（Zed 的 UI 框架）。产品形态是"托盘常驻 + 按需弹窗"，而 gpui 官方定位是编辑器 UI，托盘/后台常驻属于社区强需求但官方明确不收（PR #13098、#44047 先后被关闭；discussion #40318 确认默认行为是"最后一个窗口关闭即退出"）。2026-11 调研（来源见文末）确认：

- gpui 三平台成熟（Windows 2025-10 稳定；Linux 原生 Wayland 后端已是默认路径），license **Apache-2.0**（非 GPL）。
- 生态版本碎片化：crates.io `gpui 0.2.2`（滞后）/ `gpui-pre` 快照（当前 0.3.12）/ git rev / 社区 `gpui-ce`，四条线类型不互通。
- `gpui-kit` 0.9.1（原 longbridge gpui-component，20.5k stars，活跃）锁定 gpui-pre 快照，组件齐备（Settings/Form/List 等）。
- `QuitMode::Explicit`（关全部窗口进程存活）已合并入 gpui 主线，**但是否包含于 gpui-pre 0.3.12 未确认**——这是托盘形态的硬前提。
- 托盘先例：Ropy（gpui-kit 0.9 + tray-icon 0.26，Linux 用 ksni 后端，channel 转发进 GPUI executor）已验证可行；社区 `gpui-tray` 0.3 复用 gpui 事件循环但很年轻（macOS 有构建问题，其 gpui-kit feature 停在 0.6 与 0.9.1 对齐未验证）。

## 备选方案

1. **官方 gpui crate（0.2.2）+ tray-icon/ksni 自集成**：crates 版滞后，QuitMode 可能缺失；需自追 git。
2. **git rev 依赖 zed 仓库**：永远最新但不可复现（rev 变更即全量重编），CI 依赖 GitHub 可达性。
3. **gpui-ce（社区 fork）**：独立演进风险，另立生态。
4. **gpui-kit → gpui-pre 快照线**：版本可复现、组件库一站、Longbridge 生产验证。
5. （托盘子决策）gpui-tray vs Ropy 模式自集成。
6. （备胎，非本 ADR 决策）若托盘 spike 两周内全败：换 iced/Tauri —— 仅记录，不采用。

## 决策

1. **依赖只走 gpui-kit 一条线**：`gpui-kit = "0.9"`，gpui API 一律经其 re-export 使用，不显式依赖 gpui/gpui-pre/git。升级 gpui-kit 时整条线同步升级。
2. **托盘自建 `xcontrol-tray` crate（Ropy 模式）**：macOS/Windows 用 `tray-icon`，Linux 用 `ksni`（纯 D-Bus SNI，无 GTK）；菜单为各平台原生构建，事件归一为 `TrayCommand` 枚举经 channel 送 GPUI executor。Linux 禁止任何 GTK 主循环依赖（与 gpui 自建循环冲突）。
3. **M0 托盘 spike（硬门禁）**：① 验证 gpui-pre 0.3.12 是否含 `QuitMode::Explicit`（无则升级 gpui-kit 或临时 git patch + 重新评估）；② 同时试验 `gpui-tray` 的 gpui-kit feature 能否与 0.9.x 共存——若可用则允许替换自建实现（减少维护面），spike 结果回写 [06-ui.md](../06-ui.md) §1。
4. macOS 用 `on_reopen()` 重开设置窗；win/linux 单实例锁 + 本地 socket 唤起；Wayland 不承诺托盘点击抢焦点（xdg-activation 由 compositor 决定，best-effort）。

## 后果

- ✅ Apache-2.0 全链路；版本可复现；组件库免自研表单/列表。
- ⚠️ 接受 gpui-kit 的运行基线（macOS 15+ / Win10+ / Ubuntu 24.04 验证）——已同步放宽 NFR-4（headless 从机不受此限）。
- ⚠️ 托盘为本项目唯一自维护的平台胶水模块（三平台各 ~中等工作量）；gpui-tray 若成熟可择机替换。（2026-11-16 更新：spike 后已改为 gpui-tray 直连，自建方案归档为备胎，见文末更新记录）
- ⚠️ gpui 系 crate 需 `opt-level=3`（debug 卡顿）；Linux 构建需 Vulkan 驱动与一组系统库；MSRV 1.92。（2026-11-16 更新：实测 gpui-pre 0.3.12 在 1.92/1.95 编译失败、stable 1.99.1 通过，实际 MSRV 高于宣称值，见文末更新记录）
- 跟踪项：~~spike 两项结论（M0 出口条件）~~（已完成，两项全过，见文末更新记录）；gpui-tray/gpui-kit 版本对齐状态（每季度复查）；~~工具链升级落地~~（✅ 2026-11-16 完成：rust-toolchain.toml/CI → 1.99，docs/01 §6、docs/08 §2、docs/06 附录、clippy.toml msrv、根 Cargo.toml rust-version 同步修订）；Linux/Wayland 托盘实测（本机无 Linux 桌面，spike 未覆盖）；`on_reopen` Dock 点击触发人工确认。

## 更新（2026-11-16）：M0 托盘 spike 结论（两项全过，M0 出口条件满足）

实测环境：macOS（darwin 25.5.0，arm64）。spike 代码 `docs/spike-tray/`（standalone crate，脱离主 workspace，不进 CI 矩阵与 check-deps 闸门）；完整证据与命令输出见 `docs/spike-tray/EVIDENCE.md`。

| Spike | 结论 | 证据要点 |
| --- | --- | --- |
| ① QuitMode::Explicit（决策 3-①） | **可用** ✔ | gpui-pre 0.3.12 含 `QuitMode::Explicit` / `with_quit_mode`（app.rs:526/291）与 `on_reopen`（app.rs:369）；macOS 实测：程序化关闭全部窗口后进程持续存活（8s 心跳无中断，日志 EVIDENCE §A），退出干净无残留。`on_reopen` 的 Dock 点击触发为待人工项。注意：该快照**无 `Application::new()`**，入口为 `gpui_kit::application()`；另发现 `Application::headless()`（FR-6.4 可直接依赖） |
| ② gpui-tray 共存（决策 3-②） | **可用** ✔ → **选定 gpui-tray 直连** | gpui-tray 0.3.0（`gpui-kit` feature → gpui-pre ^0.3.4）与 gpui-kit 0.9.1（`gpui-pre =0.3.12`）被 cargo 统一到**单一 0.3.12 版本源**（类型共享，cargo tree 见 EVIDENCE §B）；托盘图标出现（System Events 可达性查询证据）、菜单点击事件回传 GPUI executor（osascript 真实点击 ×3 全部到达日志）、勾选态菜单随状态刷新 ✔ |

失败细节与新增约束：

- **工具链（本 spike 最重要的意外发现）**：gpui-pre 0.3.12 在 1.92 编译失败（`gpui-pre-util` 使用未稳定的 `slice_as_array`，E0658），1.95 亦失败（`gpui-pre-macos` 使用未稳定的 `result_option_map_or_default`），stable 1.99.1 全树通过；降级备选 gpui-kit 0.9.0（gpui-pre 0.3.11）在 1.92 同样失败。**决策 1 的"gpui-kit 一条线"维持不变，但工具链基线必须升级**：rust-toolchain.toml（1.92 锁定）与 docs/01 §6 "MSRV 1.92（gpui-kit 0.9.1 要求）"的断言与实测不符，需主会话修订（不在 spike 任务权限内，列为最高优先跟踪项；✅ 已于同日由主会话完成——rust-toolchain/CI/clippy msrv/根 rust-version 与相关 docs 已同步修订为 1.99，见上文跟踪项勾销记录）。
- **决策 2 修订**：托盘不再自建平台胶水（tray-icon/ksni + `TrayCommand` channel 的"Ropy 模式"归档为备胎）；改用 **gpui-tray 直连**——菜单即原生 gpui `MenuItem`，事件以 GPUI action 形式分发且天然落在 GPUI 主线程，等价满足原"托盘回调线程不得触碰核心状态"约束；`xcontrol-tray` crate 收敛为核心状态快照 → 图标/菜单投影的薄适配层（不再含平台胶水，仍承担 docs/06 §5.1 图标状态机）。
- gpui-tray 的 Linux 后端为 zbus SNI 实现（无 GTK 依赖），本 ADR 的 Linux 红线保持满足；**Linux/Wayland 侧为纯编译结论外推，未实测**（spike 本机为 macOS），列为后续 Linux 实测跟踪项。
- 复查项补充：gpui-tray 0.3 尚年轻，升级 gpui-kit 时须同步验证其 `gpui-kit` feature 的 gpui-pre 版本要求仍可统一（本次统一机制见 EVIDENCE §B）。

## 来源（节选）

- https://gpui-kit.com/docs/installation （依赖与 re-export 说明、平台基线、MSRV 1.92）
- https://github.com/zed-industries/zed/discussions/40318 （后台运行限制与关闭窗口即退出）
- https://github.com/zed-industries/zed/pull/41436 （QuitMode::Explicit 合并）
- https://github.com/zed-industries/zed/pull/44047 、https://github.com/zed-industries/zed/pull/13098 （官方拒绝托盘 PR）
- https://github.com/longbridge/gpui-kit/issues/3843 （托盘需求 + Ropy 技术栈披露）
- https://github.com/StudentWeis/Ropy （gpui+托盘先例应用）
- https://github.com/domenkozar/gpui-tray
- https://raw.githubusercontent.com/zed-industries/zed/main/crates/gpui/Cargo.toml （license=Apache-2.0）
