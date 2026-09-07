# 开源生态深挖调研：WorkBuddy / CodeBuddy / Trae / 豆包 可扩展能力归纳

> **文档版本**: 2026-09-06 · 调研分支 `feat/traecode_doubao`
> **调研方式**: 6 个开源仓库全量浅克隆 + 源码级精读（非 README 转述），所有结论均标注来源仓库与文件路径
> **用途**: 为本项目（AI Work 助手）后续扩展——API 暴露、DeepSeek Harness 接入、签到、账号管理/切换、会话保存续期、积分管理——提供可落地的参考实现索引

---

## 0. 来源清单与定位

| # | 仓库 | 语言/形态 | 定位 | 对本项目价值 |
|---|---|---|---|---|
| 1 | [Sliverkiss/workbuddy2api](https://github.com/Sliverkiss/workbuddy2api) | Go + Docker | WorkBuddy OpenAI 兼容反向代理：OAuth 登录、多账号轮转、签到调度 | ★★★★★ API 暴露/账号池的权威参考 |
| 2 | [corrinehu/dsh-workbuddy-connect](https://github.com/corinnehu/dsh-workbuddy-connect) | TypeScript (DSH 插件) | 把 WorkBuddy 模型接入 DeepSeek Harness（DSH） | ★★★★★ Harness 接入唯一开源实现 |
| 3 | [changexbc/workbuddy-switch](https://github.com/changexbc/workbuddy-switch) | Rust (Tauri) + npm | 账号切换/会话复制/Token 保活/积分到期轮换/官方用量统计 | ★★★★★ 账号管理与会话数据模型 |
| 4 | [qinchangxv/antigravity-tools](https://github.com/hailinzhao/antigravity-tools)（同项目，发布于 qinchangxv） | Python (PySide) | WorkBuddy/CodeBuddy 批量签到、`ck_xxx` API Key 代理池 | ★★★★ API Key 模式 + 代理池设计 |
| 5 | [88lin/workbuddy-auto-signin](https://github.com/88lin/workbuddy-auto-signin) | Python 单文件 | 签到 + **成长中心**自动化（Buddy 旅行/盲盒/任务） | ★★★★ 成长中心端点独家 |
| 6 | [GitOfUser/workbuddy-checkin](https://github.com/GitOfUser/workbuddy-checkin) | PS/Python/Shell | UI 坐标点击模拟签到（无 API） | ★★ 仅作兜底思路（无 API 时的最后手段） |

克隆件位置：`%TEMP%\oss-research\<repo>`（含完整源码，可随时查阅）。

---

## 1. 跨仓库交叉验证的 API 全集（按主题归纳）

### 1.1 对话上游（★ API 暴露的核心，三仓库一致）

**来源**: workbuddy2api `internal/upstream/client.go:138-283`；dsh-workbuddy-connect `src/upstream.ts:110-112,432-483`（后者注明"wire behavior ported from Sliverkiss/workbuddy2api"）；antigravity-tools `src/modules/api_client.py:37-48`

| 用途 | 端点 | 说明 |
|---|---|---|
| 对话补全 | `POST https://copilot.tencent.com/v2/chat/completions` | **CN 区对话上游**（非 codebuddy.cn！），上游只回 SSE，非流式需本地聚合 |
| 模型目录 | `GET {chatBase}/console/enterprises/personal/models` | 返回模型 id/名称/上下文窗口/图片能力/思考强度（low/medium/high/xhigh/max）/积分倍率/促销徽章 |
| Token 刷新 | `POST {chatBase}/v2/plugin/auth/token/refresh` | `X-Refresh-Token` + `X-Auth-Refresh-Source: workbuddy` 头 |
| 积分资源 | `POST {billingBase}/v2/billing/meter/get-user-resource` | `ProductCode: p_tcaca`，响应 `data.Response.Data.Accounts[]` |
| 支付类型 | `POST /v2/billing/meter/get-payment-type` | 仅 antigravity-tools 使用 |
| 用量通知 | `POST /v2/billing/meter/get-dosage-notify` | 仅 antigravity-tools 使用 |
| 官方请求用量 | `POST https://www.workbuddy.cn/billing/meter/get-user-request-usage` | 返回 `usageToday/usage7Days/usageThisMonth`（workbuddy-switch `official_usage.rs:19`） |
| 活动横幅 | `POST /v2/activity/banner` | 仅 antigravity-tools 使用 |

**域名区域路由**（workbuddy2api `auth.go:37-48` + dsh `upstream.ts:249-257`）：
- CN 区（`domain` 不含 `.workbuddy.ai`）：chat = `copilot.tencent.com`，billing = `www.codebuddy.cn`（dsh 与 antigravity 用 copilot.tencent.com 做 billing 也通，两域均可实测）。
- Global 区（`domain` 含 `.workbuddy.ai`）：全部走 `www.workbuddy.ai`。

**对话请求头**（dsh `upstream.ts:271-284`，官方 CLI 同款）：
```
Authorization: Bearer <accessToken>
User-Agent: CLI/2.63.2 CodeBuddy/2.63.2      ← 伪装官方 CLI
X-Requested-With: XMLHttpRequest
X-User-Id: <uid>        （无 uid 时用 X-No-User-Id: 1）
X-Enterprise-Id: <eid>  （无则 X-No-Enterprise-Id: 1）
X-Domain: <domain>      （无则 X-No-Department-Info: 1）
X-Product: SaaS
```

**错误分类**（dsh `upstream.ts:118-131` 硬编码标记词）：积分耗尽类（`积分不足/额度不足/insufficient credit/quota exceeded`…）→ `hard_credit`（触发换号/冷却）；限频类 → `soft_rate`；401/403 → `session_dead`（触发刷新重试）。

### 1.2 认证、续期与多端共存

**来源**: dsh-workbuddy-connect `src/auth.ts`（全文）；workbuddy2api `internal/auth/auth.go`

- **双源凭证共存策略**（auth.ts 模块注释，最优雅的设计）：桌面 App 的 `workbuddy-desktop.info` **只读**；插件在 `$DSH_HOME/.workbuddy-auth.json` 维护自己的副本（带 `version` 字段，拒绝未知格式）；**生效凭证 = 两者中 `expiresAtMs` 更晚者**——任何一方刷新都胜出，互不覆盖。避免"工具写桌面文件被 App 重写冲突"（本项目 workbuddy-switch-plan §4 的痛点）。
- 刷新落盘用**原子写**（`@deepseek-ai/dsh-atomic-write` 的 `writeFileAtomic` + `withFileLock` 文件锁）。
- **Keycloak 原生端点确认可用**（antigravity-tools `api_client.py:57`）：`KEYCLOAK_TOKEN_URL = https://www.codebuddy.cn/auth/realms/copilot/protocol/openid-connect/token`——印证 workbuddy-switch-plan §2.3 的备选路径真实存在。
- **跨平台/WSL 凭证路径候选**（auth.ts:80-100）：Windows `%LOCALAPPDATA%`、macOS `~/Library/Application Support`、Linux `~/.config` + `~/.workbuddy/auth/` 兜底 + WSL `/mnt/c` 映射 + `WORKBUDDY_AUTH_FILE` 环境变量覆盖——可直接抄进我们的 `app_locate`。

### 1.3 签到与成长中心（88lin 独家端点）

**来源**: workbuddy-auto-signin `signin.py:4-5,177-350`；workbuddy-switch `modules/checkin.rs`；antigravity-tools `modules/checkin.py`

签到主流程（与 workbuddy-switch-plan §2.5 一致）：`POST /v2/billing/meter/checkin-activity-status` → 未签则 `POST /v2/billing/meter/daily-checkin`；88lin 实测 `DEFAULT_ENDPOINT = https://copilot.tencent.com` 同样可用。

**成长中心自动化**（`/v2/activity/growth/*`，全部已实测）：

| 操作 | 端点 | 说明 |
|---|---|---|
| Buddy 旅行状态 | `GET /v2/activity/growth/buddy/travel/status` | `state`: arrived/traveling/idle + `record_id` |
| 领旅行礼物 | `POST .../buddy/travel/claim` `{record_id}` | 返回 `reward_credit` |
| 旅行目的地配置 | `GET .../buddy/travel/config` | `locations[]`，取第一个 |
| 派 Buddy 出发 | `POST .../buddy/travel/depart` `{location_id}` | 返回目的地/时长 |
| 盲盒次数 | `GET .../lottery/chances` | `balance` |
| 开盲盒 | `POST .../lottery/draw` `{}` | 返回 `prize_name` |
| 任务列表 | `GET .../tasks` | `tasks[]`（progress/accept_status/has_reward/reward_credit/reward_energy） |
| 领任务奖 | `POST .../tasks/accept` `{task_code}` | |
| 能量余额 | `GET .../energy` | `balance` |
| 连签天数 | `GET .../streak` | `streak.days` |

额外经验（signin.py）：兼容"已签"两种返回形态；识别 401/403 登录态过期（报 NO_SESSION）；识别非签到季；签到包名含"运营裂变包"用于统计连签积分（antigravity `checkin.py:93`）。

### 1.4 会话数据模型与复制（workbuddy-switch 独家）

**来源**: workbuddy-switch `crates/wb-switch-core/src/modules/session.rs:1-15`

**WorkBuddy 5.x 会话三件套（缺一不可）**——这是"会话保存/迁移/复制"的完整数据模型：
1. **正文**: `~/.workbuddy/projects/{workspace}/{cid}.jsonl`（JSONL，每行含 `sessionId` 字段）
2. **元数据**: `~/.workbuddy/workbuddy.db` 的 `sessions` 表（id = conversation id = UUID）
3. **云端映射**: `~/.workbuddy/edge-sync-mapping-v2.db` 的 `edge_sync_mapping` 表（`session_id`=conversation_id，`msg_channel=convmsg:{uid}` **决定云端归属**）

**会话复制算法**（路径 B：生成新 id，云端可正常同步）：读源账号 jsonl → 替换 sessionId 为新 UUID → 写入目标账号 projects 目录 → 在 workbuddy.db sessions 表插入新行 → 在 edge_sync_mapping 注册 `convmsg:{目标uid}` 映射。复制前 `backup_workbuddy_db`。另有 `export_import.rs`（会话导出/导入）。

### 1.5 CodeBuddy CLI 账号体系（workbuddy-switch 独家）

**来源**: workbuddy-switch `modules/codebuddy_cli.rs`、`rotate.rs`

- **CLI 切号**：Windows 直接维护 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN`（绕过 `apiKeyHelper` 的 Windows 路径坑）；macOS/Linux 用 `~/.codebuddy-rotate/helper.cjs`（Node shebang 脚本，`include_str!` 内嵌 STANDARD_HELPER）。与 WorkBuddy App 复用同一账号库但**当前账号独立**；不修改运行中会话。
- **自动轮换**（防积分过期浪费，rotate.rs）：周期检查各账号积分到期时间 → 切到"最早到期且仍有剩余"的账号。**防抖动两约束**：① 冷却期（切换后 cooldown_minutes 内不再切）；② 到期差异阈值（目标比当前早到期超过 min_gap_hours 才切，避免三个账号都明天到期时来回横跳）。状态落 `~/.codebuddy-rotate/state.json`。

### 1.6 账号池调度（workbuddy2api 权威实现）

**来源**: workbuddy2api `internal/pool/pool.go`、`internal/scheduler/scheduler.go`、`README.md`

- **三因子加权随机选号**：`credits 占比×10 + 闲置时长补偿（每小时+权重，封顶） + 成功率×3`（成功率 = successCount/(successCount+errTotal)，累计不清零）→ 先取 Top5 短名单，再在名单内按同权重加权随机——防热点 + 防惊群（100ms 窗口去重）。
- **熔断器**：错误阈值 + 冷却（冷却时长指数递增至 cooldownMax）；`hard_credit` 冷却到次日，积分耗尽账号由调度器 04:00 自动恢复探测。
- **定时器**：每日 09:00/21:00 双时段签到 + 积分查询；保活刷新独立开关。
- **对外接口**：`/v1/chat/completions`（流式透传/非流式聚合）、`/v1/models`、`/status`（total/healthy/cooling/disabled + 每账号画像）、`/healthz`（无健康账号 503，可接 LB）、请求级日志（seq/TTFB/uid/tokens/latency）。
- **子 Key 体系**（antigravity-tools `proxy_server.py:103-157`，2143 行）：上游真实 Key（sk-xxx/ck_xxx）与对外分发的子 Key 分离，子 Key 可限定可用上游、调用模式二选一（专一模式=用完再换 / 临期优先=先用最快过期的），按日统计 token 与 credits 消耗，健康检测带时间戳。

---

## 2. DeepSeek Harness（DSH）接入机制（dsh-workbuddy-connect 独家）

**来源**: `src/adapter.ts`、`src/index.ts`、`src/shim.ts`、`src/host-heartbeat.ts`、`src/loopback.ts`、`src/catalog.ts`

- **接入原理**：DSH 的插件机制暴露 `dsh-llm-pi-ai` 扩展点（`createProvider` / `openAICompletionsApi`）。本插件注册一个名为 `workbuddy` 的 pi-ai provider，把 WorkBuddy 的模型目录映射进 DSH 模型选择器——**WorkBuddy 上游本身就是 OpenAI 兼容协议**（`/v2/chat/completions`），适配层很薄。
- **模型元数据透传**：上下文窗口/maxTokens/`supportsImages`（缺失按 false，宁缺勿滥——避免发图后被上游拒绝在消息已持久化之后）/思考强度（`supportedEfforts` 映射到 pi-ai thinking levels）/积分倍率归一化为 `x0.79` 无语言形式/促销徽章 `badge:限时免费:#FF0000` 解析。
- **每次 DSH 启动同步目录**：徽章/倍率以服务端为准（catalog.ts）。
- **版本强对应**：插件 0.3.0+ 要求 DSH 核心 `0.1.2-rc.1+`；安装：`dsh plugin --profile web|desktop|dsh-tui add dsh-workbuddy-connect`。
- **loopback 架构**（loopback.ts + shim.ts）：provider 经本地回环 shim 转发到上游，shim 负责错误分类到不同 HTTP 答案 + 流超时（idle 300s）。
- **心跳**（host-heartbeat.ts）：与 DSH host 保活，避免长流被回收。
- **对本项目的意义**：若 AI Work 助手 要做"Harness 接入"，最短路径是**复用本项目现有 API 网关**（已是 OpenAI 兼容）+ 参照 dsh 的 catalog/adapter 元数据映射写一个 DSH provider；或直接引导用户安装 dsh-workbuddy-connect 连我们的网关。

---

## 3. 对本项目的扩展路线建议（按优先级）

| 优先级 | 扩展项 | 参考 | 说明 |
|---|---|---|---|
| P0 | **API 暴露升级**：现 API 网关 + workbuddy 账号池 | workbuddy2api 全套 | 上游改 `copilot.tencent.com/v2/chat/completions`；抄三因子加权随机 + 熔断冷却 + `/status`/`/healthz` + UA/X-No-* 头规范；非流式本地聚合 |
| P0 | **签到增强：成长中心** | 88lin signin.py | 旅行/盲盒/任务三自动化，纯增量积分 |
| P1 | **会话复制/迁移** | workbuddy-switch session.rs | 三件套数据模型 + 新 id 复制算法；也可先做会话备份/恢复 |
| P1 | **Token 保活双源化** | dsh auth.ts | 工具侧凭证副本 + "谁新用谁"，避免与桌面 App 写冲突 |
| P1 | **积分到期轮换** | workbuddy-switch rotate.rs | CLI 切号桥 + 冷却/差异阈值防抖 |
| P2 | **官方用量统计** | workbuddy-switch official_usage.rs | `get-user-request-usage` 拉日/周/月用量 + 本地 jsonl token 统计（token_stats.rs 遍历 projects/*.jsonl） |
| P2 | **DSH/Harness 接入** | dsh-workbuddy-connect | 网关就绪后做 provider 元数据映射；或文档引导装 dsh 插件 |
| P2 | **API Key（ck_xxx）模式** | antigravity-tools | `ck_xxx` 可直接调 /v2/billing/meter/*，账号导入门槛更低 |
| P3 | **Global 区支持** | workbuddy2api auth.go | domain 含 `.workbuddy.ai` 走 `www.workbuddy.ai` |
| P3 | **豆包/Trae 延伸** | —— | 本批仓库未覆盖豆包/Trae CN；沿用 doubao-trae-switch-plan.md，Trae 侧可借鉴本批的"到期轮换+三因子选号"思路 |

## 4. 风险与注意事项

1. **协议非公开**：以上全部端点为逆向/实测所得（多仓库、多语言、多作者交叉一致，可信度高），但腾讯可随时变更——实现时保持 workbuddy-switch-plan §4 的"接口层独立 + 失败明示"策略。
2. **UA 伪装**：对话上游校验 `User-Agent: CLI/x CodeBuddy/x` 形态（workbuddy2api/dsh 均硬编码 `CLI/2.63.2 CodeBuddy/2.63.2`），版本号升级需跟踪。
3. **copilot.tencent.com 域名勘误**：此前 workbuddy-switch-plan §1.3 记录"copilot.tencent.com 404"——本批实测它就是 CN 区对话与签到主域（workbuddy2api/88lin/antigravity 三源一致），特此更正；实现时保留 codebuddy.cn 双域探测即可。
4. **合规**：所有仓库均声明仅供本人账号管理。workbuddy2api 的多账号池用于 API 服务时注意不要演变为对外售卖转租。
5. **许可证**：6 仓库均为 MIT，可合规借鉴代码（保留版权声明）。

---

## 5. 二次深挖补充（第二轮精读，未覆盖文件）

### 5.1 workbuddy2api 补充（OAuth 工具 / 请求改写 / 会话粘性）

- **OAuth 设备流细节**（`cmd/login/main.go`）：`POST copilot.tencent.com/v2/plugin/auth/state?platform=CLI`——注意 **platform=CLI**（dsh 插件用 `platform=workbuddy`，两种都有效）；**无 PKCE**（state 由服务端签发）；每个登录流程独立 cookie jar（多账号互不串会话）；state 落盘后 `login poll` 子命令轮询 `auth/token?state=`，再 `login/account?state=` 取 uid/nickname；Origin/Referer = `https://www.codebuddy.cn`。
- **请求头三条铁律**（`internal/upstream/headers.go`）：
  1. `Origin` + `Referer` 必须带（CN=codebuddy.cn / Global=workbuddy.ai，按账号 region 切换）；
  2. 缺省字段有显式 `X-No-*` 占位（`X-No-Authorization/X-No-User-Id/X-No-Enterprise-Id/X-No-Department-Info: 1`）；
  3. **安全红线：chat 请求绝不携带 `X-Refresh-Token`**（只允许出现在 refresh 端点，配 `X-Auth-Refresh-Source: workbuddy`）。
- **请求体改写**（`internal/upstream/payload.go`）：① 上游**拒绝非流式** → 强制 `stream:true`（非流式由本地聚合）；② `tool_choice` 必须是 **string**（对象形式报 400 `code=11101`，需归一化：`{"type":"function","function":{"name":X}}` → `"X"`）；③ `reasoning_effort` 按模型 `supportedEfforts` 自动降级（档位序 off<minimal<low<medium<high<xhigh<max，snake/camel 双字段兼容）。
- **指纹清洗**（`internal/upstream/sanitize.go`，可开关 `sanitize_blacklist_fingerprints`）：剥离消息中的 Claude Code 痕迹——身份句改写（"official CLI for"→"official CLI tool for"）、"Main branch"→"Default branch"、`cc_xxx=...;` 键值对循环剥离、`X-Anthropic-*` 头引用剥离；零分配预检（先 `strings.Contains` 快路径再正则兜底）。
- **SSE 处理**（`internal/upstream/sse.go`）：非流式 `Aggregate`（tool_calls delta 按 index 合并、帧归一化）+ 流式 `Stream` 透传。
- **会话粘性路由**（`internal/session/session.go`）：conversationId/metadata 键 → 账号绑定；**双段分配**（优先"空闲账号"哈希，其次全池哈希）；写锁 re-check 防 TOCTOU；TTL 30m + LastActive 滚动续期 + redis 异步镜像防重启丢粘性。
- **完整配置 schema**（`config.example.json`，权威）：`pool.max_in_flight=3`、`breaker_threshold=3`、`breaker_cooldown=30m`（递增至 `6h`）、`idle_weight_per_hour=0.5/max=5.0`、`session_sticky.ttl=30m`、`cooldown.soft_rate=60s`、`schedule.checkin_hours=[9,21]`、`keepalive_hours=[22]`、upstash redis 可选。

### 5.2 dsh-workbuddy-connect 补充（模型目录 / 图片模态 / 注册机制）

- **静态模型目录 15 个**（`src/catalog.ts`，2026-09-01 与线上核验，可作离线兜底/参考值）：

| id | 上下文 | maxTokens | 图片 | 思考档位 | 倍率 |
|---|---|---|---|---|---|
| auto | 168K | 32K | ✅ | 默认 high，不可关 | — |
| hy3 / hy4-preview | 192K / 1M | 64K | ✅ | high（hy4 仅 high） | **x0.00 限时免费** |
| glm-5.3 / -flash | 1M | 48K / 32K | ✅ | low/high/xhigh；flash low/high/max | x0.79 / **x0.06** |
| glm-5.2 | 1M | 48K | ✅ | medium | x0.79 夜间折扣 |
| glm-5.1 | 200K | 48K | ❌ | medium | x0.79 |
| glm-5v-turbo | 200K | 64K | ✅ | medium | x0.71 |
| kimi-k3-1 | 1M | 32K | ✅ | medium | x1.62 |
| kimi-k2.7-code / k2.6 | 256K | 32K | ✅ | medium | x0.57 / x0.52 |
| minimax-m3 | 512K | 128K | ✅ | medium | x0.25 |
| deepseek-v4-flash / -pro | 1M | 50K | ✅ | high | x0.17 / x0.51 |
| hy3-x | 192K | 64K | ✅ | low/high | x0.05 |

  目录结构分新旧两代：旧代 `{effort, summary}`（无 supportedEfforts，多数拒绝 off）；新代含 `supportedEfforts` + `canDisableThinking`。**设计模式：静态兜底目录 + 启动后动态替换**（首拉在途/离线时 provider 仍可用）。
- **图片模态案例研究**（`docs/image-modality-gap.md`，完整排障记录）："渠道不支持图片" **100% 是宿主本地拦截**（DSH host `dsh-host-apiproxy` 经 `resolveModelInfo` 检查 `inputModalities`，不含 image 直接拒，消息不落库不发出）；上游模型目录本就带 `inputModalities:["text","image"]` 字段。修复 = 读上游字段而非硬编码。**教训：能力声明永远以上游目录为准**。
- **插件注册**（`cordis.patch.yml`）：DSH 用 cordis patch 声明式注册 provider（`insert: llm-workbuddy`），不改动用户当前默认模型。

### 5.3 workbuddy-switch 补充（进程管理 / token 统计 / 快照体系）

- **进程管理**（`modules/process.rs`）：Windows 关闭 = 枚举进程行（映像名匹配 `WorkBuddy.exe`/`CodeBuddy.exe` + **crashpad helper 识别 + 自身排除**）→ `taskkill /PID x /T`（树杀，宽限 8s）→ 残留 `/F` 强杀 → 仍存活则报错让人工介入；启动 = 认证文件里的 app 路径（`persist_workbuddy_exe` 持久化上次路径兜底）+ `CREATE_NO_WINDOW` 静默拉起。切换主流程（`switch.rs`）：备份 → close(20s) → 写 auth 文件 → launch，全程进度回调。
- **本地 token 统计契约**（`.trellis/spec/wb-switch-core/backend/token-statistics.md`）：数据源 = `~/.workbuddy/projects` + `~/.codebuddy/projects` 的 **JSONL 事件流**，解码出 input/output/cacheRead/cacheWrite/uncachedInput/records/cacheHitRate，按模型/项目/会话三维聚合，`days ∈ {7,30,90}`，Tauri 与 HTTP（`GET /api/token-stats`）同构双暴露。
- **积分用量快照**（`credit_usage.rs`）：本地快照（total/remaining 时序）+ 签到日志 → 推导每日用量窗口，官方用量不可用时的回退数据源。
- **账号库导入导出**（`export_import.rs`）：JSON 格式 preview/merge（按 token 去重追加/更新）/按索引导入，纯逻辑无文件系统依赖便于单测。

### 5.4 antigravity-tools 补充（旧版登录态考古 / 代理协议细则）

- **WorkBuddy 旧版登录态考古**（`src/modules/oauth.py`）：客户端曾是 **VSCode fork（Electron）**，旧版凭证在 `%APPDATA%/WorkBuddy/User/globalStorage/state.vscdb`——AccessToken 用 **Chromium v10 加密**（`Local State → os_crypt.encrypted_key`（DPAPI）→ AES-256-GCM 解密：去 3 字节 `v10` 前缀 + 12B nonce），另有 `.neodata_token` 兼容回退。**16+ 项认证残留清理清单**（`_clear_all_auth`）：vscdb 各认证 key、`Tencent-Cloud.coding-copilot` 产品缓存、`__$__targetStorageMarker`、vscdb.backup 等——做"彻底登出/环境重置"时的完整 checklist。Keycloak 登出 = `{issuer}/protocol/openid-connect/logout`。
- **代理调度协议细则**（`docs/proxy_optimization_design.md`，17 项优化的工程协议）：
  - **错误分类三态**：`RETRY_SAME`（502/503/超时→同 Key 重试 1 次）/ `SWITCH_KEY`（401/403/429→直接换）/ `FATAL`（400 context_too_long→终止）；
  - **模型级冷却**：按 model 记冷却（渐进退避 10→20→40s），优先级高于 Key 级状态；
  - **流式保活**：15s 一次 SSE 注释行 `: keep-alive\n\n`；首字超时 10s（可故障转移）；空闲 60s 主动断；
  - **断连兜底**：`_drain_upstream` 客户端断开后继续读完上游（保 usage 统计完整）；
  - **健康检测**：5min + 0-60s 随机抖动，发 `/v1/models` 轻量请求；
  - **sticky session 提取优先级**：`X-Session-ID` 头 > messages cache_control hash > 内容摘要；key_mode=4 = 粘性会话模式；
  - 非流式响应 50MB 上限；`MODEL_CONTEXT_LENGTHS["auto"]=168000` 与目录一致。

### 5.5 workbuddy-auto-signin 补充

- 鉴权头极简版可用：`User-Agent: WorkBuddy`（桌面端 UA）+ Bearer + X-User-Id（+X-Enterprise-Id/X-Tenant-Id/X-Domain）即可调 billing/activity 接口（`signin.py:62-77`）。
- `dig()` 信封解包工具：字段可能被 `data/result/resp/response` 任意一层包裹，递归查找——解析层的通用兜底模式。

### 5.6 二次深挖对扩展路线的增量影响

| 扩展项 | 增量结论 |
|---|---|
| API 暴露（P0） | 增加硬约束：Origin/Referer、tool_choice string 化、强制 stream、effort 降级、指纹清洗（可选）；粘性会话用 workbuddy2api 的双段分配方案 |
| DSH 接入（P2） | 有现成 15 模型静态目录可当兜底；能力声明读上游 `inputModalities/supportedEfforts`，勿硬编码 |
| 环境重置（新增） | antigravity 的 16 项残留清理清单 + state.vscdb v10 解密路径，可用于"彻底登出/多账号隔离"功能 |
| 切号流程 | 关进程用"树杀+宽限+强杀"三级，映像名匹配须排除 crashpad helper 与工具自身 |

## 6. 第三轮调研：Trae Work 与 DSH/Codex 的接入现状（2026-09-07）

> 调研问题：① Trae Work 支不支持 DSH？② Trae Work 支不支持 Codex？③ WorkBuddy 支不支持 Codex？
> 方法：网络检索 + 已克隆仓库交叉引用 + 本机实测（WorkBuddy app.asar 字符串分析、Codex CLI 本机配置取证）。

### 6.1 Trae Work ↔ DSH：已有成熟社区插件，双积分体系都被覆盖

**结论：Trae 官方不原生支持 DSH，但社区插件已把"本机登录的 Trae 模型"完整接入 DSH，且能读取 Work 积分。**

- **`dingminhua/dsh-connect-trae`**（MIT，npm v1.2.0，[awesome-dsh-plugin 收录](https://awesome-dsh-plugin.com/p/dingminhua/dsh-connect-trae)）：
  - 把本机登录的 Trae CN / **TRAE SOLO CN** 模型注册为 DSH 的 `trae` provider（DeepSeek-V4-Flash/Pro 等）；
  - **DSH 本地工具循环**：Trae 只生成结构化 `tool_calls`，bash/read/write 由 DSH 本地执行再回传——绕开了 Trae 上游不执行工具的限制；
  - **多账号切换**：自动发现 `%APPDATA%\Trae CN` / `%APPDATA%\TRAE SOLO CN` 的 `User/globalStorage/storage.json` 登录账号，Token 不写入 DSH 设置；
  - **只读积分面板：Work 积分与通用积分分开显示**（走 `https://api.trae.cn/trae/api/v2/pay/*` 与 `/ug/*`，只读查询不消耗积分）——与我们项目的积分 API 结论一致（IDE 208 / Work 209 两套）；
  - 上游链路：`https://trae-api-cn.mchost.guru/api/agent/v3/llm_utils_chat` → Trae SSE/pending function_call → OpenAI SSE tool_calls → DSH；
  - 安全设计：随机端口 + 进程内随机 secret 的 loopback shim，真实 Trae token 不交给 pi-ai 层；
  - 参考实现致谢：`Wang-JQ77/dsh-trae-api`（MIT，Trae 认证/会话/模型协议研究）。
- **`@casually/dsh-trae-api`**（npm，双形态）：
  - 既可作 DSH 插件（默认 `http://localhost:9220`，端口/Key 可在 `cordis.patch.yml` 配置），也可独立运行；
  - 首启自动从本机 Trae IDE `storage.json` **解密认证数据**存入 `.env`；
  - 暴露 **OpenAI 兼容 `/v1`**，同时给出 Claude Code（`ANTHROPIC_BASE_URL`）、Cursor、Cline/Roo、Windsurf 的接入配置——**这实质上就是"Trae Work 的 API 暴露"现成方案**，与我们 P0 扩展项高度同构；
  - 模型名可任意填（如 `claude-sonnet-4-6`），由 Trae 侧 `auto` 路由。
- 对我们的意义：**Trae 侧 DSH 接入不必自研**——安装 `dsh-connect-trae` 即用；而它对 Trae 认证解密、`llm_utils_chat` 协议、积分只读接口的实现可直接参照（与我们 `docs/doubao-trae-switch-plan.md` 的 Trae 方案互为印证）。

### 6.2 Trae Work ↔ Codex：无原生支持，需经协议转换层

- Trae 上游是自有协议（`llm_utils_chat` + SSE function_call），**不提供 OpenAI Responses API**，因此 Codex CLI（新版走 `wire_api = "responses"`）**无法直连 Trae**。
- 可行路径（同 6.1 的 `@casually/dsh-trae-api` 或自建转换器）：Trae → 本地转换器补齐 `/v1/responses` 投影 → Codex CLI `config.toml` 指向 `base_url`。**截至调研时点未见现成的 "trae2codex" / Trae Responses API 转换器开源实现**——若要做，参考 `tonny0812/workbuddy2api` 的 converter（见 6.3，其 `/v1/responses` 投影逻辑可整体复用，只换上游适配层）。

### 6.3 WorkBuddy ↔ Codex：无原生集成，但两条社区链路均已跑通

**结论：WorkBuddy 桌面端本身不支持 Codex 模型（本机 `app.asar` 内 236 处 codex 命中全部来自内置 OpenAI SDK 的类型定义，如 `gpt-5.1-codex-max` 枚举，无任何 Codex 集成代码）。** 但社区双向链路都已存在：

**链路 A：WorkBuddy/CodeBuddy 订阅 → 当作 Codex CLI 的模型后端**（把腾讯订阅喂给 Codex）
- **`tonny0812/workbuddy2api`**（另见 GitHub 同名仓库，Python converter.py）：**已实现 `/v1/responses` 端点投影**，Codex CLI 直配即可用 WorkBuddy 模型（`codex-codebuddy.example.toml`）：
  - `~/.codex/config.toml`：`[model_providers.workbuddy]` + `base_url="http://127.0.0.1:8787/v1"` + `wire_api="responses"`；`[profiles.workbuddy]` model = glm-5.2 / kimi-k2.7 / deepseek-v4-pro / auto；
  - 同一转换器还内置 **Anthropic `/v1/messages` 适配层**（CC Switch 接 Claude Code）与 OpenAI 兼容 `/v1/chat/completions`（Cherry Studio 等）——一份转换器三协议；
  - 配套 `--desensitize` 脱敏（防后端审核拦截）、`--no-compact` 保留完整 system prompt、脱敏仍命中审核时自动退回紧凑模式重试。
- **`workbuddy-mcp`**（npm，[LobeHub 收录](https://lobehub.com/mcp/linhaij-workbuddy-mcp)）：反方向——把 WorkBuddy（驱动 `@tencent-ai/codebuddy-code` CLI）注册为 **Codex/Claude Code/Cursor/OpenCode 的 MCP 工具**（`codex mcp add workbuddy -- node server.js`），agent 说"用 workbuddy 做 X"即调用 `run_workbuddy_task`；`WB_SKIP_PERMISSIONS=true` 时无人值守执行。
- **WorkBuddyProxy**（社区方案，见 ima.qq.com《WorkBuddy接入Codex解决方案》及变现营课程）：Electron 本地代理，WorkBuddy 走 OpenAI 兼容接口 `127.0.0.1:<port>/v1` → 代理持 **Codex OAuth access token**（存 `%APPDATA%\WorkBuddyProxy\config.json`）→ 调 Codex 后端——即"WorkBuddy 当驾驶舱、Codex 当执行器"。

**链路 B：Codex（ChatGPT 订阅）→ 当作 DSH 的模型后端**（与 WorkBuddy 无关，但属同族）
- `franksong2702/dsh-codex-connect`（Apache-2.0，alpha 4.26）：ChatGPT OAuth 登录 → `openai-codex` 模型目录进 DSH；Fast Mode（1.5×）、5 小时/每周双配额窗口显示、可选搜索/图片工具；DSH 插件结构的参照实现（dsh-workbuddy-connect 即仿它组装 provider）。

### 6.4 本机现状取证（2026-09-07）

- **Codex CLI 已安装**：`~/AppData/Roaming/npm/codex` + `~/.codex/`（auth.json、archived_sessions、automations 等）；
- **Codex 当前走 CC Switch 本地代理**：`~/.codex/config.toml` → `base_url = "http://127.0.0.1:15721/v1"`，`wire_api="responses"`，`experimental_bearer_token="PROXY_MANAGED"`，model=`z-ai/glm-5.3-free`；15721 端口实为 **`D:/software/CC Switch\cc-switch.exe`**（PID 实测）——即用户已在用"代理托管 OAuth + 模型切换器"的模式，WorkBuddyProxy 属同类形态；
- **WorkBuddy 桌面端无 Codex 集成**（asar 字符串分析，见 6.3）。

### 6.5 对本项目的落地建议（增量）

| 方向 | 建议 | 参考 |
|---|---|---|
| Trae Work → DSH | 不自研，直接装 `dsh-connect-trae`；若要产品化多账号切换，参照其 storage.json 发现 + loopback shim 设计 | dingminhua/dsh-connect-trae |
| Trae Work API 暴露 | `@casually/dsh-trae-api` 的解密+OpenAI 兼容层即现成方案；与本项目 P0 同构，可吸收其 Anthropic 适配思路 | @casually/dsh-trae-api、Wang-JQ77/dsh-trae-api |
| WorkBuddy → Codex 后端 | 复用 `tonny0812/workbuddy2api` converter（/v1/responses 投影 + 三协议 + 脱敏），替换上游适配层即可让 Codex CLI 吃 WorkBuddy 积分 | tonny0812/workbuddy2api |
| Codex 集成进本项目 | 用户已有 CC Switch 管理多 provider；本项目可做"账号池视角"的补充——把 WorkBuddy/Trae 转换端点注册进 CC Switch 配置而非自建切换器 | 本机 config.toml 取证 |
