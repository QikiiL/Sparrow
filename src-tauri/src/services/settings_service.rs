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

fn run_schtasks(args: &[&str]) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW:应用在 UI 会话里跑 schtasks,别闪控制台黑窗
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("无法执行 schtasks: {}", e))
}

pub fn enable_autostart(exe_path: &str) -> Result<(), String> {
    let tr = format!("\"{}\" --minimized", exe_path);
    let output = run_schtasks(&[
        "/Create", "/TN", AUTOSTART_TASK, "/TR", &tr, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
    ])?;
    if !output.status.success() {
        return Err(format!(
            "创建自启动任务失败: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

pub fn disable_autostart() -> Result<(), String> {
    // 任务本就不存在视为已关闭,不算失败(幂等)
    let query = run_schtasks(&["/Query", "/TN", AUTOSTART_TASK])?;
    if !query.status.success() {
        return Ok(());
    }
    let output = run_schtasks(&["/Delete", "/TN", AUTOSTART_TASK, "/F"])?;
    if !output.status.success() {
        return Err(format!(
            "删除自启动任务失败: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// 品牌改名迁移:把 1.x 的旧自启动任务换成新任务名,并让新任务与当前设置一致。
///
/// 无条件 best-effort 删除旧任务(不存在/删除失败都只记日志,绝不阻断启动);
/// 随后按当前设置收敛新任务:
/// - start_with_windows = true  → 用当前 exe 重建新任务(enable_autostart 幂等)
/// - start_with_windows = false → 确保新任务不存在(disable_autostart 幂等)
///
/// 在 setup 里调用一次:老用户升级后不会新旧两个任务同时开机拉起,
/// 同时保留原先"任务被安全软件清理后自愈"的语义。
pub fn migrate_autostart_task(app_handle: &tauri::AppHandle) {
    match run_schtasks(&["/Delete", "/TN", LEGACY_AUTOSTART_TASK, "/F"]) {
        Ok(output) if output.status.success() => {
            eprintln!("Removed legacy autostart task: {}", LEGACY_AUTOSTART_TASK);
        }
        // 旧任务本就不存在:schtasks 返回非 0,属正常情况
        Ok(_) => {}
        Err(e) => eprintln!("Failed to remove legacy autostart task: {}", e),
    }

    let settings = load_settings(app_handle);
    let result = if settings.start_with_windows {
        std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|p| enable_autostart(&p.to_string_lossy()))
    } else {
        disable_autostart()
    };
    if let Err(e) = result {
        eprintln!("Autostart migration failed: {}", e);
    }
}

/// settings.json 固定放在程序目录的 config/ 下(绝对路径传入,
/// 不走插件默认的 %APPDATA% 解析)
fn store_path() -> std::path::PathBuf {
    crate::app_config_dir().join(STORE_FILE)
}

pub fn load_settings(app_handle: &tauri::AppHandle) -> AppSettings {
    let store = app_handle.store(store_path());
    let mut settings = match store {
        Ok(store) => {
            if let Some(value) = store.get(STORE_KEY) {
                serde_json::from_value(value.clone()).unwrap_or_default()
            } else {
                AppSettings::default()
            }
        }
        Err(e) => {
            eprintln!("Failed to load settings store: {}", e);
            AppSettings::default()
        }
    };
    // 旧默认热键值跟随新默认(见 AppSettings::migrate_legacy_seq_hotkey)
    settings.migrate_legacy_seq_hotkey();
    settings
}

pub fn save_settings(app_handle: &tauri::AppHandle, settings: &AppSettings) -> Result<(), String> {
    // 取改动前的值:自动启动只在开关真正变化时同步,避免每次保存都触碰注册表
    let previous = load_settings(app_handle);

    let store = app_handle
        .store(store_path())
        .map_err(|e| e.to_string())?;
    let value = serde_json::to_value(settings).map_err(|e| e.to_string())?;
    store.set(STORE_KEY, value);
    store.save().map_err(|e| e.to_string())?;

    // 排除规则改动立即生效(轮询线程读的是 ExclusionState,不是这份文件)。
    // 刷新失败不能让保存失败:设置已落盘,规则最多等下次启动再加载
    if let Some(state) = app_handle.try_state::<crate::services::exclusion_service::ExclusionState>()
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

    // 同步自动启动状态（最佳努力，不影响设置保存）
    if settings.start_with_windows != previous.start_with_windows {
        let result = if settings.start_with_windows {
            std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|p| enable_autostart(&p.to_string_lossy()))
        } else {
            disable_autostart()
        };
        if let Err(e) = result {
            eprintln!("Failed to sync autostart: {}", e);
        }
    }

    Ok(())
}
