# 技术架构设计 — AI Work 助手 v3.7.0

> 本文是技术侧唯一总纲：技术选型、架构分层、数据模型、进程契约、API 协议参考、开发运维与排错。
> 产品侧（需求/交互/界面）见 [product-design.md](product-design.md)；未排期优化项见 [backlog.md](backlog.md)（WorkBuddy 接入蓝本协议事实已归并至本文附录 B）。
> **完整 Tauri 命令契约表以根目录 `AGENT.md` §5 为权威**，本文只保留契约概览与协议细节，避免双维护漂移。
> **v3.7.0 变更**（2026-10-06，随 F-80 Qoder 支持同步）：① API 网关由三池升级为 **trae / buddy / qoder / custom 四池调度**，Qoder 池以 COSY 签名接入上游（§5.4–5.6）；② Qoder 账号三通道入池（PAT / OAuth 设备流 / IDE 登录态扫描）+ 双活动签到 + 积分看板三平台化（§3、§6、附录 C）；③ 切换器扩展为**七应用档案表**（新增 Qoder IDE / Qoder Work）与设备指纹覆写（§5.3）。

## 1. 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| UI | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 + lucide-react | Web 技术栈还原设计稿；Zustand 单一状态源；Recharts 图表 |
| 外壳 | Tauri 2.x（Rust 1.85+，MSVC / macOS 双平台，Cargo.toml rust-version） | 包体 8~15MB（远小于 Electron），可调用系统 API（注册表/证书/计划任务/DPAPI/Keychain） |
| 核心逻辑 | Rust（`src-tauri/src/tasks/` 直调模块） | 原 Python 脚本已全部重写为 Rust 后台任务（trae_checkin / wb_* / doubao_* / qoder_* / device_proxy），无子进程、无解释器依赖 |
| 登录态切换 | Rust `switcher/` 模块（七应用档案表 × 4 快照布局驱动，进程内直调） | 原 `trae-switch-bridge.ps1` 已全量 Rust 化：sysinfo 进程管理 + windows-registry + lnk 解析（Windows）/ bundle 探测 + SIGTERM（macOS，F-75），无外部运行时 |
| 平台服务层 | Rust `platform/` 模块（F-75） | 数据根目录 / 子进程构建（sys_command）/ vault 原语（DPAPI \| Keychain）/ 系统代理（注册表 \| networksetup）/ CA（certutil \| security）双实现收口，`#[cfg]` 分派 |
| API 网关 | Rust axum（内嵌，复用 Tauri tokio runtime） | OpenAI / Anthropic 双协议端点 + SSE 转换 + 账号池调度（四池），无独立进程 |
| HTTP 客户端 | ureq（同步）+ `spawn_blocking` 包装 | 双 Client 设计：短请求 120s 超时 / 流式仅 ResponseHeaderTimeout 120s，共享连接池 |
| 加密 | tauri-plugin-stronghold + windows-sys(DPAPI) / macOS keyring(Keychain) | jwt/refresh_token 入 vault，主密码 Windows 经 DPAPI、macOS 经 Keychain 仅本机当前用户可解 |
| 测试 | cargo test + vitest | Rust 700+ 用例（tasks/qoder_* 签名与上游、api_server/dispatch 四池调度、switcher/platform 纯函数） / 前端 `src/lib/format.test.ts` |
| 打包 | Tauri Bundler → MSI / NSIS（win 自定义模板）+ dmg（mac aarch64 / x64 双架构） | 全 Rust 零外部运行时（Python 与 PowerShell 均已移除）；平台配置拆分 `tauri.{windows,macos}.conf.json` 深度合并；产物经 `scripts/rename_release.mjs` 输出中文命名到 release/ |

**不采用**：Electron（体积过大）、WPF/WinUI（样式成本高）、PyQt（视觉不达要求）、React Router/Redux（依赖最小原则）。
**平台支持（F-75，2026-09-17）**：Windows 10/11（完整功能）+ macOS 12+（Apple Silicon / Intel）——mac 差异收敛于 `platform/` 与 `switcher/{proc,locate,machine}.rs` 的 `#[cfg]` 分支，应用域灰度放开（`mac_supported`）；schtasks / MachineGuid / UI 点击兜底为 Windows 专属，mac 由内置调度器 + 开机自启覆盖。设计与进度见 `docs/tmp/f75-macos-support-design.md`。

## 2. 架构分层

```
Presentation  React + Tailwind（Dashboard/Accounts/Checkin/Credits/Logs/ApiService/Settings
              + pages/qoder/ 六页：QoderOverview / QoderAccounts / QoderCheckin /
                QoderApiService / QoderSettings / HelpModal——Qoder 积分页复用 Dashboard
                体系 `qoder-credits` 视图，无独立页面）
      │  Tauri invoke + Event Bus
State         Zustand（store.ts 单一真相：init / 刷新 / checkin/switch/saveLogin 事件归约）
      │
Bridge        Tauri Commands（src-tauri/src/commands/，注册表见 main.rs `generate_handler!`）
 ├─ env / cert / proxy        环境检测、CA 证书、MITM 代理生命周期
 ├─ accounts / oauth / jwt    账号 CRUD、OAuth 登录、JWT 解析与刷新
 ├─ checkin / misc / process  签到编排、schtasks（chcp 65001）、三级进程关闭
 ├─ switch / profile          登录态切换、快照管理（target_app 七应用参数化）
 ├─ doubao                    豆包账号池 / 凭证 / 保活 / 额度 / 对话备份导出
 ├─ trae_apps                 双应用账号自动发现（dc id 与 Cloud-IDE id 双体系）
 ├─ qoder/                    Qoder 命令域（F-80）：accounts（PAT 导入）/ oauth（设备流登录）
 │                            / ide_store（IDE·Work 登录态扫描）/ checkin / credits / groups
 │                            / data_io（AES-256-GCM 导入导出）/ env_reset / cli_status
 │                            —— 27 个 qoder_* 命令 + 网关侧 qoder_pool_status /
 │                               qoder_catalog_sync / api_qoder_usage_stats
 ├─ vault                     Stronghold 加解密（load_accounts / save_accounts）
 └─ api_server                axum 网关：Trae 池（routes/pool/payload/sse/auth/models_sync/usage/api_logger）
                              + WB 池（wb_route/wb_payload/wb_sse/wb_upstream/wb_responses/wb_images/wb_sticky/wb_toolexec/wb_catalog/wb_model_route）
                              + Qoder 池（qoder_route：hedge 竞速 / 排队退避 / 模型级冷却 / 粘性选号，
                                上游签名与协议在 tasks/qoder_sign.rs + qoder_upstream.rs）
                              + 四池调度（dispatch/unified_catalog/custom_models/custom_route/retry/api_keys/gateway_settings/config_cache/efforts）
Rust Tasks    tasks/（trae_checkin / wb_checkin / wb_common / wb_credits / ui_click / doubao_session / doubao_quota / doubao_chats
              / qoder_checkin / qoder_credits / qoder_refresh / qoder_catalog / qoder_common / qoder_sign / qoder_upstream / qoder_device
              —— Qoder 上游 COSY 签名与协议层集中于 tasks/qoder_sign.rs + qoder_upstream.rs，网关与签到共用
              / scheduler——应用内定时调度器，schtasks 的跨平台补充）+ device_proxy/（MITM 代理模块，hyper+rustls 自建）
Switcher      tasks 外的独立域：switcher/（原 PS 切换桥——profile 档案表（七应用）/ locate 六级 exe 发现 /
              proc 三级关闭 / machine 6 层重置 / copy+icube+chromium+authfile+electron_root 快照管线）
Store         store/（SQLite 存储层——全量状态库 aiwork.sqlite：kv 键值文档表 / 行文档实体表 / 列化流水表；
              WAL + busy_timeout，首启迁移器把旧 JSON 导 backup/，v3.4.5 起替代 data 目录 JSON 读写；
              schema.rs SCHEMA_VERSION=3，qoder 四表随 F-80 M1 加入 ROW_TABLES）
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
- 侧边栏应用 Tab（`Sidebar.tsx::APP_TABS`）：trae / buddy / **qoder** / doubao 四项，Qoder 项 title 标注「Qoder CN（IDE / Work / CLI）」，下设六页菜单（overview/accounts/checkin/credits/api-service/settings）。
- 积分看板三平台化（`pages/dashboard/Dashboard.tsx`）：`platform: 'trae' | 'buddy' | 'qoder'` 作为 `PlatformScope` 下传 `CreditsTab` / `ExpiryTab`，各 Tab 内 `scope === 'qoder'` 分支渲染；Qoder 曲线数据由 `adapters.ts` 的 `qoderSnapshotsToPoints` / `qoderEarnedByDate` 做**本地快照差分**（上游无逐日接口），网关用量取 `gateway.qoder` 池 stats。
- 签到档期日历为纯模型 + 卡片组件（`components/checkinCalendarModel.ts` → `CheckinCalendarCard`），Qoder 双活动（0:00 每日签到 + 10:00 登录奖励）逐日聚合，已推广到 Trae / Buddy 签到页复用。

## 3. 数据模型

统一存于 `%APPDATA%\AIWorkAssistant\`（旧 TraeWorkAssistant 目录由 `state.rs::migrate_legacy_dirs` 启动自动复制迁移）：

> **v3.4.5 起 data 目录 JSON 全量迁入 `data/aiwork.sqlite`**（WAL；kv 键值文档表 + 行文档实体表 + 列化流水表三形态，`store/schema.rs` 为权威定义，`SCHEMA_VERSION=3`；首启迁移器导旧 JSON 入 `backup/`，`user_version` 幂等闸门）。下表 JSON 文件名为**逻辑名**（kv 键 = 文件名去 .json），运行期不产生 JSON 读写。

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
| `api_pool`（kv） | 四池配置：账号池 enabled_uids + `strategy`(expire_first/credit_first/random/weighted/p2c) + `group_ids` + **每池独立开关** `trae_enabled` / `wb_enabled` / `qoder_enabled` 及各池同构参数（详见 §5.5） |
| `api_keys`（表）/ `api_usage`（表）/ `api_models`（kv） | 多 API Key+每日配额 / 用量按日统计（`(bucket, day)` 复合主键，Qoder 桶 `UsageBucket::Qoder`）/ 模型目录 |
| `data/profiles*/`、`doubao_chats/`、`exports/`、`certs/` | 快照槽（current_account.txt + <uid> 槽位 + .bak 单代回滚）/ 对话备份 / 导出产物 / 自签 CA——保留文件形态不入库 |
| `doubao_accounts`（表）/ `doubao_captured_credentials`（kv）/ `doubao_health_events`（表） | 豆包账号池 / 抓包凭证回写 / 运维健康史 |
| `qoder_accounts`（表） | Qoder 账号池（行文档，pk 例外采用 seq 自增保序）：`QoderAccount`{ id(`qd-<hex12>`) / uid / plan(free·pro·pro+·teams) / `credential_source`(pat·ide_store·qoderwork_store·mitm·cli) / `needs_relogin` / `device_profile` } |
| `qoder_tokens`（表）+ kv `qoder_tokens_meta` | token store；敏感字段**不落库**（见下方加密说明） |
| `qoder_checkin_results`（表） | 双活动签到结果，pk = `{date}\|{uid}\|{tail}`，90 天逐 pk DELETE 滚动 |
| `qoder_credits_history`（表） | 每日积分快照，pk = date 同日覆盖，365 天裁剪；kv `qoder_credits_cache` 读缓存 TTL 600s + stale-on-error |
| kv `qoder_settings` / `qoder_groups` | Qoder 自动签到开关与调度时刻设置 / Qoder 分组 |
| `logs/` | proxy / checkin / switcher / api / proxy-requests / app.log |

账号唯一主键 `UserID`（JWT payload `data.id`）。**红线**：账户中心 dc id与 Cloud-IDE id 两套 id 空间不通用，混用会产生重复账号。

**Qoder 凭证加密红线**（`vault.rs::ns_get/ns_set` + `tasks/qoder_common.rs`）：敏感键集 `TOKEN_SENSITIVE_KEYS = [access_token, refresh_token, pat, machine_token]` 入 vault（命名空间 `ns:qoder:<account_id>`，底层 DPAPI），DB 侧一律写占位空串；`save_token_store` 持 `TOKEN_STORE_LOCK` 行级合并，**vault 写失败仍落占位并返回 Err，绝不外泄明文**；读取时 `token_store_load_secure` 从 vault 回填仅驻内存（DB 明文优先，防串号）；启动 `migrate_ns_on_startup` 把历史明文 token 收敛入 vault。`machine_id` 视为非凭证留 DB。

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

- Action：`Switch / SaveCurrentLogin / ResetMachineId / ResetDeviceIds / BackupCurrent / RestoreOnly / KeepAlive`；入参 `target_app: TraeWork|Trae|Doubao|WorkBuddy|CodeBuddy|Qoder|QoderWork`（七应用档案表，`switcher/mod.rs::TargetApp`，未知值回退 TraeWork）、`proxy_port`（>0 注入 `--proxy-server`）、`include_indexeddb`、`expected_current_uid`（防误覆盖守卫）、`machine_id_override`（F-80 Qoder 专用）。
- icube 布局（Trae 双应用 + Qoder IDE）：精准备份核心文件清单由档案表 `icube_items` 驱动（Trae 系 15 项 / Qoder 用 `QODER_IDE_ITEMS`，数据目录实测 `%APPDATA%\QoderCN`）；chromium 布局（豆包）：白名单目录快照 + `snapshot_meta.json`（schemaVersion=1）+ 完整性三层校验 + `.bak` 单代回滚 + ExpectedCurrentUid 防误覆盖守卫；authfile 布局（WorkBuddy/CodeBuddy）：L1 auth 文件 + L2 storage/user-* + L3（仅 CodeBuddy）vscdb 登录真源（含 -wal/-shm 边车）；**electron_root 布局（Qoder Work，2026-10-02 实测新增）**：数据目录 `%APPDATA%\com.qodercn.app.stable` 根级 Network/Cookies + Local State + Preferences 会话快照，与豆包多 Profile 布局不同构故独立管线，`graceful_wait_secs=8` 等 leveldb 落盘；进程名 `Qoder CN` 与 IDE 壳同名 → **精确匹配只停 Work 本体**（wildcard 会误杀 Qoder CN IDE），与 Qoder IDE 为两套独立登录态。
- **Qoder 设备指纹覆写（F-80 §5.10.2）**：`switch_profile` 在 `is_qoder` 分支从账号池取 `device_profile.machine_id`（`commands/qoder/common.rs::machine_id_of`）→ `Session.machine_id_override` → 切号/恢复成功后、启动前 `apply_fingerprint_override` → `switcher/machine.rs::apply_qoder_fingerprint` 写三处：① `machineid` 文件（无 BOM 32-hex）② `storage.json` 的 `telemetry.machineId / sqmId / devDeviceId` ③ `state.vscdb ItemTable` 的 `storage.serviceMachineId`。指纹由 `tasks/qoder_device.rs::QoderDeviceProfile` 在**入池时生成一次、永不轮换**（`machine_id = sha256(uuid4)[..32]`、`device_id/umid = uuid4`、随机 `machine_token`）；`merge_device_profile` 三级优先：真实捕获 > 绑定指纹 + 现场随机 machine_token > 不注入；`ensure_pool_profiles` 持池锁惰性回填。
- 当前登录账号探测：icube 布局以命令层预探测为唯一可靠源（Qoder 保存链经 vscdb `secret://userInfo` 解密传入；池 id 为 `qd-<hex>` 形态不可混用 Trae uid），空才回退日志探测（仅 Trae 系）；QoderWork 由 `live_work_account_id` 解密 `auth.v1.dat` 预探测。
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
- **Qoder 池（v3.7.0）**：命中 Qoder 源模型时由 `qoder_route.rs` 执行，上游为 Qoder agent 网关（非 OpenAI 兼容协议，需 COSY 签名 + 请求体加密 + SSE 信封翻译，详见 §5.6 与附录 C）。

### 5.5 四池调度与统一模型目录（v3.3.x 起，v3.7.0 扩为四池）

- **资源池**：`trae`（SOLO `llm_utils_chat`，积分 208）/ `buddy`（copilot.tencent.com 或 www.workbuddy.ai `/v2/chat/completions`）/ `qoder`（`gateway.qoder.com.cn` agent 上游，COSY 签名，见 §5.6）/ `custom`（自定义 OpenAI 兼容上游，`custom_models` 表，命中即直达不参与池间策略）。
- **池开关（`ApiPoolFile`，`models.rs`；持久化为 kv 键 `api_pool`，非独立文件）**：`trae_enabled`(默认 true) / `wb_enabled`(默认 false) / **`qoder_enabled`(默认 false)**，运行期镜像为 `ApiSharedState` 的 `AtomicBool` 三兄弟，`pool_set` 命令保存后热更新（`Ordering::Relaxed` store），无需重启。Qoder 池同构参数组：`qoder_enabled_uids`（`qd-` 前缀白名单，空 = fail-open 全池入池）/ `qoder_group_ids` / `qoder_strategy` / `qoder_hedge_threshold_ms` / `qoder_sticky_enabled` / `qoder_account_concurrency_limit`(默认 1) / `qoder_pool_sticky_ttl_secs`(默认 300) / `qoder_sticky_ttl_secs`(默认 1800)；trae_/wb_ 各有同构三参数。
- **池间策略（`dispatch.rs` → kv 键 `dispatch_policy`）**：`TargetPool { Trae, Buddy, Custom, Qoder }`；`smart`（默认：复合键 `SmartKey = (池内最早积分到期, 模型倍率小者, 健康账号积分总和多)`，并列回退固定序）/ `priority`（严格按 priority 数组取首个可用池）；默认优先级 `["buddy","trae","qoder"]`；`per_model` 模型级覆盖优先于智能重排；`fallback` 开关控制多源模型是否跨池回退。`TargetPool::parse` **只接受 trae/buddy/qoder**（custom 不可配为策略池，仅第⓪步直达）。
- **`resolve_target` 主流程**：⓪ custom 模型直达短路 → ② Buddy 模型归一化 → ③ Trae 源解析 → ③⁻ Qoder 源解析（`qoder_upstream::resolve(model, QoderRegion::Cn)`，**v1 调度恒 CN 区执行**）→ 池开关检查（`DispatchError::{TraeDisabled, WbDisabled, QoderDisabled}`）→ Key 绑定池 → 池粘性 `pool_sticky` → 候选序 → `pool_health`（Qoder 侧查 `model_cooling_remaining_secs` + `has_selectable_in`）→ 跨池 fallback。
- **统一模型目录（`unified_catalog.rs`）**：`api_unified_models` 命令与 `GET /v1/models` 共用**四源合并视图**（Trae 官网同步 + WB 目录 + Qoder 目录 + 自定义模型），canonical_id 归并（trim+lowercase）；模型元数据四层兜底（L1 人工覆盖 `trae_model_meta.json` → L2 官网同步 → L3 默认 128K/倍率参考 → L4 系列/思考档位/图片支持推断）。
- **自定义模型（`custom_models.rs`）**：upsert 校验（name/base_url 必填、canonical 不重复、id `cm-<12hex>` 自动生成）；`chat_url` 归一（base 含 `/v1` 与否两种形态）；`custom_model_test` 连通性测试与保存同口径预检。
- **热路径缓存（`config_cache.rs`）**：调度配置 5s TTL + 写失效，避免每请求读 SQLite（F-79 过渡方案之一）。
- **WB 池细节**：headers 三铁律 / 请求体改写 / 五态机 / 分级重试 / 会话粘性 / ck_ 子 Key 等，完整契约见 `AGENT.md` §5.2（权威）。

### 5.6 Qoder 池与双活动签到（v3.7.0，F-80）

**调度侧执行层（`api_server/qoder_route.rs`）**
- 慢请求竞速：阈值 `qoder_hedge_threshold_ms`（0 = 关闭，运行时 `clamp(1000, 8000)`，默认 8000ms）；**行源层首字节竞速** `lines_with_first_byte_hedged(primary, hedge_ms, spawn_backup)`（首字节到达前发起备份请求），`settle_qoder_hedge` 记 `hedge_takeover` / `hedge_lost` 日志。与 Trae/WB 池同构（F-76 能力覆盖 Qoder）。
- 排队退避：上游错误码 `10605` 判定为排队（Quota 类为 105 / 110–122）；`QUEUE_RETRY_LIMIT = 3`、`QUEUE_DEFAULT_BACKOFF_SECS = 5`、`QUEUE_BACKOFF_MAX_SECS = 30`。
- 模型级冷却 `QODER_MODEL_COOLDOWNS`（键为**裸 model 名**——接线 Global 区需加区前缀）；账号粘性 `qoder_sticky_enabled`（默认关）+ `pick_sticky_yield`（busy 让位）；`sticky_bindings` 表键命名空间 `"q:"`（qoder）/ `"t:"`（trae）。
- 用量记账：独立桶 `UsageBucket::Qoder`（`record_usage_qoder`），命令 `api_qoder_usage_stats` / `qoder_pool_status` 透出。

**上游协议与 COSY 签名（`tasks/qoder_upstream.rs` + `tasks/qoder_sign.rs`）**
- 双区：`QoderRegion::Global` = `api3.qoder.sh`、`Cn` = `gateway.qoder.com.cn`；**目录双区、调度执行 v1 恒 CN 区**。
- 聊天端点：`QODER_CHAT_URL = https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1`（SSE，`QoderTranslate` 翻译为 OpenAI chunk 流；`build_upstream_body` 构造 agent 信封）。
- **COSY 签名六步**（`qoder_sign.rs`，常量 `GATEWAY_COSY_VERSION = "1.1.38"`、`CLIENT_TYPE = "5"`）：① 身份 JSON 固定键序 → ② AES-128-CBC 加密（**key = iv**）→ ③ RSA-1024 PKCS#1 v1.5 加密 AES 密钥 → ④ payload base64 → ⑤ 签名 `MD5(payload \n key \n timestamp \n body \n path)` → ⑥ 头 `Authorization: Bearer COSY.<payload>.<sig>`；`build_cosy_headers` 产出 19 个 `Cosy-*` 头；请求体 `encode_body` 三段轮转 base64 + 字母表替换；`signature_path` 先剥 `/algo` 前缀。

**命令层（`commands/qoder/`，注册见 `main.rs::generate_handler!`——项目无 lib.rs）**
- **三通道入池**：`qoder_account_import_pat`（accounts.rs；`pt-`/`jt-` 前缀校验、`id = sha256(account_id)[..12]`、uid 兜底幂等、`credential_source = "pat"`，M1 最可靠通道）/ `qoder_oauth_login`（oauth.rs；PKCE 设备流，授权页 `qoder.cn/device/selectAccounts` + 轮询 `deviceToken/poll`，`dt-` 凭证入池，事件 `qoder-oauth-progress/done`，`OAUTH_RUNNING`/`OAUTH_CANCEL` 原子态 + `qoder_oauth_cancel`）/ `qoder_ide_scan`（ide_store.rs；扫 `%APPDATA%\QoderCN\User\globalStorage\state.vscdb` 表 `ItemTable` 键 `secret://aicoding.auth.userInfo`，Chromium os_crypt v10 + DPAPI 解密，含 WAL 检测；Work 侧另有 `scan_work_login_uid`）。
- **双活动签到**：`qoder_checkin_start` → `tasks/qoder_checkin.rs`：sash 活动体系 `GET {open_api}/sash/api/v1/me/campaigns` + `POST …/campaigns/{campaignId}/claim`（幂等 `CLAIMED` + `replayed`），`OPEN_API_BASE = https://openapi.qoder.com.cn`；轮次锁 `try_acquire_qoder_round`（抢不到 `skipped_busy`）；`jitter_sleep` 1~3s（xorshift64\*）；逐活动明细 `campaigns_log` 供档期日历；结果 90 天滚动。
- **积分**：`qoder_credits_fetch` / `qoder_credits_history_list` → `tasks/qoder_credits.rs`：`GET {open_api}/sash/api/v2/me/usage`（头 `cosy-clienttype: 10`）；`userQuota`/`addOnQuota` **聚合供余额链路 + 逐包 `packages` 供到期日历**（Add-on 包带 `detailUrl`）。
- **PAT/凭证惰性刷新（`tasks/qoder_refresh.rs` + `qoder_common.rs::ensure_fresh`）**：按凭证前缀分派刷新端点 `refresh_endpoint_for`——`pt-`(PAT) → `POST /api/v1/me/jobToken` 重换（access 24h / refresh 48h）；`jrt-` → `/api/v1/me/jobToken/refresh`；`dt-` 系 → `/api/v1/me/deviceToken/refresh`。note 状态全集：`no_credential / fresh / expired_needs_relogin / refreshed / refreshed_unsaved / refresh_failed / auth_dead / pat_rejected`，其中 `pat_rejected` 与 `expired_needs_relogin` 回写池 `needs_relogin`；`clamp_expires_at` 超界视为无过期信息；手动强刷 `qoder_account_refresh_token`（force + `i64::MAX` lazy）。
- **导入导出**：`data_io.rs` AES-256-GCM，密钥派生 Argon2id（19 MiB / t=2），魔数 `AIWQENC1`。另有 `qoder_env_check` / `qoder_open_ide` / `qoder_open_work` / `qoder_cli_status`（只读 `~/.qoder-cn/.qoder-app-status.json`，不涉凭证）/ `qoder_live_logins` / `qoder_env_reset_items` / `qoder_env_reset`（9 项清理映射，勾选「清 machine_identity」= 放弃设备身份）。
- **池损坏防护**：`load_pool_checked` 遇损坏行备份 `qoder_pool.corrupt.json` 并拒绝覆盖；写池统一 `with_pool_mut` 持 `qoder_pool_lock`。

**定时任务（`tasks/scheduler.rs::TASKS` 四条目）**

| key | 时刻/周期 | 说明 | 可配键 |
|---|---|---|---|
| `qoder-checkin` | 固定 10:15 | 单次覆盖「0 点每日签到 + 10:00 登录奖励」双活动；抢 `QODER_ROUND_LOCK` | `qoder_checkin_hhmm`、启用判定 `QoderSettings.auto_checkin`（默认 true） |
| `qoder-credits-snapshot` | 固定 23:40 | 每日积分快照落 `qoder_credits_history` | `qoder_credits_sync_hhmm` / `qoder_credits_sync_enabled` |
| `qoder-refresh` | 每 6h | `ensure_fresh(lazy_hours=7)` 遍历池刷新 token，顺带 fresh 拉积分暖缓存 | `qoder_token_renew_enabled`（默认开） |
| `qoder-catalog-sync` | 固定 05:50 | 真 COSY 签名拉 `model/list` → `adopt_remote` 替换 CN 缓存（按池序逐账号，任一成功即收口；空池静默跳过） | `qoder_catalog_sync_hhmm` / `qoder_catalog_sync_enabled` |

双轨机制：以上为应用内轨道（60s tick + 启动补跑 + 30min `RETRY_COOLDOWN_MS` + `scheduler_state.json` + `scheduler_status` 命令）；另一轨为 Windows `schtasks` 每日任务经主 exe `--task-run <name>` 兜底直调，注册命令 `qoder_checkin_task_register(times)`（配套 `_status` / `_unregister`）。

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
| Rust | ≥ 1.85 stable（MSVC，Cargo.toml rust-version，edition 2021） | `rustc --version` |
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
| **Qoder 协议逆向依赖 COSY 版本漂移**（上游升级换 `GATEWAY_COSY_VERSION`/头集合即全线失效） | 高 | 签名/信封解析全部纯函数单测锁定（`qoder_sign.rs` / `qoder_upstream.rs`）；版本常量集中定义便于随实测更新；错误码分类精确匹配防误伤 |
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

## 10. 附录 C：Qoder 协议参考（F-80 抓包实证 R-1~R-11）

> 实现落点见 §5.4–5.6；本节只留排错必需的端点、凭证体系与错误码事实（常量出处：`tasks/qoder_common.rs`、`qoder_checkin.rs`、`qoder_credits.rs`、`qoder_oauth.rs`、`qoder_upstream.rs`、`qoder_sign.rs`）。

### C.1 域名速查

| 域 | 常量 | 用途 |
|---|---|---|
| `openapi.qoder.com.cn` | `OPEN_API_BASE` | 活动签到、积分用量、token 签发/刷新（PAT 重换） |
| `qoder.cn` | `WEB_BASE` | 设备授权页 `/device/selectAccounts`、官网账户页 `/api/v2/me/usages/big_model_credits` |
| `gateway.qoder.com.cn` | CN 区网关 | 对话上游 + 模型目录（COSY 签名域，`/algo` 前缀） |
| `api3.qoder.sh` | Global 区网关 | Global 账号上游（**目录双区、调度 v1 恒 CN**） |

### C.2 端点速查

| 用途 | 端点 | 要点 |
|---|---|---|
| 活动列表 | `GET {openapi}/sash/api/v1/me/campaigns` | 双活动（签到+档期）自然全覆盖 |
| 活动领取 | `POST {openapi}/sash/api/v1/me/campaigns/{campaignId}/claim` | **幂等**，重复领取安全 |
| 积分用量 | `GET {openapi}/sash/api/v2/me/usage` | Bearer + `cosy-clienttype: 10` + UA `Qoder` |
| PAT 重换作业令牌 | `POST {openapi}/api/v1/me/jobToken` | body `{clientId}`；实证路径必须居首（R-6） |
| 刷新（前缀分派） | `jrt-` → `POST {openapi}/api/v1/jobToken/refresh`；其余 → `/api/v1/deviceToken/refresh` | `refresh_endpoint_for()`；PAT 通道 refresh 48h、作业令牌 24h |
| 设备流轮询 | `GET {openapi}/api/v1/deviceToken/poll?nonce=&verifier=&challenge_method=S256` | **无 Authorization 头**，与客户端一致 |
| 对话上游 | `POST {cn}/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1` | SSE 信封；COSY 签名 + `encode_body` 三段轮转加密 |
| 模型目录 | `GET {cn}/algo/api/v2/model/list?Encode=1` | 签名 path 剥 `/algo` 后入摘要 |

### C.3 凭证前缀体系

| 前缀 | 含义 | 生命周期 |
|---|---|---|
| `pt-` | 个人访问令牌（用户手工生成导入） | 长期；经 `/api/v1/me/jobToken` 换 `jt-` 后使用 |
| `jt-` | 作业令牌（access） | 24h |
| `jrt-` | 作业令牌刷新令牌（PAT 通道产物） | 48h；走 `jobToken/refresh` |
| `dt-` | 设备流 access 令牌（OAuth 登录产物） | 惰性刷新走 `deviceToken/refresh` |
| `qd-id` | 设备指纹标识（入池生成一次，**永不轮换**） | 覆写 machineid / storage.json / state.vscdb 三处 |

### C.4 业务错误码（SSE 信封内层 `code`，HTTP 常为 200/403）

- **10605 = 模型排队中**：不是错误，唯一动作是「等一会儿再发」；触发退避（3 次 / 5s / 上限 30s）。排队信号优先于额度关键词判定（`credit exhausted` 嵌在排队消息里是实测陷阱）。
- **105 = 登录态失效** → `auth_dead`，需重导/重登。
- **额度类**：`110` 每日用量上限、`112` 额度耗尽、`113` 配额耗尽、`114` 试用用完、`115`–`118`、`119` 所选模型免费额度用完、`122` 计费组上限 → 模型/账号冷却。
- 精确匹配防误伤：`raw_has_code` 保证 `1053` 不命中 `105`、`10605` 不命中 `1060`（裸文本兜底解析同规则）。

### C.5 IDE 登录态扫描（qoder_ide_scan）

`state.vscdb`（SQLite）`ItemTable` 键 `secret://aicoding.auth.userInfo` → Chromium `os_crypt` v10 密文：Local State `os_crypt.encrypted_key`（`DPAPI` 前缀段解包 AES key）解密得 `dt-` 系凭证；旧格式直接 DPAPI。

### C.6 COSY 签名要点（六步，`qoder_sign.rs`）

身份 JSON 固定键序 → AES-128-CBC（key=iv）→ RSA-1024 PKCS#1 v1.5 加密 AES key → payload base64 → `MD5(payload\nkey\ntimestamp\nbody\npath)` → `Authorization: Bearer COSY.<payload>.<sig>`；`GATEWAY_COSY_VERSION="1.1.38"`、chat 域 `CLIENT_TYPE="5"`（openapi 积分域 `cosy-clienttype: 10`）、19 个 `Cosy-*` 头、`signature_path` 剥 `/algo`。上游升级换版本即全线失效（见 §8 风险表）。

## 11. 附录 D：豆包对话协议情报（原 doubao-api-feasibility.md 精华归并）

> E-01/E-02/E-03 的需求与实现路径见 [backlog.md](backlog.md)；本节保留协议层事实。

- **对话端点**：`POST www.doubao.com/samantha/chat/completion`（SSE）；三模式 `doubao` / `doubao-think` / `doubao-expert` → `completion_option` 参数组。
- **风控形态**：验证码墙而非拒绝服务——错误码 `710012001`（sessionid 吊销）/ `710022004`（需验证码，人工过后恢复）/ `712010702`（Cookie/设备指纹缺失或编码错误）；HTTP 200 无数据流 = 连续失败退避信号。
- **签名体系**：`msToken`（URL query + Cookie 双处）+ `a_bogus`（192 字符 = SM3 双哈希 + RC4 固定 keystream + s4 自定义 base64，**绑定单次请求的 query+UA+时间戳，必须纯算法生成**，嗅探只能短窗重放）。
- **设备指纹**：`ttwid` / `passport_csrf_token` / `device_id` / `web_id` / `tea_uuid`（19 位）；**必须与账号绑定且保持一致**——频繁更换 device_id 是风控高危信号；MITM 嗅探按账号落库（E-02）。
- **多模态**：生图 SSE `block_type=2074`（`creations[]`，`image.status==2` 完成，URL 优先级 `image_ori > image_raw > thumb`，漏图轮询 `/message_node_info` 兜底）；生视频 `content_type=2020` 下发 → `fin_reason.async_task.id` → `/samantha/chat/async/stream` 等 `2021`（1~3 分钟，需任务桥 + event_id 游标重连）；文件中转站 TOS 上传 ≤1GB 得永久 URI；多模态 bot_id `7338286299411103781`。
- **反封号组合拳**：限速 + 随机延迟 + 指数退避，UA 保持真实采样值；设备指纹静态化。

## 12. 附录 E：开源参考仓库映射（learn-the-design, write-our-own-code）

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
