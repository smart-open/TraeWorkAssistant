# 豆包 + Trae CN 账号切换 / 会话续期 / 余额展示 实现方案

> **文档版本**: 2026-09-06 · 调研分支 `feat/traecode_doubao`
> **调研方式**: 本机实测侦察（Win11 实机文件系统 / 注册表 / 数据库）+ 项目已有 Trae Work 抓包资产（`api-credit-analysis.md`）+ 开源社区资料交叉验证
> **结论先行**: 两个目标均可行。豆包为 Chromium 壳 + v10(DPAPI) 加密 Cookie，可用"整目录快照 + 无感注入"方案；Trae CN 与本项目已实现的 Trae Work 同构，账号切换/余额展示可直接复用现有快照桥与积分 API，属**低成本移植**。

---

## 0. 结论速览（TL;DR）

| 能力 | 豆包桌面版 | Trae CN 1.107.1 |
|---|---|---|
| 账号切换 | ✅ User Data 快照/恢复（cookie 级或目录级） | ✅ 复用本项目 `trae-switch-bridge.ps1` 快照桥（改路径即可） |
| 会话保存续期 | ✅ v10(DPAPI) 可解密，`sid_guard` 30 天滑动续期可编程 | ✅ storage.json 快照 + 现有 JWT refresh 机制 |
| 余额展示 | ⚠️ 二期：MITM 抓包会员额度接口（基础设施已有） | ✅ 直接复用 `ide_user_pay_status` / `get_session_usage`（product 208/209） |
| 设备隔离 | ✅ Chromium 无 device_id 强绑定，风险低 | ✅ 已实现 6 层重置（machineId/aha 等） |
| 实现成本 | 中（新写 doubao 模块） | **低**（改路径 + 版本兼容） |

---

## 1. 本机实测侦察结论（证据基础）

### 1.1 豆包桌面版（实测版本内核 Chromium 147.0.7727.149）

```
C:\Users\<user>\AppData\Local\Doubao\
├── Application\                      # 程序体
│   ├── Doubao.exe                    # 主程序
│   ├── Doubao.ini / DoubaoWork.ini   # 启动配置（内容 "app"，可忽略）
│   └── app\                          # Electron/Chromium 资源（147 内核）
└── User Data\                        # Chromium 标准用户数据（关键）
    ├── Local State                   # 全局状态：含 saman 账号信息缓存
    ├── Default\
    │   ├── Network\Cookies           # SQLite，登录态核心
    │   ├── Local Storage\leveldb\    # web 侧 KV
    │   ├── Session Storage\
    │   └── DoubaoStorage\Aida\       # 豆包自有业务存储
    ├── aha\  ahanet\                 # 字节统一客户端基建（与 Trae 同源 aha_kit）
    └── saman_app_state / saman_shell_db_storage\   # 字节 saman 账号体系
C:\Users\<user>\AppData\Roaming\Doubao\public_config.json   # 含 user_id 明文
```

**登录态实测**（`Default/Network/Cookies`，SQLite）：
- 字节 passport 标准登录 cookie：`sessionid` / `sessionid_ss` / `sid_tt` / `uid_tt` / `sid_guard`（30 天滑动续期载体）/ `passport_csrf_token` / `msToken` / `ttwid` / `odin_tt` / `multi_sids`，域 `.doubao.com`
- **加密前缀实测为 `v10`（0x763130）**——不是 v20/app-bound，即：**AES-256-GCM + DPAPI（当前 Windows 用户上下文即可解密）**，无需攻击 app-bound 加密。开源工具链（`browser_cookie3` 等）可直接读。
- 附加发现：`bd_sso_hi3jfd` cookie（字节 SSO 多账号切换载体）、`LARK_SUITE_ACCESS_TOKEN`（飞书账号打通）、`Local State → saman` 节点含 `account_isolation_config`（客户端自带多账号隔离开关）。

### 1.2 Trae CN（实测版本 1.107.1，VS Code fork）

```
C:\Users\<user>\AppData\Local\Programs\Trae CN\Trae CN.exe     # 程序
C:\Users\<user>\AppData\Roaming\Trae CN\
├── User\globalStorage\storage.json    # 登录态核心：iCubeAuthInfo://* 键（aha_kit 加密 blob）
│                                      #   + telemetry.machineId / devDeviceId（设备指纹）
├── aha\  ahanet\                      # 与本项目 TRAE SOLO CN 完全同构
└── User\globalStorage\storage.json.vscdb 等其余 VS Code 存储
```

- `iCubeAuthInfo://icube-dc:<uid>` 等键为 aha_kit 加密（`dGMFEAAA` 前缀），**本项目不做解密**，沿用"整文件快照/恢复"策略（与 Trae Work 现行方案一致，已验证稳定）。
- 设备标识键与 Trae Work 完全同名（`telemetry.machineId` 等），现有 6 层重置逻辑直接适用。
- **Trae CN 与 Trae Work（SOLO CN）共用同一账号体系与积分体系**（IDE 积分 208 / Work 积分 209，见项目 `docs/api-credit-analysis.md`），余额 API 无需重新逆向。

### 1.3 安装位置自动识别（两应用通用，三级探测）

1. **注册表卸载键**（最可靠，官方安装器都会写）：
   - `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\*\DisplayName` LIKE `%豆包%`/`%Doubao%`/`%Trae%` → 读 `InstallLocation` / `DisplayIcon`（图标路径即 exe 路径）。
2. **默认路径探测**（兜底 + 新装未刷新注册表场景）：
   - 豆包：`%LOCALAPPDATA%\Doubao\Application\Doubao.exe`
   - Trae CN：`%LOCALAPPDATA%\Programs\Trae CN\Trae CN.exe`（另有 `Trae\` 国际版、`Trae CN` 同目录 `Trae.exe`）
3. **运行进程反查**：`Get-Process` 取 `Doubao.exe` / `Trae CN.exe` 的 `Path`（应用正在运行时最准）。
   - 实现建议：Rust 侧新增 `app_locate(appKind)` 命令，按 1→2→3 顺序返回 `{exe, userDataDir, version}`，前端在设置页展示并允许手动改。

---

## 2. 豆包：账号切换实现方案

### 2.1 方案 A（推荐）：User Data 目录级快照（对齐 Trae Work 成熟模式）

**原理**：豆包全部登录态都在 `User Data\` 下，且加密密钥（DPAPI）与机器+用户绑定但**不与目录路径绑定**——同机同用户下整目录快照/恢复即完成账号切换。

**快照白名单**（精准备份，非全量镜像，控制体积）：
| 项 | 路径（相对 User Data） | 作用 |
|---|---|---|
| 必选 | `Default/Network/Cookies*` | 登录 cookie（sessionid 等） |
| 必选 | `Local State` | saman 账号缓存 + 加密密钥句柄元数据 |
| 必选 | `Default/Local Storage/leveldb/` | web 侧登录/偏好 KV |
| 建议 | `Default/Session Storage/` | 会话级状态 |
| 建议 | `Default/DoubaoStorage/` | 豆包业务数据（对话偏好等） |
| 建议 | `saman_app_state`、`saman_shell_db_storage/` | saman 账号体系状态 |
| 可选 | `Default/IndexedDB/` | 体积大，默认排除 |

**流程**（与 `trae-switch-bridge.ps1` Switch 流程同构）：
1. 检测 `Doubao.exe` 运行中 → 优雅关闭（`CloseMainWindow` → 超时 `taskkill /IM Doubao.exe`）。
2. 保存当前：白名单复制到 `data/doubao_profiles/<uid>/`（uid 从 `public_config.json` 或 cookie 解密后的 passport 接口取）。
3. 恢复目标：从快照目录回写（文件级覆盖，`.bak` 回滚保护）。
4. 重启 `Doubao.exe`，输出 NDJSON 步骤事件（复用现有 `switch-progress`/`switch-done` 事件管线）。

**风险与对策**：
- 版本升级导致 leveldb 结构变化 → 快照带 `schemaVersion` + 恢复前校验 `Last Version` 文件。
- 豆包强推"设备锁/异地提醒" → 同机同用户操作，无新设备特征，风险低。

### 2.2 方案 B（进阶，二期）：Cookie 级热切换（不重启）

利用 v10/DPAPI 可解密特性：
1. 读取 `Network/Cookies`（复制后 sqlite 读 `encrypted_value`）→ DPAPI 解密 → 拿到明文 sessionid。
2. 多账号池以 `sessionid` 为 key 管理（与本项目 Trae JWT 池同构）。
3. 切换 = 进程内重写 Cookies 表对应行（先 DPAPI 重加密写回）+ 通过豆包的 devtools/重启触发生效。
- **验证过的事实基础**：开源项目 `2025doubao-free-api`、`doubao2API`、`doubao-2api` 均以 `sessionid` 作为唯一凭证直调豆包 web 接口（含 `a_bogus` 风控签名、`msToken` 刷新等方案），证明 cookie 级凭证完全可用。
- 但**桌面客户端内热生效**需要额外验证（客户端可能持有内存态会话），因此列为方案 A 之后的增强项。

### 2.3 豆包：会话保存与续期

- **保存**：即快照方案本身（cookie + storage 快照即"会话保存"）；另将解密后的 `sessionid`/`sid_guard`/过期时间入 `doubao_accounts.json`（对齐 `checkin_accounts.json` 结构），支持"到期前提醒"。
- **续期**：字节 passport 为**滑动续期**——`sid_guard` 记录 30 天窗口，每次携带有效会话访问 `www.doubao.com` 都会滚动刷新并下发新 cookie。工具续期策略：
  1. 定时任务（复用现有 `schtasks` 注册机制）每日用账号池中各账号的 cookie 调一个**轻量已登录接口**（如用户信息接口；具体端点在二期 MITM 抓包中固化）。
  2. 抓取响应 `Set-Cookie` 更新本地 `doubao_accounts.json`（v10 重加密写回或仅存明文池）。
  3. 若接口返回未登录（401/重定向到 passport），标记账号过期 + 桌面通知（复用 `tauri-plugin-notification`）。
- **手动续期兜底**：豆包客户端本身支持多账号管理（bd_sso 机制），切回账号登录一次即可刷新。

### 2.4 豆包：余额展示（二期，MITM 抓包路径）

- 事实：豆包免费基础对话无限次，"额度"集中在**豆包专业版订阅额度**（按次数/时长计量、按月刷新，见官方付费服务协议）与生图/视频日额度。客户端查询路径：`设置 → 订阅豆包专业版`；网页路径：登录态下会员中心页。
- 该额度接口为 `www.doubao.com` 域下已登录 XHR（社区无公开文档），**本项目已有完整 MITM 基础设施**（`device_proxy.py`，Trae 抓包即由它完成），实施步骤：
  1. 开启代理接管豆包客户端流量（需为 `User Data` 追加代理配置/或系统代理模式，豆包走 Chromium 网络栈，可用 `--proxy-server` 启动参数注入）。
  2. 打开会员/订阅页面，抓取额度 XHR（建议关键词过滤 `membership|entitlement|quota|remaining|benefit`）。
  3. 固化端点 + 字段后，实现 `credits` 页的"豆包"Tab：会员等级 / 到期时间 / 各能力剩余额度条。
- **降级方案**：若接口带强风控签名无法直调，用"网页视图内嵌 + 数据注入"或仅展示抓包缓存值并标注更新时间。

---

## 3. Trae CN：实现方案（低成本移植）

### 3.1 账号切换

- **现状**：本项目已对 `TRAE SOLO CN` 完成账号切换（精准备份 9 类文件 + PS 桥 + NDJSON 事件）。Trae CN 数据结构与之同构，差异仅在根目录（`%APPDATA%\Trae CN` vs `%APPDATA%\TRAE SOLO CN`）与快照文件集合。
- **改造点**（预计 1~2 天）：
  1. `trae-switch-bridge.ps1` 参数化"目标应用"：`-AppKind Work|Ide`，内部切换入口目录 `$env:APPDATA\Trae CN` 与对应 profiles 根。
  2. 快照文件清单增量：`storage.json`（含 iCubeAuthInfo）、`machineid`、`aha/`、`storage.json.vscdb`、`Network/`（VS Code 侧登录缓存）——以实测"登录态受影响文件"为准，沿用现有 9 类 + Trae CN 特有项。
  3. 数据目录布局：`data/trae_ide_profiles/<user_id>/`（与现有 `profiles/` 平行，避免互相污染）。
  4. Tauri 命令 `switch_account(userId, appKind)` 扩展；前端 Accounts 页加"Work / IDE"双列或 Tab。

### 3.2 会话保存续期

- `iCubeAuthInfo` blob 加密但**快照恢复即生效**（aha_kit 本地解密，与设备绑定在 machineId 而非文件本身），保存 = 快照。
- 续期：Trae 客户端内置 refresh（本项目 Trae Work 侧 `refresh_jwt` 已验证同账号体系 JWT 可用 refresh_token 续期）；对无 refresh_token 的账号沿用现有"到期检测 + 提醒重新 OAuth"策略。

### 3.3 余额展示（直接复用，零逆向）

| 项 | 现成资产 |
|---|---|
| IDE 积分（208） | `ide_user_pay_status` 接口 + 现有 Credits 页三线趋势图 |
| Work 积分（209） | `get_session_usage` + `work_credit` 包（`feat/work-credit-pool` 分支） |
| 账号归属 | `Accounts` 页 AccountView 扩展 `ideCredits` / `workCredits` 双字段 |

**关键结论**：Trae CN 的余额接口与 Trae Work 完全同源（同域名 `api5-normal.mchost.guru`、同鉴权头），现有查询代码改个入参账号即可同时服务两个客户端的登录态。

---

## 4. 实施计划（建议顺序）

| 阶段 | 内容 | 预估 |
|---|---|---|
| P0 | 安装位置自动识别 `app_locate` + Trae CN 切换移植（PS 桥参数化） | 2~3 天 |
| P1 | Trae CN 余额展示接入 Credits 页（复用 API） | 1 天 |
| P2 | 豆包目录级快照切换 + doubao_accounts.json 账号池 | 3~4 天 |
| P3 | 豆包 cookie 续期定时任务 + 到期提醒 | 1~2 天 |
| P4 | 豆包 MITM 抓包固化额度接口 → 余额展示 Tab | 2~3 天（含抓包验证） |
| P5 | 豆包 cookie 级热切换（方案 B） | 视 P4 成果，1~2 天 |

## 5. 风险与合规

- 豆包/Trae 均为第三方账号体系，本工具定位与项目一致：**仅管理本人合法持有的账号**，不破解、不绕过付费；会员额度接口仅做"展示"不做"代刷"。
- `sessionid`/`accessToken` 等凭证等同密码：入库文件列入 `.gitignore`（沿用 `work_accounts.json` 模式），UI 全程掩码展示。
- 字节/腾讯风控更新可能使方案 B/Cookie 续期失效——目录级快照（方案 A）是最抗版本变化的底座，**先落地 A 再谈 B**。
