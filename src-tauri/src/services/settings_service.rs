use crate::models::settings::AppSettings;
use tauri::Manager;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "settings.json";
const STORE_KEY: &str = "app_settings";

/// 开机自启动的任务计划名。提权应用(requireAdministrator manifest)
/// 无法用注册表 Run 键自启动——登录时 Windows 不自动弹 UAC,会静默跳过;
/// 任务计划程序的"最高权限运行"是标准做法,登录即启动且无需 UAC
const AUTOSTART_TASK: &str = "SparrowAutostart";

/// 改名(2.0.0)前的旧任务名:1.x 的"剪贴板管理器"用它注册。升级后必须清掉,
/// 否则新旧两个任务会在登录时同时拉起两个进程。
const LEGACY_AUTOSTART_TASK: &str = "ClipboardManagerAutostart";

/// 自启动诊断日志。GUI 进程没有控制台,`eprintln!` 的内容彻底丢失,
/// 出问题时只能靠这个文件还原 schtasks 到底返回了什么
const AUTOSTART_LOG_FILE: &str = "autostart.log";

/// 日志超过该体积直接截断重写,避免长期运行无限增长
const AUTOSTART_LOG_MAX_BYTES: u64 = 256 * 1024;

/// 日志里 schtasks 输出摘要的最大字符数
const LOG_SNIPPET_MAX: usize = 300;

// ---------------------------------------------------------------------------
// 诊断日志
// ---------------------------------------------------------------------------

/// 追加一行自启动诊断日志到 `<程序目录>/config/autostart.log`。
/// 日志自身失败绝不能影响主流程,所有 IO 错误一律吞掉
fn log_autostart(msg: &str) {
    use std::io::Write;

    let path = crate::app_config_dir().join(AUTOSTART_LOG_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 超过上限先删掉重建:自启动日志没有历史价值,只需要最近一段
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > AUTOSTART_LOG_MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        let _ = std::fs::remove_file(&path);
    }
    let line = format!("[{}] {}\n", now_stamp(), msg);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{} UTC", format_utc(secs))
}

/// 无 chrono 依赖的时间戳:`YYYY-MM-DD HH:MM:SS`(UTC)。
/// 用 days→civil 的标准算法换算,不为一个日志时间戳引入新依赖
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, h, mi, s)
}

/// 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// schtasks 调用与输出解析
// ---------------------------------------------------------------------------

fn run_schtasks(args: &[&str]) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW:应用在 UI 会话里跑 schtasks,别闪控制台黑窗
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("无法执行 schtasks: {}", e))
}

/// schtasks 的输出编码随环境变化:管道下通常是控制台代码页(中文系统为 GBK),
/// 某些版本/区域设置会吐 UTF-16LE。这里只做能确定的那部分还原,
/// 解不出来的字节按 UTF-8 宽松解码(路径比较另有归一化兜底)
fn decode_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        return from_utf16le(&bytes[2..]);
    }
    // 无 BOM 但大量 0x00 → 实际是 UTF-16LE(ASCII 字符的高字节全 0)
    let zeros = bytes.iter().filter(|b| **b == 0).count();
    if !bytes.is_empty() && zeros * 3 > bytes.len() {
        return from_utf16le(bytes);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn from_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// 压成单行并截断,用于日志/错误信息里的输出摘要
fn snippet(bytes: &[u8]) -> String {
    let text = decode_output(bytes);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > LOG_SNIPPET_MAX {
        let mut out: String = collapsed.chars().take(LOG_SNIPPET_MAX).collect();
        out.push('…');
        out
    } else {
        collapsed
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

// 中文 Windows 下 schtasks 按 GBK 输出提示,UTF-8 解码后是乱码,
// 因此这几条已知提示按原始字节匹配后再翻译成可读文本
const GBK_ACCESS_DENIED: &[u8] = &[0xBE, 0xDC, 0xBE, 0xF8, 0xB7, 0xC3, 0xCE, 0xCA]; // 拒绝访问
const GBK_TASK_NOT_FOUND: &[u8] = &[0xCF, 0xB5, 0xCD, 0xB3, 0xD5, 0xD2, 0xB2, 0xBB, 0xB5, 0xBD]; // 系统找不到

fn is_access_denied(output: &std::process::Output) -> bool {
    let denied = |bytes: &[u8]| {
        contains_bytes(bytes, GBK_ACCESS_DENIED)
            || decode_output(bytes)
                .to_lowercase()
                .contains("access is denied")
    };
    denied(&output.stdout) || denied(&output.stderr)
}

fn is_task_not_found(output: &std::process::Output) -> bool {
    let missing = |bytes: &[u8]| {
        contains_bytes(bytes, GBK_TASK_NOT_FOUND)
            || decode_output(bytes)
                .to_lowercase()
                .contains("cannot find the file")
    };
    missing(&output.stdout) || missing(&output.stderr)
}

/// 拼出可读的失败详情:stderr 优先,其次 stdout,都空就带退出码。
/// 命中已知 GBK 提示时用中文翻译,避免 UI 里只剩乱码
/// (原始字节仍完整写进 autostart.log,排障不受影响)
fn output_detail(output: &std::process::Output) -> String {
    let raw = if !output.stderr.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    let code = exit_code_text(output);
    let text = snippet(raw);
    // GBK 输出按 UTF-8 解码会带替换字符,这种"原文"放进弹窗毫无价值
    let unreadable = text.is_empty() || text.contains('\u{FFFD}');
    let known = if is_access_denied(output) {
        Some("拒绝访问(权限不足)")
    } else if is_task_not_found(output) {
        Some("任务不存在")
    } else {
        None
    };
    match (known, unreadable) {
        (Some(k), true) => format!("{} (schtasks 退出码 {})", k, code),
        (Some(k), false) => format!("{} | {} (schtasks 退出码 {})", k, text, code),
        (None, true) => format!("schtasks 退出码 {}", code),
        (None, false) => format!("{} (schtasks 退出码 {})", text, code),
    }
}

fn exit_code_text(output: &std::process::Output) -> String {
    match output.status.code() {
        Some(code) => code.to_string(),
        None => "未知".to_string(),
    }
}

// ---------------------------------------------------------------------------
// 纯解析函数(可单测)
// ---------------------------------------------------------------------------

/// 取计划任务 XML 里某个标签的文本(仅支持成对标签,输出被截断时返回 None)。
/// 不引 XML crate:这里只需要 <Command> / <Arguments> 两个标签
fn xml_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    let end = rest.find(&close)?;
    Some(decode_xml_entities(rest[..end].trim()))
}

fn decode_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Windows 路径归一化,仅用于判等:去两侧引号与空白、`/`→`\`、
/// 去掉 `\\?\` / `\\?\UNC\` 前缀、大小写不敏感
fn normalize_win_path(p: &str) -> String {
    let trimmed = p.trim().trim_matches('"').trim();
    let slashed = trimmed.replace('/', "\\").to_lowercase();
    if let Some(rest) = slashed.strip_prefix("\\\\?\\unc\\") {
        return format!("\\\\{}", rest);
    }
    if let Some(rest) = slashed.strip_prefix("\\\\?\\") {
        return rest.to_string();
    }
    slashed
}

/// 任务指向的可执行文件是不是给定的 exe(路径判等,不看参数)
fn task_targets_exe(xml: &str, exe_path: &str) -> bool {
    match xml_tag_text(xml, "Command") {
        Some(command) => normalize_win_path(&command) == normalize_win_path(exe_path),
        None => false,
    }
}

/// 删除前的归属校验。只有两种情况允许删除:
/// 1. 任务指向的就是当前程序(同一个 exe 路径);
/// 2. 任务指向的路径在本机已不存在(安装被删/换目录后的陈旧残留)。
///
/// 否则说明这条任务属于另一份副本(便携版、开发构建、换过目录的安装),
/// 删掉它等于替别的副本做了它没要求的事
fn should_delete_task(xml: &str, exe_path: &str) -> bool {
    if task_targets_exe(xml, exe_path) {
        return true;
    }
    match xml_tag_text(xml, "Command") {
        Some(target) => {
            let normalized = normalize_win_path(&target);
            !normalized.is_empty() && !std::path::Path::new(&normalized).exists()
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// 任务查询
// ---------------------------------------------------------------------------

/// 查询任务 XML:
/// - `Ok(Some(xml))` 任务存在
/// - `Ok(None)`      任务不存在(或输出里读不出 `<Command>`,一律按不存在处理,宁可不删)
/// - `Err(msg)`      schtasks 本身失败(拒绝访问等),调用方决定是否继续
fn query_task_xml(task: &str) -> Result<Option<String>, String> {
    let output = run_schtasks(&["/Query", "/TN", task, "/XML", "ONE"])?;
    let text = decode_output(&output.stdout);
    log_autostart(&format!(
        "query {}: exit={} stdout={} stderr={}",
        task,
        exit_code_text(&output),
        snippet(&output.stdout),
        snippet(&output.stderr)
    ));
    if output.status.success() && text.contains("<Task") {
        return Ok(Some(text));
    }
    if is_access_denied(&output) {
        return Err(format!(
            "查询计划任务 {} 失败: {}",
            task,
            output_detail(&output)
        ));
    }
    if is_task_not_found(&output) {
        return Ok(None);
    }
    // 命令成功但没读到 <Task>,或失败原因不明:按不存在处理(绝不因此删任务)
    Ok(None)
}

/// 创建后的回查:任务必须存在且 `<Command>` 指向当前 exe。
/// 没有这一步,schtasks 的 0 退出码可能对应一条指向别的 exe 的任务
fn verify_autostart(exe_path: &str) -> Result<(), String> {
    let xml = match query_task_xml(AUTOSTART_TASK) {
        Ok(Some(xml)) => xml,
        Ok(None) => {
            let msg = format!("自启动任务创建后回查失败: 任务 {} 不存在", AUTOSTART_TASK);
            log_autostart(&format!("verify: exe={} {}", exe_path, msg));
            return Err(msg);
        }
        Err(e) => {
            log_autostart(&format!("verify: exe={} 查询失败: {}", exe_path, e));
            return Err(format!("自启动任务创建后回查失败: {}", e));
        }
    };
    let target = xml_tag_text(&xml, "Command");
    if !task_targets_exe(&xml, exe_path) {
        let msg = format!(
            "自启动任务指向 {:?},与当前程序 {} 不一致",
            target.unwrap_or_else(|| "<未知>".to_string()),
            exe_path
        );
        log_autostart(&format!("verify: exe={} {}", exe_path, msg));
        return Err(format!("自启动任务创建后回查失败: {}", msg));
    }
    log_autostart(&format!(
        "verify: exe={} ok target={:?}",
        exe_path,
        target.unwrap_or_default()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// 自启动状态
// ---------------------------------------------------------------------------

/// 系统里自启动任务的真实状态,供设置页显示(配置说开 ≠ 系统真的注册了)
#[derive(Debug, Clone, serde::Serialize)]
pub struct AutostartStatus {
    /// 任务存在且按预期参数(--minimized)注册
    pub configured: bool,
    /// 系统里存在同名计划任务
    pub task_exists: bool,
    /// 任务指向的可执行文件(取自任务 XML 的 `<Command>`)
    pub task_target: Option<String>,
    /// 任务存在且指向当前运行的程序 —— UI 判断「开机自启动是否真的生效」的唯一依据
    pub ready: bool,
}

/// 查询自启动任务真实状态。查询失败(拒绝访问等)按"不存在"上报并写日志,
/// 让 UI 至少能提示"未注册"
pub fn autostart_status(exe_path: &str) -> AutostartStatus {
    let xml = match query_task_xml(AUTOSTART_TASK) {
        Ok(xml) => xml,
        Err(e) => {
            log_autostart(&format!("status: exe={} 查询失败: {}", exe_path, e));
            None
        }
    };
    let task_exists = xml.is_some();
    let task_target = xml
        .as_deref()
        .and_then(|x| xml_tag_text(x, "Command"))
        .filter(|t| !t.is_empty());
    let args_ok = xml
        .as_deref()
        .and_then(|x| xml_tag_text(x, "Arguments"))
        .map(|a| a.contains("--minimized"))
        .unwrap_or(false);
    let ready = xml
        .as_deref()
        .map(|x| task_targets_exe(x, exe_path))
        .unwrap_or(false);
    log_autostart(&format!(
        "status: exe={} exists={} target={:?} args_minimized={} ready={}",
        exe_path,
        task_exists,
        task_target.clone().unwrap_or_default(),
        args_ok,
        ready
    ));
    AutostartStatus {
        configured: task_exists && args_ok,
        task_exists,
        task_target,
        ready,
    }
}

// ---------------------------------------------------------------------------
// 启用 / 停用
// ---------------------------------------------------------------------------

pub fn enable_autostart(exe_path: &str) -> Result<(), String> {
    let tr = format!("\"{}\" --minimized", exe_path);
    let output = run_schtasks(&[
        "/Create",
        "/TN",
        AUTOSTART_TASK,
        "/TR",
        &tr,
        "/SC",
        "ONLOGON",
        "/RL",
        "HIGHEST",
        "/F",
    ])?;
    log_autostart(&format!(
        "enable: exe={} args=[/Create /TN {} /TR {} /SC ONLOGON /RL HIGHEST /F] exit={} stdout={} stderr={}",
        exe_path,
        AUTOSTART_TASK,
        tr,
        exit_code_text(&output),
        snippet(&output.stdout),
        snippet(&output.stderr)
    ));
    if !output.status.success() {
        let detail = output_detail(&output);
        log_autostart(&format!("enable: exe={} 失败 detail={}", exe_path, detail));
        return Err(format!("创建自启动任务失败: {}", detail));
    }
    verify_autostart(exe_path)
}

pub fn disable_autostart() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("无法获取当前程序路径: {}", e))?
        .to_string_lossy()
        .to_string();
    disable_autostart_for(&exe)
}

/// 停用自启动:先查任务归属,确认是自己的任务(或陈旧残留)才删
fn disable_autostart_for(exe_path: &str) -> Result<(), String> {
    let xml = match query_task_xml(AUTOSTART_TASK) {
        Ok(Some(xml)) => xml,
        // 任务本就不存在视为已关闭,不算失败(幂等)
        Ok(None) => {
            log_autostart(&format!(
                "disable: exe={} 任务 {} 不存在,无需删除",
                exe_path, AUTOSTART_TASK
            ));
            return Ok(());
        }
        Err(e) => {
            log_autostart(&format!(
                "disable: exe={} 查询失败,跳过删除: {}",
                exe_path, e
            ));
            return Err(e);
        }
    };

    if !should_delete_task(&xml, exe_path) {
        let target = xml_tag_text(&xml, "Command").unwrap_or_else(|| "<未知>".to_string());
        log_autostart(&format!(
            "disable: exe={} 任务属于其它副本(target={}),已跳过删除",
            exe_path, target
        ));
        return Ok(());
    }

    let output = run_schtasks(&["/Delete", "/TN", AUTOSTART_TASK, "/F"])?;
    log_autostart(&format!(
        "disable: exe={} delete exit={} stdout={} stderr={}",
        exe_path,
        exit_code_text(&output),
        snippet(&output.stdout),
        snippet(&output.stderr)
    ));
    if !output.status.success() {
        let detail = output_detail(&output);
        log_autostart(&format!(
            "disable: exe={} 删除失败 detail={}",
            exe_path, detail
        ));
        return Err(format!("删除自启动任务失败: {}", detail));
    }
    Ok(())
}

/// 品牌改名迁移:把 1.x 的旧自启动任务换成新任务名,并让新任务与当前设置一致。
///
/// 旧任务按名字删除(该任务名只被本产品 1.x 用过,不存在"别的副本"问题);
/// 新任务按当前设置收敛:
/// - start_with_windows = true  → 用当前 exe 重建新任务(enable_autostart 幂等)
/// - start_with_windows = false → 确保新任务不存在(disable_autostart 幂等,带归属校验)
///
/// 读不出配置时整个自启动同步直接跳过:否则回落的默认值(false)会把任务误删
pub fn migrate_autostart_task(app_handle: &tauri::AppHandle) {
    match run_schtasks(&["/Delete", "/TN", LEGACY_AUTOSTART_TASK, "/F"]) {
        Ok(output) if output.status.success() => {
            log_autostart(&format!(
                "migrate: 已删除旧任务 {} exit=0",
                LEGACY_AUTOSTART_TASK
            ));
            eprintln!("Removed legacy autostart task: {}", LEGACY_AUTOSTART_TASK);
        }
        // 旧任务本就不存在:schtasks 返回非 0,属正常情况
        Ok(output) => log_autostart(&format!(
            "migrate: 旧任务 {} 未删除(可能不存在) exit={}",
            LEGACY_AUTOSTART_TASK,
            exit_code_text(&output)
        )),
        Err(e) => {
            log_autostart(&format!("migrate: 删除旧任务失败: {}", e));
            eprintln!("Failed to remove legacy autostart task: {}", e);
        }
    }

    let settings = match load_settings_checked(app_handle) {
        Ok(settings) => settings,
        Err(e) => {
            let msg = format!("读取设置失败,跳过自启动同步: {}", e);
            log_autostart(&format!("migrate: {}", msg));
            eprintln!("Autostart migration skipped: {}", msg);
            return;
        }
    };

    let result = if settings.start_with_windows {
        std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|p| enable_autostart(&p.to_string_lossy()))
    } else {
        disable_autostart()
    };
    match result {
        Ok(()) => log_autostart(&format!(
            "migrate: 收敛完成 start_with_windows={}",
            settings.start_with_windows
        )),
        Err(e) => {
            log_autostart(&format!("migrate: 自启动同步失败: {}", e));
            eprintln!("Autostart migration failed: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// 设置读写
// ---------------------------------------------------------------------------

/// settings.json 固定放在程序目录的 config/ 下(绝对路径传入,
/// 不走插件默认的 %APPDATA% 解析)
fn store_path() -> std::path::PathBuf {
    crate::app_config_dir().join(STORE_FILE)
}

/// 严格版读取:store 打不开或反序列化失败都返回 Err。
/// 自启动同步必须用它——读不到配置时绝不能拿默认值(false)去删任务
fn load_settings_checked(app_handle: &tauri::AppHandle) -> Result<AppSettings, String> {
    let store = app_handle
        .store(store_path())
        .map_err(|e| format!("打开设置文件失败: {}", e))?;
    let mut settings = match store.get(STORE_KEY) {
        Some(value) => {
            serde_json::from_value(value.clone()).map_err(|e| format!("解析设置文件失败: {}", e))?
        }
        // 键不存在(首次启动/空文件)是合法状态,不是读取失败
        None => AppSettings::default(),
    };
    // 旧默认热键值跟随新默认(见 AppSettings::migrate_legacy_seq_hotkey)
    settings.migrate_legacy_seq_hotkey();
    Ok(settings)
}

/// UI 用的宽松版读取:读不出来就回落默认值(界面至少能显示)
pub fn load_settings(app_handle: &tauri::AppHandle) -> AppSettings {
    match load_settings_checked(app_handle) {
        Ok(settings) => settings,
        Err(e) => {
            log_autostart(&format!("load_settings: 回落到默认设置: {}", e));
            eprintln!("Failed to load settings store: {}", e);
            AppSettings::default()
        }
    }
}

pub fn save_settings(app_handle: &tauri::AppHandle, settings: &AppSettings) -> Result<(), String> {
    // 取改动前的值:自动启动只在开关真正变化时同步,避免每次保存都触碰任务计划
    let previous = load_settings(app_handle);

    // 自启动同步放在写文件之前:失败时返回 Err,配置保持原样,
    // 不会出现"配置说开、系统没开"的永久不一致
    if settings.start_with_windows != previous.start_with_windows {
        let result = if settings.start_with_windows {
            std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|p| enable_autostart(&p.to_string_lossy()))
        } else {
            disable_autostart()
        };
        if let Err(e) = result {
            log_autostart(&format!("save_settings: 自启动同步失败,设置未写入: {}", e));
            eprintln!("Failed to sync autostart: {}", e);
            return Err(format!("同步开机自启动失败: {}", e));
        }
    }

    let store = app_handle.store(store_path()).map_err(|e| e.to_string())?;
    let value = serde_json::to_value(settings).map_err(|e| e.to_string())?;
    store.set(STORE_KEY, value);
    store.save().map_err(|e| e.to_string())?;

    // 排除规则改动立即生效(轮询线程读的是 ExclusionState,不是这份文件)。
    // 刷新失败不能让保存失败:设置已落盘,规则最多等下次启动再加载
    if let Some(state) =
        app_handle.try_state::<crate::services::exclusion_service::ExclusionState>()
    {
        state.reload(app_handle);
    }

    // 云同步配置改动立即生效:轮询循环读的是 SyncHandle 里的副本,
    // 这里替换副本并唤醒循环(启用/停用/改地址都在下一轮生效)
    if let Some(handle) = app_handle.try_state::<crate::services::sync::service::SyncHandle>() {
        handle.set_settings(settings.sync.clone());
    }

    // 短信验证码开关:轮询线程读的是原子标志,这里同步翻转,即改即生效
    crate::services::sms_code_service::set_enabled(settings.sms_code_enabled);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- normalize_win_path:路径判等的归一化 ----

    #[test]
    fn normalize_path_is_case_insensitive() {
        assert_eq!(
            normalize_win_path("C:\\Program Files\\Sparrow\\sparrow.exe"),
            normalize_win_path("c:\\program files\\SPARROW\\Sparrow.EXE")
        );
    }

    #[test]
    fn normalize_path_treats_slash_and_backslash_alike() {
        assert_eq!(
            normalize_win_path("C:/Program Files/Sparrow/sparrow.exe"),
            normalize_win_path("C:\\Program Files\\Sparrow\\sparrow.exe")
        );
    }

    #[test]
    fn normalize_path_strips_quotes_and_verbatim_prefix() {
        assert_eq!(
            normalize_win_path("\"C:\\Sparrow\\sparrow.exe\""),
            normalize_win_path("C:\\Sparrow\\sparrow.exe")
        );
        assert_eq!(
            normalize_win_path("\\\\?\\C:\\Sparrow\\sparrow.exe"),
            normalize_win_path("C:\\Sparrow\\sparrow.exe")
        );
        assert_eq!(
            normalize_win_path("\\\\?\\UNC\\server\\share\\sparrow.exe"),
            normalize_win_path("\\\\server\\share\\sparrow.exe")
        );
    }

    // ---- xml_tag_text:计划任务 XML 取值 ----

    #[test]
    fn xml_tag_text_reads_command() {
        let xml = r#"<?xml version="1.0"?>
<Task><Actions Context="Author"><Exec>
<Command>C:\Sparrow\sparrow.exe</Command>
<Arguments>--minimized</Arguments>
</Exec></Actions></Task>"#;
        assert_eq!(
            xml_tag_text(xml, "Command").as_deref(),
            Some("C:\\Sparrow\\sparrow.exe")
        );
        assert_eq!(
            xml_tag_text(xml, "Arguments").as_deref(),
            Some("--minimized")
        );
    }

    #[test]
    fn xml_tag_text_missing_tag_is_none() {
        let xml = "<Task><Actions /></Task>";
        assert!(xml_tag_text(xml, "Command").is_none());
        // 只有开标签、没有闭标签(输出被截断)也不能误判
        assert!(xml_tag_text("<Command>C:\\a.exe", "Command").is_none());
    }

    #[test]
    fn xml_tag_text_decodes_entities() {
        let xml = "<Command>C:\\a&amp;b\\sparrow.exe</Command>";
        assert_eq!(
            xml_tag_text(xml, "Command").as_deref(),
            Some("C:\\a&b\\sparrow.exe")
        );
    }

    // ---- task_targets_exe:删除前的归属校验 ----

    #[test]
    fn task_targets_exe_matches_same_path_with_different_case_and_quotes() {
        let xml = "<Exec><Command>\"c:/sparrow/sparrow.exe\"</Command></Exec>";
        assert!(task_targets_exe(xml, "C:\\Sparrow\\Sparrow.exe"));
    }

    #[test]
    fn task_targets_exe_rejects_other_copy() {
        // 旧程序名(clipboard-manager-tauri.exe)或别的目录的副本都不算归属
        let old = "<Exec><Command>C:\\old\\clipboard-manager-tauri.exe</Command></Exec>";
        assert!(!task_targets_exe(old, "C:\\Sparrow\\sparrow.exe"));

        let other = "<Exec><Command>D:\\portable\\sparrow.exe</Command></Exec>";
        assert!(!task_targets_exe(other, "C:\\Sparrow\\sparrow.exe"));
    }

    #[test]
    fn task_targets_exe_without_command_is_false() {
        assert!(!task_targets_exe("<Task />", "C:\\Sparrow\\sparrow.exe"));
    }

    #[test]
    fn should_delete_task_only_for_own_or_stale_target() {
        let me = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();

        // 1. 指向当前 exe → 自己的任务,可删
        let own = format!("<Exec><Command>{}</Command></Exec>", me);
        assert!(should_delete_task(&own, &me));

        // 2. 指向本机已不存在的路径 → 陈旧残留,可删
        let stale = "<Exec><Command>C:\\Gone\\sparrow.exe</Command></Exec>";
        assert!(should_delete_task(stale, &me));

        // 3. 指向本机仍存在的另一个 exe(别的副本)→ 跳过删除
        let other = "<Exec><Command>C:\\Windows\\System32\\notepad.exe</Command></Exec>";
        assert!(!should_delete_task(other, &me));

        // 4. 读不出 <Command> → 无法判断归属,一律不删
        assert!(!should_delete_task("<Task />", &me));
    }

    // ---- 日志时间戳 ----

    #[test]
    fn format_utc_matches_known_epochs() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09 01:46:40");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13:20");
    }
}
