# 31. 容器镜像

> *English version: [31-container-image.md](31-container-image.md).*

`Dockerfile` 分两个阶段构建 Core 服务（`wild-agent-os-core`）。

## 构建阶段

- 基础镜像 `rust:${RUST_VERSION}-slim-bookworm`。`RUST_VERSION` 必须等于
  `rust-toolchain.toml` 的 `channel`，否则构建直接失败；CI 同时比对 Dockerfile 参数和
  builder 内 `rustc --version`。
- 用 `cargo auditable build --release` 构建，依赖清单内嵌在二进制里。上线后可直接用
  `cargo audit bin <二进制>` 扫描，或用 `rust-audit-info <二进制>` 打印清单。
- HTTP 客户端只用 rustls，二进制不链接 OpenSSL。
- `cargo auditable build --locked`：`Cargo.lock` 过期时构建直接失败，不会重新解析依赖。
- 基础镜像按 `tag@sha256:<digest>` 固定（`RUST_IMAGE_DIGEST`、`DISTROLESS_DIGEST`）；
  以 digest 为准，走镜像源（`MIRROR` / `DISTROLESS`）时同样生效，镜像源必须原样代理上游
  manifest。改 `RUST_VERSION` 或要拿基础镜像安全更新时，用
  `docker buildx imagetools inspect <image:tag>`（或 `crane digest`）刷新。
- `.dockerignore` 把 `.env*`（`.env.example` 除外）、`*.pem`、`*.key` 等凭据文件挡在构建上下文之外。

## 运行阶段

- 基础镜像 `gcr.io/distroless/cc-debian12:nonroot`：没有 shell、curl、包管理器；
  CI 发现其中任何一个就失败。
- 内容：`/usr/local/bin/wild-agent-os-core` 二进制和默认 `/app/config.yaml`。
  `/app/data`（数据卷）与 `/app/logs` 在构建阶段建好，属主 uid/gid `10001`。
- 进程以 `10001:10001` 运行，与旧镜像相同，已有数据卷无需 `chown`。

## 健康检查

镜像里没有 curl。二进制自带 `healthcheck` 子命令：

```
/usr/local/bin/wild-agent-os-core healthcheck
```

它请求 `GET http://127.0.0.1:${AGENT_OS_HTTP_PORT:-8080}/health`，超时 3 秒，不跟随重定向；
HTTP 200 退出 `0`，其它情况（非 200、连接失败、超时）退出 `1`。不读取任何密钥，
不输出环境变量值。`Dockerfile` 的 `HEALTHCHECK` 与 `docker-compose.yml` 以 exec 形式
调用它。Kubernetes 继续用 `httpGet` 探针（`deploy/k8s/deployment.yaml`）。

## 升级已有部署

- Compose：新镜像要配合新的 `docker-compose.yml`（`healthcheck.test` 不再调用 curl）。
  旧 compose 文件仍调用 `curl`，会把新容器判为 unhealthy。
- 排障：`docker exec ... sh` 不再可用。请用 `docker logs`、`/health`、`/metrics`，
  或挂一个共享进程命名空间的调试容器。
