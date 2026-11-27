# 04 · 剪贴板同步规范

> 状态：草案 v0.1 · 实现 FR-4.x。传输分帧/流控见 [02-protocol.md](02-protocol.md) §6；平台读写路径见 [05-platform.md](05-platform.md)。

## 1. 设计原则

1. **线格式（wire format）统一**：跨机交换使用平台无关格式；各平台的本地格式转换在两端平台层完成（学 Input-Leap 的 wire 约定，源码验证）。
2. **尽力而为**：剪贴板同步失败不重传、不断连（通知 UI 即可）；能力不匹配时干净降级（HTML→纯文本）。
3. **防回环优先**：两台机器互相监听对方剪贴板变化，回环风暴是此类工具最常见 bug，见 §5。
4. **大块数据不阻塞输入**：独立 TCP 通道 + 流控（[02-protocol.md](02-protocol.md) §4.4/§6）。

## 2. 格式与线格式

| 线格式 | 内容 | 平台本地格式转换 |
| --- | --- | --- |
| `text/plain` | UTF-8，**换行统一 LF** | Windows 读写时 LF↔CRLF（写时 `\n`→`\r\n`，读时去 `\r`）；macOS/Linux 原生 LF 直通 |
| `image/png` | PNG 字节流（图片交换唯一必选格式） | mac：NSPasteboard PNG/TIFF（NSImage↔RGBA，读回**重编码**）；win：CF_DIB↔RGBA（arboard）；X11：`image/png` target（arboard 内部编解码）；Wayland：`image/png` mime **直通**（wl-clipboard-rs，零转换） |
| `text/html` | HTML fragment（UTF-8、LF） | win：CF_HTML 包装（arboard clipboard-win `wrap_html` 偏移头；读回提取行为实机复核项）；mac：`public.html` **原样挂载**（objc2 直写，不经 arboard 模板包装）；X11/Wayland：`text/html` target/mime 原样 |
| `text/uri-list`（P2，FR-4.6） | 文件路径列表（仅文本，不传内容） | win：CF_HDROP ↔ URI 列表转换 |

- **图片 round-trip 等价判据 = 像素级**（11-19-m5-clipboard-full 裁定）：PNG 是可重编码容器——mac 经 NSImage/TIFF、win 经 CF_DIB、X11 经 arboard 内部编解码后读回的 PNG 字节与写入字节必然不同（压缩器非规范），但 8bit RGBA 像素逐字节无损；wayland 为 mime 直通（字节级等价）。图片回声指纹因此取**解码后 RGBA 规范形**（宽高+像素，`png_codec::rgba_canonical_fingerprint_input`），不取 PNG 字节。
- **16MiB 上限对图片的适用性复核结论（FR-4.5，2026-11-19）**：维持 16MiB 不变——上限口径 = wire PNG 字节数（传输货币）；4K 全屏截图 PNG 实测量级 2–8MiB、常规截图 <2MiB，16MiB 覆盖 4×4K 全屏量级；超出走既有"超限不传输 + 托盘提示"路径（FR-4.5），无新增协商位。RGBA 原始数据（如 4K RGBA = 32MiB）经 PNG 压缩后通常已低于上限，且同步以 PNG 为准不传 RGBA 原始面。
- **HTML round-trip 判据 = fragment 字节级**：mac 写入走 public.html 原样落板（避免 arboard `set_html` 的 `<html><head><meta…></head><body>…</body></html>` 模板包装破坏读回等价性）；win 经 arboard/clipboard-win 的 CF_HTML 包装——读回提取（StartFragment/EndFragment）行为须实机复核（docs/05 §2.2 item 6），若读回形态 ≠ 写入 fragment，回声指纹按读回形态对齐（防线 1 语义不变）。
- 一次 `ClipAnnounce` 可携带**多格式并存**（text+png+html），接收端按自身能力择优挂载（多格式同挂，由粘贴目标决定取用——Input-Leap 同款语义）。多格式时单传输 content 的分段布局见 [02-protocol.md](02-protocol.md) §6（formats.len()==1 → 原始字节；≥2 → postcard `Vec<Vec<u8>>` 段序同 formats 序）。
- 格式协商：Caps 位声明（`CAP_IMAGE`/`CAP_HTML`），发送端只发对端声明支持的格式。
- **文件引用剪贴板不走本表**（FR-7.3，XCP 1.2+）：不是 `ClipFormat` 变体、不经
  `ClipAnnounce/ClipChunk`——文件域走独立文件通道与消息（[02-protocol.md](02-protocol.md) §4.6，
  [ADR-0004](adr/0004-file-transfer-channel-and-materialization.md)）；剪贴板侧仅产生元数据
  清单（§8），对端 pending 槽承接，本机系统剪贴板不被触碰。FR-4.6 的 `text/uri-list`
  仍是"仅路径文本"的独立最小形态（FR-7.3 不可用时的降级路径，docs/00 FR-4.6 备注）。

## 3. 传输流程（与协议 §4.4/§6 对应）

```
本机剪贴板变化(监听) → 防回环检查(§5) → 读取各格式(≤上限) → ClipAnnounce{formats,total,blake3}
  → 对端按需请求(默认全收) → ClipChunk(≤65472B 发送有效分块, 信用窗口4) → ClipEnd → blake3 校验
  → 本地写入(带回环标记) → 对端 UI/托盘提示"已同步"
```

- 默认策略：**变化方主动推送**（"拉"模式会让源机器在粘贴瞬间才读，源可能已退出受控态，时序脆弱）。
- 超限（默认 16MiB，可配）：不传输，托盘气泡提示"剪贴板内容 N MB 超过上限"（FR-4.5）。
- 分块大小（发送侧）：64KiB 是接收端解码校验的语义上限，发送端有效分块 ≤ **65472B（64KiB−64B）**——满 64KiB 块 + postcard 头必然超帧层 `len: u16`（65535）与 Noise 单报文明文（65519）两种物理上限（2026-11-17 修订，见 [02-protocol.md](02-protocol.md) §6.2）。
- 同步范围（FR-4.4）：`current`（默认，仅当前受控从机↔主控）/ `all`（主控↔全部在线从机；从机之间不直连，经主控中转）。

## 4. 变化监听（平台路径）

| 平台 | 机制 | 参数 |
| --- | --- | --- |
| macOS | `NSPasteboard.changeCount` 轮询 | 250ms（rustdesk 同款 333ms 量级） |
| Windows | `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE`（隐藏消息窗口） | 事件驱动；被占用 3×33ms 重试（rustdesk 同款） |
| X11 | XFixes `SelectionNotify`（Clipboard selection） | 事件驱动；读时经 selection 请求协议 |
| Wayland | `wl-clipboard-rs` Watcher（wlr/ext-data-control 协议事件）；GNOME 探测失败退 ashpd clipboard portal | 事件驱动 |

- X11/Wayland 需**常驻持有 selection** 响应对端 paste 请求（进程退出即丢——文档中提示"退出 XControl 会清空 Linux 剪贴板内容"或做"临终转交"尽力而为）。
- Primary selection（X11 中键粘贴）：v1 不同步（P2，独立开关）。

## 5. 防回环（关键正确性设计）

三重防线，全部实现：

1. **写入自标记**：本进程写入剪贴板时记录 `{写入时刻, blake3(内容规范形), origin_id=self}`；监听器收到变化时若 `(内容指纹 == 最近一次自写指纹)` 且在 2s 窗口内 → 丢弃。学 rustdesk 的 owner 格式思路，但不额外占剪贴板格式位（跨平台写入自定义格式的兼容坑多，哈希+时间窗足够）。**M5 指纹规范形（11-19-m5-clipboard-full）**：text = LF 归一化 UTF-8 字节（Windows 指纹字节教训的 round-trip 不动点）；image = 解码 PNG 后的 RGBA 像素（宽高+像素，PNG 重编码不影响——见 §2）；html = UTF-8 字节原样（mac public.html 原样挂载保证写读等价）；多格式内容按 text→image→html 的 `tag+len` 布局拼接（单格式无包裹，跨格式不碰撞）。
2. **来源序列号**：`ClipAnnounce` 携带 `{origin_id, origin_seq}`；每台机器维护 `已见(origin_id, seq)` 去重——同一内容二次到达不重写。
3. **写后静默窗**：本地写入后 500ms 内监听到的任何变化事件一律视为自身写入的回声，不外发且**不移动哈希基线**（基线始终为最近一次本地写入；2026-11-17 修订：原"仅更新哈希基线"若把基线漂移到窗口内观测到的外部内容，该内容在其后 2s 内再次触发时会被误判为回声而丢失同步——11-17-m2-clipboard-macos 实现语义）。

- 边界：用户在两台机器 2s 内先后复制**完全相同内容** → 被防线 1 拦截，属可接受假阴性（内容相同，同步与否无差别）。

## 6. 安全与资源

- 内容在 Noise 通道内加密传输（与输入同 PSK 体系）。
- 接收端逐帧限额/超时/重组隔离（[02-protocol.md](02-protocol.md) §6，Deskflow CVE 教训）。
- 剪贴板内容**不写日志**；诊断模式（用户显式开启）仅记录格式与长度。
- 大内容读取在平台层必须异步（不阻塞输入线程）。

## 7. 测试要求

- 单元：换行规范化（CRLF/LF/CR 混合）、PNG 编解码 round-trip（含 RGB/灰度/调色板归一化、重编码指纹不变）、CF_HTML 偏移头构造/解析（arboard 内建；实机复核项）。
- 集成（mock 平台层）：双端回环场景矩阵——A 复制→B 收到→B 被再次触发→断言无二次传播（三重防线各关闭一项的故障注入测试）。
- 状态机（M5 补录）：图片/HTML/多格式 announce→chunk→end 全链路（信用窗口 ≤4 多轮翻转）；图片 16MiB 恰好通过、+1 拒绝（`xcontrol-core` tests/clip_flow.rs 四项 M5 测试）。
- 大块：16MiB 通过、16MiB+1 拒绝且通知、10s 慢速发送超时清理。
- 平台手动：六向图片矩阵（mac↔win↔linux）、富文本降级、emoji/组合字符文本；mac 图片/HTML round-trip 已实测（2026-11-19，本仓冒烟 PASS——图片像素级等价 + RGBA 指纹判据、HTML fragment 字节等价 + 格式隔离）；远端 KDE Wayland 文本 round-trip + image/html 直通实测 PASS（配方与结果 docs/05 §4）。

## 8. 文件引用剪贴板（FR-7.3，XCP 1.2+，规划）

> 通道与消息见 [02-protocol.md](02-protocol.md) §4.6；通道拓扑与物化策略决策见
> [ADR-0004](adr/0004-file-transfer-channel-and-materialization.md)。本节定义剪贴板侧语义：
> 检测、pending 槽、代次失效、指纹规范形与物化。**随实装切片落地，当前为规范先行。**

### 8.1 检测与元数据面

- 复制端：既有剪贴板监听器（§4 平台路径）新增识别文件引用格式（平台格式矩阵见
  [05-platform.md](05-platform.md) §0.2）→ stat 元数据（条目名/大小/mtime/是否目录）
  → `FileRefAnnounce`（仅顶层清单，无内容字节；目录条目 `children` 为子树条目数，
  可为 0 = 未统计，子树在拉取时经 `FileExpand` 展开）。
- 接收端：清单入 **pending 文件槽**（会话内状态，独立于系统剪贴板），托盘提示
  「剪贴板文件：N 项 / X MB（来源机名）」；**不触碰本机系统剪贴板**——挂板只在
  物化时发生（§8.4）。
- 与文本/图片同步并存：文件 announce 不改变 text/png/html 的同步行为；pending 槽
  是剪贴板域之外的附加状态。

### 8.2 代次失效（防线 2 的文件域扩展）

- 任何新的 `FileRefAnnounce`（任意来源机）使对端旧文件槽失效；本机任何新的本地
  复制（text/png/html/文件引用）同样使槽失效——**最后复制者赢**，与 `origin_seq`
  代次语义一致（§5 防线 2 复用）。
- 物化挂板（写系统剪贴板）进入防线 1 写入自标记（写后 500ms 静默窗语义不变）。

### 8.3 指纹规范形（防线 1 的文件域定义）

- 文件清单指纹 = 条目 `(name, size)` 有序序列的 blake3，**不含 mtime、不含绝对
  路径**——物化落盘必然改变两者，纳入指纹必回环（A 复制 → B 物化挂板 → B 监听器
  误判为新复制回传 A）。
- 同板多域并存（文件引用与文本同时挂板）：文件域指纹独立计算、与 text 域指纹
  分开比较，两域防线互不干扰。

### 8.4 物化（stage-then-mount，[ADR-0004](adr/0004-file-transfer-channel-and-materialization.md)）

- 触发：接收端粘贴动作——切片 c 透明拦截（注入路径拦截自身要注入的粘贴键，先物化
  再注入）/ 切片 b 托盘显式「传输并粘贴」动作。
- 流程：`FilePullReq` → 暂存区落盘（专用子目录，剩余空间预检，超阈值需确认，
  [07-security.md](07-security.md) §9）→ 挂**真实文件引用**到系统剪贴板（win `CF_HDROP` /
  mac file URLs / X11 `text/uri-list` / Wayland mime 直通，docs/05 §0.2）→ 注入粘贴键
  （透明路径）。
- 失败：条目级 `FileError` 可重试（[02-protocol.md](02-protocol.md) §4.6）；源离线 /
  条目消失给明确提示；部分成功策略随切片 d 定稿。

### 8.5 与 §3「推送决策」的辨析（防规范自相矛盾）

- §3 否决"拉模式"针对**剪贴板内容字节**：源应用可能清板、源可能已退出受控态，
  粘贴瞬间读源板时序脆弱——故 announce/内容推送按 §3 语义不变。
- 文件内容持久在文件系统、XControl 常驻运行（不随受控态启停），粘贴时刻拉取的
  脆弱面仅剩"源机在线"——离线即条目级错误提示，不做重试风暴。元数据面仍是
  复制瞬间主动推送。两者不冲突。

### 8.6 测试要求

- 指纹规范形单测：mtime 变化 / 路径变化 / 重命名不改变指纹；改名 + 同大小碰撞场景
  明确为可接受假阴性（§5 边界惯例）。
- pending 槽代次失效矩阵：文件→文件、文件→文本、文本→文件、跨机并发来源。
- 三防线故障注入（§7 惯例）扩展到文件域：关闭防线 1/2/静默窗各一，断言无回环传播。
- mock 平台层物化回环：announce → 拉流 → 挂板 → 监听器触发 → 断言被防线拦截。
