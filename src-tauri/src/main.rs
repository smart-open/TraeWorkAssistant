// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod checkin_results;
mod device_proxy;
mod fs_utils;
mod icube_auth;
mod jwt;
mod models;
mod notify;
mod platform;
mod state;
mod store;
mod switcher;
mod tasks;
mod vault;
mod api_server;
mod workbuddy_cli;

use state::AppState;
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

/// 托盘菜单句柄：API 网关与代理启停项文本随服务状态动态切换
///（API 由 api_server::sync_tray_api_text 同步；代理由 proxy::sync_tray_proxy_text 同步）
pub struct TrayMenu {
    pub api_item: MenuItem<tauri::Wry>,
    pub proxy_item: MenuItem<tauri::Wry>,
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

    // CLI 任务模式（D2）：schtasks 计划任务直调主 exe（`--task-run <name>`），
    // 执行完任务即退出；分支在 Builder 之前，天然绕开单实例插件，不启动 GUI。
    // Python 运行时移除后计划任务链依赖此入口（原 python 脚本直调的替代）。
    if let Some(task) = tasks::parse_task_mode(&std::env::args().collect::<Vec<_>>()) {
        std::process::exit(tasks::run_cli_task(&task, &state));
    }

    if let Some(note) = &migrate_note {
        fs_utils::app_log(&state.data_dir, note);
        eprintln!("{note}");
    }

    // 旧版计划任务迁移（并存语义）：按旧任务触发时间重建 AIWorkAssistant_DailyCheckin，
    // 旧任务保留供老应用继续使用；任何一步失败都静默跳过。
    // 仅 Windows 有 schtasks 体系（cfg! 运行时判断保持函数可达，mac 无 dead_code 警告，
    // 也不会空跑注定失败的 cmd 子进程）
    if cfg!(windows) {
        if let Some(note) = commands::misc::try_migrate_legacy_task(&state) {
            fs_utils::app_log(&state.data_dir, &note);
        }
    }
    // PS 桥 KeepAlive 启动器一次性迁移：旧 task_doubao_renew.cmd 引用
    // trae-switch-bridge.ps1 → 原地改写为 --task-run doubao-keepalive（幂等，失败静默）
    if let Some(note) = commands::doubao::try_migrate_keepalive_launcher(&state) {
        fs_utils::app_log(&state.data_dir, &note);
    }
    // OAuth 代理直连豁免崩溃残留清理（F-78 批次 2/缺陷13）：上次进程异常退出
    // 未还原 ProxyOverride 时，按标记文件只移除本软件追加的条目（无标记幂等空操作）
    device_proxy::bypass::cleanup_residual_bypass(&state.data_dir);

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
            commands::env::platform_info,
            commands::env::env_check,
            commands::env::app_locate,
            commands::env::open_doubao_app,
            commands::env::open_trae_website,
            commands::env::open_trae_app,
            commands::env::env_check_trae_cn,
            commands::env::open_trae_cn_app,
            commands::env::open_workbuddy_app,
            commands::env::open_codebuddy_app,
            commands::env::codebuddy_env_check,
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
            commands::accounts::account_get_jwt,
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
            commands::api_server::wb_pool_status,
            commands::api_server::api_logs_list,
            commands::api_server::api_logs_detail,
            commands::api_server::api_logs_search,
            commands::api_server::api_debug_toggle,
            commands::api_server::api_debug_status,
            commands::api_server::api_models_list,
            commands::api_server::api_models_sync,
            commands::api_server::api_usage_stats,
            commands::api_server::api_wb_usage_stats,
            commands::api_server::api_custom_usage_stats,
            commands::api_server::api_keys_list,
            commands::api_server::api_keys_save,
            commands::api_server::api_wb_catalog_sync,
            commands::api_server::api_wb_catalog_list,
            commands::wb_config::wb_route_config_get,
            commands::wb_config::wb_route_config_set,
            commands::wb_config::wb_template_map_get,
            commands::wb_config::wb_template_map_set,
            commands::api_server::api_unified_models,
            commands::api_server::dispatch_policy_get,
            commands::api_server::dispatch_policy_set,
            commands::api_server::gateway_settings_get,
            commands::api_server::gateway_settings_set,
            commands::api_server::custom_models_list,
            commands::api_server::custom_models_save,
            commands::api_server::custom_models_remove,
            commands::api_server::custom_model_test,
            commands::api_server::trae_model_meta_set,
            commands::api_server::trae_model_meta_get,
            commands::api_server::trae_model_meta_clear,
            commands::ccswitch::ccswitch_status,
            commands::ccswitch::ccswitch_register,
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
            commands::doubao::doubao_account_get_credential,
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
            commands::oauth_loopback::oauth_start_loopback,
            commands::oauth_loopback::oauth_stop_loopback,
            commands::trae_apps::apps_accounts_discover,
            commands::trae_apps::apps_account_add,
            commands::trae_apps::apps_entitlement_read,
            commands::trae_apps::refresh_pay_status,
            commands::trae_apps::accounts_backfill_dc_ids,
            commands::usage_history::usage_history_fetch,
            commands::updater::update_check,
            commands::updater::update_download,
            commands::updater::update_run_installer,
            commands::updater::update_restart_app,
            commands::workbuddy::workbuddy_env_check,
            commands::workbuddy::workbuddy_open_auth_dir,
            commands::workbuddy::workbuddy_accounts_list,
            commands::workbuddy::workbuddy_account_save,
            commands::workbuddy::workbuddy_account_move,
            commands::workbuddy::workbuddy_groups_list,
            commands::workbuddy::workbuddy_groups_create,
            commands::workbuddy::workbuddy_groups_update,
            commands::workbuddy::workbuddy_groups_remove,
            commands::workbuddy::workbuddy_account_remove,
            commands::workbuddy::workbuddy_scan_auth_file,
            commands::workbuddy::workbuddy_account_import_auth,
            commands::workbuddy::workbuddy_refresh_token,
            commands::workbuddy::workbuddy_checkin_start,
            commands::workbuddy::workbuddy_growth_run,
            commands::workbuddy::workbuddy_checkin_results,
            commands::workbuddy::workbuddy_checkin_task_register,
            commands::workbuddy::workbuddy_checkin_task_status,
            commands::workbuddy::workbuddy_checkin_task_unregister,
            commands::workbuddy::workbuddy_renew_task_register,
            commands::workbuddy::workbuddy_renew_task_status,
            commands::workbuddy::workbuddy_renew_task_unregister,
            commands::workbuddy::workbuddy_credits_fetch,
            commands::workbuddy::workbuddy_editions_backfill,
            commands::workbuddy::workbuddy_settings_get,
            commands::workbuddy::workbuddy_ui_click_capture,
            commands::workbuddy::workbuddy_ui_click_checkin,
            commands::workbuddy::workbuddy_settings_set,
            commands::workbuddy::workbuddy_cli_status,
            commands::workbuddy::workbuddy_cli_bridge_set,
            commands::workbuddy::workbuddy_cli_rotate_run,
            commands::workbuddy::workbuddy_cli_rotate_logs,
            commands::workbuddy::workbuddy_chatdata_backup,
            commands::workbuddy::workbuddy_chatdata_restore,
            commands::workbuddy::workbuddy_chatdata_info,
            commands::workbuddy::workbuddy_chatdata_copy,
            commands::workbuddy::workbuddy_accounts_export,
            commands::workbuddy::workbuddy_accounts_import,
            commands::workbuddy::workbuddy_oauth_login,
            commands::workbuddy::workbuddy_env_reset_items,
            commands::workbuddy::workbuddy_env_reset,
            commands::workbuddy::workbuddy_usage_official,
            commands::workbuddy::workbuddy_usage_fallback,
            commands::workbuddy::workbuddy_usage_official_all,
            commands::workbuddy::workbuddy_activity_info,
            commands::workbuddy_stats::workbuddy_token_stats,
            tasks::scheduler::scheduler_status,
        ])
        .setup(|app| {
            // F-75：macOS 无边框窗口（decorations:false，前端自绘标题栏）下 tauri.conf
            // 的 "maximized": true 不生效（Tauri v2 已知缺陷：borderless 样式忽略
            // maximize），窗口退回 1180×760 原始尺寸显得过大。此处按所在屏幕手动
            // 铺满可视区（下移菜单栏高度，避免自绘标题栏被系统菜单栏遮挡）；
            // Windows 走 conf 原生最大化，不受本段影响（行为零变化）。
            #[cfg(target_os = "macos")]
            {
                if let Some(win) = app.get_webview_window("main") {
                    let monitor = win
                        .current_monitor()
                        .ok()
                        .flatten()
                        .or_else(|| win.primary_monitor().ok().flatten());
                    if let Some(monitor) = monitor {
                        let mpos = monitor.position();
                        let msize = monitor.size();
                        let scale = monitor.scale_factor();
                        // 菜单栏高度经验值 ~25 逻辑 px（Retina/普通屏通用近似）
                        let menu_h = (25.0 * scale) as i32;
                        let _ = win.hide();
                        let _ = win.set_position(tauri::PhysicalPosition::new(
                            mpos.x,
                            mpos.y + menu_h,
                        ));
                        let _ = win.set_size(tauri::PhysicalSize::new(
                            msize.width,
                            msize.height - menu_h as u32,
                        ));
                        let _ = win.show();
                    }
                }
            }

            let state = app.state::<AppState>();

            // 数据存储层 SQLite 化（docs/sqllite-storage-plan.md）：旧 JSON 导入 aiwork.sqlite
            // 并移入 data/backup/（幂等；失败不阻断启动，下次启动重试）。
            // 必须先于本回调内一切 kv 消费方执行（settings/trim_logs 等）——否则升级
            // 首次启动读到全默认设置，trim_logs 会按默认保留期误裁日志。
            // 注意：必须先于 vault 迁移执行——JSON 中的明文凭据先入库，
            // 再由随后的 vault::migrate_on_startup 收敛进 Stronghold 并从库中占位化抹除。
            if let Some(summary) = store::migrate::migrate_on_startup(&state.data_dir) {
                fs_utils::app_log(&state.data_dir, &summary);
            }

            let settings = state.settings();

            // 启动期日志清理：按 log_retention_days 丢弃过期日志行（消费设置项，避免无限增长）
            let retention = settings.log_retention_days.max(0) as u64;
            fs_utils::trim_logs(&state.data_dir, retention);

            // 清理上次运行残留的临时凭据文件（崩溃时未及删除的明文文件，失败不阻断启动）
            vault::cleanup_temp_accounts(&state);

            // 敏感数据迁移：库中明文 jwt/refresh_token → Stronghold vault（幂等，失败不阻断启动）
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
                        MenuItem::with_id(app, "checkin", "一键签到", true, None::<&str>)?;
                    let api_item =
                        MenuItem::with_id(app, "api-toggle", "启动API网关", true, None::<&str>)?;
                    let proxy_item =
                        MenuItem::with_id(app, "proxy-toggle", "启动代理", true, None::<&str>)?;
                    let sep = PredefinedMenuItem::separator(app)?;
                    let quit_item =
                        MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
                    let menu = Menu::with_items(
                        app,
                        &[&toggle_item, &sep, &checkin_item, &api_item, &proxy_item, &sep, &quit_item],
                    )?;
                    app.manage(TrayMenu { api_item, proxy_item });

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
                                // 托盘一键签到：Trae（start_checkin_core 内部防重入锁）
                                // + WorkBuddy 签到 + 成长计划（同步串行线程，各阶段完成发系统通知）
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
                                            &format!("托盘 Trae 签到失败: {e}"),
                                        );
                                        notify::notify(&app2, "Trae 签到启动失败", &e);
                                    }
                                    commands::workbuddy::tray_checkin_all(&app2, &st);
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
                            "proxy-toggle" => {
                                // 托盘启停代理：与前端共用 proxy_start/proxy_stop 命令
                                //（菜单文本由 proxy.rs 内 sync_tray_proxy_text 统一同步，覆盖托盘/前端/自动启动三条路径）
                                let running = app
                                    .state::<Mutex<Option<commands::proxy::ProxyHandle>>>()
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .is_some();
                                let result = if running {
                                    let st = app.state::<AppState>();
                                    let ps =
                                        app.state::<Mutex<Option<commands::proxy::ProxyHandle>>>();
                                    commands::proxy::proxy_stop(app.clone(), st, ps).map(|_| ())
                                } else {
                                    let port = app.state::<AppState>().settings().proxy_port;
                                    let st = app.state::<AppState>();
                                    let ps =
                                        app.state::<Mutex<Option<commands::proxy::ProxyHandle>>>();
                                    // P4 Rust 化：proxy_start 已是异步命令（进程内代理启动含异步绑定），
                                    // 托盘回调运行在主线程，用 block_on 等待（与 API 服务托盘同款模式）
                                    tauri::async_runtime::block_on(commands::proxy::proxy_start(
                                        app.clone(), st, ps, port,
                                    ))
                                    .map(|_| ())
                                };
                                if let Err(e) = result {
                                    let st = app.state::<AppState>();
                                    fs_utils::app_log(
                                        &st.data_dir,
                                        &format!("托盘代理操作失败: {e}"),
                                    );
                                    notify::notify(app, "代理操作失败", &e);
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

            // WorkBuddy 启动自动补签（F-55）：延迟 60s 核验未签账号并补签，
            // 复用 python 签到脚本（--skip-checked 幂等），独立于 Trae 静默签到
            {
                let app2 = app.handle().clone();
                let st = app.state::<AppState>();
                commands::workbuddy::startup_auto_checkin(&app2, &st);
            }

            // CodeBuddy CLI 五重防护自动轮换（F-59）：独立后台线程按检查间隔执行，
            // 开关关闭时空转；decide_target 纯函数判定，切号写 ~/.codebuddy/settings.json
            commands::workbuddy::start_cli_rotate_thread();

            // 应用内定时调度器（Rust 原生方案，补充 Windows schtasks）：
            // 每日签到/巡检/续期到点补跑 + 积分余额每日快照（新增任务，补齐近 7 日消耗时序）
            tasks::scheduler::start(app.handle().clone());

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
            // 应用退出时停止进程内代理（P4 Rust 化），防止端口占用
            commands::proxy::mark_intentional_stop();
            let proxy_state = app_handle.state::<Mutex<Option<commands::proxy::ProxyHandle>>>();
            let mut g = proxy_state.lock().unwrap_or_else(|e| e.into_inner());
            let mut proxy_was_running = false;
            if let Some(h) = g.take() {
                proxy_was_running = true;
                let state = app_handle.state::<AppState>();
                fs_utils::app_log(&state.data_dir, "应用退出：正在停止进程内代理");
                h.server.stop(); // 发送 shutdown 信号；句柄 drop 亦触发退出
            }
            // 应用退出时停止 API 服务
            let api_state = app_handle
                .state::<Mutex<Option<commands::api_server::ApiServerRuntime>>>();
            let mut ag = api_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut rt) = ag.take() {
                let state = app_handle.state::<AppState>();
                fs_utils::app_log(&state.data_dir, "应用退出：正在停止 API 服务");
                rt.handle.stop();
                // 批次 C/E：退出前排空用量脏队列、api_keys 计数与日志队列
                rt.shared.flush_pending_writes();
            }
            // 还原系统代理（#14）：仅当「我们曾接管系统代理」时才还原——
            // 有用户 VPN 原值则原样还原（原 clear_win_proxy 会把用户梯子一并清掉）；
            // 代理从未启动/已正常停止则不触碰，避免误关用户自己的 VPN
            if let Err(e) = commands::proxy::restore_system_proxy_on_exit(proxy_was_running) {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    fs_utils::app_log(&state.data_dir, &format!("应用退出：还原系统代理失败(可手动关闭): {e}"));
                }
            }
            // 兜底强制退出（Python 版为独立进程 kill 瞬退、无此问题；Rust 进程内化后，
            // 析构阶段的 WebView2 销毁/异步运行时 drop 可能卡死——实测退出停留在
            // 「正在停止进程内代理」后进程挂死，用户重复点关闭 9 次）。上方清理均为
            // 同步快操作且已完成，2 秒后无条件 process::exit 保证必然退出。
            if let Some(state) = app_handle.try_state::<AppState>() {
                fs_utils::app_log(&state.data_dir, "应用退出：清理完成，2 秒内强制退出（兜底防析构卡死）");
            }
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                std::process::exit(0);
            });
        }
        // F-75：mac 点击 Dock 图标重新打开（Reopen 变体带 #[cfg(target_os = "macos")]，
        // Windows 编译不存在该变体，匹配须整体门控）。无边框窗口（decorations:false）
        // 最小化后，macOS 不会自动恢复窗口——手动 unminimize + show + 置前聚焦。
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { has_visible_windows, .. } = event {
            if !has_visible_windows {
                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        }
    });
}
