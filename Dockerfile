# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Build notes
#
# Base images are unavoidable Docker Hub pulls; there is no CN mirror that
# reliably serves them. Everything *inside* the build is pinned to CN mirrors so
# the slow part (crates.io / npm) never touches an overseas host:
#   * cargo  -> rsproxy.cn        (see .cargo/config.toml)
#   * npm    -> registry.npmmirror.com (set below)
# Those two are fast and reliable from CN networks; Docker Hub is only needed
# once, to pull the two base images.
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Stage 1: front end
# ---------------------------------------------------------------------------
FROM oven/bun:1-alpine AS web
WORKDIR /build/web

# Point bun at the CN registry. npmmirror.com mirrors npm and is orders of
# magnitude faster from mainland China than registry.npmjs.org.
ENV NPM_CONFIG_REGISTRY=https://registry.npmmirror.com \
    BUN_CONFIG_REGISTRY=https://registry.npmmirror.com

# Copy manifests first so a source-only change does not invalidate the install.
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile

COPY web/ ./
RUN bun run build

# ---------------------------------------------------------------------------
# Stage 2: Rust build
# ---------------------------------------------------------------------------
FROM rust:1-slim AS builder
WORKDIR /build

# Nothing to apt-get: rusqlite uses `bundled` (SQLite compiled in) and reqwest
# uses rustls, so there is no need for libsqlite3-dev, pkg-config, or OpenSSL
# headers. Staying off apt keeps this stage working even when Debian mirrors are
# unreachable from the build environment.
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ ./.cargo/
COPY crates/ ./crates/

RUN cargo build --release --locked && \
    strip target/release/iyuu-reseed || true

# ---------------------------------------------------------------------------
# Stage 3: runtime
# ---------------------------------------------------------------------------
# Reuse the Rust base for the runtime stage: it is Debian-based, so the binary's
# only shared-library need (glibc) is already satisfied. It also ships CA
# certificates and tzdata, so no apt-get is required at all.
FROM rust:1-slim

# Match the host's timezone so cron schedules and log timestamps line up with
# what the user configured. Override with TZ at runtime.
ENV TZ=Asia/Shanghai \
    IYUU_DATA_DIR=/data \
    IYUU_WEB_DIR=/app/web \
    IYUU_LISTEN=0.0.0.0:8098
# IYUU_API_BASE is intentionally NOT set here. An empty value would look like an
# override, and the UI never sees this variable anyway — set it from compose only
# when you actually run a mirror or reverse proxy.

WORKDIR /app
COPY --from=builder /build/target/release/iyuu-reseed /usr/local/bin/iyuu-reseed
COPY --from=web /build/web/dist /app/web

# The database lives here; mount it as a volume so config and the verification
# queue survive a container recreation (that queue is the whole point).
VOLUME ["/data"]
EXPOSE 8098

# No curl in the image; bash's /dev/tcp is enough for a liveness probe.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/8098 && printf "GET /api/health HTTP/1.0\r\n\r\n" >&3 && head -1 <&3 | grep -q 200' || exit 1

ENTRYPOINT ["iyuu-reseed"]
