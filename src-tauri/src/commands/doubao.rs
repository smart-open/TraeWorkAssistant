//! 豆包应用账号池（P2）：doubao_accounts.json + 快照槽合并视图 + 当前登录账号探测。
//! 方案依据 doubao-trae-switch-plan.md §2.1（目录级快照）/ §2.3（账号池：保存 = 快照，
//! 账号元数据入 doubao_accounts.json，结构对齐 checkin_accounts.json 理念）。
//!
//! ⚠ serde 命名约定：全部 snake_case，与前端 types.ts 严格对齐；
//!   严禁 rename_all = "camelCase"（曾导致导入预览弹框前端必崩）。
//! P3（会话续期）将在 DoubaoAccount 上追加 sessionid/sid_guard 等字段（serde default，向后兼容）。

use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;

/// doubao_accounts.json 单条账号记录（P2 元数据 + P3 会话续期字段，均 serde default 向后兼容）
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct DoubaoAccount {
    /// 豆包 user_id（与快照槽目录名一致）
    pub user_id: String,
    /// 别名（展示名，默认 = user_id）
    #[serde(default)]
    pub name: String,
    /// 备注
    #[serde(default)]
    pub note: String,
    /// 入池时间（YYYY-MM-DD HH:MM:SS）
    #[serde(default)]
    pub added_at: String,
    /// 最近一次切换/保存登录态时间（展示用，PS 桥写 current_account.txt，这里由前端操作后回填）
    #[serde(default)]
    pub last_active_at: Option<String>,
    // ── P3 会话续期字段（由 doubao_renew.py 写回）──
    /// 明文 sessionid（凭证等同密码：仅存本地文件，前端全程掩码展示）
    #[serde(default)]
    pub session_id: Option<String>,
    /// sid_guard 原文（'sid|create_ts|duration|...'，滑动续期载体）
    #[serde(default)]
    pub sid_guard: Option<String>,
    /// 会话到期时间（由 sid_guard 解析）
    #[serde(default)]
    pub session_expire_at: Option<String>,
    /// 巡检判定：true=过期 / false=有效 / None=未知
    #[serde(default)]
    pub expired: Option<bool>,
    /// 最近一次 cookie 解密同步时间
    #[serde(default)]
    pub cookies_synced_at: Option<String>,
    /// 最近一次续期探活时间
    #[serde(default)]
    pub last_renew_at: Option<String>,
    /// 会话来源：live=当前 User Data / snapshot=快照槽解密
    #[serde(default)]
    pub session_source: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct DoubaoAccountPool {
    #[serde(default)]
    accounts: Vec<DoubaoAccount>,
    /// 最近一次 KeepAlive 保活时间（池级：保活由豆包客户端对当前登录会话统一滑动续期）
    #[serde(default)]
    pub last_keepalive_at: Option<String>,
}

/// 前端合并视图：快照槽（profiles_doubao/<uid>/）+ 账号池别名 + 当前账号标记
#[derive(Serialize, Clone)]
pub struct DoubaoAccountView {
    pub user_id: String,
    pub name: String,
    pub note: String,
    pub has_snapshot: bool,
    pub size_bytes: u64,
    pub file_count: u64,
    pub last_modified: String,
    pub is_current: bool,
    pub added_at: Option<String>,
    // ── P3 会话状态 ──
    /// ok=有效 / expired=已过期 / unknown=未探活 / none=无 sessionid
    pub session_state: String,
    pub session_expire_at: Option<String>,
    pub cookies_synced_at: Option<String>,
    pub last_renew_at: Option<String>,
    pub session_source: Option<String>,
    /// 池级：最近一次 KeepAlive 保活时间（所有行同值，供前端提醒判断）
    pub last_keepalive_at: Option<String>,
}

fn session_state_of(acc: &DoubaoAccount) -> String {
    match (&acc.session_id, acc.expired) {
        (None, _) => "none".to_string(),
        (Some(_), Some(true)) => "expired".to_string(),
        (Some(_), Some(false)) => "ok".to_string(),
        (Some(_), None) => "unknown".to_string(),
    }
}

fn pool_path(state: &State<AppState>) -> PathBuf {
    state.data_dir.join("data").join("doubao_accounts.json")
}

fn profiles_root(state: &State<AppState>) -> PathBuf {
    state.data_dir.join("data").join("profiles_doubao")
}

/// 读取账号池；文件不存在/损坏时返回空池（损坏仅记日志，避免一个坏文件锁死整页）
fn load_pool(state: &State<AppState>) -> DoubaoAccountPool {
    let path = pool_path(state);
    if !path.exists() {
        return DoubaoAccountPool::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<DoubaoAccountPool>(&raw) {
            Ok(pool) => pool,
            Err(e) => {
                fs_utils::app_log(&state.data_dir, &format!("doubao_accounts.json 解析失败（忽略）: {e}"));
                DoubaoAccountPool::default()
            }
        },
        Err(e) => {
            fs_utils::app_log(&state.data_dir, &format!("doubao_accounts.json 读取失败（忽略）: {e}"));
            DoubaoAccountPool::default()
        }
    }
}

fn save_pool(state: &State<AppState>, pool: &DoubaoAccountPool) -> Result<(), String> {
    let path = pool_path(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建账号池目录失败: {e}"))?;
    }
    // fs_utils::write_json：pid+纳秒 tmp 名防并发覆写冲突，内部 pretty 序列化
    fs_utils::write_json(&path, pool)
}

/// 读取当前账号（PS 桥 Set-CurrentAccount 写 profiles_doubao/current_account.txt，UTF8 带 BOM）
fn read_current_uid(state: &State<AppState>) -> Option<String> {
    let path = profiles_root(state).join("current_account.txt");
    let raw = std::fs::read_to_string(path).ok()?;
    let uid = raw.trim().trim_start_matches('\u{feff}').trim().to_string();
    if uid.is_empty() { None } else { Some(uid) }
}

/// 豆包账号合并视图：账号池 ∪ 快照槽目录，标注当前账号。
/// 账号池可含无快照的账号（如手动收录待登录）；快照槽也可含未入池账号（如 PS 桥自动备份的 last）。
#[tauri::command]
pub fn doubao_accounts_list(state: State<AppState>) -> Result<Vec<DoubaoAccountView>, String> {
    let pool = load_pool(&state);
    let current = read_current_uid(&state);
    let root = profiles_root(&state);
    let keepalive_at = pool.last_keepalive_at.clone();

    // 以账号池为基底
    let mut views: Vec<DoubaoAccountView> = pool
        .accounts
        .iter()
        .map(|a| DoubaoAccountView {
            user_id: a.user_id.clone(),
            name: if a.name.is_empty() { a.user_id.clone() } else { a.name.clone() },
            note: a.note.clone(),
            has_snapshot: false,
            size_bytes: 0,
            file_count: 0,
            last_modified: String::new(),
            is_current: current.as_deref() == Some(a.user_id.as_str()),
            added_at: if a.added_at.is_empty() { None } else { Some(a.added_at.clone()) },
            session_state: session_state_of(a),
            session_expire_at: a.session_expire_at.clone(),
            cookies_synced_at: a.cookies_synced_at.clone(),
            last_renew_at: a.last_renew_at.clone(),
            session_source: a.session_source.clone(),
            last_keepalive_at: keepalive_at.clone(),
        })
        .collect();

    // 并入快照槽（last 为 PS 桥安全备份槽，不在账号列表展示）
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let slot = entry.file_name().to_string_lossy().to_string();
            if slot == "last" {
                continue;
            }
            let (size_bytes, file_count) = crate::commands::profile::dir_stats(&entry.path());
            let last_modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| {
                    let dt = chrono::DateTime::<chrono::Local>::from(std::time::SystemTime::UNIX_EPOCH + d);
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_default();
            if let Some(v) = views.iter_mut().find(|v| v.user_id == slot) {
                v.has_snapshot = true;
                v.size_bytes = size_bytes;
                v.file_count = file_count;
                v.last_modified = last_modified;
            } else {
                views.push(DoubaoAccountView {
                    user_id: slot.clone(),
                    name: slot,
                    note: String::new(),
                    has_snapshot: true,
                    size_bytes,
                    file_count,
                    last_modified,
                    is_current: current.as_deref() == Some(entry.file_name().to_string_lossy().as_ref()),
                    added_at: None,
                    session_state: "none".to_string(),
                    session_expire_at: None,
                    cookies_synced_at: None,
                    last_renew_at: None,
                    session_source: None,
                    last_keepalive_at: keepalive_at.clone(),
                });
            }
        }
    }

    // 排序：当前账号 > 有快照 > 无快照；同级按最近活动倒序
    views.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then(b.has_snapshot.cmp(&a.has_snapshot))
            .then(b.last_modified.cmp(&a.last_modified))
    });
    Ok(views)
}

/// 收录/更新账号（upsert 别名与备注；不存在则入池）
#[tauri::command]
pub fn doubao_account_save(
    state: State<AppState>,
    user_id: String,
    name: Option<String>,
    note: Option<String>,
) -> Result<(), String> {
    let user_id = user_id.trim().to_string();
    if user_id.is_empty() {
        return Err("user_id 不能为空".to_string());
    }
    let mut pool = load_pool(&state);
    if let Some(acc) = pool.accounts.iter_mut().find(|a| a.user_id == user_id) {
        if let Some(n) = name {
            acc.name = n.trim().to_string();
        }
        if let Some(n) = note {
            acc.note = n.trim().to_string();
        }
    } else {
        pool.accounts.push(DoubaoAccount {
            name: name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| user_id.clone()),
            note: note.map(|n| n.trim().to_string()).unwrap_or_default(),
            user_id,
            added_at: fs_utils::now_ts(),
            last_active_at: None,
            session_id: None,
            sid_guard: None,
            session_expire_at: None,
            expired: None,
            cookies_synced_at: None,
            last_renew_at: None,
            session_source: None,
        });
    }
    save_pool(&state, &pool)
}

/// 从账号池移除（不动快照目录；快照删除走 profile_delete）
#[tauri::command]
pub fn doubao_account_remove(state: State<AppState>, user_id: String) -> Result<(), String> {
    let mut pool = load_pool(&state);
    let before = pool.accounts.len();
    pool.accounts.retain(|a| a.user_id != user_id);
    if pool.accounts.len() == before {
        return Ok(()); // 不存在视为已移除
    }
    save_pool(&state, &pool)
}

/// 探测豆包当前登录账号：%APPDATA%\Doubao\public_config.json 含 user_id 明文（plan §1.1）。
/// 结构无公开文档，dig 宽容解析（user_id / uid 键），找不到返回 None 由用户手动输入。
#[tauri::command]
pub fn doubao_detect_uid() -> Result<Option<String>, String> {
    let appdata = std::env::var("APPDATA").map_err(|e| format!("读取 APPDATA 环境变量失败: {e}"))?;
    let path = PathBuf::from(appdata)
        .join("Doubao")
        .join("public_config.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 public_config.json 失败: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("解析 public_config.json 失败: {e}"))?;
    for key in ["user_id", "uid", "user_id_str"] {
        if let Some(hit) = fs_utils::dig(&v, &[key]) {
            let s = match hit {
                serde_json::Value::String(s) => s.trim().to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => continue,
            };
            if !s.is_empty() {
                return Ok(Some(s));
            }
        }
    }
    Ok(None)
}

// ── P3 会话续期 ──────────────────────────────────────────────────────────────
//
// 实测结论（2026-09-08，本机 Doubao Chromium 147）：
// 桌面客户端的 cookie 值（sessionid/sid_guard 等）在 Chromium os_crypt 之下还有一层客户端级
// 加密——v10/DPAPI + AES-GCM 解出的明文仍为二进制密文（GCM tag 验证通过），无法离线得到
// 明文 sessionid。因此续期主路径为 KeepAlive（让豆包客户端自己联网滑动续期 sid_guard），
// 探活巡检仅对用户手动录入的 sessionid（高级功能）生效。

/// 运行 PS 桥 KeepAlive：启动豆包 → 等待会话联网刷新（25s）→ 优雅关闭（运行中则跳过）。
/// NDJSON 进度走 keepalive-progress / keepalive-done 事件，完成后记录池级保活时间戳。
#[tauri::command]
pub fn doubao_keepalive_run(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }
    let mut child = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &bridge.to_string_lossy(),
            "-Action",
            "KeepAlive",
            "-TargetApp",
            "Doubao",
            "-Json",
        ])
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动保活脚本失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("保活脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let stderr_dir = data_dir.clone();

    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        let mut done_emitted = false;
        for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
            let l = line.trim().to_string();
            if l.is_empty() {
                continue;
            }
            let _ = app2.emit("keepalive-progress", &l);
            if l.contains("\"stage\":\"done\"") || l.contains("\"stage\":\"fatal\"") {
                let success = l.contains("\"stage\":\"done\"");
                done_emitted = true;
                let _ = app2.emit("keepalive-done", serde_json::json!({ "success": success, "raw": l }));
                if success {
                    // 记录池级保活时间戳（写入失败不影响保活结果）
                    let pool_path = data_dir.join("data").join("doubao_accounts.json");
                    if let Ok(raw) = std::fs::read_to_string(&pool_path) {
                        if let Ok(mut pool) = serde_json::from_str::<DoubaoAccountPool>(&raw) {
                            pool.last_keepalive_at = Some(fs_utils::now_ts());
                            let _ = fs_utils::write_json(&pool_path, &pool);
                        }
                    }
                }
            }
        }
        let exit_status = child.wait();
        if !done_emitted {
            let success = matches!(&exit_status, Ok(s) if s.success());
            let _ = app2.emit(
                "keepalive-done",
                serde_json::json!({ "success": success, "raw": format!("exit: {:?}", exit_status) }),
            );
        }
    });

    // stderr 线程：防管道写满死锁，落 switcher.log
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let log_path = stderr_dir.join("logs").join("switcher.log");
            let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
            for line in std::io::BufRead::lines(std::io::BufReader::new(stderr)).map_while(Result::ok) {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    use std::io::Write;
                    let _ = f.write_all(format!("[{}] [keepalive][stderr] {}\n", fs_utils::now_ts(), line.trim()).as_bytes());
                }
            }
        });
    }

    Ok(())
}

/// 更新账号的手动录入会话凭证（可选高级功能；有凭证的账号才能走探活巡检）
#[tauri::command]
pub fn doubao_account_set_credential(
    state: State<AppState>,
    user_id: String,
    session_id: Option<String>,
    sid_guard: Option<String>,
) -> Result<(), String> {
    let mut pool = load_pool(&state);
    let acc = pool
        .accounts
        .iter_mut()
        .find(|a| a.user_id == user_id)
        .ok_or_else(|| format!("账号 {user_id} 不在账号池中"))?;
    acc.session_id = session_id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    acc.sid_guard = sid_guard.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    acc.session_expire_at = if acc.sid_guard.is_some() {
        // sid_guard 到期时间由 python 巡检解析；此处简单重置为未知
        None
    } else {
        acc.session_expire_at.clone()
    };
    acc.expired = None;
    if acc.session_id.is_some() {
        acc.session_source = Some("manual".to_string());
    }
    save_pool(&state, &pool)
}

/// 运行续期巡检脚本（解密同步 cookie + 探活续期，模式由 sync_only 决定）。
/// 脚本含网络请求可达数秒，async 派发线程池执行避免阻塞 UI。
#[tauri::command(async)]
pub fn doubao_renew_run(
    state: State<AppState>,
    sync_only: Option<bool>,
) -> Result<serde_json::Value, String> {
    let script = state.python_dir.join("doubao_renew.py");
    if !script.exists() {
        return Err(format!("找不到续期脚本: {}", script.display()));
    }
    let mut cmd = std::process::Command::new(&state.python_exe);
    cmd.arg(&script);
    if sync_only.unwrap_or(false) {
        cmd.arg("--sync-only");
    }
    cmd.env("AIWORKDATA_DIR", &state.data_dir)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = cmd
        .output()
        .map_err(|e| format!("运行续期脚本失败: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    // 脚本约定：stdout 最后一行（以 { 开头）为摘要 JSON；进度/错误日志走 stderr
    let summary_line = stdout.lines().rev().find(|l| l.trim_start().starts_with('{'));
    if let Some(line) = summary_line {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            return Ok(v);
        }
    }
    let stderr_tail: String = String::from_utf8_lossy(&out.stderr)
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "续期脚本未输出摘要（exit={:?}）{}",
        out.status.code(),
        if stderr_tail.is_empty() { String::new() } else { format!(":\n{stderr_tail}") }
    ))
}

/// 注册豆包会话续期每日计划任务（schtasks 调 PS 桥 KeepAlive：启动豆包 25s 联网滑动续期后关闭）
#[tauri::command(async)]
pub fn doubao_renew_task_register(state: State<AppState>, time: String) -> Result<(), String> {
    if !time.contains(':') || time.len() < 4 {
        return Err(format!("时间格式无效: {time}（应为 HH:MM）"));
    }
    let ps_dir = crate::state::resolve_ps_dir();
    let bridge = ps_dir.join("trae-switch-bridge.ps1");
    if !bridge.exists() {
        return Err(format!("找不到切换脚本: {}", bridge.display()));
    }
    let data_dir = state.data_dir.to_string_lossy();
    // /TR 不继承环境变量，显式 set AIWORKDATA_DIR（带引号兼容空格路径）；KeepAlive 无需 UserId
    let tr = format!(
        "cmd /c set \"AIWORKDATA_DIR={data_dir}\" && powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\" -Action KeepAlive -TargetApp Doubao -Json",
        bridge.to_string_lossy()
    );
    let (ok, _stdout, stderr) = crate::commands::misc::run_schtasks(&[
        "/Create",
        "/TN",
        crate::commands::misc::DOUBAO_TASK_NAME,
        "/TR",
        tr.as_str(),
        "/SC",
        "DAILY",
        "/ST",
        time.as_str(),
        "/F",
    ])?;
    if !ok {
        let detail = stderr.trim();
        let is_access_denied = detail.contains("Access is denied")
            || detail.contains("ERROR: Access is denied")
            || detail.contains("拒绝访问")
            || detail.contains("权限");
        if is_access_denied {
            return Err("权限不足（Access Denied）：请以管理员身份运行本应用后重新注册任务".to_string());
        }
        return Err(detail.to_string());
    }
    fs_utils::app_log(&state.data_dir, &format!("豆包续期定时任务已注册: {time}"));
    Ok(())
}

/// 查询豆包续期计划任务状态（存在与否 + 触发时间）
#[tauri::command(async)]
pub fn doubao_renew_task_status(_state: State<AppState>) -> Result<String, String> {
    let name = crate::commands::misc::DOUBAO_TASK_NAME;
    let (ok, stdout, _stderr) =
        crate::commands::misc::run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"])?;
    if !ok {
        return Ok("not_registered".to_string());
    }
    // 输出形如 "Start Time: HH:MM:SS"（本地化系统可能是「开始时间:」），取 HH:MM 部分
    let time = stdout
        .lines()
        .find_map(|l| {
            let idx = l.find(": ")?;
            let v = &l[idx + 2..];
            let hhmm: String = v.chars().take(5).collect();
            if hhmm.len() == 5 && hhmm.as_bytes()[2] == b':' {
                Some(hhmm)
            } else {
                None
            }
        })
        .unwrap_or_default();
    Ok(format!("registered:{time}"))
}

/// 注销豆包续期计划任务
#[tauri::command(async)]
pub fn doubao_renew_task_unregister(state: State<AppState>) -> Result<(), String> {
    let name = crate::commands::misc::DOUBAO_TASK_NAME;
    let (ok, _stdout, stderr) =
        crate::commands::misc::run_schtasks(&["/Delete", "/TN", name, "/F"])?;
    if !ok && !stderr.contains("不存在") && !stderr.contains("does not exist") {
        return Err(stderr.trim().to_string());
    }
    fs_utils::app_log(&state.data_dir, "豆包续期定时任务已注销");
    Ok(())
}
