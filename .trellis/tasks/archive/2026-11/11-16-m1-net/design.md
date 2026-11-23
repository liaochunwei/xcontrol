# design: xcontrol-net Noise 加密与回环传输

## 同步 vs 异步决策

**优先同步（零 tokio）**：回环 channel 用 `std::sync::mpsc` 或 crossbeam；理由：① M1 无真实 socket，async 无收益；② 加解密本身 CPU-bound；③ 基准更纯净；④ M2 引入真实 TCP 时再评估把 Transport trait 升级为 async（trait 设计预留 `poll`-friendly 边界，即读写方法分离、内部无阻塞 sleep——超时由调用方控制）。若实现中发现 snow 握手与 channel 组合确需 async，可引入 tokio 并在此文档补记理由。

## 模块结构

```
crates/xcontrol-net/src/
├── lib.rs
├── psk.rs        # derive_psk(code, instance_uuid) -> [u8;32]（blake3×2 + argon2id）+ PskCache
├── noise.rs      # NoiseHandshake（initiator/responder，5s 超时由调用方时钟控制）+ SecureTransport 封装 snow TransportState
├── transport.rs  # Transport trait（send/recv/try_recv/shutdown）+ LoopbackTransport（channel 双端 + 延迟/断开注入）
├── throttle.rs   # 握手限速计数器（per-peer，泛型 key：peer id/IP）
└── error.rs      # NetError（thiserror；握手失败统一 HandshakeFailed 不泄露原因）
```

## 关键契约

- `NoiseHandshake::run(io, role, psk) -> Result<SecureTransport>`：握手报文经 Transport 收发；成功返回传输态封装（`encrypt/decrypt`）+ `session_id`。
- snow API：`Builder::with_params("Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s".parse()?).psk(0, &psk)`；prologue 常量 `PROLOGUE: &[u8] = b"xcontrol/1"`。
- `LoopbackTransport::pair(delay: Option<Duration>)` 返回双端 + 控制句柄（注入断开）。
- SecureTransport 消息边界：snow packet 流自带帧边界（snow 传输态 write_message 产出自分帧报文），无需再叠加帧层——文档注明与 §4.1 应用帧的关系（应用帧在密文内）。

## 测试向量与钩子

- psk.rs：固定 (code="test-code", uuid=[0u8;16]) → 记录 16 字节样例 hex 作回归断言（不含真实安全码）。
- 回环集成测试放 `crates/xcontrol-net/tests/chain.rs`：dev-dep `xcontrol-core`（mock sink）+ `xcontrol-proto`；xtask 白名单 dev 条目 `("xcontrol-net", dev: xcontrol-core, "chain integration tests only")`。

## 依赖与闸门

新增：`snow`、`blake3`（已在 T2 登记则复用）、`argon2`、`criterion`(dev)。全部双登记 + 白名单；argon2 默认 feature 检查（避免引入不必要的 SIMD/并行 crate——用 `std` feature 最小组合）。
