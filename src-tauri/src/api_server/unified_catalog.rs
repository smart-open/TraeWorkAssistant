//! 统一模型目录（unified-api-gateway-design §3）
//!
//! 派生聚合视图：不落盘第三份目录，实时合并两个源——
//! - 源1 `data/api_models.json`（Trae 官网同步 + 内置补位，models_sync）
//! - 源2 `data/wb_model_catalog.json`（Buddy 目录，wb_catalog；对外标识 buddy）
//!
//! Trae 侧元数据四层来源（§3.2，逐级兜底）：
//! - L1 人工维护：`data/trae_model_meta.json` 覆盖层（键 canonical_id），官网同步永不覆盖
//! - L2 官网同步：api_models.json 条目扩展字段（ModelOption serde default，宽容解析）
//! - L3 文档参考值：内置表（Max 模式 1M 上下文 + 其余 128K、倍率初始参考）
//! - L4 名称推断：系列规则（思考档位 / 图片支持）；上下文 L3 已全量覆盖
//!   （1M 表命中 → 1M，其余 → 128K），故无 L4 上下文规则
//!
//! 归并键 `canonical_id()` 三处统一：目录归并 / dispatch_policy.per_model /
//! trae_model_meta 覆盖层（§3.3 #1），防止规则漂移。

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

/// Max 模式 1M 上下文档（docs.trae.cn/ide_max-mode）
const MAX_MODE_1M: [&str; 11] = [
    "doubao-seed-evolving",
    "glm-5.3",
    "glm-5.2",
    "deepseek-v4-pro",
    "deepseek-v4-pro-official",
    "deepseek-v4-flash",
    "deepseek-v4-flash-official",
    "kimi-k3",
    "minimax-m3",
    "qwen3.8-max",
    "qwen-3.7-plus",
];
const CTX_1M: u64 = 1_000_000;
/// L3：其余模型默认 128K（与旧 /v1/models 的 131072 一致）
const CTX_128K: u64 = 131_072;

/// 倍率初始参考（客户端下拉实测，随官网同步覆盖，§3.2）
const RATE_REF: [(&str, f64); 13] = [
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
];

fn doc_rate(canonical: &str) -> Option<f64> {
    RATE_REF
        .iter()
        .find(|(id, _)| *id == canonical)
        .map(|(_, r)| *r)
}

// ==================== L4 名称推断（可被 L1–L3 覆盖，§3.2） ====================

/// 思考档位推断：Flash/Turbo 轻量系列 `["low","medium"]`；
/// DeepSeek-4 / Kimi-K2·K3 / GLM-5 系列 `["medium","high"]`；未知 → 空（显示 —）
fn infer_efforts(canonical: &str) -> Vec<String> {
    if canonical.contains("flash") || canonical.contains("turbo") {
        return vec!["low".into(), "medium".into()];
    }
    let in_series = canonical.starts_with("deepseek-v4")
        || canonical.starts_with("kimi-k2")
        || canonical.starts_with("kimi-k3")
        || canonical.starts_with("glm-5");
    if in_series {
        return vec!["medium".into(), "high".into()];
    }
    Vec::new()
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

fn meta_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("data").join("trae_model_meta.json")
}

/// 读取 L1 覆盖层（键 canonical_id；缺失/损坏回退空表）
pub fn load_meta(data_dir: &Path) -> HashMap<String, TraeModelMeta> {
    crate::fs_utils::read_json(&meta_path(data_dir))
}

/// L1 写入（upsert；编辑弹框整条覆盖，官网同步不触碰本文件）
pub fn meta_set(data_dir: &Path, model: &str, meta: TraeModelMeta) -> Result<(), String> {
    let id = canonical_id(model);
    if id.is_empty() {
        return Err("模型 ID 不能为空".into());
    }
    let mut map = load_meta(data_dir);
    map.insert(id, meta);
    crate::fs_utils::write_json(&meta_path(data_dir), &map)
}

/// L1 清除（恢复自动来源链 L2→L3→L4）；返回是否确有删除
pub fn meta_clear(data_dir: &Path, model: &str) -> Result<bool, String> {
    let id = canonical_id(model);
    let mut map = load_meta(data_dir);
    let removed = map.remove(&id).is_some();
    if removed {
        crate::fs_utils::write_json(&meta_path(data_dir), &map)?;
    }
    Ok(removed)
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

/// 四层链解析 Trae 条目：L1 覆盖层 → L2 条目扩展字段 → L3 文档表 → L4 名称推断
fn trae_entry_of(m: &ModelOption, l1: Option<&TraeModelMeta>) -> TraeEntry {
    let canonical = canonical_id(&m.id);
    // rate: L1 → L2 → L3
    let rate = l1
        .and_then(|l| l.rate)
        .or(m.rate)
        .or_else(|| doc_rate(&canonical));
    // efforts: L1 → L2（非空才算有值）→ L4
    let efforts = l1
        .and_then(|l| l.efforts.clone())
        .filter(|e| !e.is_empty())
        .or_else(|| {
            if m.efforts.is_empty() {
                None
            } else {
                Some(m.efforts.clone())
            }
        })
        .unwrap_or_else(|| infer_efforts(&canonical));
    // context: L1 → L2 → L3（1M 表命中 1M，其余 128K——L3 全量覆盖）
    let context_length = l1
        .and_then(|l| l.context_length)
        .or(m.context_length)
        .or_else(|| {
            Some(if MAX_MODE_1M.contains(&canonical.as_str()) {
                CTX_1M
            } else {
                CTX_128K
            })
        });
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

/// wb 目录倍率：0 视为未声明（交由另一源兜底）
fn wb_rate(m: &WbModel) -> Option<f64> {
    if m.rate > 0.0 {
        Some(m.rate)
    } else {
        None
    }
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
                if !m.supported_efforts.is_empty() {
                    u.efforts = m.supported_efforts.clone();
                }
                if let Some(c) = wctx {
                    u.context_length = Some(c);
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
        let crate_rate = if m.rate > 0.0 { Some(m.rate) } else { None };
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
        std::fs::write(
            dir.join("data").join("api_models.json"),
            serde_json::to_string(&list).unwrap(),
        )
        .unwrap();
        if !wb.is_empty() {
            let list: Vec<serde_json::Value> = wb
                .iter()
                .map(|id| {
                    json!({"id": id, "display": id.to_uppercase(), "context_length": 200000,
                           "max_tokens": 64000, "supports_image": true,
                           "supported_efforts": ["low","medium","high"], "rate": 0.79})
                })
                .collect();
            std::fs::write(
                dir.join("data").join("wb_model_catalog.json"),
                json!({"models": list}).to_string(),
            )
            .unwrap();
        }
        if let Some(m) = meta {
            std::fs::write(
                dir.join("data").join("trae_model_meta.json"),
                m.to_string(),
            )
            .unwrap();
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
        assert_eq!(m.context_length, Some(200000));
        assert_eq!(m.supports_image, Some(true));
    }

    /// 单源各自保留：Trae-only 走 L3/L4 兜底；Buddy-only 直接透传目录
    #[test]
    fn t03_single_source_fallbacks() {
        let f = fixture(
            &[("Kimi-K2.7-Code", None), ("Doubao-Seed-Evolving", None)],
            &["hy4"],
            None,
        );
        let list = unified_models(&f.dir, true, true, true);
        // kimi-k2.7-code：L3 倍率 0.83 / L4 档位 medium,high / L4 图片不支持（Code）/ L3 上下文 128K
        let k = find(&list, "Kimi-K2.7-Code");
        assert_eq!(k.rate, Some(0.83));
        assert_eq!(k.efforts, vec!["medium".to_string(), "high".to_string()]);
        assert_eq!(k.supports_image, Some(false));
        assert_eq!(k.context_length, Some(CTX_128K));
        // Doubao-Seed-Evolving：L3 1M 上下文 / L3 倍率 0.08 / L4 Seed 系列支持图片
        let s = find(&list, "Doubao-Seed-Evolving");
        assert_eq!(s.context_length, Some(CTX_1M));
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
        std::fs::write(
            f.dir.join("data").join("dispatch_policy.json"),
            json!({"priority": ["buddy", "trae"],
                   "per_model": {"glm-5.3": ["trae", "buddy"]}, "fallback": true}).to_string(),
        )
        .unwrap();
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").rate, Some(0.78));
        // 命中侧未声明倍率（WB rate=0 视为未声明）→ 退另一可用源
        std::fs::write(
            f.dir.join("data").join("wb_model_catalog.json"),
            json!({"models": [{"id": "glm-5.3", "display": "GLM-5.3", "context_length": 0,
                               "max_tokens": 0, "supports_image": true,
                               "supported_efforts": [], "rate": 0.0}]})
                .to_string(),
        )
        .unwrap();
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "glm-5.3").rate, Some(0.78), "WB 未声明倍率退 Trae 源");
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
        assert_eq!(
            find(&list, "GLM-5V-Turbo").efforts,
            vec!["low".to_string(), "medium".to_string()],
            "turbo 轻量系列档位"
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
        // 模拟官网同步：parse_official 整表重写 api_models.json（新倍率 0.99 + 新增条目）
        std::fs::write(
            f.dir.join("data").join("api_models.json"),
            serde_json::to_string(&json!([
                {"id": "glm-5.3", "label": "GLM-5.3", "rate": 0.99},
                {"id": "new-official-model", "label": "New"}
            ]))
            .unwrap(),
        )
        .unwrap();
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
        std::fs::write(
            f.dir.join("data").join("dispatch_policy.json"),
            json!({"priority": ["buddy", "trae"],
                   "per_model": {"kimi-k2.7-code": ["trae", "buddy"]}, "fallback": true})
                .to_string(),
        )
        .unwrap();
        let list = unified_models(&f.dir, true, true, true);
        let m = find(&list, "kimi-k2.7-code");
        assert_eq!(m.display, "Kimi-K2.7-Code");
        assert_eq!(m.supports_image, Some(false));
        // 命中 trae 且 Trae 侧未声明图片（glm-5.3 L4 无推断）→ 退 WB 声明兜底
        let f2 = fixture(&[("glm-5.3", None)], &["glm-5.3"], None);
        std::fs::write(
            f2.dir.join("data").join("dispatch_policy.json"),
            json!({"priority": ["trae", "buddy"], "fallback": true}).to_string(),
        )
        .unwrap();
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
        std::fs::write(
            f.dir.join("data").join("custom_models.json"),
            json!({"models": [{"id": "cm1", "name": "my-vision-model", "base_url": "https://x",
                               "enabled": false, "supports_image": true}]})
                .to_string(),
        )
        .unwrap();
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(
            find(&list, "my-vision-model").supports_image,
            None,
            "disabled 条目不覆盖顶层 supports_image"
        );
        // enabled 条目参与覆盖 → true
        std::fs::write(
            f.dir.join("data").join("custom_models.json"),
            json!({"models": [{"id": "cm1", "name": "my-vision-model", "base_url": "https://x",
                               "enabled": true, "supports_image": true}]})
                .to_string(),
        )
        .unwrap();
        let list = unified_models(&f.dir, true, true, true);
        assert_eq!(find(&list, "my-vision-model").supports_image, Some(true));
    }
}
