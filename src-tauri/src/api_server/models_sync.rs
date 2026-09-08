//! 模型列表配置化与官网同步
//!
//! - 模型下拉列表持久化在 `data/api_models.json`（data/ 子目录），不硬编码在前端
//! - 「同步官网模型」双源合并：
//!   1) `batch_get_detail_param` 配置接口 → 用户可见模型 + 官方展示名（verified）
//!   2) `get_skill_detail` 的 `skill_payload.meta.models` → 全量模型注册表
//!      （含客户端内置模型 glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code），
//!      与 1) 的差集作为「未验证」模型追加（过滤内部 agent / custom_model / -auto 变体）
//! - 未验证模型调用返回 4001（model config is empty）时，运行时自动改用
//!   solo_agent 重试一次；成功后将 model→function 覆盖自学习持久化

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::fs_utils;
use crate::models::{AccountsFile, DeviceMap};

/// api_models.json 读改写互斥：
/// learn_function_override（请求成功时自学习）与 fetch_official（官网同步）
/// 都是「读 → 改 → 写」，并发时后写会覆盖先写；write_json 的临时文件 + rename
/// 只保证单次写入原子性，需额外串行化整个读改写周期
static MODELS_FILE_LOCK: Mutex<()> = Mutex::new(());

/// api_models.json 统一存放路径：<data_dir>/data/api_models.json。
/// 与其他数据文件（checkin_accounts.json 等）一致落 data/ 子目录；
/// 旧版本曾直接放在数据根目录，此处顺带做幂等迁移。
fn models_path(data_dir: &Path) -> PathBuf {
    let dir = data_dir.join("data");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("api_models.json");
    let legacy = data_dir.join("api_models.json");
    if legacy.is_file() {
        if !path.exists() {
            // 升级迁移：旧位置 → data/ 子目录（同盘 rename，几乎不会失败）
            if let Err(e) = std::fs::rename(&legacy, &path) {
                fs_utils::app_log(
                    data_dir,
                    &format!("api_models.json 迁移到 data/ 失败（保留旧文件）: {e}"),
                );
            }
        } else {
            // 新位置已是权威数据，旧位置文件为升级残留
            let _ = std::fs::remove_file(&legacy);
        }
    }
    path
}

/// 客户端内置模型专用的上游 function（4001 自学习重试目标）
pub const SOLO_AGENT_FUNCTION: &str = "solo_agent";

/// 单个模型选项：id = 上游 config_name（原样透传），label = 官方展示名
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    /// 是否经实测/官方可见性确认；get_skill_detail 注册表补集发现的为 false（未验证）
    #[serde(default = "default_verified")]
    pub verified: bool,
    /// function 覆盖（自学习结果）：该模型仅在指定 function 下可用（如 solo_agent）
    #[serde(default)]
    pub function: Option<String>,
}

fn default_verified() -> bool {
    true
}

/// 同步接口专用版本头（与 2026-09 真实客户端 TraeCN 3.3.98 一致；旧版本号可能返回过期列表）
const SYNC_IDE_VERSION: &str = "3.3.98";
const SYNC_IDE_VERSION_CODE: &str = "20260901";

const URL_BATCH_DETAIL: &str = "https://api5-normal.mchost.guru/api/ide/v1/batch_get_detail_param";
const URL_SKILL_DETAIL: &str = "https://api5-normal.mchost.guru/api/ide/v1/get_skill_detail";

/// 配置接口不返回、但客户端内置可用的模型（id, label）
/// 用途：① get_skill_detail 降级失败时的保底插入 ② 补集模型的官方展示名覆盖
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
            verified: true,
            function: None,
        })
        .collect()
}

/// 模型 → 上游 function 兜底规则：部分模型仅在 solo_agent 下可用，
/// 其余走 Trae Work 模式的默认 function
pub fn function_for_model(model_lower: &str) -> &'static str {
    match model_lower {
        "doubao-seed-code" | "glm-5.3-flash" | "qwen3.8-flash" => "solo_agent",
        _ => super::FUNCTION,
    }
}

/// 读取模型的 function 覆盖（自学习结果）；未配置时返回 None
pub fn function_override(data_dir: &Path, model_lower: &str) -> Option<String> {
    let list: Vec<ModelOption> = fs_utils::read_json(&models_path(data_dir));
    list.into_iter()
        .find(|m| m.id.to_lowercase() == model_lower)
        .and_then(|m| m.function)
        .filter(|f| !f.trim().is_empty())
}

/// 自学习持久化：模型在指定 function 下请求成功后记录覆盖，并标记为已验证
pub fn learn_function_override(data_dir: &Path, model: &str, function: &str) {
    let model_lower = model.to_lowercase();
    let path = models_path(data_dir);
    let _guard = MODELS_FILE_LOCK.lock();
    let mut list: Vec<ModelOption> = fs_utils::read_json(&path);
    match list.iter_mut().find(|m| m.id.to_lowercase() == model_lower) {
        Some(m) => {
            if m.verified && m.function.as_deref() == Some(function) {
                return; // 已记录，避免重复写盘
            }
            m.function = Some(function.to_string());
            m.verified = true;
        }
        None => list.push(ModelOption {
            id: model.to_string(),
            label: model.to_string(),
            verified: true,
            function: Some(function.to_string()),
        }),
    }
    if let Err(e) = fs_utils::write_json(&path, &list) {
        fs_utils::app_log(data_dir, &format!("function 自学习写入失败: {e}"));
        return;
    }
    fs_utils::app_log(data_dir, &format!("function 自学习: {model} → {function}"));
}

/// 读取模型列表；文件缺失或为空时写入默认列表
pub fn load_models(data_dir: &Path) -> Vec<ModelOption> {
    let path = models_path(data_dir);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(list) = serde_json::from_str::<Vec<ModelOption>>(&text) {
            if !list.is_empty() {
                return list;
            }
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
    const EXACT: [&str; 5] = ["Doubao_1_6", "doubao_1_6", "aquila", "sagitta", "doubao-for-auto"];
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

/// 补集模型展示名：内置 3 项用官方名，其余用原 ID
fn official_label(id: &str) -> String {
    BUILTIN_EXTRA_LABELS
        .iter()
        .find(|(bid, _)| *bid == id)
        .map(|(_, l)| l.to_string())
        .unwrap_or_else(|| id.to_string())
}

/// 规范排序：已验证按默认列表顺序、官网新增追加其后；未验证（注册表补集）排最后
fn normalize_order(mut fetched: Vec<ModelOption>) -> Vec<ModelOption> {
    let defaults = default_models();
    // 降级保底：get_skill_detail 补集不可用且 batch 结果也不含内置模型时，保位插入
    for extra in BUILTIN_EXTRA {
        if fetched.iter().any(|m| m.id.eq_ignore_ascii_case(extra)) {
            continue;
        }
        let label = official_label(extra);
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
        fetched.insert(
            insert_at,
            ModelOption {
                id: extra.to_string(),
                label,
                verified: true,
                function: None,
            },
        );
    }
    let rank = |id: &str| {
        defaults
            .iter()
            .position(|d| d.id == id)
            .unwrap_or(usize::MAX)
    };
    fetched.sort_by(|a, b| match (a.verified, b.verified) {
        (false, true) => std::cmp::Ordering::Greater,
        (true, false) => std::cmp::Ordering::Less,
        _ => rank(&a.id).cmp(&rank(&b.id)),
    });
    fetched
}

/// 重放 batch_get_detail_param 拉取官网最新模型列表并落盘（双源合并）
pub fn fetch_official(data_dir: &Path) -> Result<Vec<ModelOption>, String> {
    // 取第一个可用账号（最多尝试 3 个）
    let accounts: AccountsFile = fs_utils::read_json(&data_dir.join("checkin_accounts.json"));
    let device_map: DeviceMap = fs_utils::read_json(&data_dir.join("device_map.json"));
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

    // 显式禁用环境变量代理探测：ureq 2.12 默认不读 HTTP(S)_PROXY（需
    // proxy-from-env feature），此处显式声明意图，防止未来误启该 feature
    // 导致请求绕进用户环境变量里的代理（原实现依赖进程级 NO_PROXY=* 已移除，
    // set_var 会污染 Trae 等子进程环境，使其 --proxy-server 注入失效）
    let agent = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let batch_body = json!({
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
        match post_ide_api(&agent, URL_BATCH_DETAIL, &batch_body, &account.jwt, device_id, machine_id) {
            Ok(text) => match parse_official(&text) {
                Ok(list) if !list.is_empty() => {
                    parsed = Some(list);
                    break;
                }
                Ok(_) => last_err = "官网返回模型列表为空".into(),
                Err(e) => {
                    // HTTP 成功但解析失败：响应结构可能已变更，记录响应片段便于排查
                    let preview: String = text.chars().take(300).collect();
                    fs_utils::app_log(
                        data_dir,
                        &format!("batch_get_detail_param 解析失败: {e}；响应片段: {preview}"),
                    );
                    last_err = e;
                }
            },
            Err(e) => last_err = e,
        }
    }

    let mut list = parsed.ok_or_else(|| {
        format!("同步失败（已尝试 {} 个账号）: {last_err}", candidates.len())
    })?;

    // 双源合并：get_skill_detail 全量注册表 → 与 batch 结果的差集作为未验证模型追加
    match fetch_skill_models(&agent, &candidates) {
        Ok(registry) => {
            let mut added = 0;
            for id in registry {
                if is_internal(&id) || id.ends_with("-auto") {
                    continue;
                }
                if list.iter().any(|m| m.id.eq_ignore_ascii_case(&id)) {
                    continue;
                }
                let verified = BUILTIN_EXTRA.contains(&id.as_str());
                list.push(ModelOption {
                    label: official_label(&id),
                    verified,
                    function: None,
                    id,
                });
                added += 1;
            }
            if added > 0 {
                fs_utils::app_log(
                    data_dir,
                    &format!("get_skill_detail 注册表补集: 新增 {added} 个未验证模型"),
                );
            }
        }
        Err(e) => {
            // 降级：仅用 batch 结果 + 内置保底插入，不影响同步成功
            fs_utils::app_log(data_dir, &format!("get_skill_detail 补集获取失败（降级）: {e}"));
        }
    }

    let mut list = normalize_order(list);
    // 落盘前合并本地 function 覆盖（自学习结果），避免同步整体覆盖丢失
    {
        let _guard = MODELS_FILE_LOCK.lock();
        let existing: Vec<ModelOption> = fs_utils::read_json(&models_path(data_dir));
        for m in list.iter_mut() {
            if m.function.is_some() {
                continue;
            }
            if let Some(old) = existing
                .iter()
                .find(|o| o.id.eq_ignore_ascii_case(&m.id))
                .cloned()
            {
                if old.function.is_some() {
                    m.function = old.function;
                }
            }
        }
        fs_utils::write_json(&models_path(data_dir), &list)?;
    }
    fs_utils::app_log(
        data_dir,
        &format!("官网模型列表同步成功: {} 个模型", list.len()),
    );
    Ok(list)
}

/// 重放 IDE 配置类接口（batch_get_detail_param / get_skill_detail），返回响应文本
fn post_ide_api(
    agent: &ureq::Agent,
    url: &str,
    body: &serde_json::Value,
    jwt: &str,
    device_id: &str,
    machine_id: &str,
) -> Result<String, String> {
    let resp = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("Request-Traffic-Type", "prod")
        .set("User-Agent", "TraeClient/TTNet")
        .set("x-app-id", super::APP_ID)
        .set("x-app-version", SYNC_IDE_VERSION)
        .set("x-app-version-code", SYNC_IDE_VERSION_CODE)
        .set("x-bridge-transport", "aha")
        .set("x-device-brand", "CREFG-XX")
        .set("x-device-cpu", "Intel")
        .set("x-device-id", device_id)
        .set("x-device-type", "windows")
        .set("x-ide-token", jwt.trim())
        .set("x-ide-version", SYNC_IDE_VERSION)
        .set("x-ide-version-code", SYNC_IDE_VERSION_CODE)
        .set("x-ide-version-type", "stable")
        .set("x-lgw-req-sdk-type", "3")
        .set("x-machine-id", machine_id)
        .set("x-os-version", "Windows 11 Home China")
        .set("package-type", "stable_cn")
        .set("x-lscbd-aid", "787976")
        .set("x-lscbd-platform", "windows")
        .set("app-version", SYNC_IDE_VERSION)
        .set("x-ss-dp", "787976")
        .send_json(body);
    match resp {
        Ok(r) => into_string(r),
        Err(ureq::Error::Status(code, r)) => {
            // 401 等：由调用方换下一个账号重试
            let detail = into_string(r).unwrap_or_default();
            Err(format!("HTTP {code}: {}", detail.chars().take(160).collect::<String>()))
        }
        Err(e) => Err(format!("请求失败: {e}")),
    }
}

/// get_skill_detail 全量模型注册表（skill_payload.meta.models 的 key 集合）
fn fetch_skill_models(
    agent: &ureq::Agent,
    candidates: &[(&crate::models::RawAccount, String, String)],
) -> Result<Vec<String>, String> {
    let body = json!({
        "os": "windows",
        "build_time": false,
        "functions": ["solo_agent"],
        "access_type": 0,
    });
    let mut last_err = String::new();
    for (account, device_id, machine_id) in candidates {
        match post_ide_api(agent, URL_SKILL_DETAIL, &body, &account.jwt, device_id, machine_id) {
            Ok(text) => match parse_skill_models(&text) {
                Some(ids) if !ids.is_empty() => return Ok(ids),
                _ => last_err = "meta.models 为空或缺失".into(),
            },
            Err(e) => last_err = e,
        }
    }
    Err(format!("已尝试 {} 个账号: {last_err}", candidates.len()))
}

fn into_string(r: ureq::Response) -> Result<String, String> {
    r.into_string()
        .map_err(|e| format!("读取响应失败: {e}"))
}

/// 解析 get_skill_detail 响应中的全量模型注册表
fn parse_skill_models(text: &str) -> Option<Vec<String>> {
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let models = root
        .get("skill_payload")?
        .get("meta")?
        .get("models")?
        .as_object()?;
    Some(models.keys().cloned().collect())
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
                        verified: true,
                        function: None,
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
        assert!(list.iter().all(|m| m.verified));
    }

    #[test]
    fn parse_skill_models_extracts_registry() {
        let text = serde_json::json!({
            "skill_payload": {
                "meta": {
                    "models": {
                        "glm-5.3-flash": "non_multimodal",
                        "qwen3.8-flash": "default",
                        "Doubao-Seed-Code": "doutops",
                        "custom_model_claude": "default"
                    },
                    "region": "cn"
                }
            }
        })
        .to_string();
        let mut ids = parse_skill_models(&text).unwrap();
        ids.sort();
        assert_eq!(
            ids,
            vec!["Doubao-Seed-Code", "custom_model_claude", "glm-5.3-flash", "qwen3.8-flash"]
        );
        assert!(parse_skill_models("{\"other\":1}").is_none());
    }

    #[test]
    fn normalize_order_inserts_builtins_unverified_last() {
        let fetched = vec![
            ModelOption { id: "brand-new-model".into(), label: "Brand New".into(), verified: true, function: None },
            ModelOption { id: "glm-5.3".into(), label: "GLM-5.3".into(), verified: true, function: None },
            ModelOption { id: "qwen3.8-max".into(), label: "Qwen3.8-Max".into(), verified: true, function: None },
            // 注册表补集发现的未验证模型应排在最后
            ModelOption { id: "kimi-k2.5".into(), label: "kimi-k2.5".into(), verified: false, function: None },
        ];
        let list = normalize_order(fetched);
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        // 内置 3 项按默认列表位置插入，未知已验证模型排其后，未验证模型排最末
        let idx = |s: &str| ids.iter().position(|&x| x == s).unwrap();
        assert!(idx("Doubao-Seed-Code") < idx("glm-5.3-flash"));
        assert!(idx("glm-5.3-flash") < idx("glm-5.3"));
        assert!(idx("qwen3.8-flash") < idx("qwen3.8-max"));
        assert!(idx("brand-new-model") < idx("kimi-k2.5"));
        assert_eq!(ids.last(), Some(&"kimi-k2.5"));
        assert_eq!(ids.len(), 7);
    }

    #[test]
    fn learn_and_read_function_override() {
        let dir = std::env::temp_dir().join(format!("twa_models_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // 未收录模型：读取为空，学习后可读且标记已验证
        assert!(function_override(&dir, "brand-new-model").is_none());
        learn_function_override(&dir, "brand-new-model", "solo_agent");
        assert_eq!(
            function_override(&dir, "brand-new-model").as_deref(),
            Some("solo_agent")
        );
        // 已收录模型：学习后 verified 翻转
        let defaults = default_models();
        fs_utils::write_json(&models_path(&dir), &defaults).unwrap();
        learn_function_override(&dir, "GLM-5.2", "solo_agent");
        let list: Vec<ModelOption> =
            fs_utils::read_json(&models_path(&dir));
        let m = list.iter().find(|m| m.id == "glm-5.2").unwrap();
        assert_eq!(m.function.as_deref(), Some("solo_agent"));
        assert!(m.verified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_root_file_migrates_to_data_subdir() {
        let dir = std::env::temp_dir().join(format!("twa_models_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        // 旧版本布局：api_models.json 直接放在数据根目录
        fs_utils::write_json(&dir.join("api_models.json"), &default_models()).unwrap();
        // 任一访问入口触发幂等迁移
        let list = load_models(&dir);
        assert!(!list.is_empty());
        assert!(!dir.join("api_models.json").exists());
        assert!(dir.join("data").join("api_models.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
