# 技术架构设计 — AI Work 助手 v3.2.7

> 本文是技术侧唯一总纲：技术选型、架构分层、数据模型、进程契约、API 协议参考、开发运维与排错。
> 产品侧（需求/交互/界面）见 [product-design.md](product-design.md)；WorkBuddy 接入设计见 [workbuddy-product-design.md](workbuddy-product-design.md)。
> **完整 Tauri 命令契约表以根目录 `AGENT.md` §5 为权威**，本文只保留契约概览与协议细节，避免双维护漂移。

## 1. 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| UI | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 + lucide-react | Web 技术栈还原设计稿；Zustand 单一状态源；Recharts 图表 |
| 外壳 | Tauri 2.x（Rust 1.75+ MSVC） | 包体 8~15MB（远小于 Electron），可调用系统 API（注册表/证书/计划任务/DPAPI） |
| 核心逻辑 | Python 3.9+（仅标准库 + cryptography） | 复用已验证的签到/代理/豆包脚本逻辑，可热更 |
| 登录态切换 | PowerShell 5.1+（`trae-switch-bridge.ps1`，四应用档案表驱动） | 复用备份/恢复/设备标识重置逻辑，系统自带 |
| API 网关 | Rust axum（内嵌，复用 Tauri tokio runtime） | OpenAI / Anthropic 双协议端点 + SSE 转换 + 账号池调度，无独立进程 |
| HTTP 客户端 | ureq（同步）+ `spawn_blocking` 包装 | 双 Client 设计：短请求 120s 超时 / 流式仅 ResponseHeaderTimeout 120s，共享连接池 |
| 加密 | tauri-plugin-stronghold + windows-sys(DPAPI) | jwt/refresh_token 入 vault，主密码经 DPAPI 仅本机当前用户可解 |
| 测试 | cargo test + Python unittest + vitest | Rust 30 用例 / Python 纯函数 / 前端 `src/lib/format.test.ts` |
| 打包 | Tauri Bundler → MSI / NSIS（自定义模板） | 含 Python 运行时与 PS 脚本；产物经 `scripts/rename_release.py` 输出中文命名到 release/ |

**不采用**：Electron（体积过大）、WPF/WinUI（样式成本高）、PyQt（视觉不达要求）、React Router/Redux（依赖最小原则）。

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
 └─ api_server                axum 网关（routes/pool/payload/sse/auth/models_sync/usage/api_logger）
Python Core   auto_checkin.py / device_proxy.py / doubao_*.py（renew/quota/chats）
PowerShell    trae-switch-bridge.ps1（-TargetApp TraeWork|Trae|Doubao|WorkBuddy + SnapshotLayout）
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

| 文件 | 说明 |
|---|---|
| `conf/app_settings.json` | Settings 全字段（snake_case） |
| `conf/vault.stronghold` + `conf/vault_key.bin` | jwt/refresh_token 权威存储（按 uid 键）+ DPAPI 加密的 vault 主密码；JSON 落盘占位化，Python 签到走临时解密文件（用后即删） |
| `data/checkin_accounts.json` | 账号 + JWT（敏感字段 vault 化后为占位） |
| `data/device_map.json` | user_id → 虚拟设备身份（`rand_digits(n, seed=user_id)` 稳定派生） |
| `data/groups.json` | 分组 + membership |
| `data/credits_history.json` / `credits_daily.json` / `remaining_credits.json` | 积分明细 / 每日三线快照 / 剩余积分缓存（均裁剪 90 天） |
| `data/checkin_results.json` | 签到最终态按日落库（T8，保留 90 天） |
| `data/account_cooldowns.json` | 签到错误冷却状态 |
| `data/api_pool.json` | 账号池：enabled_uids + `strategy`(expire_first/credit_first/random) + `group_ids`（T10） |
| `data/api_keys.json` / `api_usage.json` / `api_models.json` | 多 API Key+每日配额 / 用量按日统计 / 模型目录 |
| `data/profiles/`、`profiles_trae/`、`profiles_doubao/` | 三应用登录态快照（current_account.txt + <uid> 槽位 + .bak 单代回滚） |
| `data/doubao_accounts.json` / `doubao_captured_credentials.json` / `doubao_health_history.json` / `doubao_chats/` | 豆包账号池 / 抓包凭证回写 / 运维健康史 / 对话备份 |
| `data/certs/` | 自签 CA |
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

### 5.1 `auto_checkin.py`

- 参数（向后兼容）：`--json-stream`（NDJSON）、`--accounts UID1,UID2`、`--scope all|group:<id>`、`--accounts-file`（vault 临时解密文件）。
- NDJSON 示例：

```json
{"type":"start","total":6}
{"type":"account","index":1,"user_id":"4487…","name":"…","status":"success","delta":300,"elapsed":1.24}
{"type":"account","index":2,"user_id":"…","status":"fail","error_type":"PlanLimit","cooldown_until":1786700000}
{"type":"done","ok":5,"already":0,"failed":1}
```

- `status` ∈ `already|success|fail`；每次签到追加 `{date, user_id, credits, delta}` 入 `credits_history.json`。

### 5.2 `device_proxy.py`

- env：`AIWORKDATA_DIR`、`PROXY_PORT`（默认 8899）、`AUTO_CAPTURE_JWT=1`、可选 `UPSTREAM_PROXY[_USER/_PASS]`（http/socks5）。
- MITM 捕获 `trae.cn`/`trae.com.cn` 带 `Cloud-IDE-JWT` 的请求写回账号库（exp 防降级）；`mchost.guru` 解密记录对话摘要；WebSocket 隧道转发不记录内容。
- 结构化日志 `logs/proxy_req_YYYY-MM-DD.log` 供 `proxy_logs_list/detail` 查询。
- 同时承担豆包凭证抓包（doubao.com Cookie sessionid/sid_guard/ttwid 落盘供回写）。

### 5.3 `trae-switch-bridge.ps1`（四应用切换桥）

- Action：`Switch / SaveCurrentLogin / ResetMachineId / ResetDeviceIds / BackupCurrent / RestoreOnly / KeepAlive`；通用参数 `-TargetApp TraeWork|Trae|Doubao|WorkBuddy`、`-Json`、`-ProxyPort`、`-IncludeIndexedDB`、`-ExpectedCurrentUid`。
- icube 布局（Trae 双应用）：精准备份 9 类核心文件；chromium 布局（豆包）：白名单目录快照 + `snapshot_meta.json`（schemaVersion=1）+ `Test-SnapshotIntegrity` 三层校验 + `.bak` 单代回滚 + ExpectedCurrentUid 防误覆盖守卫；authfile 布局（WorkBuddy）随其批次接入。
- 保存前预检登录会话（Cookie 存在性检测），未登录态拒绝入槽。
- PowerShell 5 需 UTF-8 with BOM + CRLF 行尾（LF 无 BOM 中文解析错误）。

### 5.4 API 网关（api_server 模块）

| 端点 | 说明 |
|---|---|
| `GET /health`（免鉴权）/ `GET /status` | 健康检查 / 账号池状态 |
| `GET /v1/models` | 与 `data/api_models.json` 同源，官网同步后无需重启 |
| `POST /v1/chat/completions` | OpenAI 协议（流式 + 非流式） |
| `POST /v1/messages` | Anthropic Messages 协议（message_start → content_block_* → message_delta → message_stop） |
| `POST /v1/completions` | legacy text completion（prompt 转 user message 复用链路） |
| `/v1/embeddings` | 明确 501（上游无对应能力，不做假实现） |

- 鉴权：API Keys 列表（T2/T15），`Authorization: Bearer` + `x-api-key` 双风格；未配置启用 Key 时不鉴权；超日配额 429。
- 请求侧统一转 OpenAI 内部格式复用池调度；响应侧按协议分别输出；reasoning_content 暂不输出（thinking 块需签名）。
- 账号池 app 无关：Trae / Trae Work 账号入池即被同一网关服务（通用积分 208）。

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
- `llm_utils_chat` 端点**明文 JSON 可行**（已验证），`create_agent_task`（Work 积分 209）为 ~123KB 富上下文加密体，**外部无法复刻**（真实身份复刻仍 4001）——Work 积分接入只能走多活会话编排，见 [product-optimization-backlog.md](product-optimization-backlog.md) W-01。

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

### 7.1 环境准备（一次性，仅 Windows）

| 依赖 | 要求 | 校验 |
|---|---|---|
| Windows | 10 / 11 | `winver` |
| Node.js | ≥ 18（建议 22） | `node -v` |
| Rust | ≥ 1.77 stable（MSVC，edition 2021） | `rustc --version` |
| VS Build Tools | 「使用 C++ 的桌面开发」+ Windows SDK | 链接错误多因缺失此项 |
| Python | ≥ 3.9（打包时内嵌，运行期自动探测；内嵌不可用回退系统解释器） | `python --version` |
| WebView2 | Win11 自带 / Win10 装 Evergreen Bootstrapper | — |

```powershell
npm install
npm run tauri dev      # 开发模式（Vite 5173 + Rust 热重载；勿裸 npm run dev，白屏）
npm run tauri build    # 打包 MSI + NSIS → src-tauri/target/release/bundle/
python scripts/rename_release.py    # 产物输出 release/，中文命名
python scripts/package_portable.py  # 便携版 zip
```

测试：`cargo test`（Rust）、`python src-python/tests/test_auto_checkin.py`（Python）、`npm run test`（vitest 前端）。

### 7.2 打包注意

- `src-python/` 打进 `resources/python/`：**Python 侧改动在正式版必须重新 `tauri build`**（dev 模式直读源码即生效）。
- `src-python/` 严禁混入 Python 运行时文件（python.exe/Lib 等）；解释器统一 `import encodings` 自举验证。
- NSIS 用自定义模板 `build-assets/installer.nsi`（升级安装默认直接覆盖）；`installer-hooks.nsh` 处理旧品牌静默卸载（需 UTF-8 BOM）。
- 升级 Tauri CLI 后如 NSIS 构建报错，需从对应版本 tag 重新同步模板。

### 7.3 常见问题排错

| 现象 | 原因 | 处理 |
|---|---|---|
| `cargo build` 链接失败 / 找不到 link.exe | 未装 VS Build Tools C++ 工作负载 | 装「使用 C++ 的桌面开发」+ SDK，确认 MSVC 目标 |
| 启动白屏 / `invoke` 不存在 | 浏览器直开 5173，未走 Tauri 外壳 | 用 `npm run tauri dev` |
| 代理捕获不到 JWT | 未装 CA 或 Trae 未走代理 | 一键安装证书（UAC）→ 启动代理 → 看日志 listening |
| 开代理后部分网站打不开 | 系统代理被改写 | v2.4.3 起自动串联已有代理为上游；Python 改动需重新打包 |
| 停代理后 VPN 失效 | 旧版只置 0 未还原 | 已改为原样还原 ProxyEnable/ProxyServer/ProxyOverride |
| 计划任务输出乱码 / Access Denied | schtasks GBK / `/RL HIGHEST` | 统一走 `misc.rs::run_schtasks()`（chcp 65001）；不加 /RL HIGHEST |
| 打包后报缺脚本/Python | resources 未包含或混入运行时 | 检查 `bundle.resources`；解释器自举回退兜底 |

## 8. 风险与应对

| 风险 | 等级 | 应对 |
|---|---|---|
| 上游升级导致接口/路径变化 | 高 | 核心逻辑留在可热更的 Python/PS；`dig()` 宽容解析；接口层独立模块 |
| 上游启用证书固定 | 高 | 降级：OAuth 登录获取可续期凭据（refresh_token 13 天自动续期） |
| 安全软件拦截 CA/代理 | 中 | 白名单指引 + 代码签名 |
| 多账号触发风控 | 中 | 免责声明 + 签到间隔随机抖动 + 冷却状态机 |
| 凭证明文泄露 | 中 | vault + DPAPI；UI 掩码；账号池文件 .gitignore；导出提醒备份 vault |
| UAC 拒绝 | 低 | 明确提示 + 手动步骤 |
| Python 运行时残缺 | 低 | 自举验证 + 系统解释器回退 |
