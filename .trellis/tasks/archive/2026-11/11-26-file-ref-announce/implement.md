# implement：切片 a（执行清单）

> 顺序即执行顺序；每步后的验证命令必须过再进下一步。

## 1. proto：文件域消息

- [ ] `frame.rs`：kind 常量 `FILE_BIND`(0x58)–`FILE_EXPAND`(0x60)、`KindClass::File`、
      `kind_class` 臂、`is_file_kind`；`kind_table_matches_docs` 加断言、
      `kind_class_covers_tables` 加文件组 + Unknown 负例值更新（0x55 已占用）。
- [ ] `error.rs`：`FileEntriesExceeded`/`FileNameInvalid`/`FileTotalBytesExceeded`/
      `FileChunkTooLarge` 变体。
- [ ] `file.rs`：9 消息 + FileEntry/FileEntryKind/FileErrCode + FileMsg 枚举 +
      decode_file + 解码校验（512 条/256B 纯名无 `/`/total_bytes 上限/chunk 64KiB）
      + 编码期单帧承载校验 + 内嵌单测。
- [ ] `version.rs`：PROTO_MINOR=2 + doc + 锚点测试。
- [ ] `ctrl.rs`：`Caps::FILETRANSFER` bit4 + KNOWN + caps 测试。
- [ ] `lib.rs`：mod/re-export/导览表/分派示例。
- [ ] `tests/fixtures.rs`：9 样本 + Sample::File + 计数 35 → `--ignored` 刷新落盘。

验证：`cargo test -p xcontrol-proto && cargo clippy -p xcontrol-proto --all-targets -- -D warnings`

## 2. core：file_ref 模块

- [ ] `src/file_ref.rs`：FileManifestBuilder（上限校验 + 编码期拒绝对齐）+
      `manifest_fingerprint`（(name,size) blake3，tag+len 布局）+
      `FileRefRx<C: Clock>`（单槽 + origin 去重 + invalidate）。
- [ ] `lib.rs`：mod + re-export（mock-platform 之外也要可编译——file_ref 不依赖 mock）。
- [ ] `mock/clipboard.rs`：`set_file_refs`/`file_refs` 槽 + change_count 联动。
- [ ] `tests/file_flow.rs`：指纹矩阵 + 代次失效矩阵 + 上限防御 + mock 端到端
      （产清单 → announce → 对端 FileRefRx → Applied；再发文本 announce 语义由
      invalidate 调用方语义锁定）。

验证：`cargo test -p xcontrol-core --features mock-platform && cargo clippy -p xcontrol-core --all-targets --features mock-platform -- -D warnings`

## 3. net：版本矩阵核对

- [ ] `xcontrol-net` 版本协商测试补 1.2 组合断言（negotiate(1,1)↔(1,2)=1.1；
      min 语义回归）。

验证：`cargo test -p xcontrol-net`

## 4. 质量门（全绿后提交）

- [ ] `cargo +nightly fmt`（工作区）
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo test -p xcontrol-core --features mock-platform`
- [ ] `cargo run -p xtask -- check-deps`
- [ ] fixtures `git status` 确认 9 个新 .bin 入库

## 提交拆分（产品语义）

1. `feat(proto): XCP 1.2 文件域消息——FileBind/FileRefAnnounce 等九消息与 golden fixtures（FR-7.3）`
2. `feat(core): 文件引用清单构建与 pending 槽——代次失效与回环指纹（FR-7.3）`
3. （如有零散）`chore(trellis): 切片 a 工件`
