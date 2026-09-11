//! 四段模型路由管线（T5.2/F-61）
//!
//! 「任意模型名 → 上游真实模型」四级路由，每级命中即止，全未命中回落原名：
//! ① 别名静态映射：`wb_model_route.json.aliases`（精确匹配，大小写不敏感）；
//! ② 用户自定义通配规则：`wb_model_route.json.rules[].pattern`——零正则依赖，
//!    pattern 支持 `*`（任意串）与 `?`（单字符）通配符；
//! ③ 内置系列通配：claude-*/gemini-* → glm-5.3、gpt-* → deepseek-v4-pro、
//!    o1*/o3*/o4* → hy4（可被 ② 覆盖；命中目标必须存在于 WB 目录才生效）；
//! ④ 后缀检测：`-thinking`（内置）及 `suffixes[]`（可配置）——剥离后缀注入
//!    思考参数（effort），剥离后的基名必须命中目录。
//!
//! 降级目标（⑤，T5.6③ 后台任务识别）：`cheapest_catalog_model` 取目录内倍率
//! 最低的模型，供「标题/摘要类短请求」降级使用。
//!
//! 映射目标一律校验目录命中（catalog），未命中视为该级未命中继续下探——
//! 防止配置错误把请求打到上游不存在的模型。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 路由配置文件名（data/wb_model_route.json）
pub const ROUTE_FILE: &str = "wb_model_route.json";

/// 内置后缀：剥离后注入 reasoning_effort（§5.6 effort 降级链仍会按目录校验）
pub const BUILTIN_THINKING_SUFFIX: &str = "-thinking";
pub const BUILTIN_THINKING_EFFORT: &str = "high";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    pub pattern: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteSuffix {
    pub suffix: String,
    /// 剥离后缀后注入的 reasoning_effort（可空 = 仅改名不注入）
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WbRouteFile {
    /// ① 别名静态映射：客户端模型名 → 目录模型 id
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// ② 用户自定义通配规则（按声明顺序匹配）
    #[serde(default)]
    pub rules: Vec<RouteRule>,
    /// ④ 自定义后缀规则（在内置 -thinking 之后匹配）
    #[serde(default)]
    pub suffixes: Vec<RouteSuffix>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

/// 路由结果：resolved 模型 + 可选 effort 注入 + 命中级别
#[derive(Debug, Clone, PartialEq)]
pub struct RouteResult {
    pub model: String,
    pub effort_hint: Option<String>,
    /// direct / alias / rule / series / suffix
    pub stage: &'static str,
}

impl RouteResult {
    fn direct(model: String) -> Self {
        Self { model, effort_hint: None, stage: "direct" }
    }
}

/// 读取路由配置；缺失/损坏 → 空配置（四级中 ①②④ 用户部分退化为内置层）
pub fn load_config(data_dir: &Path) -> WbRouteFile {
    crate::fs_utils::read_json(&data_dir.join(ROUTE_FILE))
}

/// 内置系列通配（③）：知名闭源模型族 → 目录代表模型。
/// 仅在 ①② 未命中时参与；目标不在目录则跳过（见 resolve 校验）。
pub fn builtin_series() -> Vec<(&'static str, &'static str)> {
    vec![
        ("claude-*", "glm-5.3"),
        ("gemini-*", "glm-5.3"),
        ("gpt-*", "deepseek-v4-pro"),
        ("o1*", "hy4"),
        ("o3*", "hy4"),
        ("o4*", "hy4"),
    ]
}

/// 目录命中（大小写不敏感），命中返回目录规范 id
fn catalog_hit(catalog: &[super::wb_catalog::WbModel], model: &str) -> Option<String> {
    super::wb_catalog::find(catalog, model).map(|m| m.id.clone())
}

/// 通配匹配（零依赖）：`*` 任意串、`?` 单字符，大小写不敏感
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let s: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 四段路由解析（每级命中即止；目标不在目录视为该级未命中）
pub fn resolve(cfg: &WbRouteFile, catalog: &[super::wb_catalog::WbModel], requested: &str) -> RouteResult {
    let req = requested.trim().to_string();
    if req.is_empty() {
        return RouteResult::direct(req);
    }

    // ① 别名静态映射
    let alias_hit = cfg
        .aliases
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&req))
        .and_then(|(_, v)| catalog_hit(catalog, v));
    if let Some(target) = alias_hit {
        return RouteResult { model: target, effort_hint: None, stage: "alias" };
    }

    // ② 用户自定义通配规则
    for rule in &cfg.rules {
        if wildcard_match(&rule.pattern, &req) {
            if let Some(target) = catalog_hit(catalog, &rule.target) {
                return RouteResult { model: target, effort_hint: None, stage: "rule" };
            }
        }
    }

    // ③ 内置系列通配
    for (pattern, target) in builtin_series() {
        if wildcard_match(pattern, &req) {
            if let Some(t) = catalog_hit(catalog, target) {
                return RouteResult { model: t, effort_hint: None, stage: "series" };
            }
        }
    }

    // ④ 后缀检测：先内置 -thinking，再自定义 suffixes
    if let Some(base) = strip_suffix_ci(&req, BUILTIN_THINKING_SUFFIX) {
        if let Some(target) = catalog_hit(catalog, &base) {
            return RouteResult {
                model: target,
                effort_hint: Some(BUILTIN_THINKING_EFFORT.to_string()),
                stage: "suffix",
            };
        }
    }
    for s in &cfg.suffixes {
        if s.suffix.is_empty() {
            continue;
        }
        if let Some(base) = strip_suffix_ci(&req, &s.suffix) {
            if let Some(target) = catalog_hit(catalog, &base) {
                return RouteResult { model: target, effort_hint: s.effort.clone(), stage: "suffix" };
            }
        }
    }

    // 全未命中 → 原名回落（是否可服务由调用方目录判定）
    RouteResult::direct(req)
}

/// 大小写不敏感剥离后缀；剥离后为空返回 None
fn strip_suffix_ci(name: &str, suffix: &str) -> Option<String> {
    if suffix.is_empty() || name.len() <= suffix.len() {
        return None;
    }
    if name.to_lowercase().ends_with(&suffix.to_lowercase()) {
        let base = &name[..name.len() - suffix.len()];
        if base.trim().is_empty() {
            return None;
        }
        return Some(base.trim_end_matches(['-', '_']).to_string());
    }
    None
}

/// 目录内倍率最低的模型（后台任务降级目标，T5.6③/F-65）；
/// 全目录倍率相同/为空时取首个
pub fn cheapest_catalog_model(catalog: &[super::wb_catalog::WbModel]) -> Option<String> {
    catalog
        .iter()
        .min_by(|a, b| a.rate.partial_cmp(&b.rate).unwrap_or(std::cmp::Ordering::Equal))
        .map(|m| m.id.clone())
}

/// 后台任务识别（标题/摘要类短请求，T5.6③/F-65）：
/// max_tokens ≤ 128 且全部消息文本总长 ≤ 512 字符。
/// 保守启发式：仅影响显式开启 wb_bg_downgrade 后的路由，误判代价 = 换低成本模型。
pub fn is_background_task(body: &serde_json::Value) -> bool {
    let max_tokens = body
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| body.get("max_output_tokens").and_then(|v| v.as_u64()));
    let Some(mt) = max_tokens else { return false };
    if mt > 128 {
        return false;
    }
    let total_chars: usize = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .map(|msg| match msg.get("content") {
                    Some(serde_json::Value::String(s)) => s.chars().count(),
                    Some(serde_json::Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .map(|t| t.chars().count())
                        .sum(),
                    _ => 0,
                })
                .sum()
        })
        .unwrap_or(0);
    total_chars <= 512
}

/// 注入 effort 提示（T5.2 路由结果 → 请求体）：body 已带 reasoning_effort 时不覆盖
pub fn inject_effort_hint(body: &[u8], hint: &Option<String>) -> Vec<u8> {
    let Some(h) = hint.as_deref().filter(|s| !s.trim().is_empty()) else {
        return body.to_vec();
    };
    let mut obj: serde_json::Value = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(v) if v.is_object() => v,
        _ => return body.to_vec(),
    };
    if obj.get("reasoning_effort").and_then(|v| v.as_str()).is_some() {
        return body.to_vec();
    }
    obj["reasoning_effort"] = serde_json::json!(h.trim());
    serde_json::to_vec(&obj).unwrap_or_else(|_| body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::wb_catalog;
    use serde_json::json;

    fn catalog() -> Vec<wb_catalog::WbModel> {
        wb_catalog::builtin()
    }

    fn empty_cfg() -> WbRouteFile {
        WbRouteFile::default()
    }

    #[test]
    fn wildcard_matches_star_and_question() {
        assert!(wildcard_match("claude-*", "claude-sonnet-4"));
        assert!(wildcard_match("claude-*", "Claude-Sonnet-4"));
        assert!(wildcard_match("o1*", "o1-mini"));
        assert!(wildcard_match("model-?", "model-a"));
        assert!(!wildcard_match("model-?", "model-ab"));
        assert!(!wildcard_match("claude-*", "glm-5.3"));
        assert!(wildcard_match("*", "anything"));
    }

    #[test]
    fn direct_catalog_model_passes_through() {
        let r = resolve(&empty_cfg(), &catalog(), "glm-5.3");
        assert_eq!(r.stage, "direct");
        assert_eq!(r.model, "glm-5.3");
        assert!(r.effort_hint.is_none());
    }

    #[test]
    fn alias_maps_to_catalog_target() {
        let mut cfg = empty_cfg();
        cfg.aliases.insert("gpt-4o".into(), "GLM-5.3".into());
        let r = resolve(&cfg, &catalog(), "gpt-4o");
        assert_eq!(r.stage, "alias");
        assert_eq!(r.model, "glm-5.3");
        // 别名目标不在目录 → 视为未命中继续下探（内置系列 gpt-* → deepseek-v4-pro）
        let mut cfg2 = empty_cfg();
        cfg2.aliases.insert("gpt-4o".into(), "not-a-model".into());
        let r2 = resolve(&cfg2, &catalog(), "gpt-4o");
        assert_eq!(r2.stage, "series");
        assert_eq!(r2.model, "deepseek-v4-pro");
    }

    #[test]
    fn user_rules_override_builtin_series() {
        let mut cfg = empty_cfg();
        cfg.rules.push(RouteRule { pattern: "claude-*".into(), target: "kimi-k3-1".into() });
        let r = resolve(&cfg, &catalog(), "claude-sonnet-4");
        assert_eq!(r.stage, "rule");
        assert_eq!(r.model, "kimi-k3-1");
    }

    #[test]
    fn builtin_series_routes_families() {
        let r = resolve(&empty_cfg(), &catalog(), "claude-sonnet-4");
        assert_eq!(r.stage, "series");
        assert_eq!(r.model, "glm-5.3");
        let r = resolve(&empty_cfg(), &catalog(), "GPT-4o-mini");
        assert_eq!(r.model, "deepseek-v4-pro");
        let r = resolve(&empty_cfg(), &catalog(), "o3-mini");
        assert_eq!(r.model, "hy4");
    }

    #[test]
    fn thinking_suffix_strips_and_injects_effort() {
        let r = resolve(&empty_cfg(), &catalog(), "glm-5.3-thinking");
        assert_eq!(r.stage, "suffix");
        assert_eq!(r.model, "glm-5.3");
        assert_eq!(r.effort_hint.as_deref(), Some("high"));
        // 基名不在目录 → 不命中（原样回落）
        let r = resolve(&empty_cfg(), &catalog(), "not-a-model-thinking");
        assert_eq!(r.stage, "direct");
        assert_eq!(r.model, "not-a-model-thinking");
    }

    #[test]
    fn custom_suffix_rules_apply_after_builtin() {
        let mut cfg = empty_cfg();
        cfg.suffixes.push(RouteSuffix { suffix: "-xhigh".into(), effort: Some("medium".into()) });
        let r = resolve(&cfg, &catalog(), "hy4-xhigh");
        assert_eq!(r.stage, "suffix");
        assert_eq!(r.model, "hy4");
        assert_eq!(r.effort_hint.as_deref(), Some("medium"));
    }

    #[test]
    fn unknown_model_falls_back_to_requested_name() {
        let r = resolve(&empty_cfg(), &catalog(), "totally-unknown");
        assert_eq!(r.stage, "direct");
        assert_eq!(r.model, "totally-unknown");
        assert!(r.effort_hint.is_none());
    }

    #[test]
    fn cheapest_model_is_lowest_rate() {
        let c = cheapest_catalog_model(&catalog()).unwrap();
        assert_eq!(c, "hy4-preview"); // rate 0.00
    }

    #[test]
    fn background_task_detection() {
        // 标题/摘要类短请求：小 max_tokens + 短文本
        let bg = json!({"max_tokens": 64, "messages":[{"role":"user","content":"总结一下"}]});
        assert!(is_background_task(&bg));
        // 大 max_tokens → 非后台
        let big = json!({"max_tokens": 4096, "messages":[{"role":"user","content":"短"}]});
        assert!(!is_background_task(&big));
        // 小 max_tokens 但长文本 → 非后台
        let long = json!({"max_tokens": 64, "messages":[{"role":"user","content":"x".repeat(600)}]});
        assert!(!is_background_task(&long));
        // 缺 max_tokens → 非后台
        let none = json!({"messages":[{"role":"user","content":"hi"}]});
        assert!(!is_background_task(&none));
        // content blocks 形态
        let blocks = json!({"max_tokens": 100, "messages":[{"role":"user","content":[{"type":"text","text":"短"}]}]});
        assert!(is_background_task(&blocks));
    }

    #[test]
    fn effort_hint_injection_respects_existing() {
        let body = serde_json::to_vec(&json!({"model":"m","messages":[]})).unwrap();
        let out = inject_effort_hint(&body, &Some("high".into()));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], json!("high"));
        // 已有 effort 不覆盖
        let body2 = serde_json::to_vec(&json!({"reasoning_effort":"low","messages":[]})).unwrap();
        let out2 = inject_effort_hint(&body2, &Some("high".into()));
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2["reasoning_effort"], json!("low"));
        // None 提示原样返回
        assert_eq!(inject_effort_hint(&body, &None), body);
    }
}
