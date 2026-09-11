# Buddy（WorkBuddy / CodeBuddy）需求产品设计文档

> **文档版本**: v1.3 · 2026-09-11
> **v1.3 变更**: 批次 5 立项（开源生态价值点纳入）：① 依据 `docs/tmp/oss-ecosystem-value-analysis.md`（升级 v1.1，✅ 标记同步）新增 J 节补充需求 **F-61~F-66**（四段模型路由管线 / reasoning_content 思考链透传 / 生图双端点 / 网关工具代执行 / 协议细节补强 / CLI 多账号环境隔离评估）；② 任务清单新增「批次 5 · 生态吸收与网关增强」——顺延项 F-37/F-43/F-21 转正式任务，F-42/F-52/F-41 转机会项；③ §5.8 参照表新增 #11~#16，§7.2 参考仓库新增 3 项；④ 实施顺序结论补批次 5。
> **v1.2 变更**: ① §3.7 UI 设计对齐本应用既有「Trae 页面布局」——启用 Sidebar 底部应用切换 Tab 的 Buddy 项（代码已预留），Buddy 采用与豆包同款**应用级子导航**（概述/账号管理/签到与成长/积分与统计/环境配置 五页，buddy-* 视图），替换 v1.1「散落进 Trae 各页做分区/Tab」的方案，并补齐组件复用清单与版式规范；② 吸收 2026-09-10 开源生态调研（`docs/tmp/oss-ecosystem-value-analysis.md`）：F-29/F-30/F-31/F-33 就地升级（P2C 调度、账号五态机、审核模板黑名单最小改写、会话粘性双模式、分级重试表），§5.5 避坑清单新增 2 条，§5.6 补按模型族思考参数映射，新增 §5.8 调度与粘性工程参照，§7.2 新增 2 个参考仓库。
> **v1.1 变更**: 结合开源项目 [changexbc/workbuddy-switch](https://github.com/changexbc/workbuddy-switch)（v0.3.1 运行截图 5 张 + 本地克隆件源码核对）补充——① 新增补充需求 F-54~F-60（账号双态卡片/启动自动补签/积分包明细/Token 统计增强/按模型积分排行/CLI 轮换四重防护/聚合迁移入口）；② §3.7 UI 设计扩写为逐页可落地规格（信息架构 + 字段级组件清单）；③ §3.10 CLI 轮换由双约束扩为五重防护；④ 新增 §5.7 统计与调度实现细节（源码级）；⑤ 任务清单批次 1/3 相应扩项。
> **产品归属**: AI Work 助手（ai-work-assistant，当前 v3.2.7）
> **定位**: WorkBuddy 应用全量接入的产品需求与设计总纲——需求、功能设计、任务清单、关键实现技术方案
> **来源文档**: 原 `workbuddy-switch-plan.md`（端点与方案）、`oss-ecosystem-research.md`（6 仓库源码级调研）、`product-enhancement-inventory.md`（53 项全量盘点，F-xx 编号沿用）、`future-roadmap.md`（排期基线）已于 2026-09-10 文档精简中合并删除——本文已吸收其全部有效内容，现为**自包含**文档
> **范围声明**: 本文档是 WorkBuddy 接入的**唯一执行蓝本**；上游研究文档只读不改，实现以本文为准。仅管理本人合法持有的账号，不做对外售卖转租。

---

## 一、产品概述

### 1.1 背景

AI Work 助手已实现 Trae Work / Trae CN 双应用的「多账号签到 + 登录态切换 + 设备隔离 + API 网关」一站式工作台，并完成豆包的快照切换 / 保活 / 额度框架。WorkBuddy（桌面端 + CodeBuddy CLI，登录态为明文 JSON + 标准 Keycloak OIDC token）是第三个、也是**端点全部有可运行开源佐证、接入风险最低**的目标应用。

### 1.2 产品目标

| 目标 | 衡量指标 |
|---|---|
| 四应用统一工作台 | WorkBuddy 账号与 Trae/豆包同池管理，一处切换、一处签到、一处看积分 |
| 端到端自动化 | 识别 → 入池 → 切换 → 续期 → 签到 → 成长中心 → 余额展示，全程免手动脚本 |
| API 资产化 | WorkBuddy 订阅积分通过本地网关暴露为 OpenAI / Anthropic / Codex 三协议可用算力 |
| 低风险渐进 | 分 4 个批次交付，每批独立可验收；接口层独立模块，上游变更只改一处 |
| 安全合规 | 凭证零明文输出、账号池文件永不入库、客户端运行时互斥保护 |

### 1.3 目标用户与核心场景

**用户**：持有多个 WorkBuddy/CodeBuddy 账号（免费版 500 积分/月，专业版 2000 积分/月）的重度用户；已使用本工具管理 Trae/豆包账号的存量用户。

**核心用户故事**：

| # | 用户故事 | 对应能力 |
|---|---|---|
| US-1 | 我有 3 个 WorkBuddy 账号，希望像 Trae 一样一键切换登录，不用登出登入 | M2 账号切换 |
| US-2 | accessToken 60 天 / refreshToken 90 天过期，我不想到期才发现签到失败 | M3 续期与保活 |
| US-3 | 每天手动签到 + 领成长中心奖励太繁琐，希望定时自动完成 | M4 签到与成长中心 |
| US-4 | 我想一眼看到每个账号还剩多少积分、哪些快到期 | M5 余额与用量 |
| US-5 | 我想把 WorkBuddy 订阅当本地 API 用，积分快耗尽时自动换号 | M6 网关上游 |
| US-6 | 我用 Codex CLI / Claude Code / DSH，希望直接吃 WorkBuddy 模型 | M7 生态接入 |
| US-7 | 换机/重装前，我想备份对话记录与账号，之后完整恢复 | M8 会话数据 |

### 1.4 可直接复用的既有资产（已实现，不重复开发）

| 资产 | 位置 | 在 WorkBuddy 接入中的复用方式 |
|---|---|---|
| F-01 `app_locate` 四应用三级探测 | `src-tauri/src/commands/env.rs` | WorkBuddy 档案已就绪，直接返回 `{exe, authDir, dataDir, version}` |
| F-48 快照桥参数化（`-TargetApp WorkBuddy`，SnapshotLayout=`authfile`） | `src-ps/trae-switch-bridge.ps1` | 补 authfile 布局的快照/恢复实现即可 |
| F-47 进程管理三级关闭 | `commands/process.rs` | WorkBuddy.exe 优雅关闭/树杀/强杀直接复用 |
| F-49 `dig()` 宽容解析 | `fs_utils.rs` | 积分/签到响应解析统一采用 |
| F-46 账号库导入导出（预览+按索引） | `commands/accounts.rs` | 扩展 WorkBuddy 账号结构 |
| API 网关骨架（OpenAI+Anthropic 双协议、pool.rs 账号池、payload/sse/auth/models_sync） | `src-tauri/src/api_server/` | WorkBuddy 作为**新上游类型**接入，调度/熔断/协议输出全复用 |
| schtasks 定时任务封装 + GBK 编码方案 | `commands/misc.rs` | 签到/续期定时任务直接注册 |
| NDJSON 事件管线（checkin-progress / switch-progress） | store.ts 事件归约 | WorkBuddy 事件复用同一管线 |
| 桌面通知、冷却状态机、设置页框架 | 既有 | 直接挂接 |

---

## 二、需求清单

### 2.1 功能需求（编号沿用 F-xx，标注来源与批次）

#### A. 账号管理与切换（P0）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-02 ✅ | WorkBuddy 账号切换：auth 文件快照 + 用户数据双层恢复（L1 必选 / L2 体验 / L3 可选） | 切换后重启客户端登录身份变为目标账号；轮询 `account-snapshot.json.uid` 确认；客户端运行中先三级关闭；全程 NDJSON 进度 |
| F-04 ✅ | 多账号池入库：`workbuddy_accounts.json`（.gitignore），uid/昵称/手机掩码/editionType/凭证/快照时间 | 账号 id = `wb-` + token 哈希（同 token 稳定同 id）；auth 导入兼容 `account.uid/auth.accessToken` 等多种嵌套形态；凭证不在 UI 明文 |
| F-50 ✅ | OAuth 扫码登录工具（P2 批次 3）：`auth/state` → 浏览器扫码 → `auth/token` 轮询 → `login/account` 取 uid | 每流程独立 cookie jar 防串号；扫码成功账号自动入池 |

#### B. 会话与凭证续期（P0）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-09 ✅ | token 续期：首选 `POST /v2/plugin/auth/token/refresh`（`X-Refresh-Token` 头）；备选 Keycloak 原生端点 | 惰性刷新（临期 <24h 才刷）+ **过期前主动刷新**（v1.2 吸收 trae2api-web：每日兜底任务对临期账号不等 401、主动刷新——「过期前刷新」比「过期后补救」更稳）；每账号互斥锁；失败标 `needs_relogin` + 原因 + 桌面通知；schtasks 每周兜底 |
| F-10 ✅ | Token 保活双源化：工具侧凭证副本与桌面 auth 文件「谁新用谁」（`expiresAtMs` 晚者胜出） | 原子写 + 文件锁；任何一方刷新后另一方读取时自动采纳新凭证，互不覆盖 |
| F-14 ✅ | 环境重置/彻底登出（P2）：16 项认证残留清理清单 + Keycloak logout | 清理项可勾选预览；执行前二次确认；清理后客户端回到未登录态 |

#### C. 签到与积分增值（P0/P1）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-15 ✅ | 一键签到：状态查询（旧路径回退）→ 执行 → `code:10001`/已签容错 → 401 刷新重试一次 | NDJSON 事件；零 token 输出；冷却复用 `account_cooldowns.json` |
| F-16 ✅ | 签到调度增强（P1）：每日 09:00/21:00 双时段 schtasks；token 保活独立开关 | 定时可注册/查询/卸载；失败桌面通知 |
| F-17 ✅ | 成长中心自动化（P1）：Buddy 旅行（状态/出发/领奖）、盲盒（次数/抽取）、任务领奖、能量、连签天数 | 全流程可单独开关；纯增量积分；奖励数额以接口返回为准不硬编码 |
| F-18 ✅ | UI 坐标点击签到兜底（P3）：无 API 可用时的最后手段 | 仅手动触发，默认关闭 |

#### D. 积分余额与用量（P0/P1/P2）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-20 ✅ | 余额展示：云端积分三件套（summary/paid/free-packages）+ 旧接口回退 | 免 MITM；宽容解析（6 种嵌套路径 + 容量字段链式取值）；`DeductionEndTime` 7 天内到期提醒；查询 ≥5min 缓存 |
| F-22 ✅ | 多账号余额聚合 + 趋势图（P1）：buddy-credits 页 | 余额大字 + 月度消耗趋势 + 各账号对比条形图 |
| F-21 | 本地 quota API 兜底（P2）：`GET 127.0.0.1:<port>/api/v1/quota` | 端口发现 = 扫 `~/.workbuddy/*.port` + 端口段探测；按 `remaining` 特征确认 |
| F-25 ✅ | 官方用量统计（P2）：`get-user-request-usage` 日/周/月用量 | 与本地 token 统计并列展示 |
| F-26 ✅ | 本地 token 统计（P2）：解析 `~/.workbuddy/projects` + `~/.codebuddy/projects` JSONL | input/output/cacheRead/cacheWrite/cacheHitRate，按模型/项目/会话聚合，days∈{7,30,90} |
| F-27 ✅ | 积分用量快照回退（P3）：本地时序 + 签到日志推导每日用量 | 官方用量不可用时自动切换数据源并标注 |
| F-51 ✅ | 活动信息展示（P3）：`/v2/activity/banner` + payment-type + dosage-notify | 低频附加展示 |

#### E. API 暴露与网关升级（P0/P1/P2）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-28 ✅ | 网关上游接入：对话上游 `POST copilot.tencent.com/v2/chat/completions`（CN 区），只回 SSE → 非流式本地聚合 | UA 伪装 `CLI/2.63.2 CodeBuddy/2.63.2`；模型目录接口同步进 `api_models.json`；Global 区（domain 含 `.workbuddy.ai`）全走 `www.workbuddy.ai`（F-36） |
| F-29 ✅ | 账号池调度引擎（P1，v1.2 升级）：三因子加权随机选号（credits×10 + 闲置补偿 + 成功率×3 → Top5 二次加权）+ 熔断；**账号五态机**（v1.2 吸收 antigravity-tools：`Available` / `QuotaProtection`（配额<阈值自动剔除）/ `RateLimited`（429 指数退避自动过期恢复）/ `Forbidden`（403 封禁需人工介入标注）/ `ProxyDisabled`），替换现有「冷却/禁用」二元态；**P2C 备选策略**（Power-of-Two-Choices 随机选二取优，作为三因子加权的可切换策略对比吸收） | hard_credit 冷却到次日 04:00 自动恢复；soft_rate 60s；100ms 窗口去重防惊群；五态迁移全部落 `/status` 账号画像 |
| F-30 ✅ | 请求规范与改写层（P1，v1.2 升级）：Origin/Referer 必带、`X-No-*` 占位、强制 stream、`tool_choice` string 化、effort 降级、指纹清洗（可开关）；**指纹清洗 v2 = 精确匹配黑名单 + 最小改写**（v1.2 吸收 workbuddy-cliproxy：腾讯审核把 Claude Code 两句固定 system 模板**逐字入黑名单**，命中即拒答、任何一字改动即绕过——清洗层由「正则剥离为主」升级为「模板句映射表精确匹配 + 最小改写」（CLI→CLI tool、Main branch→Default branch），**映射表外置可热更新**（cat-and-mouse，不硬编码进二进制） | **红线：chat 请求绝不携带 X-Refresh-Token**；违反任一规范的上游 400 类错误在联调清单中逐项验证；模板映射表随 UA 常量一并集中维护 |
| F-31 ✅ | 会话粘性路由（P2，v1.2 升级为**双模式**）：① 显式模式——conversationId → 账号绑定，双段分配（先空闲账号哈希再全池哈希）；② 指纹模式（v1.2 吸收 antigravity-tools）——无 conversationId 时对**前 3 条消息内容做 SHA256 取 6 位指纹** + 60s 时间窗锁定，同一会话恒落同一账号 | TTL 30m 滚动续期；写锁 re-check 防 TOCTOU；指纹模式注意：Buddy 上游 prompt cache 对代理流量**不生效**（§5.5 #10），指纹模式价值在会话一致性与上游侧缓存（若有），积分成本模型仍按冷启动估算，不做缓存命中承诺 |
| F-32 ✅ | 网关运维接口（P1）：`/v1/models`、`/status`（每账号画像）、`/healthz`（无健康账号 503） | 请求级日志（seq/TTFB/uid/tokens/latency） |
| F-33 ✅ | 错误三态分类（P1，v1.2 升级）：`RETRY_SAME` / `SWITCH_KEY` / `FATAL` + **分级重试策略表**（v1.2 吸收 antigravity-tools：429 → 优先解析 `Retry-After`，缺省线性退避 1/2/3s；503/529 → 指数退避；400 且含 `thinking.signature` → 固定 200ms 重试一次；其余 → 换号） | 硬编码标记词表（积分不足/insufficient credit/quota exceeded…）集中维护；重试表可配置，默认值按上表 |
| F-34 ✅ | 代理协议工程化（P2）：模型级冷却渐进退避 10→20→40s；SSE keep-alive 15s；首字超时 10s 故障转移；断连 `_drain_upstream` 保 usage | 健康检测 5min + 抖动 |
| F-35 ✅ | `ck_xxx` API Key 模式（P2）：API Key 直连 billing；子 Key 体系（限定上游、专一/临期优先两模式、按日统计） | 对外子 Key 与上游真实凭证分离 |

#### F. 生态接入（P2/P3）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-06 ✅ | CodeBuddy CLI 切号桥 + 积分到期自动轮换（P2）：维护 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN` | 自动轮换 = 切到「最早到期且仍有剩余」账号；防抖双约束（冷却期 + 到期差异阈值 min_gap_hours） |
| F-37 | DSH provider（P2）：15 模型静态目录兜底 + 启动动态替换 | 元数据透传 inputModalities/supportedEfforts/倍率/徽章；能力读上游勿硬编码 |
| F-40 ✅ | Codex 后端转换器（P2）：`/v1/responses` 投影 + Anthropic `/v1/messages` + OpenAI 兼容三协议一份 | `--desensitize` 脱敏、失败退回紧凑模式；Codex CLI `config.toml` 直配可用 |
| F-42 | workbuddy-mcp 模式（P3）：把 WorkBuddy 注册为 Codex/CC/Cursor 的 MCP 工具 | `WB_SKIP_PERMISSIONS` 可控 |
| F-43 | CC Switch 协同（P2）：把本项目转换端点注册进 CC Switch 配置 | 不自建切换器 |
| F-52 | WorkBuddyProxy 模式（P3）：WorkBuddy 驾驶舱 + Codex 执行器 | 与 F-40 方向相反，远期评估 |

#### G. 会话数据管理（P1/P2）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-44 ✅ | 会话备份/恢复（P1）：三件套 = `projects/{ws}/{cid}.jsonl` 正文 + `workbuddy.db` sessions + `edge-sync-mapping-v2.db` 云端映射 | 缺一不可；备份/恢复前校验客户端已关闭 |
| F-45 ✅ | 会话复制/迁移（P2）：新 id 复制算法（替换 sessionId → 写目标 projects → db 插行 → edge_sync_mapping 注册） | 复制前 `backup_workbuddy_db`；复制后云端可正常同步 |

#### H. 跨应用通用（随批次嵌入）

| 编号 | 需求 | 验收标准 |
|---|---|---|
| F-13 ✅ | 到期日历（P1）：各账号 token/积分到期绝对时间入库 + UI 日历 + 到期前提醒 | WorkBuddy/Trae/豆包共用一套 |
| F-19 ✅ | 失败通知渠道扩展（P2）：企业微信 / Server酱 | 桌面通知之外的可选渠道 |

#### I. 补充需求（v1.1，基于 workbuddy-switch v0.3.1 截图实测 + 源码核对）

| 编号 | 需求 | 说明 / 验收标准 | 预估 | 优先级 |
|---|---|---|---|---|
| F-54 ✅ | 账号卡片双态与在线状态 | 卡片式账号管理：登录源头像（QQ/微信）+ 昵称 + uid 掩码 + 在线/离线徽标 +「设为当前 / 设为备用」双态操作；当前账号卡片高亮、备用一键提升；对齐本项目「切换」语义（当前=本机 auth 生效账号） | 1.5 天 | P1 |
| F-55 ✅ | 自动签到配置化 + 启动自动补签 | ① 启用开关：应用启动时立即核验服务端签到状态，未签到账号自动补签（对齐 workbuddy-switch「启动时立即核验…自动补签」）；② 保活阈值 `keepalive_days`（天，0=每天无条件刷新全部带 refreshToken 账号）；③ 惰性刷新 `lazy_refresh_hours`（小时，默认 24）——刷新参数由硬编码 24h 改为配置化 | 1 天 | P1 |
| F-56 ✅ | 积分包明细弹窗 | 「查看全部积分包」弹窗：逐包展示 包名 / 剩余/总量 / 已用 / 到期日 / 进度条，滚动列表；数据源=积分三件套（paid/free-packages），包级 `DeductionEndTime` 直接入到期日历（F-13） | 1 天 | P1 |
| F-57 ✅ | Token 统计增强（F-26 升级） | ① 总览四指标卡：总 Token / 输入 / 输出 / **缓存命中率**（`cache_hit_rate = cache_read/(input+cache_read)`）；② Token 构成堆叠条（缓存读取/新增输入/输出/缓存写入 占比）；③ Token 与调用趋势**双轴图**（堆叠柱=四类构成，虚线=调用次数；模型筛选 + 今天/近7天/近30天/本月）；④ **年度活动热力图**（GitHub 风格，每日粒度，最近一年） | 1.5 天 | P2 |
| F-58 ✅ | 按模型积分分类排行 | 官方用量（F-25）+ 本地 token 统计按模型映射：四 KPI 卡（剩余/今日消耗/近7天/本月）+ 官方消耗堆叠柱（按模型）+ 模型排行（请求数/合计积分/占比+进度条，展示 Top8、其余计入合计）；口径标注「来自 WorkBuddy 官方请求用量」 | 1 天 | P2 |
| F-59 ✅ | CLI 自动轮换五重防护（F-06 升级） | 在原冷却期 + 到期差异阈值基础上，补齐 workbuddy-switch `rotate.rs` 完整防护：③ 到期紧迫阈值 `min_urgency`（目标到期剩余超过该值=都还早，不切）；④ 活跃保护 `active_guard`（CLI 最近对话 N 分钟内不切，防打断工作中会话）；⑤ 最小剩余积分 `min_remaining`（目标低于该值不切）；+ 检查间隔（分钟）配置 | 1 天 | P2 |
| F-60 ✅ | 「添加与迁移账号」聚合入口 | 账号页顶部聚合卡：OAuth 扫码添加（F-50）/ 导入本机账号（扫 auth 文件+CLI）/ 导入备份（账号库导入，F-46 扩展）/ 导出——四入口一键直达 | 0.5 天 | P1 |

> **源码核对要点**（克隆件 `%TEMP%\oss-research\workbuddy-switch`，commit bb46e90 · 2026-09-04 · v0.3.1）：`refresh.rs:127` 惰性刷新（剩余 < `lazy_refresh_hours` 才刷）与 `refresh.rs:149` 保活检查（每日一次，`keepalive_days<=0` 无条件刷全部）——F-55 的两个参数名与语义直接沿用；`rotate.rs::decide_target` 为纯函数（候选按 `urgency_key` 排序：到期越早越紧迫、无到期排最后 → 五重防护逐层过滤），**可纯逻辑单测**，移植时保持该形态；`token_stats.rs:46` 缓存命中率公式 + `:92-99` cache_read 别名链（`cache_read_input_tokens` 优先取正值，防 stale 0 掩盖 `prompt_cache_hit_tokens`，兼容嵌套 provider details）——F-57 解析层直接照抄。

#### J. 生态吸收补充需求（v1.3 批次 5 立项，源自 `docs/tmp/oss-ecosystem-value-analysis.md` v1.1 未吸收价值点）

| 编号 | 需求 | 说明 / 验收标准 | 优先级 |
|---|---|---|---|
| F-61 ✅ | 四段模型路由管线（P1） | 「任意模型名 → 上游真实模型」四级路由：① 别名静态映射（`wb_model_catalog.json`）→ ② 用户自定义正则 → ③ 系列通配（如 `claude-sonnet-*` → glm/hy 系列）→ ④ 后缀检测注入参数（`-thinking`/`-quality` 等注入思考/画质参数）；每级命中即止，全未命中回落 `/v1/models` 目录原名。来源：antigravity-tools（同栈平行实现实证） | P1 |
| F-62 ✅ | reasoning_content 思考链透传（P2） | 网关 OpenAI 输出透传上游思考链 `reasoning_content` 字段 + 「默认深度思考」开关（设置项，默认关）；Anthropic 协议侧映射为 thinking block。来源：Tom6814/WorkBuddy2API | P2 |
| F-63 ✅ | 生图双端点（P2） | `/v1/images/generations`（文生图）+ `/v1/images/edits`（图生图）投影至上游生图能力；上游不支持时明示报错不静默；与 F-61 路由管线联动（生图模型识别）。来源：Tom6814/WorkBuddy2API | P2 |
| F-64 ✅ | 网关工具代执行（P2） | 客户端下发上游不支持的工具（如 Codex 发 `type:"web_search"`）时代理侧代执行：搜索/页面读取 → 结果回喂上游 → 按原生 `web_search_call` 事件流返回——「上游不支持的工具调用在代理侧补齐」完整范式，跨上游通用。来源：muskke/trae-api-proxy v0.5.1 | P2 |
| F-65 ✅ | 协议细节补强（P1） | ① 连续同角色消息自动合并（上游要求消息交替，改写层在透传前合并）；② 单端口三协议靠 `anthropic-version` 头/路径双维度区分（防路径嗅探误判）；③ 后台任务（生成标题/摘要类短请求）识别并降级（低优先级账号/低成本模型）。来源：antigravity-tools | P1 |
| F-66 | CLI 多账号环境隔离（P3 评估项） | 每账号独立 `CODEX_HOME`/`CLAUDE_CONFIG_DIR`/`KIMI_CODE_HOME` 环境目录 + 全局同名变量剥离 + 「严格账号模式」（无激活账号即报错、不回落本机登录态）+ 接口返回一律脱敏；与 F-06 CLI 切号桥互补（写 token vs 隔目录），做 dsh/CC 多 CLI 场景扩展评估。来源：xiaolizi0v0/CliProxy | P3 |

### 2.2 非功能需求

| 维度 | 要求 |
|---|---|
| 安全 | `accessToken`/`refreshToken`/sessionid 等同密码：账号池文件入 `.gitignore`；日志/异常消息/UI 零明文输出（沿用「已加载(内容已隐藏)」模式）；凭证字段掩码展示 |
| 客户端互斥 | auth 文件被客户端启动时重写——**切换/续期/快照写入必须在客户端关闭窗口期执行**；文件监听或进程检测防写冲突；双源化（F-10）兜底 |
| 接口稳定性 | 全部端点为逆向/实测所得，腾讯可随时变更：接口层独立模块 + 域名双探测（codebuddy.cn / workbuddy.cn）+ 失败明示不静默重试 + 不硬编码奖励数额 |
| 频控 | 签到每日一次、续期惰性每周兜底、余额查询 ≥5min 缓存、健康检测 5min+抖动；不做批量注册/多开薅积分 |
| UA 伪装 | `CLI/2.63.2` 版本号需跟踪官方升级，集中为常量配置 |
| 零新增依赖 | Rust 侧沿用 ureq/serde/chrono；Python 侧仅标准库 + `cryptography`；借鉴代码保留 MIT 版权声明 |
| 工程规范 | 中文 strings.xml/前端文案规范、显式数据迁移、commit 前缀 [feature]/[fix] 分拆并附显式路径、`.workbuddy/` 不入库 |

---

## 三、功能设计

### 3.1 模块架构总览

```
┌────────────────────────── AI Work 助手 (Tauri) ──────────────────────────┐
│ 前端: Dashboard / Accounts / Checkin / Credits / ApiService / Settings    │
│        └─ Buddy 应用级子导航五页 buddy-*（本文 §3.7，Sidebar 应用 Tab 切换）  │
├──────────────────────────────────────────────────────────────────────────┤
│ Rust 层                                                                   │
│  commands/workbuddy.rs   ← 新增：账号池/切换/续期/签到/成长中心/余额/会话    │
│  workbuddy/mod.rs        ← 新增：WB 域逻辑（auth 读写/双源凭证/调度状态机）  │
│  api_server/             ← 扩展：新上游类型 WorkBuddyUpstream（§3.6）       │
├──────────────────────────────────────────────────────────────────────────┤
│ 辅助进程                                                                  │
│  src-python/workbuddy_checkin.py    ← 签到 + 成长中心（NDJSON）             │
│  src-python/workbuddy_credits.py    ← 积分三件套 + 用量 + token 统计        │
│  src-ps/trae-switch-bridge.ps1      ← 扩 authfile 布局快照/恢复            │
└──────────────────────────────────────────────────────────────────────────┘
```

### 3.2 M1 应用识别与环境（批次 1，≈0.5 天）

- `app_locate("workbuddy")` 已就绪：注册表卸载键 → `%LOCALAPPDATA%\Programs\WorkBuddy\WorkBuddy.exe` 默认路径 → 进程反查；返回 `authFile`（`%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`）与 `dataDir`（`~/.workbuddy`）。
- 新增 `workbuddy_env_check` 命令：客户端安装/运行状态、auth 文件存在性、`~/.workbuddy/storage/skeleton/account-snapshot.json` 可读性（当前登录 uid/昵称/editionType）。buddy-overview 概述页展示（§3.7.1）。

### 3.3 M2 账号管理与切换（批次 1，≈3 天）

**账号池**（`data/workbuddy_accounts.json`，入 `.gitignore`）：

```json
{
  "accounts": [{
    "id": "wb-<sha256(token)前12位>",
    "uid": "e5ea09b1-...", "nickname": "七点半", "phone_masked": "158****xxx",
    "edition_type": "free",
    "access_token_expires_at": 1780000000,
    "refresh_token_expires_at": 1783000000,
    "auth_saved_at": 1760000000,
    "needs_relogin": false, "relogin_reason": "",
    "group_id": "", "note": ""
  }]
}
```

**切换流程（authfile 布局，PS 桥 `-Action Switch -TargetApp WorkBuddy`）**：

1. 进程检测：`WorkBuddy.exe` 运行中 → 三级关闭（F-47 已就绪）。
2. 备份当前 `workbuddy-desktop.info` → `data/workbuddy_profiles/<uid>/auth.info`；客户端自留的历史快照（`workbuddy-desktop.<ts>.<pid>.<uuid>.info`）作交叉校验。
3. 写入目标账号 `auth.info`；按需恢复 L2（`~/.workbuddy/storage/user-<uid>*` 目录）与 L3（session Cookies，可选）。
4. 拉起 WorkBuddy.exe → 轮询 `account-snapshot.json.uid`（超时 30s）→ `switch-done` 事件。
5. **防误覆盖守卫**：写 auth 文件前检测当前客户端实际登录 uid，与预期不一致时拒绝回写来源槽（对齐豆包 ExpectedCurrentUid 守卫经验）。

**入池途径**：①「保存当前登录态」读 auth 文件；② auth 文件导入（兼容嵌套字段链）；③ OAuth 扫码（F-50）；④ 账号库导入导出（F-46 扩展）。

### 3.4 M3 凭证续期与保活（批次 1~2，≈3 天）

**刷新状态机（每账号）**：

```
[OK] --accessToken 距过期<24h 且有 refreshToken--> [REFRESHING(互斥锁)]
  ├─ 成功 → 回写凭证（原子写+文件锁） → [OK]，同时更新工具侧副本 version/expiresAtMs
  ├─ 401/refresh 失效 → [NEEDS_RELOGIN(reason)] → 桌面通知 + UI 红标 + 到期日历
  └─ 客户端运行中 → [DEFERRED]（跳过并提示，不写 auth 文件）
```

- **首选端点**：`POST https://www.codebuddy.cn/v2/plugin/auth/token/refresh`，头 = 统一认证头 + `X-Refresh-Token: <refreshToken>`，空体 `{}`；响应 `data.accessToken/refreshToken/expiresIn/refreshExpiresIn`（绝对时间 = 响应时刻 + expiresIn 换算入库）。
- **备选端点**：Keycloak 原生 `POST {iss}/protocol/openid-connect/token`（`grant_type=refresh_token&client_id=console`）。
- **双源化（F-10）**：工具侧副本存 `data/workbuddy_token_store.json`（带 version + expiresAtMs）；桌面 auth 文件只读比对；**生效凭证 = expiresAtMs 更晚者**——任何一方刷新都胜出，彻底规避写冲突。
- **调度**：`refresh_jwt` 同款惰性触发 + `schtasks` 每周兜底任务 `AIWorkAssistant_WorkBuddyRenew`；每日兜底检查时对剩余有效期 < `lazy_refresh_hours` 的账号**主动刷新**（不等 401，v1.2 对齐 trae2api-web「token 过期前主动刷新」模式）；「一次查询最多一次刷新」——多路请求任一 401 统一刷新一次仅重试失败分支，**禁止二次刷新**（防旧 refresh token 覆盖新 token）。

### 3.5 M4 签到与成长中心（批次 1~2，≈3 天）

**`src-python/workbuddy_checkin.py`**（仅标准库，对齐 `auto_checkin.py` 风格）：

| 步骤 | 端点（base=www.codebuddy.cn） | 容错 |
|---|---|---|
| 状态查询 | `POST /v2/billing/meter/checkin-activity-status`（回退旧路径 `/checkin-status`） | `data.today_checked_in` 布尔 |
| 执行签到 | `POST /v2/billing/meter/daily-checkin` 空体 `{}` | `code∈{0,200}` 成功；`code:10001`/message 含「已签到」按成功；当日状态以本地签到日志最新一条为准 |
| 401 处理 | 刷新一次后重试 | 同一账号同一时刻互斥 |
| 输出 | `--json-stream` NDJSON（对齐 checkin-progress 管线） | 零 token 输出 |

**成长中心自动化（F-17，`/v2/activity/growth/*`）**：

| 操作 | 端点 | 调度 |
|---|---|---|
| 旅行 | `GET .../buddy/travel/status` → `arrived` 则 `POST .../claim {record_id}` → `GET .../config` 取目的地 → `POST .../depart` | 签到成功后链式执行，各步独立容错 |
| 盲盒 | `GET .../lottery/chances`（balance>0）→ `POST .../lottery/draw {}` 循环 | 可开关 |
| 任务 | `GET .../tasks` → 过滤 `has_reward && accept_status=未领` → `POST .../tasks/accept {task_code}` | 可开关 |
| 展示 | `GET .../energy`（能量）、`GET .../streak`（连签天数） | buddy-checkin 页附注 |

**Tauri 侧**：`workbuddy_checkin_start(opts)` 命令 + buddy-checkin 页（§3.7.3）+ `workbuddy_checkin_task_register(time)` 每日任务（09:00/21:00 双时段，F-16）+ 失败桌面通知。

### 3.6 M5 积分余额与用量（批次 1~3，≈5 天）

**取数三层降级**：

1. **云端三件套（首选，免 MITM）**：`POST <domain>/billing/meter/get-user-resource-summary` + `get-user-resource-paid-packages` + `get-user-resource-free-packages`（免费包带当日 `SlicePeriodStartTime/EndTime`）。头 = 统一认证头 + `X-Client-Platform: web`；origin 按 token 域路由（domain 含 workbuddy.cn → www.workbuddy.cn，否则 www.codebuddy.cn）。全部 401 → 刷新一次仅重试失败分支；三件套全部无效 → 回退旧接口 `POST /v2/billing/meter/get-user-resource`（`ProductCode: p_tcaca`、`Status:[0,3]`、`PackageEndTimeRange` 拉满）。
2. **本地 quota API（兜底）**：客户端运行中 `GET 127.0.0.1:<port>/api/v1/quota`；端口发现 = 扫 `~/.workbuddy/*.port` + 常见端口段探测，按响应含 `remaining` 确认。
3. **静态快照**：`account-snapshot.json` 的 editionType + savedAt，标注数据时间。

**解析宽容策略（统一走 `dig()` + 链式取值）**：容量字段按 `CycleCapacitySizePrecise → CycleTotalCapacity → CapacitySize` 链式取值；余额 = 各资源 remaining 求和；响应兼容 `data.Accounts / data.data.Accounts / data.Response.Data.Accounts` 等 6 种嵌套路径；`DeductionEndTime` < 7 天 → 「即将到期」徽标 + 到期日历（F-13）。

**落地形态**：`workbuddy_credits.py`（查询+缓存回写 `data/workbuddy_credits_cache.json`，≥5min 缓存）+ `workbuddy_credits_fetch` 命令；buddy-credits 页（余额大字 + 月度趋势 + 各账号对比条形图，F-22）；F-25 官方用量 / F-26 本地 token 统计（JSONL 解析按模型/项目/会话聚合）/ F-27 快照回退随批次 2~3 补齐。

### 3.7 UI 设计（v1.2 重写：对齐本应用「Trae 页面布局」——应用级子导航模式）

> **v1.2 变更说明**：v1.1 采用「把 WorkBuddy 元素散落进 Trae 各页（分区/Tab）」的方案；经与现有代码核对，应用早已确立**应用级子导航模式**（豆包先例）：Sidebar 底部「应用切换 Tab」（Trae / Buddy / 豆包 三格，`src/components/Sidebar.tsx` APP_TABS，Buddy 项已预留且置灰，title="后期扩展（WorkBuddy / CodeBuddy）"），切换应用后左侧主导航整体切换为该应用的专属页组（豆包 = 概述/账号管理/环境配置 三页，视图键 `doubao-*`）。Buddy 接入**沿用同款模式**：启用 Buddy Tab → 导航切换为 Buddy 专属五页。这比 v1.1 方案的优势：① 与豆包心智一致，应用边界清晰；② 避免 Trae 页面（Accounts.tsx 已 1900+ 行）继续膨胀；③ 共享组件与事件管线照常复用。
>
> 参考界面仍为 workbuddy-switch v0.3.1 五张实测截图（账号管理/全部积分包/Token 统计/积分统计/设置），但其导航不照搬——吸收为 buddy-* 五页内的内容形态。

#### 3.7.0 导航与路由注册（一次性改动，落点明确）

| 改动点 | 内容 |
|---|---|
| `types.ts` | `ViewKey` 扩展 `buddy-overview / buddy-accounts / buddy-checkin / buddy-credits / buddy-settings` |
| `Sidebar.tsx` | 新增 `BUDDY_NAV`（与 `DOUBAO_NAV` 并列）：概述 / 账号管理 / 签到与成长 / 积分与统计 / 环境配置；`APP_TABS` 的 buddy 项去除 `disabled`；`nav` 切换逻辑 `activeApp === 'buddy' ? BUDDY_NAV : …` |
| `App.tsx` | `renderView` 新增五个 `buddy-*` 分支，页面组件放 `src/pages/buddy/`（对齐 `src/pages/doubao/` 目录惯例） |
| `store.ts` | `activeApp` 已支持 'buddy'；新增 Buddy 分区状态（账号列表/签到进度/统计数据）与事件归约，复用 `switch-progress / switch-done / checkin-progress` 管线，新增 `wb-credits-updated` |

**组件复用清单**（全部为既有组件，零新增依赖）：

| 组件 | 位置 | 在 Buddy 页面的用途 |
|---|---|---|
| `PageHeader`（左竖条 + 标题 + desc + actions 按钮组） | components/PageHeader.tsx | 每页页头，格式「WorkBuddy · <页名>」 |
| `StatCard`（label + 大字值 + hint + 顶部色条，7 tone） | components/ui.tsx | 概述页四指标卡、积分统计 KPI 卡、Token 总览卡 |
| `Modal`（lg/xl/2xl + `bodyClass="max-h-[80vh] overflow-y-auto"`） | components/ui.tsx | 积分包明细弹窗、OAuth 扫码弹框、切换确认、凭证编辑 |
| `Badge`（7 tone）/ `Progress` / `EmptyState` / `Spinner` | components/ui.tsx | 徽标体系（free/pro/临期/到期/needs_relogin）、包进度条、无账号空态、加载态 |
| `Toaster` + `pushToast` | store + components/Toaster.tsx | 全部异步操作反馈（错误 `pushToast('error', …)`） |
| recharts 图表 | Credits 页已有先例 | 趋势双轴图 / 堆叠柱 / 热力图（自绘 div 网格，不引新库） |
| `withMinDelay(promise, 1000)` | lib/delay.ts | 所有异步按钮最少 1s loading（前端约定） |
| 卡片版式 | 各页通用 | 分区一律 `mt-4 card p-4`（对齐 DoubaoSettings），页头下间距 `mt-5`，网格 `grid gap-4 lg:grid-cols-2` / KPI `grid-cols-2 xl:grid-cols-4` |
| 主题 | lib/themes.ts | 全部样式走 Tailwind `dark:` 类，6 套主题自动适配，禁写死颜色 |

**页面映射总表**（v1.2：Trae 页面 → Buddy 专属页）：

| Buddy 页（视图键） | 吸收内容（workbuddy-switch 参照） | 详细规格 |
|---|---|---|
| `buddy-overview` 概述 | 底部状态条（运行中 + 版本 + 有新版本）→ 概述页环境卡 | §3.7.1 |
| `buddy-accounts` 账号管理 | 账号管理页（双态卡片）+ 全部积分包弹窗 + 聚合迁移入口 | §3.7.2 |
| `buddy-checkin` 签到与成长 | （v1.1 Checkin 分区独立成页）签到 + 成长中心 + 双时段定时 + 签到日志 | §3.7.3 |
| `buddy-credits` 积分与统计 | 积分统计页 + Token 统计页（页内 Tab 二选一） | §3.7.4 |
| `buddy-settings` 环境配置 | 设置页（环境/自动签到/CLI 轮换/通知渠道）+ 到期日历入口 | §3.7.5 |

#### 3.7.1 `buddy-overview` 概述

- `PageHeader title="WorkBuddy · 概述" actions=[打开客户端][刷新]`。
- **四指标卡**（`grid-cols-2 xl:grid-cols-4`，StatCard）：当前账号（昵称 + editionType Badge）/ 池内账号数（备用 N 个）/ 总剩余积分（今日消耗 hint）/ 今日签到（已签 N/共 M，tone 绿/红）。
- **环境卡**（`card p-4`，`lg:grid-cols-2`）：安装路径（探测来源 Badge：注册表/默认路径/进程反查）+ 客户端版本 + 运行状态（运行中绿点/已停止）+ auth 文件路径（存在性 ✓）+ `~/.workbuddy` 数据目录 + 「有新版本」提醒（吸收 workbuddy-switch 底部状态条）。
- **到期提醒条**：7 天内到期的 token / 积分包横向滚动列表（Badge red「即将到期」），点击跳 `buddy-credits` 或 `buddy-settings` 到期日历。
- **快捷入口**：去切换账号 / 立即签到 / 查看积分 三按钮（`withMinDelay` 反馈）。

#### 3.7.2 `buddy-accounts` 账号管理（F-54/F-60/F-56）

**顶部聚合区「添加与迁移账号」**（一张横向 `card p-4`，副文案「快速导入账号，或从已有环境恢复」）：

| 入口 | 动作 |
|---|---|
| OAuth 扫码添加 | 打开扫码弹框（F-50：state → 浏览器扫码 → 轮询 → 自动入池；Modal 内嵌二维码 + 轮询 Spinner） |
| 导入本机账号 | 扫描 auth 文件 + `~/.codebuddy` CLI 凭证，预览后入池 |
| 导入备份 | 选文件 → JSON 预览勾选 → 按索引导入（复用 F-46 两步式） |
| 导出 | 账号库导出 JSON（掩码凭证） |

右侧并排：自动签到总开关（对齐 F-55 启动补签）、视图切换（卡片/列表）、刷新按钮。

**账号卡片区**（`grid gap-4 lg:grid-cols-2 xl:grid-cols-3`，每卡为独立 `card p-4` 组件 `src/pages/buddy/AccountCard.tsx`，对齐 workbuddy-switch `account-card.tsx` 417 行参照）：

```
┌────────────────────────────────────┐
│ (头像) 昵称              [···]      │  ← 头像=QQ/微信登录源；···菜单=编辑/删除快照/凭证
│ (uid 掩码)   (在线/离线徽标)         │  ← uid 取前 8+后 6 位掩码；在线=本机 auth 当前生效
│                                    │
│ ★ 1,192.08  8 个积分包    (09:53 更新)│  ← 余额大字 + 包数 + 最近刷新时间
│ ──────────────────────────────────│
│ 活跃明细                            │
│ ▓▓▓▓░ 42.37 积分  运营裂变包  09/29 到期│  ← 按到期升序取前 2 个包：Progress + 剩余+包名+到期
│ ▓▓▓▓▓ 100 积分   运营裂变包  09/30 到期│
│ 查看全部积分包 →                    │  ← 打开明细弹窗（Modal size="lg"）
│ ──────────────────────────────────│
│ (设为当前)  (设为备用)               │  ← 当前账号：高亮卡片+对勾徽标，仅显示「设为备用」
└────────────────────────────────────┘
```

- 交互约束：「设为当前」= 调 `workbuddy_switch`（客户端关闭窗口期执行，NDJSON 进度 → 复用 `switch-progress` 归约）；「设为备用」仅改标记位（toast 提示需点「设为当前」才真正切换登录态）。
- 卡片右上角徽标体系（Badge tone 映射）：editionType（free=slate / pro=blue）、token 临期（<24h amber / 过期 red）、needs_relogin（red）、今日已签 ✓（green）。
- 无账号时 `EmptyState`（icon=Users，hint 引导「导入本机账号」）。

**积分包明细弹窗（F-56，Modal size="lg" + bodyClass 内滚动）**：

- 弹框头：「<昵称> · 共 N 个积分包」+ 关闭按钮。
- 逐包行：包名（如「CodeBuddy个人版国内运营裂变包」）+ 右侧 `剩余 / 总量`（如 `42.37 / 100`）与 `已用 X`；第二行到期日（`到期 2026/09/29`）+ Progress 进度条（占比 = 剩余/总量）；按到期日升序——**最先到期的排最前**（与 CLI 轮换「最早到期先用」口径一致）。
- 7 天内到期包：到期日红字 + Badge red「即将到期」（联动 F-13 到期日历）。

#### 3.7.3 `buddy-checkin` 签到与成长（F-15/F-16/F-17）

- **签到控制卡**：账号范围多选（跳过已签/过期开关）+「立即签到」主按钮 → NDJSON 进度（复用 `checkin-progress` 归约，逐账号行：成功绿/已签蓝/失败红+原因）。
- **成长中心卡**（F-17）：Buddy 旅行 / 盲盒 / 任务领奖三开关（`label` 行式布局，各带一句白话说明）+「立即执行成长任务」按钮；能量与连签天数展示（GET energy/streak 附注）。
- **定时任务卡**（F-16）：每日 09:00/21:00 双时段 schtasks 注册/查询/卸载；token 保活独立开关。
- **签到日志卡**：最近 30 天滚动（数据源 `workbuddy_checkin_results.json` 90 天存储），逐条：账号名 + 结果 Badge（成功 green / 失败 red + 原因如 `context deadline exceeded`）+ 时间；「清理」按钮复用 T6 `logs_clear` 模式。

#### 3.7.4 `buddy-credits` 积分与统计（F-22/F-56/F-57/F-58，页内 Tab）

页头右侧 `actions` 放**积分统计 / Token 统计**二选一 Tab（useState 切换，不新增路由）。

**Tab A · 积分统计（F-58）**：

- **四 KPI 卡**：剩余积分 / 今日消耗 / 近 7 天消耗 / 本月消耗（StatCard，大字 + 图标）；数据更新时间标注。
- **官方积分消耗堆叠柱**（recharts）：副标题注明「来自 WorkBuddy 官方请求用量 · <起始> 至 <截止>」（官方接口有明确的统计窗口，UI 必须带口径说明）；「所有账号」筛选 + 时间范围；按模型分色堆叠（glm-5.2 / deepseek-v4-pro / minimax-m3 / deepseek-v4-flash / glm-5.3-flash / 其他）；脚注「当前窗口合计 X 积分」。
- **按模型分类排行**：模型数徽标（如「11 个模型」）+「共 N 次请求」；逐行：模型名 + Progress + 右侧 `请求数 / 合计积分 / 占比%`；按合计积分降序；**展示消耗最高的 8 个，脚注「已显示消耗最高的 8 个模型，其余模型仍计入上方合计」**（防长尾撑爆列表）。

**Tab B · Token 统计（F-57）**：

- 顶部：WorkBuddy / CodeBuddy CLI **双源 Tab**（数据目录分别为 `~/.workbuddy/projects` 与 `~/.codebuddy/projects`）+ 数据更新时间 +「刷新统计」。
- **Token 总览卡**：构成堆叠条（绿=缓存读取 86.7% / 紫=新增输入 11.8% / 蓝=输出 1.4% / 黄=缓存写入 0.6%，图例带百分比）+ 四指标：总 Token、输入、输出、**缓存命中率**（大字，如 88.0%）；时间范围切换：今日 / 近 7 天 / 近 30 天 / 总计。
- **Token 与调用趋势**：双轴组合图——左轴堆叠柱（缓存读取/新增输入/输出/缓存写入四色）+ 右轴调用次数虚线；图例可点选；「所有模型」筛选 + 今天/近 7 天/近 30 天/本月；脚注「近 30 天合计 X Token · Y 次调用 / 数据覆盖截至 <时间>」。
- **Token 活动热力图**：GitHub 风格年度日历（列=周、行=星期，最近一年，灰→绿四档强度，自绘 div 网格），右上「每日」粒度切换。

#### 3.7.5 `buddy-settings` 环境配置（F-55/F-59/F-13/F-19）

版式对齐 DoubaoSettings：`PageHeader` + 多张 `mt-4 card p-4` 配置卡，每卡一个主题。

| 分区卡 | 字段 | 说明 |
|---|---|---|
| 环境 | 客户端路径 + 自动检测 + 打开所在目录 | 复用 `app_locate`；显示探测来源与版本（workbuddy-switch 的「权限检测」为 macOS 专属，Windows 版替换为 auth 文件路径存在性校验 + 打开所在目录按钮） |
| 自动签到 | 启用自动签到开关 | 副文案「启动时立即核验服务端状态，未签到账号会自动补签」 |
| | 保活阈值（天） | `keepalive_days`；0=每天无条件刷新全部带 refreshToken 账号 |
| | 惰性刷新（小时） | `lazy_refresh_hours`，默认 24；剩余有效期低于该值才刷新 |
| | 操作 | 保存配置 / 全部立即签到 |
| 签到日志 | 最近 30 天滚动 | 数据源=`workbuddy_checkin_results.json`（90 天存储，UI 默认展示 30 天）；与 `buddy-checkin` 日志卡同源 |
| CLI 自动轮换 | 当前 CLI 账号 + 启用开关 | 副文案「启用后按下方间隔自动检查并切换 CodeBuddy CLI 账号」 |
| | 检查间隔（分钟）/ 冷却期（分钟）/ 到期差异阈值（小时）/ 到期紧迫阈值（小时）/ 活跃保护（分钟）/ 最小剩余积分 | 五重防护参数（F-59），每项带一句白话解释 |
| | 说明行 | 「切换时机：目标账号剩余到期时间少于『紧迫阈值』且比当前账号早超过『差异阈值』，且最近『活跃保护』分钟内 CLI 无对话、目标剩余积分不低于『最小剩余积分』」 |
| 到期日历 | F-13 入口 | token 到期（access/refresh 双轨）、积分包到期（DeductionEndTime）、会员到期（复用 Trae 侧）统一日历组件 |
| 通知渠道 | 桌面通知 / 企业微信 / Server酱 | F-19 |

### 3.8 数据文件与命令契约

**新增数据文件**（均 `%APPDATA%\AIWorkAssistant\data\`，凭证类入 `.gitignore`）：

| 文件 | 内容 |
|---|---|
| `workbuddy_accounts.json` | 账号池（§3.3 结构） |
| `workbuddy_token_store.json` | 工具侧凭证副本（version + expiresAtMs，双源化） |
| `workbuddy_profiles/<uid>/auth.info(+meta.json)` | 切换快照槽 |
| `workbuddy_credits_cache.json` | 积分缓存（带 fetched_at） |
| `workbuddy_checkin_results.json` | 签到结果（90 天滚动，趋势图数据源） |
| `wb_sticky_sessions.json` | 会话粘性映射（TTL 30m） |
| `wb_model_catalog.json` | 模型目录静态兜底（dsh 15 模型 + 动态替换） |
| `workbuddy_chats/<uid>/` | 会话三件套备份 |

**新增 Tauri 命令**（嵌套对象字段保持 snake_case，顶层参数驼峰跟随 Rust 签名）：

| 模块 | 命令 |
|---|---|
| 环境 | `workbuddy_env_check()` |
| 账号 | `workbuddy_accounts_list` / `workbuddy_account_save` / `workbuddy_account_remove` / `workbuddy_account_set_credential` / `workbuddy_scan_auth_file` |
| 切换 | `workbuddy_switch(user_id)` / `workbuddy_save_login(user_id)`（复用 profile_* `target_app=workbuddy`） |
| 续期 | `workbuddy_refresh_token(user_id)` / `workbuddy_renew_task_register/status/unregister` |
| 签到 | `workbuddy_checkin_start(opts)` → NDJSON / `workbuddy_checkin_task_register(time)` / `workbuddy_growth_run(opts)` |
| 积分 | `workbuddy_credits_fetch(user_id?)` / `workbuddy_credits_history(days)` / `workbuddy_token_stats(days)` |
| 会话 | `workbuddy_chatdata_backup/restore/info(user_id)` |
| 生态 | `workbuddy_oauth_login()` / `workbuddy_cli_bridge_set(user_id)` / `workbuddy_env_reset(user_id, items)` |
| 事件 | 复用 `switch-progress/switch-done/checkin-progress` 管线，新增 `wb-credits-updated` |

### 3.9 M6 API 网关上游扩展（批次 2，≈8 天）——关键技术方案

现有 `api_server/` 已具备 OpenAI + Anthropic 双协议输出与 app 无关账号池；WorkBuddy 接入 = **新增上游类型**，改动集中在 upstream 适配层，协议输出层零改动。

**① 上游适配（payload.rs / 新 upstream 模块）**：

- 对话上游 `POST https://copilot.tencent.com/v2/chat/completions`（Global 区 `www.workbuddy.ai`），上游**只回 SSE**——非流式由本地 `Aggregate` 聚合（tool_calls delta 按 index 合并、帧归一化）。
- 请求头三铁律：
  1. `Origin` + `Referer` 必带（按账号 region 切换 CN=codebuddy.cn / Global=workbuddy.ai）；
  2. 缺省字段显式 `X-No-*: 1` 占位（Authorization/User-Id/Enterprise-Id/Department-Info 四件）；
  3. **红线：chat 请求绝不携带 `X-Refresh-Token`**（只允许出现在 refresh 端点，配 `X-Auth-Refresh-Source: workbuddy`）。
- 完整对话头：`Authorization: Bearer` + `User-Agent: CLI/2.63.2 CodeBuddy/2.63.2` + `X-Requested-With: XMLHttpRequest` + `X-User-Id` + `X-Enterprise-Id` + `X-Domain` + `X-Product: SaaS`。
- 请求体改写：强制 `stream:true`；`tool_choice` 对象 → string（对象形式上游报 400 `code=11101`）；`reasoning_effort` 按模型 `supportedEfforts` 自动降级；指纹清洗可开关（剥离 Claude Code 痕迹：身份句改写、`cc_xxx=...;` 键值对剥离、`X-Anthropic-*` 头引用剥离，零分配预检 + 正则兜底）；**审核黑名单预处理（v1.2）**——腾讯内容审核把 Claude Code 两句固定 system 模板逐字入黑名单（`You are Claude Code, Anthropic's official CLI for Claude.` / `Main branch (you will usually use this for PRs)`），命中即「敏感内容」拒答，改任一字即绕过——改写层内置**模板句映射表精确匹配 + 最小改写**（CLI→CLI tool、Main branch→Default branch），映射表外置文件可热更新（借鉴 workbuddy-cliproxy `sanitizeBlockedTemplates`，learn-the-design 不抄码）。

**② 调度引擎（pool.rs 扩展，与现有积分感知调度并存）**：

- 三因子加权随机：`credits 占比×10 + 闲置时长补偿（每小时+0.5，封顶 5.0） + 成功率×3` → Top5 短名单内二次加权随机——防热点 + 防惊群（100ms 窗口去重）。
- **P2C 备选策略（v1.2）**：Power-of-Two-Choices 随机选二取优（antigravity-tools 实证延迟优于轮询/加权随机）——作为 `api_pool.json` 的可切换 `strategy` 项与三因子加权并存，实测对比后取默认。
- **账号五态机（v1.2）**：`Available` / `QuotaProtection`（配额低于阈值自动剔除）/ `RateLimited`（429，指数退避，到期自动恢复）/ `Forbidden`（403 封禁，标注账号需人工确认）/ `ProxyDisabled`——替换现有「冷却/禁用」二元态；状态迁移事件落 `/status` 画像与 api_logs。
- 熔断器：错误阈值 3 + 冷却 30m 起指数递增至 6h；`hard_credit`（积分耗尽）冷却到次日，04:00 自动恢复探测；`soft_rate`（限频）60s。
- 会话粘性（F-31 双模式）：显式 conversationId 绑定（双段分配 + TTL 30m 滚动续期 + 写锁 re-check 防 TOCTOU）；无 conversationId 时退化**指纹模式**（前 3 条消息 SHA256 取 6 位 + 60s 时间窗锁定，v1.2）。

**③ 错误三态（F-33 + v1.2 分级重试表）**：`RETRY_SAME`（502/503/超时 → 同 Key 重试 1 次）/ `SWITCH_KEY`（401/403/429 → 刷新换号）/ `FATAL`（400 context_too_long → 终止）；积分耗尽标记词表（积分不足/额度不足/insufficient credit/quota exceeded…）集中维护。**分级重试策略表（v1.2，吸收 antigravity-tools）**：

| 错误 | 策略 |
|---|---|
| 429 | 优先解析 `Retry-After` 头按其等待；缺省线性退避 1/2/3s |
| 503 / 529 | 指数退避（并入既有模型级 10→20→40s 渐进退避） |
| 400 且错误体含 `thinking.signature` | 固定 200ms 后重试一次（上游偶发签名校验抖动） |
| 其余 | 走三态判定：换号或终止 |

**④ 工程化（F-34）**：模型级冷却渐进退避 10→20→40s（优先级高于 Key 级）；SSE keep-alive 15s（`: keep-alive` 注释行）；首字超时 10s 触发故障转移；客户端断连 `_drain_upstream` 读完上游保 usage；健康检测 5min + 0-60s 抖动（轻量 `/v1/models` 请求）。

**⑤ 运维接口（F-32）**：`/v1/models`（模型目录动态同步 + 静态兜底）、`/status`（total/healthy/cooling/disabled + 每账号画像）、`/healthz`（无健康账号 503）；请求级日志 seq/TTFB/uid/tokens/latency 并入现有 api_logs。

### 3.10 M7 生态接入（批次 3~4）

- **CLI 切号桥（F-06，五重防护）**：Windows 直接维护 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN`（绕过 apiKeyHelper Windows 坑）；目标 = 「到期最早且仍有剩余」账号（`urgency_key` 排序：到期越早越紧迫、无到期排最后）。切换需同时满足五重防护（对齐 workbuddy-switch `rotate.rs::decide_target`）：① 冷却期 `cooldown_minutes`（切换后 N 分钟内不切）；② 到期差异阈值 `min_gap_hours`（目标比当前早到期超过该值才切，防多账号同期到期来回横跳）；③ 到期紧迫阈值 `min_urgency`（目标到期剩余超过该值=都还早，不切）；④ 活跃保护 `active_guard`（CLI 最近对话 N 分钟内不切，防打断工作中会话；CLI 最近写入时间取自会话目录 mtime）；⑤ 最小剩余积分 `min_remaining`（目标低于该值不切，0=关闭）。外加检查间隔（分钟）周期触发。**`decide_target` 保持纯函数形态（无 IO），便于单测**；状态落 `wb_cli_rotate_state.json`。
- **OAuth 工具（F-50）**：`POST /v2/plugin/auth/state?platform=CLI` → 浏览器打开 authUrl → `GET /v2/plugin/auth/token?state=` 轮询 → `GET /v2/plugin/login/account?state=` 取 uid/nickname；每流程独立 cookie jar；无 PKCE（state 服务端签发）。
- **Codex 转换器（F-40）**：复用 `tonny0812/workbuddy2api` 投影逻辑——`/v1/responses` 端点（Codex CLI `wire_api="responses"` 直配 `base_url`）+ 既有 Anthropic `/v1/messages` + OpenAI 兼容三协议一份；`--desensitize` 脱敏、审核命中自动退回紧凑模式重试。
- **DSH provider（F-37）**：15 模型静态目录兜底（auto/hy3/hy4/glm-5.x/kimi/minimax/deepseek-v4 等，含上下文/maxTokens/图片/思考档位/倍率）+ 启动动态替换（模型目录接口）；能力声明读上游 `inputModalities/supportedEfforts`，勿硬编码（图片模态案例教训）。
- **CC Switch 协同（F-43）**：把本项目转换端点写入 CC Switch 配置，不自建切换器。
- **环境重置（F-14）**：16 项认证残留清理清单（vscdb 认证 key、`Tencent-Cloud.coding-copilot` 产品缓存、marker、vscdb.backup 等）+ Keycloak logout；执行前勾选预览 + 二次确认。

### 3.11 M8 会话数据管理（批次 3，≈3 天）

- **三件套（缺一不可）**：① 正文 `~/.workbuddy/projects/{workspace}/{cid}.jsonl`（每行含 sessionId）；② 元数据 `~/.workbuddy/workbuddy.db` sessions 表（id = conversation id = UUID）；③ 云端映射 `~/.workbuddy/edge-sync-mapping-v2.db` 的 `edge_sync_mapping` 表（`msg_channel=convmsg:{uid}` 决定云端归属）。
- **备份/恢复（F-44）**：整目录 + 双 db 快照至 `workbuddy_chats/<uid>/`；执行前校验客户端已关闭。
- **复制/迁移（F-45）**：读源账号 jsonl → 替换 sessionId 为新 UUID → 写目标 projects → sessions 表插行 → edge_sync_mapping 注册 `convmsg:{目标uid}`；复制前 `backup_workbuddy_db`。

---

## 四、任务清单（WBS · 5 批次）

> 每批独立可验收；预估为净开发人日。DoD 统一含：UTF-8 校验通过、单测通过（cargo test + python tests）、中文文案规范、commit 按 [feature]/[fix] 分拆附显式路径。

### 批次 1 · 快赢闭环（≈12~14 天）——目标：识别→切换→续期→签到→余额全通

| # | 任务 | 涉及 | 预估 | DoD 摘要 |
|---|---|---|---|---|
| T1.0 ✅ | **Buddy 应用级子导航骨架（§3.7.0）**：Sidebar Buddy Tab 启用 + `buddy-*` 五页路由 + `src/pages/buddy/` 目录 + store Buddy 分区状态（空页占位） | types.ts、Sidebar.tsx、App.tsx、store.ts | 0.5d | 五页可达，PageHeader/版式对齐豆包先例；Trae/豆包页面零改动 |
| T1.1 ✅ | `workbuddy_env_check` + buddy-overview 概述页 | commands/workbuddy.rs（新建）、pages/buddy/Overview.tsx | 0.5d | 四级探测返回 exe/authDir/dataDir；四指标卡 + 环境卡 + 到期提醒条 |
| T1.2 ✅ | auth 文件扫描入池（F-04） | workbuddy/mod.rs、accounts | 1d | 嵌套字段链兼容；token 哈希稳定 id；凭证掩码 |
| T1.3 ✅ | PS 桥 authfile 布局快照/恢复 + 切换/保存命令（F-02） | trae-switch-bridge.ps1、profile_*、前端账号行 | 2d | 切换后 uid 轮询确认；防误覆盖守卫；NDJSON 进度 |
| T1.4 ✅ | token 续期（F-09）+ 每周兜底任务 | workbuddy/mod.rs、misc.rs | 1.5d | 惰性刷新；互斥锁；needs_relogin 通知；实测刷新一轮 |
| T1.5 ✅ | workbuddy_checkin.py + buddy-checkin 签到与成长页（F-15） | src-python、pages/buddy/Checkin.tsx | 2d | 已签容错；401 刷新重试；NDJSON；零 token 输出 |
| T1.6 ✅ | 积分三件套 + 旧接口回退 + buddy-credits 页（F-20/F-22） | workbuddy_credits.py、pages/buddy/Credits.tsx | 2d | 双域路由；6 种嵌套解析；5min 缓存；到期提醒 |
| T1.7 ✅ | 到期日历（F-13，跨应用） | 新组件 + 三应用接入 | 1d | token/积分/会员三类到期统一日历 + 提醒 |
| T1.9 ✅ | buddy-accounts 页 UI：双态卡片 + 积分包明细弹窗 + 聚合迁移入口（F-54/F-56/F-60） | pages/buddy/Accounts.tsx、AccountCard 组件 | 2d | 卡片布局/在线徽标/余额大字/活跃明细对齐 §3.7.2；明细弹窗逐包进度条；版式/组件对齐 §3.7.0 复用清单 |
| T1.10 ✅ | 自动签到配置化 + 启动补签（F-55） | pages/buddy/Settings.tsx、checkin 管线 | 1d | keepalive_days / lazy_refresh_hours 参数化；启动核验未签自动补签；签到日志 30 天滚动 |
| T1.8 ✅ | 批次 1 集成测试 + 文档同步（AGENT.md/CHANGELOG） | docs | 0.5d | 全链路手工回归清单通过 |

### 批次 2 · API 暴露 + 成长中心（≈11~13 天）

| # | 任务 | 涉及 | 预估 |
|---|---|---|---|
| T2.1 ✅ | WorkBuddy 上游适配：headers 三铁律 + 强制 stream + tool_choice/effort 改写 + 非流式聚合 + **审核模板黑名单最小改写（映射表热更新）**（F-28/F-30 v1.2） | api_server/payload.rs、新 upstream | 3.5d |
| T2.2 ✅ | 调度引擎：三因子加权 + **P2C 备选策略 + 账号五态机** + 熔断 + 04:00 恢复（F-29 v1.2）+ 错误三态与**分级重试策略表**（F-33 v1.2） | api_server/pool.rs | 3d |
| T2.3 ✅ | 运维接口 /v1/models /status /healthz + 请求级日志（F-32） | api_server/routes.rs | 1d |
| T2.4 ✅ | 会话粘性**双模式**：conversationId 绑定 + 前 3 消息 SHA256 指纹 60s 窗 + TTL（F-31 v1.2） | api_server | 1.5d |
| T2.5 ✅ | 成长中心自动化：旅行/盲盒/任务链式 + 独立开关（F-17）+ 双时段调度（F-16） | workbuddy_checkin.py、Checkin.tsx | 2d |
| T2.6 ✅ | 双源化 token 保活（F-10） | workbuddy/mod.rs | 1d |
| T2.7 ✅ | 工程化：模型级冷却退避 + SSE keep-alive + 首字超时 + _drain_upstream（F-34） | api_server | 1.5d |

### 批次 3 · 会话数据 + 用量 + CLI 桥（≈12~15 天）

| # | 任务 | 预估 |
|---|---|---|
| T3.1 ✅ | 会话三件套备份/恢复（F-44） | 1.5d |
| T3.2 ✅ | 会话复制/迁移新 id 算法（F-45） | 2d |
| T3.3 ✅ | 官方用量 get-user-request-usage（F-25）+ Token 统计增强版（F-26/F-57/F-58：四指标+缓存命中率+双轴趋势+年度热力图+按模型排行） | 3.5d |
| T3.4 ✅ | CLI 切号桥 + 五重防护轮换 decide_target 纯函数 + 单测（F-06/F-59） | 2d |
| T3.5 ✅ | OAuth 扫码工具（F-50）+ 环境重置 16 项清理（F-14） | 2.5d |
| T3.6 ✅ | 账号导入导出扩展 + 通知渠道企业微信/Server酱（F-19） | 1.5d |
| T3.7 ✅ | ck_xxx API Key 子 Key 体系（F-35） | 2d |

### 批次 4 · 生态与远期（≈6~8 天 + 机会项）

| # | 任务 | 预估 |
|---|---|---|
| T4.1 ✅ | Codex `/v1/responses` 投影转换器 + 脱敏（F-40） | 2.5d |
| T4.2 ✅ | UI 坐标点击签到兜底（F-18） | 1d |
| T4.3 ✅ | 积分用量快照回退（F-27） | 1d |
| T4.4 ✅ | 活动信息展示（F-51） | 0.5d |
| T4.5 ✅ | Global 区上游域名路由（F-36）：domain 含 `.workbuddy.ai` 的账号全走 `www.workbuddy.ai` | 0.5d |
| T4.6 ✅ | 批次 4 收尾审查 + 文档 + 分拆提交 | — |

> 顺延注记（v1.3 更新）：批次 5 已正式立项（见下）——F-37/F-43/F-21 转批次 5 正式任务；F-42/F-52/F-41 转机会项。

### 批次 5 · 生态吸收与网关增强（≈11~13 天）——目标：开源生态价值点全量落地

> 依据 `docs/tmp/oss-ecosystem-value-analysis.md` v1.1（✅ 标记同步）：批次 1-4 已吸收高价值项（审核黑名单/粘性指纹/P2C/五态机/分级重试/hy3 effort/prompt cache 口径等）之外，剩余可纳入价值点在本批次收口。

| # | 任务 | 涉及 | 预估 |
|---|---|---|---|
| T5.1 | DSH provider：15 模型静态目录兜底 + 启动动态替换（F-37，元数据透传 inputModalities/supportedEfforts/倍率/徽章，能力读上游勿硬编码） | `wb_model_catalog.json`、api_server | 2d |
| T5.2 ✅ | 四段模型路由管线：别名静态映射 → 用户自定义正则 → 系列通配 → 后缀检测注入参数（F-61） | api_server | 1.5d |
| T5.3 ✅ | reasoning_content 思考链透传 + 默认深度思考开关（F-62） | api_server、buddy-settings | 1d |
| T5.4 ✅ | 生图双端点投影：`/v1/images/generations` + `/v1/images/edits`（F-63） | api_server/routes.rs | 1.5d |
| T5.5 ✅ | 网关工具代执行：上游不支持工具（web_search）代理侧代执行 + 结果回喂 + 原生事件返回（F-64） | api_server | 2d |
| T5.6 ✅ | 协议细节补强：连续同角色消息合并 / 单端口三协议 anthropic-version 区分 / 后台任务识别降级（F-65） | api_server | 1d |
| T5.7 | CC Switch 协同：把本项目转换端点注册进 CC Switch 配置，不自建切换器（F-43） | commands + 前端 | 0.5d |
| T5.8 ✅ | 本地 quota API 兜底：扫 `~/.workbuddy/*.port` + 端口段探测 + `remaining` 特征确认（F-21） | commands/workbuddy.rs | 1d |
| T5.9 ✅ | 批次 5 收尾审查（九大类黑盒复查）+ 文档同步 + 分拆提交 | — | — |

> 机会项（按需评估，不阻塞批次 5 验收）：workbuddy-mcp（F-42，P3）、WorkBuddyProxy（F-52，P3 远期）、CLI 多账号环境隔离（F-66，P3 评估）、trae2codex（F-41，按需）。

---

## 五、关键实现技术方案（深入）

### 5.1 凭证双源共存（本方案最关键设计）

**问题**：auth 文件被客户端启动时重写（每次启动生成新历史快照），工具写桌面文件会与 App 冲突。

**方案**（移植 dsh-workbuddy-connect auth.ts 设计）：

- 桌面 `workbuddy-desktop.info` **只读**；工具在 `data/workbuddy_token_store.json` 维护自己的副本（带 `version` 字段，拒绝未知格式）。
- **生效凭证 = 两者中 `expiresAtMs` 更晚者**。任何一方（App 自动刷新 / 工具续期）刷新后都胜出，读取方自动采纳，互不覆盖。
- 写盘一律原子写（`fs_utils::write_json` tmp+rename）+ 文件锁；切换/续期写 auth 文件前必须确认客户端已关闭。
- 每账号刷新互斥锁（RAII 守卫防 panic 泄漏）+ 全局轮次锁（重复触发整轮直接跳过并明确报错）。

### 5.2 域名路由规则

| 区域 | 判定 | chat 上游 | billing/积分 | 活动接口 |
|---|---|---|---|---|
| CN | domain 不含 `.workbuddy.ai` | `copilot.tencent.com` | `www.codebuddy.cn`（dsh/antigravity 实测 copilot.tencent.com 也通，保留双域探测） | `www.codebuddy.cn` |
| Global | domain 含 `.workbuddy.ai` | `www.workbuddy.ai` | `www.workbuddy.ai` | `www.workbuddy.ai` |

**令牌域与请求域不一致会被网关拒绝**——积分新接口 origin 必须与 token `domain` 字段一致；plugin/billing v2 网关固定在 codebuddy.cn。

### 5.3 统一请求头构建（`build_auth_headers`）

```
Authorization: Bearer <accessToken>
X-User-Id: <uid>                 # 无则 X-No-User-Id: 1
X-Enterprise-Id: <eid>           # 企业账号才带；无则 X-No-Enterprise-Id: 1
X-Tenant-Id / X-Domain: <domain> # 无则 X-No-Department-Info: 1
X-Client-Platform: web           # 仅积分三件套需要（缺了被网关拒）
```

签到/活动接口可用极简头：`User-Agent: WorkBuddy`（桌面端 UA）+ Bearer + X-User-Id 即可（88lin 实测）。

### 5.4 响应宽容解析（解析层统一采用）

- `dig()` 信封解包：字段可能被 `data/result/resp/response/info` 任意一层包裹，递归查找（限深 8 层，数组同层展开）——fs_utils 已就绪。
- 6 种嵌套路径兼容 + 容量字段链式取值（`CycleCapacitySizePrecise → CycleTotalCapacity → CapacitySize`）。
- 奖励数额一律以接口返回为准；「已签到」按成功容错（code:10001 / message 含「已签到」/「repeat」）。

### 5.5 网关上游协议要点（联调避坑清单）

| # | 坑 | 对策 |
|---|---|---|
| 1 | 上游拒绝非流式 | 强制 `stream:true`，非流式本地聚合 |
| 2 | `tool_choice` 对象报 400 code=11101 | 归一化为 string（`{"type":"function","function":{"name":X}}` → `"X"`） |
| 3 | effort 档位上游不支持下发 | 按模型 `supportedEfforts` 自动降级 |
| 4 | Claude Code 指纹触发审核 | 指纹清洗开关（默认开，可配置关闭） |
| 5 | chat 带 X-Refresh-Token 触发安全拦截 | 红线：该头只出现在 refresh 端点 |
| 6 | 缺 Origin/Referer 被拒 | 按账号 region 强制携带 |
| 7 | SSE 长流被中间层回收 | 15s keep-alive 注释行 + 心跳 |
| 8 | 客户端断连丢 usage | `_drain_upstream` 读完上游 |
| 9 | **Claude Code system 模板审核黑名单**（v1.2，workbuddy-cliproxy）：腾讯审核把两句固定模板**逐字入黑名单**——`You are Claude Code, Anthropic's official CLI for Claude.` 与 `Main branch (you will usually use this for PRs)`，命中即「敏感内容」拒答；**任何一字改动即绕过** | 改写层内置模板句映射表精确匹配 + 最小改写（CLI→CLI tool、Main branch→Default branch）；映射表外置可热更新（cat-and-mouse，禁止硬编码进二进制）；与指纹清洗开关联动（清洗关闭时跳过该预处理） |
| 10 | **prompt cache 对代理流量不生效**（v1.2，workbuddy-cliproxy Issue #4）：字节完全一致的请求 `prompt_cache_hit_tokens` 恒 0——上游缓存不对代理流量命中，按冷启动全价计费 | 积分成本模型按「无缓存」估算；用量统计不依赖缓存命中字段（UI 缓存命中率指标仅基于本地 token 统计，标注口径来源，避免误导） |

### 5.6 模型目录策略

- 静态兜底目录 15 模型（hy4-preview/hy3-x 限时免费 x0.00/x0.05；glm-5.3 x0.79；kimi-k3-1 x1.62；deepseek-v4-flash x0.17 等，含上下文 168K~1M / maxTokens 32K~128K / 图片 / 思考档位）落 `wb_model_catalog.json`。
- 启动后调 `GET {chatBase}/console/enterprises/personal/models` 动态替换（倍率/徽章以服务端为准）；`api_models_sync` 复用官网 `batch_get_detail_param` 同款思路。
- **能力声明永远读上游字段**（`inputModalities` / `supportedEfforts` / `canDisableThinking`），勿硬编码——图片模态案例的直接教训。
- **按模型族思考参数强制映射（v1.2，workbuddy-cliproxy 实测）**：hy3 系列仅 `reasoning_effort=high` 真正生效（medium/max/xhigh 被上游忽略）——在「能力声明读上游」原则之上叠加**实测修正层**：模型目录维护 `effort_override` 字段（按模型族，如 `hy3-*: high`），网关改写时最终生效值 = 修正层 > 客户端请求 > 上游默认；该映射与 UA 常量、审核模板映射表同处集中配置。

### 5.7 统计与调度实现细节（v1.1 新增，源自 workbuddy-switch 源码核对）

**① Token 统计解析（token_stats.rs，F-57 照抄要点）**：

- 数据源 JSONL 事件流解码字段：input / output / cacheRead / cacheWrite / uncachedInput / records；`cache_hit_rate = cache_read / (input + cache_read)`（input>0 时计算，否则 0）。
- **cache_read 别名链**：`cache_read_input_tokens` → `prompt_cache_hit_tokens`，**优先取正值**——stale 的 0 值不得掩盖后写的有效值；兼容嵌套 `provider_details` 内层字段。
- 聚合三维：按模型 / 项目 / 会话；`days ∈ {7,30,90}` + 总计；双源（WorkBuddy / CodeBuddy CLI）独立 Tab。
- 趋势图数据 = 按日桶聚合四类 token + 调用次数；热力图 = 按日总量分四档强度。

**② 签到日志规则（checkin.rs）**：

- 仅「状态查询=未签 → 提交 daily-checkin → 有明确结果」的分支才计入签到日志（已签容错不重复记）；`add_checkin_log` 滚动保留 30 天。
- 失败原因原样入日志（如 `context deadline exceeded`——上游超时），UI 红字展示；成功绿字 + 时间。
- `RunFlagGuard` 运行互斥：整轮签到进行中重复触发直接拒绝（对齐本项目全局轮次锁设计）。

**③ 续期参数配置化（refresh.rs）**：

- 惰性刷新：`expiresAt` 缺失或剩余 < `lazy_refresh_hours`（默认 24）才刷新。
- 保活检查：每日由后台循环调用一次；`keepalive_days <= 0` 时**无条件刷新全部**带 refreshToken 的账号，>0 时仅刷剩余不足该天数的账号。
- 本项目映射：`keepalive_days` → schtasks 每日兜底任务的刷新判定；`lazy_refresh_hours` → 手动/网关 401 触发路径的判定阈值——两者共用同一刷新函数，仅入参不同。

**④ CLI 轮换决策算法（rotate.rs::decide_target，纯函数）**：

```
输入: candidates[], now, cooldown_ms, min_gap_ms, min_urgency_ms,
      cli_recent_activity_ms: Option<i64>, active_guard_ms, min_remaining
1. 过滤: 排除冷却期内(上次切换 ts + cooldown_ms > now)的账号
2. 排序: 按 urgency_key 升序（到期越早越紧迫；无到期时间排最后）
3. 紧迫检查: 最紧迫目标剩余 > min_urgency_ms → 不切（"所有账号到期都还早"）
4. 活跃保护: CLI 最近活动距今 < active_guard_ms → 不切（正在用）
5. 差异检查: 当前账号剩余 - 目标剩余 < min_gap_ms → 不切（防横跳）
6. 剩余检查: 目标 remaining < min_remaining → 不切
7. 全部通过 → 写 state.json（含切换原因日志）→ 更新 settings.json
```

**⑤ 前端结构参考（移植映射）**：

| workbuddy-switch 文件 | 行数 | 移植去向 |
|---|---|---|
| `src/pages/AccountsPage.tsx` | 754 | buddy-accounts 页（卡片区+聚合入口） |
| `src/pages/TokenStatsPage.tsx` | 1355 | buddy-credits 页 Token 统计 Tab（总览卡/双轴趋势/热力图） |
| `src/pages/CreditStatsPage.tsx` | 1388 | buddy-credits 页积分统计 Tab（KPI/官方消耗/模型排行） |
| `src/pages/SettingsPage.tsx` | 956 | buddy-settings 页（环境/自动签到/CLI 轮换/通知渠道） |
| `src/components/account-card.tsx` | 417 | AccountCard 组件 |
| `crates/.../modules/{account,auth_file,checkin,credits,official_usage,refresh,rotate,switch,token_stats}.rs` | — | workbuddy/mod.rs 领域逻辑分层参照 |

### 5.8 调度与协议工程参照（v1.2 新增，源自 2026-09-10 开源生态调研）

> 依据 `docs/tmp/oss-ecosystem-value-analysis.md`（28+5 仓库调研快照）；本节汇总 Buddy 侧吸收项与落地落点。合规原则：**learn-the-design, write-our-own-code**（dingminhua 范式），吸收设计思路与协议情报，不整体复制源码。

| # | 情报/设计 | 来源 | 落点 |
|---|---|---|---|
| 1 | 上游 code 11101 拒绝非流式 → 统一「内部转流式再聚合」 | workbuddy-cliproxy（与蓝本既有结论交叉验证） | §5.5 #1（已吸收） |
| 2 | Claude Code system 模板审核黑名单逐字匹配 + 最小改写绕过 | workbuddy-cliproxy `sanitizeBlockedTemplates` | F-30 指纹清洗 v2 + §5.5 #9 |
| 3 | hy3 系列仅 `reasoning_effort=high` 生效（medium/max/xhigh 被忽略） | workbuddy-cliproxy | §5.6 `effort_override` 修正层 |
| 4 | prompt cache 对代理流量恒不命中（按冷启动全价计费） | workbuddy-cliproxy Issue #4 | §5.5 #10 + F-31 口径注记 |
| 5 | 会话粘性指纹：前 3 条消息 SHA256 取 6 位 + 60s 时间窗锁定 | antigravity-tools | F-31 指纹模式 |
| 6 | P2C 负载均衡（随机选二取优，延迟优于轮询/加权随机） | antigravity-tools | F-29 备选策略 |
| 7 | 账号五态机（Available/QuotaProtection/RateLimited/Forbidden/ProxyDisabled） | antigravity-tools | F-29 状态机 |
| 8 | 分级重试表（429 Retry-After/线性、503/529 指数、400+thinking.signature 200ms） | antigravity-tools | F-33 重试策略表 |
| 9 | 协议细节：连续同角色消息自动合并、单端口三协议靠 `anthropic-version` 头/路径区分、后台任务（生成标题/摘要）识别降级 | antigravity-tools | §3.9 ① 改写层 + §5.5 后续联调补充位 |
| 10 | token 过期前主动刷新（不等 401） | trae2api-web | F-09 + §3.4 调度 |
| 11 | **四段模型路由管线**：别名静态映射 → 用户自定义正则 → 系列通配 → 后缀检测注入参数（v1.3 新增） | antigravity-tools | F-61（批次 5） |
| 12 | **reasoning_content 思考链透传 + 默认深度思考开关**（v1.3 新增） | Tom6814/WorkBuddy2API | F-62（批次 5） |
| 13 | **生图双端点**（generations + edits 图生图投影）（v1.3 新增） | Tom6814/WorkBuddy2API | F-63（批次 5） |
| 14 | **Responses API 工具代执行**：上游不支持的工具代理侧代执行、结果回喂、原生 `web_search_call` 事件返回（v1.3 新增） | muskke/trae-api-proxy | F-64（批次 5） |
| 15 | **协议细节**：连续同角色消息自动合并、单端口三协议 `anthropic-version` 头/路径区分、后台任务识别降级（v1.3 独立立项，原 #9 拆出） | antigravity-tools | F-65（批次 5） |
| 16 | **多 CLI 账号环境隔离**：独立 `CODEX_HOME`/`CLAUDE_CONFIG_DIR` + 全局变量剥离 + 严格账号模式（v1.3 新增，评估项） | xiaolizi0v0/CliProxy | F-66（批次 5 机会项） |

**与既有设计的冲突调和**：#5 粘性指纹的 cache 收益宣称（+300%）来自 Gemini 上游（antigravity-tools）；Buddy 上游为腾讯 copilot，代理流量缓存恒不命中（#4）——因此指纹模式在 Buddy 场景的价值是**会话一致性与上游侧缓存（若有）**，积分成本模型统一按无缓存估算，两处结论已在 F-31 验收标准中写明口径，避免实现期自相矛盾。

### 5.9 W-01（Work 积分接入网关）联动注记（v1.2 新增）

生态调研确认 `Ttungx/trae-solo-local-api` 与 `Sliverkiss/traework2api` 包装同一通道 `llm_utils_chat + function=solo_work_lite`（双实现交叉验证），W-01 从「方案已论证」进入「有实现可抄」阶段；该需求属 Trae 侧（详见 `docs/work-credit-pool-design.md`），Buddy 网关改造（批次 2）完成后其调度/熔断/协议输出层可直接复用，建议 W-01 升 P2 与批次 2 并行评估。

---

## 六、风险与合规

| 风险 | 等级 | 对策 |
|---|---|---|
| 端点为逆向/实测所得，腾讯可随时变更 | 高 | 接口层独立模块 + 双域探测 + 失败明示不静默重试；UA 版本号集中配置跟踪升级 |
| 凭证泄露 | 高 | 账号池/凭证文件入 `.gitignore`；日志/UI/异常零明文；文档掩码 |
| auth 文件写冲突（客户端运行时重写） | 中 | 客户端关闭窗口期写入 + 双源化「谁新用谁」+ 原子写/文件锁 |
| Keycloak 单点会话校验 | 低 | 多账号属同一人不同账号天然无冲突；同账号多处刷新为官方支持行为 |
| 积分规则多变（年内已变多次） | 中 | 只依赖接口不硬编码奖励值；UI 展示接口返回实际数额 |
| 频控/风控 | 低 | 签到日频、续期周频、余额 5min 缓存；不做批量注册/多开薅积分 |
| 多账号池合规 | 中 | 仅管理本人合法持有的账号；API 服务不演变为对外售卖转租；免责声明沿用 |
| 开源借鉴许可 | 低 | 参考仓库均 MIT/Apache-2.0，借鉴代码保留版权声明 |

---

## 七、附录

### 7.1 已验证端点全表（速查）

| 用途 | 端点 | 要点 |
|---|---|---|
| token 续期 | `POST www.codebuddy.cn/v2/plugin/auth/token/refresh` | `X-Refresh-Token` 头，空体 `{}` |
| Keycloak 备选 | `POST {iss}/protocol/openid-connect/token` | `grant_type=refresh_token&client_id=console` |
| OAuth 发起/轮询/资料 | `POST /v2/plugin/auth/state?platform=CLI` → `GET /v2/plugin/auth/token?state=` → `GET /v2/plugin/login/account?state=` | 独立 cookie jar；无 PKCE |
| 签到状态 | `POST /v2/billing/meter/checkin-activity-status`（回退 `/checkin-status`） | `today_checked_in` |
| 执行签到 | `POST /v2/billing/meter/daily-checkin` | 空体 `{}`；10001 已签 |
| 积分三件套 | `POST <domain>/billing/meter/get-user-resource-{summary,paid-packages,free-packages}` | 需 `X-Client-Platform: web` |
| 积分旧接口 | `POST /v2/billing/meter/get-user-resource` | `ProductCode: p_tcaca`，`Status:[0,3]` |
| 官方用量 | `POST www.workbuddy.cn/billing/meter/get-user-request-usage` | 日/周/月 |
| 对话上游 | `POST copilot.tencent.com/v2/chat/completions` | CN 区；只回 SSE |
| 模型目录 | `GET {chatBase}/console/enterprises/personal/models` | 倍率/徽章/思考档位 |
| 成长中心 | `/v2/activity/growth/buddy/travel/{status,config,depart,claim}`、`.../lottery/{chances,draw}`、`.../tasks{,/accept}`、`.../energy`、`.../streak` | 全部已实测 |
| 活动 | `/v2/activity/banner`、`get-payment-type`、`get-dosage-notify` | 低频附加 |

### 7.2 参考仓库映射（均为 MIT，除标注外）

| 仓库 | 移植价值 | 主要借鉴文件 |
|---|---|---|
| Sliverkiss/workbuddy2api（Go） | 上游适配/调度/请求改写/粘性会话 | internal/upstream/*、internal/pool/pool.go |
| **lovingfish/workbuddy-cliproxy**（v1.2 新增，CodeBuddy 转 OpenAI/Anthropic 的 CLIProxyAPI 插件，clean-room 重写自 Sliverkiss/cpa-plugin） | **上游协议避坑金矿**：审核模板黑名单最小改写、hy3 强制 effort、prompt cache 陷阱、11101 拒非流式 | 插件清洗层 `sanitizeBlockedTemplates` 设计（learn-the-design，不抄码） |
| **hailinzhao/antigravity-tools（Antigravity-Manager）**（v1.2 新增，Rust + Tauri 2 + React + axum，**与本项目同栈同类产品**） | 会话粘性指纹（前 3 消息 SHA256 + 60s 窗）、P2C 负载均衡、账号五态机、分级重试表、多段模型路由管线——网关调度层最佳工程参照 | 调度/状态机/重试模块设计（对照源码自行实现） |
| corrinehu/dsh-workbuddy-connect（TS） | 双源凭证/模型目录/DSH provider | src/auth.ts、src/catalog.ts |
| changexbc/workbuddy-switch（Rust/Tauri） | 账号管理/会话复制/CLI 轮换/进程管理/**统计页 UI 基准（v1.1 截图实测）** | crates/wb-switch-core/src/modules/{rotate,refresh,checkin,token_stats,official_usage}.rs；src/pages/{Accounts,TokenStats,CreditStats,Settings}Page.tsx；src/components/account-card.tsx |
| qinchangxv/antigravity-tools（Python） | 旧版考古/16 项清理/子 Key/协议细则 | src/modules/{api_client,checkin,oauth}.py |
| 88lin/workbuddy-auto-signin（Python） | 成长中心端点/极简头/dig 模式 | signin.py |
| tonny0812/workbuddy2api（Python） | Codex /v1/responses 投影/三协议/脱敏 | converter.py |
| **Tom6814/WorkBuddy2API**（v1.3 新增） | reasoning_content 思考链透传 + 默认深度思考、生图双端点（generations + edits）、反封号组合拳（E1 风控引用） | F-62/F-63 输出层与生图投影设计（learn-the-design） |
| **muskke/trae-api-proxy**（v1.3 新增，Go，2026-09 活跃） | Responses API 工具代执行完整范式（web_search 代理侧代执行/回喂/原生事件返回）、抓包 header 环境变量注入 | F-64 工具代执行设计（跨上游通用，不抄码） |
| **xiaolizi0v0/CliProxy**（v1.3 新增） | 多 CLI 账号环境隔离 + 严格账号模式 + 接口脱敏 | F-66 评估参照 |
| trae2api-web / trae-local-api 等其余 Trae 生态 | 见 `docs/tmp/oss-ecosystem-value-analysis.md` §3~§5（W-01/tc 解密/Responses 工具代执行等） | 一次性调研快照，实施前拉取最新源码核对 |

### 7.3 开源克隆件位置

`%TEMP%\oss-research\<repo>`（完整源码，可随时查阅）。

---

## 八、实施顺序结论

> **WorkBuddy 端点全有可运行开源佐证（风险最低）**，且 F-01/F-48/F-49/F-47/F-46 等基建已就绪——批次 1 可立即开工。与豆包批次并行度低（豆包仅剩 F-07 热切换与 F-24 端点抓包两项二期），推荐按 **批次 1 → 2 → 3 → 4 → 5** 串行推进，批次 2 的网关改造与 Trae 侧既有网关回归测试同步进行。批次 1-4 已全部完成（✅），当前处于批次 5（生态吸收与网关增强，v1.3 立项）待启动状态。
