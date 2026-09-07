# 18. Wild AgentOS Evolution Roadmap (after v0.1.6)

> *A Chinese version is available in [18-evolution-roadmap.zh.md](18-evolution-roadmap.zh.md).*

This public strategic roadmap follows v0.1.6. It defines capability boundaries
to validate, not commitments to particular APIs, release dates, or performance
metrics.

## Product positioning and boundaries

Wild AgentOS is a **semantic-kernel AgentOS**: Rust PDCA orchestration with
Oxigraph RDF/SPARQL as its semantic-graph foundation, Hyperspace, the
`IsolationClaims` naming contract, and an ontology Action **data** sandbox.

- It is not a bare-metal microkernel operating system.
- It is not a comprehensive proprietary-platform recreation.
- Graph queries continue to use Oxigraph and SPARQL; Nebula/Cypher does not
  replace them.
- It does not mix separate product/business repositories or product boundaries
  into this open-source tree.
- `IsolationClaims` mint safe names; minting is **not** migration of existing
  data. See the current historical-key status in the
  [Isolation Contract](17-isolation-contract.md).

## Release buckets

### v0.1.6 — complete: isolated naming and fail-closed wiring

Verified JWT `IsolationClaims` mint graph, blob, vector, and L0 targets. HTTP
paths fail closed when claims are absent. Historical keys have not been
migrated. See the [Isolation Contract](17-isolation-contract.md).

### v0.1.7 — complete: isolation proof and evaluation

The auditable isolation proof package includes:

- the read-only `isolation-diagnose` CLI, which distinguishes claims-minted
  targets from historical keys;
- a customer-facing [Isolation Matrix](17-isolation-matrix.md) and CI
  fail-closed `isolation_contract` golden cases;
- an optional, explicit `isolation-migrate` historical-key migration tool. It
  diagnoses by default and never silently uses `UNION` or represents minting as
  migration.

Related work: [R17 diagnostic CLI](https://github.com/skaiy/wild_agentos/issues/82),
[CI and isolation matrix](https://github.com/skaiy/wild_agentos/issues/83), and
[optional migration](https://github.com/skaiy/wild_agentos/issues/84).

### v0.1.8 — complete: Ontology Action HITL

The ontology Action data sandbox now supports a human approval loop:

- `commit_strategy` can retain a staging graph for approval, merge, or discard,
  with TTL expiry;
- configurable guardrails support SPARQL `ASK` assertion sets and the
  `high_risk` hook;
- `ACTION_AUDIT` events record committed, pending, approved, rejected, and
  violated results through EventBus.

This remains a data sandbox; it does not promise arbitrary-code execution
sandboxing. See [Ontology Action Data Sandbox](15-ontology-action-sandbox.md).
Related work: [HITL](https://github.com/skaiy/wild_agentos/issues/85),
[guardrails and assertions](https://github.com/skaiy/wild_agentos/issues/86), and
[event auditing](https://github.com/skaiy/wild_agentos/issues/87).

### v0.2.0 — complete: Control Plane + Skill CI

- The Skill package format has a CI gate for package verification and golden
  input/output checks. Its optional Judge hook is disabled by default.
- Passing packages publish through the gated tenant channel; failing fixtures
  are blocked.
- Rust CI runs Agent-plan, Skill-Markdown, and Action-invocation golden
  evaluations through `scripts/test_golden.sh`.

Related work: [Skill CI and release](https://github.com/skaiy/wild_agentos/issues/88)
and [golden evaluations](https://github.com/skaiy/wild_agentos/issues/89).

Companion Admin #16 delivered the five-screen control-plane skeleton (Runs,
Skills, KB · Ontology, Keys · Models, and Isolation) in its separate
repository. This roadmap notes that companion change only; it does not change
that repository's scope or implementation.

### v0.2.1 — complete: Ontology Data + Protocols

- ObjectType / LinkType drafts can be generated from CSV or JSON Schema. They
  are claims-scoped and require authorized approval before promotion.
- The inbound MCP tool catalog is filtered by verified `IsolationClaims`.
- Gated tenant Skills can be explicitly exposed as claims-authorized MCP tools
  (default deny; kernel Skills excluded).
- A thin outbound A2A adapter is feature-flagged off by default. It is
  best-effort, does not add an inbound A2A server, and does not rewrite the
  local task lifecycle. See [Outbound A2A adapter](19-a2a-outbound.md).

Related work: [object-model drafts](https://github.com/skaiy/wild_agentos/issues/90),
[MCP catalog](https://github.com/skaiy/wild_agentos/issues/91),
[Skill-as-MCP](https://github.com/skaiy/wild_agentos/issues/92), and
[A2A adapter](https://github.com/skaiy/wild_agentos/issues/93).

### v0.2.2 — complete: Artifacts + Sandbox + Bench

- The claims-scoped coding artifact store writes immutable metadata to the
  caller's `IsolationClaims` graph and artifact bytes under a server-minted
  tenant blob prefix.
- The external `SandboxProvider` adapter is feature-flagged off by default;
  its async path does not hold `MutexGuard` across an `await`.
- Private-deployment benchmarks reproducibly measure Oxigraph, redb, and
  Hyperspace without inventing speed claims.

Related work: [artifact store](https://github.com/skaiy/wild_agentos/issues/94),
[external compute sandbox](https://github.com/skaiy/wild_agentos/issues/95), and
[reproducible benchmark](https://github.com/skaiy/wild_agentos/issues/96).

### v0.3.0 — complete: Markets + IdP + Emergent

- The versioned Logic and Skill package market provides immutable package
  versions, claims-scoped tenant access, and explicit install, upgrade, and
  rollback.
- OIDC/JWKS authentication operates beside local-development HS256. It verifies
  asymmetric JWTs against the configured issuer, audience, and JWKS endpoint,
  and fails closed for invalid configuration or verification.
- The emergent-tool promotion pipeline keeps generated tools untrusted until
  each required sandbox/judge gate and human approval passes.
- Limited RDFS inference is optional and disabled by default. It performs
  query-time subclass and type expansion for claims-scoped graph reads without
  persisting inferred triples.

Related work: [marketplace](https://github.com/skaiy/wild_agentos/issues/97),
[OIDC/IdP](https://github.com/skaiy/wild_agentos/issues/98),
[emergent tools](https://github.com/skaiy/wild_agentos/issues/99), and
[limited OWL/rules](https://github.com/skaiy/wild_agentos/issues/100).

### v0.5.0 — complete: Ontology Knowledge Engineering + Graph Engineering

[Ontology Knowledge Engineering Pipeline](21-ontology-knowledge-engineering-pipeline.md)
records the completed bounded milestone across two complementary tracks:

- **Pre-kernel Ontology Design Automation:** OpenAPI and SQL DDL create
  claims-scoped type drafts; schema induction remains draft-only; the
  readiness report is read-only; and promotion applies compatibility gates with
  explicit `force_breaking` audit evidence.
- **Kernel Graph Engineering:** constrained extraction and optional
  Morph-KGC/RML inputs stage canonicalized, provenance-bearing candidates;
  `KgQualityGate` supervises quality and review; materialization is anchored
  and auditable; and entity-resolution suggestions require approval before
  merge.
- Frozen ontology KE golden fixtures are SHA-gated, the measurement-decay
  policy is documented, and the read-only health report supplies slow-loop
  evidence. Tenant Skill and emergent promotion also require golden SHA
  verification, named rule review, and audit evidence.

Neither track auto-promotes a production ontology or bypasses verified claims.
The companion Admin ontology design studio is noted as a separate delivery and
does not change this repository's scope. Continuous online corpus watching and
automatic end-to-end processing remain outside this completed milestone.

### v0.6 — planned: Online Corpus Job + Watcher

This planned boundary orchestrates the existing bounded KE loops; it does not
introduce a second KE stack. Claims-scoped online jobs will process configured
corpus changes or incremental deltas through existing constrained extraction,
canonicalization, `KgQualityGate`, optional approval-held entity-resolution
suggestions, and staging. Default-off, opt-in watchers may enqueue those jobs.

The scope requires verified `IsolationClaims`, idempotency, retry/backpressure,
source-to-decision provenance, and observable job state. It does not silently
promote ontology, auto-merge entities, replace Oxigraph/SPARQL, add Cypher or
Nebula, or mix separate product/business repositories into this tree.
Materialization remains approval-held and anchored.

See [Ontology Knowledge Engineering Pipeline](21-ontology-knowledge-engineering-pipeline.md)
for the v0.6 boundary and proposed future Issue checklist.

## Explicit non-goals

1. No category-IV “microkernel OS” or bare-metal OS.
2. No pursuit of a 100% recreation of any proprietary platform.
3. No mixing separate product/business repositories or product boundaries into
   this open-source tree.
4. No replacement of Oxigraph/SPARQL with Nebula/Cypher.
5. No claim that minting names completes historical-data migration.
