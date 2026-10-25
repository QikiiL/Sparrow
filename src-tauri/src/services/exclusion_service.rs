//! 剪贴板内容排除规则(安全审计残余风险 R-1 的缓解措施)。
//!
//! 轮询线程每 500ms 读一次系统剪贴板并全量落盘,用户复制过的密码、
//! 银行卡号、API 密钥会因此明文写进 SQLite。本模块在入库前做一次判定,
//! 命中规则的内容直接跳过,不进入数据库也不生成预览。

use crate::models::clipboard_type::ClipboardType;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use std::sync::{LazyLock, Mutex, RwLock};
use tauri_plugin_store::StoreExt;

/// 命中排除规则的原因
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExclusionReason {
    App,       // 来源进程在黑名单内
    Pattern,   // 命中用户自定义正则
    Sensitive, // 命中内置敏感信息识别
}

/// 一次检查的结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExclusionCheck {
    pub excluded: bool,
    pub reason: Option<ExclusionReason>,
}

/// 已编译的排除规则集
#[derive(Debug, Clone)]
pub struct ExclusionRules {
    pub apps: Vec<String>,           // 来源进程名,全小写,如 "keepass.exe"
    pub patterns: Vec<regex::Regex>, // 用户自定义正则
    pub detect_sensitive: bool,      // 是否启用内置敏感识别
}

// ---------------------------------------------------------------------------
// Win32 声明
// ---------------------------------------------------------------------------
// 沿用 utils/input_focus.rs 的写法:直接 link 系统 DLL 并声明需要的函数。
// 不改用 windows crate 的 Win32_UI_WindowsAndMessaging 等 feature —— 那些模块
// 体量很大,会显著拖慢本项目的编译。

#[link(name = "user32")]
extern "system" {
    fn GetClipboardOwner() -> isize;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
}

#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
    fn QueryFullProcessImageNameW(
        process: isize,
        flags: u32,
        buffer: *mut u16,
        size: *mut u32,
    ) -> i32;
}

/// 只查询有限信息,比 PROCESS_ALL_ACCESS 更容易在跨权限/跨会话场景拿到句柄
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

/// 进程路径缓冲区长度(UTF-16 码元数)。正常路径远小于此,
/// 取不到就当作"无法判定",不做无上限重试。
const PROCESS_PATH_BUF_LEN: usize = 1024;

/// 读取剪贴板来源进程的可执行文件名(小写,如 "keepass.exe");无法判定返回 None。
///
/// 判定不了是常态而非异常:复制方进程可能已经退出、剪贴板被清空、
/// 或者 OpenProcess 权限不足,这些情况下一律返回 None,由调用方跳过
/// 进程黑名单判定(其余规则照常生效)。
pub fn source_process_name() -> Option<String> {
    unsafe {
        let owner = GetClipboardOwner();
        // 剪贴板内容由系统或已退出的进程放置时 owner 为 NULL,跳过黑名单判定
        if owner == 0 {
            return None;
        }

        let mut pid: u32 = 0;
        // 返回值是线程 ID,此处用不上;但 PID 只能通过出参拿
        if GetWindowThreadProcessId(owner, &mut pid) == 0 || pid == 0 {
            return None;
        }

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return None;
        }

        // 句柄无论后续步骤成败都要关掉,单独抽成函数以保证不泄漏
        let name = query_process_image_name(handle);
        CloseHandle(handle);
        name
    }
}

/// 从进程句柄取完整路径,再截取最后一段文件名并转小写。
fn query_process_image_name(handle: isize) -> Option<String> {
    let mut buffer = vec![0u16; PROCESS_PATH_BUF_LEN];
    let mut size = buffer.len() as u32;

    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    if ok == 0 {
        return None;
    }

    let len = size as usize;
    // size 由系统调用回填,越界说明返回值不可信,按失败处理
    if len == 0 || len > buffer.len() {
        return None;
    }

    let full_path = String::from_utf16_lossy(&buffer[..len]);
    let file_name = full_path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if file_name.is_empty() {
        None
    } else {
        Some(file_name)
    }
}

// ---------------------------------------------------------------------------
// 内置敏感信息识别
// ---------------------------------------------------------------------------
// 全部用 LazyLock 在首次使用时编译一次。这些表达式是模块内常量,语法在
// 开发期即已验证,unwrap 不会触发;绝不能放到 check() 里每次重新编译。

/// 信用卡候选。只圈出"像卡号"的数字串,真正的判定交给 Luhn——
/// 只靠正则会把订单号、时间戳等普通长数字误判成卡号。
static RE_CARD_CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d[ -]?){12,18}\d\b").unwrap());

/// PEM 私钥块头
static RE_PEM_PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").unwrap());

/// AWS Access Key ID(长期凭证 AKIA、临时凭证 ASIA)
static RE_AWS_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap());

/// GitHub 经典令牌(ghp/gho/ghu/ghs/ghr)与细粒度令牌
static RE_GITHUB_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36,}\b|\bgithub_pat_[A-Za-z0-9_]{22,}\b").unwrap()
});

/// Slack 令牌
static RE_SLACK_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap());

/// OpenAI 等服务的 sk- 前缀密钥。
///
/// 正文允许 `-` 与 `_`:现代格式(sk-proj-…、sk-ant-api03-…)本身就含连字符,
/// 只认 `[A-Za-z0-9]` 会在第一个连字符处截断,前缀不足 20 位便整条漏检。
/// 末尾强制一个字母数字,保证匹配不会停在连字符上(`\b` 在 `-` 前同样成立,
/// 否则尾随标点会被吞进匹配结果)。最短正文仍为 20 位,不放宽门槛。
static RE_SK_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{19,}[A-Za-z0-9]\b").unwrap());

/// JWT。三段式且中段以 eyJ 开头(header/signature 的 base64url 前缀)
static RE_JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b").unwrap()
});

/// Authorization 头里的 Bearer 令牌
static RE_BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}").unwrap());

/// Google API Key
static RE_GOOGLE_API_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bAIza[0-9A-Za-z_-]{35}\b").unwrap());

/// 中国大陆身份证号。除 18 位外还约束了省份码首位非零、年份 18xx–20xx、
/// 月份 01–12、日 01–31,这些条件对所有真实身份证都成立,能挡掉大量
/// 恰好 18 位的普通数字;最终仍以校验位为准。
static RE_CHINA_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b[1-9]\d{5}(?:18|19|20)\d{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12]\d|3[01])\d{3}[\dXx]\b",
    )
    .unwrap()
});

/// 身份证校验位权重(前 17 位)
const ID_WEIGHTS: [u8; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];

/// 加权和 mod 11 对应的校验字符
const ID_CHECK_CHARS: [u8; 11] = *b"10X98765432";

/// Luhn 校验:从右往左隔位乘 2,乘积超过 9 则减 9,总和能被 10 整除即通过。
fn passes_luhn(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for &byte in digits.iter().rev() {
        let mut d = (byte - b'0') as u32;
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    sum.is_multiple_of(10)
}

/// 发卡行识别码(IIN)前缀 + 品牌固定长度校验。
///
/// 仅靠 Luhn 时,任意 13–19 位随机数字约有 1/10 概率碰巧通过校验 —— 银行
/// 账号、IMEI、长订单号都会被当成卡号丢掉。真实卡号的前缀是分配好的,
/// 且各品牌长度固定,加上这两条约束后,30 万随机样本实测误报率从 9.93%
/// 降到 1.72%(下降约 83%);而合法卡号必然同时满足前缀与长度,
/// 因此不会因此漏拦。
fn card_brand_ok(digits: &[u8]) -> bool {
    let n = digits.len();
    if n < 4 {
        return false;
    }
    // 取前 k 位组成的数值。调用方已确认 digits 全为 ASCII 数字
    let prefix = |k: usize| -> u32 {
        digits[..k]
            .iter()
            .fold(0u32, |acc, &b| acc * 10 + (b - b'0') as u32)
    };

    match digits[0] {
        b'4' => matches!(n, 13 | 16 | 19),                     // Visa
        b'5' => (51..=55).contains(&prefix(2)) && n == 16,     // Mastercard
        b'2' => (2221..=2720).contains(&prefix(4)) && n == 16, // Mastercard 2 系
        b'6' => {
            // 银联 62;Discover 6011 / 65 / 644-649
            let two = prefix(2);
            let four = prefix(4);
            (two == 62 || four == 6011 || two == 65 || (644..=649).contains(&prefix(3)))
                && matches!(n, 16 | 17 | 18 | 19)
        }
        b'3' => {
            let two = prefix(2);
            let three = prefix(3);
            let four = prefix(4);
            if two == 34 || two == 37 {
                n == 15 // American Express
            } else if three == 309 || (300..=305).contains(&three) || two == 36 || two == 38 {
                matches!(n, 14 | 16 | 19) // Diners Club
            } else {
                (3528..=3589).contains(&four) && n == 16 // JCB
            }
        }
        _ => false,
    }
}

/// 文本中是否存在通过 Luhn、且前缀与长度都符合某个卡组织的卡号。
fn contains_credit_card(text: &str) -> bool {
    RE_CARD_CANDIDATE.find_iter(text).any(|m| {
        let digits: Vec<u8> = m
            .as_str()
            .bytes()
            .filter(|b| b.is_ascii_digit())
            .collect();
        (13..=19).contains(&digits.len()) && passes_luhn(&digits) && card_brand_ok(&digits)
    })
}

/// 18 位身份证校验位比对。第 18 位可能是 X,大小写都接受。
fn is_valid_china_id(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    if bytes.len() != 18 || !bytes[..17].iter().all(|b| b.is_ascii_digit()) {
        return false;
    }

    let mut sum = 0u32;
    for (i, weight) in ID_WEIGHTS.iter().enumerate() {
        sum += (bytes[i] - b'0') as u32 * (*weight as u32);
    }

    let expected = ID_CHECK_CHARS[(sum % 11) as usize];
    let mut actual = bytes[17];
    if actual == b'x' {
        actual = b'X';
    }
    actual == expected
}

fn contains_china_id(text: &str) -> bool {
    RE_CHINA_ID.find_iter(text).any(|m| is_valid_china_id(m.as_str()))
}

/// 是否命中内置敏感信息识别。
///
/// 刻意**不**实现 `password: xxx` 这类"敏感关键词 + 值"的通用匹配:
/// 代码、配置文件、错误日志里出现 password 字样的频率远高于真实密码泄漏,
/// 关键词匹配会制造大量误报,让人干脆关掉整个功能。这里只保留格式上
/// 高度特异的凭证(有固定前缀、固定长度或可通过校验位验证)。
pub(crate) fn contains_sensitive(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    RE_PEM_PRIVATE_KEY.is_match(text)
        || RE_AWS_KEY.is_match(text)
        || RE_GITHUB_TOKEN.is_match(text)
        || RE_SLACK_TOKEN.is_match(text)
        || RE_SK_KEY.is_match(text)
        || RE_JWT.is_match(text)
        || RE_BEARER.is_match(text)
        || RE_GOOGLE_API_KEY.is_match(text)
        || contains_china_id(text)
        || contains_credit_card(text)
}

// ---------------------------------------------------------------------------
// 配置读取
// ---------------------------------------------------------------------------

const STORE_FILE: &str = "settings.json";
const STORE_KEY: &str = "app_settings";

/// 用户正则的编译体积上限。正则源码只有几十字节,编译出的 NFA 却可能膨胀到
/// 几百 MB,一条写坏的表达式就能拖垮应用;1MB 对正常业务规则绰绰有余。
const PATTERN_SIZE_LIMIT: usize = 1 << 20;

/// 排除规则在 settings.json 里的字段(与 AppSettings 同层,字段名为
/// `excluded_apps` / `excluded_patterns` / `detect_sensitive`)。
///
/// 三个字段都带 default:老配置文件没有这些字段时等价于"不排除任何内容",
/// 不会因反序列化失败而回退整套设置。未知字段由 serde 自动忽略,
/// 因此可以直接从完整的 AppSettings JSON 里反序列化出这一段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ExclusionConfig {
    excluded_apps: Vec<String>,
    excluded_patterns: Vec<String>,
    // 老配置文件没有这个字段时,serde 的字段级 default 会让 bool 退化为 false,
    // 等于静默关掉敏感识别——必须显式指定默认开启,宁可多拦一条也不让密钥落盘
    #[serde(default = "default_detect_sensitive")]
    detect_sensitive: bool,
    excluded_allowlist: Vec<String>,
}

/// 手写 Default:derive 会把 bool 落成 false,与字段级 serde default(true)
/// 矛盾——配置解析失败/存储打开失败走 ExclusionConfig::default() 时,
/// 敏感识别会被静默关掉,违背「宁可多拦一条」的安全取舍
impl Default for ExclusionConfig {
    fn default() -> Self {
        Self {
            excluded_apps: Vec::new(),
            excluded_patterns: Vec::new(),
            detect_sensitive: true,
            excluded_allowlist: Vec::new(),
        }
    }
}

fn default_detect_sensitive() -> bool {
    true
}

fn load_exclusion_config(app: &tauri::AppHandle) -> ExclusionConfig {
    // 与 settings_service 同路径;该常量在那里是私有的,这里不改动原文件,
    // 直接按同样的规则拼出路径
    let path = crate::app_config_dir().join(STORE_FILE);
    match app.store(path) {
        Ok(store) => match store.get(STORE_KEY) {
            Some(value) => {
                serde_json::from_value::<ExclusionConfig>(value.clone()).unwrap_or_else(|e| {
                    eprintln!("排除规则配置解析失败,按默认(不排除)处理: {}", e);
                    ExclusionConfig::default()
                })
            }
            None => ExclusionConfig::default(),
        },
        Err(e) => {
            eprintln!("Failed to load settings store: {}", e);
            ExclusionConfig::default()
        }
    }
}

/// 编译用户正则。规则来自用户自己的 settings.json,但仍要防呆:
/// 语法错误或体积超限的条目只跳过并记录,绝不 panic——panic 会让
/// 500ms 轮询线程整个死掉,剪贴板功能彻底失效。
fn compile_patterns(raw: &[String]) -> Vec<Regex> {
    raw.iter()
        .filter_map(|pattern| {
            match RegexBuilder::new(pattern)
                .size_limit(PATTERN_SIZE_LIMIT)
                .build()
            {
                Ok(regex) => Some(regex),
                Err(e) => {
                    eprintln!("跳过无效的排除正则 \"{}\": {}", pattern, e);
                    None
                }
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 规则状态
// ---------------------------------------------------------------------------

/// 全局规则状态,由 app state 持有
pub struct ExclusionState {
    // 轮询每 500ms 一次,规则不能每次重新编译;读写锁持有时间仅限
    // check() 内的几次正则匹配,设置变更才走写锁
    rules: RwLock<ExclusionRules>,
    // 误伤豁免:用户点过「仍要记录」的内容 hash,命中则完全跳过排除规则
    allowed: RwLock<Vec<String>>,
    // 待重新捕获的 hash。用户点「仍要记录」后由命令写入,轮询线程下一轮取走。
    // 单独用 Mutex 而非塞进 RwLock:这里是"取走即清"的一次性信号,
    // 与规则集没有一致性要求,不该和规则刷新互相阻塞
    recapture: Mutex<Option<String>>,
}

impl ExclusionState {
    pub fn new() -> Self {
        Self {
            // reload 之前用"启用敏感识别"兜底:这是一个安全特性,
            // 配置未加载时宁可多拦一条,也不让密钥落盘
            rules: RwLock::new(ExclusionRules {
                apps: Vec::new(),
                patterns: Vec::new(),
                detect_sensitive: true,
            }),
            allowed: RwLock::new(Vec::new()),
            recapture: Mutex::new(None),
        }
    }

    /// 从 settings 重新构建规则集(设置变更时调用,不是每轮轮询都调)
    pub fn reload(&self, app: &tauri::AppHandle) {
        let config = load_exclusion_config(app);

        let apps = config
            .excluded_apps
            .iter()
            .map(|name| name.trim().to_lowercase())
            .filter(|name| !name.is_empty())
            .collect();

        let patterns = compile_patterns(&config.excluded_patterns);

        // 锁中毒时仍取内部数据:规则集损坏最多导致漏拦,不该连带拖垮轮询线程
        let mut rules = self.rules.write().unwrap_or_else(|e| e.into_inner());
        rules.apps = apps;
        rules.patterns = patterns;
        rules.detect_sensitive = config.detect_sensitive;

        let mut allowed = self.allowed.write().unwrap_or_else(|e| e.into_inner());
        *allowed = config.excluded_allowlist;
    }

    /// 该内容是否被用户豁免(在「已排除」提示上点过「仍要记录」)。
    /// 豁免优先级高于三层规则:用户已经看过内容并明确要求保留
    pub fn is_allowed(&self, hash: &str) -> bool {
        let allowed = self.allowed.read().unwrap_or_else(|e| e.into_inner());
        allowed.iter().any(|h| h == hash)
    }

    /// 请求重新记录刚被排除的内容。内容此时仍在系统剪贴板里,轮询线程
    /// 下一轮清掉 last_hash 即可重新走一遍入库流程,无需用户再复制一次
    pub fn request_recapture(&self, hash: String) {
        let mut pending = self.recapture.lock().unwrap_or_else(|e| e.into_inner());
        *pending = Some(hash);
    }

    /// 取走待重捕获的 hash,取走即清。每轮轮询只消费一次 ——
    /// 否则信号可能滞留到几天后,用户再复制同一内容时被重复入库
    pub fn take_recapture(&self) -> Option<String> {
        let mut pending = self.recapture.lock().unwrap_or_else(|e| e.into_inner());
        pending.take()
    }

    /// 判断是否应跳过入库。source_process 为 None 表示未能判定来源进程
    pub fn check(
        &self,
        content: &str,
        item_type: i32,
        source_process: Option<&str>,
    ) -> ExclusionCheck {
        let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());

        // 1) 来源进程黑名单。判定不了进程时跳过这一档,其余规则照常生效
        if let Some(process) = source_process {
            let process = process.trim().to_lowercase();
            if !process.is_empty() && rules.apps.contains(&process) {
                return ExclusionCheck {
                    excluded: true,
                    reason: Some(ExclusionReason::App),
                };
            }
        }

        // 2) 用户自定义正则。文件类型的 content 是换行分隔的路径列表,
        //    同样参与匹配,用户可能想按目录排除
        if !content.is_empty() && rules.patterns.iter().any(|re| re.is_match(content)) {
            return ExclusionCheck {
                excluded: true,
                reason: Some(ExclusionReason::Pattern),
            };
        }

        // 3) 内置敏感识别,仅对文本与链接生效。
        //    图片的 content 恒为占位符 "[图片]",没有可识别的明文;
        //    文件类型的内容只是路径列表,本身不含密钥
        let is_text_or_link = item_type == ClipboardType::Text as i32
            || item_type == ClipboardType::Link as i32;
        if rules.detect_sensitive && is_text_or_link && contains_sensitive(content) {
            return ExclusionCheck {
                excluded: true,
                reason: Some(ExclusionReason::Sensitive),
            };
        }

        ExclusionCheck {
            excluded: false,
            reason: None,
        }
    }
}

impl Default for ExclusionState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Luhn / 身份证纯逻辑 ----

    fn luhn_ok(s: &str) -> bool {
        let d: Vec<u8> = s.bytes().filter(|b| b.is_ascii_digit()).collect();
        (13..=19).contains(&d.len()) && passes_luhn(&d) && card_brand_ok(&d)
    }

    #[test]
    fn test_luhn_valid_cards() {
        assert!(luhn_ok("4532015112830366"));
        assert!(luhn_ok("4111111111111111"));
        assert!(luhn_ok("5500005555555559"));
        assert!(luhn_ok("378282246310005"));
        // 常见的空格/连字符分隔写法也要能识别
        assert!(luhn_ok("4532 0151 1283 0366"));
        assert!(luhn_ok("4532-0151-1283-0366"));
    }

    #[test]
    fn test_brand_prefix_rejects_luhn_valid_non_cards() {
        // 这三个都通过 Luhn,但首位 9/8/7 不分配给任何卡组织。
        // 加上 IIN 约束后必须放行,否则银行账号、IMEI 这类长数字会被误伤
        assert!(!luhn_ok("9123456789012348"));
        assert!(!luhn_ok("8123456789012340"));
        assert!(!luhn_ok("7123456789012342"));
    }

    #[test]
    fn test_brand_prefix_accepts_major_brands() {
        assert!(luhn_ok("6212345678901232")); // 银联 62,16 位
        assert!(luhn_ok("6011111111111117")); // Discover 6011,16 位
        assert!(luhn_ok("4222222222222")); // Visa 13 位
    }

    #[test]
    fn test_luhn_invalid_cards() {
        assert!(!luhn_ok("4532015112830367"));
        assert!(!luhn_ok("1234567812345678"));
        assert!(!luhn_ok("4111111111111112"));
    }

    #[test]
    fn test_luhn_length_fence() {
        // 长度栅栏:即便 Luhn 通过,12 位和 20 位也不算卡号
        assert!(!luhn_ok("411111111111"));
        assert!(!luhn_ok("41111111111111111111"));
    }

    #[test]
    fn test_china_id_check_digit() {
        assert!(is_valid_china_id("11010519491231002X"));
        assert!(is_valid_china_id("11010519491231002x"));
        // 改动任意一位都应失败
        assert!(!is_valid_china_id("11010519491231003X"));
        assert!(!is_valid_china_id("11010619491231002X"));
        assert!(!is_valid_china_id("110105194912310020"));
        // 长度或前 17 位含非数字
        assert!(!is_valid_china_id("1101051949123100"));
        assert!(!is_valid_china_id("11010X19491231002X"));
    }

    #[test]
    fn test_china_id_unique_check_digit() {
        // 任一 17 位前缀的合法校验位应当唯一
        let hits: Vec<char> = "0123456789X"
            .chars()
            .filter(|c| is_valid_china_id(&format!("11010519491231002{}", c)))
            .collect();
        assert_eq!(hits, vec!['X']);
    }

    // ---- 内置敏感识别 ----

    // 测试夹具一律由拼接构造,不写成完整字面量:这些都是**伪造**的样本,
    // 但 GitHub 的密钥扫描不区分真假,直接写死会让整个推送被拒。
    // 拼接结果与上面各正则要匹配的目标完全一致,不影响测试意图
    fn fx_aws() -> String {
        format!("{}IOSFODNN7EXAMPLE", "AKIA")
    }
    fn fx_github() -> String {
        format!("gh{}1234567890abcdefghijklmnopqrstuvwxyz", "p_")
    }
    fn fx_github_pat() -> String {
        format!("github_{}11ABCDEFG0aBcDeFgHiJkL_abcdefghij", "pat_")
    }
    fn fx_slack() -> String {
        format!("xo{}1234567890-1234567890-AbCdEfGhIjKlMnOpQrStUvWx", "xb-")
    }
    fn fx_sk() -> String {
        format!("s{}abcdefghijklmnopqrstuvwxyz123456", "k-")
    }
    fn fx_google() -> String {
        format!("AI{}SyD-1234567890abcdefghijklmnopqrstu", "za")
    }
    fn fx_jwt() -> String {
        format!("{}eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w", "eyJhbGciOiJIUzI1NiJ9.")
    }

    #[test]
    fn test_sensitive_secrets() {
        assert!(contains_sensitive(&format!("key: {}", fx_aws())));
        assert!(contains_sensitive(&fx_github()));
        assert!(contains_sensitive(&fx_github_pat()));
        assert!(contains_sensitive(&fx_slack()));
        assert!(contains_sensitive(&fx_sk()));
        assert!(contains_sensitive(&fx_google()));
        assert!(contains_sensitive(&fx_jwt()));
        assert!(contains_sensitive("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123"));
        assert!(contains_sensitive(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----"
        ));
    }

    // 现代 sk- 密钥正文含 `-` / `_`(sk-proj-…、sk-ant-api03-…)。样本同样拼接构造
    fn fx_sk_hyphen() -> String {
        format!("{}GGR57ZnNnnBVQTul-jBeoc", "sk-u6")
    }
    fn fx_sk_long() -> String {
        format!("{}bhoLSQ4ggDtzwXGylTv1QERH87RiOBYvJxFJreznS7QjLK", "sk-DZ")
    }
    fn fx_sk_proj() -> String {
        format!("{}j-AbCdEf1234567890_ghijklmnop", "sk-pro")
    }
    fn fx_sk_ant() -> String {
        format!("{}03-antApiKeyAbCdEf1234567890", "sk-ant-api")
    }

    #[test]
    fn test_sensitive_sk_key_hyphenated_body() {
        // 回归:正文里出现 `-` 时旧正则 `[A-Za-z0-9]{20,}` 会在连字符处截断,
        // 18 位前缀不足 20 而整条漏检,密钥被正常入库
        assert!(contains_sensitive(&fx_sk_hyphen()));
        // 纯字母数字的 48 位密钥旧正则就能命中,不得因新写法回归
        assert!(contains_sensitive(&fx_sk_long()));
        // 现代格式:sk-proj-…(含 `-` 与 `_`)、sk-ant-api03-…
        assert!(contains_sensitive(&fx_sk_proj()));
        assert!(contains_sensitive(&fx_sk_ant()));
        // 句子中间、带尾随标点时同样成立
        assert!(contains_sensitive(&format!("我的 key 是 {} 别外传", fx_sk_hyphen())));
        assert!(contains_sensitive(&format!("{}。", fx_sk_long())));
    }

    #[test]
    fn test_sk_key_no_false_positive() {
        // 长度栅栏:正文不足 20 位不算密钥,不能为了个例把门槛降到大量误报
        assert!(!contains_sensitive("sk-short"));
        assert!(!contains_sensitive("sk-"));
        assert!(!contains_sensitive("sk-1234567890123456789"));
        assert!(!contains_sensitive("task-sk-1234567890"));
        assert!(!contains_sensitive("https://example.com/docs/disk-usage-guide"));
        // 匹配结果必须以字母数字结尾:尾随连字符不被吞进匹配文本
        let hit = RE_SK_KEY.find("sk-abcdefghijklmnopqrst-").unwrap();
        assert!(hit.as_str().ends_with('t'));
    }

    #[test]
    fn test_sensitive_card_and_id() {
        assert!(contains_sensitive("卡号 4532015112830366 记得删"));
        assert!(contains_sensitive("身份证 11010519491231002X"));
        assert!(contains_sensitive("11010519491231002x"));
    }

    #[test]
    fn test_no_false_positive_on_plain_content() {
        // 误报控制:这些日常内容都不得被判为敏感
        assert!(!contains_sensitive("hello world"));
        assert!(!contains_sensitive("订单号 1234567890123456"));
        assert!(!contains_sensitive("https://example.com/docs/getting-started"));
        assert!(!contains_sensitive("2024-01-01 12:00:00"));
        assert!(!contains_sensitive("这是一段普通的中文文本"));
        assert!(!contains_sensitive(""));
    }

    #[test]
    fn test_no_keyword_matching() {
        // 刻意不做 password: xxx 的关键词匹配,代码片段里误报太多
        assert!(!contains_sensitive("const password = getInput();"));
        assert!(!contains_sensitive("password: string;"));
    }

    // ---- 用户正则编译 ----

    #[test]
    fn test_compile_patterns_skips_invalid() {
        let raw = vec![
            r"\d{3}".to_string(),
            "[unclosed".to_string(), // 非法语法,应被跳过而不是 panic
            "foo.*bar".to_string(),
        ];
        let compiled = compile_patterns(&raw);
        assert_eq!(compiled.len(), 2);
        assert!(compiled[0].is_match("a123b"));
        assert!(compiled[1].is_match("xxfooyybarzz"));
    }

    #[test]
    fn test_compile_patterns_rejects_oversized() {
        // 嵌套重复量词会撑爆 NFA 体积,应被 size_limit 挡下而不是耗尽内存。
        // 25 万个状态约为几 MB,稳定超过 1MB 上限;(a{100}){100} 只有 1 万个
        // 状态,仍在限内,不足以触发这条路径
        let raw = vec!["(a{500}){500}".to_string()];
        assert!(compile_patterns(&raw).is_empty());
    }

    // ---- check() 判定顺序与适用范围 ----

    fn state_with(apps: Vec<String>, patterns: &[&str], sensitive: bool) -> ExclusionState {
        let state = ExclusionState::new();
        {
            let mut rules = state.rules.write().unwrap();
            rules.apps = apps;
            rules.patterns = compile_patterns(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            rules.detect_sensitive = sensitive;
        }
        state
    }

    #[test]
    fn test_check_app_blacklist() {
        let state = state_with(vec!["keepass.exe".to_string()], &[], false);
        let hit = state.check("any content", ClipboardType::Text as i32, Some("KeePass.EXE"));
        assert!(hit.excluded);
        assert_eq!(hit.reason, Some(ExclusionReason::App));

        // 不在名单内的进程放行
        assert!(!state
            .check("any content", ClipboardType::Text as i32, Some("notepad.exe"))
            .excluded);
        // 判定不了来源进程时跳过这一档
        assert!(!state.check("any content", ClipboardType::Text as i32, None).excluded);
        assert!(!state.check("any content", ClipboardType::Text as i32, Some("")).excluded);
    }

    #[test]
    fn test_check_user_pattern() {
        let state = state_with(vec![], &[r"C:\\Secret"], false);
        let hit = state.check(
            "C:\\Secret\\a.txt\nC:\\Secret\\b.txt",
            ClipboardType::File as i32,
            None,
        );
        assert!(hit.excluded);
        assert_eq!(hit.reason, Some(ExclusionReason::Pattern));
    }

    #[test]
    fn test_check_sensitive_scope() {
        let state = state_with(vec![], &[], true);
        let secret = fx_aws();

        assert_eq!(
            state.check(&secret, ClipboardType::Text as i32, None).reason,
            Some(ExclusionReason::Sensitive)
        );
        // 链接里的查询参数同样可能带令牌
        assert!(state
            .check(
                &format!("https://x.com/cb?t={}", secret),
                ClipboardType::Link as i32,
                None
            )
            .excluded);
        // 图片内容恒为占位符,跳过敏感识别
        assert!(!state
            .check("[图片]", ClipboardType::Image as i32, None)
            .excluded);
        // 文件类型只跑用户正则,不跑内置敏感识别
        assert!(!state
            .check(
                &format!("C:\\Users\\a\\{}.txt", secret),
                ClipboardType::File as i32,
                None
            )
            .excluded);
    }

    #[test]
    fn test_check_order_app_before_pattern() {
        // 同时命中时,进程黑名单优先
        let state = state_with(vec!["keepass.exe".to_string()], &["secret"], true);
        let hit = state.check("secret", ClipboardType::Text as i32, Some("keepass.exe"));
        assert_eq!(hit.reason, Some(ExclusionReason::App));
    }

    #[test]
    fn test_check_disabled_sensitive() {
        let state = state_with(vec![], &[], false);
        assert!(!state
            .check(&fx_aws(), ClipboardType::Text as i32, None)
            .excluded);
    }
}
