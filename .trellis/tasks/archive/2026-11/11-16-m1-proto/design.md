# design: xcontrol-proto 帧层与全消息编解码

## 模块结构

```
crates/xcontrol-proto/src/
├── lib.rs        # crate 文档 + re-export
├── xkey.rs       # XKey 新类型 + 常量表 + 保留区校验
├── frame.rs      # 帧头编解码（FrameHeader { seq, kind, len }）、FrameKind 常量、decode 流式辅助
├── input.rs      # InputEvent 定长手写编解码（docs/02 §4.2 kind 表 1:1）+ MouseMoveRel 合帧辅助
├── ctrl.rs       # CtrlMsg postcard 枚举 + ProtoVer/Caps/ScreenInfo/Options/ErrCode
├── clip.rs       # ChannelBind/ClipAnnounce/ClipChunk/ClipAck/ClipEnd（postcard）
├── version.rs    # negotiate() + 兼容承诺常量（MAJOR=1）
└── error.rs      # ProtoError（thiserror：Frame/Length/Text/Key/Version/Postcard）
```

## 关键契约

- 帧头 7 字节定长：`seq: u32 LE | kind: u8 | len: u16 LE`；payload 上限常量 `MAX_FRAME_PAYLOAD = 64 KiB`（与 ClipChunk 上限对齐，docs/02 §6）。
- kind 常量以 `mod kind` 内 `pub const KEY: u8 = 0x01;` 形式定义（docs/02 §4.2 表逐字对应）；InputEvent ↔ kind 双向转换集中一处。
- 定长编解码手写 `encode(&self, out: &mut Vec<u8>)` / `decode(&[u8]) -> Result<(Self, usize)>`；不 derive serde（热路径零依赖抽象）。
- `Caps` 位掩码位定义照 docs/05-platform §0 对齐（bit 顺序引用文档，不发明）。
- Clip 消息同为 postcard；`ClipAnnounce.total_len` 校验、`ClipChunk.data ≤ 64KiB` 在 decode 层强制。

## 错误策略

- thiserror 单一 `ProtoError`；变体携带偏移/字段路径；库代码零 unwrap/expect（外部输入全部 Result）。

## 依赖与闸门

- 新增：`postcard`（含 serde 传递）。双登记：根 `[workspace.dependencies]` + xtask `RED_LINE_ALLOWED_DEPS` proto 条目。
- serde 仅经 postcard 传递出现，不直接依赖（CtrlMsg derive serde 供 postcard 需要——postcard 支持 serde derive；若需直接 serde derive 则 serde 也进白名单并注明）。
- criterion 仅 dev-dep（benches），白名单 dev 条目注明。

## 兼容性

- major=1 内只增不改（docs/02 §5.4）；未知 kind 按 len 跳过在 frame decode 层实现，调用方拿到 `(kind, payload)` 自行决定。
