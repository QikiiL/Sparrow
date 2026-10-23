//! S3 兼容后端：SigV4 签名、ListObjectsV2 分页、path-style / virtual-host 寻址。
//!
//! 远端布局：`{root_prefix}/{key}`，`devices/` 存设备快照、`blobs/` 存图片密文
//! （S3 没有目录，`ensure_dirs` 用默认空实现）。
//!
//! 条件写与错误映射见 [`super::backend`]；日志纪律见 [`super`]（错误消息只带
//! `key_tag` 与状态码，绝不带 URL / Authorization / body）。

use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use quick_xml::events::Event;
use quick_xml::Reader;
use url::Url;

use crate::models::sync::{SyncSecrets, SyncSettings};

use super::backend::{ObjectMeta, SyncBackend};
use super::sigv4;
use super::{
    build_agent, check_scheme, key_tag, object_path, push_general_ref, reject_redirect, root_of,
    transport_error, validate_key, SyncError, BLOB_TIMEOUT, META_TIMEOUT,
};

/// 空 body 的 SHA-256（GET/HEAD/DELETE/LIST 都用它）。
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// 分页上限：防远端 NextContinuationToken 死循环。
const MAX_LIST_PAGES: usize = 10;

pub struct S3Backend {
    meta: ureq::Agent,
    blob: ureq::Agent,
    /// 归一化后的 endpoint（含 scheme，无尾 `/`，无 query）
    endpoint: Url,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
    path_style: bool,
    root: String,
    /// 时间源（epoch 秒）；测试注入固定时间以获得确定性签名。
    now: fn() -> i64,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl S3Backend {
    pub fn new(settings: &SyncSettings, secrets: &SyncSecrets) -> Result<Self, SyncError> {
        let raw = settings.s3_endpoint.trim();
        if raw.is_empty() {
            return Err(SyncError::config("S3 endpoint 未配置"));
        }
        let with_scheme = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        let mut endpoint = Url::parse(&with_scheme)
            .map_err(|e| SyncError::config(format!("S3 endpoint 格式无效: {e}")))?;
        // endpoint 里的 userinfo（`https://AKIA:secret@host`）会随 settings.json 明文落盘，
        // 而且和 s3_access_key / 凭据管理器里的 secret 重复。直接拒绝，不静默丢弃：
        // 静默丢弃会让用户以为填进去的凭证生效了，实际用的是另外两个字段。
        // 与 webdav.rs::new() 的检查同源（同族漏点）。
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(SyncError::config(
                "S3 Endpoint 不要包含 Access Key/Secret，请分别填写",
            ));
        }
        check_scheme(&endpoint)?;
        let host = endpoint
            .host_str()
            .ok_or_else(|| SyncError::config("S3 endpoint 缺少主机名"))?
            .to_string();
        endpoint.set_query(None);
        endpoint.set_fragment(None);

        let bucket = settings.s3_bucket.trim().to_string();
        if bucket.is_empty() {
            return Err(SyncError::config("S3 bucket 未配置"));
        }
        if !bucket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(SyncError::config("S3 bucket 名含非法字符"));
        }
        let region = settings.s3_region.trim().to_string();
        if region.is_empty() {
            return Err(SyncError::config("S3 region 未配置"));
        }
        let access_key = settings.s3_access_key.trim().to_string();
        // Access Key 只是 Key ID。粘整条 URL（`https://AKIA:secret@host/bucket`）或
        // `AKIA:secret` 时，Key ID 里会带 `:` `/` `@` 等字符：SigV4 签名必然失败，
        // 而且 secret 会被明文写进 settings.json。直接拒绝并说清怎么填。
        if access_key.is_empty()
            || access_key
                .chars()
                .any(|c| matches!(c, ':' | '/' | '@' | '?' | '#'))
        {
            return Err(SyncError::config(
                "S3 Access Key 只填写 Key ID，不要粘贴整条 URL",
            ));
        }
        if secrets.s3_secret_key.is_empty() {
            return Err(SyncError::config("缺少 S3 Secret Key"));
        }
        let path_style = settings.s3_path_style;
        if !path_style && !matches!(endpoint.host(), Some(url::Host::Domain(_))) {
            return Err(SyncError::config(
                "S3 virtual-host 寻址要求 endpoint 是域名（IP/自定义服务请开启 path-style）",
            ));
        }

        Ok(Self {
            meta: build_agent(&host, META_TIMEOUT),
            blob: build_agent(&host, BLOB_TIMEOUT),
            endpoint,
            bucket,
            region,
            access_key,
            secret_key: secrets.s3_secret_key.clone(),
            path_style,
            root: root_of(settings),
            now: now_secs,
        })
    }

    /// bucket 级 URL（`{endpoint}/{bucket}` 或 `https://{bucket}.{host}`）。
    fn bucket_url(&self) -> Url {
        let mut url = self.endpoint.clone();
        let base_path = url.path().trim_end_matches('/').to_string();
        if self.path_style {
            url.set_path(&format!("{base_path}/{}", self.bucket));
        } else {
            // virtual-host：host 已在 new() 校验为域名，端口由 Url 保留
            let host = self.endpoint.host_str().unwrap_or_default();
            let _ = url.set_host(Some(&format!("{}.{}", self.bucket, host)));
            if base_path.is_empty() {
                url.set_path("/");
            } else {
                url.set_path(&base_path);
            }
        }
        url
    }

    /// `{bucket_url}/{root}/{key}`（key 为空时是 root 前缀本身）。
    fn url(&self, key: &str) -> Url {
        let mut url = self.bucket_url();
        let base_path = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!("{base_path}/{}", object_path(&self.root, key)));
        url
    }

    /// 签一个请求，返回 (`Authorization`, `x-amz-date`)。
    fn sign(&self, method: &str, url: &Url, payload_hash: &str) -> (String, String) {
        let amz_date = sigv4::format_amz_date((self.now)());
        sigv4::sign(
            method,
            url,
            &self.region,
            &self.access_key,
            &self.secret_key,
            payload_hash,
            &amz_date,
        )
    }

    fn signed_request(
        &self,
        agent: &ureq::Agent,
        method: &str,
        url: &Url,
        payload_hash: &str,
    ) -> ureq::Request {
        let (auth, amz_date) = self.sign(method, url, payload_hash);
        let req = match method {
            "PUT" => agent.put(url.as_str()),
            "GET" => agent.get(url.as_str()),
            "HEAD" => agent.head(url.as_str()),
            "DELETE" => agent.delete(url.as_str()),
            _ => agent.request(method, url.as_str()),
        };
        req.set("Authorization", &auth)
            .set("x-amz-content-sha256", payload_hash)
            .set("x-amz-date", &amz_date)
    }
}

impl SyncBackend for S3Backend {
    fn put(
        &self,
        key: &str,
        body: &[u8],
        if_match: Option<&str>,
    ) -> Result<Option<String>, SyncError> {
        validate_key(key)?;
        let url = self.url(key);
        let payload_hash = sigv4::sha256_hex(body);
        let mut req = self
            .signed_request(&self.blob, "PUT", &url, &payload_hash)
            .set("Content-Type", "application/octet-stream");
        req = match if_match {
            Some("*") => req.set("If-None-Match", "*"),
            Some(etag) => req.set("If-Match", etag),
            None => req,
        };
        let conditional = if_match.is_some();
        match req.send_bytes(body) {
            Ok(resp) => {
                // 无 Location 的 3xx 会被 ureq 当成正常响应交回来，先拦掉
                reject_redirect(resp.status())?;
                Ok(resp.header("ETag").map(|v| v.to_string()))
            }
            // 部分 S3 兼容实现不认条件头（回 400/501）：按 Conflict 返回并在
            // 消息里注明，上层会降级成"先 HEAD 再 PUT"的重试路径。
            Err(ureq::Error::Status(code, _)) if conditional && (code == 400 || code == 501) => {
                Err(SyncError::conflict(format!(
                    "PUT 条件写不被服务器支持: key={} status={code}",
                    key_tag(key)
                )))
            }
            Err(e) => Err(transport_error("PUT", key, &e)),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SyncError> {
        validate_key(key)?;
        let url = self.url(key);
        let resp = match self
            .signed_request(&self.blob, "GET", &url, EMPTY_SHA256)
            .call()
        {
            Ok(resp) => {
                reject_redirect(resp.status())?;
                resp
            }
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(e) => return Err(transport_error("GET", key, &e)),
        };
        let mut buf = Vec::new();
        resp.into_reader().read_to_end(&mut buf).map_err(|e| {
            SyncError::network(format!("GET 读取响应失败: key={} {e}", key_tag(key)))
        })?;
        Ok(Some(buf))
    }

    fn head(&self, key: &str) -> Result<Option<ObjectMeta>, SyncError> {
        validate_key(key)?;
        let url = self.url(key);
        let resp = match self
            .signed_request(&self.meta, "HEAD", &url, EMPTY_SHA256)
            .call()
        {
            Ok(resp) => {
                reject_redirect(resp.status())?;
                resp
            }
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(e) => return Err(transport_error("HEAD", key, &e)),
        };
        let size = resp
            .header("Content-Length")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Ok(Some(ObjectMeta {
            key: key.to_string(),
            etag: resp.header("ETag").map(|v| v.to_string()),
            size,
        }))
    }

    fn delete(&self, key: &str) -> Result<(), SyncError> {
        validate_key(key)?;
        let url = self.url(key);
        match self
            .signed_request(&self.meta, "DELETE", &url, EMPTY_SHA256)
            .call()
        {
            Ok(resp) => {
                reject_redirect(resp.status())?;
                Ok(())
            }
            // S3 对不存在的 key 本来就返回 204；404 同样视为已删除
            Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(e) => Err(transport_error("DELETE", key, &e)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, SyncError> {
        validate_key(prefix)?;
        let root_prefix = format!("{}/", self.root);
        let trimmed = prefix.trim_matches('/');
        // 前缀补尾 `/`：避免 `devices` 误匹配 `devices2/...`
        let full_prefix = if trimmed.is_empty() {
            root_prefix.clone()
        } else {
            format!("{root_prefix}{trimmed}/")
        };

        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _page in 0..MAX_LIST_PAGES {
            let mut url = self.bucket_url();
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("list-type", "2");
                query.append_pair("prefix", &full_prefix);
                if let Some(t) = &token {
                    query.append_pair("continuation-token", t);
                }
            }
            let resp = match self
                .signed_request(&self.meta, "GET", &url, EMPTY_SHA256)
                .call()
            {
                Ok(resp) => {
                    reject_redirect(resp.status())?;
                    resp
                }
                Err(e) => return Err(transport_error("LIST", prefix, &e)),
            };
            let xml = resp.into_string().map_err(|e| {
                SyncError::network(format!("LIST 读取响应失败: key={} {e}", key_tag(prefix)))
            })?;
            let page = parse_list(&xml, &root_prefix, prefix)?;
            out.extend(page.objects);
            match page.next_token {
                Some(next) if page.truncated => token = Some(next),
                // 未截断（或没有 token）即取完
                _ => break,
            }
        }
        Ok(out)
    }
}

/// 一页 ListObjectsV2 结果。
struct ListPage {
    objects: Vec<ObjectMeta>,
    truncated: bool,
    next_token: Option<String>,
}

/// 解析 `ListBucketResult`：按本地名取 `Key` / `ETag` / `Size` / `IsTruncated` /
/// `NextContinuationToken`（S3 用默认命名空间，local name 即元素名）。
fn parse_list(xml: &str, root_prefix: &str, prefix: &str) -> Result<ListPage, SyncError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut objects = Vec::new();
    let mut cur: Option<(String, Option<String>, u64)> = None;
    let mut truncated = false;
    let mut next_token: Option<String> = None;
    let mut capture = false;
    let mut text = String::new();

    let parse_err = |e: &dyn std::fmt::Display| {
        SyncError::network(format!("LIST 响应解析失败: key={} {e}", key_tag(prefix)))
    };

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"Contents" => cur = Some((String::new(), None, 0)),
                b"Key" | b"ETag" | b"Size" | b"IsTruncated" | b"NextContinuationToken" => {
                    capture = true;
                    text.clear();
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if capture {
                    let decoded = e.decode().map_err(|err| parse_err(&err))?;
                    text.push_str(&decoded);
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if capture {
                    push_general_ref(&e, &mut text, "LIST", prefix)?;
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"Key" | b"ETag" | b"Size" | b"IsTruncated" | b"NextContinuationToken" => {
                    let value = text.trim().to_string();
                    match e.local_name().as_ref() {
                        b"Key" => {
                            if let Some(cur) = cur.as_mut() {
                                cur.0 = value;
                            }
                        }
                        b"ETag" => {
                            if let Some(cur) = cur.as_mut() {
                                cur.1 = Some(value);
                            }
                        }
                        b"Size" => {
                            if let Some(cur) = cur.as_mut() {
                                cur.2 = value.parse().unwrap_or(0);
                            }
                        }
                        b"IsTruncated" => truncated = value.eq_ignore_ascii_case("true"),
                        b"NextContinuationToken" => next_token = Some(value),
                        _ => {}
                    }
                    capture = false;
                    text.clear();
                }
                b"Contents" => {
                    if let Some((key, etag, size)) = cur.take() {
                        if let Some(rel) = key.strip_prefix(root_prefix) {
                            if !rel.is_empty() {
                                objects.push(ObjectMeta {
                                    key: rel.to_string(),
                                    etag,
                                    size,
                                });
                            }
                        }
                    }
                }
                _ => {}
            },
            Ok(_) => {}
            Err(e) => return Err(parse_err(&e)),
        }
    }

    Ok(ListPage {
        objects,
        truncated,
        next_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::sync::SyncProvider;
    use crate::services::sync::SyncErrorKind;
    use httpmock::prelude::*;

    const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    /// 1369353600 = 2013-05-24T00:00:00Z
    const FIXED_NOW: i64 = 1369353600;
    const FIXED_AMZ_DATE: &str = "20130524T000000Z";

    const PAGE1: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>sparrow-sync</Name>
  <IsTruncated>true</IsTruncated>
  <Contents><Key>sparrow/devices/a.json</Key><ETag>"e1"</ETag><Size>10</Size></Contents>
  <NextContinuationToken>tok1</NextContinuationToken>
</ListBucketResult>"#;

    const PAGE2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <IsTruncated>false</IsTruncated>
  <Contents><Key>sparrow/devices/b.json</Key><ETag>"e2"</ETag><Size>20</Size></Contents>
</ListBucketResult>"#;

    fn settings(server: &MockServer) -> SyncSettings {
        SyncSettings {
            provider: SyncProvider::S3,
            s3_endpoint: server.base_url(),
            s3_region: "us-east-1".to_string(),
            s3_bucket: "sparrow-sync".to_string(),
            s3_access_key: ACCESS.to_string(),
            s3_path_style: true,
            root_prefix: "sparrow".to_string(),
            ..Default::default()
        }
    }

    fn secrets() -> SyncSecrets {
        SyncSecrets {
            s3_secret_key: SECRET.to_string(),
            ..Default::default()
        }
    }

    fn backend(server: &MockServer) -> S3Backend {
        let mut b = S3Backend::new(&settings(server), &secrets()).expect("构造 S3 后端");
        b.now = || FIXED_NOW;
        b
    }

    /// 第 1 页请求（无 continuation-token）。
    fn first_page(req: &HttpMockRequest) -> bool {
        !req.query_params
            .as_ref()
            .map(|ps| ps.iter().any(|(k, _)| k == "continuation-token"))
            .unwrap_or(false)
    }

    #[test]
    fn put_signs_request_with_body_hash() {
        let server = MockServer::start();
        let body = br#"{"v":1}"#;
        let payload_hash = sigv4::sha256_hex(body);
        let url = Url::parse(&format!(
            "{}/sparrow-sync/sparrow/devices/a.json",
            server.base_url()
        ))
        .unwrap();
        let (expected_auth, _) = sigv4::sign(
            "PUT",
            &url,
            "us-east-1",
            ACCESS,
            SECRET,
            &payload_hash,
            FIXED_AMZ_DATE,
        );
        assert!(
            expected_auth.starts_with(&format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS}/20130524/us-east-1/s3/aws4_request"
            )),
            "{expected_auth}"
        );
        let m = server.mock(|when, then| {
            when.method(PUT)
                .path("/sparrow-sync/sparrow/devices/a.json")
                .header("Authorization", expected_auth.clone())
                .header("x-amz-content-sha256", payload_hash.clone())
                .header("x-amz-date", FIXED_AMZ_DATE);
            then.status(200).header("ETag", "\"etag-1\"");
        });
        let etag = backend(&server)
            .put("devices/a.json", body, None)
            .expect("put");
        m.assert_hits(1);
        assert_eq!(etag.as_deref(), Some("\"etag-1\""));
    }

    #[test]
    fn put_maps_status_codes() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/sparrow-sync/sparrow/devices/401.json");
            then.status(401);
        });
        server.mock(|when, then| {
            when.path("/sparrow-sync/sparrow/devices/412.json");
            then.status(412);
        });
        server.mock(|when, then| {
            when.path("/sparrow-sync/sparrow/devices/500.json");
            then.status(500);
        });
        server.mock(|when, then| {
            when.path("/sparrow-sync/sparrow/devices/400.json");
            then.status(400);
        });
        let b = backend(&server);
        assert_eq!(
            b.put("devices/401.json", b"x", None).unwrap_err().kind,
            SyncErrorKind::Auth
        );
        assert_eq!(
            b.put("devices/412.json", b"x", Some("*")).unwrap_err().kind,
            SyncErrorKind::Conflict
        );
        assert_eq!(
            b.put("devices/500.json", b"x", None).unwrap_err().kind,
            SyncErrorKind::Server
        );
        // 条件头不被支持（400）：Conflict + 消息里注明
        let err = b.put("devices/400.json", b"x", Some("\"e\"")).unwrap_err();
        assert_eq!(err.kind, SyncErrorKind::Conflict);
        assert!(
            err.message.contains("条件写不被服务器支持"),
            "{}",
            err.message
        );
    }

    #[test]
    fn list_follows_pagination() {
        let server = MockServer::start();
        let page1 = server.mock(|when, then| {
            when.method(GET)
                .path("/sparrow-sync")
                .query_param("list-type", "2")
                .query_param("prefix", "sparrow/devices/")
                .matches(first_page);
            then.status(200).body(PAGE1);
        });
        let page2 = server.mock(|when, then| {
            when.method(GET)
                .path("/sparrow-sync")
                .query_param("continuation-token", "tok1");
            then.status(200).body(PAGE2);
        });
        let list = backend(&server).list("devices").expect("list");
        page1.assert_hits(1);
        page2.assert_hits(1);
        let keys: Vec<&str> = list.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["devices/a.json", "devices/b.json"]);
        assert_eq!(list[0].etag.as_deref(), Some("\"e1\""));
        assert_eq!(list[0].size, 10);
        assert_eq!(list[1].size, 20);
    }

    #[test]
    fn head_404_is_none() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/sparrow-sync/sparrow/devices/missing.json");
            then.status(404);
        });
        let b = backend(&server);
        assert!(b.head("devices/missing.json").expect("head").is_none());
        assert!(b.get("devices/missing.json").expect("get").is_none());
    }

    #[test]
    fn url_shapes_path_style_and_virtual_host() {
        let base = SyncSettings {
            s3_region: "us-east-1".to_string(),
            s3_bucket: "sparrow-sync".to_string(),
            s3_access_key: ACCESS.to_string(),
            ..Default::default()
        };

        // 1) path-style + 无 scheme（补 https）
        let s = SyncSettings {
            s3_endpoint: "s3.example.com".to_string(),
            s3_path_style: true,
            ..base.clone()
        };
        let b = S3Backend::new(&s, &secrets()).expect("path-style");
        assert_eq!(
            b.url("devices/a.json").as_str(),
            "https://s3.example.com/sparrow-sync/sparrow/devices/a.json"
        );

        // 2) virtual-host
        let s = SyncSettings {
            s3_endpoint: "https://s3.us-east-1.amazonaws.com".to_string(),
            s3_path_style: false,
            ..base.clone()
        };
        let b = S3Backend::new(&s, &secrets()).expect("virtual-host");
        assert_eq!(
            b.url("devices/a.json").as_str(),
            "https://sparrow-sync.s3.us-east-1.amazonaws.com/sparrow/devices/a.json"
        );

        // 3) 带端口 endpoint（virtual-host 保留端口）
        let s = SyncSettings {
            s3_endpoint: "https://s3.example.com:9000".to_string(),
            s3_path_style: false,
            ..base.clone()
        };
        let b = S3Backend::new(&s, &secrets()).expect("带端口");
        assert_eq!(
            b.url("devices/a.json").as_str(),
            "https://sparrow-sync.s3.example.com:9000/sparrow/devices/a.json"
        );

        // 4) 自定义 root 前缀（含首尾斜杠也要归一化）
        let s = SyncSettings {
            s3_endpoint: "https://example.com/minio".to_string(),
            s3_path_style: true,
            root_prefix: "/custom/".to_string(),
            ..base.clone()
        };
        let b = S3Backend::new(&s, &secrets()).expect("自定义 root");
        assert_eq!(
            b.url("blobs/x.bin").as_str(),
            "https://example.com/minio/sparrow-sync/custom/blobs/x.bin"
        );

        // 5) 空 root_prefix 回落 "sparrow"
        let s = SyncSettings {
            s3_endpoint: "https://example.com".to_string(),
            s3_path_style: true,
            root_prefix: String::new(),
            ..base
        };
        let b = S3Backend::new(&s, &secrets()).expect("默认 root");
        assert_eq!(
            b.url("devices/a.json").as_str(),
            "https://example.com/sparrow-sync/sparrow/devices/a.json"
        );
    }

    #[test]
    fn new_rejects_endpoint_with_embedded_userinfo() {
        let base = SyncSettings {
            s3_endpoint: "https://s3.example.com".to_string(),
            s3_region: "us-east-1".to_string(),
            s3_bucket: "sparrow-sync".to_string(),
            s3_access_key: ACCESS.to_string(),
            s3_path_style: true,
            ..Default::default()
        };
        // userinfo 会明文进 settings.json，且与 access key / 凭据管理器里的 secret 重复，
        // 必须在 new() 就被拒绝（与 webdav.rs::new 同一条检查）
        let s = SyncSettings {
            s3_endpoint: "https://AKIA:secret@s3.example.com".to_string(),
            ..base.clone()
        };
        let err = S3Backend::new(&s, &secrets())
            .err()
            .expect("内嵌 userinfo 的 endpoint 应被拒绝");
        assert_eq!(err.kind, SyncErrorKind::Config);
        assert!(
            err.message.contains("Access Key"),
            "提示应指向分开填写: {}",
            err.message
        );
        // 错误消息本身不得回显凭证
        assert!(!err.message.contains("AKIA"), "{}", err.message);
        assert!(!err.message.contains("secret"), "{}", err.message);

        // 只有用户名、没有密码也要拒绝
        let s = SyncSettings {
            s3_endpoint: "https://AKIA@s3.example.com".to_string(),
            ..base
        };
        assert_eq!(
            S3Backend::new(&s, &secrets())
                .err()
                .expect("仅 Access Key 也应被拒绝")
                .kind,
            SyncErrorKind::Config
        );
    }

    #[test]
    fn new_rejects_access_key_that_looks_like_url() {
        let base = SyncSettings {
            s3_endpoint: "https://s3.example.com".to_string(),
            s3_region: "us-east-1".to_string(),
            s3_bucket: "sparrow-sync".to_string(),
            s3_access_key: ACCESS.to_string(),
            s3_path_style: true,
            ..Default::default()
        };
        // 粘整条 URL / `AKIA:secret` 进来时，`:` `/` `@` 会进 Key ID：
        // 签名必然失败，secret 还会明文落盘 —— 必须在 new() 就拒绝
        for bad in [
            "https://AKIA:secret@s3.example.com",
            "AKIA:secret",
            "AKIA/secret",
            "AKIA@example.com",
            "AKIA?x=1",
            "AKIA#frag",
        ] {
            let s = SyncSettings {
                s3_access_key: bad.to_string(),
                ..base.clone()
            };
            let err = S3Backend::new(&s, &secrets())
                .err()
                .unwrap_or_else(|| panic!("应拒绝非法 Access Key: {bad}"));
            assert_eq!(err.kind, SyncErrorKind::Config, "bad={bad}");
            assert!(
                err.message.contains("Key ID"),
                "提示应说清只填 Key ID: {}",
                err.message
            );
            // 错误消息不得回显粘贴进来的凭证
            assert!(!err.message.contains("AKIA"), "{}", err.message);
            assert!(!err.message.contains("secret"), "{}", err.message);
        }
        // 合法 Key ID 照常通过
        assert!(S3Backend::new(&base, &secrets()).is_ok());
    }

    #[test]
    fn new_rejects_missing_secret_key() {
        let s = SyncSettings {
            s3_endpoint: "https://s3.example.com".to_string(),
            s3_region: "us-east-1".to_string(),
            s3_bucket: "sparrow-sync".to_string(),
            s3_access_key: ACCESS.to_string(),
            s3_path_style: true,
            ..Default::default()
        };
        // Access Key 合法、Secret 为空 → 必须明确提示缺 Secret Key
        let err = S3Backend::new(&s, &SyncSecrets::default())
            .err()
            .expect("缺 secret 应报配置错误");
        assert_eq!(err.kind, SyncErrorKind::Config);
        assert!(
            err.message.contains("Secret Key"),
            "提示应指向 Secret Key: {}",
            err.message
        );
    }

    #[test]
    fn redirect_without_location_is_config_error() {
        let server = MockServer::start();
        // 3xx 没有 Location：既不能静默成功（PUT 会得到"无 ETag 的成功"），
        // 也不能映射成可重试的 Network
        server.mock(|when, then| {
            when.path("/sparrow-sync/sparrow/devices/r.json");
            then.status(302);
        });
        server.mock(|when, then| {
            when.path("/sparrow-sync");
            then.status(302);
        });
        let b = backend(&server);
        let err = b.put("devices/r.json", b"x", None).unwrap_err();
        assert_eq!(err.kind, SyncErrorKind::Config);
        assert!(!err.kind.retryable());
        // get / head / delete / list 走同一条防御
        for kind in [
            b.get("devices/r.json").unwrap_err().kind,
            b.head("devices/r.json").unwrap_err().kind,
            b.delete("devices/r.json").unwrap_err().kind,
            b.list("devices").unwrap_err().kind,
        ] {
            assert_eq!(kind, SyncErrorKind::Config);
            assert!(!kind.retryable());
        }
    }

    #[test]
    fn new_rejects_bad_config() {
        let base = SyncSettings {
            s3_endpoint: "https://s3.example.com".to_string(),
            s3_region: "us-east-1".to_string(),
            s3_bucket: "sparrow-sync".to_string(),
            s3_access_key: ACCESS.to_string(),
            s3_path_style: true,
            ..Default::default()
        };
        let mk = |f: fn(&mut SyncSettings)| {
            let mut s = base.clone();
            f(&mut s);
            S3Backend::new(&s, &secrets())
                .err()
                .expect("应报配置错误")
                .kind
        };
        assert_eq!(mk(|s| s.s3_endpoint = String::new()), SyncErrorKind::Config);
        assert_eq!(mk(|s| s.s3_bucket = String::new()), SyncErrorKind::Config);
        assert_eq!(mk(|s| s.s3_region = String::new()), SyncErrorKind::Config);
        assert_eq!(
            mk(|s| s.s3_access_key = String::new()),
            SyncErrorKind::Config
        );
        assert_eq!(
            mk(|s| s.s3_endpoint = "http://s3.example.com".to_string()),
            SyncErrorKind::Config
        );
        // virtual-host 不允许 IP endpoint
        assert_eq!(
            mk(|s| {
                s.s3_endpoint = "https://127.0.0.1:9000".to_string();
                s.s3_path_style = false;
            }),
            SyncErrorKind::Config
        );
        // 缺 secret
        assert_eq!(
            S3Backend::new(&base, &SyncSecrets::default())
                .err()
                .expect("应报配置错误")
                .kind,
            SyncErrorKind::Config
        );
    }
}
