# 31. Container image

> *A Chinese version is available in [31-container-image.zh.md](31-container-image.zh.md).*

The `Dockerfile` builds the Core server (`wild-agent-os-core`) in two stages.

## Build stage

- Base image `rust:${RUST_VERSION}-slim-bookworm`. `RUST_VERSION` must equal the
  `channel` in `rust-toolchain.toml`; the build fails otherwise, and CI compares
  both the Dockerfile argument and the `rustc --version` inside the builder.
- The binary is built with `cargo auditable build --release`, which embeds the
  dependency list in the binary. Scan a deployed binary directly with
  `cargo audit bin <binary>` or print the list with `rust-audit-info <binary>`.
- HTTP clients use rustls only; the binary does not link OpenSSL.
- `cargo auditable build --locked`: the build fails instead of re-resolving
  dependencies when `Cargo.lock` is out of date.
- Base images are pinned as `tag@sha256:<digest>` (`RUST_IMAGE_DIGEST`,
  `DISTROLESS_DIGEST`); the digest wins, also behind a registry mirror
  (`MIRROR` / `DISTROLESS`), which must serve the upstream manifest unchanged.
  Refresh with `docker buildx imagetools inspect <image:tag>` (or
  `crane digest`) whenever `RUST_VERSION` changes or to pick up base-image
  security updates.
- `.dockerignore` keeps `.env*` (except `.env.example`), `*.pem`, `*.key` and
  similar credential files out of the build context.

## Runtime stage

- Base image `gcr.io/distroless/cc-debian12:nonroot`: no shell, no curl, no
  package manager. CI fails if any of them appear in the image.
- Contents: the binary at `/usr/local/bin/wild-agent-os-core` and the default
  `/app/config.yaml`. `/app/data` (volume) and `/app/logs` are created at build
  time and owned by uid/gid `10001`.
- The process runs as `10001:10001`, the same uid as earlier images, so existing
  data volumes keep working without a `chown`.

## Health check

There is no curl in the image. The binary has a `healthcheck` subcommand:

```
/usr/local/bin/wild-agent-os-core healthcheck
```

It requests `GET http://127.0.0.1:${AGENT_OS_HTTP_PORT:-8080}/health` with a
3-second timeout, never follows redirects, and exits `0` on HTTP 200, `1` otherwise (non-200, connection
failure, timeout). It reads no secrets and prints no environment values. The
`Dockerfile` `HEALTHCHECK` and `docker-compose.yml` use it in exec form.
Kubernetes keeps its `httpGet` probes (`deploy/k8s/deployment.yaml`).

## Upgrading an existing deployment

- Compose: pull the new `docker-compose.yml` (its `healthcheck.test` no longer
  calls curl) together with the new image. An old compose file that still runs
  `curl` reports the new container as unhealthy.
- Debugging: `docker exec ... sh` no longer works. Use `docker logs`, the
  `/health` and `/metrics` endpoints, or attach a debug container that shares
  the process namespace.
