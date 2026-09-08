use std::io::{BufRead, BufReader, Read};

use serde_json::{json, Map, Value};

/// SOLO SSE 单事件
struct SoloEvent {
    event: String,
    response: String,
    reasoning: String,
    tool_calls: Option<Value>,
    usage: Option<Value>,
    finish_reason: String,
    error_code: Option<i64>,
    error_message: String,
}

impl SoloEvent {
    fn new(event: &str) -> Self {
        Self {
            event: event.to_string(),
            response: String::new(),
            reasoning: String::new(),
            tool_calls: None,
            usage: None,
            finish_reason: String::new(),
            error_code: None,
            error_message: String::new(),
        }
    }
}

/// SSE 跨行状态
struct SseState {
    event: String,
    data: String,
}

impl SseState {
    fn new() -> Self {
        Self {
            event: String::new(),
            data: String::new(),
        }
    }

    fn reset(&mut self) {
        self.event.clear();
        self.data.clear();
    }
}

/// 处理一行，返回触发的事件（空行时解析并返回）
fn scan_line(st: &mut SseState, line: &str) -> Option<SoloEvent> {
    if line.is_empty() {
        if st.event.is_empty() {
            st.reset();
            return None;
        }
        let ev = parse_solo_line(&st.event, &st.data);
        st.reset();
        return ev;
    }
    if let Some(rest) = line.strip_prefix("event:") {
        st.event = rest.trim().to_string();
    } else if let Some(rest) = line.strip_prefix("data:") {
        st.data.push_str(rest);
    }
    None
}

fn parse_solo_line(event: &str, data: &str) -> Option<SoloEvent> {
    let mut ev = SoloEvent::new(event.trim());
    if data.is_empty() {
        return Some(ev);
    }
    let raw: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let obj = match raw.as_object() {
        Some(o) => o,
        None => return Some(ev),
    };
    match ev.event.as_str() {
        "output" => {
            if let Some(s) = obj.get("response").and_then(|v| v.as_str()) {
                ev.response = s.to_string();
            }
            if let Some(s) = obj.get("reasoning_content").and_then(|v| v.as_str()) {
                ev.reasoning = s.to_string();
            }
            if let Some(tc) = obj.get("tool_calls") {
                if !tc.is_null() {
                    ev.tool_calls = Some(tc.clone());
                }
            }
        }
        "thought" => {
            // create_agent_task 曾使用 event:thought，llm_utils_chat 使用 event:output
            // 保留 thought 处理以兼容两种端点
            if let Some(s) = obj.get("thought").and_then(|v| v.as_str()) {
                ev.response = s.to_string();
            }
            if let Some(s) = obj.get("reasoning_content").and_then(|v| v.as_str()) {
                ev.reasoning = s.to_string();
            }
            if let Some(tc) = obj.get("tool_calls") {
                if !tc.is_null() {
                    ev.tool_calls = Some(tc.clone());
                }
            }
        }
        "token_usage" => {
            ev.usage = Some(raw.clone());
        }
        "done" => {
            if let Some(s) = obj.get("finish_reason").and_then(|v| v.as_str()) {
                ev.finish_reason = s.to_string();
            }
        }
        "turn_completion" => {
            // create_agent_task 曾使用 event:turn_completion，保留兼容
            ev.finish_reason = "stop".to_string();
        }
        "error" => {
            ev.error_code = obj.get("code").and_then(|v| v.as_i64());
            ev.error_message = obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
        _ => {}
    }
    Some(ev)
}

/// 流式转换：SOLO SSE → OpenAI SSE chunks，逐 chunk 通过 sender 发送
///
/// 返回 (错误信息, 是否已向客户端发送过数据)。
/// 若上游首个事件即 error（尚未发送任何数据），错误不下发，
/// 由调用方决定重试（如 4001 改 function）或透传给客户端。
pub fn stream_convert<R: Read + Send>(
    reader: R,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    chat_id: &str,
) -> (Option<(i64, String)>, bool) {
    let br = BufReader::new(reader);
    let mut st = SseState::new();
    let mut pending_usage: Option<Value> = None;
    let mut saw_done = false;
    let mut sent_any = false;
    let mut error_info: Option<(i64, String)> = None;

    let write_chunk = |delta: Value, finish: &str, pending_usage: &Option<Value>| -> String {
        let mut choices = vec![json!({
            "index": 0,
            "delta": delta,
        })];
        if !finish.is_empty() {
            if let Some(c) = choices.get_mut(0).and_then(|c| c.as_object_mut()) {
                c.insert("finish_reason".into(), json!(finish));
            }
        }
        let mut chunk = json!({
            "id": chat_id,
            "object": "chat.completion.chunk",
            "created": now_ts(),
            "model": "",
            "choices": choices,
        });
        if pending_usage.is_some() {
            if let Some(c) = chunk.as_object_mut() {
                c.insert("usage".into(), pending_usage.clone().unwrap());
            }
        }
        format!("data: {}\n\n", chunk)
    };

    for line in br.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Some(ev) = scan_line(&mut st, &line.trim_end()) {
            match ev.event.as_str() {
                "output" | "thought" => {
                    let mut delta = Map::new();
                    if !ev.response.is_empty() {
                        delta.insert("content".into(), json!(ev.response));
                    }
                    if !ev.reasoning.is_empty() {
                        delta.insert("reasoning_content".into(), json!(ev.reasoning));
                    }
                    if let Some(tc) = &ev.tool_calls {
                        if let Some(arr) = tc.as_array() {
                            let converted: Vec<Value> = arr
                                .iter()
                                .map(|call| {
                                    let mut c = call.clone();
                                    if let Some(fc) = c.get("function_call").cloned() {
                                        if let Some(obj) = c.as_object_mut() {
                                            obj.insert("function".into(), fc);
                                            obj.remove("function_call");
                                        }
                                    }
                                    if let Some(fn_obj) = c.get("function").and_then(|f| f.as_object()).cloned() {
                                        let mut clean = fn_obj.clone();
                                        clean.remove("namespace");
                                        clean.remove("partial_arguments");
                                        if let Some(obj) = c.as_object_mut() {
                                            obj.insert("function".into(), Value::Object(clean));
                                        }
                                    }
                                    c
                                })
                                .collect();
                            if !converted.is_empty() {
                                delta.insert("tool_calls".into(), json!(converted));
                            }
                        }
                    }
                    if !delta.is_empty() {
                        let data = write_chunk(Value::Object(delta), "", &pending_usage);
                        let _ = sender.blocking_send(Ok(bytes::Bytes::from(data)));
                        sent_any = true;
                    } else if !sent_any {
                        // 空 delta 的首个 output：上游已开始产出，
                        // 发出仅含 role 的空 chunk 占位，让 sent_any 语义与真实下发一致
                        let data = write_chunk(json!({ "role": "assistant" }), "", &pending_usage);
                        let _ = sender.blocking_send(Ok(bytes::Bytes::from(data)));
                        sent_any = true;
                    }
                }
                "token_usage" => {
                    pending_usage = Some(json!(ev.usage.clone().unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    let data = write_chunk(json!({}), &ev.finish_reason, &pending_usage);
                    let _ = sender.blocking_send(Ok(bytes::Bytes::from(data)));
                    let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                    saw_done = true;
                    sent_any = true;
                }
                "error" => {
                    error_info = Some((ev.error_code.unwrap_or(0), ev.error_message.clone()));
                    // 已有数据流出：就地透传错误并结束；否则延迟给调用方决策（可重试）
                    if sent_any {
                        let error_chunk = json!({
                            "error": {
                                "message": ev.error_message,
                                "type": "api_error",
                                "code": ev.error_code.unwrap_or(0),
                            }
                        });
                        let _ = sender.blocking_send(Ok(bytes::Bytes::from(format!(
                            "data: {}\n\n",
                            error_chunk
                        ))));
                        let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                        saw_done = true;
                    }
                }
                _ => {}
            }
        }
    }

    if !saw_done && error_info.is_none() {
        let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
    }

    (error_info.map(|(code, msg)| (code, msg)), sent_any)
}

/// 非流式聚合：读取完整 SOLO SSE，聚合为单个 OpenAI chat.completion
pub fn aggregate<R: Read + Send>(
    reader: R,
    chat_id: &str,
) -> (Option<Value>, Option<(i64, String)>) {
    let br = BufReader::new(reader);
    let mut st = SseState::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = "stop".to_string();
    let mut usage: Option<Value> = None;
    let mut error_info: Option<(i64, String)> = None;

    for line in br.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Some(ev) = scan_line(&mut st, &line.trim_end()) {
            match ev.event.as_str() {
                "output" | "thought" => {
                    content.push_str(&ev.response);
                    reasoning.push_str(&ev.reasoning);
                }
                "token_usage" => {
                    usage = Some(json!(ev.usage.unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    if !ev.finish_reason.is_empty() {
                        finish_reason = ev.finish_reason;
                    }
                }
                "error" => {
                    error_info = Some((ev.error_code.unwrap_or(0), ev.error_message));
                }
                _ => {}
            }
        }
    }

    if let Some((code, msg)) = &error_info {
        return (None, Some((*code, msg.clone())));
    }

    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }

    let mut resp = json!({
        "id": chat_id,
        "object": "chat.completion",
        "created": now_ts(),
        "model": "",
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
    });
    if let Some(u) = usage {
        resp["usage"] = u;
    }

    (Some(resp), None)
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ==================== Anthropic 协议输出（F-39：+Anthropic 适配） ====================

/// 工具调用缓冲：跨事件合并同名/同序工具的分片参数
#[derive(Default)]
struct ToolBuf {
    id: String,
    name: String,
    args: String,
    /// 上游分片的 index（无 index 时按事件内位置）
    slot: usize,
}

fn anthropic_stop_reason(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}

fn usage_i64(u: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| u.get(*k).and_then(|v| v.as_i64()))
}

fn anthropic_event(name: &str, data: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", name, data)
}

/// 流式转换：SOLO SSE → Anthropic Messages SSE（/v1/messages）
/// 事件序列：message_start → content_block_start/delta/stop… → message_delta → message_stop
/// 注：reasoning_content 暂不输出（Anthropic thinking 块需签名，严格客户端会拒绝未签名的 thinking_delta）
// 宏内末次赋值（message_started/text_block_open）在收尾路径后不再读取，属预期行为
#[allow(unused_assignments)]
pub fn stream_convert_anthropic<R: Read + Send>(
    reader: R,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    msg_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool) {
    let br = BufReader::new(reader);
    let mut st = SseState::new();
    let mut message_started = false;
    let mut text_block_open = false;
    let mut block_index: i64 = -1;
    let mut tools: Vec<ToolBuf> = Vec::new();
    let mut usage: Option<Value> = None;
    let mut finish_reason = String::new();
    let mut saw_done = false;
    let mut error_info: Option<(i64, String)> = None;

    macro_rules! send {
        ($s:expr) => {
            let _ = sender.blocking_send(Ok(bytes::Bytes::from($s)));
        };
    }

    macro_rules! send_event {
        ($name:expr, $data:expr) => {
            send!(anthropic_event($name, &$data));
        };
    }

    macro_rules! start_message {
        () => {
            if !message_started {
                message_started = true;
                send_event!(
                    "message_start",
                    json!({
                        "type": "message_start",
                        "message": {
                            "id": msg_id,
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "content": [],
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": { "input_tokens": 0, "output_tokens": 0 },
                        },
                    })
                );
            }
        };
    }

    macro_rules! open_text_block {
        () => {{
            block_index += 1;
            text_block_open = true;
            send_event!(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": { "type": "text", "text": "" },
                })
            );
        }};
    }

    macro_rules! close_text_block {
        () => {
            if text_block_open {
                text_block_open = false;
                send_event!(
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": block_index })
                );
            }
        };
    }

    macro_rules! finish_stream {
        () => {{
            close_text_block!();
            // 工具块：text 之后统一追加（content_block_start + input_json_delta + stop）
            for (i, t) in tools.iter().enumerate() {
                let idx = block_index + 1 + i as i64;
                let id = if t.id.is_empty() {
                    format!("toolu_{}_{}", msg_id, i)
                } else {
                    t.id.clone()
                };
                send_event!(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": { "type": "tool_use", "id": id, "name": t.name, "input": {} },
                    })
                );
                if !t.args.is_empty() {
                    send_event!(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "input_json_delta", "partial_json": t.args },
                        })
                    );
                }
                send_event!(
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": idx })
                );
            }
            let mut out_tokens = None;
            if let Some(u) = &usage {
                out_tokens = usage_i64(u, &["output_tokens", "completion_tokens"]);
            }
            let mut message_delta = json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": anthropic_stop_reason(&finish_reason),
                    "stop_sequence": null,
                },
                "usage": {},
            });
            if let Some(o) = out_tokens {
                message_delta["usage"]["output_tokens"] = json!(o);
            }
            send_event!("message_delta", message_delta);
            send_event!("message_stop", json!({ "type": "message_stop" }));
        }};
    }

    for line in br.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Some(ev) = scan_line(&mut st, &line.trim_end()) {
            match ev.event.as_str() {
                "output" | "thought" => {
                    if !ev.response.is_empty() {
                        start_message!();
                        if !text_block_open {
                            open_text_block!();
                        }
                        send_event!(
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": block_index,
                                "delta": { "type": "text_delta", "text": ev.response },
                            })
                        );
                    }
                    // reasoning_content：跳过（见函数注释）
                    if let Some(tc) = &ev.tool_calls {
                        if let Some(arr) = tc.as_array() {
                            for (pos, call) in arr.iter().enumerate() {
                                let mut c = call.clone();
                                if let Some(fc) = c.get("function_call").cloned() {
                                    if let Some(obj) = c.as_object_mut() {
                                        obj.insert("function".into(), fc);
                                        obj.remove("function_call");
                                    }
                                }
                                let fn_obj = c.get("function").and_then(|f| f.as_object()).cloned();
                                let fn_obj = match fn_obj {
                                    Some(mut f) => {
                                        f.remove("namespace");
                                        f.remove("partial_arguments");
                                        f
                                    }
                                    None => continue,
                                };
                                let name = fn_obj
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args = fn_obj
                                    .get("arguments")
                                    .and_then(|a| a.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let id = c
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let idx = c
                                    .get("index")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as usize)
                                    .unwrap_or(pos);
                                // 找到同 index 的缓冲（或新建）
                                if let Some(existing) = tools.iter_mut().find(|t| t.slot == idx) {
                                    if existing.name.is_empty() {
                                        existing.name = name;
                                    }
                                    if existing.id.is_empty() {
                                        existing.id = id;
                                    }
                                    existing.args.push_str(&args);
                                } else {
                                    tools.push(ToolBuf {
                                        id,
                                        name,
                                        args: args,
                                        slot: idx,
                                    });
                                }
                                // 工具调用出现时先收口文本块
                                close_text_block!();
                            }
                        }
                    }
                }
                "token_usage" => {
                    usage = Some(json!(ev.usage.clone().unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    if !ev.finish_reason.is_empty() {
                        finish_reason = ev.finish_reason;
                    }
                    start_message!();
                    finish_stream!();
                    saw_done = true;
                }
                "error" => {
                    error_info = Some((ev.error_code.unwrap_or(0), ev.error_message.clone()));
                    // 已有内容发出：就地透传错误并结束；否则延迟给调用方决策（可重试）
                    if message_started || !tools.is_empty() {
                        send_event!(
                            "error",
                            json!({
                                "type": "error",
                                "error": {
                                    "type": "api_error",
                                    "message": ev.error_message,
                                },
                            })
                        );
                        saw_done = true;
                    }
                }
                _ => {}
            }
        }
    }

    if !saw_done {
        if message_started || !tools.is_empty() {
            // 上游未发 done 即断流：把已收到的内容按正常收尾发出，避免客户端挂起
            finish_stream!();
        } else if error_info.is_none() {
            // 空流：发一个空的合法 message
            start_message!();
            finish_stream!();
        }
    }

    // sent_any 仅统计真实发出过的事件：
    // tools 只是聚合缓冲（input 尚未发送），不能视为已向客户端输出
    let sent_any = message_started;
    (error_info.map(|(code, msg)| (code, msg)), sent_any)
}

/// 非流式聚合：SOLO SSE → Anthropic message 对象（/v1/messages）
pub fn aggregate_anthropic<R: Read + Send>(
    reader: R,
    msg_id: &str,
    model: &str,
) -> (Option<Value>, Option<(i64, String)>) {
    let br = BufReader::new(reader);
    let mut st = SseState::new();
    let mut content = String::new();
    let mut finish_reason = "stop".to_string();
    let mut usage: Option<Value> = None;
    let mut tools: Vec<ToolBuf> = Vec::new();
    let mut error_info: Option<(i64, String)> = None;

    for line in br.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Some(ev) = scan_line(&mut st, &line.trim_end()) {
            match ev.event.as_str() {
                "output" | "thought" => {
                    content.push_str(&ev.response);
                    if let Some(tc) = &ev.tool_calls {
                        if let Some(arr) = tc.as_array() {
                            for (pos, call) in arr.iter().enumerate() {
                                let mut c = call.clone();
                                if let Some(fc) = c.get("function_call").cloned() {
                                    if let Some(obj) = c.as_object_mut() {
                                        obj.insert("function".into(), fc);
                                        obj.remove("function_call");
                                    }
                                }
                                let fn_obj = c.get("function").and_then(|f| f.as_object()).cloned();
                                let fn_obj = match fn_obj {
                                    Some(mut f) => {
                                        f.remove("namespace");
                                        f.remove("partial_arguments");
                                        f
                                    }
                                    None => continue,
                                };
                                let name = fn_obj
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args = fn_obj
                                    .get("arguments")
                                    .and_then(|a| a.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let id = c
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let idx = c
                                    .get("index")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as usize)
                                    .unwrap_or(pos);
                                if let Some(existing) = tools.iter_mut().find(|t| t.slot == idx) {
                                    if existing.name.is_empty() {
                                        existing.name = name;
                                    }
                                    if existing.id.is_empty() {
                                        existing.id = id;
                                    }
                                    existing.args.push_str(&args);
                                } else {
                                    tools.push(ToolBuf {
                                        id,
                                        name,
                                        args,
                                        slot: idx,
                                    });
                                }
                            }
                        }
                    }
                }
                "token_usage" => {
                    usage = Some(json!(ev.usage.unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    if !ev.finish_reason.is_empty() {
                        finish_reason = ev.finish_reason;
                    }
                }
                "error" => {
                    error_info = Some((ev.error_code.unwrap_or(0), ev.error_message));
                }
                _ => {}
            }
        }
    }

    if let Some((code, msg)) = &error_info {
        return (None, Some((*code, msg.clone())));
    }

    let mut blocks: Vec<Value> = Vec::new();
    if !content.is_empty() {
        blocks.push(json!({ "type": "text", "text": content }));
    }
    for (i, t) in tools.iter().enumerate() {
        let input: Value = serde_json::from_str(t.args.trim()).unwrap_or(json!({}));
        let id = if t.id.is_empty() {
            format!("toolu_{}_{}", msg_id, i)
        } else {
            t.id.clone()
        };
        blocks.push(json!({ "type": "tool_use", "id": id, "name": t.name, "input": input }));
    }

    let mut usage_obj = json!({ "input_tokens": 0, "output_tokens": 0 });
    if let Some(u) = &usage {
        if let Some(v) = usage_i64(u, &["input_tokens", "prompt_tokens"]) {
            usage_obj["input_tokens"] = json!(v);
        }
        if let Some(v) = usage_i64(u, &["output_tokens", "completion_tokens"]) {
            usage_obj["output_tokens"] = json!(v);
        }
    }

    let resp = json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        "stop_reason": anthropic_stop_reason(&finish_reason),
        "stop_sequence": null,
        "usage": usage_obj,
    });

    (Some(resp), None)
}
