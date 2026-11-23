# M2: TCP 传输层与安全码握手实战

## Goal

`xcontrol-net` 真实 TCP 传输（tokio）——M1 回环验证过的协议栈跑上真网络。localhost 双角色集成测试实现 FR-1.4"安全码握手实战"的可自动化部分（真实 socket + 真实 Noise 握手 + 错误码拒绝路径）；两台 Mac 实机验收仍留人工。同时收尾 M1 遗留 CI 项。

## Requirements

1. **TCP 传输**（tokio）：`TcpTransport` 实现 M1 的 `Transport` 语义（异步化）——**长度前缀分帧适配器**（u32 BE 长度 + Noise 报文，M1 design 已预告）；半关闭/断开语义与 M1 回环版对齐；`TCP_NODELAY` 强制（docs/02 §0）。
2. **异步化边界**：net 引入 tokio（红线 3 允许：网络）；`HandshakeDeadline` 5s 超时在 async 下用 `tokio::time::timeout`；不改动 core/proto。
3. **安全码握手实战**（FR-1.4）：localhost 集成测试——真实 TCP 连接上完整 XXpsk0 握手成功路径；**错误安全码 → 握手失败 → 连接断开 → 日志记录**（无"部分匹配"反馈，复用 `HandshakeFailed` 折叠）；限速集成（throttle 挂到真实 peer addr）。
4. **端口绑定**：可配置端口（默认 4243）监听/连接 API 雏形（`serve()`/`connect()`，M2 只需 localhost 场景可跑）。
5. **CI 收尾（M1 遗留）**：`.github/workflows/ci.yml` 增加 llvm-cov 覆盖率步骤（linux，阈值 ≥85% fail-closed）+ fuzz 冒烟 job（linux nightly，`cargo fuzz build` 编译性检查 + 每目标 30s 冒烟运行，超时不算失败——注明原因）。
6. **测试**：TCP 分帧 round-trip、粘包/拆包场景（写半个帧再写余下）、localhost 握手成功/失败/限速三路径、传输断开传播。

## Acceptance Criteria

- [ ] localhost 双进程（或双任务）真实 TCP + Noise 握手全链路通；FR-1.4 错误码拒绝路径有测试断言
- [ ] 粘包/拆包测试通过（分帧适配器正确性）
- [ ] CI 文件含 llvm-cov 步骤 + fuzz 冒烟 job（本地可 `act` 不可跑，但 yaml 语法与步骤逻辑经复查）
- [ ] tokio 双登记 + 白名单；check-deps/clippy/fmt/test 全绿；net 覆盖 ≥85%
- [ ] proto/core 零改动（红线不动）

## Notes

- net → proto + config（+ dev core）依赖面不变；tokio 只进 net（及其 dev）。
- 真实两机验收（边缘切换/打字/拖拽）不在本任务——人工清单。
- 回环 Transport trait 保留（测试继续用）；TCP 是新增实现而非替换。
