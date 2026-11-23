//! 版本协商与兼容承诺（docs/02 §5）。
//!
//! major=1 内保持帧格式向后兼容（**只增不改不删**，docs/02 §5.4）；
//! 破坏性变更必须 major+1 并双栈并存至少一个 major 周期。

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

/// 当前协议 major（兼容承诺锚点，docs/02 §5.4）。
pub const PROTO_MAJOR: u8 = 1;
/// 当前协议 minor（v1.1：`ScreenInfo.owner` 屏幕归属字段 + ScreenId 全局语义，
/// docs/02 §4.2/§4.3 修订记录；1.0 = v1 实装起点）。
pub const PROTO_MINOR: u8 = 1;

/// 协议版本 `major.minor`（docs/02 §4.3/§5；postcard 按 Rust 类型编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtoVer {
    /// 主版本（不等即断开）。
    pub major: u8,
    /// 次版本（会话取 min）。
    pub minor: u8,
}

impl ProtoVer {
    /// 本 crate 实装的协议版本（`= { PROTO_MAJOR, PROTO_MINOR }`，随常量锚点更新）。
    pub const CURRENT: Self = Self {
        major: PROTO_MAJOR,
        minor: PROTO_MINOR,
    };

    /// 是否与本 crate 实装的 major 一致。
    #[must_use]
    pub const fn is_same_major(self) -> bool {
        self.major == PROTO_MAJOR
    }
}

/// 版本协商（docs/02 §5.2）：`major` 不等即 Err（发送 `Error{UnsupportedVersion}` 后断开）；
/// minor 取双方较小值作为**会话 minor**——只发送会话 minor 之下允许的消息集（§5.3）。
///
/// `negotiate(local, remote)` 与 `negotiate(remote, local)` 结果一致（对称）。
///
/// # Errors
/// [`ProtoError::VersionIncompatible`]：major 不等。
pub const fn negotiate(local: ProtoVer, remote: ProtoVer) -> Result<ProtoVer, ProtoError> {
    if local.major != remote.major {
        return Err(ProtoError::VersionIncompatible {
            local_major: local.major,
            local_minor: local.minor,
            remote_major: remote.major,
            remote_minor: remote.minor,
        });
    }
    let minor = if local.minor <= remote.minor {
        local.minor
    } else {
        remote.minor
    };
    Ok(ProtoVer {
        major: local.major,
        minor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(major: u8, minor: u8) -> ProtoVer {
        ProtoVer { major, minor }
    }

    /// docs/02 §8 版本协商矩阵：1.0 ↔ 1.1 ↔ 2.0 全组合。
    #[test]
    fn negotiation_matrix() {
        // 同 major：minor 取 min（对称）
        for (a, b, expected) in [
            (ver(1, 0), ver(1, 0), ver(1, 0)),
            (ver(1, 0), ver(1, 1), ver(1, 0)),
            (ver(1, 1), ver(1, 0), ver(1, 0)),
            (ver(1, 1), ver(1, 1), ver(1, 1)),
        ] {
            assert_eq!(negotiate(a, b), Ok(expected), "{a:?}↔{b:?}");
            assert_eq!(negotiate(b, a), Ok(expected), "对称：{b:?}↔{a:?}");
        }
        // major 不等：Err（1.0↔2.0 双向）
        for (a, b) in [(ver(1, 0), ver(2, 0)), (ver(1, 1), ver(2, 0))] {
            let err = negotiate(a, b).unwrap_err();
            assert!(matches!(err, ProtoError::VersionIncompatible { .. }));
            assert!(negotiate(b, a).is_err());
        }
        // 2.0↔2.0（未来 major 自身可协商）
        assert_eq!(negotiate(ver(2, 0), ver(2, 0)), Ok(ver(2, 0)));
    }

    /// 当前版本锚点。
    #[test]
    fn current_version() {
        assert_eq!(PROTO_MAJOR, 1);
        assert_eq!(ProtoVer::CURRENT, ProtoVer { major: 1, minor: 1 });
        assert!(ProtoVer::CURRENT.is_same_major());
        assert!(!ver(2, 0).is_same_major());
    }
}
