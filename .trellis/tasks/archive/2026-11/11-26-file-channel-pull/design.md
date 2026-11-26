# design：切片 b（文件通道拉流）

> wire 契约 docs/02 §4.6；安全红线 docs/07 §9；本文件定实现边界与分层归属。
> 分阶段交付：**b-core（本轮）** 状态机层 → b-net 文件通道接线 → b-平台 真实检测 → b-app 托盘动作。

## 1. 分层归属（零 I/O 红线的推论）

core 零 I/O ⇒ 文件字节读取/落盘在 app 层；core 持有**编排与校验**：

| 单元 | crate | 职责 | I/O |
| --- | --- | --- | --- |
| `file_serve` | core | 源端清单注册表（transfer_id → 条目下标空间）+ PullReq 校验（未知 id/越界下标/offset 超尺寸 = 协议违规）+ 并发 transfer ≤4 | 无 |
| `file_tx` | core | 源端条目流编排：seq 分配（u32）、信用窗口（初始 16）、已发字节累计、发满即产出 FileEnd | 无 |
| `file_rx` | core | 接收端条目组装：seq 校验（乱序违规/重复去重）、累计 ≤ size、**blake3 增量计算**（每条目一个 Hasher）、End 校验、分块空闲超时 10s + 单 transfer 总时长上限 30min（Clock 注入） | 无 |
| 文件读取/落盘 | app | 按 serve 计划读文件块；按 rx 的 `WriteChunk` 动作写暂存文件；拉取前 re-stat（不一致 → `FileError{SizeMismatch}`）；完成后挂真实文件引用 | 全部 |

- 帧方向：`PullReq`/`Ack` = 接收端→源端；`Chunk`/`End`/`Error`/`Cancel` = 源端→接收端。
- 落盘防穿越依赖 proto 层名称校验（`/`、`.`/`..`、NUL 已禁）+ app 暂存专用目录。
- blake3 源端边流边算 = app 读块时喂 tx 侧 Hasher（与 rx 侧对等）；`FileEnd.blake3` 由 app 提供，core 只做**接收端**比对（tx 侧摘要组装不属状态机）。

## 2. b-core 状态机规格（本轮）

### file_serve（源端）

- `register(FileRefAnnounce)` → 登记下标空间（条目 size 表）；重复 transfer_id → 忽略（幂等）。
- `register_expand(FileExpand)` → 校验 base == 当前下标空间上界，追加条目；违规 → Violation。
- `on_pull(FilePullReq)` → `Grant{transfer_id, entry, offset, size}` | `Violation{reason}`；
  活动服务数 ≤ 4（docs/02 §4.6 防护规则 7），超出 → Violation。
- `finish_entry/cancel_transfer` → 释放服务槽位。

### file_tx（源端编排）

- `begin_entry(transfer_id, entry, size)` → 进入条目流（同时只服务一条目，串行多文件）。
- `next_seq()` + `on_chunk_sent(seq, len)` 占用信用；`on_ack(seq)` 归还（同 ClipTx，u32）。
- `bytes_sent() == size` → 调用方产出 `FileEnd`（core 提供 `should_end()` 判定）。
- 自定义常量：`INITIAL_FILE_CREDIT_CHUNKS = 16`。

### file_rx（接收端组装）

- `begin_pull(transfer_id, entry, size)` → 登记期望（size 来自 pending 槽清单）。
- `on_chunk`：seq 判定（期望=接受+`WriteChunk{data}`+归还 `SendAck{seq}`；重复=去重忽略；跳号=Violation）；
  累计 ≤ size（超 = Violation）；Hasher 增量喂入；刷新 last_chunk_ms。
- `on_end`：received == size 且 blake3 一致 → `EntryVerified`；不一致 → `EntryFailed`（可重试，不断连）。
- `on_error(FileError)` / `on_cancel(FileCancel)` → `EntryFailed`/teardown（可重试语义，docs/02 §4.6 防护规则 6）。
- `on_tick(now_ms)`：10s 无 chunk → `EntryFailed{idle-timeout}`（软）；30min 总时长 → 整单失败。
- 动作枚举 `FileRxAction`：`SendAck{seq}` / `WriteChunk{transfer_id, entry, data}` /
  `EntryVerified{transfer_id, entry}` / `EntryFailed{transfer_id, entry: Option<u32>, reason}` /
  `Violation{reason}`。

## 3. b-net / b-平台 / b-app（后续轮次）

- b-net：文件 TCP 生命周期仿 `clip_channel.rs`（host 首帧分派 FileBind / 双端泵 / 3s 独立退避 / 空闲 30s 关闭）；控制/剪贴板连接上收到文件域帧 = 剥出丢弃告警（域隔离防御面）。
- b-平台：四平台文件引用探测与挂板（docs/05 §0.2 矩阵；mac public.file-url / win CF_HDROP / X11 uri-list / Wayland mime）。
- b-app：`file_rt` 运行时 + 托盘「传输并粘贴」动作 + 暂存目录管理 + 剩余空间预检/阈值确认 + i18n + docs/06 §5.2 菜单更新（菜单顺序 golden 测试同步）。

## 4. 测试面（b-core）

`tests/file_transfer_flow.rs`：内存"文件"全链路（pull → serve 分块 → tx 信用窗口 → rx 组装 → EntryVerified，拼接 == 原内容、哈希一致）；校验矩阵（未知 id/越界下标/offset 超尺寸/跳号/累计超限/哈希不符/空闲超时 TestClock/并发上限）。
