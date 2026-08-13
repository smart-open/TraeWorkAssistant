# Phase 1 开发计划

## 技术栈基线

| 层级 | 技术 | 说明 |
|------|------|------|
| 后端 | Rust (Tauri) | 命令、状态管理、文件 IO |
| 代理 | Python 3 | MITM 代理、签到脚本 |
| 前端 | React + TypeScript + Tailwind | UI 展示 |
| 通信 | Tauri IPC (invoke/emit) | 前后端通信 |
| 数据 | JSON 文件 | 无数据库，所有状态持久化到 JSON |

## 阶段划分

| 阶段 | 需求 | 预估时间 | 依赖 |
|------|------|---------|------|
| W1 | 1.1 mchost.guru 监听域名 | 15 分钟 | 无 |
| W2 | 1.4 多策略 TRAE 路径探测 | 30 分钟 | 无 |
| W3 | 1.2 签到错误分类冷却状态机 | 2 小时 | 无 |
| W4 | 1.3 签到自动解冻闭环 | 30 分钟 | W3 完成 |

## 设计原则

- 向后兼容：NDJSON 新字段可选，旧解析器不报错
- 最小必要：不新增非必要文件、不过度工程
- 数据分离：冷却状态独立文件 `account_cooldowns.json`
- 先 Read 后 Edit：禁止盲改

## W1：mchost.guru 监听域名

### 任务表

| 任务 | 说明 | 交付物 |
|------|------|--------|
| 修改默认域名列表 | `models.rs` + `device_proxy.py` + `store.ts` | 域名列表含 mchost.guru |
| 更新 placeholder | `Settings.tsx` | 展示完整默认列表 |

### 关键文件

- `src-tauri/src/models.rs` — `default_proxy_domains()`
- `src-python/device_proxy.py` — `TARGET_DOMAINS`
- `src/store.ts` — `defaultSettings()`
- `src/pages/Settings.tsx` — placeholder

### 验收标准

- 代理日志中出现 `mchost.guru` 请求记录
- 设置页 placeholder 含 `mchost.guru`

## W2：多策略 TRAE 路径探测

### 任务表

| 任务 | 说明 | 交付物 |
|------|------|--------|
| 实现多策略探测函数 | PowerShell 脚本中替换硬编码路径 | `Find-TraeExe` 函数 |
| 读取自定义路径 | 从 app_settings.json 读取 | 支持用户自定义路径 |

### 关键文件

- `src-ps/trae-switch-bridge.ps1` — 替换 `$Script:TraeExe` 硬编码

### 验收标准

- 非默认安装路径下 Start-Trae 正常工作
- 支持从 app_settings.json 读取自定义路径

## W3：签到错误分类冷却状态机

### 任务表

| 任务 | 说明 | 交付物 |
|------|------|--------|
| Python 错误分类 | `auto_checkin.py` signin 函数增加错误类型判断 | 6 种错误类型 |
| NDJSON 输出扩展 | account 事件增加 error_type/cooldown_until 字段 | 向后兼容 |
| Rust 数据结构 | `AccountCooldownsFile` 结构 + `AccountView` 冷却字段 | 新增 models |
| 冷却状态读写 | `accounts.rs` 读取/写入 account_cooldowns.json | 新增命令 |
| 签到过滤冷却账号 | `checkin.rs` checkin_start 跳过冷却中账号 | 过滤逻辑 |
| 前端类型 | types.ts AccountView 增加冷却字段 | 类型定义 |
| 前端展示 | Accounts.tsx 冷却标签 + Checkin.tsx 过滤 | UI 组件 |

### 关键文件

- `src-python/auto_checkin.py` — `signin()` 错误分类 + NDJSON 扩展
- `src-tauri/src/models.rs` — `AccountCooldownsFile`、`AccountView` 字段
- `src-tauri/src/commands/accounts.rs` — 冷却读写 + build_account_views
- `src-tauri/src/commands/checkin.rs` — 签到过滤 + 冷却写入
- `src/types.ts` — AccountView 类型
- `src/pages/Accounts.tsx` — 冷却标签
- `src/pages/Checkin.tsx` — 候选列表过滤

### 冷却时长

| 错误类型 | 触发条件 | 冷却时长 | 累计错误 |
|---------|---------|---------|---------|
| PlanLimit | code:1005 | 12h | 否 |
| SoftRate | HTTP 429 | 60s | 否 |
| SessionDead | HTTP 401 | 永久 | 否 |
| NotFound | HTTP 404 | 60s | 否 |
| Server | HTTP 5xx | 10m（累计3次后） | 是 |
| Client | 其他 4xx | 10m（累计3次后） | 是 |

### 验收标准

- 签到失败时账号进入对应冷却
- 冷却中账号在一键签到时自动跳过
- 冷却状态持久化，重启后恢复
- 前端展示冷却标签和剩余时间

## W4：签到自动解冻闭环

### 任务表

| 任务 | 说明 | 交付物 |
|------|------|--------|
| 解冻逻辑 | refresh_remaining_credits 中检查冷却并清除 | 解冻函数 |

### 关键文件

- `src-tauri/src/commands/accounts.rs` — `refresh_remaining_credits` 增加解冻

### 解冻规则

- 签到成功 + remaining_credits > 0 + 冷却类型 != SessionDead → 清除冷却
- 签到成功 + remaining_credits == 0 → 保持冷却
- SessionDead 永不自动解冻

### 验收标准

- PlanLimit 冷却账号签到成功后有积分则自动解冻
- SessionDead 冷却账号签到成功后不自动解冻

## 技术风险

| 风险 | 应对 |
|------|------|
| mchost.guru MITM 后 TRAE 对话异常 | 确保 CA 证书已安装 |
| 冷却 JSON 文件并发写入 | Rust 端单线程写入，Python 通过 NDJSON 通知 Rust 写 |
| NDJSON 新字段兼容性 | 新字段可选，Rust 解析时用 `serde(default)` |
| PowerShell 路径探测在非 Windows 不可用 | 项目仅支持 Windows，无需跨平台 |
