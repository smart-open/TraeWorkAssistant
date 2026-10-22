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
use tauri::State;

use crate::api_server::wb_model_route::{self, WbRouteFile};
use crate::api_server::wb_payload::TemplateMapFile;
use crate::state::AppState;

// ==================== 模型路由配置（data/wb_model_route.json） ====================

#[tauri::command]
pub fn wb_route_config_get(state: State<AppState>) -> Result<Value, String> {
    route_config_get_at(&state.data_dir)
}

#[tauri::command]
pub fn wb_route_config_set(state: State<AppState>, config: Value) -> Result<(), String> {
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

// ==================== 审核模板映射（SQLite kv，按档位分区：wb / buddy-ai） ====================

/// 模板表 KV 键按档位解析（issue #129：§4.3 分区清洗——buddy-ai 国际档独立
/// 键，此前 get/set 硬编码 wb 键导致指纹清洗 tab 对国际档完全不生效、样本
/// 「一键生成规则」闭环在国际档断裂）：None/"wb" → wb_template_map（wb 池与
/// Trae 池共用）；"buddy-ai" → buddy_ai_template_map；其余档位拒绝落库
fn template_kv_key(tier: Option<&str>) -> Result<&'static str, String> {
    match tier {
        None | Some("wb") => Ok("wb_template_map"),
        Some("buddy-ai") => Ok("buddy_ai_template_map"),
        Some(other) => Err(format!("未知清洗规则档位: {other}")),
    }
}

#[tauri::command]
pub fn wb_template_map_get(state: State<AppState>, tier: Option<String>) -> Result<Value, String> {
    template_map_get_at(&state.data_dir, tier.as_deref())
}

#[tauri::command]
pub fn wb_template_map_set(
    state: State<AppState>,
    tier: Option<String>,
    map: Value,
) -> Result<(), String> {
    template_map_set_at(&state.data_dir, tier.as_deref(), &map)
}

fn template_map_get_at(data_dir: &std::path::Path, tier: Option<&str>) -> Result<Value, String> {
    // SQLite 化（P2）：kv `wb_template_map`；buddy-ai 档独立键（issue #129）。
    // 无自定义（KV 缺失或 templates 空 = load_templates 走纯默认）时回显内置
    // 默认规则并标记 builtin:true——前端「指纹清洗」tab 以此展示生效中规则；
    // 保存后成为自定义覆盖表（合并语义：默认表全量生效，自定义按 from 覆盖/
    // 追加，to 空串 = 剥离、to null/缺省 = 停用该规则；与 merged_template_rules
    // 口径一致）。内置默认表两档共享，自定义覆盖表按档位隔离
    let key = template_kv_key(tier)?;
    let file: TemplateMapFile = crate::store::db(data_dir).kv_get(key);
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
/// templates 为 [{from,to?}] 数组（from 必填字符串；to 可空：字符串/空串=剥离/
/// null 或缺省=停用），updated_at 可空
fn template_map_set_at(
    data_dir: &std::path::Path,
    tier: Option<&str>,
    map: &Value,
) -> Result<(), String> {
    let key = template_kv_key(tier)?;
    if !map.is_object() {
        return Err("审核模板映射必须是 JSON 对象（含 templates 数组）".into());
    }
    serde_json::from_value::<TemplateMapFile>(map.clone())
        .map_err(|e| format!("审核模板映射格式不正确（templates[].from 必填，to 可为字符串/空串/null）: {e}"))?;
    crate::store::db(data_dir).kv_set(key, map)
}

// ==================== 指纹样本（issue #125 增强：一键从样本生成规则） ====================

/// 读取网关命中（11128 拦截 / 空完成影子风控）落下的指纹样本。
/// 内存环形缓冲（容量 50，最新在前），重启即清空；持久反查靠 app.log 的
/// 「指纹样本:」行。前端「指纹清洗」tab 以此展示并一键预填清洗规则。
#[tauri::command]
pub fn fingerprint_samples_get() -> Result<Value, String> {
    serde_json::to_value(crate::api_server::wb_payload::fingerprint_sample_entries())
        .map_err(|e| format!("序列化指纹样本失败: {e}"))
}

/// 清空指纹样本内存缓冲
#[tauri::command]
pub fn fingerprint_samples_clear() -> Result<(), String> {
    crate::api_server::wb_payload::fingerprint_sample_entries_clear();
    Ok(())
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

    /// 校验拒绝畸形模板映射（templates 元素缺 from / 非对象根）；to 缺省/null
    /// = 停用规则（3.7.11 起合法落库）
    #[test]
    fn template_map_validation_rejects_malformed() {
        let dir = tmp_dir("tpl_invalid");
        assert!(template_map_set_at(&dir, None, &json!({"templates": [{"to": "b"}]})).is_err());
        assert!(template_map_set_at(&dir, None, &json!([1, 2])).is_err());
        // 拒绝后不得落库
        assert!(crate::store::db(&dir).kv_get_raw("wb_template_map").is_none());
        // to 缺省 = 停用规则（合法）
        assert!(template_map_set_at(&dir, None, &json!({"templates": [{"from": "a"}]})).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写后可读回（SQLite 化 P2：kv `wb_template_map`，get/set 走同一读取语义）
    #[test]
    fn template_map_set_then_get_roundtrip() {
        let dir = tmp_dir("tpl_roundtrip");
        let map = json!({"templates": [{"from": "a", "to": "b"}], "updated_at": 123});
        template_map_set_at(&dir, None, &map).unwrap();
        assert!(crate::store::db(&dir).kv_get_raw("wb_template_map").is_some());
        let got = template_map_get_at(&dir, None).unwrap();
        assert_eq!(got["templates"][0]["from"], json!("a"));
        assert_eq!(got["templates"][0]["to"], json!("b"));
        assert_eq!(got["updated_at"], json!(123));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无自定义（空 KV）时回显内置默认规则 + builtin 标志（指纹清洗 tab 展示口径）
    #[test]
    fn template_map_get_falls_back_to_builtin_defaults() {
        let dir = tmp_dir("tpl_builtin");
        let got = template_map_get_at(&dir, None).unwrap();
        assert_eq!(got["builtin"], json!(true));
        let arr = got["templates"].as_array().expect("默认规则数组");
        assert!(!arr.is_empty(), "内置默认规则非空");
        assert!(arr[0]["from"].is_string() && arr[0]["to"].is_string());
        // 自定义后 builtin 翻转为 false
        template_map_set_at(&dir, None, &json!({"templates": [{"from": "x", "to": "y"}]})).unwrap();
        let got2 = template_map_get_at(&dir, None).unwrap();
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
        template_map_set_at(&dir, None, &json!({"templates": [{"from": "x", "to": "y"}]})).unwrap();
        assert_eq!(template_map_get_at(&dir, None).unwrap()["builtin"], json!(false));
        // 保存空数组 = 恢复默认（FingerprintSanitizePanel::resetToBuiltin 的落盘形状）
        template_map_set_at(&dir, None, &json!({"templates": [], "updated_at": 1})).unwrap();
        let got = template_map_get_at(&dir, None).unwrap();
        assert_eq!(got["builtin"], json!(true));
        let n = got["templates"].as_array().unwrap().len();
        assert_eq!(
            n,
            crate::api_server::wb_payload::default_template_map().len(),
            "回显规则数应与内置默认一致"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 停用规则（to null）落库回读：get 原样回显 null（UI 展示「已停用」态）
    #[test]
    fn template_map_tombstone_roundtrip() {
        let dir = tmp_dir("tpl_tombstone");
        template_map_set_at(&dir, None, &json!({"templates": [{"from": "a", "to": null}]})).unwrap();
        let got = template_map_get_at(&dir, None).unwrap();
        assert_eq!(got["builtin"], json!(false));
        assert_eq!(got["templates"][0]["from"], json!("a"));
        assert_eq!(got["templates"][0]["to"], serde_json::Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #129：档位分区——buddy-ai 档写独立键与 wb 键互不影响，内置默认
    /// 两档共享；未知档位拒绝落库
    #[test]
    fn template_map_tier_keys_are_isolated() {
        let dir = tmp_dir("tpl_tier");
        template_map_set_at(
            &dir,
            Some("buddy-ai"),
            &json!({"templates": [{"from": "a", "to": "b"}]}),
        )
        .unwrap();
        // buddy-ai 键写入生效（自定义态）
        let intl = template_map_get_at(&dir, Some("buddy-ai")).unwrap();
        assert_eq!(intl["builtin"], json!(false));
        assert_eq!(intl["templates"][0]["from"], json!("a"));
        // wb 键不受影响（仍 builtin 回显）
        assert_eq!(template_map_get_at(&dir, None).unwrap()["builtin"], json!(true));
        // tier 缺省与显式 "wb" 同键
        template_map_set_at(
            &dir,
            Some("wb"),
            &json!({"templates": [{"from": "w", "to": "x"}]}),
        )
        .unwrap();
        let wb = template_map_get_at(&dir, None).unwrap();
        assert_eq!(wb["builtin"], json!(false));
        assert_eq!(wb["templates"][0]["from"], json!("w"));
        // 未知档位拒绝（get/set 均报错，不得落库）
        assert!(template_map_set_at(&dir, Some("qoder"), &json!({})).is_err());
        assert!(template_map_get_at(&dir, Some("qoder")).is_err());
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
