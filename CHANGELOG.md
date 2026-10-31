# 更新日志

本文件记录 AI Work Assistant（Web/Docker 镜像版）的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

> 分支说明：本分支（`docker_main`）为 Docker 镜像产品线（Web-only，axum 单体 + 浏览器管理面），与桌面版 `main` 分支不是同一产品形态；仅按提交语义移植系统无关的修复与功能。

---

## [1.5.11] · 2026-10-31 · 语义移植 main dfa6e02 => f53e213a

> 语义移植 main `dfa6e02`（不含）=> `f53e213a`（4 提交含 1 merge，未 merge）。跳过：版本号 chore（本分支版本自管）、AGENT.md 与 main 自身 CHANGELOG（文档）、桌面 OS 专属；上下文窗口全链路本分支 1.5.10 已先行实现（仅补 #163 审查修复的并发防护），f53e213a 的 custom_route mock 整读加固与解冻相对秒大写归一化本分支已自研等价实现（含回归测试），不重复移植。

### 新功能

- **各模型消耗趋势图（#160，ba326c7）**：用量看板「各模型消耗趋势」由 Top 排行卡升级为趋势折线图——区间内累计降序 Top8 + 其余聚合为「其他」，积分与 Token 双看板接入；趋势恒为全模型聚合，不随单模型筛选联动；内部序列键规避含 `.` 模型名被图表库按路径解析断链；附纯函数测试 7 条。
- **传输掐断聚合路径归因（PR #162，4a50304/002d407）**：非流式聚合（含 Anthropic 聚合）在完成事件前的任何读错误按传输掐断哨兵上抛，截断响应不再伪装成功；Trae 流式/聚合轮换耗尽收尾将掐断计数并入归因——全池被掐时错误文案指向「上游连接中断」（账号/模型无关，建议检查本地网络/代理）而非 "no healthy account"，日志计数对齐。
- **Qoder 长上下文写入门控 + 注入日志节流（f53e213a）**：上下文长度设置要求模型在 Qoder 目录内（resolve 与注入生效面同口径，拼错/下架即拒；清除不受门控，残留/超长条目仍可清理），模型 ID 入参限长 128 字节（仅拦写入）；注入日志按模型 60s 节流，高频流量不再把日志轮转挤掉其他诊断行。

### 修复与加固

- **账号级/模型级冷却解耦（#159，ba326c7）**：解析上游解冻文案得到账号解冻时刻（绝对时刻与相对秒双形态，钳制 [now+60s, now+7d]），限额类错误只冷却对应账号至解冻时刻，不再连带把整池同名模型钉入冷却（原缺陷：单账号限额 → 模型冷却表被钉到解冻时刻 → pool_health 跳过整个池 → 回退池无账号 → 503 no healthy）；503 归因补齐——真实根因为「前置候选池因模型级冷却被整池跳过」时显式注明，防误排账号/Key 配置。附回归测试 4 条。
- **WB 余额双口径去重（#120，ba326c7）**：积分余额改为优先读 summary 行，聚合父字段/无 code 汇总行与明细包并存时同值（浮点尾数近似比较）只计其一，不再重复计数；明细读不出时宁可漏算不回退「全量和」；附 issue #120 评论区实测形态回归锁定。
- **传输掐断计数接入 qoder/wb 耗尽归因（f53e213a）**：`cut_hits` 接入 Qoder 流式/聚合 4 处、WB 流式/聚合 2 处轮换收尾——全池被掐时错误文案报传输掐断而非误报 "no healthy account"；耗尽文案按池标注（`Trae shadow` → `{pool} shadow`），qoder/wb 复用收尾不再误标 Trae。
- **503 冷却归因冷却键候选化（f53e213a）**：`skipped_pool_note` 归因冷却键由单一请求名扩为候选列表（请求名 + wb 路由管线归一名 + Qoder 目录 id，取最大剩余秒），修复别名改写场景静默漏归因。
- **长上下文/上下文长度前端并发防护（#163 审查修复，ba326c7）**：Trae 长上下文开关——目录刷新的整表快照在 toggle 在途期间跳过覆盖（在途镜像 ref），toggle 落库后使先前发出的快照作废（序号 TOCTOU），防旧快照回退刚勾选的项；Qoder 上下文长度——页面刷新与弹框保存后的后台对齐共用请求序号，仅应用最后一次发起的结果。
- **Key 展示名空回退改 unnamed（f53e213a）**：NoHealthy 详情展示名缺失时回退固定占位符，API Key 原文不再拼进 503 响应体/日志。
- **长上下文开关加载失败提示（f53e213a）**：开关列首次加载失败整列禁用时提示一次，不再静默禁用让用户无从知晓原因；toggle 落库后保留后台对齐整表收敛。

### 验证

- `cargo test --workspace`（aiwork-core 897 通过 / aiwork-server 13 通过）、`npx tsc --noEmit`、`npm test`（88 通过，含新增 modelTrend 7 用例）全部通过。

## [1.5.10] · 2026-10-31 · 按模型上下文窗口设置（#163/#165）+ 上游断连死循环修复（#164）

> 合并 PR #165 按模型上下文窗口设置，纳入 Docker 用户实战报告的断连死循环紧急修复（issue #164），另修配置缓存与静态缓存两处基础设施问题。main 分支无对应提交，本条目为本分支自管记录。

### 新功能

- **按模型上下文窗口设置（issue #163，PR #165）**：Trae 按模型勾选长上下文（注入 `is_max_mode:true`，11 模型支持表门控）、Qoder 按模型设置数值（下发 `parameters.context_length`，流式与聚合全覆盖）；WorkBuddy 客户端无请求侧字段，明确不提供。未设置的模型请求逐字节保持现状。
- **管理面口径澄清（issue #163）**：模型目录「默认上下文」列改名并补 tooltip，防静态声明值被误解为按模型可配置的生效值。

### 修复与加固

- **上游中途断连双线程死循环（issue #164）**：对端 FIN 后 read 恒立即报错，`relay_lines` 转发线程与对冲路径吞 Err 的 `chain_rest` 互相泵错误形成双线程死循环——容器 CPU 恒烧 2 核不自愈，伴生 CLOSE-WAIT fd 泄漏与对冲账号租约僵死。修复：读错误转发一次后立即退出（错误仍达折叠点，掐断可见性不变），reader 随线程退出关闭 fd；与 1.5.9 消费侧折叠（#158 P2-2）形成双保险，全池受益。附回归测试 2 条。
- **Qoder 400 拒绝 context_length 如实分类（issue #163）**：按业务拒绝分类，不再误判 Quota 冷却健康账号；信封 400 终局处理不换号不冷却，错误信息附清空设置指引。
- **config_cache 每键代际校验**：迟到回填不再覆盖失效后的新值，修复「保存成功但下一请求仍读旧配置」。
- **静态资源缓存头**：`/assets/*` 长缓存 immutable，其余响应 `no-store`，杜绝部署后旧 chunk 被继续引用。

### 验证

- `cargo test --workspace`（898 通过 0 失败，含 issue #164 回归测试 2 条）、`npx tsc --noEmit`、`npm test`（81 通过）全部通过。

## [1.5.9] · 2026-10-30 · 语义移植 main f04d495 => dfa6e02

> 语义移植 main `f04d495`（不含）=> `dfa6e02`（7 提交，未 merge）。跳过桌面/OS 专属或本分支不适用内容：托盘与通知渠道系统（main.rs/notify.rs/PushPlus，含 dfa6e02 托盘 push_notify_bg 与 workbuddy push_notify 后台线程版重构——本分支 Web 化已移除桌面通知栈）、豆包存储链路（commands/doubao.rs）、桌面浏览器打开（Web 化已移除）、QoderWork 快照 .bak 回退（无 switcher 模块）、版本号 chore（本分支版本自管）。

### 新功能

- **多客户端指纹布防扩充（#150/#155）**：七款客户端最新请求形态补齐身份句整句改写与 aggressive 兜底，残指纹特征表扩充；ZCode 双身份句实证布防；工具别名内置表扩至 41 条（请求侧改写、响应侧回改）。
- **Qoder 池指纹清洗管线（#158 P1）**：清洗与工具别名接入 Qoder 池（清洗打副本、原始 peek 留记账与图片门控；会话稳定种子换号递增逃逸方差）；池级开关与 wb 池独立热切换；指纹拦截短记忆对齐 trae/buddy 池。
- **传输层掐断可见化（#158 P2）**：掐断哨兵 `-9902` 全池接线——不冷却账号/模型、不解绑粘性、不计空完成指纹；有内容就地收尾落账失败、零内容换号；非流式聚合丢弃截断响应自动换号。
- **Qoder 轮换预算（#156）**：45s 轮换总预算，超预算按 exhaust 语义收尾错误必达；首字超时补记模型级 Server 冷却。
- **图片输入适配（#154）**：Anthropic image 与 Responses input_image 统一映射 OpenAI image_url；Qoder 上游不支持图片明示 400 拒绝，不静默丢图。

### 修复与加固

- **聚合侧传输掐断补齐（#158 P2-1，dfa6e02）**：Trae PlainCollect 留存传输掐断读错误，aggregate/aggregate_text 上抛 `-9902` 哨兵——半截内容不再以 `finish_reason="stop"` 伪装成功；routes 聚合预检丢弃截断响应换号，不冷却不解绑；custom 流式/聚合掐断分支补 `set_last_error` 对齐观测口径。
- **Qoder 聚合侧轮换预算与双败冷却（#156，dfa6e02）**：聚合路径补 45s 轮换预算（超预算按 exhaust 语义收尾 + 非指纹主导补模型级冷却对齐流式）；竞速双败补记模型级 Server 冷却。
- **Qoder 耗尽错误帧按协议分流（dfa6e02）**：流式轮换耗尽/预算收尾错误帧按客户端协议分流（Anthropic `event: error`、Responses `response.failed`），`send_stream_error_named` 保持字符串 code `"no_healthy_account"` 客户端兼容。
- **Anthropic 图片 MIME 白名单（dfa6e02）**：base64 内嵌图 `media_type` 白名单校验（png/jpeg/webp/gif），白名单外丢块不透传上游，防异常 MIME 注入 data: URL 前缀。
- **WB 刷新解码失败如实上抛（dfa6e02）**：CN/intl 刷新响应解码失败不再 `unwrap_or_default` 落 Null 伪装成「无 accessToken」误导重新登录。
- **11128 强制清洗激进口径贯穿（dc8a382）**：主/对冲同强度不回落重算；轮换耗尽文案指路工具别名 tab；content_filter→refusal 语义归位。
- **用量统计契约对齐（dc8a382）**：UsageDayView/mergeDay 补 cached_tokens 与 p50，明细失败独立错误态、DST 与 sessPage 回写。
- **UI 全局最小逻辑宽度（#152）**：`--app-min-width` 变量 + 整体横向滚动，Sidebar sticky 常驻；仅 ≥768px 桌面视口生效（本分支 Web 化无 TitleBar，main 口径的「TitleBar 固定全宽」不适用）。
- **vault 故障不销毁凭证（#153）**：写失败不落库直接返 Err，数据库保持原状；`vault_down` 暂态防误判永久禁用；data_dir canonicalize Junction 提示与日志去重。
- **Qoder 免费模型与文案（#156）**：DeepSeek-Flash 纳入 0 倍率免费活动；积分耗尽文案明示次日 04:00 恢复且已禁用账号不计入；429 冷却补处理指引。

### 验证

- `cargo test --workspace`（873 通过）、`npx tsc --noEmit`、`npm test`（81 通过）全部通过。

## [1.5.8] · 2026-10-27 · 语义移植 main aa4ec5a => f04d495

> 语义移植 main `aa4ec5a`（不含）=> `f04d495`（9 提交，未 merge）。跳过：切换器链路（095e3ca 部分、84a5402、c440b20，本分支无 switcher 模块）与豆包桌面端开关（c097170，本分支无 doubao 模块）。

### 新功能

- **渠道内单模型开关（#145）**：模型可按源启用/禁用（管理端手选 + 「已禁」徽标），dispatch 不再路由被禁源，网关目录全禁条目不发布。
- **付费受限免误冷却（#145）**：新增 `PaidRestricted` 错误分类——免费账号打付费模型不再触发冷却/禁用/熔断（仅换号或 fail-fast）。
- **自定义池会话明细（#149）**：请求级本地流水表（会话指纹 + 请求阶段 + 消息预览，90 天保留）；用量统计「自定义模型」新增按会话归并的明细表，可展开看各轮。
- **工具名别名改写（#150）**：破解按 `tools` 工具名集合识别客户端的指纹风控——请求侧改名、响应侧自动回改（流式/聚合/Anthropic 双形态 + 对冲/重试同步重放）；管理面新增「工具别名」tab（内置默认表 + 自定义覆盖/停用，保存即生效）。

### 修复

- **finish_reason 误标 stop（#148）**：带工具调用的完成帧统一升级为 `tool_calls`，严格客户端不再取消 pending 工具。
- **Qoder 凭证续期（#143）**：兼容 `device_token`/`expires_at` 响应形态，修复手动续期恒报「响应结构异常」。
- **元数据编辑弹框串值（#146）**：快速连点模型时慢响应覆盖/残留旧值——序号令牌 + 回填期清空禁用；指纹清洗面板规则行改稳定 key。

### 验证

- `cargo test --workspace`（834 通过）、`npx tsc --noEmit`、`npm test`（81 通过）全部通过。

## [1.5.7] · 2026-10-25 · Qoder 登录直连 SSO + 签到盲发兜底 + vault 加固

> 语义移植 main `2e02cfc`（不含）=> `aa4ec5a`（3 提交，未 merge）。跳过（OS/桌面专属或本分支不适用）：2fe9396 `commands/env.rs`（Windows 注册表安装检测 WOW6432Node，桌面探测模块本分支无此文件）与 `build-assets/installer-hooks.nsh`（NSIS 安装器注释，本分支无 build-assets）；aa4ec5a vault DPAPI 双格式自适应与 `AIWORK_VAULT_SHARED` 一次性迁移（桌面侧特性——本分支 vault 即 V2: hex 格式源头，DPAPI blob 维持明确迁移报错）、`commands/qoder/oauth.rs` 桌面 open_in_browser（Web 化已移除，授权页由前端 window.open）、AGENT.md / docs/tech-framework.md 的 Qoder OAuth 行（本分支无对应行）、版本号 chore（main 3.8.4，本分支版本自管）。74b4d55（issue #138）与本分支 1.5.5 的 3349a9e 为同题异构实现，仅补移植其独有增量。

### 修复与加固

- **粘性显式键限长 128 字符（74b4d55）**：`conversation_id` 超长值进 sticky 表会放大存储体积——from_body / from_slice 两构造点统一经 `SessionKey::explicit` 按 chars 截断 128，WB/Qoder 两侧确定性截断保证匹配一致性（附回归测试）。
- **组/标签约束 no_healthy 提示分流（2fe9396）**：org 约束（展开条目必带池前缀）交集为空时，原「账号约束均指向其他资源池」提示与本池即该池的事实同句矛盾——改给「组/标签约束与本池账号交集为空」口径提示。
- **custom_model_test 预检复用 norm_base（2fe9396）**：与保存侧 upsert 完全同口径（scheme 小写 + 剥默认端口 443/80 + 去尾斜杠），修复 `HTTPS://x:443/` 能保存、探活却被大小写敏感 starts_with 拒绝的两侧口径分裂。
- **11140 禁用语义注释纠偏（2fe9396）**：mod.rs / retry.rs / wb_route.rs 三处「申诉成功后 UI 手动启用恢复」纠正为「禁用为内存态，无手动启用入口——重启或重新登录即随池快照重建恢复调度」。
- **Qoder 登录直连 SSO（aa4ec5a issue #140）**：设备授权页自构 SSO 跳转会向 oauth_callback 写入显式 `:443` 端口，被 SSO 回调白名单拒绝（未登录用户经授权页登录必现 400）——改为直连 `qoder.cn/sso/login/aliyun?oauth_callback=<全量百分号编码的设备授权页（附 directLogin=true）>`；oauth_callback 全量编码、poll verifier 转义改 `urlencoding::encode`（删除本地最小转义 urlencode）；Web 端提示文案「授权页」→「登录页」。
- **签到空表盲发兜底（aa4ec5a issue #141）**：活动列表对账号不可见（empty_campaigns）时先盲发试领（claim 端点不受列表过滤约束且幂等），命中即真实领取/回放确认；fail_kind=empty_campaigns 语义保持（原消息在前、兜底详情追加）；四处兜底 match 收敛为 `run_fallback` 共用。
- **到期列表剔除长期有效哨兵（aa4ec5a issue #141）**：Qoder 到期列表对 9999-12-31 哨兵包按 `PERPETUAL_TS` 口径剔除（包到期与 Plan 订阅两处），防「最近到期」显示 9999/12/31。
- **vault 加固（aa4ec5a issue #139）**：密钥文件 hex 全量校验（多字节 UTF-8 凑足 64 字节时旧代码按字节切片会 char boundary panic，收敛为友好 Err，附 panic 回归测试）；快照存在但 client 加载失败时拒绝打开（防密钥/快照错配时空 client 经 save() 覆盖原快照）。

### 验证

- `cargo test --workspace`（803 通过 0 失败，含本轮新增 wb_sticky 截断 / qoder_oauth SSO / vault hex 校验用例）、`npx tsc --noEmit`、`npm test`（81 通过）全部通过。

## [1.5.6] · 2026-10-24 · 调度归因可观测 + 11140 账号级风控识别

> 语义移植 main `21fc6fb`（不含）=> `2e02cfc`（9 提交，未 merge）。跳过 NSIS/更新链路、certutil/reg.exe/netstat 进程内化等 OS/桌面客户端专属改动（仅取其中调度配置弹框宽度一行）；main 自有 CHANGELOG 与版本 chore 由本分支自管版本替代。

### 修复与加固

- **识别上游 11140 账号级风控并自动换号（issue #136）**：上游「账号存在异常行为，已被限制使用」此前零识别，受限账号持续被调度每请求必败。现按账号级风控准确识别（与 11128 渠道级指纹拦截相反，换号有意义）：未产出内容时自动换号接替，并自动禁用待申诉，申诉成功后手动启用即恢复；与 11128 双向不误撞。
- **调度失败逐池归因与提示分流（issue #136）**：调度耗尽时逐池落归因日志（不可用 / 无可用账号 / 模型冷却），白名单排除与账号不健康直接可辨；「无可用上游」提示按根因分流——白名单条目全指向其他资源池与白名单指向的本池账号均不可用分别给出针对性提示，消除跨池错位误导排障；相同归因 60 秒内去重防刷屏。
- **稳定性审查批次**：custom 池 Key 白名单三处口径统一（白名单外条目视同不存在，组内全排除自动跨池回退而非执行侧报错）；流内失败不再误判记账成功；上游连通性测试改独立短超时连接，不复用主路径连接池；base_url 归一化（scheme 大小写 / 默认端口变体）杜绝绕过查重，编辑保存自动清熔断；侧边栏应用顺序入库前合法性校验（非法 key 过滤 + 去重保序，前后端同口径）；调度配置弹框限定宽度。

## [1.5.5] · 2026-10-23 · 常驻内存治理（issue #138：2核2G RSS 驻留 512MiB 被 earlyoom 反复 SIGTERM）

### 修复与加固

- **peek DOM 窄探测化**：会话粘性键改 `SessionKey::from_slice` 流式窄探测（仅目标字段反序列化，不建整包 DOM），WB peek 改作用域块「用完即弃」——消除大 body 下 DOM 放大 3~6 倍的驻留大头。
- **对冲大 body 门槛**：Trae/WB/Qoder 三池首字对冲在 body > 256KiB 时退化为纯首字超时（语义与关闭对冲一致），消除全量快照 + 整包重建的瞬时内存放大。
- **jemalloc 全局分配器**：Linux gnu 目标换 tikv-jemallocator，容器注入 `_RJEM_MALLOC_CONF` 后台归还页——治 glibc 碎片导致的「RSS 只涨不降」。
- **请求体上限环境变量化**：默认 32MiB → 8MiB（`AIWORK_MAX_BODY_BYTES` 可调，clamp [256KiB, 64MiB]）。**升级提示**：长上下文/图片 base64 客户端若升级后遇 413（`request body exceeds 8MB limit`），设 `AIWORK_MAX_BODY_BYTES=33554432` 可恢复 1.5.4 的 32MiB 行为。

### 新增

- **/health 内存探针**：响应新增 `rss_kib`（仅 Linux）与 `inflight`（在途请求数），容器内存告警/OOM 排查可直接 curl。

### 验证

- `cargo check` + `cargo test --workspace` 通过（795 通过 0 失败）；Linux gnu 侧 jemalloc 由 Docker 构建覆盖。

## [1.5.4] · 2026-10-22 · Custom 模型上游组（第五资源池）+ 侧边栏应用拖拽排序

> 语义移植 main `f4888f6` => `21fc6fb`（11 提交，未 merge）。跳过：d34a99d 桌面更新器/NSIS（OS 专属）；61bd1ab / 21fc6fb 为 main 自有 CHANGELOG（本文件替代）；f8f1bce 为 main 版本 chore（本分支自管 1.5.4）；2079d8c 为 merge 提交。

### 新增

- **Custom 模型上游组**：`custom_models.json` 同名多条即上游组（weight 负载权重 / standby 备胎、完全重复校验、目录聚合合并展示、CustomModelsPanel 分组 UI）。
- **上游组负载均衡与实例级熔断（custom_lb）**：加权轮询 + 主备顺延 + 会话粘性；渐进熔断 30s×2ⁿ 封顶 480s（401/403/402 长 600s）、3s 快速失败；组内全熔断自动跨池兜底；`custom_lb_status` 可观测。
- **跨池整合**：Custom 作为第五资源池参与统一调度——前缀锁池 / Key 绑定池 / `per_model` 写 `"custom"` / Smart 倍率 / 别名链路全部生效；纯 custom 模型唯一源直达，混合场景候选序尾部兜底。
- **侧边栏应用拖拽排序**：系统设置「应用显示」新增拖拽抓手实时换位，`app_order` 持久化（后端保序去重，未提及项按默认顺序补尾）；Sidebar Tab 按新顺序展示，网格最多 4 列（>4 折两行）。
- **测试与文档**：端到端轮换 / 熔断窗口 / Smart 免费胜出 / 回退方向 / 目录聚合新用例；[DEBUG] UPSTREAM_GROUP_EXHAUSTED 排障行；tech-framework §6.1 改写「多池与统一目录」（补录 qoder 池行）。

### 修复与加固

- 统一目录组倍率只统计 enabled 条目（disabled 免费(0) 不再压低组内最小倍率）；aggregate 排障行去重；模块注释同步 v3 语义。
- **移植审查修复**：`parse_bind_pool` 补 `buddy-ai` 臂（移植漏臂致 BuddyAI 池绑定静默失效、组织预览错池，dispatch 消费分支成死代码）；非流式组耗尽兜底日志 502 对齐客户端实收（stream 路径本为 503 自洽）。
- **移植后全面审查修复（2026-10-23）**：① 统一目录命中侧移除已废除的 custom 短路，改为与 dispatch 候选序一致（per_model/优先级序 + 缺池尾部补齐），custom 侧 rate/context/max_tokens/supports_image 全部改由命中侧回选（合并臂不再无条件覆盖），消除「目录展示取 custom 侧、实际调度走 Trae/Buddy」的口径错位；Smart 重排为请求期动态口径、目录不镜像（既有取舍，注释如实声明）；全 disabled 组不发布 vendor/上下文/多模态声明（多源与纯 custom 条目同口径）；② custom_lb Auth 长窗记账清零渐进片段计数（长窗过期后 Server 失败从 30s 基准重起步，不继承既有 fails，新增 t05c 回归）；③ custom_models upsert/remove 读-改-写持专用互斥锁（对齐 POOL_CFG_LOCK 范式，防并发丢更新）+ base_url 信任边界声明（有意放行 127.0.0.1 本机推理服务）；④ 模型探活 agent 空闲读缩至 30s（spawn_blocking 线程最长占用 ~63s 上界）；⑤ aggregate 逐 attempt 日志状态码约定注释固化；⑥ 前端拖拽改 Pointer Events（触摸端可用 + pointercancel 兜底）、补左键校验与两列布局水平边界判定、抓手键盘 ↑/↓ 换位 + aria、分组折叠行键盘/aria-expanded；⑦ 编辑模式连通性三要素（名/地址/Key）未变更时跳过强制测试（上游临时宕机仍可保存修改）；⑧ 上游组成员删除确认文案区分组内语义（剩余 N-1 条继续承接）；⑨ ResourceSummary 调度语义文案对齐 v3；⑩ weight≤1 渲染兜底不再显示「权重×0」；⑪ AGENT.md `custom_models_*` 命令前缀修正。

### 验证

- `cargo test --workspace` 793 通过（7 忽略，含新增 t05c 回归）· `tsc --noEmit` 0 错误 · `vitest` 76 通过。

## [1.5.3] · 2026-10-22 · WB 轮次锁持有者标签 + 后台轮次事件转发 + 当日全败门禁

> 语义移植 main `f4888f6`（未 merge）。桌面/OS 专属跳过：Windows 命名互斥体跨进程互斥（#134 方案4，docker 单进程无 GUI/托盘/CLI 多入口，进程内锁已覆盖全部入口）；Qoder campaigns 空表最小头复取已随 #133 批次等价落地，`6dd9ff3` 版本 chore 不适用。

### 新增

- **WB 轮次锁持有者标签（issue #134 方案2）**：UI/调度器抢锁登记持有者标签与时刻，争抢报错携带「谁在跑、跑了多久」（不足 1 分钟显示「刚启动」）；持有超 15 分钟疑似卡死落日志告警一次（防重复）；锁释放自动清登记（守卫 Drop，panic 中毒兜底）。
- **后台轮次事件转发（issue #134 方案1）**：调度器 wb-checkin/wb-growth 轮次同步转发 wb-checkin-progress 至事件管线（SSE 下发浏览器），前端运行态同步置位——不再只由页面点击驱动，后台在跑时用户点击即见原因。

### 修复与加固

- **当日连续整轮全败门禁（issue #134 方案3）**：wb-checkin/wb-growth 连续 3 轮全败当日暂停重试链（返 Ok + mark_run 记「今日已处理」，终止全天退避空烧与反复撞锁噪声），任一成功清零次日恢复；签到/成长独立 kv 计数、计数与日期同文档单次写入防半写分叉、跨日自动重置；连击推导复用 buddy-ai 同款纯函数（空轮原值不动）。
- **移植后全面审查修复（6 项）**：门禁 skip 返回体补 `summary`（否则「当日暂停」通知正文沦为原始 JSON）；UI 手动轮次接入连击同步（手动成功清零调度器门禁连击、手动全败计入连击，与调度器共用纯函数与 kv 键）；手动轮次 panic 兜底（catch_unwind 落日志，exit 复位事件始终发出，防前端运行态永挂）；空池空转 done(0,0,0) 静默 toast（不再弹「成功 0」噪声）；连击持久化/跨日/损坏文档补单测；两分支事件转发闭包提取共用 helper。
- **移植后全面审查修复·第二轮（3 项）**：后台轮次 panic 兜底补齐（调度器 wb-checkin/wb-growth 轮次中途 panic 时 start 已转发而 done/exit 永不发出，前端运行态永久悬挂——tick 层 catch_unwind 拿不到轮次事件句柄，改为轮次现场 catch_unwind + exit 补发，panic 返 Err 交冷却重试）；后台成长轮空池空转不再弹「成长中心执行完成」toast（对齐 No.6 口径，前端按 start total 静默）；门禁暂停轮通知标题由「完成」改「暂停」（原标题与正文互相矛盾）。

### 验证

- `cargo test --workspace` 771 通过（7 忽略）· `tsc --noEmit` 0 错误 · `vitest` 76 通过。

## [1.5.2] · 2026-10-22 · 指纹清洗增强与 11128/空完成拦截链

> 语义移植 main `5c4014f`→`origin/main`（5 提交，未 merge）。桌面专属跳过：WorkBuddy 本地路径/注册表指纹解析、数据目录注入、Trae 钥匙「启动代理 + 切换账号被动捕获」预检及其 UI。

### 新增

- **指纹清洗方案 C（issue #122）**：规则 `to` 支持 `|` 多候选按请求种子轮换；空完成 / 11128 重试追加激进短前缀规则；改写后首条 system 残指纹自检。
- **池级拦截短记忆**：空完成主导收尾标记该池，10 分钟内同池新请求直接激进起步（不再从零试探烧账号）。
- **指纹样本环形缓冲（issue #125）**：11128 与空完成采样入内存（上限 50、去重），「指纹清洗」tab 一键生成规则草案。
- **会话头三件套（dsh-xdpool#58）**：chat 请求必带 `X-Conversation-ID/Request-ID/Message-ID`，国际版网关缺一即 11128。
- **档位分区清洗（issue #129）**：模板表按 `wb` / `buddy-ai` 两档 KV 隔离（内置默认共享），面板内切换。

### 修复与加固

- **残指纹与请求规范**：OpenCode/DeepSeek 实证句改整句同义改写；非小写 role 归一、`developer`→`system`（上游不认 developer）。
- **11128 全形态处置**：Trae/WB 流内 11128 就地激进清洗重试（toolexec / aggregate / stream）。
- **空完成判定（issue #130）**：补最小样本 `rotated>=2` 防误报；`no_healthy` 诊断补「约束排除全部健康账号」hint。
- **积分解析（issue #120）**：跳过无 code 的分类汇总行与聚合父容器，保留未打 code 真实包，明细读不出不回退全量和。
- **签到 AuthRejected（issue #128）**：服务端拒绝认证归当日不可恢复——红色徽标 + 恢复指引，重试筛选与调度永久失败剔除。
- **审查修复**：池级拦截短记忆时钟不可用时不再误判「刚拦截」（负差值会导致全池永久激进清洗）；样本面板按来源池归属档位（跨档位/qoder 样本禁生成规则）、切档与清空补二次确认。

### 验证

- `cargo test --workspace` 769 通过（7 忽略）· `tsc --noEmit` 0 错误 · `vitest` 74 通过。

## [1.5.1] · 2026-10-20 · 移植 main 审查修复批 + 写权竞态加固

> 移植 main `5f88dfe`→`5c4014f`（4 提交：六轮全面审查修复、14 项审查修复、复审修复、死代码清理），非 OS 依赖语义移植，未 merge；应用显隐等口径按 docker 后端对齐，桌面/OS 专属跳过。

### 修复与加固

- **网关**：buddy-ai 国际档折入仅开启生效（`buddy_in_both`），CN 专属模型不再静默切国际上游；模型级冷却按 TargetPool 归因真实池；WbDisabled 按 `available` 判定；非幂等建会话 POST 不重试；4xx 响应体脱敏（补 sessionId）；responses 429 冷却改视图表；Buddy 池降级候选取 CN∩Intl 交集。
- **BuddyAI 链路**：凭证导出强制口令加密（AIWQENC1：Argon2id + AES-256-GCM，明文旧文件兼容导入）；打卡到账回写运行期网关池、空轮不固化当日已跑（`skipped_empty_pool`）；renew 永久失效连击熔断（3 轮当日跳过、跨日重置）；`import_creds` 拒 CN token 入国际池；账号库 id 白名单 `ensure_uid_safe`。
- **调度与部署**：`buddy_ai_*_hhmm` 补 settings 回退与非法值二级回退；用量跨日归因；`AIWORK_DISABLE_LOCAL_QUOTA` 免容器本地 quota 空探测；OAuth 外置配置优先 `AIWORK_DATA_DIR`；签到间隔 `effective_checkin_gap` 统一。
- **前端**：BuddyAiCredits 载入/同步改双守卫（写权版本门控，失败不占号）；saveFlags 失败按序号分流上报（被取代静默 + 诊断日志）；app_icons 白名单补 buddy-ai（Globe）；GatewayHelpModal 复制定时器卸载清理；ResourceSummary 保存成功后拉取最新池；应用显隐口径对齐后端（hidden_apps 3 键 / 图标 4 键）。
- **杂项**：清理死代码 `model_cooling_remaining_wb`；凭证导入空 accessToken 500 改业务校验；qoder 会话说明剥离注入标签；模型 ID 归一化保留小数点。

### 验证

- `cargo test --workspace` 741 通过（7 忽略）· `tsc` 0 错误 · `vitest` 68 通过。

## [1.5.0] · 2026-10-19 · BuddyAI 国际池五池化 + 审查修复

> 手工语义移植 main `f5f2cd3`→`origin/main`（28+2 提交），桌面/OS 专属跳过；「应用显隐联动」以 Web 侧 `appVisibility` 池级显隐落地（隐藏仅移除界面入口，调度/记账/绑定继续生效）。

### 新增

- **BuddyAI 国际版独立池（五池化）**：账号导入/导出/迁移/启停/备注/分组；ACP 六步链打卡与积分三表记账（90 天滚动）；Intl 目录（静态 17 模型 + 官网动态抓取，静态兜底合并）；dispatch Intl 判定动态优先/静态兜底 + 粘性 TTL；`pool_set` 扩 `buddyAiStrategy`。
- **调度任务四件**：打卡 / 目录同步（05:55）/ 续期（lazy 48h）/ 积分同步（23:45）；启动迁移将全局账号幂等入池。
- **前端**：BuddyAI 七页；资源调度中心五池化（五池全排列预设、BuddyAI 摘要卡、poolMetrics 七参、dispatchPresets 五预设）；cmd_bridge 20 命令。

### 修复与加固

- **网关分发（5）**：Intl 目录折入仅国际档开启生效；国际池加 CN 门控（`buddy_in_both`），CN 专属模型不静默切国际上游；WbDisabled 改按 `available` 判定（恢复 400 而非 503）；冷却错误归真实池；`dispatch exhausted` 补排障快照字段。
- **BuddyAI 后端（6）**：导入后网关热重载（先释放池锁，仅实际变更触发）；凭证回灌读 token store 现状，防过期副本覆盖已轮换 refresh_token；打卡删号保护区分停用/移除（`buddy_ai_pool_contains`）；跨进程锁失败按因分流归因；轮询确定性 4xx（除 408/429）早退；永久失败连续 3 轮当日暂停（`buddy_ai_checkin_fail_streak`，跨日重置）。
- **安全（4）**：含凭证导出强制口令加密（AIWQENC1 信封，Argon2id + AES-256-GCM，对齐 Qoder 口径，元数据导出保持明文）；非环回监听忽略 `web_auth_disabled`（管理面门禁不豁免）；外置 OAuth 配置优先 `AIWORK_DATA_DIR`（容器不再恒回落内置值）；`AIWORK_DISABLE_LOCAL_QUOTA` 免容器本地 quota 空探测（Dockerfile/compose 默认开）。
- **调度与用量（3）**：`buddy_ai_*_hhmm` 设置补 settings 回退使调度时刻真实生效；用量跨日归因（`record_cached_in_on` 调用方持锁取日）；BuddyAI 打卡永久失败不再全天 30min 噪声冷却。
- **风控与解析（4）**：issue #120 总余额翻倍修复 + coded 判定严格化（`fs_utils::dig` 深搜误判致父/子双算）；issue #118 Buddy refresh 三类归因；issue #119 Qoder BLOCKED 指纹信任分桶；issue #109 三键回写补校验；`credits.rs` 域名单点化。
- **前端与杂项（4）**：调度策略读取失败改错误态 + 重试（不再静默消失）；凭证导入空 AT 500 改业务校验（`opt_arg`）；BuddyAI/Buddy 文案对齐真实入口（清理「扫描本机账号」等残留）；`/v1/responses` Intl 分流边界补注释明示（不改行为）。

### 验证

- `cargo test --workspace` 739 通过（7 忽略）· `tsc` 0 错误 · `vitest` 193 通过（含 dispatchPresets 五池 10 例 + poolMetrics 15 例）。

## [1.4.17] · 2026-10-16 · 移植 main：Flash 免费豁免 + 积分三键拨新 + 审查修复批

> 合并点 `main@b0f81db` → `main@f5f2cd3`（16 个提交，手工语义移植，未 merge）。桌面/OS 专属（计划任务 schtasks、TraeWork 迁移引导）、CI（@tauri-apps/cli 回钉）、纯文档跳过；5468e05/541f33b/d9bf500 已随前次移植包含。

### 新增

- **Qwen3.8-Flash 免费活动倍率覆盖（issue #111）**：上游免费活动期实际不扣分但目录价不反映免费，零积分账号被积分过滤挡在模型外。新增 `entry_is_free`（倍率 0 判免费，credits 缺失 fail-closed）+ `apply_flash_free_override`（条目倍率改写 0，容忍命名漂移）；取号链新增 `skip_credit_filters`——免费模型豁免积分维度过滤（账号级状态过滤不豁免，Trae/WB 恒 false 零行为变化）；免费档消耗按 0 落账；目录倍率展示 0×；`api_pool.json` 新增 `qoder_flash_free_rate`（默认开，serde default 兼容旧文件）+ 热应用 + 资源调度页开关。

### 修复与加固

- **运行期积分三键拨新（issue #109）**：Trae/WB 池回写此前只拨余额两键，`credits_expire_at` 钉死装配时刻——expire_first 下新登录账号被恒压、充值续期进不了池。退役两键 `update_credits` 统一 `update_credits_snapshot` 三键；`push_credits_if_running` 到期对齐装配口径；`write_back_pool_balances` 增加运行池句柄（docker 适配 `Arc<ApiSharedState>`，锁外拨新）即时生效。
- **审查修复批（①②③④）**：记账宽口径 `u64_pair`（字符串/浮点形态不再静默得 0）+ toolexec 失败终局保留真实消耗；SSE 双完成/第三守卫（终态幂等）；kv 迁移原子化（tmp+rename）+ no-clobber；断连 usage 补记 + cached_tokens 旁路记账；healthz 三池逐池判定；签到奖励运行期回写 `apply_credits_delta`；qoder_credits 混合账户到期口径。
- **签到链（issue #110/#114）**：设备当日已签错误转译（归 Already 并排除出重试轮）+ 签到前设备冲突预警（`checkin_device_conflicts` 命令 + 前端提示）；claim 请求统一最小头形态（风控按形态拦截）。

### 发布前审查修复

- **版本升级引入 UTF-8 BOM（审查发现）**：`package.json`、`crates/aiwork-core/Cargo.toml`、`crates/aiwork-server/Cargo.toml` 三文件被写入 UTF-8 BOM（`EF BB BF`，字节级定界 7d66e6c 无 / e8e343f 起）。vite PostCSS 配置加载按原始字节 `JSON.parse`，BOM 致 `vitest`/`vite build` 全量失败；toml 解析器容忍 BOM 故 cargo 侧未暴露（更隐蔽）。已剥除；1.4.14 曾发生同类问题（530ec1e），版本升级写入链路需加 BOM 校验。无运行时语义变化，纯构建/测试链修复。

### 验证

- `cargo test --workspace` 701+12 通过（7 忽略）· `tsc` 0 错误 · `vitest` 61 通过（BOM 剥除后实测）。

---

## [1.4.16] · 2026-10-14 · 多架构 Docker 镜像发布

### 新增

- **多架构 Docker 镜像（issue #107）**：镜像同时发布 `linux/amd64` + `linux/arm64`。arm64 走 GitHub public 仓库免费原生 runner（`ubuntu-24.04-arm`），非 QEMU 模拟（Rust + rusqlite C 编译在模拟下极慢）；两平台并行按 digest 推送，末尾合并为多架构 manifest 并 inspect 校验。Dockerfile 零改动（基础镜像均官方多架构），GHA 缓存按平台隔离 scope。
- **首跑修复（CI）**：push-by-digest 的镜像名不经 metadata-action（其才做小写化），`smart-open/TraeWorkAssistant` 大写仓库名被 buildx 拒绝（`invalid reference format: must be lowercase`）——两个 job 增加 `${GITHUB_REPOSITORY,,}` 小写化步骤，buildx 直接引用点（outputs/printf/inspect）改用小写输出。

### 验证

- 工作流 YAML 语法校验通过（js-yaml）· 纯 CI 改动，无 Rust/前端代码变更；双平台构建与 manifest 合并由推送后 Actions 首跑验证。

---

## [1.4.15] · 2026-10-14 · 移植 main：记账终态单行 + Qoder 调度三键 + 签到重学

> 合并点 `main@dea9ceb` → `main@b0f81db`（4 个提交，手工语义移植，未 merge）+ PR #106（手工移植，未 merge）。桌面/OS 专属（TraeWork 迁移与升级引导、switcher、env 检测、device_proxy 等）跳过。

### 新增

- **Qoder 智能调度三键入池（issue #102）**：积分余额 / 最早积分包到期 / 快照时间透传入池，smart 排序（先到期先消耗）与 reserve 保留额过滤生效（旧数据 fail-open）；池运行期拨新 + 启动保鲜，充值/续期后无需重启网关即恢复可选。
- **签到当轮重学闭环（issue #105）**：盲发候选全 409 时当轮重取活动列表 + 宽松档三档重学 + 补一轮盲发，消除全天 409 空转。
- **Qoder 每日活动 id 批次轮换自愈（PR #106）**：兜底表更新至 act-20261013-603 批次；新增 conf 覆盖表 `<conf>/qoder_campaigns.json`（免重编译人工下发，条目优先于内置表，每轮读取）；新增最小头补学（仅鉴权四件套请求形态，列表分桶投放下容器形态领取前不可见 CLAIMABLE、最小头可见——10-14 全天 409 事故实证），叠加进当轮重学链（主形态宽松档 → 仍空 → 最小头）；409 过期 id 同步从跨账号共享候选表剔除，同轮后续账号不再重复盲发同一 409；最小头列表视野与 409 时候选池现状落 app_log 诊断。

### 修复与加固

- **记账每请求终态单行（审查问题6）**：Trae/WB 流式与非流式聚合路径，换号重试为中间态不落账，终态（成功 / 请求级错误 / Fatal / 池耗尽）恰落一行，`requests` 计数不再虚高；Fatal 终态补记账补齐用量页缺口。
- **空完成哨兵与客户端在线解耦（审查问题7）**：断连也上抛哨兵换号重试，防调用方误判成功绑粘性；Qoder 流式共享路径同步受益。
- **用量统计**：`recent_in` 改日历窗口过滤（7/14/30 天选择与图表严格对应）；Key「今日已用」四桶并拉跨桶求和。
- **Trae 单客户端收敛**：icube 扫描列表收敛 Trae CN，`trae_path` 设置退役。
- **杂项**：账号删除设备指纹清理失败落日志；API 使用帮助列宽调整、调度配置弹框移除 issue 标注；`docs/pending` 新增负载均衡与熔断切换方案。

### 验证

- `cargo test --workspace` 685+12 通过（7 忽略）· `tsc` 0 错误 · `vitest` 61 通过。

---

## [1.4.14] · 2026-10-14 · Trae OAuth 每账号合成设备身份

> PR #101（OAuth 设备隔离）。

### 修复与加固

- **Web-only OAuth 设备指纹按账号隔离**：容器内无本机 Trae 客户端凭证时，每次 OAuth 登录会话生成一套独立 synthetic 设备，登录成功按 uid 持久化；AuthCode `DeviceInfo` 交换、refresh `DeviceProof`、签到与积分请求共用同一身份，多账号不再共享一台设备触发「当前设备今日已经签到」。
- **API 管理近 30 天趋势图补 Qoder 池（issue #103）**：前端漏拉 `api_qoder_usage_stats`，趋势图/图例/合计补齐 Qoder 系列。
- **OAuth 设备指纹生命周期**：`account_delete` 同步清理 `device_map` 与 synthetic 身份，删除重登得到全新设备；同 uid 重新 OAuth 覆盖为本次新设备。
- **移除 synthetic 全局首凭证兜底**：刷新链路找不到账号绑定身份时如实报错，防设备漂移。**行为变更：存量 Web-only 部署绑定旧共享设备的账号需重新 OAuth。**
- **保留官方交换 host 白名单**：AuthCode 交换继续限定官方 API 域。
- **审查收尾修复**：重启后粘贴回调的 fallback 优先复用仍存活的 pending 合成身份（防重新生成覆盖导致交换设备漂移）；pending 转正删除失败补日志；refresh 链路注释与移除首凭证兜底后的实现对齐。

### 验证

- `cargo test --workspace` 674+12 通过（7 忽略）· `tsc` 0 错误 · `vitest` 61 通过 · Docker `/health` ok。

---

## [1.4.13] · 2026-10-13 · 移植 main：指纹诊断闭环 + 模板规则合并语义

> 合并点 `main@f5e3f48` → `main@dea9ceb`（2 个提交，手工语义移植，未 merge）。桌面/OS 专属改动跳过。

### 新增

- **指纹诊断闭环（issue #99）**：11128 拦截自动提取指纹样本（去重落日志），流式/聚合、WB/Qoder 全管线覆盖；池耗尽诊断行结构化，指纹封禁收尾补落样本。
- **模板规则合并语义（issue #99）**：默认表全量生效，自定义表覆盖/追加；`to` 可空（null=停用、空串=剥离）；前端指纹清洗面板支持逐条停用。
- **落池观测**：dispatch 落池时落 pool/model/bind/fallback_from 结构化日志。

### 修复与加固

- **日志本地时区**：`api_logger` 时间戳改 chrono 本地时间，容器 `TZ` 生效。
- **usage 宽口径**：token 数提取支持 f64/字符串形态，不再静默得 0。
- **OAuth 掩码豁免**：≤12 位纯数字串原样返回。
- **调度幂等与重试**：wb-checkin / wb-growth 抢锁失败改 `skipped_busy` 跳过；成长任务失败区分可重试与永久失败，可重试自动重试。
- **审查收尾修复**：wb-checkin / wb-growth 凭证类失败事件补结构化 `permanent` 标记，调度重试判定改结构化字段权威（显式 false 不被文案翻案、缺省退化文案匹配），不再依赖中文措辞稳定；`sse.rs` token 宽口径解析补负值过滤（数字/浮点/字符串三分支，对齐 `wb_sse` u64 口径）。
- **模板映射校验**：校验提示明确 `to` 可为字符串/空串/null。

### 跳过（桌面/OS 专属）

- OAuth 平台分派、`fs_utils` %APPDATA%、`qoder_common` Windows API、ideScan 桌面导入、`device_proxy`、`ide_store`、`updater`、`proxy` 等。

### 验证

- `cargo test --workspace` 674+12 通过（7 忽略）· `tsc` 0 错误 · `vitest` 61 通过。

---

## [1.4.12] · 2026-10-13 · 移植 main 缓存 tokens 口径对齐批：宽口径提取 + usage 归一化 + OpenAI 流式噪声帧治理

> 合并点 `main@aa4b475` → `main@f5e3f48`（3 个补充提交 + 本分支 PR #97 收敛，手工语义移植，未 merge）。全部为网关协议出口层系统无关改动，无桌面/OS 依赖。

### 修复与加固

- **缓存 tokens 宽口径提取收敛（main@1bbe54f）**：缓存读/写提取统一收敛为 `wb_sse` 单一宽口径实现（`cached_tokens_of` / `cache_write_tokens_of`）——键集对齐用量统计既有口径（补 `cacheReadInputTokens` / `prompt_cache_hit_tokens` / `cache_write_input_tokens` / `prompt_cache_write_tokens` 等），正值过滤（0 值陈旧别名不再遮蔽有效别名），嵌套兜底 `prompt_tokens_details.cached_tokens` 与 `inputTokensDetails[].cached_tokens`；`sse.rs` 与 `wb_responses.rs` 删除各自窄口径实现改为复用。
- **usage 归一化投影补齐（main@1bbe54f）**：WB 管线 OpenAi / OpenAiText 流式出口 usage 改走 `normalize_openai_usage`（WB 原始字段 → OpenAI 标准 `prompt_tokens_details.cached_tokens`，下游缓存读不再恒 0）。
- **多轮工具循环写回口径（main@37cdae7 / f5e3f48）**：`wb_route` 多轮累计写回 `write_back_acc_usage`——先清除全部读写侧遮蔽键（顶层别名 + 嵌套容器）再写 `cache_read_token` / `cache_write_token` 累计值，防上游末轮单轮残留值遮蔽跨轮累计（读侧少计、写侧首轮最大少报最明显）；各轮累计改用宽口径 `token_number`（f64/字符串形态 token 数不再静默得 0）。
- **Anthropic 出口缓存读/写透传（main@37cdae7 + PR #97）**：WB 流式 `message_delta` 与非流式 `completion_to_anthropic` 透传 `cache_read_input_tokens` / `cache_creation_input_tokens`（>0 才带）；SOLO 管线 `aggregate_anthropic` / 流式 `message_delta` 同口径补缓存写透传。
- **OpenAI 流式噪声帧治理（issue #96）**：Buddy 上游逐帧携带空 `tool_calls` 数组、重复 `role`、空串 `content/reasoning_content/refusal`、恒 null `extra_fields/function_call`，AI SDK v5 系客户端把空数组当思考封口信号导致思考被撕成上百碎片——透传前按 OpenAI 流式惯例归一化（空数组剥离、role 仅首帧、空串/null 剥离、纯噪声帧整帧跳过、`[DONE]` 不重复下发）。
- **移植审查防御加固**：多轮 usage 累计与 usage 总数求和改饱和加法（异常上游超大 token 值不 panic / 不回绕错账）；`write_back_acc_usage` 对非对象 usage（异常上游标量/数组）先归一为对象再写回，消除 `IndexMut[str]` panic 路径。

### 验证

- `cargo test -p aiwork-core --lib` 663 通过（7 忽略）· `cargo check`（core + server）0 错误。

---

## [1.4.11] · 2026-10-13 · 移植 main@7883d1c：OAuth 防伪加固 + namespace 拆分精确化 + Qoder 冷却快速失败

> 合并点 `main@af5c59e` → `main@7883d1c`（1 个提交，手工语义移植，未 merge）。桌面/OS 专属与依赖本分支不存在的 settings 字段的改动跳过。

### 修复与加固

- **OAuth 防伪**：登录 uid 一律以 JWT 解析值为权威，回调 URL 参数与 JWT 不一致即拒绝（含设备绑定回滚与在途会话清理）；`expect_uid` 空白串视为未指定。
- **namespace 回程拆分精确化**：布尔开关升级为 `encoded_tool_names` 编码名集合——仅请求侧展平生成的编码名参与回程 `{name, namespace}` 还原，客户端原生含 `__` 扁平名（MCP 惯例）不再误拆；全执行链接线。
- **Qoder 模型冷却**：冷却中排队请求终止轮换直接返回 429（不再逐账号硬打故障上游）；Fatal/冷却终态补 usage 记账；排队退避短窗遇 Server 失败抬升基准窗；退避统一走分段可中断等待。
- **时间解析补充**：`now_iso()` 补时区偏移；`parse_ts_loose` 支持纯数字 Unix 秒/毫秒与 `Z`/`/` 归一（保留 Local 时区口径，兼容容器 `TZ`）。
- **杂项**：共享分组删除清理 Key 引用后落日志；调度中心「账号保留剩余积分」输入改字符串草稿（清空/非法失焦还原不落盘，防误清零）。

### 验证

- `cargo test --workspace` 644+12 通过 · `tsc` 0 错误 · `vitest` 61 通过 · 移植后复审修正 OAuth 回滚基线与一处记账口径。

---

## [1.4.10] · 2026-10-12 · 移植 main 3.7.9/3.7.10：调度保留剩余积分 + Qoder 冷却回退 + OAuth 行内重登

> 合并点 `main@b59eb4b` → `main@af5c59e`（15 个提交，手工语义移植，未 merge）。已含移植后全面审查的修复（兼容性、数据保真、并发与安全）。

### 新增

- **行内重新 OAuth 登录**：无/失效 refresh_token 的账号可在账号页一键重登；`expect_uid` 以 JWT 解析 uid 为权威校验（回调 URL 参数外部可控，不可信），不匹配即拒绝并回滚本次写入。
- **调度保留剩余积分（issue #91）**：三池统一应用 `reserve_credits`——快照积分 ≤ 保留额且 `credits_fetched_at` 超 24h 视为陈旧降权（无时间戳 fail-open）；策略保存热应用至运行中网关；启动时检测陈旧快照后台保鲜（Trae 拉余额、WB 拨官网快照）。**存量兼容**：既有策略记录/存量安装沿用 0，仅全新安装默认 50。
- **issue #90：Qoder 5xx/504 模型级渐进冷却与跨池回退**：30s 起翻倍、上限 10 分钟，冷却窗口内不再逐账号硬打上游；模型目录新增来源池标识与跨池命名归一（前端展示「来源：Trae/WB/Qoder」）；冷却/禁用标记落 app.log（`pool=` 上下文，回退日志节流）。
- **Qoder 会话聚合展示与定时合并**：新增 `qoder-sessions-compact` 定时任务（每 6h 合并 3 天前的历史流水），CLI/调度卡片双入口；仅合并多行会话组，单行组保留请求级明细，失败会话以 `MIN(ok)` 如实标记。
- **issue #93**：`/v1/responses` 不再丢弃 `type=="namespace"` 工具组——展平为 `<ns>__<inner>` 编码名转发，请求/响应两侧还原 `{name, namespace}`；无 namespace 组的请求不受影响。

### 修复与加固

- **credits_fetched_at 解析断点（审查发现，阻塞）**：本地时间串过不了严格 rfc3339 解析，保留额过滤与 24h 陈旧判定曾结构性失效；新增 `fs_utils::parse_ts_loose` 统一解析并补回归测试，同时修复「每次启动恒触发全池保鲜刷新」。
- **vault 回填（04214ba）**：JWT 与 refresh_token 回填独立判断，修复代理捕获明文 JWT 后 RT 恒不回填；回填判定统一 trim。
- **数据与容错**：WB 积分回填失败不再清旧值；会话合并幂等防重（`agg:` 前缀大小写两侧同口径 + `(date,uid,preview)` 副索引）；`shared_groups_list` 读失败 fail-closed；多处静默失败补日志。
- **稳定性批（3.7.9）**：`/health` 收敏；新增 `group_defs_lock` 补齐分组定义写互斥；迁移/导入韧性；看板口径修复。
- **安全**：IP 白名单拒绝落审计日志、错误回显清洗控制字符并截断；identity 错误脱敏进 `last_error`。

### 跳过（桌面/OS 专属 / docs-only）

- `409be09`/`d09fd63` 为 docs-only 跳过。

### 验证

- `cargo test -p aiwork-core -p aiwork-server` 654 通过（7 忽略）· `tsc` 0 错误 · `vitest` 61 通过。

---

## [1.4.9] · 2026-10-12 · 移植 main：共享分组/账号标签 + 会话明细 + 网关积分计量

> 合并点 `main@8628629` → `main@b59eb4b`（4 个提交，手工语义移植，未 merge）。

### 新增

- **共享分组与账号标签（D1-D5）**：新增共享分组/标签命令模块与 `org_index` 组织索引；三池私有分组统一迁移为共享分组（`shg_` 前缀，幂等可断点续跑，可回滚）；API Key 支持 `allowed_groups` / `allowed_tags` 约束与覆盖数预览；三池装配层标签筛选与分组取交集（均空 = 不限）；三平台导入链路恢复标签绑定。前端新增 `TagSelect` / `TagsModal`，账号页「标签」列、ApiKeysManager 分组/标签双列勾选 + 实时预览、服务页标签筛选 chips。
- **会话明细与网关积分计量（issue #82）**：三平台会话明细查询（分页 + 模型/用户过滤，Trae 补到期包清单）；用量计数补 `credits` / `original_credits`；Qoder 网关积分计量（复数 `credits` 优先）。前端 Dashboard 新增 SessionsTab 会话明细，分组/账号全局筛选联动 KPI 与各 Tab；ExpiryTab / CreditsTab / TokensTab 相应适配。

### 修复与加固（发布前 + 二轮复核，12 项发现 11 修 1 明示不修）

- **并发**：新增 `org_defs_lock` 组织定义表互斥（groups/tags 整删全写模型防丢更新，12 处写路径纳锁，锁序单向）；分组定义迁移前移至引用重定向前，断点续跑不再产生重复定义。
- **数据与容错**：导入凭证回写/标签绑定改主体落库后批量执行，单条失败聚合不中断（防半成品，重导可补齐，三平台口径对齐）；Qoder 会话 `account_name` 恒空修复；孤儿标签绑定自愈；`org_index` 读库失败降级空索引补日志。
- **分页**：三平台会话明细页码夹取至最后一页，防 u32 乘法回绕，前端空页自动回夹。
- **杂项**：SchedulerTasksCard 补齐 every_hours 三档模式；`config_set` 整表携带任务时刻/模式/小时防清空；启动公告一次性取走；删除重复预览块、弹框失败重抛等小修。
- 明示不修：Qoder 导入覆盖已存在账号的标签/分组绑定（导入文件为权威态，与 WB 语义一致）。

### 跳过（桌面/OS 专属）

- `workbuddy_stats` 本地 Token 统计、profile 槽位体积缓存、state sync_mode 零值归一、`refreshLocalEntitlement`。

### 验证

- `cargo test --workspace` 621 通过（7 忽略）· `tsc` 0 错误 · `vitest` 61 通过。

---

## [1.4.8] · 2026-10-10 · 移植 main 3.7.6 剩余审查修复批 + 锁纪律对齐

> 合并点自 `main@10627d0` 推进至 `main@8628629`（1 个提交 `8628629` 3.7.6 审查修复批，手工语义移植）。该批中锁体系/表锁/看门狗/设置守卫等大部分已在 1.4.7 先行落地，本批补齐剩余缺口并复核加固。

### 网关并发（aiwork-core）

- **WB 网关 401 自愈忙态哨兵**：`wb_upstream::refresh_access_token` 忙态错误提升为 `ERR_PEER_REFRESHING` 常量；流式/聚合/工具代执行三处 401 自愈识别忙态——他方进程刷新中本轮直接换号，不再走 `classify_error`→`note_error`，防瞬时忙态被误判 SessionDead → disabled 踢出轮转（对方刷新落库后由持锁二次检查/stale 对比自动复用新凭证）。
- **`cooldown_clear_all` 叶子锁纪律对齐**：内存池清理（内部持池锁）移出 `ACCOUNT_COOLDOWNS_LOCK` 持锁段——叶子锁不得嵌套其他锁，消除与「持池锁→取冷却锁」路径的锁序反转死锁隐患。
- **`refresh_jwt_impl` 解冻段锁内瘦身**：`app_log` 移出冷却表叶子锁持锁段，锁内仅 load/remove/save。
- **积分刷新自动解冻两阶段回写升级为快照比对**：回写阶段仅移除与扫描时快照完全一致的冷却条目（type/until/reason/count 四字段），扫描窗口内调度签到/手动操作重写的新冷却不再被误清。

### OAuth 与签到（aiwork-core）

- **OAuth 回调 fallback 设备去漂移**：无在途会话时的 fallback 设备改为回调入口一次性选定，AuthCode 交换与尾部 `device_id` 透传（→ 刷新交换、device_map 回写）共用同一台——消除横跨分钟级交换窗口内两处独立选定可能选中不同设备的错配。
- **设备绑定上限特征词收窄**：`humanize_device_limit` 命中词由「已达上限」/「账户数量」过宽子串收窄为「账户数量已达上限」+「绑定」前置词同现判断——「使用额度已达上限」等非设备绑定类错误不再被误追加重新 OAuth 登录引导，同时兼容上游有无「的」的文案变体；补正反例测试。

### 前端健壮性与 UX

- 删除账号确认弹框补 `busy` 防重入：确认请求 pending 期间禁用按钮并阻止关闭（防 1s 最小展示窗口内连点二次触发），按钮显式「删除中…」。
- 分组 `create/update/remove` 失败 rethrow：GroupsModal 统一捕获后保留输入便于重试，不再产生 unhandled rejection。
- 删除分组确认补防重入：pending 期间禁用按钮与关闭，失败保留确认弹框。

### 杂项

- `package-lock.json` 版本号补同步至当前版本（1.4.7 时漏更，仍停留 1.4.6）。

### 跳过（桌面/OS 专属）

- `switcher` 账号切换加固（本分支无 switcher 模块）、`cert.rs` PowerShell 单引号转义（无证书命令）、窗口最大化修复、Buddy/Qoder 导出改 Tauri 原生另存为（浏览器管理面维持下载）、Doubao 设置保存守卫（本分支无 Doubao 页）。

### 验证

- `cargo test` 602 通过 · `tsc` 0 错误。

---

## [1.4.7] · 2026-10-10 · 全项目审查加固

> 并发 / 安全 / UX / 兼容四维审查后的问题修复批次。

### 并发与数据安全（aiwork-core）

- WB 网关 401 自愈刷新接入每账号锁 + 跨进程锁 + 持锁二次检查（防并发刷新互相覆盖丢凭证）。
- wb_tokens 表锁统一为 `WB_TOKEN_STORE_LOCK`，消除网关与 tasks 侧双锁分裂。
- `device_map` / `account_cooldowns` 整表读-改-写加共享互斥，防多写方整表覆盖丢条目。
- 调度器配置落库失败改上抛；任务状态落库失败补日志。
- `jwt_refresh_lock` 锁中毒恢复（panic 后不再永久拒绝刷新）。

### 安全加固（aiwork-server）

- `/api/login` 成败均记录来源 IP（登录失败可观测）。
- 会话 cookie 经 HTTPS 到达时附加 `Secure`。
- `admin_tokens` 缓存锁毒化恢复（鉴权热路径不再 panic）。

### 前端健壮性与 UX

- invoke 统一 120s 超时兜底（登录 30s）；网络层失败翻译为可读文案。
- WS 半开连接检测：连续两周期无 pong 主动关闭并回退 SSE。
- `ws-lagged` 丢帧事件纳入订阅并提示用户。
- 签到「进行中」看门狗兜底：15 分钟无事件自动解除卡死。
- 设置保存守卫：配置未加载成功时拒绝保存，防默认值覆盖服务端配置。
- Buddy 分组增删改补 toast/catch；新建防连点。
- 吊销令牌 / 开启免令牌 / 关闭网关鉴权三处二次确认。
- Trae 账号页与 Dashboard 刷新防重入；错误 toast 驻留延长至 10s；API 日志页失败可见化。

### 兼容性

- `read_json` 剥 UTF-8 BOM，解析失败打印文件名与原因。
- 启动日志输出生效调度时区偏移，便于 Docker 部署核对 `TZ`。

### 明示未修（后续批次）

- `auth_middleware` 同步读库于 tokio worker；`trust_proxy` XFF 语义依赖反代清洗；`api_pool` 整池跨进程写回无合并；Trae 刷新全局锁；WS 控制帧无速率限制；Docker 容器 root 运行。

### 验证

- `cargo test` 602 通过 · `tsc` 0 错误 · `npm test` 61 通过。

---

## [1.4.6] · 2026-10-10 · 移植 OAuth 设备凭证轮转 + 同 uid 登录合并 + 持久化断链修复

> 合并点自 `main@bc58506` 推进至 `main@10627d0`（2 个提交，手工语义移植）。

### 新功能

- **OAuth 设备凭证轮转（issue #76）**：登录/刷新从本机凭证集（含容器合成设备兜底）按 device_map 绑定数确定性择优，登录成功回写 device_map，登录 URL、Token 交换、刷新、签到全链路设备同源，账号均匀摊到多套设备凭证上避开服务端每设备绑定上限。Web-only 容器无本机凭证时轮转退化为合成设备恒选，存量账号行为不变。
- **OAuth 同 uid 登录合并（issue #80）**：同一服务端账号再次 OAuth 登录不再产生重复账号，按 uid 直接更新已有账号凭证；登录所选分组对合并账号同样生效；前端提示与日志如实表述「已更新其凭证，未新增账号」。
- **「无 Refresh」账号徽标**：账号列表对缺 refresh_token 的历史导入账号显示灰徽标并给出恢复路径（重新 OAuth 登录即可自动更新）。

### 修复加强

- **`general_expire_times` 持久化断链修复（issue #75）**：该字段此前仅内存使用、重启即丢，网关调度通用积分到期口径回退混合口径；补齐 `remaining_credits` 表 `general_expire_time` 列读写，并新增 save → load → merge 端到端回归测试。
- **设备绑定上限错误可读化（issue #76）**：签到/巡检命中该类错误时按中英文特征词追加轮换指引，不再倾倒原始文案。
- **API 网关装配重复 uid 告警（issue #80）**：账号池存在同 uid 多条目时启动即记录告警，防多条目干扰调度与排查。
- **模型厂商归属修正**：space-bunny/spacebunny → MiniMax、mimo → 小米（网关模型目录 + Qoder 目录 + 自定义模型面板预设同步更新）。

### 审查修复（移植后逐行复核批次）

- **合并路径重置 `refresh_token_expires_at`（issue #80）**：同 uid 再登录换新 refresh_token 时清空按旧 token 估算的到期时间，消除「即将过期」徽标误报（下次刷新成功自动回写）。
- **登录交换失败透出详情**：refresh_token 交换失败不再吞掉信封错误码/message（20403/20405/20101 区分是轮转排查关键），统一文案改为携带原文。
- **登录闭环清理在途会话**：成功登录后清空 `LAST_OAUTH_STATE`（PKCE verifier / 会话设备不再残留；并发双开互踩限制已在注释声明，回调重放由 AuthCode 服务端单次有效兜底）。
- **登录 URL hostname 跨平台**：`x_device_brand` 在 Linux/docker 下回退 `HOSTNAME` / `/etc/hostname`（原仅读 Windows 专属 `COMPUTERNAME`），兜底仍为 Windows-PC 与伪装口径一致。
- **重复 uid 告警每进程一次**：池热重载重跑 `build_shared` 不再重复刷屏。
- **「无 Refresh」徽标覆盖池视图**：`ApiService` 池账号列表同步传 `missing`，与账号管理一致。

### 跳过（桌面/OS 专属）

- WorkBuddy 冷启动切换死锁解除与守卫文案分流（issue #78，switcher 桌面切换体系）、证书手动导入指引与降级链（issue #12/#79，Windows 证书存储/UAC）、`oauth_loopback` merged 回环页文案（docker 为粘贴回调模式）。

### 验证

- `cargo test --workspace` 602 通过 · 0 失败（含新增 6 项单测）· `npx tsc --noEmit` 0 错误 · `npx vite build` 通过 · `npm test` 61 通过。

---

## [1.4.5] · 2026-10-09 · 移植 WB 账号池互斥防丢失 + 上游空 message 兜底

> 合并点自 `main@5d0dd66` 推进至 `main@bc58506`（3 个提交，手工语义移植）。

### 修复加强

- **WB 账号池读-改-写互斥防 lost-update**（`6284ef3`）：全部运行期池写点持 `wb_pool_lock` 执行，防并发线程用旧池副本整文档覆盖丢账号；额度巡检、凭证续期、套餐回填改两阶段回写（锁外网络调用、短临界区字段级合并）。
- **空完成取证**（`6284ef3`）：Trae 对冲竞速捕获上游响应头摘要，接管生效时空完成日志可溯源上游侧证据。
- **device_map 缺条目回落**（`6284ef3`）：Trae 调度缺映射条目时按 uid 确定性派生 device_id，不再发送空 `x-device-id`。
- **上游错误空 message 统一兜底（issue #71）**（`67f9e5b`）：空 message 回填含状态码/错误码的诊断文案，inline 与 Trae/WB/Qoder 流式错误全接入，消除 `{"message":""}` 透传旁路。

### 文档

- user-manual 补「客户端报 502 upstream_error（全局代理劫持回环流量）」FAQ（端口适配 Web 版网关 8080）。

### 跳过（桌面/OS 专属）

- doubao 双池锁全套、代理日志分片数值排序（misc 抓包 docker 已下线）。

### 验证

- `cargo test --workspace` 597 通过 · `npx tsc --noEmit` 0 错误 · `npm test` 61 通过。

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
