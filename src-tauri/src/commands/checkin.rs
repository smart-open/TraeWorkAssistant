use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use tauri::{AppHandle, Emitter, Manager, State};

use serde::Deserialize;
use crate::fs_utils;
use crate::state::AppState;
use crate::commands::accounts::{build_account_views, resolve_user_ids};
use crate::python::spawn_script;

/// 签到运行防重入锁（应用级）：页面手动签到 / 托盘签到 / 静默签到共用，
/// 占用期间 try_lock 失败即拒绝新的签到请求。
/// 使用 tokio::sync::Mutex：其 Guard 为 Send，可在工作线程内持有到子进程结束
pub struct CheckinGuard(pub tokio::sync::Mutex<()>);

#[derive(Deserialize)]
pub struct CheckinOpts {
    pub scope: String,
    #[serde(default)]
    pub user_ids: Option<Vec<String>>,
    #[serde(default)]
    pub skip_checked_in: bool,
    #[serde(default)]
    pub skip_expired: bool,
}

/// 失败重试轮次配置：最多 2 轮，间隔逐轮加长（第 1 轮 30s、第 2 轮 90s），仅重试 failed 账号
const RETRY_DELAYS: [u64; 2] = [30, 90];

/// 单轮签到结果：per-uid 最终状态（重试轮覆盖旧状态，最终统计不重复计数）
#[derive(Default)]
struct RoundOutcome {
    /// uid -> "success" | "already" | "fail"
    statuses: std::collections::HashMap<String, &'static str>,
    /// uid -> 失败类型（Python 事件 error_type，如 "SessionDead"），重试筛选据此排除永久失效账号
    error_types: std::collections::HashMap<String, String>,
}

/// 一轮签到的子进程句柄（含解密临时账号文件路径，消费结束后删除）
struct RoundProc {
    child: std::process::Child,
    tmp_accounts: PathBuf,
}

#[tauri::command]
pub fn checkin_start(
    app: AppHandle,
    state: State<AppState>,
    opts: CheckinOpts,
) -> Result<(), String> {
    start_checkin_core(&app, &state, opts, false)
}

/// 签到核心入口（页面 / 托盘 / 静默签到共用）。
///
/// 实际流程在工作线程执行：先抢防重入锁并完成账号筛选，结果经 channel 同步
/// 返回给调用方（失败立即报错）；随后同线程继续跑「全量轮 + 最多 2 轮失败重试」，
/// 防重入锁由该线程持有到全部轮次结束。
///
/// `notify_done = true` 时完成后发系统通知（托盘 / 静默签到无窗口可见，需通知反馈）。
pub fn start_checkin_core(
    app: &AppHandle,
    _state: &AppState,
    opts: CheckinOpts,
    notify_done: bool,
) -> Result<(), String> {
    let app2 = app.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let st = app2.state::<AppState>();
        run_checkin_worker(&app2, &st, opts, notify_done, tx);
    });
    // 等待启动阶段结果（抢锁 + 筛选 + 拉起子进程），通常毫秒级
    rx.recv().unwrap_or_else(|_| Err("签到工作线程异常退出".into()))
}

/// 拉起一轮签到子进程：构建参数（含凭据解密临时文件），失败时清理临时文件
fn spawn_round(state: &AppState, uids: &[String]) -> Result<RoundProc, String> {
    let retry = state.settings().retry.max(0) as u32;
    let mut args = vec!["--json-stream".to_string()];
    args.push("--accounts".to_string());
    args.push(uids.join(","));
    if retry > 0 {
        args.push("--retry".to_string());
        args.push(retry.to_string());
    }

    // 凭据解密：checkin_accounts.json 只存占位，实际 jwt 在 Stronghold vault 中。
    // 每轮为本轮候选账号生成解密临时文件（--accounts-file），用后即删。
    let tmp_accounts = crate::vault::write_temp_accounts(state, uids)
        .map_err(|e| format!("生成签到凭据临时文件失败: {e}"))?;
    args.push("--accounts-file".to_string());
    args.push(tmp_accounts.to_string_lossy().to_string());

    let child = match spawn_script(state, "auto_checkin.py", &args, true) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_accounts);
            return Err(e);
        }
    };
    Ok(RoundProc { child, tmp_accounts })
}

/// 消费一轮子进程输出：NDJSON 解析 -> 事件 emit + 日志追加。
/// 脚本自身的 done 事件不转发——最终 done 由调用方在各轮汇总后统一发（避免重复计数）。
fn consume_round(app: &AppHandle, mut proc: RoundProc, log_path: &Path) -> RoundOutcome {
    let mut outcome = RoundOutcome::default();
    let stdout = proc.child.stdout.take();
    let stderr = proc.child.stderr.take();

    // stderr 线程：防止管道缓冲区写满导致子进程死锁（无需防重入锁）
    if let Some(stderr) = stderr {
        let log_path2 = log_path.to_path_buf();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let l = format!("[stderr] {}", l.trim());
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path2)
                    {
                        let _ = writeln!(f, "[{}] {}", crate::fs_utils::now_ts(), l);
                    }
                }
            }
        });
    }

    if let Some(stdout) = stdout {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if let Ok(l) = line {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) {
                    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "start" | "account" => {
                            if v.get("type").and_then(|t| t.as_str()) == Some("account") {
                                // 记录 per-uid 状态（重试轮结果覆盖旧状态）
                                let uid = v
                                    .get("user_id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let st = match v.get("status").and_then(|x| x.as_str()).unwrap_or("") {
                                    "success" => "success",
                                    "already" => "already",
                                    _ => "fail",
                                };
                                if !uid.is_empty() {
                                    outcome.statuses.insert(uid.clone(), st);
                                    // 保留失败类型供重试筛选：SessionDead（JWT 被服务端吊销）为
                                    // 永久失效，重试必然再 401，不进重试轮白等 30+90s
                                    if st == "fail" {
                                        if let Some(et) = v.get("error_type").and_then(|x| x.as_str()) {
                                            if !et.is_empty() {
                                                outcome.error_types.insert(uid.clone(), et.to_string());
                                            }
                                        }
                                    } else {
                                        outcome.error_types.remove(&uid);
                                    }
                                }
                            }
                            let _ = app.emit("checkin-progress", &v);
                        }
                        // done 事件不转发：由调用方汇总后统一发
                        _ => {}
                    }
                }
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_path)
                {
                    let _ = writeln!(f, "[{}] {}", crate::fs_utils::now_ts(), l);
                }
            }
        }
    }
    let _ = proc.child.wait();
    // 清理解密临时账号文件（无论本轮成败）
    let _ = std::fs::remove_file(&proc.tmp_accounts);
    outcome
}

/// 发送最终汇总 done 事件（checkin-progress + checkin-done），并按需发系统通知
fn emit_final_done(app: &AppHandle, ok: usize, already: usize, failed: usize, total: usize, notify_done: bool) {
    let payload = serde_json::json!({
        "type": "done", "ok": ok, "already": already, "failed": failed, "total": total
    });
    let _ = app.emit("checkin-progress", &payload);
    let _ = app.emit("checkin-done", &payload);
    if notify_done {
        crate::notify::notify(
            app,
            "签到完成",
            &format!("成功 {ok} · 已签 {already} · 失败 {failed}"),
        );
    }
}

/// 签到工作线程主体：抢锁 → 筛选 → 全量轮 → 最多 2 轮失败重试（间隔加长）→ 汇总 done
fn run_checkin_worker(
    app: &AppHandle,
    state: &AppState,
    opts: CheckinOpts,
    notify_done: bool,
    tx: Sender<Result<(), String>>,
) {
    // 防重入：正在签到时直接拒绝
    let guard = app.state::<CheckinGuard>();
    let held = match guard.0.try_lock() {
        Ok(g) => g,
        Err(_) => {
            fs_utils::app_log(&state.data_dir, "签到拒绝: 已有签到任务运行中");
            if notify_done {
                crate::notify::notify(app, "签到跳过", "已有签到任务正在进行中");
            }
            let _ = tx.send(Err("已有签到任务正在进行中".into()));
            return;
        }
    };

    let mut uids = match resolve_user_ids(state, &opts.scope, opts.user_ids) {
        Ok(u) => u,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    // 全集视图一次构建：跳过原因登记与 start 事件全集清单共用
    let views_all = build_account_views(state);
    // scope 内全集（跳过规则过滤前快照）：start 事件清单只覆盖本次签到范围，
    // 避免把范围外账号误显示为「跳过」
    let scope_all: Vec<String> = uids.clone();
    let name_of = |uid: &str| -> String {
        views_all
            .iter()
            .find(|v| v.user_id == uid)
            .map(|v| v.name.clone())
            .unwrap_or_else(|| uid.to_string())
    };
    // uid -> 跳过原因（checked_in=已签 / expired=JWT 过期 / cooldown=冷却中）。
    // start 事件携带全集清单：被跳过的账号也在进度列表中显示原因，不再凭空消失
    // （用户反馈：已签账号被跳过却「看起来没签到」，且下一轮列表行数/行序错乱）
    let mut skip_reasons: std::collections::HashMap<String, &'static str> =
        std::collections::HashMap::new();
    if opts.skip_checked_in || opts.skip_expired {
        uids.retain(|u| {
            let Some(v) = views_all.iter().find(|a| a.user_id == *u) else {
                return true;
            };
            if opts.skip_checked_in && v.checked_today == Some(true) {
                skip_reasons.insert(u.clone(), "checked_in");
                return false;
            }
            if opts.skip_expired && (v.jwt_exp_hours.is_none() || v.jwt_exp_hours.unwrap() <= 0.0)
            {
                skip_reasons.insert(u.clone(), "expired");
                return false;
            }
            true
        });
    }
    // 过滤冷却中的账号（SessionDead 永久跳过，其他类型冷却中跳过）
    {
        let cooled: std::collections::HashSet<&str> = views_all
            .iter()
            .filter(|v| v.cooldown_type.is_some())
            .map(|v| v.user_id.as_str())
            .collect();
        let before = uids.len();
        uids.retain(|u| {
            if cooled.contains(u.as_str()) {
                skip_reasons.insert(u.clone(), "cooldown");
                false
            } else {
                true
            }
        });
        let skipped = before - uids.len();
        if skipped > 0 {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("跳过 {} 个冷却中账号", skipped),
            );
        }
        // 积分过期感知调度：按 credits_expire_at 升序排列（最近过期的优先签到）
        // 无过期时间的账号排在最后；过期时间相同的按剩余积分降序
        let rc: crate::models::RemainingCreditsFile =
            crate::fs_utils::read_json(&state.path("remaining_credits.json"));
        uids.sort_by(|a, b| {
            let ea = rc.expire_times.get(a).copied();
            let eb = rc.expire_times.get(b).copied();
            match (ea, eb) {
                (Some(ta), Some(tb)) => {
                    if ta == tb {
                        // 过期时间相同 → 剩余积分降序
                        let ca = rc.credits.get(a).copied().unwrap_or(0.0);
                        let cb = rc.credits.get(b).copied().unwrap_or(0.0);
                        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
                    } else {
                        ta.cmp(&tb)
                    }
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });
    }
    // 全集账号清单构造（scope 内）：final_st 提供时非候选行沿用其最终状态（重试轮
    // 列表连续），否则非候选一律按登记的跳过原因显示；候选行统一 pending
    let accounts_payload =
        |final_st: Option<&std::collections::HashMap<String, &'static str>>| -> Vec<serde_json::Value> {
            scope_all
                .iter()
                .map(|u| {
                    let uid = u.as_str();
                    let (status, skip_reason) = if uids.iter().any(|x| x == uid) {
                        (serde_json::json!("pending"), serde_json::Value::Null)
                    } else if let Some(reason) = skip_reasons.get(uid) {
                        (serde_json::json!("skip"), serde_json::json!(reason))
                    } else if let Some(st) = final_st.and_then(|m| m.get(uid)) {
                        (serde_json::json!(st), serde_json::Value::Null)
                    } else {
                        (serde_json::json!("skip"), serde_json::Value::Null)
                    };
                    serde_json::json!({
                        "user_id": uid,
                        "name": name_of(uid),
                        "status": status,
                        "skip_reason": skip_reason,
                    })
                })
                .collect()
        };
    // 过滤后为空（全部已签/过期/冷却中）时不启动脚本：
    // 脚本在无 --accounts 参数时会回退为签全部账号，会绕过跳过规则并造成重复签到风险。
    // 直接向前端 emit 空轮次事件，UI 显示 0/0/0 的完成态。
    if uids.is_empty() {
        crate::fs_utils::app_log(
            &state.data_dir,
            "签到未启动: 过滤后无候选账号（全部已签/过期/冷却中）",
        );
        let _ = app.emit(
            "checkin-progress",
            serde_json::json!({ "type": "start", "total": 0, "accounts": accounts_payload(None) }),
        );
        let _ = app.emit(
            "checkin-progress",
            serde_json::json!({ "type": "done", "ok": 0, "already": 0, "failed": 0, "total": 0 }),
        );
        let _ = app.emit(
            "checkin-done",
            serde_json::json!({ "type": "done", "ok": 0, "already": 0, "failed": 0, "total": 0 }),
        );
        if notify_done {
            crate::notify::notify(app, "签到完成", "没有需要签到的账号（全部已签/冷却中）");
        }
        drop(held);
        let _ = tx.send(Ok(()));
        return;
    }

    let log_path = state.data_dir.join("logs").join("checkin.log");
    if let Some(p) = log_path.parent() {
        let _ = std::fs::create_dir_all(p);
    }

    // 第 0 轮（全量）先拉起子进程，成功后才向调用方报「已启动」
    let proc = match spawn_round(state, &uids) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Err(e));
            return; // held 在此自动释放
        }
    };
    let total_all = uids.len();
    // 主轮 start：候选 pending、跳过账号带原因（scope 内全集），先于 Python 事件发出
    let _ = app.emit(
        "checkin-progress",
        serde_json::json!({ "type": "start", "total": total_all, "accounts": accounts_payload(None) }),
    );
    crate::fs_utils::app_log(&state.data_dir, &format!("签到已启动: {} 个账号", total_all));
    // 启动阶段完成，通知调用方（页面/托盘只关心是否成功拉起）
    let _ = tx.send(Ok(()));

    // 汇总各轮 per-uid 最终状态：初始全部置 fail，实际结果逐轮覆盖；
    // 保证 ok + already + failed == total_all（脚本崩溃/丢事件的账号按失败计）
    let mut final_status: std::collections::HashMap<String, &'static str> = uids
        .iter()
        .map(|u| (u.clone(), "fail"))
        .collect();
    let outcome = consume_round(app, proc, &log_path);
    // 跨轮失败类型登记（重试轮覆盖旧值），供重试筛选排除 SessionDead 等永久失效账号
    let mut round_error_types: std::collections::HashMap<String, String> = outcome.error_types;
    for (uid, st) in outcome.statuses {
        final_status.insert(uid, st);
    }

    // 失败重试：最多 2 轮，仅重试 failed 账号，间隔逐轮加长（30s / 90s）
    for (i, &delay) in RETRY_DELAYS.iter().enumerate() {
        let round = i + 1;
        let mut failed_uids: Vec<String> = final_status
            .iter()
            .filter(|(_, st)| **st == "fail")
            // SessionDead（JWT 被服务端吊销）为永久失效：重试必然再次 401，跳过以免白等
            .filter(|(uid, _)| round_error_types.get(*uid).map(|e| e.as_str()) != Some("SessionDead"))
            .map(|(uid, _)| uid.clone())
            .collect();
        if failed_uids.is_empty() {
            break;
        }
        failed_uids.sort(); // 稳定顺序，便于日志比对
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!(
                "签到第 {round} 轮重试: {} 个失败账号，{delay}s 后开始",
                failed_uids.len()
            ),
        );
        // 重试倒计时事件（前端展示横幅）
        let _ = app.emit(
            "checkin-progress",
            serde_json::json!({
                "type": "retry", "round": round, "delay": delay, "total": failed_uids.len()
            }),
        );
        std::thread::sleep(std::time::Duration::from_secs(delay));
        match spawn_round(state, &failed_uids) {
            Ok(p) => {
                // 重试轮 start：候选置 pending，其余行沿用上轮最终状态/跳过原因，
                // 列表跨轮连续（不会把已签账号重置成「等待中」）
                let _ = app.emit(
                    "checkin-progress",
                    serde_json::json!({
                        "type": "start", "total": failed_uids.len(),
                        "accounts": accounts_payload(Some(&final_status))
                    }),
                );
                let o = consume_round(app, p, &log_path);
                // 重试轮结果覆盖对应 uid 的旧状态：非 fail 同步清除旧失败类型登记
                for (uid, st) in o.statuses {
                    final_status.insert(uid.clone(), st);
                    if st != "fail" {
                        round_error_types.remove(&uid);
                    }
                }
                for (uid, et) in o.error_types {
                    round_error_types.insert(uid, et);
                }
            }
            Err(e) => {
                // 重试轮拉起失败：保持原 fail 状态，继续后续轮次
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("签到第 {round} 轮重试拉起失败: {e}"),
                );
            }
        }
    }

    // 汇总最终状态，统一发 done（不重复计数）
    let ok = final_status.values().filter(|s| **s == "success").count();
    let already = final_status.values().filter(|s| **s == "already").count();
    let failed = final_status.values().filter(|s| **s == "fail").count();
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!("签到结束: 成功 {ok} · 已签 {already} · 失败 {failed} · 总计 {total_all}"),
    );
    // 签到结果按日落库（per-uid 最终状态，重试轮已合并），供 Dashboard 趋势图查询
    {
        let accounts = crate::vault::load_accounts(state);
        let name_of = |uid: &str| -> String {
            accounts
                .accounts
                .iter()
                .find(|a| a.user_id.as_deref() == Some(uid))
                .map(|a| a.name.clone())
                .unwrap_or_else(|| uid.to_string())
        };
        let entries: Vec<(String, String, String)> = final_status
            .iter()
            .map(|(uid, st)| (uid.clone(), name_of(uid), st.to_string()))
            .collect();
        crate::checkin_results::record_today(&state.data_dir, entries);
    }
    emit_final_done(app, ok, already, failed, total_all, notify_done);
    // _held 持有到全部轮次结束，线程退出自动释放
    let _held = held;
}

/// 查询近 N 天签到结果趋势（按日期升序），供 Dashboard 堆叠图使用
#[tauri::command]
pub fn checkin_trends(
    state: State<AppState>,
    days: Option<u32>,
) -> Vec<crate::checkin_results::TrendPoint> {
    crate::checkin_results::query_recent(&state.data_dir, days.unwrap_or(30))
}
