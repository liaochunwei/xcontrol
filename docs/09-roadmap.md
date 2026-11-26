# 09 · 路线图与任务分解

> 状态：草案 v0.1 · 里程碑周期为估计值（单人投入），冻结于规范 v1.0 后开始计时。

## 总览

```
M0 规范冻结与脚手架 ──► M1 协议与回环 ──► M2 最小可用(mac↔mac) ──► M3 三平台与发现
                                                                    │
                                                                    ▼
                          M6 打磨与发布 0.1 ◄── M5 Wayland 与剪贴板完整版 ◄── M4 映射/热键/托盘
```

## M0 · 规范冻结与脚手架（1 周）

- [x] 三份调研报告结论合入对应规范（平台矩阵 / 协议 / UI）✅（本次已合入，见各文档附录/ADR）
- [x] ADR-0001/0002/0003 落档 ✅；IANA 4243 端口核查完成 ✅（结论：可用，登记关闭，见 [02-protocol.md](02-protocol.md) §0，2026-11-16）
- [x] **托盘 spike（硬门禁，ADR-0001 §决策 3）** ✅：QuitMode::Explicit 与 gpui-tray 直连双验证通过；结论已回写 06-ui §1 与 ADR-0001（2026-11-16）
- [x] workspace 脚手架：全部 crate 空实现 + 三平台 CI 矩阵就绪 ✅（本地全绿；CI 实跑待配置 remote 后验证）
- [x] `xcontrol-config`：配置模型 + TOML 读写 ✅（45 单测，行覆盖 95.08%，2026-11-16）

**出口条件**：`cargo build/test` 三平台绿；托盘 spike 结论落地；规范文档被 PR 引用的编号体系可用。

## M1 · XCP 协议与回环测试（3 周）

- [x] `xcontrol-proto`：帧层（magic/版本/长度/校验）、全部消息编解码、golden 样本 ✅（26 fixtures，行覆盖 98.55%，2026-11-17）
- [x] `xcontrol-core`：会话状态机（握手→运行→断开）、屏幕布局模型、事件路由 ✅（转移表 200 组合验证，ControlDir 门控，覆盖 96.98%）
- [x] `xcontrol-net`：回环传输（in-memory channel）+ 加密层 ✅（Noise_XXpsk0 + PSK 派生链 + 限速器，覆盖 98.76%）
- [x] mock 平台层：虚拟输入设备/虚拟剪贴板，跑通"捕获→协议→注入"全链路 ✅（FR-2.x 12/12 端到端，226 测试）
- [x] criterion 基准：编解码吞吐、回环延迟基线 ✅（回环 P50 ≈ 0.82–1.06µs；decode/key ≈ 4.4ns）

**出口条件**：回环集成测试覆盖 FR-2.x 全部事件类型 ✅；NFR-1 的回环分量 P50 < 5ms ✅（余量 ~1600×）。延后项：fuzz 实跑定时化、llvm-cov 进 CI（M2 首批收尾）。

## M2 · 最小可用（3 周，macOS ↔ macOS）

- [x] `xcontrol-input/macos`：CGEventTap 捕获 + CGEventPost 注入 + 权限检测 ✅（kVK 全表 112 项对 Apple SDK 头文件核验；注入→捕获回环实测，2026-11-17）
- [x] 屏幕边缘检测与跨屏切换（FR-2.1/2.2）✅（core 纯函数 + EdgeSensor 实时接线，归属帧消解/去抖闩锁/ControlDir 门控）
- [x] 文本剪贴板（macOS 侧，FR-4.1）✅（arboard + changeCount 轮询 + 防回环双判据；真实读写冒烟，2026-11-17）
- [x] `xcontrol-app`：最小 gpui 窗口（连接状态 + 日志页）+ `--loopback` ✅（AppState 纯投影 + headless 冒烟 9 事件双向；gpui-kit 入主 workspace）
- [x] 安全码握手实战（FR-1.4）✅（localhost 双实例真实进程 XXpsk0 握手 + 错误码拒绝路径；两台实机待人工）

**出口条件**：两台 Mac 实机：边缘切换、打字、拖拽、文本剪贴板全通；手动清单 mac 部分绿——**代码侧全部就绪，实机验收移交人工**（运行方式：`cargo run -p xcontrol-app -- --host` / `--client <ip>:4243`）。

## M3 · 三平台从机与自动发现（4 周）

- [x] `xcontrol-input/windows`：LL hook 捕获 + SendInput 注入 ✅（scancode 表 130 项 MS Learn 官方核验；windows target 编译级）；`x11`：XRecord 捕获 + XTEST 注入 ✅（evdev 表 145 项内核 v6.6 核验；SSH 远端 XTEST→XRecord 回环实测，keycode 语义勘误先行）
- [x] 剪贴板：Windows（AddClipboardFormatListener + 3×33ms 重试）/ X11（XFixes）✅（EchoFilter 复用 84/0；远端 X11 实测含 xclip 交叉验证）
- [x] 发现：mDNS + UDP beacon + 手动 IP（FR-5.1/5.2）；断线重连（FR-5.3）✅（本机双进程演练全通：mDNS 发现 →2s 会话；退避 1→30s 实测）
- [x] Windows 防火墙/安装器预配置脚本；macOS 自签起步 ✅（codesign 本机实测；docs/10-install.md；release.yml tag 触发）
- [x] NFR-8：tracing 结构化日志 + `--status` ✅（config v2/滚动+清理/REDACTED 层/cli 端到端实测）

**出口条件**：mac 主控 ↔ win/x11 从机、win 主控 ↔ mac 从机组合实测通过；FR-5.x AC 全绿——**代码侧全部就绪**（win 从机实机验收移交 docs/05 §2.1/§2.2 清单；x11 从机已部分实测，grab/多屏移交 §3.1/§3.2）。

## M4 · 映射、热键与托盘（3 周）

- [x] 映射引擎：规则模型 + 平台预设 + 设置编辑器 ✅（两级状态机/8 预设/config v3 四形态；引用计数语义修正 docs/03 §6.2；2026-11-19）
- [x] 全局热键（mac/win/x11）与切换路由 ✅（mac 捕获流识别/win RegisterHotKey/x11 grab×8 变体；XKB autorepeat P1 修复；远端实测）
- [x] 托盘（三平台）+ 状态图标 + 暂停/恢复 + 角色切换 ✅（gpui-tray 直连/菜单 §5.2/角色 supervisor/自启三平台；AX 全链路实测）
- [x] 首次运行向导与权限引导 ✅（四步向导/五设置页/生效时点表/config 原子写；外部 AX 缺口→进程内 harness 替代）

**出口条件**：全程无命令行操作完成配对与使用 ✅（代码侧闭环：向导配对 + 托盘使用；控制转移完成=EnterScreen 选点属 M5）；FR-3.x/6.1/6.3 AC 绿（真人双机配对冒烟移交用户）。

## M5 · Wayland 与剪贴板完整版（4 周）

- [x] Wayland 捕获：InputCapture portal（ashpd）+ X11 后备 ✅（11-19-m5-wayland：portal 四接口 KDE 实测 Available；GNOME 降级探测留移交）
- [x] Wayland 注入：uinput/evdev 路径 + RemoteDesktop portal 路径 ✅（rei EIS fd 通道 + uinput 全码位声明——P1 修复"漏声明=静默失效"）
- [x] Wayland 剪贴板：zwlr_data_control；图片剪贴板全平台（FR-4.2）；HTML（FR-4.3）✅（wl-clipboard-rs 0.9.5 直依赖 mime 直通 + Watcher 监听；arboard image-data mac/win/x11；mac public.html 零 unsafe；mac 本机 + 远端 KDE Wayland 实测 PASS；GNOME ashpd portal 降级探测留移交；2026-11-19）
- [x] 屏幕布局编辑器 UI（FR-1.3）✅（11-19-m5-layout：方位选择制 + 协议 1.1 Layout 协商/Next-Prev 闭环；绝对坐标映射 FR-2.7 留 v0.2 延后池）+ mac warp 语义修订（CGWarp 实测定案）
- [x] GlobalShortcuts portal 热键 ✅（KDE 实测绑定；.desktop 桌面身份为打包前置——docs/05 §4.1）

**出口条件**：GNOME/KDE/sway 三 Wayland 环境主从各组合可用 ✅ 代码侧（KDE 实测 PASS；GNOME/sway 矩阵移交 docs/05 §7 清单）；不可用组合在 UI 中明确降级提示 ✅（CapabilityTable）。

## M6 · 打磨与 0.1 发布（3 周）

- [x] 性能达标验证（NFR-1/2/7）+ 基准报告 ✅（[docs/11-benchmarks.md](11-benchmarks.md)：回环 P50 0.81µs / 空闲 CPU ≤0.2% / 托盘可用 ≈1s；headless 从机 RSS 如实记录 0.1.x 优化项；2026-11-20）
- [x] 白名单/网段限制（FR-5.4）、多主控候选（FR-5.5）、headless（FR-6.4）✅（11-19-m6-security：含钥匙串真接线/换码协议/抓包+fuzz 实测，docs/07 §8 六项全勾）
- [x] 打包：dmg/msi/deb/AppImage ✅（11-19-m6-release：dmg 本机实测 verify PASS / deb+AppImage 远端实测 / msi WiX 脚本交付移交 Windows 实跑；自启 M4 已交付）
- [x] i18n 中文/英文（FR-6.5）；文档站/README 完善 ✅（xcontrol-i18n 文案表 zh-CN/en 全量 + [general].language；CHANGELOG/README/docs/10 收口）
- [x] 安全自查对照 [07-security.md](07-security.md) 清单 ✅（§8 六项核验 11-19-m6-security 全勾）

**出口条件**：三平台安装包发布 GitHub Release（dmg 实测 + deb/AppImage 实测 = 两平台实证 + msi 脚本交付；Release 上传随 `v*` tag push 由 release.yml 承载），手动验收清单全绿（实机项移交人工，docs/05 §7）。

## 延后池（v0.2+）

- **FR-7.3 跨设备文件复制粘贴（已立项，2026-11-26）**：通道拓扑与物化策略已定稿（[ADR-0004](adr/0004-file-transfer-channel-and-materialization.md)）；协议 XCP 1.2 文件通道已入 [02-protocol.md](02-protocol.md) §4.6，剪贴板侧语义入 [04-clipboard.md](04-clipboard.md) §8，平台路径与安全面分别入 [05-platform.md](05-platform.md) §0.2 / [07-security.md](07-security.md) §9。任务树 `.trellis/tasks/11-26-file-transfer/`：规范 → 元数据面 → 通道拉流 → 透明粘贴 → 打磨，五切片独立验收
- **FR-7.1 共享文件夹 / FR-7.2 拖放文件**（对标 Deskflow 延伸方向）：复用 FR-7.3 文件通道（[ADR-0004](adr/0004-file-transfer-channel-and-materialization.md)）；实现路径（内置文件服务 vs FUSE vs 系统共享）开工前另需 ADR
- FR-3.5 按应用映射；FR-4.6 文件路径同步（FR-7.3 落地后作为降级形态保留）；FR-2.8 更细的边界策略
- 多显示器单机多屏布局；HiDPI 混合缩放精化
- 远端电源事件（锁屏/熄屏联动）、睡眠唤醒重连优化
- crate 发布（`xcontrol-proto` 独立发布供第三方客户端实现）
