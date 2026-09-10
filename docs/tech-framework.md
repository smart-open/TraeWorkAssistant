# 技术架构设计 — Trae Work 助手

> 适用版本：v2.9.1 ｜ 平台：**仅 Windows 10 / 11**
> 本文档为技术侧单一入口，整合了原《技术框架方案》《API 命令契约（api-doc）》《运行手册（开发与构建部分）》
> 与《Trae API 积分与接口分析》的协议要点，以及两份**待实施**的专项设计（§8）。
> 界面 / 交互 / 数据模型的产品视角描述见 `product-design.md`，最终用户操作指南见 `user-manual.md`。
> 版本脉络：v2.0 本地 API 网关（axum、SSE 转换、账号池调度、冷却状态机、6 层设备重置）→
> v2.1 每日积分快照 / 暗色图表 / Mutex 安全锁 / 代理日志竞态修复 → v2.9.0 T1-T11（用量统计、多 API Key、
> 托盘增强、vault 加密、签到重试、日志清理、签到趋势、completions 端点、池调度策略、开机自启、移除主 Key）。

## 1. 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| UI | React 18 + TypeScript + Tailwind CSS + shadcn/ui + Recharts + Zustand | Web 技术栈可完整还原设计稿；状态用 Zustand；图表用 Recharts |
| 外壳 | Tauri 2.x（Rust） | 包体 8~15MB（远小于 Electron），可调用系统 API（注册表/证书/计划任务） |
| 核心逻辑 | Python 3.13（`auto_checkin.py` / `device_proxy.py`，迁移并增强） | 复用已验证的签到/代理逻辑，降低重写风险 |
| 登录态切换 | PowerShell（`trae-switch-bridge.ps1`，含 6 层设备标识重置） | 复用已验证的备份/恢复/机器码重置逻辑 |
| API 网关 | Rust axum（内嵌，复用 Tauri tokio runtime） | OpenAI 兼容端点 + SSE 转换 + 账号池调度，无需独立进程 |
| HTTP 客户端 | ureq（同步）+ spawn_blocking 包装 | 双 Client 设计：短请求 120s 超时 / 流式仅 ResponseHeaderTimeout |
| 凭据存储 | Stronghold vault + Windows DPAPI（`vault.rs`，v2.9.0） | jwt / refresh_token 加密落盘，主密码仅本机当前用户可解 |
| 打包 | Tauri Bundler → MSI / NSIS 单文件 | 含 Python embeddable 运行时与 PS 脚本 |

**不采用**：Electron（体积过大）、WPF/WinUI（样式成本高）、PyQt（视觉不达要求）。

## 2. 架构分层

```
Presentation  React + Tailwind（概览/账号/签到/积分/日志/设置/API 服务）
      │  Tauri invoke
State         Zustand + Tauri Event Bus（账号/任务/日志/环境）
      │
Bridge        Tauri Commands (Rust)
 ├─ fs         JSON 读写（tmp+rename 原子替换 + 文件锁）
 ├─ vault      Stronghold + DPAPI 凭据存取（v2.9.0）
 ├─ proc       子进程管理（spawn/kill/stdout 流）
 ├─ sys        TW 检测 / CA 检测与安装 / UAC / 计划任务 / 开机自启
 ├─ jwt        JWT 解析（exp / data.id），不校验签名
 ├─ api_server axum 内嵌 HTTP 服务（/v1/chat/completions 等 + SSE 转换）
 ├─ pool       账号池调度（策略 + 冷却状态机 + 轮转）
 └─ watch      文件监听（accounts.json 变更→推事件）
      │
Python Core   auto_checkin.py（签到 + 错误分类冷却） / device_proxy.py（代理 + mchost.guru 监听）
PowerShell     trae-switch-bridge.ps1（登录态切换 + 6 层设备标识重置）
```

### 2.1 前端实现要点

- **启动加载态**：`App.tsx` 在 store `ready` 为 `false` 时渲染全局 Loading，待 `init()` 完成后才挂载主界面。
- **设置页（显式保存）**：本地表单状态 + `dirty` 标记，点击「保存」才落盘；`saveSettings` 失败自动回滚。
- **签到页**：候选账号表格 + 全部/分组/手动勾选三种范围 + 跳过规则；v2.9.0 增加重试轮横幅（倒计时）。
- **日志页**：实时代理输出最新置顶（最多 200 行）+ 查询日志（按类型/日期/关键字）；v2.9.0 增加按类型清理。
- **Modal 组件**：`Escape` 关闭 + 锁定 `body` 滚动，防止滚动穿透。
- **代理日志竞态修复（v2.1）**：`showDetail` 使用 `useRef` 递增请求 ID，关闭弹窗后使进行中的请求失效。
- **暗色模式图表（v2.1）**：`useIsDark()` hook 动态适配 Recharts 颜色。
- **积分趋势三线图（v2.1）**：数据源 `credits_daily.json`（total / earned / consumed）。

### 2.2 API 网关实现要点（v2.9.0 T1-T11）

- **用量统计（T1）**：`data/api_usage.json` 按日落盘（模型/上游账号/API Key/流式/成败/耗时/token 多维聚合，保留 90 天），`api_usage_stats(days)` 直读落盘（服务未运行也可查）。
- **多 API Key（T2/T15）**：`data/api_keys.json` 多 Key 独立签发 + 每日配额（0=不限，跨天重置，超限 429）；每请求重读文件，增删/启停立即生效；主 Key 双轨已移除，鉴权统一走 Key 列表（未配置启用 Key 时默认拒绝所有业务请求并返回 401 auth_not_configured，防本机任意进程无鉴权消耗上游额度；仅 /health 豁免）。
- **签到重试（T5）**：最多 2 轮（30s/90s），仅重试失败账号；`CheckinGuard`（tokio::sync::Mutex）应用级防重入，页面/托盘/静默签到共用；per-uid 最终态合并保证 `ok+already+failed==total`。
- **凭据加密（T4）**：jwt/refresh_token 迁入 Stronghold vault（`conf/vault.stronghold`），主密码经 DPAPI 保护（`conf/vault_key.bin`）；JSON 落盘占位化；vault 写失败降级明文 + 下次启动重试迁移；Python 签到走临时解密文件（`--accounts-file`，用后即删 + 启动清理崩溃残留）。
- **池调度策略（T10）**：`api_pool.json` 扩展 `strategy`（expire_first / credit_first / random）与 `group_ids`（空=全部参与）；策略纯函数化，分组同步期过滤。
- **端点扩展（T9）**：`POST /v1/completions`（legacy 文本补全，prompt 转 user message 复用对话链路）；`/v1/embeddings` 返回 501。
- **自启与静默签到（T11）**：`tauri-plugin-autostart` + 启动延迟 60s 对未签到账号自动签到（复用统一链路，`skip_checked_in=true` 幂等，完成发系统通知）。

## 3. 数据模型

统一存于 `%APPDATA%\TraeWorkAssistant\`：

| 文件 | 来源 | 说明 |
|---|---|---|
| `conf/app_settings.json` | v1.0 | 应用设置（含 `silent_checkin`，`api_key` 字段已移除） |
| `conf/vault.stronghold` | v2.9.0 | Stronghold 快照：jwt / refresh_token 权威存储（按 uid 键） |
| `conf/vault_key.bin` | v2.9.0 | DPAPI 加密的 vault 主密码（32 字节，仅本机当前用户可解） |
| `data/checkin_accounts.json` | 沿用 | 账号元数据（凭据已占位化，明文权威在 vault） |
| `data/device_map.json` | 沿用 | user_id → 虚拟设备身份 |
| `data/groups.json` | v1.0 | 分组定义 + membership（UserID→groupId） |
| `data/checkin_summary.json` | 沿用 | 最近一次签到结果 |
| `data/checkin_results.json` | v2.9.0 | 签到最终态按日落库（保留 90 天，趋势图数据源） |
| `data/credits_history.json` | v1.0 | 积分历史（看板绘图，自动裁剪 90 天） |
| `data/credits_daily.json` | v2.1 | 每日积分快照（total/earned/consumed 三线趋势） |
| `data/account_cooldowns.json` | v2.0 | 签到错误冷却状态（error_type + cooldown_until） |
| `data/remaining_credits.json` | v2.0 | 各账号剩余积分缓存 |
| `data/api_pool.json` | v2.0 | API 账号池（`enabled_uids` + v2.9.0 扩展 `strategy`/`group_ids`） |
| `data/api_usage.json` | v2.9.0 | 网关用量按日统计（保留 90 天） |
| `data/api_keys.json` | v2.9.0 | API Key 列表 + 当日用量记账 |
| `data/certs/` | 沿用 | 自签 CA |
| `data/profiles/<user_id>/` | 沿用 | 各账号 TW 登录态备份 |
| `logs/` | v1.0 | proxy.log / checkin.log / switcher.log / api 请求日志 / 代理请求日志 |

账号唯一主键：`UserID`（JWT payload `data.id`，16 位数字）。

### 3.1 签到错误冷却状态机（v2.0）

按 HTTP 状态码和响应体分类签到错误，每种类型对应不同冷却策略：

| 错误类型 | 触发条件 | 冷却时长 | 账号池处理 |
|---------|---------|---------|-----------|
| `PlanLimit` | 响应体含 `code:1005` | 12 小时 | 换号重试 |
| `SoftRate` | HTTP 429 | 60 秒 | 换号重试 |
| `SessionDead` | HTTP 401 | 永久（需重登） | 换号重试 |
| `NotFound` | HTTP 404 | 60 秒 | 换号重试 |
| `Server` | HTTP 5xx | 10 分钟（累计） | 换号重试 |
| `Client` | 其他 4xx | 10 分钟（累计） | 换号重试 |

冷却状态持久化到 `account_cooldowns.json`。签到成功且积分 > 0 时自动清除冷却（SessionDead 除外）。

### 3.2 API 账号池调度

1. 跳过禁用/冷却中/积分已过期/零积分账号（v2.9.0 起先按 `group_ids` 过滤分组外账号）
2. 按策略挑选：`expire_first`（默认，过期时间升序→积分降序）/ `credit_first`（剩余积分降序）/ `random`
3. 单请求最多换号 3 次（`MaxRotate`）；错误联动冷却状态机

状态持久化到 `api_pool.json`，重启后冷却状态保持。

### 3.3 每日积分快照（v2.1）

每天写入 `credits_daily.json` 一条快照：`date`（YYYY-MM-DD）、`total`（各账号剩余积分之和）、
`earned`（签到获得 + 非签到获得：`user_entitlement_pack_list` 中 `start_time` 在今日且 `package_source_type != 9` 的积分包累加）、
`consumed`（`|total - earned - 昨日total|`）。

### 3.4 Mutex 安全锁模式（v2.1）

Rust 后端统一 `safe_lock()` 替代 `Mutex::lock().unwrap()`，锁毒化时 `unwrap_or_else(|e| e.into_inner())` 恢复数据继续运行；`Response::builder()...body().unwrap()` 统一替换为 fallback，防 panic。

## 4. 进程与子进程契约

### 4.1 `auto_checkin.py`

- 沿用：读 `checkin_accounts.json` → 逐账号 `status_check`+`signin` → 写 `checkin_summary.json`。
- 参数（向后兼容）：`--json-stream`（NDJSON 逐条输出）、`--accounts UID1,UID2`、`--scope all|group:<id>`、`--accounts-file <path>`（v2.9.0，vault 临时解密文件）。
- NDJSON 示例：
  ```
  {"type":"start","total":6}
  {"type":"account","index":2,"user_id":"1556…","name":"青衣网络","status":"success","delta":300,"elapsed":1.24}
  {"type":"account","index":3,"user_id":"…","status":"fail","code":1001,"message":"JWT 无效","error_type":"SessionDead","cooldown_until":null}
  {"type":"done","ok":5,"already":0,"failed":1}
  ```
- `status` 取值：`already` / `success` / `fail`；`fail` 行附带 `error_type` 与 `cooldown_until`。
- 每次签到把 `{date, user_id, credits, delta}` 追加写入 `credits_history.json`（已签/失败也写，delta=0）。

### 4.2 `device_proxy.py`

- 环境变量：`PROXY_PORT`（默认 8899）、`AUTO_CAPTURE_JWT`（默认 1）。
- 行为：透明 MITM；捕获 `trae.cn` / `trae.com.cn` 带 `Cloud-IDE-JWT` 的请求写回账号文件（按 UserID 匹配，exp 防降级）；仅对 `checkin_credits/claim` 改写设备头。
- v2.0：新增 `mchost.guru` 监听域名（MITM 解密 TRAE 对话流量）；WebSocket 隧道转发（不记录消息内容）；代理请求日志按日期分割。
- 日志：`proxy.log`（实时）+ `logs/proxy_req_YYYY-MM-DD.log`（结构化，支持关键字/时间段查询），关键标记 `[JWT 自动更新]` / `[JWT 自动追加新账号]`。
- **SSE 流式转发**：`_stream_response` 用 chunked 逐块转发（请求头含 `accept: text/event-stream` 时触发），避免全量缓冲导致客户端超时。

### 4.3 `trae-switch-bridge.ps1`

- **非交互模式**：`-Action <Switch|Save|New|Reset|List|ResetDeviceIds>` + `-Json` + `-UserId <id>`，NDJSON 输出每步进度。
- `ResetDeviceIds`：6 层设备标识重置（machineid 文件、storage.json telemetry/sqmId/aha.device.device_id、aha/TinyStorage、HKLM MachineGuid、trae-webview 追踪数据、`has_device_id_updated_to_aha` 标记位）；多策略 TRAE 安装路径探测。
- 需管理员权限（重置 MachineGuid），Rust 侧 runas 提权启动。
- **编码要求**：PowerShell 5 需 UTF-8 with BOM + CRLF 行尾（LF 无 BOM 会导致中文解析错误）。

## 5. 外部 Trae API 协议参考

> 整合自原《Trae API 积分消耗与接口分析》（全文见 git 历史）。逆向调研结论，供网关维护与后续演进参考。

### 5.1 积分体系

| 属性 | IDE 积分 | Work 积分 |
|------|----------|-----------|
| product_id | 208 | 209 |
| 消耗接口 | `/api/agent/v3/llm_utils_chat` | `/api/agent/v3/create_agent_task` |
| 使用场景 | Trae Code (IDE) 内 AI 对话 | Trae Work (SOLO) 的 Agent 任务 |
| 获取方式 | 订阅 IDE 套餐 | 订阅 Work 套餐 / 每日签到 / 购买 |

**当前决策（2026-08-14）**：API 网关使用 `llm_utils_chat`（消耗 IDE 积分）。`create_agent_task`（Work 积分）
已证伪外部复刻——请求体由原生层 `ai_agent.dll` 构造、闭源 `@aha-kit` 加密，真实身份复刻仍返回
`4001 failed to get summary template data`。详见 §8.2 Work 积分资源池设计。

### 5.2 域名与端点

| 域名 | 端点 | 协议 | 用途 |
|------|------|------|------|
| `api5-normal.mchost.guru` | `POST /api/agent/v3/llm_utils_chat` | HTTP + SSE | 网关上游（IDE 积分，`AGENT_HOST` 常量） |
| `trae-api-cn.mchost.guru` | — | HTTPS | Referer / 页面域名（`REFERER_BASE` 常量） |
| `api.trae.cn` | `POST /trae/api/v2/ug/checkin_credits/claim` | HTTP + JSON | 执行签到 |
| `api.trae.cn` | `POST /trae/api/v2/ug/checkin_credits/status` | HTTP + JSON | 签到状态 |
| `api.trae.cn` | `POST /trae/api/v2/pay/ide_user_ent_usage` | HTTP + JSON | 积分/权益查询 |
| `api.trae.com.cn` | `POST /cloudide/api/v3/trae/oauth/ExchangeToken` | HTTP + JSON | Token 刷新 |
| `api.trae.com.cn` | `POST /cloudide/api/v3/trae/GetUserInfo` | HTTP + JSON | 用户信息 |
| `www.trae.cn` | `GET /authorization` | HTTPS | 登录授权页 |

**认证方式**：`Authorization: Cloud-IDE-JWT <accessToken>`，附带 `X-Cloudide-Token`、`X-Ide-Token`、`X-App-Id`、`X-Ide-Version`、`X-Device-Id` 等请求头。

**SOLO SSE 自定义事件**（由 `sse.rs` 转换为 OpenAI chunk 格式）：

```
event:metadata        → 会话元数据（session_id, model 等）
event:timing_cost     → 耗时统计
event:output          → ×N，增量内容（response / reasoning_content / tool_calls）
event:extra_info      → 额外信息
event:token_usage     → token 统计（prompt_tokens / completion_tokens / total_tokens）
event:done            → 结束信号（finish_reason）
event:error           → 流内错误（code:1005 等）
```

### 5.3 错误分类（网关侧 ErrKind）

| 错误类型 | 触发条件 | 冷却时长 |
|----------|----------|----------|
| `PlanLimit` | `code=1005` 或 message 含 "plan" | 12 小时 |
| `SoftRate` | `code=4008` 或 429 或含 "quota"/"rate" | 60 秒（4008 短冷却避免误杀） |
| `SessionDead` | HTTP 401 或 code=401 | 禁用（需重登） |
| `NotFound` | HTTP 404 | 60 秒 |
| `Server` | HTTP 5xx | 10 分钟（累计） |
| `Client` | 其他 4xx | 10 分钟（累计） |
| 连续错误 | 连续 3 次 | 10 分钟 |

分类入口：`classify_error(status, body)`（HTTP 层）与 `classify_solo_error(code, msg)`（SSE 流内 error 事件）。

### 5.4 模型名映射

`create_agent_task`/`llm_utils_chat` 请求体的 `model_name` 内部标识通过 `model_name_map` 从显示名映射，
格式为**连字符 + `__dev` 后缀**（如 `glm-5.2 → glm-5.2__dev`，以代理日志 `model_config` 事件实测为准；
早期调研记录的下划线格式 `glm_52__dev` 有误）。未知模型回退 `deepseek-v4-flash__dev`。

### 5.5 关键实现注意事项（踩坑记录）

- **`NO_PROXY=*`**：API 服务启动时必须设置，防止 ureq 走系统代理（127.0.0.1:8899）形成回环 → 10s 超时 → 连续错误冷却。
- **SSE 流式超时**：流式 Client 只设 `timeout_write(30s)` + `timeout_connect(10s)`，**不设 timeout_read**；
  `timeout_read(Duration::from_secs(0))` 会触发 std panic（"cannot set a 0 duration timeout"），绝对不能用。
- **`device_id` / `machine_id` 派生**：`seeded_hex` 从 uid 确定性派生（SHA256 迭代哈希），同账号始终同标识。

## 6. API 命令契约（Tauri 前端 ↔ Rust）

> 前端通过 `invoke('command', args)` 调用。所有命令运行在 Rust 主线程（耗时操作放子线程并通过事件回传）。
> 错误以 `string` 返回（空=成功），或以 `{ ok: boolean, error?: string }` 结构返回。

### 6.1 环境检测 / CA 证书 / 代理

- `env_check()` → `EnvStatus { installed, running, version, path }`
- `open_trae_website()` — 打开 `https://www.trae.cn`（未安装引导）
- `cert_status()` → `{ installed }`；`cert_install()` — 提权（UAC）装入受信任根证书
- `proxy_start(port)` / `proxy_stop()` / `proxy_status()`
  - 事件：`proxy-log`（逐行）、`account-captured`（uid）；停止为优雅退出（CTRL_BREAK）→ 超时强杀

### 6.2 账号与分组

- `accounts_list()` → `Account[]`
  - `Account`: `{ userId, name, groupId|null, jwt, jwtExpHours|null, jwtExpTimestamp|null, checkedToday|null, credits|null, deviceIdMasked|null, remainingCredits|null, cooldown|null }`
  - `credits` 取 `credits_history.json` 最新日期余额；`remainingCredits` 取 `remaining_credits.json` 缓存；`cooldown` 取 `account_cooldowns.json`
- `credits_history()` → `CreditRecord[] { date, user_id, credits, delta }`
- `account_add_manual(name, jwt, groupId?)` / `account_update(userId, name?, jwt?)` / `account_delete(userId, deleteProfile)`
- `groups_list()` → `Group[] { id, name, color, order, count, uids }`
- `group_create(name, color)` / `group_update(id, ...)` / `group_delete(id)` / `group_move(userId, groupId|null)`

### 6.3 签到

- `checkin_start(opts)` → 事件 `checkin-progress`（NDJSON 行）+ `checkin-done`
  - `opts`: `{ scope: "all"|"group:<id>"|"selected", userIds?, skipCheckedIn, skipExpired }`
  - v2.9.0：失败自动重试最多 2 轮（30s/90s）；`retry` 事件驱动前端倒计时横幅
- `checkin_trends(days?)` → `CheckinTrendPoint[] { date, ok, already, failed }`（近 N 天按日汇总，数据源 `checkin_results.json`，保留 90 天）

### 6.4 JWT / 切换 / 设备

- `jwt_parse(jwt)` → `{ userId, expHours, expTimestamp, status }`（exp 兼容整数/浮点/数字字符串）
- `refresh_jwt(userId)` — 用 `refresh_token` 调 ExchangeToken 刷新，原子写回；`jwt_refresh_lock` 防并发 + double-check
- `switch_account(userId)` — 事件 `switch-progress` 回传步骤
- `device_reset(userId)` — 删除 `device_map.json` 该条目
- `switch_reset_device_ids()` — 6 层设备重置（UAC 提权）

### 6.5 日志 / 设置 / 邀请

- `logs_query(opts)` → `LogLine[]`；`opts`: `{ type?, date?, keyword?, limit? }`
- `logs_clear(log_type)` → `u32` — 按类型删除日志文件（all/proxy/checkin/switch，幂等）
- `settings_get()` / `settings_set(patch)` → `AppSettings`（全 snake_case）
- `invite_link()` → `{ url }`

### 6.6 API 服务（网关）

- `api_server_start()` → `ApiServiceStatus` — 启动内嵌 axum 服务（端口/默认模型由设置页提供）；鉴权统一走 API Keys 列表；应用退出自动停止
- `api_server_stop()` / `api_server_status()` → `{ running, port, total_requests, active_uid, last_error, started_at }`
- `pool_list()` → `ApiPoolFile { enabled_uids, strategy, group_ids }`
- `pool_set(uids, strategy?, group_ids?)` — v2.9.0 扩展调度策略与分组筛选
- `pool_status()` → 账号池实时状态（uid/name/credits/disabled/cooling/cooldown_reason/err_count）
- `api_usage_stats(days?)` → `UsageDayView[]` — 网关用量按日统计（默认 14，上限 90），直读落盘
- `api_keys_list()` → `ApiKeyEntry[]` / `api_keys_save(keys)` — 多 Key 管理；`ApiKeyEntry`: `{ id, name, key, enabled, daily_limit, created_at, used_date, used_today }`

### 6.7 代理日志 / 积分 / 快照

- `proxy_logs_list(opts)` → `ProxyLogEntry[]`（跨文件按时间倒序，method 区分 HTTP/WebSocket）
- `proxy_log_detail(log_file, line_number)` → 完整请求/响应详情
- `fetch_remaining_credits(userId)` / `refresh_remaining_credits()` — 实时查剩余积分并缓存
- `cooldown_clear(userId)` — 手动清除冷却
- `credits_daily_list()` → `CreditsDailySnapshot[] { date, total, earned, consumed }`
- `autostart_status()` / `autostart_set(enabled)` — 开机自启（即时生效）；配套 `settings.silent_checkin`

## 7. 开发与构建指南

### 7.1 环境准备（一次性）

| 依赖 | 版本要求 | 用途 | 校验命令 |
| --- | --- | --- | --- |
| Windows | 10 / 11 | 运行与打包平台 | `winver` |
| Node.js | ≥ 18（建议 22） | 前端构建 / Tauri CLI | `node -v` |
| Rust | ≥ 1.77（stable，edition 2021） | Tauri 后端编译 | `rustc --version` |
| Python | ≥ 3.9 | 内置签到/代理脚本 | `python --version` |
| WebView2 Runtime | Win11 自带 / Win10 需装 | 前端渲染内核 | 控制面板查看 |
| VS Build Tools | 「C++ 桌面开发」+ Windows SDK | Rust 原生依赖编译 | VS Installer 查看 |

```powershell
winget install Rustlang.Rustup
rustup toolchain install stable && rustup default stable
rustup target add x86_64-pc-windows-msvc   # MSVC 目标（Windows 默认）
```

Python 双角色：

- **构建机 Python 3.13.x**（必须，ABI 要求）：运行 `scripts/prepare_python_runtime.py` 准备内嵌运行时（用其 pip 拉取与 embeddable 同版本 ABI 的 wheel）。
- **应用内嵌运行时**：`npm run tauri build` 的 `beforeBuildCommand` 会自动执行准备脚本——下载 Windows embeddable Python 解压到 `src-python/`（解释器 + 标准库 + 预装 `requirements.txt` 依赖），再按发布白名单装配到 `build/python-bundle/`，随后随 `bundle.resources` 打进安装包。运行期 Rust 优先使用资源目录的 `python/python.exe`，不存在才回退系统解释器（兼容 dev 环境）。

运行时文件不入 git（`.gitignore` 已按 embeddable 产物清单忽略）；embeddable zip 缓存于 `%LOCALAPPDATA%/TraeWorkAssistant/build-cache`，脚本分层幂等（已就绪则秒级跳过）。

### 7.2 开发模式

```powershell
npm install            # 前端依赖；Rust 依赖首次 tauri dev/build 时 Cargo 自动拉取
npm run tauri dev      # 开发模式（Vite 5173 + Rust 热重载）
npm run dev            # 仅前端 Vite（无 Tauri API，invoke 会报错属正常）
npm run build          # tsc 类型检查 + vite 生产打包到 dist/
```

### 7.3 打包发布

```powershell
npm run tauri build    # 产出 msi / nsis 安装包
```

流程：`beforeBuildCommand` = `python scripts/prepare_python_runtime.py && npm run build`
（①准备内嵌 Python 运行时 → ②按白名单装配打包暂存 `build/python-bundle/` 并自检 → ③tsc + vite 产出 `dist/`）→
编译 Rust release → 按 `bundle.targets` 打包 → `build/python-bundle/` → `python/`、`src-ps/` → `ps/` 作为资源打入。
产物在 `src-tauri/target/release/bundle/`。
打包暂存与源目录 `src-python/` 解耦：只复制解释器 + 标准库 + 两个业务脚本 + `Lib/`，排除 `tests/`、
`requirements.txt`、`pythonw.exe`、`python.cat`，并裁剪 site-packages 中 pywin32 的 IDE/COM/文档附属
（pythonwin/win32com*/adodbapi/isapi/bin/PyWin32.chm/dist-info，约 16 MB）——运行脚本仅用
`win32crypt`（依赖 `win32/` 与顶层 pywin32 DLL，均保留）。安装包体积增加约 15-20 MB（NSIS 压缩后）。

### 7.4 测试

```powershell
cargo test                                      # Rust 单测（vault/配额/池策略/usage 聚合/签到合并等）
npx tsc --noEmit                                # 前端类型检查
npm run test                                    # vitest（cn/delay/format 纯函数用例）
python src-python/tests/test_auto_checkin.py    # Python 单测（JWT 解析/过期判定/seeded_hex）
```

### 7.5 命令速查

| 命令 | 作用 |
| --- | --- |
| `npm install` | 安装依赖 |
| `npm run dev` | 仅前端 Vite（5173） |
| `npm run build` | 前端类型检查 + 生产打包（`dist/`） |
| `npm run preview` | 预览 `dist/` |
| `npm run tauri dev` | 开发模式（前端 + Rust 热重载） |
| `npm run tauri build` | 打包 msi / nsis 安装包 |
| `cargo test` / `npm run test` | Rust / 前端单测 |

### 7.6 开发期常见问题排错

| 现象 | 可能原因 | 处理 |
| --- | --- | --- |
| `cargo build` 链接失败 / 找不到 `link.exe` | 未装 VS Build Tools 或 C++ 工作负载 | 装「使用 C++ 的桌面开发」+ Windows SDK，确认 MSVC 目标 |
| 启动白屏 / `invoke` 不存在 | 直接在浏览器打开 5173，未走 Tauri 外壳 | 用 `npm run tauri dev` 启动 |
| `cargo check/test` 写 target 拒绝访问 (os error 5) | IDE/杀软锁 target 目录 | 直接重试；必要时关闭占用进程 |
| 代理启动失败 / 捕获不到 JWT | 未装 CA 证书，或 Trae 未走本地代理 | 「一键安装证书」（UAC）→ 启动代理 → 确认日志 listening |
| 提示「未检测到 Python」 | 系统无 Python 或不在 PATH | 安装包已内置 Python 运行时；仅 dev 模式需要本机 Python |
| 证书安装无反应 / 代理启动即退 | 旧版本包未带 Python 依赖（缺 cryptography） | 已修复：安装包内置运行时 + 失败原因经 toast 透出；旧包临时规避 `python -m pip install cryptography pywin32`（装到 app.log `python_exe=` 指向的解释器） |
| 打包后运行缺脚本 | `bundle.resources` 未包含 | 确认指向 `../src-python/` 与 `../src-ps/` |
| 计划任务查询输出乱码 | `schtasks` GBK 输出被按 UTF-8 解读 | 统一走 `misc.rs::run_schtasks()`（前置 `chcp 65001`），勿裸调 `Command` |
| 注册计划任务 Access Denied | `/RL HIGHEST` 强制最高权限 | 已移除该参数，任务以当前用户身份运行 |
| 开代理后外网断网 / 停代理后 VPN 失效 | 系统代理覆盖 VPN 接管点 / 未还原 | v2.4.3 已修复（上游代理串联 + 原样还原）；需重新打包生效 |

### 7.7 约束与红线

- **仅 Windows 验证**：CA 证书与 MachineGuid 流程仅在 Windows 验证。
- **本地优先**：不连接任何自有服务器，账号/JWT/积分均留本机。
- **PowerShell 脚本编码**：UTF-8 with BOM + CRLF。
- **工程规范**：零新增依赖需评估；中文文案集中管理；显式数据 Migration；提交前缀 `[fix]/[feature]/[docs]/[config]/[test]`。

## 8. 专项设计（待实施）

> 以下两份设计已完成可行性分析但尚未落地；全文结论保留于此，实施时以本节为起点。

### 8.1 账号切换时会话/项目迁移

**问题**：切换 Trae 账号后，项目列表退回目标账号快照时点的旧数据（`state.vscdb` 被槽位快照整体回滚）。

**数据归属分层（实测 `%APPDATA%\TRAE SOLO CN`）**：

| 层 | 位置 | 归属 | 切换时行为 |
|---|---|---|---|
| 云端 | SOLO 服务端 | 会话（Mongo ObjectId）+ 项目按 `user_id` 隔离 | 切号后拉新账号数据 |
| 本地索引 | `state.vscdb` | `solo-lite:content-map:<uid>`（会话映射，按账号分区）；`solo-lite.local-project-folders`（项目→本地路径，**全局键**） | 被目标槽位快照整体回滚 ← 问题根源 |
| 本地文件 | `workspaceStorage` / `User/History` / `Workspaces` | 全局共享 | bridge 未快照，天然保留 |

**结论**：项目跨账号可见 ✅（本地改造即可）；会话真迁移 ❌（云端 `user_id` 归属校验，产生幽灵会话）。

**P1 方案（建议落地）**：bridge.ps1 恢复 `state.vscdb` 前抽出全局键（`solo-lite.local-project-folders`、`history.recentlyOpenedPathsList`），恢复后合并写回。注意：SQLite 键级合并需在 Trae 未运行时操作，操作前做 `.bak` 备份。

**不可合并项红线**：`storage.json` / `machineid`（登录态）、`content-map:<uid>`（账号分区键，避免幽灵会话）、`aha/` 与 Chromium 目录（维持槽位快照现状）。

后续阶段：P2 会话导出存档（旧账号 jwt 拉会话 → Markdown 归档）；P3 会话复制回放（实验性，需先抓包验证消息体可回放，暂缓）。

### 8.2 Work 积分资源池（work_transport）

**目标**：把多账号 Work 积分（product_id 209）聚成对外部调用方无感的资源池（OpenAI 兼容接口）。

**关键约束（抓包实证）**：Work 积分只能由**实时 Trae SOLO 会话**自身发起 `create_agent_task` 消耗；
请求体由原生层 `ai_agent.dll` 构造、闭源 `@aha-kit` 加密，真实身份复刻仍 4001，外部无法合成该调用。

**唯一可行路径 = 多活会话编排**：每个 Work 账号跑一个实时 Trae SOLO 实例，助手作为编排层
按池选账号 → 驱动对应实例 → MITM 捕获 SSE → `sse.rs` 转 OpenAI。现有 `routes.rs / pool.rs / sse.rs / server.rs / payload.rs` 全部不动，仅增量：

1. 新增 `api_server/work_transport.rs`：持有会话句柄，`trigger_create_agent_task(account, prompt) -> SSE reader`
2. 新增 Work 余额来源命令（端点需单独确认）→ `remaining_work_credits.json`
3. `routes.rs` 加 ~10 行配置开关（`credit_type: "ide" | "work"`，默认 `ide` 行为不变）

**分阶段**：Phase 0 用现有 IDE 池跑通端到端形态（零改动）；Phase 1 上 WorkTransport。

**未决项**（决定 Phase 1 能否轻量落地）：如何驱动实时 SOLO 会话发起请求（优先查本地 IPC/扩展 API，退化 UI 自动化兜底）；Work 余额 API 端点；单实例多账号是否可行。

**风险**：驱动真实客户端批量消耗积分可能触及 Trae ToS，上线前需评估；N 个实时会话的运维成本（登录态维护、崩溃恢复——`pool.rs` 的 SessionDead 冷却机制天然适配）。

## 9. 风险与应对

| 风险 | 等级 | 应对 |
|---|---|---|
| TW 升级导致接口/路径变化 | 高 | 核心逻辑留在可热更的 Python/PS；应用内检测版本并提示 |
| TRAE 启用证书固定 | 高 | 降级：改用登录授权获取可续期凭据（见设计文档 7.3） |
| 上游协议加密升级（aha/TTNet） | 高 | 已实证明文路径失效；跟随真实客户端行为，不逆向闭源组件 |
| 安全软件拦截 CA/代理 | 中 | 白名单指引 + 代码签名 |
| 多账号触发风控 | 中 | 免责声明 + 签到间隔随机抖动 + 不超个人使用并发 |
| JWT 泄露 | 中 | v2.9.0 已迁入 Stronghold vault + DPAPI；展示仅脱敏 |
| Token 自动刷新失败 | 低 | 回退旧 token 不改写；前端提醒手动处理 |
| SSE 日志膨胀 | 低 | 仅记录摘要元数据；代理日志按日分割 + 保留期清理 |
| UAC 拒绝 / Python 缺失 | 低 | 明确提示 + 手动步骤；安装包内置 embeddable Python |
