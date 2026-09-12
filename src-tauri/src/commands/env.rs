use serde::Serialize;
use std::os::windows::process::CommandExt;
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

// 以下命令含耗时操作（注册表全量搜索、PowerShell 进程调用、最多 5s 的进程关闭轮询），
// 一律标记 async 派发到线程池执行，避免阻塞 UI 主线程（与 updater 冻结修复同因）。

#[tauri::command(async)]
pub fn env_check(_app: AppHandle, state: State<AppState>) -> EnvStatus {
    let (installed, path, version) = detect_trae(state.settings().trae_path);
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
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 启动本地 Trae Work 客户端。
/// 若传入 proxy_port（代理运行中），自动注入 `--proxy-server` 让 Trae 走本地代理，
/// 无需用户在 Trae 设置里手动配置代理。
#[tauri::command(async)]
pub fn open_trae_app(_app: AppHandle, state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    let (installed, path, _) = detect_trae(state.settings().trae_path);
    if !installed {
        return Err("未检测到本地 Trae Work 安装，请在「环境配置」中指定 exe 路径".into());
    }
    let exe = path.ok_or("未找到 Trae Work 可执行文件路径")?;
    persist_detected_path(&state, "trae_path", &exe);
    // 直开（不注入代理）前，清理可能指向已停止本地代理的残留系统代理，避免请求被 RESET
    if proxy_port.is_none() {
        crate::commands::proxy::cleanup_stale_local_proxy(&state);
    }
    // 代理注入要求 Trae 以 --proxy-server 启动。Electron 单实例下，已运行的窗口会忽略新启动
    // 参数，再次点击只会聚焦旧窗口，导致全程不走代理、无法捕获账号。故注入代理前先关闭现有
    // 进程，确保参数真正生效（F-47 三级关闭：优雅关闭→树杀强杀→人工介入）。
    // （无代理时正常打开，不杀进程。）
    if proxy_port.is_some() {
        crate::commands::process::graceful_kill_app("TraeWork")?;
    }
    let mut cmd = Command::new(&exe);
    if let Some(port) = proxy_port {
        // Electron/Chromium 支持 --proxy-server 启动参数
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    cmd.spawn()
        .map_err(|e| format!("启动 Trae Work 失败: {e}"))?;
    Ok(())
}

/// 检测 Trae CN IDE（与 Trae Work/SOLO CN 是两个独立应用）
#[tauri::command(async)]
pub fn env_check_trae_cn(_app: AppHandle, state: State<AppState>) -> EnvStatus {
    let (installed, path, version) = detect_trae_cn(state.settings().trae_cn_path.clone());
    let running = is_running_cn();
    EnvStatus { installed, running, version, path }
}

/// 打开 Trae CN IDE。与 Trae Work 同款代理注入：传入 proxy_port 时以 --proxy-server 启动，
/// 让 Trae 的流量也走本地 MITM 代理（捕获账号/观察请求）。
#[tauri::command(async)]
pub fn open_trae_cn_app(_app: AppHandle, state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    let (installed, path, _) = detect_trae_cn(state.settings().trae_cn_path.clone());
    if !installed {
        return Err("未检测到 Trae 安装，请在「环境配置」中指定 Trae 安装路径".into());
    }
    let exe = path.ok_or("未找到 Trae 可执行文件路径")?;
    persist_detected_path(&state, "trae_cn_path", &exe);
    // 直开前清理可能指向已停止本地代理的残留系统代理
    if proxy_port.is_none() {
        crate::commands::proxy::cleanup_stale_local_proxy(&state);
    }
    // Electron 单实例：已运行的窗口会忽略新启动参数，注入代理前先三级关闭现有进程确保生效
    if proxy_port.is_some() {
        crate::commands::process::graceful_kill_app("Trae")?;
    }
    let mut cmd = Command::new(&exe);
    if let Some(port) = proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    cmd.spawn().map_err(|e| format!("启动 Trae 失败: {e}"))?;
    Ok(())
}

/// exe 路径持久化兜底（F-47）：自动探测成功时把结果写入 app_settings.json，
/// 之后即使注册表/默认目录变化，也能用上次成功的路径直接启动。
/// 用户手动指定（设置页）优先级更高，且仅在探测值与存量值不同时写盘。
fn persist_detected_path(state: &State<AppState>, key: &str, exe: &str) {
    let path = state.path("app_settings.json");
    let mut current: serde_json::Value = crate::fs_utils::read_json(&path);
    if !current.is_object() {
        current = serde_json::json!({});
    }
    let changed = current
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s != exe)
        .unwrap_or(true);
    if changed {
        if let Some(obj) = current.as_object_mut() {
            obj.insert(key.to_string(), serde_json::json!(exe));
        }
        let _ = crate::fs_utils::write_json(&path, &current);
    }
}

fn detect_trae_cn(custom: Option<String>) -> (bool, Option<String>, Option<String>) {
    if let Some(p) = custom {
        let p = p.trim().to_string();
        if !p.is_empty() && std::path::Path::new(&p).is_file() {
            let version = version_of(&p);
            return (true, Some(p), version);
        }
    }
    let candidates = [
        "%LOCALAPPDATA%\\Programs\\Trae CN\\Trae CN.exe",
        "%ProgramFiles%\\Trae CN\\Trae CN.exe",
    ];
    for c in candidates {
        let expanded = expand_env(c);
        if std::path::Path::new(&expanded).exists() {
            let version = version_of(&expanded);
            return (true, Some(expanded), version);
        }
    }
    (false, None, None)
}

fn detect_trae(custom: Option<String>) -> (bool, Option<String>, Option<String>) {
    // 优先使用用户在设置中指定的路径（兼容自定义安装目录）
    if let Some(p) = custom {
        let p = p.trim().to_string();
        if !p.is_empty() && std::path::Path::new(&p).is_file() {
            let version = version_of(&p);
            return (true, Some(p), version);
        }
    }
    let candidates = [
        "%LOCALAPPDATA%\\Programs\\TRAE SOLO CN\\TRAE SOLO CN.exe",
        "%LOCALAPPDATA%\\Programs\\TRAE SOLO\\TRAE SOLO.exe",
        "%ProgramFiles%\\TRAE SOLO CN\\TRAE SOLO CN.exe",
        "%ProgramFiles%\\TRAE SOLO\\TRAE SOLO.exe",
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
        .creation_flags(0x08000000)
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
    for root in ["HKCU", "HKLM"] {
        let out = match Command::new("reg")
            .args([
                "query",
                &format!("{root}\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
                "/s",
                "/f",
                "TRAE",
            ])
            .creation_flags(0x08000000)
            .output()
        {
            Ok(o) => o,
            Err(_) => continue,
        };
        let s = String::from_utf8_lossy(&out.stdout);
        // reg query /s 以 HKEY_ 开头的行分隔每个注册表键，逐键解析
        let mut icon: Option<String> = None;
        let mut loc: Option<String> = None;
        let mut name_ok = false;
        let mut best: Option<String> = None;
        for line in s.lines() {
            let line = line.trim();
            if line.starts_with("HKEY_") {
                if name_ok {
                    if let Some(p) = resolve_reg_candidate(&icon, &loc) {
                        best = Some(p);
                        break;
                    }
                }
                icon = None;
                loc = None;
                name_ok = false;
                continue;
            }
            if let Some(v) = line.strip_prefix("DisplayName") {
                if let Some(val) = v.split("REG_SZ").nth(1) {
                    if val.to_uppercase().contains("TRAE") {
                        name_ok = true;
                    }
                }
            } else if let Some(v) = line.strip_prefix("DisplayIcon") {
                if let Some(val) = v.split("REG_SZ").nth(1) {
                    icon = Some(val.trim().to_string());
                }
            } else if let Some(v) = line.strip_prefix("InstallLocation") {
                if let Some(val) = v.split("REG_SZ").nth(1) {
                    loc = Some(val.trim().to_string());
                }
            }
        }
        if name_ok {
            if let Some(p) = resolve_reg_candidate(&icon, &loc) {
                best = Some(p);
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

/// 从注册表 DisplayIcon / InstallLocation 推导 exe 路径
fn resolve_reg_candidate(icon: &Option<String>, loc: &Option<String>) -> Option<String> {
    if let Some(icon) = icon {
        if icon.to_lowercase().ends_with(".exe") && std::path::Path::new(icon).is_file() {
            return Some(icon.clone());
        }
    }
    if let Some(loc) = loc {
        for name in ["TRAE SOLO CN.exe", "TRAE SOLO.exe", "Trae.exe"] {
            let cand = format!("{loc}\\{name}");
            if std::path::Path::new(&cand).is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn is_running_cn() -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Trae CN.exe", "/NH"])
        .creation_flags(0x08000000)
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("Trae CN.exe")
        }
        Err(_) => false,
    }
}

fn is_running() -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq TRAE SOLO CN.exe", "/NH"])
        .creation_flags(0x08000000)
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("TRAE SOLO CN.exe")
        }
        Err(_) => false,
    }
}

// ── F-01：安装位置自动识别 app_locate（跨应用通用，三级探测）────────────────
// 探测顺序：用户手动指定（app_settings.json 持久化值）→ 注册表卸载键 → 默认路径候选
// → 运行进程反查。方案依据 doubao-trae-switch-plan.md §1.3 / workbuddy-switch-plan.md §2.1。

#[derive(Serialize)]
pub struct AppLocate {
    /// 应用标识：trae_work | trae | doubao | workbuddy
    pub app: String,
    pub exe: Option<String>,
    pub user_data_dir: String,
    pub version: Option<String>,
    /// settings | registry | default | process | not_found
    pub source: String,
}

struct AppProfile {
    display: &'static str,
    /// 注册表 DisplayName 匹配片段（按顺序尝试，大小写不敏感）
    reg_patterns: &'static [&'static str],
    /// 注册表 InstallLocation 下尝试的 exe 名
    reg_exe_names: &'static [&'static str],
    exe_candidates: &'static [&'static str],
    /// 进程名（不带 .exe）
    proc_names: &'static [&'static str],
    user_data_dir: String,
    /// 设置页手动路径的 settings 键（无则跳过 settings 级）
    settings_key: Option<&'static str>,
}

fn app_profile(target_app: Option<&str>) -> AppProfile {
    let key = target_app.unwrap_or("trae_work").to_lowercase();
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    match key.as_str() {
        "trae" | "trae_cn" | "traecn" | "ide" => AppProfile {
            display: "Trae",
            reg_patterns: &["Trae CN"],
            reg_exe_names: &["Trae CN.exe"],
            exe_candidates: &[
                "%LOCALAPPDATA%\\Programs\\Trae CN\\Trae CN.exe",
                "%ProgramFiles%\\Trae CN\\Trae CN.exe",
            ],
            proc_names: &["Trae CN"],
            user_data_dir: format!("{appdata}\\Trae CN"),
            settings_key: Some("trae_cn_path"),
        },
        "doubao" => AppProfile {
            display: "豆包",
            reg_patterns: &["Doubao", "豆包"],
            reg_exe_names: &["Doubao.exe"],
            exe_candidates: &[
                "%LOCALAPPDATA%\\Doubao\\Application\\Doubao.exe",
                "%ProgramFiles%\\Doubao\\Application\\Doubao.exe",
            ],
            proc_names: &["Doubao"],
            user_data_dir: format!("{local}\\Doubao\\User Data"),
            settings_key: Some("doubao_path"),
        },
        "workbuddy" => AppProfile {
            display: "WorkBuddy",
            reg_patterns: &["WorkBuddy", "CodeBuddy"],
            reg_exe_names: &["WorkBuddy.exe"],
            exe_candidates: &["%LOCALAPPDATA%\\Programs\\WorkBuddy\\WorkBuddy.exe"],
            proc_names: &["WorkBuddy"],
            user_data_dir: format!("{home}\\.workbuddy"),
            settings_key: Some("workbuddy_path"),
        },
        // CodeBuddy IDE 独立探测（顶栏安装徽标/打开客户端）：默认路径同 WorkBuddy 的 Electron 惯例
        "codebuddy" => AppProfile {
            display: "CodeBuddy",
            reg_patterns: &["CodeBuddy"],
            reg_exe_names: &["CodeBuddy.exe"],
            exe_candidates: &["%LOCALAPPDATA%\\Programs\\CodeBuddy\\CodeBuddy.exe"],
            proc_names: &["CodeBuddy"],
            user_data_dir: format!("{home}\\.codebuddy"),
            settings_key: Some("codebuddy_path"),
        },
        // trae_work / traework / work / solo 及其它值 → 默认 Trae Work
        _ => AppProfile {
            display: "Trae Work",
            reg_patterns: &["TRAE SOLO", "Trae Work"],
            reg_exe_names: &["TRAE SOLO CN.exe", "TRAE SOLO.exe", "Trae.exe"],
            exe_candidates: &[
                "%LOCALAPPDATA%\\Programs\\TRAE SOLO CN\\TRAE SOLO CN.exe",
                "%LOCALAPPDATA%\\Programs\\TRAE SOLO\\TRAE SOLO.exe",
                "%ProgramFiles%\\TRAE SOLO CN\\TRAE SOLO CN.exe",
                "%ProgramFiles%\\TRAE SOLO\\TRAE SOLO.exe",
                "%LOCALAPPDATA%\\Programs\\Trae\\Trae.exe",
                "%ProgramFiles%\\Trae\\Trae.exe",
            ],
            proc_names: &["TRAE SOLO CN", "TRAE SOLO", "Trae"],
            user_data_dir: format!("{appdata}\\TRAE SOLO CN"),
            settings_key: Some("trae_path"),
        },
    }
}

/// 打开豆包桌面版（复用 app_locate 豆包档案四级探测；命中即回写设置以便下次直开）。
/// 与 Trae 同款代理注入：proxy_port 存在时以 --proxy-server 启动，让豆包客户端流量
/// 必走本地 MITM 代理（凭证/额度自动抓取不再依赖系统代理设置）。
#[tauri::command(async)]
pub fn open_doubao_app(state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    let loc = app_locate_inner(&state, "doubao");
    let exe = loc.exe.ok_or("未检测到豆包安装，请在豆包「环境配置」中指定 Doubao.exe 路径")?;
    // 直开（不注入代理）前，清理可能指向已停止本地代理的残留系统代理
    if proxy_port.is_none() {
        crate::commands::proxy::cleanup_stale_local_proxy(&state);
    }
    // Chromium 单实例：已运行的窗口会忽略新启动参数，注入代理前先关闭现有进程确保生效
    if proxy_port.is_some() {
        crate::commands::process::graceful_kill_app("Doubao")?;
    }
    let mut cmd = Command::new(&exe);
    if let Some(port) = proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    cmd.spawn().map_err(|e| format!("启动豆包失败: {e}"))?;
    Ok(())
}

/// app_locate 的内部版本（供 open_* 命令与 workbuddy 模块复用；无需 Option 包装）
pub(crate) fn app_locate_inner(state: &State<AppState>, app: &str) -> AppLocate {
    let profile = app_profile(Some(app));

    if let Some(sk) = profile.settings_key {
        let settings = state.settings();
        let custom = match sk {
            "trae_path" => settings.trae_path,
            "trae_cn_path" => settings.trae_cn_path,
            "doubao_path" => settings.doubao_path,
            "workbuddy_path" => settings.workbuddy_path,
            "codebuddy_path" => settings.codebuddy_path,
            _ => None,
        };
        if let Some(p) = custom {
            let p = p.trim().to_string();
            if !p.is_empty() && std::path::Path::new(&p).is_file() {
                return finish_locate(&profile, p, "settings", None);
            }
        }
    }
    if let Some(exe) = registry_app_path(&profile) {
        return finish_locate(&profile, exe, "registry", None);
    }
    for c in profile.exe_candidates {
        let expanded = c
            .replace("%LOCALAPPDATA%", &std::env::var("LOCALAPPDATA").unwrap_or_default())
            .replace("%ProgramFiles%", &std::env::var("ProgramFiles").unwrap_or_default());
        if std::path::Path::new(&expanded).is_file() {
            return finish_locate(&profile, expanded, "default", None);
        }
    }
    if let Some(exe) = process_exe_path(profile.proc_names) {
        return finish_locate(&profile, exe, "process", None);
    }
    AppLocate {
        app: app.to_string(),
        exe: None,
        user_data_dir: profile.user_data_dir,
        version: None,
        source: "not_found".into(),
    }
}

/// 安装位置自动识别：统一返回 {exe, userDataDir, version, source}。
/// async 派发：内部有注册表全量搜索与 PowerShell 调用，同步会冻结 UI。
#[tauri::command(async)]
pub fn app_locate(state: State<AppState>, target_app: Option<String>) -> AppLocate {
    app_locate_inner(&state, target_app.as_deref().unwrap_or("trae_work"))
}

/// 命中后统一补齐版本号并组装结果
fn finish_locate(profile: &AppProfile, exe: String, source: &str, version: Option<String>) -> AppLocate {
    let version = version.or_else(|| version_of(&exe));
    AppLocate {
        app: profile.display.to_lowercase().replace(' ', "_"),
        exe: Some(exe),
        user_data_dir: profile.user_data_dir.clone(),
        version,
        source: source.into(),
    }
}

/// 注册表卸载键搜索（按应用档案的 DisplayName 片段与 exe 名参数化）
fn registry_app_path(profile: &AppProfile) -> Option<String> {
    for pattern in profile.reg_patterns {
        for root in ["HKCU", "HKLM"] {
            let out = match Command::new("reg")
                .args([
                    "query",
                    &format!("{root}\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
                    "/s",
                    "/f",
                    pattern,
                ])
                .creation_flags(0x08000000)
                .output()
            {
                Ok(o) => o,
                Err(_) => continue,
            };
            let s = String::from_utf8_lossy(&out.stdout);
            let pat_upper = pattern.to_uppercase();
            let mut icon: Option<String> = None;
            let mut loc: Option<String> = None;
            let mut name_ok = false;
            for line in s.lines() {
                let line = line.trim();
                if line.starts_with("HKEY_") {
                    if name_ok {
                        if let Some(hit) = resolve_reg_profile_candidate(&icon, &loc, profile) {
                            return Some(hit);
                        }
                    }
                    icon = None;
                    loc = None;
                    name_ok = false;
                    continue;
                }
                if let Some(v) = line.strip_prefix("DisplayName") {
                    if let Some(val) = v.split("REG_SZ").nth(1) {
                        if val.to_uppercase().contains(&pat_upper) {
                            name_ok = true;
                        }
                    }
                } else if let Some(v) = line.strip_prefix("DisplayIcon") {
                    if let Some(val) = v.split("REG_SZ").nth(1) {
                        icon = Some(val.trim().to_string());
                    }
                } else if let Some(v) = line.strip_prefix("InstallLocation") {
                    if let Some(val) = v.split("REG_SZ").nth(1) {
                        loc = Some(val.trim().to_string());
                    }
                }
            }
            if name_ok {
                if let Some(hit) = resolve_reg_profile_candidate(&icon, &loc, profile) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

/// 从注册表 DisplayIcon / InstallLocation 推导 exe 路径（按档案 exe 名匹配）
fn resolve_reg_profile_candidate(
    icon: &Option<String>,
    loc: &Option<String>,
    profile: &AppProfile,
) -> Option<String> {
    if let Some(icon) = icon {
        // DisplayIcon 可能带 ",0" 图标索引后缀
        let clean = icon.split(',').next().unwrap_or(icon).trim().to_string();
        if clean.to_lowercase().ends_with(".exe") && std::path::Path::new(&clean).is_file() {
            return Some(clean);
        }
    }
    if let Some(loc) = loc {
        for name in profile.reg_exe_names {
            let cand = format!("{loc}\\{name}");
            if std::path::Path::new(&cand).is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 运行进程反查 exe 路径（Get-Process 取 Path，应用运行中时最准）
fn process_exe_path(proc_names: &[&str]) -> Option<String> {
    let names = proc_names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(",");
    let ps = format!(
        "(Get-Process -Name @({names}) -ErrorAction SilentlyContinue | Where-Object {{ $_.Path }} | Select-Object -First 1).Path"
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || !std::path::Path::new(&s).is_file() {
        None
    } else {
        Some(s)
    }
}
