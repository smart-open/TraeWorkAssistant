// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod checkin_results;
mod fs_utils;
mod jwt;
mod models;
mod notify;
mod python;
mod state;
mod vault;
mod api_server;

use state::AppState;
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

/// 托盘菜单句柄：API 启停项文本随服务状态动态切换（do_start/do_stop 内同步）
pub struct TrayMenu {
    pub api_item: MenuItem<tauri::Wry>,
}

fn main() {
    // 品牌迁移（老版本 Trae Work Assistant → AI Work 助手）：
    // 必须在 AppState::new 创建新数据目录之前执行；迁移为「复制」语义，
    // 旧数据目录原地保留（老应用可继续使用，两版并存），已迁移过则自动跳过
    let migrate_note = state::migrate_legacy_dirs();

    let state = match AppState::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("初始化失败: {e}");
            std::process::exit(1);
        }
    };

    if let Some(note) = &migrate_note {
        fs_utils::app_log(&state.data_dir, note);
        eprintln!("{note}");
    }

    // 旧版计划任务迁移（并存语义）：按旧任务触发时间重建 AIWorkAssistant_DailyCheckin，
    // 旧任务保留供老应用继续使用；任何一步失败都静默跳过
    if let Some(note) = commands::misc::try_migrate_legacy_task(&state) {
        fs_utils::app_log(&state.data_dir, &note);
    }

    // 单实例防护（仅正式版）：第二个进程启动时，本回调在首个实例中执行——把主窗口
    // 还原/显示/聚焦后，第二进程由插件自动退出。必须第一个注册（在创建窗口前持有互斥锁）。
    // dev 模式不启用：dev 与已安装版共用 identifier，启用会导致 `npm run tauri dev`
    // 与已安装应用互相顶替退出，干扰开发调试。
    #[cfg_attr(debug_assertions, allow(unused_mut))] // dev 不注册单实例插件
    let mut builder = tauri::Builder::default();
    #[cfg(not(debug_assertions))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
            if let Some(state) = app.try_state::<AppState>() {
                fs_utils::app_log(&state.data_dir, "检测到重复启动：已聚焦已有实例窗口");
            }
        }));
    }

    let app = builder
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(state)
        .manage(Mutex::new(Option::<commands::proxy::ProxyHandle>::None))
        .manage(Mutex::new(Option::<commands::api_server::ApiServerRuntime>::None))
        .manage(commands::checkin::CheckinGuard(tokio::sync::Mutex::new(())))
        .invoke_handler(tauri::generate_handler![
            commands::env::env_check,
            commands::env::app_locate,
            commands::env::open_doubao_app,
            commands::env::open_trae_website,
            commands::env::open_trae_app,
            commands::env::env_check_trae_cn,
            commands::env::open_trae_cn_app,
            commands::cert::cert_status,
            commands::cert::cert_install,
            commands::proxy::proxy_start,
            commands::proxy::proxy_stop,
            commands::proxy::proxy_status,
            commands::accounts::accounts_list,
            commands::accounts::accounts_export_raw,
            commands::accounts::accounts_import,
            commands::accounts::accounts_import_preview,
            commands::accounts::account_add_manual,
            commands::accounts::account_delete,
            commands::accounts::account_update,
            commands::accounts::groups_list,
            commands::accounts::group_create,
            commands::accounts::group_update,
            commands::accounts::group_delete,
            commands::accounts::group_move,
            commands::accounts::fetch_remaining_credits,
            commands::accounts::fetch_credit_detail,
            commands::accounts::refresh_remaining_credits,
            commands::accounts::credits_daily_list,
            commands::accounts::cooldown_clear,
            commands::accounts::cooldown_clear_all,
            commands::accounts::refresh_jwt,
            commands::checkin::checkin_start,
            commands::checkin::checkin_trends,
            commands::switch::switch_account,
            commands::switch::save_current_login,
            commands::switch::reset_device_ids,
            commands::misc::device_reset,
            commands::misc::autostart_status,
            commands::misc::autostart_set,
            commands::misc::jwt_parse,
            commands::misc::logs_query,
            commands::misc::logs_clear,
            commands::misc::settings_get,
            commands::misc::settings_set,
            commands::misc::invite_link,
            commands::misc::credits_history,
            commands::misc::task_register,
            commands::misc::task_status,
            commands::misc::task_unregister,
            commands::misc::proxy_logs_list,
            commands::misc::proxy_log_detail,
            commands::misc::write_text_file,
            commands::misc::read_text_file,
            commands::api_server::api_server_start,
            commands::api_server::api_server_stop,
            commands::api_server::api_server_status,
            commands::api_server::pool_list,
            commands::api_server::pool_set,
            commands::api_server::pool_status,
            commands::api_server::api_logs_list,
            commands::api_server::api_logs_detail,
            commands::api_server::api_logs_search,
            commands::api_server::api_debug_toggle,
            commands::api_server::api_debug_status,
            commands::api_server::api_models_list,
            commands::api_server::api_models_sync,
            commands::api_server::api_usage_stats,
            commands::api_server::api_keys_list,
            commands::api_server::api_keys_save,
            commands::profile::profile_list,
            commands::profile::profile_backup,
            commands::profile::profile_restore,
            commands::profile::profile_delete,
            commands::profile::profile_format_size,
            commands::doubao::doubao_accounts_list,
            commands::doubao::doubao_account_save,
            commands::doubao::doubao_account_remove,
            commands::doubao::doubao_detect_uid,
            commands::doubao::doubao_quota_fetch,
            commands::doubao::doubao_captured_credential,
            commands::doubao::doubao_credential_auto_apply,
            commands::doubao::doubao_renew_run,
            commands::doubao::doubao_keepalive_run,
            commands::doubao::doubao_account_set_credential,
            commands::doubao::doubao_renew_task_register,
            commands::doubao::doubao_renew_task_status,
            commands::doubao::doubao_renew_task_unregister,
            commands::doubao::doubao_history,
            commands::doubao::doubao_quota_task_register,
            commands::doubao::doubao_quota_task_status,
            commands::doubao::doubao_quota_task_unregister,
            commands::doubao::doubao_open_as_account,
            commands::doubao::doubao_snapshot_meta,
            commands::doubao::doubao_chatdata_backup,
            commands::doubao::doubao_chatdata_restore,
            commands::doubao::doubao_chatdata_info,
            commands::doubao::doubao_export_chats,
            commands::oauth::oauth_get_login_url,
            commands::oauth::oauth_parse_callback,
            commands::oauth::oauth_login,
            commands::trae_apps::apps_accounts_discover,
            commands::trae_apps::apps_account_add,
            commands::trae_apps::apps_entitlement_read,
            commands::trae_apps::refresh_pay_status,
            commands::trae_apps::accounts_backfill_dc_ids,
            commands::updater::update_check,
            commands::updater::update_download,
            commands::updater::update_run_installer,
        ])
        .setup(|app| {
            let state = app.state::<AppState>();
            let settings = state.settings();

            // 启动期日志清理：按 log_retention_days 丢弃过期日志行（消费设置项，避免无限增长）
            let retention = settings.log_retention_days.max(0) as u64;
            fs_utils::trim_logs(&state.data_dir, retention);

            // 清理上次运行残留的临时凭据文件（崩溃时未及删除的明文文件，失败不阻断启动）
            vault::cleanup_temp_accounts(&state);

            // 敏感数据迁移：checkin_accounts.json 明文 jwt/refresh_token → Stronghold vault（幂等，失败不阻断启动）
            vault::migrate_on_startup(&state);

            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "应用启动: tray=enabled, launch_minimized={}, auto_start_proxy={}",
                    settings.launch_minimized, settings.auto_start_proxy
                ),
            );

            // 创建系统托盘（始终启用，支持最小化到托盘；失败不阻断启动）
            {
                let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                    let toggle_item =
                        MenuItem::with_id(app, "toggle", "显示/隐藏", true, None::<&str>)?;
                    let checkin_item =
                        MenuItem::with_id(app, "checkin", "立即签到", true, None::<&str>)?;
                    let api_item =
                        MenuItem::with_id(app, "api-toggle", "启动 API 服务", true, None::<&str>)?;
                    let sep = PredefinedMenuItem::separator(app)?;
                    let quit_item =
                        MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
                    let menu = Menu::with_items(
                        app,
                        &[&toggle_item, &sep, &checkin_item, &api_item, &sep, &quit_item],
                    )?;
                    app.manage(TrayMenu { api_item });

                    let icon = app
                        .default_window_icon()
                        .cloned()
                        .ok_or("找不到默认窗口图标")?;

                    TrayIconBuilder::new()
                        .icon(icon)
                        .tooltip("AI Work 助手")
                        .menu(&menu)
                        .on_tray_icon_event(|tray, event| {
                            if let TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            } = event
                            {
                                let app = tray.app_handle();
                                let st = app.state::<AppState>();
                                if let Some(window) = app.get_webview_window("main") {
                                    if window.is_visible().unwrap_or(false) {
                                        let _ = window.hide();
                                        fs_utils::app_log(&st.data_dir, "托盘左键点击：隐藏窗口");
                                    } else {
                                        let _ = window.show();
                                        let _ = window.set_focus();
                                        fs_utils::app_log(&st.data_dir, "托盘左键点击：显示窗口");
                                    }
                                } else {
                                    fs_utils::app_log(&st.data_dir, "托盘左键点击：找不到主窗口");
                                }
                            }
                        })
                        .on_menu_event(|app, event| match event.id.as_ref() {
                            "toggle" => {
                                let st = app.state::<AppState>();
                                if let Some(window) = app.get_webview_window("main") {
                                    if window.is_visible().unwrap_or(false) {
                                        let _ = window.hide();
                                        fs_utils::app_log(&st.data_dir, "菜单：隐藏窗口");
                                    } else {
                                        let _ = window.show();
                                        let _ = window.set_focus();
                                        fs_utils::app_log(&st.data_dir, "菜单：显示窗口");
                                    }
                                }
                            }
                            "checkin" => {
                                // 托盘一键签到：后台线程执行，防重入锁内完成；
                                // 跳过已签/冷却账号，完成后发系统通知
                                let app2 = app.clone();
                                std::thread::spawn(move || {
                                    let st = app2.state::<AppState>();
                                    let opts = commands::checkin::CheckinOpts {
                                        scope: "all".into(),
                                        user_ids: None,
                                        skip_checked_in: true,
                                        skip_expired: false,
                                    };
                                    if let Err(e) = commands::checkin::start_checkin_core(
                                        &app2, &st, opts, true,
                                    ) {
                                        fs_utils::app_log(
                                            &st.data_dir,
                                            &format!("托盘签到失败: {e}"),
                                        );
                                        notify::notify(&app2, "签到启动失败", &e);
                                    }
                                });
                            }
                            "api-toggle" => {
                                // 托盘启停 API 服务：block_on 等待快速启停完成，
                                // 菜单文本与通知由 do_start/do_stop 内统一处理
                                let st = app.state::<AppState>();
                                let runtime = app
                                    .state::<Mutex<Option<commands::api_server::ApiServerRuntime>>>();
                                let running = runtime
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .is_some();
                                let result = if running {
                                    tauri::async_runtime::block_on(
                                        commands::api_server::do_stop(app, &st, &runtime),
                                    )
                                    .map(|_| ())
                                } else {
                                    tauri::async_runtime::block_on(
                                        commands::api_server::do_start(app, &st, &runtime),
                                    )
                                    .map(|_| ())
                                };
                                if let Err(e) = result {
                                    fs_utils::app_log(
                                        &st.data_dir,
                                        &format!("托盘 API 服务操作失败: {e}"),
                                    );
                                    notify::notify(app, "API 服务操作失败", &e);
                                }
                            }
                            "quit" => {
                                let st = app.state::<AppState>();
                                fs_utils::app_log(&st.data_dir, "菜单：用户请求退出应用");
                                // 与窗口关闭一致：先隐藏主窗口再退出，避免 WebView2 销毁期间窗口冻结
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.hide();
                                }
                                app.exit(0);
                            }
                            _ => {}
                        })
                        .build(app)?;
                    Ok(())
                })();

                match &result {
                    Ok(()) => fs_utils::app_log(&state.data_dir, "托盘图标创建成功"),
                    Err(e) => {
                        let msg = format!("托盘创建失败（应用将继续运行）: {e}");
                        fs_utils::app_log(&state.data_dir, &msg);
                        eprintln!("{msg}");
                    }
                }
            }

            // 启动时最小化到托盘
            if settings.launch_minimized {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                    fs_utils::app_log(&state.data_dir, "启动最小化：窗口已隐藏到托盘");
                }
            }

            // 启动静默签到（T11）：延迟 60s 后对未签到账号自动执行一轮签到，
            // 复用托盘签到链路（含防重入锁，与手动/托盘签到天然互斥）；
            // skip_checked_in=true 保证幂等，重复开机不会重复签
            if settings.silent_checkin {
                let app2 = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    let st = app2.state::<AppState>();
                    fs_utils::app_log(&st.data_dir, "静默签到：启动延迟到期，开始检查签到");
                    let opts = commands::checkin::CheckinOpts {
                        scope: "all".into(),
                        user_ids: None,
                        skip_checked_in: true,
                        skip_expired: false,
                    };
                    if let Err(e) = commands::checkin::start_checkin_core(&app2, &st, opts, true) {
                        fs_utils::app_log(&st.data_dir, &format!("静默签到失败: {e}"));
                        notify::notify(&app2, "静默签到失败", &e);
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭即退出应用（前端已弹确认框；退出时 RunEvent::Exit 自动清理代理与 API 服务）
            // 先隐藏窗口再退出：exit(0) 内部的 WebView2 销毁可能耗时数秒（运行越久越明显），
            // 若直接退出，用户会看到窗口冻结「卡死」。隐藏后清理再慢也无感（issue：关闭卡死）。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() != "main" {
                    return;
                }
                api.prevent_close();
                let st = window.app_handle().state::<AppState>();
                fs_utils::app_log(&st.data_dir, "用户确认退出应用");
                let _ = window.hide();
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            // 应用退出时清理代理子进程，防止端口占用
            let proxy_state = app_handle.state::<Mutex<Option<commands::proxy::ProxyHandle>>>();
            let mut g = proxy_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = g.take() {
                let state = app_handle.state::<AppState>();
                fs_utils::app_log(&state.data_dir, "应用退出：正在清理代理子进程");
                drop(h); // Drop trait 会 kill + wait 子进程
            }
            // 应用退出时停止 API 服务
            let api_state = app_handle
                .state::<Mutex<Option<commands::api_server::ApiServerRuntime>>>();
            let mut ag = api_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut rt) = ag.take() {
                let state = app_handle.state::<AppState>();
                fs_utils::app_log(&state.data_dir, "应用退出：正在停止 API 服务");
                rt.handle.stop();
            }
            // 还原系统代理，避免退出后本机全局断网
            if let Err(e) = commands::proxy::clear_win_proxy() {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    fs_utils::app_log(&state.data_dir, &format!("应用退出：还原系统代理失败(可手动关闭): {e}"));
                }
            }
        }
    });
}
