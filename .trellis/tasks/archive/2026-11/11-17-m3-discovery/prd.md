# M3: 自动发现与断线重连

## Goal

`xcontrol-net` 发现层 + 重连层（docs/02 §2 全量）——从机自动找到主控、断了自动回来（FR-5.1/5.2/5.3）。本机 macOS 可全链路实测（mDNS/beacon 在本机回环环境真实工作）。

## Requirements

1. **mDNS 通告**（docs/02 §2.1）：`mdns-sd` 注册 `_xcontrol._tcp.local.`；TXT 记录 `proto=1.0`、`id=<instance-uuid>`、`name=<显示名>`、`caps=<hex>`、`port=4243`；可配开关。
2. **UDP beacon**（docs/02 §2.1）：每网络接口（跳过虚拟网卡，allowlist 可配）向**该接口子网定向广播地址**发送（`SO_BROADCAST` + bind 接口 IP）；周期 5s（未满配对）→ 60s（已满）；负载 `magic "XCD1" | proto u16 | port u16 | name_len u8 | name | id 16B`；**明文、无安全码派生物**（docs/02 §0 WPS 红线）。
3. **探测应答**：单播 `XCP1?`（同端口）→ 主控单播回应 beacon。
4. **从机发现流程**（docs/02 §2.2）：mDNS browse + 并发 UDP 探测（CIDR 逐 IP，300ms 收集，并发上限 128）+ 兜底 TCP 扫描（默认关）→ 按 instance id 去重 → 候选列表。
5. **断线重连**（FR-5.3）：指数退避 1s→30s；重连期间每轮刷新候选；节流（每候选每轮至多 1 次握手）。
6. **安全红线**：发现流量零安全码派生物（测试抓包断言 beacon/探测/mDNS TXT 内容）；beacon 解析对畸形包鲁棒（截断/坏 magic 直接丢弃不 panic）。
7. **实测**：本机双进程发现演练（host 通告 + client 扫描 → 发现 → 直连握手全链路）；beacon/beacon 应答在 lo 与真实接口的行为记录。

## Acceptance Criteria

- [ ] 本机双进程：host 启动通告 → client 发现 → 自动握手 → 会话建立（日志证明全链路）
- [ ] beacon 负载解析 round-trip + 畸形包负例（截断/坏 magic/超长 name）不 panic
- [ ] 发现流量零派生物断言测试
- [ ] 重连退避序列测试（1s→2s→4s…→30s 封顶；成功重置）
- [ ] mdns-sd/tokio 已在白名单（tokio 已有），check-deps/clippy/fmt/test 全绿；net 覆盖 ≥85%
- [ ] FR-5.1/5.2 AC 对照表在测试文件头

## Notes

- docs/02 §2.3 防火墙注意事项是打包任务（m3-packaging）的范围。
- 手动 IP 直连已由 M2 的 `--client <addr>` 支持，本任务补"候选列表 UI/日志呈现"的钩子（完整 UI 列表 M4 FR-5.5）。
- 本机无 AP 隔离环境，mDNS 多播在本机可用（实测为准；受限环境降级 beacon 的行为在代码注释说明）。
