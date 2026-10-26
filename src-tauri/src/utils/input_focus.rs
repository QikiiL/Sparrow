use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

#[link(name = "user32")]
extern "system" {
    fn GetForegroundWindow() -> isize;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn GetWindow(hwnd: isize, cmd: u32) -> isize;
    fn IsWindowEnabled(hwnd: isize) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    // 与 foreground_tracker 保持一致:统一用 GetWindowLongPtrW(x64 上的标准取法)
    fn GetWindowLongPtrW(hwnd: isize, nindex: i32) -> isize;
    fn SetWindowPos(
        hwnd: isize,
        insert_after: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
}

const GW_HWNDNEXT: u32 = 2;
const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOPMOST: isize = 0x0000_0008;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;
const HWND_TOPMOST: isize = -1;
const SW_RESTORE: i32 = 9;

/// 焦点交接抑制标志:应用正在把前台交给**外部目标窗口**期间为 true。
///
/// 背景:窗口创建后 `window_manager::ensure_focus` 会在后台线程里反复
/// `set_focus`,直到确认拿到前台(最长约 0.75 秒)。用户唤出面板后很快点击
/// 条目时,这段重试仍在跑 —— 「粘贴后保持打开」刚把前台交给目标窗口,
/// 重试线程立刻又把焦点抢回本面板,随后模拟的 Ctrl+V 就落进了面板自己的
/// 搜索框,表现为"点了没粘贴"。交接期间把该标志置 true,重试线程据此立即
/// 停手(见 ensure_focus)。
///
/// 语义:只是"别抢焦点"的抑制信号,不影响任何 Win32 调用本身。
static FOCUS_HANDOFF: AtomicBool = AtomicBool::new(false);

/// 标记"开始把前台交给外部目标窗口"。必须与 [`end_focus_handoff`] 成对,
/// 否则重试线程被永久抑制(面板唤出后可能"可见但非前台")。
pub fn begin_focus_handoff() {
    FOCUS_HANDOFF.store(true, Ordering::SeqCst);
}

/// 结束交接:恢复"可以抢回焦点"的常态。幂等。
pub fn end_focus_handoff() {
    FOCUS_HANDOFF.store(false, Ordering::SeqCst);
}

/// 当前是否处于焦点交接中(为 true 时任何"把焦点抢回本面板"的重试都必须停止)
pub fn is_focus_handoff_active() -> bool {
    FOCUS_HANDOFF.load(Ordering::SeqCst)
}

/// 直接把窗口提到置顶层。必须绕过 tao 的 `set_always_on_top`:它走"账面
/// 标志差量",账面已是 true 时不会再发任何 Win32 调用 —— 而创建竞态可能
/// 让那唯一一次 SetWindowPos 静默失败(错误被吞、账面不回滚),之后置顶
/// 样式缺失却永远补不回来,表现即"面板不悬浮且反复隐藏唤出无法恢复"。
/// SWP_NOACTIVATE:只改层级,不抢焦点
pub fn force_topmost(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) != 0
    }
}

/// 捕获当前前台窗口句柄。必须在本应用窗口显示之前调用,
/// 此刻前台还是目标应用,粘贴按键要发还给这个窗口。
pub fn capture_foreground_hwnd() -> isize {
    unsafe { GetForegroundWindow() }
}

/// 隐藏本应用窗口后,主动把前台焦点还给目标窗口,并等待其真正就绪。
/// SetForegroundWindow 的调用方需为前台进程或最近接收输入的进程——
/// 用户刚在我们的窗口里点击过,满足条件。
/// 目标最小化时先 SW_RESTORE:最小化窗口的 IsWindowVisible 仍为真,但
/// SetForegroundWindow 对最小化窗口只会闪一下任务栏,焦点不会真正过去,
/// 随后模拟的 Ctrl+V 就落空(表现为"点了没粘贴")。
/// 返回目标窗口是否已成为前台。
pub fn restore_target_focus(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        for _ in 0..25 {
            if GetForegroundWindow() == hwnd {
                return true;
            }
            SetForegroundWindow(hwnd);
            std::thread::sleep(Duration::from_millis(20));
        }
        GetForegroundWindow() == hwnd
    }
}

/// 发送 Ctrl+V 前的最后一道闸:确认前台确实是要粘贴的窗口。
///
/// - `target != 0`(本次粘贴有明确落点):要求 `GetForegroundWindow() == target`;
/// - `target == 0`(托盘打开、连续粘贴等"无明确落点"的旧路径):要求前台通过
///   [`crate::utils::foreground_tracker::is_acceptable_paste_foreground`] ——
///   它排掉任务栏/任务视图/不可激活覆盖层/cloaked/本进程窗口(否则按键会打回
///   自己的面板搜索框,或"粘进"不可见的任务栏——历史上真实发生过),
///   但**放行桌面**(「框选文件 → 连贴到桌面」是合法用法)。
///
/// 两种情况都最多重试 15×20ms(约 300ms):覆盖焦点切换、以及销毁窗口后
/// Windows 隐式激活的尾部延迟(机器繁忙时 150ms 往往不够)。
/// 返回 false 时调用方必须放弃模拟按键(剪贴板内容照常写入,用户手动 Ctrl+V
/// 仍然可用),宁可"只复制"也绝不把内容粘进错误的软件。
pub fn wait_for_paste_target(target: isize) -> bool {
    const ATTEMPTS: usize = 15;
    for attempt in 0..ATTEMPTS {
        let foreground = unsafe { GetForegroundWindow() };
        let ok = if target != 0 {
            foreground == target
        } else {
            // 无明确落点时允许桌面(「框选文件 → 连贴到桌面」是合法用法),
            // 但任务栏/任务视图/不可激活覆盖层/本进程一律拒绝
            crate::utils::foreground_tracker::is_acceptable_paste_foreground(foreground)
        };
        if ok {
            return true;
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    false
}

/// 窗口是否为当前前台窗口。tao/tauri 的 is_focused 标志基于 WM_ACTIVATE /
/// WM_SETFOCUS 消息异步更新,窗口刚创建或刚显示时会滞后;
/// 这里直接用原生 GetForegroundWindow 比对,作为判定兜底。
pub fn is_foreground_window(hwnd: isize) -> bool {
    hwnd != 0 && unsafe { GetForegroundWindow() } == hwnd
}

/// 窗口是否带 WS_EX_TOPMOST 扩展样式(置顶守护线程据此发现置顶被外部剥除)
pub fn is_topmost(hwnd: isize) -> bool {
    hwnd != 0 && unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } & WS_EX_TOPMOST != 0
}

/// 找出本窗口若被销毁时 Windows 会隐式激活的窗口:Z 序上本窗口下方第一个
/// 可激活的**普通层**窗口。找不到返回 0。
/// 「粘贴后保持打开」在冻结落点为 `Some(0)`(托盘/冷启动没有记录)时的
/// **最后兜底**,对齐销毁路径的隐式激活结果。
/// 候选过滤直接复用 `foreground_tracker::is_valid_target`(置顶、壳/桌面、
/// cloaked、本进程窗口、`WS_EX_NOACTIVATE` 覆盖层全在里面),再加一条
/// "窗口必须是启用态"——禁用的窗口(模态对话框背后的父窗口)贴不进去。
pub fn find_window_below(our_hwnd: isize) -> isize {
    unsafe {
        let mut hwnd = GetWindow(our_hwnd, GW_HWNDNEXT);
        while hwnd != 0 {
            let topmost = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST != 0;
            if !topmost
                && IsWindowEnabled(hwnd) != 0
                && crate::utils::foreground_tracker::is_valid_target(hwnd)
            {
                return hwnd;
            }
            hwnd = GetWindow(hwnd, GW_HWNDNEXT);
        }
        0
    }
}

/// 单击条目的目标行为(true = 粘贴到之前聚焦的输入框)与目标窗口句柄。
/// 默认粘贴;"仅复制"由前端状态栏手动切换,会话内保持。
pub struct PasteMode {
    input: Mutex<bool>,
    target: Mutex<isize>,
}

impl PasteMode {
    pub fn new() -> Self {
        Self {
            input: Mutex::new(true),
            target: Mutex::new(0),
        }
    }

    pub fn get(&self) -> bool {
        *self.input.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, value: bool) {
        *self.input.lock().unwrap_or_else(|e| e.into_inner()) = value;
    }

    pub fn set_target(&self, hwnd: isize) {
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = hwnd;
    }

    pub fn target(&self) -> isize {
        *self.target.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言失败/恐慌时也要复位全局标志,否则会污染同进程内后续测试
    struct ResetHandoff;
    impl Drop for ResetHandoff {
        fn drop(&mut self) {
            end_focus_handoff();
        }
    }

    /// 交接标志的 begin/end 往返:初始未抑制 → begin 后抑制 → end 后恢复。
    /// FOCUS_HANDOFF 是进程级静态量,本模块只有这一个测试读写它,
    /// 故不需要额外串行保护;若将来新增读它的测试,须与本测试合并或加锁。
    #[test]
    fn focus_handoff_flag_roundtrip() {
        end_focus_handoff();
        assert!(!is_focus_handoff_active(), "初始应为未抑制");

        begin_focus_handoff();
        let _reset = ResetHandoff;
        assert!(is_focus_handoff_active(), "begin 后应处于抑制态");

        end_focus_handoff();
        assert!(!is_focus_handoff_active(), "end 后应恢复未抑制");
    }
}
