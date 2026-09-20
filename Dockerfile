# ─────────────────────────────────────────────────────────────
# Wild AgentOS Core — 多阶段构建
#   build 阶段：编译 Rust release 二进制（需 protoc + C 工具链给 tree-sitter）
#   runtime 阶段：仅带二进制 + 默认 config.yaml，数据落 /app/data
#
# MIRROR: Docker Hub 镜像仓库前缀(带尾部/)。默认 docker.io/。
#   国内/受限网络: --build-arg MIRROR=docker.m.daocloud.io/
# ─────────────────────────────────────────────────────────────
ARG MIRROR=docker.io/
# 用最新稳定 Rust:部分依赖(sysinfo/time 等)要求 rustc >= 1.88
FROM ${MIRROR}library/rust:1-slim-bookworm AS builder

# tonic-build 需 protobuf-compiler；tree-sitter/oxigraph(RocksDB) 需 C/C++ 工具链(gcc/g++)
# 保留官方 Debian 源，避免 CI 依赖特定第三方镜像的可用性。
RUN set -eux; \
    echo 'Acquire::Retries "3";' > /etc/apt/apt.conf.d/80-retries; \
    apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        build-essential \
        cmake \
        pkg-config \
        libssl-dev \
        libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# 先拷贝 manifest 以利用层缓存（依赖不变时跳过重编）
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps ./apps
COPY proto ./proto
COPY build.rs ./build.rs
COPY src ./src
COPY benches ./benches

# 默认 feature（含 ontology），不含 embeddings/causal 重依赖
RUN cargo build --release --bin wild-agent-os-core \
    && strip target/release/wild-agent-os-core

# ─────────────────────────────────────────────────────────────
ARG MIRROR=docker.io/
FROM ${MIRROR}library/debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 -m -d /app agentos

WORKDIR /app

COPY --from=builder /build/target/release/wild-agent-os-core /usr/local/bin/wild-agent-os-core
COPY config.yaml /app/config.yaml

# 数据目录（PVC / volume 挂载点）
RUN mkdir -p /app/data /app/logs \
    && chown -R agentos:agentos /app

USER agentos

# 数据根：所有嵌入式存储（redb / oxigraph / 向量库）落此
ENV AGENTOS_DATA_DIR=/app/data \
    AGENT_OS_HTTP_PORT=8080 \
    AGENT_OS_API_GRPC_ADDR=0.0.0.0:50051 \
    RUST_LOG=info

# HTTP / gRPC / metrics
EXPOSE 8080 50051 9090

VOLUME ["/app/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/wild-agent-os-core"]
