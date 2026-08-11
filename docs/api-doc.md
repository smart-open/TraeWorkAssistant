# API 文档 — Tauri 前端 ↔ Rust 命令契约

> 前端通过 `invoke('command', args)` 调用。所有命令运行在 Rust 主线程（耗时操作放子线程并通过事件回传）。错误以 `string` 返回（空=成功），或以 `{ ok: boolean, error?: string }` 结构返回。

## 1. 环境检测

### `env_check()` → `EnvStatus`
- 返回：`{ installed: boolean, running: boolean, version: string|null, path: string|null }`
- 副作用：无。

### `open_trae_website()` → `void`
- 打开 `https://www.trae.cn`（未安装引导）。

## 2. CA 证书

### `cert_status()` → `{ installed: boolean }`
### `cert_install()` → `{ installed: boolean, error?: string }`
- 提权（UAC）将 `certs/ca.cer` 装入本地计算机受信任根证书颁发机构。

## 3. 代理服务

### `proxy_start(port: number)` → `{ ok: boolean, error?: string }`
- 启动 `device_proxy.py` 子进程（env: `PROXY_PORT`, `AUTO_CAPTURE_JWT=1`）。
- 事件 `proxy-log`（逐行）、`account-captured`（uid）。

### `proxy_stop()` → `{ ok: boolean }`
- 优雅退出（CTRL_BREAK）→ 超时强杀。

### `proxy_status()` → `{ running: boolean, port: number, captured: number, started_at: number|null }`

## 4. 账号与分组

### `accounts_list()` → `Account[]`
- `Account`: `{ userId, name, groupId|null, jwtExpHours: number|null, checkedToday: boolean|null, credits: number|null, deviceIdMasked: string|null }`

### `account_add_manual(name, jwt, groupId?)` → `{ ok, error? }`
### `account_delete(userId, deleteProfile)` → `{ ok, error? }`

### `groups_list()` → `Group[]`
- `Group`: `{ id, name, color, order, count }`

### `group_create(name, color)` → `{ id }`
### `group_update(id, name?, color?, order?)` → `{ ok }`
### `group_delete(id)` → `{ ok }`（账号回落未分组）
### `group_move(userId, groupId|null)` → `{ ok }`

## 5. 签到

### `checkin_start(opts)` → `{ ok, error? }`
- `opts`: `{ scope: "all"|"group:<id>"|"selected", userIds?: string[], skipCheckedIn: boolean, skipExpired: boolean }`
- 启动 `auto_checkin.py --json-stream --scope ... --accounts ...`，通过事件 `checkin-progress` 回传 NDJSON 行（`start`/`account`/`done`）。
- 完成事件 `checkin-done`：`{ ok, already, failed, total }`。

## 6. JWT 续期 / 切换 / 设备

### `jwt_parse(jwt)` → `{ userId, expHours, status: "ok"|"warn"|"expired" }`
### `switch_account(userId)` → `{ ok, error? }`
- 调 PowerShell 切换器（非交互 `-Action Switch -UserId`）；事件 `switch-progress` 回传步骤。
### `device_reset(userId)` → `{ ok, error? }`
- 删除 `device_map.json` 该条目。

## 7. 日志 / 设置 / 邀请

### `logs_query(opts)` → `LogLine[]`
- `opts`: `{ type?: "proxy"|"checkin"|"switch"|"system"|"error", date?: "today", keyword?: string, limit?: number }`
- `LogLine`: `{ time, type, message }`

### `settings_get()` / `settings_set(patch)` → `AppSettings`
### `invite_link()` → `{ url }`（固化邀请链接）

## 8. Python 子进程 NDJSON 协议（签到）

`auto_checkin.py --json-stream` 输出：

```
{"type":"start","total":6}
{"type":"account","index":1,"user_id":"4487…","name":"清风杜宾","status":"already","credits":8200,"elapsed":0.31}
{"type":"account","index":2,"user_id":"1556…","name":"青衣网络","status":"success","delta":300,"elapsed":1.24}
{"type":"account","index":3,"user_id":"…","name":"…","status":"fail","code":1001,"message":"JWT 无效"}
{"type":"done","ok":5,"already":0,"failed":1}
```

`status` 取值：`already`(已签) / `success` / `fail`。前端据此渲染进度与颜色。

## 9. 数据 DTO 关系

- `UserID` 为所有关联主键；`Account.groupId` → `Group.id`；`Account.deviceIdMasked` 来自 `device_map.json`；`credits` 来自 `checkin_summary.json` / `credits_history.json`。
