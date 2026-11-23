//! PSK 派生（docs/02 §3.1 逐行契约）与派生缓存。
//!
//! ```text
//! salt = BLAKE3-derive-key("xcontrol/salt-v1", instance_uuid)[0..16]   // uuid 公开，per-host 防跨用户彩虹表
//! p1   = Argon2id(code, salt, m=19MiB, t=2, p=1)                        // OWASP 口令基线
//! psk  = BLAKE3-derive-key("xcontrol/psk-v1", p1)                       // 域分离输出 32B
//! ```
//!
//! 派生是纯函数（无 I/O、无时钟）；[`PskCache`] 包装"派生一次"的缓存语义
//! （docs/02 §3.1：argon2 结果缓存，启动时后台执行，不阻塞发现）。
//! 测试向量见本模块 `#[cfg(test)]`（固定输入 → 固定输出 hex；假码，不含真实安全码）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use argon2::{Algorithm, Argon2, Params, Version};

use crate::error::NetError;

/// salt 派生域（docs/02 §3.1 第一行）。
pub const SALT_CONTEXT: &str = "xcontrol/salt-v1";
/// psk 派生域（docs/02 §3.1 第三行）。
pub const PSK_CONTEXT: &str = "xcontrol/psk-v1";
/// 缓存查找键派生域（进程内内存键，不落盘、不参与协议——见 [`PskCache`] 文档）。
pub const CACHE_KEY_CONTEXT: &str = "xcontrol/psk-cache-key-v1";

/// Argon2id 内存参数（KiB）：m=19MiB（docs/02 §3.1，OWASP 口令哈希基线）。
pub const ARGON2_M_KIB: u32 = 19 * 1024;
/// Argon2id 迭代参数：t=2（docs/02 §3.1）。
pub const ARGON2_T_COST: u32 = 2;
/// Argon2id 并行参数：p=1（docs/02 §3.1）。
pub const ARGON2_P_COST: u32 = 1;

/// salt 长度：BLAKE3 输出截取 `[0..16]`（docs/02 §3.1）。
pub const SALT_LEN: usize = 16;
/// psk 长度：BLAKE3 derive-key 输出 32B（snow `Builder::psk` 直接可用）。
pub const PSK_LEN: usize = 32;

/// `salt = BLAKE3-derive-key("xcontrol/salt-v1", instance_uuid)[0..16]`（docs/02 §3.1）。
#[must_use]
pub fn derive_salt(instance_uuid: &[u8; 16]) -> [u8; SALT_LEN] {
    let digest = blake3::derive_key(SALT_CONTEXT, instance_uuid);
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&digest[..SALT_LEN]);
    salt
}

/// 完整 PSK 派生链（docs/02 §3.1）：
/// salt（BLAKE3 域分离）→ `p1 = Argon2id(code, salt, 19MiB/2/1)` → psk（BLAKE3 域分离）。
///
/// # Errors
/// [`NetError::KdfFailed`]：argon2 参数构造或执行失败。参数为编译期常量、
/// salt/输出长度由本函数固定，正常路径不可能失败；错误不携带任何派生中间值
/// （docs/08 §2 错误信息纪律）。
pub fn derive_psk(code: &str, instance_uuid: &[u8; 16]) -> Result<[u8; PSK_LEN], NetError> {
    let salt = derive_salt(instance_uuid);
    let params = Params::new(ARGON2_M_KIB, ARGON2_T_COST, ARGON2_P_COST, Some(PSK_LEN))
        .map_err(|_| NetError::KdfFailed)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut p1 = [0u8; PSK_LEN];
    argon2
        .hash_password_into(code.as_bytes(), &salt, &mut p1)
        .map_err(|_| NetError::KdfFailed)?;
    Ok(blake3::derive_key(PSK_CONTEXT, &p1))
}

/// 缓存内部状态：条目表 + 实际派生计数（观测缓存命中）。
#[derive(Debug, Default)]
struct CacheInner {
    entries: HashMap<[u8; 32], [u8; PSK_LEN]>,
    derive_count: u64,
}

/// argon2 派生缓存（docs/02 §3.1「派生一次」）。
///
/// 缓存查找键 = `BLAKE3-derive-key(CACHE_KEY_CONTEXT, code ‖ instance_uuid)`：
/// 进程内内存键，不落盘、不参与协议。其内存泄露风险等级等同已文档化的
/// 「录制握手可离线穷举」诚实边界（07-security.md §3.2），不新增泄露面——
/// 持有进程内存的攻击者本就握有比该键更强的握手转录本。
/// 同一 (code, uuid) 第二次起命中缓存；换码重配对自动失效（键含 code 派生）。
#[derive(Debug, Default)]
pub struct PskCache {
    inner: Mutex<CacheInner>,
}

impl PskCache {
    /// 空缓存。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                entries: HashMap::new(),
                derive_count: 0,
            }),
        }
    }

    /// 取 PSK：命中缓存直接返回；未命中则派生一次并缓存。
    ///
    /// # Errors
    /// [`NetError::KdfFailed`]：同 [`derive_psk`]。
    pub fn get_or_derive(
        &self,
        code: &str,
        instance_uuid: &[u8; 16],
    ) -> Result<[u8; PSK_LEN], NetError> {
        let key = cache_key(code, instance_uuid);
        let mut inner = self.lock();
        if let Some(psk) = inner.entries.get(&key) {
            return Ok(*psk);
        }
        let psk = derive_psk(code, instance_uuid)?;
        inner.entries.insert(key, psk);
        inner.derive_count = inner.derive_count.saturating_add(1);
        Ok(psk)
    }

    /// 实际执行 argon2 派生的次数（缓存命中不计数；观测/测试用）。
    #[must_use]
    pub fn derive_count(&self) -> u64 {
        self.lock().derive_count
    }

    /// 已缓存条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// 缓存是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 锁获取（锁中毒恢复：缓存是纯内存派生值，中毒后保留原内容继续服务
    /// 优于整体失效；`unwrap_or_else` 非 unwrap，不触 CI 禁令）。
    fn lock(&self) -> MutexGuard<'_, CacheInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 缓存查找键：域分离派生（不与 salt/psk 域重叠）。
fn cache_key(code: &str, instance_uuid: &[u8; 16]) -> [u8; 32] {
    let mut input = Vec::with_capacity(code.len() + instance_uuid.len());
    input.extend_from_slice(code.as_bytes());
    input.extend_from_slice(instance_uuid);
    blake3::derive_key(CACHE_KEY_CONTEXT, &input)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试固定输入（假码；docs/08 §4：fixture/测试永不用真实安全码）。
    const TEST_CODE: &str = "test-code";
    const TEST_UUID: [u8; 16] = [0u8; 16];

    /// docs/02 §3.1 回归向量：固定输入 → 固定输出。
    ///
    /// 变更派生链（context/参数）会改变此向量——若属协议变更，须先改 docs/02
    /// 再更新本常量（协议即契约）。
    const VECTOR_HEX: &str = "da8c4d008d2e5dde54f9855baf2d7ebbd7e9e7c815b906a81a23a7074207c0aa";

    fn decode_vector() -> [u8; PSK_LEN] {
        let mut out = [0u8; PSK_LEN];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&VECTOR_HEX[i * 2..i * 2 + 2], 16)
                .expect("测试向量必须是合法 hex");
        }
        out
    }

    /// 派生链逐字节等于记录向量（docs/02 §3.1 回归锚点）。
    #[test]
    fn psk_vector_fixed_input_fixed_output() {
        let psk = derive_psk(TEST_CODE, &TEST_UUID).unwrap();
        assert_eq!(psk, decode_vector(), "派生链与 docs/02 §3.1 回归向量不符");
    }

    /// 派生确定性 + 对 code/uuid 双向敏感 + 输出非中间值退化。
    #[test]
    fn psk_derivation_sensitivity() {
        let a = derive_psk(TEST_CODE, &TEST_UUID).unwrap();
        let b = derive_psk(TEST_CODE, &TEST_UUID).unwrap();
        assert_eq!(a, b, "同输入必须同输出（纯函数）");
        // code 敏感
        assert_ne!(a, derive_psk("other-code", &TEST_UUID).unwrap());
        // uuid 敏感
        let mut uuid = TEST_UUID;
        uuid[0] = 1;
        assert_ne!(a, derive_psk(TEST_CODE, &uuid).unwrap());
        // 输出 ≠ salt 域对同 uuid 的输出（域分离生效）
        assert_ne!(a, blake3::derive_key(SALT_CONTEXT, &TEST_UUID));
    }

    /// salt 长度与确定性（docs/02 §3.1 第一行：uuid 公开值即可派生）。
    #[test]
    fn salt_is_deterministic_16b() {
        let s1 = derive_salt(&TEST_UUID);
        let s2 = derive_salt(&TEST_UUID);
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), SALT_LEN);
        let mut uuid = TEST_UUID;
        uuid[15] = 7;
        assert_ne!(s1, derive_salt(&uuid), "per-host salt 随 uuid 变化");
    }

    /// 缓存：首查派生（计数 1），二查命中（计数不变、条目为 1）；换码新条目。
    #[test]
    fn psk_cache_hits_and_invalidation() {
        let cache = PskCache::new();
        assert!(cache.is_empty());
        let first = cache.get_or_derive(TEST_CODE, &TEST_UUID).unwrap();
        assert_eq!(cache.derive_count(), 1);
        assert_eq!(cache.len(), 1);
        // 命中：结果一致且不再派生
        let second = cache.get_or_derive(TEST_CODE, &TEST_UUID).unwrap();
        assert_eq!(first, second);
        assert_eq!(cache.derive_count(), 1, "命中不得重新派生（docs/02 §3.1）");
        // 换码（重配对）：新键新派生
        let other = cache.get_or_derive("other-code", &TEST_UUID).unwrap();
        assert_ne!(first, other);
        assert_eq!(cache.derive_count(), 2);
        assert_eq!(cache.len(), 2);
    }

    /// 默认构造等价 `new`（clippy new_without_default）。
    #[test]
    fn psk_cache_default() {
        let cache = PskCache::default();
        assert!(cache.is_empty());
        assert_eq!(cache.derive_count(), 0);
    }
}
