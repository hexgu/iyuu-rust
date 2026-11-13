# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# iyuu-reseed — 多阶段构建
#
#   1. web      前端产物（bun + vite）
#   2. builder  Rust release 二进制
#   3. runtime  只带二进制 + 静态文件
#
# 两个与网络有关的开关，默认值面向中国大陆，海外（含 GitHub Actions）可覆盖：
#   --build-arg CARGO_SOURCE=global   # 用 crates.io，不用 rsproxy 镜像
#   --build-arg NPM_REGISTRY=https://registry.npmjs.org
#
# 国内构建直接 `docker build .` 即可，无需额外配置。
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Stage 1: 前端
# ---------------------------------------------------------------------------
FROM oven/bun:1-alpine AS web
WORKDIR /build/web

# npmmirror 是 npm 的国内全量镜像，比 registry.npmjs.org 快一到两个数量级。
ARG NPM_REGISTRY=https://registry.npmmirror.com
ENV NPM_CONFIG_REGISTRY=${NPM_REGISTRY} \
    BUN_CONFIG_REGISTRY=${NPM_REGISTRY}

# 先拷清单：只改源码不会让 install 层失效。
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile

COPY web/ ./
RUN bun run build

# ---------------------------------------------------------------------------
# Stage 2: Rust release 构建
# ---------------------------------------------------------------------------
FROM rust:1-slim AS builder
WORKDIR /build

# rusqlite 用 `bundled`（SQLite 编进二进制）、reqwest 用 rustls，所以不需要
# libsqlite3-dev / pkg-config / OpenSSL 头文件，这一层完全不碰 apt。
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ ./.cargo/
COPY crates/ ./crates/

# `.cargo/config.toml` 把 crates.io 指向 rsproxy.cn。CI 和海外用户走 crates.io
# 反而更快，所以提供开关；删掉配置文件即可回落到官方源。
ARG CARGO_SOURCE=cn
RUN if [ "$CARGO_SOURCE" != "cn" ]; then rm -f .cargo/config.toml; fi

RUN cargo build --release --locked && \
    strip target/release/iyuu-reseed

# ---------------------------------------------------------------------------
# Stage 3: 运行时
# ---------------------------------------------------------------------------
# 刻意不用 rust:1-slim 当运行时：它 1.2 GB，而这里只需要 glibc + CA 证书 +
# 时区数据。debian bookworm-slim 加两个包约 90 MB。
FROM debian:bookworm-slim

# 国内拉 apt 包慢时覆盖，例如：
#   --build-arg DEBIAN_MIRROR=mirrors.tuna.tsinghua.edu.cn
ARG DEBIAN_MIRROR=deb.debian.org
RUN if [ "$DEBIAN_MIRROR" != "deb.debian.org" ]; then \
        sed -i "s|deb.debian.org|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources; \
    fi && \
    apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates tzdata && \
    rm -rf /var/lib/apt/lists/*

# 容器时区影响 cron 触发与日志时间，用户可以在 compose 里覆盖。
ENV TZ=Asia/Shanghai \
    IYUU_DATA_DIR=/data \
    IYUU_WEB_DIR=/app/web \
    IYUU_LISTEN=0.0.0.0:8098
# IYUU_API_BASE 故意不设：空值看起来像覆盖，而且它是部署层面的事（镜像/反代），
# 只应在真正需要时从 compose 传入。

WORKDIR /app
COPY --from=builder /build/target/release/iyuu-reseed /usr/local/bin/iyuu-reseed
COPY --from=web /build/web/dist /app/web

# 数据库放这里，挂成卷：校准队列存在库里，容器重建就会丢掉所有在途辅种。
VOLUME ["/data"]
EXPOSE 8098

LABEL org.opencontainers.image.title="iyuu-reseed" \
      org.opencontainers.image.description="基于 IYUU 开放 API 的自动辅种工具（非官方）" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/hexgu/iyuu-rust"

# 镜像里没有 curl/wget；bash 的 /dev/tcp 足够做存活探测。
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/8098 && printf "GET /api/health HTTP/1.0\r\n\r\n" >&3 && head -1 <&3 | grep -q 200' || exit 1

ENTRYPOINT ["iyuu-reseed"]
