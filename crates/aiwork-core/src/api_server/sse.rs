use std::io::{BufRead, BufReader, Read};

use serde_json::{json, Map, Value};

use super::wb_upstream::{InterruptibleLines, LINE_POLL};

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

/// 行源抽象（P1 修复2）：上游已经 wb_upstream::lines_with_first_byte_timeout
/// 包装（首字节 10s 超时 → 故障转移）产出的可中断行源。读错误等同 EOF
/// 终止转换（包装层已把读错误折叠为流结束）；上游停滞期间转换循环可通过
/// next_timeout 周期性醒来检查客户端断连
type LineSrc = InterruptibleLines;

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
/// 返回 (错误信息, 是否已向客户端发送过数据, 上游 token usage)。
/// 若上游首个事件即 error（尚未发送任何数据），错误不下发，
/// 由调用方决定重试（如 4001 改 function）或透传给客户端。
/// 行源：wb_upstream::lines_with_first_byte_timeout 包装（首字 10s 超时 →
/// 故障转移）产出的可中断行源
pub fn stream_convert_lines(
    lines: InterruptibleLines,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    chat_id: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    stream_convert_src(lines, sender, chat_id)
}

fn stream_convert_src(
    mut src: LineSrc,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    chat_id: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    let mut st = SseState::new();
    let mut pending_usage: Option<Value> = None;
    let mut saw_done = false;
    let mut sent_any = false;
    // 真实内容标记（正文/思考链/工具调用任一非空）；占位 role chunk 不计入。
    // 空完成（影子风控）检测依据（issue #57）
    let mut has_content = false;
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

    loop {
        // 客户端断连：立即停止读上游，释放连接与账号并发槽（对齐 anthropic 版）
        if sender.is_closed() {
            break;
        }
        // 断连 + 上游停滞：带超时取行，停滞期间每 LINE_POLL 醒来检查客户端
        // 是否断连（sender.is_closed()），不再死等上游下一行（最长 300s 读超时）
        let line = match src.next_timeout(LINE_POLL) {
            Ok(Some(line)) => line,
            Ok(None) => break, // 流结束（读错误折叠为 EOF）
            Err(()) => {
                if sender.is_closed() {
                    break; // 客户端已断连：退出，释放上游连接与账号并发槽
                }
                continue; // 仅停滞未断连：继续等待
            }
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
                        has_content = true;
                        let data = write_chunk(Value::Object(delta), "", &pending_usage);
                        // 客户端断连检测：发送失败即退出读循环，释放上游与账号并发槽
                        if sender.blocking_send(Ok(bytes::Bytes::from(data))).is_err() {
                            break;
                        }
                        sent_any = true;
                    } else if !sent_any {
                        // 空 delta 的首个 output：上游已开始产出，
                        // 发出仅含 role 的空 chunk 占位，让 sent_any 语义与真实下发一致
                        let data = write_chunk(json!({ "role": "assistant" }), "", &pending_usage);
                        if sender.blocking_send(Ok(bytes::Bytes::from(data))).is_err() {
                            break;
                        }
                        sent_any = true;
                    }
                }
                "token_usage" => {
                    pending_usage = Some(json!(ev.usage.clone().unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    if !has_content {
                        // 空完成（影子风控/上游异常）：不发收尾帧，以哨兵错误上抛
                        // 调用方换号重试（issue #57，不再把空响应伪装成正常完成）
                        error_info = Some((
                            crate::api_server::EMPTY_COMPLETION_CODE,
                            super::EMPTY_COMPLETION_MSG.to_string(),
                        ));
                        break;
                    }
                    let data = write_chunk(json!({}), &ev.finish_reason, &pending_usage);
                    if sender.blocking_send(Ok(bytes::Bytes::from(data))).is_err() {
                        break;
                    }
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
                        if sender.blocking_send(Ok(bytes::Bytes::from(format!(
                            "data: {}\n\n",
                            error_chunk
                        )))).is_err() {
                            break;
                        }
                        let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                        saw_done = true;
                    }
                }
                _ => {}
            }
        }
    }

    if !saw_done && error_info.is_none() {
        if !has_content && !sender.is_closed() {
            // EOF 且零内容：同样视为空完成（换号重试，不发 [DONE]）
            error_info = Some((
                crate::api_server::EMPTY_COMPLETION_CODE,
                super::EMPTY_COMPLETION_MSG.to_string(),
            ));
        } else {
            let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
    }

    (
        error_info.map(|(code, msg)| (code, msg)),
        sent_any,
        pending_usage,
    )
}

/// 流式转换：SOLO SSE → OpenAI legacy text completion SSE（/v1/completions，T9）
/// delta.content → choices[].text 块；reasoning_content 无对应字段，跳过
pub fn stream_convert_text_lines(
    lines: InterruptibleLines,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    completion_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    stream_convert_text_src(lines, sender, completion_id, model)
}

fn stream_convert_text_src(
    mut src: LineSrc,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    completion_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    let mut st = SseState::new();
    let mut pending_usage: Option<Value> = None;
    let mut saw_done = false;
    let mut sent_any = false;
    let mut has_content = false; // 空完成检测（issue #57）
    let mut error_info: Option<(i64, String)> = None;

    let write_chunk = |text: &str, finish: &str, usage: &Option<Value>| -> String {
        let mut choice = json!({ "text": text, "index": 0 });
        if !finish.is_empty() {
            choice["finish_reason"] = json!(finish);
        }
        let mut chunk = json!({
            "id": completion_id,
            "object": "text_completion",
            "created": now_ts(),
            "model": model,
            "choices": [choice],
        });
        if let Some(u) = usage {
            chunk["usage"] = u.clone();
        }
        format!("data: {}\n\n", chunk)
    };

    loop {
        // 客户端断连：立即停止读上游，释放连接与账号并发槽（对齐 anthropic 版）
        if sender.is_closed() {
            break;
        }
        // 断连 + 上游停滞：同 stream_convert_src，停滞期间周期性检查断连
        let line = match src.next_timeout(LINE_POLL) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(()) => {
                if sender.is_closed() {
                    break;
                }
                continue;
            }
        };
        if let Some(ev) = scan_line(&mut st, &line.trim_end()) {
            match ev.event.as_str() {
                "output" | "thought" => {
                    if !ev.response.is_empty() {
                        has_content = true;
                        let data = write_chunk(&ev.response, "", &pending_usage);
                        if sender.blocking_send(Ok(bytes::Bytes::from(data))).is_err() {
                            break;
                        }
                        sent_any = true;
                    }
                }
                "token_usage" => {
                    pending_usage = Some(json!(ev.usage.clone().unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    if !has_content {
                        // 空完成（影子风控/上游异常）：不发收尾帧，以哨兵错误上抛
                        // 调用方换号重试（issue #57）
                        error_info = Some((
                            crate::api_server::EMPTY_COMPLETION_CODE,
                            super::EMPTY_COMPLETION_MSG.to_string(),
                        ));
                        break;
                    }
                    let data = write_chunk("", &ev.finish_reason, &pending_usage);
                    if sender.blocking_send(Ok(bytes::Bytes::from(data))).is_err() {
                        break;
                    }
                    let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                    saw_done = true;
                    sent_any = true;
                }
                "error" => {
                    error_info = Some((ev.error_code.unwrap_or(0), ev.error_message.clone()));
                    // 已有文本流出：就地透传错误并结束；否则延迟给调用方决策（可重试）
                    if sent_any {
                        let error_chunk = json!({
                            "error": {
                                "message": ev.error_message,
                                "type": "api_error",
                                "code": ev.error_code.unwrap_or(0),
                            }
                        });
                        if sender.blocking_send(Ok(bytes::Bytes::from(format!(
                            "data: {}\n\n",
                            error_chunk
                        )))).is_err() {
                            break;
                        }
                        let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                        saw_done = true;
                    }
                }
                _ => {}
            }
        }
    }

    if !saw_done && error_info.is_none() {
        if !has_content && !sender.is_closed() {
            // EOF 且零内容：同样视为空完成（换号重试，不发 [DONE]）
            error_info = Some((
                crate::api_server::EMPTY_COMPLETION_CODE,
                super::EMPTY_COMPLETION_MSG.to_string(),
            ));
        } else {
            let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
    }

    (error_info, sent_any, pending_usage)
}

/// 纯文本收集器：output/thought 文本 + usage + error（无 tool_calls 场景）
struct PlainCollect {
    content: String,
    reasoning: String,
    finish_reason: String,
    usage: Option<Value>,
    error_info: Option<(i64, String)>,
}

/// 读取完整 SOLO SSE，收集文本/结束原因/用量/错误
fn collect_plain<R: Read + Send>(reader: R) -> PlainCollect {
    let br = BufReader::new(reader);
    let mut st = SseState::new();
    let mut out = PlainCollect {
        content: String::new(),
        reasoning: String::new(),
        finish_reason: "stop".to_string(),
        usage: None,
        error_info: None,
    };
    for line in br.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Some(ev) = scan_line(&mut st, &line.trim_end()) {
            match ev.event.as_str() {
                "output" | "thought" => {
                    out.content.push_str(&ev.response);
                    out.reasoning.push_str(&ev.reasoning);
                }
                "token_usage" => {
                    out.usage = Some(json!(ev.usage.unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    if !ev.finish_reason.is_empty() {
                        out.finish_reason = ev.finish_reason;
                    }
                }
                "error" => {
                    out.error_info = Some((ev.error_code.unwrap_or(0), ev.error_message));
                }
                _ => {}
            }
        }
    }
    out
}

/// 非流式聚合：读取完整 SOLO SSE，聚合为单个 OpenAI chat.completion
pub fn aggregate<R: Read + Send>(
    reader: R,
    chat_id: &str,
) -> (Option<Value>, Option<(i64, String)>) {
    let c = collect_plain(reader);
    if let Some((code, msg)) = &c.error_info {
        return (None, Some((*code, msg.clone())));
    }

    let mut message = json!({
        "role": "assistant",
        "content": c.content,
    });
    if !c.reasoning.is_empty() {
        message["reasoning_content"] = json!(c.reasoning);
    }

    let mut resp = json!({
        "id": chat_id,
        "object": "chat.completion",
        "created": now_ts(),
        "model": "",
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": c.finish_reason,
        }],
    });
    if let Some(u) = c.usage {
        resp["usage"] = u;
    }

    (Some(resp), None)
}

/// 非流式聚合：SOLO SSE → OpenAI legacy text completion（/v1/completions，T9）
pub fn aggregate_text<R: Read + Send>(
    reader: R,
    completion_id: &str,
    model: &str,
) -> (Option<Value>, Option<(i64, String)>) {
    let c = collect_plain(reader);
    if let Some((code, msg)) = &c.error_info {
        return (None, Some((*code, msg.clone())));
    }

    let mut resp = json!({
        "id": completion_id,
        "object": "text_completion",
        "created": now_ts(),
        "model": model,
        "choices": [{
            "text": c.content,
            "index": 0,
            "logprobs": null,
            "finish_reason": c.finish_reason,
        }],
    });
    if let Some(u) = c.usage {
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
pub fn stream_convert_anthropic_lines(
    lines: InterruptibleLines,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    msg_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    stream_convert_anthropic_src(lines, sender, msg_id, model)
}

/// 事件序列：message_start → content_block_start/delta/stop… → message_delta → message_stop
/// 注：reasoning_content 暂不输出（Anthropic thinking 块需签名，严格客户端会拒绝未签名的 thinking_delta）
// 宏内末次赋值（message_started/text_block_open）在收尾路径后不再读取，属预期行为
#[allow(unused_assignments)]
fn stream_convert_anthropic_src(
    mut src: LineSrc,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    msg_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    let mut st = SseState::new();
    let mut message_started = false;
    let mut text_block_open = false;
    let mut block_index: i64 = -1;
    let mut tools: Vec<ToolBuf> = Vec::new();
    let mut usage: Option<Value> = None;
    let mut finish_reason = String::new();
    let mut saw_done = false;
    let mut error_info: Option<(i64, String)> = None;
    // 客户端断连标志：宏内发送失败置位，主循环检测退出（宏也被循环外收尾调用，不能直接 break）
    let mut disconnected = false;

    macro_rules! send {
        ($s:expr) => {
            if sender.blocking_send(Ok(bytes::Bytes::from($s))).is_err() {
                disconnected = true;
            }
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
            // 审查 P2：纯 tool_calls 流（message_started=false，工具仅缓冲未输出）
            // EOF 收尾时先补 message_start——content_block 必须有 message 前缀，
            // 严格客户端（如 Claude Code）才会接受该序列；幂等（done 分支已先行调用）
            start_message!();
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

    loop {
        // 客户端断连：立即停止读上游，释放连接与账号并发槽
        if disconnected {
            break;
        }
        // 断连 + 上游停滞：带超时取行，停滞期间周期性检查断连（同上两个转换）
        let line = match src.next_timeout(LINE_POLL) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(()) => {
                if sender.is_closed() {
                    break;
                }
                continue;
            }
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
                    if !(message_started || !tools.is_empty()) {
                        // 空完成（影子风控/上游异常）：不发 message_delta/message_stop，
                        // 以哨兵错误上抛调用方换号重试（issue #57）
                        error_info = Some((
                            crate::api_server::EMPTY_COMPLETION_CODE,
                            super::EMPTY_COMPLETION_MSG.to_string(),
                        ));
                        break;
                    }
                    start_message!();
                    finish_stream!();
                    saw_done = true;
                }
                "error" => {
                    error_info = Some((ev.error_code.unwrap_or(0), ev.error_message.clone()));
                    // 已有内容发出：就地透传错误并结束；否则延迟给调用方决策（可重试）
                    if message_started || !tools.is_empty() {
                        // P2 修复8：先收口打开的 content_block，再发 event:error——
                        // 此前直接 error 且置 saw_done 跳过 finish_stream，未闭合的
                        // content_block 会让严格客户端挂起/报错；不补发 message_stop
                        close_text_block!();
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

    if !saw_done && !disconnected {
        if message_started || !tools.is_empty() {
            // 上游未发 done 即断流：把已收到的内容按正常收尾发出，避免客户端挂起
            finish_stream!();
        } else if error_info.is_none() {
            // 空流：以空完成哨兵上抛（原发一个空的合法 message，issue #57 改为
            // 换号重试，避免客户端收到 "empty provider response"）
            error_info = Some((
                crate::api_server::EMPTY_COMPLETION_CODE,
                super::EMPTY_COMPLETION_MSG.to_string(),
            ));
        }
    }

    // sent_any 仅统计真实发出过的事件：
    // tools 只是聚合缓冲（input 尚未发送），不能视为已向客户端输出；
    // 但「工具已缓冲 + 中途错误」时 error 分支已就地透传错误事件，
    // 需计入 sent_any，否则调用方会重复下发 error / 误触发重试重放
    let sent_any = message_started || (!tools.is_empty() && error_info.is_some());
    (
        error_info.map(|(code, msg)| (code, msg)),
        sent_any,
        usage,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 收集转换输出的全部 SSE 事件（空行分段）；tx 在被测函数返回时 drop，
    /// 本测试运行于非 async 上下文，blocking_recv/blocking_send 均合法
    fn collect_events(
        mut rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(chunk) = rx.blocking_recv() {
            let s = String::from_utf8_lossy(&chunk.unwrap()).to_string();
            for part in s.split("\n\n") {
                if !part.is_empty() {
                    out.push(part.to_string());
                }
            }
        }
        out
    }

    /// 去掉 created 时间戳字段（跨秒边界两次调用可能不同），其余内容应一致
    fn without_created(s: &str) -> String {
        const MARKER: &str = "\"created\":";
        let mut res = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(pos) = rest.find(MARKER) {
            res.push_str(&rest[..pos + MARKER.len()]);
            let after = &rest[pos + MARKER.len()..];
            let digit_len = after.chars().take_while(|c| c.is_ascii_digit()).count();
            rest = &after[digit_len..];
        }
        res.push_str(rest);
        res
    }

    // ==================== P2 修复8：Anthropic 流内错误收口 ====================

    #[test]
    fn anthropic_midstream_error_closes_open_block_without_message_stop() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let input = "event: output\ndata: {\"response\":\"hello\"}\n\n\
                     event: error\ndata: {\"code\":500,\"message\":\"boom\"}\n\n";
        let (error_info, sent_any, _) = stream_convert_anthropic_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "msg_t",
            "m",
        );
        assert_eq!(error_info, Some((500, "boom".to_string())));
        assert!(sent_any, "已有内容发出时 sent_any 应为 true");
        let events = collect_events(rx);
        let joined = events.join("\n");
        // P2 修复8：error 前必须先 content_block_stop 收口打开的文本块
        let stop_pos = events
            .iter()
            .position(|e| e.starts_with("event: content_block_stop"))
            .expect("应先发 content_block_stop 收口");
        let err_pos = events
            .iter()
            .position(|e| e.starts_with("event: error"))
            .expect("应发 event:error");
        assert!(stop_pos < err_pos, "content_block_stop 必须先于 event:error");
        assert!(joined.contains("event: message_start"));
        assert!(joined.contains("event: content_block_delta"));
        assert!(!joined.contains("message_stop"), "error 收口不重复补发 message_stop");
    }

    #[test]
    fn anthropic_error_before_message_start_is_deferred() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let input = "event: error\ndata: {\"code\":4001,\"message\":\"model config is empty\"}\n\n";
        let (error_info, sent_any, _) = stream_convert_anthropic_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "msg_t",
            "m",
        );
        assert_eq!(error_info, Some((4001, "model config is empty".to_string())));
        assert!(!sent_any, "流未开始：错误延迟给调用方决策（可重试）");
        assert!(collect_events(rx).is_empty());
    }

    // ==================== 行源等价性：IO 逐行读 vs 内存切分 ====================

    #[test]
    fn lines_entry_io_cursor_and_split_agree() {
        let input = "event: output\ndata: {\"response\":\"hi\"}\n\nevent: done\ndata: {\"finish_reason\":\"stop\"}\n\n";
        // IO 逐行读形态（std::io::Cursor::lines，等价真实 reader 场景）
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let cursor_lines =
            std::io::Cursor::new(input).lines().map(|l| l.expect("读行失败"));
        let (e1, s1, _) =
            stream_convert_lines(InterruptibleLines::from_iterator(Box::new(cursor_lines)), tx, "c1");
        // 内存切分形态（首字超时包装产物形态）
        let (tx2, rx2) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let lines: InterruptibleLines =
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string)));
        let (e2, s2, _) = stream_convert_lines(lines, tx2, "c1");
        assert_eq!(e1, e2);
        assert_eq!(s1, s2);
        let a = collect_events(rx).join("\n");
        let b = collect_events(rx2).join("\n");
        assert_eq!(without_created(&a), without_created(&b));
        assert!(a.contains("\"finish_reason\":\"stop\""));
        assert!(a.ends_with("data: [DONE]"));
    }

    // ==================== 空完成哨兵（issue #57 影子风控）====================

    #[test]
    fn empty_completion_done_without_output_returns_sentinel() {
        // 只有 done 帧零内容：不发收尾帧，以哨兵错误上抛调用方换号重试
        let input = "event: done\ndata: {\"finish_reason\":\"stop\"}\n\n";
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (error_info, sent_any, _) = stream_convert_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "c1",
        );
        let (code, msg) = error_info.expect("空完成必须返回哨兵错误");
        assert_eq!(code, crate::api_server::EMPTY_COMPLETION_CODE);
        assert!(crate::api_server::is_empty_completion(code, &msg));
        assert!(!sent_any, "零内容不得置 sent_any");
        let joined = collect_events(rx).join("\n");
        assert!(
            !joined.contains("data: [DONE]"),
            "空完成不发收尾帧，客户端不得误认为正常结束"
        );
    }

    #[test]
    fn empty_completion_eof_without_output_returns_sentinel() {
        // 上游直接 EOF（无 done 帧零输出）：EOF 兜底同样命中哨兵
        let input = ": keep-alive\n\n";
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (error_info, sent_any, _) = stream_convert_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "c1",
        );
        let (code, msg) = error_info.expect("EOF 零内容必须返回哨兵错误");
        assert_eq!(code, crate::api_server::EMPTY_COMPLETION_CODE);
        assert!(crate::api_server::is_empty_completion(code, &msg));
        assert!(!sent_any);
        assert!(!collect_events(rx).join("\n").contains("data: [DONE]"));
    }

    #[test]
    fn normal_done_with_output_not_flagged_empty() {
        // 对照组：有内容时 done 正常收尾，无哨兵且发 [DONE]
        let input = "event: output\ndata: {\"response\":\"hi\"}\n\nevent: done\ndata: {\"finish_reason\":\"stop\"}\n\n";
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (error_info, sent_any, _) = stream_convert_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "c1",
        );
        assert!(error_info.is_none(), "有内容不得触发哨兵");
        assert!(sent_any);
        let joined = collect_events(rx).join("\n");
        assert!(joined.contains("hi"), "output 文本应已转发");
        assert!(joined.ends_with("data: [DONE]"));
    }

    #[test]
    fn aggregated_response_empty_detection() {
        // OpenAI chat：content/reasoning/tool_calls/text 全空 → 空
        let empty = serde_json::json!({
            "choices": [{"message": {"content": "", "reasoning_content": ""}, "finish_reason": "stop"}]
        });
        assert!(crate::api_server::aggregated_response_is_empty(&empty));
        // 缺字段同样视为空
        assert!(crate::api_server::aggregated_response_is_empty(&serde_json::json!({"choices": [{}]})));
        // 有内容 → 非空
        let with = serde_json::json!({
            "choices": [{"message": {"content": "hi"}}]
        });
        assert!(!crate::api_server::aggregated_response_is_empty(&with));
        // 仅有 tool_calls → 非空
        let tools = serde_json::json!({
            "choices": [{"message": {"content": "", "tool_calls": [{"id": "t1"}]}}]
        });
        assert!(!crate::api_server::aggregated_response_is_empty(&tools));
        // 仅有 legacy function_call → 非空（防御性：聚合器当前归一为 tool_calls，
        // 此处兜底未来透传形态；issue #57 审查修复）
        let legacy_fn = serde_json::json!({
            "choices": [{"message": {"content": "", "function_call": {"name": "f", "arguments": "{}"}}}]
        });
        assert!(!crate::api_server::aggregated_response_is_empty(&legacy_fn));
        // function_call 显式 null → 视为空
        assert!(crate::api_server::aggregated_response_is_empty(
            &serde_json::json!({"choices": [{"message": {"content": "", "function_call": null}}]})
        ));
        // Anthropic message：顶层 content 块数组为空 → 空
        assert!(crate::api_server::aggregated_response_is_empty(&serde_json::json!({"content": []})));
        // Anthropic 有文本块 → 非空
        let blocks = serde_json::json!({"content": [{"type": "text", "text": "hi"}]});
        assert!(!crate::api_server::aggregated_response_is_empty(&blocks));
    }
}
