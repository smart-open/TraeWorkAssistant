# 未来规划（Roadmap）

> **文档版本**: 2026-09-08 v1.3 · 调研分支 `feat/traecode_doubao`（已并入 `main`）
> **v1.3 变更**: 经代码核实并实施——F-01 `app_locate`（四应用三级探测）、F-49 `dig()` 宽容解析、F-46 残余（导入预览+按索引导入）已完成；F-48 完成档案表扩豆包/WorkBuddy（定位/启停就绪，快照管线随各自批次接入），移入「已完成」。
> **v1.2 变更**: 经代码核实，F-03 / F-12 / F-23 已实现（F-03 的快照管理参数化于同日补齐：`profile_*` 命令与快照管理弹框支持 `-TargetApp`），移入「已完成」；快照目录以实际实现 `data/profiles_trae/` 为准（非原计划的 `trae_ide_profiles`）。
> **v1.1 变更**: 补遗 8 项遗漏（F-03/F-12/F-23 Trae CN 移植、F-10、F-37/F-40、F-42/F-52）；修正 §四计数（34→35）；F-53 豁免说明。对照 `product-enhancement-inventory.md` 逐编号审查后修订。
> **定位**: 记录已评估但**尚未实施**的功能项，作为后续迭代的排期依据。功能点编号、来源与详细方案见 `product-enhancement-inventory.md`（53 项全量盘点）、`oss-ecosystem-research.md`、`doubao-trae-switch-plan.md`。
> **已完成基线**: 见 `product-enhancement-inventory.md` §0 及 git 提交历史。

---

## 一、本轮已完成（2026-09-07）

| 功能 | 说明 | 实现要点 |
|---|---|---|
| **F-47 进程管理增强** | 三级关闭 + exe 路径持久化 + 等待缩短 | Rust `commands/process.rs`：优雅关闭（taskkill 不带 /F，等 **3s**）→ 树杀强杀（/T /F，等 2s，轮询 250ms）→ 人工介入提示；「打开应用注入代理」与 PS 桥 `Stop-Trae`（CloseMainWindow 优先）均已接入；自动探测成功的 exe 路径持久化到 `app_settings.json` 兜底 |
| **F-08 双应用账号自动发现** | 扫描本机两个 Trae 应用的登录账号 + uid 体系修复 | Rust `commands/trae_apps.rs`：解析双应用 `storage.json`；`iCubeAuthInfo://icube-dc:<uid>` 实测为设备/数据中心级标识，当前登录账号的 Cloud-IDE uid 由本机使用痕迹推导（Trae CN `icube_gtm.users`、Trae Work vscdb `allowControl` 时间戳+键名证据），去重同时匹配 user_id 字段与 JWT 解析 uid；「扫描本机」弹框展示、标记已入池、一键加入 |
| **Trae 会员/套餐信息展示** | 账号级 + 本机应用级双视图 + 到期时间 | ① 账号级：`ide_user_pay_status` API 批量刷新缓存，账号列表套餐徽标；② 本机级：storage.json 明文键 `iCubeServerData://icube.cloudide` → `entitlementInfo`（零 API），概览页「本机套餐」卡片；③ 到期时间：`ide_user_ent_usage` 会员包提取 `expire_time`/`next_billing_time`，徽标显示「Lite · M/D到期」 |
| **账户中心 dc id 预留记录** | `RawAccount.DcID` 字段（只记录不展示） | 切换/保存登录态成功后自动回填（live storage.json → 快照）；发现入池随写；实测 dc id 为设备/数据中心级标识，仅作未来对账预留，不参与去重合并 |
| **F-46 账号库导入导出（基础版）** | 导出完整性优化 + JSON 导入 | 导出：版本号取 `CARGO_PKG_VERSION`、补 `dcId`/`addedAt` 字段、兜底纳入视图外原始账号；导入：`accounts_import` 命令兼容导出格式/原始格式/裸数组，按 uid+JWT 去重，分组按 id 合并，前端「导入账号」按钮选文件一键导入并报告新增/跳过数量 |
| **F-46 残余 导入预览 + 按索引导入** | ✅ 已完成（2026-09-08） | `accounts_import_preview` 命令解析文件、标记 uid/是否已存在/待新增分组（不写盘）；`accounts_import` 增 `only` 索引过滤；前端导入改两步式——选文件 → 预览弹框勾选（已存在默认不勾、可全选未存在）→ 按索引导入 |
| **F-03/F-12/F-23 Trae CN 移植三件套** | ✅ 已完成（2026-09-08 核实，F-03 快照管理参数化同日补齐） | ① 切换移植（F-03）：PS 桥 `-TargetApp TraeWork\|Trae` 全参数化（数据目录 / 快照根 `data\profiles_trae` / 进程名 / exe 候选表驱动），`switch_account` / `save_current_login` / `reset_device_ids` 带 `target_app`，Accounts 行「切换/保存到…」应用选择菜单；快照管理 `profile_*` 四命令与弹框支持目标应用切换（本次补齐）② 会话续期（F-12）：快照即保存；发现/入池账号与 Trae Work 共用账号池，`refresh_jwt` 自动续期，无 refresh_token 账号走 JWT 到期检测 + 提醒 ③ 余额展示（F-23）：剩余积分按 `product_id`（208 通用 / 209 Work）分类缓存，AccountView `general_credits`/`work_credits` 双字段，Credits 页与概览页「通用 X · Work Y」双展示 |
| **F-39 Trae API 暴露** | ✅ 已并入现有网关（随通用积分语义统一天然覆盖双应用） | 账号池 app 无关（uid+JWT+设备指纹），上游统一 `trae-api-cn.mchost.guru/api/agent/v3/llm_utils_chat`（通用积分 208，Trae/Trae Work 共享扣减）；F-08 扫描发现的 Trae（CN）账号入池即可被 `/v1` 服务；Anthropic 适配：新增 `POST /v1/messages`（Anthropic Messages 协议，x-api-key/Bearer 双鉴权），请求侧 `payload::anthropic_to_openai` 转 OpenAI 内部格式复用链路，输出侧 `sse::stream_convert_anthropic`/`aggregate_anthropic` 转换（message_start/content_block/message_delta/message_stop 事件序列 + tool_use 块），单元测试覆盖 text 与 tool 往返转换 |

---

## 二、Trae 侧（下一步优先）

| 编号 | 功能点 | 说明 | 预估 | 优先级 |
|---|---|---|---|---|
| ~~F-39~~ | ~~**Trae API 暴露**~~ | ✅ **已完成**（2026-09-07）：核心能力随 v3.1.0 网关 + F-08 双应用发现天然达成；Anthropic `/v1/messages` 适配已补齐，详见「本轮已完成」 | — | — |
| ~~F-03~~ | ~~**Trae CN 账号切换移植**~~ | ✅ **已完成**（2026-09-08）：PS 桥 `-TargetApp` 参数化 + Rust 三命令 `target_app` + 前端应用选择菜单；快照管理参数化同日补齐，详见「本轮已完成」 | — | — |
| ~~F-12~~ | ~~Trae CN 会话续期~~ | ✅ **已完成**（2026-09-08 核实）：共用账号池 + `refresh_jwt` 自动续期 + 到期检测提醒，详见「本轮已完成」 | — | — |
| ~~F-23~~ | ~~**Trae CN 余额展示**~~ | ✅ **已完成**（2026-09-08 核实）：`product_id` 208/209 分类 → `general_credits`/`work_credits` 双字段 + Credits/概览页展示，详见「本轮已完成」 | — | — |
| F-38 | **Trae → DSH 引导（不自研）** | 引导用户安装 `dingminhua/dsh-connect-trae`（装即用）；产品化时参照其 storage.json 发现 + loopback shim 设计 | ≈0 | P1 |
| F-41 | trae2codex 转换器 | Trae 上游为自有 `llm_utils_chat` 协议、无 Responses API，Codex CLI 不能直连；复用 `tonny0812/workbuddy2api` 投影逻辑换上游——社区空白机会 | 3 天 | P3 |

## 三、跨应用通用基建

| 编号 | 功能点 | 说明 | 预估 | 优先级 |
|---|---|---|---|---|
| F-01 | **安装位置自动识别 `app_locate`** | ✅ 已完成（2026-09-08）：Rust `env.rs::app_locate`——手动指定（settings 持久化值优先）→ 注册表卸载键 → 默认路径候选 → 运行进程反查（Get-Process Path），统一返回 `{exe, userDataDir, version, source}`；档案表驱动四应用（trae_work/trae/doubao/workbuddy）；设置页两处「自动检测」按钮已改走该命令并显示探测来源与版本 | — | — |
| F-48 | 快照桥参数化 | ✅ 已完成（2026-09-08）：桥档案表扩至四应用 `-TargetApp TraeWork\|Trae\|Doubao\|WorkBuddy`（数据目录/进程名/exe 候选/快照根按实测布局就绪）；`SnapshotLayout` 布局标记（icube/chromium/authfile），非 icube 布局的快照/恢复/设备重置显式报错待接入——豆包（User Data 目录快照，doubao §2.1）与 WorkBuddy（auth 文件双层恢复，workbuddy §2.2）的快照管线随各自应用批次实现 | — | — |
| F-49 | 响应宽容解析工具 | ✅ 已完成（2026-09-08）：`fs_utils::dig()`——沿 data/result/resp/response/info 包裹键递归下钻（限深 8 层，数组同层展开）；已采纳到 `query_ent_packs`、`query_pay_status`、ExchangeToken 解析，抗官方信封字段变动 | — | — |
| F-13 | 到期日历 | 各账号 JWT/积分/会员到期绝对时间入库 + UI 日历 + 到期前桌面提醒 | 1 天 | P1 |
| F-19 | 失败通知渠道扩展 | 桌面通知之外接入企业微信 / Server酱 | 0.5 天 | P2 |
| F-43 | CC Switch 协同 | 用户已用 CC Switch 管理多 provider；把本项目转换端点注册进其配置，不自建切换器 | 0.5 天 | P2 |
| ~~F-46 残余~~ | ~~账号库导入导出增强（残余项）~~ | ✅ 已完成（2026-09-08）：导入前 JSON preview 预览确认、按索引导入均已实现，见「本轮已完成」 | — | — |

## 四、WorkBuddy / CodeBuddy 应用（35 项，按批次）

主体清单见 `product-enhancement-inventory.md` §一，按其路线图分四批：

- **批次 1（快赢，~1.5 周）**：F-02 账号切换、F-04 多账号池、F-09 token 续期、F-15 一键签到、F-20 余额展示、F-22 多账号聚合趋势
- **批次 2（API 暴露 + 成长中心）**：F-28 网关上游、F-29 调度引擎、F-30 请求规范、F-32 运维接口、F-33 错误三态、F-16 调度增强、F-17 成长中心自动化、F-10 token 保活双源化（dsh auth.ts「谁新用谁」方案，随本批通用项实施）
- **批次 3（会话数据 + 用量 + CLI 桥）**：F-44 会话备份、F-45 会话复制、F-25 官方用量、F-26 本地 token 统计、F-06 CLI 切号桥、F-31 会话粘性、F-34 代理工程化、F-14 环境重置、F-50 OAuth 工具
- **批次 4 / 远期**：F-35 ck_xxx API Key、F-40 Codex 后端转换器（F-41 trae2codex 的前置，复用其 `/v1/responses` 投影）、F-37 DSH provider（15 模型静态目录兜底）、F-21 本地 quota 兜底、F-18 UI 坐标签到、F-27 用量快照回退、F-51 活动展示、F-36 Global 区、F-42 workbuddy-mcp 模式、F-52 WorkBuddyProxy 模式

> 注：F-53（dsh-codex-connect 参考定位）为纯参考项、无实施动作，不列入排期。

## 五、豆包应用（4 项，二期）

前置事实与方案见 `doubao-trae-switch-plan.md` §2：

- F-05 目录级快照账号切换（3~4 天，P1）
- F-11 cookie 续期定时任务（1~2 天，P2）
- F-24 会员额度展示（MITM 抓包路径，2~3 天，P2）
- F-07 cookie 级热切换（1~2 天，P3）

## 六、建议排序（近期 2 周）

1. **F-13 到期日历** —— 套餐展示已落地，到期时间入日历是自然延伸（F-49 解析加固已就位）
2. **WorkBuddy 批次 1** —— F-01 定位 / F-48 档案表已就绪，直接进入识别→切换→续期→签到→余额闭环（端点全有开源佐证，风险最低）
3. **F-38 DSH 引导页** —— 成本≈0，随手带上

> 注：原排序第 1 的 **F-39 Trae API 暴露已完成**（OpenAI `/v1` + Anthropic `/v1/messages`），从待办移除。

## 七、风险与合规（继承全量盘点）

- 全部端点为逆向/实测所得，腾讯/字节可随时变更：接口层独立模块 + 失败明示 + 不硬编码奖励数额。
- 凭证等同密码：账号池文件入 `.gitignore`，日志/UI 零明文。
- 多账号池仅管理本人合法持有的账号，不得演变为对外售卖转租。
- 参考仓库均为 MIT / Apache-2.0，借鉴代码保留版权声明。

## 附注：icube-dc id 实测结论（2026-09-07）

- `storage.json` 键 `iCubeAuthInfo://icube-dc:<uid>` 经全量验证（15 个快照 / 8 个不同 Cloud-IDE uid / 两应用 live）恒为同一值 199439841787403，且跨设备标识重置不变 → 为**设备/数据中心级标识**，非账号 id，不具备账号区分度。
- 账号池 `RawAccount.DcID` 字段已预留：切换/保存登录态成功后自动回填（backfill_dc_id_for），发现入池时随发现结果写入；**只记录不展示，不参与去重与合并**。未来若从登录页/OAuth 等渠道获得真正的账户中心账号 id，可用该字段承接对账合并。
- 真正按账号区分的设备指纹（telemetry.machineId、aha.device.device_id 等）在各账号快照内已隔离，如未来需要「账号级设备 id」可从快照提取。
