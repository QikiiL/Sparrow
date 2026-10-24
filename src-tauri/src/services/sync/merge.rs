//! 纯函数合并：本地视图 + peer 快照 + 墓碑 → 插入/删除计划。
//!
//! 语义是 LWW（last-write-wins），比较依据只有时间戳：
//! - 同一 `hash` 在多个 peer 快照里出现时取 `updated_at` 最大的一条；
//! - 墓碑的 `deleted_at` 不早于条目 `updated_at` 时视为"已删除"（平手判删除，
//!   与 [`super::snapshot`] 的过滤口径一致）；
//! - 本地已存在且远端没有更新的记录时不动作（内容按 `hash` 相同，元数据不追）；
//! - 本地更晚的编辑（`updated_at` 晚于墓碑）不会被远端墓碑删除，留给本机快照上传。
//!
//! 本模块不做任何 IO，也不打日志（记录内容不得进日志）。

use std::collections::{HashMap, HashSet};

use crate::models::sync::{Snapshot, SyncRecord};

use super::state::Tombstone;

/// 本地库里的一条条目，只保留合并决策需要的字段。
#[derive(Debug, Clone)]
pub struct LocalItem {
    /// 本地库 content_hash，由调用方填充；合并只按 `updated_at` 决策，
    /// 这里保留给诊断 / 排序用（不删字段，免得调用方构造处全要改）。
    #[allow(dead_code)]
    pub hash: String,
    pub updated_at: i64,
    /// 本地条目类型（0 文本 / 1 链接 / 2 图片），由调用方填充；
    /// 当前仅用于诊断与排序保留，不参与 LWW 判定。
    #[allow(dead_code)]
    pub kind: i32,
}

/// 合并计划的一步。
#[derive(Debug, Clone)]
pub enum MergeAction {
    Insert(SyncRecord),
    Delete(String),
}

/// 计算把 peer 快照合并进本地库所需的动作。
///
/// `self_device` 是本机 device_id：本机自己的快照会被忽略（内容以本地库为准，
/// 回放自己的快照只会把刚删掉的东西又插回来）。
/// 输出按 `hash` 升序排列，保证同样的输入产生同样的计划，便于测试与排查。
pub fn plan_merge(
    local: &HashMap<String, LocalItem>,
    snapshots: &[Snapshot],
    tombstones: &[Tombstone],
    self_device: &str,
) -> Vec<MergeAction> {
    // 1. 每个 hash 的远端最佳记录：updated_at 最大者。
    //    先把 peer 按 device_id 排序，让"时间戳完全相同"时的胜者也是确定的。
    let mut peers: Vec<&Snapshot> = snapshots
        .iter()
        .filter(|s| s.device_id != self_device)
        .collect();
    peers.sort_by(|a, b| a.device_id.cmp(&b.device_id));

    let mut best: HashMap<&str, &SyncRecord> = HashMap::new();
    for snap in peers {
        for rec in &snap.records {
            if rec.hash.is_empty() {
                continue;
            }
            match best.get(rec.hash.as_str()) {
                // 严格大于才替换：时间戳相同时保留先遇到的那条（已按设备 ID 定序）
                Some(cur) if cur.updated_at >= rec.updated_at => {}
                _ => {
                    best.insert(rec.hash.as_str(), rec);
                }
            }
        }
    }

    // 2. 每个 hash 的最新墓碑时间。
    let mut dead: HashMap<&str, i64> = HashMap::new();
    for t in tombstones {
        if t.hash.is_empty() {
            continue;
        }
        match dead.get_mut(t.hash.as_str()) {
            Some(cur) => {
                if t.deleted_at > *cur {
                    *cur = t.deleted_at;
                }
            }
            None => {
                dead.insert(t.hash.as_str(), t.deleted_at);
            }
        }
    }

    // 3. 逐 hash 决策。候选集合 = 远端记录 ∪ 墓碑（本地独有的条目不需要动作，
    //    它们会通过本机快照上传）。
    let mut hashes: HashSet<&str> = HashSet::new();
    hashes.extend(best.keys().copied());
    hashes.extend(dead.keys().copied());

    let mut planned: Vec<(String, MergeAction)> = Vec::new();
    for hash in hashes {
        let rec = best.get(hash).copied();
        let tomb = dead.get(hash).copied();

        // 远端认为已删除：有墓碑，且墓碑不早于该 hash 的最新记录
        // （无记录时也成立，覆盖"本地有、peer 已删"的情形）。
        let remote_deleted = match (rec, tomb) {
            (Some(r), Some(d)) => d >= r.updated_at,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => false,
        };

        if remote_deleted {
            let deleted_at = tomb.unwrap_or(0);
            // 本地更晚的编辑（复活）优先于墓碑；平手判删除。
            if let Some(l) = local.get(hash) {
                if l.updated_at <= deleted_at {
                    planned.push((hash.to_string(), MergeAction::Delete(hash.to_string())));
                }
            }
        } else if let Some(r) = rec {
            // 本地已有 → 不动作；本地没有 → 插入。
            if !local.contains_key(hash) {
                planned.push((hash.to_string(), MergeAction::Insert(r.clone())));
            }
        }
    }

    planned.sort_by(|a, b| a.0.cmp(&b.0));
    planned.into_iter().map(|(_, action)| action).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "device-me";

    fn rec(hash: &str, kind: i32, content: &str, updated_at: i64) -> SyncRecord {
        SyncRecord {
            hash: hash.to_string(),
            kind,
            content: Some(content.to_string()),
            preview: Some(content.to_string()),
            created_at: updated_at,
            updated_at,
            deleted_at: None,
            blob_size: None,
            html_blob: None,
            html_size: None,
            rtf_blob: None,
            rtf_size: None,
        }
    }

    fn snap(device: &str, records: Vec<SyncRecord>) -> Snapshot {
        Snapshot {
            v: 1,
            device_id: device.to_string(),
            device_name: device.to_string(),
            generated_at: 0,
            records,
        }
    }

    fn local_of(items: &[(&str, i64)]) -> HashMap<String, LocalItem> {
        items
            .iter()
            .map(|(hash, updated_at)| {
                (
                    hash.to_string(),
                    LocalItem {
                        hash: hash.to_string(),
                        updated_at: *updated_at,
                        kind: 0,
                    },
                )
            })
            .collect()
    }

    fn tomb(hash: &str, deleted_at: i64) -> Tombstone {
        Tombstone {
            hash: hash.to_string(),
            deleted_at,
        }
    }

    #[test]
    fn empty_inputs_yield_no_actions() {
        assert!(plan_merge(&HashMap::new(), &[], &[], ME).is_empty());
        // 只有自己的快照、没有墓碑 → 也不产生动作
        let own = snap(ME, vec![rec("h1", 0, "a", 100)]);
        assert!(plan_merge(&HashMap::new(), &[own], &[], ME).is_empty());
    }

    #[test]
    fn lww_takes_newest_record() {
        let a = snap("device-a", vec![rec("h1", 0, "old", 100)]);
        let b = snap("device-b", vec![rec("h1", 0, "new", 300)]);
        let plan = plan_merge(&HashMap::new(), &[a, b], &[], ME);
        assert_eq!(plan.len(), 1);
        match &plan[0] {
            MergeAction::Insert(r) => {
                assert_eq!(r.hash, "h1");
                assert_eq!(r.updated_at, 300);
                assert_eq!(r.content.as_deref(), Some("new"));
            }
            other => panic!("期望 Insert，实际 {other:?}"),
        }
    }

    #[test]
    fn tombstone_wins_over_older_record() {
        // 远端还有旧记录，但墓碑更新 → 视为已删除，本地没有就不插入
        let a = snap("device-a", vec![rec("h1", 0, "old", 100)]);
        let plan = plan_merge(&HashMap::new(), &[a], &[tomb("h1", 200)], ME);
        assert!(plan.is_empty(), "墓碑更新时不得复活: {plan:?}");

        // 平手也判删除（与 snapshot 的 deleted_at >= updated_at 口径一致）
        let a = snap("device-a", vec![rec("h1", 0, "old", 200)]);
        assert!(plan_merge(&HashMap::new(), &[a], &[tomb("h1", 200)], ME).is_empty());
    }

    #[test]
    fn record_newer_than_tombstone_resurrects() {
        let a = snap("device-a", vec![rec("h1", 0, "revived", 500)]);
        let plan = plan_merge(&HashMap::new(), &[a], &[tomb("h1", 200)], ME);
        assert_eq!(plan.len(), 1);
        assert!(matches!(&plan[0], MergeAction::Insert(r) if r.updated_at == 500));
    }

    #[test]
    fn same_hash_from_multiple_devices_is_deduped() {
        let a = snap("device-a", vec![rec("h1", 0, "a", 100)]);
        let b = snap("device-b", vec![rec("h1", 0, "b", 300)]);
        let c = snap("device-c", vec![rec("h1", 0, "c", 200)]);
        let plan = plan_merge(&HashMap::new(), &[a, b, c], &[], ME);
        assert_eq!(plan.len(), 1, "同 hash 多设备只应产生一条计划: {plan:?}");
        assert!(matches!(&plan[0], MergeAction::Insert(r) if r.content.as_deref() == Some("b")));
    }

    #[test]
    fn own_snapshot_is_ignored() {
        let mine = snap(ME, vec![rec("h1", 0, "mine", 900)]);
        let plan = plan_merge(&HashMap::new(), &[mine], &[], ME);
        assert!(plan.is_empty(), "不得回放自己的快照: {plan:?}");
    }

    #[test]
    fn remote_tombstone_deletes_existing_local_item() {
        let local = local_of(&[("h1", 100)]);
        let plan = plan_merge(&local, &[], &[tomb("h1", 200)], ME);
        assert_eq!(plan.len(), 1);
        match &plan[0] {
            MergeAction::Delete(h) => assert_eq!(h, "h1"),
            other => panic!("期望 Delete，实际 {other:?}"),
        }
    }

    #[test]
    fn local_newer_edit_survives_remote_tombstone() {
        let local = local_of(&[("h1", 300)]);
        let plan = plan_merge(&local, &[], &[tomb("h1", 200)], ME);
        assert!(plan.is_empty(), "本地更晚的编辑不该被删: {plan:?}");
    }

    #[test]
    fn existing_local_item_is_not_reinserted() {
        let local = local_of(&[("h1", 100)]);
        let a = snap("device-a", vec![rec("h1", 0, "a", 300)]);
        let plan = plan_merge(&local, &[a], &[], ME);
        assert!(plan.is_empty(), "本地已存在时元数据不追: {plan:?}");
    }

    #[test]
    fn output_is_sorted_by_hash() {
        let a = snap(
            "device-a",
            vec![
                rec("h3", 0, "c", 100),
                rec("h1", 0, "a", 100),
                rec("h2", 0, "b", 100),
            ],
        );
        let plan = plan_merge(&HashMap::new(), &[a], &[], ME);
        let hashes: Vec<String> = plan
            .iter()
            .map(|a| match a {
                MergeAction::Insert(r) => r.hash.clone(),
                MergeAction::Delete(h) => h.clone(),
            })
            .collect();
        assert_eq!(hashes, vec!["h1", "h2", "h3"]);
    }

    #[test]
    fn empty_hash_records_are_skipped() {
        let a = snap("device-a", vec![rec("", 0, "x", 100)]);
        assert!(plan_merge(&HashMap::new(), &[a], &[], ME).is_empty());
    }
}
