# design: 自动发现与断线重连

## 模块增量（xcontrol-net）

```
src/
├── discovery.rs     # beacon 编解码（XCD1 帧格式逐字 docs/02 §2.1）+ 探测 XCP1? 应答
├── announce.rs      # Announcer：mDNS 注册（mdns-sd）+ beacon 周期广播（接口枚举/跳虚拟网卡）+ 周期切换（5s/60s）
├── browse.rs        # Browser：mDNS browse + UDP 并发探测（CIDR 展开/128 并发/300ms 窗）+ 去重 → 候选流
└── reconnect.rs     # 重连状态机：退避序列 1s→2s→…→30s 封顶 + 成功重置 + 每轮刷新候选（纯逻辑 + 时钟注入可测）
```

## 关键决策

- **async 归属**：发现层全部 tokio（mdns-sd 自带后台线程，beacon 用 tokio UdpSocket）；Announcer/Browser 提供 async API；重连状态机同步纯逻辑（Clock 注入，测试用 TestClock）。
- **接口枚举**：`tokio::net` 无接口枚举——用 `if-addrs` crate（轻量纯 bindings，查传递树）或 `get_if_addrs`；虚拟网卡过滤：名字启发式（lo/*/utun/docker/vpn）+ allowlist。选型实现时以 crates.io 实际为准，进白名单前查树。
- **定向广播地址计算**：接口 IP + 前缀长度 → 广播地址（纯函数可测：10.0.0.5/24 → 10.0.0.255）；无前缀信息时 /24 兜底 + 注释。
- **候选 → 握手衔接**：Browser 产出 `Candidate { addr, id, name, source }`；重连层消费候选按序尝试 connect（M2 serve/connect API）+ throttle（已有 per-IP）。
- **app 接线**：`--host` 自动开 Announcer（可 `--no-discover` 关）；`--client` 无地址时进入 discover 模式（手动 IP 仍是显式参数优先）。

## 测试策略

- beacon 编解码 round-trip + 畸形负例（纯函数）。
- 广播地址计算表驱动测试。
- 重连退避：TestClock 驱动序列断言。
- 本机集成（#[ignore] 或默认跑）：Announcer + Browser 同机环回（mDNS 本机自见/loopback beacon）→ 候选非空 → connect 握手成功。

## 依赖

`mdns-sd`、`if-addrs`（或同类，查树后定）——双登记 + net 白名单并入既有条目（单条目规则）。
