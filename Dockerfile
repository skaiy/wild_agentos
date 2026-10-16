# ─────────────────────────────────────────────────────────────
# Wild AgentOS Core — 多阶段构建 / multi-stage build
#   build 阶段：rust-toolchain.toml 同版本 rustc + cargo auditable 编译 release 二进制
#   runtime 阶段：distroless（无 shell / 无 curl / 无包管理器），仅带二进制 + 默认 config.yaml，
#                 数据落 /app/data
#
# MIRROR: Docker Hub 镜像仓库前缀(带尾部/)。默认 docker.io/。
#   国内/受限网络: --build-arg MIRROR=docker.m.daocloud.io/
# DISTROLESS: distroless 镜像仓库前缀(带尾部/)。默认 gcr.io/distroless/。
# RUST_VERSION: 必须与 rust-toolchain.toml 的 channel 一致(构建时校验，不一致直接失败)。
#
# 基础镜像按 tag@sha256:<digest> 固定（digest 是多架构 index 的 digest）。tag 只作可读标注，
# 拉取以 digest 为准；MIRROR / DISTROLESS 换成镜像源时 digest 同样生效——镜像源必须原样
# 代理上游 manifest，否则拉取直接失败，不会悄悄换成别的镜像。
# 更新方式（改 RUST_VERSION 或定期刷新安全补丁时）：
#   docker buildx imagetools inspect rust:<RUST_VERSION>-slim-bookworm      # 取 Digest
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian12:nonroot  # 取 Digest
# 把结果写进下面的 RUST_IMAGE_DIGEST / DISTROLESS_DIGEST（或用 crane digest <image>）。
# ─────────────────────────────────────────────────────────────
ARG MIRROR=docker.io/
ARG DISTROLESS=gcr.io/distroless/
ARG RUST_VERSION=1.90.0
ARG RUST_IMAGE_DIGEST=sha256:64232e656c058f4468e8d024e990acff04f0fd5a5c0a88a574dc37773d7325c9
ARG DISTROLESS_DIGEST=sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
FROM ${MIRROR}library/rust:${RUST_VERSION}-slim-bookworm@${RUST_IMAGE_DIGEST} AS builder

# tonic-build 需 protobuf-compiler；tree-sitter/oxigraph(RocksDB) 需 C/C++ 工具链(gcc/g++)。
# reqwest 只用 rustls，不再需要 libssl-dev / pkg-config。
# 保留官方 Debian 源，避免 CI 依赖特定第三方镜像的可用性。
RUN set -eux; \
    echo 'Acquire::Retries "3";' > /etc/apt/apt.conf.d/80-retries; \
    apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        build-essential \
        cmake \
        libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# 依赖清单内嵌到二进制（.dep-v0 段），上线后可用 `cargo audit bin` 直接扫描镜像里的二进制。
ARG CARGO_AUDITABLE_VERSION=0.7.7
RUN cargo install cargo-auditable --locked --version "${CARGO_AUDITABLE_VERSION}"

WORKDIR /build

# 工具链一致性：镜像里的 rustc 必须等于 rust-toolchain.toml 指定的版本。
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN set -eux; \
    want="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' /tmp/rust-toolchain.toml)"; \
    have="$(rustc --version | cut -d' ' -f2)"; \
    echo "rust-toolchain.toml channel=${want} builder rustc=${have}"; \
    test -n "${want}" && test "${want}" = "${have}"

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps ./apps
COPY proto ./proto
COPY build.rs ./build.rs
COPY src ./src
COPY benches ./benches

# 默认 feature（含 ontology），不含 embeddings/causal 重依赖
RUN cargo auditable build --locked --release --bin wild-agent-os-core \
    && strip target/release/wild-agent-os-core

# 运行镜像没有 shell，不能 RUN mkdir；在这里建好数据/日志目录，带属主一起 COPY。
# 沿用 uid/gid 10001：已有数据卷的属主不变，升级无需 chown。
RUN mkdir -p /out/app/data /out/app/logs

# ─────────────────────────────────────────────────────────────
FROM ${DISTROLESS}cc-debian12:nonroot@${DISTROLESS_DIGEST} AS runtime

COPY --from=builder --chown=10001:10001 /out/app /app
COPY --from=builder /build/target/release/wild-agent-os-core /usr/local/bin/wild-agent-os-core
COPY --chown=10001:10001 config.yaml /app/config.yaml

WORKDIR /app

USER 10001:10001

# 数据根：所有嵌入式存储（redb / oxigraph / 向量库）落此
ENV HOME=/app \
    AGENTOS_DATA_DIR=/app/data \
    AGENT_OS_HTTP_PORT=8080 \
    AGENT_OS_API_GRPC_ADDR=0.0.0.0:50051 \
    RUST_LOG=info

# HTTP / gRPC / metrics
EXPOSE 8080 50051 9090

VOLUME ["/app/data"]

# 不依赖 shell/curl：二进制自带 healthcheck 子命令（只请求本机 /health，超时 3s，退出码 0/1）。
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/wild-agent-os-core", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/wild-agent-os-core"]
