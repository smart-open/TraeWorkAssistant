//! WorkBuddy 积分域（原 workbuddy.rs 机械拆分）：M5 积分余额（F-20/F-22）、
//! 快照回退用量（T4.3/F-27）、M5 官方请求用量（F-25/F-58）、活动信息展示（T4.4/F-51）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, State};

use crate::commands::usage_history::{UsageSessionView, UsageSessionsResult};
use crate::fs_utils;
use crate::state::AppState;

use super::common::{as_str, auth_file_path_of, load_pool, save_pool};

// ── M5 积分（F-20/F-22，python 三件套 + 缓存）──────────────────────────────

#[tauri::command(async)]
pub fn workbuddy_credits_fetch(app: AppHandle, state: State<AppState>, user_id: Option<String>, fresh: Option<bool>) -> Result<serde_json::Value, String> {
    // 运行池句柄：刷新落账后即时三键拨新（issue #109 同类，WB 池运行期保鲜）
    let rt_state =
        app.state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
    // Rust 直调 tasks::wb_credits（原 python workbuddy_credits.py 移植）；
    // 失败以 Err 返回，成功恒为 {"ok":true,"cached":bool,"accounts":[...]}（消费契约见 tasks/wb_credits.rs 模块注释）
    let fresh = fresh.unwrap_or(false);
    let uid = user_id.as_deref();
    // stale-while-revalidate（2026-10-08）：缓存过期时先回旧值并转后台刷新，切板块不再等网络
    // （全局代理/跨境出口下单请求可达数秒，而每账号 ≥3 次串行请求）。手动刷新（fresh=true）
    // 与「无缓存」仍走同步路径，语义与耗时保持原样。
    if !fresh {
        if let Some(reply) = crate::tasks::wb_credits::stale_reply(&state, uid) {
            spawn_background_refresh(&state, &app);
            return Ok(reply);
        }
    }
    credits_refresh_now(&state, uid, fresh, Some(rt_state.inner()))
}

/// 同步取数一轮：取数（缓存/网络）→ 池余额回写 → 套餐回填 → 非缓存命中时追加快照。
/// 命令层与后台刷新线程共用，保证后台刷新与手动刷新行为一致。
fn credits_refresh_now(
    state: &AppState,
    uid: Option<&str>,
    fresh: bool,
    runtime: Option<&std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) -> Result<serde_json::Value, String> {
    let parsed = crate::tasks::wb_credits::fetch_credits(state, uid, fresh)?;
    // 回写账号池余额缓存（列表/概述展示），并经 runtime 句柄三键拨新运行池
    write_back_pool_balances(state, &parsed, runtime);
    // 会员套餐回填（仅 edition_type 为空的账号，见 backfill_edition_from_payment_type）
    backfill_edition_from_payment_type(state);
    // 每日余额快照（F-27 数据源）：非缓存命中时追加，按日去重，cap 365 天
    if parsed.get("cached") != Some(&serde_json::json!(true)) {
        append_credits_snapshot(state, &parsed);
    }
    Ok(parsed)
}

/// 后台刷新（SWR 的「后台」半程）：概览页与顶栏可能连续触发，用进程内在途标记去重，
/// 同一时刻只跑一轮；失败仅记日志（旧缓存仍在，页面无需感知，下次调用自然重试）。
/// 另加最小间隔节流（issue #84，2026-10-10）：缓存 10min 过期后若页面 30s 轮询一次，
/// 每次都会重新触发后台全量取数；节流窗口内直接跳过（调用方已回旧值，无感知），
/// 待窗口过后下一次调用再真正刷新。上次刷新未完成时不记账（避免把在途轮误当已完成）。
fn spawn_background_refresh(state: &State<AppState>, app: &AppHandle) {
    static LAST_BG_REFRESH: std::sync::Mutex<Option<std::time::Instant>> =
        std::sync::Mutex::new(None);
    const BG_REFRESH_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);
    {
        let last = LAST_BG_REFRESH
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(t) = *last {
            if t.elapsed() < BG_REFRESH_MIN_INTERVAL {
                return;
            }
        }
    }
    static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if IN_FLIGHT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // 通过在途检查后立即记账：本轮刷新被视为「正在进行」，节流窗口自此起算
    *LAST_BG_REFRESH
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    let st = state.inner().clone();
    let app = app.clone();
    std::thread::spawn(move || {
        // 复位兜底（2026-10-08 评审）：线程 panic 时 Drop 守卫仍复位 IN_FLIGHT，
        // 否则原子标记永久卡 true、后台刷新静默失效（SWR 恒回旧值）
        struct ResetInFlight;
        impl Drop for ResetInFlight {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _reset = ResetInFlight;
        let started = std::time::Instant::now();
        // panic 兜底（审查 P6，同 scheduler.rs 调度线程先例）：任务体 panic 照常落日志；
        // IN_FLIGHT 复位由上方 Drop 守卫保证，此处仅补可观测性
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = app
                .state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
            credits_refresh_now(&st, None, true, Some(rt.inner()))
        }));
        match outcome {
            Ok(Ok(_)) => fs_utils::app_log(
                &st.data_dir,
                &format!(
                    "[wb-credits] 后台刷新完成，共 {}ms",
                    started.elapsed().as_millis()
                ),
            ),
            Ok(Err(e)) => fs_utils::app_log(
                &st.data_dir,
                &format!(
                    "[wb-credits] 后台刷新失败（{}ms）：{e}",
                    started.elapsed().as_millis()
                ),
            ),
            Err(_) => fs_utils::app_log(
                &st.data_dir,
                &format!(
                    "[wb-credits] 后台刷新线程 panic（已捕获，不影响下次触发）：{}ms",
                    started.elapsed().as_millis()
                ),
            ),
        }
    });
}

/// 将积分查询结果中的余额回写账号池缓存（列表/概述展示；命令与调度器快照任务共用）。
/// 持 wb_pool_lock 罩住整段读-改-写：命令线程/后台刷新线程/调度器三方并发时
/// 不持锁的整文档写回会用旧副本抹掉彼此改动（lost-update）。
/// runtime 句柄非空时在账号锁释放后对网关运行池三键拨新（issue #109 同类）
fn write_back_pool_balances(
    state: &AppState,
    parsed: &Value,
    runtime: Option<&std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) {
    if let Some(accounts) = parsed.get("accounts").and_then(|v| v.as_array()) {
        let _guard = state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = load_pool(state);
        let now_ts = chrono::Utc::now().timestamp();
        // 运行池三键拨新数据源（ok=true 账号）：uid → (balance, earliest_expire)
        let mut snapshot: std::collections::HashMap<String, (f64, Option<i64>)> =
            std::collections::HashMap::new();
        for acc in accounts {
            let uid = acc.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            // ok=false（该账号本次查询失败）时不回写 balance/fetched_at/expire_at，
            // 保留旧值防误清——此前 balance/fetched_at 无条件回写，失败账号旧余额
            // 被覆写为 None，与下方注释口径相反，并加剧 reserve 过滤 fail-open（S3）
            let ok = acc.get("ok").and_then(Value::as_bool).unwrap_or(true);
            if !ok {
                continue;
            }
            let bal = acc.get("balance").and_then(|v| v.as_f64());
            let expire = earliest_pack_expire(acc, now_ts);
            if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == uid) {
                a.credits_balance = bal;
                a.credits_fetched_at = acc.get("fetched_at").and_then(|v| v.as_str()).map(|s| s.to_string());
                // 最早积分包到期（issue #28 API 网关调度口径，Buddy 不分包类型）：
                // 仅成功取数的账号回写
                a.credits_expire_at = expire;
            }
            if let Some(bal) = bal {
                snapshot.insert(uid.to_string(), (bal, expire));
            }
        }
        if let Err(e) = save_pool(state, &pool) {
            // 缓存级回填失败不中断主流程，但落日志不静默（对齐代码库写失败落 app_log 惯例）
            fs_utils::app_log(&state.data_dir, &format!("workbuddy: 积分回填落池失败: {e}"));
        }
        drop(_guard); // 先释放账号锁再取 runtime 锁：持锁再取其他锁违反项目锁纪律
        // 运行池三键拨新（issue #109 同类，WB 池）：账号记录已回写，但网关运行池此前
        // 钉死在装配时刻——expire_first「有过期时间者优先」会恒压池内 expire=None 的
        // 新登录账号，充值续期后的新到期也进不了池。仅 ok=true 账号入表，失败账号
        // 不提及即保留旧值（update_credits_snapshot 的 get_mut 纪律）。
        // buddy-ai 池装配源与 WB store 完全隔离（apply_pool_snapshot 独立装配），
        // 不推 WB 快照以免备份重导入窗口 uid 碰撞误写
        if let Some(rt) = runtime {
            let shared = rt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|g| g.shared.clone());
            if let Some(shared) = shared {
                if !snapshot.is_empty() {
                    shared.wb_pool.update_credits_snapshot(&snapshot);
                }
            }
        }
    }
}

/// 派生账号积分包最早到期（纯函数，便于单测）：
/// packages[]{remaining, expire_ts} 中剩余>0 且未过期的包取 min；
/// 无到期时间的包视为长期有效不参与；全部长期有效/无包信息 → None
fn earliest_pack_expire(acc: &Value, now_ts: i64) -> Option<i64> {
    acc.get("packages")
        .and_then(|v| v.as_array())
        .and_then(|pkgs| {
            pkgs.iter()
                .filter_map(|p| {
                    let rem = p.get("remaining").and_then(Value::as_f64)?;
                    let exp = p.get("expire_ts").and_then(Value::as_i64)?;
                    (rem > 0.0 && exp > now_ts).then_some(exp)
                })
                .min()
        })
}

/// 每日积分余额快照任务（调度器 `wb-credits-snapshot`；补齐近 7 日用量时序的关键）：
/// 强制刷新全部账号积分 → 回写池余额 → 追加当日快照（同日覆盖）。
/// 此前快照仅在「打开积分页且非缓存命中」时写入，应用内调度器到点自动建快照，
/// 差分序列不再因未打开页面而断档。runtime 句柄用于运行池三键拨新（issue #109
/// 同类）：GUI/网关进程传 Some，CLI 无网关传 None 跳过（对齐 qoder run_snapshot_task）
pub(crate) fn wb_credits_snapshot_task(
    state: &AppState,
    runtime: Option<&std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) -> Result<Value, String> {
    let parsed = crate::tasks::wb_credits::fetch_credits(state, None, true)?;
    write_back_pool_balances(state, &parsed, runtime);
    append_credits_snapshot(state, &parsed);
    fs_utils::app_log(
        &state.data_dir,
        "workbuddy: 每日积分余额快照已写入（应用内调度器 wb-credits-snapshot）",
    );
    Ok(parsed)
}

// ── 会员套餐回填（edition_type 数据源补齐）────────────────────────────────
// edition_type 现仅两条写入路径：OAuth login/account（带 editionType）与备份导入；
// auth 文件本身无该字段（实测顶层/ account 键均无），「导入本机账号」入池的账号套餐恒空，
// 两个页面的会员套餐列全为「—」。此处用计费域的付费类型接口补齐（实测可用，域随凭证）。

/// paymentType → 套餐展示名（对齐 CodeBuddy 套餐体系：free=体验版；未知值原样透传）
fn payment_type_to_edition(pt: &str) -> String {
    match pt {
        "free" => "体验版".into(),
        other => other.to_string(),
    }
}

/// 套餐回填：池内 edition_type 为空且有凭证副本的账号，逐个调
/// POST /v2/billing/meter/get-payment-type（域随凭证 domain 路由）→ 映射套餐名回写池。
/// 仅补空，不覆盖 OAuth 已写入的 editionType；单账号失败静默跳过（纯展示增强，fail-open）。
/// 回填一次后池内不再为空，后续调用零网络开销。
///
/// 两阶段防 lost-update（原实现在跨 N×10s 网络调用期间持整个池旧副本，写回会
/// 整体抹掉窗口内的并发改动——导入的新账号被旧副本覆盖即凭据丢失）：
///   Phase 1（锁外）：读池收集待回填账号 → 逐账号网络请求，仅收集 (id → edition) 结果；
///   Phase 2（短临界区）：重读最新池 → 按 id 只更新仍存在且 edition_type 仍为空的账号
///   （窗口内删除的账号自动跳过、OAuth 已填套餐的账号不被覆盖）。
fn backfill_edition_from_payment_type(state: &AppState) -> usize {
    // Phase 1（锁外）：收集 + 网络
    let need: Vec<String> = load_pool(state)
        .accounts
        .iter()
        .filter(|a| a.edition_type.is_empty())
        .map(|a| a.id.clone())
        .collect();
    if need.is_empty() {
        return 0;
    }
    let store: Value = crate::tasks::wb_common::load_token_store(state);
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let mut results: Vec<(String, String)> = Vec::new(); // (id, edition)
    for id in need {
        let Some(rec) = tokens.get(&id) else { continue };
        let Some(token) = as_str(fs_utils::dig(rec, &["access_token"])).filter(|t| !t.is_empty()) else {
            continue;
        };
        let domain = as_str(fs_utils::dig(rec, &["domain"])).unwrap_or_default();
        // 域名收敛（§4.5）：活动/积分网关 base 单点取用 region_activity_base
        let base = crate::tasks::wb_common::region_activity_base(&domain);
        let Ok(v) = billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-payment-type"), &token) else {
            continue;
        };
        // 注意：dig 是候选键语义（自动穿透 data 信封），候选里不能再写 "data"——
        // 否则 "data" 抢先命中整个信封对象，as_str 恒为 None（曾致套餐回填全静默失败）
        let Some(pt) = as_str(fs_utils::dig(&v, &["paymentType", "payment_type"]))
            .filter(|s| !s.is_empty() && *s != "unknown")
        else {
            continue;
        };
        results.push((id, payment_type_to_edition(&pt)));
    }
    if results.is_empty() {
        return 0;
    }
    // Phase 2（短临界区）：重读最新池，按 id 合并（保「仅补空」语义）
    let _guard = state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_pool(state);
    let mut filled = 0usize;
    for (id, edition) in &results {
        if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == *id) {
            if a.edition_type.is_empty() {
                a.edition_type = edition.clone();
                filled += 1;
            }
        }
    }
    if filled > 0 {
        if let Err(e) = save_pool(state, &pool) {
            fs_utils::app_log(&state.data_dir, &format!("workbuddy: 会员套餐回填落池失败: {e}"));
        }
        fs_utils::app_log(&state.data_dir, "workbuddy: 会员套餐已回填（payment-type → edition_type，仅补空）");
    }
    filled
}

/// 手动触发套餐回填（前端在列表存在空套餐账号时调用），返回本次回填的账号数。
/// async：内含逐账号计费域网络请求（≤10s/账号），同步命令会冻结 UI。
#[tauri::command(async)]
pub fn workbuddy_editions_backfill(state: State<AppState>) -> Result<usize, String> {
    Ok(backfill_edition_from_payment_type(&state))
}

/// 账号集合指纹（排序去重后的 user_id 列表）；无账号明细（空数组/缺字段）返回 None。
/// 用途：判定相邻快照是否「同账号集合可比」——见 append_credits_snapshot 与
/// workbuddy_usage_fallback 的可比性守卫。
fn accounts_fingerprint(items: &[Value]) -> Option<Vec<String>> {
    if items.is_empty() {
        return None;
    }
    let mut ids: Vec<String> = items
        .iter()
        .map(|a| {
            a.get("user_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect();
    ids.sort();
    ids.dedup();
    Some(ids)
}

/// 从快照对象取账号集合指纹（`{date,ts,total_balance,accounts:[{user_id,balance}],earned}`）
fn snapshot_fingerprint(snapshot: &Value) -> Option<Vec<String>> {
    accounts_fingerprint(snapshot.get("accounts").and_then(Value::as_array)?)
}

/// 追加每日积分余额快照（F-27）：SQLite 化 P6 → wb_credits_history 表，同日覆盖最新 + 365 天裁剪。
/// credits-dashboard-plan.md §2.2 方案 B：快照行新增 earned（当日新增积分）——
/// 口径 = max(当日余额差分(≥0), 当日签到 reward 合计)；首日无历史差分时退化为仅签到 reward。
/// **账号集合可比性守卫（2026-10-06 修复）**：与上一快照账号集合不一致（增/删账号）时
/// 余额差分不可比——新账号余额直接抬高 total，会被当成「当日获得」（实测 2026-10-05/10-06
/// 加号后 earned 虚增 2217.83/3059.06），且 fallback 侧消耗被 .max(0.0) 钳成 0；
/// 此时 earned 只取签到 reward。对齐 Qoder 侧账号数不一致即不采信差分的既有口径。
fn append_credits_snapshot(state: &AppState, parsed: &Value) {
    let store = crate::store::db(&state.data_dir);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let accounts: Vec<Value> = parsed
        .get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    serde_json::json!({
                        "user_id": a.get("user_id").cloned().unwrap_or_default(),
                        "balance": a.get("balance").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let total: f64 = accounts.iter().filter_map(|a| a.get("balance").and_then(Value::as_f64)).sum();
    // 当日签到 reward 合计（90 天滚动签到日志；reward 列缺省时回退 message「+N」解析）
    let checkin_reward = checkin_reward_today(&store, &today);
    // 当日余额差分（最近一条「早于今天」的快照 → 当日总余额）；无历史快照 = 首日，无差分
    let hist: Value = crate::store::docs::wb_credits_history_load(&store);
    let prev_snapshot = hist
        .get("snapshots")
        .and_then(Value::as_array)
        .and_then(|snaps| {
            snaps
                .iter()
                .filter(|s| s.get("date").and_then(Value::as_str).map(|d| d < today.as_str()).unwrap_or(false))
                .filter(|s| s.get("total_balance").and_then(Value::as_f64).is_some())
                .last()
        });
    let prev_total = prev_snapshot.and_then(|s| s.get("total_balance").and_then(Value::as_f64));
    // 可比性守卫：账号集合与上一快照不一致 → 差分含新账号余额，不可信，earned 只取签到 reward
    // （任一侧无账号明细的旧快照视为可比，维持既有差分行为）
    let set_changed = match (
        prev_snapshot.and_then(snapshot_fingerprint),
        accounts_fingerprint(&accounts),
    ) {
        (Some(prev), Some(cur)) => prev != cur,
        _ => false,
    };
    // 旧格式升级窗口留痕（2026-10-12 审查 P3）：任一侧无账号明细时差分不可校验
    // ——失败账号的余额缺在 total 里且无法补前值，earned 差分偏低无痕迹。
    // 记日志提示；次日起 prev 快照携带明细后守卫恢复判别力，自愈不再触发
    if !set_changed
        && prev_snapshot.and_then(snapshot_fingerprint).is_none()
        && prev_total.is_some()
    {
        fs_utils::app_log(
            &state.data_dir,
            "workbuddy: 积分快照差分处于旧格式升级窗口（上一次快照无账号明细），失败账号无法补前值，earned 可能偏低；次日快照携带明细后自愈",
        );
    }
    let earned = if set_changed {
        checkin_reward
    } else {
        match prev_total {
            Some(prev) => (total - prev).max(0.0).max(checkin_reward),
            None => checkin_reward,
        }
    };
    let snap = serde_json::json!({
        "date": today,
        "ts": chrono::Utc::now().timestamp_millis(),
        "total_balance": total,
        "accounts": accounts,
        "earned": earned,
    });
    if let Err(e) = crate::store::docs::wb_credits_history_upsert(&store, &snap) {
        fs_utils::app_log(&state.data_dir, &format!("workbuddy 积分快照写入失败: {e}"));
        return;
    }
    let _ = crate::store::docs::wb_credits_history_prune(&store);
}

/// 当日签到 reward 合计（纯聚合，便于单测口径）：优先 reward 列，缺省回退 message「+N」解析
fn checkin_reward_today(store: &std::sync::Arc<crate::store::Store>, today: &str) -> f64 {
    let results: Value = crate::store::docs::wb_checkin_results_load(store);
    results
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("date").and_then(Value::as_str) == Some(today))
        .filter(|r| r.get("status").and_then(Value::as_str) == Some("success"))
        .filter_map(|r| {
            r.get("reward")
                .and_then(Value::as_f64)
                .or_else(|| parse_reward_plus(r.get("message").and_then(Value::as_str).unwrap_or("")))
        })
        .sum()
}

/// 快照时序读取（credits-dashboard-plan.md §4.3 最小新增命令，方案 B 前端消费）：
/// wb_credits_history 全量（365 天，含 earned），{snapshots:[...]} 形状与 usage_fallback 读取侧一致。
#[tauri::command]
pub fn workbuddy_credits_history_list(state: State<AppState>) -> Result<serde_json::Value, String> {
    let hist: Value = crate::store::docs::wb_credits_history_load(&crate::store::db(&state.data_dir));
    Ok(hist)
}

// ── 积分用量快照回退（T4.3/F-27）────────────────────────────────────────────

/// 快照回退用量：官方用量不可用时自动切换数据源（F-27）。
/// 推导：当日消耗 = 前一日总余额 − 当日总余额 + 当日签到奖励（签到日志 message「+N」）；
/// 负差值（充值包到账/快照波动）记 0。口径明示「快照回退」，非官方逐请求统计。
#[tauri::command]
pub fn workbuddy_usage_fallback(state: State<AppState>) -> Result<serde_json::Value, String> {
    let hist: Value = crate::store::docs::wb_credits_history_load(&crate::store::db(&state.data_dir));
    let snapshots = hist.get("snapshots").and_then(Value::as_array).cloned().unwrap_or_default();
    if snapshots.len() < 2 {
        return Err(
            "快照回退不可用：本地余额时序不足（至少两天快照）。请在积分页刷新几次建立时序后重试。".to_string(),
        );
    }

    // 签到日志 → 每日奖励充值（90 天滚动，仅 success 事件；SQLite 化 P3 走 store）
    let results: Value = crate::store::docs::wb_checkin_results_load(&crate::store::db(&state.data_dir));
    let mut recharge: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for r in results.get("results").and_then(Value::as_array).into_iter().flatten() {
        if r.get("status").and_then(Value::as_str) != Some("success") {
            continue;
        }
        let Some(date) = r.get("date").and_then(Value::as_str) else { continue };
        let msg = r.get("message").and_then(Value::as_str).unwrap_or("");
        if let Some(v) = parse_reward_plus(msg) {
            *recharge.entry(date.to_string()).or_insert(0.0) += v;
        }
    }

    // 快照差分（快照按 date 升序，credits 追加时保序）
    // 账号集合可比性守卫（2026-10-06 修复）：相邻两快照账号集合不一致（增/删账号）当日
    // 差分不可比——新账号余额抬高 cur_bal 会把消耗 .max(0.0) 钳成 0（实测 2026-10-05/10-06
    // 加号日恒 0），删账号则反向虚增消耗。该日**不计入 daily**（趋势留空，对齐 Qoder
    // consumed=null 过滤「不可比日不计入」口径；旧快照无账号明细时维持既有差分行为）。
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut daily: Vec<Value> = vec![];
    let mut skipped_uncomparable = 0usize;
    for i in 1..snapshots.len() {
        let (Some(prev_bal), Some(cur_bal)) = (
            snapshots[i - 1].get("total_balance").and_then(Value::as_f64),
            snapshots[i].get("total_balance").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let Some(date) = snapshots[i].get("date").and_then(Value::as_str) else { continue };
        let comparable = match (
            snapshot_fingerprint(&snapshots[i - 1]),
            snapshot_fingerprint(&snapshots[i]),
        ) {
            (Some(prev), Some(cur)) => prev == cur,
            _ => true,
        };
        if !comparable {
            skipped_uncomparable += 1;
            continue;
        }
        let reward = recharge.get(date).copied().unwrap_or(0.0);
        let usage = (prev_bal - cur_bal + reward).max(0.0);
        daily.push(serde_json::json!({ "date": date, "usage": usage }));
    }

    // 聚合：今日/近 7 天/本月（口径与官方用量对齐）
    use chrono::Datelike;
    let now = chrono::Local::now().date_naive();
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    for d in &daily {
        let (Some(date), Some(u)) = (d.get("date").and_then(Value::as_str), d.get("usage").and_then(Value::as_f64)) else {
            continue;
        };
        let Ok(day) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") else { continue };
        let dist = (now - day).num_days();
        if dist == 0 {
            usage_today += u;
        }
        if (0..7).contains(&dist) {
            usage_week += u;
        }
        if day.year() == now.year() && day.month() == now.month() {
            usage_month += u;
        }
    }

    let note = if skipped_uncomparable > 0 {
        format!(
            "快照回退数据源（本地余额时序差分 + 签到日志推导），非官方逐请求口径；账号增删日不可比已跳过 {skipped_uncomparable} 天"
        )
    } else {
        "快照回退数据源（本地余额时序差分 + 签到日志推导），非官方逐请求口径".to_string()
    };
    Ok(serde_json::json!({
        "status": "snapshot",
        "snapshot_days": snapshots.len(),
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily,
        "note": note,
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "_today": today,
    }))
}

/// 从签到 message 提取「+N」奖励数额（不硬编码数额，仅解析接口回显）
fn parse_reward_plus(msg: &str) -> Option<f64> {
    let idx = msg.find('+')?;
    let rest = &msg[idx + 1..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    num.parse::<f64>().ok().filter(|v| *v > 0.0)
}

// ── M5 官方请求用量（F-25/F-58，批次3）────────────────────────────────────
//
// POST <domain>/billing/meter/get-user-request-usage（设计 §7.1，语义对齐
// oss-research/workbuddy-switch official_usage.rs）：近 31 天窗口分页拉取请求明细，
// 聚合 今日/近7天/本月 消耗积分 + 逐日/按模型排行；本地缓存 10 分钟（refresh 强制刷新）。
// 脱敏红线：上游可能携带的 prompt/input 等字段一律不复制，不入缓存不返回。

/// requestTime → 本地日期 "YYYY-MM-DD"（兼容 epoch 秒/毫秒、RFC3339、本地时间串、纯日期）
fn usage_row_date(item: &Value) -> Option<String> {
    use chrono::TimeZone;
    let raw = item.get("requestTime").or_else(|| item.get("request_time"))?;
    if let Some(n) = raw.as_f64() {
        if !n.is_finite() {
            return None;
        }
        let ms = if n.abs() < 10_000_000_000.0 { (n * 1000.0).round() as i64 } else { n.round() as i64 };
        return chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
    }
    let text = raw.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, fmt) {
            return chrono::Local
                .from_local_datetime(&parsed)
                .single()
                .map(|d| d.date_naive().format("%Y-%m-%d").to_string());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(d.format("%Y-%m-%d").to_string());
    }
    None
}

fn usage_credit(item: &Value) -> Option<f64> {
    let v = item.get("credit")?;
    let n = v.as_f64().or_else(|| v.as_str()?.trim().parse::<f64>().ok())?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// Buddy 请求级明细行（issue #82 会话明细 Tab；官网 get-user-request-usage
/// 返回 requestTime/credit/model/requestId + 会话说明 input/inputTrunc，无 token 字段）
#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct WbSessionRow {
    /// requestTime（Unix 秒；date-only 行回退当日零点）
    ts: i64,
    /// requestId（缺失时 uuidless_key 兜底，仅用于去重与稳定排序）
    request_id: String,
    model: String,
    credits: f64,
    /// 会话说明（inputTrunc 优先、input 兜底，≤200 字符；旧缓存行 serde 默认空串）
    #[serde(default)]
    preview: String,
}

/// requestTime → Unix 秒（兼容 epoch 秒/毫秒、RFC3339、本地时间串；纯日期回退当日零点）
fn usage_row_ts(item: &Value) -> Option<i64> {
    use chrono::TimeZone;
    let raw = item.get("requestTime").or_else(|| item.get("request_time"))?;
    if let Some(n) = raw.as_f64() {
        if !n.is_finite() {
            return None;
        }
        let ms = if n.abs() < 10_000_000_000.0 { (n * 1000.0).round() as i64 } else { n.round() as i64 };
        return chrono::DateTime::from_timestamp_millis(ms).map(|d| d.timestamp());
    }
    let text = raw.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed.timestamp());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, fmt) {
            return chrono::Local
                .from_local_datetime(&parsed)
                .single()
                .map(|d| d.timestamp());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        // date-only 无法恢复精确时刻，回退当日零点（区间过滤/排序够用）
        return chrono::Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
            .single()
            .map(|dt| dt.timestamp());
    }
    None
}

/// 会话说明提取：inputTrunc（官网已截断）优先，缺失取 input，兜底 agentPurpose；
/// 统一压平换行并截断 200 字符（防异常长文本撑爆缓存与表格，对齐 Trae truncate_preview）
fn usage_row_preview(item: &Value) -> String {
    let pick = ["inputTrunc", "input_trunc", "input", "agentPurpose", "agent_purpose"]
        .iter()
        .find_map(|k| as_str(fs_utils::dig(item, &[*k])).filter(|s| !s.trim().is_empty()))
        .unwrap_or_default();
    let flat: String = pick
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= 200 {
        return flat.to_string();
    }
    flat.chars().take(200).collect()
}

/// 行级明细 kv 读-改-写的串行化锁（对齐 profile.rs STATS_KV_LOCK）：单账号拉取命令与
/// 后台全账号刷新（单轮 20-53s）可并发，整对象覆盖写回会丢失对方刚写入的其他账号桶
/// （lost-update，审查 P1）。
static SESSIONS_KV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

fn sessions_kv_lock() -> &'static std::sync::Mutex<()> {
    SESSIONS_KV_LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Buddy 行级明细落盘（kv 缓存；31 天滑动窗口全量替换该账号桶）。
/// 结构：`{ fetched_at: i64, accounts: { <acct_id>: { name, rows: [WbSessionRow] } } }`
/// 读-改-写经 SESSIONS_KV_LOCK 串行化（审查 P1）。
/// `full_refresh`（G6）：仅全量路径更新全局 `fetched_at`——单账号刷新也覆盖会让
/// 「上次全量刷新时间」显示为最近一次单账号刷新时刻，失真
fn save_account_session_rows(state: &AppState, acct_id: &str, name: &str, rows: &[WbSessionRow], full_refresh: bool) {
    const KEY: &str = "workbuddy_usage_sessions_cache";
    let _guard = sessions_kv_lock().lock().unwrap_or_else(|e| e.into_inner());
    let db = crate::store::db(&state.data_dir);
    let mut cache: Value = db.kv_get(KEY);
    if !cache.is_object() {
        cache = serde_json::json!({ "fetched_at": 0, "accounts": {} });
    }
    if !cache.get("accounts").map(Value::is_object).unwrap_or(false) {
        cache["accounts"] = serde_json::json!({});
    }
    if full_refresh {
        cache["fetched_at"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
    }
    cache["accounts"][acct_id] = serde_json::json!({ "name": name, "rows": rows });
    let _ = db.kv_set(KEY, &cache);
}

/// 官方请求用量（F-25）：user_id 指定账号，缺省取 auth 文件当前账号或首个有凭证账号。
#[tauri::command(async)]
pub fn workbuddy_usage_official(
    state: State<AppState>,
    user_id: Option<String>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    workbuddy_usage_official_impl(&state, user_id, refresh.unwrap_or(false))
}

/// 实现（本命令与调度器 wb-credits-snapshot Token 同步共用）：
/// refresh=true 跳过 10min 缓存强制上游拉取；无可用凭证账号时 Err（调用方自行降级）
pub(crate) fn workbuddy_usage_official_impl(
    state: &AppState,
    user_id: Option<String>,
    refresh: bool,
) -> Result<serde_json::Value, String> {
    let cache_path = "workbuddy_usage_official_cache"; // kv 键（SQLite 化 P2）

    // 选号：user_id → auth 文件当前账号 → 首个有 token store 凭证的账号
    //（审查 P1：选号解析提到缓存命中判断之前——缓存按账号区分，命中须同账号）
    let store: serde_json::Value = crate::tasks::wb_common::load_token_store(&state);
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let pick = |id: &str| -> Option<(String, String, String)> {
        let rec = tokens.get(id)?;
        let token = as_str(fs_utils::dig(&rec, &["access_token"]))?;
        if token.is_empty() {
            return None;
        }
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        Some((id.to_string(), token, domain))
    };
    let pool = load_pool(&state);
    let chosen = user_id
        .as_deref()
        .and_then(|uid| pick(uid))
        .or_else(|| {
            let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(&state));
            let fuid = as_str(fs_utils::dig(&raw, &["uid"]))?;
            pool.accounts.iter().find(|a| a.uid == fuid).map(|a| a.id.clone()).and_then(|id| pick(&id))
        })
        .or_else(|| pool.accounts.iter().find_map(|a| pick(&a.id)))
        .or_else(|| tokens.keys().find_map(|k| pick(k)))
        .ok_or("无可用账号凭证（请先在账号管理导入/扫码入池并续期）")?;
    let (acct_id, token, domain) = chosen;

    // 缓存命中条件（审查 P1）：状态存在 + 10min 内 + 缓存 account_id 与本次解析账号一致；
    // 不一致或旧缓存缺 account_id 一律视为未命中，重新按当前账号拉取。
    // F-59 stale-on-error：过期缓存保留一份，拉取失败时降级回退（见下方 fail 闭包）。
    let cached_val: Option<serde_json::Value> = {
        let c: serde_json::Value = crate::store::db(&state.data_dir).kv_get(cache_path);
        (c.get("status").is_some()).then_some(c)
    };
    if !refresh {
        if let Some(cached) = &cached_val {
            let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
            let cached_acct = cached.get("account_id").and_then(Value::as_str);
            if cached_acct == Some(acct_id.as_str())
                && chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000
            {
                return Ok(cached.clone());
            }
        }
    }
    // 拉取失败的降级出口：同账号存在历史缓存（即使过期）→ 返回缓存 + stale 标记，
    // 不让看板空屏；无任何缓存才向上抛错（前端再走 usageFallback 快照回退链）
    let fail = |msg: &str| -> Result<serde_json::Value, String> {
        if let Some(c) = &cached_val {
            if c.get("account_id").and_then(Value::as_str) == Some(acct_id.as_str()) {
                let mut stale = c.clone();
                stale["stale"] = serde_json::json!(true);
                stale["stale_reason"] = serde_json::json!(msg);
                return Ok(stale);
            }
        }
        Err(msg.to_string())
    };

    // 核心：拉取 + 聚合（拆出供全账号聚合命令 workbuddy_usage_official_all 复用）
    let (payload, session_rows) = match usage_official_fetch(&acct_id, &token, &domain) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let _ = crate::store::db(&state.data_dir).kv_set(cache_path, &payload);
    // 行级明细落盘（issue #82 Buddy 会话明细 Tab；31 天窗口全量替换该账号桶）
    let acct_name = pool
        .accounts
        .iter()
        .find(|a| a.id == acct_id)
        .map(|a| a.nickname.clone())
        .unwrap_or_default();
    save_account_session_rows(state, &acct_id, &acct_name, &session_rows, false);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "workbuddy: 官方用量刷新（{acct_id}，{} 行）",
            payload.get("request_count_total").and_then(Value::as_u64).unwrap_or(0)
        ),
    );
    Ok(payload)
}

/// 官方用量核心（拉取近 31 天分页明细 + 聚合，缓存逻辑留在命令层）。
/// 拆出供单账号命令 `workbuddy_usage_official` 与全账号聚合命令
/// `workbuddy_usage_official_all`（Buddy 积分看板近 7 日趋势数据源）复用。
/// 返回（聚合 payload, 请求级行列表）：行列表供会话明细落盘（issue #82）。
fn usage_official_fetch(
    acct_id: &str,
    token: &str,
    domain: &str,
) -> Result<(serde_json::Value, Vec<WbSessionRow>), String> {
    // 区域路由（T4.5/F-36，§5.2）：Global 账号（domain 含 workbuddy.ai）billing
    // 全走 www.workbuddy.ai；CN 账号维持既有 workbuddy.cn 网关。
    // 域名收敛（§4.5）：判定 + base 构造单点化 region_activity_base（显式 URL domain 保留）
    let base = if domain.is_empty() {
        crate::tasks::wb_common::region_activity_base("").to_string()
    } else if domain.starts_with("http://") || domain.starts_with("https://") {
        domain.trim_end_matches('/').to_string()
    } else {
        crate::tasks::wb_common::region_activity_base(&domain).to_string()
    };
    let url = format!("{base}/billing/meter/get-user-request-usage");
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).build();

    let today = chrono::Local::now().date_naive();
    let start = today - chrono::Duration::days(30);
    let start_text = format!("{start} 00:00:00");
    let end_text = format!("{today} 23:59:59");

    // 分页拉取（≤20 页 × 3000 行；requestId 去重；跳过 credit 缺失/负值/时间不可解析行）
    let mut rows: Vec<(String, f64, String)> = vec![]; // (date, credit, model)
    let mut session_rows: Vec<WbSessionRow> = vec![]; // 请求级行（会话明细落盘）
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reported_total: u64 = 0;
    // 批内行序号（G5）：uuidless 兜底键的哈希输入，防同键碰撞去重漏计
    let mut uuidless_seq: u64 = 0;
    for page in 1u64..=20 {
        let body = serde_json::json!({
            "startTime": start_text,
            "endTime": end_text,
            "pageNum": page,
            "pageSize": 3000,
        });
        let resp = agent
            .post(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("X-Client-Platform", "web")
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let v: serde_json::Value = match resp {
            Ok(r) => r.into_json().unwrap_or_default(),
            Err(ureq::Error::Status(code, _)) => {
                return Err(format!("官方用量请求失败（HTTP {code}）：请检查凭证有效期"));
            }
            Err(e) => return Err(format!("官方用量请求失败: {e}")),
        };
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 && code != 200 {
            return Err(format!("官方用量请求失败（code={code}）"));
        }
        let data = match v.get("data") {
            Some(d) => d,
            None => return Err("官方响应格式无效".into()),
        };
        let items = match data.get("data").and_then(Value::as_array) {
            Some(a) => a,
            None => return Err("官方响应格式无效".into()),
        };
        reported_total = reported_total.max(
            data.get("total")
                .and_then(Value::as_u64)
                .unwrap_or(items.len() as u64),
        );
        for item in items {
            let Some(credit) = usage_credit(item) else { continue };
            let Some(date) = usage_row_date(item) else { continue };
            let model = as_str(fs_utils::dig(item, &["model"]))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "未知模型".into());
            let rid = as_str(fs_utils::dig(item, &["requestId", "request_id"]))
                .unwrap_or_else(|| {
                    let k = uuidless_key(&date, &model, &credit, uuidless_seq);
                    uuidless_seq += 1;
                    k
                });
            if seen.insert(rid.clone()) {
                rows.push((date, credit, model.clone()));
                if let Some(ts) = usage_row_ts(item) {
                    session_rows.push(WbSessionRow {
                        ts,
                        request_id: rid,
                        model,
                        credits: credit,
                        preview: usage_row_preview(item),
                    });
                }
            }
        }
        if items.is_empty() || seen.len() as u64 >= reported_total {
            break;
        }
    }

    // 聚合：今日/近7天/本月 + 逐日（含按模型）+ 模型排行
    use chrono::Datelike;
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    let mut daily_credits: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut daily_models: std::collections::HashMap<String, std::collections::HashMap<String, (u64, f64)>> =
        std::collections::HashMap::new();
    let mut model_totals: std::collections::HashMap<String, (u64, f64)> = std::collections::HashMap::new();
    for (date, credit, model) in &rows {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            let dist = (today - d).num_days();
            if dist == 0 {
                usage_today += credit;
            }
            if (0..7).contains(&dist) {
                usage_week += credit;
            }
            if d.year() == today.year() && d.month() == today.month() {
                usage_month += credit;
            }
        }
        *daily_credits.entry(date.clone()).or_insert(0.0) += credit;
        let dm = daily_models.entry(date.clone()).or_default();
        let e = dm.entry(model.clone()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += credit;
        let mt = model_totals.entry(model.clone()).or_insert((0, 0.0));
        mt.0 += 1;
        mt.1 += credit;
    }

    let mut models_out: Vec<serde_json::Value> = model_totals
        .into_iter()
        .map(|(model, (count, credit))| {
            serde_json::json!({ "model": model, "request_count": count, "credit": credit })
        })
        .collect();
    models_out.sort_by(|a, b| {
        let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut daily_out: Vec<serde_json::Value> = vec![];
    for i in (0..=30).rev() {
        let d = today - chrono::Duration::days(i);
        let key = d.format("%Y-%m-%d").to_string();
        let mut day_models: Vec<serde_json::Value> = daily_models
            .get(&key)
            .map(|m| {
                m.iter()
                    .map(|(model, (count, credit))| {
                        serde_json::json!({ "model": model, "request_count": count, "credit": credit })
                    })
                    .collect()
            })
            .unwrap_or_default();
        day_models.sort_by(|a, b| {
            let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
        });
        daily_out.push(serde_json::json!({
            "date": key,
            "usage": daily_credits.get(&key).copied().unwrap_or(0.0),
            "models": day_models,
        }));
    }

    let payload = serde_json::json!({
        "status": "complete",
        "account_id": acct_id,
        "domain": base,
        "range_start": start.format("%Y-%m-%d").to_string(),
        "range_end": today.format("%Y-%m-%d").to_string(),
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "request_count_total": seen.len(),
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily_out,
        "models": models_out,
    });
    Ok((payload, session_rows))
}

/// 全账号官方用量聚合（Buddy 积分看板「近 7 日积分消耗」主数据源）：
/// force=false 走 10min 缓存；调度器 wb-credits-snapshot 以 force=true 定时预热本缓存
/// （issue #61 同款缺口：此前调度器只刷单账号 workbuddy_usage_official_cache，
/// 而看板消费的是本全账号聚合缓存，页面常驻时近 7 日趋势停留在最后打开时刻）。
#[tauri::command(async)]
pub fn workbuddy_usage_official_all(state: State<AppState>) -> Result<serde_json::Value, String> {
    workbuddy_usage_official_all_impl(&state, false)
}

/// 实现（本命令与调度器 wb-credits-snapshot 用量刷新共用）：
/// 遍历账号池全部有凭证账号，逐个拉取官方用量明细后按日/按模型求和（31 天零填充）。
/// 此前看板用快照差分（usageFallback）作唯一数据源——快照只在打开积分页且非缓存
/// 命中时写入，未打开应用的日子无快照，7 日趋势只剩「昨天」一格。
/// 单账号失败跳过（accounts_ok 计数），全部失败才报错并回退过期缓存（stale）。
/// 聚合结果缓存 10 分钟（跨账号全量拉取代价高，避免看板每次刷新都打满分页请求）。
/// SWR（issue #84，2026-10-10）：有缓存但已过期时先回旧值并转后台全量刷新——
/// 全量拉取为 7 账号×31 天分页串行请求，纯 CPU 20-53s（用户报告 25% 占用主项），
/// 缓存过期后的看板打开/轮询不再同步扛这个耗时；后台完成后下次调用自然命中新缓存。
pub(crate) fn workbuddy_usage_official_all_impl(
    state: &AppState,
    force: bool,
) -> Result<serde_json::Value, String> {
    let cache_path = "workbuddy_usage_official_all_cache"; // kv 键（SQLite 化 P2）
    let cached_val: Option<Value> = {
        let c: Value = crate::store::db(&state.data_dir).kv_get(cache_path);
        (c.get("status").is_some()).then_some(c)
    };
    if !force {
        if let Some(cached) = &cached_val {
            let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
            if chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000 {
                return Ok(cached.clone());
            }
            // SWR：转后台刷新并先回旧值（标记 stale，前端已兼容该字段——失败回退同款）
            spawn_official_all_refresh(state);
            let mut stale = cached.clone();
            stale["stale"] = serde_json::json!(true);
            stale["stale_reason"] = serde_json::json!("swr_background_refresh");
            return Ok(stale);
        }
    }
    let fail = |msg: &str| -> Result<Value, String> {
        // 失败落日志（issue #84）：stale-on-error 此前完全静默，排查无凭据
        fs_utils::app_log(
            &state.data_dir,
            &format!("workbuddy: 全账号官方用量聚合失败，回退 stale/报错：{msg}"),
        );
        if let Some(c) = &cached_val {
            let mut stale = c.clone();
            stale["stale"] = serde_json::json!(true);
            stale["stale_reason"] = serde_json::json!(msg);
            return Ok(stale);
        }
        Err(msg.to_string())
    };

    // 枚举有凭证账号：账号池优先，token store 补充（按 id 去重）
    let store: Value = crate::tasks::wb_common::load_token_store(state);
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let pick = |id: &str| -> Option<(String, String, String)> {
        let rec = tokens.get(id)?;
        let token = as_str(fs_utils::dig(&rec, &["access_token"]))?;
        if token.is_empty() {
            return None;
        }
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        Some((id.to_string(), token, domain))
    };
    let pool = load_pool(state);
    let mut list: Vec<(String, String, String)> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in &pool.accounts {
        if let Some(t) = pick(&a.id) {
            if seen_ids.insert(a.id.clone()) {
                list.push(t);
            }
        }
    }
    for id in tokens.keys() {
        if let Some(t) = pick(id) {
            if seen_ids.insert(id.clone()) {
                list.push(t);
            }
        }
    }
    if list.is_empty() {
        return Err("无可用账号凭证（请先在账号管理导入/扫码入池并续期）".into());
    }

    // 逐账号拉取 + 按日/按模型聚合（单账号失败跳过，不让一个失效凭证拖垮整板趋势）
    let mut daily_credits: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut model_totals: std::collections::HashMap<String, (u64, f64)> = std::collections::HashMap::new();
    let mut ok = 0usize;
    let mut req_total = 0u64;
    for (id, token, domain) in &list {
        let Ok((p, session_rows)) = usage_official_fetch(id, token, domain) else {
            continue;
        };
        ok += 1;
        // 行级明细落盘（issue #82 Buddy 会话明细 Tab；31 天窗口全量替换该账号桶）
        let acct_name = pool
            .accounts
            .iter()
            .find(|a| &a.id == id)
            .map(|a| a.nickname.clone())
            .unwrap_or_default();
        save_account_session_rows(state, id, &acct_name, &session_rows, true);
        req_total += p.get("request_count_total").and_then(Value::as_u64).unwrap_or(0);
        for row in p.get("daily").and_then(Value::as_array).into_iter().flatten() {
            let Some(date) = row.get("date").and_then(Value::as_str) else { continue };
            let Some(u) = row.get("usage").and_then(Value::as_f64) else { continue };
            *daily_credits.entry(date.to_string()).or_insert(0.0) += u;
        }
        // 按模型汇总（单账号接口 31 天全窗口口径，跨账号合并；此前丢弃导致看板模型排行无数据）
        for m in p.get("models").and_then(Value::as_array).into_iter().flatten() {
            let Some(model) = m.get("model").and_then(Value::as_str) else { continue };
            let e = model_totals.entry(model.to_string()).or_insert((0, 0.0));
            e.0 += m.get("request_count").and_then(Value::as_u64).unwrap_or(0);
            e.1 += m.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        }
    }
    if ok == 0 {
        return fail("全部账号官方用量拉取失败（凭证可能已失效，请续期后重试）");
    }

    let mut models_out: Vec<serde_json::Value> = model_totals
        .into_iter()
        .map(|(model, (count, credit))| {
            serde_json::json!({ "model": model, "request_count": count, "credit": credit })
        })
        .collect();
    models_out.sort_by(|a, b| {
        let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });

    // 31 天零填充输出（汇总口径与单账号命令一致）
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let start = today - chrono::Duration::days(30);
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    let mut daily_out: Vec<Value> = vec![];
    for i in (0..=30).rev() {
        let d = today - chrono::Duration::days(i);
        let key = d.format("%Y-%m-%d").to_string();
        let u = daily_credits.get(&key).copied().unwrap_or(0.0);
        if i == 0 {
            usage_today = u;
        }
        if i < 7 {
            usage_week += u;
        }
        if d.year() == today.year() && d.month() == today.month() {
            usage_month += u;
        }
        daily_out.push(serde_json::json!({ "date": key, "usage": u }));
    }

    let payload = serde_json::json!({
        "status": "complete",
        "source": "official_all",
        "accounts_total": list.len(),
        "accounts_ok": ok,
        "range_start": start.format("%Y-%m-%d").to_string(),
        "range_end": today.format("%Y-%m-%d").to_string(),
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "request_count_total": req_total,
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily_out,
        "models": models_out,
    });
    let _ = crate::store::db(&state.data_dir).kv_set(cache_path, &payload);
    fs_utils::app_log(
        &state.data_dir,
        &format!("workbuddy: 全账号官方用量聚合（{ok}/{} 账号，{} 行）", list.len(), req_total),
    );
    Ok(payload)
}

/// 官方用量全账号聚合的后台刷新（SWR 的「后台」半程，issue #84）：
/// 最小间隔节流——失败时缓存仍过期，看板 30s 轮询会反复触发全量拉取，窗口内直接跳过；
/// 在途去重——全量聚合单轮 20-53s，同一时刻只跑一轮；记账在通过在途检查后立即写入
/// （失败重试节奏由节流窗口兜底）。成功/失败均落 app_log（此前后台路径零日志）。
fn spawn_official_all_refresh(state: &AppState) {
    static LAST_BG_REFRESH: std::sync::Mutex<Option<std::time::Instant>> =
        std::sync::Mutex::new(None);
    const BG_REFRESH_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
    {
        let last = LAST_BG_REFRESH.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = *last {
            if t.elapsed() < BG_REFRESH_MIN_INTERVAL {
                return;
            }
        }
    }
    static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if IN_FLIGHT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    *LAST_BG_REFRESH.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    let st = state.clone();
    std::thread::spawn(move || {
        struct ResetInFlight;
        impl Drop for ResetInFlight {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _reset = ResetInFlight;
        let started = std::time::Instant::now();
        // panic 兜底（审查 P6，同 scheduler.rs 调度线程先例）：任务体 panic 照常落日志；
        // IN_FLIGHT 复位由上方 Drop 守卫保证，此处仅补可观测性
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            workbuddy_usage_official_all_impl(&st, true)
        }));
        match outcome {
            Ok(Ok(v)) => {
                // 全部账号失败时 fail 闭包带缓存回退返回 Ok(stale) 而非 Err，
                // 须按 stale 标记区分日志——否则内层「失败」与本处「完成」并存，
                // 语义矛盾误导排查（审查 P1）
                if v.get("stale").and_then(Value::as_bool).unwrap_or(false) {
                    let reason =
                        v.get("stale_reason").and_then(Value::as_str).unwrap_or("未知原因");
                    fs_utils::app_log(
                        &st.data_dir,
                        &format!(
                            "workbuddy: 全账号官方用量后台刷新失败（{}ms），已回退旧缓存：{reason}",
                            started.elapsed().as_millis()
                        ),
                    );
                } else {
                    fs_utils::app_log(
                        &st.data_dir,
                        &format!(
                            "workbuddy: 全账号官方用量后台刷新完成，共 {}ms",
                            started.elapsed().as_millis()
                        ),
                    );
                }
            }
            Ok(Err(e)) => fs_utils::app_log(
                &st.data_dir,
                &format!(
                    "workbuddy: 全账号官方用量后台刷新失败（{}ms）：{e}",
                    started.elapsed().as_millis()
                ),
            ),
            Err(_) => fs_utils::app_log(
                &st.data_dir,
                &format!(
                    "workbuddy: 全账号官方用量后台刷新线程 panic（已捕获，不影响下次触发）：{}ms",
                    started.elapsed().as_millis()
                ),
            ),
        }
    });
}

/// Buddy 会话明细查询（issue #82；行级缓存纯读零网络）。
/// 数据源为官网 get-user-request-usage 请求级明细落盘（随单账号/全账号官方用量
/// 拉取及调度器预热保鲜）；会话说明取上游 input/inputTrunc，token 列上游无字段 → 留空。
/// 账号清单以 token store 为准（已删除账号的残留桶不展示）。
#[tauri::command(async)]
pub fn workbuddy_sessions_query(
    state: State<AppState>,
    start: String,
    end: String,
    model: Option<String>,
    user_id: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
) -> Result<UsageSessionsResult, String> {
    let cache: Value = crate::store::db(&state.data_dir).kv_get("workbuddy_usage_sessions_cache");

    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(50).clamp(1, 200);
    let model_filter = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let uid_filter = user_id.as_deref().map(str::trim).filter(|u| !u.is_empty());

    let store: Value = crate::tasks::wb_common::load_token_store(&state);
    let token_ids: std::collections::BTreeSet<String> = store
        .get("tokens")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    // 缓存 fetched_at 为毫秒；契约统一为 Unix 秒（与 Trae usage_sessions_query 一致，审查 P3）
    let fetched_at = cache.get("fetched_at").and_then(Value::as_i64).unwrap_or(0) / 1000;

    let mut rows: Vec<UsageSessionView> = Vec::new();
    let mut models: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (acct_id, bucket) in cache.get("accounts").and_then(Value::as_object).into_iter().flatten() {
        if !token_ids.contains(acct_id) || uid_filter.is_some_and(|f| acct_id != f) {
            continue;
        }
        let name = bucket.get("name").and_then(Value::as_str).unwrap_or("");
        for r in bucket.get("rows").and_then(Value::as_array).into_iter().flatten() {
            let ts = r.get("ts").and_then(Value::as_i64).unwrap_or(0);
            // ts → 本地自然日，与 Trae 命令同口径（YYYY-MM-DD 字典序即时间序）
            let date = chrono::DateTime::from_timestamp(ts, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            if date.as_str() < start.as_str() || date.as_str() > end.as_str() {
                continue;
            }
            let m = r.get("model").and_then(Value::as_str).unwrap_or("未知模型");
            models.insert(m.to_string());
            if model_filter.is_some_and(|f| m != f) {
                continue;
            }
            rows.push(UsageSessionView {
                user_id: acct_id.clone(),
                account_name: name.to_string(),
                session_id: r.get("request_id").and_then(Value::as_str).unwrap_or("").to_string(),
                ts,
                date,
                model: m.to_string(),
                // Buddy 官网行级明细无 mode/cost/token 字段 → 全空（前端显示 —）；
                // 会话说明取上游 input/inputTrunc（旧缓存行无此字段时为空串）
                mode: String::new(),
                failed: None,
                credits: r.get("credits").and_then(Value::as_f64).unwrap_or(0.0),
                cost_money: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                preview: r.get("preview").and_then(Value::as_str).unwrap_or("").to_string(),
                record_id: String::new(),
            });
        }
    }

    rows.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.session_id.cmp(&a.session_id)));
    let total = rows.len() as u64;
    let credits_total = rows.iter().map(|r| r.credits).sum();
    // 页码夹取：越界页回夹到最后一页（数据减少后翻页不再出现空页）；乘法 u64 饱和
    let total_pages = total.div_ceil(page_size as u64).max(1) as u32;
    let page = page.min(total_pages);
    let begin = ((page as u64 - 1).saturating_mul(page_size as u64)) as usize;
    let rows: Vec<UsageSessionView> = rows
        .into_iter()
        .skip(begin)
        .take(page_size as usize)
        .collect();

    Ok(UsageSessionsResult {
        fetched_at,
        total,
        credits_total,
        page,
        page_size,
        rows,
        models: models.into_iter().collect(),
    })
}

/// requestId 缺失时的稳定兜底 key（不含任何敏感字段）。
/// `seq` = 批内行序号（G5）：仅按 (date, model, credit) 派生时，同日同模型同
/// 积分的相邻行键碰撞，`seen` 去重会静默漏计——追加批内序号保证唯一；
/// 正常 requestId 路径不经过本函数，不受影响
fn uuidless_key(date: &str, model: &str, credit: &f64, seq: u64) -> String {
    let mut h = Sha256::new();
    h.update(date.as_bytes());
    h.update(model.as_bytes());
    h.update(credit.to_le_bytes());
    h.update(seq.to_le_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("auto-{}", &hex[..16])
}

// ==================== 活动信息展示（T4.4/F-51） ====================

/// 活动信息三端点聚合（F-51，§7.1）：活动 banner（公开 GET）+ 付费类型 +
/// 用量提醒（billing POST）。低频附加展示：10min 缓存；端点失败不致命，
/// 逐项容错并记入 errors（宽容解析，字段缺失返回 null）。
// async：内含 3 次串行网络请求（各 10s 超时），同步命令会冻结 UI（审查修复）
#[tauri::command(async)]
pub fn workbuddy_activity_info(
    state: State<AppState>,
    user_id: Option<String>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let cache_path = "workbuddy_activity_cache"; // kv 键（SQLite 化 P2）

    // 选号逻辑与 workbuddy_usage_official 一致（复用同一降级链）
    //（审查 P1：选号解析提到缓存命中判断之前——缓存按账号区分，命中须同账号）
    let store: serde_json::Value = crate::tasks::wb_common::load_token_store(&state);
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let pick = |id: &str| -> Option<(String, String, String)> {
        let rec = tokens.get(id)?;
        let token = as_str(fs_utils::dig(&rec, &["access_token"]))?;
        if token.is_empty() {
            return None;
        }
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        Some((id.to_string(), token, domain))
    };
    let pool = load_pool(&state);
    let chosen = user_id
        .as_deref()
        .and_then(|uid| pick(uid))
        .or_else(|| {
            let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(&state));
            let fuid = as_str(fs_utils::dig(&raw, &["uid"]))?;
            pool.accounts.iter().find(|a| a.uid == fuid).map(|a| a.id.clone()).and_then(|id| pick(&id))
        })
        .or_else(|| pool.accounts.iter().find_map(|a| pick(&a.id)))
        .or_else(|| tokens.keys().find_map(|k| pick(k)));
    let resolved_acct = chosen.as_ref().map(|(id, _, _)| id.clone());

    // 缓存命中条件（审查 P1）：10min 内 + 缓存 account_id 与本次解析账号一致
    //（无凭证时解析结果为 None，与无凭证缓存 payload 的 account_id=null 对齐）
    if !refresh.unwrap_or(false) {
        let cached: serde_json::Value = crate::store::db(&state.data_dir).kv_get(cache_path);
        let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
        let cached_acct = cached.get("account_id").and_then(Value::as_str);
        if cached.get("account_id").is_some()
            && cached_acct == resolved_acct.as_deref()
            && chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000
        {
            return Ok(cached);
        }
    }

    let Some((acct_id, token, domain)) = chosen else {
        // 无凭证：banner 仍可拉（公开端点），付费类型/用量提醒跳过
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
        let banners = fetch_activity_banners(&agent, "https://www.workbuddy.cn");
        return Ok(serde_json::json!({
            "account_id": null, "payment_type": null, "dosage_notify": null,
            "banners": banners, "errors": [], "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        }));
    };

    // 区域路由（F-36，§5.2）：billing/activity 随账号 domain（域名收敛 §4.5 单点化）
    let base = crate::tasks::wb_common::region_activity_base(&domain).to_string();
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let mut errors: Vec<String> = vec![];

    let banners = fetch_activity_banners(&agent, &base);

    // 付费类型：POST /v2/billing/meter/get-payment-type → data.paymentType
    let payment_type = match billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-payment-type"), &token) {
        // dig 候选键语义：自动穿透 data 信封，候选里不写 "data"（同 backfill 处注释）
        Ok(v) => as_str(fs_utils::dig(&v, &["paymentType", "payment_type"]))
            .filter(|s| !s.is_empty() && *s != "unknown")
            .map(|s| s.to_string()),
        Err(e) => {
            errors.push(format!("payment-type: {e}"));
            None
        }
    };

    // 用量提醒：POST /v2/billing/meter/get-dosage-notify（宽容透传 data 内容）
    let dosage_notify = match billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-dosage-notify"), &token) {
        Ok(v) => {
            let data = v.get("data").cloned().unwrap_or(Value::Null);
            if data.is_null() { None } else { Some(data) }
        }
        Err(e) => {
            errors.push(format!("dosage-notify: {e}"));
            None
        }
    };

    let payload = serde_json::json!({
        "account_id": acct_id,
        "payment_type": payment_type,
        "dosage_notify": dosage_notify,
        "banners": banners,
        "errors": errors,
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
    });
    let _ = crate::store::db(&state.data_dir).kv_set(cache_path, &payload);
    Ok(payload)
}

/// 活动 banner：公开 GET /v2/activity/banner（宽容解析 banners/banner/list 数组）
fn fetch_activity_banners(agent: &ureq::Agent, base: &str) -> Vec<Value> {
    let resp = agent.get(&format!("{base}/v2/activity/banner")).call();
    let v: Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(_) => return vec![],
    };
    // dig 候选键语义：自动穿透 data 信封，候选里不写 "data"（否则命中整个信封对象，as_array 恒 None）
    let arr = fs_utils::dig(&v, &["banners"])
        .and_then(|x| x.as_array().cloned())
        .or_else(|| fs_utils::dig(&v, &["banner"]).and_then(|x| x.as_array().cloned()))
        .or_else(|| fs_utils::dig(&v, &["list"]).and_then(|x| x.as_array().cloned()))
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    // 只保留展示字段，剥离未知字段
    arr.iter()
        .filter(|b| b.is_object())
        .map(|b| {
            serde_json::json!({
                "title": as_str(fs_utils::dig(b, &["title", "name"])).unwrap_or_default(),
                "content": as_str(fs_utils::dig(b, &["content", "description", "desc"])).unwrap_or_default(),
                "url": as_str(fs_utils::dig(b, &["url", "link", "jump_url", "jumpUrl"])).unwrap_or_default(),
                "start_time": as_str(fs_utils::dig(b, &["start_time", "startTime"])).unwrap_or_default(),
                "end_time": as_str(fs_utils::dig(b, &["end_time", "endTime"])).unwrap_or_default(),
            })
        })
        .collect()
}

/// billing POST（Bearer + web 平台头，脱敏红线：不携带 X-Refresh-Token）
fn billing_post_json(agent: &ureq::Agent, url: &str, token: &str) -> Result<Value, String> {
    let resp = agent
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("X-Client-Platform", "web")
        .set("Content-Type", "application/json")
        .send_string("{}");
    match resp {
        Ok(r) => r.into_json().map_err(|e| format!("响应解析失败: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(test)]
mod credits_tests {
    use super::*;

    #[test]
    fn parse_reward_plus_extracts_amounts() {
        assert_eq!(parse_reward_plus("签到成功 +10"), Some(10.0));
        assert_eq!(parse_reward_plus("签到成功 +2.5"), Some(2.5));
        assert_eq!(parse_reward_plus("签到成功"), None);
        assert_eq!(parse_reward_plus("已签到"), None);
        assert_eq!(parse_reward_plus("签到成功 +0"), None);
    }

    // ── usage_row_ts（issue #82 Buddy 会话明细：requestTime → Unix 秒）────────

    #[test]
    fn usage_row_ts_parses_epoch_rfc3339_and_local_formats() {
        use chrono::TimeZone;
        let local = |h: u32, m: u32, s: u32| -> i64 {
            chrono::Local
                .from_local_datetime(
                    &chrono::NaiveDate::from_ymd_opt(2026, 10, 11)
                        .unwrap()
                        .and_hms_opt(h, m, s)
                        .unwrap(),
                )
                .single()
                .unwrap()
                .timestamp()
        };

        // epoch 秒 / 毫秒
        assert_eq!(usage_row_ts(&serde_json::json!({ "requestTime": local(8, 30, 0) })), Some(local(8, 30, 0)));
        assert_eq!(
            usage_row_ts(&serde_json::json!({ "requestTime": local(8, 30, 0) * 1000 })),
            Some(local(8, 30, 0))
        );
        // RFC3339
        assert_eq!(
            usage_row_ts(&serde_json::json!({ "request_time": "2026-10-11T00:30:00Z" })),
            Some(chrono::DateTime::parse_from_rfc3339("2026-10-11T00:30:00Z").unwrap().timestamp())
        );
        // 本地时间串
        assert_eq!(usage_row_ts(&serde_json::json!({ "requestTime": "2026-10-11 08:30:00" })), Some(local(8, 30, 0)));
        // date-only 回退当日零点
        assert_eq!(usage_row_ts(&serde_json::json!({ "requestTime": "2026-10-11" })), Some(local(0, 0, 0)));
        // 不可解析 / 缺字段
        assert_eq!(usage_row_ts(&serde_json::json!({ "requestTime": "nonsense" })), None);
        assert_eq!(usage_row_ts(&serde_json::json!({})), None);
    }

    // ── usage_row_preview（会话说明：官网行 input/inputTrunc 接入 preview）────────

    #[test]
    fn usage_row_preview_prefers_trunc_then_input() {
        // inputTrunc 优先（官网已截断形态）
        let item = serde_json::json!({
            "inputTrunc": "截断版说明",
            "input": "完整版说明，很长很长的原始用户输入"
        });
        assert_eq!(usage_row_preview(&item), "截断版说明");
        // inputTrunc 缺失 → input
        let item = serde_json::json!({ "input": "只有完整版" });
        assert_eq!(usage_row_preview(&item), "只有完整版");
        // inputTrunc 空串 → 跳过取 input
        let item = serde_json::json!({ "inputTrunc": "", "input": "兜底完整" });
        assert_eq!(usage_row_preview(&item), "兜底完整");
    }

    #[test]
    fn usage_row_preview_flattens_newlines_and_caps_200_chars() {
        // 换行/回车压平为空格
        let item = serde_json::json!({ "input": "第一行\n第二行\r\n第三行" });
        assert_eq!(usage_row_preview(&item), "第一行 第二行 第三行");
        // 200 字符截断（按字符不按字节，中文安全）
        let long = "啊".repeat(250);
        let item = serde_json::json!({ "input": long });
        let out = usage_row_preview(&item);
        assert_eq!(out.chars().count(), 200);
    }

    #[test]
    fn usage_row_preview_empty_without_any_field() {
        assert_eq!(usage_row_preview(&serde_json::json!({ "credit": 0.1 })), "");
        // agentPurpose 仅在有值时参与（空串跳过）
        let item = serde_json::json!({ "agentPurpose": "通用问答" });
        assert_eq!(usage_row_preview(&item), "通用问答");
    }

    // ── earliest_pack_expire（issue #28：Buddy 调度到期派生）────────────────

    #[test]
    fn earliest_pack_expire_takes_min_of_valid_packs() {
        let acc = serde_json::json!({"packages": [
            {"remaining": 10.0, "expire_ts": 2000},
            {"remaining": 5.0, "expire_ts": 1000}
        ]});
        assert_eq!(earliest_pack_expire(&acc, 500), Some(1000));
    }

    #[test]
    fn earliest_pack_expire_skips_drained_expired_and_no_expiry() {
        // 已用完 / 已过期 / 无到期时间（长期有效）的包均不参与，Buddy 不分包类型
        let acc = serde_json::json!({"packages": [
            {"remaining": 0.0, "expire_ts": 900},
            {"remaining": 8.0, "expire_ts": 400},
            {"remaining": 8.0},
            {"remaining": 3.0, "expire_ts": 1500}
        ]});
        assert_eq!(earliest_pack_expire(&acc, 500), Some(1500));
    }

    #[test]
    fn earliest_pack_expire_none_without_valid_packages() {
        assert_eq!(
            earliest_pack_expire(&serde_json::json!({"packages": []}), 500),
            None
        );
        assert_eq!(earliest_pack_expire(&serde_json::json!({}), 500), None);
    }

    // ── accounts_fingerprint（2026-10-06：账号增删日差分可比性守卫）────────────

    #[test]
    fn accounts_fingerprint_sorts_dedups_and_ignores_order() {
        let a = serde_json::json!([{"user_id": "wb-b"}, {"user_id": "wb-a"}]);
        let b = serde_json::json!([{"user_id": "wb-a"}, {"user_id": "wb-b"}]);
        assert_eq!(
            accounts_fingerprint(a.as_array().unwrap()),
            accounts_fingerprint(b.as_array().unwrap())
        );
        assert_eq!(
            accounts_fingerprint(a.as_array().unwrap()),
            Some(vec!["wb-a".to_string(), "wb-b".to_string()])
        );
    }

    #[test]
    fn accounts_fingerprint_none_without_details_and_detects_set_change() {
        // 无账号明细（空数组 / 缺字段）= 不可比信息缺失，返回 None 由调用方维持既有行为
        assert_eq!(accounts_fingerprint(&[]), None);
        assert_eq!(
            snapshot_fingerprint(&serde_json::json!({"date": "2026-10-06"})),
            None
        );
        // 加号 / 换号（同数量不同集合）都必须判为集合变化
        let prev = serde_json::json!({"accounts": [{"user_id": "wb-a"}, {"user_id": "wb-b"}]});
        let added = serde_json::json!({"accounts": [{"user_id": "wb-a"}, {"user_id": "wb-b"}, {"user_id": "wb-c"}]});
        let swapped = serde_json::json!({"accounts": [{"user_id": "wb-a"}, {"user_id": "wb-c"}]});
        assert_ne!(snapshot_fingerprint(&prev), snapshot_fingerprint(&added));
        assert_ne!(snapshot_fingerprint(&prev), snapshot_fingerprint(&swapped));
        assert_eq!(
            snapshot_fingerprint(&prev),
            snapshot_fingerprint(&serde_json::json!({
                "accounts": [{"user_id": "wb-b", "balance": 1.0}, {"user_id": "wb-a", "balance": 2.0}]
            }))
        );
    }
}
