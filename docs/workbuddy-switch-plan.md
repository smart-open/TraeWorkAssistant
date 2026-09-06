# WorkBuddy 账号切换 / 会话续期 / 余额展示 / 一键签到 实现方案

> **文档版本**: 2026-09-06 · 调研分支 `feat/traecode_doubao`
> **调研方式**: 本机实测侦察（`~/.workbuddy`、`%LOCALAPPDATA%\CodeBuddyExtension` 实机文件）+ 官网（workbuddy.cn）+ 开放平台文档 + 社区已实测文章交叉验证 + **开源参考实现源码级验证（[cxqc168-wq/Trae-workbuddyAssistant](https://github.com/cxqc168-wq/Trae-workbuddyAssistant)，MIT，另参考 changexbc/workbuddy-switch）**
> **结论先行**: **四个能力全部可行，且整体难度低于 Trae**。WorkBuddy 登录态是**明文 JSON + 标准 Keycloak OIDC token**，且开源参考实现已完整跑通签到/续期/积分查询/OAuth 四条链路，端点与请求头均有可运行代码佐证。预计 5~7 天可完成全部功能。

---

## 0. 结论速览（TL;DR）

| 能力 | 可行性 | 依据 |
|---|---|---|
| 账号切换 | ✅ 高 | 登录态集中在 1 个明文 JSON 文件 + 用户数据目录分层清晰，快照/恢复即可；开源实现同款 auth 文件导入方案 |
| 会话保存续期 | ✅ 高 | 官方插件网关续期端点 `POST /v2/plugin/auth/token/refresh`（`X-Refresh-Token` 头）已被开源实现实测跑通 |
| 余额展示 | ✅ 高 | 云端积分接口三件套（summary/paid/free-packages）+ 旧接口回退已被开源实现源码验证，**无需 MITM 抓包** |
| 一键签到 | ✅ 高 | 开源实现完整验证（状态查询→提交→幂等容错→日志），本项目加一个定时任务即可 |

---

## 1. 本机实测侦察结论（证据基础）

### 1.1 安装与数据布局（实测）

```
C:\Users\<user>\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe   # 程序（Electron）
C:\Users\<user>\AppData\Local\CodeBuddyExtension\Data\Public\auth\
├── workbuddy-desktop.info                # ★ 登录态主文件（明文 JSON）
└── workbuddy-desktop.<时间戳>.<pid>.<uuid>.info   # 客户端自动留存的登录态历史快照
C:\Users\<user>\.workbuddy\              # 主数据目录（Electron 自定义 userData）
├── storage\
│   ├── skeleton\account-snapshot.json    # 账号摘要：uid/nickname/editionType(free)/savedAt
│   └── user-<uid>[-personal]\global|scoped\   # 按 uid 隔离的用户数据（实测已存在两个 user 目录）
├── app\session\                          # Chromium 会话（Cookies/Local Storage，运行中被进程独占锁定）
├── workbuddy.db                          # SQLite：sessions / session_usage / automations
├── device-id                             # UUID 设备标识
└── settings.json / usage-log.json 等
```

### 1.2 登录态文件实测结构（`workbuddy-desktop.info`，敏感值已脱敏）

```json
{
  "account": {
    "uid": "e5ea09b1-...",          // 与 .workbuddy/storage/user-<uid> 目录名一致
    "nickname": "七点半",
    "uin": "33010xxxxx",
    "type": "personal",
    "lastLogin": true,
    "phoneNumber": "158xxxxxxxx"
  },
  "auth": {
    "accessToken":  "eyJhbGciOiJSUzI1NiI...(RS256 JWT, ~1300 字符)",
    "refreshToken": "eyJhbGciOiJIUzUxMiI...(HS512, ~700 字符)",
    "expiresIn": 5184000,            // accessToken：60 天
    "refreshExpiresIn": 7776000,     // refreshToken：90 天
    "tokenType": "Bearer",
    "sessionState": "9d8c4998-...",
    "scope": "openid profile offline_access",
    "domain": "www.workbuddy.cn"
  }
}
```

**JWT 解码实测**：`iss = https://www.workbuddy.cn/auth/realms/copilot`，`azp = console`，`sub = uid`——**标准 Keycloak 授权码流程产物**。这意味着续期、鉴权全部走官方 OIDC 规范，无需任何逆向。

### 1.3 社区公开实测资料（交叉验证）

1. **签到接口**（腾讯云开发者社区 / 掘金 2026 年文章，多源一致）：
   - 状态查询：`POST https://www.codebuddy.cn/v2/billing/meter/checkin-activity-status`
   - 执行签到：`POST https://www.codebuddy.cn/v2/billing/meter/daily-checkin`
   - 请求头：`Authorization: Bearer <accessToken>` + `X-User-Id: <uid>`
   - 幂等：已签返回 `code: 10001`（"今天已签到，请明天再来"）
   - ⚠️ 域名注意：是 `www.codebuddy.cn`，不是 `copilot.tencent.com`（后者 404）；本机 auth 文件 `domain=www.workbuddy.cn`（业务主站），签到计费接口在 codebuddy.cn 域——两者并存，实现时以实测为准做域名探测。
   - ⚠️ 积分规则近期多变：旧版签到 100/天（7 天循环第 7 天 1000），2026-05 调整为「Buddy 加油站能量包 150/天」；实现上**只依赖接口，不硬编码奖励值**，UI 展示接口返回的实际数额。
2. **余额查询**：
   - 客户端运行时本地 API：`curl http://127.0.0.1:<port>/api/v1/quota` → JSON `remaining` 字段（端口需实测发现，见 2.3）。
   - UI 路径：头像悬停 →「积分余额」；「用量管理」页核对 `初始额度 − 总消耗积分 = 当前余额`。
3. **积分体系**（官网+社区）：免费版 500 积分/月；个人专业版 58 元/月 2000 积分/月；活动积分有效期约 30 天；另有能量包/成长计划/邀请/社区发文等获取渠道。

### 1.4 开源参考实现交叉验证（★ 关键补充）

参考 [cxqc168-wq/Trae-workbuddyAssistant](https://github.com/cxqc168-wq/Trae-workbuddyAssistant)（MIT，2026-09 活跃）的 `src-tauri/src/workbuddy/` 模块（`auth_file.rs` / `refresh.rs` / `checkin.rs` / `credits.rs` / `oauth.rs` / `http.rs`），本方案的四个能力均有**可运行源码佐证**，并修正了此前基于社区文章的两个假设：

**① 已验证端点全表**（base = `https://www.codebuddy.cn`）：

| 用途 | 端点 | 要点 |
|---|---|---|
| Token 续期 | `POST /v2/plugin/auth/token/refresh` | 请求头带 `X-Refresh-Token: <refreshToken>`（**不是** Keycloak 原生端点），响应 `data.accessToken/refreshToken/expiresIn/refreshExpiresIn` |
| OAuth 扫码·发起 | `POST /v2/plugin/auth/state?platform=workbuddy` | 返回 `state` + `authUrl`（浏览器打开扫码） |
| OAuth 扫码·轮询 | `GET /v2/plugin/auth/token?state=<state>` | 成功返回 `data.accessToken/refreshToken/domain` |
| OAuth 拉账号资料 | `GET /v2/plugin/login/account?state=<state>` | `Bearer` 头，返回 `uid/nickname/email/enterpriseId` |
| 签到状态查询 | `POST /v2/billing/meter/checkin-activity-status` | 失败回退旧路径 `/checkin-status`；`data.today_checked_in` 布尔 |
| 执行签到 | `POST /v2/billing/meter/daily-checkin` | 空体 `{}`；已签返回 `code:10001`/message 含"已签到"，**按成功容错** |
| 积分·汇总 | `POST <domain>/billing/meter/get-user-resource-summary` | 需加 `X-Client-Platform: web` 头；返回 `Packages[]` |
| 积分·付费包明细 | `POST <domain>/billing/meter/get-user-resource-paid-packages` | 返回 `Accounts[]`，含 `CycleCapacityRemainPrecise` 等字段 |
| 积分·免费包明细 | `POST <domain>/billing/meter/get-user-resource-free-packages` | 同上，需带 `SlicePeriodStartTime/EndTime`（当日） |
| 积分·旧接口回退 | `POST /v2/billing/meter/get-user-resource` | body 带 `ProductCode: p_tcaca` + `Status:[0,3]` + 时间范围 |

**② 统一请求头**（对齐官方客户端，`build_auth_headers`）：`Authorization: Bearer <accessToken>`、`X-User-Id: <uid>`、`X-Enterprise-Id`/`X-Tenant-Id`（企业账号）、`X-Domain: <domain>`（如 `www.workbuddy.cn`）；积分三件套额外带 `X-Client-Platform: web`（官网 Axios 拦截器同款，缺了会被网关拒）。

**③ 域名路由规则**：业务 token 由 `www.codebuddy.cn` 签发（auth 文件 `domain` 字段可能为 `www.workbuddy.cn`）；积分新接口的 origin 必须与 token 域一致（`domain` 匹配 workbuddy.cn 则发往 `www.workbuddy.cn`，否则发 `www.codebuddy.cn`）——**令牌域与请求域不一致会被网关拒绝**。这解释了此前"签到接口在 codebuddy.cn、auth 文件 domain 是 workbuddy.cn"的疑点：plugin/billing v2 网关固定在 codebuddy.cn，官网套餐页资源接口跟随 domain。

**④ 值得直接复用的工程设计**：
- **一次查询最多一次刷新**：多路请求（summary/paid/free）任一 401 时统一刷新一次并仅重试失败分支；旧接口回退必须复用已刷新账号，**禁止二次刷新**（防止用旧 refresh token 覆盖刚落盘的新 token）。
- **并发防护**：每账号运行互斥锁（RAII 守卫防 panic 泄漏）+ 全局轮次锁（重复触发整轮直接跳过并明确报错）。
- **惰性刷新**：`expiresAt` 距今 < 24h 或已过期且有 refreshToken 时才刷新；刷新失败标 `needs_relogin` + 原因，UI 据此提示重新扫码。
- **"已签到"容错**：`code∈{0,200}` 成功；message 含"已签到"/"repeat" 也按成功；当日状态以本地签到日志最新一条为准（免接口依赖）。
- **账号 id 生成**：`wb-` 前缀 + token 哈希（同 token 稳定同 id）；auth 文件导入字段链兼容 `account.uid/auth.accessToken` 等多种嵌套形态。
- **响应形状归一化**：积分接口兼容 `data.Accounts`/`data.data.Accounts`/`data.Response.Data.Accounts` 等 6 种嵌套路径——上游字段多变，解析层必须宽容。

**⑤ 对本方案的两处修正**：
1. **续期端点**（§2.3）：首选 `POST /v2/plugin/auth/token/refresh`（`X-Refresh-Token` 头）——开源实现已实测；Keycloak 原生端点（`iss` 推导）保留为备选验证路径。
2. **余额展示**（§2.4）：原方案"MITM 抓包固化云端端点"可**直接跳过**——积分三件套端点已被开源实现验证，P3 阶段缩为纯接入工作。

---

## 2. 实现方案

### 2.1 安装位置自动识别

三级探测（与豆包/Trae 方案共用同一 `app_locate(appKind)` Tauri 命令）：
1. 注册表卸载键 `DisplayName LIKE %WorkBuddy%` → `InstallLocation`。
2. 默认路径：`%LOCALAPPDATA%\Programs\WorkBuddy\WorkBuddy.exe`。
3. 返回值含 `{exe, authFile: CodeBuddyExtension 路径, dataDir: ~/.workbuddy, version}`——auth 路径可用环境变量拼接：`%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`。

### 2.2 账号切换

**方案：auth 文件快照 + 用户数据目录双层恢复**

| 层 | 内容 | 必要性 |
|---|---|---|
| L1（核心） | `workbuddy-desktop.info` 整文件 | 登录 token、uid、昵称——恢复后重启客户端即换号 |
| L2（体验） | `~/.workbuddy/storage/user-<uid>*` 目录 | 用户偏好/专家配置随账号走 |
| L3（可选） | `~/.workbuddy/app/session/Network/Cookies` | Web 侧会话（客户端可能用 webview 加载积分页） |

流程（与现有 `switch_account` 管线同构）：
1. 检测 `WorkBuddy.exe` 运行中 → 提示优雅关闭（注意：**本工具若作为 WorkBuddy 内的自动化运行，需提示用户"切换将重启宿主"或改用外部触发**）。
2. 备份当前 `workbuddy-desktop.info` → `data/workbuddy_profiles/<uid>/auth.info`（客户端自身已在 auth 目录留历史快照，可作为交叉校验）。
3. 写入目标账号 `auth.info`（+按需恢复 L2/L3）。
4. 重启 WorkBuddy.exe → 轮询 `account-snapshot.json` 的 `uid` 字段确认切换成功 → NDJSON 事件上报。

**多账号入库**：`workbuddy_accounts.json`（列入 `.gitignore`）记录 `{uid, nickname, phone掩码, editionType, token快照时间}`；**建议同时保存 refreshToken**（用于 2.3 续期），凭证字段永不进 UI 明文。

### 2.3 会话保存与续期（标准 OIDC）

- **保存**：即快照；另将 `expiresIn/refreshExpiresIn` 换算为绝对时间入库，形成"到期日历"。
- **续期**（refreshToken 有效期 90 天，窗口充裕）：
  1. **首选：官方插件网关端点**（开源参考实现已实测）：
     `POST https://www.codebuddy.cn/v2/plugin/auth/token/refresh`
     请求头 = 统一认证头 + `X-Refresh-Token: <refreshToken>`，空体 `{}`；响应 `data.accessToken/refreshToken/expiresIn/refreshExpiresIn`。
  2. 备选（若网关端点失效）：Keycloak 原生 Token 端点（由 `iss` 推导）：
     `POST https://www.workbuddy.cn/auth/realms/copilot/protocol/openid-connect/token`
     `grant_type=refresh_token & client_id=console & refresh_token=<refreshToken>`
  3. 刷新策略（复用开源实现的成熟设计）：**惰性刷新**——仅当 accessToken 过期或临期（<24h）且有 refreshToken 时发起；定时任务（复用 `schtasks` + `misc.rs::run_schtasks()` 中文编码方案）每周跑一次兜底。成功则回写 `workbuddy-desktop.info`（客户端未运行时；运行中则跳过并提示）+ 更新账号池；失败则标记 `needs_relogin` + 原因并通知重新扫码登录。**刷新期间同一账号加互斥锁，避免并发刷新用旧 refresh token 覆盖新 token**。
- **风险**：Keycloak 可能校验 `sessionState` 单点会话（同账号异地登录使旧 session 失效）→ 多账号属同一人的不同账号，天然无冲突；同账号多处刷新属官方支持行为。

### 2.4 余额展示

三层取数，按可用性降级：
1. **云端积分接口（首选，已被开源实现验证，无需抓包）**：
   - 三件套并行：`POST <domain>/billing/meter/get-user-resource-summary`（汇总）+ `get-user-resource-paid-packages`（付费包明细）+ `get-user-resource-free-packages`（免费包明细，带当日 `SlicePeriodStartTime/EndTime`）；
   - 请求头 = 统一认证头 + `X-Client-Platform: web`；origin 按 token 域路由（`domain` 含 workbuddy.cn → `www.workbuddy.cn`，否则 `www.codebuddy.cn`）；
   - 全部 401 → 刷新一次后仅重试失败分支；三件套全部无效 → 回退旧接口 `POST /v2/billing/meter/get-user-resource`（`ProductCode: p_tcaca`，`Status:[0,3]`，`PackageEndTimeRange` 拉满）；
   - 字段解析（开源实现已验证的宽容策略）：容量字段按 `CycleCapacitySizePrecise → CycleTotalCapacity → CapacitySize` 等链式取值，剩余/消耗同构；余额 = 各资源 `remaining` 求和；`DeductionEndTime` 判定"7 天内到期"提醒；响应形状兼容 6 种嵌套路径。
   - **这是多账号池余额聚合展示的关键**（本地 API 只反映当前登录账号）。
2. **本地 quota API（零成本兜底）**：客户端运行中时 `GET http://127.0.0.1:<port>/api/v1/quota`。端口发现策略：扫描 `~/.workbuddy` 下 `*.port` 文件（本机已见 `tencent-docs-engine.port`，同模式）+ 探测常见端口段，从响应 JSON 特征（含 `remaining`）确认。
3. **快照字段（静态兜底）**：`storage/skeleton/account-snapshot.json` 的 `editionType`（free/pro）+ `savedAt`，展示"套餐类型 + 数据时间"。

前端：Credits 页新增「WorkBuddy」Tab——余额大字 + 月度消耗趋势（云端接口有历史用量时）+ 各账号余额对比条形图。

### 2.5 一键签到

社区已实测直签方案，直接落地为本项目技能：
1. **新增 Python 脚本** `src-python/workbuddy_checkin.py`（仅标准库，对齐 `auto_checkin.py` 风格）：
   - 读 `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`（或账号池文件）；
   - `POST /v2/billing/meter/checkin-activity-status` 查状态（旧路径 `/checkin-status` 回退；幂等前置）；
   - 未签 → `POST /v2/billing/meter/daily-checkin`（空体 `{}`）；"已签到"/`code:10001` 按成功容错；解析返回的积分数额与连续天数；
   - 401 自动用 `X-Refresh-Token` 刷新重试一次（对齐开源实现，同一账号同一时刻互斥）；
   - `--json-stream` 输出 NDJSON 事件（对齐 `checkin-progress` 事件管线）；
   - 全程不打印任何真实 token（沿用社区最佳实践：只输出 `已加载(内容已隐藏)`）。
2. **Tauri 侧**：`workbuddy_checkin_start(opts)` 命令 + Checkin 页新增 WorkBuddy 分区；错误冷却复用 `account_cooldowns.json` 机制。
3. **定时**：`schtasks` 注册每日任务（复用现有计划任务注册逻辑，注意 GBK 编码与 `/RL HIGHEST` 禁忌——见 AGENT.md 第 14 节）。
4. **失败通知**：`tauri-plugin-notification` 桌面通知；可选接入企业微信/Server酱（社区文章同款做法）。

---

## 3. 实施计划

| 阶段 | 内容 | 预估 |
|---|---|---|
| P0 | `app_locate` 识别 + 多账号池入库 + auth 快照切换 | 2 天 |
| P1 | 插件网关端点 refreshToken 定时续期 + 到期提醒 | 1~2 天 |
| P2 | `workbuddy_checkin.py` 直签 + 前端 Checkin 页集成 + 定时任务 | 2 天 |
| P3 | 余额展示：积分三件套接入（端点已验证，免抓包）→ Credits 页 Tab | 1~2 天 |
| P4 | 多账号余额聚合 + 趋势图 | 1 天 |

> 端点请求头、响应形状、容错与并发防护细则可直接对照开源参考实现 `src-tauri/src/workbuddy/` 模块（MIT 许可）移植，P1~P3 均为低风险接入工作。

## 4. 风险与合规

- **凭证安全**：`accessToken`/`refreshToken` 等同密码。账号池文件入 `.gitignore`；日志/日志上报/异常消息中零 token 输出；文档与 UI 一律掩码。
- **接口稳定性**：签到/余额接口为非公开契约，腾讯可能调整（签到规则 2026 年内已变多次）。对策：接口层独立模块 + 域名探测（codebuddy.cn / workbuddy.cn 双域）+ 失败即通知而非静默重试；奖励数额以接口返回为准，不硬编码。
- **频控与风控**：签到每日一次天然低频；续期每周一次低频；余额查询建议 ≥5 分钟间隔缓存。不做批量注册、不做多开薅积分。
- **与官方关系**：本工具与 WorkBuddy/腾讯无关联，仅供本人账号管理，沿用项目既有免责声明。
- **客户端运行时互斥**：auth 文件被客户端启动时重写（每次启动生成新历史快照），**切换与续期必须在客户端关闭窗口期执行**，工具需做文件监听/进程检测防止写冲突。

## 5. 与豆包+Trae 方案的复用关系

- 三应用共用：`app_locate` 安装识别、快照桥（PS 参数化）、账号池文件结构、NDJSON 事件管线、Credits 页多 Tab 骨架、`schtasks` 定时任务封装。
- 实施顺序建议：**WorkBuddy（本文档，最快见效）→ Trae CN（改路径移植）→ 豆包（新模块 + 二期抓包）**。
