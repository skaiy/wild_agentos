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
- It is not a full Palantir clone.
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

### [v0.2.1 Ontology Data + Protocols](https://github.com/skaiy/wild_agentos/milestone/4)

- Semi-automatic ObjectType / LinkType drafts, with human approval required.
- MCP inbound tenant catalog.
- Skill-as-MCP publishing: gated tenant Skills can be explicitly exposed as
  JWT/claims-authorized MCP tools (default deny; kernel Skills excluded).
- A thin outbound A2A adapter without a kernel rewrite.

Related work: [object-model drafts](https://github.com/skaiy/wild_agentos/issues/90),
[MCP catalog](https://github.com/skaiy/wild_agentos/issues/91),
[Skill-as-MCP](https://github.com/skaiy/wild_agentos/issues/92), and
[A2A adapter](https://github.com/skaiy/wild_agentos/issues/93).

### [v0.2.2 Artifacts + Sandbox + Bench](https://github.com/skaiy/wild_agentos/milestone/5)

- Claims-scoped coding-artifact storage.
- An external compute-sandbox adapter (OpenHands/E2B-style mounting); the
  kernel receives structured results only.
- Reproducible benchmarks on modest compute, without invented speed claims.

Related work: [artifact store](https://github.com/skaiy/wild_agentos/issues/94),
[external compute sandbox](https://github.com/skaiy/wild_agentos/issues/95), and
[reproducible benchmark](https://github.com/skaiy/wild_agentos/issues/96).

### [v0.3.0 Markets + IdP + Emergent](https://github.com/skaiy/wild_agentos/milestone/6)

- Versioned Function / Skill marketplace.
- OIDC / IdP; claims remain minted at the authentication boundary.
- A gated emergent-tool pipeline.
- Limited OWL / rules as an optional feature, disabled by default.

Related work: [marketplace](https://github.com/skaiy/wild_agentos/issues/97),
[OIDC/IdP](https://github.com/skaiy/wild_agentos/issues/98),
[emergent tools](https://github.com/skaiy/wild_agentos/issues/99), and
[limited OWL/rules](https://github.com/skaiy/wild_agentos/issues/100).

## Explicit non-goals

1. No category-IV “microkernel OS” or bare-metal OS.
2. No pursuit of a 100% Palantir recreation.
3. No mixing separate product/business repositories or product boundaries into
   this open-source tree.
4. No replacement of Oxigraph/SPARQL with Nebula/Cypher.
5. No claim that minting names completes historical-data migration.
