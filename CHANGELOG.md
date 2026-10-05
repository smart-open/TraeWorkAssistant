# 更新日志

本文件记录 AI Work Assistant（Web/Docker 镜像版）的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

> 分支说明：本分支（`docker_main`）为 Docker 镜像产品线（Web-only，axum 单体 + 浏览器管理面），与桌面版 `main` 分支不是同一产品形态；仅按提交语义移植系统无关的修复与功能。

---

## [1.4.1] · 2026-10-05 · 资源调度 per-pool 拆分 + API 管理 Qoder 池绑定 + Web 化体验修复

> 本轮无 main 移植内容（Qoder 池绑定等在 main/feat-qoder 均不存在，为 docker 分支三池结构下的自研对齐）；合并点维持 `main@9cdce69` 不变。

### 新增

- **资源调度参数 per-pool 拆分**（1.4.0 曾以「docker 后端无对应结构」跳过，本轮三池结构下完整落地）：
  - `api_pool` 共享字段 `account_concurrency_limit` / `pool_sticky_ttl_secs` 退役，拆分为 Trae / Buddy / Qoder 三池独立参数组（账号并发上限、池粘性 TTL、显式会话粘性 TTL、竞速对冲阈值）；读取侧统一走 `load_pool_file_with_legacy_migration`——仅 Buddy 池沿用旧共享值（缺失回填、下次保存固化、幂等），Trae / Qoder 池落默认值（并发 1 / 池粘性 300s / 会话粘性 1800s / 对冲 8s）。
  - Trae 池补齐：`trae_enabled` 参与调度开关（关闭后仅 Trae 源模型显式报错，Buddy/Qoder 不受影响）+ 慢请求竞速对冲（首字节超阈值向第二账号发对冲，先出首字者胜，`TraeHedgeLease` 管理在途计数防泄漏）+ 显式会话粘性（`t:` 命名空间 sticky_bindings，滚动续期）；Qoder 池显式会话粘性 TTL 由硬编码 1800s 改可配。
  - 前端三页（Trae / Buddy / Qoder · 资源调度）per-pool 参数编辑面板（回显 / 校验 / 保存即热生效），Buddy 页参数键名同步 per-pool 化。
- **API 管理 · Qoder 池绑定**（issue #25/#30 三池化）：子 Key 资源池绑定新增「Qoder 池」选项（新增表单 + 调度配置弹框，单选切换清空跨池勾选）；限定上游 / 专一候选跟随全局时合并三池展示（Qoder 徽标 amber 区分）；绑定 Qoder 但上游未启用时弹框内显式告警（提示将回退 Buddy/Trae 池）；混合白名单编码扩展 `qoder:` 前缀（后端 `parse_bind_pool` / `qoder_route` 约束链路 1.4.0 已就绪，本轮补前端）。
- **Buddy 环境配置 · 本机 auth 文件路径**：`Settings.wb_auth_file_path` 暴露到环境配置页（Docker / 远端部署将宿主机客户端 auth 文件挂载进容器后填写容器内路径，「账号管理 → 扫描本机账号」从该文件读取；服务端 `wb_common` 人工指定优先逻辑既有）；扫描零凭证提示同步引导至该配置。

### 修复

- **Qoder OAuth 登录无法自动打开浏览器**：`qoder_oauth_login` 命令同步返回授权页 URL（桌面版 `open_in_browser` 的 Web 化等价——命令返值 + 前端在点击手势的 transient activation 窗口内 `window.open`，避免浏览器弹窗拦截）；被拦截时 toast 提示，弹框内授权链接改为可点击 `<a>`（进度事件兜底下发同一链接）。
- **Qoder 凭证定时刷新误渲染时间输入框**：every6h 固定周期任务改渲染 violet 徽标（title 注明「固定每 6 小时执行、触发时刻不可配置」），不再展示无效 time input 引导用户误改。
- **Buddy 概述残留客户端本机态区块**：移除「登录账号」「本机套餐」StatCard（依赖客户端本机 auth 在线判定，Web 版无数据源恒空），统计行 5 → 3 列。
- **BuddySettings 全局设置调用路径错误**：`api.settingsGet/Set` 顶层误用 → `api.misc.settingsGet/Set`（tsc 拦截修正）。

### 验证

- `cargo test --workspace` 548 通过（536 + 12）· `npm test` 57 通过 · `npx tsc --noEmit` 0 错误。

---

## [1.4.0] · 2026-10-05 · 移植 main Qoder 平台全链路 + 系统无关修复

> **合并点记录**：移植范围 `main@1c29564`（**不含**）至 `main@c3f3211`，另含 `main@9cdce69`（Qoder 签到兜底直领）。**下次合并请从 `9cdce69` 之后接着移植**。手工语义移植、未经 merge。

### 新增

- **Qoder 平台全链路**：协议层（积分账户/积分包/签到/用量/模型目录）+ 调度器与网关池接入 + 3 命令（`qoder_pool_status` / `api_qoder_usage_stats` / `api_qoder_catalog_sync`）；前端 `src/pages/qoder/*` 六页；Dashboard 三平台化（Trae/Buddy/Qoder：KPI、快照差分曲线/热力图、Token 用量、到期日历按平台口径适配）。

### 修复（移植）

- **签到档期日历双修**（`ded687d` 系统无关部分）：同日同账号多轮记录按最终态去重；BuddyCheckin 接入活动档期日历。
- **模型目录**（`6ed778a`/`c3f3211` 系统无关部分）：Max Mode 角标 + 厂商列；积分明细包数口径与 KPI 对齐。
- **Qoder 每日签到误报「无可领活动」**（`main@9cdce69`）：campaigns 列表对工具请求形态过滤 CLAIMABLE——列表零 CLAIMABLE 时对已知每日活动盲发直领兜底（严格判定 `200+CLAIMED+!replayed`，回放/4xx/5xx 维持 already，401 走自愈重试；首次与 401 重试路径同口径）；campaignId 强制路径安全白名单。
- **审计补移植**：WB chat `prompt_cache_key` 注入（`8838e85`）；6004 配额三态多锚点（`ab67a60`）；auth 文件提取分类报错（`406c50b`）；凭证 vault 收敛改道/刷新失败四分类/响应体读取失败不吞错/SSRF WHATWG 解析（`4f1174b`）；到期日历「长期有效」哨兵（`34e1757`）；Buddy 模型厂商列（`59a594d` 系统无关部分）。

### 安全与健壮性（发布前审查 19 项全修）

- **Critical**：WB 对冲账号按自身 uid 重建 body（防跨账号前缀缓存泄露对话）；WB/Qoder sticky 绑定命名空间隔离（防互删）；WB 账号入池改走 vault secure 读。
- **Major**：SSRF 加固（尾点/IPv6 兼容段/NAT64/CGNAT）；wb_tokens 存量明文启动收敛；429/408 不再误标永久态；OAuth 事件桥改 web `listen`；撤除 Qoder 资源调度页假保存参数。
- **Minor 11 项**：冷却多锚点取最大、热路径零克隆、sticky seed 限长、vault 值优先、ns 键校验、导出防双击、重登录清态等。

### 构建修复

- **CI Linux 构建缺依赖（docker-image.yml Rust 测试门禁失败）**：`aiwork-core/Cargo.toml` 本次移植新增的 `[target.'cfg(windows)'.dependencies]`（windows-sys，Qoder CrossProcLock 用）被插在依赖清单中间，其后 13 个跨平台依赖（aes/cbc/p256/rand/argon2/aes-gcm/rusqlite/iota_stronghold/zeroize/flate2/futures-util/regex/time）全部误入 Windows 专属段——本地 Windows 构建无感，CI Ubuntu 上 87 个编译错误。已将 Windows 专属段移至文件末尾；`cargo tree --target x86_64-unknown-linux-gnu` 验证 Linux 依赖图恢复，Cargo.lock 无变化。
- **qoder_sign 测试平台硬编码**：`build_cosy_headers_produces_all_19` 断言 `Cosy-Machineos` 硬编码 `x86_64_windows`，Linux CI 实际产出 `x86_64_linux` 失败。改为断言头值等于 `machine_os()` 输出（平台自适应，Windows/Linux/macOS 全通过）；`qoder_common.rs` 探针内的 `x86_64_windows` 为复刻抓包包头的 `#[ignore]` 字面值，保留。
- **Docker 镜像构建 rust 1.88 被 uuid MSRV 卡住（GHCR 推送 job 失败）**：本次移植新增的 `uuid` 依赖锁到 1.27.0（要求 rustc ≥1.89），`rust:1.88-slim` 构建阶段 exit 101（此前两轮门禁失败时该 job 一直被跳过、首次真正执行即暴露）。基础镜像升级 `rust:1.88-slim → rust:1.97-slim`（与 CI stable 工具链/本地验证同代，避免逐版追赶）；workspace `rust-version` 声明 1.88 → 1.89（真实 MSRV），AGENT.md / docs/tech-framework.md 同步。

### 明确跳过（桌面客户端专属）

- Trae 包级积分口径/tokenStats 懒加载；BuddyAccounts 桌面环境操作（凭证导出除外，已带 Modal 强确认）；DiscoverModal 徽标；per-pool 字段拆分（docker 后端无对应结构）。

### 验证

- `cargo test --workspace` 544 通过 · `npm test` 57 通过 · `npx tsc --noEmit` 0 错误。

---

## [1.3.6] · 2026-10-04 · 移植 main 渠道风控对抗 + Claude Code 分类器兜底

> **合并点记录**：本次移植范围 `main@29d106af1da7bc6e1df477c8f0b7c4ebb9811ab3`（**不含**）至 `main@1c295643c0d5996be32d0a2f9f19e6ab22c1a9be`；**下次合并请从 `1c29564` 之后接着移植**。手工语义移植、未经 merge。

### 修复（移植 main `0441851`，issue #54）

- **/v1/messages 分类器模型兜底**：Claude Code auto 模式的安全分类器以 side_query 请求本网关，模型名为服务端下发的官方名（如 `claude-sonnet-5[1m]`），无环境变量可覆盖；纯 Trae 用户三池不可路由 → 上游 4001 → 分类器 fail-closed 报 "temporarily unavailable" 并阻断 Bash/Write：
  - 入口三级兜底：原名可路由原样保留（WB 用户内置系列映射行为不变）→ 剥 `[1m]` 窗口标记后基名可路由用基名 → claude-* 系不可路由回落网关默认模型；非 claude 未知名维持透传语义。
  - 改写原子生效（peek/body/model 三处同步）；热路径零分配短路。
  - `[1m]` 剥离用 ASCII 字节尾比较，规避 to_lowercase 字节漂移导致的多字节字符切片 panic（对照 `wb_model_route::strip_suffix_ci` 同类修复）。

### 修复（移植 main `dc3b32d` + `fc13a1b`，issue #57）

- **Trae 池接入指纹清洗 + 11128/空完成感知重试**：渠道风控（11128 Illegal API invocation）按请求指纹判定、与账号/模型无关，换模型换账号无效：
  - 模板映射预防布防：Cline/Roo/通用 CLI/OpenCode 全 flavor 身份句（`wb_payload.rs` default_template_map + 命中计数/统计导出）。
  - Trae 池接入清洗管线（content/tool arguments/工具描述），与 WB 池同一套规则表（`payload.rs::prepare_llm_chat_body` 新增 sanitize/templates 参数）。
  - 11128 渠道风控感知：未清洗请求被拦（HTTP 400）时强制清洗重算请求体同号重试一次（Trae 流式/聚合 + WB 流式/聚合/工具代执行共 5 处调用点）；已清洗仍命中 → retry_plan 原样 Fatal。
  - 空完成哨兵（`EMPTY_COMPLETION_CODE = -9901`）：上游 HTTP 200 正常收流但零内容（影子风控静默拦截）不再伪装成正常完成——流式转换层不发收尾帧以哨兵上抛，调用方换号重试（不冷却、不透传）；聚合响应零内容（OpenAI chat/legacy text/Anthropic message 三形态）同样换号。
  - sanitize/templates 提升为**请求级快照**（`fc13a1b`）：11128 强制开启后跨账号保持，换号不再以未清洗状态重烧一次拦截（每账号 +200ms），且每请求只做一次 load_templates SQLite 读；热更新开关下一请求生效。
  - 模板命中计数与风控日志挂钩（「空完成 → 换号重试；模板命中: …」），ZCode/DSH 等无公开资料客户端可经日志反查精确触发句 → 热更新规则表。
  - 新增 `scripts/sim_zcode_dsh.mjs` 客户端模拟测试（Node 18+ 零依赖，probe 打满全部已知指纹句，`--system-file` 注入真实抓包 system prompt 作迭代通道）。

### 明确跳过（桌面客户端专属，docker 分支不适用）

- main `8d540e4`（device_proxy e2e 隧道测试补读 body 修 CI 偶发失败）：docker 分支无 `src-tauri/src/device_proxy/` 模块（桌面设备代理隧道）。
- main `1c29564` 的 3.6.6 版本号升级：docker 分支走独立 1.3.x 版本线（本次升级 1.3.5 → 1.3.6，合并点即记录于本条目）。

### 验证

- `cargo test --workspace`：419 通过（含新增 issue #54 兜底 6 例 / issue #57 空完成哨兵、模板命中计数、指纹清洗等）。
- `npm run test`：32 通过（前端无本次相关变更）。

---

## [1.3.5] · 2026-10-01 · 移植 main 客户端指纹伪装 + auth 键诊断

> **合并点记录**：本次移植范围 `main@31fa051c52beb9b0e227f87dd87b5f94f6d28477`（**含**）至 `main@29d106af1da7bc6e1df477c8f0b7c4ebb9811ab3`；**下次合并请从 `29d106a` 之后接着移植**。手工语义移植、未经 merge。

### 修复（移植 main `e845bf2`，issue #48）

- **WB 客户端指纹伪装补齐**：个人中心「请求明细」客户端列不再显示 "-"——
  - chat 链路（`wb_upstream.rs::build_chat_headers`）：新增 `X-IDE-Type/Name/Version`（CLI 身份，版本与 UA 一致，新常量 `WB_CLI_VERSION`）+ `X-Request-ID`（每请求随机 32 位 hex）+ `X-Machine-ID/X-Session-ID`（账号级稳定派生）。
  - billing/签到链路（`wb_common.rs::build_auth_headers`）：UA 升级为 `WorkBuddy/5.5.6` 带版本形态（新常量 `WB_DESKTOP_UA`，裸 "WorkBuddy" 会被识别为 "-"）+ 指纹头 + `X-Domain`。
  - 刷新端点（`wb_upstream.rs::refresh_access_token` / `workbuddy/accounts.rs`）UA 同步对齐。
  - 指纹派生 `derive_device_fingerprint`：sha256(`wb-fingerprint:{kind}:{uid}`) 前 16 字节 → 32 位 hex；同账号恒定、跨账号隔离防关联；uid 缺失即不带不伪造。
  - **docker 分支兼容性偏离**：`X-Request-ID` 用 core 既有 `commands::oauth::random_hex(32)` 产出（与 main 的 `uuid::Uuid::new_v4().simple()` 同为 32 位 hex），避免为 aiwork-core 新增 uuid 依赖。

### 修复（移植 main `07aa845`，issue #51）

- **auth 文件提取键对齐 creds_of 超集**：scan/import 的 token 键补通用 `token`，expires 键补 `expires_at_ms` / `accessTokenExpiresAtMs`（`as_ts_seconds` 自动毫秒折算秒）。
- **失败错误自带键名诊断**：新增 `common.rs::auth_key_names`——「未找到 accessToken」错误附带顶层及 auth/account 一层子对象键名（仅键名绝不含值），用户截图即可定位结构变更。
- main 同提交中的 `env_reset.rs` 三键口径同步**未移植**：docker 分支无桌面环境重置模块。

### 文档（移植 main `1b8937d` + `d2d4843`）

- AGENT.md 新增「远端同步规范」（main §16 → docker §15，编号偏移已在文中注明）。
- 新增 `docs/tmp/trae-cli-bridge-plan.md`（Trae CLI 桥接落地方案：个人账号不可用实证 + 企业后端模拟四阶段计划）。

### 明确跳过（桌面客户端专属，docker 分支不适用）

- main `11c25bc`（switch 登录态双层身份守卫 + 槽位 sidecar + .bak 两代轮转）：依赖 `src-tauri/src/switcher/` 与 `commands/switch.rs` 桌面槽位切换模块，docker 分支无此模块。
- main `31fa051` 的 `wb_route.rs` 模块头注释修正：已随 1.3.4 断连检测移植带入。
- main `31fa051` / `29d106a` 的 3.6.4/3.6.5 版本升级：docker 分支走独立 1.3.x 版本线（本次升级 1.3.4 → 1.3.5）。

---

## [1.3.4] · 2026-09-29 · SSE 断连检测全链路移植

### 修复（移植 main@31fa051 自 8665e4c 以来的系统无关变更）

- **SSE 客户端断连检测全链路（移植 main `24eb1d0` + `0028c93` + `73f3480` + `5abb891`）**：客户端（agent）断开后，僵尸流不再占用账号并发槽导致新请求排队超时（499 "Request aborted" 聚集于 maxWaitMs）：
  - Trae 路径（`routes.rs` / `sse.rs`）：轮换与同账号重试入口 `tx.is_closed()` 快速终止；流转换发送点失败即退出读循环；Anthropic 路径 `send!` 宏置位 `disconnected` 标志主循环检测退出，断连后跳过收尾。
  - 可中断行源（`wb_upstream.rs`）：新增 `InterruptibleLines`（`next_timeout` 区分 行/EOF/停滞窗口 三态，Iterator 语义兼容）；`lines_with_first_byte_timeout_interruptible` 供流式路径直用，ttfb 包装产物经 `from_iterator` 桥接（语义不变）。
  - 停滞期轮询（`sse.rs` / `wb_sse.rs`）：转换循环改 500ms `LINE_POLL` 轮询取行，停滞窗口内检查 `sender.is_closed()`，断连即退出（不再死等上游 300s 读超时）；WB 解析器拆出 `feed_line` 共用，新增 `next_event_polling`。
  - WB / 自定义渠道路由（`wb_route.rs` / `custom_route.rs`）：外层轮换、内层重试入口及 RetrySame 退避后断连即 return；活跃流期间逐事件顶部 `tx.is_closed()` 快速检测（对齐「发送失败即断」）。
  - 断连即释放上游连接与账号并发槽，usage 记账取断连前已收到的 usage 事件；新增 5 个断连语义测试（`api_server::` 304 通过）。

### 修复（移植后审查对齐，本地主动偏离 main 的 4 处）

- **审查修复**：
  - `routes.rs` 内层重试循环顶部补 `tx.is_closed()` 断连检查，对齐 `wb_route.rs` 既有写法（修复 401 自愈 continue 路径绕过外层检查、客户端已断连仍多发一次上游请求）。
  - `sse.rs` 两个 OpenAI 系转换循环（chat/completions）补循环顶主动断连检测，与 Anthropic 版 / `wb_sse.rs` 风格统一（检测及时性增强）。
  - `LINE_POLL` 轮询步长收敛至 `wb_upstream.rs` 单一事实来源（`sse.rs` / `wb_sse.rs` 改为引用），防后续调参漂移。
  - 语义声明：流式路径改用 `InterruptibleLines` 后，流中途读错误由旧的「跳过继续读」（`chain_rest` filter_map）变为「首错即 EOF 终止」——SSE 场景读错误通常意味着连接坏死，终止属改进，并避免旧实现对持续读错误的忙转。

---

## [1.3.3] · 2026-09-28 · 网关状态页看板增强

### 增强

- **`/gw-status` 看板优化（数据面 + 页面）**：
  - `/health` 数据扩展：`wb` 池补 `cooling` / `disabled` / `total_credits`（Buddy 池总积分）；新增 `tokens_today` 今日输入/输出 token 汇总（Trae/WB/Custom 三池合计，读内存用量快照、免磁盘 IO，与记账同用本地时区日键）。
  - KPI 分区布局：概览（总请求 / 今日输入 / 输出 token）、Trae 池（可用账号 / 冷却禁用 / 通用积分合计）、Buddy 池（可用账号 / 冷却禁用 / 池总积分，未启用时整区隐藏）；大数缩写（亿/万）。
  - 移动端适配：窄屏 KPI 双列、API Key 输入框 16px 防 iOS 聚焦缩放、表格横向滚动。
  - API Key 401 显式提示与自查指引（区分网关 Key 与管理面登录令牌、禁用 Key 同样 401）。
  - 网关不可达时状态点回落、清空 Trae 池 KPI 并隐藏 Buddy 池分区，避免残留旧值呈矛盾视图（恢复后 5s 自愈）。
  - 文档与注释同步：`/health` 免鉴权口径统一表述为「仅输出聚合探活级汇总，无账号级明细」（`server-deploy.md`、`auth.rs`）。

---

## [1.3.2] · 2026-09-27 · WebUI 状态页 + 移动端适配 + 免令牌开关

### 新功能

- **网关 Web 状态页 `/gw-status`**：新增 `api_server/status_page.rs` 单文件内嵌 HTML 状态页（深色主题 CSS 变量、KPI 分区卡、账号用量表格、移动端适配、5s 自动刷新）；`/gw-status` 与 `/health` 免鉴权——页面为静态 HTML，`/health` 仅输出聚合探活级汇总（池计数 / 通用积分合计 / 今日 token 三池合计），无账号级明细；页面内账号明细与模型目录由浏览器另行请求鉴权端点获得。根路径 `/` 保持管理面入口（SPA → 登录页，ADR-4）不变；登录页页脚与状态页副标题提供 `/` ⇄ `/gw-status` 互跳链接。
- **WebUI 免令牌访问开关**：`Settings` 新增 `web_auth_disabled`（默认关）；管理面鉴权中间件在开关开启时整体跳过 Cookie 鉴权，每请求读 kv 即时生效、无需重启；安全与管理页新增开关卡片（开启后刷新页面即免登录）。受信任内网专用，公网部署应保持关闭。

### 修复

- **移动端适配**：窄屏抽屉式侧栏（遮罩 + 汉堡按钮，`md` 及以上保持常驻侧栏）；`Sidebar` 根元素补 `h-full`，修复侧栏高度不铺满（常驻与抽屉两场景均满高）。

---

## [1.3.1] · 2026-09-26 · 移植 main 积分看板重建 + Issue #38 `-max` 修复批

> 范围：移植 `main@425008c0`（v3.6.2）以来的系统无关变更（`4760e9d` / `82065d4` / `9cc9c86`），手工迁移、未经 merge。

### 新功能

- **积分看板重建——Trae/Buddy 平台拆分 + 三源数据矩阵**（移植 main `9cc9c86`）：同一看板组件按 `platform` 参数渲染两个独立页面（Trae `credits` 视图 / Buddy `buddy-credits` 视图），旧 `Credits` / `BuddyCredits` 两页退役删除；KPI 7 卡（单平台各自口径）+ 积分统计 Tab（官网/API 网关两源切换，网关以请求数为口径不估算积分）+ Token 统计 Tab（网关 Trae/Buddy 池 90 天 / 官网 Trae token 明细 365 天）+ 积分到期 Tab（账号明细 + 到期日历）；抽公共组件 `useDateRange` / `ChartFilterBar` / `ActivityHeatmap` / `ModelRanking` 与 BoardPoint 适配层。
- **WB 每日快照方案 B**：`wb_credits_history` 快照新增 `earned` 列（当日余额差分与签到 reward 归并，schema 幂等补列免版本迁移）；新增 `workbuddy_credits_history_list` 命令供看板读取快照时序（365 天）；官方用量聚合（`workbuddy_usage_official_all`）新增按模型 31 天全窗口汇总输出。
- **Trae 官网消耗明细**：移植 `usage_history` 模块——直连 Trae 用量接口按会话拉取（credits_float / model / token 明细），按本地自然日聚合落盘、增量重拉替换语义（fresh=false 纯缓存读取），供积分看板官网源与「今日消耗」KPI 使用。

### 修复（移植 main `4760e9d` + `82065d4`）

- **`-max` 后缀请求上游 4001**：dispatch 剥离 `-max`/`-thinking` 后回写请求体 `model` 为基名（此前 payload 按未收录名生成 `xxx-max__dev` 致上游 `4001 param is invalid`）；Max Mode 注入值改布尔 `is_max_mode:true`（数值 1 被上游拒绝）；`prompt_max_tokens` 固定 168000；全局模型白名单准入对齐后缀剥离规则（基名在名单即放行）。
- **流内请求级错误不再打满整池**：4001 等请求级错误终止账号轮换、按 400 透传且不冷却；聚合路径补请求级错误守卫。
- **`/v1/models` 补序列化 `max_mode` 字段**（Max Mode 对客户端可发现）；双源模型 `context_length` 按调度命中侧选定，WB 池未启用时不再被残留快照拖低。
- **app_log 轮转**：单文件 10MB 滚动裁剪，防长跑日志无限增长。

### Web 版适配（与 main 的有意差异）

- **本地 Token 统计源下线**：桌面版的本地 token 统计依赖扫描本机 `~/.workbuddy` / `~/.codebuddy` 客户端会话文件，Web 版无意义——数据源切换器不渲染「本地」选项。
- **Trae 逐条积分流水回退口径省略**：KPI「今日新增」直接采用快照 `earned` 口径（无 `creditsHistory` 逐条流水回退）。

### 文档

- 用户手册 §7.6：`is_max_mode` 实证口径、`/v1/models` 各字段口径（`context_length` / `max_mode`）补充；AGENT.md 前端结构与命令表同步。

---

## [1.3.0] · 2026-09-25 · 模型档位统一空间 + Max Mode 出站 + 定时同步扩展

### 新功能

- **模型档位统一空间与 Max Mode 出站接线（Issue #31，移植 main `c8e855b` 批）**：统一档位空间（minimal/low/medium/high/xhigh/max）与 Trae wire（light/high/extra_high）按池映射转换；Max Mode 出站接线（`-max` 后缀路由剥离 + `is_max_mode` 注入）；档位声明双源诚实合并；使用帮助新增档位/Max Mode 模型表与说明；Trae 表外模型显式请求档位按映射填充默认下发。
- **积分看板与官网模型定时同步**：调度器新增 Buddy 上游模型目录同步 / Trae 官网模型列表同步等定时任务（可配置时刻，适配 Web 版调度）。
- **网关监听 0.0.0.0**：支持局域网接入（安全提示：未启用 Key 时匿名放行）。

### 优化

- **调度器**：调度配置整轮单次读取，降低每轮 IO；补充调度计划单测。

---

## [1.2.0] · 2026-09-23 · main 批量语义移植（main@43dc9d6..df8010e）

### 新功能 / 移植

- **批量移植 main 功能并适配 Web-only Docker 版**（`main@43dc9d6..df8010e` 按提交语义逐项落地）。
- Buddy 定时任务卡补 `wb-growth` 条目（移植审查发现的展示遗漏）。

---

## [1.1.0] · 2026-09-22 · Key 级资源池绑定 + 全局模型白名单 + Buddy 积分趋势

### 新功能

- **API Key 级资源池绑定与全局模型白名单**（移植 main）：ck_ 子 Key 可绑定指定资源池，模型白名单全局准入控制。
- **Buddy 积分趋势图三线**（总余额 / 获得 / 消耗）+ 五档日期区间切换。
- **调度任务可自定义执行时刻** + 定时任务开关配置；通知渠道并入系统设置并纳入底部「保存设置」统一保存。
- **网关地址跟随访问域名/端口**：移除独立端口配置，接口地址自适应当前访问地址；鉴权层拒绝请求补记网关日志。
- **BoundDeviceID 持久化** + 环境配置页改版（与系统设置整合）。
- 账号导出导入支持 Web 简版 JSON；Buddy OAuth 幂等、登录链接可点击。

### 修复

- **容器内 OAuth 交换 20405（Device proof required）**：无 Trae 客户端环境自生成合成设备凭证。
- WB 积分快照数据质量防护（异常余额跳变不污染趋势）；积分看板空态语义优化（冷启动返回 `status=empty` 替代 500，前端引导提示卡）。
- WorkBuddy OAuth 流程线程 panic 保护。
- Docker 构建：rust 基础镜像 1.85→1.88（修复依赖 MSVR 冲突致镜像构建 exit 101）。

---

## [1.0.0] · 2026-09-22 · Web 版首发

### 新功能

- **Web-only Docker 产品化首发**：桌面应用（Tauri）改造为 axum 单体服务 + 浏览器管理面，命令经 `POST /api/cmd/{name}` 白名单命令桥调用，实时事件走 WS 优先 / SSE 回退。
- **数据目录切换 `/data`**：容器数据目录默认 `/data/AIWorkAssistant`（`AIWORK_DATA_DIR` 可覆盖）；首启自动从旧平台目录一次性复制迁移。
- **CI/镜像发布流水线**：新增 `docker-image.yml`——rust/web 测试门禁 → buildx 推送 GHCR（`main`→latest、`docker_main`→分支标签、`v*` tag→semver tag）。
- **版本号单源同步**：`scripts/sync_version.mjs` 以 `crates/aiwork-core/Cargo.toml` 为单一来源，同步 Cargo 双包/lock/package.json/AGENT.md；`about.ts` 从 package.json 导入版本号消除硬编码漂移。
