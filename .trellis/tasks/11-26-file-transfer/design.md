# design：跨设备文件复制粘贴（技术设计）

> 本文件是 ADR-0004 与 docs 修订的源材料；定稿后事实源回到 docs/，本文件不随后续实现演进。

## 1. 三段式模型

```
[复制时] 源端监听到文件引用剪贴板（Finder/资源管理器复制 = 只含路径，读取零成本）
  → 仅发送元数据清单（条目名/大小/mtime/目录树摘要/总字节，几 KiB）
  → 对端存 pending 槽，托盘提示；对端系统剪贴板不动，无内容传输

[粘贴时] 接收端拉流：
  FilePullReq(条目索引) → FileChunk* (64KiB 分块、信用窗口) → FileEnd(blake3)
  → 落盘暂存区 → 把真实文件引用挂上本机系统剪贴板 → 注入粘贴键

[失效] 源端任何新的剪贴板复制（文本/图片/文件）→ 新 announce 使旧 pending 槽失效
  （origin_id/origin_seq 代次机制复用 docs/04 §5 防线 2）
```

关键洞察：源端「复制」天然就是元数据操作——OS 剪贴板里只有路径，文件不动在文件系统里。
惰性传输在源端零改造；复杂度集中在接收端「粘贴物化」。

## 2. 通道拓扑决策（ADR-0004 主议题）

**决策：独立第三条 TCP（file 通道），不复用剪贴板 TCP。**

| 备选 | 结论 |
| --- | --- |
| 复用剪贴板 TCP（同连接新 kind） | 否——文件拉流可达 GB 级、长时占用，与剪贴板同步互为队头阻塞；剪贴板通道现有状态机参数（16MiB 上限/单活动 clip/10s 超时/不重传）是「剪贴板语义」，文件语义不同，同连接分派会让一个连接承担两套重组状态机 |
| 控制连接上复用（同 1.0 裁定教训） | 否——docs/02 §4.4 2026-11-21 修订已裁定大块数据必须独立连接（队头阻塞 = 控制被切回，实测教训） |
| **独立 file TCP**（选定） | 与 §4.4 双连接同构：另拨 TCP → 独立 Noise 握手 → 首帧 FileBind{session_id} 绑定；任何文件通道故障只影响文件传输，独立重连 |

连接模型变更：每对机器至多 3 条 TCP（控制 / 剪贴板 / 文件），均按需建立、空闲关闭。

## 3. 协议消息草案（XCP minor 1.1 → 1.2，新 kind 随 minor 提升）

kind 分配走文件域 0x58–0x5F（clip 域止于 0x54，0x55–0x57 留作 clip 域扩展）：

| kind | 消息 | 说明 |
| --- | --- | --- |
| 0x58 | `FileBind { session_id: [u8;16] }` | 首帧绑定控制会话（ChannelBind 同款校验） |
| 0x59 | `FileRefAnnounce { transfer_id, origin_id, origin_seq, entries, total_bytes }` | 元数据清单；可走文件通道或控制连接（待规范定稿：倾向文件通道，控制连接零新增帧类） |
| 0x5A | `FilePullReq { transfer_id, entry: u32, offset: u64 }` | **条目索引寻址，绝不含路径**（安全红线） |
| 0x5B | `FileChunk { transfer_id, seq: u32, data }` | 分块沿用 65472B 有效值（物理上限不变）；seq u32（GB 级流） |
| 0x5C | `FileAck { seq: u32 }` | 信用归还；窗口可配（默认放大到 16） |
| 0x5D | `FileEnd { transfer_id, entry: u32, blake3: [u8;32] }` | 条目级校验 |
| 0x5E | `FileError { transfer_id, entry: Option<u32>, code }` | 条目级失败/整单失败；**可重试语义**（与 clip「不重传」不同） |
| 0x5F | `FileCancel { transfer_id }` | 用户取消 |

与 Clip 状态机的参数差异（必须写进 docs/02 防护节）：
- 总量 `u64`、上限可配 GB 级（默认 2GiB?，config 项）；16MiB 剪贴板上限不适用于文件域。
- 超时：改「分块空闲超时」（如 10s 无 chunk 判死）+ 总时长上限，不随大小缩放的固定 10s 不可用。
- 哈希：源端边流边算（复制瞬间不预哈希，保持复制零延迟）。
- 清单条目数上限（防清单爆炸；目录顶层先行，子树拉取时再展开）。

## 4. 接收端物化（stage-then-mount，统一三平台）

- 拉流落盘到暂存区（临时目录，XControl 子目录命名，防与用户文件混淆）。
- 挂板 = 把真实文件引用写上系统剪贴板：win `CF_HDROP`（clipboard-win）、mac file URLs（objc2 直写，同 HTML 直写路子）、X11 `text/uri-list`、Wayland mime 直通。
- v1 不用 OS 原生惰性（mac `NSFilePromiseReceiver` / win `CFSTR_FILEDESCRIPTOR`）：
  粘贴路径支持需实机 spike，且 Linux 无对应机制——统一落盘挂板，原生惰性列 P2 增强。
- 透明粘贴（切片 c）依赖：粘贴键本来就是 XControl 注入的（KVM 架构天然钩子）——
  拦截自己要注入的键、先物化再注入，零系统级 hook。
- 物化挂板在文本应用里的行为（mac Finder 复制文件贴进 TextEdit 的表现）须实机复核，
  保证「与本地剪贴板行为一致」。

## 5. 平台矩阵（检测 = 已有监听器新增识别；挂板 = 新平台代码）

| 平台 | 复制端检测 | 粘贴端挂板 |
| --- | --- | --- |
| macOS | changeCount 轮询识别 `public.file-url` | objc2 直写 file URLs |
| Windows | WM_CLIPBOARDUPDATE 识别 CF_HDROP | clipboard-win 写 CF_HDROP |
| X11 | XFixes 识别 `text/uri-list` / `x-special/gnome-copied-files` | x11-clipboard 写对应 target |
| Wayland | zwlr_data_control mime | wl-clipboard-rs mime 直通 |

## 6. 安全不变量（docs/07 落点）

1. **条目索引寻址**：拉取请求只含 transfer_id + entry 下标 + offset；源端拒绝未通告清单的
   任何请求 → 杜绝「构造请求读任意文件」。
2. 元数据面最小化：清单只含名字/大小/mtime/树形，不含绝对路径明文（条目相对路径仅在
   清单内、以接收端视角呈现）。
3. 磁盘耗尽：接收端总量预检（剩余空间）+ 超阈值（可配）粘贴前需确认。
4. 清单/条目数/深度/总字节上限全部接收端校验（信任边界在接收端，docs/02 §6 原则沿用）。

## 7. 防回环新规范形（docs/04 落点）

文件清单指纹 = 条目 `(名字, 大小)` 序列化后哈希，**不含 mtime、不含绝对路径**
（物化落盘后 mtime/路径必变，含之必回环）。物化挂板同样进入「写入自标记」防线 1
（写后 500ms 静默窗语义不变），防线 2 代次去重使跨机传播天然终止。

## 8. 与既有决策的衔接（docs/04 §3 辨析，必须写入文档）

docs/04 §3 否决过「拉模式」，理由是源机器粘贴瞬间可能已退出受控态——那是针对
**剪贴板内容**（源应用可能清板）。文件在文件系统里、XControl 常驻，脆弱面缩到只剩
「源在线」：离线 → FileError + 明确提示，不重试风暴。不构成与既有决策的冲突。

## 9. 风险登记

| 风险 | 缓解 |
| --- | --- |
| 清单爆炸（复制 node_modules） | 条目数/深度上限 + 顶层先行 |
| 源端离线/文件消失 | 拉取时 re-stat，逐条目错误 + 部分成功策略（切片 d 定稿） |
| pending 槽与文本剪贴板竞争 | 代次失效：任何新复制（任何格式）使文件槽失效 |
| 大流量拖慢剪贴板同步 | 独立通道（§2 决策） |
| 粘贴拦截误伤普通文本粘贴 | 仅 pending 槽有货且未失效时拦截；文本粘贴路径零改动 |

## 10. 落地切片（任务地图见父 prd）

a 元数据面 → b 通道拉流+显式动作 → c 透明粘贴 → d 目录/进度/校验。
每切片独立可验收；b 完成即有端到端可用价值（不依赖 c）。
