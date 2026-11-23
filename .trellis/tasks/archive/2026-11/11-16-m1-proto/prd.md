# M1: xcontrol-proto 帧层与全消息编解码

## Goal

实现 XCP v1 协议的全部 wire 编解码（docs/02 §4–§8）——M1 第一块基石，后续 core/net 全部建立在其上。`xcontrol-proto` 是零 I/O、零平台依赖红线 crate，必须能在普通 `cargo test` 全量跑。

## Requirements

1. **XKey 体系**（docs/03 §2）：`XKey(u16)` 新类型 + 全量常量表（键盘页 0x04–0xE7 `KEY_*`、消费者页 0x100–0x1FF `CK_*`、指针区 0x200–0x2FF），保留区构造拒绝。
2. **帧层**（docs/02 §4.1）：帧头 `seq: u32 | kind: u8 | len: u16 | payload` 小端；编解码 API；未知 kind 按 len 跳过；len 越界必须报错。
3. **输入事件**（docs/02 §4.2 全量 12 种）：定长手写小端（不用 postcard）；`Text` ≤512B 超长报错；`MouseMoveRel` 合帧辅助（delta 累加 ≤8 条/窗口）；键盘/按钮严禁合并重排（只提供合帧辅助，不做自动重排）。
4. **控制消息**（docs/02 §4.3）：`CtrlMsg` postcard 枚举全量 + `ProtoVer`/`Caps` 位掩码/`ScreenInfo`/`Options`/`ErrCode`。
5. **剪贴板消息**（docs/02 §4.4/§6 + docs/04 §3）：`ChannelBind`、`ClipAnnounce`、`ClipChunk`（≤64KiB）、`ClipAck`、`ClipEnd` 编解码。
6. **版本协商**（docs/02 §5）：`negotiate()` major 不等即 Err、minor 取 min。
7. **测试**（docs/02 §8）：golden fixtures（`tests/fixtures/xcp/*.bin` 每消息 ≥1 个）；round-trip 全消息；版本矩阵（1.0↔1.1↔2.0）；未知 kind 跳过；恶意帧负例（超长 len/截断）不 panic。
8. **fuzz 脚手架**：`fuzz/` targets（帧解码、ClipAnnounce），只建不进 CI。
9. **criterion 基准**：`benches/` 编解码吞吐（输入事件 + CtrlMsg）。

## Acceptance Criteria

- [ ] 全部输入 kind / CtrlMsg / Clip 消息 round-trip 等价 + golden fixture
- [ ] 版本协商矩阵测试通过
- [ ] 恶意流负例正确报错/跳过，库 crate 零 unwrap（外部输入路径）
- [ ] 行覆盖 ≥85%
- [ ] postcard 新依赖双登记，check-deps 全绿
- [ ] `cargo build/test/clippy/fmt` 全绿；基准可运行

## Notes

- 红线：proto 零 I/O、零平台 API、零 tokio；`#![forbid(unsafe_code)]` 维持。
- wire 契约以 docs/02 为准（docs/03 §3 已改为语义模型）；发现 docs/02 缺口先改文档再改代码。
- fuzz targets 依赖 cargo-fuzz（nightly），仅 scaffolding 并注明运行方法。
