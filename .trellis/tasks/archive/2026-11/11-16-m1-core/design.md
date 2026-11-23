# design: xcontrol-core 会话状态机与事件路由

## 实现调整（check 阶段补记，与实现保持一致）

设计稿为草拟形态，实现做了以下偏离，均为收敛而非方向变更：

1. **`SessionEvent` 增加 `HandshakeReady { hs_hash: [u8; 32] }` 变体**：设计稿只列
   Frame/Timer/Local 三类。实现中 net 层 Noise 握手完成经此事件进入 `Idle →
   Handshaking`，并携带握手摘要供 `derive_session_id`（docs/02 §3.3）在状态机内
   派生 session_id——摘要生命周期与会话绑定，由 core 管理比由 net 旁路传递更内聚。
2. **`Session<C: Clock>` 泛型化**：设计稿的 `Session` 为具体类型。时钟注入是 prd
   硬性要求（心跳 3s/9s 可测），泛型使测试注入 `TestClock`、生产注入
   `SystemClock`，core 内零系统时间直取。
3. **动作类型定名 `SessionAction`**（设计稿草写 `Action`），并拆出两个子枚举：
   `Outbound`（Ctrl/Input 出站载荷 + kind/encode_frame_into 助手）与 `Notice`
   （状态迁移对外可见事件）。`Send` 的 `seq` 由会话内单调计数器分配。
4. **`LocalCmd` 扩展**：设计稿草写 Pause/Resume/Switch/Quit 四种，实现增加
   `EdgeCrossing`（宿主侧 `edge_crossing()` 检测后的穿越上报）与 `LocalMods`
   （捕获侧修饰键快照直通，出站 LeaveScreen/EnterScreen/ModifierSync 载荷来源）。
5. **`route.rs` 定名 `Dispatcher`**（设计稿草写 FrameDispatcher），输出
   `FeedOutcome { actions, fatal }`：解码失败/非法转移自动附加违规断开序列
   （`Error{ProtocolViolation}` + ReleaseAll + Notice），调用方只执行动作并关连接。
6. **`Disconnected(DisconnectReason)` 五种原因**：Timeout / VersionIncompatible /
   ProtocolViolation / PeerError / LocalQuit（设计稿草写 Reason 单数）。
7. **断开统一收尾（check 阶段补入）**：任何进入 `Disconnected` 的转移统一产出
   `ReleaseAll` + 清注入基准 + `Notice::Disconnected`（docs/03 §3 防卡键不变量的
   断开分支；初版实现仅在暂停/切出释放，断开路径漏发，check 修复）。
8. **docs 未定义语义的裁定**共 6 处，全部写入 `session.rs` 模块文档「语义裁定」节
   并配测试：EdgeCrossing 成对动作（ReleaseAll → LeaveScreen+EnterScreen）、收到
   LeaveScreen 释放全部、收到 Pause 置暂停+释放、SetOptions 0 值字段视为缺失、
   SwitchRequest 默认 Ack ok=true、断开统一 ReleaseAll。

## 模块结构

```
crates/xcontrol-core/src/
├── lib.rs         # re-export + crate 文档
├── session.rs     # Session 状态机（含 Paused 子态、心跳计时语义）
├── clock.rs       # Clock trait（毫秒时钟注入）+ TestClock
├── layout.rs      # ScreenLayout（Vec<ScreenInfo> 网格）+ edge_crossing() / to_abs() 纯函数
├── route.rs       # FrameDispatcher：帧 → SessionAction（转发给 InputSink / 状态变更 / 忽略）
├── sink.rs        # InputSink / InputSource trait（docs/08 §2 命名）
└── mods.rs        # ModifierSet 跟踪 + 对齐/释放/周期校验差异计算
```

## 状态机契约

```rust
enum SessionState { Idle, Handshaking, Running { paused: bool }, Disconnected(Reason) }
enum SessionEvent { Frame(Frame), Timer(ClockTick), Local(LocalCmd) }   // LocalCmd: Pause/Resume/Switch/Quit
// step(&mut self, event) -> Result<Vec<Action>, StateError>
```

- 转移表集中在 `fn transition()` 单函数（可枚举测试全组合）；Paused 中收输入帧 = `StateError::PausedReject`。
- 心跳：`Running` 进入时请求 `Clock` 计时；`last_frame_ts` 更新于任何收帧；`9s` 判死 → `Disconnected(Timeout)` + 通知动作。
- session_id：`session.rs::derive_session_id(hs_hash: &[u8;32]) -> [u8;16]`（BLAKE3 若入 core 则白名单；否则由 net 传入——design 决策点：**blake3 入 core 白名单**，因 session_id 属会话语义且 blake3 纯软无 I/O）。

## 与 proto 边界

- core 只消费 proto 类型（Frame/InputEvent/CtrlMsg），不重复定义 wire 结构。
- Layout 下发（`CtrlMsg::Layout`）更新 `ScreenLayout`；edge_crossing 输出 `EnterScreen` 帧所需的全部字段。

## 测试策略

- 转移表全组合枚举测试（状态 × 事件）；虚拟 Clock 手动推进（3s Ping / 9s 判死）；布局用 2×1 与 3 屏 L 形两套 fixture。

## 依赖

- 新增 `blake3`（根 deps + core 白名单条目，注明 session_id 用途）。其余零新增。
