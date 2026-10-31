//! API 服务网关管理命令（自 src-tauri/src/commands/api_server.rs 平移，T5）。
//! server 单体中网关常驻运行（main.rs 启动即注册共享状态）：启停命令、托盘同步、
//! 运行时句柄（ApiServerRuntime）不平移；原句柄参数改经
//! `crate::api_server::runtime::gateway_shared()` 读全局注册的共享状态。

use crate::models::{ApiPoolFile, PoolStatus, RemainingCreditsFile};
use crate::state::AppState;

use crate::api_server::models_sync;
use crate::api_server::ApiLogger;

// ==================== 池管理命令 ====================

pub fn pool_list(state: &AppState) -> ApiPoolFile {
    // 统一走迁移入口：直读 kv_get 会落 serde default（旧共享字段已退役），
    // 保证未启动服务时 pool_list 展示与启动热应用取值一致
    crate::api_server::runtime::load_pool_file(&state.data_dir)
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
    // Qoder 上游开关（p3-3）；None = 保留原值
    qoder_enabled: Option<bool>,
    // Qoder 竞速对冲阈值毫秒（F-80-余 v2，0 = 关闭）；None = 保留原值
    qoder_hedge_threshold_ms: Option<u64>,
    // Qoder 会话粘性开关（F-80-余 v2）；None = 保留原值
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
        trae_hedge_threshold_ms: trae_hedge_threshold_ms
            .unwrap_or(existing.trae_hedge_threshold_ms),
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
        qoder_enabled_uids: qoder_uids.unwrap_or_else(|| existing.qoder_enabled_uids.clone()),
        qoder_hedge_threshold_ms: qoder_hedge_threshold_ms
            .unwrap_or(existing.qoder_hedge_threshold_ms),
        qoder_sticky_enabled: qoder_sticky_enabled.unwrap_or(existing.qoder_sticky_enabled),
        qoder_flash_free_rate: qoder_flash_free_rate.unwrap_or(existing.qoder_flash_free_rate),
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
/// 策略部分热应用：网关常驻运行（共享状态已注册）立即生效。
#[allow(clippy::too_many_arguments)]
pub fn pool_set(
    state: &AppState,
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
    // Qoder 竞速对冲阈值毫秒（0 = 关闭）；None = 保留原值
    qoder_hedge_threshold_ms: Option<u64>,
    // Qoder 会话粘性开关；None = 保留原值
    qoder_sticky_enabled: Option<bool>,
    // Qwen3.8-Flash 免费活动倍率覆盖开关（issue #111，默认开）；None = 保留原值
    qoder_flash_free_rate: Option<bool>,
    // Qoder 池入池白名单（qd- 前缀账号 id）；None = 保留原值，
    // Some(list) = 覆盖（Qoder 页账号池勾选保存；空数组 = fail-open 全量入池）
    qoder_uids: Option<Vec<String>>,
    // Qoder 池分组筛选（qoder_groups 分组 id）；None = 保留原值，
    // Some(list) = 覆盖（Qoder 页分组筛选保存；空数组 = 不限分组）
    qoder_group_ids: Option<Vec<String>>,
    // Qoder 池内调度策略；空串 = 跟随 Trae 池（同 wb_strategy 语义）；None = 保留原值
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
    // 读-改-写互斥（审查 B1）：整文档覆盖写，防与其他写点并发时旧副本覆盖丢更新
    let _pool_cfg = crate::api_server::runtime::api_pool_cfg_lock();
    let existing: ApiPoolFile = crate::api_server::runtime::load_pool_file(&state.data_dir);
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
    // 签名不动，存量单测零改动）；运行中热应用到 AtomicBool
    if let Some(v) = buddy_ai_enabled {
        pool_file.buddy_ai_enabled = v;
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
    drop(_pool_cfg); // 写回完成即释放；下方热应用段读内存值，不持配置锁
    // 热应用：网关运行中即改内存池策略（Buddy 池空值沿用 Trae 池策略，与启动逻辑一致）
    if let Some(shared) = crate::api_server::runtime::gateway_shared() {
        shared
            .pool
            .set_strategy(crate::api_server::pool::PoolStrategy::parse(&pool_file.strategy));
        shared.wb_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy),
        );
        // Qoder 池策略热应用（空 = 跟随 Trae 池，与启动逻辑一致）
        shared.qoder_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_qoder(&pool_file.strategy, &pool_file.qoder_strategy),
        );
        // buddy-ai 池策略热应用（空 = 跟随 Trae 池，与启动逻辑一致）
        shared.buddy_ai_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_buddy_ai(
                &pool_file.strategy,
                &pool_file.buddy_ai_strategy,
            ),
        );
        // per-pool 三参数热应用（F-76②/F-77 拆分版）：并发上限 / 池粘性 TTL /
        // 显式会话粘性 TTL 三池各自生效
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
            .trae_sticky
            .set_explicit_ttl(pool_file.trae_sticky_ttl_secs as i64);
        // Trae 池竞速对冲阈值热应用（F-76③ 同构，per-pool 独立配置）
        shared.trae_hedge_threshold_ms.store(
            pool_file.trae_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.wb_sticky.set_explicit_ttl(pool_file.wb_sticky_ttl_secs as i64);
        shared
            .qoder_sticky
            .set_explicit_ttl(pool_file.qoder_sticky_ttl_secs as i64);
        // buddy-ai 池粘性 TTL + 显式会话粘性热应用（§4.2）
        shared.buddy_ai_pool_sticky_ttl_secs.store(
            pool_file.buddy_ai_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.buddy_ai_sticky_enabled.store(
            pool_file.buddy_ai_sticky_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        // buddy-ai per-pool 三参数热应用（并发上限 / 显式会话粘性 TTL / 对冲阈值）
        shared
            .buddy_ai_pool
            .set_concurrency_limit(pool_file.buddy_ai_account_concurrency_limit);
        shared
            .buddy_ai_sticky
            .set_explicit_ttl(pool_file.buddy_ai_sticky_ttl_secs as i64);
        shared.buddy_ai_hedge_threshold_ms.store(
            pool_file.buddy_ai_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        // buddy-ai 上游开关热应用（dispatch 显式开启制）
        shared.buddy_ai_enabled.store(
            pool_file.buddy_ai_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.wb_longctx_downgrade.store(
            pool_file.wb_longctx_downgrade,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.wb_hedge_threshold_ms.store(
            pool_file.wb_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.trae_pool_sticky_ttl_secs.store(
            pool_file.trae_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.wb_pool_sticky_ttl_secs.store(
            pool_file.wb_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.qoder_pool_sticky_ttl_secs.store(
            pool_file.qoder_pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Buddy 资源开关热应用（此前仅启动时读取，改动需重启服务生效）
        shared
            .wb_enabled
            .store(pool_file.wb_enabled, std::sync::atomic::Ordering::Relaxed);
        // Trae 池开关热应用（每池自管开关，默认开）
        shared.trae_enabled.store(
            pool_file.trae_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.wb_default_thinking.store(
            pool_file.wb_default_thinking,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared
            .wb_tool_exec
            .store(pool_file.wb_tool_exec, std::sync::atomic::Ordering::Relaxed);
        shared
            .wb_bg_downgrade
            .store(pool_file.wb_bg_downgrade, std::sync::atomic::Ordering::Relaxed);
        // Qoder 上游开关 + 竞速对冲阈值 + 会话粘性热应用（p3-3 / F-80-余 v2）
        shared
            .qoder_enabled
            .store(pool_file.qoder_enabled, std::sync::atomic::Ordering::Relaxed);
        shared.qoder_hedge_threshold_ms.store(
            pool_file.qoder_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.qoder_sticky_enabled.store(
            pool_file.qoder_sticky_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Qwen3.8-Flash 免费活动倍率覆盖热应用（issue #111）
        shared.qoder_flash_free_rate.store(
            pool_file.qoder_flash_free_rate,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    // 成员/分组热应用：此前仅策略/参数热生效，成员变更要求重启服务；
    // 现统一走凭据/成员变更联动热重载，保存账号池后立即生效
    crate::api_server::runtime::reload_pools_after_change(state);
    Ok(())
}

/// 返回运行中池的实时状态（冷却/积分等）；网关未注册时返回空数组
pub fn pool_status() -> Vec<PoolStatus> {
    match crate::api_server::runtime::gateway_shared() {
        Some(shared) => shared.pool.status_list(),
        None => vec![],
    }
}

/// 返回运行中 WB 池的实时状态（F-77⑤ 可观测：含 per-account inflight 在途计数）；
/// 网关未注册时返回空数组
pub fn wb_pool_status() -> Vec<PoolStatus> {
    match crate::api_server::runtime::gateway_shared() {
        Some(shared) => shared.wb_pool.status_list(),
        None => vec![],
    }
}

/// 返回运行中 Qoder 池的实时状态（Qoder「资源调度」页可观测，含 per-account
/// inflight 在途计数）；网关未注册时返回空数组
pub fn qoder_pool_status() -> Vec<PoolStatus> {
    match crate::api_server::runtime::gateway_shared() {
        Some(shared) => shared.qoder_pool.status_list(),
        None => vec![],
    }
}

/// 返回运行中 Buddy 国际版（buddy-ai）池的实时状态（全局 API 管理·API Keys
/// 调度配置候选 + 摘要卡可观测，含 per-account inflight 在途计数）；
/// 网关未注册时返回空数组
pub fn buddy_ai_pool_status() -> Vec<PoolStatus> {
    match crate::api_server::runtime::gateway_shared() {
        Some(shared) => shared.buddy_ai_pool.status_list(),
        None => vec![],
    }
}

// ==================== 池级指纹清洗开关（issue #158 P1-4，移植 main@be198af） ====================

/// 池级指纹清洗开关视图：`wb_sanitize`（wb + Trae 池共用）与 `qoder_sanitize`
/// （Qoder 池独立，池级风控画像分开治理）。清洗是防风控手段，默认恒开；
/// 网关未注册时返回错误（与 api_debug_toggle 同口径）——前端按加载失败
/// 置灰展示，避免「可点但保存必失败」的误导态（审查修复：此前回显默认值，
/// 置灰设计失效）
pub fn sanitize_switches_get() -> Result<serde_json::Value, String> {
    let shared = crate::api_server::runtime::gateway_shared()
        .ok_or_else(|| "API 服务未运行，开关不可用".to_string())?;
    Ok(serde_json::json!({
        "wb": shared.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed),
        "qoder": shared
            .qoder_sanitize
            .load(std::sync::atomic::Ordering::Relaxed),
    }))
}

/// 池级指纹清洗开关热切换：仅改内存 AtomicBool，下一请求即生效，无持久化
/// （重启复位默认开——关清洗属临时排障手段，不做持久默认）
pub fn sanitize_switch_set(pool: &str, enabled: bool) -> Result<(), String> {
    let shared = crate::api_server::runtime::gateway_shared()
        .ok_or_else(|| "API 服务未运行，开关不可用".to_string())?;
    let (flag, label) = match pool {
        "wb" => (&shared.wb_sanitize, "wb/Trae"),
        "qoder" => (&shared.qoder_sanitize, "qoder"),
        other => return Err(format!("未知清洗开关池: {other}")),
    };
    flag.store(enabled, std::sync::atomic::Ordering::Relaxed);
    // 关清洗是排障动作（会重新暴露客户端指纹、可能触发渠道风控），落一行
    // app.log 留痕：事后复盘「何时关过哪个池」可查，无需依赖用户记忆
    crate::fs_utils::app_log(
        &shared.data_dir,
        &format!(
            "指纹清洗开关热切换：pool={label} enabled={enabled}（仅内存生效，网关重启复位为开启）"
        ),
    );
    Ok(())
}

/// 网关运行中回写 Trae 池积分快照（issue #67，移植 main@36d628f）：
/// refresh_remaining_credits 刷新后调用，让 expire_first 等策略基于最新积分选号。
/// 此前刷新只写库不回写池，秃号内存中 credits>0 永远 selectable、粘性/策略持续
/// 命中同一账号。网关未注册（启动早期）时 no-op；仅回写 Trae 池（WB/Qoder 池积分
/// 随各自 sync_* 数据链走，不适用）。
/// docker 适配：main 版取 `Mutex<Option<ApiServerRuntime>>` 句柄；server 单体网关
/// 常驻运行，改经 `gateway_shared()` 读全局注册的共享状态。
pub fn push_credits_if_running(state: &AppState) {
    let Some(shared) = crate::api_server::runtime::gateway_shared() else {
        return;
    };
    let credits_file: RemainingCreditsFile =
        crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));
    // 三键拨新（issue #109）：此前只拨余额两键，credits_expire_at 钉死在装配时刻——
    // 网关启动后新登录/新刷新的账号池内 expire=None，被 expire_first
    // 「有过期时间者优先」恒压（调度持续命中带旧到期信息的老账号，新会话同样命中）；
    // 充值续期后的新到期也进不了池，旧值已过期的账号甚至被 selectable 整体排除。
    // 积分与到期均对齐 apply_pool_snapshot 装配口径：优先 general 表，到期用
    // merge_pool_expire_times（general 优先、混合口径兜底），池状态与重启后一致
    let expire_times = crate::api_server::runtime::merge_pool_expire_times(&credits_file);
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
    shared.pool.update_credits_snapshot(&pool_snapshot);
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!("API服务池积分回写(运行期): n={}", pool_snapshot.len()),
    );
}

/// 列出 API 日志可用日期列表
pub fn api_logs_list(state: &AppState) -> Vec<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.list_dates(30)
}

/// 读取指定日期的 API 日志内容
pub fn api_logs_detail(state: &AppState, date: String) -> Option<String> {
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

pub fn api_logs_search(state: &AppState, opts: ApiLogSearchOpts) -> Option<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.search_log(
        &opts.date,
        opts.start_time.as_deref().unwrap_or(""),
        opts.end_time.as_deref().unwrap_or(""),
        opts.keyword.as_deref().unwrap_or(""),
    )
}

/// 切换 API Debug 模式（开启后记录完整请求/响应）
pub fn api_debug_toggle() -> Result<bool, String> {
    let Some(shared) = crate::api_server::runtime::gateway_shared() else {
        return Err("网关未启动".into());
    };
    let current = shared
        .debug_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let new_val = !current;
    shared
        .debug_enabled
        .store(new_val, std::sync::atomic::Ordering::Relaxed);
    Ok(new_val)
}

/// 查询 API Debug 模式状态
pub fn api_debug_status() -> bool {
    crate::api_server::runtime::gateway_shared()
        .map_or(false, |shared| {
            shared
                .debug_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
        })
}

// ==================== 模型列表命令 ====================

/// 读取模型列表（api_models.json，缺失时写入默认列表）
pub fn api_models_list(state: &AppState) -> Vec<models_sync::ModelOption> {
    models_sync::load_models(&state.data_dir)
}

/// 从官网配置接口同步模型列表（batch_get_detail_param，不消耗积分）
pub async fn api_models_sync(state: &AppState) -> Result<Vec<models_sync::ModelOption>, String> {
    let data_dir = state.data_dir.clone();
    // 预先在调用方解密账号（vault 依赖 AppState，阻塞线程内不便访问）
    let accounts = crate::vault::load_accounts(state);
    // 阻塞网络请求放入阻塞线程池，避免卡住异步运行时
    tokio::task::spawn_blocking(move || models_sync::fetch_official(&data_dir, accounts))
        .await
        .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// WB 上游模型目录同步实现（手动同步命令与调度器 wb-catalog-sync 共用）：
/// 取首个含凭证的 WB 账号拉取并替换目录；无凭证账号时 Err（调度侧转为静默跳过）
pub(crate) fn wb_catalog_sync_impl(state: &AppState) -> Result<usize, String> {
    let accounts = crate::api_server::runtime::wb_upstream_accounts(state);
    let acct = accounts
        .first()
        .ok_or("无可用 WB 账号凭证，无法拉取上游目录")?;
    crate::api_server::wb_catalog::fetch_and_replace(
        &state.data_dir,
        &acct.uid,
        &acct.token,
        &acct.domain,
        &acct.enterprise_id,
        acct.global_region,
    )
}

/// 从 WB 上游模型目录接口同步 wb_model_catalog.json（T5.1/F-37，动态替换；
/// 网关启动时已自动做一次，此命令供手动刷新）。取任一含凭证的 WB 账号。
pub async fn api_wb_catalog_sync(state: &AppState) -> Result<usize, String> {
    // 阻塞网络请求放入阻塞线程池，避免卡住异步运行时
    let data_dir = state.data_dir.clone();
    let accounts = crate::api_server::runtime::wb_upstream_accounts(state);
    tokio::task::spawn_blocking(move || {
        let acct = accounts.first().ok_or("无可用 WB 账号凭证，无法拉取上游目录")?;
        crate::api_server::wb_catalog::fetch_and_replace(
            &data_dir,
            &acct.uid,
            &acct.token,
            &acct.domain,
            &acct.enterprise_id,
            acct.global_region,
        )
    })
    .await
    .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// 列出 wb_model_catalog.json 中的 WB 模型（Buddy API 服务页展示模型 id/倍率/档位）
pub fn api_wb_catalog_list(state: &AppState) -> Vec<crate::api_server::wb_catalog::WbModel> {
    crate::api_server::wb_catalog::load(&state.data_dir)
}

/// Buddy 国际版（buddy-ai）模型目录：kv 动态目录（/v3/config 同步落盘）优先，
/// 缺失回落内置静态表（§4.4）。此前恒回静态表（倍率占位 1.0、能力字段全空），
/// 「同步官网模型」落盘后表格仍不更新的根因。
pub fn api_buddy_ai_catalog_list(state: &AppState) -> Vec<crate::api_server::wb_catalog::WbModel> {
    crate::api_server::wb_catalog::buddy_ai_load(&state.data_dir)
}

/// 查询最近 N 天的 API 用量统计（Trae 模型请求桶，按日聚合，直接读盘，服务未运行也可查）
pub fn api_usage_stats(state: &AppState, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent(&state.data_dir, days, false)
}

/// 查询最近 N 天的 WB 上游用量统计（wb_days 桶，Buddy「API 服务」页专用，与 Trae 侧分账）
pub fn api_wb_usage_stats(state: &AppState, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent(&state.data_dir, days, true)
}

/// 查询最近 N 天的自定义模型用量统计（custom_days 桶，API 管理·用量统计「自定义」筛选专用）
pub fn api_custom_usage_stats(state: &AppState, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent_in(
        &state.data_dir,
        days,
        crate::api_server::usage::UsageBucket::Custom,
    )
}

/// 查询最近 N 天的 buddy-ai 国际池用量统计（buddy_ai_days 桶，BuddyAI「积分看板·模型消耗排行」专用）
pub fn api_buddy_ai_usage_stats(
    state: &AppState,
    days: Option<u32>,
) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(30).clamp(1, 90);
    crate::api_server::usage::query_recent_in(
        &state.data_dir,
        days,
        crate::api_server::usage::UsageBucket::BuddyAi,
    )
}

/// 手动同步 Qoder 模型目录（Qoder「资源调度」页；复用每日调度任务入口
/// qoder_catalog::run_task，按池序逐可用账号拉取 CN 区 model/list 并原子采纳）。
/// 返回采纳的模型数；无账号/无凭证返回 Ok(0) 语义的信息（与调度器静默跳过一致）
pub async fn api_qoder_catalog_sync(state: &AppState) -> Result<usize, String> {
    // AppState 可 Clone 语义的字段快照（Arc 锁与真实状态共享，token 刷新互斥不失效）
    let st = AppState {
        data_dir: state.data_dir.clone(),
        jwt_refresh_lock: state.jwt_refresh_lock.clone(),
        qoder_pool_lock: state.qoder_pool_lock.clone(),
        wb_pool_lock: state.wb_pool_lock.clone(),
        buddy_ai_pool_lock: state.buddy_ai_pool_lock.clone(),
        group_defs_lock: state.group_defs_lock.clone(),
        events: state.events.clone(),
    };
    // 阻塞网络请求放入阻塞线程池，避免卡住异步运行时
    tokio::task::spawn_blocking(move || {
        // run_task 返回 json：{ok:true, models:n} 或 {ok:true, skipped:...}
        let out = crate::tasks::qoder_catalog::run_task(&st)?;
        if let Some(n) = out.get("models").and_then(serde_json::Value::as_u64) {
            Ok(n as usize)
        } else {
            // 审查 #14：空池/无可用账号属正常跳过（qoder_catalog 空池返回
            // {ok:true,skipped:...}），按 0 个模型采纳成功返回，不再映射 500 误报失败
            Ok(0)
        }
    })
    .await
    .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// 查询最近 N 天的 Qoder 上游用量统计（qoder_days 桶，与 Trae/WB/Custom 侧分账）
pub fn api_qoder_usage_stats(state: &AppState, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent_in(
        &state.data_dir,
        days,
        crate::api_server::usage::UsageBucket::Qoder,
    )
}

// ==================== 多 API Key 命令 ====================

/// 读取 API Key 列表与鉴权开关
pub fn api_keys_list(
    state: &AppState,
) -> crate::api_server::api_keys::ApiKeysFile {
    crate::api_server::api_keys::load(&state.data_dir)
}

/// 保存 API Key 列表（整表写盘；每次请求重读文件，改动立即生效）。
/// `auth_disabled` 不传时保留现值（避免整表保存覆盖鉴权开关）。
pub fn api_keys_save(
    state: &AppState,
    keys: Vec<crate::api_server::api_keys::ApiKeyEntry>,
    auth_disabled: Option<bool>,
) -> Result<(), String> {
    let prev = crate::api_server::api_keys::load(&state.data_dir);
    let file = crate::api_server::api_keys::ApiKeysFile {
        keys,
        auth_disabled: auth_disabled.unwrap_or(prev.auth_disabled),
    };
    // M3 修复（语义移植）：非环回监听 + （新状态显式关闭鉴权 或 无任何启用 Key）
    // → 拒绝保存。监听地址与运行时物理同源（对齐 auth.rs 双门禁）：单体版网关常驻
    // 运行，判定直接走 auth.rs 的运行期环回观测（AIWORK_LISTEN_ADDR，与
    // aiwork-server 绑定同源），不存在「配置值 vs 实际监听值」分离导致的 fail-open；
    // CLI 任务模式（网关未随本进程启动）按缺省环回放行，运行期仍有 bearer_auth
    // 双门禁兜底
    if !crate::api_server::auth::listen_is_loopback()
        && (file.auth_disabled || !file.has_enabled())
    {
        let addr = std::env::var("AIWORK_LISTEN_ADDR").unwrap_or_default();
        return Err(format!(
            "当前网关监听地址为 {addr}（非本机环回），局域网可访问；\
             出于安全要求必须启用至少一个 API Key 且不可关闭鉴权"
        ));
    }
    crate::api_server::api_keys::save(&state.data_dir, &file);
    Ok(())
}

/// Key 组/标签约束覆盖预览（D4/D5）：对传入条目按 org_index 实时展开计算
/// 各池覆盖账号数（前端「覆盖账号数 = 0」强提示用；只读不落盘）。
/// 返回 {key_id: {trae: n, buddy: n, qoder: n, total: n}}。
pub fn api_keys_org_preview(
    state: &AppState,
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
/// 网关常驻运行：用实时池健康派生 enabled 标记；共享状态未注册（CLI 任务模式）
/// 时返回 Err——目录展示依赖运行中的网关（§3.3 #5）
pub fn api_unified_models(
    state: &AppState,
    available_only: Option<bool>,
) -> Result<Vec<crate::api_server::unified_catalog::UnifiedModel>, String> {
    let Some(shared) = crate::api_server::runtime::gateway_shared() else {
        return Err("网关未启动".into());
    };
    let (wb_enabled, trae_enabled, qoder_enabled, trae_ok, buddy_ok, qoder_ok, qoder_flash_free, buddy_ai_enabled, buddy_ai_ok) = (
        shared
            .wb_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        shared
            .trae_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        shared
            .qoder_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        shared.pool.has_selectable(),
        shared.wb_pool.has_selectable(),
        shared.qoder_pool.has_selectable(),
        // Qwen3.8-Flash 免费活动覆盖（issue #111），与取号/排序同口径
        shared
            .qoder_flash_free_rate
            .load(std::sync::atomic::Ordering::Relaxed),
        shared
            .buddy_ai_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        shared.buddy_ai_pool.has_selectable(),
    );
    let data_dir = state.data_dir.clone();
    let mut list = crate::api_server::unified_catalog::unified_models_ex(
        &data_dir,
        wb_enabled,
        trae_enabled && trae_ok,
        buddy_ok,
        Some((qoder_enabled, qoder_ok)),
        Some((buddy_ai_enabled, buddy_ai_ok)),
        qoder_flash_free,
    );
    if available_only.unwrap_or(false) {
        // issue #145：可用口径须含 L1 静态禁用——disabled 源不承接请求，不能计为可用
        list.retain(|m| m.sources.iter().any(|s| s.enabled && !s.disabled));
    }
    Ok(list)
}

/// 读取全局模型白名单（canonical 归一列表；空 = 不限，issue #26）
pub fn model_whitelist_get(state: &AppState) -> Vec<String> {
    crate::api_server::unified_catalog::load_whitelist(&state.data_dir)
}

/// 保存全局模型白名单（canonical 归一 + 去空 + 去重；空列表 = 不限）；
/// 返回归一后的生效值（前端展示以返回值为准）
pub fn model_whitelist_set(state: &AppState, models: Vec<String>) -> Result<Vec<String>, String> {
    crate::api_server::unified_catalog::save_whitelist(&state.data_dir, &models)
}

/// 读取调度策略（规范化后视图：非法池名/空优先级已回退默认）
pub fn dispatch_policy_get(
    state: &AppState,
) -> crate::api_server::dispatch::DispatchPolicy {
    crate::api_server::dispatch::load_policy(&state.data_dir)
}

/// 保存调度策略；返回规范化后的生效值（前端展示以返回值为准）
pub fn dispatch_policy_set(
    state: &AppState,
    policy: crate::api_server::dispatch::DispatchPolicy,
) -> Result<crate::api_server::dispatch::DispatchPolicy, String> {
    crate::api_server::dispatch::save_policy(&state.data_dir, &policy)?;
    // 网关运行中热应用（issue #91）：配置锁外，与 pool_set 热应用同模式；
    // 负值由 set_reserve_credits 防御归零。未注册（CLI 模式）时静默跳过
    if let Some(shared) = crate::api_server::runtime::gateway_shared() {
        shared.pool.set_reserve_credits(policy.reserve_credits);
        shared.wb_pool.set_reserve_credits(policy.reserve_credits);
        shared.qoder_pool.set_reserve_credits(policy.reserve_credits);
    }
    Ok(crate::api_server::dispatch::load_policy(&state.data_dir))
}

/// 读取网关设置（port / default_model；缺失时从 app_settings 旧字段一次性迁移）
pub fn gateway_settings_get(
    state: &AppState,
) -> crate::api_server::gateway_settings::GatewaySettings {
    crate::api_server::gateway_settings::load(&state.data_dir)
}

/// 保存网关设置（端口改动在下次启动 API 服务后生效；返回规范化后的生效值）
pub fn gateway_settings_set(
    state: &AppState,
    settings: crate::api_server::gateway_settings::GatewaySettings,
) -> Result<crate::api_server::gateway_settings::GatewaySettings, String> {
    crate::api_server::gateway_settings::save(&state.data_dir, settings)?;
    Ok(crate::api_server::gateway_settings::load(&state.data_dir))
}

// ==================== 自定义模型资源池（custom_models.json） ====================

/// 自定义模型列表（OpenAI 兼容上游直通；命中即直达，§custom_models）
pub fn custom_models_list(
    state: &AppState,
) -> Vec<crate::api_server::custom_models::CustomModel> {
    crate::api_server::custom_models::load(&state.data_dir)
}

/// 保存自定义模型（upsert：id 为空新增并生成 cm- id，存在则整条覆盖；
/// 校验 name/base_url 必填 + 名称 canonical 唯一；返回保存后的完整列表）
pub fn custom_models_save(
    state: &AppState,
    model: crate::api_server::custom_models::CustomModel,
) -> Result<Vec<crate::api_server::custom_models::CustomModel>, String> {
    crate::api_server::custom_models::upsert(&state.data_dir, model)
}

/// 删除自定义模型（按 id）；返回是否确有删除
pub fn custom_models_remove(state: &AppState, id: String) -> Result<bool, String> {
    crate::api_server::custom_models::remove(&state.data_dir, &id)
}

/// 自定义模型连通性测试：向上游发一条最小 chat 请求（max_tokens=16），
/// 成功返回摘要、失败返回原因（保存/编辑前的前置校验入口）。
/// 阻塞 IO 放 spawn_blocking，外加 30s 总超时（覆盖连接 5s + 首字读 10s + 出字余量；
/// probe 走独立短超时 agent，不复用主路径 custom_agent 连接池——B3）。
pub async fn custom_model_test(
    model: crate::api_server::custom_models::CustomModel,
) -> Result<String, String> {
    // 与保存侧（custom_models::upsert）完全同口径的参数预检：给出明确错误而非
    // 透传上游 4xx。base_url 必须先过 norm_base 归一（scheme 小写 + 剥默认端口
    // 443/80 + 去尾斜杠），校验镜像 upsert——否则 HTTPS://x:443/ 能保存、此处
    // 探活却被大小写敏感 starts_with 拒绝，两侧口径分裂（port main 2fe9396）
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

pub fn custom_lb_status() -> Vec<CustomUpstreamCircuitStatus> {
    crate::api_server::custom_lb::circuit_snapshot()
        .into_iter()
        .map(|(id, fails, cooling_secs)| CustomUpstreamCircuitStatus { id, fails, cooling_secs })
        .collect()
}

/// Trae 模型元数据人工覆盖（L1 覆盖层，键 canonical_id；编辑后聚合视图即时生效）
pub fn trae_model_meta_set(
    state: &AppState,
    model: String,
    meta: crate::api_server::unified_catalog::TraeModelMeta,
) -> Result<(), String> {
    crate::api_server::unified_catalog::meta_set(&state.data_dir, &model, meta)
}

/// 读取 Trae 模型元数据人工覆盖（编辑弹框回显用；None = 无人工值，交由自动来源链）
pub fn trae_model_meta_get(
    state: &AppState,
    model: String,
) -> Option<crate::api_server::unified_catalog::TraeModelMeta> {
    crate::api_server::unified_catalog::load_meta(&state.data_dir)
        .remove(&crate::api_server::unified_catalog::canonical_id(&model))
}

/// 清除 Trae 模型元数据人工覆盖；返回是否存在过
pub fn trae_model_meta_clear(state: &AppState, model: String) -> Result<bool, String> {
    crate::api_server::unified_catalog::meta_clear(&state.data_dir, &model)
}

// ==================== 按模型长上下文（按池隔离：Trae 开关 / Qoder 数值） ====================

/// Trae 长上下文开关视图：已开启的模型（canonical_id）；未列出 = 跟随上游
#[derive(Debug, Clone, serde::Serialize)]
pub struct TraeLongContextView {
    pub enabled: Vec<String>,
}

pub fn trae_long_context_list(state: &AppState) -> TraeLongContextView {
    TraeLongContextView {
        enabled: crate::api_server::context_window::trae_list(&state.data_dir)
            .into_iter()
            .collect(),
    }
}

/// 开启/关闭单模型长上下文（仅 Trae 池）；支持表外模型拒绝开启。返回生效状态
pub fn trae_long_context_set(state: &AppState, model: String, enabled: bool) -> Result<bool, String> {
    crate::api_server::context_window::trae_set(&state.data_dir, &model, enabled)
}

/// Qoder 上下文长度单条（管理面列表行）
#[derive(Debug, Clone, serde::Serialize)]
pub struct QoderContextLengthItem {
    pub model: String,
    pub tokens: u64,
}

/// Qoder 上下文长度视图：已设置条目 + 合法区间（未列出的模型 = 跟随上游）
#[derive(Debug, Clone, serde::Serialize)]
pub struct QoderContextLengthView {
    pub items: Vec<QoderContextLengthItem>,
    pub min: u64,
    pub max: u64,
}

pub fn qoder_context_length_list(state: &AppState) -> QoderContextLengthView {
    use crate::api_server::context_window as cw;
    QoderContextLengthView {
        items: cw::qoder_list(&state.data_dir)
            .into_iter()
            .map(|(model, tokens)| QoderContextLengthItem { model, tokens })
            .collect(),
        min: cw::QODER_MIN_TOKENS,
        max: cw::QODER_MAX_TOKENS,
    }
}

/// 设置单模型 Qoder 上下文长度（仅 Qoder 池）；`tokens` 为 null/0 = 清除（跟随上游）。
/// 保存即对下一请求生效（config_cache 显式失效），返回生效值
pub fn qoder_context_length_set(
    state: &AppState,
    model: String,
    tokens: Option<u64>,
) -> Result<Option<u64>, String> {
    crate::api_server::context_window::qoder_set(&state.data_dir, &model, tokens)
}

// ==================== 单元测试：pool_set 合并语义（调度策略收口） ====================

#[cfg(test)]
mod pool_merge_tests {
    use super::merge_pool_set;
    // effective_wb_uids 已下沉 api_server::runtime（原本文件内定义，平移期不重复定义）
    use crate::api_server::runtime::effective_wb_uids;
    use crate::models::ApiPoolFile;

    /// 模拟已存在的 api_pool.json（各字段均非默认值，验证"保留"是否生效）
    fn existing() -> ApiPoolFile {
        ApiPoolFile {
            trae_enabled: true,
            enabled_uids: vec!["u1".into()],
            strategy: "weighted".into(),
            wb_strategy: "p2c".into(),
            qoder_strategy: "random".into(),
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
            qoder_enabled_uids: vec!["qd-1".into()],
            qoder_hedge_threshold_ms: 6000,
            qoder_sticky_enabled: true,
            qoder_flash_free_rate: true,
            qoder_account_concurrency_limit: 3,
            qoder_pool_sticky_ttl_secs: 900,
            qoder_sticky_ttl_secs: 3600,
            qoder_group_ids: Vec::new(),
            trae_tag_ids: Vec::new(),
            wb_tag_ids: Vec::new(),
            qoder_tag_ids: Vec::new(),
            // buddy-ai 独立池字段：merge_pool_set 测试关注 WB/Qoder 保留语义，
            // 新增池字段取 Default（false/0/空，与 Batch 4 pool_set 接线前口径一致）
            ..Default::default()
        }
    }

    /// 测试便捷封装：显式命中的字段生效，其余 None（保留原值）。
    /// 避免位置参数裸传 None 的脆弱写法（每加一个池字段就要全量改一遍）。
    #[derive(Default)]
    struct PoolSet {
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
        trae_sticky_ttl_secs: Option<u64>,
        trae_hedge_threshold_ms: Option<u64>,
        wb_sticky_ttl_secs: Option<u64>,
        wb_account_concurrency_limit: Option<u32>,
        wb_pool_sticky_ttl_secs: Option<u64>,
        qoder_account_concurrency_limit: Option<u32>,
        qoder_pool_sticky_ttl_secs: Option<u64>,
        qoder_sticky_ttl_secs: Option<u64>,
        wb_uids: Option<Vec<String>>,
        qoder_enabled: Option<bool>,
        qoder_hedge_threshold_ms: Option<u64>,
        qoder_sticky_enabled: Option<bool>,
        qoder_flash_free_rate: Option<bool>,
        qoder_uids: Option<Vec<String>>,
        qoder_group_ids: Option<Vec<String>>,
        qoder_strategy: Option<String>,
        trae_enabled: Option<bool>,
        buddy_ai_sticky_enabled: Option<bool>,
        buddy_ai_pool_sticky_ttl_secs: Option<u64>,
    }

    #[allow(clippy::too_many_arguments)]
    fn pool_set(
        existing: &ApiPoolFile,
        uids: Vec<String>,
        p: PoolSet,
    ) -> ApiPoolFile {
        merge_pool_set(
            existing,
            uids,
            p.strategy,
            p.wb_strategy,
            p.group_ids,
            p.wb_group_ids,
            p.wb_enabled,
            p.wb_default_thinking,
            p.wb_tool_exec,
            p.wb_bg_downgrade,
            p.wb_longctx_downgrade,
            p.wb_hedge_threshold_ms,
            p.trae_account_concurrency_limit,
            p.trae_pool_sticky_ttl_secs,
            p.wb_sticky_ttl_secs,
            p.trae_sticky_ttl_secs,
            p.trae_hedge_threshold_ms,
            p.wb_account_concurrency_limit,
            p.wb_pool_sticky_ttl_secs,
            p.qoder_account_concurrency_limit,
            p.qoder_pool_sticky_ttl_secs,
            p.qoder_sticky_ttl_secs,
            p.wb_uids,
            p.qoder_enabled,
            p.qoder_hedge_threshold_ms,
            p.qoder_sticky_enabled,
            p.qoder_flash_free_rate,
            p.qoder_uids,
            p.qoder_group_ids,
            p.qoder_strategy,
            p.trae_enabled,
            p.buddy_ai_sticky_enabled,
            p.buddy_ai_pool_sticky_ttl_secs,
        )
    }

    #[test]
    fn none_fields_preserve_existing() {
        // 只改成员（uids 必传覆盖），其余未传 → 全部保留原值（含 F-76/F-77 新参数）
        let m = pool_set(&existing(), vec!["u2".into()], PoolSet::default());
        assert_eq!(m.enabled_uids, vec!["u2".to_string()]);
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
        // per-pool 三参数 + Trae 对冲未传 → 保留原值
        assert_eq!(m.trae_account_concurrency_limit, 2);
        assert_eq!(m.trae_pool_sticky_ttl_secs, 600);
        assert_eq!(m.trae_sticky_ttl_secs, 2400);
        assert_eq!(m.trae_hedge_threshold_ms, 5000);
        assert_eq!(m.wb_account_concurrency_limit, 2);
        assert_eq!(m.wb_pool_sticky_ttl_secs, 620);
        assert_eq!(m.wb_sticky_ttl_secs, 3600);
        assert_eq!(m.qoder_account_concurrency_limit, 3);
        assert_eq!(m.qoder_pool_sticky_ttl_secs, 900);
        assert_eq!(m.qoder_sticky_ttl_secs, 3600);
        // qoder 参数未传 → 保留原值
        assert!(m.qoder_enabled);
        assert_eq!(m.qoder_enabled_uids, vec!["qd-1".to_string()]);
        assert_eq!(m.qoder_hedge_threshold_ms, 6000);
        assert!(m.qoder_sticky_enabled);
        assert_eq!(m.qoder_strategy, "random");
        assert!(m.trae_enabled);
    }

    #[test]
    fn trae_save_migrates_legacy_wb_uids() {
        // 旧版共享白名单混存 wb- 条目：Trae 页保存（未传 wb_uids）时 wb- 条目
        // 从 enabled_uids 剥离并迁移进 wb_enabled_uids，Buddy 池成员不丢失
        let mut legacy = existing();
        legacy.enabled_uids = vec!["1001".into(), "wb-abc".into(), "1002".into()];
        legacy.wb_enabled_uids = Vec::new();
        let m = pool_set(
            &legacy,
            vec!["1001".into(), "wb-abc".into(), "1002".into(), "wb-def".into()],
            PoolSet::default(),
        );
        // Trae 白名单剥离 wb- 条目
        assert_eq!(m.enabled_uids, vec!["1001".to_string(), "1002".to_string()]);
        // 仅 existing 混存的 wb- 条目迁移进 WB 白名单；传入名单中的未知 wb- 条目
        // （wb-def，非本次迁移来源）不并入，随剥离丢弃（白名单隔离语义）
        assert_eq!(m.wb_enabled_uids, vec!["wb-abc".to_string()]);
    }

    #[test]
    fn explicit_wb_uids_override() {
        // Buddy 页勾选保存：显式传 wb_uids → 覆盖（不再走旧数据迁移）
        let mut legacy = existing();
        legacy.enabled_uids = vec!["u1".into(), "wb-old".into()];
        let m = pool_set(
            &legacy,
            vec!["u1".into()],
            PoolSet { wb_uids: Some(vec!["wb-new".to_string()]), ..Default::default() },
        );
        assert_eq!(m.wb_enabled_uids, vec!["wb-new".to_string()]);
        assert_eq!(m.enabled_uids, vec!["u1".to_string()]);
    }

    #[test]
    fn effective_wb_uids_priority() {
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
    fn some_fields_override_and_empty_string_is_legal_value() {
        // 显式空串 = "跟随默认"合法值（区别于 None 未传）；显式空数组 = 清空分组
        let m = pool_set(
            &existing(),
            vec![],
            PoolSet {
                strategy: Some("p2c".into()),
                wb_strategy: Some(String::new()),
                group_ids: Some(vec![]),
                // wb_group_ids 显式覆盖
                wb_group_ids: Some(vec!["wg2".into()]),
                wb_enabled: Some(false),
                wb_default_thinking: Some(false),
                wb_tool_exec: Some(true),
                wb_bg_downgrade: Some(true),
                wb_longctx_downgrade: Some(false),
                wb_hedge_threshold_ms: Some(3000),
                trae_account_concurrency_limit: Some(0),
                trae_pool_sticky_ttl_secs: Some(60),
                wb_sticky_ttl_secs: Some(120),
                // per-pool 三参数显式传入 → 覆盖；Trae 对冲阈值未传 → 保留
                trae_sticky_ttl_secs: Some(90),
                wb_account_concurrency_limit: Some(4),
                wb_pool_sticky_ttl_secs: Some(45),
                qoder_account_concurrency_limit: Some(5),
                qoder_pool_sticky_ttl_secs: Some(70),
                qoder_sticky_ttl_secs: Some(100),
                ..Default::default()
            },
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
        // per-pool 三参数显式传入 → 覆盖
        assert_eq!(m.trae_sticky_ttl_secs, 90);
        assert_eq!(m.trae_hedge_threshold_ms, 5000);
        assert_eq!(m.wb_account_concurrency_limit, 4);
        assert_eq!(m.wb_pool_sticky_ttl_secs, 45);
        assert_eq!(m.qoder_account_concurrency_limit, 5);
        assert_eq!(m.qoder_pool_sticky_ttl_secs, 70);
        assert_eq!(m.qoder_sticky_ttl_secs, 100);
    }

    #[test]
    fn strategy_only_caller_does_not_touch_wb_and_flags() {
        // Trae 资源调度页收口后只保存成员/分组：不传 strategy/wb_strategy/开关组 → 均保留
        let m = pool_set(
            &existing(),
            vec!["u1".into(), "u3".into()],
            PoolSet { group_ids: Some(vec!["g2".into()]), ..Default::default() },
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
        let m = pool_set(&ApiPoolFile::default(), vec!["u1".into()], PoolSet::default());
        assert_eq!(m.strategy, "");
        assert_eq!(m.wb_strategy, "");
        assert!(m.group_ids.is_empty());
        assert!(!m.wb_enabled);
        assert!(!m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 8000);
        assert_eq!(m.trae_account_concurrency_limit, 1);
        assert_eq!(m.trae_pool_sticky_ttl_secs, 300);
        assert_eq!(m.trae_sticky_ttl_secs, 1800);
        assert_eq!(m.trae_hedge_threshold_ms, 8000);
        assert_eq!(m.wb_account_concurrency_limit, 1);
        assert_eq!(m.wb_pool_sticky_ttl_secs, 300);
        assert_eq!(m.wb_sticky_ttl_secs, 1800);
        assert_eq!(m.qoder_account_concurrency_limit, 1);
        assert_eq!(m.qoder_pool_sticky_ttl_secs, 300);
        assert_eq!(m.qoder_sticky_ttl_secs, 1800);
        assert!(m.trae_enabled);
    }
}
