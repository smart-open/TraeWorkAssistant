//! WorkBuddy 共享底层（原 workbuddy.rs 机械拆分）：路径常量、账号池/凭证库/设置读写、
//! 字段提取、脚本启动、续期互斥锁、会话 uid 防护、CLI 设置回滚等被多域复用的辅助。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;

// ── 路径常量（与 wb_common.py / trae-switch-bridge.ps1 保持一致）────────────

pub(super) fn auth_file_path() -> PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local)
        .join("CodeBuddyExtension")
        .join("Data")
        .join("Public")
        .join("auth")
        .join("workbuddy-desktop.info")
}

/// auth 文件读取路径：settings.wb_auth_file_path 人工指定优先（环境配置页），否则默认布局。
/// 仅作用于读取类路径（检测/列表/导入/续期/用量）；环境重置等清理动作仍针对客户端真实落盘位置。
pub(super) fn auth_file_path_of(state: &AppState) -> PathBuf {
    if let Some(p) = state.settings().wb_auth_file_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    auth_file_path()
}

pub(super) fn wb_data_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    PathBuf::from(home).join(".workbuddy")
}

pub(super) fn snapshot_json_path() -> PathBuf {
    wb_data_dir().join("storage").join("skeleton").join("account-snapshot.json")
}

fn pool_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_accounts.json")
}

pub(super) fn token_store_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_token_store.json")
}

fn settings_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_settings.json")
}

pub(super) fn checkin_results_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_checkin_results.json")
}

pub(super) fn account_id_of(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("wb-{}", &hex[..12])
}

// ── 数据结构 ────────────────────────────────────────────────────────────────

/// 账号池记录（§3.3 结构）
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct WorkBuddyAccount {
    /// wb-<sha256(token) 前 12 位>（同 token 稳定同 id，F-04）
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub phone_masked: String,
    #[serde(default)]
    pub edition_type: String,
    /// accessToken 过期时间（Unix 秒；刷新/导入时回填）
    #[serde(default)]
    pub access_token_expires_at: Option<i64>,
    #[serde(default)]
    pub refresh_token_expires_at: Option<i64>,
    #[serde(default)]
    pub auth_saved_at: Option<i64>,
    #[serde(default)]
    pub needs_relogin: bool,
    #[serde(default)]
    pub relogin_reason: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub note: String,
    /// 余额缓存（credits_fetch 成功后回写，供列表/概述展示）
    #[serde(default)]
    pub credits_balance: Option<f64>,
    #[serde(default)]
    pub credits_fetched_at: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub(super) struct WbPool {
    #[serde(default)]
    pub(super) accounts: Vec<WorkBuddyAccount>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct WorkBuddySettings {
    /// 启动自动补签（F-55）：启动时核验未签账号自动补签
    #[serde(default)]
    pub auto_checkin: bool,
    /// 保活阈值（天）；0 = 每天无条件刷新全部带 refreshToken 账号
    #[serde(default)]
    pub keepalive_days: i64,
    /// 惰性刷新（小时）：剩余有效期低于该值才刷新
    #[serde(default = "default_lazy_hours")]
    pub lazy_refresh_hours: i64,
    // 成长中心开关（F-17，批次2 消费；先落配置）
    #[serde(default = "default_true")]
    pub growth_travel: bool,
    #[serde(default = "default_true")]
    pub growth_lottery: bool,
    #[serde(default = "default_true")]
    pub growth_tasks: bool,
    // CLI 五重防护自动轮换（F-59，批次3 T3.4）
    #[serde(default)]
    pub cli_rotate_enabled: bool,
    /// 检查间隔（分钟，后台线程周期触发）
    #[serde(default = "default_cli_interval")]
    pub cli_rotate_interval_minutes: i64,
    /// ① 冷却期：切换后 N 分钟内不切
    #[serde(default = "default_cli_cooldown")]
    pub cli_cooldown_minutes: i64,
    /// ② 到期差异阈值：目标比当前早到期超过 N 小时才切（防横跳）
    #[serde(default = "default_cli_gap")]
    pub cli_min_gap_hours: i64,
    /// ③ 到期紧迫阈值：目标剩余超过 N 小时 = 都还早，不切
    #[serde(default = "default_cli_urgency")]
    pub cli_min_urgency_hours: i64,
    /// ④ 活跃保护：CLI 最近会话写入 N 分钟内不切
    #[serde(default = "default_cli_guard")]
    pub cli_active_guard_minutes: i64,
    /// ⑤ 最小剩余积分：目标低于该值不切（0 = 关闭）
    #[serde(default)]
    pub cli_min_remaining_credits: f64,
    // 失败通知渠道（F-19，批次3 T3.6）：桌面通知之外的可选渠道
    /// 企业微信群机器人 webhook（空 = 关闭）
    #[serde(default)]
    pub notify_wechat_webhook: Option<String>,
    /// Server酱 SendKey（空 = 关闭）
    #[serde(default)]
    pub notify_serverchan_sendkey: Option<String>,
    // UI 坐标点击签到兜底（F-18，批次4 T4.2）：仅手动触发，默认关闭
    #[serde(default)]
    pub ui_click_enabled: bool,
    /// 签到按钮屏幕坐标（0 = 未配置）
    #[serde(default)]
    pub ui_click_x: i64,
    #[serde(default)]
    pub ui_click_y: i64,
}

fn default_lazy_hours() -> i64 {
    24
}
fn default_true() -> bool {
    true
}
fn default_cli_interval() -> i64 {
    30
}
fn default_cli_cooldown() -> i64 {
    120
}
fn default_cli_gap() -> i64 {
    24
}
fn default_cli_urgency() -> i64 {
    72
}
fn default_cli_guard() -> i64 {
    30
}

impl WorkBuddySettings {
    fn with_defaults() -> Self {
        Self {
            auto_checkin: false,
            keepalive_days: 0,
            lazy_refresh_hours: 24,
            growth_travel: true,
            growth_lottery: true,
            growth_tasks: true,
            cli_rotate_enabled: false,
            cli_rotate_interval_minutes: 30,
            cli_cooldown_minutes: 120,
            cli_min_gap_hours: 24,
            cli_min_urgency_hours: 72,
            cli_active_guard_minutes: 30,
            cli_min_remaining_credits: 0.0,
            notify_wechat_webhook: None,
            notify_serverchan_sendkey: None,
            ui_click_enabled: false,
            ui_click_x: 0,
            ui_click_y: 0,
        }
    }
}

// ── 工具函数 ────────────────────────────────────────────────────────────────

pub(super) fn load_pool(state: &AppState) -> WbPool {
    fs_utils::read_json(&pool_path(state))
}

pub(super) fn save_pool(state: &AppState, pool: &WbPool) -> Result<(), String> {
    fs_utils::write_json(&pool_path(state), pool)
}

pub(super) fn load_settings(state: &AppState) -> WorkBuddySettings {
    let mut s: WorkBuddySettings = fs_utils::read_json(&settings_path(state));
    // 审查 P2：单字段非法只钳制该字段为默认值，不再整体 with_defaults() 重置
    //（避免损坏一个字段连带丢掉轮换/通知等其余配置）
    if s.lazy_refresh_hours <= 0 {
        s.lazy_refresh_hours = WorkBuddySettings::with_defaults().lazy_refresh_hours;
    }
    s
}

/// 失败通知统一入口（F-19）：桌面通知（有 AppHandle 时）+ 企业微信/Server酱可选渠道。
/// 渠道配置来自 workbuddy_settings.json；渠道失败静默记日志，不影响主流程。
pub fn push_notify(app: Option<&AppHandle>, data_dir: &std::path::Path, title: &str, body: &str) {
    let s: WorkBuddySettings = fs_utils::read_json(&data_dir.join("data").join("workbuddy_settings.json"));
    let channels = crate::notify::NotifyChannels {
        wechat_webhook: s.notify_wechat_webhook.clone().filter(|x| !x.trim().is_empty()),
        serverchan_sendkey: s.notify_serverchan_sendkey.clone().filter(|x| !x.trim().is_empty()),
    };
    crate::notify::notify_all(app, data_dir, title, body, &channels);
}

pub(super) fn is_running() -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq WorkBuddy.exe", "/NH"])
        .creation_flags(0x08000000)
        .output();
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("WorkBuddy.exe"))
}

// 宽容字段提取统一走 fs_utils::dig（含 data/result/resp/response/info/auth/account
// 包裹键下钻，与新版客户端 auth 文件嵌套结构 {account, auth} 兼容；审查 P2：消除本地重复实现漂移）。

pub(super) fn as_str(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

pub(super) fn as_ts_seconds(v: Option<&serde_json::Value>) -> Option<i64> {
    let raw = v?;
    if let Some(ms) = raw.as_i64() {
        // expiresAtMs 毫秒级（>1e12），统一折算秒
        return Some(if ms > 1_000_000_000_000 { ms / 1000 } else { ms });
    }
    if let Some(f) = raw.as_f64() {
        return Some(if f > 1_000_000_000_000.0 { (f / 1000.0) as i64 } else { f as i64 });
    }
    raw.as_str().and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp())
            .or_else(|| s.parse::<i64>().ok().map(|ms| if ms > 1_000_000_000_000 { ms / 1000 } else { ms }))
    })
}

// ── 设置（F-55 配置化）──────────────────────────────────────────────────────

#[tauri::command]
pub fn workbuddy_settings_get(state: State<AppState>) -> WorkBuddySettings {
    load_settings(&state)
}

#[tauri::command]
pub fn workbuddy_settings_set(state: State<AppState>, patch: WorkBuddySettings) -> Result<(), String> {
    fs_utils::write_json(&settings_path(&state), &patch)
}

// ── 工具侧凭证副本写入（F-10 双源化）───────────────────────────────────────

pub(super) fn upsert_token_store(state: &AppState, id: &str, creds: &serde_json::Value) -> Result<(), String> {
    let mut store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    if !store.is_object() {
        store = serde_json::json!({});
    }
    let obj = store.as_object_mut().unwrap();
    if obj.get("version").is_none() {
        obj.insert("version".into(), serde_json::json!(1));
    }
    let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    if let Some(t) = tokens.as_object_mut() {
        let mut rec = t.get(id).cloned().unwrap_or(serde_json::json!({}));
        if let Some(rm) = rec.as_object_mut() {
            for (k, v) in creds.as_object().unwrap_or(&serde_json::Map::new()) {
                if !v.is_null() {
                    rm.insert(k.clone(), v.clone());
                }
            }
            rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
        }
        t.insert(id.to_string(), rec);
    }
    fs_utils::write_json(&token_store_path(state), &store)
}

// ── M3 凭证续期互斥（F-09，Rust 侧手动触发；schtasks 每周兜底走 python --renew-only）──

/// 每账号续期互斥（审查 P1）：外层 std Mutex 只保护锁表本身（短临界区），
/// 内层 tokio Mutex 才是账号级互斥——try_lock 拿不到即拒绝，不排队不阻塞。
/// 锁表用 OnceLock 惰性初始化（HashMap::new 非 const，无法直接用于 static）。
pub(super) fn wb_renew_locks(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

// ── M4 python 脚本管线（NDJSON 事件）───────────────────────────────────────

/// 启动 python 脚本并把 stdout 逐行 emit 为 NDJSON 事件；done 行附带完成事件。
/// `round`：签到/成长全局轮次锁 guard，移入 stdout 工作线程持有至脚本退出。
pub(super) fn spawn_wb_script(
    app: AppHandle,
    state: &State<AppState>,
    script: &str,
    args: &[String],
    event: &str,
    round: tokio::sync::MutexGuard<'static, ()>,
) -> Result<(), String> {
    let script_path = state.python_dir.join(script);
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let mut cmd = Command::new(&state.python_exe);
    cmd.arg(&script_path)
        .args(args)
        .creation_flags(0x08000000)
        .env("AIWORKDATA_DIR", &state.data_dir)
        .env("PYTHONIOENCODING", "utf-8")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动脚本失败: {e}"))?;
    let stdout = child.stdout.take().ok_or("脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let ev = event.to_string();
    let data_dir = state.data_dir.clone();

    // stderr 独立线程读取（审查 P2，写法对齐 doubao.rs）：与 stdout 循环并行消费管道，
    // 防止 stderr 缓冲区写满使子进程阻塞、而父线程仍卡在等 stdout 的互锁死锁；
    // 同时落日志保留排查线索
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                fs_utils::app_log(&data_dir, &format!("[wb-script] {line}"));
            }
        });
    }

    // stdout 循环照旧；轮次锁 guard 在此线程持有至 wait() 返回（脚本退出），RAII 防泄漏
    std::thread::spawn(move || {
        let _round_guard = round;
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let l = line.trim().to_string();
            if l.is_empty() {
                continue;
            }
            // done 行同时发独立 done 事件（前端据 "type":"done" 归约即可，无需额外事件名）
            let _ = app2.emit(&ev, &l);
        }
        let status = child.wait();
        let _ = app2.emit(&ev, format!("{{\"type\":\"exit\",\"ok\":{}}}", status.map(|s| s.success()).unwrap_or(false)));
    });
    Ok(())
}

// ── CLI settings.json 回滚（审查 P2 原子写）────────────────────────────────

/// 回滚 settings.json 到写入前原文（审查 P2 原子写）：
/// JSON 内容走 fs_utils::write_json（临时文件 + rename，防断电/崩溃时文件处于半写损坏态）；
/// 原文非法 JSON（历史脏数据）或序列化路径失败时，退化为「临时文件 + rename」原文回写。
pub(super) fn restore_cli_settings(previous: &str) {
    let path = cli_settings_path();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(previous) {
        if fs_utils::write_json(&path, &v).is_ok() {
            return;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("json.rollback.{}.{}", std::process::id(), nanos));
    let wrote = std::fs::File::create(&tmp).and_then(|mut f| {
        use std::io::Write as _;
        f.write_all(previous.as_bytes())?;
        f.flush()
    });
    if wrote.is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// CodeBuddy CLI settings.json 路径（.codebuddy/settings.json）。
pub(super) fn cli_settings_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    PathBuf::from(home).join(".codebuddy").join("settings.json")
}

// ── M8 会话三件套命令的 uid 防护（审查 P0-1）───────────────────────────────

/// 会话三件套命令的 user_id 入参防护（审查 P0-1）：路径段只允许池内账号 id，
/// 且必须匹配白名单字符集——杜绝 `..`/绝对路径/分隔符注入导致的目录逃逸
/// （backup 对该路径有 remove_dir_all，逃逸即任意目录删除，restore/info 可读任意路径）。
pub(super) fn wb_chat_uid_guard(state: &AppState, user_id: &str) -> Result<(), String> {
    if user_id.is_empty()
        || user_id.len() > 64
        || !user_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || user_id.contains("..")
    {
        return Err(format!("非法账号标识: {user_id}"));
    }
    let pool = load_pool(state);
    if !pool.accounts.iter().any(|a| a.id == user_id) {
        return Err(format!("账号不在池中: {user_id}"));
    }
    Ok(())
}
