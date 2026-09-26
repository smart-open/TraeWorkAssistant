# 更新日志

本文件记录 AI Work Assistant（Web/Docker 镜像版）的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

> 分支说明：本分支（`docker_main`）为 Docker 镜像产品线（Web-only，axum 单体 + 浏览器管理面），与桌面版 `main` 分支不是同一产品形态；仅按提交语义移植系统无关的修复与功能。

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
