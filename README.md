# Wild AgentOS
<div align="center">

<img src="assets/logo_transparent.png" width="120" alt="Wild AgentOS Logo" />

**A governed operating system for teams that run multiple AI agents**

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

Wild AgentOS is a **governed operating system for multi-agent work**, built in
Rust (a systems programming language). It helps teams coordinate AI agents through PDCA (Plan, Do, Check, Act),
share knowledge, and keep an auditable record of work and decisions.

It is designed around tenant and project boundaries. A verified JWT (an
identity token) carries tenant/project isolation claims (`IsolationClaims`).
The system uses those claims to select the caller's graph, vector, file, and
work-record storage. Missing or invalid identity means access is refused; a
caller cannot select another tenant's storage. This applies to the
claims-scoped interfaces, not every legacy HTTP endpoint. The naming scheme
does not migrate historical data—see the [Isolation Contract](docs/17-isolation-contract.md).

## What you get

- **Coordinated work:** Rust-based PDCA workflows, task services, and an event
  audit trail for multi-agent Plan/Do/Check/Act work.
- **Shared knowledge and retrieval:** an Oxigraph knowledge graph (a structured
  store) queried with SPARQL, plus embedded vector retrieval for finding
  relevant information.
- **Controlled knowledge changes:** drafts from CSV, JSON Schema, OpenAPI, and
  SQL DDL can be checked, placed in a staging area, reviewed, and—when
  approved—written to production data. Drafts never become official
  definitions automatically.
- **Human oversight and evidence:** high-risk or approval-held Actions require
  a decision; low-risk Actions can commit only when their policy permits it.
  The system records checks, reviews, audit events, and read-back evidence.
  Entity-resolution suggestions (whether two records are the same entity) are
  staged and never merged automatically.
- **Identity and integrations:** production uses OIDC/JWKS identity validation;
  local development has a separate development signing mode. gRPC is available
  for service-to-service communication, and MCP (Model Context Protocol) can
  expose explicitly published, tenant-scoped Skills. The outbound A2A adapter
  is optional, off by default, and best-effort only.
- **Governed packages:** Logic and Skill packages have immutable versions,
  tenant-scoped visibility, and recorded install, upgrade, and rollback steps.
  Promotion requires defined gates, review evidence, and human approval.

The companion Admin control-plane and online-job-list user interfaces are
separate products and are not in this repository.

## v0.6: Online corpus jobs, with people in control

v0.6.0 adds authenticated online-corpus jobs within the same tenant/project
boundary. Watchers are enabled by default but only watch configured source
versions and place one job in the queue for each new version. A deployment can
turn that polling and queueing off without deleting retained jobs, cursors,
reviews, or audit records. Watchers do not fetch source URLs, calculate content
changes, or run jobs.

An authenticated person or service manually runs a queued job with supplied
text, extraction method, candidates, and a quality-check request. The job standardizes
the candidates, keeps their evidence in a staging area, runs checks, creates an
approval-held entity-resolution suggestion, and then awaits review.

The system keeps limited source-tracing and operational records, including
source versions, content digests, checks, review status, retries, capacity
pressure, and job state. Temporary sidecar failures retry at most three times;
identity, validation, and policy failures stop the job. CI verifies that failed,
invalid, cross-boundary, or unauthenticated paths cannot write production data.

Crucially, these jobs do **not** automatically merge entities, make an ontology
official, or write staged results to production. Those are explicit,
human-governed, auditable decisions.

## Release highlights

| Version | Date | Business summary |
|---|---:|---|
| **v0.6.0** | 2026-09-08 | Authenticated online-corpus job records, manual staging runner, queue-only watchers, bounded source tracing and operational records, retry/capacity controls, and CI-proven production-write isolation. |
| **v0.5.0** | 2026-09-07 | Governed knowledge-engineering flow: staged extraction, quality and review, approval-held identity suggestions, evidence, health reporting, and compatibility-gated ontology design. |
| **v0.3.0** | 2026-09-05 | Versioned Logic and Skill packages, production identity validation, human-gated emerging tools, and optional read-time knowledge expansion. |
| **v0.2.0–v0.2.2** | 2026-09-05 | Package verification and publishing controls, scoped knowledge drafts and integrations, artifact storage, an optional external sandbox adapter, and reproducible deployment benchmarks. |
| **v0.1.5–v0.1.8** | 2026-08-18–2026-09-04 | Causal analysis, graph and memory foundations, Action review and audit controls, and tenant/project isolation with diagnosis and migration tooling. |

See the full [changelog](CHANGELOG.md).

## How governance works

- **Knowledge stays explainable:** Oxigraph stores the knowledge graph; SPARQL
  is the query language used to inspect it. Separate knowledge-graph areas keep
  tenant/project data distinct.
- **Changes require the right decision:** candidates are standardized against
  approved definitions, checked, staged, and reviewed. Production promotion and
  writing production data require explicit governed steps.
- **Evidence is independent:** deterministic checks, frozen reference cases,
  audit records, and production read-backs mean a model or worker saying
  “complete” is not treated as proof.
- **Access is contained:** verified tenant/project claims choose the storage
  boundary, so claims-scoped interfaces do not write across boundaries.

## Build from source

```bash
git clone https://github.com/skaiy/wild_agentos.git
cd wild_agentos
# Install Protocol Buffers' protoc compiler before building.
cargo build --workspace
cargo test --workspace
```

## Run locally

### Runtime configuration

The server fails closed and exits non-zero if `config.yaml`, the runtime
override, or configuration environment variables cannot be parsed. For local
development only, explicitly opt into defaults with
`AGENT_OS_CONFIG_PROFILE=development` or `AGENT_OS_ALLOW_DEFAULT_CONFIG=true`;
do not set either in production.

---

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
