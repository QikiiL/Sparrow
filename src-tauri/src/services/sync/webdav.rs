//! WebDAV 后端：MKCOL / PROPFIND / PUT / GET / HEAD / DELETE + Basic Auth。
//!
//! 远端布局：`{webdav_url}/{root_prefix}/{key}`，目录 `devices/` 存设备快照、
//! `blobs/` 存图片密文（目录靠 MKCOL 显式创建）。
//!
//! 条件写（`if_match`）与错误映射的约定见 [`super::backend`] 的 trait 文档；
//! 日志纪律见 [`super`]：错误消息只带 `key_tag` 与状态码。

use std::io::Read;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use quick_xml::events::Event;
use quick_xml::Reader;
use url::Url;

use crate::models::sync::{SyncSecrets, SyncSettings};

use super::backend::{ObjectMeta, SyncBackend};
use super::{
    build_agent, check_scheme, key_tag, object_path, percent_decode, push_general_ref,
    reject_redirect, root_of, transport_error, validate_key, SyncError, BLOB_TIMEOUT, META_TIMEOUT,
};

const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop>"#,
    r#"<d:getetag/><d:getcontentlength/>"#,
    r#"</d:prop></d:propfind>"#
);

pub struct WebDavBackend {
    /// 元数据请求（MKCOL / PROPFIND / HEAD / DELETE）
    meta: ureq::Agent,
    /// 大对象传输（GET / PUT），超时更宽
    blob: ureq::Agent,
    /// 用户配置的集合根 URL（不含 root_prefix）
    base: Url,
    /// `Authorization: Basic ...` 的完整值
    auth: String,
    root: String,
}

impl WebDavBackend {
    pub fn new(settings: &SyncSettings, secrets: &SyncSecrets) -> Result<Self, SyncError> {
        let raw = settings.webdav_url.trim();
        if raw.is_empty() {
            return Err(SyncError::config("WebDAV 地址未配置"));
        }
        let mut base =
            Url::parse(raw).map_err(|e| SyncError::config(format!("WebDAV 地址格式无效: {e}")))?;
        // 地址里的 userinfo（`https://user:pass@host/`）会随 settings.json 明文落盘，
        // 而且和 username / 凭据管理器里的密码重复。直接拒绝，不静默丢弃：
        // 静默丢弃会让用户以为填进去的凭证生效了，实际用的是另外两个字段。
        if !base.username().is_empty() || base.password().is_some() {
            return Err(SyncError::config(
                "WebDAV 地址不要包含用户名或密码，请分别填写用户名和密码",
            ));
        }
        check_scheme(&base)?;
        let host = base
            .host_str()
            .ok_or_else(|| SyncError::config("WebDAV 地址缺少主机名"))?
            .to_string();
        if base.cannot_be_a_base() {
            return Err(SyncError::config("WebDAV 地址不能作为基准 URL"));
        }
        // 配置里的 query/fragment 一律丢掉：对象路径由 root + key 决定
        base.set_query(None);
        base.set_fragment(None);

        let credentials = format!("{}:{}", settings.username, secrets.webdav_password);
        Ok(Self {
            meta: build_agent(&host, META_TIMEOUT),
            blob: build_agent(&host, BLOB_TIMEOUT),
            base,
            auth: format!("Basic {}", BASE64.encode(credentials)),
            root: root_of(settings),
        })
    }

    /// `{base}/{root}/{key}`（key 为空时是 root 目录本身）。
    fn url(&self, key: &str) -> Url {
        let mut url = self.base.clone();
        let base_path = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!("{base_path}/{}", object_path(&self.root, key)));
        url
    }

    /// 与 `url(prefix)` 对应的路径前缀，用来把 PROPFIND 返回的 href 剥成相对 key。
    fn href_base(&self) -> String {
        format!("{}/{}", self.base.path().trim_end_matches('/'), self.root)
    }

    fn mkcol(&self, key: &str) -> Result<(), SyncError> {
        let url = self.url(key);
        let req = self
            .meta
            .request("MKCOL", url.as_str())
            .set("Authorization", &self.auth);
        match req.call() {
            // 201 Created / 200 OK 之外，405 表示目录已存在，同样视为成功
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(405, _)) => Ok(()),
            Err(e) => Err(transport_error("MKCOL", key, &e)),
        }
    }
}

impl SyncBackend for WebDavBackend {
    fn ensure_dirs(&self) -> Result<(), SyncError> {
        for key in ["", "devices", "blobs"] {
            self.mkcol(key)?;
        }
        Ok(())
    }

    fn put(
        &self,
        key: &str,
        body: &[u8],
        if_match: Option<&str>,
    ) -> Result<Option<String>, SyncError> {
        validate_key(key)?;
        let url = self.url(key);
        let mut req = self
            .blob
            .put(url.as_str())
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/octet-stream");
        req = match if_match {
            // 仅创建：已存在则服务器回 409/412
            Some("*") => req.set("If-None-Match", "*"),
            // 乐观并发：ETag 不匹配则 412
            Some(etag) => req.set("If-Match", etag),
            None => req,
        };
        let resp = req
            .send_bytes(body)
            .map_err(|e| transport_error("PUT", key, &e))?;
        // 无 Location 的 3xx 会被 ureq 当成正常响应交回来，先拦掉
        reject_redirect(resp.status())?;
        // ETag 保持服务器原样（含引号），上层拿它做下次 If-Match
        Ok(resp.header("ETag").map(|v| v.to_string()))
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SyncError> {
        validate_key(key)?;
        let url = self.url(key);
        let resp = match self
            .blob
            .get(url.as_str())
            .set("Authorization", &self.auth)
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
            .meta
            .head(url.as_str())
            .set("Authorization", &self.auth)
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
            .meta
            .delete(url.as_str())
            .set("Authorization", &self.auth)
            .call()
        {
            Ok(resp) => {
                reject_redirect(resp.status())?;
                Ok(())
            }
            // 已经不存在视为删除成功
            Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(e) => Err(transport_error("DELETE", key, &e)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, SyncError> {
        validate_key(prefix)?;
        let url = self.url(prefix);
        let resp = match self
            .meta
            .request("PROPFIND", url.as_str())
            .set("Authorization", &self.auth)
            .set("Depth", "1")
            .set("Content-Type", "application/xml; charset=utf-8")
            .send_string(PROPFIND_BODY)
        {
            Ok(resp) => {
                reject_redirect(resp.status())?;
                resp
            }
            // 集合还不存在：等价于空列表（ensure_dirs 之后的正常情况不会走到这里）
            Err(ureq::Error::Status(404, _)) => return Ok(Vec::new()),
            Err(e) => return Err(transport_error("PROPFIND", prefix, &e)),
        };
        // 207 Multi-Status 才是列表；其它 2xx 按空列表处理
        if resp.status() != 207 {
            return Ok(Vec::new());
        }
        let xml = resp.into_string().map_err(|e| {
            SyncError::network(format!(
                "PROPFIND 读取响应失败: key={} {e}",
                key_tag(prefix)
            ))
        })?;
        parse_propfind(&xml, prefix, &self.href_base())
    }
}

/// PROPFIND 响应里的一条 `d:response`。
#[derive(Default)]
struct DavEntry {
    href: Option<String>,
    etag: Option<String>,
    size: u64,
}

/// 解析 `d:multistatus`：按本地名匹配（前缀可能是 `d:` / `D:` / 无前缀），
/// href 做 `%xx` 解码并剥掉 `{base}/{root}`，最后丢掉集合自身与子目录项。
fn parse_propfind(xml: &str, prefix: &str, href_base: &str) -> Result<Vec<ObjectMeta>, SyncError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut entries: Vec<DavEntry> = Vec::new();
    let mut cur: Option<DavEntry> = None;
    // 当前正在收集文本的叶子元素（"href" / "getetag" / "getcontentlength"）
    let mut capture: Option<&'static str> = None;
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"response" => cur = Some(DavEntry::default()),
                b"href" => {
                    capture = Some("href");
                    text.clear();
                }
                b"getetag" => {
                    capture = Some("getetag");
                    text.clear();
                }
                b"getcontentlength" => {
                    capture = Some("size");
                    text.clear();
                }
                _ => {}
            },
            Ok(Event::Empty(_)) => {}
            Ok(Event::Text(e)) => {
                if capture.is_some() {
                    let decoded = e.decode().map_err(|err| {
                        SyncError::network(format!(
                            "PROPFIND 响应解析失败: key={} {err}",
                            key_tag(prefix)
                        ))
                    })?;
                    text.push_str(&decoded);
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if capture.is_some() {
                    push_general_ref(&e, &mut text, "PROPFIND", prefix)?;
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"href" | b"getetag" | b"getcontentlength" => {
                    let value = text.trim().to_string();
                    if let Some(entry) = cur.as_mut() {
                        match capture.take() {
                            Some("href") => entry.href = Some(value),
                            Some("getetag") => entry.etag = Some(value),
                            Some("size") => entry.size = value.parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                    capture = None;
                    text.clear();
                }
                b"response" => {
                    if let Some(entry) = cur.take() {
                        entries.push(entry);
                    }
                }
                _ => {}
            },
            Ok(_) => {}
            Err(e) => {
                return Err(SyncError::network(format!(
                    "PROPFIND 响应解析失败: key={} {e}",
                    key_tag(prefix)
                )))
            }
        }
    }

    let prefix_trimmed = prefix.trim_matches('/');
    let mut out = Vec::new();
    for entry in entries {
        let Some(href) = entry.href else { continue };
        let decoded = percent_decode(href.trim());
        let Some(rel) = strip_base(&decoded, href_base) else {
            continue;
        };
        // 集合项（以 / 结尾）与 prefix 自身都跳过
        if rel.ends_with('/') || rel.is_empty() || rel == prefix_trimmed {
            continue;
        }
        out.push(ObjectMeta {
            key: rel,
            etag: entry.etag,
            size: entry.size,
        });
    }
    Ok(out)
}

/// 把 href 变成相对 `{base}/{root}` 的 key；不匹配（不同集合）返回 None。
fn strip_base(href: &str, href_base: &str) -> Option<String> {
    // 有些服务器返回绝对 URL，先取 path 部分
    let path = match href.find("://") {
        Some(idx) => {
            let rest = &href[idx + 3..];
            match rest.find('/') {
                Some(slash) => &rest[slash..],
                None => return None,
            }
        }
        None => href,
    };
    let base = href_base.trim_end_matches('/');
    let stripped = path.strip_prefix(base)?;
    Some(stripped.trim_start_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::sync::{SyncProvider, SyncSettings};
    use crate::services::sync::SyncErrorKind;
    use httpmock::prelude::*;

    const PROPFIND_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/sparrow/devices/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/sparrow/devices/aaa.json</d:href>
    <d:propstat><d:prop>
      <d:getetag>&quot;etag-a&quot;</d:getetag>
      <d:getcontentlength>42</d:getcontentlength>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/sparrow/devices/ccc%2Ejson</d:href>
    <d:propstat><d:prop>
      <d:getetag>"etag-c"</d:getetag>
      <d:getcontentlength>7</d:getcontentlength>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

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

    fn backend(server: &MockServer) -> WebDavBackend {
        WebDavBackend::new(&settings(server), &secrets()).expect("构造 WebDAV 后端")
    }

    #[test]
    fn ensure_dirs_sends_three_mkcol() {
        let server = MockServer::start();
        let root = server.mock(|when, then| {
            when.path("/sparrow");
            then.status(201);
        });
        let devices = server.mock(|when, then| {
            when.path("/sparrow/devices");
            then.status(405);
        });
        let blobs = server.mock(|when, then| {
            when.path("/sparrow/blobs");
            then.status(201);
        });
        backend(&server).ensure_dirs().expect("ensure_dirs");
        root.assert_hits(1);
        devices.assert_hits(1);
        blobs.assert_hits(1);
    }

    #[test]
    fn put_sends_conditional_headers_and_returns_etag() {
        let server = MockServer::start();
        let create = server.mock(|when, then| {
            when.path("/sparrow/blobs/abc.bin")
                .header("If-None-Match", "*")
                .header("Content-Type", "application/octet-stream");
            then.status(201).header("ETag", "\"v1\"");
        });
        let b = backend(&server);
        let etag = b.put("blobs/abc.bin", b"hello", Some("*")).expect("put");
        assert_eq!(etag.as_deref(), Some("\"v1\""));
        create.assert_hits(1);

        let update = server.mock(|when, then| {
            when.path("/sparrow/blobs/abc.bin")
                .header("If-Match", "\"v1\"");
            then.status(200).header("ETag", "\"v2\"");
        });
        let etag = b
            .put("blobs/abc.bin", b"hello2", Some("\"v1\""))
            .expect("put with if-match");
        assert_eq!(etag.as_deref(), Some("\"v2\""));
        update.assert_hits(1);
    }

    #[test]
    fn basic_auth_header_is_sent() {
        let server = MockServer::start();
        // "user:pass" 的 base64
        let m = server.mock(|when, then| {
            when.path("/sparrow/devices/x.json")
                .header("Authorization", "Basic dXNlcjpwYXNz");
            then.status(200).body("{}");
        });
        let got = backend(&server).get("devices/x.json").expect("get");
        assert_eq!(got.as_deref(), Some(&b"{}"[..]));
        m.assert_hits(1);
    }

    #[test]
    fn get_404_is_none_and_head_404_is_none() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/sparrow/devices/missing.json");
            then.status(404);
        });
        let b = backend(&server);
        assert!(b.get("devices/missing.json").expect("get").is_none());
        assert!(b.head("devices/missing.json").expect("head").is_none());
    }

    #[test]
    fn head_returns_etag_and_size() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/sparrow/devices/a.json");
            then.status(200)
                .header("ETag", "\"e9\"")
                .header("Content-Length", "123");
        });
        let meta = backend(&server)
            .head("devices/a.json")
            .expect("head")
            .expect("存在");
        assert_eq!(meta.etag.as_deref(), Some("\"e9\""));
        assert_eq!(meta.size, 123);
        assert_eq!(meta.key, "devices/a.json");
    }

    #[test]
    fn propfind_lists_two_objects_with_depth_one() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.path("/sparrow/devices")
                .header("Depth", "1")
                .body_contains("propfind");
            then.status(207).body(PROPFIND_XML);
        });
        let list = backend(&server).list("devices").expect("list");
        m.assert_hits(1);
        let keys: Vec<&str> = list.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["devices/aaa.json", "devices/ccc.json"]);
        assert_eq!(list[0].etag.as_deref(), Some("\"etag-a\""));
        assert_eq!(list[0].size, 42);
        assert_eq!(list[1].size, 7);
    }

    #[test]
    fn non_207_propfind_yields_empty_list() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/sparrow/devices");
            then.status(200);
        });
        assert!(backend(&server).list("devices").expect("list").is_empty());
    }

    #[test]
    fn status_codes_map_to_error_kinds() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.path("/sparrow/devices/401.json");
            then.status(401);
        });
        server.mock(|when, then| {
            when.path("/sparrow/devices/412.json");
            then.status(412);
        });
        server.mock(|when, then| {
            when.path("/sparrow/devices/500.json");
            then.status(500);
        });
        server.mock(|when, then| {
            when.path("/sparrow/devices/403.json");
            then.status(403);
        });
        let b = backend(&server);
        assert_eq!(
            b.put("devices/401.json", b"x", None).unwrap_err().kind,
            SyncErrorKind::Auth
        );
        assert_eq!(
            b.put("devices/403.json", b"x", None).unwrap_err().kind,
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
        // 错误消息不得泄漏完整路径
        let err = b.get("devices/500.json").unwrap_err();
        assert!(!err.message.contains("devices/500.json"), "{}", err.message);
    }

    #[test]
    fn rejects_url_with_embedded_userinfo() {
        let mut s = settings(&MockServer::start());
        // userinfo 会明文进 settings.json，必须在 new() 就被拒绝
        s.webdav_url = "https://alice:secret@dav.example.com/x".to_string();
        let err = WebDavBackend::new(&s, &secrets())
            .err()
            .expect("内嵌 userinfo 的地址应被拒绝");
        assert_eq!(err.kind, SyncErrorKind::Config);
        assert!(
            err.message.contains("用户名或密码"),
            "提示应指向分开填写: {}",
            err.message
        );
        // 错误消息本身不得回显口令
        assert!(!err.message.contains("secret"), "{}", err.message);

        // 只有用户名、没有密码也要拒绝
        s.webdav_url = "https://alice@dav.example.com/x".to_string();
        assert_eq!(
            WebDavBackend::new(&s, &secrets())
                .err()
                .expect("仅用户名也应被拒绝")
                .kind,
            SyncErrorKind::Config
        );
    }

    #[test]
    fn redirect_is_config_error_and_not_retryable() {
        let server = MockServer::start();
        let _m = server.mock(|when, then| {
            when.path("/sparrow/devices/r.json");
            then.status(302).header("Location", "/elsewhere/r.json");
        });
        // build_agent 不跟随重定向（redirects(1) 在 ureq 里等价于"首个 3xx 即失败"），
        // 3xx 必须映射成 Config（不可重试），而不是 Network
        let err = backend(&server).put("devices/r.json", b"x", None).unwrap_err();
        assert_eq!(err.kind, SyncErrorKind::Config);
        assert!(!err.kind.retryable());
        assert!(!err.message.contains("/elsewhere"), "{}", err.message);
    }

    #[test]
    fn redirect_without_location_is_config_error() {
        let server = MockServer::start();
        // 3xx 没有 Location：ureq 会把它当正常响应交回来，既不能静默成功
        // （PUT 得到"无 ETag 的成功"、DELETE 得到"已删除"），也不能映射成可重试的 Network
        server.mock(|when, then| {
            when.path("/sparrow/devices/r.json");
            then.status(302);
        });
        server.mock(|when, then| {
            when.path("/sparrow/devices");
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
    fn rejects_plain_http_for_non_loopback_and_bad_keys() {
        let mut s = settings(&MockServer::start());
        s.webdav_url = "http://dav.example.com/sparrow".to_string();
        let err = WebDavBackend::new(&s, &secrets())
            .err()
            .expect("http 非回环地址应被拒绝");
        assert_eq!(err.kind, SyncErrorKind::Config);

        let server = MockServer::start();
        let b = backend(&server);
        for bad in ["../escape", "/abs", "a//b", "with space"] {
            assert_eq!(
                b.put(bad, b"x", None).unwrap_err().kind,
                SyncErrorKind::Config,
                "应拒绝非法 key: {bad}"
            );
        }
        // https 允许
        let mut s = settings(&server);
        s.webdav_url = "https://dav.example.com/sparrow".to_string();
        assert!(WebDavBackend::new(&s, &secrets()).is_ok());
    }
}
