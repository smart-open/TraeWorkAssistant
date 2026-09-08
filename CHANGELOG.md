# 更新日志

本文件记录 AI Work 助手（ai-work-assistant，原 Trae Work Assistant）的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

---

## [未发布]

### 新增

- **单实例防护**：重复启动应用（双击/自启动后再点）时，不再产生第二个进程——已有实例的主窗口自动还原、显示并聚焦，新进程自动退出。基于官方 `tauri-plugin-single-instance` 实现，仅正式版启用（dev 模式与已安装版共用 identifier，启用会互相顶替干扰调试）。

### 修复

- **检查更新「下载安装包」国内网络直连超时（os error 10060）**：`update_check` 请求的 `api.github.com` 可直连，但下载 302 跳转到 `objects.githubusercontent.com`（GitHub 下载 CDN）直连不通，而下载此前只认环境变量代理、不认系统代理（VPN）。改为逐通道尝试：**系统代理（注册表，即用户 VPN）→ 环境变量代理 → 直连**，任一通道成功即完成；检查更新同样受益。另修复下载误设 30s 整体超时（慢网络下必被掐断），改为连接 10s + 读 60s、不设整体超时。

---

## [3.2.3] - 2026-09-08

### 修复

- **签到必崩 `Failed to import encodings module`（v3.2.0 – v3.2.2 全部受影响）**：v3.2.0 起的安装包/便携包把 `src-python/` 内混入的一套残缺 Python 3.13 运行时（缺 `Lib/encodings`）打进了 `resources/python/`，而应用优先使用内嵌解释器 → 升级后签到必崩。两步修复：移除残缺运行时；`state.rs` 内嵌与系统解释器探测全部改用 `import encodings` 自举验证（原 `--version` 探测不触发 stdlib 导入，残缺运行时也能通过），残缺内嵌解释器自动回退系统 Python——已安装 3.2.x 的机器升级本版后立即恢复，无需手动清理残留文件。

---

## [3.2.2] - 2026-09-07

### 修复

- **检查更新下载步骤必报错（3.1.0 / 3.2.x 全部受影响）**：前端 invoke `update_download` 时参数 key 传了 `version`，而 Tauri 2 顶层参数按驼峰匹配 Rust 参数 `expected_version` → `expectedVersion`，导致「下载更新包」必报 `missing required key expectedVersion`（v3.1.0 的 `update_install` 同一问题，应用内更新从未成功）。修正为 `expectedVersion`。

---

## [3.2.1] - 2026-09-07

### 修复

- **检查更新只匹配本产品线（≥ 3.0.0）**：同一仓库同时发布 2.x（Trae Work 助手）与 3.x（AI Work 助手）两条产品线、是两个不同产品，`releases/latest` 会指向最近发布的那条线。改为拉取 releases 列表（跳过 draft / prerelease），只认 ≥ 3.0.0 的 release 并取版本最高者；`release_page` 改指具体 release 页；发布页链接改为 `…/releases`（不再用 `/latest`）。

---

## [3.2.0] - 2026-09-07

### 变更

- **应用内更新改为两步确认制**：检查更新发现新版本后不再自动下载安装，拆分为「下载」与「安装」两个独立步骤，UI 各有一次确认——确认一下载更新包（带进度条），确认二「立即安装并重启」。
- **安装器启动参数 `/S` → `/P /UPDATE /R`**：`/P` 被动模式（安装进度条可见）、`/UPDATE` 跳过卸载直接覆盖安装、`/R` 安装完成后自动重启应用（自定义 NSIS 模板已支持）。`update_run_installer` 增加路径校验：仅允许运行临时更新目录内的安装包且版本须大于当前版本。
- **NSIS 安装包升级体验优化**：升级安装（检测到旧版本）时不再弹出「卸载后安装 / 不卸载直接安装」选择页（原默认推荐先卸载），改为**跳过该页直接覆盖安装**；同版本重装与降级仍显示选择页。实现方式：新增自定义 NSIS 模板 `build-assets/installer.nsi`（基于 tauri v2.11.4 上游模板定制），经 `tauri.conf.json` 的 `bundle.windows.nsis.template` 启用。
- **版本号收敛为单源**：单一来源 = `src-tauri/Cargo.toml`。
  - `tauri.conf.json` 移除 `version` 字段（Tauri 自动回读 Cargo.toml）；
  - 关于页版本号改为运行时 `getVersion()` 读取，移除 `about.ts` 中的 `APP_VERSION` 硬编码；
  - 新增 `scripts/sync_version.py`：一条命令把版本同步到 package.json / AGENT.md 标题 / Cargo.lock；
  - `rename_release.py` / `package_portable.py` 版本读取改为 Cargo.toml 回退。

### 新增

- **API 服务新增 Anthropic 兼容端点 `POST /v1/messages`**（F-39「+Anthropic 适配」落地）：
  - 请求侧 `payload::anthropic_to_openai` 将 Anthropic Messages 请求（system / text blocks / tool_use / tool_result / tools / tool_choice）转换为 OpenAI 内部格式，复用既有账号池调度与 llm_utils_chat 链路；
  - 响应侧 `sse::stream_convert_anthropic` / `aggregate_anthropic` 输出 Anthropic 协议（流式 message_start → content_block_start/delta/stop → message_delta → message_stop 事件序列，支持 tool_use 块；非流式 message 对象含 usage）；
  - 鉴权支持 `x-api-key`（Anthropic 风格）与 `Authorization: Bearer`（OpenAI 风格）双风格；
  - 单元测试覆盖 text 与 tool 往返转换（`cargo test` 2 项通过）。
- API 服务页「使用方式 & 配置示例」补充 Anthropic 端点说明与 /v1/messages cURL 测试示例。

### 文档

- `docs/future-roadmap.md`：F-39「Trae API 暴露」标记完成并从待办排序移除（核心能力随 v3.1.0 网关 + F-08 双应用发现天然达成，本次补齐 Anthropic 适配）。
- `AGENT.md`：API 网关模块结构与端点契约同步（/v1/messages、双风格鉴权、账号池 app 无关说明）。

### 排查

- `src-ps/trae-switch-bridge.ps1` 编码排查：文件头已含 UTF-8 BOM（EF BB BF），不存在 PowerShell 5.1 按 GBK 误读问题，无需调整。

---

## [3.1.0] - 2026-09-07

API 服务页界面微调。

### 变更

- **API 服务页**：删除「使用方式 & 配置示例」标题右侧的「通用积分」徽章文字。
- **API 服务页**：积分体系说明面板中「当前全部账号通用积分总余额」改为靠右展示（`ml-auto`，空间不足自动换行并保持右对齐），去掉中间「·」分隔符。
- 版本号 3.0.0 → **3.1.0**（`package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` / `about.ts` 同步）。

---

## [3.0.0] - 2026-09-07

品牌定位迁移：产品更名为 **AI Work 助手（ai-work-assistant）**，面向多个 work 工具提供功能支持；同时清理全部旧品牌痕迹并保证老应用升级兼容。

### 变更

- **品牌统一**：代码注释、界面文案、README、AGENT.md、docs 全部文档由 Trae Work Assistant / trae-work-assistant 统一为 AI Work 助手 / ai-work-assistant。
- **打包标识**：identifier `com.traework.assistant` → `com.aiwork.assistant`，`mainBinaryName` → `ai-work-assistant`（主程序 ai-work-assistant.exe），Cargo 包名与 package.json 同步；新增 `scripts/rename_release.py` 将安装包统一输出到 `release/`，产物使用中文产品名命名（如 `AI Work 助手_3.0.0_x64-setup.exe` / `AI Work 助手_3.0.0_x64_zh-CN.msi` / `AI Work 助手_3.0.0_x64_portable.zip`）。
- **老应用升级兼容（NSIS）**：新增 `build-assets/installer-hooks.nsh`，安装时自动结束旧进程、静默卸载旧品牌「Trae Work 助手」并清理残留目录 / 卸载键 / 快捷方式 / 旧命名主程序。判定依据为安装时产品名而非版本号：已发布的 v2.4.4 及更早安装包均为旧品牌，同样被自动清理；仅「AI Work 助手」品牌（v3.0.0 起）走 NSIS 原生原地升级。
- **老应用数据自动迁移（启动时，复制语义）**：`state.rs::migrate_legacy_dirs()` 将 `%APPDATA%\TraeWorkAssistant` **递归复制**为 `%APPDATA%\AIWorkAssistant`，并复制 WebView2 界面偏好目录（identifier 变更所致）；**旧目录原地保留，老应用可继续使用，新旧两版可并存**；新目录已有数据则自动跳过（不重复迁移）；失败不影响启动。
- **计划任务并存迁移**：`misc.rs::try_migrate_legacy_task()` 检测到旧任务时按其原触发时间重建 `AIWorkAssistant_DailyCheckin`，**旧任务保留**供老应用继续使用；「取消注册」只删除新任务名。
- **环境变量**：`TRAEDATA_DIR` → `AIWORKDATA_DIR`（Python 脚本与 PowerShell 桥接脚本兼容读取旧变量名）。
- **版本线划分**：新版本自 3.0.0 起维护，**之前所有 2.x 版本升级到 3.x 均需数据迁移**（安装 / 首次启动自动完成）；原「Trae Work 助手」产品通过 `trae_work_main` 分支维护（仅 Trae Work 单应用，2.x.x，仅必要修复）。
- **界面**：账号管理页「使用帮助」按钮改为与页头描述文字水平对齐，并以圆形色块徽章突出展示（PageHeader 的 leftExtra 移入描述行内，与描述行垂直居中）。
- **应用内检查更新**：「关于」页版本号旁新增「检查更新」按钮——分析 GitHub Releases 最新发布，发现比当前更大的版本时自动下载 NSIS 安装包（实时进度条）并静默安装（/S，走安装钩子自动清理旧版），随后应用自动退出完成升级；网络异常时提示并附发布页直链。
- 版本号 2.5.0 → **3.0.0**（品牌迁移后的新版本起点；`package.json` / `tauri.conf.json` / `Cargo.toml` / `about.ts` 同步）。

### 说明

- **老 MSI 安装包无法原地升级**：MSI UpgradeCode 随 identifier 变化，老版本 MSI 用户请先卸载后安装新版，或改用 NSIS 安装包（-setup.exe）升级（推荐，自动迁移）。
- 旧数据目录迁移采用「复制」：迁移后旧目录原地保留（老应用可继续使用，两版并存）；迁移只在首次启动执行一次，之后新目录已有数据即跳过。

### 审查修正（发布前全量审查）

- 升级兼容判定依据修正为「安装时产品名（NSIS 卸载键）」而非版本号，并经本机 2.4.4 注册表实证（卸载键/安装目录均为「Trae Work 助手」）；README / AGENT.md / 本条目同步。
- 安装钩子 PREINSTALL 补充结束过渡版主进程 "AI Work 助手.exe"（防止其运行中锁住旧命名主程序清理）。
- 修正「注册任务」权限不足提示中的手动命令引号错误（`&`→`&&`、数据目录值补闭合引号，与实际 /TR 一致）。
- AGENT.md 三处与 `src-python/tests/test_proxy.py` 的环境变量名同步为 `AIWORKDATA_DIR`。

---

## [2.5.0] - 2026-09-06

功能版本：账号导入 + 导出优化 + 工具栏重排（提交 bddfdf0）。

### 变更

- 账号管理工具栏重排：右侧依次为 刷新数据 / 扫描本机 / OAuth 登录 / 添加账户 / 导出账户 / 导入账户 / 分组管理 / 快照管理，帮助按钮纯图标移至描述后（PageHeader 新增 leftExtra 插槽）。
- 导出账户增强：携带应用版本、dcId、addedAt，兜底导出视图外原始账号；新增「导入账户」按钮与 `accounts_import` 命令（三种格式兼容，uid+JWT 去重，分组按 id 合并）。
- 账号池预留记录数据中心级 id（icube-dc）：`RawAccount.DcID` + 切换 / 保存登录态时自动回填 + 批量回填命令。
- F-08 双应用账号自动发现修复：本机证据推导 Cloud-IDE uid（Trae CN 读 storage.json，SOLO 读 state.vscdb）；套餐到期时间从会员包 expire_time 提取。
- F-47 进程关闭等待缩短为 3s / 2s（轮询 250ms）。
- AGENT.md 新增 §15 版本升级规则（完整功能 = 中位 +1，修复 / 优化 / 微小 = 低位 +1）。

---

## [2.4.4] - 2026-08-16

维护版本：清理临时文档并同步版本号。

### 变更

- 删除临时问题分析报告 `docs/issue-analysis-2026-08-16.md`，其功能已由 `CHANGELOG.md` 与 `AGENT.md` 中的变更说明覆盖，避免重复维护。
- 版本号 2.4.3 → 2.4.4（`package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` 四处同步）。
- 同步更新 `README.md`、`AGENT.md`、`docs/user-manual.md`、`docs/tech-framework.md`、`docs/operation-manual.md` 中的版本标注，以及 `scripts/make_portable_zip.py` 的便携包文件名。

### 说明

- 本版本**无代码逻辑改动**，仅文档与版本号维护；v2.4.3 的代理/VPN 共存与定时任务修复保持有效。
- 若需重新打包安装包，仍须执行 `npm run tauri build`（Python 侧修复已随 v2.4.3 打包）。

---

## [2.4.3] - 2026-08-16

修复「开启本地代理后 GitHub / Google 打不开」与「定时签到注册·查询·取消无反应」两类问题。

### 修复

- **本地代理与 VPN 冲突导致外网无法访问**（`ERR_TUNNEL_CONNECTION_FAILED`）
  - 根因：`proxy_start` 把 Windows 系统代理**整体覆盖**为 `127.0.0.1:8899`，抹掉了 VPN（Clash / v2rayN 等本地 HTTP/SOCKS 代理）的接管点；而 `tunnel_raw()` 对非 Trae 域名使用 `socket.create_connection` **直连**上游，完全绕开 VPN，导致 GitHub / Google 被阻断，而 baidu / qq 等国内站点直连可达故始终正常。
  - 修复：引入**上游代理链式转发**。`proxy_start` 在改写系统代理**之前**先读取已有的系统代理配置，作为 `UPSTREAM_PROXY` 环境变量注入 Python 代理进程；`device_proxy.py` 新增 `_parse_upstream()` / `connect_via_upstream()`，支持 **HTTP CONNECT** 与 **SOCKS5**（含用户名密码认证）两类上游。`tunnel_raw()` 与明文 HTTP 转发路径对**非 Trae 域名**优先经上游（即 VPN）出站，上游不可用时自动回退直连。Trae 域名仍由本代理 MITM 解密以捕获 JWT。
- **CONNECT 隧道缺少握手应答**
  - `tunnel_raw()` 从未向客户端回送 `HTTP/1.1 200 Connection Established`，客户端因此永远不会发起 TLS 握手；上游不可达时也无任何应答，浏览器无限等待。现已补齐 `200` 握手，失败时回 `502 Bad Gateway`。
- **停止代理会破坏 VPN 设置**
  - `proxy_stop` 原先只是把 `ProxyEnable` 置 0。现改为**原样还原**启动前捕获的系统代理（含 `ProxyServer` 与 `ProxyOverride`），停止本地代理后 VPN 立即恢复可用。
- **计划任务查询结果中文乱码**
  - 根因：`schtasks` 的中文输出为 **GBK** 编码，Rust 侧用 `String::from_utf8_lossy` 按 UTF-8 解读，产生 mojibake（如 `ϵͳ�Ҳ���ָ�����ļ���`，实为「系统找不到指定的文件」）；乱码进一步导致「找不到」关键字匹配失效，无法命中「任务未注册」的友好分支。
  - 修复：新增 `run_schtasks()` 统一入口，前置 `chcp 65001` 强制 schtasks 以 UTF-8 输出，中文错误信息可正确解码与匹配。
- **错误提示前缀重复**
  - 原先 Rust 返回 `查询计划任务失败：…`，前端 `Settings.tsx` 又拼接 `查询失败：`，叠加成「查询失败：查询计划任务失败：…」。现 Rust 端只返回纯错误文案，前端前缀成为唯一前缀。
- **定时任务注册在普通用户下失败**
  - 移除 `schtasks /RL HIGHEST`（签到脚本只读写 `%APPDATA%` 并运行 Python，无需提权，强制最高权限会让普通用户卡在 Access Denied）；`/TR` 命令行改为 `cmd /c set "TRAEDATA_DIR=…" && "<python>" "<script>"`，对含空格的路径安全。
- **查询 / 取消操作静默吞错误**
  - `task_status` 原先无论成功失败都返回 `Ok(stdout)`，任务不存在时返回空串，界面显示空白；`task_unregister` 原先丢弃执行结果永远返回 `Ok(())`。现均真实上报结果：任务不存在时返回明确提示「未注册每日签到任务（请先在设置页点击「注册任务」）。」，取消时若任务本就不存在按已删除处理。

### 变更文件

| 文件 | 说明 |
|---|---|
| `src-python/device_proxy.py` | 上游代理链式转发（HTTP CONNECT / SOCKS5）、`tunnel_raw` 补 `200` 握手与 `502` 兜底 |
| `src-tauri/src/commands/proxy.rs` | 启动前捕获系统代理并注入 `UPSTREAM_PROXY`、停止时原样还原、抽出 `apply_proxy` / `get_existing_win_proxy` |
| `src-tauri/src/commands/misc.rs` | 新增 `run_schtasks()`（`chcp 65001`）、去重复前缀、移除 `/RL HIGHEST`、错误可见性增强 |
| `docs/issue-analysis-2026-08-16.md` | 新增问题深度分析报告（调用链、根因、修复、验证方法） |

### 升级注意

`device_proxy.py` 会被打包进安装包的 `resources/python/`，**代理相关修复必须重新执行 `npm run tauri build` 才会进入正式版**；开发模式 `npm run tauri dev` 直接读取 `src-python/`，重启代理即生效。

---

## [2.4.2] - 2026-08-15

### 修复
- 修复发布版黑框 / 闪退 / 排队提醒丢失等 GUI 失灵问题。
- 健康检查端点统一为 `/health`，文档英文化与路径清理。
- 移除 `proxy_logs` 目录引用，日志统一存放在 `logs/` 下。
- 全面修复文档错误；恢复误删的 `src-python/tests/test_auto_checkin.py`。

### 新增
- 便携版打包脚本 `scripts/make_portable_zip.py` / `scripts/package_portable.py`。

---

## [2.4.1] - 2026-08-14

### 变更
- 项目重命名为 `trae-work-assistant`，同步更新文档与用户手册。

### 新增
- 账号切换流程重构、保存登录态能力、帮助说明。

### 修复
- 日志相关问题修复。

---

## [2.4.0]

- API 服务页面重构、日志页面整合与 UI 优化。

## [2.3.0]

- API 服务协议对齐、代理修复、交互优化与日志增强。

## [2.2.0]

- 全面质量优化：Mutex 安全锁（poison 恢复）、竞态修复、暗色模式图表适配、积分三线趋势图。

## [2.0.0]

- 本地 API 网关（axum + ureq）、SSE 协议转换、账号池智能调度、签到错误冷却状态机、6 层设备标识重置。
