# Changelog

日期以 [GitHub Releases](https://github.com/skaiy/wild_agentos/releases) 为准。crate 版本号为 `0.6.0`。

## [0.6.0] — 2026-09-08

### Online Corpus Job + Watcher

- Added authenticated, claims-scoped online corpus jobs for configured corpus
  changes and incremental deltas, with idempotent create, list, get, cancel,
  and runner paths.
- Added enabled-by-default corpus watchers that enqueue work through the same
  idempotent path. Deployments can explicitly disable watcher polling and
  enqueueing without deleting jobs, cursors, reviews, or audit records.
- The runner now requires entity-resolution suggestions and retains them for
  explicit approval; it stages evidence and never auto-merges entities,
  promotes ontology, or materializes production data.
- Added bounded provenance and observability for source versions and content
  digests, canonicalization, quality/review, ER suggestions, queue saturation,
  retries, and job state. Transient sidecar failures retry at most three times;
  validation, authentication, and policy failures are terminal.
- Added fail-closed isolation and production-write CI coverage for online jobs,
  runners, and watchers. Unauthenticated, invalid, cross-scope, and failed
  paths cannot write production data.
- The companion Admin online-corpus job list is delivered separately and does
  not expand this repository's scope.

## [0.5.0] — 2026-09-07

### Ontology Knowledge Engineering + Graph Engineering

- Completed the bounded, claims-scoped ontology Knowledge Engineering pipeline:
  constrained extraction and optional Morph-KGC/RML materialization write only
  to staging; canonicalization uses promoted ontology definitions and retains
  provenance and mapping decisions.
- Added the `KgQualityGate` supervisory loop with deterministic SPARQL `ASK`
  anchors, an opt-in pySHACL sidecar, a compliance-first
  quality-versus-coverage policy, and a claims-scoped review queue. Failed
  anchors cannot be overridden by a Judge.
- Added anchored staging-to-production materialization. It requires verified
  claims, explicit confirmation, and a passed gate or recorded human approval;
  the server re-reads production before reporting success and retains staging
  evidence for audit.
- Added conservative entity-resolution suggestions using a process-isolated
  GLinker-style sidecar. Suggestions write `owl:sameAs` and provenance only to
  staging and require approval; they never auto-merge.
- Froze ontology KE golden evaluations behind a SHA gate and documented the
  measurement-decay audit policy. Added the read-only, claims-scoped ontology
  health report for slow-loop goal review.
- Added golden-SHA, named rule-review, and audit-evidence requirements to
  tenant Skill and emergent-candidate promotion.
- Completed the pre-kernel ontology design path: OpenAPI and SQL DDL
  type-drafts, draft-only schema induction, a read-only readiness report, and
  compatibility-gated promotion with explicit `force_breaking` audit evidence.
  The companion Admin ontology design studio is delivered separately and does
  not expand this repository's scope.
- Extended the explicit, offline `isolation-migrate` tool to safely migrate
  historical local vectors, L0 data, and blobs in addition to named graphs;
  it validates targets and records audit evidence while retaining sources by
  default.
- Production deployments now require OIDC/JWKS authentication and refuse to
  boot with HS256 or incomplete OIDC configuration.

## [0.3.0] — 2026-09-05

### Markets + IdP + Emergent

- Added a versioned Logic and Skill package market. Package versions are
  immutable, tenant access is claims-scoped, and install, upgrade, and
  rollback select explicit versions.
- Added an OIDC/JWKS authentication mode alongside local-development HS256.
  It verifies asymmetric JWTs using configured issuer, audience, and HTTPS
  JWKS settings before minting `IsolationClaims`, and fails closed on
  verification or configuration errors.
- Added a gated emergent-tool promotion pipeline. Generated tools remain
  untrusted until each sandbox/judge gate and required human approval passes;
  no direct publish path is provided.
- Added optional, default-off limited RDFS inference for claims-scoped graph
  reads. It provides query-time subclass and type expansion only and does not
  persist inferred triples.

## [0.2.2] — 2026-09-05

### Artifacts + Sandbox + Bench

- Added a claims-scoped coding artifact store. Immutable artifact metadata is
  written to the caller's `IsolationClaims` graph, while artifact bytes use a
  server-minted tenant blob prefix.
- Added an external `SandboxProvider` adapter behind a default-off feature
  flag. Its async path does not hold `MutexGuard` across an `await`.
- Added reproducible private-deployment benchmarks for Oxigraph, redb, and
  Hyperspace that record measured results without fabricating speedups.

## [0.2.1] — 2026-09-05

### Ontology Data + Protocols

- Added ObjectType and LinkType drafts generated from CSV or JSON Schema.
  Drafts are tenant/project claims-scoped and promote only after an authorized
  human approval.
- Added an inbound MCP tool catalog filtered by verified `IsolationClaims`, so
  discovery exposes only tools authorized for the requesting tenant/project.
- Added Skills as MCP publication units: gated tenant Skills can be explicitly
  exposed as claims-authorized MCP tools, while kernel Skills remain excluded.
- Added a thin outbound A2A adapter behind a default-off feature flag. It sends
  best-effort outbound updates without adding an inbound A2A server or changing
  the local task lifecycle. See [Outbound A2A adapter](docs/19-a2a-outbound.md).

## [0.2.0] — 2026-09-05

### Control Plane + Skill CI

- Added the Skill package format and CI gate: package verification, golden
  input/output checks, and an optional Judge hook that is disabled by default.
  Passing packages publish through the gated tenant channel; failing fixtures
  are blocked.
- Added the Rust-CI golden evaluation suite for Agent plans, Skill Markdown,
  and Action invocation via `scripts/test_golden.sh`.
- Companion Admin #16 delivered the five-screen control-plane skeleton in its
  separate repository. That companion change is noted here only; it does not
  change this repository's scope.

## [0.1.8] — 2026-09-04

### Ontology Action HITL

- Added Action staging with configurable `commit_strategy`: actions can commit
  automatically or remain pending explicit approval, with merge, discard, and
  TTL-expiry handling.
- Added configurable ontology guardrails, including SPARQL `ASK` assertions,
  and a `high_risk` hook for approval-sensitive actions.
- Published `ACTION_AUDIT` EventBus events for committed, pending, approved,
  rejected, and violated Action outcomes. See the
  [Ontology Action Data Sandbox](docs/15-ontology-action-sandbox.md) for the
  current data-sandbox boundary.

## [0.1.7] — 2026-09-04

### Isolation proof and operations

- Added the read-only `isolation-diagnose` CLI to distinguish
  claims-minted targets from historical keys.
- Added a customer-readable isolation matrix and fail-closed
  `isolation_contract` golden test coverage in CI. See the
  [Isolation Contract](docs/17-isolation-contract.md) and
  [Isolation Matrix](docs/17-isolation-matrix.md).
- Added the optional, explicit `isolation-migrate` CLI for named-graph
  migration. It never performs a silent `UNION`; diagnosis remains the default
  operational path.

## [0.1.6] — 2026-09-04

### Isolation and hardening

- Added verified `IsolationClaims` and JWT project scope as the trusted boundary
  for tenant-scoped storage names. New graph, blob, vector, and L0 writes mint
  their targets from those claims.
- Scoped HTTP knowledge-graph, ontology, knowledge-base, and chat RAG paths to
  verified claims. Runtime graph/vector tools likewise ignore caller-selected
  graph and namespace targets.
- Scoped knowledge-base catalog writes, graph/vector ingestion and retrieval,
  raw-document blob access, and HTTP PDCA L0 persistence to verified claims.
- New user agents are stamped with the verified tenant/project scope; internal
  chat RAG rejects agents with missing or mismatched scope.
- Added an optional process-local tenant tool-call cap via
  `AGENTOS_TENANT_TOOL_CALL_CAP`, third-party MCP behavior disclosure, bash
  child-environment sanitization, and current-turn tool-schema enforcement.
- Persisted PDCA L0 envelopes and hardened isolated graph/vector/blob/L0
  paths. See the [Isolation Contract](docs/17-isolation-contract.md) for the
  complete boundary and historical-key status.

### Breaking / upgrade notes

- Tenant-scoped graph, vector, knowledge-base, and chat RAG paths now require a
  JWT that produces verified isolation claims; they fail closed without claims.
- Public API-key chat is explicitly non-tenant-RAG: it does not implicitly use
  a tenant graph or vector namespace.
- Agents without tenant/project scope are no longer implicitly shared across
  tenants. New user agents are stamped with verified scope; legacy unscoped
  agents are rejected by scoped chat RAG.
- Historical `tenant:` graphs, `graph:world`, and similar legacy keys are not
  migrated. New writes use targets minted from verified claims; plan any data
  migration separately.

## [0.1.5] — 2026-08-18

GitHub Release: https://github.com/skaiy/wild_agentos/releases/tag/v0.1.5

认知因果引擎与图治理升级（`CausalEngine` / 统一 `GraphBackend` / 图特征 / Snapshot Timeline / 技能中心 CRUD 与系统级 `iri://` 只读守卫）。

README 中英发版表此前误写 `2026-07-08`，已与 Release 发布日对齐。
