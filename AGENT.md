# AGENT.md — AI Work 助手 (ai-work-assistant) v1.0.0

> 项目级别速查手册。给后续会话（人或 AI）秒接上下文用。任何会改契约的提交请同步更新本文档。
> **产品形态（2026-09 Web 化转型已落地）**：Web-only 单体——`crates/aiwork-core`（业务核心，零桌面依赖）+ `crates/aiwork-server`（axum：管理面 `/api/*` + 网关 `/v1/*` + 静态托管 + 调度器），浏览器直访，Docker 部署。桌面壳（Tauri/托盘/代理/切换器/豆包/CC Switch/更新器）已整体退役并删除 `src-tauri/`（git 历史归档）。转型决策与裁剪清单见 `docs/tmp/docker-headless-server-plan.md`（ADR-1~4）。

## 1. 一句话

浏览器直访的多账号签到 + 积分查看 + 资源调度 + OpenAI/Anthropic/Codex 兼容 API 网关一站式工作台（React 18 前端 + Rust axum 单体），支持 Trae 与 WorkBuddy 双账号体系，Docker 一键部署。**所有数据仅存服务端 `AIWORK_DATA_DIR`（Docker 内 `/app/data`），出网仅 Trae/腾讯上游（ureq 直连，不读系统代理）。**

## 2. Quick Start

```powershell
# 前端开发调试（后端另起：cargo run -p aiwork-server，vite 代理 /api /v1 → 127.0.0.1:7864）
npm install
npm run dev

# 后端
cargo run -p aiwork-server       # 监听 127.0.0.1:7864；AIWORK_LISTEN_ADDR 可配；数据目录默认 /data/AIWorkAssistant（Windows 按启动盘符解析，AIWORK_DATA_DIR 可覆盖）
cargo test --workspace           # Rust 单测（core 304+ / server 12+）

# 生产部署
docker compose up -d --build     # 详见 docs/server-deploy.md
```

## 3. 技术栈

| 层 | 技术 |
|---|---|
| 后端 | Rust 1.88（workspace：aiwork-core + aiwork-server，axum 0.7 / tokio / ureq / rusqlite bundled / iota_stronghold） |
| 前端 | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 + lucide-react |
| 部署 | Docker 多阶段构建（node:20 → rust:1.88-slim → debian:bookworm-slim）+ docker-compose |
| 辅助 | Node.js 18+（scripts/*.mjs，零 npm 依赖） |

## 4. 目录地图

```
ai-work-assistant/
├── AGENT.md                      # 本文件（项目速查）
├── Cargo.toml                    # workspace（aiwork-core + aiwork-server）
├── package.json / vite.config.ts / tsconfig.json / tailwind.config.js / postcss.config.js / index.html
├── Dockerfile / docker-compose.yml / .dockerignore   # 部署
├── docs/                         # server-deploy.md（部署）/ user-manual（手册）/ tech-framework（架构）/ product-design（设计）/ backlog（待办）/ tmp（转型计划）
├── src/                          # 前端（复用桌面版页面骨架，lib/tauri.ts 单点适配 REST + WS/SSE 实时层）
│   ├── App.tsx                   # 登录 gate（authed）+ Sidebar + 页面切换 + Toaster
│   ├── store.ts                  # Zustand 单一真相（init/afterLogin/resetAuth + 401 全局拦截 + checkin SSE 归约）
│   ├── types.ts                  # 与 Rust DTO 对齐（snake_case）
│   ├── lib/tauri.ts              # invoke→POST /api/cmd/{name} + 实时 listen（WS 优先/SSE 回退）+ login（唯一适配层）
│   ├── components/               # Sidebar/Toaster/PageHeader/ui + SystemDialog/GeneralSettingsPanel/AboutDialog/BrandMark
│   └── pages/                    # Login / Dashboard / Accounts(accounts/ 子组件) / Checkin / Credits / Logs / ApiService / Settings / buddy/*
├── crates/
│   ├── aiwork-core/              # 业务核心：state/models/store/tasks/{trae_checkin,wb_checkin,wb_common,wb_credits,scheduler}/api_server/* + notify.rs（Bark/Server酱/webhook）
│   └── aiwork-server/            # axum 单体：main.rs(启动序+--task-run) / admin(cmd_bridge 白名单命令桥 + sse + ws 双向推送 + ip_allow + admin_tokens + 鉴权) / static_files.rs
└── scripts/                      # sync_version.mjs(版本单源同步) / gen_asset_base64.mjs(promo banner)
```

## 5. 管理面命令桥契约（Web 版权威）

**调用约定**：前端一律经 `src/lib/tauri.ts` 的 `invoke(name, args)` → `POST /api/cmd/{name}`（body JSON）；**顶层参数名** camelCase 自动转 snake_case，**嵌套对象**（`opts`/`patch`）字段保持 snake_case。响应 `{"ok":true,"data":...}` / `{"ok":false,"error":...}`；未登录 401（前端全局拦截回登录页）；**未注册命令 404（fail-closed 安全特性）**。

**权威白名单**：`crates/aiwork-server/src/admin/cmd_bridge.rs` 的 dispatch 表（90+ 命令）。按模块概览：

| 模块 | 命令（前缀/代表） | 说明 |
|---|---|---|
| 账号 | `accounts_list` / `account_add_manual` / `account_delete` / `account_update` / `account_get_jwt` / `accounts_export_raw` / `accounts_import` / `accounts_import_preview` | CRUD + 掩码下发（完整 JWT 按需取）+ 导入导出（导入前预览） |
| OAuth | `oauth_get_login_url` / `oauth_parse_callback` / `oauth_login` | Trae 粘贴回调模式：取登录链接 → 任意设备登录 → 复制回调 URL → 解析入池 |
| 分组 | `groups_list` / `group_create` / `group_update` / `group_delete` / `group_move` | 删除分组时账号回落「未分组」 |
| 积分 | `fetch_remaining_credits` / `fetch_credit_detail` / `refresh_remaining_credits` / `credits_daily_list` | 查询 / 明细 / 批量刷新（快照任务共用）/ 每日快照列表 |
| 冷却 | `cooldown_clear` / `cooldown_clear_all` | 清除签到错误冷却 |
| JWT | `jwt_parse` / `refresh_jwt` | 解析 / 自动续期（48h lazy gate，需 refresh_token） |
| 签到 | `checkin_start(opts)` / `checkin_trends(days)` | `opts: {scope, user_ids?, skip_checked_in, skip_expired}`；失败自动重试 2 轮（30s/90s）；进度走 SSE |
| WorkBuddy | `workbuddy_accounts_*` / `workbuddy_checkin_start` / `workbuddy_growth_run` / `workbuddy_checkin_results` / `workbuddy_credits_fetch` / `workbuddy_settings_*` / `workbuddy_oauth_login` / `workbuddy_usage_official` / `workbuddy_usage_fallback` / `workbuddy_activity_info` / `workbuddy_token_stats` | WB 账号/签到/成长中心/积分/官方用量（31 天分页）/快照回退/活动信息/本地 Token 统计；OAuth 为 authUrl+state 轮询（≤300s，纯 HTTP 无需粘贴） |
| API 网关 | `pool_list/set/status` / `api_keys_list/save` / `api_models_list/sync` / `api_unified_models` / `api_custom_models_*` / `dispatch_policy_*` / `gateway_settings_*` / `trae_model_meta_*` | 三池调度策略（smart/priority + per_model 覆盖）、ck_ 子 Key、模型目录三源合并（四层兜底）、自定义上游、网关设置；网关常驻随服务启停（无启停命令） |
| 用量/日志 | `api_usage_stats` / `api_wb_usage_stats` / `api_custom_usage_stats` / `api_logs_list/detail/search` / `logs_query` / `logs_clear` | 按日统计（分池）、请求日志、运行日志 |
| 调度 | `scheduler_status` / `scheduler_config_get` / `scheduler_config_set(config)` | 任务状态（kv `scheduler_state`）+ 任务配置（kv `scheduler_cfg`：`disabled_tasks` 停用名单 + `task_times` 自定义时刻 HH:MM，未知键/非法时刻整体拒绝；缺省全开 + 默认时刻 = 推荐配置；set 为整表替换，前端须同时携带两字段） |
| 通知 | `notify_config_get` / `notify_config_set` / `notify_test` | Bark / Server酱 / 通用 webhook 三渠道配置与测试（T11） |
| 安全 | `ip_allowlist_get` / `ip_allowlist_set` | IP 允许列表（T12a）：enabled/trust_proxy/cidrs；保存即热生效，无效 CIDR 整体拒绝 |
| 管理员令牌 | `admin_tokens_list` / `admin_token_create(label)` / `admin_token_revoke(id)` | 附加可吊销令牌（T12b）：list 掩码、create 返回明文仅一次、上限 20 个 |
| 设置 | `settings_get` / `settings_set` | Settings 全字段 snake_case |

**不含**（已随桌面退役）：proxy/cert/switch/process/env/trae_apps/ccswitch/doubao/updater/autostart/profile/oauth_loopback/task_register（schtasks）。

### 实时事件推送：WebSocket 优先，SSE 回退（T12c）

**WS 端点 `GET /api/ws`**（cookie 鉴权通过后升级，`admin/ws.rs`）：
- 服务端把 broadcast 事件以 `{"event":...,"payload":...}` JSON 文本帧推送；
- 客户端控制帧：`{"type":"ping"}` → `{"type":"pong"}`（前端 30s 心跳防反代 idle）；
  `{"type":"subscribe"|"unsubscribe","events":[...]}` 事件过滤（默认全量）；
- 慢消费者丢帧下发 `{"event":"ws-lagged","payload":{"missed":N}}`；
- 前端 `src/lib/tauri.ts`：WS 优先（断开 30s 重试），SSE 自动回退，对外 `listen()` 签名不变。

### SSE 事件（`GET /api/events/checkin`，WS 不可用时的回退通道）

| 事件 | payload |
|---|---|
| `checkin-progress` / `checkin-done` | `{type:'start',total}` / `{type:'account',...}` / `{type:'done',ok,already,failed}`（别名归一分发） |
| `wb-checkin-progress` | `{type:"start"/"account"/"growth"/"done"/"exit",...}`（WB 签到/成长独立管线） |
| `wb-oauth-progress` / `wb-oauth-done` | `{stage,message,auth_url?}` / `{ok,id?,nickname?,message}` |

## 6. 调度器与通知

**单后台线程 60s tick**（`aiwork-core/src/scheduler.rs`），触发语义 `今天已过触发时刻 && 今天未跑`；服务重启后自动补跑当天错过的任务；失败记 `last_fail_ts`，30 分钟冷却自动重试；全部任务幂等。

| 任务 | 每日触发 | 说明 |
|---|---|---|
| `trae-jwt-renew` | 05:30 | 遍历 vault 账号 `refresh_jwt_impl(force=false)`，48h lazy gate 内置（未临期零网络请求）；invalid 计入「需重新 OAuth」摘要 |
| `models-sync` | 05:40 | 官网模型列表每日同步（batch_get_detail_param，不消耗积分）；无可用账号时跳过不计失败 |
| `trae-checkin` | 09:00 | Trae 全账号签到（状态核验幂等） |
| `wb-checkin` | 09:10 | WB 签到+成长中心；跟随「启动自动补签」开关；与手动路径抢轮次锁互斥 |
| `wb-renew` | 10:30 | WB token 兜底续期（lazy 24h） |
| `wb-credits-snapshot` | 23:30 | WB 积分快照（近 7 日消耗差分数据源） |
| `trae-credits-snapshot` | 23:40 | Trae 积分快照 |

状态落 SQLite kv `scheduler_state`，前端 `scheduler_status` 查看（`time` 字段为生效时刻）。任务配置：kv `scheduler_cfg`（前端 `scheduler_config_get/set`）——`disabled_tasks` 停用名单、`task_times` 自定义每日触发时刻（触发判定用生效时刻，自定义早于当前时刻会当天补跑）；`enabled` 与既有设置语义合成（`wb-checkin` 仍跟随「启动自动补签」）。`--task-run <name>` CLI 兜底（单任务执行后退出）。

**通知渠道**（`notify.rs`，kv `notify_config`）：Bark / Server酱 / 通用 webhook 三渠道顺序推送；触发点=签到完成（仅签到类任务+手动签到）与任务失败；总开关默认关；单渠道失败仅记 `app_log` 不阻塞主流程（旁路语义）。

## 7. 数据与目录（server 视角）

```
$AIWORK_DATA_DIR（Docker: /app/data）
├── conf/
│   ├── admin_token        # 管理面令牌（AIWORK_ADMIN_TOKEN 未注入时首启生成 64 位 hex）
│   ├── vault_key.bin      # vault 主密钥（0600；AIWORK_VAULT_KEY 优先）——丢失即凭据不可解密
│   └── vault.stronghold   # jwt/refresh_token 权威加密存储
├── data/
│   └── aiwork.sqlite      # 全量状态库（WAL）：kv 文档表 + 行文档实体表 + 列化流水表
│                          #   kv：app_settings/api_pool/dispatch_policy/api_models/wb_model_catalog/
│                          #       wb_model_route/wb_template_map/workbuddy_settings/notify_config/
│                          #       ip_allowlist/admin_tokens/scheduler_state 等
└── logs/                  # checkin / api / app.log（按保留天数自动清理）
```

写入约定：SQLite 经 `store::db(data_dir)` 单连接串行访问（WAL + busy_timeout 5000）；日志/导出走 `fs_utils`（tmp + rename 原子替换）。桌面版 JSON→SQLite 迁移与 profiles/certs 等目录已随桌面壳退役。

## 8. API 网关契约（Web 版完整保留）

- **三池**：`trae`（SOLO `llm_utils_chat`，通用积分 208）/ `buddy`（copilot.tencent.com 或 www.workbuddy.ai，按账号 domain 判定）/ `custom`（自定义 OpenAI 兼容上游，**命中即直达、不参与池间策略**）。
- **池间策略**（`dispatch.rs`）：`smart` 默认（到期优先→倍率→健康积分和）；`priority` 严格按序；`per_model` 显式覆盖不做智能重排；`fallback` 跨池回退开关。
- **WB headers 三铁律（红线）**：① Origin/Referer 按区域必带；② 缺省字段显式 `X-No-User-Id` 等占位；③ **chat 请求绝不携带 `X-Refresh-Token`**（仅 refresh 端点）。
- **请求体改写**（`wb_payload.rs`）：强制 `stream:true`（非流式本地聚合）；reasoning_effort 按目录降级；审核指纹清洗（cc_xxx 剥离 + 模板黑名单改写，`wb_template_map` mtime 热更新）。
- **分级重试**（`retry.rs`）：429 Retry-After 优先→耗尽换号；503/529 指数退避；401/403 换号（WB 401 先刷新一次）；Fatal 透传。SOLO 与 WB 共用同一 `retry_plan`。
- **会话粘性**（仅 WB）：显式 conversation_id 绑定（TTL 30m）+ 无 id 指纹模式；五态机 + 模型级/熔断冷却。
- **四段模型路由**（`wb_model_route.json`）：aliases/rules/suffixes → 别名/通配/系列/`-thinking` 后缀注入 effort=high；全未命中回落原名。
- **ck_ 子 Key**：对外子 Key 与上游凭证分离；`allowed_accounts` 白名单 / `schedule_mode`（expire_first/dedicated）/ 每日统计。
- **端点**：`/v1/chat/completions`、`/v1/completions`、`/v1/messages`（Anthropic）、`/v1/responses`（Codex，仅 WB）、`/v1/images/generations|edits`、`/v1/models`、`/v1/embeddings`（固定 501）、`/status`、`/health`、`/healthz`。
- **鉴权**：api_keys fail-closed（未配置 key 全拒）；`Authorization: Bearer` 与 `x-api-key` 双风格。
- 协议细节与抓包实证见 `docs/tech-framework.md` 附录 A/B。

## 9. 前端约定

- **登录 gate**：`App.tsx` 按 `authed` 渲染 Login 或主界面；`invoke` 收 401 派发 `UNAUTHORIZED_EVENT`，store 监听回登录页。
- **store 单例**：`useAppStore` 聚合所有状态；`init()` 在 `App.tsx` `useEffect` 启动一次；`afterLogin` 拉取账号/分组/调度状态。
- **适配层唯一**：所有命令经 `src/lib/tauri.ts`（401 拦截/camelCase 转换/SSE 收敛于此），禁止组件直接 fetch `/api/*`。
- **主题系统**：Tailwind 3 + `darkMode:'class'`；`lib/themes.ts` 6 套主题经 `data-theme` 驱动，与 `index.css` 覆盖块一一对应——新增主题两处同步。
- **snake_case**：`types.ts` 字段名与 Rust DTO 完全一致。
- **路由**：极简 `useState`，不引 react-router。
- **Modal**：不支持 `window.confirm()`，用自定义 `Modal`（支持 `size="lg"|"xl"`）。
- **按钮反馈**：异步按钮动作 `withMinDelay(promise, 1000)` 确保最少 1 秒 loading；操作结果用 Toaster 提示。

## 10. 安全与合规

- **零外发**：不连接任何自有后端；出网仅 `api.trae.cn` / `trae-api-cn.mchost.guru` / `copilot.tencent.com` 等上游（合成指纹，不读系统代理）。
- **管理面**：主 token（env/文件）+ 附加可吊销令牌（T12b）并集校验 → HttpOnly cookie 会话（7 天，存登录所用 token）；未登录 `/api/*` 一律 401；`/health`、`/healthz` 公开。
- **IP 允许列表**（T12a，应用层）：启用后仅列表内 CIDR 可访问 `/api/*`、`/v1/*` 与静态资源；回环始终放行防自锁，`trust_proxy=false` 时仅信 TCP 对端（防伪造头绕过）；与反代层 allow/deny 可叠加。
- **网关**：api_keys fail-closed；`/v1/*` 与 `/api/*` 鉴权域分离。
- **凭证展示**：列表接口 JWT 掩码下发（前4+****+后4），完整值经 `account_get_jwt` 按需取；uid 作路径段过 `fs_utils::ensure_uid_safe` 白名单。
- **vault 红线**：禁止明文 jwt/refresh_token 落盘；`vault_key.bin`（或 `AIWORK_VAULT_KEY`）丢失 = 全部凭据不可解密。

## 11. 禁止与红线（Do NOT）

- ❌ 修改 Rust 命令嵌套参数（如 `CheckinOpts`）的字段名 → 前端 invoke 载荷与 `--task-run` CLI 序列化均按字段名匹配，改名即断链。
- ❌ 命令桥白名单外新增命令不改 `cmd_bridge.rs` dispatch → 未注册命令 404（fail-closed 是安全特性）。
- ❌ 引入 React Router / Redux / 额外 UI 库 → 保持依赖最小。
- ❌ 提交 `dist/`、`node_modules/`、`target/`、`data/`（已在 `.gitignore`）。
- ❌ 使用 `window.confirm()` → 用自定义 Modal。
- ❌ 前端绕过 `src/lib/tauri.ts` 直接调 `/api/*`。
- ❌ WB chat 请求携带 `X-Refresh-Token`、或丢弃 `X-No-User-Id` 占位（三铁律）。

## 12. 已知约束

- JWT 默认约 13 天过期；带 refresh_token 的账号走 48h lazy 续期（调度任务 05:30 兜底）；refresh_token 被吊销只能重新 OAuth。
- Trae OAuth 为**粘贴回调模式**：浏览器跳 `http://127.0.0.1:17388/authorize` 停在「无法连接」页是**预期现象**，用户复制地址栏 URL 粘贴回 Web UI。
- LLM 上游请求 ureq 直连，不读系统/环境代理（容器内行为确定）。
- 调度按本地时间 HH:MM 触发，容器需 `TZ=Asia/Shanghai`。
- API 模型同步重放官网配置接口；内置模型（glm-5.3-flash 等）需 llm_utils_chat 以 `function=solo_agent` 补齐。
- WB 官方用量上游 prompt/input 字段一律不复制（脱敏红线）。
- SQLite WAL 模式下 `-wal/-shm` 常驻属正常。

## 13. 版本号升级规则

语义化版本 `MAJOR.MINOR.PATCH`。**升版时机（红线）：只有用户明确说「升级版本」时才升版**；日常提交/bug 修复一律不动版本号。

- 版本号**单一来源**为 `crates/aiwork-core/Cargo.toml`：执行 `npm run set-version <x.y.z>` 一键同步（底层 `scripts/sync_version.mjs`：server crate / package.json / AGENT.md 标题）。
- 新完整功能升 MINOR，修复/优化升 PATCH；多类变更按最高级别；纯文档改动不升级。
- GitHub Release 标题固定格式：`v{MAJOR}.{MINOR}.{PATCH} 版本发布`。

## 14. Git 引用静默丢失坑（2026-09-13 事故复盘，每次提交必守）

> 环境：WorkBuddy 便携版 Git。**嵌套分支 ref**（如 `refs/heads/feature/buddy`）若仅存于 `packed-refs`（loose 文件已被收编），则 `git commit` 会**报成功但分支指针静默回滚**到 packed-refs 旧值——提交对象与 reflog 正常入库，唯 ref 落盘丢失，下一次提交挂在旧父节点上（实例：e14ee2d 孤儿 + 6c1fdca 错父）。顶层 ref 写入正常，仅嵌套 ref 触发。

**规避规范（红线，逐条必守）**：

1. **提交后必校验**：每次 `git commit` 后立刻比对 `git rev-parse HEAD` 与 `git rev-parse <当前分支>`，不一致 = 指针回滚，按第 4 条修复后再继续。
2. **提交前查 loose 状态**：目标分支若在 packed-refs 中且对应 loose 文件不存在，提交风险最高；优先在顶层分支操作。
3. **整树提交，不用 pathspec 部分提交**：统一 `git add <paths>` + `git commit`（不带 `-- <paths>` 后缀）。
4. **修复只走 packed-refs 原位替换**：`git update-ref` 与直写 loose 文件在本环境均会被静默丢弃，**唯一可靠手段**是用 Python 原位替换 `packed-refs` 中该分支行；替换后 `git rev-parse` 读回验证。
5. **孤儿提交勿清理**：`git gc` / `git prune` 一律不跑；删除文件禁用裸 `rm`。
6. **慎用 `git pack-refs --all`**：它会把 loose ref 收编进 packed-refs，正是制造本坑的前提。
