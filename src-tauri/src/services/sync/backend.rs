//! 对象存储抽象。WebDAV 与 S3 各自实现 [`SyncBackend`]（task 2），
//! 上层（crypto / state / service）只依赖本 trait，便于用 mock 后端测试。
//!
//! 条件写约定：`put` 的 `if_match` 参数
//! - `Some("*")` → 发 `If-None-Match: *`（仅创建，已存在则 409/412）；
//! - `Some(etag)` → 发 `If-Match: <etag>`（乐观并发，不匹配则 412）；
//! - `None` → 无条件覆盖。
//!
//! 后端实现必须把 401/403 映射为 [`SyncErrorKind::Auth`]、404 为
//! [`SyncErrorKind::NotFound`]、409/412 为 [`SyncErrorKind::Conflict`]、
//! 5xx/429 为 [`SyncErrorKind::Server`]、传输层错误为 [`SyncErrorKind::Network`]。

use crate::models::sync::{SyncProvider, SyncSecrets, SyncSettings};

use super::s3::S3Backend;
use super::webdav::WebDavBackend;
use super::SyncError;

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub key: String,
    pub etag: Option<String>,
    /// 由 HEAD / LIST（PROPFIND 的 `getcontentlength`）填充，当前仅用于诊断与
    /// 大小展示，业务逻辑不依赖它，故允许未被读取。
    #[allow(dead_code)]
    pub size: u64,
}

pub trait SyncBackend: Send + Sync {
    /// 写入对象，成功时返回远端 ETag（服务器不返回则 None）。
    fn put(
        &self,
        key: &str,
        body: &[u8],
        if_match: Option<&str>,
    ) -> Result<Option<String>, SyncError>;
    /// 读取对象；不存在返回 `Ok(None)`。
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SyncError>;
    /// 取元数据（ETag/大小）；不存在返回 `Ok(None)`。
    fn head(&self, key: &str) -> Result<Option<ObjectMeta>, SyncError>;
    fn delete(&self, key: &str) -> Result<(), SyncError>;
    /// 列前缀下的对象（不含目录本身）。
    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, SyncError>;
    /// WebDAV 的 MKCOL；S3 无需目录，默认空实现。
    fn ensure_dirs(&self) -> Result<(), SyncError> {
        Ok(())
    }
}

/// 按配置构造后端（URL / 凭证不合法时返回 [`SyncErrorKind::Config`]）。
pub fn build_backend(
    settings: &SyncSettings,
    secrets: &SyncSecrets,
) -> Result<Box<dyn SyncBackend>, SyncError> {
    Ok(match settings.provider {
        SyncProvider::Webdav => Box::new(WebDavBackend::new(settings, secrets)?),
        SyncProvider::S3 => Box::new(S3Backend::new(settings, secrets)?),
    })
}
