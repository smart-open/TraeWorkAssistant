# AI Work 助手 Web 版（aiwork-server）多阶段构建
# 阶段 1 node:20  → npm run build 产出 dist/
# 阶段 2 rust:1.88-slim → cargo build --release 产出 aiwork-server
#      （假工程先编译依赖：源码改动不触发全量依赖重编译）
# 阶段 3 debian:bookworm-slim → 运行级（ca-certificates/tzdata/curl + HEALTHCHECK）
# 用法：docker compose up -d --build（见 docs/server-deploy.md）

# ===== 阶段 1：前端构建 =====
FROM node:20-slim AS web
WORKDIR /build
# 先拷依赖清单装包（package-lock 锁定，层缓存稳定）
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
# 再拷源码与配置（.dockerignore 已排除 target/node_modules/dist/src-tauri 等）
COPY . .
RUN npm run build

# ===== 阶段 2：Rust 服务端构建 =====
# 1.88：Cargo.lock 内 icu_*/time/zip 等依赖 MSRV ≥1.86~1.88，1.85 会编译失败（exit 101）
FROM rust:1.88-slim AS server
WORKDIR /build
# rusqlite(bundled) 需要 cc；slim 镜像默认无 gcc
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*
# 依赖缓存层：先拷 Cargo 清单，用假工程预编译全部依赖
COPY Cargo.toml Cargo.lock ./
COPY crates/aiwork-core/Cargo.toml crates/aiwork-core/Cargo.toml
COPY crates/aiwork-server/Cargo.toml crates/aiwork-server/Cargo.toml
RUN mkdir -p crates/aiwork-core/src crates/aiwork-server/src \
    && echo 'pub fn __placeholder() {}' > crates/aiwork-core/src/lib.rs \
    && echo 'fn main() {}' > crates/aiwork-server/src/main.rs \
    && cargo build --release -p aiwork-server --bin aiwork-server \
    && rm -rf crates/aiwork-core/src crates/aiwork-server/src
# 真源码增量编译（依赖产物已在上层缓存）
COPY crates ./crates
RUN touch crates/aiwork-core/src/lib.rs crates/aiwork-server/src/main.rs \
    && cargo build --release -p aiwork-server

# ===== 阶段 3：运行级 =====
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=server /build/target/release/aiwork-server /app/aiwork-server
COPY --from=web /build/dist /app/dist
# 数据目录约定（ADR-2/T3）：SQLite/stronghold/conf/logs 全在 /app/data，volume 必须持久化
# vault 密钥丢失 = 已存账号凭据不可解密：务必固定 AIWORK_VAULT_KEY env 或备份 conf/vault_key.bin
ENV AIWORK_DATA_DIR=/app/data \
    AIWORK_WEB_DIST=/app/dist \
    AIWORK_LISTEN_ADDR=0.0.0.0:8080 \
    AIWORK_PORT=8080 \
    TZ=Asia/Shanghai
EXPOSE 8080
VOLUME ["/app/data"]
# 健康 check：AIWORK_LISTEN_ADDR 改端口时需同步调整 AIWORK_PORT
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${AIWORK_PORT}/health" || exit 1
ENTRYPOINT ["/app/aiwork-server"]
