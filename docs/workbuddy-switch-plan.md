# WorkBuddy 账号切换 / 会话续期 / 余额展示 / 一键签到 实现方案

> **文档版本**: 2026-09-06 · 调研分支 `feat/traecode_doubao`
> **调研方式**: 本机实测侦察（`~/.workbuddy`、`%LOCALAPPDATA%\CodeBuddyExtension` 实机文件）+ 官网（workbuddy.cn）+ 开放平台文档 + 社区已实测文章交叉验证
> **结论先行**: **四个能力全部可行，且整体难度低于 Trae**。WorkBuddy 登录态是**明文 JSON + 标准 Keycloak OIDC token**（社区已公开实测其签到 HTTP 接口），不需要逆向加密、不需要 MITM。预计 5~7 天可完成全部功能。

---

## 0. 结论速览（TL;DR）

| 能力 | 可行性 | 依据 |
|---|---|---|
| 账号切换 | ✅ 高 | 登录态集中在 1 个明文 JSON 文件 + 用户数据目录分层清晰，快照/恢复即可 |
| 会话保存续期 | ✅ 高 | 标准 Keycloak（realm=`copilot`），`refreshToken` 有效期 90 天，官方标准端点续期 |
| 余额展示 | ✅ 高 | 客户端自带本地 quota API + 云端用量接口；社区文章实测 `remaining` 字段 |
| 一键签到 | ✅ 高 | 社区已公开实测接口（`codebuddy.cn/v2/billing/meter/daily-checkin`），本项目加一个定时任务即可 |

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
  1. Keycloak 标准 Token 端点（由 `iss` 推导，已实测）：
     `POST https://www.workbuddy.cn/auth/realms/copilot/protocol/openid-connect/token`
     `grant_type=refresh_token & client_id=console & refresh_token=<refreshToken>`
  2. 定时任务（复用 `schtasks` + `misc.rs::run_schtasks()` 中文编码方案）每周跑一次：刷新 token → 成功则回写 `workbuddy-desktop.info`（客户端未运行时；运行中则跳过并提示）+ 更新账号池；失败则通知重新扫码登录。
  3. 刷新响应结构与登录一致（`access_token/refresh_token/expires_in`），整体覆盖 `auth` 节点。
- **风险**：Keycloak 可能校验 `sessionState` 单点会话（同账号异地登录使旧 session 失效）→ 多账号属同一人的不同账号，天然无冲突；同账号多处刷新属官方支持行为。

### 2.4 余额展示

三层取数，按可用性降级：
1. **云端接口（首选，待一次抓包确认端点）**：客户端「积分余额/用量管理」页的数据源是 workbuddy.cn / codebuddy.cn 域下的已登录 XHR。实现时用本项目 MITM 代理（`device_proxy.py`）抓一次：过滤 `quota|billing|credit|usage` 关键词即得端点与字段（如 `remaining/initial/used`）。之后可脱离客户端直查——**这是多账号池余额聚合展示的关键**（本地 API 只反映当前登录账号）。
2. **本地 quota API（零成本兜底）**：客户端运行中时 `GET http://127.0.0.1:<port>/api/v1/quota`。端口发现策略：扫描 `~/.workbuddy` 下 `*.port` 文件（本机已见 `tencent-docs-engine.port`，同模式）+ 探测常见端口段，从响应 JSON 特征（含 `remaining`）确认。
3. **快照字段（静态兜底）**：`storage/skeleton/account-snapshot.json` 的 `editionType`（free/pro）+ `savedAt`，展示"套餐类型 + 数据时间"。

前端：Credits 页新增「WorkBuddy」Tab——余额大字 + 月度消耗趋势（云端接口有历史用量时）+ 各账号余额对比条形图。

### 2.5 一键签到

社区已实测直签方案，直接落地为本项目技能：
1. **新增 Python 脚本** `src-python/workbuddy_checkin.py`（仅标准库，对齐 `auto_checkin.py` 风格）：
   - 读 `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`（或账号池文件）；
   - `POST /v2/billing/meter/checkin-activity-status` 查状态（幂等前置）；
   - 未签 → `POST /v2/billing/meter/daily-checkin`；解析返回的积分数额与连续天数；
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
| P1 | Keycloak refreshToken 定时续期 + 到期提醒 | 1~2 天 |
| P2 | `workbuddy_checkin.py` 直签 + 前端 Checkin 页集成 + 定时任务 | 2 天 |
| P3 | 余额展示：本地 quota API 接入 + MITM 抓包固化云端端点 → Credits 页 Tab | 2~3 天 |
| P4 | 多账号余额聚合 + 趋势图 | 1 天 |

## 4. 风险与合规

- **凭证安全**：`accessToken`/`refreshToken` 等同密码。账号池文件入 `.gitignore`；日志/日志上报/异常消息中零 token 输出；文档与 UI 一律掩码。
- **接口稳定性**：签到/余额接口为非公开契约，腾讯可能调整（签到规则 2026 年内已变多次）。对策：接口层独立模块 + 域名探测（codebuddy.cn / workbuddy.cn 双域）+ 失败即通知而非静默重试；奖励数额以接口返回为准，不硬编码。
- **频控与风控**：签到每日一次天然低频；续期每周一次低频；余额查询建议 ≥5 分钟间隔缓存。不做批量注册、不做多开薅积分。
- **与官方关系**：本工具与 WorkBuddy/腾讯无关联，仅供本人账号管理，沿用项目既有免责声明。
- **客户端运行时互斥**：auth 文件被客户端启动时重写（每次启动生成新历史快照），**切换与续期必须在客户端关闭窗口期执行**，工具需做文件监听/进程检测防止写冲突。

## 5. 与豆包+Trae 方案的复用关系

- 三应用共用：`app_locate` 安装识别、快照桥（PS 参数化）、账号池文件结构、NDJSON 事件管线、Credits 页多 Tab 骨架、`schtasks` 定时任务封装。
- 实施顺序建议：**WorkBuddy（本文档，最快见效）→ Trae CN（改路径移植）→ 豆包（新模块 + 二期抓包）**。
