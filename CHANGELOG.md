# 更新日志

本文件记录 AI Work Assistant（Web/Docker 镜像版）的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

> 分支说明：本分支（`docker_main`）为 Docker 镜像产品线（Web-only，axum 单体 + 浏览器管理面），与桌面版 `main` 分支不是同一产品形态；仅按提交语义移植系统无关的修复与功能。

---

## [1.4.5] · 2026-10-09 · 移植 WB 账号池互斥防丢失 + 上游空 message 兜底

> 合并点自 `main@5d0dd66` 推进至 `main@bc58506`（3 个提交，手工语义移植）。

### 修复加强

- **WB 账号池读-改-写互斥防 lost-update**（`6284ef3`）：`AppState` 新增 `wb_pool_lock`，全部运行期「load_pool→内存改→save_pool」写点（命令 10 处 + 每日签到 `sync_pool_expiry` + `mark_needs_relogin`）持锁执行，防命令/后台/调度线程用旧池副本整文档覆盖丢账号；额度巡检、凭证续期、套餐回填改**两阶段回写**——锁外网络调用，短临界区重读最新池按 id 字段级合并（窗口内删除的账号自动跳过、OAuth 已填套餐不被覆盖）。
- **空完成取证**（`6284ef3`）：Trae 对冲竞速捕获上游响应头摘要（logid / x-tt-logid 等 7 头，截断 96 字符），接管生效时取对冲侧摘要，空完成日志可溯源上游侧证据。
- **device_map 缺条目回落**（`6284ef3`）：Trae 调度取设备指纹时缺映射条目改按 uid 确定性派生 device_id，不再发送空 `x-device-id`（影子风控）。
- **上游错误空 message 统一兜底（issue #71）**（`67f9e5b`）：新增 `msg_or_fallback` / `stream_msg_or_fallback`，空 message 回填含状态码或错误码的诊断文案（网络波动/上游异常/全局代理劫持回环多因并列）；OpenAI/Anthropic inline 错误与 Trae/WB/Qoder 三个流式 `send_stream_error` 全部接入，消除 `{"message":""}` 透传旁路；业务码不冒充 HTTP 状态码、与 `error.code` 字段一致。

### 文档

- user-manual 补「客户端报 502 upstream_error（全局代理劫持回环流量）」FAQ（端口适配 Web 版网关 8080）。

### 发布前全面审查

- 逐文件 diff 与 main 语义比对 + 锁序专项 + 安全/兼容交叉复核。修复 1 项：FAQ 第三条「浏览器直开根路径看到 `api_key_required`」系 main 桌面版语义（网关独占端口），docker 单体根路径落入静态托管（SPA 登录页），已改用 `GET /v1/models`（不带 Key）的网关鉴权层探活口径。
- 锁序确认单向（`wb_pool_lock` → `WB_TOKEN_STORE_LOCK`）：`mark_needs_relogin` 持池锁调用 `reload_pools_after_change` 全链无重入池锁、无反向获取；`wb_pool_save` 全部调用点核查无锁外写点遗漏（migrate.rs:398 为启动期迁移，与 main 口径一致不加锁）。
- 验证阶段暴露 aiwork-server 3 处测试直构 `AppState` 缺 `wb_pool_lock`（E0063，admin_tokens/cmd_bridge/ip_allow），已补齐——再次印证 AppState 增字段必须 `cargo test --workspace` 全量编译。

### 跳过（桌面/OS 专属）

- doubao 双池锁全套（`doubao_pool_lock`、commands/doubao、tasks/doubao_quota、tasks/doubao_session、vault doubao 迁移段）、代理日志分片数值排序（misc 抓包 docker 已下线）。

### 验证

- `cargo test --workspace` 597 通过（含新增 3 项兜底单测）· `npx tsc --noEmit` 0 错误 · `npm test` 61 通过。

---

## [1.4.4] · 2026-10-08 · 移植 Qoder 签到健壮性四连修 + 网关调度五连修 + WB 积分并发/SWR

> 合并点自 `main@813db35` 推进至 `main@5d0dd66`（20 个提交，手工语义移植，4 个 merge / 2 个 docs 不计内容）。⚠️ 行为变更：Qoder 签到连续失败冷却改 30→60→120 分钟指数退避（原固定 30 分钟）；积分耗尽账号由 60s 软冷却改硬冷却至次日 04:00。

### 新功能

- **WB 积分取数并发化 + stale-while-revalidate**（`9194b4f` + `d4c8cd3`）：账号间 4 路并发（同账号内保持 summary → 包明细顺序），慢网络下全池刷新线性等待改并行；缓存过期时先回旧值（`refreshing:true`）并转后台单飞强刷（AtomicBool 防重入 + Drop 守卫防 panic 卡死），切板块不再等网络；概述页检测到 `refreshing` 后自动重取（最多 4 次、1.5s 起递增间隔）。docker 特有守卫保留：单账号查询不落每日快照。

### 修复加强

- **Qoder 签到健壮性四连修**（`641376b`）：每日 100 Credits 活动 campaignId 每日轮换，签到轮次自动从活动列表重学习当日候选（过期 id 同轮剔除、次日自愈）；409 活动过期专门识别归类「过期」不掩码同轮结果；claim 成功后回读余额核对，未增长如实降级 fail 杜绝「假成功」（回读失败不判负防误杀）；调度器失败冷却 30→60→120 分钟封顶指数退避（含 streak=0 移位溢出边界修复），失败提示改「稍后自动重试」。
- **网关调度五连修（issue #67）**（`36d628f`）：`insufficient credit`/积分不足/额度不足归类硬冷却至次日 04:00，不再被当频率限流反复轮换打满整池（4008 `quota exceeded` 有意保持软冷却防误杀）；流内收到硬冷却/会话失效/封禁错误且零内容输出时换号重试，不再把错误透传给用户；账号级长冷却解绑全部会话粘性（Trae/Buddy/Qoder 三池 16 处接线）；手动刷新积分后运行中网关池画像热回写，无需重启；Responses 代理 400+积分耗尽改切号而非终死。
- **Responses→Chat 投影工具结果修复**（`2c96e88` + `5d0dd66`）：工具结果按 `tool_calls` 顺序回填，乱序结果重排 + object 输出文本化 + 投影配对连续性规整。
- **「积分保鲜」预设描述更新**：明确「集中消耗临期积分包直至该账号耗尽后自动切换，非均匀轮换」。

### 跳过（桌面/OS 专属）

- 抓包日志条目索引翻页优化（misc 抓包 docker 已下线）、快照体积统计缓存与 fresh 参数（profiles 桌面专属）、Token 统计三路扫描优化与重复计算修复（workbuddy_stats）、env 探测 tasklist→sysinfo 改造与 pe_version 字节序修复（Windows 专用）、switcher 相关调整。

### 验证

- `cargo test -p aiwork-core --lib` 582 通过 · `npx tsc --noEmit` 0 错误 · `npm test` 61 通过。

---

## [1.4.3] · 2026-10-07 · 移植 main 侧边栏应用显示 + 看板联动 + 指纹清洗 UI + Qoder 签到风控对齐

> 合并点自 `main@9cdce69` 推进至 `main@813db35`（5 个提交，手工语义移植）。⚠️ 行为变更：Qoder「每日 100 Credits」已领判定从严；积分快照/用量同步默认改每小时。

### 移植内容

- **侧边栏应用显示可配置**（`52b5e38`）：固定应用单选 + 其余勾选隐藏；图标自选 48 个 Lucide 图标（恢复默认回退内置），`Settings.app_icons` 持久化 + 非法值双端清洗。
- **看板数据联动**（`b1531f5`）：积分快照/用量同步完成后经 `board-data-synced` 软通知看板页静默刷新（不进 loading、序号防竞态）。
- **指纹清洗 UI 化**（`b1531f5`）：WB 模板清洗规则升级为 API 管理弹框「指纹清洗」页签（内置默认只读回显，空数组恢复默认）。
- **多账号签到间隔可配**（`b1531f5`）：三平台签到账号间隔秒数（默认 3s，0=关闭，上限 600），配置页输入卡 + 调度器生效。
- **Qoder 签到风控对齐**（`448092a` + `813db35`，OS 依赖裁剪）：补 0.4.3 设备描述头（docker 走 hostname 链 fallback）；「每日 100 Credits」已领判定从严四条件；盲发兜底任一失败如实归 fail 交调度器重试（宁 fail 不假 already），失败提示可读化。
- **Trae JWT 刷新失效判定修复**（`cbd2c7e`）：火山信封错误判定死代码修复（`volcano_error()` 两段式解析），20101 立即置 invalid 停止变体探测。
- **WB 积分快照差分守卫**（`cbd2c7e`）：增删账号当日差分不可比时 earned 只取签到 reward、该日不计入消耗趋势。
- **三池耗尽归因徽标**（`b1531f5`）：健康徽标统一 `PoolHealthBadges`（禁用/硬耗尽/冷却/零积分/凭证过期），口径对齐池调度 selectable；三页接入。

### 跳过（桌面/OS 专属）

- runtime-info.exe 风控真值桥、CodeBuddy IDE 统计、doubao/device_proxy/workbuddy_stats/schtasks 批次。

### 发布前审查

- 两轮审查（逐文件 + 安全/适配交叉复核）无阻塞问题；修复 1 项 Minor：`wb_template_map_set` 补软上限（≤200 条/单条 ≤4KB）防网关自伤型性能面。

### CI 与构建

- **vault 单例串目录竞态根治**（`startup_migration_collects_plaintext_keys` Linux CI 三次偶发失败）：迁移链路吞错点补 stderr 诊断后真因确认为——vault 进程级单例被并行测试绑定到其他测试目录/已删目录，`ns_set` 快照落盘 Err 导致明文未占位化。改造为**按 conf_path 分桶**（`VAULTS: Vec<(PathBuf, Handle)>`，生产单目录行为不变，测试多目录各自持有独立 stronghold 实例，互不串扰）；`vault_test_guard` 保留兜底同目录复测。
- 清除 Cargo.toml BOM（版本升级时误写入，触发 rust-cache 解析 warning）与 `qoder_common.rs` 的 `unused_mut` 构建警告。

### 验证

- `cargo test --workspace` 578 通过 · `npx tsc --noEmit` 0 错误 · `npm test` 61 通过。

---

## [1.4.2] · 2026-10-06 · API 管理 Qoder 集成回溯补齐

> 回溯补齐 1.4.0 遗漏：Qoder 的 API 管理集成（调度策略中心 / 资源池摘要卡 / 混合白名单 UI）源自 main Qoder 早期 commit（`e5da2d1` / `0b615ce` / `23e7117` / `a2cf743`），早于 1.3.6 合并点 `1c29564`，移植 Qoder 全链路时未回溯到。合并点维持 `main@9cdce69` 不变。

### 修复

- **Qoder 池恒空根因（后端）**：`apply_pool_snapshot` 直传可能为空的 `qoder_enabled_uids`，白名单为空时 Qoder 池恒为空——支持模型 qoder 源恒「未启用」、调度配置「暂无上游账号候选」两症状同源。补 `effective_qoder_uids` fail-open 解析（显式白名单优先，空 = 全部含凭证账号自动入池，对齐 Buddy 池 `effective_wb_uids` 语义），含纯函数单测。
- **调度策略中心 Qoder 化**（移植 `23e7117` / `a2cf743` 语义）：组合预设新增 Qoder 池内策略维度（`matchPreset` 纳入 `qoder_strategy` 匹配，偏离即「自定义组合」）；池内调度补 Qoder 池下拉（空 = 跟随 Trae 池，同 Buddy 语义）；优先级序含 Qoder 池标签与「默认序尾部、可上移」说明；新增 **Qoder 资源池摘要卡**（上游开关徽标 / 池内与总积分「未知」口径 / 池内策略文案），布局改四池等宽。
- **API Keys 管理**（移植 `e5da2d1` 文案）：资源池选项改「全局 / Trae / Buddy / Qoder」（去池字）；绑定 Qoder 池且上游未启用时表格 amber「上游未启用」警告徽标 + 编辑弹框提示（上游原文）；Qoder 池徽标 tone 改 violet（原 amber 与警告徽标撞色）；候选白名单混合编码注释补 `qoder:` 前缀。
- **文案与聚合补齐**：使用帮助「双源模型」改「多源同名模型（Trae/Buddy/Qoder）」+ Qoder 档位透传说明 + 池标签加 Qoder；用量统计改四桶聚合（新增 Qoder 独立筛选，`api_qoder_usage_stats`，全部 = 四桶相加）；自定义模型回落文案统一改 Trae/Buddy/Qoder（头注释 / 副标题 / 空态 / 禁用 title / 启用 label / 删除确认 6 处）；接口配置 PoolBadges 加 Qoder 徽标 + 统一目录说明含 Qoder。

### 验证

- `cargo test --workspace` 553 通过（含新增 fail-open 单测）· `npm test` 61 通过（dispatchPresets / poolMetrics 单测扩至 Qoder 维度）· `npx tsc --noEmit` 0 错误。

---

## [1.4.1] · 2026-10-05 · 资源调度 per-pool 拆分 + API 管理 Qoder 池绑定 + Web 化体验修复

> 本轮无 main 移植内容（Qoder 池绑定等在 main/feat-qoder 均不存在，为 docker 分支三池结构下的自研对齐）；合并点维持 `main@9cdce69` 不变。

### 新增

- **资源调度参数 per-pool 拆分**（1.4.0 跳过项，本轮三池结构下完整落地）：`api_pool` 共享字段退役，拆分为 Trae / Buddy / Qoder 三池独立参数组（账号并发上限、池粘性 TTL、会话粘性 TTL、竞速对冲阈值）；读取侧统一走 `load_pool_file_with_legacy_migration`——仅 Buddy 池沿用旧共享值（缺失回填、下次保存固化、幂等），Trae / Qoder 池落默认值。Trae 池补齐启用开关、竞速对冲（先出首字者胜）、显式会话粘性；Qoder 会话粘性 TTL 改可配；前端三页 per-pool 参数面板（保存即热生效）。
- **API 管理 · Qoder 池绑定**（issue #25/#30）：子 Key 资源池绑定新增「Qoder 池」选项（单选切换清空跨池勾选；绑定 Qoder 但上游未启用时弹框告警回退）；混合白名单编码扩展 `qoder:` 前缀（后端 1.4.0 已就绪，本轮补前端）。
- **Buddy 环境配置 · 本机 auth 文件路径**：`Settings.wb_auth_file_path` 暴露到环境配置页——Docker/远端部署将宿主机客户端 auth 文件挂载进容器后填写容器内路径，「扫描本机账号」从该文件读取；扫描零凭证提示同步引导。

### 修复

- **Qoder OAuth 登录无法自动打开浏览器**：命令同步返回授权页 URL，前端在点击手势的 transient activation 窗口内 `window.open`；被拦截时 toast 提示并回退弹框内可点击链接。
- **会话粘性 TTL 0 值语义陷阱**：后端 0 按「未设置」回退默认 1800s，而 UI 统称「0 = 关闭」——行为与预期相反。三页会话粘性 TTL 最小值改 60（存量 0 下次保存固化），desc 注明「最小 60 秒，无独立关闭开关」；Buddy 页调度参数统称对齐 Trae/Qoder 页。
- **杂项**：Qoder 凭证定时刷新误渲染时间输入框（改固定周期徽标）；Buddy 概述移除 Web 版无数据源的「登录账号/本机套餐」区块（5 → 3 列）；BuddySettings 全局设置调用路径修正（`api.misc.settingsGet/Set`）。

### 验证

- `cargo test --workspace` 552 通过（540 + 12）· `npm test` 57 通过 · `npx tsc --noEmit` 0 错误。

---

## [1.4.0] · 2026-10-05 · 移植 main Qoder 平台全链路 + 系统无关修复

> **合并点记录**：移植范围 `main@1c29564`（**不含**）至 `main@c3f3211`，另含 `main@9cdce69`（Qoder 签到兜底直领）。**下次合并请从 `9cdce69` 之后接着移植**。手工语义移植、未经 merge。

### 新增

- **Qoder 平台全链路**：协议层（积分/签到/用量/模型目录）+ 调度器与网关池接入 + 3 命令（`qoder_pool_status` / `api_qoder_usage_stats` / `api_qoder_catalog_sync`）；前端 `src/pages/qoder/*` 六页；Dashboard 三平台化（KPI、快照差分曲线/热力图、Token 用量、到期日历按平台口径适配）。

### 修复（移植）

- **Qoder 每日签到误报「无可领活动」**（`main@9cdce69`）：列表零 CLAIMABLE 时对已知每日活动盲发直领兜底（严格判定 `200+CLAIMED+!replayed`，401 走自愈重试）；campaignId 强制路径安全白名单。
- **签到档期日历双修**：同日同账号多轮记录按最终态去重；BuddyCheckin 接入活动档期日历。
- **模型目录**：Max Mode 角标 + 厂商列；积分明细包数口径与 KPI 对齐。
- **审计补移植**：WB chat `prompt_cache_key` 注入（`8838e85`）；6004 配额三态多锚点（`ab67a60`）；auth 文件提取分类报错（`406c50b`）；凭证 vault 收敛改道/刷新失败四分类/SSRF WHATWG 解析（`4f1174b`）；到期日历「长期有效」哨兵（`34e1757`）；Buddy 模型厂商列（`59a594d`）。

### 安全与健壮性（发布前审查 19 项全修）

- **Critical**：WB 对冲账号按自身 uid 重建 body（防跨账号前缀缓存泄露对话）；WB/Qoder sticky 绑定命名空间隔离（防互删）；WB 账号入池改走 vault secure 读。
- **Major**：SSRF 加固（尾点/IPv6 兼容段/NAT64/CGNAT）；wb_tokens 存量明文启动收敛；429/408 不再误标永久态；OAuth 事件桥改 web `listen`；撤除 Qoder 资源调度页假保存参数。
- **Minor 11 项**：冷却多锚点取最大、热路径零克隆、sticky seed 限长、vault 值优先、ns 键校验、导出防双击、重登录清态等。

### 构建修复

- **CI Linux 构建缺依赖**：`[target.'cfg(windows)'.dependencies]`（windows-sys）误插依赖清单中间，其后 13 个跨平台依赖全部误入 Windows 专属段——CI Ubuntu 上 87 个编译错误。Windows 专属段移至文件末尾。
- **qoder_sign 测试平台硬编码**：`Cosy-Machineos` 断言改平台自适应（等于 `machine_os()` 输出），Linux CI 通过。
- **Docker 基础镜像 MSRV**：新增 `uuid` 依赖要求 rustc ≥1.89，`rust:1.88-slim` 构建 exit 101——基础镜像升级 `rust:1.97-slim`，成员 crate `rust-version` 声明 1.89（真实 MSRV），AGENT.md / docs/tech-framework.md 同步。

### 明确跳过（桌面客户端专属）

- Trae 包级积分口径/tokenStats 懒加载；BuddyAccounts 桌面环境操作（凭证导出除外，已带 Modal 强确认）；DiscoverModal 徽标；per-pool 字段拆分（1.4.1 已落地）。

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
