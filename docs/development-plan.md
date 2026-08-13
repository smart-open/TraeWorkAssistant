# 分阶段开发计划 — Trae Work 助手 v1.0

> 依据 `requirements.md` / `tech-framework.md` / `api-doc.md` / `产品设计文档.md` 执行。每阶段走 SOP 6 步（写码→测试→审查→标记→commit→下一阶段），禁止跳过审查。

## 0. 技术栈基线

| 项 | 值 |
|---|---|
| UI | React18 + TS + Tailwind + shadcn/ui + Recharts + Zustand |
| 外壳 | Tauri 2.x (Rust) |
| 核心 | Python 3.13（`auto_checkin.py`/`device_proxy.py`，迁移+增强）+ PowerShell 切换器 |
| 数据 | `%APPDATA%\TraeWorkAssistant\` 下 JSON 文件 |
| 打包 | Tauri Bundler → MSI/NSIS |

## 阶段划分

| 阶段 | 主题 | 关键交付 | 验收 |
|---|---|---|---|
| **M1** | 骨架 | Tauri 工程 + React 6 页静态布局 + 深浅色主题 + 设计 token | 页面可切换、视觉符合设计语言 |
| **M2** | 核心 | 环境检测 / CA / 代理启停+日志流 / 账号捕获 / 列表 CRUD / Python 脚本迁移增强 | 代理能启停、捕获账号、列表同步 |
| **M3** | 运营 | 分组 / 批量签到(NDJSON) / JWT 续期引导 / 设备ID重置 / 日志聚合 / TW 切换 | 勾选签到实时结果、切换闭环 |
| **M4** | 打磨 | 积分看板 / 定时任务 / 设置页 / 首次引导 / 通知 / 打包 / README | 安装包可分发、引导完整 |

## M1 · 骨架

**任务表**

| 任务 | 说明 | 交付物 | 状态 |
|---|---|---|---|
| 1.1 Tauri 脚手架 | package.json / tauri.conf.json / src-tauri(Cargo.toml,main.rs) | 可 `tauri dev` 空壳 | ✅ |
| 1.2 工程配置 | TS + Tailwind + shadcn + 路由 + 主题 | 设计 token / 深浅色 | ✅ |
| 1.3 页面骨架 | 概览/账号/签到/积分/日志/设置 + 状态栏 + 侧边导航 | 6 页静态布局 | ✅ |

**关键文件**：`src/App.tsx`、`src/pages/*`、`src/components/*`、`src/store/*`、`src-tauri/tauri.conf.json`、`src-tauri/src/main.rs`

**验收**：页面可切换、深浅色切换有效、布局留白/圆角/配色符合设计语言。

## M2 · 核心

**任务表**

| 任务 | 说明 | 交付物 | 状态 |
|---|---|---|---|
| 2.1 Python 迁移增强 | 复制 `auto_checkin.py`/`device_proxy.py` 到 `src-python/`，加 `--json-stream`/`--accounts`/`--scope` | 增强脚本 + 单测 | ✅ |
| 2.2 环境检测命令 | `env_check`/`open_trae_website`（注册表+进程枚举） | Rust 命令 | ✅ |
| 2.3 CA 证书命令 | `cert_status`/`cert_install`（UAC runas） | Rust 命令 | ✅ |
| 2.4 代理命令 | `proxy_start/stop/status` + stdout 解析 + 事件 emit | Rust 命令 | ✅ |
| 2.5 账号数据层 | `accounts_list`/`add_manual`/`delete` + 文件原子读写 | Rust 命令 + JSON 模型 | ✅ |
| 2.6 JWT 解析 | `jwt_parse`（exp/data.id，不校验签名） | Rust 命令 + 单测 | ✅ |

**关键文件**：`src-tauri/src/commands/*`、`src-tauri/src/python.rs`、`src-python/*`、`src/store/accounts.ts`

**验收**：启代理→TW 走代理登录→账号自动入库；列表与 JSON 一致；JWT 状态着色正确。

## M3 · 运营

**任务表**

| 任务 | 说明 | 交付物 | 状态 |
|---|---|---|---|
| 3.1 分组 | `groups_*` + 拖拽/筛选/批量签到本组 | 命令 + UI | ✅ |
| 3.2 批量签到 | `checkin_start` + `checkin-progress` 事件 + 进度 UI | 命令 + UI | ✅ |
| 3.3 JWT 续期引导 | 临期高亮 + 启动代理并切换流程 | UI + 流程 | ✅ |
| 3.4 设备ID重置 | `device_reset` + 二次确认弹窗 | 命令 + UI | ✅ |
| 3.5 日志聚合 | `logs_query` + 多类型色标 + 导出 | 命令 + UI | ✅ |
| 3.6 TW 切换 | PowerShell 封装 `-Action Switch -Json` + 步骤条 | 命令 + UI | ✅ |

**关键文件**：`src-tauri/src/commands/switch.rs`、`src-ps/TraeWorkAccountSwitcher.ps1`、`src/pages/Accounts.tsx`、`src/pages/Checkin.tsx`

**验收**：勾选 3 账号签到实时结果、失败不阻断；切换闭环免验证码；重置设备 ID 后重新生成。

## M4 · 打磨

**任务表**

| 任务 | 说明 | 交付物 | 状态 |
|---|---|---|---|
| 4.1 积分看板 | `credits_history.json` + Recharts 趋势/排行 | UI | ✅ |
| 4.2 定时任务 | `schtasks` 注册/卸载/查询（图形化） | 命令 + UI | ✅ |
| 4.3 设置页 | 通用/代理/签到/路径/高级 分区卡片 | UI | ✅ |
| 4.4 首次引导 | 3 步向导（检测 TW→装 CA→加账号） | 向导组件 | ✅ |
| 4.5 通知与打包 | 系统通知 + Tauri Bundler 配置 + README | 交付物 | ✅ |

**关键文件**：`src/pages/Dashboard.tsx`、`src/pages/Settings.tsx`、`src-tauri/tauri.conf.json`（bundle）、`README.md`

**验收**：安装包可分发；首次引导完整；定时任务在计划程序可见。

## 技术风险与应对

| 风险 | 应对 |
|---|---|
| Windows 专属 API（注册表/证书/计划任务）无法在本环境全量验证 | Rust 命令按 Windows API 编写，提供 `--check` 自检；关键路径加单测 mock |
| Tauri/Rust 构建需本机工具链 | 代码按 Tauri 2 规范编写；交付后由用户在 Windows 上 `npm i && npm run tauri dev` 验证 |
| Python 子进程输出解析脆弱 | NDJSON 协议 + 明确标记；解析失败降级为日志行 |

## 依赖与关键路径

```
M1 骨架 -> M2 核心（代理/捕获/数据层） -> M3 运营（分组/签到/切换） -> M4 打磨
关键路径：2.4 代理命令 -> 4.x 签到/捕获；2.6 JWT 解析 -> 3.3 续期；3.6 切换 -> 4.4 引导
```

## 增量增强（已完成）

> M1~M4 全部任务已交付。以下为在交付基础上补充的体验增强，均已完成。

| 增强项 | 说明 | 状态 |
|---|---|---|
| 账号编辑（`account_update`） | 新增 Rust 命令，支持修改账号名称和/或 JWT（更新 JWT 时重新解析 UserID）；前端编辑弹窗 | ✅ |
| JWT 查看弹窗 | 展示 UserID / 剩余有效期 / 过期时间（本地格式化）/ JWT 原文，支持一键复制 | ✅ |
| `AccountView` 扩展字段 | 新增 `jwt`（原文）、`jwt_exp_timestamp`（过期时间戳）供前端展示 | ✅ |
| `jwt_parse` 类型兼容 | `exp` 兼容整数/浮点/数字字符串；`userId` 兼容字符串/整数；返回新增 `expTimestamp` | ✅ |
| 设置页显式保存 | 本地表单 + dirty 标记 + 「保存/撤销」按钮，非自动保存；`saveSettings` 失败回滚 | ✅ |
| 签到页账号列表 | 展示候选账号表格（JWT 状态/今日签到/积分）+ 防多次签到警告；`startCheckin` 前重置状态 | ✅ |
| 日志页复制与新置顶 | 代理输出最新置顶（200 行）；代理/查询日志一键复制；查询日志导出 CSV | ✅ |
| Modal 交互 | `Escape` 关闭 + `body` 滚动锁定 | ✅ |
| App 启动加载态 | `ready` 为 false 时渲染全局 Loading | ✅ |
