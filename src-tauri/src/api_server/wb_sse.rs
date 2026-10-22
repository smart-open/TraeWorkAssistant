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

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use serde_json::{json, Value};

use super::sse::normalize_openai_usage;
use super::wb_upstream::InterruptibleLines;
use crate::api_server::routes::stream_msg_or_fallback;

/// 上游停滞期间的断连轮询间隔：每 500ms 醒来检查一次客户端是否断连
/// （tx.is_closed()），对齐 sse.rs LINE_POLL——避免「断连 + 上游停滞」时
/// 僵尸流死等上游下一行（最长 300s 读超时）、持续占用 WB 账号并发槽
const LINE_POLL: Duration = Duration::from_millis(500);

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
            if let Some(ev) = self.feed_line(&line) {
                return Some(ev);
            }
        }
    }

    /// 单行喂入：返回该行触发的事件（跨行 data 拼接 + 紧凑流兼容 + `:` 注释跳过）。
    /// 供 next_event 与 next_event_polling 共用（逻辑与拆分前逐行等价）
    fn feed_line(&mut self, line: &str) -> Option<WbEvent> {
        let line = line.trim_end();
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            let payload = std::mem::take(&mut self.data);
            if let Some(ev) = parse_data(&payload) {
                if matches!(ev, WbEvent::Done) {
                    self.done = true;
                }
                return Some(ev);
            }
            return None;
        }
        if line.starts_with(':') {
            return None; // SSE 注释（keep-alive 等）
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
        None
    }
}

impl WbSseParser<InterruptibleLines> {
    /// 停滞感知取事件（WB 路由流式专用）：行源为可中断行源，上游停滞窗口内
    /// 周期性醒来检查客户端断连（client_gone，通常为 tx.is_closed()）——
    /// 断连即返回 None 终止转发，释放上游连接与账号并发槽；未断连则继续等待
    pub fn next_event_polling(
        &mut self,
        poll: Duration,
        client_gone: &dyn Fn() -> bool,
    ) -> Option<WbEvent> {
        if self.done {
            return None;
        }
        loop {
            let line = match self.lines.next_timeout(poll) {
                Ok(Some(line)) => line,
                Ok(None) => return None, // 流结束（读错误折叠为 EOF）
                Err(()) => {
                    if client_gone() {
                        return None; // 客户端已断连：终止，不再读上游
                    }
                    continue; // 仅停滞未断连：继续等待
                }
            };
            if let Some(ev) = self.feed_line(&line) {
                return Some(ev);
            }
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
    // 错误帧：{"error": {...}}（与 OpenAI 一致；兼容顶层 code/message）。
    // filter null：{"error": null} 非错误帧（部分网关心跳形态），不得产生伪错误
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let msg = err
            .get("message")
            .or_else(|| err.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        // 审查修复（M1）：错误码宽口径提取——code 宽口径：上游可能以字符串形态
        // 下发（"code":"4001"），as_i64 落 0 会误入 ErrKind::Server 兜底（30m 熔断），
        // 丢失 classify_wb_error 的模型级豁免判定依据——字符串可 parse 时取值
        let code = err
            .get("code")
            .and_then(|c| {
                c.as_i64()
                    .or_else(|| c.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
            })
            .unwrap_or(0);
        return Some(WbEvent::Error { code, msg });
    }
    // 顶层错误形态（issue #123）：渠道风控直发 {"code":11128,"msg":"request
    // illegal",...}（无 error 包裹、无 choices）——此前落入 Chunk 分支产出
    // 空 delta → 空完成黑洞，11128 身份彻底丢失。识别为流内错误交由调用方
    // 处置（口径与 error 包裹帧一致）。map_or true："choices": null 同样
    // 视为无 choices（Some(Value::Null) 会绕过 is_none 漏检）
    if v.get("choices").map_or(true, |c| c.is_null()) {
        let code = v
            .get("code")
            .and_then(|c| c.as_i64().or_else(|| c.as_str().and_then(|s| s.parse::<i64>().ok())))
            .unwrap_or(0);
        if code >= 400 {
            let msg = v
                .get("msg")
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("upstream error")
                .to_string();
            return Some(WbEvent::Error { code, msg });
        }
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
            "input_tokens_details": {"cached_tokens": cached_tokens_of(u)},
            "output_tokens": ot,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": it + ot,
        });
    }
    obj
}

/// 流式转发：WB SSE → 客户端协议帧（扩展返回）
///
/// 返回 (流内错误, 是否已发送过数据, 流内失败已就地下发, usage, EOF 截断)：
/// - error_info：尚未向下游发送任何数据时捕获的流内错误（调用方据此换号重试）；
/// - failed_inline：流内失败事件（response.failed / error 帧）已就地透传客户端——
///   调用方不得再按成功收尾（不记成功、不清冷却、不绑定粘性），也不应重试
///   （错误已原样给到客户端，重试会造成重复流）。
/// - eof_truncated：上游 EOF 未收到 Done/Error 且已有内容（审查 #6）：客户端
///   仍在线时已按协议补齐终止帧优雅收尾，拿到的是「补齐后的完整形态」而非
///   上游原貌；客户端已断连（tx.is_closed()）时补帧无接收方、跳过补发，该标记
///   仅表示「流被截断收尾」——调用方记账/日志可据此标注截断，便于排障归因
/// - 行源须为可中断行源：上游停滞期间每 LINE_POLL 检查一次客户端断连
///   （tx.is_closed()），断连即终止转发（上游连接与账号并发槽随 Drop 释放）
// 宏内末次赋值（text_block_open）在收尾路径后不再读取，属预期行为（对齐 sse.rs）
#[allow(unused_assignments)]
pub fn stream_forward_ex(
    lines: InterruptibleLines,
    tx: &Sender,
    proto: crate::api_server::routes::Protocol,
    chat_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, bool, Option<Value>, bool) {
    stream_forward_ex_ns(lines, tx, proto, chat_id, model, &std::collections::HashSet::new())
}

/// 同 stream_forward_ex，带 Responses 请求侧展平编码名集合（审查 #4/F4）：
/// `proto == Protocol::Responses` 时仅对集合命中者还原 {name, namespace} 双字段，
/// 原生含 `__` 的扁平 function 名（如 MCP mcp__server__tool）保持单 name 字段
// 宏内末次赋值（text/thinking_block_open）在收尾路径后不再读取，属预期行为
// （对齐 sse.rs 与 stream_forward_ex——本函数自后者重构而来，allow 须随迁）
#[allow(unused_assignments)]
pub fn stream_forward_ex_ns(
    lines: InterruptibleLines,
    tx: &Sender,
    proto: crate::api_server::routes::Protocol,
    chat_id: &str,
    model: &str,
    ns_encoded: &std::collections::HashSet<String>,
) -> (Option<(i64, String)>, bool, bool, Option<Value>, bool) {
    let mut parser = WbSseParser::new(lines);
    let mut sent_any = false;
    let mut failed_inline = false;
    // 真实内容标记（正文/思考链/工具调用任一非空）：空完成（影子风控）检测依据
    // （issue #57）；占位帧（Responses created / 空 delta chunk）不计入
    let mut has_content = false;
    let mut usage: Option<Value> = None;
    let mut error_info: Option<(i64, String)> = None;
    // 终态帧防重（审查 2026-10-07，对齐 sse.rs saw_done 模式）：正常 Done /
    // 流内 error 已发过收尾帧时，循环后断流收尾不得再发（否则重发 tool_use
    // 块 + message_stop、response.completed 或第二个 [DONE]）
    let mut saw_done = false;

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
    // 上游最后携带的 finish_reason（P1 修复：Anthropic message_delta 按此映射 stop_reason）
    let mut last_finish = String::new();
    // issue #96：OpenAI 透传 role 只保留首个下发帧（Buddy 上游逐帧携带 role，
    // 违反 OpenAI 流式惯例 role 仅出现在首个 delta）
    let mut openai_role_sent = false;

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

    // 正常收尾序列（审查 P1-5）：上游 Done 与「EOF 断流但有内容」共用，
    // 按协议补齐终止帧，避免严格客户端因未闭合的 content_block / message 项挂起
    macro_rules! finish_stream {
        () => {
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
                    // 缓存读 tokens 透传：message_delta 携带 cache_read_input_tokens，
                    // 对齐 sse.rs solo 管线口径（下游缓存读不恒为 0）
                    let cached = usage.as_ref().map(cached_tokens_of).unwrap_or(0);
                    // 缓存写 tokens 透传（cache_creation_input_tokens，Anthropic 标准
                    // usage 字段；宽口径键集对齐用量统计 CACHE_WRITE_KEYS）
                    let written = usage.as_ref().map(cache_write_tokens_of).unwrap_or(0);
                    // P1 修复：stop_reason 按上游 finish_reason 映射，工具块存在时
                    // 必为 tool_use（原硬编码 end_turn 与已输出的 tool_use 块矛盾）
                    let stop_reason = if !tool_buf.is_empty() {
                        "tool_use"
                    } else {
                        finish_to_stop(&last_finish)
                    };
                    let mut delta_usage = json!({"input_tokens": it, "output_tokens": ot});
                    if cached > 0 {
                        delta_usage["cache_read_input_tokens"] = json!(cached);
                    }
                    if written > 0 {
                        delta_usage["cache_creation_input_tokens"] = json!(written);
                    }
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                        "event: message_delta\ndata: {}\n\n",
                        json!({
                            "type":"message_delta",
                            "delta":{"stop_reason":stop_reason,"stop_sequence":null},
                            "usage":delta_usage,
                        })
                    ))));
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                        "event: message_stop\ndata: {}\n\n",
                        json!({"type":"message_stop"})
                    ))));
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
                        let mut item = json!({
                            "id": item_id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": if tid.is_empty() { format!("call_{}_{}", chat_id, i) } else { tid.clone() },
                            "arguments": args,
                        });
                        // 编码名还原为 Codex 只接受的 {name, namespace} 双字段；
                        // 仅对请求侧登记的编码名拆分（审查 #4/F4，issue #93）——
                        // 客户端原生扁平工具名可能天然含 `__`，未登记者保持单 name
                        if ns_encoded.contains(name.as_str()) {
                            match crate::api_server::wb_responses::split_namespaced(name) {
                                Some((ns, inner)) => {
                                    item["name"] = json!(inner);
                                    item["namespace"] = json!(ns);
                                }
                                None => {
                                    item["name"] = json!(name);
                                }
                            }
                        } else {
                            item["name"] = json!(name);
                        }
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
        };
    }

    loop {
        // 客户端断连快速检测（对齐 sse.rs「发送失败即断」）：send! 宏忽略发送
        // 失败，活跃流期间的断连依赖此处逐事件检查，最坏延迟一个事件的处理耗时；
        // 停滞期间的断连由 next_event_polling 的 Err 分支覆盖
        if tx.is_closed() {
            break;
        }
        match parser.next_event_polling(LINE_POLL, &|| tx.is_closed()) {
            None => break,
            Some(WbEvent::Done) => {
                if !has_content {
                    // 空完成（影子风控/上游异常）：不发任何收尾帧，以哨兵错误上抛
                    // 调用方换号重试（issue #57，不再把空响应伪装成正常完成）。
                    // 审查 P2-9：Responses 协议若已发 created（空 delta chunk 触发
                    // resp_created 置位），哨兵上抛前补发 response.failed 收尾——
                    // Codex 等客户端在 created 后等待终态事件，悬空流会一直等待；
                    // error.code=empty_completion 保留哨兵语义（调用方仍按空完成
                    // 换号重试，返回元组的 sent_any/failed_inline 语义不变）。
                    // 发送前检查在线（对齐下方 EOF 分支门控，避免向已关闭通道写帧）
                    if resp_created && !tx.is_closed() {
                        let mut resp = responses_object(chat_id, model, "failed", vec![], None);
                        resp["error"] = json!({
                            "code": "empty_completion",
                            "message": super::EMPTY_COMPLETION_MSG,
                        });
                        resp_send(tx, "response.failed", json!({
                            "type": "response.failed",
                            "response": resp,
                        }));
                    }
                    error_info = Some((
                        super::EMPTY_COMPLETION_CODE,
                        super::EMPTY_COMPLETION_MSG.to_string(),
                    ));
                    break;
                }
                // Done 与 EOF 断流共用收尾序列（P1-5 提取为 finish_stream!）
                finish_stream!();
                saw_done = true;
                sent_any = true;
                break;
            }
            Some(WbEvent::Error { code, msg }) => {
                if sent_any {
                    // 已有数据流出：就地透传错误并收尾（failed_inline 标记给调用方）；
                    // 空 message 兜底（issue #71）：inline 与延迟透传同防线
                    let msg = stream_msg_or_fallback(&msg, code);
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
                    failed_inline = true;
                } else {
                    error_info = Some((code, msg));
                }
                break;
            }
            Some(WbEvent::Chunk { delta, finish, usage: u, id }) => {
                if !finish.is_empty() {
                    last_finish = finish.clone();
                }
                // 本帧是否携带 usage（issue #96：纯噪声帧跳过判定依据，
                // 需在 u 合并进函数级 usage 前捕获）
                let frame_has_usage = u.is_some();
                if let Some(x) = u {
                    usage = Some(x);
                }
                // 真实内容标记（issue #57 空完成检测）：空 delta chunk 不计入。
                // 同帧载荷判定复用为 Anthropic message_start 的延迟发送条件
                //（审查修复 L：纯噪声帧不再提前触发 message_start）
                let frame_has_payload =
                    delta.get("content").and_then(|c| c.as_str()).map_or(false, |s| !s.is_empty())
                    || delta
                        .get("reasoning_content")
                        .and_then(|c| c.as_str())
                        .map_or(false, |s| !s.is_empty())
                    || delta
                        .get("tool_calls")
                        .and_then(|t| t.as_array())
                        .map_or(false, |a| !a.is_empty());
                if frame_has_payload {
                    has_content = true;
                }
                if proto == crate::api_server::routes::Protocol::Anthropic {
                    // 审查修复（issue #96 噪声帧 + 空完成换号重试协议违规）：
                    // message_start 延迟到首个真实载荷帧再发——纯噪声 delta 帧
                    // 不提前开启 message，避免空完成换号重试时同一 SSE 流收到
                    // 重复 message_start / 悬空 message（严格客户端协议违规）
                    if frame_has_payload {
                        anthropic_start(tx, &mut message_started, chat_id, model);
                    }
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
                            // OpenAI chunk 归一化透传（rewrite id/model）。
                            // issue #96：Buddy 上游每个 delta 携带与内容无关的噪声
                            // 字段——空 tool_calls 数组、逐帧重复 role、空串
                            // content/reasoning_content/refusal、恒 null 的
                            // extra_fields/function_call。AI SDK v5 系客户端把
                            // "delta 存在 tool_calls 字段（哪怕空数组）"当作本轮
                            // reasoning 结束信号，空数组被放大为"每个思考碎片单独
                            // 成段"（深度思考模型一次响应被撕成上百张思考卡）。
                            // 透传前按 OpenAI 流式惯例做 delta 归一化。
                            let mut delta = delta;
                            if let Some(obj) = delta.as_object_mut() {
                                // 关键修复：空 tool_calls 数组整体移除
                                if obj
                                    .get("tool_calls")
                                    .and_then(|v| v.as_array())
                                    .map_or(false, |a| a.is_empty())
                                {
                                    obj.remove("tool_calls");
                                }
                                // role 只保留在首个下发帧，其余帧移除
                                if openai_role_sent {
                                    obj.remove("role");
                                } else if obj.contains_key("role") {
                                    openai_role_sent = true;
                                }
                                // 空字符串字段剥离
                                for key in ["content", "reasoning_content", "refusal"] {
                                    if obj.get(key).and_then(|v| v.as_str()) == Some("") {
                                        obj.remove(key);
                                    }
                                }
                                // 恒空噪声字段剥离
                                for key in ["extra_fields", "function_call"] {
                                    if obj.get(key).map_or(false, |v| v.is_null()) {
                                        obj.remove(key);
                                    }
                                }
                            }
                            // 剥离后为空、且本帧无 finish_reason 与 usage 的纯噪声
                            // 帧整帧跳过：has_content 基于非空语义判定，不受影响，
                            // 空完成哨兵检测（issue #57）语义不变
                            let frame_empty =
                                delta.as_object().map_or(true, |o| o.is_empty());
                            if !(frame_empty && finish.is_empty() && !frame_has_usage) {
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
                                    // usage 归一化投影（同 solo 管线 sse.rs 口径）：
                                    // WB 原始字段 → OpenAI 标准
                                    // prompt_tokens_details.cached_tokens，下游只认标准字段
                                    chunk["usage"] = normalize_openai_usage(x);
                                }
                                send!(format!("data: {}\n\n", chunk));
                                sent_any = true;
                            }
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
                                    // usage 归一化投影（同 OpenAi 分支口径）
                                    chunk["usage"] = normalize_openai_usage(x);
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

    // 上游断流（EOF 未收到 Done/Error 事件）优雅收尾（审查 P1-5，对齐
    // sse.rs SOLO finish_stream 语义）：已有内容 → 按协议补齐终止帧，避免
    // 严格客户端因未闭合的 content_block / message 项挂起；零内容不伪装
    // 成功，走下方空完成哨兵换号重试。
    // 审查 #6：截断补齐是「有内容但未正常闭合」的降级收尾，与上游原貌不符，
    // 以第 5 元组上抛，由调用方在记账/日志行标注（截断可观测，便于排障归因）。
    // saw_done 兼作 main 侧「已执行 finish_stream」防重（成功流 [DONE]/
    // message_stop 不再重复下发），语义与 eof_truncated 判定统一
    let eof_truncated = !saw_done && error_info.is_none() && !failed_inline && has_content;
    if eof_truncated && !tx.is_closed() {
        finish_stream!();
        sent_any = true;
    }

    // 空完成兜底（issue #57）：EOF 且零内容零错误 → 哨兵上抛（原样收尾会让
    // 客户端收到 "empty provider response"）。
    // 审查 F7：哨兵判定与客户端在线解耦——客户端已断连（tx.is_closed）且上游
    // 零内容时若跳过哨兵，调用方会以 error_info=None+sent_any=false 误判成功
    // 并绑定粘性（空完成账号被钉住）。断连时仍上抛哨兵，调用方按失败处理；
    // 仅 response.failed 帧发送前检查在线，避免向已关闭通道写帧。
    // 终审补齐（P2-9 同构）：resp_created 已置位（空 delta/usage chunk 触发）时
    // 与 Done 分支对称地补发 response.failed——EOF 断流同属「created 后悬空」
    // 场景（qoder_route 证实 EOF 零完成真实存在，非理论不可达），补发后仍上抛
    // 哨兵交调用方换号重试，sent_any/failed_inline 语义不变
    if error_info.is_none() && !has_content && !failed_inline {
        if resp_created && !tx.is_closed() {
            let mut resp = responses_object(chat_id, model, "failed", vec![], None);
            resp["error"] = json!({
                "code": "empty_completion",
                "message": super::EMPTY_COMPLETION_MSG,
            });
            resp_send(tx, "response.failed", json!({
                "type": "response.failed",
                "response": resp,
            }));
        }
        error_info = Some((
            super::EMPTY_COMPLETION_CODE,
            super::EMPTY_COMPLETION_MSG.to_string(),
        ));
    }

    (error_info, sent_any, failed_inline, usage, eof_truncated)
}

/// 兼容封装：旧三元组返回（既有调用方不感知流内失败标记；
/// 新调用方请用 stream_forward_ex）
pub fn stream_forward(
    lines: InterruptibleLines,
    tx: &Sender,
    proto: crate::api_server::routes::Protocol,
    chat_id: &str,
    model: &str,
    encoded: &HashSet<String>,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    let (error_info, sent_any, _failed_inline, usage, _eof_truncated) =
        stream_forward_ex_ns(lines, tx, proto, chat_id, model, encoded);
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

/// OpenAI finish_reason → Anthropic stop_reason 映射（P1 修复）
fn finish_to_stop(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
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
    // 缓存读 tokens 透传：非流式出口同样携带 cache_read_input_tokens
    let cached = v.get("usage").map(cached_tokens_of).unwrap_or(0);
    // 缓存写 tokens 透传（cache_creation_input_tokens，Anthropic 标准 usage 字段）
    let written = v.get("usage").map(cache_write_tokens_of).unwrap_or(0);
    let mut usage = json!({"input_tokens": it, "output_tokens": ot});
    if cached > 0 {
        usage["cache_read_input_tokens"] = json!(cached);
    }
    if written > 0 {
        usage["cache_creation_input_tokens"] = json!(written);
    }
    // P1 修复：stop_reason 读上游 finish_reason 映射（原硬编码 end_turn 丢失
    // tool_use/max_tokens 语义）；有 tool_calls 时强制 tool_use 兜底
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .is_some_and(|a| !a.is_empty());
    let stop_reason = if has_tool_calls {
        "tool_use"
    } else {
        finish_to_stop(choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or(""))
    };
    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage,
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

/// usage tokens 宽口径提取（记账统一口径，审查 P2）：别名（input/output_tokens）
/// 与字符串数字形态不再静默得 0。供流式/非流式/toolexec/custom_route 各记账点共用
pub(crate) fn u64_pair(u: &Value) -> (u64, u64) {
    let get = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|k| u.get(*k).and_then(token_number))
            .unwrap_or(0)
    };
    (
        get(&["prompt_tokens", "input_tokens"]),
        get(&["completion_tokens", "output_tokens"]),
    )
}

/// 宽口径数值解析：对齐 workbuddy_stats::number()——u64 → i64 → f64（非负有限
/// 截断）→ 字符串 parse；浮点/字符串形态的 token 数不再静默得 0
pub(crate) fn token_number(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| v.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64))
        .or_else(|| v.as_str()?.trim().parse::<u64>().ok())
}

/// 正值提取：首个 > 0 的键生效（0 值别名不遮蔽后续别名，对齐
/// workbuddy_stats::positive_field——陈旧 0 值别名掩盖有效别名的场景已实证）
fn positive_of(u: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| u.get(*k).and_then(token_number).filter(|v| *v > 0))
}

/// 上游缓存读 tokens：宽口径提取（对齐 workbuddy_stats::cached_input_field），
/// 仅认正值。键集覆盖：
/// - cache_read_input_tokens：WB token_usage 主形态（glm 抓包实证）
/// - cache_read_token：用量历史 extra_info 同源口径
/// - cacheReadInputTokens / prompt_cache_hit_tokens：camelCase / 命中别名（用量统计同款）
/// - cached_tokens / cache_read_tokens：OpenAI 风格
/// 兜底嵌套：prompt_tokens_details.cached_tokens、inputTokensDetails[].cached_tokens
pub(crate) fn cached_tokens_of(u: &Value) -> u64 {
    positive_of(
        u,
        &[
            "cache_read_input_tokens",
            "cache_read_token",
            "cacheReadInputTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
            "cache_read_tokens",
        ],
    )
    .or_else(|| {
        u.get("prompt_tokens_details")
            .and_then(|d| positive_of(d, &["cached_tokens"]))
    })
    .or_else(|| {
        u.get("inputTokensDetails")
            .and_then(|d| d.as_array())
            .and_then(|ds| ds.iter().find_map(|d| positive_of(d, &["cached_tokens"])))
    })
    .unwrap_or(0)
}

/// 上游缓存写 tokens（cache_creation 口径）：宽口径提取，仅认正值。键集覆盖：
/// - cache_creation_input_tokens：WB token_usage 主形态
/// - cache_write_input_tokens / cacheWriteInputTokens：camelCase / 备选别名
/// - prompt_cache_write_tokens：用量统计 CACHE_WRITE_KEYS 同款
/// - cache_write_token：用量历史 extra_info 同源单数口径（读侧 cache_read_token
///   的对称收录；usage_history 按此键落账）
pub(crate) fn cache_write_tokens_of(u: &Value) -> u64 {
    positive_of(
        u,
        &[
            "cache_creation_input_tokens",
            "cache_write_input_tokens",
            "cacheWriteInputTokens",
            "prompt_cache_write_tokens",
            "cache_write_token",
        ],
    )
    .unwrap_or(0)
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

    /// 测试行源：包装为可中断行源（stream_forward_ex 现要求 InterruptibleLines；
    /// 其同时实现 Iterator，aggregate 等泛型调用点无需区分）
    fn lines(v: &[&str]) -> InterruptibleLines {
        InterruptibleLines::from_iterator(Box::new(
            v.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter(),
        ))
    }

    /// 停滞 + 断连：上游永不产出、客户端已断连 → next_event_polling 应在一个
    /// 轮询窗口内返回 None（而非死等上游下一行 / 300s 读超时）
    #[test]
    fn next_event_polling_aborts_on_disconnect_during_stall() {
        let (_tx_keep, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        // sender 存活但无数据：桥接线程阻塞在行迭代器上，模拟上游停滞
        let stall = Box::new(rx.into_iter().filter_map(|r| r.ok()));
        let mut parser = WbSseParser::new(InterruptibleLines::from_iterator(stall));
        let client_gone = || true;
        let start = std::time::Instant::now();
        assert!(parser.next_event_polling(LINE_POLL, &client_gone).is_none());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "断连后应在一个轮询窗口内返回，实测 {:?}",
            start.elapsed()
        );
    }

    /// 活跃流 + 断连：上游持续产出且无 [DONE]（不断流），客户端通道已关闭 →
    /// 逐事件快速检测应立即终止（sent_any=false），而非读完整条流才结束
    #[test]
    fn stream_forward_aborts_immediately_when_client_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        drop(rx); // 客户端已断连
        let data: Vec<String> = (0..1000)
            .flat_map(|i| {
                vec![
                    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"t{}\"}}}}]}}", i),
                    String::new(),
                ]
            })
            .collect();
        let src = InterruptibleLines::from_iterator(Box::new(data.into_iter()));
        let start = std::time::Instant::now();
        let (_err, sent_any, _fi, _u, _t) = stream_forward_ex(
            src, &tx, crate::api_server::routes::Protocol::OpenAi, "c", "m",
        );
        assert!(!sent_any, "断连后不得有任何事件下发");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "活跃流断连应立即终止，实测 {:?}",
            start.elapsed()
        );
    }

    /// 审查 2026-10-07：活跃流中途断连且有内容——eof_truncated 应置位（流被
    /// 截断收尾），sent_any 语义不变；该场景 tx 已关、wb_sse 跳过补帧，
    /// 调用方日志文案据 tx.is_closed() 区分「客户端已断开」与「已补齐终止帧」
    ///
    /// 确定性：from_iterator 转发线程向无界 std channel 预取行，本闭包 drop rx
    /// 的时机由生产者线程决定——CI 高负载下可能先于消费者首个 delta 下发执行，
    /// 消费者首轮 tx.is_closed() 即 break，sent_any=false 误报（2026-10-13
    /// macOS aarch64 job 偶发）。故 drop rx 前先 try_recv 观测到消费者已实际
    /// 下发首帧（rx 未 drop，观测不影响消费），「断连前已下发」前提确定性成立；
    /// 10s 兜底 deadline 防异常情况下测试无限挂起。
    #[test]
    fn stream_forward_eof_truncated_when_client_disconnects_midstream() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let mut rx = Some(rx);
        let mut i = 0usize;
        // 行序列：delta + 空行交替；产出行 5 前模拟客户端断开（drop rx），
        // 此前至少 1 个 delta 事件已完整下发（has_content/sent_any 已置位）
        let iter = std::iter::from_fn(move || {
            i += 1;
            if i == 5 {
                if let Some(r) = rx.as_mut() {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while matches!(r.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty))
                    {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
                drop(rx.take());
            }
            if i > 9 {
                return None;
            }
            if i % 2 == 1 {
                Some(format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"t{}\"}}}}]}}",
                    (i + 1) / 2
                ))
            } else {
                Some(String::new())
            }
        });
        let src = InterruptibleLines::from_iterator(Box::new(iter));
        let (error_info, sent_any, failed_inline, _usage, eof_truncated) =
            stream_forward_ex(src, &tx, crate::api_server::routes::Protocol::OpenAi, "c", "m");
        assert!(error_info.is_none(), "断连不应产生流内错误");
        assert!(sent_any, "断连前已下发 delta");
        assert!(!failed_inline, "断连不是流内失败");
        assert!(
            eof_truncated,
            "断连且有内容应标记 eof_truncated（流被截断收尾）"
        );
    }

    /// 审查 P2-9：Responses 协议空完成——created 已发（空 delta chunk 触发）时，
    /// 哨兵上抛前补发 response.failed 收尾帧（客户端不再悬空等待终态事件）；
    /// 哨兵语义不变：error_info 仍上抛空完成错误驱动调用方换号，failed_inline
    /// 不置位（未按「失败已透传终态」处理）
    #[test]
    fn stream_forward_responses_empty_completion_after_created_sends_failed_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let src = lines(&[
            // 空 delta chunk：触发 created 占位帧（不计入 has_content）
            "data: {\"choices\":[{\"delta\":{}}]}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (error_info, sent_any, failed_inline, _usage, _t) = stream_forward_ex(
            src,
            &tx,
            crate::api_server::routes::Protocol::Responses,
            "chat-1",
            "m",
        );
        assert_eq!(
            error_info,
            Some((
                crate::api_server::EMPTY_COMPLETION_CODE,
                crate::api_server::EMPTY_COMPLETION_MSG.to_string()
            ))
        );
        assert!(sent_any, "created 已下发");
        assert!(!failed_inline);
        drop(tx); // 关闭通道后收集全部下发帧
        let mut body = String::new();
        while let Some(Ok(b)) = rx.try_recv().ok() {
            body.push_str(&String::from_utf8_lossy(&b));
        }
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.failed"));
        assert!(body.contains("empty_completion"));
        // 空完成不得伪装成正常完成
        assert!(!body.contains("response.completed"));
    }

    /// issue #96：Buddy 上游每个 delta 携带噪声字段（空 tool_calls 数组、逐帧
    /// role、空串 content/reasoning_content/refusal、null extra_fields/
    /// function_call）。AI SDK v5 系客户端把"delta 存在 tool_calls 字段（哪怕
    /// 空数组）"当作思考 part 封口信号，导致思考被撕成上百个碎片。归一化后：
    /// 空 tool_calls 剥离、role 仅首帧保留、空串/null 字段剥离、纯噪声帧整帧跳过。
    #[test]
    fn stream_forward_openai_strips_per_frame_noise_fields() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        // issue 原始抓包样例帧：verbose 噪声字段逐帧携带
        let verbose = |reasoning: &str, content: &str| {
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\",\"extra_fields\":null,\"function_call\":null,\"reasoning_content\":\"{}\",\"refusal\":\"\",\"role\":\"assistant\",\"tool_calls\":[]}}}}]}}",
                content, reasoning
            )
        };
        let mut data: Vec<String> = Vec::new();
        for (r, c) in [("The", ""), ("", ""), ("继续", ""), ("", "正文")] {
            data.push(verbose(r, c));
            data.push(String::new());
        }
        data.push(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}"
                .to_string(),
        );
        data.push(String::new());
        data.push("data: [DONE]".to_string());
        data.push(String::new());
        let src = InterruptibleLines::from_iterator(Box::new(data.into_iter()));
        let (err, sent_any, _fi, usage, _t) = stream_forward_ex(
            src, &tx, crate::api_server::routes::Protocol::OpenAi, "chat_96", "step-5-preview",
        );
        assert!(err.is_none());
        assert!(sent_any);
        assert!(usage.is_some());
        drop(tx);

        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        // 关键修复：空 tool_calls 数组必须整体剥离
        assert!(!body.contains("\"tool_calls\":[]"), "空 tool_calls 数组必须剥离");
        // role 只保留在首个下发帧
        assert_eq!(body.matches("\"role\"").count(), 1, "role 只应出现在首个下发帧");
        // 空串与恒 null 噪声字段剥离
        assert!(!body.contains("\"refusal\":\"\""), "空串 refusal 必须剥离");
        assert!(!body.contains("extra_fields"), "null extra_fields 必须剥离");
        assert!(!body.contains("\"function_call\""), "null function_call 必须剥离");
        // 思考与正文内容完整保留
        assert!(
            body.contains("The") && body.contains("继续") && body.contains("正文"),
            "思考与正文内容必须完整保留"
        );
        // finish 与 usage 照常下发，[DONE] 正常收尾
        assert!(body.contains("\"finish_reason\":\"stop\""), "finish 帧必须下发");
        assert!(body.contains("\"prompt_tokens\":3"), "usage 必须透传");
        assert!(body.ends_with("data: [DONE]\n\n"), "流必须以 [DONE] 收尾");
        // 纯噪声帧整帧跳过：4 个有效帧（首思考/续思考/正文/finish）+ [DONE]
        assert_eq!(body.matches("data: ").count(), 5, "纯噪声帧应整帧跳过");
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

    /// 边缘加固：error 包裹块 null 值不产生伪错误；"choices": null 仍走顶层
    /// 错误判定（Some(Value::Null) 会绕过 is_none 漏检）
    #[test]
    fn parse_data_null_edge_cases() {
        // {"error": null}：非错误帧（部分网关心跳形态），不得产生伪 Error
        let ev = parse_data(r#"{"error":null}"#);
        assert!(!matches!(ev, Some(WbEvent::Error { .. })), "error:null 不得误报错误");
        // {"choices": null, "code": 11128}：顶层错误帧必须被识别
        match parse_data(r#"{"choices":null,"code":11128,"msg":"request illegal"}"#) {
            Some(WbEvent::Error { code, msg }) => {
                assert_eq!(code, 11128);
                assert_eq!(msg, "request illegal");
            }
            other => panic!("choices:null 顶层错误帧应被识别，实际 {:?}", other),
        }
        // {"choices": null} 无 code：不误报错误（落空 Chunk）
        assert!(!matches!(parse_data(r#"{"choices":null}"#), Some(WbEvent::Error { .. })));
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
        let (err, sent_any, usage) = stream_forward(
            lines,
            &tx,
            crate::api_server::routes::Protocol::Responses,
            "resp_1",
            "glm-5.3",
            &HashSet::new(),
        );
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

    /// issue #93：Responses 流式下编码名 function_call 还原为 {name, namespace}
    /// 双字段；扁平 function 工具保持单 name 且不带 namespace（回归）；
    /// 审查 #4：仅请求侧展平集合命中者拆分，原生 `__` 名不拆
    #[test]
    fn stream_forward_responses_restores_namespaced_function_call() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"multi_agent_v1__spawn_agent\",\"arguments\":\"{\\\"a\\\":1}\"}}]}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"mcp__node_repl__js\",\"arguments\":\"{}\"}}]}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":2,\"id\":\"call_3\",\"function\":{\"name\":\"shell\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":4}}",
            "",
            "data: [DONE]",
            "",
        ]);
        // 请求侧展平集合：multi_agent_v1__spawn_agent 为展平编码名；
        // mcp__node_repl__js、shell 为原生扁平名（未入集合）
        let mut ns = std::collections::HashSet::new();
        ns.insert("multi_agent_v1__spawn_agent".to_string());
        let (err, sent_any, _failed_inline, usage, _t) = stream_forward_ex_ns(
            lines,
            &tx,
            crate::api_server::routes::Protocol::Responses,
            "resp_6",
            "m",
            &ns,
        );
        assert!(err.is_none());
        assert!(sent_any);
        assert!(usage.is_some());
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        // done 项：集合命中的编码名拆为 spawn_agent + namespace=multi_agent_v1
        assert!(body.contains("\"name\":\"spawn_agent\""));
        assert!(body.contains("\"namespace\":\"multi_agent_v1\""));
        // 审查 #4：原生含 `__` 扁平名不拆，保持单 name 字段
        assert!(body.contains("\"name\":\"mcp__node_repl__js\""));
        assert!(!body.contains("\"name\":\"js\""));
        assert!(!body.contains("\"namespace\":\"mcp__node_repl\""));
        // 扁平工具不带 namespace 字段
        assert!(body.contains("\"name\":\"shell\""));
        assert!(!body.contains("\"namespace\":\"\""));
        let shell_done = body
            .split("\"name\":\"shell\"")
            .nth(1)
            .and_then(|s| s.split("response.output_item.done").next())
            .unwrap_or("");
        assert!(!shell_done.contains("namespace"), "扁平工具 item 不得携带 namespace 字段");
    }

    /// 回归（F-40 审查修复）：文本已流出后遇错误帧 → created 已发即 sent_any=true，
    /// 错误就地 response.failed，不返回 error_info（否则上层换号重试会造成重复流）；
    /// failed_inline=true 供调用方跳过成功收尾（不记成功/不清冷却/不绑粘性）
    #[test]
    fn stream_forward_responses_instream_error_after_content_fails_inplace() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"部分输出\"}}]}",
            "",
            "data: {\"error\":{\"message\":\"中途失败\",\"code\":1001}}",
            "",
        ]);
        let (err, sent_any, failed_inline, _usage, _t) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Responses, "resp_9", "m");
        assert!(err.is_none(), "流内错误已就地下发，不得上抛触发换号重试");
        assert!(sent_any);
        assert!(failed_inline, "流内失败必须以 failed_inline 上报调用方");
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.failed"));
        assert!(body.contains("中途失败"));
    }

    /// 正常完成流：failed_inline 必须为 false（不得误报流内失败）
    #[test]
    fn stream_forward_success_reports_no_inline_failure() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (err, sent_any, failed_inline, usage, eof_truncated) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Responses, "resp_2", "m");
        assert!(err.is_none());
        assert!(sent_any);
        assert!(!failed_inline);
        assert!(usage.is_none(), "上游未下发 usage 时不得伪造");
        assert!(!eof_truncated, "正常 Done 收尾不得误报截断");
    }

    /// 审查 #6 回归：EOF 断流（有内容、未收到 Done/Error）→ eof_truncated=true
    /// 上抛调用方（终止帧已就地补齐，客户端正常收尾，但与上游原貌不符需可观测）
    #[test]
    fn stream_forward_eof_truncation_with_content_is_reported() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            // 有内容但上游直接断流：无 [DONE]、无 error 帧
            "data: {\"choices\":[{\"delta\":{\"content\":\"截断输出\"}}]}",
            "",
        ]);
        let (err, sent_any, failed_inline, _usage, eof_truncated) = stream_forward_ex(
            lines, &tx, crate::api_server::routes::Protocol::OpenAi, "c", "m",
        );
        assert!(err.is_none());
        assert!(sent_any);
        assert!(!failed_inline);
        assert!(eof_truncated, "EOF 截断补齐必须作为第 5 元上抛");
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("[DONE]"), "截断流必须补齐终止帧: {body}");
    }

    /// 流内失败标记对 Anthropic 协议同样生效（event: error 就地下发）
    #[test]
    fn stream_forward_anthropic_instream_error_marks_failed_inline() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"部分\"}}]}",
            "",
            "data: {\"error\":{\"message\":\"中途失败\",\"code\":7}}",
            "",
        ]);
        let (err, _sent_any, failed_inline, _usage, _t) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Anthropic, "msg_3", "m");
        assert!(err.is_none());
        assert!(failed_inline);
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("event: error"));
    }

    /// 流开始前遇错误：错误上抛（error_info）、无 failed_inline（调用方换号重试）
    #[test]
    fn stream_forward_error_before_stream_uplifts() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&["data: {\"error\":{\"message\":\"boom\",\"code\":9}}", ""]);
        let (err, sent_any, failed_inline, _usage, _t) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::OpenAi, "c", "m");
        assert_eq!(err.unwrap(), (9, "boom".to_string()));
        assert!(!sent_any);
        assert!(!failed_inline);
    }

    /// Anthropic message_delta 的 usage 附带 input_tokens（上游已知时）
    #[test]
    fn anthropic_message_delta_includes_input_tokens_when_known() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (err, sent_any, failed_inline, _u, _t) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Anthropic, "msg_4", "m");
        assert!(err.is_none() && sent_any && !failed_inline);
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        let md = body.split("event: message_delta").nth(1).unwrap_or("");
        assert!(md.contains("\"input_tokens\":7"), "message_delta 需携带 input_tokens: {md}");
        assert!(md.contains("\"output_tokens\":3"));
    }

    /// 上游未下发 usage 时 message_delta 的 input_tokens 保持 0
    #[test]
    fn anthropic_message_delta_input_tokens_zero_when_unknown() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
            "",
            "data: [DONE]",
            "",
        ]);
        let _ = stream_forward_ex(
            lines, &tx, crate::api_server::routes::Protocol::Anthropic, "msg_5", "m",
        );
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        let md = body.split("event: message_delta").nth(1).unwrap_or("");
        assert!(md.contains("\"input_tokens\":0"));
    }

    /// 缓存读 tokens 宽口径提取：主形态、别名、嵌套兜底、正值过滤
    #[test]
    fn cached_tokens_of_wide_caliber() {
        // 主形态：WB token_usage（glm 抓包实证）
        assert_eq!(cached_tokens_of(&json!({"cache_read_input_tokens": 7})), 7);
        // 别名：用量历史 extra_info 同源口径
        assert_eq!(cached_tokens_of(&json!({"cache_read_token": 8})), 8);
        // camelCase / 命中别名（对齐用量统计口径）
        assert_eq!(cached_tokens_of(&json!({"cacheReadInputTokens": 3})), 3);
        assert_eq!(cached_tokens_of(&json!({"prompt_cache_hit_tokens": 12})), 12);
        // 0 值别名不遮蔽后续别名（对齐 positive_field 语义）
        assert_eq!(
            cached_tokens_of(&json!({"cache_read_input_tokens": 0, "prompt_cache_hit_tokens": 12})),
            12
        );
        // 嵌套兜底：OpenAI prompt_tokens_details
        assert_eq!(
            cached_tokens_of(&json!({"prompt_tokens_details": {"cached_tokens": 9}})),
            9
        );
        // 嵌套兜底：inputTokensDetails 数组
        assert_eq!(
            cached_tokens_of(&json!({"inputTokensDetails": [{"cached_tokens": 4}]})),
            4
        );
        // 全 0 与缺省均返回 0
        assert_eq!(cached_tokens_of(&json!({"cache_read_input_tokens": 0})), 0);
        assert_eq!(cached_tokens_of(&json!({})), 0);
    }

    /// 缓存写 tokens 宽口径提取：别名覆盖与正值过滤
    #[test]
    fn cache_write_tokens_of_wide_caliber() {
        assert_eq!(cache_write_tokens_of(&json!({"cache_creation_input_tokens": 5})), 5);
        assert_eq!(cache_write_tokens_of(&json!({"cache_write_input_tokens": 6})), 6);
        assert_eq!(cache_write_tokens_of(&json!({"cacheWriteInputTokens": 7})), 7);
        assert_eq!(cache_write_tokens_of(&json!({"prompt_cache_write_tokens": 2})), 2);
        // 单数 cache_write_token：用量历史 extra_info 同源口径（读侧 cache_read_token 的对称收录）
        assert_eq!(cache_write_tokens_of(&json!({"cache_write_token": 8})), 8);
        // 0 值别名不遮蔽后续别名（含单数键）
        assert_eq!(
            cache_write_tokens_of(&json!({
                "cache_creation_input_tokens": 0,
                "prompt_cache_write_tokens": 3
            })),
            3
        );
        assert_eq!(
            cache_write_tokens_of(&json!({"cache_creation_input_tokens": 0, "cache_write_token": 3})),
            3
        );
        assert_eq!(cache_write_tokens_of(&json!({})), 0);
    }

    /// 宽口径数值解析对齐 workbuddy_stats::number()：f64 / 字符串形态不再静默得 0
    #[test]
    fn token_number_parses_f64_and_string() {
        // f64 截断（读侧与写侧键）
        assert_eq!(cached_tokens_of(&json!({"cached_tokens": 12.0})), 12);
        assert_eq!(cache_write_tokens_of(&json!({"cache_write_token": 7.0})), 7);
        // 字符串数字
        assert_eq!(cache_write_tokens_of(&json!({"cache_write_token": "7"})), 7);
        assert_eq!(cached_tokens_of(&json!({"cache_read_input_tokens": "34"})), 34);
        // u64_pair 同口径：prompt/completion 支持浮点与字符串
        assert_eq!(
            u64_pair(&json!({"prompt_tokens": 10.0, "completion_tokens": "3"})),
            (10, 3)
        );
        // 负值 / 非法字符串按缺省 0 处理
        assert_eq!(cached_tokens_of(&json!({"cached_tokens": -3})), 0);
        assert_eq!(cache_write_tokens_of(&json!({"cache_write_token": "abc"})), 0);
    }

    /// Anthropic 流式出口 message_delta 透传缓存读/写 tokens（0 值别名不遮蔽）
    #[test]
    fn anthropic_message_delta_carries_cache_tokens() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":6,\"cache_read_input_tokens\":0,\"prompt_cache_hit_tokens\":80,\"cache_creation_input_tokens\":5}}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (err, sent_any, failed_inline, _u, _t) = stream_forward_ex(
            lines, &tx, crate::api_server::routes::Protocol::Anthropic, "msg_6", "m",
        );
        assert!(err.is_none() && sent_any && !failed_inline);
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        let md = body.split("event: message_delta").nth(1).unwrap_or("");
        assert!(md.contains("\"input_tokens\":100"), "message_delta 需携带 input_tokens: {md}");
        assert!(
            md.contains("\"cache_read_input_tokens\":80"),
            "0 值别名不得遮蔽正值别名: {md}"
        );
        assert!(md.contains("\"cache_creation_input_tokens\":5"), "需透传写侧 tokens: {md}");
    }

    /// Anthropic 非流式出口 completion_to_anthropic 透传缓存读/写 tokens
    #[test]
    fn completion_to_anthropic_carries_cache_tokens() {
        let v = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 1,
                "cache_read_input_tokens": 0,
                "prompt_cache_hit_tokens": 80,
                "cache_creation_input_tokens": 5
            }
        });
        let out = completion_to_anthropic(&v, "msg_7", "glm-5.3");
        let usage = out.get("usage").cloned().unwrap_or(json!({}));
        assert_eq!(usage["input_tokens"], json!(9));
        assert_eq!(usage["output_tokens"], json!(1));
        assert_eq!(
            usage["cache_read_input_tokens"], json!(80),
            "0 值别名不得遮蔽正值别名"
        );
        assert_eq!(usage["cache_creation_input_tokens"], json!(5));
    }
}
