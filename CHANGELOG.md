# 更新日志

本文件记录 AI Work 助手（ai-work-assistant，原 Trae Work Assistant）的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

---

## [3.6.3] · 2026-09-26 · 积分看板重建（Trae/Buddy 平台拆分）+ Issue #38 `-max` 修复批

> 范围：自 [3.6.2]（tag v3.6.2，commit 89b0b6b）以来的全部变更。

### 新功能

- **[P1] 积分看板重建——Trae/Buddy 平台拆分 + 三源数据矩阵（credits-dashboard-plan.md）**：同一看板组件按 platform 参数渲染两个独立页面（Trae `credits` 视图 / Buddy `buddy-credits` 视图，互不混装），旧 Credits / BuddyCredits / TokenStatsPanel 三页退役删除；统计面板 KPI 7 卡（账号数/可用积分/平均/积分包/今日新增/今日消耗/7 天内到期，单平台口径）+ 积分统计 Tab（官网/本地/API 网关三源切换——网关源 `api_usage` 仅记 tokens，**决策落地：以请求数为主要口径、积分数不估算**）+ Token 统计 Tab 三源完整（本地 JSONL 365 天 / 网关 Trae/Buddy 池 90 天 / 官网 Trae token 字段，issue #35 Bug2 用户可见修复）+ 积分到期 Tab（双平台账号明细 + 到期日历）；布局两层分类（Tab / 数据源面板外切换）与面板同宽弹性热力图；抽公共组件 useDateRange / ChartFilterBar / ActivityHeatmap / ModelRanking 与 BoardPoint 纯函数适配层。Rust 侧方案 B：`wb_credits_history` 每日快照新增 `earned`（当日余额差分与签到 reward 归并，schema 幂等补列免版本迁移），新增 `workbuddy_credits_history_list` 命令；Buddy「获得积分」快照 earned 优先、缺失日回退签到口径；覆盖窗口与口径差异（§8）页面内诚实标注，不做静默合并。

### 修复

- **[P1] `-max` 后缀请求上游 4001 并轮换打满账号池（Issue #38②）**：真机根因为 dispatch 剥离 `-max`/`-thinking` 后仅路由与日志使用基名，请求体 `model` 字段原样透传 → payload 按未收录名生成 `xxx-max__dev` → 上游 `4001 param is invalid`；Trae 出站注入层现回写 body `model` 为剥离后基名（Buddy 管线同语义）。配套：Max Mode 注入值改布尔 `true`（真机实证 `is_max_mode:true` 可用）；全局模型白名单准入对齐后缀剥离规则（基名在名单即放行，白名单部署下 `-max` 入口不再 404）；流内请求级错误（4001 等 `ErrKind::None`）终止账号轮换、按 400 透传上游错误且不冷却（不再无意义打满整池）。
- **[P1] `/v1/models` 漏序列化 `max_mode` 字段（Issue #38①）**：内部 `UnifiedModel.max_mode` 已按支持表计算，手工 JSON 序列化遗漏致 release notes 声明的字段从未出现在响应中，Max Mode 对客户端不可发现；现补齐输出。
- **[P2] WB 池未启用时双源模型 `context_length` 被残留快照拖低（Issue #38④）**：双源合并原对 Buddy 侧上下文无条件取 min（`glm-5.2` 等被压到 128K）；现 `context_length` 纳入聚合末段按调度策略命中侧选定（与 rate/display 同模式），禁用池数值不再约束声明。
- **[P2] `prompt_max_tokens` 口径固化**：真机实证矩阵（省略 → 4001 上游必填；放大 1M → 4001 拒绝；`is_max_mode:true` + 168000 → 200），固定 168000；长上下文用例实测 `-max` 通路 180051 prompt_tokens（>168000）HTTP 200，确认 1M 窗口实际兑现、该字段不钳制 Max Mode 上下文。

### 可观测性

- **[P3] Trae 出站链路落盘（Issue #38⑤）**：app.log 新增 `trae effort: requested → wire`（档位映射）与 `trae outbound: requested_model/model/effort_injected/max_mode_injected`（后缀剥离回写与 Max Mode 注入），三类失败分支（未剥离/剥离未注入/注入被拒）可区分。

### 测试

- cargo 单测 **559** 全绿（新增/更新：context 命中侧选定、`-max` 白名单放行与 body model 回写、Max Mode 注入布尔值、`prompt_max_tokens` 恒定等）；真机端到端验证 `-max` 全链路 200、账号池零冷却。

---

## [3.6.2] · 2026-09-25 · 模型档位统一空间 + Max Mode 出站 + 局域网网关接入 + 定时同步扩展

> 范围：自 [3.6.1]（commit df8010e）以来的全部变更。

### 功能优化

- **[P1] 模型档位统一空间与 Trae Max Mode 出站接线（Issue #31）**：新增统一档位空间（minimal..max）与 Trae wire（light/high/extra_high）映射，Trae 档位停用名称推断改静态实证表（调用时降级兼容 ≤请求值的最大支持档位）；Max Mode 出站接线，单次 parse 合并注入 `reasoning_effort_level` 与 `is_max_mode`，`-max` 后缀路由剥离并补观测日志；effort 三源合成（显式 > 路由提示 > 默认 high）经降级链出站注入，空串短路实现显式关闭；档位声明双源合并（Buddy 空时保留 Trae 侧），`/v1/models` 上下文改实际请求口径（1M 声明不冒充，仅 Max Mode 可达）并新增 `max_mode` 字段；`wb_rate` 原始值透传（0 = 免费声明，修复 Buddy 免费模型被当未声明退源）；Trae 表外模型显式请求档位按统一→Trae 映射填充默认下发（合成默认仍不下发）；L2 档位归一为空回退 L3 实证表；Anthropic thinking 参数接入显式检测。
- **[P1] 网关监听 0.0.0.0 并展示局域网接入地址（Issue #34）**：监听地址 127.0.0.1 → 0.0.0.0，局域网设备可经内网 IP 访问网关；新增 `lan_iface_ips` 命令（if-addrs 枚举 IPv4 单播，排除回环/链路本地/Docker 与虚拟化/代理虚拟网卡，按 IP 去重保序）；接口配置/使用帮助展示局域网接入地址与安全提示（无启用 Key 匿名放行），复制配置示例与概览行同步追加局域网 IP。
- **[P1] 积分看板与官网模型定时同步（Trae/Buddy 双平台）**：调度器新增 4 个可配置任务（每日 HH:MM/每小时/关闭三态）——Buddy 积分快照（积分 + Token 统计重扫 + 官网用量刷新，默认 23:30）与 Trae 积分同步（默认 23:40）改造、Buddy 上游模型目录同步（05:45）与 Trae 官网模型列表同步（05:40）新增；hourly 按成功 last_run_ts 节流 ≥1h，无账号静默跳过不计失败；两平台接口配置页模型目录卡均新增定时同步行。
- **[P2] 签到时刻可配置（Trae/Buddy）**：Trae 默认 09:00、Buddy 默认 09:10（零值自动回填），调度器 effective_hhmm 接线并与 Windows 计划任务注册时间共用同一设置值；环境配置页新增时刻选择器，配置非法自动回退默认。
- **[P3] 档位/Max Mode 帮助展示**：使用帮助弹窗新增档位/Max Mode 说明与七列表格（table-fixed 定宽 + 列宽微调），接口配置页加简版说明，用户手册新增 §7.6 局域网访问与 §7.6 档位说明。

### 修复

- **[P1] 设备标识重置失败（Issue #33）**：注册表 MachineGuid 两处写入改 `create()`（原 `open()` 仅 KEY_READ 只读句柄，set_string 必然拒绝访问，管理员权限下也一样，该层此前从未成功过），错误信息透出真实原因不再一律归因权限；aha/TinyStorage 兼容单文件形态（对文件 read_dir 报 os error 267，先判 is_file 且含 device_id 标记才删），子目录遍历失败最佳努力跳过不中断清理。
- **[P2] WB 每日签到状态解析兼容 schtasks 列序差异**：部分 Win11 无 HostName 列导致按固定下标取到日期、状态永远「未注册」，改为扫描带 `\` 前缀字段（抽出纯函数 + 单测）；WB 每日签到/每周续期注册与卸载补 app_log；任务配置卡双机制说明文案对齐（「Windows 计划任务（兜底）」）。
- **[P2] 运行日志双栏 1:2 布局去 md: 断点全尺寸生效**：高 DPI 小窗下视口低于 768px 不再退化为单列堆叠压扁实时代理输出。

### 其他

- AGENT.md §11.1 版本号升级规则补全双平台发版产物清单与 latest.json 全资产收录红线（v3.6.1 mac 更新阻断事故复盘）。
- wb-catalog-sync 路径去冗余：调度分支与手动命令共用同一 impl，消除 wb_upstream_accounts 双读；定时配置保存补最小过渡反馈（withMinDelay 400ms）。

### 测试

- cargo 单测 **557** 全绿零 warning（新增：effort 统一映射与降级链、局域网 IP 过滤、schtasks 列序解析、TinyStorage 单文件形态等）；vitest 32 全绿。

---

## [3.6.1] · 2026-09-23 · 调度可观测性增强 + 通知渠道统一 + JWT 定时续期

> 范围：自 [3.6.0]（commit aca5ead）以来的全部变更。

### 修复

- **[P1] 调度失败不落 API 请求日志（Issue #29）**：统一调度分流点三类失败（WB 上游未启用 400 / 模型冷却 429 / 无健康账号 503）现均落「API 请求日志」（uid/acct 显示 `-`，error 携带失败原因），消除「请求失败但日志页无记录」的可观测性盲区；NoHealthy 错误消息携带 `pool/healthy/key/bind/whitelist/dedicated/healthy_in_scope` 自查细节，`healthy_in_scope=0` 直接暴露 Key 约束排除根因。
- **[P1] 积分保鲜调度到期口径（Issue #28）**：Trae 池调度改看「通用积分」最早到期（Work 包不参与调度扣费），Buddy 积分包最早到期接入池调度；混合到期口径保留给 UI 到期徽章/日历与签到排序。
- **[P2] 用量统计 5 个指标块换行**：总请求数/成功率/Token 消耗/平均耗时/TTFT 单行铺开（sm 5 列 + 收紧间距），窄屏仍 2 列。

### 功能优化

- **[P1] 失败通知渠道迁移系统设置（Trae/Buddy 全平台共用）**：系统设置新增「通知渠道」面板（总开关/签到完成通知/调度任务失败通知三开关 + Bark/Server 酱/通用 Webhook 地址，右侧「发送测试」按钮），独立保存不受外观表单影响；环境配置页同步重组为「通用配置｜任务配置」两列 + CLI 自动轮换卡。
- **[P1] Trae JWT Token 定时续期与 401 自愈（Issue #27）**：新增 trae-jwt-renew 调度任务（默认 05:30，时刻可改、可启停、「恢复推荐配置」一键还原），请求遇 401 自动触发续期自愈，配置入口 Trae 环境配置「任务配置」面板。
- **[P1] 全局调度混合白名单（Issue #30）**：Key 未绑定池时白名单条目支持 `trae:`/`buddy:` 池前缀，按前缀分池作用域（某池零条目 = 该池排除，预检判不健康走 fallback）；前端 Key 编辑合并展示两池候选并带池徽标，专一下拉带 `[Trae]`/`[Buddy]` 前缀；调度日志新增 `key=`/`acct=` 字段（Key 展示名与取号账号名）。
- **[P2] Buddy 成长任务调度化**：wb-growth 每日定时执行，与签到任务共用时刻配置（默认 09:10）。

### CI

- Windows 构建产出三资源（NSIS setup + MSI + portable zip）对齐 Release 资产格式；portable 打包两处修复（显式用 System32 bsdtar、临时文件名 argv 全 ASCII），规避 runner ACP 1252 下中文文件名乱码。

### 测试

- cargo 单测 **527** 全绿零 warning（新增：调度失败日志落盘直测、NoHealthy 约束详情、混合白名单 pool_constraints、积分到期口径拆分、通知渠道迁移等 30+ 项回归）；vitest 32 全绿。

---

## [3.6.0] · 2026-09-23 · Key 级资源池绑定 + 全局模型白名单

> 范围：自 [3.5.8]（commit a970fc3）以来的全部变更。

### 新增

- **[P1] Key 级资源池绑定（Issue #25）**：API Key 调度配置支持绑定 trae/buddy 单池，调度预检、取号、跨池回退全链路感知绑定——绑定池不健康时走全局回退开关回退另一池，不再等取号失败才暴露；向后兼容（未绑定 = 跟随全局调度，零行为差异）。
- **[P1] 全局模型白名单（Issue #26）**：三层管控——`/v1/models` 目录过滤、推理端点 404 准入（OpenAI `model_not_found` / Anthropic `not_found_error`）、后台任务降级候选与白名单求交（交集为空跳过降级）；白名单为空 = 不限（默认行为零变化）；配置入口系统设置。

### 修复

- **[P1] 签到「今日已签」误报导致无法自动签到（Issue #24）**：判定收紧为响应 `ok==true` 且按 `user_id` 匹配，异构成功响应不再被误判已签而跳过签到。

---

## [3.5.8] · 2026-09-20 · Buddy 账号分组 + 会话域内联 + 添加账号流程帮助

> 范围：自 [3.5.7]（commit 1c89568）以来的全部变更。

### 功能优化

- **[P1] Buddy 账号分组**：新增分组管理（分组定义存 kv，成员挂账号 group_id）、分组过滤 chips、账号行分组列；资源调度账号池支持分组筛选（`wb_group_ids`，池装配层过滤），分组外账号半透明标记，池内指标按筛选后计数。
- **[P2] WB/CB 会话域选择内联**：会话 tab 撤销，域选择内联进备份/恢复/复制弹窗；Buddy 账号管理按钮改名重排（OAuth登录/扫描本机账号/导出账号/导入账号/环境重置/分组管理/快照管理）。
- **[P2] 添加账号流程帮助**：Trae/Buddy 账号管理使用帮助置顶「添加账号流程（三步）」：添加账号（OAuth/扫描/代理/手动）→ 登录客户端 → 保存登录会话；Buddy 新增使用帮助入口 + BuddyHelpModal（OAuth 扫码/导入本机/双端登录态保存与切换/凭证续期/CLI 桥接/会话备份恢复复制/快照管理/环境重置），内容按功能实际口径编写。
- **[P3] Trae 文案对齐**：「扫描本机」→「扫描本机账号」并与 OAuth 登录换位、「导出账户」→「导出账号」；同步两处帮助弹框、签到/概览/引导文案旧称谓；Trae 帮助 OAuth 小节降为普通样式避免双高亮。

---

## [3.5.7] · 2026-09-20 · CC Switch 注册链路修复 + 网关协议对齐 + 多账号互踢优化

> 范围：自 [3.5.6]（commit a15e822）以来的全部变更。

### 修复

- **[P1] CC Switch Claude 条目注册后完全失效**：settings_config 由扁平结构改为官方 `{"env": {...}}` 嵌套结构（官网文档 + cc-switch main provider.rs + 本机真实条目三重实证）——此前切换后 Claude Code settings.json 顶层无 env 键，端点/凭据全部丢失。
- **[P1] CC Switch Codex 条目启动即报 "Model provider `custom` not found"（Issue #20）**：config.toml 内 provider id 固定为 CC Switch 常量 `custom`（其切换逻辑按常量改写 model_provider，自定义 id 会「键在表不在」）；TOML 插值补 basic string 转义（引号/反斜杠/控制字符）。
- **[P1] API 网关请求体超限行为（Issue #21）**：axum `Bytes` 提取器默认 2MiB 限制，超限请求在进 handler 前即被纯文本 413 拒绝（handler 内 8MB 检查不可达）——显式放开 DefaultBodyLimit 至 32MiB（适配长上下文客户端每轮重发完整历史 + 图片 base64），鉴权中间件前置 Content-Length 预检，超限返回结构化 JSON 413，阈值/文案由常量统一推导。
- **[P1] Anthropic 协议 stop_reason 硬编码 end_turn**：流式 message_delta 与非流式 /v1/messages 均按上游 finish_reason 映射（length→max_tokens / tool_calls→tool_use / content_filter→refusal），存在 tool_use 块时强制 tool_use——修复工具调用客户端把「等工具结果」误判为「正常结束」。
- **[P1] SSE keep-alive 拖延流终结（WB/Custom 路径）**：AtomicBool 15s 轮询改 watch + DoneSignal（主任务结束 Drop 即触发，含 panic 展开），流终结不再被拖延最多一个 15s 周期，与 Trae solo 路径同方案。
- **[P1] CA 证书读取拒绝访问 os error 5（Issue #14）**：历史 harden_ca_dir 授权无 (OI)(CI) 继承标志，目录收紧时子文件 DACL 被动态清空；加载分支探测不可读即 `icacls /reset /T` 自愈重试，仍失败报「删 certs 目录重新生成」指引（cert_install 与代理启动同链路受益）。
- **[P2] CC Switch 注册复用业务 Key 被每日配额 429**：未显式传 Key 时自动使用「CC Switch 专用」Key（复用或新建，不限每日配额，被禁用自动恢复启用），与业务 Key 隔离；注册成功文案附 Key 来源说明。
- **[P2] CC Switch 数据库写入健壮性**：连接设 busy_timeout 3s（运行中持写锁不再立即报 database is locked）；providers 列级兼容（SCHEMA_VERSION 19 基线无 cost_multiplier 列时按实际列动态拼装 INSERT）；备份只保留最近 10 份本工具创建的备份（.bak_aiwork_ 标记，不触碰 CC Switch 自身备份）；主目录定位失败不再兜底 "." 误报 cwd 下数据库。
- **[P2] 非流式聚合/工具代执行占用主阻塞池**：两者含分级重试（退避最长 60s×N），迁入 stream_runtime 专用阻塞池，不再饿死鉴权等短任务（与流式线程隔离同策略）。
- **[P2] Trae 池空警告误导排查方向**：Trae 池空 ≠ 全部资源不可用——启动日志与系统通知分池展示（Trae 池 accounts/healthy、Buddy 池 enabled/accounts/healthy），分别指明哪个池不可用、去哪里处理。
- **[P2] 上游采样参数丢失**：Trae solo 请求 max_tokens 尊重客户端显式值（含 max_completion_tokens，缺省兜底 4096）；Anthropic→OpenAI 转换透传 temperature / top_p / stop_sequences→stop；tool_result is_error 以「Error:」前缀标注，保证模型可感知工具执行失败。
- **[P2] 豆包保活 TOCTOU 与 PID 复用误杀**：记录 spawn PID，stop 前重检存活 + 映像名匹配后按 PID 精确关闭，消除 8s 窗口竞态。

### 功能优化

- **[P1] 多账号互踢冲突面优化**：Trae refresh_jwt 惰性刷新门（JWT/refresh_token 剩余 >48h 跳过 ExchangeToken）+ invalid 时从 IDE 本地登录态自动恢复；Buddy workbuddy_refresh_token 惰性续期门（access_token 剩余 >24h 跳过上游取号，expires_at 缺失保守放行）；前端刷新支持 force 强制，惰性跳过走中性提示。
- **WB 指纹清洗扩展**：新增 Codex CLI v1–v4 harness 身份句与 TraeCode 身份句的最小改写（9/19 新版 codex.exe 实证 + pool=trae 日志实证预防性兜底）；清洗覆盖 tools[].function.description（CLI harness 将身份句嵌入工具描述的绕过通道）。
- **Buddy 资源调度页布局重组**：账号池选择与资源开关/调度参数合并为左列同一面板，右上角「保存」一次保存全部；右列模型目录；「池内账号」指标改为「勾选参与 WB 调度的账号数」。
- **资源总览三池指标口径升级**：新增池内成员数（Trae enabled_uids / Buddy 白名单或 fail-open 全量）与「可用积分」（池内账号积分和），与「积分总余额」（全部账号）区分；指标纯计算抽出 poolMetrics.ts 并配单测；生态接入卡展示 CC Switch 安装/条目注册状态，注册成功后即时刷新。

### 测试

- cargo 单测 446 → **453** 全绿（新增：Codex/Trae 身份句与工具描述指纹清洗 5 项、CC Switch 专用 Key/TOML 转义 2 项）；vitest 26 → **32**（poolMetrics 6 项）；`tsc --noEmit` 全绿。

---

## [3.5.6] · 2026-09-19 · API 池凭据/成员热重载 + Trae/Buddy 模型目录客户端对齐过滤

> 范围：自 [3.5.5]（commit 27c5e71）以来的全部变更。

### 修复

- **[P1] Trae「同步官网模型」401（存储 JWT 带前缀）**：自动捕获/本地抓取/刷新链路均以 `Cloud-IDE-JWT ` 前缀规范化写入，`fetch_official` 漏剥前缀 → mchost.guru 网关在 token 校验前统一拒绝（401 code 1001）。发送前剥前缀，对齐 pool.rs 入池行为（消融探针实证）。
- **[P1] Trae 同步目录混入客户端隐藏模型**：过滤逻辑对齐客户端模型选择器（抓包逆向四层判定）——内部配置 / 自定义回显（`custom_` 前缀等）/ `is_invisible_to_user` / 当代代际（`context_window_tokens.max ≥ 250_000`）逐层剔除，glm-5.1、kimi-k2.6、custom_claude-* 等不再出现；同步结果 16 条与客户端截图逐条零误差（含倍率）。请求形态换用客户端 Agent 聊天选择器真实形态（7 函数 + access_type=0 + solo_agent）；展示名/倍率/上下文取 solo_agent/chat_v3 主语境位；新增 DeepSeek-V4.1-Flash、Kimi-K2.8-Preview 路由映射。
- **[P1] Buddy 模型目录污染（agent/别名/内部条目）**：`wb_catalog` 三重过滤——`looks_like_model` 能力特征剔除无 models 子数组的 agent 条目（general-purpose/compact/plan 等）；噪音 id 黑名单（`auto` AutoMode 元模型 / `default` 路由别名 / `hunyuan-chat` 无定价内部条目）；`cli_agent_whitelist` 客户端菜单白名单逆向（tags 含 cli+default 的 agent models ∩ data.models），同名双档价位（hy3/hy3-x）保留双档。倍率解析对齐 `credits` 字符串链（"x0.05" / "x0.00 credits"）。

### 功能优化（API 服务热重载）

- **[P1] 凭据/成员变更联动热重载**：新增 `reload_pools_if_running` 全量重建运行中双池（Trae + WB），取代原 `note_refresh_success` 单点回填 JWT——覆盖其管不到的陈旧快照（SessionDead 禁用 / 冷却 / 积分 / 新账号缺失），修复「刚登录的 JWT 网关还是不行」。联动入口：OAuth 重登、refresh_token 刷新、手动更新 JWT、导入账号、保存账号池。
- **[P2] Buddy 资源开关热应用**：pool_set 保存时同步热应用 `wb_enabled` / `wb_default_thinking` / `wb_tool_exec` / `wb_bg_downgrade` 四开关（此前仅启动时读取，改动需重启服务）；前端「需重启 API 服务」提示文案全部移除。

### 测试

- cargo 单测 425 → **426** 全绿零 warning（新增：旧代模型/自定义回显过滤、主语境展示名优先等回归）。

---

## [3.5.5] · 2026-09-19 · 豆包登录误报修复 + Buddy 模型目录兼容 agents 新容器形态

> 范围：自 [3.5.4]（commit 0215498）以来的全部变更。

### 修复

- **[P1] 豆包「保存登录态失败」误报（Issue #15）**：`check_profile_login_cookie` 活跃 Profile 单一口径改为 `analyze_login_sessions` 多 Profile 聚合判定——任一 Profile 持有非游客登录会话即放行；存在读取失败（Cookies 锁/活跃 Profile 错位）时 fail-open，不再误报「未检测到登录会话」；确认未登录时的报错附带 Profile 扫描诊断；保存预检 / 切换守卫（保持 fail-closed）/ 快照槽恢复预检三个消费方口径统一；新增 6 个回归测试（多 Profile 聚合、锁读失败、游客态边界等）。
- **[P1] Buddy 模型目录拉取 0 模型（官网 2026-09 新容器形态）**：`wb_catalog` parse_upstream_catalog 重构三层策略——定向容器探测（兼容 `{code:0,data:{agents:[…]}}` 新形态，data 由数组变为对象）→ agent 内嵌 models 展开（无内嵌则 agent 条目本身作为候选）→ 全树深扫兜底（要求非空字符串显式 id），按 id 大小写不敏感去重。
- **[P2] models_sync 鉴权失败指引增强**：401/code 1001 自动附带凭据修复指引（续期 JWT / 重新 OAuth / 保存当前登录态），避免误判为同步功能故障；Buddy API 服务页卡片文案对齐「同步官网模型」。

### 测试

- cargo 单测 412 → **425** 全绿（本周期新增：豆包登录会话聚合判定 6 项回归、wb_catalog agents 新容器形态解析）；vitest 26/26、`tsc --noEmit` 全绿。

---

## [3.5.4] · 2026-09-16 · API 网关性能优化 + refresh_token 刷新链路修复

> 范围：自 [3.5.3]（commit 663b8a4）以来的全部变更。

### 性能优化（API 网关）

- **[P1] 日志异步化（B）**：api_logger 改内存队列 + 独立写线程（Condvar 唤醒 + Weak 引用防泄漏），请求路径不再同步写 app.log。
- **[P1] 配置内存缓存（A）**：新增 `config_cache.rs`（5s TTL + 写路径显式失效），dispatch_policy / wb_model_catalog / wb_model_route / api_models / api_gateway_settings / wb_sticky 六热点文件每请求读盘归零。
- **[P1] 记账削峰（C+E）**：用量与 API Key 状态改内存权威副本 + 脏标记，flusher 每 2s 批量落盘（按 (bucket, day) 去重，SQLite 事务从 O(请求数) → ≤3 次/2s）；Key 保存锁内合并计数（UI 编辑字段优先、记账字段以内存副本为准），消除 UI 保存回退当日计数与 save/flusher 锁外写库竞态；flush 写库失败恢复脏标记重试（替代静默吞错）；constraints_for 锁内直查去整表克隆。
- **[P1] 流式线程隔离（D-1）**：SSE 流式请求隔离到独立阻塞线程池（stream_runtime），不再耗尽主 worker 池。
- 测试缓存污染修复：直写 SQLite kv 的测试补 `config_cache::invalidate`。

### 修复

- **[P1] refresh_token 刷新链路（code=-1: 未知错误根因）**：刷新路径迁移固化协议——`exchange_token_refresh` 变体探测链（DeviceProof 签名 `POST\npath\nClientID\nRefreshToken\nts\nnonce` + `x-cloudide-token: ""` 空头 + `Result.Token` 响应），旧协议（cloudide 端点 + ClientSecret 体）降为兜底探测；判定收窄——仅服务端明确返回数字 `code != 0` 才标记 refresh_token 失效，无 code 字段的异构响应不再被 `unwrap_or(-1)` 误判（原实证 19s 内 4 连败 + 每次触发 vault 全量加密写盘）；已判失效账号入口直接拦截（不发网络请求）；失败后同账号 60s 短冷却（进程内 LazyLock，消除重试风暴与写放大）；各变体请求/响应脱敏记 app.log。
- **[P2] 复制/操作 toast 全线失效**：notify 脏值吞提示 + 剪贴板挂起兜底。
- **[P2] WB 模型目录拉取失败诊断**：HTTP 状态错误与 0 模型解析失败均附响应体前 200 字符摘要，区分鉴权拒绝与端点结构变更（此前仅报「4 种容器均不匹配」无从排查）。

---

## [3.5.3] · 2026-09-16 · OAuth 登录真实协议闭环（F-78 批次 3）+ 代理稳定性与网关白名单修复

> 范围：自 [3.5.2] 打包（commit 33a5a06）以来的全部变更。

### 新增

- **F-78 批次 3 OAuth 交换真实协议闭环（本版主线，9 个提交）**：以 Trae CN main.js 逆向实锤为最终裁决重写 AuthCode 交换链路——
  - **抓包固化基建**：登录参数对齐真实客户端 `native_ide` 形态；回调解析 `authCodeInfo`（JSON 形态，兼容旧 `refreshToken`/`code` 直传）+ PKCE（RFC 7636，S256 code_challenge，verifier 随登录生成随交换提交）；新增 `AIWORK_OAUTH_DEBUG_PROXY` 环境变量抓包调试路由（设为本软件 MITM 端口时 ExchangeToken/GetUserInfo 经代理并信任本地 CA，默认不设＝直连不变）；交换请求/响应全量脱敏打印 app.log（排查期）。
  - **设备凭证与签名（F-70 情报落地）**：新增 `icube_auth.rs`——icube tc 信封解密提取设备 P-256 私钥（签名私钥与 icube-dc 同源，device_id 首选 icube 凭证），ECDSA P-256 生成 DeviceProof（20405 实测要求 PascalCase 字段）。
  - **协议对齐实锤**：AuthCode 交换**不发 DeviceProof**（其属 refreshToken 刷新场景），主变体改发 `DeviceInfo{DeviceID,MachineID,PlatformCode,DevicePublicKey(SPKI PEM),...}` + IDEVersion；端点修正 `${host}/trae/api/v3/oauth/ExchangeToken`（host 取授权页回调参数）；DeviceCredential 补提 telemetry.machineId 与安装目录 version；Timestamp 必须为 JSON int（字符串被 schema 拒绝）；实测确认 AuthCode 一次业务级失败即失效（探测链仅首个变体有效），无 proof 变体自动回落兜底。
  - **探测链与诊断加固**：AuthCode/Code × ExchangeToken/GetToken 变体自动探测；响应解析对齐火山信封（ResponseMetadata.Error / Result）；token 提取全树深挖兜底 + 未知形态键路径诊断；4xx 响应体捕获 + x-device-id/x-app-id 设备头；调试代理 CA 缺失时降级直连不阻断登录闭环。
- **auth_saved_at 凭证落盘时间字段**：RawAccount/AccountView 补字段（OAuth 登录/导入/手动添加/刷新成功四处写入），前端徽标展示，对齐 Buddy 侧先例；`access_token_expires_at` 经评估不落盘（jwt_exp_timestamp 已实时解析）。

### 修复

- **[P1] 豆包客户端白屏/超时/退出卡死（代理稳定性，对齐 Python thread-per-connection 阻塞隔离语义）**：根因为凭证捕获/JWT 写库（SQLite + vault DPAPI）同步阻塞 tokio worker → 整个代理冻结（白屏 / forward 30s 超时 / 15 在途）。修复：写库移 `spawn_blocking` + ProxyLog/RequestLogger 改专用落盘线程；并发上限 128→512 + overload 日志 5s 节流（启动风暴拒绝→客户端重试恶性循环）；共享上游 hyper Client（跨连接复用连接池）；移除 ALPN 广播（对齐 Python ssl 默认）；退出 RunEvent::Exit 清理后 2s 强制退出（WebView2/runtime 析构挂死兜底）。
- **[P1] MITM 叶子证书独立 RSA 密钥**：原复用 CA 密钥被 ttnet 严格校验拒绝 → 每叶子证书独立密钥（对齐 Python 每证书独立密钥）；握手连续 3 次中止自适应锁定降级透明直通；OAuth 证书兼容。
- **[P1] 自适应锁定降级改失败率判定**：豆包预连接风暴（大量并发握手在途）被误判「锁定」降级 → Cookie 捕获停摆，改按失败率判定。
- **[P1] WB 池入池白名单独立化（Buddy 源恒 503 no_healthy_account 根因）**：wb_pool 误用 Trae 共享白名单（enabled_uids）过滤 wb- 账号且 UI 只能勾选 Trae 账号 → WB 池恒空，「API 使用帮助」运行时派生 buddy_ok=false 全部「未启用」。新增 `wb_enabled_uids` 独立白名单（空 = 全部含凭证账号自动入池 fail-open）；旧数据混存 wb- 条目保存时自动迁移归位；修复前端白名单值域错位（勾选/保存误用 a.uid 真实 uuid 而非 a.id 池键，显式名单永不匹配堵死 fail-open）与 inflight 显示同域错位；Buddy 页账号池卡升级为可勾选白名单（全选/清空/已选计数）。
- **[P1] 老版 Python CA（PKCS#1 RSA）加载全链路修复**：rcgen 默认 ring 后端 KeyPair 只认 PKCS#8，老版 Python CA 私钥报「Could not parse key pair」→ 启用 aws_lc_rs feature 支撑 PKCS#1/SEC1 解析；`load_issuer` 按 PEM 标签解析为原始格式 PrivateKeyDer 原样传 rustls（serialize_der 对非 PKCS#8 进出同格式，硬包 PrivatePkcs8KeyDer 会在 ServerConfig 构建时报错）。
- **[P1] CodeBuddy 切换假成功（会话回退旧账号）**：vscdb mtime 只证明「客户端动过登录库」，防不了启动后会话回退旧账号（实测恢复后 verify OK，45s 后 live 身份仍是旧账号）——确认收尾补信号④ `confirm_ok`：轮询 live storage.json genie.userId（3 次 × 2s），非目标非空 uid 判 Reverted 并如实告知；仅当槽位快照含 storage.json 才启用复核（槽位缺该文件时 live 是旧账号残留，判 Reverted 即假阳性）。
- **[P2] 证书提权安装被拒降级当前用户直装（issue #12）**：杀软/企业组策略/VPN 客户端锁 HKLM 根存储时 certutil 提权仍报 0x80070005——提权失败（非用户取消 UAC）降级 `certutil -user -addstore` 写 HKCU Root（无需管理员，Chrome/Edge 信任），两条路径都失败才报错，降级成败由根存储复查统一判定；`cert_status` HKLM/HKCU 任一命中即已安装；0x80070005 文案如实列出文件权限/杀软/策略/VPN 锁存储等根因。
- **[P2] WB 签到「获取积分」列修复**：双余额差值恒执行（不因单项缺失跳过）+ 已签状态回填今日奖励；奖励提取递归深挖奖励键 + WB credits 余额差值兜底。
- 豆包 check-login Cookies 锁占用重试 2 次后明确跳过（原静默无语义）。

### 变更

- **账号管理页**：账号池「保存」按钮移至卡片头部操作区（仅池非空时显示，长列表免滚动到底）；Trae 端入口 TRAE SOLO CN 调至首位。
- **WB 签到**：签到/成长轮次后端互斥（WB_ROUND_LOCK 共享一轮）+ 前端同步互斥禁用与 title 提示（防请求被后端拒绝的按钮闪烁）。
- `docs/backlog.md`：F-24-余（豆包会员额度端点）真机复验通过正式闭环（2026-09-16）；F-78 批次 3 收尾记录对齐。

### 测试

- cargo 单测 398 → **412** 全绿（本周期新增：PKCS#1 CA 加载→签发叶子→ServerConfig 全链路回归、WB 白名单 3 组、CodeBuddy 切换复核等）；vitest 26/26、`tsc --noEmit` 全绿。

---

## [3.5.2] · 2026-09-15 · 切换链路实测根因修复（端隔离 F2-4 / 守卫 F2-5 / 混合推导 F2-6）

> 范围：切换器全链路实测问题修复——「切换不生效 / 切 CodeBuddy 连带切 WorkBuddy / 切换后账号不变 / 徽标不更新」。

### 修复

- **[P1] 进程识别恒不命中 → stop_app 从不关客户端**（豆包/CodeBuddy/TRAE 全线「切换不生效」根因）：sysinfo 在 Windows 返回的映像名带 `.exe` 后缀（如 `Doubao.exe`），与白名单不带后缀形态精确比较恒 false——快照在运行中被覆盖 + 启动变多开。`proc::name_matches` 匹配前统一剥离 `.exe`（大小写不敏感），附回归测试。
- **[P1] icube 恢复残留 WAL 回放复活旧账号**（Trae「切换后账号不变/本机识别错乱」根因）：强杀后现场残留 `state.vscdb-wal/-shm`，客户端启动回放旧 WAL 把切换前账号写回恢复后的主库。恢复前先删边车；快照白名单补入 `-wal/-shm` 成对快照；恢复改对称语义（槽位没有的项删除现场残留，Live 恒等于槽位内容）。
- **[P1] Trae/TraeWork 优雅等待 3s → 8s**：3s 实测恒超时 → 每次切换都强杀（与豆包 8s 同理，落盘/退出需要时间），是 WAL 残留的上游诱因。
- **F2-4 端隔离：CodeBuddy 不再回写共享 auth 文件**：该文件是 WorkBuddy 专属登录驱动源，CodeBuddy 登录真源在自身 vscdb——切/存 CodeBuddy 回写共享 auth 会把 WorkBuddy 登录一并切走（实测「切 CodeBuddy 时 WorkBuddy 伴随切换」根因）。`restore_authfile` L1 按 app 跳过，`confirm_switch` 信号②（auth uid）仅 WorkBuddy 检查；旧版快照（无 vscdb）告警文案同步改为如实提示无登录数据可恢复。
- **F2-5 账号槽位防污染**：① `save_current_login` 新增保存守卫——校验客户端实际登录（WorkBuddy=auth 文件 uid 池反查；CodeBuddy=live `storage.json` `genie.userId` 池反查）与目标账号一致，不一致拒绝保存并给出指引（实测事故：CodeBuddy 槽位互相污染后怎么切都是同一账号）；② 账号列表 CodeBuddy 端「CB当前」徽标改以 genie.userId 实测为准（回退桥标记）——桥标记在客户端手动重登后失真；③ 切换守卫 `expected_uid` 的 CodeBuddy 分支同步改 genie 实测优先。
- **F2-6 当前登录 uid 混合推导**：切换器写 `current_account.txt` 时同步写 sidecar `current_account.meta.json`（switchedAtMs）；`trae_apps::current_cloud_uid_hybrid` 按「证据 vs 标记谁更新」判定——纯证据推导会被快照冻结的旧时间戳误导（实测指向一个月前历史账号，守卫恒跳过回写），纯标记在客户端手动重登后失真；发现/套餐页与切换守卫统一改混合推导。
- **前端可观测**：Buddy 账号页监听 `switch-done`/`save-login-done` 自动刷新徽标（后台长流程完成时列表不再停留旧态）；切换完成后刷新本机登录徽标（立即 + 8s 兜底，等客户端写入使用证据）；设置页定时签到文案如实区分内置调度器与计划任务兜底。
- switcher 两处 `run_action` 测试加 `test_io_lock` 串行（并行争抢 `action_gate` 致 steps[0] 越界的偶发红）。

### 测试

- cargo 单测 396 → **398**（`proc::name_matches` .exe 后缀回归、`trae_apps::marker_with_ts` 标记/sidecar/双端路由）；vitest 26/26、`tsc --noEmit`、`cargo check` 全绿零警告。

---

## [3.5.1] · 2026-09-15 · SQLite 存储迁移 + Python/PowerShell 全量 Rust 化 + F-68 / F-74 / F-76~F-78 落地

> 范围：自 [3.4.5]（commit 651b056）以来的全部变更。

### 新增

- **SQLite 存储层全量迁移（P1~P6）**：新增 `store/` 模块（`mod.rs`/`schema.rs`/`docs.rs`/`migrate.rs`）——单连接 `Mutex<Connection>`（WAL + busy_timeout=5000），kv 文档表 / 行文档表 / 列化流水表三组 DDL（`PRAGMA user_version` 版本化）；启动迁移器三态（导入成功→原 JSON 移 `backup/` + manifest 落盘 / 损坏→隔离 / 失败→原位重试），遗留根路径文件兜底且不覆盖正牌数据，全量导入零失败才置版本号。分批切换：
  - **P2 KV 配置组**：23 个 JSON 配置文档迁入 kv 表（app_settings、api_pool、dispatch_policy、api_models、wb_model_catalog、wb_model_route 等热路径），删除 mtime 解析缓存改单行 SELECT 直读；
  - **P3 实体/流水组**：账号池（vault 43 调用点，raw 保真保留扩展字段）、groups/冷却/积分流水（INSERT + 90 天裁剪）/签到摘要、WorkBuddy 池与 token store（单行 UPSERT）、豆包池与健康事件表（append + 裁剪）、api_keys（鉴权记账 KEYS_LOCK 保留）/api_usage/custom_models 全部入 SQLite；
  - **P6 流水型 KV 迁出为真表**：credits 快照 → `wb_credits_history`（date PK，365 天 DELETE 裁剪）、usage_history → `usage_history_days`（PK(uid,date)，**补齐原本无界增长、缺失的 365 天裁剪**）+ 元数据表、wb_sticky_sessions → `sticky_bindings`（落库前 evict_expired）；schema v1→v2 增量迁移；
  - **P5 明文凭据收敛验收**：专项测试锁定「迁移保真导入 → vault 收敛进 Stronghold → 库中占位化抹除」链路与 main.rs 启动顺序契约（store 迁移先于 vault 迁移，避免明文经备份文件回流）。
- **应用内定时调度器 `tasks/scheduler.rs`**：每日签到/巡检/续期到点执行（含启动补跑，失败 30 分钟冷却重试），新增 WB/Trae 积分余额每日快照任务补齐差分时序；状态落盘 `scheduler_state.json`，`scheduler_status` 命令可查。
- **F-76 慢请求竞速对冲（网关）**：`RaceOutcome<T>` 泛型化 + `HedgeLease` RAII（对冲侧 inflight 计数全路径配对释放），对冲阈值默认 8s 对齐运行时 clamp。
- **F-77 账号级并发感知调度（网关）**：per-account inflight 计数、busy 过滤与全忙降级、粘性让位语义（sticky_yield/sticky_fallback）+ busy_yield/busy_fallback 调度事件日志；`/status` 暴露 inflight，前端账号列表实时在途徽标。
- **F-78 Trae OAuth 授权闭环**：本机回环监听器（oauth_loopback）+ 系统代理豁免（含崩溃残留清理）+ code 交换分支；`client_secret` 外置 `conf/oauth_client.json`；refresh_token 生命周期（expires_at/失败计数/失效）联动池同步禁用与前端 RefreshTokenBadge 徽标；OAuth 回调页 HTML 转义修 XSS。
- **CLI `--task-run refresh-credits`**：免 GUI 刷新全部账号积分，按积分包 CycleStartTime 归日口径重算 credits_daily 快照（实测将 API 可见历史 earned 修正为积分包周期重置口径）。
- **F-68 Trae 项目列表/最近打开跨账号保留**：新增 `src-tauri/src/switcher/vscdb.rs`——切换恢复快照**前**抽出 `state.vscdb` 的两个全局键（`solo-lite.local-project-folders` 项目列表、`history.recentlyOpenedPathsList` 最近打开），恢复**后**按条目合并回写（快照内已有以快照为准，仅补入切换前多出的条目；数组按 id、entries 按 folderUri 去重，快照项在前；非 JSON 结构保守不改）；写前 `state.vscdb.f68.bak` 单代备份，失败自动回滚。`switcher/mod.rs::restore_profile` 仅在 icube 布局（TraeWork/Trae）且恢复成功时调用，进度流输出「项目列表/最近打开已跨账号保留（项目列表 +N / 最近打开 +N）」，合并失败仅 warn 不阻断切换。**账号分区键（`solo-lite:content-map:<uid>` 等）零改动**（跨账号合并会产生服务端归属校验失败的"幽灵会话"）。
- **F-74 Buddy 切换时自动迁移会话（B2 会话域扩展）**：`commands/workbuddy/chatdata.rs` 新增 `BuddyApp{WorkBuddy,CodeBuddy}`——数据目录参数化为 `~/.workbuddy` / `~/.codebuddy`，备份根分离为 `data/workbuddy_chats` / `data/codebuddy_chats`（WorkBuddy 沿用原名，存量备份零迁移）；`chatdata_backup/restore/info/copy` 四命令新增可选 `app` 参数（空/未知回落 WorkBuddy＝旧行为）；核心逻辑抽出为与 Tauri 无关的 `backup_chats`/`restore_chats`/`copy_chats` 纯函数；前端账号管理页新增「WB 会话 / CB 会话」会话域切换（切换即按域重取「已备份」徽标）。
- **F-74 Buddy 切换时自动迁移会话（B1 切换编排）**：新增设置项 `buddy_switch_migrate_chats`（**默认关**，设置页 Buddy 区「切换账号时自动迁移会话」卡片，勾选即存、失败原地回滚）。开启后 `switch_account` 的 WorkBuddy/CodeBuddy 分支会在桥的 **Stop→Restore→Start 之前**完成全部动作：① 判定当前账号（WorkBuddy 走共享 auth 文件反查，CodeBuddy 走桥 `current_account.txt` 标记优先——auth 文件会被 WorkBuddy 覆盖）→ 与目标相同或判定失败则跳过；② 备份当前账号三件套（失败仅告警并跳过迁移，**不阻断切换**）；③ `copy_chats(当前 → 目标)` 新 id 复制 + 云端映射注册。全程以 `switch-progress` 的 `stage=migrate` 行输出进度。

### 变更

- **【BREAKING】移除 Python 运行时依赖，全量 Rust 重写**：`tasks/`（wb_checkin / trae_checkin / doubao_quota / doubao_session / doubao_chats / ui_click 等 CLI 任务域）与 `device_proxy/`（hyper 自建 MITM：JWT 捕获、签到头改写、WS 帧解析、上游 VPN 透传、抓包脱敏日志）进程内承接原 src-python 全部能力，Python 100% 对齐补齐（`--capture-local` 本地 Cookies 解密兜底、明文上游路由、WS 握手/响应体超时、端口占用 PID 诊断、`AUTO_CAPTURE_JWT` 环境变量开关等）；审查修复 15 项（账号池回写保留全字段、并发 permit 持有至连接结束、WS 分帧累积/控制帧/帧脱敏、JWT+refresh 原子更新、Exit 还原 VPN 原值、WB 托盘/启动补签轮次锁等）。**打包产物不再携带 Python 运行时；计划任务直调主 exe（`--task-run <name>`），原 python 脚本 CLI 入口全部移除。**
- **trae-switch-bridge.ps1 全量 Rust 化**：原 PowerShell 1534 行 / 24 函数对译为 `switcher/` 模块（mod / profile / locate / proc / machine / copy / icube / chromium / authfile）——exe 六级发现（`lnk`/`windows-registry` crate 替代 COM 与 Get-ItemProperty）、进程三级关闭（EnumWindows WM_CLOSE → TerminateProcess，sysinfo 0.33 锁定版对齐 MSRV 1.85）、6 层设备标识重置、三布局快照管线（.bak 单代轮转、完整性四项校验、vscdb -wal/-shm 边车、mtime 锚点防假阳性）细节全保留；前端零改动（NDJSON 行与 `*-done` 事件逐字段兼容、stage 文案逐字保留）、快照数据零迁移；8 处 powershell 管道调用点收敛为进程内直调，豆包 keepalive 计划任务启动器改直调主 exe `--task-run doubao-keepalive`（旧 cmd 启动期原地迁移）。
- **积分「获得积分」归日口径重算**：原恒等式反推口径（earned = total − 昨日total + consumed）在积分包过期/消耗波动时虚增（实测昨日 +1300 失真）；改为「某日获得 = 该日新开积分包（entitlement_base_info.start_time 即 CycleStartTime）的 credits_limit 合计」（签到包与购买包均计，固定 UTC+8 归日，跨账号合并）；consumed 口径不变，API 可见范围内历史快照一并修正；移除 `CreditStats.today_non_checkin_earned` 失效口径。
- **Buddy 资源调度页布局调整**：资源开关与调度参数合并为单一面板共用「保存」按钮；账号池选择上移至模型目录（Buddy）之前。
- **积分看板增强**：Trae 积分到期日历移除 JWT token 条目、新增「剩余 X / 总 Y」展示（贯通 total_credits=积分包 credits_limit 合计）；Buddy 近 7 日积分消耗主数据源改为官方用量聚合（`workbuddy_usage_official_all`，31 天零填充 + 10 分钟缓存 + stale 回退），快照差分降级为回退；Buddy 积分包到期日历过滤剩余积分为 0 的包。
- `run_in_background` 新增 `pre` 前置作业参数（其余调用点传 `None`，行为不变）；切换守卫 `expected_uid` 语义未改动。
- `docs/backlog.md` v2.7：**W-01（Work 积分 209 接入 API 网关）标记 ❌ 已排除**——Trae 积分签到调整，前提与收益均不成立；条目与 §三 专题保留作技术留档，§四新增排除行、§五排序移除。F-68 / F-74 标记已完成。本周期 backlog 还登记 F-74 会话迁移（v2.1）、F-75 macOS 平台支持与 Windows 依赖分层迁移方案（v2.2）、F-76/F-77 网关优化；另完成 Python→Rust 迁移文档收尾（AGENT.md / tech-framework / user-manual 全量改 Rust tasks 表述）与五份主文档全面审校、完成计划归档。

### 修复

- **[P1] 证书安装 UAC 后闪退**（根因 icacls/NTFS 实验复现，对齐 3.4.5 用户反馈）：`harden_ca_dir` grant 不带 (OI)(CI) 继承标志 + `/inheritance:r` 致目录 DACL 清空（连属主都拒绝访问），certutil 提权也读不到 ca.cer → UAC 允许后控制台一闪而过、证书从未装上。修复：grant 加继承标志使收紧真正生效 + 双探针自验证（任一失败 `/reset /T` 回滚 fail-open）；`cert_install` 失败自愈（icacls /reset 后自动重试一次）；UAC 取消映射退出码 1223（原误报成功）+ 退出码翻译（0x80070005=权限不足）；`ensure_ca` 已存在分支缺 ca.cer 时从 ca.crt 补导出 DER；Dashboard 安装成功 toast 补 certmgr 搜索 `TraeDeviceProxyCA` 验证指引。
- **[P1] 一键签到结果展示「积分+0」**（移植 main 12d051e 语义至 Rust）：`trae_checkin.rs` 信封宽容解析（沿 data/result/resp/response/info 递归下钻限深 8 层）+ `parse_claim_reward` 多层信封内层优先、跳过 0 值占位字段（`credits:0` 不再提前命中错判 delta，仅接受正值）+ `as_int_tolerant` 宽容归一；`Checkin.tsx` 兜底：delta 未获取到时显示「已签 · 余额 N/已签到」，already 分支不再把余额当增量展示。
- **SQLite 迁移复审修复（P7 + 收尾）**：① accounts.user_id UNIQUE 防线——历史 JSON 遗留重复 uid 保序取首条，不再卡死启动迁移/静默清空账号表；② `Store::open` 损坏库自愈——健康探针失败后隔离主库/-wal/-shm 为 `*.corrupt-<ts>`（保留现场可人工抢救）+ app_log，重建空库不闪退；③ CLI 分支先执行 `migrate_on_startup`（否则升级后首次 GUI 前触发的计划任务对空库静默空转，且窗口内写入会被 GUI 首启迁移整表覆盖）；④ main.rs 迁移块前移至 settings/trim_logs 之前（否则升级首启读到全默认设置、日志按默认保留期误裁）；⑤ `import_accounts` 改 raw 保真导入（device_proxy 写入的 struct 外扩展字段如 refresh_token_updated_at 不再被 typed roundtrip 丢弃）+ 去重丢弃数写 app_log（静默丢账号可观测）；⑥ `wb_upstream::refresh_access_token` 死引用修复——统一走 store wb_tokens 表，网关 401 刷新不再错位到 data_dir 根路径的旧文件。
- **F-74 复审 2 处缺口**：BuddyAccounts 徽标刷新 `useCallback([])` 固定身份闭包捕获首渲染 chatApp 恒为 WorkBuddy——切域后手动刷新/删除/导入/快照均按过期域重拉徽标，改经 `chatAppRef` 读最新值；切换失败客户端不重启（迁移前置作业内先杀客户端，而「目标账号无快照」预检失败路径在 stop_app 之前返回且无 start_app）——前置 `buddy_target_slot_exists` 预检，无快照跳过迁移。

### 移除

- `src-python/`（全部运行时脚本与测试）、`python.rs` 子进程管线、打包链 `prepare_python_runtime` / `make_portable_zip`；`src-ps/`、`tests/ps/`（Pester 黑盒测试由 cargo test 承接）；`tauri.conf.json` 相关 resources 与 `state.rs::resolve_ps_dir`。

### 测试

- cargo 单测 311 → **396** 全绿（本周期累计新增：switcher 29、store 基础设施与迁移 10+、`switcher::vscdb` 7（TEXT/BLOB 读取、缺键整体补入、数组按 id 去重、entries 按 folderUri 去重、完全一致零写入、非 JSON 保守不改、文件缺失安全跳过）、`chatdata::f74_app_tests` 2（应用域解析宽容回落、两域目录与备份根互不干扰）、签到信封解析 3、证书 ACL 回归等）；vitest 26/26、`tsc --noEmit` 全绿；全周期 cargo build 零编译警告。

---

## [3.4.5] · feature/buddy 批次 5（生态吸收与网关增强，T5.2~T5.6/T5.8）

### 新增

- **四段模型路由管线（T5.2/F-61）**：新增 `api_server/wb_model_route.rs`（12 单测）——① 别名静态映射（`data/wb_model_route.json.aliases`，大小写不敏感）→ ② 用户自定义通配规则（`rules[].pattern`，`*`/`?` 通配零正则依赖）→ ③ 内置系列通配（claude-*/gemini-*→glm-5.3、gpt-*→deepseek-v4-pro、o1*/o3*/o4*→hy4）→ ④ 后缀检测（内置 `-thinking` 注入 effort=high + 自定义 `suffixes[]`）；每级命中即止，映射目标一律校验目录命中防打空；四端点（chat/completions/messages/responses）统一经 `resolve_wb_target` 解析，全未命中回落原名走 SOLO
- **reasoning_content 思考链透传 + 默认深度思考（T5.3/F-62）**：OpenAI 协议思考链天然透传（stream delta 整体转发 + aggregate 保留 reasoning_content，批次 2 已具备）；本批补齐 Anthropic 侧——流式 thinking block（thinking_delta/content_block_start/stop，先于文本块、次序异常兜底收口）+ 非流式 completion_to_anthropic 前置 thinking block；「默认深度思考」开关（`api_pool.json.wb_default_thinking`，默认关）：客户端未显式请求 effort 且无路由级提示时注入 high（Responses/Anthropic/OpenAI 三协议判定各自显式语义）
- **生图双端点投影（T5.4/F-63）**：新增 `api_server/wb_images.rs`（4 单测）+ `/v1/images/generations`（文生图）与 `/v1/images/edits`（图生图，JSON 变体：image 为 base64/data URL，OpenAI multipart 不接受——零新增依赖红线）——目录校验（模型存在 + supports_image + prompt/image 非空）→ 上游 `{chat_base}/v2/images/generations`（headers 三铁律，Accept 换 JSON）；**上游不支持明示 501 不静默**（404/not found/不支持 关键词判定）；响应宽容归一（data[].url / b64_json / image_url 三形态 → OpenAI images 格式）
- **网关工具代执行（T5.5/F-64）**：新增 `api_server/wb_toolexec.rs`（8 单测）+ `wb_route.wb_tool_exec_chat` 编排——/v1/responses 声明 `type:"web_search"` 且开关开启时代理侧注入 function 工具（web_search/open_url）+ system 提示，上游 function 调用 → 本地代执行（DuckDuckGo HTML lite 搜索 + 页面抓取剥 script/style 剥标签截断 4000 字，ureq 已有零新增依赖）→ tool 消息回喂循环（**上限 3 轮防积分失控**）→ 历史搜索轮以原生 `web_search_call` 输出项返回（stream 按 created→items→completed 合成 SSE，非流式 completion_to_responses 前置 ws 项）；仅代理注入的这两个工具会被代执行，客户端真实 function 照常透传
- **协议细节补强（T5.6/F-65）**：① 连续同角色消息自动合并（批次 2 wb_payload 已具备，本轮核对确认）；② 单端口三协议区分——/v1/chat/completions 收到 `anthropic-version` 头返回 400 明示改走 /v1/messages（防协议混投字段级静默错乱）；③ 后台任务降级——`wb_bg_downgrade` 开启时标题/摘要类短请求（max_tokens≤128 且全文≤512 字符，保守启发式）路由到目录最低倍率模型（`cheapest_catalog_model`）
- **本地 quota 端口发现兜底（T5.8/F-21）**：`wb_common.py` 新增 `discover_local_quota_services`/`local_quota_balance`——① 扫 `~/.workbuddy/*.port` 端口声明文件 → ② 固定候选端口（18789/11101/8890/8899）+ 有界端口段（18780-18795）探测 → ③ GET `/api/v1/quota` 按 remaining/credits/quota/balance 特征确认（urllib 单发 0.8s 超时，最坏 ~15s 有界）；credits_fetch 云端三件套+旧接口全失败后最后兜底（source=`local_quota`）

### 变更

- **WB 上游开关组 UI（T5.2/T5.3/T5.5/T5.6③ 配套）**：ApiService 池设置卡新增 4 开关（启用 WB 上游/默认深度思考/网关工具代执行/后台任务降级），`pool_set` 命令未传字段保留原值（serde default 兼容旧 api_pool.json）；wb_tool_exec 默认开
- **DSH provider 目录动态替换（T5.1/F-37）**：`wb_catalog.rs` 新增 `parse_upstream_catalog`（宽容解析：根数组/data/models 三容器形态，字段链逐级探测 id/display/contextLength/maxTokens/inputModalities→supports_image/supportedEfforts/rate，字符串数值宽容，产出 0 条不落盘）+ `fetch_and_replace`（GET `{chatBase}/console/enterprises/personal/models`，headers 三铁律 accept 换 JSON）；网关启动自动做一次（best effort，失败保持静态兜底）+ `api_wb_catalog_sync` 手动命令；`/v1/models` WB 条目透传 `supports_image`/`supported_efforts` 元数据
- **CC Switch 协同（T5.7/F-43）**：新增 `commands/ccswitch.rs`（3 单测）——不自建切换器，把网关端点作为 provider 条目 upsert 进 `~/.cc-switch/cc-switch.db`（固定 id `aiwork-gateway-<app_type>`）：claude=扁平 env（ANTHROPIC_BASE_URL 不带 /v1 + 模型映射）/ codex=auth+config.toml（wire_api=responses）；写前整库备份至 `~/.cc-switch/backups/`、只动自有条目、Key 不入日志；`ccswitch_status`/`ccswitch_register` 命令 + ApiService 生态接入区 UI（同步 WB 目录 / 注册 Claude / 注册 Codex）
- **测试基线**：cargo 108→**136** 单测全绿（wb_model_route 12 + wb_toolexec 8 + wb_images 4 + wb_catalog 3 + ccswitch 3 新增）；vitest 18/18、npm build、src-python py_compile 全绿

### 修复（批次 5 全面审查，commit 832704a）

- **[P2] 四段路由 `strip_suffix_ci` 字符边界**：字节切片 `&name[..len-suffix.len()]` 在自定义后缀含多字节字符且大小写转换改变字节长度时可能越过字符边界 panic（网络请求路径）；改为纯字符级切分并补多字节回归单测。
- **[P2] 工具代执行账号归因**：`wb_tool_exec_chat` 的 `record_usage`/请求日志 uid 由常量 `wb-toolexec` 改为真实成功账号 uid，按账号用量统计不再失真。
- **[P3] `is_background_task` 补检 `max_completion_tokens`**（OpenAI 新字段），后台任务降级覆盖更全。
- **[P3] `percent_decode` 冗余重复条件清理；`open_url` 死变量 `cleaned` 删除**（省一次整页 `to_lowercase` 分配）。

### 已审查通过项（全面审查，未发现问题的维度）

- **业务正确性**：四段路由每级命中即止 + 目录校验兜底、生图 501 红线明示、工具代执行 MAX_ROUNDS=3 积分上界、后台降级仅显式开关生效、CC Switch 只动自有条目 + 写前整库备份——均符合设计文档 F-61~F-65/F-21/F-37/F-43 验收口径。
- **安全**：pool_set 未传开关保留原值（默认值不回退）、CC Switch API Key 不入日志不回显、quota 兜底仅 127.0.0.1 探测 + 响应 64KB 上限 + 递归深度限制、`/v1/images` 走既有 Key 鉴权中间件。
- **异常健壮性**：目录动态替换产出 0 条不落盘（静态兜底永不被网络抖动清掉）、上游全形态错误映射（401 刷新重试/分级冷却/换号）、spawn_blocking 隔离全部阻塞网络调用。
- **性能**：wb_model_route 通配零正则依赖、DDG 解析字符串定位无 HTML 解析器分配放大。
- **审查后校验基线**：cargo **137/137**（+2 回归单测）、vitest 18/18、npm build、py_compile 全绿、零编译告警。

### UI 布局全面审查与美化（commit f48bd95）

- **Buddy 顶栏上下文修复**：TopBar 原仅双分支（doubao/Trae），Buddy 工作区误显示 Trae 安装状态与代理按钮——新增 `BuddyTopBar`（客户端安装/运行/登录态/账号池徽标 + 打开客户端），三应用各自上下文独立。
- **全局微交互统一**：`index.css` 新增 `.row-hover`/`.card-hover` 组件类；六个数据表行 hover 补齐（原仅 Logs 有，Accounts 主表/Credits/DoubaoAccounts/BuddyAccounts/ApiService 子Key/Checkin 均无）；`::selection` 主题中性选区色；`focus-visible` 键盘焦点环（a11y，鼠标点击不打扰）。
- **可点击指标卡反馈**：BuddyOverview 两处包 StatCard 的跳转按钮补 cursor+上浮+阴影悬浮反馈。

---

## [3.4.4] · feature/buddy §2.2 非功能需求补齐 + 批次 1-4 九大类黑盒审查修复

### Added
- **健康检测（F-34 ④/§2.2 频控）**：API 网关启动即派健康探针线程——每 5min + 0-60s 抖动对 WB 上游 CN 主域名发无凭证轻量 GET（任何 HTTP 响应=在线，仅连接失败判不可达，零凭证暴露、单次单请求不重试）；结果经 `/status` 的 `wb.probe_ok`/`wb.probe_ts_ms` 透出（-1 未探测/0 不可达/1 在线）。
- **域名双探测（§2.2 接口稳定性）**：`wb_common.billing_bases(domain)` 返回主/备域名（codebuddy.cn ↔ workbuddy.ai）；积分三件套主域名整体网络不可达时切备用重试一轮（`_fetch_round` 抽取）；签到 `checkin_do` 网络不可达时切备用重试一次。

### Fixed
- **[P0] 会话三件套命令路径逃逸**：`workbuddy_chatdata_backup/restore/info` 的 `user_id` 入参新增 `wb_chat_uid_guard`（字符白名单 + 池内存在性校验），杜绝 `..`/绝对路径注入导致 backup 的 `remove_dir_all` 任意目录删除（与 `workbuddy_chatdata_copy` 同类防护对齐）。
- **[P1] 会话备份改原子替换**：先写 `<uid>.staging` 临时目录，完整性校验通过后才替换旧份——复制中断不再毁掉唯一备份。
- **[P1] 会话恢复失败自动回滚**：`copy_dir_recursive`/db 复制失败时自动从 `.bak` 还原 projects 与双 db，消除"半恢复 + db 已挪走"悬挂态（对齐 CHANGELOG 批次 3 声称的自动回滚语义）。
- **[P1] API Key 记账并发覆盖**：`api_keys.json` 读-改-写改为进程级锁内原子的 `verify_and_consume_locked`，并发请求不再互相覆盖 `used_today`/`daily_stats`（配额防穿透）。
- **[P2] Key 比较常量时间化**：子 Key 校验对两侧求 sha256 后比对，防逐字节提前返回泄露前缀匹配长度。
- **[P2] `safe_slice` 字符边界截断**：字节落点在多字节字符内时回退到最近合法边界，不再把整个超长上游响应体放进错误消息/日志。
- **[P2] 成长中心错误消息三元优先级**：`"...HTTP %s" % status if status else raw` 三处修正括号归属，网络不可达时不再丢失上下文。
- **[P2] sessions/edge 克隆 SQL 标识符转义**：新增 `sql_quote_ident`（内嵌双引号转义），列名/表名含 `"` 时不再拼接畸形 SQL。

### 备注
- 黑盒审查机制：无会话上下文子代理按九大类清单独立审查批次 1-4 热区，双轴结论（规格轴/标准轴）均为「有条件通过」，上述问题全部修复闭环。
- 已知技术债（记录不阻塞）：ck_ 子 Key 明文落盘（data/ 不入库，涉存量迁移后置）；wb_route 流式/非流式取号循环 ~180 行重复（重构后置）。

---

## [3.4.3] · feature/buddy 批次 4（Codex 投影 + 区域路由 + 快照回退 + 活动展示 + UI 兜底）

### 新增

- **Codex `/v1/responses` 投影转换器（T4.1/F-40）**：新增 `api_server/wb_responses.rs`（7 单测）——请求投影 instructions→system、input（string | items）→ messages（message/function_call→tool_calls/function_call_output→tool/reasoning 跳过）、tools 平铺→function 包裹、`max_output_tokens`→`max_tokens`、`reasoning.effort`→`reasoning_effort`；非流式投影 completion→response 对象（output_text / function_call items + usage input/output/total_tokens）；流式投影 `Protocol::Responses`（wb_sse.rs：response.created → output_item.added → output_text.delta → output_item.done → response.completed，流内错误→response.failed，无 [DONE] 帧，6 处 solo 管线 match 兜底补全）；`server.rs` 注册 `/v1/responses`（仅 WB 上游模型，明确报错提示）；Codex CLI `config.toml` 直配 `wire_api="responses"` + `base_url=http://127.0.0.1:<port>/v1`；脱敏沿用全局 wb_sanitize 与既有审核退回管线（三协议一份）
- **区域路由 Global 区（T4.5/F-36）**：对话上游 CN/Global 双域名（wb_upstream，批次2 已备）；本批补齐——积分三件套（workbuddy_credits.py `_billing_urls` 按账号 domain）、签到 + 成长中心（workbuddy_checkin.py `_urls()` 按账号区域切换全部端点）、官方用量（usage_official Global 账号修正为 `www.workbuddy.ai`，修复原 `https://.workbuddy.ai` 坏 URL）、活动接口随账号区域；`wb_common.py` 沉淀 `region_billing_base`/`is_global_region`；plugin 网关（token refresh）固定 codebuddy.cn 不随区域
- **积分用量快照回退（T4.3/F-27）**：credits_fetch 非缓存命中时追加每日余额快照（`data/workbuddy_credits_history.json`，按日去重 cap 365）；`workbuddy_usage_fallback` 官方用量不可用时自动切换——快照差分 + 签到日志「+N」奖励推导当日充值（负差值记 0），今日/近7天/本月聚合口径与官方对齐；TokenStatsPanel 官方请求失败自动回退渲染（amber「快照回退数据源」标注 + KPI + 每日消耗柱图 + 推导口径说明）
- **活动信息展示（T4.4/F-51）**：`workbuddy_activity_info` 三端点聚合——公开 GET `/v2/activity/banner`（宽容解析 banners/banner/list，只保留展示字段）+ billing POST `get-payment-type`（paymentType 徽标）+ `get-dosage-notify`（透传 data）；逐项容错 errors[] 明示；10min 缓存；BuddyOverview 活动信息卡（banner 横滑 + 付费类型徽标 + 用量提醒条）
- **UI 坐标点击签到兜底（T4.2/F-18）**：新增 `src-python/workbuddy_ui_click.py`（ctypes user32 SetCursorPos/mouse_event，零新依赖）；`workbuddy_ui_click_capture`（3 秒倒计时取点）/ `workbuddy_ui_click_checkin`（单次单击，settings `ui_click_enabled` 默认关闭 + `ui_click_x/y` 坐标校验）；BuddyCheckin 兜底卡（启用开关 + 取点 + 执行 + 使用说明）

### 变更

- `Protocol` 枚举新增 `Responses` 变体（log_path `/v1/responses`）；wb_route chat_id 生成 `resp_` 前缀；`WorkBuddySettings` +3 字段（ui_click_*，serde default 向后兼容）；types.ts 新增 WbActivityInfo/WbUsageFallback

### 说明

- `/v1/responses` 仅支持 WB 上游模型（Codex 直配目标场景）；非 WB 模型返回明确 400 提示
- 快照回退从本版起积累时序（需 ≥2 天快照），历史数据无法回溯推导
- UI 坐标点击为 P3 兜底：分辨率/缩放/DPI 变化会使预存坐标失效，需重新取点

## [3.4.2] · feature/buddy 批次 3（会话数据 + 用量 + CLI 桥 + 生态）

### 新增

- **CLI 切号桥 + 五重防护轮换（T3.4/F-06/F-59）**：新增 `src-tauri/src/workbuddy_cli.rs`（决策纯函数 `decide_target`：有效候选过滤 → 紧迫排序 → 紧迫阈值 → 当前即目标 → 冷却期 → CLI 活跃保护（jsonl mtime）→ 最小剩余积分 → 最小横跳间隔，13 单测）；`commands/workbuddy.rs` CLI 桥：`workbuddy_cli_status`（含 environment_override 进程 env 警告）/ `workbuddy_cli_bridge_set`（写 `~/.codebuddy/settings.json` env 直桥）/ `workbuddy_cli_rotate_run` / `workbuddy_cli_rotate_logs`（cap 50）；后台轮换线程按 settings 七个 `cli_*` 参数独立运行，Windows 路线绕过 apiKeyHelper 直写 env；BuddySettings 新增 CliRotateCard（状态行 + 参数网格 + 立即检查 + 轮换日志）
- **会话三件套备份/恢复 + 复制迁移（T3.1/T3.2/F-44/F-45）**：`workbuddy_chatdata_backup/restore/info`（正文 `~/.workbuddy/projects` + workbuddy.db + edge-sync-mapping-v2.db → `data/workbuddy_chats/<uid>/`；恢复前 `.bak` 单代保护 + 完整性校验失败自动回滚）与 `workbuddy_chatdata_copy`（jsonl 逐行 sessionId 换全新 UUID——`pseudo_uuid_v4` sha256 纳秒源纯函数；sessions 表动态列整行克隆；edge 映射全表扫描 convmsg 替换；`.pre-copy.bak` 双 db 预备份）；账号卡菜单 + 表视图操作接入
- **官方用量 + Token 统计增强（T3.3/F-25/26/57/58）**：新增 `commands/workbuddy_stats.rs`——本地 JSONL 统计合并 `~/.workbuddy/projects` 与 `~/.codebuddy/projects`（跳过 subagents；usage 取值 message.usage > providerData.usage > 顶层；cache_read 别名链优先正值防陈旧 0 掩盖；cache_write 仅认显式别名；365 天窗口，6 单测）；`workbuddy_usage_official` 官方请求用量（`POST <domain>/billing/meter/get-user-request-usage` 近 31 天分页 requestId 去重，今日/近7天/本月 + 逐日按模型，10min 缓存，prompt/input 字段脱敏不落盘）；TokenStatsPanel（四指标卡 + 构成堆叠条 + ComposedChart 双轴趋势[堆叠柱四类构成 + 调用次数虚线] + GitHub 风格年度热力图 + 模型排行 Top8；官方用量区含剩余/今日/近7天/本月 KPI + 按模型堆叠柱 + 官方模型排行）；BuddyCredits「Token 统计」Tab 占位转正
- **OAuth 扫码登录 + 环境重置（T3.5/F-50/F-14）**：`workbuddy_oauth_login`（`auth/state?platform=CLI` → 系统浏览器 → `auth/token?state=` 轮询 ≤300s → `login/account?state=` 取资料 → 自动入池 + 凭证回写 token store；每流程独立 cookie jar = Set-Cookie 手工捕获回传，零新依赖；事件 wb-oauth-progress/done）；`workbuddy_env_reset_items/_env_reset`（16 项认证残留清理清单——对齐 oss-research antigravity-tools 17 物理位置，「认证文件」合并两文件；Keycloak SSO 注销先于清理（JWT iss → 浏览器 logout）；执行前自动关闭 WorkBuddy；单项失败不中断）；账号页聚合区 OAuth 按钮激活 + PageHeader 环境重置入口（勾选预览 + 二次确认弹框）
- **账号库导入导出 + 通知渠道（T3.6/F-19/F-46）**：`workbuddy_accounts_export/import`（kind 标记 `aiwork-workbuddy-pool`、按 id 去重、凭证可选随行并回写 token store）；notify.rs 重写为 `NotifyChannels`（企业微信 webhook + Server酱 sendkey），`push_notify` 统一入口，渠道失败静默记日志；BuddySettings 通知渠道卡
- **ck_ 子 Key 体系（T3.7/F-35）**：`api_keys.rs` 扩展——子 Key `ck_` 前缀生成（旧 sk- 兼容）、`allowed_accounts` 限定上游白名单、`schedule_mode` 专一/临期优先两模式（`dedicated_account` 绑定）、`daily_stats` 按日请求统计（cap 90）；鉴权中间件下发 `ResolvedKey` 快照，wb_route 流式/非流式统一走 `pick_excluding_constrained`（专一锁定 > 白名单过滤 > 池策略；粘性账号不在白名单时忽略粘性）；ApiService Key 表新增调度列 + 配置弹框（模式切换/专一账号/上游多选/近 7 日统计迷你柱图）；Key 删除确认改弹框（移除 window.confirm 红线违例）

### 变更

- `upsert_token_store` 签名放宽为 `&AppState`（OAuth 后台线程复用）；`process.rs` 映像表新增 WorkBuddy.exe（三级关闭）
- `main.rs` 注册 12 个新命令 + `workbuddy_cli` / `workbuddy_stats` 模块；启动线程挂载 CLI 轮换
- BuddyCheckin settings 类型对齐 `WorkBuddySettings`（修 TS2345）

### 说明

- OAuth 三端点与官方用量的响应结构按设计文档 §3.10/§7.1 宽容解析（dig 多 key 回退），首次真实登录联调前字段名可能需按实测微调
- 环境重置清理项基于 oss-research 实测清单 Rust 化重写（learn-the-design 不抄码）；对 state.vscdb / workbuddy.db 的 DELETE 均在客户端关闭后执行
- 本地 Token 统计口径：input 已含缓存读取（供应商语义），总 Token = input + output + cache_write 不重复计 read；缓存命中率 = cache_read / input

## [3.4.1] · feature/buddy 批次 2（API 暴露 + 成长中心）

### 新增

- **WorkBuddy 网关上游适配（T2.1/F-28/F-30）**：新增 `api_server/wb_catalog.rs`（15 模型静态目录：上下文/maxTokens/图片模态/supported_efforts/effort_override 修正层/倍率）、`wb_payload.rs`（强制 stream:true；tool_choice 对象→string；effort 按目录降级；指纹清洗 cc_xxx/x-anthropic-* 剥离；审核模板黑名单最小改写——映射表 `wb_template_map.json` mtime 热更新，内置兜底 CLI→CLI tool、Main branch→Default branch；连续同角色消息合并）、`wb_sse.rs`（WB OpenAI 风格 SSE 解析：多行 data/紧凑流/注释行兼容；tool_calls 按 index 合并聚合；OpenAI/text/Anthropic 三协议流式与聚合输出）、`wb_upstream.rs`（headers 三铁律：Origin/Referer 按区域必带 + X-No-* 占位 + **chat 绝不带 X-Refresh-Token 红线**；UA `CLI/2.63.2 CodeBuddy/2.63.2`；CN/Global 双域路由）、`wb_route.rs`（WB 请求主路径）
- **调度引擎扩展（T2.2/F-29/F-33）**：pool.rs 新增 `weighted`（三因子加权：积分占比×10 + 闲置补偿 0.5/h 封顶 5.0 + 成功率×3，Top5 二次加权随机）与 `p2c`（随机选二取优）策略；账号五态机（Available/QuotaProtection/RateLimited/Forbidden/ProxyDisabled，随 PoolStatus.state 下发）；hard_credit 冷却至次日 04:00 自动恢复；熔断 30m 起指数递增封顶 6h；防惊群 100ms 窗口；新增 `retry.rs` 分级重试表（429 Retry-After/线性、503/529 指数 10/20/40s、400+thinking.signature 200ms 一次、401/403 换号、400 Fatal），纯函数 5 组单测
- **会话粘性双模式（T2.4/F-31）**：`wb_sticky.rs` 显式 conversationId 绑定（TTL 30m 滚动续期）+ 前 3 消息 SHA256 指纹 60s 窗；上游 conversation_id 双段分配；Mutex 内 re-check 防 TOCTOU；持久化 `wb_sticky_sessions.json`（含 5 组单测）
- **成长中心自动化（T2.5/F-17）**：`workbuddy_checkin.py --growth` 执行器——Buddy 旅行（status→claim→config→depart 链式）/ 盲盒（chances→draw 循环，上限 20）/ 任务领奖（tasks→accept 过滤未领）/ 能量与连签天数展示；各步独立容错 + 401 刷新一次重试；奖励数额以接口返回为准；NDJSON 复用 wb-checkin-progress 管线（`mode:"growth"` 分流）；buddy-checkin 页接线「立即执行成长任务」+ 逐账号明细（旅行/盲盒/任务结果 + 能量/连签）
- **双源 token 保活（T2.6）**：网关 401 自动刷新——WB 上游 401 时调 refresh 端点（X-Refresh-Token 红线约束）刷新工具侧副本并同号重试一次（每账号每请求一次），与桌面 auth 文件谁新用谁（F-10）
- **运维接口（T2.3/F-32）**：`/healthz`（无健康账号 503）；`/v1/models` 合并 WB 目录（owned_by=workbuddy）；`/status` `/health` 新增 `wb` 段（池画像/模型冷却/粘性会话数）；WB 请求日志含 TTFB
- **工程化（T2.7/F-34）**：模型级冷却渐进退避 10→20→40s（优先级高于 Key 级）；SSE keep-alive 15s 注释行（SOLO 与 WB 流式均接入）；首字超时 10s 故障转移（转发线程 + recv_timeout，Agent 300s 读超时兜底）；客户端断连后继续消费上游保 usage 完整
- `wb_common.py` 新增 `get_json`（GET 请求，对齐 post_json 容错语义）
- **到期日历 Trae / 豆包侧挂载（F-13 批次 1 遗留补齐）**：Trae「积分看板」新增到期日历卡（token JWT / 积分包 / 会员三类，Unix 秒直读）；豆包「概述」新增到期日历卡（会员 quota_expire_at / 会话 session_expire_at，本地时间字符串转 Unix 秒）

### 变更

- `ApiPoolFile`（api_pool.json）新增 `wb_enabled` 字段（默认 false）；`pool_set` 命令扩展可选 `wb_enabled` 参数（向后兼容）
- 模型路由：请求模型命中 wb_model_catalog → WB 上游；wb_enabled=false 时返回 400 明确报错
- `workbuddy_growth_run` 不再是占位：实际驱动 python 成长中心执行器

### 说明

- WB 上游未接入真实账号联调（需账号池含 token store 凭证副本 + wb_enabled 开启）；协议要点均按设计文档 §3.9/§5.5/§5.6 落地并附单测
- 会话粘性在 Buddy 上游的价值为会话一致性与上游侧缓存（若有）；代理流量 prompt cache 恒不命中（§5.5 #10），成本模型按无缓存估算
- Codex /v1/responses 投影、DSH provider 动态目录同步、CC Switch 注册按设计文档归批次 3/4

## [3.4.0] · feature/buddy 批次 1（WorkBuddy 接入快赢闭环）

### 新增

- **Buddy 应用级子导航**：Sidebar 底部应用 Tab 的「Buddy」项启用（对齐豆包先例），新增 概述 / 账号管理 / 签到与成长 / 积分与统计 / 环境配置 五页（`src/pages/buddy/`，视图键 `buddy-*`）；到期日历通用组件 `ExpiryCalendar`（F-13）
- **WorkBuddy 账号池与切换**：`workbuddy_env_check` 环境检测（auth 文件 / `~/.workbuddy` 快照解析）；auth 文件扫描入池（id = `wb-<sha256(token)前12位>`，F-04）；PS 桥 authfile 布局快照/恢复（L1 auth 文件 + L2 用户数据，单代 .bak 回滚，切换后 30s uid 轮询确认，F-02）——切换/保存登录态复用既有 `switch_account` / `save_current_login`（`target_app=WorkBuddy`）
- **凭证续期（F-09/F-10）**：`workbuddy_refresh_token`（plugin refresh 端点，X-Refresh-Token 仅限该端点）；工具侧凭证副本 `workbuddy_token_store.json` 与桌面 auth 文件「谁新用谁」；schtasks 每周兜底任务 `AIWorkAssistant_WorkBuddyRenew`（python --renew-only 惰性刷新）
- **一键签到（F-15/F-16）**：python `workbuddy_checkin.py`（状态查询新路径回退旧路径 / code:10001 已签容错 / 401 刷新一次重试 / NDJSON `wb-checkin-progress` 独立管线 / 零 token 输出）；每日 09:00/21:00 双时段 schtasks；启动自动补签（F-55，延迟 60s 静默执行）
- **积分余额（F-20）**：python `workbuddy_credits.py`（积分三件套 + 旧接口回退 + 容量字段链宽容解析 + ≥5min 缓存）；`workbuddy_credits_fetch` 命令 + buddy-credits 页（KPI / 逐账号余额 / 积分包到期日历）；buddy-accounts 双态账号卡片（F-54，当前/备用 + 余额大字 + 活跃明细前 2 包 + 积分包明细弹窗 F-56 + 聚合迁移入口 F-60）
- **自动签到配置化（F-55）**：`workbuddy_settings.json`（auto_checkin / keepalive_days / lazy_refresh_hours / growth_* 开关）+ buddy-settings 配置页
- 新增 Python 公共库 `wb_common.py`（双源凭证 / dig 宽容解析 / 统一请求头 / token 刷新），均仅标准库

### 说明

- 成长中心执行器（旅行/盲盒/任务链式，F-17）与 Token 统计（F-57）、官方用量（F-25）按设计文档批次 2/3 交付，本期 UI 已预留入口与开关
- 到期日历已接入 WorkBuddy 五页；Trae / 豆包侧挂载随批次 2 补齐

---

## [3.3.5] - 2026-09-11

### 新增

- **TRAE 切换前会话预检**：切换账号前探活目标账号 JWT，已被服务端吊销则中止切换并指引「重新登录该账号并保存」，不再白切一次；续期 JWT 流程自动跳过预检（Refs #9）
- **TRAE 切换恢复后校验**：恢复完成校验关键登录态文件，快照为空/损坏或疑似 TRAE 新版布局漂移时自动回滚到切换前状态并明确报错，不再静默无效切换（Refs #9）

### 变更

- **应用选择大块布局**：切换账号/保存登录的 Trae CN 与 TRAE SOLO CN 选择改为左右两块大按钮（悬停高亮），目标区域大、不易点错
- **一键签到进度全集展示**：进度列表显示参与签到范围内的全部账号，被跳过的账号明示原因（已签/JWT 过期/冷却中），多次执行行序稳定、上轮结果保留

### 修复

- **签到失败精确指引**：JWT 被服务端吊销的账号明确提示重新登录并保存（不再笼统报「失败」），且自动跳过重试避免无效等待（Refs #9）
- **「今日已签」被覆盖丢失**：签到结果摘要改为同日合并，第二轮起被跳过的已签账号不再退回「未签」

## [3.3.4] - 2026-09-11

### 修复

- **API 服务「鉴权开关」按钮文案与实际动作相反**：关闭态误显示「关闭鉴权」、开启态误显示「开启鉴权」。现关闭态显示「开启鉴权」、开启态显示「关闭鉴权」，按钮配色改为跟随动作（开启=绿、关闭=琥珀）
- **一键签到空轮提示一闪而过**：全部账号已签/过期/冷却中时，「实时进度」卡片随 done 事件整体消失，用户无从得知原因。现空轮结束后卡片与徽章持续保留（下次签到自动重置），并新增常驻说明行告知无候选账号的具体原因

## [3.3.3] - 2026-09-11

### 修复（代码审查两项 + 吸收 trae_work_main 分支修复 81f8b20）

- **API 服务鉴权默认拒绝**：未配置任何启用 Key 时请求默认拒绝（401 JSON，含创建 Key 引导文案），不再无鉴权放行本机任意进程消耗上游额度；新增显式「鉴权开关」（关闭鉴权后放行并记 anonymous，UI 标注不推荐），改动立即生效
- **更新包完整性校验**：新增 `latest.json` 发布校验清单（`rename_release.py` 生成，发布时随 Release 上传），`update_check` 清单优先且 fail-closed（清单缺失/损坏/版本不符/未收录资产均阻止自动更新并引导手动下载）；无清单回退 release 正文约定行，两者皆无跳过（兼容存量 release）；下载完成后强制比对 SHA256，不匹配即删除并中止；`relaxed_key` 归一修复 GitHub 资产名重写（空格转点）导致正文校验永不命中的缺陷
- **日志轮询防风暴**：上一轮未返回跳过本轮（防 2s 轮询堆积）；失败 toast 60s 限频（手动刷新直通）；并发序号「最新请求胜出」，手动刷新与轮询并发时旧响应不再乱序覆盖
- 测试补齐：`api_keys` 测试补 `auth_disabled` 字段（`cargo check` 不编译测试代码曾漏检，教训已入 AGENT 记忆）

## [3.3.2] - 2026-09-10

### 修复

- **关闭应用卡死**：点击关闭/托盘退出后窗口冻结数秒才退出——根因为 `exit(0)` 内部 WebView2 窗口销毁在主线程同步执行（运行越久越明显），且 `RunEvent::Exit` 清理日志从未落盘证实卡点。改为 hide-then-exit：先隐藏窗口再退出，窗口立即消失，后台清理再慢也无感；窗口关闭与托盘「退出」两条路径统一处理
- **一键签到「点了没反应」**：无账号时签到按钮为禁用态且 `start()` 静默返回，双重无反馈。现无账号时整页显示引导空态（含「前往账号管理添加」跳转按钮）；按钮恢复可点击，按场景精确提示（暂无账号 / 所选分组无账号 / 未勾选账号）

### 优化

- 静默反馈排查收尾：账号池「刷新」按钮失败时给出错误提示（此前 loading 结束无任何反馈）；日志页空日志点击「复制」时提示「暂无日志可复制」（此前静默返回）

## [3.3.1] - 2026-09-10

### 修复

- **证书安装（#6）**：cert_install 前置依赖自检并自动 pip 自愈；gen_ca 捕获子进程真实报错（含 `No module named` 修复指引）；certutil 路径加引号防空格拆参、`-PassThru` 取真实退出码；安装后复查根存储；前端 handleRun 兜底 toast，消除「点了没反应」
- **代理生命周期（#7）**：`SO_EXCLUSIVEADDRUSE` 独占绑定，端口被占时带占用 PID 诊断并退出（不再「假启动」）；`log()` print 纳入容错；CONNECT 先握手后记日志；proxy_start 端口预检 + 清理遗留孤儿进程；python 子进程统一挂 Job Object（kill-on-close）防崩溃/强杀泄漏

### 新增

- 安装/portable 包内置精简 Python 3.12 运行时 + cryptography/pywin32，不再依赖系统 Python（新增 `scripts/prepare_python_runtime.py`）
- 运行时精简：剔除 pywin32 自带 IDE/COM 扩展/帮助文档等无用项（净减 ~10.7MB 未压缩），保留 dist-info 与 `__pycache__`

## [3.3.0] - 2026-09-10

### 新增（T1-T11 自 trae_work_main 手工移植合入，未使用 merge/cherry-pick）

- **API 网关用量统计（T1）**：`data/api_usage.json` 按日落盘（模型 / 上游账号 / API Key / 流式 / 成败 / 耗时 / token 多维聚合，保留 90 天）；`api_usage_stats(days)` 命令 + API 服务页「用量统计」面板（StatCard ×4 + 堆叠柱状图 + 模型分布 Top5 表）。
- **多 API Key + 每日配额（T2，含 T15 调整）**：`data/api_keys.json` 多 Key 独立签发与每日限额（0=不限），超配额返回 429；移除主 API Key 双轨校验，鉴权统一走 Key 列表（未配置启用 Key 时不鉴权，携带未知 Key 放行记 anonymous）；前端「API Keys 管理」卡片（生成 / 启停 / 限额失焦保存 / 删除 / 复制）。
- **托盘菜单增强（T3）**：托盘「立即签到」与「启动/停止 API 服务」（动态文本）+ 完成系统通知；与页面操作共用防重入锁，不冲突。
- **敏感数据加密（T4）**：jwt / refresh_token 迁入 Stronghold vault（`conf/vault.stronghold`），主密码经 Windows DPAPI 保护（`conf/vault_key.bin`，仅本机当前用户可解）；JSON 落盘占位化，读写统一 `vault::load_accounts / save_accounts`（vault 写失败降级明文 + 下次启动重试迁移）；Python 签到脚本走临时解密文件（`--accounts-file`，用后即删 + 启动按前缀清理崩溃残留）。
- **签到失败自动重试（T5）**：最多 2 轮（间隔 30s / 90s）仅重试失败账号；`CheckinGuard`（tokio::sync::Mutex）应用级防重入，页面 / 托盘 / 静默签到共用；per-uid 最终态合并保证 `ok+already+failed==total`；前端重试倒计时横幅。
- **日志按类型清理（T6）**：`logs_clear(log_type)` 命令（all/proxy/checkin/switch），日志页「清理」按钮 + 确认弹窗；不做导出（明确排除）。
- **前端测试基建（T7）**：抽取 `src/lib/format.ts`（maskApiKey / fmtTokens），新增 vitest@^2（`npm run test`），cn / delay / format 共 13 个纯函数用例。
- **签到成功率趋势（T8）**：`data/checkin_results.json` 按日 per-uid 落库最终态（保留 90 天）；`checkin_trends(days)` 命令 + Dashboard「近 30 天签到结果」堆叠柱状图（成功绿 / 已签蓝 / 失败红）。
- **OpenAI 兼容端点扩展（T9）**：新增 `POST /v1/completions`（legacy text completion，prompt 转 user message 复用现有链路，流式 / 非流式）；`/v1/embeddings` 明确返回 501（不做假实现）。
- **账号池调度策略 + 分组筛选（T10）**：`api_pool.json` 扩展 `strategy`（expire_first / credit_first / random）与 `group_ids`（空=全部参与）；策略纯函数化 + 分组同步期过滤；前端策略下拉 + 分组多选 + 实时预览（分组外账号置灰标记）。
- **开机自启 + 启动静默签到（T11）**：`tauri-plugin-autostart`（开关即时生效）+ 启动延迟 60s 对未签到账号自动签到（复用统一签到链路，skip_checked_in=true 幂等，完成发系统通知）。

### 变更

- 高版本适配：RawAccount 新增 `dc_id` 字段的构造点补齐（pool.rs 单测等）；自启 / 静默签到开关落在 `GeneralSettingsPanel.tsx`（高版本设置面板）；`trae_apps.rs` 接入 vault（高版本无 pay_status.rs / trae_local.rs）。
- `Settings` 移除 `api_key` 字段（serde 默认忽略旧配置残留字段，该 Key 不再参与鉴权）。
- `pool_set` 扩展 `strategy` / `group_ids` 参数；`models_sync::fetch_official` 签名改为调用方预解密账号；`/v1/completions`、`/v1/embeddings` 路由注册。
- `Cargo.toml` 新增 `tauri-plugin-stronghold`、`tauri-plugin-autostart`、windows-sys（DPAPI）与 `[profile.dev.package."*"] opt-level = 2`；`package.json` 新增 `vitest@^2.1.9` 与 `test` script。

### 修复

- **积分 0 显示为 "-0"**：账号管理悬停明细（通用/Work 积分与积分包）、积分看板统计卡/趋势 tooltip/明细表、签到页积分列——JS `(-0).toLocaleString()` 输出 "-0" 且 `JSON.parse("-0.0")` 得负零，Rust `(v*100).round()/100` 舍入亦保留负零。前端新增共享 `normZero`/`fmtCredits` 在全部积分展示点归一化；后端 `calc_remaining_credits` / `fetch_credit_detail` 的 r2 舍入结果归一为 +0.0，落盘与 IPC 不再出现 "-0.0"。

### 文档

- 新增 `docs/optimization-implementation.md`（T1-T11 需求 / 价值 / 实现逻辑 / 代码参考，按本分支适配）与 `docs/optimization-plan.md`；AGENT.md 命令契约表、api-doc.md 新增命令章节与过时签名同步修正。

### 验证

- `cargo test` 通过、`npx tsc --noEmit` 通过、`npm run test`（vitest）18/18（含 normZero/fmtCredits 回归用例）、`npx vite build` 成功。

## [3.2.7] - 2026-09-08

### 新增

- **安装位置自动识别（F-01）**：新增 `app_locate(target_app)` 命令——手动指定 → 注册表卸载键 → 默认路径 → 运行进程反查 四级探测，统一返回 `{exe, userDataDir, version, source}`；档案表驱动四应用（Trae Work / Trae CN / 豆包 / WorkBuddy），设置页「自动检测」按钮改走该命令。
- **响应宽容解析（F-49）**：`fs_utils::dig()` 沿 data/result/resp/response/info 包裹键递归下钻（限深 8 层），采纳到积分包、套餐身份、ExchangeToken 三处解析点，抗官方信封字段变动。
- **账号导入预览（F-46 残余）**：新增 `accounts_import_preview` 命令（解析文件、标记已存在账号与待新增分组，不写盘）；导入改两步式——选文件 → 预览弹框逐条勾选 → 按索引导入。
- **快照桥扩展（F-48 部分）**：PS 桥档案表扩至四应用（`-TargetApp TraeWork|Trae|Doubao|WorkBuddy`），引入 `SnapshotLayout` 布局标记，非 icube 布局的快照/设备重置显式报错待各应用批次接入。

### 修复

- **导入预览弹框必崩**：`ImportPreview` / `ImportPreviewAccount` / `AppLocate` 三个新 DTO 误加 serde `camelCase` 改名，与项目蛇形命名约定及前端失配（`new_groups` 等字段为 undefined），渲染时抛 TypeError——移除改名恢复蛇形上线。
- **分组默认名多字节 panic**：分组 id 含多字节字符时 `&id[..4]` 字节切片越界 panic，改按字符截断；账号 uid 尾部摘要同类隐患一并修复。
- **悬停积分明细 / 刷新积分 / 刷新套餐 / 扫描本机账号冻结 UI**：`fetch_credit_detail`、`fetch_remaining_credits`、`refresh_remaining_credits`、`refresh_pay_status`、`apps_accounts_discover` 五条同步网络命令改 `#[tauri::command(async)]`。
- **套餐刷新静默降级 Free**：`query_pay_status` 对 2xx 但缺关键字段的异常响应返回错误，不再用 "Free" 覆盖缓存中的正确套餐。
- **卸载「删除应用数据」删不干净**：NSIS 卸载钩子补删 `%APPDATA%\AIWorkAssistant`（含旧 TraeWorkAssistant 遗留），此前只删 WebView2 的 identifier 目录。
- **并发写 JSON 踩踏临时文件**：`write_json` 临时文件名加 pid+纳秒唯一化；模型列表损坏时记日志 + `.bak` 备份 + 默认配置自愈，不再静默覆盖。
- **更新器残留与路径防御**：下载前清理 `%TEMP%` 旧版本安装包残留；拒绝含路径分隔符 / `..` 的资产名（防临时目录逃逸）。
- **积分明细口径不一**：无 `expire_time` 的积分包计入明细（显示「长期有效」），与统计口径对齐；明细失败后下次悬停可重试。
- **导入 / 入池交互**：导入空文件明确提示且按钮加载态防重复点击；发现账号入池按钮 per-item pending；更新包大小为 0 时显示「未知大小」而非 0 B。
- **安装器边缘**：被动模式（/P）同/升级直接覆盖安装、降级遵循 ALLOWDOWNGRADES，不再误触发旧版卸载器；`rename_release.py` 单产物缺失输出 WARNING 并支持 `--strict`。

### 文档

- AGENT.md 对照 v3.2.6 代码库校准（目录地图、命令契约表、双 uid 红线、主题约定）；`future-roadmap.md` 升 v1.3（F-01/F-03/F-12/F-23/F-46 残余/F-48/F-49 移入已完成）。

---

## [3.2.6] - 2026-09-08

### 修复

- **官网模型同步必然失败**：`fetch_official` 误从数据根目录读取 `checkin_accounts.json` / `device_map.json`（实际位于 `data/` 子目录），导致恒报「没有可用账号（缺少 JWT）」；同时 `api_models.json` 统一移至 `data/` 子目录（与 `AppState::path()` 路由一致），旧位置自动兼容迁移。
- **`/v1/models` 与应用配置不一致**：原返回硬编码静态列表（缺 glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code 三个内置模型），改为与 `api_models.json` 同源——官网同步后无需重启 API 服务即可通过 `/v1/models` 看到最新列表。
- **上游中文错误响应可致 panic**：日志预览 `safe_slice` 按字节截断，第 200 字节落在 UTF-8 多字节字符中间（中文错误 JSON 常见）即 panic——流式客户端收到截断空流、非流式返回 500。改为按字符边界截断，账号 uid 摘要同步安全化。
- **环境检测 / 打开 Trae 应用期间 UI 卡顿**：`env_check`（注册表全量搜索可达数秒 + PowerShell 取版本）、`open_trae_app`（代理注入时进程优雅关闭最长 5s 轮询）、`task_*`（schtasks 子进程调用）均为同步命令在主线程执行，全部改 `#[tauri::command(async)]` 派发线程池（与 3.2.5 updater 冻结修复同因）。
- **OpenAI 端点对无效 JSON 请求返回 502**：原 `unwrap_or(json!({}))` 后仍转发原始 body 到上游；现校验失败直接返回 400 `invalid_request_error`（与 `/v1/messages` 行为对齐）。
- **错误分类误冷却**：`classify_solo_error` 宽泛匹配 `contains("plan")`，"planned maintenance" 等消息会被误判 PlanLimit 导致账号冷却 12 小时；收紧为 code 1005 / `plan limit` / `额度用尽` / `套餐额度` 精确匹配。
- **流式上游 Agent 无读超时**：连接建立后若上游不发数据，请求与线程永久挂起；增加 300s 读超时（正常 SSE token 间隔远小于此），并修正与实现不符的 `response_header_timeout` 注释。
- **`read_text_file` 无大小限制**：增加 10MB 上限与常规文件校验，防误选超大文件拖垮前端。
- `models.rs` 两处 GBK 乱码注释修复。

### 变更

- 移除进程级 `NO_PROXY` 环境变量设置：项目未启用 ureq 的 `proxy-from-env` feature，Agent 默认直连不读代理环境变量，原设置本就无效（删除无行为变化）；updater「直连」通道语义同步澄清。
- **品牌迁移健壮性增强**：目录复制失败时回滚半成品（避免下次启动误判「已迁移」导致文件缺失）；WebView2 用户数据目录迁移排除 Cache / GPUCache / Crashpad 等 8 类缓存子目录（体积可达 GB 级且常被运行中的老应用锁定，新版本首启自动重建）；数据已迁移后静默跳过，不再每次启动输出提示。

### 安装器

- **不再静默卸载老品牌「Trae Work 助手」**：原 NSIS 安装钩子在安装/升级时会执行老品牌卸载器并清理其安装目录、注册表卸载键与快捷方式（且强杀老品牌进程）；现全部移除——两版并存、互不干扰，用户数据目录复制迁移逻辑不变（老版本数据原地保留）。

### 文案

- API 网关描述统一为「OpenAI / Anthropic 兼容接口」：API 服务页页头、关于页简介、README、用户手册。

---

## [3.2.5] - 2026-09-08

### 新增

- **模型列表配置化与官网同步**（自 2.x v2.8.0 移植）：新增 `models_sync` 模块——模型下拉列表持久化在 `api_models.json`（缺失时写入内置默认 18 个模型）；ApiService 页新增「同步官网模型」按钮，重放 Trae 客户端 `batch_get_detail_param` 配置接口获取权威列表（不消耗积分，自动跳过内部/隐藏模型并按默认顺序归位）；`payload` 模型映射表同步扩充至官网最新（新增 Doubao-Seed-Evolving / Doubao-Seed-Code / DeepSeek-V4-Pro-Official / kimi-k2.6 / kimi-k3 / qwen3.8-flash / qwen3.8-max 等）；实测 glm-5.3-flash / qwen3.8-flash / Doubao-Seed-Code 仅在 `function=solo_agent` 下可用，改为按模型分发 function；`/v1/models` 静态列表同步更新（移除已下线的 sagitta / aquila / doubao-seed-2.0-code / glm-5 / glm-5-turbo）。
- **单实例防护**：重复启动应用（双击/自启动后再点）时，不再产生第二个进程——已有实例的主窗口自动还原、显示并聚焦，新进程自动退出。基于官方 `tauri-plugin-single-instance` 实现，仅正式版启用（dev 模式与已安装版共用 identifier，启用会互相顶替干扰调试）。

### 修复

- **检查更新「下载安装包」国内网络直连超时（os error 10060）**：`update_check` 请求的 `api.github.com` 可直连，但下载 302 跳转到 `objects.githubusercontent.com`（GitHub 下载 CDN）直连不通，而下载此前只认环境变量代理、不认系统代理（VPN）。改为逐通道尝试：**系统代理（注册表，即用户 VPN）→ 环境变量代理 → 直连**，任一通道成功即完成；检查更新同样受益。另修复下载误设 30s 整体超时（慢网络下必被掐断），改为连接 10s + 读 60s、不设整体超时。
- **检查更新期间整个应用卡死**：`update_check` / `update_download` 为同步命令，Tauri 默认在主线程执行，网络重试最坏 90 秒会冻住 UI。改为 `#[tauri::command(async)]` 派发到异步运行时线程池，主线程不再阻塞（窗口/按钮全程可响应）。

---

## [3.2.3] - 2026-09-08

### 修复

- **签到必崩 `Failed to import encodings module`（v3.2.0 – v3.2.2 全部受影响）**：v3.2.0 起的安装包/便携包把 `src-python/` 内混入的一套残缺 Python 3.13 运行时（缺 `Lib/encodings`）打进了 `resources/python/`，而应用优先使用内嵌解释器 → 升级后签到必崩。两步修复：移除残缺运行时；`state.rs` 内嵌与系统解释器探测全部改用 `import encodings` 自举验证（原 `--version` 探测不触发 stdlib 导入，残缺运行时也能通过），残缺内嵌解释器自动回退系统 Python——已安装 3.2.x 的机器升级本版后立即恢复，无需手动清理残留文件。

---

## [3.2.2] - 2026-09-07

### 修复

- **检查更新下载步骤必报错（3.1.0 / 3.2.x 全部受影响）**：前端 invoke `update_download` 时参数 key 传了 `version`，而 Tauri 2 顶层参数按驼峰匹配 Rust 参数 `expected_version` → `expectedVersion`，导致「下载更新包」必报 `missing required key expectedVersion`（v3.1.0 的 `update_install` 同一问题，应用内更新从未成功）。修正为 `expectedVersion`。

---

## [3.2.1] - 2026-09-07

### 修复

- **检查更新只匹配本产品线（≥ 3.0.0）**：同一仓库同时发布 2.x（Trae Work 助手）与 3.x（AI Work 助手）两条产品线、是两个不同产品，`releases/latest` 会指向最近发布的那条线。改为拉取 releases 列表（跳过 draft / prerelease），只认 ≥ 3.0.0 的 release 并取版本最高者；`release_page` 改指具体 release 页；发布页链接改为 `…/releases`（不再用 `/latest`）。

---

## [3.2.0] - 2026-09-07

### 变更

- **应用内更新改为两步确认制**：检查更新发现新版本后不再自动下载安装，拆分为「下载」与「安装」两个独立步骤，UI 各有一次确认——确认一下载更新包（带进度条），确认二「立即安装并重启」。
- **安装器启动参数 `/S` → `/P /UPDATE /R`**：`/P` 被动模式（安装进度条可见）、`/UPDATE` 跳过卸载直接覆盖安装、`/R` 安装完成后自动重启应用（自定义 NSIS 模板已支持）。`update_run_installer` 增加路径校验：仅允许运行临时更新目录内的安装包且版本须大于当前版本。
- **NSIS 安装包升级体验优化**：升级安装（检测到旧版本）时不再弹出「卸载后安装 / 不卸载直接安装」选择页（原默认推荐先卸载），改为**跳过该页直接覆盖安装**；同版本重装与降级仍显示选择页。实现方式：新增自定义 NSIS 模板 `build-assets/installer.nsi`（基于 tauri v2.11.4 上游模板定制），经 `tauri.conf.json` 的 `bundle.windows.nsis.template` 启用。
- **版本号收敛为单源**：单一来源 = `src-tauri/Cargo.toml`。
  - `tauri.conf.json` 移除 `version` 字段（Tauri 自动回读 Cargo.toml）；
  - 关于页版本号改为运行时 `getVersion()` 读取，移除 `about.ts` 中的 `APP_VERSION` 硬编码；
  - 新增 `scripts/sync_version.py`：一条命令把版本同步到 package.json / AGENT.md 标题 / Cargo.lock；
  - `rename_release.py` / `package_portable.py` 版本读取改为 Cargo.toml 回退。

### 新增

- **API 服务新增 Anthropic 兼容端点 `POST /v1/messages`**（F-39「+Anthropic 适配」落地）：
  - 请求侧 `payload::anthropic_to_openai` 将 Anthropic Messages 请求（system / text blocks / tool_use / tool_result / tools / tool_choice）转换为 OpenAI 内部格式，复用既有账号池调度与 llm_utils_chat 链路；
  - 响应侧 `sse::stream_convert_anthropic` / `aggregate_anthropic` 输出 Anthropic 协议（流式 message_start → content_block_start/delta/stop → message_delta → message_stop 事件序列，支持 tool_use 块；非流式 message 对象含 usage）；
  - 鉴权支持 `x-api-key`（Anthropic 风格）与 `Authorization: Bearer`（OpenAI 风格）双风格；
  - 单元测试覆盖 text 与 tool 往返转换（`cargo test` 2 项通过）。
- API 服务页「使用方式 & 配置示例」补充 Anthropic 端点说明与 /v1/messages cURL 测试示例。

### 文档

- `docs/future-roadmap.md`：F-39「Trae API 暴露」标记完成并从待办排序移除（核心能力随 v3.1.0 网关 + F-08 双应用发现天然达成，本次补齐 Anthropic 适配）。
- `AGENT.md`：API 网关模块结构与端点契约同步（/v1/messages、双风格鉴权、账号池 app 无关说明）。

### 排查

- `src-ps/trae-switch-bridge.ps1` 编码排查：文件头已含 UTF-8 BOM（EF BB BF），不存在 PowerShell 5.1 按 GBK 误读问题，无需调整。

---

## [3.1.0] - 2026-09-07

API 服务页界面微调。

### 变更

- **API 服务页**：删除「使用方式 & 配置示例」标题右侧的「通用积分」徽章文字。
- **API 服务页**：积分体系说明面板中「当前全部账号通用积分总余额」改为靠右展示（`ml-auto`，空间不足自动换行并保持右对齐），去掉中间「·」分隔符。
- 版本号 3.0.0 → **3.1.0**（`package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` / `about.ts` 同步）。

---

## [3.0.0] - 2026-09-07

品牌定位迁移：产品更名为 **AI Work 助手（ai-work-assistant）**，面向多个 work 工具提供功能支持；同时清理全部旧品牌痕迹并保证老应用升级兼容。

### 变更

- **品牌统一**：代码注释、界面文案、README、AGENT.md、docs 全部文档由 Trae Work Assistant / trae-work-assistant 统一为 AI Work 助手 / ai-work-assistant。
- **打包标识**：identifier `com.traework.assistant` → `com.aiwork.assistant`，`mainBinaryName` → `ai-work-assistant`（主程序 ai-work-assistant.exe），Cargo 包名与 package.json 同步；新增 `scripts/rename_release.py` 将安装包统一输出到 `release/`，产物使用中文产品名命名（如 `AI Work 助手_3.0.0_x64-setup.exe` / `AI Work 助手_3.0.0_x64_zh-CN.msi` / `AI Work 助手_3.0.0_x64_portable.zip`）。
- **老应用升级兼容（NSIS）**：新增 `build-assets/installer-hooks.nsh`，安装时自动结束旧进程、静默卸载旧品牌「Trae Work 助手」并清理残留目录 / 卸载键 / 快捷方式 / 旧命名主程序。判定依据为安装时产品名而非版本号：已发布的 v2.4.4 及更早安装包均为旧品牌，同样被自动清理；仅「AI Work 助手」品牌（v3.0.0 起）走 NSIS 原生原地升级。
- **老应用数据自动迁移（启动时，复制语义）**：`state.rs::migrate_legacy_dirs()` 将 `%APPDATA%\TraeWorkAssistant` **递归复制**为 `%APPDATA%\AIWorkAssistant`，并复制 WebView2 界面偏好目录（identifier 变更所致）；**旧目录原地保留，老应用可继续使用，新旧两版可并存**；新目录已有数据则自动跳过（不重复迁移）；失败不影响启动。
- **计划任务并存迁移**：`misc.rs::try_migrate_legacy_task()` 检测到旧任务时按其原触发时间重建 `AIWorkAssistant_DailyCheckin`，**旧任务保留**供老应用继续使用；「取消注册」只删除新任务名。
- **环境变量**：`TRAEDATA_DIR` → `AIWORKDATA_DIR`（Python 脚本与 PowerShell 桥接脚本兼容读取旧变量名）。
- **版本线划分**：新版本自 3.0.0 起维护，**之前所有 2.x 版本升级到 3.x 均需数据迁移**（安装 / 首次启动自动完成）；原「Trae Work 助手」产品通过 `trae_work_main` 分支维护（仅 Trae Work 单应用，2.x.x，仅必要修复）。
- **界面**：账号管理页「使用帮助」按钮改为与页头描述文字水平对齐，并以圆形色块徽章突出展示（PageHeader 的 leftExtra 移入描述行内，与描述行垂直居中）。
- **应用内检查更新**：「关于」页版本号旁新增「检查更新」按钮——分析 GitHub Releases 最新发布，发现比当前更大的版本时自动下载 NSIS 安装包（实时进度条）并静默安装（/S，走安装钩子自动清理旧版），随后应用自动退出完成升级；网络异常时提示并附发布页直链。
- 版本号 2.5.0 → **3.0.0**（品牌迁移后的新版本起点；`package.json` / `tauri.conf.json` / `Cargo.toml` / `about.ts` 同步）。

### 说明

- **老 MSI 安装包无法原地升级**：MSI UpgradeCode 随 identifier 变化，老版本 MSI 用户请先卸载后安装新版，或改用 NSIS 安装包（-setup.exe）升级（推荐，自动迁移）。
- 旧数据目录迁移采用「复制」：迁移后旧目录原地保留（老应用可继续使用，两版并存）；迁移只在首次启动执行一次，之后新目录已有数据即跳过。

### 审查修正（发布前全量审查）

- 升级兼容判定依据修正为「安装时产品名（NSIS 卸载键）」而非版本号，并经本机 2.4.4 注册表实证（卸载键/安装目录均为「Trae Work 助手」）；README / AGENT.md / 本条目同步。
- 安装钩子 PREINSTALL 补充结束过渡版主进程 "AI Work 助手.exe"（防止其运行中锁住旧命名主程序清理）。
- 修正「注册任务」权限不足提示中的手动命令引号错误（`&`→`&&`、数据目录值补闭合引号，与实际 /TR 一致）。
- AGENT.md 三处与 `src-python/tests/test_proxy.py` 的环境变量名同步为 `AIWORKDATA_DIR`。

---

## [2.5.0] - 2026-09-06

功能版本：账号导入 + 导出优化 + 工具栏重排（提交 bddfdf0）。

### 变更

- 账号管理工具栏重排：右侧依次为 刷新数据 / 扫描本机 / OAuth 登录 / 添加账户 / 导出账户 / 导入账户 / 分组管理 / 快照管理，帮助按钮纯图标移至描述后（PageHeader 新增 leftExtra 插槽）。
- 导出账户增强：携带应用版本、dcId、addedAt，兜底导出视图外原始账号；新增「导入账户」按钮与 `accounts_import` 命令（三种格式兼容，uid+JWT 去重，分组按 id 合并）。
- 账号池预留记录数据中心级 id（icube-dc）：`RawAccount.DcID` + 切换 / 保存登录态时自动回填 + 批量回填命令。
- F-08 双应用账号自动发现修复：本机证据推导 Cloud-IDE uid（Trae CN 读 storage.json，SOLO 读 state.vscdb）；套餐到期时间从会员包 expire_time 提取。
- F-47 进程关闭等待缩短为 3s / 2s（轮询 250ms）。
- AGENT.md 新增 §15 版本升级规则（完整功能 = 中位 +1，修复 / 优化 / 微小 = 低位 +1）。

---

## [2.4.4] - 2026-08-16

维护版本：清理临时文档并同步版本号。

### 变更

- 删除临时问题分析报告 `docs/issue-analysis-2026-08-16.md`，其功能已由 `CHANGELOG.md` 与 `AGENT.md` 中的变更说明覆盖，避免重复维护。
- 版本号 2.4.3 → 2.4.4（`package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` 四处同步）。
- 同步更新 `README.md`、`AGENT.md`、`docs/user-manual.md`、`docs/tech-framework.md`、`docs/operation-manual.md` 中的版本标注，以及 `scripts/make_portable_zip.py` 的便携包文件名。

### 说明

- 本版本**无代码逻辑改动**，仅文档与版本号维护；v2.4.3 的代理/VPN 共存与定时任务修复保持有效。
- 若需重新打包安装包，仍须执行 `npm run tauri build`（Python 侧修复已随 v2.4.3 打包）。

---

## [2.4.3] - 2026-08-16

修复「开启本地代理后 GitHub / Google 打不开」与「定时签到注册·查询·取消无反应」两类问题。

### 修复

- **本地代理与 VPN 冲突导致外网无法访问**（`ERR_TUNNEL_CONNECTION_FAILED`）
  - 根因：`proxy_start` 把 Windows 系统代理**整体覆盖**为 `127.0.0.1:8899`，抹掉了 VPN（Clash / v2rayN 等本地 HTTP/SOCKS 代理）的接管点；而 `tunnel_raw()` 对非 Trae 域名使用 `socket.create_connection` **直连**上游，完全绕开 VPN，导致 GitHub / Google 被阻断，而 baidu / qq 等国内站点直连可达故始终正常。
  - 修复：引入**上游代理链式转发**。`proxy_start` 在改写系统代理**之前**先读取已有的系统代理配置，作为 `UPSTREAM_PROXY` 环境变量注入 Python 代理进程；`device_proxy.py` 新增 `_parse_upstream()` / `connect_via_upstream()`，支持 **HTTP CONNECT** 与 **SOCKS5**（含用户名密码认证）两类上游。`tunnel_raw()` 与明文 HTTP 转发路径对**非 Trae 域名**优先经上游（即 VPN）出站，上游不可用时自动回退直连。Trae 域名仍由本代理 MITM 解密以捕获 JWT。
- **CONNECT 隧道缺少握手应答**
  - `tunnel_raw()` 从未向客户端回送 `HTTP/1.1 200 Connection Established`，客户端因此永远不会发起 TLS 握手；上游不可达时也无任何应答，浏览器无限等待。现已补齐 `200` 握手，失败时回 `502 Bad Gateway`。
- **停止代理会破坏 VPN 设置**
  - `proxy_stop` 原先只是把 `ProxyEnable` 置 0。现改为**原样还原**启动前捕获的系统代理（含 `ProxyServer` 与 `ProxyOverride`），停止本地代理后 VPN 立即恢复可用。
- **计划任务查询结果中文乱码**
  - 根因：`schtasks` 的中文输出为 **GBK** 编码，Rust 侧用 `String::from_utf8_lossy` 按 UTF-8 解读，产生 mojibake（如 `ϵͳ�Ҳ���ָ�����ļ���`，实为「系统找不到指定的文件」）；乱码进一步导致「找不到」关键字匹配失效，无法命中「任务未注册」的友好分支。
  - 修复：新增 `run_schtasks()` 统一入口，前置 `chcp 65001` 强制 schtasks 以 UTF-8 输出，中文错误信息可正确解码与匹配。
- **错误提示前缀重复**
  - 原先 Rust 返回 `查询计划任务失败：…`，前端 `Settings.tsx` 又拼接 `查询失败：`，叠加成「查询失败：查询计划任务失败：…」。现 Rust 端只返回纯错误文案，前端前缀成为唯一前缀。
- **定时任务注册在普通用户下失败**
  - 移除 `schtasks /RL HIGHEST`（签到脚本只读写 `%APPDATA%` 并运行 Python，无需提权，强制最高权限会让普通用户卡在 Access Denied）；`/TR` 命令行改为 `cmd /c set "TRAEDATA_DIR=…" && "<python>" "<script>"`，对含空格的路径安全。
- **查询 / 取消操作静默吞错误**
  - `task_status` 原先无论成功失败都返回 `Ok(stdout)`，任务不存在时返回空串，界面显示空白；`task_unregister` 原先丢弃执行结果永远返回 `Ok(())`。现均真实上报结果：任务不存在时返回明确提示「未注册每日签到任务（请先在设置页点击「注册任务」）。」，取消时若任务本就不存在按已删除处理。

### 变更文件

| 文件 | 说明 |
|---|---|
| `src-python/device_proxy.py` | 上游代理链式转发（HTTP CONNECT / SOCKS5）、`tunnel_raw` 补 `200` 握手与 `502` 兜底 |
| `src-tauri/src/commands/proxy.rs` | 启动前捕获系统代理并注入 `UPSTREAM_PROXY`、停止时原样还原、抽出 `apply_proxy` / `get_existing_win_proxy` |
| `src-tauri/src/commands/misc.rs` | 新增 `run_schtasks()`（`chcp 65001`）、去重复前缀、移除 `/RL HIGHEST`、错误可见性增强 |
| `docs/issue-analysis-2026-08-16.md` | 新增问题深度分析报告（调用链、根因、修复、验证方法） |

### 升级注意

`device_proxy.py` 会被打包进安装包的 `resources/python/`，**代理相关修复必须重新执行 `npm run tauri build` 才会进入正式版**；开发模式 `npm run tauri dev` 直接读取 `src-python/`，重启代理即生效。

---

## [2.4.2] - 2026-08-15

### 修复
- 修复发布版黑框 / 闪退 / 排队提醒丢失等 GUI 失灵问题。
- 健康检查端点统一为 `/health`，文档英文化与路径清理。
- 移除 `proxy_logs` 目录引用，日志统一存放在 `logs/` 下。
- 全面修复文档错误；恢复误删的 `src-python/tests/test_auto_checkin.py`。

### 新增
- 便携版打包脚本 `scripts/make_portable_zip.py` / `scripts/package_portable.py`。

---

## [2.4.1] - 2026-08-14

### 变更
- 项目重命名为 `trae-work-assistant`，同步更新文档与用户手册。

### 新增
- 账号切换流程重构、保存登录态能力、帮助说明。

### 修复
- 日志相关问题修复。

---

## [2.4.0]

- API 服务页面重构、日志页面整合与 UI 优化。

## [2.3.0]

- API 服务协议对齐、代理修复、交互优化与日志增强。

## [2.2.0]

- 全面质量优化：Mutex 安全锁（poison 恢复）、竞态修复、暗色模式图表适配、积分三线趋势图。

## [2.0.0]

- 本地 API 网关（axum + ureq）、SSE 协议转换、账号池智能调度、签到错误冷却状态机、6 层设备标识重置。
