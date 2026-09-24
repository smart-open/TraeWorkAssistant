use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::models::Settings;

/// 应用数据目录名（品牌 ai-work-assistant）
pub const DATA_DIR_NAME: &str = "AIWorkAssistant";
/// 旧版数据目录名（品牌迁移前为 Trae Work Assistant），启动时自动迁移到新版目录
pub const LEGACY_DATA_DIR_NAME: &str = "TraeWorkAssistant";
/// 旧版 bundle identifier（品牌迁移前），其 WebView2 数据目录同样需要迁移（仅 Windows）
#[cfg(windows)]
pub const LEGACY_IDENTIFIER: &str = "com.traework.assistant";
/// 新版 bundle identifier（与 LEGACY 同为 WebView2 目录迁移专用，仅 Windows 消费）
#[cfg(windows)]
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

    // 1) 数据目录迁移（复制语义；mac 无旧版数据，legacy.is_dir() 恒 false 天然跳过）
    if let Ok(appdata) = crate::platform::app_support_root() {
        let legacy = appdata.join(LEGACY_DATA_DIR_NAME);
        let new_dir = appdata.join(DATA_DIR_NAME);
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
    //    F-75 M0-0.3：mac 无 WebView2 用户数据目录概念（WKWebView 数据由系统按
    //    bundle identifier 管理随 app 删除清除），整段 cfg(windows) 门控
    #[cfg(windows)]
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
#[cfg(windows)]
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
        // 数据目录：%APPDATA%\AIWorkAssistant（macOS: ~/Library/Application Support/AIWorkAssistant，
        // F-75 M0-0.3），不存在则创建
        let data_dir = crate::platform::app_support_root()?.join(DATA_DIR_NAME);
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("创建数据目录失败: {e}"))?;

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
        // Trae JWT 续期调度（issue #27）：kv 完全缺失走 Settings::default()（bool=false/
        // String=""）会漏开——时刻为空视为从未配置，统一回填默认开 + 09:00；
        // 用户显式关闭后 hhmm 保持非空，不会被此处重新打开
        if s.jwt_renew_hhmm.trim().is_empty() {
            s.jwt_renew_enabled = true;
            s.jwt_renew_hhmm = "09:00".into();
        }
        // WorkBuddy 每日成长任务（任务配置页）：同款零值回填（默认开 + 09:00）
        if s.wb_growth_hhmm.trim().is_empty() {
            s.wb_growth_enabled = true;
            s.wb_growth_hhmm = "09:00".into();
        }
        // WorkBuddy 每日签到时刻零值回填（默认 09:10；开关 auto_checkin 在 workbuddy_settings 不动）
        if s.wb_checkin_hhmm.trim().is_empty() {
            s.wb_checkin_hhmm = "09:10".into();
        }
        // Trae 每日签到时刻零值回填（默认 09:00；签到恒开无独立开关）
        if s.trae_checkin_hhmm.trim().is_empty() {
            s.trae_checkin_hhmm = "09:00".into();
        }
        // 模型目录每日同步（Buddy/Trae）：同款零值回填（默认开）；同步幂等且不消耗积分，
        // 无账号时调度器静默跳过，默认开安全
        if s.wb_catalog_sync_hhmm.trim().is_empty() {
            s.wb_catalog_sync_enabled = true;
            s.wb_catalog_sync_hhmm = "05:45".into();
        }
        if s.trae_models_sync_hhmm.trim().is_empty() {
            s.trae_models_sync_enabled = true;
            s.trae_models_sync_hhmm = "05:40".into();
        }
        // 看板数据同步模式零值归一：空/未知值视为默认 daily（时刻由调度器回退内置默认），
        // 显式 "off"/"hourly"/"daily" 原样生效
        if s.wb_credits_sync_mode.trim().is_empty() {
            s.wb_credits_sync_mode = "daily".into();
        }
        if s.trae_credits_sync_mode.trim().is_empty() {
            s.trae_credits_sync_mode = "daily".into();
        }
        // 通知渠道迁移（F-19 → 系统设置页通知渠道面板，Trae/Buddy 共用）：
        // app_settings 渠道字段双 None 时从旧 workbuddy_settings 一次性搬运。
        // 命中即 kv_set 回写持久化——push_notify 裸读 kv 不经过本函数，仅内存视图会让
        // 推送链路读不到迁移值（老用户通知静默失效）；同时移除旧 kv 的通知键，
        // 区分「从未配置」与「用户显式清空」，防止清空后被迁移逻辑复活。
        if s.notify_webhook_url.is_none() && s.notify_serverchan_sendkey.is_none() {
            let db = crate::store::db(&self.data_dir);
            let raw = db.kv_get_raw("workbuddy_settings");
            let wb: serde_json::Value = raw
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok())
                .unwrap_or(serde_json::Value::Null);
            let legacy = |key: &str| {
                wb.get(key)
                    .and_then(|x| x.as_str())
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
            };
            let migrated_webhook = legacy("notify_wechat_webhook");
            let migrated_sendkey = legacy("notify_serverchan_sendkey");
            if migrated_webhook.is_some() || migrated_sendkey.is_some() {
                s.notify_webhook_url = migrated_webhook;
                s.notify_serverchan_sendkey = migrated_sendkey;
                // 写失败下次重试（与 load_models 回填同策略）；写成功后清掉旧键，迁移不可逆
                if db.kv_set("app_settings", &s).is_ok() {
                    if let Some(obj) = wb.as_object() {
                        let mut cleaned = obj.clone();
                        cleaned.remove("notify_wechat_webhook");
                        cleaned.remove("notify_serverchan_sendkey");
                        let _ = db.kv_set_raw("workbuddy_settings", &serde_json::to_string(&cleaned).unwrap_or_default());
                    }
                }
            }
        }
        s
    }
}

// （PS 桥 Rust 化后 resolve_ps_dir 与 tauri.conf.json resources 的 ps/ 资源一并移除——
// 切换/保存/备份/恢复/保活全链路由 switcher 模块进程内直调，无外部运行时依赖）

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 数据目录落app_support_root() {
        // F-75 M0-0.3：数据根目录不再直读 APPDATA 环境变量，锁定与 platform 基根一致
        let s = AppState::new().expect("AppState::new");
        let expected = crate::platform::app_support_root()
            .expect("app_support_root")
            .join(DATA_DIR_NAME);
        assert_eq!(s.data_dir, expected);
        // 子目录结构（conf/data/logs）在 new() 内已创建
        assert!(s.data_dir.join("conf").is_dir());
        assert!(s.data_dir.join("data").is_dir());
        assert!(s.data_dir.join("logs").is_dir());
    }

    fn tmp_state(tag: &str) -> AppState {
        let d = std::env::temp_dir().join(format!("twa_state_notify_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        AppState { data_dir: d, jwt_refresh_lock: Arc::new(Mutex::new(())) }
    }

    /// 旧 workbuddy_settings 通知字段 → app Settings 一次性搬运：回写 app_settings 持久化 +
    /// 移除旧 kv 通知键（push_notify 裸读 kv，必须落盘才能读到迁移值）
    #[test]
    fn settings_migrates_notify_from_workbuddy_kv() {
        let st = tmp_state("migrate");
        crate::store::db(&st.data_dir)
            .kv_set_raw(
                "workbuddy_settings",
                r#"{"notify_wechat_webhook":"https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc","notify_serverchan_sendkey":"SCT123","other_field":1}"#,
            )
            .unwrap();
        let s = st.settings();
        assert_eq!(
            s.notify_webhook_url.as_deref(),
            Some("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc")
        );
        assert_eq!(s.notify_serverchan_sendkey.as_deref(), Some("SCT123"));
        // 落盘验证：kv app_settings 已含迁移值；旧 kv 通知键已移除（其余字段保留）
        let persisted: crate::models::Settings = crate::store::db(&st.data_dir).kv_get("app_settings");
        assert_eq!(persisted.notify_serverchan_sendkey.as_deref(), Some("SCT123"));
        let wb_raw = crate::store::db(&st.data_dir).kv_get_raw("workbuddy_settings").unwrap();
        assert!(!wb_raw.contains("notify_wechat_webhook"), "旧通知键应被移除: {wb_raw}");
        assert!(wb_raw.contains("other_field"), "其余字段保留: {wb_raw}");
        // 幂等：再次读取结果一致（迁移已持久化，不再重复触发）
        let s2 = st.settings();
        assert_eq!(s2.notify_webhook_url, s.notify_webhook_url);
        assert_eq!(s2.notify_serverchan_sendkey, s.notify_serverchan_sendkey);
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }

    /// 用户显式清空渠道（存 null → None）后不再被迁移复活（旧键已在迁移时移除）
    #[test]
    fn settings_notify_not_revived_after_explicit_clear() {
        let st = tmp_state("no_revive");
        let db = crate::store::db(&st.data_dir);
        db.kv_set_raw("workbuddy_settings", r#"{"notify_wechat_webhook":"https://old.example.com/hook"}"#).unwrap();
        let s = st.settings();
        assert_eq!(s.notify_webhook_url.as_deref(), Some("https://old.example.com/hook"), "首启迁移生效");
        // 模拟用户在面板清空渠道：app_settings 存 null（前端 `|| null` 语义）
        db.kv_set_raw(
            "app_settings",
            r#"{"notify_enabled":true,"notify_webhook_url":null,"notify_serverchan_sendkey":null}"#,
        )
        .unwrap();
        let s2 = st.settings();
        assert_eq!(s2.notify_webhook_url, None, "显式清空后不得复活");
        assert_eq!(s2.notify_serverchan_sendkey, None);
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }

    /// 面板已显式配置（非 None）时不被旧值覆盖
    #[test]
    fn settings_migration_skipped_when_panel_already_set() {
        let st = tmp_state("no_overwrite");
        let db = crate::store::db(&st.data_dir);
        db.kv_set_raw("app_settings", r#"{"notify_webhook_url":"https://new.example.com/hook"}"#).unwrap();
        db.kv_set_raw("workbuddy_settings", r#"{"notify_wechat_webhook":"https://old.example.com/hook"}"#).unwrap();
        let s = st.settings();
        assert_eq!(s.notify_webhook_url.as_deref(), Some("https://new.example.com/hook"));
        assert_eq!(s.notify_serverchan_sendkey, None, "webhook 已配置时整体跳过搬运");
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }

    /// 旧值为空白串视为未配置：不搬运，保持 None
    #[test]
    fn settings_migration_ignores_blank_legacy_values() {
        let st = tmp_state("blank");
        crate::store::db(&st.data_dir)
            .kv_set_raw("workbuddy_settings", r#"{"notify_wechat_webhook":"  ","notify_serverchan_sendkey":""}"#)
            .unwrap();
        let s = st.settings();
        assert_eq!(s.notify_webhook_url, None);
        assert_eq!(s.notify_serverchan_sendkey, None);
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }
}
