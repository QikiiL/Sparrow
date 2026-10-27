//! 短信验证码自动捕获。
//!
//! 实现方式(逆向调研 TeleLink 后采用的同款路径):
//! 手机收到短信 → 「链接至 Windows」转发 → Phone Link 弹 Windows toast →
//! 通知中心 → [本服务] UserNotificationListener 事件驱动捕获
//! (NotificationChanged 唤醒 + 5 秒兜底轮询,捕获延迟从秒级降到
//! 亚秒级)→ 过滤出 Phone Link 的通知 → 提取验证码 → 写剪贴板。
//!
//! 安卓与 iPhone 走同一条路径(都汇到 Phone Link 的 toast),无需手机侧
//! 安装任何第三方 App,数据全程本地。
//!
//! 两个关键设计(照抄 TeleLink 的成熟经验):
//! 1. 按 AUMID 过滤(`microsoft.yourphone` 前缀)而非窗口标题——
//!    AUMID 语言无关,显示名会随系统语言变(Phone Link / 手机连接)
//! 2. 去重键 (aumid, notification_id),且每轮用「通知中心当前仍存在的
//!    id 集合」修剪已见集合——通知被清除后同 id 复用不会误判为已见

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tauri::Emitter;
use tauri_plugin_notification::NotificationExt;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::Management::UserNotificationListener;
use windows::UI::Notifications::Management::UserNotificationListenerAccessStatus;
use windows::UI::Notifications::NotificationKinds;

use crate::services::identity;

/// 功能总开关。轮询线程常驻(与剪贴板监控同生命周期),靠这个标志决定
/// 每轮是否真正去读通知——关闭时零 WinRT 调用,不占资源
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 最近一次成功捕获的 Unix 时间戳(秒),给设置面板显示「最近捕获」,
/// 让用户能感知功能是否真的在工作(Phone Link 断连时不会报错,只会静默)
static LAST_CAPTURE: AtomicI64 = AtomicI64::new(0);

/// 捕获总次数(本次会话内),同上用于状态展示
static CAPTURE_COUNT: AtomicI64 = AtomicI64::new(0);

/// 已见通知的 (aumid, id) 集合。轮询线程独占访问即可,不必上锁
type SeenSet = HashSet<(String, u32)>;

/// Phone Link 的 AUMID 前缀(小写比较)。Phone Link 包名是
/// Microsoft.YourPhone_8wekyb3d8bbwe,toast 的 AUMID 形如
/// `Microsoft.YourPhone_8wekyb3d8bbwe!YourPhone`,前缀匹配即可覆盖
const PHONE_LINK_AUMID_PREFIX: &str = "microsoft.yourphone";

/// Windows 纪元(1601-01-01)与 Unix 纪元(1970-01-01)的秒差
const WIN_EPOCH_DIFF_SECS: u64 = 11_644_473_600;

/// 本会话起点,WinRT DateTime 的 100ns 刻度数(自 1601-01-01 起)。
/// 捕获门槛:创建时间晚于该点的通知才算"新"。启动播种靠内存已见集合,
/// 首轮查询一旦失败或返回空列表,通知中心里的旧验证码 toast 就会被
/// 兜底轮询全部当"新通知"重新捕获 —— 时间过滤是无条件兜底:
/// 软件关闭期间收到的验证码永远不属于本会话
fn session_start_ticks() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    ((now.as_secs() + WIN_EPOCH_DIFF_SECS) * 10_000_000 + now.subsec_nanos() as u64 / 100) as i64
}

pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn last_capture() -> i64 {
    LAST_CAPTURE.load(Ordering::Relaxed)
}

pub fn capture_count() -> i64 {
    CAPTURE_COUNT.load(Ordering::Relaxed)
}

/// 当前通知监听权限状态。字符串直接给前端:
/// allowed / denied / unspecified / unsupported(系统不支持该 API)
pub fn access_status() -> String {
    match UserNotificationListener::Current() {
        Ok(listener) => match listener.GetAccessStatus() {
            Ok(UserNotificationListenerAccessStatus::Allowed) => "allowed".to_string(),
            Ok(UserNotificationListenerAccessStatus::Denied) => "denied".to_string(),
            _ => "unspecified".to_string(),
        },
        Err(_) => "unsupported".to_string(),
    }
}

/// 请求通知监听权限(阻塞调用,命令层须放 spawn_blocking)。
/// 非打包(non-MSIX)桌面应用调用它通常不弹系统对话框,作用是把本应用
/// 注册进「设置>隐私和安全性>通知」的应用列表;真正放行仍需用户在
/// 设置里手动允许(TeleLink 的 SETUP 文档记录了同样的坑)
pub fn request_access_blocking() -> String {
    match UserNotificationListener::Current() {
        Ok(listener) => match listener.RequestAccessAsync() {
            Ok(op) => match op.get() {
                Ok(UserNotificationListenerAccessStatus::Allowed) => "allowed".to_string(),
                Ok(UserNotificationListenerAccessStatus::Denied) => "denied".to_string(),
                _ => "unspecified".to_string(),
            },
            Err(_) => "unspecified".to_string(),
        },
        Err(_) => "unsupported".to_string(),
    }
}

/// 启动捕获线程。线程常驻,失败只记日志不崩溃——权限被用户在系统设置里
/// 掐掉、OS 拒绝服务等都按「本轮拿不到」处理,稍后重试。
///
/// 事件驱动 + 兜底轮询:订阅 UserNotificationListener 的 NotificationChanged,
/// 通知一到立即唤醒主循环(延迟亚秒级);5 秒无事件则兜底轮询一次,
/// 防止事件丢失导致漏捕。事件订阅失败时降级为 2 秒纯轮询,功能不废
pub fn spawn(app_handle: tauri::AppHandle) {
    let builder = std::thread::Builder::new().name("sms-code".into());
    let _ = builder.spawn(move || {
        let listener = match UserNotificationListener::Current() {
            Ok(l) => l,
            Err(e) => {
                // Windows 10 早期版本没有 UserNotificationListener,功能整体不可用
                eprintln!("[sms-code] UserNotificationListener 不可用: {}", e);
                return;
            }
        };

        let mut seen: SeenSet = HashSet::new();
        // 会话起点:创建时间早于它的通知一律不捕获(见 session_start_ticks)
        let session_start = session_start_ticks();

        // 先初始轮询播种已见集合,再订阅事件(顺序不能反):启动时通知中心里
        // 已存在的旧 toast 记为已见但**不捕获**(capture=false)——否则每次
        // 进程启动,还留在通知中心的旧验证码 toast 都会被重新提取、写剪贴板、
        // 重复入库(与 sms-helper 播种逻辑同源,语义保持一致)
        if let Err(e) = poll_once(&listener, &mut seen, &app_handle, false, session_start) {
            eprintln!("[sms-code] 初始轮询失败(由兜底轮询稍后重试): {}", e);
        }

        // 订阅 NotificationChanged(封装成可重试函数):回调跑在 WinRT 线程池线程
        // 上,只往通道发一个唤醒信号,不做任何实际工作(剪贴板/前端事件都由主循环
        // 处理);回调绝不能 panic(跨 FFI 边界),send 失败同样静默忽略。
        //
        // ⚠️ 已知系统限制:无包身份(non-MSIX)的桌面应用订阅该事件必然报
        // 0x80070490(ERROR_NOT_FOUND)——事件要求应用具有 MSIX 身份,
        // 而轮询接口不需要。多个独立案例(Stack Overflow 74124560 等)确认。
        //
        // 重试策略:首次订阅失败时,先尝试注册稀疏包身份
        // (identity::ensure_identity_registered);注册成功则再订阅一次。
        // 注意:注册发生在本进程启动之后,进程身份在创建时确定,所以很可能
        // 「注册成功但本次进程订阅仍失败」——这时打日志提示重启后生效,降级轮询。
        // 失败的 handler 随闭包一并 drop(其中的 wake_tx 也 drop),rx 弃用,
        // 下次重试会新建 channel,因此封装成「订阅一次并返回 rx」的函数。
        let (wake_rx, event_driven) = match subscribe_event(&listener) {
            Ok(rx) => {
                // 订阅直接成功:身份可用,清除自动重启冷却标记
                identity::clear_relaunch_marker();
                (rx, true)
            }
            Err(e) => {
                eprintln!(
                    "[sms-code] NotificationChanged 订阅失败(无包身份应用的已知限制),尝试注册稀疏包身份: {}",
                    e
                );
                match identity::ensure_identity_registered() {
                    Ok(()) => {
                        // 重试订阅前先刷新过期注册:稀疏包版本或清单内容若与
                        // 当前不一致(如 0.2.1.4 → 0.2.1.5 新增 SmsHelper 条目),
                        // 旧注册会让激活 helper 的 AUMID 找不到对应条目
                        if let Err(stale_err) = identity::reregister_if_stale() {
                            eprintln!(
                                "[sms-code] 过期稀疏包注册刷新失败(继续重试订阅): {}",
                                stale_err
                            );
                        }
                        match subscribe_event(&listener) {
                        Ok(rx) => {
                            eprintln!("[sms-code] 注册稀疏包身份后事件订阅成功,事件驱动已启用");
                            // 身份确认可用:清除自动重启冷却标记,
                            // 之后每次直接启动 exe 都允许一次自动重启
                            identity::clear_relaunch_marker();
                            (rx, true)
                        }
                        Err(e2) => {
                            // 注册成功但当前进程无身份(身份在进程创建时确定)。
                            //
                            // ⚠️ 主程序带 requireAdministrator 清单,Shell 激活
                            // (ActivateApplication)只支持中等完整性启动,拉起提权
                            // exe 必报 0x80070032「不支持该请求」。所以不能重启
                            // 主进程,改为激活 asInvoker 的 sms-helper.exe
                            // (Helper_AUMID):helper 出生即带身份、订阅事件、
                            // 提取验证码后直接写剪贴板,主程序监控线程照常入库,
                            // 本线程转入监工循环(见 supervise_helper)
                            eprintln!(
                                "[sms-code] 稀疏包身份已注册,但当前进程无身份,激活 sms-helper: {e2}"
                            );
                            match identity::activate_helper() {
                                Ok(pid) => supervise_helper(pid),
                                Err(e) => {
                                    // 激活失败(如包注册异常/清单缺 SmsHelper 条目):
                                    // 打日志,退回 1s 纯轮询降级,功能不废
                                    eprintln!(
                                        "[sms-code] 激活 sms-helper 失败,降级为 1s 轮询: {e}"
                                    );
                                    (mpsc::channel::<()>().1, false)
                                }
                            }
                        }
                        }
                    }
                    Err(reg_err) => {
                        eprintln!(
                            "[sms-code] 稀疏包身份注册失败,降级为 1s 纯轮询: {}",
                            reg_err
                        );
                        (mpsc::channel::<()>().1, false)
                    }
                }
            }
        };

        // 事件驱动时 5s 无信号兜底轮询一次;降级模式 1s 纯轮询
        // (平均捕获延迟 ~0.5s;每次 poll 是一次轻量 COM 查询,1s 频率开销可忽略)
        let wait = if event_driven {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(1000)
        };
        // 上次 poll 失败的时刻,用于失败退避(至少隔 5s 再 poll)
        let mut last_error: Option<Instant> = None;

        loop {
            // 失败退避:距上次失败不足 5 秒就先补足等待,避免权限被拒时
            // 每次唤醒都发起注定失败的 WinRT 调用刷日志。只在循环开头短暂
            // sleep、不占用 recv——退避期间到达的唤醒信号留在通道里,
            // 退避结束立刻被处理,不会堆积延迟
            if let Some(t) = last_error {
                let remain = Duration::from_secs(5).saturating_sub(t.elapsed());
                if !remain.is_zero() {
                    std::thread::sleep(remain);
                }
            }

            match wake_rx.recv_timeout(wait) {
                Ok(()) => {
                    // 突发合并:一条短信可能连弹多条 toast,先让它们到齐,
                    // 再排空通道里积压的信号,合并成一次 poll
                    std::thread::sleep(Duration::from_millis(250));
                    while wake_rx.try_recv().is_ok() {}
                }
                // 兜底轮询 / 降级纯轮询:超时也往下走一轮
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // wake_tx 在事件回调闭包里、随 listener 常驻,不会 drop;
                // 真发生时按兜底节奏继续纯轮询,保持线程存活
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }

            // 总开关关闭:零 WinRT 调用,直接等下一个信号/超时
            if !is_enabled() {
                continue;
            }

            match poll_once(&listener, &mut seen, &app_handle, true, session_start) {
                Ok(_) => last_error = None,
                Err(e) => {
                    eprintln!("[sms-code] 轮询失败(权限或系统服务): {}", e);
                    last_error = Some(Instant::now());
                }
            }
        }
    });
}

/// 监工循环:主程序无身份、捕获工作全部由 sms-helper 承担,本线程只负责
/// 看 helper 死没死 —— 每 30 秒检查一次,退出就重新激活,永不返回
/// (线程常驻)。开关语义在这里不再需要:开关由 helper 读配置文件处理。
fn supervise_helper(first_pid: u32) -> ! {
    let mut pid = first_pid;
    eprintln!("[sms-code] sms-helper 已激活 (pid={pid}),本线程进入监工模式,捕获由 helper 负责");
    loop {
        std::thread::sleep(Duration::from_secs(30));
        if !process_alive(pid) {
            eprintln!("[sms-code] sms-helper (pid={pid}) 已退出,重新激活");
            match identity::activate_helper() {
                Ok(p) => {
                    eprintln!("[sms-code] sms-helper 已重新激活 (pid={p})");
                    pid = p;
                }
                Err(e) => {
                    eprintln!("[sms-code] 重新激活 sms-helper 失败(30 秒后重试): {e}");
                }
            }
        }
    }
}

/// 进程是否存活:OpenProcess(最小查询权限) + GetExitCodeProcess。
/// 打不开句柄按已退出处理;STILL_ACTIVE(259) = 存活
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // windows 0.61 未导出 STILL_ACTIVE(伪退出码),按 MSDN 值本地定义
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code: u32 = 0;
        let alive = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE;
        let _ = CloseHandle(h);
        alive
    }
}

/// 订阅一次 NotificationChanged,返回唤醒信号接收端。
///
/// 失败(`Err`)时 handler 随闭包一并 drop(其中的 `wake_tx` 也 drop),
/// 调用方应弃用本次创建的 `rx` 并重新调用本函数重试。
fn subscribe_event(
    listener: &UserNotificationListener,
) -> windows::core::Result<mpsc::Receiver<()>> {
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    listener.NotificationChanged(&TypedEventHandler::new(
        move |_listener, _args| {
            let _ = wake_tx.send(());
            Ok(())
        },
    ))?;
    Ok(wake_rx)
}

/// 一轮轮询。返回本轮新处理的验证码条数。
/// `capture=false` 时只播种已见集合、不捕获(启动时把通知中心里已有的
/// 旧 toast 标记为已见 —— 否则每次进程启动都会把旧验证码重复复制、入库)。
/// `session_start` 之前的创建的通知一律不捕获:播种靠内存集合,首轮查询
/// 失败/返回空时旧通知会漏标记,时间是靠得住的无条件兜底。
/// 已见集合按 TeleLink 的方式修剪:通知从通知中心消失后,同 id 的
/// 新通知(Windows 会复用小整数 id)不应被误判为已见
fn poll_once(
    listener: &UserNotificationListener,
    seen: &mut SeenSet,
    app_handle: &tauri::AppHandle,
    capture: bool,
    session_start: i64,
) -> windows::core::Result<usize> {
    let notifications = listener
        .GetNotificationsAsync(NotificationKinds::Toast)?
        .get()?;

    let mut hits = 0usize;
    let mut active: HashSet<(String, u32)> = HashSet::new();

    for note in notifications {
        let aumid = match note.AppInfo().and_then(|a| a.AppUserModelId()) {
            Ok(id) => id.to_string_lossy().to_lowercase(),
            Err(_) => continue,
        };
        if !aumid.starts_with(PHONE_LINK_AUMID_PREFIX) {
            continue;
        }
        let id = match note.Id() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let key = (aumid.clone(), id);
        active.insert(key.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);

        // 会话开始前创建的通知(软件关闭期间收到的验证码)永不捕获;
        // CreationTime 拿不到时按"新通知"处理,宁可多看一眼不可静默漏掉
        let created_before_session = match note.CreationTime() {
            Ok(t) => t.UniversalTime < session_start,
            Err(_) => false,
        };
        if created_before_session {
            continue;
        }

        // 播种轮(capture=false)只记已见:启动时已存在的旧 toast 一律不处理
        if capture {
            if let Some((sender, body)) = extract_toast_texts(&note) {
                if let Some(code) = extract_code(&body) {
                    hits += 1;
                    handle_code(app_handle, &code, &sender);
                }
            }
        }
    }

    // 修剪:只保留通知中心里仍存在的条目
    seen.retain(|k| active.contains(k));
    Ok(hits)
}

/// 从 toast 通知提取 (发送者, 正文)。
/// text elements 约定:第 0 个是标题(发送者名或 App 名),其余是正文
fn extract_toast_texts(
    note: &windows::UI::Notifications::UserNotification,
) -> Option<(String, String)> {
    let notification = note.Notification().ok()?;
    let visual = notification.Visual().ok()?;
    let bindings = visual.Bindings().ok()?;

    let mut parts: Vec<String> = Vec::new();
    for binding in bindings {
        // 单个 binding 取不到文本只跳过它,不能因此丢掉整条通知 ——
        // 一个坏 binding 就 return None 会静默漏掉真实验证码
        let Ok(elements) = binding.GetTextElements() else {
            continue;
        };
        for element in elements {
            if let Ok(text) = element.Text() {
                let text = text.to_string_lossy().trim().to_string();
                // 同一份 toast 若有多个 binding(不同尺寸的模板),文本会
                // 重复出现;去重避免验证码被拼进正文两次
                if !text.is_empty() && !parts.contains(&text) {
                    parts.push(text);
                }
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let sender = parts.remove(0);
    Some((sender, parts.join(" ")))
}

/// 命中验证码:写剪贴板 + 前端提示 + 失焦系统通知。
///
/// 写剪贴板**不设 suppress**:剪贴板监控会把验证码当作普通内容自动记进
/// 历史(500ms 内),历史列表里直接能翻到——这是刻意设计的零成本集成
fn handle_code(app_handle: &tauri::AppHandle, code: &str, sender: &str) {
    if !write_clipboard_text(code) {
        eprintln!("[sms-code] 写剪贴板失败(重试后仍被占用),跳过 {}", code);
        return;
    }

    LAST_CAPTURE.store(now_unix(), Ordering::Relaxed);
    CAPTURE_COUNT.fetch_add(1, Ordering::Relaxed);

    // 前端居中提示(窗口存在且前端在监听时可见)
    let _ = app_handle.emit(
        "sms-code-copied",
        serde_json::json!({ "code": code, "sender": sender }),
    );

    // 主窗口未获焦(或已销毁)时发系统通知——用户多半正在别的窗口里
    // 等着粘贴验证码,通知比应用内 toast 更能到达视野。
    // 与排除提示同一守门逻辑,避免获焦时双重打扰
    use tauri::Manager;
    let focused = app_handle
        .get_webview_window("main")
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(false);
    if !focused {
        let _ = app_handle
            .notification()
            .builder()
            .title("验证码已复制")
            .body(format!("来自 {} 的验证码 {} 已写入剪贴板,直接粘贴即可", sender, code))
            .show();
    }
}

/// 写剪贴板(带重试):监控线程每 500ms 也在开关剪贴板,偶发占用冲突
fn write_clipboard_text(text: &str) -> bool {
    for _ in 0..3 {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if cb.set_text(text.to_string()).is_ok() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    false
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 验证码提取(纯函数,重点单测)
// ---------------------------------------------------------------------------

/// 关键词(小写匹配)。没有关键词的纯数字短信(账单/流量/物流)直接放弃:
/// 把「支出100元」写进剪贴板覆盖用户内容是帮倒忙,宁可不提取
const CODE_KEYWORDS: &[&str] = &[
    "验证码", "校验码", "动态码", "动态密码", "识别码", "verification", "code", "otp",
];

/// 负向关键词:紧邻这些词的数字是卡号/账号尾号,不是验证码
const NEGATIVE_PREFIXES: &[&str] = &["尾号", "卡号", "末四位", "账号", "单号", "工号"];

/// 从短信正文中提取验证码。返回 None 表示不是验证码短信或拿不准。
///
/// 原则:宁缺毋滥。提取失败的代价只是「用户手动看一眼手机」,
/// 提取错误的代价是「错误内容覆盖了用户正在复制的剪贴板」——不对称
pub fn extract_code(body: &str) -> Option<String> {
    let lower = body.to_lowercase();
    if !CODE_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return None;
    }

    // 归一化分组数字:「163-882」「9 1 2 8」→「163882」「9128」。
    // 只合并「数字 分隔符 数字」的相邻组;多轮替换处理连续分组
    let mut normalized = body.to_string();
    for _ in 0..3 {
        let next = merge_grouped_digits(&normalized);
        if next == normalized {
            break;
        }
        normalized = next;
    }

    // 候选:连续 4-8 位数字(跳过手机号 11 位、卡号 13-19 位等长数字)。
    // 手写字符扫描而非 regex,提取逻辑高频跑在轮询线程,保持轻量
    let chars: Vec<char> = normalized.chars().collect();
    let mut best: Option<(i32, String)> = None;
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let digits: String = chars[start..i].iter().collect();
        let len = digits.len();
        if !(4..=8).contains(&len) {
            continue; // 11 位手机号 / 13-19 位卡号 / 3 位短号都不属于验证码
        }
        // 8 位的 YYYYMMDD 日期不是验证码。这条必须有:merge_grouped_digits
        // 会把「2026 08 31」拼成 20260831,而 8 位数字的得分门槛只要 5 分
        // (长度 1 分 + 近距 4 分),「您的验证码将于 2026 08 31 过期」这种
        // 短信里没有别的数字,日期会被当成验证码提走并覆盖剪贴板
        if len == 8 && looks_like_date(&digits) {
            continue;
        }
        // 前文 6 个字符内的负向关键词 → 那是尾号/卡号不是验证码。
        // 3 字符窗口盖不住「尾号 - 1234」「尾号****1234」这类带分隔符的写法,
        // 漏判会把尾号当验证码提走并覆盖用户剪贴板;放宽到 6 仍紧邻,不会误伤
        let prefix: String = chars[start.saturating_sub(6)..start].iter().collect();
        if NEGATIVE_PREFIXES.iter().any(|p| prefix.contains(p)) {
            continue;
        }
        let score = score_candidate(&normalized, start, len);
        if let Some(score) = score {
            if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, digits));
            }
        }
    }
    best.filter(|(score, _)| *score >= 5).map(|(_, code)| code)
}

/// 计算候选数字串的得分。返回 None = 一票否决。
/// 打分:长度 4-6 是主流验证码长度(+3),7-8 较少见(+1);
/// 与**最近**关键词出现的字符距离 <15 强信号(+4),<30 弱信号(+2)。
/// 阈值 5:要么近距+主流长度(3+4=7),要么远距但主流(3+2=5)
fn score_candidate(text: &str, digit_start: usize, len: usize) -> Option<i32> {
    let mut score = match len {
        4..=6 => 3,
        7..=8 => 1,
        _ => return None,
    };

    let lower = text.to_lowercase();
    // 找所有关键词的所有出现位置,取与候选数字距离最小的一个
    // (验证码可能在关键词之后,也可能在之前,如 "123456 is your code")
    let mut min_dist = usize::MAX;
    for keyword in CODE_KEYWORDS {
        let mut search_from = 0usize;
        while let Some(found) = lower[search_from..].find(keyword) {
            let byte_pos = search_from + found;
            // 字节下标 → 字符下标(评分全程用字符距离,中文正文必须)
            let char_pos = lower[..byte_pos].chars().count();
            let dist = char_pos.abs_diff(digit_start);
            if dist < min_dist {
                min_dist = dist;
            }
            search_from = byte_pos + keyword.len();
        }
    }
    if min_dist == usize::MAX {
        return None;
    }

    score += match min_dist {
        0..=15 => 4,
        16..=30 => 2,
        _ => 0,
    };
    Some(score)
}

/// 合并「数字+单个空格/短横线+数字」的分组:返回新字符串
fn merge_grouped_digits(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit()
            && i + 2 < chars.len()
            && (chars[i + 1] == ' ' || chars[i + 1] == '-')
            && chars[i + 2].is_ascii_digit()
        {
            // 跳过分隔符,保留两侧数字
            out.push(c);
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// 判断 8 位数字是否形如 YYYYMMDD 日期。
///
/// 只针对 8 位:6 位 YYMMDD 虽然也能构成日期,但 6 位是验证码最主流的
/// 长度,为了排除一个低概率的日期误报而牺牲真实验证码,不划算 ——
/// 而 8 位验证码本身就罕见,排除掉的代价几乎为零
fn looks_like_date(digits: &str) -> bool {
    let b = digits.as_bytes();
    if b.len() != 8 || !b.iter().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let year: u16 = digits[0..4].parse().unwrap_or(0);
    let month: u8 = digits[4..6].parse().unwrap_or(0);
    let day: u8 = digits[6..8].parse().unwrap_or(0);
    (1900..=2100).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_start_ticks_is_plausible() {
        // 会话起点必须晚于 2020-01-01(WinRT 刻度),且接近当下:
        // 大于 2020、小于"2020 + 100 年",防止纪元换算写错方向
        let ticks_2020: i64 = (1_577_836_800 + WIN_EPOCH_DIFF_SECS as i64) * 10_000_000;
        let now = session_start_ticks();
        assert!(now > ticks_2020);
        assert!(now < ticks_2020 + 100i64 * 365 * 24 * 3600 * 10_000_000);
    }

    #[test]
    fn test_chinese_code_sms() {
        assert_eq!(
            extract_code("【淘宝】验证码:483920,您正在登录,请勿泄露给他人"),
            Some("483920".to_string())
        );
    }

    #[test]
    fn test_english_grouped_code() {
        // WhatsApp 风格:分组数字要归一
        assert_eq!(
            extract_code("Your WhatsApp code is 163-882"),
            Some("163882".to_string())
        );
    }

    #[test]
    fn test_spaced_code() {
        assert_eq!(
            extract_code("【京东】动态密码:9 1 2 8,请勿告知他人"),
            Some("9128".to_string())
        );
    }

    #[test]
    fn test_no_keyword_no_extract() {
        // 无关键词:宁可漏,不可错
        assert_eq!(extract_code("【工商银行】您尾号3456的卡支出100元,余额5000元"), None);
    }

    #[test]
    fn test_negative_prefix_rejected() {
        // 关键词在场,但最近的数字是「尾号」——必须跳过它取真验证码
        assert_eq!(
            extract_code("【银行】动态密码:9128,尾号3456的卡入账100元"),
            Some("9128".to_string())
        );
    }

    #[test]
    fn test_phone_number_not_code() {
        // 11 位手机号不是验证码;400 电话合并后 10 位也被长度过滤
        assert_eq!(
            extract_code("您的验证码是1234,请在5分钟内输入。客服电话400-800-9000"),
            Some("1234".to_string())
        );
    }

    #[test]
    fn test_long_digits_ignored() {
        // 卡号形态(16 位)不提取
        assert_eq!(
            extract_code("验证码已发送至您手机,卡号4532015112830366关联的手机"),
            None
        );
    }

    #[test]
    fn test_code_after_keyword_far() {
        // 验证码与关键词距离较远但仍可识别
        assert_eq!(
            extract_code("Your Apple ID verification code is: 582913"),
            Some("582913".to_string())
        );
    }

    #[test]
    fn test_pure_digits_without_keyword() {
        assert_eq!(extract_code("123456"), None);
    }

    #[test]
    fn test_merge_grouped() {
        assert_eq!(merge_grouped_digits("163-882"), "163882");
        assert_eq!(merge_grouped_digits("9 1 2 8"), "9128");
        assert_eq!(merge_grouped_digits("400-800-9000"), "4008009000");
        assert_eq!(merge_grouped_digits("验证码 1234"), "验证码 1234");
    }

    #[test]
    fn test_order_number_negative_prefix() {
        // 「订单号」含负向词「单号」:8 位订单号不能被当成验证码
        assert_eq!(
            extract_code("尊敬的客户:您的订单号20260831已发货,验证码功能需开通"),
            None
        );
    }

    #[test]
    fn test_multi_group_spaces() {
        // 三段式空格分组:57 39 21 → 573921
        assert_eq!(
            extract_code("【支付宝】校验码:57 39 21,请勿泄露"),
            Some("573921".to_string())
        );
    }

    #[test]
    fn test_grouped_date_not_code() {
        // 分组日期被 merge 成 20260831 后长度合法、距离关键词也够近,
        // 但它不是验证码 —— 提走它会覆盖用户正在复制的内容
        assert_eq!(extract_code("【银行】您的验证码将于 2026 08 31 过期"), None);
        assert_eq!(extract_code("验证码 2026-08-31 前有效,请尽快使用"), None);
        assert_eq!(extract_code("【商家】您的优惠券验证码 20260831 已生成"), None);
    }

    #[test]
    fn test_date_does_not_shadow_real_code() {
        // 真验证码与日期同时出现时,应取真验证码(它离关键词更近,得分更高)
        assert_eq!(
            extract_code("【银行】验证码 483920,有效期至 2026-08-31"),
            Some("483920".to_string())
        );
    }

    #[test]
    fn test_looks_like_date() {
        assert!(looks_like_date("20260831"));
        assert!(looks_like_date("19991231"));
        assert!(!looks_like_date("20261331")); // 月份 13 非法
        assert!(!looks_like_date("20260800")); // 日 00 非法
        assert!(!looks_like_date("12345678")); // 年份不在 1900-2100
        assert!(!looks_like_date("483920")); // 非 8 位直接不算
    }

    #[test]
    fn test_code_before_keyword() {
        // 验证码在关键词之前(Telegram/Google 风格)
        assert_eq!(
            extract_code("G-548921 is your Google verification code"),
            Some("548921".to_string())
        );
    }
}
