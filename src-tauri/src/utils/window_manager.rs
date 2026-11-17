use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::models::settings::CloseBehavior;
use crate::services::settings_service::load_settings;

/// 上次窗口创建开始的时间戳(毫秒),0=空闲。不用布尔标志:登录瞬间等
/// 桌面未就绪的场景下创建可能卡死不返回,布尔会永久封死后续唤出;
/// 时间戳超时(CREATE_TIMEOUT_MS)后自动放行重试
static CREATING_AT: AtomicU64 = AtomicU64::new(0);
const CREATE_TIMEOUT_MS: u64 = 30_000;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 统一的“唤出”入口:窗口存在则显示聚焦,不存在(已被销毁)则后台线程重建。
/// 热键、托盘、show_window 命令都走这里。
pub fn show_or_create(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_minimized().unwrap_or(false) {
            let _ = window.unminimize();
        }
        ensure_on_screen(app, &window);
        let _ = window.show();
        let _ = window.set_focus();
        apply_pinned(app, &window);
        // 首帧空窗:show() 到守护线程下一次巡检之间(≤200ms)面板已可见但可能
        // 还没带上 WS_EX_TOPMOST,此时目标窗口一激活就把面板压到后面。pinned
        // 开启时再无条件补一次(幂等,SWP_NOACTIVATE 不抢焦点),消掉这段空窗。
        // 注意仍受 pinned 设置约束:用户关掉置顶时不得强行置顶
        if load_settings(app).pinned {
            if let Ok(h) = window.hwnd() {
                crate::utils::input_focus::force_topmost(h.0 as isize);
            }
        }
        // 已存在窗口的分支以前只 show()+set_focus():而唤出本应用时我们正是
        // "非前台进程",那次 set_focus 常被 Windows 前台锁静默拒绝,窗口
        // 可见但不是前台 —— 用户点第一下条目时点击根本没送到面板(表现为
        // "第一次点不粘贴,第二次才粘")。这里补一次强夺取 + 与建窗路径一致
        // 的异步兜底,保证面板真正拿到前台输入权。交接期间(正在把前台交给
        // 粘贴落点)绝不抢,与 ensure_focus 的抑制语义一致
        if let Ok(h) = window.hwnd() {
            let hwnd = h.0 as isize;
            if !crate::utils::input_focus::is_focus_handoff_active() {
                crate::utils::input_focus::force_foreground(hwnd);
            }
        }
        ensure_focus(window.clone());
        return;
    }
    spawn_create(app);
}

/// 按设置恢复置顶。注意不能只调 tao 的 `set_always_on_top`:它走"账面
/// 标志差量",账面已是 true 时不会再发任何 Win32 调用,而创建竞态可能让
/// 那唯一一次 SetWindowPos 静默失败(错误被吞、账面不回滚)—— 之后真实
/// 样式缺失却永远补不回来,表现即"唤出后不悬浮,反复隐藏唤出无法恢复"。
/// 所以每次都核对窗口真实样式,缺失就直接用 Win32 补上
/// (见 input_focus::force_topmost / is_topmost)
pub fn apply_pinned(app: &AppHandle, window: &tauri::WebviewWindow) {
    if !load_settings(app).pinned {
        return;
    }
    let _ = window.set_always_on_top(true);
    let hwnd = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    if hwnd != 0 && !crate::utils::input_focus::is_topmost(hwnd) {
        eprintln!("[topmost] style missing at visibility transition — forcing via SetWindowPos");
        crate::utils::input_focus::force_topmost(hwnd);
    }
}

/// 置顶守护:面板可见期间每 200ms 核对 WS_EX_TOPMOST 真实样式,缺失立即
/// 用 Win32 直补(绕过 tao 账面,见 apply_pinned 注释)。覆盖两类丢失:
/// 创建竞态让唯一一次置顶调用静默失败,以及外部窗口管理类软件在面板
/// 使用期间剥除置顶。终端里反复出现的 re-applying 即丢失在持续发生,
/// 可据此排查那台机器上的相关软件。间隔从 500ms 收到 200ms:窗口重建后
/// 的"已可见但缺置顶"空窗越短越好,单次巡检只是两次 Win32 查询,开销可忽略
pub fn spawn_topmost_guard(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let Some(window) = app.get_webview_window("main") else {
            continue;
        };
        if !window.is_visible().unwrap_or(false) {
            continue;
        }
        if !load_settings(&app).pinned {
            continue;
        }
        let hwnd = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
        if hwnd != 0 && !crate::utils::input_focus::is_topmost(hwnd) {
            eprintln!("[topmost] WS_EX_TOPMOST lost while visible — re-applying");
            crate::utils::input_focus::force_topmost(hwnd);
        }
    });
}

/// 自愈:窗口保存的坐标可能已不在任何显示器上(登录早期枚举不全、
/// 拔了显示器、改了分辨率),越界时拉回主显示器居中,否则用户会以为
/// "唤不出窗口"。仅对可见判定,不改变正常位置。
fn ensure_on_screen(app: &AppHandle, window: &tauri::WebviewWindow) {
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return;
    };
    let Ok(monitors) = app.available_monitors() else {
        return;
    };
    if monitors.is_empty() {
        return;
    }
    let on_screen = monitors.iter().any(|m| {
        let mp = m.position();
        let ms = m.size();
        pos.x + size.width as i32 > mp.x
            && pos.x < mp.x + ms.width as i32
            && pos.y + size.height as i32 > mp.y
            && pos.y < mp.y + ms.height as i32
    });
    if !on_screen {
        let Some(primary) = app
            .primary_monitor()
            .ok()
            .flatten()
            .or_else(|| monitors.first().cloned())
        else {
            return;
        };
        let mp = primary.position();
        let ms = primary.size();
        let x = mp.x + (ms.width as i32 - size.width as i32).max(0) / 2;
        let y = mp.y + (ms.height as i32 - size.height as i32).max(0) / 2;
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

/// 在独立线程中重建窗口:避免占用事件循环线程,
/// 也避开“从主线程事件回调里同步 build 可能死锁”的问题。
/// 防重入用时间戳:进行中且未超时才跳过,卡死后 30 秒自动解锁重试。
fn spawn_create(app: &AppHandle) {
    let now = now_ms();
    let prev = CREATING_AT.swap(now, Ordering::SeqCst);
    if prev != 0 && now.saturating_sub(prev) < CREATE_TIMEOUT_MS {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        // Drop 守卫保证任何路径(含 panic)都复位防重入标志:原先靠函数
        // 末尾手动 store,创建线程一旦 panic,标志滞留,30 秒内所有唤出被
        // 静默跳过;若 panic 可复现(个别机器环境相关),热键就“永久失灵”,
        // 用户只能重启应用
        struct ResetCreating;
        impl Drop for ResetCreating {
            fn drop(&mut self) {
                CREATING_AT.store(0, Ordering::SeqCst);
            }
        }
        let _guard = ResetCreating;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Err(e) = create_main_window(&app, true) {
                eprintln!("Failed to recreate main window: {}", e);
            }
        }));
        if result.is_err() {
            eprintln!("create_main_window panicked; flag reset, next summon will retry");
        }
    });
}

/// 创建主窗口。tauri.conf.json 不再声明窗口,启动与唤出共用此函数。
/// 窗口先隐藏,页面加载完成后才显示(show_when_ready),避免唤出瞬间的白屏闪烁。
/// 返回值仅给启动路径用:setup 阶段创建失败应直接终止应用。
pub fn create_main_window(app: &AppHandle, show_when_ready: bool) -> tauri::Result<()> {
    let state = load_window_state();

    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("剪雀 (Sparrow)")
        // 初始尺寸仅作占位(基准 420×520 逻辑):随后 apply_initial_geometry 会按
        // 目标显示器与缩放重算(老用户跨 DPI 还原,新用户按屏幕相对 1080p 缩放)
        .inner_size(420.0, 520.0)
        // 最小尺寸(逻辑单位):防用户拖到不可用;与 window_sizing 的钳制下限一致
        .min_inner_size(crate::utils::window_sizing::MIN_W, crate::utils::window_sizing::MIN_H)
        .resizable(true)
        .decorations(false)
        .shadow(true)
        .visible(false)
        .on_page_load(move |window, payload| {
            if show_when_ready
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
            {
                let _ = window.show();
                let _ = window.set_focus();
                // 置顶在 show 之后重申:创建线程落置顶与页面加载完成之间可能
                // 隔数百毫秒,快速热键操作可在间隙里插入销毁/重建
                apply_pinned(window.app_handle(), &window);
                // 建窗路径同样受前台锁影响:热键唤出时我们不是前台进程,
                // 紧邻的 set_focus 可能被静默拒绝(只闪任务栏)。先做一次
                // AttachThreadInput 强夺取,拿到真正的前台输入权后再交给
                // ensure_focus 做异步兜底
                if let Ok(h) = window.hwnd() {
                    let hwnd = h.0 as isize;
                    if !crate::utils::input_focus::is_focus_handoff_active() {
                        crate::utils::input_focus::force_foreground(hwnd);
                    }
                }
                ensure_focus(window);
            }
        });
    if let Some(icon) = app.default_window_icon() {
        // icon() 消耗 builder;仅在不支持窗口图标的平台才可能失败,Windows 不会
        builder = builder.icon(icon.clone()).expect("set window icon");
    }
    let window = builder.build()?;

    // 自适应尺寸 + DPI 感知定位:按鼠标所在显示器的工作区与缩放因子,
    // 计算并钳制面板尺寸与位置(销毁重建不像 hide/show 那样由系统免费保留几何)
    crate::utils::window_sizing::apply_initial_geometry(app, &window, &state);
    if state.maximized {
        let _ = window.maximize();
    }
    // build 之后、show 之前就落置顶:窗口此刻仍 visible(false),先把
    // WS_EX_TOPMOST 带上,首帧可见即悬浮,不依赖 PageLoad / 守护线程补
    apply_pinned(app, &window);

    // 几何状态由移动/缩放事件实时持久化到独立文件(不进 AppSettings:
    // 前端设置面板即改即存会整体回传设置对象,曾把几何字段清空)
    register_geometry_persistence(&window);
    // 面板被拖到缩放因子不同的显示器时,保持逻辑尺寸并重新钳制进新工作区
    register_dpi_handler(&window);
    // 以下是每次重建都必须重新挂上的窗口级接线
    register_close_handler(&window);
    crate::utils::webview_control::apply_memory_optimizations(app);

    Ok(())
}

/// 窗口几何状态:独立于 AppSettings 的持久化文件,
/// 由 Moved/Resized 事件实时写入,窗口销毁重建后精确恢复。
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct WindowState {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub w: Option<u32>,
    pub h: Option<u32>,
    pub maximized: bool,
    /// 保存 w/h 时所在显示器的缩放因子。跨 DPI 显示器时用它把物理尺寸还原回
    /// 逻辑尺寸再换算,保证视觉大小一致;`#[serde(default)]` 兼容无此字段的旧文件。
    #[serde(default)]
    pub scale: Option<f64>,
}

fn window_state_path() -> std::path::PathBuf {
    crate::app_config_dir().join("window-state.json")
}

pub fn load_window_state() -> WindowState {
    let mut state = std::fs::read_to_string(window_state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // 上一版默认尺寸跟随新默认(见 window_sizing::migrate_legacy_size)
    crate::utils::window_sizing::migrate_legacy_size(&mut state);
    state
}

fn save_window_state(s: &WindowState) {
    let dir = crate::app_config_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(window_state_path(), json);
    }
}

/// 监听移动/缩放:非最大化时记下几何,最大化只记标志(保留上次的正常尺寸)。
/// 文件极小,拖拽过程中的连续写入可接受。
fn register_geometry_persistence(window: &tauri::WebviewWindow) {
    let win = window.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Moved(pos) => {
            let mut s = load_window_state();
            if !win.is_maximized().unwrap_or(false) {
                s.x = Some(pos.x);
                s.y = Some(pos.y);
            }
            save_window_state(&s);
        }
        tauri::WindowEvent::Resized(size) => {
            let mut s = load_window_state();
            let maximized = win.is_maximized().unwrap_or(false);
            s.maximized = maximized;
            if !maximized {
                s.w = Some(size.width);
                s.h = Some(size.height);
                // 一并记录当前缩放因子:跨 DPI 重建时按“逻辑尺寸”还原,视觉大小不跳变。
                // 读不到时保留旧值(老文件就是 None,按当前 scale 处理)
                s.scale = win.scale_factor().ok().or(s.scale);
            }
            save_window_state(&s);
        }
        _ => {}
    });
}

/// 监听缩放因子变化(面板被拖到另一块 DPI 不同的显示器):
/// 保持逻辑尺寸不变,再钳制进新显示器工作区。只在缩放变化时动作,
/// 不触碰用户手动调整的尺寸。
fn register_dpi_handler(window: &tauri::WebviewWindow) {
    let win = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::ScaleFactorChanged {
            scale_factor,
            new_inner_size,
            ..
        } = event
        {
            crate::utils::window_sizing::handle_scale_factor_changed(
                &win,
                *scale_factor,
                *new_inner_size,
            );
        }
    });
}

/// CloseRequested 拦截,按 close_behavior 分流:询问 / 最小化(销毁)/ 直接退出
fn register_close_handler(window: &tauri::WebviewWindow) {
    let win = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            let app_handle = win.app_handle();
            let settings = load_settings(app_handle);
            match settings.close_behavior {
                CloseBehavior::Minimize => {
                    api.prevent_close();
                    destroy_main_window(app_handle);
                }
                CloseBehavior::Close => {
                    // 主进程不随最后一个窗口销毁而退出(见 lib.rs 的 prevent_exit),
                    // “直接退出”必须显式调 exit;统一走 exit_app 先销毁窗口,
                    // 避免 WebView2 注销窗口类失败的退出噪音
                    exit_app(app_handle);
                }
                CloseBehavior::Ask => {
                    api.prevent_close();
                    let _ = app_handle.emit("ask-close-behavior", ());
                }
            }
        }
    });
}

/// 销毁主窗口:WebView2 控件与整棵 msedgewebview2.exe 进程树随之退出,内存归还系统。
/// 几何状态已由 Moved/Resized 事件实时持久化,这里无需再保存。
pub fn destroy_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.destroy();
    // 主进程工作集裁剪,物理内存立即下降
    crate::utils::webview_control::set_webview_visible(app, false);
}

/// 统一退出入口:先销毁主窗口,再 exit。
///
/// 为什么必须先销毁:Chromium/WebView2 在进程直接退出时会来不及注销窗口类,
/// 打印 [`Failed to unregister class Chrome_WidgetWin_0. Error = 1412`] 噪音
/// (ERROR_CLASS_HAS_WINDOWS;Tauri issue #7606、Wails #4373 均为同一现象)。
/// 只是日志、不影响功能,但先把 WebView2 拆干净能让退出路径安静且资源释放完整。
pub fn exit_app(app: &AppHandle) {
    destroy_main_window(app);
    app.exit(0);
}

/// 页面加载完成后的焦点保障:紧跟 show() 调用的 set_focus() 可能被
/// tao 的可见性标志门槛或系统前台锁定静默吞掉,窗口"可见但非前台"——
/// 下一次热键会被 toggle 判定为未聚焦而只走显示分支,表现为
/// "第一次按键仅切焦点、第二次才隐藏"。这里异步轮询,未真正取得
/// 前台焦点就重试 set_focus,最多 15 次 × 50ms(约 0.75 秒,已足够覆盖
/// tao 标志滞后);窗口期间被销毁则安静退出。
///
/// 两处提前返回:
/// - 已真正取得前台焦点(**只**认原生 `GetForegroundWindow` 比对)—— 目标达成;
/// - [`input_focus::is_focus_handoff_active`] 为真 —— 应用正在把前台交给
///   粘贴目标窗口(「粘贴后保持打开」),此时再抢前台会把焦点夺回面板,
///   随后模拟的 Ctrl+V 就落进面板自己的搜索框(表现为"点了没粘贴")。
///   正常路径下焦点已由 show()+set_focus() 给到,停手不影响聚焦。
fn ensure_focus(window: tauri::WebviewWindow) {
    std::thread::spawn(move || {
        let hwnd = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
        for _ in 0..15 {
            if crate::utils::input_focus::is_focus_handoff_active() {
                return;
            }
            // 判定只用原生前台比对。这里**不能**再用 tao 的 window.is_focused():
            // 它基于 WM_ACTIVATE/WM_SETFOCUS 异步更新,窗口刚显示时滞后,会把
            // "可见但并非前台"误报成已聚焦 —— 循环随即提前退出,前台锁导致的
            // 抢焦点失败再没人重试,用户点第一下条目就落空(第一次点击不粘贴)
            if crate::utils::input_focus::is_foreground_window(hwnd) {
                return;
            }
            // 每轮先尝试绕过前台锁的强夺取(AttachThreadInput + SetForegroundWindow),
            // 它比 tao 的 set_focus 更可能成功;失败才退回 set_focus
            if crate::utils::input_focus::force_foreground(hwnd) {
                return;
            }
            let _ = window.set_focus();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}
