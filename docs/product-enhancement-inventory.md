# 产品功能增强点汇总（Enhancement Inventory）

> **文档版本**: 2026-09-07 · 调研分支 `feat/traecode_doubao`
> **汇总来源**: 三份调研文档交叉汇总——`workbuddy-switch-plan.md`（WorkBuddy 四件套方案）、`doubao-trae-switch-plan.md`（豆包+Trae CN 方案）、`oss-ecosystem-research.md`（六仓库生态 + DSH/Codex 三轮调研）
> **范围界定**: 仅列**新增/增强**功能点；项目现有能力（Trae Work 账号切换、IDE 签到、API 网关 IDE 路由、Credits 趋势图、device_proxy、schtasks 定时、桌面通知）作为基线，不重复列出。
> **编号规则**: F-xx 全局唯一；"来源"标注调研文档章节与参考仓库；"预估"为单人开发工作量。

---

## 0. 现有能力基线（界定增强范围）

| 已有能力 | 状态 |
|---|---|
| Trae Work 多账号切换（快照桥 + 6 层设备指纹重置） | ✅ 已实现 |
| Trae Work IDE 签到（auto_checkin.py + Checkin 页 + schtasks） | ✅ 已实现 |
| API 网关（OpenAI 兼容，X-Credit-Type: work\|ide 路由，work_credit 积分池在 `feat/work-credit-pool` 分支） | ✅ 分支已提交 |
| Credits 页积分趋势图（IDE 208 / Work 209） | ✅ 已实现 |
| device_proxy MITM 基础设施 | ✅ 已实现 |
| NDJSON 事件管线、账号冷却、桌面通知 | ✅ 已实现 |

---

## 1. 功能增强点总表（按主题分组）

### A. 账号管理与切换

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-01 | 安装位置自动识别 `app_locate` | 三级探测：注册表卸载键 → 默认路径 → 进程反查，返回 `{exe, userDataDir, version}`；豆包/Trae CN/WorkBuddy 共用 | doubao §1.3、workbuddy §2.1 | 1 天 | P0 |
| F-02 | WorkBuddy 账号切换 | auth 文件快照 + 用户数据双层恢复（L1 `workbuddy-desktop.info` / L2 `user-<uid>` / L3 session cookies）；切换后轮询 `account-snapshot.json` 确认；客户端运行时互斥（文件监听/进程检测防写冲突） | workbuddy §2.2 | 2 天 | P0 |
| F-03 | Trae CN 账号切换移植 | PS 桥参数化 `-AppKind Work\|Ide`；快照清单增量（storage.json iCubeAuthInfo、machineid、aha/、vscdb、Network/）；`data/trae_ide_profiles/` 隔离 | doubao §3.1 | 1~2 天 | P0 |
| F-04 | WorkBuddy 多账号池入库 | `workbuddy_accounts.json`（.gitignore）：uid/昵称/手机掩码/editionType/token 快照时间/refreshToken；账号 id 用 token 哈希稳定生成（`wb-` 前缀）；auth 导入兼容多种嵌套字段链 | workbuddy §2.2、oss §1.2/1.6 | 1 天 | P0 |
| F-05 | 豆包账号切换（方案 A 目录级快照） | 白名单快照（Cookies/Local State/leveldb/Session Storage/DoubaoStorage/saman_*）→ `data/doubao_profiles/<uid>/`；schemaVersion + 恢复前校验；优雅关停 + NDJSON 事件 | doubao §2.1 | 3~4 天 | P1 |
| F-06 | CodeBuddy CLI 切号桥 | 维护 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN`（绕过 apiKeyHelper Windows 坑）；与 App 账号库共用但当前账号独立 | oss §1.5 | 1 天 | P2 |
| F-07 | 豆包 cookie 级热切换（方案 B，二期） | v10/DPAPI 解密 sessionid → 账号池管理 → 进程内重写 Cookies 表；依赖开源链路（2025doubao-free-api 等验证 sessionid 即凭证） | doubao §2.2 | 1~2 天 | P3 |
| F-08 | TRAE SOLO CN / Trae CN 多账号自动发现 | 扫描 `%APPDATA%\Trae CN` 与 `%APPDATA%\TRAE SOLO CN` 的 storage.json 登录账号列表 | oss §6.1（dsh-connect-trae） | 1 天 | P2 |

### B. 会话保存与凭证续期

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-09 | WorkBuddy token 续期 | 首选 `POST /v2/plugin/auth/token/refresh`（X-Refresh-Token 头，已实测）；备选 Keycloak 原生端点；**惰性刷新**（临期 <24h 才刷）+ 每账号互斥锁（防旧 refresh token 覆盖新 token）+ schtasks 每周兜底；失败标 `needs_relogin` + 通知 | workbuddy §2.3、oss §1.2 | 1~2 天 | P0 |
| F-10 | Token 保活双源化 | 工具侧维护凭证副本（带 version），与桌面 App auth 文件"**谁新用谁**"（expiresAtMs 更晚者胜出）——彻底规避写冲突；落盘用原子写 + 文件锁 | oss §1.2（dsh auth.ts） | 1 天 | P1 |
| F-11 | 豆包会话续期 | `sid_guard` 30 天滑动续期：schtasks 每日用 cookie 调轻量已登录接口 → 抓 Set-Cookie 回写 `doubao_accounts.json`；过期标记 + 桌面通知；bd_sso 手动兜底 | doubao §2.3 | 1~2 天 | P2 |
| F-12 | Trae CN 会话续期 | 快照即保存；客户端内置 refresh + 现有 JWT refresh 机制复用；无 refresh_token 账号走"到期检测 + 提醒重新 OAuth" | doubao §3.2 | 0.5 天 | P0 |
| F-13 | 到期日历 | 各账号 `expiresIn/refreshExpiresIn` 换算绝对时间入库，UI 统一展示"到期日历" + 到期前提醒 | workbuddy §2.3 | 1 天 | P1 |
| F-14 | 环境重置 / 彻底登出 | antigravity 的 **16 项认证残留清理清单**（vscdb 认证 key、Tencent-Cloud.coding-copilot 缓存、marker、vscdb.backup 等）+ Keycloak logout 端点；用于"彻底登出/多账号隔离/环境重置" | oss §5.4 | 1~2 天 | P2 |

### C. 签到与积分增值

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-15 | WorkBuddy 一键签到 | `workbuddy_checkin.py`：状态查询（`checkin-activity-status`，旧路径回退）→ 执行（空体 `{}`）→ `code:10001`/已签按成功容错 → 401 刷新重试一次 → `--json-stream` NDJSON；不打印任何 token | workbuddy §2.5 | 2 天（含集成） | P0 |
| F-16 | 签到调度增强 | 每日 09:00/21:00 双时段（workbuddy2api schema）；token 保活独立开关（keepalive_hours=[22]）；签到冷却复用 `account_cooldowns.json` | oss §1.6/5.1、workbuddy §2.5 | 0.5 天 | P1 |
| F-17 | 成长中心自动化（★纯增量积分） | `/v2/activity/growth/*` 全端点已实测：**Buddy 旅行**（status/config/depart/claim）、**盲盒**（chances/draw）、**任务领奖**（tasks/accept）、能量余额、连签天数；识别非签到季与 401/403 过期 | oss §1.3（88lin 独家） | 2 天 | P1 |
| F-18 | UI 坐标点击签到兜底 | 无 API可用时的最后手段（workbuddy-checkin 的 PS 坐标模拟思路），优先级最低 | oss §0（仓库 6） | 1 天 | P3 |
| F-19 | 失败通知渠道扩展 | 桌面通知之外接入企业微信 / Server酱（社区同款做法） | workbuddy §2.5 | 0.5 天 | P2 |

### D. 积分余额与用量展示

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-20 | WorkBuddy 余额展示 | 云端积分三件套（summary/paid/free-packages，`X-Client-Platform: web`，origin 按 token 域路由 workbuddy.cn/codebuddy.cn）+ 旧接口回退（`ProductCode: p_tcaca`）；**免 MITM**（端点已被开源实现验证）；宽容解析（6 种嵌套路径 + 容量字段链式取值） | workbuddy §2.4、oss §1.4 | 1~2 天 | P0 |
| F-21 | 本地 quota API 兜底 | 客户端运行中时 `GET 127.0.0.1:<port>/api/v1/quota`；端口发现 = 扫 `~/.workbuddy/*.port` + 常见端口段 + 响应特征确认；余额查询 ≥5min 缓存 | workbuddy §2.4 | 0.5 天 | P2 |
| F-22 | 多账号余额聚合 + 趋势图 | Credits 页新增 WorkBuddy/豆包 Tab：余额大字 + 月度消耗趋势 + 各账号余额对比条形图；`DeductionEndTime` 判定"7 天内到期"提醒 | workbuddy §2.4 | 1 天 | P1 |
| F-23 | Trae CN 余额展示 | 直接复用现有 208/209 API（同域名同鉴权头，零逆向）；Accounts 页双字段 ideCredits/workCredits | doubao §3.3 | 1 天 | P0 |
| F-24 | 豆包余额展示 | MITM 抓包固化会员额度接口（`--proxy-server` 注入 Chromium 网络栈，关键词过滤 membership/entitlement/quota）；降级 = 网页内嵌注入 / 缓存值标注时间 | doubao §2.4 | 2~3 天 | P2 |
| F-25 | 官方用量统计 | `POST /billing/meter/get-user-request-usage` 拉 usageToday/usage7Days/usageThisMonth | oss §1.1（workbuddy-switch official_usage.rs） | 1 天 | P2 |
| F-26 | 本地 token 统计 | 解析 `~/.workbuddy/projects` + `~/.codebuddy/projects` 的 JSONL 事件流（input/output/cacheRead/cacheWrite/cacheHitRate），按模型/项目/会话三维聚合，days∈{7,30,90} | oss §5.3 | 1~2 天 | P2 |
| F-27 | 积分用量快照回退 | 本地 total/remaining 时序快照 + 签到日志 → 推导每日用量窗口（官方用量不可用时） | oss §5.3（credit_usage.rs） | 1 天 | P3 |

### E. API 暴露与网关升级（对接 workbuddy2api 全套）

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-28 | 网关上游接入 WorkBuddy | 对话上游 `POST copilot.tencent.com/v2/chat/completions`（只回 SSE，非流式本地聚合）；模型目录 `{chatBase}/console/enterprises/personal/models`；UA 伪装 `CLI/2.63.2 CodeBuddy/2.63.2` | oss §1.1 | 2 天 | P0 |
| F-29 | 账号池调度引擎 | 三因子加权随机选号（credits×10 + 闲置补偿 + 成功率×3 → Top5 短名单二次加权）；熔断器（hard_credit 冷却到次日 + 04:00 自动恢复探测；soft_rate 60s） | oss §1.6/5.1 | 2~3 天 | P1 |
| F-30 | 请求规范与改写层 | 请求头铁律：Origin/Referer 必带、缺省字段 `X-No-*` 占位、**chat 请求绝不带 X-Refresh-Token**（红线）；请求体：强制 stream、`tool_choice` string 化（对象 400 code=11101）、effort 按模型档位降级；Claude Code 指纹清洗（可开关） | oss §5.1 | 1~2 天 | P1 |
| F-31 | 会话粘性路由 | conversationId/元数据键 → 账号绑定；双段分配（先空闲账号哈希再全池）；TTL 30m 滚动续期；TOCTOU 写锁 re-check | oss §5.1 | 1 天 | P2 |
| F-32 | 网关运维接口 | `/v1/models`、`/status`（每账号画像）、`/healthz`（无健康账号 503 可接 LB）；请求级日志（seq/TTFB/uid/tokens/latency） | oss §1.6 | 1 天 | P1 |
| F-33 | 错误三态分类 | `RETRY_SAME`（502/503/超时→同号重试 1 次）/ `SWITCH_KEY`（401/403/429→换号）/ `FATAL`（400 上下文超限→终止）；硬编码标记词表（积分不足/insufficient credit 等） | oss §1.1/§5.4 | 0.5 天 | P1 |
| F-34 | 代理协议工程化 | 模型级冷却渐进退避 10→20→40s；SSE keep-alive 15s；首字超时 10s 可故障转移；客户端断连 `_drain_upstream` 保 usage 完整；健康检测 5min+抖动 | oss §5.4（antigravity） | 1~2 天 | P2 |
| F-35 | `ck_xxx` API Key 模式 | API Key 直连 billing 接口（账号导入门槛更低）；子 Key 体系：上游 Key 与对外子 Key 分离、限可用上游、专一/临期优先两模式、按日统计 | oss §1.6（antigravity） | 2 天 | P2 |
| F-36 | Global 区支持 | `domain` 含 `.workbuddy.ai` → 全部走 `www.workbuddy.ai`（chat+billing 同域） | oss §1.1 | 0.5 天 | P3 |

### F. Harness（DSH）/ Codex / MCP 生态接入

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-37 | DSH provider（WorkBuddy 侧） | 参照 dsh-workbuddy-connect 写 pi-ai provider：模型元数据透传（上下文/maxTokens/inputModalities/supportedEfforts/倍率/促销徽章）；**15 模型静态目录兜底 + 启动动态替换**；loopback shim + 心跳；能力声明永远读上游勿硬编码 | oss §2/§5.2 | 2~3 天 | P2 |
| F-38 | Trae Work → DSH（不自研） | 直接安装 `dingminhua/dsh-connect-trae`（Trae 模型进 DSH + 多账号切换 + Work/通用积分只读面板）；产品化多账号时参照其 storage.json 发现 + loopback shim 设计 | oss §6.1 | 0（装即用） | P1 |
| F-39 | Trae Work API 暴露 | 参照 `@casually/dsh-trae-api`：解密 storage.json 认证 → OpenAI 兼容 `/v1`（+Anthropic 适配思路），与本项目 P0 网关同构合并实施 | oss §6.1 | 1~2 天 | P1 |
| F-40 | Codex 后端转换器（WorkBuddy→Codex） | 复用 `tonny0812/workbuddy2api` converter：`/v1/responses` 投影（wire_api=responses）+ Anthropic `/v1/messages` + OpenAI 兼容三协议一份；`--desensitize` 脱敏防审核、失败自动退回紧凑模式 | oss §6.3 | 2~3 天 | P2 |
| F-41 | trae2codex 转换器 | Trae 上游无 Responses API，自建转换层（复用 F-40 投影逻辑换上游适配）；社区暂无现成实现，属空白机会 | oss §6.2 | 3 天 | P3 |
| F-42 | workbuddy-mcp 模式 | 把 WorkBuddy（驱动 codebuddy-code CLI）注册为 Codex/Claude Code/Cursor 的 MCP 工具（`run_workbuddy_task`）；`WB_SKIP_PERMISSIONS` 可控 | oss §6.3 | 1~2 天 | P3 |
| F-43 | CC Switch 协同 | 用户本机已用 CC Switch 管理 Codex/CC 多 provider（15721 端口实测）；本项目产出"账号池视角"的转换端点并注册进 CC Switch 配置，不自建切换器 | oss §6.4/6.5 | 0.5 天 | P2 |
| F-52 | WorkBuddyProxy 模式（WorkBuddy 驾驶舱 + Codex 执行器） | Electron 本地代理：WorkBuddy 走 OpenAI 兼容接口 → 代理持 Codex OAuth token（`%APPDATA%\WorkBuddyProxy\config.json`）→ 调 Codex 后端；与 F-40 方向相反（把 Codex 订阅接进 WorkBuddy 生态），社区已有方案佐证 | oss §6.3 | 2 天 | P3 |
| F-53 | DSH Codex provider 参考定位 | `dsh-codex-connect`（ChatGPT 订阅→DSH：Fast Mode 1.5×、5h/每周双配额窗口显示）——不属本项目账号管理范围，仅作 DSH 插件能力天花板与 UI 配额展示的参考 | oss §6.3 链路 B | 参考 | — |

### G. 会话数据管理（WorkBuddy 独有数据模型）

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-44 | 会话备份/恢复 | 三件套模型：`projects/{ws}/{cid}.jsonl` 正文 + `workbuddy.db` sessions 元数据 + `edge-sync-mapping-v2.db`（`convmsg:{uid}` 云端归属）——备份恢复缺一不可；先做备份再谈复制 | oss §1.4 | 1~2 天 | P1 |
| F-45 | 会话复制/迁移 | 新 id 复制算法：读源 jsonl → 替换 sessionId 为新 UUID → 写目标 projects → db 插行 → edge_sync_mapping 注册 `convmsg:{目标uid}`；复制前 `backup_workbuddy_db` | oss §1.4 | 2 天 | P2 |
| F-46 | 账号库导入导出 | JSON 格式 preview/merge（按 token 去重追加/更新）/按索引导入，纯逻辑可单测 | oss §5.3（export_import.rs） | 1 天 | P2 |

### H. 基础设施与工程增强（跨应用共用）

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-47 | 进程管理增强 | Windows 关闭三级策略：映像名枚举（排除 crashpad helper + 工具自身）→ `taskkill /T` 树杀宽限 8s → `/F` 强杀 → 人工介入；启动 `CREATE_NO_WINDOW` 静默拉起；exe 路径持久化兜底 | oss §5.3 | 1 天 | P1 |
| F-48 | 快照桥参数化 | `trae-switch-bridge.ps1` 支持 `-AppKind Work\|Ide\|Doubao`，快照白名单/数据目录/事件管线全部表驱动——三应用共用一套桥 | doubao §3.1、workbuddy §5 | 1 天 | P0 |
| F-49 | 响应宽容解析工具 | `dig()` 信封解包（data/result/resp/response 任意层包裹递归查找）+ 6 种嵌套路径兼容——所有积分/签到接口解析层统一采用 | oss §5.5、workbuddy §1.4 | 0.5 天 | P1 |
| F-50 | OAuth 扫码登录工具 | `auth/state?platform=CLI` → 浏览器扫码 → `auth/token?state=` 轮询 → `login/account` 取 uid；每流程独立 cookie jar 防串号；无 PKCE | oss §5.1、workbuddy §1.4 | 1 天 | P2 |
| F-51 | 活动信息展示 | `/v2/activity/banner`（活动横幅）+ `get-payment-type` + `get-dosage-notify`——UI 活动提醒与支付类型展示（低优先附加） | oss §1.1 | 0.5 天 | P3 |

---

## 2. 建议路线图（分四批，映射三份文档的实施计划）

| 批次 | 主题 | 包含功能点 | 预估 |
|---|---|---|---|
| **批次 1：WorkBuddy 快赢 + Trae CN 移植**（~1.5 周） | 对齐 workbuddy §3 与 doubao §4 的 P0~P3 | F-01、F-02、F-03、F-04、F-09、F-12、F-15、F-20、F-23、F-48 | 9~11 天 |
| **批次 2：API 暴露升级 + 积分增值** | 对齐 oss §3 P0/P1 | F-28、F-29、F-30、F-32、F-33、F-16、F-17、F-22、F-10、F-13、F-49、F-47 | 11~13 天 |
| **批次 3：会话与数据管理** | 对齐 oss §3 P1/P2 | F-44、F-45、F-46、F-25、F-26、F-31、F-34、F-06、F-08、F-14、F-50 | 10~12 天 |
| **批次 4：生态与豆包** | 对齐 oss §3 P2/P3 + doubao P2~P5 | F-05、F-11、F-24、F-37、F-39、F-40、F-43、F-21、F-19、F-35 | 12~15 天 |
| 远期/机会项 | 视官方接口与生态演进 | F-07、F-18、F-27、F-36、F-41、F-42、F-51、F-52 | — |

> 实施顺序依据：**WorkBuddy 端点全部有可运行开源佐证（风险最低、见效最快）→ Trae CN 零逆向移植 → 豆包需新模块与抓包（二期）**。DSH 侧 F-38 为零成本装即用，可随批次 1 顺手交付。

---

## 3. 遗漏审查记录（2026-09-07 逐节核对）

对照三份源文档逐节核对，确认以上 51 项已完整覆盖：

| 源文档 | 核对范围 | 结果 |
|---|---|---|
| workbuddy-switch-plan.md | §1 布局/登录态 → F-04 基础；§1.4 端点全表/统一头/域名路由/五项工程设计 → F-09/F-15/F-20/F-49/F-50；§2.1~2.5 → F-01/02/04/09/15/20/21/22；§3 计划 → 批次 1；§4 风险（缓存间隔、写冲突、不硬编码奖励）→ F-21/F-02/F-20 注记；§5 复用关系 → F-48/批次表 | ✅ 无遗漏 |
| doubao-trae-switch-plan.md | §1.1~1.2 侦察基础 → F-05/F-03 背景；§1.3 三级探测 → F-01；§2.1~2.4 → F-05/07/11/24；§3.1~3.3 → F-03/12/23；§4 P0~P5 → 批次表；§5 合规 → 各条注记 | ✅ 无遗漏 |
| oss-ecosystem-research.md | §1.1 对话上游/域名路由/请求头/错误分类 → F-28/30/33/36；§1.2 双源凭证/原子写/路径候选 → F-10/F-01（跨平台路径）；§1.3 签到+成长中心全端点 → F-15/17；§1.4 会话三件套/复制/导出导入 → F-44/45/46；§1.5 CLI 切号/轮换 → F-06 + **积分到期轮换**（并入 F-06 项内：冷却期+到期差异阈值防抖）；§1.6 选号/熔断/调度/子 Key → F-29/16/35；§2 DSH 机制 → F-37；§3 路线 → 批次表；§4 风险（UA 跟踪/域名勘误/许可）→ F-28/33 注记；§5.1 OAuth 设备流/铁律/改写/清洗/粘性/schema → F-50/30/31/29；§5.2 目录兜底/图片教训 → F-37；§5.3 进程/统计/快照/导入导出 → F-47/26/27/46；§5.4 考古/清理清单/代理协议 → F-14/34；§5.5 极简头/dig → F-49；§6.1~6.5 → F-38/39/40/41/42/43 | ✅ 无遗漏 |

**审查补记**（首轮整理遗漏、审查时补入的项）：
1. **积分到期自动轮换**（原仅记 CLI 切号桥，漏了 rotate.rs 的自动轮换与防抖约束）→ 并入 F-06；
2. **跨平台凭证路径候选**（macOS/Linux/WSL 路径 + 环境变量覆盖）→ 补入 F-01 说明；
3. **F-16 签到双时段与保活独立开关**、**F-33 错误三态**、**F-49 dig() 宽容解析**为审查时从 oss §5 细节中提升为独立功能点；
4. **F-51 活动横幅等三个低频端点**为审查时补入；
5. **F-52 WorkBuddyProxy 模式**、**F-53 dsh-codex-connect 参考定位**为审查 oss §6.3 时补入（首轮漏了 Codex 反向接入与配额窗口参考）；
6. **豆包 bd_sso 多账号载体 / LARK 打通 / saman 隔离开关**等侦察细节为背景信息，已并入 F-05/F-11 说明，不单列功能点。

## 4. 风险与合规提示（继承三份文档）

- 全部端点为逆向/实测所得（多仓库多语言交叉一致），腾讯可随时变更：接口层独立模块 + 失败明示 + 不硬编码奖励数额。
- 凭证等同密码：账号池文件入 `.gitignore`，日志/UI 零明文；豆包 sessionid 同理。
- UA 伪装版本号（`CLI/2.63.2`）需跟踪官方升级。
- 多账号池用于 API 服务时不得演变为对外售卖转租；仅管理本人合法持有的账号。
- 参考仓库均为 MIT（dsh-codex-connect 为 Apache-2.0），借鉴代码保留版权声明。
