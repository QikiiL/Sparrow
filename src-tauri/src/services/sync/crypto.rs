//! 加密层：meta.json 引导、Argon2id 密钥派生、AES-256-GCM 信封。
//!
//! 远端只有密文：
//! - 快照/JSON：`{"v":1,"alg":"A256GCM","nonce":"<b64>","ct":"<b64>"}`
//! - 图片 blob：`b"SPRW1" + nonce(12) + ciphertext`
//!
//! AAD 一律传对象 key，防止把 A 对象的密文挪到 B 对象上（跨对象替换攻击）。
//!
//! meta.json 里的 `check` 是"密钥校验值"：`HMAC-SHA256(key, CHECK_DOMAIN)`。
//! 它的作用是把"口令错了"从"某个 peer 文件解不开、跳过就好"变成**整轮显式失败**，
//! 否则错口令期间本机会用错 key 覆盖自己的快照，改回正确口令后 digest 又没变、
//! 不再重传，远端就永久留下一份谁也解不开的密文。

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::backend::SyncBackend;
use super::{SyncError, SyncErrorKind};

type HmacSha256 = Hmac<Sha256>;

/// 明文 KDF 参数文件，位于 `{root}/meta.json`。
///
/// **这是 root 相对的 key**：后端的 `url()` 自己会拼上 `{root}/`，
/// 传进来的 key 若再带 root 就会落到 `{root}/{root}/meta.json`。
pub const META_FILE: &str = "meta.json";

/// 默认 Argon2id 参数：64 MiB / 3 轮 / 1 并行度（桌面端约 0.1–0.3s）。
pub const DEFAULT_M: u32 = 65536;
pub const DEFAULT_T: u32 = 3;
pub const DEFAULT_P: u32 = 1;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const KDF_ARGON2ID: &str = "argon2id";
const ENVELOPE_V: u32 = 1;
const ENVELOPE_ALG: &str = "A256GCM";
const BLOB_MAGIC: &[u8; 5] = b"SPRW1";
/// GCM tag 长度，用于最短长度校验。
const TAG_LEN: usize = 16;
/// 密钥校验值的 HMAC 域分隔串：`check = hex(HMAC-SHA256(key, CHECK_DOMAIN))`。
const CHECK_DOMAIN: &[u8] = b"sparrow-sync-meta-check-v1";
/// 口令与远端不一致时唯一的错误文案（不含任何口令/密钥/URL 片段）。
pub const AUTH_MISMATCH: &str = "同步口令与远端不一致";

/// meta.json 内容。明文存储，只泄露 KDF 参数、salt 与密钥校验值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncMeta {
    pub v: u32,
    pub kdf: String,
    pub m: u32,
    pub t: u32,
    pub p: u32,
    /// base64(16B 随机 salt)
    pub salt: String,
    /// hex(HMAC-SHA256(key, CHECK_DOMAIN))：用口令派生出的 key 自证。
    /// `#[serde(default)]` 让缺字段的旧 meta.json 解析成空串，随后被
    /// [`verify_key_check`] 判为口令不一致（宁可显式失败，也不要静默跳过）。
    #[serde(default)]
    pub check: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    v: u32,
    alg: String,
    nonce: String,
    ct: String,
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// 密钥校验值：`hex(HMAC-SHA256(key, CHECK_DOMAIN))`，写进 meta.json 的 `check`。
pub fn key_check(key: &[u8; KEY_LEN]) -> String {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(CHECK_DOMAIN);
    hex_lower(&mac.finalize().into_bytes())
}

/// 校验口令派生出的 key 与远端 meta.json 的 `check` 是否一致。
///
/// `check` 缺失（旧版本写的 meta.json）或不匹配都返回 [`SyncErrorKind::Auth`]：
/// 错口令必须整轮失败，不能靠"跳过解不开的 peer"混过去。
pub fn verify_key_check(meta: &SyncMeta, key: &[u8; KEY_LEN]) -> Result<(), SyncError> {
    let Some(expected) = hex_decode(meta.check.trim()).filter(|bytes| !bytes.is_empty()) else {
        return Err(SyncError::auth(AUTH_MISMATCH));
    };
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(CHECK_DOMAIN);
    // `verify_slice` 是常量时间比较，不会因为前缀匹配长度泄漏信息
    mac.verify_slice(&expected)
        .map_err(|_| SyncError::auth(AUTH_MISMATCH))
}

/// 密钥缓存指纹：`sha256(passphrase + salt)` 的十六进制。
///
/// 只用于"这一轮要不要重跑 Argon2id"的判断，不参与任何加密。
pub fn key_fingerprint(passphrase: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(passphrase.as_bytes());
    hasher.update(salt.as_bytes());
    hex_lower(&hasher.finalize())
}

/// 派生密钥缓存：口令与 salt 都没变时直接复用 `key`，跳过 0.1–0.3s 的 Argon2id。
#[derive(Clone)]
pub struct KeyCache {
    pub fingerprint: String,
    pub key: [u8; KEY_LEN],
}

fn parse_meta(bytes: &[u8]) -> Result<SyncMeta, SyncError> {
    serde_json::from_slice(bytes)
        .map_err(|_| SyncError::config("meta.json 解析失败（远端文件损坏）"))
}

/// 引导 meta.json 并返回 `(meta, 校验通过的密钥)`。
///
/// - `meta.json` 存在 → 派生 key → [`verify_key_check`]：`check` 缺失或不匹配
///   返回 [`SyncErrorKind::Auth`]（口令错，重试无意义，也绝不能拿错 key 去覆盖远端）；
/// - 不存在 → 生成 16B salt，派生 key，算出 `check`，用 `If-None-Match: *` 创建；
///   409/412 说明别的设备抢先创建了，重新 GET 并以远端为准校验（否则两台设备会
///   用不同 salt 派生出不同密钥，互相解不开对方的快照）。
///
/// 注意 key 是 **root 相对**的 `meta.json`：后端 `url()` 自己会拼 `{root}/`。
pub fn load_or_init_meta(
    backend: &dyn SyncBackend,
    passphrase: &str,
) -> Result<(SyncMeta, [u8; KEY_LEN]), SyncError> {
    load_or_init_meta_cached(backend, passphrase, None)
}

/// [`load_or_init_meta`] 的带缓存版本：指纹命中时跳过 Argon2id，但**仍然**校验 `check`。
pub fn load_or_init_meta_cached(
    backend: &dyn SyncBackend,
    passphrase: &str,
    cached: Option<&KeyCache>,
) -> Result<(SyncMeta, [u8; KEY_LEN]), SyncError> {
    if let Some(bytes) = backend.get(META_FILE)? {
        let meta = parse_meta(&bytes)?;
        let fingerprint = key_fingerprint(passphrase, &meta.salt);
        let key = match cached {
            Some(cache) if cache.fingerprint == fingerprint => cache.key,
            _ => derive_key(passphrase, &meta)?,
        };
        verify_key_check(&meta, &key)?;
        return Ok((meta, key));
    }

    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let mut meta = SyncMeta {
        v: ENVELOPE_V,
        kdf: KDF_ARGON2ID.to_string(),
        m: DEFAULT_M,
        t: DEFAULT_T,
        p: DEFAULT_P,
        salt: B64.encode(salt),
        check: String::new(),
    };
    let key = derive_key(passphrase, &meta)?;
    meta.check = key_check(&key);
    let body = serde_json::to_vec(&meta).map_err(|_| SyncError::config("meta.json 序列化失败"))?;

    match backend.put(META_FILE, &body, Some("*")) {
        Ok(_) => Ok((meta, key)),
        Err(e) if e.kind == SyncErrorKind::Conflict => {
            let Some(bytes) = backend.get(META_FILE)? else {
                return Err(SyncError::server("meta.json 创建冲突但远端对象不存在"));
            };
            let remote = parse_meta(&bytes)?;
            let key = derive_key(passphrase, &remote)?;
            verify_key_check(&remote, &key)?;
            Ok((remote, key))
        }
        Err(e) => Err(e),
    }
}

/// 口令 → 32B 密钥。参数来自 meta.json，两台设备必须得到同一把密钥。
pub fn derive_key(passphrase: &str, meta: &SyncMeta) -> Result<[u8; KEY_LEN], SyncError> {
    if meta.kdf != KDF_ARGON2ID {
        return Err(SyncError::config(format!("不支持的 KDF: {}", meta.kdf)));
    }
    let salt = B64
        .decode(meta.salt.as_bytes())
        .map_err(|_| SyncError::config("meta.json 的 salt 不是合法 base64"))?;
    if salt.len() < 8 {
        return Err(SyncError::config("meta.json 的 salt 过短"));
    }
    let params = Params::new(meta.m, meta.t, meta.p, Some(KEY_LEN)).map_err(|_| {
        SyncError::config(format!(
            "Argon2 参数非法: m={} t={} p={}",
            meta.m, meta.t, meta.p
        ))
    })?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase.as_bytes(), &salt, &mut out)
        .map_err(|_| SyncError::config("密钥派生失败"))?;
    Ok(out)
}

fn cipher(key: &[u8; KEY_LEN]) -> Result<Aes256Gcm, SyncError> {
    Aes256Gcm::new_from_slice(key).map_err(|_| SyncError::config("密钥长度非法"))
}

fn seal_raw(
    key: &[u8; KEY_LEN],
    aad: &str,
    plaintext: &[u8],
) -> Result<([u8; NONCE_LEN], Vec<u8>), SyncError> {
    let cipher = cipher(key)?;
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| SyncError::server("加密失败"))?;
    Ok((nonce, ct))
}

fn open_raw(key: &[u8; KEY_LEN], aad: &str, nonce: &[u8], ct: &[u8]) -> Result<Vec<u8>, SyncError> {
    if nonce.len() != NONCE_LEN {
        return Err(SyncError::config("nonce 长度非法"));
    }
    cipher(key)?
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| SyncError::auth("解密失败：口令错误、AAD 不匹配或密文被篡改"))
}

/// JSON 信封加密。`aad` 传对象 key。
pub fn seal_json<T: Serialize>(
    key: &[u8; KEY_LEN],
    aad: &str,
    v: &T,
) -> Result<Vec<u8>, SyncError> {
    let plaintext = serde_json::to_vec(v).map_err(|_| SyncError::config("快照序列化失败"))?;
    let (nonce, ct) = seal_raw(key, aad, &plaintext)?;
    let envelope = Envelope {
        v: ENVELOPE_V,
        alg: ENVELOPE_ALG.to_string(),
        nonce: B64.encode(nonce),
        ct: B64.encode(ct),
    };
    serde_json::to_vec(&envelope).map_err(|_| SyncError::config("信封序列化失败"))
}

/// JSON 信封解密。
pub fn open_json<T: DeserializeOwned>(
    key: &[u8; KEY_LEN],
    aad: &str,
    bytes: &[u8],
) -> Result<T, SyncError> {
    let envelope: Envelope = serde_json::from_slice(bytes)
        .map_err(|_| SyncError::config("信封解析失败（远端文件损坏）"))?;
    if envelope.v != ENVELOPE_V || envelope.alg != ENVELOPE_ALG {
        return Err(SyncError::config(format!(
            "不支持的信封: v={} alg={}",
            envelope.v, envelope.alg
        )));
    }
    let nonce = B64
        .decode(envelope.nonce.as_bytes())
        .map_err(|_| SyncError::config("信封 nonce 不是合法 base64"))?;
    let ct = B64
        .decode(envelope.ct.as_bytes())
        .map_err(|_| SyncError::config("信封 ct 不是合法 base64"))?;
    let plaintext = open_raw(key, aad, &nonce, &ct)?;
    serde_json::from_slice(&plaintext).map_err(|_| SyncError::config("明文解析失败"))
}

/// 二进制信封加密（图片 blob）：`SPRW1` + nonce(12) + ciphertext。
pub fn seal_blob(key: &[u8; KEY_LEN], aad: &str, bytes: &[u8]) -> Result<Vec<u8>, SyncError> {
    let (nonce, ct) = seal_raw(key, aad, bytes)?;
    let mut out = Vec::with_capacity(BLOB_MAGIC.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(BLOB_MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// 二进制信封解密。
pub fn open_blob(key: &[u8; KEY_LEN], aad: &str, bytes: &[u8]) -> Result<Vec<u8>, SyncError> {
    if bytes.len() < BLOB_MAGIC.len() + NONCE_LEN + TAG_LEN {
        return Err(SyncError::config("blob 长度不足"));
    }
    if &bytes[..BLOB_MAGIC.len()] != BLOB_MAGIC {
        return Err(SyncError::config("blob 魔数不匹配"));
    }
    let nonce = &bytes[BLOB_MAGIC.len()..BLOB_MAGIC.len() + NONCE_LEN];
    let ct = &bytes[BLOB_MAGIC.len() + NONCE_LEN..];
    open_raw(key, aad, nonce, ct)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::super::backend::ObjectMeta;
    use super::*;

    /// 测试专用固定密钥，不依赖后端与 keyring。
    const KEY: [u8; 32] = [7u8; 32];

    /// 内存后端，只够 meta.json 引导用（task 2 会有完整 mock）。
    #[derive(Default)]
    struct MockBackend {
        objects: Mutex<HashMap<String, Vec<u8>>>,
        /// 模拟"别的设备抢先创建"：下次 PUT 时把这份远端对象塞进去并返回 412
        pending_remote: Mutex<Option<(String, Vec<u8>)>>,
        last_if_match: Mutex<Option<String>>,
        /// 最后一次 PUT 用的 key：用来断言 meta 的 key 是 root 相对的
        last_put_key: Mutex<Option<String>>,
    }

    impl SyncBackend for MockBackend {
        fn put(
            &self,
            key: &str,
            body: &[u8],
            if_match: Option<&str>,
        ) -> Result<Option<String>, SyncError> {
            *self.last_if_match.lock().unwrap() = if_match.map(str::to_string);
            *self.last_put_key.lock().unwrap() = Some(key.to_string());
            if let Some((k, v)) = self.pending_remote.lock().unwrap().take() {
                self.objects.lock().unwrap().insert(k, v);
                return Err(SyncError::conflict("412 Precondition Failed"));
            }
            self.objects
                .lock()
                .unwrap()
                .insert(key.to_string(), body.to_vec());
            Ok(Some("\"etag-1\"".to_string()))
        }

        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SyncError> {
            Ok(self.objects.lock().unwrap().get(key).cloned())
        }

        fn head(&self, key: &str) -> Result<Option<ObjectMeta>, SyncError> {
            Ok(self.objects.lock().unwrap().get(key).map(|v| ObjectMeta {
                key: key.to_string(),
                etag: Some("\"etag-1\"".to_string()),
                size: v.len() as u64,
            }))
        }

        fn delete(&self, key: &str) -> Result<(), SyncError> {
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }

        fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, SyncError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| ObjectMeta {
                    key: k.clone(),
                    etag: None,
                    size: v.len() as u64,
                })
                .collect())
        }
    }

    /// 预置一份远端 meta.json（小参数：Argon2 从 ~300ms 降到 ~5ms）。
    fn seed_meta(backend: &MockBackend, meta: &SyncMeta) {
        backend
            .objects
            .lock()
            .unwrap()
            .insert(META_FILE.to_string(), serde_json::to_vec(meta).unwrap());
    }

    #[test]
    fn meta_is_created_once_with_conditional_put() {
        let backend = MockBackend::default();
        let (meta, key) = load_or_init_meta(&backend, "correct horse").unwrap();
        assert_eq!(meta.kdf, KDF_ARGON2ID);
        assert_eq!((meta.m, meta.t, meta.p), (DEFAULT_M, DEFAULT_T, DEFAULT_P));
        assert_eq!(B64.decode(&meta.salt).unwrap().len(), SALT_LEN);
        assert_eq!(meta.check, key_check(&key), "meta.check 必须由派生 key 算出");
        assert_eq!(
            backend.last_if_match.lock().unwrap().as_deref(),
            Some("*"),
            "首建必须用 If-None-Match: *"
        );
        assert_eq!(
            backend.last_put_key.lock().unwrap().as_deref(),
            Some(META_FILE),
            "meta 的 key 必须是 root 相对的 meta.json（后端自己拼 {{root}}/），\
             否则会落到 {{root}}/{{root}}/meta.json"
        );

        // 第二次（同口令）必须复用远端那份，salt 与 key 都不变
        let (again, key2) = load_or_init_meta(&backend, "correct horse").unwrap();
        assert_eq!(again, meta, "重复引导必须复用同一 salt");
        assert_eq!(key2, key);
    }

    #[test]
    fn meta_conflict_rereads_remote() {
        let backend = MockBackend::default();
        // 别的设备已经用不同 salt 建好了 meta.json
        let remote = test_meta_with_check(8192, 1, 1, 0xAB, "correct horse");
        *backend.pending_remote.lock().unwrap() =
            Some((META_FILE.to_string(), serde_json::to_vec(&remote).unwrap()));

        let (meta, key) = load_or_init_meta(&backend, "correct horse").unwrap();
        assert_eq!(meta, remote, "409/412 后必须以远端为准，否则两设备密钥不同");
        assert_eq!(key, derive_key("correct horse", &remote).unwrap());
    }

    #[test]
    fn same_passphrase_passes_key_check() {
        let backend = MockBackend::default();
        let remote = test_meta_with_check(8192, 1, 1, 0x11, "correct horse");
        seed_meta(&backend, &remote);

        let (meta, key) = load_or_init_meta(&backend, "correct horse").unwrap();
        assert_eq!(meta, remote);
        assert!(verify_key_check(&meta, &key).is_ok());
    }

    #[test]
    fn wrong_passphrase_is_auth_error() {
        let backend = MockBackend::default();
        seed_meta(&backend, &test_meta_with_check(8192, 1, 1, 0x11, "correct horse"));

        let err = load_or_init_meta(&backend, "wrong horse").unwrap_err();
        assert_eq!(err.kind, SyncErrorKind::Auth, "错口令必须显式失败");
        assert_eq!(err.message, AUTH_MISMATCH);
        assert!(
            !err.message.contains("wrong horse"),
            "错误消息不得回显口令: {}",
            err.message
        );
    }

    #[test]
    fn missing_check_is_auth_error() {
        let backend = MockBackend::default();
        // 旧版本写的 meta.json 没有 check 字段
        seed_meta(&backend, &test_meta(8192, 1, 1, 0x11));

        let err = load_or_init_meta(&backend, "correct horse").unwrap_err();
        assert_eq!(err.kind, SyncErrorKind::Auth, "check 缺失不能静默通过");
        assert_eq!(err.message, AUTH_MISMATCH);
    }

    #[test]
    fn tampered_check_is_auth_error() {
        let backend = MockBackend::default();
        let mut remote = test_meta_with_check(8192, 1, 1, 0x11, "correct horse");
        // 翻掉 check 的第一个 hex 字符（口令正确，但远端校验值被篡改）
        let flipped = if remote.check.starts_with('0') { "1" } else { "0" };
        remote.check = format!("{flipped}{}", &remote.check[1..]);
        seed_meta(&backend, &remote);

        let err = load_or_init_meta(&backend, "correct horse").unwrap_err();
        assert_eq!(err.kind, SyncErrorKind::Auth);
        assert_eq!(err.message, AUTH_MISMATCH);
    }

    #[test]
    fn key_fingerprint_binds_passphrase_and_salt() {
        let base = key_fingerprint("pw", "salt");
        assert_eq!(base.len(), 64, "sha256 十六进制应为 64 字符");
        assert_eq!(base, key_fingerprint("pw", "salt"), "同输入必须确定性");
        assert_ne!(base, key_fingerprint("pw2", "salt"));
        assert_ne!(base, key_fingerprint("pw", "salt2"));
    }

    fn test_meta(m: u32, t: u32, p: u32, salt_byte: u8) -> SyncMeta {
        SyncMeta {
            v: 1,
            kdf: KDF_ARGON2ID.to_string(),
            m,
            t,
            p,
            salt: B64.encode([salt_byte; SALT_LEN]),
            check: String::new(),
        }
    }

    /// 带正确 `check` 的远端 meta（模拟"口令一致的正常远端"）。
    fn test_meta_with_check(
        m: u32,
        t: u32,
        p: u32,
        salt_byte: u8,
        passphrase: &str,
    ) -> SyncMeta {
        let mut meta = test_meta(m, t, p, salt_byte);
        let key = derive_key(passphrase, &meta).expect("派生 key");
        meta.check = key_check(&key);
        meta
    }

    #[test]
    fn json_roundtrip() {
        let value =
            serde_json::json!({"device_id": "dev-1", "records": [{"hash": "ab", "kind": 0}]});
        let sealed = seal_json(&KEY, "sparrow/devices/dev-1.json", &value).unwrap();
        let opened: serde_json::Value =
            open_json(&KEY, "sparrow/devices/dev-1.json", &sealed).unwrap();
        assert_eq!(opened, value);
    }

    #[test]
    fn wrong_key_fails() {
        let sealed = seal_json(&KEY, "aad", &serde_json::json!({"x": 1})).unwrap();
        let other = [9u8; 32];
        assert!(open_json::<serde_json::Value>(&other, "aad", &sealed).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let sealed = seal_json(&KEY, "aad", &serde_json::json!({"x": 1})).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_slice(&sealed).unwrap();
        let mut ct = B64
            .decode(envelope["ct"].as_str().unwrap())
            .expect("ct 是 base64");
        let last = ct.len() - 1;
        ct[last] ^= 0x01; // 翻 1 bit（必然破坏 GCM tag 校验）
        envelope["ct"] = serde_json::Value::String(B64.encode(ct));
        let tampered = serde_json::to_vec(&envelope).unwrap();
        assert!(open_json::<serde_json::Value>(&KEY, "aad", &tampered).is_err());
    }

    #[test]
    fn aad_mismatch_fails() {
        let sealed =
            seal_json(&KEY, "sparrow/devices/a.json", &serde_json::json!({"x": 1})).unwrap();
        let err =
            open_json::<serde_json::Value>(&KEY, "sparrow/devices/b.json", &sealed).unwrap_err();
        assert_eq!(err.kind, SyncErrorKind::Auth);
    }

    #[test]
    fn envelope_has_no_plaintext() {
        let secret = "hunter2-plaintext-marker";
        let sealed = seal_json(&KEY, "aad", &serde_json::json!({ "content": secret })).unwrap();
        let text = String::from_utf8_lossy(&sealed);
        assert!(!text.contains(secret), "信封泄漏明文: {text}");
        assert!(!text.contains("content"), "信封泄漏字段名: {text}");
        assert!(text.contains("\"alg\":\"A256GCM\""));
    }

    #[test]
    fn blob_roundtrip_and_tamper() {
        let plain = vec![0x89u8, 0x50, 0x4e, 0x47, 1, 2, 3, 4, 5];
        let sealed = seal_blob(&KEY, "sparrow/blobs/abc.bin", &plain).unwrap();
        assert_eq!(&sealed[..5], BLOB_MAGIC);
        assert_eq!(
            open_blob(&KEY, "sparrow/blobs/abc.bin", &sealed).unwrap(),
            plain
        );
        // 篡改密文最后一个字节
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0x01;
        assert!(open_blob(&KEY, "sparrow/blobs/abc.bin", &bad).is_err());
        // AAD 不匹配
        assert!(open_blob(&KEY, "sparrow/blobs/xyz.bin", &sealed).is_err());
        // 明文里不应出现原始字节序列
        assert!(!sealed.windows(plain.len()).any(|w| w == plain.as_slice()));
    }

    #[test]
    fn argon2_is_deterministic_per_salt() {
        // 小参数加速：m=8192, t=1, p=1
        let meta_a = test_meta(8192, 1, 1, 0x11);
        let k1 = derive_key("correct horse", &meta_a).unwrap();
        let k2 = derive_key("correct horse", &meta_a).unwrap();
        assert_eq!(k1, k2, "同 salt 两次派生必须一致");

        let meta_b = test_meta(8192, 1, 1, 0x22);
        let k3 = derive_key("correct horse", &meta_b).unwrap();
        assert_ne!(k1, k3, "不同 salt 必须派生不同密钥");

        let k4 = derive_key("wrong horse", &meta_a).unwrap();
        assert_ne!(k1, k4, "不同口令必须派生不同密钥");
    }

    #[test]
    fn derive_key_rejects_bad_meta() {
        let mut bad_kdf = test_meta(8192, 1, 1, 0x11);
        bad_kdf.kdf = "pbkdf2".to_string();
        assert_eq!(
            derive_key("x", &bad_kdf).unwrap_err().kind,
            SyncErrorKind::Config
        );

        let mut bad_salt = test_meta(8192, 1, 1, 0x11);
        bad_salt.salt = "not base64!!".to_string();
        assert_eq!(
            derive_key("x", &bad_salt).unwrap_err().kind,
            SyncErrorKind::Config
        );

        // p=0 非法 → Params::new 报错 → Config
        let bad_params = test_meta(8192, 1, 0, 0x11);
        assert_eq!(
            derive_key("x", &bad_params).unwrap_err().kind,
            SyncErrorKind::Config
        );
    }
}
