# 产品优化需求清单（全应用统一待办）

> **文档版本**: v3.0 · 2026-09-26
> **定位**: 全项目**唯一待办依据**——所有未实施的优化与需求项均在此登记，每条含需求概述 / 实现路径 / 参考开源项目。
> **v3.0 变更**（2026-09-26，待办全量复核 + F-75 收官）：① **F-75 macOS 平台支持标记 ✅ 已完成**——macos_main 分支产品化（2026-09-17~26，22 提交）：platform 服务层 + 平台门控 + dmg 构建流水线（db4efd4）、app_locate mac bundle 分派 + LaunchServices 启动（5c1f79a）、平台抽象层收敛（113350a）、R1-R3 真机收口 + 双平台二轮审查修复（4adf498）、三/四轮真机修复——vault 双源密钥 + mac JWT 本地捕获 + 切换链收口（97e4dd5）、CI 三产物矩阵 aarch64/x64/universal + 可选自签（3c89310）、package_macos.mjs dmg + 便携 zip（81a9870）、dmg 打包重试（16a9b3d）、局域网网卡黑名单补 mac 虚拟接口（3a31d17）、内置更新竞态修复（#37）+ macOS 手动安装引导（e96a218）；随 3.6.1/3.6.2 发 mac dmg，AGENT.md §11.1 双平台发版红线在案；② 其余待办经代码检索复核状态无变化（F-70 余项 tc 直读仍未接入 `apps_accounts_discover`；F-38/E-01/E-02/F-69/F-79 等确认未实施，状态属实）。
> **v2.9 变更**（2026-09-16，网关请求链路堵塞分析批次落地）：① 新增 **F-79 网关流式上游异步化**（reqwest + async SSE，原「D-2 二期」）——同批已落地的过渡方案：调度配置内存缓存（A）+ API 日志异步写入（B）+ 用量/Key 记账削峰落盘（C/E）+ 流式专用阻塞池线程隔离（D-1），P0 级堵塞（spawn_blocking 池耗尽、async worker 同步 SQLite）已消除，F-79 为彻底形态；② 其余待办项状态复核无变化。
> **v2.8 变更**（2026-09-16，对照 CHANGELOG 3.5.3 + 全仓代码检索完成度核查）：① **F-78 全链路完成**——批次 3 真实协议闭环（PKCE + authCodeInfo 回调 + DeviceInfo 主变体 + 端点修正 + 变体探测链，v3.5.3 九提交），原「待抓包验证项」经协议实测全部裁定；② **F-70 部分落地**——`icube_auth.rs`（tc 信封解密 + ECDSA P-256 DeviceProof）随 F-78 批次 3 交付，条目改「部分完成」，剩余账号发现直读下调至 1~2 天；③ §五排序刷新（已完成项退出）；④ 其余待办项（F-38/F-69/F-67/F-07/F-41/F-42/F-66/E-01~E-03 及远期项）经代码检索确认均未实现，状态属实。
> **v2.7 变更**: ① **W-01（Work 积分接入网关）标记 ❌ 已排除**——Trae 积分签到调整，前提与收益均不成立；条目与 §三 专题保留作技术留档，§四新增排除行、§五排序移除；② **F-68 已完成**（`switcher/vscdb.rs` 全局键合并，恢复前抽键 → 恢复后按条目合并回写）；③ **F-74 已完成**（chatdata 三命令 `app` 参数化 + CodeBuddy 会话域 + `buddy_switch_migrate_chats` 切换编排）。
> **v2.6 变更**: F-75 macOS 支持依赖盘点全面复审——Python/PS 全量 Rust 化、存储 SQLite 化、定时任务 Rust 原生调度器（`tasks/scheduler.rs`）落地后，原三大依赖项（PS 切换桥主工程、Python 路径中立化 M2、Job Object）整体消失，预估 6~8 周 → 3~4 周；剩余 Windows 依赖收敛为 13 个明确模块点（vault DPAPI / switcher 三模块 / 系统代理 / 证书 / MITM 绑定 / 豆包 cookie 解密 / schtasks 注册面等），详见 F-75 条目。
> **v2.5 变更**: 文件由 `product-optimization-backlog.md` 重命名为 `backlog.md`（全仓引用同步更新）；完成度核查（对照代码实测）：F-76/F-77/F-78 已实现标记属实（hedge 竞速 / `account_concurrency_limit`+per-account inflight / `oauth_loopback`），其余待办项经代码检索确认均未实现，状态不变。
> **v2.4 变更**: 合并 main 提交 726849b——新增 **F-78 Trae OAuth 授权闭环补全**（源自 issue #10 用户反馈：OAuth 登录撞 SSL + 回调无监听，现状为"半实现"——详见条目）；原提交编号 F-74 与本清单 v2.1 已登记的 F-74（Buddy 切换自动迁移会话）冲突，改号 **F-78**。
> **v2.3 变更**: 新增 F-76（网关慢接口优化——用量统计平均耗时 43.7s 的延迟归因与优化）与 F-77（账号级并发感知调度——当前账号在途即让位空闲账号），F-77 为 F-72 的账号级前序落地。
> **v2.2 变更**: 新增 F-75（macOS 平台支持）——Windows 依赖全景盘点结论：迁移策略为「分层归位」而非整体重写，系统/文件/进程层迁 Rust 原生（切换桥 Rust 化为主工程），网络协议层 Python 已天然跨平台仅路径中立化；最大不确定性是各目标应用 macOS 版数据布局，侦察先行。
> **v2.1 变更**: 新增 F-74（Buddy 切换时自动迁移会话到目标账号）——可行性分析结论：WorkBuddy 侧核心能力（F-44/F-45）已落地，仅缺切换编排与 CodeBuddy 会话域扩展；Trae 侧真迁移维持 §四已排除结论不变。
> **v2.0 变更**: ① 合并删除五份分析文档——`docs/tmp/`（trae-account-switch-data-migration-analysis / doubao-api-feasibility / oss-ecosystem-value-analysis）、`work-credit-pool-design.md`（完整并入 §W-01）、`unified-api-gateway-design.md`（已实施，要点并入 tech-framework.md）；② WorkBuddy 蓝本（原 workbuddy-product-design.md）批次 1~5 已全部完成，其机会项 F-41/F-42/F-52/F-66 转入本文；③ 新增 F-67~F-73、E-01~E-03 共 10 项（源自上述分析文档中的未实现价值点）；④ 原 F-44（TRAE 多实例并行）改号 **F-67**，消除与 WorkBuddy 蓝本 F-44（会话备份，已完成）的编号冲突。
> **原则**: 接口层独立模块 + 失败明示 + 不硬编码奖励数额；仅管理本人合法持有的账号；借鉴开源遵循 learn-the-design, write-our-own-code。

---

## 一、待办总览

| 编号 | 功能点 | 应用域 | 优先级 | 预估 | 状态 |
|---|---|---|---|---|---|
| F-76 ✅ | 网关慢接口优化（延迟归因 + TTFT 分维 + 竞速对冲） | 网关 | **P1** | 2~3 天 | 已完成（2026-09-14） |
| F-77 ✅ | 账号级并发感知调度（busy 让位 idle） | 网关 | **P1** | 2~3 天 | 已完成（2026-09-14） |
| F-68 ✅ | Trae 项目列表/最近打开跨账号保留 | Trae 生态 | **P1** | 1~2 天 | 已完成（2026-09-15） |
| F-74 ✅ | Buddy 切换时自动迁移会话到目标账号 | Buddy 生态 | **P2** | 2~3 天（含实测） | 已完成（2026-09-15，含 B2 CodeBuddy 会话域扩展） |
| F-78 ✅ | Trae OAuth 授权闭环补全（回环监听 + 代理豁免 + code 交换） | Trae 生态 | **P1** | 1~2 天（批次1）/ 3~4 天（全链路） | 已完成（批次 1+2 2026-09-14 / 批次 3 真实协议闭环 2026-09-16，v3.5.3） |
| F-24-余 ✅ | 豆包会员额度端点抓包固化 | 豆包 | **P1** | 0.5~1 天（含抓包） | 已完成（2026-09-16 真机复验通过：概述页额度卡出数，正式闭环） |
| F-38 | Trae → DSH 引导（不自研） | Trae 生态 | **P1** | ≈0（装即用） | 待开发 |
| E-01 | 豆包对话网关（OpenAI 兼容 doubao provider） | 豆包/网关 | **P2** | 8~12 天（含 E-02） | 待开发（方案 B 已论证，含探测实验前置） |
| E-02 | 豆包指纹嗅探持久化 + a_bogus 纯算法生成器 | 豆包 | **P2** | 并入 E-01 批次 | 待开发（E-01 前置） |
| W-01 ❌ | Work 积分（209）接入 API 网关（多活会话编排） | Trae/网关 | — | — | 已排除（2026-09-15）：Trae 积分签到调整，前提与收益均不成立，见 §三/§四 |
| F-70 | Trae tc 凭证直读 + ECDSA P-256 刷新情报核对 | Trae 生态 | **P2** | 1~2 天（余量） | **部分完成**（解密算法 + DeviceProof 已落地 2026-09-16；剩余账号发现直读接入） |
| F-69 | Trae 会话导出存档（Markdown + 存档浏览器） | Trae 生态 | **P3** | 2~3 天 | 待开发 |
| F-79 | 网关流式上游异步化（reqwest + async SSE 迁移） | 网关 | **P3** | 2~3 天 | 待开发（过渡方案 D-1/A/B/C/E 已落地 2026-09-16） |
| E-03 | 豆包多模态端点（生图/生视频/音乐/文件中转站） | 豆包/网关 | **P3** | 3~4 天 | 待开发（依赖 E-01） |
| F-67 | TRAE 多实例并行（原 F-44 改号） | Trae 生态 | P3 | 未定（调研先行） | 待调研（issue #9） |
| F-07 | 豆包 cookie 级热切换（方案 B） | 豆包 | P3 | 1~2 天 | 待验证后开发 |
| F-41 | trae2codex 转换器 | Trae 生态 | P3 | 3 天 | 待开发（机会项） |
| F-42 | workbuddy-mcp 模式 | Buddy 生态 | P3 | 2~3 天 | 机会项（按需评估） |
| F-66 | CLI 多账号环境隔离 | Buddy 生态 | P3 | 评估先行 | 机会项（按需评估） |
| F-75 ✅ | macOS 平台支持（分层归位） | 全应用 | P3 | 3~4 周（实际 ~1.5 周，2026-09-17~26） | 已完成（2026-09-26，macos_main 分支产品化，随 3.6.1/3.6.2 发 mac dmg） |
| F-52 | WorkBuddyProxy 模式（驾驶舱 + Codex 执行器） | Buddy 生态 | P3 | — | 远期（与 F-40 方向相反） |
| F-71 | Trae SG 版（国际版）支持 | Trae 生态 | P3 | — | 远期（前置情报已有） |
| F-72 | 网关上游多级回退 + 分档竞速调度 | 网关 | P3 | — | 远期（调度增强方向） |
| F-73 | 网关反哺 IDE（第三方模型进 Trae） | Trae/网关 | P3 | — | 远期留档（方向验证） |

> 已完成项不再列于此（F-13 到期日历 / F-43 CC Switch 协同等已在版本中落地，详见 CHANGELOG.md）。

---

## 二、条目详情

### F-76 网关慢接口优化——延迟归因 + TTFT 分维 + 慢请求竞速（P1）

- **需求概述**：用量统计页显示平均耗时 43765ms（近 14 天，glm-5.3-flash 占 95.7%）。延迟归因（2026-09-14 完成，按 Token 统计窗口 ~128 次请求折算）：
  - **单请求画像：输入 ~44.5k token（5.7M÷128）、输出仅 ~560 token（72.2k÷128）、耗时 43.7s**——输出极小说明延迟**不是生成长度问题**；44.5k 输入的 prefill 加上 43.7s 的绝对耗时，指向 **上游 prefill 计算排队 + 上游服务排队等待**为主因；网关自身（内存锁 + mtime 缓存 + 调度）开销为 µs~ms 级，可忽略。输入输出比 ~80:1 是 CLI agent / 后台任务类流量（整上下文短补全）的典型形态。
  - **排队假说的两个佐证**：① 44.5k token 的 prefill 在正常服务能力下应为秒级，43.7s 意味着大概率存在免费/低档通道的排队窗口；② 并发请求全部落同一账号（现状调度不感知在途负载，见 F-77）会进一步放大排队——两主因均可用 TTFT 数据最终裁定。
  - **均值被长请求拉偏**——统计按请求取平均，无分位数，无法区分「普遍慢」与「少数超大请求慢」。
  - **无首字延迟（TTFT）分维**——`api_logger` 只记总时长，无法区分「上游排队久才出首字」与「首字快但生成久」，两者优化手段完全不同（前者靠调度/对冲，后者靠模型/上下文瘦身）。
  - **首字超时只兜死不兜慢**——`wb_upstream.rs` `FIRST_BYTE_TIMEOUT=10s` 仅在上游完全无首字节时故障转移；上游"活着但慢"（排队 30s+）不会触发任何动作。
- **实现路径**（按收益排序，可独立交付）：
  1. **可观测先行（~0.5 天）**：`api_logger` 记录增加 `first_byte_ms`（TTFT）与 `total_ms` 双字段（`start_ts` 已有，补首字节时间戳）；用量页耗时卡片从"平均值"升级为 **P50/P95/最大值 + TTFT 均值**，并按模型分桶——后续所有优化以这组数据验证收益；
  2. **会话粘性 TTL 可配置（~0.5 天）**：上游对同上下文有 prefill 缓存收益，粘性命中 = 缓存命中；现有 `POOL_STICKY_TTL_SECS=60` 对 CLI 多轮间隔偏短，提升为设置项（建议默认 5~10min），TTL 内同会话必落同一池同账号 → 上游 KV cache 复用直接砍 prefill 时间；
  3. **慢请求竞速对冲 hedged request（~1 天，P2C 调度理念的请求级延伸）**：流式请求首字超过阈值（建议 P95 TTFT 或可配固定值如 15s）且池内还有其他健康账号时，向第二账号发起对冲请求，取先出首字者、取消另一条——把「上游排队慢」从等待变成竞速；需防重复计费（被取消一侧不计 usage，upstream 无计费即无消耗）；
  4. **上下文瘦身提示（~0.5 天，随缘）**：请求体超阈值（如输入 >100k token 估算，均值 ~44.5k 的 2 倍以上）时在响应/日志标注"上下文过大"，并在网关设置页暴露 `wb_bg_downgrade` 同族的"长上下文降档"开关（后台任务类请求自动换 flash 档模型，已有 `is_background_task` 判定可复用）。
- **边界与风险**：① 竞速对冲会短暂放大上游并发（每慢请求最多 ×2），需受 F-77 的账号并发上限约束；② TTL 延长增加粘性表内存（每键极小，量级可忽略）；③ 上下文裁剪不在网关做真裁剪（语义风险），只做提示与降档路由。
- **参考开源项目**：`laojichao/trae-api`（分档竞速调度，F-72 已引用，本项是其请求级最小落地）；Google 附标对冲请求（hedged requests）经典实践；本项目 `wb_bg_downgrade`（后台任务降档先例）。
- **验收**：用量页能分别看到 P50/P95 与 TTFT；开启对冲后 P95 总耗时显著下降（实测对比）；关闭全部新开关行为与现状一致。

### F-77 账号级并发感知调度——busy 让位 idle（P1，F-72 的账号级前序）

- **需求概述**：并发请求时，若被选中的账号已在处理请求（在途），自动换一个**空闲账号**服务，避免并发请求堆在同一账号上游排队放大延迟（与 F-76 归因互相印证：43.7s 平均耗时在并发场景会进一步劣化）。现状缺口：`pool.rs` 调度三因子（积分/闲置/成功率）**不感知账号在途负载**；`mod.rs` `active_uid` 为单值 `Option<String>`，只有"最近活跃"无 per-account 并发；防惊群仅 100ms 窗口，其后并发请求仍会重复选中同一账号。
- **实现路径**：
  1. **per-account 在途计数（~0.5 天）**：`PoolEntry` 增加 `inflight: u32` 字段；`InflightGuard` 扩展为双维护——取号成功时构建携带 uid 的 guard，进入 +1 / Drop -1（流式请求 guard 已持有至流结束，语义现成）；全局 `inflight` 计数保留不变；
  2. **取号过滤 busy（~0.5 天）**：`selectable()` 增加"账号并发上限"判定——`inflight >= account_concurrency_limit`（新配置项，默认 1，0 = 不限保持现状）的账号视为 busy 不参与候选；全部 busy 时降级为「选 inflight 最小者」（不过载拒绝，保证请求不失败）；
  3. **调度因子加入负载维（~0.5 天）**：`weighted_score` / `pick_p2c` 增加负载因子（inflight 越高得分越低，或直接作为 P2C 的第一比较键：先比 inflight 再比三因子得分）——即使不开启硬上限，P2C/Weighted 策略也天然偏向空闲账号；
  4. **粘性让位语义（关键细节）**：会话粘性（账号级 `wb_sticky` + 池级 `pool_sticky`）优先级**高于** busy 让位（粘住 = 上游缓存命中，换号反而更慢），仅当粘住的账号 busy 且存在空闲账号时才让位，并在日志标注 `sticky_yield`；
  5. **可观测**：`/status` 的 `active_uid` 单值扩展为 per-account 并发列表（`[{uid, name, inflight}]`），前端池状态页展示各账号实时负载；`api_logger` 记录每次取号的"让位"事件（原选中 busy → 实际取了谁）。
- **实现要点**：改动集中在 `pool.rs`（Entry 字段 + selectable + 得分）与 `mod.rs`（InflightGuard），`dispatch.rs` 选池逻辑零改动；Trae 池与 Buddy 池同构自动同时受益；F-76 的对冲请求受本项上限约束（对冲第二请求取号时原账号已 inflight，天然会选别的账号）。
- **边界与风险**：① `account_concurrency_limit=1` 下单账号用户会感觉"自己的请求不粘自己"——由粘性优先级兜底（空闲才让位，单账号池无让位对象）；② guard 与取号的配对路径需覆盖所有执行路径（stream_chat / aggregate_chat / wb_route 三处 + custom_route），漏一处即计数泄漏——用 RAII 保证，取号即建 guard；③ 与 F-72 的关系：本项交付后，F-72 的"分档竞速"只需在其上叠加模型维度。
- **参考开源项目**：`antigravity-tools`（P2C 负载感知调度理念，pool.rs 注释已引用）；nginx `least_conn`（最少连接调度——本项即其在账号池的等价物）；本项目 `InflightGuard`（RAII 计数基建已就绪）。
- **验收**：双账号池 + `limit=1` 时，第二个并发请求自动落空闲账号（日志可见让位）；单账号池行为与现状一致；全部 busy 时取 inflight 最小者不拒绝；流式中断/断连后计数正确归零（Drop 兜底）。

### F-68 Trae 项目列表/最近打开跨账号保留（P1）✅ 已完成（2026-09-15）

- **需求概述**：切换账号后 Trae 内「项目列表」「最近打开」随槽位快照整体回滚而"消失"——根因是 `state.vscdb` 全局键（`solo-lite.local-project-folders`、`history.recentlyOpenedPathsList`）被快照覆盖，而项目本体（本地文件夹）与 `workspaceStorage`/`User/History` 本就跨账号保留。目标：**切到任何账号，项目列表与最近打开都在**。
- **数据归属事实**（2026-09-10 实测侦察结论）：`state.vscdb` 共约 200 键，其中 7 个账号前缀键（`solo-lite:content-map:<uid>` 会话映射、`solo-lite-mode-state-map-<uid>`）**按账号分区、绝不跨账号合并**（否则产生服务端归属校验失败的"幽灵会话"）；`local-project-folders` / `recentlyOpenedPathsList` 为**全局单键**，是本项目唯一可合并对象；登录态（storage.json/machineid）绝不合并。
- **实现路径**：
  1. 在 `switcher` 模块的 `Switch` / `RestoreOnly` 管线（原 PS 桥，已 Rust 化）中，恢复槽位快照**前**从当前 state.vscdb 抽出两个全局键，恢复**后**合并写回（`local-project-folders` 按项目 id 合并、快照内已有以快照为准；`recentlyOpenedPathsList` 去重并保留最近打开时间排序）；
  2. SQLite 键级读写：零新增依赖——switcher Rust 侧直接用 `rusqlite`（已是依赖，vscdb 读库先例 `trae_apps.rs` / `doubao_chats.rs --check-login-cookie` 同源模式）对 `state.vscdb` 做 kv 表级读写；
  3. 操作前对 `state.vscdb` 做一次性 `.bak` 备份，失败回滚；全程在 Trae 未运行窗口期执行（切换流程本就先关闭，天然满足）。
- **参考开源项目**：无直接同类实现（自研分析）；SQLite 处理参照本项目 `doubao_chats.py` 既有模式。
- **验收**：双账号各建若干项目后互切，项目列表与最近打开完整保留；账号分区键零改动。
- **落地落点（2026-09-15）**：新增 `src-tauri/src/switcher/vscdb.rs`（`snapshot_global_keys` 恢复前抽键 / `merge_global_keys` 恢复后按条目合并回写，写前 `.f68.bak` 备份 + 失败回滚，BLOB/TEXT 原类型保持）；`switcher/mod.rs::restore_profile` 仅在 icube 布局（TraeWork/Trae）且恢复成功时调用，进度流输出「项目列表/最近打开已跨账号保留（项目列表 +N / 最近打开 +N）」，失败仅 warn 不阻断。7 条单测覆盖：TEXT/BLOB 两种存储、缺键整体补入、数组按 id 去重（快照项在前）、`entries` 对象按 folderUri 去重、完全一致零写入（且不产生备份文件）、非 JSON保守不改、文件缺失跳过。

### F-74 Buddy 切换时自动迁移会话到目标账号（P2，核心能力已落地，仅缺编排）✅ 已完成（2026-09-15）

- **需求概述**：切换 WorkBuddy / CodeBuddy 账号 A→B 时，把 A 名下的 AI 会话历史自动迁移到 B 名下可见可续聊，免去手动「备份 A → 会话复制 A→B」两步操作。
- **可行性结论**（2026-09-13 分析）：
  - **WorkBuddy 侧高可行**——会话迁移核心能力已在 F-44/F-45 落地：会话三件套（正文 `~/.workbuddy/projects/{workspace}/{cid}.jsonl` + 元数据 `workbuddy.db` sessions 表 + 云端映射 `edge-sync-mapping-v2.db` 的 `convmsg:{uid}`）的备份（`workbuddy_chatdata_backup`）与跨账号复制（`workbuddy_chatdata_copy`：新 UUID 重写正文 → sessions 整行克隆 → edge 映射注册到目标账号）均已实现且有 UI 弹框。**本项只做切换编排，不动迁移算法**。
  - **CodeBuddy 侧需先扩展**——`chatdata` 系列命令硬编码 `~/.workbuddy`，CodeBuddy 独立会话域 `~/.codebuddy/projects`（token 统计已扫描证实其存在）未覆盖。
  - **Trae 侧不可行**——会话真迁移已被服务端 `user_id` 归属校验证伪（§四已排除项，"幽灵会话"），维持排除结论；Trae 侧诉求由 F-69 导出存档承接，本项不涉及。
- **实现路径**：
  1. **B2 CodeBuddy 会话域扩展（前置，~1 天）**：`chatdata` 三命令增加 `app: "workbuddy" | "codebuddy"` 参数（数据目录 `wb_data_dir()` 参数化，分别指向 `~/.workbuddy` / `~/.codebuddy`；备份根 `workbuddy_chats/` 与 `codebuddy_chats/` 分离），前端账号管理按目标应用传参；
  2. **B1 切换编排（~1 天）**：设置项 `buddy_switch_migrate_chats`（默认关，设置页 Buddy 区）；`switch_account` 的 WorkBuddy/CodeBuddy 分支在**调桥之前**完成全部迁移动作——① `pool_account_id_by_auth_uid` 检测当前登录账号 id，与目标相同或检测失败则跳过；② 复用备份内部函数备份当前账号三件套（客户端关闭与切换桥天然同窗口，`graceful_kill` 幂等）；③ 立即执行 `chatdata_copy(当前, 目标)`（此时 live projects 即当前账号会话，copy 后 live 同时含 A 原件 + B 名下副本）；④ 迁移结果（N 个会话 / sessions 克隆 / 映射注册数）写入 `switch-progress` 事件流供进度页展示；
  3. **时序关键点**：全部 db 操作必须在桥的 Stop→Restore→Start 窗口之前完成（copy 后桥重启客户端，B 登录态启动即可见迁移会话）；`chatdata_copy` 自带的 `graceful_kill` 在此场景幂等无害；
  4. **实测验证步骤（含在预估内）**：双账号互切后确认 B 名下会话可见、可续聊、云端同步不报归属校验错误（edge 映射注册为客户端上传语义，若服务端校验会话归属则降级为"仅本地可见"并在 UI 明示——与 F-45 既有边界一致）。
- **边界与风险**：`edge_sync_mapping` 为宽容发现（表结构随客户端版本浮动）；目标账号从未在本机登录过时 edge db 无 `convmsg:{target}` 行可克隆，映射注册数为 0（会话正文与 sessions 克隆仍生效，云端同步待 B 首次登录后补注册）——编排前检测并降级提示；`workbuddy.db` 双写冲突由 copy 的 `INSERT OR IGNORE` 兜底。
- **参考开源项目**：无直接同类（自研能力编排）；迁移算法即本项目 F-45 实现。
- **验收**：开启设置项后，WorkBuddy 从 A 切到 B，客户端启动即见 A 的全部会话（含可续聊）；A 原件不丢失；关闭设置项行为与现状完全一致；CodeBuddy 同流程可用。
- **落地落点（2026-09-15）**：
  - **B2 会话域扩展**：`commands/workbuddy/chatdata.rs` 新增 `BuddyApp{WorkBuddy,CodeBuddy}`（`data_dir()` 指向 `~/.workbuddy` / `~/.codebuddy`，备份根 `data/workbuddy_chats` 与 `data/codebuddy_chats` 分离，`proc_kind()` 复用 `graceful_kill_app`）；四个命令（`backup`/`restore`/`info`/`copy`）新增 `app` 可选参数（空/未知回落 WorkBuddy＝旧行为），池内存在性校验保留在命令层；核心逻辑抽出为与 Tauri 无关的纯函数 `backup_chats` / `restore_chats` / `copy_chats`（供编排复用）。前端 `tauri.ts` 四接口传 `app`，`BuddyAccounts` 顶部新增「WB 会话 / CB 会话」会话域切换（切换即按域重取「已备份」徽标）。
  - **B1 切换编排**：全局设置项 `buddy_switch_migrate_chats`（默认关）；`commands/switch.rs` 在 WorkBuddy/CodeBuddy 分支、且当前账号与目标不同时构造前置作业，随 `run_in_background` 在 `run_action`（桥的 Stop→Restore→Start）**之前**执行：先 `backup_chats` 备份当前账号三件套（失败仅告警并跳过迁移，不阻断切换），再 `copy_chats(当前 → 目标)`；全程进度以 `switch-progress` 的 `stage=migrate` 行输出。当前账号判定：WorkBuddy 走 `pool_account_id_by_auth_uid`（共享 auth 文件驱动，标记兜底），CodeBuddy 走桥标记 `current_account_marker(profiles_codebuddy)` 优先（auth 文件会被 WorkBuddy 覆盖，F2-2 同因）——守卫 `expected_uid` 语义未改动。
  - **实测状态**：待真机双账号互切验证（编排与算法单测已绿：`cargo test` 396 passed）。
- **设置项位置**：设置页 Buddy 区「切换账号时自动迁移会话」卡片（勾选即存，失败原地回滚）。

### F-78 Trae OAuth 授权闭环补全（P1，半实现——源自 issue #10）

- **背景（issue #10，2026-09-13）**：用户走 OAuth 登录报 `ERR_CERT_AUTHORITY_INVALID`（www.trae.cn）且回调无法到达，WorkBuddy 侧正常。根因有二：① 本软件 MITM 代理运行时会把系统代理指向 `127.0.0.1:8899`，浏览器访问 OAuth 登录页被解密，自签 CA 未被信任即撞 SSL；② redirect_uri 指向的 `127.0.0.1:17388` **本机没有任何进程在监听**，浏览器跳转后只是"无法访问"页，需用户手动复制地址栏 URL 粘贴回来——链路从未真正闭环。
- **现状盘点（代码已实现的部分，勿重复造）**：`src-tauri/src/commands/oauth.rs` 已有 `oauth_get_login_url`（state CSRF + machine_id/device_id 生成）、`oauth_parse_callback`（宽容字段解析）、`exchange_token`（`api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken`）、`get_user_info`、`oauth_login`（vault 加密落库 + 分组）；`accounts.rs::refresh_jwt_impl` 已有 refresh_token → 新 JWT 续期（含冷却自动解冻）；前端 `OAuthLoginModal.tsx` 三步向导（打开登录页 → **手动粘贴回调 URL** → 落库）。Buddy 侧另有完整先例可对照（`workbuddy_oauth_login`：后端开浏览器 + 轮询 + 自动入池，F-50）。
- **缺口清单（"还缺什么"的准确答案）**：
  1. **本机回环监听器缺失**——`127.0.0.1:17388/authorize` 无人监听，OAuth 回调只能靠人肉复制 URL，这是"未打通"的核心；
  2. **登录链路无代理豁免**——OAuth 页在系统浏览器打开，系统代理被 MITM 端口占用时 `www.trae.cn` 流量被解密，CA 未信任即报 SSL（issue #10 直接根因）；
  3. **code 交换分支未实现**——`oauth_parse_callback` 注释自述"或可能带 code 参数需要交换"，但回调只认 `refreshToken` 参数，若上游改为标准 `code` 授权码回调则整条链路失效；
  4. **client_secret 为占位符 `"-"`**——ExchangeToken 是否强校验 secret 未验证，需经 MITM 抓包固化真实参数（与 F-24-余 同方法）；
  5. **设备标识不一致**——登录 URL 的 machine_id/device_id 每次随机生成，与 `device_map.json` 的账号稳定伪设备不对齐，OAuth 换发的 JWT 绑定设备与签到用设备不同，存在被服务端判定异动/顶替的风控隐患；
  6. **refresh_token 生命周期管理缺位**——无 expires_at / 失败计数 / 轮换旧值失效的显式标记（Buddy 侧 `refresh_token_expires_at` 已有先例），轮换失败后账号只能等签到 401 才暴露。
- **实现路径**：
  1. **批次 1（1~2 天，闭环主件）**：Rust 侧新增 `oauth_loopback.rs`——axum（已有依赖，零新增 crate）在本机 `127.0.0.1:17388` 起短生命周期 HTTP server（仅在 OAuth 流程期间监听，完成即关），GET /authorize 收到回调 → 自动调 `oauth_login` 落库 → 向浏览器返回"登录成功，可关闭此页"静态页；`OAuthLoginModal` 改为监听 `oauth-login-done` 事件自动收尾，手动粘贴 URL 降级为兜底步骤；
  2. **批次 2（0.5 天，代理豁免）**：发起 OAuth 前检测系统代理是否指向本软件 MITM 端口，登录页域名（`www.trae.cn` / `api.trae.cn` / 授权回调）加入 `device_proxy/` 模块直连白名单（PAC/bypass 列表），并在 UI 明示"OAuth 登录不走 MITM 代理"；根治形态（内嵌 WebView + 独立代理配置）留作后续增强；
  3. **批次 3（1 天，健壮性）**：`oauth_parse_callback` 增加 `code` → token 交换分支；MITM 抓包固化 ExchangeToken 真实参数（client_secret 校验行为、refresh_token 轮换语义）；machine_id/device_id 改为从 `device_map.json` 按账号稳定读取；refresh_token 生命周期字段对齐 Buddy 侧（expires_at / 失败计数 / 失效标记）。
- **参考开源项目**：`dingminhua/dsh-connect-trae`（loopback shim 接收回调的成熟形态，F-38 已引）；本项目 Buddy 侧 `workbuddy_oauth_login`（后端开浏览器 + 轮询 + 自动入池，直接对照实现）；`BlueChonk/trae-credential-reverse-engineering`（token 刷新签名情报，见 F-70，批次 3 联动核对）。
- **验收**：MITM 代理运行中（复现 issue #10 环境）发起 OAuth 登录 → 浏览器完成授权 → 应用自动弹出"账号已添加"，全程无需手动复制 URL；粘贴回调 URL 兜底路径保留可用；登录页不再出现证书告警；OAuth 账号的签到/续期与 MITM 捕获账号行为一致。
- **批次 3 收尾落地（2026-09-15）**：①`auth_saved_at` 凭证落盘时间字段补齐（RawAccount/AccountView + OAuth 登录/导入/手动添加/刷新成功四处写入，前端两处徽标展示，对齐 Buddy auth_saved_at；`access_token_expires_at` 经评估无需落盘——`jwt_exp_timestamp` 已实时解析 JWT exp 并展示）；②新增抓包调试路由 `AIWORK_OAUTH_DEBUG_PROXY` 环境变量（`oauth.rs::exchange_agent`）：设为本软件 MITM 端口时 ExchangeToken/GetUserInfo 改走代理并信任本地 CA，流量落入代理日志供固化 client_secret 校验行为与 refresh_token 轮换语义（默认不设＝直连不变）。
- **批次 3 真实协议闭环（2026-09-16，v3.5.3 主线 9 提交，全链路完成 ✅）**：以 Trae CN `main.js` 逆向实锤为最终裁决重写 AuthCode 交换链路——回调解析 `authCodeInfo`（JSON 形态，兼容旧 `refreshToken`/`code` 直传）+ PKCE（RFC 7636，S256）；AuthCode 交换**不发 DeviceProof**（其属 refreshToken 刷新场景），主变体改发 `DeviceInfo{DeviceID,MachineID,PlatformCode,DevicePublicKey(SPKI PEM),...}` + IDEVersion；端点修正 `${host}/trae/api/v3/oauth/ExchangeToken`（host 取授权页回调参数）；Timestamp 必须 JSON int；实测 AuthCode 一次业务级失败即失效；AuthCode/Code × ExchangeToken/GetToken 变体自动探测链 + 响应全量脱敏诊断。**原三项「待抓包验证项」经协议对齐实测全部裁定，不再悬置**。设备凭证与签名（F-70 情报落地）经 `icube_auth.rs` 承接：tc 信封解密提取 P-256 私钥 → DeviceProof（刷新场景备用，device_id 首选 icube 凭证，F-78 缺口清单第 5 项「设备标识不一致」同步解决）。

### F-24-余 豆包会员额度端点抓包固化（P1）✅ 已完成（2026-09-15）

- **需求概述**：豆包会员额度（套餐/到期/赠送额度）展示框架已就绪，仅剩把会员额度 XHR 端点经 MITM 抓包固化。
- **实现路径**：`device_proxy.py` 开启 + `open_doubao_app(proxyPort)` 注入 `--proxy-server` 拉起豆包客户端 → 会员页触发额度请求 → 抓包关键词 `membership|entitlement|quota|remaining|benefit` 定位端点 → 填入 `settings.doubao_quota_url` 即用。
- **落地落点（2026-09-15 收尾确认）**：端点已固化 `POST https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/`（`models.rs::default_doubao_quota_url`，代理日志实测确认；`doubao_session.rs::DEFAULT_PROBE_URL` 同源复用）；`tasks/doubao_quota.rs::parse_quota` 精确解析 2026-09 实测结构（`current_subscription` 套餐/到期/赠送 + `window_limit_section` 时段/近7天窗口百分比与重置时间）+ 宽容 dig 回退，5 条单测含实测样本；`doubao.com` 已入 `DEFAULT_TARGETS` 与 settings 默认抓包域名（旧默认自动迁移）。真机复验通过（2026-09-16）：概述页额度卡出数（会员档/到期/时段与近7天窗口百分比+重置时间），正式闭环。
- **参考开源项目**：无（端点为豆包私有；抓包链路复用本项目 MITM 基建）。

### F-38 Trae → DSH 引导（P1，不自研）

- **需求概述**：不自研 DSH 桥——引导用户安装 `dingminhua/dsh-connect-trae`（装即用：Trae 模型进 DSH + 多账号切换 + Work/通用积分只读面板）；应用内提供引导页/说明。
- **实现路径**：Trae 侧新增引导卡片（安装步骤 + 仓库链接 + 常见问题）；产品化时参照其 storage.json 发现 + loopback shim 设计。
- **参考开源项目**：`dingminhua/dsh-connect-trae`（主参照）；`corrinehu/dsh-workbuddy-connect`（同族 Buddy 版，loopback shim 加固细节）；`Wang-JQ77/dsh-trae-api`（"管理界面装进 DSH"的 Web 设置页形态，远期参照）。

### E-01 豆包对话网关——OpenAI 兼容 doubao provider（P2）

- **需求概述**：把豆包 Web 端对话能力（`POST www.doubao.com/samantha/chat/completion`）接入现有 axum 统一网关，成为与 trae/buddy/custom 并列的第四类资源池；对外暴露 `/v1/chat/completions`，多轮对话（`conversation_id` 映射表为主、消息合并兜底）、三模式（`doubao` 快速 / `doubao-think` 思考 / `doubao-expert` 专家 → `completion_option` 参数组）、思考链映射 OpenAI `reasoning_content`。风控为「验证码墙」而非拒绝服务（`710022004` → `needs_captcha`，人工过后恢复），失败模式可探测可降级。
- **实现路径**（方案 B：MITM 嗅探 + 纯算法签名，零新增外部运行时依赖）：
  1. **批次 0 探测实验（0.5~1 天，先决）**：抓一次真实对话黄金样本 → Python 重放四档签名组合（随机/真实 msToken+随机 a_bogus/真实+算法 a_bogus/原样）→ 得出风控容忍矩阵，决定签名档位；
  2. **批次 2**：Rust 移植 SM3 + RC4 + s4 自定义 base64（约 300 行，不引入新 crate），以黄金样本做同参同 UA 输出比对单测；axum 网关新增 doubao provider（Cookie 组装 `sessionid+msToken+ttwid`、FAKE_HEADERS 从真实流量采样、payload 构造、SSE→OpenAI 转换复用现有转换层、conversation_id 映射表）；
  3. **错误矩阵**：`710012001` sessionid 吊销 → 标记失效停止调度（复用探活逻辑）；`710022004` → 账号冷却 + `needs_captcha` 状态；HTTP 200 无数据流 → 计入连续失败退避升级；
  4. 反封号组合拳（限速 + 随机延迟 + 指数退避，UA 保持真实采样值）。
- **参考开源项目**：`wangchuxiaoji-oss/doubao2api`（端点/SSE 事件/风控错误码权威参考，Playwright 路线我们不采用）；`LLM-Red-Team/doubao-free-api`（OpenAI 兼容层与多账号轮换形态；其"随机签名"策略 2026 年大概率已失效，仅作历史佐证）；`Evil0ctal/Douyin_TikTok_Download_API`（`crawlers/douyin/web/abogus.py`——a_bogus 纯算法实现移植母本，注意 GPL/Apache 许可差异，learn-the-design）；`mafqla/douyin-api`（a_bogus 192 字符结构逆向文档）；`lzA6/doubao-2api`（多账号 Cookie 轮换与设备指纹静态化工程组织）。
- **验收**：OpenAI SDK 以 `base_url=http://127.0.0.1:<port>/v1` 完成流式多轮对话（快速/思考两模式）；退出某账号登录后网关 60s 内标记失效；重启应用无需重新抓包（指纹从库加载）。

### E-02 豆包指纹嗅探持久化 + a_bogus 生成器（P2，E-01 前置）

- **需求概述**：E-01 的基建前置。现有 MITM 代理扩展豆包域名过滤器，把 `msToken`（URL query + Cookie 双处）、`ttwid`/`passport_csrf_token`、`device_id`/`web_id`/`tea_uuid`（19 位设备指纹）按账号维度落库（`doubao_fingerprint`，带 `captured_at` 新鲜度）。**关键边界**：a_bogus 绑定单次请求（query + UA + 时间戳嵌入签名体），嗅探只能固定 payload 短窗重放，**必须纯算法生成**（SM3 双哈希 + RC4 固定 keystream + s4 base64，192 字符）；设备指纹必须与账号绑定且保持一致，频繁更换 device_id 是风控高危信号。
- **实现路径**：`device_proxy.py` 嗅探器扩展（与 sessionid 抓包同库同账号存储）→ 管理页展示指纹新鲜度（无指纹账号标记"未经代理采集"）→ Rust a_bogus 生成器（见 E-01 批次 2）。技术情报详见 tech-framework.md 附录 C。
- **参考开源项目**：同 E-01（abogus.py 移植母本 + doubao2api 的指纹一致性结论）。

### W-01 Work 积分（209）接入 API 网关 ❌ 已排除（2026-09-15）

- **排除说明**：**Trae 积分签到调整**——Work 积分（209）的获取与消耗规则随 Trae 侧积分/签到调整而变化，本项赖以成立的前提（209 积分池可稳定供给网关多活会话编排）与预期收益均不再成立；继续投入属于对已变动上游的无效建设。
- **处置**：不再排期（§五排序已移除），条目与 §三 专题全文保留作技术留档（通道情报 `solo_work_lite`、多活会话编排模型、N 实例运维分析对 F-67/F-75 仍有参考价值）；若 Trae 侧积分规则再次稳定且出现明确诉求，可据留档重新评估立项。
- **关联**：§三 3.5 未决项不再推进；F-67（TRAE 多实例并行）与 F-75（macOS）不再以本项为由耦合排期。

> 原独立设计文档 `work-credit-pool-design.md` 已完整并入本节（2026-09-13）。详见 §三。

### F-70 Trae tc 凭证直读 + ECDSA P-256 刷新情报核对（P2，部分完成 2026-09-16）

- **需求概述**：① Trae CN 的 `storage.json` 凭证使用自定义 "tc" 加密 = **AES-128-CBC + SHA-512**（SG 版为明文 JSON）——实现直读解密后，本机 Trae 凭证发现不再依赖 MITM 抓包；② `BlueChonk/trae-credential-reverse-engineering` 报告 TraeWork CN 凭据 4/4 解密成功 + **98 个 API 发现** + **ECDSA P-256 Token 刷新签名**——是 `refresh_jwt` 续期链路的重要底层情报，需克隆核对。
- **实现路径**：
  1. 先克隆 BlueChonk 仓库做情报核对（解密参数、ECDSA 签名细节、98 API 清单中与积分/套餐/会话相关项）；
  2. `jwt.rs` / `trae_apps.rs` 增加 tc 解密读取路径（Rust 实现 AES-128-CBC，`aes`/`cbc` crate 需评估零新增依赖红线——必要时经 Python `cryptography` 旁路，项目已依赖）；
  3. 与现有 MITM 捕获路径并存（解密成功优先，失败回退抓包），`apps_accounts_discover` 账号发现覆盖面扩大。
- **参考开源项目**：`laojichao/trae-local-api`（tc 加密格式确认 + 四版本端点路由表 + CN/SG SSE 格式差异）；`BlueChonk/trae-credential-reverse-engineering`（ECDSA P-256 + 98 API 清单）；`xhrxgr/trae-work-cn-account-manager`（同栈 Tauri 2 实现，AES-128-CBC + HMAC-SHA512 结论交叉验证）。
- **风险**：解密实现属逆向范畴，仅读本机自有凭证；接口变更由 dig() 宽容解析兜底。
- **部分落地（2026-09-16，随 F-78 批次 3 / v3.5.3）**：① 情报核对完成——无需克隆 BlueChonk，直接以 Trae CN `main.js` 逆向实锤（byteCrypto 四常量表为随安装包分发的公开混淆表、tc 信封 magic `[116,99,5,16,0,0]`、SHA-512 双轮派生 AES-128-CBC key/iv），BlueChonk 结论交叉验证；② 新增 `icube_auth.rs`——tc 信封解密提取设备 P-256 私钥（`iCubeAuthInfo://icube-dc:<deviceId>` 键，私钥只在内存流转）+ ECDSA P-256 DeviceProof（P1363 优先，20405 实测要求 PascalCase 字段），由 `commands/oauth.rs` 消费服务 OAuth 链路；③ **剩余范围**：tc 直读尚未接入 `apps_accounts_discover` 账号发现（本机凭证发现仍依赖 MITM 抓包路径），`storage.json` 凭证批量直读与「解密成功优先、失败回退抓包」双路径待做——预估由 2~3 天下调至 1~2 天。

### F-69 Trae 会话导出存档（P3）

- **需求概述**：用旧账号 JWT 调 SOLO 会话接口导出对话内容为 Markdown，按账号归档到助手数据目录，前端提供存档浏览器（按账号/日期/项目筛选）。不改变云端数据归属，零风控风险。**边界**：仅"存档"，新账号下不能继续对话——会话真迁移（场景 C）已被服务端 `user_id` 归属校验证伪，见 §四已排除项。
- **实现路径**：
  1. 抓包确认 SOLO 会话列表/详情接口（列表 + 消息体结构，工具调用/文件引用的形态）；
  2. Rust 导出命令 `tasks/trae_chats.rs`（复用 `tasks/doubao_chats.rs` 的分页拉取 → Markdown + JSON 双格式模式，落 `data/exports/trae_chats_<uid>_<ts>`）；
  3. 前端存档浏览器（复用豆包对话导出的交互形态）。
- **参考开源项目**：本项目 `doubao_export_chats`（同族先例，交互与导出格式直接复用）；`wangchuxiaoji-oss/doubao2api`（分页 anchor 游标模式参照）。

### F-79 网关流式上游异步化——reqwest + async SSE 迁移（P3，二期彻底形态）

- **需求概述**：当前网关上游 IO 为 ureq 同步栈，每条流式请求（solo `stream_chat` / WB `wb_stream_chat` / custom `custom_stream_chat`）独占一个阻塞线程直至流结束（读空闲上限 300s）。2026-09-16 已落地过渡方案（本条「背景」）：流任务迁入**独立阻塞池**（`stream_runtime`，上限 256，D-1 线程隔离）+ 调度配置内存缓存（A）+ API 日志异步写入（B）+ 用量/Key 记账削峰（C/E）——P0 级堵塞（并发流耗尽 spawn_blocking 池导致鉴权/短 IO 级联卡死；async worker 同步 SQLite）已消除。本条为二期彻底形态：上游请求迁移 `reqwest` 异步栈，流转发原生 async，不再占用任何阻塞线程。
- **实现路径**：
  1. **上游层替换**：`wb_upstream.rs` / solo 上游（`mod.rs streaming_agent`）/ custom 上游（`custom_route.rs make_custom_request`）三处 ureq Agent 迁移 reqwest Client（rustls 已是依赖栈，hyper 复用 axum 同版本）；首字超时（F-34 语义 10s）→ `tokio::time::timeout` 首帧等待；
  2. **流转发异步化**：`wb_sse::stream_forward` / `lines_with_first_byte_timeout` / `lines_with_first_byte_hedged`（F-76 对冲竞速）改为 async 迭代器（`futures::Stream`）+ `tokio::select!` 竞速；对冲第二请求与 relay 线程（现 `std::thread::spawn` relay_lines）随迁移消失；
  3. **行为不变量**：InflightGuard RAII 语义（guard 移入 async 任务）、账号级并发计数、粘性 TTL、错误分类与冷却矩阵全部保持；`DoneSignal` keep-alive ticker 与流任务同 runtime；
  4. **移除 D-1 专用池**：流任务回到主 runtime（`tokio::spawn`），`stream_runtime()` 与 `max_blocking_threads` 隔离层退役。
- **边界与风险**：① reqwest 引入为新增依赖（与 axum/hyper 版本矩阵需对齐，Tauri 主进程内已间接存在）；② SSE 解析层（`wb_sse`）协议转换逻辑可整体复用，仅行源从同步迭代器换 async stream——改动集中在上游层与行读取层；③ 生图（`wb_images`）与探活等短请求可分批迁移，不必一次到位。
- **参考开源项目**：`antonputra/tutorials`（reqwest + async SSE 流转发范式）；本项目 axum/`Body::from_stream`（响应侧已 async，仅上游侧未对齐）；`Softcreatr/json-sse`（SSE→JSON 事件桥接参照）。
- **验收**：并发 100 条长流时鉴权与 `/v1/models` 延迟无劣化（D-1 已保证，迁移后不劣化）；阻塞线程占用从「每流 1 线程」降为 ~0；TTFT/总耗时与迁移前持平（±5%）；对冲竞速与首字超时语义回归全绿。

### E-03 豆包多模态端点（P3，依赖 E-01）

- **需求概述**：E-01 之上的豆包多模态能力暴露：① **生图** `/v1/images/generations`——同端点意图路由，SSE `block_type=2074` 的 `creations[]`，`image.status==2` 完成，取 URL 优先级 `image_ori > image_raw > thumb`（**image_ori 通常无水印**）；SSE 漏图时轮询 `/message_node_info` 兜底；图生图先上传参考图得 `ref_image_key`；② **生视频** `/v1/video/generations`——两步异步（`content_type=2020` 下发 → `fin_reason.async_task.id` → `/samantha/chat/async/stream` SSE 长连接等 `2021`，1~3 分钟），需任务桥表 + 中断重连（event_id 游标）+ 账号级并发上限；③ **音乐**——同端点同步返回（30~60s）；④ **文件中转站** `/v1/files`——TOS 上传 ≤1GB 得永久 URI（免费跨机文件通道，顺带收益）。
- **实现路径**：E-01 批次 3 照原方案实施（任务桥表 `task_id ↔ 账号 ↔ 状态`、超时重连、多模态 bot_id `7338286299411103781` 路由、图片理解需先 TOS 上传）。识图/文档理解（60+ 格式）一并获得。
- **参考开源项目**：`Jackchaos2025/Doubao-Image-Proxy`（生图 SSE 解析 + message_node_info 兜底 + image_ori 优先级，固定 payload 嗅探重放佐证）；`wangchuxiaoji-oss/doubao2api`（文生图/图生图/文生视频/音乐/文件中转站全流程）。
- **边界**：**去水印仅指获取平台自有 image_ori 原图**；已烘焙进画面的 AI 水印属图像内容，网关不去除（TickClear 工具线范畴），见 §四。

### F-67 TRAE 多实例并行（P3，原 F-44，待调研）

- **需求概述**：每账号独立 `--user-data-dir` 启动多个 TRAE 实例并行运行，账号轮换不再依赖「关闭 → 快照恢复 → 重启」单实例管线，从根本上规避快照白名单随 TRAE 版本漂移失效的问题（issue #9：用户实测仅 cockpit-tools 切换成功）。
- **实现路径**（调研先行）：① TRAE 对自定义 `--user-data-dir` 的兼容性（设备指纹/登录态是否随目录隔离）；② 与现有快照管线（profiles/）、定时保活、代理注入的共存方案；③ 多实例资源占用与端口冲突。调研通过后再立项实施；单实例切换管线保留为兼容回退。
- **参考开源项目**：`jlcodes99/cockpit-tools`（开源 Tauri 应用，storage.json 路线 + 多实例隔离核心能力，只借鉴思路）；`xhrxgr/trae-work-cn-account-manager`（同栈 Tauri 2，`--user-data-dir`/`--extensions-dir` 多实例并行 + 插件共享实例隔离方案，与 W-01 多活会话编排共享底座）。
- **验收**：至少两个账号同时在线使用互不干扰；与 W-01 的 N 实例运维模型天然互补（同一底座）。

### F-07 豆包 cookie 级热切换（P3，方案 B）

- **需求概述**：不重启客户端的进程内账号热切换——读取 Cookies 表 → DPAPI 解密 → 账号池管理 → 重写 Cookies 行重加密写回。
- **实现路径**：**先验证再开发**——实测豆包客户端 cookie 在 DPAPI 之下还有一层客户端级二次加密（明文为二进制密文），离线拿不到明文 sessionid；sessionid 池化需先验证网页版 cookie 通道可行。E-02 指纹嗅探落库后可复用其凭证管理底座。
- **参考开源项目**：无成熟同类（豆包客户端二次加密为独有障碍）；Chromium Cookies DPAPI 结构处理参照 CEF 公开资料。

### F-41 trae2codex 转换器（P3，机会项）

- **需求概述**：Trae 上游为自有 `llm_utils_chat` 协议、无 Responses API，Codex CLI 不能直连；自建转换层把 Codex `/v1/responses` 请求投影到 Trae SOLO 上游——社区空白机会。
- **实现路径**：复用网关已有 WB 侧 `/v1/responses` 投影逻辑（`wb_responses.rs` 7 单测）换上游为 SOLO `llm_utils_chat`；SSE 侧在 `sse.rs` 增加 `Protocol::Responses` 的 SOLO 分支（WB 分支已就绪可对照）；Codex CLI `config.toml` 直配。
- **参考开源项目**：本项目 `wb_responses.rs`（投影逻辑直接复用）；`tonny0812/workbuddy2api`（`/v1/responses` 投影设计参照）；`muskke/trae-api-proxy`（Trae 上游的 Responses 兼容层 + 工具代执行范式）。

### F-42 workbuddy-mcp 模式（P3，机会项）

- **需求概述**：把 WorkBuddy 注册为 Codex / Claude Code / Cursor 的 MCP 工具（Model Context Protocol server），使这些客户端经 MCP 调用 Buddy 网关能力（模型对话、积分查询、账号状态）。
- **实现路径**：网关侧新增 stdio MCP server 入口（JSON-RPC 2.0，tools 暴露 chat/credits/status）；`WB_SKIP_PERMISSIONS` 权限可控；与 ck_ 子 Key 体系打通（子 Key 即 MCP 凭证）。
- **参考开源项目**：MCP 官方规范（modelcontextprotocol）；`Sliverkiss/workbuddy2api`（Buddy 网关 tools 组织形态参照）。

### F-66 CLI 多账号环境隔离（P3，机会项，评估先行）

- **需求概述**：每账号独立 `CODEX_HOME` / `CLAUDE_CONFIG_DIR` / `KIMI_CODE_HOME` 环境目录 + 全局同名变量剥离 + 「严格账号模式」（无激活账号即报错、不回落本机登录态）+ 接口返回一律脱敏——与 F-06 CLI 切号桥互补（写 token vs 隔目录），覆盖 dsh/CC 多 CLI 并行场景。
- **实现路径**：先做评估（目标 CLI 的配置目录读取优先级、与现有 `workbuddy_cli_bridge_set` 写 token 模式的冲突调和），通过后作为 CLI 桥的第二种隔离模式并存。
- **参考开源项目**：`xiaolizi0v0/CliProxy`（多 CLI 账号环境隔离 + 严格账号模式 + 接口脱敏完整范式）。

### F-75 macOS 平台支持（P3，分层归位）✅ 已完成（2026-09-26，macos_main 分支产品化）

- **需求概述**：让助手与四应用（Trae Work / Trae CN / WorkBuddy+CodeBuddy / 豆包）的账号管理、切换、签到、网关能力在 macOS 可用。**迁移策略为「分层归位」而非整体重写**。
- **v2.6 复审背景**（2026-09-15，Python/PS 移除 + SQLite 迁移 + 定时任务 Rust 化完成后的全仓代码实测）：原 Windows 依赖全景中三大项已整体消失——① 切换桥（原表最大工作量项，PS 1534 行）已 Rust 化为 `switcher/` 且平台耦合收敛到 locate/proc/machine 三模块（其余 ~70% 纯文件层天然跨平台）；② Python 网络层与豆包脚本已全量 Rust 化，**原 M2「路径中立化」阶段整体取消**；③ 存储层已迁 SQLite（rusqlite bundled 跨平台）、Job Object 随 `python.rs` 删除消失、进度事件已进程内回调（无 stdout 桥）。**预估由 6~8 周下调至 3~4 周**。
- **已天然跨平台（复审确认，无需处理）**：
  - **业务核心**：签到/积分/配额/续期/导出（`tasks/`，ureq 纯网络零 Windows 依赖）；SQLite 存储层（`store/`，bundled 编译）；API 网关（`api_server/`，hyper + rustls + rcgen 全跨平台栈）；CLI 任务模式 `--task-run`；`chatdata` 三件套与 WorkBuddy CLI 桥（`~/.workbuddy`/`~/.codebuddy` 惯例路径天然跨平台）。
  - **切换器主体**：`switcher/` 的 copy/icube/chromium/authfile/mod（快照、BOM、meta、NDJSON schema、守卫与回滚）均为纯文件层；平台差异已按设计约束在 locate/proc/machine 内部 `#[cfg]` 分支。
  - **定时任务双轨已现成**：`tasks/scheduler.rs` 应用内 Rust 原生调度器（60s tick、每日/每周任务幂等）本就是 schtasks 的补充方案——macOS 可直接以它为主，schtasks 注册面仅需 `cfg(windows)` 门控或 trait 化（schtasks/launchd），无需新写调度逻辑。
- **剩余 Windows 依赖全景**（2026-09-15 实测，按模块收敛点列出）：

| 层 | Windows 依赖（代码位置） | macOS 迁移目标 | 可行性 |
|---|---|---|---|
| **凭证保险库** | `vault.rs` DPAPI（windows-sys CryptProtect/CryptUnprotectData，`conf/vault_key.bin` 同） | `keyring` crate（Keychain/Keyring/libsecret 统一抽象），接口不变仅换实现；cfg 已就位（vault.rs:43/124/497） | 高（最小替换点，一行级） |
| **切换器三模块** | `switcher/proc.rs` WM_CLOSE（EnumWindows/PostMessageW）→ 改 sysinfo `kill_with(SIGTERM)`（Electron 同样优雅落盘）；`switcher/locate.rs` lnk crate + windows-registry + 开始菜单/桌面路径 → `/Applications/<App>.app` bundle 探测；`switcher/machine.rs` HKLM MachineGuid（windows-registry）→ `cfg(windows)` 保留，mac 等价物（IOPlatformUUID/ioreg）待研可先 skip | 高（设计已预留，`sysinfo`/`uuid` 均跨平台已在依赖） |
| **应用档案路径** | `switcher/profile.rs` + `commands/env.rs` 档案表：APPDATA/LOCALAPPDATA/USERPROFILE/ProgramFiles 环境变量 + `%VAR%` 展开 + 反斜杠路径 | `dirs` crate 统一 `~/Library/Application Support`；档案表加 os 维度（全部需 mac 实测布局，侦察先决） | 中（依赖侦察） |
| **系统代理控制** | `commands/proxy.rs` `set_win_proxy`（HKCU Internet Settings 注册表）+ `device_proxy/bypass.rs`（reg query 读代理/PAC） | `networksetup`/`scutil --proxies` 命令行等价读写 | 中高 |
| **CA 证书信任** | `commands/cert.rs` certutil + PowerShell RunAs（UAC） | `security add-trusted-cert`（用户授权弹窗），自愈 ACL 逻辑不适用需裁剪 | 高 |
| **MITM 绑定细节** | `device_proxy/mod.rs` WSASocketW + SO_EXCLUSIVEADDRUSE 独占绑定（issue #7 防孤儿假启动）→ mac `cfg(windows)` 分支换普通 bind + SO_REUSEADDR | 高（隔离在单个函数内） |
| **豆包 cookie 解密** | `tasks/doubao_session.rs`（DPAPI + AES-GCM v10，8 处 cfg(windows)）、`tasks/doubao_chats.rs`、`device_proxy/local_capture.rs`（Trae 本地捕获，自标注仅 Windows） | mac Chromium 走 Keychain `Safe Storage`（security CLI 取 key + AES-128-CBC）；**先决：豆包/Trae 是否有 mac 版**（M-1 侦察） | 中（布局未知） |
| **schtasks 注册面** | `commands/misc.rs`（task_register/launcher/GBK 解码）、`commands/workbuddy/checkin.rs`、豆包续期注册 → 启动器 `.cmd` | macOS 走已跨平台的 `tasks/scheduler.rs` 为主；如需系统级注册则 launchd LaunchAgent plist；前端 schtasks 管理入口按 cfg 隐藏 | 高 |
| **进程管理兜底** | `commands/process.rs` tasklist/taskkill 三级关闭 | 复用 `switcher/proc.rs` 的 sysinfo 实现（同构代码已存在） | 高 |
| **UI 自动化兜底** | `tasks/ui_click.rs` user32 mouse_event（windows-sys） | CGEvent（Quartz）+ 辅助功能权限；mac 无需求则 cfg 隐藏 | 中 |
| **子进程标志** | 全仓 ~30 处 `creation_flags(0x08000000)`（CREATE_NO_WINDOW）+ `CommandExt` | 命令构建 helper 按 cfg 收敛（mac 用 `process_group(0)`）；unix 侧 CommandExt 不存在，编译期即需门控 | 高（机械替换） |
| **Tauri 壳** | msi/nsis targets + icon.ico + WebView2 用户数据目录（`state.rs`）+ updater | dmg target + icns + 公证（Developer ID $99/年）+ tauri updater；WKWebView 无用户数据目录概念（对应代码 cfg 掉） | 高 |
| **杂项** | `commands/oauth.rs` random_hex BCrypt 随机源（cfg windows） | `rand`/`getrandom` crate 一行级替换 | 高 |

- **实现路径**（阶段化）：
  - **M-1 侦察（先决，3~5 天）**：①WorkBuddy/CodeBuddy/Trae/豆包 mac 版存在性与数据布局（`~/Library/Application Support/<app>`、auth 文件是否同布局）；②各应用 secret storage 形态（VS Code fork 走 Keychain，同机快照复制不受影响但需实测）；③schtasks 语义差异确认（决定 launchd 是否必要）；
  - **M0 平台抽象底座（3~5 天）**：`dirs` 统一数据目录（state.rs + profile.rs + env.rs）、命令创建 helper（cfg 抹平 CREATE_NO_WINDOW）、`keyring` 换 DPAPI（vault.rs）、档案表加 os 维度、代理/证书/绑定三处 cfg 分支；
  - **M1 切换器 mac 分支（1 周）**：proc.rs SIGTERM → locate.rs bundle 探测 → machine.rs cfg 门控，每批带快照/恢复往返单测；authfile/icube/chromium 主体零改动直接复用；
  - **M2 ~~Python 路径中立化~~**：**已取消**（Python 已全量移除，2026-09-15）；
  - **M3 打包分发（3~5 天）**：dmg + 公证 + updater 配置。
- **参考开源项目**：`tauri-apps/tauri`（v2 多平台打包与 updater 官方范式）；`hwchen/keyring-rs`（三平台 secret store 统一抽象）；本项目既有 Rust 资产（`switcher/` 平台分派设计、`tasks/scheduler.rs` 跨平台调度、sysinfo 进程管理）。
- **边界与风险**：①各目标应用 macOS 版布局是最大不确定性，M-1 侦察不通过的应用域先不支持（档案表按应用×平台灰度）；②Apple 签名公证需要 Developer ID 账号；③MITM 抓包在 macOS 需 Keychain 信任授权交互；④豆包 cookie 解密 mac 形态（Keychain Safe Storage）与 Windows DPAPI+AES-GCM 完全不同，属重写而非移植；⑤`machine.rs` 注册表层 mac 无等价物（MachineGuid），设备重置覆盖面在 mac 上降级（如实提示）。
- **验收**：macOS 上完成 Trae Work 双账号切换 + WorkBuddy 切换 + 自动签到（内置调度器）+ 网关代理全链路；Windows 行为零变化（回归：现有 Rust 测试全绿 + 前端 tsc）。
- **落地落点（2026-09-17~26，macos_main 分支 22 提交，产品化闭环 ✅）**：
  - **主体（db4efd4，09-17）**：platform 服务层 + 平台门控 + dmg 构建流水线；cert（`security add-trusted-cert`）/ proxy（networksetup/scutil）/ process（sysinfo）/ oauth / env / updater / checkin / device_proxy（bypass + ca + 绑定）等原 13 个 Windows 依赖点全量 mac 分支落地；
  - **定位与启动（5c1f79a）**：`app_locate` mac bundle 分派——`/Applications/<App>.app` 探测链 + LaunchServices 启动；**平台抽象层收敛（113350a）** + 整体黑盒复审修复（9504e3a）；
  - **真机收口（4adf498 + 97e4dd5，09-20，R1~R4 四轮）**：vault 双源密钥（Keychain 优先回落）、mac JWT 本地捕获、切换链收口 + 双平台二轮审查修复；
  - **构建与分发（3c89310 + 81a9870 + 9ab1cad + 16a9b3d）**：CI 三产物矩阵 aarch64/x64/universal + 可选自签证书签名；`package_macos.mjs` dmg 安装包 + ditto 便携 zip；产物按平台分目录（release/mac）；dmg 打包 bundle_dmg.sh 偶发失败自动清理重试；
  - **发版与更新（随 3.6.1/3.6.2）**：mac dmg 随双版本发布；内置更新竞态修复（#37）+ macOS 手动安装「先退出再拖拽」引导（e96a218，09-26）；AGENT.md §11.1 补双平台发版产物清单与 latest.json 全资产收录红线（v3.6.1 mac 更新阻断事故复盘）；
  - **合并审查跟进**：局域网网卡黑名单补 macOS 专属虚拟接口 bridge100/awdl0/llw0（3a31d17）；豆包环境配置页路径文案按平台分派（d76f900）；应用图标圆角化全套重生成并回流 main（a5fcdd6）。

### F-52 WorkBuddyProxy 模式（P3，远期）

- **需求概述**：WorkBuddy 驾驶舱 + Codex 执行器——与 F-40（Codex 协议投影进 Buddy 网关）方向相反：以 WorkBuddy 客户端为主控、Codex 作为执行后端。
- **实现路径**：远期评估，暂无排期；待 F-41 / F-42 落地后按生态需求决定。
- **参考开源项目**：`wicm84266964/Buddy2api`（多通道网关：WorkBuddy/CodeBuddy/QClaw/QwenWork/TraeWork 四类登录态统一接 OpenAI 兼容接口——与本项目"多应用统一网关"远期架构同构，验证方向可行性）。

### F-71 Trae SG 版（国际版）支持（P3，远期）

- **需求概述**：支持 Trae SG / SOLO SG（国际版）：SG 版 `storage.json` 为**明文 JSON**（无 tc 加密），端点 `a0ai-api-sg.byteintlapi.com`，SOLO 与主版共用 chat 端点仅认证路径不同；CN/SG SSE 格式有差异（CN 每条 data 前有 `event:output` 前缀，SG 无，需自适应解析）。
- **实现路径**：账号发现增加 SG 档案（`app_locate` 扩展）→ JWT/端点路由按区域分流 → `sse.rs` 解析器兼容两种格式；前置情报已由开源实现验证，实施前拉最新源码核对。
- **参考开源项目**：`laojichao/trae-local-api`（cn/solo/sg/solo-sg 四版本认证与协议全覆盖 + 端点路由表，唯一完整参照）。

### F-72 网关上游多级回退 + 分档竞速调度（P3，远期）

- **需求概述**：① 上游端点故障自动降级尝试（3 级端点回退）；② 按模型能力分 5 档、同档并发竞速、排队过长自动降档；③ 检测图片输入自动切多模态模型——网关可用性与延迟的增强方向，与现有会话粘性/五态机互补。
- **实现路径**：远期；现有五态机 + 分级重试 + 池间回退已覆盖主要故障形态，本项在多上游（E-01 落地后豆包+Trae+Buddy 三池）场景收益才显著。
- **参考开源项目**：`laojichao/trae-api`（3 级回退 + 5 档竞速 + 多模态自动切换完整实现参照）。

### F-73 网关反哺 IDE（第三方模型进 Trae）（P3，远期留档）

- **需求概述**：反向思路——让 Trae IDE 本体调用第三方模型 API（百炼 / Kimi Coding Plan 等）：hosts 劫持 `api.openai.com` → 127.0.0.1 + 443 本地反代 + CA 证书，本地伪 `/v1/models`，智能路径转换 `/v1→/v2`，多服务商配置一键切换。
- **实现路径**：远期留档；本项目 MITM 体系已覆盖同类能力（更通用），但"多服务商配置 + 一键切换激活"的交互值得借鉴；待用户需求明确再评估。
- **参考开源项目**：`mtfly/trae-switch`（DNS 劫持 + 本地 443 反代完整实现）。

---

## 三、W-01 Work 积分接入网关（专题，完整吸收原 work-credit-pool-design.md）❌ 已排除（2026-09-15：Trae 积分签到调整）

> **本专题已排除，全文保留为技术留档，不再排期。** 排除说明见 §二 W-01 条目与 §四已排除项。

### 3.1 背景与现状

现有"透明积分池 + OpenAI 接口"壳（`routes.rs / pool.rs / sse.rs / server.rs / payload.rs`）只吃 **IDE 积分（product_id 208）**——上游 `EP_LLM_CHAT = /api/agent/v3/llm_utils_chat`，余额来源 `ide_user_ent_usage` 也是 IDE 积分。**Work 积分（209）尚未接入**。

### 3.2 关键约束（为什么外部无法复刻）

- **实证**：真实 Trae SOLO 客户端发起 `create_agent_task` 返回 200 + SSE（`task_created` / `model_config`），确认成功消耗 Work 积分；请求体由原生层 `ai_agent.dll` 构造（~123KB 富上下文），闭源 `@aha-kit` 加密（仅暴露 `init`/`rawFetch`），body 加密后无法直读。
- **复刻证伪**：用真实身份（真实 JWT `data.id` + 真实 `device_id` + `machine_id` + `project_id`）复刻 → 仍返回 `4001 failed to get summary template data`。**`create_agent_task` 必须由实时 Trae SOLO 会话自身发起**；`@aha-kit` fetch 走 TTNet 隧道（MITM 只见 CONNECT 中继），真实客户端是直连 HTTPS + aha 加密体，两条路径不同。

### 3.3 可行方案结论

| 方案 | 结论 |
|---|---|
| **A. 多活会话编排 + work_transport（推荐）** | 每 Work 账号跑一个独立 `--user-data-dir` 的实时 Trae SOLO 实例，工具作编排层：按池选账号 → 驱动对应实例发起 `create_agent_task` → MITM 捕获 SSE → `sse.rs` 转 OpenAI。**增量实现：+1 个 `work_transport` 模块 + `credit_type` 配置开关，现有 routes/pool/sse/payload/server 全部不动**。★★★★★ |
| B. 复用 IDE 池过渡 | 零改动跑通形态，但消耗 IDE 积分不满足诉求（仅过渡，形态已由现有网关验证完毕） |
| C. 逆向原生层复刻 | 已证伪（4001）+ 闭源逆向合规风险，放弃 |
| D. 纯 MITM 中继 | 只能观察不能发起，不作主方案（调试价值保留） |

**实现要点**：① 新增 `src-tauri/src/api_server/work_transport.rs`（会话句柄 + `trigger_create_agent_task(account, prompt) -> SSE reader`，复用 `pool.rs` 挑选/冷却）；② 新增 Work 余额来源命令（`fetch_remaining_work_credits`，端点待查）写 `remaining_work_credits.json`；③ `routes.rs` 加 ~10 行 `credit_type: "ide" | "work"` 开关（默认 ide，行为零变化）；④ `mod.rs` 仅增常量 `EP_WORK_TASK`。

### 3.4 生态佐证（2026-09-11 调研，W-01 升 P2 依据）

`Ttungx/trae-solo-local-api` 与 `Sliverkiss/traework2api` 双独立实现交叉验证同一通道：**`llm_utils_chat + function=solo_work_lite`**（SOLO 免费对话通道，队列比 Trae CN 主通道轻）——W-01 从"方案已论证"进入"**有实现可抄**"阶段。Buddy 网关批次 2 改造完成后，其调度/熔断/协议输出层可直接复用。traework2api 的"主服务 + CLI 辅助二进制分离 + healthcheck 常驻"工程形态可作桌面端内置网关的拆分参照。

### 3.5 开发前必须解决的未决项

| # | 未决项 | 说明 |
|---|---|---|
| 1 | 如何"驱动"实时 SOLO 会话发起 `create_agent_task` | 优先确认本地命令/IPC/扩展 API；退化 headless/UI 自动化（脆弱，仅兜底）。F-67 多实例底座落地后此问题简化 |
| 2 | Work 积分余额 API 来源 | 与 IDE 的 `ide_user_ent_usage` 不同，端点/字段/鉴权需新查（BlueChonk 98 API 清单中可能有线索，见 F-70） |
| 3 | 单实例多账号可行性 | 若 `create_agent_task` 强绑定登录会话则必须 N 实例；进程内切号可大幅降运维成本，需实验确认 |

**风险**：驱动真实客户端批量消耗 Work 积分可能触及 Trae ToS，上线前需评估；N 实例资源占用/登录态维护/崩溃恢复（`pool.rs` 的 `SessionDead` 冷却机制天然适配）。

---

## 四、已排除项（明确不做，留档防重复提出）

| 项 | 排除原因 |
|---|---|
| **W-01 Work 积分（209）接入 API 网关** | **Trae 积分签到调整**（2026-09-15）：Work 积分的获取/消耗规则随 Trae 侧积分与签到机制调整而变化，多活会话编排的前提与收益均不成立；不再排期，专题留档见 §三 |
| Trae 会话真迁移/复制回放（场景 C） | 服务端按 `user_id` 归属校验拒绝（"幽灵会话"）；真迁移需以目标账号身份重建会话回放消息，非公开接口 + 风控风险 + 版本易碎——以 F-69 导出存档替代 |
| Trae state.vscdb 账号分区键跨账号合并 | 产生服务端归属校验失败的"幽灵会话"（F-68 实现红线） |
| 豆包生成图内容级去水印 | 已烘焙进画面的 AI 水印属图像内容，需 inpainting 类后处理——TickClear 工具线范畴，不在网关承诺（E-03 仅交付 image_ori 原图 URL） |
| E1 随机签名方案（方案 A） | doubao-free-api 时代产物，2026 年风控下大概率失效；仅作探测实验的对照组 |
| E1 浏览器签名方案（方案 C，Playwright） | 最稳但引入 Chromium 常驻运行时，与零新增依赖红线冲突；保留为风控升级后的 Plan B |
| GitHub Actions 免常驻签到 | 与桌面端产品定位不符（Maquer/trae-signin 模式） |
| 多通道网关统一接入（QClaw/QwenWork 等） | Buddy2api 验证了方向但当前无用户诉求，远期再议 |
| F-19 失败通知渠道扩展（企业微信/Server酱/Bark webhook） | 用户明确不做（已从待办移除；注意：WorkBuddy 蓝本 F-19 曾在企业微信/Server酱 上落地过 T3.6，豆包/Trae 侧不做） |
| 日志导出（CSV/文件导出） | 用户明确不做（T6 设计时明确排除） |
| `/v1/embeddings` 端点 | 上游无对应能力，明确返回 501，不做假实现 |
| C 方案：逆向原生层复刻 `create_agent_task` | 已证伪（4001）+ 闭源逆向合规风险（W-01 §3.3） |
| 账号池调度权重/时段轮询（T10 裁剪） | 避免过度设计 |

---

## 五、建议排序

1. **F-38 DSH 引导页** —— 成本≈0，随手带上（当前唯一未完成的 P1）
2. **E-01/E-02 豆包网关**（批次 0 探测实验 Gate 先行）—— 8~12 天，豆包积分资产化主路径
3. **F-70 tc 凭证直读（剩余收尾）** —— 解密算法与 DeviceProof 已落地（`icube_auth.rs`），仅剩账号发现直读接入，1~2 天
4. F-69 / E-03 / F-41 / F-42 / F-66 —— 按需启动
5. F-52 / F-71 / F-72 / F-73 —— 远期留档，随生态演进评估
6. ~~W-01 Work 积分接入~~ —— 已排除（2026-09-15）：Trae 积分签到调整，见 §四

> 已完成项退出排序：F-76/F-77（2026-09-14）、F-78 全批次（批次 1+2 2026-09-14 / 批次 3 2026-09-16）、F-68/F-74（2026-09-15）、F-24-余（2026-09-16 真机复验闭环）、F-75（2026-09-26，macos_main 分支产品化）。
