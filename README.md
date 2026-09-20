<div align="center">

<img src="build-assets/app-icon-rounded.png" alt="AI Work 助手" width="128" />

# AI Work 助手（AI Work Assistant）

Windows / macOS 双平台多账号签到与管理一站式工作台 · Tauri 2 + React 18 + Rust

**GitHub**：[github.com/smart-open](https://github.com/smart-open) · **个人博客**：[blog.sopenai.cn](https://blog.sopenai.cn/)

</div>

![AI Work 助手](./docs/images/main.png)

> 深度支持 **Trae Work / Trae（Trae CN IDE）/ WorkBuddy / CodeBuddy / 豆包** 五应用：多账号签到、登录态切换、积分看板、成长中心自动化、OpenAI / Anthropic / Codex 三协议兼容 API 网关（Trae + WorkBuddy + 自定义模型三池调度）、6 层设备标识重置等；Trae 双应用同一账号体系可分别切换，WorkBuddy 与 CodeBuddy 共享账号体系，豆包支持快照切换 / 保活 / 额度巡检 / 对话备份。**macOS 版已可用**（Apple Silicon + Intel dmg；部分 Windows 专属域按平台灰度，详见下方 macOS 说明）。后续规划扩展更多 AI 应用。
>
> ⚠️ 本工具与 Trae Work / WorkBuddy / 豆包等官方均无任何关联，仅供学习研究。使用本工具可能违反相关服务条款，风险自担。请仅管理本人合法持有的账号。

## 🚨 说明：豆包公开 Open API 暂时搁置

> **豆包「公开 Open API」相关功能暂时搁置，恢复时间待定。**
>
> 目前豆包官方风控较为严格，作者本人多个账号已被封禁。为避免更多用户遭受账号损失，该方向的功能暂停推进，请知悉并谨慎评估相关使用风险。

## 版本与分支

- **v3.x 新版本线（默认分支）**：产品为「AI Work 助手」，支持 Trae Work / Trae（Trae CN）/ WorkBuddy / CodeBuddy / 豆包多应用；新版本自 **3.0.0** 起开始维护。
- **原「Trae Work 助手」产品**：通过 **`trae_work_main`** 分支维护，仅支持 Trae Work 单应用，版本停留在 **2.x.x**，仅做必要修复、不再新增功能。
- **升级与数据迁移**：新版本从 3.0.0 开始，**之前所有版本（2.x 全系）升级到 3.x 都需要迁移数据**——数据目录、界面偏好、签到计划任务会在安装 / 首次启动时**自动完成迁移**，无需手动操作（详见下方「从老版本升级」）。

## 免责声明

> 本工具仅供学习研究和个人使用，使用者需自行承担一切风险与后果。

1. **非官方申明**：本工具与 Trae / TRAE Work 等相关产品官方**无任何隶属、合作或关联关系**，系个人开源项目，不代表官方立场。
2. **使用风险**：使用本工具可能违反 Trae Work 的服务条款；由此产生的任何后果（包括但不限于账号封禁、积分清零/扣除、功能限制、数据异常等）均由使用者自行承担。
3. **责任范围**：本工具不对因使用（或无法使用）本工具所导致的任何直接、间接、附带或后果性损失负责。
4. **合规义务**：使用前请务必仔细阅读 Trae Work 的服务条款，并自行判断是否使用；请确保仅用于管理本人合法持有的账号，遵守所在地法律法规。
5. **作者免责**：本工具作者对任何因使用、误用或滥用本工具而引发的纠纷、争议或问题不承担任何责任。
6. **侵权处理**：若您是相关官方且认为本工具侵犯了您的合法权益，请通过项目渠道联系作者，我们将在核实后及时下架处理。

**使用本工具即表示你已阅读、理解并同意上述全部免责声明。**

## 功能

- **账号管理**：多账号录入/编辑/OAuth 登录（含 WorkBuddy 扫码）、分组管理、设备 ID 隔离、**本机双应用（Trae Work / Trae）账号自动发现**、WorkBuddy/CodeBuddy auth 文件扫描入池、豆包抓包凭证回写
- **登录态切换**：按目标应用独立切换——保存当前登录态 → 恢复目标账号 → 启动；Trae 系精准备份 9 类核心文件，豆包 chromium 布局含快照版本校验 + 单代回滚 + 防误覆盖守卫，WorkBuddy/CodeBuddy authfile 布局；支持「一键以账号打开」
- **一键签到**：批量签到、按分组/手动勾选、跳过已签/过期、实时进度；WorkBuddy 成长中心自动化（旅行/盲盒/任务领奖）；豆包/WorkBuddy 定时保活与续期
- **积分看板**：排行、三线趋势图、今日新增统计；WorkBuddy 积分三件套 + 官方用量 + 本地 Token 统计（缓存命中率/热力图）+ 到期日历
- **本地代理**：MITM 代理自动捕获 JWT / 豆包凭证、注入独立设备 ID；**自动串联已有系统代理（VPN）作为上游**，停止时原样还原系统代理
- **API 网关**：内嵌 OpenAI / Anthropic / Codex Responses 三协议兼容 API 服务——Trae 账号池 + WorkBuddy 池 + 自定义 OpenAI 兼容模型三池调度（smart 智能策略/优先级/模型级覆盖）、会话粘性、ck_ 子 Key、四段模型路由、审核指纹清洗、生图双端点、web_search 工具代执行
- **定时任务**：Windows 计划任务 + 应用内调度器双轨，后台自动签到 / 保活 / 续期 / 额度巡检
- **6 层设备标识重置**：machineid / storage.json 遥测 / aha.device / 注册表 MachineGuid / webview 追踪数据 / aha TinyStorage
- **快照管理**：查看/备份/恢复/删除账号登录态快照（各应用独立管理）；豆包/WorkBuddy 对话数据独立备份恢复与导出
- **暗色模式**：6 套主题，图表动态适配
- **数据全部本地存储**，不上传任何服务器

## 开发

```powershell
npm install
npm run tauri dev      # 开发模式
npm run tauri build    # 打包（msi + nsis，Windows 平台自动合并 tauri.windows.conf.json）
node scripts/rename_release.mjs     # 安装包统一输出到 release/，命名 AI Work 助手_<版本>_x64*
node scripts/package_portable.mjs   # 便携版 zip（AI Work 助手_<版本>_x64_portable.zip）
```

前置：Node.js 18+、Rust 1.75+、WebView2 Runtime、VS Build Tools (C++)

macOS（F-75，Apple Silicon + Intel）：

```bash
npm install
npm run tauri build -- --bundles dmg   # 打包 dmg（自动合并 tauri.macos.conf.json，产出对应架构镜像）
node scripts/rename_release.mjs        # 输出 release/，命名 AI Work 助手_<版本>_aarch64|x64.dmg
```

## macOS 安装与「无法验证开发者」放行（未公证应用）

macOS 版为 **ad-hoc 签名**（暂未购买 Developer ID 公证），首次打开会被 Gatekeeper 拦截，两种放行方式任选其一：

1. **右键打开**：在「应用程序」文件夹中**右键点击**「AI Work 助手」→「打开」→ 再点「打开」（此后不再提示）；
2. **终端命令**：`xattr -d com.apple.quarantine "/Applications/AI Work 助手.app"`

系统级定时注册（schtasks）与设备系统级 MachineGuid 重置仅 Windows 可用；macOS 签到由内置调度器 + 开机自启覆盖。各目标应用的 macOS 版数据布局支持范围以应用内提示为准（灰度逐步放开）。

## 从老版本升级

> **提示**：如果你只使用 Trae Work（不需要 TRAE SOLO / WorkBuddy 等新支持的应用），可以不升级——原「Trae Work 助手」产品线在 `trae_work_main` 分支维护（2.x.x，仅必要修复），使用 v2.x.x 最新版本即可，功能完全一致。
>
> **新版本从 3.0.0 开始**：之前所有版本（2.x 全系）升级到 3.x 都需要迁移数据，迁移在安装 / 首次启动时自动完成。

升级兼容按**安装时的产品名**判定，与版本号无关：

- **旧品牌「Trae Work 助手」（已发布的 v2.4.4 及更早）**：新版安装时会自动结束旧进程、**静默卸载**并清理残留（安装目录 / 卸载键 / 快捷方式），首次启动自动完成数据迁移，无需手动操作。
- **新品牌「AI Work 助手」（v3.0.0 起）**：产品名一致，直接双击新安装包即走**原生原地升级**，用户数据不受影响。
- **自动迁移内容**：数据目录 `%APPDATA%\TraeWorkAssistant` → `%APPDATA%\AIWorkAssistant`（**复制**迁移，旧目录原地保留，**老应用可继续使用、新旧两版可并存**；新目录已有数据则自动跳过，不会重复迁移）、界面偏好、每日签到计划任务（按原触发时间重建新任务，旧任务保留给老应用）。
- **MSI 安装包**：因产品标识变更，老版 MSI 无法原地升级，请先卸载旧版再安装（或改用 NSIS 安装包，推荐）。
- 旧版定时任务如未自动迁移，请在「设置 → 定时任务」重新注册一次。

## 数据目录

```
%APPDATA%\AIWorkAssistant\
├── conf/
│   ├── app_settings.json        # 设置
│   ├── vault.stronghold         # 凭证加密保险库（DPAPI 保护主密码）
│   └── vault_key.bin
├── data/
│   ├── aiwork.sqlite            # 全量状态库（账号 / 分组 / 积分 / 设备映射 / 签到结果 /
│   │                            #   API 池与 Key / 模型目录 / 用量统计 / WorkBuddy / 豆包状态）
│   ├── backup/                  # 首次升级时旧版 JSON 数据自动迁入 SQLite 后的备份
│   ├── certs/                   # 自签 CA 证书
│   ├── profiles*/               # 各应用登录态快照（按账号 ID 分目录 + .bak 单代回滚）
│   └── exports/                 # 对话记录等导出产物
└── logs/                        # proxy / checkin / switcher / api 日志
```

> 从 3.4.x 之前的版本升级：旧版 `data/` 下的 JSON 状态文件会在首次启动时**自动导入 SQLite** 并移入 `data/backup/`，无需手动迁移。

## 文档

- [更新日志](CHANGELOG.md) — 各版本变更记录
- [用户手册](docs/user-manual.md) — 功能说明与使用指南
- [产品设计](docs/product-design.md) — 需求与产品设计基线（v1.0/v2.0）
- [产品优化需求清单](docs/backlog.md) — 全项目唯一待办依据（需求概述/实现路径/参考开源项目）
- [技术架构设计](docs/tech-framework.md) — 架构/数据模型/协议参考（含 WorkBuddy、豆包协议附录与开源仓库映射）/开发运维

## 赞赏

如果这个项目对你有帮助，欢迎请作者喝杯快乐水 ☕

<div align="center">
<img src="src/assets/donate-qr.jpg" alt="赞赏码" width="220" />
</div>

## License

本项目采用 [MIT License](LICENSE)，版权归 **朱天伟**（Copyright © 2026 朱天伟）所有。

Fork / 二次开发请保留 `LICENSE` 及版权声明；引用或借鉴请注明原作者及原始仓库 `https://github.com/smart-open/TraeWorkAssistant`，派生项目须说明以原库为基础，原库版权与出处不变。
