# implement：规范先行（执行清单）

> 技术源材料：父任务 `11-26-file-transfer/design.md`。以下顺序即执行顺序。

## 0. 前置

- [ ] 通读 docs/00 §4.7/§6、docs/02 §4.3/§4.4/§6/§8、docs/04 全文、docs/05 §0/§8、docs/07 §5–§8，确认插入点与既有编号惯例。

## 1. ADR-0004

- [ ] 新建 `docs/adr/0004-file-transfer-channel-and-materialization.md`（模板：背景/备选方案/决策/后果；备选须含「复用剪贴板 TCP」「控制连接复用（11-21 教训）」两个被否方案）。
- [ ] `docs/adr/README.md` 索引表加行。

## 2. docs/00（需求）

- [ ] §4.7 表新增 FR-7.3（跨设备文件复制粘贴：复制端元数据清单、粘贴时拉流物化；说明与 FR-4.6/7.1/7.2 关系）。
- [ ] §6 非目标第 3 条修订：由「v1 不做文件内容的网络传输」改为保留 v1 边界、指向 FR-7.3 规划（表述自洽）。
- [ ] FR-4.6 行补备注：FR-7.3 落地后其降级形态（对端能力不匹配时仅同步路径文本）。

## 3. docs/02（协议，XCP 1.1 → 1.2）

- [ ] §4.1 后新增 §4.6 文件通道：连接模型（第三条 TCP、FileBind 绑定、按需建立/空闲关闭）、kind 分配表 0x58–0x5F。
- [ ] wire 类型钉死块（postcard Rust 类型）+ 与 Clip 参数差异（u64 总量、分块空闲超时、信用窗口、可重试 FileError、清单上限）。
- [ ] 防护规则节：条目索引寻址红线、接收端逐帧/总量校验、信任边界原则沿用。
- [ ] 修订记录（2026-11-26，任务 11-26-file-transfer-spec；兼容性论证：新 kind 随 minor 1.2 提升，1.1 对端未知 kind 跳过）。
- [ ] §5 版本协商无结构变化（确认 1.2 只增 kind，不改既有 wire）。
- [ ] §8 测试要求补文件域 fixtures/fuzz 项。

## 4. docs/04（剪贴板）

- [ ] 新增 §8 文件引用剪贴板（FR-7.3）：检测格式矩阵（引 docs/05）、pending 槽与代次失效、物化流程（stage-then-mount）、指纹规范形（名字+大小，不含 mtime/绝对路径）、与 §3 推送决策的辨析。
- [ ] §2 线格式表补一行说明：文件引用不走 ClipFormat（独立文件域），仅元数据。

## 5. docs/05（平台）

- [ ] 新增节：四平台文件引用读（检测）/写（挂板）路径矩阵 + 依赖邻域（clipboard-win/objc2/x11-clipboard/wl-clipboard-rs）。
- [ ] 实机复核项录入 §8：mac Finder 复制文件 → TextEdit 粘贴行为；NSFilePromiseReceiver 粘贴路径支持（spike 候选）；CF_HDROP 长路径/Unicode。

## 6. docs/07（安全）

- [ ] 新增 §（文件传输面）：条目索引寻址防任意读（红线）、元数据最小化、磁盘耗尽与阈值确认、清单/条目数/深度上限、信任边界沿用 §6 原则。
- [ ] §8 发布自查清单补文件传输项（v0.2 生效时勾选）。

## 7. 收尾

- [ ] docs/09 延后池条目更新：FR-7.3 已立项（任务树 11-26-*），ADR-0004 已落档，链接父任务。
- [ ] CHANGELOG Unreleased / Added：规范条目（ADR-0004、FR-7.3、XCP 1.2 预留）。
- [ ] `.trellis/spec/` rust-core / rust-platform 卡检查，需要则同步文件域指针。
- [ ] 一致性校验（见 check 清单）→ 分批提交。

## 校验命令

```bash
grep -rn "0x5[89abcef]\|0x58" docs/ | grep -v 0004   # kind 空间无冲突
grep -rn "FR-7.3" docs/ | wc -l                       # 编号引用落位
grep -rn "11-26-file-transfer" docs/ .trellis/spec/   # 任务号可追溯
```
