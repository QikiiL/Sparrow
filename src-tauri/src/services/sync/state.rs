//! 本地同步状态：`config/sync-state.json`。
//!
//! 存的是"不写进 DB 表结构"的同步元数据：设备标识、每个 peer 的 ETag、
//! 墓碑（显式删除的 hash + 时间）、上次上传的快照摘要、已上传的 blob 列表。
//!
//! 读写纪律：
//! - 文件缺失/损坏一律回退默认值，绝不 panic（同步状态坏了不该让应用起不来）；
//! - 写入走 `sync-state.json.tmp` + rename，避免掉电/崩溃留下半个 JSON；
//! - 错误消息只出现文件名（`sync-state.json`），不出现用户目录全路径。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::SyncError;

/// 状态文件名（位于 config 目录）。
pub const STATE_FILE: &str = "sync-state.json";
/// 原子写的临时文件名（同目录，rename 才是原子的）。
const TMP_FILE: &str = "sync-state.json.tmp";
/// 墓碑保留 60 天：足够覆盖一台设备离线两个月的情况，又不至于无限增长。
pub const TOMBSTONE_TTL_MS: i64 = 60 * 24 * 60 * 60 * 1000;
/// 已上传 blob 记录上限（`uploaded_blobs` 是"这个 hash 已经传过"的缓存，
/// 丢了只是多发一次 HEAD，所以可以放心截断）。
pub const MAX_UPLOADED_BLOBS: usize = 2000;

/// 一条删除墓碑：`hash` 在 `deleted_at`（epoch ms）被显式删除。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub hash: String,
    pub deleted_at: i64,
}

/// 一个 peer 的同步进度。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerState {
    /// 上次读到的远端 ETag；None 表示还没成功读过。
    pub etag: Option<String>,
    /// 该快照的 generated_at（epoch ms），仅用于展示/排查。
    pub updated_at: i64,
}

/// `config/sync-state.json` 的内容。
///
/// 所有字段都带 default：字段缺失（旧版本写的文件）等价于"没有同步过"，
/// 不会因为少一个键就丢掉整个状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncStateFile {
    pub device_id: String,
    pub device_name: String,
    pub peers: HashMap<String, PeerState>,
    pub tombstones: Vec<Tombstone>,
    /// 上次上传的快照摘要（sha256 hex），相同则跳过上传。
    pub last_upload_hash: Option<String>,
    /// 已成功上传的 blob hash；push 顺序即时间序，供 [`Self::prune`] 截断。
    pub uploaded_blobs: Vec<String>,
    /// 上次同步用的密钥校验值（= `meta.json` 的 `check`）。
    ///
    /// 与当前不一致说明换了口令或换了远端 meta：远端密文都是旧 key 加密的，
    /// 必须清空上传缓存重新覆盖，否则 digest 没变就永远不会重传。
    pub meta_check: Option<String>,
    /// 上次同步完成时间（epoch ms），0 表示从未同步。
    pub last_sync_at: i64,
}

impl Default for SyncStateFile {
    fn default() -> Self {
        Self {
            device_id: String::new(), // 由 load_or_init 补 UUID
            device_name: default_device_name(),
            peers: HashMap::new(),
            tombstones: Vec::new(),
            last_upload_hash: None,
            uploaded_blobs: Vec::new(),
            meta_check: None,
            last_sync_at: 0,
        }
    }
}

/// 设备名只用于 peer 快照里的展示，取 COMPUTERNAME（Windows），取不到用 "device"。
fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "device".to_string())
}

/// 当前时间（epoch ms）。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn state_path(config_dir: &Path) -> PathBuf {
    config_dir.join(STATE_FILE)
}

/// 读取状态；文件不存在或内容损坏时返回带新 `device_id` 的默认状态。
///
/// 注意：这里**不写盘**。首次生成的 `device_id` 要等调用方 `save()` 之后才固定下来，
/// 所以 service 在第一次同步周期结束时必须保存（计划第 5 节第 11 步）。
pub fn load_or_init(config_dir: &Path) -> SyncStateFile {
    let mut state = std::fs::read_to_string(state_path(config_dir))
        .ok()
        .and_then(|text| match serde_json::from_str::<SyncStateFile>(&text) {
            Ok(state) => Some(state),
            Err(_) => {
                // 不打印文件内容（可能含 peer 名与 hash），只说"损坏，已重置"
                eprintln!("sync: {STATE_FILE} 解析失败，已重置为默认状态");
                None
            }
        })
        .unwrap_or_default();

    if state.device_id.trim().is_empty() {
        state.device_id = uuid::Uuid::new_v4().to_string();
    }
    if state.device_name.trim().is_empty() {
        state.device_name = default_device_name();
    }
    state
}

/// IO 错误转 [`SyncError`]：消息里只有文件名与错误类型，不带目录全路径。
fn io_err(name: &str, e: &std::io::Error) -> SyncError {
    SyncError::config(format!("写入 {name} 失败: {:?}", e.kind()))
}

impl SyncStateFile {
    /// 原子写：先写 `sync-state.json.tmp`，再 rename 到 `sync-state.json`。
    ///
    /// Windows 上 `rename` 覆盖已存在文件会失败，所以先 `remove_file`（NotFound 忽略）。
    /// 删不掉或 rename 失败时退化为直接写目标文件——丢原子性，但总比丢状态好。
    pub fn save(&self, config_dir: &Path) -> Result<(), SyncError> {
        std::fs::create_dir_all(config_dir).map_err(|e| io_err("config 目录", &e))?;

        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| SyncError::config(format!("序列化 {STATE_FILE} 失败: {e}")))?;

        let dest = state_path(config_dir);
        let tmp = config_dir.join(TMP_FILE);

        std::fs::write(&tmp, &bytes).map_err(|e| io_err(TMP_FILE, &e))?;

        match std::fs::remove_file(&dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                // 目标删不掉（被占用/权限），退回直接覆盖写
                let _ = std::fs::remove_file(&tmp);
                std::fs::write(&dest, &bytes).map_err(|e| io_err(STATE_FILE, &e))?;
                return Ok(());
            }
        }

        if std::fs::rename(&tmp, &dest).is_err() {
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(&dest, &bytes).map_err(|e| io_err(STATE_FILE, &e))?;
        }
        Ok(())
    }

    /// 清理过期数据：墓碑超过 60 天丢弃；`uploaded_blobs` 去重并只留最近
    /// [`MAX_UPLOADED_BLOBS`] 条（push 顺序即时间序，越靠后越新）。
    pub fn prune(&mut self, now_ms: i64) {
        self.tombstones
            .retain(|t| now_ms.saturating_sub(t.deleted_at) < TOMBSTONE_TTL_MS);

        // 从尾部往前保留"每个 hash 最近一次出现"，够 2000 条就停
        let mut seen: HashSet<&str> = HashSet::new();
        let mut kept: Vec<String> =
            Vec::with_capacity(self.uploaded_blobs.len().min(MAX_UPLOADED_BLOBS));
        for hash in self.uploaded_blobs.iter().rev() {
            if kept.len() >= MAX_UPLOADED_BLOBS {
                break;
            }
            if seen.insert(hash.as_str()) {
                kept.push(hash.clone());
            }
        }
        kept.reverse();
        self.uploaded_blobs = kept;
    }

    /// 记一条墓碑（同 hash 重复删除时保留较新的时间）。
    pub fn add_tombstone(&mut self, hash: &str, deleted_at: i64) {
        match self.tombstones.iter_mut().find(|t| t.hash == hash) {
            Some(t) => {
                if deleted_at > t.deleted_at {
                    t.deleted_at = deleted_at;
                }
            }
            None => self.tombstones.push(Tombstone {
                hash: hash.to_string(),
                deleted_at,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个独立临时目录，结束时删掉。
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sparrow-sync-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    #[test]
    fn first_load_generates_device_id() {
        let dir = temp_dir();
        let state = load_or_init(&dir);
        assert!(!state.device_id.is_empty(), "首次加载必须生成 device_id");
        assert!(
            uuid::Uuid::parse_str(&state.device_id).is_ok(),
            "device_id 应是 UUID"
        );
        assert!(!state.device_name.trim().is_empty(), "device_name 不能为空");
        assert!(state.tombstones.is_empty());
        assert!(state.uploaded_blobs.is_empty());
        assert_eq!(state.last_sync_at, 0);
        cleanup(&dir);
    }

    #[test]
    fn save_then_reload_is_identical() {
        let dir = temp_dir();
        let mut state = load_or_init(&dir);
        state.device_name = "我的电脑".to_string();
        state.last_sync_at = 1_700_000_000_000;
        state.last_upload_hash = Some("abc123".to_string());
        state.peers.insert(
            "peer-1".to_string(),
            PeerState {
                etag: Some("\"e1\"".to_string()),
                updated_at: 42,
            },
        );
        state.add_tombstone("h1", 1000);
        state.uploaded_blobs.push("b1".to_string());
        state.meta_check = Some("c0ffee".to_string());

        state.save(&dir).expect("保存状态");

        let reloaded = load_or_init(&dir);
        assert_eq!(reloaded.device_id, state.device_id);
        assert_eq!(reloaded.device_name, "我的电脑");
        assert_eq!(reloaded.last_sync_at, state.last_sync_at);
        assert_eq!(reloaded.last_upload_hash.as_deref(), Some("abc123"));
        assert_eq!(
            reloaded.peers.get("peer-1").and_then(|p| p.etag.clone()),
            Some("\"e1\"".to_string())
        );
        assert_eq!(
            reloaded.tombstones,
            vec![Tombstone {
                hash: "h1".to_string(),
                deleted_at: 1000
            }]
        );
        assert_eq!(reloaded.uploaded_blobs, vec!["b1".to_string()]);
        assert_eq!(reloaded.meta_check.as_deref(), Some("c0ffee"));
        // 临时文件不应残留
        assert!(!dir.join(TMP_FILE).exists(), "tmp 文件必须已被 rename 掉");
        cleanup(&dir);
    }

    #[test]
    fn save_is_idempotent_on_existing_file() {
        let dir = temp_dir();
        let mut state = load_or_init(&dir);
        state.save(&dir).expect("首次保存");
        state.last_sync_at = 7;
        state.save(&dir).expect("覆盖保存（Windows 上需先删目标）");
        assert_eq!(load_or_init(&dir).last_sync_at, 7);
        cleanup(&dir);
    }

    #[test]
    fn prune_drops_tombstones_older_than_60_days() {
        let now = 1_800_000_000_000;
        let mut state = SyncStateFile {
            tombstones: vec![
                Tombstone {
                    hash: "old".into(),
                    deleted_at: now - 61 * DAY_MS,
                },
                Tombstone {
                    hash: "edge".into(),
                    deleted_at: now - 60 * DAY_MS,
                }, // 恰好 60 天 → 丢弃
                Tombstone {
                    hash: "fresh".into(),
                    deleted_at: now - 59 * DAY_MS,
                },
            ],
            ..Default::default()
        };

        state.prune(now);

        let hashes: Vec<&str> = state.tombstones.iter().map(|t| t.hash.as_str()).collect();
        assert_eq!(hashes, vec!["fresh"], "只保留 60 天内的墓碑: {hashes:?}");
    }

    #[test]
    fn prune_truncates_blobs_to_limit_keeping_newest() {
        let total = MAX_UPLOADED_BLOBS + 100;
        let mut state = SyncStateFile {
            uploaded_blobs: (0..total).map(|i| format!("b{i}")).collect(),
            ..Default::default()
        };

        state.prune(1_000);

        assert_eq!(state.uploaded_blobs.len(), MAX_UPLOADED_BLOBS);
        assert_eq!(
            state.uploaded_blobs.first().map(String::as_str),
            Some("b100")
        );
        let last_expected = format!("b{}", total - 1);
        assert_eq!(
            state.uploaded_blobs.last().map(String::as_str),
            Some(last_expected.as_str())
        );
    }

    #[test]
    fn prune_dedupes_blobs_keeping_latest_position() {
        let mut state = SyncStateFile {
            uploaded_blobs: vec!["a".into(), "b".into(), "a".into(), "c".into()],
            ..Default::default()
        };

        state.prune(1_000);

        assert_eq!(
            state.uploaded_blobs,
            vec!["b".to_string(), "a".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn corrupt_json_falls_back_to_default() {
        let dir = temp_dir();
        std::fs::write(dir.join(STATE_FILE), b"{ this is not json").expect("写坏文件");

        let state = load_or_init(&dir); // 不得 panic
        assert!(!state.device_id.is_empty(), "损坏后仍要给出可用 device_id");
        assert!(state.peers.is_empty());
        assert!(state.tombstones.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let dir = temp_dir();
        std::fs::write(
            dir.join(STATE_FILE),
            br#"{"device_id":"fixed-id","last_sync_at":5}"#,
        )
        .expect("写部分字段");

        let state = load_or_init(&dir);
        assert_eq!(state.device_id, "fixed-id");
        assert_eq!(state.last_sync_at, 5);
        assert!(
            !state.device_name.trim().is_empty(),
            "缺省 device_name 要回落 COMPUTERNAME/device"
        );
        assert!(state.last_upload_hash.is_none());
        cleanup(&dir);
    }
}
