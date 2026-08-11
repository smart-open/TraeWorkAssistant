// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod fs_utils;
mod jwt;
mod models;
mod python;
mod state;

use state::AppState;
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

fn main() {
    let state = match AppState::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("初始化失败: {e}");
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        .manage(Mutex::new(Option::<commands::proxy::ProxyHandle>::None))
        .invoke_handler(tauri::generate_handler![
            commands::env::env_check,
            commands::env::open_trae_website,
            commands::cert::cert_status,
            commands::cert::cert_install,
            commands::proxy::proxy_start,
            commands::proxy::proxy_stop,
            commands::proxy::proxy_status,
            commands::accounts::accounts_list,
            commands::accounts::account_add_manual,
            commands::accounts::account_delete,
            commands::accounts::groups_list,
            commands::accounts::group_create,
            commands::accounts::group_update,
            commands::accounts::group_delete,
            commands::accounts::group_move,
            commands::checkin::checkin_start,
            commands::switch::switch_account,
            commands::misc::device_reset,
            commands::misc::jwt_parse,
            commands::misc::logs_query,
            commands::misc::settings_get,
            commands::misc::settings_set,
            commands::misc::invite_link,
            commands::misc::task_register,
            commands::misc::task_status,
            commands::misc::task_unregister,
        ])
        .setup(|app| {
            let state = app.state::<AppState>();
            let settings = state.settings();
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "应用启动: tray={}, launch_minimized={}, auto_start_proxy={}",
                    settings.tray, settings.launch_minimized, settings.auto_start_proxy
                ),
            );

            // 创建系统托盘（仅在设置启用时；失败不阻断启动）
            if settings.tray {
                let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                    let toggle_item =
                        MenuItem::with_id(app, "toggle", "显示/隐藏", true, None::<&str>)?;
                    let sep = PredefinedMenuItem::separator(app)?;
                    let quit_item =
                        MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
                    let menu = Menu::with_items(app, &[&toggle_item, &sep, &quit_item])?;

                    let icon = app
                        .default_window_icon()
                        .cloned()
                        .ok_or("找不到默认窗口图标")?;

                    TrayIconBuilder::new()
                        .icon(icon)
                        .tooltip("Trae Work 助手")
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
                            "quit" => {
                                let st = app.state::<AppState>();
                                fs_utils::app_log(&st.data_dir, "菜单：用户请求退出应用");
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

            // 启动时最小化（托盘模式下隐藏窗口，否则仅最小化到任务栏）
            if settings.launch_minimized {
                if let Some(window) = app.get_webview_window("main") {
                    if settings.tray {
                        let _ = window.hide();
                        fs_utils::app_log(&state.data_dir, "启动最小化：窗口已隐藏到托盘");
                    } else {
                        let _ = window.minimize();
                        fs_utils::app_log(&state.data_dir, "启动最小化：窗口已最小化到任务栏");
                    }
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 托盘启用时，关闭按钮隐藏窗口而非退出
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.app_handle().state::<AppState>();
                let settings = state.settings();
                if settings.tray {
                    api.prevent_close();
                    let _ = window.hide();
                    fs_utils::app_log(&state.data_dir, "窗口关闭请求被拦截：已隐藏到托盘");
                }
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
        }
    });
}
