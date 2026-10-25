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
/// 待写墓碑日志（位于 config 目录）。
///
/// 与 `sync-state.json` **解耦**：删除钩子只往这个文件追加一行，不读改写整份状态，
/// 因此不会和同步循环的 `save()` 互相覆盖。同步循环落盘成功后再删掉它。
pub const PENDING_TOMBSTONES_FILE: &str = "sync-tombstones.pending";
/// 原子写的临时文件名（同目录，rename 才是原子的）。
const TMP_FILE: &str = "sync-state.json.tmp";
/// 墓碑保留 60 天：足够覆盖一台设备离线两个月的情况，又不至于无限增长。
pub const TOMBSTONE_TTL_MS: i64 = 60 * 24 * 60 * 60 * 1000;
/// 已上传 blob 记录上限（`uploaded_blobs` 是"这个 hash 已经传过"的缓存，
/// 丢了只是多发一次 HEAD，所以可以放心截断）。
pub const MAX_UPLOADED_BLOBS: usize = 2000;
/// blob 对象 key 的布局版本。
///
/// 1 = `blobs/{hash}.bin`（旧版）；2 = `blobs/{hash}.dat`。
/// Koofr 的 WebDAV 对 `.bin` 的 GET 返回 200 但 body 为空，所以 key 必须换扩展名。
/// 版本升级时 `uploaded_blobs` / `last_upload_hash` 全部作废（旧记录指向的 key 已经没人读）。
pub const BLOB_KEY_VERSION: u32 = 2;

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
    /// 拉取失败后的冷却截止时间（epoch ms）：在此之前不再 GET 这个 peer。
    ///
    /// 远端出现一个读不通的快照（第三台旧设备、被写坏/半截的对象）时，
    /// 若每个 5s 周期都为它跑满重试（5 次、累计约 17s），同步会被一个
    /// 无关对象长期拖慢。冷却期过后再试一次，成功了就清空。
    pub retry_after_ms: Option<i64>,
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
    /// blob 对象 key 的布局版本，见 [`BLOB_KEY_VERSION`]。
    ///
    /// 旧文件没有这个键 → 反序列化为 0 → 触发一次上传缓存清空（重新上传全部附件）。
    /// 字段级 `#[serde(default)]` 必须留着：容器级 default 会取 `Default` 里的
    /// `BLOB_KEY_VERSION`，老文件就永远不会触发迁移。
    #[serde(default)]
    pub blob_key_version: u32,
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
            blob_key_version: BLOB_KEY_VERSION,
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
    // blob key 布局升级：旧的 `.bin` 记录全部作废，否则 `uploaded_blobs` 会让老 blob
    // 永远不重传（远端 `.bin` 对象 GET 回来是空 body）。只改内存，周期结束 `save()` 落盘；
    // 没保存就再清一次，幂等。
    if state.blob_key_version < BLOB_KEY_VERSION {
        state.uploaded_blobs.clear();
        // 快照里的 blob hash 没变 → 不清这个就会一直跳过上传
        state.last_upload_hash = None;
        state.blob_key_version = BLOB_KEY_VERSION;
        eprintln!("sync: blob key 升级到 .dat，已清空上传缓存");
    }
    state
}

/// IO 错误转 [`SyncError`]：消息里只有文件名与错误类型，不带目录全路径。
fn io_err(name: &str, e: &std::io::Error) -> SyncError {
    SyncError::config(format!("写入 {name} 失败: {:?}", e.kind()))
}

/// 读取类错误的映射：同样只带文件名。
fn read_err(name: &str, e: &std::io::Error) -> SyncError {
    SyncError::config(format!("读取 {name} 失败: {:?}", e.kind()))
}

fn pending_path(config_dir: &Path) -> PathBuf {
    config_dir.join(PENDING_TOMBSTONES_FILE)
}

/// 待写日志里的 hash 形态校验：非空、长度合理（≤128）、纯 `[a-z0-9]`。
///
/// 日志行是磁盘上的外部输入（可能被手工编辑/被别的程序写坏），拼进
/// `sync-state.json` 前必须过一遍：只放行 ASCII 小写 hex 与数字，
/// 杜绝引号、换行、`../` 之类的注入/越界值。
fn is_pending_hash(hash: &str) -> bool {
    !hash.is_empty()
        && hash.len() <= 128
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// 追加待写墓碑到 [`PENDING_TOMBSTONES_FILE`]（一次写入 + `sync_all`）。
///
/// 用 `create(true).append(true)` 而不是读改写整份 `sync-state.json`：删除钩子
/// 与同步循环会并发跑，追加写不会互相覆盖。返回实际写入的条数（非法 hash 被丢弃）。
pub fn append_pending_tombstones(
    config_dir: &Path,
    hashes: &[String],
    deleted_at: i64,
) -> Result<usize, SyncError> {
    use std::io::Write;

    let mut buf = String::new();
    let mut written = 0usize;
    for hash in hashes.iter().filter(|hash| is_pending_hash(hash)) {
        let line = serde_json::to_string(&Tombstone {
            hash: hash.clone(),
            deleted_at,
        })
        .map_err(|e| SyncError::config(format!("序列化 {PENDING_TOMBSTONES_FILE} 失败: {e}")))?;
        buf.push_str(&line);
        buf.push('\n');
        written += 1;
    }
    if written == 0 {
        return Ok(0);
    }

    std::fs::create_dir_all(config_dir).map_err(|e| io_err("config 目录", &e))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending_path(config_dir))
        .map_err(|e| io_err(PENDING_TOMBSTONES_FILE, &e))?;
    file.write_all(buf.as_bytes())
        .map_err(|e| io_err(PENDING_TOMBSTONES_FILE, &e))?;
    // 退出即丢的根因就是"只写进页缓存"：这里显式落盘（Windows 上是 FlushFileBuffers）
    file.sync_all()
        .map_err(|e| io_err(PENDING_TOMBSTONES_FILE, &e))?;
    Ok(written)
}

/// 读取待写墓碑日志（**不删文件**）。文件不存在 → 空。
///
/// 坏行（JSON 解析失败 / hash 不合法）跳过，只打行号与计数，不打内容。
/// 同 hash 去重取最大 `deleted_at`，最后按 hash 排序（顺序稳定，digest 不抖）。
pub fn read_pending_tombstones(config_dir: &Path) -> Result<Vec<Tombstone>, SyncError> {
    let text = match std::fs::read_to_string(pending_path(config_dir)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(read_err(PENDING_TOMBSTONES_FILE, &e)),
    };

    let mut out: Vec<Tombstone> = Vec::new();
    let mut bad_lines: Vec<usize> = Vec::new();
    let mut total = 0usize;
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        let parsed = serde_json::from_str::<Tombstone>(line).ok();
        match parsed {
            Some(t) if is_pending_hash(&t.hash) => {
                match out.iter_mut().find(|o| o.hash == t.hash) {
                    Some(existing) => {
                        if t.deleted_at > existing.deleted_at {
                            existing.deleted_at = t.deleted_at;
                        }
                    }
                    None => out.push(t),
                }
            }
            _ => bad_lines.push(line_no),
        }
    }

    if !bad_lines.is_empty() {
        // 只报行号与计数：行内容里可能有 hash，不能进日志
        eprintln!(
            "sync: {PENDING_TOMBSTONES_FILE} 跳过 {} 行无效记录（行号 {:?}，共 {total} 行）",
            bad_lines.len(),
            bad_lines
        );
    }

    out.sort_by(|a, b| a.hash.cmp(&b.hash));
    Ok(out)
}

/// 删除待写墓碑日志（NotFound 视为成功）。
pub fn clear_pending_tombstones(config_dir: &Path) -> Result<(), SyncError> {
    match std::fs::remove_file(pending_path(config_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SyncError::config(format!(
            "删除 {PENDING_TOMBSTONES_FILE} 失败: {:?}",
            e.kind()
        ))),
    }
}

/// 读取并删除（`read` + `clear`）。同步循环**不要**用它：落盘失败时日志必须保留，
/// 循环里应显式 `read_pending_tombstones` → 保存 → `clear_pending_tombstones`。
// 只有测试/简单调用方用；生产路径刻意不用它（会先删后写）
#[allow(dead_code)]
pub fn take_pending_tombstones(config_dir: &Path) -> Result<Vec<Tombstone>, SyncError> {
    let tombstones = read_pending_tombstones(config_dir)?;
    clear_pending_tombstones(config_dir)?;
    Ok(tombstones)
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
                retry_after_ms: Some(1_700_000_060_000),
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

    // ---- blob key 布局迁移（`.bin` → `.dat`） ----

    /// 老状态（没有 `blob_key_version`）必须清空上传缓存：否则 `uploaded_blobs`
    /// 会让所有老 blob 永远不重传，远端 `.bin` 对象又读不回来。
    #[test]
    fn load_migrates_old_blob_key_version() {
        let dir = temp_dir();
        std::fs::write(
            dir.join(STATE_FILE),
            br#"{"device_id":"old","uploaded_blobs":["abc"],"last_upload_hash":"x"}"#,
        )
        .expect("写老版本状态");

        let state = load_or_init(&dir);
        assert!(state.uploaded_blobs.is_empty(), "老 blob 记录必须作废");
        assert!(state.last_upload_hash.is_none(), "快照摘要必须作废");
        assert_eq!(state.blob_key_version, BLOB_KEY_VERSION);
        cleanup(&dir);
    }

    /// 迁移结果落盘后不再重复清空（幂等），新上传的记录要保留。
    #[test]
    fn blob_key_migration_persists_and_keeps_new_uploads() {
        let dir = temp_dir();
        std::fs::write(
            dir.join(STATE_FILE),
            br#"{"device_id":"old","uploaded_blobs":["abc"],"last_upload_hash":"x"}"#,
        )
        .expect("写老版本状态");

        let mut state = load_or_init(&dir);
        assert!(state.uploaded_blobs.is_empty());
        state.uploaded_blobs.push("new-hash".to_string());
        state.last_upload_hash = Some("y".to_string());
        state.save(&dir).expect("保存迁移后的状态");

        let reloaded = load_or_init(&dir);
        assert_eq!(reloaded.blob_key_version, BLOB_KEY_VERSION);
        assert_eq!(
            reloaded.uploaded_blobs,
            vec!["new-hash".to_string()],
            "迁移只做一次，新记录不能被清掉"
        );
        assert_eq!(reloaded.last_upload_hash.as_deref(), Some("y"));
        cleanup(&dir);
    }

    // ---- 待写墓碑日志（sync-tombstones.pending） ----

    #[test]
    fn append_then_take_reads_all_and_removes_file() {
        let dir = temp_dir();
        let hashes = vec!["aaaa".to_string(), "bbbb".to_string()];

        assert_eq!(
            append_pending_tombstones(&dir, &hashes, 1234).expect("追加"),
            2,
            "两条合法 hash 都要写入"
        );
        assert!(dir.join(PENDING_TOMBSTONES_FILE).exists(), "文件应已创建");

        let taken = take_pending_tombstones(&dir).expect("读取并删除");
        assert_eq!(
            taken,
            vec![
                Tombstone {
                    hash: "aaaa".into(),
                    deleted_at: 1234
                },
                Tombstone {
                    hash: "bbbb".into(),
                    deleted_at: 1234
                },
            ]
        );
        assert!(
            !dir.join(PENDING_TOMBSTONES_FILE).exists(),
            "take 之后文件必须消失"
        );
        cleanup(&dir);
    }

    #[test]
    fn append_skips_invalid_hashes_and_creates_no_file() {
        let dir = temp_dir();
        let bad = vec![
            String::new(),
            "../etc/passwd".to_string(),
            "ABC".to_string(),
            "a".repeat(129),
        ];

        assert_eq!(append_pending_tombstones(&dir, &bad, 1).expect("追加"), 0);
        assert!(
            !dir.join(PENDING_TOMBSTONES_FILE).exists(),
            "没有合法 hash 时不该创建文件"
        );
        cleanup(&dir);
    }

    #[test]
    fn read_skips_bad_lines_and_keeps_file() {
        let dir = temp_dir();
        std::fs::write(
            dir.join(PENDING_TOMBSTONES_FILE),
            concat!(
                "{\"hash\":\"aaaa\",\"deleted_at\":10}\n",
                "not json at all\n",
                "{\"hash\":\"bad/../hash\",\"deleted_at\":11}\n",
                "\n",
                "{\"hash\":\"bbbb\"}\n",
                "{\"hash\":\"cccc\",\"deleted_at\":12}\n"
            ),
        )
        .expect("写混合日志");

        let read = read_pending_tombstones(&dir).expect("读日志");
        let hashes: Vec<&str> = read.iter().map(|t| t.hash.as_str()).collect();
        assert_eq!(hashes, vec!["aaaa", "cccc"], "只保留合法行: {hashes:?}");
        assert!(
            dir.join(PENDING_TOMBSTONES_FILE).exists(),
            "read 不删文件（落盘失败时要能重试）"
        );
        cleanup(&dir);
    }

    #[test]
    fn duplicate_hash_keeps_max_deleted_at() {
        let dir = temp_dir();
        std::fs::write(
            dir.join(PENDING_TOMBSTONES_FILE),
            concat!(
                "{\"hash\":\"aaaa\",\"deleted_at\":10}\n",
                "{\"hash\":\"aaaa\",\"deleted_at\":30}\n",
                "{\"hash\":\"aaaa\",\"deleted_at\":20}\n"
            ),
        )
        .expect("写重复日志");

        let read = read_pending_tombstones(&dir).expect("读日志");
        assert_eq!(
            read,
            vec![Tombstone {
                hash: "aaaa".into(),
                deleted_at: 30
            }]
        );
        cleanup(&dir);
    }

    #[test]
    fn empty_or_missing_pending_log_returns_empty() {
        let dir = temp_dir();
        assert!(
            take_pending_tombstones(&dir).expect("缺文件").is_empty(),
            "文件不存在应返回空"
        );

        std::fs::write(dir.join(PENDING_TOMBSTONES_FILE), b"").expect("写空文件");
        assert!(
            read_pending_tombstones(&dir).expect("空文件").is_empty(),
            "空文件应返回空"
        );
        clear_pending_tombstones(&dir).expect("清空文件");
        assert!(!dir.join(PENDING_TOMBSTONES_FILE).exists());
        // 再清一次（NotFound 视为成功）不得报错
        clear_pending_tombstones(&dir).expect("重复清理");
        cleanup(&dir);
    }

    /// 复现"删完直接退出"：日志由上一个进程写，新进程读日志合并进 state。
    #[test]
    fn pending_log_survives_restart_and_merges_into_state() {
        let dir = temp_dir();
        let hashes = vec!["aaaa".to_string(), "bbbb".to_string()];

        // 进程 A：删除钩子只追加日志，此时还没有 sync-state.json
        assert_eq!(
            append_pending_tombstones(&dir, &hashes, 111).expect("追加"),
            2
        );
        assert!(!dir.join(STATE_FILE).exists(), "追加阶段不写 state 文件");

        // 进程 B（新 state）：读日志 → 合并 → 保存 → 清日志
        let mut st = load_or_init(&dir);
        assert!(st.tombstones.is_empty(), "新进程的 state 里本来没有墓碑");
        for t in read_pending_tombstones(&dir).expect("读日志") {
            st.add_tombstone(&t.hash, t.deleted_at);
        }
        st.save(&dir).expect("保存状态");
        clear_pending_tombstones(&dir).expect("清日志");

        let reloaded = load_or_init(&dir);
        let mut got: Vec<&str> = reloaded
            .tombstones
            .iter()
            .map(|t| t.hash.as_str())
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec!["aaaa", "bbbb"], "重启后墓碑必须还在 state 里");
        assert!(
            !dir.join(PENDING_TOMBSTONES_FILE).exists(),
            "清过之后日志文件必须消失"
        );
        cleanup(&dir);
    }
}
