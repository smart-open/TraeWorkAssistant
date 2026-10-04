use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::switcher::{Action, RunArgs, TauriSink, TargetApp};

/// 登录态快照信息
#[derive(Serialize, Clone)]
pub struct ProfileInfo {
    pub slot: String,
    pub size_bytes: u64,
    pub file_count: u64,
    pub last_modified: String,
}

/// profiles 根目录：%APPDATA%\AIWorkAssistant\data\profiles\（Trae Work）
/// 或 data\profiles_trae\（Trae CN IDE）、data\profiles_doubao\（豆包）、
/// data\profiles_codebuddy\（CodeBuddy 桌面）、data\profiles_workbuddy\（WorkBuddy 桌面，
/// authfile 布局）——profiles_workbuddy 由切换桥按 -TargetApp WorkBuddy 写入，
/// 此处映射保证通用快照命令（list/backup/restore/delete）与桥同源，防误操作 TraeWork 快照
fn profiles_dir(state: &State<AppState>, target_app: Option<&str>) -> PathBuf {
    match target_app {
        Some("Trae") => state.data_dir.join("data").join("profiles_trae"),
        Some("Doubao") => state.data_dir.join("data").join("profiles_doubao"),
        Some("CodeBuddy") => state.data_dir.join("data").join("profiles_codebuddy"),
        Some("WorkBuddy") => state.data_dir.join("data").join("profiles_workbuddy"),
        Some("Qoder") => state.data_dir.join("data").join("profiles_qoder"),
        Some("QoderWork") => state.data_dir.join("data").join("profiles_qoder_work"),
        _ => state.data_dir.join("data").join("profiles"),
    }
}

/// 归一化 target_app：仅接受 "Trae"（Trae CN IDE）/ "Doubao"（豆包）/ "CodeBuddy"（CodeBuddy 桌面）/
/// "WorkBuddy"（WorkBuddy 桌面，authfile 布局）/ "Qoder"（Qoder CN IDE）/
/// "QoderWork"（Qoder Work 独立客户端，electron-root 布局），其余一律视为 TraeWork
fn normalize_target_app(target_app: Option<&str>) -> &'static str {
    match target_app {
        Some("Trae") => "Trae",
        Some("Doubao") => "Doubao",
        Some("CodeBuddy") => "CodeBuddy",
        Some("WorkBuddy") => "WorkBuddy",
        Some("Qoder") => "Qoder",
        Some("QoderWork") => "QoderWork",
        _ => "TraeWork",
    }
}

/// 递归计算目录大小和文件数
pub(crate) fn dir_stats(path: &std::path::Path) -> (u64, u64) {
    let mut size = 0u64;
    let mut count = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let (s, c) = dir_stats(&p);
                size += s;
                count += c;
            } else {
                size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                count += 1;
            }
        }
    }
    (size, count)
}

/// 格式化文件大小
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// 列出所有已保存的登录态快照（target_app: TraeWork=TRAE SOLO CN / Trae=Trae CN IDE）
#[tauri::command]
pub fn profile_list(state: State<AppState>, target_app: Option<String>) -> Vec<ProfileInfo> {
    let dir = profiles_dir(&state, target_app.as_deref());
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let slot = entry.file_name().to_string_lossy().to_string();
                let (size, count) = dir_stats(&entry.path());
                let last_modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| {
                        let dt = chrono::DateTime::<chrono::Local>::from(std::time::SystemTime::UNIX_EPOCH + d);
                        dt.format("%Y-%m-%d %H:%M:%S").to_string()
                    })
                    .unwrap_or_else(|| "-".to_string());
                out.push(ProfileInfo {
                    slot,
                    size_bytes: size,
                    file_count: count,
                    last_modified,
                });
            }
        }
    }
    // 按修改时间倒序
    out.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    out
}

/// 备份当前登录态到指定 slot（switcher BackupCurrent 动作，进程内直调）
#[tauri::command]
pub fn profile_backup(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // uid 直接作为快照槽目录名传入 switcher，先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let target = normalize_target_app(target_app.as_deref());
    fs_utils::app_log(
        &state.data_dir,
        &format!("开始备份登录态: user_id={user_id}, target_app={target}"),
    );
    // C4：豆包快照可选纳入 IndexedDB
    let include_idb = target == "Doubao" && state.settings().doubao_snapshot_include_idb;
    // F-80 §5.10 保存守卫预探测（2026-10-02 审查新增）：Qoder（IDE vscdb 解密）/
    // QoderWork（客户端 Cookies qoderuid 解密）预探测当前登录账号并传入 L2 守卫
    //（icube/electron_root 守卫优先采用），防把 A 的登录态存进 B 的槽位。
    // 未登录/解密失败 → 空串 fail-open（守卫回退策略见 switcher::mod）。
    // profile_backup 为同步命令：DPAPI 解密仅读一个小文件，耗时可忽略
    let expected_current_uid = match target {
        "Qoder" => crate::commands::qoder::live_account_id(&state).unwrap_or_default(),
        "QoderWork" => crate::commands::qoder::live_work_account_id(&state).unwrap_or_default(),
        _ => String::new(),
    };
    let args = RunArgs {
        action: Action::BackupCurrent,
        target_app: TargetApp::parse(target),
        user_id: Some(user_id),
        proxy_port: None,
        include_indexeddb: include_idb,
        expected_current_uid,
        machine_id_override: None,
        data_dir: state.data_dir.clone(),
    };
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    // 后台线程执行（含优雅关闭等待，不阻塞命令返回；与原 stdout 读线程同语义）
    std::thread::spawn(move || {
        // issue #44：与切换管线对齐——panic 时 profile-done 仍会发射，前端不会永久锁在备份中
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sink = TauriSink::new(&app2, "profile-progress", &data_dir);
            crate::switcher::run_action(args, &sink)
        }));
        super::switch::finish_action_thread(
            &app2,
            "profile-done",
            &data_dir,
            result,
            serde_json::json!({ "action": "backup" }),
        );
    });
    Ok(())
}

/// 恢复指定 slot 的登录态（关闭客户端 → 恢复 → 启动，switcher RestoreOnly 动作）
#[tauri::command]
pub fn profile_restore(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // 检查快照是否存在（按目标应用的 profiles 根目录）
    let slot_dir = profiles_dir(&state, target_app.as_deref()).join(&user_id);
    if !slot_dir.exists() {
        return Err(format!("账号 {} 的登录态快照不存在", user_id));
    }
    // uid 直接作为快照槽目录名传入 switcher，先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let target = normalize_target_app(target_app.as_deref());
    fs_utils::app_log(
        &state.data_dir,
        &format!("开始恢复登录态: user_id={user_id}, target_app={target}"),
    );
    // C4：豆包快照可选纳入 IndexedDB（恢复侧对快照内含 IndexedDB 一律回写，此开关主要影响备份）
    let include_idb = target == "Doubao" && state.settings().doubao_snapshot_include_idb;
    // F-80 §5.10.2：Qoder 恢复取账号绑定 machine_id（须在 user_id 被 move 前计算）
    let machine_id_override = if target == "Qoder" {
        crate::commands::qoder::machine_id_of(&state, user_id.trim())
    } else {
        None
    };
    let args = RunArgs {
        action: Action::RestoreOnly,
        target_app: TargetApp::parse(target),
        user_id: Some(user_id),
        proxy_port: None,
        include_indexeddb: include_idb,
        expected_current_uid: String::new(),
        machine_id_override,
        data_dir: state.data_dir.clone(),
    };
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    std::thread::spawn(move || {
        // issue #44：与切换管线对齐——panic 时 profile-done 仍会发射，前端不会永久锁在恢复中
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sink = TauriSink::new(&app2, "profile-progress", &data_dir);
            crate::switcher::run_action(args, &sink)
        }));
        super::switch::finish_action_thread(
            &app2,
            "profile-done",
            &data_dir,
            result,
            serde_json::json!({ "action": "restore" }),
        );
    });
    Ok(())
}

/// 删除指定 slot 的登录态快照
#[tauri::command]
pub fn profile_delete(
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // uid 直接拼进 profiles 根目录路径且本命令整目录删除（remove_dir_all），必须先校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let slot_dir = profiles_dir(&state, target_app.as_deref()).join(&user_id);
    if !slot_dir.exists() {
        return Ok(()); // 不存在视为已删除
    }
    std::fs::remove_dir_all(&slot_dir)
        .map_err(|e| format!("删除快照失败: {e}"))?;
    fs_utils::app_log(&state.data_dir, &format!("已删除登录态快照: user_id={user_id}"));
    Ok(())
}

/// 格式化辅助函数（给前端展示用）
#[tauri::command]
pub fn profile_format_size(bytes: u64) -> String {
    format_size(bytes)
}
