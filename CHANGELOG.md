# 更新日志

本文件记录 AI Work 助手（ai-work-assistant，原 Trae Work Assistant）的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

---

## [3.3.5] - 2026-09-11

### 新增

- **TRAE 切换前会话预检**：切换账号前探活目标账号 JWT，已被服务端吊销则中止切换并指引「重新登录该账号并保存」，不再白切一次；续期 JWT 流程自动跳过预检（Refs #9）
- **TRAE 切换恢复后校验**：恢复完成校验关键登录态文件，快照为空/损坏或疑似 TRAE 新版布局漂移时自动回滚到切换前状态并明确报错，不再静默无效切换（Refs #9）

### 变更

- **应用选择大块布局**：切换账号/保存登录的 Trae CN 与 TRAE SOLO CN 选择改为左右两块大按钮（悬停高亮），目标区域大、不易点错
- **一键签到进度全集展示**：进度列表显示参与签到范围内的全部账号，被跳过的账号明示原因（已签/JWT 过期/冷却中），多次执行行序稳定、上轮结果保留

### 修复

- **签到失败精确指引**：JWT 被服务端吊销的账号明确提示重新登录并保存（不再笼统报「失败」），且自动跳过重试避免无效等待（Refs #9）
- **「今日已签」被覆盖丢失**：签到结果摘要改为同日合并，第二轮起被跳过的已签账号不再退回「未签」

## [3.3.4] - 2026-09-11

### 修复

- **API 服务「鉴权开关」按钮文案与实际动作相反**：关闭态误显示「关闭鉴权」、开启态误显示「开启鉴权」。现关闭态显示「开启鉴权」、开启态显示「关闭鉴权」，按钮配色改为跟随动作（开启=绿、关闭=琥珀）
- **一键签到空轮提示一闪而过**：全部账号已签/过期/冷却中时，「实时进度」卡片随 done 事件整体消失，用户无从得知原因。现空轮结束后卡片与徽章持续保留（下次签到自动重置），并新增常驻说明行告知无候选账号的具体原因

## [3.3.3] - 2026-09-11

### 修复（代码审查两项 + 吸收 trae_work_main 分支修复 81f8b20）

- **API 服务鉴权默认拒绝**：未配置任何启用 Key 时请求默认拒绝（401 JSON，含创建 Key 引导文案），不再无鉴权放行本机任意进程消耗上游额度；新增显式「鉴权开关」（关闭鉴权后放行并记 anonymous，UI 标注不推荐），改动立即生效
- **更新包完整性校验**：新增 `latest.json` 发布校验清单（`rename_release.py` 生成，发布时随 Release 上传），`update_check` 清单优先且 fail-closed（清单缺失/损坏/版本不符/未收录资产均阻止自动更新并引导手动下载）；无清单回退 release 正文约定行，两者皆无跳过（兼容存量 release）；下载完成后强制比对 SHA256，不匹配即删除并中止；`relaxed_key` 归一修复 GitHub 资产名重写（空格转点）导致正文校验永不命中的缺陷
- **日志轮询防风暴**：上一轮未返回跳过本轮（防 2s 轮询堆积）；失败 toast 60s 限频（手动刷新直通）；并发序号「最新请求胜出」，手动刷新与轮询并发时旧响应不再乱序覆盖
- 测试补齐：`api_keys` 测试补 `auth_disabled` 字段（`cargo check` 不编译测试代码曾漏检，教训已入 AGENT 记忆）

## [3.3.2] - 2026-09-10

### 修复

- **关闭应用卡死**：点击关闭/托盘退出后窗口冻结数秒才退出——根因为 `exit(0)` 内部 WebView2 窗口销毁在主线程同步执行（运行越久越明显），且 `RunEvent::Exit` 清理日志从未落盘证实卡点。改为 hide-then-exit：先隐藏窗口再退出，窗口立即消失，后台清理再慢也无感；窗口关闭与托盘「退出」两条路径统一处理
- **一键签到「点了没反应」**：无账号时签到按钮为禁用态且 `start()` 静默返回，双重无反馈。现无账号时整页显示引导空态（含「前往账号管理添加」跳转按钮）；按钮恢复可点击，按场景精确提示（暂无账号 / 所选分组无账号 / 未勾选账号）

### 优化

- 静默反馈排查收尾：账号池「刷新」按钮失败时给出错误提示（此前 loading 结束无任何反馈）；日志页空日志点击「复制」时提示「暂无日志可复制」（此前静默返回）

## [3.3.1] - 2026-09-10

### 修复

- **证书安装（#6）**：cert_install 前置依赖自检并自动 pip 自愈；gen_ca 捕获子进程真实报错（含 `No module named` 修复指引）；certutil 路径加引号防空格拆参、`-PassThru` 取真实退出码；安装后复查根存储；前端 handleRun 兜底 toast，消除「点了没反应」
- **代理生命周期（#7）**：`SO_EXCLUSIVEADDRUSE` 独占绑定，端口被占时带占用 PID 诊断并退出（不再「假启动」）；`log()` print 纳入容错；CONNECT 先握手后记日志；proxy_start 端口预检 + 清理遗留孤儿进程；python 子进程统一挂 Job Object（kill-on-close）防崩溃/强杀泄漏

### 新增

- 安装/portable 包内置精简 Python 3.12 运行时 + cryptography/pywin32，不再依赖系统 Python（新增 `scripts/prepare_python_runtime.py`）
- 运行时精简：剔除 pywin32 自带 IDE/COM 扩展/帮助文档等无用项（净减 ~10.7MB 未压缩），保留 dist-info 与 `__pycache__`

## [3.3.0] - 2026-09-10

### 新增（T1-T11 自 trae_work_main 手工移植合入，未使用 merge/cherry-pick）

- **API 网关用量统计（T1）**：`data/api_usage.json` 按日落盘（模型 / 上游账号 / API Key / 流式 / 成败 / 耗时 / token 多维聚合，保留 90 天）；`api_usage_stats(days)` 命令 + API 服务页「用量统计」面板（StatCard ×4 + 堆叠柱状图 + 模型分布 Top5 表）。
- **多 API Key + 每日配额（T2，含 T15 调整）**：`data/api_keys.json` 多 Key 独立签发与每日限额（0=不限），超配额返回 429；移除主 API Key 双轨校验，鉴权统一走 Key 列表（未配置启用 Key 时不鉴权，携带未知 Key 放行记 anonymous）；前端「API Keys 管理」卡片（生成 / 启停 / 限额失焦保存 / 删除 / 复制）。
- **托盘菜单增强（T3）**：托盘「立即签到」与「启动/停止 API 服务」（动态文本）+ 完成系统通知；与页面操作共用防重入锁，不冲突。
- **敏感数据加密（T4）**：jwt / refresh_token 迁入 Stronghold vault（`conf/vault.stronghold`），主密码经 Windows DPAPI 保护（`conf/vault_key.bin`，仅本机当前用户可解）；JSON 落盘占位化，读写统一 `vault::load_accounts / save_accounts`（vault 写失败降级明文 + 下次启动重试迁移）；Python 签到脚本走临时解密文件（`--accounts-file`，用后即删 + 启动按前缀清理崩溃残留）。
- **签到失败自动重试（T5）**：最多 2 轮（间隔 30s / 90s）仅重试失败账号；`CheckinGuard`（tokio::sync::Mutex）应用级防重入，页面 / 托盘 / 静默签到共用；per-uid 最终态合并保证 `ok+already+failed==total`；前端重试倒计时横幅。
- **日志按类型清理（T6）**：`logs_clear(log_type)` 命令（all/proxy/checkin/switch），日志页「清理」按钮 + 确认弹窗；不做导出（明确排除）。
- **前端测试基建（T7）**：抽取 `src/lib/format.ts`（maskApiKey / fmtTokens），新增 vitest@^2（`npm run test`），cn / delay / format 共 13 个纯函数用例。
- **签到成功率趋势（T8）**：`data/checkin_results.json` 按日 per-uid 落库最终态（保留 90 天）；`checkin_trends(days)` 命令 + Dashboard「近 30 天签到结果」堆叠柱状图（成功绿 / 已签蓝 / 失败红）。
- **OpenAI 兼容端点扩展（T9）**：新增 `POST /v1/completions`（legacy text completion，prompt 转 user message 复用现有链路，流式 / 非流式）；`/v1/embeddings` 明确返回 501（不做假实现）。
- **账号池调度策略 + 分组筛选（T10）**：`api_pool.json` 扩展 `strategy`（expire_first / credit_first / random）与 `group_ids`（空=全部参与）；策略纯函数化 + 分组同步期过滤；前端策略下拉 + 分组多选 + 实时预览（分组外账号置灰标记）。
- **开机自启 + 启动静默签到（T11）**：`tauri-plugin-autostart`（开关即时生效）+ 启动延迟 60s 对未签到账号自动签到（复用统一签到链路，skip_checked_in=true 幂等，完成发系统通知）。

### 变更

- 高版本适配：RawAccount 新增 `dc_id` 字段的构造点补齐（pool.rs 单测等）；自启 / 静默签到开关落在 `GeneralSettingsPanel.tsx`（高版本设置面板）；`trae_apps.rs` 接入 vault（高版本无 pay_status.rs / trae_local.rs）。
- `Settings` 移除 `api_key` 字段（serde 默认忽略旧配置残留字段，该 Key 不再参与鉴权）。
- `pool_set` 扩展 `strategy` / `group_ids` 参数；`models_sync::fetch_official` 签名改为调用方预解密账号；`/v1/completions`、`/v1/embeddings` 路由注册。
- `Cargo.toml` 新增 `tauri-plugin-stronghold`、`tauri-plugin-autostart`、windows-sys（DPAPI）与 `[profile.dev.package."*"] opt-level = 2`；`package.json` 新增 `vitest@^2.1.9` 与 `test` script。

### 修复

- **积分 0 显示为 "-0"**：账号管理悬停明细（通用/Work 积分与积分包）、积分看板统计卡/趋势 tooltip/明细表、签到页积分列——JS `(-0).toLocaleString()` 输出 "-0" 且 `JSON.parse("-0.0")` 得负零，Rust `(v*100).round()/100` 舍入亦保留负零。前端新增共享 `normZero`/`fmtCredits` 在全部积分展示点归一化；后端 `calc_remaining_credits` / `fetch_credit_detail` 的 r2 舍入结果归一为 +0.0，落盘与 IPC 不再出现 "-0.0"。

### 文档

- 新增 `docs/optimization-implementation.md`（T1-T11 需求 / 价值 / 实现逻辑 / 代码参考，按本分支适配）与 `docs/optimization-plan.md`；AGENT.md 命令契约表、api-doc.md 新增命令章节与过时签名同步修正。

### 验证

- `cargo test` 通过、`npx tsc --noEmit` 通过、`npm run test`（vitest）18/18（含 normZero/fmtCredits 回归用例）、`npx vite build` 成功。

## [3.2.7] - 2026-09-08

### 新增

- **安装位置自动识别（F-01）**：新增 `app_locate(target_app)` 命令——手动指定 → 注册表卸载键 → 默认路径 → 运行进程反查 四级探测，统一返回 `{exe, userDataDir, version, source}`；档案表驱动四应用（Trae Work / Trae CN / 豆包 / WorkBuddy），设置页「自动检测」按钮改走该命令。
- **响应宽容解析（F-49）**：`fs_utils::dig()` 沿 data/result/resp/response/info 包裹键递归下钻（限深 8 层），采纳到积分包、套餐身份、ExchangeToken 三处解析点，抗官方信封字段变动。
- **账号导入预览（F-46 残余）**：新增 `accounts_import_preview` 命令（解析文件、标记已存在账号与待新增分组，不写盘）；导入改两步式——选文件 → 预览弹框逐条勾选 → 按索引导入。
- **快照桥扩展（F-48 部分）**：PS 桥档案表扩至四应用（`-TargetApp TraeWork|Trae|Doubao|WorkBuddy`），引入 `SnapshotLayout` 布局标记，非 icube 布局的快照/设备重置显式报错待各应用批次接入。

### 修复

- **导入预览弹框必崩**：`ImportPreview` / `ImportPreviewAccount` / `AppLocate` 三个新 DTO 误加 serde `camelCase` 改名，与项目蛇形命名约定及前端失配（`new_groups` 等字段为 undefined），渲染时抛 TypeError——移除改名恢复蛇形上线。
- **分组默认名多字节 panic**：分组 id 含多字节字符时 `&id[..4]` 字节切片越界 panic，改按字符截断；账号 uid 尾部摘要同类隐患一并修复。
- **悬停积分明细 / 刷新积分 / 刷新套餐 / 扫描本机账号冻结 UI**：`fetch_credit_detail`、`fetch_remaining_credits`、`refresh_remaining_credits`、`refresh_pay_status`、`apps_accounts_discover` 五条同步网络命令改 `#[tauri::command(async)]`。
- **套餐刷新静默降级 Free**：`query_pay_status` 对 2xx 但缺关键字段的异常响应返回错误，不再用 "Free" 覆盖缓存中的正确套餐。
- **卸载「删除应用数据」删不干净**：NSIS 卸载钩子补删 `%APPDATA%\AIWorkAssistant`（含旧 TraeWorkAssistant 遗留），此前只删 WebView2 的 identifier 目录。
- **并发写 JSON 踩踏临时文件**：`write_json` 临时文件名加 pid+纳秒唯一化；模型列表损坏时记日志 + `.bak` 备份 + 默认配置自愈，不再静默覆盖。
- **更新器残留与路径防御**：下载前清理 `%TEMP%` 旧版本安装包残留；拒绝含路径分隔符 / `..` 的资产名（防临时目录逃逸）。
- **积分明细口径不一**：无 `expire_time` 的积分包计入明细（显示「长期有效」），与统计口径对齐；明细失败后下次悬停可重试。
- **导入 / 入池交互**：导入空文件明确提示且按钮加载态防重复点击；发现账号入池按钮 per-item pending；更新包大小为 0 时显示「未知大小」而非 0 B。
- **安装器边缘**：被动模式（/P）同/升级直接覆盖安装、降级遵循 ALLOWDOWNGRADES，不再误触发旧版卸载器；`rename_release.py` 单产物缺失输出 WARNING 并支持 `--strict`。

### 文档

- AGENT.md 对照 v3.2.6 代码库校准（目录地图、命令契约表、双 uid 红线、主题约定）；`future-roadmap.md` 升 v1.3（F-01/F-03/F-12/F-23/F-46 残余/F-48/F-49 移入已完成）。

---

## [3.2.6] - 2026-09-08

### 修复

- **官网模型同步必然失败**：`fetch_official` 误从数据根目录读取 `checkin_accounts.json` / `device_map.json`（实际位于 `data/` 子目录），导致恒报「没有可用账号（缺少 JWT）」；同时 `api_models.json` 统一移至 `data/` 子目录（与 `AppState::path()` 路由一致），旧位置自动兼容迁移。
- **`/v1/models` 与应用配置不一致**：原返回硬编码静态列表（缺 glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code 三个内置模型），改为与 `api_models.json` 同源——官网同步后无需重启 API 服务即可通过 `/v1/models` 看到最新列表。
- **上游中文错误响应可致 panic**：日志预览 `safe_slice` 按字节截断，第 200 字节落在 UTF-8 多字节字符中间（中文错误 JSON 常见）即 panic——流式客户端收到截断空流、非流式返回 500。改为按字符边界截断，账号 uid 摘要同步安全化。
- **环境检测 / 打开 Trae 应用期间 UI 卡顿**：`env_check`（注册表全量搜索可达数秒 + PowerShell 取版本）、`open_trae_app`（代理注入时进程优雅关闭最长 5s 轮询）、`task_*`（schtasks 子进程调用）均为同步命令在主线程执行，全部改 `#[tauri::command(async)]` 派发线程池（与 3.2.5 updater 冻结修复同因）。
- **OpenAI 端点对无效 JSON 请求返回 502**：原 `unwrap_or(json!({}))` 后仍转发原始 body 到上游；现校验失败直接返回 400 `invalid_request_error`（与 `/v1/messages` 行为对齐）。
- **错误分类误冷却**：`classify_solo_error` 宽泛匹配 `contains("plan")`，"planned maintenance" 等消息会被误判 PlanLimit 导致账号冷却 12 小时；收紧为 code 1005 / `plan limit` / `额度用尽` / `套餐额度` 精确匹配。
- **流式上游 Agent 无读超时**：连接建立后若上游不发数据，请求与线程永久挂起；增加 300s 读超时（正常 SSE token 间隔远小于此），并修正与实现不符的 `response_header_timeout` 注释。
- **`read_text_file` 无大小限制**：增加 10MB 上限与常规文件校验，防误选超大文件拖垮前端。
- `models.rs` 两处 GBK 乱码注释修复。

### 变更

- 移除进程级 `NO_PROXY` 环境变量设置：项目未启用 ureq 的 `proxy-from-env` feature，Agent 默认直连不读代理环境变量，原设置本就无效（删除无行为变化）；updater「直连」通道语义同步澄清。
- **品牌迁移健壮性增强**：目录复制失败时回滚半成品（避免下次启动误判「已迁移」导致文件缺失）；WebView2 用户数据目录迁移排除 Cache / GPUCache / Crashpad 等 8 类缓存子目录（体积可达 GB 级且常被运行中的老应用锁定，新版本首启自动重建）；数据已迁移后静默跳过，不再每次启动输出提示。

### 安装器

- **不再静默卸载老品牌「Trae Work 助手」**：原 NSIS 安装钩子在安装/升级时会执行老品牌卸载器并清理其安装目录、注册表卸载键与快捷方式（且强杀老品牌进程）；现全部移除——两版并存、互不干扰，用户数据目录复制迁移逻辑不变（老版本数据原地保留）。

### 文案

- API 网关描述统一为「OpenAI / Anthropic 兼容接口」：API 服务页页头、关于页简介、README、用户手册。

---

## [3.2.5] - 2026-09-08

### 新增

- **模型列表配置化与官网同步**（自 2.x v2.8.0 移植）：新增 `models_sync` 模块——模型下拉列表持久化在 `api_models.json`（缺失时写入内置默认 18 个模型）；ApiService 页新增「同步官网模型」按钮，重放 Trae 客户端 `batch_get_detail_param` 配置接口获取权威列表（不消耗积分，自动跳过内部/隐藏模型并按默认顺序归位）；`payload` 模型映射表同步扩充至官网最新（新增 Doubao-Seed-Evolving / Doubao-Seed-Code / DeepSeek-V4-Pro-Official / kimi-k2.6 / kimi-k3 / qwen3.8-flash / qwen3.8-max 等）；实测 glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code 仅在 `function=solo_agent` 下可用，改为按模型分发 function；`/v1/models` 静态列表同步更新（移除已下线的 sagitta / aquila / doubao-seed-2.0-code / glm-5 / glm-5-turbo）。
- **单实例防护**：重复启动应用（双击/自启动后再点）时，不再产生第二个进程——已有实例的主窗口自动还原、显示并聚焦，新进程自动退出。基于官方 `tauri-plugin-single-instance` 实现，仅正式版启用（dev 模式与已安装版共用 identifier，启用会互相顶替干扰调试）。

### 修复

- **检查更新「下载安装包」国内网络直连超时（os error 10060）**：`update_check` 请求的 `api.github.com` 可直连，但下载 302 跳转到 `objects.githubusercontent.com`（GitHub 下载 CDN）直连不通，而下载此前只认环境变量代理、不认系统代理（VPN）。改为逐通道尝试：**系统代理（注册表，即用户 VPN）→ 环境变量代理 → 直连**，任一通道成功即完成；检查更新同样受益。另修复下载误设 30s 整体超时（慢网络下必被掐断），改为连接 10s + 读 60s、不设整体超时。
- **检查更新期间整个应用卡死**：`update_check` / `update_download` 为同步命令，Tauri 默认在主线程执行，网络重试最坏 90 秒会冻住 UI。改为 `#[tauri::command(async)]` 派发到异步运行时线程池，主线程不再阻塞（窗口/按钮全程可响应）。

---

## [3.2.3] - 2026-09-08

### 修复

- **签到必崩 `Failed to import encodings module`（v3.2.0 – v3.2.2 全部受影响）**：v3.2.0 起的安装包/便携包把 `src-python/` 内混入的一套残缺 Python 3.13 运行时（缺 `Lib/encodings`）打进了 `resources/python/`，而应用优先使用内嵌解释器 → 升级后签到必崩。两步修复：移除残缺运行时；`state.rs` 内嵌与系统解释器探测全部改用 `import encodings` 自举验证（原 `--version` 探测不触发 stdlib 导入，残缺运行时也能通过），残缺内嵌解释器自动回退系统 Python——已安装 3.2.x 的机器升级本版后立即恢复，无需手动清理残留文件。

---

## [3.2.2] - 2026-09-07

### 修复

- **检查更新下载步骤必报错（3.1.0 / 3.2.x 全部受影响）**：前端 invoke `update_download` 时参数 key 传了 `version`，而 Tauri 2 顶层参数按驼峰匹配 Rust 参数 `expected_version` → `expectedVersion`，导致「下载更新包」必报 `missing required key expectedVersion`（v3.1.0 的 `update_install` 同一问题，应用内更新从未成功）。修正为 `expectedVersion`。

---

## [3.2.1] - 2026-09-07

### 修复

- **检查更新只匹配本产品线（≥ 3.0.0）**：同一仓库同时发布 2.x（Trae Work 助手）与 3.x（AI Work 助手）两条产品线、是两个不同产品，`releases/latest` 会指向最近发布的那条线。改为拉取 releases 列表（跳过 draft / prerelease），只认 ≥ 3.0.0 的 release 并取版本最高者；`release_page` 改指具体 release 页；发布页链接改为 `…/releases`（不再用 `/latest`）。

---

## [3.2.0] - 2026-09-07

### 变更

- **应用内更新改为两步确认制**：检查更新发现新版本后不再自动下载安装，拆分为「下载」与「安装」两个独立步骤，UI 各有一次确认——确认一下载更新包（带进度条），确认二「立即安装并重启」。
- **安装器启动参数 `/S` → `/P /UPDATE /R`**：`/P` 被动模式（安装进度条可见）、`/UPDATE` 跳过卸载直接覆盖安装、`/R` 安装完成后自动重启应用（自定义 NSIS 模板已支持）。`update_run_installer` 增加路径校验：仅允许运行临时更新目录内的安装包且版本须大于当前版本。
- **NSIS 安装包升级体验优化**：升级安装（检测到旧版本）时不再弹出「卸载后安装 / 不卸载直接安装」选择页（原默认推荐先卸载），改为**跳过该页直接覆盖安装**；同版本重装与降级仍显示选择页。实现方式：新增自定义 NSIS 模板 `build-assets/installer.nsi`（基于 tauri v2.11.4 上游模板定制），经 `tauri.conf.json` 的 `bundle.windows.nsis.template` 启用。
- **版本号收敛为单源**：单一来源 = `src-tauri/Cargo.toml`。
  - `tauri.conf.json` 移除 `version` 字段（Tauri 自动回读 Cargo.toml）；
  - 关于页版本号改为运行时 `getVersion()` 读取，移除 `about.ts` 中的 `APP_VERSION` 硬编码；
  - 新增 `scripts/sync_version.py`：一条命令把版本同步到 package.json / AGENT.md 标题 / Cargo.lock；
  - `rename_release.py` / `package_portable.py` 版本读取改为 Cargo.toml 回退。

### 新增

- **API 服务新增 Anthropic 兼容端点 `POST /v1/messages`**（F-39「+Anthropic 适配」落地）：
  - 请求侧 `payload::anthropic_to_openai` 将 Anthropic Messages 请求（system / text blocks / tool_use / tool_result / tools / tool_choice）转换为 OpenAI 内部格式，复用既有账号池调度与 llm_utils_chat 链路；
  - 响应侧 `sse::stream_convert_anthropic` / `aggregate_anthropic` 输出 Anthropic 协议（流式 message_start → content_block_start/delta/stop → message_delta → message_stop 事件序列，支持 tool_use 块；非流式 message 对象含 usage）；
  - 鉴权支持 `x-api-key`（Anthropic 风格）与 `Authorization: Bearer`（OpenAI 风格）双风格；
  - 单元测试覆盖 text 与 tool 往返转换（`cargo test` 2 项通过）。
- API 服务页「使用方式 & 配置示例」补充 Anthropic 端点说明与 /v1/messages cURL 测试示例。

### 文档

- `docs/future-roadmap.md`：F-39「Trae API 暴露」标记完成并从待办排序移除（核心能力随 v3.1.0 网关 + F-08 双应用发现天然达成，本次补齐 Anthropic 适配）。
- `AGENT.md`：API 网关模块结构与端点契约同步（/v1/messages、双风格鉴权、账号池 app 无关说明）。

### 排查

- `src-ps/trae-switch-bridge.ps1` 编码排查：文件头已含 UTF-8 BOM（EF BB BF），不存在 PowerShell 5.1 按 GBK 误读问题，无需调整。

---

## [3.1.0] - 2026-09-07

API 服务页界面微调。

### 变更

- **API 服务页**：删除「使用方式 & 配置示例」标题右侧的「通用积分」徽章文字。
- **API 服务页**：积分体系说明面板中「当前全部账号通用积分总余额」改为靠右展示（`ml-auto`，空间不足自动换行并保持右对齐），去掉中间「·」分隔符。
- 版本号 3.0.0 → **3.1.0**（`package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` / `about.ts` 同步）。

---

## [3.0.0] - 2026-09-07

品牌定位迁移：产品更名为 **AI Work 助手（ai-work-assistant）**，面向多个 work 工具提供功能支持；同时清理全部旧品牌痕迹并保证老应用升级兼容。

### 变更

- **品牌统一**：代码注释、界面文案、README、AGENT.md、docs 全部文档由 Trae Work Assistant / trae-work-assistant 统一为 AI Work 助手 / ai-work-assistant。
- **打包标识**：identifier `com.traework.assistant` → `com.aiwork.assistant`，`mainBinaryName` → `ai-work-assistant`（主程序 ai-work-assistant.exe），Cargo 包名与 package.json 同步；新增 `scripts/rename_release.py` 将安装包统一输出到 `release/`，产物使用中文产品名命名（如 `AI Work 助手_3.0.0_x64-setup.exe` / `AI Work 助手_3.0.0_x64_zh-CN.msi` / `AI Work 助手_3.0.0_x64_portable.zip`）。
- **老应用升级兼容（NSIS）**：新增 `build-assets/installer-hooks.nsh`，安装时自动结束旧进程、静默卸载旧品牌「Trae Work 助手」并清理残留目录 / 卸载键 / 快捷方式 / 旧命名主程序。判定依据为安装时产品名而非版本号：已发布的 v2.4.4 及更早安装包均为旧品牌，同样被自动清理；仅「AI Work 助手」品牌（v3.0.0 起）走 NSIS 原生原地升级。
- **老应用数据自动迁移（启动时，复制语义）**：`state.rs::migrate_legacy_dirs()` 将 `%APPDATA%\TraeWorkAssistant` **递归复制**为 `%APPDATA%\AIWorkAssistant`，并复制 WebView2 界面偏好目录（identifier 变更所致）；**旧目录原地保留，老应用可继续使用，新旧两版可并存**；新目录已有数据则自动跳过（不重复迁移）；失败不影响启动。
- **计划任务并存迁移**：`misc.rs::try_migrate_legacy_task()` 检测到旧任务时按其原触发时间重建 `AIWorkAssistant_DailyCheckin`，**旧任务保留**供老应用继续使用；「取消注册」只删除新任务名。
- **环境变量**：`TRAEDATA_DIR` → `AIWORKDATA_DIR`（Python 脚本与 PowerShell 桥接脚本兼容读取旧变量名）。
- **版本线划分**：新版本自 3.0.0 起维护，**之前所有 2.x 版本升级到 3.x 均需数据迁移**（安装 / 首次启动自动完成）；原「Trae Work 助手」产品通过 `trae_work_main` 分支维护（仅 Trae Work 单应用，2.x.x，仅必要修复）。
- **界面**：账号管理页「使用帮助」按钮改为与页头描述文字水平对齐，并以圆形色块徽章突出展示（PageHeader 的 leftExtra 移入描述行内，与描述行垂直居中）。
- **应用内检查更新**：「关于」页版本号旁新增「检查更新」按钮——分析 GitHub Releases 最新发布，发现比当前更大的版本时自动下载 NSIS 安装包（实时进度条）并静默安装（/S，走安装钩子自动清理旧版），随后应用自动退出完成升级；网络异常时提示并附发布页直链。
- 版本号 2.5.0 → **3.0.0**（品牌迁移后的新版本起点；`package.json` / `tauri.conf.json` / `Cargo.toml` / `about.ts` 同步）。

### 说明

- **老 MSI 安装包无法原地升级**：MSI UpgradeCode 随 identifier 变化，老版本 MSI 用户请先卸载后安装新版，或改用 NSIS 安装包（-setup.exe）升级（推荐，自动迁移）。
- 旧数据目录迁移采用「复制」：迁移后旧目录原地保留（老应用可继续使用，两版并存）；迁移只在首次启动执行一次，之后新目录已有数据即跳过。

### 审查修正（发布前全量审查）

- 升级兼容判定依据修正为「安装时产品名（NSIS 卸载键）」而非版本号，并经本机 2.4.4 注册表实证（卸载键/安装目录均为「Trae Work 助手」）；README / AGENT.md / 本条目同步。
- 安装钩子 PREINSTALL 补充结束过渡版主进程 "AI Work 助手.exe"（防止其运行中锁住旧命名主程序清理）。
- 修正「注册任务」权限不足提示中的手动命令引号错误（`&`→`&&`、数据目录值补闭合引号，与实际 /TR 一致）。
- AGENT.md 三处与 `src-python/tests/test_proxy.py` 的环境变量名同步为 `AIWORKDATA_DIR`。

---

## [2.5.0] - 2026-09-06

功能版本：账号导入 + 导出优化 + 工具栏重排（提交 bddfdf0）。

### 变更

- 账号管理工具栏重排：右侧依次为 刷新数据 / 扫描本机 / OAuth 登录 / 添加账户 / 导出账户 / 导入账户 / 分组管理 / 快照管理，帮助按钮纯图标移至描述后（PageHeader 新增 leftExtra 插槽）。
- 导出账户增强：携带应用版本、dcId、addedAt，兜底导出视图外原始账号；新增「导入账户」按钮与 `accounts_import` 命令（三种格式兼容，uid+JWT 去重，分组按 id 合并）。
- 账号池预留记录数据中心级 id（icube-dc）：`RawAccount.DcID` + 切换 / 保存登录态时自动回填 + 批量回填命令。
- F-08 双应用账号自动发现修复：本机证据推导 Cloud-IDE uid（Trae CN 读 storage.json，SOLO 读 state.vscdb）；套餐到期时间从会员包 expire_time 提取。
- F-47 进程关闭等待缩短为 3s / 2s（轮询 250ms）。
- AGENT.md 新增 §15 版本升级规则（完整功能 = 中位 +1，修复 / 优化 / 微小 = 低位 +1）。

---

## [2.4.4] - 2026-08-16

维护版本：清理临时文档并同步版本号。

### 变更

- 删除临时问题分析报告 `docs/issue-analysis-2026-08-16.md`，其功能已由 `CHANGELOG.md` 与 `AGENT.md` 中的变更说明覆盖，避免重复维护。
- 版本号 2.4.3 → 2.4.4（`package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` 四处同步）。
- 同步更新 `README.md`、`AGENT.md`、`docs/user-manual.md`、`docs/tech-framework.md`、`docs/operation-manual.md` 中的版本标注，以及 `scripts/make_portable_zip.py` 的便携包文件名。

### 说明

- 本版本**无代码逻辑改动**，仅文档与版本号维护；v2.4.3 的代理/VPN 共存与定时任务修复保持有效。
- 若需重新打包安装包，仍须执行 `npm run tauri build`（Python 侧修复已随 v2.4.3 打包）。

---

## [2.4.3] - 2026-08-16

修复「开启本地代理后 GitHub / Google 打不开」与「定时签到注册·查询·取消无反应」两类问题。

### 修复

- **本地代理与 VPN 冲突导致外网无法访问**（`ERR_TUNNEL_CONNECTION_FAILED`）
  - 根因：`proxy_start` 把 Windows 系统代理**整体覆盖**为 `127.0.0.1:8899`，抹掉了 VPN（Clash / v2rayN 等本地 HTTP/SOCKS 代理）的接管点；而 `tunnel_raw()` 对非 Trae 域名使用 `socket.create_connection` **直连**上游，完全绕开 VPN，导致 GitHub / Google 被阻断，而 baidu / qq 等国内站点直连可达故始终正常。
  - 修复：引入**上游代理链式转发**。`proxy_start` 在改写系统代理**之前**先读取已有的系统代理配置，作为 `UPSTREAM_PROXY` 环境变量注入 Python 代理进程；`device_proxy.py` 新增 `_parse_upstream()` / `connect_via_upstream()`，支持 **HTTP CONNECT** 与 **SOCKS5**（含用户名密码认证）两类上游。`tunnel_raw()` 与明文 HTTP 转发路径对**非 Trae 域名**优先经上游（即 VPN）出站，上游不可用时自动回退直连。Trae 域名仍由本代理 MITM 解密以捕获 JWT。
- **CONNECT 隧道缺少握手应答**
  - `tunnel_raw()` 从未向客户端回送 `HTTP/1.1 200 Connection Established`，客户端因此永远不会发起 TLS 握手；上游不可达时也无任何应答，浏览器无限等待。现已补齐 `200` 握手，失败时回 `502 Bad Gateway`。
- **停止代理会破坏 VPN 设置**
  - `proxy_stop` 原先只是把 `ProxyEnable` 置 0。现改为**原样还原**启动前捕获的系统代理（含 `ProxyServer` 与 `ProxyOverride`），停止本地代理后 VPN 立即恢复可用。
- **计划任务查询结果中文乱码**
  - 根因：`schtasks` 的中文输出为 **GBK** 编码，Rust 侧用 `String::from_utf8_lossy` 按 UTF-8 解读，产生 mojibake（如 `ϵͳ�Ҳ���ָ�����ļ���`，实为「系统找不到指定的文件」）；乱码进一步导致「找不到」关键字匹配失效，无法命中「任务未注册」的友好分支。
  - 修复：新增 `run_schtasks()` 统一入口，前置 `chcp 65001` 强制 schtasks 以 UTF-8 输出，中文错误信息可正确解码与匹配。
- **错误提示前缀重复**
  - 原先 Rust 返回 `查询计划任务失败：…`，前端 `Settings.tsx` 又拼接 `查询失败：`，叠加成「查询失败：查询计划任务失败：…」。现 Rust 端只返回纯错误文案，前端前缀成为唯一前缀。
- **定时任务注册在普通用户下失败**
  - 移除 `schtasks /RL HIGHEST`（签到脚本只读写 `%APPDATA%` 并运行 Python，无需提权，强制最高权限会让普通用户卡在 Access Denied）；`/TR` 命令行改为 `cmd /c set "TRAEDATA_DIR=…" && "<python>" "<script>"`，对含空格的路径安全。
- **查询 / 取消操作静默吞错误**
  - `task_status` 原先无论成功失败都返回 `Ok(stdout)`，任务不存在时返回空串，界面显示空白；`task_unregister` 原先丢弃执行结果永远返回 `Ok(())`。现均真实上报结果：任务不存在时返回明确提示「未注册每日签到任务（请先在设置页点击「注册任务」）。」，取消时若任务本就不存在按已删除处理。

### 变更文件

| 文件 | 说明 |
|---|---|
| `src-python/device_proxy.py` | 上游代理链式转发（HTTP CONNECT / SOCKS5）、`tunnel_raw` 补 `200` 握手与 `502` 兜底 |
| `src-tauri/src/commands/proxy.rs` | 启动前捕获系统代理并注入 `UPSTREAM_PROXY`、停止时原样还原、抽出 `apply_proxy` / `get_existing_win_proxy` |
| `src-tauri/src/commands/misc.rs` | 新增 `run_schtasks()`（`chcp 65001`）、去重复前缀、移除 `/RL HIGHEST`、错误可见性增强 |
| `docs/issue-analysis-2026-08-16.md` | 新增问题深度分析报告（调用链、根因、修复、验证方法） |

### 升级注意

`device_proxy.py` 会被打包进安装包的 `resources/python/`，**代理相关修复必须重新执行 `npm run tauri build` 才会进入正式版**；开发模式 `npm run tauri dev` 直接读取 `src-python/`，重启代理即生效。

---

## [2.4.2] - 2026-08-15

### 修复
- 修复发布版黑框 / 闪退 / 排队提醒丢失等 GUI 失灵问题。
- 健康检查端点统一为 `/health`，文档英文化与路径清理。
- 移除 `proxy_logs` 目录引用，日志统一存放在 `logs/` 下。
- 全面修复文档错误；恢复误删的 `src-python/tests/test_auto_checkin.py`。

### 新增
- 便携版打包脚本 `scripts/make_portable_zip.py` / `scripts/package_portable.py`。

---

## [2.4.1] - 2026-08-14

### 变更
- 项目重命名为 `trae-work-assistant`，同步更新文档与用户手册。

### 新增
- 账号切换流程重构、保存登录态能力、帮助说明。

### 修复
- 日志相关问题修复。

---

## [2.4.0]

- API 服务页面重构、日志页面整合与 UI 优化。

## [2.3.0]

- API 服务协议对齐、代理修复、交互优化与日志增强。

## [2.2.0]

- 全面质量优化：Mutex 安全锁（poison 恢复）、竞态修复、暗色模式图表适配、积分三线趋势图。

## [2.0.0]

- 本地 API 网关（axum + ureq）、SSE 协议转换、账号池智能调度、签到错误冷却状态机、6 层设备标识重置。
