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
use crate::tasks::qoder_upstream;

/// 池优先级默认序：显式化现状（Buddy 目录命中即 Buddy）。
/// Qoder 追加尾部：双源/三源模型的默认命中顺序保持改造前行为
///（Buddy → Trae），Qoder 池仅在前两者不可用或单源时接管（§9.1 零行为差异）
pub fn default_priority() -> Vec<String> {
    vec!["buddy".into(), "trae".into(), "qoder".into()]
}
/// A2 审查修复：固定候选序共享常量（与缺省 priority 序对齐的尾部补全序）。
/// dispatch 尾部补全与 unified_catalog 显示层「无显式序命中」回退共用——
/// 此前显示层按 sources 插入序（trae→buddy→…）回退，与调度侧固定序漂移。
pub const POOL_FIXED_ORDER: [TargetPool; 5] = [
    TargetPool::Buddy,
    TargetPool::Trae,
    TargetPool::Qoder,
    TargetPool::BuddyAi,
    TargetPool::Custom,
];

/// 资源池标识（对外统一 `trae` / `buddy` / `qoder` / `buddy-ai` / `custom`）。
/// v3 拍板：custom 为第五资源池参与统一调度规则（priority / per_model / Smart /
/// Key 绑定 / fallback 全链路生效），默认候选序尾部兜底——不再「命中即直达」
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPool {
    Trae,
    Buddy,
    /// Buddy 国际版（buddy-ai 独立池，阶段 2）：与 `buddy` 档平级的独立档位，
    /// 模型源判定共用 wb 目录（R-9 两域同一账号体系，同名模型跨区不混轮靠独立池保证）；
    /// 默认不启用（buddy_ai_enabled，用户在 API 服务页显式开启国际档——避免
    /// 用户仍在国内账号消耗时模型被静默路由到国际池的歧义）
    BuddyAi,
    /// 自定义模型上游组（P1 同名多条 = 上游组；custom_lb 组内负载均衡 + 熔断；
    /// v3：普通池参与统一调度，`custom/模型名` 前缀锁池、per_model/Key 绑定可写 custom）
    Custom,
    /// Qoder 上游（p3-3，gateway.qoder.com.cn 推理网关；Qoder 目录命中即源）
    Qoder,
}

impl TargetPool {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetPool::Trae => "trae",
            TargetPool::Buddy => "buddy",
            TargetPool::BuddyAi => "buddy-ai",
            TargetPool::Custom => "custom",
            TargetPool::Qoder => "qoder",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "trae" => Some(TargetPool::Trae),
            "buddy" => Some(TargetPool::Buddy),
            "buddy-ai" => Some(TargetPool::BuddyAi),
            "qoder" => Some(TargetPool::Qoder),
            // v3 拍板：custom 转正——前缀锁池（custom/xxx）与 per_model/priority
            // 写入 "custom" 随 parse 打通自然生效
            "custom" => Some(TargetPool::Custom),
            _ => None,
        }
    }
}

/// 解析池前缀模型 id（issue #90 修复③a）：`trae/xxx` / `buddy/xxx` / `qoder/xxx`
/// / `custom/xxx` → (锁定池, 裸模型名)。v3：custom 前缀锁池直达（单池候选、
/// 不可用显式报错不跨池回退，与其他池前缀同语义）。
/// 前缀段/裸名均 trim + 小写比对；裸名为空视为无效前缀，原样透传（不误吞
/// 斜杠模型名）。白名单准入（routes::whitelist_check）复用本函数剥离前缀。
pub(crate) fn split_pool_prefix(model: &str) -> Option<(TargetPool, &str)> {
    let (prefix, bare) = model.split_once('/')?;
    if bare.trim().is_empty() {
        return None;
    }
    Some((TargetPool::parse(prefix)?, bare.trim()))
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
    /// 池优先级顺序数组（取值 `trae` / `buddy` / `qoder` / `buddy-ai` / `custom`；
    /// priority 模式或 smart 并列时生效，缺省序尾部补全循环追加 buddy-ai/custom）
    #[serde(default = "default_priority")]
    pub priority: Vec<String>,
    /// 模型级覆盖（键为 canonical_id，值同为优先级数组），优先于 priority；
    /// 显式覆盖的模型不做智能重排（用户显式配置优先）
    #[serde(default)]
    pub per_model: HashMap<String, Vec<String>>,
    /// 双源模型首选池不可用时按序回退；关闭后仅用首选池
    #[serde(default = "default_true")]
    pub fallback: bool,
    /// 账号保留剩余积分（issue #91）：剩余 ≤ 该值的账号暂停调度（0 = 关闭），
    /// 默认 50——防止账号积分被调度耗尽后连免费模型也不可用
    #[serde(default = "default_reserve_credits")]
    pub reserve_credits: f64,
    /// /v1/models 为多源同名模型附带池前缀别名条目（issue #90 修复③a）：
    /// `trae/xxx` / `buddy/xxx` / `buddy-ai/xxx` / `qoder/xxx`——各池同名模型
    /// 倍率与效果不同，开启后客户端可按前缀点名指定池服务（resolve_target 按
    /// 前缀锁定单池）。默认关（目录保持原形态，零行为差异）
    #[serde(default)]
    pub model_source_labels: bool,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_true() -> bool {
    true
}

fn default_reserve_credits() -> f64 {
    50.0
}

impl Default for DispatchPolicy {
    fn default() -> Self {
        DispatchPolicy {
            strategy: DispatchStrategy::Smart,
            priority: default_priority(),
            per_model: HashMap::new(),
            fallback: true,
            reserve_credits: default_reserve_credits(),
            model_source_labels: false,
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
    // 防御：保留积分阈值负值归零（0 = 关闭过滤）
    p.reserve_credits = p.reserve_credits.max(0.0);
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
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchError {
    /// 仅 Buddy 源模型且 wb_enabled=false（保持现有 wb_upstream_disabled 语义）
    WbDisabled,
    /// 仅 Qoder 源模型且 qoder_enabled=false（同 WbDisabled 语义镜像）
    QoderDisabled,
    /// 仅 Trae 源模型且 trae_enabled=false（同族镜像；Trae 开关默认开，
    /// 关闭属用户显式动作，与"池耗尽 503"区分开）
    TraeDisabled,
    /// 模型级冷却中（单源显式 429；双源已在选池时回退）。
    /// 审查修复（模型冷却归因）：附带发起请求的目标池（buddy/buddy-ai/qoder
    /// 各持独立冷却表），供 routes 层请求日志按真实池归因——此前类型不带池、
    /// 响应层硬编码 "buddy"，buddy-ai/qoder 的模型级 429 在用量看板与
    /// app_log 中归因失真
    ModelCooling(i64, TargetPool),
    /// 首选（唯一可用）池耗尽：无健康账号 / 全冷却
    NoHealthy(TargetPool),
    /// 池耗尽提前终止 + 主选池循环耗尽（issue #159 归因增强，合并审查 Medium
    /// 修复）：附带本轮实际候选序——503 归因注记「前置池因模型冷却被跳过」
    /// 必须按实际序判断，按 POOL_FIXED_ORDER 硬编码在前缀锁定/Key 绑定/
    /// per_model 场景会虚假归因（不在候选序里的池被注记 skipped）或漏归因
    /// （真实前置池不在固定序前缀里）。两处构造点同在 resolve_target ④ 段
    /// order 变量作用域（提前终止在循环迭代借用中需 clone，循环尾 move），
    /// 零重算成本
    NoHealthyWithOrder(TargetPool, Vec<TargetPool>),
}

/// 选池结果
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 实际服务的资源池
    pub pool: TargetPool,
    /// Buddy 池：四段管线归一化后的最终模型；Trae 池：后缀剥离后的基名
    /// （issue #31 T3.1，无后缀/透传语义 = 原名）；Custom：原名
    pub model: String,
    /// 路由级 effort 提示（统一档位）：Buddy 池直接注入 reasoning_effort；
    /// Trae 池由 routes 层转 wire 档位下发（efforts::trae_request_wire）
    pub effort_hint: Option<String>,
    /// Max Mode 入口标志（issue #31 T4.2）：请求模型带 `-max` 入口后缀且基名可由
    /// Trae 服务时置位（整名命中列表的真模型不置位，见 trae_final 计算段守卫）；
    /// 实际注入由 routes 层结合 efforts::TRAE_MAX_MODE_REF 门控，仅 Trae 池为 true
    pub max_mode_hint: bool,
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
    /// Buddy 源：四段管线归一化命中（Some(最终模型, effort 提示)）；含 Intl 静态
    /// 目录折入（buddy_intl_only 区分来源域）
    buddy: Option<(String, Option<String>)>,
    /// BuddyAi 域最终模型（issue #19）：CN 收录模型的 Intl 目录降级候选——目标池
    /// 为 BuddyAi 时最终模型取本值（候选按 Intl 目录取，上游必存在该 id）；None =
    /// Intl 专属折入（无降级语义），回落 buddy.0（两域同值）
    buddy_intl_final: Option<String>,
    /// Intl 专属标记（审查修复）：buddy 源来自 Intl 静态目录而非 CN 目录——
    /// Buddy(CN) 池不服务该模型（available 门控），仅 buddy-ai 池可取号
    buddy_intl_only: bool,
    /// CN+Intl 双收录标记（审查修复 第2项）：模型同时被 CN 目录与 Intl 目录
    /// 收录——国际档开启时 buddy-ai 池可服务；CN 专属模型（false）不得静默
    /// 路由国际池（跨区计费/合规歧义，available 门控）
    buddy_in_both: bool,
    /// Trae 源：api_models 命中；或模型不属于任何目录（透传语义，单源 Trae）
    trae: bool,
    /// Trae 侧最终模型 + effort 提示 + Max Mode 入口标志（issue #31 T3.1/T4.2：
    /// Trae-only 模型带路由后缀时剥离为基名，effort 提示随池携带、由 routes 层转
    /// wire 下发；带 `-max` 入口后缀时剥离为基名并置 max 标志、由 routes 层结合
    /// efforts::TRAE_MAX_MODE_REF 门控注入 is_max_mode:true；透传语义为原名）
    trae_final: (String, Option<String>, bool),
    /// Qoder 源（p3-3）：Qoder 目录命中（Some(目录归一化 id)）；
    /// upstreamKey/efforts 等请求素材由执行路径经 qoder_upstream::resolve 再查
    qoder: Option<String>,
    /// custom 上游组（P1::find_enabled_all 结果，空 = 无 custom 源；v3 第四源：
    /// 与 buddy/trae/qoder 并列参与源集合判定与 fallback 管线，组内选路/熔断
    /// 由 custom_lb 在执行路径自理，pool_health 仅判「任一上游未熔断」）
    custom: Vec<super::custom_models::CustomModel>,
}

impl ModelSources {
    /// 是否多源（≥2 个源存在，忽略开关状态）：跨池回退的前提。
    /// 可用源计数（感知开关状态）：智能调度重排与回退闸门用
    ///（BM-3：原 is_dual 家族计数已由本方法取代——Buddy/BuddyAi 独立失败域各自计数）
    fn available_count(
        &self,
        trae_enabled: bool,
        wb_enabled: bool,
        qoder_enabled: bool,
        buddy_ai_enabled: bool,
    ) -> usize {
        let mut n = 0;
        if self.trae && trae_enabled {
            n += 1;
        }
        if self.buddy.is_some() && wb_enabled && !self.buddy_intl_only {
            n += 1;
        }
        // buddy-ai 与 buddy 平级计数（同一 wb 目录源、独立池/开关）；仅 Intl
        // 专属或 CN+Intl 双收录模型计入（审查修复 第2项：CN 专属模型不得静默
        // 路由国际池，智能调度/换号回退闸门与 available 同口径）
        if self.buddy.is_some()
            && buddy_ai_enabled
            && (self.buddy_intl_only || self.buddy_in_both)
        {
            n += 1;
        }
        if self.qoder.is_some() && qoder_enabled {
            n += 1;
        }
        // custom 上游组（v3 第四源）：组非空即独立失败域计数（组内熔断细节由
        // pool_health 判定，此处只算源存在性）
        if !self.custom.is_empty() {
            n += 1;
        }
        n
    }

    /// 源是否可用（trae/buddy/qoder/custom 各感知自己的池状态；custom = 组非空）
    fn available(
        &self,
        pool: TargetPool,
        trae_enabled: bool,
        wb_enabled: bool,
        qoder_enabled: bool,
        buddy_ai_enabled: bool,
    ) -> bool {
        match pool {
            TargetPool::Trae => self.trae && trae_enabled,
            // Intl 专属模型源不得路由 Buddy(CN) 池（审查修复：CN 上游无该模型 id）
            TargetPool::Buddy => self.buddy.is_some() && wb_enabled && !self.buddy_intl_only,
            // 审查修复（第2项）：仅 Intl 专属或 CN+Intl 双收录模型可走国际池——
            // CN 专属模型即使国际档开启也不被智能调度/换号回退静默切国际上游
            TargetPool::BuddyAi => {
                self.buddy.is_some()
                    && buddy_ai_enabled
                    && (self.buddy_intl_only || self.buddy_in_both)
            }
            TargetPool::Qoder => self.qoder.is_some() && qoder_enabled,
            // v3：custom 为真源判定（原恒 false 仅为匹配穷尽）——组非空即可候选，
            // 全熔断由 pool_health 拦截走 fallback
            TargetPool::Custom => !self.custom.is_empty(),
        }
    }
}

/// 跨池回退 warn 日志（§4.5）：
/// `dispatch fallback: model=… pool=… preferred=… reason=…`（pool 即 actual）
/// 审查修复（中等项）：60s 节流——同因回退逐请求落盘会刷爆 app 日志（上游
/// 故障期间每个请求都触发）；二审细化：按 model|reason 分键限流，
/// 不同模型/不同因的回退互不抑制，避免观测盲区
fn log_fallback(state: &ApiSharedState, model: &str, preferred: TargetPool, actual: TargetPool, reason: FallbackReason) {
    const FALLBACK_LOG_THROTTLE_MS: i64 = 60_000;
    /// 键上限：模型组合再多也有限（按 model|reason 组合增长），
    /// 超限整表清理，防极端场景下无界增长
    const FALLBACK_LOG_MAX_KEYS: usize = 256;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let key = format!("{model}|{}", reason.as_str());
    let mut last_map = state
        .fallback_log_last_ms
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&last) = last_map.get(&key) {
        if now_ms - last < FALLBACK_LOG_THROTTLE_MS {
            return;
        }
    }
    if last_map.len() >= FALLBACK_LOG_MAX_KEYS {
        last_map.clear();
    }
    last_map.insert(key, now_ms);
    drop(last_map);
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "dispatch fallback: model={} pool={} preferred={} reason={}",
            model,
            actual.as_str(),
            preferred.as_str(),
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
    // 端点层 model 仅过滤非空未 trim：入口统一去空白（strip_route_suffix 后缀
    // 匹配 / canonical 比对 / Custom 条目查找均依赖干净形态，错误信息同步受益）
    let model = model.trim();
    // issue #90 修复③a：池前缀模型 id（`trae/xxx` / `buddy/xxx` / `qoder/xxx`）
    // 显式锁定服务池——单池候选、跳过粘性/per_model/smart 重排，不可用即显式
    // 报错不跨池回退（用户点名的就是这一池；前缀语义 > Key 绑定与全局策略）
    let (forced_pool, model) = match split_pool_prefix(model) {
        Some((pool, bare)) => (Some(pool), bare),
        None => (None, model),
    };
    // v3（P4）：⓪ custom 短路已移除——custom 为独立源，与前缀锁定解耦：
    // 源集合照常收集（下方 sources.custom 填充），前缀锁定只影响候选序
    //（custom/xxx = 单池候选直达；同名多源默认按统一调度规则、custom 尾部兜底）。
    // 纯 custom 模型（无同名 trae/buddy/qoder 源）为唯一源，天然直达不受序影响

    // ② 模型名归一化（四段管线）+ Buddy 源判定：管线命中目录 → Buddy 源存在
    let cfg = wb_model_route::load_config(&state.data_dir);
    let catalog = super::wb_catalog::load(&state.data_dir);
    let r = wb_model_route::resolve(&cfg, &catalog, model);
    // Intl 目录动态化（2026-10-15）：动态抓取结果优先（kv buddy_ai_model_catalog），
    // 缺失回落内置静态表——/v3/config 实抓成功后倍率/上下文即随动态目录生效
    let intl_catalog = super::wb_catalog::buddy_ai_load(&state.data_dir);
    // buddy-ai 国际档开关（阶段 2）：显式开启制，默认关（前移读取：Intl 专属
    // id 折入与双收录判定均以本开关为前提）
    let buddy_ai_enabled = state
        .buddy_ai_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    // 审查修复（buddy-ai Intl 专属模型误路由）：CN 目录未命中时并查 Intl 目录
    //（§4.4 Global 专属模型可路由）——deepseek-v4.1-flash / gpt-* 等 Intl 专属 id
    // 不在 CN 目录，原实现 buddy_hit=None → 落 Trae 透传语义被误路由 Trae 上游。
    // 命中折入 buddy 源（final_model/effort/冷却链路复用），buddy_intl_only 标记
    // 随源携带：Buddy(CN) 池不得服务 Intl 专属模型（available 门控），buddy-ai 池正常取号
    // 审查修复（第1项）：折入以国际档开启为前提——关闭态 Intl 专属 id 不折入，
    // 恢复 Trae 透传既有语义（无条件折入会使国际档关闭时四源全不可用 →
    // 503 NoHealthy(Trae)，被误判为「Trae 池耗尽」）
    let cn_hit = super::wb_catalog::find(&catalog, &r.model).is_some();
    // 审查修复（第2项）：CN+Intl 双收录判定——CN 目录命中且 Intl 目录同收录，
    // 国际档开启时 buddy-ai 池可服务（available 门控放行依据）
    let buddy_in_both = cn_hit
        && super::wb_catalog::find(&intl_catalog, &r.model)
            .or_else(|| super::wb_catalog::find(&intl_catalog, model))
            .is_some();
    let intl_hit = if !cn_hit && buddy_ai_enabled {
        super::wb_catalog::find(&intl_catalog, &r.model)
            .or_else(|| super::wb_catalog::find(&intl_catalog, model))
            .map(|m| (m.id.clone(), None))
    } else {
        None
    };
    let buddy_intl_only = intl_hit.is_some();
    let buddy_hit = super::wb_catalog::find(&catalog, &r.model)
            .map(|_| {
                // T5.6③ 后台任务降级（显式开启才生效）：标题/摘要类短请求 → 目录最低倍率模型
                // issue #26 白名单感知：降级候选 ∩ 白名单为空则跳过降级（原模型已过端点准入）
                // Intl 专属请求跳过降级：降级候选出自 CN 目录，替换后的模型 id 发往
                // buddy-ai 池上游不存在（审查修复）
                let whitelist = super::unified_catalog::load_whitelist(&state.data_dir);
                let in_wl = |m: &str| super::unified_catalog::whitelist_allows(&whitelist, m);
                // issue #19 修复：降级候选按目标池对应域目录取——BuddyAi 池上游按
                // Intl 目录提供模型（CN 目录候选 id 在 Intl 上游可能不存在，双收录
                // 模型的廉价候选常为 CN 专属 id），Buddy 池仍用 CN 目录；两域候选
                // 均过白名单过滤，候选不存在时回落原模型（不降级）
                //（合并说明：main 侧「CN∩Intl 交集过滤」方案与本「分域取候选」方案
                // 同修 #19，本方案每池候选出自自身域目录、覆盖更优，保留本侧结构）
                let bg_on = state
                    .wb_bg_downgrade
                    .load(std::sync::atomic::Ordering::Relaxed)
                    && wb_model_route::is_background_task(body);
                let longctx_on = state
                    .wb_longctx_downgrade
                    .load(std::sync::atomic::Ordering::Relaxed)
                    // F-76④ 长上下文降档：输入粗估超阈值（默认 100k token）→ flash 档模型
                    && wb_model_route::estimate_input_tokens(body)
                        >= wb_model_route::LONGCTX_TOKEN_THRESHOLD;
                let domain_candidate = |intl_domain: bool| -> Option<String> {
                    let dom = if intl_domain {
                        intl_catalog.as_slice()
                    } else {
                        catalog.as_slice()
                    };
                    if bg_on {
                        wb_model_route::cheapest_catalog_model_filtered(dom, &in_wl)
                    } else if longctx_on {
                        wb_model_route::flash_catalog_model_filtered(dom, &in_wl)
                    } else {
                        None
                    }
                };
                let cn_final = domain_candidate(false).unwrap_or_else(|| r.model.clone());
                let intl_final = domain_candidate(true).unwrap_or_else(|| r.model.clone());
                ((cn_final, r.effort_hint.clone()), intl_final)
            })
            .map(|(hit, intl_final)| (Some(hit), Some(intl_final)))
            .unwrap_or((intl_hit, None));

    // ③ Trae 源判定：canonical_id 命中 Trae 模型列表；未命中任何目录时保持
    // 透传语义（单源 Trae，与现状一致）。
    // issue #31 T3.1 后缀剥离跨池：Trae-only 模型带路由后缀（如 "glm-5.3-thinking"，
    // 基名仅在 Trae 源）时，Buddy 管线 ④ 段因基名不在 Buddy 目录直落 direct，
    // 全名带后缀透传 Trae 上游会报模型不存在 → 剥离后缀以基名作为 Trae 侧模型，
    // effort 提示随池携带。基名未命中 Trae 模型列表时保持原名透传（未知模型零行为差异）
    // issue #31 T4.2 Max Mode 入口：`-max` 后缀同链路剥离（内置后缀最低优先），
    // 剥离成功置 max 入口标志随池携带；整名以 -max/-thinking 结尾的真模型
    // （如 qwen3.8-max）整名在列表时优先透传、不误剥（t47 固化；未请求 = 不注入，
    // 保持零行为差异）。整名真模型开 Max 的逃生口：① 双后缀 "qwen3.8-max-max"
    // （整名不在列表 → 剥外层 -max → 基名命中列表，正常注入）；② OpenAI 协议
    // 请求直传 is_max_mode 字段（payload 层透传+增补不删未知字段，可存活到上游）；
    // Anthropic 协议经 anthropic_to_openai 重建请求对象会丢弃直传字段，不可用
    // Trae 模型列表一次加载（config_cache 缓存），③ 段剥离守卫与 trae_hit 判定共用
    let trae_list = models_sync::load_models(&state.data_dir);
    let in_trae = |n: &str| trae_list.iter().any(|m| canonical_id(&m.id) == canonical_id(n));
    // ③⁻ Qoder 源判定（p3-3）前移：③ 段后缀剥离需先知道 Qoder 是否命中，防
    // 「Qoder 目录收录形如 xxx-max 的模型、基名恰在 Trae 列表」时被 -max 剥离
    // 劫持到 Trae 池（审查 m1）。resolve 先 CN 区后 Global 区——v1 恒以 CN 网关
    // 执行，区域分流见 qoder_route 注释。final model 取目录归一化 id。
    // v1 不做路由后缀剥离（-thinking/-max 是 WB/Trae 侧机制），请求素材由执行路径按条目再查
    let qoder_hit = qoder_upstream::resolve(model, qoder_upstream::QoderRegion::Cn)
        .and_then(|entry| entry.get("id").and_then(Value::as_str).map(str::to_string));
    let trae_final = if buddy_hit.0.is_none() {
        if in_trae(model) {
            // 整名命中：真模型名透传，不做任何后缀剥离
            (model.to_string(), None, false)
        } else if qoder_hit.is_some() {
            // Qoder 目录命中且整名未命中 Trae 列表：跳过后缀剥离，整名保留
            // （剥离只服务 WB/Trae 侧机制，此处剥离会把 Qoder 专属模型改写为
            // 基名误路由 Trae）
            (model.to_string(), None, false)
        } else if let Some((base, hint)) = wb_model_route::strip_route_suffix(model, &cfg)
            .filter(|(base, _)| in_trae(base))
        {
            (base, hint, false)
        } else if let Some(base) =
            wb_model_route::strip_max_suffix(model).filter(|base| in_trae(base))
        {
            // 可观测性：基名在 Trae 列表但不在 Max Mode 支持表（efforts::
            // TRAE_MAX_MODE_REF）→ 注入层将静默跳过 is_max_mode，落日志避免
            // 「请求带 -max 却无 1M 行为」无从排查（零行为变更）
            if !super::efforts::trae_max_mode_supported(&canonical_id(&base)) {
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!(
                        "dispatch max-mode unsupported: model={} base={} (-max suffix stripped, is_max_mode not injected)",
                        model,
                        base,
                    ),
                );
            }
            (base, None, true)
        } else {
            (model.to_string(), None, false)
        }
    } else {
        (model.to_string(), None, false)
    };
    let canonical = canonical_id(&trae_final.0);
    // issue #145 渠道内单模型开关：L1 meta.enabled=false 的模型不再作为 Trae 源
    // 参与调度（目录保留条目，管理端可重新启用）。其余源判定不受影响——多源模型
    // 仍可由 Buddy/Qoder/custom 服务；唯一源被禁则选池循环耗尽 → NoHealthy
    //（免费账号请求付费受限模型的场景由该开关根治，配合 PaidRestricted 不误冷却）
    let trae_model_disabled = super::unified_catalog::load_meta(&state.data_dir)
        .get(&canonical)
        .and_then(|l| l.enabled)
        .map_or(false, |e| !e);
    // v3 第四源：custom 上游组（P1 find_enabled_all，仅 enabled；提前求值供
    // Trae 透传判定与 sources 填充共用，保证两处口径同源）
    let custom_group = super::custom_models::find_enabled_all(&state.data_dir, model);
    // Trae 透传语义收窄：Qoder 目录命中的模型不再默认透传 Trae（透传会让
    // Trae 上游报「模型不存在」；显式在 Trae 列表中的仍按双源处理）。
    // 审查修复（H-1）：custom 组命中的模型同样不再无条件透传 Trae——纯
    // custom 模型（无同名 trae/buddy/qoder 源）为唯一源直达（见下方 v3 注释），
    // 否则 Priority 策略下 Trae 排在 custom 之前，自建中转模型会被截胡打到
    // Trae 上游报「模型不存在」；Smart 下仅靠「Trae 未声明倍率后置」侥幸正确
    let trae_hit = !trae_model_disabled
        && ((buddy_hit.0.is_none() && qoder_hit.is_none() && custom_group.is_empty())
            || trae_list.iter().any(|m| canonical_id(&m.id) == canonical));
    let sources = ModelSources {
        buddy: buddy_hit.0,
        // issue #19：BuddyAi 域降级候选随源携带（按目标池取域）
        buddy_intl_final: buddy_hit.1,
        // 审查修复：Intl 专属模型源（CN 目录未命中、Intl 目录命中）
        buddy_intl_only,
        // 审查修复（第2项）：CN+Intl 双收录判定随源携带
        buddy_in_both,
        trae: trae_hit,
        trae_final,
        qoder: qoder_hit,
        // v3 第四源：custom 上游组（P1 find_enabled_all，与 buddy/qoder 判定并列
        // 全量收集后按序筛选同构；命中信息由执行路径重新按 data_dir 读取一致性
        // 由 config_cache 5s TTL 保证，此处仅做源存在性与健康判定）
        custom: custom_group,
    };

    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let qoder_enabled = state
        .qoder_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let trae_enabled = state
        .trae_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    // 仅 Buddy 源模型 + wb_enabled=false → 显式报错（§4.3：保持现有
    // wb_upstream_disabled 语义；"关闭"与"不可用"是两个概念）。
    // 三源扩展（p3-3 审查修复）：Qoder 源可用时 Buddy 关闭不构成"无源可用"，
    // 不早退、交由下方选池路由 Qoder（与下方镜像判定的 buddy.is_none() 守卫对称）
    // 审查修复（第3项）：早退门控与 available(BuddyAi) 同口径——仅 Intl 专属/
    // 双收录模型在国际档开启时跳过早退走国际池；CN 专属模型即使国际档开启
    // 仍报 WbDisabled（用户显式关闭 WB 上游的语义不被静默绕过）
    if sources.buddy.is_some()
        && !sources.trae
        && !wb_enabled
        && !sources.available(
            TargetPool::BuddyAi,
            trae_enabled,
            wb_enabled,
            qoder_enabled,
            buddy_ai_enabled,
        )
        && !(sources.qoder.is_some() && qoder_enabled)
        && sources.custom.is_empty()
    {
        return Err(DispatchError::WbDisabled);
    }
    // 仅 Qoder 源模型 + qoder_enabled=false → 同语义镜像
    if sources.qoder.is_some()
        && !sources.trae
        && sources.buddy.is_none()
        && !qoder_enabled
        && sources.custom.is_empty()
    {
        return Err(DispatchError::QoderDisabled);
    }
    // 仅 Trae 源模型 + trae_enabled=false → 同族镜像（Trae 开关默认开，
    // 关闭属用户显式动作，与"池耗尽 503"区分开）
    if sources.trae
        && sources.buddy.is_none()
        && sources.qoder.is_none()
        && !trae_enabled
        && sources.custom.is_empty()
    {
        return Err(DispatchError::TraeDisabled);
    }

    // ③⁻ Key 级资源池绑定（issue #25）：解析命中 Key 的约束快照。
    // 绑定是「池偏好」而非模型过滤——绑定池无该模型源时仍路由到唯一可用池（D2）；
    // 未绑定（""/非法值/无 Key）跟随全局调度，行为与改造前一致
    let key_rk = key_id.and_then(|id| super::api_keys::constraints_for(&state.data_dir, id));
    let bind = key_rk.as_ref().and_then(|rk| rk.bind_pool());
    // 绑定 Key 的白名单健康预检（仅绑定池生效）：绑定池内若仅剩白名单外账号，
    // 选池阶段即判不健康走全局 fallback，而非取号阶段才失败。
    // issue #30 混合白名单：未绑定但带池前缀的 Key 同样预检感知（按前缀作用域
    // 分池，零条目池 = 排除 → 预检不健康走 fallback）；旧未绑定 Key 维持现状——
    // 预检不感知白名单，F-35 约束由执行路径取号时自理
    let key_allowed_for = |pool: TargetPool| -> Option<HashSet<String>> {
        let rk = key_rk.as_ref()?;
        if bind.is_none() && !rk.is_mixed_whitelist() {
            return None;
        }
        rk.pool_constraints(pool.as_str()).and_then(|c| c.allowed)
    };

    // 会话池粘性（§4.4 软粘，TTL 60s，内存态不落盘）：命中且池仍可用 → 直接沿用。
    // Key 绑定池优先于粘性：粘性池 ≠ 绑定池时不沿用（重新按候选序选池）
    let sticky_key = pool_session_key(body);
    if let Some(sk) = &sticky_key {
        // issue #90 修复③a：前缀锁定池时跳过粘性（粘住的是调度选择，显式点名优先）
        let sticky_pool = if forced_pool.is_none() {
            take_sticky(state, sk)
        } else {
            None
        };
        if let Some(pool) = sticky_pool {
            if bind.map_or(true, |b| pool.as_str() == b)
                && sources.available(pool, trae_enabled, wb_enabled, qoder_enabled, buddy_ai_enabled)
                && pool_healthy(state, pool, &sources, wb_enabled, key_allowed_for(pool).as_ref())
            {
                record_sticky(state, sk, pool);
                return Ok(Resolved {
                    pool,
                    model: final_model_for(pool, &sources, model),
                    effort_hint: effort_for(pool, &sources),
                    max_mode_hint: max_mode_for(pool, &sources),
                    fallback_from: None,
                });
            }
        }
    }

    // ④ 按策略优先级序选池：有绑定时强制 [绑定池, 另一池] 候选序，跳过
    // per_model 与 smart 重排（用户 Key 级显式配置优先于全局策略）
    let policy = load_policy(&state.data_dir);
    let mut order: Vec<TargetPool> = if let Some(p) = forced_pool {
        // issue #90 修复③a：单池候选（不追加其他池，无跨池回退）
        vec![p]
    } else if let Some(b) = bind {
        // b 经 parse_bind_pool 归一化，仅可能为 "trae"/"buddy"/"buddy-ai"/"qoder"/"custom"，无需再 parse
        if b == "trae" {
            vec![TargetPool::Trae, TargetPool::Buddy]
        } else if b == "custom" {
            // v3 决策 C：绑定 custom = custom 优先 + 不可用按序回退（「偏好非硬限制」）；
            // 其余池由尾部补全循环按默认序追加
            vec![TargetPool::Custom]
        } else if b == "qoder" {
            // Qoder 绑定（审查修复：原先落 else 被当 Buddy 处理，「Key 绑定池 >
            // 全局策略」对 Qoder 断链——多源模型先打 Buddy，仅回退才到 Qoder）
            vec![TargetPool::Qoder, TargetPool::Buddy, TargetPool::Trae]
        } else if b == "buddy-ai" {
            // buddy-ai 绑定：国际档优先，国内 Buddy 兜底（同源回退不跨账号体系）
            vec![TargetPool::BuddyAi, TargetPool::Buddy, TargetPool::Trae]
        } else {
            vec![TargetPool::Buddy, TargetPool::Trae]
        }
    } else {
        policy
            .per_model
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| policy.priority.clone())
            .iter()
            .filter_map(|s| TargetPool::parse(s))
            .collect()
    };
    // per_model / 绑定序可能只写了部分池：其余可用源追加尾部，保证多源回退有序
    // （issue #90 修复③a：前缀锁定 = 单池候选，不追加。v3：custom 尾部兜底位——
    // 默认候选序 = [buddy, trae, qoder, custom]）
    if forced_pool.is_none() {
        // 尾部补全固定序与缺省 priority 序对齐（[buddy, trae, qoder, buddy-ai,
        // custom]）——审查修复：旧序把 buddy-ai 插在 trae 之前，Key 绑定 custom
        // 等部分序回退时 buddy-ai 会先于 trae/qoder 接管，与默认候选序口径不一
        // （A2 审查修复：序抽 POOL_FIXED_ORDER 共享常量，与显示层回退同源）
        for p in POOL_FIXED_ORDER {
            if !order.contains(&p) {
                order.push(p);
            }
        }
    }
    // ④+ 智能调度（strategy=smart）：可用源 ≥2 且无 per_model 显式覆盖时，
    // 按请求模型对候选池重排（到期 → 倍率/免费 → 积分多；并列保持优先级序）。
    // 有绑定/前缀锁定时跳过（绑定序/单池即最终候选序）
    if forced_pool.is_none() && bind.is_none()
        && policy.strategy == DispatchStrategy::Smart
        && policy.per_model.get(&canonical).is_none()
        && sources.available_count(trae_enabled, wb_enabled, qoder_enabled, buddy_ai_enabled) >= 2
    {
        order = smart_pool_order(state, &order, &canonical, &catalog, &intl_catalog, &sources);
    }

    let preferred = order
        .iter()
        .copied()
        .find(|p| sources.available(*p, trae_enabled, wb_enabled, qoder_enabled, buddy_ai_enabled));

    let mut fallback_from: Option<(TargetPool, FallbackReason)> = None;
    // issue #136 逐池归因：记录候选序中每个池的失败原因（unavailable / 账号不健康 /
    // 模型冷却），全部耗尽时随 app_log 落盘——「白名单把池排除」与「账号不健康」
    // 在一行日志内即可区分，替代盲猜调用路径
    let mut pool_reasons: Vec<String> = Vec::new();
    for (i, pool) in order.iter().copied().enumerate() {
        if !sources.available(pool, trae_enabled, wb_enabled, qoder_enabled, buddy_ai_enabled) {
            pool_reasons.push(format!("{}=unavailable", pool.as_str()));
            continue;
        }
        match pool_health(state, pool, &sources, wb_enabled, key_allowed_for(pool).as_ref()) {
            Ok(()) => {
                // 首个可用池胜出；此前有可用池失败 → 跨池回退 warn 日志（§4.5）。
                // 源剔除（如 wb_enabled=false）不算回退，不记日志
                if let Some((pref, reason)) = fallback_from {
                    log_fallback(state, model, pref, pool, reason);
                }
                // issue #99 ④：请求落池原因逐请求可观测——Key 绑定池生效或发生
                // 跨池回退时，在请求日志流落 [SCHED] 行（紧邻该请求的请求日志行；
                // log_fallback 另有 60s 节流的 app.log 运维行，此处为逐请求口径）
                if bind.is_some() || fallback_from.is_some() {
                    state.logger.log_sched_event(&format!(
                        "落池: pool={} model={} bind={} fallback_from={}",
                        pool.as_str(),
                        model,
                        bind.unwrap_or("-"),
                        fallback_from.map(|p| p.0.as_str()).unwrap_or("-"),
                    ));
                }
                if let Some(sk) = &sticky_key {
                    record_sticky(state, sk, pool);
                }
                return Ok(Resolved {
                    pool,
                    model: final_model_for(pool, &sources, model),
                    effort_hint: effort_for(pool, &sources),
                    max_mode_hint: max_mode_for(pool, &sources),
                    fallback_from: fallback_from.map(|(p, _)| p),
                });
            }
            Err(reason) => {
                pool_reasons.push(format!("{}={}", pool.as_str(), reason.as_str()));
                // 首选池不可用：双源 + fallback 开 + 后续还有可用源 → 按序回退；
                // 单源（或 fallback 关）→ 错误矩阵显式报错（§4.3，不静默换池）。
                // BM-3 审查修复：门控从 is_dual（家族计数，Buddy/BuddyAi 同族不叠加）
                // 改为可用源计数 ≥2——同目录模型的 buddy/buddy-ai 是两个独立失败域
                //（§3.1），姊妹池回退应放行；available_count 只数可用源，语义更精确
                //（旧 is_dual 把已停用池也计入家族数）
                let has_next = order[i + 1..]
                    .iter()
                    .any(|p| sources.available(*p, trae_enabled, wb_enabled, qoder_enabled, buddy_ai_enabled));
                let can_fallback = policy.fallback
                    && sources.available_count(trae_enabled, wb_enabled, qoder_enabled, buddy_ai_enabled) >= 2
                    && has_next;
                if !can_fallback {
                    // issue #136：提前终止（无更多回退目标）同样落逐池归因——实证
                    // 「白名单排除 + 单/末位可用池」场景走的是本提前 return，仅
                    // 零可用源才会自然走到循环末尾的 exhausted 块，两处都须落日志
                    log_dispatch_exhausted(state, model, key_id, preferred, &pool_reasons);
                    return Err(match (pool, reason) {
                        // 审查修复（模型冷却归因）：错误携带真实目标池，冷却表
                        // 本就按池独立（CN=wb 视图 / Intl=intl 视图 / qoder 独立表）
                        (TargetPool::Buddy, FallbackReason::ModelCooldown) => {
                            let view = super::wb_route::WbView::wb(state.clone());
                            let secs = super::wb_route::model_cooling_remaining(
                                &view,
                                &final_buddy_model(TargetPool::Buddy, &sources),
                            )
                            .unwrap_or(0);
                            DispatchError::ModelCooling(secs, TargetPool::Buddy)
                        }
                        // buddy-ai 模型级冷却同构映射（独立冷却表：同名模型跨区不混轮）
                        (TargetPool::BuddyAi, FallbackReason::ModelCooldown) => {
                            let view = super::wb_route::WbView::intl(state.clone());
                            let secs = super::wb_route::model_cooling_remaining(
                                &view,
                                &final_buddy_model(TargetPool::BuddyAi, &sources),
                            )
                            .unwrap_or(0);
                            DispatchError::ModelCooling(secs, TargetPool::BuddyAi)
                        }
                        // Qoder 模型级冷却同构映射（审查修复）：单源排队冷却显式 429
                        (TargetPool::Qoder, FallbackReason::ModelCooldown) => {
                            let secs = super::qoder_route::model_cooling_remaining_secs(
                                &sources.qoder.clone().unwrap_or_default(),
                            )
                            .unwrap_or(0) as i64;
                            DispatchError::ModelCooling(secs, TargetPool::Qoder)
                        }
                        // 审查修复：提前终止路径同样携带实际候选序——普通
                        // NoHealthy 会让 routes 层回退 POOL_FIXED_ORDER 硬编码序，
                        // Key 绑定（如 bind=qoder 序 [Qoder,Buddy,Trae]）/per_model
                        // 场景下固定序前置池被虚假注记 skipped、真实前置池漏注记
                        // （order 处于循环迭代借用中，此处只能 clone）
                        (p, _) => DispatchError::NoHealthyWithOrder(p, order.clone()),
                    });
                }
                if fallback_from.is_none() {
                    fallback_from = Some((pool, reason));
                }
            }
        }
    }
    // 仅零可用源（循环内全部 continue）才走到此处；有可用池但失败的场景均在
    // 循环内 !can_fallback 提前终止并落同款归因日志（issue #136）。落盘调度
    // 日志便于排查轮询异常（issue #30：凭证调度可观测，含 Key 展示名）
    log_dispatch_exhausted(state, model, key_id, preferred, &pool_reasons);
    // issue #90 修复③a：前缀锁定池不可用时错误指向该池（而非兜底 Trae，
    // 否则「qoder/xxx 但 qoder 池耗尽」报成 Trae 无健康账号，误导排障）。
    // 携带实际候选序（NoHealthyWithOrder）供 503 注记按真实序归因
    Err(DispatchError::NoHealthyWithOrder(
        // 审查修复：能走到循环尾说明零可用源（有可用池失败必在循环内
        // !can_fallback 提前终止），preferred 恒为 None，死防御 .or(preferred) 移除
        forced_pool.unwrap_or(TargetPool::Trae),
        order,
    ))
}

/// dispatch 耗尽/提前终止的逐池归因日志（issue #30 可观测 + issue #136 逐池
/// candidates）：候选序中每个池的失败原因（unavailable / no_healthy_account /
/// model_cooldown）一行落盘，「白名单把池排除」与「账号不健康」直接可辨。
/// 连续相同归因 60s 内去重（客户端重试风暴防刷屏，对齐调度器 Busy 不重复
/// 落日志先例），data_dir 参与去重键——多实例/测试目录互不干扰
fn log_dispatch_exhausted(
    state: &Arc<ApiSharedState>,
    model: &str,
    key_id: Option<&str>,
    preferred: Option<TargetPool>,
    reasons: &[String],
) {
    let key_label = key_id
        .map(|id| super::api_keys::key_name_for(&state.data_dir, id))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "anonymous".to_string());
    let line = format!(
        "dispatch exhausted: model={} key={} preferred={} candidates={}",
        model,
        key_label,
        preferred.map(|p| p.as_str()).unwrap_or("none"),
        reasons.join(","),
    );
    static LAST: std::sync::OnceLock<std::sync::Mutex<(String, std::time::Instant)>> =
        std::sync::OnceLock::new();
    let cell = LAST
        .get_or_init(|| std::sync::Mutex::new((String::new(), std::time::Instant::now())));
    if let Ok(mut g) = cell.lock() {
        let dedup_key = format!("{}\u{1}{}", state.data_dir.display(), line);
        if g.0 == dedup_key && g.1.elapsed() < std::time::Duration::from_secs(60) {
            return;
        }
        *g = (dedup_key, std::time::Instant::now());
    }
    crate::fs_utils::app_log(&state.data_dir, &line);
}

/// 会话池粘性键（§4.4：复用现有粘性会话键，conversation 维度）。
/// 空指纹（无 messages / 无 conversation_id）不可粘 → None。
/// pub：custom 上游组会话粘上游（custom_lb）复用同款会话键口径
pub fn pool_session_key(body: &Value) -> Option<String> {
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
    intl_catalog: &[super::wb_catalog::WbModel],
    sources: &ModelSources,
) -> Vec<TargetPool> {
    let trae_stats = state.pool.stats();
    let wb_stats = state.wb_pool.stats();
    let buddy_ai_stats = state.buddy_ai_pool.stats();
    let qoder_stats = state.qoder_pool.stats();
    // 倍率数据源：Buddy = wb 目录原始值（0 = 免费）；Trae = 官网同步声明值（None = 未声明）；
    // Qoder = 目录 credits 文本解析（"x0.1 credits" → 0.1）。
    // 各列表均为内存缓存，热路径零磁盘读
    let trae_rate = models_sync::load_models(&state.data_dir)
        .iter()
        .find(|m| canonical_id(&m.id) == canonical)
        .and_then(|m| m.rate);
    let buddy_rate = sources
        .buddy
        .as_ref()
        .and_then(|(m, _)| catalog.iter().find(|c| c.id == *m))
        .map(|c| c.rate);
    // issue #20：buddy-ai 档倍率改查 Intl 动态目录（/v3/config 实抓倍率优先，
    // 此前复用 CN 目录值导致 smart 排序对 buddy-ai 档失准）；该模型未收录 Intl
    // 目录时回退 CN 目录值。模型取 BuddyAi 域最终模型（与 #19 按池取域同口径，
    // BuddyAi 池实发模型 = buddy_intl_final）
    let buddy_ai_rate = if sources.buddy.is_some() {
        let m = final_buddy_model(TargetPool::BuddyAi, sources);
        (!m.is_empty())
            .then_some(m)
            .and_then(|m| {
                intl_catalog
                    .iter()
                    .find(|c| c.id == m)
                    .or_else(|| catalog.iter().find(|c| c.id == m))
            })
            .map(|c| c.rate)
    } else {
        None
    };
    let qoder_rate = sources
        .qoder
        .as_ref()
        .and_then(|id| {
            // 免费活动覆盖同口径：开关开启时 Qwen3.8-Flash 按 0 倍率参与排序
            //（免费最优），否则排序键与取号豁免口径分叉
            qoder_upstream::list_overridden(
                state
                    .qoder_flash_free_rate
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .into_iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(id.as_str()))
        })
        .and_then(|m| {
            m.get("credits")
                .and_then(Value::as_str)
                .and_then(qoder_upstream::credits_rate_of)
        });
    // custom 组倍率键（v3 四池参与）：组内最小 rate 正常参与（rate=0 免费上游
    // 可在 Smart 下自然胜出）；到期/积分两键无数据按后置处理，不阻塞排序
    let custom_rate = sources
        .custom
        .iter()
        .map(|m| m.rate)
        .fold(None::<f64>, |acc, r| Some(acc.map_or(r, |a| a.min(r))));
    let key_of = |p: TargetPool| -> SmartKey {
        let ((earliest, total), rate) = match p {
            TargetPool::Trae => (trae_stats, trae_rate),
            TargetPool::Buddy => (wb_stats, buddy_rate),
            // buddy-ai：Intl 动态目录倍率（issue #20：/v3/config 实抓值优先，
            // 未收录 Intl 目录回退 CN 目录值；R-9 同账号体系定价同构），积分快照取国际池
            TargetPool::BuddyAi => (buddy_ai_stats, buddy_ai_rate),
            TargetPool::Qoder => (qoder_stats, qoder_rate),
            // custom：到期 = i64::MAX 后置、倍率 = 组内最小 rate 正常参与、
            // 积分和 = f64::MIN（-total = f64::MAX，第三键后置）
            TargetPool::Custom => ((None, f64::MIN), custom_rate),
        };
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
            let model = final_buddy_model(TargetPool::Buddy, sources);
            let view = super::wb_route::WbView::wb(state.clone());
            if super::wb_route::model_cooling_remaining(&view, &model).is_some() {
                return Err(FallbackReason::ModelCooldown);
            }
            if !state.wb_pool.has_selectable_in(allowed, false) {
                return Err(FallbackReason::NoHealthyAccount);
            }
            Ok(())
        }
        TargetPool::BuddyAi => {
            // 与 Buddy 同构：独立模型冷却表 + 独立池账号健康预检（§3.1 失败域隔离）
            // issue #19：冷却/预检模型取 BuddyAi 域最终模型（Intl 目录候选）
            let model = final_buddy_model(TargetPool::BuddyAi, sources);
            let view = super::wb_route::WbView::intl(state.clone());
            if super::wb_route::model_cooling_remaining(&view, &model).is_some() {
                return Err(FallbackReason::ModelCooldown);
            }
            if !state.buddy_ai_pool.has_selectable_in(allowed, false) {
                return Err(FallbackReason::NoHealthyAccount);
            }
            Ok(())
        }
        TargetPool::Trae => {
            if !state.pool.has_selectable_in(allowed, false) {
                return Err(FallbackReason::NoHealthyAccount);
            }
            Ok(())
        }
        TargetPool::Qoder => {
            // 模型级冷却预检（审查修复，与 Buddy 同构）：排队/超限类错误后窗口
            // 内快速回退——多源交由后续池，单源在 resolve_target 显式 429
            if super::qoder_route::model_cooling_remaining_secs(
                &sources.qoder.clone().unwrap_or_default(),
            )
            .is_some()
            {
                return Err(FallbackReason::ModelCooldown);
            }
            // 账号级健康预检（issue #111：免费模型豁免积分维度过滤，口径对齐取号路径——
            // 预检比取号严会让免费模型请求在预检即 NoHealthy，白改）
            let qoder_free = sources
                .qoder
                .as_deref()
                .map(|m| {
                    // 免费活动覆盖与取号同口径（resolve_overridden），否则开关开启时
                    // 免费活动模型（Qwen3.8-Flash / DeepSeek-Flash，issue #156）预检
                    // 仍按目录价判非免费，零积分池被误挡
                    qoder_upstream::resolve_overridden(
                        m,
                        qoder_upstream::QoderRegion::Cn,
                        state
                            .qoder_flash_free_rate
                            .load(std::sync::atomic::Ordering::Relaxed),
                    )
                    .map(|e| qoder_upstream::entry_is_free(&e))
                    .unwrap_or(false)
                })
                .unwrap_or(false);
            if !state.qoder_pool.has_selectable_in(allowed, qoder_free) {
                return Err(FallbackReason::NoHealthyAccount);
            }
            Ok(())
        }
        // v3：custom 上游组健康判定——任一上游未熔断且（若 Key 绑定白名单）
        // 在 allowed 内即可候选（组内选路跳过熔断与白名单外条目；全不可选 →
        // NoHealthyAccount 走现有 fallback 按序回退下一池）。审查修复：预检
        // 补 allowed 感知，与执行侧 pick_upstream 同口径（对齐四池
        // has_selectable_in）——否则白名单排除组内全部健康条目时预检误判 Ok
        // → pick_next None → 503 exhausted，丢失本可成功的跨池 fallback
        TargetPool::Custom => {
            if sources.custom.iter().any(|cm| {
                allowed.map_or(true, |set| set.contains(&cm.id))
                    && !super::custom_lb::circuit_open(&cm.id)
            }) {
                Ok(())
            } else {
                Err(FallbackReason::NoHealthyAccount)
            }
        }
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

/// Buddy 源最终模型（issue #19 按目标池取域）：BuddyAi 池取 Intl 目录降级候选
/// （buddy_intl_final，候选 id 在 Intl 上游必存在），Buddy 池取 CN 目录候选
///（buddy.0）；Intl 专属折入与未降级场景两域同值
fn final_buddy_model(pool: TargetPool, sources: &ModelSources) -> String {
    if pool == TargetPool::BuddyAi {
        if let Some(m) = &sources.buddy_intl_final {
            return m.clone();
        }
    }
    sources
        .buddy
        .as_ref()
        .map(|(m, _)| m.clone())
        .unwrap_or_default()
}

fn final_model_for(pool: TargetPool, sources: &ModelSources, request_model: &str) -> String {
    match pool {
        TargetPool::Buddy | TargetPool::BuddyAi => final_buddy_model(pool, sources),
        // Trae：后缀剥离后的基名（issue #31 T3.1；无后缀/透传语义 = 原名）
        TargetPool::Trae => sources.trae_final.0.clone(),
        // Qoder：目录归一化 id（去空白形态，执行路径据此 resolve 请求素材）
        TargetPool::Qoder => sources.qoder.clone().unwrap_or_default(),
        TargetPool::Custom => request_model.to_string(),
    }
}

fn effort_for(pool: TargetPool, sources: &ModelSources) -> Option<String> {
    match pool {
        TargetPool::Buddy | TargetPool::BuddyAi => sources.buddy.as_ref().and_then(|(_, h)| h.clone()),
        // Trae：路由级 effort 提示同样随池携带（routes 层统一档位 → wire 转换下发）
        TargetPool::Trae => sources.trae_final.1.clone(),
        // Qoder：v1 无路由级 effort 通道（思考档位由执行路径按目录条目解析）
        TargetPool::Qoder => None,
        TargetPool::Custom => None,
    }
}

/// Max Mode 入口标志随池取值（issue #31 T4.2）：仅 Trae 池携带入口标志；实际注入
/// 由 routes 层结合 efforts::TRAE_MAX_MODE_REF 门控，Buddy/Custom/Qoder 恒 false
fn max_mode_for(pool: TargetPool, sources: &ModelSources) -> bool {
    match pool {
        TargetPool::Trae => sources.trae_final.2,
        TargetPool::Buddy | TargetPool::BuddyAi | TargetPool::Custom | TargetPool::Qoder => false,
    }
}

// ==================== 会话池粘性（§4.4，内存态） ====================

// 池粘性 TTL 兜底值（秒）：软粘防抖动，重启即清不落盘；
// 实际 TTL 按胜出池读 ApiSharedState 三个 per-pool 原子量（trae/wb/qoder
// _pool_sticky_ttl_secs，F-76② 可配置，默认 300s——上游对同上下文有
// prefill 缓存收益，粘性命中 = 缓存命中）
// （默认值由 default_pool_sticky_ttl_secs() 提供，无需独立常量）

/// 取出池粘性绑定（命中后移除过期项；命中项由调用方决定是否续期）
fn take_sticky(state: &Arc<ApiSharedState>, key: &str) -> Option<TargetPool> {
    let now = now_ts();
    let mut map = state.pool_sticky.lock().unwrap_or_else(|e| e.into_inner());
    // 懒清理过期项
    map.retain(|_, (_, exp)| *exp > now);
    map.get(key).map(|(p, _)| *p)
}

/// 池粘性 TTL 按胜出池取值（per-pool 三参数，F-76② 拆分版）：
/// Custom（v3：custom 胜出后正常 record_sticky）无独立 TTL 配置，兜底取 Trae 值
///
/// 命名区分（勿混淆，issue 复核）：`*_pool_sticky_ttl_secs` = 池粘性 TTL
/// （本函数唯一消费方，内存态 state.pool_sticky，重启即清）；`*_sticky_ttl_secs`
/// = 显式会话粘性 TTL（落库 sticky_bindings，经 set_explicit_ttl 热应用）——
/// 两条链路独立，此处取 pool 变体是正确取值
fn pool_sticky_ttl_secs(state: &ApiSharedState, pool: TargetPool) -> u64 {
    let cell = match pool {
        TargetPool::Trae | TargetPool::Custom => &state.trae_pool_sticky_ttl_secs,
        TargetPool::Buddy => &state.wb_pool_sticky_ttl_secs,
        TargetPool::BuddyAi => &state.buddy_ai_pool_sticky_ttl_secs,
        TargetPool::Qoder => &state.qoder_pool_sticky_ttl_secs,
    };
    cell.load(std::sync::atomic::Ordering::Relaxed)
}

/// 记录/续期池粘性绑定（TTL 按胜出池的独立配置取值，F-76② per-pool）
fn record_sticky(state: &Arc<ApiSharedState>, key: &str, pool: TargetPool) {
    let ttl = pool_sticky_ttl_secs(state, pool).max(1) as i64;
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
        // 目录名唯一性交给 fs_utils::test_temp_dir（进程内计数器），防 CI 偶发撞名
        let dir = crate::fs_utils::test_temp_dir("twa_dispatch_test");
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
            fallback_log_last_ms: std::sync::Mutex::new(std::collections::HashMap::new()),
            wb_enabled: AtomicBool::new(true),
            wb_sanitize: AtomicBool::new(true),
            wb_default_thinking: AtomicBool::new(false),
            wb_tool_exec: AtomicBool::new(false),
            wb_bg_downgrade: AtomicBool::new(false),
            wb_longctx_downgrade: AtomicBool::new(false),
            wb_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            trae_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            trae_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            wb_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            qoder_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            qoder_flash_free_rate: std::sync::atomic::AtomicBool::new(true),
            wb_sticky: super::super::wb_sticky::StickyStore::default(),
            model_cooldowns: std::sync::Mutex::new(HashMap::new()),
            buddy_ai_model_cooldowns: std::sync::Mutex::new(HashMap::new()),
            default_model: "deepseek-v4-flash".into(),
            data_dir: dir.clone(),
            listen_host: std::sync::Mutex::new("127.0.0.1".to_string()),
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
            buddy_ai_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            buddy_ai_probe_ok: std::sync::atomic::AtomicI64::new(-1),
            trae_jwt_refresh: None,
            trae_enabled: AtomicBool::new(true),
            // 遗留调度矩阵测试编码 Buddy/Trae 契约（§9.1）：按「Qoder 未启用」
            // 的部署形态构造；Qoder 参与调度的行为由 qoder 启用态的专属测试覆盖
            qoder_pool: super::super::pool::ApiPool::new(),
            qoder_enabled: AtomicBool::new(false),
            qoder_identity: None,
            qoder_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            qoder_sticky_enabled: AtomicBool::new(false),
            qoder_sanitize: AtomicBool::new(true),
            qoder_sticky: super::super::wb_sticky::StickyStore::default(),
            trae_sticky: super::super::wb_sticky::StickyStore::default(),
            buddy_ai_sticky: super::super::wb_sticky::StickyStore::default(),
            buddy_ai_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            buddy_ai_pool: super::super::pool::ApiPool::new(),
            buddy_ai_enabled: AtomicBool::new(false),
            buddy_ai_sticky_enabled: AtomicBool::new(false),
            buddy_ai_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            custom_upstream_sticky: std::sync::Mutex::new(std::collections::HashMap::new()),
            custom_lb_counters: std::sync::Mutex::new(std::collections::HashMap::new()),
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
                allowed_groups: vec![],
                allowed_tags: vec![],
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
                    credits_fetched_at: None,
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

    // ---------- issue #90 修复③a：池前缀模型 id ----------

    /// split_pool_prefix：合法前缀解析（trim/大小写宽容）；未知池名、
    /// 空裸名、无前缀 → None（原样透传，不误吞斜杠模型名）。
    /// v3：custom 转正为合法前缀（锁池 Custom）
    #[test]
    fn t90_split_pool_prefix_parse() {
        assert_eq!(
            split_pool_prefix("trae/glm-5.3"),
            Some((TargetPool::Trae, "glm-5.3"))
        );
        assert_eq!(
            split_pool_prefix("Qoder/DeepSeek-Flash"),
            Some((TargetPool::Qoder, "DeepSeek-Flash"))
        );
        assert_eq!(
            split_pool_prefix(" buddy/ hy4 "),
            Some((TargetPool::Buddy, "hy4"))
        );
        // v3 决策 B：custom/ 前缀锁池转正
        assert_eq!(split_pool_prefix("custom/m1"), Some((TargetPool::Custom, "m1")));
        assert_eq!(split_pool_prefix("openai/gpt-4o"), None);
        assert_eq!(split_pool_prefix("trae/"), None);
        assert_eq!(split_pool_prefix("glm-5.3"), None);
    }

    /// 前缀锁定：默认策略 Buddy 优先，`trae/` 前缀强制 Trae 胜出、`buddy/` 维持
    /// Buddy；Resolved.model 均剥为裸名（执行路径按裸名解析目录）
    #[test]
    fn t90_prefix_locks_pool_over_policy() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        let r = f.resolve("trae/glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "glm-5.3");
        let r = f.resolve("buddy/glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert_eq!(r.model, "glm-5.3");
    }

    /// 锁定池无该模型源/池耗尽 → NoHealthy(锁定池)，不回退其他池、不误报 Trae
    ///（主选循环耗尽现统一走 NoHealthyWithOrder 携带实际候选序——前缀锁定 = 单池
    /// [锁定池]，503 注记侧无前置池可归因，行为不变）
    #[test]
    fn t90_prefix_unavailable_source_errors_locked_pool() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        assert_eq!(
            f.resolve("qoder/glm-5.3").unwrap_err(),
            DispatchError::NoHealthyWithOrder(TargetPool::Qoder, vec![TargetPool::Qoder])
        );
    }

    /// v3：同名 custom + trae/buddy 多源全部健康 → 按统一调度规则落 Buddy
    /// （custom 尾部兜底位，不再 ⓪ 短路恒赢；替代原「custom 恒赢」断言）；
    /// `custom/xxx` 前缀锁池直达；`trae/xxx` 前缀不被 custom 劫持。
    /// 用 Priority 策略隔离 Smart 干扰（custom rate=0 免费在 Smart 下按设计可胜出，
    /// 该行为由 Smart 排序键语义保证，不属本用例断言范围）
    #[test]
    fn t90_prefix_lock_not_hijacked_by_same_name_custom() {
        let mut p = policy_default();
        p.strategy = DispatchStrategy::Priority;
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        let cm: super::super::custom_models::CustomModel = serde_json::from_value(json!({
            "name": "glm-5.3",
            "base_url": "https://custom.example.invalid",
            "enabled": true
        }))
        .unwrap();
        super::super::custom_models::upsert(&f.dir, cm).unwrap();
        // v3：裸名按统一调度规则 → Buddy（默认候选序 custom 尾部兜底）
        assert_eq!(f.resolve_no_session("glm-5.3").unwrap().pool, TargetPool::Buddy);
        // custom/ 前缀锁池直达（v3 决策 B 转正）
        let r = f.resolve_no_session("custom/glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Custom);
        assert_eq!(r.model, "glm-5.3");
        // 前缀锁池不被劫持：显式点名 Trae 池服务
        let r = f.resolve_no_session("trae/glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "glm-5.3");
    }

    /// v3 P4：纯 custom 模型（无同名 trae/buddy/qoder 源）= 唯一源直达 Custom
    /// （主场景回归——自建中转等用法零行为变化）
    #[test]
    fn t91_custom_only_model_direct() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        super::super::custom_models::upsert(
            &f.dir,
            serde_json::from_value(json!({
                "name": "my-relay-model",
                "base_url": "https://relay.example.invalid",
                "enabled": true
            }))
            .unwrap(),
        )
        .unwrap();
        let r = f.resolve_no_session("my-relay-model").unwrap();
        assert_eq!(r.pool, TargetPool::Custom);
        assert_eq!(r.model, "my-relay-model");
    }

    /// 审查修复（H-1）：Priority 策略下纯 custom 模型不被 Trae 透传截胡——
    /// trae_hit 透传分支须对 custom 组命中收窄，否则候选序 [trae, custom]
    /// 让 Trae 先接管并报「模型不存在」（Smart 仅靠倍率后置侥幸正确，无测试固化）
    #[test]
    fn t91_custom_only_model_priority_not_hijacked_by_trae() {
        let mut p = policy_default();
        p.strategy = DispatchStrategy::Priority;
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&p));
        f.seed_healthy(true);
        super::super::custom_models::upsert(
            &f.dir,
            serde_json::from_value(json!({
                "name": "my-relay-model",
                "base_url": "https://relay.example.invalid",
                "enabled": true
            }))
            .unwrap(),
        )
        .unwrap();
        let r = f.resolve_no_session("my-relay-model").unwrap();
        assert_eq!(r.pool, TargetPool::Custom);
        assert_eq!(r.model, "my-relay-model");
    }

    /// v3 P4：custom 组全熔断 + 多源 → 按候选序回退下一池（fallback_from=Custom）
    #[test]
    fn t91_custom_all_circuit_falls_back() {
        let mut p = policy_default();
        p.per_model.insert("glm-5.3".into(), vec!["custom".into(), "trae".into()]);
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        let cm: super::super::custom_models::CustomModel = serde_json::from_value(json!({
            "name": "glm-5.3",
            "base_url": "https://custom.example.invalid",
            "enabled": true
        }))
        .unwrap();
        let list = super::super::custom_models::upsert(&f.dir, cm).unwrap();
        // 单条上游全熔断（组内任一未熔断即可用 → 记账唯一一条即整体不健康）
        super::super::custom_lb::note_circuit_failure(&list[0].id, super::super::custom_lb::FailKind::Server);
        let r = f.resolve_no_session("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.fallback_from, Some(TargetPool::Custom));
        assert!(f
            .app_log_contains("dispatch fallback: model=glm-5.3 pool=trae preferred=custom reason=no_healthy_account"));
        super::super::custom_lb::clear_circuit(&list[0].id);
    }

    /// 审查修复（Major-2）：pool_health Custom 分支补齐 allowed 白名单感知（与
    /// 四池 has_selectable_in 同口径）——Key 绑定 custom 白名单排除组内全部
    /// 健康条目时，预检须判不健康走跨池 fallback；修复前误判 Ok 选中 Custom，
    /// 执行侧 pick_next None → 503 exhausted，丢失本可成功的回退
    #[test]
    fn t91_custom_whitelist_excluded_falls_back() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        let list = super::super::custom_models::upsert(
            &f.dir,
            serde_json::from_value(json!({
                "name": "glm-5.3",
                "base_url": "https://custom.example.invalid",
                "enabled": true,
                // 显式唯一 id：upsert 仅在 id 为空时按时间戳生成，并行测试同时间片
                // 可能撞键（进程级全局熔断表按 id 键控）→ 显式 id 隔离熔断状态
                "id": "cm-wlfix00000001"
            }))
            .unwrap(),
        )
        .unwrap();

        let cm_id = list[0].id.clone();
        // 两个 Key 均须在首次 resolve 前种入：constraints_for 经 KEYS_STATE
        // 首读固化内存副本，其后 seed 的 Key 不可见（bind 丢失走默认候选序）
        f.seed_key("k-exclude", "custom", &["custom:other-model", "trae:t1"]);
        f.seed_key("k-include", "custom", &[format!("custom:{cm_id}").as_str(), "trae:t1"]);
        // 白名单不含组内唯一条目（trae:t1 保 fallback 目标池健康）→ 预检不健康 → 回退 Trae
        let r = f.resolve_key("glm-5.3", "k-exclude").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.fallback_from, Some(TargetPool::Custom));
        // 正对照：白名单含组内条目 → 正常选中 Custom，不误伤
        let r = f.resolve_key("glm-5.3", "k-include").unwrap();
        assert_eq!(r.pool, TargetPool::Custom);
        assert_eq!(r.fallback_from, None);
    }

    /// M-2 补充（origin/main 同名用例，覆盖与上方 t91 互补的旧语义路径）：
    /// Key 绑定 custom + 裸 uid 白名单（issue #30 绑定池旧语义）+ 白名单条目熔断
    /// → 组内全部健康条目被白名单排除 → 预检不健康走跨池 fallback，且落 fallback
    /// 归因日志；熔断清除后照常落 Custom。用 Auth 类 600s 长窗记账（pool_health
    /// 仅判 circuit_open，不分类别）：Server 类基准窗仅 30s，全量并行跑时
    /// constraints_for 可能在 KEYS_STATE 锁上被其他测试的 vault 加密操作阻塞超
    /// 30s，窗口过期会假失败
    #[test]
    fn t91b_custom_bind_whitelist_circuit_falls_back() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        // 绑定 custom + 白名单仅含 cm-whitelist0001（bind Key → pool_constraints
        // 旧语义生效）；两条 custom 均显式唯一 id
        f.seed_key("k-cw", "custom", &["cm-whitelist0001"]);
        let cm1: super::super::custom_models::CustomModel = serde_json::from_value(json!({
            "id": "cm-whitelist0001",
            "name": "glm-5.3",
            "base_url": "https://custom-a.example.invalid",
            "enabled": true
        }))
        .unwrap();
        let cm2: super::super::custom_models::CustomModel = serde_json::from_value(json!({
            "id": "cm-excluded00001",
            "name": "glm-5.3",
            "base_url": "https://custom-b.example.invalid",
            "enabled": true
        }))
        .unwrap();
        super::super::custom_models::upsert(&f.dir, cm1).unwrap();
        super::super::custom_models::upsert(&f.dir, cm2).unwrap();

        // 反对照：白名单条目熔断 → 预检不健康 → 落 Trae（fallback_from=Custom）
        super::super::custom_lb::note_circuit_failure(
            "cm-whitelist0001",
            super::super::custom_lb::FailKind::Auth,
        );
        let r = f.resolve_key("glm-5.3", "k-cw").unwrap();
        assert_eq!(r.pool, TargetPool::Trae, "白名单排除全部健康条目须走跨池回退");
        assert_eq!(r.fallback_from, Some(TargetPool::Custom));
        assert!(f
            .app_log_contains("dispatch fallback: model=glm-5.3 pool=trae preferred=custom reason=no_healthy_account"));

        // 正对照：白名单条目熔断清除 → 预检健康 → 照常落 Custom
        super::super::custom_lb::clear_circuit("cm-whitelist0001");
        let r = f.resolve_key("glm-5.3", "k-cw").unwrap();
        assert_eq!(r.pool, TargetPool::Custom, "白名单内条目健康时正常命中 custom");
        super::super::custom_lb::clear_circuit("cm-whitelist0001");
    }

    /// v3 P4：custom/xxx 前缀锁池 + 组全熔断 → NoHealthy(Custom) 显式报错
    /// 不跨池回退（单池候选语义）
    #[test]
    fn t91_custom_prefix_lock_errors_when_exhausted() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        let cm: super::super::custom_models::CustomModel = serde_json::from_value(json!({
            "name": "glm-5.3",
            "base_url": "https://custom.example.invalid",
            "enabled": true
        }))
        .unwrap();
        let list = super::super::custom_models::upsert(&f.dir, cm).unwrap();
        super::super::custom_lb::note_circuit_failure(&list[0].id, super::super::custom_lb::FailKind::Server);
        assert_eq!(
            f.resolve_no_session("custom/glm-5.3").unwrap_err(),
            DispatchError::NoHealthyWithOrder(TargetPool::Custom, vec![TargetPool::Custom])
        );
        super::super::custom_lb::clear_circuit(&list[0].id);
    }

    /// v3 决策 C：Key 绑定 custom + 同名多源 → 落 custom（绑定优先）
    #[test]
    fn t91_bind_custom_prefers_custom() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        super::super::custom_models::upsert(
            &f.dir,
            serde_json::from_value(json!({
                "name": "glm-5.3",
                "base_url": "https://custom.example.invalid",
                "enabled": true
            }))
            .unwrap(),
        )
        .unwrap();
        f.seed_key("k-custom", "custom", &[]);
        assert_eq!(f.resolve_key("glm-5.3", "k-custom").unwrap().pool, TargetPool::Custom);
    }

    /// v3 P4：per_model 数组写 "custom"（任意位置）→ 校验放行且生效
    #[test]
    fn t91_per_model_custom_position() {
        let mut p = policy_default();
        p.per_model.insert("glm-5.3".into(), vec!["custom".into(), "buddy".into()]);
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f.seed_healthy(true);
        f.seed_healthy(false);
        super::super::custom_models::upsert(
            &f.dir,
            serde_json::from_value(json!({
                "name": "glm-5.3",
                "base_url": "https://custom.example.invalid",
                "enabled": true
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(f.resolve_no_session("glm-5.3").unwrap().pool, TargetPool::Custom);
        // load_policy 防御过滤后 custom 条目保留（不再被 parse=None 过滤丢弃）
        let loaded = load_policy(&f.dir);
        assert_eq!(
            loaded.per_model.get("glm-5.3").map(|v| v.join(",")),
            Some("custom,buddy".to_string())
        );
    }

    /// v3 §7.4：Smart 重排四池——custom rate=0（免费）+ trae 同名（rate 未声明）
    /// → Smart 按倍率键 custom 胜出（无 per_model/priority 显式覆盖时）
    #[test]
    fn t91_smart_free_custom_wins_over_trae() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        super::super::custom_models::upsert(
            &f.dir,
            serde_json::from_value(json!({
                "name": "glm-5.3",
                "base_url": "https://custom.example.invalid",
                "enabled": true,
                "rate": 0.0
            }))
            .unwrap(),
        )
        .unwrap();
        // 默认 Smart 策略：可用源 = trae + custom ≥2 → 重排生效；
        // custom rate=0（免费）< trae 未声明（f64::MAX 后置）→ custom 胜出
        assert_eq!(f.resolve_no_session("glm-5.3").unwrap().pool, TargetPool::Custom);
    }

    /// v3 §7.4：同名双源 trae 不健康（池空 = 无健康账号）→ 序回退落 custom
    /// （per_model ["trae","custom"] 显式序；fallback 管线复用）
    #[test]
    fn t91_trae_unhealthy_falls_back_custom() {
        let mut p = policy_default();
        p.strategy = DispatchStrategy::Priority;
        p.per_model.insert("glm-5.3".into(), vec!["trae".into(), "custom".into()]);
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&p));
        // 不 seed trae 池（空池 = 无健康账号）；custom 组健康
        super::super::custom_models::upsert(
            &f.dir,
            serde_json::from_value(json!({
                "name": "glm-5.3",
                "base_url": "https://custom.example.invalid",
                "enabled": true
            }))
            .unwrap(),
        )
        .unwrap();
        let r = f.resolve_no_session("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Custom);
        assert_eq!(r.fallback_from, Some(TargetPool::Trae));
    }


    /// 覆写 Buddy 目录为仅含 `keep` 的显式条目（builtin_rev 拉平退出内置重建）：
    /// fixture 的 wb_model_catalog 写入缺 builtin_rev 会被 wb_catalog::load 重建为
    /// 内置 15 模型（含 glm-5.3/kimi-k3），无法构造「基名不在 Buddy 目录」的
    /// Trae-only 前提（issue #31 T3.1 测试专用）
    fn catalog_only(dir: &Path, keep: &str) {
        crate::store::db(dir)
            .kv_set(
                "wb_model_catalog",
                &json!({
                    "models": [{"id": keep, "display": keep, "context_length": 128000,
                                "max_tokens": 64000, "supports_image": false,
                                "supported_efforts": ["low","medium","high"], "rate": 0.5}],
                    "builtin_rev": super::super::wb_catalog::BUILTIN_REV
                }),
            )
            .unwrap();
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
        assert!(f.app_log_contains("dispatch fallback: model=glm-5.3 pool=trae preferred=buddy reason=no_healthy_account"));
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

    /// 仅 Buddy 源 + Buddy 池耗尽 → 503 语义（NoHealthyWithOrder），不跨池回退
    #[test]
    fn t12_buddy_only_exhausted_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true); // Trae 池健康但非双源，不得回退
        assert_eq!(
            f.resolve("hy4").unwrap_err(),
            DispatchError::NoHealthyWithOrder(TargetPool::Buddy, POOL_FIXED_ORDER.to_vec())
        );
    }

    /// 仅 Buddy 源 + 模型级冷却 → 429 语义（ModelCooling）
    #[test]
    fn t13_buddy_only_model_cooldown_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(false);
        super::super::wb_route::note_model_failure(
            &super::super::wb_route::WbView::wb(f.state.clone()),
            "hy4",
        );
        // 审查修复（模型冷却归因）：错误现携带目标池，顺带断言归因池为 Buddy
        assert!(matches!(
            f.resolve("hy4").unwrap_err(),
            DispatchError::ModelCooling(_, TargetPool::Buddy)
        ));
    }

    /// 双源 + Buddy 模型级冷却 → 跨池回退 Trae（reason=model_cooldown）
    #[test]
    fn t14_dual_model_cooldown_falls_back_trae() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(true);
        f.seed_healthy(false);
        super::super::wb_route::note_model_failure(
            &super::super::wb_route::WbView::wb(f.state.clone()),
            "glm-5.3",
        );
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
            DispatchError::NoHealthyWithOrder(TargetPool::Trae, POOL_FIXED_ORDER.to_vec())
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
            // 审查修复：错误携带实际候选序。glm-5.3 双源（≥2）且默认 Smart 策略
            // → smart 重排后的实际候选序非 POOL_FIXED_ORDER；Qoder 无源
            // unavailable 跳过，Buddy 有源但池空且 fallback 关 → 在 Buddy 提前
            // 终止（健康的 Trae 不被越过，正是本测试要锁的语义）
            DispatchError::NoHealthyWithOrder(
                TargetPool::Buddy,
                vec![
                    TargetPool::Qoder,
                    TargetPool::Buddy,
                    TargetPool::BuddyAi,
                    TargetPool::Trae,
                    TargetPool::Custom,
                ]
            )
        );
    }

    /// 审查修复回归：池耗尽提前终止同样携带实际候选序（NoHealthyWithOrder）——
    /// per_model 自定义序 [buddy-ai, trae] 下：BuddyAi 源缺失跳过 → Trae 池空
    /// 但后继 Buddy 有源（can_fallback 成立）→ 回退 Buddy → Buddy 也空且后继
    /// [Qoder, Custom] 零可用源 → 在 Buddy 提前终止（非循环尾耗尽），错误须附
    /// 真实序。旧实现回退普通 NoHealthy → routes 层按 POOL_FIXED_ORDER 硬编码
    /// 序归因，真实序前置池的冷却会被漏注记（归因序与真实序错位）
    #[test]
    fn t16b_early_terminate_carries_real_order() {
        let mut p = policy_default();
        p.per_model.insert(
            "glm-5.3".into(),
            vec!["buddy-ai".into(), "trae".into()],
        );
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        assert_eq!(
            f.resolve("glm-5.3").unwrap_err(),
            DispatchError::NoHealthyWithOrder(
                // 提前终止发生在首个「有源但无后继可用源」的失败池 = Buddy
                TargetPool::Buddy,
                // per_model [buddy-ai, trae] + 尾部按 POOL_FIXED_ORDER 补全
                // （Buddy 不在 per_model 序中，补全时排在 Trae 之后）
                vec![
                    TargetPool::BuddyAi,
                    TargetPool::Trae,
                    TargetPool::Buddy,
                    TargetPool::Qoder,
                    TargetPool::Custom,
                ]
            )
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

    // ---------- 阶段 2 审查修复：Intl 专属模型源判定 ----------

    /// Intl 专属 id（仅 builtin_buddy_ai 目录，CN 目录无）：国际档开启时折入
    /// buddy 源后路由 buddy-ai 池；原实现 buddy_hit=None 落 Trae 透传语义被误
    /// 路由 Trae 上游。
    /// 同时固化：Buddy(CN) 池不得服务 Intl 专属模型（available 门控）；
    /// 国际档关闭时不折入、恢复 Trae 透传既有语义（审查修复 第1项）；
    /// CN 目录模型 WbDisabled 语义不变
    #[test]
    fn t_intl_only_model_routes_buddy_ai_pool() {
        use std::sync::atomic::Ordering;
        // Intl 专属模型 + buddy-ai 开启且池健康 → BuddyAi 池
        let f = fixture(&["unrelated-trae"], Some(&["hy4"]), Some(&policy_default()));
        f.state.wb_enabled.store(false, Ordering::Relaxed);
        f.state.trae_enabled.store(false, Ordering::Relaxed);
        f.state.buddy_ai_enabled.store(true, Ordering::Relaxed);
        f.state.buddy_ai_pool.sync_from_wb(
            &[super::super::pool::WbSyncAccount {
                uid: "g1".into(),
                name: "intl".into(),
                token: "tk".into(),
                domain: "www.workbuddy.ai".into(),
                enterprise_id: String::new(),
                global_region: true,
                credits: Some(10.0),
                credits_expire_at: None,
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["g1".to_string()],
        );
        let r = f.resolve("deepseek-v4.1-flash").expect("Intl 专属模型应可调度");
        assert_eq!(r.pool, TargetPool::BuddyAi);
        assert_eq!(r.model, "deepseek-v4.1-flash");

        // 审查修复（第1项）：国际档关闭时 Intl 专属 id 不折入 buddy 源 → 恢复
        // Trae 透传既有语义（原实现无条件折入 → 四源全不可用 503 NoHealthy(Trae)，
        // 被误判为「Trae 池耗尽」）
        let f2 = fixture(&["unrelated-trae"], Some(&["hy4"]), Some(&policy_default()));
        f2.state.wb_enabled.store(false, Ordering::Relaxed);
        f2.seed_healthy(true);
        let r2 = f2
            .resolve("deepseek-v4.1-flash")
            .expect("国际档关闭时 Intl 专属 id 应恢复 Trae 透传");
        assert_eq!(r2.pool, TargetPool::Trae);
        assert_eq!(r2.model, "deepseek-v4.1-flash");

        // CN 目录模型不受 Intl 折入影响：wb 关闭 → WbDisabled 语义保持
        //（hy4 仅 CN 目录，不在 Intl 17 表/Qoder 目录）
        let f3 = fixture(&["unrelated-trae"], Some(&["hy4"]), Some(&policy_default()));
        f3.state.wb_enabled.store(false, Ordering::Relaxed);
        f3.state.trae_enabled.store(false, Ordering::Relaxed);
        f3.state.buddy_ai_enabled.store(false, Ordering::Relaxed);
        match f3.resolve("hy4") {
            Err(DispatchError::WbDisabled) => {}
            other => panic!("CN 模型 wb 关闭应报 WbDisabled，实际 {other:?}"),
        }
    }

    /// 审查修复（第2项）：国际档开启时 CN 专属模型不静默路由国际池——
    /// available/available_count 门控（intl_only ‖ in_both）：国际池积分远高于
    /// CN 池，修复前 Smart 重排会把 hy4 切到 BuddyAi；修复后 BuddyAi 不计入
    /// 可用源（count=1 不触发重排）→ Buddy 胜出。
    /// wb 关闭时 CN 专属模型仍报 WbDisabled（第3项：不因国际档开启被静默绕过）
    #[test]
    fn t_intl_on_cn_only_model_not_routed_buddy_ai() {
        use std::sync::atomic::Ordering;
        let f = fixture(&["unrelated-trae"], Some(&["hy4"]), Some(&policy_default()));
        f.state.buddy_ai_enabled.store(true, Ordering::Relaxed);
        f.seed_healthy(true);
        f.seed_healthy(false);
        f.state.buddy_ai_pool.sync_from_wb(
            &[super::super::pool::WbSyncAccount {
                uid: "g1".into(),
                name: "intl".into(),
                token: "tk".into(),
                domain: "www.workbuddy.ai".into(),
                enterprise_id: String::new(),
                global_region: true,
                credits: Some(100.0),
                credits_expire_at: None,
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["g1".to_string()],
        );
        let r = f.resolve_no_session("hy4").expect("CN 专属模型应可调度");
        assert_eq!(r.pool, TargetPool::Buddy);
        // wb 关闭 + 国际档开启：CN 专属模型仍显式报 WbDisabled（不静默走国际池）
        f.state.wb_enabled.store(false, Ordering::Relaxed);
        assert_eq!(
            f.resolve_no_session("hy4").unwrap_err(),
            DispatchError::WbDisabled
        );
    }

    /// 审查修复（第2项）：双收录模型（CN+Intl 同收录）国际档开启时可走国际池
    #[test]
    fn t_in_both_model_routes_buddy_ai_when_intl_on() {
        use std::sync::atomic::Ordering;
        // glm-5.3 同时收录于 CN 与 Intl 目录；wb 关 + trae 关 + 国际档开
        let f = fixture(&["unrelated-trae"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.state.wb_enabled.store(false, Ordering::Relaxed);
        f.state.trae_enabled.store(false, Ordering::Relaxed);
        f.state.buddy_ai_enabled.store(true, Ordering::Relaxed);
        f.state.buddy_ai_pool.sync_from_wb(
            &[super::super::pool::WbSyncAccount {
                uid: "g1".into(),
                name: "intl".into(),
                token: "tk".into(),
                domain: "www.workbuddy.ai".into(),
                enterprise_id: String::new(),
                global_region: true,
                credits: Some(10.0),
                credits_expire_at: None,
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["g1".to_string()],
        );
        let r = f.resolve("glm-5.3").expect("双收录模型国际档开启应可走国际池");
        assert_eq!(r.pool, TargetPool::BuddyAi);
        assert_eq!(r.model, "glm-5.3");
    }

    // ---------- 策略加载与合法性 ----------

    /// 策略文件缺失 → 默认 ["buddy","trae","qoder"]；非法池名被过滤
    #[test]
    fn t21_policy_missing_and_invalid_values() {
        let dir = std::env::temp_dir().join(format!("twa_policy_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        // 缺失
        let p = load_policy(&dir);
        assert_eq!(
            p.priority,
            vec!["buddy".to_string(), "trae".to_string(), "qoder".to_string()]
        );
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

    // ---------- 后缀剥离跨池（issue #31 T3.1） ----------

    /// Trae-only 模型带 -thinking 后缀 → 剥离后以基名路由，effort 提示随池携带
    /// （Buddy 管线 ④ 段因基名不在 Buddy 目录未命中，全名透传会报模型不存在）
    #[test]
    fn t42_trae_only_suffix_stripped_with_effort_hint() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        // 目录排除 glm-5.3 → "glm-5.3-thinking" 的基名仅在 Trae 源（T3.1 触发前提）
        catalog_only(&f.dir, "hy4");
        f.seed_healthy(true);
        let r = f.resolve("glm-5.3-thinking").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "glm-5.3");
        assert_eq!(r.effort_hint.as_deref(), Some("high"));
    }

    /// 无后缀 Trae-only 模型保持原名透传 + 无 effort 提示（零行为差异）
    #[test]
    fn t43_trae_only_no_suffix_unchanged() {
        let f = fixture(&["Kimi-K3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        let r = f.resolve("Kimi-K3").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "Kimi-K3");
        assert!(r.effort_hint.is_none());
    }

    /// 未知模型带后缀但基名不在 Trae 模型列表 → 原名透传（不剥离，透传语义不变）
    #[test]
    fn t44_unknown_model_suffix_passthrough() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        let r = f.resolve("ghost-model-thinking").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "ghost-model-thinking");
        assert!(r.effort_hint.is_none());
    }

    /// 自定义后缀跨池剥离（effort 值取条目配置；基名命中 Trae 源即生效）
    #[test]
    fn t45_custom_suffix_stripped_cross_pool() {
        let f = fixture(&["kimi-k3"], Some(&["hy4"]), Some(&policy_default()));
        // 目录排除 kimi-k3 → "kimi-k3-deep" 的基名仅在 Trae 源（T3.1 触发前提）
        catalog_only(&f.dir, "hy4");
        crate::store::db(&f.dir)
            .kv_set(
                "wb_model_route",
                &json!({"suffixes": [{"suffix": "-deep", "effort": "xhigh"}]}),
            )
            .unwrap();
        f.seed_healthy(true);
        let r = f.resolve("kimi-k3-deep").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "kimi-k3");
        assert_eq!(r.effort_hint.as_deref(), Some("xhigh"));
    }

    // ---------- Max Mode 入口后缀（issue #31 T4.2） ----------

    /// Trae-only 模型带 -max 入口后缀 → 剥离为基名 + max 入口标志置位（无 effort 提示）
    #[test]
    fn t46_trae_only_max_suffix_stripped() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        // 目录排除 glm-5.3 → "glm-5.3-max" 的基名仅在 Trae 源
        catalog_only(&f.dir, "hy4");
        f.seed_healthy(true);
        let r = f.resolve("glm-5.3-max").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "glm-5.3");
        assert!(r.max_mode_hint);
        assert!(r.effort_hint.is_none());
    }

    /// 整名以 -max 结尾的真模型（qwen3.8-max）整名在列表 → 原名透传不误剥、
    /// 不置 max 入口标志（整名优先守卫）
    #[test]
    fn t47_real_name_max_suffix_not_stripped() {
        let f = fixture(&["qwen3.8-max"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        let r = f.resolve("qwen3.8-max").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "qwen3.8-max");
        assert!(!r.max_mode_hint);
    }

    /// -max 基名不在 Trae 模型列表 → 原名透传（透传语义不变、不置标志）
    #[test]
    fn t48_unknown_model_max_suffix_passthrough() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.seed_healthy(true);
        let r = f.resolve("ghost-model-max").unwrap();
        assert_eq!(r.pool, TargetPool::Trae);
        assert_eq!(r.model, "ghost-model-max");
        assert!(!r.max_mode_hint);
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
        assert!(f.app_log_contains("dispatch fallback: model=glm-5.3 pool=buddy preferred=trae reason=no_healthy_account"));

        // fallback 关：绑定池不健康 → 显式报错（不回退）
        let mut p = policy_default();
        p.fallback = false;
        let f2 = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&p));
        f2.seed_healthy(false);
        f2.seed_key("k1", "trae", &[]);
        assert_eq!(
            f2.resolve_key("glm-5.3", "k1").unwrap_err(),
            DispatchError::NoHealthyWithOrder(
                TargetPool::Trae,
                vec![
                    TargetPool::Trae,
                    TargetPool::Buddy,
                    TargetPool::Qoder,
                    TargetPool::BuddyAi,
                    TargetPool::Custom,
                ]
            )
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
            DispatchError::NoHealthyWithOrder(TargetPool::Buddy, POOL_FIXED_ORDER.to_vec())
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

    // ---------- issue #29 调度失败可观测性 ----------

    /// issue #159：NoHealthy 归因「前置候选池因模型级冷却被跳过」——buddy 池
    /// 已启用且同名模型在冷却表内时，503 消息显式携带 buddy skipped 注记
    #[test]
    fn t39b_no_healthy_detail_attributes_model_cooldown_skip() {
        let f = fixture(&[], None, None);
        let view = super::super::wb_route::WbView::wb(f.state.clone());
        // 渐进退避路径：无消息 → 模型冷却 10s（足以触发归因注记）
        super::super::wb_route::note_model_failure_ex(&view, "hy4", None);
        let msg = super::super::routes::no_healthy_detail_with_model(
            &f.state,
            TargetPool::Trae,
            "anonymous",
            Some("hy4"),
            // None = 无实际候选序上下文，注记回退 POOL_FIXED_ORDER（Buddy 在 Trae 前）
            None,
        );
        assert!(
            msg.contains("buddy skipped: model_cooldown remaining="),
            "{msg}"
        );
        // 无模型上下文（旧包装口径）不追加归因
        let plain =
            super::super::routes::no_healthy_detail(&f.state, TargetPool::Trae, "anonymous");
        assert!(!plain.contains("skipped:"), "{plain}");
    }

    /// NoHealthy 详情（修复2）：匿名/未知 Key 仅报池健康数，不编造约束细节
    #[test]
    fn t39_no_healthy_detail_anonymous_pool_only() {
        let f = fixture(&[], None, None);
        let msg = super::super::routes::no_healthy_detail(&f.state, TargetPool::Trae, "anonymous");
        assert!(msg.starts_with("no healthy account available"), "{msg}");
        assert!(msg.contains("pool=trae healthy=0"), "{msg}");
        assert!(!msg.contains("key="), "匿名 Key 不应携带约束细节: {msg}");
    }

    /// NoHealthy 详情（修复2）：Key 约束细节（绑定池/白名单条数/作用域健康数）——
    /// 白名单指向池外账号时 healthy_in_scope=0 直接暴露「约束排除致败」根因
    #[test]
    fn t40_no_healthy_detail_key_constraints() {
        let f = fixture(&[], None, None);
        f.seed_healthy(true); // trae 池健康账号 t1
        // 先种满全部 Key 再断言：constraints_for 首读后走内存缓存，后续直写不可见
        f.seed_key("k1", "trae", &["t9"]); // 白名单指向池外账号
        f.seed_key("k2", "trae", &["t1"]); // 白名单命中池内健康账号

        let msg = super::super::routes::no_healthy_detail(&f.state, TargetPool::Trae, "k1");
        assert!(msg.contains("pool=trae healthy=1"), "{msg}");
        assert!(msg.contains("key=k1"), "{msg}");
        assert!(msg.contains("bind=trae"), "{msg}");
        assert!(msg.contains("whitelist=1"), "{msg}");
        assert!(msg.contains("healthy_in_scope=0"), "{msg}");
        // issue #130 指引：约束排除全部健康账号（池内尚有 1 个）→ 显式 hint
        assert!(msg.contains("hint=约束排除了池内全部健康账号"), "{msg}");
        assert!(msg.contains("池内尚有 1 个"), "{msg}");

        let msg2 = super::super::routes::no_healthy_detail(&f.state, TargetPool::Trae, "k2");
        assert!(msg2.contains("healthy_in_scope=1"), "{msg2}");
        // 作用域内有健康账号 → 不追加 hint（避免噪音）
        assert!(!msg2.contains("hint="), "{msg2}");
    }

    /// issue #136 hint 按根因分流（Case A）：混合白名单条目全指向其他池（buddy-ai）
    /// → trae 池被整体排除（whitelist=0），hint 列出约束涉及池并点明「本次请求由
    /// 本池提供」；白名单含本池条目但均不可用的 Case B 文案由 t40 覆盖（前缀不变）
    #[test]
    fn t40b_no_healthy_detail_hint_pool_mismatch() {
        let f = fixture(&[], None, None);
        f.seed_healthy(true); // trae 池健康账号 t1（healthy=1，在作用域外）
        // 先种满全部 Key 再断言：constraints_for 首读后走内存缓存，后续直写不可见
        f.seed_key("k1", "", &["buddy-ai:u1", "buddy-ai:u2"]); // 白名单仅国际版账号
        // 专一锁他池变体：白名单指向 buddy-ai + 专一指向 qoder → trae 整体排除
        // （dedicated 不随行，且 dedicated 指向池对约束涉及池列表有可见增量 qoder）
        f.seed_key_cfg("k2", "", &["buddy-ai:u1"], "dedicated", "qoder:u9");

        let msg = super::super::routes::no_healthy_detail(&f.state, TargetPool::Trae, "k1");
        assert!(msg.contains("pool=trae healthy=1"), "{msg}");
        assert!(
            msg.contains("whitelist=0"),
            "白名单条目全指向其他池 → trae 作用域空集: {msg}"
        );
        assert!(
            msg.contains("hint=该 Key 的账号约束均指向其他资源池（buddy-ai）"),
            "{msg}"
        );
        assert!(msg.contains("未包含 trae 池账号"), "{msg}");
        assert!(msg.contains("由 trae 池提供"), "{msg}");
        // Case A 不得误用 Case B 文案（该文案保留给白名单含本池条目的场景）
        assert!(!msg.contains("约束排除了池内全部健康账号"), "{msg}");

        let msg2 = super::super::routes::no_healthy_detail(&f.state, TargetPool::Trae, "k2");
        assert!(msg2.contains("whitelist=0"), "{msg2}");
        assert!(msg2.contains("dedicated=-"), "非归属池排除分支 dedicated 不随行: {msg2}");
        assert!(
            msg2.contains("hint=该 Key 的账号约束均指向其他资源池（buddy-ai/qoder）"),
            "dedicated 指向池应并入约束涉及池列表: {msg2}"
        );
    }

    /// issue #136 修复2：候选池耗尽/提前终止落逐池归因日志——「白名单把池排除」
    /// （trae=no_healthy_account）与「源剔除」（buddy=unavailable）一行可辨；
    /// 实证路径是循环内提前 return（单可用池失败即终止），非循环末尾 exhausted 块
    #[test]
    fn t40c_dispatch_exhausted_logs_per_pool_reasons() {
        let f = fixture(&["glm-5.3"], None, None);
        f.seed_healthy(true); // trae 池健康账号 t1（会被 Key 白名单排除）
        f.seed_key("k1", "", &["buddy-ai:u1"]); // 混合白名单仅国际版账号

        // buddy 无源目录走内置表 → 源存在但 wb_pool 无账号 → no_healthy_account；
        // trae 有源但 Key 作用域空集 → no_healthy_account；无源池 → unavailable
        // → 无更多回退目标，提前终止归因 trae
        let r = f.resolve_key("glm-5.3", "k1");
        assert!(
            matches!(r, Err(DispatchError::NoHealthyWithOrder(TargetPool::Trae, _))),
            "{r:?}"
        );
        assert!(
            f.app_log_contains("dispatch exhausted: model=glm-5.3 key=k1"),
            "应落逐池归因日志"
        );
        assert!(
            f.app_log_contains("trae=no_healthy_account"),
            "被 Key 白名单排除的池应记 no_healthy_account"
        );
        assert!(
            f.app_log_contains("buddy=no_healthy_account"),
            "目录命中但池内无账号同样记 no_healthy_account"
        );
        assert!(f.app_log_contains("qoder=unavailable"), "无源池应记 unavailable");
    }

    /// 修复1 直测：调度阶段失败落 API 请求日志——NoHealthy → status=503、
    /// uid="-"（未取号）、acct="-"、error 携带 no healthy 详情与约束根因
    #[test]
    fn t41_dispatch_error_logs_request() {
        let f = fixture(&[], None, None);
        f.seed_healthy(true); // trae 池健康账号 t1
        // 先种满全部 Key 再断言：constraints_for 首读后走内存缓存，后续直写不可见
        f.seed_key("k1", "trae", &["t9"]); // 白名单指向池外 → healthy_in_scope=0

        let resp = super::super::routes::dispatch_error_response(
            &f.state,
            DispatchError::NoHealthy(TargetPool::Trae),
            super::super::routes::Protocol::OpenAi,
            "glm-5.3",
            false,
            "k1",
            12,
        );
        assert_eq!(resp.status().as_u16(), 503);

        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let content = f
            .state
            .logger
            .read_log(&date)
            .expect("调度失败应落 API 请求日志");
        let line = content
            .lines()
            .find(|l| l.contains("pool=trae") && l.contains("model=glm-5.3"))
            .expect("应存在 pool=trae model=glm-5.3 日志行");
        assert!(line.contains("status=503"), "{line}");
        assert!(line.contains("key=k1"), "{line}");
        assert!(line.contains("uid=- "), "调度阶段未取号，uid 应为占位符: {line}");
        assert!(line.contains(" acct=- "), "调度阶段无账号，acct 应为占位符: {line}");
        assert!(line.contains("error=no healthy account available"), "{line}");
        assert!(line.contains("healthy_in_scope=0"), "{line}");
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

    /// 审查 #19 Case A 回归锁定（分域取候选方案）：国际档开启 + 双收录模型
    /// （glm-5.3 ∈ CN∩Intl）时，后台降级候选按目标池自身域目录取——
    /// Buddy(CN) 池取 CN 目录最低价（hy4-preview 0.00），BuddyAi 池取 Intl
    /// 目录候选（静态表倍率全为占位 1.0，并列取首条 hy4-preview-f），
    /// 从构造上保证「发往 buddy-ai 池的降级 id 必在 Intl 目录存在」
    ///（main 侧「CN∩Intl 交集过滤」方案同修 #19，本方案覆盖更优，断言随本方案调整）。
    /// 与 t38（国际档关闭不过滤）构成双向口径。
    #[test]
    fn t38a_bg_downgrade_intl_filter_requires_intl_enabled() {
        use std::sync::atomic::Ordering;
        let bg = || {
            json!({"model": "glm-5.3", "max_tokens": 16,
                   "messages": [{"role": "user", "content": "生成标题"}]})
        };

        // ① Buddy(CN) 池：降级候选取 CN 域目录最低价
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f.seed_healthy(false);
        f.state.buddy_ai_enabled.store(true, Ordering::Relaxed);
        f.state.wb_bg_downgrade.store(true, Ordering::Relaxed);
        super::super::unified_catalog::save_whitelist(&f.dir, &[]).unwrap();
        let r = resolve_target(&f.state, "glm-5.3", &bg(), None).unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert_eq!(r.model, "hy4-preview", "Buddy 池降级候选出自 CN 域目录最低价");

        // ② BuddyAi 池：候选出自 Intl 域目录（CN 专属 id 不会发往国际上游）
        let f2 = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        f2.state.wb_enabled.store(false, Ordering::Relaxed);
        f2.state.trae_enabled.store(false, Ordering::Relaxed);
        f2.state.buddy_ai_enabled.store(true, Ordering::Relaxed);
        f2.state.wb_bg_downgrade.store(true, Ordering::Relaxed);
        f2.state.buddy_ai_pool.sync_from_wb(
            &[super::super::pool::WbSyncAccount {
                uid: "g1".into(),
                name: "intl".into(),
                token: "tk".into(),
                domain: "www.workbuddy.ai".into(),
                enterprise_id: String::new(),
                global_region: true,
                credits: Some(10.0),
                credits_expire_at: None,
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["g1".to_string()],
        );
        super::super::unified_catalog::save_whitelist(&f2.dir, &[]).unwrap();
        let r2 = resolve_target(&f2.state, "glm-5.3", &bg(), None).unwrap();
        assert_eq!(r2.pool, TargetPool::BuddyAi);
        assert_eq!(
            r2.model, "hy4-preview-f",
            "BuddyAi 池降级候选出自 Intl 域目录（CN 目录最低价 hy4-preview 非 Intl 收录，不会被选用）"
        );
    }

    // ==================== Qoder 源参与调度（p3-3） ====================

    /// 启用 Qoder 并种一个健康账号
    fn seed_qoder(f: &Fixture) {
        f.state
            .qoder_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        f.state.qoder_pool.sync_from_qoder(
            &[super::super::pool::QoderSyncAccount {
                uid: "q1".into(),
                name: "qacc".into(),
                access_token: "tk".into(),
                machine_id: String::new(),
                needs_relogin: false,
                credits: None,
                credits_expire_at: None,
                credits_fetched_at: None,
            }],
            &["q1".to_string()],
        );
    }

    /// 仅 Qoder 源模型（MiniMax-M2.7 为 Qoder 目录独有；Sonus 已按产品决策
    /// 下线，见 qoder_upstream::REMOVED_MODEL_IDS）+ 池健康 → Qoder 接管
    #[test]
    fn t40_qoder_only_model_goes_qoder() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        seed_qoder(&f);
        let r = f.resolve("MiniMax-M2.7").unwrap();
        assert_eq!(r.pool, TargetPool::Qoder);
        assert_eq!(r.model, "MiniMax-M2.7", "final model = 目录归一化 id");
        assert!(r.fallback_from.is_none());
    }

    /// 仅 Qoder 源 + qoder_enabled=false → 显式报错（WbDisabled 镜像语义）
    #[test]
    fn t41_qoder_only_disabled_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        assert_eq!(
            f.resolve("MiniMax-M2.7").unwrap_err(),
            DispatchError::QoderDisabled
        );
    }

    /// 仅 Qoder 源 + 池耗尽 → 503 语义 NoHealthyWithOrder(Qoder)，不回退 Buddy/Trae
    #[test]
    fn t42_qoder_only_exhausted_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.state
            .qoder_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 池空 = 无健康账号（单源不跨池回退）
        assert_eq!(
            f.resolve("MiniMax-M2.7").unwrap_err(),
            DispatchError::NoHealthyWithOrder(TargetPool::Qoder, POOL_FIXED_ORDER.to_vec())
        );
    }

    /// buddy+qoder 双源模型（不在 Trae 列表）：wb_enabled=false 且 qoder_enabled=true
    /// → 不误报 WbDisabled，路由 Qoder（三源早退对称性修复，审查 p3-3）
    #[test]
    fn t43_buddy_qoder_dual_source_wb_off_routes_qoder() {
        // DeepSeek-V4-Pro 同在 WB 目录与 Qoder CN 兜底表、不在 Trae 列表
        let f = fixture(
            &["glm-5.3"],
            Some(&["DeepSeek-V4-Pro"]),
            Some(&policy_default()),
        );
        seed_qoder(&f);
        f.state
            .wb_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let r = f.resolve("DeepSeek-V4-Pro").unwrap();
        assert_eq!(r.pool, TargetPool::Qoder);
        assert_eq!(r.model, "DeepSeek-V4-Pro");
    }

    /// buddy+qoder 双源模型：wb 与 qoder 双双关闭 → 显式报错不静默（WbDisabled
    /// 先命中：默认优先级 Buddy 居首，关闭侧语义以 Buddy 为准）
    #[test]
    fn t44_buddy_qoder_dual_source_both_off_errors() {
        let f = fixture(
            &["glm-5.3"],
            Some(&["DeepSeek-V4-Pro"]),
            Some(&policy_default()),
        );
        f.state
            .wb_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            f.resolve("DeepSeek-V4-Pro").unwrap_err(),
            DispatchError::WbDisabled
        );
    }

    /// 仅 Trae 源模型 + trae_enabled=false → 显式报错（同族镜像；默认开不受影响）。
    /// ghost-model = 未知模型透传形态（单源 Trae：WB 路由未命中 + Qoder 目录未收录，
    /// t44 已固化其 buddy/qoder 双无源语义）
    #[test]
    fn t45_trae_only_disabled_errors() {
        let f = fixture(&["glm-5.3"], Some(&["hy4"]), Some(&policy_default()));
        f.state
            .trae_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            f.resolve("ghost-model").unwrap_err(),
            DispatchError::TraeDisabled
        );
    }

    /// trae+buddy 双源模型 + trae_enabled=false + wb 开 → Buddy 接管不报错
    /// （开关关闭 = 源剔除语义，与 wb_enabled 关闭同族）
    #[test]
    fn t46_trae_disabled_falls_back_to_buddy() {
        let f = fixture(&["glm-5.3"], Some(&["glm-5.3"]), Some(&policy_default()));
        // 健康账号两侧各一：Trae 池健康但开关关，Buddy 池健康接管
        f.seed_healthy(true);
        f.seed_healthy(false);
        f.state
            .trae_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let r = f.resolve("glm-5.3").unwrap();
        assert_eq!(r.pool, TargetPool::Buddy);
        assert!(r.fallback_from.is_none(), "源剔除不算回退");
    }
}
