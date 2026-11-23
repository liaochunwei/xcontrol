//! 网络层错误类型（docs/08 §2 错误规范；docs/02 §0 WPS 教训、§3.2 防爆破）。
//!
//! # 泄露纪律（docs/02 §0/§3.2、07-security.md §3）
//!
//! - [`NetError::HandshakeFailed`] 是握手失败的**唯一**出口变体：超时、安全码不匹配
//!   （AEAD 解密失败）、报文格式错误、对端中途断开全部折叠到它。本端日志不携带
//!   密钥派生中间值，对端收不到任何"错误码/原因"反馈——只有连接消失
//!   （无部分匹配 oracle，WPS 之鉴）；
//! - [`NetError::RateLimited`] 是握手前的本地策略判定（不触网，对端无感知）；
//! - 其余变体的错误信息均不含安全码、剪贴板内容或派生中间值（docs/08 §2）。

/// 网络层错误（可编程区分；握手路径统一折叠，见模块文档泄露纪律）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NetError {
    /// 握手失败（docs/02 §3.2：失败即断开）。原因折叠：超时 / AEAD 解密失败 /
    /// 报文格式违规 / 对端中途断开——调用方与对端均不得也无法区分。
    #[error("握手失败")]
    HandshakeFailed,
    /// 本地限速拒绝本次握手尝试（docs/02 §3.2 防爆破：退避中或距上次失败过近）。
    #[error("握手被本地限速拒绝")]
    RateLimited,
    /// 传输已关闭（本端 shutdown 或链路断开注入后的收发调用）。
    #[error("传输已关闭")]
    Closed,
    /// 传输态解密失败（密钥失配/密文篡改/报文错位）：一律断开，不重试（docs/02 §7）。
    #[error("传输态解密失败")]
    DecryptFailed,
    /// 加密失败（报文超出单条 Noise 消息明文上限等，见
    /// [`crate::noise::MAX_NOISE_PLAINTEXT`]）。
    #[error("加密失败")]
    EncryptFailed,
    /// KDF 参数构造或执行失败（参数为编译期常量，理论不可达；不携带细节）。
    #[error("KDF 失败")]
    KdfFailed,
    /// 传输 I/O 失败（11-17-m2-tcp：TCP 连接/读写错误）。变体不携带 io::Error
    /// 细节——失败即断开由调用方执行（docs/02 §7），对端不可达/端口未监听等
    /// 环境性信息不构成握手 oracle（尚未进入握手，无安全码可猜测）。
    #[error("传输 I/O 失败")]
    IoFailed,
    /// 帧层违规（11-17-m2-tcp：长度前缀非法——空帧或超过单条 Noise 报文上限）。
    /// 传输态违规语义：一律断开、不重试（docs/02 §7 协议违规）；握手期该错误
    /// 折叠进 [`NetError::HandshakeFailed`]（无部分匹配 oracle，docs/02 §0）。
    #[error("帧长度违规")]
    FrameInvalid,
    /// 发现层基础设施不可用（11-17-m3-discovery：mDNS 守护线程创建/注册失败、
    /// 发现 UDP 端口绑定失败）。变体不携带细节——发现层失败不中断重连循环
    /// （error-handling.md 规则 4）：调用方降级为空候选 + 下一轮重试，日志由
    /// app/cli 顶层接入点承载（net 层零日志纪律，docs/02 §2）。
    #[error("发现层不可用")]
    DiscoveryUnavailable,
}
