# WB 池断流空回完整解决方案（issue #213）

> **docker 版说明**：本文档自 main 分支（桌面版，提交 6f8a8cb）语义移植，方案与实现已在
> docker_main 分支落地（`wb_pseudo_stream` 入口 / 预算函数 / 聚合 sink 抽取 / S6·S7 接线）。
> 正文中的代码路径按桌面版撰写（`src-tauri/src/api_server/*`），docker 形态对应
> `crates/aiwork-core/src/api_server/*`；涉及 `#[tauri::command]` 的接线为 cmd_bridge
> `pseudo_stream_switches_get/set` / `retry_budgets_get/set` / `pseudo_keepalive_get/set`。
> 桌面客户端专属细节（Tauri 窗口/托盘联动）在 docker 形态不适用。
>
> 状态：已落地（1.6.6）　|　关联 issue：#213（WB 池上游流式传输被掐，断流率 ~30%）　|　关联：#158（传输掐断识别）、#162（cut 归因）、#57（空完成换号）、#122（指纹清洗）
> 日期：2026-11-19

---

## 📋 1. 背景与根因分析

### 1.1 问题现象

WB 池（Buddy CN / BuddyAI Intl）上游流式传输在响应中途被上游传输层掐断（rustls `cannot decrypt peer's message` / `Error while decoding chunks`），客户端侧表现为：

- **已流出内容**：流被掐断，客户端长时间无新帧后连接超时（用户感知「卡死」）；
- **未流出内容**：卡死同样发生（某些路径收尾帧未补发）；
- 断流率约 30%，与账号/模型/指纹/并发均无关；
- 上游存在约 3 分钟「全黑」窗口（连续 12 次重试全 503）。

### 1.2 根因链路（代码证据）

| # | 证据点 | 位置 | 说明 |
|---|--------|------|------|
| 1 | 上游被强制流式 | [wb_payload.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_payload.rs) `prepare_wb_chat_body` 内强制 `stream:true` | 无论客户端是否流式，出站恒为流式请求 |
| 2 | `sent_any` 护栏 | [wb_route.rs#L1563-1575](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_route.rs#L1563-1575) `run_wb_stream` 传输掐断分支：内容已流出 → 只能就地收尾（客户端看到截断/卡死）；未流出 → break 换号 | 护栏是对的（防止向同一 SSE 流拼接第二份完整响应），但代价是**流式路径中途掐断不可恢复** |
| 3 | 换号预算 = 池内账号数 | `run_wb_stream` 中 `tried: HashSet<String>` 去重，每账号仅一次机会 | 3 账号池遇到 3 分钟全黑窗口 → 轮换即耗尽 → 503 |
| 4 | 聚合路径天然免疫 | [wb_route.rs#L1850](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_route.rs#L1850) `wb_aggregate_chat`：`sent_any` 恒 false（内容尚未下发），断流/空完成**全部可无损换号重试** | 关键洞察：聚合路径的重试自由度远高于流式路径 |

### 1.3 根因总结

> **客户端流式语义（stream:true）与上游不稳的流式通道耦合过紧**：客户端要求流式时，网关被迫直接转发上游 SSE，一次中途掐断即不可恢复（重试会拼接第二份响应）；而网关本地的聚合重试管线（换号、指纹清洗、跨池兜底）明明具备完整的自愈能力，只是流式客户端用不到。

修复思路由此确定：**把「客户端流式」与「上游流式转发」解耦**——客户端要流式，网关内部仍走聚合管线（可无损换号重试），拿到完整聚合结果后按客户端协议合成 SSE 下发。这是「伪流式」（pseudo streaming）。

### 1.4 已具备的积木（无需重做）

| 积木 | 位置 | 复用点 |
|------|------|--------|
| 非流式聚合管线（含分级重试/11128 清洗/空完成换号/对冲竞速） | `wb_aggregate_chat`（wb_route.rs#L1850） | 伪流式内核直接复用 |
| Responses SSE 合成器（created → items → completed） | [wb_route.rs#L2945-3000](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_route.rs#L2945-3000)（tool_exec 现成） | Responses 协议合成直接复用 |
| SSE keep-alive ticker（watch + DoneSignal 模式） | [wb_route.rs#L1118-1141](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_route.rs#L1118-1141) | 等待期 keep-alive 直接复用 |
| 跨池兜底 `continue_stream_on_next_pool`（F-81） | [cross_pool.rs#L60](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/cross_pool.rs#L60) | 聚合路径此前未接线，本方案补上 |
| 池级开关热切换范式 | [commands/api_server.rs#L1869](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/api_server.rs#L1869) `sanitize_switch_set` | 伪流式开关沿用同一范式 |
| PoolFile 配置字段范式 | [models.rs#L840-960](file:///d:/code/AIWorkAssistant_main/src-tauri/src/models.rs#L840-960)（serde default + AtomicBool 装载） | 新配置字段沿用 |
| 传输掐断不计账号健康 | [mod.rs#L872-912](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/mod.rs#L872-912) | 保持不动 |
| cut_hits 计入耗尽归因（PR #162） | `exhaust_message_with_transport_cut` | 保持不动 |
| 空完成换号（#57）/ 11128 强制清洗（#195） | 各执行路径 | 保持不动 |

---

## 🎯 2. 目标与非目标

### 2.1 目标

1. **G1（核心）**：客户端 stream:true 时，可按池级开关切换为「内部聚合 + 合成 SSE 下发」，断流场景全部可无损换号重试，用户不再看到卡死/截断。
2. **G2**：聚合路径接上跨池兜底链（fallback），本池耗尽时移交下一池，进一步压低 503 概率。
3. **G3**：换号重试预算可配置（`wb_retry_budget`），突破「账号数 = 重试次数」的天花板，支撑 3 分钟全黑窗口的等待重试。
4. **G4**：传输掐断关键词补齐（rustls 形态），cut 归因更准；断流现场可观测（sent_bytes/最后帧类型）。
5. **G5**：全部改动池级可配、热切换、可回滚（开关关闭 = 现状行为逐字节一致）。

### 2.2 非目标

- 不改上游 payload 强制 `stream:true` 的现状（这是上游通道形态，不属于本 issue 处置范围）；
- 不做逐 token 真流式合成（伪流式整段下发已满足客户端协议；真流式回放属于后续增强，见 §11 决策点）;
- 不动指纹清洗模板体系与 11128 处置口径；
- 不改 Trae/Qoder 池（它们有自己的 sent_any 收尾语义，问题形态不同）。

---

## 🔧 3. 总体设计

### 3.1 核心思路：三阶段

```
阶段 1（P0，治本）：伪流式 wb_pseudo_stream
  客户端 stream:true → 内部走聚合管线（上游恒 SSE、断流全可无损换号）
  → 成功后按客户端协议合成 SSE 帧序列下发
  → 聚合路径接 fallback 跨池兜底链
阶段 2（P1，提额）：wb_retry_budget 重试预算
  tried 耗尽且未达 budget → 清空非冷却账号重开轮换 + 抖动退避
阶段 3（P2，观测）：cut 关键词补齐 + 断流现场记录
```

### 3.2 改造后请求走向

```
客户端（stream:true）
  │
  ├─ wb_pseudo_stream OFF（默认回退 = 现状）：
  │    routes.rs 四处分流点 → wb_stream_chat → run_wb_stream（现状不动）
  │
  └─ wb_pseudo_stream ON：
       routes.rs 分流点改投 wb_pseudo_stream
         → spawn_blocking 聚合内核（复用 wb_aggregate_chat 主体逻辑，
            含 sticky/约束/11128/空完成/对冲/分级重试/keep-alive ticker）
         → 成功：按 proto 合成 SSE 帧序列（OpenAI / Anthropic / Responses / OpenAiText）
         → 本池耗尽：fallback 跨池兜底 → continue_stream_on_next_pool（阶段1接线）
         → 全链耗尽：按 proto 发错误帧收尾（对齐现 wb_stream_chat 耗尽收尾）
```

### 3.3 分阶段总览

| 阶段 | 内容 | 主要文件 | 风险 |
|------|------|----------|------|
| P0 | 伪流式入口 + 分流接线 + 聚合侧 fallback + keep-alive | wb_route.rs / routes.rs / mod.rs | 中（新增路径，默认关） |
| P1 | 重试预算 | wb_route.rs / models.rs | 低 |
| P2 | cut 关键词补齐 + 断流现场记录 | mod.rs / wb_route.rs / wb_sse.rs | 低 |

---

## 🔧 4. 阶段 1（P0）：伪流式 `wb_pseudo_stream`

### 4.1 配置与开关

**PoolFile 新字段**（[models.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/models.rs)，沿用既有 per-pool 字段范式）：

```rust
/// WB 池伪流式开关（issue #213）：客户端 stream:true 时内部走聚合管线，
/// 成功后合成 SSE 下发——断流场景全部可无损换号重试。默认 false（现状行为）。
#[serde(default)]
pub wb_pseudo_stream: bool,
/// buddy-ai 档同构开关（独立配置，与 CN 档分开灰度）
#[serde(default)]
pub buddy_ai_pseudo_stream: bool,
/// 伪流式等待期 keep-alive 间隔秒（0 = 关闭 keep-alive，默认 5）
#[serde(default = "default_pseudo_keepalive_secs")]
pub pseudo_keepalive_secs: u64,
```

**运行时热切换**：`ApiSharedState` 新增 `wb_pseudo_stream: AtomicBool` 与 `buddy_ai_pseudo_stream: AtomicBool`（装载自 PoolFile，与 `wb_sanitize` 同位 [mod.rs#L62](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/mod.rs#L62)）。`commands/api_server.rs` 新增 Tauri command：

```rust
#[tauri::command]
pub fn pseudo_stream_switch_set(runtime, pool: String, enabled: bool) -> Result<(), String>
// pool ∈ {"wb", "buddy_ai"}；实现完全对齐 sanitize_switch_set（L1869）：
// AtomicBool store + app_log 留痕 + 服务未运行报错
```

配套 `pseudo_stream_switches_get` 供前端回显（对齐 `sanitize_switches_get`）。前端在 WB / BuddyAI 资源页各加一枚开关（样式复用指纹清洗开关）。

**WbView 取值**：`WbView` 新增 `pseudo_stream_on()`（对齐 `sticky_on()` 范式——CN 读 `wb_pseudo_stream`，Intl 读 `buddy_ai_pseudo_stream`）。

### 4.2 路由分流点改造（routes.rs 四处）

四处 `if stream` 分流（OpenAi [L1274-1277](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/routes.rs#L1274-1277)、Responses L1451、Anthropic L1651、OpenAiText L1806）统一改为三分支：

```rust
if stream && state.pseudo_stream_on(buddy_ai) {
    return wb_route::wb_pseudo_stream(state_clone, buddy_ai, body_vec, r.model, start_ts,
        Protocol::OpenAi, key_str, guard, std::collections::HashSet::new(), r.fallback_chain);
}
if stream {
    return wb_route::wb_stream_chat(/* 现状不动 */);
}
return wb_route::wb_aggregate_chat(/* 现状不动 */);
```

判定函数放 `ApiSharedState`（routes.rs 持有共享态，无 WbView）：

```rust
pub fn pseudo_stream_on(&self, buddy_ai: bool) -> bool {
    (if buddy_ai { &self.buddy_ai_pseudo_stream } else { &self.wb_pseudo_stream })
        .load(std::sync::atomic::Ordering::Relaxed)
}
```

注意：`/v1/responses` 的 tool_exec 路径（L1431 `wb_tool_exec_chat`）**不走**伪流式分流——它内部已是「上游非流式聚合 + 合成 SSE」，天然免疫断流拼接，保持现状。

### 4.3 新入口 `wb_pseudo_stream`（wb_route.rs）

签名与 `wb_stream_chat`（L1088）完全同构（方便未来来回切换，也便于单测对齐）：

```rust
pub fn wb_pseudo_stream(
    state: Arc<ApiSharedState>,
    buddy_ai: bool,
    body_vec: Vec<u8>,
    model: String,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
    encoded: std::collections::HashSet<String>,
    fallback: Vec<super::dispatch::PoolCandidate>,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    // keep-alive ticker：与 wb_stream_chat 同款 watch + DoneSignal（L1118-1141），
    // 间隔取 pseudo_keepalive_secs（默认 5s，比 15s 更激进防 idle 回收）
    spawn pseudo_keepalive_ticker(&tx);
    // 聚合内核迁入专用阻塞池（对齐 wb_aggregate_chat 的 stream_runtime 迁移注记）
    super::stream_runtime().spawn_blocking(move || {
        let view = WbView::new(state, buddy_ai);
        run_wb_pseudo_stream(&view, &body_vec, &model, proto, &key_id, &chat_id, &tx,
            start_ts, guard, &encoded, &fallback);
    });
    // SSE 响应头与 wb_stream_chat 一致
    Response::builder().header("content-type", "text/event-stream") /* … */
}
```

`run_wb_pseudo_stream` 内核 = **`wb_aggregate_chat` 闭包主体的参数化移植**（`stream=true` 口径），差异点仅三处：

1. **成功出口**：聚合成功拿到 `completion: Value` 后不返回 JSON，而是调 `synthesize_sse(tx, proto, completion, chat_id, model, encoded)` 合成 SSE 下发；
2. **fallback 接线**：本池轮换耗尽（pick None 分支）时，先走跨池兜底：
   ```rust
   if !fallback.is_empty() && !tx.is_closed() {
       if super::cross_pool::continue_stream_on_next_pool(&state.state, state.pool_label(),
           fallback, body_vec, proto, key_id, chat_id, tx, guard, encoded, start_ts) {
           return; // 移交成功：接收池落终态用量，本池不补 "none" 行（防双记账）
       }
   }
   ```
   （对齐 run_wb_stream 耗尽收尾 L1364-1368 的既有移交范式。）
3. **keep-alive 交互**：等待期（首次取号前 / RetrySame 退避 sleep 期间 / 对冲竞速期间）不阻塞 keep-alive ticker——ticker 独立 task 已保证；`RetrySame` 的 `std::thread::sleep` 保持现状（阻塞内核但不阻塞 ticker）。

其余（粘性解析、Key 约束、tried/refreshed/empty_hits/cut_hits 计数、种子改写、别名改写、残指纹自检、11128 流内/HTTP 双形态处置、retry_plan 分级重试、TTFT 记账、耗尽收尾文案）**零改动复用聚合路径既有逻辑**。实现上建议：把 `wb_aggregate_chat` 闭包主体抽出为 `run_wb_aggregate_inner(..., sink: AggregateSink)`，聚合路径以 `AggregateSink::Json` 调用（现状行为不变），伪流式以 `AggregateSink::Sse { tx, proto, chat_id }` 调用——单一内核两出口，杜绝复制漂移。

### 4.4 SSE 帧合成（各协议帧序列）

新增 `wb_route::synthesize_sse`（或置于 wb_sse.rs，视内聚性）。**各协议帧序列规格**：

#### OpenAI（/v1/chat/completions）

```
data: {"id":"chatcmpl-…","object":"chat.completion.chunk","created":…,"model":…,
       "choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}
                       ← 首帧（role 帧）
data: {"…choices":[{"index":0,"delta":{"content":"<整段文本>"}}]}        ← 内容帧（整段一次下发）
data: {"…choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}        ← 收尾帧
[若聚合结果含 tool_calls：以 finish_reason:"tool_calls" 收尾，
 内容帧改为 delta.tool_calls 按 index 逐项完整下发（id/function.name/arguments 整段）]
[若 completion.usage 非空：收尾帧前补一帧 stream_options 语义的 usage 帧（choices:[] + usage）]
data: [DONE]
```

#### Anthropic（/v1/messages）

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_…","type":"message","role":"assistant",
       "model":…,"content":[],"stop_reason":null,"usage":{"input_tokens":…,"output_tokens":1}}}
event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<整段文本>"}}
event: content_block_stop       data: {"type":"content_block_stop","index":0}
[tool_use：content_block_start(type=tool_use,id,name) → content_block_delta(input_json_delta,
 partial_json=整段 arguments) → content_block_stop，stop_reason="tool_use"]
event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},
       "usage":{"output_tokens":…}}
event: message_stop             data: {"type":"message_stop"}
```

帧序列从 `wb_sse::completion_to_anthropic` 的既有投影逻辑推导（同一 completion 输入，流式版只是把它拆成事件序列）。

#### Responses（/v1/responses）

**直接复用** [wb_route.rs#L2945-3000](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_route.rs#L2945-3000) 现成合成器：`response.created → response.output_item.added → response.output_text.delta（整段一次）/ response.function_call_arguments.delta → response.output_item.done → response.completed`。

#### OpenAiText（/v1/completions）

```
data: {"id":"cmpl-…","object":"text_completion","choices":[{"index":0,"text":"<整段>","finish_reason":null}]}
data: {"…","choices":[{"index":0,"text":"","finish_reason":"stop"}]}
data: [DONE]
```

从 `wb_sse::completion_to_text` 投影推导。

> **实现提示**：四个协议的「整段一次下发」均与 tool_exec 合成器口径一致（聚合投影无逐 token 流）。若客户端（如 Codex CLI）对 `output_text.delta` 分帧粒度有断言（个别客户端按 delta 计数做进度条），可在合成时按 ~512 字符切片拆多个 delta 帧——纯帧粒度调整，语义不变，作为实现细节保留。

### 4.5 keep-alive 规格

- 间隔：`pseudo_keepalive_secs`（默认 5s；0 = 关闭）。等待聚合期间每 5s 发 `: keep-alive\n\n`（SSE 注释帧，各协议客户端均忽略，wb_sse 解析器同样跳过 `:` 行）；
- 复用 watch + DoneSignal 模式（[routes.rs DoneSignal](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/routes.rs)）：内核任务结束 Drop 触发 watch → ticker 退出 → sender 关闭 → 流正常终结（含 panic 展开兜底）；
- keep-alive 帧与合成数据帧共用同一 tx（mpsc 串行化，无交错风险——ticker 只在内核未发数据帧时有机会发出，且 `: ` 注释帧即使穿插在数据帧之间也是合法 SSE）。

---

## 🔧 5. 阶段 2（P1）：重试预算 `wb_retry_budget`

### 5.1 动机

现状换号预算 = 池内账号数（tried HashSet 去重）。3 账号池遇 3 分钟全黑窗口：账号 A→B→C 各试一次全 503 → 耗尽 → 503。`wb_retry_budget` 允许在预算内**清空非冷却账号重开轮换**，配合退避等待跨过黑窗。

### 5.2 配置

```rust
/// WB 换号重试预算（issue #213 阶段2）：0 = 旧行为（重试机会 = 池内账号数）；
/// >0 = 绝对次数上限——tried 耗尽且总换号次数未达 budget 时，清空非冷却账号
/// 重开轮换 + 抖动退避后继续（预算含首轮所有账号的尝试）
#[serde(default)]
pub wb_retry_budget: u32,
/// buddy-ai 档同构（默认 0）
#[serde(default)]
pub buddy_ai_retry_budget: u32,
```

（PoolFile 字段 + ApiSharedState AtomicU32 装载；暂不做热切换 UI——先配置文件生效，与 hedge 阈值同档。）

### 5.3 内核改造（wb_route.rs 三条路径统一：流式 / 聚合 / 伪流式）

`run_wb_stream` 与聚合内核的取号 None 分支（耗尽收尾处）之前插入预算判定：

```rust
None => {
    let budget = state.retry_budget();           // 0 = 旧行为
    let rotations = tried.len() as u32;          // 现状耗尽判定不变
    if budget > 0 && rotations < budget {
        // 清空非冷却账号重开轮换（冷却账号保持冷却——不再烧已判定不可用的号）
        let mut cleared = HashSet::new();
        for uid in tried.iter() {
            if account_model_cooling_remaining(state, uid, model).is_none() {
                cleared.insert(uid.clone());
            }
        }
        if cleared.len() < tried.len() {
            // 抖动退避：base 2s × 轮次，±25% 抖动，封顶 15s（防雪崩对齐上游节奏）
            let round = (rotations / pool_size).max(1);
            let delay = (2_000u64 * round).min(15_000);
            let jittered = jitter(delay);
            std::thread::sleep(...jittered...);
            // 清除后 tried 保留仍在冷却中的账号，重开轮换
            tried = cleared_with_cooldown_kept;
            continue;
        }
        // 所有已试账号均进冷却：无重开意义，落原耗尽收尾
    }
    // …原耗尽收尾（exhaust_message_with_transport_cut / 指纹短记忆 / fallback 移交 / 错误帧）
}
```

**口径细节**：
- 预算计数以「本轮内发起过上游请求的账号数」为准（tried.len()），CAS 拒绝未发请求的候选不计；
- 重开轮换不重置 `empty_hits`/`cut_hits`/`aggressive_tried`（归因与指纹升级保持请求级累计）；
- 每轮重开必须观察到「至少一个账号状态变化」（冷却到期/新账号入池/凭证刷新），否则 `cleared.len() == tried.len()` 直接落耗尽收尾，防止空转；
- RetrySame（同号退避）不计入预算——预算只约束**换号轮换**。

### 5.4 与全黑窗口的配合

3 账号池 + `wb_retry_budget=9` + 503 渐进退避（retry_plan 10/20/40s）：A→B→C（~70s）→ 重开 A→B→C（部分进冷却则跳过，~90s）→ 重开（~150s）→ 累计可跨过 3 分钟黑窗。预算默认 0（行为不变），建议灰度值 6~9。

---

## 🔧 6. 阶段 3（P2）：可观测性

### 6.1 cut 关键词补齐（[mod.rs#L885-896](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/mod.rs#L885-896) `is_transport_cut_text`）

issue #213 实证 rustls 形态未覆盖，补两条：

```rust
|| m.contains("cannot decrypt peer's message")   // rustls 解密失败（TLS 层掐断）
|| m.contains("error while decoding chunks")     // hyper 解码失败（流体截断）
```

命中即走既有 TRANSPORT_CUT 口径（不冷却、不解绑粘性、cut_hits 归因）。注意 `to_ascii_lowercase()` 已在函数内完成，新增词条用小写即可。

### 6.2 断流现场记录

`wb_sse::stream_forward_ex` 返回值已有 `sent_any`，扩展为随错误信息附带现场：

- 聚合/伪流式路径：cut 命中时 debug 日志附 `sent_bytes=<聚合字节数> last_frame_kind=<Chunk|Error|Done>`（聚合器内已逐帧解析，加两个计数字段零成本）；
- 流式路径：cut 且 `sent_any=true` 就地收尾时，`log_request_ttfb` 的 note 已含 msg；补充 `sent_bytes`（转发器内累计字节数）便于量化「截断发生位置分布」（若截断集中在固定字节数附近，即可推断上游缓冲/超时配额，为反推上游策略提供数据）。

---

## ⚠️ 7. 边界场景与处置

| # | 场景 | 处置 |
|---|------|------|
| 1 | **客户端断连**（等待聚合期间离开） | 内核各既有断连检测点照常生效（tx.is_closed()）；keep-alive ticker send 失败即退出。聚合路径无逐帧转发，断连检测依赖「取号前 / RetrySame 退避后」的检查点 + ticker send 错误兜底——补一个检查点：聚合成功合成前 `if tx.is_closed() { return; }`（不落成功账） |
| 2 | **响应首帧延迟** | 伪流式 TTFT = 完整聚合耗时（可能 10~60s）。keep-alive 注释帧保活防 idle 超时；客户端如设了较短的 read timeout（< 聚合耗时且 keep-alive 帧间隔 > timeout），需调大 keep-alive 频率——默认 5s 已覆盖绝大多数客户端（read timeout 通常 ≥ 30s）。文档明示：伪流式以**首字延迟换完整性**，按需开启 |
| 3 | **TTFT 统计失真** | 伪流式路径的 ttfb_ms 记录的是聚合耗时，直接入账会拉高 P50/P95 分位、污染对冲阈值调参。处置：`record_usage_bucket_ttfb` 入账时伪流式请求的 ttfb 打标（usage 行 pool 标签后附 `mode=pseudo`），看板侧可过滤；不单独建桶（避免分桶爆炸） |
| 4 | **usage 记账口径** | 伪流式成功 = 聚合成功，与聚合路径完全同口径（成功行落 win_uid 真实账号、失败中间态不落账、跨池移交成功由接收池落终态）；`stream` 维度入账传 true（客户端视角是流式请求，用量页 requests 口径不变） |
| 5 | **粘性会话语义** | 聚合内核复用既有 sticky0/sticky_bind 逻辑：绑定发生在聚合成功后（与现状聚合路径一致）；伪流式不改变粘性 TTL/绑定语义。换号重试期间粘性不绑定（失败不绑，既有口径） |
| 6 | **工具调用响应** | 聚合器已按 index 合并 tool_calls delta（wb_sse::aggregate），合成帧按 §4.4 规格下发完整 tool_calls/tool_use/function_call；OpenAI 协议收尾帧 finish_reason 按「有无 tool_calls」分流（stop / tool_calls） |
| 7 | **工具代执行互斥** | `/v1/responses` tool_exec 路径不进伪流式分流（§4.2 已述）；客户端声明 web_search 类工具时优先走 tool_exec，无互斥冲突 |
| 8 | **上游空完成 + 内容已流出** | 伪流式下不存在该场景——内容只在聚合成功后才合成下发，上游空完成时客户端侧零流出，换号重试恒无损。这正是伪流式的核心收益 |
| 9 | **对冲竞速** | 聚合内核保留 race_first_byte/settle_hedge 既有逻辑，语义不变；伪流式下「首字」是上游 SSE 首帧而非客户端首帧，对冲判定标准不漂移 |
| 10 | **`stream_options.include_usage`** | OpenAI 协议合成时若客户端请求体带 include_usage，收尾帧前补 usage 帧（choices:[]）；不带则不补（严格对齐 OpenAI 语义） |
| 11 | **编码参数** | `encoded`（响应编码约束）透传合成器（Responses 合成器已接 encoded 参数），Anthropic/OpenAI 合成器按既有聚合路径同口径处理 |
| 12 | **fallback 跨池后协议** | `continue_stream_on_next_pool` 已按 proto 分流接收池执行路径；接收池若为流式直连池（如 trae），其 sent_any 收尾语义由接收池自行处置——移交点在聚合成功**之前**（耗尽收尾处），客户端侧仍零流出，无拼接风险 |

---

## ✅ 8. 测试与验收标准

### 8.1 单元测试

| 用例 | 断言 |
|------|------|
| synthesize_sse 各协议帧序列 | OpenAI：首帧 role / 内容帧整段 / finish_reason stop / [DONE]；tool_calls → finish_reason tool_calls + usage 帧条件补发；Anthropic：message_start→…→message_stop 六段序列与 tool_use 变体；Responses：created→…→completed；OpenAiText：两帧 + [DONE] |
| retry 预算重开 | budget=0 → 现状耗尽；budget>0 且有非冷却账号 → tried 重开、冷却账号保留；全部冷却 → 直接耗尽收尾 |
| cut 关键词 | "cannot decrypt peer's message" / "Error while decoding chunks" 大小写混合形态均命中 is_transport_cut_text；正常业务错误消息不误撞 |
| pseudo_stream_on | wb/buddy_ai 独立取值、默认 false |

### 8.2 集成验收（人工 + 脚本）

1. **模拟断流重试成功**：测试钩子（或 mock 上游）让首轮账号聚合中途 transport cut → 伪流式路径换号 → 第二账号成功 → 客户端收到**完整**合成 SSE（无截断、无拼接双份）；
2. **keep-alive 防超时**：构造 30s+ 聚合耗时请求 → 客户端（curl --no-buffer / Codex CLI）在等待期持续收到 `: keep-alive`，最终收到完整响应，无 idle 断连；
3. **budget 生效**：3 账号池注入「前 2 轮全 503、第 3 轮恢复」故障 → budget=6 时请求最终成功，app_log 可见两轮轮换记录；budget=0（默认）时首轮耗尽即 503；
4. **fallback 移交**：WB 池全耗尽 + 链上有 buddy-ai 候选 → 伪流式路径移交成功，客户端收到接收池合成的完整响应，无双记账（usage 单行）；
5. **回退一致性**：开关 OFF 时，流式请求走向与改造前逐字节一致（diff 请求日志 + 客户端收帧）；聚合请求完全不动；
6. **既有测试全绿**：`cargo test -p aiworkassistant`（wb_route / wb_sse / retry / cross_pool 相关用例不回归）。

---

## 🔧 9. 实施步骤分解

| 步骤 | 内容 | 依赖 |
|------|------|------|
| S1 | mod.rs：cut 关键词补齐（6.1）+ 断流现场字段（6.2，wb_sse 配合） | 无（可先行合入） |
| S2 | models.rs：PoolFile 三新字段 + default 函数；ApiSharedState 新 Atomic 装载 | 无 |
| S3 | wb_route.rs：抽出 `run_wb_aggregate_inner`（AggregateSink 参数化），聚合路径改调（行为零变化），回归既有测试 | S2 |
| S4 | wb_route.rs：`synthesize_sse` 四协议实现 + 单测 | 无 |
| S5 | wb_route.rs：`wb_pseudo_stream` 入口（keep-alive ticker + 内核 Sse 出口 + 耗尽处 fallback 移交 + 合成前断连检查点） | S3+S4 |
| S6 | routes.rs：四处分流点接线 + `pseudo_stream_on` | S5 |
| S7 | commands/api_server.rs：`pseudo_stream_switch_set/get` + main.rs 注册 | S6 |
| S8 | wb_route.rs 三路径：retry 预算重开逻辑（5.3） | S2 |
| S9 | 前端：WB/BuddyAI 页伪流式开关（复用指纹清洗开关组件） | S7 |
| S10 | 集成验收（8.2 全清单）+ 文档（池配置说明补段落） | 全部 |

每个步骤独立可编译可测；S1/S2/S4 可并行先行。

---

## 📌 10. 风险与回滚

| 风险 | 等级 | 缓解 |
|------|------|------|
| 伪流式首字延迟显著变长，用户体验回退 | 中 | 默认 OFF；文档明示权衡（完整性 vs 首字延迟）；keep-alive 5s 保活；开关热切换可秒级回退 |
| 整段下发对个别客户端 delta 粒度断言不兼容 | 低 | §4.4 实现提示：按 ~512 字符切片拆 delta 帧 |
| 抽出 aggregate_inner 引入行为漂移 | 中 | S3 独立提交、仅参数化不改逻辑；改造前后请求日志 diff 验证；既有聚合测试全绿门禁 |
| 预算重开在池极小时空转烧请求 | 低 | 5.3 口径：无状态变化即落耗尽；预算默认 0 |
| cut 关键词过宽误撞业务错误 | 低 | 仅作用于 io::Error Display 文本（传输层），业务错误消息不经该函数（mod.rs 注释口径不变） |
| fallback 移交双记账 | 低 | 沿用 run_wb_stream 既有移交范式（usage 行后置，审查 A-1 口径） |

**回滚**：所有新行为均由默认关闭的开关/字段控制；关闭即恢复现状。最坏情况直接 revert 对应提交（各阶段独立提交，无交叉耦合）。

---

## 📌 11. 决策点（评审确认）

1. **伪流式默认值**：默认 OFF（本方案），或对 BuddyAI Intl 档默认 ON（该档断流率最高、账号少更易全黑）？建议：先 OFF 灰度，Intl 档文档推荐开启。
2. **keep-alive 默认间隔**：5s（本方案）vs 3s（更保守）。5s 已远小于常见客户端 read timeout，建议 5s。
3. **delta 切片粒度**：整段一帧（最简）vs ~512 字符切片（兼容性更稳）。建议切片，成本一条循环。
4. **预算默认值**：0（旧行为，本方案）vs 6（直接生效）。建议 0，灰度后在推荐配置文档给 6~9。
5. **TTFT 打标方式**：usage 行 pool 后缀 `mode=pseudo`（本方案）vs 独立分桶。建议后缀，避免分桶爆炸。
