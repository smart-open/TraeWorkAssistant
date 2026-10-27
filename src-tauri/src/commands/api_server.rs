use std::sync::{Arc, Mutex};

use tauri::{Manager, State};

use crate::fs_utils;
use crate::models::{
    AccountCooldownsFile, ApiPoolFile, ApiServiceStatus, DeviceMap, PoolStatus,
    RemainingCreditsFile,
};
use crate::state::AppState;

use crate::api_server::models_sync;
use crate::api_server::pool::ApiPool;
use crate::api_server::server::{start_api_server, ApiServerHandle};
use crate::api_server::{ApiLogger, ApiSharedState};

/// 运行时状态：服务器句柄 + 共享状态
pub struct ApiServerRuntime {
    pub handle: ApiServerHandle,
    pub shared: Arc<ApiSharedState>,
    pub started_at: u64,
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// api_pool 整文档覆盖写互斥（审查修复清单 v1.4.9 B1，进程级叶子锁）：
/// api_pool 为 kv 整文档「load→merge→kv_set」模型，3 个写点
/// （pool_set / shared_groups delete ③段 / tags delete ③段）无互斥时并发互相
/// 覆盖丢配置。纪律：写点读改写+落盘持锁、热重载放锁外；写点均在池/org 锁
/// 释放后获取本锁，持锁期间严禁再取任何其他锁；启动迁移在 server bind 前执行
/// 无并发窗口，不取锁
pub(crate) static POOL_CFG_LOCK: Mutex<()> = Mutex::new(());

// ==================== 启停命令 ====================

/// 启动 API 服务核心逻辑（页面命令 / 托盘菜单共用）。
/// do_start 的进程级启动闸门（L 修复）：「检查已运行 → 长时间 await 启动 → 写入
/// 句柄」之间的 TOCTOU——并发双启动此前会双双通过检查。占位失败方直接报已运行。
static DO_START_GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动本地 LLM 网关服务（带 TOCTOU 闸门包装，见 DO_START_GATE）。
/// 成功后同步托盘菜单文本并发送系统通知。
pub async fn do_start(
    app: &tauri::AppHandle,
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) -> Result<ApiServiceStatus, String> {
    if DO_START_GATE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("API 服务已在运行或正在启动".into());
    }
    let out = do_start_inner(app, state, runtime).await;
    DO_START_GATE.store(false, std::sync::atomic::Ordering::SeqCst);
    out
}

async fn do_start_inner(
    app: &tauri::AppHandle,
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) -> Result<ApiServiceStatus, String> {
    // 检查是否已运行
    {
        let guard = safe_lock(runtime);
        if guard.is_some() {
            return Err("API 服务已在运行".into());
        }
    }

    // 网关设置（§8.1/§9.2）：port / default_model 改读 data/api_gateway_settings.json；
    // 新文件缺失时从 app_settings.json 旧字段一次性迁移（旧字段保留不删，防回滚）。
    // load 已将空 default_model 兜底为内置默认，无需再 trim 判空
    let gw = crate::api_server::gateway_settings::load(&state.data_dir);
    let port = gw.port;

    // 代理循环说明：ureq 2.12 未启用 proxy-from-env feature，构建 Agent 时
    // 既不读 HTTP(S)_PROXY/NO_PROXY 环境变量、也不读系统代理，Agent 未显式
    // 配置 proxy 即直连，不会形成 127.0.0.1:8899 回环。
    // 旧实现曾进程级 set_var("NO_PROXY","*")：多线程下 setenv 有竞态
    // （Rust 2024 已标 unsafe），且污染 python 签到等子进程的代理行为，已移除
    let default_model = gw.default_model;

    // 读取账号数据、冷却状态、剩余积分（账号经 vault 解密还原明文 jwt）
    let accounts = crate::vault::load_accounts(state);
    // SQLite 化（P2）：api_pool.json → kv `api_pool`（含 Buddy 旧共享值迁移）
    let pool_file = load_pool_file(&state.data_dir);
    // SQLite 化（P3）：groups/cooldowns/remaining_credits/device_map 经 store 读取
    let groups_file: crate::models::GroupsFile =
        crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));

    // 调度策略（T10）：api_pool.json.strategy，空/未知值回退 expire_first
    let strategy = crate::api_server::pool::PoolStrategy::parse(&pool_file.strategy);

    // ===== 启动诊断日志：详细记录账号池资源情况 =====
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-账号池诊断: total_accounts={} enabled_in_pool={} strategy={} group_filter={} cooldowns={} credits_entries={}",
            accounts.accounts.len(),
            pool_file.enabled_uids.len(),
            strategy.as_str(),
            if pool_file.group_ids.is_empty() { "all".to_string() } else { pool_file.group_ids.join(",") },
            cooldowns_file.cooldowns.len(),
            credits_file.credits.len(),
        ),
    );

    // 逐账号诊断：哪些会被加入池，哪些会被跳过及原因
    let enabled_set: std::collections::HashSet<&str> =
        pool_file.enabled_uids.iter().map(|s| s.as_str()).collect();
    let group_filter: Option<std::collections::HashSet<&str>> = if pool_file.group_ids.is_empty() {
        None
    } else {
        Some(pool_file.group_ids.iter().map(|s| s.as_str()).collect())
    };
    // 诊断口径与池装配一致：通用积分余额/到期（老缓存账号回退混合口径）
    let merged_expires = merge_pool_expire_times(&credits_file);
    // 同 uid 重复条目告警：服务端同 uid 即同一账号，池装配与凭证更新仅按 uid 单条生效，
    // 多条目会加剧「多账号」错觉（issue #80）；建议保留一条、删除多余
    let mut seen_uids: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for a in &accounts.accounts {
        if let Some(uid) = a.user_id.as_deref() {
            match seen_uids.get(uid) {
                Some(first_name) => fs_utils::app_log(
                    &state.data_dir,
                    &format!(
                        "  账号池告警: 发现重复 uid={}（已有账号 [{}]，当前账号 [{}]）；池装配与凭证更新仅按 uid 单条生效，建议保留一条、删除多余条目",
                        uid, first_name, a.name
                    ),
                ),
                None => {
                    seen_uids.insert(uid, a.name.as_str());
                }
            }
        }
    }
    for a in &accounts.accounts {
        let uid = a.user_id.as_deref().unwrap_or("(none)");
        let name = &a.name;
        if !enabled_set.contains(uid) {
            fs_utils::app_log(
                &state.data_dir,
                &format!("  账号池跳过: name={} uid={} reason=not_in_enabled_list", name, uid),
            );
        } else if !group_filter.as_ref().map_or(true, |f| {
            groups_file.membership.get(uid).map_or(false, |g| f.contains(g.as_str()))
        }) {
            fs_utils::app_log(
                &state.data_dir,
                &format!("  账号池跳过: name={} uid={} reason=not_in_selected_group", name, uid),
            );
        } else {
            // 检查冷却和积分状态
            let cd = cooldowns_file.cooldowns.get(uid);
            let credits = credits_file
                .general
                .get(uid)
                .copied()
                .or_else(|| credits_file.credits.get(uid).copied());
            let expire = merged_expires.get(uid).copied();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let status = if cd.map_or(false, |c| c.error_type == "SessionDead") {
                "SessionDead(disabled)".to_string()
            } else if cd.map_or(false, |c| c.until > 0 && now < c.until) {
                format!("cooldown(remaining={}s)", cd.unwrap().until - now)
            } else if let Some(exp) = expire {
                if exp > 0 && exp < now {
                    "credits_expired".to_string()
                } else if let Some(c) = credits {
                    if c <= 0.0 {
                        "zero_credits".to_string()
                    } else {
                        "healthy".to_string()
                    }
                } else {
                    "healthy(no_credits_info)".to_string()
                }
            } else {
                "healthy(no_expiry)".to_string()
            };
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "  账号池纳入: name={} uid={} credits={} expire={} status={}",
                    name, uid,
                    credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "None".into()),
                    expire.map(|e| e.to_string()).unwrap_or_else(|| "None".into()),
                    status,
                ),
            );
        }
    }

    // 创建池并同步（装配逻辑抽公共函数：do_start 与凭据变更热重载共用，见 apply_pool_snapshot）
    let pool = ApiPool::new();
    pool.set_strategy(strategy);
    let wb_pool = ApiPool::new();
    let qoder_pool = ApiPool::new();
    let buddy_ai_pool = ApiPool::new();
    // 冷却/禁用标记落 app.log（F-80）：四池注入 (data_dir, 池标签)，
    // note_error 打冷却/禁用时同步写 `account cooldown: pool=…` 便于排查跨池回退链路
    pool.set_log_ctx(&state.data_dir, "trae");
    wb_pool.set_log_ctx(&state.data_dir, "buddy");
    qoder_pool.set_log_ctx(&state.data_dir, "qoder");
    buddy_ai_pool.set_log_ctx(&state.data_dir, "buddy-ai");
    // 账号保留剩余积分（issue #91）：调度策略统一配置，启动时四池应用
    //（credits=0 时无过滤；Qoder 池 credits 恒 None 不受影响，保持口径一致仍设置。
    // M3 审查修复：buddy-ai 池补齐——原先冷启动漏设，调度策略保存前启动的服务
    // 国际档不过滤低余额账号，与 dispatch_policy_set 热应用「四池即时生效」不一致）
    let reserve_credits = crate::api_server::dispatch::load_policy(&state.data_dir).reserve_credits;
    pool.set_reserve_credits(reserve_credits);
    wb_pool.set_reserve_credits(reserve_credits);
    qoder_pool.set_reserve_credits(reserve_credits);
    buddy_ai_pool.set_reserve_credits(reserve_credits);
    let (pool_count, wb_uids_len, wb_accounts_total, qoder_count) =
        apply_pool_snapshot(state, &pool, &wb_pool, &qoder_pool, &buddy_ai_pool);

    let healthy_count = pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-池状态: pool_size={} healthy={} port={}",
            pool_count, healthy_count, port,
        ),
    );

    // Buddy 池策略：wb_strategy 独立配置优先；空 = 跟随 Trae 池（与 pool_set 热应用逻辑一致）
    let wb_strategy = crate::api_server::pool::PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy);
    let wb_healthy = wb_pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-WB上游池: enabled={} accounts={} healthy={} strategy={} whitelist={} (total_accounts={})",
            pool_file.wb_enabled,
            wb_pool.count(),
            wb_healthy,
            wb_strategy.as_str(),
            wb_uids_len,
            wb_accounts_total,
        ),
    );
    wb_pool.set_strategy(wb_strategy);

    // Qoder 池启动概况（p3-3 + 池内策略）：开关默认 false，未启用时不产生告警噪音；
    // qoder_strategy 空 = 跟随 Trae 池策略（与 Buddy 池 resolve_wb 同语义）
    let qoder_strategy =
        crate::api_server::pool::PoolStrategy::resolve_qoder(&pool_file.strategy, &pool_file.qoder_strategy);
    qoder_pool.set_strategy(qoder_strategy);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-Qoder上游池: enabled={} accounts={} strategy={}",
            pool_file.qoder_enabled,
            qoder_count,
            qoder_strategy.as_str(),
        ),
    );

    // buddy-ai 国际池启动概况（池内策略：buddy_ai_strategy 空 = 跟随 Trae 池，
    // 与 Buddy/Qoder 池同语义；开关默认 false，未启用时不产生告警噪音）
    let buddy_ai_strategy = crate::api_server::pool::PoolStrategy::resolve_buddy_ai(
        &pool_file.strategy,
        &pool_file.buddy_ai_strategy,
    );
    buddy_ai_pool.set_strategy(buddy_ai_strategy);
    // G7 审查修复：补 accounts 计数——WB/Qoder 池启动日志均带账号数，buddy-ai 池
    // 原先只记开关与策略，导入/同步是否落号无法从启动日志确认（排查盲区）
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-BuddyAI国际池: enabled={} accounts={} strategy={}",
            pool_file.buddy_ai_enabled,
            buddy_ai_pool.count(),
            buddy_ai_strategy.as_str(),
        ),
    );

    // 池为空时给出明确警告：分别指明是哪个池——Trae 池空 ≠ 全部资源不可用，
    // Buddy(WB) 池可能正常服务（2026-09-20 实测反馈：警告误导排查方向）
    let wb_pool_count = wb_pool.count(); // ApiPool 非 Copy：移入 shared 前取值
    let wb_summary = format!(
        "Buddy(WB)池 enabled={} accounts={} healthy={}",
        pool_file.wb_enabled,
        wb_pool_count,
        wb_healthy
    );
    if pool_count == 0 {
        if pool_file.wb_enabled && wb_pool.count() > 0 {
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "警告: Trae 模型池为空（Trae 池请求不可用），{wb_summary} 可正常服务。\
如需 Trae 池，请在API服务页面勾选 Trae 账号并保存。",
                ),
            );
        } else {
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "警告: 全部账号池为空或未启用！Trae 池: 0 个账号；{wb_summary}。\
请在API服务页面勾选账号并保存后再启动。",
                ),
            );
        }
    } else if healthy_count == 0 {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "警告: Trae 模型池中无健康账号（冷却/积分过期/SessionDead），{wb_summary}。\
请检查 Trae 账号状态或清除冷却。",
            ),
        );
    }

    // 指纹解析 data_dir 注册（issue #123）：settings 自定义路径读取需要，
    // 必须在首个 wb_client_fingerprint 调用（api 启动预检）前完成
    crate::api_server::wb_upstream::set_fingerprint_data_dir(&state.data_dir);

    let shared = Arc::new(ApiSharedState {
        pool,
        wb_pool,
        fallback_log_last_ms: std::sync::Mutex::new(std::collections::HashMap::new()),
        wb_enabled: std::sync::atomic::AtomicBool::new(pool_file.wb_enabled),
        wb_sanitize: std::sync::atomic::AtomicBool::new(true),
        // T5.3/T5.5/T5.6③ 开关（api_pool.json，serde default 兼容旧文件）
        wb_default_thinking: std::sync::atomic::AtomicBool::new(pool_file.wb_default_thinking),
        wb_tool_exec: std::sync::atomic::AtomicBool::new(pool_file.wb_tool_exec),
        wb_bg_downgrade: std::sync::atomic::AtomicBool::new(pool_file.wb_bg_downgrade),
        // F-76/F-77 新开关（api_pool.json，serde default 兼容旧文件）
        wb_longctx_downgrade: std::sync::atomic::AtomicBool::new(pool_file.wb_longctx_downgrade),
        wb_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(pool_file.wb_hedge_threshold_ms),
        // Trae 池竞速对冲阈值（F-76③ 同构，per-pool 独立配置）
        trae_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(pool_file.trae_hedge_threshold_ms),
        // per-pool 三参数：池粘性 TTL 三池各自配置（record_sticky 按胜出池取值）
        trae_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(
            pool_file.trae_pool_sticky_ttl_secs,
        ),
        wb_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(
            pool_file.wb_pool_sticky_ttl_secs,
        ),
        qoder_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(
            pool_file.qoder_pool_sticky_ttl_secs,
        ),
        buddy_ai_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(
            pool_file.buddy_ai_pool_sticky_ttl_secs,
        ),
        buddy_ai_sticky_enabled: std::sync::atomic::AtomicBool::new(
            pool_file.buddy_ai_sticky_enabled,
        ),
        buddy_ai_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(
            pool_file.buddy_ai_hedge_threshold_ms,
        ),
        // Qwen3.8-Flash 免费活动倍率覆盖（issue #111，api_pool.json，serde default 兼容旧文件）
        qoder_flash_free_rate: std::sync::atomic::AtomicBool::new(pool_file.qoder_flash_free_rate),
        wb_sticky: crate::api_server::wb_sticky::StickyStore::load(&state.data_dir),
        // Trae 池会话粘性存储（per-pool 批次新增）："t:" 命名空间与 wb/qoder 互不串绑
        trae_sticky: crate::api_server::wb_sticky::StickyStore::load_ns(&state.data_dir, "t:"),
        // buddy-ai 显式会话粘性存储（§4.2）："g:" 命名空间与 wb（无前缀）/q:"/t:" 互不串绑
        buddy_ai_sticky: crate::api_server::wb_sticky::StickyStore::load_ns(&state.data_dir, "g:"),
        // buddy-ai 独立池（§3.4 隔离清单）：开关默认关（dispatch 显式开启制）
        buddy_ai_pool,
        buddy_ai_enabled: std::sync::atomic::AtomicBool::new(pool_file.buddy_ai_enabled),
        listen_host: std::sync::Mutex::new("127.0.0.1".to_string()),
        pool_sticky: Mutex::new(std::collections::HashMap::new()),
        custom_upstream_sticky: Mutex::new(std::collections::HashMap::new()),
        custom_lb_counters: Mutex::new(std::collections::HashMap::new()),
        model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        buddy_ai_model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        default_model,
        data_dir: state.data_dir.clone(),
        total_requests: std::sync::atomic::AtomicU64::new(0),
        inflight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        active_uid: Mutex::new(None),
        last_error: Mutex::new(None),
        logger: ApiLogger::new(state.logs_dir()),
        debug_enabled: std::sync::atomic::AtomicBool::new(false),
        usage: Mutex::new(crate::api_server::usage::load(&state.data_dir)),
        usage_dirty: Mutex::new(Vec::new()),
        wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        buddy_ai_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        buddy_ai_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        // Trae 401 自愈回调（issue #27 方案 B）：网关层无 AppState，闭包借 AppHandle
        // 每次取 State 调 refresh_jwt_impl(force=true)（全防护：锁/冷却/轮换/失效标记）
        trae_jwt_refresh: Some({
            let app = app.clone();
            std::sync::Arc::new(move |uid: &str| {
                let st = app.state::<AppState>();
                crate::commands::accounts::refresh_jwt_impl(&st, uid, true)
            })
        }),
        // Qoder 上游（p3-3）：专用池 + identity 回调注入。回调借 AppHandle 每次
        // 取 State 走 ensure_fresh 全防护链路（H-1 串行化锁 + PAT/客户端双通道
        // 惰性刷新 + 设备指纹合并注入）；凭证缺失返回 Err → 网关侧 SessionDead
        // 语义（禁用换号），未启用 Qoder 时回调不触发
        trae_enabled: std::sync::atomic::AtomicBool::new(pool_file.trae_enabled),
        qoder_pool,
        qoder_enabled: std::sync::atomic::AtomicBool::new(pool_file.qoder_enabled),
        qoder_identity: Some({
            let app = app.clone();
            std::sync::Arc::new(move |acct_id: &str| {
                let st = app.state::<AppState>();
                // 请求路径临期窗口 1h：1 小时内到期的令牌先刷再用，避免带着
                // 临期令牌发请求吃 401（对齐 qoder_refresh LAZY_HOURS>6 的调度
                // 语义，网关请求路径取更激进的短窗口）
                // 带超时 agent（整体 20s，审查修复）：ensure_fresh 临期时真发刷新
                // 请求，裸 Agent::new() 无超时会在 H-1 串行锁内挂起并阻塞同账号
                // 后续刷新
                let agent = crate::tasks::http_agent(20);
                let (creds, _, status) =
                    crate::tasks::qoder_common::ensure_fresh(&st, &agent, acct_id, 1);
                if creds.access_token.is_empty() {
                    // 审查修复：暂态失败（网络/5xx → refresh_failed）带标记前缀，
                    // 网关侧走 Server 熔断可自愈；仅永久失效（凭证缺失/过期需重登/
                    // PAT 被拒/refresh_token 被拒）走 SessionDead 永久禁用
                    return Err(match status {
                        // refresh_busy（H3）：跨进程刷新锁被他进程持有——暂态，
                        // 必须与 refresh_failed 同走熔断可自愈，否则会被误标
                        // SessionDead 永久禁用（误标恰是 H3 要消除的后果）
                        "refresh_failed" | "refresh_busy" => format!(
                            "{}qoder 凭证暂不可用(id={acct_id}, status={status}；网络/服务端暂态或他进程正在刷新，稍后自动恢复)",
                            crate::tasks::qoder_common::TRANSIENT_ERR_TAG
                        ),
                        _ => format!("qoder 凭证缺失(id={acct_id}, status={status})"),
                    });
                }
                Ok(creds)
            })
        }),
        // F-80-余 v2：Qoder 竞速对冲阈值 + 会话粘性（开关默认关，粘性绑定
        // 落 sticky_bindings 表 "q:" 命名空间，与 WB 互不串绑）
        qoder_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(
            pool_file.qoder_hedge_threshold_ms,
        ),
        qoder_sticky_enabled: std::sync::atomic::AtomicBool::new(pool_file.qoder_sticky_enabled),
        qoder_sticky: crate::api_server::wb_sticky::StickyStore::load_ns(&state.data_dir, "q:"),
    });

    // per-pool 三参数热应用（F-76②/F-77 拆分版）：账号并发上限 / 池粘性 TTL /
    // 显式会话粘性 TTL 三池各自配置，启动时从 api_pool.json 读入
    shared
        .pool
        .set_concurrency_limit(pool_file.trae_account_concurrency_limit);
    shared
        .wb_pool
        .set_concurrency_limit(pool_file.wb_account_concurrency_limit);
    shared
        .qoder_pool
        .set_concurrency_limit(pool_file.qoder_account_concurrency_limit);
    shared
        .buddy_ai_pool
        .set_concurrency_limit(pool_file.buddy_ai_account_concurrency_limit);
    shared
        .buddy_ai_sticky
        .set_explicit_ttl(pool_file.buddy_ai_sticky_ttl_secs as i64);
    shared
        .trae_sticky
        .set_explicit_ttl(pool_file.trae_sticky_ttl_secs as i64);
    shared
        .wb_sticky
        .set_explicit_ttl(pool_file.wb_sticky_ttl_secs as i64);
    shared
        .qoder_sticky
        .set_explicit_ttl(pool_file.qoder_sticky_ttl_secs as i64);

    // 审查修复（L10）：启动闸门内二次检查——首次检查（函数开头）后即释放锁，
    // 至此处的池装配/配置装载均为耗时操作，期间并发 do_start（页面命令/托盘菜单/
    // 自启动路径）可能都通过首检导致重复启动尝试；bind 前持锁复查，已被并发
    // 实例抢先则直接报「已在运行」退出
    {
        let guard = safe_lock(runtime);
        if guard.is_some() {
            return Err("API 服务已在运行".into());
        }
    }

    let handle = start_api_server(port, shared.clone()).await?;

    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API 服务已启动: port={} 资源概况: Trae池 accounts={} healthy={} | {}",
            port, pool_count, healthy_count, wb_summary
        ),
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let status = ApiServiceStatus {
        running: true,
        port,
        total_requests: 0,
        active_uid: None,
        last_error: None,
        started_at: Some(now),
    };

    *safe_lock(runtime) = Some(ApiServerRuntime {
        handle,
        shared: shared.clone(),
        started_at: now,
    });

    // 启动积分保鲜（issue #91 缓存问题；审查 C3）：池内存在陈旧/缺时间戳的
    // 积分快照时后台刷新一次——reserve 过滤对陈旧快照 fail-open，刷新拨新后
    // 自动恢复生效。三池口径：
    //   Trae 池：refresh_remaining_credits_impl 逐账号刷新，经 push_credits 回写拨新；
    //   WB 池：wb_credits_snapshot_task 快照后经 write_back_pool_balances 内部
    //     runtime 句柄 update_credits_snapshot 三键拨新（issue #109 同类：
    //     credits/expire/fetched_at，ok=true 账号，失败账号保留旧值）；
    //   Qoder 池（issue #102）：run_snapshot_task 快照后按 ok 行经
    //     update_credits_snapshot 三键拨新（credits/expire_at/fetched_at）——
    //     selectable 零积分与到期过滤均无新鲜度自愈，装配时 Some(0.0)/已过期的
    //     账号在充值或续期后需拨新才恢复可选（此前注释「恒 credits=None 无保鲜
    //     需求」随三键透传已过时）。
    // 静默失败不影响启动流程
    {
        let app2 = app.clone();
        let st2 = state.clone();
        std::thread::spawn(move || {
            let rt_state = app2.state::<Mutex<Option<ApiServerRuntime>>>();
            let Some(shared) = safe_lock(&rt_state).as_ref().map(|rt| rt.shared.clone()) else {
                return; // runtime 未就绪（启动中途失败）
            };
            let trae_stale = shared.pool.has_stale_credits();
            let wb_stale = shared.wb_pool.has_stale_credits();
            let qoder_stale = shared.qoder_pool.has_zero_or_expired_credits();
            if !trae_stale && !wb_stale && !qoder_stale {
                return;
            }
            fs_utils::app_log(&st2.data_dir, "API服务启动-积分保鲜: 检测到陈旧积分快照, 后台刷新中");
            if trae_stale {
                match crate::commands::accounts::refresh_remaining_credits_impl(
                    &st2,
                    Some(rt_state.inner()),
                ) {
                    Ok(n) => fs_utils::app_log(
                        &st2.data_dir,
                        &format!("API服务启动-积分保鲜: Trae池刷新完成 accounts={}", n),
                    ),
                    Err(e) => fs_utils::app_log(
                        &st2.data_dir,
                        &format!("API服务启动-积分保鲜: Trae池刷新失败(不影响使用) {}", e),
                    ),
                }
            }
            if wb_stale {
                // 拨新在 wb_credits_snapshot_task 内部经 runtime 完成（ok 行三键拨新，
                // 对齐下方 Qoder 池口径）
                match crate::commands::workbuddy::wb_credits_snapshot_task(&st2, Some(rt_state.inner()))
                {
                    Ok(parsed) => {
                        // 计 ok=true 且 balance 可解析（实际拨新）账号，对齐
                        // write_back_pool_balances 的 snapshot 收集口径（Trae(ok_count)/Qoder(ok 行)）
                        let n = parsed
                            .get("accounts")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter(|x| {
                                        x.get("ok").and_then(|v| v.as_bool()).unwrap_or(true)
                                            && x.get("balance").and_then(|v| v.as_f64()).is_some()
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        fs_utils::app_log(
                            &st2.data_dir,
                            &format!("API服务启动-积分保鲜: WB池拨新完成 accounts={}", n),
                        );
                    }
                    Err(e) => fs_utils::app_log(
                        &st2.data_dir,
                        &format!("API服务启动-积分保鲜: WB池刷新失败(不影响使用) {}", e),
                    ),
                }
            }
            if qoder_stale {
                // 拨新在 run_snapshot_task 内部经 runtime 完成（ok 行三键拨新）；
                // 永久失效账号（needs_relogin）其内部已落日志，这里只汇总
                match crate::tasks::qoder_credits::run_snapshot_task(
                    &st2,
                    Some(rt_state.inner()),
                ) {
                    Ok(v) => {
                        let n = v.get("accounts").and_then(serde_json::Value::as_i64).unwrap_or(0);
                        let ok = v.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(true);
                        fs_utils::app_log(
                            &st2.data_dir,
                            &format!("API服务启动-积分保鲜: Qoder池拨新完成 ok={ok} accounts={n}"),
                        );
                    }
                    Err(e) => fs_utils::app_log(
                        &st2.data_dir,
                        &format!("API服务启动-积分保鲜: Qoder池刷新失败(不影响使用) {}", e),
                    ),
                }
            }
        });
    }

    // 同步托盘菜单文本 + 系统通知（账号数分池展示：Trae 池空 ≠ 无资源，Buddy 池可能正常服务）
    sync_tray_api_text(app, true);
    let notify_body = if pool_file.wb_enabled {
        format!("端口 {port}，Trae 池 {pool_count} 个账号，Buddy 池 {wb_pool_count} 个账号")
    } else {
        format!("端口 {port}，池内 {pool_count} 个账号")
    };
    crate::notify::notify(app, "API 网关已启动", &notify_body);

    Ok(status)
}

#[tauri::command]
pub async fn api_server_start(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<ApiServiceStatus, String> {
    do_start(&app, &state, &runtime).await
}

/// 停止 API 服务核心逻辑（页面命令 / 托盘菜单共用）
pub async fn do_stop(
    app: &tauri::AppHandle,
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) -> Result<(), String> {
    // 审查 P2-10：先取走 runtime 并立即释放 std 锁，再做异步等待——
    // 锁卫不得跨 await 持有（future 会变 !Send，过不了 tauri command 的
    // Send 检查），且持锁等待会阻塞并发的启动/停止调用
    let mut taken = {
        let mut guard = safe_lock(runtime);
        guard.take()
    };
    if let Some(rt) = taken.as_mut() {
        // 审查 P2-10：异步等待优雅停机（tokio timeout + await），
        // 不再以 thread::sleep 轮询阻塞 tokio worker 线程
        rt.handle.stop_async().await;
        // 批次 C/E：停止前排空用量脏队列、api_keys 计数与日志队列（flusher 已停）
        rt.shared.flush_pending_writes();
        fs_utils::app_log(&state.data_dir, "API 服务已停止");
        sync_tray_api_text(app, false);
        crate::notify::notify(app, "API 网关已停止", "本地网关已关闭");
    }
    Ok(())
}

#[tauri::command]
pub async fn api_server_stop(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<(), String> {
    do_stop(&app, &state, &runtime).await
}

/// 同步托盘「API 服务」菜单文本（服务未运行显示"启动"，运行中显示"停止"）
fn sync_tray_api_text(app: &tauri::AppHandle, running: bool) {
    if let Some(tray) = app.try_state::<crate::TrayMenu>() {
        let _ = tray
            .api_item
            .set_text(if running { "停止 API 服务" } else { "启动 API 服务" });
    }
}

#[tauri::command]
pub fn api_server_status(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> ApiServiceStatus {
    let guard = safe_lock(&runtime);
    // 端口显示与 do_start 同源（gateway_settings，§8.2），避免双源显示漂移
    let port = crate::api_server::gateway_settings::load(&state.data_dir).port;
    match guard.as_ref() {
        Some(rt) => {
            let total = rt
                .shared
                .total_requests
                .load(std::sync::atomic::Ordering::Relaxed);
            let active = safe_lock(&rt.shared.active_uid).clone();
            let last_err = safe_lock(&rt.shared.last_error).clone();
            ApiServiceStatus {
                running: true,
                port,
                total_requests: total,
                active_uid: active,
                last_error: last_err,
                started_at: Some(rt.started_at),
            }
        }
        None => ApiServiceStatus {
            running: false,
            port,
            total_requests: 0,
            active_uid: None,
            last_error: None,
            started_at: None,
        },
    }
}

// ==================== 池管理命令 ====================

/// 读取 api_pool 并应用 Buddy 池旧共享值一次性迁移（纯函数，便于单测）：
/// per-pool 拆分后旧三池共用字段 `account_concurrency_limit` / `pool_sticky_ttl_secs`
/// 已退役，迁移口径为**仅 Buddy 池沿用旧共享值**——Buddy 新字段缺失且旧字段存在时
/// 回填旧值（下次 pool_set 保存即落盘固化，读取侧幂等），Trae/Qoder 池直接落
/// serde default（并发 1 / 池粘性 300s / 会话粘性 1800s）。
/// 入参为 kv 原始文本（None = 无记录），反序列化失败回退 Default（与 kv_get 一致）
fn load_pool_file_with_legacy_migration(raw: Option<String>) -> ApiPoolFile {
    let Some(text) = raw else {
        return ApiPoolFile::default();
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let mut pf: ApiPoolFile = serde_json::from_value(parsed.clone()).unwrap_or_default();
    if parsed.get("wb_account_concurrency_limit").is_none() {
        if let Some(old) = parsed
            .get("account_concurrency_limit")
            .and_then(serde_json::Value::as_u64)
        {
            pf.wb_account_concurrency_limit = old.min(u32::MAX as u64) as u32;
        }
    }
    if parsed.get("wb_pool_sticky_ttl_secs").is_none() {
        if let Some(old) = parsed
            .get("pool_sticky_ttl_secs")
            .and_then(serde_json::Value::as_u64)
        {
            pf.wb_pool_sticky_ttl_secs = old;
        }
    }
    pf
}

/// 便捷封装：读 api_pool 原始文本 + Buddy 旧共享值迁移（pub(crate)：全部读取/
/// 读改写基线统一走此入口，含 groups.rs 等跨模块写回点——直读 kv_get 会落
/// serde default，整表写回把迁移值静默覆盖丢失；保证未启动服务时 pool_list
/// 展示与启动热应用取值一致）
pub(crate) fn load_pool_file(data_dir: &std::path::Path) -> ApiPoolFile {
    load_pool_file_with_legacy_migration(crate::store::db(data_dir).kv_get_raw("api_pool"))
}

#[tauri::command]
pub fn pool_list(state: State<'_, AppState>) -> ApiPoolFile {
    load_pool_file(&state.data_dir)
}

/// Buddy 池生效入池白名单（纯函数，便于单测）：显式 wb_enabled_uids 优先；
/// 空时兼容旧数据——旧版 WB 账号混存于共享 enabled_uids（wb- 前缀条目），有则沿用；
/// 两者皆空 = 全部含凭证账号自动入池（对齐 Buddy 页「含凭证账号参与 WB 上游调度」
/// 的设计语义。修复：WB 池此前误用 Trae 共享白名单过滤，而 UI 只能勾选 Trae 账号，
/// WB 池恒空 → Buddy 源模型永远 503 no_healthy_account、双源模型失去跨池兜底）
fn effective_wb_uids(
    pf: &ApiPoolFile,
    wb_accounts: &[crate::api_server::pool::WbSyncAccount],
) -> Vec<String> {
    if !pf.wb_enabled_uids.is_empty() {
        return pf.wb_enabled_uids.clone();
    }
    let legacy: Vec<String> = pf
        .enabled_uids
        .iter()
        .filter(|u| u.starts_with("wb-"))
        .cloned()
        .collect();
    if !legacy.is_empty() {
        return legacy;
    }
    wb_accounts.iter().map(|a| a.uid.clone()).collect()
}

/// Qoder 池生效入池白名单（纯函数，便于单测）：显式 qoder_enabled_uids 优先；
/// 空 = 全部含凭证账号自动入池（fail-open，对齐 Qoder 资源调度页「清空 = 全部入池」
/// 的设计语义，与 effective_wb_uids 同构）
fn effective_qoder_uids(pf: &ApiPoolFile, qoder_ids: &[String]) -> Vec<String> {
    if !pf.qoder_enabled_uids.is_empty() {
        return pf.qoder_enabled_uids.clone();
    }
    qoder_ids.to_vec()
}

/// 池到期表装配（issue #28）：通用积分（product_id != 209）最早到期优先，
/// 与 pool_credits 的通用口径对齐。回退规则：
/// - 通用剩余表未覆盖的老缓存账号（升级前未刷新过）→ 回退混合口径 expire_times
/// - 已刷新但通用包均长期有效/耗尽的账号（general 有键而 general_expire_times 无键）
///   → 不回退，无到期约束（混合口径里的 Work 包到期不得泄漏进调度）
fn merge_pool_expire_times(rc: &RemainingCreditsFile) -> std::collections::HashMap<String, i64> {
    let mut out: std::collections::HashMap<String, i64> = rc
        .expire_times
        .iter()
        .filter(|(uid, _)| !rc.general.contains_key(*uid))
        .map(|(uid, e)| (uid.clone(), *e))
        .collect();
    for (uid, exp) in &rc.general_expire_times {
        out.insert(uid.clone(), *exp);
    }
    out
}

/// 池装配公共逻辑（do_start 构建 / 凭据变更热重载共用）：
/// 读取 vault 账号 + 池配置 + 分组 + 冷却 + 积分 + 设备映射，全量重建三池内条目。
/// 返回 (trae 池条目数, wb 白名单长度, wb 账号总数, qoder 池条目数) 供调用方记日志。
fn apply_pool_snapshot(
    state: &AppState,
    pool: &ApiPool,
    wb_pool: &ApiPool,
    qoder_pool: &ApiPool,
    buddy_ai_pool: &ApiPool,
) -> (usize, usize, usize, usize) {
    let accounts = crate::vault::load_accounts(state);
    let pool_file = load_pool_file(&state.data_dir);
    let groups_file: crate::models::GroupsFile =
        crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));
    let device_map: DeviceMap = crate::store::docs::device_map_load(&crate::store::db(&state.data_dir));
    // 池积分语义 = 通用积分（product_id 208，llm_utils_chat 实际扣减的类别）：
    // 优先取 general 表，账号未重新刷新过缓存时回退旧的总积分表（与 do_start 原装配一致）
    let pool_credits: std::collections::HashMap<String, f64> = credits_file
        .credits
        .iter()
        .map(|(uid, c)| (uid.clone(), credits_file.general.get(uid).copied().unwrap_or(*c)))
        .collect();
    // 共享标签筛选（P2 需求④）：tag_uid_set 非空时仅纳入带任一所选标签的账号，
    // 与分组筛选取交集（D5 池语义：组 ∩ 标签，均空 = 不限）
    let tag_map = crate::store::docs::account_tags_map(&crate::store::db(&state.data_dir));
    let trae_tag_filter = crate::api_server::org_index::tag_uid_set(&pool_file.trae_tag_ids, &tag_map);
    // 积分快照文件级采集时间（issue #91 缓存保鲜）：宽容解析 → unix 秒（B1）。
    // 一次刷新全量写入 credits 与 updated_at，文件级时间戳对全体条目生效；
    // 池据此判断 reserve 过滤新鲜度（陈旧快照 fail-open，见 pool.rs）。
    // 此前裸用 parse_from_rfc3339，对历史无偏移裸串恒失败 → 恒 None → fail-open
    let credits_fetched_at = credits_file
        .updated_at
        .as_deref()
        .and_then(crate::fs_utils::parse_ts_loose);
    let trae_accounts: Vec<_> = match &trae_tag_filter {
        Some(set) => accounts
            .accounts
            .iter()
            .filter(|a| a.user_id.as_deref().map(|u| set.contains(u)).unwrap_or(false))
            .cloned()
            .collect(),
        None => accounts.accounts.clone(),
    };
    pool.sync_from_accounts(
        &trae_accounts,
        &pool_file.enabled_uids,
        &pool_file.group_ids,
        &groups_file.membership,
        &cooldowns_file.cooldowns,
        &pool_credits,
        credits_fetched_at,
        &merge_pool_expire_times(&credits_file),
        &device_map,
    );
    let wb_accounts_all = crate::commands::workbuddy::wb_upstream_accounts(state);
    // Buddy 池分组筛选（对齐 Trae 池 T10 语义）：wb_group_ids 非空时仅纳入所选
    // 分组的 WB 账号，未分组账号不参与；空 = 不限分组。筛选在装配层先行完成，
    // sync_from_wb 白名单交集语义不变。标签筛选与分组合取（D5 交集）。
    let wb_group_filter: Option<std::collections::HashSet<&str>> = if pool_file.wb_group_ids.is_empty() {
        None
    } else {
        Some(pool_file.wb_group_ids.iter().map(|s| s.as_str()).collect())
    };
    let wb_tag_filter = crate::api_server::org_index::tag_uid_set(&pool_file.wb_tag_ids, &tag_map);
    let wb_accounts: Vec<_> = match (&wb_group_filter, &wb_tag_filter) {
        (Some(f), Some(t)) => wb_accounts_all
            .into_iter()
            .filter(|a| f.contains(a.group_id.as_str()) && t.contains(&a.uid))
            .collect(),
        (Some(f), None) => wb_accounts_all
            .into_iter()
            .filter(|a| f.contains(a.group_id.as_str()))
            .collect(),
        (None, Some(t)) => wb_accounts_all.into_iter().filter(|a| t.contains(&a.uid)).collect(),
        (None, None) => wb_accounts_all,
    };
    let wb_uids = effective_wb_uids(&pool_file, &wb_accounts);
    wb_pool.sync_from_wb(&wb_accounts, &wb_uids);
    // Qoder 池装配（p3-3 + 资源调度页账号池配置）：账号池行 + token store 快照 →
    // QoderSyncAccount。分组筛选先行（qoder_group_ids 非空时仅纳入所选分组的账号，
    // 未分组账号不参与，对齐 Buddy 池 T10 语义）；白名单 fail-open（空 = 全部含
    // 凭证账号入池，needs_relogin 单点表达禁用）；开关由 qoder_enabled 全局控制。
    // access_token 取 token store 快照（sync_from_qoder 跳过空令牌账号）；请求期
    // 真凭证由 identity 回调按次经 ensure_fresh 解析（含惰性刷新），池内快照仅作入池门槛。
    let qoder_accounts_raw = crate::commands::qoder::load_pool(state);
    let token_store = crate::tasks::qoder_common::load_token_store(state);
    let qoder_group_filter: Option<std::collections::HashSet<&str>> =
        if pool_file.qoder_group_ids.is_empty() {
            None
        } else {
            Some(pool_file.qoder_group_ids.iter().map(|s| s.as_str()).collect())
        };
    // Qoder 池标签筛选（与分组合取交集，D5）
    let qoder_tag_filter = crate::api_server::org_index::tag_uid_set(&pool_file.qoder_tag_ids, &tag_map);
    let qoder_sync: Vec<crate::api_server::pool::QoderSyncAccount> = qoder_accounts_raw
        .iter()
        .filter(|a| {
            let group_ok = qoder_group_filter
                .as_ref()
                .map_or(true, |f| f.contains(a.group_id.as_str()));
            let tag_ok = qoder_tag_filter.as_ref().map_or(true, |t| t.contains(&a.id));
            group_ok && tag_ok
        })
        .map(|a| crate::api_server::pool::QoderSyncAccount {
            uid: a.id.clone(),
            name: a.nickname.clone(),
            access_token: token_store
                .get("tokens")
                .and_then(|t| t.get(&a.id))
                .and_then(|c| c.get("access_token"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            machine_id: a
                .device_profile
                .as_ref()
                .map(|p| p.machine_id.clone())
                .unwrap_or_default(),
            needs_relogin: a.needs_relogin,
            // issue #102 智能调度 + issue #91 reserve 过滤：余额/到期/快照时间
            // 随 qoder_credits 回写链路入池（旧数据无字段 → None → fail-open）
            credits: a.credits_balance,
            credits_expire_at: a.credits_expire_at,
            credits_fetched_at: a
                .credits_fetched_at
                .as_deref()
                .and_then(fs_utils::parse_ts_loose),
        })
        .collect();
    let qoder_ids: Vec<String> = qoder_sync.iter().map(|a| a.uid.clone()).collect();
    let qoder_uids = effective_qoder_uids(&pool_file, &qoder_ids);
    qoder_pool.sync_from_qoder(&qoder_sync, &qoder_uids);
    // buddy-ai 独立池装配：buddy_ai_accounts 表独立取号独立入池，与 wb 池零混居
    //（§3.1 硬约束 1）；fail-open 全量含凭证账号入池（禁用由 needs_relogin 表达，
    // 对齐 Qoder 池语义）。分组筛选先行（buddy_ai_group_ids 非空时仅纳入所选分组，
    // 未分组账号不参与，对齐 WB/Qoder 池 T10 语义）+ 标签筛选取交集（D5 池语义）。
    // 开关 buddy_ai_enabled 在网关路由层消费（默认关，dispatch 显式开启制）
    let buddy_ai_accounts_raw = crate::commands::workbuddy::buddy_ai_upstream_accounts(state);
    let buddy_ai_group_filter: Option<std::collections::HashSet<&str>> =
        if pool_file.buddy_ai_group_ids.is_empty() {
            None
        } else {
            Some(
                pool_file
                    .buddy_ai_group_ids
                    .iter()
                    .map(|s| s.as_str())
                    .collect(),
            )
        };
    let buddy_ai_tag_filter =
        crate::api_server::org_index::tag_uid_set(&pool_file.buddy_ai_tag_ids, &tag_map);
    let buddy_ai_accounts: Vec<_> = match (&buddy_ai_group_filter, &buddy_ai_tag_filter) {
        (Some(f), Some(t)) => buddy_ai_accounts_raw
            .into_iter()
            .filter(|a| f.contains(a.group_id.as_str()) && t.contains(&a.uid))
            .collect(),
        (Some(f), None) => buddy_ai_accounts_raw
            .into_iter()
            .filter(|a| f.contains(a.group_id.as_str()))
            .collect(),
        (None, Some(t)) => buddy_ai_accounts_raw
            .into_iter()
            .filter(|a| t.contains(&a.uid))
            .collect(),
        (None, None) => buddy_ai_accounts_raw,
    };
    let buddy_ai_uids: Vec<String> = buddy_ai_accounts.iter().map(|a| a.uid.clone()).collect();
    buddy_ai_pool.sync_from_wb(&buddy_ai_accounts, &buddy_ai_uids);
    (pool.count(), wb_uids.len(), wb_accounts.len(), qoder_pool.count())
}

/// 网关运行中热重载两池（凭据/成员变更联动）：OAuth 重登、refresh_token 刷新、
/// 手动更新 JWT、导入账号、保存账号池后调用；服务未运行时为 no-op。
/// 全量重建修复运行中池的陈旧快照——旧 JWT / SessionDead 禁用 / 冷却 / 积分 /
/// 新勾选成员缺失（此前仅 note_refresh_success 单点回填 JWT，覆盖不了这些场景，
/// 用户实测「刚登录的 JWT 网关还是不行」即此根因）。
pub fn reload_pools_if_running(
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) {
    let guard = safe_lock(runtime);
    let Some(rt) = guard.as_ref() else { return };
    let (trae, wb_uids, wb_total, qoder) = apply_pool_snapshot(
        state,
        &rt.shared.pool,
        &rt.shared.wb_pool,
        &rt.shared.qoder_pool,
        &rt.shared.buddy_ai_pool,
    );
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务池热重载(凭据/成员变更联动): trae_pool={trae} wb_whitelist={wb_uids} wb_accounts={wb_total} qoder_pool={qoder}"
        ),
    );
}

/// 网关运行中回写 Trae 池积分快照（issue #67）：refresh_remaining_credits 刷新后
/// 调用，让 expire_first 等策略基于最新积分选号。此前刷新只写库不回写池，秃号
/// 内存中 credits>0 永远 selectable、粘性/策略持续命中同一账号。服务未运行时
/// no-op；仅回写 Trae 池（WB 池经 write_back_pool_balances 独立链路运行期拨新）。
pub fn push_credits_if_running(
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) {
    let guard = safe_lock(runtime);
    let Some(rt) = guard.as_ref() else { return };
    let credits_file: RemainingCreditsFile =
        crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));
    // 三键拨新（issue #109）：此前只拨余额两键，credits_expire_at 钉死在装配时刻——
    // 网关启动后新登录/新刷新的账号池内 expire=None，被 expire_first
    // 「有过期时间者优先」恒压（调度持续命中带旧到期信息的老账号，新会话同样命中）；
    // 充值续期后的新到期也进不了池，旧值已过期的账号甚至被 selectable 整体排除。
    // 积分与到期均对齐 apply_pool_snapshot 装配口径：优先 general 表，到期用
    // merge_pool_expire_times（general 优先、混合口径兜底），池状态与重启后一致
    let expire_times = merge_pool_expire_times(&credits_file);
    let pool_snapshot: std::collections::HashMap<String, (f64, Option<i64>)> = credits_file
        .credits
        .iter()
        .map(|(uid, c)| {
            (
                uid.clone(),
                (
                    credits_file.general.get(uid).copied().unwrap_or(*c),
                    expire_times.get(uid).copied(),
                ),
            )
        })
        .collect();
    rt.shared.pool.update_credits_snapshot(&pool_snapshot);
    fs_utils::app_log(
        &state.data_dir,
        &format!("API服务池积分回写(运行期): n={}", pool_snapshot.len()),
    );
}

/// pool_set 字段合并（纯函数，便于单测）：未传（None）保留 existing 原值，传值覆盖。
/// 注意 strategy/wb_strategy 的显式空串是合法值（"跟随默认"语义），与 None（未传）区分；
/// group_ids 显式空数组 = 清空分组，None = 保留（语义与其他字段统一）。
fn merge_pool_set(
    existing: &ApiPoolFile,
    uids: Vec<String>,
    strategy: Option<String>,
    wb_strategy: Option<String>,
    group_ids: Option<Vec<String>>,
    wb_group_ids: Option<Vec<String>>,
    wb_enabled: Option<bool>,
    wb_default_thinking: Option<bool>,
    wb_tool_exec: Option<bool>,
    wb_bg_downgrade: Option<bool>,
    wb_longctx_downgrade: Option<bool>,
    wb_hedge_threshold_ms: Option<u64>,
    trae_account_concurrency_limit: Option<u32>,
    trae_pool_sticky_ttl_secs: Option<u64>,
    wb_sticky_ttl_secs: Option<u64>,
    // per-pool 三参数拆分（F-76②/F-77）：Trae/Buddy/Qoder 三池各自的并发上限与
    // 池粘性/会话粘性 TTL；None = 保留原值。旧共用参数 account_concurrency_limit /
    // pool_sticky_ttl_secs 已退役（Trae 池按默认值落地，不沿用旧共享值）
    trae_sticky_ttl_secs: Option<u64>,
    // Trae 池竞速对冲阈值毫秒（0 = 关闭）；None = 保留原值
    trae_hedge_threshold_ms: Option<u64>,
    wb_account_concurrency_limit: Option<u32>,
    wb_pool_sticky_ttl_secs: Option<u64>,
    qoder_account_concurrency_limit: Option<u32>,
    qoder_pool_sticky_ttl_secs: Option<u64>,
    qoder_sticky_ttl_secs: Option<u64>,
    wb_uids: Option<Vec<String>>,
    qoder_enabled: Option<bool>,
    qoder_hedge_threshold_ms: Option<u64>,
    qoder_sticky_enabled: Option<bool>,
    // Qwen3.8-Flash 免费活动倍率覆盖开关（issue #111）；None = 保留原值
    qoder_flash_free_rate: Option<bool>,
    // Qoder 池入池白名单（qd- 前缀账号 id）；None = 保留原值，
    // Some(list) = 覆盖（Qoder 页账号池勾选保存；空数组 = fail-open 全量入池）
    qoder_uids: Option<Vec<String>>,
    // Qoder 池分组筛选（共享分组 shg_ 前缀 id）；None = 保留原值
    qoder_group_ids: Option<Vec<String>>,
    // Qoder 池内调度策略；空串 = 跟随 Trae 池（同 wb_strategy 语义）；None = 保留原值
    qoder_strategy: Option<String>,
    trae_enabled: Option<bool>,
    // buddy-ai（国际版）显式会话粘性开关 + 池粘性 TTL（§4.2）；None = 保留原值
    buddy_ai_sticky_enabled: Option<bool>,
    buddy_ai_pool_sticky_ttl_secs: Option<u64>,
) -> ApiPoolFile {
    // Trae 池白名单剥离 wb- 前缀条目：WB 账号归属独立白名单 wb_enabled_uids，
    // 旧版混存于共享 enabled_uids（历史兼容形态），保存时迁移归位防丢失
    let trae_uids: Vec<String> = uids
        .into_iter()
        .filter(|u| !u.starts_with("wb-"))
        .collect();
    let wb_enabled_uids = wb_uids.unwrap_or_else(|| {
        // 未显式传 WB 白名单（如仅改 Trae 池配置的保存）：迁移 existing 中混存的
        // wb- 条目，避免 Trae 页保存把 Buddy 池成员清空
        let legacy: Vec<String> = existing
            .enabled_uids
            .iter()
            .filter(|u| u.starts_with("wb-"))
            .cloned()
            .collect();
        if existing.wb_enabled_uids.is_empty() && !legacy.is_empty() {
            legacy
        } else {
            existing.wb_enabled_uids.clone()
        }
    });
    ApiPoolFile {
        trae_enabled: trae_enabled.unwrap_or(existing.trae_enabled),
        enabled_uids: trae_uids,
        strategy: strategy.unwrap_or_else(|| existing.strategy.clone()),
        wb_strategy: wb_strategy.unwrap_or_else(|| existing.wb_strategy.clone()),
        group_ids: group_ids.unwrap_or_else(|| existing.group_ids.clone()),
        wb_group_ids: wb_group_ids.unwrap_or_else(|| existing.wb_group_ids.clone()),
        wb_enabled: wb_enabled.unwrap_or(existing.wb_enabled),
        wb_default_thinking: wb_default_thinking.unwrap_or(existing.wb_default_thinking),
        wb_tool_exec: wb_tool_exec.unwrap_or(existing.wb_tool_exec),
        wb_bg_downgrade: wb_bg_downgrade.unwrap_or(existing.wb_bg_downgrade),
        wb_longctx_downgrade: wb_longctx_downgrade.unwrap_or(existing.wb_longctx_downgrade),
        wb_hedge_threshold_ms: wb_hedge_threshold_ms.unwrap_or(existing.wb_hedge_threshold_ms),
        trae_account_concurrency_limit: trae_account_concurrency_limit
            .unwrap_or(existing.trae_account_concurrency_limit),
        trae_pool_sticky_ttl_secs: trae_pool_sticky_ttl_secs
            .unwrap_or(existing.trae_pool_sticky_ttl_secs),
        trae_sticky_ttl_secs: trae_sticky_ttl_secs.unwrap_or(existing.trae_sticky_ttl_secs),
        trae_hedge_threshold_ms: trae_hedge_threshold_ms.unwrap_or(existing.trae_hedge_threshold_ms),
        wb_sticky_ttl_secs: wb_sticky_ttl_secs.unwrap_or(existing.wb_sticky_ttl_secs),
        wb_account_concurrency_limit: wb_account_concurrency_limit
            .unwrap_or(existing.wb_account_concurrency_limit),
        wb_pool_sticky_ttl_secs: wb_pool_sticky_ttl_secs
            .unwrap_or(existing.wb_pool_sticky_ttl_secs),
        qoder_account_concurrency_limit: qoder_account_concurrency_limit
            .unwrap_or(existing.qoder_account_concurrency_limit),
        qoder_pool_sticky_ttl_secs: qoder_pool_sticky_ttl_secs
            .unwrap_or(existing.qoder_pool_sticky_ttl_secs),
        qoder_sticky_ttl_secs: qoder_sticky_ttl_secs.unwrap_or(existing.qoder_sticky_ttl_secs),
        wb_enabled_uids,
        qoder_enabled: qoder_enabled.unwrap_or(existing.qoder_enabled),
        qoder_hedge_threshold_ms: qoder_hedge_threshold_ms
            .unwrap_or(existing.qoder_hedge_threshold_ms),
        qoder_sticky_enabled: qoder_sticky_enabled.unwrap_or(existing.qoder_sticky_enabled),
        qoder_flash_free_rate: qoder_flash_free_rate.unwrap_or(existing.qoder_flash_free_rate),
        qoder_enabled_uids: qoder_uids.unwrap_or_else(|| existing.qoder_enabled_uids.clone()),
        qoder_group_ids: qoder_group_ids.unwrap_or_else(|| existing.qoder_group_ids.clone()),
        qoder_strategy: qoder_strategy.unwrap_or_else(|| existing.qoder_strategy.clone()),
        // 标签筛选不参与 merge_pool_set（pool_set 命令在 merge 后追加应用），原样保留
        trae_tag_ids: existing.trae_tag_ids.clone(),
        wb_tag_ids: existing.wb_tag_ids.clone(),
        qoder_tag_ids: existing.qoder_tag_ids.clone(),
        // buddy-ai 开关同 tag_ids 口径：pool_set 命令在 merge 后追加应用（热应用 AtomicBool）
        buddy_ai_enabled: existing.buddy_ai_enabled,
        buddy_ai_sticky_enabled: buddy_ai_sticky_enabled.unwrap_or(existing.buddy_ai_sticky_enabled),
        buddy_ai_pool_sticky_ttl_secs: buddy_ai_pool_sticky_ttl_secs
            .unwrap_or(existing.buddy_ai_pool_sticky_ttl_secs),
        // buddy-ai per-pool 三参数 + 分组/标签筛选：pool_set 命令在 merge 后追加应用
        buddy_ai_account_concurrency_limit: existing.buddy_ai_account_concurrency_limit,
        buddy_ai_sticky_ttl_secs: existing.buddy_ai_sticky_ttl_secs,
        buddy_ai_hedge_threshold_ms: existing.buddy_ai_hedge_threshold_ms,
        buddy_ai_group_ids: existing.buddy_ai_group_ids.clone(),
        buddy_ai_tag_ids: existing.buddy_ai_tag_ids.clone(),
        // buddy-ai 池内调度策略：pool_set 命令在 merge 后追加应用
        buddy_ai_strategy: existing.buddy_ai_strategy.clone(),
    }
}

/// 批量设置池中的账号 UID 列表 + 调度策略 + 分组筛选（T10）+ WB 上游开关（T2.1）
/// + T5.3 默认深度思考 / T5.5 工具代执行 / T5.6③ 后台任务降级（未传字段保留原值）。
/// + F-76/F-77 热参数：长上下文降档 / 慢请求对冲阈值 / 账号并发上限 /
/// 池粘性 TTL / wb_sticky TTL（未传字段保留原值）。
/// + Qoder 池配置：白名单 / 分组筛选 / 池内策略（未传字段保留原值）。
/// 策略与参数热应用；成员/分组变更走凭据/成员变更联动热重载，保存后立即生效。
#[tauri::command]
pub fn pool_set(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    uids: Vec<String>,
    strategy: Option<String>,
    wb_strategy: Option<String>,
    group_ids: Option<Vec<String>>,
    wb_group_ids: Option<Vec<String>>,
    wb_enabled: Option<bool>,
    wb_default_thinking: Option<bool>,
    wb_tool_exec: Option<bool>,
    wb_bg_downgrade: Option<bool>,
    wb_longctx_downgrade: Option<bool>,
    wb_hedge_threshold_ms: Option<u64>,
    trae_account_concurrency_limit: Option<u32>,
    trae_pool_sticky_ttl_secs: Option<u64>,
    wb_sticky_ttl_secs: Option<u64>,
    // per-pool 三参数拆分（F-76②/F-77，旧共用参数已退役）；None = 保留原值
    trae_sticky_ttl_secs: Option<u64>,
    // Trae 池竞速对冲阈值毫秒（0 = 关闭）；None = 保留原值
    trae_hedge_threshold_ms: Option<u64>,
    wb_account_concurrency_limit: Option<u32>,
    wb_pool_sticky_ttl_secs: Option<u64>,
    qoder_account_concurrency_limit: Option<u32>,
    qoder_pool_sticky_ttl_secs: Option<u64>,
    qoder_sticky_ttl_secs: Option<u64>,
    // Buddy 池入池白名单（wb- 前缀账号 id）；None = 保留原值（含旧数据迁移），
    // Some(list) = 覆盖（Buddy 页账号池勾选保存）
    wb_uids: Option<Vec<String>>,
    // Qoder 上游开关（p3-3）；None = 保留原值
    qoder_enabled: Option<bool>,
    // Qoder 竞速对冲阈值毫秒（F-80-余 v2，0 = 关闭）；None = 保留原值
    qoder_hedge_threshold_ms: Option<u64>,
    // Qoder 会话粘性开关（F-80-余 v2）；None = 保留原值
    qoder_sticky_enabled: Option<bool>,
    // Qwen3.8-Flash 免费活动倍率覆盖开关（issue #111，默认开）；None = 保留原值
    qoder_flash_free_rate: Option<bool>,
    // Qoder 池入池白名单（qd- 前缀账号 id）；None = 保留原值
    qoder_uids: Option<Vec<String>>,
    // Qoder 池分组筛选；None = 保留原值
    qoder_group_ids: Option<Vec<String>>,
    // Qoder 池内调度策略（空串 = 跟随 Trae 池）；None = 保留原值
    qoder_strategy: Option<String>,
    // Trae 池参与调度开关（默认开）；None = 保留原值
    trae_enabled: Option<bool>,
    // buddy-ai（国际版）池上游开关（默认关，dispatch 显式开启制）；None = 保留原值
    buddy_ai_enabled: Option<bool>,
    // buddy-ai 显式会话粘性开关 + 池粘性 TTL（§4.2）；None = 保留原值
    buddy_ai_sticky_enabled: Option<bool>,
    buddy_ai_pool_sticky_ttl_secs: Option<u64>,
    // buddy-ai per-pool 三参数（并发上限 / 显式会话粘性 TTL / 竞速对冲阈值）；
    // None = 保留原值（对齐 Qoder 池同型参数）
    buddy_ai_account_concurrency_limit: Option<u32>,
    buddy_ai_sticky_ttl_secs: Option<u64>,
    buddy_ai_hedge_threshold_ms: Option<u64>,
    // buddy-ai 池分组/标签筛选；None = 保留原值，Some(list) = 覆盖
    buddy_ai_group_ids: Option<Vec<String>>,
    buddy_ai_tag_ids: Option<Vec<String>>,
    // buddy-ai 池内调度策略（空串 = 跟随 Trae 池）；None = 保留原值
    buddy_ai_strategy: Option<String>,
    // 各池共享标签筛选（tag_ 前缀 id）；None = 保留原值，Some(list) = 覆盖
    // （与分组筛选取交集，D5 池语义）
    trae_tag_ids: Option<Vec<String>>,
    wb_tag_ids: Option<Vec<String>>,
    qoder_tag_ids: Option<Vec<String>>,
) -> Result<(), String> {
    // B1（审查修复清单 v1.4.9）：api_pool 整文档覆盖写互斥——3 个写点
    // （本命令 / shared_groups ③段 / tags ③段）并发「load→merge→kv_set」互相
    // 覆盖丢配置。进程级叶子锁：写点均在池/org 锁释放后获取，持锁期间不再取
    // 其他锁（热重载在锁外）；启动迁移在 server bind 前无并发窗口，不取锁
    let _cfg = POOL_CFG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let existing: ApiPoolFile = load_pool_file(&state.data_dir);
    let mut pool_file = merge_pool_set(
        &existing,
        uids,
        strategy,
        wb_strategy,
        group_ids,
        wb_group_ids,
        wb_enabled,
        wb_default_thinking,
        wb_tool_exec,
        wb_bg_downgrade,
        wb_longctx_downgrade,
        wb_hedge_threshold_ms,
        trae_account_concurrency_limit,
        trae_pool_sticky_ttl_secs,
        wb_sticky_ttl_secs,
        trae_sticky_ttl_secs,
        trae_hedge_threshold_ms,
        wb_account_concurrency_limit,
        wb_pool_sticky_ttl_secs,
        qoder_account_concurrency_limit,
        qoder_pool_sticky_ttl_secs,
        qoder_sticky_ttl_secs,
        wb_uids,
        qoder_enabled,
        qoder_hedge_threshold_ms,
        qoder_sticky_enabled,
        qoder_flash_free_rate,
        qoder_uids,
        qoder_group_ids,
        qoder_strategy,
        trae_enabled,
        buddy_ai_sticky_enabled,
        buddy_ai_pool_sticky_ttl_secs,
    );
    // 标签筛选在 merge 后追加应用（merge_pool_set 纯函数签名不动，存量单测零改动）：
    // 显式空数组 = 清空标签筛选，None = 保留
    if let Some(v) = trae_tag_ids {
        pool_file.trae_tag_ids = v;
    }
    if let Some(v) = wb_tag_ids {
        pool_file.wb_tag_ids = v;
    }
    if let Some(v) = qoder_tag_ids {
        pool_file.qoder_tag_ids = v;
    }
    // buddy-ai 开关在 merge 后追加应用（对齐 tag_ids 先例：merge_pool_set 纯函数
    // 签名不动，存量单测零改动）；运行中热应用移到配置锁外（M12/M8 修复：此前持
    // POOL_CFG_LOCK 叶子锁期间嵌套取 runtime 锁，违反「持锁期间不再取其他锁」纪律）
    let mut buddy_ai_enabled_applied: Option<bool> = None;
    if let Some(v) = buddy_ai_enabled {
        pool_file.buddy_ai_enabled = v;
        buddy_ai_enabled_applied = Some(v);
    }
    // buddy-ai per-pool 三参数 + 分组/标签筛选（merge 后追加应用，对齐 buddy_ai_enabled
    // 与 tag_ids 先例：merge_pool_set 纯函数签名不动，存量单测零改动）
    if let Some(v) = buddy_ai_account_concurrency_limit {
        pool_file.buddy_ai_account_concurrency_limit = v;
    }
    if let Some(v) = buddy_ai_sticky_ttl_secs {
        pool_file.buddy_ai_sticky_ttl_secs = v;
    }
    if let Some(v) = buddy_ai_hedge_threshold_ms {
        pool_file.buddy_ai_hedge_threshold_ms = v;
    }
    if let Some(v) = buddy_ai_group_ids {
        pool_file.buddy_ai_group_ids = v;
    }
    if let Some(v) = buddy_ai_tag_ids {
        pool_file.buddy_ai_tag_ids = v;
    }
    // buddy-ai 池内调度策略（merge 后追加应用，对齐 buddy_ai_* 先例：
    // merge_pool_set 纯函数签名不动，存量单测零改动）
    if let Some(v) = buddy_ai_strategy {
        pool_file.buddy_ai_strategy = v;
    }
    crate::store::db(&state.data_dir).kv_set("api_pool", &pool_file)?;
    drop(_cfg);
    // 热应用（配置锁外）：运行中即改内存池策略（Buddy 池空值沿用 Trae 池策略，与启动逻辑一致）
    // buddy-ai 开关热应用（M12/M8：随本段移出配置锁外，避免临界区内嵌套取 runtime
    // 锁放大持锁时长；None = 存原值不触碰，与旧行为等价）
    if let Some(v) = buddy_ai_enabled_applied {
        if let Some(rt) = safe_lock(&runtime).as_ref() {
            rt.shared
                .buddy_ai_enabled
                .store(v, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if let Some(rt) = safe_lock(&runtime).as_ref() {
        rt.shared
            .pool
            .set_strategy(crate::api_server::pool::PoolStrategy::parse(&pool_file.strategy));
        rt.shared.wb_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy),
        );
        // Qoder 池策略热应用（空 = 跟随 Trae 池，与启动逻辑一致）
        rt.shared.qoder_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_qoder(&pool_file.strategy, &pool_file.qoder_strategy),
        );
        // buddy-ai 池策略热应用（空 = 跟随 Trae 池，与启动逻辑一致）
        rt.shared.buddy_ai_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_buddy_ai(
                &pool_file.strategy,
                &pool_file.buddy_ai_strategy,
            ),
        );
        // per-pool 三参数热应用（F-76②/F-77 拆分版）：并发上限 / 池粘性 TTL /
        // 显式会话粘性 TTL 三池各自生效
        rt.shared
            .pool
            .set_concurrency_limit(pool_file.trae_account_concurrency_limit);
        rt.shared
            .wb_pool
            .set_concurrency_limit(pool_file.wb_account_concurrency_limit);
        rt.shared
            .qoder_pool
            .set_concurrency_limit(pool_file.qoder_account_concurrency_limit);
        rt.shared
            .trae_sticky
            .set_explicit_ttl(pool_file.trae_sticky_ttl_secs as i64);
        // Trae 池竞速对冲阈值热应用（F-76③ 同构，per-pool 独立配置）
        rt.shared.trae_hedge_threshold_ms.store(
            pool_file.trae_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.wb_sticky.set_explicit_ttl(pool_file.wb_sticky_ttl_secs as i64);
        rt.shared
            .qoder_sticky
            .set_explicit_ttl(pool_file.qoder_sticky_ttl_secs as i64);
        rt.shared.wb_longctx_downgrade.store(
            pool_file.wb_longctx_downgrade,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.wb_hedge_threshold_ms.store(
            pool_file.wb_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.trae_pool_sticky_ttl_secs.store(
            pool_file.trae_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.wb_pool_sticky_ttl_secs.store(
            pool_file.wb_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.qoder_pool_sticky_ttl_secs.store(
            pool_file.qoder_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        // buddy-ai 池粘性 TTL + 显式会话粘性热应用（§4.2）
        rt.shared.buddy_ai_pool_sticky_ttl_secs.store(
            pool_file.buddy_ai_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.buddy_ai_sticky_enabled.store(
            pool_file.buddy_ai_sticky_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        // buddy-ai per-pool 三参数热应用（并发上限 / 显式会话粘性 TTL / 对冲阈值）
        rt.shared
            .buddy_ai_pool
            .set_concurrency_limit(pool_file.buddy_ai_account_concurrency_limit);
        rt.shared
            .buddy_ai_sticky
            .set_explicit_ttl(pool_file.buddy_ai_sticky_ttl_secs as i64);
        rt.shared.buddy_ai_hedge_threshold_ms.store(
            pool_file.buddy_ai_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        // buddy-ai 池上游开关热应用已在上方配置锁外段统一执行（M12/M8，见
        // buddy_ai_enabled_applied），此处不再重复 store
        // Buddy 资源开关热应用（此前仅启动时读取，改动需重启服务生效）
        rt.shared
            .wb_enabled
            .store(pool_file.wb_enabled, std::sync::atomic::Ordering::Relaxed);
        // Trae 池开关热应用（每池自管开关，默认开）
        rt.shared.trae_enabled.store(
            pool_file.trae_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.wb_default_thinking.store(
            pool_file.wb_default_thinking,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared
            .wb_tool_exec
            .store(pool_file.wb_tool_exec, std::sync::atomic::Ordering::Relaxed);
        rt.shared
            .wb_bg_downgrade
            .store(pool_file.wb_bg_downgrade, std::sync::atomic::Ordering::Relaxed);
        // Qoder 上游开关热应用（p3-3）
        rt.shared.qoder_enabled.store(
            pool_file.qoder_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Qoder 竞速对冲阈值 + 会话粘性热应用（F-80-余 v2）
        rt.shared.qoder_hedge_threshold_ms.store(
            pool_file.qoder_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.qoder_sticky_enabled.store(
            pool_file.qoder_sticky_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Qwen3.8-Flash 免费活动倍率覆盖热应用（issue #111）
        rt.shared.qoder_flash_free_rate.store(
            pool_file.qoder_flash_free_rate,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    // 成员/分组热应用：此前仅策略/参数热生效，成员变更要求重启服务；
    // 现统一走凭据/成员变更联动热重载，保存账号池后立即生效
    reload_pools_if_running(&state, &runtime);
    Ok(())
}

/// 返回运行中池的实时状态（冷却/积分等）；服务未运行时返回空数组
#[tauri::command]
pub fn pool_status(runtime: State<'_, Mutex<Option<ApiServerRuntime>>>) -> Vec<PoolStatus> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt.shared.pool.status_list(),
        None => vec![],
    }
}

/// 返回运行中 WB 池的实时状态（F-77⑤ 可观测：含 per-account inflight 在途计数）；
/// 服务未运行时返回空数组
#[tauri::command]
pub fn wb_pool_status(runtime: State<'_, Mutex<Option<ApiServerRuntime>>>) -> Vec<PoolStatus> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt.shared.wb_pool.status_list(),
        None => vec![],
    }
}

/// 返回运行中 Qoder 池的实时状态（Qoder「资源调度」页可观测，含 per-account
/// inflight 在途计数）；服务未运行时返回空数组
#[tauri::command]
pub fn qoder_pool_status(runtime: State<'_, Mutex<Option<ApiServerRuntime>>>) -> Vec<PoolStatus> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt.shared.qoder_pool.status_list(),
        None => vec![],
    }
}

/// 返回运行中 Buddy 国际版（buddy-ai）池的实时状态（全局 API 管理·API Keys
/// 调度配置候选 + 摘要卡可观测，含 per-account inflight 在途计数）；
/// 服务未运行时返回空数组
#[tauri::command]
pub fn buddy_ai_pool_status(runtime: State<'_, Mutex<Option<ApiServerRuntime>>>) -> Vec<PoolStatus> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt.shared.buddy_ai_pool.status_list(),
        None => vec![],
    }
}

/// 手动同步 Qoder 模型目录（Qoder「资源调度」页；复用每日调度任务入口
/// qoder_catalog::run_task，按池序逐可用账号拉取 CN 区 model/list 并原子采纳）。
/// 返回采纳的模型数；无账号/无凭证返回 Ok(0) 语义的信息（与调度器静默跳过一致）
#[tauri::command]
pub async fn qoder_catalog_sync(state: State<'_, AppState>) -> Result<usize, String> {
    // AppState 可 Clone 语义的字段快照（Arc 锁与真实状态共享，token 刷新互斥不失效）
    let st = AppState {
        data_dir: state.data_dir.clone(),
        jwt_refresh_lock: state.inner().jwt_refresh_lock.clone(),
        qoder_pool_lock: state.inner().qoder_pool_lock.clone(),
        wb_pool_lock: state.inner().wb_pool_lock.clone(),
        buddy_ai_pool_lock: state.inner().buddy_ai_pool_lock.clone(),
        doubao_pool_lock: state.inner().doubao_pool_lock.clone(),
        group_defs_lock: state.inner().group_defs_lock.clone(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        // run_task 返回 json：{ok:true, models:n} 或 {ok:true, skipped:...}
        let out = crate::tasks::qoder_catalog::run_task(&st)?;
        if let Some(n) = out.get("models").and_then(serde_json::Value::as_u64) {
            Ok(n as usize)
        } else {
            Err(out
                .get("skipped")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("目录未更新")
                .to_string())
        }
    })
    .await
    .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// 列出 API 日志可用日期列表
#[tauri::command]
pub fn api_logs_list(state: State<'_, AppState>) -> Vec<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.list_dates(30)
}

/// 读取指定日期的 API 日志内容
#[tauri::command]
pub fn api_logs_detail(state: State<AppState>, date: String) -> Option<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.read_log(&date)
}

/// 按时间段和关键字搜索 API 日志
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiLogSearchOpts {
    pub date: String,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
}

#[tauri::command]
pub fn api_logs_search(state: State<AppState>, opts: ApiLogSearchOpts) -> Option<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.search_log(
        &opts.date,
        opts.start_time.as_deref().unwrap_or(""),
        opts.end_time.as_deref().unwrap_or(""),
        opts.keyword.as_deref().unwrap_or(""),
    )
}

/// 切换 API Debug 模式（开启后记录完整请求/响应）
#[tauri::command]
pub fn api_debug_toggle(
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<bool, String> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => {
            let current = rt
                .shared
                .debug_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let new_val = !current;
            rt.shared
                .debug_enabled
                .store(new_val, std::sync::atomic::Ordering::Relaxed);
            Ok(new_val)
        }
        None => Err("API 服务未运行".into()),
    }
}

/// 查询 API Debug 模式状态
#[tauri::command]
pub fn api_debug_status(
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> bool {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt
            .shared
            .debug_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        None => false,
    }
}

// ==================== 模型列表命令 ====================

/// 读取模型列表（api_models.json，缺失时写入默认列表）
#[tauri::command]
pub fn api_models_list(state: State<'_, AppState>) -> Vec<models_sync::ModelOption> {
    models_sync::load_models(&state.data_dir)
}

/// 从官网配置接口同步模型列表（batch_get_detail_param，不消耗积分）
#[tauri::command]
pub async fn api_models_sync(
    state: State<'_, AppState>,
) -> Result<Vec<models_sync::ModelOption>, String> {
    let data_dir = state.data_dir.clone();
    // 预先在调用方解密账号（vault 依赖 AppState，阻塞线程内不便访问）
    let accounts = crate::vault::load_accounts(&state);
    // 阻塞网络请求放入阻塞线程池，避免卡住异步运行时
    tauri::async_runtime::spawn_blocking(move || models_sync::fetch_official(&data_dir, accounts))
        .await
        .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// 从 WB 上游模型目录接口同步 wb_model_catalog.json（T5.1/F-37，动态替换；
/// 网关启动时已自动做一次，此命令供手动刷新）。取任一含凭证的 WB 账号。
#[tauri::command]
pub async fn api_wb_catalog_sync(state: State<'_, AppState>) -> Result<usize, String> {
    // 阻塞网络请求放入阻塞线程池，避免卡住异步运行时
    let data_dir = state.data_dir.clone();
    let accounts = crate::commands::workbuddy::wb_upstream_accounts(&state);
    tauri::async_runtime::spawn_blocking(move || wb_catalog_sync_impl(&data_dir, &accounts))
        .await
        .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// WB 上游模型目录同步实现（手动命令与调度器 wb-catalog-sync 共用）：
/// accounts 由调用方取好（wb_upstream_accounts，避免双读 pool/token store），
/// 取首个含凭证账号拉取并替换目录；空列表时 Err（调度侧据此转静默跳过不计失败）
pub(crate) fn wb_catalog_sync_impl(
    data_dir: &std::path::Path,
    accounts: &[crate::api_server::pool::WbSyncAccount],
) -> Result<usize, String> {
    let acct = accounts
        .first()
        .ok_or("无可用 WB 账号凭证，无法拉取上游目录")?;
    crate::api_server::wb_catalog::fetch_and_replace(
        data_dir,
        &acct.uid,
        &acct.token,
        &acct.domain,
        &acct.enterprise_id,
        acct.global_region,
    )
}

/// 列出 wb_model_catalog.json 中的 WB 模型（Buddy API 服务页展示模型 id/倍率/档位）
#[tauri::command]
pub fn api_wb_catalog_list(state: State<'_, AppState>) -> Vec<crate::api_server::wb_catalog::WbModel> {
    crate::api_server::wb_catalog::load(&state.data_dir)
}

/// Buddy 国际版（buddy-ai）模型目录：kv 动态目录（/v3/config 同步落盘）优先，
/// 缺失回落内置静态表（§4.4）。此前恒回静态表（倍率占位 1.0、能力字段全空），
/// 「同步官网模型」落盘后表格仍不更新的根因。
#[tauri::command]
pub fn api_buddy_ai_catalog_list(state: State<'_, AppState>) -> Vec<crate::api_server::wb_catalog::WbModel> {
    crate::api_server::wb_catalog::buddy_ai_load(&state.data_dir)
}

/// 查询最近 N 天的 API 用量统计（Trae 模型请求桶，按日聚合，直接读盘，服务未运行也可查）
#[tauri::command]
pub fn api_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent(&state.data_dir, days, false)
}

/// 查询最近 N 天的 WB 上游用量统计（wb_days 桶，Buddy「API 服务」页专用，与 Trae 侧分账）
#[tauri::command]
pub fn api_wb_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent(&state.data_dir, days, true)
}

/// 查询最近 N 天的自定义模型用量统计（custom_days 桶，API 管理·用量统计「自定义」筛选专用）
#[tauri::command]
pub fn api_custom_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent_in(
        &state.data_dir,
        days,
        crate::api_server::usage::UsageBucket::Custom,
    )
}

/// 查询最近 N 天的 buddy-ai 国际池用量统计（buddy_ai_days 桶，BuddyAI「积分看板·模型消耗排行」专用）
#[tauri::command]
pub fn api_buddy_ai_usage_stats(
    state: State<'_, AppState>,
    days: Option<u32>,
) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(30).clamp(1, 90);
    crate::api_server::usage::query_recent_in(
        &state.data_dir,
        days,
        crate::api_server::usage::UsageBucket::BuddyAi,
    )
}

/// 查询最近 N 天的 Qoder 上游用量统计（qoder_days 桶，与 Trae/WB/Custom 侧分账；上游接入前恒空）
#[tauri::command]
pub fn api_qoder_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent_in(
        &state.data_dir,
        days,
        crate::api_server::usage::UsageBucket::Qoder,
    )
}

// ==================== 多 API Key 命令 ====================

/// 读取 API Key 列表与鉴权开关
#[tauri::command]
pub fn api_keys_list(
    state: State<'_, AppState>,
) -> crate::api_server::api_keys::ApiKeysFile {
    crate::api_server::api_keys::load(&state.data_dir)
}

/// 保存 API Key 列表（整表写盘；每次请求重读文件，改动立即生效）。
/// `auth_disabled` 不传时保留现值（避免整表保存覆盖鉴权开关）。
/// 审查 G3：保存动作同样是鉴权状态的写入点，非环回监听时与启动门禁
/// （server.rs::ensure_bind_auth_policy）同策略拦截——否则服务运行中经
/// 此命令关闭鉴权/清空 Key 可绕过启动时校验，局域网裸奔。
#[tauri::command]
pub fn api_keys_save(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    keys: Vec<crate::api_server::api_keys::ApiKeyEntry>,
    auth_disabled: Option<bool>,
) -> Result<(), String> {
    let prev = crate::api_server::api_keys::load(&state.data_dir);
    let file = crate::api_server::api_keys::ApiKeysFile {
        keys,
        auth_disabled: auth_disabled.unwrap_or(prev.auth_disabled),
    };
    // 非环回监听 + （新状态显式关闭鉴权 或 无任何启用 Key）→ 拒绝保存
    // host 与运行时物理同源（对齐 auth.rs /health 豁免的 M3 修复）：服务运行中读
    // rt.shared.listen_host（实际绑定值）——配置被改回 127.0.0.1 但服务仍监听
    // 0.0.0.0 时按实际值判定，不出现「配置值 vs 实际监听值」分离导致的 fail-open；
    // 未运行时读配置值（下次启动还有 ensure_bind_auth_policy 兜底）
    let host = {
        let guard = safe_lock(&runtime);
        match guard.as_ref() {
            Some(rt) => rt
                .shared
                .listen_host
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            None => crate::api_server::gateway_settings::load(&state.data_dir).host,
        }
    };
    if !crate::api_server::server::is_loopback_host(&host)
        && (file.auth_disabled || !file.has_enabled())
    {
        return Err(format!(
            "当前网关监听地址为 {host}（非本机环回），局域网可访问；\
             出于安全要求必须启用至少一个 API Key 且不可关闭鉴权"
        ));
    }
    crate::api_server::api_keys::save(&state.data_dir, &file);
    Ok(())
}

/// Key 组/标签约束覆盖预览（D4/D5）：对传入条目按 org_index 实时展开计算
/// 各池覆盖账号数（前端「覆盖账号数 = 0」强提示用；只读不落盘）。
/// 返回 {key_id: {trae: n, buddy: n, qoder: n, total: n}}。
#[tauri::command]
pub fn api_keys_org_preview(
    state: State<'_, AppState>,
    keys: Vec<crate::api_server::api_keys::ApiKeyEntry>,
) -> std::collections::HashMap<String, std::collections::HashMap<String, usize>> {
    use crate::api_server::api_keys::{parse_bind_pool, split_pool_tagged};
    use std::collections::HashMap;
    let idx = crate::api_server::org_index::get(&state.data_dir);
    let mut out: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for k in keys {
        // effective_allowed = allowed_accounts（精确白名单）∪ 组∩标签展开（D5）
        let mut allowed: std::collections::HashSet<String> =
            k.allowed_accounts.iter().cloned().collect();
        if !k.allowed_groups.is_empty() || !k.allowed_tags.is_empty() {
            allowed.extend(crate::api_server::org_index::expand(
                &k.allowed_groups,
                &k.allowed_tags,
                &idx,
            ));
        }
        // 与运行时 pool_constraints 分池语义对齐（issue #30）：
        // - 混合白名单（组/标签已配置 → 展开条目必带前缀；或精确白名单含前缀条目）
        //   → 裸 uid 防御性归属 buddy（旧数据混入兜底，与运行时一致）
        // - 非混合（全裸 uid 旧语义）→ 白名单整体作用于 bind_pool 所指池
        //   （未绑定 = buddy，F-35 旧语义；bind=trae/buddy 各归其池）
        let mixed = !k.allowed_groups.is_empty()
            || !k.allowed_tags.is_empty()
            || k.allowed_accounts.iter().any(|u| split_pool_tagged(u).0.is_some());
        let legacy_pool = parse_bind_pool(&k.bind_pool).unwrap_or("buddy");
        let mut counts: HashMap<String, usize> = HashMap::new();
        for u in &allowed {
            let pool = match split_pool_tagged(u) {
                (Some(p), _) => p,
                (None, _) => if mixed { "buddy" } else { legacy_pool },
            };
            *counts.entry(pool.to_string()).or_default() += 1;
        }
        let total: usize = allowed.len();
        counts.insert("total".into(), total);
        out.insert(k.id, counts);
    }
    out
}

// ==================== 统一网关命令（Phase 1 §8.1） ====================

/// 统一模型目录聚合视图（实时派生，不落盘）。
/// `available_only=true`：过滤全部来源不可用的模型。
/// 服务运行中用实时池健康派生 enabled 标记；未运行时放宽（池健康视为可选，
/// wb_enabled 读落盘值）——目录展示不因服务停启而失真（§3.3 #5）
#[tauri::command]
pub fn api_unified_models(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    available_only: Option<bool>,
) -> Vec<crate::api_server::unified_catalog::UnifiedModel> {
    let (
        wb_enabled,
        qoder_enabled,
        trae_ok,
        buddy_ok,
        qoder_ok,
        buddy_ai_enabled,
        buddy_ai_ok,
        qoder_flash_free,
    ) = match safe_lock(&runtime).as_ref() {
        Some(rt) => {
            let s = &rt.shared;
            (
                s.wb_enabled.load(std::sync::atomic::Ordering::Relaxed),
                s.qoder_enabled.load(std::sync::atomic::Ordering::Relaxed),
                // Trae 开关（默认开）参与可用性：关闭时 Trae 源徽章置灰
                s.trae_enabled.load(std::sync::atomic::Ordering::Relaxed)
                    && s.pool.has_selectable(),
                s.wb_pool.has_selectable(),
                s.qoder_pool.has_selectable(),
                s.buddy_ai_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                s.buddy_ai_pool.has_selectable(),
                // Qwen3.8-Flash 免费活动覆盖（issue #111），与取号/排序同口径
                s.qoder_flash_free_rate
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
        }
        None => {
            let pf: ApiPoolFile = crate::store::db(&state.data_dir).kv_get("api_pool");
            (
                pf.wb_enabled,
                pf.qoder_enabled,
                pf.trae_enabled,
                true,
                true,
                pf.buddy_ai_enabled,
                true,
                pf.qoder_flash_free_rate,
            )
        }
    };
    let data_dir = state.data_dir.clone();
    let mut list = crate::api_server::unified_catalog::unified_models_ex(
        &data_dir,
        wb_enabled,
        trae_ok,
        buddy_ok,
        Some((qoder_enabled, qoder_ok)),
        Some((buddy_ai_enabled, buddy_ai_ok)),
        qoder_flash_free,
    );
    if available_only.unwrap_or(false) {
        // issue #145：可用口径须含 L1 静态禁用——disabled 源不承接请求，不能计为可用
        list.retain(|m| m.sources.iter().any(|s| s.enabled && !s.disabled));
    }
    list
}

/// 读取全局模型白名单（canonical 归一列表；空 = 不限，issue #26）
#[tauri::command]
pub fn model_whitelist_get(state: State<'_, AppState>) -> Vec<String> {
    crate::api_server::unified_catalog::load_whitelist(&state.data_dir)
}

/// 保存全局模型白名单（canonical 归一 + 去空 + 去重；空列表 = 不限）；
/// 返回归一后的生效值（前端展示以返回值为准）
#[tauri::command]
pub fn model_whitelist_set(
    state: State<'_, AppState>,
    models: Vec<String>,
) -> Result<Vec<String>, String> {
    crate::api_server::unified_catalog::save_whitelist(&state.data_dir, &models)
}

/// 读取调度策略（规范化后视图：非法池名/空优先级已回退默认）
#[tauri::command]
pub fn dispatch_policy_get(
    state: State<'_, AppState>,
) -> crate::api_server::dispatch::DispatchPolicy {
    crate::api_server::dispatch::load_policy(&state.data_dir)
}

/// 保存调度策略；返回规范化后的生效值（前端展示以返回值为准）
#[tauri::command]
pub fn dispatch_policy_set(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    policy: crate::api_server::dispatch::DispatchPolicy,
) -> Result<crate::api_server::dispatch::DispatchPolicy, String> {
    crate::api_server::dispatch::save_policy(&state.data_dir, &policy)?;
    // 账号保留剩余积分（issue #91）热应用：保存后四池即时生效
    //（配置锁外，与 pool_set 热应用同模式；负值由 set_reserve_credits 防御归零）
    if let Some(rt) = safe_lock(&runtime).as_ref() {
        rt.shared.pool.set_reserve_credits(policy.reserve_credits);
        rt.shared.wb_pool.set_reserve_credits(policy.reserve_credits);
        rt.shared
            .qoder_pool
            .set_reserve_credits(policy.reserve_credits);
        rt.shared
            .buddy_ai_pool
            .set_reserve_credits(policy.reserve_credits);
    }
    Ok(crate::api_server::dispatch::load_policy(&state.data_dir))
}

/// 读取网关设置（port / default_model；缺失时从 app_settings 旧字段一次性迁移）
#[tauri::command]
pub fn gateway_settings_get(
    state: State<'_, AppState>,
) -> crate::api_server::gateway_settings::GatewaySettings {
    crate::api_server::gateway_settings::load(&state.data_dir)
}

/// 网关设置保存载荷（patch 合并语义）：None 字段保留现值——监听地址/端口/默认
/// 模型仅由显式修改才变，部分载荷（旧版前端/其他入口缺字段）不会把用户已配置的
/// 非环回监听地址覆盖回 127.0.0.1（审查 G2 场景的结构化修复）
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewaySettingsPatch {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub default_model: Option<String>,
}

/// 保存网关设置（patch 合并：读现值 → 应用 Some 字段 → 落盘；端口改动在下次
/// 启动 API 服务后生效；返回规范化后的生效值）
#[tauri::command]
pub fn gateway_settings_set(
    state: State<'_, AppState>,
    settings: GatewaySettingsPatch,
) -> Result<crate::api_server::gateway_settings::GatewaySettings, String> {
    let mut s = crate::api_server::gateway_settings::load(&state.data_dir);
    if let Some(p) = settings.port {
        s.port = p;
    }
    if let Some(h) = settings.host {
        s.host = h;
    }
    if let Some(m) = settings.default_model {
        s.default_model = m;
    }
    crate::api_server::gateway_settings::save(&state.data_dir, s)?;
    Ok(crate::api_server::gateway_settings::load(&state.data_dir))
}

// ==================== 局域网接入地址（issue #34：网关 0.0.0.0 监听展示用） ====================

/// 局域网网卡地址条目（前端展示网关接入地址：`http://{ip}:{port}/v1`）
#[derive(Debug, Clone, serde::Serialize)]
pub struct LanIfaceIp {
    /// 接口名（如「以太网」「WLAN」）
    pub name: String,
    /// IPv4 地址
    pub ip: String,
}

/// 虚拟/回环接口名黑名单（小写子串匹配）：Docker 网桥与 veth 对端、虚拟化平台
/// Host-Only/NAT 网卡（Hyper-V / WSL / VMware / VirtualBox）、TUN/TAP 代理与
/// VPN 虚拟网卡（Tailscale / ZeroTier / Clash 等）、蓝牙 PAN / 拨号虚拟适配器、
/// macOS 专属虚拟接口（Docker Desktop 网桥 bridge100* / Apple Wireless Direct
/// Link awdl0 / 低时延 WLAN llw0——`br-` 不命中 `bridge100`，单独收录）。
/// bridge 编号特例（合并审查 #8）：`bridge0`…`bridge99` 是 mac Thunderbolt 网桥
/// 的真实网卡命名（可承载局域网流量），先于黑名单放行；`bridge100` 起为 Docker
/// Desktop 保留编号，仍按黑名单拦截。非纯数字后缀（bridge / bridgeabc /
/// bridge100x）parse 失败 → 落回黑名单子串匹配。
/// 中文接口名（「以太网」「WLAN」「本地连接」）与常规英文网卡名均不含关键词
fn is_virtual_iface(name: &str) -> bool {
    const BLACKLIST: &[&str] = &[
        "loopback", "docker", "br-", "bridge", "veth", "virbr", "vmnet", "vethernet", "vmware",
        "virtualbox", "virtual", "hyper-v", "wsl", "bluetooth", "tailscale", "zerotier",
        "hamachi", "tap", "tun", "wintun", "clash", "sing-box", "singbox", "mihomo",
        "wireguard", "openvpn", "wan miniport", "ras async", "wi-fi direct",
        // macOS 专属：Docker Desktop 网桥（bridge100…N）/ Apple Awdl / Llw 虚拟接口
        "awdl", "llw",
    ];
    let n = name.to_lowercase();
    // Thunderbolt 网桥放行（bridge0…bridge99）；Docker Desktop 从 bridge100 起编号
    if let Some(num) = n.strip_prefix("bridge") {
        if let Ok(id) = num.parse::<u32>() {
            return id >= 100;
        }
    }
    if BLACKLIST.iter().any(|k| n.contains(k)) {
        return true;
    }
    // Linux/macOS 回环接口名 lo / lo0 / lo1…：精确匹配（"lo" 子串会误伤
    // 「Local Area Connection」等物理网卡命名，故单独处理）
    n == "lo" || (n.starts_with("lo") && n.as_bytes()[2..].iter().all(|b| b.is_ascii_digit()))
}

/// 局域网网卡 IPv4 地址列表（多物理网卡多 IP）：
/// 仅 IPv4 单播；排除回环（127/8）、链路本地（169.254/16）、Docker/虚拟化/
/// 代理虚拟网卡（名称黑名单）；按 IP 去重、保持系统枚举顺序
#[tauri::command]
pub fn lan_iface_ips() -> Vec<LanIfaceIp> {
    lan_iface_ips_impl()
}

fn lan_iface_ips_impl() -> Vec<LanIfaceIp> {
    let mut out: Vec<LanIfaceIp> = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return out;
    };
    for ifa in ifaces {
        let std::net::IpAddr::V4(v4) = ifa.ip() else { continue };
        if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
            continue;
        }
        if is_virtual_iface(&ifa.name) {
            continue;
        }
        let ip_str = v4.to_string();
        if out.iter().any(|e| e.ip == ip_str) {
            continue; // 同 IP 多接口（如桥接别名）只保留首条
        }
        out.push(LanIfaceIp { name: ifa.name, ip: ip_str });
    }
    out
}

// ==================== 自定义模型资源池（custom_models.json） ====================

/// 自定义模型列表（OpenAI 兼容上游直通；命中即直达，§custom_models）
#[tauri::command]
pub fn custom_models_list(
    state: State<'_, AppState>,
) -> Vec<crate::api_server::custom_models::CustomModel> {
    crate::api_server::custom_models::load(&state.data_dir)
}

/// 保存自定义模型（upsert：id 为空新增并生成 cm- id，存在则整条覆盖；
/// 校验 name/base_url 必填 + 同名仅拒绝「同名+同 base_url+同 api_key」完全重复
/// （同名不同上游 = 上游组成员）；返回保存后的完整列表）。
/// 编辑（id 非空）同步清该条目实例级熔断（B2 审查修复：否则修正 base_url/
/// 换 api_key 后旧冷却窗口仍拦到重启，配置改动看似无效）
#[tauri::command]
pub fn custom_models_save(
    state: State<'_, AppState>,
    model: crate::api_server::custom_models::CustomModel,
) -> Result<Vec<crate::api_server::custom_models::CustomModel>, String> {
    let edited_id = if model.id.is_empty() { None } else { Some(model.id.clone()) };
    let list = crate::api_server::custom_models::upsert(&state.data_dir, model)?;
    if let Some(id) = edited_id {
        crate::api_server::custom_lb::clear_circuit(&id);
    }
    Ok(list)
}

/// 删除自定义模型（按 id）；返回是否确有删除。
/// 删除成功同步清除该条目的实例级熔断残留（审查修复：否则同 id 复用/残留
/// 冷却窗口在重启前一直存在；粘性条目不在组内时选路自然回落，TTL 60s 自愈）
#[tauri::command]
pub fn custom_models_remove(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let removed = crate::api_server::custom_models::remove(&state.data_dir, &id)?;
    if removed {
        crate::api_server::custom_lb::clear_circuit(&id);
    }
    Ok(removed)
}

/// 自定义模型连通性测试：向上游发一条最小 chat 请求（max_tokens=16），
/// 成功返回摘要、失败返回原因（保存/编辑前的前置校验入口）。
/// 阻塞 IO 放 spawn_blocking，外加 30s 总超时（覆盖连接 5s + 首字读 10s + 出字余量；
/// probe 走独立短超时 agent，不复用主路径 custom_agent 连接池——B3）。
#[tauri::command]
pub async fn custom_model_test(
    model: crate::api_server::custom_models::CustomModel,
) -> Result<String, String> {
    // 与保存侧（custom_models::upsert）完全同口径的参数预检：给出明确错误而非
    // 透传上游 4xx。base_url 必须先过 norm_base 归一（scheme 小写 + 剥默认端口
    // 443/80 + 去尾斜杠），校验镜像 upsert——否则 HTTPS://x:443/ 能保存、此处
    // 探活却被大小写敏感 starts_with 拒绝，两侧口径分裂
    let mut cm = model;
    cm.name = cm.name.trim().to_string();
    cm.base_url = crate::api_server::custom_models::norm_base(&cm.base_url);
    if cm.name.is_empty() {
        return Err("请先填写模型名称".into());
    }
    if cm.base_url.is_empty()
        || (!cm.base_url.starts_with("http://") && !cm.base_url.starts_with("https://"))
    {
        return Err("API 地址必须以 http:// 或 https:// 开头".into());
    }
    let handle = tokio::task::spawn_blocking(move || crate::api_server::custom_route::probe(&cm));
    match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(format!("测试任务失败: {e}")),
        Err(_) => Err("测试超时（30 秒）".into()),
    }
}

/// 自定义上游熔断状态快照（P3 可观测）：每条仍在冷却中的上游
/// (cm.id, 连续失败片段数, 剩余冷却秒)。UI 徽标消费，与 custom_models_list
/// 按 id 关联展示「熔断中」态；重启清零（内存态）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CustomUpstreamCircuitStatus {
    pub id: String,
    pub fails: u32,
    pub cooling_secs: u64,
}

#[tauri::command]
pub fn custom_lb_status() -> Vec<CustomUpstreamCircuitStatus> {
    crate::api_server::custom_lb::circuit_snapshot()
        .into_iter()
        .map(|(id, fails, cooling_secs)| CustomUpstreamCircuitStatus { id, fails, cooling_secs })
        .collect()
}

/// Trae 模型元数据人工覆盖（L1 覆盖层，键 canonical_id；编辑后聚合视图即时生效）
#[tauri::command]
pub fn trae_model_meta_set(
    state: State<'_, AppState>,
    model: String,
    meta: crate::api_server::unified_catalog::TraeModelMeta,
) -> Result<(), String> {
    crate::api_server::unified_catalog::meta_set(&state.data_dir, &model, meta)
}

/// 读取 Trae 模型元数据人工覆盖（编辑弹框回显用；None = 无人工值，交由自动来源链）
#[tauri::command]
pub fn trae_model_meta_get(
    state: State<'_, AppState>,
    model: String,
) -> Option<crate::api_server::unified_catalog::TraeModelMeta> {
    crate::api_server::unified_catalog::load_meta(&state.data_dir)
        .remove(&crate::api_server::unified_catalog::canonical_id(&model))
}

/// 清除 Trae 模型元数据人工覆盖；返回是否存在过
#[tauri::command]
pub fn trae_model_meta_clear(state: State<'_, AppState>, model: String) -> Result<bool, String> {
    crate::api_server::unified_catalog::meta_clear(&state.data_dir, &model)
}

// ==================== 单元测试：pool_set 合并语义（调度策略收口） ====================

#[cfg(test)]
mod legacy_migration_tests {
    use super::load_pool_file_with_legacy_migration;

    #[test]
    fn buddy_inherits_legacy_shared_values() {
        // 旧格式文件（只有旧共用字段，无 per-pool 新字段）：Buddy 沿用旧共享值，
        // Trae/Qoder 池不沿用、直接落 serde default（并发 1 / 池粘性 300s / 会话粘性 1800s）
        let raw = r#"{
            "enabled_uids": ["u1"],
            "strategy": "weighted",
            "account_concurrency_limit": 4,
            "pool_sticky_ttl_secs": 900
        }"#;
        let pf = load_pool_file_with_legacy_migration(Some(raw.into()));
        assert_eq!(pf.wb_account_concurrency_limit, 4);
        assert_eq!(pf.wb_pool_sticky_ttl_secs, 900);
        assert_eq!(pf.trae_account_concurrency_limit, 1);
        assert_eq!(pf.trae_pool_sticky_ttl_secs, 300);
        assert_eq!(pf.trae_sticky_ttl_secs, 1800);
        assert_eq!(pf.qoder_account_concurrency_limit, 1);
        assert_eq!(pf.qoder_pool_sticky_ttl_secs, 300);
        assert_eq!(pf.qoder_sticky_ttl_secs, 1800);
    }

    #[test]
    fn explicit_wb_fields_win_over_legacy() {
        // 新格式文件（Buddy 新字段已存在）：旧共用字段即使残留也不回填
        let raw = r#"{
            "account_concurrency_limit": 4,
            "pool_sticky_ttl_secs": 900,
            "wb_account_concurrency_limit": 2,
            "wb_pool_sticky_ttl_secs": 620
        }"#;
        let pf = load_pool_file_with_legacy_migration(Some(raw.into()));
        assert_eq!(pf.wb_account_concurrency_limit, 2);
        assert_eq!(pf.wb_pool_sticky_ttl_secs, 620);
    }

    #[test]
    fn missing_or_invalid_raw_falls_back_to_default() {
        // 无记录 / 非法 JSON：全默认（与 kv_get 回退语义一致）
        let pf = load_pool_file_with_legacy_migration(None);
        assert_eq!(pf.wb_account_concurrency_limit, 1);
        assert_eq!(pf.wb_pool_sticky_ttl_secs, 300);
        let bad = load_pool_file_with_legacy_migration(Some("not-json".into()));
        assert_eq!(bad.wb_account_concurrency_limit, 1);
        assert_eq!(bad.wb_pool_sticky_ttl_secs, 300);
    }

    #[test]
    fn legacy_value_overflow_clamped_to_u32_max() {
        // 旧并发值超出 u32 范围：钳制为 u32::MAX（防 as 转换静默截断）
        let raw = r#"{ "account_concurrency_limit": 99999999999 }"#;
        let pf = load_pool_file_with_legacy_migration(Some(raw.into()));
        assert_eq!(pf.wb_account_concurrency_limit, u32::MAX);
    }
}

#[cfg(test)]
mod pool_merge_tests {
    use super::merge_pool_set;
    use crate::models::ApiPoolFile;

    /// 模拟已存在的 api_pool.json（各字段均非默认值，验证"保留"是否生效）
    fn existing() -> ApiPoolFile {
        ApiPoolFile {
            trae_enabled: true,
            enabled_uids: vec!["u1".into()],
            strategy: "weighted".into(),
            wb_strategy: "p2c".into(),
            group_ids: vec!["g1".into()],
            wb_enabled: true,
            wb_default_thinking: true,
            wb_tool_exec: false,
            wb_bg_downgrade: false,
            wb_longctx_downgrade: true,
            wb_hedge_threshold_ms: 8000,
            trae_account_concurrency_limit: 2,
            trae_pool_sticky_ttl_secs: 600,
            trae_sticky_ttl_secs: 2400,
            trae_hedge_threshold_ms: 5000,
            wb_sticky_ttl_secs: 3600,
            wb_account_concurrency_limit: 2,
            wb_pool_sticky_ttl_secs: 620,
            wb_enabled_uids: Vec::new(),
            wb_group_ids: vec!["wg1".into()],
            qoder_enabled: true,
            qoder_hedge_threshold_ms: 4000,
            qoder_sticky_enabled: true,
            qoder_flash_free_rate: true,
            qoder_account_concurrency_limit: 3,
            qoder_pool_sticky_ttl_secs: 900,
            qoder_sticky_ttl_secs: 3600,
            qoder_strategy: "p2c".into(),
            qoder_enabled_uids: vec!["qd-1".into()],
            qoder_group_ids: Vec::new(),
            trae_tag_ids: Vec::new(),
            wb_tag_ids: Vec::new(),
            qoder_tag_ids: Vec::new(),
            buddy_ai_enabled: false,
            buddy_ai_sticky_enabled: true,
            buddy_ai_pool_sticky_ttl_secs: 300,
            buddy_ai_account_concurrency_limit: 1,
            buddy_ai_sticky_ttl_secs: 1800,
            buddy_ai_hedge_threshold_ms: 8000,
            buddy_ai_group_ids: Vec::new(),
            buddy_ai_tag_ids: Vec::new(),
            buddy_ai_strategy: "credit_first".into(),
        }
    }

    #[test]
    fn none_fields_preserve_existing() {
        // 只改成员（uids 必传覆盖），其余未传 → 全部保留原值（含 F-76/F-77 新参数）
        let m = merge_pool_set(
            &existing(),
            vec!["u2".into()],
            None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None,
            // buddy-ai 粘性开关 + 池粘性 TTL 未传 → 保留原值
            None, None,
            // Qwen3.8-Flash 免费活动覆盖未传 → 保留原值
            None,
        );
        assert_eq!(m.enabled_uids, vec!["u2".to_string()]);
        assert!(m.buddy_ai_sticky_enabled, "未传应保留原值");
        assert_eq!(m.strategy, "weighted");
        assert_eq!(m.wb_strategy, "p2c");
        assert_eq!(m.group_ids, vec!["g1".to_string()]);
        // wb_group_ids 未传 → 保留原值
        assert_eq!(m.wb_group_ids, vec!["wg1".to_string()]);
        assert!(m.wb_enabled);
        assert!(m.wb_default_thinking);
        assert!(!m.wb_tool_exec);
        assert!(!m.wb_bg_downgrade);
        // F-76/F-77 新参数未传 → 保留原值
        assert!(m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 8000);
        assert_eq!(m.trae_account_concurrency_limit, 2);
        assert_eq!(m.trae_pool_sticky_ttl_secs, 600);
        assert_eq!(m.trae_sticky_ttl_secs, 2400);
        assert_eq!(m.trae_hedge_threshold_ms, 5000);
        assert_eq!(m.wb_sticky_ttl_secs, 3600);
        // per-pool 三参数未传 → 各自保留原值
        assert_eq!(m.wb_account_concurrency_limit, 2);
        assert_eq!(m.wb_pool_sticky_ttl_secs, 620);
        assert_eq!(m.qoder_account_concurrency_limit, 3);
        assert_eq!(m.qoder_pool_sticky_ttl_secs, 900);
        assert_eq!(m.qoder_sticky_ttl_secs, 3600);
        // Qoder 开关未传 → 保留原值（p3-3）；Qoder v2 参数未传 → 保留原值；
        // Trae 开关未传 → 保留原值（默认开）
        assert!(m.qoder_enabled);
        assert_eq!(m.qoder_hedge_threshold_ms, 4000);
        assert!(m.qoder_sticky_enabled);
        assert!(m.trae_enabled);
        // buddy-ai 池内策略未传 → 保留原值（pool_set 在 merge 后追加应用，本函数恒保留）
        assert_eq!(m.buddy_ai_strategy, "credit_first");
    }

    #[test]
    fn trae_save_migrates_legacy_wb_uids() {
        // 旧版共享白名单混存 wb- 条目：Trae 页保存（未传 wb_uids）时 wb- 条目
        // 从 enabled_uids 剥离并迁移进 wb_enabled_uids，Buddy 池成员不丢失
        let mut legacy = existing();
        legacy.enabled_uids = vec!["1001".into(), "wb-abc".into(), "1002".into()];
        legacy.wb_enabled_uids = Vec::new();
        let m = merge_pool_set(
            &legacy,
            vec!["1001".into(), "wb-abc".into(), "1002".into(), "wb-def".into()],
            None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None,
            None, None,
            // Qwen3.8-Flash 免费活动覆盖未传 → 保留原值
            None,
        );
        // Trae 白名单剥离 wb- 条目
        assert_eq!(m.enabled_uids, vec!["1001".to_string(), "1002".to_string()]);
        // 仅 existing 混存的 wb- 条目迁移进 WB 白名单；传入名单中的未知 wb- 条目
        // （wb-def，非本次迁移来源）不并入，随剥离丢弃（白名单隔离语义）
        assert_eq!(m.wb_enabled_uids, vec!["wb-abc".to_string()]);
    }

    // ── merge_pool_expire_times（issue #28：调度到期 = 通用口径）────────────

    use super::merge_pool_expire_times;
    use crate::models::RemainingCreditsFile;

    #[test]
    fn merge_expires_prefers_general_table() {
        // 新旧两表同账号并存 → 通用口径胜出（Work 包污染的混合值不参与）
        let mut rc = RemainingCreditsFile::default();
        rc.expire_times.insert("u1".into(), 1000);
        rc.general.insert("u1".into(), 90.0);
        rc.general_expire_times.insert("u1".into(), 2000);
        let got = merge_pool_expire_times(&rc);
        assert_eq!(got.get("u1"), Some(&2000));
    }

    #[test]
    fn merge_expires_legacy_fallback_without_general_cache() {
        // 老缓存账号（通用剩余表未覆盖，升级前未刷新过）→ 回退混合口径保底
        let mut rc = RemainingCreditsFile::default();
        rc.expire_times.insert("legacy".into(), 1000);
        let got = merge_pool_expire_times(&rc);
        assert_eq!(got.get("legacy"), Some(&1000));
    }

    #[test]
    fn merge_expires_no_fallback_when_refreshed_without_general_expiry() {
        // 已刷新（general 有键）但通用包均长期有效/耗尽（general_expire_times 无键）
        // → 不回退，混合口径里的 Work 包到期不得泄漏进调度
        let mut rc = RemainingCreditsFile::default();
        rc.expire_times.insert("u1".into(), 1000);
        rc.general.insert("u1".into(), 0.0);
        let got = merge_pool_expire_times(&rc);
        assert!(got.get("u1").is_none());
    }

    #[test]
    fn merge_expires_end_to_end_after_persistence_roundtrip() {
        // issue #75 验收链路：刷新任务写 general_expire_times → 持久化 →
        // 重启后 load 恢复 → merge 命中调度口径（Work 包混合口径不泄漏）。
        // 持久化层断链回归测试：save/load 列名或字段不一致时此处必失败。
        let dir = std::env::temp_dir().join(format!("twa_merge_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let s = crate::store::db(&dir);
            let mut rc = RemainingCreditsFile::default();
            rc.general.insert("u1".into(), 90.0);
            rc.expire_times.insert("u1".into(), 1000);
            rc.general_expire_times.insert("u1".into(), 2000);
            crate::store::docs::remaining_credits_save(&s, &rc).unwrap();
        }
        let got = {
            let s = crate::store::db(&dir);
            let rc = crate::store::docs::remaining_credits_load(&s);
            merge_pool_expire_times(&rc)
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.get("u1"), Some(&2000));
    }

    #[test]
    fn explicit_wb_uids_override() {
        // Buddy 页勾选保存：显式传 wb_uids → 覆盖（不再走旧数据迁移）
        let mut legacy = existing();
        legacy.enabled_uids = vec!["u1".into(), "wb-old".into()];
        let m = merge_pool_set(
            &legacy,
            vec!["u1".into()],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            // per-pool 三参数 + 对冲阈值（trae_sticky / trae 对冲 / wb 并发 /
            // wb 池粘性 / qoder 并发 / qoder 池粘性 / qoder 会话粘性）未传 → 保留原值
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(vec!["wb-new".into()]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            // buddy-ai 粘性开关 + 池粘性 TTL 未传 → 保留原值
            None,
            None,
            // Qwen3.8-Flash 免费活动覆盖未传 → 保留原值
            None,
        );
        assert_eq!(m.wb_enabled_uids, vec!["wb-new".to_string()]);
        assert_eq!(m.enabled_uids, vec!["u1".to_string()]);
    }

    #[test]
    fn effective_wb_uids_priority() {
        use super::effective_wb_uids;
        use crate::api_server::pool::WbSyncAccount;
        let acc = |uid: &str| WbSyncAccount {
            uid: uid.into(),
            name: String::new(),
            token: "t".into(),
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
            credits: None,
            credits_expire_at: None,
            credits_fetched_at: None,
            needs_relogin: false,
            group_id: String::new(),
        };
        let accounts = vec![acc("wb-a"), acc("wb-b")];
        // ① 显式白名单优先
        let mut pf = existing();
        pf.wb_enabled_uids = vec!["wb-a".into()];
        assert_eq!(effective_wb_uids(&pf, &accounts), vec!["wb-a".to_string()]);
        // ② 旧混存 wb- 条目沿用
        pf.wb_enabled_uids = Vec::new();
        pf.enabled_uids = vec!["1001".into(), "wb-b".into()];
        assert_eq!(effective_wb_uids(&pf, &accounts), vec!["wb-b".to_string()]);
        // ③ 两者皆空 = 全部含凭证账号自动入池（fail-open）
        pf.enabled_uids = vec!["1001".into()];
        assert_eq!(
            effective_wb_uids(&pf, &accounts),
            vec!["wb-a".to_string(), "wb-b".to_string()]
        );
    }

    #[test]
    fn effective_qoder_uids_failopen_and_explicit() {
        // Qoder 池白名单生效语义（与 effective_wb_uids 同构）：空 = fail-open 全量，
        // 显式白名单优先（交集过滤由 sync_from_qoder 完成）
        use super::effective_qoder_uids;
        let mut pf = ApiPoolFile::default();
        let ids = vec!["qd-1".to_string(), "qd-2".to_string()];
        assert_eq!(effective_qoder_uids(&pf, &ids), ids);
        pf.qoder_enabled_uids = vec!["qd-2".into()];
        assert_eq!(effective_qoder_uids(&pf, &ids), vec!["qd-2".to_string()]);
    }

    #[test]
    fn some_fields_override_and_empty_string_is_legal_value() {
        // 显式空串 = "跟随默认"合法值（区别于 None 未传）；显式空数组 = 清空分组
        let m = merge_pool_set(
            &existing(),
            vec![],
            Some("p2c".into()),
            Some("".into()),
            Some(vec![]),
            // wb_group_ids 显式覆盖
            Some(vec!["wg2".into()]),
            Some(false),
            Some(false),
            Some(true),
            Some(true),
            Some(false),
            Some(3000),
            Some(0),
            Some(60),
            Some(120),
            // per-pool 三参数显式传入 → 覆盖（trae 显式粘性 / wb 并发 / wb 池粘性 /
            // qoder 并发 / qoder 池粘性 / qoder 会话粘性）；trae 对冲阈值未传 → 保留
            Some(90),
            None,
            Some(2),
            Some(240),
            Some(3),
            Some(360),
            Some(2400),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            // buddy-ai 粘性开关 + 池粘性 TTL 未传 → 保留原值
            None,
            None,
            // Qwen3.8-Flash 免费活动覆盖未传 → 保留原值
            None,
        );
        assert_eq!(m.strategy, "p2c");
        assert_eq!(m.wb_strategy, "");
        assert!(m.group_ids.is_empty());
        assert_eq!(m.wb_group_ids, vec!["wg2".to_string()]);
        assert!(!m.wb_enabled);
        assert!(!m.wb_default_thinking);
        assert!(m.wb_tool_exec);
        assert!(m.wb_bg_downgrade);
        // F-76/F-77 新参数显式传入 → 覆盖
        assert!(!m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 3000);
        assert_eq!(m.trae_account_concurrency_limit, 0);
        assert_eq!(m.trae_pool_sticky_ttl_secs, 60);
        assert_eq!(m.wb_sticky_ttl_secs, 120);
        // per-pool 三参数显式传入 → 覆盖；trae 对冲阈值未传 → 保留原值
        assert_eq!(m.trae_sticky_ttl_secs, 90);
        assert_eq!(m.trae_hedge_threshold_ms, 5000);
        assert_eq!(m.wb_account_concurrency_limit, 2);
        assert_eq!(m.wb_pool_sticky_ttl_secs, 240);
        assert_eq!(m.qoder_account_concurrency_limit, 3);
        assert_eq!(m.qoder_pool_sticky_ttl_secs, 360);
        assert_eq!(m.qoder_sticky_ttl_secs, 2400);
    }

    #[test]
    fn strategy_only_caller_does_not_touch_wb_and_flags() {
        // Trae 资源调度页收口后只保存成员/分组：不传 strategy/wb_strategy/开关组 → 均保留
        let m = merge_pool_set(
            &existing(),
            vec!["u1".into(), "u3".into()],
            None, None, Some(vec!["g2".into()]), None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None,
            None, None,
            None,
        );
        assert_eq!(m.enabled_uids.len(), 2);
        assert_eq!(m.group_ids, vec!["g2".to_string()]);
        assert_eq!(m.strategy, "weighted");
        assert_eq!(m.wb_strategy, "p2c");
        assert!(m.wb_enabled);
    }

    #[test]
    fn empty_existing_preserves_nothing_but_fills_defaults() {
        // 旧版 api_pool.json（无策略字段）+ 只传成员：策略落为空串（运行时 parse 回退 expire_first）；
        // F-76/F-77 新参数未传 → serde default 生效（对冲 8s、并发=1、池粘性 300s）
        let m = merge_pool_set(
            &ApiPoolFile::default(),
            vec!["u1".into()],
            None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None,
            None, None,
            // Qwen3.8-Flash 免费活动覆盖未传 → 保留原值
            None,
        );
        assert_eq!(m.strategy, "");
        assert_eq!(m.wb_strategy, "");
        assert!(m.group_ids.is_empty());
        assert!(!m.wb_enabled);
        assert!(!m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 8000);
        assert_eq!(m.trae_account_concurrency_limit, 1);
        assert_eq!(m.trae_pool_sticky_ttl_secs, 300);
        assert_eq!(m.wb_sticky_ttl_secs, 1800);
        // per-pool 三参数 serde default：三池并发 1 / 池粘性 300s / 显式粘性 1800s
        assert_eq!(m.trae_sticky_ttl_secs, 1800);
        assert_eq!(m.trae_hedge_threshold_ms, 8000);
        assert_eq!(m.wb_account_concurrency_limit, 1);
        assert_eq!(m.wb_pool_sticky_ttl_secs, 300);
        assert_eq!(m.qoder_account_concurrency_limit, 1);
        assert_eq!(m.qoder_pool_sticky_ttl_secs, 300);
        assert_eq!(m.qoder_sticky_ttl_secs, 1800);
        // Trae 开关 serde default = true（主池缺省恒可用）
        assert!(m.trae_enabled);
    }
}

// ==================== 单元测试：局域网网卡地址过滤（issue #34） ====================

#[cfg(test)]
mod lan_iface_tests {
    use super::{is_virtual_iface, lan_iface_ips_impl};

    /// 黑名单命中：Docker / 虚拟化平台 / 代理 TUN / 回环 / 蓝牙 PAN / 拨号 /
    /// macOS 专属虚拟接口（bridge100 / awdl0 / llw0）
    #[test]
    fn virtual_iface_blacklist() {
        assert!(is_virtual_iface("Loopback Pseudo-Interface 1"));
        assert!(is_virtual_iface("lo0"));
        assert!(is_virtual_iface("docker0"));
        assert!(is_virtual_iface("br-3f9a2b1c"));
        assert!(is_virtual_iface("veth9a2b1c0"));
        assert!(is_virtual_iface("virbr0"));
        assert!(is_virtual_iface("vEthernet (Default Switch)"));
        assert!(is_virtual_iface("vEthernet (WSL)"));
        assert!(is_virtual_iface("VMware Network Adapter VMnet1"));
        assert!(is_virtual_iface("VirtualBox Host-Only Network"));
        assert!(is_virtual_iface("TAP-Windows Adapter V9"));
        assert!(is_virtual_iface("Wintun Userspace Tunnel"));
        assert!(is_virtual_iface("Tailscale"));
        assert!(is_virtual_iface("ZeroTier One [Ethernet]"));
        assert!(is_virtual_iface("Clash"));
        assert!(is_virtual_iface("Mihomo"));
        assert!(is_virtual_iface("WireGuard Tunnel"));
        assert!(is_virtual_iface("Bluetooth Device (Personal Area Network)"));
        assert!(is_virtual_iface("WAN Miniport (IP)"));
        assert!(is_virtual_iface("Microsoft Wi-Fi Direct Virtual Adapter"));
        // macOS 专属（合并审查跟进：bridge100 不含 "br-"，此前会泄漏为局域网地址）
        assert!(is_virtual_iface("bridge100"), "Docker Desktop mac 网桥");
        assert!(is_virtual_iface("bridge101"));
        assert!(is_virtual_iface("awdl0"), "Apple Wireless Direct Link");
        assert!(is_virtual_iface("llw0"), "Apple 低时延 WLAN");
    }

    /// 物理网卡（中英文常见命名）不误伤
    #[test]
    fn physical_iface_not_blocked() {
        assert!(!is_virtual_iface("以太网"));
        assert!(!is_virtual_iface("本地连接"));
        assert!(!is_virtual_iface("Ethernet"));
        assert!(!is_virtual_iface("Ethernet 2"));
        assert!(!is_virtual_iface("WLAN"));
        assert!(!is_virtual_iface("Wi-Fi"));
        assert!(!is_virtual_iface("Intel(R) Wi-Fi 6 AX201 160MHz"));
        assert!(!is_virtual_iface("Realtek Gaming 2.5GbE Family Controller"));
        // macOS Thunderbolt 网桥（合并审查 #8：bridge0…bridge99 为真实网卡，放行）
        assert!(!is_virtual_iface("bridge0"), "mac Thunderbolt 网桥");
        assert!(!is_virtual_iface("bridge2"));
    }

    /// 真机冒烟：不 panic；结果无回环/链路本地/IPv6，无虚拟网卡名，按 IP 去重
    #[test]
    fn lan_iface_ips_smoke() {
        let ips = lan_iface_ips_impl();
        for (i, e) in ips.iter().enumerate() {
            assert!(!e.ip.starts_with("127."), "回环泄漏: {}", e.ip);
            assert!(!e.ip.starts_with("169.254."), "链路本地泄漏: {}", e.ip);
            assert!(!e.ip.contains(':'), "混入 IPv6: {}", e.ip);
            assert!(!is_virtual_iface(&e.name), "虚拟网卡泄漏: {}", e.name);
            assert!(!ips[..i].iter().any(|p| p.ip == e.ip), "重复 IP: {}", e.ip);
        }
    }
}
