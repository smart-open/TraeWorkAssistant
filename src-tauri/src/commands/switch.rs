use std::io::{BufRead, BufReader, Write};
use std::process::Command;
use tauri::{AppHandle, Emitter, State};

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
    let app2 = app.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                let _ = app2.emit("switch-progress", &l);
            }
        }
        let _ = child.wait();
    });
    Ok(())
}
