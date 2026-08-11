use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

pub struct ProxyHandle {
    pub child: Child,
    pub port: u16,
    pub started_at: i64,
}

#[derive(serde::Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub port: u16,
    pub captured: i64,
    pub started_at: Option<i64>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[tauri::command]
pub fn proxy_start(
    app: AppHandle,
    state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
    port: u16,
) -> Result<ProxyStatus, String> {
    {
        let guard = proxy_state.lock().unwrap();
        if guard.is_some() {
            return Err("代理已在运行".into());
        }
    }
    let script_path = state.python_dir.join("device_proxy.py");
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let data_dir = state.data_dir.to_string_lossy().to_string();
    let port_s = port.to_string();
    let mut cmd = Command::new(&state.python_exe);
    cmd.arg(&script_path)
        .env("TRAEDATA_DIR", &data_dir)
        .env("PROXY_PORT", &port_s)
        .env("AUTO_CAPTURE_JWT", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动代理失败: {e}"))?;
    let stdout = child.stdout.take().ok_or("代理无标准输出")?;
    let stderr = child.stderr.take();

    let started_at = now_secs();
    {
        let mut g = proxy_state.lock().unwrap();
        *g = Some(ProxyHandle {
            child,
            port,
            started_at,
        });
    }

    let app_for_thread = app.clone();
    let data_dir2 = data_dir.clone();
    std::thread::spawn(move || {
        let log_path = std::path::Path::new(&data_dir2).join("logs").join("proxy.log");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                let _ = app_for_thread.emit("proxy-log", &l);
                if let Some(uid) = extract_uid(&l) {
                    let _ = app_for_thread.emit("account-captured", &uid);
                }
                let _ = append_log(&log_path, &l);
            }
        }
        drop(stderr);
    });

    Ok(ProxyStatus {
        running: true,
        port,
        captured: 0,
        started_at: Some(started_at),
    })
}

#[tauri::command]
pub fn proxy_stop(
    _app: AppHandle,
    _state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
) -> Result<ProxyStatus, String> {
    let mut g = proxy_state.lock().unwrap();
    let port = match &*g {
        Some(h) => h.port,
        None => 0,
    };
    if let Some(mut h) = g.take() {
        let _ = h.child.kill();
        let _ = h.child.wait();
    }
    Ok(ProxyStatus {
        running: false,
        port,
        captured: 0,
        started_at: None,
    })
}

#[tauri::command]
pub fn proxy_status(
    _app: AppHandle,
    _state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
) -> ProxyStatus {
    let g = proxy_state.lock().unwrap();
    match &*g {
        Some(h) => ProxyStatus {
            running: true,
            port: h.port,
            captured: 0,
            started_at: Some(h.started_at),
        },
        None => ProxyStatus {
            running: false,
            port: 0,
            captured: 0,
            started_at: None,
        },
    }
}

fn extract_uid(line: &str) -> Option<String> {
    // 形如 "...user=4487568582777872..." 或 "user_id=..."
    let idx = line.find("user=").or_else(|| line.find("user_id="))?;
    let rest = &line[idx + 5..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '_'))
        .unwrap_or(rest.len());
    let uid = &rest[..end];
    if uid.chars().all(|c| c.is_ascii_digit()) && !uid.is_empty() {
        Some(uid.to_string())
    } else {
        None
    }
}

fn append_log(path: &std::path::Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] {}", crate::fs_utils::now_ts(), line);
    }
}
