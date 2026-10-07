//! WB 手工配置的程序化管理命令（模型路由 / 审核模板映射）
//!
//! 此前 `wb_model_route.json`（模型四级路由的 ①②④ 可配置层）与
//! `wb_template_map.json`（审核模板黑名单热更新，§5.5 #9）只能手工编辑 JSON，
//! 这里提供 get/set 命令供设置页读写：
//! - get：读 data/ 新路径，缺失回退旧根路径（存量用户数据兼容），仍缺失返回
//!   空默认结构（与 wb_model_route::load_config / wb_route::load_templates 的
//!   读取语义一致）；
//! - set：先做结构校验（对齐读取方 serde 反序列化形状，失败返回中文错误），
//!   通过后 fs_utils::write_json 落盘 data/ 新路径（写后逐出读缓存，网关
//!   热路径立即生效）。

use serde_json::Value;

use crate::api_server::wb_model_route::{self, WbRouteFile};
use crate::api_server::wb_payload::TemplateMapFile;
use crate::state::AppState;

// ==================== 模型路由配置（data/wb_model_route.json） ====================

pub fn wb_route_config_get(state: &AppState) -> Result<Value, String> {
    route_config_get_at(&state.data_dir)
}

pub fn wb_route_config_set(state: &AppState, config: Value) -> Result<(), String> {
    route_config_set_at(&state.data_dir, &config)
}

fn route_config_get_at(data_dir: &std::path::Path) -> Result<Value, String> {
    let cfg = wb_model_route::load_config(data_dir);
    serde_json::to_value(cfg).map_err(|e| format!("序列化模型路由配置失败: {e}"))
}

fn route_config_set_at(data_dir: &std::path::Path, config: &Value) -> Result<(), String> {
    validate_route_config(config)?;
    // SQLite 化（P2）：data/wb_model_route.json → kv `wb_model_route`（写后网关热路径立即生效）
    let r = crate::store::db(data_dir).kv_set("wb_model_route", config);
    if r.is_ok() {
        // 写路径显式失效（批次 A）：路由配置改动即时生效
        crate::api_server::config_cache::invalidate(data_dir, "wb_model_route");
    }
    r
}

/// 结构校验：形状必须与 wb_model_route::load_config 的反序列化（WbRouteFile）
/// 对齐——aliases 为「模型名 → 目录模型 id」映射（object），rules 为
/// [{pattern,target}] 数组，suffixes 为 [{suffix,effort?}] 数组。
/// 校验失败必须拒绝落盘：否则网关读取时静默退回默认配置，用户改动悄然失效
fn validate_route_config(config: &Value) -> Result<(), String> {
    if !config.is_object() {
        return Err("模型路由配置必须是 JSON 对象（含 aliases/rules/suffixes 字段）".into());
    }
    serde_json::from_value::<WbRouteFile>(config.clone())
        .map(|_| ())
        .map_err(|e| format!("模型路由配置格式不正确（aliases 应为映射，rules/suffixes 应为对象数组）: {e}"))
}

// ==================== 审核模板映射（data/wb_template_map.json） ====================

pub fn wb_template_map_get(state: &AppState) -> Result<Value, String> {
    template_map_get_at(&state.data_dir)
}

pub fn wb_template_map_set(state: &AppState, map: Value) -> Result<(), String> {
    template_map_set_at(&state.data_dir, &map)
}

fn template_map_get_at(data_dir: &std::path::Path) -> Result<Value, String> {
    // SQLite 化（P2）：kv `wb_template_map`。
    // 无自定义（KV 缺失或 templates 空 = load_templates 走内置默认）时回显内置
    // 默认规则并标记 builtin:true——前端「指纹清洗」tab 以此展示生效中规则；
    // 保存后才成为独立自定义副本（整体替换语义，与 into_rules 口径一致）
    let file: TemplateMapFile = crate::store::db(data_dir).kv_get("wb_template_map");
    if file.templates.is_empty() {
        let templates: Vec<serde_json::Value> = crate::api_server::wb_payload::default_template_map()
            .into_iter()
            .map(|(from, to)| serde_json::json!({"from": from, "to": to}))
            .collect();
        return Ok(serde_json::json!({ "templates": templates, "updated_at": null, "builtin": true }));
    }
    let mut v = serde_json::to_value(&file).map_err(|e| format!("序列化审核模板映射失败: {e}"))?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("builtin".into(), serde_json::Value::Bool(false));
    }
    Ok(v)
}

/// 结构校验：对齐 wb_route::load_templates 的读取形状（TemplateMapFile）——
/// templates 为 [{from,to}] 数组（两字段均为必填字符串），updated_at 可空。
/// 软上限守卫（审查 minor）：规则表每请求做 O(n) 子串替换且整体落 KV，
/// 不设上限时管理员可存近 2MB 规则表构成网关自伤型性能面——条数 200 /
/// 单条 4KB 远超实际使用（内置默认约 10 条短句），超限拒绝落盘。
const TEMPLATE_MAP_MAX_RULES: usize = 200;
const TEMPLATE_MAP_MAX_FIELD_BYTES: usize = 4 * 1024;

fn template_map_set_at(data_dir: &std::path::Path, map: &Value) -> Result<(), String> {
    if !map.is_object() {
        return Err("审核模板映射必须是 JSON 对象（含 templates 数组）".into());
    }
    serde_json::from_value::<TemplateMapFile>(map.clone())
        .map_err(|e| format!("审核模板映射格式不正确（templates[].from/to 均为必填字符串）: {e}"))?;
    let templates = map["templates"].as_array().cloned().unwrap_or_default();
    if templates.len() > TEMPLATE_MAP_MAX_RULES {
        return Err(format!(
            "审核模板映射规则数超上限（{TEMPLATE_MAP_MAX_RULES} 条），当前 {} 条",
            templates.len()
        ));
    }
    for (i, t) in templates.iter().enumerate() {
        for key in ["from", "to"] {
            if t[key].as_str().map(|s| s.len()) > Some(TEMPLATE_MAP_MAX_FIELD_BYTES) {
                return Err(format!(
                    "审核模板映射第 {} 条的 {key} 超长（上限 {TEMPLATE_MAP_MAX_FIELD_BYTES} 字节）",
                    i + 1
                ));
            }
        }
    }
    crate::store::db(data_dir).kv_set("wb_template_map", map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("wb_config_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 无自定义（空 KV）时回显内置默认规则 + builtin 标志（指纹清洗 tab 展示口径）
    #[test]
    fn template_map_get_falls_back_to_builtin_defaults() {
        let dir = tmp_dir("tpl_builtin");
        let got = template_map_get_at(&dir).unwrap();
        assert_eq!(got["builtin"], json!(true));
        let arr = got["templates"].as_array().expect("默认规则数组");
        assert!(!arr.is_empty(), "内置默认规则非空");
        assert!(arr[0]["from"].is_string() && arr[0]["to"].is_string());
        // 自定义后 builtin 翻转为 false
        template_map_set_at(&dir, &json!({"templates": [{"from": "x", "to": "y"}]})).unwrap();
        let got2 = template_map_get_at(&dir).unwrap();
        assert_eq!(got2["builtin"], json!(false));
        assert_eq!(got2["templates"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「恢复内置默认」链路：保存空 templates（UI 恢复默认按钮的落盘形状）
    /// → load_templates 走 into_rules→None→default，get 回显 builtin:true 全量默认
    #[test]
    fn template_map_set_empty_restores_builtin_defaults() {
        let dir = tmp_dir("tpl_reset");
        // 先写一份自定义
        template_map_set_at(&dir, &json!({"templates": [{"from": "x", "to": "y"}]})).unwrap();
        assert_eq!(template_map_get_at(&dir).unwrap()["builtin"], json!(false));
        // 保存空数组 = 恢复默认（FingerprintSanitizePanel::resetToBuiltin 的落盘形状）
        template_map_set_at(&dir, &json!({"templates": [], "updated_at": 1})).unwrap();
        let got = template_map_get_at(&dir).unwrap();
        assert_eq!(got["builtin"], json!(true));
        let n = got["templates"].as_array().unwrap().len();
        assert_eq!(
            n,
            crate::api_server::wb_payload::default_template_map().len(),
            "回显规则数应与内置默认一致"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 校验拒绝畸形路由结构（aliases 传数组 / rules 元素缺 target）
    #[test]
    fn route_config_validation_rejects_malformed() {
        assert!(validate_route_config(&json!({"aliases": ["a"]})).is_err());
        assert!(validate_route_config(&json!({"rules": [{"pattern": "x"}]})).is_err());
        assert!(validate_route_config(&json!({"suffixes": [{"suffix": "-t", "effort": 1}]})).is_err());
        assert!(validate_route_config(&json!("not an object")).is_err());
        // 合法最小结构放行（各字段均可缺省）
        assert!(validate_route_config(&json!({})).is_ok());
        assert!(validate_route_config(&json!({"aliases": {"claude-x": "glm-5.3"}})).is_ok());
    }

    /// 校验拒绝畸形模板映射（templates 元素缺 to / 非对象根）
    #[test]
    fn template_map_validation_rejects_malformed() {
        let dir = tmp_dir("tpl_invalid");
        assert!(template_map_set_at(&dir, &json!({"templates": [{"from": "a"}]})).is_err());
        assert!(template_map_set_at(&dir, &json!([1, 2])).is_err());
        // 拒绝后不得落库
        assert!(crate::store::db(&dir).kv_get_raw("wb_template_map").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 软上限守卫：规则数超 200 / 单条超 4KB 拒绝落盘（审查 minor）
    #[test]
    fn template_map_validation_rejects_oversized() {
        let dir = tmp_dir("tpl_oversize");
        // 条数超限
        let many: Vec<_> = (0..201)
            .map(|i| json!({"from": format!("f{i}"), "to": "t"}))
            .collect();
        assert!(template_map_set_at(&dir, &json!({ "templates": many })).is_err());
        // 单条字段超长（from 4097 字节）
        let long = "x".repeat(TEMPLATE_MAP_MAX_FIELD_BYTES + 1);
        assert!(template_map_set_at(
            &dir,
            &json!({"templates": [{"from": long, "to": "t"}]})
        )
        .is_err());
        // 边界内放行（恰好 200 条 + 4KB 字段）
        let ok: Vec<_> = (0..TEMPLATE_MAP_MAX_RULES)
            .map(|i| json!({"from": format!("f{i}"), "to": "t"}))
            .collect();
        template_map_set_at(&dir, &json!({ "templates": ok })).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写后可读回（SQLite 化 P2：kv `wb_template_map`，get/set 走同一读取语义）
    #[test]
    fn template_map_set_then_get_roundtrip() {
        let dir = tmp_dir("tpl_roundtrip");
        let map = json!({"templates": [{"from": "a", "to": "b"}], "updated_at": 123});
        template_map_set_at(&dir, &map).unwrap();
        assert!(crate::store::db(&dir).kv_get_raw("wb_template_map").is_some());
        let got = template_map_get_at(&dir).unwrap();
        assert_eq!(got["templates"][0]["from"], json!("a"));
        assert_eq!(got["templates"][0]["to"], json!("b"));
        assert_eq!(got["updated_at"], json!(123));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 路由配置写后可读回；空目录 get 返回空默认结构（与 load_config 语义一致）
    #[test]
    fn route_config_set_then_get_roundtrip() {
        let dir = tmp_dir("route_roundtrip");
        let cfg = json!({"aliases": {"claude-x": "glm-5.3"}, "rules": [], "suffixes": []});
        route_config_set_at(&dir, &cfg).unwrap();
        assert!(crate::store::db(&dir).kv_get_raw("wb_model_route").is_some());
        let got = route_config_get_at(&dir).unwrap();
        assert_eq!(got["aliases"]["claude-x"], json!("glm-5.3"));

        let empty = tmp_dir("route_empty");
        let def = route_config_get_at(&empty).unwrap();
        assert_eq!(def["aliases"], json!({}));
        assert!(def["rules"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
