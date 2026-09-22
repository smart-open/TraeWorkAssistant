# 🚀 Web 版服务端部署指南

> 单进程 axum 服务（`aiwork-server`）：管理面 REST + OpenAI/Anthropic 兼容网关 + 7 项定时任务调度 + 三渠道通知推送（Bark/Server酱/Webhook，应用内配置）+ React 静态托管，浏览器任意设备直访。

## 📋 快速开始

前置：Docker 20.10+ 与 Docker Compose v2。

```bash
# 1. 构建并启动（首次构建约 10-20 分钟，Rust 编译占大头）
docker compose up -d --build

# 2. 查看健康状态（HEALTHCHECK 30s 间隔，等待变 healthy）
docker compose ps

# 3. 获取管理令牌（未注入 AIWORK_ADMIN_TOKEN 时自动生成）
docker compose exec aiwork-server cat /app/data/conf/admin_token

# 4. 浏览器访问 http://<服务器IP>:8080 ，粘贴令牌登录
```

宿主端口自定义：项目根目录建 `.env` 写入 `AIWORK_PORT=9090` 后重建容器。

## 🔧 配置项（环境变量）

| 变量 | 默认值 | 说明 |
|---|---|---|
| `AIWORK_LISTEN_ADDR` | `0.0.0.0:8080` | 监听地址；改端口时需同步 `AIWORK_PORT` 与 compose 端口映射 |
| `AIWORK_PORT` | `8080` | 仅供容器 HEALTHCHECK 探活取端口 |
| `AIWORK_ADMIN_TOKEN` | 空（自动生成） | 管理面令牌；留空则首启生成 64 位随机 hex 写 `conf/admin_token` |
| `AIWORK_VAULT_KEY` | 空（自动生成） | 敏感数据加密密钥（任意字符串 SHA-256 归一 32B）；**容器重建必须可复现**，否则已存凭据不可解密 |
| `AIWORK_DATA_DIR` | `/app/data` | 数据目录（容器内固定挂载 volume） |
| `AIWORK_WEB_DIST` | `/app/dist` | 前端静态资源目录（镜像内置） |
| `TZ` | `Asia/Shanghai` | 时区（影响签到/调度的时间判定） |

## 💾 数据持久化（volume `./data:/app/data`）

| 子目录 | 内容 | 备份要点 |
|---|---|---|
| `data/` | SQLite 库（账号/分组/签到记录/积分快照/用量统计） | 常规备份 |
| `conf/` | `admin_token`、`vault_key.bin` | **必须备份**：`vault_key.bin` 丢失 = 全部账号凭据不可解密 |
| `logs/` | 运行日志（按保留天数自动清理） | 可不备份 |

升级流程：`git pull && docker compose up -d --build`（数据在 volume，升级不丢）。

## 🔑 OAuth 账号录入（无桌面参与）

### Trae（粘贴回调模式）

1. 账号管理页 → OAuth 登录 → 复制登录链接，在**任意设备**浏览器打开并完成 Trae 登录；
2. 登录成功后浏览器跳转 `http://127.0.0.1:17388/authorize?...` 并停在「无法连接」页——**这是预期现象**；
3. 复制浏览器地址栏完整 URL，粘贴回 Web UI 提交，账号自动入池。

### WorkBuddy（authUrl 轮询）

Buddy 账号页发起 OAuth 后按提示在浏览器完成授权，服务端自动轮询收取 token（≤300s），无需粘贴。

## 📦 桌面版数据迁移

1. 桌面版 → 账号管理 → 导出 JSON（含明文 JWT/refresh_token）；
2. Web 版 → 账号管理 → 导入（按 uid 去重、分组合并；导入前可预览）；
3. WorkBuddy 账号同理（Buddy 账号页导入导出）。

> ⚠️ 桌面版 vault 快照（DPAPI 加密）**不可迁移**——凭据经导出文件以明文中转，导出后请及时删除该文件。

## 🛡️ 安全边界

- **管理面** `/api/*`：主令牌 + 附加管理员令牌（可签发/吊销，最多 20 个）并集校验 → HttpOnly Cookie 会话（7 天有效期），未登录一律 401；
- **网关** `/v1/*`：api_keys 鉴权 fail-closed（未配置 key 即全拒）；
- **`/health`**：公开探活端点，无敏感信息；
- 出网：服务端以合成指纹直连 `api.trae.cn` / `trae-api-cn.mchost.guru` / `copilot.tencent.com`，**不读系统代理**；
- 建议加固（公网部署必做）：反向代理加 TLS（见下节）、管理面 IP 允许列表（反代层或应用层内置，见下节）、`AIWORK_ADMIN_TOKEN` 显式注入强随机值、禁用 8080 直接对公网暴露（compose 端口映射改 `127.0.0.1:8080:8080` 仅本机反代可达）。

### 🔒 TLS 反向代理示例

Caddy（自动 HTTPS，最省事）：

```caddy
# Caddyfile
aiwork.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Nginx（含 SSE/WebSocket 关键配置：WS 升级头 + 关闭缓冲 + 长超时，否则实时进度推送不可用）：

```nginx
server {
    listen 443 ssl;
    server_name aiwork.example.com;
    # ssl_certificate / ssl_certificate_key 按需配置（或用 certbot 自动签）

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        # WebSocket 必需：/api/ws 实时事件通道（缺升级头时前端自动回退 SSE，建议显式开启）
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        # SSE 必需：/api/events/* 禁缓冲、不超时断流
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }
}
```

### 🚫 管理面 IP 允许列表（反代层）

Nginx 在 `location /` 内加：

```nginx
    allow 203.0.113.0/24;   # 办公网段
    allow 198.51.100.7;     # 个人固定 IP
    deny  all;
```

Caddy 用 `@blocked not remote_ip ...` + `respond @blocked 403` 等价实现；云服务器也可直接在安全组限制 443 来源 IP。

### 🛡️ 管理面 IP 允许列表（应用层，服务端内置）

除反代层外，服务端内置应用层 IP 允许列表（设置页「IP 允许列表」卡，kv 热生效）：

- **CIDR 白名单**：每行一条（如 `203.0.113.0/24`、`198.51.100.7`），支持 IPv4/IPv6；保存即时生效，含无效条目整体拒绝不落库；
- **trust_proxy**：经反向代理部署时**必须勾选**，此时取 `X-Real-IP`（或 `X-Forwarded-For` 首项）判定真实来源——上文 Nginx 示例已配置该头；不勾选则取 TCP 对端地址（即反代自身 IP，会导致全放行或全拒绝）；
- **回环豁免**：`127.0.0.1` / `::1` 始终放行，避免误配置锁死本机运维；`/health` 探活不受限；
- 未启用或列表为空时不拦截；取不到来源 IP 时 fail-closed 拒绝（403）。

反代层与应用层可叠加：反代层挡流量，应用层兜底直连 8080 的旁路访问。

## 💾 备份与恢复

```bash
# 备份（停机一致性最好，运行中备份 SQLite 建议用 sqlite3 .backup）
tar czf aiwork-backup-$(date +%F).tgz data/

# 恢复：解压回原路径后 docker compose up -d
# 关键文件：data/*.db（业务库）、conf/vault_key.bin（加密密钥）、conf/admin_token
```

> ⚠️ `vault_key.bin` 丢失 = 已存账号凭据永久不可解密（无找回手段）。跨机迁移要么携带该文件，要么两侧固定相同 `AIWORK_VAULT_KEY`。

## 🩺 常见问题

| 现象 | 处置 |
|---|---|
| 容器 restart 循环 | `docker compose logs aiwork-server` 看启动错误；多为 `AIWORK_VAULT_KEY` 与已有 `vault_key.bin` 不一致 |
| 登录提示 token 无效 | 令牌取自 `conf/admin_token` 文件或 env 注入值；env 变更后需 `docker compose up -d` 重建；附加管理员令牌（设置页签发）同样可登录，吊销后立即失效 |
| 签到时间不对 | 确认 `TZ=Asia/Shanghai` 未被覆盖 |
| 手动签到无进度 | 实时通道走 WebSocket `/api/ws`（连接失败前端自动回退 SSE `/api/events/checkin`）；Nginx 需带 Upgrade 升级头并对 SSE `proxy_buffering off`（见上节示例） |
| 定时任务没跑 | 任务由服务端内置调度器托管（05:30 Trae JWT 续期 / 05:40 模型列表同步 / 09:00 Trae 签到 / 09:10 WB 签到 / 10:30 WB 续期 / 23:30、23:40 积分快照），重启后自动补跑当天错过的任务；确认环境配置页对应任务开关未停用 |
