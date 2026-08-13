# Phase 3 开发计划：OpenAI 兼容 API 服务

## 计划概述

| 属性 | 值 |
|------|-----|
| 技术栈 | Tauri (Rust) + axum + tokio + React + TypeScript |
| 阶段数 | 5 个工作包（W1-W5） |
| 预估周期 | 2-3 天 |
| 基线文档 | `docs/requirements-v3.md` |
| 参考实现 | `D:\open-tools\traework2api`（Go） |

### 工作包划分

| 工作包 | 需求 | 优先级 | 预估时间 | 依赖 |
|--------|------|--------|---------|------|
| W1 | 3.1 内嵌 API 服务器骨架 | P0 | 0.5 天 | Phase 2 W3（streaming_agent） |
| W2 | 3.2 账号池管理与轮转 | P0 | 0.5 天 | Phase 1 W3（冷却状态机） |
| W3 | 3.4 SSE 转换与完整端点 | P0 | 1 天 | W1, W2 |
| W4 | 3.3 前端配置与控制菜单 | P0 | 0.5 天 | W1, W2 |
| W5 | 集成、配置持久化、退出清理 | P1 | 0.5 天 | W1-W4 |

---

## W1：内嵌 API 服务器骨架（需求 3.1）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T1.1 | 添加 axum + tokio 依赖 | `Cargo.toml` 修改 | 待开发 |
| T1.2 | 创建 api_server 模块结构 | `api_server/mod.rs` | 待开发 |
| T1.3 | 实现 HTTP 服务器启停 | `api_server/server.rs` | 待开发 |
| T1.4 | AppState 增加 API 服务器句柄 | `state.rs` 修改 | 待开发 |
| T1.5 | 实现 Bearer Token 鉴权中间件 | `api_server/auth.rs` | 待开发 |
| T1.6 | 实现 /healthz 端点 | `api_server/routes.rs` | 待开发 |
| T1.7 | 注册 Tauri 命令 | `commands/api_server.rs`, `main.rs` | 待开发 |

### 关键文件
- `src-tauri/Cargo.toml`：`axum = "0.7"`, `tokio = { version = "1", features = ["full"] }`
- `src-tauri/src/api_server/mod.rs`：模块入口，导出 server/pool/routes/sse/payload 子模块
- `src-tauri/src/api_server/server.rs`：`start_api_server(port, config) -> JoinHandle`，`stop_api_server(handle)`
- `src-tauri/src/state.rs`：新增 `api_server_handle: Mutex<Option<tokio::task::JoinHandle<()>>>`

### 验收标准
- `api_server_start` 命令启动 HTTP 服务，`api_server_stop` 停止
- `GET /healthz` 返回 `ok`
- 无 Bearer Token 的请求返回 401
- 应用退出时自动停止 API 服务

---

## W2：账号池管理与轮转（需求 3.2）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T2.1 | ApiPool 结构与 Pick 逻辑 | `api_server/pool.rs` | 待开发 |
| T2.2 | api_pool.json 持久化 | `models.rs` 新增 `ApiPoolFile` | 待开发 |
| T2.3 | 错误分类与冷却（复用） | `api_server/pool.rs` 引用 `account_cooldowns.json` | 待开发 |
| T2.4 | 请求级轮换 PickExcluding | `api_server/pool.rs` | 待开发 |
| T2.5 | pool_add / pool_remove / pool_list 命令 | `commands/api_server.rs` | 待开发 |

### 关键文件
- `src-tauri/src/api_server/pool.rs`：
  - `ApiPool` struct：`entries: HashMap<String, PoolEntry>`, `Mutex` 保护
  - `PoolEntry`：`jwt, uid, name, credits, credits_expire_at, disabled, err_count`
  - `pick_excluding(tried: &HashSet<String>) -> Option<&PoolEntry>`
  - `note_error(uid, kind)`, `note_success(uid)`, `cooldown(uid, kind, duration)`
- `src-tauri/src/models.rs`：
  - `ApiPoolFile { enabled_uids: Vec<String> }`
  - `PoolStatus { uid, name, credits, credits_expire_at, cooling, disabled, err_count }`

### 验收标准
- 前端可勾选账号加入/移出池，选择持久化
- 挑选策略：积分过期最近者优先，其次积分降序
- 错误时自动冷却 + 轮换下一个账号
- `/status` 返回池状态

---

## W3：SSE 转换与完整端点（需求 3.4）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T3.1 | OpenAI → SOLO 请求体改写 | `api_server/payload.rs` | 待开发 |
| T3.2 | SOLO SSE 解析器 | `api_server/sse.rs` | 待开发 |
| T3.3 | 流式转换 SOLO→OpenAI SSE | `api_server/sse.rs` | 待开发 |
| T3.4 | 非流式聚合 | `api_server/sse.rs` | 待开发 |
| T3.5 | /v1/chat/completions 端点 | `api_server/routes.rs` | 待开发 |
| T3.6 | /v1/models 端点 | `api_server/routes.rs` | 待开发 |
| T3.7 | /status 端点 | `api_server/routes.rs` | 待开发 |

### 关键文件
- `src-tauri/src/api_server/payload.rs`：
  - `prepare_body(src: &[u8]) -> Vec<u8>`：OpenAI → SOLO 改写
  - messages content string → `[{type:text, text:...}]`
  - `stream: true`, `function: "solo_work_lite"`, `config_name + model`
  - tool_calls 归一化（function → function_call）
- `src-tauri/src/api_server/sse.rs`：
  - `SoloEvent` struct：event/response/reasoning/usage/finish_reason/error
  - `parse_solo_line(event, data) -> SoloEvent`
  - `stream_convert(upstream_body, sender) -> ()`：逐行读取 SOLO SSE，转换并发送 OpenAI chunk
  - `aggregate(upstream_body) -> Value`：非流式聚合

### SOLO SSE 事件序列
```
event:metadata     → 会话元数据（含 model）
event:output       → ×N，增量内容（response + reasoning_content + tool_calls）
event:token_usage  → token 统计
event:done         → finish_reason
event:error        → 业务错误（code + message）
```

### 验收标准
- 流式：逐 output 事件 → OpenAI chunk，末尾 [DONE]
- 非流式：聚合所有 output 为完整 chat.completion
- error 事件冷却账号 + 返回 SSE error
- 上游中断仍输出 [DONE]

---

## W4：前端配置与控制菜单（需求 3.3）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T4.1 | Sidebar 新增"API 服务"导航项 | `Sidebar.tsx` 修改 | 待开发 |
| T4.2 | ViewKey 类型扩展 | `types.ts` 修改 | 待开发 |
| T4.3 | API 服务页面 | `pages/ApiService.tsx` 新建 | 待开发 |
| T4.4 | 配置表单（端口/Key/模型） | `ApiService.tsx` | 待开发 |
| T4.5 | 账号池选择卡片 | `ApiService.tsx` | 待开发 |
| T4.6 | 启停按钮 + 状态卡片 | `ApiService.tsx` | 待开发 |
| T4.7 | Tauri API 绑定 | `tauri.ts` 修改 | 待开发 |
| T4.8 | App.tsx 路由注册 | `App.tsx` 修改 | 待开发 |

### 关键文件
- `src/pages/ApiService.tsx`：三卡片布局
  - 配置卡片：端口输入、API Key 密码框、默认模型下拉
  - 账号池卡片：账号列表 + 复选框 + 积分/冷却状态
  - 状态卡片：运行状态、监听地址、请求数、活跃账号
- `src/types.ts`：
  - `ApiServiceConfig { port, api_key, default_model }`
  - `PoolStatus { uid, name, credits, credits_expire_at, cooling, disabled, err_count }`
  - `ApiServiceStatus { running, port, total_requests, active_uid, last_error }`

### 验收标准
- 前端可配置端口/API Key/默认模型
- 前端可勾选账号加入/移出池
- 启停按钮实时反馈状态
- 配置持久化到 app_settings.json

---

## W5：集成、配置持久化、退出清理

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T5.1 | Settings 扩展 API 服务字段 | `models.rs` 修改 | 待开发 |
| T5.2 | app_settings.json 读写 API 配置 | `misc.rs` settings_get/set 扩展 | 待开发 |
| T5.3 | main.rs 注册命令 + 退出清理 | `main.rs` 修改 | 待开发 |
| T5.4 | commands/mod.rs 注册模块 | `mod.rs` 修改 | 待开发 |
| T5.5 | 请求计数器与最近错误记录 | `api_server/server.rs` | 待开发 |
| T5.6 | 编译验证（tsc + cargo build） | — | 待开发 |

### Settings 扩展字段
```rust
// models.rs Settings 新增
#[serde(default = "default_api_port")]
pub api_port: u16,           // 默认 7864
#[serde(default)]
pub api_key: String,         // 默认空（空则不鉴权）
#[serde(default = "default_api_model")]
pub api_default_model: String, // 默认 "glm-5.2"
```

### 验收标准
- API 配置通过 Settings 体系持久化
- 应用退出时自动停止 API 服务
- cargo build + tsc 编译通过

---

## 技术风险与应对

| 风险 | 影响 | 应对 |
|------|------|------|
| axum 与 Tauri tokio runtime 冲突 | 服务无法启动 | 使用 Tauri 内置 runtime，不新建 runtime |
| ureq 同步阻塞 tokio 线程 | 请求卡住 | `tokio::task::spawn_blocking` 包装 ureq 调用 |
| SSE 流式背压 | 内存溢出 | channel 缓冲区设为 64，背压时阻塞读取 |
| 端口冲突 | 启动失败 | 启动前检测端口占用，返回友好错误 |
| 上游接口变更 | 请求失败 | 错误分类 + 自动轮换 + 前端提示 |
