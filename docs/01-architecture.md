# 01 · 系统架构

> 状态：草案 v0.1 · crate 布局与分层红线见 [08-engineering.md](08-engineering.md) §1；选型决策见 [adr/](adr/README.md)。

## 1. 总体形态

**单二进制、单进程、角色可切换**（主控 Host / 从机 Client）。进程常驻系统托盘，按需弹出 gpui 设置窗口。

```
┌─────────────────────────────────── xcontrol 进程 ───────────────────────────────────┐
│                                                                                     │
│  ┌───────── UI 层（仅 xcontrol-app）─────────┐      ┌───────── 平台层（平台线程）────┐        │
│  │ gpui 设置窗口 / 向导 / 布局编辑器          │      │ mac: CGEventTap RunLoop 线程   │        │
│  │ 托盘（xcontrol-tray: gpui-tray 直连）     │      │ win: LL-Hook 消息线程          │        │
│  └──────────────┬───────────────────────────┘      │ x11: x11rb 连接线程            │        │
│                 │ AppState 快照(watch) + 命令       │ wl: ashpd/reis/layer-shell 任务│        │
│  ┌──────────────▼─────────── 核心层（xcontrol-core + xcontrol-net，tokio 运行时）────┐│        │
│  │ 角色状态机 · 会话管理 · 屏幕布局/路由 · 映射引擎 · 剪贴板同步策略 · 发现/重连       ││        │
│  └──────┬──────────────────────────────────────────────┬───────────────────────────┘│        │
│         │ XCP 消息（xcontrol-proto）                    │ 平台 trait（xcontrol-input/-clipboard）│
│  ┌──────▼──────────────┐                     ┌─────────▼────────────────────────────┐        │
│  │ 传输（xcontrol-net）  │                     │ 输入/剪贴板平台实现（feature 门控）     │        │
│  │ Noise/snow · TCP×2   │                     │ macos / windows / x11 / wayland      │        │
│  │ mdns-sd · UDP beacon │                     └──────────────────────────────────────┘        │
│  └──────────────────────┘                                                                   │
└─────────────────────────────────────────────────────────────────────────────────────────────┘
```

## 2. 线程/任务模型

| 执行域 | 归属 | 内容 |
| --- | --- | --- |
| **gpui 主线程** | xcontrol-app | 事件循环 + 渲染；`QuitMode::Explicit` 保活；托盘事件经 channel 汇入 |
| **tokio 多线程运行时** | xcontrol-net / core 服务任务 | TCP 监听/连接、Noise 握手、mDNS、UDP beacon、扫描、心跳、剪贴板流 |
| **平台输入线程** | xcontrol-input | 系统回调（CGEventTap RunLoop / LL-Hook 消息循环 / x11rb 读循环 / reis 读取）；**禁止 async**，与 tokio 用 mpsc 桥接 |
| **剪贴板监听线程** | xcontrol-clipboard | 平台 watcher / macOS 轮询定时器 |
| **专用线程：argon2 派生** | 启动期一次性 | PSK 派生（~几十 ms 级，不阻塞主线程/运行时） |

规则：

- 平台回调线程只做"事件转换 + 投递"，任何慢操作（读写剪贴板、网络）一律转发给核心任务；Windows LL-hook 回调尤其要零阻塞（LowLevelHooksTimeout）。
- UI ↔ 核心：`AppState` 快照（watch channel，200ms 合并节流）+ `CoreHandle` 命令（mpsc）。UI 不直接触碰网络/平台层。

## 3. 核心状态机

### 3.1 从机（Client）

```
Idle → Scanning ──发现候选──► Probing(握手) ──成功──► Connected ⇄ Paused
 ▲                                                    │  断线
 └────────────────── 用户停止 ◄────────────────────────┴── Retrying(指数退避 1s→30s) ──► Scanning
```

- `Connected` 子状态：`IdleLocal`（未被控）⇄ `Active`（受控中，EnterScreen 已发生）；修饰键快照对齐在两个方向切换时都执行。
- 回本地控制的触发：LeaveScreen / 热键 / 主控心跳超时(9s) / 用户暂停 —— 四路都不得遗漏（NFR-5）。

### 3.2 主控（Host）

```
Listening ──从机握手──► Paired(n) ⇄ Paused
   │                        │
   │        输入流路由：focused_client ∈ {None(本机), Some(client_id)}
   └── 热键/边缘切换 → 修改 focused_client（先 LeaveScreen 旧目标/本机，再 EnterScreen 新目标）
```

- 屏幕布局图（`xcontrol-core::layout`）是路由唯一依据；从机上下线时重建邻接表并下发 `Layout`。

## 4. 数据流（端到端）

**输入（主控→从机）**：`平台捕获线程 → InputEvent(XKey 语义化) → [主控过滤:热键] → 路由(focused? local : client) → proto 帧 → Noise → TCP#1 → 从机 → [映射引擎(03-input §4)] → 平台注入`
**剪贴板（双向）**：`平台监听 → 防回环(04 §5) → 读格式 → proto ClipAnnounce/Chunk → Noise → TCP#2 → 对端校验/重组 → 本地写(带自标记)`

延迟预算（NFR-1 的分解，作为 M1 基准验收）：

```
捕获回调→出网 <2ms · LAN RTT <1ms(有线) · 入网→注入回调 <3ms · 总预算 P50 15ms / P99 40ms（含 Wi-Fi）
```

## 5. 配置（xcontrol-config）

- 位置：平台标准目录（mac `~/Library/Application Support/XControl/`，win `%APPDATA%\XControl\`，linux XDG）。
- `config.toml`：schema 带 `version` 字段，迁移函数链；**安全码默认存 OS 钥匙串**（keyring crate），不可用时回退配置文件 + `0600` + UI 警告（[07-security.md](07-security.md)）。
  当前 schema 版本 **v3**：v2→v3 扩 `[keymap]`（新增 `passthrough` 直通开关 FR-3.6；`rules` 升级为 docs/03 §4.1 的 trigger/action 四形态规则模型）；v1→v2 新增 `[general]` 节（`log_level`，NFR-8）。历史 v0/v1/v2 fixture 永远可升级（迁移链只追加，v2 的 `from`/`to` 简式规则由迁移链自动转换为 trigger/action 形态）。**v3 内追加字段（默认值兼容旧文件无需迁移）**：`[net].allowlist`（FR-5.4 主控接入白名单，CIDR 数组，空 = 不启用）与 `[net].confirmed_host`（FR-5.5 已记忆主控，instance id 的 32 位 hex）——11-19-m6-security；`[general].language`（FR-6.5 界面语言，`zh-CN` 默认 | `en`，11-19-m6-release）。
- 热加载：文件监听 + 内部 `ConfigReload` 事件；角色/安全码变更触发受控的重连流程。keymap/hotkeys 变更**运行中会话即时热替换**（11-21 实测反馈，docs/06 §4.1 生效时点表：热键匹配表 / 映射规则原位替换——`apply_hot_key_configs`；文件监听仍未实装，改动经设置页 → `RuntimeConfigUpdate` 命令通道）。

```toml
role = "host"                 # host | client
security_code_ref = "keychain:xcontrol"   # 或 file（回退模式）
[general]                     # 通用项（v2 起，NFR-8）
log_level = "info"            # error | warn | info | debug | trace（默认 info；--log-level 可覆盖）
language = "zh-CN"            # FR-6.5 界面语言：zh-CN（默认）| en（UI 层文案表，docs/06 §7）
[net]
port = 4243
cidrs = []                    # 空 = 自动取各接口真实前缀
discovery = ["mdns", "beacon", "scan-off"]
allowlist = []                # FR-5.4 主控接入白名单（CIDR 数组；空 = 不启用；serve 层握手前过滤）
confirmed_host = "abab..."    # FR-5.5 已记忆主控（instance id 32 位 hex；缺省 = 无记忆）
[layout]                      # screens 网格（FR-1.3）：[[layout.screens]] 逐条 { id, relative_to, direction = up|down|left|right }；
                              # relative_to 缺省 = 锚定主控主屏（11-19-m5-layout：方位选择制最小模型，方向决定邻接边）
[keymap]                      # 规则四形态见 docs/03 §4.1（v3 起 trigger/action 模型）
preset = "mac2win"
passthrough = false           # FR-3.6 直通模式：true 时绕过全部规则（托盘/热键一键切换的落点）
rules = []                    # [[keymap.rules]] 逐条展开；自定义叠加在预设之后（先命中预设则不查自定义）
[hotkeys]
next = "Ctrl+Alt+Right"
gesture_fallback = false      # docs/05 §5 优先级 3：Wayland 手势回退开关（11-19-m5-wayland）
[clipboard]
scope = "current"             # current | all
max_bytes = 16777216
formats = ["text", "image", "html"]
```

## 6. 工具链与依赖基线（回填自调研，M0 冻结）

| 项 | 基线 |
| --- | --- |
| Rust | **1.99**（M0 spike 实测修订 2026-11-16：gpui-pre 0.3.12 在 1.92/1.95 编译失败、stable 1.99.1 通过，原"1.92"为调研误判；见 ADR-0001 更新记录；`rust-toolchain.toml` 三平台锁定同版） |
| UI | `gpui-kit = 0.9`（经其使用 gpui；不自写 gpui 依赖）+ `xcontrol-tray`（gpui-tray 直连适配层，自建 tray-icon/ksni 仅为备胎，见 [06-ui.md](06-ui.md) 与 ADR-0001） |
| 网络/加密 | `tokio` · `snow`（Noise） · `mdns-sd` · `rustls` 不用于 PSK（无此能力，ADR-0002） |
| 序列化 | 输入手写定长；控制 `postcard`；配置 `toml` |
| 平台 | `core-graphics`/`objc2-app-kit`（mac）；`windows`（win）；`x11rb`（X11）；`ashpd`+`reis`+`wayland-client`/`wl-clipboard-rs`（Wayland）；`enigo`（成熟平台注入备选） |
| KDF | `argon2` + `blake3` |
| 构建注意 | gpui 系 crate 需 `opt-level=3`（debug 卡顿）；Windows 需 MSVC+SDK+CMake；Linux 需 Vulkan 驱动 + fontconfig/wayland/xkbcommon 系统库 |

## 7. 关键架构决策索引

| 决策 | ADR |
| --- | --- |
| gpui 依赖方式与托盘集成策略 | [ADR-0001](adr/0001-gpui-and-tray.md) |
| 传输层/加密/发现选型（TCP×2 + Noise + 三层发现） | [ADR-0002](adr/0002-transport-and-discovery.md) |
| 各平台输入/剪贴板实现路径 | [ADR-0003](adr/0003-platform-input-paths.md) |
| 文件传输通道拓扑与物化策略（独立文件 TCP + stage-then-mount，FR-7.3） | [ADR-0004](adr/0004-file-transfer-channel-and-materialization.md) |

（输入抽象与映射语义不设 ADR：属规范本体，见 [03-input.md](03-input.md)。）
