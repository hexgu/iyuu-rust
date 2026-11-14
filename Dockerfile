# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# iyuu-reseed — 多阶段构建
#
#   1. web      前端产物（bun + vite）
#   2. planner  把依赖清单抽成 recipe.json（cargo-chef）
#   3. builder  先只编依赖，再编我们的代码
#   4. runtime  只带二进制 + 静态文件，进程以非 root 运行
#
# cargo-chef 把「编依赖」和「编我们自己的代码」拆成两层：原来 `COPY crates/` 和
# `cargo build` 在同一层，改任何一行源码都会让整层失效，~200 个 crate 全部重编译。
# 前端只要 20 秒，Rust 要几分钟（arm64 上还是 QEMU 模拟，实测 47 分钟）。
#
# 构建走官方源（crates.io / registry.npmjs.org），不预设任何镜像或代理：镜像站点和
# 代理属于个人网络环境，应该留在本机的 gitignore 文件里，而不是随仓库分发。
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Stage 1: 前端
# ---------------------------------------------------------------------------
FROM oven/bun:1-alpine AS web
WORKDIR /build/web

# 先拷清单：只改源码不会让 install 层失效。
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile

COPY web/ ./
RUN bun run build

# ---------------------------------------------------------------------------
# Stage 2: 依赖清单
# ---------------------------------------------------------------------------
FROM lukemathwalker/cargo-chef:latest-rust-1-slim AS chef
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------
# Stage 3: Rust release 构建
# ---------------------------------------------------------------------------
FROM chef AS builder
WORKDIR /build

# 只依赖清单编依赖：这一层能跨构建命中缓存，源码改动不会让它失效。
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --release --locked && \
    strip target/release/iyuu-reseed

# ---------------------------------------------------------------------------
# Stage 4: 运行时
# ---------------------------------------------------------------------------
# 刻意不用 rust:1-slim 当运行时：它 1.2 GB，而这里只需要 glibc + CA 证书 +
# 时区数据 + setpriv（util-linux，用来降权）。
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates tzdata util-linux && \
    rm -rf /var/lib/apt/lists/*

# 应用用户：固定的 uid/gid 10001，便于宿主机上给 ./data 设属主。
RUN groupadd --gid 10001 app && \
    useradd --uid 10001 --gid app --no-create-home --shell /usr/sbin/nologin app

# 容器时区影响 cron 触发与日志时间，用户可以在 compose 里覆盖。
ENV TZ=Asia/Shanghai \
    IYUU_DATA_DIR=/data \
    IYUU_WEB_DIR=/app/web \
    IYUU_LISTEN=0.0.0.0:8098
# IYUU_API_BASE 故意不设：空值看起来像覆盖，而且它是部署层面的事（镜像/反代），
# 只应在真正需要时从 compose 传入。

WORKDIR /app
COPY --link --from=builder /build/target/release/iyuu-reseed /usr/local/bin/iyuu-reseed
COPY --link --from=web /build/web/dist /app/web
COPY --link docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod 0755 /usr/local/bin/docker-entrypoint.sh && \
    mkdir -p /data && \
    chown 10001:10001 /data /app /app/web

# 数据库放这里，挂成卷：校准队列存在库里，容器重建就会丢掉所有在途辅种。
VOLUME ["/data"]
EXPOSE 8098

LABEL org.opencontainers.image.title="iyuu-reseed" \
      org.opencontainers.image.description="基于 IYUU 开放 API 的自动辅种工具（非官方）" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/hexgu/iyuu-rust"

# 镜像里没有 curl/wget；bash 的 /dev/tcp 足够做存活探测。
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8098 && printf 'GET /api/health HTTP/1.0\\r\\n\\r\\n' >&3 && head -1 <&3 | grep -q 200"]

# 入口先修 /data 属主再降权，应用进程最终以 uid 10001 运行（见 docker-entrypoint.sh）。
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["iyuu-reseed"]
