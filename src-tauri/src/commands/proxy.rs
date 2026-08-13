use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;

pub struct ProxyHandle {
    pub child: Child,
    pub port: u16,
    pub started_at: i64,
    pub captured: Arc<AtomicI64>,
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

/// 安全获取 Mutex 锁，即使中毒也能恢复（避免 panic 级联）。
fn safe_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[tauri::command]
pub fn proxy_start(
    app: AppHandle,
    state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
    port: u16,
) -> Result<ProxyStatus, String> {
    {
        let guard = safe_lock(&proxy_state);
        if guard.is_some() {
            return Err("代理已在运行".into());
        }
    }
    // 兜底：端口为 0 时退化为固定端口 8899，避免注入 TRAE 的代理地址无效（见 store.ts 同款兜底）
    let port = if port == 0 { 8899 } else { port };
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
    let captured = Arc::new(AtomicI64::new(0));
    {
        let mut g = safe_lock(&proxy_state);
        *g = Some(ProxyHandle {
            child,
            port,
            started_at,
            captured: captured.clone(),
        });
    }

    fs_utils::app_log(
        &state.data_dir,
        &format!("代理已启动: port={port}, pid 已归入 ProxyHandle"),
    );

    // 启动成功：向「实时代理输出」面板明确推送监听地址，便于一眼确认代理是否接上流量
    let listen_line = format!("代理已监听 127.0.0.1:{port}（等待 Trae Work 流量…）");
    let _ = app.emit("proxy-log", &listen_line);
    {
        let log_path =
            std::path::Path::new(&data_dir).join("logs").join("proxy.log");
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        append_log(&log_path, &listen_line);
    }

    let app_for_thread = app.clone();
    let app_for_stderr = app.clone();
    let data_dir2 = data_dir.clone();
    let data_dir3 = data_dir.clone();
    let captured_thread = captured.clone();

    // stdout 线程：逐行读取 -> 事件 emit + 日志追加
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
                    captured_thread.fetch_add(1, Ordering::Relaxed);
                    let _ = app_for_thread.emit("account-captured", &uid);
                }
                let _ = append_log(&log_path, &l);
            }
        }
    });

    // stderr 线程：单独读取，防止管道缓冲区写满导致子进程死锁
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path =
                std::path::Path::new(&data_dir3).join("logs").join("proxy.log");
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let l = format!("[stderr] {}", l.trim());
                    // 同时发到前端，避免 Python 启动即崩（如缺依赖）时面板空白、错误只进文件
                    let _ = app_for_stderr.emit("proxy-log", &l);
                    let _ = append_log(&log_path, &l);
                }
            }
        });
    }

    // 同步把 Windows 系统代理指向本机端口，使 TRAE 鉴权请求(api.trae.cn)汇入本代理
    let proxy_addr = format!("127.0.0.1:{port}");
    match set_win_proxy(&proxy_addr) {
        Ok(()) => {
            let _ = app.emit(
                "proxy-log",
                &format!("已设置系统代理 -> {proxy_addr}（TRAE 鉴权流量将汇入本代理）"),
            );
            fs_utils::app_log(&state.data_dir, &format!("已设置系统代理 -> {proxy_addr}"));
        }
        Err(e) => {
            let _ = app.emit("proxy-log", &format!("[warn] 设置系统代理失败: {e}"));
        }
    }

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
    state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
) -> Result<ProxyStatus, String> {
    let mut g = safe_lock(&proxy_state);
    let (port, captured) = match &*g {
        Some(h) => (h.port, h.captured.load(Ordering::Relaxed)),
        None => (0, 0),
    };
    if let Some(h) = g.take() {
        let c = h.captured.load(Ordering::Relaxed);
        fs_utils::app_log(&state.data_dir, &format!("代理已停止: 共捕获 {c} 个账号"));
        // h 在此处 drop，Drop trait 会 kill + wait 子进程
    }
    // 还原系统代理，避免本机全局断网
    if let Err(e) = clear_win_proxy() {
        fs_utils::app_log(&state.data_dir, &format!("还原系统代理失败(可手动在设置中关闭): {e}"));
    }
    Ok(ProxyStatus {
        running: false,
        port,
        captured,
        started_at: None,
    })
}

#[tauri::command]
pub fn proxy_status(
    _app: AppHandle,
    _state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
) -> ProxyStatus {
    let g = safe_lock(&proxy_state);
    match &*g {
        Some(h) => ProxyStatus {
            running: true,
            port: h.port,
            captured: h.captured.load(Ordering::Relaxed),
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
    if let Some(idx) = line.find("user=") {
        let rest = &line[idx + 5..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len());
        let uid = &rest[..end];
        if uid.chars().all(|c| c.is_ascii_digit()) && !uid.is_empty() {
            return Some(uid.to_string());
        }
    }
    if let Some(idx) = line.find("user_id=") {
        let rest = &line[idx + 8..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len());
        let uid = &rest[..end];
        if uid.chars().all(|c| c.is_ascii_digit()) && !uid.is_empty() {
            return Some(uid.to_string());
        }
    }
    None
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

// ---------------- Windows 系统代理设置 ----------------
// TRAE 的鉴权请求(api.trae.cn)不走 Electron `--proxy-server` 命令行代理，但会读取
// Windows 系统代理(WinINet)。故启动本地代理时同步把系统代理指向本机端口，TRAE 的全部
// 流量(含鉴权)即汇入我们的 MITM 代理；停止时还原，避免全局断网。
#[cfg(target_os = "windows")]
pub(crate) fn set_win_proxy(addr: &str) -> Result<(), String> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    run_reg(key, "ProxyEnable", "REG_DWORD", "1")?;
    run_reg(key, "ProxyServer", "REG_SZ", addr)?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn clear_win_proxy() -> Result<(), String> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    run_reg(key, "ProxyEnable", "REG_DWORD", "0")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_reg(key: &str, name: &str, kind: &str, value: &str) -> Result<(), String> {
    let status = Command::new("reg")
        .args(["add", key, "/v", name, "/t", kind, "/d", value, "/f"])
        .status()
        .map_err(|e| format!("设置系统代理失败: {e}"))?;
    if !status.success() {
        return Err(format!("reg add 失败: {name}"));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn set_win_proxy(_addr: &str) -> Result<(), String> {
    Err("仅 Windows 支持系统代理设置".into())
}

#[cfg(not(target_os = "windows"))]
fn clear_win_proxy() -> Result<(), String> {
    Err("仅 Windows 支持系统代理设置".into())
}
