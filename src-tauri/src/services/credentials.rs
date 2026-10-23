//! 同步凭证读取：环境变量优先，其次 Windows 凭据管理器（keyring）。
//!
//! 口令/密钥永不写入 settings.json，也不出现在任何错误消息里——
//! 所有错误只报 kind 与错误类型，绝不回显值。

use serde::Serialize;

use crate::models::sync::SyncSecrets;

/// Windows 凭据管理器里的目标服务名。
pub const SERVICE: &str = "Sparrow.Sync";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    WebdavPassword,
    S3SecretKey,
    Passphrase,
}

impl SecretKind {
    /// 前端 `set_sync_secret(field, value)` 的 `field` → 凭证种类。
    /// 未知字段返回 `None`（命令层据此报错，绝不回退到"猜一个"）。
    pub fn from_field(field: &str) -> Option<Self> {
        match field {
            "webdav_password" => Some(Self::WebdavPassword),
            "s3_secret_key" => Some(Self::S3SecretKey),
            "passphrase" => Some(Self::Passphrase),
            _ => None,
        }
    }

    /// keyring 里的账户名（同一 service 下区分三种凭证）。
    pub fn key(self) -> &'static str {
        match self {
            Self::WebdavPassword => "webdav_password",
            Self::S3SecretKey => "s3_secret_key",
            Self::Passphrase => "passphrase",
        }
    }

    /// 覆盖用的环境变量名（便于脚本/CI 免写凭据管理器）。
    pub fn env_var(self) -> &'static str {
        match self {
            Self::WebdavPassword => "SPARROW_WEBDAV_PASSWORD",
            Self::S3SecretKey => "SPARROW_S3_SECRET_KEY",
            Self::Passphrase => "SPARROW_SYNC_PASSPHRASE",
        }
    }
}

fn entry(kind: SecretKind) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, kind.key())
        .map_err(|e| format!("无法打开凭据管理器（{}）: {e}", kind.key()))
}

/// 读一个凭证：先环境变量（空串视为未设置），再 keyring。
pub fn get_secret(kind: SecretKind) -> Result<Option<String>, String> {
    if let Ok(value) = std::env::var(kind.env_var()) {
        if !value.is_empty() {
            return Ok(Some(value));
        }
    }
    match entry(kind)?.get_password() {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) => Ok(None),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("读取凭据失败（{}）: {e}", kind.key())),
    }
}

/// 写一个凭证；空字符串等于删除。
pub fn set_secret(kind: SecretKind, value: &str) -> Result<(), String> {
    let entry = entry(kind)?;
    if value.is_empty() {
        return match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("删除凭据失败（{}）: {e}", kind.key())),
        };
    }
    entry
        .set_password(value)
        .map_err(|e| format!("保存凭据失败（{}）: {e}", kind.key()))
}

/// 一次性读齐三项，缺省为空串（调用方自行判断缺哪个）。
pub fn load_all() -> Result<SyncSecrets, String> {
    Ok(SyncSecrets {
        webdav_password: get_secret(SecretKind::WebdavPassword)?.unwrap_or_default(),
        s3_secret_key: get_secret(SecretKind::S3SecretKey)?.unwrap_or_default(),
        passphrase: get_secret(SecretKind::Passphrase)?.unwrap_or_default(),
    })
}

/// `has_sync_secrets` 的返回体：哪些凭证已经存好。字段名与前端
/// `types/sync.ts::SyncSecretsPresence` 一致。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SyncSecretsPresence {
    pub webdav_password: bool,
    pub s3_secret_key: bool,
    pub passphrase: bool,
}

/// 探测三项凭证是否已设置。只回答"有没有"，绝不回显值；
/// 读取失败（凭据管理器不可用）按"未设置"处理，不把错误抛给前端。
pub fn presence() -> SyncSecretsPresence {
    let has = |kind: SecretKind| matches!(get_secret(kind), Ok(Some(_)));
    SyncSecretsPresence {
        webdav_password: has(SecretKind::WebdavPassword),
        s3_secret_key: has(SecretKind::S3SecretKey),
        passphrase: has(SecretKind::Passphrase),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_env_names_are_stable() {
        assert_eq!(SecretKind::WebdavPassword.key(), "webdav_password");
        assert_eq!(SecretKind::S3SecretKey.key(), "s3_secret_key");
        assert_eq!(SecretKind::Passphrase.key(), "passphrase");
        assert_eq!(
            SecretKind::WebdavPassword.env_var(),
            "SPARROW_WEBDAV_PASSWORD"
        );
        assert_eq!(SecretKind::S3SecretKey.env_var(), "SPARROW_S3_SECRET_KEY");
        assert_eq!(
            SecretKind::Passphrase.env_var(),
            "SPARROW_SYNC_PASSPHRASE"
        );
        assert_eq!(SERVICE, "Sparrow.Sync");
    }

    /// `set_sync_secret` 的 field → 凭证种类映射：三个合法值 + 非法值拒绝
    #[test]
    fn from_field_maps_known_fields_only() {
        assert_eq!(
            SecretKind::from_field("webdav_password"),
            Some(SecretKind::WebdavPassword)
        );
        assert_eq!(
            SecretKind::from_field("s3_secret_key"),
            Some(SecretKind::S3SecretKey)
        );
        assert_eq!(
            SecretKind::from_field("passphrase"),
            Some(SecretKind::Passphrase)
        );
        assert_eq!(SecretKind::from_field("password"), None);
        assert_eq!(SecretKind::from_field(""), None);
        // 映射结果必须与 keyring 账户名一致（否则写进去读不出来）
        for field in ["webdav_password", "s3_secret_key", "passphrase"] {
            assert_eq!(SecretKind::from_field(field).map(SecretKind::key), Some(field));
        }
    }

    /// 返回体字段名与前端 types/sync.ts 的 SyncSecretsPresence 必须逐字一致
    #[test]
    fn presence_serializes_expected_field_names() {
        let json = serde_json::to_value(SyncSecretsPresence {
            webdav_password: true,
            s3_secret_key: false,
            passphrase: true,
        })
        .expect("序列化");
        assert_eq!(json["webdav_password"], serde_json::json!(true));
        assert_eq!(json["s3_secret_key"], serde_json::json!(false));
        assert_eq!(json["passphrase"], serde_json::json!(true));
        assert_eq!(json.as_object().map(|o| o.len()), Some(3));
    }
}
