# 切片a：FileRefAnnounce 元数据同步与托盘指示

> 父任务：11-26-file-transfer（design.md 是技术源材料）。
> 协议依据：docs/02 §4.6（XCP 1.2，已落稿）；剪贴板语义：docs/04 §8（已落稿）；
> Caps：docs/05 §0.1 bit4；安全红线：docs/07 §9。

## Goal

复制端检测「文件引用剪贴板」→ 广播 `FileRefAnnounce` 元数据清单 → 对端 pending 槽
+ 托盘提示。复制瞬间零内容传输（清单 < 1MiB），为切片 b/c 准备数据面。

## Requirements

- R1 文件引用检测：平台剪贴板读侧新增"文件引用"探测（mock 平台层先行；
  真实平台路径按 docs/05 §0.2——mac `public.file-url` / win `CF_HDROP` /
  X11 `text/uri-list` / Wayland mime，量力落地，未落地平台如实标注）。
- R2 源端元数据清单构建（core，平台无关）：条目（相对名/大小/mtime/是否目录/
  children 估计）、上限校验（**单帧承载**：条目 ≤512、名称 ≤256B UTF-8 纯名——
  docs/02 §4.6 防护规则 1，2026-11-26 物理上限自查修正版）；源端不读文件内容。
- R3 `FileRefAnnounce` 协议实装（xcontrol-proto `file` 模块）：kind 0x58–0x60
  全量消息 + wire 钉死 + golden fixtures；`PROTO_MINOR` 1 → 2；消息集随会话
  minor ≥ 2 门控。
- R4 对端 pending 槽状态机（core）：清单入槽、托盘提示事件上抛；**代次失效**
  ——任何新 `FileRefAnnounce` 或本机/远端新剪贴板同步（text/png/html）使旧槽
  失效（docs/04 §8.2 最后复制者赢）。
- R5 防回环指纹（core）：文件清单指纹规范形 = `(name, size)` 有序序列 blake3，
  **不含 mtime/绝对路径**（docs/04 §8.3）；接入 EchoFilter 既有三防线结构。
- R6 能力协商：`CAP_FILETRANSFER`（docs/05 §0.1 bit4）进 Caps 位表；对端未声明
  不发送（发送端过滤）。
- R7 分层红线：proto/core 零平台依赖零 I/O；golden fixtures 同步（含新 kind）；
  质量门全绿。

## Acceptance Criteria

- [x] AC1 proto：9 消息 round-trip + decode 分派 + 上限校验负例（条目数 512/
      名称 256B/名称含 `/` 与 `.`/`..`/NUL 拒绝/总字节 u64 上限）；`file_*.bin`
      golden fixtures 入库（26→35）并比对通过。
- [x] AC2 core 单测：清单指纹——mtime/kind/children 变化不改指纹，name/size/
      顺序变化改指纹，len 前缀防拼接碰撞；pending 槽代次失效矩阵（同源重放
      Duplicate、低 seq 过期拒绝、跨来源覆盖、invalidate 清槽）。未知
      transfer_id/越界下标防御随切片 b 拉流面实装（design.md §5 边界）。
- [x] AC3 mock 端到端：mock 剪贴板产文件引用 → 清单构建 → 帧级 wire 往返 →
      pending 槽 Applied（tests/file_flow.rs）；本机文本写入 → invalidate 失效锚点。
- [x] AC4 版本门控：协商矩阵补 1.2 组合（proto version.rs）；会话 minor < 2
      不发送文件域——切片 a 无文件域发送路径（通道属切片 b），门控随
      `FileRefAnnounce` 发送路径在切片 b 落地（net chain 断言改锚 PROTO_MINOR）。
- [x] AC5 质量门：check-deps PASS、`clippy --workspace --all-targets -D warnings`
      零警告、`+nightly fmt --check` 干净、`cargo test --workspace` 32 套件全绿
      （含 mock-platform 4 套件）。

## 交接备注（2026-11-26 完成）

- 真实平台文件引用检测（mac/win/x11/wayland）未实装——按 design §5 移交切片 b
  与通道联调同批（避免半接线状态）；mock 语义已钉死（写入清空多域、变更计数联动）。
- 附带修复：session.rs `Layout` 下发门控由 `PROTO_MINOR` 改为特性级
  `LAYOUT_MIN_MINOR`（1.1）——原实现会因 minor 提升误停 1.1 对端布局下发。
- net 层协议版本相关测试改锚 `PROTO_MINOR`，后续 minor 提升不再涟漪。

## Out of Scope

- 内容字节传输（切片 b）；粘贴拦截（切片 c）；真实平台检测若本切片未全覆盖，
  剩余平台路径留 follow-up（prd R1 的"量力落地"边界，交接时如实标注）。
- 托盘菜单动作（传输并粘贴 = 切片 b）；进度 UI（切片 d）。
