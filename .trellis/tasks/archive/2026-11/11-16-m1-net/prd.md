# M1: xcontrol-net Noise 加密与回环传输

## Goal

`xcontrol-net` 的加密与传输层：PSK 派生、Noise XXpsk0 握手、传输抽象 + in-memory 回环实现、回环延迟基准（NFR-1 分量 P50 < 5ms）。真实 TCP socket 属 M2+ 会话管理范畴，本任务不做。

## Requirements

1. **PSK 派生**（docs/02 §3.1）：`salt = BLAKE3-derive-key("xcontrol/salt-v1", instance_uuid)[0..16]`；`p1 = Argon2id(code, salt, m=19MiB, t=2, p=1)`；`psk = BLAKE3-derive-key("xcontrol/psk-v1", p1)`；Argon2 结果可缓存（派生一次）；纯函数 + 缓存包装。
2. **Noise 握手**（docs/02 §3.2）：snow `Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s`，`Builder::psk(0, psk)`，prologue `b"xcontrol/1"`；initiator/responder 双侧 API；握手超时 5s；成功后产出 `TransportState` 封装（加解密 API）+ `session_id`（BLAKE3(hs_hash)[0..16]，调 core 的 `derive_session_id` 或本地等价实现）。
3. **传输抽象**：`Transport` trait（async 或同步按设计取舍）+ `LoopbackTransport`（in-memory channel 双端）；支持模拟延迟/断开注入（测试钩子）。
4. **防爆破策略**（docs/02 §3.2）：失败握手计数器（per-peer-key 泛型，回环用 peer id；真实 IP 绑定 M2/M3）——5 次失败 → 指数退避 10s 起封顶 10min；从机侧 ≥2s/次自我限速；计数器纯逻辑可测。
5. **全链路回环集成测试**：mock 输入序列 → proto 编码 → Noise 加密 → 回环传输 → 解密 → 解码 → 断言事件到达（dev-dep core + proto；xtask 白名单加 dev 条目并注明"集成测试专用"理由）。
6. **criterion 基准**：回环往返延迟（含 Noise 加解密）——验证 P50 < 5ms。
7. **错误处理**：握手失败不泄露"密码错误"信号（对端只见断开，docs/02 §0 WPS 教训）；错误枚举不区分失败原因给对端。

## Acceptance Criteria

- [ ] PSK 派生向量测试（固定输入 → 固定 psk；文档记录测试向量）
- [ ] 握手成功/超时/安全码不匹配三路径测试（回环双端）
- [ ] 加密后明文不可见（回环捕获 channel 数据断言无明文特征）
- [ ] 防爆破计数器：5 次失败退避、窗口重置测试
- [ ] 全链路回环集成测试通过；延迟基准 P50 < 5ms 实测记录
- [ ] snow/argon2/blake3/tokio(如用) 双登记，check-deps 全绿
- [ ] `cargo build/test/clippy/fmt` 全绿；行覆盖 ≥85%

## Notes

- net 允许 tokio（architecture-rules 红线 3：tokio 用于网络）——同步/异步取舍在 design.md 决定并说明；回环 transport 若同步实现可零 tokio，基准更纯粹，优先评估同步方案。
- argon2 m=19MiB 在测试中开销可接受（~20ms 级），派生缓存避免重复。
- 不做发现（mDNS/beacon，M3）、不做真实 TCP、不做断线重连。
