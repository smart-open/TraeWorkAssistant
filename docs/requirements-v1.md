# Phase 1 需求规格（v1 迭代）

> 基于 `requirements-plan.md` Phase 1 的 4 项需求，结合现有代码分析细化为可执行的需求规格。

---

## 需求 1.1：添加 mchost.guru 到默认监听域名

### 用户故事

作为用户，我希望代理能监控 TRAE 的核心对话流量（`trae-api-cn.mchost.guru`），以便在代理日志中看到对话请求记录。

### 现状分析

- `models.rs` 第 117-119 行 `default_proxy_domains()` 返回 7 个域名，不含 `mchost.guru`
- `device_proxy.py` 第 144-151 行 `TARGET_DOMAINS` 同步定义，同样不含
- `state.rs` 第 48-56 行 `settings()` 在 `proxy_domains` 为空时回填默认值
- `Settings.tsx` 第 286-296 行 placeholder 展示默认域名列表

### 需求描述

将 `mchost.guru` 添加到默认监听域名列表，使代理对该域名做 MITM 解密并记录到代理日志。

### 验收标准

- Given 代理已启动，When TRAE 发起对话请求，Then 代理日志中出现 `mchost.guru` 的请求记录
- Given 用户未自定义域名列表，When 打开设置页，Then placeholder 展示包含 `mchost.guru` 的完整默认列表
- Given 代理已启动且 TRAE 信任 CA 证书，When TRAE 发起对话，Then 对话功能不受影响

### 优先级：P0

### 涉及文件

- `src-tauri/src/models.rs` — `default_proxy_domains()` 添加 `mchost.guru`
- `src-python/device_proxy.py` — `TARGET_DOMAINS` 添加 `mchost.guru`
- `src/pages/Settings.tsx` — placeholder 更新
- `src/store.ts` — `defaultSettings()` 中 `proxy_domains` 同步更新

---

## 需求 1.2：签到错误分类冷却状态机

### 用户故事

作为用户，我希望签到失败时系统能区分错误类型并自动冷却，避免无意义重试导致账号被禁封。

### 现状分析

- Python `signin()` 返回 `(success, message, code)`，仅判断 `code == 0`，非 0 一律归为失败
- `signin_with_retry()` 仅对网络层异常（`code is None`）重试，业务失败不重试
- 无任何冷却/禁用字段或逻辑（`RawAccount` 只有 name/user_id/jwt/added_at/updated_at）
- `Settings` 结构体无 cooldown 相关配置
- 前端无冷却状态展示

### 需求描述

#### 1.2.1 错误分类

在 Python `signin()` 中，根据 HTTP 状态码和响应体内容分类签到错误：

| 错误类型 | 触发条件 | 冷却时长 | 说明 |
|---------|---------|---------|------|
| `PlanLimit` | HTTP 200 但响应体 `code:1005` | 12 小时 | 额度不足，短期不会恢复 |
| `SoftRate` | HTTP 429 | 60 秒 | 限流，短冷却 |
| `SessionDead` | HTTP 401 | 永久（需重新登录） | Token 失效 |
| `NotFound` | HTTP 404 | 60 秒 | 接口暂不可用，不累计错误 |
| `Server` | HTTP 5xx | 10 分钟（累计 3 次后） | 服务器错误 |
| `Client` | 其他 4xx | 10 分钟（累计 3 次后） | 客户端错误 |

#### 1.2.2 冷却状态持久化

新增 `account_cooldowns.json` 文件，结构：

```json
{
  "cooldowns": {
    "4487568582777872": {
      "type": "PlanLimit",
      "until": 1786700000,
      "reason": "额度不足(code:1005)",
      "error_count": 0
    }
  },
  "updated_at": "2026-08-13T15:00:00"
}
```

#### 1.2.3 签到流程集成

- `checkin_start` 命令过滤账号时，跳过冷却中的账号
- Python `auto_checkin.py` 输出 NDJSON 增加 `error_type` 和 `cooldown_until` 字段
- 签到完成后，失败账号的冷却信息写入 `account_cooldowns.json`

#### 1.2.4 前端展示

- `AccountView` 增加 `cooldown_type: Option<String>` 和 `cooldown_until: Option<i64>` 字段
- 账号列表在冷却中的账号显示冷却标签和剩余时间
- 一键签到时自动跳过冷却中的账号

### 验收标准

- Given 账号签到返回 `code:1005`，When 签到完成，Then 该账号进入 12 小时冷却
- Given 账号处于冷却中，When 执行一键签到，Then 该账号被自动跳过
- Given 账号冷却到期，When 刷新账号列表，Then 冷却标签消失
- Given 账号签到返回 HTTP 401，When 签到完成，Then 该账号标记为需重新登录
- Given 应用重启，When 加载账号列表，Then 冷却状态从 `account_cooldowns.json` 恢复

### 优先级：P0

### 涉及文件

- `src-python/auto_checkin.py` — 错误分类逻辑、NDJSON 输出扩展
- `src-tauri/src/models.rs` — 新增 `AccountCooldownsFile` 结构、`AccountView` 增加冷却字段
- `src-tauri/src/commands/accounts.rs` — `build_account_views` 读取冷却状态、`checkin_start` 过滤冷却账号
- `src-tauri/src/commands/checkin.rs` — 签到完成后写入冷却状态
- `src/types.ts` — `AccountView` 增加冷却字段
- `src/pages/Accounts.tsx` — 冷却标签展示
- `src/pages/Checkin.tsx` — 签到候选列表过滤冷却账号

---

## 需求 1.3：签到自动解冻闭环

### 用户故事

作为用户，我希望账号签到成功后，如果之前因额度不足被冷却，能自动恢复可签到状态，无需手动干预。

### 现状分析

- 当前无冷却机制（依赖 1.2 实现）
- 签到成功后会调用 `refresh_remaining_credits` 刷新剩余积分
- 但不会检查冷却状态并自动清除

### 需求描述

在签到成功后增加解冻逻辑：

1. 签到成功 → 调用剩余积分查询接口
2. 若 `remaining_credits > 0` 且账号当前处于冷却状态 → 清除冷却
3. 若 `remaining_credits == 0` → 保持冷却（签到补充的积分可能已用完或签到积分尚未到账）

### 验收标准

- Given 账号因 PlanLimit 冷却中，When 次日签到成功且剩余积分 > 0，Then 冷却自动清除
- Given 账号因 PlanLimit 冷却中，When 次日签到成功但剩余积分仍为 0，Then 冷却保持
- Given 账号因 SessionDead 永久禁用，When 次日签到成功，Then 冷却不自动清除（需手动重新登录）

### 优先级：P1

### 依赖：需求 1.2

### 涉及文件

- `src-tauri/src/commands/accounts.rs` — `refresh_remaining_credits` 增加解冻逻辑
- `src-python/auto_checkin.py` — 签到成功后输出积分信息供后端判断

---

## 需求 1.4：多策略 TRAE 安装路径探测

### 用户故事

作为用户，我希望无论 TRAE 安装在哪个盘符或目录，启动/停止/重启功能都能正常工作。

### 现状分析

- Rust 端 `env.rs` 第 64-93 行已有三级探测（自定义 → 6 个候选路径 → 注册表），覆盖较全
- PowerShell `trae-switch-bridge.ps1` 第 40 行硬编码 `$env:LOCALAPPDATA\Programs\Trae\Trae.exe`，与 Rust 端不一致
- 若用户安装的是 "TRAE SOLO CN"，PowerShell 切换脚本的 `Start-Trae` 会找不到 exe

### 需求描述

统一 PowerShell 脚本的路径探测策略，与 Rust 端保持一致：

1. 优先使用设置中自定义的路径（从 `app_settings.json` 读取）
2. 多候选路径探测（与 Rust 端 `env.rs` 的 6 个候选保持一致）
3. 注册表查询回退（`HKCU` 和 `HKLM` 的 `Uninstall` 键）
4. 探测结果缓存到模块级变量，避免重复扫描

### 验收标准

- Given TRAE 安装在 `D:\Programs\TRAE SOLO CN\`，When 执行 PowerShell 切换脚本，Then 能正确启动 TRAE
- Given 用户在设置中指定了自定义路径，When 执行切换脚本，Then 使用自定义路径
- Given TRAE 未安装，When 执行切换脚本，Then 提示"未找到 TRAE 安装路径"

### 优先级：P1

### 涉及文件

- `src-ps/trae-switch-bridge.ps1` — 替换硬编码路径为多策略探测函数

---

## 需求依赖关系

```
1.1 mchost.guru 监听（独立）
1.2 签到错误分类冷却（独立）
1.3 签到自动解冻（依赖 1.2）
1.4 多策略路径探测（独立）
```

## 边界与约束

- **不修改** TRAE SOLO CN 客户端本身
- **不修改** 代理的透明隧道行为（非目标域名继续透明转发）
- 冷却状态文件 `account_cooldowns.json` 与现有 `checkin_accounts.json` 分离，避免耦合
- Python 脚本的 NDJSON 输出向后兼容（新字段可选，旧解析器不报错）
