# Wild AgentOS
<div align="center">

<img src="assets/logo_transparent.png" width="120" alt="Wild AgentOS Logo" />

**A Rust semantic-kernel AgentOS for governed multi-agent systems**

[![Star on GitHub](https://img.shields.io/github/stars/skaiy/wild_agentos?style=flat)](https://github.com/skaiy/wild_agentos)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![gRPC](https://img.shields.io/badge/gRPC-Protocol-green.svg)](https://grpc.io/)
[![Knowledge Graph](https://img.shields.io/badge/Knowledge%20Graph-Oxigraph-purple.svg)](https://oxigraph.org/)
[![Release](https://img.shields.io/badge/release-v0.6.0-blue)](https://github.com/skaiy/wild_agentos/releases)

---

[**English**](README.md) · [**中文**](README.zh.md) · [**Design detail →**](docs/13-DESIGN_DETAIL.md)

<img src="assets/github-readme.png" alt="Wild AgentOS" width="100%" />

</div>

---

## What is Wild AgentOS?

Wild AgentOS is a **semantic-kernel AgentOS** built in Rust. It orchestrates
multi-agent work through PDCA loops and combines an Oxigraph RDF/SPARQL
semantic graph, Hyperspace vector storage, and governed ontology knowledge
engineering.

Its security boundary is verified JWT **`IsolationClaims`**: claims mint graph
and vector targets for a tenant and project, plus tenant-scoped blob prefixes
and L0 paths. Scoped paths fail closed when claims are absent or invalid.
Safe-name minting is not a migration of historical data; see the
[Isolation Contract](docs/17-isolation-contract.md).

## Core stack

| Component | Implementation |
|---|---|
| Agent coordination | Rust PDCA orchestration, EventBus, and task/runtime services |
| Semantic graph | Oxigraph RDF store with SPARQL 1.1 and claims-derived named graphs |
| Vector retrieval | Embedded `hyperspace-engine` HNSW store with configurable embedding services |
| Isolation and identity | `IsolationClaims`, JWT verification, and production OIDC/JWKS validation |
| Interfaces | HTTP/SSE and gRPC; inbound MCP publishing and optional outbound A2A |
| Governance | Ontology staging, deterministic checks, review/approval records, and audit events |

## v0.6.0: Online corpus job orchestration

v0.6.0 delivers authenticated, claims-scoped online corpus job metadata and
orchestration. Enabled-by-default watchers poll configured source-version
registrations and enqueue one idempotent job for each unseen version.
Deployments can explicitly disable this polling and enqueueing without deleting
retained jobs, cursors, reviews, or audit records. Watchers do not fetch a
source URI, compute content deltas, or run jobs themselves.

An authenticated caller runs a queued job by supplying its text, extractor,
extraction candidates, and quality-gate request. The manual runner then follows
the bounded KE flow:

```text
caller-supplied candidates → canonicalize → stage → quality gate → approval-held entity-resolution suggestion → awaiting review
```

It retains bounded provenance and scoped observability for source versions and
content digests, canonicalization, quality/review, ER suggestions, saturation,
retries, and job state. Transient sidecar failures retry at most three times;
validation, authentication, and policy failures are terminal. Fail-closed CI
also covers the online job, runner, watcher, and production-write paths.

Jobs stage evidence. They do not automatically merge entities, promote an
ontology, or materialize production data. Those operations remain explicitly
human-governed and auditable.

## Shipped capabilities

| Area | What is available |
|---|---|
| **PDCA orchestration** | Rust coordination and lifecycle primitives for multi-agent Plan/Do/Check/Act workflows, with EventBus audit signals and persisted L0 envelopes. |
| **Claims isolation** | Verified JWT `IsolationClaims` mint tenant/project graph and vector targets plus tenant blob prefixes and L0 paths; scoped HTTP and runtime graph/vector paths fail closed. |
| **Identity** | OIDC/JWKS verification for production deployments, with fail-closed issuer, audience, and JWKS validation. Local-development HS256 remains a separate development mode. |
| **Ontology KE / GE** | Claims-scoped type drafts from CSV, JSON Schema, OpenAPI, and SQL DDL; draft-only schema induction; constrained extraction and canonicalization; `KgQualityGate`; review; anchored materialization; frozen golden evaluations; and read-only ontology health evidence. |
| **HITL and audit** | Approval-held Action staging with merge/discard and TTL handling, SPARQL `ASK` guardrails, high-risk approval hooks, and `ACTION_AUDIT` events. Entity-resolution suggestions are conservative, staged, and never auto-merged. |
| **Online corpus operations** | JSON-persisted, claims-scoped create/list/get/cancel jobs; a manual staging runner; default-on configured-version polling that enqueues but does not execute jobs; retry/backpressure, bounded provenance, and claims-scoped observability. |
| **Markets and promotion** | A versioned Logic and Skill package catalog with immutable versions, claims-scoped visibility, and explicit install/upgrade/rollback records. Tenant Skill and emergent promotion use gates, golden SHA verification, rule review, audit evidence, and required human approval. Package installation does not yet register Skills in the process-global registry. |
| **Interoperability** | An inbound MCP catalog filtered by `IsolationClaims`; explicitly published, gated tenant Skills can be MCP tools. A thin outbound A2A adapter is feature-flagged off by default and is best-effort only. |
| **Knowledge and retrieval** | Oxigraph RDF/SPARQL named graphs, claims-scoped graph and vector ingestion/retrieval, and an embedded Hyperspace HNSW vector engine. Optional limited RDFS query-time expansion is disabled by default and never persists inferred triples. |
| **Artifacts and sandboxing** | Claims-scoped coding-artifact metadata and server-minted blob prefixes; an external `SandboxProvider` adapter behind a default-off feature flag. |

The companion Admin control-plane and online-job-list surfaces are delivered
separately; they are not part of this repository.

## Release highlights

| Version | Date | Highlights |
|---|---:|---|
| **v0.6.0** | 2026-09-08 | Authenticated, claims-scoped online corpus job metadata and manual staging runner; default-enabled configured-version watchers that enqueue only; idempotency, bounded provenance, observability, retry/backpressure, and fail-closed production-write CI. |
| **v0.5.0** | 2026-09-07 | Bounded ontology Knowledge Engineering and Graph Engineering: staging-only constrained extraction, quality/review, anchored materialization, approval-held ER suggestions, golden SHA gates, health reporting, and compatibility-gated ontology design. |
| **v0.3.0** | 2026-09-05 | Immutable-version Logic and Skill markets; OIDC/JWKS; gated emergent-tool promotion; optional default-off query-time RDFS expansion. |
| **v0.2.2** | 2026-09-05 | Claims-scoped artifact store, default-off external sandbox adapter, and reproducible private-deployment benchmarks. |
| **v0.2.1** | 2026-09-05 | Claims-scoped ontology type drafts, filtered inbound MCP catalog, gated Skill-as-MCP publishing, and default-off outbound A2A. |
| **v0.2.0** | 2026-09-05 | Skill package verification, golden checks, gated tenant publishing, and Rust CI golden evaluations. |
| **v0.1.8** | 2026-09-04 | Ontology Action HITL staging, guardrails, SPARQL assertions, and event audit. |
| **v0.1.6–v0.1.7** | 2026-09-04 | `IsolationClaims`, fail-closed scoped paths, isolation diagnosis/matrix/CI, and explicit historical-key migration tooling. |
| **v0.1.5** | 2026-08-18 | Causal reasoning, a unified graph backend, graph features, snapshot timeline, and Skill Center CRUD with system Skill guards. |

See the full [changelog](CHANGELOG.md).

## Architecture and governance

Wild AgentOS keeps the semantic and governance boundaries explicit:

- **Semantic foundation:** Oxigraph RDF/SPARQL and named graphs remain the
  knowledge-graph store and query layer.
- **Governed ontology writes:** extracted candidates are canonicalized against
  promoted ontology definitions, verified, staged, and reviewed. Schema drafts
  never auto-promote.
- **Human authority:** entity merges, ontology promotion, and production
  materialization require their respective explicit approval paths.
- **Independent evidence:** deterministic SPARQL/SHACL checks, frozen golden
  fixtures, audit records, and post-materialization re-reads prevent an LLM or
  worker completion report from being treated as success evidence.
- **Isolation by construction:** callers cannot choose scoped storage targets;
  verified claims select server-minted targets, and cross-scope or failed paths
  cannot write production.

## Build from source

```bash
git clone https://github.com/skaiy/wild_agentos.git
cd wild_agentos
# Install Protocol Buffers' protoc compiler before building.
cargo build --workspace
cargo test --workspace
```

## Run locally

The default binary starts the HTTP/SSE server on port `8080` and gRPC on
`50051`. Start it from a checkout with the provided `config.yaml`:

```bash
cargo run --bin wild-agent-os-core
```

Configure an LLM gateway before using LLM-backed features. The supplied config
contains empty gateway credentials; keep deployment credentials outside version
control. For production, set `AGENTOS_ENV=production` and configure the required
OIDC/JWKS environment values described in the
[Isolation Contract](docs/17-isolation-contract.md). The server refuses a
production HS256 configuration.

## Documentation

- [Evolution roadmap](docs/18-evolution-roadmap.md) — shipped milestones,
  boundaries, and explicit non-goals
- [Isolation contract](docs/17-isolation-contract.md) — verified claims,
  fail-closed storage targeting, and historical-key status
- [Isolation matrix](docs/17-isolation-matrix.md) — CI-verified isolation
  behavior
- [Ontology KE pipeline](docs/21-ontology-knowledge-engineering-pipeline.md) —
  online corpus jobs/watchers and KE governance boundaries
- [Ontology Action data sandbox](docs/15-ontology-action-sandbox.md) — HITL
  staging, guardrails, and audit
- [Ontology KE golden-freeze policy](docs/22-ontology-ke-golden-freeze-policy.md)
- [Outbound A2A adapter](docs/19-a2a-outbound.md)
- [Private-deployment benchmark](docs/19-private-deploy-benchmark.md)
- [Design detail](docs/13-DESIGN_DETAIL.md)

## Contributing

- Report bugs in [GitHub Issues](https://github.com/skaiy/wild_agentos/issues).
- Propose ideas in [GitHub Discussions](https://github.com/skaiy/wild_agentos/discussions).
- Submit pull requests against `main`.

Before submitting, run the relevant checks:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

## License

Wild AgentOS is dual-licensed:

- **Community Edition** — [GNU AGPL v3.0](LICENSE) (see [NOTICE](NOTICE)).
- **Commercial Edition** — commercial use that cannot comply with AGPLv3
  requires a separate commercial license; see [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md).

For commercial licensing, contact **diaoguoliang@gmail.com**. Contributions
require the [Contributor License Agreement](CLA.md).

Copyright (c) 2026 skaiy (diaoguoliang@gmail.com).
