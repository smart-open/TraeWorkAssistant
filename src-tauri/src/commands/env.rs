use serde::Serialize;
use std::collections::HashMap;
// Windows 直启链专用（mac 走 `open` bundle 启动，不经 Command）
#[cfg(not(target_os = "macos"))]
use std::process::Command;
// Windows 直启链 stdio 静默专用（mac 启动走 `open`，不经 Command）
#[cfg(not(target_os = "macos"))]
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
// 仅 Windows 探测链兜底缓存消费（mac bundle 链无注册表/进程反查兜底）
#[cfg(not(target_os = "macos"))]
use std::time::{Duration, Instant};
use tauri::{AppHandle, State};

use crate::fs_utils;
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

/// 检测 Trae 客户端（双应用合并后唯一客户端）。
/// 原 `env_check` / `open_trae_app`（Trae Work 域）已随双应用合并退役，
/// 本命令为唯一 Trae 环境入口。
#[tauri::command(async)]
pub fn env_check_trae_cn(_app: AppHandle, state: State<AppState>) -> EnvStatus {
    let loc = app_locate_inner(&state, "trae");
    EnvStatus {
        installed: loc.exe.is_some(),
        running: is_running_cn(),
        version: loc.version,
        path: loc.exe,
    }
}

/// 打开 Trae 客户端（双应用合并后唯一客户端）。传入 proxy_port（代理运行中）时以
/// `--proxy-server` 启动，让 Trae 的流量走本地 MITM 代理（捕获账号/观察请求）。
#[tauri::command(async)]
pub fn open_trae_cn_app(_app: AppHandle, state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    // 统一探测（app_locate_inner 平台分派）：mac 命中 .app bundle、Windows 命中 exe。
    let loc = app_locate_inner(&state, "trae");
    let exe = loc
        .exe
        .ok_or("未检测到 Trae 安装，请在「环境配置」中指定 Trae 安装路径")?;
    persist_detected_path(&state, "trae_cn_path", &exe);
    // 直开前清理可能指向已停止本地代理的残留系统代理
    if proxy_port.is_none() {
        crate::commands::proxy::cleanup_stale_local_proxy(&state);
    }
    // Electron 单实例：已运行的窗口会忽略新启动参数，注入代理前先三级关闭现有进程确保生效
    if proxy_port.is_some() {
        crate::commands::process::graceful_kill_app("Trae")?;
    }
    // Agent 窗口修正（2026-10-14 合并版实测）：启动窗口类型取 live storage.json 的
    // runtimeWindow.lastTask（缺省回落 Agent）。客户端未运行时补丁为 Agent，保证
    // 顶部打开与切换恢复两条路径恒开 Agent 窗口；运行中不写（退出时会被客户端
    // 自身状态覆盖，写了也无效）。
    if !is_running_cn() {
        // 复用 app_locate 的 user_data_dir（与 app_profile/档案表同源，两平台
        // 分派：Windows=%APPDATA%\Trae CN、mac=~/Library/Application Support/Trae CN；
        // 审查修复 2026-10-14：原手拼 %APPDATA%\Trae CN 为第三处硬编码）
        let live_dir = std::path::PathBuf::from(&loc.user_data_dir);
        // best-effort 不阻断打开，但失败落日志（与 restore 路径的 Warn 口径一致，
        // 便于 storage.json 异常时排查「为何开的是 IDE 窗口」）
        if let Err(e) = crate::switcher::icube::apply_agent_window_fixup(&live_dir) {
            crate::fs_utils::app_log_default(&format!("Agent 窗口修正失败（打开路径）: {e}"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        // mac：LaunchServices `open` 单实例激活 .app bundle，--args 透传代理参数
        let mut cmd = crate::platform::cmd::sys_command("open");
        cmd.arg(&exe);
        if let Some(port) = proxy_port {
            cmd.args(["--args", &format!("--proxy-server=http://127.0.0.1:{port}")]);
        }
        cmd.spawn().map_err(|e| format!("启动 Trae 失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut cmd = Command::new(&exe);
        if let Some(port) = proxy_port {
            cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
        }
        // 静默子进程 stdio：客户端自身日志不混入应用控制台（客户端有自己的日志文件）
        cmd.stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().map_err(|e| format!("启动 Trae 失败: {e}"))?;
        Ok(())
    }
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
        // 审查修复（M13，融合 M11）：写失败原被 `let _` 吞掉——F-47 探测路径持久化
        // 兜底失效（注册表/默认目录变化后无法用上次成功路径启动）且无迹可循，落 app.log 留痕
        if let Err(e) = store.kv_set("app_settings", &current) {
            fs_utils::app_log(
                &state.data_dir,
                &format!("[env] app_settings 写入失败（探测路径 {key} 持久化兜底失效）: {e}"),
            );
        }
    }
}

/// exe 文件版本（ProductVersion 优先，回退 FileVersion）——qoder_env_check 复用。
/// Windows 三级实现（main 合并引入）：进程内 (路径, mtime, 大小) 缓存 → PE 版本资源
/// 直读（pe_version 模块，微秒级）→ powershell 兜底；mac 读 .app bundle Info.plist；
/// 其余平台恒 None。
#[cfg(windows)]
pub(crate) fn version_of(path: &str) -> Option<String> {
    version_cached(path, || {
        crate::pe_version::product_or_file_version(path)
            .or_else(|| powershell_version_of(path))
            .map(normalize_version)
    })
}

/// 原实现（保留作 PE 资源读取失败时的兜底）：ProductVersion 优先，缺失回退 FileVersion。
/// 实测 Electron 系客户端两者差异巨大（CodeBuddy CN.exe FileVersion=1.106.1.0 而
/// ProductVersion=4.12.0），
/// 旧版恒读 FileVersion 导致顶栏版本显示为构建号而非产品版本
#[cfg(windows)]
fn powershell_version_of(path: &str) -> Option<String> {
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
        None
    } else {
        Some(s)
    }
}

/// 版本串归一化：4 段式去掉末尾冗余 ".0"（WorkBuddy 5.4.7.0 → 5.4.7）；
/// 3 段式保持原样（CodeBuddy 4.12.0 不能截成 4.12）
#[cfg(windows)]
fn normalize_version(s: String) -> String {
    if s.matches('.').count() == 3 && s.ends_with(".0") {
        s[..s.len() - 2].to_string()
    } else {
        s
    }
}

/// mac 版本号读取：exe 路径（如 /Applications/Qoder CN.app/Contents/MacOS/Qoder CN IDE）
/// 上溯定位 .app bundle 根，读 Info.plist CFBundleShortVersionString——与
/// finish_locate_macos 同源，qoder_env_check（main 合并引入）跨平台复用入口；
/// 套用与 Windows 同款的 (路径, mtime, 大小) 记忆化缓存。
#[cfg(target_os = "macos")]
pub(crate) fn version_of(path: &str) -> Option<String> {
    version_cached(path, || {
        let start = std::path::Path::new(path);
        // 起点自身即 .app（2026-10-05 审查修复）：qoder_env_check 的 mac 候选直接是
        // bundle 目录（/Applications/Qoder CN IDE.app），原实现从 parent() 起步上溯，
        // 永远遇不到 .app 扩展名 → 版本号恒 null；先查自身再上溯，exe 裸路径行为不变
        if start.extension().map(|e| e == "app").unwrap_or(false) {
            return crate::switcher::locate::read_info_plist_value(
                start,
                "CFBundleShortVersionString",
            );
        }
        let mut cur = start.parent();
        while let Some(dir) = cur {
            if dir.extension().map(|e| e == "app").unwrap_or(false) {
                return crate::switcher::locate::read_info_plist_value(
                    dir,
                    "CFBundleShortVersionString",
                );
            }
            cur = dir.parent();
        }
        None
    })
}

/// 其余平台（非 Windows/macOS）占位：恒 None，保证调用方编译通过
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn version_of(_path: &str) -> Option<String> {
    None
}

/// 版本缓存条目：(mtime 毫秒, 文件大小) → 版本串（None 也缓存，避免反复读无版本资源的文件）
type VersionEntry = (u64, u64, Option<String>);

fn version_cache() -> &'static Mutex<HashMap<String, VersionEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, VersionEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 文件「版本是否变化」指纹（mtime + 大小）；取不到（文件不存在/无权限）时不做缓存
fn file_stamp(path: &str) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

/// 版本读取的记忆化：指纹（路径 + mtime + 大小）未变则直接返回缓存，否则 `compute` 后写回
/// （None 同样缓存，避免反复读无版本资源的文件）；文件 stat 不到时不缓存，每次重试。
fn version_cached(path: &str, compute: impl FnOnce() -> Option<String>) -> Option<String> {
    let key = path.to_ascii_lowercase();
    let stamp = file_stamp(path);
    if let Some(stamp) = stamp {
        let guard = version_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((mtime, len, v)) = guard.get(&key) {
            if (*mtime, *len) == stamp {
                return v.clone();
            }
        }
    }
    let v = compute();
    if let Some((mtime, len)) = stamp {
        let mut guard = version_cache().lock().unwrap_or_else(|e| e.into_inner());
        // 防御：键集合本应有界（客户端 exe 数量级），异常膨胀时整体清空重来
        if guard.len() > 256 {
            guard.clear();
        }
        guard.insert(key, (mtime, len, v.clone()));
    }
    v
}

/// Trae 客户端进程检测：Windows 用 sysinfo 进程表枚举 + 短 TTL 复用（main 合并，
/// 原 tasklist 子进程 ~260ms）；mac 按 bundle 主进程路径段匹配（M-1 ⑥：映像名均
/// 为 Electron，按名字比对恒不命中）
fn is_running_cn() -> bool {
    #[cfg(windows)]
    {
        crate::switcher::proc::any_running(&["Trae CN"])
    }
    #[cfg(target_os = "macos")]
    {
        crate::commands::process::mac_app_running(&["Trae CN"])
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

// ── F-01：安装位置自动识别 app_locate（跨应用通用，平台分派探测链）────────
// Windows 四级探测：用户手动指定（app_settings.json 持久化值）→ 默认路径候选 →
// 注册表卸载键 → 运行进程反查（F-45 对齐全库原有语义：默认路径优先、注册表兜底）。
// macOS bundle 定位链：settings → bundle 探测 → mdfind → 进程回退。
// 方案依据 doubao-trae-switch-plan.md §1.3 / workbuddy-switch-plan.md §2.1。

#[derive(Serialize)]
pub struct AppLocate {
    /// 应用标识：trae | doubao | workbuddy
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
    let key = target_app.unwrap_or("trae").to_lowercase();
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    match key.as_str() {
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
        // WorkBuddy AI 国际版客户端（buddy-ai 独立应用顶栏/环境配置专用档案，P1 审查修复）。
        // 2026-10-17 并存实测：国际版安装器与国内共用 %LOCALAPPDATA%\Programs\WorkBuddy\，
        // 以独立 exe 名 WorkBuddyAI.exe 区分（未落 WorkBuddyAI\ 独立目录）——候选表
        // 独立目录形态在前（标准布局优先），共享目录并存路径兜底；reg_exe_names 剔除
        // 国内 "WorkBuddy.exe" 防注册表/回查串台；注册表模式串收窄为 "WorkBuddyAI"/
        // "WorkBuddy AI"，避免误匹配国内 "WorkBuddy" 卸载项
        "workbuddy_ai" => AppProfile {
            display: "WorkBuddy AI",
            reg_patterns: &["WorkBuddyAI", "WorkBuddy AI"],
            reg_exe_names: &["WorkBuddyAI.exe", "WorkBuddy AI.exe"],
            exe_candidates: &[
                "%LOCALAPPDATA%\\Programs\\WorkBuddyAI\\WorkBuddyAI.exe",
                "%LOCALAPPDATA%\\Programs\\WorkBuddyAI\\WorkBuddy AI.exe",
                "%LOCALAPPDATA%\\Programs\\WorkBuddy\\WorkBuddyAI.exe",
            ],
            proc_names: &["WorkBuddyAI", "WorkBuddy AI"],
            user_data_dir: format!("{home}\\.workbuddy-ai"),
            settings_key: Some("workbuddy_ai_path"),
        },
        // CodeBuddy AI 国际客户端档案：用户实测与 codebuddy 同目录
        //（%LOCALAPPDATA%\Programs\CodeBuddy\CodeBuddy.exe），独立档案仅为
        // BuddyAI 应用上下文语义清晰（徽标/打开文案标注 AI，避免与国内页共用探测口径混淆）
        "codebuddy_ai" => AppProfile {
            display: "CodeBuddy AI",
            reg_patterns: &["CodeBuddy"],
            reg_exe_names: &["CodeBuddy.exe"],
            exe_candidates: &["%LOCALAPPDATA%\\Programs\\CodeBuddy\\CodeBuddy.exe"],
            proc_names: &["CodeBuddy"],
            user_data_dir: format!("{home}\\.codebuddy"),
            settings_key: Some("codebuddy_ai_path"),
        },
        // CodeBuddy IDE 独立探测（顶栏安装徽标/打开客户端）：默认路径同 WorkBuddy 的 Electron 惯例。
        // 本机实测（2026-09-12）：安装形态为 CodeBuddy CN（带空格），exe/进程名均为 "CodeBuddy CN"，
        // 注册表 DisplayName=CodeBuddy CN (User)，故 exe/进程/注册表候选同时覆盖不带 CN 的通用形态
        "codebuddy" => AppProfile {
            display: "CodeBuddy",
            reg_patterns: &["CodeBuddy"],
            reg_exe_names: &["CodeBuddy.exe", "CodeBuddy CN.exe"],
            // 2026-10-16（双客户端并存实测）：CN exe 优先——国际版 CodeBuddy.exe 读
            // %APPDATA%\CodeBuddy，环境探测/重置/「打开 CodeBuddy」若选它会操作错端；
            // 国际路径仅作老单安装布局兜底（国际版走 codebuddy_ai 档案）
            exe_candidates: &[
                "%LOCALAPPDATA%\\Programs\\CodeBuddy CN\\CodeBuddy CN.exe",
                "%LOCALAPPDATA%\\Programs\\CodeBuddy\\CodeBuddy.exe",
            ],
            proc_names: &["CodeBuddy", "CodeBuddy CN"],
            // W7 审查注记（2026-10-16 并存实测）：proc_names 仅第 4 级探测消费，而
            // CN 在盘时 exe_candidates 首位必先命中——"CodeBuddy" 映像名在并存布局
            // 下归属国际版的交叉仅影响 CN exe 缺失的极端现场（此时机器只装了国际
            // 端，兜底认国际 exe 是唯一可用语义）；运行态归属仲裁见 is_running_codebuddy
            // 的 codebuddy_cn_installed 收窄与切换器侧 ImageArk（switcher/profile.rs）
            user_data_dir: format!("{home}\\.codebuddy"),
            settings_key: Some("codebuddy_path"),
        },
        // trae / trae_cn / traecn / ide 及其它值 → 默认 Trae（双应用合并后唯一客户端）
        _ => AppProfile {
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
    }
}

// ── macOS 移植备忘（issue #45 修复时预埋）────────────────────────────────
// 本探测链路的 OS 耦合点集中在以下三处，移植时只需替换实现、档案表结构不变：
// 1. app_profile 本函数：exe_candidates / user_data_dir / proc_names 需按 macOS
//    实测路径出第二套档案（cfg(target_os) 分派）。已知映射：
//    - exe: /Applications/Trae Code.app/Contents/MacOS/Trae Code（默认安装无
//      per-user 目录，无 LOCALAPPDATA 等价物）
//    - 数据目录: ~/Library/Application Support/Trae CN（对应 %APPDATA%\Trae CN）
// 2. registry_app_path：注册表探测仅 Windows 有 → mac 实现直接返回 None
//    （或扩展为 Info.plist/Spotlight 查询，非必须）
// 3. process_exe_path / version_of / is_running_*：powershell+tasklist →
//    pgrep -l / mdls 或可执行文件属性读取；CommandExt.creation_flags 为
//    Windows 专属 API，需 cfg 包裹
// 注意：切换器侧 switcher/locate.rs（lnk/注册表/进程六级）有同构耦合点，
// mac 移植需一并处理；快照数据目录布局（switcher AppProfile.data_dir，即本文件
// AppProfile.user_data_dir 的对位字段）是行为核心，
// 移植前必须实机确认 macOS 版 Trae 的登录态文件结构与 Windows 同构。

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
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| format!("启动豆包失败: {e}"))?;
    Ok(())
}

// ── Buddy 双应用（WorkBuddy / CodeBuddy）打开与环境检测 ────────────────────

/// 打开 WorkBuddy AI 国际版客户端（buddy-ai 应用顶栏/环境配置，P1 审查修复）：
/// 走 workbuddy_ai 独立档案探测（Programs\WorkBuddyAI），不再误开国内 WorkBuddy。
/// 代理运行中传 proxy_port 以 `--proxy-server` 启动（对齐 Trae/豆包同款注入，
/// workbuddy.ai 流量必走本地 MITM 代理——模型目录 /v3/config 等官网请求可抓）。
#[tauri::command(async)]
pub fn open_workbuddy_ai_app(state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    open_buddy_app(&state, "workbuddy_ai", "WorkBuddy AI", proxy_port, "WorkBuddy AI")
}

/// 打开 CodeBuddy AI 国际客户端（buddy-ai 应用上下文；探测同 codebuddy 目录）。
/// 审查修复（M6）：kill_kind 传 "CodeBuddy AI"（映像表仅国际 CodeBuddy.exe），
/// 原传 "CodeBuddy" 在双客户端并存时会连带关闭国内 "CodeBuddy CN.exe" 实例。
#[tauri::command(async)]
pub fn open_codebuddy_ai_app(state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    open_buddy_app(&state, "codebuddy_ai", "CodeBuddy AI", proxy_port, "CodeBuddy AI")
}

/// 打开 WorkBuddy 桌面版：复用 app_locate workbuddy 档案四级探测取 exe。
/// 代理运行中传 proxy_port 以 `--proxy-server` 启动（copilot.tencent.com/codebuddy.cn/
/// workbuddy.cn 已入解密白名单，客户端侧流量可抓包观察）。
#[tauri::command(async)]
pub fn open_workbuddy_app(state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    open_buddy_app(&state, "workbuddy", "WorkBuddy", proxy_port, "WorkBuddy")
}

/// 打开 CodeBuddy 桌面版：复用 app_locate codebuddy 档案四级探测取 exe。
/// 代理注入同 open_workbuddy_app。
#[tauri::command(async)]
pub fn open_codebuddy_app(state: State<AppState>, proxy_port: Option<u16>) -> Result<(), String> {
    open_buddy_app(&state, "codebuddy", "CodeBuddy", proxy_port, "CodeBuddy")
}

/// Buddy 双应用打开共用实现。CN 双应用（open_workbuddy_app/open_codebuddy_app）
/// 传 proxy_port=None 走极简路径；国际双应用代理运行中注入 `--proxy-server`
/// （对齐 open_doubao_app：直开前清理残留系统代理；注入前三级关闭既有进程——
/// Electron/Chromium 单实例会忽略新启动参数）。
fn open_buddy_app(
    state: &State<AppState>,
    app: &str,
    display: &str,
    proxy_port: Option<u16>,
    kill_kind: &str,
) -> Result<(), String> {
    let loc = app_locate_inner(state, app);
    let exe = loc
        .exe
        .ok_or_else(|| format!("未检测到 {display} 客户端，请先安装或手动指定路径"))?;
    // 直开（不注入代理）前，清理可能指向已停止本地代理的残留系统代理
    if proxy_port.is_none() {
        crate::commands::proxy::cleanup_stale_local_proxy(state);
    }
    if proxy_port.is_some() {
        crate::commands::process::graceful_kill_app(kill_kind)?;
    }
    launch_buddy(exe, display, proxy_port)
}

/// Buddy 启动平台分派（F-75 P2-2：mac 走 LaunchServices `open` 直启 .app）
#[cfg(target_os = "macos")]
fn launch_buddy(exe: String, display: &str, proxy_port: Option<u16>) -> Result<(), String> {
    let mut cmd = crate::platform::cmd::sys_command("open");
    cmd.arg(exe);
    if let Some(port) = proxy_port {
        cmd.args(["--args", &format!("--proxy-server=http://127.0.0.1:{port}")]);
    }
    cmd.spawn().map_err(|e| format!("启动 {display} 失败: {e}"))?;
    Ok(())
}

/// Buddy 启动平台分派（Windows：分离启动，行为零变化）
#[cfg(not(target_os = "macos"))]
fn launch_buddy(exe: String, display: &str, proxy_port: Option<u16>) -> Result<(), String> {
    let mut cmd = Command::new(exe);
    if let Some(port) = proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().map_err(|e| format!("启动 {display} 失败: {e}"))?;
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

/// CodeBuddy 进程检测：同时覆盖通用形态 CodeBuddy 与本机实测的 "CodeBuddy CN"。
/// Windows：sysinfo 进程表一次枚举（main 合并，原逐名 tasklist 子进程 ~520ms）；
/// 并存布局只认 "CodeBuddy CN"（W8：映像名 "CodeBuddy" 被国际版与国内老单安装共用，
/// 并存时国际进程在跑会让国内环境检测误报 running=true）；
/// mac：按 bundle 主进程路径段匹配（M-1 ⑥），收窄为仅 "CodeBuddy CN"（2026-10-19
/// 双客户端并存实测，对齐 W8 与 switcher/proc.rs #1：mac 国内 bundle 目录名恒为
/// CodeBuddy CN.app，裸名 "CodeBuddy" 段恰是并存国际版 CodeBuddy.app，双形态
/// 兼容会让国内环境检测误报 running=true）
fn is_running_codebuddy() -> bool {
    #[cfg(windows)]
    {
        if codebuddy_cn_installed() {
            crate::switcher::proc::any_running(&["CodeBuddy CN"])
        } else {
            crate::switcher::proc::any_running(&["CodeBuddy CN", "CodeBuddy"])
        }
    }
    #[cfg(target_os = "macos")]
    {
        // M-1 ⑥：按 bundle 主进程路径段匹配；2026-10-19 实测收窄为仅 CN（同上）
        crate::commands::process::mac_app_running(&["CodeBuddy CN"])
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// CN 拆分形态安装判定（exe 候选首位同款路径）：用于区分共用映像名 "CodeBuddy"
/// 的归属——在盘 = 并存布局，国际进程属国际版；不在盘 = 老单布局，归国内。
#[cfg(windows)]
fn codebuddy_cn_installed() -> bool {
    std::env::var("LOCALAPPDATA")
        .map(|l| {
            std::path::Path::new(&l)
                .join("Programs")
                .join("CodeBuddy CN")
                .join("CodeBuddy CN.exe")
                .is_file()
        })
        .unwrap_or(false)
}

/// 兜底两级（注册表搜索 / 进程反查）结果短 TTL 缓存（P1 去重）。
/// 一轮界面切换里概览页与顶栏会对同一应用各探测一次（CodeBuddy 本机默认路径不存在，
/// 每轮都会走 `reg query`，实测 ~250ms/次），命中缓存直接复用上一轮结果。
/// 只缓存兜底两级：手动指定与默认路径仍实时判定，用户刚改设置需立即生效。
/// 仅 Windows 探测链消费（mac 走 bundle 定位链，无注册表/进程反查兜底）。
#[cfg(not(target_os = "macos"))]
const LOCATE_FALLBACK_TTL: Duration = Duration::from_secs(3);

#[cfg(not(target_os = "macos"))]
fn locate_fallback_cached<F>(app: &str, level: &str, f: F) -> Option<String>
where
    F: FnOnce() -> Option<String>,
{
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Option<String>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{app}:{level}");
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        // 「未找到」同样缓存：避免同轮切换里重复跑一次注定失败的注册表全量搜索
        if let Some((at, v)) = guard.get(&key) {
            if at.elapsed() < LOCATE_FALLBACK_TTL {
                return v.clone();
            }
        }
    }
    let v = f();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    // 防御：键集合本应有界（应用数 × 两级），异常膨胀时整体清空重来
    if guard.len() > 64 {
        guard.clear();
    }
    guard.insert(key, (Instant::now(), v.clone()));
    v
}

/// api_server 指纹解析复用：读 settings 自定义路径（键与 app_locate 同源，
/// 仅读 kv 不依赖 AppState；无自定义或路径无效返回 None）
#[cfg(not(target_os = "macos"))]
pub(crate) fn custom_path_kv(data_dir: &std::path::Path, app: &str) -> Option<String> {
    let profile = app_profile(Some(app));
    let key = profile.settings_key?;
    let settings: serde_json::Value = crate::store::db(data_dir).kv_get("app_settings");
    let p = settings.get(key)?.as_str()?.trim().to_string();
    (!p.is_empty() && std::path::Path::new(&p).is_file()).then_some(p)
}

/// api_server 指纹解析复用：注册表卸载键解析（复用 app_locate 的 registry 源）
#[cfg(not(target_os = "macos"))]
pub(crate) fn registry_app_path_for(app: &str) -> Option<String> {
    registry_app_path(&app_profile(Some(app)))
}

/// mac 变体：settings 自定义路径为 .app bundle 目录形态（非文件），校验口径
/// 与 app_locate_macos 的 settings 分支同源（is_bundle_dir）；键名表与
/// app_locate_macos 档案表一致
#[cfg(target_os = "macos")]
pub(crate) fn custom_path_kv(data_dir: &std::path::Path, app: &str) -> Option<String> {
    use crate::switcher::locate::is_bundle_dir;
    let key = match app.to_lowercase().as_str() {
        "trae" | "trae_cn" | "traecn" | "ide" => "trae_cn_path",
        "doubao" => "doubao_path",
        "workbuddy" => "workbuddy_path",
        "codebuddy" => "codebuddy_path",
        "workbuddy_ai" => "workbuddy_ai_path",
        "codebuddy_ai" => "codebuddy_ai_path",
        _ => return None,
    };
    let settings: serde_json::Value = crate::store::db(data_dir).kv_get("app_settings");
    let p = settings.get(key)?.as_str()?.trim().to_string();
    (!p.is_empty() && is_bundle_dir(std::path::Path::new(&p))).then_some(p)
}

/// mac 变体：无注册表概念，恒 None——bundle 探测/Spotlight 兜底由调用方
///（api_server::wb_upstream::locate_client_exe mac 分支）走 bundle 定位链。
/// 零调用方依据：唯一调用点位于 `#[cfg(not(macos))]` 块内，mac 构建不编译该
/// 调用（同 pe_version 桩口径），保留本函数维持双平台对称 API
#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub(crate) fn registry_app_path_for(_app: &str) -> Option<String> {
    None
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
    };

    let key = app.to_lowercase();
    let home = std::env::var("HOME").unwrap_or_default();
    // (显示名, 主进程名/bundle 目录名候选, CFBundleIdentifier 白名单, mac 数据目录预填, settings 键)
    // bundle_ids 必填（M-1 ⑥：CFBundleExecutable 均为 "Electron"，单靠 exe 名白名单恒 miss）；
    // 值取自 profile.rs mac_bundle_ids 侦察表（R2 实测）
    let (display, proc_names, mac_bundle_ids, user_data_dir, settings_key): (
        &str,
        &[&str],
        &[&str],
        String,
        Option<&str>,
    ) = match key.as_str() {
        "trae" | "trae_cn" | "traecn" | "ide" => (
            "Trae",
            &["Trae CN"],
            &["cn.trae.app"],
            format!("{home}/Library/Application Support/Trae CN"),
            Some("trae_cn_path"),
        ),
        "doubao" => (
            "豆包",
            &["Doubao"],
            &["com.bot.pc.doubao"],
            format!("{home}/Library/Application Support/Doubao"),
            Some("doubao_path"),
        ),
        "workbuddy" => (
            "WorkBuddy",
            &["WorkBuddy"],
            &["com.tencent.workbuddy.mac"],
            format!("{home}/.workbuddy"),
            Some("workbuddy_path"),
        ),
        "codebuddy" => (
            "CodeBuddy",
            &["CodeBuddy", "CodeBuddy CN"],
            &["com.tencent.codebuddycn"],
            format!("{home}/.codebuddy"),
            Some("codebuddy_path"),
        ),
        // H-1 修复：BuddyAI 国际版双档案（此前漏配命中 `_` 兜底 → mac 上误定位成
        // Trae 并直开 Trae）。数据目录对齐 switcher/profile.rs（~/.workbuddy-ai 国际
        // 独立根；~/.codebuddy 与国内双端共享）。CFBundleIdentifier 2026-10-19 真机
        // 实测回填（原按国内命名惯例推断的 id 与实测不符，推断未校准时表现为
        // fail-safe「未找到」；现随 profile.rs 两国际档案 mac_supported 放开同步校正）：
        // WorkBuddy AI.app = com.workbuddy.workbuddy-ai；CodeBuddy.app = com.tencent.codebuddy
        "workbuddy_ai" => (
            "WorkBuddy AI",
            &["WorkBuddyAI", "WorkBuddy AI"],
            &["com.workbuddy.workbuddy-ai"],
            format!("{home}/.workbuddy-ai"),
            Some("workbuddy_ai_path"),
        ),
        "codebuddy_ai" => (
            "CodeBuddy AI",
            &["CodeBuddyAI", "CodeBuddy AI", "CodeBuddy"],
            &["com.tencent.codebuddy"],
            format!("{home}/.codebuddy"),
            Some("codebuddy_ai_path"),
        ),
        // trae_work / traework / work / solo 及其它值 → 默认 Trae（对齐 main 双应用合并：
        // Windows 侧同款兜底；Trae Work 档案与 trae_path 设置键已随合并退役）
        _ => (
            "Trae",
            &["Trae CN"],
            &["cn.trae.app"],
            format!("{home}/Library/Application Support/Trae CN"),
            Some("trae_cn_path"),
        ),
    };

    // 1) 用户手动指定（settings）：mac 形态为 .app 目录（非文件），is_file 校验不适用
    if let Some(sk) = settings_key {
        let settings = state.settings();
        let custom = match sk {
            "trae_cn_path" => settings.trae_cn_path,
            "doubao_path" => settings.doubao_path,
            "workbuddy_path" => settings.workbuddy_path,
            "codebuddy_path" => settings.codebuddy_path,
            "workbuddy_ai_path" => settings.workbuddy_ai_path,
            "codebuddy_ai_path" => settings.codebuddy_ai_path,
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
        if let Some(found) = locate_bundle_dir(name, proc_names, mac_bundle_ids) {
            return finish_locate_macos(display, &user_data_dir, found, "default");
        }
    }
    // 3) Spotlight 兜底（mdfind 秒回；仅上面未命中时）
    for name in &names {
        if let Some(found) = mdfind_bundle(name, proc_names, mac_bundle_ids) {
            return finish_locate_macos(display, &user_data_dir, found, "default");
        }
    }

    // 4) 运行中进程回退：sysinfo exe 路径归一回 .app 根（exe → MacOS → Contents
    //    → <App>.app，ancestors().nth(3)）后经双信号白名单防串台
    for p in crate::commands::process::mac_exe_paths() {
        if let Some(app_dir) = p.ancestors().nth(3) {
            if is_bundle_dir(app_dir) && bundle_exe_matches(app_dir, proc_names, mac_bundle_ids) {
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
        app: app_slug(display),
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
            "trae_cn_path" => settings.trae_cn_path,
            "doubao_path" => settings.doubao_path,
            "workbuddy_path" => settings.workbuddy_path,
            "codebuddy_path" => settings.codebuddy_path,
            "workbuddy_ai_path" => settings.workbuddy_ai_path,
            "codebuddy_ai_path" => settings.codebuddy_ai_path,
            _ => None,
        };
        if let Some(p) = custom {
            let p = p.trim().to_string();
            if !p.is_empty() && std::path::Path::new(&p).is_file() {
                return finish_locate(&profile, p, "settings", None);
            }
        }
    }
    // 探测顺序对齐全库原有语义（旧 detect_trae/detect_trae_cn 与切换器 locate.rs
    // 均为默认路径优先、注册表兜底）：多安装共存时优先取官方默认位置的最新安装，
    // 注册表残留的过期条目只作兜底，避免选到已卸载/迁移的旧路径
    for c in profile.exe_candidates {
        let expanded = c
            .replace("%LOCALAPPDATA%", &std::env::var("LOCALAPPDATA").unwrap_or_default())
            .replace("%ProgramFiles%", &std::env::var("ProgramFiles").unwrap_or_default());
        if std::path::Path::new(&expanded).is_file() {
            return finish_locate(&profile, expanded, "default", None);
        }
    }
    if let Some(exe) = locate_fallback_cached(app, "registry", || registry_app_path(&profile)) {
        return finish_locate(&profile, exe, "registry", None);
    }
    if let Some(exe) = locate_fallback_cached(app, "process", || process_exe_path(profile.proc_names))
    {
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
    app_locate_inner(&state, target_app.as_deref().unwrap_or("trae"))
}

/// AppLocate.app 值域统一（合并审查 #7）：display → 英文 slug，双平台一致。
/// 唯一非 ASCII display 是豆包「豆包」（to_lowercase 不变 → 原产出中文），
/// 特判为 "doubao"；其余走小写 + 空格转下划线。前端不消费 .app 字段（仅
/// version/exe/userDataDir/source），值域变化安全；not_found 分支返回原始
/// 参数的语义保持不变。
fn app_slug(display: &str) -> String {
    match display {
        "豆包" => "doubao".to_string(),
        d => d.to_lowercase().replace(' ', "_"),
    }
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
        app: app_slug(&profile.display),
        exe: Some(exe),
        user_data_dir: profile.user_data_dir.clone(),
        version,
        source: source.into(),
    }
}

/// 注册表卸载键搜索（按应用档案的 DisplayName 片段与 exe 名参数化）。
/// windows-registry 进程内枚举（HKCU + HKLM 64 位视图，与原 `reg query /s /f`
/// 语义对齐）：DisplayName 含模式串 → DisplayIcon / InstallLocation 解析。
/// 不再 spawn reg.exe 子进程（issue #137：开机自启后登录早期 spawn 的 reg.exe
/// 撞系统初始化窗口期报 0xc0000142），同时消除 /s /f 全量搜索 ~250ms/次开销；
/// 同键直读三个值，原「/f 输出不全再回查全值」的两段式不再需要。
/// Windows 注册表通道专用（mac 无注册表；调用方 app_locate 链同门控）
#[cfg(not(target_os = "macos"))]
fn registry_app_path(profile: &AppProfile) -> Option<String> {
    use windows_registry::{CURRENT_USER, LOCAL_MACHINE};
    let uninstall_sub = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
    for pattern in profile.reg_patterns {
        let pat_upper = pattern.to_uppercase();
        for root in [CURRENT_USER, LOCAL_MACHINE] {
            let Ok(uninstall) = root.open(uninstall_sub) else {
                continue;
            };
            let Ok(subkeys) = uninstall.keys() else {
                continue;
            };
            for key in subkeys {
                let Ok(item) = uninstall.open(&key) else {
                    continue;
                };
                // 原实现仅接受 DisplayName 数据含模式串的键（/f 命中行人工过滤）
                let Ok(display) = item.get_string("DisplayName") else {
                    continue;
                };
                if !display.to_uppercase().contains(&pat_upper) {
                    continue;
                }
                let icon = item.get_string("DisplayIcon").ok();
                let loc = item.get_string("InstallLocation").ok();
                if let Some(hit) = resolve_reg_profile_candidate(&icon, &loc, profile) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

/// 从注册表 DisplayIcon / InstallLocation 推导 exe 路径（按档案 exe 名匹配）
#[cfg(not(target_os = "macos"))] // 唯一调用方 registry_app_path 为 Windows 分支；AppProfile 亦非 mac 编译
fn resolve_reg_profile_candidate(
    icon: &Option<String>,
    loc: &Option<String>,
    profile: &AppProfile,
) -> Option<String> {
    if let Some(icon) = icon {
        // DisplayIcon 可能带 ",0" 图标索引后缀
        let clean = icon.split(',').next().unwrap_or(icon).trim().to_string();
        // icon 文件名须精确命中档案白名单：国内档案 reg_patterns 含 "WorkBuddy"
        // 会 contains 匹配国际键 "WorkBuddyAI (User)"，无白名单则其 DisplayIcon
        // （WorkBuddyAI.exe）被国内档案误收，注册表级串台
        let fname_ok = std::path::Path::new(&clean)
            .file_name()
            .and_then(|f| f.to_str())
            .map(|f| {
                let f = f.to_lowercase();
                profile.reg_exe_names.iter().any(|n| n.to_lowercase() == f)
            })
            .unwrap_or(false);
        if fname_ok
            && clean.to_lowercase().ends_with(".exe")
            && std::path::Path::new(&clean).is_file()
        {
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

/// 运行进程反查 exe 路径（应用运行中时最准）：精确映像名匹配，取首个带路径的进程。
/// 原 powershell 实现（单次 1.2s+）改 sysinfo 直读（`switcher::proc::running_exe_exact`，
/// main 合并）；仅 Windows 探测链消费（mac 走 bundle 定位链的进程回退级）。
#[cfg(windows)]
fn process_exe_path(proc_names: &[&str]) -> Option<String> {
    let exe = crate::switcher::proc::running_exe_exact(proc_names)?;
    let s = exe.to_string_lossy().to_string();
    if s.is_empty() || !std::path::Path::new(&s).is_file() {
        None
    } else {
        Some(s)
    }
}
