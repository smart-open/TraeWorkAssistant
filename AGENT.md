# AGENT.md — Trae Work 助手 v1.0

> 项目级别速查手册。给后续会话（人或 AI）秒接上下文用。任何会改契约的提交请同步更新本文档。

## 1. 一句话

Windows 桌面端多账号签到 + 登录态切换 + 设备隔离 + 账号分组工具。**所有数据仅存在 `%APPDATA%\TraeWorkAssistant\`，零外部网络**。

## 2. Quick Start

```powershell
# 仅 Windows，需要 Node 18+ / Rust stable (MSVC) / VS Build Tools C++ 工作负载 / WebView2
cd D:\open-tools\trae-work-helper
npm install
npm run tauri dev          # 开发模式（Tauri WebView 加载 Vite 5173）
npm run tauri build        # 打包 MSI + NSIS 到 src-tauri/target/release/bundle/
```

测试：

```powershell
python src-python/tests/test_auto_checkin.py   # Python 纯函数单测
cargo test                                    # Rust 单测（需先装工具链）
```

## 3. 技术栈

| 层 | 技术 |
|---|---|
| 外壳 | Tauri 2.x (Rust 1.75+ MSVC) |
| 前端 | React 18 + TypeScript 5 + Vite 5 + Tailwind 3 + Zustand 4 + Recharts 2 + lucide-react |
| 后端 | Rust (serde / chrono / tauri-plugin-{shell,dialog,notification}) |
| 辅助 | Python 3.9+（仅标准库 + `cryptography`）+ PowerShell 5.1+（系统自带） |

## 4. 目录地图

```
trae-work-helper/
├── AGENT.md                      # 本文件（项目速查）
├── README.md                     # 用户文档
├── package.json / vite.config.ts / tsconfig.json / tailwind.config.js / postcss.config.js / index.html
├── docs/                         # 设计/技术/API/开发计划 四件套
├── src/                          # 前端
│   ├── App.tsx                   # 外壳（TitleBar + Sidebar + TopBar + 页面切换 + Toaster）
│   ├── store.ts                  # Zustand 单一真相（init / 刷新 / checkin 事件归约）
│   ├── types.ts                  # 与 Rust DTO 对齐（snake_case）
│   ├── lib/tauri.ts              # invoke 封装 + 事件订阅（setupListeners）
│   ├── components/               # TitleBar/Sidebar/TopBar/Toaster/PageHeader/ui
│   └── pages/                    # Dashboard / Accounts / Checkin / Credits / Logs / Settings
├── src-tauri/
│   ├── tauri.conf.json           # 无装饰窗 / bundle.resources = ../src-python/ + ../src-ps/
│   └── src/
│       ├── main.rs               # 注册全部命令
│       ├── state.rs              # AppState（%APPDATA%\TraeWorkAssistant + python_dir）
│       ├── models.rs             # DTO（含 CheckinSummary.time 字段）
│       ├── fs_utils.rs           # 原子 read_json / write_json / mask / 时间辅助
│       ├── jwt.rs                # parse() + status_of()（>24h ok / ≤24 warn / ≤0 expired）
│       ├── python.rs             # spawn_script（注入 TRAEDATA_DIR）
│       └── commands/             # env / cert / accounts / checkin / proxy / switch / misc
├── src-python/
│   ├── device_proxy.py           # MITM 代理（env TRAEDATA_DIR、--gen-ca）
│   ├── auto_checkin.py           # 批量签到（--json-stream / --accounts / --scope）
│   ├── requirements.txt          # cryptography
│   └── tests/test_auto_checkin.py
└── src-ps/trae-switch-bridge.ps1 # #Requires RunAsAdministrator + NDJSON 步骤输出
```

## 5. Tauri 命令契约

> **调用约定**：invoke 的**顶层参数名**跟随 Rust 函数签名（驼峰不替换，参数名直接匹配）。**嵌套对象**（`opts` / `patch`）的字段名保持 **snake_case**（Tauri 默认 serde 字段名，不做 camelCase 转换）。

| 模块 | 命令 | 说明 |
|---|---|---|
| 环境 | `env_check` → `EnvStatus` | `installed/running/version/path` |
| 环境 | `open_trae_website` | 打开 `https://www.trae.cn` |
| 证书 | `cert_status` / `cert_install` | 安装走 UAC `certutil -addstore -f Root` |
| 代理 | `proxy_start(port)` / `proxy_stop()` / `proxy_status()` | ProxyStatus：`running/port/captured/started_at` |
| 账号 | `accounts_list` → `AccountView[]` | 聚合 JWT / 分组 / 设备 / 积分 / 今日 |
| 账号 | `account_add_manual(name, jwt, groupId?)` | 解析 JWT → userId 入库；userId 重复报错 |
| 账号 | `account_delete(userId, deleteProfile)` | 同时清分组 membership；`deleteProfile=true` 删 profiles/<uid> |
| 分组 | `groups_list` / `group_create(name, color)` / `group_update(id, name?, color?, order?)` / `group_delete(id)` / `group_move(userId, groupId\|null)` | 删除分组时账号回落「未分组」 |
| 签到 | `checkin_start(opts)` → NDJSON 事件 | `opts: { scope: "all"\|"group:<id>"\|"selected", user_ids?, skip_checked_in, skip_expired }` |
| 切换 | `switch_account(userId)` | 调 `trae-switch-bridge.ps1 -Action Switch -UserId -Json` |
| 设备 | `device_reset(userId)` | 删 `device_map.json[ uid ]` |
| JWT | `jwt_parse(jwt)` → `{ user_id, exp_hours, status: ok\|warn\|expired\|unknown }` |
| 日志 | `logs_query({ opts: { log_type, date, keyword, limit } })` → `LogLine[]` | `log_type` 是 snake_case；`LogLine.log_type` 也同 |
| 设置 | `settings_get()` / `settings_set(patch: Settings)` | Settings 全部 snake_case |
| 邀请 | `invite_link()` → `{ url }` | 固化 `https://www.trae.cn/work-fission/4CP3KDBT5W9A…` |
| 计划 | `task_register(time)` / `task_status()` / `task_unregister()` | `schtasks` 注册每日 `time` 自动跑 Python 签到 |

## 6. Tauri 事件（Rust → 前端）

| 事件 | payload |
|---|---|
| `proxy-log` | `string`（代理 stdout 逐行） |
| `account-captured` | `string`（新捕获的 userId） |
| `checkin-progress` | `{type:'start',total}` / `{type:'account',index,user_id,name,status:'already'\|'success'\|'fail',credits?,delta?,elapsed?,code?,message?}` / `{type:'done',ok,already,failed,total?}` |
| `switch-progress` | `string`（PowerShell NDJSON 单行） |

## 7. 数据文件

```
%APPDATA%\TraeWorkAssistant\
├── checkin_accounts.json     # { accounts: [{name, UserID, jwt, added_at, updated_at}] }
├── device_map.json           # { <userId>: { device_id, market_user_id, session_id } }
├── groups.json               # { groups: [{id,name,color,order}], membership: {<uid>:<gid>} }
├── app_settings.json         # Settings 全字段（snake_case）
├── credits_history.json      # { records: [{date,user_id,credits,delta}] }
├── checkin_summary.json      # { time, results, total_ok, already, failed } ← 注意 time 字段
├── logs/{proxy,checkin,switcher}.log
└── profiles/<user_id>/       # 切换登录态时的 TRAE Profile 快照
```

**写入约定**：`fs_utils::write_json` 用 `tmp + rename` 原子替换，避免断电损坏。

## 8. Python 约定

- **数据目录**：通过 `os.environ["TRAEDATA_DIR"]` 注入（Rust `spawn_script` 负责），缺省回退到脚本所在目录，便于独立调试。
- **NDJSON**：`--json-stream` 输出 `{"type":"start"|"account"|"done",...}` 单行 JSON，便于 Rust 解析与前端逐条渲染。
- **稳定设备 ID**：`device_map.json` 缺条目时由 `rand_digits(n, seed=user_id)` 派生；**seed 必须是字符串（user_id），每次新建 `random.Random` 对象**（不缓存，因为缓存会让第二次调用从已位置产出不同序列）；字符串 seed 跨进程通过 sha256 归一为 int，保证幂等。
- **docstring**：包含 Windows 路径（如 `%APPDATA%\…`）时**必须用 raw 字符串 `r"""…"""`**，否则会触发 `SyntaxWarning: invalid escape sequence '\T'`。

## 9. PowerShell 切换桥约定

- 必须 `#Requires -RunAsAdministrator`（UAC）。
- `-Json` 时输出 NDJSON 单行 `{"stage":"stop|backup|restore|machine|start|done|fatal","status":"info|ok|skip|running","message":"...","time":"yyyy-MM-dd HH:mm:ss"}`。
- 入口目录：`$env:LOCALAPPDATA\Programs\Trae\Trae.exe` + `$env:APPDATA\TRAE SOLO CN` + `$env:APPDATA\TraeWorkAssistant\profiles`。
- 流程：关闭 TRAE → 备份 last → 恢复目标快照 → 重置 `HKLM:\SOFTWARE\Microsoft\Cryptography\MachineGuid` → 启动 TRAE（带 `--proxy-server=127.0.0.1:8899`）。

## 10. 前端约定

- **store 单例**：`useAppStore` 聚合所有状态；`init()` 在 `App.tsx` `useEffect` 启动一次，`setupListeners` 注册事件。
- **样式**：Tailwind 3 + `darkMode:'class'`；`index.css` 的 `@layer components` 提供 `card` / `btn-primary` / `input` 等基础件。
- **snake_case**：前端类型定义（`types.ts`）的字段名与 Rust DTO 完全一致（含 `Settings`、`CheckinOpts`、`LogsOpts`、`LogLine`）。
- **路由**：极简 `useState<'dashboard'|'accounts'|'checkin'|'credits'|'logs'|'settings'>`，不引 react-router。
- **图标**：`lucide-react`；**图表**：`recharts`（`BarChart` + `Cell`）。
- **窗口**：`decorations: false` → `TitleBar` 用 `getCurrentWindow().minimize/close/toggleMaximize` + `data-tauri-drag-region`。

## 11. 常用任务 SOP

| 任务 | 路径 |
|---|---|
| 新增账号 | Accounts 页 → 「添加账号」→ 粘贴 JWT（自动 `jwt_parse` 校验）→ 选分组 → 入库 |
| 注册每日定时签到 | Settings 页 → 输入 `HH:MM` → 「注册任务」→ 写入 `schtasks /Create /TN TraeWorkAssistant_DailyCheckin /TR <py> <script> /SC DAILY /ST <time> /RL HIGHEST` |
| 切换账号 | Accounts 行 → 点击登录图标 → `switch_account(userId)` → PowerShell 桥走 NDJSON 步骤条 |
| 重置设备 ID | Accounts 行 → `device_reset(userId)` → 删除 `device_map.json[uid]` → 下次签到自动重建 |

## 12. 安全与合规（🚨）

- **零外发**：不连接任何自有后端。
- **CA 证书**：仅本地回环 `127.0.0.1:8899`，自签根 CA 需 UAC 安装到「受信任根证书颁发机构」。
- **UAC**：仅在 `cert_install` 与 `trae-switch-bridge.ps1` 提权，最小化提权面。
- **邀请链接**：固化在 Rust `INVITE_LINK` 常量，不从前端传入。
- **traework2api**：**仅作为下期 API 网关的需求/难点参考样例，不实现、不集成、不复用其代码/文件格式**。

## 13. 禁止与红线（Do NOT）

- ❌ 修改 Rust 命令嵌套参数（如 `CheckinOpts`）的字段名 → 与 Python 子进程 serde 契约耦合。
- ❌ 把 Rust 命令顶层参数改为 camelCase → Tauri 用 Rust 函数签名原名匹配。
- ❌ 引入 React Router / Redux / 额外 UI 库（shadcn 等）→ 保持依赖最小。
- ❌ 提交 `.workbuddy/`、`dist/`、`node_modules/`、`src-tauri/target/`、`__pycache__/`（已在 `.gitignore`）。
- ❌ 在 `cmd / powershell / bash` 三种 shell 之间用 `genie-trash` 删除项目目录内文件会失败 → 用 `Remove-Item -LiteralPath` 或 `cmd /c del` 兜底。
- ❌ 用 `npm run dev` 直接跑 Vite（脱离 Tauri WebView）→ 白屏 `__TAURI_INVOKE__ is not a function`，必须 `npm run tauri dev`。

## 14. 已知约束

- 仅 Windows（Tauri 2 可跨平台编译，但代理证书安装 + MachineGuid 重置只在 Windows 验证）。
- PowerShell 切换桥需 Win10/11 自带 PowerShell 5.1+。
- 沙箱无法编译 Tauri → 所有 Rust 端验证需用户在本机执行；沙箱内仅能跑 Python 单测 + 静态检查。
- Rust 首次编译拉依赖耗时长（数分钟），建议配 `rsproxy.cn` 国内镜像。

## 15. 下期（M5，未立项）

本地 API 网关（OpenAI/Anthropic 协议兼容 +按账号积分路由）。
- 仅调研、不开发；traework2api 项目**只作为参考样例**，不集成不复用其内核/文件格式。
- 待 v1.0 在真实环境跑通、用户量与积分调度策略明确后再立项。