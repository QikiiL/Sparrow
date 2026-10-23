//! AWS Signature Version 4（纯函数，无网络、无 chrono）。
//!
//! 只覆盖本项目的需求：S3 对象读写 + ListObjectsV2，固定签
//! `host` / `x-amz-content-sha256` / `x-amz-date` 三个头。
//! 日期用 civil-from-days 算法自己算，避免为格式化时间引 chrono。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use url::Url;

type HmacSha256 = Hmac<Sha256>;

/// 固定参与签名的头（顺序即 `SignedHeaders`，必须是字典序）。
pub const SIGNED_HEADERS: &str = "host;x-amz-content-sha256;x-amz-date";

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// 签名密钥派生：`AWS4+secret` → date → region → service → `aws4_request`。
pub fn signing_key(secret_key: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date_stamp);
    let k_region = hmac_sha256(&k_date, region);
    let k_service = hmac_sha256(&k_region, service);
    hmac_sha256(&k_service, "aws4_request")
}

/// epoch 秒 → `YYYYMMDDTHHMMSSZ`（UTC）。
pub fn format_amz_date(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Howard Hinnant 的 `civil_from_days`：1970-01-01 起的天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// RFC 3986 非保留字符之外一律 `%XX`（大写十六进制）。S3 的 `/` 按段保留。
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// CanonicalURI：逐段编码后重新用 `/` 拼回去（空 path 视为 `/`）。
fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// CanonicalQueryString：先解码再编码，按编码后的 (key, value) 字典序排序。
fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (
                encode_segment(&super::percent_decode(k)),
                encode_segment(&super::percent_decode(v)),
            )
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Host 头（含非默认端口）。
fn host_header(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        (None, _) => String::new(),
    }
}

/// CanonicalRequest 原文（测试直接断言这个字符串）。
pub fn canonical_request(method: &str, url: &Url, payload_hash: &str, amz_date: &str) -> String {
    let headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host_header(url),
        payload_hash,
        amz_date
    );
    format!(
        "{method}\n{}\n{}\n{headers}\n{SIGNED_HEADERS}\n{payload_hash}",
        canonical_uri(url.path()),
        canonical_query(url.query().unwrap_or(""))
    )
}

/// 生成 `Authorization` 头与 `x-amz-date` 值（后者原样返回，便于调用方直接设置）。
pub fn sign(
    method: &str,
    url: &Url,
    region: &str,
    access_key: &str,
    secret_key: &str,
    payload_hash: &str,
    amz_date: &str,
) -> (String, String) {
    let date_stamp = amz_date.get(..8).unwrap_or(amz_date);
    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let canonical = canonical_request(method, url, payload_hash, amz_date);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let key = signing_key(secret_key, date_stamp, region, "s3");
    let signature = hex(&hmac_sha256(&key, &string_to_sign));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, \
         SignedHeaders={SIGNED_HEADERS}, Signature={signature}"
    );
    (authorization, amz_date.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS 官方文档的签名密钥派生向量（date 20120215 / us-east-1 / iam）。
    #[test]
    fn signing_key_matches_aws_vector() {
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn format_amz_date_is_utc_compact() {
        assert_eq!(format_amz_date(0), "19700101T000000Z");
        assert_eq!(format_amz_date(1369353600), "20130524T000000Z");
        // 闰日 + 非整点，防止只测到整天边界
        assert_eq!(format_amz_date(951_827_696), "20000229T123456Z");
    }

    #[test]
    fn canonical_request_is_stable() {
        let url =
            Url::parse("https://s3.us-east-1.amazonaws.com/sparrow-sync/sparrow/devices/abc.json")
                .unwrap();
        let payload_hash = sha256_hex(b"");
        assert_eq!(
            payload_hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let canonical = canonical_request("PUT", &url, &payload_hash, "20130524T000000Z");
        assert_eq!(
            canonical,
            "PUT\n\
             /sparrow-sync/sparrow/devices/abc.json\n\
             \n\
             host:s3.us-east-1.amazonaws.com\n\
             x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
             x-amz-date:20130524T000000Z\n\
             \n\
             host;x-amz-content-sha256;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        let url = Url::parse(
            "https://b.s3.example.com/?prefix=sparrow%2Fdevices%2F&list-type=2&continuation-token=a%2Bb",
        )
        .unwrap();
        assert_eq!(
            canonical_query(url.query().unwrap()),
            "continuation-token=a%2Bb&list-type=2&prefix=sparrow%2Fdevices%2F"
        );
        // 空 query 保持空串，不能变成 "="
        assert_eq!(canonical_query(""), "");
    }

    #[test]
    fn authorization_has_expected_shape() {
        let url = Url::parse("https://s3.example.com:9000/sparrow-sync/sparrow/devices/abc.json")
            .unwrap();
        let payload_hash = sha256_hex(b"body");
        let (auth, amz_date) = sign(
            "PUT",
            &url,
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "secret",
            &payload_hash,
            "20130524T000000Z",
        );
        assert_eq!(amz_date, "20130524T000000Z");
        let prefix = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=";
        assert!(auth.starts_with(prefix), "{auth}");
        let signature = &auth[prefix.len()..];
        assert_eq!(signature.len(), 64, "签名必须是 64 位 hex: {auth}");
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()), "{auth}");
        // 端口必须进入 host 头（canonical request 里可见）
        let canonical = canonical_request("PUT", &url, &payload_hash, "20130524T000000Z");
        assert!(
            canonical.contains("host:s3.example.com:9000\n"),
            "{canonical}"
        );
    }
}
