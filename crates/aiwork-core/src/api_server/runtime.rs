//! 网关运行时装配：自 commands/api_server.rs `do_start`（L33-303）下沉，
//! 剥离托盘同步/系统通知/AppHandle；诊断日志语义原样保留。
//! server 以单端口单体运行：build_shared 构造共享状态，路由合并见 aiwork-server。

use std::sync::{Arc, Mutex};

use super::gateway_settings;
use super::pool::{ApiPool, PoolStrategy, WbSyncAccount};
use super::wb_sticky::StickyStore;
use super::{ApiLogger, ApiSharedState, usage};
use crate::fs_utils;
use crate::models::{AccountCooldownsFile, ApiPoolFile, DeviceMap, GroupsFile, RemainingCreditsFile};
use crate::state::AppState;
use crate::store;

/// 读取 api_pool 并应用 Buddy 池旧共享值一次性迁移（per-pool 拆分读取侧回填）：
/// 旧三池共用字段 `account_concurrency_limit` / `pool_sticky_ttl_secs` 已退役，
/// 迁移口径为**仅 Buddy 池沿用旧共享值**——Buddy 新字段缺失且旧字段存在时回填
/// 旧值（下次 pool_set 保存即落盘固化，读取侧幂等），Trae/Qoder 池直接落
/// serde default（并发 1 / 池粘性 300s / 会话粘性 1800s）。
/// 入参为 kv 原始文本（None = 无记录），反序列化失败回退 Default（与 kv_get 一致）。
/// 全部读取/读改写基线统一走此入口（含 pool_list / pool_set / groups 删除写回），
/// 直读 kv_get 会落 serde default，整表写回把迁移值静默覆盖丢失
pub(crate) fn load_pool_file_with_legacy_migration(raw: Option<String>) -> ApiPoolFile {
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

/// 便捷封装：读 api_pool 原始文本 + Buddy 旧共享值迁移（pub(crate)：build_shared /
/// apply_pool_snapshot / commands 层 pool_list / pool_set / qoder groups 写回统一入口）
pub(crate) fn load_pool_file(data_dir: &std::path::Path) -> ApiPoolFile {
    load_pool_file_with_legacy_migration(store::db(data_dir).kv_get_raw("api_pool"))
}

/// api_pool 配置读-改-写互斥（审查 B1）：kv("api_pool") 为整文档覆盖写，
/// 三个写点（pool_set / 分组删除清引用 / 标签删除清引用）的
/// 「load_pool_file → 修改 → kv_set」复合段并发时，旧副本整文档写回会互相
/// 覆盖丢更新。所有此类复合段必须持本锁；纯读（load_pool_file）无需持锁。
/// 锁序约定：叶子锁——现有写点均在池锁/org 锁释放后获取本锁；严禁持本锁
/// 再取任何池锁或 group_defs_lock。启动迁移（kv api_pool 首次落库）在 server
/// bind 前完成，无并发窗口，不取本锁。
static POOL_CFG_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

pub(crate) fn api_pool_cfg_lock() -> std::sync::MutexGuard<'static, ()> {
    POOL_CFG_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 汇总网关共享状态（原 do_start 装配段）：网关设置 → 池装配 → 诊断日志 → 共享状态。
/// 不负责监听绑定（由调用方 merge build_router 后自行 axum::serve）。
pub fn build_shared(state: &AppState) -> Arc<ApiSharedState> {
    // 网关设置（§8.1/§9.2）：port / default_model 读 data/api_gateway_settings.json；
    // load 已将空 default_model 兜底为内置默认
    let gw = gateway_settings::load(&state.data_dir);
    let port = gw.port;
    let default_model = gw.default_model;

    // 读取账号数据、冷却状态、剩余积分（账号经 vault 解密还原明文 jwt）
    let accounts = crate::vault::load_accounts(state);
    // SQLite 化（P2）：api_pool.json → kv `api_pool`（含 Buddy 池旧共享值迁移）
    let pool_file: ApiPoolFile = load_pool_file(&state.data_dir);
    // SQLite 化（P3）：groups/cooldowns/remaining_credits/device_map 经 store 读取
    let groups_file: GroupsFile = store::docs::groups_load(&store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        store::docs::account_cooldowns_load(&store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        store::docs::remaining_credits_load(&store::db(&state.data_dir));
    // 诊断口径与池装配一致：通用积分余额/到期（老缓存账号回退混合口径）
    let merged_expires = merge_pool_expire_times(&credits_file);
    // 同 uid 重复条目告警（issue #80）：服务端同 uid 即同一账号，池装配与凭证更新仅按
    // uid 单条生效，多条目会加剧「多账号」错觉；建议保留一条、删除多余。
    // 每进程仅告警一次（build_shared 随每次池热重载重跑，重复刷屏无增量信息）
    static DUP_UID_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    {
        let mut seen_uids: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for a in &accounts.accounts {
            if let Some(uid) = a.user_id.as_deref() {
                match seen_uids.get(uid) {
                    Some(first_name) => {
                        if !DUP_UID_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            fs_utils::app_log(
                                &state.data_dir,
                                &format!(
                                    "  账号池告警: 发现重复 uid={}（已有账号 [{}]，当前账号 [{}]）；池装配与凭证更新仅按 uid 单条生效，建议保留一条、删除多余条目",
                                    uid, first_name, a.name
                                ),
                            );
                        }
                    }
                    None => {
                        seen_uids.insert(uid, a.name.as_str());
                    }
                }
            }
        }
    }

    // 调度策略（T10）：api_pool.json.strategy，空/未知值回退 expire_first
    let strategy = PoolStrategy::parse(&pool_file.strategy);

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

    // 创建池并同步（装配逻辑抽公共函数：build_shared 与凭据变更热重载共用）
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
    let (pool_count, wb_uids_len, wb_accounts_total, buddy_ai_count) =
        apply_pool_snapshot(state, &pool, &wb_pool, &qoder_pool, &buddy_ai_pool);

    let healthy_count = pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-池状态: pool_size={} healthy={} port={}",
            pool_count, healthy_count, port,
        ),
    );

    // WB 池策略：wb_strategy 显式配置优先；空 = 加权公平轮转（WB 专属默认，2026-11 修复）
    let wb_strategy = PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy);
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

    // Qoder 池启动概况（p3-3 + 池内策略）：qoder_strategy 空 = 跟随 Trae 池策略
    //（resolve_qoder 独立跟随，不走 WB 池的公平轮转专属默认）
    let qoder_strategy =
        PoolStrategy::resolve_qoder(&pool_file.strategy, &pool_file.qoder_strategy);
    qoder_pool.set_strategy(qoder_strategy);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-Qoder上游池: enabled={} accounts={} strategy={}",
            pool_file.qoder_enabled,
            qoder_pool.count(),
            qoder_strategy.as_str(),
        ),
    );

    // buddy-ai 国际池启动概况（池内策略：buddy_ai_strategy 空 = 跟随 Trae 池，
    // 与 Qoder 池同语义（不走 WB 池专属默认）；开关默认 false，未启用时不产生告警噪音）
    let buddy_ai_strategy = PoolStrategy::resolve_buddy_ai(
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
            buddy_ai_count,
            buddy_ai_strategy.as_str(),
        ),
    );

    // 池为空时给出明确警告：Trae 池空 ≠ 全部资源不可用，Buddy(WB) 池可能正常服务
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

    let shared = Arc::new(ApiSharedState {
        pool,
        wb_pool,
        trae_enabled: std::sync::atomic::AtomicBool::new(pool_file.trae_enabled),
        qoder_pool,
        qoder_enabled: std::sync::atomic::AtomicBool::new(pool_file.qoder_enabled),
        // Qoder 凭证按次解析回调：PAT 换 24h 作业令牌 / 客户端通道惰性刷新
        //（ensure_fresh 全防护：并发锁 + 落库 + 指纹注入），池内 jwt 仅快照展示
        qoder_identity: {
            let st = state.clone();
            Some(std::sync::Arc::new(move |uid: &str| {
                let (creds, _, note) = crate::tasks::qoder_common::ensure_fresh(
                    &st,
                    &crate::tasks::qoder_upstream::qoder_agent(),
                    uid,
                    2,
                );
                match note {
                    // 暂态失败（网络/5xx → refresh_failed；refresh_busy（H3）→ 他进程
                    // 刷新中）带标记前缀，网关侧走 Server 熔断可自愈；仅永久失效
                    //（凭证缺失/过期需重登/PAT 被拒/refresh_token 被拒）走 SessionDead
                    // 永久禁用（此前 refresh_failed 裸 Err 会被误标永久禁用）；
                    // vault_down（issue #153 修复 2）：vault 系统性故障导致凭证
                    // 读空——暂态，同样走熔断，故障恢复后自动自愈
                    "refresh_failed" | "refresh_busy" | "vault_down" => Err(format!(
                        "{}qoder 凭证暂不可用(uid={uid}, status={note}；网络/服务端暂态或他进程正在刷新，稍后自动恢复)",
                        crate::tasks::qoder_common::TRANSIENT_ERR_TAG
                    )),
                    "no_credential" | "expired_needs_relogin" | "auth_dead" | "pat_rejected" => {
                        Err(note.to_string())
                    }
                    _ => Ok(creds),
                }
            })
                as std::sync::Arc<dyn Fn(&str) -> Result<crate::tasks::qoder_common::QoderCreds, String> + Send + Sync>)
        },
        qoder_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(
            pool_file.qoder_hedge_threshold_ms,
        ),
        qoder_sticky_enabled: std::sync::atomic::AtomicBool::new(
            pool_file.qoder_sticky_enabled,
        ),
        // 审查 #2：Qoder 线绑定带 "q:" 命名空间，与 WB 线共用 sticky_bindings 表互不串载
        qoder_sticky: StickyStore::load_ns(&state.data_dir, super::wb_sticky::QODER_NS),
        qoder_sanitize: std::sync::atomic::AtomicBool::new(true),
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
        trae_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(
            pool_file.trae_hedge_threshold_ms,
        ),
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
        // buddy-ai per-pool 三参数（§4.2 按池独立）：池粘性 TTL / 显式会话粘性开关
        //（默认关——国际档显式开启制）/ 竞速对冲阈值
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
        // Trae 池会话粘性存储（per-pool 批次新增）："t:" 命名空间与 wb/qoder 互不串绑
        trae_sticky: StickyStore::load_ns(&state.data_dir, super::wb_sticky::TRAE_NS),
        // buddy-ai 显式会话粘性存储（§4.2）："g:" 命名空间与 wb（无前缀）/q:/t: 互不串绑
        buddy_ai_sticky: StickyStore::load_ns(&state.data_dir, super::wb_sticky::BUDDY_AI_NS),
        // buddy-ai 独立池（§3.4 隔离清单）：开关默认关（dispatch 显式开启制）
        buddy_ai_pool,
        buddy_ai_enabled: std::sync::atomic::AtomicBool::new(pool_file.buddy_ai_enabled),
        wb_sticky: StickyStore::load(&state.data_dir),
        pool_sticky: Mutex::new(std::collections::HashMap::new()),
        // custom 上游组会话粘上游 + 组内轮询计数（P2 移植）：内存态不落盘，重启清零
        custom_upstream_sticky: Mutex::new(std::collections::HashMap::new()),
        custom_lb_counters: Mutex::new(std::collections::HashMap::new()),
        model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        // buddy-ai 模型级冷却独立实例（§4.2：Intl 池冷却表独立，CN 侧模型故障
        // 不得冷却 Intl 池同型号，反之亦然）
        buddy_ai_model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        default_model,
        data_dir: state.data_dir.clone(),
        total_requests: std::sync::atomic::AtomicU64::new(0),
        inflight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        active_uid: Mutex::new(None),
        last_error: Mutex::new(None),
        logger: ApiLogger::new(state.logs_dir()),
        debug_enabled: std::sync::atomic::AtomicBool::new(false),
        usage: Mutex::new(usage::load(&state.data_dir)),
        usage_dirty: Mutex::new(Vec::new()),
        wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        // buddy-ai 上游健康探针（G8 审查修复）：与 wb_probe 同频控口径，探测
        // BILLING_BASE_GLOBAL（workbuddy.ai）；-1 未探测 / 0 不可达 / 1 在线
        buddy_ai_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        buddy_ai_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        // Trae 401 自愈回调（issue #27 方案 B）：AppState 克隆进闭包，走 refresh_jwt_impl
        // 全防护链路（并发锁/冷却/轮换写回/5s 成功去重窗）强制刷新并持久化
        trae_jwt_refresh: {
            let st = state.clone();
            Some(std::sync::Arc::new(move |uid: &str| {
                crate::commands::accounts::refresh_jwt_impl(&st, uid, true)
            })
                as std::sync::Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>)
        },
        fallback_log_at: Mutex::new(std::collections::HashMap::new()),
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

    // 启动积分保鲜（issue #91 缓存问题；审查 C3）：池内存在陈旧/缺时间戳的
    // 积分快照时后台刷新一次——reserve 过滤对陈旧快照 fail-open，刷新拨新后
    // 自动恢复生效。三池口径：
    //   Trae 池：refresh_remaining_credits_impl 逐账号刷新，经 push_credits 回写拨新；
    //   WB 池：wb_credits_snapshot_task 快照后经 write_back_pool_balances 内部
    //     运行池句柄 update_credits_snapshot 三键拨新（issue #109 同类：
    //     credits/expire/fetched_at，ok=true 账号，失败账号保留旧值）；
    //   Qoder 池（issue #102）：run_snapshot_task 快照后按 ok 行经
    //     update_credits_snapshot 三键拨新（credits/expire_at/fetched_at）——
    //     selectable 零积分与到期过滤均无新鲜度自愈，装配时 Some(0.0)/已过期的
    //     账号在充值或续期后需拨新才恢复可选（此前注释「恒 credits=None 无保鲜
    //     需求」随三键透传已过时）。
    // 静默失败不影响启动流程。docker 适配：build_shared 仅 aiwork-server 启动
    // 调用一次（热重载走 reload_pools_after_change），此处一次性保鲜与 main 的
    // do_start 语义等价；Trae 臂回写依赖 gateway_shared() 注册，线程开头轮询等待。
    {
        let st2 = state.clone();
        let shared2 = shared.clone();
        std::thread::spawn(move || {
            // 等待网关注册（GATEWAY OnceLock 由 aiwork-server 启动流程写入），上限 30s；
            // Trae 臂 refresh_remaining_credits_impl 内部经 push_credits_if_running →
            // gateway_shared() 回写拨新，须注册就绪后才执行
            let mut registered = false;
            for _ in 0..300 {
                if gateway_shared().is_some() {
                    registered = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if !registered {
                return; // runtime 未就绪（启动中途失败）
            }
            let trae_stale = shared2.pool.has_stale_credits();
            let wb_stale = shared2.wb_pool.has_stale_credits();
            let qoder_stale = shared2.qoder_pool.has_zero_or_expired_credits();
            if !trae_stale && !wb_stale && !qoder_stale {
                return;
            }
            fs_utils::app_log(&st2.data_dir, "API服务启动-积分保鲜: 检测到陈旧积分快照, 后台刷新中");
            if trae_stale {
                match crate::commands::accounts::refresh_remaining_credits_impl(&st2) {
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
                // 拨新在 wb_credits_snapshot_task 内部经运行池句柄完成（ok 行三键拨新，
                // 对齐下方 Qoder 池口径）。docker 适配：main 版传 runtime Mutex 句柄；
                // server 单体直接传本函数装配好的 shared2（Arc<ApiSharedState>）
                match crate::commands::workbuddy::wb_credits_snapshot_task(&st2, Some(&shared2)) {
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
                // 拨新在 run_snapshot_task 内部经 gateway_shared() 完成（ok 行三键
                // 拨新）；永久失效账号（needs_relogin）其内部已落日志，这里只汇总
                match crate::tasks::qoder_credits::run_snapshot_task(&st2) {
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

    shared
}

/// 池到期表装配（issue #28）：通用积分（product_id != 209）最早到期优先，
/// 与 pool_credits 的通用口径对齐。回退规则：
/// - 通用剩余表未覆盖的老缓存账号（升级前未刷新过）→ 回退混合口径 expire_times
/// - 已刷新但通用包均长期有效/耗尽的账号（general 有键而 general_expire_times 无键）
///   → 不回退，无到期约束（混合口径里的 Work 包到期不得泄漏进调度）
pub(crate) fn merge_pool_expire_times(rc: &crate::models::RemainingCreditsFile) -> std::collections::HashMap<String, i64> {
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

/// 池装配公共逻辑（build_shared 构建 / 凭据变更热重载共用）：
/// 读取 vault 账号 + 池配置 + 分组 + 冷却 + 积分 + 设备映射，全量重建四池内条目
///（Trae/WB/Qoder/BuddyAI）。返回 (trae 池条目数, wb 白名单长度, wb 账号总数,
/// buddy-ai 池条目数) 供调用方记日志。
pub fn apply_pool_snapshot(
    state: &AppState,
    pool: &ApiPool,
    wb_pool: &ApiPool,
    qoder_pool: &ApiPool,
    buddy_ai_pool: &ApiPool,
) -> (usize, usize, usize, usize) {
    let accounts = crate::vault::load_accounts(state);
    // 走迁移入口（防止直读落 serde default 后整表写回静默覆盖迁移值）
    let pool_file: ApiPoolFile = load_pool_file(&state.data_dir);
    let groups_file: GroupsFile = store::docs::groups_load(&store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        store::docs::account_cooldowns_load(&store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        store::docs::remaining_credits_load(&store::db(&state.data_dir));
    let device_map: DeviceMap = store::docs::device_map_load(&store::db(&state.data_dir));
    // 池积分语义 = 通用积分（product_id 208，llm_utils_chat 实际扣减的类别）：
    // 优先取 general 表，账号未重新刷新过缓存时回退旧的总积分表
    let pool_credits: std::collections::HashMap<String, f64> = credits_file
        .credits
        .iter()
        .map(|(uid, c)| (uid.clone(), credits_file.general.get(uid).copied().unwrap_or(*c)))
        .collect();
    // 共享标签筛选（P2 需求④）：tag_uid_set 非空时仅纳入带任一所选标签的账号，
    // 与分组筛选取交集（D5 池语义：组 ∩ 标签，均空 = 不限）
    let tag_map = crate::store::docs::account_tags_map(&store::db(&state.data_dir));
    let trae_tag_filter =
        crate::api_server::org_index::tag_uid_set(&pool_file.trae_tag_ids, &tag_map);
    // 积分快照文件级采集时间（issue #91 缓存保鲜）：串 → unix 秒。
    // 一次刷新全量写入 credits 与 updated_at，文件级时间戳对全体条目生效；
    // 池据此判断 reserve 过滤新鲜度（陈旧快照 fail-open，见 pool.rs）。
    // 走 fs_utils::parse_ts_loose：本机写入的是 **无时区偏移的本地裸串**
    // （now_iso/now_ts），严格 rfc3339 解析恒失败会让 reserve 过滤结构性失效
    let credits_fetched_at = credits_file
        .updated_at
        .as_deref()
        .and_then(fs_utils::parse_ts_loose);
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
    let wb_accounts_all = wb_upstream_accounts(state);
    // Buddy 池分组筛选（对齐 Trae 池 T10 语义）：wb_group_ids 非空时仅纳入所选分组的
    // WB 账号，未分组账号不参与；空 = 不限分组。筛选在装配层先行完成，
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
    // Qoder 池同步（api_pool.json.qoder_enabled_uids 白名单 + qoder_group_ids 分组筛选，
    // 对齐 Buddy 池 wb_group_filter 语义：非空时仅纳入所选分组的账号，未分组不参与；
    // token 空不入池；积分三键快照入池（issue #91 reserve 过滤 + issue #102 智能调度））
    let qoder_accounts_all = qoder_upstream_accounts(state);
    let qoder_group_filter: Option<std::collections::HashSet<&str>> =
        if pool_file.qoder_group_ids.is_empty() {
            None
        } else {
            Some(pool_file.qoder_group_ids.iter().map(|s| s.as_str()).collect())
        };
    // Qoder 池标签筛选（与分组合取交集，D5）
    let qoder_tag_filter =
        crate::api_server::org_index::tag_uid_set(&pool_file.qoder_tag_ids, &tag_map);
    let qoder_accounts: Vec<_> = qoder_accounts_all
        .into_iter()
        .filter(|a| {
            let group_ok = qoder_group_filter
                .as_ref()
                .map_or(true, |f| f.contains(a.group_id.as_str()));
            // tag_map 对 Qoder 的键即账号 id（qd-xxx）；QoderSyncAccount.uid 承载账号 id，
            // 与 main 侧 `t.contains(&a.id)` 语义等价（main 的 QoderSyncAccount.uid 亦取 a.id）
            let tag_ok = qoder_tag_filter.as_ref().map_or(true, |t| t.contains(&a.uid));
            group_ok && tag_ok
        })
        .collect();
    // 生效白名单 fail-open 解析（对齐 Buddy 池 effective_wb_uids）：显式白名单优先，
    // 空 = 全部含凭证账号自动入池——直传空白名单会导致 Qoder 池恒为空
    //（支持模型 qoder 源恒「未启用」、调度配置无 Qoder 候选）
    let qoder_uids: Vec<String> = qoder_accounts.iter().map(|a| a.uid.clone()).collect();
    qoder_pool.sync_from_qoder(&qoder_accounts, &effective_qoder_uids(&pool_file, &qoder_uids));
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
    (
        pool.count(),
        wb_uids.len(),
        wb_accounts.len(),
        buddy_ai_pool.count(),
    )
}

/// Buddy 池生效入池白名单（纯函数，便于单测）：显式 wb_enabled_uids 优先；
/// 空时兼容旧数据——旧版 WB 账号混存于共享 enabled_uids（wb- 前缀条目），有则沿用；
/// 两者皆空 = 全部含凭证账号自动入池。
pub fn effective_wb_uids(
    pf: &ApiPoolFile,
    wb_accounts: &[WbSyncAccount],
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
pub(crate) fn effective_qoder_uids(pf: &ApiPoolFile, qoder_ids: &[String]) -> Vec<String> {
    if !pf.qoder_enabled_uids.is_empty() {
        return pf.qoder_enabled_uids.clone();
    }
    qoder_ids.to_vec()
}

// ── WB 上游取号（自 commands/workbuddy/accounts.rs wb_upstream_accounts 下沉）──

/// wb_accounts 表的最小反序列化视图（仅网关装配所需字段；serde 契约与
/// WorkBuddyAccount 一致——snake_case，无 rename_all）
#[derive(serde::Deserialize, Default)]
struct WbPoolLite {
    #[serde(default)]
    accounts: Vec<WbAcctLite>,
}

#[derive(serde::Deserialize, Default, Clone)]
struct WbAcctLite {
    #[serde(default)]
    id: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    needs_relogin: bool,
    #[serde(default)]
    group_id: String,
    #[serde(default)]
    credits_balance: Option<f64>,
    /// 最早积分包到期（issue #28 调度口径透传）；老数据缺省 None
    #[serde(default)]
    credits_expire_at: Option<i64>,
    /// 积分快照采集时间（issue #91 保鲜）：wb_accounts 表 rfc3339 时间戳，
    /// 池装配时换算 unix 秒供池判断 reserve 过滤新鲜度；老数据缺省 None
    #[serde(default)]
    credits_fetched_at: Option<String>,
}

/// 宽容字符串字段提取（对齐 fs_utils::dig 语义的本地 helper）
fn as_str(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 汇总 WB 上游账号：账号池启用账号 + token store 凭证 → WbSyncAccount。
/// 区域判定（§5.2）：domain 含 `.workbuddy.ai` → Global（chat 全走 www.workbuddy.ai）。
/// 仅纳入有工具侧凭证副本的账号（auth 文件为只读态，不在此兜底）。
pub fn wb_upstream_accounts(state: &AppState) -> Vec<WbSyncAccount> {
    let pool: WbPoolLite =
        serde_json::from_value(store::docs::wb_pool_load(&store::db(&state.data_dir)))
            .unwrap_or_default();
    // 凭证收敛（P0-1）：读走 secure 回填（vault 取明文），DB 只留占位空串——
    // 直读 DB 会导致收敛后 token 恒空、账号不入池（审查 #8）
    let tokens = crate::tasks::wb_common::token_store_load_secure(&state.data_dir)
        .get("tokens")
        .and_then(|t| t.as_object())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in &pool.accounts {
        let rec = tokens.get(&a.id).cloned().unwrap_or_default();
        let token = as_str(fs_utils::dig(&rec, &["access_token"])).unwrap_or_default();
        if token.is_empty() {
            continue;
        }
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        let eid = as_str(fs_utils::dig(&rec, &["enterprise_id", "enterpriseId"])).unwrap_or_default();
        out.push(WbSyncAccount {
            uid: a.id.clone(),
            name: if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() },
            token,
            domain: domain.clone(),
            enterprise_id: eid,
            global_region: domain.contains(".workbuddy.ai"),
            credits: a.credits_balance,
            credits_expire_at: a.credits_expire_at,
            // 走 fs_utils::parse_ts_loose：WB 侧 fetched_at 为 now_ts() 空格分隔裸串
            // （无时区偏移），严格 rfc3339 恒失败 → reserve 过滤永不生效
            credits_fetched_at: a
                .credits_fetched_at
                .as_deref()
                .and_then(fs_utils::parse_ts_loose),
            needs_relogin: a.needs_relogin,
            group_id: a.group_id.clone(),
        });
    }
    out
}

// ── Qoder 上游取号（与 wb_upstream_accounts 同构）────────────────────────────

/// 汇总 Qoder 上游账号：qoder_accounts 表账号 + token store 凭证 → QoderSyncAccount。
/// 池内 access_token 仅快照展示（请求时经 qoder_identity 回调按次解析凭证）；
/// machine_id 为账号绑定指纹透传（COSY 头签名依赖）。仅纳入有凭证的账号。
pub fn qoder_upstream_accounts(state: &AppState) -> Vec<crate::api_server::pool::QoderSyncAccount> {
    use serde_json::Value;
    let pool = crate::store::docs::qoder_pool_load(&store::db(&state.data_dir));
    let tokens = crate::tasks::qoder_common::load_token_store(state)
        .get("tokens")
        .and_then(|t| t.as_object())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in pool
        .get("accounts")
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
    {
        let uid = a.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if uid.is_empty() {
            continue;
        }
        let rec = tokens.get(&uid).cloned().unwrap_or_default();
        let access_token = as_str(fs_utils::dig(&rec, &["access_token"])).unwrap_or_default();
        if access_token.is_empty() {
            continue;
        }
        out.push(crate::api_server::pool::QoderSyncAccount {
            name: as_str(fs_utils::dig(a, &["name", "nickname"]))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| uid.clone()),
            machine_id: as_str(fs_utils::dig(a, &["device_profile", "machine_id"]))
                .unwrap_or_default(),
            needs_relogin: a
                .get("needs_relogin")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            uid,
            access_token,
            group_id: as_str(fs_utils::dig(a, &["group_id"])).unwrap_or_default(),
            // issue #91 三池统一：Qoder 余额快照（credits_fetch / qoder-credits-snapshot
            // 回写 QoderAccount.credits_balance + credits_fetched_at）入池，使 reserve
            // 保留额过滤在 Qoder 池同样生效。旧数据无字段 → None → fail-open
            credits: a.get("credits_balance").and_then(Value::as_f64),
            // issue #102 智能调度：最早积分包到期随 qoder_credits 回写链路入池
            //（smart 排序键①「先到期先消耗」+ selectable 到期过滤数据源；旧数据 None）
            credits_expire_at: a.get("credits_expire_at").and_then(Value::as_i64),
            credits_fetched_at: a
                .get("credits_fetched_at")
                .and_then(Value::as_str)
                .and_then(fs_utils::parse_ts_loose),
        });
    }
    out
}

// ── 网关共享句柄注册（server 单体常驻运行）──────────────────────────────────

/// 网关共享状态全局注册点：server main 启动时 build_shared 后 set 一次，
/// commands 层（pool_set / oauth_login / cooldown 清除等凭据变更方）据此取用做热重载。
static GATEWAY: std::sync::OnceLock<Arc<ApiSharedState>> = std::sync::OnceLock::new();

/// 注册网关共享状态（重复 set 静默忽略，首注册生效）
pub fn set_gateway_shared(shared: Arc<ApiSharedState>) {
    let _ = GATEWAY.set(shared);
}

/// 已注册的网关共享状态；None = 网关未运行（CLI 任务模式 / 启动早期）
pub fn gateway_shared() -> Option<Arc<ApiSharedState>> {
    GATEWAY.get().cloned()
}

/// 凭据/池配置变更后热重载（原 commands/api_server.rs `reload_pools_if_running` 语义）：
/// 网关在运行时全量重建四池条目（Trae/WB/Qoder/BuddyAI）；未注册（CLI 模式）时静默跳过。
pub fn reload_pools_after_change(state: &AppState) {
    let Some(shared) = gateway_shared() else { return };
    let (n, wb_uids, wb_total, buddy_ai) = apply_pool_snapshot(
        state,
        &shared.pool,
        &shared.wb_pool,
        &shared.qoder_pool,
        &shared.buddy_ai_pool,
    );
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "凭据变更热重载: trae_pool={n} wb_uids={wb_uids} wb_accounts={wb_total} buddy_ai_pool={buddy_ai}"
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Buddy 池白名单优先级：显式 > 旧版混存 wb- 前缀 > 全部含凭证账号
    #[test]
    fn effective_wb_uids_priority() {
        let mut pf = ApiPoolFile::default();
        pf.wb_enabled_uids = vec!["wb-a".into()];
        pf.enabled_uids = vec!["uid-1".into(), "wb-old".into()];
        let accs = vec![WbSyncAccount {
            uid: "wb-b".into(),
            name: "b".into(),
            token: "t".into(),
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
            credits: None,
            credits_expire_at: None,
            credits_fetched_at: None,
            needs_relogin: false,
            group_id: String::new(),
        }];
        // 显式优先
        assert_eq!(effective_wb_uids(&pf, &accs), vec!["wb-a".to_string()]);
        // 显式为空 → 旧版混存 wb- 条目沿用
        pf.wb_enabled_uids.clear();
        assert_eq!(effective_wb_uids(&pf, &accs), vec!["wb-old".to_string()]);
        // 两者皆空 → 全部含凭证账号
        pf.enabled_uids.clear();
        assert_eq!(effective_wb_uids(&pf, &accs), vec!["wb-b".to_string()]);
    }

    /// Qoder 池白名单 fail-open：显式优先；空 = 全部含凭证账号自动入池
    #[test]
    fn effective_qoder_uids_failopen_and_explicit() {
        let mut pf = ApiPoolFile::default();
        pf.qoder_enabled_uids = vec!["qd-1".into()];
        let ids = vec!["qd-1".to_string(), "qd-2".to_string()];
        // 显式白名单优先（交集过滤由 sync_from_qoder 完成）
        assert_eq!(effective_qoder_uids(&pf, &ids), vec!["qd-1".to_string()]);
        // 空白名单 = fail-open 全量候选
        pf.qoder_enabled_uids.clear();
        assert_eq!(effective_qoder_uids(&pf, &ids), ids);
    }

    // ── merge_pool_expire_times（issue #28：调度到期 = 通用口径）────────────

    #[test]
    fn merge_expires_prefers_general_table() {
        // 新旧两表同账号并存 → 通用口径胜出（Work 包污染的混合值不参与）
        let mut rc = crate::models::RemainingCreditsFile::default();
        rc.expire_times.insert("u1".into(), 1000);
        rc.general.insert("u1".into(), 90.0);
        rc.general_expire_times.insert("u1".into(), 2000);
        let got = merge_pool_expire_times(&rc);
        assert_eq!(got.get("u1"), Some(&2000));
    }

    #[test]
    fn merge_expires_legacy_fallback_without_general_cache() {
        // 老缓存账号（通用剩余表未覆盖，升级前未刷新过）→ 回退混合口径保底
        let mut rc = crate::models::RemainingCreditsFile::default();
        rc.expire_times.insert("legacy".into(), 1000);
        let got = merge_pool_expire_times(&rc);
        assert_eq!(got.get("legacy"), Some(&1000));
    }

    #[test]
    fn merge_expires_no_fallback_when_refreshed_without_general_expiry() {
        // 已刷新（general 有键）但通用包均长期有效/耗尽（general_expire_times 无键）
        // → 不回退，混合口径里的 Work 包到期不得泄漏进调度
        let mut rc = crate::models::RemainingCreditsFile::default();
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
        let dir = std::env::temp_dir().join(format!("aiwork_merge_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let s = store::db(&dir);
            let mut rc = crate::models::RemainingCreditsFile::default();
            rc.general.insert("u1".into(), 90.0);
            rc.expire_times.insert("u1".into(), 1000);
            rc.general_expire_times.insert("u1".into(), 2000);
            crate::store::docs::remaining_credits_save(&s, &rc).unwrap();
        }
        let got = {
            let s = store::db(&dir);
            let rc = crate::store::docs::remaining_credits_load(&s);
            merge_pool_expire_times(&rc)
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.get("u1"), Some(&2000));
    }

    // ── load_pool_file_with_legacy_migration（per-pool 拆分读取侧回填）─────────

    #[test]
    fn legacy_migration_buddy_inherits_shared_values() {
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
    fn legacy_migration_explicit_wb_fields_win_over_legacy() {
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
    fn legacy_migration_missing_or_invalid_raw_falls_back_to_default() {
        // 无记录 / 非法 JSON：全默认（与 kv_get 回退语义一致）
        let pf = load_pool_file_with_legacy_migration(None);
        assert_eq!(pf.wb_account_concurrency_limit, 1);
        assert_eq!(pf.wb_pool_sticky_ttl_secs, 300);
        let bad = load_pool_file_with_legacy_migration(Some("not-json".into()));
        assert_eq!(bad.wb_account_concurrency_limit, 1);
        assert_eq!(bad.wb_pool_sticky_ttl_secs, 300);
    }

    #[test]
    fn legacy_migration_value_overflow_clamped_to_u32_max() {
        // 旧并发值超出 u32 范围：钳制为 u32::MAX（防 as 转换静默截断）
        let raw = r#"{ "account_concurrency_limit": 99999999999 }"#;
        let pf = load_pool_file_with_legacy_migration(Some(raw.into()));
        assert_eq!(pf.wb_account_concurrency_limit, u32::MAX);
    }
}
