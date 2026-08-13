# Trae Work 助手

Windows 桌面端多账号签到与管理工具 · Tauri 2 + React 18 + Rust

> ⚠️ 本工具与 Trae Work 官方无任何关联，仅供学习研究。使用本工具可能违反 Trae Work 服务条款，风险自担。请仅管理本人合法持有的账号。

## 免责声明

> 本工具仅供学习研究和个人使用，使用者需自行承担一切风险与后果。

1. **非官方申明**：本工具与 Trae / TRAE Work 官方**无任何隶属、合作或关联关系**，系个人开源项目，不代表官方立场。
2. **使用风险**：使用本工具可能违反 Trae Work 的服务条款；由此产生的任何后果（包括但不限于账号封禁、积分清零/扣除、功能限制、数据异常等）均由使用者自行承担。
3. **责任范围**：本工具不对因使用（或无法使用）本工具所导致的任何直接、间接、附带或后果性损失负责。
4. **合规义务**：使用前请务必仔细阅读 Trae Work 的服务条款，并自行判断是否使用；请确保仅用于管理本人合法持有的账号，遵守所在地法律法规。
5. **作者免责**：本工具作者对任何因使用、误用或滥用本工具而引发的纠纷、争议或问题不承担任何责任。
6. **侵权处理**：若您是 Trae Work 官方且认为本工具侵犯了您的合法权益，请通过项目渠道联系作者，我们将在核实后及时下架处理。

**使用本工具即表示你已阅读、理解并同意上述全部免责声明。**

## 功能

- **账号管理**：多账号 JWT 录入/编辑/查看、分组管理、设备 ID 隔离
- **一键签到**：批量签到、按分组/手动勾选、跳过已签/过期、实时进度
- **登录态切换**：PowerShell 桥接，关闭 -> 备份 -> 恢复 -> 带代理重启
- **积分看板**：排行、趋势图、今日新增统计
- **本地代理**：MITM 代理自动捕获 JWT、注入独立设备 ID
- **定时任务**：Windows 计划任务，后台自动签到
- **数据全部本地存储**，不上传任何服务器

## 开发

```powershell
npm install
npm run tauri dev      # 开发模式
npm run tauri build    # 打包（msi + nsis）
```

前置：Node.js 18+、Rust 1.75+、Python 3.9+、WebView2 Runtime、VS Build Tools (C++)

## 数据目录

```
%APPDATA%\TraeWorkAssistant\
├── checkin_accounts.json    # 账号 + JWT
├── device_map.json          # 设备 ID 映射
├── groups.json              # 分组
├── app_settings.json        # 设置
├── credits_history.json     # 积分历史
└── logs/                    # proxy / checkin / switcher 日志
```

## License

MIT
