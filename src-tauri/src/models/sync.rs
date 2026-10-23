use serde::{Deserialize, Serialize};

/// 同步后端类型。序列化为小写字符串，前端 `types/sync.ts` 与此保持一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncProvider {
    #[default]
    Webdav,
    S3,
}

/// 云同步配置。整对象随 settings.json 覆盖写，所以必须 `#[serde(default)]`：
/// 旧版 settings.json 里没有 `sync` 字段时不能导致整个结构体反序列化失败。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSettings {
    pub enabled: bool,
    pub provider: SyncProvider,
    pub webdav_url: String,
    pub username: String,
    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_bucket: String,
    pub s3_access_key: String,
    pub s3_path_style: bool,
    pub root_prefix: String,
    pub poll_interval_sec: u32,
    pub device_name: String,
    pub sync_images: bool,
    pub max_blob_mb: u32,
}

/// 默认设备名取 COMPUTERNAME（Windows），取不到时用 "device"。
/// 只作为 peer 快照里的展示名，真正的设备标识是 state.rs 里的 device_id。
fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "device".to_string())
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: SyncProvider::Webdav,
            webdav_url: String::new(),
            username: String::new(),
            s3_endpoint: String::new(),
            s3_region: String::new(),
            s3_bucket: String::new(),
            s3_access_key: String::new(),
            s3_path_style: false,
            root_prefix: "sparrow".to_string(),
            poll_interval_sec: 5,
            device_name: default_device_name(),
            sync_images: true,
            max_blob_mb: 20,
        }
    }
}

/// 上报给前端的同步状态（`sync-status` 事件与 `get_sync_status` 命令共用）。
#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    /// disabled | syncing | idle | error
    pub state: String,
    pub device_id: String,
    /// epoch ms，0 表示从未同步
    pub last_sync_at: i64,
    pub last_error: Option<String>,
    pub pending: u32,
}

/// 快照里的一条记录（= 一条剪贴板条目在远端的形态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRecord {
    /// content_hash（内容寻址，也是去重键）
    pub hash: String,
    /// 0 text / 1 link / 2 image
    pub kind: i32,
    /// text/link 原文；image 为 None
    pub content: Option<String>,
    pub preview: Option<String>,
    /// epoch ms
    pub created_at: i64,
    /// epoch ms（= last_used_at），LWW 的比较依据
    pub updated_at: i64,
    /// 显式删除的墓碑时间戳
    pub deleted_at: Option<i64>,
    /// 图片明文字节数（用于校验与展示）
    pub blob_size: Option<u64>,
}

/// 每设备一份的全量视图，加密后放在 `{root}/devices/{deviceId}.json`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub v: u32,
    pub device_id: String,
    pub device_name: String,
    pub generated_at: i64,
    pub records: Vec<SyncRecord>,
}

/// 敏感凭证。只从 keyring / 环境变量来，绝不写入 settings.json，
/// 因此不实现 Debug（避免被 `{:?}` 打日志）。
#[derive(Clone, Default)]
pub struct SyncSecrets {
    pub webdav_password: String,
    pub s3_secret_key: String,
    pub passphrase: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_yields_defaults() {
        let s: SyncSettings = serde_json::from_str("{}").expect("空对象应能反序列化");
        assert!(!s.enabled);
        assert_eq!(s.provider, SyncProvider::Webdav);
        assert_eq!(s.root_prefix, "sparrow");
        assert_eq!(s.poll_interval_sec, 5);
        assert!(s.sync_images);
        assert_eq!(s.max_blob_mb, 20);
        assert!(!s.s3_path_style);
        assert!(s.webdav_url.is_empty() && s.username.is_empty());
        assert!(s.s3_endpoint.is_empty() && s.s3_bucket.is_empty());
        // 设备名：环境变量取到就用它，取不到必须是 "device"，不能是空串
        assert!(!s.device_name.is_empty());
        assert_eq!(s.device_name, default_device_name());
    }

    #[test]
    fn provider_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SyncProvider::Webdav).unwrap(),
            "\"webdav\""
        );
        assert_eq!(serde_json::to_string(&SyncProvider::S3).unwrap(), "\"s3\"");
        assert_eq!(
            serde_json::from_str::<SyncProvider>("\"s3\"").unwrap(),
            SyncProvider::S3
        );
        // 缺省字段（旧配置）回落默认 provider
        let s: SyncSettings = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(s.enabled);
        assert_eq!(s.provider, SyncProvider::Webdav);
    }
}
