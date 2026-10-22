# Buddy 国际版支持落地方案

> - **日期**：2026-10-15
> - **状态**：已完成（2026-10-15 Gate 全清零 + 阶段 1/2/3 主体落地；开发计划见 `docs/completed/BuddyAI对齐国内Buddy六页开发计划.md`，P1-P8 全绿）
> - **关联**：`docs/tech-framework.md` §5.2 区域路由 / `docs/archived/WorkBuddy产品设计蓝本.md` F-36 / `docs/completed/Qoder平台支持设计方案.md`（国际版决策先例）/ backlog F-71（Trae SG，同族远期项）
> - **结论先行**：国际版走**独立池 `buddy-ai`**，与国内 `wb` 池账号、凭证、调度、指纹、记账、UI 六面完全隔离；对话内核（SSE/聚合/payload）参数化复用不复制代码。当前代码已具备约 40% 基础（F-36 双区路由），缺口集中在**账号入口与运营面**。~~开工前必须先过 §5 的 Gate 探测~~ → **Gate 阻塞层已全部清零（2026-10-15 真实国际账号实测：R-1 全链路 / R-2a / R-6 / R-9 一次登录收口，R-4 打卡全链 3 秒跑通；R-9 实锤两域同一账号体系）**。剩余仅为 R-7/R-8 放量观察；**2026-10-15 代码审查核对：阶段 1/2/3 主体已在代码落地**（§2.2/§6 已逐项标注），同日收口三处残余——CN 手动续期端点收敛、wb_common 过时注释修正、CN 池双导入入口拒绝国际凭证（`is_intl_credential` + 单测）。

---

## 📋 1. 背景与目标

### 🌐 1.1 三个官方域名的定位区分

| 域名 | 产品定位 | 在本方案中的角色 |
|---|---|---|
| `www.workbuddy.ai` | **WorkBuddy 国际版**（全场景 AI 工作台）：专家 agent 并行、交付 Markdown/Word/PPT、集成 Slack/Telegram/Jira/Notion 等 | 国际版**对话网关 + 计费域**（`/v2/chat/completions`、`/billing/meter/*`，F-36 已实证可达） |
| `www.codebuddy.ai` | **CodeBuddy 国际版门户**：IDE / IDE 插件 / CLI 三入口的官网；官方客户端配置中是**鉴权域名**（external 白名单，鉴权前缀为空 `/v2/auth/...`） | CodeBuddy 国际 CLI 的原生认证域；但外部实测该域**在部分网络 DNS 不解析**（sinkhole 0.0.0.1），国际 CLI 身份实测可 ride `workbuddy.ai` 边缘（§9 附录 A） |
| `www.codebuddy.ai/cli` | **CodeBuddy Code 国际 CLI**：`npm install -g @tencent-ai/codebuddy-code`（Node 22+），终端编码 agent（/init、CodeBuddy.md、MCP、sub-agents） | 国际版**身份指纹候选之一**；外部项目实证 WorkBuddy 桌面端身份（`X-IDE-Type: WorkBuddy`）在国际网关同样可用且模型目录更全（§9 附录 A/B） |

对应国内族（仅作对照，本方案不改其任何行为）：

| 国内域 | 角色 |
|---|---|
| `copilot.tencent.com` | 国内 CLI/插件 API 网关（chat、auth/state、auth/token、login/account） |
| `www.codebuddy.cn` | 国内门户 + plugin auth（token refresh）+ billing |
| `www.workbuddy.cn` | 国内 WorkBuddy 工作台站（套餐/活动页数据源之一） |

> ⚠️ 注意区分 `workbuddy.cn`（国内）与 `workbuddy.ai`（国际）——现有 [credits.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/workbuddy/credits.rs) 中 CN 分支实际混用 `codebuddy.cn` 与 `workbuddy.cn` 两个域，方案落码时一并收敛为 `region_billing_base()` 单源（见 §4.5）。✅ 已收敛：`region_activity_base`/`region_billing_base` 单源（wb_common.rs:591/:603），credits.rs 散点已全部接线。

### 🎯 1.2 目标

1. **接入国际版 Buddy 上游**：国际账号可登录/导入、可对话（流式/非流式/工具代执行）、可查积分，网关 `/v1/*` 统一出口可路由到国际池。
2. **不交织**：国内 `wb` 池的数据、配置、调度行为、失败域**零改动**；国际版故障不得触发国内池换号/熔断，反之亦然。
3. **复用内核**：SSE 转发、非流式聚合、payload 归一化、记账骨架按区域参数化复用，不复制 wb_* 模块族。

### 🚫 1.3 非目标

- 不做 Trae SG（F-71）与 Qoder 国际版（既有决策：Credits 不互通、账号体系独立，维持不做）。
- 不做国际版 OAuth 之外的登录方式（如账号密码托管）。
- 不做国际版模型倍率的自动抓取（首版静态配置，沿用「官网实际值修正」口径）。

---

## 📊 2. 现状盘点

### ✅ 2.1 已实现（可直接复用）

| 能力 | 位置 | 说明 |
|---|---|---|
| chat 双区路由 | [wb_upstream.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_upstream.rs) `WB_CHAT_HOST_CN/GLOBAL`、`WbCreds::chat_base()` | `global_region=true` → `www.workbuddy.ai`，F-36 已实证 |
| 区域判定 | [wb_common.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/wb_common.rs) `is_global_region()` | `domain.contains(".workbuddy.ai")`，token store `domain` 字段承载 |
| billing 双区分 base | 同上 `BILLING_BASE_CN/GLOBAL`、`region_billing_base()`、`billing_bases()` 双域探测 | 签到余额查询已按区切换 |
| 请求头三铁律 | [wb_upstream.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_upstream.rs) `build_chat_headers()` | Origin/Referer 随 `chat_base()` 自动切区 |
| 池账号区域字段 | [pool.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/pool.rs) `PoolAccount.global_region`、`WbSyncAccount.global_region` | 贯穿 pick→route 全链（wb_route 4 处传递） |
| 模型目录区域参数 | [wb_catalog.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_catalog.rs) `global_region` 入参 | 拉取侧已具备分区能力 |

### ⚠️ 2.2 缺口清单（本方案工作量主体）

> **2026-10-15 代码审查回填**：下表缺口主体已落地，逐项现状以「落地状态」列为准。

| # | 缺口 | 位置 | 现状 | 落地状态（2026-10-15 核对） |
|---|---|---|---|---|
| G-1 | **登录入口** | [oauth.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/workbuddy/oauth.rs) `oauth_flow()` | ~~`BASE=copilot.tencent.com`、`WEB_ORIGIN=www.codebuddy.cn` 硬编码~~ | ✅ 已实现：`oauth_login_async(ep: &WbRegionEndpoints)` 区域参数化 + `buddy_ai_oauth_login`（`REGION_GLOBAL` + `PoolSink::BuddyAi`），Intl OAuth 可用 |
| G-2 | **token 刷新分区** | accounts.rs L590、wb_upstream `WB_REFRESH_URL`、wb_common `REFRESH_URL` | ~~三处硬编码~~ | ✅ 已收口：网关侧 `refresh_access_token_for(intl)` 双池双端点（wb_upstream.rs:583）；CN 手动续期改经 `region_refresh_url(&domain)`（accounts.rs）；`REFRESH_URL` 仅作 CN 常量保留 |
| G-3 | **签到/成长/活动** | [wb_checkin.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/wb_checkin.rs) `urls_for()` | ~~国际活动接口未知~~ | ✅ 已实现：`tasks/buddy_ai_checkin.rs`（ACP 每日活跃打卡 + `fetch_total_dosage` + 启动补跑），调度任务 `buddy-ai-checkin`（09:20，开关默认关） |
| G-4 | **模型目录与倍率** | [wb_catalog.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/wb_catalog.rs) | ~~Global 清单未知~~ | ✅ 目录已实现：`builtin_buddy_ai()` 内置 Global 目录（SQLite kv 缓存）；⏳ 倍率静态配置与帮助页 Intl 档展示待补 |
| G-5 | **指纹清洗分区** | `wb_template_map` + `default_template_map()` | ~~国际身份句无覆盖~~ | ✅ 已实现：`wb_route.rs:150` `template_map_key()` 分区键 `buddy_ai_template_map`；内置英文身份句条目随放量观察补齐 |
| G-6 | **池隔离** | [pool.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/pool.rs)、dispatch.rs | ~~混居同一 wb 池~~ | ✅ 已实现：`state.buddy_ai_pool` 独立池 + `buddy_ai_pool_lock` + 一次性迁移 `migrate_wb_global_accounts`（幂等 marker + 备份）；导入侧 `is_intl_credential` 拒绝国际凭证入 CN 池（双重保险） |
| G-7 | **记账口径** | [usage.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/usage.rs) | ~~无独立 bucket~~ | ✅ 已实现：`UsageBucket::BuddyAi => "buddy-ai"`（usage.rs:499） |
| G-8 | **前端** | `src/pages/buddy/*`、PoolHealthBadges.tsx | ~~无区域列/分区 UI~~ | ✅ 已实现：`src/pages/buddyai/BuddyAiHome.tsx` 独立页 + 四页分区；PoolHealthBadges 组件池无关（props 驱动），healthz 后端已四池 selectable 上报，无需改造 |
| G-9 | **UA/客户端指纹** | wb_upstream `WB_UA`/`WB_CLI_VERSION` | ~~固定国内 CLI 2.63.2~~ | ✅ 已实现：`REGION_CN.ua = CLI/2.63.2...`、`REGION_GLOBAL.ua = WorkBuddy/5.5.2...` 双身份常量分立（wb_common.rs:657-685） |

### 🧬 2.3 域名族谱

```text
CN 族（现状，不动）
├── copilot.tencent.com        chat 网关 + auth/state + auth/token + login/account
├── www.codebuddy.cn           plugin auth/refresh + billing/meter + 门户
└── www.workbuddy.cn           工作台站（套餐/活动页）

Intl 族（已由外部项目情报确认，详见 §9 附录）
├── www.workbuddy.ai           chat 网关（✅ F-36）+ billing/meter（✅）+ OAuth auth
│                              三件套 /v2/plugin/auth/*（platform=workbuddy-ai）
├── www.codebuddy.ai           CodeBuddy 海外站：鉴权 prefixPath 为空（/v2/auth/...），
│                              部分网络 DNS 不解析（国际 CLI 实测 ride workbuddy.ai）
├── staging-codebuddy.tencent.com   国际预发布端点
└── tencent.sso.codebuddy.cn 等企业 SSO 中转域（ioa 白名单）
```

---

## 🏗️ 3. 总体设计：独立池 `buddy-ai`

### 🧭 3.1 设计原则：「不交织」六条硬约束

1. **账号分池**：国际账号入 `buddy-ai` 池（独立池文件/表、独立 token store 键空间），禁止与 `wb` 池混居；存量混居账号一次性迁移（§4.7）。
2. **配置分键**：调度策略、签到间隔、积分同步等配置键独立（`buddy_ai_*`），`Settings` 不复用国内字段。
3. **失败域隔离**：换号轮换、冷却、熔断、空完成收尾均在池边界内闭环；`/healthz` 按池独立上报 selectable（对齐 [pool.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/pool.rs) `pool::selectable` 口径，Qoder 池接入同款教训）。**拍板注记（2026-10-15 审查）**：冷却表严格分池独立（buddy_ai_model_cooldowns），但 dispatch 回退闸门（BM-3 改造：`available_count >= 2`）允许 **Buddy↔BuddyAi 两池对「同目录共有模型」互为回退源**——这是有意的可用性放宽（两区共有 id 如 glm/kimi 系），Intl 专属 id 仍严格锁 buddy-ai 池（`buddy_intl_only` 门控）；若要求两区完全互不回退，需把回退候选按池过滤，当前按放宽执行。
4. **指纹分区**：清洗模板表独立键（`buddy_ai_template_map`），内置默认表按区生成；11128 强制清洗、请求级快照机制照搬复用。
5. **记账分区**：`UsageBucket::BuddyAi => "buddy-ai"`，非流式聚合所有轮换出口仍保证**恰好一条 record_usage 终态行**。
6. **UI 分区**：账号、签到、设置、API 服务页均以区域维度分区展示，不出现同列表混排。

### 🗺️ 3.2 架构与数据流

```text
                        网关统一出口 /v1/*（不变）
                                 │
                        dispatch（模型路由）
                                 │
              ┌──────────────────┼──────────────────┐
              │                  │                  │
        wb 池（CN，不动）   buddy-ai 池（新增）   trae/qoder/custom 池
              │                  │
              │            WbUpstreamContext{ region: Global }
              │                  │
              └────────┬─────────┘
                       │  参数化复用内核（wb_upstream/wb_sse/wb_payload/
                       │  wb_responses/wb_images/wb_sticky 按账号 region 分流）
                       ▼
      CN: copilot.tencent.com        Intl: www.workbuddy.ai
      refresh: www.codebuddy.cn      refresh: ?R-2 探测后注入
```

**内核复用方式**：`WbCreds` 已携带 `global_region`，wb_upstream/wb_route 内部本就按账号分流。独立池改造的实质是——**pick 层的账号来源分池，内核层的区域分流保持现状**。即：`wb_upstream_accounts(state)` 拆为按池取号（CN 池读 `wb` 池文件，Intl 池读 `buddy-ai` 池文件），`wb_route` 三路径（流式/非流式聚合/toolexec）按请求上下文绑定池视图，其余不动。

### 📌 3.3 端点矩阵（已按 §9 附录情报填充）

| 用途 | CN（现状） | Intl（目标态） | 状态 |
|---|---|---|---|
| chat（SSE，仅 stream:true） | `copilot.tencent.com/v2/chat/completions` | `www.workbuddy.ai/v2/chat/completions` | ✅ 已实证（两外部项目同证） |
| OAuth 三件套 | `copilot.tencent.com/v2/plugin/auth/state?platform=CLI`（我方）/ `?platform=workbuddy`（官方桌面端） | `www.workbuddy.ai/v2/plugin/auth/state?platform=workbuddy-ai` → `/token?state=` → `/account?state=` | ✅✅ 全链路实测通过（R-1）；⚠️ `/account` 单数端点国际域 404，账号标识走 billing |
| token refresh | `www.codebuddy.cn/v2/plugin/auth/token/refresh`（`X-Auth-Refresh-Source: workbuddy`） | `www.workbuddy.ai/v2/plugin/auth/token/refresh`（prefix 同为 `/plugin`；`X-Auth-Refresh-Source` 两种取值均接受，rt 每次轮换） | ✅✅ R-2a 实测收口 |
| 多账号列表 | — | `GET /v2/plugin/auth/accounts` → `data.accounts[]{uid,nickname,enterpriseId,type,pluginEnabled}` | ✅ 情报（复数端点未实测，单数 `/account` 404） |
| billing/meter | `www.codebuddy.cn`（checkin-activity-status / daily-checkin / get-user-resource `ProductCode:"p_tcaca"` 等） | `www.workbuddy.ai` 同路径（协议一致、仅端点不同） | ✅ 路径确认，字段同构 R-6 |
| 活动页 | `/v2/activity/workbuddy/banner`（缺 X-Product 被白名单拦） | 同路径于 `www.workbuddy.ai` | ✅ 路径确认 |
| 签到/成长 | `wb_checkin::urls_for()` 双探测 | 国际版为「每日活跃打卡」（网页会话 + 沙箱 ACP 跑完领 30/50 积分），**非简单 daily-checkin** | ✅✅ R-4 实测全链通过（§4.5 补篇） |
| 模型目录 | wb_catalog 源 | `GET /v3/config` 的 `agents[cli].models`（UA 带 CLI 段才下发 51 全量，缺则 37 旧版） | ✅ 来源确认（R-5 解） |
| 身份指纹 UA | `CLI/2.63.2 CodeBuddy/2.63.2` | 桌面端身份：`WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/2.137.1`；CLI 身份：`CLI/2.117.2 CodeBuddy/2.117.2`（ride workbuddy.ai） | ✅ R-3 解（候选二选一，§9） |

### 🏷️ 3.4 隔离清单（命名约定）

| 维度 | CN（现状，不动） | Intl（新增，标识定稿 `buddy-ai`） |
|---|---|---|
| 产品标识 | **buddy**（UI 显示名；技术层内部命名 `wb_*` 保留不改——不交织硬约束，改名会全库波及；对外池标识统一 `buddy`，dispatch v1.2 命名约定 `TargetPool::Buddy`，`wb` 仅存内部字段名 `wb_pool`/`wb_enabled` 与账号 id 前缀 `wb-`） | **buddy-ai**（池 id / app_id / bucket / 前端档位全链统一，代码 `TargetPool::BuddyAi => "buddy-ai"`） |
| 池 id / app_id | 对外 `buddy` / 内部 `wb_*` | `buddy-ai` |
| 池存储 | `wb` 池（store 库 `wb_accounts` 表） | `buddy_ai_accounts` 独立表（store 库 SQLite，已落地） |
| token store 键空间 | `wb_tokens` 表 + vault 命名空间 | **`buddy_ai_tokens` 独立表 + vault 命名空间 `buddy-ai`**（§8-5 定稿：敏感字段入 stronghold vault、DB 一律占位；`wb_common.rs` `BUDDY_AI_TOKEN_TABLE`/`BUDDY_AI_VAULT_NS`） |
| 用量 bucket | `UsageBucket::Wb => "wb"` | `UsageBucket::BuddyAi => "buddy-ai"`（已落地） |
| 粘性绑定键 | wb_sticky 现有前缀（`q:`/`t:`） | 前缀 **`g:`**（与 §4.2 一致；`wb_sticky.rs` 命名空间 `["q:", "t:", "g:"]`，已落地） |
| 清洗模板 KV | `wb_template_map` | `buddy_ai_template_map`（独立热更新键，UI 独立入口） |
| 调度策略键 | `wb_strategy` | `buddy_ai_strategy` |
| 签到间隔 | `wb_checkin_gap_secs` | `buddy_ai_checkin_gap_secs`（`Option<u64>` 语义对齐） |
| 池运行锁 | `wb_pool_lock` | `buddy_ai_pool_lock` |
| UI 图标 / 档位标识 | buddy 现有图标 | **BuddyAi** 独立图标（账号页 / PoolHealthBadges / API 服务档位 / 模型目录处与 buddy 视觉区分，标识文案 `BuddyAi`） |

### 📊 3.5 国内已实现功能 → 国际版（buddy-ai）功能对照

> 国内 `wb` 池功能面为蓝本，逐项映射国际版：**内核复用 / 机制复用 + 模板分区 / 新实现** 三类。

| 国内 buddy（wb 池已实现） | 国际版 buddy-ai 对应 | 复用方式 |
|---|---|---|
| OAuth 登录（copilot.tencent.com 三件套，platform=CLI） | OAuth 登录（workbuddy.ai 三件套，platform=workbuddy-ai，authUrl 带 `version`） | `oauth_flow` 区域参数化（§4.1，R-1 已实测） |
| 手工导入凭证 | 同 + 区域必选校验 | 复用命令 |
| token refresh（codebuddy.cn，source=workbuddy） | refresh（workbuddy.ai，双取值均可，**rt 轮换回写**） | `region_refresh_url` 参数化 |
| 多账号管理 + 设备凭证轮转 | 同（UID 派生指纹天然跨区隔离） | 直接复用 |
| chat 流式路由（wb_route 三路径） | 同构 | 内核参数化复用 |
| 非流式聚合 / toolexec | 同构 | 复用 |
| SSE 转发（has_content / -9901 哨兵） | 同 + **heartbeat 注释行容忍 / reasoning_content 兼容**（R-8 实测新增） | 复用 + 小增强 |
| 指纹清洗（`wb_template_map` + 11128 强制清洗 + 请求级快照） | `buddy_ai_template_map`（英文身份句内置表；11128 实测为 payload 校验，首条必须 system prompt） | 机制复用、模板分区 |
| 粘性会话 wb_sticky | 独立前缀 `g:`（2026-10-15 勘误：§3.5 原写 `ba:` 与实现/隔离清单不符，代码为准） | 复用骨架 |
| 每日签到 wb_checkin（daily-checkin） | **每日活跃打卡**（网页会话 + ACP 六步链，§4.5 补篇，全链已实测） | 新任务 `buddy_ai_checkin` |
| 积分查询 wb_credits | get-user-resource（`TotalDosage` / `Accounts[]` 字段链，R-6 实测） | 解析链按区对齐 |
| 积分快照同步 / 运行期保鲜 | 同构 | 复用 |
| 成长任务 / 活动接取 | 国际版无对应（网页腿打卡即每日活动） | 不做（YAGNI） |
| 模型目录 wb_catalog | `/v3/config` `agents[cli].models`（UA 带 CLI 段 → 51 全量） | 参数化源 |
| 模型倍率展示（最小倍率口径） | 首版静态配置 | 复用展示层 |
| 池调度 / 冷却 / healthz / 空完成收尾 | buddy-ai 池独立闭环（跨域同体系已实锤，无 edition 分叉） | 骨架复用 |
| 记账 `UsageBucket::Wb`（恰好一条终态行） | `UsageBucket::BuddyAi => "buddy-ai"` | 枚举新增，口径不变 |
| 健康徽标 PoolHealthBadges（三池共用） | 四池化 | 复用组件 |
| 调度器 checkin / 成长定时任务 | buddy-ai 每日打卡定时任务（09:00 口径对齐外部项目） | 新增，套调度器骨架 |

---

## 🔧 4. 关键设计点

### 4.1 账号与凭证

- **登录**（G-1）：`oauth_flow()` 区域参数化——CN 保持 `copilot.tencent.com` 链路原样；Intl 按 R-1 探测结果注入 `auth_base`/`web_origin`/`login_base` 三常量（从硬编码提升为 `WbRegionEndpoints` 结构体，CN/Global 两份静态实例）。探测结论未出前，Intl 登录按钮置灰 + 提示「待上游验证」，**仅开放手工导入通道**。
- **导入**：现有凭证导入命令增加区域必选参数（默认 CN，显式选 Global 时校验 domain 含 `.workbuddy.ai`，二者不一致直接拒绝——区域判定唯一真源仍是 `domain` 字段，不引入第二个真源）。
- **刷新**（G-2）：三处硬编码统一收敛到 `wb_common::region_refresh_url(domain)`；R-2 探测通过前，Intl 账号刷新失败按「暂态网络」处理**不标记 needs_relogin**（避免误杀真实有效的国际凭证），落 app_log 留痕。
- **刷新并发**：账号级跨进程刷新锁语义照搬（WB 池既有 `Busy` 幂等回退口径，见 `RefreshFail::Busy`）。
- **设备指纹**：`derive_device_fingerprint(uid, kind)` 按账号派生，天然跨区隔离；R-7 确认国际版是否校验 `X-Machine-ID/X-Session-ID` 后决定是否需要额外适配。

### 4.2 网关转发与调度

- **取号**：`wb_upstream_accounts()` 拆双池视图；dispatch 侧 `priority` 数组支持 `"buddy-ai"` 档位（与 `"wb"` 平级，默认不启用，由用户在 API 服务页显式开启国际档——避免模型路由歧义）。
- **粘性**：会话粘性按池独立绑定（`g:` 前缀），`pick_sticky_yield`/`qoder_sticky_enabled` 同款开关独立为 `buddy_ai_sticky_enabled`。
- **换号收尾**：`exhaust_message(empty_hits, rotated, healthy)` 口径复用——空完成主导时同样落 `CHANNEL_FINGERPRINT_BLOCK` 标记 + `fingerprint_sample`（issue #99 三池 7 条收尾路径全覆盖的口径扩展为「含 buddy-ai」）。
- **护栏全量照搬**（这些是 wb 池踩过坑的结晶，国际池一律继承）：
  - 空完成换号前 `sent_any` 护栏；零内容不发收尾帧，-9901 上抛且免 Server 熔断；
  - CAS 绑定成功后才更新 `active_uid`；
  - 11128 / "Illegal API invocation from an unapproved channel" → 强制清洗同号重试一次（不消耗换号额度），清洗状态走请求级快照；
  - 首字超时 10s 故障转移（`FIRST_BYTE_TIMEOUT`）。
- **冷却**：模型级冷却表键为裸 model 名（wb 池现状），Intl 池冷却表独立实例；`PoolStatus.cooling`（软冷却）与 `hard_credit`（硬冷却）单列语义不变。

### 4.3 指纹清洗分区

- 模板表拆分：`wb_template_map`（CN，现状不动）+ `buddy_ai_template_map`（Intl）。内置默认表新增国际 CLI 身份句覆盖（英文句式，具体条目待 R-3 抓包确认后入库；`powered by → built on` 手法沿用 DSH 案例）。
- 合并语义沿用 3.7.11 口径：内置全量生效，自定义按 `from` 逐字覆盖/追加；`to:""`=剥离、`to:null`=停用两空态语义不变。
- UI：API 管理「指纹清洗」tab 增加区域切换（CN/Intl 两组模板表，共用 FingerprintSanitizePanel 组件，`builtin:true` 回显逻辑按区独立）。

### 4.4 模型目录与倍率

- 目录：`wb_catalog` 拉取侧已带 `global_region` 参数，Intl 池目录独立缓存（`api_models.json` 分池键或独立文件，落码时对齐现状）；Global 专属模型在目录中标注地区列（对齐 Qoder 目录「地区」列先例：仅目录可见、不可路由时显式 404，不静默降级）。
- 倍率：首版静态配置（官网实际值人工维护，Qoder 倍率修正同款原则）；「API 使用帮助展示取各应用对应模型最小倍率值」口径扩展到 `buddy-ai` 档。
- 模型 ID 归一化：沿用「仅去除空白和连字符、保留小数点」规则。

### 4.5 签到 / 积分 / 成长

- **前提**（已更新，2026-10-15 拍板）：国际版无国内式 daily-checkin，取而代之的是**每日活跃打卡**（官方每日 30/50 积分）——已拍板要做（§8-2）。国际版签到 UI 不隐藏，改为展示「每日活跃打卡」状态与手动触发入口；实现口径见本节末「补篇」。
- **打卡不做国内式接口复用**：国际打卡是「网页会话 + 沙箱 ACP 跑完一轮对话」复合动作，不复用/不污染国内 wb_checkin 骨架，独立 `buddy_ai_checkin` 任务（调度间隔键 `buddy_ai_checkin_gap_secs` 已在隔离清单）。
- **积分快照**：`wb_credits::fetch_credits` 字段解析链已兼容多嵌套路径，Intl 复用前过 R-6 字段同构性探测；快照三键同拨（credits/expire_at/fetched_at）口径不变；签到到账回写运行池 credits 的 runtime 句柄路径照搬。
- **域名收敛**：[credits.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/workbuddy/credits.rs) 中 `workbuddy.ai/workbuddy.cn` 散落硬编码（L234-238、L794-796 等 6 处）统一收敛到 `region_billing_base()`，消除 CN 分支双域混用——此项**属国内侧低风险重构**，单独成 commit 便于回滚。
- **套餐回填**：payment-type → edition_type 的「仅补空」语义保留，Intl 分区走收敛后的 base。

**补篇：每日活跃打卡实现口径**（2026-10-15 深挖 workbuddy2api-hub 实证：wb_accounts.py `_daily_chat_web_upstream` + wb_webagent.py `run_turn` + wb_scheduler.py）

端点链（全走 `www.workbuddy.ai`；网页腿身份 = 复用网关账号 token：`Authorization: Bearer <accessToken>` + `X-User-Id` + Chrome UA + Origin/Referer，**不带桌面 X-IDE-\* 指纹头**）：

| 步骤 | 端点 | 要点 |
|---|---|---|
| ① 建会话 | POST `/console/as/conversations/` | body `{prompt:"Hi", model:"deepseek-v4.1-flash", conversationOrigin:"workbuddy-app", plugins:[{name:"weixinpay",marketplace:"codebuddy-builtin"}]}` → `data.id` |
| ② 取沙箱 | GET `/console/as/conversations/{id}/session` | 返回沙箱 `link` + **服务端新发的独立 `token`**（非账号 token）+ `sessionId` |
| ③ ACP 通道 | GET `<link>`（`Accept: text/event-stream`） | 响应头取 `Acp-Connection-Id`；`Authorization: Bearer <沙箱token>` |
| ④ ACP 跑一轮 | POST `<link>`（带 `Acp-Connection-Id`） | JSON-RPC 序列：`initialize` → `session/load` → `session/set_model` → `session/prompt`；HTTP 202 即受理；SSE 收 `session/update` |
| ⑤ 完成判定 | GET `/console/as/conversations/{id}` | `data.status == "completed"`，超时 120s（可配） |
| ⑥ 到账校验 | POST `/v2/billing/meter/get-user-resource` | `credits.remain` 对比打卡前快照（R-6 同款端点） |

落码要点（映射到我方 `buddy_ai_checkin` 任务）：

- **无 claim 端点**：奖励由服务端在会话 completed 后**自动入账**，无需领奖调用；30/50 两档由服务端按套餐决定，客户端不选档——仅保证「跑完的 agent 会话」成立即得分。外部实证：桌面腿 `chat/completions` 对话**不计入**活跃，网页腿（console 会话）才是计数通道。
- **省钱红线：`session/set_model` 必发**——建会话 body 里的 model 只进 manifest，不额外发这条请求则会话按默认 Auto 模型计费**扣积分**（外部项目踩坑实证；免费模型钉 `deepseek-v4.1-flash`）。
- **幂等**：本地 `lastDailyChat` 日期戳（落 token store 账号命名空间），网页腿成功才落盘；失败不落盘、下轮巡检自动重试——与「重试退避/断连都落账」口径互为镜像（不成功不记打卡，防假打卡占位）。
- **调度**：对齐国内签到调度器形态（整点巡检），每账号先查幂等戳再动；间隔键 `buddy_ai_checkin_gap_secs`。
- **代理**：~~ACP 通道走 CONNECT 隧道且必须显式塞 `Proxy-Authorization`（缺失 407）~~ → **实现口径（2026-10-15 勘误）**：全链 ureq 直连（R-4 实测 3 秒跑通），CONNECT+Proxy-Authorization 适配仅在「出站被强制代理（407 环境）」时才需要，当前未实现——若部署环境强制出站代理，打卡会恒败于「ACP 通道建立失败」，届时补 ureq Proxy 层。出口级瞬时抖动秒级退避（1s 起翻倍封顶 15s），避免一次抖动熔断整池。
- **身份**：网页腿不用设备指纹头（UID 派生指纹仅桌面腿用，本任务无桌面腿）。

### 4.6 前端

| 页面 | 改动 |
|---|---|
| [BuddyAccounts.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/buddy/BuddyAccounts.tsx) | 顶部区域 Tab（国内 / 国际）分列账号；账号卡加区域徽标；国际区登录按钮状态随 R-1 |
| [BuddySettings.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/buddy/BuddySettings.tsx) | 任务配置按区分组；Intl 签到/成长开关在 R-4 结论前不渲染 |
| [BuddyApiService.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/buddy/BuddyApiService.tsx) | 模型档位支持 `buddy-ai`（默认关）；国际区资源开关独立 |
| [PoolHealthBadges.tsx](file:///d:/code/AIWorkAssistant_main/src/components/api/PoolHealthBadges.tsx) | ✅ 无需改造：组件池无关（徽标由 `PoolStatus` props 驱动），后端 healthz 已四池 selectable 上报（`buddy_ai_available`，routes.rs）；disabled/cooling/hard_credit/零积分/积分过期全挡「就绪」徽标口径不变 |
| BuddyCheckin.tsx | 国际区按 R-4 结论决定渲染签到页或仅积分页 |
| 帮助页（模型倍率） | 加 `buddy-ai` 档倍率列 |

### 4.7 存量迁移与兼容

- 现网 `wb` 池中 `global_region=true` 的账号（当前混居）→ 启动时一次性迁移到 `buddy-ai` 池（**实现口径 2026-10-15 勘误**：备份为 store kv `buddy_ai_migration_backup` 快照而非文件 `move_to_backup`，意图等价——中断可恢复、幂等 marker 防重扫；marker 置位先于 token 行删除，rename 失败降级语义同下）：
  - 迁移前 `move_to_backup` 备份原池文件；**rename 失败降级 Error 且不置位迁移版本号**（Trae 存量迁移同款教训，防旧 JSON 回覆覆盖运行期配置）；
  - token store 侧同步把 `tokens.<id>` 搬到 `tokens_buddy_ai.<id>`（原子写，断电半截防护对齐 `edit_qoder_storage_ids` 口径）；
  - 迁移报告结构化（`settings_note` 字段先例），不搞子串匹配推断；
  - 迁移幂等：版本号置位后不重跑；「旧池目录杂散文件」过滤规则照搬（防迁移恒真弹窗）。
- 兼容窗口：迁移后 `wb` 池不再接受 `global_region=true` 账号（导入命令已显式分区，双重保险）。

---

## 🧪 5. Gate 探测清单（先侦察后施工）

> 原则：所有 R 项未通过前，对应能力**只做骨架不做接线**（置灰/提示），避免盲发请求触发国际站风控。探测产物回填本表并更新 §3.3 端点矩阵。
>
> **分层**：
> - ~~阻塞层（R-1 全链路 + R-2a + R-6 + R-9）~~ ✅ **已全部实测收口**（2026-10-15 真实国际账号一次登录完成，见下表）；
> - **决策层**（评审拍板，§8）：R-3 身份基座（已拍板 R-4 打卡要做）；
> - **观察层**（不阻塞开工）：R-7 设备指纹、R-8 影子风控，随放量观察。
>
> ~~**最小确认路径**：拿一个真实国际账号登录一次 → R-1 全链路 / R-2a / R-6 / R-9 四项同一次实测顺带收口，零额外成本。~~ 已执行完毕。

| # | 探测项 | 状态（2026-10-15 外部情报回填） | 残余动作 |
|---|---|---|---|
| R-1 | 国际版登录端点 | ✅✅ **全链路实测通过**（2026-10-15）：state（匿名四连头，code 0 → `data.state`+`data.authUrl`）→ 浏览器授权 → token 轮询 `11217`→code 0（`expiresIn≈365天`，`scope=openid profile offline_access email`，Keycloak 风格）。**authUrl 格式**：`{base}/login/?platform=workbuddy-ai&state=<uuid>&version=<客户端版本>`——官方桌面端实证带 `version=5.7.6`（⚠️ 比 agent2api 基准 5.5.2 新，选桌面端身份时 UA/版本应跟进 5.7.6）；我方探测不带 version 亦可通过（可选参数，落码对齐官方带上）；授权完成页弹 **deep link 唤起桌面客户端**，plugin 通道点取消即可｜⚠️ `/v2/plugin/auth/account`（单数）国际域 **404 不存在**，账号标识从 billing `Accounts[].AccountId/Uin` 取 | 无（§3.3 已更新） |
| R-2 | 国际 refresh 端点 | ✅✅ **R-2a 实测收口**：`www.workbuddy.ai/v2/plugin/auth/token/refresh` code 0；`X-Auth-Refresh-Source` 取 `plugin` 与 `workbuddy` **均接受**（不做强校验）；**refreshToken 每次轮换** | 落码以响应新 rt 回写为准（对齐国内 refresh 逻辑） |
| R-3 | 国际身份指纹 | ✅ **已解决**：两套候选（桌面端 `WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/2.137.1` / CLI `CLI/2.117.2 CodeBuddy/2.117.2`），均可走 `workbuddy.ai`（§9 附录 A）；实测桌面端身份头集 chat 200 通过 | ✅ 已拍板桌面端身份并落地 `REGION_GLOBAL`（UA 5.5.2 + `client_version=5.7.6`，wb_common.rs:672-685）；UA 是否跟进官方 5.7.6 列 R-7 同级观察项 |
| R-4 | 签到/成长体系存在性 | ✅✅ **拍板要做 + 网页会话全链实测通过**（2026-10-15）：建会话→session→ACP 四连（全 202/200）→**completed 仅 3 秒**；沙箱为 e2b（`*.ap-singapore.sandbox.cloudstudio.club`，`acpBackend=sandbox_gateway`）；免费模型 `set_model` 生效实证（TotalDosage 350 前后不变） | 无（阶段 3 按 §4.5 补篇落码） |
| R-5 | 模型目录与 Global 专属模型 | ✅ **已解决**：来源 `GET /v3/config` `agents[cli].models`；UA 带 CLI 段 → 51 模型全量，缺 → 37 旧版；Intl 17 个 / CN 14 个清单已拿到（§9 附录 A） | 目录拉取接入 + 同名模型跨区不混轮（两项目一致决策，与我方不交织原则同构） |
| R-6 | billing 字段同构性 | ✅✅ **实测同构**：`data.Response.Data.{TotalCount, TotalDosage, Accounts[{AccountId, Uin, AppId, ProductCode:"p_tcaca", Status, ExpiredTime}]}`；⚠️ 无 `credits.remain`（该字段属 `get-enterprise-user-usage`）；TotalDosage=350 即总积分口径 | wb_credits 解析链对齐 TotalDosage/Accounts 字段 |
| R-7 | 设备指纹校验强度 | 🔄 **低风险**：workbuddy2api-hub 用同款 UID 派生（machine+session，固定盐）长期稳定运行，说明派生指纹方案在国际版可用 | 随首次实测观察即可 |
| R-8 | 风控形态差异 | ✅ 部分：11128 实测形态 = HTTP 400 + `"first message is not system prompt"`（**payload 校验而非风控拒绝**，displayMsg 走安全策略文案）；SSE 首行为 `: heartbeat` 注释行；delta 含 `reasoning_content` 字段 | 放量观察影子风控；SSE/payload 归一化需容忍 heartbeat 行并兼容 reasoning_content |
| R-9 | 跨域账号体系互认（workbuddy.ai ↔ codebuddy.ai） | ✅✅ **金标准实锤**（2026-10-15）：同一 wb token 在 `www.workbuddy.ai` 与 `www.codebuddy.ai` 均 **HTTP 200 流式对话成功**（system 首条）；错误形态两域逐字一致（同后端集群）——**同一账号体系，buddy-ai 只需一套登录入口，无需双鉴权门面 + edition 维度** | 无（G-1 单入口方案确认） |

---

## 🚀 6. 分阶段实施计划

| 阶段 | 内容 | 前置 | 验收标准 |
|---|---|---|---|
| **0 侦察** | ✅ **已完成**（2026-10-15）：Gate 阻塞层全部实测收口（§5）；`WbRegionEndpoints` 结构体落地（仅常量，不接线）移入阶段 1 开工项 | — | ~~端点矩阵全部有结论~~ ✅；~~身份基座选定（§8-1）~~ → 移至阶段 1 前拍板；探测过程零脏数据入生产 KV ✅（探测脚本/凭证均在 Temp，已清理 token 落盘） |
| **1 池骨架** | §8-1 身份基座拍板 + `WbRegionEndpoints` 结构体落地、`buddy-ai` 池注册（pool/runtime/锁）、token store `tokens_buddy_ai` 命名空间、**Intl OAuth 登录入口（R-1 已实测，按 §8-4 建议并入）**、手工导入（区域必选 + domain 校验）、存量混居账号一次性迁移、健康徽标四池化、healthz 纳入 `buddy-ai` selectable | 阶段 0（✅ 已满足） | `cargo test` 全绿（迁移单测含 rename 失败降级用例）；存量 global 账号迁移后 CN 池零 global 残留；healthz 在仅部署 Intl 账号时探活正常 |
| **2 网关转发** | `wb_upstream_accounts` 双池拆分、dispatch `buddy-ai` 档、`wb_route` 池视图绑定、粘性 `g:` 前缀、11128 强制清洗/sent_any/-9901/CAS 护栏全量继承、`UsageBucket::BuddyAi` 记账、`buddy_ai_template_map` 分区清洗 | 阶段 0 R-3/R-5（R-8 观察层不阻塞） | 流式+非流式+toolexec 三路径连通 Intl 上游；换号收尾文案/样本落盘正确；`record_usage` 恰好一条终态行（Intl 非流式轮换用例）；`tsc` 无错误 |
| **3 运营面** | **每日活跃打卡 Intl 化（已拍板，§8-2）**：网页会话建立 + 沙箱 ACP 跑完一轮 + 积分领取（实现口径按 §4.5 补篇）；积分同步 Intl 化；模型倍率 Intl 静态配置 + 帮助页展示；前端分区 UI 收尾（Accounts/Settings/ApiService/Checkin）；域名硬编码收敛 commit | 阶段 2 | 打卡全链路自动领取 30/50 积分到账（防重复打卡幂等）；前端四页分区展示完整；倍率展示含 Intl 档且取最小倍率口径正确；国内侧回归：CN 签到/积分/对话全链路行为不变（对比迁移前后 app_log 关键行） |

**回滚设计**：池骨架与迁移均有版本号置位 + 备份文件，回滚 = 还原备份 + 版本号回退；dispatch `buddy-ai` 档默认关，网关行为回滚零影响；CN 侧唯一改动（域名收敛）独立 commit，可单独 revert。

> **2026-10-15 代码审查落地核对**：
> - **阶段 1**（池骨架）✅ 主体已落地：`WbRegionEndpoints` 两实例、`buddy-ai` 池注册（`state.buddy_ai_pool`/`buddy_ai_pool_lock`）、`buddy_ai_tokens` 表 + vault ns、Intl OAuth（`buddy_ai_oauth_login`）、手工导入区域校验、存量混居迁移 `migrate_wb_global_accounts`（幂等 marker + 备份）、healthz 四池 selectable。
> - **阶段 2**（网关转发）✅ 主体已落地：双池取号（`WbView::wb/intl`）、dispatch `buddy-ai` 档（默认关 + Key 绑定兜底序列）、粘性 `g:` 前缀、11128/sent_any/-9901/CAS 护栏继承、`UsageBucket::BuddyAi` 记账、`buddy_ai_template_map` 分区清洗。
> - **阶段 3**（运营面）✅ 大部分已落地：`buddy_ai_checkin` 打卡任务（含启动补跑）、`buddy_ai_credits_fetch` 积分同步、`builtin_buddy_ai` 目录、前端 `BuddyAiHome.tsx` 分区页；⏳ 待补：模型倍率 Intl 静态配置 + 帮助页档位列。
> - 同日收口残余：CN 手动续期端点经 `region_refresh_url` 分区（accounts.rs）、wb_common 过时注释修正、CN 池双导入入口（auth 文件 / 导出文件）经 `is_intl_credential` 拒绝国际凭证 + 单测。

---

## ⚠️ 7. 风险与对策

| 风险 | 等级 | 对策 |
|---|---|---|
| 国际版账号体系与国内完全不同构（如纯 OAuth SSO、无 plugin 网关） | 高 | 阶段 0 R-1/R-2 是全案闸门；最坏退路 = Intl 仅手工导入 + 不可刷新（重导周期由用户承担），池/网关/UI 投入不受阻 |
| 国际站风控未知（11128 是否存在、影子风控形态） | 高 | R-8 低频探测；清洗模板先保守（国际身份句全覆盖）；空完成收尾样本机制先于放量上线 |
| 国际 CLI 版本迭代快，指纹头易过期 | 中 | UA/版本号进 `WbRegionEndpoints` 单点；app_log 记录上游拒识形态，版本更新仅改常量 |
| 双池并存增加调度复杂度 | 中 | 严格按 §3.4 隔离清单执行；dispatch Intl 档默认关；模型名冲突时目录加区前缀（Qoder `global:{model}` 先例备选） |
| 国内侧回归风险 | 低 | 唯一国内改动是域名收敛（§4.5），独立 commit + 回归用例；其余全部走「新增而非修改」 |

---

## 📎 8. 遗留决策点（评审时定）

> 分层：2 已拍板（2026-10-15：每日活跃打卡**要做**，实现口径见 §4.5 补篇）；1 直接影响阶段 0 产出与阶段 2 接线，**优先拍板**；3-6 属实现选型，可在对应阶段开工前定。

1. ~~**身份基座选型**（R-3 残余）~~ → **已拍板并落地（2026-10-15）**：WorkBuddy 桌面端身份（§9 附录 A 推荐项：模型目录最全 51 个、两外部项目长期实证），`REGION_GLOBAL.ua = "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/2.137.1"` + `client_version = "5.7.6"`（wb_common.rs:672-685）；UA 是否跟进官方 5.7.6 列 R-7 同级观察项。国内 CLI 身份（`SaaS` + `platform=CLI`）不可平移国际区，`REGION_CN` 保持不动。
2. ~~**R-4 每日活跃打卡做不做**~~ ✅ **已拍板（2026-10-15）：要做**。国际版机制是「网页会话 + 沙箱把一轮对话真正跑完」（ACP over HTTP+SSE），实现成本远高于国内 daily-checkin——阶段 3 确定为「每日活跃打卡 Intl 化 + 积分同步」，实现口径侦察结论回填 §4.5 补篇。
3. ~~**池 id 命名**~~ → **已定稿 `buddy-ai`**（2026-10-15 用户拍板；国内产品标识 buddy / 技术层池 id `wb` 不动，国际版池 id / app_id / bucket / 前端档位统一 `buddy-ai`，对照表见 §3.4/§3.5）。
4. ~~**Intl 登录是否首版交付**~~ → **建议并入阶段 1**（2026-10-15 R-1 全链路实测通过，登录流已无技术不确定性；OAuth 三件套 + authUrl 流转 + rt 轮换口径全部实锤，见 §5 R-1/R-2）。
5. ~~**token store 单文件双命名空间 vs 独立文件**~~ → **已定稿（2026-10-15）**：独立 SQLite 表 `buddy_ai_tokens` + stronghold vault 命名空间 `buddy-ai`（敏感字段不入 DB 明文；`wb_common.rs` `BUDDY_AI_TOKEN_TABLE`/`BUDDY_AI_VAULT_NS`），按「store 库表结构更宜分表则从其约定」条款落地。
6. **是否同步立项 F-71（Trae SG）**：本方案的「区域端点结构体 + 独立池」骨架可直接被 F-71 复用，若同期立项可共享阶段 0 侦察产出。

---

## 📚 9. 附录：外部项目情报（2026-10-15 取证）

> 来源：`github.com/ardeyouxipianyi/workbuddy2api-hub`（v1.6.16，Python，定位「WorkBuddy 国际国内多账号反代网关」）与 `github.com/aimod-cc/agent2api`（Rust/Tauri，与本项目同构，`core/endpoints.rs` 自称端点「唯一事实来源」，移植自其 Node 版 `workbuddy-endpoints.mjs`）。两项目相互独立却交叉印证了同一套国际端点，可信度高。

### 🅰️ 附录 A：workbuddy2api-hub（wb_identity.py + README）

**三套出站身份 × 双区域**（`endpoint_for(realm, product)` → (chat base, X-Domain)）：

| 身份 | intl | cn |
|---|---|---|
| WorkBuddy 桌面端（`X-IDE-Type/Name: WorkBuddy`，`X-Product: WorkBuddy`，`X-Agent-Purpose: conversation`） | `www.workbuddy.ai`，UA `WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2` | `copilot.tencent.com`，UA `WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1` |
| VSCode 插件（`X-IDE-Type/Name: VSCode`，`X-Product: SaaS`，`X-Env-ID: production`，`X-Agent-Intent: craft`） | `www.workbuddy.ai`，UA `VSCode/1.119.0 WorkBuddy/4.9.29177644` | `www.workbuddy.cn` |
| CodeBuddy CLI（`X-IDE-Type/Name: CLI`，`X-Product: SaaS`，`X-Agent-Intent: craft`） | `www.workbuddy.ai`（源码注释：**`www.codebuddy.ai` 实测 DNS sinkhole 不解析，CLI 身份 ride `workbuddy.ai` 边缘，接受 CLI 头**），UA `CLI/2.117.2 CodeBuddy/2.117.2` | `copilot.tencent.com` |

关键头集差异（对我方 `build_chat_headers` 的增量）：VSCode/CLI 身份带 `X-Conversation-ID`（**大写 UUID**）、`X-Conversation-Request-ID`、`X-Conversation-Message-ID`、`X-Product-Version`、`X-Env-ID`；企业身份用 `X-Enterprise-Id` + `X-Tenant-Id` 成对。

**运营情报**：OAuth 免客户端登录（看板授权自动入库）、桌面客户端凭证导入 + 加密 token 就地解密、`derive_id` 指纹派生（UID+固定盐 → machine/session，与我方 `derive_device_fingerprint` 同构，长期稳定运行）、**国际版每日活跃打卡**（自动建网页端会话 + 沙箱把一轮对话真正跑完，ACP over HTTP+SSE，领官方每日 30/50 积分）、限额四护栏（`limits: {global, intl, cn}`，null=继承全局）、OpenRouter 价估算（定价按版本留档）。

**模型目录**：源 `GET /v3/config` 的 `agents[cli].models`；**UA 带 CLI 段才下发完整清单（51 个），缺失只给 37 个旧版**（agent2api 同款发现）。Intl 17 个：`hy4-preview-f`、`hy3`、`deepseek-v4.1-flash`、`gpt-6-astra`、`gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`、`gpt-5.5`、`gpt-5.4`、`grok-4.7`、`gemini-3.5-flash`、`glm-5.3-flash`、`glm-5.3`、`glm-5.2`、`kimi-k3`、`kimi-k2.6`、`kimi-k2.8-preview`。CN 14 个（含 `deepseek-v4-pro` 等国内独占）。**同名模型跨区不做混合轮询**（作者理由：网络隔离、出站指纹对齐、防关联风控——与我方不交织原则一致）。

### 🅱️ 附录 B：agent2api（core/endpoints.rs，Edition 配置）

**版本（edition）配置**——两版协议一致、仅端点与客户端身份不同：

| 字段 | cn | intl |
|---|---|---|
| endpoint | `https://copilot.tencent.com`（staging `staging-copilot.tencent.com`） | `https://www.workbuddy.ai`（staging `staging-codebuddy.tencent.com`） |
| prefixPath（鉴权前缀） | `/plugin` | `/plugin`（同款）；**仅 `www.codebuddy.ai` 与 staging-codebuddy 为空串** |
| platform 参数（auth/state） | `workbuddy` | **`workbuddy-ai`** |
| UA（`user_agent_for_edition`） | `WorkBuddy/5.5.4 WorkBuddy/5.5.4 CLI/2.137.1` | `WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/2.137.1`（product_name=WorkBuddy AI） |
| 客户端数据目录 | `.workbuddy` | `.workbuddy-ai` |

**域名白名单**（官方 cli/product.json → authentication.attributes）：internal = copilot.tencent.com 系 6 域；external = `www.codebuddy.ai`、`staging-codebuddy.tencent.com`；ioa（企业 SSO 中转）= `tencent.sso.copilot.tencent.com`、`tencent.sso.codebuddy.cn` 等 4 域。

**鉴权端点表**（`{base}/v2{prefix}/auth/...`）：

| 端点 | 方法 | 关键点 |
|---|---|---|
| `state` | POST `?platform=<platform>` | **匿名**：带 `X-No-Authorization` / `X-No-User-Id` / `X-No-Enterprise-Id` / `X-No-Department-Info: true` 四连 |
| `token` | GET `?state=` | 匿名；未就绪业务码 **11217** |
| `account` | GET `?state=` | 未就绪业务码 **12151** |
| `refresh` | POST | refreshToken 放 **`X-Refresh-Token` 头**（不在 body）；`X-Auth-Refresh-Source: plugin` |
| `accounts` | GET | `data.accounts[]{uid, nickname, enterpriseId, type, pluginEnabled}` |
| `switchAccount` | POST `/login/enterprise`（个人）或 `/login/enterprise/{enterpriseId}`（企业） | |
| `wxToken` | POST `?tmpCode=` | 微信侧换 token，桌面端不用 |

**计费/活动端点**（不带 prefix，直挂 `/v2/...`）：

| 端点 | 方法/请求体 | 响应要点 |
|---|---|---|
| `billing/meter/checkin-activity-status` | POST `{}` | 成功 = `code===0` 且 data 非空（签到状态） |
| `billing/meter/daily-checkin` | POST `{}` | `{code, msg, requestId}` |
| `billing/meter/get-user-resource` | POST `{PageNumber:1, PageSize:100, ProductCode:"p_tcaca", Status:[0,3], OnlyValidPeriod:true}` | `data.Response.Data.Accounts[]`（套餐/加量包明细） |
| `billing/meter/get-enterprise-user-usage` | POST `{}` | `limitNum + credit + cycleResetTime` |
| `activity/workbuddy/banner` | GET | **whitelistHeaders**：缺 `X-Product` 等头被白名单拦截 |
| `activity/ambassador/status` | GET | |
| `v3/config` | GET | `agents[cli].models` 模型目录（CLI 段版本号决定 51/37） |

**ServerErrorCode 表**：`11217` RetryFetchToken、`12151` RetryFetchAccount、`12005` LicenseSeatLimit、`11212` LicenseExpired、`11216` TrialExpired、`10081` IpLimit、`11101` NonStreamNotSupported（**上游仅支持 stream:true**）、`11128` RateLimited。

**其他约定**：`X-Domain` 缺省取端点 authority 兜底；`/v2/chat/completions` 不带 prefixPath；匿名请求头四连与我方 `X-No-*` 占位机制同源（可互为印证）。

### 🎯 对本方案的三点直接修正

1. **国际版身份基座建议选 WorkBuddy 桌面端身份**（`X-IDE-Type: WorkBuddy` + `X-Product: WorkBuddy` + `platform=workbuddy-ai`）：两项目均以桌面端身份为主力，模型目录最全（51），且国际网关对其接受度经过长期实证；CLI 身份作为降级候选。我方现用的国内 CLI 身份（`SaaS` + `platform=CLI`）**不可直接平移**到国际区。
2. **refresh 端点确在国际域**：`www.workbuddy.ai/v2/plugin/auth/token/refresh`，推翻现有代码注释「plugin 网关固定 codebuddy.cn 不随区域」在国际区的适用性；`X-Auth-Refresh-Source` 取值差异（plugin vs workbuddy）列为 R-2a 窄探针。
3. **平台参数是国际登录的灵魂差异**：`?platform=workbuddy-ai`（intl）vs `workbuddy`（cn 桌面）vs `CLI`（cn CLI）——`WbRegionEndpoints` 结构体必须把 platform 纳入字段，不能只配 base URL。
