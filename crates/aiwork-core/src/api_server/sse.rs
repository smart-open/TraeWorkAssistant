use std::io::{BufRead, BufReader, Read};

use serde_json::{json, Map, Value};

use super::wb_sse::{cache_write_tokens_of, cached_tokens_of};
use super::wb_upstream::{InterruptibleLines, LINE_POLL};
use crate::api_server::routes::stream_msg_or_fallback;

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
            // create_agent_task 曾使用 event:turn_completion，保留兼容；
            // finish_reason 优先读上游字段（缺失时回退 stop），不再硬编码——
            // 硬编码会让工具回合收尾帧丢失 tool_calls 语义（issue #148，
            // 升级逻辑见 stream_convert_src 收尾分支）
            // 审查加固：值过 OpenAI 词表白名单，词表外/空值归 stop——旧实现
            // 硬编码 stop 兼具归一化效果，裸透传会让非词表值（如 "completed"）
            // 污染 OpenAI 出口（收尾帧 / aggregate / aggregate_text）
            const KNOWN_FINISH: [&str; 5] =
                ["stop", "length", "tool_calls", "function_call", "content_filter"];
            let fr = obj
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            ev.finish_reason = if KNOWN_FINISH.contains(&fr) {
                fr.to_string()
            } else {
                "stop".to_string()
            };
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

/// 工具名逆向回改映射（issue #150）：请求侧 apply_tool_alias_forward 产出的
/// 别名→原名逆映射，按请求作用域（与账号/轮换无关）；None = 无工具别名
type ToolAliasReverse = std::collections::HashMap<String, String>;

/// 工具名逆向回改（issue #150）：响应侧把上游所见别名换回客户端原名——
/// 客户端按名匹配响应 tool_calls/tool_use，单向改名即废。None / 未命中 = 原样
fn rename_tool_back(map: Option<&ToolAliasReverse>, name: &mut String) {
    if let Some(orig) = map.and_then(|m| m.get(name.as_str())) {
        *name = orig.clone();
    }
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
    tool_alias: Option<&ToolAliasReverse>,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    stream_convert_src(lines, sender, chat_id, tool_alias)
}

fn stream_convert_src(
    mut src: LineSrc,
    sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    chat_id: &str,
    tool_alias: Option<&ToolAliasReverse>,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    let mut st = SseState::new();
    let mut pending_usage: Option<Value> = None;
    let mut saw_done = false;
    let mut sent_any = false;
    // 真实内容标记（正文/思考链/工具调用任一非空）；占位 role chunk 不计入。
    // 空完成（影子风控）检测依据（issue #57）
    let mut has_content = false;
    // issue #148：本流是否出现过 tool_calls。Trae 上游对带工具调用的完成帧
    // 误标 finish_reason=stop，逐字透传会被严格客户端（Vercel AI SDK 等）
    // 取消全部 pending 工具并结束回合——收尾帧需按此状态升级
    let mut saw_tool_calls = false;
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
                                        // issue #150：别名回改——上游所见的别名换回客户端原名
                                        if let Some(m) = tool_alias {
                                            if let Some(n) =
                                                clean.get("name").and_then(Value::as_str)
                                            {
                                                if let Some(orig) = m.get(n) {
                                                    clean.insert(
                                                        "name".into(),
                                                        Value::String(orig.clone()),
                                                    );
                                                }
                                            }
                                        }
                                        if let Some(obj) = c.as_object_mut() {
                                            obj.insert("function".into(), Value::Object(clean));
                                        }
                                    }
                                    c
                                })
                                .collect();
                            if !converted.is_empty() {
                                saw_tool_calls = true;
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
                    pending_usage = Some(normalize_openai_usage(
                        &ev.usage.clone().unwrap_or(json!({})),
                    ));
                }
                "done" | "turn_completion" => {
                    // 审查 Low 修复：上游偶发双完成事件（done + turn_completion），
                    // 首个完成帧已发则忽略后续，防同一 SSE 流拼接第二份收尾帧
                    if saw_done {
                        continue;
                    }
                    if !has_content {
                        // 空完成（影子风控/上游异常）：不发收尾帧，以哨兵错误上抛
                        // 调用方换号重试（issue #57，不再把空响应伪装成正常完成）
                        error_info = Some((
                            crate::api_server::EMPTY_COMPLETION_CODE,
                            super::EMPTY_COMPLETION_MSG.to_string(),
                        ));
                        break;
                    }
                    // issue #148：OpenAI 语义要求本流出现过 tool_calls 时收尾帧
                    // finish_reason 必须为 "tool_calls"（无论上游给 stop、空串还是
                    // turn_completion 缺省 stop），否则客户端取消全部工具调用；
                    // 仅升级 stop/空串，保留 length/content_filter 等真实截断语义
                    let finish = if saw_tool_calls
                        && (ev.finish_reason.is_empty() || ev.finish_reason == "stop")
                    {
                        "tool_calls"
                    } else {
                        ev.finish_reason.as_str()
                    };
                    let data = write_chunk(json!({}), finish, &pending_usage);
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
                        // 空 message 兜底（issue #71）：inline 与延迟透传同防线
                        let msg = stream_msg_or_fallback(&ev.error_message, ev.error_code.unwrap_or(0));
                        let error_chunk = json!({
                            "error": {
                                "message": msg,
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

    // 传输层掐断捕获（issue #158 报错A，P2-1）：读错误此前被折叠为 EOF——
    // 有内容时补帧静默伪装成功、零内容时误计空完成。按文本特征识别为传输
    // 掐断哨兵上抛（其余读错误维持折叠语义不变）
    let read_cut = src
        .take_read_error()
        .filter(|e| super::is_transport_cut_text(&e.to_string()));

    if !saw_done && error_info.is_none() {
        if let Some(e) = read_cut {
            if !has_content {
                // 零内容被掐：传输掐断哨兵（不用 EMPTY——网络掐断不应计入
                // 空完成指纹口径）；不发 [DONE]，交调用方换号重试
                error_info = Some((
                    super::TRANSPORT_CUT_CODE,
                    format!("{}{}", super::TRANSPORT_CUT_MSG_PREFIX, e),
                ));
            } else {
                // 有内容被掐：照旧补发收尾帧+[DONE]（客户端可见行为不变，
                // issue #148 口径维持），但不再静默伪装成功——上抛哨兵供
                // route 层结构化日志观测（长流被掐从「不可见」变「可见」）
                let finish = if saw_tool_calls { "tool_calls" } else { "stop" };
                let data = write_chunk(json!({}), finish, &pending_usage);
                let _ = sender.blocking_send(Ok(bytes::Bytes::from(data)));
                let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                error_info = Some((
                    super::TRANSPORT_CUT_CODE,
                    format!("{}{}", super::TRANSPORT_CUT_MSG_PREFIX, e),
                ));
            }
        } else if !has_content {
            // EOF 且零内容：同样视为空完成（换号重试，不发 [DONE]）；判定与
            // 客户端在线解耦——断连也上抛哨兵，否则调用方误判成功绑粘性
            error_info = Some((
                crate::api_server::EMPTY_COMPLETION_CODE,
                super::EMPTY_COMPLETION_MSG.to_string(),
            ));
        } else {
            // issue #148：EOF 兜底补发收尾帧（含 finish_reason）再发 [DONE]——
            // 此前只发 [DONE]，客户端按缺省 stop 处理，带工具调用的半截流
            // 同样会被取消工具
            let finish = if saw_tool_calls { "tool_calls" } else { "stop" };
            let data = write_chunk(json!({}), finish, &pending_usage);
            let _ = sender.blocking_send(Ok(bytes::Bytes::from(data)));
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
                    pending_usage = Some(normalize_openai_usage(
                        &ev.usage.clone().unwrap_or(json!({})),
                    ));
                }
                "done" | "turn_completion" => {
                    // 审查 Low 修复：上游偶发双完成事件（done + turn_completion），
                    // 首个完成帧已发则忽略后续，防同一 SSE 流拼接第二份收尾帧
                    if saw_done {
                        continue;
                    }
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
                        // 空 message 兜底（issue #71）：inline 与延迟透传同防线
                        let msg = stream_msg_or_fallback(&ev.error_message, ev.error_code.unwrap_or(0));
                        let error_chunk = json!({
                            "error": {
                                "message": msg,
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

    // 传输层掐断捕获（issue #158 报错A，P2-1）：同 stream_convert_lines
    let read_cut = src
        .take_read_error()
        .filter(|e| super::is_transport_cut_text(&e.to_string()));

    if !saw_done && error_info.is_none() {
        if let Some(e) = read_cut {
            if !has_content {
                // 零内容被掐：传输掐断哨兵（不用 EMPTY）；不发 [DONE]，交调用方换号
                error_info = Some((
                    super::TRANSPORT_CUT_CODE,
                    format!("{}{}", super::TRANSPORT_CUT_MSG_PREFIX, e),
                ));
            } else {
                // 有内容被掐：照旧补发收尾帧+[DONE]（客户端可见行为不变），
                // 再上抛哨兵供 route 层观测
                let data = write_chunk("", "stop", &pending_usage);
                let _ = sender.blocking_send(Ok(bytes::Bytes::from(data)));
                let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                error_info = Some((
                    super::TRANSPORT_CUT_CODE,
                    format!("{}{}", super::TRANSPORT_CUT_MSG_PREFIX, e),
                ));
            }
        } else if !has_content {
            // EOF 且零内容：同样视为空完成（换号重试，不发 [DONE]）；判定与
            // 客户端在线解耦——断连也上抛哨兵，否则调用方误判成功绑粘性
            error_info = Some((
                crate::api_server::EMPTY_COMPLETION_CODE,
                super::EMPTY_COMPLETION_MSG.to_string(),
            ));
        } else {
            // issue #148：EOF 兜底补发收尾帧（legacy text 无工具语义，固定 stop）
            let data = write_chunk("", "stop", &pending_usage);
            let _ = sender.blocking_send(Ok(bytes::Bytes::from(data)));
            let _ = sender.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
    }

    (error_info, sent_any, pending_usage)
}

/// 纯文本收集器：output/thought 文本 + tool_calls 跨事件合并 + usage + error
struct PlainCollect {
    content: String,
    reasoning: String,
    finish_reason: String,
    usage: Option<Value>,
    error_info: Option<(i64, String)>,
    /// 传输层掐断读错误（issue #158 报错A，P2-1 聚合侧补齐）：此前读错误被
    /// `Err(_) => break` 静默折叠为 EOF——上游 h2 RST/连接重置掐断长响应时，
    /// 半截内容以 finish_reason="stop" 完整返回客户端，tool_calls 残缺也不换号。
    /// 仅留存传输掐断特征读错误（对齐流式侧 is_transport_cut_text 过滤口径），
    /// 其余读错误维持折叠语义不变；saw_done 后的读错误不算（完整响应后关闭）
    read_error: Option<String>,
    /// 审查修复（M6）：按 index 分段合并后的 tool_calls（与流式侧对称）
    tools: Vec<Value>,
}

/// 审查修复（M6）：SOLO tool_call 分片规范化——function_call 旧形态转
/// function、剔除 namespace/partial_arguments（与流式侧转换同口径）
fn normalize_tool_delta(mut c: Value) -> Value {
    if let Some(fc) = c.get("function_call").cloned() {
        if let Some(obj) = c.as_object_mut() {
            obj.insert("function".into(), fc);
            obj.remove("function_call");
        }
    }
    if let Some(fn_obj) = c.get("function").and_then(Value::as_object).cloned() {
        let mut clean = fn_obj;
        clean.remove("namespace");
        clean.remove("partial_arguments");
        if let Some(obj) = c.as_object_mut() {
            obj.insert("function".into(), Value::Object(clean));
        }
    }
    c
}

/// 审查修复（M6）：向既有 tool 槽位合并后续分片——id/type 非空覆盖（首片带全），
/// function.name 非空覆盖，function.arguments 字符串追加
fn merge_tool_slot(t: &mut Value, call: &Value) {
    let Some(obj) = t.as_object_mut() else { return };
    for k in ["id", "type"] {
        if let Some(v) = call.get(k).and_then(Value::as_str) {
            if !v.is_empty() {
                obj.insert(k.to_string(), json!(v));
            }
        }
    }
    let name = call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if name.is_empty() && args.is_empty() {
        return;
    }
    if let Some(fo) = obj.get_mut("function").and_then(Value::as_object_mut) {
        if !name.is_empty() {
            fo.insert("name".into(), json!(name));
        }
        if !args.is_empty() {
            let cur = fo
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            fo.insert("arguments".into(), json!(format!("{cur}{args}")));
        }
    }
}

/// 非流式读错误 → 传输掐断哨兵：响应未收到完成事件前，任何读取失败都意味着
/// 聚合结果可能截断（含残缺 tool_calls JSON），不得按成功交付。
fn read_cut_error(e: &std::io::Error) -> (i64, String) {
    (
        crate::api_server::TRANSPORT_CUT_CODE,
        format!("{} {}", crate::api_server::TRANSPORT_CUT_MSG_PREFIX, e),
    )
}

/// 读取完整 SOLO SSE，收集文本/结束原因/用量/错误
fn collect_plain<R: Read + Send>(reader: R) -> PlainCollect {
    let br = BufReader::new(reader);
    let mut st = SseState::new();
    let mut saw_done = false;
    let mut out = PlainCollect {
        content: String::new(),
        reasoning: String::new(),
        finish_reason: "stop".to_string(),
        usage: None,
        error_info: None,
        read_error: None,
        tools: Vec::new(),
    };
    for line in br.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                // 传输层掐断捕获（聚合侧补齐，口径同流式侧 take_read_error）：
                // 仅 !saw_done 且无业务错误时留存哨兵，交 aggregate/aggregate_text
                // 上抛；其余读错误维持折叠语义（main 保守门控，PR #162 的
                // read_cut_error 助手由 aggregate_anthropic 复用）
                if !saw_done && out.error_info.is_none() {
                    let text = e.to_string();
                    if super::is_transport_cut_text(&text) {
                        out.read_error = Some(text);
                    }
                }
                break;
            }
        };
        if let Some(ev) = scan_line(&mut st, &line.trim_end()) {
            match ev.event.as_str() {
                "output" | "thought" => {
                    out.content.push_str(&ev.response);
                    out.reasoning.push_str(&ev.reasoning);
                    // 审查修复（M6）：非流式聚合补 tool_calls 跨事件合并——此前
                    // 仅收集文本，stream=false 客户端拿到空 content 丢工具调用
                    //（与流式 ToolBuf 不对称）；按 index 分段：首见创建条目、
                    // arguments 字符串追加
                    if let Some(arr) = ev.tool_calls.and_then(|tc| tc.as_array().cloned()) {
                        for call in arr {
                            let call = normalize_tool_delta(call);
                            match call.get("index").and_then(Value::as_u64) {
                                Some(i) => {
                                    if let Some(t) = out
                                        .tools
                                        .iter_mut()
                                        .find(|t| t.get("index").and_then(Value::as_u64) == Some(i))
                                    {
                                        merge_tool_slot(t, &call);
                                    } else {
                                        out.tools.push(call);
                                    }
                                }
                                // 无 index 的分片无法跨事件关联，按独立条目保留（与流式透传对称）
                                None => out.tools.push(call),
                            }
                        }
                    }
                }
                "token_usage" => {
                    out.usage = Some(normalize_openai_usage(&ev.usage.unwrap_or(json!({}))));
                }
                "done" | "turn_completion" => {
                    // 审查加固：双完成事件守卫对齐流式侧（首个完成事件生效，
                    // 后到忽略）——否则 done 与 turn_completion、聚合与流式
                    // 对同一上游形态可能产出不同 finish_reason
                    if saw_done {
                        continue;
                    }
                    if !ev.finish_reason.is_empty() {
                        out.finish_reason = ev.finish_reason;
                    }
                    saw_done = true;
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
    tool_alias: Option<&ToolAliasReverse>,
) -> (Option<Value>, Option<(i64, String)>) {
    let mut c = collect_plain(reader);
    if let Some((code, msg)) = &c.error_info {
        return (None, Some((*code, msg.clone())));
    }
    // 传输层掐断哨兵上抛（聚合侧补齐，P2-1）：半截内容不得以 finish_reason="stop"
    // 伪装成功，交调用方丢弃换号重试（routes 聚合错误分支 is_transport_cut 预检）
    if let Some(e) = c.read_error {
        return (
            None,
            Some((
                super::TRANSPORT_CUT_CODE,
                format!("{} {e}", super::TRANSPORT_CUT_MSG_PREFIX),
            )),
        );
    }
    // issue #150：别名回改——合并后的 tool_calls.function.name 还原客户端原名
    if let Some(m) = tool_alias {
        for t in c.tools.iter_mut() {
            if let Some(orig) = t
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .and_then(|n| m.get(n))
                .cloned()
            {
                if let Some(slot) = t.get_mut("function").and_then(Value::as_object_mut) {
                    slot.insert("name".into(), Value::String(orig));
                }
            }
        }
    }

    let mut message = json!({
        "role": "assistant",
        "content": c.content,
    });
    if !c.reasoning.is_empty() {
        message["reasoning_content"] = json!(c.reasoning);
    }
    // 审查修复（M6）：非流式聚合补发合并后的 tool_calls（与流式侧对称）
    let has_tools = !c.tools.is_empty();
    if has_tools {
        message["tool_calls"] = json!(c.tools);
    }

    // issue #148：message.tool_calls 非空时 finish_reason 必须为 "tool_calls"——
    // Trae 上游误标 stop 逐字透传会让 stream=false 客户端同样取消工具；
    // 仅升级 stop（collect_plain 缺省值），保留 length/content_filter 等真实语义
    let finish_reason = if has_tools && c.finish_reason == "stop" {
        "tool_calls".to_string()
    } else {
        c.finish_reason
    };

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
    // 传输层掐断哨兵上抛（聚合侧补齐，P2-1）：同 aggregate
    if let Some(e) = c.read_error {
        return (
            None,
            Some((
                super::TRANSPORT_CUT_CODE,
                format!("{} {e}", super::TRANSPORT_CUT_MSG_PREFIX),
            )),
        );
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
        // content_filter 是真实截断语义：映射 refusal（对齐 wb_sse 聚合出口），
        // 静默归 end_turn 会把「内容被过滤」伪装成「自然结束」
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

/// token 数值宽口径解析：整数直收；f64 四舍五入（上游偶发 123.0 形态）；字符串
/// 形态（"123"）解析。审查修复：原 as_i64 窄口径下这两种形态静默归零，可出现
/// 「缓存读 > 总输入」的矛盾账。
/// 负值过滤（审查收尾）：token 数恒非负，负值按缺省处理——对齐 wb_sse::token_number
/// 的 u64 口径（负数/负 f64/负字符串三分支同拒），避免求和被异常负值拉低出矛盾账
fn token_number(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return (n >= 0).then_some(n);
    }
    if let Some(f) = v.as_f64() {
        return f.is_finite().then(|| f.round() as i64).filter(|n| *n >= 0);
    }
    v.as_str()?.trim().parse::<i64>().ok().filter(|n| *n >= 0)
}

fn usage_i64(u: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| u.get(*k).and_then(token_number))
}

/// 归一化 OpenAI usage：WB token_usage 原始字段（cache_read_token 等）→
/// OpenAI 标准（prompt_tokens/completion_tokens + prompt_tokens_details.cached_tokens）。
/// 下游（CPA 等）只认标准字段，直接透传原始事件会导致缓存读恒为 0。
pub(crate) fn normalize_openai_usage(u: &Value) -> Value {
    let prompt = usage_i64(u, &["prompt_tokens", "input_tokens"]).unwrap_or(0);
    let completion = usage_i64(u, &["completion_tokens", "output_tokens"]).unwrap_or(0);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        // 饱和求和：异常上游的超大值不 panic / 不回绕
        "total_tokens": prompt.saturating_add(completion),
        "prompt_tokens_details": { "cached_tokens": cached_tokens_of(u) },
    })
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
    tool_alias: Option<&ToolAliasReverse>,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    stream_convert_anthropic_src(lines, sender, msg_id, model, tool_alias)
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
    tool_alias: Option<&ToolAliasReverse>,
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
                            "usage": {
                                "input_tokens": 0,
                                "output_tokens": 0,
                                "cache_read_input_tokens": 0,
                                "cache_creation_input_tokens": 0,
                            },
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
            let mut cached_tokens = 0u64;
            let mut cache_write_tokens = 0u64;
            if let Some(u) = &usage {
                out_tokens = usage_i64(u, &["output_tokens", "completion_tokens"]);
                cached_tokens = cached_tokens_of(u);
                cache_write_tokens = cache_write_tokens_of(u);
            }
            // issue #148：工具回合不得把误标的 stop/空串回落为 end_turn（严格
            // 客户端会取消全部工具调用）。与 wb_sse P1 修复意图一致但规则更
            // 保守——wb_sse 无条件强制 tool_use，此处仅升级 stop/空串，
            // length/content_filter 等真实截断语义保留（→ max_tokens/refusal）
            let stop_src = if !tools.is_empty()
                && (finish_reason.is_empty() || finish_reason == "stop")
            {
                "tool_calls"
            } else {
                finish_reason.as_str()
            };
            let mut message_delta = json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": anthropic_stop_reason(stop_src),
                    "stop_sequence": null,
                },
                "usage": {},
            });
            if let Some(o) = out_tokens {
                message_delta["usage"]["output_tokens"] = json!(o);
            }
            if cached_tokens > 0 {
                message_delta["usage"]["cache_read_input_tokens"] = json!(cached_tokens);
            }
            if cache_write_tokens > 0 {
                message_delta["usage"]["cache_creation_input_tokens"] = json!(cache_write_tokens);
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
                                let mut name = fn_obj
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                // issue #150：别名回改——tool_use.name 还原客户端原名
                                rename_tool_back(tool_alias, &mut name);
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
                    // 上游偶发双完成（sibling 两转换器同款守卫）：二次完成会重发全部
                    // tool_use 块 + message_delta/message_stop，对 Claude Code 等严格
                    // 客户端属协议违规——首帧后静默忽略后续完成事件
                    if saw_done {
                        continue;
                    }
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
                        // 空 message 兜底（issue #71）：inline 与延迟透传同防线
                        let msg = stream_msg_or_fallback(&ev.error_message, ev.error_code.unwrap_or(0));
                        send_event!(
                            "error",
                            json!({
                                "type": "error",
                                "error": {
                                    "type": "api_error",
                                    "message": msg,
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

    // 传输层掐断捕获（issue #158 报错A，P2-1）：同 stream_convert_lines
    let read_cut = src
        .take_read_error()
        .filter(|e| super::is_transport_cut_text(&e.to_string()));

    if !saw_done {
        if message_started || !tools.is_empty() {
            // 上游未发 done 即断流：把已收到的内容按正常收尾发出，避免客户端挂起
            // （仅发帧前检查断连；断连时发送失败无副作用）
            if !disconnected {
                finish_stream!();
            }
            // 传输掐断：就地收尾后上抛哨兵供 route 层观测（不覆盖流内错误事件）
            if error_info.is_none() {
                if let Some(e) = read_cut {
                    error_info = Some((
                        super::TRANSPORT_CUT_CODE,
                        format!("{}{}", super::TRANSPORT_CUT_MSG_PREFIX, e),
                    ));
                }
            }
        } else if error_info.is_none() {
            if let Some(e) = read_cut {
                // 零内容被掐：传输掐断哨兵（不用 EMPTY——网络掐断不应计入
                // 空完成指纹口径）；交调用方换号重试
                error_info = Some((
                    super::TRANSPORT_CUT_CODE,
                    format!("{}{}", super::TRANSPORT_CUT_MSG_PREFIX, e),
                ));
            } else {
                // 空流：以空完成哨兵上抛（原发一个空的合法 message，issue #57 改为
                // 换号重试，避免客户端收到 "empty provider response"）；判定与客户
                // 端在线解耦——断连也上抛，否则调用方误判成功绑粘性
                error_info = Some((
                    crate::api_server::EMPTY_COMPLETION_CODE,
                    super::EMPTY_COMPLETION_MSG.to_string(),
                ));
            }
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
    tool_alias: Option<&ToolAliasReverse>,
) -> (Option<Value>, Option<(i64, String)>) {
    let br = BufReader::new(reader);
    let mut st = SseState::new();
    let mut saw_done = false;
    let mut content = String::new();
    let mut finish_reason = "stop".to_string();
    let mut usage: Option<Value> = None;
    let mut tools: Vec<ToolBuf> = Vec::new();
    let mut error_info: Option<(i64, String)> = None;

    for line in br.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                // 口径同 collect_plain（is_transport_cut_text 门控，PR #162 补齐
                // main 侧缺口）：完成前断流按传输掐断上抛哨兵（read_cut_error），
                // 不把截断的 Anthropic message 当成功交付；其余读错误维持折叠语义
                if !saw_done && error_info.is_none() {
                    let text = e.to_string();
                    if super::is_transport_cut_text(&text) {
                        error_info = Some(read_cut_error(&e));
                    }
                }
                break;
            }
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
                                let mut name = fn_obj
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                // issue #150：别名回改——tool_use.name 还原客户端原名
                                rename_tool_back(tool_alias, &mut name);
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
                    // 审查加固：双完成事件守卫对齐流式侧（首个完成事件生效）
                    if saw_done {
                        continue;
                    }
                    if !ev.finish_reason.is_empty() {
                        finish_reason = ev.finish_reason;
                    }
                    saw_done = true;
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
        // 缓存读：宽口径提取（对齐 wb_sse::cached_tokens_of，正值过滤）
        let cached = cached_tokens_of(u);
        if cached > 0 {
            usage_obj["cache_read_input_tokens"] = json!(cached);
        }
        // 缓存写 tokens（cache_creation_input_tokens）：写侧口径补齐
        let written = cache_write_tokens_of(u);
        if written > 0 {
            usage_obj["cache_creation_input_tokens"] = json!(written);
        }
    }

    let resp = json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        // issue #148：content 含 tool_use 块时，误标/缺省的 stop 不得回落为
        // end_turn——wb_sse 的 Anthropic 聚合出口（completion_to_anthropic）
        // 对此无条件强制 tool_use，此处仅升级 stop/空串并保留
        // length/content_filter 等真实截断语义（更符合协议语义，测试锁定）
        "stop_reason": anthropic_stop_reason(if !tools.is_empty()
            && (finish_reason.is_empty() || finish_reason == "stop")
        {
            "tool_calls"
        } else {
            finish_reason.as_str()
        }),
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
            None,
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
            None,
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
            stream_convert_lines(InterruptibleLines::from_iterator(Box::new(cursor_lines)), tx, "c1", None);
        // 内存切分形态（首字超时包装产物形态）
        let (tx2, rx2) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let lines: InterruptibleLines =
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string)));
        let (e2, s2, _) = stream_convert_lines(lines, tx2, "c1", None);
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
            None,
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
            None,
        );
        let (code, _msg) = error_info.expect("EOF 零内容必须返回哨兵错误");
        assert_eq!(code, crate::api_server::EMPTY_COMPLETION_CODE);
        assert!(!sent_any);
        assert!(!collect_events(rx).join("\n").contains("data: [DONE]"));
    }

    // ==================== 传输层掐断哨兵（issue #158 报错A，P2-1） ====================

    /// 传输掐断文案识别矩阵：RST_STREAM 变体与干扰词正反例
    #[test]
    fn is_transport_cut_text_keyword_matrix() {
        let hits = [
            "http2 stream error RST_STREAM",
            "stream error received: RST Stream",
            "connection error: GOAWAY",
            "Connection reset by peer (os error 104)",
            "broken pipe (os error 32)",
            "connection closed before message completed",
            "h2 error: protocol violation",
        ];
        for s in hits {
            assert!(crate::api_server::is_transport_cut_text(s), "应命中: {s}");
        }
        let misses = [
            "",
            "weird os error 42",
            "upstream 500: internal server error",
            "stream completed normally",
        ];
        for s in misses {
            assert!(!crate::api_server::is_transport_cut_text(s), "不应命中: {s}");
        }
    }

    /// 哨兵双口径判定（对齐 is_empty_completion：码相等 || msg 前缀）
    #[test]
    fn is_transport_cut_matches_code_or_prefix() {
        assert!(crate::api_server::is_transport_cut(
            crate::api_server::TRANSPORT_CUT_CODE,
            "anything"
        ));
        assert!(crate::api_server::is_transport_cut(0, "transport cut: http2 ..."));
        assert!(!crate::api_server::is_transport_cut(
            crate::api_server::EMPTY_COMPLETION_CODE,
            crate::api_server::EMPTY_COMPLETION_MSG
        ));
    }

    /// 产出行后抛读错误的 Reader（模拟上游 h2 RST_STREAM 掐断长流）
    struct CutAfterReader {
        data: Vec<u8>,
        pos: usize,
        err: &'static str,
    }
    impl std::io::Read for CutAfterReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.data.len() {
                let n = (self.data.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::Other, self.err))
            }
        }
    }

    #[test]
    fn transport_cut_with_content_keeps_client_frames_but_raises_sentinel() {
        // 有内容被掐：客户端可见行为不变（内容帧 + 补发收尾帧 + [DONE]），
        // 但后端不再静默伪装成功——上抛传输掐断哨兵
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let src = InterruptibleLines::from_reader(CutAfterReader {
            // Trae SOLO 协议（stream_convert_lines 解析目标格式）
            data: b"event: output\ndata: {\"response\":\"hi\"}\n\n".to_vec(),
            pos: 0,
            err: "http2 stream error RST_STREAM",
        })
        .expect("首行应正常产出");
        let (error_info, sent_any, _) = stream_convert_lines(src, tx, "c1", None);
        let (code, msg) = error_info.expect("掐断应上抛哨兵");
        assert_eq!(code, crate::api_server::TRANSPORT_CUT_CODE);
        assert!(crate::api_server::is_transport_cut(code, &msg));
        assert!(msg.starts_with("transport cut:"), "msg 应带前缀: {msg}");
        assert!(sent_any, "有内容被掐 sent_any 应为 true");
        // 不得误标空完成（网络掐断不进空完成指纹口径）
        assert!(!crate::api_server::is_empty_completion(code, &msg));
        let events = collect_events(rx);
        let joined = events.join("\n");
        assert!(joined.contains("hi"), "内容帧应已发出");
        assert!(joined.contains("\"finish_reason\":\"stop\""), "应补发收尾帧");
        assert!(
            events.last().map(|e| e.contains("[DONE]")).unwrap_or(false),
            "应发 [DONE]，客户端按正常结束处理"
        );
    }

    #[test]
    fn transport_cut_without_content_raises_sentinel_not_empty_completion() {
        // 零内容被掐：传输掐断哨兵（不用 EMPTY），不发 [DONE]，交调用方换号
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let src = InterruptibleLines::from_reader(CutAfterReader {
            data: b": keepalive\n\n".to_vec(),
            pos: 0,
            err: "connection reset by peer",
        })
        .expect("首行应正常产出");
        let (error_info, sent_any, _) = stream_convert_lines(src, tx, "c1", None);
        let (code, msg) = error_info.expect("掐断应上抛哨兵");
        assert_eq!(code, crate::api_server::TRANSPORT_CUT_CODE);
        assert!(crate::api_server::is_transport_cut(code, &msg));
        assert!(!sent_any, "零内容被掐未流出 sent_any 应为 false");
        assert!(
            !crate::api_server::is_empty_completion(code, &msg),
            "网络掐断不得计入空完成指纹口径"
        );
        assert!(!collect_events(rx).join("\n").contains("data: [DONE]"));
    }

    #[test]
    fn non_transport_read_error_keeps_legacy_eof_semantics() {
        // 对照组：非传输特征读错误维持旧行为——零内容折叠为 EOF → 空完成哨兵
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let src = InterruptibleLines::from_reader(CutAfterReader {
            data: b": keepalive\n\n".to_vec(),
            pos: 0,
            err: "weird os error 42",
        })
        .expect("首行应正常产出");
        let (error_info, sent_any, _) = stream_convert_lines(src, tx, "c1", None);
        let (code, _msg) = error_info.expect("EOF 零内容仍应命中空完成哨兵");
        assert_eq!(code, crate::api_server::EMPTY_COMPLETION_CODE);
        assert!(!sent_any);
        assert!(!collect_events(rx).join("\n").contains("data: [DONE]"));
    }

    #[test]
    fn anthropic_transport_cut_with_content_finishes_and_raises_sentinel() {
        // Anthropic 版有内容被掐：就地收尾（message_stop）+ 上抛哨兵
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        // 输入为 Trae SOLO 上游协议（scan_line 解析 event: output / response 字段）
        let src = InterruptibleLines::from_reader(CutAfterReader {
            data: b"event: output\ndata: {\"response\":\"hi\"}\n\n".to_vec(),
            pos: 0,
            err: "http2 stream error RST_STREAM",
        })
        .expect("首行应正常产出");
        let (error_info, sent_any, _) =
            stream_convert_anthropic_lines(src, tx, "msg_t", "m", None);
        let (code, msg) = error_info.expect("掐断应上抛哨兵");
        assert_eq!(code, crate::api_server::TRANSPORT_CUT_CODE);
        assert!(sent_any, "有内容被掐 sent_any 应为 true");
        assert!(!crate::api_server::is_empty_completion(code, &msg));
        assert!(
            collect_events(rx).join("\n").contains("event: message_stop"),
            "有内容被掐应就地收尾"
        );
    }

    #[test]
    fn anthropic_transport_cut_without_content_raises_sentinel() {
        // Anthropic 版零内容被掐：传输掐断哨兵（不用 EMPTY）
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let src = InterruptibleLines::from_reader(CutAfterReader {
            data: b": keepalive\n\n".to_vec(),
            pos: 0,
            err: "connection reset by peer",
        })
        .expect("首行应正常产出");
        let (error_info, sent_any, _) =
            stream_convert_anthropic_lines(src, tx, "msg_t", "m", None);
        let (code, _msg) = error_info.expect("掐断应上抛哨兵");
        assert_eq!(code, crate::api_server::TRANSPORT_CUT_CODE);
        assert!(!sent_any, "零内容被掐 sent_any 应为 false");
        assert!(
            !crate::api_server::is_empty_completion(code, _msg.as_str()),
            "网络掐断不得计入空完成指纹口径"
        );
        assert!(!collect_events(rx).join("\n").contains("message_stop"));
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
            None,
        );
        assert!(error_info.is_none(), "有内容不得触发哨兵");
        assert!(sent_any);
        let joined = collect_events(rx).join("\n");
        assert!(joined.contains("hi"), "output 文本应已转发");
        assert!(joined.ends_with("data: [DONE]"));
    }

    // ==================== 非流式聚合断流（issue #158 同族缺口）====================

    #[test]
    fn nonstream_partial_transport_cut_discards_response() {
        let reader = CutAfterReader {
            data: b"event: output\ndata: {\"response\":\"partial\"}\n\n".to_vec(),
            pos: 0,
            err: "http2 stream error RST_STREAM",
        };
        let (resp, err) = aggregate(reader, "c1", None);
        assert!(resp.is_none(), "截断的部分响应必须丢弃，不得当成功交付");
        let (code, msg) = err.expect("断流必须转错误通道");
        assert_eq!(code, crate::api_server::TRANSPORT_CUT_CODE);
        assert!(crate::api_server::is_transport_cut(code, &msg));
        assert!(
            !crate::api_server::is_empty_completion(code, &msg),
            "传输掐断不得计入空完成指纹口径"
        );
    }

    #[test]
    fn nonstream_zero_content_transport_cut_is_not_empty_completion() {
        let reader = CutAfterReader {
            data: b": keepalive\n\n".to_vec(),
            pos: 0,
            err: "connection reset by peer",
        };
        let (resp, err) = aggregate(reader, "c1", None);
        assert!(resp.is_none());
        let (code, msg) = err.expect("零内容断流也必须上抛哨兵");
        assert!(crate::api_server::is_transport_cut(code, &msg));
        assert!(
            !crate::api_server::is_empty_completion(code, &msg),
            "网络掐断不得误判为影子风控空完成"
        );
    }

    #[test]
    fn nonstream_done_then_read_error_keeps_complete_response() {
        let reader = CutAfterReader {
            data: b"event: output\ndata: {\"response\":\"complete\"}\n\nevent: done\ndata: {\"finish_reason\":\"stop\"}\n\n".to_vec(),
            pos: 0,
            err: "http2 stream error RST_STREAM",
        };
        let (resp, err) = aggregate(reader, "c1", None);
        assert!(err.is_none(), "已收到 done 后的连接收尾错误不得推翻完整响应");
        let resp = resp.expect("完整响应应正常返回");
        assert_eq!(resp["choices"][0]["message"]["content"], "complete");
    }

    #[test]
    fn nonstream_anthropic_partial_transport_cut_discards_message() {
        let reader = CutAfterReader {
            data: b"event: output\ndata: {\"response\":\"partial\"}\n\n".to_vec(),
            pos: 0,
            err: "http2 stream error RST_STREAM",
        };
        let (resp, err) = aggregate_anthropic(reader, "msg_1", "m", None);
        assert!(resp.is_none(), "截断的 Anthropic message 必须丢弃");
        let (code, msg) = err.expect("断流必须转错误通道");
        assert!(crate::api_server::is_transport_cut(code, &msg));
        assert!(!crate::api_server::is_empty_completion(code, &msg));
    }

    // ==================== 非流式聚合 tool_calls 合并（审查修复 M6）====================

    #[test]
    fn aggregate_merges_tool_call_fragments_across_events() {
        // 上游 tool_calls 分片下发（首片 id+name+半截 arguments，次片补余下）：
        // 非流式聚合必须按 index 合并为单条目，否则 stream=false 客户端丢工具调用
        let input = concat!(
            "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\"}}]}\n\n",
            "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"北京\\\"}\"}}]}\n\n",
            "event: done\ndata: {\"finish_reason\":\"tool_calls\"}\n\n",
        );
        let (resp, err) = aggregate(std::io::Cursor::new(input), "c1", None);
        assert!(err.is_none());
        let resp = resp.expect("tool_calls 流必须产出聚合结果");
        let msg = &resp["choices"][0]["message"];
        let tools = msg["tool_calls"].as_array().expect("tool_calls 必须下发");
        assert_eq!(tools.len(), 1, "同 index 分片合并为单条目");
        assert_eq!(tools[0]["id"], "call_1");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(
            tools[0]["function"]["arguments"].as_str().unwrap(),
            "{\"city\":\"北京\"}",
            "arguments 跨事件字符串追加"
        );
        assert_eq!(resp["choices"][0]["finish_reason"], "tool_calls");
    }

    // ==================== issue #148：工具回合 finish_reason 升级 ====================

    #[test]
    fn tool_call_stream_upgrades_stop_finish_reason() {
        // Trae 上游：output 带 tool_calls + done 误标 stop → 收尾帧必须为 tool_calls
        let input = concat!(
            "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{}\"}}]}\n\n",
            "event: done\ndata: {\"finish_reason\":\"stop\"}\n\n",
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (error_info, sent_any, _) = stream_convert_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "c1",
            None,
        );
        assert!(error_info.is_none());
        assert!(sent_any);
        let joined = collect_events(rx).join("\n");
        assert!(
            joined.contains("\"finish_reason\":\"tool_calls\""),
            "工具回合收尾帧必须升级为 tool_calls：{joined}"
        );
        assert!(
            !joined.contains("\"finish_reason\":\"stop\""),
            "不得透传上游误标的 stop：{joined}"
        );
        assert!(joined.ends_with("data: [DONE]"));
    }

    #[test]
    fn tool_call_turn_completion_upgrades_default_stop() {
        // turn_completion 兼容路径：缺省（{}）与显式空串（{"finish_reason":""}）
        // 两种形态均按缺省 stop 处理并升级（空串经词表白名单归一化为 stop）
        let tool_frame = "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{}\"}}]}\n\n";
        for done_data in ["{}", "{\"finish_reason\":\"\"}"] {
            let input = format!("{tool_frame}event: turn_completion\ndata: {done_data}\n\n");
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
            let (error_info, _, _) = stream_convert_lines(
                // lines() 借用局部 input，先收集为自有 Vec 以满足 'static
                InterruptibleLines::from_iterator(Box::new(
                    input
                        .lines()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                        .into_iter(),
                )),
                tx,
                "c1",
                None,
            );
            assert!(error_info.is_none());
            let joined = collect_events(rx).join("\n");
            assert!(
                joined.contains("\"finish_reason\":\"tool_calls\""),
                "{done_data}: {joined}"
            );
        }
    }

    #[test]
    fn tool_call_length_finish_reason_is_preserved() {
        // 真实截断语义保留：工具回合 + 上游 length 不得升级为 tool_calls
        let input = concat!(
            "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{}\"}}]}\n\n",
            "event: done\ndata: {\"finish_reason\":\"length\"}\n\n",
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (error_info, _, _) = stream_convert_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "c1",
            None,
        );
        assert!(error_info.is_none());
        let joined = collect_events(rx).join("\n");
        assert!(joined.contains("\"finish_reason\":\"length\""), "{joined}");
    }

    #[test]
    fn tool_call_eof_fallback_sends_finish_frame() {
        // 上游未发 done 即 EOF：兜底需补发含 finish_reason 的收尾帧，不能只发 [DONE]
        let input = "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{}\"}}]}\n\n";
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (error_info, sent_any, _) = stream_convert_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "c1",
            None,
        );
        assert!(error_info.is_none());
        assert!(sent_any);
        let joined = collect_events(rx).join("\n");
        assert!(
            joined.contains("\"finish_reason\":\"tool_calls\""),
            "EOF 兜底必须补发 tool_calls 收尾帧：{joined}"
        );
        assert!(joined.ends_with("data: [DONE]"));
    }

    #[test]
    fn aggregate_upgrades_tool_call_finish_reason() {
        // 非流式聚合：tool_calls 非空 + 上游误标 stop → finish_reason 置 tool_calls
        let input = concat!(
            "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{}\"}}]}\n\n",
            "event: done\ndata: {\"finish_reason\":\"stop\"}\n\n",
        );
        let (resp, err) = aggregate(std::io::Cursor::new(input), "c1", None);
        assert!(err.is_none());
        let resp = resp.expect("聚合结果");
        assert_eq!(resp["choices"][0]["finish_reason"], "tool_calls");
        assert!(resp["choices"][0]["message"]["tool_calls"].is_array());
    }

    #[test]
    fn anthropic_tool_stream_forces_tool_use_stop_reason() {
        // Anthropic 出口：工具块 + 上游误标 stop → message_delta.stop_reason 必须 tool_use
        let input = concat!(
            "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{}\"}}]}\n\n",
            "event: done\ndata: {\"finish_reason\":\"stop\"}\n\n",
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (error_info, sent_any, _) = stream_convert_anthropic_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "msg_t",
            "m",
            None,
        );
        assert!(error_info.is_none());
        assert!(sent_any);
        let joined = collect_events(rx).join("\n");
        assert!(
            joined.contains("\"stop_reason\":\"tool_use\""),
            "工具回合 stop_reason 必须为 tool_use：{joined}"
        );
        assert!(
            !joined.contains("\"stop_reason\":\"end_turn\""),
            "不得透传 end_turn：{joined}"
        );
    }

    #[test]
    fn aggregate_anthropic_tool_use_stop_reason() {
        // Anthropic 非流式聚合：tool_use 块 + 上游误标 stop → stop_reason 置 tool_use
        let input = concat!(
            "event: output\ndata: {\"response\":\"\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run\",\"arguments\":\"{}\"}}]}\n\n",
            "event: done\ndata: {\"finish_reason\":\"stop\"}\n\n",
        );
        let (resp, err) = aggregate_anthropic(std::io::Cursor::new(input), "msg_t", "m", None);
        assert!(err.is_none());
        let resp = resp.expect("聚合结果");
        assert_eq!(resp["stop_reason"], "tool_use");
        assert!(resp["content"].as_array().expect("blocks").iter().any(|b| b["type"] == "tool_use"));
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
    #[test]
    fn normalize_openai_usage_maps_cache_read_token() {
        // WB token_usage 事件（上游字段 cache_read_token）→ OpenAI 标准 usage
        let u = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_token": 80,
            "cache_write_token": 5,
        });
        let n = normalize_openai_usage(&u);
        assert_eq!(n["prompt_tokens"], serde_json::json!(100));
        assert_eq!(n["completion_tokens"], serde_json::json!(20));
        assert_eq!(n["total_tokens"], serde_json::json!(120));
        assert_eq!(n["prompt_tokens_details"]["cached_tokens"], serde_json::json!(80));
    }

    #[test]
    fn normalize_openai_usage_defaults_cached_zero_when_absent() {
        let u = serde_json::json!({"input_tokens": 10, "output_tokens": 3});
        let n = normalize_openai_usage(&u);
        assert_eq!(n["prompt_tokens"], serde_json::json!(10));
        assert_eq!(n["prompt_tokens_details"]["cached_tokens"], serde_json::json!(0));
    }

    #[test]
    fn normalize_openai_usage_total_saturates() {
        // 异常上游超大值：总数十饱和，不 panic / 不回绕
        let u = serde_json::json!({"prompt_tokens": i64::MAX, "completion_tokens": 1});
        let n = normalize_openai_usage(&u);
        assert_eq!(n["total_tokens"], serde_json::json!(i64::MAX));
    }

    #[test]
    fn normalize_openai_usage_accepts_f64_and_string_tokens() {
        // 审查修复回归：f64（123.0）与字符串（"20"）形态 token 不得静默归零
        let n = normalize_openai_usage(&serde_json::json!({
            "prompt_tokens": 100.7, "completion_tokens": "20"
        }));
        assert_eq!(n["prompt_tokens"], serde_json::json!(101));
        assert_eq!(n["completion_tokens"], serde_json::json!(20));
        assert_eq!(n["total_tokens"], serde_json::json!(121));
    }

    #[test]
    fn token_number_rejects_negative_values() {
        // 审查收尾回归：token 数恒非负，负值（数字/浮点/字符串三分支）按缺省处理，
        // 对齐 wb_sse::token_number 的 u64 口径
        assert_eq!(usage_i64(&serde_json::json!({"prompt_tokens": -3}), &["prompt_tokens"]), None);
        assert_eq!(usage_i64(&serde_json::json!({"prompt_tokens": -1.5}), &["prompt_tokens"]), None);
        assert_eq!(usage_i64(&serde_json::json!({"prompt_tokens": "-3"}), &["prompt_tokens"]), None);
        // 正值三分支不受影响
        assert_eq!(usage_i64(&serde_json::json!({"prompt_tokens": 5}), &["prompt_tokens"]), Some(5));
        assert_eq!(usage_i64(&serde_json::json!({"prompt_tokens": "5"}), &["prompt_tokens"]), Some(5));
    }

    #[test]
    fn cached_tokens_of_accepts_openai_style_keys() {
        assert_eq!(
            cached_tokens_of(&serde_json::json!({"cached_tokens": 7})),
            7
        );
        assert_eq!(
            cached_tokens_of(&serde_json::json!({"cache_read_tokens": 9})),
            9
        );
        assert_eq!(cached_tokens_of(&serde_json::json!({})), 0);
    }

    #[test]
    fn aggregate_anthropic_emits_cache_read_and_creation() {
        // SOLO token_usage 事件 → Anthropic usage：缓存读（宽口径正值）+ 缓存写透传
        // （issue #94：Trae 池 Anthropic 出口需与 WB 管线同口径输出读/写两侧）
        let sse = concat!(
            "event: output\n",
            "data: {\"response\": \"hi\"}\n",
            "\n",
            "event: token_usage\n",
            "data: {\"prompt_tokens\": 100, \"output_tokens\": 20, ",
            "\"cache_read_input_tokens\": 0, \"cache_read_token\": 80, ",
            "\"cache_creation_input_tokens\": 5}\n",
            "\n",
            "event: done\n",
            "data: {\"finish_reason\": \"stop\"}\n",
            "\n",
        );
        let (resp, err) = aggregate_anthropic(sse.as_bytes(), "msg_test", "glm-5.3", None);
        assert!(err.is_none());
        let resp = resp.expect("message");
        let usage = &resp["usage"];
        assert_eq!(usage["input_tokens"], serde_json::json!(100));
        assert_eq!(usage["output_tokens"], serde_json::json!(20));
        // 0 值别名（cache_read_input_tokens=0）不遮蔽有效别名（cache_read_token=80）
        assert_eq!(usage["cache_read_input_tokens"], serde_json::json!(80));
        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(5));
    }

    /// Anthropic 流式出口透出缓存读/写 tokens（token_usage 的 usage 即 data 本体）
    #[test]
    fn anthropic_delta_emits_cache_read_and_creation_tokens() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let input = "event: output\ndata: {\"response\":\"hi\"}\n\n\
                     event: token_usage\ndata: {\"input_tokens\":10,\"output_tokens\":3,\"cache_read_input_tokens\":7,\"cache_creation_input_tokens\":2}\n\n\
                     event: done\ndata: {\"finish_reason\":\"stop\"}\n\n";
        let (error_info, _sent_any, _) = stream_convert_anthropic_lines(
            InterruptibleLines::from_iterator(Box::new(input.lines().map(str::to_string))),
            tx,
            "msg_cache",
            "m",
            None,
        );
        assert!(error_info.is_none());
        let joined = collect_events(rx).join("\n");
        assert!(
            joined.contains("\"cache_read_input_tokens\":7"),
            "message_delta 需透出缓存读: {joined}"
        );
        assert!(
            joined.contains("\"cache_creation_input_tokens\":2"),
            "message_delta 需透出缓存写: {joined}"
        );
    }

    // ==================== 工具名别名响应侧回改（issue #150）====================

    const ALIAS_TOOL_SSE: &str = "event: output\ndata: {\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function_call\":{\"name\":\"ask_user\",\"arguments\":\"{\\\"q\\\":\\\"1\\\"}\"}}]}\n\nevent: done\ndata: {\"finish_reason\":\"stop\"}\n\n";

    fn alias_reverse() -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("ask_user".to_string(), "ask_user_question".to_string());
        m
    }

    /// OpenAI 流式：delta.tool_calls.function.name 把上游所见别名回改为客户端
    /// 原名（请求侧改写 + 响应侧回改，协议对客户端透明）
    #[test]
    fn tool_alias_reverse_applied_on_openai_stream() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let lines: InterruptibleLines =
            InterruptibleLines::from_iterator(Box::new(ALIAS_TOOL_SSE.lines().map(str::to_string)));
        let (e, s, _) = stream_convert_lines(lines, tx, "c1", Some(&alias_reverse()));
        assert_eq!(e, None);
        assert!(s);
        let joined = collect_events(rx).join("\n");
        assert!(joined.contains("ask_user_question"), "{joined}");
        assert!(
            !joined.contains("\"ask_user\""),
            "别名不得残留（原名为其前缀，须带引号精确断言）: {joined}"
        );
        // 收尾帧 finish_reason 应为 tool_calls（issue #148 语义）
        assert!(joined.contains("\"finish_reason\":\"tool_calls\""), "{joined}");
    }

    /// OpenAI 非流式聚合：合并后 tool_calls.function.name 回改原名
    #[test]
    fn tool_alias_reverse_applied_on_aggregate() {
        let (v, err) = aggregate(std::io::Cursor::new(ALIAS_TOOL_SSE), "c1", Some(&alias_reverse()));
        assert!(err.is_none());
        let v = v.expect("聚合应产出消息");
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            serde_json::json!("ask_user_question")
        );
    }

    /// Anthropic 流式：tool_use.name 回改原名
    #[test]
    fn tool_alias_reverse_applied_on_anthropic_stream() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let lines: InterruptibleLines =
            InterruptibleLines::from_iterator(Box::new(ALIAS_TOOL_SSE.lines().map(str::to_string)));
        let (e, s, _) = stream_convert_anthropic_lines(lines, tx, "msg_t", "m", Some(&alias_reverse()));
        assert_eq!(e, None);
        assert!(s);
        let joined = collect_events(rx).join("\n");
        assert!(joined.contains("\"type\":\"tool_use\""), "{joined}");
        assert!(joined.contains("ask_user_question"), "{joined}");
        assert!(!joined.contains("\"ask_user\""), "别名不得残留: {joined}");
    }
}
