//! 文件引用清单：发送侧构建 + 接收侧 pending 槽（docs/02 §4.6 / docs/04 §8；纯逻辑、零 I/O）。
//!
//! 位置与边界（同 [`crate::clip_rx`] 口径）：文件域消息走独立文件连接
//! （docs/02 §4.6，[ADR-0004](../../../docs/adr/0004-file-transfer-channel-and-materialization.md)），
//! 不经过 [`Session`](crate::session::Session) 的控制通道转移表——本模块由连接
//! 接线层持有（切片 b 接入真实通道；切片 a 由 mock 与集成测试驱动）。
//!
//! 与剪贴板重组状态机（[`ClipRx`](crate::clip_rx::ClipRx)）的差异：announce 是
//! **幂等整包**（元数据几 KiB、无分块无流控无超时），故本模块无 tick/无信用窗口；
//! 生命周期由**代次失效**驱动（docs/04 §8.2"最后复制者赢"）——任何新的
//! `FileRefAnnounce` 覆盖旧槽，本机/远端新剪贴板同步由调用方显式
//! [`FileRefRx::invalidate`]（本模块不嗅探剪贴板）。
//!
//! 防回环（docs/04 §5/§8.3）：文件域指纹 = 条目 `(name, size)` 有序序列
//! BLAKE3 截断（[`manifest_fingerprint`]），**不含 mtime/绝对路径**——物化
//! 落盘必然改变两者，纳入指纹必回环。防线 2（origin 去重）在本模块首次
//! 真实落地（剪贴板域的同名机制属上层通知语义，见 clip_rx 模块文档）。

use xcontrol_proto::{FileEntry, FileRefAnnounce, ProtoError};

/// 接收端配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRefRxConfig {
    /// 协商文件总字节上限（默认 2GiB，docs/02 §4.6 防护规则 1；独立于剪贴板 16MiB）。
    pub max_total_bytes: u64,
}

impl Default for FileRefRxConfig {
    fn default() -> Self {
        Self {
            max_total_bytes: FileRefAnnounce::DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

/// 单条 `FileRefAnnounce` 的处理裁定（调用方按裁定上抛托盘/忽略/告警）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRefVerdict {
    /// 新清单入槽（覆盖旧槽）：对端托盘提示"剪贴板文件：N 项 / X MB"。
    Applied {
        /// 条目数（顶层）。
        entry_count: usize,
        /// 内容总字节（清单宣称）。
        total_bytes: u64,
        /// 清单指纹（`(name, size)` 规范形，docs/04 §8.3）。
        fingerprint: [u8; 16],
    },
    /// 同源同 seq 重放（`(origin_id, origin_seq)` 已见）：去重忽略，
    /// 不重复通知（防线 2，docs/04 §5）。
    Duplicate,
    /// 策略拒绝（**非协议违规、不断连**）：过期限清单 / 协商超限。
    Rejected {
        /// 拒绝原因（诊断用，不含条目内容）。
        reason: String,
    },
}

/// pending 文件槽（单槽，最后复制者赢）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSlot {
    /// 清单宣称的传输标识（切片 b 拉流按此寻址）。
    pub transfer_id: u32,
    /// 来源实例 UUID。
    pub origin_id: [u8; 16],
    /// 来源序列号。
    pub origin_seq: u32,
    /// 顶层条目清单（子树经 FileExpand 惰性展开，切片 b/d）。
    pub entries: Vec<FileEntry>,
    /// 内容总字节。
    pub total_bytes: u64,
    /// 复制时刻（Unix 毫秒，展示用）。
    pub copied_at_ms: u64,
    /// 清单指纹（[`manifest_fingerprint`]）。
    pub fingerprint: [u8; 16],
}

/// 文件引用接收状态（每条文件连接一个实例）。
///
/// 消息入口：[`FileRefRx::on_announce`]（幂等整包）；失效入口：
/// [`FileRefRx::invalidate`]（调用方在本机/远端新剪贴板同步时显式调用，
/// docs/04 §8.2）。
#[derive(Debug, Clone)]
pub struct FileRefRx {
    config: FileRefRxConfig,
    slot: Option<PendingSlot>,
    /// 已见 `(origin_id, origin_seq)`（单值足够：同源 seq 单调，跨源覆盖即换值）。
    last_seen: Option<([u8; 16], u32)>,
}

impl FileRefRx {
    /// 构造接收端。
    #[must_use]
    pub fn new(config: FileRefRxConfig) -> Self {
        Self {
            config,
            slot: None,
            last_seen: None,
        }
    }

    /// 当前 pending 槽（`None` = 无可粘贴文件）。
    #[must_use]
    pub fn slot(&self) -> Option<&PendingSlot> {
        self.slot.as_ref()
    }

    /// `FileRefAnnounce`（docs/02 §4.6 元数据面）：协商上限/来源代次校验通过后
    /// 入槽并覆盖旧槽（最后复制者赢）。
    ///
    /// wire 级校验（条目数/名称/默认总字节上限）已在 proto 解码层强制；
    /// 此处的协商缩限（`config.max_total_bytes` < 宣称值）按策略拒绝
    /// （`Rejected`，非协议违规、不断连）。
    pub fn on_announce(&mut self, announce: FileRefAnnounce) -> FileRefVerdict {
        if announce.total_bytes > self.config.max_total_bytes {
            return FileRefVerdict::Rejected {
                reason: format!(
                    "total_bytes {} 超协商上限 {}",
                    announce.total_bytes, self.config.max_total_bytes
                ),
            };
        }
        let seen = (announce.origin_id, announce.origin_seq);
        if let Some(last) = self.last_seen {
            if last == seen {
                return FileRefVerdict::Duplicate;
            }
            if last.0 == seen.0 && last.1 > seen.1 {
                return FileRefVerdict::Rejected {
                    reason: format!("过期清单：已见 seq {} > 来达 seq {}", last.1, seen.1),
                };
            }
        }
        let fingerprint = manifest_fingerprint(&announce.entries);
        self.last_seen = Some(seen);
        self.slot = Some(PendingSlot {
            transfer_id: announce.transfer_id,
            origin_id: announce.origin_id,
            origin_seq: announce.origin_seq,
            entries: announce.entries,
            total_bytes: announce.total_bytes,
            copied_at_ms: announce.copied_at_ms,
            fingerprint,
        });
        FileRefVerdict::Applied {
            entry_count: self.slot.as_ref().map_or(0, |slot| slot.entries.len()),
            total_bytes: self.slot.as_ref().map_or(0, |slot| slot.total_bytes),
            fingerprint,
        }
    }

    /// 代次失效（docs/04 §8.2）：本机或远端出现**任何**新剪贴板内容
    /// （text/png/html/文件）时由调用方调用；返回被失效的槽（诊断用）。
    pub fn invalidate(&mut self) -> Option<PendingSlot> {
        self.slot.take()
    }
}

/// 发送侧清单构建（docs/04 §8.1：复制端检测到文件引用 → stat 元数据 → 清单）。
///
/// - 空清单 → `Ok(None)`（无文件引用不广播）；
/// - 条目数 > [`FileRefAnnounce::MAX_ENTRIES`] / 名称非法 → `Err`
///   （与 proto 编码期校验同规则，fail-fast 镜像）；
/// - `total_bytes` 由顶层条目 `size` 聚合（目录条目 `size` 即子树聚合估计）。
///
/// # Errors
/// [`ProtoError::FileEntriesExceeded`] / [`ProtoError::FileNameInvalid`]。
pub fn build_manifest(
    transfer_id: u32,
    origin_id: [u8; 16],
    origin_seq: u32,
    entries: Vec<FileEntry>,
    copied_at_ms: u64,
) -> Result<Option<FileRefAnnounce>, ProtoError> {
    if entries.is_empty() {
        return Ok(None);
    }
    if entries.len() > FileRefAnnounce::MAX_ENTRIES {
        return Err(ProtoError::FileEntriesExceeded {
            count: entries.len(),
            max: FileRefAnnounce::MAX_ENTRIES,
        });
    }
    for entry in &entries {
        if !FileEntry::is_valid_name(&entry.name) {
            return Err(ProtoError::FileNameInvalid {
                max: FileEntry::MAX_NAME_BYTES,
            });
        }
    }
    let total_bytes = entries.iter().map(|entry| entry.size).sum();
    Ok(Some(FileRefAnnounce {
        transfer_id,
        origin_id,
        origin_seq,
        entries,
        total_bytes,
        copied_at_ms,
    }))
}

/// 清单指纹（docs/04 §8.3 规范形）：条目 `(name, size)` 有序序列 BLAKE3 截断 16B。
///
/// **不含 mtime / 绝对路径 / children**——物化落盘必然改变前两者，纳入指纹必回环。
/// 帧式布局（条目数 + 每条目 len 前缀名 + 定宽 size）防拼接碰撞：
/// `("ab", "c")` 与 `("a", "bc")` 字节拼接相同但指纹不同。
#[must_use]
pub fn manifest_fingerprint(entries: &[FileEntry]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(entries.len() as u64).to_le_bytes());
    for entry in entries {
        let name = entry.name.as_bytes();
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name);
        hasher.update(&entry.size.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest.as_bytes()[0..16]);
    out
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::FileEntryKind;

    use super::*;

    fn entry(name: &str, size: u64) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            size,
            mtime_ms: 1_700_000_000_000,
            kind: FileEntryKind::File,
            children: 0,
        }
    }

    fn announce(entries: Vec<FileEntry>) -> FileRefAnnounce {
        FileRefAnnounce {
            transfer_id: 1,
            origin_id: [1; 16],
            origin_seq: 1,
            total_bytes: entries.iter().map(|e| e.size).sum(),
            entries,
            copied_at_ms: 0,
        }
    }

    /// 指纹规范形（docs/04 §8.3）：mtime/children/kind 不参与；name/size/顺序参与；
    /// 帧式布局防拼接碰撞。
    #[test]
    fn fingerprint_canonical_form() {
        let base = vec![entry("a.txt", 1), entry("b.txt", 2)];
        let fp = manifest_fingerprint(&base);

        // mtime 变化（物化落盘的必然后果）不改指纹
        let mut touched = base.clone();
        touched[0].mtime_ms += 123_456;
        assert_eq!(manifest_fingerprint(&touched), fp, "mtime 不参与指纹");

        // children/kind 变化不改指纹（目录聚合估计是易变量）
        let mut dir_ish = base.clone();
        dir_ish[0].kind = FileEntryKind::Dir;
        dir_ish[0].children = 7;
        assert_eq!(
            manifest_fingerprint(&dir_ish),
            fp,
            "children/kind 不参与指纹"
        );

        // name 或 size 变化改指纹
        let mut renamed = base.clone();
        renamed[0].name = "c.txt".to_owned();
        assert_ne!(manifest_fingerprint(&renamed), fp, "name 参与指纹");
        let mut resized = base.clone();
        resized[1].size = 3;
        assert_ne!(manifest_fingerprint(&resized), fp, "size 参与指纹");

        // 顺序参与（有序序列）
        let swapped = vec![entry("b.txt", 2), entry("a.txt", 1)];
        assert_ne!(manifest_fingerprint(&swapped), fp, "顺序参与指纹");

        // 拼接碰撞：("ab","c") 与 ("a","bc") 拼接字节相同，指纹必须不同
        let split_ab = vec![entry("ab", 1), entry("c", 1)];
        let split_a = vec![entry("a", 1), entry("bc", 1)];
        assert_ne!(
            manifest_fingerprint(&split_ab),
            manifest_fingerprint(&split_a),
            "len 前缀防拼接碰撞"
        );

        // 空清单指纹确定（不 panic 即边界成立）
        let _empty = manifest_fingerprint(&[]);
    }

    /// 清单构建（发送侧）：空 → None；超限/坏名 → Err；聚合 total_bytes。
    #[test]
    fn manifest_builder_rules() {
        assert_eq!(build_manifest(1, [1; 16], 1, Vec::new(), 0).unwrap(), None);

        let ok = build_manifest(1, [1; 16], 1, vec![entry("a", 10), entry("b", 32)], 42)
            .unwrap()
            .expect("非空清单应产出");
        assert_eq!(ok.total_bytes, 42, "total_bytes 由条目聚合");
        assert_eq!(ok.entries.len(), 2);

        let over: Vec<FileEntry> = (0..=FileRefAnnounce::MAX_ENTRIES)
            .map(|i| entry(&format!("f{i}"), 1))
            .collect();
        assert!(matches!(
            build_manifest(1, [1; 16], 1, over, 0),
            Err(ProtoError::FileEntriesExceeded { .. })
        ));

        let bad = vec![entry("../escape", 1)];
        assert!(matches!(
            build_manifest(1, [1; 16], 1, bad, 0),
            Err(ProtoError::FileNameInvalid { .. })
        ));
    }

    /// 接收端：入槽 → 可读；同源同 seq 重放 → Duplicate；低 seq → 过期拒绝；
    /// 跨来源覆盖（最后复制者赢）；invalidate 清槽。
    #[test]
    fn rx_apply_dedup_invalidate() {
        let mut rx = FileRefRx::new(FileRefRxConfig::default());

        let first = announce(vec![entry("a.txt", 1)]);
        let applied = rx.on_announce(first.clone());
        assert_eq!(
            applied,
            FileRefVerdict::Applied {
                entry_count: 1,
                total_bytes: 1,
                fingerprint: manifest_fingerprint(&first.entries),
            }
        );
        assert_eq!(rx.slot().map(|slot| slot.transfer_id), Some(1));

        // 同源同 seq 重放 → Duplicate（槽不动）
        assert_eq!(rx.on_announce(first.clone()), FileRefVerdict::Duplicate);
        assert_eq!(rx.slot().map(|slot| slot.origin_seq), Some(1));

        // 低 seq（乱序迟到）→ 过期拒绝（槽不动）
        let mut stale = first.clone();
        stale.origin_seq = 0;
        assert!(matches!(
            rx.on_announce(stale),
            FileRefVerdict::Rejected { .. }
        ));
        assert_eq!(rx.slot().map(|slot| slot.origin_seq), Some(1));

        // 同源更高 seq → 覆盖
        let mut newer = first.clone();
        newer.origin_seq = 2;
        newer.entries = vec![entry("b.txt", 2)];
        newer.total_bytes = 2;
        assert!(matches!(
            rx.on_announce(newer),
            FileRefVerdict::Applied { .. }
        ));
        assert_eq!(rx.slot().map(|slot| slot.origin_seq), Some(2));

        // 跨来源覆盖（最后复制者赢，docs/04 §8.2）
        let mut other = first;
        other.origin_id = [2; 16];
        other.entries = vec![entry("other.txt", 5)];
        other.total_bytes = 5;
        assert!(matches!(
            rx.on_announce(other),
            FileRefVerdict::Applied { .. }
        ));
        assert_eq!(rx.slot().map(|slot| slot.origin_id), Some([2; 16]));

        // invalidate 清槽并返回旧槽；空槽 invalidate → None
        let evicted = rx.invalidate().expect("有槽可失效");
        assert_eq!(evicted.entries[0].name, "other.txt");
        assert!(rx.slot().is_none());
        assert!(rx.invalidate().is_none());
    }

    /// 协商缩限（docs/02 §4.6 防护规则 1 可配）：默认内通过；缩限后策略拒绝
    /// （`Rejected`，非违规）。
    #[test]
    fn negotiated_limit_rejects() {
        let mut rx = FileRefRx::new(FileRefRxConfig {
            max_total_bytes: 1024,
        });
        let verdict = rx.on_announce(announce(vec![entry("big.bin", 2048)]));
        assert!(
            matches!(verdict, FileRefVerdict::Rejected { .. }),
            "超协商总字节按策略拒绝"
        );
        assert!(rx.slot().is_none(), "拒绝不入槽");

        let mut rx = FileRefRx::new(FileRefRxConfig::default());
        assert!(matches!(
            rx.on_announce(announce(vec![entry("ok.bin", 1024)])),
            FileRefVerdict::Applied { .. }
        ));
    }
}
