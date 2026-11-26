# 切片a：FileRefAnnounce 元数据同步与托盘指示

> 父任务：11-26-file-transfer；技术设计见父 design.md；协议依据 docs/02 §4.6（XCP 1.2，本切片前须已实装）。

## Goal

复制端四平台检测「文件引用剪贴板」→ 广播元数据清单 → 对端 pending 槽 + 托盘提示。
复制瞬间零内容传输（清单 < 16KiB），为切片 b/c 准备数据面。

## Requirements（开工时细化）

- R1 四平台文件引用检测（复用既有监听器：mac changeCount / win WM_CLIPBOARDUPDATE / X11 XFixes / Wayland data-control），识别 public.file-url、CF_HDROP、text/uri-list 等格式（docs/05 矩阵）。
- R2 源端 stat 元数据（名字/大小/mtime/是否目录），顶层清单化，条目数/深度/总字节上限（docs/02 防护值）。
- R3 `FileRefAnnounce` 广播与 origin 代次去重接入（复用 docs/04 §5 防线 2）。
- R4 对端 pending 槽：任何新 announce（含文本/图片格式）使旧文件槽失效；托盘提示「剪贴板文件：N 项 / X MB（来自 <机器名>）」。
- R5 协议实现零平台依赖（xcontrol-proto/core 红线）；golden fixtures 同步（docs/02 §8）。

## Acceptance Criteria（开工时细化）

- [ ] mock 平台层端到端：A 产文件引用清单 → B pending 槽出现且托盘提示；A 再复制文本 → B 槽失效。
- [ ] 防回环单测：清单指纹规范形（名字+大小，不含 mtime/绝对路径）三防线故障注入。
- [ ] 质量门：check-deps、clippy -D warnings、fmt、workspace tests 全绿。

## Out of Scope

- 任何内容字节传输（切片 b）；粘贴拦截（切片 c）；目录递归展开（切片 d）。
