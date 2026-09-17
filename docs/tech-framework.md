# 技术架构设计 — AI Work 助手 v3.4.5

> 本文是技术侧唯一总纲：技术选型、架构分层、数据模型、进程契约、API 协议参考、开发运维与排错。
> 产品侧（需求/交互/界面）见 [product-design.md](product-design.md)；未排期优化项见 [backlog.md](backlog.md)（WorkBuddy 接入蓝本协议事实已归并至本文附录 B）。
> **完整 Tauri 命令契约表以根目录 `AGENT.md` §5 为权威**，本文只保留契约概览与协议细节，避免双维护漂移。

## 1. 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| UI | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 + lucide-react | Web 技术栈还原设计稿；Zustand 单一状态源；Recharts 图表 |
| 外壳 | Tauri 2.x（Rust 1.75+，MSVC / macOS 双平台） | 包体 8~15MB（远小于 Electron），可调用系统 API（注册表/证书/计划任务/DPAPI/Keychain） |
| 核心逻辑 | Rust（`src-tauri/src/tasks/` 直调模块） | 原 Python 脚本已全部重写为 Rust 后台任务（trae_checkin / wb_* / doubao_* / device_proxy），无子进程、无解释器依赖 |
| 登录态切换 | Rust `switcher/` 模块（5 应用 × 3 快照布局档案表驱动，进程内直调） | 原 `trae-switch-bridge.ps1` 已全量 Rust 化：sysinfo 进程管理 + windows-registry + lnk 解析（Windows）/ bundle 探测 + SIGTERM（macOS，F-75），无外部运行时 |
| 平台服务层 | Rust `platform/` 模块（F-75） | 数据根目录 / 子进程构建（sys_command）/ vault 原语（DPAPI \| Keychain）/ 系统代理（注册表 \| networksetup）/ CA（certutil \| security）双实现收口，`#[cfg]` 分派 |
| API 网关 | Rust axum（内嵌，复用 Tauri tokio runtime） | OpenAI / Anthropic 双协议端点 + SSE 转换 + 账号池调度，无独立进程 |
| HTTP 客户端 | ureq（同步）+ `spawn_blocking` 包装 | 双 Client 设计：短请求 120s 超时 / 流式仅 ResponseHeaderTimeout 120s，共享连接池 |
| 加密 | tauri-plugin-stronghold + windows-sys(DPAPI) / macOS keyring(Keychain) | jwt/refresh_token 入 vault，主密码 Windows 经 DPAPI、macOS 经 Keychain 仅本机当前用户可解 |
| 测试 | cargo test + vitest | Rust 420+ 用例（tasks/device_proxy/switcher/platform 纯函数） / 前端 `src/lib/format.test.ts` |
| 打包 | Tauri Bundler → MSI / NSIS（win 自定义模板）+ dmg（mac aarch64 / x64 双架构） | 全 Rust 零外部运行时（Python 与 PowerShell 均已移除）；平台配置拆分 `tauri.{windows,macos}.conf.json` 深度合并；产物经 `scripts/rename_release.mjs` 输出中文命名到 release/ |

**不采用**：Electron（体积过大）、WPF/WinUI（样式成本高）、PyQt（视觉不达要求）、React Router/Redux（依赖最小原则）。
**平台支持（F-75，2026-09-17）**：Windows 10/11（完整功能）+ macOS 12+（Apple Silicon / Intel）——mac 差异收敛于 `platform/` 与 `switcher/{proc,locate,machine}.rs` 的 `#[cfg]` 分支，应用域灰度放开（`mac_supported`）；schtasks / MachineGuid / UI 点击兜底为 Windows 专属，mac 由内置调度器 + 开机自启覆盖。设计与进度见 `docs/tmp/f75-macos-support-design.md`。

## 2. 架构分层

```
Presentation  React + Tailwind（Dashboard/Accounts/Checkin/Credits/Logs/ApiService/Settings）
      │  Tauri invoke + Event Bus
State         Zustand（store.ts 单一真相：init / 刷新 / checkin/switch/saveLogin 事件归约）
      │
Bridge        Tauri Commands（src-tauri/src/commands/）
 ├─ env / cert / proxy        环境检测、CA 证书、MITM 代理生命周期
 ├─ accounts / oauth / jwt    账号 CRUD、OAuth 登录、JWT 解析与刷新
 ├─ checkin / misc / process  签到编排、schtasks（chcp 65001）、三级进程关闭
 ├─ switch / profile          登录态切换、快照管理（target_app 四应用参数化）
 ├─ doubao                    豆包账号池 / 凭证 / 保活 / 额度 / 对话备份导出
 ├─ trae_apps                 双应用账号自动发现（dc id 与 Cloud-IDE id 双体系）
 ├─ vault                     Stronghold 加解密（load_accounts / save_accounts）
 └─ api_server                axum 网关：Trae 池（routes/pool/payload/sse/auth/models_sync/usage/api_logger）
                              + WB 池（wb_route/wb_payload/wb_sse/wb_upstream/wb_responses/wb_images/wb_sticky/wb_toolexec/wb_catalog/wb_model_route）
                              + 三池调度（dispatch/unified_catalog/custom_models/custom_route/retry/api_keys/gateway_settings）
Rust Tasks    tasks/（trae_checkin / wb_checkin / wb_common / wb_credits / ui_click / doubao_session / doubao_quota / doubao_chats
              / scheduler——应用内定时调度器，schtasks 的跨平台补充）+ device_proxy/（MITM 代理模块，hyper+rustls 自建）
Switcher      tasks 外的独立域：switcher/（原 PS 切换桥——profile 档案表 / locate 六级 exe 发现 /
              proc 三级关闭 / machine 6 层重置 / copy+icube+chromium+authfile 三布局快照管线）
Store         store/（SQLite 存储层——全量状态库 aiwork.sqlite：kv 键值文档表 / 行文档实体表 / 列化流水表；
              WAL + busy_timeout，首启迁移器把旧 JSON 导 backup/，v3.4.5 起替代 data 目录 JSON 读写）
```

关键机制：

- **数据原子写**：`fs_utils::write_json` tmp + rename；`dig()` 沿 data/result/resp/response/info 包裹键递归下钻（限深 8 层），抗官方信封字段变动（F-49）。
- **Mutex 安全锁**：统一 `safe_lock()` 替代 `lock().unwrap()`，锁毒化时恢复内部数据继续运行；响应构建 `unwrap_or_else` fallback。
- **进程三级关闭（F-47）**：优雅关闭（taskkill 不带 /F 发 WM_CLOSE，等 3s）→ 树杀（/T /F，等 2s）→ 返回 Err 由前端提示人工介入；子进程一律 CREATE_NO_WINDOW。
- **代理生命周期**：`proxy_start` 先捕获用户已有系统代理（VPN）作 `UPSTREAM_PROXY` 再改写系统代理为 127.0.0.1:<port>；`proxy_stop`/看门狗原样还原。LLM 上游请求须 `NO_PROXY=*` 防系统代理循环。
- **动态叶子证书 AKI**：MITM 叶子证书必须带 Authority Key Identifier（OpenSSL 3.2+/Python 3.13 客户端强制）；只补叶子、不改 CA 本体。工具自身出站请求一律 `ProxyHandler({})` 绕系统代理直连。

### 2.1 前端实现要点

- 启动加载态：`App.tsx` 等待 store `init()` 完成才挂载主界面，避免空状态闪烁。
- 设置页显式保存：本地表单 + dirty 标记，保存才落盘；`saveSettings` 失败自动回滚。
- 签到发起前先重置 checkin 状态，避免上一次进度残留；异步按钮统一 `withMinDelay(promise, 1000)`。
- 日志页实时代理输出最新置顶（unshift），最多 200 行；`ProxyLogsTab` 详情弹窗用 useRef 递增请求 ID 防竞态。
- 图表经 `useIsDark()` 随主题动态适配；弹窗 Modal 支持 Esc 关闭 + body 滚动锁（不支持 `window.confirm`）。
- 版本号运行时经 `getVersion()` 读取（Cargo.toml 单一来源），前端不硬编码。

## 3. 数据模型

统一存于 `%APPDATA%\AIWorkAssistant\`（旧 TraeWorkAssistant 目录由 `state.rs::migrate_legacy_dirs` 启动自动复制迁移）：

> **v3.4.5 起 data 目录 JSON 全量迁入 `data/aiwork.sqlite`**（WAL；kv 键值文档表 29 键 + 行文档实体表 12 + 列化流水表 8；首启迁移器导旧 JSON 入 `backup/`，`user_version` 幂等闸门）。下表 JSON 文件名为**逻辑名**（kv 键 = 文件名去 .json），运行期不产生 JSON 读写。

| 逻辑名（库中位置） | 说明 |
|---|---|
| `conf/app_settings.json` | Settings 全字段（snake_case，UI 配置保留文件形态） |
| `conf/vault.stronghold` + `conf/vault_key.bin` | jwt/refresh_token 权威存储（按 uid 键）+ DPAPI 加密的 vault 主密码；JSON 落盘占位化，签到/网关在 Rust 内存中临时解密（无落盘子进程） |
| `checkin_accounts`（accounts 表） | 账号 + JWT（敏感字段 vault 化后为占位） |
| `device_map`（表） | user_id → 虚拟设备身份（`rand_digits(n, seed=user_id)` 稳定派生） |
| `groups`（表） | 分组 + membership |
| `credits_history` / `credits_daily` / `remaining_credits`（表） | 积分明细 / 每日三线快照 / 剩余积分缓存（裁剪 90~365 天） |
| `checkin_results`（表） | 签到最终态按日落库（T8，保留 90 天） |
| `account_cooldowns`（表） | 签到错误冷却状态 |
| `api_pool`（kv） | 账号池：enabled_uids + `strategy`(expire_first/credit_first/random/weighted/p2c) + `group_ids` + 调度开关 |
| `api_keys`（表）/ `api_usage`（表）/ `api_models`（kv） | 多 API Key+每日配额 / 用量按日统计 / 模型目录 |
| `data/profiles*/`、`doubao_chats/`、`exports/`、`certs/` | 快照槽（current_account.txt + <uid> 槽位 + .bak 单代回滚）/ 对话备份 / 导出产物 / 自签 CA——保留文件形态不入库 |
| `doubao_accounts`（表）/ `doubao_captured_credentials`（kv）/ `doubao_health_events`（表） | 豆包账号池 / 抓包凭证回写 / 运维健康史 |
| `logs/` | proxy / checkin / switcher / api / proxy-requests / app.log |

账号唯一主键 `UserID`（JWT payload `data.id`）。**红线**：账户中心 dc id与 Cloud-IDE id 两套 id 空间不通用，混用会产生重复账号。

## 4. 签到冷却与账号池调度

### 4.1 错误分类冷却状态机

| 错误类型 | 触发条件 | 冷却时长 | 账号池处理 |
|---|---|---|---|
| `PlanLimit` | 响应体含 `code:1005` | 12 小时 | 换号重试 |
| `SoftRate` | HTTP 429 | 60 秒 | 换号重试 |
| `SessionDead` | HTTP 401 | 永久（需重登） | 换号重试 |
| `NotFound` | HTTP 404 | 60 秒（不累计） | 换号重试 |
| `Server` / `Client` | 5xx / 其他 4xx | 累计达阈值 10 分钟 | 换号重试 |

签到成功且积分 > 0 自动清除冷却（SessionDead 除外）；`CheckinGuard`（tokio Mutex）应用级防重入，页面/托盘/静默签到共用；失败自动重试最多 2 轮（30s/90s），per-uid 最终态合并。

### 4.2 账号池调度

1. 跳过禁用/冷却中/积分已过期/零积分账号（分组筛选后）
2. 按 `strategy` 排序：`expire_first`（默认，积分先过期优先）/ `credit_first` / `random`
3. 单请求最多换号 3 次（MaxRotate）；状态持久化 `api_pool.json`，重启不丢

## 5. 进程与子进程契约

### 5.1 `tasks/trae_checkin.rs`（批量签到，Rust 直调）

- 入口 `checkin_start(opts)`：`opts: { scope: all|group:<id>, user_ids?, skip_checked_in, skip_expired }`；vault 解密在内存中完成（无子进程、无临时文件）。
- 进度经 `checkin-progress` 事件下发，事件载荷（原 NDJSON 行结构）保持向后兼容：

```json

```json
{"type":"start","total":6}
{"type":"account","index":1,"user_id":"4487…","name":"…","status":"success","delta":300,"elapsed":1.24}
{"type":"account","index":2,"user_id":"…","status":"fail","error_type":"PlanLimit","cooldown_until":1786700000}
{"type":"done","ok":5,"already":0,"failed":1}
```

- `status` ∈ `already|success|fail`；每次签到追加 `{date, user_id, credits, delta}` 入 `credits_history.json`。

### 5.2 `device_proxy/`（MITM 代理模块）

- 配置：`AIWORKDATA_DIR` 决定数据根目录；端口经 `proxy_start(port)` 直传（默认 8899）；上游代理经 `ProxyConfig.upstream` 直传（http/socks5，含认证字段）；`AUTO_CAPTURE_JWT=1` 语义由模块内常量承接。
- MITM 捕获 `trae.cn`/`trae.com.cn` 带 `Cloud-IDE-JWT` 的请求写回账号库（exp 防降级）；`mchost.guru` 解密记录对话摘要；WebSocket 隧道转发不记录内容。
- 结构化日志 `logs/proxy_req_YYYY-MM-DD.log` 供 `proxy_logs_list/detail` 查询。
- 同时承担豆包凭证抓包（doubao.com Cookie sessionid/sid_guard/ttwid 落盘供回写）。

### 5.3 `switcher/`（登录态切换器，原 PS 切换桥 Rust 化）

- Action：`Switch / SaveCurrentLogin / ResetMachineId / ResetDeviceIds / BackupCurrent / RestoreOnly / KeepAlive`；入参 `target_app: TraeWork|Trae|Doubao|WorkBuddy|CodeBuddy`、`proxy_port`（>0 注入 `--proxy-server`）、`include_indexeddb`、`expected_current_uid`（防误覆盖守卫）。
- icube 布局（Trae 双应用）：精准备份 9 类核心文件；chromium 布局（豆包）：白名单目录快照 + `snapshot_meta.json`（schemaVersion=1）+ 完整性三层校验 + `.bak` 单代回滚 + ExpectedCurrentUid 防误覆盖守卫；authfile 布局（WorkBuddy/CodeBuddy）：L1 auth 文件 + L2 storage/user-* + L3（仅 CodeBuddy）vscdb 登录真源（含 -wal/-shm 边车）。
- 保存前预检登录会话（Cookie 存在性检测），未登录态拒绝入槽；全局互斥拒绝并发切换。
- 进度通道：`ProgressSink` 回调（TauriSink emit 事件 / CliSink 打印 stdout），NDJSON 行结构与 `*-done` 事件契约与 PS 桥逐字段兼容。

### 5.4 API 网关（api_server 模块）

| 端点 | 说明 |
|---|---|
| `GET /health`（免鉴权）/ `GET /status` | 健康检查 / 账号池状态 |
| `GET /v1/models` | 统一模型目录（Trae 官网同步 + WB 目录 + 自定义三源合并），官网同步后无需重启 |
| `POST /v1/chat/completions` | OpenAI 协议（流式 + 非流式） |
| `POST /v1/messages` | Anthropic Messages 协议（message_start → content_block_* → message_delta → message_stop） |
| `POST /v1/responses` | Codex Responses API 投影（WB 上游模型） |
| `POST /v1/images/generations` / `/v1/images/edits` | 生图双端点（WB 上游，模型需 `supports_image=true`） |
| `POST /v1/completions` | legacy text completion（prompt 转 user message 复用链路） |
| `/v1/embeddings` | 明确 501（上游无对应能力，不做假实现） |

- 鉴权：API Keys 列表（T2/T15），`Authorization: Bearer` + `x-api-key` 双风格；未配置启用 Key 时不鉴权；超日配额 429；`ck_` 子 Key 支持 `allowed_accounts` 上游白名单 + `schedule_mode`（expire_first/dedicated）+ 按日统计。
- 请求侧统一转 OpenAI 内部格式复用池调度；响应侧按协议分别输出；WB 上游 reasoning_content 已透传（Anthropic 侧映射 thinking block）。
- 账号池 app 无关：Trae / Trae Work 账号入池即被同一网关服务（通用积分 208）。

### 5.5 三池调度与统一模型目录（v3.3.x）

- **资源池**：`trae`（SOLO `llm_utils_chat`，积分 208）/ `buddy`（copilot.tencent.com 或 www.workbuddy.ai `/v2/chat/completions`）/ `custom`（自定义 OpenAI 兼容上游，`data/custom_models.json`，命中即直达不参与池间策略）。
- **池间策略（`dispatch.rs` → `data/dispatch_policy.json`）**：`smart`（默认：池内最早积分到期优先 → 模型倍率小者优先 → 健康账号积分总和多优先，并列回退固定序）/ `priority`（严格按 priority 数组取首个可用池）；`per_model` 模型级覆盖优先于智能重排；`fallback` 开关控制双源模型是否跨池回退。优先级缺失回退 `["buddy","trae"]`。
- **统一模型目录（`unified_catalog.rs`）**：`api_unified_models` 命令与 `GET /v1/models` 共用三源合并视图（Trae 官网同步 + WB 目录 + 自定义模型），canonical_id 归并（trim+lowercase）；模型元数据四层兜底（L1 人工覆盖 `trae_model_meta.json` → L2 官网同步 → L3 默认 128K/倍率参考 → L4 系列/思考档位/图片支持推断）。
- **自定义模型（`custom_models.rs`）**：upsert 校验（name/base_url 必填、canonical 不重复、id `cm-<12hex>` 自动生成）；`chat_url` 归一（base 含 `/v1` 与否两种形态）；`custom_model_test` 连通性测试与保存同口径预检。
- **WB 池细节**：headers 三铁律 / 请求体改写 / 五态机 / 分级重试 / 会话粘性 / ck_ 子 Key 等，完整契约见 `AGENT.md` §5.2（权威）。

## 6. 附录 A：Trae API 协议参考（抓包实证）

> 完整抓包过程记录见 git 历史（原 api-credit-analysis.md，2026-08-14 归档）。此处保留开发必需的协议事实。

### 6.1 域名与端点

| 域名 | 端点 | 用途 |
|---|---|---|
| `trae-api-cn.mchost.guru` | `POST /api/agent/v3/llm_utils_chat` | 核心对话（IDE 积分 208），HTTP + SSE |
| `trae-api-cn.mchost.guru` | `POST /api/ide/v1/get_detail_param` | 模型列表 |
| `api.trae.cn` | `POST /trae/api/v2/ug/checkin_credits/claim` / `status` | 执行签到 / 签到状态 |
| `api.trae.cn` | `POST /trae/api/v2/pay/ide_user_ent_usage` | 积分/权益查询（208/209 分包） |
| `api.trae.com.cn` | `POST /cloudide/api/v3/trae/oauth/ExchangeToken` | Token 刷新 |
| `api.trae.com.cn` | `POST /cloudide/api/v3/trae/GetUserInfo` | 用户信息 |
| `www.trae.cn` | `GET /authorization` | OAuth 登录授权页 |

### 6.2 请求头与认证

`Authorization: Cloud-IDE-JWT <accessToken>`，附带 `X-Cloudide-Token`、`X-Ide-Token`、`X-App-Id`、`X-Ide-Version`、`X-Device-Id` 等头。签到接口的 `x-device-id` 由代理按 `device_map.json` 改写。

### 6.3 请求体加密结论（重要）

- TTNet/aha 传输层存在 `@aha-kit` 加密（`x-bridge-transport: aha` 下 body 加密，走 TTNet 隧道）；真实客户端对话为**直连 HTTPS POST + aha 加密体**。
- `llm_utils_chat` 端点**明文 JSON 可行**（已验证），`create_agent_task`（Work 积分 209）为 ~123KB 富上下文加密体，**外部无法复刻**（真实身份复刻仍 4001）——Work 积分接入只能走多活会话编排，见 [backlog.md](backlog.md) W-01（**2026-09-15 已排除**：Trae 积分签到调整，前提与收益不成立，专题转技术留档）。

### 6.4 SOLO SSE 自定义事件

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

## 7. 开发与运维

### 7.1 环境准备（一次性）

**Windows**（完整功能验证环境）：

| 依赖 | 要求 | 校验 |
|---|---|---|
| Windows | 10 / 11 | `winver` |
| Node.js | ≥ 18（建议 22） | `node -v` |
| Rust | ≥ 1.77 stable（MSVC，edition 2021） | `rustc --version` |
| VS Build Tools | 「使用 C++ 的桌面开发」+ Windows SDK | 链接错误多因缺失此项 |
| WebView2 | Win11 自带 / Win10 装 Evergreen Bootstrapper | — |

**macOS**（F-75，构建 / CI / 真机验证）：

| 依赖 | 要求 | 校验 |
|---|---|---|
| macOS | 12+（Apple Silicon 或 Intel；CI 矩阵 macos-14 / macos-13） | `sw_vers` |
| Node.js | ≥ 18（建议 20） | `node -v` |
| Rust | stable + 对应架构 target（`aarch64-apple-darwin` / `x86_64-apple-darwin`；universal 需双 target） | `rustc --version` |
| Xcode CLT | `xcode-select --install`（链接器 / Security.framework） | `clang -v` |

打包：Windows `npm run tauri build`（MSI + NSIS）；macOS `npm run tauri build -- --bundles dmg`（对应架构 dmg，`tauri.macos.conf.json` 自动合并）。

```powershell
npm install
npm run tauri dev      # 开发模式（Vite 5173 + Rust 热重载；勿裸 npm run dev，白屏）
npm run tauri build    # 打包 MSI + NSIS → src-tauri/target/release/bundle/
node scripts/rename_release.mjs    # 产物输出 release/，中文命名
node scripts/package_portable.mjs  # 便携版 zip
```

测试：`cargo test`（Rust）、`npm run test`（vitest 前端）。

### 7.2 打包注意

- NSIS 用自定义模板 `build-assets/installer.nsi`（升级安装默认直接覆盖）；`installer-hooks.nsh` 处理旧品牌静默卸载（需 UTF-8 BOM）。
- 升级 Tauri CLI 后如 NSIS 构建报错，需从对应版本 tag 重新同步模板。

### 7.3 常见问题排错

| 现象 | 原因 | 处理 |
|---|---|---|
| `cargo build` 链接失败 / 找不到 link.exe | 未装 VS Build Tools C++ 工作负载 | 装「使用 C++ 的桌面开发」+ SDK，确认 MSVC 目标 |
| 启动白屏 / `invoke` 不存在 | 浏览器直开 5173，未走 Tauri 外壳 | 用 `npm run tauri dev` |
| 代理捕获不到 JWT | 未装 CA 或 Trae 未走代理 | 一键安装证书（UAC）→ 启动代理 → 看日志 listening |
| 开代理后部分网站打不开 | 系统代理被改写 | v2.4.3 起自动串联已有代理为上游 |
| 停代理后 VPN 失效 | 旧版只置 0 未还原 | 已改为原样还原 ProxyEnable/ProxyServer/ProxyOverride |
| 计划任务输出乱码 / Access Denied | schtasks GBK / `/RL HIGHEST` | 统一走 `misc.rs::run_schtasks()`（chcp 65001）；不加 /RL HIGHEST |

## 8. 风险与应对

| 风险 | 等级 | 应对 |
|---|---|---|
| 上游升级导致接口/路径变化 | 高 | `dig()` 宽容解析；接口层独立模块（Rust 单测锁定解析行为，随发版整体交付） |
| 上游启用证书固定 | 高 | 降级：OAuth 登录获取可续期凭据（refresh_token 13 天自动续期） |
| 安全软件拦截 CA/代理 | 中 | 白名单指引 + 代码签名 |
| 多账号触发风控 | 中 | 免责声明 + 签到间隔随机抖动 + 冷却状态机 |
| 凭证明文泄露 | 中 | vault + DPAPI；UI 掩码；账号池文件 .gitignore；导出提醒备份 vault |
| UAC 拒绝 | 低 | 明确提示 + 手动步骤 |

## 9. 附录 B：WorkBuddy / CodeBuddy 协议参考（原 workbuddy-product-design.md 精华归并）

> 实施蓝本原文见 git 历史（2026-09-13 归档删除）；批次 1~4 已全部落地，本节保留开发排错必需的协议事实。

### B.1 已验证端点速查

| 用途 | 端点 | 要点 |
|---|---|---|
| token 续期 | `POST www.codebuddy.cn/v2/plugin/auth/token/refresh` | `X-Refresh-Token` 头，空体 `{}`；**该头仅允许出现在此端点** |
| Keycloak 备选 | `POST {iss}/protocol/openid-connect/token` | `grant_type=refresh_token&client_id=console` |
| OAuth 扫码 | `POST /v2/plugin/auth/state?platform=CLI` → `GET /v2/plugin/auth/token?state=` → `GET /v2/plugin/login/account?state=` | 独立 cookie jar；无 PKCE |
| 签到 | `POST /v2/billing/meter/daily-checkin`（状态回退 `/checkin-activity-status`） | 空体 `{}`；`code:10001`=已签容错 |
| 积分三件套 | `POST <domain>/billing/meter/get-user-resource-{summary,paid-packages,free-packages}` | 需 `X-Client-Platform: web` |
| 积分旧接口 | `POST /v2/billing/meter/get-user-resource` | `ProductCode: p_tcaca`，`Status:[0,3]` |
| 官方用量 | `POST /billing/meter/get-user-request-usage` | 日/周/月，分页 requestId 去重 |
| 对话上游 | `POST copilot.tencent.com/v2/chat/completions`（CN）/ `www.workbuddy.ai`（Global） | **只回 SSE**，非流式本地聚合 |
| 模型目录 | `GET {chatBase}/console/enterprises/personal/models` | 倍率/徽章/思考档位动态替换 |
| 成长中心 | `/v2/activity/growth/buddy/travel|lottery|tasks|energy|streak` | 全部实测 |
| 本地 quota 兜底 | `GET 127.0.0.1:<port>/api/v1/quota` | 扫 `~/.workbuddy/*.port` + 端口段探测 |

### B.2 域名路由与请求头铁律

| 区域 | 判定 | chat 上游 | billing/积分 |
|---|---|---|---|
| CN | domain 不含 `.workbuddy.ai` | `copilot.tencent.com` | `www.codebuddy.cn` |
| Global | domain 含 `.workbuddy.ai` | `www.workbuddy.ai` | `www.workbuddy.ai` |

- **令牌域与请求域不一致会被网关拒绝**；plugin 网关（token refresh）固定 codebuddy.cn 不随区域。
- 三铁律：① Origin/Referer 必带（按区域）；② 缺省字段显式 `X-No-User-Id / X-No-Enterprise-Id / X-No-Department-Info: 1` 占位；③ **chat 请求绝不携带 `X-Refresh-Token`**。UA 伪装 `CLI/2.63.2 CodeBuddy/2.63.2`。
- 签到/活动接口可用极简头（`User-Agent: WorkBuddy` + Bearer + X-User-Id）。

### B.3 联调避坑清单（实测实证）

| # | 坑 | 对策 |
|---|---|---|
| 1 | 上游拒绝非流式（code 11101） | 强制 `stream:true`，非流式本地聚合（tool_calls delta 按 index 合并） |
| 2 | `tool_choice` 对象报 400 | 归一化为 string |
| 3 | effort 档位上游忽略 | 按 `supportedEfforts` 降级；**hy3 系列仅 `high` 真正生效**（effort_override 修正层） |
| 4 | Claude Code 指纹触发审核 | 指纹清洗（cc_xxx 键值 / x-anthropic-* 引用剥离）+ **两句固定 system 模板逐字入黑名单**（`You are Claude Code…` / `Main branch…`），映射表最小改写（外置 `wb_template_map.json` 热更新） |
| 5 | chat 带 X-Refresh-Token 触发安全拦截 | 红线：仅 refresh 端点 |
| 6 | SSE 长流被中间层回收 | 15s keep-alive 注释行 |
| 7 | 客户端断连丢 usage | `_drain_upstream` 读完上游 |
| 8 | **prompt cache 对代理流量恒不命中**（按冷启动全价计费） | 成本模型按无缓存估算；缓存命中率指标仅基于本地 token 统计并标注口径 |
| 9 | 连续同角色消息 | 自动合并（antigravity-tools 实证） |
| 10 | 凭证双源冲突（auth 文件被客户端启动重写） | 桌面文件只读 + 工具侧 token store 副本「谁新用谁」（`expiresAtMs` 晚者胜出）+ 原子写/文件锁 |

### B.4 会话数据三件套

`projects/{ws}/{cid}.jsonl` 正文 + `workbuddy.db` sessions 表 + `edge-sync-mapping-v2.db` 云端映射（`convmsg:{uid}` 决定云端归属）——备份/复制缺一不可；复制 = jsonl 逐行 sessionId 换新 UUID + sessions 克隆 + edge 映射替换。

## 10. 附录 C：豆包对话协议情报（原 doubao-api-feasibility.md 精华归并）

> E-01/E-02/E-03 的需求与实现路径见 [backlog.md](backlog.md)；本节保留协议层事实。

- **对话端点**：`POST www.doubao.com/samantha/chat/completion`（SSE）；三模式 `doubao` / `doubao-think` / `doubao-expert` → `completion_option` 参数组。
- **风控形态**：验证码墙而非拒绝服务——错误码 `710012001`（sessionid 吊销）/ `710022004`（需验证码，人工过后恢复）/ `712010702`（Cookie/设备指纹缺失或编码错误）；HTTP 200 无数据流 = 连续失败退避信号。
- **签名体系**：`msToken`（URL query + Cookie 双处）+ `a_bogus`（192 字符 = SM3 双哈希 + RC4 固定 keystream + s4 自定义 base64，**绑定单次请求的 query+UA+时间戳，必须纯算法生成**，嗅探只能短窗重放）。
- **设备指纹**：`ttwid` / `passport_csrf_token` / `device_id` / `web_id` / `tea_uuid`（19 位）；**必须与账号绑定且保持一致**——频繁更换 device_id 是风控高危信号；MITM 嗅探按账号落库（E-02）。
- **多模态**：生图 SSE `block_type=2074`（`creations[]`，`image.status==2` 完成，URL 优先级 `image_ori > image_raw > thumb`，漏图轮询 `/message_node_info` 兜底）；生视频 `content_type=2020` 下发 → `fin_reason.async_task.id` → `/samantha/chat/async/stream` 等 `2021`（1~3 分钟，需任务桥 + event_id 游标重连）；文件中转站 TOS 上传 ≤1GB 得永久 URI；多模态 bot_id `7338286299411103781`。
- **反封号组合拳**：限速 + 随机延迟 + 指数退避，UA 保持真实采样值；设备指纹静态化。

## 11. 附录 D：开源参考仓库映射（learn-the-design, write-our-own-code）

> 原 oss-ecosystem-value-analysis.md（28+5 仓库调研快照）与 workbuddy-product-design.md §7.2 归并；克隆件在 `%TEMP%\oss-research\<repo>`。实施前拉最新源码核对。

| 仓库 | 价值落点 |
|---|---|
| Sliverkiss/workbuddy2api（Go） | WB 上游适配/调度/请求改写/粘性会话 |
| lovingfish/workbuddy-cliproxy | 审核模板黑名单最小改写 / hy3 强制 effort / prompt cache 陷阱 / 11101 拒非流式（避坑金矿） |
| hailinzhao/antigravity-tools（Rust+Tauri 2，同栈同类） | 会话粘性指纹 / P2C / 五态机 / 分级重试表 / 四段模型路由 |
| changexbc/workbuddy-switch（Rust/Tauri） | 账号管理/CLI 轮换五重防护/统计页 UI 基准 |
| corrinehu/dsh-workbuddy-connect（TS） | 双源凭证 / 模型目录 / DSH provider |
| tonny0812/workbuddy2api · muskke/trae-api-proxy（Go） | `/v1/responses` 投影 / 工具代执行（web_search 代理侧代执行回喂） |
| Tom6814/WorkBuddy2API | reasoning_content 透传 / 生图双端点 / 反封号组合拳 |
| xiaolizi0v0/CliProxy | 多 CLI 账号环境隔离 + 严格账号模式 + 接口脱敏（F-66） |
| wangchuxiaoji-oss/doubao2api · lzA6/doubao-2api | 豆包端点/SSE/风控错误码权威参考；多账号 Cookie 轮换与指纹静态化 |
| Evil0ctal/Douyin_TikTok_Download_API | `crawlers/douyin/web/abogus.py`——a_bogus 纯算法移植母本（注意 GPL/Apache 许可差异） |
| Jackchaos2025/Doubao-Image-Proxy | 生图 SSE 解析 + message_node_info 兜底 + image_ori 优先级 |
| laojichao/trae-local-api · laojichao/trae-api | tc 加密格式确认 + 四版本（cn/solo/sg/solo-sg）端点路由表；3 级回退 + 5 档竞速调度（F-72） |
| BlueChonk/trae-credential-reverse-engineering | tc 解密（AES-128-CBC+SHA-512）+ ECDSA P-256 刷新签名 + 98 API 清单（F-70） |
| xhrxgr/trae-work-cn-account-manager（Tauri 2 同栈） | `--user-data-dir` 多实例并行 + 插件共享实例隔离（F-67 底座；原 W-01 多活会话编排底座，W-01 已排除） |
| Ttungx/trae-solo-local-api · Sliverkiss/traework2api | `llm_utils_chat + function=solo_work_lite` 通道双实现交叉验证（原 W-01 可抄实现；W-01 已排除，通道情报仍有留档价值） |
| wicm84266964/Buddy2api · mtfly/trae-switch | 多通道网关统一接入方向验证；hosts 劫持 + 本地 443 反代（F-73） |
| jlcodes99/cockpit-tools · dingminhua/dsh-connect-trae | TRAE 多实例思路（F-67）；DSH 桥装即用（F-38 主参照） |
