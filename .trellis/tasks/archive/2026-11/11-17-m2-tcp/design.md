# design: TCP 传输层与安全码握手实战

## 模块增量（xcontrol-net）

```
src/
├── transport.rs   # + TcpTransport（tokio TcpStream 封装）：send/recv 分帧适配
├── framing.rs     # 长度前缀编解码：u32 BE len + Noise 报文；MAX_NOISE_MSG=65535+4 头
├── serve.rs       # serve(addr, psk|code) -> 监听 + 每连接握手 spawn；connect(addr, code)
└── (noise/psk/throttle 复用 M1)
```

## 关键决策

- **异步化策略**：`TcpTransport` 只实现 async 版本（`impl TcpTransport` 固有方法，不强行统一进同步 `Transport` trait——两条并存，M1 回环 trait 服务于测试与 core 的纯逻辑演示）；async 化的整体统一（trait 重构）延后到真实需要时，避免现在破坏 M1 测试。
- 分帧：`len: u32 BE`（Noise 报文 ≤65535，u16 也够，但 u32 给未来留余量并注明）；读侧 `read_exact` 两段式（4B 头 + len 载荷），天然处理粘包/拆包。
- serve/connect 返回 `(SecureTransport, session_id)` + 对端地址；握手在连接后立即执行（超时 5s 经 `tokio::time::timeout`）。
- 错误码路径：握手失败 → 记录（对端只见断开）→ throttle 按 `SocketAddr` 计数。

## tokio 引入

- `tokio = { version = "1", default-features = false, features = ["rt-multi-thread", "net", "io-util", "macros", "time", "sync"] }`——检查传递树（mio/windows-sys 属合法平台依赖，net 非全树黑名单对象，GATED_CRATES 白名单登记即可）。

## CI 收尾

- llvm-cov：linux job `taiki-e/install-action@cargo-llvm-cov` + `cargo llvm-cov --workspace --summary-only --fail-under-lines 85`。
- fuzz 冒烟：nightly job，`cargo install cargo-fuzz --locked` + 对每 target `timeout 30 cargo fuzz run <t> -- -max_total_time=20`；编译失败算红，运行期发现 crash 输入算红，超时属正常（注释注明）。

## 测试设计

- 分帧：编码→字节流切片模拟粘包（一次 write 全部、分两次 write）→解码还原。
- localhost 握手：`tokio::spawn` 双端，127.0.0.1:0（随机端口）；成功/错码/限速（同 addr 连打 6 次）。
