//! 统一思考档位（issue #31）
//!
//! 对外统一 OpenAI 风格档位（低→高）：`low < medium < high < xhigh`，
//! 下层池差异在边界适配：
//! - Trae wire 档位（2026-09 客户端实证，issue #31）：`light < high < extra_high`
//! - Buddy wire 档位：即统一档位（上游 OpenAI 兼容），恒等映射
//!
//! 两条口径：
//! - 声明（/v1/models、聚合目录）：各池支持的档位映射回统一空间后取**并集**；
//!   无实证数据的模型声明为空数组（诚实缺省，不按名称猜测）
//! - 请求（出口转换）：调度命中池后把统一档位转为该池 wire 值；不受支持时按
//!   「≤ 请求值的最大支持档位，否则最低档」降级；无实证数据不下发（保持现状）

/// 统一档位序（与 wb_catalog::resolve_effort 既有降级链一致，含宽容入参）
pub const UNIFIED_ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// Trae wire 档位序（低→高，实证）
pub const TRAE_ORDER: [&str; 3] = ["light", "high", "extra_high"];

/// 统一档位序位（未知值 → None，调用方按需兜底）
pub fn unified_rank(e: &str) -> Option<usize> {
    UNIFIED_ORDER.iter().position(|o| *o == e)
}

/// Trae wire 档位序位（未知值 → None）
pub fn trae_rank(e: &str) -> Option<usize> {
    TRAE_ORDER.iter().position(|o| *o == e)
}

/// 统一 → Trae wire（越界钳制两端；未知值按最低档，保守）。
/// 入口先 trim + 小写归一：外部 explicit 可能传 "High"/"HIGH" 等大小写变体，
/// 归一后命中 high 而非静默落最低档 light（内部 route_hint 本为小写，幂等）
pub fn unified_to_trae(unified: &str) -> &'static str {
    match unified.trim().to_lowercase().as_str() {
        "xhigh" | "max" => "extra_high",
        "high" => "high",
        _ => "light",
    }
}

/// Trae wire → 统一；未知 wire 值 → None（声明并集时丢弃，不猜测）
pub fn trae_to_unified(wire: &str) -> Option<&'static str> {
    match wire {
        "light" => Some("low"),
        "high" => Some("high"),
        "extra_high" => Some("xhigh"),
        _ => None,
    }
}

// ==================== L3 实证表（Trae，替代名称推断） ====================

/// Trae 思考档位实证表（issue #31，2026-09 客户端实测）：
/// 所列模型 wire 档位均为 light / high / extra_high。表外模型 = 无实证 →
/// 声明空数组、请求不下发，等抓包/官网披露补充（勿按名称推断，issue #31 教训）。
pub const TRAE_EFFORTS_REF: [(&str, &[&str]); 4] = [
    ("glm-5.3", &["light", "high", "extra_high"]),
    ("glm-5.2", &["light", "high", "extra_high"]),
    ("kimi-k3", &["light", "high", "extra_high"]),
    ("qwen3.8-max", &["light", "high", "extra_high"]),
];

/// Trae 模型支持的 wire 档位（canonical 查表；无实证 → 空数组）
pub fn trae_supported_wire(canonical: &str) -> Vec<String> {
    TRAE_EFFORTS_REF
        .iter()
        .find(|(id, _)| *id == canonical)
        .map(|(_, list)| list.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

/// Trae 池声明（统一空间）：实证 wire 档位映射回统一档位，升序去重
pub fn trae_declared_unified(canonical: &str) -> Vec<String> {
    let mut out: Vec<String> = trae_supported_wire(canonical)
        .iter()
        .filter_map(|w| trae_to_unified(w).map(str::to_string))
        .collect();
    out.sort_by_key(|e| unified_rank(e).unwrap_or(usize::MAX));
    out.dedup();
    out
}

// ==================== L3 文档参考表（Trae Max Mode，issue #31 T4.1） ====================

/// Trae Max Mode 支持表（2026-09 T0.3 客户端实证 + docs.trae.cn/ide_max-mode）：
/// Max Mode 为请求级布尔字段 `is_max_mode:1` 注入（客户端门控表达式
/// `is_max_mode:(…)&&t.isMaxMode?1:0`），非模型名变体；支持集取官方文档
/// Max Mode 1M 上下文档（原 unified_catalog::MAX_MODE_1M 占位迁入，单一事实源）。
/// 表外模型不注入（未知字段不冒进，与 effort 实证表同规）
pub const TRAE_MAX_MODE_REF: [&str; 11] = [
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

/// Trae 模型是否支持 Max Mode（canonical 查表；表外 → false 不注入）
pub fn trae_max_mode_supported(canonical: &str) -> bool {
    TRAE_MAX_MODE_REF.contains(&canonical)
}

// ==================== 降级与并集 ====================

/// 通用降级（自 wb_catalog::resolve_effort 抽出泛化）：适用于 supported 非空且
/// 不含 requested 的场景，返回「≤ 请求值的最大支持档位，否则最低档」。
/// `rank` 给出该空间的档位序（未知值按 usize::MAX，与既有语义一致）。
pub fn downgrade_by<F>(requested: &str, supported: &[String], rank: F) -> String
where
    F: Fn(&str) -> Option<usize>,
{
    let rk = |e: &str| rank(e).unwrap_or(usize::MAX);
    let req_rank = rk(requested);
    supported
        .iter()
        .filter(|e| rk(e) <= req_rank)
        .max_by_key(|e| rk(e))
        .or_else(|| supported.iter().min_by_key(|e| rk(e)))
        .cloned()
        .unwrap_or_else(|| requested.to_string())
}

/// 多源声明并集（统一空间）：去空 + 去重 + 按统一档位序升序（未知值排末尾）
pub fn declared_union(lists: &[Vec<String>]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for l in lists {
        for e in l {
            if !e.is_empty() && seen.insert(e.clone()) {
                out.push(e.clone());
            }
        }
    }
    out.sort_by_key(|e| unified_rank(e).unwrap_or(usize::MAX));
    out
}

// ==================== 请求出口转换（Trae 池） ====================

/// 统一档位请求 → Trae wire 档位（调度命中 Trae 池后调用）。
/// - 未请求（None/空）→ None（不下发，走上游默认，与 Buddy 语义一致）
/// - 无实证数据（supported 空）→ None（不下发，保持既有行为）
/// - 映射后精确命中 → 该 wire 值；否则按降级链取兼容值
pub fn trae_request_wire(requested: Option<&str>, supported_wire: &[String]) -> Option<String> {
    let req = requested.map(str::trim).filter(|s| !s.is_empty())?;
    if supported_wire.is_empty() {
        return None;
    }
    let wire = unified_to_trae(req);
    if supported_wire.iter().any(|e| e == wire) {
        return Some(wire.to_string());
    }
    Some(downgrade_by(wire, supported_wire, trae_rank))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t01_rank_orders() {
        assert_eq!(unified_rank("low"), Some(1));
        assert_eq!(unified_rank("xhigh"), Some(4));
        assert_eq!(unified_rank("bogus"), None);
        assert_eq!(trae_rank("light"), Some(0));
        assert_eq!(trae_rank("extra_high"), Some(2));
        assert_eq!(trae_rank("medium"), None);
    }

    #[test]
    fn t02_unified_trae_mapping() {
        assert_eq!(unified_to_trae("low"), "light");
        assert_eq!(unified_to_trae("medium"), "light"); // 降级兼容
        assert_eq!(unified_to_trae("high"), "high");
        assert_eq!(unified_to_trae("xhigh"), "extra_high");
        assert_eq!(unified_to_trae("minimal"), "light");
        assert_eq!(unified_to_trae("max"), "extra_high");
        // 大小写/空白归一（外部 explicit 变体不静默落 light）
        assert_eq!(unified_to_trae("HIGH"), "high");
        assert_eq!(unified_to_trae(" MAX "), "extra_high");
        assert_eq!(unified_to_trae("Bogus"), "light");
        assert_eq!(trae_to_unified("light"), Some("low"));
        assert_eq!(trae_to_unified("high"), Some("high"));
        assert_eq!(trae_to_unified("extra_high"), Some("xhigh"));
        assert_eq!(trae_to_unified("bogus"), None);
    }

    /// 实证表：4 个模型 → 统一档位 [low, high, xhigh]；表外 → 空
    #[test]
    fn t03_trae_declared() {
        for id in ["glm-5.3", "glm-5.2", "kimi-k3", "qwen3.8-max"] {
            assert_eq!(
                trae_declared_unified(id),
                vec!["low".to_string(), "high".to_string(), "xhigh".to_string()],
                "{id}"
            );
        }
        assert!(trae_declared_unified("glm-5.3-flash").is_empty());
        assert!(trae_supported_wire("unknown-model").is_empty());
    }

    /// 降级链：≤ 请求值的最大支持档位，否则最低档（自 wb resolve_effort 泛化）
    #[test]
    fn t04_downgrade_chain() {
        let sup: Vec<String> = ["low", "medium", "high"].iter().map(|s| s.to_string()).collect();
        assert_eq!(downgrade_by("xhigh", &sup, unified_rank), "high");
        assert_eq!(downgrade_by("medium", &sup, unified_rank), "medium");
        // 请求值低于全部支持档位 → 最低档
        assert_eq!(downgrade_by("minimal", &sup, unified_rank), "low");
        let hi: Vec<String> = ["high"].iter().map(|s| s.to_string()).collect();
        assert_eq!(downgrade_by("low", &hi, unified_rank), "high"); // 兜底最低档
        let wire: Vec<String> = ["light", "high", "extra_high"].iter().map(|s| s.to_string()).collect();
        assert_eq!(downgrade_by("extra_high", &wire, trae_rank), "extra_high");
        assert_eq!(downgrade_by("high", &wire, trae_rank), "high");
    }

    /// 出口转换：未请求 / 无实证 → None；支持 → 原值；medium 降级 light
    #[test]
    fn t05_trae_request_wire() {
        let sup: Vec<String> = ["light", "high", "extra_high"].iter().map(|s| s.to_string()).collect();
        assert_eq!(trae_request_wire(None, &sup), None);
        assert_eq!(trae_request_wire(Some("  "), &sup), None);
        assert_eq!(trae_request_wire(Some("high"), &sup), Some("high".into()));
        assert_eq!(trae_request_wire(Some("xhigh"), &sup), Some("extra_high".into()));
        assert_eq!(trae_request_wire(Some("medium"), &sup), Some("light".into()));
        assert_eq!(trae_request_wire(Some("low"), &sup), Some("light".into()));
        // 无实证模型不下发
        assert_eq!(trae_request_wire(Some("high"), &[]), None);
    }

    /// 并集：去空去重 + 统一序升序；空输入 → 空
    #[test]
    fn t06_declared_union() {
        let a = vec!["high".to_string(), "low".to_string()];
        let b = vec!["medium".to_string(), "low".to_string(), "".to_string()];
        assert_eq!(
            declared_union(&[a, b]),
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        );
        assert!(declared_union(&[vec![], vec![]]).is_empty());
    }

    /// Max Mode 支持表：表内命中；表外/空串不命中
    #[test]
    fn t07_max_mode_supported() {
        assert!(trae_max_mode_supported("glm-5.3"));
        assert!(trae_max_mode_supported("qwen3.8-max"));
        assert!(trae_max_mode_supported("doubao-seed-evolving"));
        assert!(!trae_max_mode_supported("doubao-seed-code"));
        assert!(!trae_max_mode_supported("glm-5.3-flash"));
        assert!(!trae_max_mode_supported(""));
    }
}
