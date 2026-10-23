/**
 * 云同步的前端类型(与 Rust `models/sync.rs` 保持同形)。
 *
 * 注意:口令 / WebDAV 密码 / S3 Secret Key 都**不在**这里 —— 它们走
 * `set_sync_secret` 写进 Windows 凭据管理器,绝不进 settings 文件。
 */

export type SyncProvider = 'webdav' | 's3';

export interface SyncSettings {
  enabled: boolean;
  provider: SyncProvider;
  webdav_url: string;
  username: string;
  s3_endpoint: string;
  s3_region: string;
  s3_bucket: string;
  s3_access_key: string;
  s3_path_style: boolean;
  root_prefix: string;
  poll_interval_sec: number;
  device_name: string;
  sync_images: boolean;
  max_blob_mb: number;
}

export interface SyncStatus {
  /** disabled 未启用 / syncing 同步中 / idle 空闲(上次成功) / error 失败 */
  state: 'disabled' | 'syncing' | 'idle' | 'error';
  device_id: string;
  /** epoch ms;0 表示从未同步 */
  last_sync_at: number;
  last_error: string | null;
  pending: number;
}

/** `has_sync_secrets` 的返回:哪些凭证已经存进凭据管理器 */
export interface SyncSecretsPresence {
  webdav_password: boolean;
  s3_secret_key: boolean;
  passphrase: boolean;
}

/** `set_sync_secret` 的 field 取值 */
export type SyncSecretField = 'webdav_password' | 's3_secret_key' | 'passphrase';
