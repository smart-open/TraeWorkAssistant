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
use crate::api_server::wb_payload::{self, TemplateMapFile};
use crate::fs_utils;
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
    let path = data_dir.join("data").join(wb_model_route::ROUTE_FILE);
    fs_utils::write_json(&path, config)
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

#[tauri::command]
pub fn wb_template_map_get(state: State<AppState>) -> Result<Value, String> {
    template_map_get_at(&state.data_dir)
}

#[tauri::command]
pub fn wb_template_map_set(state: State<AppState>, map: Value) -> Result<(), String> {
    template_map_set_at(&state.data_dir, &map)
}

fn template_map_get_at(data_dir: &std::path::Path) -> Result<Value, String> {
    let new_path = wb_payload::template_map_path(data_dir);
    let effective = if new_path.exists() {
        new_path
    } else {
        wb_payload::template_map_path_legacy(data_dir)
    };
    let file: TemplateMapFile = fs_utils::read_json(&effective);
    serde_json::to_value(file).map_err(|e| format!("序列化审核模板映射失败: {e}"))
}

/// 结构校验：对齐 wb_route::load_templates 的读取形状（TemplateMapFile）——
/// templates 为 [{from,to}] 数组（两字段均为必填字符串），updated_at 可空
fn template_map_set_at(data_dir: &std::path::Path, map: &Value) -> Result<(), String> {
    if !map.is_object() {
        return Err("审核模板映射必须是 JSON 对象（含 templates 数组）".into());
    }
    serde_json::from_value::<TemplateMapFile>(map.clone())
        .map_err(|e| format!("审核模板映射格式不正确（templates[].from/to 均为必填字符串）: {e}"))?;
    let path = wb_payload::template_map_path(data_dir);
    fs_utils::write_json(&path, map)
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

    /// 校验拒绝畸形模板映射（templates 元素缺 to / 非对象根）
    #[test]
    fn template_map_validation_rejects_malformed() {
        let dir = tmp_dir("tpl_invalid");
        assert!(template_map_set_at(&dir, &json!({"templates": [{"from": "a"}]})).is_err());
        assert!(template_map_set_at(&dir, &json!([1, 2])).is_err());
        // 拒绝后不得落盘
        assert!(!dir.join("data").join("wb_template_map.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 临时目录写后可读回（落盘 data/ 新路径，get 走同一读取语义）
    #[test]
    fn template_map_set_then_get_roundtrip() {
        let dir = tmp_dir("tpl_roundtrip");
        // write_json 在目标同目录落临时文件，data/ 子目录须先存在（生产由 AppState 初始化创建）
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let map = json!({"templates": [{"from": "a", "to": "b"}], "updated_at": 123});
        template_map_set_at(&dir, &map).unwrap();
        assert!(dir.join("data").join("wb_template_map.json").exists());
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
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let cfg = json!({"aliases": {"claude-x": "glm-5.3"}, "rules": [], "suffixes": []});
        route_config_set_at(&dir, &cfg).unwrap();
        assert!(dir.join("data").join("wb_model_route.json").exists());
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
