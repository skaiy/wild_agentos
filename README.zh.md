# 如野智能体操作系统 Wild AgentOS
<div align="center">

<img src="assets/logo_transparent.png" width="120" alt="Wild AgentOS Logo" />

**面向受治理多智能体系统的 Rust 语义内核 AgentOS**

[![Star on GitHub](https://img.shields.io/github/stars/skaiy/wild_agentos?style=flat)](https://github.com/skaiy/wild_agentos)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![gRPC](https://img.shields.io/badge/gRPC-Protocol-green.svg)](https://grpc.io/)
[![Knowledge Graph](https://img.shields.io/badge/Knowledge%20Graph-Oxigraph-purple.svg)](https://oxigraph.org/)
[![Release](https://img.shields.io/badge/release-v0.6.0-blue)](https://github.com/skaiy/wild_agentos/releases)

---

[**中文**](README.zh.md) · [**English**](README.md) · [**设计细节 →**](docs/13-DESIGN_DETAIL.zh.md)

<img src="assets/github-readme.png" alt="Wild AgentOS" width="100%" />

</div>

---

## 什么是 Wild AgentOS？

Wild AgentOS 是一个以 Rust 构建的**语义内核 AgentOS**。它以 PDCA
循环编排多智能体工作，并结合 Oxigraph RDF/SPARQL 语义图、Hyperspace
向量存储和受治理的本体知识工程。

系统的安全边界是经过验证的 JWT **`IsolationClaims`**：claims 为租户和
项目 mint graph 与 vector 目标，并为租户 mint blob prefix 与 L0 path。缺少
或无效 claims 的 scoped path 会 fail closed。安全命名不等于迁移历史数据；
请参阅[隔离契约](docs/17-isolation-contract.zh.md)。

## 核心技术栈

| 组件 | 实现 |
|---|---|
| Agent 协调 | Rust PDCA orchestration、EventBus 与 task/runtime service |
| 语义图 | Oxigraph RDF store、SPARQL 1.1 与 claims-derived named graph |
| 向量检索 | 内嵌 `hyperspace-engine` HNSW store 与可配置 embedding service |
| 隔离与身份 | `IsolationClaims`、JWT verification 与 production OIDC/JWKS validation |
| 接口 | HTTP/SSE 与 gRPC；inbound MCP publishing 和 optional outbound A2A |
| 治理 | Ontology staging、deterministic check、review/approval record 与 audit event |

## v0.6.0：Online corpus job 与 watcher

v0.6.0 为已配置的 corpus 变更和增量 delta 提供已认证、claims-scoped 的
online corpus job。默认开启的 watcher 通过同一幂等路径入队；部署可以
显式关闭 watcher 的 polling 与 enqueueing，而不会删除保留的 job、cursor、
review 或 audit record。

runner 遵循有边界的 KE 流程：

```text
extract → canonicalize → quality gate → 保持待审批的 entity-resolution suggestion → staging
```

它保留有上限的 provenance 和按 scope 隔离的 observability，覆盖 source
version、content digest、canonicalization、quality/review、ER suggestion、
saturation、retry 与 job state。transient sidecar failure 最多重试三次；
validation、authentication 和 policy failure 为 terminal。fail-closed CI 也
覆盖 online job、runner、watcher 和 production-write path。

job 只将证据写入 staging，不会自动合并实体、promote ontology 或
materialize production data。这些操作仍然需要显式人工治理并保留审计。

## 已交付能力

| 领域 | 当前可用能力 |
|---|---|
| **PDCA 编排** | 用于多智能体 Plan/Do/Check/Act 工作流的 Rust 协调与生命周期原语，包含 EventBus 审计信号和持久化 L0 envelope。 |
| **Claims 隔离** | 已验证 JWT `IsolationClaims` mint 租户/项目 graph 与 vector 目标，并 mint 租户 blob prefix 与 L0 path；scoped HTTP 与 runtime graph/vector path fail closed。 |
| **身份认证** | 面向生产部署的 OIDC/JWKS 验证，对 issuer、audience 和 JWKS 进行 fail-closed 校验。本地开发 HS256 是独立的 development mode。 |
| **本体 KE / GE** | 从 CSV、JSON Schema、OpenAPI 和 SQL DDL 生成 claims-scoped type draft；仅生成 draft 的 schema induction；constrained extraction 与 canonicalization；`KgQualityGate`；review；anchored materialization；冻结 golden evaluation；只读 ontology health evidence。 |
| **HITL 与审计** | 支持 approval-held Action staging、merge/discard 和 TTL；SPARQL `ASK` guardrail、高风险审批 hook 与 `ACTION_AUDIT` event。entity-resolution suggestion 保守、写入 staging，绝不 auto-merge。 |
| **Online corpus 运行** | 幂等 create/list/get/cancel/run job、默认开启 watcher、retry/backpressure、有上限 provenance 及 claims-scoped observability。 |
| **Market 与 promote** | 带不可变版本、claims-scoped visibility 以及显式 install/upgrade/rollback record 的 Logic 和 Skill package catalog。tenant Skill 和 emergent promotion 使用 gate、golden SHA verification、rule review、audit evidence 和所需人工审批。package installation 尚不会把 Skill 注册到 process-global registry。 |
| **互操作性** | 由 `IsolationClaims` 过滤的 inbound MCP catalog；显式发布且通过 gate 的 tenant Skill 可成为 MCP tool。薄型 outbound A2A adapter 默认由 feature flag 关闭，仅以 best-effort 方式运行。 |
| **知识与检索** | Oxigraph RDF/SPARQL named graph、claims-scoped graph/vector ingest 与 retrieval，以及内嵌 Hyperspace HNSW vector engine。有限 RDFS query-time expansion 默认关闭，绝不持久化 inferred triple。 |
| **制品与沙箱** | claims-scoped coding-artifact metadata 与服务端 mint 的 blob prefix；由默认关闭 feature flag 保护的外部 `SandboxProvider` adapter。 |

配套的 Admin control-plane 和 online-job-list surface 已独立交付，不属于本
仓库。

## 版本亮点

| 版本 | 日期 | 亮点 |
|---|---:|---|
| **v0.6.0** | 2026-09-08 | 已认证、claims-scoped online corpus job；默认开启 watcher；idempotency、有上限 provenance、observability、retry/backpressure 与 fail-closed production-write CI。 |
| **v0.5.0** | 2026-09-07 | 有边界的本体 Knowledge Engineering 与 Graph Engineering：仅写 staging 的 constrained extraction、quality/review、anchored materialization、待审批 ER suggestion、golden SHA gate、health reporting 和 compatibility-gated ontology design。 |
| **v0.3.0** | 2026-09-05 | 具不可变版本的 Logic 与 Skill market；OIDC/JWKS；受 gate 的 emergent-tool promotion；可选、默认关闭的 query-time RDFS expansion。 |
| **v0.2.2** | 2026-09-05 | claims-scoped artifact store、默认关闭的 external sandbox adapter 和可复现 private-deployment benchmark。 |
| **v0.2.1** | 2026-09-05 | claims-scoped ontology type draft、已过滤的 inbound MCP catalog、通过 gate 的 Skill-as-MCP 发布，以及默认关闭的 outbound A2A。 |
| **v0.2.0** | 2026-09-05 | Skill package verification、golden check、受控 tenant publishing 与 Rust CI golden evaluation。 |
| **v0.1.8** | 2026-09-04 | Ontology Action HITL staging、guardrail、SPARQL assertion 和 event audit。 |
| **v0.1.6–v0.1.7** | 2026-09-04 | `IsolationClaims`、fail-closed scoped path、isolation diagnosis/matrix/CI，以及显式 historical-key migration tool。 |
| **v0.1.5** | 2026-08-18 | Causal reasoning、unified graph backend、graph feature、snapshot timeline，以及带 system Skill guard 的 Skill Center CRUD。 |

完整版本记录请参阅 [changelog](CHANGELOG.md)。

## 架构与治理

Wild AgentOS 明确区分语义与治理边界：

- **语义基础：** Oxigraph RDF/SPARQL 与 named graph 保持为 knowledge graph
  store 和 query layer。
- **受治理的本体写入：** extracted candidate 依据 promoted ontology definition
  canonicalize、verify、stage 和 review。schema draft 永不 auto-promote。
- **人工权威：** entity merge、ontology promotion 和 production
  materialization 必须分别走显式 approval path。
- **独立证据：** deterministic SPARQL/SHACL check、冻结 golden fixture、audit
  record 与 materialization 后 re-read，避免将 LLM 或 worker completion report
  当作成功证据。
- **隔离即默认：** 调用方不能选择 scoped storage target；verified claims 选择
  server-minted target，cross-scope 或 failed path 无法写入 production。

## 从源码构建

```bash
git clone https://github.com/skaiy/wild_agentos.git
cd wild_agentos
cargo build --workspace
cargo test --workspace
```

## 本地运行

默认 binary 启动 HTTP/SSE server（端口 `8080`）与 gRPC（端口 `50051`）。在
checkout 根目录使用提供的 `config.yaml` 启动：

```bash
cargo run --bin wild-agent-os-core
```

使用 LLM-backed feature 前需配置 LLM gateway。仓库内的 config 保留空的
gateway credential；deployment credential 应始终存放在 version control
之外。生产环境请设置 `AGENTOS_ENV=production`，并配置[隔离契约](docs/17-isolation-contract.zh.md)
所述必需 OIDC/JWKS environment value。server 会拒绝 production HS256
configuration。

## 文档

- [演进路线图](docs/18-evolution-roadmap.zh.md) — 已交付里程碑、边界与明确非目标
- [隔离契约](docs/17-isolation-contract.zh.md) — verified claims、fail-closed
  storage target 与 historical-key status
- [隔离矩阵](docs/17-isolation-matrix.zh.md) — 经 CI 验证的 isolation behavior
- [本体 KE 流水线](docs/21-ontology-knowledge-engineering-pipeline.zh.md) —
  online corpus job/watcher 与 KE governance boundary
- [本体 Action 数据沙箱](docs/15-ontology-action-sandbox.zh.md) — HITL staging、
  guardrail 与 audit
- [本体 KE Golden Freeze Policy](docs/22-ontology-ke-golden-freeze-policy.zh.md)
- [Outbound A2A adapter](docs/19-a2a-outbound.md)
- [Private-deployment benchmark](docs/19-private-deploy-benchmark.md)
- [设计细节](docs/13-DESIGN_DETAIL.zh.md)

## 参与贡献

- 在 [GitHub Issues](https://github.com/skaiy/wild_agentos/issues) 报告问题。
- 在 [GitHub Discussions](https://github.com/skaiy/wild_agentos/discussions) 提出建议。
- 向 `main` 提交 pull request。

提交前请运行相关检查：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

## 许可证

Wild AgentOS 采用双许可：

- **Community Edition** — [GNU AGPL v3.0](LICENSE)（参见 [NOTICE](NOTICE)）。
- **Commercial Edition** — 无法遵守 AGPLv3 的商业使用需要单独获取商业授权；
  详见 [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)。

商业授权请联系 **diaoguoliang@gmail.com**。参与贡献需签署
[Contributor License Agreement](CLA.md)。

版权所有 (c) 2026 skaiy (diaoguoliang@gmail.com)。
