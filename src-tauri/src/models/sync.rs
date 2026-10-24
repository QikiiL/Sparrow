use serde::{Deserialize, Serialize};

/// 同步后端类型。序列化为小写字符串，前端 `types/sync.ts` 与此保持一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncProvider {
    #[default]
    Webdav,
    S3,
}

/// 同步方式。
///
/// - `Interval`（默认）：按 `poll_interval_sec` 轮询，本机复制/删除事件立即唤醒。
/// - `Startup`：进程启动后跑一轮，之后**不设定时器**；`clipboard-changed` /
///   `item-deleted` 不唤醒，只有手动「立即同步」、保存设置、切换模式才跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    #[default]
    Interval,
    Startup,
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
    /// 同步方式（轮询 / 仅启动时）。旧配置缺该字段时回落 `interval`。
    #[serde(default)]
    pub sync_mode: SyncMode,
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
            sync_mode: SyncMode::Interval,
            device_name: default_device_name(),
            sync_images: true,
            max_blob_mb: 20,
        }
    }
}

/// 图片 blob 的大小上限（字节）。
///
/// `max_blob_mb` 夹到 1–512 MB：0 会让每一张图片都被判"超限"而静默不同步，
/// 过大的值则可能把内存与带宽打满。快照构建、blob 上传、blob 下载三处必须
/// 用同一个口径，否则会出现"发得出去、收不回来"的不对称。
pub fn max_blob_bytes(settings: &SyncSettings) -> u64 {
    u64::from(settings.max_blob_mb.clamp(1, 512)) * 1024 * 1024
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
    /// 用户可见的告警（如"3 张图片未同步"）。`#[serde(default)]` 保证旧前端
    /// 或旧版本状态载荷缺字段时仍能解析。
    #[serde(default)]
    pub warning: Option<String>,
    /// 本轮被跳过的图片数（与 `warning` 成对出现）。
    #[serde(default)]
    pub skipped: u32,
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
    /// CF_HTML 原始字节的 sha256 hex。`None` = 该条目没有 HTML 副本
    /// （或本地文件缺失/超限，发送端已计入 `skipped_formats`）。
    /// 内容寻址：远端对象是 `blobs/{html_blob}.bin`，与 record.hash 无关。
    #[serde(default)]
    pub html_blob: Option<String>,
    /// CF_HTML 明文字节数（接收端落盘后据此校验，展示用）。
    #[serde(default)]
    pub html_size: Option<u64>,
    /// RTF 原始字节的 sha256 hex（语义同 `html_blob`）。
    #[serde(default)]
    pub rtf_blob: Option<String>,
    /// RTF 明文字节数。
    #[serde(default)]
    pub rtf_size: Option<u64>,
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
        assert_eq!(s.sync_mode, SyncMode::Interval, "旧配置默认按间隔轮询");
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

    /// `max_blob_mb = 0` 会让每一张图都被判超限（静默不同步）：必须按 1MB 兜底。
    #[test]
    fn max_blob_mb_zero_is_clamped_to_one() {
        let mb = 1024 * 1024;
        let zero = SyncSettings {
            max_blob_mb: 0,
            ..Default::default()
        };
        assert_eq!(max_blob_bytes(&zero), mb, "0 必须夹到 1MB");

        let normal = SyncSettings {
            max_blob_mb: 20,
            ..Default::default()
        };
        assert_eq!(max_blob_bytes(&normal), 20 * mb);

        let huge = SyncSettings {
            max_blob_mb: 100_000,
            ..Default::default()
        };
        assert_eq!(max_blob_bytes(&huge), 512 * mb, "上限夹到 512MB");
    }

    /// 状态里的 `warning` / `skipped` 必须出现在序列化结果里（前端据此提示），
    /// 缺省时是 `null` / `0`。
    #[test]
    fn sync_status_serializes_warning_and_skipped() {
        let status = SyncStatus {
            state: "idle".to_string(),
            device_id: "dev-1".to_string(),
            last_sync_at: 0,
            last_error: None,
            pending: 0,
            warning: None,
            skipped: 0,
        };
        let value = serde_json::to_value(&status).expect("序列化状态");
        assert!(value.get("warning").is_some(), "必须含 warning 字段");
        assert!(value["warning"].is_null(), "默认无告警应为 null");
        assert_eq!(value["skipped"], 0);

        let warned = SyncStatus {
            warning: Some("3 张图片未同步".to_string()),
            skipped: 3,
            ..status
        };
        let value = serde_json::to_value(&warned).expect("序列化状态");
        assert_eq!(value["warning"], "3 张图片未同步");
        assert_eq!(value["skipped"], 3);
    }

    /// 同步方式：缺字段 → interval；显式 `"startup"` → Startup；序列化小写。
    #[test]
    fn sync_mode_round_trips_lowercase() {
        // 缺字段（旧 settings.json）回落默认值，不能反序列化失败
        let s: SyncSettings = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(s.sync_mode, SyncMode::Interval);

        let s: SyncSettings = serde_json::from_str(r#"{"sync_mode":"startup"}"#).unwrap();
        assert_eq!(s.sync_mode, SyncMode::Startup);

        let s: SyncSettings = serde_json::from_str(r#"{"sync_mode":"interval"}"#).unwrap();
        assert_eq!(s.sync_mode, SyncMode::Interval);

        // 序列化必须是小写字符串，前端 `types/sync.ts` 与之同形
        assert_eq!(
            serde_json::to_string(&SyncMode::Interval).unwrap(),
            "\"interval\""
        );
        assert_eq!(
            serde_json::to_string(&SyncMode::Startup).unwrap(),
            "\"startup\""
        );

        let value = serde_json::to_value(SyncSettings::default()).expect("序列化设置");
        assert_eq!(value["sync_mode"], "interval", "默认值必须显式落盘");
    }
}
