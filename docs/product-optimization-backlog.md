# 产品优化需求清单（不含 WorkBuddy 接入）

> **文档版本**: v1.0 · 2026-09-10
> **定位**: 汇总各历史文档中**尚未实施**的优化与需求项，作为后续迭代的唯一待办依据。WorkBuddy 接入相关任务（F-02/04/09/15/20/22 等批次 1~4）不在本文，见 [workbuddy-product-design.md](workbuddy-product-design.md) 任务清单。
> **来源**: 由 future-roadmap.md、product-enhancement-inventory.md、doubao-trae-switch-plan.md、work-credit-pool-design.md、optimization-plan.md（已全部完成并归档删除）中的未完成项合并提炼。
> **原则**: 接口层独立模块 + 失败明示 + 不硬编码奖励数额；仅管理本人合法持有的账号。

---

## 一、待办总览

| 编号 | 功能点 | 应用域 | 优先级 | 预估 | 状态 |
|---|---|---|---|---|---|
| F-13 | 到期日历 | 跨应用通用 | P1 | 1 天 | 已完成（ExpiryCalendar 组件，WorkBuddy 批次 1 T1.7 落地） |
| F-38 | Trae → DSH 引导（不自研） | Trae 生态 | P1 | ≈0（装即用） | 待开发 |
| F-24-余 | 豆包会员额度端点抓包固化 | 豆包 | P1 | 0.5~1 天（含抓包） | 框架已完成，仅剩前置 |
| F-43 | CC Switch 协同 | 跨应用通用 | P2 | 0.5 天 | 已完成（commands/ccswitch.rs，批次 5 T5.7 落地） |
| F-07 | 豆包 cookie 级热切换（方案 B） | 豆包 | P3 | 1~2 天 | 待验证后开发 |
| F-41 | trae2codex 转换器 | Trae 生态 | P3 | 3 天 | 待开发 |
| W-01 | Work 积分（209）接入 API 网关（多活会话编排） | Trae/网关 | P3 | 未定（运维重） | 方案已论证，未决项见 §三 |
| F-44 | TRAE 多实例并行（借鉴 cockpit-tools） | Trae 生态 | P3 | 未定（调研先行） | 待调研（issue #9） |

---

## 二、条目详情

### F-13 到期日历（P1，跨应用通用）

- **需求**：各账号 JWT / 积分包 / 会员套餐的到期绝对时间入库 + UI 日历视图 + 到期前桌面提醒（WorkBuddy / Trae / 豆包共用）。
- **基础**：套餐展示已落地（`ide_user_ent_usage` 的 `expire_time` / `next_billing_time` 已解析，`F-49 dig()` 宽容解析就位）；积分包 `DeductionEndTime` 已在豆包/WB 方案中识别。
- **验收**：日历按日聚合展示到期事件；到期前 N 天（可配置）发桌面通知。

### F-38 Trae → DSH 引导（P1，Trae 生态）

- **需求**：不自研——引导用户安装 `dingminhua/dsh-connect-trae`（装即用：Trae 模型进 DSH + 多账号切换 + Work/通用积分只读面板）；应用内提供引导页/说明。
- **产品化时参照**：其 storage.json 发现 + loopback shim 设计。

### F-24-余 豆包会员额度端点抓包固化（框架已完成）

- **现状**：`doubao_quota.py` + `doubao_quota_fetch` + 前端额度条/等级/到期展示已就绪；解析层已按 entitlement 常见结构宽容适配。
- **剩余前置**：会员额度 XHR 端点须经 MITM 抓包（device_proxy.py，`--proxy-server` 注入豆包客户端）固化后填入 `settings.doubao_quota_url` 即用。建议抓包关键词 `membership|entitlement|quota|remaining|benefit`。

### F-44 TRAE 多实例并行（P3，Trae 生态，待调研）

- **来源**：issue #9（2026-09-10，用户实测仅 cockpit-tools 切换成功，建议借鉴其方法）。
- **需求**：每账号独立 `--user-data-dir` 启动多个 TRAE 实例并行运行，账号轮换不再依赖「关闭 → 快照恢复 → 重启」单实例管线，从根本上规避快照白名单随 TRAE 版本漂移失效的问题（cockpit-tools v1.3.x 的核心能力之一）。
- **参照**：`jlcodes99/cockpit-tools`（开源 Tauri 应用，同样走 storage.json 路线，多实例隔离实现与各 IDE 布局适配点可对照调研；只借鉴思路不抄代码）。
- **调研前置**：① TRAE 对自定义 `--user-data-dir` 的兼容性（设备指纹/登录态是否随目录隔离）；② 与现有快照管线（profiles/）、定时保活、代理注入的共存方案；③ 多实例的资源占用与端口冲突。
- **验收**：至少两个账号可同时在线使用，互不干扰；单实例切换管线保留为兼容回退。

### F-43 CC Switch 协同（P2，跨应用通用）

- **需求**：用户本机已用 CC Switch 管理 Codex/CC 多 provider（15721 端口实测）；把本项目产出的转换端点注册进 CC Switch 配置，不自建切换器。

### F-07 豆包 cookie 级热切换（P3，豆包方案 B）

- **需求**：不重启客户端的进程内账号热切换——读取 Cookies 表 → DPAPI 解密 → 账号池管理 → 重写 Cookies 行重加密写回。
- **风险前置**：实测豆包客户端 cookie 在 DPAPI 之下还有**一层客户端级二次加密**（明文为二进制密文），离线拿不到明文 sessionid；sessionid 池化需先验证网页版 cookie 通道。**先验证再开发**。

### F-41 trae2codex 转换器（P3，Trae 生态）

- **需求**：Trae 上游为自有 `llm_utils_chat` 协议、无 Responses API，Codex CLI 不能直连；自建转换层复用 workbuddy2api 的 `/v1/responses` 投影逻辑换上游——社区空白机会。

---

## 三、Work 积分接入网关（W-01，方案已论证）

**结论**（原 work-credit-pool-design.md 实证）：

1. `create_agent_task`（消耗 Work 积分 209）的请求体由原生层 `ai_agent.dll` 构造、闭源 `@aha-kit` 加密；用真实身份复刻仍返回 4001，**外部复刻已证伪**——必须由实时 Trae SOLO 会话自身发起。
2. 唯一可行且对外部无感的路径 = **多活会话编排（方案 A）**：每 Work 账号跑一个独立 `--user-data-dir` 的 Trae SOLO 实例，工具作编排层按池选账号 → 驱动对应实例发起任务 → MITM 捕获 SSE → `sse.rs` 转 OpenAI。增量实现：`+1 个 work_transport 模块 + credit_type 配置开关`，现有 `routes/pool/sse/payload/server` 全部不动。

**开发前必须解决的未决项**：

| # | 未决项 | 说明 |
|---|---|---|
| 1 | 如何"驱动"实时 SOLO 会话发起 `create_agent_task` | 优先确认本地命令/IPC/扩展 API；退化 headless/UI 自动化（脆弱） |
| 2 | Work 积分余额 API 来源 | 与 IDE 的 `ide_user_ent_usage` 不同，端点/字段/鉴权需新查 |
| 3 | 单实例多账号可行性 | 若 `create_agent_task` 强绑定登录会话则必须 N 实例；进程内切号可大幅降运维成本，需实验确认 |

**风险**：驱动真实客户端批量消耗 Work 积分可能触及 Trae ToS，上线前需评估；N 实例的资源占用/登录态维护/崩溃恢复（`pool.rs` 的 `SessionDead` 冷却机制天然适配）。

---

## 四、已排除项（明确不做，留档防重复提出）

| 项 | 排除原因 |
|---|---|
| F-19 失败通知渠道扩展（企业微信 / Server酱 / Bark webhook） | 用户明确不做（optimization-plan 已排除） |
| 日志导出（CSV/文件导出） | 用户明确不做（T6 设计时明确排除） |
| `/v1/embeddings` 端点 | 上游无对应能力，明确返回 501，不做假实现 |
| C 方案：逆向原生层复刻 `create_agent_task` | 已证伪（4001）+ 闭源逆向合规风险 |
| F-53 dsh-codex-connect 参考定位 | 纯参考项、无实施动作 |
| 账号池调度权重/时段轮询（T10 裁剪） | 避免过度设计 |

---

## 五、建议排序

1. **F-13 到期日历** —— 套餐/到期时间解析已全部就位，自然延伸
2. **F-24-余 豆包额度端点固化** —— 半天抓包即可点亮已建好的框架
3. **F-38 DSH 引导页** —— 成本≈0，随手带上
4. **F-43 CC Switch 协同** —— 0.5 天
5. F-07 / F-41 / W-01 —— 均有验证前置或较大投入，按需启动
