use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;

#[tauri::command]
pub fn switch_account(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
    proxy_port: Option<u16>,
) -> Result<(), String> {
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "switch/重置/保存 已到达 Rust: ps_dir={:?}, bridge 存在={}",
            ps_dir,
            bridge.exists()
        ),
    );
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }

    fs_utils::app_log(&state.data_dir, &format!("开始切换账号: user_id={user_id}"));

    // C4：豆包快照可选纳入 IndexedDB（设置开关控制，其他应用不受影响）
    let include_idb = target_app.as_deref() == Some("Doubao")
        && state.settings().doubao_snapshot_include_idb;
    // C1：一键以账号打开时注入代理（>0 才传给桥）
    let inject_port = proxy_port.filter(|p| *p > 0);

    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &bridge.to_string_lossy(),
        "-Action",
        "Switch",
        "-UserId",
        &user_id,
        "-TargetApp",
        target_app.as_deref().unwrap_or("TraeWork"),
        "-Json",
    ]);
    if let Some(p) = inject_port {
        cmd.args(["-ProxyPort", &p.to_string()]);
    }
    if include_idb {
        cmd.arg("-IncludeIndexedDB");
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：隐藏切换时闪出的黑色控制台窗口
        .spawn()
        .map_err(|e| format!("启动切换失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("切换脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let uid_for_dc = user_id.clone();
    let dc_dir = data_dir.clone();

    // stdout 线程：NDJSON -> switch-progress 事件
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut done_emitted = false;
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                let _ = app2.emit("switch-progress", &l);
                // 检测 done / fatal 行，emit switch-done 事件
                if l.contains("\"stage\":\"done\"") || l.contains("\"stage\":\"fatal\"") {
                    let success = l.contains("\"stage\":\"done\"");
                    done_emitted = true;
                    let _ = app2.emit("switch-done", serde_json::json!({ "success": success, "raw": l }));
                    // 切换成功后补充该账号的账户中心（icube-dc）id 预留记录（只记录不展示）
                    // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
                    if success && target_app.as_deref() != Some("Doubao") {
                        let _ = crate::commands::trae_apps::backfill_dc_id_for(&dc_dir, &uid_for_dc);
                    }
                }
            }
        }
        let exit_status = child.wait();
        // 仅当脚本未输出 done/fatal 时才在结束时兜底 emit，避免对同一次切换重复发两次 switch-done
        if !done_emitted {
            let success = matches!(&exit_status, Ok(s) if s.success());
            let _ = app2.emit("switch-done", serde_json::json!({ "success": success, "raw": format!("exit: {:?}", exit_status) }));
        }
    });

    // stderr 线程：防止管道缓冲区写满导致子进程死锁
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path = data_dir.join("logs").join("switcher.log");
            let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let l = format!("[stderr] {}", l.trim());
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                    {
                        let _ = writeln!(f, "[{}] {}", fs_utils::now_ts(), l);
                    }
                }
            }
        });
    }

    Ok(())
}

/// 保存当前登录态：关闭 Trae → 精准备份到 userId 槽位 → 重新启动
/// 通过 NDJSON 事件流式返回进度，前端订阅 save-login-progress / save-login-done
#[tauri::command]
pub fn save_current_login(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "switch/重置/保存 已到达 Rust: ps_dir={:?}, bridge 存在={}",
            ps_dir,
            bridge.exists()
        ),
    );
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }

    fs_utils::app_log(&state.data_dir, &format!("开始保存当前登录态: user_id={user_id}"));

    // C4：豆包快照可选纳入 IndexedDB
    let include_idb = target_app.as_deref() == Some("Doubao")
        && state.settings().doubao_snapshot_include_idb;

    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &bridge.to_string_lossy(),
        "-Action",
        "SaveCurrentLogin",
        "-UserId",
        &user_id,
        "-TargetApp",
        target_app.as_deref().unwrap_or("TraeWork"),
        "-Json",
    ]);
    if include_idb {
        cmd.arg("-IncludeIndexedDB");
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：隐藏控制台窗口
        .spawn()
        .map_err(|e| format!("启动保存登录态失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("保存登录态脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let uid_for_dc = user_id.clone();
    let dc_dir = data_dir.clone();

    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut done_emitted = false;
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                let _ = app2.emit("save-login-progress", &l);
                if l.contains("\"stage\":\"done\"") || l.contains("\"stage\":\"fatal\"") {
                    let success = l.contains("\"stage\":\"done\"");
                    done_emitted = true;
                    let _ = app2.emit(
                        "save-login-done",
                        serde_json::json!({ "success": success, "raw": l }),
                    );
                    // 保存登录态成功后同样补充 dc id 预留记录（快照刚生成，来源最可靠）
                    // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
                    if success && target_app.as_deref() != Some("Doubao") {
                        let _ = crate::commands::trae_apps::backfill_dc_id_for(&dc_dir, &uid_for_dc);
                    }
                }
            }
        }
        let exit_status = child.wait();
        if !done_emitted {
            let success = matches!(&exit_status, Ok(s) if s.success());
            let _ = app2.emit(
                "save-login-done",
                serde_json::json!({ "success": success, "raw": format!("exit: {:?}", exit_status) }),
            );
        }
    });

    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path = data_dir.join("logs").join("switcher.log");
            let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let l = format!("[stderr] {}", l.trim());
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                    {
                        let _ = writeln!(f, "[{}] {}", fs_utils::now_ts(), l);
                    }
                }
            }
        });
    }

    Ok(())
}

/// 6 层设备标识重置：调用 PowerShell 脚本的 ResetDeviceIds 动作
/// 通过 NDJSON 事件流式返回进度，前端订阅 device-reset-progress / device-reset-done
/// target_app：TraeWork（默认，TRAE SOLO CN）/ Trae（Trae CN IDE），决定清理哪个应用的数据目录
#[tauri::command]
pub fn reset_device_ids(
    app: AppHandle,
    state: State<AppState>,
    target_app: Option<String>,
) -> Result<(), String> {
    let target = match target_app.as_deref() {
        Some("Trae") => "Trae",
        _ => "TraeWork",
    };
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "switch/重置/保存 已到达 Rust: ps_dir={:?}, bridge 存在={}, target_app={}",
            ps_dir,
            bridge.exists(),
            target
        ),
    );
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }

    fs_utils::app_log(&state.data_dir, "开始 6 层设备标识重置");

    let mut child = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &bridge.to_string_lossy(),
            "-Action",
            "ResetDeviceIds",
            "-TargetApp",
            target,
            "-Json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：隐藏控制台窗口
        .spawn()
        .map_err(|e| format!("启动设备标识重置失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("设备重置脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();

    // stdout 线程：NDJSON -> device-reset-progress 事件
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut done_emitted = false;
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                let _ = app2.emit("device-reset-progress", &l);
                if l.contains("\"stage\":\"done\"") || l.contains("\"stage\":\"fatal\"") {
                    let success = l.contains("\"stage\":\"done\"");
                    done_emitted = true;
                    let _ = app2.emit(
                        "device-reset-done",
                        serde_json::json!({ "success": success, "raw": l }),
                    );
                }
            }
        }
        let exit_status = child.wait();
        if !done_emitted {
            let success = matches!(&exit_status, Ok(s) if s.success());
            let _ = app2.emit(
                "device-reset-done",
                serde_json::json!({ "success": success, "raw": format!("exit: {:?}", exit_status) }),
            );
        }
    });

    // stderr 线程：防止管道缓冲区写满导致子进程死锁
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path = data_dir.join("logs").join("switcher.log");
            let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let l = format!("[stderr] {}", l.trim());
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                    {
                        let _ = writeln!(f, "[{}] {}", fs_utils::now_ts(), l);
                    }
                }
            }
        });
    }

    Ok(())
}
