# design：切片 a（元数据面）

> 实现地图来自全仓探索（2026-11-26）；wire 契约 docs/02 §4.6，剪贴板语义 docs/04 §8。
> 本切片交付**协议与状态机层**（transport 无关）；文件 TCP 通道与 app 泵接线属切片 b。

## 1. xcontrol-proto：`file` 模块

- 新文件 `src/file.rs`，照 `clip.rs` 模式：每消息 struct（serde derive）+
  `encode_payload`/`decode` + `impl Message` + `FileMsg` 枚举（`as_message()`）+
  `decode_file(kind, payload) -> Result<Option<FileMsg>, ProtoError>`。
- wire 类型逐字段照 docs/02 §4.6 钉死块（FileBind 0x58 … FileExpand 0x60）。
- 接收端解码层校验（信任边界在接收端）：
  - `FileRefAnnounce`/`FileExpand`：`entries.len() > 512` → 拒绝；`name` 含 `/`
    或超 256B UTF-8 → 拒绝；`total_bytes` > 协商上限 → 拒绝
    （`decode_with_limit`，默认 2GiB）。
  - `FileChunk`：`data.len() > 64KiB` → 拒绝。
  - 编码期：`encode_payload` 后超帧 payload 上限 → 编码期拒绝（清单单帧承载，
    物理上限先行）。
- `frame.rs`：kind 常量 `FILE_BIND..=FILE_EXPAND`（0x58–0x60）+ `KindClass::File`
  + `kind_class` 新臂 + `is_file_kind`；`kind_table_matches_docs` /
  `kind_class_covers_tables` 两个 golden 锚点测试同步（Unknown 负例值 0x55 → 换）。
- `version.rs`：`PROTO_MINOR` 1 → 2（doc 注释 + `current_version` 锚点）。
- `ctrl.rs`：`Caps::FILETRANSFER = 1 << 4` 入 KNOWN 掩码（caps 测试同步）。
- `error.rs`：新变体（仿 ClipTotalLenExceeded）：
  `FileEntriesExceeded` / `FileNameInvalid`（含 `/` 或超 256B）/
  `FileTotalBytesExceeded` / `FileChunkTooLarge`。
- `lib.rs`：mod 登记 + re-export + 模块导览表 + 分派示例 KindClass 补 File。
- `tests/fixtures.rs`：9 条 `file_*.bin` 样本 + 计数断言 26 → 35 +
  `Sample::File` 变体 + `decode_sample` 补 KindClass::File → `--ignored`
  生成测试刷新落盘。

## 2. xcontrol-core：`file_ref` 模块

新文件 `src/file_ref.rs`（与 clip_rx/clip_tx 同构，**与 Session 转移表无关**，
由连接接线层持有——同 clip 模块文档口径）：

- **发送侧 `FileManifestBuilder`**（纯函数式）：输入 `Vec<FileEntry>`（来自平台
  探测产物，slice a 由 mock 提供），校验上限（512 条/256B 纯名）→
  `Option<FileRefAnnounce>` 语义（超限返回 Err，发送端编码期拒绝语义对齐）；
  附 `manifest_fingerprint(&[FileEntry]) -> [u8; 16]`：`(name, size)` 有序序列
  blake3 截断（tag+len 布局防拼接碰撞，同 `content_fingerprint_input` 惯例）；
  **不含 mtime/绝对路径**（docs/04 §8.3）。
- **接收侧 `FileRefRx<C: Clock>`**：
  - 状态：`slot: Option<PendingSlot>`（单槽，最后复制者赢）+
    `last_seen: Option<([u8;16], u32)>`（origin 去重，防线 2 的文件域首实装）+
    `max_total_bytes: u64`（协商上限）。
  - `PendingSlot { transfer_id, origin_id, origin_seq, entries, total_bytes,
      copied_at_ms, fingerprint }`。
  - 入口：`on_announce(FileRefAnnounce) -> FileRefVerdict`
    （Applied{slot}/Duplicate/Rejected{reason}）；`invalidate(reason)` 供上层在
    本机/远端新剪贴板同步时调用（docs/04 §8.2 最后复制者赢——**失效由调用方
    显式触发**，本模块不嗅探剪贴板）。
  - 出口动作用 `FileRefVerdict`（区别于 clip 的 Vec<Action>：文件域 announce
    是幂等整包，无分块无流控）。
- `lib.rs` 登记 + re-export；blake3 用 core 既有白名单依赖。

## 3. mock 平台层（`mock-platform` feature）

`src/mock/clipboard.rs`：`MockClipboard` 增加 `set_file_refs(Vec<FileEntry>)` /
`file_refs()` 槽 + 变化计数纳入既有 `change_count()`（检测路径同构真实平台：
变化 → 探测 → 元数据）。数据形状直接用 proto `FileEntry`（core 已依赖 proto）。

## 4. 测试

- proto 内嵌单测：round-trip + 分派 + 上限负例矩阵（条目数/名称/`/`/total_bytes/
  chunk 超限/malformed）+ 编码期单帧承载（513 条目 → 编码 Err）。
- `tests/file_flow.rs`（仿 clip_flow 夹具纪律，`must` helper）：
  - 指纹规范形矩阵：mtime 变/相对序前缀（模拟路径差异）不变指纹；name/size
    变 → 指纹变；空清单边界。
  - 代次失效矩阵：文件→文件（覆盖）、同 origin 同 seq 重放（Duplicate 不重通知）、
    `invalidate` 后槽空、origin 去重跨 restart 语义。
  - 上限防御：超 512 条目、名称含 `/`、total_bytes 超协商上限 → Rejected。
- `xcontrol-net` scenarios：版本协商矩阵补 1.2 组合（AC4）——`PROTO_MINOR`=2 后
  既有 1.0/1.1 测试不动，negotiate 纯函数已覆盖，核对即可。
- 防御面：`KindClass::File` 帧出现在控制/剪贴板连接的处置属 app/net 接线
  （切片 b 泵接线时落地剥出丢弃）；本切片以 `kind_class` 单测锁定分类正确性。

## 5. 明确不做（切片边界）

- 文件 TCP 通道、FileBind 连接绑定、app 泵接线、托盘显示（切片 b——托盘提示
  与通道同批落地才有真实触发路径；本切片 FileRefVerdict 即上抛事件接口）。
- 真实平台探测（mac public.file-url / win CF_HDROP / X11 uri-list / Wayland
  mime）：mock 先行锁定语义，平台实装随切片 b 的通道联调同批（避免半接线状态）。
- PendingSlot 的过期 Clock：slice a 不做时间过期（代次失效已覆盖生命周期；
  TTL 属切片 d 打磨项）。
