use std::io::{BufRead, BufReader, Write};
use std::process::Command;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;

#[tauri::command]
pub fn switch_account(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
) -> Result<(), String> {
    let ps_dir = if let Ok(r) = std::env::var("TAURI_RESOURCE_DIR") {
        std::path::PathBuf::from(r).join("ps")
    } else {
        state.python_dir.join("../ps")
    };
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }

    fs_utils::app_log(&state.data_dir, &format!("开始切换账号: user_id={user_id}"));

    let mut child = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &bridge.to_string_lossy(),
            "-Action",
            "Switch",
            "-UserId",
            &user_id,
            "-Json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动切换失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("切换脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();

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
