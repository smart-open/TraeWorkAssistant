# AI Work 助手（AI Work Assistant）

浏览器直访的多账号签到与 AI 资源调度一站式工作台 · React 18 + Rust axum 单体 + Docker 一键部署

**GitHub**：[github.com/smart-open](https://github.com/smart-open) · **个人博客**：[blog.sopenai.cn](https://blog.sopenai.cn/)

> 深度支持 **Trae / WorkBuddy（CodeBuddy）** 双账号体系：多账号 OAuth/凭证管理、每日自动签到、积分看板、JWT 自动续期，内嵌 **OpenAI / Anthropic / Codex Responses 三协议兼容 API 网关**（Trae 池 + WorkBuddy 池 + 自定义模型三池调度、会话粘性、ck\_ 子 Key）。单进程 axum 服务承载管理面 + 网关 + 调度器，浏览器任意设备直访，Docker 部署。
>
> ⚠️ 本工具与 Trae / WorkBuddy 等官方均无任何关联，仅供学习研究。使用本工具可能违反相关服务条款，风险自担。请仅管理本人合法持有的账号。

## 功能

- **账号管理**：OAuth 登录（Trae 粘贴回调 / WorkBuddy 扫码轮询）与手动凭证录入、分组管理、JWT 解析与自动续期（临期 48h 懒刷新）、账号导入导出（导入前预览）
- **一键签到**：批量签到、按分组/手动勾选、跳过已签/过期、失败自动重试（30s/90s 两轮）、近 N 天趋势图；WorkBuddy 成长中心自动化（旅行/盲盒/任务领奖）
- **积分看板**：Trae 积分排行与三线趋势、每日快照自动采集；WorkBuddy 积分三件套 + 官方请求用量 + 本地 Token 统计（缓存命中率/热力图）
- **API 网关**：内嵌 OpenAI / Anthropic / Codex Responses 三协议兼容 API 服务——Trae 账号池 + WorkBuddy 池 + 自定义 OpenAI 兼容模型三池调度（smart 智能策略/优先级/模型级覆盖）、会话粘性、ck\_ 子 Key 配额、四段模型路由、审核指纹清洗、生图双端点、web\_search 工具代执行
- **内置调度器**：单后台线程每日 7 任务自动执行——JWT 续期（05:30）、模型列表同步（05:40）、双端签到（09:00/09:10）、WB token 兜底续期（10:30）、双端积分快照（23:30/23:40）；任务可在环境配置页开关，失败 30 分钟冷却自动重试
- **通知渠道**：Bark / Server酱 / 通用 Webhook 三渠道推送（签到完成、任务失败），设置页可发测试通知
- **数据全部本地存储**（SQLite，WAL 模式），出网仅 Trae / 腾讯上游，不上传任何服务器

## 快速开始

前置：Docker 20.10+ 与 Docker Compose v2。

### 方式一：GHCR 镜像启动（推荐，免编译）

```bash
# 1. 拉取镜像（版本号随发布更新，也可用 latest / docker_main / sha-xxxxxxx）
docker pull ghcr.io/smart-open/traeworkassistant:1.0.0

# 2. 启动（数据持久化在 ./data；TZ 默认 Asia/Shanghai 已内置镜像）
#    注意：docker run 的 -v 源路径必须为绝对路径（Linux/macOS 用 $PWD，Windows PowerShell 用 ${PWD}）
mkdir -p data
docker run -d --name aiwork-server \
  -p 8080:8080 \
  -v "$PWD/data:/app/data" \
  --restart unless-stopped \
  ghcr.io/smart-open/traeworkassistant:1.0.0

# 3. 等待 healthy 后获取管理令牌（未注入 AIWORK_ADMIN_TOKEN 时自动生成）
docker exec aiwork-server cat /app/data/conf/admin_token

# 4. 浏览器访问 http://<服务器IP>:8080 ，粘贴令牌登录
```

**镜像 Tag 说明**：`1.0.0` 版本号（随每次发布更新）· `latest`（main 分支最新）· `docker_main`（开发线最新）· `sha-xxxxxxx`（提交快照，用于锁定版本回溯）。仅构建 `linux/amd64` 架构。

**支持的环境变量**（均可通过 `docker run -e` 或 compose `environment` 注入）：

| 环境变量                 | 默认值             | 说明                                                               |
| -------------------- | --------------- | ---------------------------------------------------------------- |
| `AIWORK_LISTEN_ADDR` | `0.0.0.0:8080`  | 服务监听地址                                                           |
| `AIWORK_ADMIN_TOKEN` | 空（自动生成）         | 管理面登录令牌；留空则首启生成 64 位随机 hex 写 `conf/admin_token`；**生产建议显式注入强随机值** |
| `AIWORK_VAULT_KEY`   | 空（自动生成）         | 敏感数据加密密钥（任意字符串 SHA-256 归一 32B）；**容器重建必须可复现**，否则已存凭据不可解密          |
| `AIWORK_DATA_DIR`    | `/app/data`     | 数据目录（容器内固定挂载 volume）                                             |
| `AIWORK_PORT`        | `8080`          | 仅供容器 HEALTHCHECK 探活取端口                                           |
| `TZ`                 | `Asia/Shanghai` | 调度任务按本地时刻触发（签到/续期/快照）                                            |

### 方式二：源码构建

```bash
# 首次构建约 10-20 分钟，Rust 编译占大头
docker compose up -d --build

# 获取管理令牌
docker compose exec aiwork-server cat /app/data/conf/admin_token
```

宿主端口自定义：项目根目录建 `.env` 写入 `AIWORK_PORT=9090` 后重建容器。端口自定义、TLS 反向代理、IP 允许列表、备份与恢复等详见 **[docs/server-deploy.md](docs/server-deploy.md)**。

> ⚠️ 数据持久化在 volume `./data:/app/data`，其中 `conf/vault_key.bin` 是账号凭据加密密钥——**丢失即全部凭据不可解密**，务必纳入备份。

## 开发

```bash
npm install && npm run dev   # 前端（vite 代理 /api /v1 → 127.0.0.1:7864）
cargo run -p aiwork-server   # 后端（监听 127.0.0.1:7864；AIWORK_LISTEN_ADDR 可配）
cargo test --workspace       # Rust 单测
npm run test                 # 前端单测
```

前置：Node.js 18+、Rust 1.88+。

## 从桌面版迁移 / 老版本说明

- **Web 版（本仓库 main 分支，v1.0.0+）**：桌面壳（Tauri/托盘/登录态切换/豆包/MITM 代理/设备标识重置/计划任务）已整体退役，改为服务器常驻 + 浏览器访问。
- **桌面版数据迁移**：桌面版 → 账号管理 → 导出 JSON → Web 版 → 账号管理 → 导入（WorkBuddy 账号同理）；桌面版 vault 快照（DPAPI 加密）不可迁移，详见 [server-deploy.md](docs/server-deploy.md) 迁移章节。
- **原「Trae Work 助手」产品（2.x）**：在 `trae_work_main` 分支维护，仅支持 Trae Work 单应用，仅必要修复。

## 文档

- [服务端部署指南](docs/server-deploy.md) — Docker 部署 / 环境变量 / TLS 反代 / 数据迁移 / 安全加固
- [用户手册](docs/user-manual.md) — 功能页面操作指南：账号 / 签到 / 积分 / 网关 / 调度 / 通知 / FAQ
- [项目速查（AGENT.md）](AGENT.md) — 面向人与 AI 的秒接上下文手册：架构 / 契约 / 红线
- [产品设计](docs/product-design.md) — 产品定位 / 功能清单 / 交互流程 / 非功能需求
- [技术架构设计](docs/tech-framework.md) — 架构分层 / 数据模型 / 调度器 / 网关机制 + Trae/WorkBuddy 协议参考（抓包实证）
- [产品优化需求清单](docs/backlog.md) — 待办依据

## 赞赏

如果这个项目对你有帮助，欢迎请作者喝杯快乐水 ☕

<div align="center">
<img src="src/assets/donate-qr.jpg" alt="赞赏码" width="220" />
</div>

## License

本项目采用 [MIT License](LICENSE)，版权归 **朱天伟**（Copyright © 2026 朱天伟）所有。

Fork / 二次开发请保留 `LICENSE` 及版权声明；引用或借鉴请注明原作者及原始仓库 `https://github.com/smart-open/TraeWorkAssistant`，派生项目须说明以原库为基础，原库版权与出处不变。
