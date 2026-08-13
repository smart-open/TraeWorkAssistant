# Phase 3 需求规格：OpenAI 兼容 API 服务

> 迭代基线：Phase 1（签到冷却状态机）、Phase 2（Token 刷新、双 HTTP Client、SSE 摘要、设备重置）已完成。
> 本阶段聚焦：在桌面端内嵌 OpenAI 兼容 API 反向代理，复用已有账号池与冷却状态机，参考 `traework2api` Go 实现。

---

## 需求 3.1：内嵌 OpenAI 兼容 API 服务器

### 用户故事
作为开发者/高级用户，我希望在桌面端内启动一个 OpenAI 兼容的 API 服务，这样我的其他工具（Cursor、OpenAI SDK、各种 AI 客户端）可以直接连接 TRAE SOLO 的免费对话通道，无需部署独立的 Go 服务。

### 背景
参考项目 `traework2api`（Go）已实现完整的 OpenAI 兼容代理。Phase 2 创建的 `streaming_agent()` 已预留用于此场景。现有账号管理、冷却状态机、积分查询、Token 刷新等基础设施可直接复用。

### 验收标准
- [ ] API 服务在桌面端内以 Rust 后台任务运行，无需额外进程
- [ ] 支持端口配置（默认 7864），启动时检测端口占用
- [ ] 支持 Bearer Token 鉴权（API Key 配置）
- [ ] `POST /v1/chat/completions`：流式（SSE）和非流式均支持
- [ ] `GET /v1/models`：返回可用模型列表
- [ ] `GET /healthz`：健康检查
- [ ] `GET /status`：返回账号池状态（脱敏，不含 token）
- [ ] 服务启动/停止由前端控制，状态实时反馈
- [ ] 应用退出时自动关闭 API 服务

### 技术约束
- 使用 `axum` + `tokio` 在 Tauri 进程内运行 HTTP 服务器
- 上游请求复用 `streaming_agent()`（无总超时，仅 header 超时 120s）
- SOLO SSE → OpenAI SSE 转换逻辑参考 `traework2api/internal/upstream/solosse.go`
- 请求体改写（OpenAI → SOLO `llm_utils_chat`）参考 `payload.go`

### 涉及文件
- `src-tauri/Cargo.toml` — 新增 `axum`、`tokio` 依赖
- `src-tauri/src/api_server/` — 新建模块（mod.rs, routes.rs, pool.rs, sse.rs, payload.rs）
- `src-tauri/src/state.rs` — AppState 增加 API 服务器句柄
- `src-tauri/src/main.rs` — 注册新命令、退出清理

---

## 需求 3.2：账号池管理与轮转

### 用户故事
作为用户，我希望从已有账号列表中选择部分账号加入 API 池，系统自动轮转使用，当一个账号积分不足或被限流时自动切换到下一个。

### 背景
参考 `traework2api/internal/pool/pool.go` 的账号池设计。现有 `account_cooldowns.json` 已实现冷却状态机（PlanLimit/SoftRate/SessionDead/NotFound/Server/Client），可直接复用。

### 验收标准
- [ ] 前端可勾选账号加入/移出 API 池，选择持久化到 `api_pool.json`
- [ ] 账号池挑选策略：healthy 账号中优先选积分过期时间最近者
- [ ] 请求级轮换：当前账号出错时自动尝试下一个（排除已尝试的）
- [ ] 错误分类与冷却：复用现有冷却状态机
  - 1005 PlanLimit → 12h 冷却
  - 429 SoftRate → 60s 冷却
  - 401 SessionDead → 禁用
  - 连续错误 ≥3 → 10m 冷却
- [ ] 签到解冻后自动恢复：积分 > 0 时清除冷却（SessionDead 除外）
- [ ] `/status` 端点返回各账号的冷却/积分/禁用状态

### 技术约束
- 池状态在内存中维护（HashMap<UID, PoolEntry>），冷却状态同步到 `account_cooldowns.json`
- 挑选时跳过：已禁用、冷却中、积分已过期、零积分且有过期时间的账号
- `PickExcluding` 支持请求级轮换（传入已尝试的 UID 集合）

### 涉及文件
- `src-tauri/src/api_server/pool.rs` — 账号池核心逻辑
- `src-tauri/src/models.rs` — 新增 `ApiPoolConfig`、`PoolEntry` 模型
- `src-tauri/src/commands/accounts.rs` — 复用 `calc_remaining_credits`、冷却状态

---

## 需求 3.3：API 服务配置与控制菜单

### 用户故事
作为用户，我希望在桌面端有一个"API 服务"菜单页，可以配置服务参数、选择账号池、启停服务，并实时查看服务状态。

### 验收标准
- [ ] 侧边栏新增"API 服务"菜单项（Server 图标）
- [ ] 配置卡片：端口（默认 7864）、API Key（密码输入框，可显示/隐藏）、默认模型（下拉，默认 glm-5.2）
- [ ] 账号池选择卡片：展示所有账号列表，每行带复选框，显示账号名/UID/剩余积分/冷却状态
- [ ] 启停按钮：绿色"启动"/红色"停止"，带 loading 状态
- [ ] 状态卡片：运行状态（运行中/已停止）、监听地址、总请求数、当前活跃账号、最近错误
- [ ] 配置保存到 `app_settings.json`，账号池选择保存到 `api_pool.json`
- [ ] 服务运行中修改配置需重启服务生效（提示用户）

### 涉及文件
- `src/pages/ApiService.tsx` — 新建页面
- `src/components/Sidebar.tsx` — 新增导航项
- `src/types.ts` — 新增 `ApiServiceConfig`、`PoolStatus` 类型
- `src/lib/tauri.ts` — 新增 API 绑定
- `src/store.ts` — 新增 API 服务状态
- `src-tauri/src/models.rs` — `Settings` 扩展 API 服务配置字段
- `src-tauri/src/commands/api_server.rs` — 新建命令模块

---

## 需求 3.4：SOLO SSE → OpenAI SSE 转换

### 用户故事
作为 API 使用方，我希望收到的流式响应符合 OpenAI SSE 格式，这样标准 SDK 可以直接解析。

### 背景
TRAE SOLO 的 `llm_utils_chat` 返回自定义 SSE 事件序列（metadata/output/token_usage/done/error），需转换为 OpenAI 的 `chat.completion.chunk` 格式。

### 验收标准
- [ ] 流式模式：逐事件转换，每个 `output` 事件 → `chat.completion.chunk` delta
- [ ] 非流式模式：聚合所有 `output` 为完整 `chat.completion` 响应
- [ ] `token_usage` 事件 → 末尾 chunk 的 `usage` 字段
- [ ] `done` 事件 → `finish_reason` + `[DONE]`
- [ ] `error` 事件 → SSE error + `[DONE]`，同时冷却账号
- [ ] 支持 `reasoning_content`（思考链）和 `tool_calls`（工具调用）
- [ ] 上游中断（无 done）仍输出 `[DONE]`（幂等兜底）

### 涉及文件
- `src-tauri/src/api_server/sse.rs` — SSE 解析与转换
- `src-tauri/src/api_server/payload.rs` — 请求体改写（OpenAI → SOLO）

---

## 边界与约束

### 做什么
- 在 Tauri 进程内嵌 axum HTTP 服务器
- 复用现有账号管理、冷却状态机、积分查询、Token 刷新
- 提供 OpenAI 兼容 API（/v1/chat/completions、/v1/models）
- 前端配置与控制界面

### 不做什么
- 不实现 Docker 部署（桌面端即可）
- 不实现登录闭环（已有账号管理负责）
- 不实现定时签到调度（已有签到功能）
- 不实现 dollar 用量计费（仅 credits 计费）

### 风险点
- axum 与 Tauri 的 tokio runtime 共存需验证
- ureq 同步请求在 axum 异步上下文中需 `spawn_blocking`
- SSE 流式转发的背压控制（channel 缓冲区）
