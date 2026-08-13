# Phase 2 开发计划：中期增强

## 计划概述

| 属性 | 值 |
|------|-----|
| 技术栈 | Tauri (Rust) + React + TypeScript + Python 代理 + PowerShell |
| 阶段数 | 5 个工作包（W1-W5） |
| 预估周期 | 1-2 周 |
| 基线文档 | `docs/requirements-v2.md` |

### 工作包划分

| 工作包 | 需求 | 优先级 | 预估时间 | 依赖 |
|--------|------|--------|---------|------|
| W1 | 2.1 Token 自动刷新 | 高 | 2-3 天 | 无 |
| W2 | 2.2 积分过期感知调度 | 高 | 1 天 | Phase 1.2（已完成） |
| W3 | 2.3 双 HTTP Client 设计 | 中 | 0.5 天 | 无 |
| W4 | 2.4 SSE 流摘要展示 | 中 | 1-2 天 | Phase 1.1（已完成） |
| W5 | 2.5 6 层设备标识重置 | 低 | 1 天 | 无 |

---

## W1：Token 自动刷新（需求 2.1）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T1.1 | Python 代理捕获 refresh_token | `device_proxy.py` 扩展 | 待开发 |
| T1.2 | Rust 模型增加 refresh_token 字段 | `models.rs` `RawAccount` 扩展 | 待开发 |
| T1.3 | 实现 refresh_jwt 命令 | `accounts.rs` 新增命令 | 待开发 |
| T1.4 | build_account_views 自动刷新检查 | `accounts.rs` 修改 | 待开发 |
| T1.5 | 并发安全 Mutex | `state.rs` 增加刷新锁 | 待开发 |
| T1.6 | 前端类型和 API 绑定 | `types.ts` `tauri.ts` | 待开发 |
| T1.7 | 前端展示刷新状态 | `Accounts.tsx` | 待开发 |

### 关键文件
- `src-python/device_proxy.py`：在 JWT 捕获逻辑中增加 refresh_token 提取
- `src-tauri/src/commands/accounts.rs`：新增 `refresh_jwt` 命令
- `src-tauri/src/state.rs`：新增 `jwt_refresh_lock: Mutex<()>`

### 验收标准
- JWT 过期前 24h 自动刷新
- 刷新失败回退旧 token
- 并发安全

---

## W2：积分过期感知调度（需求 2.2）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T2.1 | calc_remaining_credits 返回过期时间 | `accounts.rs` 修改 | 待开发 |
| T2.2 | 缓存文件和模型扩展 | `models.rs` 修改 | 待开发 |
| T2.3 | 签到排序逻辑 | `checkin.rs` 修改 | 待开发 |
| T2.4 | 前端展示过期时间列 | `Accounts.tsx` 修改 | 待开发 |

### 关键文件
- `src-tauri/src/commands/accounts.rs`：`calc_remaining_credits` 返回 `(f64, Option<i64>)`
- `src-tauri/src/commands/checkin.rs`：签到前按过期时间排序

### 验收标准
- 签到顺序按过期时间升序
- 过期时间 < 24h 高亮

---

## W3：双 HTTP Client 设计（需求 2.3）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T3.1 | 提取 short_agent / streaming_agent | `accounts.rs` 或 `state.rs` | 待开发 |
| T3.2 | 现有请求迁移到 short_agent | `accounts.rs` 修改 | 待开发 |

### 验收标准
- 短请求 120s 超时
- 流式请求仅 header 超时

---

## W4：SSE 流摘要展示（需求 2.4）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T4.1 | Python 代理 SSE 摘要解析 | `device_proxy.py` 修改 | 待开发 |
| T4.2 | 代理日志增加摘要字段 | `ProxyRequestLogger` 修改 | 待开发 |
| T4.3 | Rust 解析摘要信息 | `misc.rs` 修改 | 待开发 |
| T4.4 | 前端详情弹窗展示摘要 | `Logs.tsx` 修改 | 待开发 |

### 验收标准
- llm_utils_chat 请求展示模型和 token 用量
- 不记录完整对话内容

---

## W5：6 层设备标识重置（需求 2.5）

### 任务表

| 任务 | 说明 | 交付物 | 状态 |
|------|------|--------|------|
| T5.1 | PowerShell 实现 6 层重置 | `trae-switch-bridge.ps1` | 待开发 |
| T5.2 | Rust 命令封装 | `misc.rs` 新增命令 | 待开发 |
| T5.3 | 前端按钮 | `Settings.tsx` 修改 | 待开发 |

### 验收标准
- 6 层标识全部重置
- 注册表操作优雅降级

---

## 技术风险与应对

| 风险 | 影响 | 应对 |
|------|------|------|
| refresh_token 捕获失败 | 无法自动续期 | 回退手动模式，前端提醒 |
| ExchangeToken 接口变更 | 刷新失败 | 失败时回退旧 token |
| SSE 解析兼容性 | 摘要缺失 | 仅提取已知字段，未知事件忽略 |
| 注册表权限不足 | 设备标识重置不完整 | 优雅降级，提示用户手动操作 |
