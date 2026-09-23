//! 统一调度器（unified-api-gateway-design §4）
//!
//! 资源化调度：Trae / Buddy 降级为两份"服务资源池"，请求按模型 ID 匹配可用
//! 资源池集合（统一目录派生），按 `dispatch_policy.json` 配置的调度策略统一选池。
//!
//! 请求处理流程（§4.1）：
//! ① 鉴权（auth 中间件，不变）→ ② 模型名归一化（wb_model_route 四段管线，
//! 定位为"名字归一化层"全保留）→ ③ 查统一目录得 sources（剔除 enabled=false）
//! → ④ 选池（会话池粘性 TTL 内优先沿用 → 否则 dispatch_policy 优先级序，
//! per_model 覆盖）→ ⑤ 池内调度（沿用各池现有算法，由各执行路径自理）
//! → ⑥ 池间故障转移（仅双源模型）→ ⑦ 记账 is_wb 分桶（不变）。
//!
//! 命名约定（v1.2 §2）：对外池标识统一 `trae` / `buddy`；内部实现 wb_* 前缀
//! 保留不改名，本模块做标识映射（TargetPool::Buddy ↔ wb_* 数据）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::models_sync;
use super::unified_catalog::canonical_id;
use super::wb_model_route;
use super::ApiSharedState;

/// 池优先级默认序：显式化现状（Buddy 目录命中即 Buddy）
pub fn default_priority() -> Vec<String> {
    vec!["buddy".into(), "trae".into()]
}

/// 资源池标识（对外统一 `trae` / `buddy`；`custom` 仅供展示/日志，不参与策略配置）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPool {
    Trae,
    Buddy,
    /// 自定义模型（custom_models.json 命中即直达，用户显式配置优先；
    /// 不参与 dispatch_policy 优先级/回退——parse("custom") 返回 None）
    Custom,
}

impl TargetPool {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetPool::Trae => "trae",
            TargetPool::Buddy => "buddy",
            TargetPool::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "trae" => Some(TargetPool::Trae),
            "buddy" => Some(TargetPool::Buddy),
            _ => None,
        }
    }
}

// ==================== 调度策略配置（§4.2） ====================

/// 池间调度策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DispatchStrategy {
    /// 智能调度（默认）：按请求模型对可用源池排序——
    /// ① 池内最早积分到期优先（无到期数据视为最晚，后置）
    /// ② 该模型倍率小者优先（0 = 免费最优；未声明视为最大，后置）
    /// ③ 池内健康账号剩余积分总和多优先
    /// 全并列时回退固定优先级序（Buddy 优先现状）；per_model 显式覆盖不受重排影响
    #[default]
    Smart,
    /// 固定优先级（改造前现状）：严格按 priority/per_model 顺序取首个可用池
    Priority,
}

/// `data/dispatch_policy.json`：池间策略 + 池优先级 + 模型级覆盖 + 回退开关。
/// 优先级缺失回退默认 `["buddy","trae"]`（与现状路由行为一致，§9.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchPolicy {
    /// 池间调度策略（缺省 smart）
    #[serde(default)]
    pub strategy: DispatchStrategy,
    /// 池优先级顺序数组（取值 `trae` / `buddy`；priority 模式或 smart 并列时生效）
    #[serde(default = "default_priority")]
    pub priority: Vec<String>,
    /// 模型级覆盖（键为 canonical_id，值同为优先级数组），优先于 priority；
    /// 显式覆盖的模型不做智能重排（用户显式配置优先）
    #[serde(default)]
    pub per_model: HashMap<String, Vec<String>>,
    /// 双源模型首选池不可用时按序回退；关闭后仅用首选池
    #[serde(default = "default_true")]
    pub fallback: bool,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_true() -> bool {
    true
}

impl Default for DispatchPolicy {
    fn default() -> Self {
        DispatchPolicy {
            strategy: DispatchStrategy::Smart,
            priority: default_priority(),
            per_model: HashMap::new(),
            fallback: true,
            updated_at: 0,
        }
    }
}

/// 读取调度策略：缺失/损坏回退默认（不落盘——首次 set 时才写，保持数据干净）。
/// SQLite 化（P2）：kv 文档直读（单行 SELECT+解析 µs 级，替代原 mtime 解析缓存）。
pub fn load_policy(data_dir: &Path) -> DispatchPolicy {
    // 热路径缓存（批次 A）：resolve_target 每请求读取；写路径 save_policy 显式失效
    super::config_cache::get_or_load(data_dir, "dispatch_policy", || load_policy_uncached(data_dir))
}

fn load_policy_uncached(data_dir: &Path) -> DispatchPolicy {
    let mut p: DispatchPolicy = crate::store::db(data_dir).kv_get("dispatch_policy");
    // 防御：优先级数组非法值过滤 + 空数组回退默认
    let valid: Vec<String> = p
        .priority
        .iter()
        .filter_map(|s| TargetPool::parse(s).map(|t| t.as_str().to_string()))
        .collect();
    p.priority = if valid.is_empty() { default_priority() } else { valid };
    // per_model 键统一 canonical_id（与目录归并/元数据覆盖层同语义），
    // 值过滤非法池名
    p.per_model = p
        .per_model
        .drain()
        .map(|(k, mut v)| {
            v.retain(|s| TargetPool::parse(s).is_some());
            (canonical_id(&k), v)
        })
        .filter(|(_, v)| !v.is_empty())
        .collect();
    p
}

/// 保存调度策略（调用方负责校验后的最终形态落盘）
pub fn save_policy(data_dir: &Path, policy: &DispatchPolicy) -> Result<(), String> {
    let mut p = policy.clone();
    p.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::store::db(data_dir).kv_set("dispatch_policy", &p)?;
    // 写路径显式失效（批次 A）：策略改动即时生效
    super::config_cache::invalidate(data_dir, "dispatch_policy");
    Ok(())
}

// ==================== 选池决策（§4.3/§4.4） ====================

/// 调度错误（错误矩阵 §4.3，由端点按客户端协议映射响应格式）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DispatchError {
    /// 仅 Buddy 源模型且 wb_enabled=false（保持现有 wb_upstream_disabled 语义）
    WbDisabled,
    /// Buddy 模型级冷却中（单源显式 429；双源已在选池时回退）
    ModelCooling(i64),
    /// 首选（唯一可用）池耗尽：无健康账号 / 全冷却
    NoHealthy(TargetPool),
}

/// 选池结果
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 实际服务的资源池
    pub pool: TargetPool,
    /// Buddy 池：四段管线归一化后的最终模型；Trae 池：原模型名透传
    pub model: String,
    /// 路由级 effort 注入提示（仅 Buddy 池有效）
    pub effort_hint: Option<String>,
    /// 跨池回退来源：Some(首选池) 表示发生了回退（warn 日志已在 resolve_target
    /// 内部落 app_log；本字段供调度测试断言与 Phase 2 资源页展示预留）
    #[allow(dead_code)]
    pub fallback_from: Option<TargetPool>,
}

/// 回退原因（供 warn 日志）
#[derive(Debug, Clone, Copy, PartialEq)]
enum FallbackReason {
    NoHealthyAccount,
    ModelCooldown,
}

impl FallbackReason {
    fn as_str(self) -> &'static str {
        match self {
            FallbackReason::NoHealthyAccount => "no_healthy_account",
            FallbackReason::ModelCooldown => "model_cooldown",
        }
    }
}

/// 单请求资源源集合：该模型可用的资源池（③ 统一目录查表结果）
struct ModelSources {
    /// Buddy 源：四段管线归一化命中（Some(最终模型, effort 提示)）
    buddy: Option<(String, Option<String>)>,
    /// Trae 源：api_models 命中；或模型不属于任何目录（透传语义，单源 Trae）
    trae: bool,
}

impl ModelSources {
    fn is_dual(&self) -> bool {
        self.buddy.is_some() && self.trae
    }

    /// 源是否可用（buddy 需 wb_enabled；trae 恒可用；custom 走 resolve_target
    /// 顶部短路，不进入源集合判定——恒 false 仅为匹配穷尽）
    fn available(&self, pool: TargetPool, wb_enabled: bool) -> bool {
        match pool {
            TargetPool::Trae => self.trae,
            TargetPool::Buddy => self.buddy.is_some() && wb_enabled,
            TargetPool::Custom => false,
        }
    }
}

/// 跨池回退 warn 日志（§4.5）：`dispatch fallback: model=… preferred=… actual=… reason=…`
fn log_fallback(state: &ApiSharedState, model: &str, preferred: TargetPool, actual: TargetPool, reason: FallbackReason) {
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "dispatch fallback: model={} preferred={} actual={} reason={}",
            model,
            preferred.as_str(),
            actual.as_str(),
            reason.as_str(),
        ),
    );
}

/// 统一调度分流点（§4.1 ②~⑥）：替代原 `resolve_wb_target` 单向判定。
///
/// - `key_id`：命中 Key 条目 id（None = 匿名/未启用鉴权）；用于 Key 级资源池
///   绑定（issue #25）。选池优先级：**Key 绑定池 > 会话池粘性 > 全局策略**
///   （per_model > priority，smart 重排）。
/// - 返回 `Ok(Resolved)`：按 pool 分发到对应执行路径（Buddy → wb_route，Trae → solo）；
/// - 返回 `Err(DispatchError)`：错误矩阵处置，由端点按协议格式化响应。
///
/// 默认策略（无绑定 Key）下路由行为与改造前一致（§9.1 零行为差异）。
pub fn resolve_target(
    state: &Arc<ApiSharedState>,
    model: &str,
    body: &Value,
    key_id: Option<&str>,
) -> Result<Resolved, DispatchError> {
    // ⓪ 自定义模型直达（custom_models.json 命中 enabled 条目）：用户显式配置
    // 优先于内置目录；单源无跨池回退，模型名原样透传（custom_route 按条目
    // base_url/key 直连）。mtime 缓存读取，未配置时零开销跳过
    if let Some(cm) = super::custom_models::find_enabled(&state.data_dir, model) {
        let _ = cm; // 命中信息由执行路径重新按 data_dir 读取（条目可能热更新）
        return Ok(Resolved {
            pool: TargetPool::Custom,
            model: model.to_string(),
            effort_hint: None,
            fallback_from: None,
        });
    }

    // ② 模型名归一化（四段管线）+ Buddy 源判定：管线命中目录 → Buddy 源存在
    let cfg = wb_model_route::load_config(&state.data_dir);
    let catalog = super::wb_catalog::load(&state.data_dir);
    let r = wb_model_route::resolve(&cfg, &catalog, model);
    let buddy_hit = super::wb_catalog::find(&catalog, &r.model)
            .map(|_| {
                // T5.6③ 后台任务降级（显式开启才生效）：标题/摘要类短请求 → 目录最低倍率模型
                // issue #26 白名单感知：降级候选 ∩ 白名单为空则跳过降级（原模型已过端点准入）
                let whitelist = super::unified_catalog::load_whitelist(&state.data_dir);
                let in_wl = |m: &str| super::unified_catalog::whitelist_allows(&whitelist, m);
                let final_model = if state
                    .wb_bg_downgrade
                    .load(std::sync::atomic::Ordering::Relaxed)
                    && wb_model_route::is_background_task(body)
                {
                    wb_model_route::cheapest_catalog_model_filtered(&catalog, &in_wl)
                        .unwrap_or_else(|| r.model.clone())
                } else if state.wb_longctx_downgrade.load(std::sync::atomic::Ordering::Relaxed)
                    // F-76④ 长上下文降档：输入粗估超阈值（默认 100k token）→ flash 档模型
                    && wb_model_route::estimate_input_tokens(body)
                        >= wb_model_route::LONGCTX_TOKEN_THRESHOLD
                {
                    wb_model_route::flash_catalog_model_filtered(&catalog, &in_wl)
                        .unwrap_or_else(|| r.model.clone())
                } else {
                    r.model.clone()
                };
                (final_model, r.effort_hint.clone())
            });

    // ③ Trae 源判定：canonical_id 命中 Trae 模型列表；未命中任何目录时保持
    // 透传语义（单源 Trae，与现状一致）
    let canonical = canonical_id(model);
    let trae_hit = buddy_hit.is_none()
        || models_sync::load_models(&state.data_dir)
            .iter()
            .any(|m| canonical_id(&m.id) == canonical);
    let sources = ModelSources {
        buddy: buddy_hit,
        trae: trae_hit,
    };

    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);

    // 仅 Buddy 源模型 + wb_enabled=false → 显式报错（§4.3：保持现有
    // wb_upstream_disabled 语义；"关闭"与"不可用"是两个概念）
    if sources.buddy.is_some() && !sources.trae && !wb_enabled {
        return Err(DispatchError::WbDisabled);
    }

    // ③⁻ Key 级资源池绑定（issue #25）：解析命中 Key 的约束快照。
    // 绑定是「池偏好」而非模型过滤——绑定池无该模型源时仍路由到唯一可用池（D2）；
    // 未绑定（""/非法值/无 Key）跟随全局调度，行为与改造前一致
    let key_rk = key_id.and_then(|id| super::api_keys::constraints_for(&state.data_dir, id));
    let bind = key_rk.as_ref().and_then(|rk| rk.bind_pool());
    // 绑定 Key 的白名单健康预检（仅绑定池生效）：绑定池内若仅剩白名单外账号，
    // 选池阶段即判不健康走全局 fallback，而非取号阶段才失败。
    // 未绑定 Key 维持现状——预检不感知白名单，F-35 约束由执行路径取号时自理
    let key_allowed_for = |pool: TargetPool| -> Option<HashSet<String>> {
        let rk = key_rk.as_ref()?;
        if bind.is_none() || !rk.constrains_pool(pool.as_str()) || rk.allowed_accounts.is_empty() {
            return None;
        }
        Some(rk.allowed_accounts.iter().cloned().collect())
    };

    // 会话池粘性（§4.4 软粘，TTL 60s，内存态不落盘）：命中且池仍可用 → 直接沿用。
    // Key 绑定池优先于粘性：粘性池 ≠ 绑定池时不沿用（重新按候选序选池）
    let sticky_key = pool_session_key(body);
    if let Some(sk) = &sticky_key {
        if let Some(pool) = take_sticky(state, sk) {
            if bind.map_or(true, |b| pool.as_str() == b)
                && sources.available(pool, wb_enabled)
                && pool_healthy(state, pool, &sources, wb_enabled, key_allowed_for(pool).as_ref())
            {
                record_sticky(state, sk, pool);
                return Ok(Resolved {
                    pool,
                    model: final_model_for(pool, &sources, model),
                    effort_hint: effort_for(pool, &sources),
                    fallback_from: None,
                });
            }
        }
    }

    // ④ 按策略优先级序选池：有绑定时强制 [绑定池, 另一池] 候选序，跳过
    // per_model 与 smart 重排（用户 Key 级显式配置优先于全局策略）
    let policy = load_policy(&state.data_dir);
    let mut order: Vec<TargetPool> = if let Some(b) = bind {
        // b 经 parse_bind_pool 归一化，仅可能为 "trae"/"buddy"，无需再 parse
        if b == "trae" {
            vec![TargetPool::Trae, TargetPool::Buddy]
        } else {
            vec![TargetPool::Buddy, TargetPool::Trae]
        }
    } else {
        let mut o: Vec<TargetPool> = policy
            .per_model
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| policy.priority.clone())
            .iter()
            .filter_map(|s| TargetPool::parse(s))
            .collect();
        // per_model 可能只写了一个池：另一可用源追加尾部，保证双源回退有序
        for p in [TargetPool::Buddy, TargetPool::Trae] {
            if !o.contains(&p) {
                o.push(p);
            }
        }
        o
    };
    // ④+ 智能调度（strategy=smart）：双源可用且无 per_model 显式覆盖时，
    // 按请求模型对候选池重排（到期 → 倍率/免费 → 积分多；并列保持优先级序）。
    // 有绑定时跳过（绑定序即最终候选序）
    if bind.is_none()
        && policy.strategy == DispatchStrategy::Smart
        && policy.per_model.get(&canonical).is_none()
        && sources.is_dual()
        && wb_enabled
    {
        order = smart_pool_order(state, &order, &canonical, &catalog, &sources);
    }

    let preferred = order
        .iter()
        .copied()
        .find(|p| sources.available(*p, wb_enabled));

    let mut fallback_from: Option<(TargetPool, FallbackReason)> = None;
    for (i, pool) in order.iter().copied().enumerate() {
        if !sources.available(pool, wb_enabled) {
            continue;
        }
        match pool_health(state, pool, &sources, wb_enabled, key_allowed_for(pool).as_ref()) {
            Ok(()) => {
                // 首个可用池胜出；此前有可用池失败 → 跨池回退 warn 日志（§4.5）。
                // 源剔除（如 wb_enabled=false）不算回退，不记日志
                if let Some((pref, reason)) = fallback_from {
                    log_fallback(state, model, pref, pool, reason);
                }
                if let Some(sk) = &sticky_key {
                    record_sticky(state, sk, pool);
                }
                return Ok(Resolved {
                    pool,
                    model: final_model_for(pool, &sources, model),
                    effort_hint: effort_for(pool, &sources),
                    fallback_from: fallback_from.map(|(p, _)| p),
                });
            }
            Err(reason) => {
                // 首选池不可用：双源 + fallback 开 + 后续还有可用源 → 按序回退；
                // 单源（或 fallback 关）→ 错误矩阵显式报错（§4.3，不静默换池）
                let has_next = order[i + 1..]
                    .iter()
                    .any(|p| sources.available(*p, wb_enabled));
                let can_fallback = policy.fallback && sources.is_dual() && has_next;
                if !can_fallback {
                    return Err(match (pool, reason) {
                        (TargetPool::Buddy, FallbackReason::ModelCooldown) => {
                            let secs = super::wb_route::model_cooling_remaining(state, &final_buddy_model(&sources))
                                .unwrap_or(0);
                            DispatchError::ModelCooling(secs)
                        }
                        (p, _) => DispatchError::NoHealthy(p),
                    });
                }
                if fallback_from.is_none() {
                    fallback_from = Some((pool, reason));
                }
            }
        }
    }
    // 可用源全部耗尽（双源回退目标也不可用 / 源剔除后无源）
    Err(DispatchError::NoHealthy(
        preferred.unwrap_or(TargetPool::Trae),
    ))
}

/// 会话池粘性键（§4.4：复用现有粘性会话键，conversation 维度）。
/// 空指纹（无 messages / 无 conversation_id）不可粘 → None
fn pool_session_key(body: &Value) -> Option<String> {
    let key = super::wb_sticky::SessionKey::from_body(body).cache_key();
    // 空指纹的 cache_key 为 "fp:"（无内容后缀）→ 不可粘
    (!key.is_empty() && !key.ends_with(':')).then_some(key)
}

// ==================== 智能调度（§4.2 DispatchStrategy::Smart） ====================

/// 智能调度排序键：①最早积分到期（升序，无到期数据 = i64::MAX 后置）
/// ②模型倍率（升序，0 = 免费最优，未声明 = f64::MAX 后置）③健康积分总和多优先（负值升序）
type SmartKey = (i64, f64, f64);

/// 双源候选池按「到期 → 倍率/免费 → 积分多」复合键稳定排序；
/// 全并列保持传入序（= priority/per_model 固定优先级，Buddy 优先现状）。
fn smart_pool_order(
    state: &Arc<ApiSharedState>,
    order: &[TargetPool],
    canonical: &str,
    catalog: &[super::wb_catalog::WbModel],
    sources: &ModelSources,
) -> Vec<TargetPool> {
    let trae_stats = state.pool.stats();
    let wb_stats = state.wb_pool.stats();
    // 倍率数据源：Buddy = wb 目录原始值（0 = 免费）；Trae = 官网同步声明值（None = 未声明）。
    // 两份列表均为 read_json_cached 内存缓存，热路径零磁盘读
    let trae_rate = models_sync::load_models(&state.data_dir)
        .iter()
        .find(|m| canonical_id(&m.id) == canonical)
        .and_then(|m| m.rate);
    let buddy_rate = sources
        .buddy
        .as_ref()
        .and_then(|(m, _)| catalog.iter().find(|c| c.id == *m))
        .map(|c| c.rate);
    let key_of = |p: TargetPool| -> SmartKey {
        let (earliest, total) = if p == TargetPool::Trae { trae_stats } else { wb_stats };
        let rate = if p == TargetPool::Trae { trae_rate } else { buddy_rate };
        (
            earliest.unwrap_or(i64::MAX),
            rate.unwrap_or(f64::MAX),
            -total,
        )
    };
    let mut sorted = order.to_vec();
    sorted.sort_by(|a, b| {
        let (ka, kb) = (key_of(*a), key_of(*b));
        ka.0.cmp(&kb.0)
            .then_with(|| ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| ka.2.partial_cmp(&kb.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    sorted
}

/// 首选池健康检查：Buddy 池额外检查模型级冷却（优先级高于账号级）
/// `allowed`：Key 绑定的白名单（仅绑定池生效），None = 不感知白名单
fn pool_health(
    state: &Arc<ApiSharedState>,
    pool: TargetPool,
    sources: &ModelSources,
    _wb_enabled: bool,
    allowed: Option<&HashSet<String>>,
) -> Result<(), FallbackReason> {
    match pool {
        TargetPool::Buddy => {
            let model = final_buddy_model(sources);
            if super::wb_route::model_cooling_remaining(state, &model).is_some() {
                return Err(FallbackReason::ModelCooldown);
            }
            if !state.wb_pool.has_selectable_in(allowed) {
                return Err(FallbackReason::NoHealthyAccount);
            }
            Ok(())
        }
        TargetPool::Trae => {
            if !state.pool.has_selectable_in(allowed) {
                return Err(FallbackReason::NoHealthyAccount);
            }
            Ok(())
        }
        // 不可达：custom 在 resolve_target 顶部短路返回（匹配穷尽兜底）
        TargetPool::Custom => Ok(()),
    }
}

fn pool_healthy(
    state: &Arc<ApiSharedState>,
    pool: TargetPool,
    sources: &ModelSources,
    wb_enabled: bool,
    allowed: Option<&HashSet<String>>,
) -> bool {
    pool_health(state, pool, sources, wb_enabled, allowed).is_ok()
}

fn final_buddy_model(sources: &ModelSources) -> String {
    sources
        .buddy
        .as_ref()
        .map(|(m, _)| m.clone())
        .unwrap_or_default()
}

fn final_model_for(pool: TargetPool, sources: &ModelSources, request_model: &str) -> String {
    match pool {
        TargetPool::Buddy => final_buddy_model(sources),
        TargetPool::Trae | TargetPool::Custom => request_model.to_string(),
    }
}

fn effort_for(pool: TargetPool, sources: &ModelSources) -> Option<String> {
    match pool {
        TargetPool::Buddy => sources.buddy.as_ref().and_then(|(_, h)| h.clone()),
        TargetPool::Trae | TargetPool::Custom => None,
    }
}

// ==================== 会话池粘性（§4.4，内存态） ====================

// 池粘性 TTL 兜底值（秒）：软粘防抖动，重启即清不落盘；
// 实际 TTL 读 ApiSharedState.pool_sticky_ttl_secs（F-76② 可配置，默认 300s
// ——上游对同上下文有 prefill 缓存收益，粘性命中 = 缓存命中）
// （默认值由 default_pool_sticky_ttl_secs() 提供，无需独立常量）

/// 取出池粘性绑定（命中后移除过期项；命中项由调用方决定是否续期）
fn take_sticky(state: &Arc<ApiSharedState>, key: &str) -> Option<TargetPool> {
    let now = now_ts();
    let mut map = state.pool_sticky.lock().unwrap_or_else(|e| e.into_inner());
    // 懒清理过期项
    map.retain(|_, (_, exp)| *exp > now);
    map.get(key).map(|(p, _)| *p)
}

/// 记录/续期池粘性绑定（TTL 取可配置值，F-76②）
fn record_sticky(state: &Arc<ApiSharedState>, key: &str, pool: TargetPool) {
    let ttl = state
        .pool_sticky_ttl_secs
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1) as i64;
    let mut map = state.pool_sticky.lock().unwrap_or_else(|e| e.into_inner());
    map.insert(
        key.to_string(),
        (pool, now_ts() + ttl),
    );
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ==================== 测试（调度矩阵 ≥18 用例，§10 Phase 1 验收） ====================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    /// 测试数据目录脚手架：写 api_models / wb_catalog / dispatch_policy
    struct Fixture {
        dir: std::path::PathBuf,
        state: Arc<ApiSharedState>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// `trae_models`：Trae 侧 api_models 条目 id；`wb_models`：Buddy 侧目录 id
    /// （None 表示不写 wb_model_catalog.json → wb_catalog::load 会落盘内置 15 模型；
    /// 注意 wb_catalog 语义"空文件=缺失"，显式空目录不可表达——
    /// 需要"无 WB 干扰"时传一个不相关模型，如 Some(&["hy4"])）
    fn fixture(trae_models: &[&str], wb_models: Option<&[&str]>, policy: Option<&DispatchPolicy>) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "twa_dispatch_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        // SQLite 化（P2）：测试种子改走 kv（api_models / wb_model_catalog / dispatch_policy）
        let st = crate::store::db(&dir);
        let trae: Vec<Value> = trae_models
            .iter()
            .map(|id| json!({"id": id, "label": id}))
            .collect();
        st.kv_set("api_models", &trae).unwrap();
        if let Some(wb) = wb_models {
            let list: Vec<Value> = wb
                .iter()
                .map(|id| {
                    json!({"id": id, "display": id, "context_length": 128000,
                           "max_tokens": 64000, "supports_image": false,
                           "supported_efforts": ["low","medium","high"], "rate": 0.5})
                })
                .collect();
            st.kv_set("wb_model_catalog", &json!({"models": list})).unwrap();
        }
        if let Some(p) = policy {
            st.kv_set("dispatch_policy", p).unwrap();
        }
        let state = Arc::new(ApiSharedState {
            pool: super::super::pool::ApiPool::new(),
            wb_pool: super::super::pool::ApiPool::new(),
            wb_enabled: AtomicBool::new(true),
            wb_sanitize: AtomicBool::new(true),
            wb_default_thinking: AtomicBool::new(false),
            wb_tool_exec: AtomicBool::new(false),
            wb_bg_downgrade: AtomicBool::new(false),
            wb_longctx_downgrade: AtomicBool::new(false),
            wb_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            account_concurrency_limit: std::sync::atomic::AtomicU32::new(0),
            pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            wb_sticky: super::super::wb_sticky::StickyStore::default(),
            model_cooldowns: std::sync::Mutex::new(HashMap::new()),
            default_model: "deepseek-v4-flash".into(),
            data_dir: dir.clone(),
            total_requests: std::sync::atomic::AtomicU64::new(0),
            inflight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            active_uid: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            pool_sticky: std::sync::Mutex::new(HashMap::new()),
            logger: super::super::ApiLogger::new(dir.join("logs")),
            debug_enabled: AtomicBool::new(false),
            usage: std::sync::Mutex::new(super::super::usage::UsageFile::default()),
            usage_dirty: std::sync::Mutex::new(Vec::new()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
            trae_jwt_refresh: None,
        });
        Fixture { dir, state }
    }

    impl Fixture {
        /// 带会话指纹的解析（messages 非空 → 粘性生效）
        fn resolve(&self, model: &str) -> Result<Resolved, DispatchError> {
            resolve_target(
                &self.state,
                model,
                &json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
                None,
            )
        }

        /// 无会话键的解析（无 messages → 不粘性）
        fn resolve_no_session(&self, model: &str) -> Result<Resolved, DispatchError> {
            resolve_target(&self.state, model, &json!({"model": model}), None)
        }

        /// 带会话指纹 + Key 的解析（issue #25 资源池绑定用例）
        fn resolve_key(&self, model: &str, key_id: &str) -> Result<Resolved, DispatchError> {
            resolve_target(
                &self.state,
                model,
                &json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
                Some(key_id),
            )
        }

        /// 种子一个启用的 Key 条目（store 直写，追加/替换式合并；
        /// 须在首次 resolve_key 前调用——constraints_for 首读后缓存不可见后续直写）
        fn seed_key(&self, id: &str, bind_pool: &str, allowed: &[&str]) {
            self.seed_key_cfg(id, bind_pool, allowed, "", "")
        }

        /// 同上，可指定调度模式与专一账号（trae_pool_constraints 等用例用）
        fn seed_key_cfg(&self, id: &str, bind_pool: &str, allowed: &[&str], mode: &str, dedicated: &str) {
            let mut file = crate::store::docs::api_keys_load(&crate::store::db(&self.dir));
            file.keys.retain(|k| k.id != id);
            file.keys.push(super::super::api_keys::ApiKeyEntry {
                id: id.into(),
                name: id.into(),
                key: format!("ck-{id}"),
                enabled: true,
                daily_limit: 0,
                created_at: 0,
                used_date: String::new(),
                used_today: 0,
                allowed_accounts: allowed.iter().map(|s| s.to_string()).collect(),
                schedule_mode: mode.into(),
                dedicated_account: dedicated.into(),
                bind_pool: bind_pool.into(),
                daily_stats: vec![],
            });
            crate::store::docs::api_keys_save(&crate::store::db(&self.dir), &file).unwrap();
        }

        fn set_wb_enabled(&self, on: bool) {
            self.state
                .wb_enabled
                .store(on, std::sync::atomic::Ordering::Relaxed);
        }

        /// 往池里塞一个健康账号（credits=10）
        fn seed_healthy(&self, trae: bool) {
            let pool = if trae { &self.state.pool } else { &self.state.wb_pool };
            pool.sync_from_wb(
                &[super::super::pool::WbSyncAccount {
                    uid: if trae { "t1" } else { "b1" }.into(),
                    name: "acc".into(),
                    token: "tk".into(),
                    domain: if trae { String::new() } else { "d".into() },
                    enterprise_id: String::new(),
                    global_region: false,
                    credits: Some(10.0),
                    credits_expire_at: None,
                    needs_relogin: false,
                    group_id: String::new(),
                }],
                &[if trae { "t1" } else { "b1" }.to_string()],
            );
        }

        fn app_log_contains(&self, needle: &str) -> bool {
            let log_dir = self.dir.join("logs");
            let Ok(entries) = std::fs::read_dir(&log_dir) else {
                return false;
            };
            for e in entries.flatten() {
                if let Ok(text) = std::fs::read_to_string(e.path()) {
                    if text.contains(needle) {
                        return true;
                    }
                }
            }
            false
        }
    }

    fn policy_default() -> DispatchPolicy {
        DispatchPolicy::default()
    }

    // ---------- 维度一：优先级 ----------

    /// 默认策略 ["buddy","trae"] + 双源 + 双池健康 → Buddy（与现状一致 §9.1）
    #[test]
    fn t01_default_policy_dual_source_healthy_goes_buddy() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        let r = f.resolve("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert_eq!(r.model, "glm-5.3");
        assert!(r.fallback_from.is_none());
    }

    /// 策略 ["trae","buddy"] + 双源健康 → Trae（priority 模式验证）
    #[test]
    fn t02_trae_first_policy_goes_trae() {
        let mut p = policy_default();
        p.strategy = DispatchStrategy::Priority;
        p.priority = vec!["trae".into(), "buddy".into()];
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Trae);
    }

    /// per_model 覆盖优先于全局 priority
    #[test]
    fn t03_per_model_override_beats_priority() {
        let mut p = policy_default();
        p.priority = vec!["trae".into(), "buddy".into()];
        p.per_model.insert("glm-5.3".into(), vec!["buddy".into()]);
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Buddy);
    }

    /// per_model 键大小写归一（canonical_id）匹配
    #[test]
    fn t04_per_model_key_canonicalized() {
        let mut p = policy_default();
        p.priority = vec!["trae".into(), "buddy".into()];
        p.per_model.insert("GLM-5.3".into(), vec!["buddy".into()]);
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(f.resolve("GLM-5.3").unwrap().pool, TargetPool::Buddy);
    }

    // ---------- 维度二：模型源 ----------

    /// 仅 Buddy 源（hy4）→ Buddy
    #[test]
    fn t05_buddy_only_model_goes_buddy() {
        let f = fixture(&["glm-5.3"], Some(&["hy4", "glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(false);
        let r = f.resolve("hy4").unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
    }

    /// 仅 Trae 源（Doubao-Seed-Evolving 不在 WB 目录）→ Trae 透传
    #[test]
    fn t06_trae_only_model_goes_trae() {
        let f = fixture(&["Doubao-Seed-Evolving"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        let r = f.resolve("Doubao-Seed-Evolving").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "Doubao-Seed-Evolving");
        assert!(r.effort_hint.is_none());
    }

    /// 不属于任何目录的未知模型 → Trae 透传（现状语义：未命中 WB 目录走 SOLO）
    #[test]
    fn t07_unknown_model_passthrough_trae() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        let r = f.resolve("my-custom-model").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "my-custom-model");
    }

    // ---------- 维度三：池状态 ----------

    /// 双源 + Buddy 池耗尽 → 跨池回退 Trae + fallback_from 记录
    #[test]
    fn t08_dual_buddy_exhausted_falls_back_trae() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        // wb_pool 空 = 无健康账号
        let r = f.resolve("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.fallback_from, Some(TargetPool::Buddy));
        assert!(f.app_log_contains("dispatch fallback: model=glm-5.3 preferred=buddy actual=trae reason=no_healthy_account"));
    }

    /// 双源 + Trae 池耗尽（priority 模式 trae 优先）→ 回退 Buddy
    #[test]
    fn t09_dual_trae_exhausted_falls_back_buddy() {
        let mut p = policy_default();
        p.strategy = DispatchStrategy::Priority;
        p.priority = vec!["trae".into(), "buddy".into()];
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(false);
        let r = f.resolve("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert_eq!(r.fallback_from, Some(TargetPool::Trae));
    }

    /// 双源 + wb_enabled=false → Buddy 源剔除，走 Trae
    #[test]
    fn t10_dual_wb_disabled_goes_trae() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.set_wb_enabled(false);
        let r = f.resolve("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert!(r.fallback_from.is_none(), "源剔除不算回退");
    }

    /// 仅 Buddy 源 + wb_enabled=false → 显式报错（保持 wb_upstream_disabled 语义）
    #[test]
    fn t11_buddy_only_wb_disabled_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.set_wb_enabled(false);
        assert_eq!(f.resolve("hy4").unwrap_err(), DispatchError::WbDisabled);
    }

    /// 仅 Buddy 源 + Buddy 池耗尽 → 503 语义（NoHealthy），不跨池回退
    #[test]
    fn t12_buddy_only_exhausted_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true); // Trae 池健康但非双源，不得回退
        assert_eq!(
            f.resolve("hy4").unwrap_err(),
            DispatchError::NoHealthy(TargetPool::Buddy)
        );
    }

    /// 仅 Buddy 源 + 模型级冷却 → 429 语义（ModelCooling）
    #[test]
    fn t13_buddy_only_model_cooldown_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(false);
        super::super::wb_route::note_model_failure(&f.state, "hy4");
        assert!(matches!(
            f.resolve("hy4").unwrap_err(),
            DispatchError::ModelCooling(_)
        ));
    }

    /// 双源 + Buddy 模型级冷却 → 跨池回退 Trae（reason=model_cooldown）
    #[test]
    fn t14_dual_model_cooldown_falls_back_trae() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        super::super::wb_route::note_model_failure(&f.state, "glm-5.3");
        let r = f.resolve("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert!(f.app_log_contains("reason=model_cooldown"));
    }

    /// 仅 Trae 源 + Trae 池耗尽 → 503 语义
    #[test]
    fn t15_trae_only_exhausted_errors() {
        let f = fixture(&["Doubao-Seed-Evolving"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(false); // Buddy 池健康但非双源
        assert_eq!(
            f.resolve("Doubao-Seed-Evolving").unwrap_err(),
            DispatchError::NoHealthy(TargetPool::Trae)
        );
    }

    /// fallback=false + 双源 + 首选池耗尽 → 不回退，显式报错
    #[test]
    fn t16_fallback_disabled_no_cross_pool() {
        let mut p = policy_default();
        p.fallback = false;
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true); // Trae 池健康，但 fallback 关闭
        assert_eq!(
            f.resolve("glm-5.3").unwrap_err(),
            DispatchError::NoHealthy(TargetPool::Buddy)
        );
    }

    // ---------- 会话池粘性（§4.4） ----------

    /// 粘性命中：上一轮 Buddy 服务成功后，策略改为 trae 优先仍在 TTL 内沿用 Buddy
    #[test]
    fn t17_pool_sticky_overrides_policy() {
        let mut p = policy_default();
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        // 第一轮：默认策略 → Buddy，写入粘性
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Buddy);
        // 策略热改为 trae 优先
        p.priority = vec!["trae".into(), "buddy".into()];
        crate::store::db(&f.dir).kv_set("dispatch_policy", &p).unwrap();
        // 批次 A：绕过 save_policy 的 kv 直写需手动失效缓存（与生产写路径语义对齐）
        super::super::config_cache::invalidate(&f.dir, "dispatch_policy");
        // 同一会话（消息指纹相同）→ 沿用 Buddy
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Buddy);
    }

    /// 粘性池不可用（Buddy 耗尽）→ 实时回退，不卡死在粘性池
    #[test]
    fn t18_sticky_pool_unavailable_falls_back() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Buddy);
        // Buddy 池清空（模拟全冷却）：注入冷却使账号不可选
        f.state.wb_pool.note_error("b1", super::super::ErrKind::SessionDead);
        let r = f.resolve("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.fallback_from, Some(TargetPool::Buddy));
    }

    /// 不同会话不共享粘性：B 会话按策略走 Trae
    #[test]
    fn t19_sticky_is_per_session() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        // 会话 A（带 conversation_id）→ Buddy 并粘性
        let a = resolve_target(
            &f.state,
            "glm-5.3",
            &json!({"model": "glm-5.3", "conversation_id": "sessA", "messages": []}),
            None,
        )
        .unwrap();
        assert_eq!(a.pool, TargetPool::Buddy);
        // 会话 B（不同 conversation_id）→ 按策略仍 Buddy（默认序），但改动策略验证不粘连：
        // 这里验证粘性键按会话隔离：仅会话 A 续期，B 不受 A 影响（两者池一致故对比 fallback）
        // 直接改策略后 B 应走 trae（未被 A 的粘性污染）
        let mut p = policy_default();
        p.strategy = DispatchStrategy::Priority;
        p.priority = vec!["trae".into(), "buddy".into()];
        crate::store::db(&f.dir).kv_set("dispatch_policy", &p).unwrap();
        // 批次 A：kv 直写后失效策略缓存（见 t17 注释）
        super::super::config_cache::invalidate(&f.dir, "dispatch_policy");
        let b = resolve_target(
            &f.state,
            "glm-5.3",
            &json!({"model": "glm-5.3", "conversation_id": "sessB", "messages": []}),
            None,
        )
        .unwrap();
        assert_eq!(b.pool, TargetPool::Trae);
        // 会话 A 仍然粘 Buddy
        let a2 = resolve_target(
            &f.state,
            "glm-5.3",
            &json!({"model": "glm-5.3", "conversation_id": "sessA", "messages": []}),
            None,
        )
        .unwrap();
        assert_eq!(a2.pool, TargetPool::Buddy);
    }

    /// 空消息体（无会话键）→ 不粘性，每请求按策略判定
    #[test]
    fn t20_no_session_key_no_sticky() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        let r1 = resolve_target(&f.state, "glm-5.3", &json!({"model": "glm-5.3"}), None).unwrap();
        assert_eq!(r1.pool, TargetPool::Buddy);
        // 粘性表为空
        assert!(f.state.pool_sticky.lock().unwrap().is_empty());
    }

    // ---------- 策略加载与合法性 ----------

    /// 策略文件缺失 → 默认 ["buddy","trae"]；非法池名被过滤
    #[test]
    fn t21_policy_missing_and_invalid_values() {
        let dir = std::env::temp_dir().join(format!("twa_policy_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        // 缺失
        let p = load_policy(&dir);
        assert_eq!(p.priority, vec!["buddy".to_string(), "trae".to_string()]);
        assert!(p.fallback);
        // 非法值（kv 直写）
        crate::store::db(&dir)
            .kv_set("dispatch_policy", &json!({"priority": ["wb", "", "trae"], "fallback": false}))
            .unwrap();
        // 批次 A：kv 直写后失效缓存（生产路径经 save_policy 自动失效）
        super::super::config_cache::invalidate(&dir, "dispatch_policy");
        let p = load_policy(&dir);
        assert_eq!(p.priority, vec!["trae".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// save → load 往返 + updated_at 落盘
    #[test]
    fn t22_policy_roundtrip() {
        let dir = std::env::temp_dir().join(format!("twa_policy_rt_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let mut p = DispatchPolicy::default();
        p.per_model.insert("glm-5.3".into(), vec!["trae".into()]);
        save_policy(&dir, &p).unwrap();
        let loaded = load_policy(&dir);
        assert_eq!(loaded.per_model.get("glm-5.3").unwrap(), &vec!["trae".to_string()]);
        assert!(loaded.updated_at > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// canonical_id 三处键统一：目录归并 / per_model / meta 覆盖层同一语义
    #[test]
    fn t23_canonical_id_semantics() {
        assert_eq!(canonical_id("  GLM-5.3 "), "glm-5.3");
        assert_eq!(canonical_id("DeepSeek-V4-Flash"), "deepseek-v4-flash");
        assert_eq!(canonical_id("glm-5.3"), canonical_id(" GLM-5.3"));
    }

    // ---------- 行为兼容（对比用例，§10 关键断言） ----------

    /// 对比用例：默认策略下，WB 目录命中即 Buddy、未命中即 Trae——
    /// 与改造前 resolve_wb_target 判定完全一致（含 -thinking 后缀四段管线）
    #[test]
    fn t24_default_routing_parity_with_pre_refactor() {
        // hy4 仅 WB → Buddy（改前：resolve_wb_target Some → WB）
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(false);
        assert_eq!(f.resolve("hy4").unwrap().pool, TargetPool::Buddy);
        // Doubao-Seed-Evolving 仅 Trae → Trae（改前：None → SOLO）
        let f2 = fixture(&["Doubao-Seed-Evolving"], Some(&["hy4"]), Some(&policy_default()));
        f2.seed_healthy(true);
        assert_eq!(f2.resolve("Doubao-Seed-Evolving").unwrap().pool, TargetPool::Trae);
        // glm-5.3 双源 → Buddy（改前：WB 优先）
        let f3 = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f3.seed_healthy(true);
        f3.seed_healthy(false);
        assert_eq!(f3.resolve("glm-5.3").unwrap().pool, TargetPool::Buddy);
    }

    /// 双源模型 wb_enabled=false 时走 Trae，恢复后回到 Buddy（开关热切换；
    /// 无会话键请求不受粘性干扰）
    #[test]
    fn t25_wb_toggle_hot_switch() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        f.set_wb_enabled(false);
        assert_eq!(f.resolve_no_session("glm-5.3").unwrap().pool, TargetPool::Trae);
        f.set_wb_enabled(true);
        assert_eq!(f.resolve_no_session("glm-5.3").unwrap().pool, TargetPool::Buddy);
    }

    /// 回退后软粘生效：同会话沿用实际服务的 Trae 池（TTL 内防抖动），
    /// 新会话按策略回到恢复后的 Buddy
    #[test]
    fn t26_fallback_then_recovery() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        // Buddy 耗尽 → 回退 Trae
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Trae);
        // 同会话：软粘沿用 Trae（池粘性设计语义 §4.4）
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Trae);
        // Buddy 恢复（重新注入健康账号）后，新会话按策略走 Buddy
        f.seed_healthy(false);
        let fresh = resolve_target(
            &f.state,
            "glm-5.3",
            &json!({"model": "glm-5.3", "conversation_id": "sess-new", "messages": []}),
            None,
        )
        .unwrap();
        assert_eq!(fresh.pool, TargetPool::Buddy);
    }

    /// Trae 目标透传模型名且无 effort 提示；Buddy 目标携带归一化模型
    #[test]
    fn t27_resolved_payload_shape() {
        let f = fixture(&["glm-5.3"], Some(["glm-5.3"].as_slice()), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        let b = f.resolve("glm-5.3").unwrap();
        assert_eq!(b.pool, TargetPool::Buddy);
        assert_eq!(b.model, "glm-5.3");
        // 仅 Trae：模型名原样透传
        let f2 = fixture(&["Kimi-K3"], Some(&["hy4"]), Some(&policy_default()));
        f2.seed_healthy(true);
        let t = f2.resolve("Kimi-K3").unwrap();
        assert_eq!(t.pool, TargetPool::Trae);
        assert_eq!(t.model, "Kimi-K3");
        assert!(t.effort_hint.is_none());
    }

    // ---------- 智能调度（DispatchStrategy::Smart） ----------

    /// smart：Buddy 声明倍率 0.5 < Trae 未声明（后置）→ 即使 priority trae 优先也选 Buddy
    #[test]
    fn t28_smart_prefers_declared_rate() {
        let mut p = policy_default();
        p.priority = vec!["trae".into(), "buddy".into()];
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Buddy);
    }

    /// smart：Trae 声明更低倍率 0.3 < Buddy 0.5 → Trae 胜出（倍率序反向验证）
    #[test]
    fn t29_smart_lower_rate_wins() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        // 给 Trae 侧 api_models 条目补 rate=0.3（fixture 默认不带 rate）
        crate::store::db(&f.dir)
            .kv_set("api_models", &json!([{"id": "glm-5.3", "label": "glm-5.3", "rate": 0.3}]))
            .unwrap();
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Trae);
    }

    /// smart：per_model 显式覆盖不做智能重排（用户显式配置优先）
    #[test]
    fn t30_smart_skips_per_model_override() {
        let mut p = policy_default();
        p.per_model.insert("glm-5.3".into(), vec!["trae".into()]);
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Trae);
    }

    // ---------- Key 资源池绑定（issue #25） ----------

    /// 绑定池 > 全局策略：默认策略 Buddy 优先 + smart 重排，Key 绑定 trae → Trae
    #[test]
    fn t31_bind_pool_overrides_policy_and_smart() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        // 先无 Key 请求：按全局策略走 Buddy 并写粘性（避免污染后续断言）
        assert_eq!(f.resolve("glm-5.3").unwrap().pool, TargetPool::Buddy);
        // 绑定 trae 的 Key 同会话请求：粘性池 ≠ 绑定池不沿用，绑定序覆盖全局策略
        f.seed_key("k1", "trae", &[]);
        assert_eq!(f.resolve_key("glm-5.3", "k1").unwrap().pool, TargetPool::Trae);
    }

    /// 绑定池 > 会话池粘性（双向）：粘性池 ≠ 当前 Key 绑定池时不沿用，
    /// 每次都按当前 Key 的绑定序重新选池
    #[test]
    fn t32_bind_pool_beats_sticky() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        f.seed_key("k1", "trae", &[]);
        f.seed_key("k2", "buddy", &[]);
        // k1 → Trae，写粘性 Trae
        assert_eq!(f.resolve_key("glm-5.3", "k1").unwrap().pool, TargetPool::Trae);
        // 同会话 k2：粘性 Trae ≠ 绑定 Buddy → 不沿用 → Buddy
        assert_eq!(f.resolve_key("glm-5.3", "k2").unwrap().pool, TargetPool::Buddy);
        // 同会话 k1：粘性 Buddy ≠ 绑定 Trae → 不沿用 → Trae
        assert_eq!(f.resolve_key("glm-5.3", "k1").unwrap().pool, TargetPool::Trae);
    }

    /// 绑定池不健康：fallback 开（默认）→ 回退另一池 + warn 日志（D1 优先非硬限制）
    #[test]
    fn t33_bind_pool_unhealthy_falls_back() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(false); // 仅 Buddy 健康，Trae 池耗尽
        f.seed_key("k1", "trae", &[]);
        let r = f.resolve_key("glm-5.3", "k1").unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert_eq!(r.fallback_from, Some(TargetPool::Trae));
        assert!(f.app_log_contains("dispatch fallback: model=glm-5.3 preferred=trae actual=buddy reason=no_healthy_account"));

        // fallback 关：绑定池不健康 → 显式报错（不回退）
        let mut p = policy_default();
        p.fallback = false;
        let f2 = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f2.seed_healthy(false);
        f2.seed_key("k1", "trae", &[]);
        assert_eq!(
            f2.resolve_key("glm-5.3", "k1").unwrap_err(),
            DispatchError::NoHealthy(TargetPool::Trae)
        );
    }

    /// 绑定池白名单感知健康预检：绑定 buddy + 白名单 [b2]（池内仅 b1）→
    /// Buddy 对该 Key 不健康 → 回退 Trae；fallback 关 → NoHealthy(Buddy)
    #[test]
    fn t34_bind_pool_whitelist_aware_health() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false); // 池内仅 b1，不在白名单
        f.seed_key("k1", "buddy", &["b2"]);
        assert_eq!(f.resolve_key("glm-5.3", "k1").unwrap().pool, TargetPool::Trae);

        let mut p = policy_default();
        p.fallback = false;
        let f2 = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f2.seed_healthy(true);
        f2.seed_healthy(false);
        f2.seed_key("k1", "buddy", &["b2"]);
        assert_eq!(
            f2.resolve_key("glm-5.3", "k1").unwrap_err(),
            DispatchError::NoHealthy(TargetPool::Buddy)
        );
    }

    /// D2 绑定是池偏好非模型过滤：绑定 trae + 模型仅 Buddy 源（hy4）→ 仍走 Buddy
    #[test]
    fn t35_bind_pool_is_preference_not_filter() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(false);
        f.seed_key("k1", "trae", &[]);
        let r = f.resolve_key("hy4", "k1").unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert!(r.fallback_from.is_none(), "唯一可用池不算回退");
    }

    /// 非法 bind_pool 值视为未绑定：跟随全局策略（默认 Buddy 优先）
    #[test]
    fn t36_bind_pool_invalid_value_follows_global() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        f.seed_key("k1", "openai", &[]);
        assert_eq!(f.resolve_key("glm-5.3", "k1").unwrap().pool, TargetPool::Buddy);
    }

    /// routes::trae_pool_constraints 四种作用域（复用本模块 Fixture 构造状态）：
    /// 未绑定/绑定 buddy → (None, None)；绑定 trae → 提取白名单 + 专一；未知 Key → 匿名
    #[test]
    fn t37_trae_pool_constraints_scopes() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), None);
        // 先种满全部 Key 再断言：constraints_for 首读后走内存缓存，后续直写不可见
        f.seed_key("k0", "", &["b1"]);
        f.seed_key_cfg("k1", "trae", &["t1", "t2"], super::super::api_keys::MODE_DEDICATED, "t2");
        f.seed_key("k2", "buddy", &["b1"]);
        f.seed_key_cfg("k3", "trae", &["t1"], super::super::api_keys::MODE_EXPIRE_FIRST, "");

        // 未绑定 Key：白名单历来只作用 WB 池，绝不波及 Trae 池（F-35 兼容红线）
        let (a, d) = super::super::routes::trae_pool_constraints(&f.state, "k0");
        assert!(a.is_none() && d.is_none(), "未绑定 Key 的白名单不得作用于 Trae 池");

        // 绑定 trae + 专一：白名单与 dedicated 均提取（dedicated 非空优先于 allowed 首个）
        let (a, d) = super::super::routes::trae_pool_constraints(&f.state, "k1");
        let set = a.expect("绑定 trae 应提取白名单");
        assert!(set.contains("t1") && set.contains("t2"));
        assert_eq!(d.as_deref(), Some("t2"));

        // 绑定 buddy：白名单指 Buddy 账号体系，Trae 侧不应用
        let (a, d) = super::super::routes::trae_pool_constraints(&f.state, "k2");
        assert!(a.is_none() && d.is_none());

        // 绑定 trae + 临期优先：只提取白名单，无 dedicated
        let (a, d) = super::super::routes::trae_pool_constraints(&f.state, "k3");
        assert!(a.is_some() && d.is_none());

        // 未知 Key（已注销/匿名）：不限制
        let (a, d) = super::super::routes::trae_pool_constraints(&f.state, "nope");
        assert!(a.is_none() && d.is_none());
    }

    // ---------- issue #26 全局模型白名单 ----------

    /// 白名单感知后台任务降级（dispatch 白名单联动）：候选 ∩ 白名单非空 →
    /// 取白名单内最低倍率；交集为空 → 跳过降级保持原模型（原模型已过端点准入）
    #[test]
    fn t38_whitelist_aware_bg_downgrade() {
        // 内置目录（fixture 写入的 kv 因 builtin_rev=0 触发内置表重建）：
        // 全局最低为 hy4-preview(0.00 免费档)，原模型 glm-5.3(0.78)
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(false);
        f.state
            .wb_bg_downgrade
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 后台任务体：max_tokens ≤ 128 + 短文本（is_background_task = true）
        let bg_body = || {
            json!({"model": "glm-5.3", "max_tokens": 16,
                   "messages": [{"role": "user", "content": "生成标题"}]})
        };

        // 白名单含 glm-5.3-flash(0.06) + glm-5.3 → 降级取白名单内最低倍率（跳过被禁的 0.00 免费档）
        super::super::unified_catalog::save_whitelist(
            &f.dir,
            &["GLM-5.3-Flash".to_string(), "GLM-5.3".to_string()],
        )
        .unwrap();
        let r = resolve_target(&f.state, "glm-5.3", &bg_body(), None).unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert_eq!(r.model, "glm-5.3-flash", "降级候选 ∩ 白名单 → 白名单内最低倍率");

        // 白名单仅含原模型 → 候选集只剩 glm-5.3，不落入被禁模型
        super::super::unified_catalog::save_whitelist(&f.dir, &["GLM-5.3".to_string()]).unwrap();
        let r = resolve_target(&f.state, "glm-5.3", &bg_body(), None).unwrap();
        assert_eq!(r.model, "glm-5.3", "降级候选不在白名单 → 跳过降级保持原模型");

        // 白名单清空（不限）→ 正常降级到目录最低倍率 hy4-preview
        super::super::unified_catalog::save_whitelist(&f.dir, &[]).unwrap();
        let r = resolve_target(&f.state, "glm-5.3", &bg_body(), None).unwrap();
        assert_eq!(r.model, "hy4-preview", "空白名单不限 → 后台任务降级取目录最低倍率");
    }
}
