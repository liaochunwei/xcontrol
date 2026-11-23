//! `xcontrol-net` — 网络层：发现（UDP beacon / mDNS / 扫描）、传输（双 TCP 连接）、
//! Noise 加密（snow）与重连/心跳。
//!
//! 选型基线（docs/01 §6、ADR-0002）：`tokio` · `snow` · `mdns-sd`；传输 = TCP×2
//! （#1 输入流、#2 剪贴板流）+ Noise PSK（argon2 派生，专用线程执行）。
//!
//! 三层发现（docs/01 §5）：`mdns` / `beacon` / `scan-off` 与 config 对应。
//! 本 crate 归 tokio 多线程运行时执行域；与平台输入层的 async 边界经
//! `tokio::sync::mpsc` 桥接（docs/01 §2）。
//!
//! # M1 已落地（Noise 加密与回环传输）· M2 增量（真实 TCP 传输与握手实战）
//!
//! | 模块 | 内容 | 语义锚点 |
//! | --- | --- | --- |
//! | [`psk`] | PSK 派生链（blake3 域分离 ×2 + Argon2id）+ [`psk::PskCache`] | docs/02 §3.1 |
//! | [`noise`] | XXpsk0 握手（initiator/responder 双侧，同步 + async 两形态）+ [`noise::SecureTransport`] | docs/02 §3.2/§3.3 |
//! | [`transport`] | [`transport::Transport`] 回环抽象（M1，同步零 tokio）+ [`transport::TcpTransport`]（M2，tokio TCP 封装） | design.md（两条并存） |
//! | [`throttle`] | per-peer 握手限速（5 失败 → 10s 指数退避封顶 10min + 2s 自我限速） | docs/02 §3.2 |
//! | [`framing`] | 长度前缀分帧（u32 BE len + Noise 报文，粘包/拆包天然正确） | design.md（M2） |
//! | [`serve`] | `serve()`/`connect()` 监听与连接入口 + 握手 5s 超时 + throttle 计数（主控按来源 `IpAddr`、从机按拨号地址） | docs/02 §1/§3.2（M2） |
//! | [`discovery`] | XCD1 beacon 编解码 + `XCP1?` 探测识别/应答 + 定向广播地址/网段/接口过滤纯函数（发现负载零派生物） | docs/02 §2.1（M3） |
//! | [`announce`] | [`announce::Announcer`]：mDNS 注册 + beacon 逐接口定向广播（5s/60s 满配对切换）+ `XCP1?` 应答 | docs/02 §2.1（M3） |
//! | [`browse`] | [`browse::Browser`]：mDNS browse + UDP 并发探测（CIDR/128 并发/300ms 窗）+ 候选去重 | docs/02 §2.2（M3） |
//! | [`reconnect`] | [`reconnect::ReconnectPlanner`]：断线重连退避 1s→30s 封顶 + 成功重置（纯逻辑，Instant 注入） | FR-5.3（M3） |
//!
//! 同步/异步决策（design.md）：M1 全同步回环实现服务于测试与基准；M2 的
//! [`transport::TcpTransport`] 与 [`serve`] 为 async 实现（tokio），**不强行统一**
//! 进 M1 同步 `Transport` trait（两条并存，避免破坏 M1 测试；整体 async 统一
//! 延后到真实需要时）。
//!
//! 真实双机发现（mDNS/beacon/探测）与断线重连由 M3 落地（见 [`announce`]/
//! [`browse`]/[`reconnect`]）；M2 以 localhost 双角色集成测试实现 FR-1.4 的
//! 可自动化部分（真实 socket + 真实 XXpsk0 握手 + 错误安全码拒绝路径）。
//!
//! # 泄露纪律（docs/02 §0 WPS 教训）
//!
//! 握手失败统一 [`NetError::HandshakeFailed`]（原因折叠）；限速为本地判定
//! 不触网；对端只见断开——安全码匹配不存在任何"部分匹配"反馈。发现流量
//! （beacon/探测/TXT）零安全码派生物（[`discovery`] 模块文档 + 测试锚点）。
//!
//! # 分层红线（docs/08 §1）
//!
//! net → 仅 `xcontrol-proto` + `xcontrol-config`（+ 已过审第三方：snow/argon2/
//! blake3/thiserror/tokio/mdns-sd/if-addrs/socket2）；`xcontrol-core` 仅 dev-dep
//! （tests/chain.rs 集成测试专用）。tokio 是本 crate 的执行域归属（docs/01 §2），
//! 不进 proto/core。
#![forbid(unsafe_code)]

pub mod announce;
pub mod browse;
pub mod discovery;
pub mod error;
pub mod framing;
pub mod noise;
pub mod psk;
pub mod reconnect;
pub mod serve;
pub mod throttle;
pub mod transport;

pub use announce::{Announcer, AnnouncerOptions, BEACON_PERIOD_PAIRED, BEACON_PERIOD_UNPAIRED};
pub use browse::{Browser, BrowserOptions, Candidate, CandidateSource};
pub use error::NetError;
pub use noise::{HandshakeDeadline, HandshakeRole, NoiseHandshake, SecureTransport};
pub use psk::{PskCache, derive_psk, derive_salt};
pub use reconnect::{ReconnectConfig, ReconnectPlanner};
pub use serve::{Connection, HandshakeFailure, HandshakeFailureReason, Server, ServerOptions};
pub use throttle::{Admission, HandshakeThrottle, ThrottleConfig};
pub use transport::{LoopbackControl, LoopbackTransport, TcpTransport, Transport};
