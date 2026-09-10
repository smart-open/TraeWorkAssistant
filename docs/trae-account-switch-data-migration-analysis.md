# Trae 账号切换时会话/项目迁移可行性分析

> 创建时间：2026-09-10。本文档分析 Trae Work助手「切换账号」功能的数据归属现状，评估**切换账号时将账号的会话（SOLO 对话）与创建的项目迁移到目标账号下**的可行性，并给出分期落地方案。
> 结论摘要：**「项目」跨账号共享完全可行且低风险（本地改造即可）；「会话」的真迁移受云端账号绑定限制，仅能做到导出存档（P2）或复制回放（P3，高风险），不建议首版实现。**

## 1. 背景

当前账号切换通过 `src-ps/trae-switch-bridge.ps1` 完成：按账号槽位快照/恢复 Trae 的本地数据（`storage.json` 登录态、`state.vscdb` 全局状态库、`aha/`、Chromium 系列目录）。切换后 Trae 内的会话列表与项目列表会"消失"，实际是云端数据按新账号重新拉取 + 本地索引被槽位快照整体回滚的叠加效果。

本分析基于对本机 `%APPDATA%\TRAE SOLO CN` 数据目录的实测侦察（只读），不涉及任何写入操作。

## 2. 数据归属分层（实测结论）

| 层 | 位置 | 内容 | 归属 | 切换时行为 |
|---|---|---|---|---|
| **云端** | SOLO 服务端 | 会话（`chat_session_id`，Mongo ObjectId 风格）+ 项目（`project_id`），按 `user_id` 隔离 | 服务端按账号隔离 | 切号后云端拉取新账号数据，旧会话/项目在 UI 中"消失" |
| **本地索引** | `User/globalStorage/state.vscdb`（bridge 快照已覆盖） | `solo-lite:content-map:<uid>`（会话→工作区映射，**按账号分区**）；`solo-lite-mode-state-map-<uid>`（会话对象内嵌 `user_id`）；`solo-lite.local-project-folders`（项目 id→本地路径，**全局键**）；`history.recentlyOpenedPathsList`（最近打开，全局键） | 账号分区键 + 全局键混合 | 被目标槽位快照**整体回滚** ← 项目列表"丢失"的根源 |
| **本地文件** | `User/workspaceStorage`（11 个）、`User/History`、`Workspaces` | 工作区状态、本地文件历史 | 全局共享 | bridge **未快照**，天然跨账号保留 |
| **本地缓存** | `IndexedDB`（仅约 2KB）、`WebStorage` | 几乎为空 | — | 会话正文不在本地，权威数据在云端 |

### 关键证据

1. **state.vscdb 共约 200 个键，其中 7 个账号前缀（uid）的键共存** —— Trae 客户端自己就按 uid 分区存储各账号的索引数据，切换时只是读取不同前缀。
2. **`solo-lite:content-map:<uid>`**：会话 id → 工作区的映射，按账号分区；跨账号复制会产生"幽灵会话"。
3. **`solo-lite-mode-state-map-<uid>`**：会话对象内部显式携带 `user_id` 字段，服务端会做归属校验。
4. **`solo-lite.local-project-folders`**：项目 id → 本地磁盘路径的注册表，**全局单键**，不按账号分区 —— 项目本体就是本地文件夹，天然可跨账号访问。
5. **会话 id 为云端 ObjectId 风格**、IndexedDB 几乎为空：会话正文的权威存储在云端，本地只是索引与缓存。
6. **bridge 快照清单未覆盖** `workspaceStorage` / `User/History` / `Workspaces`：这些数据切换后保留，属于全局共享。

## 3. 三个场景的可行性

### 场景 A：项目跨账号可见 — ✅ 可行（推荐先做）

项目本体是本地文件夹（如 `d:\ai_work\skills`），`local-project-folders` 是全局键。唯一问题是 bridge 恢复槽位快照时把 `state.vscdb` 整体回滚，项目列表退回目标账号快照时点的旧数据。

**解法**：恢复前把全局键抽出、恢复后合并写回。改动集中在 `src-ps/trae-switch-bridge.ps1`，加一步 SQLite 键级合并即可，工作量约 1-2 天。详见第 4 节 P1。

### 场景 B：会话只读档案 — ⚠️ 可行，建议二期

用旧账号 jwt 调 SOLO 会话接口导出对话内容（本项目已有 mitm 代理与 jwt 基础设施），在助手中提供按账号归档的 Markdown 存档查看器。

- 优点：不改变云端数据归属，零风控风险，旧账号会话内容可随时查阅。
- 局限：只是"存档"，新账号下不能继续对话。

### 场景 C：会话真迁移到新账号名下 — ❌ 不建议首版实现

本地把 A 账号的 `content-map:<A>` 复制到 `<B>` 名下没有用 —— B 的 jwt 拉取 A 的 session 会被服务端按 `user_id` 归属校验拒绝，产生无法加载的"幽灵会话"。

真迁移需要以 B 的身份通过 SOLO 前端接口**重建会话并回放消息**：

- 涉及非公开接口，消息结构未必可回放（工具调用、文件引用、图片等）；
- 存在账号风控风险；
- Trae 版本演进后接口易碎，维护成本高。

**建议**：先抓包验证消息体结构可回放，再决定是否投入；否则放弃该场景。

## 4. 分期落地方案

### P1：项目列表跨账号保留（本轮建议落地）

在 `trae-switch-bridge.ps1` 的 `Switch` / `RestoreOnly` 管线中增加"全局键保留合并"步骤：

```
恢复槽位快照前：
  1. 从当前（即将被覆盖的）state.vscdb 抽出全局键：
     - solo-lite.local-project-folders
     - history.recentlyOpenedPathsList
  2. 恢复目标槽位快照（整体覆盖 state.vscdb）
恢复后、启动 Trae 前：
  3. 将抽出的键合并写回新 state.vscdb
     - local-project-folders：按项目 id 合并（快照内已有的以快照为准，缺失的补回）
     - recentlyOpenedPathsList：合并去重，保留最近打开时间排序
  4. 启动 Trae
```

效果：**切到任何账号，项目列表、最近打开都在**；`workspaceStorage` / `History` 本就未被快照，工作区状态天然保留。

实现要点：

- SQLite 键级读写用 PowerShell + `System.Data.SQLite` 或打包一个小型 sqlite3 工具（项目约束：零新增依赖，优先用系统自带/已有工具链）；
- 操作前对 `state.vscdb` 做一次性备份（`.bak`），失败可回滚；
- 必须在 Trae **未运行**时操作（切换流程本就先关闭 Trae，满足条件）。

### P2：会话导出存档（二期）

- 用各账号 jwt 调 SOLO 会话接口，导出为 Markdown，按账号归档到助手数据目录；
- 前端提供存档浏览器（按账号 / 日期 / 项目筛选）。

### P3：会话复制回放（实验性，暂缓）

- 前置条件：抓包确认消息体结构可回放；
- 以目标账号身份重建会话并逐条回放消息；
- 高风险、高维护成本，P1/P2 价值兑现后再评估。

## 5. 边界与红线（不可合并项）

| 数据 | 处理 | 原因 |
|---|---|---|
| `storage.json`、`machineid` 等登录态 | **绝不合并** | 合并会导致切换失效、多账号登录态互相污染 |
| `content-map:<uid>` 等账号分区键 | **不跨账号合并** | 产生服务端归属校验失败的"幽灵会话" |
| `aha/`、Chromium 系列目录 | 维持现状（随槽位快照） | 与账号绑定的缓存/凭据，合并风险不明 |

## 6. 参考位置

- 切换实现：`src-ps/trae-switch-bridge.ps1`（快照/恢复管线）
- 切换命令入口：`src-tauri/src/commands/switch.rs`
- 实测数据目录：`%APPDATA%\TRAE SOLO CN\`（`User/globalStorage/state.vscdb`、`User/workspaceStorage`、`IndexedDB` 等）
