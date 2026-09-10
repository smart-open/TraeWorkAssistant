//! Codex Responses API 投影转换器（T4.1/F-40）
//!
//! `/v1/responses`（Codex CLI `wire_api="responses"` 直配 base_url）：
//! - 请求投影：Responses 格式 → OpenAI chat completions 内部格式，
//!   复用 wb_route 既有取号/重试/粘性/脱敏管线（三协议一份）；
//! - 非流式响应投影：内部 OpenAI completion → Responses 对象；
//! - 流式响应投影在 wb_sse.rs `Protocol::Responses` 分支实现。
//!
//! 宽容解析红线：字段缺失不报错，只按规范降级（input 缺失 → 空消息列表）。

use serde_json::{json, Map, Value};

/// Responses 请求 → OpenAI chat completions 内部格式
///
/// 映射：instructions → system；input（string | items[]）→ messages；
/// function_call/function_call_output → tool_calls/tool 消息；
/// tools 平铺（Responses 顶层 name/parameters → chat function 包裹）；
/// max_output_tokens → max_tokens；reasoning.effort → reasoning_effort。
pub fn responses_to_chat(body: &Value) -> Result<Value, String> {
    if !body.is_object() {
        return Err("request body must be a JSON object".to_string());
    }
    let mut out = Map::new();

    // 消息列表
    let mut messages: Vec<Value> = Vec::new();
    if let Some(inst) = body.get("instructions").and_then(|v| v.as_str()) {
        if !inst.is_empty() {
            messages.push(json!({"role": "system", "content": inst}));
        }
    }
    match body.get("input") {
        None | Some(Value::Null) => {
            messages.push(json!({"role": "user", "content": ""}));
        }
        Some(Value::String(s)) => {
            messages.push(json!({"role": "user", "content": s}));
        }
        Some(Value::Array(items)) => {
            let converted = convert_input_items(items);
            if converted.is_empty() {
                return Err("input: at least one usable item required".to_string());
            }
            messages.extend(converted);
        }
        Some(_) => return Err("input: expected string or array".to_string()),
    }
    out.insert("messages".to_string(), json!(messages));

    // 采样参数直传
    for (src, dst) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(v) = body.get(src) {
            if v.is_number() {
                out.insert(dst.to_string(), v.clone());
            }
        }
    }
    if let Some(v) = body.get("max_output_tokens").and_then(|v| v.as_u64()) {
        out.insert("max_tokens".to_string(), json!(v));
    }
    if let Some(v) = body.get("parallel_tool_calls") {
        if v.is_boolean() {
            out.insert("parallel_tool_calls".to_string(), v.clone());
        }
    }
    // reasoning.effort → reasoning_effort（WB 目录 effort 降级链复用）
    if let Some(effort) = body.pointer("/reasoning/effort").and_then(|v| v.as_str()) {
        if !effort.is_empty() {
            out.insert("reasoning_effort".to_string(), json!(effort));
        }
    }

    // tools：Responses 平铺 → chat 包裹（仅 function；其余类型丢弃）
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let converted: Vec<Value> = tools
            .iter()
            .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("function"))
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").cloned().unwrap_or(json!("")),
                        "parameters": t.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}})),
                        "strict": t.get("strict").cloned().unwrap_or(json!(false)),
                    },
                }))
            })
            .collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), json!(converted));
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        match tc {
            Value::String(s) => {
                out.insert("tool_choice".to_string(), json!(s));
            }
            Value::Object(o) if o.get("type").and_then(|v| v.as_str()) == Some("function") => {
                if let Some(name) = o.get("name").and_then(|v| v.as_str()) {
                    out.insert(
                        "tool_choice".to_string(),
                        json!({"type": "function", "function": {"name": name}}),
                    );
                }
            }
            _ => {}
        }
    }

    // tool_choice string 化（F-30）与 effort 改写由 wb_payload::prepare_wb_chat_body 统一处理
    Ok(Value::Object(out))
}

/// input items → chat messages
fn convert_input_items(items: &[Value]) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    for item in items {
        let itype = item.get("type").and_then(|v| v.as_str()).unwrap_or("message");
        match itype {
            "message" => {
                let role = item
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("user")
                    .to_string();
                let text = content_text(item.get("content"));
                let role = if role == "assistant" || role == "system" { role } else { "user".to_string() };
                messages.push(json!({"role": role, "content": text}));
            }
            "function_call" => {
                // 历史回放：assistant tool_calls 消息
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                messages.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": if call_id.is_empty() { format!("call_{}", messages.len()) } else { call_id.to_string() },
                        "type": "function",
                        "function": {"name": name, "arguments": args},
                    }],
                }));
            }
            "function_call_output" => {
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                if call_id.is_empty() {
                    continue; // 无 call_id 的输出无法配对，跳过
                }
                let output = match item.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
            // reasoning / local_shell_call / 其他：跳过（reasoning 不回投，上游自行产生）
            _ => {}
        }
    }
    messages
}

/// content：string 或 content parts 数组（input_text/output_text/refusal → 拼接）
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| p.get("refusal").and_then(|t| t.as_str()))
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 内部 OpenAI completion → Responses 对象（非流式 /v1/responses）
pub fn completion_to_responses(v: &Value, resp_id: &str, model: &str) -> Value {
    let message = v
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let finish = v
        .pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop")
        .to_string();
    // 截断（finish_reason=length）→ status=incomplete + incomplete_details（Responses 规范），
    // 客户端据此区分「正常完成」与「max_output_tokens 截断」
    let (status, incomplete_details) = if finish == "length" {
        ("incomplete", json!({"reason": "max_output_tokens"}))
    } else {
        ("completed", Value::Null)
    };
    let mut output: Vec<Value> = Vec::new();
    if let Some(t) = message.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            output.push(json!({
                "id": format!("msg_{}", resp_id),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": t, "annotations": []}],
            }));
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for (i, tc) in tcs.iter().enumerate() {
            output.push(json!({
                "id": format!("fc_{}_{}", resp_id, i),
                "type": "function_call",
                "status": "completed",
                "call_id": tc.get("id").cloned().unwrap_or(json!(format!("call_{}", i))),
                "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")),
                "arguments": tc.pointer("/function/arguments").cloned().unwrap_or(json!("{}")),
            }));
        }
    }
    let (it, ot) = v.get("usage").map(u64_pair).unwrap_or((0, 0));
    json!({
        "id": resp_id,
        "object": "response",
        "created_at": now_ts(),
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "incomplete_details": incomplete_details,
        "error": null,
        "usage": {
            "input_tokens": it,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": ot,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": it + ot,
        },
        "stop_reason": if finish == "tool_calls" { json!("tool_use") } else { json!(null) },
    })
}

fn u64_pair(u: &Value) -> (u64, u64) {
    let get = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|k| u.get(*k).and_then(|v| v.as_u64()))
            .unwrap_or(0)
    };
    (
        get(&["prompt_tokens", "input_tokens"]),
        get(&["completion_tokens", "output_tokens"]),
    )
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_instructions_string_input_and_tools() {
        let body = json!({
            "model": "glm-5.3",
            "instructions": "You are a coder.",
            "input": "hello",
            "tools": [{"type": "function", "name": "shell", "description": "run", "parameters": {"type": "object"}}],
            "reasoning": {"effort": "high", "summary": "auto"},
            "max_output_tokens": 1024,
        });
        let chat = responses_to_chat(&body).unwrap();
        assert_eq!(chat["messages"][0]["role"], json!("system"));
        assert_eq!(chat["messages"][0]["content"], json!("You are a coder."));
        assert_eq!(chat["messages"][1], json!({"role": "user", "content": "hello"}));
        assert_eq!(chat["tools"][0]["function"]["name"], json!("shell"));
        assert_eq!(chat["reasoning_effort"], json!("high"));
        assert_eq!(chat["max_tokens"], json!(1024));
    }

    #[test]
    fn converts_message_items_and_function_history() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
                {"type": "reasoning", "summary": []},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["content"], json!("list files"));
        assert_eq!(msgs[1]["tool_calls"][0]["id"], json!("c1"));
        assert_eq!(msgs[2]["role"], json!("tool"));
        assert_eq!(msgs[2]["tool_call_id"], json!("c1"));
        assert_eq!(msgs[2]["content"], json!("a.txt"));
        assert_eq!(msgs[3]["content"], json!("done"));
    }

    #[test]
    fn rejects_missing_input_items() {
        assert!(responses_to_chat(&json!({"model": "m", "input": []})).is_err());
        assert!(responses_to_chat(&json!({"model": "m", "input": 42})).is_err());
        // input 缺失 → 宽容降级为空 user 消息
        let chat = responses_to_chat(&json!({"model": "m"})).unwrap();
        assert_eq!(chat["messages"][0]["role"], json!("user"));
    }

    #[test]
    fn completion_maps_to_responses_object() {
        let v = json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3},
        });
        let resp = completion_to_responses(&v, "resp_1", "glm-5.3");
        assert_eq!(resp["object"], json!("response"));
        assert_eq!(resp["status"], json!("completed"));
        assert_eq!(resp["output"][0]["type"], json!("message"));
        assert_eq!(resp["output"][0]["content"][0]["text"], json!("hi"));
        assert_eq!(resp["usage"]["input_tokens"], json!(7));
        assert_eq!(resp["usage"]["total_tokens"], json!(10));
    }

    #[test]
    fn completion_maps_tool_calls_to_function_call_items() {
        let v = json!({
            "choices": [{"index": 0, "finish_reason": "tool_calls",
                         "message": {"role": "assistant", "content": null,
                                     "tool_calls": [{"id": "call_x", "type": "function",
                                                     "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}]}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
        });
        let resp = completion_to_responses(&v, "resp_2", "m");
        assert_eq!(resp["output"][0]["type"], json!("function_call"));
        assert_eq!(resp["output"][0]["call_id"], json!("call_x"));
        assert_eq!(resp["stop_reason"], json!("tool_use"));
    }

    /// finish_reason=length（max_output_tokens 截断）→ status=incomplete + incomplete_details
    #[test]
    fn completion_maps_length_truncation_to_incomplete() {
        let v = json!({
            "choices": [{"index": 0, "finish_reason": "length",
                         "message": {"role": "assistant", "content": "truncat"}}],
        });
        let resp = completion_to_responses(&v, "resp_3", "m");
        assert_eq!(resp["status"], json!("incomplete"));
        assert_eq!(resp["incomplete_details"]["reason"], json!("max_output_tokens"));
        // 正常完成仍是 completed + incomplete_details=null
        let mut v2 = v.clone();
        v2["choices"][0]["finish_reason"] = json!("stop");
        let resp2 = completion_to_responses(&v2, "resp_4", "m");
        assert_eq!(resp2["status"], json!("completed"));
        assert!(resp2["incomplete_details"].is_null());
    }
}
