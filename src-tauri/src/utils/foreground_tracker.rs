//! 最近外部前台窗口跟踪器。
//!
//! 为什么需要:粘贴落点必须按「用户点击条目那一刻」的真实意图确定。用户常常
//! 先按热键唤出面板,再点另一个应用(浏览器/微信)的输入框,最后才回来点条目
//! —— 面板默认置顶且失焦不隐藏(见 window_manager),这段"中间换台"完全合法。
//! 而热键唤出时 `input_focus::capture_foreground_hwnd()` 只反映**唤出瞬间**的
//! 前台,此后从不刷新,于是陈旧的 hwnd 会在点击条目时把前台抢回上一个软件、
//! 把 Ctrl+V 发过去(docs/KEEP-OPEN-DESIGN.md「粘贴落点的确定」)。
//!
//! 做法:`SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 监听全系统前台切换,
//! 只记录**其它进程**的顶层窗口;点击条目时用 [`freeze_paste_target`] 把它
//! 冻结成本次粘贴目标。
//!
//! 状态是**三态**([`TrackedForeground`]),不能用 0 兼表"从未跟踪"与"明确
//! 没有落点":用户点桌面/任务栏之后必须能区分出"别自动激活任何窗口",
//! 否则回退到唤出瞬间的陈旧捕获值又会把几分钟前的应用拉回前台。
//!
//! 三态还必须**穿过调用方**:冻结结果是 `Option<isize>` ——
//! `None` = 本次不该激活任何窗口(只复制),`Some(0)` = 托盘/冷启动那种
//! "确实没有记录,允许走 Z 序兜底/隐式激活"的老路径。若在边界塌成单个
//! `isize`,KeepOpen 的 Z 序兜底会把"点了桌面"变成"粘进随机窗口"。
//!
//! 线程模型:out-of-context 钩子的回调投递到**安装线程**的消息队列,该线程
//! 必须持续 `GetMessageW` 取消息,否则回调永不被调用。线程随进程存活,不卸载。
//! 钩子安装失败时降级为 40ms 轮询(功能不丢,只多几次 Win32 查询)。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::time::Duration;

/// 钩子事件范围:只关心"前台窗口换了"
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
/// 回调在安装线程的上下文中调用(不需要注入到目标进程)
const WINEVENT_OUTOFCONTEXT: u32 = 0;
/// 自身进程的窗口切换不发事件(下面仍按 pid 双保险过滤)
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;
const DWMWA_CLOAKED: u32 = 14;
/// 钩子不可用时的降级轮询间隔。40ms:前台切换要尽快记下,漏一拍就可能
/// 把落点错判成上一条记录
const POLL_INTERVAL_MS: u64 = 40;

/// 三态记录的存储编码:0 = 从未收到过前台事件(冷启动);
/// -1 = 明确没有可用落点(壳/桌面/不可激活覆盖层/记录已失效);> 0 = 外部窗口
const TRACK_UNKNOWN: isize = 0;
const TRACK_NO_TARGET: isize = -1;

/// 桌面窗口类名。**单独一组**:它们接不了键盘输入(不是输入落点),但
/// 「框选文件 → 连贴到桌面」确实是把文件贴到桌面上,所以"无明确落点"路径
/// 要放行它们(见 [`is_acceptable_paste_foreground`])。
const DESKTOP_CLASSES: [&str; 2] = ["Progman", "WorkerW"];

/// 壳窗口类名:任务栏、任务视图、XAML 岛宿主等。它们成为前台意味着用户
/// 点了任务栏或按了 Win+D / Win+Tab —— 这时必须判"无落点"。
const SHELL_CLASSES: [&str; 6] = [
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    "Shell_CharmWindow",
    "MultitaskingViewFrame",
];

/// `Windows.UI.Core.CoreWindow` 被**全屏 UWP 应用**(照片/计算器/设置)共用,
/// 不能一刀切当壳窗口 —— 只有这些壳宿主进程的 CoreWindow 才算壳。
const SHELL_HOST_EXES: [&str; 4] = [
    "StartMenuExperienceHost.exe",
    "SearchHost.exe",
    "SearchApp.exe",
    "ShellExperienceHost.exe",
];

/// **瞬态壳浮层**:托盘溢出面板(Win10 `NotifyIconOverflowWindow` /
/// Win11 `TopLevelWindowForOverflowXamlIsland`)。它们是 explorer 拥有的
/// 可激活浮层,会短暂成为前台;用户点里面的托盘图标后浮层隐藏、句柄随即失效。
/// 若把它记成落点,冻结时窗口已不存在 → 判成"无落点" → 从溢出区打开面板后
/// 点条目会变成"只复制"(没反应)。所以对它们**忽略**(保留上一条有效记录),
/// 让落点自然回落到用户原来的应用 —— 既不清空,也不记录。
const TRANSIENT_OVERLAY_CLASSES: [&str; 2] = [
    "NotifyIconOverflowWindow",
    "TopLevelWindowForOverflowXamlIsland",
];

const GWL_EXSTYLE: i32 = -20;
/// 不可激活窗口(工具条/覆盖层/任务视图等):永远不会是键盘输入的目标
const WS_EX_NOACTIVATE: isize = 0x0800_0000;
/// OpenProcess 的最小查询权限(只要进程映像名,不碰内存)
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

/// WINEVENTPROC:回调签名固定为
/// `(hook, event, hwnd, idObject, idChild, idEventThread, dwmsEventTime)`。
/// 别名不用全大写,避免 clippy::upper_case_acronyms(项目里 -D warnings 会报)
type WinEventProc = Option<unsafe extern "system" fn(isize, u32, isize, i32, i32, u32, u32)>;

/// GetMessageW 用的 MSG 布局(与 paste_hook::Msg 一致:x64 上 48 字节,
/// 末尾多留一个 DWORD 让填充安全)。两处各自持有,避免线程模块互相依赖
#[repr(C)]
struct Msg {
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
    l_private: i32,
}

impl Msg {
    fn zeroed() -> Self {
        // 安全:MSG 是纯 POD,全零是 GetMessageW 合法的初始状态
        unsafe { std::mem::zeroed() }
    }
}

#[link(name = "user32")]
extern "system" {
    fn SetWinEventHook(
        event_min: u32,
        event_max: u32,
        hmod: isize,
        proc_: WinEventProc,
        pid: u32,
        tid: u32,
        flags: u32,
    ) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn GetForegroundWindow() -> isize;
    fn IsWindow(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowLongPtrW(hwnd: isize, nindex: i32) -> isize;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetTickCount64() -> u64;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
    fn QueryFullProcessImageNameW(
        process: isize,
        flags: u32,
        exe_name: *mut u16,
        size: *mut u32,
    ) -> i32;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmGetWindowAttribute(hwnd: isize, attr: u32, out: *mut u32, size: u32) -> i32;
}

/// 最近一次**外部进程**前台窗口(三态编码,见 TRACK_UNKNOWN / TRACK_NO_TARGET)
static LAST_EXTERNAL: AtomicIsize = AtomicIsize::new(TRACK_UNKNOWN);
/// 记录时刻(GetTickCount64,毫秒):只用于诊断日志,不做时效过滤 ——
/// 面板可能被用户开着好几分钟,那时"最后一次的外部前台"依然是正确落点
static LAST_EXTERNAL_TICK: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

/// 跟踪到的最近外部前台窗口。三态是刻意设计:
/// - `Unknown` 与 `NoTarget` 都"没有可用窗口",但**语义相反** ——
///   前者表示"还没观察到任何前台变化"(可以安全回退唤出时的捕获值),
///   后者表示"最近的前台是壳/桌面/不可激活窗口"(必须什么都不激活,
///   绝不能回退到陈旧捕获值,否则点完桌面又会把旧应用拉回前台)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedForeground {
    /// 冷启动后还没收到任何前台事件
    Unknown,
    /// 最近的前台是壳/桌面/不可激活窗口,或记录已失效:本次不该激活任何窗口
    NoTarget,
    /// 一个外部顶层窗口(读取时仍需 [`is_valid_target`] 复核)
    Window(isize),
}

/// 启动跟踪(幂等)。在 lib.rs setup 里调用一次,线程随进程存活。
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        // SKIPOWNPROCESS 让自身窗口切换根本不进回调(pid 过滤是双保险);
        // hmod=0 表示"回调在本进程内",不需要 DLL 注入
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                0,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook != 0 {
            // 消息泵:回调在本线程取消息时被调用;GetMessageW 阻塞等待,
            // 永不退出(线程随进程结束)。返回 <=0 只可能是 WM_QUIT 或错误
            let mut msg = Msg::zeroed();
            while unsafe { GetMessageW(&mut msg, 0, 0, 0) } > 0 {}
            eprintln!("[focus] foreground hook pump exited — falling back to polling");
        } else {
            eprintln!(
                "[focus] SetWinEventHook failed — falling back to {}ms polling",
                POLL_INTERVAL_MS
            );
        }
        poll_loop();
    });
}

/// 降级路径:定时比对前台窗口。只在前台**变化**时走同一套过滤,
/// 避免每 40ms 都打一遍 Win32 属性查询
fn poll_loop() {
    eprintln!("[focus] polling fallback active ({}ms)", POLL_INTERVAL_MS);
    // -1 不可能是 GetForegroundWindow 的返回值,用作"尚未观察"的初值
    let mut last_seen: isize = -1;
    loop {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground != last_seen {
            last_seen = foreground;
            handle_foreground(foreground);
        }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    }
}

/// 当前跟踪状态(读取时**不做**校验:调用方通常还要拿 hwnd 去做别的事)
pub fn tracked_foreground() -> TrackedForeground {
    match LAST_EXTERNAL.load(Ordering::SeqCst) {
        TRACK_UNKNOWN => TrackedForeground::Unknown,
        TRACK_NO_TARGET => TrackedForeground::NoTarget,
        hwnd => TrackedForeground::Window(hwnd),
    }
}

/// 清空记录(测试与极端场景:例如调用方确认落点已不可用)。
/// 注意清空后回到 [`TrackedForeground::Unknown`] 语义(允许回退捕获值);
/// 生产路径不主动调用 —— 只有壳窗口事件才置 NoTarget。
#[allow(dead_code)] // 留给测试/排障,生产路径由 handle_foreground 维护
pub fn clear() {
    LAST_EXTERNAL.store(TRACK_UNKNOWN, Ordering::SeqCst);
    LAST_EXTERNAL_TICK.store(0, Ordering::SeqCst);
}

/// 冻结本次粘贴的目标。返回值语义(**三态穿过调用方**):
/// - `Some(hwnd)`:`hwnd > 0` 时直接激活该窗口;`hwnd == 0` 表示"没有记录但
///   允许走老路径"(托盘/冷启动:KeepOpen 用 Z 序兜底、HideFirst 用隐式激活);
/// - `None`:本次**不该激活任何窗口**(用户点了桌面/任务栏、记录已失效、
///   捕获值也不可用)→ 只写剪贴板,不发按键。
///
/// 必须在任何窗口隐藏/销毁/异步等待**之前**调用 —— 点击条目后前台已经
/// 变成我们自己的面板,再也读不到用户真正想去的地方了。
pub fn freeze_paste_target(captured: isize) -> Option<isize> {
    let tracked = tracked_foreground();
    let frozen = choose_paste_target(tracked, captured, is_valid_target);
    let source = match (tracked, frozen) {
        (TrackedForeground::Window(hwnd), Some(target)) if hwnd == target => "tracker",
        (TrackedForeground::Window(_), _) => "tracker-stale",
        (TrackedForeground::NoTarget, _) => "no-target",
        (TrackedForeground::Unknown, Some(_)) => "unknown->captured",
        (TrackedForeground::Unknown, None) => "unknown->none",
    };
    // tracker 命中是常态(不打日志);其余来源各打一行,便于现场排障
    if source != "tracker" {
        eprintln!(
            "[focus] paste target captured={} -> {:?} (source={}, age={}ms)",
            captured,
            frozen,
            source,
            tracked_age_ms()
        );
    }
    frozen
}

/// 记录距现在多久(诊断用;0 = 无记录)
fn tracked_age_ms() -> u64 {
    let tick = LAST_EXTERNAL_TICK.load(Ordering::SeqCst);
    if tick == 0 {
        return 0;
    }
    // 安全:GetTickCount64 无参数、无副作用
    unsafe { GetTickCount64() }.saturating_sub(tick)
}

/// 目标选择优先级(纯逻辑,可单测):
/// 1. `Window(h)` 且通过 `valid` 校验 → `Some(h)`(用户最后真正在用的外部窗口);
/// 2. `NoTarget`,或 `Window(h)` 已失效 → `None`(只复制)。这两种情况都表示
///    "最近的前台不可能是粘贴目标"(点了桌面/任务栏/窗口已关闭),回退到
///    `captured` 会把几分钟前的应用重新拉回前台 —— 正是本 bug 的症状;
/// 3. `Unknown`(冷启动,从未收到前台事件):`captured == 0` → `Some(0)`
///    (托盘/命令打开,允许走 Z 序兜底/隐式激活的老路径);
///    `captured` 可用 → `Some(captured)`;`captured` 也不可用 → `None`。
pub fn choose_paste_target<F>(
    tracked: TrackedForeground,
    captured: isize,
    valid: F,
) -> Option<isize>
where
    F: Fn(isize) -> bool,
{
    match tracked {
        TrackedForeground::Window(hwnd) if hwnd > 0 && valid(hwnd) => Some(hwnd),
        TrackedForeground::Window(_) | TrackedForeground::NoTarget => None,
        TrackedForeground::Unknown if captured == 0 => Some(0),
        TrackedForeground::Unknown if captured > 0 && valid(captured) => Some(captured),
        TrackedForeground::Unknown => None,
    }
}

/// 窗口是否可作为粘贴落点:真实存在、可见、非自身进程、非 cloaked、非壳/桌面窗口、
/// 可激活。与跟踪记录用同一套判定([`decide`]),避免"记进去的能选、选出来的记不进"。
///
/// cloaked(DWM 遮蔽)查询可能阻塞(目标进程挂起时),只在这里做,不放进前台
/// 事件回调 —— 回调必须极快,否则整个消息泵被拖住。
pub fn is_valid_target(hwnd: isize) -> bool {
    if hwnd <= 0 {
        return false;
    }
    let mut facts = cheap_facts(hwnd);
    facts.cloaked = is_cloaked(hwnd);
    decide(&facts) == TrackDecision::Record
}

/// "无明确落点"路径(托盘/冷启动/连续粘贴)的前台可接受性:比
/// [`is_valid_target`] 宽一档 —— **放行桌面**(`Progman`/`WorkerW`):
/// 「框选文件 → 连贴到桌面」就是往桌面上贴文件,拦掉等于废掉这个用法。
/// 任务栏/任务视图/不可激活覆盖层/瞬态壳浮层/本进程/cloaked 仍一律拒绝 ——
/// 历史上 Ctrl+V 真的粘进过不可见的任务栏。
pub fn is_acceptable_paste_foreground(hwnd: isize) -> bool {
    if hwnd <= 0 {
        return false;
    }
    let mut facts = cheap_facts(hwnd);
    if !acceptable_facts(&facts) {
        return false;
    }
    facts.cloaked = is_cloaked(hwnd);
    acceptable_facts(&facts)
}

/// [`is_acceptable_paste_foreground`] 的纯逻辑部分(可单测)。
/// 谓词与 [`decide`] 保持一致:瞬态壳浮层(托盘溢出面板)在 decide 里是
/// `Ignore`(不能当落点),这里同样拒绝 —— 否则"从溢出区打开面板 → 点条目"
/// 会把 Ctrl+V 发给一个点完就消失的浮层。
fn acceptable_facts(facts: &WindowFacts) -> bool {
    if !facts.is_window
        || facts.same_process
        || facts.no_activate
        || !facts.is_visible
        || facts.transient_overlay
    {
        return false;
    }
    !facts.cloaked && (!facts.shell_class || facts.desktop_class)
}

/// 窗口是否属于本进程(面板/托盘等):粘贴按键打给它等于打给自己
pub fn is_own_process_window(hwnd: isize) -> bool {
    if hwnd <= 0 {
        return false;
    }
    let mut pid: u32 = 0;
    // 安全:GetWindowThreadProcessId 只写出一个 DWORD;无效句柄时返回 0 且
    // 不写 pid(保持 0),于是下面判 false,不会误判成"自身进程"
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    thread_id != 0 && pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// 窗口是否被 DWM 遮蔽(UWP 幽灵窗口:IsWindowVisible 为真但用户看不见)
fn is_cloaked(hwnd: isize) -> bool {
    let mut cloaked: u32 = 0;
    // 安全:DwmGetWindowAttribute 写入 4 字节到 cloaked;失败时返回值非 0,
    // 此时 cloaked 保持 0,按"未遮蔽"处理
    let hr = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked, 4) };
    hr == 0 && cloaked != 0
}

/// 窗口是否带 WS_EX_NOACTIVATE:这类窗口(工具条/覆盖层/任务视图)永远不会
/// 成为键盘输入目标,粘进去等于丢内容
fn is_no_activate(hwnd: isize) -> bool {
    // 安全:GetWindowLongPtrW 只读取窗口样式,无副作用
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_NOACTIVATE != 0 }
}

/// 读取窗口类名(失败返回 None)
fn class_name(hwnd: isize) -> Option<String> {
    let mut buf = [0u16; 64];
    // 安全:GetClassNameW 最多写 buf.len() 个 u16,返回写入长度(不含结尾 0)
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// 窗口类名是否属于桌面(`Progman`/`WorkerW`)
fn is_desktop_class(hwnd: isize) -> bool {
    class_name(hwnd).is_some_and(|name| {
        DESKTOP_CLASSES
            .iter()
            .any(|desktop| desktop.eq_ignore_ascii_case(&name))
    })
}

/// 窗口类名是否属于壳(任务栏/任务视图/XAML 岛宿主)。
/// `Windows.UI.Core.CoreWindow` 只有壳宿主进程才算 —— 全屏 UWP 应用
/// (照片/计算器)的顶层窗口类名同样是它,一刀切会把正常应用判成"无落点"。
fn is_shell_class(hwnd: isize) -> bool {
    let Some(name) = class_name(hwnd) else {
        return false;
    };
    if !SHELL_CLASSES
        .iter()
        .any(|shell| shell.eq_ignore_ascii_case(&name))
    {
        return false;
    }
    if name.eq_ignore_ascii_case("Windows.UI.Core.CoreWindow") {
        return is_shell_host_exe(owning_exe_name(hwnd).as_deref());
    }
    true
}

/// 窗口类名是否属于**瞬态壳浮层**(托盘溢出面板)。这类窗口只在打开瞬间
/// 存在,记成落点会让"从溢出区打开面板 → 点条目"退化成只复制。
fn is_transient_overlay(hwnd: isize) -> bool {
    class_name(hwnd).is_some_and(|name| {
        TRANSIENT_OVERLAY_CLASSES
            .iter()
            .any(|overlay| overlay.eq_ignore_ascii_case(&name))
    })
}

/// 该进程名是否属于壳宿主(纯逻辑,便于单测)
pub(crate) fn is_shell_host_exe(exe: Option<&str>) -> bool {
    exe.is_some_and(|name| {
        SHELL_HOST_EXES
            .iter()
            .any(|host| host.eq_ignore_ascii_case(name))
    })
}

/// 读取窗口所属进程的映像名(只查名字,不碰内存、不发消息,回调里也不会阻塞)
fn owning_exe_name(hwnd: isize) -> Option<String> {
    let mut pid: u32 = 0;
    // 安全:全部是"查询后立即 CloseHandle"的短路径;拿不到(权限不足/进程已退出)
    // 就返回 None,调用方按"非壳宿主"处理
    unsafe {
        if GetWindowThreadProcessId(hwnd, &mut pid) == 0 || pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process == 0 {
            return None;
        }
        let mut buf = [0u16; 512];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut size) != 0;
        CloseHandle(process);
        if !ok || size == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        let exe = path.rsplit(['\\', '/']).next()?;
        Some(exe.to_string())
    }
}

/// 前台窗口的判定素材(真机上采集,`decide` 只做纯逻辑判断)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WindowFacts {
    pub is_window: bool,
    pub is_visible: bool,
    pub same_process: bool,
    pub cloaked: bool,
    pub shell_class: bool,
    pub desktop_class: bool,
    pub no_activate: bool,
    /// 瞬态壳浮层(托盘溢出面板):只在打开瞬间存在,不能记成落点
    pub transient_overlay: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackDecision {
    /// 记为新落点
    Record,
    /// 前台变成壳/桌面/不可激活窗口:置 NoTarget(不是忽略)
    Clear,
    /// 与落点无关(无效句柄/自身窗口/不可见/cloaked/瞬态浮层):保持上一条记录不动
    Ignore,
}

/// 记录策略(纯逻辑):顺序即优先级 ——
/// 无效句柄忽略;自身窗口忽略(不能覆盖上一条有效记录);**瞬态壳浮层忽略**
/// (托盘溢出面板消失后落点应回落到用户原来的应用,既不能记成落点、也不该
/// 清空);壳窗口、桌面与不可激活窗口置 NoTarget;不可见/cloaked 忽略
/// (它们"成为前台"是瞬态,不该污染落点)。
pub(crate) fn decide(facts: &WindowFacts) -> TrackDecision {
    if !facts.is_window {
        return TrackDecision::Ignore;
    }
    if facts.same_process {
        return TrackDecision::Ignore;
    }
    if facts.transient_overlay {
        return TrackDecision::Ignore;
    }
    if facts.shell_class || facts.desktop_class || facts.no_activate {
        return TrackDecision::Clear;
    }
    if !facts.is_visible || facts.cloaked {
        return TrackDecision::Ignore;
    }
    TrackDecision::Record
}

/// 把一条前台事件应用到当前状态(纯逻辑,可单测):
/// Record → `Window(hwnd)`;Clear → `NoTarget`;Ignore → 保持原状态。
pub(crate) fn apply(
    prev: TrackedForeground,
    decision: TrackDecision,
    hwnd: isize,
) -> TrackedForeground {
    match decision {
        TrackDecision::Record => TrackedForeground::Window(hwnd),
        TrackDecision::Clear => TrackedForeground::NoTarget,
        TrackDecision::Ignore => prev,
    }
}

/// 采集判定素材。只用**不会跨进程阻塞**的查询(IsWindow / IsWindowVisible /
/// GetWindowThreadProcessId / GetClassNameW / GetWindowLongPtrW,以及只取映像名
/// 的 OpenProcess+QueryFullProcessImageNameW);cloaked 由调用方按需补
/// (见 [`is_valid_target`])。
fn cheap_facts(hwnd: isize) -> WindowFacts {
    if hwnd <= 0 {
        return WindowFacts::default();
    }
    // 安全:全部是只读查询,句柄来自系统回调/GetForegroundWindow;
    // 无效句柄下各 API 只返回错误值,不会崩溃
    unsafe {
        if IsWindow(hwnd) == 0 {
            return WindowFacts::default();
        }
        WindowFacts {
            is_window: true,
            is_visible: IsWindowVisible(hwnd) != 0,
            same_process: is_own_process_window(hwnd),
            cloaked: false,
            shell_class: is_shell_class(hwnd),
            desktop_class: is_desktop_class(hwnd),
            no_activate: is_no_activate(hwnd),
            transient_overlay: is_transient_overlay(hwnd),
        }
    }
}

/// 回调与轮询共用的记账入口:决策纯逻辑在 [`decide`] + [`apply`],
/// 这里只负责把结果写回原子量(状态没变就不写,避免刷屏/无谓唤醒)
fn handle_foreground(hwnd: isize) {
    let decision = decide(&cheap_facts(hwnd));
    let prev = tracked_foreground();
    let next = apply(prev, decision, hwnd);
    match (decision, next) {
        (TrackDecision::Record, TrackedForeground::Window(target)) => {
            LAST_EXTERNAL.store(target, Ordering::SeqCst);
            // 安全:GetTickCount64 无参数、无副作用
            LAST_EXTERNAL_TICK.store(unsafe { GetTickCount64() }, Ordering::SeqCst);
        }
        (TrackDecision::Clear, TrackedForeground::NoTarget) => {
            // 只在"从有窗口变成无落点"时打一行,避免点桌面/任务视图刷屏
            if matches!(prev, TrackedForeground::Window(_)) {
                eprintln!(
                    "[focus] shell/desktop/non-activatable is foreground — paste target cleared"
                );
            }
            LAST_EXTERNAL.store(TRACK_NO_TARGET, Ordering::SeqCst);
            LAST_EXTERNAL_TICK.store(0, Ordering::SeqCst);
        }
        // Ignore:状态保持原样,不碰原子量
        _ => {}
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: isize,
    _event: u32,
    hwnd: isize,
    _id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    // 回调必须极快:只做几次**不跨进程阻塞**的属性查询 + 原子写,
    // 绝不调 DWM/SendMessage 之类的阻塞型 API,否则消息泵被拖住
    handle_foreground(hwnd);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常外部可见窗口作为判定基线,各用例只覆盖自己关心的字段
    fn plain() -> WindowFacts {
        WindowFacts {
            is_window: true,
            is_visible: true,
            ..Default::default()
        }
    }

    /// 正常外部窗口 → 记录
    #[test]
    fn record_plain_external_window() {
        assert_eq!(decide(&plain()), TrackDecision::Record);
    }

    /// 无明确落点路径的前台可接受性:与 decide 的谓词保持一致 ——
    /// 桌面放行(连贴文件到桌面),任务栏/覆盖层/瞬态壳浮层/本进程/cloaked 拒绝
    #[test]
    fn acceptable_foreground_matches_decide_predicates() {
        assert!(acceptable_facts(&plain()), "普通外部窗口可接受");
        assert!(
            acceptable_facts(&WindowFacts {
                shell_class: true,
                desktop_class: true,
                ..plain()
            }),
            "桌面必须放行:框选文件连贴到桌面"
        );
        for (label, facts) in [
            (
                "任务栏/壳窗口",
                WindowFacts {
                    shell_class: true,
                    ..plain()
                },
            ),
            (
                "瞬态壳浮层",
                WindowFacts {
                    transient_overlay: true,
                    ..plain()
                },
            ),
            (
                "不可激活覆盖层",
                WindowFacts {
                    no_activate: true,
                    ..plain()
                },
            ),
            (
                "本进程",
                WindowFacts {
                    same_process: true,
                    ..plain()
                },
            ),
            (
                "不可见",
                WindowFacts {
                    is_visible: false,
                    ..plain()
                },
            ),
            (
                "cloaked",
                WindowFacts {
                    cloaked: true,
                    ..plain()
                },
            ),
        ] {
            assert!(!acceptable_facts(&facts), "{label} 不可作为落点");
        }
        assert!(!acceptable_facts(&WindowFacts::default()), "无效句柄");
    }

    /// 自身进程窗口 → 忽略(不覆盖上一条有效记录)
    #[test]
    fn ignore_own_process_window() {
        assert_eq!(
            decide(&WindowFacts {
                same_process: true,
                ..plain()
            }),
            TrackDecision::Ignore,
            "点面板/托盘不能把外部落点冲掉"
        );
    }

    /// 壳/桌面窗口 → 清空(而不是忽略):点完桌面后不该再拉回几分钟前的应用
    #[test]
    fn clear_on_shell_and_desktop() {
        assert_eq!(
            decide(&WindowFacts {
                shell_class: true,
                ..plain()
            }),
            TrackDecision::Clear
        );
        assert_eq!(
            decide(&WindowFacts {
                desktop_class: true,
                ..plain()
            }),
            TrackDecision::Clear
        );
    }

    /// WS_EX_NOACTIVATE 覆盖层(任务视图/工具条)→ 清空:它接不了键盘输入
    #[test]
    fn clear_on_non_activatable_window() {
        assert_eq!(
            decide(&WindowFacts {
                no_activate: true,
                ..plain()
            }),
            TrackDecision::Clear
        );
    }

    /// 瞬态壳浮层(托盘溢出面板 `NotifyIconOverflowWindow` /
    /// `TopLevelWindowForOverflowXamlIsland`)→ **忽略**:
    /// 既不能记成落点(浮层一点就隐藏、句柄随即失效,会让"从溢出区打开面板 →
    /// 点条目"退化成只复制),也不能清空记录 —— 保留上一条有效记录,
    /// 落点自然回落到用户原来的应用。
    #[test]
    fn ignore_transient_tray_overflow_overlay() {
        let overlay = WindowFacts {
            transient_overlay: true,
            ..plain()
        };
        assert_eq!(decide(&overlay), TrackDecision::Ignore);
        assert_eq!(
            apply(TrackedForeground::Window(7), decide(&overlay), 0x99),
            TrackedForeground::Window(7),
            "浮层不能冲掉上一条有效落点"
        );
        // 溢出浮层常同时带 WS_EX_NOACTIVATE:仍必须是忽略,不能滑到 Clear
        let overlay_no_activate = WindowFacts {
            transient_overlay: true,
            no_activate: true,
            ..plain()
        };
        assert_eq!(decide(&overlay_no_activate), TrackDecision::Ignore);
    }

    /// 无效句柄 / 不可见 / cloaked → 忽略
    #[test]
    fn ignore_invalid_invisible_and_cloaked() {
        assert_eq!(
            decide(&WindowFacts::default()),
            TrackDecision::Ignore,
            "无效句柄"
        );
        assert_eq!(
            decide(&WindowFacts {
                is_visible: false,
                ..plain()
            }),
            TrackDecision::Ignore
        );
        assert_eq!(
            decide(&WindowFacts {
                cloaked: true,
                ..plain()
            }),
            TrackDecision::Ignore,
            "UWP 幽灵窗口不能当落点"
        );
    }

    /// 自身窗口判定优先于壳/不可激活判定:本进程窗口即使类名撞上也不清空记录
    #[test]
    fn own_process_wins_over_shell_and_noactivate() {
        assert_eq!(
            decide(&WindowFacts {
                same_process: true,
                shell_class: true,
                desktop_class: true,
                no_activate: true,
                ..plain()
            }),
            TrackDecision::Ignore
        );
    }

    // ---------- 状态迁移(纯逻辑) ----------

    /// Record:无论之前是什么状态,都变成这个新窗口
    #[test]
    fn apply_record_moves_to_window() {
        for prev in [
            TrackedForeground::Unknown,
            TrackedForeground::NoTarget,
            TrackedForeground::Window(1),
        ] {
            assert_eq!(
                apply(prev, TrackDecision::Record, 7),
                TrackedForeground::Window(7)
            );
        }
    }

    /// Clear:从"有窗口"变成 NoTarget(点桌面后必须能区分出"别激活任何窗口")
    #[test]
    fn apply_clear_moves_window_to_no_target() {
        assert_eq!(
            apply(TrackedForeground::Window(7), TrackDecision::Clear, 0),
            TrackedForeground::NoTarget
        );
        assert_eq!(
            apply(TrackedForeground::Unknown, TrackDecision::Clear, 0),
            TrackedForeground::NoTarget
        );
    }

    /// Ignore:保持原状态(自身窗口/瞬时不可见不能冲掉有效记录)
    #[test]
    fn apply_ignore_keeps_previous() {
        assert_eq!(
            apply(TrackedForeground::Window(7), TrackDecision::Ignore, 9),
            TrackedForeground::Window(7)
        );
        assert_eq!(
            apply(TrackedForeground::NoTarget, TrackDecision::Ignore, 9),
            TrackedForeground::NoTarget
        );
        assert_eq!(
            apply(TrackedForeground::Unknown, TrackDecision::Ignore, 9),
            TrackedForeground::Unknown
        );
    }

    /// Clear 之后再 Record 能恢复(点完桌面又切回应用,应重新成为落点)
    #[test]
    fn apply_clear_then_record_recovers() {
        let after_clear = apply(TrackedForeground::Window(7), TrackDecision::Clear, 0);
        assert_eq!(after_clear, TrackedForeground::NoTarget);
        assert_eq!(
            apply(after_clear, TrackDecision::Record, 8),
            TrackedForeground::Window(8)
        );
    }

    // ---------- 目标选择优先级(三态 → Option,纯逻辑) ----------

    /// Unknown(冷启动)+ 有有效捕获值 → 回退捕获值(旧行为)
    #[test]
    fn unknown_falls_back_to_captured() {
        assert_eq!(
            choose_paste_target(TrackedForeground::Unknown, 0xA, |_| true),
            Some(0xA)
        );
    }

    /// Unknown + captured == 0(托盘/命令打开)→ Some(0):允许 Z 序兜底/隐式激活
    #[test]
    fn unknown_without_capture_yields_zero_for_fallbacks() {
        assert_eq!(
            choose_paste_target(TrackedForeground::Unknown, 0, |_| true),
            Some(0),
            "托盘/冷启动没有记录时,老路径(Z 序兜底/隐式激活)必须保留"
        );
    }

    /// Unknown 且捕获值也不可用(已关闭/是自身窗口)→ None(只复制)
    #[test]
    fn unknown_with_invalid_capture_yields_none() {
        assert_eq!(
            choose_paste_target(TrackedForeground::Unknown, 0xA, |_| false),
            None
        );
    }

    /// NoTarget(点了桌面/任务栏)→ None = 只复制,**不得**回退 captured,
    /// 也不得退化成 Some(0) 让 KeepOpen 的 Z 序兜底去粘随机窗口
    #[test]
    fn no_target_yields_none() {
        assert_eq!(
            choose_paste_target(TrackedForeground::NoTarget, 0xA, |_| true),
            None,
            "点完桌面再点条目,不能把唤出前的旧应用拉回前台"
        );
        assert_eq!(
            choose_paste_target(TrackedForeground::NoTarget, 0, |_| true),
            None,
            "也不能退化成 Some(0) 走 Z 序兜底"
        );
    }

    /// Window(可用)→ 用它(而不是唤出时捕获的旧目标)
    #[test]
    fn prefer_tracked_window() {
        assert_eq!(
            choose_paste_target(TrackedForeground::Window(0xB), 0xA, |_| true),
            Some(0xB),
            "用户在面板打开后切到另一个应用,落点应是那个应用"
        );
    }

    /// Window(已失效)→ None = 只复制,同样不回退 captured
    #[test]
    fn stale_tracked_window_yields_none() {
        assert_eq!(
            choose_paste_target(TrackedForeground::Window(0xB), 0xA, |_| false),
            None,
            "记录里的窗口已关闭时,不能回退到陈旧的捕获值"
        );
    }

    /// 记录值为 0 视为无记录(不会被当成"有效窗口 0")
    #[test]
    fn zero_tracked_value_is_not_a_target() {
        assert_eq!(
            choose_paste_target(TrackedForeground::Window(0), 0xA, |h| h == 0),
            None
        );
    }

    // ---------- 壳宿主判定(纯逻辑) ----------

    /// 只有壳宿主的 CoreWindow 才算壳;全屏 UWP 应用(照片/计算器)不算
    #[test]
    fn only_shell_hosts_are_shell_hosts() {
        for host in [
            "StartMenuExperienceHost.exe",
            "SearchHost.exe",
            "SearchApp.exe",
            "ShellExperienceHost.exe",
            "searchhost.exe", // 大小写不敏感
        ] {
            assert!(is_shell_host_exe(Some(host)), "应识别为壳宿主: {host}");
        }
        for app in [
            "ApplicationFrameHost.exe",
            "Photos.exe",
            "Calculator.exe",
            "explorer.exe",
        ] {
            assert!(!is_shell_host_exe(Some(app)), "不是壳宿主: {app}");
        }
        assert!(!is_shell_host_exe(None), "拿不到进程名时按非壳处理");
    }
}
