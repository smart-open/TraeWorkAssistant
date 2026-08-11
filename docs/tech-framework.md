# 技术框架方案 — Trae Work 助手 v1.0

> 详细界面/交互/数据模型见 `产品设计文档.md`。本文档给出技术选型、架构、进程契约与风险。

## 1. 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| UI | React 18 + TypeScript + Tailwind CSS + shadcn/ui + Recharts + Zustand | Web 技术栈可完整还原设计稿；状态用 Zustand；图表用 Recharts |
| 外壳 | Tauri 2.x（Rust） | 包体 8~15MB（远小于 Electron），可调用系统 API（注册表/证书/计划任务） |
| 核心逻辑 | Python 3.13（`auto_checkin.py` / `device_proxy.py`，迁移并增强） | 复用已验证的签到/代理逻辑，降低重写风险 |
| 登录态切换 | PowerShell（`TraeWorkAccountSwitcher.ps1`，加非交互模式） | 复用已验证的备份/恢复/机器码重置逻辑 |
| 打包 | Tauri Bundler → MSI / NSIS 单文件 | 含 Python embeddable 运行时与 PS 脚本 |

**不采用**：Electron（体积过大）、WPF/WinUI（样式成本高）、PyQt（视觉不达要求）。

## 2. 架构分层

```
Presentation  React + Tailwind（概览/账号/签到/积分/日志/设置）
      │  Tauri invoke
State         Zustand + Tauri Event Bus（账号/任务/日志/环境）
      │
Bridge        Tauri Commands (Rust)
 ├─ fs         JSON 读写（tmp+rename 原子替换 + 文件锁）
 ├─ proc       子进程管理（spawn/kill/stdout 流）
 ├─ sys        TW 检测 / CA 检测与安装 / UAC / 计划任务
 ├─ jwt        JWT 解析（exp / data.id），不校验签名
 └─ watch      文件监听（accounts.json 变更→推事件）
      │
Python Core   auto_checkin.py（签到） / device_proxy.py（代理）
PowerShell     TraeWorkAccountSwitcher.ps1（登录态切换）
```

原则：v1.0 分层不含 API 网关；网关为下期独立模块，立项时再以独立进程接入。

## 3. 数据模型

统一存于 `%APPDATA%\TraeWorkAssistant\`：

| 文件 | 来源 | 说明 |
|---|---|---|
| `checkin_accounts.json` | 沿用原格式 | 账号 + JWT，原脚本与 Python 核心共用 |
| `device_map.json` | 沿用原格式 | user_id → 虚拟设备身份 |
| `groups.json` | 本期新增 | 分组定义 + membership（UserID→groupId） |
| `app_settings.json` | 本期新增 | 应用设置 |
| `checkin_summary.json` | 沿用 | 最近一次签到结果 |
| `credits_history.json` | 本期新增 | 积分历史（看板绘图） |
| `logs/` | 本期新增 | proxy.log / checkin.log / switcher.log |
| `certs/` | 沿用 | 自签 CA |
| `profiles/<user_id>/` | 沿用 | 各账号 TW 登录态备份 |

账号唯一主键：`UserID`（JWT payload `data.id`，16 位数字）。分组信息独立存储，不污染原 JSON。

## 4. 进程与子进程契约

### 4.1 `auto_checkin.py`（增强）
- 沿用：读 `checkin_accounts.json` → 逐账号 `status_check`+`signin` → 写 `checkin_summary.json`。
- **新增参数**（向后兼容，默认行为不变）：
  - `--json-stream`：每账号结果以单行 JSON（NDJSON）输出，便于前端逐条渲染。
  - `--accounts UID1,UID2`：仅签指定 UserID。
  - `--scope all|group:<id>`：执行范围（供分组/勾选场景）。
- NDJSON 示例：`{"type":"start","total":6}` / `{"type":"account","index":1,"user_id":"...","name":"...","status":"already|success|fail","delta":300,"elapsed":1.2}` / `{"type":"done","ok":5,"already":0,"failed":1}`

### 4.2 `device_proxy.py`
- 环境变量：`PROXY_PORT`（默认 8899）、`AUTO_CAPTURE_JWT`（默认 1）。
- 行为：透明 MITM；捕获 `api.trae.cn` 带 `Cloud-IDE-JWT` 的请求写回 `checkin_accounts.json`（按 UserID 匹配，exp 防降级）；仅对 `checkin_credits/claim` 改写设备头。
- 日志：`proxy.log`，关键标记 `[JWT 自动更新]` / `[JWT 自动追加新账号]`，供 Rust 解析并 `emit` 事件。

### 4.3 `TraeWorkAccountSwitcher.ps1`
- **新增非交互模式**：`-Action <Switch|Save|New|Reset|List>` + `-Json` + `-UserId <id>`，以 NDJSON 输出每步进度，供 Rust 转发渲染步骤条。
- 需管理员权限（重置 MachineGuid），Rust 侧以 runas 提权启动。

## 5. 风险与应对（摘要）

| 风险 | 等级 | 应对 |
|---|---|---|
| TW 升级导致接口/路径变化 | 高 | 核心逻辑留在可热更的 Python/PS；应用内检测版本并提示 |
| TRAE 启用证书固定 | 高 | 降级：改用登录授权获取可续期凭据（见设计文档 7.3） |
| 安全软件拦截 CA/代理 | 中 | 白名单指引 + 代码签名 |
| 多账号触发风控 | 中 | 免责声明 + 签到间隔随机抖动 + 不超个人使用并发 |
| JWT 明文存储 | 中 | 下版 DPAPI 加密；导出强制加密 |
| UAC 拒绝 | 低 | 明确提示 + 手动步骤 |
| Python 缺失 | 低 | 安装包内置 embeddable Python |
