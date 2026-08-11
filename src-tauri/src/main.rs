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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
