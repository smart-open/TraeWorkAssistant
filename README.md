# Trae Work 助手

> 本地优先 · 桌面端多账号管理工具 · Tauri 2 + React 18 + TypeScript + Tailwind

Trae Work 助手是面向 Windows 的桌面端应用，把「多账号签到 + 登录态切换 + 设备隔离 + 账号分组」整合到一个简约大方的界面里。所有账号、登录态、设备 ID、积分数据**只存在本机**（`%APPDATA%\TraeWorkAssistant`），不上传任何服务器。

> ⚠️ 本项目**仅用于管理本人合法持有的多个 Trae Work 账号**。请遵守 Trae Work 用户协议与相关法律法规。

---

## 功能一览

| 模块 | 功能 |
| --- | --- |
| 环境 | 检测/引导安装 Trae Work、打开官网 |
| 证书 | 一键安装 MITM 代理 CA 证书到「受信任根证书颁发机构」（UAC） |
| 代理 | 启动/停止本地 MITM 代理，按账号注入独立 `x-device-id` 绕过「每设备每天」配额 |
| 账号 | 列表 + 分组筛选 + 粘贴 JWT 手动添加 + 删除（含 TRAE Profile 清理） + 重置设备 ID + 切换账号登录态 + 实时刷新 |
| 分组 | 新建/重命名/调色/删除，账号可自由分组或回落到未分组 |
| 签到 | 全部 / 指定分组 / 手动勾选 + 跳过今日已签 + 跳过 JWT 过期 + 实时进度条（success/already/fail）|
| 积分 | Top 8/Top 12 排行柱状图 + 列表 + 邀请得 5000 积分 |
| 日志 | 代理/签到/切换日志查询（关键字、类型） + 实时代理流 |
| 设置 | 主题 / 端口 / 自动启代理 / 跳过策略 / 通知 / 语言 / Windows 计划任务 / 邀请链接 |
| 切换 | 非交互 PowerShell 桥（关闭 TRAE → 备份 last → 恢复目标快照 → 重置 MachineGuid → 带代理启动 TRAE）|

---

## 技术栈

- **桌面框架**：Tauri 2.x（Rust 后端 + WebView 前端）
- **前端**：React 18 + TypeScript + Vite 5 + Tailwind CSS 3 + Zustand + Recharts + lucide-react
- **后端**：Rust（serde / chrono / tauri's plugin-shell/dialog/notification）
- **辅助脚本**：Python 3（`device_proxy.py` + `auto_checkin.py`，纯标准库 + `cryptography`）
- **系统集成**：PowerShell 5.1+（非交互切换桥，`#Requires -RunAsAdministrator`）

### 目录结构

```
trae-work-helper/
├── docs/                          # 产品/技术/API/开发计划文档
├── package.json                   # 前端依赖与脚本
├── vite.config.ts
├── tsconfig.json
├── tailwind.config.js
├── postcss.config.js
├── index.html
├── src/                           # 前端源码
│   ├── App.tsx                    # 外壳（TitleBar + Sidebar + TopBar + 页面）
│   ├── main.tsx
│   ├── index.css                  # Tailwind + 设计 token + 组件层
│   ├── types.ts                   # 与 Rust DTO 对齐
│   ├── store.ts                   # Zustand 全局状态 + init/刷新/事件归约
│   ├── lib/
│   │   ├── tauri.ts               # invoke 封装 + 事件订阅
│   │   └── cn.ts
│   ├── components/
│   │   ├── TitleBar.tsx           # 自定义无装饰窗 + 窗口控件
│   │   ├── Sidebar.tsx
│   │   ├── TopBar.tsx             # 状态条 + 启动/停止代理
│   │   ├── Toaster.tsx
│   │   ├── PageHeader.tsx
│   │   └── ui.tsx                 # Card/Badge/Progress/Modal/EmptyState/StatCard/Spinner
│   └── pages/
│       ├── Dashboard.tsx          # 概览（统计 + 积分榜 + 邀请）
│       ├── Accounts.tsx           # 账号 + 分组 + 添加（JWT 自动解析）
│       ├── Checkin.tsx            # 一键签到（实时进度）
│       ├── Credits.tsx            # 积分看板
│       ├── Logs.tsx               # 日志查询 + 实时代理输出
│       └── Settings.tsx           # 主题/端口/计划任务/邀请/关于
├── src-tauri/                     # Rust 后端
│   ├── Cargo.toml
│   ├── tauri.conf.json            # productName/identifier/无装饰窗/bundle(msi+nsis)/resources
│   ├── build.rs
│   └── src/
│       ├── main.rs                # 注册所有命令
│       ├── state.rs               # AppState（data_dir/python_dir/python_exe）
│       ├── models.rs              # AccountView / Group / Settings / DeviceMap / CreditsFile / CheckinSummary
│       ├── fs_utils.rs            # 原子 read_json / write_json / mask / 时间
│       ├── jwt.rs                 # parse() + status_of()
│       ├── python.rs              # spawn_script（注入 TRAEDATA_DIR）
│       └── commands/
│           ├── env.rs             # env_check / open_trae_website
│           ├── cert.rs            # cert_status / cert_install（UAC runas）
│           ├── proxy.rs           # proxy_start/stop/status（事件 proxy-log/account-captured）
│           ├── accounts.rs        # accounts_list/add_manual/delete + groups_* + resolve_user_ids
│           ├── checkin.rs         # checkin_start（事件 checkin-progress/done）
│           ├── switch.rs          # switch_account（事件 switch-progress）
│           └── misc.rs            # device_reset / jwt_parse / logs_query / settings_* / invite_link / task_*
├── src-python/                    # Python 内置脚本
│   ├── device_proxy.py            # MITM 代理（TRAEDATA_DIR / --gen-ca）
│   ├── auto_checkin.py            # 批量签到（--json-stream / --accounts / --scope）
│   ├── requirements.txt           # cryptography（MITM 证书生成）
│   └── tests/
│       └── test_auto_checkin.py   # 纯函数单测
├── src-ps/
│   └── trae-switch-bridge.ps1     # 非交互切换桥（#Requires RunAsAdministrator + -Json NDJSON）
└── .gitignore
```

---

## 数据目录（Windows）

```
%APPDATA%\TraeWorkAssistant\
├── checkin_accounts.json     # 账号 + JWT（含 UserID）
├── device_map.json           # 每个账号独立的伪设备 ID（账号隔离的核心）
├── groups.json               # 分组定义 + 账号↔分组 membership
├── app_settings.json         # 主题/端口/跳过策略…
├── credits_history.json      # 每次签到积分落盘（按账号聚合给前端）
├── checkin_summary.json      # 最近一次签到结果（用于「今日已签」徽章）
├── logs/
│   ├── proxy.log
│   ├── checkin.log
│   └── switcher.log
└── profiles/<user_id>/       # 切换登录态时备份/恢复的 TRAE Profile 快照
```

---

## 开发

### 前置

- Windows 10/11
- Node.js >= 18（建议 22）
- Rust >= 1.75（stable）
- Python >= 3.9（开发期可用系统 Python；正式打包会嵌入精简 Python）
- WebView2 Runtime（Win11 自带，Win10 需手动安装）
- Visual Studio Build Tools（C++ 桌面开发工作负载，用于 Rust 编译）

### 启动开发模式

```powershell
cd trae-work-helper
npm install
npm run tauri dev
```

首次启动会：
1. Vite 5173 -> Tauri WebView 加载；
2. Rust 主进程创建 `%APPDATA%\TraeWorkAssistant`；
3. 注册表 / 文件路径探测 Trae.exe；
4. 自动加载 `app_settings.json`（首次为空，使用默认值）。

### 打包

```powershell
npm run tauri build
```

产物：
- `src-tauri/target/release/bundle/msi/*.msi`
- `src-tauri/target/release/bundle/nsis/*.exe`

`src-python/` 与 `src-ps/` 会通过 `tauri.conf.json` 的 `bundle.resources` 一并打包到安装目录的 `python/` 与 `ps/`。

---

## 运行流程（典型用户故事）

1. **安装并启动 Trae Work**，登录一次任意账号（产生首次登录态）。
2. **打开 Trae Work 助手** -> 顶栏提示「未安装 CA 证书」-> 点击「一键安装证书」（UAC）。
3. **启动代理**（顶栏按钮）-> 日志页看到 `[代理] listening 127.0.0.1:8899`。
4. 在 Trae Work 中**切换不同账号**，每次都会触发代理捕获 JWT，写回 `checkin_accounts.json`。
5. **账号管理**页确认账号列表、调整分组；过期 JWT 用 `jwt_parse` 自动判别（红/黄/绿）。
6. **一键签到**页选择范围与跳过规则 -> 实时进度（success/already/fail）-> 完成 toast。
7. **积分看板**查看排行 -> 一键复制邀请链接 -> 邀请新用户注册得 5000 积分。
8. **切换账号**：在「账号管理」行点击登录图标 -> PowerShell 桥执行「关闭 TRAE -> 备份 last -> 恢复快照 -> 重置 MachineGuid -> 带代理启动 TRAE」。
9. **设置**页可注册 Windows 计划任务，在每天固定时间后台跑 Python 签到（无需打开应用）。

---

## 单测

```powershell
python src-python/tests/test_auto_checkin.py
```

覆盖：JWT 解析、过期判定、`rand_digits` 同 seed 同输出（稳定设备 ID）。Rust 端编译与端到端测试请在 Windows 上 `cargo test` 与手动 `npm run tauri dev` 验证。

---

## 安全与合规

- **零外发**：所有账号、JWT、登录态、积分仅存在 `%APPDATA%\TraeWorkAssistant`，应用不连接任何自有服务器。
- **依赖最小**：前端仅 `@tauri-apps/* + clsx + lucide-react + recharts + zustand`；Python 仅 `cryptography`（证书生成）。
- **UAC**：仅在安装 CA 证书与切换登录态时申请管理员权限（最小化提权面）。
- **设备 ID 注入**：代理只在本地回环（`127.0.0.1:8899`），不监听外部接口；通过自签 CA 与「受信任根证书颁发机构」让 TRAE 信任本地拦截。
- **下期预研**：兼容 OpenAI/Anthropic 协议的本地 API 网关（按账号积分智能路由）——本期**不实现、不集成** traework2api，仅作需求参考。

---

## 已知约束

- 不支持 macOS / Linux（Tauri 2 可编译但本项目仅在 Windows 上验证代理证书与 MachineGuid 流程）。
- 切换登录态依赖 PowerShell 5.1+（Win10/11 自带）。
- 邀请链接固化在 Rust `INVITE_LINK` 常量：`https://www.trae.cn/work-fission/4CP3KDBT5W9A?utm_source=copy_link&utm_medium=friends_invite`（前端展示与复制均取自该常量，保证一致）。
- 积分看板当前展示「积分总额 / 账号数 / 平均积分 / 账号排行」，需求中的「今日新增 / 近 7 日趋势」尚未实现（规划中）。

---

## License

MIT