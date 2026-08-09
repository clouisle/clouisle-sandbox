# ============================================================
# Stage 1: 编译 Rust 二进制（多平台）
# ============================================================
FROM --platform=$TARGETPLATFORM rust:1-slim-bookworm AS builder

ARG TARGETPLATFORM
ARG BUILDPLATFORM
ARG TARGETARCH

RUN apt-get update -qq && apt-get install -y -qq protobuf-compiler pkg-config libssl-dev musl-tools && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY benches/ ./benches/
COPY sdk/rust/ ./sdk/rust/

# API/CLI 在运行镜像中执行；guest agent 必须静态链接，兼容 Ubuntu 18.04 rootfs。
# Builder 与运行目标同架构，使 musl-gcc 可为 amd64 和 arm64 生成本机静态二进制。
RUN cargo build --release -p clouisle-api -p clouislectl -p clouisled \
    && case "$TARGETARCH" in \
        amd64) target=x86_64-unknown-linux-musl ;; \
        arm64) target=aarch64-unknown-linux-musl ;; \
        *) echo "unsupported target architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && rustup target add "$target" \
    && env "CARGO_TARGET_$(printf '%s' "$target" | tr '[:lower:]-' '[:upper:]_')_LINKER=musl-gcc" \
       RUSTFLAGS="-C relocation-model=static" \
       cargo build --release -p clouisle-agent --target "$target" \
    && cp "target/$target/release/clouisle-agent" /build/clouisle-agent-guest

# ============================================================
# Stage 2: 运行镜像（多平台）
#   根据 TARGETARCH 下载对应架构的 Firecracker
# ============================================================
FROM --platform=$TARGETPLATFORM debian:bookworm-slim

ARG TARGETARCH

# 安装 Firecracker（按架构）+ 运行时依赖
RUN apt-get update -qq && apt-get install -y -qq \
    curl ca-certificates iproute2 nftables iptables procps \
    && rm -rf /var/lib/apt/lists/* \
    && if [ "$TARGETARCH" = "arm64" ]; then \
        FC_ARCH=aarch64; \
    elif [ "$TARGETARCH" = "amd64" ]; then \
        FC_ARCH=x86_64; \
    else \
        echo "Unsupported arch: $TARGETARCH" && exit 1; \
    fi \
    && curl -sL "https://github.com/firecracker-microvm/firecracker/releases/download/v1.10.1/firecracker-v1.10.1-${FC_ARCH}.tgz" \
    | tar xz -C /usr/local/bin/ --strip-components=1

# 复制编译产物
COPY --from=builder /build/target/release/clouisle-api /usr/local/bin/
COPY --from=builder /build/target/release/clouislectl /usr/local/bin/
COPY --from=builder /build/target/release/clouisled /usr/local/bin/
COPY --from=builder /build/clouisle-agent-guest /usr/local/bin/clouisle-agent

# 创建 firecracker / jailer 符号链接（解压后文件名带版本号）
RUN if [ "$TARGETARCH" = "arm64" ]; then FC_ARCH=aarch64; else FC_ARCH=x86_64; fi \
    && ln -sf "firecracker-v1.10.1-${FC_ARCH}" /usr/local/bin/firecracker \
    && ln -sf "jailer-v1.10.1-${FC_ARCH}" /usr/local/bin/jailer

# 健康检查
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -sf http://localhost:8080/health || exit 1

# 数据目录
VOLUME ["/data"]
EXPOSE 8080

# The image contains both control-plane and node-daemon binaries. Deployment
# manifests select the process explicitly with `command`.

# 默认启动（SQLite 单机模式）
# 生产环境建议：--db "postgres://user:pass@host:5432/clouisle"
ENTRYPOINT ["clouisle-api"]
CMD ["--addr", "0.0.0.0:8080", "--db", "/data/clouisle.db"]