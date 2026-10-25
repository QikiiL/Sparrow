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

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use tauri::{Emitter, Listener, Manager};

use crate::models::sync::{
    max_blob_bytes, Snapshot, SyncMode, SyncProvider, SyncRecord, SyncSecrets, SyncSettings,
    SyncStatus,
};
use crate::services::credentials;
use crate::services::sync::backend::{build_backend, ObjectMeta, SyncBackend};
use crate::services::sync::crypto::KeyCache;
use crate::services::sync::merge::{self, MergeAction};
use crate::services::sync::state::{self, PeerState, SyncStateFile, Tombstone};
use crate::services::sync::{crypto, key_prefix, key_tag, snapshot, SyncError, SyncErrorKind};

/// 远端目录（相对 root，后端的 key 不含 root）。
const DEVICES_DIR: &str = "devices";
const BLOBS_DIR: &str = "blobs";
/// 图片条目的 content/preview 占位，与 `snapshot.rs` / 前端展示一致。
const IMAGE_PREVIEW: &str = "[图片]";
/// 一次操作最多尝试 5 次（首次 + 4 次重试）。
///
/// 为什么是 5：校园网/运营商 DNS 污染是**间歇**的——同一次同步里第一次解析
/// 失败、几秒后重试就成功的情况很常见，多试几次能明显提高成功率。
const MAX_ATTEMPTS: u32 = 5;

/// 重试退避：500ms → 2s → 5s → 10s（5 次尝试共 4 次等待，累计约 17.5s）。
#[cfg(not(test))]
const RETRY_BACKOFF_MS: [u64; 4] = [500, 2000, 5000, 10000];
/// 测试里把退避压到毫秒级，避免整个测试套件为退避多等十几秒；
/// 重试判定逻辑与生产路径完全一致，只是等待时间不同。
#[cfg(test)]
const RETRY_BACKOFF_MS: [u64; 4] = [1, 2, 3, 4];

/// peer 快照 GET 的尝试次数：比常规操作少。
///
/// 远端有一个读不通的对象时，本轮不该为它等满 4 次退避（约 17s）——
/// 失败后有 [`PEER_RETRY_COOLDOWN_MS`] 冷却，下一轮再试更划算。
const PEER_GET_ATTEMPTS: u32 = 2;
/// peer 快照拉取失败后的冷却期：60s 内不再尝试同一个 peer。
const PEER_RETRY_COOLDOWN_MS: i64 = 60 * 1000;

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
    /// formats 目录：富文本副本（CF_HTML / RTF 原始字节）的落地位置，
    /// 与 `storage_service::formats_dir` 同源（自定义存储位置时跟着走）。
    pub formats_dir: PathBuf,
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
    /// 本轮没能同步的图片数（发送端文件缺失/0 字节/超限 + 接收端 blob 缺失等）。
    /// 只计"图片丢了"，用户主动关掉图片同步不计。
    pub skipped_images: u32,
    /// 本轮没能同步的富文本格式副本数（发送端文件缺失/超限 + 接收端 blob 缺失/
    /// 解密失败）。HTML 与 RTF 独立计数；纯文本本身照常同步，只是丢了格式。
    pub skipped_formats: u32,
    /// 本轮没能拉到的 peer 快照数（网络/5xx/404，或仍在冷却期内）。
    ///
    /// 单个 peer 读不通**不**中断整轮：远端多出一个读不通的对象（常见于第三台
    /// 旧设备留下的快照）不该让本机的文本/图片同步一起停摆。
    pub skipped_peers: u32,
}

/// 接收端一条图片的落地结果。
///
/// 跳过必须带原因：原来静默 `Ok(false)` 的结果就是"文本同步正常、图片不同步"
/// 而日志里一个字都没有，用户与开发者都无从下手。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOutcome {
    Inserted,
    Skipped(&'static str),
}

/// 一条 `MergeAction::Insert` 的执行结果（内部用，与 [`ImageOutcome`] 一一对应）。
enum InsertOutcome {
    /// 真的新增了一行；`skipped_formats` 是这条记录里没能落地的富文本副本数
    /// （文本已插入，只是那几种格式置 NULL）。
    Inserted { skipped_formats: u32 },
    /// 已存在（只刷新 last_used_at）或没有可插入的内容
    Ignored,
    /// 图片没落地，带跳过原因
    ImageSkipped(&'static str),
}

/// 跳过时给用户看的告警文案（0 个 → 无告警）。
///
/// 图片、富文本、peer 快照分开说：用户需要知道丢的是"图片"还是"富文本格式"，
/// 两者处理方式不同（图片是整条没同步，富文本只是退化成纯文本）；而
/// "设备快照未拉取"意味着**某个设备**的条目这轮没进来，跟本机内容无关。
fn sync_warning(skipped_images: u32, skipped_formats: u32, skipped_peers: u32) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    match (skipped_images, skipped_formats) {
        (0, 0) => {}
        (images, 0) => parts.push(format!("{images} 张图片未同步")),
        (0, formats) => parts.push(format!("{formats} 个富文本未同步")),
        (images, formats) => {
            parts.push(format!("{images} 张图片、{formats} 个富文本未同步"));
        }
    }
    if skipped_peers > 0 {
        parts.push(format!("{skipped_peers} 个设备快照未拉取"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("；"))
    }
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
        warning: None,
        skipped: 0,
    });

    let outcome = cycle_body(ctx, &mut st).await;

    match &outcome {
        Ok(report) => (ctx.emit)(SyncStatus {
            state: "idle".to_string(),
            device_id: st.device_id.clone(),
            last_sync_at: st.last_sync_at,
            last_error: None,
            pending: report.pending,
            warning: sync_warning(
                report.skipped_images,
                report.skipped_formats,
                report.skipped_peers,
            ),
            skipped: report.skipped_images + report.skipped_formats,
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
    //    单个 peer 读不通只跳过（并记冷却期），不中断整轮
    let (snapshots, skipped_peers) = fetch_peer_snapshots(ctx, st).await?;

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
    let mut skipped_images: u32 = 0;
    let mut skipped_formats: u32 = 0;
    for action in merge::plan_merge(&local, &snapshots, &st.tombstones, &st.device_id) {
        match action {
            MergeAction::Insert(rec) => match insert_record(ctx, &rec).await? {
                InsertOutcome::Inserted { skipped_formats: n } => {
                    inserted += 1;
                    skipped_formats += n;
                }
                InsertOutcome::Ignored => {}
                InsertOutcome::ImageSkipped(reason) => {
                    skipped_images += 1;
                    eprintln!("sync: skip image {} ({reason})", short_id(&rec.hash));
                }
            },
            MergeAction::Delete(hash) => {
                if delete_local(&ctx.db, &ctx.images_dir, &ctx.formats_dir, &hash).await? {
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
    let build = snapshot::build_snapshot(
        &ctx.db,
        &ctx.settings,
        &st.tombstones,
        &st.device_id,
        &name,
        &ctx.images_dir,
    )
    .await?;
    // 发送端被跳过的图片/富文本同样计入本轮告警。明细由 snapshot 收集在
    // `warnings` 里，这里统一打日志（只含 hash 前 8 位与原因，不含路径），
    // 否则"留在本机没上去"这件事对用户和开发者都不可见
    for warning in &build.warnings {
        eprintln!("sync: snapshot skip {warning}");
    }
    skipped_images += build.skipped_images;
    skipped_formats += build.skipped_formats;
    let mut snap = build.snapshot;
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
        skipped_images,
        skipped_formats,
        skipped_peers,
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
/// DNS 污染（`kind=Dns`）映射成 `Network`，所以域名解析的间歇失败同样走这条
/// 退避重试：1 次失败不代表配置有问题。
///
/// 日志只在最终失败时打一行（操作名 + key_tag + key 形状 + kind），
/// 不打印 URL/响应体。
fn retry<T>(
    op: &str,
    key: &str,
    mut f: impl FnMut() -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    retry_with_attempts(op, key, MAX_ATTEMPTS, &mut f)
}

/// 同 [`retry`]，但允许调用方指定尝试次数。
///
/// peer 快照用更少的次数：远端有一个读不通的对象时，本轮不该为它等满
/// 4 次退避（约 17s）——失败后还有 [`PEER_RETRY_COOLDOWN_MS`] 的冷却，
/// 下一轮再试更划算。
fn retry_with_attempts<T>(
    op: &str,
    key: &str,
    max_attempts: u32,
    mut f: impl FnMut() -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    let max_attempts = max_attempts.max(1);
    let mut attempt: u32 = 1;
    loop {
        match f() {
            Ok(value) => return Ok(value),
            Err(e) => {
                if !e.kind.retryable() || attempt >= max_attempts {
                    eprintln!(
                        "sync: {op} 失败: key={} shape={} kind={} attempts={attempt}",
                        key_tag(key),
                        key_prefix(key),
                        e.kind.as_str()
                    );
                    return Err(e);
                }
                // 退避表按生产档位设计；尝试次数被调小时只用到前面几档
                let idx = ((attempt - 1) as usize).min(RETRY_BACKOFF_MS.len() - 1);
                std::thread::sleep(std::time::Duration::from_millis(RETRY_BACKOFF_MS[idx]));
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
    remote_get_with_attempts(ctx, key, MAX_ATTEMPTS).await
}

/// 同 [`remote_get`]，可指定尝试次数（peer 快照用 [`PEER_GET_ATTEMPTS`]）。
async fn remote_get_with_attempts(
    ctx: &CycleCtx,
    key: &str,
    attempts: u32,
) -> Result<Option<Vec<u8>>, SyncError> {
    let backend = Arc::clone(&ctx.backend);
    let key = key.to_string();
    io(move || retry_with_attempts("GET", &key, attempts, || backend.get(&key))).await
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

/// 拉取所有 peer 快照。
///
/// 返回 `(快照列表, 跳过的 peer 数)`。**单个 peer 读不通不中断整轮**：
/// 远端多出一个读不通的对象（典型来源是第三台旧设备留下的快照、或写了一半的
/// 对象）不该让本机的文本/图片同步一起停摆。失败后给该 peer 记一个冷却期
/// （[`PEER_RETRY_COOLDOWN_MS`]），避免每轮都为它等满重试。
///
/// 例外是**认证失败**（GET 401/403，或解不开 peer 密文）：口令/权限问题必须
/// 整轮显式失败，跳过 + 报"同步正常"只会掩盖问题。
async fn fetch_peer_snapshots(
    ctx: &CycleCtx,
    st: &mut SyncStateFile,
) -> Result<(Vec<Snapshot>, u32), SyncError> {
    let metas = remote_list(ctx, DEVICES_DIR).await?;
    let mut out: Vec<Snapshot> = Vec::with_capacity(metas.len());
    let mut skipped_peers: u32 = 0;
    let now = state::now_ms();

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
        // 冷却期内不再尝试：上一个周期已经确认这个对象读不通
        if let Some(until) = st.peers.get(device).and_then(|p| p.retry_after_ms) {
            if until > now {
                skipped_peers += 1;
                continue;
            }
        }

        let fetched = match remote_get_with_attempts(ctx, &meta.key, PEER_GET_ATTEMPTS).await {
            Ok(bytes) => bytes,
            // 认证失败不是"这个 peer 坏了"：口令/权限问题必须整轮显式失败，
            // 否则错口令期间每轮都只跳过一个 peer、还照样报"同步正常"。
            Err(e) if e.kind == SyncErrorKind::Auth => {
                eprintln!(
                    "sync: peer GET 认证失败 kind={} shape={}（整轮中止）",
                    e.kind.as_str(),
                    key_prefix(&meta.key)
                );
                return Err(e);
            }
            // 其余（网络/5xx/404/配置）：跳过这个 peer、记冷却期，本轮继续。
            // 远端一个读不通的对象不该让本机的文本/图片同步一起停摆。
            Err(e) => {
                eprintln!(
                    "sync: skip peer {} kind={} shape={}（{}s 内不再重试）",
                    short_id(device),
                    e.kind.as_str(),
                    key_prefix(&meta.key),
                    PEER_RETRY_COOLDOWN_MS / 1000
                );
                let entry = st.peers.entry(device.to_string()).or_default();
                entry.retry_after_ms = Some(now + PEER_RETRY_COOLDOWN_MS);
                skipped_peers += 1;
                continue;
            }
        };
        let Some(bytes) = fetched else {
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
                        // 成功即清空冷却
                        retry_after_ms: None,
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
            Err(e) => {
                eprintln!(
                    "sync: skip peer {} kind={}",
                    short_id(device),
                    e.kind.as_str()
                );
                skipped_peers += 1;
            }
        }
    }

    Ok((out, skipped_peers))
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

/// 执行一条 `Insert`。`Inserted` 表示真的新增了一行（"已存在只更新 last_used_at"
/// 返回 `Ignored`）；图片没落地时返回带原因的 `ImageSkipped`，由调用方计数上报。
async fn insert_record(ctx: &CycleCtx, rec: &SyncRecord) -> Result<InsertOutcome, SyncError> {
    // 防御：墓碑记录不是可插入的条目（plan_merge 正常不会给出，但别让
    // 一条 deleted_at 记录变成 content 为空的幽灵行）
    if rec.hash.is_empty() || rec.deleted_at.is_some() {
        return Ok(InsertOutcome::Ignored);
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
        return Ok(InsertOutcome::Ignored);
    }

    if rec.kind == 2 {
        return Ok(match insert_image(ctx, rec).await? {
            ImageOutcome::Inserted => InsertOutcome::Inserted { skipped_formats: 0 },
            ImageOutcome::Skipped(reason) => InsertOutcome::ImageSkipped(reason),
        });
    }

    let Some(content) = rec.content.as_deref().filter(|c| !c.is_empty()) else {
        return Ok(InsertOutcome::Ignored);
    };
    let preview = rec
        .preview
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| content.chars().take(100).collect());

    // 富文本副本：先把远端 blob 落到本机 formats 目录，成功才写路径列。
    // 任一种格式拿不到都只把该列置 NULL 并计数，纯文本照常插入
    // （宁可少一个格式，也不要因为格式丢了整条内容）。
    let mut skipped_formats: u32 = 0;
    let html_path = match land_format(
        ctx,
        &rec.hash,
        rec.html_blob.as_deref(),
        rec.html_size,
        FORMAT_HTML,
    )
    .await?
    {
        Landed::Path(path) => Some(path),
        Landed::Absent => None,
        Landed::Skipped(reason) => {
            skipped_formats += 1;
            eprintln!("sync: skip html {} ({reason})", short_id(&rec.hash));
            None
        }
    };
    let rtf_path = match land_format(
        ctx,
        &rec.hash,
        rec.rtf_blob.as_deref(),
        rec.rtf_size,
        FORMAT_RTF,
    )
    .await?
    {
        Landed::Path(path) => Some(path),
        Landed::Absent => None,
        Landed::Skipped(reason) => {
            skipped_formats += 1;
            eprintln!("sync: skip rtf {} ({reason})", short_id(&rec.hash));
            None
        }
    };

    sqlx::query(
        "INSERT INTO items (type, content, content_hash, preview, html_path, rtf_path,
                            created_at, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?, datetime(? / 1000, 'unixepoch'), datetime(? / 1000, 'unixepoch'))",
    )
    .bind(rec.kind)
    .bind(content)
    .bind(&rec.hash)
    .bind(preview)
    .bind(&html_path)
    .bind(&rtf_path)
    .bind(rec.created_at)
    .bind(rec.updated_at)
    .execute(&ctx.db)
    .await
    .map_err(db_err)?;

    Ok(InsertOutcome::Inserted { skipped_formats })
}

/// 图片条目：先把 blob 落到 `images_dir/{hash 前 16 位}.png`（已有非空文件则复用），
/// 再插行。任何一步失败都返回带原因的 [`ImageOutcome::Skipped`]，绝不静默返回。
async fn insert_image(ctx: &CycleCtx, rec: &SyncRecord) -> Result<ImageOutcome, SyncError> {
    // 本机不同步图片时也不要把 peer 的图片拉下来（用户关掉图片同步是明确的意图）
    if !ctx.settings.sync_images {
        return Ok(ImageOutcome::Skipped("sync_images 关闭"));
    }

    // 远端 hash 会拼进 blob key 与本地文件名：只接受 ≥32 位纯 ASCII hex，
    // 否则 `../` 之类的内容能把路径拼出 images 目录。
    if !is_blob_hash(&rec.hash) {
        return Ok(ImageOutcome::Skipped("hash 非法"));
    }
    let max_bytes = max_blob_bytes(&ctx.settings);
    if let Some(size) = rec.blob_size {
        if size > max_bytes {
            return Ok(ImageOutcome::Skipped("超过大小上限"));
        }
    }

    let path = ctx.images_dir.join(format!("{}.png", short16(&rec.hash)));
    // 目标文件存在但为 0 字节：上次落盘被打断（掉电/崩溃/磁盘满）留下的残骸。
    // 早退"已存在就复用"会让这张图永远坏在本地，所以必须删掉重下。
    let usable = std::fs::metadata(&path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false);
    if !usable {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let key = blob_key(&rec.hash);
        // 单张图片的 blob 下载失败不能拖垮整轮：跳过它并计入告警，文本/其它图片照常同步。
        let sealed = match remote_get(ctx, &key).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(ImageOutcome::Skipped("blob 缺失")),
            Err(e) if e.kind == SyncErrorKind::Auth => return Err(e),
            Err(e) => {
                eprintln!(
                    "sync: skip image blob {} kind={}",
                    short_id(&rec.hash),
                    e.kind.as_str()
                );
                return Ok(ImageOutcome::Skipped("下载失败"));
            }
        };
        let plain = match crypto::open_blob(&ctx.key, &key, &sealed) {
            Ok(plain) => plain,
            Err(_) => return Ok(ImageOutcome::Skipped("解密失败")),
        };
        // 明文长度必须与快照声明一致：不一致说明 blob 被截断/写坏，
        // 落盘只会得到一张坏图（临时文件一并清掉，别留残骸）
        if let Some(expected) = rec.blob_size {
            if plain.len() as u64 != expected {
                let tmp = path.with_extension("png.tmp");
                let _ = std::fs::remove_file(&tmp);
                let _ = std::fs::remove_file(&path);
                return Ok(ImageOutcome::Skipped("大小不符"));
            }
        }
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

    Ok(ImageOutcome::Inserted)
}

// ---------------------------------------------------------------------------
// 富文本副本（HTML / RTF 原始字节）落地
// ---------------------------------------------------------------------------

/// formats 目录里的两种扩展名（与 `clipboard_monitor::save_format_to_disk` 一致）。
const FORMAT_HTML: &str = "html";
const FORMAT_RTF: &str = "rtf";

/// 一种富文本副本的落地结果。
enum Landed {
    /// 该条目没有这种格式（快照里 blob 字段为 None）
    Absent,
    /// 已落盘（或复用本机已有的同名文件），返回写进 `html_path`/`rtf_path` 的路径
    Path(String),
    /// 有 blob 但拿不到（缺失 / 解密失败 / 超限 / 校验失败），带原因
    Skipped(&'static str),
}

/// 把一条记录的某一种富文本副本从远端落到本机 formats 目录。
///
/// 本地文件名用 **`record.hash` 前 16 位 + 扩展名**（与采集端
/// `clipboard_monitor::save_format_to_disk` 的命名一致），**不用 blob hash**：
/// 两个条目可能共享同一份 html 字节，按 blob hash 命名会让它们指向同一个文件，
/// 删掉其中一个就把另一个的格式文件一起带走了。
async fn land_format(
    ctx: &CycleCtx,
    record_hash: &str,
    blob_hash: Option<&str>,
    size: Option<u64>,
    ext: &str,
) -> Result<Landed, SyncError> {
    let Some(blob_hash) = blob_hash.filter(|h| !h.is_empty()) else {
        return Ok(Landed::Absent);
    };
    // blob hash 会拼进对象 key（`blobs/{hash}.bin`），peer 快照里的字段不可全信
    if !is_blob_hash(blob_hash) {
        return Ok(Landed::Skipped("blob hash 非法"));
    }
    // record.hash 会拼进**本地文件名**：同样只接受 ≥32 位 hex，
    // 否则 peer 用 `../../x` 之类的 hash 就能把文件写到 formats 目录外
    if !is_blob_hash(record_hash) {
        return Ok(Landed::Skipped("record hash 非法"));
    }
    let max_bytes = max_blob_bytes(&ctx.settings);
    if let Some(size) = size {
        if size > max_bytes {
            return Ok(Landed::Skipped("超过大小上限"));
        }
    }

    let path = ctx
        .formats_dir
        .join(format!("{}.{ext}", short16(record_hash)));
    // 本机采集时就已经写过一份同名文件（同一 content_hash → 同一份 html 字节），
    // 大小一致即可复用，省一次下载。0 字节/大小不符的残骸必须删掉重下。
    let existing = std::fs::metadata(&path)
        .ok()
        .filter(|meta| meta.is_file() && meta.len() > 0)
        .map(|meta| meta.len());
    if let (Some(len), Some(expected)) = (existing, size) {
        if len == expected {
            return Ok(Landed::Path(path.to_string_lossy().to_string()));
        }
    }
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    let key = blob_key(blob_hash);
    // 单个格式 blob 下载失败只降级该格式（纯文本仍入库），不能中断整轮。
    let sealed = match remote_get(ctx, &key).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(Landed::Skipped("blob 缺失")),
        Err(e) if e.kind == SyncErrorKind::Auth => return Err(e),
        Err(e) => {
            eprintln!(
                "sync: skip format blob {} kind={}",
                short_id(blob_hash),
                e.kind.as_str()
            );
            return Ok(Landed::Skipped("下载失败"));
        }
    };
    let plain = match crypto::open_blob(&ctx.key, &key, &sealed) {
        Ok(plain) => plain,
        Err(_) => return Ok(Landed::Skipped("解密失败")),
    };
    // 内容寻址自校验：明文必须真的哈希回 blob_hash，否则等于往本机写未知内容
    if sha256_hex(&plain) != blob_hash {
        return Ok(Landed::Skipped("内容校验失败"));
    }
    if let Some(expected) = size {
        if plain.len() as u64 != expected {
            return Ok(Landed::Skipped("大小不符"));
        }
    }
    write_atomic(&ctx.formats_dir, &path, &plain)?;

    Ok(Landed::Path(path.to_string_lossy().to_string()))
}

/// 执行一条 `Delete`：先取 `file_path`/`html_path`/`rtf_path` 再删行，
/// 最后按各自目录守卫删文件。
async fn delete_local(
    db: &SqlitePool,
    images_dir: &Path,
    formats_dir: &Path,
    hash: &str,
) -> Result<bool, SyncError> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT file_path, html_path, rtf_path FROM items WHERE content_hash = ? LIMIT 1",
    )
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

    if let Some((file_path, html_path, rtf_path)) = row {
        if let Some(path) = file_path {
            remove_local_image(images_dir, &path);
        }
        // 富文本副本必须跟着条目一起删：留下孤儿文件会被 image_cleanup 之外
        // 的清理路径漏掉，白占磁盘
        for path in [html_path, rtf_path].into_iter().flatten() {
            remove_local_format(formats_dir, &path);
        }
    }
    Ok(true)
}

/// 删除 images 目录内的图片文件。
///
/// `image_cleanup::remove_image_file` 需要 `AppHandle`（它走
/// `storage_service::resolve_image_path`），本模块刻意不依赖 Tauri，
/// 所以在这里复刻同一条守卫：规范化后必须落在 images 目录内才删。
fn remove_local_image(images_dir: &Path, file_path: &str) {
    remove_within(images_dir, file_path);
}

/// 删除 formats 目录内的富文本副本文件（守卫与 [`remove_local_image`] 相同：
/// `html_path`/`rtf_path` 来自数据库，可能被改成任意路径）。
fn remove_local_format(formats_dir: &Path, file_path: &str) {
    remove_within(formats_dir, file_path);
}

/// canonicalize 后必须落在 `dir` 内才删（两侧都规范化，避免 `..` 绕过）。
fn remove_within(dir: &Path, file_path: &str) {
    let Ok(dir) = std::fs::canonicalize(dir) else {
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
            html_blob: None,
            html_size: None,
            rtf_blob: None,
            rtf_size: None,
        });
    }
}

// ---------------------------------------------------------------------------
// 3. 上传
// ---------------------------------------------------------------------------

/// 上传快照里引用的、本机还没传过的 blob：图片 + 富文本副本（HTML/RTF）。
///
/// 两者共用同一份 `uploaded_blobs` 去重集合 —— 它们都是内容寻址对象，
/// 键就是内容 hash，落在同一个 `blobs/` 前缀下。
async fn upload_blobs(
    ctx: &CycleCtx,
    st: &mut SyncStateFile,
    snap: &Snapshot,
) -> Result<(), SyncError> {
    let max_bytes = max_blob_bytes(&ctx.settings);
    let mut done: HashSet<String> = st.uploaded_blobs.iter().cloned().collect();

    // 1. 图片：blob key 就是 record.hash（图片的 content_hash 即文件字节的 hash）
    let images = image_rows(&ctx.db).await?;
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
                "sync: skip blob {} (文件缺失或超过大小上限)",
                short_id(&rec.hash)
            );
            continue;
        };

        let key = blob_key(&rec.hash);
        let sealed = crypto::seal_blob(&ctx.key, &key, &bytes)?;
        // 无条件 PUT：换口令后必须用新 key 覆盖旧密文，条件写会让 412 把
        // "内容寻址、已经存在"误当成"无需上传"，远端就永远留着解不开的旧密文。
        remote_put(ctx, &key, sealed, None).await?;
        st.uploaded_blobs.push(rec.hash.clone());
        done.insert(rec.hash.clone());
    }

    // 2. 富文本副本：blob key 是**内容** hash（html_blob/rtf_blob），
    //    与 record.hash 无关 —— 同一份 html 被两个条目共享时只上传一次。
    let formats = format_rows(&ctx.db).await?;
    for rec in snap
        .records
        .iter()
        .filter(|r| r.kind != 2 && r.deleted_at.is_none())
    {
        let Some((_, html_path, rtf_path)) = formats.iter().find(|(hash, _, _)| hash == &rec.hash)
        else {
            continue;
        };
        for (blob_hash, path) in [
            (rec.html_blob.as_deref(), html_path.as_deref()),
            (rec.rtf_blob.as_deref(), rtf_path.as_deref()),
        ] {
            let Some(blob_hash) = blob_hash else {
                continue;
            };
            upload_one_blob(ctx, st, &mut done, blob_hash, path, max_bytes).await?;
        }
    }

    Ok(())
}

/// 上传一个富文本副本 blob。
///
/// 读取后必须自校验 `sha256(bytes) == blob_hash`：快照构建完成到此刻之间文件
/// 可能被改写（用户编辑条目会换 content_hash，但删除/清理可能留下半截文件）。
/// 不一致就跳过 —— 下一轮快照重算会得出新的 blob hash，自然重试。
async fn upload_one_blob(
    ctx: &CycleCtx,
    st: &mut SyncStateFile,
    done: &mut HashSet<String>,
    blob_hash: &str,
    path: Option<&str>,
    max_bytes: u64,
) -> Result<(), SyncError> {
    if done.contains(blob_hash) {
        return Ok(());
    }
    if !is_blob_hash(blob_hash) {
        eprintln!("sync: skip format blob {} (hash 非法)", short_id(blob_hash));
        return Ok(());
    }
    let Some(path) = path else {
        eprintln!(
            "sync: skip format blob {} (无本地文件)",
            short_id(blob_hash)
        );
        return Ok(());
    };
    let Some(bytes) = read_blob_file(path, max_bytes).await? else {
        eprintln!(
            "sync: skip format blob {} (文件缺失或超过大小上限)",
            short_id(blob_hash)
        );
        return Ok(());
    };
    if sha256_hex(&bytes) != blob_hash {
        eprintln!(
            "sync: skip format blob {} (内容与 hash 不符)",
            short_id(blob_hash)
        );
        return Ok(());
    }

    let key = blob_key(blob_hash);
    let sealed = crypto::seal_blob(&ctx.key, &key, &bytes)?;
    // 无条件 PUT，理由同图片 blob
    remote_put(ctx, &key, sealed, None).await?;
    st.uploaded_blobs.push(blob_hash.to_string());
    done.insert(blob_hash.to_string());
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

/// `SELECT content_hash, html_path, rtf_path FROM items WHERE type IN (0,1)`。
async fn format_rows(
    db: &SqlitePool,
) -> Result<Vec<(String, Option<String>, Option<String>)>, SyncError> {
    let rows =
        sqlx::query("SELECT content_hash, html_path, rtf_path FROM items WHERE type IN (0,1)")
            .fetch_all(db)
            .await
            .map_err(db_err)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let hash: String = row.try_get("content_hash").map_err(sqlx_err)?;
        let html: Option<String> = row.try_get("html_path").map_err(sqlx_err)?;
        let rtf: Option<String> = row.try_get("rtf_path").map_err(sqlx_err)?;
        out.push((hash, html, rtf));
    }
    Ok(out)
}

/// 读一个 blob 源文件（在阻塞池里跑）；缺失/非文件/超限返回 None。
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
            SyncError::config(format!("读取文件失败: {:?}", e.kind()))
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
            retry_after_ms: None,
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

/// 先写 `.tmp` 再 rename，避免掉电/崩溃留下半个文件。
fn write_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| SyncError::config(format!("创建数据目录失败: {:?}", e.kind())))?;
    let tmp = dest.with_file_name(format!(
        "{}.tmp",
        dest.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "blob".to_string())
    ));
    std::fs::write(&tmp, bytes)
        .map_err(|e| SyncError::config(format!("写入临时文件失败: {:?}", e.kind())))?;
    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        SyncError::config(format!("落盘失败: {:?}", e.kind()))
    })
}

/// sha256 hex（小写），用于 blob 内容自校验。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
///
/// 这里刻意**不**用 `wait_next`：未启用时循环不联网，2s 只是重新读一次内存配置，
/// 保证「启用」即使漏掉一次唤醒也会在 2s 内被观察到；与「仅启动时」的静默语义
/// 不冲突（那时本来就没有任何同步动作）。
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
        && a.warning == b.warning
        && a.skipped == b.skipped
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
    ///
    /// 入队的同时把 hash **追加**到 `sync-tombstones.pending`（与 `sync-state.json`
    /// 解耦的日志文件）：`sync_mode = startup` 不唤醒循环，用户删完直接退出进程时
    /// 内存队列会丢，日志文件不会——下次启动第一轮读回来合并，已删条目就不会复活。
    ///
    /// `sync_mode = interval`：入队后立即唤醒一轮，墓碑尽快落盘。
    /// `sync_mode = startup`：只入队、**不唤醒**，等下一次手动同步或下次启动的那一轮落盘。
    pub fn queue_tombstones(&self, hashes: Vec<String>) {
        let hashes: Vec<String> = hashes.into_iter().filter(|h| !h.is_empty()).collect();
        if hashes.is_empty() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        tauri::async_runtime::spawn(async move {
            // 先落盘再入内存队列：即使本进程下一秒被关掉，墓碑也已经写在磁盘上
            let dir = inner.config_dir.clone();
            let to_disk = hashes.clone();
            let appended = tauri::async_runtime::spawn_blocking(move || {
                state::append_pending_tombstones(&dir, &to_disk, state::now_ms())
            })
            .await;
            match appended {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("sync: 墓碑日志写入失败 kind={}", e.kind.as_str()),
                Err(_) => eprintln!("sync: 墓碑日志写入失败 kind=join"),
            }

            let mut pending = inner.pending_tombstones.lock().await;
            for hash in hashes {
                pending.insert(hash);
            }
            drop(pending);
            // 仅启动时：删除**不**触发同步。墓碑留在队列里，等下次启动或手动
            // 「立即同步」时随那一轮落盘（`run_loop` 每轮先取队列）。
            let mode = inner.settings.lock().await.sync_mode;
            if should_wake(mode) {
                inner.notify.notify_one();
            }
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
///
/// `sync_mode = startup` 时回调直接返回：复制/删除不触发同步，只有手动
/// 「立即同步」、保存设置、切换模式才会唤醒循环。
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
            // 仅启动时：本机复制/删除一律不唤醒（严格静默）。
            // 取不到配置锁（极少见）时按"能唤醒"处理，宁可多跑一轮也不漏同步。
            if let Ok(cfg) = inner.settings.try_lock() {
                if !should_wake(cfg.sync_mode) {
                    return;
                }
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
        warning: None,
        skipped: 0,
    }
}

/// 轮询间隔：`poll_interval_sec` 夹到 2–300s（0 会让循环空转）。
fn poll_interval(cfg: &SyncSettings) -> Duration {
    Duration::from_secs(u64::from(cfg.poll_interval_sec.clamp(2, 300)))
}

/// 下一轮要等多久（纯逻辑，便于单测）。
///
/// - `Interval` → 按 `poll_interval_sec` 定时唤醒；
/// - `Startup` → `None`：**不设定时器**，只有手动「立即同步」/保存设置/切换模式
///   的 [`Inner::notify`] 才会让循环再跑一轮。
fn next_wait(cfg: &SyncSettings) -> Option<Duration> {
    match cfg.sync_mode {
        SyncMode::Interval => Some(poll_interval(cfg)),
        SyncMode::Startup => None,
    }
}

/// 事件是否该唤醒循环：只有轮询模式响应本机复制/删除。
fn should_wake(mode: SyncMode) -> bool {
    mode == SyncMode::Interval
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
        warning: None,
        skipped: 0,
    });
}

/// 等待 `dur`，但被 [`Inner::notify`] 唤醒时立即返回。
async fn wait(inner: &Inner, dur: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(dur) => {}
        _ = inner.notify.notified() => {}
    }
}

/// 等下一轮：`Interval` 用定时器，`Startup` 只等唤醒信号（不设定时器）。
async fn wait_next(inner: &Inner, cfg: &SyncSettings) {
    match next_wait(cfg) {
        Some(d) => wait(inner, d).await,
        // 仅启动时：进程启动跑过一轮后就在这里静默，直到有人 notify
        None => {
            inner.notify.notified().await;
        }
    }
}

/// 出错后的等待：`Interval` 仍保留 `max(间隔, 30s)` 的退避；`Startup` 不自动重试，
/// 下一次手动同步或重启才再试（严格「仅启动时」语义）。
async fn wait_retry(inner: &Inner, cfg: &SyncSettings) {
    match next_wait(cfg) {
        Some(d) => wait(inner, d.max(ERROR_MIN_INTERVAL)).await,
        None => {
            inner.notify.notified().await;
        }
    }
}

/// 同步循环：配置 → 密钥 → 后端 → 墓碑落盘 → 一个周期 → 等待。
///
/// `sync_mode = interval`：跑完等 `poll_interval_sec`，事件也会提前唤醒。
/// `sync_mode = startup`：启动跑一轮后不再设定时器，只等唤醒信号
/// （见 [`wait_next`] / [`should_wake`]）。
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
                warning: None,
                skipped: 0,
            });
            // 未启用分支仍用 2s 的轻量配置轮询（理由见 DISABLED_POLL）
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
            warning: None,
            skipped: 0,
        });

        // 3. 凭证（keyring / 环境变量，阻塞 API → 线程池）
        let secrets = match tauri::async_runtime::spawn_blocking(credentials::load_all).await {
            Ok(Ok(secrets)) => secrets,
            Ok(Err(e)) => {
                publish_error(&inner, &device_id, last_sync_at, e);
                wait_retry(&inner, &cfg).await;
                continue;
            }
            Err(e) => {
                publish_error(
                    &inner,
                    &device_id,
                    last_sync_at,
                    format!("读取凭据失败: {e}"),
                );
                wait_retry(&inner, &cfg).await;
                continue;
            }
        };
        if secrets.passphrase.trim().is_empty() {
            publish_error(
                &inner,
                &device_id,
                last_sync_at,
                NEED_PASSPHRASE.to_string(),
            );
            wait_retry(&inner, &cfg).await;
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
                wait_retry(&inner, &cfg).await;
                continue;
            }
            Err(e) => {
                publish_error(
                    &inner,
                    &device_id,
                    last_sync_at,
                    format!("同步准备失败: {e}"),
                );
                wait_retry(&inner, &cfg).await;
                continue;
            }
        };
        if need_ensure {
            last_ensured = Some(ensure_fp);
        }

        // 5. 待写墓碑先落盘：run_cycle 读的是同一份 sync-state.json，
        //    不先写进去，本轮快照里就不会带这些删除。
        //    两路来源：内存队列（本进程入队）+ 磁盘日志（上个进程删完就退出，
        //    只留下 sync-tombstones.pending）。顺序固定为"读日志 → 写 state →
        //    清日志"：state 写失败时日志必须留着，下次启动重试。
        let from_memory: Vec<String> = {
            let mut guard = inner.pending_tombstones.lock().await;
            let mut taken: Vec<String> = std::mem::take(&mut *guard).into_iter().collect();
            // 集合本身无序：排一下，快照记录的追加顺序才稳定（digest 不抖）
            taken.sort();
            taken
        };
        let read_dir = inner.config_dir.clone();
        let (from_disk, disk_read_ok) = match tauri::async_runtime::spawn_blocking(move || {
            state::read_pending_tombstones(&read_dir)
        })
        .await
        {
            Ok(Ok(list)) => (list, true),
            Ok(Err(e)) => {
                eprintln!("sync: 墓碑日志读取失败 kind={}", e.kind.as_str());
                (Vec::new(), false)
            }
            Err(_) => {
                eprintln!("sync: 墓碑日志读取失败 kind=join");
                (Vec::new(), false)
            }
        };
        if !from_memory.is_empty() || !from_disk.is_empty() {
            let mut st = state::load_or_init(&inner.config_dir);
            let now = state::now_ms();
            for hash in from_memory.iter().filter(|hash| !hash.is_empty()) {
                st.add_tombstone(hash, now);
            }
            for tombstone in &from_disk {
                st.add_tombstone(&tombstone.hash, tombstone.deleted_at);
            }
            match st.save(&inner.config_dir) {
                Ok(()) => {
                    // 写成功才删日志；读失败过（disk_read_ok=false）时也不删
                    if disk_read_ok {
                        if let Err(e) = state::clear_pending_tombstones(&inner.config_dir) {
                            eprintln!("sync: 墓碑日志清理失败 kind={}", e.kind.as_str());
                        }
                    }
                }
                // 保存失败：日志不动，下一轮/下次启动还会读到（宁可重复也不丢删除）
                Err(e) => eprintln!("sync: 墓碑落盘失败 kind={}", e.kind.as_str()),
            }
        } else if disk_read_ok {
            // 日志里没有可合并的墓碑（空文件 / 全是坏行）：清掉，否则每轮都白读一遍
            if let Err(e) = state::clear_pending_tombstones(&inner.config_dir) {
                eprintln!("sync: 墓碑日志清理失败 kind={}", e.kind.as_str());
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
            formats_dir: crate::services::storage_service::formats_dir(&inner.app),
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
            // 但前端 WebView 照常收到，列表因此立刻刷新。
            // 有图片/富文本被跳过时也 emit：前端至少能刷新状态、看到告警。
            if report.inserted > 0
                || report.deleted > 0
                || report.skipped_images > 0
                || report.skipped_formats > 0
            {
                let _ = inner
                    .app
                    .emit(CLIPBOARD_CHANGED, serde_json::json!({"action": "synced"}));
            }
        }
        inner.applying.store(false, Ordering::SeqCst);

        match cycle {
            Ok(report) => {
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
                    warning: sync_warning(
                        report.skipped_images,
                        report.skipped_formats,
                        report.skipped_peers,
                    ),
                    skipped: report.skipped_images + report.skipped_formats,
                });
            }
            Err(e) => {
                publish_error(&inner, &device_id, last_sync_at, e.to_string());
                wait_retry(&inner, &cfg).await;
                continue;
            }
        }

        // 8. 等下一轮：定时器与唤醒信号二选一
        wait_next(&inner, &cfg).await;
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
        /// 这些 key 的 GET 永远返回 5xx（模拟远端读不通的坏对象）
        get_failures: Mutex<HashSet<String>>,
        /// 每个 key 各被 GET 了多少次（断言冷却期"没再请求"）
        gets_by_key: Mutex<HashMap<String, usize>>,
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

        /// 让某个 key 的 GET 一直返回 5xx。
        fn fail_get(&self, key: &str) {
            self.get_failures.lock().unwrap().insert(key.to_string());
        }

        /// 某个 key 的 GET 次数。
        fn gets_for(&self, key: &str) -> usize {
            self.gets_by_key
                .lock()
                .unwrap()
                .get(key)
                .copied()
                .unwrap_or(0)
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
            *self
                .gets_by_key
                .lock()
                .unwrap()
                .entry(key.to_string())
                .or_insert(0) += 1;
            if self.get_failures.lock().unwrap().contains(key) {
                return Err(SyncError::server("GET 失败: status=500"));
            }
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
            formats_dir: dir.join("formats"),
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

    /// 仅启动时模式**没有定时器**：`next_wait` 返回 None，循环只等唤醒信号；
    /// 轮询模式仍返回 `poll_interval_sec`。
    #[test]
    fn sync_mode_startup_has_no_timer() {
        let startup = SyncSettings {
            sync_mode: SyncMode::Startup,
            ..Default::default()
        };
        assert_eq!(next_wait(&startup), None, "startup 不设定时器");

        let interval = SyncSettings {
            poll_interval_sec: 5,
            ..Default::default()
        };
        assert_eq!(
            next_wait(&interval),
            Some(Duration::from_secs(5)),
            "interval 按 poll_interval_sec 定时"
        );
    }

    /// 仅启动时模式下，`clipboard-changed` / `item-deleted` 不得唤醒同步。
    #[test]
    fn wake_events_are_ignored_in_startup_mode() {
        assert!(!should_wake(SyncMode::Startup), "startup 事件不唤醒");
        assert!(should_wake(SyncMode::Interval), "interval 事件立即唤醒");
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

    /// 接收端目标文件是 0 字节（上次落盘被打断）→ 必须删掉重下。
    /// "文件已存在就早退"会把这张图永久留在坏状态：行有了、图是空的。
    #[tokio::test]
    async fn insert_image_redownloads_zero_byte_target() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        let hash = "00112233445566778899aabbccddeeff";
        let bytes: Vec<u8> = (0u8..32).collect();
        std::fs::create_dir_all(&a.images_dir).expect("建 A 的 images 目录");
        let path_a = a.images_dir.join(format!("{hash}.png"));
        std::fs::write(&path_a, &bytes).expect("写图片");
        insert_image_row(&a.db, hash, &path_a.to_string_lossy()).await;

        let ra = run_cycle(&a).await.expect("A 周期");
        assert_eq!(ra.skipped_images, 0, "发送端不该丢图");
        assert!(backend.exists(&format!("blobs/{hash}.bin")));

        // B 本地留一个 0 字节残骸：复用它就会永远坏着
        std::fs::create_dir_all(&b.images_dir).expect("建 B 的 images 目录");
        // 接收端按 `hash 前 16 位` 命名（与 clipboard_monitor::save_image_to_disk 一致）
        let target = b.images_dir.join(format!("{}.png", &hash[..16]));
        std::fs::write(&target, b"").expect("写 0 字节残骸");
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 0);

        let rb = run_cycle(&b).await.expect("B 周期");
        assert_eq!(rb.inserted, 1, "0 字节残骸必须被重下覆盖");
        assert_eq!(rb.skipped_images, 0, "重下成功不该计入跳过");
        assert_eq!(
            std::fs::read(&target).expect("读回 B 的图片"),
            bytes,
            "字节应与 A 完全一致"
        );

        // 再跑一轮：行已存在，不重复插入、也不再重下
        let rb2 = run_cycle(&b).await.expect("B 第二轮");
        assert_eq!(rb2.inserted, 0);
        assert_eq!(count_hash(&b.db, hash).await, 1);
    }

    // -----------------------------------------------------------------------
    // 富文本副本（HTML / RTF 原始字节）
    // -----------------------------------------------------------------------

    /// 插入一条带富文本副本的文本条目（路径为绝对路径，与真实采集一致）。
    async fn insert_rich_text_row(
        db: &SqlitePool,
        hash: &str,
        content: &str,
        html: &Path,
        rtf: &Path,
    ) {
        sqlx::query(
            "INSERT INTO items (type, content, content_hash, preview, html_path, rtf_path)
             VALUES (0, ?, ?, NULL, ?, ?)",
        )
        .bind(content)
        .bind(hash)
        .bind(html.to_string_lossy().to_string())
        .bind(rtf.to_string_lossy().to_string())
        .execute(db)
        .await
        .expect("插入富文本条目");
    }

    /// A 的文本 + html/rtf 原始字节 → 加密 blob → B 落盘，字节逐字节一致。
    #[tokio::test]
    async fn rich_text_round_trip_between_two_devices() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        // CF_HTML 包装 + 带 \bin 段的 RTF：都必须原样搬运（不解析、不 sanitize）
        let html: Vec<u8> =
            b"Version:0.9\r\nStartHTML:0000000105\r\n<html><body>\xe4\xbd\xa0\xe5\xa5\xbd</body></html>"
                .to_vec();
        let rtf: Vec<u8> = b"{\\rtf1\\ansi\\bin4 abcd\\par hi}".to_vec();
        let hash = "11112222333344445555666677778888";

        std::fs::create_dir_all(&a.formats_dir).expect("建 A 的 formats 目录");
        let html_path = a.formats_dir.join(format!("{}.html", &hash[..16]));
        let rtf_path = a.formats_dir.join(format!("{}.rtf", &hash[..16]));
        std::fs::write(&html_path, &html).expect("写 html");
        std::fs::write(&rtf_path, &rtf).expect("写 rtf");
        insert_rich_text_row(&a.db, hash, "带格式的文本", &html_path, &rtf_path).await;

        let ra = run_cycle(&a).await.expect("A 周期");
        assert!(ra.uploaded);
        assert_eq!(ra.skipped_formats, 0, "发送端不该丢格式");
        assert!(
            backend.exists(&format!("blobs/{}.bin", sha256_hex(&html))),
            "html blob 应以内容 hash 为键上传"
        );
        assert!(
            backend.exists(&format!("blobs/{}.bin", sha256_hex(&rtf))),
            "rtf blob 应以内容 hash 为键上传"
        );

        let rb = run_cycle(&b).await.expect("B 周期");
        assert_eq!(rb.inserted, 1, "B 应插入该条目");
        assert_eq!(rb.skipped_formats, 0, "富文本必须全部落地");
        assert_eq!(rb.skipped_images, 0);

        let row: Option<(String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT content, html_path, rtf_path FROM items WHERE content_hash = ?")
                .bind(hash)
                .fetch_optional(&b.db)
                .await
                .expect("查 B 的条目");
        let (content, b_html, b_rtf) = row.expect("B 应有该条目");
        assert_eq!(content, "带格式的文本");

        let b_html = b_html.expect("B 的 html_path 必须有值");
        let b_rtf = b_rtf.expect("B 的 rtf_path 必须有值");
        let b_formats = std::fs::canonicalize(&b.formats_dir).expect("规范化 B 的 formats 目录");
        assert!(
            std::fs::canonicalize(&b_html)
                .expect("B 的 html 文件应已落盘")
                .starts_with(&b_formats),
            "html_path 必须落在 B 自己的 formats 目录内"
        );
        assert_eq!(
            std::fs::read(&b_html).expect("读回 B 的 html"),
            html,
            "html 字节必须逐字节一致"
        );
        assert_eq!(
            std::fs::read(&b_rtf).expect("读回 B 的 rtf"),
            rtf,
            "rtf 字节必须逐字节一致"
        );
    }

    /// blob 键是**内容** hash，不是 record.hash：同文本不同 html 的两条记录
    /// 各自上传自己的 blob，互不覆盖。
    #[tokio::test]
    async fn format_blob_keyed_by_content_hash_not_record_hash() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        let html1: Vec<u8> = b"<html><body>red</body></html>".to_vec();
        let html2: Vec<u8> = b"<html><body>blue</body></html>".to_vec();
        let hash1 = "aaaa1111aaaa1111aaaa1111aaaa1111";
        let hash2 = "bbbb2222bbbb2222bbbb2222bbbb2222";

        std::fs::create_dir_all(&a.formats_dir).expect("建 A 的 formats 目录");
        let rtf_path = a.formats_dir.join("shared.rtf");
        std::fs::write(&rtf_path, b"{\\rtf1 shared}").expect("写 rtf");
        for (hash, html) in [(hash1, &html1), (hash2, &html2)] {
            let path = a.formats_dir.join(format!("{}.html", &hash[..16]));
            std::fs::write(&path, html).expect("写 html");
            insert_rich_text_row(&a.db, hash, "完全相同的正文", &path, &rtf_path).await;
        }

        let ra = run_cycle(&a).await.expect("A 周期");
        assert_eq!(ra.skipped_formats, 0);

        let blob1 = format!("blobs/{}.bin", sha256_hex(&html1));
        let blob2 = format!("blobs/{}.bin", sha256_hex(&html2));
        assert_ne!(blob1, blob2, "两份不同 html 必须是两个 blob key");
        assert_ne!(sha256_hex(&html1), hash1, "blob key 与 record.hash 无关");
        assert!(backend.exists(&blob1) && backend.exists(&blob2));

        // 解密比对：两个对象的内容确实不同
        let read_plain = |key: &str| -> Vec<u8> {
            let sealed = backend.get(key).expect("读对象").expect("对象应存在");
            crypto::open_blob(&KEY, key, &sealed).expect("解密")
        };
        assert_eq!(read_plain(&blob1), html1);
        assert_eq!(read_plain(&blob2), html2);

        // B 两条都落地，且各自的 html 文件内容不同
        let rb = run_cycle(&b).await.expect("B 周期");
        assert_eq!(rb.inserted, 2);
        assert_eq!(rb.skipped_formats, 0);
        let mut seen: Vec<(String, Vec<u8>)> = Vec::new();
        for hash in [hash1, hash2] {
            let path: Option<String> =
                sqlx::query_scalar("SELECT html_path FROM items WHERE content_hash = ?")
                    .bind(hash)
                    .fetch_one(&b.db)
                    .await
                    .expect("查 B 的 html_path");
            let path = path.expect("html_path 必须有值");
            seen.push((path.clone(), std::fs::read(&path).expect("读 B 的 html")));
        }
        assert_eq!(seen[0].1, html1);
        assert_eq!(seen[1].1, html2);
        assert_ne!(seen[0].0, seen[1].0, "两条记录各有自己的本地文件");
    }

    /// 删除条目时把 formats 目录里的副本一起删掉；目录外的文件一个都不许碰。
    #[tokio::test]
    async fn delete_local_removes_format_files() {
        let backend = Arc::new(MockBackend::default());
        let dir = temp_dir();
        let ctx = make_ctx(backend, &dir).await;
        std::fs::create_dir_all(&ctx.formats_dir).expect("建 formats 目录");

        let hash = "cccc3333cccc3333cccc3333cccc3333";
        let html_path = ctx.formats_dir.join(format!("{}.html", &hash[..16]));
        let rtf_path = ctx.formats_dir.join(format!("{}.rtf", &hash[..16]));
        std::fs::write(&html_path, b"<html><body>del</body></html>").expect("写 html");
        std::fs::write(&rtf_path, b"{\\rtf1 del}").expect("写 rtf");
        insert_rich_text_row(&ctx.db, hash, "待删除", &html_path, &rtf_path).await;

        // 目录外的文件（守卫必须挡住）
        let outside = dir.join("outside.html");
        std::fs::write(&outside, b"<html>keep me</html>").expect("写目录外文件");
        let hash_out = "dddd4444dddd4444dddd4444dddd4444";
        insert_rich_text_row(&ctx.db, hash_out, "越界路径", &outside, &outside).await;

        assert!(
            delete_local(&ctx.db, &ctx.images_dir, &ctx.formats_dir, hash)
                .await
                .expect("删除条目"),
            "应删掉一行"
        );
        assert_eq!(count_hash(&ctx.db, hash).await, 0);
        assert!(!html_path.exists(), "html 副本必须一起删掉");
        assert!(!rtf_path.exists(), "rtf 副本必须一起删掉");

        assert!(
            delete_local(&ctx.db, &ctx.images_dir, &ctx.formats_dir, hash_out)
                .await
                .expect("删除越界条目")
        );
        assert!(outside.exists(), "formats 目录外的文件绝不能被删");

        // 幂等：已删过的 hash 再删返回 false
        assert!(
            !delete_local(&ctx.db, &ctx.images_dir, &ctx.formats_dir, hash)
                .await
                .expect("重复删除"),
            "没有该条目时应返回 false"
        );
    }

    /// 远端缺 blob（上传失败/被清理）→ 纯文本照常插入，该格式置 NULL 并计数。
    #[tokio::test]
    async fn missing_format_blob_still_inserts_text() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        let html: Vec<u8> = b"<html><body>gone</body></html>".to_vec();
        let rtf: Vec<u8> = b"{\\rtf1 gone}".to_vec();
        let hash = "eeee5555eeee5555eeee5555eeee5555";
        std::fs::create_dir_all(&a.formats_dir).expect("建 A 的 formats 目录");
        let html_path = a.formats_dir.join(format!("{}.html", &hash[..16]));
        let rtf_path = a.formats_dir.join(format!("{}.rtf", &hash[..16]));
        std::fs::write(&html_path, &html).expect("写 html");
        std::fs::write(&rtf_path, &rtf).expect("写 rtf");
        insert_rich_text_row(&a.db, hash, "缺格式的正文", &html_path, &rtf_path).await;

        run_cycle(&a).await.expect("A 周期");
        // 模拟 html blob 丢了（上传失败 / 远端被清理）
        backend
            .delete(&format!("blobs/{}.bin", sha256_hex(&html)))
            .expect("删掉 html blob");

        let rb = run_cycle(&b).await.expect("B 周期");
        assert_eq!(rb.inserted, 1, "纯文本必须照常插入");
        assert_eq!(rb.skipped_formats, 1, "缺一个格式就计一次");

        let row: Option<(String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT content, html_path, rtf_path FROM items WHERE content_hash = ?")
                .bind(hash)
                .fetch_optional(&b.db)
                .await
                .expect("查 B 的条目");
        let (content, b_html, b_rtf) = row.expect("条目仍在");
        assert_eq!(content, "缺格式的正文");
        assert!(b_html.is_none(), "缺 blob 的格式必须置 NULL");
        let b_rtf = b_rtf.expect("另一种格式不受影响");
        assert_eq!(std::fs::read(&b_rtf).expect("读回 rtf"), rtf);
    }

    /// peer 给的 record.hash 会拼进本地文件名：非 hex 的 hash（如 `../../x`）
    /// 绝不能把格式文件写到 formats 目录之外 —— 该格式跳过，文本照常插入。
    #[tokio::test]
    async fn traversal_like_record_hash_never_writes_format_files() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        let html: Vec<u8> = b"<html><body>evil</body></html>".to_vec();
        let hash = "ffff6666ffff6666ffff6666ffff6666";
        std::fs::create_dir_all(&a.formats_dir).expect("建 A 的 formats 目录");
        let html_path = a.formats_dir.join(format!("{}.html", &hash[..16]));
        std::fs::write(&html_path, &html).expect("写 html");
        insert_rich_text_row(&a.db, hash, "正常条目", &html_path, &html_path).await;
        run_cycle(&a).await.expect("A 周期");

        // 把 A 的快照记录改成恶意 hash 后再让 B 拉：直接构造一条 peer 记录更直接，
        // 这里用 delete_local 之外的最短路径 —— 手动插一条本地没有的记录不可行，
        // 所以改成篡改远端对象：解密 → 改 hash → 重新加密写回。
        let key = format!(
            "devices/{}.json",
            state::load_or_init(&a.state_path).device_id
        );
        let sealed = backend.get(&key).expect("读 A 的快照").expect("快照存在");
        let mut snap: Snapshot = crypto::open_json(&KEY, &key, &sealed).expect("解密快照");
        for rec in &mut snap.records {
            rec.hash = "../../evil".to_string();
        }
        let body = crypto::seal_json(&KEY, &key, &snap).expect("重新加密");
        backend.put(&key, &body, None).expect("写回快照");

        let rb = run_cycle(&b).await.expect("B 周期");
        assert_eq!(rb.inserted, 1, "文本照常插入");
        assert_eq!(rb.skipped_formats, 2, "html 与 rtf 各自跳过（非法 hash）");

        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT content, html_path FROM items WHERE content_hash = ?")
                .bind("../../evil")
                .fetch_optional(&b.db)
                .await
                .expect("查 B 的条目");
        let (content, html_path) = row.expect("条目仍在");
        assert_eq!(content, "正常条目");
        assert!(html_path.is_none(), "非法 hash 不得写 html_path");
        assert!(
            !dir_b.join("evil.html").exists(),
            "绝不能把文件写到 formats 目录外"
        );
    }

    /// 告警文案：图片与富文本分开说，两者都为 0 时没有告警。
    #[test]
    fn warning_mentions_images_and_formats() {
        assert_eq!(sync_warning(0, 0, 0), None);
        assert_eq!(sync_warning(3, 0, 0).as_deref(), Some("3 张图片未同步"));
        assert_eq!(sync_warning(0, 2, 0).as_deref(), Some("2 个富文本未同步"));
        assert_eq!(
            sync_warning(3, 2, 0).as_deref(),
            Some("3 张图片、2 个富文本未同步")
        );
        // peer 快照没拉到时也要说清楚是"某个设备"，而不是本机内容丢了
        assert_eq!(sync_warning(0, 0, 1).as_deref(), Some("1 个设备快照未拉取"));
        assert_eq!(
            sync_warning(1, 1, 2).as_deref(),
            Some("1 张图片、1 个富文本未同步；2 个设备快照未拉取")
        );
    }

    /// 单个图片 blob 下载失败不能拖垮整轮：文本仍要落库，图片计入 skipped。
    #[tokio::test]
    async fn image_blob_get_failure_does_not_block_text() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;

        insert_text(&a.db, "仍然要同步的文本", "text1").await;
        let hash = "aabbccddeeff00112233445566778899";
        let bytes: Vec<u8> = (0u8..16).collect();
        std::fs::create_dir_all(&a.images_dir).expect("建 images 目录");
        let path_a = a.images_dir.join(format!("{}.png", &hash[..16]));
        std::fs::write(&path_a, &bytes).expect("写图片");
        insert_image_row(&a.db, hash, &path_a.to_string_lossy()).await;

        run_cycle(&a).await.expect("A 周期");
        backend.fail_get(&format!("blobs/{hash}.bin"));

        let rb = run_cycle(&b).await.expect("坏 blob 不应让整轮失败");
        assert_eq!(rb.skipped_images, 1, "图片应计入 skipped");
        assert_eq!(count_hash(&b.db, "text1").await, 1, "文本仍应插入");
        assert_eq!(count_hash(&b.db, hash).await, 0, "坏图片不应插入");
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
        // 前 4 次失败、第 5 次成功：刚好用满 MAX_ATTEMPTS
        backend.fail_next_lists(4);
        let dir = temp_dir();
        let ctx = make_ctx(Arc::clone(&backend), &dir).await;
        insert_text(&ctx.db, "重试后仍要成功", "h1").await;

        let report = run_cycle(&ctx).await.expect("四次 5xx 后应成功");
        assert!(report.uploaded);
        assert_eq!(
            backend.list_calls(),
            MAX_ATTEMPTS as usize,
            "应恰好尝试 MAX_ATTEMPTS 次"
        );
    }

    #[tokio::test]
    async fn list_gives_up_after_max_attempts() {
        let backend = Arc::new(MockBackend::default());
        // 一直失败：重试必须在 MAX_ATTEMPTS 次后停下，不能无限重试
        backend.fail_next_lists(MAX_ATTEMPTS as usize + 5);
        let dir = temp_dir();
        let ctx = make_ctx(Arc::clone(&backend), &dir).await;
        insert_text(&ctx.db, "永远失败", "h1").await;

        let err = run_cycle(&ctx).await.expect_err("一直 5xx 必须失败");
        assert_eq!(err.kind, SyncErrorKind::Server);
        assert_eq!(
            backend.list_calls(),
            MAX_ATTEMPTS as usize,
            "重试次数必须收敛到 MAX_ATTEMPTS"
        );
    }

    /// DNS 失败（Network）也要走满重试：校园网污染是间歇的，多试几次能成功
    #[test]
    fn dns_errors_are_retried() {
        let calls = AtomicUsize::new(0);
        let value = retry::<u8>("GET", "blobs/x.bin", || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n < MAX_ATTEMPTS as usize {
                Err(SyncError::network(
                    crate::services::sync::dns_error_message("GET", "blobs/x.bin"),
                ))
            } else {
                Ok(7)
            }
        })
        .expect("DNS 间歇失败后应成功");
        assert_eq!(value, 7);
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS as usize);
    }

    /// 远端有一个读不通的 peer 快照（典型：第三台旧设备留下的坏对象）时，
    /// 整轮同步不能被它拖死：好 peer 的条目照常合并，坏 peer 计入 skipped_peers。
    #[tokio::test]
    async fn bad_peer_snapshot_does_not_block_cycle() {
        let backend = Arc::new(MockBackend::default());
        let dir_a = temp_dir();
        let dir_c = temp_dir();
        let a = make_ctx(Arc::clone(&backend), &dir_a).await;
        let c = make_ctx(Arc::clone(&backend), &dir_c).await;
        insert_text(&a.db, "来自 A 的内容", "hA").await;
        insert_text(&c.db, "来自 C 的内容", "hC").await;
        run_cycle(&a).await.expect("A 上传快照");
        run_cycle(&c).await.expect("C 上传快照");

        // C 的快照读不通
        let c_key = format!(
            "{}/{}.json",
            DEVICES_DIR,
            state::load_or_init(&dir_c).device_id
        );
        backend.fail_get(&c_key);

        let dir_b = temp_dir();
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;
        let report = run_cycle(&b).await.expect("一个坏 peer 不该阻断整轮");

        assert_eq!(report.skipped_peers, 1, "应报告 1 个 peer 未拉取");
        assert_eq!(report.peer_count, 1, "只读到 A 的快照");
        assert_eq!(
            count_hash(&b.db, "hA").await,
            1,
            "好 peer 的条目必须照常同步进来"
        );
        assert_eq!(count_hash(&b.db, "hC").await, 0, "坏 peer 的条目这轮进不来");
    }

    /// 坏 peer 失败后进入冷却期：60s 内不再 GET 它，
    /// 否则每个 5s 周期都要为同一个坏对象等满重试。
    #[tokio::test]
    async fn bad_peer_is_not_retried_within_cooldown() {
        let backend = Arc::new(MockBackend::default());
        let dir_c = temp_dir();
        let c = make_ctx(Arc::clone(&backend), &dir_c).await;
        insert_text(&c.db, "来自 C 的内容", "hC").await;
        run_cycle(&c).await.expect("C 上传快照");

        let c_key = format!(
            "{}/{}.json",
            DEVICES_DIR,
            state::load_or_init(&dir_c).device_id
        );
        backend.fail_get(&c_key);

        let dir_b = temp_dir();
        let b = make_ctx(Arc::clone(&backend), &dir_b).await;
        let first = run_cycle(&b).await.expect("第一轮应 Ok");
        assert_eq!(first.skipped_peers, 1);
        let attempts = backend.gets_for(&c_key);
        assert_eq!(
            attempts, PEER_GET_ATTEMPTS as usize,
            "坏 peer 只试 PEER_GET_ATTEMPTS 次（不为它等满 4 档退避）"
        );

        let second = run_cycle(&b).await.expect("第二轮同样应 Ok");
        assert_eq!(second.skipped_peers, 1, "冷却期内仍如实报告未拉取");
        assert_eq!(
            backend.gets_for(&c_key),
            attempts,
            "冷却期内不该再 GET 同一个坏 peer"
        );
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
        assert!(
            !is_latest_generation(&generation, first),
            "过期事件不该唤醒"
        );
        assert!(
            !is_latest_generation(&generation, second),
            "过期事件不该唤醒"
        );
        assert!(
            is_latest_generation(&generation, third),
            "最后一次事件必须唤醒"
        );

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
        assert!(
            !is_latest_generation(&generation, copy),
            "先到的事件被后来者覆盖"
        );
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
        assert_eq!(
            resolve_device_name("   ", Some("PC-A")),
            "PC-A",
            "纯空白等于没填"
        );
        assert_eq!(resolve_device_name("", None), "device");
        assert_eq!(
            resolve_device_name("", Some("   ")),
            "device",
            "空的 COMPUTERNAME 也不能变成空设备名"
        );
    }

    #[test]
    fn device_id_from_key_parses_only_device_objects() {
        assert_eq!(device_id_from_key("devices/abc-123.json"), Some("abc-123"));
        assert_eq!(device_id_from_key("devices/.json"), None);
        assert_eq!(device_id_from_key("blobs/abc.bin"), None);
        assert_eq!(device_id_from_key("devices/sub/abc.json"), None);
    }
}
