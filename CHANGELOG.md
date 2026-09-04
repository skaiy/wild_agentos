# Changelog

日期以 [GitHub Releases](https://github.com/skaiy/wild_agentos/releases) 为准。crate 版本号为 `0.1.8`。

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
