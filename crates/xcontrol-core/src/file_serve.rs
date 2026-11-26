//! 文件域源端清单注册表与拉取校验（docs/02 §4.6 / docs/07 §9；纯逻辑、零 I/O）。
//!
//! **索引寻址红线**（docs/07 §9）：源端只服务**本连接已通告/已展开**的
//! `transfer_id + entry 下标`；未知 id / 越界下标 / `offset` 超条目 size =
//! 协议违规（调用方断开）。下标 → 本机路径的映射由接线层（app）私有维护，
//! 本模块只存 wire 元数据（size），不触碰路径。
//!
//! 文件字节读取在 app 层（core 零 I/O 红线）：[`FileServeGrant`] 只声明
//! 「许可读哪一条目的哪一段」，读多快、怎么分块由调用方结合
//! [`crate::file_tx::FileTx`] 信用窗口决定。
//!
//! 并发上限：每连接同时活动 transfer ≤ 4（docs/02 §4.6 防护规则 7）。

use xcontrol_proto::{FileExpand, FilePullReq, FileRefAnnounce};

/// 每连接同时活动 transfer 上限（docs/02 §4.6 防护规则 7）。
pub const MAX_ACTIVE_TRANSFERS: usize = 4;

/// 已登记的清单元数据（下标空间 + 条目尺寸表；不含路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegisteredManifest {
    /// 条目尺寸表（下标空间随 announce + FileExpand 连续增长）。
    entry_sizes: Vec<u64>,
}

/// 拉取校验产出：`Grant` = 许可源端读取该段；`Violation` = 协议违规（断开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeVerdict {
    /// 校验通过：许可读 `transfer_id` 清单的第 `entry` 条目自 `offset` 起的
    /// `remaining` 字节（`remaining = size − offset`，恒 ≥ 1——offset == size
    /// 的空段拉取按违规处理，无意义流量）。
    Grant {
        /// 目标传输。
        transfer_id: u32,
        /// 条目下标。
        entry: u32,
        /// 起始偏移。
        offset: u64,
        /// 剩余字节数（`size − offset`）。
        remaining: u64,
    },
    /// 协议违规：未知 transfer_id / 越界下标 / offset ≥ size / 并发超限 /
    /// Expand base 不连续。调用方断开（docs/02 §4.6 防护规则 2/3/7）。
    Violation {
        /// 违规原因（诊断用）。
        reason: String,
    },
}

/// 源端注册表（每条文件连接一个实例）。
#[derive(Debug, Clone, Default)]
pub struct FileServe {
    manifests: ManifestMap,
    /// 活动（被拉取中）的 transfer_id 集合。
    active: Vec<u32>,
}

/// u32 键小容器（线性 Vec 对，清单量 ≤ 并发上限量级，线性查找足够且零依赖）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ManifestMap {
    keys: Vec<u32>,
    values: Vec<RegisteredManifest>,
}

impl ManifestMap {
    fn get(&self, key: u32) -> Option<&RegisteredManifest> {
        self.keys
            .iter()
            .position(|&k| k == key)
            .map(|i| &self.values[i])
    }

    fn insert(&mut self, key: u32, value: RegisteredManifest) {
        match self.keys.iter().position(|&k| k == key) {
            Some(i) => self.values[i] = value,
            None => {
                self.keys.push(key);
                self.values.push(value);
            }
        }
    }

    fn remove(&mut self, key: u32) {
        if let Some(i) = self.keys.iter().position(|&k| k == key) {
            self.keys.swap_remove(i);
            self.values.swap_remove(i);
        }
    }
}

impl FileServe {
    /// 空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `FileRefAnnounce` 登记（幂等：重复 transfer_id 以最新为准——
    /// 发送端重发清单时刷新条目空间）。
    pub fn register(&mut self, announce: &FileRefAnnounce) {
        self.manifests.insert(
            announce.transfer_id,
            RegisteredManifest {
                entry_sizes: announce.entries.iter().map(|entry| entry.size).collect(),
            },
        );
    }

    /// `FileExpand` 展开：`base` 必须等于该 transfer 当前下标空间上界
    /// （docs/02 §4.6 防护规则 2，接收端对齐校验的同源规则）；不连续 = 违规。
    pub fn register_expand(&mut self, expand: &FileExpand) -> Result<(), String> {
        let Some(manifest) = self.manifests.get_mut_for_write(expand.transfer_id) else {
            return Err(format!(
                "Expand 引用未知 transfer_id {}",
                expand.transfer_id
            ));
        };
        if expand.base != manifest.entry_sizes.len() as u32 {
            return Err(format!(
                "Expand base {} 不等于下标空间上界 {}",
                expand.base,
                manifest.entry_sizes.len()
            ));
        }
        manifest
            .entry_sizes
            .extend(expand.entries.iter().map(|entry| entry.size));
        Ok(())
    }

    /// `FilePullReq` 校验（docs/02 §4.6 防护规则 2/3/7 + docs/07 §9 红线）。
    pub fn on_pull(&mut self, req: &FilePullReq) -> ServeVerdict {
        let Some(manifest) = self.manifests.get(req.transfer_id) else {
            return ServeVerdict::Violation {
                reason: format!("PullReq 引用未知 transfer_id {}", req.transfer_id),
            };
        };
        let Some(size) = manifest.entry_sizes.get(req.entry as usize) else {
            return ServeVerdict::Violation {
                reason: format!(
                    "PullReq 下标 {} 越界（下标空间 {}）",
                    req.entry,
                    manifest.entry_sizes.len()
                ),
            };
        };
        if req.offset >= *size {
            return ServeVerdict::Violation {
                reason: format!("PullReq offset {} ≥ 条目 size {size}", req.offset),
            };
        }
        if !self.active.contains(&req.transfer_id) {
            if self.active.len() >= MAX_ACTIVE_TRANSFERS {
                return ServeVerdict::Violation {
                    reason: format!("活动 transfer 数超上限 {MAX_ACTIVE_TRANSFERS}"),
                };
            }
            self.active.push(req.transfer_id);
        }
        ServeVerdict::Grant {
            transfer_id: req.transfer_id,
            entry: req.entry,
            offset: req.offset,
            remaining: size - req.offset,
        }
    }

    /// 条目服务完毕（`FileEnd` 已产出）/ 传输结束：释放活动槽位。
    pub fn release(&mut self, transfer_id: u32) {
        self.active.retain(|&id| id != transfer_id);
    }

    /// 整单删除（Cancel/超时清理后；后续 PullReq 按未知 id 拒绝）。
    pub fn remove(&mut self, transfer_id: u32) {
        self.release(transfer_id);
        self.manifests.remove(transfer_id);
    }

    /// 当前活动 transfer 数（诊断/测试）。
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// 已登记清单数（诊断/测试）。
    #[must_use]
    pub fn manifest_count(&self) -> usize {
        self.manifests.keys.len()
    }
}

impl ManifestMap {
    fn get_mut_for_write(&mut self, key: u32) -> Option<&mut RegisteredManifest> {
        let index = self.keys.iter().position(|&k| k == key)?;
        self.values.get_mut(index)
    }
}

#[cfg(test)]
mod tests {
    use xcontrol_proto::{FileEntry, FileEntryKind};

    use super::*;

    fn entry(name: &str, size: u64) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            size,
            mtime_ms: 0,
            kind: FileEntryKind::File,
            children: 0,
        }
    }

    fn announce(transfer_id: u32, sizes: &[u64]) -> FileRefAnnounce {
        FileRefAnnounce {
            transfer_id,
            origin_id: [1; 16],
            origin_seq: 1,
            entries: sizes
                .iter()
                .enumerate()
                .map(|(i, size)| entry(&format!("f{i}"), *size))
                .collect(),
            total_bytes: sizes.iter().sum(),
            copied_at_ms: 0,
        }
    }

    fn pull(transfer_id: u32, entry: u32, offset: u64) -> FilePullReq {
        FilePullReq {
            transfer_id,
            entry,
            offset,
        }
    }

    /// Grant 语义：remaining = size − offset；重复拉取同一 transfer 不重复占槽。
    #[test]
    fn grant_and_slot_reuse() {
        let mut serve = FileServe::new();
        serve.register(&announce(7, &[100, 200]));
        assert_eq!(
            serve.on_pull(&pull(7, 0, 40)),
            ServeVerdict::Grant {
                transfer_id: 7,
                entry: 0,
                offset: 40,
                remaining: 60
            }
        );
        assert_eq!(serve.active_count(), 1, "同 transfer 不重复占槽");
        assert_eq!(
            serve.on_pull(&pull(7, 1, 0)),
            ServeVerdict::Grant {
                transfer_id: 7,
                entry: 1,
                offset: 0,
                remaining: 200
            }
        );
        serve.release(7);
        assert_eq!(serve.active_count(), 0);
    }

    /// 索引寻址红线（docs/07 §9）：未知 id / 越界下标 / offset ≥ size 全部违规。
    #[test]
    fn pull_validation_violations() {
        let mut serve = FileServe::new();
        serve.register(&announce(7, &[100]));

        for (req, why) in [
            (pull(9, 0, 0), "未知 transfer_id"),
            (pull(7, 5, 0), "越界下标"),
            (pull(7, 0, 100), "offset == size"),
            (pull(7, 0, 500), "offset > size"),
        ] {
            assert!(
                matches!(serve.on_pull(&req), ServeVerdict::Violation { .. }),
                "{why} 应违规"
            );
        }
        assert_eq!(serve.active_count(), 0, "违规不占槽");
    }

    /// 并发上限（docs/02 §4.6 防护规则 7）：第 5 个不同 transfer 拉取违规。
    #[test]
    fn concurrent_transfer_limit() {
        let mut serve = FileServe::new();
        for id in 1..=4u32 {
            serve.register(&announce(id, &[10]));
        }
        serve.register(&announce(5, &[10]));
        for id in 1..=4u32 {
            assert!(matches!(
                serve.on_pull(&pull(id, 0, 0)),
                ServeVerdict::Grant { .. }
            ));
        }
        assert!(
            matches!(
                serve.on_pull(&pull(5, 0, 0)),
                ServeVerdict::Violation { .. }
            ),
            "第 5 个并发 transfer 违规"
        );
        serve.release(2);
        assert!(
            matches!(serve.on_pull(&pull(5, 0, 0)), ServeVerdict::Grant { .. }),
            "释放槽位后放行"
        );
    }

    /// FileExpand：base 连续性校验 + 下标空间增长后新条目可拉。
    #[test]
    fn expand_extends_index_space() {
        let mut serve = FileServe::new();
        serve.register(&announce(1, &[10]));
        serve.register(&announce(1, &[10])); // 幂等覆盖
        let bad = FileExpand {
            transfer_id: 1,
            parent: 0,
            base: 5,
            entries: vec![entry("x", 7)],
        };
        assert!(serve.register_expand(&bad).is_err(), "base 不连续违规");

        let ok = FileExpand {
            transfer_id: 1,
            parent: 0,
            base: 1,
            entries: vec![entry("x", 7)],
        };
        assert!(serve.register_expand(&ok).is_ok());
        assert_eq!(
            serve.on_pull(&pull(1, 1, 0)),
            ServeVerdict::Grant {
                transfer_id: 1,
                entry: 1,
                offset: 0,
                remaining: 7
            },
            "展开后的新下标可拉取"
        );

        let mut serve = FileServe::new();
        assert!(
            serve.register_expand(&ok).is_err(),
            "未知 transfer 的 Expand 违规"
        );
        serve.remove(1);
        assert!(
            matches!(
                serve.on_pull(&pull(1, 0, 0)),
                ServeVerdict::Violation { .. }
            ),
            "remove 后按未知 id 拒绝"
        );
    }
}
