//! 统一模型目录（unified-api-gateway-design §3）
//!
//! 派生聚合视图：不落盘第三份目录，实时合并两个源——
//! - 源1 `data/api_models.json`（Trae 官网同步 + 内置补位，models_sync）
//! - 源2 `data/wb_model_catalog.json`（Buddy 目录，wb_catalog；对外标识 buddy）
//!
//! Trae 侧元数据四层来源（§3.2，逐级兜底）：
//! - L1 人工维护：`data/trae_model_meta.json` 覆盖层（键 canonical_id），官网同步永不覆盖
//! - L2 官网同步：api_models.json 条目扩展字段（ModelOption serde default，宽容解析；
//!   context 双口径独立记录——context_length=dev 实际请求槽、context_length_max=max 声明槽）
//! - L3 文档参考值：内置表（主条目诚实兜底 128K、倍率初始参考、
//!   思考档位实证表 efforts::TRAE_EFFORTS_REF、Max Mode 支持表
//!   efforts::TRAE_MAX_MODE_REF——1M 仅 is_max_mode 请求级字段可达，
//!   T0.3 实证后无静态 -max 条目形态，不做 1M 静态声明）
//! - L4 名称推断：仅图片支持（Code/Flash 不支持、Seed/数字+V 支持）；思考档位
//!   已停用名称推断（issue #31：推断值与真实档位不符），无实证 → 空数组
//!
//! 归并键 `canonical_id()` 三处统一：目录归并 / dispatch_policy.per_model /
//! trae_model_meta 覆盖层（§3.3 #1），防止规则漂移。
//!
//! 档位口径（issue #31，统一空间见 efforts 模块）：Trae 侧 L1/L2/L3 值一律
//! 归一到统一档位（wire light/high/extra_high → low/high/xhigh）；双源条目
//! 顶层 efforts = 各池映射后取并集，context_length 按调度命中侧选定
//! （issue #38-4：WB 池未启用时残留快照不得拖低声明）。

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::dispatch::TargetPool;
use super::models_sync::{self, ModelOption};
use super::wb_catalog::{self, WbModel};

/// 归并键：trim + 小写（与 wb_catalog::find 语义一致，§3.3 #1）
pub fn canonical_id(id: &str) -> String {
    id.trim().to_lowercase()
}

// ==================== L3 文档参考值（随版本更新维护，§3.2） ====================

/// L3：主条目诚实兜底 128K（与旧 /v1/models 默认一致）——L2 dev 口径缺失时的
/// 保守声明，不冒充实际窗口。1M 上下文档仅 Max Mode 通道（efforts::TRAE_MAX_MODE_REF
/// 门控 is_max_mode:1 请求级注入，T0.3 实证无静态 -max 模型条目形态）可达，故主条目
/// 不做 1M 静态声明；原 MAX_MODE_1M/CTX_1M 占位随 T4.1 迁入 efforts::TRAE_MAX_MODE_REF
/// （单一事实源）
const CTX_128K: u64 = 131_072;

/// 倍率初始参考（客户端下拉实测，随官网同步覆盖，§3.2）。
/// 审查补充（2026-09-13）：本机 api_models.json 官网同步从未带出 rate（L2 恒 None），
/// 下列 5 个模型三层兜底全空 → 帮助列表倍率恒"—"。按官方上线公告/社区实测帖补齐：
/// - qwen3.8-max 1.50x：forum.trae.cn/t/topic/175814（官方上线公告 2026-08-13）
/// - qwen3.8-flash 0.08x：forum.trae.cn/t/topic/178258（官方上线公告 2026-08-27）
/// - kimi-k2.6 0.69 / minimax-m3 0.26 / qwen-3.7-plus 0.25：
///   forum.trae.cn/t/topic/175456（社区实测帖 2026-08-11）
/// 注意：该帖与其余 RATE_REF 值存在口径差异（如 glm-5.2 0.40 vs 0.78），为不覆盖
/// 原有"客户端下拉实测"口径，仅补缺失条目、不改既有值。
const RATE_REF: [(&str, f64); 18] = [
    ("doubao-seed-evolving", 0.08),
    ("doubao-seed-2.1-pro", 0.08),
    ("doubao-seed-2.1-turbo", 0.20),
    ("doubao-seed-code", 0.06),
    ("glm-5.3-flash", 0.06),
    ("glm-5.3", 0.78),
    ("glm-5.2", 0.78),
    ("deepseek-v4-flash", 0.16),
    ("deepseek-v4-flash-official", 0.16),
    ("deepseek-v4-pro", 0.72),
    ("deepseek-v4-pro-official", 0.72),
    ("kimi-k3", 1.83),
    ("kimi-k2.7-code", 0.83),
    ("kimi-k2.6", 0.69),
    ("minimax-m3", 0.26),
    ("qwen-3.7-plus", 0.25),
    ("qwen3.8-flash", 0.08),
    ("qwen3.8-max", 1.50),
];

fn doc_rate(canonical: &str) -> Option<f64> {
    RATE_REF
        .iter()
        .find(|(id, _)| *id == canonical)
        .map(|(_, r)| *r)
}

// ==================== L4 名称推断（可被 L1–L3 覆盖，§3.2） ====================

/// 档位值归一到统一空间（issue #31）：Trae wire 值（light/high/extra_high）映射
/// 统一档位；已是统一档位的原样保留；未知值丢弃（声明口径不含猜测值）。
/// 去空 + 去重 + 按统一档位序升序。
fn normalize_unified_efforts(list: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for e in list {
        let e = e.trim().to_lowercase();
        if e.is_empty() {
            continue;
        }
        let unified = super::efforts::trae_to_unified(&e)
            .map(str::to_string)
            .or_else(|| super::efforts::unified_rank(&e).map(|_| e.clone()));
        if let Some(u) = unified {
            if seen.insert(u.clone()) {
                out.push(u);
            }
        }
    }
    out.sort_by_key(|e| super::efforts::unified_rank(e).unwrap_or(usize::MAX));
    out
}

/// 图片支持推断：Code / Flash 系列明确不支持（优先判定，如 Seed-Code）；
/// Seed 系列（豆包多模态）与名称含「数字+V」（如 GLM-5V-Turbo）支持；未知 → None（显示 —）
fn infer_supports_image(canonical: &str) -> Option<bool> {
    if canonical.contains("code") || canonical.contains("flash") {
        return Some(false);
    }
    if canonical.contains("seed") {
        return Some(true);
    }
    let has_vision_v = canonical
        .as_bytes()
        .windows(2)
        .any(|w| w[0].is_ascii_digit() && w[1] == b'v');
    if has_vision_v {
        return Some(true);
    }
    None
}

/// 供应商系列推断（按 canonical 前缀；未知 → 空串，前端显示 —）
fn vendor_of(canonical: &str) -> &'static str {
    for (prefix, vendor) in [
        ("glm-", "智谱"),
        ("deepseek", "DeepSeek"),
        ("kimi", "Moonshot"),
        ("doubao", "字节·豆包"),
        ("hy3", "腾讯·混元"),
        ("hy4", "腾讯·混元"),
        ("qwen", "阿里·通义"),
        ("minimax", "MiniMax"),
        ("claude", "Anthropic"),
        ("gemini", "Google"),
        ("grok", "xAI"),
        ("gpt", "OpenAI"),
        ("o1", "OpenAI"),
        ("o3", "OpenAI"),
    ] {
        if canonical.starts_with(prefix) {
            return vendor;
        }
    }
    ""
}

// ==================== L1 人工维护覆盖层（trae_model_meta.json，§6.1） ====================

/// L1 覆盖层条目（编辑弹框落盘；None = 该字段未人工指定，交由下层兜底）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraeModelMeta {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub rate: Option<f64>,
    #[serde(default)]
    pub efforts: Option<Vec<String>>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub supports_image: Option<bool>,
}

/// 读取 L1 覆盖层（键 canonical_id；缺失/损坏回退空表）。
/// SQLite 化（P2）：data/trae_model_meta.json → kv `trae_model_meta`。
pub fn load_meta(data_dir: &Path) -> HashMap<String, TraeModelMeta> {
    crate::store::db(data_dir).kv_get("trae_model_meta")
}

/// L1 写入（upsert；编辑弹框整条覆盖，官网同步不触碰本文件）
pub fn meta_set(data_dir: &Path, model: &str, meta: TraeModelMeta) -> Result<(), String> {
    let id = canonical_id(model);
    if id.is_empty() {
        return Err("模型 ID 不能为空".into());
    }
    let mut map = load_meta(data_dir);
    map.insert(id, meta);
    crate::store::db(data_dir).kv_set("trae_model_meta", &map)
}

/// L1 清除（恢复自动来源链 L2→L3→L4）；返回是否确有删除
pub fn meta_clear(data_dir: &Path, model: &str) -> Result<bool, String> {
    let id = canonical_id(model);
    let mut map = load_meta(data_dir);
    let removed = map.remove(&id).is_some();
    if removed {
        crate::store::db(data_dir).kv_set("trae_model_meta", &map)?;
    }
    Ok(removed)
}

// ==================== 全局模型白名单（issue #26） ====================

/// 读取全局模型白名单（kv `model_whitelist`，Vec<canonical_id>；缺失/空 = 不限）。
/// 准入校验在每请求热路径上，经 config_cache 缓存；写路径显式失效
pub fn load_whitelist(data_dir: &Path) -> Vec<String> {
    super::config_cache::get_or_load(data_dir, "model_whitelist", || {
        crate::store::db(data_dir).kv_get("model_whitelist")
    })
}

/// 白名单准入判定：空名单 = 不限；请求模型名 canonical 化后精确匹配
pub fn whitelist_allows(list: &[String], model: &str) -> bool {
    list.is_empty() || list.iter().any(|w| *w == canonical_id(model))
}

/// 保存白名单（canonical 归一 + 去空 + 去重保序）；返回归一后的生效列表。
/// 目录未收录的条目允许保存（模型下架/目录同步前的历史勾选仍可表达）
pub fn save_whitelist(data_dir: &Path, models: &[String]) -> Result<Vec<String>, String> {
    let mut seen = std::collections::HashSet::new();
    let out: Vec<String> = models
        .iter()
        .map(|m| canonical_id(m))
        .filter(|c| !c.is_empty() && seen.insert(c.clone()))
        .collect();
    crate::store::db(data_dir).kv_set("model_whitelist", &out)?;
    super::config_cache::invalidate(data_dir, "model_whitelist");
    Ok(out)
}

/// 聚合目录 ∩ 白名单（GET /v1/models 对外目录用）；
/// 管理端 api_unified_models 不过滤（需全量 + 白名单状态展示）
pub fn unified_models_whitelisted(
    data_dir: &Path,
    wb_enabled: bool,
    trae_ok: bool,
    buddy_ok: bool,
) -> Vec<UnifiedModel> {
    let wl = load_whitelist(data_dir);
    unified_models(data_dir, wb_enabled, trae_ok, buddy_ok)
        .into_iter()
        .filter(|m| whitelist_allows(&wl, &m.id))
        .collect()
}

// ==================== 统一目录聚合（§3.1/§3.3） ====================

/// 单池来源（`enabled` 为运行时派生标记：wb_enabled / 账号池健康，不落盘 §3.3 #5）
#[derive(Debug, Clone, Serialize)]
pub struct UnifiedSource {
    pub pool: &'static str,
    pub rate: Option<f64>,
    pub enabled: bool,
}

/// 统一目录条目（api_unified_models 命令 / GET /v1/models 共用）
#[derive(Debug, Clone, Serialize)]
pub struct UnifiedModel {
    pub id: String,
    /// 展示名：L1 人工 label 绝对优先；否则双源按调度策略命中侧（trae 侧含 L2 label）
    pub display: String,
    /// 供应商：自定义模型用户填写值优先；否则按模型名系列推断（未知为空串，前端显示 —）
    pub vendor: String,
    /// 实际生效倍率 = 当前调度策略命中的来源侧（§3.1），非"最优值"
    pub rate: Option<f64>,
    /// 思考档位（双语义合并展示，§3.1 注：仅 Buddy 池作为请求参数下发）
    pub efforts: Vec<String>,
    /// Max Mode 支持（Trae 池 1M 上下文，efforts::TRAE_MAX_MODE_REF 查表）：
    /// 含 Trae 源的条目按表标记；Buddy/自定义单源条目恒 false（Max Mode 仅 Trae 管线注入）
    pub max_mode: bool,
    pub context_length: Option<u64>,
    pub max_tokens: Option<u64>,
    pub supports_image: Option<bool>,
    pub sources: Vec<UnifiedSource>,
    /// L1 人工维护标记（编辑标记展示用，§6.1）
    pub manual: bool,
}

/// Trae 单源条目（四层兜底解析结果）
struct TraeEntry {
    id: String,
    display: String,
    rate: Option<f64>,
    efforts: Vec<String>,
    context_length: Option<u64>,
    max_tokens: Option<u64>,
    supports_image: Option<bool>,
}

/// 四层链解析 Trae 条目：L1 覆盖层 → L2 条目扩展字段 → L3 文档表（无 L4 档位猜测）
fn trae_entry_of(m: &ModelOption, l1: Option<&TraeModelMeta>) -> TraeEntry {
    let canonical = canonical_id(&m.id);
    // rate: L1 → L2 → L3
    let rate = l1
        .and_then(|l| l.rate)
        .or(m.rate)
        .or_else(|| doc_rate(&canonical));
    // efforts: L1 → L2 → L3 实证表（TRAE_EFFORTS_REF）。
    // L4 名称推断已停用（issue #31：light/high/extra_high 与名称推断的
    // low/medium/high 不符），无实证 → 空数组；全链归一到统一档位空间。
    // 回退判断基于「归一后非空」而非原始值：L2 官网同步返回非空但全为未知
    // 档位字面量（脏数据）时不得遮蔽 L3 实证表（归一为空 = 视为未声明）
    let efforts = l1
        .and_then(|l| l.efforts.clone())
        .filter(|e| !e.is_empty())
        .map(normalize_unified_efforts)
        .filter(|e| !e.is_empty())
        .or_else(|| {
            let normalized = normalize_unified_efforts(m.efforts.clone());
            (!normalized.is_empty()).then_some(normalized)
        })
        .unwrap_or_else(|| super::efforts::trae_declared_unified(&canonical));
    // context: L1 → L2（models_sync 双口径的 dev 实际请求槽）→ L3 诚实兜底 128K。
    // issue #31：主条目不做 1M 静态声明——实际请求走 __dev 通道，1M 仅 Max Mode
    // 请求级字段（is_max_mode:1，efforts::TRAE_MAX_MODE_REF 门控）可达
    let context_length = Some(
        l1.and_then(|l| l.context_length)
            .or(m.context_length)
            .unwrap_or(CTX_128K),
    );
    // max_tokens: L1 → L2（ModelOption 无此字段）→ 无兜底（显示 —）
    let max_tokens = l1.and_then(|l| l.max_tokens);
    // image: L1 → L2 → L4
    let supports_image = l1
        .and_then(|l| l.supports_image)
        .or(m.supports_image)
        .or_else(|| infer_supports_image(&canonical));
    let display = l1
        .and_then(|l| l.label.clone())
        .unwrap_or_else(|| m.label.clone());
    TraeEntry {
        id: m.id.clone(),
        display,
        rate,
        efforts,
        context_length,
        max_tokens,
        supports_image,
    }
}

/// wb 目录倍率：原始值透传，0 = 免费声明（与调度层「wb 原始值 0 = 免费」同语义；
/// 此前 0 丢为 None 会导致 Buddy 免费模型被当未声明后置/退源）
fn wb_rate(m: &WbModel) -> Option<f64> {
    Some(m.rate)
}

/// 聚合统一目录（纯派生，实时计算）。
///
/// `wb_enabled`：Buddy 源总开关；`trae_ok` / `buddy_ok`：两池是否存在可选账号
/// （§3.3 #5 运行时派生，调用方按需取值——HTTP 端点用实时池，命令在服务未运行时放宽）。
pub fn unified_models(
    data_dir: &Path,
    wb_enabled: bool,
    trae_ok: bool,
    buddy_ok: bool,
) -> Vec<UnifiedModel> {
    let l1 = load_meta(data_dir);
    // 注（issue #38-4 排查决策）：Trae 侧快照（kv api_models）无条件参与聚合，
    // trae_ok=false 仅置 enabled 徽章、不过滤条目——Trae 池健康是瞬时状态，
    // 过滤会导致 /v1/models 随账号冷却抖动；池不可用的信号由 sources[].enabled
    // 传递（与 buddy-only 过滤依赖稳定的 wb_enabled 设置开关不同构）
    let trae_list = models_sync::load_models(data_dir);
    let wb_list = wb_catalog::load(data_dir);

    let enabled_of = |pool: TargetPool| match pool {
        TargetPool::Trae => trae_ok,
        TargetPool::Buddy => wb_enabled && buddy_ok,
        // 自定义模型可用性 = 条目 enabled（find_enabled 只回 enabled 条目）
        TargetPool::Custom => true,
    };

    let mut order: Vec<String> = Vec::new();
    let mut acc: HashMap<String, UnifiedModel> = HashMap::new();
    // 双源条目的 Buddy 侧候选值：顶层 display / supports_image 不再合并时无条件取 WB，
    // 而是由末段按调度策略命中侧选定（与 rate 同源，§3.1/§3.3 #2）
    let mut buddy_disp: HashMap<String, String> = HashMap::new();
    let mut buddy_img: HashMap<String, bool> = HashMap::new();
    // Buddy 侧上下文候选值：由末段按调度策略命中侧选定（issue #38-4，
    // WB 池未启用时残留快照数值不得拖低双源条目的上下文声明）
    let mut buddy_ctx: HashMap<String, u64> = HashMap::new();
    // 自定义模型供应商（canonical → 用户填写值；聚合末段优先于系列推断）
    let mut custom_vendor: HashMap<String, String> = HashMap::new();

    // 源1：Trae（先入者；同 canonical 的 wb 条目随后合并）
    for m in &trae_list {
        let canonical = canonical_id(&m.id);
        if canonical.is_empty() || acc.contains_key(&canonical) {
            continue;
        }
        order.push(canonical.clone());
        let t = trae_entry_of(m, l1.get(&canonical));
        acc.insert(
            canonical.clone(),
            UnifiedModel {
                id: t.id.clone(),
                display: t.display.clone(),
                vendor: String::new(),
                rate: t.rate,
                efforts: t.efforts.clone(),
                max_mode: super::efforts::trae_max_mode_supported(&canonical),
                context_length: t.context_length,
                max_tokens: t.max_tokens,
                supports_image: t.supports_image,
                sources: vec![UnifiedSource {
                    pool: "trae",
                    rate: t.rate,
                    enabled: enabled_of(TargetPool::Trae),
                }],
                manual: l1.contains_key(&canonical),
            },
        );
    }

    // 源2：Buddy（wb_catalog，对外标识 buddy）——双源合并取有值优先（WB 为主源）
    for m in &wb_list {
        let canonical = canonical_id(&m.id);
        if canonical.is_empty() {
            continue;
        }
        let wrate = wb_rate(m);
        let wctx = if m.context_length > 0 { Some(m.context_length) } else { None };
        let wmt = if m.max_tokens > 0 { Some(m.max_tokens) } else { None };
        match acc.get_mut(&canonical) {
            Some(u) => {
                u.sources.push(UnifiedSource {
                    pool: "buddy",
                    rate: wrate,
                    enabled: enabled_of(TargetPool::Buddy),
                });
                // display / supports_image 保留 Trae 侧现值，候选记入 buddy_disp/img，
                // 由末段按策略命中侧选定（L1 label 绝对优先，§3.2）
                if !m.display.is_empty() {
                    buddy_disp.insert(canonical.clone(), m.display.clone());
                }
                buddy_img.insert(canonical.clone(), m.supports_image);
                if let Some(r) = wrate {
                    u.rate = Some(r);
                }
                // issue #31：双源档位声明 = 各池映射统一空间后取并集
                //（原实现 WB 无条件覆盖 Trae 侧，丢掉 Trae 档位声明）
                if !m.supported_efforts.is_empty() {
                    u.efforts = super::efforts::declared_union(&[
                        std::mem::take(&mut u.efforts),
                        m.supported_efforts.clone(),
                    ]);
                }
                // issue #38-4：上下文候选记入 buddy_ctx，由末段按命中侧选定。
                // 原实现无条件取 min——WB 池未启用时磁盘/ kv 残留快照仍拖低声明
                //（glm-5.2 等被压到 128K）；禁用池不会被调度命中，其数值不应约束声明
                if let Some(c) = wctx {
                    buddy_ctx.insert(canonical.clone(), c);
                }
                if let Some(t) = wmt {
                    u.max_tokens = Some(t);
                }
            }
            None => {
                order.push(canonical.clone());
                acc.insert(
                    canonical.clone(),
                    UnifiedModel {
                        id: m.id.clone(),
                        display: if m.display.is_empty() {
                            m.id.clone()
                        } else {
                            m.display.clone()
                        },
                        vendor: String::new(),
                        rate: wrate,
                        efforts: m.supported_efforts.clone(),
                        max_mode: false,
                        context_length: wctx,
                        max_tokens: wmt,
                        supports_image: Some(m.supports_image),
                        sources: vec![UnifiedSource {
                            pool: "buddy",
                            rate: wrate,
                            enabled: enabled_of(TargetPool::Buddy),
                        }],
                        manual: false,
                    },
                );
            }
        }
    }

    // 源3：自定义模型（custom_models.json，对外标识 custom）——命中即直达的
    // 第三资源来源；同 canonical 与内置目录合并时仅追加来源标记（调度短路 custom）
    for m in super::custom_models::load(data_dir).iter() {
        let canonical = canonical_id(&m.name);
        if canonical.is_empty() {
            continue;
        }
        if !m.vendor.is_empty() {
            custom_vendor.insert(canonical.clone(), m.vendor.clone());
        }
        // 展示倍率：0 = 免费（有效语义，透传聚合层）——命中 custom 即按免费展示；
        // 聚合尾部 rate=命中侧倍率，disabled 的 custom 源不会被命中，不影响 Trae/Buddy 侧
        let crate_rate: Option<f64> = Some(m.rate);
        let cctx = if m.context_length > 0 { Some(m.context_length) } else { None };
        let cmt = if m.max_tokens > 0 { Some(m.max_tokens) } else { None };
        match acc.get_mut(&canonical) {
            Some(u) => {
                u.sources.push(UnifiedSource {
                    pool: "custom",
                    rate: crate_rate,
                    enabled: m.enabled,
                });
                if let Some(r) = crate_rate {
                    u.rate = Some(r);
                }
                if let Some(c) = cctx {
                    u.context_length = Some(c);
                }
                if let Some(t) = cmt {
                    u.max_tokens = Some(t);
                }
                // 审查修复：disabled 自定义条目不参与顶层 supports_image 覆盖
                // （停用模型的多模态声明不得改变聚合视图展示，仅 enabled 条目参与）
                if m.enabled {
                    u.supports_image = Some(m.supports_image);
                }
            }
            None => {
                order.push(canonical.clone());
                acc.insert(
                    canonical.clone(),
                    UnifiedModel {
                        id: m.name.clone(),
                        display: m.name.clone(),
                        vendor: m.vendor.clone(),
                        rate: crate_rate,
                        efforts: Vec::new(),
                        max_mode: false,
                        context_length: cctx,
                        max_tokens: cmt,
                        supports_image: Some(m.supports_image),
                        sources: vec![UnifiedSource {
                            pool: "custom",
                            rate: crate_rate,
                            enabled: m.enabled,
                        }],
                        manual: false,
                    },
                );
            }
        }
    }

    let mut out: Vec<UnifiedModel> = order
        .into_iter()
        .filter_map(|c| acc.remove(&c))
        .collect();

    // 顶层字段按当前调度策略命中来源侧选定（§3.1/§3.3 #2）：
    // per_model 覆盖优先 → 全局 priority；命中侧未声明时退可用源，最后退首源
    let policy = super::dispatch::load_policy(data_dir);
    for u in &mut out {
        let canonical = canonical_id(&u.id);
        let pools: Vec<TargetPool> = policy
            .per_model
            .get(&canonical)
            .unwrap_or(&policy.priority)
            .iter()
            .filter_map(|s| TargetPool::parse(s))
            .collect();
        // 命中池：自定义来源命中即直达（dispatch ⓪ 短路）→ 显示层同步命中 custom；
        // 否则优先级序首个 enabled 源 → 任一 enabled 源 → 首源
        let hit = u
            .sources
            .iter()
            .find(|s| s.pool == "custom" && s.enabled)
            .map(|s| s.pool)
            .or_else(|| {
                pools
                    .iter()
                    .find_map(|p| {
                        u.sources
                            .iter()
                            .find(|s| s.pool == p.as_str() && s.enabled)
                            .map(|s| s.pool)
                    })
            })
            .or_else(|| u.sources.iter().find(|s| s.enabled).map(|s| s.pool))
            .or_else(|| u.sources.first().map(|s| s.pool));
        // rate = 命中侧倍率；未声明退可用源 → 首源 → 保持原值
        u.rate = hit
            .and_then(|hp| u.sources.iter().find(|s| s.pool == hp).and_then(|s| s.rate))
            .or_else(|| u.sources.iter().find(|s| s.enabled).and_then(|s| s.rate))
            .or_else(|| u.sources.first().and_then(|s| s.rate))
            .or(u.rate);
        // context_length 跟随命中侧（issue #38-4）：命中 buddy → WB 目录值；
        // 命中 trae/custom 或未命中 → Trae 四层链值（L1/L2/L3 诚实口径）。
        // WB 池未启用时命中侧不可能是 buddy，残留快照数值不再拖低双源条目
        if hit == Some("buddy") {
            if let Some(c) = buddy_ctx.get(&canonical) {
                u.context_length = Some(*c);
            }
        }
        // display：L1 人工 label 绝对最高优先（§3.2，覆盖双源展示名）；
        // 未人工维护时命中 buddy → 取 WB 展示名，命中 trae → 保持 Trae 侧（已含 L2 label）
        if let Some(label) = l1.get(&canonical).and_then(|m| m.label.clone()) {
            u.display = label;
        } else if hit == Some("buddy") {
            if let Some(d) = buddy_disp.get(&canonical) {
                u.display = d.clone();
            }
        }
        // supports_image：命中 buddy → WB 声明值；命中 trae → Trae 侧值（L1/L2/L4），
        // 未声明（None）时退 WB 声明兜底（有值优先，§3.3 #2）
        if hit == Some("buddy") {
            if let Some(img) = buddy_img.get(&canonical) {
                u.supports_image = Some(*img);
            }
        } else if u.supports_image.is_none() {
            if let Some(img) = buddy_img.get(&canonical) {
                u.supports_image = Some(*img);
            }
        }
        // L1 人工维护标记统一按覆盖层判定
        u.manual = l1.contains_key(&canonical);
        // 供应商：自定义模型用户填写值优先，否则按模型名系列推断（未知空串 → 前端显示 —）
        u.vendor = custom_vendor
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| vendor_of(&canonical).to_string());
    }

    // 目录序：双源在前、单源在后，组内字母序（§3.3 #4）
    out.sort_by(|a, b| {
        let ka = if a.sources.len() >= 2 { 0 } else { 1 };
        let kb = if b.sources.len() >= 2 { 0 } else { 1 };
        ka.cmp(&kb).then_with(|| a.id.to_lowercase().cmp(&b.id.to_lowercase()))
    });
    out
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试数据目录脚手架：写 api_models / wb_model_catalog / trae_model_meta
    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(
        trae: &[(&str, Option<f64>)],
        wb: &[&str],
        meta: Option<serde_json::Value>,
    ) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "twa_ucat_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        // SQLite 化（P2）：测试种子改走 kv（api_models / wb_model_catalog / trae_model_meta）
        let st = crate::store::db(&dir);
        let list: Vec<serde_json::Value> = trae
            .iter()
            .map(|(id, rate)| {
                let mut o = json!({"id": id, "label": id});
                if let Some(r) = rate {
                    o["rate"] = json!(r);
                }
                o
            })
            .collect();
        st.kv_set("api_models", &list).unwrap();
        if !wb.is_empty() {
            let list: Vec<serde_json::Value> = wb
                .iter()
                .map(|id| {
                    json!({"id": id, "display": id.to_uppercase(), "context_length": 200000,
                           "max_tokens": 64000, "supports_image": true,
                           "supported_efforts": ["low","medium","high"], "rate": 0.79})
                })
                .collect();
            st.kv_set(
                "wb_model_catalog",
                // 模拟「人工维护」目录：必须携带当前 builtin_rev 才退出内置表重建
                //（wb_catalog::load 对旧版本自动落盘的目录按新快照重建，见该文件 BUILTIN_REV 注释）
                &json!({"models": list, "builtin_rev": crate::api_server::wb_catalog::BUILTIN_REV}),
            )
            .unwrap();
        }
        if let Some(m) = meta {
            st.kv_set_raw("trae_model_meta", &m.to_string()).unwrap();
        }
        Fixture { dir }
    }

    fn find<'a>(list: &'a [UnifiedModel], id: &str) -> &'a UnifiedModel {
        list.iter()
            .find(|m| canonical_id(&m.id) == canonical_id(id))
            .unwrap_or_else(|| panic!("model {id} not in unified catalog"))
    }

    #[test]
    fn t01_canonical_id_trims_and_lowercases() {
        assert_eq!(canonical_id("  GLM-5.3 "), "glm-5.3");
        assert_eq!(canonical_id("DeepSeek-V4-Flash"), "deepseek-v4-flash");
        assert_eq!(canonical_id("Doubao-Seed-Evolving"), "doubao-seed-evolving");
    }

    /// 双源合并：sources 双侧、WB 结构化元数据为主源、目录序双源在前
    #[test]
    fn t02_dual_source_merge_prefers_wb_values() {
        let f = fixture(
            &[("DeepSeek-V4-Flash", None)],
            &["deepseek-v4-flash"],
            None,
        );
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(list.len(), 1);
        let m = &list[0];
        // 对外 id 保留 Trae 形态（存量客户端透传语义不变 §9.6）
        assert_eq!(m.id, "DeepSeek-V4-Flash");
        // WB 为主源：display 取 WB、倍率取 WB
        assert_eq!(m.display, "DEEPSEEK-V4-FLASH");
        assert_eq!(m.rate, Some(0.79));
        assert_eq!(m.sources.len(), 2);
        assert!(m.sources.iter().all(|s| s.enabled));
        // context 按命中侧选定（issue #38-4）：默认策略 buddy 优先 → WB 目录值
        assert_eq!(m.context_length, Some(200_000));
        assert_eq!(m.supports_image, Some(true));
    }

    /// 单源各自保留：Trae-only 走 L3 兜底；Buddy-only 直接透传目录
    #[test]
    fn t03_single_source_fallbacks() {
        let f = fixture(
            &[("Kimi-K2.7-Code", None), ("Doubao-Seed-Evolving", None)],
            &["hy4"],
            None,
        );
        let list = unified_models(&f.dir, true, true, true);
        // kimi-k2.7-code：L3 倍率 0.83 / 档位无实证 → 空（issue #31 停用名称推断）/
        // L4 图片不支持（Code）/ L3 上下文 128K
        let k = find(&list, "Kimi-K2.7-Code");
        assert_eq!(k.rate, Some(0.83));
        assert!(k.efforts.is_empty(), "无实证模型档位为空，不按名称猜测");
        assert_eq!(k.supports_image, Some(false));
        assert_eq!(k.context_length, Some(CTX_128K));
        // Doubao-Seed-Evolving：L3 诚实兜底 128K（1M 声明仅 Max Mode 可达，issue #31）/
        // L3 倍率 0.08 / L4 Seed 系列支持图片
        let s = find(&list, "Doubao-Seed-Evolving");
        assert_eq!(s.context_length, Some(CTX_128K));
        assert_eq!(s.rate, Some(0.08));
        assert_eq!(s.supports_image, Some(true));
        // hy4：Buddy-only，直取目录值
        let h = find(&list, "hy4");
        assert_eq!(h.sources.len(), 1);
        assert_eq!(h.sources[0].pool, "buddy");
        assert_eq!(h.rate, Some(0.79));
    }

    /// L1 人工维护覆盖一切（含 WB 主源值），manual 标记生效
    #[test]
    fn t04_l1_meta_overrides_all_layers() {
        let f = fixture(
            &[("glm-5.3", Some(0.78))],
            &["glm-5.3"],
            Some(json!({"glm-5.3": {"label": "自命名", "rate": 0.5,
                                    "efforts": ["high"], "context_length": 999,
                                    "max_tokens": 111, "supports_image": false}})),
        );
        let list = unified_models(&f.dir, true, true, true);
        let m = find(&list, "glm-5.3");
        assert!(m.manual);
        // Trae 源侧取 L1 值
        let ts = m.sources.iter().find(|s| s.pool == "trae").unwrap();
        assert_eq!(ts.rate, Some(0.5));
        // L1 人工 label 绝对最高优先：即使默认策略命中 buddy（WB 展示名），顶层 display 也取人工值
        assert_eq!(m.rate, Some(0.79));
        assert_eq!(m.display, "自命名");
        assert_eq!(
            m.efforts,
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        );
        // Trae-only 模型 L1 全链生效
        let f2 = fixture(&[("my-model", None)], &[], Some(json!({"my-model": {"label": "人工", "rate": 0.33}})));
        let list2 = unified_models(&f2.dir, true, true, true);
        let m2 = find(&list2, "my-model");
        assert!(m2.manual);
        assert_eq!(m2.display, "人工");
        assert_eq!(m2.rate, Some(0.33));
        assert_eq!(m2.context_length, Some(CTX_128K), "L3 兜底未指定字段");
    }

    /// meta_set / meta_clear 落盘与恢复
    #[test]
    fn t05_meta_set_clear_roundtrip() {
        // wb 侧放无关模型，保证 glm-5.3 为 Trae-only（顶层 rate 才取 Trae 源/L1 值）
        let f = fixture(&[("glm-5.3", None)], &["hy4"], None);
        meta_set(
            &f.dir,
            " GLM-5.3 ",
            TraeModelMeta {
                rate: Some(0.42),
                ..Default::default()
            },
        )
        .unwrap();
        // 键归一：canonical 键存储
        let map = load_meta(&f.dir);
        assert!(map.contains_key("glm-5.3"));
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").rate, Some(0.42));
        assert!(meta_clear(&f.dir, "glm-5.3").unwrap());
        assert!(!meta_clear(&f.dir, "glm-5.3").unwrap(), "重复清除返回 false");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").rate, Some(0.78), "清除后回 L3 参考值");
    }

    /// 运行时派生 enabled：wb_enabled=false → Buddy 源剔除标记；池不健康同理
    #[test]
    fn t06_enabled_flags_runtime_derived() {
        // glm-5.3 双源 + hy4 仅 Buddy
        let f = fixture(&[("glm-5.3", None)], &["hy4", "glm-5.3"], None);
        let list = unified_models(&f.dir, false, true, true);
        let h = find(&list, "hy4");
        assert!(!h.sources[0].enabled, "wb_enabled=false → buddy 源不可用");
        let g = find(&list, "glm-5.3");
        assert!(g.sources.iter().find(|s| s.pool == "trae").unwrap().enabled);
        assert!(!g.sources.iter().find(|s| s.pool == "buddy").unwrap().enabled);
        // Trae 池无可选账号 → trae 源不可用
        let list = unified_models(&f.dir, true, false, true);
        let g = find(&list, "glm-5.3");
        assert!(!g.sources.iter().find(|s| s.pool == "trae").unwrap().enabled);
    }

    /// 双源 context_length 跟随命中侧/启用池（issue #38-4）：WB 池未启用时
    /// 残留 buddy 快照数值不得拖低双源条目的上下文声明（原实现无条件取 min，
    /// glm-5.2 等被压到 128K）
    #[test]
    fn t06b_dual_context_follows_enabled_pool() {
        let f = fixture(&[("DeepSeek-V4-Flash", None)], &["deepseek-v4-flash"], None);
        // WB 池启用 + 默认 buddy 优先 → 命中 buddy → WB 目录值
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(
            find(&list, "DeepSeek-V4-Flash").context_length,
            Some(200_000)
        );
        // WB 池未启用 → 命中 trae → Trae 四层链值（L3 兜底 128K），快照不拖低
        let list = unified_models(&f.dir, false, true, true);
        assert_eq!(
            find(&list, "DeepSeek-V4-Flash").context_length,
            Some(CTX_128K)
        );
        // WB 启用但 per_model 覆盖为 trae 优先 → 命中 trae → Trae 值
        crate::store::db(&f.dir)
            .kv_set(
                "dispatch_policy",
                &json!({"priority": ["buddy", "trae"],
                    "per_model": {"deepseek-v4-flash": ["trae", "buddy"]}, "fallback": true}),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "dispatch_policy");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(
            find(&list, "DeepSeek-V4-Flash").context_length,
            Some(CTX_128K)
        );
    }

    /// 目录序：双源在前、单源在后、组内字母序
    #[test]
    fn t07_order_dual_first_then_alphabetical() {
        let f = fixture(
            &[("zzz-trae-only", None), ("glm-5.3", None)],
            &["aaa-buddy-only", "glm-5.3"],
            None,
        );
        let list = unified_models(&f.dir, true, true, true);
        let ids: Vec<String> = list.iter().map(|m| canonical_id(&m.id)).collect();
        assert_eq!(ids, vec!["glm-5.3", "aaa-buddy-only", "zzz-trae-only"]);
    }

    /// 顶层 rate 跟随策略命中侧：per_model 覆盖 trae 优先 → Trae 源倍率
    #[test]
    fn t08_top_rate_follows_dispatch_policy() {
        let f = fixture(&[("glm-5.3", None)], &["glm-5.3"], None);
        // 默认策略 buddy 优先 → WB 倍率
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").rate, Some(0.79));
        // per_model 覆盖为 trae 优先 → Trae 源倍率（L3 参考值 0.78）
        crate::store::db(&f.dir)
            .kv_set("dispatch_policy", &json!({"priority": ["buddy", "trae"],
                   "per_model": {"glm-5.3": ["trae", "buddy"]}, "fallback": true}))
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "dispatch_policy");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").rate, Some(0.78));
        // 命中侧 WB rate=0 = 免费声明（与调度层同语义）→ 顶层透传免费，不再退 Trae 源
        //（恢复默认 buddy 优先：第二段 per_model 仍生效会使命中侧停留在 trae）
        crate::store::db(&f.dir)
            .kv_set("dispatch_policy",
                    &json!({"priority": ["buddy", "trae"], "per_model": {}, "fallback": true}))
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "dispatch_policy");
        crate::store::db(&f.dir)
            .kv_set(
                "wb_model_catalog",
                &json!({"models": [{"id": "glm-5.3", "display": "GLM-5.3", "context_length": 0,
                               "max_tokens": 0, "supports_image": true,
                               "supported_efforts": [], "rate": 0.0}],
                        "builtin_rev": crate::api_server::wb_catalog::BUILTIN_REV}),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "wb_model_catalog");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").rate, Some(0.0), "WB rate=0 = 免费声明（命中 buddy 侧透传）");
    }

    /// 同 canonical 重复 Trae 条目去重（首条胜出）
    #[test]
    fn t09_duplicate_canonical_deduped() {
        // wb 侧放无关模型（空目录语义不可表达，会回退内置表）
        let f = fixture(&[("glm-5.3", None), ("GLM-5.3", None)], &["hy4"], None);
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(list.len(), 2, "glm-5.3 去重后仅剩 glm-5.3 + hy4");
        let g: Vec<&UnifiedModel> = list
            .iter()
            .filter(|m| canonical_id(&m.id) == "glm-5.3")
            .collect();
        assert_eq!(g.len(), 1, "同 canonical 仅保留一条");
        assert_eq!(g[0].id, "glm-5.3", "首条胜出（小写原样形态）");
    }

    /// L4 视觉模型名推断：数字+V 模式（如 GLM-5V-Turbo）
    #[test]
    fn t10_l4_vision_name_inference() {
        let f = fixture(&[("GLM-5V-Turbo", None)], &[], None);
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "GLM-5V-Turbo").supports_image, Some(true));
        assert!(
            find(&list, "GLM-5V-Turbo").efforts.is_empty(),
            "turbo 不再按名称推断档位（issue #31）"
        );
    }

    /// 人工维护值在官网同步后保留（§10 关键断言）：同步重写 api_models.json
    /// 不触碰 trae_model_meta.json（文件隔离），聚合仍取 L1 优先于 L2
    #[test]
    fn t11_manual_meta_survives_official_sync() {
        let f = fixture(&[("glm-5.3", None)], &["hy4"], None);
        meta_set(
            &f.dir,
            " GLM-5.3 ",
            TraeModelMeta {
                label: Some("人工名".into()),
                rate: Some(0.42),
                context_length: Some(999_999),
                ..Default::default()
            },
        )
        .unwrap();
        // 模拟官网同步：parse_official 整表重写 api_models（新倍率 0.99 + 新增条目）
        crate::store::db(&f.dir)
            .kv_set(
                "api_models",
                &json!([
                    {"id": "glm-5.3", "label": "GLM-5.3", "rate": 0.99},
                    {"id": "new-official-model", "label": "New"}
                ]),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "api_models");
        let list = unified_models(&f.dir, true, true, true);
        let g = find(&list, "glm-5.3");
        assert_eq!(g.rate, Some(0.42), "L1 人工值保留，不被同步值覆盖");
        assert_eq!(g.display, "人工名");
        assert_eq!(g.context_length, Some(999_999));
        assert!(g.manual);
        assert_eq!(find(&list, "new-official-model").id, "new-official-model", "同步新增条目可见");
        // meta 文件未被同步重写触碰（文件隔离）
        assert!(load_meta(&f.dir).contains_key("glm-5.3"));
    }

    /// 双源顶层 display / supports_image 跟随调度策略命中侧（修复：不再无条件取 WB 值）
    #[test]
    fn t12_dual_display_and_image_follow_policy() {
        // kimi-k2.7-code：Trae 侧 L4 推断图片不支持（Code 系列）、label = id；
        // WB 侧 display 大写、图片支持 true
        let f = fixture(&[("Kimi-K2.7-Code", None)], &["kimi-k2.7-code"], None);
        // 默认策略 buddy 优先 → 展示名/图片 = WB 值
        let list = unified_models(&f.dir, true, true, true);
        let m = find(&list, "kimi-k2.7-code");
        assert_eq!(m.display, "KIMI-K2.7-CODE");
        assert_eq!(m.supports_image, Some(true));
        // per_model 覆盖为 trae 优先 → 展示名 = Trae 侧 L2 label、图片 = Trae 侧 L4 推断
        crate::store::db(&f.dir)
            .kv_set(
                "dispatch_policy",
                &json!({"priority": ["buddy", "trae"],
                   "per_model": {"kimi-k2.7-code": ["trae", "buddy"]}, "fallback": true}),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "dispatch_policy");
        let list = unified_models(&f.dir, true, true, true);
        let m = find(&list, "kimi-k2.7-code");
        assert_eq!(m.display, "Kimi-K2.7-Code");
        assert_eq!(m.supports_image, Some(false));
        // 命中 trae 且 Trae 侧未声明图片（glm-5.3 L4 无推断）→ 退 WB 声明兜底
        let f2 = fixture(&[("glm-5.3", None)], &["glm-5.3"], None);
        crate::store::db(&f2.dir)
            .kv_set("dispatch_policy", &json!({"priority": ["trae", "buddy"], "fallback": true}))
            .unwrap();
        super::super::config_cache::invalidate(&f2.dir, "dispatch_policy");
        let list2 = unified_models(&f2.dir, true, true, true);
        let g = find(&list2, "glm-5.3");
        assert_eq!(g.display, "glm-5.3", "命中 trae → Trae 侧展示名");
        assert_eq!(g.supports_image, Some(true), "Trae 侧未声明退 WB 兜底");
    }

    /// disabled 自定义条目不得覆盖聚合视图顶层 supports_image（仅 enabled 参与）。
    /// 用不在内置目录的唯一模型名，避开 wb_catalog 缺失自愈内置表的干扰
    #[test]
    fn t13_disabled_custom_entry_does_not_override_supports_image() {
        // Trae 侧 L4 对 my-vision-model 无图片推断 → supports_image = None；
        // disabled 自定义条目声明 supports_image=true → 不得覆盖
        let f = fixture(&[("my-vision-model", None)], &[], None);
        // SQLite 化（P3）：custom_models 表
        crate::store::docs::custom_models_save(
            &crate::store::db(&f.dir),
            &crate::api_server::custom_models::CustomModelsFile {
                models: vec![crate::api_server::custom_models::CustomModel {
                    id: "cm1".into(),
                    name: "my-vision-model".into(),
                    base_url: "https://x".into(),
                    enabled: false,
                    supports_image: true,
                    ..Default::default()
                }],
                updated_at: 0,
            },
        )
        .unwrap();
        super::super::config_cache::invalidate(&f.dir, "custom_models");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(
            find(&list, "my-vision-model").supports_image,
            None,
            "disabled 条目不覆盖顶层 supports_image"
        );
        // enabled 条目参与覆盖 → true
        crate::store::docs::custom_models_save(
            &crate::store::db(&f.dir),
            &crate::api_server::custom_models::CustomModelsFile {
                models: vec![crate::api_server::custom_models::CustomModel {
                    id: "cm1".into(),
                    name: "my-vision-model".into(),
                    base_url: "https://x".into(),
                    enabled: true,
                    supports_image: true,
                    ..Default::default()
                }],
                updated_at: 0,
            },
        )
        .unwrap();
        super::super::config_cache::invalidate(&f.dir, "custom_models");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "my-vision-model").supports_image, Some(true));
    }

    /// 自定义模型 rate=0 = 免费（有效展示语义）：source/顶层均透传 Some(0.0)，
    /// 帮助列表按「免费」徽标展示（不得再当「未声明」丢弃为 None）
    #[test]
    fn t14_custom_zero_rate_is_free() {
        let f = fixture(&[], &[], None);
        // SQLite 化（P3）：custom_models 表
        crate::store::docs::custom_models_save(
            &crate::store::db(&f.dir),
            &crate::api_server::custom_models::CustomModelsFile {
                models: vec![crate::api_server::custom_models::CustomModel {
                    id: "cm1".into(),
                    name: "my-free-model".into(),
                    base_url: "https://x".into(),
                    enabled: true,
                    rate: 0.0,
                    ..Default::default()
                }],
                updated_at: 0,
            },
        )
        .unwrap();
        super::super::config_cache::invalidate(&f.dir, "custom_models");
        let list = unified_models(&f.dir, true, true, true);
        let m = find(&list, "my-free-model");
        assert_eq!(m.rate, Some(0.0), "rate=0 必须透传为免费（Some(0.0)）");
        assert_eq!(m.sources[0].rate, Some(0.0));
    }

    // ==================== issue #26 全局模型白名单 ====================

    /// save 归一（canonical + 去空 + 去重保序）/ 回读 / 判定语义（空 = 不限）
    #[test]
    fn t15_whitelist_save_normalize_and_allows() {
        let f = fixture(&[("glm-5.3", None)], &[], None);
        let saved = save_whitelist(
            &f.dir,
            &["  GLM-5.3 ".into(), "".into(), "glm-5.3".into(), "DeepSeek-V4-Flash".into()],
        )
        .unwrap();
        // 归一 + 去空 + 去重保序
        assert_eq!(saved, vec!["glm-5.3".to_string(), "deepseek-v4-flash".to_string()]);
        assert_eq!(load_whitelist(&f.dir), saved, "回读与保存值一致");
        // 命中：canonical 化匹配（大小写/空白不敏感）
        assert!(whitelist_allows(&saved, "glm-5.3"));
        assert!(whitelist_allows(&saved, "  DeepSeek-V4-Flash "));
        assert!(!whitelist_allows(&saved, "kimi-k3"));
        // 空名单 = 不限（未保存时缺失也是空）
        assert!(whitelist_allows(&[], "anything"));
        // 空保存 → 归一为空列表（= 不限）
        let cleared = save_whitelist(&f.dir, &[]).unwrap();
        assert!(cleared.is_empty());
        assert!(whitelist_allows(&load_whitelist(&f.dir), "kimi-k3"));
    }

    /// unified_models_whitelisted：白名单过滤对外目录；空名单全量透传
    #[test]
    fn t16_unified_models_whitelisted_filter() {
        let f = fixture(&[("glm-5.3", None)], &["hy4"], None);
        // 空名单 → 全量
        let all = unified_models_whitelisted(&f.dir, true, true, true);
        assert_eq!(all.len(), 2);
        // 仅 glm-5.3 → hy4 被过滤
        save_whitelist(&f.dir, &["GLM-5.3".to_string()]).unwrap();
        let filtered = unified_models_whitelisted(&f.dir, true, true, true);
        assert_eq!(filtered.len(), 1);
        assert_eq!(canonical_id(&filtered[0].id), "glm-5.3");
    }

    /// 未知条目允许保存（先建名单后同步目录的工作流，save 不校验目录）；
    /// 仅含未知条目时对外目录收紧为空（/v1/models 无可返回模型，不静默放行）
    #[test]
    fn t17_whitelist_unknown_entries_and_empty_catalog() {
        let f = fixture(&[("glm-5.3", None)], &[], None);
        // 目录不存在的条目可直接保存
        let saved = save_whitelist(
            &f.dir,
            &["future-model-x".to_string(), "GLM-5.3".to_string()],
        )
        .unwrap();
        assert_eq!(saved.len(), 2, "未知 + 已知条目均入库");
        assert_eq!(load_whitelist(&f.dir), saved);
        // 已知模型保留，未知条目不产生目录条目
        let filtered = unified_models_whitelisted(&f.dir, true, true, true);
        assert_eq!(filtered.len(), 1);
        assert_eq!(canonical_id(&filtered[0].id), "glm-5.3");
        // 仅未知条目 → 对外目录为空
        save_whitelist(&f.dir, &["future-model-x".to_string()]).unwrap();
        assert!(unified_models_whitelisted(&f.dir, true, true, true).is_empty());
    }

    // ==================== issue #31 档位统一（并集 + 归一） ====================

    /// 双源档位声明取并集（WB 不再覆盖 Trae 侧），context 按命中侧选定
    #[test]
    fn t18_dual_source_efforts_union_and_context_hit_side() {
        // glm-5.3：Trae 侧 L3 实证 [low,high,xhigh]；WB 侧目录 [low,medium,high]
        let f = fixture(&[("glm-5.3", None)], &["glm-5.3"], None);
        let list = unified_models(&f.dir, true, true, true);
        let m = find(&list, "glm-5.3");
        assert_eq!(
            m.efforts,
            vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string()
            ],
            "双源并集：Trae 实证 ∪ WB 目录"
        );
        // context 按命中侧选定（issue #38-4）：默认策略 buddy 优先 → WB 目录值
        assert_eq!(m.context_length, Some(200_000));
        // per_model 覆盖为 trae 优先 → 命中 trae → Trae L3 诚实兜底 128K
        //（主条目不声明 1M，issue #31）
        crate::store::db(&f.dir)
            .kv_set(
                "dispatch_policy",
                &json!({"priority": ["buddy", "trae"],
                    "per_model": {"glm-5.3": ["trae", "buddy"]}, "fallback": true}),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "dispatch_policy");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").context_length, Some(CTX_128K));
    }

    /// L2 同步档位归一：wire 值（light/extra_high）映射统一档位，
    /// 已是统一值的保留、大小写归一、未知值丢弃；L2 非空时不回退 L3 实证表
    #[test]
    fn t19_l2_efforts_normalized_to_unified() {
        let f = fixture(&[], &[], None);
        crate::store::db(&f.dir)
            .kv_set(
                "api_models",
                &json!([
                    {"id": "wire-model", "label": "W", "efforts": ["light", "extra_high"]},
                    {"id": "mixed-model", "label": "M", "efforts": ["light", "low", "bogus", "HIGH"]}
                ]),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "api_models");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(
            find(&list, "wire-model").efforts,
            vec!["low".to_string(), "xhigh".to_string()],
            "wire 值归一为统一档位"
        );
        assert_eq!(
            find(&list, "mixed-model").efforts,
            vec!["low".to_string(), "high".to_string()],
            "统一值保留 / 大小写归一 / 未知值丢弃 / 去重"
        );
    }

    /// /v1/models 上下文诚实口径（issue #31）：L2 dev 槽（实际请求口径）传导为
    /// 统一 context_length；max 声明槽不冒充——即使 L2 带出 1M 声明也取 dev 值
    #[test]
    fn t20_l2_dev_slot_flows_through_max_not_claimed() {
        let f = fixture(&[], &[], None);
        crate::store::db(&f.dir)
            .kv_set(
                "api_models",
                &json!([
                    {"id": "ctx-model", "label": "C",
                     "context_length": 168000, "context_length_max": 1000000},
                    {"id": "ctx-max-only", "label": "M", "context_length_max": 1000000}
                ]),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "api_models");
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(
            find(&list, "ctx-model").context_length,
            Some(168_000),
            "dev 实际口径胜出，1M 声明不冒充（issue #31）"
        );
        // 仅 max（dev 缺失）→ 不冒充声明值，回落 L3 保守 128K
        assert_eq!(
            find(&list, "ctx-max-only").context_length,
            Some(CTX_128K),
            "仅声明槽不冒充实际口径，L3 保守兜底"
        );
    }

    /// L2 全未知档位（脏数据）不遮蔽 L3 实证表：归一后为空视为未声明，
    /// 回退 L3（审查修复：原判断用原始 efforts 非空，脏档位会静默清空档位列）
    #[test]
    fn t21_l2_all_unknown_efforts_fall_back_to_l3() {
        let f = fixture(&[], &[], None);
        crate::store::db(&f.dir)
            .kv_set(
                "api_models",
                &json!([
                    {"id": "glm-5.3", "label": "G", "efforts": ["bogus-a", "bogus-b"]}
                ]),
            )
            .unwrap();
        super::super::config_cache::invalidate(&f.dir, "api_models");
        let list = unified_models(&f.dir, true, true, true);
        // 双源并集：WB 内置 [low,medium,high] ∪ Trae L3 实证 [low,high,xhigh]；
        // 修复前 L2 脏档位遮蔽 L3 → 实证侧为空 → 并集缺 xhigh
        assert_eq!(
            find(&list, "glm-5.3").efforts,
            vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string()
            ],
            "L2 脏档位归一为空 → 回退 L3 实证表（xhigh 仅来自 L3）"
        );
    }
}
