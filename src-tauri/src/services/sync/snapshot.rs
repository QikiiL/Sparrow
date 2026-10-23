//! 从本地 SQLite 构建本机快照。
//!
//! 时间戳一律交给 SQLite 自己算（`datetime` 列存的是 UTC 文本，`strftime('%s', …)`
//! 转 epoch 秒再 ×1000 得毫秒），避免引 chrono，也避免本地时区参与换算。
//!
//! 日志纪律：本模块只允许打印 `hash` 前 8 位与计数，绝不打印内容、预览或文件路径。

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::models::sync::{Snapshot, SyncRecord, SyncSettings};
use crate::services::exclusion_service;

use super::state::{now_ms, Tombstone};
use super::SyncError;

/// 快照格式版本。字段语义变化时才递增。
pub const SNAPSHOT_VERSION: u32 = 1;

/// 图片记录的预览占位（与前端展示一致）。
const IMAGE_PREVIEW: &str = "[图片]";

/// 只取可同步的三种类型（0 文本 / 1 链接 / 2 图片）；文件路径条目（3）不同步。
const ITEMS_SQL: &str = "
    SELECT type, content, content_hash, preview, file_path,
           CAST(strftime('%s', created_at) AS INTEGER) * 1000 AS created_at_ms,
           CAST(strftime('%s', last_used_at) AS INTEGER) * 1000 AS last_used_at_ms
    FROM items WHERE type IN (0,1,2) ORDER BY id ASC
";

/// 从本地库构建本机快照。
///
/// 过滤规则（纵深防御，任何一条命中就跳过该条目）：
/// - `content_hash` 为空，或非图片条目 `content` 为空；
/// - 文本/链接命中 [`exclusion_service::contains_sensitive`]（凭证类内容不上云）；
/// - 被墓碑覆盖且条目之后没有再更新（`deleted_at >= updated_at`）；
/// - 图片：`sync_images=false`、`file_path` 缺失/读不到、文件超过 `max_blob_mb`。
pub async fn build_snapshot(
    db: &SqlitePool,
    settings: &SyncSettings,
    tombstones: &[Tombstone],
    device_id: &str,
    device_name: &str,
) -> Result<Snapshot, SyncError> {
    let rows = sqlx::query(ITEMS_SQL)
        .fetch_all(db)
        .await
        .map_err(|e| SyncError::config(format!("读取本地条目失败: {e}")))?;

    let max_blob_bytes = u64::from(settings.max_blob_mb) * 1024 * 1024;
    let mut records: Vec<SyncRecord> = Vec::with_capacity(rows.len());

    for row in &rows {
        let kind = column::<i64>(row, "type")?;
        let content: Option<String> = column(row, "content")?;
        let hash = column::<Option<String>>(row, "content_hash")?.unwrap_or_default();
        let preview: Option<String> = column(row, "preview")?;
        let file_path: Option<String> = column(row, "file_path")?;
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
            if !settings.sync_images {
                continue;
            }
            let Some(path) = file_path.as_deref().filter(|p| !p.trim().is_empty()) else {
                continue;
            };
            // 读不到（已删除/权限）就跳过：文件路径不进日志
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let len = meta.len();
            if len > max_blob_bytes {
                eprintln!(
                    "sync: skip oversized image {} ({} bytes)",
                    hash_tag(&hash),
                    len
                );
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

        records.push(SyncRecord {
            hash,
            kind: kind as i32,
            content: Some(text),
            preview: Some(preview),
            created_at,
            updated_at,
            deleted_at: None,
            blob_size: None,
        });
    }

    Ok(Snapshot {
        v: SNAPSHOT_VERSION,
        device_id: device_id.to_string(),
        device_name: device_name.to_string(),
        generated_at: now_ms(),
        records,
    })
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
        pool
    }

    fn settings() -> SyncSettings {
        SyncSettings::default()
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

        let snap = build_snapshot(&pool, &settings(), &[], "dev-a", "A")
            .await
            .expect("构建快照");

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

        let snap = build_snapshot(&pool, &settings(), &[], "dev-a", "A")
            .await
            .unwrap();
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

        let snap = build_snapshot(&pool, &settings(), &[], "dev-a", "A")
            .await
            .unwrap();

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
        let snap = build_snapshot(&pool, &s, &[], "dev-a", "A").await.unwrap();

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
        let snap = build_snapshot(&pool, &off, &[], "dev-a", "A")
            .await
            .unwrap();
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
        let snap = build_snapshot(&pool, &settings(), &tombstones, "dev-a", "A")
            .await
            .unwrap();

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

        let snap = build_snapshot(&pool, &settings(), &[], "dev-a", "A")
            .await
            .unwrap();
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

        let snap = build_snapshot(&pool, &settings(), &[], "dev-a", "A")
            .await
            .unwrap();
        assert!(snap.records.is_empty(), "文件路径条目不同步");
        pool.close().await;
    }
}
