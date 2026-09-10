//! WorkBuddy 模型目录（T2.1/F-37 静态兜底层）
//!
//! §5.6 原则：
//! - 静态兜底目录落 `wb_model_catalog.json`（缺失时以本文件内置表写入）；
//! - 能力声明永远读上游字段（inputModalities/supportedEfforts），勿硬编码——
//!   本表仅作上游目录接口（`GET {chatBase}/console/enterprises/personal/models`，
//!   批次 4 F-37 动态替换）不可用时的兜底；
//! - `effort_override` 为实测修正层（hy3 系列仅 high 真正生效，v1.2），
//!   优先级：修正层 > 客户端请求 > 上游默认。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 单个模型的能力声明
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WbModel {
    /// 模型 id（客户端请求的 model 字段，小写）
    pub id: String,
    /// 展示名
    #[serde(default)]
    pub display: String,
    /// 上下文长度（token）
    #[serde(default)]
    pub context_length: u64,
    /// 单次最大输出
    #[serde(default)]
    pub max_tokens: u64,
    /// 是否支持图片模态（读上游 inputModalities，勿拍脑袋）
    #[serde(default)]
    pub supports_image: bool,
    /// 支持的 reasoning_effort 档位（升序）
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    /// 实测修正层：按模型族强制映射（如 "high"）
    #[serde(default)]
    pub effort_override: Option<String>,
    /// 积分倍率（展示用，成本模型按无缓存估算 §5.5 #10）
    #[serde(default)]
    pub rate: f64,
}

impl WbModel {
    /// reasoning_effort 降级：请求档位不受支持时按「≤ 请求值的最大支持档位，
    /// 否则最低档位」降级（§5.5 #3）
    pub fn resolve_effort(&self, requested: Option<&str>) -> Option<String> {
        // 修正层最优先（v1.2 §5.6）
        if let Some(o) = &self.effort_override {
            if !o.is_empty() {
                return Some(o.clone());
            }
        }
        if self.supported_efforts.is_empty() {
            return requested.map(str::to_string);
        }
        let req = match requested {
            Some(r) if !r.trim().is_empty() => r.trim().to_lowercase(),
            _ => return None, // 未请求 → 不下发，走上游默认
        };
        if self.supported_efforts.iter().any(|e| e == &req) {
            return Some(req);
        }
        let order = ["minimal", "low", "medium", "high", "xhigh", "max"];
        let rank = |e: &str| order.iter().position(|o| *o == e).unwrap_or(usize::MAX);
        let req_rank = rank(&req);
        // ≤ 请求档位的最大支持档位；都没有则最低档
        self.supported_efforts
            .iter()
            .filter(|e| rank(e) <= req_rank)
            .max_by_key(|e| rank(e))
            .or_else(|| {
                self.supported_efforts
                    .iter()
                    .min_by_key(|e| rank(e))
            })
            .cloned()
    }
}

/// 内置静态兜底目录（15 模型，§5.6；倍率/上下文为 2026-09 快照）
pub fn builtin() -> Vec<WbModel> {
    let m = |id: &str,
             display: &str,
             ctx: u64,
             mt: u64,
             img: bool,
             efforts: &[&str],
             ov: Option<&str>,
             rate: f64| {
        WbModel {
            id: id.to_string(),
            display: display.to_string(),
            context_length: ctx,
            max_tokens: mt,
            supports_image: img,
            supported_efforts: efforts.iter().map(|s| s.to_string()).collect(),
            effort_override: ov.map(str::to_string),
            rate,
        }
    };
    vec![
        m("hy4-preview", "Hy4 Preview", 1_000_000, 128_000, true, &["low", "medium", "high"], None, 0.00),
        m("hy4", "Hy4", 1_000_000, 128_000, true, &["low", "medium", "high"], None, 0.20),
        m("hy3-x", "Hy3-X", 200_000, 64_000, false, &["high"], Some("high"), 0.05),
        m("hy3", "Hy3", 200_000, 64_000, false, &["high"], Some("high"), 0.05),
        m("glm-5.3", "GLM-5.3", 200_000, 96_000, true, &["low", "medium", "high"], None, 0.79),
        m("glm-5.3-flash", "GLM-5.3 Flash", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.10),
        m("glm-5.2", "GLM-5.2", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.30),
        m("glm-5", "GLM-5", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.20),
        m("kimi-k3-1", "Kimi K3.1", 256_000, 64_000, true, &["medium", "high"], None, 1.62),
        m("kimi-k3", "Kimi K3", 256_000, 64_000, false, &["medium", "high"], None, 0.90),
        m("deepseek-v4-flash", "DeepSeek V4 Flash", 168_000, 32_000, false, &["low", "medium"], None, 0.17),
        m("deepseek-v4-pro", "DeepSeek V4 Pro", 168_000, 64_000, false, &["medium", "high"], None, 1.10),
        m("minimax-m3", "MiniMax M3", 200_000, 64_000, false, &["low", "medium", "high"], None, 0.55),
        m("qwen3.8-max", "Qwen3.8 Max", 262_000, 64_000, true, &["low", "medium", "high"], None, 0.85),
        m("qwen-3.7-plus", "Qwen 3.7 Plus", 131_000, 32_000, false, &["low", "medium", "high"], None, 0.25),
    ]
}

/// 目录文件路径（data/wb_model_catalog.json，§3.8）
pub fn catalog_path(data_dir: &Path) -> PathBuf {
    data_dir.join("wb_model_catalog.json")
}

/// 目录文件结构（支持上游动态替换后的全量覆盖）
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct WbCatalogFile {
    #[serde(default)]
    pub models: Vec<WbModel>,
    #[serde(default)]
    pub fetched_at: Option<i64>,
}

/// 加载目录：wb_model_catalog.json 优先（动态替换结果 / 人工维护），
/// 缺失或为空时落盘内置表并返回内置表
pub fn load(data_dir: &Path) -> Vec<WbModel> {
    let path = catalog_path(data_dir);
    let file: WbCatalogFile = crate::fs_utils::read_json(&path);
    if !file.models.is_empty() {
        return file.models;
    }
    let builtin = builtin();
    let _ = crate::fs_utils::write_json(
        &path,
        &WbCatalogFile {
            models: builtin.clone(),
            fetched_at: None,
        },
    );
    builtin
}

/// 大小写不敏感查找
pub fn find<'a>(catalog: &'a [WbModel], model: &str) -> Option<&'a WbModel> {
    let lower = model.trim().to_lowercase();
    catalog.iter().find(|m| m.id == lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_has_15_models_and_unique_ids() {
        let c = builtin();
        assert_eq!(c.len(), 15);
        let mut ids: Vec<&str> = c.iter().map(|m| m.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 15);
    }

    #[test]
    fn hy3_effort_override_forces_high() {
        let c = builtin();
        let hy3 = find(&c, "hy3-x").unwrap();
        // 客户端请求 medium/xhigh 均被修正层强制为 high
        assert_eq!(hy3.resolve_effort(Some("medium")).as_deref(), Some("high"));
        assert_eq!(hy3.resolve_effort(Some("xhigh")).as_deref(), Some("high"));
        assert_eq!(hy3.resolve_effort(None).as_deref(), Some("high"));
    }

    #[test]
    fn effort_downgrade_picks_closest_supported() {
        let c = builtin();
        // deepseek-v4-flash 仅支持 low/medium：请求 high → 降级 medium
        let ds = find(&c, "deepseek-v4-flash").unwrap();
        assert_eq!(ds.resolve_effort(Some("high")).as_deref(), Some("medium"));
        assert_eq!(ds.resolve_effort(Some("low")).as_deref(), Some("low"));
        // 请求不支持的最小档以下 → 最低档
        assert_eq!(ds.resolve_effort(Some("minimal")).as_deref(), Some("low"));
        // 未请求 → 不下发
        assert_eq!(ds.resolve_effort(None), None);
    }

    #[test]
    fn find_is_case_insensitive_and_unknown_returns_none() {
        let c = builtin();
        assert!(find(&c, "GLM-5.3").is_some());
        assert!(find(&c, "glm-5.3").is_some());
        assert!(find(&c, "not-a-model").is_none());
    }
}
