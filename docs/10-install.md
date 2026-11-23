# 10 · 安装与运行前配置指南

> 状态：草案 v0.1 · M3 打包预配置与自签交付（任务 11-17-m3-packaging；[09-roadmap.md](09-roadmap.md) M3"Windows 防火墙/安装器预配置；macOS 打包签名"行）；M6 安装包脚本交付（dmg 实测 / deb+AppImage 远端实测 / msi 移交——任务 11-19-m6-release，见 §1.1/§2.1/§3.1）。
> 权限向导的 UI 侧实现依据见 [05-platform.md](05-platform.md) §6（FR-6.3）；本文是安装侧的操作指引。

## 0. 前置条件总览

| 平台 | 系统要求（NFR-4） | 网络 | 权限 | 防火墙 |
| --- | --- | --- | --- | --- |
| macOS | macOS 15+（gpui UI 完整体验；headless 放宽至 13+） | 局域网 | 辅助功能（主/从都要，§1.2） | 应用防火墙 + 签名（§1.3） |
| Windows | Windows 10 22H2+ | 局域网，**网卡须为"专用网络"**（§2.2） | 无强制（UIPI 提权可选，§2.5） | 入站规则 TCP+UDP 4243 Private（§2.3） |
| Linux | 主流发行版 | 局域网 | $DISPLAY 可达（X11，§3.2） | 系统防火墙放行 4243（§3.3，如启用） |

- 端口：TCP+UDP **4243**（默认，可配置；IANA 核查与冲突面见 [02-protocol.md](02-protocol.md) §0——改配置端口时防火墙规则须同步调整）。
- 主控机/从机术语见 README；发现/连接机制见 [02-protocol.md](02-protocol.md) §2。
- 三平台验证均以 `xcontrol-cli --status` 收口（§5；NFR-8，任务 11-17-m3-status 交付）。
- Rust 工具链要求（MSRV 1.99）见 [08-engineering.md](08-engineering.md) §2；`rust-toolchain.toml` 已锁定。

## 1. macOS

### 1.1 安装（从源码构建 / 安装包）

从源码构建：

```bash
cargo build --release -p xcontrol-app --features macos   # UI / 托盘入口（平台层随 feature 进树）
cargo build --release -p xcontrol-cli                    # headless / 诊断
```

产物路径 `target/release/xcontrol-app`、`target/release/xcontrol-cli`。平台 feature
是捕获/注入/剪贴板后端的进树开关（`macos`/`windows`/`x11`/`wayland`，默认全关——
docs/08 §3 "单平台发布构建由打包脚本固定 `--features`" 的同款组合；漏传时进程可启动
但无平台后端，输入转发不可用）。

**安装包（M6 交付，11-19-m6-release）**：`scripts/macos/dmg.sh` 构建 .app bundle + DMG
（bundle 组装 + ad-hoc 签名 `--deep` + `hdiutil create` UDZO + `hdiutil verify`）：

```bash
scripts/macos/dmg.sh          # 产物 target/pkg/macos/XControl-<版本>.dmg（拖拽安装：含 /Applications 链接）
```

TCC 授权注意事项同样适用：**固定路径安装**——.app 移动会破坏已授予的权限
（[05-platform.md](05-platform.md) §1 打包行）。正式签名（Developer ID + 公证）属
CI 发布流程移交项（[05-platform.md](05-platform.md) §1）。

### 1.2 权限引导：辅助功能

- 主控与从机角色都需要**辅助功能**授权（[05-platform.md](05-platform.md) §0 矩阵、§6 对照表）。
- 授权路径：系统设置 > 隐私与安全性 > 辅助功能 → 将 xcontrol-app 加入并开启。
- 向导深链（FR-6.3）：运行时以 `AXIsProcessTrustedWithOptions(kAXTrustedCheckOptionPrompt)` 检测并跳转系统设置（[05-platform.md](05-platform.md) §1）。
- 未授权的表现：事件 tap 回调被系统**静默忽略**（不报错）——表现为捕获无事件、注入无效，务必先授权（[05-platform.md](05-platform.md) §1）。

### 1.3 签名与防火墙

- **必须签名（ad-hoc 起步）**：未签名的二进制在 macOS 应用防火墙下会被反复拦截（[02-protocol.md](02-protocol.md) §2.3、[05-platform.md](05-platform.md) §1"正式/Ad-hoc 签名，否则应用防火墙反复拦截"）。
- 本仓库交付 `scripts/macos/codesign.sh`：构建 release 产物并对 `target/release/xcontrol-app` 做 ad-hoc 签名（`codesign --force -s -`），并输出 `codesign -dv --verbose=2` 验证信息（`Signature=adhoc` 即成功）。重复运行无害（`--force` 覆盖签名）：

  ```bash
  scripts/macos/codesign.sh          # 默认 release；debug 传 debug
  ```

- 同一命令适用于 `xcontrol-cli`（其同样可作为监听端），见脚本头注。
- 首次监听若仍弹出授权框，选择"允许"；签名后系统按签名身份记忆授权，不再反复询问。
- 系统防火墙开启"**阻止所有传入连接**"时，mDNS/UDP 发现均不可用（[02-protocol.md](02-protocol.md) §2.3）——排查发现问题时先核对该开关；UI 诊断提示以此为依据。
- 正式签名（Developer ID + 公证）属 M6 打包（[05-platform.md](05-platform.md) §1、[09-roadmap.md](09-roadmap.md) M6）。

### 1.4 验证

见 §5。

### 1.5 已知限制

- 平台能力矩阵与实现路径：[05-platform.md](05-platform.md) §1；发布门禁手动清单：[05-platform.md](05-platform.md) §7。
- macOS 输入/剪贴板层已在本机完成实测（无 §2.1/§3.1 型运行时移交清单）；实测配方与结果见各平台模块文档。

## 2. Windows

### 2.1 安装（从源码构建 / 安装包）

从源码构建：

```powershell
cargo build --release -p xcontrol-app --features windows   # UI / 托盘入口（平台层随 feature 进树）
cargo build --release -p xcontrol-cli                      # headless / 诊断
```

（需 MSVC 工具链；工具链锁定见 `rust-toolchain.toml`。平台 feature 说明同 §1.1——漏传时无平台后端。）产物路径 `target\release\xcontrol-app.exe`、`target\release\xcontrol-cli.exe`。

**MSI 安装包（M6 脚本交付，11-19-m6-release——win 无本机构建环境，移交 Windows 侧实跑）**：
`scripts/windows/msi/`（`xcontrol.wxs` + `build.ps1`）：安装二进制至 Program Files + 开始菜单
快捷方式，并以提升权限 custom action 调用 `firewall.ps1` 做 §2.3 预配置（卸载以 `-Uninstall`
对称清理，幂等语义保留，§2.4 蓝本兑现）：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\msi\build.ps1   # 前置 WiX v3（candle/light）
```

### 2.2 网络位置：专用网络（必须）

- 防火墙规则按 **Private profile** 下发（[02-protocol.md](02-protocol.md) §2.3）；网卡的网络位置必须为"**专用网络**"，否则规则不生效，发现与连接均不可用。
- 检查/切换：设置 > 网络和 Internet > 以太网（或 WLAN）> 网络配置文件类型 → **专用网络**。

### 2.3 防火墙预配置（首监听弹窗误拒的预防）

Windows 在应用首次监听端口时弹授权框，用户误点"取消"会静默生成阻止规则且难以排查——安装器预授权是 [02-protocol.md](02-protocol.md) §2.3 的明确要求（[05-platform.md](05-platform.md) §2 打包行、§6"win 防火墙"行）。

- 脚本：`scripts/windows/firewall.ps1`（管理员 PowerShell 运行；幂等——已存在同名规则则跳过）：

  ```powershell
  powershell -ExecutionPolicy Bypass -File scripts\windows\firewall.ps1
  ```

- 行为：入站 allow **TCP 4243 + UDP 4243**，`profile=private`。
- 不用脚本时的等效手动命令（管理员提示符）：

  ```bat
  netsh advfirewall firewall add rule name="XControl (TCP 4243)" dir=in action=allow protocol=TCP localport=4243 profile=private
  netsh advfirewall firewall add rule name="XControl (UDP 4243)" dir=in action=allow protocol=UDP localport=4243 profile=private
  ```

- 规则核对：`Get-NetFirewallRule -DisplayName "XControl*" | Format-Table DisplayName,Enabled,Direction,Profile`。

### 2.4 MSI 集成说明（M6 安装器实现时的命令级蓝本）

- 安装器（WiX 等）以**提升权限**执行预配置：custom action 调用本脚本或直接下发 §2.3 的等效 netsh 命令。
- 幂等语义必须在 custom action 中保留：先 `Get-NetFirewallRule -DisplayName` 查重再 add（netsh add 对同名规则不查重、会叠加）。
- 卸载对称清理：

  ```bat
  netsh advfirewall firewall delete rule name="XControl (TCP 4243)"
  netsh advfirewall firewall delete rule name="XControl (UDP 4243)"
  ```

- 可选收紧（M6 评估项）：以 `program="<安装路径>\xcontrol-app.exe"` 把规则从端口级收窄为程序级——端口级更通用（默认按 [02-protocol.md](02-protocol.md) §2.3 端口级交付），程序级更收敛。

### 2.5 权限

- 运行本身无强制权限（[05-platform.md](05-platform.md) §0 矩阵）。
- UIPI（可选）：注入提权前台进程（任务管理器等）需本进程提权；`SendInput` 对高完整性窗口静默失败——UI 提供"以管理员运行"可选项并说明（[05-platform.md](05-platform.md) §2 注入行、§2.1 item 3、§6 对照表）。

### 2.6 验证 / 已知限制

- 验证见 §5。
- 运行时行为移交清单（发布门禁——本机无 Windows，须在有 Windows 的机器上逐项实测并回填 [05-platform.md](05-platform.md)）：§2.1（输入层）、§2.2（剪贴板层）。

## 3. Linux

### 3.1 安装（从源码构建 / 安装包）

从源码构建：

```bash
cargo build --release -p xcontrol-cli                  # headless / 诊断（无 gpui 依赖）
cargo build --release -p xcontrol-app --features x11   # UI / 托盘入口（需 gpui 系统库，见下；平台 feature 说明同 §1.1）
```

- 平台路径：X11 输入/剪贴板层当前可用（feature `x11`——从源码构建同样要显式传，见上）；Wayland 属 M5（[05-platform.md](05-platform.md) §4、[09-roadmap.md](09-roadmap.md) M5）。
- **Linux 构建 xcontrol-app 的系统库前置**：gpui 依赖 font-kit → freetype/fontconfig 的 pkg-config 探测与 Vulkan/wgpu 运行链，需预装对应 -dev 包（判断依据记录在 `.github/workflows/ci.yml` 头注——CI linux job 暂以 `--exclude xcontrol-app` 跳过其编译，runner 装机后移除）；headless 场景用 `xcontrol-cli` 可完全避开 gpui。
- **安装包（M6 交付，11-19-m6-release）**：`scripts/linux/deb.sh`（目录结构 + DEBIAN/control + .desktop 桌面身份 + `dpkg-deb --build`，产物 `target/pkg/linux/xcontrol_<版本>_<架构>.deb`）与 `scripts/linux/appimage.sh`（AppDir + AppRun + appimagetool，工具缺失时交付 AppDir 并打印安装步骤）。两脚本默认 `--features x11`（[08-engineering.md](08-engineering.md) §3 打包脚本固定组合；剪贴板 crate 单平台互斥门禁——x11/wayland 不可同开，Xwayland 兜底由 x11 变体覆盖）；Wayland 原生变体：`FEATURES=wayland bash scripts/linux/deb.sh`（KDE 已实测，GNOME/sway 矩阵移交 [05-platform.md](05-platform.md) §7）。.desktop 是 GlobalShortcuts portal 的桌面身份前提（[05-platform.md](05-platform.md) §4.1）：

```bash
scripts/linux/deb.sh        # 产物 deb（x11）；安装 sudo apt install ./xcontrol_*.deb（或 dpkg -i）
scripts/linux/appimage.sh   # 产物 .AppImage（chmod +x 直接运行）或 xcontrol.AppDir
```

### 3.2 会话与权限（X11）

- 需要 `$DISPLAY` 可达（[05-platform.md](05-platform.md) §6"x11 主/从"行；连接测试即检测方式，失败走错误诊断页）。
- **X11 无安全边界**：任何 X 客户端都能监听全局输入——本项目输入捕获依赖此特性（也是 Wayland 封杀它的原因，[05-platform.md](05-platform.md) §3）；在不受信任主机共享的 X 会话上运行前知悉此语义（安全模型见 [07-security.md](07-security.md)）。
- Wayland（portal / wlroots / uinput 三路径）M5 接入，届时权限路径变为 portal 会话授权（restore token）或 `input` 组/udev 规则（[05-platform.md](05-platform.md) §4、§6 wayland 行）。

### 3.3 防火墙

- 若系统启用防火墙（ufw/firewalld 等），放行 **TCP+UDP 4243** 入站；具体命令以发行版防火墙文档为准（[02-protocol.md](02-protocol.md) §2.3 未对 Linux 作强约束——多数桌面发行版默认不拦截入站，企业/自加固环境需自查）。

### 3.3.1 SSH 远程启动（无桌面终端部署，实测配方）

从 SSH 会话启动 app（KDE Wayland 实机），桌面会话环境不继承——五件套缺一即功能空壳：

```bash
export PATH=$HOME/.cargo/bin:$PATH
export $(systemctl --user show-environment | grep -E "^(WAYLAND_DISPLAY|DISPLAY|XDG_SESSION_TYPE|XDG_RUNTIME_DIR)=" | xargs)
export XAUTHORITY=$(ls /run/user/1000/xauth_* 2>/dev/null | head -1)   # kwin 会话 cookie 归该随机名文件，仅 DISPLAY 不够（"Authorization required" 即此因）
```

- **构建必须带平台 feature**（§3.1）：`cargo build --release -p xcontrol-app --features x11`——漏传进程照常启动，但**无捕获/注入/剪贴板/托盘后端**，仅日志可见 `平台剪贴板后端不可用` warn（易误判为部署成功；2026-11-21 实踩，§1.1 更新记录同源）。
- **feature 按桌面会话类型选，不是按打包脚本默认值**（2026-11-21 KDE Wayland 实机实踩）：Wayland 会话（`XDG_SESSION_TYPE=wayland`）必须用 `--features wayland`（wl-clipboard-rs 原生 data-control，`image/png` 等 MIME 完整可读）；x11 变体走 arboard/Xwayland 桥，KDE 实测**文本能同步但图片 MIME 同步有缺口**——表现为文字正常、截图发不回来（Linux 端读不出图），且启动日志缺 `连接基线：既有剪贴板已登记` 行即为读内容失败的可判症状。x11 变体适合 Xorg 会话（[05-platform.md](05-platform.md) §3）。
- `XDG_RUNTIME_DIR` 在 SSH 会话由 pam_systemd 设置，但显式 export 无害；Wayland 剪贴板路径（wl-clipboard-rs）依赖它定位 `wayland-0` socket。
- 排查顺序：`xdpyinfo` 验 X 连接（XAUTHORITY）→ 进程 `/proc/<pid>/environ` 核五变量 → 日志 grep `后端不可用`（feature 面）→ `lsof -iTCP:4243` 核连接（控制+剪贴板两条 ESTABLISHED 为正常态）。

### 3.4 验证 / 已知限制

- 验证见 §5。
- 运行时行为移交清单（第一轮远端实测已完成，剩余项待实测回填 [05-platform.md](05-platform.md)）：§3.1（输入层）、§3.2（剪贴板层）；Xwayland 行为差异取数项同列。

## 4. 防火墙规则参考（跨平台汇总）

| 平台 | 机制 | 规则面 | 自动化 |
| --- | --- | --- | --- |
| Windows | 预下入站规则（防首监听弹窗误拒） | TCP+UDP 4243，入站，Private | `scripts/windows/firewall.ps1`（§2.3/§2.4）；MSI custom action 自动承载（§2.1，`-Uninstall` 对称清理） |
| macOS | 应用防火墙按签名身份授权 | ad-hoc 签名起步 | `scripts/macos/codesign.sh`（§1.3）；dmg.sh bundle 同签名（§1.1） |
| Linux | 发行版防火墙（如启用） | TCP+UDP 4243 入站 | 手动（§3.3） |

- UDP 侧实现约束：发现 beacon 必须逐接口 bind 具体接口 IP，否则可能静默发不出去（[02-protocol.md](02-protocol.md) §2.3；实现侧已约束，与防火墙无涉但同属"发不出去"排查清单）。

## 5. 安装后验证（三平台通用）

```bash
xcontrol-cli --status            # 配置摘要 + 端口探测 + 日志尾部（只读、不联网）
xcontrol-cli --status --json     # 机器可读（config / process / logs 三节）
xcontrol-cli --status --log-tail 50
```

预期（对应 `--status` 三节输出）：

- **config**：schema 版本、角色（host/client）、端口 4243、发现机制（mdns/beacon…）、日志级别、安全码引用；未写配置时如实标注"默认配置（未找到配置文件）"并显示所查路径。
- **process**：`未检测到监听` = 本机无进程占用端口（xcontrol-app 未运行）；启动 xcontrol-app 后重跑应显示"检测到监听"。**防火墙联动检查**：Windows 上启动过程中不应出现防火墙授权弹窗——若弹出说明预配置未生效（重跑 §2.3 脚本并核对 §2.2 网络位置；弹窗上点"取消"会生成静默阻止规则）；macOS 上签名产物首次询问后不再反复询问（未签名会反复拦截，§1.3）。
- **logs**：xcontrol-app 运行过则显示滚动日志尾部（`<data_dir>/logs/xcontrol.log.<日期>`，NFR-8）。
- 排障：`--log-tail 50` 多看尾部；配置解析/校验错误会响亮上抛（不静默兜底）；任何进程占用端口都会命中探测（伪造服务误报可接受——`--status` 不做协议判断，纯传输层探测）。
- 主从配对的完整 UI 向导属 FR-6.3（M4 首次运行向导，[09-roadmap.md](09-roadmap.md) M4）；本节验证不依赖向导。

## 6. 已知限制与移交清单（汇总）

| 平台 | 锚点 |
| --- | --- |
| macOS | [05-platform.md](05-platform.md) §1（能力矩阵/打包行）、§7（发布门禁清单） |
| Windows | [05-platform.md](05-platform.md) §2.1/§2.2（运行时验收移交）、§7 |
| Linux X11 | [05-platform.md](05-platform.md) §3.1/§3.2（运行时验收移交）、§7 |
| Linux Wayland | [05-platform.md](05-platform.md) §4（M5 接入）、§8（已知风险） |

打包工具链选型（MSI/DMG 制作工具）延后 M6（本任务仅交付脚本 + 本文档，见任务 11-17-m3-packaging prd.md Notes）——**已交付（11-19-m6-release）**：dmg = hdiutil（§1.1，本机实测）；deb = dpkg-deb（§3.1，远端实测）；AppImage = AppDir + appimagetool（§3.1，远端实测）；msi = WiX v3（§2.1，脚本交付 + Windows 侧实跑移交）。

## 修订记录

| 日期 | 变更 |
| --- | --- |
| 2026-11-18 | 初版（任务 11-17-m3-packaging）：三平台安装/防火墙/权限指引；交付 `scripts/windows/firewall.ps1`、`scripts/macos/codesign.sh` |
| 2026-11-20 | M6 安装包收口（任务 11-19-m6-release）：§1.1 dmg.sh（本机实测）/ §2.1 msi 脚本（WiX，移交实跑）/ §3.1 deb+AppImage 脚本（远端实测）；firewall.ps1 增 `-Uninstall`（MSI 卸载对称清理）；§4 表更新 |
| 2026-11-20 | m6-release check 修正：`xcontrol.wxs` 防火墙 custom action 由 FileRef 型（type 34 = CreateProcess 直接启动安装文件——.ps1 非 Win32 可执行，必失败 error 1721）改为 SetProperty（type 51 存 powershell.exe 完整命令行）+ Property 型 deferred 执行（type 50）；Windows 侧实跑验证仍为移交项（§2.6）。§1.1/§2.1/§3.1 从源码构建命令补平台 feature（`--features macos/windows/x11`——默认全关，漏传无捕获/注入后端，README 快速开始同步） |
