# 23. Admin Control-Plane ↔ Kernel API Matrix

> *A Chinese version is available in
> [23-admin-control-plane-api-matrix.zh.md](23-admin-control-plane-api-matrix.zh.md).*

This is the v0.8 acceptance matrix for the Admin control plane and the kernel
HTTP API. Hash routes identify Admin screens; they are not kernel paths.
“Claims required” describes the current kernel behavior or the linked
in-flight change, not an authorization policy inferred from a screen name.

## Status vocabulary

- **Existing** — the listed path is registered on current `main`.
- **v0.8 milestone** — the path or claims boundary is being added by the linked
  open v0.8 work; it is not yet part of `main`.
- **Not doing** — deliberately outside this matrix and milestone.

## Matrix

| Admin screen | hash route | kernel method+path | claims required | status |
| --- | --- | --- | --- | --- |
| Runs | `#/runs` | `GET /api/v1/tasks` | Verified tenant/project `IsolationClaims` (planned); list only the caller’s persisted scope. | **v0.8 milestone** — [#221](https://github.com/skaiy/wild_agentos/issues/221), [PR #225](https://github.com/skaiy/wild_agentos/pull/225) |
| Runs — task detail | `#/runs` | `GET /api/v1/tasks/:task_iri`, `GET /api/v1/tasks/:task_iri/status`, `GET /api/v1/tasks/:task_iri/details`, `GET /api/v1/tasks/trends` | No uniform verified-claims gate on current `main`; do not treat these detail/read paths as a scoped Admin-list contract. | **Existing** |
| Agents | `#/agents` | `GET, POST /api/v1/agents`; `PUT, DELETE /api/v1/agents/:id`; `POST /api/v1/agents/:id/chat` | Creation and internal chat require verified `IsolationClaims`; the current list/update/delete handlers do not uniformly require or filter by claims. | **Existing** — mixed claims coverage |
| Skills | `#/skills` | `GET, POST, DELETE /api/v1/skills`; `GET /api/v1/skills/manifest`; `POST /api/v1/skills/import-git`; `GET /api/v1/skills/pipeline-runs`; `POST /api/v1/skills/pipeline-rerun` | Skill mutations require `DA`; reads do not have a uniform `IsolationClaims` gate. | **Existing** |
| KB · Ontology | `#/kb-ontology` | `GET, POST /api/v1/kb/bases`; `GET, POST /api/v1/kb/categories`; `GET, POST /api/v1/knowledge-packs`; `GET /api/v1/ontology/types`; `GET /api/v1/ontology/health` | KB graph/vector ingestion, catalog CRUD, and ontology writes use verified tenant/project `IsolationClaims`; missing claims fail closed. | **Existing** |
| Isolation | `#/isolation` | No create-tenant HTTP path. Local read-only diagnostic: `scripts/isolation-diagnose --data-root <path>` | JWT verification mints tenant/project claims. The diagnostic CLI needs no JWT and remains a read-only local import/inventory aid; it is not an HTTP endpoint. | **Existing** — no Admin create-tenant form |
| Keys · Models | `#/keys-models` | `GET, POST /api/v1/api-clients`; `PUT, DELETE /api/v1/api-clients/:id`; `POST, DELETE /api/v1/api-clients/:id/keys[/:kid]`; `GET /api/v1/api-audit`; `GET, PUT /api/v1/config`; `POST /api/v1/models/test`; `POST /api/v1/providers/models`; `POST /api/v1/embedding/activate` | API-client and audit operations require `DA`; config update requires verified JWT claims plus `DA`. Model test/provider discovery/embedding activation have no uniform claims gate on current `main`. | **Existing** — mixed claims coverage |
| Memory · Blackboard | `#/memory` (also deep-link `#/blackboard`) | `GET /api/v1/blackboard/tasks`; `GET /api/v1/blackboard/nodes?task_iri=…` | Verified tenant/project `IsolationClaims` (planned); legacy records without persisted scope must not be returned. | **v0.8 milestone** — [#223](https://github.com/skaiy/wild_agentos/issues/223), [PR #227](https://github.com/skaiy/wild_agentos/pull/227) |
| Ops | `#/ops` (planned); current sidebar leftovers: `#/overview`, `#/runtime`, `#/security` | `GET /api/v1/batch/agents`; `POST /api/v1/batch/agents/:name/control`; `GET /api/v1/guard/audit`; `GET /api/v1/guard/stats`; `GET /metrics` | Batch control requires `DA`; batch list and metrics have no uniform claims gate. Guard audit/stats require verified tenant/project claims only after the linked change, with audit and stats using the same scoped set. | **Existing** for batch/metrics; **v0.8 milestone** for guard — [#222](https://github.com/skaiy/wild_agentos/issues/222), [PR #226](https://github.com/skaiy/wild_agentos/pull/226) |
| Online corpus | `#/online-corpus-jobs` | `GET, POST /api/v1/online-corpus-jobs`; `GET /api/v1/online-corpus-jobs/observability`; `GET /api/v1/online-corpus-jobs/:id`; `POST /api/v1/online-corpus-jobs/:id/cancel`; `POST /api/v1/online-corpus-jobs/:id/run` | Verified tenant/project `IsolationClaims`; list, read, transition, runner, and observability data are scoped. | **Existing** |
| Ontology design studio | `#/ontology-studio` | `GET, POST /api/v1/ontology/type-drafts`; `POST /api/v1/ontology/type-drafts/from-{csv,json-schema,openapi,sql-ddl,induction}`; `POST /api/v1/ontology/type-drafts/:draft_id/promote`; `POST, PUT, DELETE /api/v1/ontology/{object-types,link-types,action-types,function-defs}` | Verified tenant/project `IsolationClaims` for draft and ontology write flows; promotion remains explicit and auditable. | **Existing** |
| No-Code IDE | — | — | — | **Not doing** |
| Second Grafana | — | — | — | **Not doing** |
| Admin create-tenant form | — | — | Tenant scope comes from verified JWT claims, not an Admin tenant-creation API. | **Not doing** |
| Business orchestration | — | — | — | **Not doing** |

## Interpretation and boundaries

The three linked v0.8 changes are intentionally listed as in flight:

1. [#221](https://github.com/skaiy/wild_agentos/issues/221) /
   [PR #225](https://github.com/skaiy/wild_agentos/pull/225) supplies the
   claims-scoped Runs list.
2. [#222](https://github.com/skaiy/wild_agentos/issues/222) /
   [PR #226](https://github.com/skaiy/wild_agentos/pull/226) scopes and
   redacts guard audit/stats.
3. [#223](https://github.com/skaiy/wild_agentos/issues/223) /
   [PR #227](https://github.com/skaiy/wild_agentos/pull/227) scopes Blackboard
   task and node browsing.

Until those pull requests merge, an Admin consumer must not claim that the
Runs list, guard audit/stats, or Blackboard reads have their proposed claims
boundary. Conversely, the isolation diagnostic is intentionally still usable
without a token because it is a local, read-only filesystem tool. It neither
creates tenants nor grants HTTP access.

See [Isolation Contract](17-isolation-contract.md), [Isolation Matrix](17-isolation-matrix.md),
[Knowledge Ingestion](16-knowledge-ingest-import-graph.md), and
[Ontology Knowledge Engineering Pipeline](21-ontology-knowledge-engineering-pipeline.md)
for the underlying kernel contracts.
