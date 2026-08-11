use serde::Serialize;
use std::process::Command;
use tauri::{AppHandle, State};

use crate::state::AppState;

#[derive(Serialize)]
pub struct EnvStatus {
    pub installed: bool,
    pub running: bool,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[tauri::command]
pub fn env_check(_app: AppHandle, _state: State<AppState>) -> EnvStatus {
    let (installed, path, version) = detect_trae();
    let running = is_running();
    EnvStatus {
        installed,
        running,
        version,
        path,
    }
}

#[tauri::command]
pub fn open_trae_website(_app: AppHandle) -> Result<(), String> {
    Command::new("cmd")
        .args(["/c", "start", "https://www.trae.cn"])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn detect_trae() -> (bool, Option<String>, Option<String>) {
    let candidates = [
        "%LOCALAPPDATA%\\Programs\\Trae\\Trae.exe",
        "%ProgramFiles%\\Trae\\Trae.exe",
    ];
    for c in candidates {
        let expanded = expand_env(c);
        if std::path::Path::new(&expanded).exists() {
            let version = version_of(&expanded);
            return (true, Some(expanded), version);
        }
    }
    // 回退：注册表查询
    if let Some(p) = registry_trae_path() {
        let version = version_of(&p);
        return (true, Some(p), version);
    }
    (false, None, None)
}

fn expand_env(p: &str) -> String {
    p.replace("%LOCALAPPDATA%", &std::env::var("LOCALAPPDATA").unwrap_or_default())
        .replace("%ProgramFiles%", &std::env::var("ProgramFiles").unwrap_or_default())
}

fn version_of(path: &str) -> Option<String> {
    let ps = format!(
        "(Get-Item '{}').VersionInfo.FileVersion",
        path.replace('\'', "''")
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn registry_trae_path() -> Option<String> {
    let out = Command::new("reg")
        .args([
            "query",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
            "/s",
            "/f",
            "Trae",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        let line = line.trim();
        if line.starts_with("DisplayIcon") {
            if let Some(v) = line.split("REG_SZ").nth(1) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn is_running() -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Trae.exe", "/NH"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("Trae.exe")
        }
        Err(_) => false,
    }
}
