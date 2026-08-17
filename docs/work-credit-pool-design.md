# Work 积分资源池：方案设计与落地建议

> 目标：把多个 Trae 账号的 **Work 积分（product_id 209）** 聚成一个对外部调用方**无感**的资源池，
> 通过调度机制对外提供 **OpenAI 兼容接口**（`POST /v1/chat/completions`）。
>
> 原则（本方案严格遵守）：**最小代码、不改现有业务与架构**。现有能力只复用、不修改。

---

## 0. 结论先行（TL;DR）

1. 项目里**已经有一套完整的「透明积分池 + OpenAI 接口」壳**，但它当前只吃 **IDE 积分（208）**——
   上游是 `EP_LLM_CHAT = /api/agent/v3/llm_utils_chat`，余额来源 `ide_user_ent_usage` 也是 IDE 积分。
   **Work 积分（209）尚未接入**。
2. 抓包实证：Work 积分只能由**实时 Trae SOLO 会话**本身发起 `create_agent_task` 来消耗；
   该请求体由原生层 `ai_agent.dll` 构造、由闭源 `@aha-kit` 加密，**外部无法复刻**（用真实身份复刻仍 4001，已证伪）。
3. **唯一可行且对外部无感的突破路径 = 多活会话编排**：每个 Work 账号跑一个实时 Trae SOLO 实例，
   Work Assistant 作为编排层按池选账号 → 驱动对应实例 → 捕获 SSE（现有 MITM 代理）→ `sse.rs` 转 OpenAI。
4. 这条路径是**增量的**：在现有池/端点/转换之上 **+1 个 `work_transport` 模块 + 一个配置开关**，
   现有 `routes.rs / pool.rs / sse.rs / server.rs / payload.rs` **全部不动**。
5. 真正的成本在**运维**（N 个实时会话 + 如何"驱动"它们发起对话），不在代码量。

---

## 1. 现状盘点：已建好的壳（直接复用，不要动）

| 能力 | 位置 | 说明 |
|---|---|---|
| 对外 OpenAI 兼容端点 | `src-tauri/src/api_server/server.rs:74` | `POST /v1/chat/completions` |
| 账号池 + 调度器 | `src-tauri/src/api_server/pool.rs` | `ApiPool`：`pick_excluding` 按剩余积分挑选、跳过零积分、错误冷却 `note_error`、`MAX_ROTATE` 轮换 |
| body 改写 | `src-tauri/src/api_server/payload.rs` | `prepare_llm_chat_body` |
| SSE→OpenAI 转换 | `src-tauri/src/api_server/sse.rs` | 已有 `stream_convert` / `aggregate` |
| 余额刷新 | `src-tauri/src/commands/accounts.rs` | `refresh_remaining_credits`，调用 `api.trae.cn/trae/api/v2/pay/ide_user_ent_usage` |
| 上游常量 | `src-tauri/src/api_server/mod.rs` | `AGENT_HOST`、`EP_LLM_CHAT`、`FUNCTION="solo_work_lite"` |

**缺口（本章重点）**：上述壳的上游是 `EP_LLM_CHAT`（IDE 积分 208），余额也是 IDE 积分。
Work 积分（209）从「上游端点、请求体构造、余额来源」三个层面都**没有接入**。

---

## 2. 关键约束：为什么 Work 积分外部无法复刻

来自本会话抓包与复刻实验的实证：

- **实证成功**：真实 Trae SOLO 客户端发起 `create_agent_task`，返回 `200 OK` + `text/event-stream`
  + `event:task_created` + `event:model_config`，确认**成功消耗 Work 积分**。
- **请求体特征**：
  - 由原生层 `ai_agent.dll` 构造，是 ~123KB 的**富上下文** body（含 workspace / agent 上下文）；
  - 由闭源 `@aha-kit` NAPI 加密（仅暴露 `init` / `rawFetch`，无独立 `encrypt` 函数）；
  - body 加密后无法直读，因此外部无法"照抄"结构。
- **复刻实验（已证伪外部复刻）**：用**真实身份**（真实 JWT `data.id` + 真实 `device_id` + 真实 `machine_id` + 真实 `project_id`）
  在 Node 端复刻 `create_agent_task` 请求 → 仍返回 `4001 failed to get summary template data`。
- **结论**：`create_agent_task` 必须由**实时 Trae SOLO 会话**自身发起，外部无法合成该调用。
  任何"独立构造明文/密文 body 直接打 API"的思路都走不通（也伴随闭源组件逆向的合规风险）。

> 顺带说明：`@aha-kit` 的 `fetch` 在 `x-bridge-transport:aha` 下确实会加密 body，但它走的是 **TTNet 隧道**
> （MITM 只能看到 `CONNECT` 中继，看不到内部请求）；而真实客户端是**直连 HTTPS POST + aha 加密体**。
> 两条路径不同，进一步说明不能绕开真实客户端。

---

## 3. 可行方案对比

| 方案 | 做法 | 代码改动 | 复用现有壳 | 优点 | 风险 / 缺点 | 契合度 |
|---|---|---|---|---|---|---|
| **A. 多活会话编排 + WorkTransport（推荐）** | 每账号一个实时 Trae SOLO 实例；新增模块按池选账号→驱动该实例发起 `create_agent_task`→捕获 SSE→`sse.rs` 转 OpenAI | 小（1 个新模块 + 配置开关 + 余额来源） | 是（端点/池/转换全复用） | 唯一能真消耗 Work 积分且对外部无感；增量、不动架构 | 运维重（N 实例 + 驱动方式待确认） | ★★★★★ |
| **B. 复用 IDE 池当过渡** | 把现有 IDE 积分池直接当"资源池"对外，零改动先跑通端到端形态 | 零 | 是 | 立刻可用，验证"透明池 + OpenAI 接口 + 外部无感"形态 | 消耗的是 IDE 积分不是 Work 积分，**不满足业务诉求** | ★★☆（仅过渡） |
| **C. 逆向原生层复刻 `create_agent_task`** | 完全复刻 body 构造 + 加密 | 极大 | 部分 | 理论最"干净" | 已证伪（4001）；闭源逆向 + ToS 风险；长周期 | ✗ 放弃 |
| **D. 纯 MITM 中继（只观察）** | Work Assistant 作系统代理只捕获 Trae SOLO 自己发的请求 | 中 | 部分 | 调试/分析价值高 | **只能观察、不能发起**，无法独立服务外部用户 | ✗ 不作主方案 |

---

## 4. 推荐落地路径（分阶段，最小代码）

### Phase 0 — 先跑通端到端形态（方案 B，零改动，立即可用）
- 直接把现有 IDE 积分池对外作为 OpenAI 接口。
- 目的：验证"透明池 + 调度 + 外部无感"这一形态，沉淀监控/轮换/冷却机制。
- 明确标注：此阶段消耗 IDE 积分，仅为形态验证。

### Phase 1 — 突破 Work 积分（方案 A，增量，不动现有架构）
在 Phase 0 的壳之上，**只新增**，不修改：

1. **新增模块 `src-tauri/src/api_server/work_transport.rs`**
   - 持有每个 Work 账号的「会话句柄」（如何驱动该账号的 Trae SOLO 实例，见 §5 未决项）。
   - 提供 `trigger_create_agent_task(account, prompt) -> SSE reader`：让对应实时会话发起 `create_agent_task`，
     返回原生 SSE，交给现有 `sse.rs` 转 OpenAI。
   - 复用 `pool.rs` 的挑选/冷却逻辑（账号池本身通用，与"积分类型"解耦）。

2. **新增 Work 余额来源（additive 命令）**
   - `commands/accounts.rs` 增加 `fetch_remaining_work_credits`（或独立小模块）。
   - 调查并接入 **Work 积分余额 API**（与 IDE 的 `ide_user_ent_usage` 不同，需单独确认端点/字段）。
   - 写回 `remaining_work_credits.json`（类比现有 `remaining_credits.json`）。

3. **`routes.rs` 加 ~10 行开关（行为默认不变）**
   ```rust
   // 配置项 credit_type: "ide" | "work"，默认 "ide"
   if state.credit_type == "work" {
       work_transport::trigger(...)   // 替代 make_upstream_request
   } else {
       make_upstream_request(...)      // 现有 IDE 路径，完全不动
   }
   ```
   默认 `ide` → 现有行为一字未改；切到 `work` 才走新通道。

4. **`mod.rs` 仅新增常量（不删不改现有）**
   - `pub const EP_WORK_TASK: &str = "/api/agent/v3/create_agent_task";`（参考/备用，真发起在客户端侧）。

5. **以下文件一律不动**：`server.rs`、`pool.rs`、`sse.rs`、`payload.rs`。

### 运维模型（真正的投入点，不是代码）
- 每个参与 Work 池的账号，跑一个**独立 Trae SOLO 实例**（隔离 `--user-data-dir`，登录该账号）。
- Work Assistant 作为**编排层**：池选中账号 → 驱动对应实例 → 其原生层执行 `create_agent_task`（消耗该账号 Work 积分）
  → 响应经现有 MITM 代理捕获 → `sse.rs` 转 OpenAI → 对外统一流式返回。
- 外部调用方始终只看到 `POST /v1/chat/completions` 一个端点，**对"积分来自哪个账号 / 哪家 Work 池"完全无感**。

---

## 5. 关键未决 & 下一步验证（决定 Phase 1 能否轻量落地）

1. **如何"驱动"实时 Trae SOLO 会话发起 `create_agent_task`？**（方案 A 的核心）
   - 优先：确认 Trae SOLO 是否暴露**本地命令 / IPC / 扩展 API**（最稳，代码最少）。
   - 退化：headless / UI 自动化（脆弱，仅兜底）。
2. **Work 积分余额 API 来源**：与 IDE 不同，需新查（端点/字段/鉴权）。
3. **单实例多账号是否可行**：若 `create_agent_task` 强绑定登录会话，则必须 N 实例；
   若能进程内切换账号，则可降为单实例多会话（大幅降低运维成本）。需逆向/实验确认。

---

## 6. 风险与合规

- **ToS / 合规**：驱动真实客户端批量消耗 Work 积分可能触及 Trae 使用条款，上线前需评估。
- **运维稳定性**：N 个实时会话的资源占用、登录态维护、崩溃恢复。
  - 好消息：`pool.rs` 已有 `SessionDead` 冷却机制，会话挂掉可自动 `disabled`，天然适配。
- **驱动方式脆弱性**：若只能 UI 自动化，会话界面变化会导致失败，需监控+告警。

---

## 7. 一句话总结

> 现有"透明池 + OpenAI 接口"是 **IDE 积分专用**；Work 积分需增量突破。
> 唯一可行且对外部无感的路径是 **多活会话编排（方案 A）**：在现有池/端点/转换之上 **+1 个 `work_transport` 模块 + 一个配置开关**，
> **不动任何现有架构**。真正的投入在运维（实时会话），不在代码量。
> 建议 **Phase 0 先用 IDE 池跑通形态，Phase 1 再上 WorkTransport 突破 Work 积分**。
