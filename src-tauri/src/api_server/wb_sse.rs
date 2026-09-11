//! WorkBuddy 上游 SSE 解析与协议输出（T2.1/F-28）
//!
//! WB 上游 `/v2/chat/completions` 只回 OpenAI 风格 SSE（`data: {chunk}` …
//! `data: [DONE]`），与 SOLO 上游的事件格式（event:output/thought/token_usage）
//! 不同，故独立实现：
//! - 流式：OpenAI chunk 归一化后按客户端协议转发（OpenAI / text / Anthropic）；
//! - 非流式：本地聚合为单个 OpenAI completion（tool_calls delta 按 index 合并、
//!   帧归一化），再按协议转换（§3.9 ①）。
//!
//! 输入统一为行迭代器（便于首字超时等上层封装），不直接持有 Reader。

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// WB 上游单个 SSE 事件（已归一化）
#[derive(Debug)]
pub enum WbEvent {
    /// 标准 OpenAI chunk：choices[0] 的 delta + finish_reason + 顶层 usage
    Chunk {
        delta: Value,
        finish: String,
        usage: Option<Value>,
        /// 客户端请求 id（原样透传）
        id: String,
    },
    Error {
        code: i64,
        msg: String,
    },
    Done,
}

/// 行迭代 → 事件流的解析器（SSE 多行 data 合并；`: ` 注释行跳过）
pub struct WbSseParser<L: Iterator<Item = String>> {
    lines: L,
    /// 跨行 data 缓冲
    data: String,
    done: bool,
}

impl<L: Iterator<Item = String>> WbSseParser<L> {
    pub fn new(lines: L) -> Self {
        Self { lines, data: String::new(), done: false }
    }

    pub fn next_event(&mut self) -> Option<WbEvent> {
        if self.done {
            return None;
        }
        loop {
            let line = self.lines.next()?;
            let line = line.trim_end();
            if line.is_empty() {
                if self.data.is_empty() {
                    continue;
                }
                let payload = std::mem::take(&mut self.data);
                if let Some(ev) = parse_data(&payload) {
                    if matches!(ev, WbEvent::Done) {
                        self.done = true;
                    }
                    return Some(ev);
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // SSE 注释（keep-alive 等）
            }
            if let Some(rest) = line.strip_prefix("data:") {
                self.data.push_str(rest.trim_start());
                // 紧凑流兼容（部分上游不按「空行分隔」发帧）：
                // 缓冲已是完整载荷（[DONE] 或完整 JSON）→ 立即产出，不等空行；
                // 否则继续按 SSE 规范做多行 data 拼接
                let t = self.data.trim();
                let complete = t == "[DONE]"
                    || (t.starts_with('{') && serde_json::from_str::<Value>(t).is_ok());
                if complete {
                    let payload = std::mem::take(&mut self.data);
                    if let Some(ev) = parse_data(&payload) {
                        if matches!(ev, WbEvent::Done) {
                            self.done = true;
                        }
                        return Some(ev);
                    }
                }
            }
            // 其余行（event:/id:/retry:/未知）忽略：WB 上游标准 OpenAI 流
        }
    }
}

/// 解析单条 data 载荷
fn parse_data(data: &str) -> Option<WbEvent> {
    let t = data.trim();
    if t.is_empty() {
        return None;
    }
    if t == "[DONE]" {
        return Some(WbEvent::Done);
    }
    let v: Value = match serde_json::from_str(t) {
        Ok(v) => v,
        Err(_) => return None,
    };
    // 错误帧：{"error": {...}}（与 OpenAI 一致；兼容顶层 code/message）
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .or_else(|| err.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        return Some(WbEvent::Error { code, msg });
    }
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first());
    let delta = choice
        .and_then(|c| c.get("delta"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("")
        .to_string();
    let usage = v.get("usage").filter(|u| u.is_object()).cloned();
    let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
    Some(WbEvent::Chunk { delta, finish, usage, id })
}

type Sender = tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>;

// ==================== Responses SSE 投影（T4.1/F-40） ====================

/// Responses SSE 事件帧：event + data（type 与 event 同名，Codex 按 data.type 解析）
fn resp_send(tx: &Sender, event: &str, data: Value) {
    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        event, data
    ))));
}

/// 构造 Responses response 对象（created/in_progress/completed/failed 共用骨架）
fn responses_object(id: &str, model: &str, status: &str, output: Vec<Value>, usage: Option<&Value>) -> Value {
    let mut obj = json!({
        "id": id,
        "object": "response",
        "created_at": now_ts(),
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "incomplete_details": null,
        "error": null,
    });
    if let Some(u) = usage {
        let (it, ot) = u64_pair(u);
        obj["usage"] = json!({
            "input_tokens": it,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": ot,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": it + ot,
        });
    }
    obj
}

/// 流式转发：WB SSE → 客户端协议帧
///
/// 返回 (流内错误, 是否已发送过数据, usage)。错误在「尚未发送任何数据」时
/// 不下发（留给调用方故障转移），与 sse.rs 同一语义。
// 宏内末次赋值（text_block_open）在收尾路径后不再读取，属预期行为（对齐 sse.rs）
#[allow(unused_assignments)]
pub fn stream_forward<L: Iterator<Item = String>>(
    lines: L,
    tx: &Sender,
    proto: crate::api_server::routes::Protocol,
    chat_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    let mut parser = WbSseParser::new(lines);
    let mut sent_any = false;
    let mut usage: Option<Value> = None;
    let mut error_info: Option<(i64, String)> = None;

    macro_rules! send {
        ($s:expr) => {
            let _ = tx.blocking_send(Ok(bytes::Bytes::from($s)));
        };
    }

    // Anthropic 输出状态
    let mut message_started = false;
    let mut text_block_open = false;
    // T5.3/F-62：思考链 thinking 块（在文本块之前输出）
    let mut thinking_block_open = false;
    let mut block_index: i64 = -1;

    let anthropic_start = |tx: &Sender, message_started: &mut bool, chat_id: &str, model: &str| {
        if !*message_started {
            *message_started = true;
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: message_start\ndata: {}\n\n",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": chat_id, "type": "message", "role": "assistant",
                        "model": model, "content": [],
                        "stop_reason": null, "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    },
                })
            ))));
        }
    };

    // Anthropic tool_use 块缓冲（index → (id,name,args)）
    let mut tool_buf: BTreeMap<i64, (String, String, String)> = BTreeMap::new();

    // Responses 输出状态（T4.1/F-40）
    let mut resp_created = false;
    let mut resp_msg_open = false;
    let mut resp_text = String::new();
    let mut resp_output_index: i64 = -1;

    macro_rules! resp_ensure_created {
        () => {
            if !resp_created {
                resp_created = true;
                resp_send(
                    tx,
                    "response.created",
                    json!({
                        "type": "response.created",
                        "response": responses_object(chat_id, model, "in_progress", vec![], None),
                        "sequence_number": 0,
                    }),
                );
            }
        };
    }

    loop {
        match parser.next_event() {
            None => break,
            Some(WbEvent::Done) => {
                match proto {
                    crate::api_server::routes::Protocol::Anthropic => {
                        anthropic_start(tx, &mut message_started, chat_id, model);
                        if thinking_block_open {
                            thinking_block_open = false;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_stop\ndata: {}\n\n",
                                json!({"type":"content_block_stop","index":block_index})
                            ))));
                        }
                        if text_block_open {
                            text_block_open = false;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_stop\ndata: {}\n\n",
                                json!({"type":"content_block_stop","index":block_index})
                            ))));
                        }
                        // 工具块统一在文本块之后输出
                        for (i, (_k, (tid, name, args))) in tool_buf.iter().enumerate() {
                            let idx = block_index + 1 + i as i64;
                            let id = if tid.is_empty() { format!("toolu_{}_{}", chat_id, i) } else { tid.clone() };
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_start\ndata: {}\n\n",
                                json!({"type":"content_block_start","index":idx,
                                       "content_block":{"type":"tool_use","id":id,"name":name,"input":{}}})
                            ))));
                            if !args.is_empty() {
                                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                    "event: content_block_delta\ndata: {}\n\n",
                                    json!({"type":"content_block_delta","index":idx,
                                           "delta":{"type":"input_json_delta","partial_json":args}})
                                ))));
                            }
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_stop\ndata: {}\n\n",
                                json!({"type":"content_block_stop","index":idx})
                            ))));
                        }
                        let (it, ot) = usage.as_ref().map(u64_pair).unwrap_or((0, 0));
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: message_delta\ndata: {}\n\n",
                            json!({
                                "type":"message_delta",
                                "delta":{"stop_reason":"end_turn","stop_sequence":null},
                                "usage":{"output_tokens":ot},
                            })
                        ))));
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: message_stop\ndata: {}\n\n",
                            json!({"type":"message_stop"})
                        ))));
                        let _ = (it, ot);
                    }
                    crate::api_server::routes::Protocol::Responses => {
                        // 未产出任何 chunk 也补 created，保证事件序列完整
                        resp_ensure_created!();
                        let mut output: Vec<Value> = Vec::new();
                        if resp_msg_open {
                            output.push(json!({
                                "id": format!("{}_msg_0", chat_id),
                                "type": "message",
                                "status": "completed",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": resp_text, "annotations": []}],
                            }));
                            resp_send(tx, "response.output_item.done", json!({
                                "type": "response.output_item.done",
                                "output_index": 0,
                                "item": output[0],
                            }));
                        }
                        // 工具调用缓冲统一在文本项之后完整输出
                        for (i, (_k, (tid, name, args))) in tool_buf.iter().enumerate() {
                            let idx = output.len() as i64;
                            let item_id = if tid.is_empty() {
                                format!("{}_fc_{}", chat_id, i)
                            } else {
                                tid.clone()
                            };
                            let item = json!({
                                "id": item_id,
                                "type": "function_call",
                                "status": "completed",
                                "call_id": if tid.is_empty() { format!("call_{}_{}", chat_id, i) } else { tid.clone() },
                                "name": name,
                                "arguments": args,
                            });
                            let mut added = item.clone();
                            added["status"] = json!("in_progress");
                            added["arguments"] = json!("");
                            resp_send(tx, "response.output_item.added", json!({
                                "type": "response.output_item.added",
                                "output_index": idx,
                                "item": added,
                            }));
                            if !args.is_empty() {
                                resp_send(tx, "response.function_call_arguments.delta", json!({
                                    "type": "response.function_call_arguments.delta",
                                    "item_id": item["id"],
                                    "output_index": idx,
                                    "delta": args,
                                }));
                            }
                            resp_send(tx, "response.output_item.done", json!({
                                "type": "response.output_item.done",
                                "output_index": idx,
                                "item": item,
                            }));
                            output.push(item);
                        }
                        resp_send(tx, "response.completed", json!({
                            "type": "response.completed",
                            "response": responses_object(chat_id, model, "completed", output, usage.as_ref()),
                        }));
                    }
                    _ => {
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                    }
                }
                sent_any = true;
                break;
            }
            Some(WbEvent::Error { code, msg }) => {
                if sent_any {
                    // 已有数据流出：就地透传错误并收尾
                    match proto {
                        crate::api_server::routes::Protocol::Anthropic => {
                            let err = json!({"type":"error","error":{"type":"api_error","message":msg}});
                            send!(format!("event: error\ndata: {}\n\n", err));
                        }
                        crate::api_server::routes::Protocol::Responses => {
                            // 流内错误 → response.failed（Responses 无 [DONE] 帧）
                            resp_ensure_created!();
                            let mut resp = responses_object(chat_id, model, "failed", vec![], None);
                            resp["error"] = json!({"code": code.to_string(), "message": msg});
                            resp_send(tx, "response.failed", json!({
                                "type": "response.failed",
                                "response": resp,
                            }));
                        }
                        _ => {
                            let body = json!({"error":{"message":msg,"type":"api_error","code":code}});
                            send!(format!("data: {}\n\n", body));
                            send!("data: [DONE]\n\n");
                        }
                    }
                    sent_any = true;
                } else {
                    error_info = Some((code, msg));
                }
                break;
            }
            Some(WbEvent::Chunk { delta, finish, usage: u, id }) => {
                if let Some(x) = u {
                    usage = Some(x);
                }
                if proto == crate::api_server::routes::Protocol::Anthropic {
                    anthropic_start(tx, &mut message_started, chat_id, model);
                    // T5.3/F-62：思考链增量 → thinking block（先于文本块）
                    if let Some(t) = delta.get("reasoning_content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                        if !thinking_block_open {
                            // 文本块已开则先关闭（上游先文本后思考的异常次序兜底）
                            if text_block_open {
                                text_block_open = false;
                                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                    "event: content_block_stop\ndata: {}\n\n",
                                    json!({"type":"content_block_stop","index":block_index})
                                ))));
                            }
                            block_index += 1;
                            thinking_block_open = true;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_start\ndata: {}\n\n",
                                json!({"type":"content_block_start","index":block_index,
                                       "content_block":{"type":"thinking","thinking":""}})
                            ))));
                        }
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            json!({"type":"content_block_delta","index":block_index,
                                   "delta":{"type":"thinking_delta","thinking":t}})
                        ))));
                        sent_any = true;
                    }
                    // 文本增量
                    if let Some(t) = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                        // thinking 块先收口，再开文本块
                        if thinking_block_open {
                            thinking_block_open = false;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_stop\ndata: {}\n\n",
                                json!({"type":"content_block_stop","index":block_index})
                            ))));
                        }
                        if !text_block_open {
                            block_index += 1;
                            text_block_open = true;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_start\ndata: {}\n\n",
                                json!({"type":"content_block_start","index":block_index,
                                       "content_block":{"type":"text","text":""}})
                            ))));
                        }
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            json!({"type":"content_block_delta","index":block_index,
                                   "delta":{"type":"text_delta","text":t}})
                        ))));
                        sent_any = true;
                    }
                    // 工具调用增量：按 index 缓冲，finish 时统一输出
                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(tool_buf.len() as i64);
                            let e = tool_buf.entry(idx).or_default();
                            if let Some(i) = tc.get("id").and_then(|i| i.as_str()) {
                                if !i.is_empty() {
                                    e.0 = i.to_string();
                                }
                            }
                            if let Some(n) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                                if !n.is_empty() {
                                    e.1 = n.to_string();
                                }
                            }
                            if let Some(a) = tc.get("function").and_then(|f| f.get("arguments")) {
                                match a {
                                    Value::String(s) => e.2.push_str(s),
                                    v if !v.is_null() => {
                                        if let Ok(s) = serde_json::to_string(v) {
                                            e.2.push_str(&s);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        sent_any = true;
                    }
                } else {
                    match proto {
                        crate::api_server::routes::Protocol::OpenAi => {
                            // OpenAI chunk 归一化透传（rewrite id/model）
                            let mut choice = json!({"index":0,"delta":delta});
                            if !finish.is_empty() {
                                choice["finish_reason"] = json!(finish);
                            }
                            let mut chunk = json!({
                                "id": if id.is_empty() { chat_id.to_string() } else { id },
                                "object": "chat.completion.chunk",
                                "created": now_ts(),
                                "model": model,
                                "choices": [choice],
                            });
                            if let Some(x) = &usage {
                                chunk["usage"] = x.clone();
                            }
                            send!(format!("data: {}\n\n", chunk));
                            sent_any = true;
                        }
                        crate::api_server::routes::Protocol::OpenAiText => {
                            // delta.content → text 块；tool_calls 无对应字段跳过
                            if let Some(t) = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                                let mut choice = json!({"text":t,"index":0});
                                if !finish.is_empty() {
                                    choice["finish_reason"] = json!(finish);
                                }
                                let mut chunk = json!({
                                    "id": chat_id,
                                    "object": "text_completion",
                                    "created": now_ts(),
                                    "model": model,
                                    "choices": [choice],
                                });
                                if let Some(x) = &usage {
                                    chunk["usage"] = x.clone();
                                }
                                send!(format!("data: {}\n\n", chunk));
                                sent_any = true;
                            }
                        }
                        crate::api_server::routes::Protocol::Anthropic => unreachable!(),
                        crate::api_server::routes::Protocol::Responses => {
                            resp_ensure_created!();
                            // created 帧已下发：后续流内错误必须就地 response.failed，
                            // 不得再走「未发送数据」换号重试路径（否则客户端收到重复流）
                            sent_any = true;
                            // 文本增量 → message 输出项 + output_text.delta
                            if let Some(t) = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                                if !resp_msg_open {
                                    resp_msg_open = true;
                                    resp_output_index += 1;
                                    resp_send(tx, "response.output_item.added", json!({
                                        "type": "response.output_item.added",
                                        "output_index": resp_output_index,
                                        "item": {
                                            "id": format!("{}_msg_0", chat_id),
                                            "type": "message",
                                            "status": "in_progress",
                                            "role": "assistant",
                                            "content": [],
                                        },
                                    }));
                                }
                                resp_text.push_str(t);
                                resp_send(tx, "response.output_text.delta", json!({
                                    "type": "response.output_text.delta",
                                    "item_id": format!("{}_msg_0", chat_id),
                                    "output_index": resp_output_index,
                                    "content_index": 0,
                                    "delta": t,
                                }));
                            }
                            // 工具调用增量：按 index 缓冲，completed 时统一输出完整项
                            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                                for tc in tcs {
                                    let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(tool_buf.len() as i64);
                                    let e = tool_buf.entry(idx).or_default();
                                    if let Some(i) = tc.get("id").and_then(|i| i.as_str()) {
                                        if !i.is_empty() {
                                            e.0 = i.to_string();
                                        }
                                    }
                                    if let Some(n) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                                        if !n.is_empty() {
                                            e.1 = n.to_string();
                                        }
                                    }
                                    if let Some(a) = tc.get("function").and_then(|f| f.get("arguments")) {
                                        match a {
                                            Value::String(s) => e.2.push_str(s),
                                            v if !v.is_null() => {
                                                if let Ok(s) = serde_json::to_string(v) {
                                                    e.2.push_str(&s);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 上游断流：error_info / sent_any 状态交由上层判定（故障转移或已收尾）

    (error_info, sent_any, usage)
}

/// 非流式聚合：WB SSE → 内部 OpenAI completion（tool_calls 按 index 合并）
/// 返回 (completion|None, error|None)
pub fn aggregate<L: Iterator<Item = String>>(
    lines: L,
    chat_id: &str,
) -> (Option<Value>, Option<(i64, String)>) {
    let mut parser = WbSseParser::new(lines);
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = String::new();
    let mut usage: Option<Value> = None;
    // index → (id, name, args)
    let mut tools: BTreeMap<i64, (String, String, String)> = BTreeMap::new();

    loop {
        match parser.next_event() {
            None => break,
            Some(WbEvent::Done) => break,
            Some(WbEvent::Error { code, msg }) => return (None, Some((code, msg))),
            Some(WbEvent::Chunk { delta, finish: f, usage: u, .. }) => {
                if let Some(x) = u {
                    usage = Some(x);
                }
                if !f.is_empty() {
                    finish = f;
                }
                if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                    content.push_str(t);
                }
                if let Some(t) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                    reasoning.push_str(t);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let idx = tc
                            .get("index")
                            .and_then(|i| i.as_i64())
                            .unwrap_or(tools.len() as i64);
                        let e = tools.entry(idx).or_default();
                        if let Some(i) = tc.get("id").and_then(|i| i.as_str()) {
                            if !i.is_empty() {
                                e.0 = i.to_string();
                            }
                        }
                        if let Some(n) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                            if !n.is_empty() {
                                e.1 = n.to_string();
                            }
                        }
                        if let Some(a) = tc.get("function").and_then(|f| f.get("arguments")) {
                            match a {
                                Value::String(s) => e.2.push_str(s),
                                v if !v.is_null() => {
                                    if let Ok(s) = serde_json::to_string(v) {
                                        e.2.push_str(&s);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tools.is_empty() {
        let calls: Vec<Value> = tools
            .values()
            .enumerate()
            .map(|(i, (id, name, args))| {
                json!({
                    "id": if id.is_empty() { format!("call_{}_{}", chat_id, i) } else { id.clone() },
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                })
            })
            .collect();
        message["tool_calls"] = json!(calls);
    }
    let mut resp = json!({
        "id": chat_id,
        "object": "chat.completion",
        "created": now_ts(),
        "model": "",
        "choices": [{ "index": 0, "message": message, "finish_reason": if finish.is_empty() { "stop" } else { &finish } }],
    });
    if let Some(u) = usage {
        resp["usage"] = u;
    }
    (Some(resp), None)
}

/// 内部 OpenAI completion → Anthropic Messages 响应（非流式 /v1/messages）
pub fn completion_to_anthropic(v: &Value, msg_id: &str, model: &str) -> Value {
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content: Vec<Value> = Vec::new();
    // T5.3/F-62：reasoning_content → thinking block（置于 text 之前）
    if let Some(t) = message
        .get("reasoning_content")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
    {
        content.push(json!({"type":"thinking","thinking":t}));
    }
    if let Some(t) = message.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            content.push(json!({"type":"text","text":t}));
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let args: Value = tc
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": tc.get("id").cloned().unwrap_or(json!("toolu_0")),
                "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")),
                "input": args,
            }));
        }
    }
    if content.is_empty() {
        content.push(json!({"type":"text","text":""}));
    }
    let (it, ot) = v.get("usage").map(u64_pair).unwrap_or((0, 0));
    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": it, "output_tokens": ot},
    })
}

/// 内部 OpenAI completion → OpenAI legacy text_completion（非流式 /v1/completions）
pub fn completion_to_text(v: &Value, model: &str) -> Value {
    let text = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let finish = v
        .pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop")
        .to_string();
    let mut resp = json!({
        "id": v.get("id").cloned().unwrap_or(json!("cmpl-0")),
        "object": "text_completion",
        "created": now_ts(),
        "model": model,
        "choices": [{ "text": text, "index": 0, "logprobs": null, "finish_reason": finish }],
    });
    if let Some(u) = v.get("usage") {
        resp["usage"] = u.clone();
    }
    resp
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

    fn lines(v: &[&str]) -> std::vec::IntoIter<String> {
        v.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parser_handles_multi_line_data_and_done() {
        let mut p = WbSseParser::new(lines(&[
            ": keep-alive",
            "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"he\"}}]}",
            "",
            "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
            "",
            "data: [DONE]",
            "",
        ]));
        assert!(matches!(p.next_event(), Some(WbEvent::Chunk { .. })));
        match p.next_event() {
            Some(WbEvent::Chunk { delta, finish, usage, .. }) => {
                assert_eq!(delta["content"], json!("llo"));
                assert_eq!(finish, "stop");
                assert!(usage.is_some());
            }
            _ => panic!("expect chunk"),
        }
        assert!(matches!(p.next_event(), Some(WbEvent::Done)));
        assert!(p.next_event().is_none());
    }

    #[test]
    fn parser_detects_error_frame() {
        let mut p = WbSseParser::new(lines(&[
            "data: {\"error\":{\"message\":\"敏感内容\",\"code\":1001}}",
            "",
        ]));
        match p.next_event() {
            Some(WbEvent::Error { code, msg }) => {
                assert_eq!(code, 1001);
                assert_eq!(msg, "敏感内容");
            }
            _ => panic!("expect error"),
        }
    }

    #[test]
    fn aggregate_merges_tool_call_deltas_by_index() {
        let out = aggregate(
            lines(&[
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"get\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}",
                "",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}",
                "",
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}",
                "",
                "data: [DONE]",
                "",
            ]),
            "chat-1",
        );
        let (resp, err) = out;
        assert!(err.is_none());
        let r = resp.unwrap();
        let msg = &r["choices"][0]["message"];
        assert_eq!(msg["content"], json!("done"));
        assert_eq!(msg["tool_calls"][0]["function"]["arguments"], json!("{\"a\":1}"));
        assert_eq!(msg["tool_calls"][0]["function"]["name"], json!("get"));
        assert_eq!(r["usage"]["prompt_tokens"], json!(10));
        assert_eq!(r["choices"][0]["finish_reason"], json!("tool_calls"));
    }

    #[test]
    fn aggregate_reports_error_before_any_content() {
        let (resp, err) = aggregate(
            lines(&["data: {\"error\":{\"message\":\"boom\",\"code\":9}}", ""]),
            "c",
        );
        assert!(resp.is_none());
        assert_eq!(err.unwrap(), (9, "boom".to_string()));
    }

    #[test]
    fn converters_roundtrip() {
        let (resp, _) = aggregate(lines(&["data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}", "data: [DONE]", ""]), "c1");
        let v = resp.unwrap();
        let anth = completion_to_anthropic(&v, "msg_1", "glm-5.3");
        assert_eq!(anth["type"], json!("message"));
        assert_eq!(anth["content"][0]["text"], json!("你好"));
        let txt = completion_to_text(&v, "glm-5.3");
        assert_eq!(txt["choices"][0]["text"], json!("你好"));
        assert_eq!(txt["object"], json!("text_completion"));
    }

    /// Responses 流式投影：created → item.added → text.delta → item.done → completed
    #[test]
    fn stream_forward_emits_responses_event_sequence() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (err, sent_any, usage) =
            stream_forward(lines, &tx, crate::api_server::routes::Protocol::Responses, "resp_1", "glm-5.3");
        assert!(err.is_none());
        assert!(sent_any);
        assert!(usage.is_some());
        drop(tx);

        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.output_item.added"));
        assert!(body.contains("event: response.output_text.delta"));
        assert!(body.contains("\"delta\":\"he\""));
        assert!(body.contains("event: response.output_item.done"));
        let completed = body.split("event: response.completed").nth(1).unwrap_or("");
        assert!(completed.contains("\"status\":\"completed\""));
        assert!(completed.contains("\"input_tokens\":3"));
        assert!(completed.contains("\"output_tokens\":2"));
        assert!(!body.contains("[DONE]"), "Responses 流不应出现 OpenAI [DONE] 帧");
    }

    /// 回归（F-40 审查修复）：文本已流出后遇错误帧 → created 已发即 sent_any=true，
    /// 错误就地 response.failed，不返回 error_info（否则上层换号重试会造成重复流）
    #[test]
    fn stream_forward_responses_instream_error_after_content_fails_inplace() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"部分输出\"}}]}",
            "",
            "data: {\"error\":{\"message\":\"中途失败\",\"code\":1001}}",
            "",
        ]);
        let (err, sent_any, _usage) =
            stream_forward(lines, &tx, crate::api_server::routes::Protocol::Responses, "resp_9", "m");
        assert!(err.is_none(), "流内错误已就地下发，不得上抛触发换号重试");
        assert!(sent_any);
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.failed"));
        assert!(body.contains("中途失败"));
    }
}
