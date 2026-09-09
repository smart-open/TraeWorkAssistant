# 优化与新功能开发计划（v2.8.4 之后）

> 创建时间：2026-09-09。本文档为本轮开发的任务清单与验收标准，按清单逐项开发。
> 约束：不主动升版本号；每项完成后跑 `cargo test` + `cargo check` + `tsc/vite build` 验证。
>
> **分支移植说明**：本计划为 trae_work_main（v2.8.4 之后 → v2.9.0）的原始开发计划，随 T1-T11 一并归档至本分支
> （fix_trae_optimization，基于 v3.2.7）。正文保留原始设计口径；其中 T2 的「主 Key 双轨校验 / 保留主 Key 展示」
> 已被 **T15 追加调整（移除主 API Key，鉴权统一走 API Keys 列表）** 取代，最终实现以
> [optimization-implementation.md](optimization-implementation.md)（含第 15 节）为准。

## 任务总览

| # | 任务 | 优先级 | 状态 |
|---|---|---|---|
| T1 | API 网关用量统计（落盘 + 统计面板） | 高 | 已完成 ✅ |
| T2 | 多 API Key + 每日配额管理 | 高 | 已完成 ✅ |
| T3 | 托盘菜单增强（一键签到 / API 启停 / 通知） | 高 | 已完成 ✅ |
| T4 | 敏感数据加密（Tauri Stronghold） | 高 | 已完成 ✅ |
| T5 | 签到失败自动重试（最多 2 轮，间隔加长） | 中 | 已完成 ✅ |
| T6 | 日志页增强（按类型清理，不做导出） | 中 | 已完成 ✅ |
| T7 | 工程欠账：前端 Vitest 测试 + 核心模块注释 | 工程 | 已完成 ✅ |
| T8 | 签到成功率趋势（落盘 + Dashboard 堆叠图） | 新增 | 已完成 ✅ |
| T9 | OpenAI 兼容端点扩展（/v1/completions） | 新增 | 已完成 ✅ |
| T10 | 账号池调度策略 + 分组筛选 | 新增 | 已完成 ✅ |
| T11 | 开机自启 + 启动静默签到 | 新增 | 已完成 ✅ |

> 已排除：通知渠道扩展（Bark/飞书等 webhook）、日志导出（用户明确不做）。

## T1 API 网关用量统计

**现状**：`ApiSharedState.total_requests` 只有内存计数，`api_logs_*.txt` 是纯文本日志，无结构化统计。

**设计**：
- 新增 `src-tauri/src/api_server/usage.rs`：按日聚合统计落盘 `data/api_usage.json`
- 维度：日期（东八区）/ 协议端点 / 模型 / 账号 uid / API Key / 成功失败 / 流式与非流式 / 耗时
- 每次请求完成即原子写盘（个人使用频率低，写放大可接受）；启动时加载，保留 90 天自动裁剪
- 鉴权中间件解析出命中的 Key ID，通过 request extensions 传递给 handler 记账
- 新命令 `api_usage_stats(days)` 返回聚合数据；前端 ApiService 页新增「用量统计」区块（recharts 图 + 明细表）

**验收标准**：
1. 发起对话请求后 `data/api_usage.json` 出现当日记录（模型/账号/Key 维度计数正确）
2. 前端可查看近 N 天请求量与成功率，服务停止后数据仍在
3. `cargo test` 通过，含 usage 聚合单元测试

## T2 多 API Key + 每日配额

**设计**：
- 新增 `data/api_keys.json`：`{ keys: [{ id, name, key, enabled, daily_limit, created_at }] }`，`daily_limit=0` 表示不限
- 兼容迁移：首次启动若文件为空且 `settings.api_key` 非空 → 迁移为「默认 Key」（settings.api_key 保留，双轨校验）
- `auth.rs` 支持多 Key 匹配（Bearer + x-api-key），超配额返回 429 + 明确 message
- 前端 ApiService 页：Key 列表（生成/启停/限额/删除/复制），保留现有主 Key 展示

**验收标准**：
1. 新增 Key 可正常鉴权，删除/禁用立即生效（下次请求）
2. 设置日限额后超限请求返回 429；限额=0 不限
3. 旧版本单 Key 配置无缝迁移

## T3 托盘菜单增强

**设计**：
- 菜单项：显示/隐藏、立即签到、启动/停止 API 服务（动态文本）、退出
- 托盘操作与主窗口页面互斥保护：签到运行中防重入（应用级 Mutex guard）
- 签到完成 / API 服务状态变化发系统通知（tauri-plugin-notification，Rust 侧）

**验收标准**：
1. 托盘「立即签到」跳过已签账号，完成后弹系统通知
2. 托盘可启停 API 服务，菜单文本随状态切换
3. 与页面内手动签到不冲突（防重入）

## T4 敏感数据加密（Stronghold）

**背景约束**：`checkin_accounts.json` 的 jwt 会被 Python 签到脚本直接读取，不能简单改字段格式。

**设计**：
- 引入 Stronghold 快照（`conf/vault.stronghold`）；vault 主密码经 Windows DPAPI 加密存放（`conf/vault_key.bin`），仅本机当前用户可解
- vault 权威存储各账号 jwt / refresh_token（record 按 uid 键）；`checkin_accounts.json` 中对应字段清空为占位
- Rust 统一走 `load_accounts` / `save_accounts` 加解密；启动时幂等迁移明文 → vault
- Python 兼容：`auto_checkin.py` 支持 `--accounts-file` 参数（默认不变），Rust spawn 前解密写临时文件、进程结束删除
- MITM 捕获新增账号（明文写入）由下次启动迁移加密

**验收标准**：
1. 迁移后 `checkin_accounts.json` 中无明文 jwt/refresh_token
2. 签到、API 网关、账号页刷新积分等所有依赖 jwt 的功能不受影响
3. 删除 vault 文件属于数据丢失场景（提示重新录入），文档注明备份建议

## T5 签到失败自动重试

**设计**：
- Rust 层按轮重试：最多 2 轮，间隔逐轮加长（第 1 轮 30s、第 2 轮 90s），仅对 failed 账号重试
- 新事件 `checkin-progress: { type:"retry", round, delay, total }`，前端 Checkin 页展示重试倒计时横幅
- 与 Python 脚本级 `--retry`（settings.retry）正交，各管一层

**验收标准**：
1. 失败账号在 30s 后自动重试第 1 轮，仍失败 90s 后第 2 轮，两轮后停止
2. UI 能看到重试提示且最终统计不重复计数（依赖 T8 的 per-uid 记账）

## T6 日志页增强

**设计**：
- 新命令 `logs_clear(log_type)`：删除指定类型日志文件（all 支持全清），文件按需重建
- Logs 页加「清理」按钮（确认弹窗 + toast 反馈）；保留天数裁剪已有（启动时 trim_logs），在设置页确认暴露该配置

**验收标准**：
1. 按类型清理后日志文件被删除且应用继续正常写新日志
2. 清理操作有确认弹窗，完成后有成功提示

## T7 工程欠账

- 引入 Vitest：`npm run test` 可跑；为纯函数（cn、delay、新增的统计格式化等）补测试
- 核心模块补头部注释：state.rs、fs_utils.rs、python.rs 等（中文、简洁）

**验收标准**：`npm run test` 全绿；核心模块有模块级文档注释。

## T8 签到成功率趋势

**设计**：
- `data/checkin_results.json`：按日 per-uid 记录最终状态（`{ days: { date: { accounts: { uid: { name, status, updated_at } } } } }`），重试轮次自然合并为最终态，保留 90 天
- done 事件落库（Rust stdout 线程内）；新命令 `checkin_trends(days)` 返回 `[{date, ok, already, failed}]`
- Dashboard 新增「近 30 天签到结果」堆叠柱状图（成功/已签/失败）

**验收标准**：
1. 签到完成后当日记录更新；重试后状态以最终一次为准
2. Dashboard 图表正确渲染，无数据显示空态

## T9 OpenAI 兼容端点扩展

**设计**：
- 新增 `POST /v1/completions`（legacy text completion）：prompt 转 user message 复用现有链路，响应包装回 completion 格式；支持非流式 + 流式（delta.content → text 块）
- `/v1/embeddings` 上游无对应能力：返回 501 + 明确错误信息（不做假实现）
- `/v1/models`、`/v1/chat/completions`、`/v1/messages` 已有，保持不变

**验收标准**：
1. `/v1/completions` 非流式与流式请求均可正常返回，字段符合 OpenAI completion 结构
2. 非法请求返回 400；`/v1/embeddings` 返回 501 + 可读 message

## T10 账号池调度策略 + 分组筛选

**设计**（范围裁剪：不做权重/时段轮询，避免过度设计）：
- `api_pool.json` 扩展：`strategy: "expire_first" | "credit_first" | "random"`（默认 expire_first，即现状）、`group_ids: []`（空=全部分组）
- 取号时按策略排序：expire_first（现状：积分先过期优先）/ credit_first（剩余积分多优先）/ random
- 同步池时过滤未选中分组的账号
- 前端 ApiService 页池配置区：策略下拉 + 分组多选，保存后需重启 API 服务生效（UI 提示）

**验收标准**：
1. 三种策略取号顺序符合预期（单测覆盖排序逻辑）
2. 分组筛选生效；旧 api_pool.json 无新字段时默认行为不变

## T11 开机自启 + 启动静默签到

**设计**：
- 引入 tauri-plugin-autostart；设置页新增「开机自启」「启动静默签到」开关
- 开启静默签到后：启动 60s 后检查，对未签到账号自动执行一轮签到（复用托盘签到链路 + 防重入锁），完成后系统通知
- 幂等：skip_checked_in=true，重复开机不会重复签

**验收标准**：
1. 开机自启开关即时生效（注册表 Run 项），便携版/安装版均可
2. 静默签到在无窗口交互下完成并通知结果；手动签到与静默签到不并发

## 收尾

- 全量验证：`cargo test` / `cargo check` / `tsc` / `vite build`
- 更新 `CHANGELOG.md`（Unreleased 段）与 `AGENT.md`（新命令契约、数据文件清单）
- **不升版本号、不打 tag、不发 Release**（等待用户明确指令）
