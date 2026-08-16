# 问题深度分析报告 · Trae Work Assistant

> 分析日期：2026-08-16
> 分析对象：`trae-work-assistant` v2.4.2（Tauri 2 + React 18 + Rust + Python MITM 代理）
> 修复发布版本：**v2.4.3**（变更清单见 [CHANGELOG.md](../CHANGELOG.md)）
> 结论：两个问题均已定位到**明确的代码根因**，并已在本仓库源码中修复。

---

## 一、问题一：运行代理后 github 等网站无法访问

### 1. 现象
启动本地代理（"本地代理"开关 / API 网关依赖的 MITM 代理）后，浏览器访问 `github.com`、`google.com` 等**非 Trae 相关域名**时全部超时/打不开；关闭代理后恢复正常。

补充诊断（用户反馈）：**开启本地 VPN 梯子 + 关闭 app 代理 → google/github 正常；开启 app 代理（无论 VPN 是否开）→ google/github 失败，但 baidu/qq 始终正常。**

### 2. 调用链梳理
1. `proxy_start`（src-tauri/src/commands/proxy.rs）启动 Python 脚本 `device_proxy.py`，并**把 Windows 系统代理指向 `127.0.0.1:8899`**（`set_win_proxy` 写注册表 `ProxyEnable=1` + `ProxyServer`）。
2. 系统代理的 `ProxyOverride` 仅豁免 `127.0.0.1;localhost;<local>`，**其余所有流量都被迫经过本代理**。
3. `device_proxy.py` 对每个 `CONNECT` 请求分流：
   - 命中 Trae 域名 → MITM 解密（代码在 `conn.sendall(b"HTTP/1.1 200 Connection Established...")` 后才握手，**正确**）。
   - **未命中 → 走 `tunnel_raw()` 透明隧道**（见 device_proxy.py:1196）。

### 3. 根因（两层）

#### 根因 A（对称性 bug，Round 1 已修）：`tunnel_raw` 漏发 200 握手
`tunnel_raw()` 原实现**从未向客户端回送 `HTTP/1.1 200 Connection Established` 应答**，直接开始双向 pipe。根据 HTTP CONNECT 协议，代理必须先回 `200`，客户端才发起 TLS 握手；缺了它，所有非 Trae 域名的 HTTPS 隧道永远建立不起来。

> 对照：MITM 分支正确发送了 `200`，而 `tunnel_raw` 遗漏，属对称性 bug。Round 1 已补 `200 + 502`。

#### 根因 B（与 VPN 冲突，本轮新定位）：代理 outbound 直连、不串 VPN
仅修根因 A 后仍打不开外网，根子在于**代理与 VPN 冲突**：
- 用户的 VPN（梯子，如 Clash/v2rayN）本质是**本地 HTTP/SOCKS 代理**（如 `127.0.0.1:7890`），它通过把自己的地址写进「系统代理」来接管流量。
- app 代理启动时 `set_win_proxy` **把系统代理覆盖为 `127.0.0.1:8899`**，于是 VPN 的接管点被抹掉。
- `tunnel_raw` 对非 Trae 域名的 CONNECT 走 `socket.create_connection((host, port))` —— 这是**直连**，根本不经过 VPN，于是 google/github 被 GFW 阻断 → 浏览器报 `ERR_TUNNEL_CONNECTION_FAILED`。
- baidu/qq 是**国内域名**，直连可达 → 始终正常，正好对应用户的对照现象。

> 一句话：**app 代理劫持了全局流量，却没把外网请求交给 VPN，于是外网断、内网通。**

### 4. 修复（已应用）

- **Round 1**：`tunnel_raw()` 补 `200 Connection Established` 握手 + 上游失败回 `502`。
- **本轮（上游代理链式转发 / upstream proxy chaining）**：
  - `src-tauri/src/commands/proxy.rs` 的 `proxy_start`：在改写系统代理**之前**，先读取已有的系统代理（`get_existing_win_proxy` 读注册表 `ProxyServer`/`ProxyOverride`）。若检测到外部代理（即用户的 VPN，且不是本机 `127.0.0.1:8899`），则把它作为 `UPSTREAM_PROXY` 环境变量注入 Python 进程，并在 `proxy_stop` 时**原样还原**用户原有的系统代理（避免覆盖/丢失 VPN 设置）。
  - `src-python/device_proxy.py`：新增 `connect_via_upstream()`，支持 HTTP 代理 `CONNECT` 与 SOCKS5（无认证/用户名密码）两种上游；`tunnel_raw()` 与 `handle_plain()` 对非 Trae 域名优先经上游转发，上游不可用时再回退直连。
  - 效果：Trae 域名仍由本代理 MITM 解密；google/github 等经 VPN 上游出去 → 外网恢复。

### 5. 验证方法
- **必须重新打包 Python**：`device_proxy.py` 改动需 `npm run tauri build` 才会进入 `resources/python/`；开发模式（`npm run tauri dev`）直接读 `src-python/` 重启代理即生效。
- 验收（用户原场景）：**同时开启 VPN 梯子 + app 代理** → 浏览器访问 github.com / google.com 应正常；baidu/qq 仍正常；Trae 签到 JWT 仍被捕获。
- 抓包确认：代理日志 `logs/proxy.log` 中 google/github 的 CONNECT 走 `[raw-tunnel] 经上游代理 ... 建立隧道`；无 `ERR_TUNNEL_CONNECTION_FAILED`。

---

## 二、问题二：每日签到定时任务注册/查询/取消"没有作用 / 没反应"

### 1. 现象
设置页"每日定时签到"区域的【注册任务】【查询】【取消】三个按钮点击后，要么报错（多为权限），要么看起来"毫无反应"，定时签到实际上从未自动执行。

### 2. 调用链梳理（确认 UI 接线是正确的）
- 前端 `src/pages/Settings.tsx`：`register` → `api.misc.taskRegister(time)`；`query` → `api.misc.taskStatus()`；`unregister` → `api.misc.taskUnregister()`。
- `src/lib/tauri.ts` 中三个 invoke 均已定义，命令名与 Rust 端一致。
- `src-tauri/src/main.rs` 的 `generate_handler!` 中 **`task_register`/`task_status`/`task_unregister` 均已注册**（proxy 能跑说明命令总线正常）。

**因此问题不在前端/接线，而在 Rust 命令本身的实现与权限模型。**

### 3. 根因（分三层）

#### 根因 A（主因）：`/RL HIGHEST` 强制要求管理员权限
`task_register`（src-tauri/src/commands/misc.rs）构造的 `schtasks /Create ... /RL HIGHEST`。`/RL HIGHEST` 需要管理员令牌，**普通用户运行应用时会直接 "Access is denied"（错误码 5）**，任务根本创建不出来 —— 这正是"注册没作用"的最常见真实原因。签到脚本只读写 `%APPDATA%` 并运行 python，**完全不需要提权**，这个参数属于过度设置。

#### 根因 B（次因）：`/TR` 命令对含空格路径不安全
原 `/TR` 为：`cmd /c set TRAEDATA_DIR={data_dir}&"{py}" "{script}"`。
- `data_dir` 形如 `C:\Users\<用户名>\AppData\Roaming\TraeWorkAssistant`。**若 Windows 用户名含空格**（如 `C:\Users\John Doe\...`），未加引号的 `set TRAEDATA_DIR=C:\Users\John Doe\...` 会在第一个空格处截断，导致 `auto_checkin.py` 拿到错误的数据目录、找不到账号 → 任务"注册成功"却**什么都不做（无效果）**。
- 用 `&&` 串联也比裸 `&` 更稳健（仅当 set 成功才执行 python）。

#### 根因 C（诊断干扰）：`task_status` / `task_unregister` 吞掉错误
- `task_status` 原实现：无论 `schtasks` 是否成功，都 `Ok(stdout)` —— 任务不存在时返回**空字符串**，前端 `setTaskInfo('')` 显示为"空白"，用户感觉"查询没反应"。
- `task_unregister` 原实现：`let _ = Command::...status();` 直接丢弃结果，**永远返回 `Ok(())`**，删除失败（如权限不足/任务不存在）也静默成功，用户感觉"取消没反应"。

#### 根因 D（本轮新定位）：GBK 乱码 + 重复前缀
用户实际报的「乱码 + 重复前缀」来自这里：
- **乱码**：`schtasks` 的中文报错（`系统找不到指定的文件`）以 **GBK** 字节输出，而 Rust 侧用 `String::from_utf8_lossy(&stderr)` 按 UTF-8 读取 → 变成 `ϵͳ�Ҳ���ָ�����ļ���` 之类的 mojibake。任务本就不存在时，真实的 stderr 是"系统找不到指定的文件"，但乱码后 `"找不到"` 永远匹配不上 `detail.contains("找不到")`，于是走到 `Err("查询计划任务失败：{乱码}")`。
- **重复前缀**：Rust 返回 `Err("查询计划任务失败：...")`，而前端 `Settings.tsx` 的 `query()` 又套了一层 `查询失败：`，于是显示成 `查询失败：查询计划任务失败：{乱码}`。

### 4. 修复（已应用）
`src-tauri/src/commands/misc.rs`：
- **移除 `/RL HIGHEST`**，使普通用户也能注册（任务以当前用户身份运行，足够签到使用）。
- **`/TR` 改为 `cmd /c set "TRAEDATA_DIR={data_dir}" && "{py}" "{script}"`**，对含空格路径安全，且仅 set 成功才跑 python。
- **`task_status` / `task_unregister` / `task_register` 统一改用 `run_schtasks()`**：前置 `chcp 65001` 让 `schtasks` 以 **UTF-8** 输出，中文报错被正确解码 —— "找不到"/"系统找不到" 能正常匹配，返回友好提示"未注册每日签到任务…"而非乱码。
- **去掉重复前缀**：Rust 端不再包"查询计划任务失败："，只返回纯错误文案；前端 `查询失败：` 成为唯一前缀。任务不存在时返回 `Ok(...)` 友好提示（前端显示成功 + 提示文案，不再报红）。

> 说明：移除 `/RL HIGHEST` 后，`is_access_denied` 分支在普通用户场景基本不会被触发，但作为兜底提示予以保留，无副作用。

### 5. 验证方法
- 以**普通用户**身份打开应用 → 设置页"每日定时签到" → 输入时间（如 09:00）→【注册任务】。
  - 修复前：大概率权限错误或直接失败。
  - 修复后：提示"已注册每日 09:00 自动签到"。
- 【查询】：返回任务详情（下次运行时间等）；未注册时给出明确提示。
- 【取消】：提示"计划任务已删除"；再次查询应提示"未注册"。
- 真机验证：让系统在设定时间处于登录状态，观察 `logs/checkin.log` 是否产生当天的签到记录。

### 6. 使用注意事项（非 bug，但影响"是否生效"）
Windows 计划任务默认**仅在相应用户"已登录"时运行**。若设定时间电脑处于锁屏/注销/关机，任务不会触发。如需"无论是否登录都运行"，需在管理员 CMD 用 `/RU <用户> /RP <密码>` 显式指定，但这会要求保存密码——按当前"零配置"设计，保持默认即可，只要到点时人已登录即可。

---

## 三、已应用的代码改动清单

| 文件 | 改动 | 解决问题 |
|---|---|---|
| `src-python/device_proxy.py` | `tunnel_raw()` 增加 `200 Connection Established` 握手 + 上游失败回 `502` | 问题一·根因A（代理后外网打不开） |
| `src-python/device_proxy.py` | 新增 `connect_via_upstream()`（HTTP CONNECT + SOCKS5），`tunnel_raw`/`handle_plain` 对非 Trae 域名经 `UPSTREAM_PROXY` 转发 | 问题一·根因B（与 VPN 冲突） |
| `src-tauri/src/commands/proxy.rs` | `proxy_start` 读取并透传已有系统代理为 `UPSTREAM_PROXY`；`proxy_stop` 还原用户原有系统代理 | 问题一·根因B（与 VPN 冲突） |
| `src-tauri/src/commands/misc.rs` | `task_register` 移除 `/RL HIGHEST`、修正 `/TR` 引号与 `&&` | 问题二（注册失败/无效果） |
| `src-tauri/src/commands/misc.rs` | `task_status` / `task_unregister` 不再吞错误，给出明确结果 | 问题二（查询/取消"没反应"） |
| `src-tauri/src/commands/misc.rs` | 新增 `run_schtasks()`（`chcp 65001` 强制 UTF-8 输出），消除 GBK 乱码 + 去掉"查询计划任务失败："重复前缀 | 问题二·根因D（乱码 + 重复前缀） |

---

## 四、生效与复现步骤

### 问题一（Python 修复）
- **开发模式**（`npm run tauri dev`）：脚本直接读 `src-python/`，保存后**重启代理**即可生效，无需重编译 Rust。
- **已打包应用**：需重新 `npm run tauri build` 以把更新后的 `device_proxy.py` 打进 `resources/python/`。

### 问题二（Rust 修复）
- 必须重新编译：`npm run tauri build`（或 `npm run tauri dev` 重新启动 Rust 后端）。

### 通用回归验证
1. 启动应用，确认 `proxy_start` 后系统代理被设置（`reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings" | findstr ProxyEnable` 应为 `1`）。
2. 浏览器访问 github.com —— 应可正常打开（修复前不可）。
3. 设置页注册每日签到 → 查询 → 取消，三者均应有明确反馈。

---

## 五、结论
- **问题一**有**两层根因**：① `tunnel_raw` 漏发 CONNECT 隧道握手应答（`200 Connection Established`）的对称性 bug（已修）；② **app 代理与 VPN 冲突**——代理把全局流量劫持到本机却直接 outbound、未串 VPN，导致外网断、内网通（本轮通过「上游代理链式转发」彻底解决）。
- **问题二**主要是 `schtasks /RL HIGHEST` 强制管理员权限 + `/TR` 路径未加引号 + 查询/取消吞错误导致的"注册不成功 / 看起来没反应"，外加 `schtasks` 的 **GBK 输出被按 UTF-8 误读成乱码**、并与前端"查询失败："叠加成**重复前缀**（本轮通过 `chcp 65001` 强制 UTF-8 + 去重前缀解决）。
- **重要生效提示**：`device_proxy.py` 是打包进 `resources/python/` 的，问题一的修复**必须用 `npm run tauri build` 重新打包**才会进入正式版；开发模式 `npm run tauri dev` 直接读 `src-python/` 重启代理即生效。
