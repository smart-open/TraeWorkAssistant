# 技术架构设计 — AI Work 助手（Web 版）

> **版本**: v1.0.0 | **更新日期**: 2026-09-22 | **形态**: Web-only 单体（Docker 部署）
> 桌面版架构（Tauri 外壳 / src-tauri / 登录态切换器 / MITM 代理 / 豆包）已整体退役删除，历史决策与转型过程见 `docs/tmp/docker-headless-server-plan.md`（ADR-1~4）与 git 历史。本文描述**当前 Web 版**的技术选型、架构分层、数据模型与协议参考。

## 1. 技术选型

| 层 | 技术 | 说明 |
|---|---|---|
| 后端核心 | Rust 1.88 + `aiwork-core` crate | 业务核心零桌面依赖：state/models/store/tasks/api_server/oauth/notify |
| 后端服务 | Rust + `aiwork-server` crate（axum 0.7 / tokio） | 单进程：管理面 REST + OpenAI 兼容网关 + 静态托管 + 调度线程 |
| HTTP 客户端 | ureq（同步，阻塞线程池） | **直连语义**：不读系统/环境代理，容器内行为确定；合成设备指纹 |
| 存储 | rusqlite（bundled SQLite，WAL 模式） | 单文件库 `data/aiwork.sqlite`，跨平台无 OS 依赖 |
| 加密存储 | iota_stronghold | jwt/refresh_token 权威加密；主密钥来自 `AIWORK_VAULT_KEY` 或 `conf/vault_key.bin`（ADR-2，去 DPAPI） |
| 前端 | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 | 复用桌面版页面骨架，`lib/tauri.ts` 单点适配 REST+SSE |
| 部署 | Docker 多阶段构建（node:20 → rust:1.88-slim → debian:bookworm-slim） | compose 编排，HEALTHCHECK `/health` |

## 2. 架构分层

```
┌─ 浏览器（任意设备）──────────────────────────────────────────┐
│ React UI：Login → Dashboard / Accounts / Buddy / Checkin /   │
│           Credits / ApiService / Logs / Settings             │
└──────────┬──────────────────────────────────────────────────┘
           │ /api/login（token→HttpOnly cookie, ADR-4）
           │ /api/cmd/*（命令桥, cookie 守卫） + /api/events/*（SSE）
           │ /v1/*（OpenAI 兼容网关, api_keys fail-closed）
┌──────────▼──────────── aiwork-server（axum 单体）─────────────┐
│ static_files.rs   ServeDir dist/ + SPA fallback              │
│ admin::auth       /api/login + cookie 中间件（7 天会话）       │
│ admin::cmd_bridge POST /api/cmd/<command> → core impl 白名单  │
│                   分发（90+ 命令，fail-closed 未注册 404）     │
│ admin::sse        GET /api/events/checkin（签到/认证进度流）   │
│ api_server/*      /v1/* 网关（§6）+ /health /healthz /status  │
│ main.rs           启动序：AppState→迁移→token→调度线程→路由；  │
│                   SIGTERM 优雅退出（flush）；--task-run 兜底   │
├──────────────────── aiwork-core（零 tauri）───────────────────┤
│ state/store/models/fs_utils/jwt/icube_auth/checkin_results   │
│ vault（KeyProvider: env 或文件，ADR-2）                        │
│ tasks/{trae_checkin, wb_checkin, wb_common, wb_credits}      │
│ tasks/scheduler（60s tick，7 任务，§5）                       │
│ commands/*（accounts/oauth/workbuddy/checkin impl 层）        │
│ notify.rs（Bark / Server酱 / webhook 三渠道推送）              │
└──────────┬──────────────────────────────────────────────────┘
           │ ureq 直连（合成指纹，不读系统代理）
           ▼
   api.trae.cn / trae-api-cn.mchost.guru / copilot.tencent.com / www.codebuddy.cn
```

**分层要点**：

- **命令桥**：前端 `invoke(name, args)` → `POST /api/cmd/{name}`，body 顶层键 camelCase 自动转 snake_case，嵌套对象保持 snake_case；响应 `{"ok":true,"data":...}`。白名单表显式注册于 `cmd_bridge.rs`，未注册命令 404（fail-closed 安全特性）。
- **SSE**：服务端命名事件 `checkin-progress` / `checkin-done` / `wb-checkin-progress` / `wb-oauth-progress` / `wb-oauth-done`；前端 `lib/tauri.ts` 集中登记监听（别名归一），断线浏览器自动重连。
- **双鉴权域**：`/api/*` 走 admin token→cookie；`/v1/*` 走 api_keys（fail-closed）；`/health` 公开。

## 3. 数据模型

SQLite 单库（WAL），三类表（详见 `crates/aiwork-core/src/store/schema.rs`）：

| 类别 | 内容 |
|---|---|
| kv 文档表 | `app_settings` / `api_pool` / `dispatch_policy` / `api_models` / `wb_model_catalog` / `wb_model_route` / `wb_template_map` / `workbuddy_settings` / `notify_config` / `scheduler_state` 等 30+ 键 |
| 行文档实体表（pk + data JSON） | `accounts` / `device_map` / `groups`(+members) / `remaining_credits` / `account_cooldowns` / `api_keys` / `custom_models` / `wb_accounts` / `wb_tokens` / `api_usage` 等 |
| 列化流水表 | `credits_history` / `credits_daily` / `checkin_results`(90 天) / `wb_checkin_results` / `wb_credits_history`(365 天) / `usage_history_*`(365 天) / `sticky_bindings` |

- 访问约定：`store::db(data_dir)` 单连接串行（WAL + busy_timeout 5000）；`-wal/-shm` 常驻属正常。
- 配置落盘仍用 `fs_utils::write_json`（tmp + rename 原子替换）：`app_settings.json`（UI 可编辑配置）、日志、导出产物。
- 敏感红线：jwt/refresh_token 只入 Stronghold vault（`conf/vault.stronghold`），禁止明文落盘；WB 凭据入 SQLite `wb_tokens` 表。

## 4. 签到冷却与账号池调度

### 4.1 错误分类冷却状态机

| 错误类型 | 判定 | 冷却 |
|---|---|---|
| PlanLimit | 上游「额度不足」类错误码 | 12h（短期不恢复） |
| SoftRate | 频率限制 | 60s |
| SessionDead | JWT 鉴权失败（401/吊销） | 需重新 OAuth，账号标记 |
| Server / Client | 5xx / 4xx（累计触发升级） | 10 分钟 |

冷却状态持久化于 `account_cooldowns`；签到成功自动清除；前端可手动清除。

### 4.2 账号池调度（网关侧）

- 请求级取号：策略（expire_first / credit_first / random / **weighted**（三因子加权随机）/ **p2c**（随机选二取优））+ 分组过滤；自动跳过禁用 / 冷却 / 积分过期账号；单账号额度耗尽自动换号（单请求最多 3 次）。
- 五态机：Available / QuotaProtection（hard_credit 冷却至次日 04:00）/ RateLimited / Forbidden（403/SessionDead 禁用）/ ProxyDisabled。
- 熔断：连续 3 错 30m 起指数递增（×2）封顶 6h，成功重置；模型级冷却 10→20→40s 渐进退避。
- 池同步：`pool.sync_from_accounts` 从账号层同步 JWT，续期任务完成后网关自然拿到新 token。

## 5. 调度器与后台任务

**单后台线程 60s tick**（`tasks/scheduler.rs`，`start(AppState)`），触发语义 `今天已过触发时刻 && 今天未跑`，启动补跑适配容器弹性重启；失败 30 分钟冷却自动重试；任务实现全部幂等。

| 任务 | 触发 | 实现 |
|---|---|---|
| `trae-jwt-renew` | 05:30 | 遍历 vault 账号 `refresh_jwt_impl(force=false)`；48h lazy gate 内置；invalid 计入「需重新 OAuth」摘要 |
| `models-sync` | 05:40 | `models_sync::fetch_official`（vault 首个含 JWT 账号，最多试 3 个；无账号跳过不计失败） |
| `trae-checkin` | 09:00 | `trae_checkin::run_round`（vault 全账号单轮，状态核验幂等） |
| `wb-checkin` | 09:10 | `wb_checkin::run_checkin_round`（抢轮次锁与手动路径互斥；跟随「自动补签」开关） |
| `wb-renew` | 10:30 | `run_renew_only`（lazy 24h） |
| `wb-credits-snapshot` | 23:30 | WB 积分快照（近 7 日消耗差分数据源） |
| `trae-credits-snapshot` | 23:40 | `refresh_remaining_credits_impl` |

- 状态落 kv `scheduler_state`（每任务 last_run_date/last_ok/last_fail_ts/last_summary），前端 `scheduler_status` 查看。
- 任务配置：kv `scheduler_cfg`（前端 `scheduler_config_get/set`）——`disabled_tasks` 停用名单（缺省全开 = 推荐配置）、`task_times` 自定义每日触发时刻（缺省用内置默认）；`enabled` 与既有设置语义合成（`wb-checkin` 仍跟随「自动补签」开关）。
- CLI 兜底：`aiwork-server --task-run <name>` 单任务执行后退出。
- 任务执行 panic 由 `catch_unwind` 捕获，不影响后续调度。
- 通知接入：签到完成（仅签到类任务）/ 任务失败 → `notify::send`（Bark / Server酱 / webhook 三渠道，总开关默认关，单渠道失败仅记日志的旁路语义）。

## 6. API 网关（api_server 模块）

纯 axum 层，随服务常驻，`AIWORK_LISTEN_ADDR` 可配（默认容器 `0.0.0.0:8080`）。

### 6.1 三池与统一目录

| 池 | 上游 | 积分 |
|---|---|---|
| `trae` | `trae-api-cn.mchost.guru` `POST /api/agent/v3/llm_utils_chat`（SOLO） | 通用积分（product_id 208） |
| `buddy` | `copilot.tencent.com`（CN）/ `www.workbuddy.ai`（Global，按账号 domain 判定） | Buddy 积分 |
| `custom` | 自定义 OpenAI 兼容上游（`custom_models.json`） | 各自计费；**命中即直达、不参与池间策略** |

- **池间策略**（`dispatch.rs`）：`smart` 默认（① 积分最早到期优先 → ② 模型倍率小者优先 → ③ 健康积分总和多优先）；`priority` 严格按序；`per_model` 显式覆盖不做智能重排；`fallback` 跨池回退开关。
- **统一目录**（`unified_catalog.rs`）：三源合并（canonical_id trim+lowercase 归并），Trae 元数据四层兜底（L1 人工 `trae_model_meta` → L2 官网同步 → L3 默认 → L4 系列推断）；`GET /v1/models` 与前端共用。

### 6.2 协议端点

| 端点 | 说明 |
|---|---|
| `POST /v1/chat/completions` | OpenAI 对话（流式 + 非流式） |
| `POST /v1/completions` | legacy 补全（prompt 转 user message 复用链路） |
| `POST /v1/messages` | Anthropic Messages（Claude Code 直连；thinking 块映射 reasoning_content） |
| `POST /v1/responses` | Codex Responses API（仅 WB 上游；请求/流式双向投影） |
| `POST /v1/images/generations` / `edits` | 生图双端点（JSON 变体；模型需声明 supports_image） |
| `GET /v1/models` / `GET /status` / `GET /health` / `GET /healthz` | 目录 / 池画像（含 WB 段）/ 探活（healthz 无健康账号 503） |
| `POST /v1/embeddings` | 固定 501（上游无向量能力） |

### 6.3 关键工程机制

- **WB 请求改写**（`wb_payload.rs`）：强制 `stream:true`（上游只回 SSE，非流式本地聚合）；`tool_choice` 归一 string；reasoning_effort 按 `supported_efforts` 降级；审核指纹清洗（cc_xxx 剥离 + 模板黑名单最小改写，`wb_template_map` mtime 热更新）；连续同角色消息合并。
- **分级重试**（`retry.rs` 纯函数，SOLO/WB 共用）：429=Retry-After 优先→耗尽换号；503/529=10/20/40s 指数；401/403=换号（WB 401 先刷新一次凭证同号重试）；400=context_too_long 类 Fatal 透传；502 同号重试 1 次。
- **会话粘性**（仅 WB）：显式 conversation_id 绑定（TTL 30m 滚动）+ 指纹模式（前 3 消息 SHA256 + 60s 窗）；Mutex 内 re-check 防 TOCTOU；落 `sticky_bindings`。
- **四段模型路由**（`wb_model_route.json`）：aliases → 通配 rules → 内置系列（claude-*→glm-5.3 / gpt-*→deepseek-v4-pro / o1*→hy4）→ `-thinking` 后缀注入 effort=high；全未命中回落原名。
- **ck_ 子 Key**：对外子 Key 与上游凭证分离；`allowed_accounts` 白名单 / `schedule_mode`（expire_first/dedicated）/ `daily_stats`；取号统一 `pick_excluding_constrained`（专一锁定 > 白名单 > 池策略）。
- **流式工程**：SSE keep-alive 15s 注释行；首字超时 10s 故障转移（Agent 300s 读超时兜底）；客户端断连后继续消费上游保 usage 完整；token_usage 附到最后 chunk。
- **WB headers 三铁律（红线）**：① Origin/Referer 按区域必带；② 缺省字段显式 `X-No-User-Id / X-No-Enterprise-Id / X-No-Department-Info: 1` 占位；③ chat 请求绝不携带 `X-Refresh-Token`。UA 伪装 `CLI/2.63.2 CodeBuddy/2.63.2`。

## 7. 开发与运维

```bash
npm install && npm run dev   # 前端（vite 代理 /api /v1 → 127.0.0.1:7864）
cargo run -p aiwork-server   # 后端（AIWORK_LISTEN_ADDR 可配）
cargo test --workspace       # Rust 单测（core 304+ / server 4）
npm run test                 # 前端 vitest
docker compose up -d --build # 生产部署
```

环境变量：`AIWORK_DATA_DIR`（数据目录）/ `AIWORK_LISTEN_ADDR` / `AIWORK_ADMIN_TOKEN` / `AIWORK_VAULT_KEY` / `AIWORK_WEB_DIST` / `TZ`——详见 server-deploy.md。

**常见排错**：

| 现象 | 原因 | 处理 |
|---|---|---|
| 容器 restart 循环 | `AIWORK_VAULT_KEY` 与已有 `vault_key.bin` 不一致 | 固定 env 或恢复 volume |
| 管理面 401 | 未登录 / cookie 过期 / token 变更未重建容器 | 重新登录；env 变更后 `docker compose up -d` |
| 网关命令 404 | 命令未在 cmd_bridge 白名单注册 | 补注册（fail-closed 是特性） |
| SSE 进度不实时 | 反代缓冲 | Nginx `proxy_buffering off` |
| 签到时刻漂移 | 容器时区 | `TZ=Asia/Shanghai` |

## 8. 风险与应对

| 风险 | 等级 | 应对 |
|---|---|---|
| 上游升级导致接口/路径变化 | 高 | 宽容解析（dig 链式取值）；协议层独立模块 + 单测锁定 |
| 上游启用证书固定 | 高 | 降级：OAuth 登录获取可续期凭据（refresh_token 链自动续期） |
| 容器出口 IP 触发风控 | 中 | 请求头为合成指纹无硬件绑定；上线初期观察 |
| 凭证泄露 | 中 | vault 加密 + UI 掩码 + 导出提醒；vault_key.bin 访问边界（0600/compose secrets） |
| 网关公网暴露 | 中 | api_keys fail-closed + admin cookie 域分离 + 反代 TLS / IP 允许列表（server-deploy.md） |
| vault_key 丢失 | 高 | 文档强制备份；丢失 = 凭据不可解密（需重新录入） |

## 9. 附录 A：Trae API 协议参考（抓包实证）

> 完整抓包过程记录见 git 历史（原 api-credit-analysis.md，2026-08-14 归档）。此处保留开发必需的协议事实。

### 9.1 域名与端点

| 域名 | 端点 | 用途 |
|---|---|---|
| `trae-api-cn.mchost.guru` | `POST /api/agent/v3/llm_utils_chat` | 核心对话（IDE 积分 208），HTTP + SSE |
| `trae-api-cn.mchost.guru` | `POST /api/ide/v1/get_detail_param` | 模型列表 |
| `api.trae.cn` | `POST /trae/api/v2/ug/checkin_credits/claim` / `status` | 执行签到 / 签到状态 |
| `api.trae.cn` | `POST /trae/api/v2/pay/ide_user_ent_usage` | 积分/权益查询（208/209 分包） |
| `api.trae.com.cn` | `POST /cloudide/api/v3/trae/oauth/ExchangeToken` | Token 刷新 |
| `api.trae.com.cn` | `POST /cloudide/api/v3/trae/GetUserInfo` | 用户信息 |
| `www.trae.cn` | `GET /authorization` | OAuth 登录授权页 |

### 9.2 请求头与认证

`Authorization: Cloud-IDE-JWT <accessToken>`，附带 `X-Cloudide-Token`、`X-Ide-Token`、`X-App-Id`、`X-Ide-Version`、`X-Device-Id` 等头。`x-device-id` 由 `device_map.json`（含 `derive_device(uid)` 确定性派生兜底）提供，与真实硬件无关。

### 9.3 请求体加密结论（重要）

- TTNet/aha 传输层存在 `@aha-kit` 加密（`x-bridge-transport: aha` 下 body 加密）；真实客户端对话为直连 HTTPS POST + aha 加密体。
- `llm_utils_chat` 端点**明文 JSON 可行**（已验证）；`create_agent_task`（Work 积分 209）为富上下文加密体，外部无法复刻——Work 积分接入已排除（2026-09-15，见 backlog.md W-01 留档）。

### 9.4 SOLO SSE 自定义事件

```
event:metadata      会话元数据（session_id/model，忽略）
event:timing_cost   耗时统计（忽略）
event:output        ×N 增量内容（response / reasoning_content / tool_calls）
event:extra_info    额外信息（忽略）
event:token_usage   token 统计（prompt_tokens/completion_tokens/total_tokens）
event:done          结束信号（finish_reason）
event:error         流内错误（code:1005 → PlanLimit 等）
```

转换要点：`output` → `delta.content/reasoning_content/tool_calls`（清理 namespace/partial_arguments 等 SOLO 专属字段）；`token_usage` 附到最后 chunk 的 `usage`；`done` → `finish_reason` + `[DONE]`；上游中断无 done 时幂等兜底仍写 `[DONE]`；`error` → 回调冷却 + 注入错误事件。

## 10. 附录 B：WorkBuddy / CodeBuddy 协议参考（抓包实证）

> 实施蓝本原文见 git 历史（2026-09-13 归档）；批次 1~4 已全部落地并随 Web 版保留。本节保留开发排错必需的协议事实。

### 10.1 已验证端点速查

| 用途 | 端点 | 要点 |
|---|---|---|
| token 续期 | `POST www.codebuddy.cn/v2/plugin/auth/token/refresh` | `X-Refresh-Token` 头，空体 `{}`；**该头仅允许出现在此端点** |
| Keycloak 备选 | `POST {iss}/protocol/openid-connect/token` | `grant_type=refresh_token&client_id=console` |
| OAuth 授权 | `POST /v2/plugin/auth/state?platform=CLI` → `GET /v2/plugin/auth/token?state=` → `GET /v2/plugin/login/account?state=` | 独立 cookie jar；无 PKCE；Web 版轮询 ≤300s |
| 签到 | `POST /v2/billing/meter/daily-checkin`（状态回退 `/checkin-activity-status`） | 空体 `{}`；`code:10001`=已签容错 |
| 积分三件套 | `POST <domain>/billing/meter/get-user-resource-{summary,paid-packages,free-packages}` | 需 `X-Client-Platform: web` |
| 积分旧接口 | `POST /v2/billing/meter/get-user-resource` | `ProductCode: p_tcaca`，`Status:[0,3]` |
| 官方用量 | `POST /billing/meter/get-user-request-usage` | 日/周/月，分页 requestId 去重 |
| 对话上游 | `POST copilot.tencent.com/v2/chat/completions`（CN）/ `www.workbuddy.ai`（Global） | **只回 SSE**，非流式本地聚合 |
| 模型目录 | `GET {chatBase}/console/enterprises/personal/models` | 倍率/徽章/思考档位动态替换 |
| 成长中心 | `/v2/activity/growth/buddy/travel|lottery|tasks|energy|streak` | 全部实测 |

### 10.2 域名路由与请求头铁律

| 区域 | 判定 | chat 上游 | billing/积分 |
|---|---|---|---|
| CN | domain 不含 `.workbuddy.ai` | `copilot.tencent.com` | `www.codebuddy.cn` |
| Global | domain 含 `.workbuddy.ai` | `www.workbuddy.ai` | `www.workbuddy.ai` |

- **令牌域与请求域不一致会被网关拒绝**；plugin 网关（token refresh）固定 codebuddy.cn 不随区域。
- 三铁律：① Origin/Referer 必带（按区域）；② 缺省字段显式 `X-No-User-Id / X-No-Enterprise-Id / X-No-Department-Info: 1` 占位；③ **chat 请求绝不携带 `X-Refresh-Token`**。UA 伪装 `CLI/2.63.2 CodeBuddy/2.63.2`。
- 签到/活动接口可用极简头（`User-Agent: WorkBuddy` + Bearer + X-User-Id）。

### 10.3 联调避坑清单（实测实证）

| # | 坑 | 对策 |
|---|---|---|
| 1 | 上游拒绝非流式（code 11101） | 强制 `stream:true`，非流式本地聚合（tool_calls delta 按 index 合并） |
| 2 | `tool_choice` 对象报 400 | 归一化为 string |
| 3 | effort 档位上游忽略 | 按 `supportedEfforts` 降级；**hy3 系列仅 `high` 真正生效**（effort_override 修正层） |
| 4 | Claude Code 指纹触发审核 | 指纹清洗（cc_xxx 键值 / x-anthropic-* 引用剥离）+ 两句固定 system 模板逐字入黑名单，映射表最小改写（外置热更新） |
| 5 | chat 带 X-Refresh-Token 触发安全拦截 | 红线：仅 refresh 端点 |
| 6 | SSE 长流被中间层回收 | 15s keep-alive 注释行 |
| 7 | 客户端断连丢 usage | `_drain_upstream` 读完上游 |
| 8 | **prompt cache 对代理流量恒不命中**（按冷启动全价计费） | 成本模型按无缓存估算；缓存命中率指标仅基于本地 token 统计并标注口径 |
| 9 | 连续同角色消息 | 自动合并 |
| 10 | WB 401 | 先刷新一次凭证同号重试，再失败才换号 |
