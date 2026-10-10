# AGENT.md — AI Work 助手 (ai-work-assistant) v3.7.5

> 项目级别速查手册。给后续会话（人或 AI）秒接上下文用。任何会改契约的提交请同步更新本文档。
> 注：品牌已由 Trae Work Assistant 迁移为 **AI Work 助手（ai-work-assistant）**，本机仓库目录暂为 `trae-work-assistant`，后续可整体重命名。

## 1. 一句话

Windows / macOS 双平台桌面端多账号签到 + 登录态切换 + 设备隔离 + API 网关一站式工作台，**深度支持 Trae Work / Trae（Trae CN IDE）/ WorkBuddy / CodeBuddy / 豆包 / Qoder 六应用**（账号自动发现、切换/快照按目标应用独立、账号池 app 无关同池调度；桥档案表可扩展更多应用；Qoder 支持 PAT / OAuth 设备流 / IDE 登录态扫描三通道入池与每日双活动签到）。API 网关为 **trae / buddy / qoder / custom 四池调度**，每池独立开关。**所有数据仅存在本地数据目录（Windows `%APPDATA%\AIWorkAssistant\`，macOS `~/Library/Application Support/AIWorkAssistant/`），零外部网络**。

> **macOS 支持（F-75）**：平台服务层 `platform/` 分层归位（子进程构建 / 数据目录 / vault Keychain / 系统代理 networksetup / CA security / 进程 SIGTERM / bundle 定位），打包 dmg（aarch64 + x64 双架构，`tauri.macos.conf.json`）。应用域灰度放开中（`mac_supported`，待各应用 mac 版数据布局侦察），设计与进度见 `docs/tmp/f75-macos-support-design.md`。schtasks 系统级定时、MachineGuid 系统级重置、UI 点击兜底为 Windows 专属（mac 由内置调度器 + 开机自启覆盖，入口按 platform 标志隐藏）。

## 2. Quick Start

```powershell
# 仅 Windows，需要 Node 18+ / Rust stable (MSVC) / VS Build Tools C++ 工作负载 / WebView2（Python 已移除，scripts 全 .mjs 零依赖）
cd ai-work-assistant   # 本机目录暂为 trae-work-assistant，见文首说明
npm install
npm run tauri dev          # 开发模式（Tauri WebView 加载 Vite 5173）
npm run tauri build        # 打包 MSI + NSIS 到 src-tauri/target/release/bundle/
node scripts/rename_release.mjs     # 产物统一输出到 release/，中文命名 AI Work 助手_<版本>_x64*
```

测试：

```powershell
cargo test                                    # Rust 单测（需先装工具链；切换器/签到/网关全覆盖）
```

## 3. 技术栈

| 层 | 技术 |
|---|---|
| 外壳 | Tauri 2.x (Rust 1.75+ MSVC) |
| 前端 | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 + lucide-react |
| 后端 | Rust (serde / chrono / axum / ureq / tauri-plugin-{shell,dialog,notification,single-instance}) |
| 辅助 | Node.js 18+（scripts/*.mjs 发布工具链，零 npm 依赖）；PowerShell 运行时依赖已移除（switcher 模块进程内直调） |

## 4. 目录地图

```
ai-work-assistant/
├── AGENT.md                      # 本文件（项目速查）
├── README.md                     # 用户文档
├── package.json / vite.config.ts / tsconfig.json / tailwind.config.js / postcss.config.js / index.html
├── docs/                         # user-manual / tech-framework / product-design / backlog（唯一待办；原五份分析文档 2026-09-13 归并删除，原文在 git 历史）
├── src/                          # 前端
│   ├── App.tsx                   # 外壳（TitleBar + Sidebar + TopBar + 页面切换 + Toaster）
│   ├── store.ts                  # Zustand 单一真相（init / 刷新 / checkin/switch/saveLogin 事件归约）
│   ├── types.ts                  # 与 Rust DTO 对齐（snake_case）
│   ├── lib/                      # tauri.ts(invoke 封装+事件订阅) / themes.ts(主题) / delay.ts(withMinDelay) / cn.ts / about.ts / useIsDark.ts
│   ├── components/               # TitleBar/Sidebar/TopBar/Toaster/PageHeader/SetupGuide/ui + SystemDialog(系统设置+系统日志弹框)/GeneralSettingsPanel/AboutDialog
│   └── pages/                    # Dashboard / Accounts(661行编排 + accounts/ 16 个拆分子组件) / Checkin / Logs / ApiService / Settings
│   │                             #   dashboard/ 积分看板（platform 参数化：credits 视图=Trae 页、buddy-credits 视图=Buddy 页、qoder-credits 视图=Qoder 页替换旧 QoderCredits；KpiRow/CreditsTab/TokensTab/ExpiryTab/adapters）
│   │                             #   buddy/（BuddyOverview/BuddyAccounts/BuddyCheckin/BuddyApiService/BuddySettings）
├── scripts/                      # dev-tauri.mjs(tauri 脚本入口) / sync_version.mjs / rename_release.mjs / package_portable.mjs / gen_asset_base64.mjs
├── src-tauri/
│   ├── tauri.conf.json           # 无装饰窗 / 无外部资源（Python 与 PS 桥均已移除，全 Rust）
│   └── src/
│       ├── main.rs               # 注册全部命令 + --task-run CLI 任务模式
│       ├── platform/             # F-75 平台服务层（M0 底座）：mod.rs 数据根/OS 标志 + cmd.rs sys_command 子进程构建（全仓 creation_flags 收敛点，禁止再散写 CommandExt）+ secret.rs vault 主密码原语（win DPAPI / mac Keychain via keyring）
│       ├── state.rs              # AppState（%APPDATA%\AIWorkAssistant + 旧目录迁移）
│       ├── models.rs             # DTO（含 CheckinSummary.time 字段）
│       ├── store/                # SQLite 存储层（v3.4.5 起全量承载 data 目录状态，替代 JSON 文件读写）
│       │   ├── mod.rs / schema.rs / migrate.rs  # 连接注册表(WAL) / 建表 / 启动迁移(旧 JSON 导 backup/)
│       │   └── *.rs              # kv 键值文档表 + 行文档实体表 + 列化流水表类型化读写
│       ├── device_proxy/         # MITM 代理模块（hyper+rustls 自建，原 Python 迁移）：CA/头改写/凭据捕获/WS 帧记录/SSE 摘要/上游透传
│       ├── fs_utils.rs           # 原子 read_json / write_json（vault/日志/导出仍用）/ mask / 时间辅助
│       ├── vault.rs              # Stronghold 凭证保险库（DPAPI 主密码；JSON 落盘占位化）
│       ├── workbuddy_cli.rs      # CLI 切号桥决策（decide_target 纯函数 + 轮换线程）
│       ├── jwt.rs                # parse() + status_of() + refresh() + oauth_parse()
│       ├── api_server/           # API 网关模块
│       │   ├── mod.rs            # 常量 + 路由注册
│       │   ├── server.rs         # axum 服务器启停
│       │   ├── routes.rs         # OpenAI /v1/chat/completions + Anthropic /v1/messages + Codex /v1/responses（SSE 流式 + 非流式，三协议输出）
│       │   ├── pool.rs           # 账号池调度（积分感知 + 冷却状态机 + 账号轮换，app 无关）
│       │   ├── payload.rs        # OpenAI/Anthropic 请求 → llm_utils_chat 改写（anthropic_to_openai 先转内部格式）
│       │   ├── sse.rs            # SSE 协议转换（SOLO → OpenAI chunk / Anthropic 事件流）
│       │   ├── auth.rs           # API Key 鉴权（Bearer + x-api-key 双风格）
│       │   ├── models_sync.rs    # 模型列表配置化（api_models.json）+ 官网 batch_get_detail_param 同步
│       │   └── api_logger.rs     # API 请求日志
│       └── commands/             # env / cert / proxy / accounts / checkin / switch / misc / profile / api_server / oauth / trae_apps(双应用发现) / process(三级关闭) / updater / wb_config(wb 手工配置读写)；workbuddy/ 为目录模块（common/accounts/checkin/credits/cli/chatdata/oauth/env_reset，mod.rs pub use 保持命令路径不变）
│       └── tasks/                # 后台业务直调模块（trae_checkin / wb_checkin / wb_common / wb_credits / doubao_session / doubao_quota / doubao_chats / ui_click）
│           ├── trae_checkin.rs   # 批量签到（vault 解密内存传递，无子进程）
│           ├── wb_checkin.rs     # WorkBuddy 签到/成长/续期（run_checkin_round / run_growth_round / run_renew_only）
│           └── doubao_*.rs       # 豆包会话续期 / 额度巡检 / 对话导出
│       └── switcher/            # 登录态切换器（原 PS 桥 trae-switch-bridge.ps1 全量 Rust 化）
│           ├── mod.rs           # run_action 入口 + 7 Action 编排 + ProgressSink（TauriSink/CliSink）
│           ├── profile.rs       # 5 应用 × 3 快照布局档案表（icube/chromium/authfile）
│           ├── locate.rs        # exe 六级发现（settings→候选→lnk→注册表→进程→缓存）
│           ├── proc.rs          # 三级关闭（WM_CLOSE→强杀→等待）+ 启动（可选 --proxy-server）
│           ├── machine.rs       # 6 层设备标识重置 + MachineGuid
│           ├── copy.rs          # 快照复制原语 + .bak 单代轮转
│           ├── vscdb.rs         # F-68 state.vscdb 全局键合并（项目列表/最近打开跨账号保留）
│           └── icube / chromium / authfile.rs  # 三布局快照管线
```

## 5. Tauri 命令契约

> **调用约定**：invoke 的**顶层参数名**跟随 Rust 函数签名（驼峰不替换，参数名直接匹配）。**嵌套对象**（`opts` / `patch`）的字段名保持 **snake_case**（Tauri 默认 serde 字段名，不做 camelCase 转换）。
> **存储说明（v3.4.5 起）**：data 目录 JSON 已**全量迁入 SQLite**（`data/aiwork.sqlite`，kv 键值文档表 + 行文档实体表 + 列化流水表，旧 JSON 首启迁入 `data/backup/`）。下文提及的 `data/*.json` 均为**逻辑名**（对应库中 kv 键/表），不再产生运行期 JSON 文件读写。

| 模块 | 命令 | 说明 |
|---|---|---|
| 环境 | `env_check` → `EnvStatus` | `installed/running/version/path`；async 命令（注册表全量搜索较慢，避免 UI 卡顿）。**提速修订（2026-10-07）**：版本号改 `pe_version` 直读 PE 版本资源（原 powershell 子进程 1.2s+/次 → 1.9ms，按 mtime 缓存命中 0.14ms），运行态改 `switcher::proc::any_running` 进程表枚举 + 2s TTL（原 tasklist 261ms/次 → 18ms），`app_locate` 注册表/进程兜底两级加 3s TTL——同一轮界面切换里概览页与顶栏重复探测不再重复掏子进程 |
| 环境 | `open_trae_website()` / `open_trae_app()` | 打开 Trae 官网 / 启动 Trae Work（代理注入时优雅关闭进程最长 5s，async） |
| 环境 | `env_check_trae_cn()` / `open_trae_cn_app()` | Trae CN IDE 环境检测 / 启动（双应用支持） |
| 证书 | `cert_status` / `cert_install` | 安装走 UAC `certutil -addstore -f Root` |
| 代理 | `proxy_start(port)` / `proxy_stop()` / `proxy_status()` | ProxyStatus：`running/port/captured/started_at` |
| 账号 | `accounts_list` → `AccountView[]` | 聚合 JWT / 分组 / 设备 / 积分 / 今日 |
| 账号 | `account_add_manual(name, jwt, groupId?)` | 解析 JWT → userId 入库 |
| 账号 | `account_delete(userId, deleteProfile)` | 同时清分组；`deleteProfile=true` 删 profiles/<uid> |
| 账号 | `account_update(...)` / `accounts_export_raw()` / `accounts_import(...)` | 编辑账号 / 原始 JSON 导出 / 导入 |
| 账号 | `account_get_jwt(userId)` | 按需获取完整 JWT——列表接口 `AccountView.jwt` 已掩码下发（前4+****+后4），完整值仅供「查看/复制 JWT」弹窗按需拉取 |
| 积分 | `fetch_remaining_credits` / `fetch_credit_detail` / `refresh_remaining_credits` | 剩余积分查询 / 明细 / 刷新 |
| 积分 | `credits_daily_list` / `credits_history` / `invite_link()` | 每日快照 / 历史记录 / 邀请链接 |
| 冷却 | `cooldown_clear(userId?)` / `cooldown_clear_all()` | 清除签到错误冷却状态 |
| 账号 | `account_oauth_add` → 实际为 `oauth_login(callback_url, account_name?, group_id?)` | 从 OAuth 回调 URL 解析 token + userInfo |
| OAuth | `oauth_get_login_url()` → `{ url }` | 构造 Trae 登录 URL |
| OAuth | `oauth_parse_callback(callback_url)` → `{ user_id, ... }` | 解析回调 URL 中的 token |
| 分组 | `groups_list` / `group_create` / `group_update` / `group_delete` / `group_move` | 删除分组时账号回落「未分组」 |
| 签到 | `checkin_start(opts)` → NDJSON 事件 | `opts: { scope, user_ids?, skip_checked_in, skip_expired }`；失败自动重试最多 2 轮（30s/90s，T5） |
| 签到 | `checkin_trends(days?)` → `CheckinTrendPoint[]` | 近 N 天签到结果按日汇总（T8，checkin_results 表，保留 90 天）；写入方 = `trae_checkin::run_round` 末尾统一落库（UI 手动/托盘/静默/应用内调度器/CLI `--task-run checkin` 全路径覆盖；UI 路径重试轮合并后由 `run_checkin_worker` 再落一次，同 uid 同日以最后一次为准，双写幂等） |
| 环境 | `app_locate(targetApp)` → `AppLocate` | 四应用安装位置四级探测（手动指定→注册表→默认路径→进程反查，F-01）；`targetApp: trae_work\|trae\|doubao\|workbuddy` |
| 环境 | `open_doubao_app()` | 启动豆包桌面版（复用 app_locate 豆包档案探测） |
| 切换 | `switch_account(userId)` | 调 `switcher::run_action(Switch)`（进程内直调，三级关闭策略）；`target_app` 支持 TraeWork/Trae/Doubao/WorkBuddy/CodeBuddy |
| 切换 | `reset_device_ids(userId)` | switch 模块：重置设备指纹（区别于 misc 的 `device_reset` 只删映射）；仅 icube 布局 |
| 保存 | `save_current_login(userId)` | 调 `switcher::run_action(SaveCurrentLogin)`；`target_app` 支持 TraeWork/Trae/Doubao/WorkBuddy/CodeBuddy |
| 快照 | `profile_list(targetApp?, fresh?)` → `ProfileInfo[]` | 列出快照槽；`target_app` 决定根目录 profiles / profiles_trae / profiles_doubao / profiles_codebuddy。**性能（2026-10-07）**：体积统计（`dir_stats` 递归整棵快照目录）改「缓存 + 多槽并行 + `spawn_blocking`」——CodeBuddy 档实测 18,372 目录 / 13,659 文件 / 300MB，单线程 4.89s、4 线程 1.64s（耗时几乎全在逐目录 `read_dir`：只遍历不取 metadata 同样 4.87s），原实现是同步命令逐槽串行 → 打开快照管理卡 UI 数秒。缓存键 = 槽位目录绝对路径，`mtime` 仅作尽力而为失效信号（NTFS 只在**直接子项**增删时更新槽位目录时间戳，且更新有延迟），正确性由 TTL 300s + 显式失效（`switcher::backup_current` 出口统一 `invalidate_profile_stats`——保存当前登录态/切换/备份/恢复回退 last 槽全部写槽路径一次收口，删除后同样失效）保证；`fresh=true` 绕过缓存（前端「刷新列表」按钮）。埋点 `[profiles] 快照体积统计 …` 仅在实际计算时落一行 |
| 快照 | `profile_backup(userId)` / `profile_restore(userId)` / `profile_delete(slot)` | 手动备份/恢复/删除；`target_app` 同上 |
| 快照 | `profile_format_size(...)` | 快照体积格式化 |
| 豆包 | `doubao_accounts_list` → `DoubaoAccountView[]` | 账号池 ∪ profiles_doubao 快照槽合并视图 + 当前账号标记 + 会话状态（last 槽与 `*.bak` 单代回滚槽不展示） |
| 豆包 | `doubao_account_save(userId, name?, note?)` / `doubao_account_remove(userId)` | 豆包账号池 upsert / 移除（data/doubao_accounts.json） |
| 豆包 | `doubao_account_set_credential(userId, sessionId?, sidGuard?, ttwid?)` | 编辑弹框保存会话凭证（已存值回填；清空保存即删除；ttwid 仅非空时更新；账号不在池时自动入池） |
| 豆包 | `doubao_account_get_credential(userId)` | 编辑弹框按需回填完整会话凭证——列表接口 `DoubaoAccountView` 的 session_id/sid_guard/ttwid 已掩码下发，完整值仅此命令按需获取 |
| 豆包 | `doubao_captured_credential()` / `doubao_credential_auto_apply()` | 读 `device_proxy/` 抓包落盘的 data/doubao_captured_credentials.json（doubao.com Cookie 中的 sessionid/sid_guard/ttwid）；auto_apply 目标 = **抓包文件自带 uid**（multi_sids 按同一条 sessionid 匹配的主人，凭证与归属同源自洽），且**只回写已入池账号、绝不自动建号**（网页版/其他字节系应用抓到的陌生会话跳过并记 app_log；新账号一律走「保存当前登录态」），前端账号页每 20s 轮询；另有 `doubao_captured_credential` 供编辑弹框手动填充 |
| 豆包 | `doubao_detect_uid()` | **主来源**：Local Storage leveldb 的 `client_device_info.userId`（客户端每次启动自写、**不依赖代理**；Rust `tasks/doubao_chats.rs` 解析并按时间戳与抓包文件比新鲜度取新者——无代理重登新账号也能识别，实测 2026-09-09）；**兜底①**：抓包文件 uid（multi_sids）→ `%LOCALAPPDATA%\Doubao\User Data\Local State` 的 saman.user_id（**同 profile 重登不更新**，只作兜底）；**兜底②**：`%APPDATA%\Doubao\public_config.json` 全树递归搜（text_picker 是输入法选择器缓存，**不随登录切换更新**，勿当主来源——bug1 根因）；**兜底③**：profiles_doubao/current_account.txt；含单元测试。局限：客户端**会话内**换登录不重启时 client_device_info 不刷新，重启豆包后即正确 |
| 豆包 | `doubao_keepalive_run()` | 续期主路径：调 `switcher::run_action(KeepAlive)`（启动豆包 8s 联网滑动续期 → 优雅关闭，运行中跳过），NDJSON → keepalive-progress/done 事件，成功后记池级 last_keepalive_at + 运维历史 |
| 豆包 | `doubao_renew_run(syncOnly?)` | Rust 直调 `tasks/doubao_session.rs`（原 python doubao_renew.py）：探活巡检（仅手动录入凭证的账号，200=有效/302→passport=过期）或 cookie 诊断（sync_only，实测客户端 cookie 为二次加密密文，不能当凭证）；结果记运维历史 |
| 豆包 | `doubao_renew_task_register(time)` / `..._status()` / `..._unregister()` | schtasks 每日保活任务 AIWorkAssistant_DoubaoRenew（/TR 调主 exe `--task-run doubao-keepalive`，启动器 `task_doubao_renew.cmd` 启动期原地迁移） |
| 豆包 | `doubao_quota_fetch(userId)` | Rust 直调 `tasks/doubao_quota.rs` 查会员额度：POST 默认接口 `/alice/commerce/sale/subscription/quota/summary/`（body `{"product_line":"membership"}`，settings.doubao_quota_url 可改）+ 账号凭证；精确解析（套餐/到期/活动赠送/订阅记录/当前时段+近7天窗口含重置时间）+ 宽容兜底，成功后回写账号池额度缓存与运维历史；保活端点默认 `/info/v2/`（settings.doubao_renew_url 可改；state.rs 启动迁移回填默认值） |
| 豆包 | `doubao_quota_task_register(time)` / `..._status()` / `..._unregister()` | schtasks 每日额度巡检任务 AIWorkAssistant_DoubaoQuotaCheck（/TR 调主 exe `--task-run doubao-quota`：批量查池内有凭证账号 → 回写缓存 + 运维历史 + 用完记录） |
| 豆包 | `doubao_history()` | 读 data/doubao_health_history.json 运维事件（keepalive/renew/quota，滚动 400 条；quota 事件含 windows 额度窗口），概述页额度趋势图与健康度卡数据源；写入方：keepalive_run/renew_run/quota_fetch（source=app）+ 定时任务（source=task） |
| 豆包 | `open_doubao_app(proxyPort?)` | 打开豆包桌面版；proxy_port 存在时注入 `--proxy-server`（启动前三级关闭现有进程确保参数生效，对齐 Trae 打开逻辑），凭证/额度抓取不依赖系统代理 |
| 豆包 | `doubao_open_as_account(userId, proxyPort?)` | **C1 一键以账号打开**：调 `switcher::run_action(Switch, Doubao)`（恢复该账号快照后直接拉起客户端，把「切换 → 等待 → 打开」两步合并为一步）；proxyPort>0 时注入 `--proxy-server`；NDJSON 进度复用 switch-progress / switch-done 事件管线（前端走 store.openDoubaoAs，与 switchTo 互斥共用 switchingTo 状态） |
| 豆包 | `doubao_snapshot_meta(userId)` → `DoubaoSnapshotMeta?` | **C3 快照版本校验**：读 `profiles_doubao/<uid>/snapshot_meta.json`（schemaVersion / createdAt / chromiumVersion / includeIndexedDB），无元数据文件时回退读快照内 `Last Version`（返回 schema_version=0 标记为旧版快照）；账号页快照列「已保存」处悬停展示版本信息 |
| 豆包 | `settings.doubao_snapshot_include_idb` | **C4 IndexedDB 可选纳入快照**：默认 false（体积大，默认排除）；开启后 profile_backup / profile_restore / switch_account / save_current_login 透传 `include_indexeddb` 给 switcher；备份时纳入 `Default/IndexedDB`，恢复时**只要快照内含就回写**（不看当前开关，保证快照完整回写） |
| 豆包 | `doubao_chatdata_backup(userId)` / `doubao_chatdata_restore(userId)` / `doubao_chatdata_info(userId)` | **D1 对话数据独立备份/恢复**：源=豆包 User Data 各 Profile 下 IndexedDB（chrome_doubao-* / https_www.doubao.com*）+ DoubaoStorage → `data/doubao_chats/<uid>/`（按 profile 名分层 + chat_backup_meta.json，覆盖式）；备份/恢复均先 graceful_kill_app("Doubao")；恢复按 profile 名回写；与快照解耦（对话正文在云端跟账号走，本地备份的是客户端状态，换机/重装后恢复备份+登录即可同步对话）；info 供账号行「对话已备份」徽标 |
| 豆包 | `doubao_export_chats(userId)` | **D2 对话记录导出**：Rust 直调 `tasks/doubao_chats.rs`（原 python `--export --uid X` 语义，stdout 末行 JSON 契约不变）；走官方 IM 接口 `POST www.doubao.com/im/chain/recent_conv`（cmd 3200 会话列表，conv_version=0 首跳 limit≤50）+ `im/chain/single`（cmd 3100 单会话消息，anchor_index=2^53-1 起翻页，index_in_conv 为字符串需 int 转换）；必需 Cookie：sessionid/sessionid_ss/sid_tt + sid_guard + **ttwid**（登录校验），sid_guard/ttwid 必须以 Set-Cookie 下发的 **URL 编码原样**发送（原始 | 形式报 712010702，_cookie_enc 兼容两种存储形式），query 必须含设备指纹 web_id/tea_uuid/fp（缺失报 712010702）+ 头 agw-js-conv:str + UA SamanthaDoubao；输出 markdown+json 到 data/exports/doubao_chats_<uid>_<ts>.*；正文提取 content_block text_block → tts_content/brief 兜底；池内凭证过期（客户端重新登录后 sessionid 轮换）时需重开代理自动回写 |
| 日志 | `proxy_logs_list(...)` / `proxy_log_detail(...)` | 代理请求日志列表 / 详情。**性能（2026-10-07 实测）**：改为「条目索引 + 按需读字节」——`proxy_req_*.log` 目录实测 11 文件 / 230MB / 35.9k 条，原实现每次把**全部文件**读成 String 并逐条 parse（最后才 `skip/take` 取当前页 30 条），翻页 / 改日期 / 改关键字都重来一遍；现每文件维护 mtime+size 增量的条目索引（字节区间 + 首行时间戳，定长数组无堆分配），时间筛选走内存、仅为当前页那 30 条读字节区间，11 个文件分 4 路并行：列表首屏（冷建索引）285ms / 热命中 12ms / 深翻页 9ms / 关键字 569ms（旧等价路径约 850ms+ 且每页重算）；详情由「读完整文件（单文件最大 60MB）」改为只读该条区间。两命令均改 `#[tauri::command(async)]`（不占 UI 线程）。**id 口径修正**：id = 文件内**原始**非空分块序号，与 detail 取序号方式一致（原实现 index 只在通过筛选时自增，带筛选时列表 id 与详情错位、点开是另一条） |
| 文件 | `read_text_file(path)` / `write_text_file(...)` | 前端通用文本读写（read 有 10MB 上限 + 常规文件校验） |
| 双应用 | `apps_accounts_discover` / `apps_account_add` / `apps_entitlement_read` | 本机 Trae Work + Trae CN 账号自动发现 / 入池 / 套餐读取（F-08，见 §5.1）；`apps_account_add` 增 `uidConfident?` 参数（false 时拒绝入池，杜绝 dc uid 误入池，命令层把关） |
| 双应用 | `refresh_pay_status(...)` / `accounts_backfill_dc_ids(...)` | 会员支付状态刷新 / 已有账号回填 dc id |
| 设备 | `device_reset(userId)` | 删 `device_map.json[ uid ]` |
| JWT | `jwt_parse(jwt)` / `refresh_jwt(userId)` | 解析 / 自动刷新（需 refresh_token） |
| API | `api_server_start()` / `api_server_stop()` / `api_server_status()` | API 网关启停（端口/默认模型由设置页提供；鉴权统一走 API Keys 列表） |
| API | `pool_list` / `pool_set` / `pool_status` | 账号池管理；`pool_set` 扩展 `strategy`（expire_first/credit_first/random/**weighted/p2c**）/ `group_ids` / `wb_enabled`（T2.1 WB 上游开关）/ `wb_default_thinking`（T5.3）/ `wb_tool_exec`（T5.5，默认开）/ `wb_bg_downgrade`（T5.6③）/ `qoder_enabled`（p3-3 Qoder 上游开关，默认关，Qoder 环境配置页·网关上游）/ `qoder_hedge_threshold_ms`（F-80-余 v2 Qoder 对冲阈值，默认 8000，0=关）/ `qoder_sticky_enabled`（F-80-余 v2 Qoder 会话粘性，默认关）/ `trae_enabled`（Trae 池开关，默认开，Trae 资源调度页）；**per-pool 调度参数八参（三池不共享，None=保留原值；旧共用参数 `account_concurrency_limit` / `pool_sticky_ttl_secs` 已退役——存量迁移仅 Buddy 池沿用旧共享值：读取侧检测 Buddy 新字段缺失且旧字段存在时回填（`load_pool_file_with_legacy_migration` 纯函数幂等，pool_set 保存后落盘固化），Trae/Qoder 池直接落默认值 1/300/1800，serde 忽略旧 JSON 键）**：`trae_account_concurrency_limit` + `trae_pool_sticky_ttl_secs` + `trae_sticky_ttl_secs` + `trae_hedge_threshold_ms`（Trae 池参数含竞速对冲阈值默认 8000/0=关，Trae 资源调度页）/ `wb_account_concurrency_limit` + `wb_pool_sticky_ttl_secs`（+ 既有 `wb_sticky_ttl_secs`，Buddy 资源调度页）/ `qoder_account_concurrency_limit` + `qoder_pool_sticky_ttl_secs` + `qoder_sticky_ttl_secs`（Qoder 资源调度页）——保存即热应用（三池 set_concurrency_limit / 三份池粘性 TTL 原子量 / 三 sticky store 各自 set_explicit_ttl）；未传字段保留原值；三池三页各自管理自己的开关与参数 |
| API | `api_debug_toggle` / `api_debug_status` | API 请求日志开关 |
| API | `api_models_list()` / `api_models_sync()` | 模型列表读取（data/api_models.json）/ 官网同步（不消耗积分，最多试 3 账号） |
| API | `api_logs_list(...)` / `api_logs_detail(...)` / `api_logs_search(...)` | API 请求日志查询 / 详情 / 搜索 |
| API | `api_usage_stats(days?)` → `UsageDayView[]` | 网关用量按日统计（T1，data/api_usage.json，保留 90 天，直读落盘） |
| API | `api_keys_list()` / `api_keys_save(keys)` | 多 API Key 列表管理（T2，data/api_keys.json，每日配额；主 Key 双轨已移除） |
| API | `api_unified_models(available_only?)` | 统一模型目录（v3.3.x）：Trae 官网同步 + WB 目录 + 自定义模型三源合并（canonical_id trim+lowercase 归并），元数据四层兜底（L1 人工覆盖 trae_model_meta.json → L2 官网 → L3 默认 → L4 系列推断）；与 `GET /v1/models` 共用视图 |
| API | `dispatch_policy_get()` / `dispatch_policy_set(policy)` | 池间调度策略（data/dispatch_policy.json）：`strategy`（smart=到期优先→倍率→健康积分和 / priority=严格按序）、`priority`（trae/buddy/qoder 数组，缺省 ["buddy","trae","qoder"]）、`per_model` 模型级覆盖（显式覆盖不做智能重排）、`fallback` 跨池回退开关；带 mtime 兜底解析缓存 |
| API | `custom_models_list()` / `custom_models_save(model)` / `custom_model_test(model)` | 自定义 OpenAI 兼容上游（data/custom_models.json，v3.3.x）：列表 / upsert（name/base_url 必填、canonical 不重复、id `cm-<12hex>` 自动生成）/ 连通性测试（与保存同口径预检）；模型命中即直达 custom 池，不参与 dispatch 池间策略 |
| API | `api_wb_usage_stats(days?)` / `api_custom_usage_stats(days?)` / `api_qoder_usage_stats(days?)` | WB 池 / 自定义池 / Qoder 池用量按日统计（四桶分库查询，days 默认 14 clamp 1~90；Qoder 桶上游接入后产生数据） |
| 日志 | `logs_query({ opts: { log_type, date, keyword, limit } })` → `LogLine[]` | `split_time` 会 strip BOM 前缀 |
| 日志 | `logs_clear(log_type)` → `u32` | 按类型删除日志文件（all/proxy/checkin/switch，T6，幂等） |
| 设置 | `settings_get()` / `settings_set(patch: Settings)` | Settings 全部 snake_case |
| 设置 | `autostart_status()` / `autostart_set(enabled)` | 开机自启查询 / 开关（T11，即时生效）；配套 `settings.silent_checkin` 启动静默签到 |
| 计划 | `task_register(time)` / `task_status()` / `task_unregister()` | `schtasks` 注册每日签到 |
| 更新 | `update_check()` / `update_download(...)` → `UpdateDownloaded` / `update_run_installer({file_path, asset_name})` | 两步确认制：下载（确认一）→ 安装（确认二）。安装器参数 `/P /UPDATE /R`：被动进度条 + 跳过卸载直接覆盖 + 完成后自动重启应用；`run_installer` 校验路径必须位于临时更新目录 |
| WorkBuddy | `workbuddy_env_check()` → `WorkBuddyEnvCheck` | 客户端安装/运行/版本 + auth 文件存在性 + `~/.workbuddy` 快照解析（uid/昵称/editionType）；复用 app_locate 四级探测 |
| WorkBuddy | `workbuddy_accounts_list` → `WorkBuddyAccountView[]` | 账号池（data/workbuddy_accounts.json）+ 在线标记（auth 文件 uid 匹配）+ 快照/凭证副本存在性；id = `wb-<sha256(token)前12位>`（同 token 稳定同 id） |
| WorkBuddy | `workbuddy_account_save/remove` / `workbuddy_scan_auth_file` / `workbuddy_account_import_auth` | 别名备注 / 删除 / auth 文件扫描预览 / 确认入池（凭证入 token store 副本，零明文出 Rust） |
| WorkBuddy | `workbuddy_refresh_token(userId)` | plugin refresh 端点（X-Refresh-Token 仅限此端点）+ 回写 token store 与账号池过期时间；失败提示需重登 |
| WorkBuddy | `workbuddy_checkin_start(opts)` → NDJSON `wb-checkin-progress` | Rust 直调 `tasks/wb_checkin.rs::run_checkin_round`（状态查询回退旧路径 / code:10001 已签容错 / 401 刷新一次重试 / 零 token 输出）；opts: `{ user_ids?, skip_checked_in, skip_expired, lazy_hours? }` |
| WorkBuddy | `workbuddy_growth_run()` | 成长中心执行入口（旅行/盲盒/任务开关从 workbuddy_settings.json 读取；`wb_checkin::run_growth_round` 链式执行：travel status→claim→config→depart / lottery chances→draw 循环（上限 20）/ tasks→accept，各步独立容错、401 刷新一次重试、奖励数额以接口返回为准） |
| WorkBuddy | `workbuddy_checkin_results(days?)` | 签到日志（wb_checkin_results 表 90 天滚动，默认展示 30 天；**纯追加**：同日同账号多轮并存，返回新→旧序；活动档期日历聚合 `daysFromWbRecords` 按账号取当日最终态——任一 success/already 即当日已领、仅全失败计 fail） |
| WorkBuddy | `workbuddy_checkin_task_register(times[]) / _status / _unregister` | schtasks 每日双时段签到任务 AIWorkAssistant_WorkBuddyCheckin_<HHMM>（09:00/21:00） |
| WorkBuddy | `workbuddy_renew_task_register(day) / _status / _unregister` | schtasks 每周凭证续期兜底任务 AIWorkAssistant_WorkBuddyRenew（周日 10:30，主 exe `--task-run wb-renew` → `run_renew_only` 惰性刷新） |
| WorkBuddy | `workbuddy_credits_fetch(userId?, fresh?)` | Rust 直调 `tasks/wb_credits.rs`：积分三件套 + 旧接口回退 + 容量字段链解析 + ≥10min 缓存；成功回写账号池余额缓存；非缓存命中时追加每日快照（含 earned = 当日余额差分与签到 reward 归并，credits-dashboard-plan.md §2.2 方案 B）。**提速修订（2026-10-08）**：账号间并发取数（≤4 路；原串行 for 让「N 账号 × ≥3 次请求」在全局代理/跨境出口下线性放大，实测单请求 0.1s→2s+ 直接反映为切板块等待）+ 缓存过期时 **stale-while-revalidate**：非 `fresh` 调用先回旧值（`cached:true, refreshing:true`）并把刷新转后台线程（进程内在途去重），前端 `BuddyOverview` 按 `refreshing` 最多重取 4 次拿新值；`fresh=true`（手动刷新）与「无缓存」仍同步拉 |
| WorkBuddy | `workbuddy_credits_history_list()` | WB 每日积分快照时序读取（wb_credits_history 表，365 天，含 earned；看板「Buddy 获得积分」方案 B 数据源） |
| WorkBuddy | `workbuddy_settings_get / workbuddy_settings_set(patch)` | data/workbuddy_settings.json：auto_checkin（启动补签）/ keepalive_days / lazy_refresh_hours / growth_* 开关 |
| WorkBuddy | `workbuddy_cli_status / _bridge_set(userId) / _rotate_run / _rotate_logs(limit?)` | CLI 切号桥（F-06/F-59，批次3）：桥接状态（含 environment_override 警告）/ 写 `~/.codebuddy/settings.json` env 直桥 / 手动触发五重防护轮换 / 轮换日志（cap 50）；后台轮换线程 `start_cli_rotate_thread()` 按 settings.cli_* 配置独立运行（`workbuddy_cli.rs` 决策纯函数 `decide_target` 13 单测） |
| WorkBuddy | `workbuddy_chatdata_backup/restore/info(userId, app?)` / `_copy(source,target,app?)` | 会话三件套（F-44/F-45，批次3）：正文 projects/ + workbuddy.db + edge-sync-mapping-v2.db → `data/<app>_chats/<uid>/`（restore 前 .bak 单代保护 + 完整性校验回滚）；copy = jsonl 逐行 sessionId 换新 UUID（pseudo_uuid_v4 纯函数）+ sessions 整行克隆 + edge 映射 convmsg 替换（`.pre-copy.bak` 预备份）。**F-74**：`app` = `"WorkBuddy"`（默认）/ `"CodeBuddy"`——数据目录 `~/.workbuddy` / `~/.codebuddy` 与备份根 `data/workbuddy_chats` / `data/codebuddy_chats` 双域隔离；空/未知回落 WorkBuddy（旧行为） |
| WorkBuddy | `workbuddy_accounts_export(includeCredentials?) / _import(payload)` | 账号库导入导出（F-46 扩展，批次3）：kind 标记 `aiwork-workbuddy-pool`、按 id 去重、含凭证导出时回写 token store；凭证是否随行由用户勾选 |
| WorkBuddy | `workbuddy_oauth_login()` | OAuth 扫码登录（F-50，批次3）：`POST /v2/plugin/auth/state?platform=CLI` → 系统浏览器打开 authUrl → 轮询 `GET /v2/plugin/auth/token?state=`（≤300s/3s）→ `GET /v2/plugin/login/account?state=` 取 uid/nickname → 自动入池 + 凭证回写 token store；每流程独立 cookie jar（Set-Cookie 手工捕获，零新依赖）；事件 wb-oauth-progress / wb-oauth-done |
| WorkBuddy | `workbuddy_env_reset_items()` / `_env_reset(items, keycloakLogout)` | 环境重置（F-14，批次3）：16 项认证残留清理清单（对齐 oss-research 17 物理位置，勾选预览 + 存在性标注）+ Keycloak SSO 注销（JWT iss → 浏览器 logout，先于清理执行）；执行前自动关闭 WorkBuddy，单项失败不中断 |
| WorkBuddy | `workbuddy_usage_official(userId?, fresh?)` | 官方请求用量（F-25，批次3）：`POST <domain>/billing/meter/get-user-request-usage` 近 31 天分页（pageSize 3000，≤20 页，requestId 去重）→ 今日/近7天/本月 + 逐日按模型聚合；缓存 10min（data/workbuddy_usage_official_cache.json）；上游 prompt/input 字段一律不复制（脱敏红线） |
| WorkBuddy | `workbuddy_token_stats()` | 本地 Token 统计（F-26/F-57，批次3 + 2026-10-06 扩展）：合并三路来源——① `~/.workbuddy/projects`/`~/.codebuddy/projects` 会话 JSONL（跳过 subagents/；usage 取值 message.usage > providerData.usage > 顶层，cache_read 别名链优先正值）；② **CodeBuddy IDE 会话索引** `%LOCALAPPDATA%\CodeBuddyExtension\Data\<uid>\<产品>\<uid>\history\<md5(工作区)>\<会话id>\index.json`（`<产品>` 实测有 `CodeBuddyIDE` / `VSCode` 两套同构布局，均收） 的 `requests[]`（`usage{inputTokens,outputTokens,cacheTokens(缓存读),cachedWriteTokens,...}`，`state=running` 与四类全 0 跳过、同会话按 request id 去重、日期取 `startedAt`、模型取工作区索引 `conversations[].modelMap[type]`（回退 modelMap.craft/selectedModelId）、项目维度记常量 `CodeBuddy IDE`；`usage.credit` 暂不并入积分以免与官方源重复计数）。三路共用 mtime+size 增量缓存，固定 365 天窗口；输出 summary/models/projects/daily/daily_by_model（snake_case，`commands/workbuddy_stats.rs` 9 单测）。**数据源定位经 WorkDaddy `scripts/codebuddy-files.js` tokenOptions() 独立互证**（其 readRecords 同样读 `index.requests`，source=`local-codebuddy-requests`）；此前「IDE 侧无本地用量」的判断是只看 messages/*.json（那里确实只有正文）导致的漏检。**性能（2026-10-07 实测优化，扫描提速）**：① 第三路改为按结构定向下探 `Data\*\*\*\history\*\*\index.json`（产品子树含 `CodeBuddyIDE` / `VSCode`；只认前者会静默漏计 VSCode 侧有用量的会话）（原实现从 `Data` 整树递归找「名为 index.json 且祖先含 history」的文件——该根下约 1 万目录 / 10 万文件 / 1.5 GB，实测遍历+stat ≈12.5s，为找数百个会话索引白走 8.5 万个 json 与 messages/file-tree 等子树），结构异常时按**产品子树**判零回退「限深递归 + 剪 messages/file-tree/check-point/plan-task」兜底（某产品布局漂移而其余产品正常时不静默漏收）；② 目录判定用 `DirEntry::file_type()`（免额外 stat）；③ `parse_file` 增加 `"usage"` 快速预筛（不含该字面量的行不解析，`PARSE_REV` 2→3 强制旧缓存重解析）；④ 视图新增 `walk_ms` / `parse_ms` / `files_parsed` / `scan_ms` 指标并把 `cache_hit_files` 修正为真实命中数（原为扫到的文件数），收尾打 `[wb-token] 扫描 …` 分段耗时日志；⑤ 调度器 `wb-credits-snapshot` 的 token 同步改 `fresh=false`（原 `true` 绕过结果缓存每日强制全量重扫）。**去重修订（2026-10-08，issue 口径修复）**：同一批请求会以**新的会话 id** 重新登记到另一个 uid 的目录下（实测同一 workspace 下 609 / 4123 行跨 uid 重复），仅靠「文件内按 request id 去重」会把整份副本再计一次 → 聚合层新增**跨文件按 request id 全局去重**（`FileCacheEntry::ids` 记录各文件「实际计入」的 id：整份重复整文件跳过、无重复走快路径、部分重复按「已计 id」过滤后重解析；过滤结果依赖扫描顺序故不进缓存），`PARSE_REV` 3→4 强制重算一次；本机实测 IDE 源 total 102.86 亿 → 85.74 亿（−16.6%，折算三路合计约 13.8%）。诊断探针 `probe_codebuddy_ide_dedup`（`#[ignore]`）可在真机目录上对比去重前后。 |
| WorkBuddy | `workbuddy_usage_fallback()` | 积分用量快照回退（F-27，批次4）：官方用量不可用时自动切换——本地余额时序差分（data/workbuddy_credits_history.json，credits_fetch 非缓存时按日追加 cap 365）+ 签到日志「+N」奖励推导当日充值；口径明示「快照回退」非官方逐请求 |
| WorkBuddy | `workbuddy_activity_info(userId?, refresh?)` | 活动信息三端点聚合（F-51，批次4）：公开 GET `/v2/activity/banner` + billing POST `get-payment-type`/`get-dosage-notify`；宽容解析逐项容错（errors[] 明示），缓存 10min（data/workbuddy_activity_cache.json） |
| WorkBuddy | `workbuddy_ui_click_capture()` / `workbuddy_ui_click_checkin()` | UI 坐标点击签到兜底（F-18，批次4）：ctypes user32 驱动鼠标（零新依赖）；仅手动触发、默认关闭（settings.ui_click_enabled）；取点 3 秒倒计时记录坐标，执行单次单击不循环 |
| WB 配置 | `wb_route_config_get()` / `wb_route_config_set(config)` / `wb_template_map_get()` / `wb_template_map_set(map)` | 四段模型路由与审核模板映射两个手工配置文件的程序化读写：读 data/ 新路径回退旧根；set 结构校验与读取方反序列化严格对齐（aliases/rules/suffixes 为 object，模板表为 {templates:[{from,to}]} 形态），写 data/ 新路径并逐出读缓存（网关热路径即时生效） |
| Buddy 双应用 | `open_workbuddy_app()` / `open_codebuddy_app()` / `codebuddy_env_check()` | 启动 WorkBuddy / CodeBuddy 桌面客户端（app_locate 探测，未装返回明确错误，不注入代理）；CodeBuddy 环境探测 `{installed,running,exe,version,uid,nickname}`（CodeBuddy CN 桌面与 WorkBuddy 共享 auth 文件 `%LOCALAPPDATA%\CodeBuddyExtension\...\workbuddy-desktop.info`，uid 同源 → 账号页「CodeBuddy在线」徽标）；`-TargetApp CodeBuddy`（authfile 布局）切换/保存快照落 profiles_codebuddy |
| Qoder | `qoder_env_check()` / `qoder_open_ide()` / `qoder_open_work()` | Qoder IDE 环境探测 / 启动 IDE / 启动 Qoder Work（app_locate 四级探测，`target_app=Qoder`） |
| Qoder | `qoder_oauth_login()` / `qoder_oauth_cancel()` | PKCE 设备流登录（R-10 抓包固化）：nonce+verifier 生成 → 系统浏览器开 `qoder.cn/device/selectAccounts?challenge=...` → 轮询 `GET /api/v1/deviceToken/poll`（pending=404，≤180s/1s）→ **响应 nonce 回验**（防会话混淆）→ 自动入池落库；事件 qoder-oauth-progress / qoder-oauth-done |
| Qoder | `qoder_settings_get()` / `qoder_settings_set(patch)` | kv("qoder_settings")：auto_checkin（启动补签）/ checkin_hhmm / credits_sync_hhmm / credits_sync_enabled / lazy_refresh_hours |
| Qoder | `qoder_accounts_list` → `QoderAccountView[]` | 账号池 ∪ token store 合并视图（脱敏：只回凭证 kind 徽标不回 token）；列表前惰性回填每账号设备指纹（§5.4）；async |
| Qoder | `qoder_account_save/remove/move` | 改名备注（脏检查：名称/备注均未变直接跳过，不触发网关池 reload）/ 移除（同步清 token store + vault，DB 落库成功才清 vault；池外孤儿凭证（token store 有记录而池条目缺失）仍可清理，清理失败返 Err 透出）/ 分组移动（跨组移动联动网关池热重载——api_pool 按 group_id 过滤调度）；save（有变更）/remove/move/import_pat/OAuth 入池/IDE 扫描/账号导入均联动网关池热重载 |
| Qoder | `qoder_account_import_pat(name?, pat)` | PAT/作业令牌手工导入（幂等：同 token 稳定同 id `qd-<sha256前12>`）；`pt-`/`jt-` 前缀均写入 access_token+pat 双字段（确保 ensure_fresh PAT 重换通道覆盖）；**导入成功后清理旧设备流凭证残留**（refresh_token/refresh_expires_at_ms；machine_id/machine_token 保留——PAT 通道仍消费有效指纹）；userinfo/plan 回填失败容错 |
| Qoder | `qoder_account_refresh_token(account_id)` → `QoderAccountView` | 单账号凭证续期（账号管理操作列手动按钮，对照 workbuddy_refresh_token 同语义）：force 恒刷（lazy_hours=i64::MAX 走 ensure_fresh 无条件重换路径——PAT 通道重换作业令牌、客户端通道 deviceToken/refresh 续期）；每账号互斥由 refresh_lock_for 兜底；成功回读最新视图（到期时间/登录态即时刷新），失败按 note 分类中文归因（no_credential/refresh_failed 暂态/pat_rejected/auth_dead 永久）；async（spawn_blocking） |
| Qoder | `qoder_ide_scan()` / `qoder_cli_status()` / `qoder_live_logins()` | 本机 Qoder 客户端账号发现 / CLI 切号桥状态 / 双客户端实时登录标记——`{ide, work}` 均为池反查后的账号 id（ide=state.vscdb `secret://auth.userInfo` 解密；work=Work 数据目录根级 `auth.v1.dat` v10 解密 `user.id`，2026-10-04 实测修正：Cookies 库无 qoderuid）；未登录/解密失败 → null（fail-open，前端不展示徽标）；async（spawn_blocking 派发，join 失败拒绝 → 前端 catch 静默置空） |
| Qoder | `qoder_checkin_start(opts)` → NDJSON `qoder-checkin-progress` | Rust 直调 `tasks/qoder_checkin.rs::run_checkin_round`（sash 双活动 claim，幂等回放归类已签；claim 间隔 1~3s 随机抖动；401 刷新一次重试；campaign_id 路径段白名单；done 事件含 `failed_empty_campaigns` 单列计数） |
| Qoder | `qoder_checkin_results(days?)` | 签到日志（qoder_checkin_results 表 90 天滚动，逐条 UPSERT 按 pk=date\|user_id\|time_ms 去重） |
| Qoder | ~~`qoder_checkin_task_register/_status/_unregister`~~（2026-10-05 移除） | 每日签到统一由应用内 Rust 调度器 `qoder-checkin` 驱动（scheduler.rs 60s tick + 当日幂等 + 启动补跑），时刻 `settings.qoder_checkin_hhmm` 可改；不再有 schtasks 双轨命令 |
| Qoder | `qoder_credits_fetch(userId?, fresh?)` | Rust 直调 `tasks/qoder_credits.rs`：usage 三通道取数（R-7 主结构 userQuota/addOnQuota + 宽容兜底全 None 显式失败）+ R-11 逐包明细端点 `GET {WEB_BASE}/api/v2/me/usages/big_model_credits`（官网域 qoder.cn 优先、openapi 兜底；total_quota.quota_detail → packages[]，source: plan 订阅配额 / bonus 个人资源包）；sash usage 原生 `dedicated_resource_packages[]` 同步解析为逐包（source: dedicated 专属/组织资源包，Work 客户端解析器同款字段、status 枚举前缀 QUOTA_DETAIL_STATUS_* 归一，已用完/非激活过滤、expires_at 缺失回退订阅周期）；字段级覆盖、全失败回退聚合口径并记 app.log warn + 30 分钟负缓存防噪音（双端点实测恒 401/503：qoder.cn 需 Web 会话鉴权、openapi 主机无路由；TTL 内静默跳过、TTL 过自动复探自愈）；缓存 10min（仅全部成功落缓存）；401 自愈刷新一次；成功回写池余额；全量且全部成功才落每日快照（含当日消耗差分） |
| Qoder | `qoder_credits_history_list()` | 积分每日快照时序读取（qoder_credits_history 表 365 天，同日覆盖） |
| Qoder | `qoder_groups_list / create / update / remove` | 分组管理（kv("qoder_groups")，结构与 Trae/Buddy 一致；create 重名校验；删除时组内账号回落未分组） |
| Qoder | `qoder_accounts_export(includeCredentials?) / _import(payload)` | 账号库导入导出（kind 标记 `aiwork-qoder-pool`；凭证是否随行由用户勾选，含凭证导出前端强确认；分组定义随载荷导出，导入按 id 幂等合并 + 幽灵 group_id 回落未分组） |
| Qoder | `qoder_env_reset_items()` / `qoder_env_reset(items)` | Qoder 环境残留清理清单 + 程序化清理（执行前关闭 Qoder IDE 与 Work 客户端——本体进程同名 "Qoder CN.exe" 一并覆盖；9 项含 QoderWork 客户端 `%APPDATA%\com.qodercn.app.stable` 的 Local State + Network\Cookies；单项失败不中断） |

### 5.1 双应用与双 uid 体系（F-08，trae_apps.rs）

- 数据源：`%APPDATA%\TRAE SOLO CN`（Trae Work）与 `%APPDATA%\Trae CN`（Trae CN IDE）各自的 `User\globalStorage\storage.json` / `state.vscdb`。
- **两套 uid 体系不通用（红线）**：`iCubeAuthInfo://icube-dc:<uid>` 键名中的 uid 是**账户中心（dc）id 空间**；账号池 / JWT `data.id` 用的是 **Cloud-IDE id 空间**。同一登录账号两者数值不同，直接混用会导致重复入池。
- 当前登录账号的 Cloud-IDE uid 由使用痕迹推导（Trae CN 看 `icube_gtm.users` 键名；Trae Work 看 state.vscdb `solo.mobile.allowControl` per-uid 最新 `updatedTime` + 键名证据计数），推导失败时回退展示 dc uid 并标记 `uid_confident=false`、**禁止入池**。

### 5.2 API 网关双上游与 WorkBuddy 适配（批次 2，T2.1~T2.4/T2.7）

- **双上游路由**：`api_server/pool.rs` 调度引擎同时服务 SOLO 池与 WB 池（`ApiSharedState.wb_pool`，两实例并存）。请求模型命中 `wb_model_catalog.json`（15 模型静态兜底 + 人工/动态可替换）→ WB 上游 `POST {chatBase}/v2/chat/completions`（CN=copilot.tencent.com / Global=www.workbuddy.ai，按账号 domain 含 `.workbuddy.ai` 判定）；否则走既有 SOLO `llm_utils_chat`。`api_pool.json.wb_enabled=false` 时 WB 模型返回 400。
- **WB headers 三铁律（wb_upstream.rs，红线）**：① Origin/Referer 必带按区域；② 缺省字段显式 `X-No-User-Id/X-No-Enterprise-Id/X-No-Department-Info: 1` 占位；③ **chat 请求绝不携带 `X-Refresh-Token`**（仅 refresh 端点，配 `X-Auth-Refresh-Source: workbuddy`）。UA 伪装 `CLI/2.63.2 CodeBuddy/2.63.2`。
- **请求体改写（wb_payload.rs）**：强制 `stream:true`（上游只回 SSE，非流式本地聚合 wb_sse::aggregate，tool_calls delta 按 index 合并）；tool_choice 对象→string；reasoning_effort 按目录 `supported_efforts` 降级（`effort_override` 修正层最优先，如 hy3-*→high）；指纹清洗（默认开）：cc_xxx 键值/x-anthropic-* 引用剥离 + 审核模板黑名单最小改写（映射表 `wb_template_map.json` mtime 热更新，缺失用内置兜底：CLI→CLI tool、Main branch→Default branch）；连续同角色消息自动合并。
- **调度扩展（pool.rs）**：策略新增 `weighted`（三因子=积分占比×10+闲置补偿 0.5/h 封顶 5.0+成功率×3，Top5 加权随机）与 `p2c`（随机选二取优），保留 expire_first/credit_first/random；100ms 防惊群窗口。五态机：Available/QuotaProtection（hard_credit 冷却至**次日 04:00** 自动恢复）/RateLimited/Forbidden（403/SessionDead 禁用）/ProxyDisabled，随 PoolStatus.state 下发。熔断：连续 3 错 30m 起指数递增（×2）封顶 6h，成功重置。
- **分级重试（retry.rs 纯函数）**：429=Retry-After 优先/线性 1/2/3s→耗尽换号；503/529=10/20/40s 指数；400+thinking.signature=200ms 重试一次；502 同号重试 1 次；401/403=换号（**WB 401 先刷新一次凭证同号重试，T2.6**）；400=context_too_long 类 Fatal 透传。**SOLO 主路径（routes.rs）现已接入同一 `retry_plan`**（同号重试/退避/换号/Fatal 透传，Retry-After 头解析）并复用 `lines_with_first_byte_timeout` 10s 首字超时；**Trae 池流式首字接入竞速对冲 `race_trae_first_byte`**（wb_route/qoder_route 同构，`trae_hedge_threshold_ms` 默认 8000/0=关，运行时 clamp 1s–8s，pool_set 保存即热应用）：首字节超阈值且存在其他健康账号时经 `lines_with_first_byte_hedged` 向第二账号发对冲请求（请求体按对冲账号指纹经 `prepare_llm_chat_body` 重建，注入对冲账号 device_id/machine_id），先出首字者胜；对冲接管后记账/日志/粘性绑定均记生效账号 win_uid（HedgeLease RAII 竞速窗口恰好释放一次）；SOLO 流式 keep-alive ticker 经 watch 信号在流结束时退出（普通 HTTP 客户端可正常收到流终结）。
- **会话粘性（wb_sticky.rs，三池同构分命名空间）**：显式 `conversation_id` 绑定（TTL 滚动续期，`wb_sticky_ttl_secs` 可配默认 1800s）+ 无 id 时指纹模式（前 3 消息 SHA256 前 6 位 + 60s 窗）；绑定含上游 conversation_id（双段分配），Mutex 内 re-check 防 TOCTOU；持久化 sticky_bindings 表，键命名空间隔离：WB=空串 / Qoder=`"q:"` / Trae=`"t:"`（`owns_key` 判定 + `save_ns` 范围替换防互删），三池互不串绑；TTL 均可配（Trae 经 `trae_sticky_ttl_secs`、Qoder 经 `qoder_sticky_ttl_secs`，evict_expired 按 store 生效值清理）。
- **工程化（T2.7/F-34）**：模型级冷却 10→20→40s 渐进退避（优先级高于 Key 级，成功清除）；SSE keep-alive 15s 注释行（SOLO 与 WB 流式均已接入）；首字超时 10s 故障转移（转发线程 + recv_timeout，Agent 300s 读超时兜底 detach）；客户端断连后继续消费上游保 usage 完整（wb_sse 忽略 send 失败直至 EOF）。
- **运维接口（T2.3/F-32）**：`/healthz`（无健康账号 503）；`/v1/models` 合并 WB 目录（owned_by=workbuddy）；`/status`、`/health` 增加 `wb` 段（池画像/模型冷却/粘性会话数/上游健康探针 `probe_ok`+`probe_ts_ms`：-1 未探测/0 不可达/1 在线，§2.2 频控 5min+0-60s 抖动）；请求日志含 TTFB（WB 与 SOLO 流式均覆盖，SOLO 经 `log_request_ttfb` 结构化字段）。
- **ck_ 子 Key 体系（F-35，批次3）**：对外子 Key（`ck_` 前缀，前端 crypto 随机源生成；旧 `sk-` 兼容）与上游真实凭证分离。`api_keys.json` 条目扩展：`allowed_accounts`（上游 uid 白名单，空=不限）、`schedule_mode`（`expire_first` 临期优先默认 / `dedicated` 专一固定 `dedicated_account`）、`daily_stats`（按日请求统计 cap 90 天）。鉴权中间件把 `ResolvedKey` 快照注入 extensions；wb_route 流式/非流式取号统一走 `pick_excluding_constrained`（专一锁定 > 白名单过滤 > 池策略），粘性绑定不白名单内时忽略粘性。

### 5.3 三池调度与统一模型目录（v3.3.x）

- **资源池**：`trae`（SOLO `llm_utils_chat`，积分 208）/ `buddy`（copilot.tencent.com 或 www.workbuddy.ai，按账号 domain 判定）/ `custom`（自定义 OpenAI 兼容上游 `custom_models.json`，**命中即直达、不参与池间策略**）。协议细节见 tech-framework.md §5.5 与附录 B。
- **池间策略（`dispatch.rs`）**：`smart` 默认——按请求模型对可用源池排序：① 池内最早积分到期优先（无到期数据后置）→ ② 该模型倍率小者优先（0=免费最优）→ ③ 健康账号剩余积分总和多优先；全并列回退固定序（Buddy 优先现状）。`priority` 为改造前严格按序行为。`per_model` 显式覆盖不做智能重排（用户显式配置优先）；`fallback=false` 时仅用首选池。
- **统一目录（`unified_catalog.rs`）**：`api_unified_models` 与 `GET /v1/models` 共用；Trae 模型元数据四层兜底——L1 人工覆盖（`trae_model_meta.json`，编辑弹框 upsert/clear）→ L2 官网同步 → L3 默认 128K / 倍率参考（含 2026-09-13 审查补充的 5 个官网同步缺失倍率）→ L4 系列/思考档位/图片支持推断。倍率口径冲突时只补缺失条目、不改既有值。
- **custom 路由（`custom_route.rs` / `custom_models.rs`）**：请求模型名 canonical（trim+lowercase）命中 enabled 自定义模型即直转其 `chat/completions`（`chat_url` 归一 base 含 `/v1` 与否两种形态），Bearer 用条目 API Key；响应侧复用既有协议输出层。

### 5.4 Qoder 约定（F-80）

- **凭证三前缀**：`pt-`（PAT，官方认可）→ 经 `POST /api/v1/me/jobToken`（R-6 抓包真实路径，探测顺序首位）换 24h 作业令牌再调业务端点；`jt-`（作业令牌）；`dt-`（设备流 token，≈30d，配 refresh_token ≈360d）。`ensure_fresh` 惰性刷新：PAT 通道（`is_pat_channel`：access_token `pt-` 前缀 ∥ pat 备份非空——jt- 导入双写 pat 后同走此通道）临期用原始 PAT 重换；客户端通道走 `deviceToken/refresh`；401 自愈传 `lazy_hours=i64::MAX` 恒刷一次（禁二次刷新）。
- **凭证存储（红线）**：敏感字段（access_token/refresh_token/pat/machine_token）只进 vault（ns="qoder"，Stronghold+DPAPI），DB `qoder_tokens` 表一律占位空串；QoderCreds Debug 手写脱敏。全程零 token 输出到日志/事件/UI。
- **并发防护（三层）**：① `TOKEN_STORE_LOCK` 表级读改写互斥；② **每账号刷新锁**（`refresh_lock_for`）串行化同账号 ensure_fresh 全程，持锁重读即二次检查（防并发刷新互相覆盖丢 token）；③ save_token_store 落库前重新 load DB 最新表做**仅目标行替换**的行级合并（收窄跨进程 last-writer-wins 窗口）。锁序：refresh 锁 → TOKEN_STORE_LOCK。
- **设备指纹（设计文档 §5.10 多账号并发）**：每账号入池即生成稳定 `QoderDeviceProfile`（一次生成永不轮换）；MITM/抓包真实捕获值优先透传，缺失时 `effective_creds` 注入账号绑定 machine_id + 现场随机 machine_token（随机值不落库）。注入唯一出口 = `effective_creds` / ensure_fresh 合并层。
- **expires_at 域钳制**：store 读入的过期时间超 (0, now+10y) 一律视为无过期信息（防脏数据溢出/千年展示）；`refresh_expires_at_ms` 随设备流/refresh 响应解析落库留档。
- **调度四任务**：每日签到（默认 10:15，覆盖 0 点签到 + 10:00 登录奖励；失败返 Err → 调度器 30min 冷却重试；启动补签 60s 延迟 + 轮次锁互斥，empty_campaigns 不推送打扰；失败口径=failed − failed_empty_campaigns，调度器/UI/CLI 三路一致）/ 积分快照（qoder_credits_sync_hhmm；全账号拉取失败 stale 回退历史缓存（cached:true）按暂态返 Err 交 30min 冷却重试——当日快照点位不可后补，失败性质分流对齐凭证刷新）/ 凭证 6h 兜底刷新（qoder_token_renew_enabled 可关默认开；lazy 7h 窗口，暂态失败返 Err 重试，永久失败落日志提示人工）/ 模型目录同步（qoder-catalog-sync，每日 05:50，qoder_catalog_sync_enabled / qoder_catalog_sync_hhmm 可控均默认开，真 COSY 签名拉 model/list → adopt_remote 替换 CN 区缓存；空池/无凭证静默跳过）。轮次锁 `QODER_ROUND_LOCK`（进程内：调度器/启动补签/UI 三路互斥，调度器抢不到锁返回 `skipped_busy` 幂等跳过）+ 跨进程锁 `CrossProcLock`（Windows 命名互斥体**单段名** `Global\AIWorkAssistant.qoder.<scope>.<data_dir哈希16>`（首版多段名 `Global\AIWorkAssistant\qoder\<scope>\<hash16>` 恒 CreateMutexW ERROR_PATH_NOT_FOUND(3)——内核对象命名空间无中间对象目录，创建失败被误判 Busy 致锁自合入起从未成功获取、双进程防护空转 54+ 分钟，2026-10-04 日志分析修复），scope=checkin/refresh、3s 抢锁——schtasks CLI `--task-run` 与应用内调度器同刻双进程对同账号并发 ensure_fresh 会以同一 refresh_token 刷新、服务端一次性轮换下后到者误标 needs_relogin；锁名含 data_dir 短哈希，多数据目录实例互不误伤；`WAIT_ABANDONED` 前持有进程崩溃视为获取成功，RAII Drop 补 ReleaseMutex 再关句柄（仅 CloseHandle 不 Release 时所有权悬于本线程、他方等待者全超时假 busy），非 Windows 占位桩直通；抢锁失败 done/返回值带 `skipped_busy:true`，日志区分原因（`CrossProcLockFail`：创建失败/真占用/等待失败——「另一进程正在执行」仅真占用时出现））；锁序：轮次锁 → 跨进程锁（三路一致）；调度器见 `skipped_busy` 不 mark_run（跳过≠完成，保留当日重试链）；**Qoder 账号生命周期事件（save（有变更）/remove/move 分组移动/import_pat/OAuth 入池/IDE 扫描/导入）均联动网关池热重载**。
- **网关上游（p3-3 + F-80-余 v2，仅 CN 区）**：`qoder_enabled` 开关（Qoder 环境配置页·网关上游，运行中热应用）→ Qoder 目录模型路由专用 `qoder_pool`（fail-open 全量入池，needs_relogin 即禁用；池内 access_token 仅入池门槛，请求期真凭证由 `qoder_identity` 回调按次 `ensure_fresh` 解析；并发上限 `qoder_account_concurrency_limit` per-pool 热生效（Qoder 资源调度页编辑）。执行链 = `prepare_qoder_body` agent 固定信封 → `encode_body` → COSY 19 头签名（qoder_sign.rs，RSA 内置公钥）→ `gateway.qoder.com.cn` agent_chat_generation SSE → 双层信封翻译（qoder_upstream.rs）。错误四分类：10605 排队（不冷却不换号，同号退避≤3 次）/ 105 鉴权 / 110~122 额度 / 裸 403 Forbidden；**identity 解析失败按暂态/永久二分**：refresh_failed（网络/5xx，含 PAT 换取首通道 0/5xx）带 `TRANSIENT_ERR_TAG` 走 Server 熔断可自愈，仅永久失效 SessionDead 禁用；用量独立 `qoder_days` 桶（`api_qoder_usage_stats`，TTFT 与 WB 同构入账）。**F-80-余 v2**：① 慢请求竞速对冲同构 WB（`race_qoder_first_byte` 原始行源层竞速 + `QoderHedgeLease` RAII，`qoder_hedge_threshold_ms` 默认 8s 热参数；对冲接管后排队同号退避不适用——重试凭证/请求体属主账号）；② 会话粘性（`qoder_sticky` 复用 StickyStore，键命名空间 `"q:"` 与 WB/Trae 同表隔离；显式 conversationId/指纹双模式，显式 TTL 经 `qoder_sticky_ttl_secs` 可配默认 1800s、指纹 60s 短窗；粘住账号 + 同种子派生同一上游 session_id；busy 且有空闲候选让位同 F-77④；`qoder_sticky_enabled` 默认关）；③ Global 区（api3.qoder.sh）**产品决策仅 CN 区**：Global 专属模型仅目录可见、显式 404，Qoder 资源调度页带地区标注说明；接线前置未决项（账号-区域关系 / Global 域 COSY 签名）已留档 backlog。
- **网关上游审查批（v3.7.0 阶段 5）**：SSE 信封 `statusCodeValue` 钳制 100..=599（i64→u16 截断会把越界值误映射进合法码区间如 65937→401 误入换号链；越界按 502）；目录 resolve 兜底序 `[region, other]` 对齐远程目录序（同名条目不再解析到对区兜底）；pricing_url 提取过 http(s) 协议白名单；请求体 `max_tokens` 兼容 `max_completion_tokens`（OpenAI 新客户端）；session_seed / sticky_seed / conversationId 确定性限长 128 字符（`chars().take`，防字节切片 panic；SessionKey 由 WB/Qoder 路由共用，两侧一致截断保证匹配）；池空错误消息中文化（`code: no_healthy_account` 保留机器可读）；`js_truthy`/`truthy` 合一（目录解析与错误分类共用）；CatalogState 死状态 `*_fetched_at` 字段删除。
- **签到/积分/导入导出审查批（v3.7.0 阶段 4-5）**：① 签到 `done` 事件补 `skipped_busy:true`（跨进程锁占用），前端专项 warn 分支（不误报「成功 0 已签 0」）、启动补签日志同步识别；② data_io 导出前剥离 `machine_id`/`machine_token`（对齐导入侧）；**KDF 升级 Argon2id**（19MiB/t=2/p=1，抗 GPU/ASIC 暴破）：导出信封 `kdf: "argon2id"`，导入经 `kdf_scheme_of` 分派派生（"sha256-iter-N" 旧格式兼容可解、kdf 字段缺失回退 sha256 旧默认、前缀不识别/越界 >10⁷ 明确报错请升级）——双格式 roundtrip 单测锁定；③ 命令异步化：accounts list/save/remove、export、import_pat 探测均 spawn_blocking；groups create/update/remove defs 读改写全程持 `qoder_pool_lock`（与导入分组合并互斥）；④ 积分 `deep_balance_dig` 负余额钳 0（与 quota_pair 同款守卫）；快照 ts 与缓存 `fetched_at_ms` 按拉取**完成**时刻取时（入口时刻会让多账号拉取耗时吃掉缓存 TTL）；⑤ OAuth 导入凭证前发 `importing` progress 事件（防 30s+ 静默）。
- **人工确认疑点批（v3.7.0 阶段 6）**：① jt- 换号链路补纯函数单测锁定（`is_pat_channel` 判定 + `job_token_attempts` 探测端点 `/api/v1/me/jobToken` 首位——真实 token 无法离线复现，以单测固化行为契约，含「jt- 无 pat 备份不走 PAT 通道」反例锁定）；② PAT 导入成功后清旧设备流凭证残留（`clear_device_flow_creds`：refresh_token/refresh_expires_at_ms；machine_id/machine_token 保留——有效指纹 PAT 通道仍消费）；③ `qoder_account_save` 脏检查（name/note 均未变跳过网关池 reload）；④ env_reset 第 9 项 `work_client`（QoderWork 客户端 Local State + Network\Cookies；Work 本体进程与 IDE 同名 "Qoder CN.exe"，关闭动作天然覆盖）；⑤ KDF 迁移 Argon2id（见上条②）；⑥ **明文兼容导入通道保留为有意决策**（向后兼容旧版导出文件；仅导入解包瞬间经手，运行期存储/导出/日志凭证红线不变）；⑦ Qoder 签到进行态 store 化（监听与归约移入前端 store 层，见 §6）；⑧ Global 区维持仅 CN 不接线（产品决策：Global 专属模型仅目录可见、显式 404；账号-区域关系 / Global 域 COSY 签名等前置未决项留档 backlog，见 F-80-余 v2）。
- **切换器**：`target_app="Qoder"`（authfile 布局），快照落 `data/profiles_qoder`；`target_app="QoderWork"`（electron_root 布局，Work 数据目录 `%APPDATA%\com.qodercn.app.stable`）快照落 `data/profiles_qoder_work`。electron_root 根级白名单 9 项含**登录凭据三件套** `auth.v1.dat` / `auth.machine-id` / `auth-profile-overlays.v1.dat`（2026-10-04 切换无效根因修复：Work 登录真源 = 数据目录根级 `auth.v1.dat`（v10 密文 JSON，token/refreshToken/user.id），原白名单仅 6 项 → 恢复 Cookies/LocalStorage 成功但客户端启动仍读旧 `auth.v1.dat`，切了等于没切）；恢复侧**对称清理**——快照不含而数据目录存活的凭据文件删除（旧版快照恢复后不串号：Work 回退未登录态，重登 + 重存快照自愈）；完整性校验对 `auth.v1.dat` 缺失 Warn（旧版快照合法存在，不 fatal）。

## 6. Tauri 事件（Rust → 前端）

| 事件 | payload |
|---|---|
| `proxy-log` | `string`（代理 stdout 逐行） |
| `account-captured` | `string`（新捕获的 userId） |
| `checkin-progress` | `{type:'start',total}` / `{type:'account',...}` / `{type:'done',ok,already,failed}` |
| `switch-progress` | `string`（switcher NDJSON 单行） |
| `switch-done` | `{ success: boolean, raw: string }` |
| `save-login-progress` | `string`（switcher NDJSON 单行） |
| `save-login-done` | `{ success: boolean, raw: string }` |
| `update-download-progress` | `{ received, total, percent }`（更新包下载进度） |
| `update-installing` | `string`（asset_name，安装器已启动、应用即将退出） |
| `wb-checkin-progress` | `{"type":"start",total,mode?}` / `{"type":"account",index,user_id,name,status,message}` / `{"type":"growth",index,user_id,name,status,travel?,lottery?,tasks?,energy?,streak?}` / `{"type":"done",ok,already,failed,mode?}` / `{"type":"exit",ok}`（WorkBuddy 签到/成长中心独立管线：`mode:"growth"` 标记成长事件，与 Trae checkin-progress 互不串扰） |
| `wb-oauth-progress` | `{stage:'init'|'browser'|'polling'|'success'|'error', message, auth_url?}`（OAuth 扫码流程进度；auth_url 仅 browser 阶段携带） |
| `wb-oauth-done` | `{ok, id?, nickname?, message}`（扫码结果；成功已入池，凭证不出 Rust） |
| `qoder-checkin-progress` | `{"type":"start",total}` / `{"type":"account",index,user_id,name,status,message,reward?,campaigns?}` / `{"type":"done",ok,already,failed,failed_empty_campaigns,skipped_busy?}`（Qoder 签到管线，与 wb-checkin-progress 前端组件同构；`failed_empty_campaigns` 为活动未开始/不可用类失败计数，启动补签推送按 `failed - failed_empty_campaigns` 判定；`campaigns` 为 F-80-余 v2 逐活动明细 `[{id,name,kind,reward?}]`，档期日历数据源；`skipped_busy=true` 表示跨进程锁占用本轮跳过——调度稍后自动重试，前端专项提示**不计入**成功/失败统计；**疑点⑦ store 化**：监听与归约在前端 store 层 setupListeners/`applyQoderCheckinLine`（qoderCheckin.running/lines/done/doneRev），页面切走/卸载不丢事件，QoderCheckin 页按 doneRev 联动刷新） |
| `qoder-oauth-progress` | `{stage:'init'\|'browser'\|'polling'\|'success'\|'error', message, auth_url?}`（Qoder 设备流登录进度；auth_url 仅 browser 阶段携带） |
| `qoder-oauth-done` | `{ok, id?, nickname?, message}`（设备流结果；成功已入池，凭证不出 Rust） |

## 7. 数据文件

> **v3.4.5 起存储层 SQLite 化**：下述 JSON 状态全部迁入 `data/aiwork.sqlite`（WAL 模式），运行期不再产生 JSON 文件读写。首启迁移器按注册表把旧 JSON 导入后移入 `data/backup/`（保留相对结构 + `migration_manifest.json` 清单）；幂等（`PRAGMA user_version` 闸门）。本文及 §5 提及的 JSON 文件名均为**逻辑名**（kv 键 = 文件名去 .json）。

```
%APPDATA%\AIWorkAssistant\
├── conf/
│   ├── app_settings.json         # Settings 全字段（snake_case）——UI 可编辑配置保留文件形态
│   ├── vault.stronghold          # jwt/refresh_token 权威加密存储（Stronghold）
│   └── vault_key.bin             # vault 主密码（DPAPI 加密）
├── data/
│   ├── aiwork.sqlite             # 全量状态库（WAL；-wal/-shm 常驻属正常）
│   │   ├── kv                    # 键值文档表：app_settings / api_pool / dispatch_policy / api_models /
│   │   │                         #   wb_model_catalog / wb_model_route / wb_template_map / wb_sticky(逻辑) /
│   │   │                         #   workbuddy_settings / credits 三缓存 / wb_cli_rotate_state / doubao_* 等 29 键
│   │   ├── 行文档实体表           # accounts / device_map / groups(+group_members) / remaining_credits /
│   │   │                         #   account_cooldowns / pay_status / api_keys / custom_models /
│   │   │                         #   doubao_accounts / wb_accounts / wb_tokens / api_usage /
│   │   │                         #   qoder_accounts(池) / qoder_tokens(凭证 store，敏感字段占位) —— (pk, data JSON)
│   │   └── 列化流水表             # credits_history / credits_daily / checkin_results(90天) / wb_checkin_results /
│   │                             #   doubao_health_events / wb_credits_history(365天) / usage_history_*(365天) / sticky_bindings /
│   │                             #   qoder_checkin_results(90天) / qoder_credits_history(365天)
│   │   kv 增量（Qoder）          # qoder_settings / qoder_groups / qoder_credits_cache(10min TTL)
│   ├── backup/                   # 首启迁移时移入的旧 JSON（含 migration_manifest.json / corrupt/）
│   ├── workbuddy_chats/          # WorkBuddy 会话三件套备份（<uid>/projects/ + 双 db + chat_backup_meta.json）
│   ├── codebuddy_chats/          # CodeBuddy 会话三件套备份（F-74，同上结构，源 ~/.codebuddy）
│   ├── doubao_chats/             # 豆包对话数据备份（<uid>/，按 profile 分层）
│   ├── exports/                  # 导出产物（doubao_chats_<uid>_<ts>.md/.json 等）
│   ├── certs/                    # 自签 CA（ca.cer / ca.crt / ca.key）
│   ├── profiles/                 # 登录态快照（Trae Work）
│   │   ├── current_account.txt   # 当前活跃账号 ID
│   │   └── <user_id>/            # 精准备份的 9 类核心文件
│   ├── profiles_trae/            # Trae CN 快照槽
│   ├── profiles_doubao/          # 豆包快照槽（chromium 布局 + snapshot_meta.json + .bak 单代回滚）
│   ├── profiles_workbuddy/       # WorkBuddy 快照槽（auth/ + storage/ + meta.json）
│   ├── profiles_codebuddy/       # CodeBuddy 快照槽（authfile 布局 + L3 vscdb 登录真源）
│   ├── profiles_qoder/           # Qoder 快照槽（authfile 布局，target_app="Qoder"）
│   ├── profiles_qoder_work/      # Qoder Work 快照槽（electron_root 布局，target_app="QoderWork"）
└── logs/                         # proxy / checkin / switcher / api / proxy-requests / app.log 日志
```

**写入约定**：SQLite 经 `store::db(data_dir)` 单连接串行访问（WAL + busy_timeout 5000）；UI 配置与 vault/日志/导出仍走 `fs_utils::write_json`（tmp + rename 原子替换）。

## 8. 登录态切换器（switcher 模块，原 PS 桥）约定

- **进程内直调**：`switcher::run_action(RunArgs, &ProgressSink)`——原 powershell 子进程管道已下线，NDJSON 行 `{"stage","status","message","time"}` 语义与 `*-done {success,raw}` 由 TauriSink/CliSink 一步到位，前端契约不变。
- **全局互斥**：并发 run_action 直接拒绝（「已有切换/备份操作进行中」），命令层预检（JWT/会话/守卫 uid）在前。
- **入参**：`action: Switch/SaveCurrentLogin/BackupCurrent/RestoreOnly/ResetMachineId/ResetDeviceIds/KeepAlive`；`user_id`（Reset*/KeepAlive 可空，经 ensure_uid_safe 白名单）；`target_app: trae_work|trae|doubao|workbuddy|codebuddy`；`proxy_port>0` 注入 `--proxy-server`（C1）；`include_indexeddb`（C4：备份纳入 `Default/IndexedDB`）；`expected_current_uid`（防误覆盖守卫）。CodeBuddy 为 authfile 布局：ProcNames 双形态 `CodeBuddy/CodeBuddy CN`、ProfilesDir=profiles_codebuddy、`~/.codebuddy` 无 account-snapshot 时确认步 skip+warn。
- **CLI 任务模式**：`--task-run doubao-keepalive`（schtasks 直调主 exe；启动器 `task_doubao_renew.cmd` 启动期原地迁移，内容不含 trae-switch-bridge.ps1 即跳过）。
- **精准备份**：仅复制 9 类核心登录文件（storage.json / state.vscdb / machineid / aha / Network 等），非全量镜像。
- **Switch 流程**：预检查目标快照 → 关闭 Trae Work → 保存当前到 last + 当前账号槽位 → 恢复目标 → 启动。
- **SaveCurrentLogin 流程**：关闭 Trae Work → 精准备份到 userId 槽位 → 启动。
- storage.json 路径：`User\globalStorage\storage.json`，键名为点号形态整体键（`telemetry.machineId`，非嵌套对象）。
- **豆包数据目录**：`%LOCALAPPDATA%\Doubao\User Data`（Trae 系用 `%APPDATA%`）；备份项含 Local State / Network/Cookies* / Local Storage/leveldb / Session Storage / DoubaoStorage / saman_app_state / saman_shell_db_storage，`Last Version`（C3 版本基线）。
- **C3 快照元数据与校验**：备份时写 `snapshot_meta.json`（`schemaVersion=1` / createdAt / chromiumVersion / includeIndexedDB）并复制 `Last Version`；恢复前 `Test-SnapshotIntegrity` 三层校验——① schemaVersion ≠ 1 直接中止（无元数据文件的旧快照仅 warn 并跳过）② leveldb 缺 CURRENT 或 CURRENT 指向的 MANIFEST 缺失 → 中止 ③ 快照版本 ≠ 当前安装版本 → 仅 warn 继续恢复。
- **单代回滚保护（chromium 布局）**：`Backup-ChromiumProfile` 覆盖已有槽位前把旧快照整体 `Move-Item` 到 `<slot>.bak`（旧 .bak 淘汰）；`Restore-ChromiumProfile` 主槽缺失时回退用 .bak，Switch 预检查同样放行 .bak。背景：Switch 的"备份当前到来源槽"依赖 current_account.txt 与客户端实际登录一致，不一致时会把错误状态反复刷进该槽且不可恢复（实测把 B 快照覆盖成混乱状态）。`Copy-SnapshotItem` 文件分支先删旧目标再拷贝——文件被锁拷贝失败时不会留下旧文件冒充成功；`Copy-SnapshotItem`/`Test-SnapshotIntegrity` 的参数为最终路径（`-Path`），由调用方解析主槽或 .bak。豆包优雅关闭等待 `GracefulWaitSecs=8`（chromium 落盘慢，3 秒强杀会导致文件锁/未落盘）。
- **防误覆盖守卫（ExpectedCurrentUid，chromium 布局）**：桌面端 Switch 前用 `detect_guard_uid_strict`（Local Storage/抓包新鲜度链检测 uid + **Live Cookies 登录会话验证**）取当前登录，经 `-ExpectedCurrentUid` 传给桥；桥仅在它与 current_account.txt **一致**时才把"当前态"回写进来源账号槽，否则只备份 last 槽并 warn（客户端手动重登/未登录/检测失败时保护账号快照不被错误状态覆盖）。`doubao_open_as_account` 与 `switch_account`（豆包路径）均接入。.bak/last 槽不在账号列表展示（`doubao_accounts_list` 过滤 `*.bak`）。
- **登录会话 Cookie 检测（原 doubao_chats.py `--check-login-cookie`，Rust `tasks/doubao_chats.rs`）**：Chromium Cookies 库的 cookie **名**为明文（值加密不影响），sqlite 判定 `host_key like %doubao.com` 且 name∈(sessionid,sid_guard) 是否存在；客户端运行中先复制 Cookies* 到临时目录再读。返回 `{ok, doubao_cookies, has_session}`。用途①`save_current_login` 保存前预检 Live profile（无登录会话 → 拒绝保存，防止未登录态入槽）；用途②`doubao_open_as_account` 目标槽预检（快照无登录会话 → 拦截并提示重存）；用途③切换守卫严格版（uid 检测可能被快照 localStorage 残留骗过——实测未登录客户端仍报旧 uid 导致守卫误放行，Cookie 存在性无法伪造）。Rust 侧 `check_profile_login_cookie` 返回 None（读库失败）时一律不阻断，保持可用性。
- **F-68 项目列表/最近打开跨账号保留（icube 布局）**：`state.vscdb` 的两个**全局单键**——`solo-lite.local-project-folders`（项目列表）与 `history.recentlyOpenedPathsList`（最近打开）——不随账号分区，恢复快照后只剩目标账号自己的那一份。`vscdb.rs` 在 `restore_profile`（仅 icube 布局、恢复成功后）先抽键、再按条目合并回写（快照内已有以快照为准，仅补入切换前多出项；数组按 id、entries 按 folderUri 去重，非 JSON 保守不改），写前 `state.vscdb.f68.bak` 单代备份 + 失败回滚，失败仅 warn。**账号分区键（`solo-lite:content-map:<uid>` 等）红线：零改动**。
- **F-74 切换时自动迁移会话（WorkBuddy/CodeBuddy）**：`settings.buddy_switch_migrate_chats`（默认关）。开启后 `switch_account` 在 `run_action`（桥 Stop→Restore→Start）**之前**执行前置作业：当前账号判定（WorkBuddy = `pool_account_id_by_auth_uid` 优先 + 桥标记兜底；CodeBuddy = 桥 `profiles_codebuddy/current_account.txt` 标记优先，因共享 auth 文件会被 WorkBuddy 覆盖）→ 与目标相同或判定失败则跳过 → `backup_chats` 备份当前三件套（失败仅告警并跳过迁移、不阻断切换）→ `copy_chats(当前→目标)`；进度以 `switch-progress` 的 `stage=migrate` 行下发。
- **authfile 布局（WorkBuddy，批次1）**：`Backup-AuthFileProfile` / `Restore-AuthFileProfile`——L1 必选 `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`；L2 体验 `~\.workbuddy\storage\user-<uid>*` 目录；槽位元数据 `meta.json`（schemaVersion=1 / uid / savedAt）。单代回滚保护对齐豆包（覆盖前挪 `<slot>.bak`）；恢复前校验 auth 文件存在（缺失即中止）；Switch 后 `Confirm-AuthFileSwitch` 轮询 `~/.workbuddy/storage/skeleton/account-snapshot.json` uid（30s 超时，fail-open 仅 warn）。客户端历史快照 `workbuddy-desktop.<ts>.<pid>.<uuid>.info` 不入快照槽。

## 9. Rust 后台任务与代理约定

- **数据目录**：`AIWORKDATA_DIR` 环境变量决定数据根目录；主进程内部模块直接持有 AppState 内存路径。`--task-run <name>` CLI 任务模式由 schtasks 以 `cmd /c set AIWORKDATA_DIR=... && 主exe --task-run <name>` 注入（state.rs 亦有缺省回退）。
- **NDJSON**：`--task-run` 签到任务与 `workbuddy_checkin_start` 事件流输出 `{"type":"start"|"account"|"done",...}` 单行 JSON（wb-checkin-progress 事件）。
- **稳定设备 ID**：`device_map.json` 缺条目时由 `commands/accounts.rs::derive_device(uid)` 确定性派生（seeded_stream，与原 Python `rand_digits(n, seed=user_id)` 算法兼容，测试 `test_derive_device_matches_python` 锁定）。
- **上游代理链（v2.4.3）**：`device_proxy/` 读取 `ProxyConfig.upstream`（由 `proxy_start` 直传），上游认证读 `UPSTREAM_PROXY_USER` / `UPSTREAM_PROXY_PASS`，支持 `http://host:port` 与 `socks5://host:port` 两种形态。**非 Trae 域名**的 CONNECT 隧道（`tunnel_raw`）与明文 HTTP 转发优先经上游出站，上游不可用时回退直连；Trae 域名仍走本地 MITM 解密以捕获 JWT。
- **动态叶子证书 AKI（2026-09-09）**：`leaf_cert` 签发的叶子证书必须带 Authority Key Identifier（OpenSSL 3.2+/Python 3.13 客户端缺 AKI 即拒：`MISSING_AUTHORITY_KEY_IDENTIFIER`）；AKI 取 CA 的 SKI，无则由 CA 公钥派生。**只补叶子、不改 CA**——CA 已被用户安装信任，改 CA 内容会使其失效（须重装证书）。
- **工具自身出站请求不走代理**：doubao_session.renew_probe（Rust ureq agent 禁用代理）/ doubao_quota.query_account 显式绕过系统代理直连——巡检无需 MITM，且系统代理开启时会撞上动态证书兼容性问题（本轮 SSL 报错根因）。

## 9.1 代理生命周期约定（v2.4.3）

- `proxy_start` **先**通过 `get_existing_win_proxy()` 读取当前系统代理（即用户的 VPN），作为 `ProxyConfig.upstream` 直传代理模块，**再**用 `set_win_proxy` 改写为 `127.0.0.1:<port>`。顺序不可颠倒，否则会把自己当成上游造成死循环。
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

> **升版时机（红线）**：**不要随意升版**——只有用户明确说「升级版本」时才升版并走全流程（见下文「发版全流程」）。日常提交 / bug 修复一律不动版本号；下表的升位判断标准仅在用户主动发版时用于确定升哪一位。

> **macOS 构建与分发（F-75）**：`tauri.conf.json` 为公共配置，Windows 配置在 `tauri.windows.conf.json`、macOS 在 `tauri.macos.conf.json`（Tauri v2 按目标平台深度合并）。mac 产物为 `dmg` + `app`（aarch64），ad-hoc 签名（`signingIdentity: "-"`，暂不申请 Developer ID）。**Gatekeeper 首装放行引导（Release 正文模板固定附加）**：
>
> > macOS 因应用未经公证会提示「无法验证开发者」，两种放行方式任选：
> > ① 在「应用程序」文件夹中**右键点击**「AI Work 助手」→「打开」→ 再点「打开」（此后不再提示）；
> > ② 或在「终端」执行：`xattr -d com.apple.quarantine "/Applications/AI Work 助手.app"`
>
> 应用内同款引导内置于 AboutDialog 的 mac 更新安装视图（update-installing 事件后渲染）。

| 提交内容 | 升级位 | 示例 |
|---|---|---|
| 新增一个完整的有意义的功能 | **中位（MINOR）** | 3.0.0 → 3.1.0 |
| 修复 bug / 功能优化 / 微小功能新增或调整 | **低位（PATCH）** | 3.1.0 → 3.1.1 |

**升位细则**：

- 大位（MAJOR）仅在重大架构/破坏性变更时升级
- 一个提交包含多类变更时，按最高级别升位；纯文档/注释改动不升级；版本同步提交本身不再升位
- 升版提交执行 `npm run set-version <x.y.z>` 一键同步（底层 `scripts/sync_version.mjs`：package.json / Cargo.toml / Cargo.lock / AGENT.md 标题）；CHANGELOG.md 手动新增条目
- 版本号单一来源为 `src-tauri/Cargo.toml`：tauri.conf.json 不写 version（自动回退），Rust 端 `env!("CARGO_PKG_VERSION")` 自动取，前端关于页运行时经 `getVersion()` 读取（about.ts 不写版本号），NSIS / MSI 安装包版本号自动跟随

**发版全流程**（仅用户明确发版时）：

1. **调整版本**：`npm run set-version <x.y.z>` + CHANGELOG.md 新增条目
2. **编译**：Windows 本机 `npm run tauri build` 产出 setup / msi / portable 三件套；macOS 由 GitHub Actions `build-macos.yml`（push macos_main 触发）产出 aarch64 / x64 / universal 三个 dmg
3. **提交 → 推送 → 打 tag 并推送**
4. **发布 GitHub Release**：上传资产 + `latest.json`，标题见下方格式约定

**Release 资产清单（红线，缺一即更新器 fail-closed）**：

| 平台 | 资产 |
|---|---|
| Windows | `AI Work 助手_<版本>_x64-setup.exe` / `_x64_zh-CN.msi` / `_x64_portable.zip` |
| macOS | `AI Work 助手_<版本>_aarch64.dmg` / `_x64.dmg` / `_universal.dmg` |
| 校验清单 | `latest.json`（**必须收录上述全部 6 个资产**的 SHA-256） |

- **latest.json 单一清单原则**：updater（Windows 与 macOS 同源）下载安装包后与清单比对哈希，清单存在即 fail-closed——缺失 / 损坏 / 版本不符 / **目标资产未收录**任一情况直接阻止自动更新。**只上传单一平台的清单 = 另一平台全部用户自动更新被阻断**（v3.6.1 事故：清单只含 exe/msi，mac dmg 未收录，mac 更新全挂）
- CI 双平台分别生成清单（Windows 落 `release/windows/`、mac 落 `release/mac/`，`rename_release.mjs` 仅同目录自动合并），**发布时必须人工合并为一份全资产清单再上传**；发布前逐项核对 6 资产均在 `assets` 键内
- 清单格式契约（`updater.rs` 消费）：`{ "version": "x.y.z", "assets": { "<本地原始文件名>": "<sha256hex>" } }`，2 空格缩进、中文原样、无末尾换行；键为**纯文件名**（含空格/中文），**严禁带任何本地路径前缀**（v3.6.3 事故：临时脚本把 `d:\code\...` 绝对路径写进键里，路径归一后无法匹配资产名）；GitHub 重写后的资产名（空格→`.`、中文→`_`）由 updater 宽松键归一匹配

**GitHub Release 标题固定格式**：`v{MAJOR}.{MINOR}.{PATCH} 版本发布`（如 `v3.1.1 版本发布`），不额外加描述后缀

## 12. 安全与合规

- **零外发**：不连接任何自有后端。
- **CA 证书**：仅本地回环 `127.0.0.1:8899`，自签根 CA 需 UAC 安装。
- **UAC**：仅在 `cert_install` 提权，切换桥已改为普通用户可运行。
- **API Key**：留空时跳过鉴权；配置时在前端掩码显示（前 4 + 后 4 + ****）。鉴权头支持 `Authorization: Bearer <key>`（OpenAI 风格）与 `x-api-key: <key>`（Anthropic 风格）双风格。
- **凭证展示（黑盒审查修复，2026-09-12）**：列表接口 `AccountView.jwt` 与豆包 `session_id/sid_guard/ttwid` 一律 `mask_secret` 掩码下发；完整值经 `account_get_jwt` / `doubao_account_get_credential` 按需获取。user_id 作路径段统一过 `fs_utils::ensure_uid_safe` 字符集白名单（豆包对话备份/快照/账号删除/桥 -UserId 全覆盖）。
- **导出路径白名单（2026-09-12）**：`write_text_file` 拒绝系统目录（Windows/Program Files）与用户/公共启动文件夹，数据目录外拒绝可执行/脚本类扩展名（.exe/.bat/.ps1 等 16 类），仅放行常规数据导出（.json/.txt/.csv 等）——前端「账号导出」功能不受影响。
- **API 网关**：v2.0 已实现本地 API 网关（axum + ureq），上游 `trae-api-cn.mchost.guru`。端点：`GET /health`（免鉴权）、`GET /status`、`GET /v1/models`（与 `data/api_models.json` 同源，官网同步后无需重启即可见最新列表）、`POST /v1/chat/completions`（OpenAI 协议）、`POST /v1/messages`（Anthropic Messages 协议，F-39）、`POST /v1/responses`（Codex Responses API，F-40 批次4，仅 WB 上游模型）。请求侧统一转 OpenAI 内部格式复用池调度链路，响应侧按协议分别输出；Anthropic 流式事件序列 message_start → content_block_* → message_delta → message_stop，reasoning_content 暂不输出（thinking 块需签名）。账号池 app 无关：Trae / Trae Work 账号入池即被同一网关服务，扣通用积分（product_id 208）。
- **Codex Responses 投影（F-40，批次4）**：`api_server/wb_responses.rs`（7 单测）——请求投影 instructions→system、input items（message/function_call/function_call_output/reasoning 跳过）→ messages、tools 平铺→function 包裹、max_output_tokens→max_tokens、reasoning.effort→reasoning_effort；流式投影在 `wb_sse.rs` `Protocol::Responses` 分支（response.created → output_item.added → output_text.delta → output_item.done → response.completed，错误→response.failed，无 [DONE] 帧）。Codex CLI `~/.codex/config.toml` 直配：`model_provider` 的 `base_url = "http://127.0.0.1:<port>/v1"`、`wire_api = "responses"`。脱敏沿用全局 wb_sanitize 开关与既有审核退回管线。
- **区域路由（F-36，批次4）**：token domain 含 `.workbuddy.ai` → Global 账号，chat 全走 `www.workbuddy.ai`（wb_upstream 双域名常量 + 单测）；billing/积分（credits 三件套）、签到/成长中心（checkin 脚本 `_urls()`）、活动接口（activity_info）、官方用量（usage_official）均按账号区域切换域名；plugin 网关（token refresh）固定 codebuddy.cn 不随区域。
- **批次5 网关增强（T5.2~T5.6/T5.8）**：
  - **四段模型路由（F-61，`wb_model_route.rs`）**：`data/wb_model_route.json`（aliases/rules/suffixes，可手工维护）→ 四端点统一经 `resolve_wb_target` 解析（别名→自定义通配 `*`/`?` →内置系列 claude-*→glm-5.3/gpt-*→deepseek-v4-pro/o1*→hy4→后缀 `-thinking` 注入 effort=high）；映射目标必须目录命中，全未命中回落原名走 SOLO。
  - **默认深度思考（F-62）**：`api_pool.json.wb_default_thinking`（默认关）——客户端未显式请求 effort 且无路由级提示时注入 high；Anthropic 侧 reasoning_content 已映射 thinking block（stream thinking_delta + 非流式前置 block）；OpenAI 侧 reasoning_content 天然透传。
  - **生图双端点（F-63）**：`/v1/images/generations` + `/v1/images/edits`（仅 JSON 变体，image=base64/data URL；OpenAI multipart 不接受）；上游不支持明示 501 不静默；模型需目录声明 `supports_image=true`。
  - **工具代执行（F-64，`wb_toolexec.rs`）**：`/v1/responses` 声明 `web_search` 且 `api_pool.json.wb_tool_exec`（默认开）→ 代理注入 web_search/open_url function + 本地代执行（DDG lite + 页面抓取）→ 回喂循环上限 3 轮；历史轮以 `web_search_call` 输出项返回。**仅代理注入的两工具会被代执行**，客户端真实 function 照常透传。
  - **后台任务降级（F-65③）**：`api_pool.json.wb_bg_downgrade`（默认关）——max_tokens≤128 且全文≤512 字符 → 目录最低倍率模型；`/v1/chat/completions` 收到 `anthropic-version` 头 → 400 明示改走 `/v1/messages`。
  - **本地 quota 兜底（F-21，`wb_common.rs`）**：credits 云端全链失败 → 扫 `~/.workbuddy/*.port` + 候选/有界端口段 → GET `/api/v1/quota` 按 remaining 特征确认（source=`local_quota`）。
  - **DSH provider 目录动态替换（F-37，`wb_catalog.rs`）**：`parse_upstream_catalog` 宽容解析（三容器形态/字段链探测/产出 0 条不落盘）+ `fetch_and_replace`（GET `{chatBase}/console/enterprises/personal/models`）；网关启动自动一次（失败保持静态兜底）+ `api_wb_catalog_sync` 手动命令；`/v1/models` WB 条目透传 `supports_image`/`supported_efforts`。
  - **CC Switch 协同（F-43，`commands/ccswitch.rs`）**：不自建切换器——upsert 固定 id `aiwork-gateway-<claude|codex>` 进 `~/.cc-switch/cc-switch.db` providers 表（claude=扁平 env，base 不带 /v1；codex=auth+config.toml，wire_api=responses）；写前整库备份至 `~/.cc-switch/backups/`、**只动自有条目**、Key 不入日志；CC Switch 运行中写入后需重启其生效。

## 13. 禁止与红线（Do NOT）

- ❌ 修改 Rust 命令嵌套参数（如 `CheckinOpts`）的字段名 → 前端 invoke 载荷与 `--task-run` CLI 序列化均按字段名匹配，改名即断链。
- ❌ 把 Rust 命令顶层参数改为 camelCase → Tauri 用 Rust 函数签名原名匹配。
- ❌ 引入 React Router / Redux / 额外 UI 库 → 保持依赖最小。
- ❌ 提交 `.workbuddy/`、`dist/`、`node_modules/`、`src-tauri/target/`、`__pycache__/`、`data/`（已在 `.gitignore`）。
- ❌ 使用 `window.confirm()` → Tauri WebView 不支持，用自定义 Modal。
- ❌ 使用 `api.prevent_close()` → 会导致 Chromium 1412 错误。
- ❌ 用 `npm run dev` 直接跑 Vite → 白屏，必须 `npm run tauri dev`（实际入口为 `scripts/dev-tauri.mjs`）。
- ❌ 混用 dc uid 与 Cloud-IDE uid 入池 → 两套 id 空间不通用，会产生重复账号（见 §5.1）。

## 14. 已知约束

- **平台支持（F-75）**：Windows 10/11（完整功能）+ macOS 12+（Apple Silicon / Intel，dmg 分发；功能域灰度放开中——schtasks 注册 / MachineGuid 重置 / UI 点击兜底为 Windows 专属，mac 由内置调度器 + 开机自启覆盖；平台差异实现收敛于 `platform/` 模块）。
- **PowerShell 运行时依赖已移除**：切换/保存/备份/恢复/保活全链路由 `switcher` 模块进程内直调（仅 Windows，sysinfo 0.33 锁定版——0.38+ 需 rustc 1.88 超出项目 MSRV 1.85）。
- **环境检测零子进程（2026-10-07）**：版本号直读 PE 版本资源（`pe_version.rs`，按 mtime+size 缓存，读不到才回退 powershell）、运行态走 `switcher::proc::any_running`（sysinfo 快照 + 2s TTL）、`app_locate` 兜底两级（注册表/进程）带 3s TTL——切换板块时概览页与顶栏会重复探测，缓存即为此设计。**注意：关闭/强杀轮询（`commands/process.rs`、`switcher::proc::list_procs`）需实时快照，勿接入上述缓存。**
- **Buddy 积分 SWR 语义（2026-10-08）**：`workbuddy_credits_fetch` 在非 `fresh` 且缓存过期时返回旧值 + `refreshing:true`，刷新转入后台线程（同刻仅一轮，日志 `[wb-credits] 后台刷新完成…`）；`stale:true` 仍**专指** F-59「刷新失败回退」，两者勿混用；前端按 `refreshing` 决定是否重取（`BuddyOverview`）。
- `profiles_dir` 路径为 `data_dir.join("data").join("profiles")`，注意 `data/` 子目录。
- LLM API 上游必须设置 `NO_PROXY=*` 避免系统代理循环。
- 日志文件首行可能有 BOM 前缀（PowerShell 5.1 `-Encoding UTF8`），`split_time` 已处理。
- JWT 默认 13 天过期；带 refresh_token 的账号可自动续期。
- **`schtasks` 中文输出是 GBK**，直接 `String::from_utf8_lossy` 会乱码。统一走 `misc.rs::run_schtasks()`（前置 `chcp 65001`），**不要**再裸调 `Command::new("schtasks")`。
- **计划任务不加 `/RL HIGHEST`**：签到任务只读写 `%APPDATA%`（Rust 直调，无子进程提权需求），加了会让普通用户注册失败（Access Denied）。
- **错误文案不重复加前缀**：Rust 端返回纯错误描述，`查询失败：` / `注册失败：` 等前缀由前端 `Settings.tsx` 统一拼接。
- **进程三级关闭策略（F-47，process.rs）**：优雅关闭（taskkill 不带 /F 发 WM_CLOSE，等 3s 让 Electron 正常落盘）→ 树杀（/T /F，等 2s）→ 仍存活则返回 Err 由前端提示人工介入。仅按主程序映像名精确匹配；所有子进程以 CREATE_NO_WINDOW 拉起。
- **API 模型同步**：官网同步重放 Trae 客户端 `batch_get_detail_param` 配置接口；内置模型 glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code 不在配置接口响应中，需经 llm_utils_chat 以 `function=solo_agent` 调用补齐。
- **品牌迁移（v3.0.0）**：identifier `com.traework.assistant`→`com.aiwork.assistant`，数据目录 `%APPDATA%\TraeWorkAssistant`→`AIWorkAssistant`（`state.rs::migrate_legacy_dirs` 启动时**复制**迁移——旧目录原地保留，老应用可继续使用、两版并存；新目录已有数据则跳过；含 WebView2 目录，排除 Cache/GPUCache 等 8 类缓存子目录，复制失败回滚半成品），计划任务由 `misc.rs::try_migrate_legacy_task` 按旧触发时间重建（**旧任务保留**，`task_unregister` 只删新任务）。环境变量统一为 `AIWORKDATA_DIR`。
- **老安装包升级**：升级兼容按**安装时产品名**判定（非版本号）。NSIS 通过 `build-assets/installer-hooks.nsh` 静默卸载清理旧品牌「Trae Work 助手」安装（已发布的 v2.4.4 及更早均属旧品牌，UTF-8 with BOM）；「AI Work 助手」品牌（v3.0.0 起）走 NSIS 原生原地升级；老 MSI 因 UpgradeCode 随 identifier 变化无法原地升级，需先卸载或改用 NSIS 包升级。打包产物统一输出到 `release/`，使用中文产品名命名 `AI Work 助手_<版本>_x64*`（`scripts/rename_release.mjs`）。
- **版本线与数据迁移**：新版本自 v3.0.0 起，**之前所有 2.x 版本升级到 3.x 均需数据迁移（安装/首次启动自动完成）**；原「Trae Work 助手」产品线在 `trae_work_main` 分支维护（仅 Trae Work 单应用，2.x.x，仅必要修复），仅使用 Trae Work 的用户可不升级，用该分支的 v2.x.x 最新版本即可。
- **分支矩阵**：`main` = Windows 主线（默认分支）；`macos_main` = **macOS 产品分支**（F-75 平台支持产品化落地于此——platform 服务层 + 平台门控 + dmg 构建流水线，`build-macos.yml` push 触发产出 aarch64/x64/universal 三 dmg，与 main 保持合并对齐、随 3.6.x 同步发版）；`docker_main` = **Docker 简化分支**（Web-only 单体 `aiwork-server`：管理 REST + OpenAI 网关 + 调度器 + 浏览器 UI，裁剪桌面壳/MITM/账号切换/豆包，独立 1.x 版本线，`docker compose up` 部署）。
- **NSIS 安装器**：使用自定义模板 `build-assets/installer.nsi`（基于 tauri v2.11.4 上游模板，配置于 tauri.conf.json `bundle.windows.nsis.template`）——升级安装时跳过「卸载旧版/不卸载」选择页，**默认直接覆盖安装**（同版本重装/降级仍显示选择页）。升级 Tauri CLI 后如构建报错，需从对应版本 tag 的 `crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi` 重新同步模板并重做定制。

## 15. Git 引用静默丢失坑（2026-09-13 事故复盘，每次提交必守）

> 环境：WorkBuddy 便携版 Git。**嵌套分支 ref**（如 `refs/heads/feature/buddy`）若仅存于 `packed-refs`（loose 文件已被收编、`.git/refs/heads/feature/` 目录不存在），则 `git commit` 会**报成功但分支指针静默回滚**到 packed-refs 旧值——提交对象与 reflog 均正常入库，唯 ref 落盘丢失。后果：下一次提交挂在旧父节点上，本次提交沦为孤儿，历史链断裂（实例：e14ee2d 孤儿 + 6c1fdca 错父）。顶层 ref（`refs/heads/master`）写入正常，仅嵌套 ref 触发。

**规避规范（红线，逐条必守）**：

1. **提交后必校验**：每次 `git commit` 后立刻比对 `git rev-parse HEAD` 与 `git rev-parse <当前分支>`（或 `git reflog -1` vs 分支 ref），两者不一致 = 指针回滚，立即按第 4 条修复后再继续。
2. **提交前查 loose 状态**：目标分支若在 packed-refs 中且对应 loose 文件不存在（`.git/refs/heads/<路径>` 缺目录），提交风险最高；可用 `git for-each-ref` / Python 检查。条件允许时优先在顶层分支（master）或确保 loose ref 存在的分支上操作。
3. **整树提交，不用 pathspec 部分提交**：统一 `git add <paths>` + `git commit`（不带 `-- <paths>` 后缀）。部分提交在本环境下让 ref 回滚的破坏面更难诊断。
4. **修复只走 packed-refs 原位替换**：`git update-ref` 与直写 loose 文件在本环境均会被 git 进程静默丢弃，**唯一可靠手段**是用 Python 原位替换 `packed-refs` 中该分支行（读入→按行尾匹配 `refs/heads/<path>` 替换哈希→整体回写，保持排序）。每次替换后用 `git rev-parse` 读回验证。
5. **孤儿提交勿清理**：`git gc` / `git prune` 一律不跑，dangling 提交（如 6c1fdca）是无害保险，误删不可逆。
6. **慎用 `git pack-refs --all`**：它会把 loose ref 收编进 packed-refs，正是制造本坑的前提条件；本仓库避免执行。
7. **关联环境故障**：同日 bash `rm` shim 损坏曾误删 docs/（已恢复）。删除文件一律用 Python `os.remove`，禁用裸 `rm`；修复类操作前先 `git status` 快照留证。

## 16. 远端同步规范（新任务开工前 / 提交推送前必守）

> 多端协作下本地极易落后远端（他端已推的修复 / tag / 新分支本地不可见），基于陈旧基线开发会产生无谓冲突或重复修复。**以下两个时机必须先同步、后动作**：

1. **开始新的需求任务前**：`git fetch --all --tags --prune` 同步全量远端引用（所有分支 / tags / 提交），当前分支落后时 `git pull` 到最新再开工。
2. **git 提交和推送前**：同样先 `fetch --all --tags --prune`，用 `git status -sb` 比对 ahead/behind，落后先 `git pull`（优先 fast-forward）再提交推送；推送被拒（non-fast-forward）时**禁止 `--force`**，先排查远端新增提交内容。

**红线**：同步必须落在动作**前**（不是想起来再补）；`--prune` 必带（清理远端已删分支的本地幽灵引用）；pull 出现冲突时按 §交互规范 跟踪冲突块双方意图，不盲提 merge commit。
