# ============================================================
# Stage 1: 编译 Rust 二进制（多平台）
# ============================================================
FROM --platform=$BUILDPLATFORM rust:1-slim-bookworm AS builder

ARG TARGETPLATFORM
ARG BUILDPLATFORM
ARG TARGETARCH

RUN apt-get update -qq && apt-get install -y -qq protobuf-compiler pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY benches/ ./benches/
COPY sdk/rust/ ./sdk/rust/

# 交叉编译目标（跨平台构建时添加目标架构）
RUN if [ "$TARGETARCH" = "arm64" ]; then rustup target add aarch64-unknown-linux-gnu; fi

# 全量编译（BUILDPLATFORM 上编译，产物对应 TARGETARCH）
RUN cargo build --release -p clouisle-api -p clouislectl -p clouisle-agent

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
COPY --from=builder /build/target/release/clouisle-agent /usr/local/bin/

# 创建 firecracker / jailer 符号链接（解压后文件名带版本号）
RUN ln -sf firecracker-v1.10.1-x86_64 /usr/local/bin/firecracker \
    && ln -sf jailer-v1.10.1-x86_64 /usr/local/bin/jailer

# 健康检查
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -sf http://localhost:8080/health || exit 1

# 数据目录
VOLUME ["/data"]
EXPOSE 8080

# 默认启动（SQLite 单机模式）
# 生产环境建议：--db "postgres://user:pass@host:5432/clouisle"
ENTRYPOINT ["clouisle-api"]
CMD ["--addr", "0.0.0.0:8080", "--db", "/data/clouisle.db"]