//! CodeArts（华为云码道）模型目录同步与解析。
//!
//! 网关路由判定（dispatch 源命中、unified_catalog 目录聚合、codearts_route
//! 执行素材）统一读本模块缓存的目录——每日由 [`run_task`] 签名调用
//! `GET /v1/model/builtin` 拉取真实模型清单，原子替换 kv 缓存：
//! - 空池空转；全部账号无凭证（未录入 AK/SK）静默跳过不计失败；
//! - 网络/服务端失败返 Err 交调度器 30 分钟冷却重试；
//! - 目录与账号无关（全局数据），按池序逐可用账号尝试，任一成功即收口；
//! - 缓存键 `codearts_catalog`（kv 文档，结构 {models: [...], updated_at: i64}）。
//!
//! 条目结构（宽容解析，上游字段漂移不致整表失效）：
//! `{id, name, context_length, rate, upstream_key, ...}`。`resolve` 按 canonical
//! 匹配返回 (upstream_key, model_id)，供执行层构造推理请求。

use serde_json::{json, Value};

use crate::state::AppState;

use super::codearts_common;
use super::codearts_sign::{self, MODEL_BUILTIN_PATH, SNAP_HOST};

/// kv 缓存键（整表文档）
const CATALOG_KEY: &str = "codearts_catalog";

/// model/builtin 完整 URL（纯函数便于单测锚定端点形态）
pub fn model_builtin_url() -> String {
    format!("https://{SNAP_HOST}{MODEL_BUILTIN_PATH}")
}

/// canonical 归一：小写 + 去首尾空白 + 连续空白折叠为单空格（与 unified_catalog
/// 同源口径，避免大小写/空格差异导致目录匹配失败）
fn canonical(s: &str) -> String {
    s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 读缓存目录（缺失/损坏回退空表）
pub fn load_catalog(data_dir: &std::path::Path) -> Vec<Value> {
    let v: Value = crate::store::db(data_dir).kv_get(CATALOG_KEY);
    v.get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 写缓存目录（原子整表替换 + 时间戳）
fn save_catalog(data_dir: &std::path::Path, models: &[Value]) -> Result<usize, String> {
    let doc = json!({
        "models": models,
        "updated_at": chrono::Utc::now().timestamp(),
    });
    crate::store::db(data_dir).kv_set(CATALOG_KEY, &doc)?;
    Ok(models.len())
}

/// 目录解析：模型名 canonical 命中 → (upstream_key, model_id)。
/// `upstream_key` 取条目 `upstreamKey`/`upstream_key`/`id` 首个非空值；
/// `model_id` 取条目 `id`/`name` 首个非空值（执行层据此下发）。
/// 未命中返回 None（该模型无 CodeArts 源）。
pub fn resolve(model: &str) -> Option<(String, String)> {
    resolve_in(&load_catalog_data_dir(), model)
}

/// 以给定目录解析（纯函数，便于单测/聚合层复用）
fn resolve_in(models: &[Value], model: &str) -> Option<(String, String)> {
    let target = canonical(model);
    if target.is_empty() {
        return None;
    }
    for m in models {
        let id = m.get("id").and_then(Value::as_str).unwrap_or("");
        let name = m.get("name").and_then(Value::as_str).unwrap_or("");
        if canonical(id) != target && canonical(name) != target {
            continue;
        }
        let upstream = m
            .get("upstreamKey")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| m.get("upstream_key").and_then(Value::as_str).filter(|s| !s.is_empty()))
            .unwrap_or(id);
        let model_id = if id.is_empty() { name } else { id };
        if model_id.is_empty() {
            continue;
        }
        return Some((upstream.to_string(), model_id.to_string()));
    }
    None
}

/// 供 unified_catalog 聚合的目录列表（含开关/池健康之外的静态条目）。
/// 与 [`resolve`] 共用同一份缓存，保证目录展示与执行解析一致。
pub fn list_models() -> Vec<Value> {
    load_catalog_data_dir()
}

/// 模型声明的输出上限（`max_tokens` 字段）：执行层据此夹取客户端超限请求。
/// 未收录/未声明返回 None（不猜值，交给上游判定）。
pub fn max_tokens_of(model: &Value) -> Option<u64> {
    let name = model.as_str()?;
    let target = canonical(name);
    if target.is_empty() {
        return None;
    }
    load_catalog_data_dir().iter().find_map(|m| {
        let id = m.get("id").and_then(Value::as_str).unwrap_or("");
        let name = m.get("name").and_then(Value::as_str).unwrap_or("");
        if canonical(id) != target && canonical(name) != target {
            return None;
        }
        m.get("max_tokens").and_then(Value::as_u64).filter(|v| *v > 0)
    })
}

/// 进程级 data_dir 注册（网关聚合/执行路径无 AppState 引用，由启动方注入一次）。
/// 未注册时 resolve/list 回退空表（测试/CLI 场景零副作用）。
static DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_data_dir(data_dir: &std::path::Path) {
    let _ = DATA_DIR.set(data_dir.to_path_buf());
}

fn load_catalog_data_dir() -> Vec<Value> {
    match DATA_DIR.get() {
        Some(p) => load_catalog(p),
        None => Vec::new(),
    }
}

/// 解析 model/builtin 响应体为条目数组（宽容多种上游形态）：
/// 顶层数组 / `data` 数组 / `models` 数组 / `data.models` 数组。
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let arr = payload
        .as_array()
        .or_else(|| payload.get("data").and_then(Value::as_array))
        .or_else(|| payload.get("models").and_then(Value::as_array))
        // 真实 /v1/model/builtin 响应（2026-11 实测）：顶层 builtinModels
        .or_else(|| payload.get("builtinModels").and_then(Value::as_array))
        .or_else(|| payload.get("data").and_then(|d| d.get("builtinModels")).and_then(Value::as_array))
        .or_else(|| payload.get("data").and_then(|d| d.get("models")).and_then(Value::as_array))
        .or_else(|| payload.get("result").and_then(Value::as_array));
    arr.map(|a| {
        a.iter()
            .filter(|v| v.is_object())
            .filter_map(|v| normalize_entry(v))
            .collect()
    })
    .unwrap_or_default()
}

/// 归一化单条目：抽取统一字段（id/name/context_length/rate/upstream_key/efforts），
/// 兼容上游多种命名；无效条目（无 id/name）返回 None。
fn normalize_entry(v: &Value) -> Option<Value> {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| v.get("model_id").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .or_else(|| v.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .or_else(|| v.get("modelId").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .or_else(|| v.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()))?
        .to_string();
    let name = v
        .get("model_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| v.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or(&id)
        .to_string();
    // 真实 builtinModels 字段：context_window / max_tokens / credit[].ratio_display
    let ctx = v
        .get("context_window")
        .and_then(Value::as_u64)
        .or_else(|| v.get("context_length").and_then(Value::as_u64))
        .or_else(|| v.get("contextLength").and_then(Value::as_u64))
        .or_else(|| v.get("maxInputTokens").and_then(Value::as_u64))
        .or_else(|| v.get("max_input_tokens").and_then(Value::as_u64));
    let max_tokens = v.get("max_tokens").and_then(Value::as_u64);
    let upstream_key = v
        .get("upstreamKey")
        .and_then(Value::as_str)
        .or_else(|| v.get("upstream_key").and_then(Value::as_str))
        .unwrap_or(&id)
        .to_string();
    // ratio_display 如 "0.7x" → 0.7；credit[].ratio 为更精细倍率，优先 ratio_display 展示口径
    let credit_ratio_display = v.get("credit").and_then(Value::as_array).and_then(|a| {
        a.first().and_then(|c| c.get("ratio_display")).and_then(Value::as_str)
            .and_then(|s| s.trim_end_matches('x').parse::<f64>().ok())
    });
    let rate = v
        .get("rate")
        .and_then(Value::as_f64)
        .or(credit_ratio_display)
        .or_else(|| v.get("credit").and_then(Value::as_f64));
    let supports_images = v.get("supports_images").and_then(Value::as_bool);
    let mut efforts: Vec<String> = v
        .get("efforts")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if efforts.is_empty() {
        if v.get("supportsReasoning").and_then(Value::as_bool).unwrap_or(false)
            || v.get("supports_reasoning").and_then(Value::as_bool).unwrap_or(false)
        {
            efforts = vec!["low".into(), "medium".into(), "high".into()];
        }
    }
    let mut out = json!({
        "id": id,
        "name": name,
        "upstream_key": upstream_key,
    });
    if let Some(c) = ctx {
        out["context_length"] = json!(c);
    }
    if let Some(m) = max_tokens {
        out["max_tokens"] = json!(m);
    }
    if let Some(r) = rate {
        out["rate"] = json!(r);
    }
    if !efforts.is_empty() {
        out["efforts"] = json!(efforts);
    }
    if let Some(img) = supports_images.or_else(|| v.get("supportsImages").and_then(Value::as_bool)) {
        out["supports_image"] = json!(img);
    }
    Some(out)
}

/// 调度器/CLI 共用入口：CodeArts 模型目录同步。
/// 按池序逐可用账号尝试，任一成功即原子替换缓存收口；全部失败才 Err。
pub fn run_task(state: &AppState) -> Result<Value, String> {
    // 注入 data_dir（网关进程内 resolve/list 与目录缓存同源）
    set_data_dir(&state.data_dir);
    let pool = crate::store::docs::codearts_pool_load(&crate::store::db(&state.data_dir));
    let accounts = pool.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();
    if accounts.is_empty() {
        return Ok(json!({ "ok": true, "skipped": "无 CodeArts 账号" }));
    }
    let token_store = crate::store::docs::codearts_token_store_load(&crate::store::db(&state.data_dir));
    let url = model_builtin_url();
    let mut last_err: Option<String> = None;
    for a in &accounts {
        let Some(id) = a.get("id").and_then(Value::as_str) else {
            continue;
        };
        let creds = match codearts_common::ensure_fresh_creds(&state.data_dir, id) {
            Ok(c) => c,
            Err(e) => {
                last_err = Some(format!("凭证读取失败: {e}"));
                continue;
            }
        };
        if !creds.is_valid() {
            continue;
        }
        // token store 占位行存在 = 已录入凭证（vault 读取失败时 effective_creds 空，已跳过）
        let has_cred = token_store
            .get("tokens")
            .and_then(|t| t.get(id))
            .is_some();
        if !has_cred {
            continue;
        }
        let identity = crate::tasks::codearts_sign::identity_from_creds(&creds);
        // 真实响应（2026-11 实测）：/v1/model/builtin 缺 Agent-Type 头返回
        // 400 TM.00001001「请求头Agent-Type为空」；该头须小写参与签名（与 ops 同款）。
        let extra: [(&str, &str); 1] = [("agent-type", "PromptCenter")];
        let signed = match codearts_sign::sign(&identity, "GET", &url, b"", &extra) {
            Ok(s) => s,
            Err(e) => {
                last_err = Some(format!("model/builtin 签名构造失败: {e}"));
                continue;
            }
        };
        let agent = super::http_agent(20);
        let mut req = agent.get(&url);
        req = req
            .set("Authorization", &signed.authorization)
            .set("x-sdk-date", &signed.x_sdk_date)
            .set("content-type", &signed.content_type);
        if let Some(st) = &signed.x_security_token {
            req = req.set("x-security-token", st.as_str());
        }
        if let Some(sha) = &signed.x_sdk_content_sha256 {
            req = req.set("x-sdk-content-sha256", sha.as_str());
        }
        for (k, v) in &signed.extra {
            req = req.set(k.as_str(), v.as_str());
        }
        let resp = match req.call() {
            Ok(resp) if resp.status() == 200 => resp,
            Ok(resp) => {
                let status = resp.status();
                let _ = resp.into_string();
                last_err = Some(format!("model/builtin HTTP {status}"));
                continue;
            }
            Err(ureq::Error::Status(code, resp)) => {
                let _ = resp.into_string();
                last_err = Some(format!("model/builtin HTTP {code}"));
                continue;
            }
            Err(e) => {
                last_err = Some(format!("model/builtin 网络失败: {e}"));
                continue;
            }
        };
        let body = match resp.into_string() {
            Ok(b) => b,
            Err(e) => {
                last_err = Some(format!("model/builtin 响应读取失败: {e}"));
                continue;
            }
        };
        let payload: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                last_err = Some(format!("model/builtin 响应非 JSON: {e}"));
                continue;
            }
        };
        let models = parse_models(&payload);
        if models.is_empty() {
            last_err = Some("model/builtin 目录为空或字段无法识别".into());
            continue;
        }
        match save_catalog(&state.data_dir, &models) {
            Ok(n) => return Ok(json!({ "ok": true, "models": n, "account": id })),
            Err(e) => last_err = Some(format!("model/builtin 目录落盘失败: {e}")),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(json!({ "ok": true, "skipped": "无可用 CodeArts 凭证" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_anchored_to_snap_host() {
        assert_eq!(
            model_builtin_url(),
            "https://snap-access.cn-north-4.myhuaweicloud.com/v1/model/builtin"
        );
    }

    #[test]
    fn parse_models_accepts_top_level_and_wrapped() {
        let top = json!([{"id": "glm-5", "name": "GLM-5"}]);
        assert_eq!(parse_models(&top).len(), 1);
        let wrapped = json!({"data": {"models": [{"modelId": "k2", "contextLength": 128000}]}});
        let got = parse_models(&wrapped);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["id"], "k2");
        assert_eq!(got[0]["context_length"], 128000);
    }

    /// 真实 builtinModels 响应（2026-11-18 抓包）：model_id/model_name/credit[].ratio_display/context_window
    #[test]
    fn parse_models_real_builtin_shape() {
        let payload = json!({"count":1,"builtinModels":[{
            "model_id":"GLM-5.2","model_name":"GLM-5.2","enable":true,
            "credit":[{"ratio":"0.05","ratio_display":"0.7x"}],
            "supports_images":false,"max_tokens":65536,"context_window":202752
        }]});
        let got = parse_models(&payload);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["id"], "GLM-5.2");
        assert_eq!(got[0]["context_length"], 202752);
        assert_eq!(got[0]["max_tokens"], 65536);
        assert_eq!(got[0]["rate"], 0.7);
        assert_eq!(got[0]["supports_image"], false);
    }

    #[test]
    fn resolve_canonical_and_upstream_key() {
        let models = json!([
            {"id": "GLM-5", "name": "GLM 5", "upstream_key": "glm5-up"}
        ]);
        let arr = models.as_array().unwrap();
        assert_eq!(
            resolve_in(arr, "glm-5"),
            Some(("glm5-up".into(), "GLM-5".into()))
        );
        assert_eq!(
            resolve_in(arr, "  GLM  5 "),
            Some(("glm5-up".into(), "GLM-5".into()))
        );
        assert_eq!(resolve_in(arr, "unknown"), None);
    }
}
