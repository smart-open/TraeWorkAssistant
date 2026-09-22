use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::models::Settings;

/// 应用数据目录名（品牌 ai-work-assistant）
pub const DATA_DIR_NAME: &str = "AIWorkAssistant";
/// 默认数据根目录（v1.0.0 起默认目录切换，AIWORK_DATA_DIR 可覆盖）：
/// Windows 下 `/data` 按进程**当前盘符**解析（如从 D:\code\AIWorkAssistant 启动则为 D:\data）；
/// Linux/macOS 为根级 /data；Docker 部署不受影响（compose 已显式注入 AIWORK_DATA_DIR=/app/data）
pub const DEFAULT_DATA_ROOT: &str = "/data";
/// 旧版数据目录名（品牌迁移前为 Trae Work Assistant），启动时自动迁移到新版目录
pub const LEGACY_DATA_DIR_NAME: &str = "TraeWorkAssistant";
/// 旧版 bundle identifier（品牌迁移前），其 WebView2 数据目录同样需要迁移
pub const LEGACY_IDENTIFIER: &str = "com.traework.assistant";
/// 新版 bundle identifier
pub const IDENTIFIER: &str = "com.aiwork.assistant";

/// 品牌迁移：老版本遗留目录**复制**到新目录（旧目录原地保留，老应用可继续使用，两版可并存）。
/// 覆盖两处：
/// 1. 数据目录 %APPDATA%\TraeWorkAssistant → %APPDATA%\AIWorkAssistant
/// 2. WebView2 用户数据目录 %LOCALAPPDATA%\com.traework.assistant → com.aiwork.assistant
///    （保存 localStorage 等界面偏好）
///
/// 策略：
/// - 目标目录已存在且有数据 → 视为「已迁移过」，跳过，绝不再次覆盖老应用数据；
/// - 否则递归复制旧目录 → 新目录（**复制而非移动**，旧目录保持完整）；
/// - 任一步失败静默跳过（下次启动重试），绝不影响本次启动。
/// 返回迁移结果说明（无迁移时为 None），供启动日志记录。
pub fn migrate_legacy_dirs() -> Option<String> {
    let mut notes: Vec<String> = Vec::new();

    // 1) 数据目录迁移（复制语义）
    if let Ok(appdata) = std::env::var("APPDATA") {
        let legacy = PathBuf::from(&appdata).join(LEGACY_DATA_DIR_NAME);
        let new_dir = PathBuf::from(&appdata).join(DATA_DIR_NAME);
        if legacy.is_dir() {
            if new_dir.is_dir() && dir_is_empty(&new_dir) == Some(false) {
                // 已迁移过：跳过（老应用数据原地保留，两版并存）
            } else {
                // 数据目录不排除任何子目录（完整迁移）；排除清单仅用于 WebView2 缓存
                match copy_dir_recursive(&legacy, &new_dir, &[]) {
                    Ok(n) => notes.push(format!(
                        "品牌迁移：数据目录已由 {} 复制迁移至 {}（{n} 个文件；旧目录原地保留，老应用可继续使用）",
                        legacy.display(),
                        new_dir.display()
                    )),
                    Err(e) => {
                        // 清理复制一半的半成品：避免下次启动误判「已迁移」而丢失部分文件
                        let _ = std::fs::remove_dir_all(&new_dir);
                        notes.push(format!(
                            "品牌迁移：数据目录复制迁移失败（{e}），已回滚半成品，下次启动重试；旧目录保留于 {}",
                            legacy.display()
                        ))
                    }
                }
            }
        }
    }

    // 2) WebView2 用户数据目录迁移（identifier 变更所致；复制语义，失败不影响启动）
    //    排除缓存类子目录（Cache/GPUCache 等）：体积可达 GB 级且常被运行中的老应用锁定，
    //    跳过后由新版本首次运行时自动重建，界面偏好等关键文件仍完整迁移
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let legacy = PathBuf::from(&local).join(LEGACY_IDENTIFIER);
        let new_dir = PathBuf::from(&local).join(IDENTIFIER);
        if legacy.is_dir() && dir_is_empty(&new_dir) != Some(false) {
            match copy_dir_recursive(&legacy, &new_dir, &WEBVIEW_CACHE_DIRS) {
                Ok(_) => notes.push("品牌迁移：WebView2 界面偏好目录已复制迁移（旧目录保留）".to_string()),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&new_dir);
                    notes.push(format!(
                        "品牌迁移：WebView2 目录复制失败（{e}），已回滚半成品，界面偏好将重置（旧目录保留）"
                    ))
                }
            }
        }
    }

    if notes.is_empty() { None } else { Some(notes.join("；")) }
}

/// 递归复制目录（复制而非移动：源目录保持完整，老应用可继续使用）。
/// 目标目录不存在则创建；同名文件直接覆盖（仅在首次迁移时发生）；
/// 遇到 exclude_dirs 中的目录名则整目录跳过（用于排除 WebView2 缓存）。
/// 返回复制的文件数。
pub(crate) fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf, exclude_dirs: &[&str]) -> Result<usize, String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目录 {} 失败: {e}", dst.display()))?;
    let mut copied = 0usize;
    let entries = std::fs::read_dir(src).map_err(|e| format!("读取 {} 失败: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("遍历 {} 失败: {e}", src.display()))?;
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        let ft = entry
            .file_type()
            .map_err(|e| format!("读取 {} 类型失败: {e}", sp.display()))?;
        if ft.is_dir() {
            // 排除缓存类目录：体积大、常被锁定、可由应用自动重建
            if exclude_dirs.contains(&entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            copied += copy_dir_recursive(&sp, &dp, exclude_dirs)?;
        } else if ft.is_file() {
            std::fs::copy(&sp, &dp)
                .map_err(|e| format!("复制 {} 失败: {e}", sp.display()))?;
            copied += 1;
        }
        // 符号链接等特殊类型跳过（Windows 数据目录中基本不存在）
    }
    Ok(copied)
}

/// WebView2 用户数据目录中可跳过的缓存类子目录（迁移时排除，运行时自动重建）
const WEBVIEW_CACHE_DIRS: [&str; 8] = [
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "ShaderCache",
    "Crashpad",
];

/// 目录是否为空：None 表示读取失败
fn dir_is_empty(dir: &PathBuf) -> Option<bool> {
    std::fs::read_dir(dir)
        .ok()
        .map(|mut entries| entries.next().is_none())
}

/// 旧默认数据根（品牌期：Windows %APPDATA% / macOS ~/Library/Application Support /
/// Linux $XDG_DATA_HOME 或 ~/.local/share）。v1.0.0 默认目录切换后仅作为一次性迁移源。
fn platform_data_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
            .unwrap_or_else(|_| PathBuf::from("."))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg);
            }
        }
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".local").join("share"))
            .unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// 桌面时代退役的遗留数据子目录（切换器 profiles 快照等，Web 版不再使用且体积可达 GB 级）——
/// 默认目录迁移时整目录排除，不搬进新目录（旧目录中原样保留）
const RETIRED_DESKTOP_DIRS: [&str; 4] = ["profiles", "profiles_trae", "profiles_codebuddy", "profiles_workbuddy"];

/// 默认数据目录切换（旧平台目录 → /data/AIWorkAssistant）的一次性迁移：
/// 旧目录有数据且目标为空/不存在时递归**复制**（旧目录原地保留，与品牌迁移同语义），
/// 排除桌面退役遗留子目录（RETIRED_DESKTOP_DIRS）；失败回滚半成品下次启动重试；
/// 显式设置 AIWORK_DATA_DIR 时完全不触发。
fn migrate_platform_data_to_default(target: &PathBuf) -> Option<String> {
    let legacy = platform_data_root().join(DATA_DIR_NAME);
    if !legacy.is_dir() || dir_is_empty(&legacy) != Some(false) {
        return None; // 无旧数据可迁
    }
    if target.is_dir() && dir_is_empty(target) == Some(false) {
        return None; // 已迁移过（或目标已有数据）
    }
    match copy_dir_recursive(&legacy, target, &RETIRED_DESKTOP_DIRS) {
        Ok(n) => Some(format!(
            "数据目录已切换至默认 {}：旧目录 {} 数据已复制迁移（{n} 个文件，旧目录原地保留）",
            target.display(),
            legacy.display()
        )),
        Err(e) => {
            // 清理复制一半的半成品：避免下次启动误判「已迁移」而丢文件
            let _ = std::fs::remove_dir_all(target);
            Some(format!(
                "数据目录切换迁移失败（{e}），已回滚半成品，下次启动重试；旧数据保留于 {}",
                legacy.display()
            ))
        }
    }
}

/// 应用全局状态。base_dir 指向 /data/AIWorkAssistant（AIWORK_DATA_DIR 可覆盖；
/// Windows 下 /data 按进程当前盘符解析）；子目录: conf/ (配置), data/ (数据), logs/ (日志)
/// Clone 支持把状态克隆进后台工作线程（签到/任务等直调场景）。
#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    /// JWT 刷新锁：防止多个并发请求同时 ExchangeToken（Arc 共享跨线程）
    pub jwt_refresh_lock: Arc<Mutex<()>>,
}

/// 配置文件名列表（路由到 conf/ 目录）
const CONF_FILES: &[&str] = &["app_settings.json"];

impl AppState {
    pub fn new() -> Result<Self, String> {
        // 数据目录（T3 可配置）：AIWORK_DATA_DIR 环境变量优先（server/Docker 部署）；
        // 未设置时默认 /data/AIWorkAssistant（v1.0.0 起），首启自动从旧平台目录一次性复制迁移
        let data_dir = match std::env::var("AIWORK_DATA_DIR") {
            Ok(v) if !v.trim().is_empty() => PathBuf::from(v.trim()),
            _ => {
                let dir = PathBuf::from(DEFAULT_DATA_ROOT).join(DATA_DIR_NAME);
                if let Some(note) = migrate_platform_data_to_default(&dir) {
                    eprintln!("aiwork-server: {note}");
                }
                dir
            }
        };
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            format!(
                "创建数据目录失败: {e}（目录 {} 不可写时可设置 AIWORK_DATA_DIR 指定其他位置）",
                data_dir.display()
            )
        })?;

        // 创建子目录结构
        let conf_dir = data_dir.join("conf");
        let data_subdir = data_dir.join("data");
        let logs_dir = data_dir.join("logs");
        let _ = std::fs::create_dir_all(&conf_dir);
        let _ = std::fs::create_dir_all(&data_subdir);
        let _ = std::fs::create_dir_all(&logs_dir);

        Ok(Self {
            data_dir,
            jwt_refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    /// 配置文件路径：base_dir/conf/name
    pub fn conf_path(&self, name: &str) -> PathBuf {
        let dir = self.data_dir.join("conf");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    /// 数据文件路径：base_dir/data/name
    pub fn data_path(&self, name: &str) -> PathBuf {
        let dir = self.data_dir.join("data");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    /// 日志目录路径：base_dir/logs
    pub fn logs_dir(&self) -> PathBuf {
        let dir = self.data_dir.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// 兼容性 path()：根据文件名自动路由到正确的子目录
    /// - 配置文件 → conf/
    /// - 数据文件 → data/
    /// - logs → logs/（日志根目录）
    /// - 其他 → data/（默认）
    pub fn path(&self, name: &str) -> PathBuf {
        if CONF_FILES.contains(&name) {
            self.conf_path(name)
        } else if name == "logs" {
            // 日志根目录：base_dir/logs
            self.logs_dir()
        } else if name == "profiles" || name == "certs" {
            // 数据子目录
            let dir = self.data_path(name);
            let _ = std::fs::create_dir_all(&dir);
            dir
        } else {
            self.data_path(name)
        }
    }

    pub fn settings(&self) -> Settings {
        // SQLite 化（docs/sqllite-storage-plan.md P2）：kv 文档读取，
        // 缺失/为空/解析失败均回退默认（与原 fs_utils::read_json 语义一致）
        let mut s: Settings = crate::store::db(&self.data_dir).kv_get("app_settings");
        // proxy_domains 为空时回填默认值，确保设置页始终展示默认解密白名单；
        // 仍为历届旧默认时也迁移到新默认（用户未自定义过才替换）。新默认移出
        // doubao.com 宽后缀：豆包客户端 ttnet 原生栈对其证书锁定，解密会被拒
        // （页面空白）；zijieapi.com 实测可正常 MITM，保留（Trae 抓包需要）
        if s.proxy_domains.trim().is_empty()
            || s.proxy_domains == crate::models::legacy_proxy_domains()
            || s.proxy_domains == crate::models::legacy_proxy_domains_with_doubao()
            || s.proxy_domains == crate::models::legacy_proxy_domains_narrow()
        {
            s.proxy_domains = crate::models::default_proxy_domains();
        }
        // 豆包端点已实测固化，回填默认值；旧默认（doubao.com 首页）也迁移到新默认（用户未自定义才替换）
        if s.doubao_renew_url.clone().unwrap_or_default().trim().is_empty()
            || s.doubao_renew_url.clone().unwrap_or_default() == crate::models::legacy_doubao_renew_url()
        {
            s.doubao_renew_url = Some(crate::models::default_doubao_renew_url());
        }
        if s.doubao_quota_url.clone().unwrap_or_default().trim().is_empty() {
            s.doubao_quota_url = Some(crate::models::default_doubao_quota_url());
        }
        s
    }
}

// （PS 桥 Rust 化后 resolve_ps_dir 与 tauri.conf.json resources 的 ps/ 资源一并移除——
// 切换/保存/备份/恢复/保活全链路由 switcher 模块进程内直调，无外部运行时依赖）
