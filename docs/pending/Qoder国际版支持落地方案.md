# Qoder 国际版支持落地方案（Qoder Global / qoder.com 体系）

> - **日期**：2026-10-22（v1.4 开源情报回填轮）
> - **状态**：待评审（pending）
> - **v1.4 变更说明**（本轮对两个开源 DSH 插件仓库做**源码级深度分析**并回填情报，映射表见附录 B.3）：① **chat/目录端点路径开源互证**——masknull/dsh-qoder-connect `endpoints.ts` 的双区端点表与 v1.3 本机实测六域拓扑**完全一致**，并给出完整路径参数（chat `?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1`、model list `?Encode=1`、campaigns/claim 锚定 openapi 域）——R-G3/R-G8 从「全参数待探」降为「现版取值复核」；② **umid 机器身份门控（重大情报）**——Intl campaigns 对不带 `Cosy-MachineToken/Code/Type` 三头的请求只回 VIEW_DETAILS 横幅（签到行 `active:false` 且不报错，hdhgsysh 2026-09-27 实测），比 CN「缺头静默空列表」更隐蔽，R-G4/§4.3 补 umid 底稿；③ **官方错误码文档存在**——`docs.qoder.com/cli/sdk/errors` 官方公开 quota/request/runtime 三族，quota 族（110/113~122）是唯一覆盖 HTTP status 的家族，R-G3 错误码探测从「纯抓包归纳」降为「文档核实+抓包复核」；④ **错误分诊三护栏**——「先认 quota 码再认队列标记」（110=日计费耗尽重试无益 / 10605=排队 transient）、队列 4 标记须 ≥2 共现、401/403 带队列标记归 RATE_LIMIT 而非重认证；`enable_thinking:false` 对 GLM-5.3-Flash/Kimi-K3 触发 `provider_error 1210` 须整字段省略（§3.6.3-10）；⑤ **PAT 管理页收口**——`qoder.com/account/integrations`（两仓库 README 同指）；⑥ **jobToken 交换双通道并存**——归档 §2.7 的 center 域 algo 路径 vs 开源的 openapi 域 `/api/v1/jobToken/exchange`，R-G6 实测定稿；⑦ **campaigns 契约细节**（双过滤器 actionType+claimStatus、顶层 claimable 不可信、claim 每次重读 campaigns、replayed 幂等不报 amount、每日 10:00 发新轮新 campaignId）与 **price_factor 折后价语义**（`before_promotion_price_factor` 才是窗口外原价，直接抄 price_factor 会把工作时段成本低估数倍）回填 §4.3/§4.5/R-G8；⑧ **`qoder-global` 命名第三方互证**（masknull 同款 china=`qoder`/global=`qoder-global`，决策点 1 增信）；⑨ machine_id 读取链（官方 CLI 优先→自有 seed 兜底、`wx` flag 防创建竞争）参照进 §4.4。
> - **v1.3 变更说明**（本轮按用户提供的两处国际版安装目录做**本机实测**，修正 v1.2 三处认知偏差）：① **双端修正（推翻 D-6 单端假设）**——国际版实测同为 **Work 端 + IDE 双端并存**（Work：`Qoder.exe` / `%APPDATA%\com.qoder.app.stable`；IDE：`Qoder IDE.exe` / `%APPDATA%\Qoder` + `~/.qoder`），与国内双命令底座（[common.rs:593/:599](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L593)）同构；`qoder_open_work` 由 ④裁剪恢复为 ②参数分区，快照/切换恢复双目标（`profiles_qoder_global_work` / `profiles_qoder_global_ide`），双路径配置键（`qoder_global_ide_path` + `qoder_global_work_path`）落定；② **端点拓扑实测回填**——Intl 六域（api3 推理 / api2 安全+遥测 / center / openapi / api6 CN-CDN 加速 / repo2 codebase）vs CN 双域（gateway 单域 + openapi），`QoderRegionEndpoints` 字段集按区不裁齐；③ **归档情报修正**——`~/.qoder/.auth/` 现版为 `machine_id` 单文件（uuid4 明文，**非归档所述 id+user 双文件**），user 解密文件未出现，CLI 解密导入通道挂 R-G9 登录复测后接线；④ **指纹覆写三处实测落定**（§4.4：Work `auth.machine-id` / IDE `SharedClientCache\cache\id` / CLI `.auth\machine_id`）；⑤ **Intl 模型阵容 14 项实测回填**（§4.5，dynamic-text/qoder.json）；⑥ Gate R-G1/G3/G9 部分收口标注（证据来源「本机实测 2026-10-22」）。
> - **v1.2 变更说明**（本轮参考 `docs/completed/BuddyAI对齐国内Buddy六页开发计划.md` 的功能对齐逻辑补齐三层框架）：① §3.5.0 新增**能力边界总表**——国内 Qoder 功能逐项 → 国际版处置六类标注（直接可做 / 等效实现 / Intl 特有新增 / 降级等效 / 国际上游不适用 / 后续项），每项带依据，评审一眼看清「做哪些、怎么做、为何不做」；② §6 分阶段实施计划从 4 个基础设施导向粗阶段细化为 **P0-P6 页面推进式**（对齐 BuddyAI 计划「逐步开发、每阶段独立可验收」逻辑：P1 池骨架+账号管理 → P2 签到页 → P3 积分/看板 → P4 网关/API 服务 → P5 设置收尾 → P6 可选后续），每阶段绑定 Gate 前置、独立验收标准，并就地标注「国际不适用」UI 提示项（不静默隐藏）；③ 新增**统一验收口径**（cargo test + tsc + 逐页对照走查 + Gate/回归/安全三条红线）。
> - **v1.1 变更说明**（本轮按「功能菜单 + API 管理尽量与国内 Qoder 功能一致」要求做对齐增强）：① §3.5 从 7 行粗粒度对照表扩充为 **10 个子节的菜单级逐项对照**（页面/卡片/按钮/弹框/命令/配置键六层全量，含 33 个 Tauri 命令逐一标注复用方式）；② 新增 **§3.6「API 管理详细对照」专章**（QoderApiService 页面逐区块 + 全局 API 管理集成点 + 后端网关改造点 10 项明细）；③ **修正 COSY 双域签名参数**（v1.0 把 openapi 域与 gateway 域两套参数混写为一套，本轮按代码核实：openapi 域 `Cosy-ClientType: "10"` + `Cosy-Version: "0.4.3"`，gateway 域 `Cosy-Clienttype: "5"` + `Cosy-Version: "1.1.38"`）；④ 全部代码锚点修正为 `AIWorkAssistant_main` 实际路径与行号（v1.0 误写 `_feature` 目录）；⑤ 新增关联文档 `docs/completed/开源生态仓库价值分析.md`。
> - **关联**：`docs/completed/BuddyAI对齐国内Buddy六页开发计划.md`（功能对齐方法论参照：能力边界总表 + 页面推进式阶段计划 + 统一验收口径，v1.2 采纳）/ `docs/completed/Buddy国际版支持落地方案.md`（独立池范式先例，2026-10-15 Gate 已全清零）/ `docs/completed/Qoder平台支持设计方案.md` v1.4.2（CN 版全量交付底稿，§5.8 页面表 + §5.10 指纹 + §5.11 网关池方案 + §六 R-1~R-13）/ `docs/completed/Qoder外部项目调研归档.md`（国际站实现级情报底稿）/ `docs/completed/开源生态仓库价值分析.md`（合规红线与网关工程参照来源，见附录 B）/ backlog F-80-余（Global 区 2026-10-02 决策留档：账号-区域关系 + COSY Global 域签名两大未决项）
> - **结论先行**：国际版走**独立池 `qoder-global`**，与国内 `qoder` 池账号、凭证、调度、指纹、记账、UI 六面完全隔离；内核（`qoder_common` / `qoder_checkin` / `qoder_credits` / `qoder_sign` / `qoder_upstream` / `qoder_route`）**区域参数化复用，不复制代码**。两个历史未决项均有现成底稿——① 账号-区域关系 → Gate 探测 R-G1；② COSY Global 域签名 → **调研归档 §2.1/2.2/2.7 即 Global 域 COSY 签名链完整底稿**（QoderGateway 全部面向 `.sh` 国际域），仅差现版客户端复核。
> - **战略动因**：Qoder CN 桌面端官宣 **2027-09-05 停运**（仅剩约 11 个月），国际版是 Qoder 生态长期存续的唯一延续路径；且国际站免费积分稳态 **≈3,000 Credits/月/账号** + **Qwen3.8-Flash 0 系数免费模型**，网关侧多一个国际模型池（与 Trae / wb / buddy-ai / qoder-CN 形成互补）。Buddy 国际版已开「独立池」先例，架构范式已被项目方接受。
> - **开工硬前置**：§5 Gate 探测清单（R-G1~G9）——一次真实国际账号登录收口阻塞层，**缺口未闭合前对应模块只做骨架不接线**（沿用 F-80「不写无端点的假实现」红线）。

---

## 📋 1. 背景与目标

### 🌐 1.1 域名族谱与角色

| 域名 | 产品定位 | 本方案角色 |
|---|---|---|
| `qoder.com` | 国际版门户 / 官网 / 账号中心（Settings > Usage 查余额） | 设备流授权页（`/device/selectAccounts`）、PAT 管理页、余额查看入口 |
| `openapi.qoder.sh` | 国际 OpenAPI | token 刷新（`/api/v1/deviceToken/refresh`、`jrt-`→`/api/v1/jobToken/refresh`）、quota（`/api/v2/quota/usage`）、sash campaigns（base 已实测锚定 = 本域 endpoint-cache `openapiEndpoint`，claim 链路 R-G4；**开源互证 v1.4**：masknull endpoints.ts 同域锚定）、设备流 poll |
| `center.qoder.sh` | 国际 algo 中心 | PAT→jobToken 交换（`/algo/api/v3/user/jobToken?Encode=1`，appcode+signature 头，调研归档 §2.7） |
| `api3.qoder.sh` | 国际对话/推理网关 | **chat 上游执行域**（`QoderRegion::Global` 已预留，见 [qoder_upstream.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_upstream.rs#L96)），请求形态待 R-G3 |
| `api2.qoder.sh` | 国际安全/遥测域（v1.3 实测） | `securityEndpoint`（quota Bearer 域）+ IDE 遥测 `…/apm`（product.json `traceTelemetryEndpoint` 实测）+ IDE preferredInferenceNode/NesNode |
| `api6.qoder.com.cn` | 国际 fast 域（CN CDN 加速，v1.3 实测） | `fastEndpoint` 实测指向 `.com.cn` CDN——**Intl 亦借道 CN 域做下载加速，区域判定不可仅凭域名后缀**（对 G-1/G-4 的区域分流实现是关键提醒） |
| `repo2.qoder.sh` | 国际 codebase 索引域（v1.3 实测） | IDE `region-config-cache.json` `preferredCodebaseNode` |

> **v1.3 实测锚定**（2026-10-22）：上表 Intl 全部域名已在 `~/.qoder/.cache/endpoint-cache.json`（五域）与两端 `product.json` / `region-config-cache.json`（api2 遥测、repo2 codebase）实证；CN 对照实测 = `gateway.qoder.com.cn` 单域承载 endpoint/fast/security/center 四类 + `openapi.qoder.com.cn` 独立（`~/.qoder-cn/.cache/endpoint-cache.json`）——**CN = 双域、Intl = 六域，拓扑不同构**。

对应国内族（现状不动）：

| 国内域 | 角色 |
|---|---|
| `openapi.qoder.com.cn` | OpenAPI：sash campaigns / claim、quota、userinfo、jobToken exchange、设备流（COSY 头：`Cosy-ClientType: "10"` + `Cosy-Version: "0.4.3"`，[qoder_common.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_common.rs#L37-L46)） |
| `gateway.qoder.com.cn` | CN 推理网关（`QODER_CHAT_URL`，COSY 签名，`Cosy-Clienttype: "5"` + `Cosy-Version: "1.1.38"`，[qoder_sign.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_sign.rs#L41-L43)） |
| `qoder.cn` / `qoder.com.cn` | CN 门户 / PAT 管理（`/account/integrations`）/ 设备流授权页 |

> **COSY 双域参数速记**（本轮代码核实，评审重点）：Qoder 客户端对 **openapi 域**（账号/积分/签到类请求）与 **gateway 域**（对话/模型类请求）使用**两套不同的 COSY 签名参数**——openapi 域走 `qoder_common.rs` 常量（ClientType `"10"`、Cosy-Version `"0.4.3"`、UA `"Qoder"`、machine_os `x86_64_win32`），gateway 域走 `qoder_sign.rs` 常量（Clienttype `"5"`、Cosy-Version `"1.1.38"`、machine_type `"5"`）。国际版 `QoderRegionEndpoints` 必须按**域 × 参数**二维建模，不能只按区域一维（v1.0 的错误即源于此）。

### 🎯 1.2 目标

1. **账号接入**：国际账号可登录（OAuth 设备流）/ 导入（PAT / CLI 本地登录态）、可管理、可查积分，入 `qoder-global` 独立池。
2. **自动领取**：每日领取 100 Credits（官方每日活动）自动化，幂等 claim，10:15 单次调度覆盖。
3. **网关接入**：`qoder-global` 档位接入 API 管理统一出口 `/v1/*`，Global 模型目录可路由（解除现有 `entry_is_global_only` fail-closed 404 限制）；Qwen3.8-Flash 0 系数免费模型进目录。
4. **不交织**：国内 `qoder` 池数据、配置、调度、失败域**零改动**；国际池故障不得触发国内池换号/熔断，反之亦然。
5. **复用内核**：qoder 模块族按区域参数化复用，不复制代码（对齐 buddy-ai 内核复用原则）。

### 🚫 1.3 非目标

- ❌ **不做注册/试用类能力**：官方明文「多建试用账户将冻结」（虚拟机登录亦不支持）——比 CN 风控红线更硬，**仅面向存量个人账号**，不做批量注册、验证码绕过。
- ❌ 不做 QoderWake / Cloud Agents / 移动端 / 网页版的独立接入（账号级 Credits 全端共享，无需逐端接入）。
- ❌ 不做 JetBrains 插件独立端（共享账号，自然覆盖）。
- ❌ 不做国际版模型倍率自动抓取（首版静态配置，官网实际值人工修正口径）。
- ❌ 不做 CN Credits 与 Intl Credits 的任何互通/换算（账号体系独立、定价不等价）。
- ❌ **合规红线**：国际版全部能力仅限本机自用（网关只监听 `127.0.0.1`、不提供对外服务、不商用、不分发凭证）——与开源生态分析 §6 合规备注同口径（附录 B）。

---

## 📊 2. 现状盘点

### ✅ 2.1 已实现（可直接复用的 CN 底座）

| 能力 | 位置 | 国际版适配说明 |
|---|---|---|
| 区域枚举预留 | [qoder_upstream.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_upstream.rs#L80-L97) `QoderRegion::{Global, Cn}`，`Global => "https://api3.qoder.sh/"` | 端点 base 已就位，仅请求形态待探（R-G3） |
| Global 专属模型判定 | [qoder_route.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L351) `entry_is_global_only()`：双路 resolve 确定性证据 + fail-closed 404 | 接线后此守卫**按池放行**（Global 池内合法路由），CN 池行为不变 |
| COSY 签名链（双域） | openapi 域：[qoder_common.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_common.rs#L37-L50)（`Cosy-ClientType: "10"`、`Cosy-Version: "0.4.3"`、UA `"Qoder"`、machine_os `x86_64_win32`）；gateway 域：[qoder_sign.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_sign.rs#L41-L50)（`Cosy-Clienttype: "5"`、`Cosy-Version: "1.1.38"`、machine_type `"5"`） | 区域 × 域二维参数化（Intl 参数取调研归档 §2.1/2.2：cosyVersion 0.1.43、clientip 硬编码、RSA 公钥加密 temp_key，现版复核 R-G3） |
| 设备流登录 | [oauth.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/oauth.rs)（CN：`qoder.cn` 授权页 + deviceToken/poll） | 区域参数化（Intl：client_id `e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb`、`qoder.com/device/selectAccounts`、`openapi.qoder.sh/api/v1/deviceToken/poll`，归档 §2.8 底稿，R-G2 复核） |
| 签到引擎 | [qoder_checkin.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_checkin.rs)（2363 行：campaigns → claim 幂等 → 四分类 → 复查恢复，最小头 4 头、CrossProcLock 3000ms、90 天滚动） | 端点 base + `Cosy-ClientType`（openapi 域参数族）+ **umid 三头**（`Cosy-MachineToken/Code/Type`，§4.3 底稿）区域化（R-G4） |
| 积分引擎 | [qoder_credits.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_credits.rs)（1244 行：三通道 + 600s 缓存 + stale-on-error + 逐包明细 source: plan/addon） | FEFO/Plan/Add-on 扣减口径**官方文档逐字同构**（见附录 A），仅 base 域切换（R-G5） |
| token 生命周期 | [qoder_common.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_common.rs) `ensure_fresh` + 6h 兜底刷新 | Intl 前缀分派同构：`jrt-`→jobToken/refresh、其余→deviceToken/refresh（归档 §2.5，R-G7） |
| 每账号稳定指纹 | CN 方案 §5.10 模型（CN：machine_id 32hex + 本地覆写三文件） | **Intl 指纹格式不同**：machine_id = uuid4 36 字符、machine_token 68 字符 base64url、machine_type 18 hex（归档 §2.3 + v1.3 三端实测互证 uuid4）→ 生成器按区分派（R-G9 复核） |
| PAT 通道 | [accounts.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/accounts.rs#L244) PAT 导入 + `exchange_job_token` | Intl PAT 换取路径不同：`center.qoder.sh/algo/api/v3/user/jobToken?Encode=1` + appcode/signature 头（归档 §2.7，R-G6） |
| 调度三件套 | TASKS 注册 + schtasks 双轨 + 启动补签 | 纯注册新任务行（§4.5） |
| 前端五页 + 全局集成 | [src/pages/qoder/](file:///d:/code/AIWorkAssistant_main/src/pages/qoder)：Overview / Accounts / Checkin / Settings / ApiService 五页 + HelpModal；全局侧 Dashboard qoder 区块（4 TABs）、ResourceSummary 预设、UsageStatsPanel qoder 桶、PoolHealthBadges | 区域 Tab 分区展示（§4.6 + §3.5 逐项对照） |

### ⚠️ 2.2 缺口清单（本方案工作量主体）

| # | 缺口 | 位置 | 现状 |
|---|---|---|---|
| G-1 | 端点常量单源化 | [qoder_common.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_common.rs#L30-L50) `OPEN_API_BASE`/`WEB_BASE`/`COSY_CLIENT_TYPE`/`CLIENT_USER_AGENT` 等 6 常量硬编码 CN 值 | 需提升为 `QoderRegionEndpoints` 结构体（**按域 × 参数二维**承载 CN/Global 两份静态实例，对齐 Buddy 方案 §4.1 同款手法） |
| G-2 | Intl 登录/导入入口 | oauth.rs / accounts.rs | 设备流 client_id、授权页、poll 域均 CN 硬编码；PAT 管理页 URL 与换取端点 CN 专用 |
| G-3 | 签到活动 Intl 化 | qoder_checkin.rs | campaigns/claim 端点 + `Cosy-ClientType`（openapi 域）取值 Intl 版待探（R-G4）；**Intl 无「每日登录奖励」双活动**（CN 独有），活动面为单一「每日领取 100」 |
| G-4 | chat 上游 Global 分流 | [qoder_route.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L659)（两处区域边界注释 ：659/:1312，v1 恒 CN 区执行 + `entry_is_global_only` 404） | 接线 Global 区：`QODER_CHAT_URL` 区域化 + dispatch 源判定按池分流（代码注释已预埋改造点） |
| G-5 | 模型目录/倍率 Intl | qoder_catalog.rs（`gateway.qoder.com.cn/algo/api/v2/model/list`） | Intl 目录源已开源互证 `{baseUrl}algo/api/v2/model/list?Encode=1`（baseUrl=api3.qoder.sh），剩倍率字段与响应结构实测（R-G8）；Qwen3.8-Flash 0 系数静态标注；**price_factor 折后价语义护栏**（§4.5，v1.4） |
| G-6 | 池隔离 | qoder_route.rs `pool_constraints("qoder")`、池存储四表 | Intl 账号与 CN 账号**不得混居**：独立池 `qoder-global`（§3.4 隔离清单） |
| G-7 | 记账口径 | usage bucket | 无 `qoder-global` bucket，国际用量与国内混计 |
| G-8 | 指纹生成器分区 | 指纹生成与本地覆写（CN 32hex 格式） | Intl machine_id = uuid4 格式，生成器 + 覆写目标路径按区分派 |
| G-9 | 前端分区 | 五页 + PoolHealthBadges / API 服务档位 | 无区域列/分区 UI；`qoder-global` 档位默认关 |

### 🧬 2.3 产品端差异（CN 全家桶 vs Intl 双端并存，v1.3 本机实测修正）

| 维度 | CN（现状） | Intl（v1.3 实测 2026-10-22） |
|---|---|---|
| 端形态 | 全家桶：IDE = QoderWork = Desktop 同一产品 | **双端并存（与 CN 同构，推翻 v1.2「单桌面端」认知）**：① Work/Agent 桌面端 `Qoder.exe`（Electron，安装于 `AppData\Local\Programs\Qoder`，product.json：`appId=com.qoder.app`、`dataDirectoryName=com.qoder.app.stable`、`protocolScheme=qoder`，插件族 qoder.canvas/sites/office 等）；② Qoder IDE `Qoder IDE.exe`（VS Code fork 1.106.3 基座，产品版本 1.35.0，commit 2026-10-19，安装于 `AppData\Local\Programs\Qoder IDE`，`dataFolderName=.qoder`、deeplink `qoder-ide`、内部代号 lingma）。账号级 Credits 全端共享 |
| Work 端数据目录 | `%APPDATA%\com.qodercn.app.stable\` | `%APPDATA%\com.qoder.app.stable\`（含 `auth.machine-id` 指纹文件，uuid4 实测） |
| IDE 数据目录 | `%APPDATA%\QoderCN\` | `%APPDATA%\Qoder\`（`SharedClientCache\cache\` 存登录态：`status.json`（logged_in / personal_standard / user_id）+ `id` + `machine_token.json`） |
| CLI 认证目录 | `~/.qoder-cn`（R-3 实测无明文凭证） | `~/.qoder/.auth/`（**v1.3 实测：`machine_id` 单文件 uuid4 明文 + `.credential-transaction`**；归档 §2.4 所述 `user` 解密文件（AES-128-CBC(key=IV=machine_id[:16]) 身份 JSON）**现版目录未见，待登录复测**——解密导入通道保留设计、挂 R-G9 复核前提） |
| IDE 区域配置 | —（CN 单区） | `%APPDATA%\Qoder\User\region-config-cache.json`（preferredInferenceNode / preferredNesNode = `api2.qoder.sh`、preferredCodebaseNode = `repo2.qoder.sh`，v1.3 实测） |
| PAT 入口 | `qoder.com.cn/account/integrations` | `qoder.com` 账号中心（具体路径 R-G6 探测） |
| 每日活动 | 双活动（0 点签到 + 10 点登录奖励，campaigns 同源） | 单活动「每日领取 100 Credits」（10:00 开窗，campaigns 体系，Intl 自 v0.8.18 统一走 campaigns——归档 §3.2 情报） |
| 版本锚点（v1.3 实测） | CN CLI 现网版本 | Intl CLI 1.1.69（Work 端内嵌 `@qoder-ai/qoder-agent-sdk` 驱动，`--model qfmodel` = Qwen3.8-Flash 内部代号）；模型 catalog 按 user_id 缓存于 `~/.qoder/.models/<user_id>/catalog-v6` |

---

## 🏗️ 3. 总体设计：独立池 `qoder-global`

### 🧭 3.1 设计原则：「不交织」六条硬约束（照抄 buddy-ai 范式）

1. **账号分池**：国际账号入 `qoder-global` 池（独立表 + 独立 token store 键空间），禁止与 `qoder` 池混居；导入命令区域必选校验（显式选 Global 时校验凭证域特征，二者不一致直接拒绝）。
2. **配置分键**：调度、签到、积分同步等配置键独立（`qoder_global_*`），`Settings` 不复用国内字段。
3. **失败域隔离**：换号轮换、冷却、熔断在池边界内闭环；`/healthz` 按池独立上报 selectable。
4. **指纹分区**：每账号稳定指纹（非随机轮换）**双区共用原则**，但生成算法与覆写目标按区分派（§4.4）。
5. **记账分区**：`UsageBucket::QoderGlobal => "qoder-global"`，口径不变。
6. **UI 分区**：五页均以区域维度分区展示，不出现同列表混排。

### 🗺️ 3.2 架构与数据流

```text
                     网关统一出口 /v1/*（不变）
                              │
                     dispatch（模型路由）
                              │
            ┌─────────────────┼──────────────────┐
            │                 │                  │
      qoder 池（CN，不动）  qoder-global 池（新增）  trae/wb/buddy-ai/custom 池
            │                 │
            │     QoderRegionEndpoints{ region: Global }
            │                 │
            └────────┬────────┘
                     │  区域参数化内核（qoder_common/qoder_checkin/
                     │  qoder_credits/qoder_sign/qoder_upstream 按区分流）
                     ▼
   CN: gateway.qoder.com.cn（endpoint/fast/security/center 四类同域）
       + openapi.qoder.com.cn（独立 openapi）
   Intl（v1.3 实测 endpoint-cache.json，六域）:
       api3.qoder.sh（推理 endpoint）+ api2.qoder.sh（security + 遥测 apm）
       + center.qoder.sh（PAT 交换）+ openapi.qoder.sh（openapi）
       + api6.qoder.com.cn（fast，CN CDN 加速）+ repo2.qoder.sh（codebase 索引）
```

**内核复用方式**：qoder 模块族内部引入 `region` 维度（`QoderCreds` 增 `global_region: bool` 或池级上下文注入），`QoderRegionEndpoints` 结构体承载全部端点/常量差异（**按域 × 参数二维**：openapi 域参数族 + gateway 域参数族各自独立）；`qoder_route` 的取号、冷却、粘性按池视图绑定，`entry_is_global_only` 守卫改为「CN 池禁 Global 专属、Global 池全放行」。

### 📌 3.3 端点矩阵（归档情报预填 + v1.3 本机实测拓扑回填，标注探测状态）

> **v1.3 实测拓扑**（`~/.qoder/.cache/endpoint-cache.json`，2026-10-22）：Intl 五域——`endpoint=api3.qoder.sh`、`securityEndpoint=api2.qoder.sh`、`centerEndpoint=center.qoder.sh`、`openapiEndpoint=openapi.qoder.sh`、`fastEndpoint=api6.qoder.com.cn`（CN CDN 加速）；另有 IDE 侧补充 `preferredCodebaseNode=repo2.qoder.sh`（codebase 索引域）与遥测 `api2.qoder.sh/apm`（IDE product.json）。CN 对照实测：`gateway.qoder.com.cn` 单域 + `openapi.qoder.com.cn` 独立。**结构体落点**：`QoderRegionEndpoints` 字段集按区不裁齐——Global 多出 codebase / apm 字段（CN 实例留空），CN 不新增字段。

| 用途 | CN（现状） | Intl（目标态） | 状态 |
|---|---|---|---|
| campaigns 列表 | `{openApi}/sash/api/v1/me/campaigns` | `{openApi?}.qoder.sh/sash/api/v1/me/campaigns`（开源互证 v1.4：masknull endpoints.ts 同域锚定） | ⏳ R-G4（归档 §3.2：Intl 自 v0.8.18 统一 campaigns，`claimStatus=="CLAIMABLE"` 唯一权威；**umid 门控底稿见 §4.3**——须带 `Cosy-MachineToken/Code/Type` 三头，否则 `active:false` 且不报错，hdhgsysh 2026-09-27 实测） |
| campaign claim | `.../campaigns/{id}/claim`（幂等 replayed） | 同路径 | ⏳ R-G4（开源互证 v1.4：masknull 每次先重读 campaigns 取 id 不缓存；`replayed:true` 上游不再发放且响应不带 amount） |
| 用量查询 | `{openApi}/api/v2/quota/usage` | `openapi.qoder.sh/api/v2/quota/usage`（Bearer security_oauth_token） | ⏳ R-G5（归档 §2.5 底稿；开源互证 v1.4：masknull 另有 `/api/v2/user/plan`（套餐）与 `/api/v3/user/status`（账号状态）两个配套端点可一并接入） |
| 设备流 poll | `qoder.com.cn/device/selectAccounts` + `/api/v1/deviceToken/poll` | `qoder.com/device/selectAccounts?nonce=&challenge=&challenge_method=S256&machine_id=&client_id=e883ade2-...` + `openapi.qoder.sh/api/v1/deviceToken/poll`（GET，404=等待，1s 间隔 300s 超时） | ⏳ R-G2（归档 §2.8 底稿，client_id 现版复核） |
| token 刷新 | `/api/v1/deviceToken/refresh` | `jrt-`→`/api/v1/jobToken/refresh`（token_key="token"）；其余→`/api/v1/deviceToken/refresh`（token_key="device_token"）；UA `qoder/1.1.16`；REFRESH_INTERVAL 6h | ⏳ R-G7（归档 §2.5 底稿） |
| PAT 换 jobToken | `POST /api/v1/jobToken/exchange`（pt-→jt-） | **双通道并存待定稿**：归档 §2.7 底稿 = `POST center.qoder.sh/algo/api/v3/user/jobToken?Encode=1`（appcode=cosy + signature=md5("cosy&{SECRET}&{RFC1123}") 头，date 用 RFC1123 GMT；响应 `id→aid/uid, name, userType, securityOauthToken, refreshToken`）；开源参照（masknull）= `POST openapi.qoder.sh/api/v1/jobToken/exchange`——两形态可能对应不同客户端代际/凭据类型，R-G6 实测定稿 | ⏳ R-G6（`?Encode=1` 自定义 base64 三段重排 + 65 字符表，归档 §2.7；PAT 创建入口已收口 `qoder.com/account/integrations`） |
| chat 上游 | `gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?...&Encode=1` | `api3.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1`（路径与查询参数**开源互证 v1.4**：masknull endpoints.ts；域与 v1.3 实测一致） | 🔶 R-G3（MITM 复核现版签名取值与流式帧；路径模板已开源锚定） |
| 模型目录 | `gateway.qoder.com.cn/algo/api/v2/model/list` | `{baseUrl}algo/api/v2/model/list?Encode=1`（baseUrl=api3.qoder.sh，开源互证 v1.4：masknull endpoints.ts） | 🔶 R-G8（目录源已锚定，剩倍率字段与响应结构实测） |
| IDE 遥测 | CN 端点缓存无独立域 | `api2.qoder.sh/apm`（IDE product.json `traceTelemetryEndpoint`，v1.3 实测） | ✅ 已实测（2026-10-22） |
| codebase 索引 | CN 端点缓存无独立域 | `repo2.qoder.sh`（IDE region-config `preferredCodebaseNode`，v1.3 实测） | ✅ 已实测（2026-10-22） |
| COSY 签名参数（openapi 域） | `Cosy-ClientType: "10"`、`Cosy-Version: "0.4.3"`、UA `"Qoder"`、machine_os `x86_64_win32` | cosyVersion "0.1.43"、cosy-clientip 硬编码 `169.254.198.161`、UA `Go-http-client/2.0`、login-version v2、RSA 1024 公钥（归档 §2.1/2.2 全量） | ⏳ R-G3/R-G4 复核现版 |
| COSY 签名参数（gateway 域） | `Cosy-Clienttype: "5"`、`Cosy-Version: "1.1.38"`、machine_type `"5"` | 同左侧结构，取值按 R-G3 现版抓包 | ⏳ R-G3 复核现版 |
| PAT 管理页 | `qoder.com.cn/account/integrations` | `qoder.com/account/integrations`（开源实证 v1.4：hdhgsysh 与 masknull README 均指向此直达路径） | 🔶 部分收口（剩余：PAT 前缀族现版验证 + 换取通道 R-G6） |

### 🏷️ 3.4 隔离清单（命名约定）

| 维度 | CN（现状，不动） | Intl（新增，标识定稿 `qoder-global`，评审可改） |
|---|---|---|
| 产品标识 | qoder | **qoder-global**（池 id / app_id / bucket / 前端档位全链统一） |
| 池 id | `qoder` | `qoder-global` |
| 账号/凭证/签到/积分存储 | `qoder_accounts` / `qoder_tokens` / `qoder_checkin_results` / `qoder_credits_history` 四表（schema v3） | `qoder_global_*` 四表（schema v3→v4 迁移；若 store 现状更宜分库则从其约定，评审决策点 D-3） |
| token store 键空间 | `qoder_tokens` | `qoder_global_tokens`（同文件独立命名空间，避免双文件迁移成本） |
| 账号 id 前缀 | `"qd-" + sha256[..12]` | `"qdg-" + sha256[..12]`（防跨池同账号 id 碰撞） |
| 端点常量 | `qoder_common.rs` 顶部常量 | `QoderRegionEndpoints::global()` 静态实例（域 × 参数二维） |
| 用量 bucket | 既有 qoder bucket | `UsageBucket::QoderGlobal => "qoder-global"` |
| 调度策略/粘性开关 | `qoder_sticky_enabled` 等 | `qoder_global_sticky_enabled` 等独立键 |
| 签到/快照任务 | `qoder-checkin` / `qoder-credits-snapshot` / 6h refresh | `qoder-global-checkin` / `qoder-global-credits-snapshot` / `qoder-global-refresh` |
| schtasks 任务前缀 | `AIWorkAssistant_QoderCheckin` | `AIWorkAssistant_QoderGlobalCheckin` |
| 池运行锁 | 既有 | `qoder_global_pool_lock` |
| UI 档位 | qoder 档 | **QoderGlobal** 独立档位（API 服务页/健康徽标与 qoder 视觉区分），默认关 |

### 📊 3.5 国内已实现功能 → 国际版（qoder-global）功能对照（菜单级逐项）

> 国内 `qoder` 池功能面为蓝本（CN 方案 §5.8 五页 + 全局集成点），逐页面、逐按钮、逐弹框、逐命令映射国际版。**复用方式四类标注**：
>
> - **①内核复用**：行为与代码近零改动，仅端点/常量按区域切换；
> - **②机制复用+参数分区**：同一命令/组件跑两遍，参数、键名、存储按池分键；
> - **③新实现**：Intl 特有能力，CN 无对应（或 CN 对应不可用）；
> - **④裁剪/不做**：CN 有但 Intl 无对应业务、或不适用。

#### 3.5.0 对齐总则

- **对齐口径**：「功能菜单尽量与国内 Qoder 一致」落在六层——① 页面（五页 + Dashboard 区块 + HelpModal）；② 页面内卡片/区块；③ 按钮（页头/行内/弹框内）；④ 弹框；⑤ Tauri 命令（33 个）；⑥ 配置键（Settings 键名镜像）。以下各节按此口径逐项标注复用方式；**国际版不新增与 CN 不对齐的独立菜单**（除 CLI 解密导入与 CN 停运提示两个 Intl 特有项）。
- **默认态**：Intl 分区所有「待探测」功能（登录/签到/目录）在 Gate 未闭合前置灰 + 提示文案，**UI 先行分区、能力随 Gate 逐个点亮**（与 Buddy 国际版同款节奏）。

**能力边界总表（国内功能 → 国际版处置）**：

> 对齐 BuddyAI 计划总表逻辑：六类处置标注——**直接可做**（组件/数据源现成）/ **等效实现**（机制同构，端点/参数/目录按区切换）/ **Intl 特有新增**（CN 无对应，合规凭证优势）/ **降级等效**（端形态差异下降级对齐，不做假数据）/ **国际上游不适用**（明确不做 + 依据）/ **后续项**（顺延可选）。逐按钮/弹框/命令明细见 §3.5.1-3.5.10；阶段落位见 §6。

| 国内功能 | 国际版处置 | 依据 |
|---|---|---|
| 打开 Qoder 客户端（IDE / Work 双端） | **等效实现（双端同构）**：Intl 实测同为双端（Work 端 `Qoder.exe` + IDE `Qoder IDE.exe`，v1.3），`qoder_open_ide`/`qoder_open_work` 双命令区域参数化直接复用 | §2.3 v1.3 实测（D-6 关闭） |
| OAuth 设备流登录 | **等效实现**：client_id / 授权页 / poll 域换 Intl 值，命令同构 | 归档 §2.8（R-G2 复核） |
| CLI 本地登录态导入 | **Intl 特有新增**：`~/.qoder/.auth/machine_id` 已实测存在（uuid4 明文）；归档 §2.4 所述 user 解密文件（AES-128-CBC）现版目录未见，通道设计保留、挂 R-G9 登录复测——CN 无此通道，Intl 合规凭证优势 | 归档 §2.4 + v1.3 实测 |
| 扫描本地账号（IDE/Work 数据目录） | **等效实现**：双目录扫描——Work 端实测 `com.qoder.app.stable` + IDE 实测 `%APPDATA%\Qoder` | v1.3 实测 |
| PAT 导入 / jobToken 交换 | **等效实现**：端点换 center.qoder.sh + appcode/signature 头 + `?Encode=1` 自定义 base64 | 归档 §2.7（R-G6） |
| 账号编辑 / 移除 / 分组 / 标签 / 批量 | **直接可做**：命令域同构，表按池分区 | §3.5.2 / §3.5.9 |
| 导出 / 导入账号（AIWQENC1 加密） | **直接可做**：kind=aiwork-qoder-global-pool + 区域必选校验 + device_profile 绝不覆盖 | 安全红线继承 |
| 凭证续期（6h ensure_fresh） | **等效实现**：`jrt-` 前缀分派 + token_key 两分支字段名 | 归档 §2.5（R-G7） |
| 每日签到（0 点 + 10 点双活动） | **等效实现（单活动化）**：Intl 单活动「每日领取 100」，10:15 单窗口调度；KNOWN_DAILY_CAMPAIGNS 盲发链同步裁剪 | 归档 §3.2 + 附录 A（R-G4） |
| 一键签到表 / NDJSON 进度 / 90 天日历 | **直接可做**：组件复用，读 `qoder_global_checkin_results` | 表结构镜像 |
| 积分余额 / 快照 / 趋势 / 到期日历 | **等效实现**：quota 端点 Intl 化；CLI `/usage` 解析兜底通道裁剪（双通道足够） | 归档 §2.5（R-G5） |
| 积分榜 / 统计卡（KPI） | **直接可做**：同构聚合，读 `qoder_global_*` | §3.5.1 |
| Token 统计 / 会话明细 | **等效实现**：`qoder-global` 记账桶 + 会话域随 R-G3 复核 | 记账分区（§3.6.3-9） |
| 网关对话（qoder 档） | **等效实现**：`qoder-global` 独立池档位（默认关），上游 api3.qoder.sh | R-G3 硬前置 |
| API Key / 池健康徽标 / ResourceSummary / UsageStatsPanel | **直接可做**：组件扩档（amber 徽标 / 五池徽标 / 预设行 / 六桶） | §3.6.2 / §3.5.8 |
| 模型目录 / 倍率 | **等效实现**：目录源 Intl 化；倍率首版静态配置；Qwen3.8-Flash 0 系数标注（前端 override 手法） | R-G8 |
| 切换登录态 / 快照管理（Work + IDE 双目标） | **等效实现（双目标保留）**：Intl 实测双端并存，快照双目标 `profiles_qoder_global_work` / `profiles_qoder_global_ide`，90s 看门狗逻辑原样复用 | §2.3 v1.3 实测（D-6 关闭） |
| 环境重置（8 项清单） | **等效实现**：清单按 Intl 三目录族重排（Work `com.qoder.app.stable` + IDE `%APPDATA%\Qoder` + CLI `~/.qoder/.auth/`），项数实测后定 | R-G9（v1.3 已探明三目录族） |
| CLI 桥状态（`qoder_cli_status`） | **等效实现**：探测目录换 `~/.qoder/.auth/` | §3.5.10-3 |
| 调度策略中心（`qoder_strategy`） | **等效实现**：Intl 无独立策略键（跟随统一策略），仅档位可选 | §3.6.1 |
| 会话压缩（`qoder_sessions_compact_*`） | **直接可做**：任务镜像 `qoder-global-sessions-compact` | §3.5.4 |
| 14 天 Pro 试用 300 Credits 自动化 | **国际上游不适用（红线）**：多建试用账户直接冻结（虚拟机亦不支持），明确不做 | 官方明文（附录 A） |
| 注册 / 验证码绕过 / 批量开户 | **国际上游不适用（红线）**：仅面向存量个人账号 | 官方明文 |
| CN / Intl Credits 互通换算 | **国际上游不适用**：账号体系独立、定价不等价 | 附录 A |
| macOS 端实施 | **后续项**：跟随 CN 池 macOS 排期（CN 方案 M5） | §5.9 平台兼容 |

#### 3.5.1 Overview（概览页）

CN 现状（[QoderOverview.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/qoder) 496 行）：PageHeader + 5 张 StatCard + 近 30 天签到堆叠柱图 + 积分榜 Top10 + 配置导航 5 步。

| 菜单项/卡片 | CN 实现 | Intl 对照 | 复用方式 |
|---|---|---|---|
| StatCard·账号总数 | `qoder_accounts_list` 计数 | 同构，读 `qoder_global_*` 表 | ② |
| StatCard·可用总积分 | `qoder_credits_fetch` 汇总（600s 缓存） | 同构，Intl quota 端点（R-G5） | ① |
| StatCard·登录账号 | 含 CLI 状态桥（`qoder_cli_status`） | 同构；CLI 目录探测换 `~/.qoder/.auth/`（Intl 特有，Gate R-G6） | ②+③ |
| StatCard·本机套餐 | quota 响应 plan 字段 | 同构，字段名 R-G5 对齐 | ① |
| StatCard·告警提醒 | 积分低/凭证临期告警 | 同构阈值复用 | ① |
| 近 30 天签到柱图 | `qoder_checkin_results` 90 天数据聚合 | 同构，读 `qoder_global_checkin_results` | ② |
| 积分榜 Top10 | 账号积分降序 | 同构 | ② |
| 配置导航 5 步 | 含 `qoder_open_ide` / `qoder_open_work` 打开客户端 | 同构；Intl 实测同为双端（v1.3），双命令区域参数化：IDE 开 `Qoder IDE.exe`、Work 开 `Qoder.exe`（路径键 §3.5.4） | ② |
| Intl 特有卡 | — | **「CN 停运倒计时 / 国际版延续」提示位**（2027-09-05 倒计时，引导迁移） | ③ |

#### 3.5.2 Accounts（账号页）——功能最重页，三分表对照

CN 现状（[QoderAccounts.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/qoder) 1818 行）：页头 11 按钮 + 9 列账号表 + 行内 5 操作 + 9 个弹框。

**A. 页头按钮（11 个）**：

| 按钮 | CN 行为（命令） | Intl 对照 | 复用方式 |
|---|---|---|---|
| 帮助 | 打开 HelpModal | 同构，内容 Intl 分区（§3.5.7） | ② |
| 刷新 | 重拉账号列表 + 积分 | 同构 | ② |
| OAuth 登录 | `qoder_oauth_login`（设备流，含 compat 参数）+ `qoder_oauth_cancel` | 同构；client_id/授权页/poll 换 Intl（R-G2），Gate 未过置灰 | ① |
| 扫描本地账号 | `qoder_ide_scan`（`%APPDATA%\QoderCN\`） | 同构；双目录扫描——Work 端 `com.qoder.app.stable`（v1.3 实测）+ IDE `%APPDATA%\Qoder`（R-G9） | ② |
| 导入 PAT | `qoder_account_import_pat`（pt-/jt- 前缀预检） | 同构；前缀体系 Intl 同构（pt-/jt-/dt-/jrt-，归档 §2.5），换取端点换 center.qoder.sh（R-G6） | ① |
| 导出账号 | `qoder_accounts_export`（AIWQENC1 AES-256-GCM） | 同构；`kind=aiwork-qoder-global-pool` | ② |
| 导入账号 | `qoder_accounts_import`（区域必选校验） | 同构；**device_profile 仅本地为空才补入，绝不覆盖**（红线继承） | ② |
| 环境重置 | `qoder_env_reset_items` + `qoder_env_reset`（8 项清单） | 同构；重置项清单按 Intl 三目录族（Work `com.qoder.app.stable` + IDE `%APPDATA%\Qoder` + CLI `~/.qoder/.auth/`）重排（R-G9 实测后定清单） | ② |
| 分组管理 | `qoder_groups_list/create/update/remove` | 同构，组表按池分区 | ② |
| 标签管理 | 同上（shared_groups 转发） | 同构 | ② |
| 快照管理 | 双目标 profiles_qoder / profiles_qoder_work（120s 看门狗） | **Intl 同为双端（v1.3 实测）**：双目标 `profiles_qoder_global_work` / `profiles_qoder_global_ide`，看门狗逻辑原样复用 | ② |

**B. 账号表（9 列）**：列定义照抄 CN（含凭证来源徽标 PAT/OAuth/IDE、分组、标签、积分、指纹列、状态等），Intl 分区新增**区域徽标**（Global 紫）与 **CLI 桥状态列**（Intl 特有通道）。

**C. 行内操作（5 个）**：

| 行内按钮 | CN 行为 | Intl 对照 | 复用方式 |
|---|---|---|---|
| 切换 | appMenu 双目标「Qoder Work」/「Qoder IDE」+ 90s 看门狗 | **Intl 同构双目标**（v1.3 实测双端并存）：菜单项「Qoder (Global)」/「Qoder IDE (Global)」，90s 看门狗逻辑原样复用 | ② |
| 保存登录态 | 写入本地指纹覆写三文件 | 同构；覆写目标换三处实测路径（Work `com.qoder.app.stable\auth.machine-id` + IDE `Qoder\SharedClientCache\cache\id` + CLI `~/.qoder/.auth/machine_id`，§4.4） | ② |
| 凭证续期 | `qoder_account_refresh_token`（6h ensure_fresh） | 同构；前缀分派 `jrt-`→jobToken/refresh（R-G7） | ① |
| 编辑 | `qoder_account_save` | 同构 | ② |
| 移除 | `qoder_account_remove` | 同构 | ② |

**D. 弹框（9 个）**：OAuth 进度（含 compat）、快照管理（Intl 双目标 Work/IDE）、PAT 导入、指纹查看、编辑、导出、导入、环境重置（8 项清单）、Groups/Tags——全部同构复用；差异仅在三处：OAuth 进度弹框的 poll 域/授权页 URL 换 Intl、PAT 弹框预检文案补 Intl 前缀说明、环境重置弹框清单按 Intl 三目录族重排。另新增第 10 个 Intl 特有弹框：**CLI 登录态解密导入**（读 `~/.qoder/.auth/machine_id` + `user`，AES-128-CBC key=IV=machine_id[:16]，归档 §2.4；**v1.3 实测：machine_id 文件在、user 文件未见，通道挂 R-G9 登录复测后接线**）。

#### 3.5.3 Checkin（签到页）

CN 现状（[QoderCheckin.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/qoder) 312 行）：双活动说明卡 + 一键签到表 + NDJSON 实时进度 + 90 天日历。

| 菜单项 | CN 实现 | Intl 对照 | 复用方式 |
|---|---|---|---|
| 活动说明卡·一 | 100 Credits / 0 点签到 | **无对应**（Intl 无 0 点签到），卡片位改放「每日领取 100」总说明 | ④ |
| 活动说明卡·二 | 10:00 开窗 Add-on 仅个人版 | **单活动说明卡**：10:00→次日 10:00 窗口（UTC+8）、桌面端渠道、30 天有效、错过不补（附录 A） | ④（内容重写，卡片骨架复用） |
| 一键签到表 | `qoder_checkin_start` 全账号逐个 claim | 同构；campaigns → CLAIMABLE 过滤 → claim 幂等（`replayed==true` 视为 already）→ **claim 失败复查恢复**（CN 已落地，直接继承） | ① |
| NDJSON 实时进度 | `qoder_checkin_results` 流式 | 同构，写 `qoder_global_checkin_results` | ② |
| 90 天日历 | pk=date\|user_id\|time_ms | 同构 | ② |
| 调度入口 | `qoder_checkin_task_register/status/unregister`（10:15 + gap 0-600s + 启动补签） | 同构；键名 `qoder_global_checkin_*` 镜像（§3.5.4） | ② |

#### 3.5.4 Settings（设置页）——配置键镜像表

CN 现状（[QoderSettings.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/qoder) 586 行）+ [common.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L154-L159) `qoder_settings_get/set`。Intl 逐键镜像：

| CN 配置键 | CN 默认值 | Intl 镜像键 | 复用方式 |
|---|---|---|---|
| `qoder_ide_path` | 空（IDE 应用路径） | `qoder_global_ide_path`（默认探测 `%LOCALAPPDATA%\Programs\Qoder IDE\Qoder IDE.exe`） | ② |
| `qoderwork_path` | 空（Work 应用路径） | `qoder_global_work_path`（默认探测 `%LOCALAPPDATA%\Programs\Qoder\Qoder.exe`；**v1.3 双端实测恢复**，D-6 关闭） | ② |
| `auto_checkin` | — | `qoder_global_auto_checkin` | ② |
| `qoder_token_renew_enabled` | 开（6h） | `qoder_global_token_renew_enabled`（同 6h） | ② |
| `qoder_checkin_hhmm` | "10:15" | `qoder_global_checkin_hhmm`（同 10:15，窗口 10:00 开） | ② |
| `qoder_checkin_gap_secs` | 3（0-600） | `qoder_global_checkin_gap_secs`（同默认） | ② |
| schtasks 前缀 | `AIWorkAssistant_QoderCheckin` | `AIWorkAssistant_QoderGlobalCheckin` | ② |
| 积分同步四模式 | every_hours(2) / daily(23:40) / hourly / off | `qoder_global_credits_sync_mode` 等同构四模式 | ② |
| `qoder_sessions_compact_enabled` | 开（days 3 / 04:30） | `qoder_global_sessions_compact_*` 镜像 | ② |
| 合规提示区 | CN 风控文案 | 文案重写：**「多建试用账户将冻结」红线 + 仅本机自用声明** | ④（文案） |

#### 3.5.5 ApiService（API 服务页）

QoderApiService 页（865 行）无独立网关管理（复用全局 ApiManagerModal），其**逐区块对照与全局 API 管理集成点全部移至 §3.6 专章**（本轮新增），此处不重复。

#### 3.5.6 Dashboard 积分看板（全局集成）

CN 现状：Dashboard 的 qoder 区块含 **4 TABs（积分统计 / Token 统计 / 积分到期 / 会话明细）** + 四处禁用文案（qoderGatewayReady 守卫控制）。

| TAB | CN 数据源 | Intl 对照 | 复用方式 |
|---|---|---|---|
| 积分统计 | `qoder_credits_fetch` / history | 同构，Intl 端点 | ① |
| Token 统计 | usage bucket（qoder 桶） | 同构，**新增 qoder-global 桶**（R-G5 fold-in：数据源同积分端点） | ② |
| 积分到期 | 逐包 expire_at 日历 | 同构（30 天逐包有效） | ① |
| 会话明细 | `qoder_sessions_query` / detail | 同构，Intl 会话域 R-G3 复核 | ② |
| 禁用文案四处 | `qoderGatewayReady` | 新增 `qoderGlobalGatewayReady` 守卫（Gate 未过同款禁用） | ② |

#### 3.5.7 HelpModal（帮助弹框）

CN 现状：10 节帮助内容。Intl 对照：骨架复用，内容扩展 Intl 分区——套餐体系（Free/Pro/Pro+/Ultra/资源包，附录 A）、每日领取规则（100/天、10:00 窗口、30 天有效、错过不补）、Qwen3.8-Flash 0 系数说明、CN/Intl Credits 不互通声明、**试用冻结红线**、CLI 解密导入教程（Intl 特有）。复用方式 ②。

#### 3.5.8 共用组件复用矩阵

| 组件 | 现状 | Intl 改动 |
|---|---|---|
| PageHeader / StatCard / 步骤条 | Overview 骨架 | 直接复用 |
| PoolHealthBadges | 四池徽标 | 扩五池（+qoder-global 紫徽标） |
| ResourceSummary 预设 | 5 预设（含 qoder 策略，`qoder_strategy` 空串=跟随 Trae） | 新增 qoder-global 预设行（默认关） |
| UsageStatsPanel | 五桶（trae_days/wb_days/qoder_days/custom_days/buddy_ai_days） | 扩六桶（+qoder_global_days） |
| ApiManagerModal | 全局网关管理 | 档位下拉 +QoderGlobal |
| POOL_KEYS 默认序 | buddy > trae > qoder > buddy-ai | 追加 qoder-global（位次 D-x 评审：建议 qoder 之后） |
| API Key 徽标 | bind_pool='qoder' violet | bind_pool='qoder-global' 新配色（amber） |
| NDJSON 进度流 / 90 天日历 / 指纹查看弹框 | Checkin/Accounts | 直接复用 |

#### 3.5.9 命令域总览（33 个 Tauri 命令逐一对照）

> 全部命令位于 [commands/qoder/](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder)，Intl 侧**不新增平行命令文件**——统一在命令入口按 `region` 参数分流（防命令面翻倍）。标注：①=内核复用（仅端点切换）/ ②=参数分区（读 `qoder_global_*` 表与键）/ ③=新实现 / ④=裁剪。

| 命令 | 位置 | Intl 复用方式 |
|---|---|---|
| `qoder_accounts_list` | [accounts.rs:100](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/accounts.rs#L100) | ② |
| `qoder_account_save` | [accounts.rs:125](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/accounts.rs#L125) | ② |
| `qoder_account_remove` | [accounts.rs:186](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/accounts.rs#L186) | ② |
| `qoder_account_import_pat` | [accounts.rs:244](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/accounts.rs#L244) | ①（端点换 center.qoder.sh，R-G6） |
| `qoder_account_refresh_token` | [accounts.rs:382](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/accounts.rs#L382) | ①（前缀分派，R-G7） |
| `qoder_accounts_export` | [data_io.rs:175](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/data_io.rs#L175) | ②（kind=aiwork-qoder-global-pool） |
| `qoder_accounts_import` | [data_io.rs:370](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/data_io.rs#L370) | ②（区域必选校验） |
| `qoder_ide_scan` | [ide_store.rs:255](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/ide_store.rs#L255) | ②（双目录：Work `com.qoder.app.stable` + IDE `%APPDATA%\Qoder`，v1.3 实测） |
| `qoder_checkin_start` | [checkin.rs:33](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/checkin.rs#L33) | ①（R-G4） |
| `qoder_checkin_results` | [checkin.rs:101](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/checkin.rs#L101) | ② |
| `qoder_checkin_task_register` | [checkin.rs:182](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/checkin.rs#L182) | ② |
| `qoder_checkin_task_status` | [checkin.rs:225](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/checkin.rs#L225) | ② |
| `qoder_checkin_task_unregister` | [checkin.rs:241](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/checkin.rs#L241) | ② |
| （启动钩子）`startup_auto_checkin` | [checkin.rs:261](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/checkin.rs#L261) | ②（双池补签并行） |
| `qoder_credits_fetch` | [credits.rs:13](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/credits.rs#L13) | ①（R-G5） |
| `qoder_credits_history_list` | [credits.rs:30](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/credits.rs#L30) | ② |
| `qoder_settings_get` | [common.rs:154](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L154) | ② |
| `qoder_settings_set` | [common.rs:159](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L159) | ② |
| `qoder_live_logins` | [common.rs:253](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L253) | ② |
| `qoder_env_check` | [common.rs:536](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L536) | ②（Intl 目录族检查项） |
| `qoder_open_ide` | [common.rs:593](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L593) | ②（开 `Qoder IDE.exe`，v1.3 实测路径） |
| `qoder_open_work` | [common.rs:599](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/common.rs#L599) | ②（开 `Qoder.exe`，v1.3 双端实测恢复，D-6 关闭） |
| `qoder_groups_list/create/update/remove` | [groups.rs:16/28/35/46](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/groups.rs#L16) | ②（组按池分区） |
| `qoder_account_move` | [groups.rs:60](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/groups.rs#L60) | ② |
| `qoder_sessions_query` | [sessions.rs:169](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/sessions.rs#L169) | ② |
| `qoder_session_detail_query` | [sessions.rs:275](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/sessions.rs#L275) | ② |
| `qoder_cli_status` | [cli_status.rs:40](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/cli_status.rs#L40) | ②（`~/.qoder/.auth/` 探测） |
| `qoder_env_reset_items` | [env_reset.rs:181](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/env_reset.rs#L181) | ②（Intl 清单重排） |
| `qoder_env_reset` | [env_reset.rs:224](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/env_reset.rs#L224) | ② |
| `qoder_oauth_login` | [oauth.rs:83](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/oauth.rs#L83) | ①（Intl client_id/授权页/poll，R-G2） |
| `qoder_oauth_cancel` | [oauth.rs:209](file:///d:/code/AIWorkAssistant_main/src-tauri/src/commands/qoder/oauth.rs#L209) | ① |
| **（新增）`qoder_global_cli_import`** | 新文件 | ③（CLI 登录态解密导入，Intl 特有；前置：`~/.qoder/.auth/machine_id` 已实测，`user` 文件待 R-G9 登录复测） |

#### 3.5.10 CN 特有事实重核清单（写 Intl 前必须逐项重核的 10 个坑）

| # | CN 事实 | Intl 重核点 |
|---|---|---|
| 1 | 双活动（0 点 + 10 点） | Intl 单活动——活动说明卡、KNOWN_DAILY_CAMPAIGNS、盲发兜底链全部裁剪为单活动形态（R-G4） |
| 2 | campaigns 缺 `Cosy-ClientType` 头返回 `campaigns: []` **静默空列表** | Intl 更隐蔽（v1.4 开源实测，hdhgsysh 2026-09-27）：缺 **umid 三头**（`Cosy-MachineToken/Code/Type`）只回 VIEW_DETAILS 横幅、签到行 `active:false` 且**不报错**；UI 侧 empty_campaigns 必须三态区分「无活动 / `Cosy-ClientType` 缺失 / umid 缺失」（底稿见 §4.3，R-G4） |
| 3 | CLI 桥读 `~/.qoder-cn` | Intl 换 `~/.qoder/.auth/`（**v1.3 实测：`machine_id` 单文件 uuid4 明文**；归档 §2.4 的 `user` 解密文件现版未见，挂 R-G9 复测） |
| 4 | Qwen3.8-Flash 0 倍率 = **前端 checkbox override**（目录实际 0.08） | Intl 保持同手法（前端 override），官方口径 0 系数以目录字段复核（R-G8） |
| 5 | PAT 前缀预检 pt-/jt- | Intl 前缀族 pt-/jt-/dt-/jrt- 同构（归档 §2.5），预检文案补 jrt-/dt- |
| 6 | env_reset 8 项清单 | 按 Intl 三目录族（Work `com.qoder.app.stable` + IDE `%APPDATA%\Qoder` + CLI `~/.qoder/.auth/`）重排，v1.3 已探明目录族，清单项数随 R-G9 收口定稿 |
| 7 | 切换/快照双目标（Work + IDE） | Intl 同构双目标（**v1.3 双端实测，D-6 关闭**）：`profiles_qoder_global_work` / `_ide` |
| 8 | COSY 参数双域混用风险 | `QoderRegionEndpoints` 按**域 × 参数二维**建模（§1.1 速记），严禁 v1.0 的单维错误 |
| 9 | 调度窗口（0 点签到 + 10:15 双窗口） | Intl 仅 10:15 单窗口（10:00 开窗 + 15min 缓冲） |
| 10 | 积分 source 枚举 plan/addon_checkin | Intl 枚举 `plan | addon_daily`（字段名落码时对齐 R-G5） |

### 🔌 3.6 API 管理详细对照（QoderApiService 页 + 全局集成 + 网关改造）

> 本节为 v1.1 新增专章，对齐「API 管理尽量与国内 Qoder 功能一致」要求。CN 的 API 管理分三层：**QoderApiService 页**（池内参数）→ **全局 API 管理组件**（跨池档位）→ **后端网关**（qoder_route.rs）。Intl 三层逐一对照。

#### 3.6.1 QoderApiService 页面逐区块对照

CN 现状（[QoderApiService.tsx](file:///d:/code/AIWorkAssistant_main/src/pages/qoder) 865 行，无独立网关管理、复用全局 ApiManagerModal）：

| 区块 | CN 实现 | Intl 对照（qoder-global 分区） | 复用方式 |
|---|---|---|---|
| 积分横幅 | 池总积分 + 低量告警 | 同构，Intl quota 端点 | ① |
| 池指标 3 卡 | `qoder_pool_status` 3s 轮询（可选账号/冷却/硬上限） | 同构，新增 `qoder_global_pool_status` 同构接口（独立池视图） | ② |
| 账号池选择 | 全选/清空/保存 + 分组/标签过滤（D5 交集）+ PoolHealthBadges | 同构，读 `qoder_global_*` 表；徽标扩五池 | ② |
| 参数·`qoderEnabled` | 池总开关 | `qoderGlobalEnabled`（**默认关**，显式开启） | ② |
| 参数·`qoderAccountConcurrencyLimit` | 0-32/账号并发 | `qoderGlobalAccountConcurrencyLimit` 同域 | ② |
| 参数·`qoderPoolStickyTtlSecs` | 0-3600 池粘性 | `qoderGlobalPoolStickyTtlSecs` 同域 | ② |
| 参数·`qoderStickyTtlSecs` | 0-86400 账号粘性 | `qoderGlobalStickyTtlSecs` 同域 | ② |
| 参数·`qoderStickyEnabled` | 默认关 | `qoderGlobalStickyEnabled`（同默认关） | ② |
| 参数·`qoderHedgeThresholdMs` | 0-8000 默认 8000 对冲阈值 | `qoderGlobalHedgeThresholdMs` 同域 | ② |
| 模型目录 | 同步按钮 + 每日 05:50 任务 + Qwen3.8-Flash 0 倍率 checkbox + 7 列模型表 + 地区说明卡 | 目录源 Intl 化（R-G8）独立缓存；0 倍率 override 手法保持；**地区说明卡文案反转**：CN 池侧「Global 区不可路由」→「Global 专属模型请启用 qoder-global 档」，Intl 分区显示 Global 模型表 | ①+② |
| 调度策略中心 | `qoder_strategy` 空串=跟随 Trae | Intl 无独立策略键（跟随统一策略），仅档位可选 | ② |

#### 3.6.2 全局 API 管理集成点对照

| 集成点 | CN 现状 | Intl 改动 |
|---|---|---|
| ApiManagerModal（网关管理弹框） | 档位列表含 qoder | +QoderGlobal 档（默认关、紫/amber 徽标区分） |
| ResourceSummary | 5 预设（trae/wb/qoder/buddy-ai/custom 策略行） | +qoder-global 预设行 |
| POOL_KEYS 默认序 | buddy > trae > qoder > buddy-ai | 追加 qoder-global（建议 qoder 之后，评审拍板） |
| API Key 管理 | bind_pool='qoder' violet 徽标 | bind_pool='qoder-global' amber 徽标；Key 创建下拉 +QoderGlobal |
| UsageStatsPanel | 五桶 | 六桶（+qoder_global_days） |
| 帮助页（倍率列） | qoder 档倍率列 | +qoder-global 档倍率列（静态配置 + Qwen3.8-Flash 0 标注） |
| /healthz | 按池上报 selectable | +qoder-global 池独立 selectable |

#### 3.6.3 后端网关改造点明细（10 项，含代码锚点）

| # | 改造点 | 锚点 | 说明 |
|---|---|---|---|
| 1 | dispatch `priority` 档位支持 `"qoder-global"` | api_server dispatch 层 | 与 `"qoder"` 平级，默认不启用 |
| 2 | `entry_is_global_only` 守卫按池放行 | [qoder_route.rs:351](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L351) | CN 池维持 fail-closed 404；Global 池内 Global 专属模型合法路由 |
| 3 | **`apply_flash_free_override` 必须在守卫之后应用**（顺序红线） | [qoder_route.rs:701](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L701) / [:1339](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L1339) | 两处调用点均保持「先守卫、后 override」，防 0 倍率模型绕过守卫路由到 CN 池 |
| 4 | 区域执行分流（两处区域边界注释改造） | [qoder_route.rs:659](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L659) / [:1312](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L1312) | CN 池 → `QoderRegion::Cn`（逐字不变）；Global 池 → `QoderRegion::Global`（api3.qoder.sh，R-G3） |
| 5 | 池约束扩双池 | [qoder_route.rs:713](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L713) / [:1351](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L1351) | `.pool_constraints("qoder")` 扩为按请求池视图绑定（"qoder" 或 "qoder-global"） |
| 6 | COSY Global 分支（域 × 参数二维） | [qoder_sign.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_sign.rs#L41-L50) + [qoder_common.rs](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_common.rs#L30-L50) | CN 分支零改动；Global 分支按归档 §2.1/2.2（RSA 公钥加密 temp_key、AES-128-CBC、五段 md5、cosy-clientip 硬编码）；**红线：先 encode_body 再签名** |
| 7 | 粘性键独立前缀 | 取号/粘性模块 | `qoder_global_sticky_enabled` + 粘性绑定键前缀 `qg:` |
| 8 | 冷却独立实例 | [qoder_route.rs:71](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L71) / [:93](file:///d:/code/AIWorkAssistant_main/src-tauri/src/api_server/qoder_route.rs#L93) | 模型级冷却表 Global 池独立实例（键仍为裸 model 名，池边界即隔离；`PoolStatus.cooling` / `hard_credit` 语义不变） |
| 9 | 记账桶扩展 | usage 模块 | `UsageBucket::QoderGlobal => "qoder-global"`；record_usage 按池路由 |
| 10 | 对话管线护栏全量继承 | build_upstream_body / resolve_thinking 等 | record_id_for / session_id_for sha256 派生、business.product="cli"、resolve_thinking（Qwen3.8 发 false 断连）、QUEUE_CODES=["10605"] 排队不熔断、AUTH_CODES=["105"]、QUOTA_CODES=["110"~"122"]、err_slot 不发 [DONE]、首字超时 10s、请求 jitter/退避/sent_any 收尾——**Global 池一律继承不裁剪**；错误码族 Intl 版以 R-G3 抓包复核；**错误分诊三护栏（v1.4 开源互证）**：① 先认 quota 码再认队列标记——110=日计费次数耗尽（日界重置，网关发小时级 Retry-After 重试无益），10605=排队/限流 transient 可重试，官方错误码文档 `docs.qoder.com/cli/sdk/errors` 已公开全表（110/113/114/115/116/117/118/119/122 quota；406/416/430/47902/48716/80411/80412 request；500/10408/10500/10605 runtime；105 auth）；② 队列 4 标记（`queueType`/`retryAfterSeconds`/`isQueued`/`serviceAvailable`）须 **≥2 共现**才归 transient，`retryAfterSeconds` 可能嵌套 detail.message 多层 JSON 字符串（递归解析最深 4 层）；③ **401/403 带队列标记 → RATE_LIMIT 而非登录失效**（防误触发重认证重试）；模型参数护栏：`enable_thinking:false` 对 GLM-5.3-Flash/Kimi-K3 被 `provider_error 1210` 拒绝，应**整字段省略**；上游 200+陌生信封按 `ProtocolShapeChangedError` 处理（retryable=false，报更新提示不重试） |

---

## 🔧 4. 关键设计点

### 4.1 账号与凭证

- **端点单源化（G-1）**：`qoder_common.rs` 顶部 6 常量提升为 `QoderRegionEndpoints` 结构体（**按域 × 参数二维**：openapi 域参数族 / gateway 域参数族 / 通用字段，字段含 open_api / web_base / chat_host / catalog_url / client_type / client_ua / cosy_version / machine_os / machine_type / auth_page / poll_url / refresh_url / pat_page / region），CN/Global 两份静态实例；CN 实例值与现状逐字一致（回归保障）。
- **登录（G-2）**：设备流区域参数化——client_id（Intl `e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb`，R-G2 复核现版有效性）、授权页（`qoder.com/device/selectAccounts`，PKCE S256 + nonce + machine_id，归档 §2.8）、poll（`openapi.qoder.sh`，GET 404=等待/200=就绪，1s 间隔 300s 超时）。探测结论未出前 Intl 登录按钮置灰 + 提示「待上游验证」，仅开放手工导入。
- **导入通道（Intl 特有增强）**：新增 CLI 登录态解密导入——读 `~/.qoder/.auth/machine_id`（**v1.3 实测：uuid4 明文单文件**，归档 §2.4 所述 `id` 文件名与实测不符）+ `.auth/user`（AES-128-CBC，key=IV=machine_id 前 16 ASCII 字符，身份 JSON 兼容 camel/snake 双命名，归档 §2.4；**现版目录未见此文件，通道保留设计、挂 R-G9 登录复测**）；这是 Intl 优于 CN 的合规凭证通道（用户本机自己的登录态）。
- **刷新**：`ensure_fresh` 按 `refresh_token` 前缀分派（Intl：`jrt-`→jobToken/refresh、其余→deviceToken/refresh，token_key 字段名两分支不同，归档 §2.5）；6h 兜底刷新任务镜像复制为 `qoder-global-refresh`。
- **刷新失败语义**：R-G2/G7 通过前，Intl 账号刷新失败按「暂态网络」处理**不标记 needs_relogin**（避免误杀真实有效凭证），落 app_log 留痕（对齐 Buddy 方案同款保护）。
- **PAT（G-2）**：PAT 创建入口**已收口**——`qoder.com/account/integrations`（v1.4 开源实证：hdhgsysh 与 masknull README 均指向此直达路径）；换取端点**双通道待 R-G6 定稿**（归档 §2.7 center 域底稿 vs masknull openapi 域 `/api/v1/jobToken/exchange`，见 §3.3）；`?Encode=1` 自定义 base64（标准 base64 → 三段重排 → 65 字符表 `_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!`，pad=`$`）纯算法可直接翻译（归档 §6.1 移植要点已列）。

### 4.2 网关转发与调度（API 管理接入）

> 逐项改造明细（含锚点）见 **§3.6.3 十项清单**，本节仅存原则。

- **取号**：qoder 池取号函数拆双池视图（CN 读 `qoder_*` 表，Intl 读 `qoder_global_*` 表）；dispatch `priority` 数组支持 `"qoder-global"` 档位（与 `"qoder"` 平级，**默认不启用**）。
- **签名（G-4 前置）**：`build_cosy_headers` 区域 × 域二维参数化——CN 分支零改动；Global 分支按归档 §2.1/2.2 底稿实现，全部参数以 R-G3 现版抓包复核为准（客户端版本演进可能已改 cosyVersion/公钥）。
- **粘性/冷却/记账**：按 §3.4 隔离清单执行（`qg:` 前缀、独立冷却实例、独立 bucket）。
- **护栏全量继承**：请求 jitter、失败退避、`sent_any` 空完成收尾、首字超时故障转移等 qoder 池既有护栏在 Global 池一律继承（§3.6.3 第 10 项，同 Buddy 方案 §4.2 原则）。

### 4.3 签到 / 积分（每日领取 100 Credits 自动化）

- **活动认知**：Intl 单活动「每日领取 100 通用 Credits」（官方每日活动，2026-09-18 起，结束另行公告）；**无 CN 的 0 点签到**；领取渠道官方口径「仅限 Qoder 桌面端」，接口层为 sash campaigns 体系（归档 §3.2：Intl 自 v0.8.18 统一 campaigns，旧 daily-check-in 端点已全球禁用）。**umid 机器身份门控（v1.4 开源实测底稿，hdhgsysh 2026-09-27）**：`GET /sash/api/v1/me/campaigns` 须携带 `Cosy-MachineToken/Code/Type` 三头（桌面端 `resources\umid\runtime-info.exe` 生成），缺失时只回 VIEW_DETAILS 横幅、签到行 `active:false` 且**不报错**——比 CN 缺 `Cosy-ClientType` 静默空列表更隐蔽；活动窗口 = `startAt/beginAt/endAt` 双字段兼容取值；每日 10:00（UTC+8）发新轮 = 新 campaignId + 固定 100 Credits。
- **实现口径**：完全复用 `qoder_checkin.rs` 骨架——GET campaigns（带 openapi 域 COSY 头 + **umid 三头**，R-G4 确认取值）→ **双过滤器** `actionType === 'CLAIM_BENEFIT'` + `claimStatus`（顶层 `claimable` 不可信——已领/无活动都回 false；VIEW_DETAILS 横幅带 `claimStatus:"CLAIMED"` 却无 benefit 的过滤坑）→ 逐个 POST claim（幂等，`replayed==true` 视为 already；**每次 claim 前重读 campaigns 取 id 不缓存**，开源互证 v1.4：masknull checkin.ts；benefit 字段链 = `benefit.amount`（积分数）/ `benefit.validity.days`（有效期）/ `benefit.kind`）→ **claim 失败复查恢复**（cn 归档 §3.2 方案，CN 池已落地，直接继承）→ 结果写 `qoder_global_checkin_results`（90 天滚动）→ NDJSON 进度流。
- **计入科目**：领取后进 **Add-on Credits**（官方文档明示），与购买资源包同科目——积分引擎 `packages[].source` 枚举 Intl 侧为 `plan | addon_daily`（映射现有 addon_checkin 语义，字段名落码时对齐）。
- **调度**：窗口每日 10:00 开（UTC+8，新加坡时间）→ **10:15 单次调度**即可覆盖（与 CN 双活动同结论）；错过不补领 → 自动化直接提高领取率，这是本项核心用户价值；间隔键 `qoder_global_checkin_hhmm`，启动补签镜像。
- **积分引擎**：`qoder_credits.rs` 复用（三通道、600s 缓存 + stale-on-error、快照 365 天）；Intl 无 CLI `/usage` 解析兜底通道（Intl CLI 有独立凭证但无解析需求，R-G5 若 quota 接口可用即双通道足够）；FEFO / 同到期先 Plan 后 Add-on / 失败不扣费——官方规则与现有实现同构，解析字段以 R-G5 实测响应为准。
- **不做**：14 天 Pro 试用 300 Credits 的自动化（一次性、新用户首登触发、多建冻结——触碰红线，明确不做）；「每日领取」若上游对非桌面端渠道风控拦截（R-G4 失败），降级方案 = CDP/UI 自动化评估（低优先，参照 CN 方案 M4 CDP 兜底先例，先评估后立项）。

### 4.4 多账号并发与设备指纹（区域分派）

- **每账号稳定绑定原则不变**（CN 方案 §5.10 模型）：账号导入时生成一次、之后恒定复用；`machine_token`/`machine_type` 可现场随机（QoderGateway 实证服务端无强绑定，R-G9 现版复核）。
- **格式按区分派**：CN machine_id = 32 位小写 hex；**Intl machine_id = uuid4 标准 36 字符**（带连字符；归档 §2.3 底稿 + **v1.3 三端实测互证**：CLI `~/.qoder/.auth/machine_id`、Work 端 `com.qoder.app.stable\auth.machine-id`、IDE 端 `%APPDATA%\Qoder\SharedClientCache\cache\id` 均为 uuid4 形态）、machine_token = base64url 68 字符、machine_type = 18 位 hex。指纹生成器抽 `QoderDeviceProfile` 区域变体，`data_io` 导入红线沿用：**device_profile 仅本地为空才补入，绝不覆盖**。
- **本地覆写目标（Intl，v1.3 三处实测路径）**：Work 端 `%APPDATA%\com.qoder.app.stable\auth.machine-id`（uuid4）；IDE 端 `%APPDATA%\Qoder\SharedClientCache\cache\id`；CLI 侧 `~/.qoder/.auth/machine_id` 可直写（`user` 文件解密 key=machine_id[:16] 必须与 machine_id 一致，R-G9 复测）。多端覆写须保持同账号一致，防跨端指纹漂移。
- **CLI machine_id 读取链（v1.4 开源互证参照，masknull machine-id.ts）**：官方 CLI `~/.qoder/.auth/machine_id` 优先读取 → 插件自有 seed 兜底生成；生成采用 uuid4 + `wx` 标志位写入防创建竞争（写失败则重读既有文件）；该读取链与 §4.1 CLI 导入通道同源，落码时统一为一个 util。

### 4.5 模型目录与倍率

- 目录：`qoder_catalog.rs` 拉取区域参数化，Global 目录独立缓存（`CatalogState` 已有 global/cn 双槽，见 [qoder_upstream.rs:409-427](file:///d:/code/AIWorkAssistant_main/src-tauri/src/tasks/qoder_upstream.rs#L409-L427)）；Intl 模型与 CN 完全不同源（国际模型池），同名模型跨区**不做混合轮询**（对齐 buddy-ai 同名模型跨区不混轮决策）。
- **v1.3 实测阵容**（国际版客户端模型列表，2026-10-22）：**Auto（智能路由）/ Lite / Cantus / DeepSeek-V4-Pro / DeepSeek-V4-Flash / GLM-5.3 / GLM-5.3-Flash / Kimi-K2.7-Code / Kimi-K3 / MiniMax-M3 / Qwen3.7-Plus / Qwen3.7-Max / Qwen3.8-Flash / Qwen3.8-Max** 共 14 项；档位分 **Ultimate / Performance / Efficient** 三档。与 CN 阵容完全不同源；客户端 catalog-v6 按 user_id 缓存（§2.3 版本锚点），落码时以 R-G8 目录接口实测为准。
- 倍率：首版静态配置（官网实际值人工维护）；**Qwen3.8-Flash 标注 0 系数免费**（2026-10-01 起继续免费，官方活动页为准）——0 成本高频低价路由是 Global 池网关侧的核心卖点；0 倍率沿用前端 checkbox override 手法（CN 先例）。**price_factor 折后价语义护栏（v1.4 开源互证，hdhgsysh offpeak.js）**：目录 `price_factor` 是**折后价**、`before_promotion_price_factor` 才是窗口外原价（`before × discount = price_factor`）——直接抄 price_factor 会把工作时段成本低估数倍；错峰窗口 22:00-08:00 跨午夜用析取判定（`h >= 22 || h < 8`），时区计算用 `Intl.DateTimeFormat`（防 DST 漂移），读取前守卫 `promotion.active !== true` 则一律取原价。
- 「API 使用帮助取最小倍率」口径扩展到 `qoder-global` 档。

### 4.6 前端

| 页面 | 改动 |
|---|---|
| QoderAccounts / Overview / Checkin / Settings | 顶部区域 Tab（国内 / 国际）分区；账号卡加区域徽标；Intl 登录按钮状态随 R-G2；Checkin 页 Intl 区展示单活动说明 |
| QoderApiService | `qoder-global` 档位（默认关）；模型目录 Intl 化 + 0 系数标注；地区说明卡文案反转（§3.6.1） |
| Dashboard qoder 区 | 4 TABs 复用 + `qoderGlobalGatewayReady` 守卫（§3.5.6） |
| PoolHealthBadges / 池健康 | 扩为含 `qoder-global` |
| 帮助页（倍率） | 加 `qoder-global` 档倍率列 |

### 4.7 存量与兼容

- 现网 `qoder` 池**无 Global 账号**（CN-only 决策自始未混入）→ **无存量迁移负担**（优于 buddy-ai，后者需迁移混居账号）；仅需导入侧双保险：区域必选参数 + 凭证域特征校验，防 Intl 账号误入 CN 池。
- 回滚设计：`qoder-global` 档默认关，dispatch 不启用即零影响；schema v3→v4 迁移带版本号置位 + 备份，回滚 = 还原备份 + 版本号回退；CN 侧唯一改动是端点常量结构体化（行为逐字不变的等价重构），独立 commit 便于单独 revert。

---

## 🧪 5. Gate 探测清单（先侦察后施工）

> 原则：所有 R-G 项未通过前，对应能力**只做骨架不做接线**。**阻塞层（R-G1~G5）用一次真实国际账号登录收口**（设备流登录 → 抓 campaigns → 抓 quota → 抓 chat MITM，同 Buddy 方案「一次登录收口」手法）；决策层（D-x）评审拍板；观察层随放量观察。产物落 `docs/completed/f80-qoder-global-recon/`（HAR + 截图 + 结论 md），回填 §3.3 端点矩阵。
>
> **Fold-in 注记**（v1.1）：部分 Gate 项同时喂养 §3.5/§3.6 的对照表——R-G5 同时定 Dashboard Token 统计数据源（§3.5.6）；R-G9 同时定快照/切换双目标形态（§3.5.2，D-6）、env_reset 清单（§3.5.10-6）与指纹覆写文件清单（§4.4）；R-G4 同时定活动面裁剪形态（§3.5.10-1/9）；R-G3 同时定错误码族 Intl 版（§3.6.3-10）。

| # | 探测项 | 手段与底稿 | 产出物 → 影响的设计决策 |
|---|---|---|---|
| R-G1 | **账号-区域关系**（未决项①）：同一邮箱可否同时存在 CN 与 Intl 账号；Intl 账号登录 CN 客户端行为 | 真实账号双端实测 + 官方 FAQ | 🔶 **部分收口（v1.3）**：双区并存目录实证（CLI `~/.qoder` vs `~/.qoder-cn`、Work `com.qoder.app.stable` vs `com.qodercn.app.stable`、IDE `%APPDATA%\Qoder` vs `%APPDATA%\QoderCN`）；账号级同邮箱并存与互斥行为待真实账号登录验证 → 导入校验规则 / UI 双区并存提示 |
| R-G2 | **设备流全链路**：client_id `e883ade2-...` 现版有效性、授权页参数、poll 时序、token 响应字段（token / refresh_token / user_id / expires_at / refresh_token_expires_at） | 归档 §2.8 底稿 + 真实登录一次 | 端点常量定稿 → §4.1 登录实现 |
| R-G3 | **chat 网关请求形态**（未决项②落地验证）：`api3.qoder.sh` 端点路径/方法/请求体/流式帧/Encode 位；COSY 头家族现版取值（两域分别取证：openapi 域与 gateway 域的 clienttype/cosyVersion/clientip/RSA 公钥/UA） | MITM SOP（复用 device_proxy）：`proxy_start` → Intl 客户端真实对话（含流式）→ 落盘提取（token 脱敏）；底稿 = 归档 §2.1/2.2 + **开源锚定（v1.4）：路径模板 `algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1` 已由 masknull endpoints.ts 互证** | §4.2 签名与上游实现定稿 + §3.6.3-6/10；**未闭合不写上游转发层**；🔶 v1.3 已实测端点域拓扑（六域，§1.1/§3.3）+ v1.4 路径模板已开源锚定（§3.3），请求体与签名取值仍待 MITM |
| R-G4 | **sash campaigns Intl**：base 域（openapi.qoder.sh?）、`Cosy-ClientType` 取值、CLAIMABLE 过滤、claim 幂等 replayed、benefit.amount | 归档 §3.1/3.2 底稿 + campaigns 抓包；**开源底稿（v1.4）**：umid 三头门控（`Cosy-MachineToken/Code/Type`，缺则 `active:false` 不报错）+ 双过滤器 `actionType==='CLAIM_BENEFIT'` + `claimStatus`（顶层 claimable 不可信）+ 每日 10:00 新轮新 campaignId（§4.3） | §4.3 签到实现定稿 + §3.5.10-1/2/9 |
| R-G5 | **quota/usage Intl 响应结构**：packages 明细（plan/addon 源、expire_at、FEFO 字段链） | Bearer 直调（幂等安全）+ 归档 §2.5 底稿；**开源互证（v1.4）**：另有 `/api/v2/user/plan`（套餐）与 `/api/v3/user/status`（账号状态）配套端点可一并接入（§3.3） | credits 解析器字段对齐 + §3.5.6 Dashboard Token 数据源 |
| R-G6 | **PAT Intl 通道**：PAT 创建入口（qoder.com 路径）、前缀、center.qoder.sh jobToken 交换 + appcode/signature + `?Encode=1` 现版有效性 | 归档 §2.7 底稿 + 实测；🔶 **部分收口（v1.4）**：PAT 创建入口已收口 `qoder.com/account/integrations`（双仓库 README 实证）；换取端点**双通道待定稿**（归档 center 域 `algo/api/v3/user/jobToken` vs 开源 openapi 域 `/api/v1/jobToken/exchange`，§3.3） | §4.1 PAT 通道实现 |
| R-G7 | **token 生命周期**：jrt-/其余前缀分派、token_key 字段名、6h 刷新间隔、refresh_token_expires_at | 归档 §2.5 底稿 + 轮询观察 | ensure_fresh 分支实现 |
| R-G8 | **模型目录与倍率**：Intl model/list 端点与响应、倍率表、Qwen3.8-Flash 0 系数在目录中的表达 | MITM 复用；🔶 **部分收口（v1.4）**：目录源已开源锚定 `{baseUrl}algo/api/v2/model/list?Encode=1`（baseUrl=api3.qoder.sh，§3.3），剩倍率字段与响应结构实测；**price_factor 折后价语义护栏**（§4.5） | catalog 区域化 + 帮助页倍率列 + §3.5.10-4 |
| R-G9 | **风控与指纹**：设备指纹校验强度现版复核（machinetoken 随机可过?）、本地指纹覆写文件清单、sash 对非桌面渠道 claim 的风控表现 | 归档 §2.3/§6.2 底稿 + 观察 | 🔶 **部分收口（v1.3）**：指纹格式 uuid4 三端互证、覆写三处路径已实测（§4.4）；`user` 解密文件存在性与服务端校验强度待登录复测 → §4.4 + §3.5.10-6 + 风险对策更新 |

> **分层**：R-G2~G5 为**阻塞层**（P0 收口，P1 池骨架与运营面动工前置）；R-G1/G6~G9 可随对应阶段顺延（R-G6/R-G9 → P1/P5，R-G4/R-G5 → P2/P3）；R-G3 是 P4 网关转发硬前置（沿 R-13「缺口未闭合不写上游转发层」红线）。

---

## 🚀 6. 分阶段实施计划（P0-P6 页面推进式）

> 对齐 `BuddyAI对齐国内Buddy六页开发计划.md` 的推进逻辑：**按功能域/页面逐步开发，每阶段独立可验收**。与 Buddy 计划（上游能力已实测、纯开发计划）的差异是本方案多一层 **Gate 前置**——R-G 未闭合的对应能力只做骨架不接线，UI 先行分区置灰、能力随 Gate 逐个点亮。每阶段改动清单**就地标注「国际不适用」UI 提示项**（对齐 Buddy 计划做法，不静默隐藏）；能力边界逐项依据见 §3.5.0 总表。

### P0 侦察收口（Gate 阻塞层，一切开发的前置）

- R-G2~G5 一次真实国际账号登录收口（设备流登录 → 抓 campaigns → 抓 quota → 抓 chat MITM，同 Buddy 方案「一次登录收口」手法）；R-G1 结论；产物落 `docs/completed/f80-qoder-global-recon/` 回填 §3.3 端点矩阵
- `QoderRegionEndpoints` 结构体落地（仅常量不接线，域 × 参数二维；CN 实例值与现状逐字一致，独立 commit 便于单独 revert）
- **验收**：阻塞层全部有结论；探测零脏数据入生产 KV（凭证在 Temp、已清理落盘）；CN 回归零变化

### P1 池骨架 + 账号管理对齐（Accounts 页，功能最重页先行）

- 后端：`qoder_global_*` 四表（schema v3→v4 + 版本号置位 + 备份）、token store 命名空间 `qoder_global_tokens`、池锁、账号 id `qdg-` 前缀；命令域 region 分流入口统一（accounts / data_io / groups / ide_store / env_reset / cli_status 第一批，其余随 P2-P5 接入）
- 导入三通道：设备流（R-G2）/ PAT（center.qoder.sh，R-G6）/ **CLI 登录态解密导入**（Intl 特有第 10 弹框，`~/.qoder/.auth/machine_id` 已实测 + `user` 待 R-G9）；探测未全过的入口置灰 + 提示
- 前端：Accounts 页区域 Tab；页头 11 按钮 + 9 列账号表（区域徽标 Global 紫 + CLI 桥状态列）+ 行内 5 操作 + 弹框群（OAuth 进度 poll 域换 Intl、PAT 预检文案补 Intl 前缀、环境重置清单 Intl 重排）；healthz 纳入 qoder-global selectable；PoolHealthBadges 扩五池
- **UI 标注项**：行内切换菜单与快照恢复为双目标 `profiles_qoder_global_work` / `_ide`（v1.3 双端实测，D-6 关闭），与 CN 同构无需裁剪
- **前置**：P0（R-G2 必须；R-G6 / CLI 解密未过则对应入口置灰）
- **验收**：`cargo test` 全绿（含 v3→v4 迁移用例）；Intl 导入幂等入池；区域必选校验拒绝跨区凭证；CN 池回归零变化

### P2 签到页对齐（每日领取 100 Credits 自动化）

- 后端：`qoder-global-checkin` 任务（10:15 单窗口 + gap 0-600s + 启动补签 + schtasks `AIWorkAssistant_QoderGlobalCheckin`）；campaigns → CLAIMABLE 过滤 → claim 幂等（`replayed==true` 视为 already）→ 失败复查恢复（CN 已落地直接继承）；结果落 `qoder_global_checkin_results`（90 天滚动）；`qoder-global-refresh` 6h 兜底任务
- 前端：Checkin 页 Intl 区——**单活动说明卡**（10:00→次日 10:00 窗口、桌面端渠道、30 天有效、错过不补）+ 一键领取表（账号/状态/领取结果/获取积分/可用积分）+ NDJSON 实时进度 + 90 天日历（月历翻页）+ 调度入口卡
- **UI 标注项**：CN「0 点签到」活动卡无对应——单活动化重写（骨架复用）；KNOWN_DAILY_CAMPAIGNS 盲发链裁剪为单活动形态
- **前置**：P1 + R-G4（**双坑护栏**：缺 `Cosy-ClientType` 头返回空列表 + 缺 umid 三头 `active:false` 不报错——empty_campaigns 三态区分「无活动 / `Cosy-ClientType` 缺失 / umid 缺失」，§3.5.10-2 / §4.3）
- **验收**：全链路自动到账（幂等重跑无副作用）；错过窗口显式提示；`qoder_global_checkin_results` 出数

### P3 积分与概述 / 看板对齐（Overview + Dashboard）

- 后端：quota Intl 化（R-G5；双通道 Bearer + client_token，CLI `/usage` 解析兜底通道裁剪）；`qoder-global-credits-snapshot`（23:40，三键同拨口径继承）+ `qoder_global_credits_history`；source 枚举 `plan | addon_daily` 对齐；会话压缩任务镜像 `qoder-global-sessions-compact`
- 前端：Overview Intl 区——5 StatCard + 近 30 天领取柱图 + 积分榜 Top10 + 配置导航（Intl 步骤收敛：装客户端 → 导入账号 → 开调度）+ **CN 停运倒计时特有卡**；Dashboard qoder 区 4 TABs Intl 分区（积分统计 / Token 统计 / 积分到期 / 会话明细）+ `qoderGlobalGatewayReady` 守卫；UsageStatsPanel 扩六桶
- **前置**：P1 + R-G5
- **验收**：余额 / 到期与官网 Settings > Usage 一致；快照 / 趋势图出数；逐包明细 30 天有效口径正确；国内侧 CN 签到/积分行为不变

### P4 网关转发 + API 服务页对齐（QoderApiService）

- 后端：§3.6.3 十项全量（签名 Global 分支域 × 参数二维 / `qoder_route` 双池按池路由 / 守卫按池放行 + `apply_flash_free_override` 顺序红线 / dispatch `qoder-global` 档默认关 / 冷却独立实例 / 粘性 `qg:` 前缀 / `UsageBucket::QoderGlobal` / 对话管线护栏全量继承）
- 模型目录 Intl 化（R-G8；CatalogState global 槽）+ 倍率首版静态配置（官网实际值人工修正）+ Qwen3.8-Flash 0 系数（前端 checkbox override 手法）
- 前端：QoderApiService Intl 区——积分横幅 + 池指标 3 卡（`qoder_global_pool_status` 3s 轮询）+ 账号池选择（分组/标签过滤 D5 交集）+ 6 参数键（`qoderGlobalEnabled` 默认关等）+ 模型目录表 + **地区说明卡文案反转**；API 使用帮助倍率列 +qoder-global 档（最小倍率口径扩展）；ApiManagerModal 档位下拉 +QoderGlobal；API Key `bind_pool='qoder-global'` amber 徽标；ResourceSummary +qoder-global 预设行
- **前置**：P1 + R-G3/R-G8（**R-G3 未闭合不写上游转发层——红线**）
- **验收**：Global 池流式对话连通 `api3.qoder.sh`；`record_usage` 落 `qoder-global` bucket；CN 池行为逐字不变（对比迁移前后 app_log 关键行）；`tsc` 无错误

### P5 设置页 + 环境配置收尾（Settings / HelpModal）

- Settings Intl 键镜像（§3.5.4 全表：`qoder_global_ide_path` / `qoder_global_work_path` / `auto_checkin` / `checkin_hhmm` / `gap_secs` / `token_renew_enabled` / credits_sync 四模式 / `sessions_compact_*`）；时刻类键零值回填（对齐 `qoder_checkin_hhmm` 教训）
- env_reset_items 按 Intl 目录族重排（R-G9 实测后定清单）；任务配置卡（每日领取 + 积分快照 + 6h token renew）
- 合规提示文案重写（**「多建试用账户将冻结」红线 + 仅本机自用声明**）；HelpModal Intl 分区（套餐体系 / 领取规则 / Qwen 0 系数 / CN/Intl 不互通声明 / 试用冻结红线 / CLI 解密导入教程）
- CLI 桥状态探测换 `~/.qoder/.auth/`；`qoder_open_ide` 开 `Qoder IDE.exe`、`qoder_open_work` 开 `Qoder.exe`（v1.3 双端实测路径）+ 打开数据目录
- **UI 标注项**：应用路径双键齐备（v1.3 双端实测，`qoderwork_path` 不再裁剪）；调度策略中心 Intl 无独立策略键（跟随统一策略，仅档位可选）——就地标注
- **前置**：P1 + R-G9
- **验收**：设置页逐键可读写；环境重置清单与实际目录族一致；HelpModal 内容完整；逐页与国内对照走查通过

### P6（可选后续）

- 会话明细 Intl 会话域复核（随 R-G3 顺延）
- CDP/UI 自动化兜底评估（仅当 R-G4 纯 HTTP claim 被渠道风控拦截；先评估成本再定，不阻塞 P1-P5）
- macOS 端实施（跟随 CN 池 M5 排期）；F-71 Trae SG 复用评估（共享 `QoderRegionEndpoints` + 独立池骨架方法论，决策点 5）

**回滚设计**：池骨架 schema 版本号置位 + 备份；dispatch `qoder-global` 档默认关；CN 侧唯一改动（端点结构体化等价重构）独立 commit 可单独 revert。

**验收口径（全阶段统一）**：

- 每阶段：`cargo test` 全绿 + `tsc --noEmit` 零错误 + 逐页与国内对照走查（结构/交互一比一，数据源差异按 §3.5.0 能力边界总表标注，不做假数据）
- **Gate 红线**：探测缺口未闭合对应能力不接线（不写无端点的假实现），UI 置灰 + 「待上游验证」提示文案
- **回归红线**：CN 池行为逐字不变（含 app_log 关键行对比），其余全部「新增而非修改」
- **安全红线**：凭证不明文出后端（导出 AIWQENC1 加密、CLI 解密导入即入 vault）；device_profile 仅本地为空才补入、绝不覆盖；删除类操作二次确认；写操作有 toast 反馈

---

## ⚠️ 7. 风险与对策

| # | 风险 | 等级 | 对策 |
|---|---|---|---|
| 1 | **情报时效**：调研归档为 2026-09-27 取证，客户端版本演进可能已改 COSY 参数/端点/公钥 | 高 | Gate 现版复核为硬前置；所有版本敏感参数（UA、cosyVersion、client_id）进 `QoderRegionEndpoints` 单点，更新仅改常量；**v1.4 起有第三方开源实现（hdhgsysh / masknull，均为 2026-09~10 活跃）交叉互证，情报时效风险显著缓解**（附录 B.3） |
| 2 | **试用冻结风控**：官方明文多建试用账户直接冻结（虚拟机不支持） | 高 | **红线**：不做注册/试用类能力，仅面向存量个人账号；UI 固定风险提示；每账号稳定指纹（非随机轮换） |
| 3 | **合规边界**：自动化访问国际站接口属灰色地带 | 中 | **仅限本机自用**（网关 `127.0.0.1`、不对外服务、不商用、不分发凭证、不破解付费墙）——开源生态分析 §6 合规备注同口径；API Key/凭证落盘仅本地 KV |
| 4 | **领取渠道仅桌面端**：官方口径 claim 入口在桌面端，纯 HTTP claim 可能被渠道风控拦截 | 中 | R-G4 实测；失败降级 = CDP/UI 自动化评估（低优先、先评估后立项）；empty_campaigns 显式区分不静默 |
| 5 | **上游成本/配额误判（v1.4 新增，源自开源实战教训）**：quota 码 110（日计费次数耗尽，日界重置）与 10605（排队/限流 transient）分诊混淆 → 无谓重试或误熔断；目录 `price_factor` 是折后价，直接抄会把工作时段成本低估数倍 | 中 | §3.6.3-10 错误分诊三护栏（先认 quota 码再认队列标记 / 队列 4 标记 ≥2 共现 / 401+队列标记归 RATE_LIMIT）；倍率以 `before_promotion_price_factor` 原价为准（§4.5 护栏） |
| 6 | **CN 停运（2027-09-05）与投入平衡** | 中 | 战略对冲定位：Intl 接入即 Qoder 生态长期存续路径；CN 池维持维护期不再扩功能 |
| 7 | **免费收益低于 CN**（3,000 vs 6,000 Credits/月/账号） | 低 | 价值定位 = 模型多样性（国际模型池 + 0 系数 Flash）+ 停运对冲，非纯额度替代；经济账见附录 A |
| 8 | **双池调度复杂度** | 中 | 严格按 §3.4 隔离清单执行；`qoder-global` 档默认关；同名模型跨区不混轮 |
| 9 | **国内侧回归** | 低 | CN 唯一改动为等价重构（端点结构体化），独立 commit + 回归用例；其余全部「新增而非修改」 |

---

## 📎 8. 遗留决策点（评审时定）

1. **池 id 命名**：§3.4 推荐 `qoder-global`（对齐 buddy-ai 命名法）；备选 `qoder-intl`。**v1.4 增信**：masknull/dsh-qoder-connect 恰用 `qoder`（china）/ `qoder-global`（global）同款双池命名，并注释「tokens 与文件永不跨区」——与本方案 §3.4 隔离清单取向一致。
2. **前端分区形态**：五页顶部区域 Tab（Buddy 同款）vs 独立 Tab（`QoderGlobal` 与 qoder 平级）——推荐前者（账号/签到/积分九成组件可共用，改动面最小）。
3. **存储方案**：`qoder_global_*` 四独立表（schema v3→v4，对齐 CN 四表先例）vs 现表加 pool 维度列——推荐前者（与 buddy-ai 独立存储取向一致、迁移回滚干净），若 store 现状更宜分库则从其约定。
4. **R-G4 若纯 HTTP claim 被渠道风控拦截**：CDP/UI 自动化兜底是否立项（建议：先评估成本再定，不阻塞 P1-P5）。
5. **是否同步立项 F-71（Trae SG）**：`QoderRegionEndpoints` + 独立池骨架可被 F-71 复用，若同期立项可共享 P0 方法论。
6. **Intl 快照/切换/Work 端目标形态（D-6）——已关闭（v1.3 双端实测）**：Intl 实测为双端并存（Work `Qoder.exe` + IDE `Qoder IDE.exe`），快照/切换双目标 `profiles_qoder_global_work` / `_ide`、`qoder_open_work` 恢复启用，与 CN 完全同构；§3.5.2/§3.5.4/§3.5.9/P1/P5 已同步回填，无需评审。

---

## 📚 9. 附录

### 🅰️ 附录 A：免费额度与积分机制（官方文档核实，2026-10-18）

**套餐体系**（docs.qoder.com/account/pricing）：

| Plan | 价格 | 高级模型额度 | 说明 |
|---|---|---|---|
| Free（体验版） | 免费 | 基础模型，消息条数受限 | 支持 BYOK；Credits 用尽自动降级基础模型 |
| Pro | $20/月 | 2,000 Credits/月 | 周期结束归零 |
| Pro+ | $60/月 | 6,000 Credits/月 | |
| Ultra | $200/月 | 20,000 Credits/月 | |
| 资源包 | $20 / 1,500 Credits | 1 个月有效 | 不可退款，可叠加 |

**免费可得额度**：

| 来源 | 额度 | 条件 |
|---|---|---|
| 每日领取 100 Credits（2026-09-18 起，结束另行公告） | **100/天 ≈ 3,000/月** | 桌面端用量面板 → 礼物图标**主动领取**；免费/试用/Pro/Pro+/Ultra 个人用户均可；Teams/企业版不适用；窗口每日 10:00→次日 10:00（UTC+8），**错过不补、不累计**；计入 **Add-on Credits**，每笔 **30 天有效**、跨天保留、逐笔分别到期 |
| 14 天 Pro 试用 | 一次性 300 Credits + Pro 全功能 | 仅新用户首登；**限一账号，多建试用账户直接冻结**（虚拟机不支持） |
| Qwen3.8-Flash 限时免费 | **0× 计费系数** | 2026-10-01 起继续免费，余额为 0 也能用，不扣任何 Credits |

**扣减规则**（官方 Credits 文档，与本项目 CN 实现同构）：FEFO 最先到期优先；同到期先 Plan 后 Add-on；失败请求不扣费；Credits 全端共享（Qoder / IDE / JetBrains / CLI / QoderWake / Cloud Agents / 移动端 / 网页版）；CN 与国际**不互通、定价不等价**。

**消耗参考**：Agent 模式 ≈50 Credits/请求、Editor Ask ≈3-4、Editor Agent ≈7-12（200K 上下文）→ 免费稳态 ≈60 次 Agent 请求/账号/月。

**余额查看**：qoder.com 右上角头像 → Settings > Usage（方案信息 + 分类余额 + 明细）。

### 🅱️ 附录 B：外部情报映射

**B.1 → `docs/completed/Qoder外部项目调研归档.md`**：

| 归档章节 | 内容 | 本方案落点 |
|---|---|---|
| §2.1/2.2 | COSY 会话密钥协商（RSA 公钥 + temp_key）+ 请求签名（五段 md5）+ 请求头全家族 | **未决项②底稿**：`qoder_sign` Global 分支（R-G3 复核） |
| §2.3 | 设备身份三字段生成（machine_id uuid4 / machine_token 68ch / machine_type 18hex）+ 无强绑定实证 | §4.4 指纹 Intl 格式（**v1.3 三端实测互证 uuid4**；服务端校验强度 R-G9 复核） |
| §2.4 | `~/.qoder/.auth/` 本地凭据解密（AES-128-CBC key=IV=machine_id[:16]） | §4.1 CLI 登录态导入通道（Intl 特有，§3.5.2 第 10 弹框；**v1.3 修正**：machine_id 文件实测名为 `machine_id`（uuid4 明文），`user` 文件现版未见、挂 R-G9 复测） |
| §2.5 | token 生命周期（jrt- 分派、6h 刷新、quota 端点） | §4.1 ensure_fresh Intl 分支（R-G7） |
| §2.7 | jobToken 交换（center.qoder.sh、appcode/signature、`?Encode=1` 自定义 base64） | §4.1 PAT 通道（R-G6）；§6.1 移植要点为 Rust 翻译对照表 |
| §2.8 | Device Flow 注册机（client_id、授权页、poll 时序、token 响应字段） | §4.1 设备流登录（R-G2 复核） |
| §3.1/3.2 | CN 签到直连互证 + campaigns 契约（Intl v0.8.18 统一、CLAIMABLE 唯一权威、claim 失败复查恢复） | §4.3 签到实现（R-G4）；复查恢复逻辑已在 CN 池落地直接继承 |
| §6.1/6.2 | Rust 移植要点表 + 需实测验证点清单 | P0/P4 实现对照表；§6.2 五点并入 Gate 清单 |
| §7/§8 | 风险清单（条款灰色/旧端点陷阱/域差） | §7 风险对策来源 |

**B.2 → `docs/completed/开源生态仓库价值分析.md`**（v1.1 新增）：

| 分析条目 | 内容 | 本方案落点 |
|---|---|---|
| §6 合规备注 | 自动化访问/凭证使用的合规边界口径 | §1.3 合规红线 + §7 风险 3（仅本机自用、不商用、不分发凭证） |
| §1 发现 1（workbuddy-cliproxy） | 上游协议避坑情报方法论（code 语义、审核拦截、最小改写） | §3.6.3-10 错误码族护栏设计方法论参照（Intl 错误码 R-G3 抓包复核） |
| §1 发现 2（antigravity-tools） | 会话粘性指纹、P2C 负载均衡、账号状态机、分级重试工程参照 | §3.6.3-7/8 粘性与冷却独立实例的工程实现参照 |
| §3.1 Trae 生态高价值项 | 同类包装器协议情报提取手法 | P0 MITM SOP 复用（复用 device_proxy 探测链） |

**B.3 → 两个 Qoder Intl 开源实现（v1.4 新增，源码级分析；`hdhgsysh/dsh-connect-qoder` MIT + `masknull/dsh-qoder-connect` MIT，后者上游 `mo-n/dsh-provider-qoder` 亦 MIT）**：

| 仓库 / 文件 | 情报点 | 本方案落点 |
|---|---|---|
| masknull `src/qoder/transport/endpoints.ts` | 双区端点全表；`QoderRegionEndpoints` 三字段形态（baseUrl/openApiUrl/centerUrl）；chat 与 model/list 完整路径模板 | §1.1 域名族谱互证 + §3.3 端点矩阵（chat/目录行开源锚定）+ §4.1 G-1 |
| masknull `src/variants.ts` | `QODER_VARIANTS` 双区变体：provider id 恰为 `qoder`（china）/ `qoder-global`（global），「tokens 与文件永不跨区、目录互不混用」 | §3.4 隔离清单 + §8 决策点 1 命名互证 |
| masknull `src/qoder/codes.ts` | 官方错误码全表（与 `docs.qoder.com/cli/sdk/errors` 一致：quota 110-122 / request 406~80412 / runtime 500-10605 / auth 105） | §3.6.3-10 错误码族 Intl 版底稿（R-G3 复核） |
| masknull `src/qoder/errors.ts` | 错误映射：quota 码 → QUOTA 且不带 providerRetryAfterMs；`deauthorizeDigits` 改写 401/403 数字防文本误判；`ProtocolShapeChangedError`（200+陌生信封 retryable=false） | §3.6.3-10 护栏③ + 风险 5 |
| masknull `src/qoder/transport/machine-id.ts` | machine_id 读取链：官方 CLI 文件优先 → 自有 seed 兜底；uuid4 + `wx` flag 防创建竞争 | §4.4 CLI machine_id 读取链 bullet |
| hdhgsysh `lib/errors.js` | 错误分诊三护栏：先认 quota 码再认队列标记；队列 4 标记 ≥2 共现；`retryAfterSeconds` 嵌套递归解析（最深 4 层）；401/403 带队列标记归 RATE_LIMIT | §3.6.3-10 三护栏 + 风险 5 |
| hdhgsysh `lib/claim.js` | campaigns 契约完整实现：umid 三头门控（2026-09-27 实测）、双过滤器、claimable 不可信、VIEW_DETAILS CLAIMED 无 benefit 坑、claim 每次重读 campaigns、replayed 不报 amount、benefit 字段链 | §4.3 活动认知 + 实现口径底稿（R-G4） |
| hdhgsysh `lib/offpeak.js` | `price_factor` 折后价语义（`before_promotion_price_factor` 才是原价）；22:00-08:00 跨午夜析取；`Intl.DateTimeFormat` 防 DST；`promotion.active !== true` 守卫 | §4.5 price_factor 折后价护栏（R-G8） |
| hdhgsysh `lib/region-gate.js` | region-gate 三分支（无凭据 info / `expired===true` 严格判定 warn / 其余 ok）——不发布死路由 | §5 Gate 分层注 + §6 UI 先行置灰节奏参照 |
| hdhgsysh `docs/KNOWN_GAPS.md` | 盲区清单：RSA 填充模式（PKCS#1 v1.5 vs OAEP）+ AES key 随机性不可测；macOS Keychain PBKDF2-HMAC-SHA1 vs Windows DPAPI；`compat.supportsDeveloperRole:false` 决定 403 10605；MACHINE_OS darwin 档实测 200 | R-G3（RSA/签名盲区）+ R-G9（跨平台指纹）+ §4.2 签名实现待测清单 |

### 🅲 附录 C：参考来源

**官方**：docs.qoder.com（pricing / Credits / events/100credits / events/flashoffer / FAQ / **cli/sdk/errors 错误码文档**，v1.4 增补——quota 家族唯一覆盖 HTTP status 的官方权威来源）；qoder.com 门户。

**开源（v1.4 新增，源码级分析，映射见附录 B.3）**：`github.com/hdhgsysh/dsh-connect-qoder`（MIT）——campaigns 契约 / 错误分诊 / offpeak 语义 / region-gate / KNOWN_GAPS 盲区清单；`github.com/masknull/dsh-qoder-connect`（MIT，上游 `mo-n/dsh-provider-qoder` 亦 MIT）——双区端点表 / 官方错误码表 / machine_id 读取链 / `qoder-global` 双池命名先例。两仓库均为 2026-09~10 活跃维护，与本方案 v1.3 本机实测（2026-10-22）交叉互证。

**内部**：`docs/completed/Qoder平台支持设计方案.md`（CN 全量底稿 + §5.8 页面表 + §5.10 指纹 + §5.11 网关池方案 + §六 R-1~R-13）；`docs/completed/Qoder外部项目调研归档.md`；`docs/completed/开源生态仓库价值分析.md`；`docs/completed/Buddy国际版支持落地方案.md`（独立池范式与 Gate 方法论）；`docs/backlog.md` F-80-余（Global 区决策留档）。

**本机实测（v1.3，2026-10-22）**：国际版 Qoder Work 端（安装目录 `C:\Users\tianw\AppData\Local\Programs\Qoder`，Electron，`appId=com.qoder.app`、`dataDirectoryName=com.qoder.app.stable`）+ Qoder IDE（VS Code fork 1.106.3 基座、产品版本 1.35.0、`dataFolderName=.qoder`、内部代号 lingma）。端点六域拓扑（§1.1/§3.3）、三处 machine_id（§2.1/§4.4）、双区四组目录对照（§2.3/§5 R-G1）、模型阵容 14 项（§4.5）均来自本机安装目录与数据目录实测（`%APPDATA%\com.qoder.app.stable`、`%APPDATA%\Qoder`、`~/.qoder/.auth/`；凭证/敏感文件 `auth.v1.dat`、`machine_token.json` 刻意未读）。

**代码锚点**（`AIWorkAssistant_main`）：`src-tauri/src/tasks/qoder_{common,checkin,credits,sign,upstream,catalog,refresh}.rs`；`src-tauri/src/api_server/qoder_route.rs`；`src-tauri/src/commands/qoder/{oauth,accounts,credits,checkin,ide_store,data_io,env_reset,cli_status,common,groups,sessions}.rs`（33 命令，§3.5.9 全量清单）；`src/pages/qoder/*.tsx`（五页 + HelpModal）。
