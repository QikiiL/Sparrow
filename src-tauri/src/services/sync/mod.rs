//! 云同步（WebDAV / S3）：对象存储抽象、端到端加密、快照合并与轮询。
//!
//! 日志纪律（见实施计划第 6 节）：任何错误/日志只允许出现后端类型、
//! 对象 key 的 SHA-256 前 8 位（[`key_tag`]）、字节数、HTTP 状态码。
//! 禁止出现 URL 的 userinfo/query、Authorization、口令、内容明文——
//! [`SyncError`] 的构造函数会主动清洗传入的 message。

pub mod backend;
pub mod crypto;
pub mod merge;
pub mod s3;
pub mod service;
pub mod sigv4;
pub mod snapshot;
pub mod state;
pub mod webdav;

#[cfg(test)]
mod e2e_tests;

use std::fmt;
use std::io::Read;
use std::time::Duration;

use sha2::{Digest, Sha256};
use url::Url;

use crate::models::sync::SyncSettings;

/// 对象 key 的日志标识：SHA-256 十六进制前 8 位。key 本身可能含设备 ID/路径，
/// 但更主要的是不要为了排查问题而把整条路径（可能带用户目录名）打进日志。
pub fn key_tag(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// 对象 key 的**形状**：`devices` / `blobs` / `meta` / `root`（其它一律 `other`）。
///
/// 只暴露"这是设备快照、图片密文还是 meta"，用来让日志与错误消息能区分
/// "同步卡在图片 blob 上"和"同步卡在设备快照上"——完整 key 含设备 ID/路径，
/// 不进日志。
pub fn key_prefix(key: &str) -> &'static str {
    let key = key.trim_matches('/');
    if key.is_empty() {
        return "root";
    }
    match key.split('/').next().unwrap_or_default() {
        "devices" => "devices",
        "blobs" => "blobs",
        "meta.json" | "meta" => "meta",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// 后端共用工具（webdav.rs / s3.rs）：HTTP agent、key/URL 校验、错误映射。
// 放在这里而不是各后端一份，是因为两边的超时/代理/状态码约定必须一致。
// ---------------------------------------------------------------------------

/// 连接超时 5s；元数据请求 20s；blob 传输 60s（图片可能几 MB）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const META_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const BLOB_TIMEOUT: Duration = Duration::from_secs(60);

/// 读取 Windows 系统代理（Clash/V2Ray 等写入的 WinINET 设置）。
///
/// 与 `commands/update.rs::system_http_proxy` 同一份逻辑：ureq 只认
/// `HTTP_PROXY` 环境变量、不读系统代理，而国内直连境外存储常超时。
/// 那个函数是私有的且本次改动不允许碰 update.rs，故在此复制一份。
fn system_http_proxy() -> Option<ureq::Proxy> {
    use winreg::enums::HKEY_CURRENT_USER;
    let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").ok()?;
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    // 逐协议形式("http=127.0.0.1:7890;https=127.0.0.1:7890")优先取 https 段
    let server = if server.contains('=') {
        server
            .split(';')
            .find_map(|part| part.strip_prefix("https="))
            .or_else(|| {
                server
                    .split(';')
                    .find_map(|part| part.strip_prefix("http="))
            })
            .map(|s| s.to_string())
            .unwrap_or_default()
    } else {
        server
    };
    if server.is_empty() {
        return None;
    }
    let url = if server.contains("://") {
        server
    } else {
        format!("http://{}", server)
    };
    ureq::Proxy::new(&url).ok()
}

/// 回环地址：只有这些主机允许 `http`，也只有它们绕过系统代理
/// （本地 mock / 自建 localhost 服务经代理会被劫持）。
pub(crate) fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// 构造 ureq Agent（连接超时 5s + 总超时 `timeout`，非回环目标走系统代理）。
///
/// **重定向：不跟随。** ureq 的 `redirects(n)` 判定是
/// `history.len() + 1 >= n` 就报 `TooManyRedirects`，所以 `n = 1` 的实际语义是
/// 「第一个 3xx 直接失败」—— 正是这里要的：3xx 经 [`transport_error`] 映射成
/// 不可重试的 `Config`，而不是被静默跟随。跟随重定向对本项目只有坏处：
/// ureq 会把 301/302/303 上的 PUT / PROPFIND 改成 GET 并丢掉请求体（条件写语义
/// 随之失效），还会把 `Authorization` 带到新地址。
///
/// **协议降级：禁止。** 非回环地址额外 `https_only(true)`：即便配置绕过
/// [`check_scheme`]，也不会发出明文 http 请求；重定向到 `http://` 同样会被
/// ureq 以 `InsecureRequestHttpsOnly` 拒绝（同样映射成 `Config`）。
/// 回环地址保持原样，本地 mock / 自建服务仍可用 http。
///
/// **不复用连接。** `max_idle_connections(0)` 关掉 ureq 的空闲连接池
/// （ureq 2.x 的 pool 在任一上限为 0 时直接 noop）：每条请求都新建连接。
/// 复用被服务器/代理单方面关闭的 keep-alive 连接，是
/// `response body closed before all bytes were read` 的典型来源；
/// 客户端要的是"要么完整拿到、要么明确失败可重试"，省下的那次握手不值当。
pub(crate) fn build_agent(host: &str, timeout: Duration) -> ureq::Agent {
    let loopback = is_loopback(host);
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(timeout)
        .redirects(1)
        .max_idle_connections(0)
        .https_only(!loopback);
    if !loopback {
        if let Some(proxy) = system_http_proxy() {
            builder = builder.proxy(proxy);
        }
    }
    builder.build()
}

/// 根前缀：默认 `sparrow`，去掉首尾空白与 `/`。
pub(crate) fn root_of(settings: &SyncSettings) -> String {
    let trimmed = settings.root_prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        "sparrow".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `{root}/{key}`；key 为空时就是 root 本身（目录）。
pub(crate) fn object_path(root: &str, key: &str) -> String {
    let key = key.trim_matches('/');
    if key.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{key}")
    }
}

/// 对象 key 只允许 `[A-Za-z0-9._/-]`，且不允许空段、`.`、`..`、开头/结尾 `/`。
/// 允许空 key（代表 root 目录本身）。
pub(crate) fn validate_key(key: &str) -> Result<(), SyncError> {
    let tag = key_tag(key);
    if key.is_empty() {
        return Ok(());
    }
    if key.starts_with('/') || key.ends_with('/') {
        return Err(SyncError::config(format!(
            "对象 key 首尾不能是 /: key={tag}"
        )));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Err(SyncError::config(format!("对象 key 含非法字符: key={tag}")));
    }
    for seg in key.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(SyncError::config(format!(
                "对象 key 含非法路径段: key={tag}"
            )));
        }
    }
    Ok(())
}

/// `https` 强制；只有 localhost/127.0.0.1/::1 允许 `http`。
pub(crate) fn check_scheme(url: &Url) -> Result<(), SyncError> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if url.host_str().map(is_loopback).unwrap_or(false) => Ok(()),
        "http" => Err(SyncError::config(
            "同步地址必须使用 https（仅 localhost/127.0.0.1 允许 http）",
        )),
        other => Err(SyncError::config(format!("不支持的地址 scheme: {other}"))),
    }
}

/// 3xx 一律拒绝：同步不跟随重定向。
///
/// ureq 2.x 的 `call()` 只把 **≥400** 当错误返回，3xx 会以 `Ok(resp)` 交回来；
/// 带 `Location` 的 3xx 被 [`build_agent`] 的 `redirects(1)` 变成
/// `TooManyRedirects`（已在 [`transport_error`] 映射成 `Config`），但**没有
/// `Location`** 的 3xx 会原样落在 `Ok` 分支里——不显式检查就会被当成 2xx 静默
/// 成功（PUT 报"成功但无 ETag"、DELETE 报"已删除"）。
///
/// 所以 webdav/s3 的 put/get/head/delete/list 拿到响应后，必须先调用本函数，
/// 再判断 2xx / 404。
pub(crate) fn reject_redirect(status: u16) -> Result<(), SyncError> {
    if (300..=399).contains(&status) {
        return Err(SyncError::config("同步不跟随重定向，请直接填写最终地址"));
    }
    Ok(())
}

/// HTTP 状态码 → `SyncError`。message 只含操作名 + key_tag + 状态码。
pub(crate) fn status_error(op: &str, key: &str, status: u16) -> SyncError {
    let msg = format!("{op} 失败: key={} status={status}", key_tag(key));
    match status {
        401 | 403 => SyncError::auth(msg),
        404 => SyncError::not_found(msg),
        409 | 412 => SyncError::conflict(msg),
        429 | 500..=599 => SyncError::server(msg),
        // 3xx：重定向不是"网络抖动"，跟着走只会把请求打到别处（见 build_agent）
        300..=399 => SyncError::config(format!("{op} 被重定向: key={}", key_tag(key))),
        400..=499 => SyncError::config(msg),
        _ => SyncError::server(msg),
    }
}

/// DNS 解析失败的可读文案（`ErrorKind::Dns`）。
///
/// 国内校园网/运营商 DNS 污染下，同一个域名会时好时坏：`kind=Dns` 基本就是
/// "解析被投毒或暂时解析不出来"，不是配置错误，所以文案直接给出排查方向，
/// 并保留 key_tag 便于定位是哪个对象（不含 URL/路径/完整 key）。
pub(crate) fn dns_error_message(op: &str, key: &str) -> String {
    format!(
        "{op} 域名解析失败（DNS）: key={} shape={}。校园网/DNS 污染环境下常见，请检查网络、DNS 或代理后重试",
        key_tag(key),
        key_prefix(key)
    )
}

/// ureq 错误 → `SyncError`。传输层只记录 `ErrorKind`，不带 URL。
pub(crate) fn transport_error(op: &str, key: &str, e: &ureq::Error) -> SyncError {
    match e {
        ureq::Error::Status(code, _) => status_error(op, key, *code),
        ureq::Error::Transport(t) => match t.kind() {
            // 重定向与协议降级都是配置问题：重试一万次也是同样的结果
            ureq::ErrorKind::TooManyRedirects => SyncError::config(format!(
                "{op} 遇到重定向: key={}（同步不跟随重定向，请直接填写最终地址）",
                key_tag(key)
            )),
            ureq::ErrorKind::InsecureRequestHttpsOnly => SyncError::config(format!(
                "{op} 被拒绝: key={}（非回环地址只允许 https）",
                key_tag(key)
            )),
            // DNS 污染是**间歇**的：单次解析失败不代表配置有问题，文案给排查方向，
            // kind 仍是 Network（可重试），交给上层退避重试。
            ureq::ErrorKind::Dns => SyncError::network(dns_error_message(op, key)),
            kind => SyncError::network(format!(
                "{op} 传输失败: key={} shape={} kind={kind:?}",
                key_tag(key),
                key_prefix(key)
            )),
        },
    }
}

/// 校验响应体长度：`Content-Length` 与实际读到的字节数不一致即视为不完整。
///
/// 代理/中间设备在 keep-alive 连接被单方面关闭时，常常表现为"读到一半 EOF"，
/// ureq 的 `read_to_end` 在 chunked 或短读时**不报错**，必须自己核对长度。
/// 消息只带 key_tag 与字节数（不含 URL/路径/内容）。
pub(crate) fn check_body_complete(
    op: &str,
    key: &str,
    declared: Option<u64>,
    actual: usize,
) -> Result<(), SyncError> {
    match declared {
        Some(expected) if expected != actual as u64 => Err(SyncError::network(format!(
            "{op} 响应不完整: key={} 期望 {expected} 字节，实际 {actual} 字节",
            key_tag(key)
        ))),
        _ => Ok(()),
    }
}

/// 读完整的 GET 响应体，并按 `Content-Length` 校验完整性。
///
/// webdav/s3 两个后端共用：读错误与长度不符都映射成可重试的 `Network`。
pub(crate) fn read_response_body(
    op: &str,
    key: &str,
    resp: ureq::Response,
) -> Result<Vec<u8>, SyncError> {
    let declared = resp
        .header("Content-Length")
        .and_then(|v| v.trim().parse::<u64>().ok());
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| SyncError::network(format!("{op} 读取响应失败: key={} {e}", key_tag(key))))?;
    check_body_complete(op, key, declared, buf.len())?;
    Ok(buf)
}

/// 把 quick-xml 的 `Event::GeneralRef`（`&quot;`、`&#38;` 之类）解析成字符追加到 `text`。
///
/// quick-xml 0.39 把 XML 实体引用拆成独立事件，`Event::Text` 里不再含 `&...;`，
/// 所以 PROPFIND 的 `&quot;etag&quot;` 必须在这里补回来。
pub(crate) fn push_general_ref(
    reference: &quick_xml::events::BytesRef<'_>,
    text: &mut String,
    op: &str,
    key: &str,
) -> Result<(), SyncError> {
    let fail = |e: &dyn fmt::Display| {
        SyncError::network(format!("{op} 响应解析失败: key={} {e}", key_tag(key)))
    };
    match reference.resolve_char_ref() {
        Ok(Some(ch)) => {
            text.push(ch);
            return Ok(());
        }
        Ok(None) => {}
        Err(e) => return Err(fail(&e)),
    }
    let name = reference.decode().map_err(|e| fail(&e))?;
    if let Some(entity) = quick_xml::escape::resolve_predefined_entity(&name) {
        text.push_str(entity);
    }
    Ok(())
}

/// `%xx` 解码（自己写 20 行，不引 percent-encoding）。
pub(crate) fn percent_decode(input: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncErrorKind {
    /// 配置/口令/参数问题，重试无意义
    Config,
    /// 认证失败（401/403、解密失败）
    Auth,
    /// 远端对象不存在
    NotFound,
    /// 并发写冲突（409/412）
    Conflict,
    /// 网络/传输层失败，可重试
    Network,
    /// 远端 5xx / 429 等，可重试
    Server,
}

impl SyncErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Network => "network",
            Self::Server => "server",
        }
    }

    /// 是否值得重试（退避 500ms → 2s → 8s）。
    pub fn retryable(self) -> bool {
        matches!(self, Self::Network | Self::Server)
    }
}

#[derive(Debug, Clone)]
pub struct SyncError {
    pub kind: SyncErrorKind,
    pub message: String,
}

impl SyncError {
    /// 所有构造函数都走这里，message 会被 [`sanitize`] 清洗。
    pub fn new(kind: SyncErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: sanitize(&message.into()),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(SyncErrorKind::Config, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(SyncErrorKind::Auth, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(SyncErrorKind::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(SyncErrorKind::Conflict, message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(SyncErrorKind::Network, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(SyncErrorKind::Server, message)
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sync {}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for SyncError {}

/// ureq/文件 IO 的错误统一按 Network 处理（可重试）。
impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        Self::network(e.to_string())
    }
}

/// 去掉任何 URL 里的 userinfo（`user:pass@`）、query 与 fragment。
///
/// 逐 token 处理：只截断明显属于 URL/路径的片段，正常的中文/英文描述不受影响。
fn sanitize(input: &str) -> String {
    input
        .split_whitespace()
        .map(sanitize_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_token(token: &str) -> String {
    let mut t = token.to_string();
    // ?query / #fragment 一律丢弃
    if let Some(cut) = [t.find('?'), t.find('#')].into_iter().flatten().min() {
        t.truncate(cut);
    }
    // scheme://user:pass@host → scheme://host
    if let Some(scheme_end) = t.find("://") {
        let rest = &t[scheme_end + 3..];
        if let Some(at) = rest.find('@') {
            t = format!("{}://{}", &t[..scheme_end], &rest[at + 1..]);
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_has_no_credential_like_text() {
        // 模拟一个"不小心把整条 URL 塞进错误"的调用点：构造函数必须把它洗干净
        let err = SyncError::network(
            "GET https://alice:hunter2@dav.example.com/sparrow/devices/a.json?token=SECRET123 失败",
        );
        let shown = err.to_string();
        assert!(!shown.contains("hunter2"), "明文口令泄漏: {shown}");
        assert!(!shown.contains("alice:"), "userinfo 泄漏: {shown}");
        assert!(!shown.contains("SECRET123"), "query 泄漏: {shown}");
        assert!(shown.contains("dav.example.com"), "主机名可保留: {shown}");
        assert!(shown.starts_with("sync network: "), "kind 前缀: {shown}");
        assert!(err.kind.retryable());
        assert!(!SyncErrorKind::Auth.retryable());
        assert_eq!(SyncErrorKind::Config.as_str(), "config");
    }

    #[test]
    fn key_tag_is_short_hex() {
        let tag = key_tag("sparrow/devices/abc.json");
        assert_eq!(tag.len(), 8);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(tag, key_tag("sparrow/devices/abc.json"));
        assert_ne!(tag, key_tag("sparrow/devices/abd.json"));
    }

    /// key 形状：只区分 devices / blobs / meta / root，不泄漏完整 key
    #[test]
    fn key_prefix_reveals_shape_only() {
        assert_eq!(key_prefix("devices/abc.json"), "devices");
        assert_eq!(key_prefix("blobs/ab/cd.png"), "blobs");
        assert_eq!(key_prefix("meta.json"), "meta");
        assert_eq!(key_prefix(""), "root");
        assert_eq!(key_prefix("/"), "root");
        assert_eq!(key_prefix("weird/thing"), "other");
        assert_eq!(key_prefix("devices"), "devices");
    }

    /// DNS 文案：给出「域名解析失败 / DNS / 代理」三个关键词，
    /// 且只带 key_tag —— 不能出现完整 key、路径或 URL。
    #[test]
    fn dns_error_message_is_actionable_and_redacted() {
        let key = "sparrow/blobs/very-secret-name.bin";
        let msg = dns_error_message("MKCOL", key);
        assert!(msg.contains("域名解析失败"), "缺少 DNS 关键词: {msg}");
        assert!(msg.contains("DNS"), "缺少 DNS 字样: {msg}");
        assert!(msg.contains("代理"), "缺少代理建议: {msg}");
        assert!(msg.contains(&key_tag(key)), "缺少 key_tag: {msg}");
        assert!(!msg.contains("very-secret-name"), "泄漏了完整 key: {msg}");
        assert!(!msg.contains("sparrow/blobs"), "泄漏了路径: {msg}");
        assert!(!msg.contains("http"), "泄漏了 URL/scheme: {msg}");

        // 走一遍真实构造路径：kind 必须是可重试的 Network，且 Display 前缀正确
        let err = SyncError::network(dns_error_message("MKCOL", key));
        assert_eq!(err.kind, SyncErrorKind::Network);
        assert!(err.kind.retryable(), "DNS 失败必须可重试");
        assert!(err.to_string().starts_with("sync network: "));
    }

    /// `Content-Length` 与实际字节数不符 → Network（可重试），消息只带 tag + 字节数。
    #[test]
    fn body_length_mismatch_is_network_error() {
        let key = "blobs/a.png";
        check_body_complete("GET", key, Some(100), 100).expect("长度一致应通过");
        // 没有 Content-Length（chunked）时不做校验
        check_body_complete("GET", key, None, 42).expect("无长度头应通过");

        let err = check_body_complete("GET", key, Some(100), 37).expect_err("长度不符必须失败");
        assert_eq!(err.kind, SyncErrorKind::Network);
        assert!(err.kind.retryable(), "不完整响应必须可重试");
        assert!(err.message.contains("响应不完整"), "{}", err.message);
        assert!(
            err.message.contains("100"),
            "应带期望字节数: {}",
            err.message
        );
        assert!(
            err.message.contains("37"),
            "应带实际字节数: {}",
            err.message
        );
        assert!(err.message.contains(&key_tag(key)), "应带 key_tag");
        assert!(!err.message.contains("blobs/a.png"), "不能带完整 key");
    }
}
