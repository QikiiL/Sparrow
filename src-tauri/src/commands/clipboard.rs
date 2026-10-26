use base64::Engine;
use regex::Regex;
use sqlx::sqlite::SqlitePool;
use std::sync::LazyLock;
use tauri::{Emitter, Manager};

#[tauri::command]
pub async fn activate_item(
    app_handle: tauri::AppHandle,
    id: i64,
    force_paste: Option<bool>,
) -> Result<(), String> {
    // 粘贴还是仅复制:优先取前端的强制指定,否则按唤出窗口时检测到的输入状态
    let do_paste = force_paste.unwrap_or_else(|| {
        app_handle
            .state::<crate::utils::input_focus::PasteMode>()
            .get()
    });

    // 「粘贴后保持打开」开启时窗口不销毁,只归还焦点;焦点无法可靠归还的
    // 情形(托盘打开、目标已关闭)在投递核心里降级为仅复制
    let handling =
        if crate::services::settings_service::load_settings(&app_handle).keep_open_on_paste {
            crate::services::paste_service::FocusHandling::KeepOpen
        } else {
            crate::services::paste_service::FocusHandling::HideFirst
        };

    // 窗口销毁/焦点归还/监听抑制/模拟按键/更新使用时间,全部在共享核心里
    // (与连续粘贴同一条投递路径,行为不会分叉)
    match crate::services::paste_service::deliver_item_by_id(
        &app_handle, id, handling, do_paste, 500,
    )
    .await?
    {
        // pasted=false(发送前校验没过,只写了剪贴板)对单击路径同样是"已投递":
        // 内容在剪贴板里,用户可以手动 Ctrl+V
        crate::services::paste_service::Delivery::Delivered { .. } => Ok(()),
        crate::services::paste_service::Delivery::Missing => Err("Item not found".to_string()),
    }
}

#[tauri::command]
pub async fn delete_item(app_handle: tauri::AppHandle, id: i64) -> Result<(), String> {
    let db = app_handle.state::<SqlitePool>();

    // 先取出文件路径(图片 + html/rtf 格式文件),删行后清理对应文件;
    // content_hash 一并取出:删除成功后要写同步墓碑(否则 peer 会把这条"复活")
    let row = sqlx::query_as::<
        _,
        (
            Option<i32>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >("SELECT type, file_path, html_path, rtf_path, content_hash FROM items WHERE id = ?")
    .bind(id)
    .fetch_optional(&*db)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("DELETE FROM items WHERE id = ?")
        .bind(id)
        .execute(&*db)
        .await
        .map_err(|e| e.to_string())?;

    if let Some((item_type, file_path, html_path, rtf_path, content_hash)) = row {
        if item_type == Some(crate::models::clipboard_type::ClipboardType::Image as i32) {
            if let Some(path) = file_path {
                crate::services::image_cleanup::remove_image_file(&app_handle, &path);
            }
        }
        // html/rtf 格式文件在删除条目时一并清理(文本条目也可能携带)
        for path in [html_path, rtf_path].into_iter().flatten() {
            crate::services::image_cleanup::remove_format_file(&app_handle, &path);
        }
        // 行已删掉才写墓碑:删除失败(上面已 early return)不入队
        if let Some(hash) = content_hash.filter(|h| !h.is_empty()) {
            crate::services::sync::service::queue_tombstones(&app_handle, vec![hash]);
        }
    }

    let _ = app_handle.emit("item-deleted", id);
    Ok(())
}

/// 清空剪贴板历史(保留收藏):删除所有未收藏条目,清理不再被引用的图片文件
/// (含孤儿文件),并尽力 VACUUM 压缩数据库。
/// days = None 或 <=0 清除全部;>0 只清除 last_used_at 早于 N 天前的条目
/// (与保留天数自动清理的时间列保持一致)。
#[tauri::command]
pub async fn clear_history(
    app_handle: tauri::AppHandle,
    days: Option<i64>,
) -> Result<(), String> {
    let db = app_handle.state::<SqlitePool>();

    // 删除前先收集 content_hash:清空是"显式删除",必须给 peer 留墓碑,
    // 否则对端下次同步会把整批内容重新插回来(条件是下面 DELETE 的逐字副本)
    let hashes: Vec<String> = match days.filter(|d| *d > 0) {
        Some(days) => sqlx::query_scalar(
            "SELECT content_hash FROM items WHERE is_favorite = 0 \
             AND last_used_at < datetime('now', '-' || ? || ' days')",
        )
        .bind(days)
        .fetch_all(&*db)
        .await
        .map_err(|e| e.to_string())?,
        None => sqlx::query_scalar("SELECT content_hash FROM items WHERE is_favorite = 0")
            .fetch_all(&*db)
            .await
            .map_err(|e| e.to_string())?,
    };

    let result = match days.filter(|d| *d > 0) {
        Some(days) => {
            sqlx::query(
                "DELETE FROM items WHERE is_favorite = 0 \
                 AND last_used_at < datetime('now', '-' || ? || ' days')",
            )
            .bind(days)
            .execute(&*db)
            .await
            .map_err(|e| e.to_string())?
        }
        None => sqlx::query("DELETE FROM items WHERE is_favorite = 0")
            .execute(&*db)
            .await
            .map_err(|e| e.to_string())?,
    };

    if result.rows_affected() > 0 {
        // 收藏条目的图片保留,其余(含孤儿文件)清除
        crate::services::image_cleanup::remove_unreferenced_images(&app_handle, &*db).await;

        // 批量入队墓碑(空列表由 queue_tombstones 静默跳过)
        crate::services::sync::service::queue_tombstones(&app_handle, hashes);

        // 尽力回收数据库空间;若恰有并发连接占用导致失败,不影响清空结果
        if let Err(e) = sqlx::query("VACUUM").execute(&*db).await {
            eprintln!("VACUUM after clear_history failed: {}", e);
        }
    }

    let _ = app_handle.emit("clipboard-changed", serde_json::json!({"action": "cleared"}));
    Ok(())
}

#[tauri::command]
pub async fn toggle_favorite(app_handle: tauri::AppHandle, id: i64) -> Result<(), String> {
    let db = app_handle.state::<SqlitePool>();

    sqlx::query("UPDATE items SET is_favorite = NOT is_favorite WHERE id = ?")
        .bind(id)
        .execute(&*db)
        .await
        .map_err(|e| e.to_string())?;

    let _ = app_handle.emit(
        "clipboard-changed",
        serde_json::json!({"action": "updated", "id": id}),
    );
    Ok(())
}

#[tauri::command]
pub async fn pause_monitoring(app_handle: tauri::AppHandle, paused: bool) -> Result<(), String> {
    let monitor = app_handle.state::<crate::services::clipboard_monitor::ClipboardMonitor>();
    monitor.set_paused(paused).await;
    Ok(())
}

#[tauri::command]
pub async fn get_image_base64(
    app_handle: tauri::AppHandle,
    file_path: String,
) -> Result<String, String> {
    // 1. SECURITY: 图片路径必须落在应用 images 目录内。
    // 统一走 storage_service::resolve_image_path 守卫(读取/删除/粘贴三条
    // 路径同源),此前这里内联复制了一份同等逻辑,守卫一旦改动容易漏改
    let canonical =
        crate::services::storage_service::resolve_image_path(&app_handle, &file_path)
            .ok_or_else(|| "图片不存在或不在应用图片目录内,已拒绝读取".to_string())?;

    // 2. File size limit: reject files over 5MB
    const MAX_SIZE: u64 = 5 * 1024 * 1024;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| format!("Failed to read file metadata: {}", e))?;
    if metadata.len() > MAX_SIZE {
        return Err(format!(
            "File too large ({}MB exceeds 5MB limit)",
            metadata.len() / (1024 * 1024)
        ));
    }

    let bytes = tokio::fs::read(&canonical)
        .await
        .map_err(|e| format!("Failed to read file '{}': {}", file_path, e))?;

    // 3. Detect MIME type from magic bytes
    let mime = detect_image_mime(&bytes);

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(format!("data:{};base64,{}", mime, encoded))
}

/// 与采集端(ClipboardMonitor)完全一致的预览截断:前 100 个**字符**。
/// 用 chars() 而非按字节切片 —— 中文/emoji 下按字节切会切出半个字符。
pub(crate) fn build_preview(content: &str) -> String {
    content.chars().take(100).collect()
}

/// 编辑条目内容:同步更新 content 与 preview,并重算 content_hash,
/// 保证后续「内容去重」语义与采集端一致(否则编辑后同一内容会被当成新条目重复插入)。
/// 仅由前端的文本预览/编辑弹窗调用;图片条目前端不提供入口。
/// 编辑后旧 html/rtf 与新文本不再匹配,必须一并清除(置 NULL + 删文件),
/// 否则粘贴会投出"新文本+旧富格式"的错配内容。
#[tauri::command]
pub async fn update_item_content(
    app_handle: tauri::AppHandle,
    id: i64,
    content: String,
) -> Result<(), String> {
    let db = app_handle.state::<SqlitePool>();
    let preview = build_preview(&content);
    let hash = crate::utils::hash::compute_hash(&content);

    // 先取出旧格式路径,UPDATE 成功后删除对应文件
    let old_paths: Vec<(String,)> = sqlx::query_as(
        "SELECT html_path FROM items WHERE id = ? AND html_path IS NOT NULL \
         UNION ALL \
         SELECT rtf_path FROM items WHERE id = ? AND rtf_path IS NOT NULL",
    )
    .bind(id)
    .bind(id)
    .fetch_all(&*db)
    .await
    .map_err(|e| e.to_string())?;

    let result = sqlx::query(
        "UPDATE items SET content = ?, preview = ?, content_hash = ?, html_path = NULL, rtf_path = NULL WHERE id = ?",
    )
    .bind(&content)
    .bind(&preview)
    .bind(&hash)
    .bind(id)
    .execute(&*db)
    .await
    .map_err(|e| e.to_string())?;

    if result.rows_affected() == 0 {
        return Err("Item not found".to_string());
    }

    // 删除旧格式文件(守卫式删除,删不掉不阻塞编辑——列已置 NULL,
    // 恢复端自然降级纯文本;残留文件由孤儿清扫回收)
    for (path,) in &old_paths {
        crate::services::image_cleanup::remove_format_file(&app_handle, path);
    }

    // 让列表按新内容重查(前端监听 clipboard-changed 后整表刷新)
    let _ = app_handle.emit(
        "clipboard-changed",
        serde_json::json!({"action": "updated", "id": id}),
    );
    Ok(())
}

/// 把一段 HTML 片段包装成标准 CF_HTML 字节块。
///
/// 采集端是"原样保存"源应用给的 CF_HTML(不做解析不重构,保证无损往返);
/// 编辑后的片段没有现成外壳,必须由我们自己拼一份自洽的:
/// `<html><body><!--StartFragment-->…<!--EndFragment--></body></html>`。
/// 头部所有偏移量都是**字节**偏移(相对整块数据起始),不是字符偏移 ——
/// 含中文/emoji 时两者差得很远,按字符算会把片段切成半个 UTF-8 序列。
pub(crate) fn build_cf_html(fragment: &str) -> Vec<u8> {
    const MARK_START: &str = "<!--StartFragment-->";
    const MARK_END: &str = "<!--EndFragment-->";
    let body = format!(
        "<html><body>{}{}{}</body></html>",
        MARK_START, fragment, MARK_END
    );

    // 占位串长度必须与 format! 出的头部一致(偏移量固定 10 位),否则整体偏移错位
    let header_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let start_html = header_len;
    let end_html = header_len + body.len();
    let start_fragment = start_html + body.find(MARK_START).map_or(0, |i| i + MARK_START.len());
    let end_fragment = start_html + body.find(MARK_END).unwrap_or(body.len());

    let header = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        start_html, end_html, start_fragment, end_fragment
    );

    let mut out = header.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// 富文本所见即所得编辑后的保存:用前端清理过的 HTML **片段**
/// (body.innerHTML,不含 `<html>/<body>` 外壳)重写该条目的富格式,
/// 同时同步纯文本(content / preview / content_hash)。
///
/// 片段必须再过一道 `sanitize_clipboard_html`:前端清理只是第一层,
/// 后端不能信任 IPC 来的内容(与 get_rich_preview 出口净化同一套规则)。
///
/// 与 `update_item_content` 的区别:这里**保留** html_path(指向新文件)、
/// 只把 rtf_path 置 NULL。`paste_service::choose_text_write_path` 只要
/// html/rtf 任一存在就走 Rich 多格式写回,所以 HTML-only 也能正常投递
/// 文本 + HTML;旧 RTF 与新内容已不匹配,留着会投出错配格式。
#[tauri::command]
pub async fn update_item_rich_content(
    app_handle: tauri::AppHandle,
    id: i64,
    content: String,
    html: String,
) -> Result<(), String> {
    // 空内容(纯空白/只剩被删掉的危险标签)由前端改走 update_item_content,
    // 正常不会命中;命中时明确报错,不写入空格式
    let Some(fragment) = sanitize_clipboard_html(&html) else {
        return Err("富文本内容为空".to_string());
    };

    let bytes = build_cf_html(&fragment);

    // 文件名带毫秒时间戳:每次编辑都是新文件,不复用旧名(否则旧路径
    // 被覆盖后,DB 里另一条引用同一文件的条目会跟着变内容)
    let formats_dir = crate::services::storage_service::formats_dir(&app_handle);
    std::fs::create_dir_all(&formats_dir).map_err(|e| format!("创建格式目录失败: {}", e))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = formats_dir.join(format!("edit-{}-{}.html", id, millis));
    if let Err(e) = std::fs::write(&path, &bytes) {
        return Err(format!("写入富文本文件失败: {}", e));
    }
    let new_path = path.to_string_lossy().to_string();

    let db = app_handle.state::<SqlitePool>();
    let preview = build_preview(&content);
    let hash = crate::utils::hash::compute_hash(&content);

    // 先取出旧格式路径,UPDATE 成功后删除对应文件
    let old_paths: Vec<(String,)> = match sqlx::query_as(
        "SELECT html_path FROM items WHERE id = ? AND html_path IS NOT NULL \
         UNION ALL \
         SELECT rtf_path FROM items WHERE id = ? AND rtf_path IS NOT NULL",
    )
    .bind(id)
    .bind(id)
    .fetch_all(&*db)
    .await
    {
        Ok(paths) => paths,
        Err(e) => {
            // 写库前就失败:新文件没有任何条目引用,删掉避免成为孤儿
            let _ = std::fs::remove_file(&path);
            return Err(e.to_string());
        }
    };

    let result = sqlx::query(
        "UPDATE items SET content = ?, preview = ?, content_hash = ?, html_path = ?, rtf_path = NULL WHERE id = ?",
    )
    .bind(&content)
    .bind(&preview)
    .bind(&hash)
    .bind(&new_path)
    .bind(id)
    .execute(&*db)
    .await;

    let result = match result {
        Ok(result) => result,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e.to_string());
        }
    };

    if result.rows_affected() == 0 {
        let _ = std::fs::remove_file(&path);
        return Err("Item not found".to_string());
    }

    // 删除旧格式文件(守卫式删除,删不掉不阻塞编辑——列已指向新文件,
    // 残留文件由孤儿清扫回收)
    for (old,) in &old_paths {
        crate::services::image_cleanup::remove_format_file(&app_handle, old);
    }

    // 让列表按新内容重查(前端监听 clipboard-changed 后整表刷新)
    let _ = app_handle.emit(
        "clipboard-changed",
        serde_json::json!({"action": "updated", "id": id}),
    );
    Ok(())
}

/// 富文本预览的返回体。`html` 为 None 表示"这条没有可预览的富格式",
/// 前端据此隐藏「富文本 / 纯文本」切换,行为与纯文本条目完全一致。
#[derive(serde::Serialize)]
pub struct RichPreview {
    html: Option<String>,
}

/// 读取条目的 CF_HTML 原始字节 → 剥掉微软头取出片段 → 净化,交给前端
/// iframe 渲染。无格式(html_path 为 NULL)、守卫拒绝(路径越界)、文件
/// 已被删除、解析失败一律返回 html: None,不向前端抛错。
#[tauri::command]
pub async fn get_rich_preview(
    app_handle: tauri::AppHandle,
    id: i64,
) -> Result<RichPreview, String> {
    let db = app_handle.state::<SqlitePool>();

    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT html_path FROM items WHERE id = ?")
        .bind(id)
        .fetch_optional(&*db)
        .await
        .map_err(|e| e.to_string())?;

    let Some((Some(path),)) = row else {
        return Ok(RichPreview { html: None });
    };

    // 与粘贴读取同源的安全守卫:html_path 来自数据库,可能被改成任意路径,
    // 必须先确认它落在应用 formats 目录内
    let Some(canonical) =
        crate::services::storage_service::resolve_format_path(&app_handle, &path)
    else {
        return Ok(RichPreview { html: None });
    };

    let Ok(bytes) = tokio::fs::read(&canonical).await else {
        return Ok(RichPreview { html: None });
    };

    let html = extract_fragment(&bytes).and_then(|fragment| sanitize_clipboard_html(&fragment));
    Ok(RichPreview { html })
}

/// CF_HTML 头部的片段偏移字段(微软包装格式,大小写不敏感)
static RE_START_FRAGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)StartFragment:(\d+)").unwrap());
static RE_END_FRAGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)EndFragment:(\d+)").unwrap());

/// 从 CF_HTML 原始字节中取出正文片段。
///
/// 原始字节**原样保留**了微软 CF_HTML 头(采集端有意不解析不重构,保证
/// 粘贴时无损往返),预览必须先剥掉头部,否则 iframe 里会先渲染一堆
/// "Version:0.9 StartHTML:..." 文本。
/// 偏移量是相对整块数据起始的**字节**偏移,所以必须在字节上切片 ——
/// 换成按字符切,含中文/emoji 的片段会被切坏。
pub(crate) fn extract_fragment(raw: &[u8]) -> Option<String> {
    if raw.is_empty() {
        return None;
    }

    // 头部字段全是 ASCII,偏移量本身是数字;这里只为取出数字,
    // 真正切片仍回到原始字节上做
    let text = String::from_utf8_lossy(raw);
    if let (Some(start_cap), Some(end_cap)) = (
        RE_START_FRAGMENT.captures(&text),
        RE_END_FRAGMENT.captures(&text),
    ) {
        if let (Ok(start), Ok(end)) = (start_cap[1].parse::<usize>(), end_cap[1].parse::<usize>()) {
            if start < end && end <= raw.len() {
                let fragment = String::from_utf8_lossy(&raw[start..end]).trim().to_string();
                if !fragment.is_empty() {
                    return Some(fragment);
                }
            }
        }
    }

    // 兜底:偏移缺失/越界/为空时,丢掉头部直到第一个 '<'。
    // 宁可多丢一点头部,也不要让整块 CF_HTML 头混进预览
    let index = text.find('<')?;
    let fragment = text[index..].trim();
    if fragment.is_empty() {
        None
    } else {
        Some(fragment.to_string())
    }
}

/// 整块 `<script>...</script>`(跨行、大小写不敏感)
static RE_SCRIPT_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>").unwrap());
/// 残留的裸 script 标签(未闭合的 `<script>`、孤立的 `</script>`)
static RE_SCRIPT_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)</?script\b[^>]*>").unwrap());
/// 导航/资源类标签,整标签(含属性)删除。
/// meta-refresh 能导航 sandbox iframe **自身**(sandbox 只拦顶层导航与弹窗,
/// 不拦 iframe 自己跳转),CSP 的 default-src 也不约束导航(navigate-to 已从
/// 规范移除);base/link 会改变后续解析基准或引入外部资源;iframe/object/embed
/// 是可加载外部文档的容器。这些标签在剪贴板富文本里都没有需要保留的内容,
/// 整标签删掉即可(闭合标签一并删,不留悬空 end tag)。
static RE_NAV_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)</?(?:meta|base|link|iframe|object|embed)\b[^>]*>").unwrap()
});
/// on* 事件属性(onerror/onclick/onload...),带引号与不带引号都要覆盖
static RE_EVENT_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\son[a-z]+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)"#).unwrap());
/// 无前导空白的 on* 属性:`src="x"onerror=...`(收尾引号后紧跟)与
/// `<img/onerror=...>`(HTML5 词法把引号后、`/` 后的非 `>` 字符重新解析为
/// 新的属性名,所以这两种写法都真实可触发)。
/// 三点讲究:前导分隔符用 `$1` 回填,否则会把上一个属性的收尾引号吃掉;
/// 前导类不含 `>` —— `>` 是标签结束符、引不出属性,放进去只会把正文里的
/// `>on<字母>=`(如 `<p>x>online=5</p>`)误删;
/// 调用前必须先过 protect_quoted_values —— 正则没有 lookbehind,判断不了
/// "当前位置是否在引号内",否则 data: URI 里恰好出现 `[/"']on<字母>=`
/// 子串时会被当成属性,把属性收尾引号一起吞掉,内嵌图片直接变坏图。
static RE_EVENT_ATTR_TIGHT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(["'/])on[a-z]+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>"']+)"#).unwrap()
});
/// 带引号的值(`"..."` / `'...'`),用于事件属性净化前的临时保护
static RE_QUOTED_VALUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'"#).unwrap());

/// 把带引号的值摘成 `"\u{1}<序号>\u{1}"` 占位符,返回(保护后的文本, 原文列表)。
/// 外侧引号必须保留:它同时是"下一个属性开始"的分隔符,抹掉会让
/// `"x"onerror=` 这种无空白写法失去可识别的前导字符。
fn protect_quoted_values(input: &str) -> (String, Vec<String>) {
    let mut saved: Vec<String> = Vec::new();
    let protected = RE_QUOTED_VALUE
        .replace_all(input, |caps: &regex::Captures| {
            let value = caps[0].to_string();
            let quote = value.chars().next().unwrap_or('"');
            saved.push(value);
            format!("{q}\u{1}{i}\u{1}{q}", q = quote, i = saved.len() - 1)
        })
        .into_owned();
    (protected, saved)
}

/// 按序号还原占位符。用 replace 而非按位置替换:占位符可能已随被删内容
/// 一起消失(那时这里自然什么也找不到)
fn restore_quoted_values(input: &str, saved: &[String]) -> String {
    let mut out = input.to_string();
    for (index, value) in saved.iter().enumerate() {
        let quote = value.chars().next().unwrap_or('"');
        out = out.replace(&format!("{q}\u{1}{i}\u{1}{q}", q = quote, i = index), value);
    }
    out
}
/// javascript 词干:允许词内混入制表/换行,URL 解析器会先剥掉它们,
/// 故 `java\tscript:` 与 `javascript:` 等价。
const JS_STEM: &str = r"j[\t\n\r]*a[\t\n\r]*v[\t\n\r]*a[\t\n\r]*s[\t\n\r]*c[\t\n\r]*r[\t\n\r]*i[\t\n\r]*p[\t\n\r]*t[\t\n\r]*:";
/// 数字字符引用:十进制 `&#106;` / 十六进制 `&#x6a;`,**分号可缺**
/// (HTML 规范里缺分号仍会解码,`&#106avascript:` 与 `javascript:` 等价)。
/// 两组分别捕获十进制与十六进制数字,由 decode_j_entities 判断码点。
static RE_NUMERIC_CHAR_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)&#(?:([0-9]+)|x([0-9a-f]+));?").unwrap());

/// 只把 j / J 的数字字符引用还原成 `j`(还原后整串就能落进下面的
/// javascript: 匹配,不需要通用实体解码器),其余数字字符引用原样保留。
///
/// 为什么必须按码点判断、而不是直接写 `&#(?:0*106|...);?`:分号可选后,
/// `&#1065;`(Щ)会被前缀匹配吃掉变成 `j5;`,`&#741;` 同理 —— 正常文本被篡改。
/// 正则一次吃下整串数字,码点不等于 106/74 就原样回填,既不漏攻击也不误伤。
fn decode_j_entities(input: &str) -> String {
    RE_NUMERIC_CHAR_REF
        .replace_all(input, |caps: &regex::Captures| {
            let code = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .or_else(|| {
                    caps.get(2)
                        .and_then(|m| u32::from_str_radix(m.as_str(), 16).ok())
                });
            match code {
                // j = 0x6A = 106,J = 0x4A = 74
                Some(106) | Some(74) => "j".to_string(),
                _ => caps[0].to_string(),
            }
        })
        .into_owned()
}
/// href/src 里的 javascript: URL(词干允许 tab/换行混入)
static RE_JS_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)\b(href|src)\s*=\s*(?:"\s*{stem}[^"]*"|'\s*{stem}[^']*'|{stem}[^\s>]+)"#,
        stem = JS_STEM
    ))
    .unwrap()
});
/// img 标签(用于逐标签处理 src)
static RE_IMG_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<img\b[^>]*>").unwrap());
static RE_SRC_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap()
});

/// 净化 CF_HTML 片段,供前端 iframe 渲染。
///
/// 这是**第二层**防御(第一层是前端 `sandbox=""` 的 iframe):即使净化
/// 有疏漏,iframe 里也没有脚本权限、没有同源、不能提交表单、不能导航顶层。
/// 但 sandbox 挡不住 iframe **自身**的导航(meta-refresh),所以导航类标签
/// 必须在净化里删掉。顺序按危害从大到小:字符引用还原 → 删脚本 → 删导航
/// 类标签 → 删事件属性 → 中和 javascript: URL → 处理图片外链。
/// 宁可保守多删 —— 富文本预览丢一点格式,好过执行脚本或对外发请求。
pub(crate) fn sanitize_clipboard_html(html: &str) -> Option<String> {
    if html.trim().is_empty() {
        return None;
    }

    // 1. 先把 j/J 的数字字符引用还原成 j(分号可缺),否则
    //    "&#106;avascript:" / "&#106avascript:" 都能绕过后面的 javascript: 匹配
    let mut out = decode_j_entities(html);
    // 2. 成对的 <script>...</script>(先删整块,避免只删标签留下裸脚本)
    out = RE_SCRIPT_BLOCK.replace_all(&out, "").into_owned();
    // 3. 未闭合/孤立的 script 标签
    out = RE_SCRIPT_TAG.replace_all(&out, "").into_owned();
    // 4. 导航/资源类标签(meta-refresh 可让预览 iframe 自己跳转到外部地址)
    out = RE_NAV_TAG.replace_all(&out, "").into_owned();
    // 5+6. on* 事件属性(带前导空白的常规写法 + 引号后/斜杠后紧跟的写法)。
    //      先把带引号的值摘成占位符再删属性:否则 data: URI 里恰好出现
    //      `[/"']on<字母>=` 子串时会被误判成属性、连收尾引号一起吃掉
    //      (内嵌图片变坏图)。副作用是"引号内的 on*= 文本"不再被误删。
    let (protected, quoted) = protect_quoted_values(&out);
    let mut cleaned = RE_EVENT_ATTR.replace_all(&protected, "").into_owned();
    cleaned = RE_EVENT_ATTR_TIGHT.replace_all(&cleaned, "$1").into_owned();
    out = restore_quoted_values(&cleaned, &quoted);
    // 7. javascript: URL(href/src),值整体置空
    out = RE_JS_URL.replace_all(&out, "$1=\"\"").into_owned();
    // 8. 图片:内嵌 data:image/* 保留渲染,file:// 与 http(s):// 外链 src 置空
    //    (外链图片在源应用关闭后本就 404,且不该让预览去访问外部地址)
    out = RE_IMG_TAG
        .replace_all(&out, |caps: &regex::Captures| {
            RE_SRC_ATTR
                .replace_all(&caps[0], |src: &regex::Captures| {
                    let value = src
                        .get(1)
                        .or_else(|| src.get(2))
                        .or_else(|| src.get(3))
                        .map(|m| m.as_str())
                        .unwrap_or("");
                    if value.trim_start().to_ascii_lowercase().starts_with("data:image/") {
                        src[0].to_string()
                    } else {
                        "src=\"\"".to_string()
                    }
                })
                .into_owned()
        })
        .into_owned();

    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn detect_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 4 && bytes[..4] == [0x89, 0x50, 0x4E, 0x47] {
        "image/png"
    } else if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        "image/jpeg"
    } else if bytes.len() >= 4 && bytes[..4] == *b"GIF8" {
        "image/gif"
    } else if bytes.len() >= 12 && bytes[..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        "image/webp"
    } else {
        "image/png"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_truncates_on_char_boundary() {
        let preview = build_preview(&"验".repeat(150));
        assert_eq!(preview.chars().count(), 100);
        assert_eq!(preview, "验".repeat(100));
    }

    #[test]
    fn preview_keeps_short_content() {
        assert_eq!(build_preview("hello"), "hello");
    }

    // ---------- 富文本预览:CF_HTML 片段提取 ----------

    /// 拼一份合规 CF_HTML:头部偏移量按**字节**实算,用 find 定位而非
    /// 复用解析端那套正则,两边算法相互独立
    fn wrap_cf_html(body: &str) -> Vec<u8> {
        const MARK_START: &str = "<!--StartFragment-->";
        const MARK_END: &str = "<!--EndFragment-->";
        // 占位串长度必须与 format! 出的头部一致(偏移量固定 10 位)
        let head_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
        let start_frag = head_len + body.find(MARK_START).unwrap() + MARK_START.len();
        let end_frag = head_len + body.find(MARK_END).unwrap();
        let head = format!(
            "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
            head_len,
            head_len + body.len(),
            start_frag,
            end_frag
        );
        format!("{}{}", head, body).into_bytes()
    }

    #[test]
    fn extract_fragment_reads_cf_html_offsets() {
        let raw = wrap_cf_html("<html><body><!--StartFragment--><b>hi</b><!--EndFragment--></body></html>");
        assert_eq!(extract_fragment(&raw).as_deref(), Some("<b>hi</b>"));
    }

    /// 真实生产者样例(手写偏移量,不信赖测试辅助函数)
    #[test]
    fn extract_fragment_parses_literal_cf_html_header() {
        let raw = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000178\r\nStartFragment:0000000137\r\nEndFragment:0000000146\r\n<html><body><!--StartFragment--><b>hi</b><!--EndFragment--></body></html>";
        assert_eq!(extract_fragment(raw.as_bytes()).as_deref(), Some("<b>hi</b>"));
    }

    /// 偏移量按字节计:含中文的片段不能按字符切坏
    #[test]
    fn extract_fragment_keeps_utf8_multibyte() {
        let raw = wrap_cf_html("<html><body><!--StartFragment--><b>你好</b><!--EndFragment--></body></html>");
        assert_eq!(extract_fragment(&raw).as_deref(), Some("<b>你好</b>"));
    }

    #[test]
    fn extract_fragment_falls_back_when_offsets_bogus() {
        let raw = "Version:0.9\r\nStartFragment:999999\r\nEndFragment:9999999\r\n<html><body>ok</body></html>";
        assert_eq!(
            extract_fragment(raw.as_bytes()).as_deref(),
            Some("<html><body>ok</body></html>")
        );
    }

    #[test]
    fn extract_fragment_rejects_empty_and_markup_free_input() {
        assert!(extract_fragment(b"").is_none());
        assert!(extract_fragment(b"no markup here").is_none());
    }

    // ---------- 富文本编辑:CF_HTML 构造 ----------

    /// 构造出的字节块必须能被解析端原样取回片段;含中文时字节偏移
    /// 与字符偏移相差很大,按字符算会切坏 UTF-8
    #[test]
    fn build_cf_html_roundtrips_utf8_fragment() {
        let fragment = "<p><b>你好</b></p>";
        let raw = build_cf_html(fragment);
        assert_eq!(extract_fragment(&raw).as_deref(), Some(fragment));
        assert!(
            std::str::from_utf8(&raw).is_ok(),
            "构造结果必须是合法 UTF-8"
        );
    }

    /// 多标签 + 换行 + emoji 仍能往返(偏移量按字节累加,不依赖片段结构)
    #[test]
    fn build_cf_html_roundtrips_multiline_fragment() {
        let fragment = "<div>第一行<br>\n<span style=\"color:red\">红 🎉</span></div>\n<p>尾</p>";
        let raw = build_cf_html(fragment);
        assert_eq!(extract_fragment(&raw).as_deref(), Some(fragment));
    }

    /// 头部四个偏移量按字节自洽:StartHTML/EndHTML 覆盖整个 body,
    /// 片段区间正好落在标记之间
    #[test]
    fn build_cf_html_offsets_are_byte_accurate() {
        let fragment = "<p>你好</p>";
        let raw = build_cf_html(fragment);
        let text = String::from_utf8(raw.clone()).unwrap();

        let field = |name: &str| -> usize {
            let at = text.find(&format!("{}:", name)).unwrap() + name.len() + 1;
            text[at..at + 10].parse().unwrap()
        };
        let start_html = field("StartHTML");
        let end_html = field("EndHTML");
        let start_fragment = field("StartFragment");
        let end_fragment = field("EndFragment");

        // body 从 StartHTML 开始,到 EndHTML 结束
        assert!(text[start_html..].starts_with("<html><body>"));
        assert_eq!(&text[end_html - 7..end_html], "</html>");
        // 片段区间就是原文(字节切片,不 trim)
        assert_eq!(&text[start_fragment..end_fragment], fragment);
        assert!(start_fragment < end_fragment);
        assert!(end_fragment <= end_html);
    }

    // ---------- 富文本预览:净化 ----------

    #[test]
    fn sanitize_removes_script_blocks() {
        for input in [
            "<p>a</p><script>alert(1)</script>",
            "<p>a</p><SCRIPT>alert(1)</SCRIPT>",
            "<p>a</p><script\n  type=\"text/javascript\">\nalert(1)\n</script\n>",
            "<div>a<script>b",
        ] {
            let out = sanitize_clipboard_html(input).unwrap();
            assert!(!out.to_lowercase().contains("script"), "残留 script: {}", out);
            assert!(!out.contains("alert"), "残留脚本载荷: {}", out);
        }
    }

    #[test]
    fn sanitize_removes_event_handler_attributes() {
        let out = sanitize_clipboard_html(
            "<img src=\"data:image/png;base64,AAA\" onerror=\"alert(1)\">\
             <a href=\"#\" onclick='x()'>t</a><body ONLOAD=y()>",
        )
        .unwrap();
        let lower = out.to_lowercase();
        assert!(!lower.contains("onerror"), "残留 onerror: {}", out);
        assert!(!lower.contains("onclick"), "残留 onclick: {}", out);
        assert!(!lower.contains("onload"), "残留 onload: {}", out);
        assert!(
            out.contains("data:image/png;base64,AAA"),
            "事件属性净化不能误删内嵌图片: {}",
            out
        );
    }

    #[test]
    fn sanitize_removes_javascript_urls() {
        for input in [
            "<a href=\"javascript:alert(1)\">x</a>",
            "<a href='javascript:alert(1)'>x</a>",
            "<a href=javascript:alert(1)>x</a>",
            "<img src=\"JaVaScRiPt:alert(1)\">",
        ] {
            let out = sanitize_clipboard_html(input).unwrap();
            assert!(
                !out.to_lowercase().contains("javascript:"),
                "残留 javascript: URL: {}",
                out
            );
            assert!(!out.contains("alert"), "残留脚本载荷: {}", out);
        }
    }

    #[test]
    fn sanitize_keeps_embedded_data_images() {
        let out =
            sanitize_clipboard_html("<p>x</p><img src=\"data:image/png;base64,iVBORw0KGgo=\">")
                .unwrap();
        assert!(
            out.contains("data:image/png;base64,iVBORw0KGgo="),
            "内嵌 base64 图片必须保留: {}",
            out
        );
    }

    #[test]
    fn sanitize_strips_external_image_src() {
        for input in [
            "<img src=\"file:///C:/tmp/a.png\">",
            "<img src=\"http://example.com/a.png\">",
            "<img src=\"https://example.com/a.png\">",
            "<img src='file:///D:/b.png' alt=\"c\">",
        ] {
            let out = sanitize_clipboard_html(input).unwrap();
            assert!(!out.contains("http"), "外链图片必须剥除: {}", out);
            assert!(!out.contains("file://"), "本地外链图片必须剥除: {}", out);
            assert!(out.contains("src=\"\""), "外链 src 应置空: {}", out);
        }
    }

    #[test]
    fn sanitize_keeps_benign_rich_text() {
        let out =
            sanitize_clipboard_html("<b>粗体</b><span style=\"color:red\">红</span>").unwrap();
        assert!(out.contains("<b>粗体</b>"), "正常富文本不能被误删: {}", out);
        assert!(out.contains("color:red"), "内联样式不能被误删: {}", out);
    }

    /// 导航/资源类标签必须整标签删除:meta-refresh 能导航 sandbox iframe
    /// 自身(sandbox 只拦顶层导航,不拦 iframe 自己跳转),base/link 会改变
    /// 解析基准或引入外部资源,iframe/object/embed 是可加载外部文档的容器。
    /// 这些标签在剪贴板富文本里都没有需要保留的内容。
    #[test]
    fn sanitize_removes_navigating_tags() {
        for (input, marker) in [
            (
                "<meta http-equiv=\"refresh\" content=\"0;url=http://evil.example\">",
                "meta",
            ),
            (
                "<META HTTP-EQUIV='refresh' CONTENT='0;URL=file:///C:/x'>",
                "meta",
            ),
            ("<base href=\"http://evil.example/\">", "base"),
            (
                "<link rel=\"stylesheet\" href=\"http://evil.example/a.css\">",
                "link",
            ),
            ("<iframe src=\"http://evil.example\"></iframe>", "iframe"),
            ("<object data=\"http://evil.example\"></object>", "object"),
            ("<embed src=\"http://evil.example\">", "embed"),
        ] {
            let out = sanitize_clipboard_html(&format!("<p>t</p>{}", input)).unwrap();
            let lower = out.to_lowercase();
            assert!(
                !lower.contains(&format!("<{}", marker)),
                "残留 <{}> 标签: {}",
                marker,
                out
            );
            assert!(!lower.contains("http"), "残留外链地址: {}", out);
            assert!(!lower.contains("file://"), "残留 file:// 地址: {}", out);
            assert!(out.contains("<p>t</p>"), "正文不能被误删: {}", out);
        }
    }

    /// 无前导空白的 on* 属性:HTML5 词法把引号后紧跟的字符、以及 `/` 后的
    /// 非 `>` 字符重新解析为新的属性名,所以 `"x"onerror=` 与 `<img/onerror=`
    /// 都是真实可触发的写法。
    #[test]
    fn sanitize_removes_unspaced_event_attributes() {
        for input in [
            "<img src=\"x\"onerror=alert(1)>",
            "<img/onerror=alert(1) src=\"x\">",
            "<a href=\"#\"onmouseover='x()'>t</a>",
            "<img src='y'onload=alert(1)>",
        ] {
            let out = sanitize_clipboard_html(input).unwrap();
            let lower = out.to_lowercase();
            for attr in ["onerror", "onmouseover", "onload"] {
                assert!(!lower.contains(attr), "残留 {}: {}", attr, out);
            }
            assert!(!out.contains("alert"), "残留脚本载荷: {}", out);
        }
    }

    /// javascript: 词干内混入 tab/换行(URL 解析器会先剥掉)、以及 j 的
    /// 字符引用编码(`&#106;` / `&#x6a;` / `&#74;` / `&#x4a;`)
    #[test]
    fn sanitize_removes_obfuscated_javascript_urls() {
        for input in [
            "<a href=\"java\tscript:alert(1)\">x</a>",
            "<a href=\"java\nscript:alert(1)\">x</a>",
            "<a href=\"&#106;avascript:alert(1)\">x</a>",
            "<a href=\"&#x6a;avascript:alert(1)\">x</a>",
            "<a href=\"&#74;avascript:alert(1)\">x</a>",
            "<a href=\"&#x4A;avascript:alert(1)\">x</a>",
        ] {
            let out = sanitize_clipboard_html(input).unwrap();
            let lower = out.to_lowercase();
            assert!(!lower.contains("javascript"), "残留 javascript: URL: {}", out);
            assert!(!out.contains("alert"), "残留脚本载荷: {}", out);
        }
    }

    /// 误删保护(边界):data: URI 里出现 `[/"']on<字母>=` 这样的子串时,
    /// 不能把属性收尾引号一起吞掉 —— 那会让内嵌图片变成坏图。
    /// 引号内的 `on*= ` 只是文本,不是属性,同样不该被删。
    #[test]
    fn sanitize_keeps_data_uri_with_on_like_substring() {
        let input = "<img src=\"data:image/png;base64,iVBORw0KGgoAAAANSUhEUg/ona=\" alt=\"图\">";
        let out = sanitize_clipboard_html(input).unwrap();
        assert!(
            out.contains("src=\"data:image/png;base64,iVBORw0KGgoAAAANSUhEUg/ona=\""),
            "内嵌图片的 data: URI 不能被事件属性净化截断: {}",
            out
        );
        assert!(out.contains("alt=\"图\""), "后续属性不能被误删: {}", out);

        let quoted_text = "<span title=\"a onclick=b\">文字</span>";
        let out = sanitize_clipboard_html(quoted_text).unwrap();
        assert!(
            out.contains("title=\"a onclick=b\""),
            "引号内的 on*= 只是文本,不该被当属性删除: {}",
            out
        );
    }

    /// 数字字符引用缺分号时 HTML 仍会解码(`&#106avascript:` 等价于
    /// `javascript:`),所以 j/J 的还原必须容忍缺分号。
    #[test]
    fn sanitize_removes_semicolonless_j_entities() {
        for input in [
            "<a href=\"&#106avascript:alert(1)\">x</a>",
            "<a href=\"&#74avascript:alert(1)\">x</a>",
            "<a href=\"&#0106avascript:alert(1)\">x</a>",
        ] {
            let out = sanitize_clipboard_html(input).unwrap();
            let lower = out.to_lowercase();
            assert!(!lower.contains("avascript"), "残留 javascript: URL: {}", out);
            assert!(!out.contains("alert"), "残留脚本载荷: {}", out);
        }
    }

    /// 误删保护:`>` 结束标签、引不出属性,正文里的 `>on<字母>=` 与以
    /// j/J 编码为前缀的其它数字字符引用都不能被改动。
    #[test]
    fn sanitize_keeps_text_that_is_not_an_attribute() {
        let out = sanitize_clipboard_html("<p>x>online=5</p>").unwrap();
        assert_eq!(out, "<p>x>online=5</p>", "正文里的 >on<字母>= 被误删: {}", out);

        // &#1065; / &#741; / &#1060; 是别的字符,不能被当成 j/J 的引用
        let refs = "<p>&#1065; &#741; &#1060;</p>";
        assert_eq!(
            sanitize_clipboard_html(refs).unwrap(),
            refs,
            "无关的数字字符引用被篡改"
        );
    }

    #[test]
    fn sanitize_rejects_empty_input() {
        assert!(sanitize_clipboard_html("").is_none());
        assert!(sanitize_clipboard_html("   \n  ").is_none());
    }
}
