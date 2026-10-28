use crate::models::settings::AppSettings;
use crate::services::settings_service::AutostartStatus;
use tauri::Manager;

#[tauri::command]
pub fn load_settings(app_handle: tauri::AppHandle) -> Result<AppSettings, String> {
    Ok(crate::services::settings_service::load_settings(
        &app_handle,
    ))
}

#[tauri::command]
pub fn save_settings(app_handle: tauri::AppHandle, settings: AppSettings) -> Result<(), String> {
    crate::services::settings_service::save_settings(&app_handle, &settings)
}

/// 开机自启动的真实状态。设置面板打开时查一次:配置文件里的布尔值只代表
/// 用户意图,任务可能被安全软件/清理工具删掉,两者必须分开显示
#[tauri::command]
pub fn get_autostart_status(_app_handle: tauri::AppHandle) -> AutostartStatus {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    crate::services::settings_service::autostart_status(&exe)
}

/// 「立即修复」:按当前 exe 重建自启动任务(enable_autostart 内部会回查校验),
/// 返回修复后的真实状态供前端直接刷新提示
#[tauri::command]
pub fn repair_autostart(_app_handle: tauri::AppHandle) -> Result<AutostartStatus, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("无法获取当前程序路径: {}", e))?
        .to_string_lossy()
        .to_string();
    crate::services::settings_service::enable_autostart(&exe)?;
    Ok(crate::services::settings_service::autostart_status(&exe))
}

/// 「粘贴后保持打开」:开启后单击条目不销毁面板,只把前台焦点还给目标窗口。
/// 与 set_always_on_top 同款模式:即改即存,下次单击立即生效
#[tauri::command]
pub fn set_keep_open(app_handle: tauri::AppHandle, keep_open: bool) -> Result<(), String> {
    let mut settings = crate::services::settings_service::load_settings(&app_handle);
    settings.keep_open_on_paste = keep_open;
    crate::services::settings_service::save_settings(&app_handle, &settings)
}

/// 豁免条目上限。名单只增不减会随使用时间无限膨胀,而老条目几乎不会再被复制
/// 到;超过上限时丢弃最早加入的(新条目追加在尾部,头部即最早)
const ALLOWLIST_MAX: usize = 200;

/// 用户点了「仍要记录」:把该内容加入豁免名单,并请求轮询线程重新捕获一次。
///
/// 这里不需要把内容再传一遍后端 —— 被排除的内容此刻仍留在系统剪贴板里,
/// 只要清掉去重用的 last_hash,下一轮轮询(≤500ms)就会重新走入库流程。
#[tauri::command]
pub fn allow_excluded_item(app_handle: tauri::AppHandle, hash: String) -> Result<(), String> {
    let hash = hash.trim().to_string();
    if hash.is_empty() {
        return Ok(());
    }

    let mut settings = crate::services::settings_service::load_settings(&app_handle);
    if !settings.excluded_allowlist.contains(&hash) {
        settings.excluded_allowlist.push(hash.clone());
        if settings.excluded_allowlist.len() > ALLOWLIST_MAX {
            let excess = settings.excluded_allowlist.len() - ALLOWLIST_MAX;
            settings.excluded_allowlist.drain(..excess);
        }
        // save_settings 内部会 reload ExclusionState,白名单随后即刻生效
        crate::services::settings_service::save_settings(&app_handle, &settings)?;
    }

    // 无论本次是否新增都请求重捕获:用户点按钮的意图是"把这条记进去",
    // 已豁免却因去重未能入库的情况(例如应用刚重启)同样应该补上
    if let Some(state) =
        app_handle.try_state::<crate::services::exclusion_service::ExclusionState>()
    {
        state.request_recapture(hash);
    }

    Ok(())
}
