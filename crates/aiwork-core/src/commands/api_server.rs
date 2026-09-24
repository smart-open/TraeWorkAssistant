//! API 服务网关管理命令（自 src-tauri/src/commands/api_server.rs 平移，T5）。
//! server 单体中网关常驻运行（main.rs 启动即注册共享状态）：启停命令、托盘同步、
//! 运行时句柄（ApiServerRuntime）不平移；原句柄参数改经
//! `crate::api_server::runtime::gateway_shared()` 读全局注册的共享状态。

use crate::models::{ApiPoolFile, PoolStatus};
use crate::state::AppState;

use crate::api_server::models_sync;
use crate::api_server::ApiLogger;

// ==================== 池管理命令 ====================

pub fn pool_list(state: &AppState) -> ApiPoolFile {
    crate::store::db(&state.data_dir).kv_get("api_pool")
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
    account_concurrency_limit: Option<u32>,
    pool_sticky_ttl_secs: Option<u64>,
    wb_sticky_ttl_secs: Option<u64>,
    wb_uids: Option<Vec<String>>,
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
        wb_hedge_threshold_ms: wb_hedge_threshold_ms
            .unwrap_or(existing.wb_hedge_threshold_ms),
        account_concurrency_limit: account_concurrency_limit
            .unwrap_or(existing.account_concurrency_limit),
        pool_sticky_ttl_secs: pool_sticky_ttl_secs.unwrap_or(existing.pool_sticky_ttl_secs),
        wb_sticky_ttl_secs: wb_sticky_ttl_secs.unwrap_or(existing.wb_sticky_ttl_secs),
        wb_enabled_uids,
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
    account_concurrency_limit: Option<u32>,
    pool_sticky_ttl_secs: Option<u64>,
    wb_sticky_ttl_secs: Option<u64>,
    // Buddy 池入池白名单（wb- 前缀账号 id）；None = 保留原值（含旧数据迁移），
    // Some(list) = 覆盖（Buddy 页账号池勾选保存）
    wb_uids: Option<Vec<String>>,
) -> Result<(), String> {
    let existing: ApiPoolFile = crate::store::db(&state.data_dir).kv_get("api_pool");
    let pool_file = merge_pool_set(
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
        account_concurrency_limit,
        pool_sticky_ttl_secs,
        wb_sticky_ttl_secs,
        wb_uids,
    );
    crate::store::db(&state.data_dir).kv_set("api_pool", &pool_file)?;
    // 热应用：网关运行中即改内存池策略（Buddy 池空值沿用 Trae 池策略，与启动逻辑一致）
    if let Some(shared) = crate::api_server::runtime::gateway_shared() {
        shared
            .pool
            .set_strategy(crate::api_server::pool::PoolStrategy::parse(&pool_file.strategy));
        shared.wb_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy),
        );
        // F-76②/F-77 热参数即时生效（两池同构）
        shared
            .pool
            .set_concurrency_limit(pool_file.account_concurrency_limit);
        shared
            .wb_pool
            .set_concurrency_limit(pool_file.account_concurrency_limit);
        shared.wb_sticky.set_explicit_ttl(pool_file.wb_sticky_ttl_secs as i64);
        shared.wb_longctx_downgrade.store(
            pool_file.wb_longctx_downgrade,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.wb_hedge_threshold_ms.store(
            pool_file.wb_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.account_concurrency_limit.store(
            pool_file.account_concurrency_limit,
            std::sync::atomic::Ordering::Relaxed,
        );
        shared.pool_sticky_ttl_secs.store(
            pool_file.pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Buddy 资源开关热应用（此前仅启动时读取，改动需重启服务生效）
        shared
            .wb_enabled
            .store(pool_file.wb_enabled, std::sync::atomic::Ordering::Relaxed);
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
    crate::api_server::api_keys::save(&state.data_dir, &file);
    Ok(())
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
    let (wb_enabled, trae_ok, buddy_ok) = (
        shared
            .wb_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        shared.pool.has_selectable(),
        shared.wb_pool.has_selectable(),
    );
    let data_dir = state.data_dir.clone();
    let mut list = crate::api_server::unified_catalog::unified_models(
        &data_dir,
        wb_enabled,
        trae_ok,
        buddy_ok,
    );
    if available_only.unwrap_or(false) {
        list.retain(|m| m.sources.iter().any(|s| s.enabled));
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
/// VPN 虚拟网卡（Tailscale / ZeroTier / Clash 等）、蓝牙 PAN / 拨号虚拟适配器。
/// 中文接口名（「以太网」「WLAN」「本地连接」）与常规英文网卡名均不含关键词
fn is_virtual_iface(name: &str) -> bool {
    const BLACKLIST: &[&str] = &[
        "loopback", "docker", "br-", "veth", "virbr", "vmnet", "vethernet", "vmware",
        "virtualbox", "virtual", "hyper-v", "wsl", "bluetooth", "tailscale", "zerotier",
        "hamachi", "tap", "tun", "wintun", "clash", "sing-box", "singbox", "mihomo",
        "wireguard", "openvpn", "wan miniport", "ras async", "wi-fi direct",
    ];
    let n = name.to_lowercase();
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
/// 阻塞 IO 放 spawn_blocking，外加 30s 总超时（覆盖连接 10s + 首字 10s + 出字余量）。
pub async fn custom_model_test(
    model: crate::api_server::custom_models::CustomModel,
) -> Result<String, String> {
    // 与保存同口径的参数预检：给出明确错误而非透传上游 4xx
    let mut cm = model;
    cm.name = cm.name.trim().to_string();
    cm.base_url = cm.base_url.trim().trim_end_matches('/').to_string();
    if cm.name.is_empty() {
        return Err("请先填写模型名称".into());
    }
    if !cm.base_url.starts_with("http://") && !cm.base_url.starts_with("https://") {
        return Err("API 地址必须以 http:// 或 https:// 开头".into());
    }
    let handle = tokio::task::spawn_blocking(move || crate::api_server::custom_route::probe(&cm));
    match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(format!("测试任务失败: {e}")),
        Err(_) => Err("测试超时（30 秒）".into()),
    }
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
            account_concurrency_limit: 2,
            pool_sticky_ttl_secs: 600,
            wb_sticky_ttl_secs: 3600,
            wb_enabled_uids: Vec::new(),
            wb_group_ids: vec!["wg1".into()],
        }
    }

    #[test]
    fn none_fields_preserve_existing() {
        // 只改成员（uids 必传覆盖），其余未传 → 全部保留原值（含 F-76/F-77 新参数）
        let m = merge_pool_set(
            &existing(),
            vec!["u2".into()],
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
            None,
        );
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
        assert_eq!(m.account_concurrency_limit, 2);
        assert_eq!(m.pool_sticky_ttl_secs, 600);
        assert_eq!(m.wb_sticky_ttl_secs, 3600);
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
            None,
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
            Some(vec!["wb-new".into()]),
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
        assert_eq!(m.account_concurrency_limit, 0);
        assert_eq!(m.pool_sticky_ttl_secs, 60);
        assert_eq!(m.wb_sticky_ttl_secs, 120);
    }

    #[test]
    fn strategy_only_caller_does_not_touch_wb_and_flags() {
        // Trae 资源调度页收口后只保存成员/分组：不传 strategy/wb_strategy/开关组 → 均保留
        let m = merge_pool_set(
            &existing(),
            vec!["u1".into(), "u3".into()],
            None,
            None,
            Some(vec!["g2".into()]),
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
            None,
        );
        assert_eq!(m.strategy, "");
        assert_eq!(m.wb_strategy, "");
        assert!(m.group_ids.is_empty());
        assert!(!m.wb_enabled);
        assert!(!m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 8000);
        assert_eq!(m.account_concurrency_limit, 1);
        assert_eq!(m.pool_sticky_ttl_secs, 300);
        assert_eq!(m.wb_sticky_ttl_secs, 1800);
    }
}

// ==================== 单元测试：局域网网卡地址过滤（issue #34） ====================

#[cfg(test)]
mod lan_iface_tests {
    use super::{is_virtual_iface, lan_iface_ips_impl};

    /// 黑名单命中：Docker / 虚拟化平台 / 代理 TUN / 回环 / 蓝牙 PAN / 拨号
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
