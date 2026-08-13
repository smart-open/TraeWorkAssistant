# Phase 2 需求规格：中期增强

> 迭代基线：Phase 1 已完成（mchost.guru 监听、签到冷却状态机、自动解冻、多策略路径探测）。
> 本阶段聚焦：Token 续期、智能调度、HTTP 客户端分离、SSE 流摘要、设备标识重置。

---

## 需求 2.1：Token 自动刷新（refresh_token → ExchangeToken）

### 用户故事
作为多账号用户，我希望 JWT 过期前自动续期，这样我不必每 13 天手动重新抓取 token。

### 背景
当前从代理流量捕获的 JWT 只有 accessToken，13 天过期后必须手动重新抓取。TRAE OAuth 流程中存在 refresh_token，可通过 `POST https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken` 续期。

### 验收标准
- [x] 代理流量中捕获到 refresh_token 时，自动写入 `checkin_accounts.json` 的 `refresh_token` 字段
- [x] refresh_token 更新带版本号防降级（新 token 的 exp 必须 > 旧的）
- [x] JWT 过期前 24h 自动调用 ExchangeToken 刷新 accessToken
- [x] 刷新成功后原子写回新 accessToken + 新 refresh_token（旧 refresh_token 立即失效）
- [x] 刷新失败时回退旧 token，不改写字段，前端展示刷新失败状态
- [x] 并发安全：使用 Mutex 防止多个并发请求同时 ExchangeToken
- [x] 前端账号列表展示 JWT 剩余有效期和"自动刷新"状态标签

### 技术约束
- ExchangeToken 请求体：`{"ClientID":"en1oxy7wnw8j9n","RefreshToken":"<rt>","ClientSecret":"-","UserID":""}`
- refresh_token 每次刷新后轮换，旧 token 立即失效
- 并发刷新需持锁重查（double-check pattern）

### 涉及文件
- `src-python/device_proxy.py` — 扩展 JWT 捕获逻辑，同时捕获 refresh_token
- `src-tauri/src/models.rs` — `RawAccount` 增加 `refresh_token` 字段
- `src-tauri/src/commands/accounts.rs` — 新增 `refresh_jwt` 命令，`build_account_views` 增加自动刷新检查
- `src-tauri/src/state.rs` — 增加 JWT 刷新锁
- `src/types.ts` / `src/lib/tauri.ts` — 前端类型和 API 绑定
- `src/pages/Accounts.tsx` — 展示刷新状态

---

## 需求 2.2：积分过期感知调度

### 用户故事
作为多账号用户，我希望系统优先签到积分即将过期的账号，避免积分浪费。

### 背景
当前签到对所有账号一视同仁。剩余积分查询接口的响应中包含每个权益包的 `expire_time`，可用于排序。

### 验收标准
- [x] `calc_remaining_credits` 扩展返回最近过期的 `expire_time`（timestamp）
- [x] `remaining_credits.json` 增加 `expire_times` 字段存储各账号最近过期时间
- [x] `AccountView` 增加 `credits_expire_at` 字段
- [x] 签到时按 `credits_expire_at` 升序排列（最近过期的优先签到）
- [x] 过期时间相同的账号按剩余积分降序排列
- [x] 账号列表展示积分过期时间列
- [x] 过期时间 < 24h 的账号高亮提醒（amber 色标）

### 技术约束
- 取所有权益包中最近的 `expire_time` 作为该账号的"积分过期时间"
- 过期时间 > 当前时间戳才有效
- 无积分或无过期时间的账号排在最后

### 涉及文件
- `src-tauri/src/commands/accounts.rs` — `calc_remaining_credits` 扩展，`build_account_views` 填充过期时间
- `src-tauri/src/models.rs` — `RemainingCreditsFile` 增加 `expire_times`，`AccountView` 增加 `credits_expire_at`
- `src-tauri/src/commands/checkin.rs` — 签到排序逻辑
- `src/types.ts` / `src/pages/Accounts.tsx` — 前端展示

---

## 需求 2.3：双 HTTP Client 设计

### 用户故事
作为开发者，我希望短请求和流式请求使用分离的超时策略，避免短请求因超时不当失败或流式请求被截断。

### 背景
当前所有 HTTP 请求（签到、积分查询、Token 刷新）使用 `ureq` 默认超时。未来 SSE 流式对话需要无总超时。

### 验收标准
- [x] 短请求 Agent：总超时 120s，用于签到/积分查询/Token 刷新
- [x] 流式 Agent：无总超时，仅 `response_header_timeout: 120s`
- [x] 两者共享连接池配置（`max_idle_connections: 20`）
- [x] 现有 `calc_remaining_credits` / `refresh_jwt` 使用短请求 Agent
- [x] 未来 SSE 请求可使用流式 Agent（预留接口）

### 技术约束
- `ureq::Agent` 通过 `AgentBuilder` 配置
- 流式 Agent 预留给 Phase 3 的 OpenAI 兼容 API 使用

### 涉及文件
- `src-tauri/src/commands/accounts.rs` — 提取 `short_agent()` / `streaming_agent()` 工厂函数
- `src-tauri/src/state.rs` — AppState 存储 Agent 实例

---

## 需求 2.4：代理日志 SSE 流摘要展示

### 用户故事
作为用户，我希望在代理日志中看到 TRAE 对话请求的模型和 token 用量摘要，而不需要查看完整对话内容。

### 背景
添加 `mchost.guru` 到监听域名后，代理会捕获 TRAE 对话请求（`/api/agent/v3/llm_utils_chat`）。SSE 流式响应的完整内容太大，应提取摘要。

### 验收标准
- [x] 代理日志中 `llm_utils_chat` 请求展示模型名称、消息数量
- [x] 响应侧解析 SSE 流，提取 `token_usage`（prompt/completion/total tokens）
- [x] 不记录完整对话内容（隐私 + 体积）
- [x] 代理日志详情弹窗中展示结构化摘要信息
- [x] 代理日志列表的 `size` 列对 SSE 请求展示 token 用量而非字节数

### SSE 事件格式
```
event:metadata        → 会话元数据（含 model）
event:output          → ×N，增量内容（忽略内容，仅计数）
event:token_usage     → token 统计
event:done            → 结束信号
```

### 涉及文件
- `src-python/device_proxy.py` — `forward_upstream` 增加 SSE 摘要解析，`ProxyRequestLogger` 增加摘要字段
- `src-tauri/src/commands/misc.rs` — `parse_proxy_entry` 解析摘要信息
- `src/pages/Logs.tsx` — 代理日志详情弹窗展示摘要

---

## 需求 2.5：6 层设备标识重置

### 用户故事
作为多账号用户，我希望新建账号时能完整重置设备标识，防止多账号被服务端关联。

### 背景
当前仅通过 MITM 代理改写签到接口的 `x-device-id`。对于新注册账号场景，需要从文件层重置设备标识。

### 验收标准
- [x] "重置设备标识"功能执行后，6 层标识全部重置
- [x] TRAE 启动后自动用新设备 ID 注册
- [x] 不影响已有账号的登录态
- [x] 注册表操作需管理员权限时优雅降级（提示但不阻断）

### 6 层重置清单
1. `machineid` 文件 — 替换为新的 hex32 UUID
2. `storage.json` 中 `telemetry.machineId` / `telemetry.sqmId` — 替换
3. `storage.json` 中 `aha.device.device_id` — 替换
4. `aha/TinyStorage` 中 `device_id` — 清除
5. 注册表 `HKLM:\SOFTWARE\Microsoft\Cryptography\MachineGuid` — 替换（需管理员）
6. `trae-webview` 追踪数据（Cookies/Local Storage/Session Storage）— 清除

### 涉及文件
- `src-ps/trae-switch-bridge.ps1` — 新增 `Reset-DeviceIdsOnly` 函数
- `src-tauri/src/commands/misc.rs` — 新增 `device_full_reset` 命令
- `src/pages/Settings.tsx` — 新增"重置设备标识"按钮
