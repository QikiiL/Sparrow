mod commands;
mod models;
mod services;
mod utils;

/// 行业标准布局(XDG 惯例):配置与数据跟随程序目录,绿色便携。
/// `<exe所在目录>\config` = 设置(settings.json / 存储位置指针 / 窗口状态);
/// `<exe所在目录>\data`   = 数据默认位置(clipboard.db / images,可在设置中改)。
/// 取不到 exe 路径时兜底旧的 %APPDATA% 标识符目录。
fn legacy_appdata_dir() -> std::path::PathBuf {
    let app_identifier = "com.lyz.clipboard-manager-tauri";
    let config_dir = if cfg!(target_os = "windows") {
        std::env::var("APPDATA").unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/Library/Application Support", home)
    } else {
        let home = std::env::var("HOME").unwrap_or_default();
        match std::env::var("XDG_CONFIG_HOME") {
            Ok(path) => path,
            Err(_) => format!("{}/.config", home),
        }
    };
    std::path::PathBuf::from(config_dir).join(app_identifier)
}

fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(legacy_appdata_dir)
}

pub(crate) fn app_config_dir() -> std::path::PathBuf {
    exe_dir().join("config")
}

pub(crate) fn app_data_dir_default() -> std::path::PathBuf {
    exe_dir().join("data")
}

/// 首次以新布局启动时,从旧 %APPDATA% 位置继承配置与(默认位置的)数据。
/// 只在新 config 目录尚无 settings.json 时执行一次性迁移;
/// 若旧配置里有自定义存储位置指针,数据不动,仅指针随配置继承。
fn migrate_legacy_layout() {
    let config = app_config_dir();
    if config.join("settings.json").exists() {
        return; // 已迁移过
    }
    let legacy = legacy_appdata_dir();
    if !legacy.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(&config);

    for name in ["settings.json", "storage.json", "window-state.json"] {
        let src = legacy.join(name);
        if src.is_file() {
            let _ = std::fs::copy(&src, config.join(name));
        }
    }

    // 无自定义存储位置指针时,旧默认位置的数据库与图片迁到新 data 目录
    let has_custom_pointer = config
        .join("storage.json")
        .is_file()
        && std::fs::read_to_string(config.join("storage.json"))
            .map(|s| s.contains("\"data_dir\": \"") && !s.contains("\"data_dir\": null"))
            .unwrap_or(false);
    if !has_custom_pointer && legacy.join("clipboard.db").is_file() {
        let data = app_data_dir_default();
        let _ = std::fs::create_dir_all(&data);
        for name in ["clipboard.db", "clipboard.db-wal", "clipboard.db-shm"] {
            let src = legacy.join(name);
            if src.is_file() {
                let _ = std::fs::copy(&src, data.join(name));
            }
        }
        let legacy_images = legacy.join("images");
        if legacy_images.is_dir() {
            let images = data.join("images");
            let _ = std::fs::create_dir_all(&images);
            for entry in std::fs::read_dir(&legacy_images).into_iter().flatten().flatten() {
                if entry.path().is_file() {
                    let _ = std::fs::copy(entry.path(), images.join(entry.file_name()));
                }
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 行业标准布局:config/ 与 data/ 跟随程序目录;
    // 首次启动从旧 %APPDATA% 位置继承配置(见 migrate_legacy_layout)。
    // 数据库与图片的实际位置由 storage_service 解析(可能被用户改到别的目录)
    migrate_legacy_layout();
    let default_config_dir = app_config_dir();
    let _ = std::fs::create_dir_all(&default_config_dir);
    // data 文件夹无条件确保存在:布局完整,也是“恢复默认”的落点
    let _ = std::fs::create_dir_all(app_data_dir_default());
    let data_dir = services::storage_service::resolve_data_dir();
    let _ = std::fs::create_dir_all(&data_dir);
    let db_path = data_dir.join("clipboard.db");
    let db_url_for_plugin = format!("sqlite:{}", db_path.to_string_lossy().replace('\\', "/"));

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::default().build())
        // 开机自启动改用任务计划程序(见 settings_service):提权应用无法
        // 经注册表 Run 键自启动,插件方案对本应用无效,故不再注册
        .plugin(
            tauri_plugin_sql::Builder::default()
                .add_migrations(&db_url_for_plugin, get_migrations())
                .build(),
        )
        .setup(move |app| {
            use tauri::Manager;

            services::tray_service::create_tray(app)?;

            // Use the pre-computed db_path for the sqlx pool (same path as plugin-sql).
            // 走文件路径而非 URL 字符串,避免中文目录/特殊前缀破坏连接串
            let db_file = db_path.clone();

            let db = tauri::async_runtime::block_on(async {
                // create_if_missing:全新安装(或存储位置刚切换)时库文件尚不存在,
                // 由后端创建;随后上方的手动迁移会建好表结构
                let options = sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&db_file)
                    .create_if_missing(true);
                let pool = sqlx::sqlite::SqlitePool::connect_with(options)
                    .await
                    .expect("Failed to connect to SQLite");

                // Enable WAL mode to prevent lock contention with tauri_plugin_sql
                sqlx::query("PRAGMA journal_mode=WAL")
                    .execute(&pool)
                    .await
                    .expect("Failed to enable WAL mode");

                // Set busy timeout so writers wait instead of failing immediately
                sqlx::query("PRAGMA busy_timeout=5000")
                    .execute(&pool)
                    .await
                    .expect("Failed to set busy timeout");

                // Run migrations manually
                for migration in get_migrations() {
                    sqlx::query(migration.sql)
                        .execute(&pool)
                        .await
                        .expect("Failed to run migration");
                }

                // 003(html_path/rtf_path)不在共用迁移列表里(ALTER 非幂等,
                // 见 get_migrations 注释),单独走列探测守卫式执行
                apply_rich_format_columns(&pool)
                    .await
                    .expect("Failed to apply migration 003_add_rich_formats");

                pool
            });

            app.manage(db.clone());

            // 数据目录(可能为用户自定义位置),供各处统一解析 db/images 路径
            app.manage(services::storage_service::StorageDir(data_dir.clone()));

            // 前端缩略图经 asset 协议加载;conf 里的作用域只覆盖默认位置,
            // 自定义存储目录必须在这里动态授权,否则迁移后图片全部显示占位符
            let images_dir = data_dir.join("images");
            let _ = std::fs::create_dir_all(&images_dir);
            if let Err(e) = app
                .asset_protocol_scope()
                .allow_directory(&images_dir, false)
            {
                eprintln!("Failed to extend asset protocol scope: {}", e);
            }

            // formats 目录(CF_HTML/RTF 原始字节):**预防性授权**。
            // 本次富文本预览并不依赖它 —— 前端拿的是 get_rich_preview 命令
            // 返回的净化片段,图片走 srcDoc 里的 data: URI,全程不经过 asset
            // 协议。这里授权只是为了与 images 对齐:自定义存储位置下任何
            // 将来按路径直读 formats 的实现不会被作用域挡掉
            let formats_dir = services::storage_service::formats_dir(app.handle());
            let _ = std::fs::create_dir_all(&formats_dir);
            if let Err(e) = app
                .asset_protocol_scope()
                .allow_directory(&formats_dir, false)
            {
                eprintln!("Failed to extend asset protocol scope for formats: {}", e);
            }

            // 单击条目的目标行为(默认粘贴,可在状态栏手动切换)与粘贴目标窗口
            app.manage(utils::input_focus::PasteMode::new());

            // 最近外部前台窗口跟踪器:粘贴落点必须在"点击条目那一刻"确定,
            // 而热键唤出时的捕获只反映唤出瞬间的前台。面板默认置顶且失焦
            // 不隐藏,用户"唤出面板 → 点另一个应用的输入框 → 回来点条目"
            // 是常规操作,靠陈旧捕获会把内容粘回上一个软件(见
            // utils/foreground_tracker)。线程随进程存活,失败降级为轮询
            utils::foreground_tracker::start();

            // 连续粘贴(框选模式)的 FIFO 队列:窗口"隐藏即销毁"会带走前端
            // 状态,队列必须活在整个应用生命周期里
            app.manage(services::continuous_paste::ContinuousPasteState::new());

            // 排除规则状态:轮询线程据此跳过密码管理器/敏感内容,不入库。
            // 必须早于 start_polling 载入配置,否则首轮轮询用空规则跑
            let exclusion_state = crate::services::exclusion_service::ExclusionState::new();
            exclusion_state.reload(app.handle());
            app.manage(exclusion_state);

            // Register clipboard monitor with polling.
            // Apply the persisted `paused` state so a user who paused monitoring
            // before quitting stays paused after restart.
            let saved_settings = services::settings_service::load_settings(app.handle());
            let monitor = services::clipboard_monitor::ClipboardMonitor::new();
            if saved_settings.paused {
                tauri::async_runtime::block_on(monitor.set_paused(true));
            }
            app.manage(monitor.clone());
            services::clipboard_monitor::ClipboardMonitor::start_polling(
                app.handle().clone(),
                db.clone(),
                monitor,
            );

            // 云同步:轮询循环 + 运行句柄。start 只 spawn,不做网络 IO,
            // 配置缺失/凭证缺失都只体现为同步状态 error,绝不影响应用启动。
            // 命令层(get_sync_status/sync_now/...)与设置保存都通过这个句柄。
            let sync_handle = services::sync::service::start(
                app.handle().clone(),
                db.clone(),
                app_config_dir(),
            );
            app.manage(sync_handle);

            // 短信验证码捕获:轮询通知中心(TeleLink 同款路径)。线程常驻,
            // 靠 ENABLED 原子开关决定每轮是否真正读通知,关闭时零开销
            services::sms_code_service::set_enabled(saved_settings.sms_code_enabled);
            services::sms_code_service::spawn(app.handle().clone());

            // 后台更新监视:每 2 小时主动检查,发现新版本走前端事件/系统通知。
            // 前端唤出时的检查保留(30 分钟节流),但用户不唤出面板也能被通知到
            commands::update::spawn_update_watcher(app.handle().clone());

            // Register global hotkey and track it for unregistration.
            // If the user has a custom hotkey, register that instead of the default
            // Ctrl+Shift+V.
            use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};
            let mut startup_shortcut = if saved_settings.win_v_integration {
                Shortcut::new(Some(Modifiers::SUPER), Code::KeyV)
            } else {
                utils::hotkey::build_shortcut(
                    &saved_settings.hotkey_modifier,
                    &saved_settings.hotkey_key,
                )
                .unwrap_or_else(|| {
                    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyV)
                })
            };
            // Try to register the hotkey; if it fails (e.g. Super+V already taken by Windows),
            // retry a few times (system may release it late after boot), then fall back to
            // the default Ctrl+Shift+V.
            let register_fn = |shortcut: &Shortcut| -> Result<(), String> {
                app.global_shortcut()
                    .on_shortcut(shortcut.clone(), commands::window::toggle_on_press)
                    .map_err(|e| e.to_string())
            };
            // 首次注册立即尝试;失败(如开机时系统尚未释放 Win+V)不再同步
            // 睡眠重试——那会把 setup 卡住最多 6 秒,窗口创建被拖慢。
            // 策略:立即注册兜底键保证可用,原热键转后台线程重试接管
            if let Err(e) = register_fn(&startup_shortcut) {
                eprintln!(
                    "Failed to register hotkey {:?}: {}. Falling back to Ctrl+Shift+V.",
                    startup_shortcut, e
                );
                let fallback = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyV);
                register_fn(&fallback)?;
                // 不持久化任何改动:保留 win_v_integration 标志与用户自定义热键,
                // 下次启动继续尝试原热键(与 enable_win_v_integration 的兜底策略一致),
                // 此前这里直接把开关写回 false,会静默关掉用户已开启的 Win+V 接管
                if saved_settings.win_v_integration {
                    // 系统常在登录后延迟释放 Win+V:后台重试接管,成功则替换兜底键
                    let app_handle = app.handle().clone();
                    let desired = startup_shortcut;
                    std::thread::spawn(move || {
                        let mut taken = false;
                        for _ in 0..3 {
                            std::thread::sleep(std::time::Duration::from_millis(1500));
                            if app_handle
                                .global_shortcut()
                                .on_shortcut(desired, commands::window::toggle_on_press)
                                .is_ok()
                            {
                                taken = true;
                                break;
                            }
                        }
                        if !taken {
                            return;
                        }
                        // 仅当当前热键仍是兜底键时替换,避免与用户此间的手动改键打架;
                        // 用户已改键则注销刚注册的 Win+V,避免残留双热键
                        if let Some(last_state) =
                            app_handle.try_state::<commands::window::LastHotkey>()
                        {
                            let mut last =
                                last_state.0.lock().unwrap_or_else(|err| err.into_inner());
                            if last.as_ref() == Some(&fallback) {
                                let _ = app_handle.global_shortcut().unregister(fallback);
                                *last = Some(desired);
                            } else {
                                let _ = app_handle.global_shortcut().unregister(desired);
                            }
                        }
                    });
                }
                startup_shortcut = fallback;
            }
            app.manage(commands::window::LastHotkey(std::sync::Mutex::new(Some(
                startup_shortcut,
            ))));

            // 自启动任务:先清掉 1.x 的旧任务名(ClipboardManagerAutostart),
            // 再按当前设置重建/收敛新任务(SparrowAutostart)。改名后若不清旧任务,
            // 老用户开机时会被新旧两个任务各拉起一次。
            // 这一步同时取代了原先的"任务计划丢失时自愈"(设置开启即重建)。
            services::settings_service::migrate_autostart_task(app.handle());

            // 启动定时清理任务（每小时）
            let app_handle_cleanup = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
                loop {
                    interval.tick().await;
                    cleanup_expired_items(&app_handle_cleanup).await;
                }
            });

            // 主窗口在此程序化创建(tauri.conf.json 不再声明窗口),
            // 与热键唤出时的重建共用同一条路径(几何恢复/置顶恢复/
            // CloseRequested 拦截/WebView2 低内存模式都挂在创建函数里)。
            // --minimized(开机自启)时完全不创建窗口:登录瞬间桌面/WebView2/
            // 显示器枚举可能未就绪,过早创建易卡死或坐标错乱(表现为托盘/热键
            // 都唤不出)。首次唤出时桌面已就绪,再走统一的创建路径。
            let start_minimized = std::env::args().any(|arg| arg == "--minimized");
            if !start_minimized {
                utils::window_manager::create_main_window(app.handle(), true)?;
            }

            // 置顶守护:面板可见期间持续核对置顶样式,防外部软件剥除(见
            // window_manager::spawn_topmost_guard)。--minimized 下窗口未建也照常启动
            utils::window_manager::spawn_topmost_guard(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::clipboard::activate_item,
            commands::clipboard::delete_item,
            commands::clipboard::toggle_favorite,
            commands::clipboard::clear_history,
            commands::clipboard::pause_monitoring,
            commands::clipboard::get_image_base64,
            commands::clipboard::update_item_content,
            commands::clipboard::update_item_rich_content,
            commands::clipboard::get_rich_preview,
            commands::continuous_paste::start_continuous_paste,
            commands::continuous_paste::start_continuous_paste_auto,
            commands::continuous_paste::continuous_paste_next,
            commands::continuous_paste::stop_continuous_paste_auto,
            commands::continuous_paste::get_continuous_paste_status,
            commands::continuous_paste::cancel_continuous_paste,
            commands::settings::save_settings,
            commands::settings::load_settings,
            commands::settings::get_autostart_status,
            commands::settings::repair_autostart,
            commands::settings::allow_excluded_item,
            commands::settings::set_keep_open,
            commands::sync::get_sync_status,
            commands::sync::sync_now,
            commands::sync::set_sync_secret,
            commands::sync::has_sync_secrets,
            commands::sync::test_sync_connection,
            commands::sms_code::sms_code_status,
            commands::sms_code::sms_code_request_access,
            commands::sms_code::open_notification_settings,
            commands::window::show_window,
            commands::window::register_hotkey,
            commands::window::hide_window,
            commands::window::close_app,
            commands::window::get_close_behavior,
            commands::window::set_close_behavior,
            commands::window::enable_win_v_integration,
            commands::window::disable_win_v_integration,
            commands::window::set_always_on_top,
            commands::window::get_paste_mode,
            commands::window::set_paste_mode,
            commands::db_path::get_db_path,
            commands::groups::create_group,
            commands::groups::update_group,
            commands::groups::delete_group,
            commands::groups::set_item_group,
            services::storage_service::get_storage_info,
            services::storage_service::change_storage_location,
            services::storage_service::reset_storage_location,
            commands::update::check_update,
            commands::update::open_external_url,
            commands::update::write_clipboard_text,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    // 窗口已改为“隐藏即销毁、唤出即重建”:销毁最后一个窗口不能退出应用,
    // 托盘/热键/剪贴板监听常驻后台,窗口按需重建。
    // code = Some(_) 是显式 exit(托盘退出、关闭行为=直接退出),必须放行。
    app.run(|_app_handle, event| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            if code.is_none() {
                api.prevent_exit();
            }
        }
    });
}

async fn cleanup_expired_items(app_handle: &tauri::AppHandle) {
    use tauri::Manager;
    let settings = services::settings_service::load_settings(app_handle);

    // Get the sqlx pool from state
    let pool = match app_handle.try_state::<sqlx::SqlitePool>() {
        Some(pool) => pool.inner().clone(),
        None => return,
    };

    // Collect the conditions first, then delete rows and their image files.
    // 条件与 DELETE 一致,先查出待删行的图片路径,删除行后再清理文件
    let mut conditions: Vec<(String, i32)> = Vec::new();
    if settings.retention_days > 0 {
        conditions.push((
            "is_favorite = 0 AND last_used_at < datetime('now', '-' || ? || ' days')"
                .to_string(),
            settings.retention_days,
        ));
    }
    if settings.max_item_count > 0 {
        conditions.push((
            "id NOT IN (SELECT id FROM items ORDER BY last_used_at DESC LIMIT ?) AND is_favorite = 0"
                .to_string(),
            settings.max_item_count,
        ));
    }

    let mut deleted: u64 = 0;
    for (where_clause, value) in conditions {
        let file_rows: Vec<(Option<String>, Option<String>, Option<String>)> =
            match sqlx::query_as(&format!(
                "SELECT file_path, html_path, rtf_path FROM items WHERE {}",
                where_clause
            ))
            .bind(value)
            .fetch_all(&pool)
            .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("Cleanup error (select): {}", e);
                    continue;
                }
            };

        match sqlx::query(&format!("DELETE FROM items WHERE {}", where_clause))
            .bind(value)
            .execute(&pool)
            .await
        {
            Ok(res) => deleted += res.rows_affected(),
            Err(e) => {
                eprintln!("Cleanup error (delete): {}", e);
                continue;
            }
        }

        for (file_path, html_path, rtf_path) in file_rows.iter() {
            if let Some(path) = file_path {
                services::image_cleanup::remove_image_file(app_handle, path);
            }
            for path in [html_path, rtf_path].into_iter().flatten() {
                services::image_cleanup::remove_format_file(app_handle, path);
            }
        }
    }

    // 有实际删除才回收数据库文件空间(与手动清空行为一致);
    // 并发连接占用导致 VACUUM 失败时只记日志,不影响清理结果
    if deleted > 0 {
        if let Err(e) = sqlx::query("VACUUM").execute(&pool).await {
            eprintln!("VACUUM after cleanup failed: {}", e);
        }
    }
}

fn get_migrations() -> Vec<tauri_plugin_sql::Migration> {
    vec![
        tauri_plugin_sql::Migration {
            version: 1,
            description: "001_create_tables",
            sql: include_str!("../migrations/001_create_tables.sql"),
            kind: tauri_plugin_sql::MigrationKind::Up,
        },
        tauri_plugin_sql::Migration {
            version: 2,
            description: "002_add_indexes",
            sql: include_str!("../migrations/002_add_indexes.sql"),
            kind: tauri_plugin_sql::MigrationKind::Up,
        },
        // 注意:003_add_rich_formats **不能**进这份共用列表。
        // 这份列表同时被两处消费:(a) 插件注册(前端 Database.load 按版本表
        // 执行,不会重复);(b) lib.rs setup 里的手动循环——**每次启动无条件
        // 重放全部迁移并 .expect()**。001/002 全是 IF NOT EXISTS 所以安全;
        // 003 是 ALTER TABLE ADD COLUMN,非幂等,重放即 duplicate column
        // panic。因此 003 走带列探测守卫的 apply_rich_format_columns(),
        // 在手动循环之后单独执行。未来新增迁移:要么写成幂等 SQL 进这份
        // 列表,要么同样走守卫式单独执行。
    ]
}

/// 003 的应用逻辑:PRAGMA table_info 探测 items 现有列,
/// html_path/rtf_path 按缺失情况各自 ALTER,已存在则跳过。
/// 幂等,可安全重放(守卫存在的意义就是让"每次启动都执行"安全)。
async fn apply_rich_format_columns(pool: &sqlx::sqlite::SqlitePool) -> Result<(), String> {
    // table_info 各行的 name 列(索引 1)是列名;cid 等其余列忽略
    let columns: Vec<(i64, String)> = sqlx::query_as("PRAGMA table_info(items)")
        .fetch_all(pool)
        .await
        .map_err(|e| format!("读取 items 表结构失败: {}", e))?;
    let existing: std::collections::HashSet<&str> = columns
        .iter()
        .map(|(_, name)| name.as_str())
        .collect();

    for (column, ddl) in [
        ("html_path", "ALTER TABLE items ADD COLUMN html_path TEXT"),
        ("rtf_path", "ALTER TABLE items ADD COLUMN rtf_path TEXT"),
    ] {
        if !existing.contains(column) {
            sqlx::query(ddl)
                .execute(pool)
                .await
                .map_err(|e| format!("迁移 003 添加 {} 失败: {}", column, e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟"连续两次启动":003 应用逻辑重放两次,
    /// 第二次必须不报错,两次后两列存在且已有数据无损。
    #[tokio::test]
    async fn rich_format_migration_is_idempotent_across_restarts() {
        let dir = std::env::temp_dir().join(format!("dsh-clip-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db_file = dir.join("idempotent.db");

        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_file)
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(options)
            .await
            .expect("连接测试库");

        // 先建基础表结构(001),模拟已有旧版本库
        sqlx::query(include_str!("../migrations/001_create_tables.sql"))
            .execute(&pool)
            .await
            .expect("建基础表");

        // 第一次应用 003
        apply_rich_format_columns(&pool).await.expect("首次应用 003");

        // 插入一条数据,验证重放不丢
        sqlx::query("INSERT INTO items (type, content, content_hash) VALUES (0, '数据', 'h1')")
            .execute(&pool)
            .await
            .expect("插入数据");

        // 第二次应用(模拟重启后的重放)——不报错是本测试的核心断言
        apply_rich_format_columns(&pool).await.expect("重放 003 必须幂等成功");

        // 两列存在且可写
        sqlx::query("INSERT INTO items (type, content, content_hash, html_path, rtf_path) VALUES (0, '富', 'h2', 'x.html', 'y.rtf')")
            .execute(&pool)
            .await
            .expect("新列可写");

        // 旧数据无损
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM items")
            .fetch_one(&pool)
            .await
            .expect("计数");
        assert_eq!(count, 2, "重放不得丢数据");

        // 第三次重放再确认(列已全部存在的完全 no-op 路径)
        apply_rich_format_columns(&pool).await.expect("第三次重放");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全新安装路径:items 表刚由 001 建好,003 正常加两列
    #[tokio::test]
    async fn rich_format_migration_on_fresh_database() {
        let dir = std::env::temp_dir().join(format!("dsh-clip-fresh-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db_file = dir.join("fresh.db");

        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_file)
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(options)
            .await
            .expect("连接测试库");

        sqlx::query(include_str!("../migrations/001_create_tables.sql"))
            .execute(&pool)
            .await
            .expect("建基础表");

        apply_rich_format_columns(&pool).await.expect("全新库应用 003");

        let columns: Vec<(i64, String)> = sqlx::query_as("PRAGMA table_info(items)")
            .fetch_all(&pool)
            .await
            .expect("读表结构");
        let names: Vec<String> = columns.into_iter().map(|(_, n)| n).collect();
        assert!(names.contains(&"html_path".to_string()), "html_path 必须存在: {:?}", names);
        assert!(names.contains(&"rtf_path".to_string()), "rtf_path 必须存在: {:?}", names);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
