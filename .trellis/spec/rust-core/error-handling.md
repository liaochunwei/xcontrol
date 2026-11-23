# 错误处理规范

来源：docs/08-engineering.md §2。

## 规则

1. **库 crate（proto/core/net/input/clipboard/config/tray）**：用 `thiserror` 定义错误枚举，错误变体携带可编程区分的语义（不是裸 String）。
2. **bin（app/cli）顶层**：`anyhow`，在 main 统一格式化输出。
3. **禁令**：任何可能由外部输入（网络帧、配置文件、平台回调数据）触发的路径禁止 `unwrap()/expect()`；测试代码豁免。CI 以 `clippy::unwrap_used` 强制（非测试代码）。
4. **错误传播不打断实时路径**：输入事件处理中的可忽略错误（如一次注入失败）→ `tracing::warn!` + 计数器，不得中断事件流；会话级错误（握手失败、协议违规）→ 显式错误类型上抛并断开。
5. **协议违规类错误**（帧越界、未知通道、clip 未声明先到）：crate 层用各自错误枚举的专用变体表达（如 proto 的 `ProtoError` 违规变体、core 的 `StateError::ProtocolViolation`），语义统一为**一律断开连接并记录**（docs/02 §7）；M1 定稿——不强制名为 `ProtocolViolation` 的独立类型，强制的是"违规→断开"行为与可编程区分的变体。
6. 错误信息禁止包含：安全码、剪贴板内容、完整密钥派生中间值。

## 示例

```rust
// 好：可编程区分 + 上下文
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("noise handshake failed within {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    #[error("peer rejected: rate limited")]
    RateLimited,
}

// 坏：外部输入路径 unwrap
let frame = decode_frame(&buf).unwrap(); // CI clippy 会拦
```
