//! 模型列表配置化与官网同步
//!
//! - 模型下拉列表持久化在 `api_models.json`，不硬编码在前端
//! - 「同步官网模型」重放 Trae 客户端的 `batch_get_detail_param` 配置接口获取权威列表
//! - 部分客户端内置模型（glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code）不出现在
//!   配置接口响应中，经 llm_utils_chat 实测需使用 function=solo_agent 调用

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::fs_utils;
use crate::models::{AccountsFile, DeviceMap};

/// 单个模型选项：id = 上游 config_name（原样透传），label = 官方展示名
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
}

/// 配置接口不返回、但客户端内置可用的模型（id, label），同步时保位插入
const BUILTIN_EXTRA: [&str; 3] = ["Doubao-Seed-Code", "glm-5.3-flash", "qwen3.8-flash"];

/// 内置模型 → 官方展示名
const BUILTIN_EXTRA_LABELS: [(&str, &str); 3] = [
    ("Doubao-Seed-Code", "Seed-Code"),
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("qwen3.8-flash", "Qwen3.8-Flash"),
];

/// 默认列表（2026-09 客户端模型选择器实测，含配置接口可见的 15 个 + 内置 3 个）
pub fn default_models() -> Vec<ModelOption> {
    const ITEMS: [(&str, &str); 18] = [
        ("Doubao-Seed-Evolving", "Seed-Evolving"),
        ("Doubao-Seed-2.1-Pro", "Seed-2.1-Pro"),
        ("Doubao-Seed-2.1-Turbo", "Seed-2.1-Turbo"),
        ("Doubao-Seed-Code", "Seed-Code"),
        ("glm-5.3-flash", "GLM-5.3-Flash"),
        ("glm-5.3", "GLM-5.3"),
        ("glm-5.2", "GLM-5.2"),
        ("DeepSeek-V4-Flash-Official", "DeepSeek-V4-Flash 正式版"),
        ("DeepSeek-V4-Flash", "DeepSeek-V4-Flash"),
        ("DeepSeek-V4-Pro-Official", "DeepSeek-V4-Pro 正式版"),
        ("DeepSeek-V4-Pro", "DeepSeek-V4-Pro"),
        ("kimi-k3", "Kimi-K3"),
        ("kimi-k2.7-code", "Kimi-K2.7-Code"),
        ("kimi-k2.6", "Kimi-K2.6"),
        ("minimax-m3", "MiniMax-M3"),
        ("qwen3.8-flash", "Qwen3.8-Flash"),
        ("qwen3.8-max", "Qwen3.8-Max"),
        ("qwen-3.7-plus", "Qwen3.7-Plus"),
    ];
    ITEMS
        .into_iter()
        .map(|(id, label)| ModelOption {
            id: id.to_string(),
            label: label.to_string(),
        })
        .collect()
}

/// 模型 → 上游 function 覆盖：部分模型仅在 solo_agent 下可用，
/// 其余走 Trae Work 模式的默认 function
pub fn function_for_model(model_lower: &str) -> &'static str {
    match model_lower {
        "doubao-seed-code" | "glm-5.3-flash" | "qwen3.8-flash" => "solo_agent",
        _ => super::FUNCTION,
    }
}

/// 模型列表文件路径：base_dir/data/api_models.json（数据文件统一放 data/ 子目录）
fn models_file(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("data").join("api_models.json")
}

/// 数据文件路径：base_dir/data/name（与 AppState::path() 的路由保持一致）
fn data_file(data_dir: &Path, name: &str) -> std::path::PathBuf {
    data_dir.join("data").join(name)
}

/// 读取模型列表；文件缺失或为空时写入默认列表。
/// 兼容旧位置（base_dir/api_models.json）：命中则迁移内容到 data/ 子目录（旧文件保留不动）。
/// 文件损坏（JSON 解析失败）：记录日志、备份为 .bak 后写入默认列表自愈，不静默。
pub fn load_models(data_dir: &Path) -> Vec<ModelOption> {
    let path = models_file(data_dir);
    // Ok(Some(list)) 读取成功；Ok(None) 文件缺失或空列表；Err(原因) 文件存在但损坏
    let read_list = |p: &Path| -> Result<Option<Vec<ModelOption>>, String> {
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        if text.trim().is_empty() {
            return Ok(None);
        }
        match serde_json::from_str::<Vec<ModelOption>>(&text) {
            Ok(list) if !list.is_empty() => Ok(Some(list)),
            Ok(_) => Ok(None),
            Err(e) => Err(format!("{} 解析失败: {e}", p.display())),
        }
    };
    match read_list(&path) {
        Ok(Some(list)) => return list,
        Err(e) => {
            fs_utils::app_log(
                data_dir,
                &format!("模型列表文件损坏，备份后回退默认列表: {e}"),
            );
            let _ = std::fs::rename(&path, path.with_extension("json.bak"));
        }
        Ok(None) => {}
    }
    // 旧位置兼容迁移（v3.2.5 曾存放在数据根目录）
    let legacy = data_dir.join("api_models.json");
    if path != legacy {
        match read_list(&legacy) {
            Ok(Some(list)) => {
                if fs_utils::write_json(&path, &list).is_ok() {
                    return list;
                }
            }
            Err(e) => {
                fs_utils::app_log(data_dir, &format!("旧位置模型列表损坏，忽略: {e}"));
            }
            Ok(None) => {}
        }
    }
    let defaults = default_models();
    if let Err(e) = fs_utils::write_json(&path, &defaults) {
        fs_utils::app_log(data_dir, &format!("模型列表默认配置写入失败: {e}"));
    }
    defaults
}

/// 官方模型内部黑名单（非用户可见的 agent/内部配置）
fn is_internal(name: &str) -> bool {
    const EXACT: [&str; 4] = ["Doubao_1_6", "doubao_1_6", "aquila", "sagitta"];
    const PREFIXES: [&str; 10] = [
        "custom_model",
        "search_agent",
        "fast_apply",
        "input_optimization",
        "explore",
        "file_search",
        "browser_use",
        "agnes",
        "summary",
        "commit",
    ];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// 规范排序：按默认列表顺序，官网新增模型追加尾部；内置 3 项保位插入
fn normalize_order(mut fetched: Vec<ModelOption>) -> Vec<ModelOption> {
    let defaults = default_models();
    // 保位插入内置项（若官网列表缺失）
    for extra in BUILTIN_EXTRA {
        if fetched.iter().any(|m| m.id == extra) {
            continue;
        }
        let label = BUILTIN_EXTRA_LABELS
            .iter()
            .find(|(id, _)| *id == extra)
            .map(|(_, l)| l.to_string())
            .unwrap_or_else(|| extra.to_string());
        let pos = defaults
            .iter()
            .position(|d| d.id == extra)
            .unwrap_or(fetched.len());
        let insert_at = fetched
            .iter()
            .position(|m| {
                defaults
                    .iter()
                    .position(|d| d.id == m.id)
                    .map_or(false, |i| i > pos)
            })
            .unwrap_or(fetched.len());
        fetched.insert(insert_at, ModelOption { id: extra.to_string(), label });
    }
    let rank = |id: &str| {
        defaults
            .iter()
            .position(|d| d.id == id)
            .unwrap_or(usize::MAX)
    };
    fetched.sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)));
    fetched
}

/// 重放 batch_get_detail_param 拉取官网最新模型列表并落盘
/// `accounts` 由调用方预先经 vault 解密（含明文 jwt）
pub fn fetch_official(data_dir: &Path, accounts: AccountsFile) -> Result<Vec<ModelOption>, String> {
    // 取第一个可用账号（最多尝试 3 个）
    let device_map: DeviceMap = fs_utils::read_json(&data_file(data_dir, "device_map.json"));
    let candidates: Vec<(&crate::models::RawAccount, String, String)> = accounts
        .accounts
        .iter()
        .filter(|a| !a.jwt.trim().is_empty())
        .take(3)
        .filter_map(|a| {
            let uid = a.user_id.as_deref()?.to_string();
            let device_id = device_map
                .get(&uid)
                .map(|d| d.device_id.clone())
                .unwrap_or_default();
            let machine_id = super::pool::seeded_hex(64, &uid, "mach");
            Some((a, device_id, machine_id))
        })
        .collect();
    if candidates.is_empty() {
        return Err("没有可用账号（缺少 JWT），请先在账号管理中添加账号".into());
    }

    // 项目未启用 ureq 的 proxy-from-env feature：Agent 默认直连，
    // 不读环境变量/系统代理，不会被本地 MITM 代理拦截形成循环
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let body = json!({
        "functions": [
            "assistant", "solo_agent_lite", "solo_coder", "solo_agent_remote",
            "solo_work_lite", "solo_work_remote", "solo_design_lite",
            "solo_design_remote", "builder"
        ],
        "agent_type": "",
        "current_config_info": { "config_name": "", "is_custom_model": false },
        "mode_type": 0,
        "access_type": 1,
        "ab_force_vids": "",
        "ab_autotest_advanced_mode": 0,
        "show_custom_model": true,
    });

    let mut last_err = String::new();
    let mut parsed: Option<Vec<ModelOption>> = None;
    for (account, device_id, machine_id) in &candidates {
        let resp = agent
            .post("https://api5-normal.mchost.guru/api/ide/v1/batch_get_detail_param")
            .set("Content-Type", "application/json")
            .set("Request-Traffic-Type", "prod")
            .set("User-Agent", "TraeClient/TTNet")
            .set("x-app-id", super::APP_ID)
            .set("x-app-version", "default")
            .set("x-app-version-code", &super::IDE_VERSION_CODE.to_string())
            .set("x-bridge-transport", "aha")
            .set("x-device-brand", "CREFG-XX")
            .set("x-device-cpu", "Intel")
            .set("x-device-id", device_id)
            .set("x-device-type", "windows")
            .set("x-ide-token", account.jwt.trim())
            .set("x-ide-version", &super::IDE_VERSION)
            .set("x-ide-version-code", &super::IDE_VERSION_CODE.to_string())
            .set("x-ide-version-type", "stable")
            .set("x-lgw-req-sdk-type", "3")
            .set("x-machine-id", machine_id)
            .set("x-os-version", "Windows 11 Home China")
            .set("package-type", "stable_cn")
            .set("x-lscbd-aid", "787976")
            .set("x-lscbd-platform", "windows")
            .set("app-version", &super::IDE_VERSION)
            .set("x-ss-dp", "787976")
            .send_json(body.clone());

        match resp {
            Ok(r) => match into_string(r) {
                Ok(text) => match parse_official(&text) {
                    Ok(list) if !list.is_empty() => {
                        parsed = Some(list);
                        break;
                    }
                    Ok(_) => last_err = "官网返回模型列表为空".into(),
                    Err(e) => last_err = e,
                },
                Err(e) => last_err = e,
            },
            Err(ureq::Error::Status(code, r)) => {
                // 401 等：换下一个账号重试
                let detail = into_string(r).unwrap_or_default();
                last_err = format!("HTTP {code}: {}", detail.chars().take(160).collect::<String>());
            }
            Err(e) => last_err = format!("请求失败: {e}"),
        }
    }

    let fetched = parsed.ok_or_else(|| {
        format!("同步失败（已尝试 {} 个账号）: {last_err}", candidates.len())
    })?;

    let list = normalize_order(fetched);
    fs_utils::write_json(&models_file(data_dir), &list)?;
    fs_utils::app_log(
        data_dir,
        &format!("官网模型列表同步成功: {} 个模型", list.len()),
    );
    Ok(list)
}

fn into_string(r: ureq::Response) -> Result<String, String> {
    r.into_string()
        .map_err(|e| format!("读取响应失败: {e}"))
}

/// 解析 batch_get_detail_param 响应：跨 function 合并可见、非内部的模型，去重
fn parse_official(text: &str) -> Result<Vec<ModelOption>, String> {
    let root: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("解析失败: {e}"))?;
    let fcs = root
        .get("function_configs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "响应缺少 function_configs".to_string())?;

    // 先取 solo_work_lite 的顺序作为基准，其余 function 的模型追加其后
    let mut ordered_fns: Vec<&serde_json::Value> = Vec::new();
    for fc in fcs {
        let fname = fc.get("function").and_then(|f| f.as_str()).unwrap_or("");
        if fname == "solo_work_lite" {
            ordered_fns.insert(0, fc);
        } else {
            ordered_fns.push(fc);
        }
    }

    let mut result: Vec<ModelOption> = Vec::new();
    let mut seen: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for fc in ordered_fns {
        let Some(items) = fc.get("config_info_list").and_then(|v| v.as_array()) else {
            continue;
        };
        for ci in items {
            let id = ci.get("config_name").and_then(|v| v.as_str()).unwrap_or("").trim();
            if id.is_empty() || is_internal(id) {
                continue;
            }
            if ci.get("is_invisible_to_user").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            if ci.get("config_switch").and_then(|v| v.as_bool()) == Some(false) {
                continue;
            }
            let label = ci
                .get("display_config")
                .and_then(|d| d.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if label.is_empty() {
                continue;
            }
            let has_dev = ci
                .get("model_detail_list")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|m| {
                        m.get("model_name")
                            .and_then(|v| v.as_str())
                            .map_or(false, |s| s.ends_with("__dev"))
                    })
                })
                .unwrap_or(false);
            match seen.get(id) {
                Some(true) => {} // 已收录且带 __dev，跳过
                Some(false) => {
                    if has_dev {
                        // 用带 __dev 的条目替换先前收录的同名条目
                        if let Some(pos) = result.iter().position(|m| m.id == id) {
                            result[pos].label = label.to_string();
                        }
                        seen.insert(id.to_string(), true);
                    }
                }
                None => {
                    seen.insert(id.to_string(), has_dev);
                    result.push(ModelOption {
                        id: id.to_string(),
                        label: label.to_string(),
                    });
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_override_only_for_builtin_three() {
        assert_eq!(function_for_model("glm-5.3-flash"), "solo_agent");
        assert_eq!(function_for_model("qwen3.8-flash"), "solo_agent");
        assert_eq!(function_for_model("doubao-seed-code"), "solo_agent");
        assert_eq!(function_for_model("glm-5.2"), super::super::FUNCTION);
        assert_eq!(function_for_model("deepseek-v4-flash"), super::super::FUNCTION);
    }

    #[test]
    fn parse_official_dedupes_and_filters() {
        let text = serde_json::json!({
            "function_configs": [
                { "function": "assistant", "config_info_list": [
                    { "config_name": "summary", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Summary" } },
                    { "config_name": "glm-5.3", "is_invisible_to_user": false,
                      "display_config": { "display_name": "glm-5.3" },
                      "model_detail_list": [{ "model_name": "glm-5.3" }] }
                ]},
                { "function": "solo_work_lite", "config_info_list": [
                    { "config_name": "glm-5.3", "is_invisible_to_user": false,
                      "display_config": { "display_name": "GLM-5.3" },
                      "model_detail_list": [{ "model_name": "glm-5.3__dev" }] },
                    { "config_name": "glm-5.2", "is_invisible_to_user": true,
                      "display_config": { "display_name": "GLM-5.2" } },
                    { "config_name": "qwen-3.7-plus", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Qwen3.7-Plus" },
                      "model_detail_list": [{ "model_name": "qwen-3.7-plus__dev" }] }
                ]}
            ]
        })
        .to_string();
        let list = parse_official(&text).unwrap();
        // summary 被过滤；glm-5.3 去重取 __dev 版本展示名；隐藏的 glm-5.2 不出现
        assert_eq!(
            list.iter().map(|m| (m.id.as_str(), m.label.as_str())).collect::<Vec<_>>(),
            vec![("glm-5.3", "GLM-5.3"), ("qwen-3.7-plus", "Qwen3.7-Plus")]
        );
    }

    #[test]
    fn normalize_order_inserts_builtins_and_sorts() {
        let fetched = vec![
            ModelOption { id: "brand-new-model".into(), label: "Brand New".into() },
            ModelOption { id: "glm-5.3".into(), label: "GLM-5.3".into() },
            ModelOption { id: "qwen3.8-max".into(), label: "Qwen3.8-Max".into() },
        ];
        let list = normalize_order(fetched);
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        // 内置 3 项按默认列表位置插入，未知模型排在最后
        let idx = |s: &str| ids.iter().position(|&x| x == s).unwrap();
        assert!(idx("Doubao-Seed-Code") < idx("glm-5.3-flash"));
        assert!(idx("glm-5.3-flash") < idx("glm-5.3"));
        assert!(idx("qwen3.8-flash") < idx("qwen3.8-max"));
        assert_eq!(ids.last(), Some(&"brand-new-model"));
        assert_eq!(ids.len(), 6);
    }
}
