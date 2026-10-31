# 代码审查报告：dfa6e02 以来合并代码 + 本地未提交变更

- **日期**：2026-10-31
- **分支**：macos_main
- **审查范围**：main 分支 `dfa6e02`（不含）以来的合并提交（`d2121ef` 等）+ 本地未提交变更
- **审查维度**：逐行 / 逐功能——功能完整性、逻辑正确性、设计全面性、系统兼容性（Windows/macOS 双平台）、bug、错误处理、安全，并重点核查「原有功能正常」
- **结论**：**0 High / 5 Medium / 若干 Low**，全部确认的问题已修复完毕；行尾归一化问题经字节级实证后修复。回归验证全绿（cargo test 1203 passed / 0 failed，npm test 111 passed）。

---

## 一、审查与修复总览

| 严重度 | 合并代码 | 本地未提交 | 处置 |
|---|---|---|---|
| High | 0 | 0 | — |
| Medium | 5 | 0 | 5 项全部修复 |
| Low | 合计 16 项发现 | 1 项（loadModels toast） | 修复 5 项；其余经评估维持现状（记录于 §四） |

除代码缺陷外，另修复了一项**仓库工程问题**：`.gitattributes` 变更后的行尾归一化错乱（§五）。

---

## 二、Medium 发现与修复（5 项，全部已修复）

### M1. vault 豆包抓包凭证 ns 断代（数据丢失风险）

- **位置**：`src-tauri/src/vault.rs`（`doubao_captured_get`）
- **问题**：经 `git show b1531f5` 实证，3.7.1 版本将抓包凭证写入 vault 命名空间 `doubao`/`captured`（迁移④与 handler 写入点均如此）；当前代码只读新 ns `doubao_captured`/`credentials`，其「旧 kv 回退」读的是已被 3.7.1 迁移④删除的旧 kv 存储——**3.7.1 抓包的豆包凭证在当前版本永远读不到**（断代成立）。
- **修复**：读取侧自愈搬迁——读新 ns 未命中时尝试旧 ns `doubao`/`captured`：`ns_set` 新 ns 成功才 `ns_remove` 旧条目并记 app_log；搬迁失败保留旧条目下次重试。不依赖重启、最多触发一次。（`doubao.rs:187` 的 `"doubao"` ns 是账号池凭证，仍活跃使用，与本问题无关。）

### M2. updater 拉起新版应用无超时（可无限挂起）

- **位置**：`src-tauri/src/commands/updater.rs`（mac 分支 `open` 调用）
- **问题**：安装完 DMG 后用系统 `open` 拉起新版应用，直接 `.status()` 同步等待且无超时；`open` 卡死（系统异常）会让更新流程永久挂起。
- **修复**：改用 `platform::cmd::sys_command("open")` + 新增的 `sys_status_timeout(&mut cmd, Duration::from_secs(5))`，超时报错「拉起新版应用失败」并上抛。

### M3. ide_store 违反 sys_command 红线（全仓 CLI 收口）

- **位置**：`src-tauri/src/commands/qoder/ide_store.rs`
- **问题**：直接 `Command::new("security")` 调用 macOS security CLI，违反项目红线「全仓系统 CLI 调用必须经 `platform::cmd::sys_command` 构建」（该收口负责 PATH/环境一致性）。
- **修复**：改为 `crate::platform::cmd::sys_command("security")`，import 收敛为 `use std::process::Stdio;`。

### M4. locate.rs mdfind 无超时 + 首 root 失败整体放弃

- **位置**：`src-tauri/src/switcher/locate.rs`（`mdfind_bundle`）
- **问题**：① `mdfind` 无超时——Spotlight 元数据查询卡死会挂住调用方；② `.output().ok()?` 在第一个搜索 root 查询失败时直接 `?` 返回 None，放弃后续所有 root（一票否决缺陷）。
- **修复**：`platform/cmd.rs` 新增 mac 专用 `sys_output_timeout`（spawn + try_wait 20ms 轮询 + deadline 到期 kill+wait 收尸，非零退出映射为 Err）；`mdfind_bundle` 逐 root 调用并设 3s 超时，`Err => continue` 继续 next root，同时移除了类型变更后的 `status.success()` 残留。

### M5. Buddy AI 自动探测覆盖用户的显式关闭决定

- **位置**：`src/store.ts`（启动自动探测块）
- **问题**：自动探测后仅检查 `settings?.buddy_ai_visible === true` 作为「用户已决定」守卫——只挡显式**开启**；用户显式**关闭**后，启动探测仍会用 `saveSettings` 把 `buddy_ai_visible` 翻回 `true`，用户决定被覆盖。
- **修复**：探测 `await` 返回后重读 `BUDDY_AI_VIS_DECIDED_KEY` 替换原检查。`GeneralSettingsPanel` 的 onChange 同步先写该标记再 update，故重读守卫可同时覆盖显式开启与显式关闭两种决定。注释中保留已知毫秒级窗口说明（重读后到 saveSettings 之间，一次性启动逻辑不再加重）。

---

## 三、Low 发现与处置（已修复 5 项）

### L1. vault 密钥文件 mac 原生写入非原子（已修复）

- **位置**：`src-tauri/src/platform/secret.rs`（`store_vault_password_at_impl` mac 分支）
- **问题**：`OpenOptions` create+truncate 与 `write_all` 两步非原子，中途崩溃/断电留下 0 字节 key file（decode_hex 已按损坏源防御，但会 fail-open 到 Keychain 兜底，旧值可能已过期且与密文失配）。
- **修复**：复用 `vault::write_key_file_atomic`（tmp 以 0600 创建 + fsync + rename 原子替换，rename 失败清 tmp）。写纯 hex 格式不变；rename 后新 inode 即 0600，原 `set_permissions` 兜底一并删除；`decode_hex` 内过时注释同步更新（防御逻辑保留，覆盖历史文件/外部写入场景）。

### L2. process.rs 永不编译的死 cfg 块（已修复）

- **位置**：`src-tauri/src/commands/process.rs`
- **问题**：`graceful_kill_images`（函数本体 `#[cfg(not(macos))]`，内嵌 mac 块引用不存在的 `mac_name_matches`）与 `images_running` 内嵌 mac 块——两处死 cfg 块约 54 行永不编译，且引用不存在符号，属于「看似有 mac 逻辑实则没有」的误导性代码。
- **修复**：删除两处死块；`images_running` 过时 doc comment（macOS sysinfo 描述）同步修正。`mac_snapshot` 仍被 mac 路径使用，保留。

### L3. qoder/common.rs 死 cfg 块（已修复）

- **位置**：`src-tauri/src/commands/qoder/common.rs`
- **问题**：windows 门控的 `ide_split_installed` / `is_stale_ide_manual` 内嵌 mac / not(windows) 死块（真实平台分支为下方独立 cfg 版本）。
- **修复**：删除内嵌死块，doc comment 同步更新；`mac_app_bases` 仍被 mac 版本使用，保留。

### L4. build-macos.yml cargo 缓存无回退键（已修复）

- **位置**：`.github/workflows/build-macos.yml`
- **问题**：cache key 精确绑定 `hashFiles('src-tauri/Cargo.lock')`，升级任一依赖后精确键 miss 即全量重编译（三矩阵 × 全量，CI 时长显著劣化）。
- **修复**：增加 `restore-keys: ${{ runner.os }}-${{ matrix.target }}-cargo-`，Cargo.lock 变更后回退到本架构最近一次缓存做增量编译。

### L5. Qoder 账号编辑空名会把昵称清空（已修复）

- **位置**：`src/pages/qoder/QoderAccounts.tsx`（`saveEdit`）
- **问题**：`accountSave(editing.id, editName.trim(), ...)` 空名传 `""`，后端 `Some("")` 会把 `nickname` 覆盖为空串；编辑弹框亦无空名禁用逻辑。与 Buddy（`editName || undefined`）、Doubao（`|| undefined`）的「留空即保持原名」既有语义不一致。
- **修复**：改为 `editName.trim() || undefined` 并注释说明（tauri 层 `?? null` 传 `null` → 后端 `None` → 不修改）。

---

## 四、评估后维持现状的记录项

以下发现经评估**不构成需要改码的缺陷**，记录备查：

1. **store.ts decided 标记时序（L2）**：GeneralSettingsPanel onChange 同步先落盘 decided 标记、后 `saveSettings`；若保存失败，decided 已写 → 自动探测对该用户永不重试。评估：一次性启动逻辑、窗口毫秒级、触发前提是本地磁盘写失败；为它增加回滚/重试会引入新竞态，得不偿失。维持现状。
2. **本地未提交变更的 1 项 Low（loadModels toast）**：属加载失败提示体验类的建议性发现，评估后维持现状，不改码。
3. 其余 Low 级观察项均为防御性备注类（已在对应代码注释中固化结论，如 secret.rs 模块头的 mac key file 威胁模型注记、Keychain fail-open 决策等），无需额外改动。

---

## 五、行尾归一化专项（仓库工程问题，已修复）

**背景**：`.gitattributes` 变更后，`git add` 因 stat 未变不重新哈希，产生大量「幽灵 diff」；前会话曾得出「6 个文件 HEAD blob 为 CRLF」的结论。

**字节级实证纠偏**（PowerShell 管道破坏字节，改用 `node -e` + `execSync` Buffer 逐 blob 统计）：

- 全仓 373 个 blob 扫描：**文本文件中 HEAD 为 CRLF 的只有 `src-tauri/src/api_server/dispatch.rs`**（2983 CRLF / 3 LF）；其余 5 个涉事文件（config_cache.rs / wb_route.rs / wb_credits.rs / CreditsTab.tsx / TokensTab.tsx）HEAD 全为 LF（其余 CRLF 命中均为 png/ico/icns/jpg 二进制误报）。
- 首轮 `git add --renormalize` 按 6 路径 `-text` 豁免，把 5 个 LF-HEAD 文件以 CRLF 入库，raw diff 膨胀（如 wb_route 3125/2862 vs 真实 304/41）——前会话结论被推翻并纠正。

**最终修复**：

- `.gitattributes` 重写为**单路径豁免**：仅 `dispatch.rs -text`（保持与 HEAD CRLF 字节一致，注释说明原因与新增文件的处理约定）；其余文本维持默认处理（本机 `core.autocrlf=true` 入库归一为 LF 正确）。
- 5 个 LF-HEAD 文件重新 renormalize 入 LF。
- **双重验证通过**：`git ls-files --eol`——dispatch.rs `i/crlf`、其余 `i/lf`，与 HEAD 一致；raw numstat 与 `--ignore-cr-at-eol` numstat 完全吻合：`.gitattributes` 7/0、config_cache 111/39、dispatch 132/29、wb_route 304/41、wb_credits 123/17、CreditsTab 61/46、TokensTab 49/38——diff 中不再有行尾噪音，blame 不断裂。

---

## 六、项目红线与专项核查结果

| 红线 / 专项 | 结果 |
|---|---|
| 全仓 CLI 调用必须经 `platform::cmd::sys_command` | 发现 1 处违规（M3，ide_store.rs `security`），已修复；其余调用点核查通过 |
| `NoHealthyWithOrder` 必须有 match 分支 | 通过（t40 系列测试全绿） |
| `exhaust_message` 族签名（pool 参数后置） | 通过（编译 + 测试） |
| Windows-only 符号 `#[cfg(windows)]` 门控 | 发现 2 处死 cfg（L2/L3），已清理；门控干净 |
| macOS 子进程超时规范（sys_status_timeout / sys_output_timeout） | M2/M4 修复后全部系统调用均有超时路径 |
| vault 凭证「禁止明文落盘、写失败不销毁既有凭据」 | 通过；key file 写入原子性同步补强（L1） |
| 前端并发守卫（在途 ref 镜像 + 快照序号） | 通过；M5/L5 补齐用户决定守卫与语义一致性 |

## 七、「原有功能正常」专项确认

- 所有修复均为**行为收敛型**：超时兜底（M2/M4）、回退链补全（M1）、原子性（L1）、语义对齐（M5/L5）、死代码清理（L2/L3）、CI 缓存（L4）——无破坏性 API/数据格式变更：
  - vault key file 纯 hex 格式不变（L1），旧 ns 搬迁失败保留原条目可重试（M1）；
  - updater / mdfind 正常路径行为不变，仅增加超时与容错（M2/M4）；
  - Qoder 改名非空输入行为不变，仅空名从「清空昵称」收敛为「保持原名」（L5）；
  - Windows 构建零变化（M2/M4/L1 均为 mac 专用分支或 macos CI）。
- **回归验证全绿**：
  - `cargo test --manifest-path src-tauri/Cargo.toml`：**1203 passed / 0 failed / 11 ignored**（64.62s）；
  - `npm test`（vitest）：**10 个测试文件 / 111 passed**（2.88s）。

---

## 八、修复文件清单

**Rust 后端**：
- `src-tauri/src/vault.rs`（M1：旧 ns 自愈搬迁）
- `src-tauri/src/commands/updater.rs`（M2：open 超时）
- `src-tauri/src/commands/qoder/ide_store.rs`（M3：sys_command）
- `src-tauri/src/platform/cmd.rs`（M4：新增 `sys_output_timeout`）
- `src-tauri/src/switcher/locate.rs`（M4：mdfind 超时 + continue）
- `src-tauri/src/platform/secret.rs`（L1：key file 原子写）
- `src-tauri/src/commands/process.rs`（L2：死 cfg 清理）
- `src-tauri/src/commands/qoder/common.rs`（L3：死 cfg 清理）

**前端**：
- `src/store.ts`（M5：decided 标记重读）
- `src/pages/qoder/QoderAccounts.tsx`（L5：空名语义）

**工程**：
- `.gitattributes`（行尾豁免单路径化 + 重刷 5 文件入 LF）
- `.github/workflows/build-macos.yml`（L4：restore-keys）
