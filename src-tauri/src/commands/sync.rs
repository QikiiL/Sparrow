//! 云同步的前端命令：状态查询、立即同步、凭证写入、连接测试。
//!
//! 纪律：任何返回值/错误都不得包含凭证（口令、WebDAV 密码、S3 Secret Key）
//! 或带 userinfo/query 的 URL —— `SyncError` 的 Display 已做脱敏，这里只
//! 透传 `to_string()`，不用 `{:?}` 打印任何 secret 结构体。

use tauri::Manager;

use crate::models::sync::SyncStatus;
use crate::services::credentials::{self, SecretKind, SyncSecretsPresence};
use crate::services::sync::backend::{build_backend, SyncBackend};
use crate::services::sync::service::SyncHandle;
use crate::services::sync::SyncError;

/// 当前同步状态（前端挂载时拉一次，之后靠 `sync-status` 事件更新）。
#[tauri::command]
pub fn get_sync_status(handle: tauri::State<'_, SyncHandle>) -> SyncStatus {
    handle.status()
}

/// 立即跑一轮：唤醒循环，不等结果（结果走 `sync-status` 事件）。
#[tauri::command]
pub fn sync_now(handle: tauri::State<'_, SyncHandle>) {
    handle.sync_now();
}

/// 写一项凭证进 Windows 凭据管理器。空字符串 = 删除。
///
/// 口令/密钥的变更意味着同步密钥要重新派生（fingerprint 变了，缓存自然失效），
/// 所以成功后立刻唤醒循环。
#[tauri::command]
pub fn set_sync_secret(app: tauri::AppHandle, field: String, value: String) -> Result<(), String> {
    let kind = SecretKind::from_field(&field)
        .ok_or_else(|| format!("未知的凭证字段: {field}"))?;
    credentials::set_secret(kind, &value)?;

    if let Some(handle) = app.try_state::<SyncHandle>() {
        handle.sync_now();
    }
    Ok(())
}

/// 三项凭证各自是否已设置（只回答有没有，不回显值）。
#[tauri::command]
pub fn has_sync_secrets() -> SyncSecretsPresence {
    credentials::presence()
}

/// 探测对象的 key，**相对 root**：后端自己会拼 `{root}`。
///
/// 曾经写成 `format!("{root}/.probe-{uuid}")`，后端再拼一次 root，真实路径变成
/// `{base}/{root}/{root}/.probe-*` → 真实 WebDAV 上父集合不存在，直接 409。
pub(crate) fn probe_key() -> String {
    format!(".probe-{}", uuid::Uuid::new_v4())
}

/// 连接探测：写一个临时对象 → 读回校验 → 删除（尽力而为），返回耗时文案。
///
/// 抽成独立函数是为了可测：单测用 mock 后端断言 key 与真实请求路径，
/// 不必拉起 Tauri AppHandle。
pub(crate) fn run_probe(backend: &dyn SyncBackend, key: &str) -> Result<String, SyncError> {
    let started = std::time::Instant::now();
    // If-None-Match: * —— 探测对象本不该存在，冲突即说明 key 撞了
    backend.put(key, b"ok", Some("*"))?;
    let read_back = backend.get(key)?;
    // 删除是尽力而为：探测对象残留不影响结论，也不该把成功报成失败
    let _ = backend.delete(key);
    if read_back.as_deref() != Some(b"ok".as_slice()) {
        return Err(SyncError::config("探测对象读回内容不一致"));
    }
    Ok(format!("连接成功 · {} ms", started.elapsed().as_millis()))
}

/// 连接测试：读当前配置与凭证 → 建后端 → 建目录 → 写一个临时探测对象 →
/// 读回校验 → 删除。不做真实全量同步（那是 `sync_now` 的事）。
///
/// 返回 `连接成功 · {ms} ms`；错误为已脱敏的 `SyncError` Display。
#[tauri::command]
pub async fn test_sync_connection(app: tauri::AppHandle) -> Result<String, String> {
    let settings = crate::services::settings_service::load_settings(&app).sync;

    let secrets = tauri::async_runtime::spawn_blocking(credentials::load_all)
        .await
        .map_err(|e| format!("读取凭据失败: {e}"))??;
    if secrets.passphrase.trim().is_empty() {
        return Err("需要设置同步口令".to_string());
    }

    // 探测对象名带 uuid：并发测试/上次失败残留都不会互相覆盖
    let probe_key = probe_key();

    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let backend = build_backend(&settings, &secrets).map_err(|e| e.to_string())?;
        backend.ensure_dirs().map_err(|e| e.to_string())?;
        run_probe(backend.as_ref(), &probe_key).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("连接测试失败: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::sync::{SyncProvider, SyncSecrets, SyncSettings};
    use crate::services::sync::webdav::WebDavBackend;
    use httpmock::prelude::*;

    fn settings(server: &MockServer) -> SyncSettings {
        SyncSettings {
            provider: SyncProvider::Webdav,
            webdav_url: server.base_url(),
            username: "user".to_string(),
            root_prefix: "sparrow".to_string(),
            ..Default::default()
        }
    }

    fn secrets() -> SyncSecrets {
        SyncSecrets {
            webdav_password: "pass".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn probe_key_is_relative_to_root() {
        let key = probe_key();
        assert!(key.starts_with(".probe-"), "key 必须以 .probe- 开头: {key}");
        // 不能自己带 root：root 由后端拼，带上就会双 root
        assert!(!key.contains("sparrow"), "key 不应包含 root 前缀: {key}");
        assert!(!key.contains('/'), "key 不应是路径: {key}");
        assert_ne!(probe_key(), probe_key(), "每次探测的 key 必须不同");
    }

    #[test]
    fn run_probe_uses_single_root_path() {
        let server = MockServer::start();
        // 路径精确匹配 {base}/{root}/.probe-*：双 root 的路径不会命中这条 mock
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path_matches(Regex::new(r"^/sparrow/\.probe-[0-9a-f-]+$").unwrap());
            then.status(201).header("ETag", "\"probe\"");
        });
        let get = server.mock(|when, then| {
            when.method(GET)
                .path_matches(Regex::new(r"^/sparrow/\.probe-[0-9a-f-]+$").unwrap());
            then.status(200).body("ok");
        });
        let delete = server.mock(|when, then| {
            when.method(DELETE)
                .path_matches(Regex::new(r"^/sparrow/\.probe-[0-9a-f-]+$").unwrap());
            then.status(204);
        });

        let backend = WebDavBackend::new(&settings(&server), &secrets()).expect("构造后端");
        let msg = run_probe(&backend, &probe_key()).expect("探测应成功");

        assert!(msg.starts_with("连接成功 · "), "耗时文案: {msg}");
        assert!(msg.ends_with(" ms"), "耗时文案: {msg}");
        put.assert_hits(1);
        get.assert_hits(1);
        delete.assert_hits(1);
    }

    #[test]
    fn run_probe_fails_when_body_mismatch() {
        let server = MockServer::start();
        let _put = server.mock(|when, then| {
            when.method(PUT);
            then.status(201);
        });
        let _get = server.mock(|when, then| {
            when.method(GET);
            then.status(200).body("not-ok");
        });
        let delete = server.mock(|when, then| {
            when.method(DELETE);
            then.status(204);
        });

        let backend = WebDavBackend::new(&settings(&server), &secrets()).expect("构造后端");
        let err = run_probe(&backend, &probe_key()).expect_err("内容不一致应报错");
        // 探测对象即使失败也要删掉，不留垃圾
        delete.assert_hits(1);
        assert!(err.message.contains("读回"), "{}", err.message);
    }
}
