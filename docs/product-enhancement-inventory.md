# 产品功能增强点汇总（按应用分类版）

> **文档版本**: 2026-09-07 v2 · 调研分支 `feat/traecode_doubao`
> **变更说明**: 应要求将 v1 的"8 大主题分组"**拆分为按应用分类**——WorkBuddy / Trae / 豆包三大应用 + 跨应用通用基建。**F-xx 编号与 v1 保持一致**（对照表见 §6），便于追溯来源文档与参考仓库。
> **来源文档**: `workbuddy-switch-plan.md`、`doubao-trae-switch-plan.md`、`oss-ecosystem-research.md`（三份均在本分支）

---

## 0. 现有能力基线（不重复列入）

Trae Work（SOLO）侧已实现：多账号切换（快照桥 + 6 层设备指纹重置）、IDE 签到、API 网关（OpenAI 兼容，X-Credit-Type 路由）、Credits 趋势图（208/209）、device_proxy MITM、schtasks 定时、NDJSON 事件、桌面通知。`work_credit` 积分池在 `feat/work-credit-pool` 分支。

---

## 一、WorkBuddy / CodeBuddy（34 项，主体最大）

### 1.1 账号管理与切换

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-02 | WorkBuddy 账号切换 | auth 文件快照 + 用户数据双层恢复（L1 `workbuddy-desktop.info` / L2 `user-<uid>` / L3 session cookies）；轮询 `account-snapshot.json` 确认；客户端历史快照可交叉校验；客户端运行时互斥（文件监听/进程检测防写冲突） | workbuddy §2.2 | 2 天 | P0 |
| F-04 | 多账号池入库 | `workbuddy_accounts.json`（.gitignore）：uid/昵称/手机掩码/editionType/token 快照时间/refreshToken；账号 id 用 token 哈希稳定生成（`wb-` 前缀）；auth 导入兼容多种嵌套字段链 | workbuddy §2.2、oss §1.2/1.6 | 1 天 | P0 |
| F-06 | CodeBuddy CLI 切号桥 + **积分到期自动轮换** | 维护 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN`（绕过 apiKeyHelper Windows 坑）；自动轮换 = 切到"最早到期且仍有剩余"账号，防抖双约束（冷却期 + 到期差异阈值） | oss §1.5（rotate.rs） | 1~2 天 | P2 |
| F-46 | 账号库导入导出 | JSON preview/merge（按 token 去重追加/更新）/按索引导入，纯逻辑可单测 | oss §5.3 | 1 天 | P2 |

### 1.2 会话与凭证续期

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-09 | token 续期 | 首选 `POST /v2/plugin/auth/token/refresh`（X-Refresh-Token，已实测）；备选 Keycloak 原生端点；**惰性刷新**（临期 <24h）+ 每账号互斥锁 + schtasks 每周兜底；失败标 `needs_relogin` + 通知 | workbuddy §2.3、oss §1.2 | 1~2 天 | P0 |
| F-10 | Token 保活双源化 | 工具侧凭证副本（带 version）与桌面 auth 文件"**谁新用谁**"（expiresAtMs 更晚者胜出），原子写 + 文件锁——彻底规避写冲突 | oss §1.2（dsh auth.ts） | 1 天 | P1 |
| F-14 | 环境重置 / 彻底登出 | **16 项认证残留清理清单**（vscdb 认证 key、Tencent-Cloud.coding-copilot 缓存、marker、vscdb.backup 等）+ state.vscdb v10(DPAPI) 解密考古 + Keycloak logout | oss §5.4（antigravity） | 1~2 天 | P2 |
| F-50 | OAuth 扫码登录工具 | `auth/state?platform=CLI` → 浏览器扫码 → `auth/token` 轮询 → `login/account` 取 uid；每流程独立 cookie jar 防串号；无 PKCE | oss §5.1、workbuddy §1.4 | 1 天 | P2 |

### 1.3 签到与积分增值

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-15 | 一键签到 | `workbuddy_checkin.py`：状态查询（旧路径回退）→ 执行（空体 `{}`）→ `code:10001`/已签容错 → 401 刷新重试一次 → NDJSON 事件；零 token 输出 | workbuddy §2.5 | 2 天（含集成） | P0 |
| F-16 | 签到调度增强 | 每日 09:00/21:00 双时段；token 保活独立开关（keepalive_hours=[22]）；冷却复用 `account_cooldowns.json` | oss §1.6/5.1 | 0.5 天 | P1 |
| F-17 | **成长中心自动化**（纯增量积分） | `/v2/activity/growth/*` 全端点已实测：Buddy 旅行（status/config/depart/claim）、盲盒（chances/draw）、任务领奖（tasks/accept）、能量余额、连签天数 | oss §1.3（88lin 独家） | 2 天 | P1 |
| F-18 | UI 坐标点击签到兜底 | 无 API 可用时的最后手段（workbuddy-checkin 思路） | oss §0 | 1 天 | P3 |

### 1.4 积分余额与用量展示

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-20 | 余额展示 | 云端积分三件套（`X-Client-Platform: web`，origin 按 token 域路由 workbuddy.cn/codebuddy.cn）+ 旧接口回退（`ProductCode: p_tcaca`）；**免 MITM**；宽容解析（6 种嵌套 + 容量字段链式取值）；`DeductionEndTime` 到期提醒 | workbuddy §2.4、oss §1.4 | 1~2 天 | P0 |
| F-21 | 本地 quota API 兜底 | `GET 127.0.0.1:<port>/api/v1/quota`；端口发现 = 扫 `~/.workbuddy/*.port` + 端口段探测；余额查询 ≥5min 缓存 | workbuddy §2.4 | 0.5 天 | P2 |
| F-22 | 多账号余额聚合 + 趋势图 | Credits 页 WorkBuddy Tab：余额大字 + 月度消耗趋势 + 各账号对比条形图 | workbuddy §2.4 | 1 天 | P1 |
| F-25 | 官方用量统计 | `get-user-request-usage` 拉 usageToday/usage7Days/usageThisMonth | oss §1.1 | 1 天 | P2 |
| F-26 | 本地 token 统计 | 解析 `~/.workbuddy/projects` + `~/.codebuddy/projects` 的 JSONL（input/output/cacheRead/cacheWrite/cacheHitRate），按模型/项目/会话聚合，days∈{7,30,90} | oss §5.3 | 1~2 天 | P2 |
| F-27 | 积分用量快照回退 | 本地 total/remaining 时序 + 签到日志推导每日用量窗口 | oss §5.3 | 1 天 | P3 |
| F-51 | 活动信息展示 | `/v2/activity/banner` + `get-payment-type` + `get-dosage-notify`（低频附加） | oss §1.1 | 0.5 天 | P3 |

### 1.5 API 暴露与网关升级

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-28 | 网关上游接入 | 对话上游 `POST copilot.tencent.com/v2/chat/completions`（只回 SSE，非流式本地聚合）；模型目录接口；UA 伪装 `CLI/2.63.2 CodeBuddy/2.63.2` | oss §1.1 | 2 天 | P0 |
| F-29 | 账号池调度引擎 | 三因子加权随机选号（credits×10 + 闲置补偿 + 成功率×3 → Top5 二次加权）；熔断（hard_credit 到次日 + 04:00 恢复；soft_rate 60s） | oss §1.6/5.1 | 2~3 天 | P1 |
| F-30 | 请求规范与改写层 | Origin/Referer 必带、`X-No-*` 占位、**chat 绝不带 X-Refresh-Token**（红线）；强制 stream、`tool_choice` string 化（400 code=11101）、effort 降级；Claude Code 指纹清洗（可开关） | oss §5.1 | 1~2 天 | P1 |
| F-31 | 会话粘性路由 | 双段分配（先空闲账号哈希再全池）；TTL 30m 滚动续期；TOCTOU 写锁 re-check | oss §5.1 | 1 天 | P2 |
| F-32 | 网关运维接口 | `/v1/models`、`/status`（每账号画像）、`/healthz`（无健康账号 503）；请求级日志 | oss §1.6 | 1 天 | P1 |
| F-33 | 错误三态分类 | `RETRY_SAME`/`SWITCH_KEY`/`FATAL` + 硬编码标记词表（积分不足/insufficient credit 等） | oss §1.1/5.4 | 0.5 天 | P1 |
| F-34 | 代理协议工程化 | 模型级冷却渐进退避 10→20→40s；SSE keep-alive 15s；首字超时 10s 故障转移；断连 `_drain_upstream` 保 usage；健康检测 5min+抖动 | oss §5.4 | 1~2 天 | P2 |
| F-35 | `ck_xxx` API Key 模式 | API Key 直连 billing；子 Key 体系（限上游、专一/临期优先两模式、按日统计） | oss §1.6 | 2 天 | P2 |
| F-36 | Global 区支持 | `domain` 含 `.workbuddy.ai` → 全走 `www.workbuddy.ai` | oss §1.1 | 0.5 天 | P3 |

### 1.6 生态接入（DSH / Codex / MCP）

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-37 | DSH provider | 参照 dsh-workbuddy-connect：模型元数据透传（inputModalities/supportedEfforts/倍率/徽章）；**15 模型静态目录兜底 + 启动动态替换**；loopback shim + 心跳；能力读上游勿硬编码 | oss §2/§5.2 | 2~3 天 | P2 |
| F-40 | Codex 后端转换器 | 复用 `tonny0812/workbuddy2api` converter：`/v1/responses` 投影 + Anthropic `/v1/messages` + OpenAI 兼容三协议一份；`--desensitize` 脱敏、失败退回紧凑模式 | oss §6.3 | 2~3 天 | P2 |
| F-42 | workbuddy-mcp 模式 | 把 WorkBuddy（驱动 codebuddy-code CLI）注册为 Codex/Claude Code/Cursor 的 MCP 工具；`WB_SKIP_PERMISSIONS` 可控 | oss §6.3 | 1~2 天 | P3 |
| F-52 | WorkBuddyProxy 模式 | WorkBuddy 驾驶舱 + Codex 执行器：本地代理持 Codex OAuth token 调 Codex 后端（与 F-40 方向相反） | oss §6.3 | 2 天 | P3 |
| F-53 | dsh-codex-connect 参考定位 | ChatGPT 订阅→DSH（Fast Mode、5h/每周双配额窗口）——不属本项目账号管理范围，仅作 DSH 插件能力与配额 UI 参考 | oss §6.3 | 参考 | — |

### 1.7 会话数据管理

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-44 | 会话备份/恢复 | 三件套：`projects/{ws}/{cid}.jsonl` 正文 + `workbuddy.db` sessions + `edge-sync-mapping-v2.db`（`convmsg:{uid}` 云端归属）——缺一不可 | oss §1.4 | 1~2 天 | P1 |
| F-45 | 会话复制/迁移 | 新 id 复制算法：替换 sessionId → 写目标 projects → db 插行 → edge_sync_mapping 注册；复制前 backup db | oss §1.4 | 2 天 | P2 |

---

## 二、Trae（Trae Work / Trae CN / TRAE SOLO CN，7 项）

> 基线：Trae Work 侧切换/签到/网关/积分趋势已实现（见 §0）。以下为增量。

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-03 | **Trae CN 账号切换移植** | PS 桥参数化 `-AppKind Work\|Ide`；快照清单增量（storage.json iCubeAuthInfo、machineid、aha/、vscdb、Network/）；`data/trae_ide_profiles/` 隔离；现有 6 层设备指纹重置直接适用 | doubao §3.1 | 1~2 天 | P0 |
| F-12 | Trae CN 会话续期 | 快照即保存；复用现有 JWT refresh 机制；无 refresh_token 账号走"到期检测 + 提醒重新 OAuth" | doubao §3.2 | 0.5 天 | P0 |
| F-23 | **Trae CN 余额展示** | 直接复用 208/209 API（同域名 `api5-normal.mchost.guru` 同鉴权头，**零逆向**）；Accounts 页双字段 ideCredits/workCredits | doubao §3.3 | 1 天 | P0 |
| F-08 | SOLO/CN 多账号自动发现 | 扫描 `%APPDATA%\Trae CN` 与 `%APPDATA%\TRAE SOLO CN` 的 storage.json 登录账号列表 | oss §6.1（dsh-connect-trae） | 1 天 | P2 |
| F-38 | **Trae → DSH（不自研）** | 直接安装 `dingminhua/dsh-connect-trae`（Trae 模型进 DSH + 多账号切换 + Work/通用积分只读面板）；产品化时参照其 storage.json 发现 + loopback shim | oss §6.1 | 0（装即用） | P1 |
| F-39 | **Trae API 暴露** | 参照 `@casually/dsh-trae-api`：解密 storage.json 认证 → OpenAI 兼容 `/v1`（+Anthropic 适配思路）；与本项目网关同构可合并实施 | oss §6.1 | 1~2 天 | P1 |
| F-41 | trae2codex 转换器 | Trae 上游无 Responses API（自有 `llm_utils_chat` 协议），Codex CLI 不能直连；自建转换层复用 F-40 投影逻辑换上游——**社区空白机会** | oss §6.2 | 3 天 | P3 |

---

## 三、豆包（4 项）

> 前置事实：Chromium 147 壳；登录态 cookie 加密实测 **v10（DPAPI 可解密，非 app-bound）**；字节 passport `sessionid` 即凭证（多个开源项目验证）；`sid_guard` 30 天滑动续期；客户端自带 bd_sso 多账号载体与 saman 隔离开关。

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-05 | **账号切换（方案 A 目录级快照，推荐）** | 白名单快照（Cookies/Local State/leveldb/Session Storage/DoubaoStorage/saman_*）→ `data/doubao_profiles/<uid>/`；schemaVersion + 恢复前校验；优雅关停 + NDJSON 事件；uid 从 `public_config.json` 或解密 cookie 取 | doubao §2.1 | 3~4 天 | P1 |
| F-11 | 会话续期 | 每日 schtasks 用 cookie 调轻量已登录接口 → 抓 Set-Cookie 回写 `doubao_accounts.json`；过期标记 + 桌面通知；bd_sso 手动切回兜底 | doubao §2.3 | 1~2 天 | P2 |
| F-24 | 余额展示（MITM 抓包路径） | `--proxy-server` 注入 Chromium 网络栈抓会员额度 XHR（关键词 membership/entitlement/quota）；降级 = 网页内嵌注入 / 缓存值标注时间 | doubao §2.4 | 2~3 天 | P2 |
| F-07 | cookie 级热切换（方案 B，二期） | DPAPI 解密 sessionid → 账号池管理 → 进程内重写 Cookies 表重加密写回；依赖 P4 抓包成果与客户端内存态验证 | doubao §2.2 | 1~2 天 | P3 |

---

## 四、跨应用通用基建（8 项，随各应用批次嵌入实施）

| 编号 | 功能点 | 说明 | 来源 | 预估 | 建议 |
|---|---|---|---|---|---|
| F-01 | 安装位置自动识别 `app_locate` | 三级探测：注册表卸载键 → 默认路径 → 进程反查；**跨平台路径候选**（macOS/Linux/WSL + 环境变量覆盖）可一并抄入；三应用共用 | doubao §1.3、workbuddy §2.1、oss §1.2 | 1 天 | P0 |
| F-48 | 快照桥参数化 | `trae-switch-bridge.ps1` 支持 `-AppKind Work\|Ide\|Doubao\|WorkBuddy`，快照白名单/数据目录/事件管线表驱动 | doubao §3.1、workbuddy §5 | 1 天 | P0 |
| F-13 | 到期日历 | 各账号 token/积分到期绝对时间入库 + UI 日历 + 到期前提醒（WorkBuddy/Trae/豆包共用） | workbuddy §2.3 | 1 天 | P1 |
| F-47 | 进程管理增强 | 三级关闭（映像名枚举排除 crashpad helper + 自身 → 树杀宽限 8s → 强杀 → 人工介入）；`CREATE_NO_WINDOW` 拉起；exe 路径持久化兜底 | oss §5.3 | 1 天 | P1 |
| F-49 | 响应宽容解析工具 | `dig()` 信封解包（任意层包裹递归查找）+ 6 种嵌套路径兼容——所有积分/签到接口解析层统一采用 | oss §5.5、workbuddy §1.4 | 0.5 天 | P1 |
| F-19 | 失败通知渠道扩展 | 桌面通知之外接入企业微信 / Server酱 | workbuddy §2.5 | 0.5 天 | P2 |
| F-43 | CC Switch 协同 | 用户本机已用 CC Switch 管理 Codex/CC 多 provider（15721 实测）；本项目产出的转换端点注册进 CC Switch 配置，不自建切换器 | oss §6.4/6.5 | 0.5 天 | P2 |

---

## 五、路线图（按应用 × 批次）

| 批次 | 应用 | 内容 | 功能点 | 预估 |
|---|---|---|---|---|
| **批次 1（~1.5 周）** | WorkBuddy | 快赢四件套：识别→切换→续期→签到→余额 | F-01、F-48、F-02、F-04、F-09、F-15、F-20、F-22 | 6~7 天 |
| | Trae | CN 切换移植 + 余额零逆向 | F-03、F-12、F-23 | 2.5~3.5 天 |
| | 通用 | 桥参数化、宽容解析、进程管理 | F-49、F-47 | 1.5 天 |
| **批次 2** | WorkBuddy | API 暴露升级 + 成长中心 | F-28、F-29、F-30、F-32、F-33、F-16、F-17 | 8~10 天 |
| | Trae | API 暴露 + DSH 装即用 | F-38、F-39 | 1~2 天 |
| | 通用 | 双源 token、到期日历 | F-10、F-13 | 2 天 |
| **批次 3** | WorkBuddy | 会话数据 + 用量 + CLI 桥 | F-44、F-45、F-46、F-25、F-26、F-06、F-31、F-34 | 9~11 天 |
| | WorkBuddy/通用 | 环境重置、OAuth 工具、通知扩展 | F-14、F-50、F-19 | 3~4 天 |
| **批次 4** | 豆包 | 目录级切换 + 续期 + MITM 余额 | F-05、F-11、F-24 | 6~9 天 |
| | 生态 | Codex 转换器、DSH provider、ck_xxx、CC Switch | F-40、F-37、F-35、F-43、F-21 | 6~8 天 |
| 远期/机会 | — | F-07、F-18、F-27、F-36、F-36、F-41、F-42、F-52、F-08 | — |

> 顺序依据：**WorkBuddy 端点全有可运行开源佐证（风险最低）→ Trae 零逆向移植 → 豆包需新模块与抓包（二期）**。

---

## 六、编号对照与审查记录

### 6.1 v1 主题分组 → v2 应用分类映射

| v1 主题 | 拆入位置 |
|---|---|
| A 账号管理与切换（F-01~08） | F-02/04/06→WB；F-03/08→Trae；F-05/07→豆包；F-01→通用 |
| B 会话保存与凭证续期（F-09~14） | F-09/10/14→WB；F-11→豆包；F-12→Trae；F-13→通用 |
| C 签到与积分增值（F-15~19） | F-15/16/17/18→WB；F-19→通用 |
| D 积分余额与用量（F-20~27） | F-20/21/22/25/26/27→WB；F-23→Trae；F-24→豆包 |
| E API 暴露与网关（F-28~36） | 全部→WB（WorkBuddy 上游协议） |
| F DSH/Codex/MCP（F-37~43,52,53） | F-37/40/42/52/53→WB 生态；F-38/39/41→Trae 生态；F-43→通用 |
| G 会话数据管理（F-44~46） | 全部→WB |
| H 基础设施（F-47~51） | F-47/49→通用；F-50/51→WB |

### 6.2 审查结论

- v1 已完成三份源文档逐节核对（补记 6 条，见 v1 §3 / git 历史）；本次**仅重排不增删**，53 项数量守恒（34+7+4+8=53），无新增遗漏。
- 复核一遍归属合理性：F-18（UI 坐标签到）源自 workbuddy-checkin 仓库 → 归 WB 兜底；F-26 数据源含 `~/.codebuddy/projects` → 归 WB 组；F-43 为跨生态协同 → 归通用。

## 七、风险与合规提示（继承三份文档）

- 全部端点为逆向/实测所得（多仓库多语言交叉一致），腾讯/字节可随时变更：接口层独立模块 + 失败明示 + 不硬编码奖励数额。
- 凭证等同密码：账号池文件入 `.gitignore`，日志/UI 零明文；豆包 sessionid 同理。
- UA 伪装版本号（`CLI/2.63.2`）需跟踪官方升级；`copilot.tencent.com` 为 CN 区对话/签到主域（勘误记录见 oss §4.3）。
- 多账号池用于 API 服务时不得演变为对外售卖转租；仅管理本人合法持有的账号。
- 参考仓库均为 MIT（dsh-codex-connect 为 Apache-2.0），借鉴代码保留版权声明。
