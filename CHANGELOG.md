# 更新日志

本文件记录 Trae Work Assistant 的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

---

## [2.5.0] - 2026-09-07

积分双轨改造（通用积分 / Work 积分区分）、多主题换肤与本机扫描修复，次要版本升级（2.4.6 → 2.5.0）。

### 新增

- **积分双轨体系**：全链路区分通用积分（product_id 208，本服务消耗）与 Work 积分（product_id 209，套餐专用）：
  - 后端积分解析双轨化（`CreditStats` 拆分 total/general/work + 最早过期 + 套餐信息），`AccountView` 新增 `general_credits` / `work_credits` / `pay_identity` / `membership_expire` / `membership_next_billing` 字段
  - 新增 `pay_status.rs`：套餐身份查询与缓存；新增 `fetch_credit_detail` 积分明细命令（各积分包剩余与到期）
  - 概览页「可用总积分」悬停拆分通用/Work，新增「本机套餐」卡片（storage.json 明文，零 API）
  - 账号管理：套餐徽标（Free/付费，悬停显示到期与续费日）、积分单元格悬停明细卡、「积分过期」列改为「X 天后过期」
  - 积分看板：总积分卡拆分提示 + 明细行小字拆分；API 服务：账号池积分显示统一为「通用积分」
- **多主题换肤**（参考 `feat/traecode_doubao` 分支）：新增 6 主题（石墨灰/炭黑/暗夜紫/墨绿/琥珀暖夜/科技蓝），tailwind slate/zinc 关键档位变量化 + `data-theme` CSS 覆盖；设置页支持「跟随系统」，左下角按钮仅轮询 6 主题
- **账号导入**：新增 `accounts_import` 命令与前端「导入账号」按钮（自动去重，报告新增/跳过/新增分组数）

### 修复

- **扫描本机账号**：TRAE SOLO CN 的 storage.json 无 `icube_gtm` 键导致扫描恒为空。重构证据链：① 会话日志 `&uid=` 参数（当前登录，置信可入池）② `state.vscdb` 键名痕迹字节扫描（历史候选，多账号不置信）③ `icube_gtm.users`（IDE 变体兼容保留）；并澄清 `iCubeAuthInfo://icube-dc:<did>` 为设备 id，不可入池

### 变更

- **系统设置重排**：左列 = 通用配置（主题/语言/通知/托盘 + Trae Work 安装路径 + 日志保留天数）/ 设备标识重置；右列 = 签到行为 / 每日定时签到 / 代理配置；移除「关于」区块（已在左下角软件说明弹框）
- **API 服务**：积分体系说明改为紧凑单行（product_id 208 / 计入规则 / 上游接口）+ 右侧实时「当前全部账号通用积分总余额」；移除「使用方式 & 配置示例」右侧徽标
- 「关于」弹框：标题简化为「关于」，概述明确面向 Trae Work（TRAE SOLO CN）

### 变更文件

| 文件 | 说明 |
|---|---|
| `src-tauri/src/models.rs` / `commands/accounts.rs` | 积分双轨解析、`accounts_import`、`fetch_credit_detail` |
| `src-tauri/src/commands/pay_status.rs` / `trae_local.rs` | 套餐身份查询缓存；本机扫描证据链重构 |
| `src/types.ts` / `src/lib/tauri.ts` / `src/store.ts` | 前端类型/绑定/状态扩展 |
| `src/pages/Dashboard.tsx` / `Accounts.tsx` / `Credits.tsx` / `ApiService.tsx` / `Settings.tsx` | 各页面积分双轨展示与布局调整 |
| `src/lib/themes.ts`（新增）/ `tailwind.config.js` / `src/index.css` / `App.tsx` / `Sidebar.tsx` | 6 主题换肤体系 |
| `package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` / `about.ts` | 版本号 2.4.6 → 2.5.0 同步 |
| `AGENT.md` / `docs/*.md` | 文档版本标注同步 |

---

## [2.4.6] - 2026-09-07

界面优化与版本号升级（2.4.4 → 2.4.6，一次跳过 2 个小版本号）。

### 变更

- **侧栏底部改版**：移除「邀请得 5000 积分」横幅（及对应 `openInvite` 前端调用），替换为 4 个居中排列的图标按钮：
  - **软件 Github 地址**：打开项目仓库 `smart-open/TraeWorkAssistant`
  - **作者博客主页**：打开 `blog.sopenai.cn`
  - **切换主题**：跟随系统 → 浅色 → 深色 三态轮询并持久化，带 toast 反馈
  - **软件说明**：新增「关于」对话框（品牌与版本、概述、GitHub/博客/仓库链接、赞赏码、作者与 MIT 版权、免责声明）
- 新增 `src/lib/about.ts` 集中管理应用品牌信息（名称/版本/概述/作者/版权/链接），改名或升版只改此处。
- 新增 `src/components/AboutDialog.tsx` 与赞赏码资产 `src/assets/donate-qr.base64.ts`（base64 内嵌，不依赖运行中的静态服务器）。
- 后端 `invite_link` 命令保留，仅前端移除调用。

### 变更文件

| 文件 | 说明 |
|---|---|
| `src/components/Sidebar.tsx` | 移除邀请横幅，底部改为 4 个图标按钮（居中），内联主题三态轮询 |
| `src/lib/about.ts` | 新增：应用品牌与关于信息集中管理 |
| `src/components/AboutDialog.tsx` | 新增：软件说明对话框 |
| `src/assets/donate-qr.base64.ts` | 新增：赞赏码 base64 资产 |
| `package.json` / `tauri.conf.json` / `Cargo.toml` / `Cargo.lock` | 版本号 2.4.4 → 2.4.6 同步 |
| `src-tauri/src/commands/accounts.rs` | 应用内版本标注同步 |
| `AGENT.md` / `docs/*.md` / `scripts/make_portable_zip.py` | 文档版本标注与便携包文件名同步 |

### 说明

- 功能实现参考 `feat/traecode_doubao` 分支（v2.6.0 品牌迁移版）的侧栏工具栏与关于对话框，并按本工程实际调整：主题切换沿用本工程既有的 `system/light/dark` 三态机制（未引入 6 主题 data-theme 体系），品牌信息以 Trae Work Assistant v2.4.6 为准。
- 后续版本号由 `src/lib/about.ts` 的 `APP_VERSION` 驱动「关于」弹窗显示。

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
