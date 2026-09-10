# AGENT.md — AI Work 助手 (ai-work-assistant) v3.3.0

> 项目级别速查手册。给后续会话（人或 AI）秒接上下文用。任何会改契约的提交请同步更新本文档。
> 注：品牌已由 Trae Work Assistant 迁移为 **AI Work 助手（ai-work-assistant）**，本机仓库目录暂为 `trae-work-assistant`，后续可整体重命名。

## 1. 一句话

Windows 桌面端多账号签到 + 登录态切换 + 设备隔离 + API 网关一站式工作台，**深度支持 Trae Work 与 Trae（Trae CN IDE）双应用**（账号自动发现、切换/快照按目标应用独立、账号池 app 无关同池调度；桥档案表已预留豆包 / WorkBuddy）。**所有数据仅存在 `%APPDATA%\AIWorkAssistant\`，零外部网络**。

## 2. Quick Start

```powershell
# 仅 Windows，需要 Node 18+ / Rust stable (MSVC) / VS Build Tools C++ 工作负载 / WebView2
cd ai-work-assistant   # 本机目录暂为 trae-work-assistant，见文首说明
npm install
npm run tauri dev          # 开发模式（Tauri WebView 加载 Vite 5173）
npm run tauri build        # 打包 MSI + NSIS 到 src-tauri/target/release/bundle/
python scripts/rename_release.py   # 产物统一输出到 release/，中文命名 AI Work 助手_<版本>_x64*
```

测试：

```powershell
python src-python/tests/test_auto_checkin.py   # Python 纯函数单测
cargo test                                    # Rust 单测（需先装工具链）
```

## 3. 技术栈

| 层 | 技术 |
|---|---|
| 外壳 | Tauri 2.x (Rust 1.75+ MSVC) |
| 前端 | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 + lucide-react |
| 后端 | Rust (serde / chrono / axum / ureq / tauri-plugin-{shell,dialog,notification,single-instance}) |
| 辅助 | Python 3.9+（仅标准库 + `cryptography`）+ PowerShell 5.1+（系统自带） |

## 4. 目录地图

```
ai-work-assistant/
├── AGENT.md                      # 本文件（项目速查）
├── README.md                     # 用户文档
├── package.json / vite.config.ts / tsconfig.json / tailwind.config.js / postcss.config.js / index.html
├── docs/                         # user-manual / tech-framework / product-design / workbuddy-product-design / product-optimization-backlog / doubao-api-feasibility
├── src/                          # 前端
│   ├── App.tsx                   # 外壳（TitleBar + Sidebar + TopBar + 页面切换 + Toaster）
│   ├── store.ts                  # Zustand 单一真相（init / 刷新 / checkin/switch/saveLogin 事件归约）
│   ├── types.ts                  # 与 Rust DTO 对齐（snake_case）
│   ├── lib/                      # tauri.ts(invoke 封装+事件订阅) / themes.ts(主题) / delay.ts(withMinDelay) / cn.ts / about.ts / useIsDark.ts
│   ├── components/               # TitleBar/Sidebar/TopBar/Toaster/PageHeader/SetupGuide/ui + SystemDialog(系统设置+系统日志弹框)/GeneralSettingsPanel/AboutDialog
│   └── pages/                    # Dashboard / Accounts / Checkin / Credits / Logs / ApiService / Settings
├── scripts/                      # dev-tauri.mjs(tauri 脚本入口) / sync_version.py / rename_release.py / package_portable.py / make_portable_zip.py / gen_asset_base64.py
├── src-tauri/
│   ├── tauri.conf.json           # 无装饰窗 / bundle.resources = ../src-python/ + ../src-ps/
│   └── src/
│       ├── main.rs               # 注册全部命令
│       ├── state.rs              # AppState（%APPDATA%\AIWorkAssistant + python_dir + 旧目录迁移）
│       ├── models.rs             # DTO（含 CheckinSummary.time 字段）
│       ├── fs_utils.rs           # 原子 read_json / write_json / mask / 时间辅助
│       ├── jwt.rs                # parse() + status_of() + refresh() + oauth_parse()
│       ├── python.rs             # spawn_script（注入 AIWORKDATA_DIR）
│       ├── api_server/           # API 网关模块
│       │   ├── mod.rs            # 常量 + 路由注册
│       │   ├── server.rs         # axum 服务器启停
│       │   ├── routes.rs         # OpenAI /v1/chat/completions + Anthropic /v1/messages（SSE 流式 + 非流式，双协议输出）
│       │   ├── pool.rs           # 账号池调度（积分感知 + 冷却状态机 + 账号轮换，app 无关）
│       │   ├── payload.rs        # OpenAI/Anthropic 请求 → llm_utils_chat 改写（anthropic_to_openai 先转内部格式）
│       │   ├── sse.rs            # SSE 协议转换（SOLO → OpenAI chunk / Anthropic 事件流）
│       │   ├── auth.rs           # API Key 鉴权（Bearer + x-api-key 双风格）
│       │   ├── models_sync.rs    # 模型列表配置化（api_models.json）+ 官网 batch_get_detail_param 同步
│       │   └── api_logger.rs     # API 请求日志
│       └── commands/             # env / cert / proxy / accounts / checkin / switch / misc / profile / api_server / oauth / trae_apps(双应用发现) / process(三级关闭) / updater
├── src-python/
│   ├── device_proxy.py           # MITM 代理（env AIWORKDATA_DIR、--gen-ca）
│   ├── auto_checkin.py           # 批量签到（--json-stream / --accounts / --scope）
│   ├── requirements.txt          # cryptography
│   └── tests/test_auto_checkin.py
└── src-ps/trae-switch-bridge.ps1 # 非交互切换桥 + NDJSON 步骤输出
```

## 5. Tauri 命令契约

> **调用约定**：invoke 的**顶层参数名**跟随 Rust 函数签名（驼峰不替换，参数名直接匹配）。**嵌套对象**（`opts` / `patch`）的字段名保持 **snake_case**（Tauri 默认 serde 字段名，不做 camelCase 转换）。

| 模块 | 命令 | 说明 |
|---|---|---|
| 环境 | `env_check` → `EnvStatus` | `installed/running/version/path`；async 命令（注册表全量搜索较慢，避免 UI 卡顿） |
| 环境 | `open_trae_website()` / `open_trae_app()` | 打开 Trae 官网 / 启动 Trae Work（代理注入时优雅关闭进程最长 5s，async） |
| 环境 | `env_check_trae_cn()` / `open_trae_cn_app()` | Trae CN IDE 环境检测 / 启动（双应用支持） |
| 证书 | `cert_status` / `cert_install` | 安装走 UAC `certutil -addstore -f Root` |
| 代理 | `proxy_start(port)` / `proxy_stop()` / `proxy_status()` | ProxyStatus：`running/port/captured/started_at` |
| 账号 | `accounts_list` → `AccountView[]` | 聚合 JWT / 分组 / 设备 / 积分 / 今日 |
| 账号 | `account_add_manual(name, jwt, groupId?)` | 解析 JWT → userId 入库 |
| 账号 | `account_delete(userId, deleteProfile)` | 同时清分组；`deleteProfile=true` 删 profiles/<uid> |
| 账号 | `account_update(...)` / `accounts_export_raw()` / `accounts_import(...)` | 编辑账号 / 原始 JSON 导出 / 导入 |
| 积分 | `fetch_remaining_credits` / `fetch_credit_detail` / `refresh_remaining_credits` | 剩余积分查询 / 明细 / 刷新 |
| 积分 | `credits_daily_list` / `credits_history` / `invite_link()` | 每日快照 / 历史记录 / 邀请链接 |
| 冷却 | `cooldown_clear(userId?)` / `cooldown_clear_all()` | 清除签到错误冷却状态 |
| 账号 | `account_oauth_add` → 实际为 `oauth_login(callback_url, account_name?, group_id?)` | 从 OAuth 回调 URL 解析 token + userInfo |
| OAuth | `oauth_get_login_url()` → `{ url }` | 构造 Trae 登录 URL |
| OAuth | `oauth_parse_callback(callback_url)` → `{ user_id, ... }` | 解析回调 URL 中的 token |
| 分组 | `groups_list` / `group_create` / `group_update` / `group_delete` / `group_move` | 删除分组时账号回落「未分组」 |
| 签到 | `checkin_start(opts)` → NDJSON 事件 | `opts: { scope, user_ids?, skip_checked_in, skip_expired }`；失败自动重试最多 2 轮（30s/90s，T5） |
| 签到 | `checkin_trends(days?)` → `CheckinTrendPoint[]` | 近 N 天签到结果按日汇总（T8，data/checkin_results.json，保留 90 天） |
| 环境 | `app_locate(targetApp)` → `AppLocate` | 四应用安装位置四级探测（手动指定→注册表→默认路径→进程反查，F-01）；`targetApp: trae_work\|trae\|doubao\|workbuddy` |
| 环境 | `open_doubao_app()` | 启动豆包桌面版（复用 app_locate 豆包档案探测） |
| 切换 | `switch_account(userId)` | 调 `trae-switch-bridge.ps1 -Action Switch`（进程三级关闭策略）；`target_app` 支持 TraeWork/Trae/Doubao |
| 切换 | `reset_device_ids(userId)` | switch 模块：重置设备指纹（区别于 misc 的 `device_reset` 只删映射）；仅 icube 布局 |
| 保存 | `save_current_login(userId)` | 调 `trae-switch-bridge.ps1 -Action SaveCurrentLogin`；`target_app` 支持 TraeWork/Trae/Doubao |
| 快照 | `profile_list` → `ProfileInfo[]` | 列出快照槽；`target_app` 决定根目录 profiles / profiles_trae / profiles_doubao |
| 快照 | `profile_backup(userId)` / `profile_restore(userId)` / `profile_delete(slot)` | 手动备份/恢复/删除；`target_app` 同上 |
| 快照 | `profile_format_size(...)` | 快照体积格式化 |
| 豆包 | `doubao_accounts_list` → `DoubaoAccountView[]` | 账号池 ∪ profiles_doubao 快照槽合并视图 + 当前账号标记 + 会话状态（last 槽与 `*.bak` 单代回滚槽不展示） |
| 豆包 | `doubao_account_save(userId, name?, note?)` / `doubao_account_remove(userId)` | 豆包账号池 upsert / 移除（data/doubao_accounts.json） |
| 豆包 | `doubao_account_set_credential(userId, sessionId?, sidGuard?, ttwid?)` | 编辑弹框保存会话凭证（已存值回填；清空保存即删除；ttwid 仅非空时更新；账号不在池时自动入池） |
| 豆包 | `doubao_captured_credential()` / `doubao_credential_auto_apply()` | 读 device_proxy.py 抓包落盘的 data/doubao_captured_credentials.json（doubao.com Cookie 中的 sessionid/sid_guard/ttwid）；auto_apply 目标 = **抓包文件自带 uid**（multi_sids 按同一条 sessionid 匹配的主人，凭证与归属同源自洽），且**只回写已入池账号、绝不自动建号**（网页版/其他字节系应用抓到的陌生会话跳过并记 app_log；新账号一律走「保存当前登录态」），前端账号页每 20s 轮询；另有 `doubao_captured_credential` 供编辑弹框手动填充 |
| 豆包 | `doubao_detect_uid()` | **主来源**：Local Storage leveldb 的 `client_device_info.userId`（客户端每次启动自写、**不依赖代理**；`doubao_chats.py --detect-uid` 解析并按时间戳与抓包文件比新鲜度取新者——无代理重登新账号也能识别，实测 2026-09-09）；**兜底①**：抓包文件 uid（multi_sids）→ `%LOCALAPPDATA%\Doubao\User Data\Local State` 的 saman.user_id（**同 profile 重登不更新**，只作兜底）；**兜底②**：`%APPDATA%\Doubao\public_config.json` 全树递归搜（text_picker 是输入法选择器缓存，**不随登录切换更新**，勿当主来源——bug1 根因）；**兜底③**：profiles_doubao/current_account.txt；含单元测试。局限：客户端**会话内**换登录不重启时 client_device_info 不刷新，重启豆包后即正确 |
| 豆包 | `doubao_keepalive_run()` | 续期主路径：调 PS 桥 `-Action KeepAlive`（启动豆包 8s 联网滑动续期 → 优雅关闭，运行中跳过），NDJSON → keepalive-progress/done 事件，成功后记池级 last_keepalive_at + 运维历史 |
| 豆包 | `doubao_renew_run(syncOnly?)` | 调 python doubao_renew.py：探活巡检（仅手动录入凭证的账号，200=有效/302→passport=过期）或 cookie 诊断（--sync-only，实测客户端 cookie 为二次加密密文，不能当凭证）；结果记运维历史 |
| 豆包 | `doubao_renew_task_register(time)` / `..._status()` / `..._unregister()` | schtasks 每日保活任务 AIWorkAssistant_DoubaoRenew（/TR 调 PS 桥 KeepAlive） |
| 豆包 | `doubao_quota_fetch(userId)` | 调 python doubao_quota.py 查会员额度：POST 默认接口 `/alice/commerce/sale/subscription/quota/summary/`（body `{"product_line":"membership"}`，settings.doubao_quota_url 可改）+ 账号凭证；精确解析（套餐/到期/活动赠送/订阅记录/当前时段+近7天窗口含重置时间）+ 宽容兜底，成功后回写账号池额度缓存与运维历史；保活端点默认 `/info/v2/`（settings.doubao_renew_url 可改；state.rs 启动迁移回填默认值） |
| 豆包 | `doubao_quota_task_register(time)` / `..._status()` / `..._unregister()` | schtasks 每日额度巡检任务 AIWorkAssistant_DoubaoQuotaCheck（/TR 调 `doubao_quota.py --all`：批量查池内有凭证账号 → 回写缓存 + 运维历史 + 用完记录） |
| 豆包 | `doubao_history()` | 读 data/doubao_health_history.json 运维事件（keepalive/renew/quota，滚动 400 条；quota 事件含 windows 额度窗口），概述页额度趋势图与健康度卡数据源；写入方：keepalive_run/renew_run/quota_fetch（source=app）+ 定时任务（source=task） |
| 豆包 | `open_doubao_app(proxyPort?)` | 打开豆包桌面版；proxy_port 存在时注入 `--proxy-server`（启动前三级关闭现有进程确保参数生效，对齐 Trae 打开逻辑），凭证/额度抓取不依赖系统代理 |
| 豆包 | `doubao_open_as_account(userId, proxyPort?)` | **C1 一键以账号打开**：调 PS 桥 `-Action Switch -TargetApp Doubao -ProxyPort <port>`（恢复该账号快照后直接拉起客户端，把「切换 → 等待 → 打开」两步合并为一步）；proxyPort>0 时桥层注入 `--proxy-server`；NDJSON 进度复用 switch-progress / switch-done 事件管线（前端走 store.openDoubaoAs，与 switchTo 互斥共用 switchingTo 状态） |
| 豆包 | `doubao_snapshot_meta(userId)` → `DoubaoSnapshotMeta?` | **C3 快照版本校验**：读 `profiles_doubao/<uid>/snapshot_meta.json`（schemaVersion / createdAt / chromiumVersion / includeIndexedDB），无元数据文件时回退读快照内 `Last Version`（返回 schema_version=0 标记为旧版快照）；账号页快照列「已保存」处悬停展示版本信息 |
| 豆包 | `settings.doubao_snapshot_include_idb` | **C4 IndexedDB 可选纳入快照**：默认 false（体积大，默认排除）；开启后 profile_backup / profile_restore / switch_account / save_current_login 透传 `-IncludeIndexedDB` 给 PS 桥；桥层备份时纳入 `Default/IndexedDB`，恢复时**只要快照内含就回写**（不看当前开关，保证快照完整回写） |
| 豆包 | `doubao_chatdata_backup(userId)` / `doubao_chatdata_restore(userId)` / `doubao_chatdata_info(userId)` | **D1 对话数据独立备份/恢复**：源=豆包 User Data 各 Profile 下 IndexedDB（chrome_doubao-* / https_www.doubao.com*）+ DoubaoStorage → `data/doubao_chats/<uid>/`（按 profile 名分层 + chat_backup_meta.json，覆盖式）；备份/恢复均先 graceful_kill_app("Doubao")；恢复按 profile 名回写；与快照解耦（对话正文在云端跟账号走，本地备份的是客户端状态，换机/重装后恢复备份+登录即可同步对话）；info 供账号行「对话已备份」徽标 |
| 豆包 | `doubao_export_chats(userId)` | **D2 对话记录导出**：调 python doubao_chats.py `--export --uid X`（AIWORKDATA_DIR/UTF-8 环境、CREATE_NO_WINDOW，stdout 末行 JSON 解析）；走官方 IM 接口 `POST www.doubao.com/im/chain/recent_conv`（cmd 3200 会话列表，conv_version=0 首跳 limit≤50）+ `im/chain/single`（cmd 3100 单会话消息，anchor_index=2^53-1 起翻页，index_in_conv 为字符串需 int 转换）；必需 Cookie：sessionid/sessionid_ss/sid_tt + sid_guard + **ttwid**（登录校验），sid_guard/ttwid 必须以 Set-Cookie 下发的 **URL 编码原样**发送（原始 | 形式报 712010702，_cookie_enc 兼容两种存储形式），query 必须含设备指纹 web_id/tea_uuid/fp（缺失报 712010702）+ 头 agw-js-conv:str + UA SamanthaDoubao；输出 markdown+json 到 data/exports/doubao_chats_<uid>_<ts>.*；正文提取 content_block text_block → tts_content/brief 兜底；池内凭证过期（客户端重新登录后 sessionid 轮换）时需重开代理自动回写 |
| 日志 | `proxy_logs_list(...)` / `proxy_log_detail(...)` | 代理请求日志列表 / 详情 |
| 文件 | `read_text_file(path)` / `write_text_file(...)` | 前端通用文本读写（read 有 10MB 上限 + 常规文件校验） |
| 双应用 | `apps_accounts_discover` / `apps_account_add` / `apps_entitlement_read` | 本机 Trae Work + Trae CN 账号自动发现 / 入池 / 套餐读取（F-08，见 §5.1） |
| 双应用 | `refresh_pay_status(...)` / `accounts_backfill_dc_ids(...)` | 会员支付状态刷新 / 已有账号回填 dc id |
| 设备 | `device_reset(userId)` | 删 `device_map.json[ uid ]` |
| JWT | `jwt_parse(jwt)` / `refresh_jwt(userId)` | 解析 / 自动刷新（需 refresh_token） |
| API | `api_server_start()` / `api_server_stop()` / `api_server_status()` | API 网关启停（端口/默认模型由设置页提供；鉴权统一走 API Keys 列表） |
| API | `pool_list` / `pool_set` / `pool_status` | 账号池管理；`pool_set` 扩展 `strategy` / `group_ids`（T10 调度策略与分组筛选） |
| API | `api_debug_toggle` / `api_debug_status` | API 请求日志开关 |
| API | `api_models_list()` / `api_models_sync()` | 模型列表读取（data/api_models.json）/ 官网同步（不消耗积分，最多试 3 账号） |
| API | `api_logs_list(...)` / `api_logs_detail(...)` / `api_logs_search(...)` | API 请求日志查询 / 详情 / 搜索 |
| API | `api_usage_stats(days?)` → `UsageDayView[]` | 网关用量按日统计（T1，data/api_usage.json，保留 90 天，直读落盘） |
| API | `api_keys_list()` / `api_keys_save(keys)` | 多 API Key 列表管理（T2，data/api_keys.json，每日配额；主 Key 双轨已移除） |
| 日志 | `logs_query({ opts: { log_type, date, keyword, limit } })` → `LogLine[]` | `split_time` 会 strip BOM 前缀 |
| 日志 | `logs_clear(log_type)` → `u32` | 按类型删除日志文件（all/proxy/checkin/switch，T6，幂等） |
| 设置 | `settings_get()` / `settings_set(patch: Settings)` | Settings 全部 snake_case |
| 设置 | `autostart_status()` / `autostart_set(enabled)` | 开机自启查询 / 开关（T11，即时生效）；配套 `settings.silent_checkin` 启动静默签到 |
| 计划 | `task_register(time)` / `task_status()` / `task_unregister()` | `schtasks` 注册每日签到 |
| 更新 | `update_check()` / `update_download(...)` → `UpdateDownloaded` / `update_run_installer({file_path, asset_name})` | 两步确认制：下载（确认一）→ 安装（确认二）。安装器参数 `/P /UPDATE /R`：被动进度条 + 跳过卸载直接覆盖 + 完成后自动重启应用；`run_installer` 校验路径必须位于临时更新目录 |

### 5.1 双应用与双 uid 体系（F-08，trae_apps.rs）

- 数据源：`%APPDATA%\TRAE SOLO CN`（Trae Work）与 `%APPDATA%\Trae CN`（Trae CN IDE）各自的 `User\globalStorage\storage.json` / `state.vscdb`。
- **两套 uid 体系不通用（红线）**：`iCubeAuthInfo://icube-dc:<uid>` 键名中的 uid 是**账户中心（dc）id 空间**；账号池 / JWT `data.id` 用的是 **Cloud-IDE id 空间**。同一登录账号两者数值不同，直接混用会导致重复入池。
- 当前登录账号的 Cloud-IDE uid 由使用痕迹推导（Trae CN 看 `icube_gtm.users` 键名；Trae Work 看 state.vscdb `solo.mobile.allowControl` per-uid 最新 `updatedTime` + 键名证据计数），推导失败时回退展示 dc uid 并标记 `uid_confident=false`、**禁止入池**。

## 6. Tauri 事件（Rust → 前端）

| 事件 | payload |
|---|---|
| `proxy-log` | `string`（代理 stdout 逐行） |
| `account-captured` | `string`（新捕获的 userId） |
| `checkin-progress` | `{type:'start',total}` / `{type:'account',...}` / `{type:'done',ok,already,failed}` |
| `switch-progress` | `string`（PowerShell NDJSON 单行） |
| `switch-done` | `{ success: boolean, raw: string }` |
| `save-login-progress` | `string`（PowerShell NDJSON 单行） |
| `save-login-done` | `{ success: boolean, raw: string }` |
| `update-download-progress` | `{ received, total, percent }`（更新包下载进度） |
| `update-installing` | `string`（asset_name，安装器已启动、应用即将退出） |

## 7. 数据文件

```
%APPDATA%\AIWorkAssistant\
├── conf/
│   └── app_settings.json         # Settings 全字段（snake_case）
├── data/
│   ├── checkin_accounts.json     # { accounts: [{name, UserID, jwt, refresh_token?, added_at}] }
│   ├── device_map.json           # { <userId>: { device_id, market_user_id, session_id } }
│   ├── groups.json               # { groups: [...], membership: {<uid>:<gid>} }
│   ├── credits_history.json      # { records: [{date,user_id,credits,delta}] }
│   ├── credits_daily.json        # 每日积分快照
│   ├── remaining_credits.json    # 各账号剩余积分缓存
│   ├── account_cooldowns.json    # 签到错误冷却状态（error_type + cooldown_until）
│   ├── api_pool.json             # API 账号池配置 + 状态
│   ├── api_models.json           # 模型下拉列表（id=config_name 原样透传，label=官方展示名；3.2.6 起位于 data/ 子目录，旧位置自动兼容迁移）
│   └── profiles/                 # 登录态快照
│       ├── current_account.txt   # 当前活跃账号 ID
│       └── <user_id>/            # 精准备份的 9 类核心文件
└── logs/                        # proxy / checkin / switcher / api / proxy-requests 日志
```

**写入约定**：`fs_utils::write_json` 用 `tmp + rename` 原子替换，避免断电损坏。

## 8. PowerShell 切换桥约定

- **非交互模式**：不需要 `#Requires RunAsAdministrator`，普通用户即可运行。
- `-Json` 时输出 NDJSON 单行 `{"stage":"...","status":"...","message":"...","time":"..."}`。
- 入口目录：`$env:APPDATA\TRAE SOLO CN` + `$env:APPDATA\AIWorkAssistant\data\profiles`。
- **Action 参数**：`Switch` / `SaveCurrentLogin` / `ResetMachineId` / `ResetDeviceIds` / `BackupCurrent` / `RestoreOnly` / `KeepAlive`。
- **通用参数**：`-TargetApp TraeWork|Trae|Doubao|WorkBuddy`、`-Json`、`-ProxyPort <int>`（C1：>0 时启动应用注入 `--proxy-server`）、`-IncludeIndexedDB`（C4：备份纳入 `Default/IndexedDB`）。
- **精准备份**：仅复制 9 类核心登录文件（storage.json / state.vscdb / machineid / aha / Network 等），非全量镜像。
- **Switch 流程**：预检查目标快照 → 关闭 Trae Work → 保存当前到 last + 当前账号槽位 → 恢复目标 → 启动。
- **SaveCurrentLogin 流程**：关闭 Trae Work → 精准备份到 userId 槽位 → 启动。
- storage.json 路径：`User\globalStorage\storage.json`，键名用点号访问（`$storage.'telemetry.machineId'`）。
- **豆包数据目录**：`%LOCALAPPDATA%\Doubao\User Data`（Trae 系用 `%APPDATA%`）；备份项含 Local State / Network/Cookies* / Local Storage/leveldb / Session Storage / DoubaoStorage / saman_app_state / saman_shell_db_storage，`Last Version`（C3 版本基线）。
- **C3 快照元数据与校验**：备份时写 `snapshot_meta.json`（`schemaVersion=1` / createdAt / chromiumVersion / includeIndexedDB）并复制 `Last Version`；恢复前 `Test-SnapshotIntegrity` 三层校验——① schemaVersion ≠ 1 直接中止（无元数据文件的旧快照仅 warn 并跳过）② leveldb 缺 CURRENT 或 CURRENT 指向的 MANIFEST 缺失 → 中止 ③ 快照版本 ≠ 当前安装版本 → 仅 warn 继续恢复。
- **单代回滚保护（chromium 布局）**：`Backup-ChromiumProfile` 覆盖已有槽位前把旧快照整体 `Move-Item` 到 `<slot>.bak`（旧 .bak 淘汰）；`Restore-ChromiumProfile` 主槽缺失时回退用 .bak，Switch 预检查同样放行 .bak。背景：Switch 的"备份当前到来源槽"依赖 current_account.txt 与客户端实际登录一致，不一致时会把错误状态反复刷进该槽且不可恢复（实测把 B 快照覆盖成混乱状态）。`Copy-SnapshotItem` 文件分支先删旧目标再拷贝——文件被锁拷贝失败时不会留下旧文件冒充成功；`Copy-SnapshotItem`/`Test-SnapshotIntegrity` 的参数为最终路径（`-Path`），由调用方解析主槽或 .bak。豆包优雅关闭等待 `GracefulWaitSecs=8`（chromium 落盘慢，3 秒强杀会导致文件锁/未落盘）。
- **防误覆盖守卫（ExpectedCurrentUid，chromium 布局）**：桌面端 Switch 前用 `detect_guard_uid_strict`（Local Storage/抓包新鲜度链检测 uid + **Live Cookies 登录会话验证**）取当前登录，经 `-ExpectedCurrentUid` 传给桥；桥仅在它与 current_account.txt **一致**时才把"当前态"回写进来源账号槽，否则只备份 last 槽并 warn（客户端手动重登/未登录/检测失败时保护账号快照不被错误状态覆盖）。`doubao_open_as_account` 与 `switch_account`（豆包路径）均接入。.bak/last 槽不在账号列表展示（`doubao_accounts_list` 过滤 `*.bak`）。
- **登录会话 Cookie 检测（doubao_chats.py `--check-login-cookie <dir>`）**：Chromium Cookies 库的 cookie **名**为明文（值加密不影响），sqlite 判定 `host_key like %doubao.com` 且 name∈(sessionid,sid_guard) 是否存在；客户端运行中先复制 Cookies* 到临时目录再读。返回 `{ok, doubao_cookies, has_session}`。用途①`save_current_login` 保存前预检 Live profile（无登录会话 → 拒绝保存，防止未登录态入槽）；用途②`doubao_open_as_account` 目标槽预检（快照无登录会话 → 拦截并提示重存）；用途③切换守卫严格版（uid 检测可能被快照 localStorage 残留骗过——实测未登录客户端仍报旧 uid 导致守卫误放行，Cookie 存在性无法伪造）。Rust 侧 `check_profile_login_cookie` 返回 None（脚本缺失/读库失败）时一律不阻断，保持可用性。

## 9. Python 约定

- **数据目录**：通过 `os.environ["AIWORKDATA_DIR"]` 注入（Rust `spawn_script` 负责；Python 侧兼容读旧变量名 `TRAEDATA_DIR`），缺省回退到脚本所在目录。
- **NDJSON**：`--json-stream` 输出 `{"type":"start"|"account"|"done",...}` 单行 JSON。
- **稳定设备 ID**：`device_map.json` 缺条目时由 `rand_digits(n, seed=user_id)` 派生。
- **docstring**：包含 Windows 路径时**必须用 raw 字符串 `r"""..."""`**。
- **上游代理链（v2.4.3）**：`device_proxy.py` 读取 `UPSTREAM_PROXY`（可选 `UPSTREAM_PROXY_USER` / `UPSTREAM_PROXY_PASS`），支持 `http://host:port` 与 `socks5://host:port` 两种形态。**非 Trae 域名**的 CONNECT 隧道（`tunnel_raw`）与明文 HTTP 转发优先经上游出站，上游不可用时回退直连；Trae 域名仍走本地 MITM 解密以捕获 JWT。
- **动态叶子证书 AKI（2026-09-09）**：`leaf_cert` 签发的叶子证书必须带 Authority Key Identifier（OpenSSL 3.2+/Python 3.13 客户端缺 AKI 即拒：`MISSING_AUTHORITY_KEY_IDENTIFIER`）；AKI 取 CA 的 SKI，无则由 CA 公钥派生。**只补叶子、不改 CA**——CA 已被用户安装信任，改 CA 内容会使其失效（须重装证书）。
- **工具自身出站请求不走代理**：doubao_renew.renew_probe / doubao_quota.query_account 显式 `ProxyHandler({})` 绕过系统代理直连——巡检无需 MITM，且系统代理开启时会撞上动态证书兼容性问题（本轮 SSL 报错根因）。

## 9.1 代理生命周期约定（v2.4.3）

- `proxy_start` **先**通过 `get_existing_win_proxy()` 读取当前系统代理（即用户的 VPN），作为 `UPSTREAM_PROXY` 注入 Python 进程，**再**用 `set_win_proxy` 改写为 `127.0.0.1:<port>`。顺序不可颠倒，否则会把自己当成上游造成死循环。
- `proxy_stop` 与看门狗**原样还原**启动前捕获的 `ProxyEnable` / `ProxyServer` / `ProxyOverride`，而非简单置 0，避免破坏 VPN 设置。
- `tunnel_raw` **必须**先回 `HTTP/1.1 200 Connection Established` 客户端才会发起 TLS 握手；连接上游失败时回 `502 Bad Gateway`，不可静默返回。

## 10. 前端约定

- **store 单例**：`useAppStore` 聚合所有状态；`init()` 在 `App.tsx` `useEffect` 启动一次。
- **主题系统**：Tailwind 3 + `darkMode:'class'`；`lib/themes.ts` 定义 6 套主题（石墨灰浅色 / 炭黑 / 暗夜紫 / 墨绿 / 琥珀暖夜 / 科技蓝，默认 charcoal），通过 `data-theme` 属性驱动，与 `index.css` 中的覆盖块一一对应——新增主题必须两处同步。左下角系统图标弹框（SystemDialog）= Tab1 系统设置（GeneralSettingsPanel：外观/语言/通知/代理）+ Tab2 系统日志（复用 Logs 页）。
- **snake_case**：前端类型定义（`types.ts`）的字段名与 Rust DTO 完全一致。
- **路由**：极简 `useState`，不引 react-router。
- **Modal**：不支持 `window.confirm()`，使用自定义 `Modal` 组件（支持 `size="lg"|"xl"`）。
- **按钮反馈**：所有异步按钮动作使用 `withMinDelay(promise, 1000)` 确保最少 1 秒 loading。

## 11. 常用任务 SOP

| 任务 | 路径 |
|---|---|
| 新增账号 | Accounts 页 → OAuth 登录 或 手动粘贴 JWT → 选分组 → 入库 |
| 保存登录态 | Accounts 行 → Save 图标 → `save_current_login(userId)` |
| 切换账号 | Accounts 行 → LogIn 图标 → `switch_account(userId)` |
| 快照管理 | Accounts 页 → 快照管理按钮 → 查看/备份/恢复/删除 |
| 重置设备 ID | Accounts 行 → RotateCcw 图标 → `device_reset(userId)` |
| 注册定时签到 | Settings 页 → 输入 `HH:MM` → 注册任务 |
| API 服务 | ApiService 页 → 配置端口/API Key → 选账号池 → 启动 |

## 11.1 版本号升级规则（每次提交）

语义化版本 `MAJOR.MINOR.PATCH`（如 3.1.0），按本次提交内容判断：

> **升版时机（红线）**：**不要随意升版**——只有用户明确说「升级版本」时才升版并走全流程：**调整版本（set-version + CHANGELOG）→ 编译（tauri build 产出 setup / msi / portable 三件套）→ 提交 → 推送 → 打 tag 并推送 → 发布 GitHub Release（上传三资产）**。日常提交 / bug 修复一律不动版本号；下表的升位判断标准仅在用户主动发版时用于确定升哪一位。

| 提交内容 | 升级位 | 示例 |
|---|---|---|
| 新增一个完整的有意义的功能 | **中位（MINOR）** | 3.0.0 → 3.1.0 |
| 修复 bug / 功能优化 / 微小功能新增或调整 | **低位（PATCH）** | 3.1.0 → 3.1.1 |

- 大位（MAJOR）仅在重大架构/破坏性变更时升级
- 升版提交执行 `npm run set-version <x.y.z>` 一键同步（底层 `scripts/sync_version.py`：package.json / Cargo.toml / Cargo.lock / AGENT.md 标题）；CHANGELOG.md 手动新增条目
- 版本号单一来源为 `src-tauri/Cargo.toml`：tauri.conf.json 不写 version（自动回退），Rust 端 `env!("CARGO_PKG_VERSION")` 自动取，前端关于页运行时经 `getVersion()` 读取（about.ts 不写版本号），NSIS / MSI 安装包版本号自动跟随
- 一个提交包含多类变更时，按最高级别升位；纯文档/注释改动不升级；版本同步提交本身不再升位
- **GitHub Release 标题固定格式**：`v{MAJOR}.{MINOR}.{PATCH} 版本发布`（如 `v3.1.1 版本发布`），不额外加描述后缀

## 12. 安全与合规

- **零外发**：不连接任何自有后端。
- **CA 证书**：仅本地回环 `127.0.0.1:8899`，自签根 CA 需 UAC 安装。
- **UAC**：仅在 `cert_install` 提权，切换桥已改为普通用户可运行。
- **API Key**：留空时跳过鉴权；配置时在前端掩码显示（前 4 + 后 4 + ****）。鉴权头支持 `Authorization: Bearer <key>`（OpenAI 风格）与 `x-api-key: <key>`（Anthropic 风格）双风格。
- **API 网关**：v2.0 已实现本地 API 网关（axum + ureq），上游 `trae-api-cn.mchost.guru`。端点：`GET /health`（免鉴权）、`GET /status`、`GET /v1/models`（与 `data/api_models.json` 同源，官网同步后无需重启即可见最新列表）、`POST /v1/chat/completions`（OpenAI 协议）、`POST /v1/messages`（Anthropic Messages 协议，F-39）。请求侧统一转 OpenAI 内部格式复用池调度链路，响应侧按协议分别输出；Anthropic 流式事件序列 message_start → content_block_* → message_delta → message_stop，reasoning_content 暂不输出（thinking 块需签名）。账号池 app 无关：Trae / Trae Work 账号入池即被同一网关服务，扣通用积分（product_id 208）。

## 13. 禁止与红线（Do NOT）

- ❌ 修改 Rust 命令嵌套参数（如 `CheckinOpts`）的字段名 → 与 Python 子进程 serde 契约耦合。
- ❌ 把 Rust 命令顶层参数改为 camelCase → Tauri 用 Rust 函数签名原名匹配。
- ❌ 引入 React Router / Redux / 额外 UI 库 → 保持依赖最小。
- ❌ 提交 `.workbuddy/`、`dist/`、`node_modules/`、`src-tauri/target/`、`__pycache__/`、`data/`（已在 `.gitignore`）。
- ❌ 使用 `window.confirm()` → Tauri WebView 不支持，用自定义 Modal。
- ❌ 使用 `api.prevent_close()` → 会导致 Chromium 1412 错误。
- ❌ 用 `npm run dev` 直接跑 Vite → 白屏，必须 `npm run tauri dev`（实际入口为 `scripts/dev-tauri.mjs`）。
- ❌ 混用 dc uid 与 Cloud-IDE uid 入池 → 两套 id 空间不通用，会产生重复账号（见 §5.1）。

## 14. 已知约束

- 仅 Windows（代理证书安装 + MachineGuid 重置只在 Windows 验证）。
- PowerShell 切换桥需 Win10/11 自带 PowerShell 5.1+。
- `profiles_dir` 路径为 `data_dir.join("data").join("profiles")`，注意 `data/` 子目录。
- LLM API 上游必须设置 `NO_PROXY=*` 避免系统代理循环。
- 日志文件首行可能有 BOM 前缀（PowerShell 5.1 `-Encoding UTF8`），`split_time` 已处理。
- JWT 默认 13 天过期；带 refresh_token 的账号可自动续期。
- **`schtasks` 中文输出是 GBK**，直接 `String::from_utf8_lossy` 会乱码。统一走 `misc.rs::run_schtasks()`（前置 `chcp 65001`），**不要**再裸调 `Command::new("schtasks")`。
- **计划任务不加 `/RL HIGHEST`**：签到脚本只读写 `%APPDATA%` 并运行 Python，加了会让普通用户注册失败（Access Denied）。
- **错误文案不重复加前缀**：Rust 端返回纯错误描述，`查询失败：` / `注册失败：` 等前缀由前端 `Settings.tsx` 统一拼接。
- **`src-python/` 会打包进 `resources/python/`**：Python 侧改动在正式版必须 `npm run tauri build` 重新打包才生效；`npm run tauri dev` 直读源码，重启对应功能即生效。
- **`src-python/` 严禁混入 Python 运行时**（python.exe / python313.dll / Lib / libs 等）：会被打进 resources，且 `state.rs` 优先内嵌解释器。解释器探测（内嵌与系统 python/python3/py）统一用 `import encodings` 自举验证（`python_can_bootstrap`），`--version` 不触发 stdlib 导入、残缺运行时也能通过；内嵌不可用时自动回退系统解释器（v3.2.3 教训：3.2.0–3.2.2 携带缺 encodings 的残缺运行时致签到必崩，NSIS 覆盖安装不清理旧资源文件，靠自举回退兜底）。
- **进程三级关闭策略（F-47，process.rs）**：优雅关闭（taskkill 不带 /F 发 WM_CLOSE，等 3s 让 Electron 正常落盘）→ 树杀（/T /F，等 2s）→ 仍存活则返回 Err 由前端提示人工介入。仅按主程序映像名精确匹配；所有子进程以 CREATE_NO_WINDOW 拉起。
- **API 模型同步**：官网同步重放 Trae 客户端 `batch_get_detail_param` 配置接口；内置模型 glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code 不在配置接口响应中，需经 llm_utils_chat 以 `function=solo_agent` 调用补齐。
- **品牌迁移（v3.0.0）**：identifier `com.traework.assistant`→`com.aiwork.assistant`，数据目录 `%APPDATA%\TraeWorkAssistant`→`AIWorkAssistant`（`state.rs::migrate_legacy_dirs` 启动时**复制**迁移——旧目录原地保留，老应用可继续使用、两版并存；新目录已有数据则跳过；含 WebView2 目录，排除 Cache/GPUCache 等 8 类缓存子目录，复制失败回滚半成品），计划任务由 `misc.rs::try_migrate_legacy_task` 按旧触发时间重建（**旧任务保留**，`task_unregister` 只删新任务）。环境变量统一为 `AIWORKDATA_DIR`（Python 侧兼容读旧 `TRAEDATA_DIR`）。
- **老安装包升级**：升级兼容按**安装时产品名**判定（非版本号）。NSIS 通过 `build-assets/installer-hooks.nsh` 静默卸载清理旧品牌「Trae Work 助手」安装（已发布的 v2.4.4 及更早均属旧品牌，UTF-8 with BOM）；「AI Work 助手」品牌（v3.0.0 起）走 NSIS 原生原地升级；老 MSI 因 UpgradeCode 随 identifier 变化无法原地升级，需先卸载或改用 NSIS 包升级。打包产物统一输出到 `release/`，使用中文产品名命名 `AI Work 助手_<版本>_x64*`（`scripts/rename_release.py`）。
- **版本线与数据迁移**：新版本自 v3.0.0 起，**之前所有 2.x 版本升级到 3.x 均需数据迁移（安装/首次启动自动完成）**；原「Trae Work 助手」产品线在 `trae_work_main` 分支维护（仅 Trae Work 单应用，2.x.x，仅必要修复），仅使用 Trae Work 的用户可不升级，用该分支的 v2.x.x 最新版本即可。
- **NSIS 安装器**：使用自定义模板 `build-assets/installer.nsi`（基于 tauri v2.11.4 上游模板，配置于 tauri.conf.json `bundle.windows.nsis.template`）——升级安装时跳过「卸载旧版/不卸载」选择页，**默认直接覆盖安装**（同版本重装/降级仍显示选择页）。升级 Tauri CLI 后如构建报错，需从对应版本 tag 的 `crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi` 重新同步模板并重做定制。
