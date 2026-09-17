use serde::Serialize;
use std::process::Command;
use tauri::{AppHandle, State};

use crate::platform::cmd::sys_command;
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

/// F-75 M0-0.5：平台标志下发（os + arch），前端存 store.platform 控制入口显隐与文案。
/// 零参数零副作用，启动时调用一次。
#[tauri::command]
pub fn platform_info() -> serde_json::Value {
    serde_json::json!({
        "os": crate::platform::OS,           // "windows" | "macos"
        "arch": std::env::consts::ARCH,      // "x86_64" | "aarch64"
    })
}

#[tauri::command]
pub fn open_trae_website(_app: AppHandle) -> Result<(), String> {
    // F-75 M2-2.5：打开网站按平台分派（Windows cmd /c start / macOS open）
    #[cfg(windows)]
    {
        sys_command("cmd")
            .args(["/c", "start", "https://www.trae.cn"])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        sys_command("open")
            .arg("https://www.trae.cn")
            .spawn()
            .map_err(|e| e.to_string())?;
    }
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
    // SQLite 化（P2）：app_settings 入 kv 文档
    let store = crate::store::db(&state.data_dir);
    let mut current: serde_json::Value = store.kv_get("app_settings");
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
        let _ = store.kv_set("app_settings", &current);
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
    // 优先 ProductVersion（用户认知的产品版本，如 Trae 3.3.100 / Trae Work 0.1.65 /
    // CodeBuddy 4.12.0），缺失时回退 FileVersion（内部构建号）——实测 Electron 系客户端
    // 两者差异巨大（TRAE SOLO CN.exe FileVersion=2.3.83557 而 ProductVersion=0.1.65，
    // CodeBuddy CN.exe FileVersion=1.106.1.0 而 ProductVersion=4.12.0），旧版恒读
    // FileVersion 导致顶栏版本显示为构建号而非产品版本
    let ps = format!(
        "$v=(Get-Item '{}').VersionInfo; if ($v.ProductVersion) {{ $v.ProductVersion }} else {{ $v.FileVersion }}",
        path.replace('\'', "''")
    );
    let out = sys_command("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    // 归一化：4 段式 ProductVersion 去掉末尾冗余 ".0"（WorkBuddy 5.4.7.0 → 5.4.7）；
    // 3 段式保持原样（CodeBuddy 4.12.0 不能截成 4.12）
    let s = if s.matches('.').count() == 3 && s.ends_with(".0") {
        s[..s.len() - 2].to_string()
    } else {
        s
    };
    Some(s)
}

fn registry_trae_path() -> Option<String> {
    for root in ["HKCU", "HKLM"] {
        let out = match sys_command("reg")
            .args([
                "query",
                &format!("{root}\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
                "/s",
                "/f",
                "TRAE",
            ])
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
    #[cfg(windows)]
    {
        let out = sys_command("tasklist")
            .args(["/FI", "IMAGENAME eq Trae CN.exe", "/NH"])
            .output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.contains("Trae CN.exe")
            }
            Err(_) => false,
        }
    }
    #[cfg(target_os = "macos")]
    {
        // F-75 审查修复 #6：mac 走 sysinfo 进程探测（设计 §5.5；主进程名待 M-1 侦察 6 核对）
        !crate::commands::process::images_running(&["Trae CN.exe"]).is_empty()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

fn is_running() -> bool {
    #[cfg(windows)]
    {
        let out = sys_command("tasklist")
            .args(["/FI", "IMAGENAME eq TRAE SOLO CN.exe", "/NH"])
            .output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.contains("TRAE SOLO CN.exe")
            }
            Err(_) => false,
        }
    }
    #[cfg(target_os = "macos")]
    {
        !crate::commands::process::images_running(&["TRAE SOLO CN.exe"]).is_empty()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

// ── F-01：安装位置自动识别 app_locate（跨应用通用，三级探测）────────────────
// 探测顺序：用户手动指定（app_settings.json 持久化值）→ 注册表卸载键 → 默认路径候选
// → 运行进程反查。方案依据 doubao-trae-switch-plan.md §1.3 / workbuddy-switch-plan.md §2.1。
// F-75 P2-2：Windows 档案与四级链整体 cfg 门控；mac 走 bundle 定位链（app_locate_macos，
// 设计 §4.2「app_locate 内 cfg 分派」——档案数据按应用×平台分离，非复用同一结构）。

#[derive(Serialize)]
pub struct AppLocate {
    /// 应用标识：trae_work | trae | doubao | workbuddy
    pub app: String,
    /// 可执行文件路径（macOS 形态为 .app bundle 目录路径，供 LaunchServices `open` 启动）
    pub exe: Option<String>,
    pub user_data_dir: String,
    pub version: Option<String>,
    /// settings | registry | default | process | not_found（mac 的 mdfind 命中归入 default）
    pub source: String,
}

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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
        // CodeBuddy IDE 独立探测（顶栏安装徽标/打开客户端）：默认路径同 WorkBuddy 的 Electron 惯例。
        // 本机实测（2026-09-12）：安装形态为 CodeBuddy CN（带空格），exe/进程名均为 "CodeBuddy CN"，
        // 注册表 DisplayName=CodeBuddy CN (User)，故 exe/进程/注册表候选同时覆盖不带 CN 的通用形态
        "codebuddy" => AppProfile {
            display: "CodeBuddy",
            reg_patterns: &["CodeBuddy"],
            reg_exe_names: &["CodeBuddy.exe", "CodeBuddy CN.exe"],
            exe_candidates: &[
                "%LOCALAPPDATA%\\Programs\\CodeBuddy\\CodeBuddy.exe",
                "%LOCALAPPDATA%\\Programs\\CodeBuddy CN\\CodeBuddy CN.exe",
            ],
            proc_names: &["CodeBuddy", "CodeBuddy CN"],
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
    launch_doubao(&state, &exe, proxy_port)
}

/// 豆包启动平台分派（F-75 P2-2：mac 走 LaunchServices `open` 直启 .app）
#[cfg(target_os = "macos")]
fn launch_doubao(state: &State<AppState>, exe: &str, proxy_port: Option<u16>) -> Result<(), String> {
    // mac 首版：`open` 直启不传启动参数，--proxy-server 注入暂不支持——豆包
    // Chromium 内核遵循系统代理，MITM 经系统代理路径仍可捕获（豆包 mac 域
    // 布局与启动形态待 M-1 侦察确认后再评估注入支持）。系统代理残留清理与
    // 三级关闭同属 Windows 注入链，此处一并跳过。
    let _ = (state, proxy_port);
    crate::platform::cmd::sys_command("open")
        .arg(exe)
        .spawn()
        .map_err(|e| format!("启动豆包失败: {e}"))?;
    Ok(())
}

/// 豆包启动平台分派（Windows：原注入链行为零变化）
#[cfg(not(target_os = "macos"))]
fn launch_doubao(state: &State<AppState>, exe: &str, proxy_port: Option<u16>) -> Result<(), String> {
    // 直开（不注入代理）前，清理可能指向已停止本地代理的残留系统代理
    if proxy_port.is_none() {
        crate::commands::proxy::cleanup_stale_local_proxy(state);
    }
    // Chromium 单实例：已运行的窗口会忽略新启动参数，注入代理前先关闭现有进程确保生效
    if proxy_port.is_some() {
        crate::commands::process::graceful_kill_app("Doubao")?;
    }
    let mut cmd = Command::new(exe);
    if let Some(port) = proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    cmd.spawn().map_err(|e| format!("启动豆包失败: {e}"))?;
    Ok(())
}

// ── Buddy 双应用（WorkBuddy / CodeBuddy）打开与环境检测 ────────────────────

/// 打开 WorkBuddy 桌面版：复用 app_locate workbuddy 档案四级探测取 exe。
/// 分离启动（spawn 不等待），不注入代理、不做三级关闭。
#[tauri::command(async)]
pub fn open_workbuddy_app(state: State<AppState>) -> Result<(), String> {
    open_buddy_app(&state, "workbuddy", "WorkBuddy")
}

/// 打开 CodeBuddy 桌面版：复用 app_locate codebuddy 档案四级探测取 exe。
/// 分离启动（spawn 不等待），不注入代理、不做三级关闭。
#[tauri::command(async)]
pub fn open_codebuddy_app(state: State<AppState>) -> Result<(), String> {
    open_buddy_app(&state, "codebuddy", "CodeBuddy")
}

/// Buddy 双应用打开共用实现（open_doubao_app 的极简版：无代理注入、无进程关闭）
fn open_buddy_app(state: &State<AppState>, app: &str, display: &str) -> Result<(), String> {
    let loc = app_locate_inner(state, app);
    let exe = loc
        .exe
        .ok_or_else(|| format!("未检测到 {display} 客户端，请先安装或手动指定路径"))?;
    launch_buddy(exe, display)
}

/// Buddy 启动平台分派（F-75 P2-2：mac 走 LaunchServices `open` 直启 .app）
#[cfg(target_os = "macos")]
fn launch_buddy(exe: String, display: &str) -> Result<(), String> {
    crate::platform::cmd::sys_command("open")
        .arg(exe)
        .spawn()
        .map_err(|e| format!("启动 {display} 失败: {e}"))?;
    Ok(())
}

/// Buddy 启动平台分派（Windows：分离启动，行为零变化）
#[cfg(not(target_os = "macos"))]
fn launch_buddy(exe: String, display: &str) -> Result<(), String> {
    Command::new(exe)
        .spawn()
        .map_err(|e| format!("启动 {display} 失败: {e}"))?;
    Ok(())
}

/// CodeBuddy 桌面环境检测结果（Buddy 双应用域；字段 snake_case 直出前端）
#[derive(Serialize)]
pub struct CodeBuddyEnvCheck {
    pub installed: bool,
    pub running: bool,
    pub exe: Option<String>,
    pub version: Option<String>,
    /// auth 文件当前登录 uid（解析失败/未登录为 None）
    pub uid: Option<String>,
    /// auth 文件当前登录昵称（解析失败/未登录为 None）
    pub nickname: Option<String>,
}

/// CodeBuddy 桌面环境检测：exe/版本走 app_locate codebuddy 档案；uid/昵称复用
/// workbuddy_scan_auth_file 的 auth 文件解析（CodeBuddy 与 WorkBuddy 共享同一 auth 文件
/// %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info，本机实测）。
/// 环境检查不抛错：auth 文件不存在/解析失败时 uid/nickname 置 None。
#[tauri::command(async)]
pub fn codebuddy_env_check(state: State<AppState>) -> CodeBuddyEnvCheck {
    let loc = app_locate_inner(&state, "codebuddy");
    let running = is_running_codebuddy();
    let (uid, nickname) = match crate::commands::workbuddy::workbuddy_scan_auth_file(state.clone())
    {
        Ok(Some(scan)) => (
            (!scan.uid.is_empty()).then_some(scan.uid),
            (!scan.nickname.is_empty()).then_some(scan.nickname),
        ),
        _ => (None, None),
    };
    CodeBuddyEnvCheck {
        installed: loc.exe.is_some(),
        running,
        exe: loc.exe,
        version: loc.version,
        uid,
        nickname,
    }
}

/// CodeBuddy 进程检测：同时覆盖通用形态 CodeBuddy.exe 与本机实测的 "CodeBuddy CN.exe"
fn is_running_codebuddy() -> bool {
    #[cfg(target_os = "macos")]
    {
        // F-75 审查修复 #6：mac 走 sysinfo 进程探测
        return !crate::commands::process::images_running(&["CodeBuddy CN.exe", "CodeBuddy.exe"])
            .is_empty();
    }
    #[cfg(not(target_os = "macos"))]
    {
        for exe in ["CodeBuddy CN.exe", "CodeBuddy.exe"] {
            let out = sys_command("tasklist")
                .args(["/FI", &format!("IMAGENAME eq {exe}"), "/NH"])
                .output();
            if matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains(exe)) {
                return true;
            }
        }
        false
    }
}

/// app_locate 的内部版本（供 open_* 命令与 workbuddy 模块复用；无需 Option 包装）。
/// F-75 P2-2 平台分派：mac 走 bundle 定位链，Windows 走四级探测链（两链互斥编译）。
pub(crate) fn app_locate_inner(state: &State<AppState>, app: &str) -> AppLocate {
    #[cfg(target_os = "macos")]
    {
        return app_locate_macos(state, app);
    }
    #[cfg(not(target_os = "macos"))]
    {
        return app_locate_windows(state, app);
    }
}

/// macOS bundle 定位链（F-75 P2-2，设计 §4.2「app_locate 内 cfg 分派」）：
/// settings（.app 目录形态）→ bundle 探测（/Applications、~/Applications）→
/// Spotlight mdfind 兜底 → 运行中进程回退（sysinfo exe → bundle 根归一）。
/// bundle 候选名（显示名 + 主进程名）与 CFBundleExecutable 白名单沿用 Windows
/// 档案的 proc_names（设计 §M2：主进程名 mac 同构）；各应用 mac bundle 名与
/// CFBundleExecutable 实测确认属 M-1 侦察，不符时最坏结果为 not_found（白名单
/// 防串台），不会误启动。user_data_dir 采用设计 §4.3 mac 预填假设（M-1 待确认）。
#[cfg(target_os = "macos")]
fn app_locate_macos(state: &State<AppState>, app: &str) -> AppLocate {
    use crate::switcher::locate::{
        bundle_exe_matches, is_bundle_dir, locate_bundle_dir, mdfind_bundle,
        read_info_plist_value,
    };

    let key = app.to_lowercase();
    let home = std::env::var("HOME").unwrap_or_default();
    // (显示名, 主进程名/CFBundleExecutable 白名单, mac 数据目录预填假设, settings 键)
    let (display, proc_names, user_data_dir, settings_key): (
        &str,
        &[&str],
        String,
        Option<&str>,
    ) = match key.as_str() {
        "trae" | "trae_cn" | "traecn" | "ide" => (
            "Trae",
            &["Trae CN"],
            format!("{home}/Library/Application Support/Trae CN"),
            Some("trae_cn_path"),
        ),
        "doubao" => (
            "豆包",
            &["Doubao"],
            format!("{home}/Library/Application Support/Doubao"),
            Some("doubao_path"),
        ),
        "workbuddy" => (
            "WorkBuddy",
            &["WorkBuddy"],
            format!("{home}/.workbuddy"),
            Some("workbuddy_path"),
        ),
        "codebuddy" => (
            "CodeBuddy",
            &["CodeBuddy", "CodeBuddy CN"],
            format!("{home}/.codebuddy"),
            Some("codebuddy_path"),
        ),
        // trae_work / traework / work / solo 及其它值 → 默认 Trae Work
        _ => (
            "Trae Work",
            &["TRAE SOLO CN", "TRAE SOLO", "Trae"],
            format!("{home}/Library/Application Support/TRAE SOLO CN"),
            Some("trae_path"),
        ),
    };

    // 1) 用户手动指定（settings）：mac 形态为 .app 目录（非文件），is_file 校验不适用
    if let Some(sk) = settings_key {
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
            if !p.is_empty() {
                let pb = std::path::PathBuf::from(&p);
                if is_bundle_dir(&pb) {
                    return finish_locate_macos(display, &user_data_dir, pb, "settings");
                }
            }
        }
    }

    // 2) bundle 探测（显示名 + 主进程名作目录名候选，白名单防串台）
    let mut names: Vec<&str> = vec![display];
    for n in proc_names {
        if !names.contains(n) {
            names.push(n);
        }
    }
    for name in &names {
        if let Some(found) = locate_bundle_dir(name, proc_names) {
            return finish_locate_macos(display, &user_data_dir, found, "default");
        }
    }
    // 3) Spotlight 兜底（mdfind 秒回；仅上面未命中时）
    for name in &names {
        if let Some(found) = mdfind_bundle(name, proc_names) {
            return finish_locate_macos(display, &user_data_dir, found, "default");
        }
    }

    // 4) 运行中进程回退：sysinfo exe 路径归一回 .app 根（exe → MacOS → Contents
    //    → <App>.app，ancestors().nth(3)）后经白名单防串台
    for p in crate::commands::process::mac_exe_paths() {
        if let Some(app_dir) = p.ancestors().nth(3) {
            if is_bundle_dir(app_dir) && bundle_exe_matches(app_dir, proc_names) {
                return finish_locate_macos(
                    display,
                    &user_data_dir,
                    app_dir.to_path_buf(),
                    "process",
                );
            }
        }
    }

    AppLocate {
        // not_found 时沿用原始参数（与 Windows 链 not_found 分支同语义，前端按此分派）
        app: app.to_string(),
        exe: None,
        user_data_dir,
        version: None,
        source: "not_found".into(),
    }
}

/// mac 命中后统一补版本号（Info.plist CFBundleShortVersionString）并组装结果。
/// 不做 Windows 版的豆包 `_win` 后缀加工（mac 版本形态待 M-1 侦察）。
#[cfg(target_os = "macos")]
fn finish_locate_macos(
    display: &str,
    user_data_dir: &str,
    app: std::path::PathBuf,
    source: &str,
) -> AppLocate {
    use crate::switcher::locate::read_info_plist_value;
    let version = read_info_plist_value(&app, "CFBundleShortVersionString");
    AppLocate {
        app: display.to_lowercase().replace(' ', "_"),
        exe: Some(app.to_string_lossy().to_string()),
        user_data_dir: user_data_dir.to_string(),
        version,
        source: source.into(),
    }
}

/// Windows 四级探测链（原 app_locate_inner 主体，F-75 P2-2 起平台分派；行为零变化）
#[cfg(not(target_os = "macos"))]
fn app_locate_windows(state: &State<AppState>, app: &str) -> AppLocate {
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
#[cfg(not(target_os = "macos"))]
fn finish_locate(profile: &AppProfile, exe: String, source: &str, version: Option<String>) -> AppLocate {
    let mut version = version.or_else(|| version_of(&exe));
    // 豆包客户端版本号官方形态带平台后缀（与安装包命名一致，如 2.28.13_win）
    if profile.settings_key == Some("doubao_path") {
        if let Some(v) = version.as_mut() {
            if !v.ends_with("_win") {
                *v = format!("{v}_win");
            }
        }
    }
    AppLocate {
        app: profile.display.to_lowercase().replace(' ', "_"),
        exe: Some(exe),
        user_data_dir: profile.user_data_dir.clone(),
        version,
        source: source.into(),
    }
}

/// 注册表卸载键搜索（按应用档案的 DisplayName 片段与 exe 名参数化）
#[cfg(not(target_os = "macos"))]
fn registry_app_path(profile: &AppProfile) -> Option<String> {
    for pattern in profile.reg_patterns {
        for root in ["HKCU", "HKLM"] {
            let out = match sys_command("reg")
                .args([
                    "query",
                    &format!("{root}\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
                    "/s",
                    "/f",
                    pattern,
                ])
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
#[cfg(not(target_os = "macos"))]
fn process_exe_path(proc_names: &[&str]) -> Option<String> {
    let names = proc_names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(",");
    let ps = format!(
        "(Get-Process -Name @({names}) -ErrorAction SilentlyContinue | Where-Object {{ $_.Path }} | Select-Object -First 1).Path"
    );
    let out = sys_command("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || !std::path::Path::new(&s).is_file() {
        None
    } else {
        Some(s)
    }
}
