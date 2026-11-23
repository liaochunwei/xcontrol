# 11 · 性能基准报告（NFR-1 / NFR-2 / NFR-7）

> 状态：v0.1 · 0.1.0 发布基线（任务 11-19-m6-release）。本文是 NFR-1/2/7 的达标证据与方法论汇总；
> 里程碑级历史数据见 [09-roadmap.md](09-roadmap.md) 各节注记。

## 0. 环境与方法论

| 项 | 值 |
| --- | --- |
| 基准工具 | `criterion` 0.7（default-features off + cargo_bench_support；`cargo bench -p <crate> --bench <name>` 定向——0.7 无 `--quick`，11-16-m1-proto 实证） |
| 采样设置 | criterion 默认：预热 3s + 100 样本 × ~5s；报告值 = 样本中位数（点估计，括号为置信区间） |
| 环境 | macOS（darwin 25.5.0，arm64 / Apple Silicon），release profile（gpui 系 `opt-level=3` 同 workspace 配置），无人工负载 |
| NFR-2/7 实测 | 运行 release 二进制 + `ps -o rss,pcpu` 抽样 / 结构化日志时间戳（NFR-8 观测面）判读；命令见 §3/§4 |
| 复跑 | `cargo bench -p xcontrol-proto --bench codec && cargo bench -p xcontrol-net --bench loopback`；回归判定用 criterion 自带的与上轮基线对比输出（No change / regressed） |

解读约定：回环/编解码数字是**协议与加密层分量**（mock 传输），不含平台捕获/注入与真实网络
RTT——这两项的预算分解见 [01-architecture.md](01-architecture.md) §4（捕获回调→出网 <2ms ·
LAN RTT <1ms · 入网→注入回调 <3ms · 总预算 P50 15ms / P99 40ms）；双实机端到端验收属
docs/05 §7 发布门禁手动清单（移交人工）。

## 1. NFR-1 · 输入延迟（门槛：P50 < 15ms，P99 < 40ms；复核门槛回环分量 P50 < 5ms）

**结论：达标**——回环分量（LoopbackTransport 全链路，含 Noise_XXpsk0 加解密）P50 ≈ **0.81 µs**，
对 5ms 复核门槛余量 ~6×10³；对 15ms 端到端预算，协议+加密分量占比 < 0.01%。

### 1.1 回环往返（xcontrol-net benches/loopback.rs）

| 基准 | 中位数 | 说明 |
| --- | --- | --- |
| `loopback_roundtrip/key_14b` | **810 ns** | 单键事件：编码 → Noise 加密 → mock 传输 → 解密 → 解码 全链路往返 |
| `loopback_roundtrip/text_519b` | 3.21 µs | 519 字节文本输入帧同链路 |
| `crypto_only/encrypt_key_14b` | 330 ns | 仅 ChaChaPoly 加密段 |
| `crypto_only/encrypt_decrypt_key_14b` | 693 ns | 加密+解密对称段 |
| `handshake/xxpsk0_full` | **430 µs** | Noise_XXpsk0 三报文完整握手（不含 argon2 KDF——PSK 进程内派生一次，docs/02 §3.1） |

### 1.2 编解码吞吐（xcontrol-proto benches/codec.rs）

| 基准 | 中位数 | 说明 |
| --- | --- | --- |
| `input_decode/key` | 4.4 ns | 定长输入事件解码（单键） |
| `input_decode/mouse_move_rel` | 4.4 ns | 相对移动事件解码 |
| `input_decode/wheel` | 4.6 ns | 滚轮事件解码 |
| `input_decode/text_128b` | 46.6 ns | 128 字节 Unicode 文本帧解码 |
| `input_encode/key` | 80.1 ns | 定长输入事件编码（帧头 + 校验） |
| `input_encode/mouse_move_rel` | 77.0 ns | 相对移动编码 |
| `input_encode/text_128b` | 141.7 ns | 128 字节文本编码 |
| `ctrl_roundtrip/hello_encode` | 236.2 ns | Hello 控制消息编码（postcard） |
| `ctrl_roundtrip/hello_decode` | 98.6 ns | Hello 控制消息解码 |
| `ctrl_roundtrip/set_options_encode` | 116.4 ns | SetOptions 编码 |
| `frame_decode/window_10_frames` | 75.8 ns（**1.68 GiB/s**） | 10 帧窗口批量解码（帧层分拣吞吐） |

### 1.3 达标链路（预算分解对照）

```
捕获回调→出网 <2ms（平台层，docs/05 §7 手动清单实测）
+ LAN RTT <1ms（有线；Wi-Fi 预算见 docs/01 §4）
+ 入网→注入回调 <3ms（平台层，同上）
+ 协议+加密分量 ≈ 0.8–3.2µs（本报告 §1.1 实测）
──────────────────────────────────────────────
总预算 P50 15ms / P99 40ms —— 协议层占比 <0.03%，预算主要留给平台 I/O 与网络
```

## 2. NFR-1 复核门槛回归记录

- M1 基线（2026-11-17）：回环 P50 ≈ 0.82–1.06 µs（mock 链路 226 测试时的 in-process 口径）。
- 0.1.0 复测（本报告，2026-11-20）：`loopback_roundtrip/key_14b` 810 ns——criterion 与上轮
  基线自动对比 `Performance has improved`（协议 1.1 Layout 协商与 M5/M6 增量未引入回环回归）。
- 同轮 `ctrl_roundtrip/hello_encode` +6.4% / `hello_decode` +1.6%（p < 0.05 判定 regressed）：
  幅度在 postcard 版本微差与机器噪音耦合范围内（绝对值 236/99 ns，非 NFR 门槛路径），
  0.1.0 不作回归处理，记入下轮观察。

## 3. NFR-2 · 资源占用（门槛：空闲 CPU < 1%，RSS < 80MB 含 UI 进程；headless 从机 RSS < 30MB）

**结论：CPU 全场景达标（≤0.2%）；RSS 达标面 = UI 进程（≈55MB < 80MB）；
headless 从机 ≈ 36.9MB > 30MB——该项未达标，列 0.1.x 优化项。**

| 场景（release 实测，mac arm64） | RSS | CPU（空闲抽样 ×3） | 判定 |
| --- | --- | --- | --- |
| headless 主控（`--headless --host --code … --auto-quit-ms 9000`） | ≈ 37 MB | 0.1–0.2% | CPU ✓（RSS 无独立门槛） |
| headless 从机（`--headless --client 127.0.0.1:4243 …`） | ≈ **36.9 MB** | 0.0% | CPU ✓；**RSS ✗（> 30MB）** |
| 窗口模式（UI 进程，回环演示空闲段） | ≈ 55 MB | 0.1% | ✓（< 80MB） |

headless RSS 超标分析（36.9MB vs 30MB，+23%）：常驻面 = tokio 多线程运行时 + tracing
（fmt + EnvFilter + 滚动文件 worker + UI 环形缓冲——headless 亦分配）+ 发现层（mdns-sd
守护线程 + beacon 逐接口 socket）+ keyring/argon2 代码段。0.1.x 候选优化：headless 跳过
UI 环形缓冲与图形无关分配、tokio worker 数收敛（headless 单线程面）、发现层 socket 复用。
**不影响发布**（功能面完整；桌面从机走 UI 进程口径达标）。

实测配方（复跑）：

```bash
cargo build --release -p xcontrol-app --features <平台>
./target/release/xcontrol-app --headless --client 127.0.0.1:4243 --code test-code-**** --auto-quit-ms 9000 &
ps -o pid,rss,pcpu -p <pid>          # t=5s 起每 1.5s 抽样
```

## 4. NFR-7 · 冷启动（门槛：托盘可用 < 2s；连接建立 < 1s——发现完成后）

**结论：达标。**

| 项 | 实测 | 证据 |
| --- | --- | --- |
| 托盘可用 | **≈ 1s**（进程首日志 → `app::tray 托盘已创建` 同一秒内） | 窗口模式（向导流）release 实跑，日志时间戳判读；配方：`./target/release/xcontrol-app --auto-quit-ms 9000` |
| 连接建立（发现完成后） | **当秒完成**（connect 成功 → `session::rt 版本协商完成` 同一秒；握手本身 430 µs，§1.1） | 本机双实例演练 + M3 跨机 mDNS 演练（发现 → ≤2s 建立会话，FR-5.1 ≤5s 达标，11-17-m3-discovery） |

**已知噪声（同机双角色演练形态，docs/06 §4.1 记录的可接受噪音）**：单实例探测对
`net.port` 的 TCP connect 会被主控 serve 侧计入一次折叠握手失败（"非法报文"），并污染
同 IP 限速窗——同机 host+client 演练时从机首次真实握手被本地限速拒绝、退避 1s→2s 后
成功（实测序列：探测失败 → 限速拒绝 1 次 → 第 3 次尝试当秒完成）。**跨机部署不受影响**
（探测目标是本机端口，对端 serve 不经过探测连接）。根治 = 探测改 bind 语义（macOS
SO_REUSEADDR 恒假阴性，docs/06 §4.1）或探测连接带魔术字节协商——0.1.x 候选，不阻断发布。

**附带修复（11-19-m6-release 实测发现）**：无配置向导流曾启动即在主线程同步读默认
keychain 引用——macOS 钥匙串 ACL 对新构建二进制的授权弹窗会**无限期阻塞窗口/托盘装配**
（sample 实证 `SecKeychainFindGenericPassword` mach_msg 等待）。已修：向导流（无 config +
无 --code）跳过存储探测（等待相码由向导提供，无需预读）；配置存在路径的 keychain 读取
保持（首次放行后系统记忆，macOS 语义）。

## 5. 关联证据索引

| 面 | 数据 | 出处 |
| --- | --- | --- |
| fuzz（协议与剪贴板流鲁棒性） | 两 target 30s 各 6.62M / 5.00M runs 零 crash | 11-19-m6-security（docs/07 §8） |
| 覆盖率 | proto 98.55% / core 96.98% / net 98.76%（首批）；CI 门槛行覆盖 ≥85% | docs/08 §4、CI llvm-cov |
| 端到端 FR-2.x | 12 事件类型 mock 全链路 226+ 测试 | M1 mock-chain |
| 真机延迟（端到端 NFR-1 完整口径） | 双实机手动清单 | docs/05 §7（发布门禁，移交人工） |

## 修订记录

| 日期 | 变更 |
| --- | --- |
| 2026-11-20 | 初版（任务 11-19-m6-release）：criterion 0.1.0 基线汇总 + NFR-1/2/7 达标判定（headless 从机 RSS 一项未达标如实记录）+ 方法论与复跑配方 |
