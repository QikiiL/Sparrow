use crate::models::clipboard_type::ClipboardType;
use enigo::{Direction, Enigo, Key, Keyboard};
use sqlx::sqlite::SqlitePool;
use std::path::Path;
use std::thread;
use std::time::Duration;
use tauri::{Emitter, Manager};

/// 解析条目的富文本格式文件并读出原始字节:
/// - 带可读的 html/rtf 文件 → Some(字节)(多格式写回用);
/// - 文件缺失/读失败/越界 → None,降级走 set_text 纯文本路径,不报错中断粘贴。
fn resolve_rich_formats(
    app_handle: &tauri::AppHandle,
    html_path: Option<&str>,
    rtf_path: Option<&str>,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let resolve_and_read = |p: Option<&str>| -> Option<Vec<u8>> {
        let raw = p?;
        let canonical =
            crate::services::storage_service::resolve_format_path(app_handle, raw)?;
        std::fs::read(&canonical).ok()
    };
    (resolve_and_read(html_path), resolve_and_read(rtf_path))
}

/// 决定文本类条目的写入路径(纯逻辑,便于降级单测):
/// - (None, None) → Plain(文件缺失/读失败/条目本无格式,降级 set_text)
/// - 其余 → Rich(多格式原子写回)
///
/// 注意:只要任一格式可用就走 Rich,并整体写回(文本+可用格式)。
#[derive(Debug, PartialEq, Eq)]
enum TextWritePath {
    Plain,
    Rich,
}

fn choose_text_write_path(html: Option<&[u8]>, rtf: Option<&[u8]>) -> TextWritePath {
    if html.is_some() || rtf.is_some() {
        TextWritePath::Rich
    } else {
        TextWritePath::Plain
    }
}

/// 单击条目的目标行为
pub enum DeliverMode {
    /// 写入剪贴板后模拟 Ctrl+V,粘贴到 `target`(本次冻结的落点;
    /// 0 = 无明确落点,只要求前台不是本进程的窗口)
    Paste { target: isize },
    /// 仅写入剪贴板,不模拟按键
    CopyOnly,
}

/// 模拟 Ctrl+V 粘贴。
/// 镜像用户的物理 Ctrl 状态:连续快速触发时用户常"按住 Ctrl 连点 V",
/// 此时应用侧修饰键已经是按下态 —— 只补一个 V 即可,且结束时绝不发
/// Ctrl-up,否则会把用户手里还没松开的 Ctrl 提前"放掉",
/// 后续的裸 V 就会被目标应用打成字母 v
pub fn simulate_paste() -> Result<(), String> {
    let mut enigo = Enigo::new(&enigo::Settings::default()).map_err(|e| e.to_string())?;
    let ctrl_held_by_user = crate::utils::paste_hook::is_ctrl_physically_down();

    if !ctrl_held_by_user {
        enigo
            .key(Key::Control, Direction::Press)
            .map_err(|e| e.to_string())?;
    }
    enigo
        .key(Key::Unicode('v'), Direction::Press)
        .map_err(|e| e.to_string())?;
    enigo
        .key(Key::Unicode('v'), Direction::Release)
        .map_err(|e| e.to_string())?;
    if !ctrl_held_by_user {
        enigo
            .key(Key::Control, Direction::Release)
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// 将文本写入剪切板;若剪贴板内容已与之一致则跳过写入(返回 false),
/// 此时直接模拟 Ctrl+V 即可,避免重复复制。
/// 注意:带 html/rtf 的条目不能走此优化 —— 剪贴板可能已被覆盖成同文本的
/// 纯内容,跳过写入会让旧的多格式残留"顶包"粘出去,格式反而错乱。
fn write_text_if_changed(text: &str) -> Result<bool, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    if let Ok(current) = cb.get_text() {
        if current == text {
            return Ok(false);
        }
    }
    cb.set_text(text.to_string())
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// 多格式原子写回:一次 OpenClipboard 会话内同时写
/// CF_UNICODETEXT + "HTML Format"(字节原样) + "Rich Text Format"(字节原样)。
/// 必须在同一会话内 set 完所有格式 —— 分多次会话会互相覆盖,只剩最后一种。
/// pub(crate): 供捕获侧真机测试复用(读写两端同一套剪贴板操作,往返断言才有意义)。
pub(crate) fn write_rich_text_to_clipboard(
    text: &str,
    html: Option<&[u8]>,
    rtf: Option<&[u8]>,
) -> Result<(), String> {
    let register = |name: &str| -> Result<u32, String> {
        clipboard_win::raw::register_format(name)
            .map(|f| f.get())
            .ok_or_else(|| format!("无法注册剪贴板格式: {}", name))
    };

    // arboard 必须先 drop(剪贴板独占打开),再持 clipboard-win guard 写全部格式
    let _clip = clipboard_win::Clipboard::new_attempts(10)
        .map_err(|e| format!("Failed to open clipboard: {}", e))?;
    let cf_html = register("HTML Format")?;
    let cf_rtf = register("Rich Text Format")?;

    // 先清空再依次写;任一格式失败即报错,避免"只写进去一半"的静默错乱
    clipboard_win::raw::empty().map_err(|e| format!("Failed to empty clipboard: {}", e))?;
    clipboard_win::raw::set_string(text)
        .map_err(|e| format!("Failed to set CF_UNICODETEXT: {}", e))?;
    if let Some(h) = html {
        clipboard_win::raw::set_without_clear(cf_html, h)
            .map_err(|e| format!("Failed to set HTML Format: {}", e))?;
    }
    if let Some(r) = rtf {
        clipboard_win::raw::set_without_clear(cf_rtf, r)
            .map_err(|e| format!("Failed to set Rich Text Format: {}", e))?;
    }
    Ok(())
}

/// 将图片写入剪切板。file_path 必须是经 storage_service::resolve_image_path
/// 校验过的规范化绝对路径
fn write_image_to_clipboard(file_path: &Path) -> Result<(), String> {
    let img = image::open(file_path).map_err(|e| format!("Failed to decode image: {}", e))?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let img_data = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: std::borrow::Cow::Owned(rgba.into_raw()),
    };
    arboard::Clipboard::new()
        .map_err(|e| e.to_string())?
        .set_image(img_data)
        .map_err(|e| format!("Failed to set image on clipboard: {}", e))?;
    Ok(())
}

/// 将文件列表写入剪切板(CF_HDROP)
fn write_files_to_clipboard(paths: Vec<String>) -> Result<(), String> {
    // clipboard_win 的 set_clipboard 按值传参,与 FileList 的 Setter<[T]> 实现
    // 不兼容,因此持 guard 后直接调用 raw API
    let _clip = clipboard_win::Clipboard::new_attempts(10)
        .map_err(|e| format!("Failed to open clipboard: {}", e))?;
    clipboard_win::raw::set_file_list_with(&paths, clipboard_win::options::DoClear)
        .map_err(|e| format!("Failed to set file list on clipboard: {}", e))?;
    Ok(())
}

/// 投递指定条目内容:按模式写入剪贴板,Paste 模式额外模拟 Ctrl+V。
/// 调用前需先隐藏本应用窗口,确保按键落到目标应用上。
/// html_path/rtf_path 非空且文件可读时,文本分支改走多格式原子写回;
/// 文件缺失/读失败自动降级为纯文本,不报错中断粘贴。
///
/// `DeliverMode::Paste { target }` 携带本次粘贴冻结的落点(0 = 无明确落点)。
/// 模拟按键前用 [`crate::utils::input_focus::wait_for_paste_target`] 再校验
/// 一次前台,不匹配就**不发** Ctrl+V(剪贴板照常写入),宁可"只复制"也不
/// 粘进错误的软件。返回值 = 是否真的模拟了按键(false = 仅复制)。
pub async fn deliver_content(
    app_handle: tauri::AppHandle,
    content: &str,
    item_type: i32,
    file_path: Option<&str>,
    html_path: Option<&str>,
    rtf_path: Option<&str>,
    mode: DeliverMode,
) -> Result<bool, String> {
    let content = content.to_string();
    let file_path = file_path.map(|s| s.to_string());
    let html_path = html_path.map(|s| s.to_string());
    let rtf_path = rtf_path.map(|s| s.to_string());
    let (do_paste, target) = match mode {
        DeliverMode::Paste { target } => (true, target),
        DeliverMode::CopyOnly => (false, 0),
    };
    tokio::task::spawn_blocking(move || {
        let wrote = match item_type {
            t if t == ClipboardType::Text as i32 || t == ClipboardType::Link as i32 => {
                // 先读格式文件:读不到(缺失/越界/IO 失败)则整体降级纯文本
                let (html, rtf) =
                    resolve_rich_formats(&app_handle, html_path.as_deref(), rtf_path.as_deref());
                match choose_text_write_path(html.as_deref(), rtf.as_deref()) {
                    // 带格式的条目每次都完整写回,不做"文本相同跳过"优化:
                    // 剪贴板可能已被同文本的纯内容覆盖,跳过写入会把
                    // 别人的内容顶包粘出去(这正是原 bug 的掩盖因素)
                    TextWritePath::Rich => {
                        write_rich_text_to_clipboard(&content, html.as_deref(), rtf.as_deref())?;
                        true
                    }
                    TextWritePath::Plain => write_text_if_changed(&content)?,
                }
            }
            t if t == ClipboardType::Image as i32 => {
                let raw = file_path.as_deref().unwrap_or(&content);
                // 图片路径来自数据库,必须先校验落在 images 目录内才打开,
                // 与读取(get_image_base64)、删除(remove_image_file)一致;
                // 校验失败直接报错,不能降级当文本处理,否则守卫形同虚设
                let path = crate::services::storage_service::resolve_image_path(&app_handle, raw)
                    .ok_or_else(|| "图片路径无效或不在应用图片目录内,已拒绝粘贴".to_string())?;
                write_image_to_clipboard(&path)?;
                true
            }
            t if t == ClipboardType::File as i32 => {
                // 文件条目的 content 是换行分隔的路径列表
                let paths: Vec<String> = content
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect();
                if paths.is_empty() {
                    write_text_if_changed(&content)?
                } else {
                    write_files_to_clipboard(paths)?;
                    true
                }
            }
            _ => write_text_if_changed(&content)?,
        };

        if do_paste {
            // 写入剪贴板后等待其生效;跳过写入时也给焦点切换留一点余量
            thread::sleep(Duration::from_millis(if wrote { 50 } else { 30 }));
            // 最后一道闸:前台必须就是要粘贴的窗口。焦点没到位(目标被
            // 抢走/已关闭/前台仍是本面板)就不发按键 —— 剪贴板已写好,
            // 用户手动 Ctrl+V 仍然可用,绝不把内容粘进错误的软件
            if crate::utils::input_focus::wait_for_paste_target(target) {
                simulate_paste()?;
                Ok(true)
            } else {
                eprintln!(
                    "[paste] foreground is not paste target {} — copy only (Ctrl+V not sent)",
                    target
                );
                Ok(false)
            }
        } else {
            Ok(false)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 投递结果:Delivered = 已写入剪贴板,`pasted` 表示**是否真的模拟了按键**
/// (false = 仅复制:发送前校验没过,内容在剪贴板里但没发 Ctrl+V);
/// Missing = 条目已不存在(连续粘贴用它跳过被删除的条目)
pub enum Delivery {
    Delivered { pasted: bool },
    Missing,
}

/// 投递前的焦点处理方式。单击条目(activate_item)与连续粘贴共用此核心,
/// 差异只有这里:
/// - HideFirst(面板内点击,默认):先销毁窗口,把前台焦点还给本次冻结的
///   粘贴落点并等待其就绪,否则模拟的 Ctrl+V 会在焦点切换完成前落空;
///   「仅复制」时只销毁面板、**不**归还焦点;
/// - KeepOpen(「粘贴后保持打开」开启):窗口不销毁,仅把前台焦点交给
///   粘贴落点 —— 面板留在屏幕上,方便连续粘贴多条。落点同样取本次冻结值,
///   只有 `target == 0`(托盘/冷启动没有记录)才退回 Z 序下第一个可激活窗口
///   (对齐销毁路径的隐式激活);失败时退回 HideFirst 的销毁路径保证内容
///   仍被粘贴,只有该路径也失败才降级为仅复制(此刻前台仍是本面板,模拟
///   按键会落进自己的搜索框);
/// - AtTarget(连续粘贴的全局触发):前台已经是目标应用,不动焦点 ——
///   粘贴总是落进用户当前光标所在的输入框。
///
/// 「本次冻结的粘贴落点」见 `deliver_item_by_id` 开头的快照与
/// `utils::foreground_tracker`(最近外部前台窗口 > 唤出时捕获的目标 > 0)。
pub enum FocusHandling {
    HideFirst,
    KeepOpen,
    AtTarget,
}

/// 不销毁窗口,把前台焦点切到粘贴落点并等待其就绪,返回是否成功。
/// 「粘贴后保持打开」路径专用;销毁版见 release_focus_to_target。
/// 落点优先用本次冻结的目标。`target != 0` 时**只认它**(失败即交调用方降级);
/// `target == 0`(托盘/冷启动没有记录)才退回 Z 序下第一个可激活窗口 ——
/// 对齐销毁路径里 Windows 的隐式激活,保证粘贴落点与"关掉开关"时一致。
/// 全部失败返回 false,由调用方降级。
async fn refocus_target_keep_window(app_handle: &tauri::AppHandle, target: isize) -> bool {
    let our_hwnd = app_handle
        .get_webview_window("main")
        .map(|w| w.hwnd().map(|h| h.0 as isize).unwrap_or(0))
        .unwrap_or(0);
    tauri::async_runtime::spawn_blocking(move || {
        let mut candidate = 0;
        let result = if target != 0 && target != our_hwnd {
            // 有明确落点:不做 Z 序兜底 —— 兜底选出的窗口与 target 不同,
            // 发送前校验必然拦下,白折腾一轮;直接失败交调用方走销毁路径
            crate::utils::input_focus::restore_target_focus(target)
        } else if our_hwnd != 0 {
            // find_window_below 复用 is_valid_target(置顶/壳/桌面/cloaked/
            // 本进程/NOACTIVATE 全排除),再要求窗口是启用态
            candidate = crate::utils::input_focus::find_window_below(our_hwnd);
            candidate != 0 && crate::utils::input_focus::restore_target_focus(candidate)
        } else {
            false
        };
        // 诊断:落点决策打一行 stderr(tauri dev 终端可见;release 为窗口
        // 程序不落任何输出)。candidate=0 且成功 = 走的冻结目标
        eprintln!(
            "[keep-open] target={} self={} candidate={} focused={}",
            target, our_hwnd, candidate, result
        );
        result
    })
    .await
    .unwrap_or(false)
}

/// 「粘贴后保持打开」的连续粘贴启动/步进:只把焦点交给冻结落点,不销毁面板。
/// `target` 由调用方**一次性**冻结后传入(不要在这里再冻结一次:两次冻结会
/// 读到不同的跟踪快照)。`Some(0)` 允许走 Z 序兜底,`None` 由调用方拦截。
/// 返回 false = 交接失败,调用方决定是否退回销毁路径。
pub async fn focus_target_keep_window(app_handle: &tauri::AppHandle, target: isize) -> bool {
    // 焦点交接期间抑制 ensure_focus 抢回焦点,守卫随作用域结束解除
    let _handoff = FocusHandoffGuard::new();
    refocus_target_keep_window(app_handle, target).await
}

/// 销毁窗口并把前台焦点还给本次冻结的粘贴落点(不投递任何内容)。
/// 单击条目投递的"先隐藏"分支、连续粘贴「只武装、不立即粘贴」的
/// 启动路径,以及 keep-open 找不到落点时的降级路径共用。
/// 返回是否已把前台交给一个可用目标(供调用方决定要不要退回仅复制)
pub async fn release_focus_to_target(app_handle: &tauri::AppHandle, target: isize) -> bool {
    // 先销毁窗口(隐藏即销毁,WebView2 进程树随之退出),
    // 再主动把前台焦点还给落点窗口并等待其真正就绪;
    // 若不等待,后续模拟的 Ctrl+V 会在焦点切换完成前发出而落空
    crate::utils::window_manager::destroy_main_window(app_handle);
    if target != 0 {
        tauri::async_runtime::spawn_blocking(move || {
            crate::utils::input_focus::restore_target_focus(target)
        })
        .await
        .unwrap_or(false)
    } else {
        // 无冻结落点:靠 Windows 销毁窗口后的隐式激活。这里不能"睡一会儿再
        // 看一眼" —— 机器繁忙时销毁 + 隐式激活可能慢于固定延时,读到的还是
        // 自己正在销毁的窗口,于是静默降级成仅复制。复用发送前那套轮询,
        // 并且要求前台通过 is_valid_target(排除壳/桌面/cloaked/本进程)
        tauri::async_runtime::spawn_blocking(|| {
            crate::utils::input_focus::wait_for_paste_target(0)
        })
        .await
        .unwrap_or(false)
    }
}

/// 冻结本次粘贴的落点:以 `PasteMode` 里"热键唤出时捕获的目标"为兜底,
/// 优先用跟踪器记录的最近外部前台窗口(见 `utils::foreground_tracker`)。
///
/// 只读不写:冻结值全程按参数传递(见 [`DeliverMode::Paste`] /
/// [`release_focus_to_target`]),**不写回** `PasteMode` —— 写回会把
/// "唤出瞬间捕获值"永久污染成上次粘贴的应用,跟踪器失效时的回退就一路
/// 错到底。
///
/// 单击条目(deliver_item_by_id)与连续粘贴的启动路径共用。
/// 返回 `None` = 本次不该激活任何窗口(只复制),`Some(0)` = 没有记录但允许
/// 走老路径(Z 序兜底/隐式激活),`Some(hwnd)` = 明确落点。
pub fn freeze_target_from_state(app_handle: &tauri::AppHandle) -> Option<isize> {
    let captured = app_handle
        .state::<crate::utils::input_focus::PasteMode>()
        .target();
    crate::utils::foreground_tracker::freeze_paste_target(captured)
}

/// 焦点交接守卫:把前台交给外部目标窗口期间抑制 `ensure_focus` 抢回焦点。
/// 用 Drop 实现,保证任何 return 路径(含 `?` 早退与 panic)都会解除抑制 ——
/// 标志一旦永久残留,`ensure_focus` 将再也不会把焦点还给面板。
struct FocusHandoffGuard;

impl FocusHandoffGuard {
    fn new() -> Self {
        crate::utils::input_focus::begin_focus_handoff();
        Self
    }
}

impl Drop for FocusHandoffGuard {
    fn drop(&mut self) {
        crate::utils::input_focus::end_focus_handoff();
    }
}

/// 按 id 读取条目并投递。单击条目(activate_item)与连续粘贴共用此核心,
/// 焦点处理差异见 FocusHandling。
/// suppress_ms = 粘贴完成后抑制剪贴板监听的时长(盖过写入瞬间即可;
/// 单击路径 500ms,队列步进用更短的值保持节奏)
pub async fn deliver_item_by_id(
    app_handle: &tauri::AppHandle,
    id: i64,
    handling: FocusHandling,
    do_paste: bool,
    suppress_ms: u64,
) -> Result<Delivery, String> {
    // ① 冻结本次粘贴的落点 —— 必须在任何窗口隐藏/销毁/异步等待**之前**:
    //    用户点条目后前台已经是我们自己的面板,再也读不到他真正想去的地方。
    //    三态穿过调用方(见 foreground_tracker::freeze_paste_target):
    //    None = 本次只复制、不激活任何窗口;Some(0) = 无记录但允许 Z 序兜底/
    //    隐式激活;Some(hwnd) = 明确落点。
    //    仅复制(do_paste=false)不冻结 —— 不碰焦点,白跑 DWM/类名探测没意义。
    //    连续粘贴(AtTarget)不动焦点,落点就是用户当前光标处。
    let frozen = if do_paste {
        match handling {
            FocusHandling::HideFirst | FocusHandling::KeepOpen => {
                freeze_target_from_state(app_handle)
            }
            FocusHandling::AtTarget => Some(0),
        }
    } else {
        None
    };
    // 冻结不到落点 → 本次退化为仅复制:内容照写剪贴板,绝不发 Ctrl+V
    let mut do_paste = do_paste && frozen.is_some();
    let target = frozen.unwrap_or(0);

    let db = app_handle.state::<SqlitePool>();
    let row = sqlx::query_as::<
       _,
        (String, i32, Option<String>, Option<String>, Option<String>),
    >("SELECT content, type, file_path, html_path, rtf_path FROM items WHERE id = ?")
    .bind(id)
    .fetch_optional(&*db)
    .await
    .map_err(|e| e.to_string())?;

    let (content, item_type, file_path, html_path, rtf_path) = match row {
        Some(row) => row,
        None => return Ok(Delivery::Missing),
    };

    // 焦点交接抑制:作用域必须覆盖到 simulate_paste 之后(写剪贴板与随后的
    // 等待仍在 ensure_focus 的重试窗口内),故守卫活到函数返回,由 Drop 统一
    // 解除。AtTarget 不动焦点,无需抑制
    let _handoff = match handling {
        FocusHandling::HideFirst | FocusHandling::KeepOpen => Some(FocusHandoffGuard::new()),
        FocusHandling::AtTarget => None,
    };
    match handling {
        FocusHandling::HideFirst => {
            if do_paste {
                // 销毁窗口时 ensure_focus 线程会因窗口消失而自然退出,
                // 显式抑制让销毁前的最后几次 set_focus 也停手
                if !release_focus_to_target(app_handle, target).await {
                    // 焦点没能交给落点:不盲发 Ctrl+V(会粘进错误的窗口),
                    // 剪贴板已写入,按"仅复制"处理
                    eprintln!(
                        "[paste] focus handoff to target {} failed — copy only",
                        target
                    );
                    do_paste = false;
                }
            } else {
                // 「仅复制」/冻结不到落点:面板照旧关闭(保留"点一下有反应"的
                // 观感),但**绝不**归还焦点 / SetForegroundWindow —— 否则
                // "只复制"也会把前台切回唤出前的应用
                crate::utils::window_manager::destroy_main_window(app_handle);
            }
        }
        FocusHandling::KeepOpen => {
            // 仅复制 / 冻结不到落点:面板保持打开、不动焦点(N1:这里绝不能
            // 落到 refocus 的 Z 序兜底,否则"点了桌面"会变成"粘进随机窗口")
            if do_paste {
                if refocus_target_keep_window(app_handle, target).await {
                    // 交接焦点后重申本窗口置顶(幂等,SWP_NOACTIVATE 不抢焦点):
                    // 目标窗口被激活时可能把面板压到后面,视觉上像"被盖住"
                    let our_hwnd = app_handle
                        .get_webview_window("main")
                        .and_then(|w| w.hwnd().ok())
                        .map(|h| h.0 as isize)
                        .unwrap_or(0);
                    let _ = crate::utils::input_focus::force_topmost(our_hwnd);
                } else {
                    // 没有任何可用落点(桌面上没有其他可激活窗口等):前台仍是
                    // 本面板,模拟按键会落进自己的搜索框。这里不再静默降级为
                    // 仅复制(release 版无任何输出,用户只看到"点了没反应"),
                    // 改走销毁窗口的可靠路径 —— 窗口销毁后 Windows 隐式激活目标
                    // 窗口,内容一定能粘出去;该路径也失败才退回仅复制
                    eprintln!("[keep-open] no usable target — degrading to HideFirst path");
                    if !release_focus_to_target(app_handle, target).await {
                        eprintln!("[keep-open] HideFirst fallback failed too — copy only");
                        do_paste = false;
                    }
                }
            }
        }
        FocusHandling::AtTarget => {
            // 焦点已在目标应用;留一点余量,避免上一轮模拟按键尚在收尾
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
    }

    let monitor = app_handle.state::<crate::services::clipboard_monitor::ClipboardMonitor>();
    monitor.set_suppress(true).await;

    let mode = if do_paste {
        DeliverMode::Paste { target }
    } else {
        DeliverMode::CopyOnly
    };
    let result = deliver_content(
        app_handle.clone(),
        &content,
        item_type,
        file_path.as_deref(),
        html_path.as_deref(),
        rtf_path.as_deref(),
        mode,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(suppress_ms)).await;
    monitor.set_suppress(false).await;

    if result.is_ok() {
        if let Err(e) = sqlx::query("UPDATE items SET last_used_at = datetime('now') WHERE id = ?")
            .bind(id)
            .execute(&*db)
            .await
        {
            eprintln!("Failed to update copy count: {}", e);
        }
        if let Err(e) = app_handle.emit(
            "clipboard-changed",
            serde_json::json!({"action": "updated", "id": id}),
        ) {
            eprintln!("Failed to emit clipboard-changed event: {}", e);
        }
    }

    // 把"是否真的发了按键"带出去:连续粘贴据此决定要不要标记「已粘贴」
    // (以前一律当成功,UI 会显示"已粘贴 n/N"但实际只复制)
    result.map(|pasted| Delivery::Delivered { pasted })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 多格式原子写回后,剪贴板上应能按字节读回 "HTML Format"/"Rich Text Format"。
    /// 需要真实桌面会话(剪贴板 + 格式注册),在无头/CI 环境不可靠,标记 ignored;
    /// 逻辑分支(降级、守卫)由纯逻辑单测覆盖。手动验收(必须单线程,
    /// 多测试并行会抢占全局剪贴板互相污染):
    /// cargo test -- --ignored --test-threads=1 services::paste_service
    #[test]
    #[ignore = "需要交互式桌面会话的真机剪贴板,CI/无头环境不可靠"]
    fn clipboard_roundtrip_writes_all_formats() {
        let text = "混排内容 roundtrip";
        // 模拟 Word 写出的 CF_HTML:头部 + HTML 片段(原样字节)
        let html: &[u8] = b"Version:0.9\r\nStartHTML:0000000105\r\n<html><body>rich</body></html>";
        let rtf: &[u8] = b"{\\rtf1\\ansi rich}";

        write_rich_text_to_clipboard(text, Some(html), Some(rtf)).expect("多格式写回应成功");

        // 独立会话读回断言字节一致
        let _clip = clipboard_win::Clipboard::new_attempts(10).expect("打开剪贴板");
        let read_back = |name: &str| -> Vec<u8> {
            let fmt = clipboard_win::raw::register_format(name).expect("注册格式");
            clipboard_win::get(clipboard_win::formats::RawData(fmt.get())).expect("读回格式")
        };
        assert_eq!(read_back("HTML Format"), html.to_vec(), "HTML 字节必须无损往返");
        assert_eq!(read_back("Rich Text Format"), rtf.to_vec(), "RTF 字节必须无损往返");
        // 文本也必须在
        let cb_text = arboard::Clipboard::new().unwrap().get_text().expect("读回文本");
        assert_eq!(cb_text, text);
    }

    /// 纯文本写回应清掉扩展格式(empty 后只写文本),不残留 HTML Format。
    /// 真机剪贴板测试,与上面的往返测试一样需单线程运行。
    #[test]
    #[ignore = "需要真机剪贴板"]
    fn plain_text_write_leaves_formats_absent() {
        write_rich_text_to_clipboard("纯文本", None, None).expect("写文本应成功");
        let _clip = clipboard_win::Clipboard::new_attempts(10).expect("打开剪贴板");
        let html_fmt = clipboard_win::raw::register_format("HTML Format");
        if let Some(fmt) = html_fmt {
            assert!(
                !clipboard_win::is_format_avail(fmt.get()),
                "纯文本写回不应携带 HTML Format"
            );
        }
    }

    // ---------- 降级决策(纯逻辑) ----------

    /// html/rtf 文件都读不到(丢失/越界/IO 失败)→ 降级纯文本路径,不 panic
    #[test]
    fn degrade_to_plain_when_format_files_missing() {
        assert_eq!(choose_text_write_path(None, None), TextWritePath::Plain);
    }

    /// 只剩一种格式文件可读 → 仍走多格式写回(带可用的那种)
    #[test]
    fn rich_path_when_only_html_survives() {
        assert_eq!(
            choose_text_write_path(Some(b"<b>x</b>"), None),
            TextWritePath::Rich
        );
        assert_eq!(
            choose_text_write_path(None, Some(b"{\\rtf1}")),
            TextWritePath::Rich
        );
    }

    /// 两种都可用 → 多格式写回
    #[test]
    fn rich_path_when_both_formats_available() {
        assert_eq!(
            choose_text_write_path(Some(b"<b>x</b>"), Some(b"{\\rtf1}")),
            TextWritePath::Rich
        );
    }
}
