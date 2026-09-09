use std::path::PathBuf;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;

use crate::fs_utils;
use crate::models::Settings;

/// 应用数据目录名（品牌 ai-work-assistant）
pub const DATA_DIR_NAME: &str = "AIWorkAssistant";
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
fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf, exclude_dirs: &[&str]) -> Result<usize, String> {
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

/// 应用全局状态。base_dir 指向 %APPDATA%\AIWorkAssistant；
/// 子目录: conf/ (配置), data/ (数据), logs/ (日志)
/// python_dir 指向打包后的 python 脚本目录（Tauri resource `python/`）。
pub struct AppState {
    pub data_dir: PathBuf,
    pub python_dir: PathBuf,
    pub python_exe: String,
    /// JWT 刷新锁：防止多个并发请求同时 ExchangeToken
    pub jwt_refresh_lock: Mutex<()>,
}

/// 配置文件名列表（路由到 conf/ 目录）
const CONF_FILES: &[&str] = &["app_settings.json"];

impl AppState {
    pub fn new() -> Result<Self, String> {
        // 数据目录：%APPDATA%\AIWorkAssistant，不存在则创建
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .map_err(|_| "无法读取 APPDATA 环境变量".to_string())?;
        let data_dir = appdata.join(DATA_DIR_NAME);
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("创建数据目录失败: {e}"))?;

        // 创建子目录结构
        let conf_dir = data_dir.join("conf");
        let data_subdir = data_dir.join("data");
        let logs_dir = data_dir.join("logs");
        let _ = std::fs::create_dir_all(&conf_dir);
        let _ = std::fs::create_dir_all(&data_subdir);
        let _ = std::fs::create_dir_all(&logs_dir);

        // python 脚本目录：优先取 Tauri 资源目录下的 python/，否则回退到源码目录
        let python_dir = resolve_python_dir();

        // python 解释器：资源目录内嵌的 python.exe 优先（须能自举导入标准库）；否则探测系统可用解释器。
        // Windows 官方安装通常提供 python.exe / py.exe，python3 反而常不存在，故依次探测。
        let embedded = python_dir.join("python.exe");
        let python_exe = if embedded.exists() && python_can_bootstrap(&embedded.to_string_lossy()) {
            embedded.to_string_lossy().to_string()
        } else {
            probe_python_exe()
        };

        Ok(Self {
            data_dir,
            python_dir,
            python_exe,
            jwt_refresh_lock: Mutex::new(()),
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
        // 统一走 fs_utils::read_json：文件缺失/为空/解析失败均回退默认，行为一致
        let mut s: Settings = fs_utils::read_json(&self.conf_path("app_settings.json"));
        // proxy_domains 为空时回填默认值，确保设置页始终展示默认监听域名；
        // 仍为旧默认（未含 doubao.com）时也迁移到新默认（用户未自定义过才替换）
        if s.proxy_domains.trim().is_empty()
            || s.proxy_domains == crate::models::legacy_proxy_domains()
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

/// 定位 python 脚本目录：覆盖安装版(MSI/NSIS)、便携版(zip 直接运行)、开发期三种布局。
fn resolve_python_dir() -> PathBuf {
    // 1) 运行期 Tauri 注入的资源目录：<RESOURCE_DIR>/python
    if let Ok(res) = std::env::var("TAURI_RESOURCE_DIR") {
        let p = PathBuf::from(res).join("python");
        if p.exists() {
            return p;
        }
    }
    // 2) 可执行文件周边布局（便携版 sidecar / 安装版均可能命中）
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // dir/resources/python
            let c1 = dir.join("resources").join("python");
            if c1.exists() {
                return c1;
            }
            // dir/python（少数打包方式把 python 放 exe 同级）
            let c2 = dir.join("python");
            if c2.exists() {
                return c2;
            }
            // 上层再找 resources/python（如 exe 在 "<App>/ai-work-assistant.exe" 嵌套一层）
            if let Some(parent) = dir.parent() {
                let c3 = parent.join("resources").join("python");
                if c3.exists() {
                    return c3;
                }
            }
        }
    }
    // 3) 开发期：仓库 src-python
    PathBuf::from("src-python")
}

/// 定位 ps 脚本目录：与 python 目录同级，覆盖安装版/便携版/开发期。
pub fn resolve_ps_dir() -> PathBuf {
    let py = resolve_python_dir();
    // 安装/便携版：python 与 ps 都在 <RESOURCE_DIR> 下，故用 python 的父目录
    if let Some(parent) = py.parent() {
        let ps = parent.join("ps");
        if ps.exists() {
            return ps;
        }
    }
    // 开发期：python 在 src-python，ps 在 src-ps
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join("resources").join("ps");
            if c.exists() {
                return c;
            }
        }
    }
    PathBuf::from("src-ps")
}

/// 验证解释器能否自举（成功导入标准库 encodings）。
/// 注意 `--version` 不触发 stdlib 导入，残缺运行时（如缺 Lib/encodings 的内嵌 Python，
/// 报 "Fatal Python error: Failed to import encodings module"）也能通过，故必须用 import 探测。
fn python_can_bootstrap(exe: &str) -> bool {
    Command::new(exe)
        .args(["-c", "import encodings"])
        .creation_flags(0x08000000)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 探测系统可用的 Python 解释器，依次尝试 python / python3 / py。
/// 均不可用时兜底返回 "python3"（保持原行为，由上层在启动时报错提示）。
fn probe_python_exe() -> String {
    for cand in ["python", "python3", "py"] {
        if python_can_bootstrap(cand) {
            return cand.to_string();
        }
    }
    "python3".to_string()
}
