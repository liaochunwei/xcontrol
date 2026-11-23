# design: mock 平台层与全链路回环验证

## mock 模块（core `#[cfg(feature = "mock-platform")]`）

```
crates/xcontrol-core/src/mock/
├── mod.rs          # feature 门控入口
├── input.rs        # ScriptedInputSource：Vec<InputEvent> 脚本 → 迭代产出；边界注入构造器
├── sink.rs         # RecordingInputSink：记录收到的注入动作供断言
└── clipboard.rs    # MockClipboard：内容 + 变更触发器；超限/超时脚本化
```

- feature 声明已在脚手架存在（`mock-platform`）；不进默认 features；core 依赖面不变（mock 只用 proto/config/std）。

## 信用窗口与重组（若 T3 未覆盖）

- 位置决策：`session.rs` 增加 `ClipRx` 状态（per session_id 重组缓冲：total_len 校验、累计长度检查、seq 窗口去重、信用计数、10s 超时由 Clock 驱动）——纯逻辑，违反协议 → `Action::Disconnect(ProtocolViolation)`。
- 发送侧信用：等待 `ClipAck` 归还；窗口空时 `step()` 返回 `Action::WaitAck`。

## 端到端测试拓扑

```
ScriptedInputSource → proto encode → Noise encrypt → LoopbackTransport → decrypt → decode
  → core Session/Route → RecordingInputSink 断言
（双端对称；剪贴板走第二对 Loopback + ChannelBind）
```

- 测试文件 `crates/xcontrol-net/tests/`（复用 T3 的 dev-dep core 授权）+ `crates/xcontrol-core/tests/`（纯逻辑重组/信用单测）。
- 覆盖清单注释置于 tests/mod.rs 头部（12 种 kind 一一对应测试函数名）。

## 基准报告

- 运行 `cargo bench -p xcontrol-proto` 与 `-p xcontrol-net`，摘录 P50/吞吐数据记入本任务 notes 与 journal；M6 性能验证以此为基线。
