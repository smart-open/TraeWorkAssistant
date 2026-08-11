use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use tauri::{AppHandle, Emitter, State};

use serde::Deserialize;
use crate::state::AppState;
use crate::commands::accounts::{build_account_views, resolve_user_ids};
use crate::python::spawn_script;

#[derive(Deserialize)]
pub struct CheckinOpts {
    pub scope: String,
    #[serde(default)]
    pub user_ids: Option<Vec<String>>,
    #[serde(default)]
    pub skip_checked_in: bool,
    #[serde(default)]
    pub skip_expired: bool,
}

#[tauri::command]
pub fn checkin_start(
    app: AppHandle,
    state: State<AppState>,
    opts: CheckinOpts,
) -> Result<(), String> {
    let mut uids = resolve_user_ids(&state, &opts.scope, opts.user_ids)?;
    if opts.skip_checked_in || opts.skip_expired {
        let views = build_account_views(&state);
        uids.retain(|u| {
            let v = views.iter().find(|a| &a.user_id == u);
            let mut keep = true;
            if let Some(v) = v {
                if opts.skip_checked_in && v.checked_today == Some(true) {
                    keep = false;
                }
                if opts.skip_expired
                    && (v.jwt_exp_hours.is_none() || v.jwt_exp_hours.unwrap() <= 0.0)
                {
                    keep = false;
                }
            }
            keep
        });
    }
    let accounts_arg = uids.join(",");
    let retry = state.settings().retry.max(0) as u32;
    let mut args = vec!["--json-stream".to_string()];
    if !accounts_arg.is_empty() {
        args.push("--accounts".to_string());
        args.push(accounts_arg);
    }
    if retry > 0 {
        args.push("--retry".to_string());
        args.push(retry.to_string());
    }

    let mut child = spawn_script(&state, "auto_checkin.py", &args, true)?;
    let stdout = child.stdout.take().ok_or("无法获取子进程输出")?;
    let stderr = child.stderr.take();
    let data_dir = state.data_dir.clone();
    let app2 = app.clone();

    crate::fs_utils::app_log(&state.data_dir, &format!("签到已启动: {} 个账号", uids.len()));

    // stdout 线程：NDJSON 解析 -> 事件 emit + 日志追加
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let log_path = data_dir.join("logs").join("checkin.log");
        if let Some(p) = log_path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) {
                    let _ = app2.emit("checkin-progress", &v);
                    if v.get("type").and_then(|t| t.as_str()) == Some("done") {
                        let _ = app2.emit("checkin-done", &v);
                    }
                }
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    let _ = writeln!(f, "[{}] {}", crate::fs_utils::now_ts(), l);
                }
            }
        }
        let _ = child.wait();
    });

    // stderr 线程：防止管道缓冲区写满导致子进程死锁
    if let Some(stderr) = stderr {
        let data_dir2 = state.data_dir.clone();
        std::thread::spawn(move || {
            let log_path = data_dir2.join("logs").join("checkin.log");
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let l = format!("[stderr] {}", l.trim());
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                    {
                        let _ = writeln!(f, "[{}] {}", crate::fs_utils::now_ts(), l);
                    }
                }
            }
        });
    }

    Ok(())
}
