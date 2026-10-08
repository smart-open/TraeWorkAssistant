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
///
/// 上游对「assistant.tool_calls ↔ role:"tool" 结果」做严格逐条配对校验（issue #69）：
/// 每个 tool_call 的结果必须连续紧跟、逐条对应，中间插入任何其它角色消息即
/// 400 `code:11148 tool calls and tool results do not match`。Codex 在并行工具
/// 调用的 `function_call_output` 之间常夹 `role:"developer"` 提示（典型
/// `<image_resize_notice>`），旧实现 1:1 直投影 + developer 降级成 user 正好打断
/// 配对（多图并行必现，单图不触发）。本实现只调整顺序与合并，不改 tool 内容语义：
/// - 连续 `function_call` 合并进同一条 assistant 消息（tool_calls 累加）；
/// - 配对窗口内（有调用未回填结果）到达的非 tool 消息推迟到该批结果之后按序补回；
///   窗口内的 assistant 文本并入 tool_calls 载体（属于同一轮 assistant 输出）
/// - 工具结果先入缓冲，窗口关闭/收尾时按载体 tool_calls 顺序统一回填（与到达序
///   无关，消除严格按位置校验上游的残余风险），无结果的调用补占位；
/// - 回填结果必须紧跟 assistant，其后才补被推迟的消息（顺序不可换，否则占位结果
///   又被推迟消息隔开、复现 11148）；孤儿/重复 `function_call_output`（无对应
///   未回填调用）直接丢弃
fn convert_input_items(items: &[Value]) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    // 已发出但尚未收到结果的 tool_call_id：非空即处于「工具结果必须连续」窗口。
    // 不变量：pending 非空 ⟺ open_assistant 为 Some
    let mut pending_calls: Vec<String> = Vec::new();
    // 窗口内已收到的工具结果 (call_id, text)，窗口关闭/收尾时按 tool_calls 顺序回填
    let mut results: Vec<(String, String)> = Vec::new();
    // 配对窗口内被推迟的非 tool 消息（role 已降级：developer→user，system 保留）
    let mut deferred: Vec<(String, String)> = Vec::new();
    // 缺 call_id 时的兜底自增序号
    let mut seq: usize = 0;
    // 当前收集 tool_calls 的 assistant 消息下标
    let mut open_assistant: Option<usize> = None;

    for item in items {
        let itype = item.get("type").and_then(|v| v.as_str()).unwrap_or("message");
        match itype {
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        seq += 1;
                        // missing_call_ 前缀防与客户端显式 "call_N" 撞名导致结果配对错乱
                        format!("missing_call_{}", seq)
                    });
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                // 连续多个 function_call 合并进同一条 assistant，避免拆成多条
                let idx = match open_assistant {
                    Some(i) if i < messages.len() => i,
                    _ => {
                        messages.push(json!({
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [],
                        }));
                        messages.len() - 1
                    }
                };
                messages[idx]["tool_calls"]
                    .as_array_mut()
                    .expect("carrier always carries a tool_calls array")
                    .push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": args},
                    }));
                open_assistant = Some(idx);
                pending_calls.push(call_id);
            }
            "function_call_output" => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                let Some(call_id) = call_id else {
                    continue; // 无 call_id 的结果无法配对，跳过
                };
                // 孤儿（无对应调用）或重复结果：丢弃，保持 tool 条数 == tool_calls 条数
                let Some(pos) = pending_calls.iter().position(|c| c == call_id) else {
                    continue;
                };
                let call_id = call_id.to_string();
                let output = tool_output_text(item.get("output"));
                // 结果先入缓冲，窗口关闭时按 tool_calls 顺序统一回填
                pending_calls.remove(pos);
                results.push((call_id, output));
                if pending_calls.is_empty() {
                    // 窗口关闭：先回填结果（紧跟 assistant），再补回被推迟的消息
                    emit_tool_results(&mut messages, open_assistant, &mut results);
                    flush_deferred(&mut messages, &mut deferred);
                    open_assistant = None;
                }
            }
            "message" => {
                let role = item
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("user")
                    .to_string();
                let text = content_text(item.get("content"));
                if pending_calls.is_empty() {
                    let role = if role == "assistant" || role == "system" {
                        role
                    } else {
                        "user".to_string()
                    };
                    messages.push(json!({"role": role, "content": text}));
                } else if role == "assistant" {
                    // 窗口内的 assistant 文本并入 tool_calls 载体（同一轮输出）
                    if !text.is_empty() {
                        if let Some(i) = open_assistant {
                            append_assistant_text(&mut messages[i], &text);
                        }
                    }
                } else if !text.is_empty() {
                    // user/developer/system：推迟到本批工具结果之后补回，保住配对连续性
                    let r = if role == "system" { "system" } else { "user" };
                    deferred.push((r.to_string(), text));
                }
            }
            // reasoning / local_shell_call / 其他：跳过（reasoning 不回投，上游自行产生）
            _ => {}
        }
    }

    // 收尾顺序不可换：先按 tool_calls 顺序回填结果/占位（必须紧跟 assistant.tool_calls），
    // 再补被推迟的非工具消息（属于批次之后）
    emit_tool_results(&mut messages, open_assistant, &mut results);
    flush_deferred(&mut messages, &mut deferred);
    messages
}

/// 配对窗口结束后，把被推迟的非工具消息按原序补回（developer 已降级为 user）
fn flush_deferred(messages: &mut Vec<Value>, deferred: &mut Vec<(String, String)>) {
    for (role, text) in deferred.drain(..) {
        messages.push(json!({"role": role, "content": text}));
    }
}

/// 窗口关闭/收尾时回填工具结果：按载体 tool_calls 顺序输出（与到达序无关），
/// 未收到结果的调用补占位——保证 tool 消息连续且逐条对齐 tool_calls
fn emit_tool_results(
    messages: &mut Vec<Value>,
    open_assistant: Option<usize>,
    results: &mut Vec<(String, String)>,
) {
    let Some(i) = open_assistant else { return };
    let order: Vec<String> = messages[i]
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .map(|tcs| {
            tcs.iter()
                .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for call_id in order {
        // 按 id 逐个弹出：tool_calls 含重复 id 时各取各的结果，不再 find-first 遮蔽
        let text = match results.iter().position(|(c, _)| c == &call_id) {
            Some(pos) => results.remove(pos).1,
            None => "(tool did not return a result)".to_string(),
        };
        messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": text}));
    }
    results.clear();
}

/// 窗口内 assistant 文本并入 tool_calls 载体：content 为字符串则续写，否则直接采用
fn append_assistant_text(msg: &mut Value, text: &str) {
    match msg.get_mut("content") {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str(text);
        }
        _ => msg["content"] = json!(text),
    }
}

/// tool 输出文本化：字符串直取；数组/object 仅拼接 text 部分，input_image 等
/// 非文本内容折叠为占位符——旧实现 `to_string()` 会把含 data URL（可达数 MB）的
/// 内容整段 JSON 序列化塞进 content，撑爆上游载荷；上游 tool content 为纯字符串，
/// 图片本就无法作为多模态透传
fn tool_output_text(output: Option<&Value>) -> String {
    const NON_TEXT: &str = "[non-text content omitted]";
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        Some(t.to_string())
                    } else if p.get("type").and_then(|t| t.as_str()).is_some() {
                        Some(NON_TEXT.to_string())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(Value::Object(o)) => o
            .get("text")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| NON_TEXT.to_string()),
        Some(v) => v.to_string(),
        None => String::new(),
    }
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

    /// issue #69 核心：并行工具调用间夹 developer notice（image_resize_notice），
    /// assistant.tool_calls 后必须紧跟连续 tool 结果，notice 推迟到批次之后
    #[test]
    fn interleaved_developer_notices_do_not_break_tool_pairing() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "describe the screenshots"}]},
                {"type": "function_call", "call_id": "c1", "name": "view_image", "arguments": "{}"},
                {"type": "function_call", "call_id": "c2", "name": "view_image", "arguments": "{}"},
                {"type": "function_call", "call_id": "c3", "name": "view_image", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": [{"type": "input_image", "image_url": "data:image/png;base64,AAA"}]},
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<notice>1</notice>"}]},
                {"type": "function_call_output", "call_id": "c2", "output": [{"type": "input_image", "image_url": "data:image/png;base64,BBB"}]},
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<notice>2</notice>"}]},
                {"type": "function_call_output", "call_id": "c3", "output": [{"type": "input_image", "image_url": "data:image/png;base64,CCC"}]},
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<notice>3</notice>"}]},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // user, assistant(3 tool_calls), tool×3 连续, user notice×3
        assert_eq!(msgs.len(), 8);
        assert_eq!(msgs[1]["role"], json!("assistant"));
        assert_eq!(msgs[1]["tool_calls"].as_array().unwrap().len(), 3);
        for (i, cid) in ["c1", "c2", "c3"].iter().enumerate() {
            assert_eq!(msgs[2 + i]["role"], json!("tool"));
            assert_eq!(msgs[2 + i]["tool_call_id"], json!(cid));
            // output 数组不整串 JSON 化：图片折叠为占位符，不携带 data URL
            let content = msgs[2 + i]["content"].as_str().unwrap();
            assert!(!content.contains("image_url"));
            assert!(content.contains("omitted"));
        }
        for (i, n) in ["1", "2", "3"].iter().enumerate() {
            assert_eq!(msgs[5 + i]["role"], json!("user"));
            assert_eq!(msgs[5 + i]["content"], json!(format!("<notice>{}</notice>", n)));
        }
    }

    /// 缺结果补占位 + 收尾顺序：占位 tool 必须先于被推迟消息（否则占位结果又被
    /// 推迟消息隔开，再次打断配对——参考补丁的收尾顺序缺陷回归）
    #[test]
    fn missing_outputs_get_placeholders_before_deferred_messages() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": "go"},
                {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
                {"type": "function_call", "call_id": "c2", "name": "b", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "trailing note"}]},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // user, assistant(2 calls), tool c1, tool c2(占位), user(trailing note)
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[1]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[2]["tool_call_id"], json!("c1"));
        assert_eq!(msgs[2]["content"], json!("ok"));
        assert_eq!(msgs[3]["role"], json!("tool"));
        assert_eq!(msgs[3]["tool_call_id"], json!("c2"));
        assert_eq!(msgs[3]["content"], json!("(tool did not return a result)"));
        assert_eq!(msgs[4]["role"], json!("user"));
        assert_eq!(msgs[4]["content"], json!("trailing note"));
    }

    /// 孤儿（无对应调用）与重复 function_call_output 丢弃，tool 条数与 tool_calls 对齐
    #[test]
    fn orphan_and_duplicate_outputs_are_dropped() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "first"},
                {"type": "function_call_output", "call_id": "c1", "output": "dup"},
                {"type": "function_call_output", "call_id": "ghost", "output": "orphan"},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // assistant(tc c1), tool c1 —— 仅一条结果
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"], json!("first"));
    }

    /// 窗口内的 assistant 文本并入 tool_calls 载体，不拆出独立消息打断配对
    #[test]
    fn assistant_text_during_tool_window_merges_into_carrier() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": "go"},
                {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "thinking..."}]},
                {"type": "function_call_output", "call_id": "c1", "output": "done"},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], json!("assistant"));
        assert_eq!(msgs[1]["content"], json!("thinking..."));
        assert_eq!(msgs[1]["tool_calls"][0]["id"], json!("c1"));
        assert_eq!(msgs[2]["role"], json!("tool"));
    }

    /// 多批次工具调用 + 批间 user 消息：各批次独立成对，批间消息不推迟
    #[test]
    fn sequential_tool_batches_with_interleaved_user_messages() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
                {"type": "function_call", "call_id": "c2", "name": "b", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c2", "output": "r2"},
                {"type": "function_call_output", "call_id": "c1", "output": "r1"},
                {"type": "message", "role": "user", "content": "next"},
                {"type": "function_call", "call_id": "c3", "name": "c", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c3", "output": "r3"},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // 乱序结果（c2 先到）在窗口关闭时按 tool_calls 顺序重排：
        // assistant(tc c1,c2), tool c1, tool c2, user, assistant(tc c3), tool c3
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[0]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["tool_call_id"], json!("c1"));
        assert_eq!(msgs[1]["content"], json!("r1"));
        assert_eq!(msgs[2]["tool_call_id"], json!("c2"));
        assert_eq!(msgs[2]["content"], json!("r2"));
        assert_eq!(msgs[3]["role"], json!("user"));
        assert_eq!(msgs[4]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[5]["tool_call_id"], json!("c3"));
    }

    /// 窗口内 system 消息推迟补回时保留 system 角色，developer 降级 user
    #[test]
    fn system_message_inside_tool_window_keeps_role_after_deferral() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": "go"},
                {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
                {"type": "message", "role": "system", "content": "sys note"},
                {"type": "message", "role": "developer", "content": "dev note"},
                {"type": "function_call_output", "call_id": "c1", "output": "done"},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // user, assistant(tc c1), tool c1, system, user(dev note)
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[2]["role"], json!("tool"));
        assert_eq!(msgs[2]["tool_call_id"], json!("c1"));
        assert_eq!(msgs[3]["role"], json!("system"));
        assert_eq!(msgs[3]["content"], json!("sys note"));
        assert_eq!(msgs[4]["role"], json!("user"));
        assert_eq!(msgs[4]["content"], json!("dev note"));
    }

    /// function_call 缺 call_id：missing_call_N 兜底自增，不与客户端显式 "call_N" 撞名，
    /// 两路调用各配各的结果（旧 call_N 兜底会撞名遮蔽显式 id 的结果）
    #[test]
    fn function_call_without_call_id_falls_back_to_sequential_id() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "name": "a", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_1", "name": "b", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "missing_call_1", "output": "r1"},
                {"type": "function_call_output", "call_id": "call_1", "output": "r2"},
            ],
        });
        let chat = responses_to_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // 两次调用合并进同一条 assistant，窗口关闭后按 tool_calls 顺序回填两条结果
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["tool_calls"][0]["id"], json!("missing_call_1"));
        assert_eq!(msgs[0]["tool_calls"][1]["id"], json!("call_1"));
        assert_eq!(msgs[1]["tool_call_id"], json!("missing_call_1"));
        assert_eq!(msgs[1]["content"], json!("r1"));
        assert_eq!(msgs[2]["tool_call_id"], json!("call_1"));
        assert_eq!(msgs[2]["content"], json!("r2"));
    }
}
