# 云同步（WebDAV / S3）实施计划

> **For agentic workers:** 按任务顺序执行，每个任务跑完自己的测试再进入下一个。仓库 `D:\copy\clipboard-manager-tauri`，Tauri 2 + React 19 + Rust，Windows only。

**Goal:** 用 WebDAV 或 S3 兼容对象存储作为共享存储抽屉，实现多设备准实时同步；同步内容端到端加密，不依赖任何中心服务器。

**Architecture:** 每设备只写自己的 `devices/{deviceId}.json`（全量视图 + 墓碑），对象内容 AES-256-GCM 加密；轮询 LIST/HEAD 比对 ETag，只拉取变化的 peer；按 `content_hash` 去重、按时间戳做 LWW；图片作为内容寻址 blob 单独加密上传。

**Tech Stack:** Rust（ureq + rustls、aes-gcm、argon2、hmac/sha2、quick-xml、keyring）、Tauri 2、React 19、vitest。

---

## 0. 与原始假设的差异（已核实）

| 假设 | 实况 |
|---|---|
| 已有本地敏感内容加密可复用 | **无**。只有 `exclusion_service.rs` 的拦截丢弃；DB/settings 均明文，无 SQLCipher、无 keyring/DPAPI。加密层必须新建 |
| 有异步 HTTP 栈 | 只有阻塞的 `ureq 2`（rustls），`reqwest` 只在 lock 里从未编译。继续用 ureq + `spawn_blocking` |
| items 有同步元数据 | 无 `updated_at`/`remote_id`/`deleted_at`；`content_hash` 无 UNIQUE 约束。同步元数据放独立 state 文件，不动表结构 |
| settings 可随意加字段 | 整对象覆盖、无版本号。Rust 与 TS 必须同一改动加齐，且 Rust `#[serde(default)]` |

## 1. 范围（用户已确认）

- 同步类型：文本、链接、图片（PNG blob）。
- 加密：用户设置同步口令 → Argon2id 派生密钥；口令存 Windows 凭据管理器。
- 拉取后只写本地 DB，不写系统剪贴板（无回环）。
- 删除：只同步显式删除/清空（墓碑）；retention/max_items 自动清理不产生墓碑。
- 不同步：文件路径条目、收藏分组关系。
- **后续增补（2026-10-25）**：富文本 HTML / RTF 副本改为**原始字节无损同步**（快照 `v = 2`，`html_blob`/`html_size`/`rtf_blob`/`rtf_size` + `blobs/{内容hash}.bin`）。不 sanitize、不解析 RTF（`\bin` 段会被破坏）；所有设备共享同一口令，视为同一信任域，HTML 净化只在预览端（`get_rich_preview` + `sandbox=""`）。详见 `docs/CLOUD-SYNC.md` 6.5。

## 2. 远端布局

```
{root}/meta.json                     # 明文 {v,kdf,m,t,p,salt}
{root}/devices/{deviceId}.json       # 密文快照
{root}/blobs/{contentHash}.bin      # 密文图片 blob
{root}/.probe                        # 测试连接临时对象
```

`root` 由 `sync.root_prefix` 配置，默认 `sparrow`。所有请求必须 HTTPS（`http://` 仅 localhost）。

## 3. 本地文件与模块

### 新增（Rust）

| 文件 | 职责 |
|---|---|
| `src-tauri/src/models/sync.rs` | `SyncSettings`/`SyncProvider`/`SyncStatus`/`SyncRecord`/`Snapshot`/`SyncSecrets` |
| `src-tauri/src/services/credentials.rs` | keyring + 环境变量读取（`SPARROW_WEBDAV_PASSWORD`/`SPARROW_S3_SECRET_KEY`/`SPARROW_SYNC_PASSPHRASE`） |
| `src-tauri/src/services/sync/mod.rs` | 子模块声明、`SyncError`、常量、后端工厂 |
| `src-tauri/src/services/sync/backend.rs` | `SyncBackend` trait + `ObjectMeta` |
| `src-tauri/src/services/sync/webdav.rs` | WebDAV 适配（MKCOL/PROPFIND/PUT/GET/HEAD/DELETE + Basic Auth） |
| `src-tauri/src/services/sync/s3.rs` | S3 适配（SigV4、ListObjectsV2、path/virtual-host 两种寻址） |
| `src-tauri/src/services/sync/sigv4.rs` | 纯函数签名器（可单测） |
| `src-tauri/src/services/sync/crypto.rs` | meta.json 引导、Argon2id、AES-256-GCM 信封、blob 二进制信封 |
| `src-tauri/src/services/sync/state.rs` | `config/sync-state.json`（deviceId、peer ETag、墓碑、已上传 blob）原子读写 |
| `src-tauri/src/services/sync/merge.rs` | 纯函数：本地视图 + peer 快照 + 墓碑 → 插入/删除计划 |
| `src-tauri/src/services/sync/snapshot.rs` | 从 SQLite 构建本机快照、时间戳转换 |
| `src-tauri/src/services/sync/service.rs` | 轮询循环、一次同步周期、图片 blob 收发、重试、状态事件 |
| `src-tauri/src/commands/sync.rs` | `get_sync_status`/`sync_now`/`test_sync_connection`/`set_sync_secret`/`has_sync_secrets` |

### 修改（Rust）

- `Cargo.toml`：`aes-gcm 0.10`、`argon2 0.5`、`hmac 0.12`、`quick-xml 0.39`、`uuid 1(v4)`、`rand 0.8`、`keyring 3(windows-native)`。
- `models/settings.rs`：`pub sync: SyncSettings` + `#[serde(default)]`，Default 里给默认值。
- `models/mod.rs` / `services/mod.rs`：导出新模块。
- `services/settings_service.rs`：保存成功后调用 `sync_service::apply_settings(&app, &settings)`（重载配置、唤醒循环）。
- `lib.rs`：setup 里 `sync_service::start(...)`、`app.manage(SyncHandle)`、注册 5 个命令。
- `commands/clipboard.rs`：`delete_item` / `clear_history` 在 DELETE 前收集 `content_hash` 并写墓碑（各 3-5 行，失败只记日志）。

### 新增（前端）

| 文件 | 职责 |
|---|---|
| `src/types/sync.ts` | `SyncSettings`/`SyncStatus`/`SyncProvider` |
| `src/components/SyncSection.tsx` | 云同步设置分区 |
| `src/hooks/useSyncStatus.ts` | `get_sync_status` + `listen('sync-status')` |
| `src/components/__tests__/SyncSection.test.tsx` | 组件测试 |

### 修改（前端）

- `src/types/settings.ts`：`AppSettings.sync` + `DEFAULT_SETTINGS.sync`。
- `src/components/settings-controls.tsx`：新增 `TextInput`、`Segmented`、`StatusDot`。
- `src/components/SettingsPanel.tsx`：`数据` 分区前插入 `<SyncSection/>`。
- `src/components/StatusBar.tsx`：右侧加同步状态点（disabled 不显示）。

## 4. 核心接口

```rust
// models/sync.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncProvider { #[default] Webdav, S3 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSettings {
    pub enabled: bool, pub provider: SyncProvider,
    pub webdav_url: String, pub username: String,
    pub s3_endpoint: String, pub s3_region: String, pub s3_bucket: String,
    pub s3_access_key: String, pub s3_path_style: bool,
    pub root_prefix: String, pub poll_interval_sec: u32,
    pub device_name: String, pub sync_images: bool, pub max_blob_mb: u32,
}
// Default: enabled=false, provider=Webdav, root_prefix="sparrow",
// poll_interval_sec=5, sync_images=true, max_blob_mb=20, 其余空串/false,
// device_name 取 COMPUTERNAME，取不到则 "device"

#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub state: String,        // disabled|syncing|idle|error
    pub device_id: String,
    pub last_sync_at: i64,    // epoch ms
    pub last_error: Option<String>,
    pub pending: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRecord {
    pub hash: String,          // content_hash
    pub kind: i32,             // 0 text / 1 link / 2 image
    pub content: Option<String>,   // text/link 原文；image 为 None
    pub preview: Option<String>,
    pub created_at: i64,       // epoch ms
    pub updated_at: i64,       // epoch ms（= last_used_at）
    pub deleted_at: Option<i64>,
    pub blob_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub v: u32,                // 1
    pub device_id: String,
    pub device_name: String,
    pub generated_at: i64,
    pub records: Vec<SyncRecord>,
}

pub struct SyncSecrets {
    pub webdav_password: String,
    pub s3_secret_key: String,
    pub passphrase: String,
}
```

```rust
// backend.rs
pub struct ObjectMeta { pub key: String, pub etag: Option<String>, pub size: u64 }

pub trait SyncBackend: Send + Sync {
    fn put(&self, key: &str, body: &[u8], if_match: Option<&str>) -> Result<Option<String>, SyncError>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SyncError>;
    fn head(&self, key: &str) -> Result<Option<ObjectMeta>, SyncError>;
    fn delete(&self, key: &str) -> Result<(), SyncError>;
    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, SyncError>;
    fn ensure_dirs(&self) -> Result<(), SyncError> { Ok(()) } // WebDAV MKCOL
}

pub fn build_backend(s: &SyncSettings, secrets: &SyncSecrets) -> Result<Box<dyn SyncBackend>, SyncError>;
```

```rust
// crypto.rs
pub const META_FILE: &str = "meta.json";
pub struct SyncMeta { pub v: u32, pub kdf: String, pub m: u32, pub t: u32, pub p: u32, pub salt: String, pub check: String }
// 引导 meta.json 并返回 (meta, 已校验的密钥)；key 是 root 相对的 `meta.json`（后端 url() 自己拼 {root}/）
pub fn load_or_init_meta(backend: &dyn SyncBackend, passphrase: &str) -> Result<(SyncMeta, [u8; 32]), SyncError>;
pub fn load_or_init_meta_cached(backend: &dyn SyncBackend, passphrase: &str, cached: Option<&KeyCache>) -> Result<(SyncMeta, [u8; 32]), SyncError>;
pub fn derive_key(passphrase: &str, meta: &SyncMeta) -> Result<[u8; 32], SyncError>;
pub fn key_check(key: &[u8; 32]) -> String;
pub fn verify_key_check(meta: &SyncMeta, key: &[u8; 32]) -> Result<(), SyncError>;
pub fn seal_json<T: Serialize>(key: &[u8; 32], aad: &str, v: &T) -> Result<Vec<u8>, SyncError>;
pub fn open_json<T: DeserializeOwned>(key: &[u8; 32], aad: &str, bytes: &[u8]) -> Result<T, SyncError>;
pub fn seal_blob(key: &[u8; 32], aad: &str, bytes: &[u8]) -> Result<Vec<u8>, SyncError>;
pub fn open_blob(key: &[u8; 32], aad: &str, bytes: &[u8]) -> Result<Vec<u8>, SyncError>;
```

`meta.check` 校验：`meta.json` 已存在时，派生 key 后必须用 `verify_key_check` 比对
`check`（`hex(HMAC-SHA256(key, "sparrow-sync-meta-check-v1"))`，常量时间比较）；
`check` 缺失（旧版 meta.json）或不匹配一律返回 `Auth`（口令错，重试无意义，更不能拿错 key
覆盖远端）。首次创建时先派生 key 再写入 `check`；`If-None-Match: *` 撞 409/412 说明别的
设备抢先创建，重新 GET 并以远端 meta 为准校验。`load_or_init_meta_cached` 只缓存派生结果
（指纹 = `sha256(passphrase + salt)`），命中缓存也仍然校验 `check`。

信封：`{"v":1,"alg":"A256GCM","nonce":"b64","ct":"b64"}`；blob：`SPRW1` + 12B nonce + ciphertext。AAD 为对象 key。

```rust
// merge.rs
pub struct LocalItem { pub hash: String, pub updated_at: i64, pub kind: i32 }
pub enum MergeAction { Insert(SyncRecord), Delete(String) }
pub fn plan_merge(
    local: &HashMap<String, LocalItem>,
    snapshots: &[Snapshot],
    tombstones: &[Tombstone],
) -> Vec<MergeAction>;
```

规则：忽略自己的快照；同 hash 取 `updated_at` 最大者；墓碑时间戳比条目新则 Delete，否则保留；本地不存在且未被墓碑覆盖则 Insert。

```rust
// state.rs
pub struct Tombstone { pub hash: String, pub deleted_at: i64 }
pub struct PeerState { pub etag: Option<String>, pub updated_at: i64 }
pub struct SyncStateFile {
    pub device_id: String, pub device_name: String,
    pub peers: HashMap<String, PeerState>,
    pub tombstones: Vec<Tombstone>,
    pub last_upload_hash: Option<String>,
    pub uploaded_blobs: Vec<String>,
    pub last_sync_at: i64,
}
pub fn load_or_init(config_dir: &Path) -> SyncStateFile;
pub fn save(&self, config_dir: &Path) -> Result<(), SyncError>; // tmp + rename
pub fn prune(&mut self, now_ms: i64); // 墓碑保留 60 天，blob 记录上限 2000
```

## 5. 同步周期（service.rs）

1. 读配置；`!enabled` → 状态 `disabled`，每 2s 检查一次配置。
2. 取密钥（keyring 或 env）→ 缺口令 → `error("需要设置同步口令")`。
3. `load_or_init_meta(backend, passphrase)` → `(SyncMeta, [u8;32])`（启动时一次，key 走 `load_or_init_meta_cached` 缓存；命中缓存也仍用 `meta.check` 校验口令，错口令 → `Auth`）。
4. 构建本机快照（`sync_images=false` 时跳过 kind=2）：
   - `SELECT id,type,content,content_hash,preview,file_path, strftime('%s',created_at)*1000, strftime('%s',last_used_at)*1000 FROM items WHERE type IN (0,1,2)`
   - 过滤：`contains_sensitive(content)` 为真 → 跳过（纵深防御，记 hash 前 8 位日志）。
   - 图片：`file_path` 存在且 ≤ `max_blob_mb`，`blob_size` = 文件长度。
5. 序列化快照 → SHA-256；与 `last_upload_hash` 相同则跳过上传。
6. 上传前确保图片 blob 已上传：`HEAD blobs/{hash}.bin`（本地状态里记过的跳过），加密后 `PUT`（`If-None-Match: *`，409/412 视为已存在）。
7. `PUT devices/{id}.json`（`If-Match: 上次ETag`；首次 `If-None-Match: *`）。412 → 重新 HEAD 后重试一次，仍失败则报 Conflict。
8. `LIST devices/` → 对每个 peer：ETag 与 `peers[id].etag` 相同则跳过；否则 GET + `open_json` → 替换缓存。
9. `plan_merge` → 执行：
   - Insert（文本/链接）：`INSERT INTO items(type,content,content_hash,preview) VALUES (?,?,?,?)`；已存在（hash 命中）只 `UPDATE last_used_at`。
   - Insert（图片）：GET blob → 解密 → 写 `images_dir/{hash前16位}.png` → INSERT 带 `file_path`。
   - Delete：查 `file_path` → 删行 → `remove_image_file`。
10. `emit("sync-status", status)`；有变更再 `emit("clipboard-changed", {action:"synced"})`。
11. 保存 state（peers/last_upload_hash/last_sync_at）。

循环：`tokio::time::sleep(interval)` 与 `Notify` 二选一（`tokio::select!`），本地变更通知去抖 800ms。所有 HTTP 在 `spawn_blocking` 中执行。

## 6. 错误与重试

- `SyncError.kind`: `Config | Auth | NotFound | Conflict | Network | Server`。
- 重试：`Network`/`Server`/HTTP 429 最多 3 次，退避 500ms → 2s → 8s（±20% jitter）；`Auth/Config` 不重试。
- 失败后状态 `error`，下一次轮询间隔取 `max(poll, 30s)`，成功后恢复。
- 日志只允许：操作名、后端类型、对象 key 的 SHA-256 前 8 位、字节数、错误 kind。禁止输出 URL query、Authorization、口令、内容明文。

## 7. 配置示例

```jsonc
// WebDAV
"sync": {
  "enabled": true, "provider": "webdav",
  "webdav_url": "https://dav.example.com/remote.php/dav/files/alice/sparrow",
  "username": "alice", "poll_interval_sec": 5,
  "device_name": "DESKTOP-A", "sync_images": true, "root_prefix": "sparrow"
}

// S3
"sync": {
  "enabled": true, "provider": "s3",
  "s3_endpoint": "https://s3.us-east-1.amazonaws.com",
  "s3_region": "us-east-1", "s3_bucket": "sparrow-sync",
  "s3_access_key": "AKIAIOSFODNN7EXAMPLE", "s3_path_style": false,
  "poll_interval_sec": 5, "device_name": "LAPTOP-B", "sync_images": true
}
```

凭证：`SPARROW_WEBDAV_PASSWORD` / `SPARROW_S3_SECRET_KEY` / `SPARROW_SYNC_PASSPHRASE`，或设置面板写入 Windows 凭据管理器（service `Sparrow.Sync`）。

## 8. 测试矩阵

| 层 | 用例 |
|---|---|
| crypto | 加密往返、错口令、密文篡改、AAD 不匹配、信封不含明文、Argon2 同 salt 确定性 |
| backend | WebDAV 路径/请求头（Basic、If-Match）、PROPFIND 解析；S3 SigV4 AWS 官方向量、ListObjectsV2 解析、path/virtual 寻址 |
| merge | LWW、墓碑优先、同 hash 去重、忽略自身、远端删除已存在条目 |
| snapshot/state | 时间戳转换、state 原子写、墓碑裁剪 |
| service | mock 后端：上传去重、无变化不 PUT、拉取后 INSERT、图片 blob 往返、敏感内容不入快照、失败重试与状态 |
| 前端 | SyncSection 渲染/保存调用/密钥走 set_sync_secret；StatusBar 状态点 |
| 手动 | 双设备：文本、图片、敏感拦截、断网恢复、并发写入、显式删除 |

## 9. 验收对照

| 验收项 | 判定 |
|---|---|
| A 复制普通文本，B 在轮询间隔内出现 | 双设备手动 + service 测试 |
| 敏感内容被拦截后不进同步队列 | snapshot 过滤单测 + 手动复制 sk- 密钥 |
| 远端只存密文、日志无明文 | crypto 单测 + 抓包/对象内容检查 + 日志 grep |
| 多设备同时写不互相覆盖 | 每设备独立对象 + If-Match，双设备并发手动 |
| 断网恢复不重复不回环 | state 去重 + ETag 跳过 + 手动拔网 |
| 不破坏本地功能 | 原有 `cargo test --lib`/`vitest`/`tsc` 全绿，采集粘贴手测 |

## 10. 任务拆解

1. **脚手架**：Cargo 依赖、`models/sync.rs`、模块声明、`backend.rs`、`crypto.rs`、`credentials.rs` + 单测。
2. **后端适配**：`webdav.rs`、`s3.rs`、`sigv4.rs` + mock/向量测试。
3. **同步引擎**：`merge.rs`、`state.rs`、`snapshot.rs` + 单测。
4. **接线**：`service.rs`、`commands/sync.rs`、settings/lib/delete 钩子、图片 blob。
5. **前端**：类型、SyncSection、StatusBar、测试。
6. **文档与验收**：`docs/CLOUD-SYNC.md`、全量测试、双设备脚本/说明、构建。

## 11. 风险

- 口令丢失不可恢复（UI 明示）。
- 每设备全量快照：500 条约 100–200KB/次上传；个人量级可接受。
- 同 deviceId 双开：If-Match 412，提示用户重置设备 ID。
- WebDAV 服务器对 PROPFIND/If-Match 支持差异：失败降级为「GET 后比对 ETag」。
- 远端旧 blob 不主动清理（内容寻址，可能被其它条目引用），v1 记入文档。
