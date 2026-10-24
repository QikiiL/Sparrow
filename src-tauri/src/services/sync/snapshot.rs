//! 从本地 SQLite 构建本机快照。
//!
//! 时间戳一律交给 SQLite 自己算（`datetime` 列存的是 UTC 文本，`strftime('%s', …)`
//! 转 epoch 秒再 ×1000 得毫秒），避免引 chrono，也避免本地时区参与换算。
//!
//! 日志纪律：本模块只允许打印 `hash` 前 8 位与计数，绝不打印内容、预览或文件路径。

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};

use crate::models::sync::{max_blob_bytes, Snapshot, SyncRecord, SyncSettings};
use crate::services::exclusion_service;

use super::state::{now_ms, Tombstone};
use super::SyncError;

/// 快照格式版本。字段语义变化时才递增。
///
/// v2：`SyncRecord` 增加 `html_blob`/`html_size`/`rtf_blob`/`rtf_size`
/// （富文本原始字节的内容寻址 blob）。v1 的 peer 读 v2 快照时会忽略这四个
/// 字段（只同步纯文本），v2 读 v1 快照时按"没有富文本副本"处理。
pub const SNAPSHOT_VERSION: u32 = 2;

/// 图片记录的预览占位（与前端展示一致）。
const IMAGE_PREVIEW: &str = "[图片]";

/// 只取可同步的三种类型（0 文本 / 1 链接 / 2 图片）；文件路径条目（3）不同步。
///
/// `html_path`/`rtf_path` 只用于**读原始字节算 blob hash**，绝不进快照：
/// 它们是本机绝对路径，跨设备毫无意义（对端只会拼出自己的目录）。
const ITEMS_SQL: &str = "
    SELECT type, content, content_hash, preview, file_path, html_path, rtf_path,
           CAST(strftime('%s', created_at) AS INTEGER) * 1000 AS created_at_ms,
           CAST(strftime('%s', last_used_at) AS INTEGER) * 1000 AS last_used_at_ms
    FROM items WHERE type IN (0,1,2) ORDER BY id ASC
";

/// 一次快照构建的结果：快照本体 + 被跳过的图片数 + 用户可见的告警文案。
///
/// 图片被跳过时**不能**只是 `continue`：发送端静默丢图，接收端就永远收不到，
/// 用户只能看到"文本同步正常、图片不同步"而没有任何线索。计数与告警会一路
/// 传到 `CycleReport` / `SyncStatus`，最终出现在状态栏。
#[derive(Debug, Clone)]
pub struct SnapshotBuild {
    pub snapshot: Snapshot,
    /// 因文件缺失 / 0 字节 / 超限被跳过的图片条目数（`sync_images=false` 不计）。
    pub skipped_images: u32,
    /// 因文件缺失 / 0 字节 / 超限 / 读失败被跳过的**富文本格式副本**数。
    /// HTML 与 RTF 各自独立计数（同一条目的两种格式都坏 → +2）：这两种格式
    /// 是彼此独立的 blob，一种丢了不代表另一种也丢了。
    pub skipped_formats: u32,
    /// 每条被跳过图片/格式的告警（只含 hash 前 8 位与原因，不含路径）。
    pub warnings: Vec<String>,
}

/// 从本地库构建本机快照。
///
/// 过滤规则（纵深防御，任何一条命中就跳过该条目）：
/// - `content_hash` 为空，或非图片条目 `content` 为空；
/// - 文本/链接命中 [`exclusion_service::contains_sensitive`]（凭证类内容不上云）；
/// - 被墓碑覆盖且条目之后没有再更新（`deleted_at >= updated_at`）；
/// - 图片：`sync_images=false`（用户主动关闭，不计入 `skipped_images`）、
///   文件缺失/0 字节（先按 `images_dir/{hash 前 16 位}.png` 的命名约定回退）、
///   文件超过 `max_blob_bytes`。
pub async fn build_snapshot(
    db: &SqlitePool,
    settings: &SyncSettings,
    tombstones: &[Tombstone],
    device_id: &str,
    device_name: &str,
    images_dir: &Path,
) -> Result<SnapshotBuild, SyncError> {
    let rows = sqlx::query(ITEMS_SQL)
        .fetch_all(db)
        .await
        .map_err(|e| SyncError::config(format!("读取本地条目失败: {e}")))?;

    let max_bytes = max_blob_bytes(settings);
    let mut records: Vec<SyncRecord> = Vec::with_capacity(rows.len());
    let mut skipped_images: u32 = 0;
    let mut skipped_formats: u32 = 0;
    let mut warnings: Vec<String> = Vec::new();

    for row in &rows {
        let kind = column::<i64>(row, "type")?;
        let content: Option<String> = column(row, "content")?;
        let hash = column::<Option<String>>(row, "content_hash")?.unwrap_or_default();
        let preview: Option<String> = column(row, "preview")?;
        let file_path: Option<String> = column(row, "file_path")?;
        let html_path: Option<String> = column(row, "html_path")?;
        let rtf_path: Option<String> = column(row, "rtf_path")?;
        let created_at = column::<Option<i64>>(row, "created_at_ms")?.unwrap_or(0);
        let updated_at = column::<Option<i64>>(row, "last_used_at_ms")?.unwrap_or(0);

        if hash.is_empty() {
            continue;
        }
        let is_image = kind == 2;
        if !is_image && content.as_deref().unwrap_or("").is_empty() {
            continue;
        }

        // 墓碑覆盖：显式删除后没有再更新过 → 不上传（之后又更新的算复活，保留）
        if let Some(deleted_at) = latest_tombstone(tombstones, &hash) {
            if deleted_at >= updated_at {
                continue;
            }
        }

        if is_image {
            // 用户主动关掉图片同步：整类跳过，不算"出错"，不计数也不告警
            if !settings.sync_images {
                continue;
            }
            let Some(path) = resolve_image_file(file_path.as_deref(), images_dir, &hash) else {
                skipped_images += 1;
                warnings.push(format!("图片 {} 文件缺失", hash_tag(&hash)));
                continue;
            };
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if len > max_bytes {
                skipped_images += 1;
                warnings.push(format!(
                    "图片 {} 超过大小上限（{len} 字节）",
                    hash_tag(&hash)
                ));
                continue;
            }
            records.push(SyncRecord {
                hash,
                kind: kind as i32,
                content: None,
                preview: Some(IMAGE_PREVIEW.to_string()),
                created_at,
                updated_at,
                deleted_at: None,
                blob_size: Some(len),
                html_blob: None,
                html_size: None,
                rtf_blob: None,
                rtf_size: None,
            });
            continue;
        }

        // 文本/链接：最后一道敏感信息闸门（采集时已被排除的内容理论上进不了库，
        // 这里再查一次，防止设置变更/历史数据绕过排除规则）
        let text = content.unwrap_or_default();
        if exclusion_service::contains_sensitive(&text) {
            eprintln!("sync: skip sensitive item {}", hash_tag(&hash));
            continue;
        }

        let preview = preview
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| text.chars().take(100).collect());

        // 富文本副本：对**原始字节**算 sha256 作为内容寻址 blob 键。
        // 不 sanitize、不解析 —— RTF 的 `\bin` 段一旦被解析重构就会损坏，
        // 而 HTML 的净化在预览端（`get_rich_preview`）已经做过一遍。
        //
        // 跳过原因只进 `warnings`（由调用方统一打日志）：本模块不再自己
        // eprintln，避免同一条跳过在日志里出现两次。
        let (html_blob, html_size) = match read_format_blob(html_path.as_deref(), max_bytes) {
            Ok(Some((blob, len))) => (Some(blob), Some(len)),
            Ok(None) => (None, None),
            Err(reason) => {
                skipped_formats += 1;
                warnings.push(format!("富文本 {} 跳过（html {reason}）", hash_tag(&hash)));
                (None, None)
            }
        };
        let (rtf_blob, rtf_size) = match read_format_blob(rtf_path.as_deref(), max_bytes) {
            Ok(Some((blob, len))) => (Some(blob), Some(len)),
            Ok(None) => (None, None),
            Err(reason) => {
                skipped_formats += 1;
                warnings.push(format!("富文本 {} 跳过（rtf {reason}）", hash_tag(&hash)));
                (None, None)
            }
        };

        records.push(SyncRecord {
            hash,
            kind: kind as i32,
            content: Some(text),
            preview: Some(preview),
            created_at,
            updated_at,
            deleted_at: None,
            blob_size: None,
            html_blob,
            html_size,
            rtf_blob,
            rtf_size,
        });
    }

    Ok(SnapshotBuild {
        snapshot: Snapshot {
            v: SNAPSHOT_VERSION,
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
            generated_at: now_ms(),
            records,
        },
        skipped_images,
        skipped_formats,
        warnings,
    })
}

/// 读一个富文本格式文件 → `(sha256 hex, 字节数)`。
///
/// - `Ok(None)`：该条目没有这个格式（列是 NULL/空串）——不是错误，不计数；
/// - `Err(reason)`：有这个格式但拿不到（缺失 / 0 字节 / 超限 / 读失败）——
///   必须计入 `skipped_formats` 并告警，否则对端会静默退化成纯文本；
/// - `Ok(Some(..))`：可以上传的原始字节摘要。
///
/// 0 字节按缺失处理：半截文件传上去，对端只会得到一个打不开的富文本副本。
fn read_format_blob(path: Option<&str>, max_bytes: u64) -> Result<Option<(String, u64)>, &'static str> {
    let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return Err("文件缺失");
    };
    if !meta.is_file() {
        return Err("文件缺失");
    }
    if meta.len() == 0 {
        return Err("0 字节");
    }
    if meta.len() > max_bytes {
        return Err("超过大小上限");
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Err("读取失败");
    };
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(Some((hex, bytes.len() as u64)))
}

/// 定位一条图片条目的文件：先信 `file_path`，不可用时按命名约定回退到
/// `images_dir/{hash 前 16 位}.png`（与 `clipboard_monitor::save_image_to_disk`
/// 的落盘名一致）。
///
/// 0 字节文件视为缺失：写了一半/被清理工具截断的图上传上去，对端只会拿到
/// 一张坏图。返回的路径保证存在、是文件、且非空。
fn resolve_image_file(file_path: Option<&str>, images_dir: &Path, hash: &str) -> Option<PathBuf> {
    if let Some(path) = file_path.map(str::trim).filter(|p| !p.is_empty()) {
        if is_non_empty_file(Path::new(path)) {
            return Some(PathBuf::from(path));
        }
    }
    let fallback = images_dir.join(format!("{}.png", short16(hash)));
    is_non_empty_file(&fallback).then_some(fallback)
}

fn is_non_empty_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.len() > 0,
        Err(_) => false,
    }
}

/// 图片文件名用的 hash 前缀（与 `clipboard_monitor::save_image_to_disk` 一致：16 位）。
fn short16(hash: &str) -> String {
    hash.chars().take(16).collect()
}

/// 快照内容摘要（SHA-256 hex），用于"内容没变就不上传"。
///
/// 只把 `(v, device_id, records 按 hash 排序)` 喂进哈希：
/// - `generated_at` 每次构建都会变，`device_name` 也可能被用户改名，
///   它们一旦参与摘要，"内容没变就不上传"就永远失效（每轮都 PUT 一次）；
/// - `records` 显式按 `hash` 排序，不依赖 SQL 的行序（本地快照按 id 排、
///   合并进来的记录按 hash 排），保证同一份内容永远得到同一个摘要。
pub fn snapshot_digest(snapshot: &Snapshot) -> Result<String, SyncError> {
    let mut records: Vec<&SyncRecord> = snapshot.records.iter().collect();
    records.sort_by(|a, b| a.hash.cmp(&b.hash).then(a.deleted_at.cmp(&b.deleted_at)));

    let mut hasher = Sha256::new();
    hasher.update(b"sparrow-snapshot-digest-v1\0");
    hasher.update(snapshot.v.to_le_bytes());
    hasher.update(snapshot.device_id.as_bytes());
    hasher.update([0u8]);
    for rec in records {
        let bytes = serde_json::to_vec(rec)
            .map_err(|e| SyncError::config(format!("序列化快照记录失败: {e}")))?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// 日志用的 hash 短标识：前 8 个字符（hash 是十六进制，不会切断 UTF-8）。
fn hash_tag(hash: &str) -> String {
    hash.chars().take(8).collect()
}

fn latest_tombstone(tombstones: &[Tombstone], hash: &str) -> Option<i64> {
    tombstones
        .iter()
        .filter(|t| t.hash == hash)
        .map(|t| t.deleted_at)
        .max()
}

/// 按列名取一个可解码值；解码失败报 config 错误（消息里只有列名）。
fn column<'r, T>(row: &'r sqlx::sqlite::SqliteRow, name: &str) -> Result<T, SyncError>
where
    T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    row.try_get(name)
        .map_err(|e| SyncError::config(format!("条目字段 {name} 解码失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单连接内存库：内存库是按连接隔离的，必须锁死 1 个连接。
    ///
    /// 001 只有基础列，`html_path`/`rtf_path` 由 003 追加（生产路径走
    /// `lib.rs::apply_rich_format_columns`）；测试直接照抄那两条 ALTER。
    async fn memory_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("连接内存库");
        sqlx::query(include_str!("../../../migrations/001_create_tables.sql"))
            .execute(&pool)
            .await
            .expect("建 items 表");
        for ddl in [
            "ALTER TABLE items ADD COLUMN html_path TEXT",
            "ALTER TABLE items ADD COLUMN rtf_path TEXT",
        ] {
            sqlx::query(ddl).execute(&pool).await.expect("加富文本列");
        }
        pool
    }

    fn settings() -> SyncSettings {
        SyncSettings::default()
    }

    /// 只关心快照本体的用例：丢弃 skipped/warnings，`images_dir` 指向一个不存在的
    /// 目录，避免"命名约定回退"意外命中别的测试留下的文件。
    async fn snap_of(pool: &SqlitePool, s: &SyncSettings, tombstones: &[Tombstone]) -> Snapshot {
        build_snapshot(pool, s, tombstones, "dev-a", "A", &missing_dir())
            .await
            .expect("构建快照")
            .snapshot
    }

    fn missing_dir() -> PathBuf {
        std::env::temp_dir().join(format!("sparrow-sync-noimg-{}", uuid::Uuid::new_v4()))
    }

    /// 插入一条图片条目（`file_path` 可为空串表示历史数据缺失）。
    async fn insert_image(pool: &SqlitePool, hash: &str, file_path: &str) {
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, preview, file_path, created_at, last_used_at)
             VALUES (2, '[图片]', ?, '[图片]', ?, '1970-01-01 00:00:01', '1970-01-01 00:00:01')",
        )
        .bind(hash)
        .bind(file_path)
        .execute(pool)
        .await
        .expect("插入图片条目");
    }

    /// 每个测试一个独立临时目录，结束时删掉。
    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sparrow-sync-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    async fn insert_text(pool: &SqlitePool, content: &str, hash: &str, created: &str, used: &str) {
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, preview, created_at, last_used_at)
             VALUES (0, ?, ?, NULL, ?, ?)",
        )
        .bind(content)
        .bind(hash)
        .bind(created)
        .bind(used)
        .execute(pool)
        .await
        .expect("插入文本条目");
    }

    #[test]
    fn digest_is_stable_and_tracks_content() {
        let mut snap = Snapshot {
            v: 1,
            device_id: "dev-a".to_string(),
            device_name: "A".to_string(),
            generated_at: 0,
            records: vec![SyncRecord {
                hash: "h1".to_string(),
                kind: 0,
                content: Some("你好".to_string()),
                preview: Some("你好".to_string()),
                created_at: 1,
                updated_at: 2,
                deleted_at: None,
                blob_size: None,
                html_blob: None,
                html_size: None,
                rtf_blob: None,
                rtf_size: None,
            }],
        };

        let first = snapshot_digest(&snap).expect("算摘要");
        assert_eq!(first.len(), 64, "sha256 hex 应是 64 字符");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            first,
            snapshot_digest(&snap).expect("再算一次"),
            "同内容摘要必须稳定"
        );

        // 改内容 → 摘要变
        snap.records[0].content = Some("你好，世界".to_string());
        assert_ne!(first, snapshot_digest(&snap).expect("算摘要"));

        // 改条数 → 摘要也变
        let mut more = snap.clone();
        more.records.push(more.records[0].clone());
        assert_ne!(
            snapshot_digest(&snap).unwrap(),
            snapshot_digest(&more).unwrap()
        );
    }

    /// 摘要必须忽略 `generated_at` 与 `device_name`（否则每轮都判定"内容变了"），
    /// 但 `device_id` 与记录内容必须参与。
    #[test]
    fn digest_ignores_generated_at_and_device_name() {
        let rec = |hash: &str| SyncRecord {
            hash: hash.to_string(),
            kind: 0,
            content: Some("内容".to_string()),
            preview: Some("内容".to_string()),
            created_at: 1,
            updated_at: 2,
            deleted_at: None,
            blob_size: None,
            html_blob: None,
            html_size: None,
            rtf_blob: None,
            rtf_size: None,
        };
        let base = Snapshot {
            v: 1,
            device_id: "dev-a".to_string(),
            device_name: "A".to_string(),
            generated_at: 1_000,
            records: vec![rec("h1"), rec("h2")],
        };

        let first = snapshot_digest(&base).expect("算摘要");
        assert_eq!(first.len(), 64);

        let mut later = base.clone();
        later.generated_at = 9_999_999;
        later.device_name = "另一台机器".to_string();
        assert_eq!(
            first,
            snapshot_digest(&later).expect("算摘要"),
            "generated_at / device_name 不得影响摘要"
        );

        // 记录顺序不影响（按 hash 排序后算）
        let mut reversed = base.clone();
        reversed.records.reverse();
        assert_eq!(
            first,
            snapshot_digest(&reversed).expect("算摘要"),
            "records 顺序不得影响摘要"
        );

        // 设备 ID 参与
        let mut other_device = base.clone();
        other_device.device_id = "dev-b".to_string();
        assert_ne!(
            first,
            snapshot_digest(&other_device).expect("算摘要"),
            "device_id 必须参与摘要"
        );

        // 墓碑记录参与
        let mut with_tombstone = base.clone();
        with_tombstone.records.push(SyncRecord {
            hash: "h3".to_string(),
            kind: 0,
            content: None,
            preview: None,
            created_at: 5,
            updated_at: 5,
            deleted_at: Some(5),
            blob_size: None,
            html_blob: None,
            html_size: None,
            rtf_blob: None,
            rtf_size: None,
        });
        assert_ne!(
            first,
            snapshot_digest(&with_tombstone).expect("算摘要"),
            "墓碑记录必须参与摘要"
        );
    }

    #[tokio::test]
    async fn timestamps_are_epoch_millis_from_sqlite() {
        let pool = memory_pool().await;
        insert_text(
            &pool,
            "你好",
            "h1",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:02",
        )
        .await;

        let snap = snap_of(&pool, &settings(), &[]).await;

        assert_eq!(snap.v, SNAPSHOT_VERSION);
        assert_eq!(snap.device_id, "dev-a");
        assert_eq!(snap.device_name, "A");
        assert_eq!(snap.records.len(), 1);
        let rec = &snap.records[0];
        assert_eq!(rec.created_at, 1_000, "created_at 应为 epoch 毫秒");
        assert_eq!(rec.updated_at, 2_000, "updated_at 取 last_used_at 的毫秒值");
        assert_eq!(rec.kind, 0);
        assert_eq!(rec.content.as_deref(), Some("你好"));
        assert!(rec.deleted_at.is_none());
        assert!(rec.blob_size.is_none());
        pool.close().await;
    }

    #[tokio::test]
    async fn empty_preview_falls_back_to_content_prefix() {
        let pool = memory_pool().await;
        insert_text(
            &pool,
            "一段没有预览的文本",
            "h1",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:01",
        )
        .await;

        let snap = snap_of(&pool, &settings(), &[]).await;
        assert_eq!(
            snap.records[0].preview.as_deref(),
            Some("一段没有预览的文本")
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn sensitive_content_never_enters_snapshot() {
        let pool = memory_pool().await;
        // 拼接构造假 key，避免在源码里出现完整字面量
        let fake_key = format!("{}IOSFODNN7EXAMPLE", "AKIA");
        insert_text(
            &pool,
            &format!("我的 key 是 {fake_key} 别外传"),
            "sec1",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:01",
        )
        .await;
        insert_text(
            &pool,
            "普通内容",
            "ok1",
            "1970-01-01 00:00:02",
            "1970-01-01 00:00:02",
        )
        .await;

        let snap = snap_of(&pool, &settings(), &[]).await;

        assert_eq!(
            snap.records.len(),
            1,
            "敏感条目必须被挡掉: {:?}",
            snap.records
        );
        assert_eq!(snap.records[0].hash, "ok1");
        assert!(
            !serde_json::to_string(&snap).unwrap().contains("AKIA"),
            "快照里不得出现敏感内容"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn oversized_image_is_skipped_and_small_one_kept() {
        let dir = temp_dir();
        let small = dir.join("small.png");
        let big = dir.join("big.png");
        std::fs::write(&small, vec![0u8; 1024]).expect("写小图");
        let f = std::fs::File::create(&big).expect("建大图");
        f.set_len(2 * 1024 * 1024).expect("扩到 2MB"); // 稀疏文件，不实际占 2MB
        drop(f);

        let pool = memory_pool().await;
        for (hash, path) in [("img-small", &small), ("img-big", &big)] {
            sqlx::query(
                "INSERT INTO items (type, content, content_hash, preview, file_path, created_at, last_used_at)
                 VALUES (2, '[图片]', ?, '[图片]', ?, '1970-01-01 00:00:01', '1970-01-01 00:00:01')",
            )
            .bind(hash)
            .bind(path.to_string_lossy().to_string())
            .execute(&pool)
            .await
            .expect("插入图片条目");
        }
        // 文件不存在的图片条目也必须跳过
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, file_path, created_at, last_used_at)
             VALUES (2, '[图片]', 'img-gone', ?, '1970-01-01 00:00:01', '1970-01-01 00:00:01')",
        )
        .bind(dir.join("missing.png").to_string_lossy().to_string())
        .execute(&pool)
        .await
        .expect("插入缺失图片条目");

        let mut s = settings();
        s.max_blob_mb = 1;
        s.sync_images = true;
        let snap = snap_of(&pool, &s, &[]).await;

        assert_eq!(snap.records.len(), 1, "只应保留小图: {:?}", snap.records);
        let rec = &snap.records[0];
        assert_eq!(rec.hash, "img-small");
        assert_eq!(rec.kind, 2);
        assert!(rec.content.is_none(), "图片记录不带原文");
        assert_eq!(rec.preview.as_deref(), Some("[图片]"));
        assert_eq!(rec.blob_size, Some(1024));

        // sync_images=false → 整类跳过
        let mut off = settings();
        off.max_blob_mb = 1;
        off.sync_images = false;
        let snap = snap_of(&pool, &off, &[]).await;
        assert!(snap.records.is_empty(), "关闭图片同步后不得有图片记录");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tombstone_filters_deleted_and_keeps_resurrected() {
        let pool = memory_pool().await;
        // deleted：last_used_at = 2000ms，墓碑 2000ms → 覆盖（平手判删除）
        insert_text(
            &pool,
            "已删除",
            "gone",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:02",
        )
        .await;
        // revived：last_used_at = 3000ms，墓碑 2000ms → 之后又更新，保留
        insert_text(
            &pool,
            "复活",
            "alive",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:03",
        )
        .await;

        let tombstones = vec![
            Tombstone {
                hash: "gone".to_string(),
                deleted_at: 2_000,
            },
            Tombstone {
                hash: "alive".to_string(),
                deleted_at: 2_000,
            },
        ];
        let snap = snap_of(&pool, &settings(), &tombstones).await;

        let hashes: Vec<&str> = snap.records.iter().map(|r| r.hash.as_str()).collect();
        assert_eq!(
            hashes,
            vec!["alive"],
            "被墓碑覆盖的应过滤，之后更新的应复活"
        );
        assert_eq!(snap.records[0].content.as_deref(), Some("复活"));
        pool.close().await;
    }

    #[tokio::test]
    async fn empty_hash_or_content_is_skipped() {
        let pool = memory_pool().await;
        insert_text(
            &pool,
            "有内容",
            "h1",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:01",
        )
        .await;
        insert_text(
            &pool,
            "",
            "h-empty",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:01",
        )
        .await;
        insert_text(
            &pool,
            "没哈希",
            "",
            "1970-01-01 00:00:01",
            "1970-01-01 00:00:01",
        )
        .await;

        let snap = snap_of(&pool, &settings(), &[]).await;
        let hashes: Vec<&str> = snap.records.iter().map(|r| r.hash.as_str()).collect();
        assert_eq!(hashes, vec!["h1"]);
        pool.close().await;
    }

    #[tokio::test]
    async fn file_items_are_not_synced() {
        let pool = memory_pool().await;
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, created_at, last_used_at)
             VALUES (3, 'C:\\a.txt', 'file1', '1970-01-01 00:00:01', '1970-01-01 00:00:01')",
        )
        .execute(&pool)
        .await
        .expect("插入文件条目");

        let snap = snap_of(&pool, &settings(), &[]).await;
        assert!(snap.records.is_empty(), "文件路径条目不同步");
        pool.close().await;
    }

    /// `file_path` 不可用（历史数据 / 换过存储目录）时必须按命名约定回退到
    /// `images_dir/{hash 前 16 位}.png`，而不是静默丢图。
    #[tokio::test]
    async fn build_snapshot_falls_back_to_hash_filename() {
        let dir = temp_dir();
        let images = dir.join("images");
        std::fs::create_dir_all(&images).expect("建 images 目录");
        let hash = "0123456789abcdef0123456789abcdef";
        let fallback = images.join(format!("{}.png", &hash[..16]));
        std::fs::write(&fallback, vec![7u8; 128]).expect("写回退文件");

        let pool = memory_pool().await;
        // file_path 指向不存在的文件 → 走命名约定回退
        insert_image(&pool, hash, &dir.join("gone.png").to_string_lossy()).await;

        let build = build_snapshot(&pool, &settings(), &[], "dev-a", "A", &images)
            .await
            .expect("构建快照");

        assert_eq!(
            build.skipped_images, 0,
            "回退命中就不算跳过: {:?}",
            build.warnings
        );
        assert!(build.warnings.is_empty(), "回退成功不该有告警");
        assert_eq!(build.snapshot.records.len(), 1, "回退后图片应进快照");
        let rec = &build.snapshot.records[0];
        assert_eq!(rec.hash, hash);
        assert_eq!(rec.kind, 2);
        assert_eq!(rec.blob_size, Some(128), "应按回退文件的实际大小上报");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件确实缺失 → 计数 + 告警（告警只含 hash 前 8 位，不含路径）。
    #[tokio::test]
    async fn missing_image_file_is_counted_and_warned() {
        let dir = temp_dir();
        let images = dir.join("images");
        std::fs::create_dir_all(&images).expect("建 images 目录");
        let hash = "fedcba9876543210fedcba9876543210";

        let pool = memory_pool().await;
        insert_image(&pool, hash, &dir.join("missing.png").to_string_lossy()).await;

        let build = build_snapshot(&pool, &settings(), &[], "dev-a", "A", &images)
            .await
            .expect("构建快照");

        assert!(build.snapshot.records.is_empty(), "缺文件的图片不得进快照");
        assert_eq!(build.skipped_images, 1, "必须计数，不能静默丢图");
        assert_eq!(build.warnings.len(), 1);
        let warning = &build.warnings[0];
        assert!(
            warning.contains(&hash[..8]),
            "告警应带 hash 前 8 位: {warning}"
        );
        assert!(!warning.contains(&hash[8..]), "不得出现完整 hash: {warning}");
        assert!(!warning.contains("missing.png"), "告警不得含文件路径: {warning}");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 0 字节文件视为缺失（半张图传上去对端只会拿到坏图）；用户关掉图片同步时不计数。
    #[tokio::test]
    async fn zero_byte_image_is_skipped() {
        let dir = temp_dir();
        let images = dir.join("images");
        std::fs::create_dir_all(&images).expect("建 images 目录");
        let hash = "aabbccddeeff0011aabbccddeeff0011";
        let empty = images.join(format!("{}.png", &hash[..16]));
        std::fs::write(&empty, b"").expect("写 0 字节文件");

        let pool = memory_pool().await;
        insert_image(&pool, hash, &empty.to_string_lossy()).await;

        let build = build_snapshot(&pool, &settings(), &[], "dev-a", "A", &images)
            .await
            .expect("构建快照");
        assert!(build.snapshot.records.is_empty(), "0 字节图片不得进快照");
        assert_eq!(build.skipped_images, 1);
        assert_eq!(build.warnings.len(), 1);

        // 用户主动关闭图片同步：不算错误，不计数也不告警
        let off = SyncSettings {
            sync_images: false,
            ..settings()
        };
        let build = build_snapshot(&pool, &off, &[], "dev-a", "A", &images)
            .await
            .expect("构建快照");
        assert_eq!(build.skipped_images, 0, "关闭图片同步不该产生告警");
        assert!(build.warnings.is_empty());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 富文本副本（HTML / RTF 原始字节）
    // -----------------------------------------------------------------------

    /// 插入一条带富文本副本的文本条目（`html`/`rtf` 为文件路径，None 表示该列 NULL）。
    async fn insert_rich_text(
        pool: &SqlitePool,
        hash: &str,
        html: Option<&str>,
        rtf: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, preview, html_path, rtf_path)
             VALUES (0, '富文本正文', ?, NULL, ?, ?)",
        )
        .bind(hash)
        .bind(html)
        .bind(rtf)
        .execute(pool)
        .await
        .expect("插入富文本条目");
    }

    fn hex_of(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// 有 HTML/RTF 文件时，快照必须带上**原始字节**的 sha256 与字节数，
    /// 而且绝不能把本机绝对路径写进快照。
    #[tokio::test]
    async fn snapshot_includes_html_blob_hash_and_size() {
        let dir = temp_dir();
        // 含 CF_HTML 头的中文正文 + 带 \bin 段的 RTF（两者都必须原样搬运）
        let html: &[u8] = b"Version:0.9\r\nStartHTML:0000000105\r\n<html><body>\xe4\xbd\xa0\xe5\xa5\xbd</body></html>";
        let rtf: &[u8] = b"{\\rtf1\\ansi\\bin4 abcd\\par hi}";
        let html_path = dir.join("rich1.html");
        let rtf_path = dir.join("rich1.rtf");
        std::fs::write(&html_path, html).expect("写 html");
        std::fs::write(&rtf_path, rtf).expect("写 rtf");

        let pool = memory_pool().await;
        insert_rich_text(
            &pool,
            "rich1",
            Some(&html_path.to_string_lossy()),
            Some(&rtf_path.to_string_lossy()),
        )
        .await;

        let build = build_snapshot(&pool, &settings(), &[], "dev-a", "A", &missing_dir())
            .await
            .expect("构建快照");
        assert_eq!(build.skipped_formats, 0, "文件都在，不该有跳过");
        assert!(build.warnings.is_empty());

        let rec = &build.snapshot.records[0];
        assert_eq!(rec.content.as_deref(), Some("富文本正文"), "纯文本照常同步");
        assert_eq!(
            rec.html_blob.as_deref(),
            Some(hex_of(html).as_str()),
            "html_blob 必须是原始字节的 sha256 hex"
        );
        assert_eq!(rec.html_size, Some(html.len() as u64));
        assert_eq!(rec.rtf_blob.as_deref(), Some(hex_of(rtf).as_str()));
        assert_eq!(rec.rtf_size, Some(rtf.len() as u64));

        let json = serde_json::to_string(&build.snapshot).expect("序列化快照");
        assert!(!json.contains("rich1.html"), "快照不得含本机路径");
        assert!(!json.contains("rich1.rtf"), "快照不得含本机路径");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 单个格式超过 `max_blob_bytes` → 只丢这一种格式（置 None + 计数 + 告警），
    /// 条目本身与另一种格式照常同步。
    #[tokio::test]
    async fn oversized_format_falls_back_to_plain_text() {
        let dir = temp_dir();
        let big = dir.join("big.html");
        let f = std::fs::File::create(&big).expect("建大 html");
        f.set_len(2 * 1024 * 1024).expect("扩到 2MB"); // 稀疏文件，不实际占 2MB
        drop(f);
        let small = dir.join("small.rtf");
        std::fs::write(&small, b"{\\rtf1 small}").expect("写小 rtf");

        let hash = "0123456789abcdef0123456789abcdef";
        let pool = memory_pool().await;
        insert_rich_text(
            &pool,
            hash,
            Some(&big.to_string_lossy()),
            Some(&small.to_string_lossy()),
        )
        .await;

        let mut s = settings();
        s.max_blob_mb = 1;
        let build = build_snapshot(&pool, &s, &[], "dev-a", "A", &missing_dir())
            .await
            .expect("构建快照");

        assert_eq!(build.skipped_formats, 1, "只该计一次（html 超限）");
        assert_eq!(build.warnings.len(), 1);
        let warning = &build.warnings[0];
        assert!(warning.contains(&hash[..8]), "告警应带 hash 前 8 位: {warning}");
        assert!(!warning.contains(&hash[8..]), "不得出现完整 hash: {warning}");
        assert!(!warning.contains("big.html"), "告警不得含路径: {warning}");

        assert_eq!(build.snapshot.records.len(), 1, "条目本身必须保留（纯文本降级）");
        let rec = &build.snapshot.records[0];
        assert_eq!(rec.content.as_deref(), Some("富文本正文"));
        assert!(rec.html_blob.is_none(), "超限格式置 None");
        assert!(rec.html_size.is_none());
        assert_eq!(
            rec.rtf_blob.as_deref(),
            Some(hex_of(b"{\\rtf1 small}").as_str()),
            "另一种格式不受影响"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 列里有路径但文件已不在（被清理/换过存储目录）→ 该格式置 None + 计数；
    /// 两列都是 NULL 的普通条目一个都不计。
    #[tokio::test]
    async fn missing_format_file_is_ignored() {
        let dir = temp_dir();
        let gone = dir.join("gone.html");

        let hash = "fedcba9876543210fedcba9876543210";
        let pool = memory_pool().await;
        insert_rich_text(&pool, hash, Some(&gone.to_string_lossy()), None).await;
        insert_rich_text(&pool, "plain", None, None).await;

        let build = build_snapshot(&pool, &settings(), &[], "dev-a", "A", &missing_dir())
            .await
            .expect("构建快照");

        assert_eq!(build.skipped_formats, 1, "只有带失效路径的那条计数");
        assert_eq!(build.warnings.len(), 1);
        let warning = &build.warnings[0];
        assert!(warning.contains(&hash[..8]), "{warning}");
        assert!(!warning.contains("gone.html"), "告警不得含路径: {warning}");

        assert_eq!(build.snapshot.records.len(), 2, "两条都要同步纯文本");
        let rich = build
            .snapshot
            .records
            .iter()
            .find(|r| r.hash == hash)
            .expect("富文本条目仍在");
        assert!(rich.html_blob.is_none());
        assert!(rich.html_size.is_none());
        assert!(rich.rtf_blob.is_none());
        assert_eq!(rich.content.as_deref(), Some("富文本正文"));

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
