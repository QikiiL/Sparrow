//! 一次同步周期：拉取 peer 快照 → 合并进本地库 → 上传图片 blob → 上传本机快照。
//!
//! 设计要点：
//! - 入口 [`run_cycle`] 只依赖 [`CycleCtx`]，不碰 `AppHandle`：单测可以直接构造
//!   （mock 后端 + `sqlite::memory:` + 临时目录），不必起 Tauri 应用。
//! - 所有 HTTP 调用（ureq 是阻塞的）都在 [`tauri::async_runtime::spawn_blocking`]
//!   里跑，DB 用 sqlx 直接 await。`retry` 里的 `thread::sleep` 因此不会阻塞
//!   tokio 的 worker。
//! - `emit` 只在周期开始（`syncing`）与成功结束（`idle`）各调一次；
//!   失败只返回错误，由调用方决定怎么上报（这样错误信息不会被 emit 吞掉）。
//! - 日志纪律（见实施计划第 6 节）：只允许出现操作名、对象 key 的
//!   [`key_tag`]、hash 前 8 位、计数与字节数；禁止内容明文、文件全路径、URL、凭证。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::{Row, SqlitePool};
use tauri::{Emitter, Listener, Manager};

use crate::models::sync::{
    Snapshot, SyncProvider, SyncRecord, SyncSecrets, SyncSettings, SyncStatus,
};
use crate::services::credentials;
use crate::services::sync::backend::{build_backend, ObjectMeta, SyncBackend};
use crate::services::sync::crypto::KeyCache;
use crate::services::sync::merge::{self, MergeAction};
use crate::services::sync::state::{self, PeerState, SyncStateFile, Tombstone};
use crate::services::sync::{crypto, key_tag, snapshot, SyncError, SyncErrorKind};

/// 远端目录（相对 root，后端的 key 不含 root）。
const DEVICES_DIR: &str = "devices";
const BLOBS_DIR: &str = "blobs";
/// 图片条目的 content/preview 占位，与 `snapshot.rs` / 前端展示一致。
const IMAGE_PREVIEW: &str = "[图片]";
/// 一次操作最多尝试 3 次（首次 + 2 次重试）。
const MAX_ATTEMPTS: u32 = 3;

/// 重试退避：500ms → 2s（计划第 6 节；3 次尝试只用到前两档）。
#[cfg(not(test))]
const RETRY_BACKOFF_MS: [u64; 2] = [500, 2000];
/// 测试里把退避压到毫秒级，避免整个测试套件为退避多等 2.5s；
/// 重试判定逻辑与生产路径完全一致，只是等待时间不同。
#[cfg(test)]
const RETRY_BACKOFF_MS: [u64; 2] = [1, 2];

/// 一次同步周期的全部输入。
///
/// 刻意不持有 `AppHandle`：命令层/循环层（task 4b）负责把 settings、密钥、
/// images 目录解析好再传进来，本模块因此可以在普通 tokio 测试里跑。
pub struct CycleCtx {
    pub db: SqlitePool,
    pub backend: Arc<dyn SyncBackend>,
    /// Argon2id 派生出的同步密钥（AES-256-GCM）。
    pub key: [u8; 32],
    pub settings: SyncSettings,
    /// config 目录（`state.rs` 在其中读写 `sync-state.json`）。
    pub state_path: PathBuf,
    pub images_dir: PathBuf,
    /// 状态上报回调；周期开始发 `syncing`，成功结束发 `idle`。
    pub emit: Box<dyn Fn(SyncStatus) + Send + Sync>,
}

/// 一次周期的结果，供调用方决定状态上报与下一轮间隔。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleReport {
    /// 本轮是否真的上传了本机快照。
    pub uploaded: bool,
    /// 本轮读到的 peer 快照数。
    pub peer_count: usize,
    /// 从远端合并进本地库的条目数。
    pub inserted: usize,
    /// 按远端墓碑删掉的本地条目数。
    pub deleted: usize,
    /// 待上传标记：成功周期恒为 0（上传成功，或 digest 未变即内容没变），
    /// 只有循环层在失败路径上才把它报成 1。否则"内容没变"的正常周期会
    /// 长期显示"有待同步"。
    pub pending: u32,
}

/// 跑一个完整同步周期。
///
/// 失败时**不** emit（由调用方发 `error`），但仍会把已经产生的本地状态落盘：
/// 周期中途删掉的条目对应的墓碑不能因为一次上传失败而丢失，否则下个周期
/// 那条内容会被 peer 的快照"复活"。
pub async fn run_cycle(ctx: &CycleCtx) -> Result<CycleReport, SyncError> {
    let mut st = state::load_or_init(&ctx.state_path);

    (ctx.emit)(SyncStatus {
        state: "syncing".to_string(),
        device_id: st.device_id.clone(),
        last_sync_at: st.last_sync_at,
        last_error: None,
        pending: 0,
    });

    let outcome = cycle_body(ctx, &mut st).await;

    match &outcome {
        Ok(report) => (ctx.emit)(SyncStatus {
            state: "idle".to_string(),
            device_id: st.device_id.clone(),
            last_sync_at: st.last_sync_at,
            last_error: None,
            pending: report.pending,
        }),
        Err(e) => {
            if let Err(save_err) = st.save(&ctx.state_path) {
                eprintln!("sync: 状态保存失败 kind={}", save_err.kind.as_str());
            }
            eprintln!("sync: cycle 失败 kind={} {}", e.kind.as_str(), e.message);
        }
    }

    outcome
}

/// 周期主体：出错时直接返回，不 emit。
async fn cycle_body(ctx: &CycleCtx, st: &mut SyncStateFile) -> Result<CycleReport, SyncError> {
    // 0. 密钥校验值变了（换口令 / 换了远端 meta）→ 远端所有密文都是旧 key 加密的，
    //    必须清空上传缓存重新覆盖。否则"内容没变 → digest 命中 → 不上传"会让
    //    远端永久留下一份谁也解不开的旧密文。
    //    `prepare` 已经用常量时间比较确认过 `meta.check == HMAC(key)`，
    //    所以这里直接由实际使用的 key 算出同一个值，避免再多传一个字段。
    let meta_check = crypto::key_check(&ctx.key);
    if st.meta_check.as_deref() != Some(meta_check.as_str()) {
        st.last_upload_hash = None;
        st.uploaded_blobs.clear();
        st.meta_check = Some(meta_check);
    }

    // 1. 扫远端 peers（LIST + 按 ETag 增量 GET）
    let snapshots = fetch_peer_snapshots(ctx, st).await?;

    // 2. 远端墓碑先并入本地状态。`merge::plan_merge` 只看 `tombstones` 参数、
    //    不看 `SyncRecord::deleted_at`，所以必须先并进来：否则远端墓碑记录
    //    会被当成"一条新条目"插进本地库（content 为 None 的幽灵条目）。
    for snap in &snapshots {
        for rec in &snap.records {
            if let Some(deleted_at) = rec.deleted_at {
                if !rec.hash.is_empty() {
                    st.add_tombstone(&rec.hash, deleted_at);
                }
            }
        }
    }

    // 3. 合并
    let local = local_view(&ctx.db).await?;
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    for action in merge::plan_merge(&local, &snapshots, &st.tombstones, &st.device_id) {
        match action {
            MergeAction::Insert(rec) => {
                if insert_record(ctx, &rec).await? {
                    inserted += 1;
                }
            }
            MergeAction::Delete(hash) => {
                if delete_local(&ctx.db, &ctx.images_dir, &hash).await? {
                    deleted += 1;
                }
                st.add_tombstone(&hash, state::now_ms());
            }
        }
    }

    // 4. 裁剪过期墓碑 / 超限的 blob 记录
    st.prune(state::now_ms());

    // 5. 构建本机快照，并把墓碑作为 deleted_at 记录追加进去
    //    （peer 只有看到记录才知道"我们删了什么"）
    //
    //    设备名以设置为准（UI 里填的名字必须生效）；设置为空时沿用 state 文件里
    //    的 COMPUTERNAME 兜底，绝不让对端看到空名字。
    let name = if ctx.settings.device_name.trim().is_empty() {
        st.device_name.clone()
    } else {
        ctx.settings.device_name.trim().to_string()
    };
    let mut snap = snapshot::build_snapshot(
        &ctx.db,
        &ctx.settings,
        &st.tombstones,
        &st.device_id,
        &name,
    )
    .await?;
    append_tombstone_records(&mut snap, &st.tombstones);

    // 6. 上传图片 blob（先 blob 后快照：快照一上去 peer 就会来取 blob）
    upload_blobs(ctx, st, &snap).await?;

    // 7. 上传本机快照
    let uploaded = upload_snapshot(ctx, st, &snap).await?;

    // 8. 收尾落盘（设备名以本轮实际用上的为准）
    st.device_name = name;
    st.last_sync_at = state::now_ms();
    st.save(&ctx.state_path)?;

    Ok(CycleReport {
        uploaded,
        peer_count: snapshots.len(),
        inserted,
        deleted,
        // 走到这里说明本轮所有远端操作都成功了：上传过（uploaded）
        // 或内容没变（digest 命中）都算"没有待同步的东西"。
        pending: 0,
    })
}

// ---------------------------------------------------------------------------
// 远端交互：每个调用都是一个 spawn_blocking 里的 retry 循环
// ---------------------------------------------------------------------------

/// 在阻塞线程池里执行一次 HTTP 操作（ureq 是阻塞 API）。
async fn io<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SyncError> + Send + 'static,
) -> Result<T, SyncError> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| SyncError::network(format!("join: {e}")))?
}

/// 仅 `Network` / `Server` 重试，最多 [`MAX_ATTEMPTS`] 次；其它错误立即返回。
///
/// 日志只在最终失败时打一行（操作名 + key_tag + kind），不打印 URL/响应体。
fn retry<T>(
    op: &str,
    key: &str,
    mut f: impl FnMut() -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    let mut attempt: u32 = 1;
    loop {
        match f() {
            Ok(value) => return Ok(value),
            Err(e) => {
                if !e.kind.retryable() || attempt >= MAX_ATTEMPTS {
                    eprintln!(
                        "sync: {op} 失败: key={} kind={} attempts={attempt}",
                        key_tag(key),
                        e.kind.as_str()
                    );
                    return Err(e);
                }
                let delay = RETRY_BACKOFF_MS[(attempt - 1) as usize];
                std::thread::sleep(std::time::Duration::from_millis(delay));
                attempt += 1;
            }
        }
    }
}

async fn remote_list(ctx: &CycleCtx, prefix: &str) -> Result<Vec<ObjectMeta>, SyncError> {
    let backend = Arc::clone(&ctx.backend);
    let key = prefix.to_string();
    io(move || retry("LIST", &key, || backend.list(&key))).await
}

async fn remote_get(ctx: &CycleCtx, key: &str) -> Result<Option<Vec<u8>>, SyncError> {
    let backend = Arc::clone(&ctx.backend);
    let key = key.to_string();
    io(move || retry("GET", &key, || backend.get(&key))).await
}

async fn remote_head(ctx: &CycleCtx, key: &str) -> Result<Option<ObjectMeta>, SyncError> {
    let backend = Arc::clone(&ctx.backend);
    let key = key.to_string();
    io(move || retry("HEAD", &key, || backend.head(&key))).await
}

async fn remote_put(
    ctx: &CycleCtx,
    key: &str,
    body: Vec<u8>,
    if_match: Option<String>,
) -> Result<Option<String>, SyncError> {
    let backend = Arc::clone(&ctx.backend);
    let key = key.to_string();
    io(move || {
        retry("PUT", &key, || {
            backend.put(&key, &body, if_match.as_deref())
        })
    })
    .await
}

// ---------------------------------------------------------------------------
// 1. 拉取 peer 快照
// ---------------------------------------------------------------------------

async fn fetch_peer_snapshots(
    ctx: &CycleCtx,
    st: &mut SyncStateFile,
) -> Result<Vec<Snapshot>, SyncError> {
    let metas = remote_list(ctx, DEVICES_DIR).await?;
    let mut out: Vec<Snapshot> = Vec::with_capacity(metas.len());

    for meta in metas {
        let Some(device) = device_id_from_key(&meta.key) else {
            continue;
        };
        if device == st.device_id {
            continue;
        }
        // ETag 没变就不用 GET（这是"准实时"的全部成本来源）
        let known = st.peers.get(device).and_then(|p| p.etag.as_deref());
        if let (Some(remote), Some(known)) = (meta.etag.as_deref(), known) {
            if remote == known {
                continue;
            }
        }

        let Some(bytes) = remote_get(ctx, &meta.key).await? else {
            continue;
        };
        // AAD = 对象 key（与上传时一致）
        match crypto::open_json::<Snapshot>(&ctx.key, &meta.key, &bytes) {
            Ok(snap) => {
                st.peers.insert(
                    device.to_string(),
                    PeerState {
                        etag: meta.etag.clone(),
                        updated_at: snap.generated_at,
                    },
                );
                out.push(snap);
            }
            // 口令错就是口令错：解不开任何 peer 的密文时必须整轮显式失败。
            // 跳过 + 报"已同步"会让用户以为一切正常，而错口令期间本机还会用错
            // key 覆盖自己的快照，改回正确口令后 digest 又没变、不再重传。
            Err(e) if e.kind == SyncErrorKind::Auth => {
                eprintln!("sync: peer 解密失败 kind={}（整轮中止）", e.kind.as_str());
                return Err(SyncError::auth(crypto::AUTH_MISMATCH));
            }
            // 其它错误（信封损坏等）仍只跳过：不能因为一个坏文件中断整轮
            Err(e) => eprintln!(
                "sync: skip peer {} kind={}",
                short_id(device),
                e.kind.as_str()
            ),
        }
    }

    Ok(out)
}

/// `devices/{id}.json` → `{id}`；不是这个形状的 key 一律忽略。
fn device_id_from_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(DEVICES_DIR)?.strip_prefix('/')?;
    let id = rest.strip_suffix(".json")?;
    if id.is_empty() || id.contains('/') {
        None
    } else {
        Some(id)
    }
}

// ---------------------------------------------------------------------------
// 2. 合并执行
// ---------------------------------------------------------------------------

/// 本地条目视图：只要 `plan_merge` 需要的三列。
async fn local_view(db: &SqlitePool) -> Result<HashMap<String, merge::LocalItem>, SyncError> {
    let rows = sqlx::query(
        "SELECT content_hash,
                CAST(strftime('%s', last_used_at) AS INTEGER) * 1000 AS updated_ms,
                type
         FROM items WHERE content_hash <> ''",
    )
    .fetch_all(db)
    .await
    .map_err(db_err)?;

    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let hash: String = row.try_get("content_hash").map_err(sqlx_err)?;
        let updated_at: Option<i64> = row.try_get("updated_ms").map_err(sqlx_err)?;
        let kind: i64 = row.try_get("type").map_err(sqlx_err)?;
        out.insert(
            hash.clone(),
            merge::LocalItem {
                hash,
                updated_at: updated_at.unwrap_or(0),
                kind: kind as i32,
            },
        );
    }
    Ok(out)
}

/// 执行一条 `Insert`。返回 true 表示真的新增了一行（"已存在只更新 last_used_at" 返回 false）。
async fn insert_record(ctx: &CycleCtx, rec: &SyncRecord) -> Result<bool, SyncError> {
    // 防御：墓碑记录不是可插入的条目（plan_merge 正常不会给出，但别让
    // 一条 deleted_at 记录变成 content 为空的幽灵行）
    if rec.hash.is_empty() || rec.deleted_at.is_some() {
        return Ok(false);
    }

    let existing: Option<i64> =
        sqlx::query_scalar("SELECT id FROM items WHERE content_hash = ? LIMIT 1")
            .bind(&rec.hash)
            .fetch_optional(&ctx.db)
            .await
            .map_err(db_err)?;
    if existing.is_some() {
        sqlx::query("UPDATE items SET last_used_at = datetime('now') WHERE content_hash = ?")
            .bind(&rec.hash)
            .execute(&ctx.db)
            .await
            .map_err(db_err)?;
        return Ok(false);
    }

    if rec.kind == 2 {
        return insert_image(ctx, rec).await;
    }

    let Some(content) = rec.content.as_deref().filter(|c| !c.is_empty()) else {
        return Ok(false);
    };
    let preview = rec
        .preview
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| content.chars().take(100).collect());

    sqlx::query(
        "INSERT INTO items (type, content, content_hash, preview, created_at, last_used_at)
         VALUES (?, ?, ?, ?, datetime(? / 1000, 'unixepoch'), datetime(? / 1000, 'unixepoch'))",
    )
    .bind(rec.kind)
    .bind(content)
    .bind(&rec.hash)
    .bind(preview)
    .bind(rec.created_at)
    .bind(rec.updated_at)
    .execute(&ctx.db)
    .await
    .map_err(db_err)?;

    Ok(true)
}

/// 图片条目：先把 blob 落到 `images_dir/{hash 前 16 位}.png`（已存在则复用），再插行。
async fn insert_image(ctx: &CycleCtx, rec: &SyncRecord) -> Result<bool, SyncError> {
    // 本机不同步图片时也不要把 peer 的图片拉下来（用户关掉图片同步是明确的意图）
    if !ctx.settings.sync_images {
        return Ok(false);
    }

    // 远端 hash 会拼进 blob key 与本地文件名：只接受 ≥32 位纯 ASCII hex，
    // 否则 `../` 之类的内容能把路径拼出 images 目录。
    if !is_blob_hash(&rec.hash) {
        eprintln!("sync: skip image {} (hash 非法)", short_id(&rec.hash));
        return Ok(false);
    }
    if let Some(size) = rec.blob_size {
        let max_bytes = u64::from(ctx.settings.max_blob_mb) * 1024 * 1024;
        if size > max_bytes {
            eprintln!(
                "sync: skip image {} (远端声明 {} 字节，超过 {} MB)",
                short_id(&rec.hash),
                size,
                ctx.settings.max_blob_mb
            );
            return Ok(false);
        }
    }

    let path = ctx.images_dir.join(format!("{}.png", short16(&rec.hash)));
    if !path.exists() {
        let key = blob_key(&rec.hash);
        let Some(sealed) = remote_get(ctx, &key).await? else {
            eprintln!("sync: skip image {} (blob 缺失)", short_id(&rec.hash));
            return Ok(false);
        };
        let plain = match crypto::open_blob(&ctx.key, &key, &sealed) {
            Ok(plain) => plain,
            Err(e) => {
                eprintln!(
                    "sync: skip image {} kind={}",
                    short_id(&rec.hash),
                    e.kind.as_str()
                );
                return Ok(false);
            }
        };
        write_atomic(&ctx.images_dir, &path, &plain)?;
    }

    let file_path = path.to_string_lossy().to_string();
    sqlx::query(
        "INSERT INTO items (type, content, content_hash, file_path, preview, created_at, last_used_at)
         VALUES (2, ?, ?, ?, ?, datetime(? / 1000, 'unixepoch'), datetime(? / 1000, 'unixepoch'))",
    )
    .bind(IMAGE_PREVIEW)
    .bind(&rec.hash)
    .bind(&file_path)
    .bind(IMAGE_PREVIEW)
    .bind(rec.created_at)
    .bind(rec.updated_at)
    .execute(&ctx.db)
    .await
    .map_err(db_err)?;

    Ok(true)
}

/// 执行一条 `Delete`：先取 `file_path` 再删行，最后按 images 目录守卫删文件。
async fn delete_local(db: &SqlitePool, images_dir: &Path, hash: &str) -> Result<bool, SyncError> {
    let file_path: Option<String> =
        sqlx::query_scalar("SELECT file_path FROM items WHERE content_hash = ? LIMIT 1")
            .bind(hash)
            .fetch_optional(db)
            .await
            .map_err(db_err)?;

    let res = sqlx::query("DELETE FROM items WHERE content_hash = ?")
        .bind(hash)
        .execute(db)
        .await
        .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }

    if let Some(path) = file_path {
        remove_local_image(images_dir, &path);
    }
    Ok(true)
}

/// 删除 images 目录内的图片文件。
///
/// `image_cleanup::remove_image_file` 需要 `AppHandle`（它走
/// `storage_service::resolve_image_path`），本模块刻意不依赖 Tauri，
/// 所以在这里复刻同一条守卫：规范化后必须落在 images 目录内才删。
fn remove_local_image(images_dir: &Path, file_path: &str) {
    let Ok(dir) = std::fs::canonicalize(images_dir) else {
        return;
    };
    let Ok(canonical) = std::fs::canonicalize(file_path) else {
        return;
    };
    if canonical.starts_with(&dir) {
        let _ = std::fs::remove_file(&canonical);
    }
}

/// 把本地墓碑追加成快照记录（`deleted_at` 有值），peer 据此才知道我们删了什么。
fn append_tombstone_records(snap: &mut Snapshot, tombstones: &[Tombstone]) {
    for t in tombstones {
        if t.hash.is_empty() {
            continue;
        }
        snap.records.push(SyncRecord {
            hash: t.hash.clone(),
            kind: 0,
            content: None,
            preview: None,
            created_at: t.deleted_at,
            updated_at: t.deleted_at,
            deleted_at: Some(t.deleted_at),
            blob_size: None,
        });
    }
}

// ---------------------------------------------------------------------------
// 3. 上传
// ---------------------------------------------------------------------------

/// 上传快照里引用的、本机还没传过的图片 blob。
async fn upload_blobs(
    ctx: &CycleCtx,
    st: &mut SyncStateFile,
    snap: &Snapshot,
) -> Result<(), SyncError> {
    let images = image_rows(&ctx.db).await?;
    let done: HashSet<String> = st.uploaded_blobs.iter().cloned().collect();
    let max_bytes = u64::from(ctx.settings.max_blob_mb) * 1024 * 1024;

    for rec in snap
        .records
        .iter()
        .filter(|r| r.kind == 2 && r.deleted_at.is_none())
    {
        if done.contains(&rec.hash) {
            continue;
        }
        let Some(path) = images
            .iter()
            .find(|(hash, _)| hash == &rec.hash)
            .and_then(|(_, path)| path.clone())
        else {
            eprintln!("sync: skip blob {} (无本地文件)", short_id(&rec.hash));
            continue;
        };

        let Some(bytes) = read_blob_file(&path, max_bytes).await? else {
            eprintln!(
                "sync: skip blob {} (文件缺失或超过 {} MB)",
                short_id(&rec.hash),
                ctx.settings.max_blob_mb
            );
            continue;
        };

        let key = blob_key(&rec.hash);
        let sealed = crypto::seal_blob(&ctx.key, &key, &bytes)?;
        // 无条件 PUT：换口令后必须用新 key 覆盖旧密文，条件写会让 412 把
        // "内容寻址、已经存在"误当成"无需上传"，远端就永远留着解不开的旧密文。
        remote_put(ctx, &key, sealed, None).await?;
        st.uploaded_blobs.push(rec.hash.clone());
    }

    Ok(())
}

/// `SELECT content_hash, file_path FROM items WHERE type = 2`。
async fn image_rows(db: &SqlitePool) -> Result<Vec<(String, Option<String>)>, SyncError> {
    let rows = sqlx::query("SELECT content_hash, file_path FROM items WHERE type = 2")
        .fetch_all(db)
        .await
        .map_err(db_err)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let hash: String = row.try_get("content_hash").map_err(sqlx_err)?;
        let path: Option<String> = row.try_get("file_path").map_err(sqlx_err)?;
        out.push((hash, path));
    }
    Ok(out)
}

/// 读图片文件（在阻塞池里跑）；缺失/非文件/超限返回 None。
async fn read_blob_file(path: &str, max_bytes: u64) -> Result<Option<Vec<u8>>, SyncError> {
    let path = path.to_string();
    io(move || {
        let Ok(meta) = std::fs::metadata(&path) else {
            return Ok(None);
        };
        if !meta.is_file() || meta.len() > max_bytes {
            return Ok(None);
        }
        std::fs::read(&path).map(Some).map_err(|e| {
            // 只报错误类型，不报路径
            SyncError::config(format!("读取图片失败: {:?}", e.kind()))
        })
    })
    .await
}

/// 上传本机快照（`If-Match` 乐观并发）。返回是否真的上传了。
async fn upload_snapshot(
    ctx: &CycleCtx,
    st: &mut SyncStateFile,
    snap: &Snapshot,
) -> Result<bool, SyncError> {
    let digest = snapshot::snapshot_digest(snap)?;
    if st.last_upload_hash.as_deref() == Some(digest.as_str()) {
        return Ok(false);
    }

    let key = format!("{DEVICES_DIR}/{}.json", st.device_id);
    let body = crypto::seal_json(&ctx.key, &key, snap)?;
    // 首次上传（还没有自己的 ETag）用 If-None-Match: *，之后用 If-Match
    let if_match = st
        .peers
        .get(&st.device_id)
        .and_then(|p| p.etag.clone())
        .unwrap_or_else(|| "*".to_string());

    let etag = match remote_put(ctx, &key, body.clone(), Some(if_match)).await {
        Ok(etag) => etag,
        Err(e) if e.kind == SyncErrorKind::Conflict => {
            // 远端已有同名对象（同 deviceId 双开，或本地状态文件被删过）：
            // 取当前 ETag 重试一次，仍冲突就明确报"设备 ID 冲突"。
            let Some(meta) = remote_head(ctx, &key).await? else {
                return Err(device_conflict(&st.device_id));
            };
            match remote_put(ctx, &key, body, meta.etag).await {
                Ok(etag) => etag,
                Err(e2) if e2.kind == SyncErrorKind::Conflict => {
                    return Err(device_conflict(&st.device_id));
                }
                Err(e2) => return Err(e2),
            }
        }
        Err(e) => return Err(e),
    };

    st.last_upload_hash = Some(digest);
    st.peers.insert(
        st.device_id.clone(),
        PeerState {
            etag,
            updated_at: snap.generated_at,
        },
    );
    Ok(true)
}

fn device_conflict(device_id: &str) -> SyncError {
    SyncError::conflict(format!(
        "设备 ID 冲突: device={}（另一台设备在用同一个 deviceId，请重置设备 ID）",
        short_id(device_id)
    ))
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn blob_key(hash: &str) -> String {
    format!("{BLOBS_DIR}/{hash}.bin")
}

/// blob hash 的形态校验：≥32 位纯 ASCII hex。
///
/// 远端给的 hash 会拼进对象 key（`blobs/{hash}.bin`）与本地文件名
/// （`{hash 前 16 位}.png`），所以必须是 hex：`../`、`/`、`:` 都不合法。
fn is_blob_hash(hash: &str) -> bool {
    hash.len() >= 32 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 日志用短标识：前 8 个字符（按 char 截断，不会切断 UTF-8）。
fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

/// 图片文件名用的 hash 前缀（与 `clipboard_monitor::save_image_to_disk` 一致：16 位）。
fn short16(hash: &str) -> String {
    hash.chars().take(16).collect()
}

/// 先写 `.tmp` 再 rename，避免掉电/崩溃留下半张图。
fn write_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| SyncError::config(format!("创建 images 目录失败: {:?}", e.kind())))?;
    let tmp = dest.with_extension("png.tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|e| SyncError::config(format!("写入图片临时文件失败: {:?}", e.kind())))?;
    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        SyncError::config(format!("图片落盘失败: {:?}", e.kind()))
    })
}

fn db_err(e: sqlx::Error) -> SyncError {
    SyncError::config(format!("数据库操作失败: {e}"))
}

fn sqlx_err(e: sqlx::Error) -> SyncError {
    SyncError::config(format!("条目字段解码失败: {e}"))
}

// ---------------------------------------------------------------------------
// 运行句柄与轮询循环（接线层）
//
// 这一层负责所有"必须碰 Tauri / 阻塞 API"的事：读配置、读 keyring、派生密钥、
// 解析 images 目录、上报状态。同步语义全在 [`run_cycle`] 里，本层只搬运。
// ---------------------------------------------------------------------------

/// 本地变更事件名：复制入库（`clipboard-changed`）与显式删除（`item-deleted`）
/// 都要尽快唤醒同步，否则最坏要等 2× 轮询间隔。
const LOCAL_CHANGE_EVENTS: [&str; 2] = ["clipboard-changed", "item-deleted"];
/// 唤醒去抖窗口：连续复制时只在最后一次事件之后唤醒一次同步。
const WAKE_DEBOUNCE: Duration = Duration::from_millis(400);
/// 同步自己写库后向前端广播的事件名（前端收到任意 payload 就整表重查）。
const CLIPBOARD_CHANGED: &str = "clipboard-changed";

/// 未启用时的配置轮询间隔：只读一次内存里的配置副本，不碰网络与密钥。
const DISABLED_POLL: Duration = Duration::from_secs(2);
/// 出错后的最小重试间隔（计划第 6 节：max(poll, 30s)）。
const ERROR_MIN_INTERVAL: Duration = Duration::from_secs(30);

const STATE_DISABLED: &str = "disabled";
const STATE_SYNCING: &str = "syncing";
const STATE_IDLE: &str = "idle";
const STATE_ERROR: &str = "error";
/// 缺口令时的用户可见提示（不含任何凭证内容）。
const NEED_PASSPHRASE: &str = "需要设置同步口令";

/// 派生密钥缓存：`fingerprint = sha256(passphrase + salt)`。
/// 口令与 salt 都没变时直接复用，跳过每次 0.1–0.3s 的 Argon2id。
/// 类型定义在 `crypto`（缓存命中仍要校验 `meta.check`），这里只存一份。
struct Inner {
    app: tauri::AppHandle,
    db: SqlitePool,
    /// config 目录：`sync-state.json` 在这里读写。
    config_dir: PathBuf,
    /// 配置副本：设置面板保存后由 [`SyncHandle::set_settings`] 替换。
    settings: tokio::sync::Mutex<SyncSettings>,
    /// 对外状态：命令层同步读取，所以用 std Mutex（临界区只有一次 clone）。
    status: std::sync::Mutex<SyncStatus>,
    /// 唤醒信号：`sync_now` / 配置变更 / 删除钩子都靠它打断等待。
    notify: tokio::sync::Notify,
    key_cache: tokio::sync::Mutex<Option<KeyCache>>,
    /// 待落盘的墓碑（删除钩子入队，周期开始前写入 state 文件）。
    pending_tombstones: tokio::sync::Mutex<HashSet<String>>,
    /// 周期正在写库：期间（含周期末尾的 `clipboard-changed`）不把事件当成
    /// 新的本地变更，避免"同步 → 事件 → 再同步"的回环。
    applying: AtomicBool,
    /// 去抖代次：每次本地变更事件自增，只有最后一次事件之后 400ms 无新事件
    /// 才真正唤醒循环。
    generation: AtomicU64,
}

impl Inner {
    /// 更新并广播状态（事件名 `sync-status`，payload 即 [`SyncStatus`]）。
    /// 内容没变就不重复广播：未启用时循环每 2s 一轮，不该每轮都往 WebView 推事件。
    fn publish(&self, status: SyncStatus) {
        let mut current = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if same_status(&current, &status) {
            return;
        }
        *current = status.clone();
        drop(current);
        let _ = self.app.emit("sync-status", &status);
    }
}

/// 两个状态是否等价（用于广播去重）。
fn same_status(a: &SyncStatus, b: &SyncStatus) -> bool {
    a.state == b.state
        && a.device_id == b.device_id
        && a.last_sync_at == b.last_sync_at
        && a.last_error == b.last_error
        && a.pending == b.pending
}

/// 同步服务的运行句柄。由 `lib.rs` 在 setup 里 `app.manage`。
///
/// 内部只有 `Arc<Inner>`，所以 `Clone` 是廉价的引用计数：事件监听回调、
/// 命令层、设置保存都可以各持一份。
#[derive(Clone)]
pub struct SyncHandle {
    inner: Arc<Inner>,
}

impl SyncHandle {
    /// 配置变更：替换副本并唤醒循环（启用/停用、改地址都在下一轮生效）。
    pub fn set_settings(&self, s: SyncSettings) {
        let inner = Arc::clone(&self.inner);
        // 锁是 tokio 的，而本方法是同步签名（设置保存路径不能 await）：
        // 换副本这一步交给运行时，唤醒在锁释放后再发
        tauri::async_runtime::spawn(async move {
            *inner.settings.lock().await = s;
            inner.notify.notify_one();
        });
    }

    /// 当前状态（`get_sync_status` 用）。
    pub fn status(&self) -> SyncStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 立即跑一轮：用户点「立即同步」，或凭证变更后需要重新派生密钥。
    pub fn sync_now(&self) {
        self.inner.notify.notify_one();
    }

    /// 入队待写墓碑（显式删除/清空后调用）。去重，空串忽略。
    pub fn queue_tombstones(&self, hashes: Vec<String>) {
        if hashes.is_empty() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        tauri::async_runtime::spawn(async move {
            let mut pending = inner.pending_tombstones.lock().await;
            for hash in hashes {
                if !hash.is_empty() {
                    pending.insert(hash);
                }
            }
            drop(pending);
            inner.notify.notify_one();
        });
    }
}

/// 删除钩子的便捷入口：`SyncHandle` 还没 manage（单测 / 未接线）时静默跳过。
pub fn queue_tombstones(app: &tauri::AppHandle, hashes: Vec<String>) {
    if hashes.is_empty() {
        return;
    }
    if let Some(handle) = app.try_state::<SyncHandle>() {
        handle.queue_tombstones(hashes);
    }
}

/// 启动同步循环并返回句柄。
///
/// **不做任何网络 IO**：配置、密钥、后端都在循环里取，任何一步失败只影响
/// 同步状态（`error`），绝不阻断应用启动。
pub fn start(app: tauri::AppHandle, db: SqlitePool, config_dir: PathBuf) -> SyncHandle {
    let cfg = crate::services::settings_service::load_settings(&app).sync;
    let st = state::load_or_init(&config_dir);
    let inner = Arc::new(Inner {
        app,
        db,
        config_dir,
        settings: tokio::sync::Mutex::new(cfg.clone()),
        status: std::sync::Mutex::new(initial_status(&cfg, &st)),
        notify: tokio::sync::Notify::new(),
        key_cache: tokio::sync::Mutex::new(None),
        pending_tombstones: tokio::sync::Mutex::new(HashSet::new()),
        applying: AtomicBool::new(false),
        generation: AtomicU64::new(0),
    });
    register_wakeup(&inner);
    tauri::async_runtime::spawn(run_loop(Arc::clone(&inner)));
    SyncHandle { inner }
}

/// 注册本地变更唤醒：`clipboard-changed` / `item-deleted` 到达后去抖唤醒循环。
///
/// 防回环：同步自己写库后也会 emit `clipboard-changed`（刷新前端列表），
/// 周期期间 `applying` 为 true，回调直接忽略，因此不会"同步 → 事件 → 再同步"。
fn register_wakeup(inner: &Arc<Inner>) {
    for event in LOCAL_CHANGE_EVENTS {
        // 只持 Weak：监听器挂在 AppHandle 上，强引用会让 Inner 永远无法释放
        let weak = Arc::downgrade(inner);
        let _ = inner.app.listen(event, move |_event| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.applying.load(Ordering::SeqCst) {
                return;
            }
            let mine = next_generation(&inner.generation);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(WAKE_DEBOUNCE).await;
                // 窗口内又有事件 → 代次已变，交给那一次唤醒
                if is_latest_generation(&inner.generation, mine) {
                    inner.notify.notify_one();
                }
            });
        });
    }
}

/// 事件到达：代次自增，返回本次事件的代次号（纯逻辑，便于单测）。
fn next_generation(generation: &AtomicU64) -> u64 {
    generation.fetch_add(1, Ordering::SeqCst) + 1
}

/// 去抖窗口结束时：代次没被后来的事件改过，才说明这是最后一次事件。
fn is_latest_generation(generation: &AtomicU64, mine: u64) -> bool {
    generation.load(Ordering::SeqCst) == mine
}

/// 设备名兜底：设置为空（前端默认值就是空串）时回落 `fallback`
/// （生产路径传 `COMPUTERNAME`），再取不到用 `"device"`。
/// `fallback` 由调用方传入而不是在这里读环境变量，单测才能确定性地覆盖。
fn resolve_device_name(configured: &str, fallback: Option<&str>) -> String {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    fallback
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("device")
        .to_string()
}

/// 生产路径的设备名兜底：读一次 `COMPUTERNAME`。
fn device_name_or_computer_name(configured: &str) -> String {
    resolve_device_name(configured, std::env::var("COMPUTERNAME").ok().as_deref())
}

/// 初始状态：未启用 → `disabled`，否则 `idle`（还没跑过任何周期）。
fn initial_status(cfg: &SyncSettings, st: &SyncStateFile) -> SyncStatus {
    SyncStatus {
        state: if cfg.enabled {
            STATE_IDLE
        } else {
            STATE_DISABLED
        }
        .to_string(),
        device_id: st.device_id.clone(),
        last_sync_at: st.last_sync_at,
        last_error: None,
        pending: 0,
    }
}

/// 轮询间隔：`poll_interval_sec` 夹到 2–300s（0 会让循环空转）。
fn poll_interval(cfg: &SyncSettings) -> Duration {
    Duration::from_secs(u64::from(cfg.poll_interval_sec.clamp(2, 300)))
}

/// `ensure_dirs` 的配置指纹：provider + 地址 + bucket + root。
/// 只有这些变了才需要重新 MKCOL（WebDAV），否则每轮一次多余请求。
fn ensure_fingerprint(cfg: &SyncSettings) -> String {
    let (provider, endpoint) = match cfg.provider {
        SyncProvider::Webdav => ("webdav", cfg.webdav_url.trim()),
        SyncProvider::S3 => ("s3", cfg.s3_endpoint.trim()),
    };
    format!(
        "{provider}|{endpoint}|{}|{}",
        cfg.s3_bucket.trim(),
        crate::services::sync::root_of(cfg)
    )
}

/// 一轮同步需要的远端前置。
struct Prepared {
    backend: Arc<dyn SyncBackend>,
    key: [u8; 32],
    fingerprint: String,
}

/// 构建后端 → （必要时）建目录 → 读/建 meta.json 并校验口令 → 取密钥。
///
/// 全部在阻塞线程池里跑：`ureq` 与 Argon2id 都是阻塞 API。
///
/// meta 的对象 key 是 root 相对的 `meta.json`（后端 `url()` 自己拼 `{root}/`），
/// 所以这里**不**需要 root。口令校验失败会返回 [`SyncErrorKind::Auth`]，
/// 绝不会拿一把错 key 继续跑周期。
fn prepare(
    cfg: &SyncSettings,
    secrets: &SyncSecrets,
    need_ensure: bool,
    cached: Option<KeyCache>,
) -> Result<Prepared, SyncError> {
    let backend = build_backend(cfg, secrets)?;
    // WebDAV 需要目录先存在（S3 是空实现），只在配置指纹变化后做
    if need_ensure {
        backend.ensure_dirs()?;
    }
    // 口令与 salt 都没变时复用缓存的 key（省一轮 0.1–0.3s 的 Argon2id），
    // 但无论走哪条路都要用 `meta.check` 校验口令，错口令一律 Auth 失败。
    let (meta, key) = match cached.as_ref() {
        Some(cache) => {
            crypto::load_or_init_meta_cached(backend.as_ref(), &secrets.passphrase, Some(cache))?
        }
        None => crypto::load_or_init_meta(backend.as_ref(), &secrets.passphrase)?,
    };
    Ok(Prepared {
        backend: Arc::from(backend),
        key,
        fingerprint: crypto::key_fingerprint(&secrets.passphrase, &meta.salt),
    })
}

/// 上报失败状态：`pending = 1`（有本地变更还没上去）。
fn publish_error(inner: &Inner, device_id: &str, last_sync_at: i64, message: String) {
    inner.publish(SyncStatus {
        state: STATE_ERROR.to_string(),
        device_id: device_id.to_string(),
        last_sync_at,
        last_error: Some(message),
        pending: 1,
    });
}

/// 等待 `dur`，但被 [`Inner::notify`] 唤醒时立即返回。
async fn wait(inner: &Inner, dur: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(dur) => {}
        _ = inner.notify.notified() => {}
    }
}

/// 轮询循环：配置 → 密钥 → 后端 → 墓碑落盘 → 一个周期 → 等待。
async fn run_loop(inner: Arc<Inner>) {
    // 上次成功 ensure_dirs 的配置指纹。循环局部即可：重启后多一次 MKCOL 无害
    let mut last_ensured: Option<String> = None;
    let boot = state::load_or_init(&inner.config_dir);
    let mut device_id = boot.device_id;
    let mut last_sync_at = boot.last_sync_at;

    loop {
        let cfg = inner.settings.lock().await.clone();

        // 1. 未启用：只报状态，不消耗重试
        if !cfg.enabled {
            inner.publish(SyncStatus {
                state: STATE_DISABLED.to_string(),
                device_id: device_id.clone(),
                last_sync_at,
                last_error: None,
                pending: 0,
            });
            wait(&inner, DISABLED_POLL).await;
            continue;
        }

        // 2. 开始一轮
        inner.publish(SyncStatus {
            state: STATE_SYNCING.to_string(),
            device_id: device_id.clone(),
            last_sync_at,
            last_error: None,
            pending: 0,
        });

        let interval = poll_interval(&cfg);
        let retry_interval = interval.max(ERROR_MIN_INTERVAL);

        // 3. 凭证（keyring / 环境变量，阻塞 API → 线程池）
        let secrets = match tauri::async_runtime::spawn_blocking(credentials::load_all).await {
            Ok(Ok(secrets)) => secrets,
            Ok(Err(e)) => {
                publish_error(&inner, &device_id, last_sync_at, e);
                wait(&inner, retry_interval).await;
                continue;
            }
            Err(e) => {
                publish_error(&inner, &device_id, last_sync_at, format!("读取凭据失败: {e}"));
                wait(&inner, retry_interval).await;
                continue;
            }
        };
        if secrets.passphrase.trim().is_empty() {
            publish_error(&inner, &device_id, last_sync_at, NEED_PASSPHRASE.to_string());
            wait(&inner, retry_interval).await;
            continue;
        }

        // 4. 后端 + meta + 密钥（阻塞池）
        let cached = inner.key_cache.lock().await.clone();
        let ensure_fp = ensure_fingerprint(&cfg);
        let need_ensure = last_ensured.as_deref() != Some(ensure_fp.as_str());
        let prepared = {
            let cfg = cfg.clone();
            tauri::async_runtime::spawn_blocking(move || {
                prepare(&cfg, &secrets, need_ensure, cached)
            })
            .await
        };
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(e)) => {
                // SyncError 的 Display 已脱敏（无 URL userinfo / query / 凭证）
                publish_error(&inner, &device_id, last_sync_at, e.to_string());
                wait(&inner, retry_interval).await;
                continue;
            }
            Err(e) => {
                publish_error(
                    &inner,
                    &device_id,
                    last_sync_at,
                    format!("同步准备失败: {e}"),
                );
                wait(&inner, retry_interval).await;
                continue;
            }
        };
        if need_ensure {
            last_ensured = Some(ensure_fp);
        }

        // 5. 待写墓碑先落盘：run_cycle 读的是同一份 sync-state.json，
        //    不先写进去，本轮快照里就不会带这些删除
        let pending: Vec<String> = {
            let mut guard = inner.pending_tombstones.lock().await;
            let mut taken: Vec<String> = std::mem::take(&mut *guard).into_iter().collect();
            // 集合本身无序：排一下，快照记录的追加顺序才稳定（digest 不抖）
            taken.sort();
            taken
        };
        if !pending.is_empty() {
            let mut st = state::load_or_init(&inner.config_dir);
            let now = state::now_ms();
            for hash in pending.iter().filter(|hash| !hash.is_empty()) {
                st.add_tombstone(hash, now);
            }
            if let Err(e) = st.save(&inner.config_dir) {
                eprintln!("sync: 墓碑落盘失败 kind={}", e.kind.as_str());
            }
        }

        // 6. 缓存密钥：口令与 salt 没变时下一轮跳过 Argon2
        *inner.key_cache.lock().await = Some(KeyCache {
            fingerprint: prepared.fingerprint,
            key: prepared.key,
        });

        // 7. 一个完整周期（同步语义全在 run_cycle 里）
        let ctx = CycleCtx {
            db: inner.db.clone(),
            backend: prepared.backend,
            key: prepared.key,
            settings: {
                // 前端 `sync.device_name` 默认是空串：留空时回落 COMPUTERNAME，
                // 不要让它把对端看到的设备名覆盖成空
                let mut settings = cfg.clone();
                settings.device_name = device_name_or_computer_name(&settings.device_name);
                settings
            },
            state_path: inner.config_dir.clone(),
            images_dir: crate::services::storage_service::images_dir(&inner.app),
            emit: {
                let inner = Arc::clone(&inner);
                Box::new(move |status: SyncStatus| inner.publish(status))
            },
        };

        // 周期期间写库引发的事件不算"新的本地变更"（防回环）
        inner.applying.store(true, Ordering::SeqCst);
        let cycle = run_cycle(&ctx).await;
        if let Ok(report) = &cycle {
            // 合并进本地库 / 按墓碑删了本地条目 → 前端列表必须重查。
            // 在 applying 窗口内 emit：监听回调会忽略它（不会引发再同步），
            // 但前端 WebView 照常收到，列表因此立刻刷新
            if report.inserted > 0 || report.deleted > 0 {
                let _ = inner
                    .app
                    .emit(CLIPBOARD_CHANGED, serde_json::json!({"action": "synced"}));
            }
        }
        inner.applying.store(false, Ordering::SeqCst);

        match cycle {
            Ok(_report) => {
                // 周期结束已落盘：设备 ID 与同步时间以 state 文件为准
                let st = state::load_or_init(&inner.config_dir);
                device_id = st.device_id;
                last_sync_at = if st.last_sync_at > 0 {
                    st.last_sync_at
                } else {
                    state::now_ms()
                };
                inner.publish(SyncStatus {
                    state: STATE_IDLE.to_string(),
                    device_id: device_id.clone(),
                    last_sync_at,
                    last_error: None,
                    pending: 0,
                });
            }
            Err(e) => {
                publish_error(&inner, &device_id, last_sync_at, e.to_string());
                wait(&inner, retry_interval).await;
                continue;
            }
        }

        // 8. 等下一轮：定时器与唤醒信号二选一
        wait(&inner, interval).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    /// 测试专用固定密钥（不跑 Argon2，省时间）。
    const KEY: [u8; 32] = [9u8; 32];

    // -----------------------------------------------------------------------
    // Mock 后端
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct MockBackend {
        objects: Mutex<HashMap<String, (Vec<u8>, String)>>,
        puts: AtomicUsize,
        gets: AtomicUsize,
        list_calls: AtomicUsize,
        /// 剩余多少次 `list` 要返回 5xx（模拟网络抖动）
        list_failures: AtomicUsize,
        seq: AtomicUsize,
    }

    impl MockBackend {
        fn puts(&self) -> usize {
            self.puts.load(Ordering::SeqCst)
        }

        fn list_calls(&self) -> usize {
            self.list_calls.load(Ordering::SeqCst)
        }

        fn fail_next_lists(&self, n: usize) {
            self.list_failures.store(n, Ordering::SeqCst);
        }

        fn exists(&self, key: &str) -> bool {
            self.objects.lock().unwrap().contains_key(key)
        }

        fn snapshot(&self) -> Vec<(String, Vec<u8>)> {
            let mut out: Vec<(String, Vec<u8>)> = self
                .objects
                .lock()
                .unwrap()
                .iter()
                .map(|(k, (body, _))| (k.clone(), body.clone()))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }

        fn next_etag(&self) -> String {
            format!("\"e{}\"", self.seq.fetch_add(1, Ordering::SeqCst))
        }
    }

    impl SyncBackend for MockBackend {
        fn put(
            &self,
            key: &str,
            body: &[u8],
            if_match: Option<&str>,
        ) -> Result<Option<String>, SyncError> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            let mut objects = self.objects.lock().unwrap();
            if let Some(expected) = if_match {
                if expected == "*" {
                    if objects.contains_key(key) {
                        return Err(SyncError::conflict("PUT 已存在"));
                    }
                } else {
                    match objects.get(key) {
                        Some((_, current)) if current == expected => {}
                        _ => return Err(SyncError::conflict("PUT ETag 不匹配")),
                    }
                }
            }
            let etag = self.next_etag();
            objects.insert(key.to_string(), (body.to_vec(), etag.clone()));
            Ok(Some(etag))
        }

        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SyncError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .objects
                .lock()
                .unwrap()
                .get(key)
                .map(|(body, _)| body.clone()))
        }

        fn head(&self, key: &str) -> Result<Option<ObjectMeta>, SyncError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .get(key)
                .map(|(body, etag)| ObjectMeta {
                    key: key.to_string(),
                    etag: Some(etag.clone()),
                    size: body.len() as u64,
                }))
        }

        fn delete(&self, key: &str) -> Result<(), SyncError> {
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }

        fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, SyncError> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            if self.list_failures.load(Ordering::SeqCst) > 0 {
                self.list_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(SyncError::server("LIST 失败: status=503"));
            }
            let full = format!("{}/", prefix.trim_matches('/'));
            let mut out: Vec<ObjectMeta> = self
                .objects
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k.starts_with(&full))
                .map(|(k, (body, etag))| ObjectMeta {
                    key: k.clone(),
                    etag: Some(etag.clone()),
                    size: body.len() as u64,
                })
                .collect();
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        }
    }

    // -----------------------------------------------------------------------
    // 夹具
    // -----------------------------------------------------------------------

    /// 单连接内存库：内存库按连接隔离，必须锁死 1 个连接。
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

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sparrow-sync-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    async fn make_ctx(backend: Arc<MockBackend>, dir: &Path) -> CycleCtx {
        CycleCtx {
            db: memory_pool().await,
            backend,
            key: KEY,
            settings: SyncSettings::default(),
            state_path: dir.to_path_buf(),
            images_dir: dir.join("images"),
            emit: Box::new(|_status| {}),
        }
    }

    async fn insert_text(db: &SqlitePool, content: &str, hash: &str) {
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, preview)
             VALUES (0, ?, ?, NULL)",
        )
        .bind(content)
        .bind(hash)
        .execute(db)
        .await
        .expect("插入文本条目");
    }

    async fn insert_image_row(db: &SqlitePool, hash: &str, file_path: &str) {
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, file_path, preview)
             VALUES (2, '[图片]', ?, ?, '[图片]')",
        )
        .bind(hash)
        .bind(file_path)
        .execute(db)
        .await
        .expect("插入图片条目");
    }

    async fn count_hash(db: &SqlitePool, hash: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE content_hash = ?")
            .bind(hash)
            .fetch_one(db)
            .await
            .expect("计数")
    }

    // -----------------------------------------------------------------------
    // 用例
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn two_devices_round_trip() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        insert_text(&a.db, "跨设备同步的文本", "h1").await;

        let ra = run_cycle(&a).await.expect("A 周期");
        assert!(ra.uploaded, "首次应上传快照");
        assert_eq!(ra.peer_count, 0, "还没有 peer");
        assert_eq!(ra.inserted, 0);
        assert_eq!(ra.pending, 0);

        let rb = run_cycle(&b).await.expect("B 周期");
        assert_eq!(rb.peer_count, 1, "B 应读到 A 的快照");
        assert_eq!(rb.inserted, 1, "B 应插入一条");

        let content: Option<String> =
            sqlx::query_scalar("SELECT content FROM items WHERE content_hash = ?")
                .bind("h1")
                .fetch_optional(&b.db)
                .await
                .expect("查 B 的库");
        assert_eq!(content.as_deref(), Some("跨设备同步的文本"));

        // A 再跑一次：B 的快照里已有 h1，本地也有 → 不动作、不上传
        let ra2 = run_cycle(&a).await.expect("A 第二次周期");
        assert_eq!(ra2.inserted, 0, "重复内容不应再插入");
        assert!(!ra2.uploaded, "内容没变不应再上传");
    }

    #[tokio::test]
    async fn emit_reports_syncing_then_idle() {
        let backend = Arc::new(MockBackend::default());
        let dir = temp_dir();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let mut ctx = make_ctx(Arc::clone(&backend), &dir).await;
        ctx.emit = Box::new(move |status: SyncStatus| {
            log.lock().unwrap().push(status.state);
        });

        run_cycle(&ctx).await.expect("周期");
        assert_eq!(*seen.lock().unwrap(), vec!["syncing", "idle"]);
    }

    #[tokio::test]
    async fn unchanged_cycle_does_not_put_again() {
        let backend = Arc::new(MockBackend::default());
        let dir = temp_dir();
        let ctx = make_ctx(Arc::clone(&backend), &dir).await;
        insert_text(&ctx.db, "不变的内容", "h1").await;

        let first = run_cycle(&ctx).await.expect("首轮");
        assert!(first.uploaded);
        let puts_after_first = backend.puts();

        let second = run_cycle(&ctx).await.expect("次轮");
        assert!(!second.uploaded, "digest 没变就不该再上传");
        assert_eq!(
            backend.puts(),
            puts_after_first,
            "无变化不应产生新的 PUT（generated_at 不得参与 digest）"
        );
        // digest 命中 = 远端已有最新内容，不是"待同步"
        assert_eq!(second.pending, 0);
    }

    /// 初始状态：未启用 → disabled（命令层/状态栏据此不显示状态点）
    #[test]
    fn initial_status_is_disabled_until_enabled() {
        let st = SyncStateFile {
            device_id: "dev-1".to_string(),
            last_sync_at: 1234,
            ..Default::default()
        };

        let disabled = initial_status(&SyncSettings::default(), &st);
        assert_eq!(disabled.state, "disabled", "默认未启用");
        assert_eq!(disabled.device_id, "dev-1");
        assert_eq!(disabled.last_sync_at, 1234);
        assert_eq!(disabled.pending, 0);
        assert!(disabled.last_error.is_none());

        let enabled = SyncSettings {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(initial_status(&enabled, &st).state, "idle");
    }

    #[test]
    fn poll_interval_is_clamped_to_sane_range() {
        let cfg = SyncSettings {
            poll_interval_sec: 0,
            ..Default::default()
        };
        assert_eq!(poll_interval(&cfg), Duration::from_secs(2), "0 会空转");
        let cfg = SyncSettings {
            poll_interval_sec: 5,
            ..Default::default()
        };
        assert_eq!(poll_interval(&cfg), Duration::from_secs(5));
        let cfg = SyncSettings {
            poll_interval_sec: 99_999,
            ..Default::default()
        };
        assert_eq!(poll_interval(&cfg), Duration::from_secs(300));
    }

    #[tokio::test]
    async fn sensitive_content_never_reaches_backend() {
        let backend = Arc::new(MockBackend::default());
        let dir = temp_dir();
        let ctx = make_ctx(Arc::clone(&backend), &dir).await;

        // 拼接构造假密钥，避免源码里出现完整字面量
        let secret = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2");
        insert_text(&ctx.db, &format!("我的密钥是 {secret} 别外传"), "sec1").await;
        insert_text(&ctx.db, "普通内容", "ok1").await;

        run_cycle(&ctx).await.expect("周期");

        let mut saw_ok = false;
        for (key, body) in backend.snapshot() {
            assert!(
                !String::from_utf8_lossy(&body).contains(&secret),
                "密文对象里不得出现明文密钥"
            );
            if !key.starts_with("devices/") {
                continue;
            }
            let snap: Snapshot = crypto::open_json(&KEY, &key, &body).expect("解密快照");
            let json = serde_json::to_string(&snap).expect("序列化");
            assert!(!json.contains(&secret), "快照泄漏敏感内容");
            assert!(!json.contains("a1B2c3D4"), "快照泄漏敏感内容片段");
            if snap.records.iter().any(|r| r.hash == "ok1") {
                saw_ok = true;
            }
        }
        assert!(saw_ok, "普通条目应该正常上传");
    }

    #[tokio::test]
    async fn image_blob_round_trip() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        // 假 PNG：代码只按字节搬运，不校验图片格式。
        // hash 必须是 ≥32 位 hex（`insert_image` 的形态校验）。
        let hash = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
        let bytes: Vec<u8> = (0u8..20).collect();
        std::fs::create_dir_all(&a.images_dir).expect("建 images 目录");
        let path_a = a.images_dir.join(format!("{hash}.png"));
        std::fs::write(&path_a, &bytes).expect("写图片");
        insert_image_row(&a.db, hash, &path_a.to_string_lossy()).await;

        let ra = run_cycle(&a).await.expect("A 周期");
        assert!(ra.uploaded);
        assert!(
            backend.exists(&format!("blobs/{hash}.bin")),
            "图片 blob 应已加密上传"
        );

        let rb = run_cycle(&b).await.expect("B 周期");
        assert_eq!(rb.inserted, 1, "B 应插入图片条目");

        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT content, file_path FROM items WHERE content_hash = ? AND type = 2",
        )
        .bind(hash)
        .fetch_optional(&b.db)
        .await
        .expect("查 B 的图片行");
        let (content, file_path) = row.expect("B 应有图片记录");
        assert_eq!(content, "[图片]");
        let got = std::fs::read(&file_path).expect("B 的图片文件应已落盘");
        assert_eq!(got, bytes, "图片字节应逐字节一致");
    }

    /// 错口令（错 key）必须整轮显式失败，而且**不许写远端**：
    /// 否则会用错 key 覆盖自己的快照，改回正确口令后 digest 没变、再也不重传。
    #[tokio::test]
    async fn wrong_key_cycle_is_auth_and_writes_nothing() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        insert_text(&a.db, "A 的内容", "h1").await;
        run_cycle(&a).await.expect("A 周期");

        let before = backend.snapshot();
        let mut b = make_ctx(Arc::clone(&backend), &dir_b).await;
        b.key = [0xABu8; 32]; // 错口令派生出的 key

        let err = run_cycle(&b).await.expect_err("错 key 必须整轮失败");
        assert_eq!(err.kind, SyncErrorKind::Auth);
        assert_eq!(err.message, crypto::AUTH_MISMATCH);

        // B 不得写任何对象（含自己的快照），A 的密文也不能被覆盖
        let b_id = state::load_or_init(&b.state_path).device_id;
        assert!(
            !backend.exists(&format!("devices/{b_id}.json")),
            "错口令期间不得写入 B 的快照"
        );
        assert_eq!(
            backend.snapshot(),
            before,
            "错口令期间远端对象必须一个字节都没变"
        );
    }

    /// 换了口令/换了远端 meta → `meta.check` 变 → 上传缓存必须作废并重新上传，
    /// 否则远端永久留着旧 key 加密的密文。
    #[tokio::test]
    async fn meta_check_change_forces_reupload() {
        let backend = Arc::new(MockBackend::default());
        let dir = temp_dir();
        let ctx = make_ctx(Arc::clone(&backend), &dir).await;
        insert_text(&ctx.db, "内容不变", "h1").await;

        assert!(run_cycle(&ctx).await.expect("首轮").uploaded);
        assert!(!run_cycle(&ctx).await.expect("次轮").uploaded, "内容没变");

        let mut st = state::load_or_init(&ctx.state_path);
        assert_eq!(
            st.meta_check.as_deref(),
            Some(crypto::key_check(&KEY).as_str()),
            "每轮都要把当前密钥校验值写进 state"
        );
        st.meta_check = Some("0000".to_string());
        st.save(&ctx.state_path).expect("保存 state");

        assert!(
            run_cycle(&ctx).await.expect("第三轮").uploaded,
            "密钥校验值变了必须重新上传（覆盖旧密文）"
        );
    }

    #[tokio::test]
    async fn tombstone_deletes_on_peer() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        insert_text(&a.db, "待删除的内容", "h2").await;
        run_cycle(&a).await.expect("A 周期");
        run_cycle(&b).await.expect("B 周期");
        assert_eq!(count_hash(&b.db, "h2").await, 1, "B 应先同步到该条目");

        // A 显式删除：删行 + 写墓碑（模拟 commands/clipboard.rs 的删除钩子）
        sqlx::query("DELETE FROM items WHERE content_hash = ?")
            .bind("h2")
            .execute(&a.db)
            .await
            .expect("删 A 的行");
        let mut st = state::load_or_init(&a.state_path);
        st.add_tombstone("h2", state::now_ms());
        st.save(&a.state_path).expect("保存 A 的 state");

        run_cycle(&a).await.expect("A 周期（带墓碑）");
        let rb = run_cycle(&b).await.expect("B 周期");

        assert_eq!(rb.deleted, 1, "B 应按墓碑删除一条");
        assert_eq!(count_hash(&b.db, "h2").await, 0, "B 的条目应被删掉");
    }

    #[tokio::test]
    async fn list_retries_through_server_errors() {
        let backend = Arc::new(MockBackend::default());
        backend.fail_next_lists(2);
        let dir = temp_dir();
        let ctx = make_ctx(Arc::clone(&backend), &dir).await;
        insert_text(&ctx.db, "重试后仍要成功", "h1").await;

        let report = run_cycle(&ctx).await.expect("两次 5xx 后应成功");
        assert!(report.uploaded);
        assert_eq!(backend.list_calls(), 3, "应恰好尝试 3 次");
    }

    #[tokio::test]
    async fn auth_errors_are_not_retried() {
        // 用 401 语义的错误直接验证 retry 的短路：不重试、立刻返回
        let calls = AtomicUsize::new(0);
        let err = retry::<()>("LIST", "devices", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(SyncError::auth("LIST 失败: status=401"))
        })
        .expect_err("应返回错误");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "Auth 不该重试");
        assert_eq!(err.kind, SyncErrorKind::Auth);
    }

    // -----------------------------------------------------------------------
    // 本地变更唤醒：去抖代次（纯逻辑，不依赖 AppHandle）
    // -----------------------------------------------------------------------

    /// 连续复制时只有**最后一次**事件能唤醒同步：前面几次的代次已被覆盖
    #[test]
    fn debounce_generation_only_wakes_for_last_event() {
        let generation = AtomicU64::new(0);
        let first = next_generation(&generation);
        let second = next_generation(&generation);
        let third = next_generation(&generation);
        assert_eq!((first, second, third), (1, 2, 3), "代次必须单调自增");
        assert!(!is_latest_generation(&generation, first), "过期事件不该唤醒");
        assert!(!is_latest_generation(&generation, second), "过期事件不该唤醒");
        assert!(is_latest_generation(&generation, third), "最后一次事件必须唤醒");

        // 第三次的 400ms 窗口里又来一次：第三次即使已经 sleep 完也不该唤醒
        let fourth = next_generation(&generation);
        assert!(!is_latest_generation(&generation, third));
        assert!(is_latest_generation(&generation, fourth));
    }

    /// `clipboard-changed` 与 `item-deleted` 共用同一个代次计数器：
    /// 复制紧接删除时只唤醒一次，不会各唤醒一次
    #[test]
    fn debounce_generation_is_shared_by_both_events() {
        let generation = AtomicU64::new(0);
        let copy = next_generation(&generation);
        let delete = next_generation(&generation);
        assert!(delete > copy, "同一计数器上代次必须递增");
        assert!(!is_latest_generation(&generation, copy), "先到的事件被后来者覆盖");
        assert!(is_latest_generation(&generation, delete));
    }

    // -----------------------------------------------------------------------
    // 设备名兜底
    // -----------------------------------------------------------------------

    /// 前端 `sync.device_name` 默认空串：留空时必须回落 COMPUTERNAME，
    /// 不能把对端看到的设备名覆盖成空
    #[test]
    fn device_name_falls_back_when_blank() {
        assert_eq!(resolve_device_name("我的电脑", Some("PC-A")), "我的电脑");
        assert_eq!(
            resolve_device_name("  我的电脑  ", Some("PC-A")),
            "我的电脑",
            "两端空白应去掉"
        );
        assert_eq!(resolve_device_name("", Some("PC-A")), "PC-A");
        assert_eq!(resolve_device_name("   ", Some("PC-A")), "PC-A", "纯空白等于没填");
        assert_eq!(resolve_device_name("", None), "device");
        assert_eq!(
            resolve_device_name("", Some("   ")),
            "device",
            "空的 COMPUTERNAME 也不能变成空设备名"
        );
    }

    #[test]
    fn device_id_from_key_parses_only_device_objects() {
        assert_eq!(
            device_id_from_key("devices/abc-123.json"),
            Some("abc-123")
        );
        assert_eq!(device_id_from_key("devices/.json"), None);
        assert_eq!(device_id_from_key("blobs/abc.bin"), None);
        assert_eq!(device_id_from_key("devices/sub/abc.json"), None);
    }
}
