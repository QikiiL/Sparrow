//! 云同步端到端测试：**真实 HTTP 协议层**，不用 mock backend。
//!
//! 下面这个用 `std::net::TcpListener` 起的最小 WebDAV 服务器会真正读 HTTP/1.1
//! 请求行/头/body，按方法分发 MKCOL / PUT / GET / HEAD / DELETE / PROPFIND，
//! 校验 Basic Auth、生成 ETag、处理 `If-None-Match: *` 与 `If-Match`。
//! 客户端侧是真实的 [`WebDavBackend`]（ureq）与真实的 [`run_cycle`]：
//! 两台"设备"各自一个临时 config 目录 + 独立 `sqlite::memory:` 库，经服务器往返。
//!
//! 覆盖的协议点（与 `webdav.rs` 的调用一一对应）：
//! - Basic Auth（错误口令 → 401 + `WWW-Authenticate`，客户端映射为 `Auth` 且不重试）
//! - MKCOL 201/405（`ensure_dirs` 的三次建目录 + 重复调用的 405 视为成功）
//! - PUT 201/204 + `ETag`，`If-None-Match: *`（仅创建）、`If-Match`（乐观并发 412）
//! - GET/HEAD 200/404（不存在 → `Ok(None)`）、DELETE 204/404
//! - PROPFIND Depth: 1 → 207 multistatus（`&quot;etag&quot;` 实体 + getcontentlength）
//!
//! 日志纪律：服务器不打印请求头（Authorization）与请求体；断言消息里只出现
//! 对象路径/字节数，与生产日志纪律一致。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::models::sync::{Snapshot, SyncProvider, SyncSecrets, SyncSettings};
use crate::services::sync::backend::SyncBackend;
use crate::services::sync::service::{run_cycle, CycleCtx};
use crate::services::sync::state;
use crate::services::sync::webdav::WebDavBackend;
use crate::services::sync::{crypto, percent_decode, SyncErrorKind};

/// 服务器用户名 / 口令（Basic Auth）。
const USER: &str = "alice";
const PASS: &str = "secret";
/// `WebDavBackend` 配置里的集合根（`{url}/{root}/{key}`）。
const ROOT: &str = "sparrow";
/// 测试口令。真实派生走 meta.json 里的参数，两台设备必须得到同一把密钥。
const PASSPHRASE: &str = "test-pass";

// ---------------------------------------------------------------------------
// 最小 WebDAV 服务器
// ---------------------------------------------------------------------------

/// 服务器端状态：对象表 + 集合表 + 计数。
///
/// 对象的 key 是 percent-decode 之后的 URL path（如 `/dav/sparrow/devices/x.json`），
/// ETag 一律是 body 的 SHA-256 hex（带引号），因此"改坏内容"就等于"换 ETag"。
#[derive(Default)]
struct DavState {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    collections: Mutex<HashSet<String>>,
    /// 收到的 PUT 次数（含被 412 拒绝的），用于断言"没有多余上传"。
    puts: AtomicUsize,
}

/// 起在后台线程里的服务器句柄；drop 时置停止标志并 join。
struct DavServer {
    port: u16,
    state: Arc<DavState>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DavServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("取本地地址").port();
        // 非阻塞 accept：靠 5ms 轮询检查停止标志，drop 时能干净退出
        listener.set_nonblocking(true).expect("设置非阻塞");

        let state = Arc::new(DavState::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // 每个连接只处理一个请求（响应里带 Connection: close）
                            let _ = stream.set_nonblocking(false);
                            let _ = handle_conn(stream, &state);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            port,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/dav", self.port)
    }

    fn puts(&self) -> usize {
        self.state.puts.load(Ordering::SeqCst)
    }

    fn object(&self, path: &str) -> Option<Vec<u8>> {
        self.state
            .objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned()
    }

    fn all_objects(&self) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = self
            .state
            .objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(path, body)| (path.clone(), body.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

impl Drop for DavServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 一个待写回的响应。
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        _ => "Unknown",
    }
}

/// ETag = body 的 SHA-256 hex（带引号，与真实服务器一致）。
fn etag_quoted(body: &[u8]) -> String {
    let hex: String = Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("\"{hex}\"")
}

/// 只要 hex 部分（PROPFIND 里要放进 `&quot;…&quot;`）。
fn etag_hex(body: &[u8]) -> String {
    Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn authorized(headers: &HashMap<String, String>) -> bool {
    let expected = format!("Basic {}", B64.encode(format!("{USER}:{PASS}")));
    headers.get("authorization") == Some(&expected)
}

/// 读一个请求（请求行 + 头 + Content-Length 指定的 body）并写回响应。
fn handle_conn(mut stream: TcpStream, state: &DavState) -> std::io::Result<()> {
    // 防止半开连接把 drop 时的 join 拖住
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    // 服务器端 key 一律是 percent-decode 后的 path（query 由客户端保证不存在）
    let path = percent_decode(target.split('?').next().unwrap_or("/"));

    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut raw = String::new();
        if reader.read_line(&mut raw)? == 0 {
            break;
        }
        let header = raw.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }

    let response = if !authorized(&headers) {
        Response::new(401).header("WWW-Authenticate", "Basic realm=\"test\"")
    } else {
        match method.as_str() {
            "MKCOL" => mkcol(state, &path),
            "PUT" => put(state, &path, &body, &headers),
            "GET" | "HEAD" => get(state, &path),
            "DELETE" => delete(state, &path),
            "PROPFIND" => propfind(state, &path),
            _ => Response::new(405),
        }
    };

    write_response(&mut stream, &response, method != "HEAD")
}

fn write_response(stream: &mut TcpStream, resp: &Response, send_body: bool) -> std::io::Result<()> {
    let mut head = String::new();
    head.push_str(&format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status)));
    for (name, value) in &resp.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    head.push_str("Connection: close\r\n\r\n");

    let mut bytes = head.into_bytes();
    if send_body {
        bytes.extend_from_slice(&resp.body);
    }
    stream.write_all(&bytes)?;
    stream.flush()
}

fn mkcol(state: &DavState, path: &str) -> Response {
    let created = state
        .collections
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_string());
    if created {
        Response::new(201)
    } else {
        Response::new(405)
    }
}

/// 父集合是否已存在（`/a/b/c` 的父是 `/a/b`）。
fn parent_collection_exists(state: &DavState, path: &str) -> bool {
    let parent = match path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent,
        // 没有父（根路径）时不额外校验
        _ => return true,
    };
    state
        .collections
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(parent)
}

fn put(state: &DavState, path: &str, body: &[u8], headers: &HashMap<String, String>) -> Response {
    state.puts.fetch_add(1, Ordering::SeqCst);

    // 真实 WebDAV 的硬约束：父集合不存在 → 409 Conflict。
    // 有了这条校验，"meta 的 key 多带了一层 root"（落到 {root}/{root}/meta.json）
    // 会在测试里立刻变红，而不是等真实服务器在用户机器上 409。
    if !parent_collection_exists(state, path) {
        return Response::new(409);
    }

    {
        let objects = state.objects.lock().unwrap_or_else(|e| e.into_inner());
        let current = objects.get(path).map(|existing| etag_quoted(existing));
        // If-None-Match: * → 仅创建，已存在则 412
        if headers.get("if-none-match").map(String::as_str) == Some("*") && current.is_some() {
            return Response::new(412);
        }
        // If-Match: <etag> → 乐观并发，不匹配（含对象不存在）则 412
        if let Some(expected) = headers.get("if-match") {
            if current.as_deref() != Some(expected.as_str()) {
                return Response::new(412);
            }
        }
    }

    let created = state
        .objects
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_string(), body.to_vec())
        .is_none();

    Response::new(if created { 201 } else { 204 }).header("ETag", &etag_quoted(body))
}

fn get(state: &DavState, path: &str) -> Response {
    let objects = state.objects.lock().unwrap_or_else(|e| e.into_inner());
    match objects.get(path) {
        Some(body) => Response::new(200)
            .header("ETag", &etag_quoted(body))
            .body(body.clone()),
        None => Response::new(404),
    }
}

fn delete(state: &DavState, path: &str) -> Response {
    let removed = state
        .objects
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path)
        .is_some();
    if removed {
        Response::new(204)
    } else {
        Response::new(404)
    }
}

/// Depth: 1 的 PROPFIND：集合自身一项 + 直接子项（含 getetag / getcontentlength）。
fn propfind(state: &DavState, path: &str) -> Response {
    let collection = path.trim_end_matches('/').to_string();
    let prefix = format!("{collection}/");

    let children: Vec<(String, Vec<u8>)> = {
        let objects = state.objects.lock().unwrap_or_else(|e| e.into_inner());
        let mut found: Vec<(String, Vec<u8>)> = objects
            .iter()
            .filter(|(key, _)| {
                key.starts_with(&prefix) && !key[prefix.len()..].contains('/')
            })
            .map(|(key, body)| (key.clone(), body.clone()))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    };

    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?><d:multistatus xmlns:d="DAV:">"#,
    );
    // 集合自身（客户端按"以 / 结尾"识别并跳过）
    xml.push_str(&format!(
        concat!(
            "<d:response><d:href>{}/</d:href><d:propstat><d:prop>",
            "<d:resourcetype><d:collection/></d:resourcetype>",
            "</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
        ),
        collection
    ));
    for (key, body) in &children {
        xml.push_str(&format!(
            concat!(
                "<d:response><d:href>{}</d:href><d:propstat><d:prop>",
                "<d:getetag>&quot;{}&quot;</d:getetag>",
                "<d:getcontentlength>{}</d:getcontentlength>",
                "</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
            ),
            key,
            etag_hex(body),
            body.len()
        ));
    }
    xml.push_str("</d:multistatus>");

    Response::new(207)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(xml.into_bytes())
}

// ---------------------------------------------------------------------------
// 客户端夹具：临时目录、内存库、配置、密钥引导
// ---------------------------------------------------------------------------

/// 每个测试一个独立临时目录，drop 时删掉。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sparrow-e2e-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

fn settings(server: &DavServer, device_name: &str) -> SyncSettings {
    SyncSettings {
        enabled: true,
        provider: SyncProvider::Webdav,
        webdav_url: server.url(),
        username: USER.to_string(),
        root_prefix: ROOT.to_string(),
        poll_interval_sec: 5,
        device_name: device_name.to_string(),
        ..Default::default()
    }
}

fn secrets() -> SyncSecrets {
    SyncSecrets {
        webdav_password: PASS.to_string(),
        passphrase: PASSPHRASE.to_string(),
        ..Default::default()
    }
}

fn backend(server: &DavServer, device_name: &str) -> WebDavBackend {
    WebDavBackend::new(&settings(server, device_name), &secrets()).expect("构造 WebDAV 后端")
}

/// 服务器上的对象路径（服务器端 key）：`/dav/{root}/{key}`。
fn dav_path(key: &str) -> String {
    format!("/dav/{ROOT}/{key}")
}

/// `meta.json` 的实际线上路径：**root 相对**的 key，后端自己拼 `{root}/`。
///
/// 曾经这里断言的是 `/dav/{root}/{root}/meta.json`（双 root），真实 WebDAV 对
/// 不存在的父集合返回 409，于是每轮都误判"别人抢先创建"→ 重 GET 404 → 永久失败。
fn meta_path() -> String {
    dav_path(crypto::META_FILE)
}

/// 真实引导：MKCOL 建目录 → GET `meta.json`（404 则用 `If-None-Match: *` 创建）
/// → Argon2id 派生密钥并校验 `check`。默认参数（64 MiB / 3 轮）约 0.3s，一个测试只做一次。
fn bootstrap_key(server: &DavServer, device_name: &str) -> [u8; 32] {
    let b = backend(server, device_name);
    b.ensure_dirs().expect("MKCOL 建目录");
    let (_, key) = crypto::load_or_init_meta(&b, PASSPHRASE).expect("引导 meta.json");
    key
}

/// 预置小参数 `meta.json` 再派生：m=8192/t=1 让 Argon2 从 ~300ms 降到 ~5ms，
/// 其余路径（PUT 创建 → GET 读回 → derive_key → 校验 check）与真实完全一致。
fn seeded_key(server: &DavServer, device_name: &str) -> [u8; 32] {
    let b = backend(server, device_name);
    b.ensure_dirs().expect("MKCOL 建目录");
    let mut meta = crypto::SyncMeta {
        v: 1,
        kdf: "argon2id".to_string(),
        m: 8192,
        t: 1,
        p: 1,
        salt: B64.encode([0x5Au8; 16]),
        check: String::new(),
    };
    // check 必须由口令派生出的 key 算出（与 load_or_init_meta 用同一个公式）
    let key = crypto::derive_key(PASSPHRASE, &meta).expect("派生密钥");
    meta.check = crypto::key_check(&key);
    let body = serde_json::to_vec(&meta).expect("序列化 meta");
    b.put(crypto::META_FILE, &body, Some("*"))
        .expect("写 meta.json");

    let (loaded, loaded_key) = crypto::load_or_init_meta(&b, PASSPHRASE).expect("读回 meta.json");
    assert_eq!(loaded, meta, "meta.json 必须经真实 GET 读回");
    assert_eq!(loaded_key, key);
    loaded_key
}

async fn make_ctx(server: &DavServer, dir: &Path, device_name: &str, key: [u8; 32]) -> CycleCtx {
    CycleCtx {
        db: memory_pool().await,
        backend: Arc::new(backend(server, device_name)),
        key,
        settings: settings(server, device_name),
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

async fn count_hash(db: &SqlitePool, hash: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE content_hash = ?")
        .bind(hash)
        .fetch_one(db)
        .await
        .expect("计数")
}

/// 本机快照的对象 key（`devices/{device_id}.json`）。
fn snapshot_key(dir: &Path) -> String {
    format!("devices/{}.json", state::load_or_init(dir).device_id)
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// 1. 文本往返：A 写入 → 服务器 → B 落库，全程真实 HTTP。
#[tokio::test]
async fn text_round_trip_over_real_http() {
    let server = DavServer::start();
    let key = bootstrap_key(&server, "A");
    let dir_a = TempDir::new("rt-a");
    let dir_b = TempDir::new("rt-b");
    let a = make_ctx(&server, dir_a.path(), "A", key).await;
    let b = make_ctx(&server, dir_b.path(), "B", key).await;

    insert_text(&a.db, "hello e2e", "hash-hello").await;

    let ra = run_cycle(&a).await.expect("A 周期");
    assert!(ra.uploaded, "首次应上传快照");
    assert_eq!(ra.peer_count, 0, "还没有 peer");
    assert_eq!(ra.inserted, 0);

    let rb = run_cycle(&b).await.expect("B 周期");
    assert_eq!(rb.peer_count, 1, "B 应读到 A 的快照");
    assert_eq!(rb.inserted, 1, "B 应插入一条");

    let content: Option<String> =
        sqlx::query_scalar("SELECT content FROM items WHERE content_hash = ?")
            .bind("hash-hello")
            .fetch_optional(&b.db)
            .await
            .expect("查 B 的库");
    assert_eq!(content.as_deref(), Some("hello e2e"));

    // 服务器上确实躺着 meta.json 与 A 的密文快照
    assert!(
        server.object(&meta_path()).is_some(),
        "meta.json 应已创建"
    );
    let snap_key = snapshot_key(dir_a.path());
    let sealed = server
        .object(&dav_path(&snap_key))
        .expect("A 的快照应已上传");
    let snap: Snapshot = crypto::open_json(&key, &snap_key, &sealed).expect("解密 A 的快照");
    // 快照里的 device_id 就是 A 的 sync-state.json 里那个；device_name 取
    // settings.device_name（UI 里填的名字必须生效）
    assert_eq!(snap.device_id, state::load_or_init(dir_a.path()).device_id);
    assert_eq!(snap.device_name, "A");
    assert!(
        snap.records.iter().any(|r| r.hash == "hash-hello"),
        "快照里应含该条目"
    );

    // 再跑一轮：内容没变 → 不再 PUT
    let puts_before = server.puts();
    let ra2 = run_cycle(&a).await.expect("A 第二次周期");
    assert!(!ra2.uploaded, "digest 没变不应再上传");
    assert_eq!(server.puts(), puts_before, "无变化不得新增 PUT");
}

/// 2. 敏感内容不落地：明文既不在服务器字节里，也不在解密后的快照里。
#[tokio::test]
async fn sensitive_content_never_reaches_server_in_plaintext() {
    let server = DavServer::start();
    let key = seeded_key(&server, "A");
    let dir = TempDir::new("sec");
    let a = make_ctx(&server, dir.path(), "A", key).await;

    // 拼接构造假密钥，源码里不出现完整字面量
    let secret = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2");
    insert_text(&a.db, &format!("我的密钥是 {secret} 别外传"), "sec-hash").await;
    insert_text(&a.db, "普通内容", "ok-hash").await;

    run_cycle(&a).await.expect("周期");

    let snap_key = snapshot_key(dir.path());
    let sealed = server.object(&dav_path(&snap_key)).expect("快照应已上传");

    // (a) 线上字节：没有明文密钥，也没有普通条目的明文（密文对象）
    let raw = String::from_utf8_lossy(&sealed).into_owned();
    assert!(!raw.contains(&secret), "密文对象里出现明文密钥");
    assert!(!raw.contains("a1B2c3D4"), "密文对象里出现密钥片段");
    assert!(!raw.contains("普通内容"), "密文对象里出现明文正文");

    // (b) 解密后：敏感条目被排除规则挡在快照之外，普通条目正常同步
    let snap: Snapshot = crypto::open_json(&key, &snap_key, &sealed).expect("解密快照");
    let json = serde_json::to_string(&snap).expect("序列化快照");
    assert!(!json.contains(&secret), "解密后也不得有明文密钥");
    assert!(
        snap.records.iter().any(|r| r.hash == "ok-hash"),
        "普通条目应正常上传"
    );
    assert!(
        snap.records.iter().all(|r| r.hash != "sec-hash"),
        "敏感条目不应进入快照"
    );

    // (c) 扫描服务器上所有对象（meta.json 本身只有 KDF 参数与 salt）
    for (path, body) in server.all_objects() {
        assert!(
            !String::from_utf8_lossy(&body).contains(&secret),
            "对象 {path} 里出现明文密钥"
        );
    }
}

/// 3. 错口令：对真实 meta.json 的 `check` 校验必须整轮失败，且不写任何对象。
#[tokio::test]
async fn wrong_passphrase_is_rejected_by_meta_check() {
    let server = DavServer::start();
    let _key = seeded_key(&server, "A");

    let b = backend(&server, "B");
    let err = crypto::load_or_init_meta(&b, "wrong-pass").expect_err("错口令必须显式失败");
    assert_eq!(err.kind, SyncErrorKind::Auth);
    assert_eq!(err.message, crypto::AUTH_MISMATCH);

    // 服务器上只有 meta.json，B 没有创建/覆盖任何对象
    let objects: Vec<String> = server
        .all_objects()
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        objects,
        vec![meta_path()],
        "错口令期间不得写入任何对象: {objects:?}"
    );
}

/// 4. ETag / If-Match：无变化不 PUT；远端被改坏 → 412 → 重新 HEAD 后覆盖成功。
#[tokio::test]
async fn etag_if_match_recovers_from_precondition_failure() {
    let server = DavServer::start();
    let key = seeded_key(&server, "A");
    let dir = TempDir::new("etag");
    let a = make_ctx(&server, dir.path(), "A", key).await;
    insert_text(&a.db, "第一条", "h1").await;

    let first = run_cycle(&a).await.expect("首轮");
    assert!(first.uploaded);
    let puts_after_first = server.puts();

    // 内容没变 → digest 命中 → 一次 PUT 都不发生
    let second = run_cycle(&a).await.expect("次轮");
    assert!(!second.uploaded, "digest 没变不应再上传");
    assert_eq!(server.puts(), puts_after_first, "无变化不得新增 PUT");

    // 用另一个 backend 把远端快照改坏：内容变了 → ETag 变
    let snap_key = snapshot_key(dir.path());
    let other = backend(&server, "A");
    other
        .put(&snap_key, br#"{"v":1,"corrupted":true}"#, None)
        .expect("覆盖远端快照");
    let stale_etag = other
        .head(&snap_key)
        .expect("HEAD 远端快照")
        .expect("对象存在")
        .etag
        .expect("服务器应返回 ETag");

    // 本地新增一条 → digest 变 → 带旧 If-Match 的 PUT 必然 412 → 重试一次成功
    insert_text(&a.db, "第二条", "h2").await;
    let puts_before = server.puts();
    let third = run_cycle(&a).await.expect("412 后应重试成功，不能死循环");
    assert!(third.uploaded, "内容变了应上传");
    assert_eq!(
        server.puts(),
        puts_before + 2,
        "一次被 412 拒绝 + 一次成功，不应死循环重试"
    );

    // 远端现在是完整可解密的新快照，且 ETag 已刷新
    let sealed = server.object(&dav_path(&snap_key)).expect("快照存在");
    let snap: Snapshot = crypto::open_json(&key, &snap_key, &sealed).expect("解密新快照");
    let hashes: Vec<&str> = snap.records.iter().map(|r| r.hash.as_str()).collect();
    assert!(hashes.contains(&"h1"), "旧条目应在: {hashes:?}");
    assert!(hashes.contains(&"h2"), "新条目应在: {hashes:?}");
    let fresh_etag = other
        .head(&snap_key)
        .expect("HEAD")
        .expect("对象存在")
        .etag
        .expect("有 ETag");
    assert_ne!(fresh_etag, stale_etag, "覆盖成功后 ETag 必须变化");
}

/// 5. 墓碑删除：A 删行 + 写墓碑 → A 周期 → B 周期 → B 的条目消失。
#[tokio::test]
async fn tombstone_deletes_peer_item_through_server() {
    let server = DavServer::start();
    let key = seeded_key(&server, "A");
    let dir_a = TempDir::new("tomb-a");
    let dir_b = TempDir::new("tomb-b");
    let a = make_ctx(&server, dir_a.path(), "A", key).await;
    let b = make_ctx(&server, dir_b.path(), "B", key).await;

    insert_text(&a.db, "待删除的内容", "h-del").await;
    run_cycle(&a).await.expect("A 周期");
    run_cycle(&b).await.expect("B 周期");
    assert_eq!(count_hash(&b.db, "h-del").await, 1, "B 应先同步到该条目");

    // A 显式删除：删行 + 写墓碑（模拟 commands/clipboard.rs 的删除钩子）
    sqlx::query("DELETE FROM items WHERE content_hash = ?")
        .bind("h-del")
        .execute(&a.db)
        .await
        .expect("删 A 的行");
    let mut st = state::load_or_init(dir_a.path());
    st.add_tombstone("h-del", state::now_ms());
    st.save(dir_a.path()).expect("保存 A 的 state");

    run_cycle(&a).await.expect("A 周期（带墓碑）");
    let rb = run_cycle(&b).await.expect("B 周期");

    assert_eq!(rb.deleted, 1, "B 应按墓碑删除一条");
    assert_eq!(count_hash(&b.db, "h-del").await, 0, "B 的条目应被删掉");
}

/// 6. 协议层本身：401、MKCOL 201/405、PUT 条件写、GET/HEAD/DELETE、PROPFIND 207。
#[tokio::test]
async fn protocol_layer_covers_auth_and_all_methods() {
    let server = DavServer::start();

    // 错误口令 → 401 + WWW-Authenticate，客户端映射为 Auth（不重试）
    let wrong = SyncSecrets {
        webdav_password: "wrong".to_string(),
        ..Default::default()
    };
    let denied = WebDavBackend::new(&settings(&server, "X"), &wrong).expect("构造后端");
    let err = denied.list("devices").expect_err("401 应报错");
    assert_eq!(err.kind, SyncErrorKind::Auth);
    assert!(server.puts() == 0, "认证失败不应有 PUT");

    // 正确口令：MKCOL 三次；重复调用命中 405 分支（同样视为成功）
    let ok = backend(&server, "X");
    ok.ensure_dirs().expect("首次 MKCOL");
    ok.ensure_dirs().expect("已存在（405）应视作成功");

    // PUT：If-None-Match: * 仅创建；重复创建 → 412
    let etag1 = ok
        .put("blobs/x.bin", b"abc", Some("*"))
        .expect("首次写")
        .expect("服务器应回 ETag");
    assert!(etag1.starts_with('"') && etag1.ends_with('"'), "{etag1}");
    assert_eq!(
        ok.put("blobs/x.bin", b"abc", Some("*")).unwrap_err().kind,
        SyncErrorKind::Conflict,
        "If-None-Match: * 命中已存在对象应 412"
    );

    // PUT：If-Match 命中 → 覆盖成功且 ETag 变；旧 ETag → 412
    let etag2 = ok
        .put("blobs/x.bin", b"abcd", Some(&etag1))
        .expect("If-Match 命中")
        .expect("服务器应回 ETag");
    assert_ne!(etag1, etag2);
    assert_eq!(
        ok.put("blobs/x.bin", b"x", Some(&etag1)).unwrap_err().kind,
        SyncErrorKind::Conflict,
        "旧 ETag 应 412"
    );

    // HEAD / GET 反映最后一次成功的写入（b"abcd"）
    let meta = ok.head("blobs/x.bin").expect("HEAD").expect("对象存在");
    assert_eq!(meta.size, 4, "长度应为最后一次成功写入的字节数");
    assert_eq!(meta.etag.as_deref(), Some(etag2.as_str()));
    assert_eq!(
        ok.get("blobs/x.bin").expect("GET").as_deref(),
        Some(&b"abcd"[..])
    );

    // PROPFIND Depth: 1 → 207 multistatus（含 getetag / getcontentlength）
    ok.put("blobs/y.bin", b"yy", None).expect("无条件写");
    let listed = ok.list("blobs").expect("PROPFIND");
    let keys: Vec<&str> = listed.iter().map(|m| m.key.as_str()).collect();
    assert_eq!(keys, vec!["blobs/x.bin", "blobs/y.bin"]);
    let sizes: Vec<u64> = listed.iter().map(|m| m.size).collect();
    assert_eq!(sizes, vec![4, 2], "getcontentlength 应逐项正确");
    assert_eq!(
        listed[0].etag.as_deref(),
        Some(etag2.as_str()),
        "getetag 应是服务器 ETag 原样（含引号，经 &quot; 实体还原）"
    );

    // DELETE：存在 → 204；不存在 → 404（客户端把 404 视为删除成功）
    ok.delete("blobs/x.bin").expect("DELETE");
    assert!(ok.get("blobs/x.bin").expect("GET").is_none(), "删除后应 404");
    assert!(ok.head("blobs/x.bin").expect("HEAD").is_none());
    ok.delete("blobs/x.bin").expect("重复删除视为成功");
    assert_eq!(
        ok.list("blobs")
            .expect("PROPFIND")
            .into_iter()
            .map(|m| m.key)
            .collect::<Vec<String>>(),
        vec!["blobs/y.bin".to_string()],
        "只剩一个对象"
    );
}
