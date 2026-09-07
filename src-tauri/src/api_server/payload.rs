use serde_json::{json, Value};

/// 模型显示名 → (canonical config_name, 内部 model_name) 映射
/// 大小写不敏感：客户端可传入 "doubao-seed-2.1-turbo" 或 "Doubao-Seed-2.1-Turbo"
/// 与上游 batch_get_detail_param（solo_work_lite，2026-09 实测）同步
fn model_config(model: &str) -> (&'static str, &'static str) {
    match model.to_lowercase().as_str() {
        "doubao-seed-evolving" => ("Doubao-Seed-Evolving", "Doubao-Seed-Evolving__dev"),
        "doubao-seed-2.1-pro" | "seed-code-pro-0430" => ("Doubao-Seed-2.1-Pro", "Doubao-Seed-2.1-Pro__dev"),
        "doubao-seed-2.1-turbo" => ("Doubao-Seed-2.1-Turbo", "Doubao-Seed-2.1-Turbo__dev"),
        "doubao-seed-code" => ("Doubao-Seed-Code", "Doubao-Seed-Code__dev"),
        "glm-5.3-flash" => ("glm-5.3-flash", "glm-5.3-flash__dev"),
        "qwen3.8-flash" => ("qwen3.8-flash", "qwen3.8-flash__dev"),
        "glm-5.2" => ("glm-5.2", "glm-5.2__dev"),
        "glm-5.3" => ("glm-5.3", "glm-5.3__dev"),
        "glm-5" => ("glm-5", "glm-5__dev"),
        "glm-5-turbo" => ("glm-5-turbo", "glm-5-turbo__dev"),
        "deepseek-v4-flash" => ("DeepSeek-V4-Flash", "deepseek_v4_flash__dev"),
        "deepseek-v4-flash-official" => ("DeepSeek-V4-Flash-Official", "DeepSeek-V4-Flash-Official__dev"),
        "deepseek-v4-pro" => ("DeepSeek-V4-Pro", "deepseek_v4_pro__dev"),
        "deepseek-v4-pro-official" => ("DeepSeek-V4-Pro-Official", "DeepSeek-V4-Pro-Official__dev"),
        "kimi-k2.6" => ("kimi-k2.6", "kimi-k2.6__dev"),
        "kimi-k2.7-code" => ("kimi-k2.7-code", "kimi-k2.7-code__dev"),
        "kimi-k3" => ("kimi-k3", "kimi-k3__dev"),
        "minimax-m3" => ("minimax-m3", "minimax-m3__dev"),
        "qwen3.8-max" => ("qwen3.8-max", "qwen3.8-max__dev"),
        "qwen-3.7-plus" => ("qwen-3.7-plus", "qwen-3.7-plus__dev"),
        _ => ("DeepSeek-V4-Flash", "deepseek_v4_flash__dev"),
    }
}

/// 生成类似 UUID 的十六进制字符串
fn gen_uuid_like() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.as_nanos();
    let seed = (nanos as u64).wrapping_mul(0x517cc1b727220a95);
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&seed.to_le_bytes());
    buf[8..16].copy_from_slice(&(seed.wrapping_add(0x9e3779b97f4a7c15)).to_le_bytes());
    let hex: String = buf.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// OpenAI 请求体 → llm_utils_chat 请求体改写
/// llm_utils_chat 消耗通用积分(product_id 208)
pub fn prepare_llm_chat_body(
    src: &[u8],
    default_model: &str,
    uid: &str,
    device_id: &str,
    machine_id: &str,
) -> Vec<u8> {
    let mut obj: Value = match serde_json::from_slice(src) {
        Ok(v) => v,
        Err(_) => return src.to_vec(),
    };
    let obj_mut = match obj.as_object_mut() {
        Some(m) => m,
        None => return src.to_vec(),
    };

    // messages content string → [{type:text, text:...}]
    if let Some(msgs) = obj_mut.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for mi in msgs.iter_mut() {
            if let Some(m) = mi.as_object_mut() {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();

                // assistant tool_calls: function → function_call
                if role == "assistant" {
                    if let Some(tcs) = m.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                        let kept: Vec<Value> = tcs
                            .iter_mut()
                            .filter_map(|tc| {
                                let tcm = tc.as_object_mut()?;
                                if let Some(fn_val) = tcm.remove("function") {
                                    tcm.insert("function_call".into(), fn_val);
                                }
                                let has_name = tcm
                                    .get("function_call")
                                    .and_then(|fc| fc.get("name"))
                                    .and_then(|n| n.as_str())
                                    .map(|s| !s.trim().is_empty())
                                    .unwrap_or(false);
                                if has_name { Some(tc.clone()) } else { None }
                            })
                            .collect();
                        if kept.is_empty() {
                            m.remove("tool_calls");
                        } else {
                            *tcs = kept;
                        }
                    }
                }

                // content string → array
                if let Some(content) = m.get("content") {
                    if let Some(s) = content.as_str() {
                        m.insert(
                            "content".into(),
                            json!([{ "type": "text", "text": s }]),
                        );
                    }
                }
            }
        }
    }

    // model → config_name + model_name (大小写不敏感，规范化为 Trae 客户端使用的标准名称)
    let model = obj_mut
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_model.to_string());
    let (config_name, model_name) = model_config(&model);
    // 部分客户端内置模型仅在 solo_agent 下可用（实测），按模型分发 function
    let function = super::models_sync::function_for_model(&model.to_lowercase());

    // normalize tool_choice and tools (reuse existing logic)
    normalize_tool_choice(obj_mut);
    normalize_tools(obj_mut);

    // 添加 llm_utils_chat 必需字段
    obj_mut.insert("config_name".into(), json!(config_name));
    obj_mut.insert("model_name".into(), json!(model_name));
    obj_mut.insert("stream".into(), json!(true));
    obj_mut.insert("function".into(), json!(function));
    obj_mut.insert("max_tokens".into(), json!(4096));
    obj_mut.insert("conversation_id".into(), json!(gen_uuid_like()));
    obj_mut.insert("user_id".into(), json!(uid));
    obj_mut.insert("session_id".into(), json!(gen_uuid_like()));
    obj_mut.insert("device_id".into(), json!(device_id));
    obj_mut.insert("machine_id".into(), json!(machine_id));
    obj_mut.insert("project_id".into(), json!(gen_uuid_like()));
    obj_mut.insert("workspace_id".into(), json!("e04cdd"));
    obj_mut.insert("prompt_max_tokens".into(), json!(168000));
    obj_mut.insert("mode".into(), json!("FunctionCall"));
    obj_mut.insert("ide_version".into(), json!(super::IDE_VERSION));
    obj_mut.insert("ide_version_code".into(), json!(super::IDE_VERSION_CODE));
    obj_mut.insert("app_id".into(), json!(super::APP_ID));
    obj_mut.insert("package_type".into(), json!("stable_cn"));

    serde_json::to_vec(&obj).unwrap_or_else(|_| src.to_vec())
}

fn normalize_tool_choice(obj: &mut serde_json::Map<String, Value>) {
    let suppress = |obj: &mut serde_json::Map<String, Value>| {
        obj.remove("tools");
        obj.remove("functions");
    };
    let tc = match obj.remove("tool_choice") {
        Some(v) => v,
        None => return,
    };
    match tc {
        Value::String(s) => {
            if s.trim().eq_ignore_ascii_case("none") {
                suppress(obj);
            } else {
                obj.insert("tool_choice".into(), Value::String(s));
            }
        }
        Value::Object(map) => {
            let typ = map
                .get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            match typ.as_str() {
                "none" => suppress(obj),
                "auto" | "required" => {
                    obj.insert("tool_choice".into(), Value::String(typ));
                }
                "function" => {
                    let name = map
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| map.get("name").and_then(|n| n.as_str()))
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "auto".to_string());
                    obj.insert("tool_choice".into(), Value::String(name));
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn normalize_tools(obj: &mut serde_json::Map<String, Value>) {
    let raw = match obj.get_mut("tools") {
        Some(v) => v,
        None => return,
    };
    let list = match raw.as_array_mut() {
        Some(a) => a,
        None => return,
    };
    if list.is_empty() {
        obj.remove("tools");
        return;
    }
    let mut out = Vec::new();
    for item in list.iter_mut() {
        let t = match item.as_object_mut() {
            Some(o) => o,
            None => continue,
        };
        let fn_obj = match t.get_mut("function").and_then(|f| f.as_object_mut()) {
            Some(o) => o,
            None => continue,
        };
        // parameters object → JSON string
        if let Some(params) = fn_obj.get("parameters") {
            if params.is_object() {
                if let Ok(s) = serde_json::to_string(params) {
                    fn_obj.insert("parameters".into(), Value::String(s));
                }
            }
        }
        out.push(item.clone());
    }
    if out.is_empty() {
        obj.remove("tools");
    } else {
        *raw = Value::Array(out);
    }
}

// ==================== Anthropic 协议输入（F-39：+Anthropic 适配） ====================

/// Anthropic Messages 请求体 → OpenAI 内部格式（随后复用 OpenAI → llm_utils_chat 链路）
/// 支持：system（字符串/blocks）、text blocks、tool_use/tool_result、tools、tool_choice
/// 不支持：image 等多模态 block（跳过）；max_tokens 由 prepare_llm_chat_body 统一设 4096
pub fn anthropic_to_openai(src: &[u8]) -> Vec<u8> {
    let v: Value = match serde_json::from_slice(src) {
        Ok(v) => v,
        Err(_) => return src.to_vec(),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return src.to_vec(),
    };

    let mut out = serde_json::Map::new();
    if let Some(m) = obj.get("model") {
        out.insert("model".into(), m.clone());
    }
    if let Some(s) = obj.get("stream") {
        out.insert("stream".into(), s.clone());
    }

    let mut messages: Vec<Value> = Vec::new();

    // system → 首条 system 消息
    match obj.get("system") {
        Some(Value::String(s)) => {
            messages.push(json!({ "role": "system", "content": s }));
        }
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                messages.push(json!({ "role": "system", "content": text }));
            }
        }
        _ => {}
    }

    for msg in obj
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("user")
            .to_string();
        match msg.get("content") {
            Some(Value::String(s)) => {
                messages.push(json!({ "role": role, "content": s }));
            }
            Some(Value::Array(blocks)) => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                let mut tool_results: Vec<Value> = Vec::new();
                for b in blocks {
                    let btype = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match btype {
                        "text" => {
                            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(t.to_string());
                            }
                        }
                        "tool_use" => {
                            let args = b.get("input").cloned().unwrap_or(json!({}));
                            tool_calls.push(json!({
                                "id": b.get("id").cloned()
                                    .unwrap_or(json!(format!("toolu_{}", tool_calls.len()))),
                                "type": "function",
                                "function": {
                                    "name": b.get("name").cloned().unwrap_or(json!("")),
                                    "arguments": serde_json::to_string(&args).unwrap_or_default(),
                                },
                            }));
                        }
                        "tool_result" => {
                            let content_str = match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(arr)) => arr
                                    .iter()
                                    .filter_map(|c| c.get("text"))
                                    .filter_map(|t| t.as_str())
                                    .collect::<Vec<_>>()
                                    .join(""),
                                _ => String::new(),
                            };
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": b.get("tool_use_id").cloned().unwrap_or(json!("")),
                                "content": content_str,
                            }));
                        }
                        _ => {} // image 等不支持类型跳过
                    }
                }
                if !tool_results.is_empty() {
                    // tool_result 属于 user 回合：先输出 tool 消息，再输出剩余文本
                    for tr in tool_results {
                        messages.push(tr);
                    }
                    if !text_parts.is_empty() {
                        messages.push(json!({ "role": role, "content": text_parts.join("") }));
                    }
                } else if !tool_calls.is_empty() {
                    let mut m = json!({
                        "role": "assistant",
                        "content": if text_parts.is_empty() { Value::Null } else { json!(text_parts.join("")) },
                    });
                    m["tool_calls"] = json!(tool_calls);
                    messages.push(m);
                } else if !text_parts.is_empty() {
                    messages.push(json!({ "role": role, "content": text_parts.join("") }));
                }
            }
            _ => {}
        }
    }
    out.insert("messages".into(), json!(messages));

    // tools: Anthropic {name, description, input_schema} → OpenAI function 格式
    if let Some(Value::Array(ts)) = obj.get("tools") {
        let converted: Vec<Value> = ts
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").cloned().unwrap_or(json!("")),
                        "parameters": t.get("input_schema").cloned().unwrap_or(json!({"type":"object"})),
                    },
                }))
            })
            .collect();
        if !converted.is_empty() {
            out.insert("tools".into(), json!(converted));
        }
    }

    // tool_choice: auto/any/tool
    if let Some(tc) = obj.get("tool_choice") {
        match tc {
            Value::String(s) => {
                out.insert("tool_choice".into(), json!(s));
            }
            Value::Object(m) => {
                let typ = m.get("type").and_then(|t| t.as_str()).unwrap_or("auto");
                match typ {
                    "any" => {
                        out.insert("tool_choice".into(), json!("required"));
                    }
                    "tool" => {
                        out.insert(
                            "tool_choice".into(),
                            json!({
                                "type": "function",
                                "function": { "name": m.get("name").cloned().unwrap_or(json!("")) },
                            }),
                        );
                    }
                    _ => {
                        out.insert("tool_choice".into(), json!("auto"));
                    }
                }
            }
            _ => {}
        }
    }

    serde_json::to_vec(&Value::Object(out)).unwrap_or_else(|_| src.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_basic_text_conversion() {
        let src = json!({
            "model": "glm-5.2",
            "max_tokens": 1024,
            "system": "你是助手",
            "stream": false,
            "messages": [
                {"role": "user", "content": "你好"},
                {"role": "assistant", "content": [{"type": "text", "text": "你好！"}]},
                {"role": "user", "content": [{"type": "text", "text": "继续"}]}
            ]
        });
        let out: Value = serde_json::from_slice(&anthropic_to_openai(serde_json::to_vec(&src).unwrap().as_slice())).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "你是助手");
        assert_eq!(msgs[1]["content"], "你好");
        assert_eq!(msgs[2]["content"], "你好！");
        assert_eq!(msgs[3]["content"], "继续");
        assert_eq!(out["model"], "glm-5.2");
    }

    #[test]
    fn anthropic_tool_roundtrip_conversion() {
        let src = json!({
            "model": "glm-5.2",
            "max_tokens": 1024,
            "tools": [{"name": "get_weather", "description": "查天气", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "北京天气"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "北京"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "晴 25 度"}
                ]}
            ]
        });
        let out: Value = serde_json::from_slice(&anthropic_to_openai(serde_json::to_vec(&src).unwrap().as_slice())).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        // user / assistant(tool_calls) / tool
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
        let args: Value = serde_json::from_str(msgs[1]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "北京");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "toolu_1");
        assert_eq!(msgs[2]["content"], "晴 25 度");
        // tools → OpenAI function 格式
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
    }
}
