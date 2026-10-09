# 23. Admin Control-Plane ↔ Kernel API Matrix

> *A Chinese version is available in
> [23-admin-control-plane-api-matrix.zh.md](23-admin-control-plane-api-matrix.zh.md).*

This is the current-`main` (v0.7.0) matrix for the Admin control plane and the
kernel HTTP API. Hash routes identify Admin screens; they are not kernel paths.
“Claims required” describes current kernel behavior, not an authorization policy
inferred from a screen name.

## Status vocabulary

- **Existing** — the listed path is registered on current `main`.
- **Not doing** — deliberately outside this matrix.

## Matrix

| Admin screen | hash route | kernel method+path | claims required | status |
| --- | --- | --- | --- | --- |
| Runs | `#/runs` | `GET /api/v1/tasks` | Verified tenant/project `IsolationClaims`; lists only the caller’s persisted scope. | **Existing** |
| Runs — task detail | `#/runs` | `GET /api/v1/tasks/:task_iri`, `GET /api/v1/tasks/:task_iri/status`, `GET /api/v1/tasks/:task_iri/details`, `GET /api/v1/tasks/trends` | Verified tenant/project `IsolationClaims`; detail reads require the same persisted task scope as the Runs list, and trends aggregate only that scope. | **Existing** |
| Agents | `#/agents` | `GET, POST /api/v1/agents`; `PUT, DELETE /api/v1/agents/:id`; `POST /api/v1/agents/:id/chat` | Verified tenant/project `IsolationClaims`; user Agents are listed and mutated only in the caller’s persisted scope. The unscoped platform catalog remains shared runtime metadata. | **Existing** |
| Skills | `#/skills` | `GET, POST, DELETE /api/v1/skills`; `GET /api/v1/skills/manifest`; `POST /api/v1/skills/import-git`; `GET /api/v1/skills/pipeline-runs`; `POST /api/v1/skills/pipeline-rerun` | Skill mutations (`POST`/`DELETE /api/v1/skills`, `import-git`, `pipeline-rerun`) change the process-global skill registry and require `require_platform_admin` (#302); a tenant `DA` gets `403 platform_admin_required`. Reads do not have a uniform `IsolationClaims` gate. | **Existing** |
| KB · Ontology | `#/kb-ontology` | `GET, POST /api/v1/kb/bases`; `GET, POST /api/v1/kb/categories`; `GET, POST /api/v1/knowledge-packs`; `GET /api/v1/ontology/types`; `GET /api/v1/ontology/health` | KB graph/vector ingestion, catalog CRUD, and ontology writes use verified tenant/project `IsolationClaims`; missing claims fail closed. | **Existing** |
| Isolation | `#/isolation` | No create-tenant HTTP path. Local read-only diagnostic: `scripts/isolation-diagnose --data-root <path>` | JWT verification mints tenant/project claims. The diagnostic CLI needs no JWT and remains a read-only local import/inventory aid; it is not an HTTP endpoint. | **Existing** — no Admin create-tenant form |
| Keys · Models | `#/keys-models` | `GET, POST /api/v1/api-clients`; `PUT, DELETE /api/v1/api-clients/:id`; `POST, DELETE /api/v1/api-clients/:id/keys[/:kid]`; `GET /api/v1/api-audit`; `GET, PUT /api/v1/config`; `POST /api/v1/models/test`; `POST /api/v1/providers/models`; `POST /api/v1/embedding/activate` | API clients, keys, and audit require verified explicit claims plus `DA` and are isolated by verified tenant; cross-tenant mutations return the same 404 as missing records. `PUT /api/v1/config` (all sections) and `POST /api/v1/embedding/activate` require `require_platform_admin`: verified JWT with explicit non-empty tenant/project, exact `PLATFORM_ADMIN` role, and tenant matching the non-default `AGENTOS_PLATFORM_ADMIN_TENANT` (fail-closed if unset). No `DA` needed. `GET /api/v1/config` requires a verified JWT plus either `require_control_plane_da` (verified explicit tenant/project + DA) or `require_platform_admin`; the response omits secret-looking fields (names normalized by lowercasing and dropping `_`/`-`, e.g. `api_key`, `accessToken`, `client_secret`, `private_key`, `authorization`, `credentials`) and exposes credentials only as `*_configured` booleans. The Admin config page needs a DA token with tenant and project, or a platform-admin token; `POST /api/v1/models/test` and `POST /api/v1/providers/models` also require `require_platform_admin` (#303) and pass the provider outbound guard (#267); see "Provider probes and the gateway key" below. | **Wired** |
| Memory · Blackboard | `#/memory` (also deep-link `#/blackboard`) | `GET /api/v1/blackboard/tasks`; `GET /api/v1/blackboard/nodes?task_iri=…` | Verified tenant/project `IsolationClaims`; legacy records without persisted scope are not returned. | **Existing** |
| Ops | `#/ops` | `GET /api/v1/batch/agents`; `POST /api/v1/batch/agents/:name/control`; `GET /api/v1/guard/audit`; `GET /api/v1/guard/stats`; `GET /metrics` | Batch list/control require verified isolation claims plus `DA`. Guard audit/stats require verified tenant/project claims, use the same scoped set, and redact sensitive values. `GET /metrics` is a process-global scrape endpoint on the **HTTP API address** (`api.http_addr`, often `:8080` or `:8081` in demos)—not on `api.metrics_port` (default 9090), which has no listener (#324). | **Wired** — batch claims + DA |
| Online corpus | `#/online-corpus-jobs` | `GET, POST /api/v1/online-corpus-jobs`; `GET /api/v1/online-corpus-jobs/observability`; `GET /api/v1/online-corpus-jobs/:id`; `POST /api/v1/online-corpus-jobs/:id/cancel`; `POST /api/v1/online-corpus-jobs/:id/run` | Verified tenant/project `IsolationClaims`; list, read, transition, runner, and observability data are scoped. | **Existing** |
| Ontology design studio | `#/ontology-studio` | `GET, POST /api/v1/ontology/type-drafts`; `POST /api/v1/ontology/type-drafts/from-{csv,json-schema,openapi,sql-ddl,induction}`; `POST /api/v1/ontology/type-drafts/:draft_id/promote`; `POST, PUT, DELETE /api/v1/ontology/{object-types,link-types,action-types,function-defs}` | Verified tenant/project `IsolationClaims` for draft and ontology write flows; promotion remains explicit and auditable. | **Existing** |
| No-Code IDE | — | — | — | **Not doing** |
| Second Grafana | — | — | — | **Not doing** |
| Admin create-tenant form | — | — | Tenant scope comes from verified JWT claims, not an Admin tenant-creation API. | **Not doing** |
| Invocations (integrator API) | — | `POST /v1/invocations`; `GET /v1/invocations`; `GET /v1/invocations/:id`; `POST /v1/invocations/:id/cancel`; `GET /v1/invocations/:id/events` | Verified JWT `IsolationClaims` only (no API-client keys, no `X-Identity`). Cross-scope reads/cancels/events → byte-identical `404`. | **Planned** — epic [#313](https://github.com/skaiy/wild_agentos/issues/313); design [`docs/29-invocations-api.md`](29-invocations-api.md); contract tests #318 |
| Business orchestration | — | — | — | **Not doing** |

## Interpretation and boundaries

`/v1/invocations` is an **integrator-facing kernel API**, not an Admin control-plane
screen and not part of the OpenAI-compatible layer (`/v1/models`,
`/v1/chat/completions`, which authenticate with API-client keys). It has no Admin
hash route; clients call the kernel paths directly with a verified IdP JWT. See
[`docs/29-invocations-api.md`](29-invocations-api.md) and epic
[#313](https://github.com/skaiy/wild_agentos/issues/313).

The v0.7.0 release includes the claims-scoped Runs list, redacted and
claims-scoped Guard audit/statistics, and claims-scoped Blackboard task and node
browsing ([#221](https://github.com/skaiy/wild_agentos/issues/221),
[#222](https://github.com/skaiy/wild_agentos/issues/222),
[#223](https://github.com/skaiy/wild_agentos/issues/223), and
[#224](https://github.com/skaiy/wild_agentos/issues/224)).

Control-plane write routes authorize before they read the request body
(#312): `PUT /api/v1/config`, `POST /api/v1/models/test`,
`POST /api/v1/providers/models` and `POST /api/v1/embedding/activate` answer
`401` to unauthenticated callers and `403` to callers that fail the gate, with
the same bytes whatever the body is. Body errors (`422` for a schema error,
`400` for malformed JSON, `415` for a non-JSON content type) are returned only
to authorized callers, so field names never reach anyone else.

The isolation diagnostic is intentionally still usable
without a token because it is a local, read-only filesystem tool. It neither
creates tenants nor grants HTTP access.

### Write routes outside the matrix (#302)

These kernel write routes have no dedicated Admin screen row above, but follow
the same two gates:

- **Process-global → `require_platform_admin`.** `POST /api/v1/prompts`,
  `POST /api/v1/prompts/:id/activate`, `PUT /api/v1/prompts/:id/canary`,
  `DELETE /api/v1/prompts/:id` (one prompt registry and one active version
  for every tenant). Prompt reads are unchanged.
- **Tenant-scoped → `require_control_plane_da`** (verified JWT, explicit
  tenant and project, `DA`): `POST /api/v1/market/packages` and
  `.../:name/{install,rollback,upgrade}`; `POST, DELETE
  /api/v1/mcp/skill-exposures` (the owning tenant is taken from verified
  claims, never from `X-Identity`); `POST /api/v1/kb/bases/:id/reindex`.
- Another tenant's knowledge base, skill exposure, or private market package
  answers the same `404` as a missing one.

### API client id collisions and recovery

An API client id that appears under more than one tenant in `api_clients.json`
(for example after a manual import) is treated as ambiguous and fails closed:

- At load, every client with that id is marked `id_conflict` and a warning is
  logged with the id and the tenant ids only.
- Public API authentication returns `401` for any key whose `client_id` is
  shared by more than one client, whatever their status. A client in
  `id_conflict` also returns `401`.
- `PUT /api/v1/api-clients/:id` returns `409` for a status change on an
  `id_conflict` client. `DELETE /api/v1/api-clients/:id` on a shared id is
  refused with `409` and changes nothing (no client, no keys): deleting one
  side would end the collision and leave the other tenant with keys it never
  owned. Under a shared id the client list shows only keys that carry the
  caller tenant's prefix (none if two tenants share the same slug). Legacy
  audit records without `tenant_id` under a shared id are never returned.

Recovery is manual. With the service stopped (it rewrites these files from
memory), an administrator edits `api_clients.json` (and
`api_keys.json` for the affected keys) so that each client id belongs to
exactly one tenant, then resets the remaining clients' `status` from
`id_conflict` to `active` in the file and starts the service. The status
persists across saves and reloads until then.

`POST /api/v1/api-clients/:id/keys` is likewise refused with `409` (no key is
written) while the id is shared or the client is `id_conflict`. Before deleting
a client during recovery, operators must first revoke **all** keys under that
client id. Otherwise a leftover key, especially a legacy key without a tenant
prefix, could change owner once the id belongs to a single tenant again.
When a client id collides across tenants (`id_conflict`), the owner still cannot
delete the client (`409`), but it can revoke its own keys under the shared id
(`200`; only keys carrying its tenant prefix count, any other key gets the same
`404` as a missing key). Revoke returns `409` only when the caller's tenant slug
is ambiguous (another colliding tenant has the same slug); then a platform
administrator must resolve the collision first. While the id is shared, active
keys under it fail authentication with `401`; a revoked key fails with `403`
`key_revoked` instead, shared id or not. Each tenant should revoke its keys
before the collision is cleared—otherwise they become usable again once it is.

### Provider probes and the gateway key (#267, #303)

`POST /api/v1/models/test` and `POST /api/v1/providers/models` read global
provider configuration and may use saved provider keys, so they require a
platform administrator; a tenant `DA` gets `403 platform_admin_required`
before any outbound request.

Every probe target, caller-supplied or saved, passes the provider outbound
guard before a connection is made:

- absolute `http`/`https` URL without user credentials;
- `PROVIDER_OUTBOUND_ALLOWED_ORIGINS` (comma-separated origins, e.g.
  `https://llm.example.test,http://10.20.0.5:3000`), when set, is an exact
  allowlist; a listed origin may resolve to private or loopback addresses;
  a malformed entry denies everything. **A hostname on the allowlist may
  resolve to an internal address**: whoever controls that name's DNS
  decides where probes go, so list only names you control (or IP literals);
- without the allowlist only public addresses are allowed (documentation
  ranges, Teredo addresses, and 6to4 addresses that tunnel to a non-public
  IPv4 address count as non-public; decimal/octal/hex IPv4 spellings are normalized first); with
  `AGENTOS_AUTH_STRICT=true` the allowlist is required and every probe is
  refused while it is unset;
- link-local / cloud metadata, unspecified, multicast and broadcast
  addresses are never allowed, even for a listed origin;
- the host is resolved once and the request is pinned to the vetted
  addresses with proxies disabled (resolution times out after 5 s);
  redirects are not followed; response bodies are capped at 1 MiB.

A refused target gets `400 provider_outbound_not_allowed` with a fixed body
that never echoes the URL. Deployments that probe a provider on a private or
loopback address (for example a local model server) must list its origin.

`PUT /api/v1/config` that moves `gateway.base_url` to a different endpoint
while a gateway key is configured must also send a non-empty
`gateway.api_key`; otherwise it returns `400 explicit_api_key_required` and
nothing is saved or applied. The configured key is never carried to the new
endpoint. Keeping the same endpoint (equivalent spelling included), clearing
the base URL, or a gateway without a key are unaffected.

The same holds across restarts. `config_override.json` never stores the
gateway key, and at startup a deployment key (`config.yaml` or
`AGENT_OS_GATEWAY_API_KEY`) is dropped, with a warning that does not include
the key, when the override's `gateway.base_url` is a different endpoint than
the deployment's and `AGENT_OS_GATEWAY_BASE_URL` is not set. Only a key stored
in the override itself is used with the override's endpoint. The same rule
applies to `embedding.oneapi.base_url` and `AGENT_OS_EMBEDDING_ONEAPI_API_KEY`,
both after a restart and when an embedding change is hot-reloaded.
To move the gateway durably, set `AGENT_OS_GATEWAY_BASE_URL` together with
`AGENT_OS_GATEWAY_API_KEY` in the deployment.

The configuration loader treats key names case-insensitively, so this check
reads the override the same way: `BASE_URL`, `OneApi.Base_Url` and `base_url`
are the same field. The `gateway` and `embedding` sections of
`PUT /api/v1/config` are typed and accept only the documented lowercase field
names; any other field, including a differently cased spelling, returns `422`
and nothing is saved or applied.

Each load (startup or hot reload) reads `config_override.json` once, and the
key check uses that same read, so the endpoint it checks is the endpoint the
loaded configuration uses. An override written before these sections were
typed may still hold other spellings. If any key under `gateway` or `embedding`
is not lowercase, or the section appears under two spellings (`embedding` and
`Embedding`, or `embedding.oneapi`), the deployment key for that section is
always dropped, whatever endpoint the section names, and a warning names the
section (no key, no path). The file is not rewritten. Rewrite the section in
lowercase to restore the deployment key. Model names under
`gateway.model_mapping` are not affected. Embedding hot reloads run one at a
time, and each moves the previous vector store to its own
`vector_store.bak-<timestamp>-<n>` directory. The file is replaced
atomically: a new owner-only (`0600`) file is written and renamed over the old
one.

See [Isolation Contract](17-isolation-contract.md), [Isolation Matrix](17-isolation-matrix.md),
[Knowledge Ingestion](16-knowledge-ingest-import-graph.md), and
[Ontology Knowledge Engineering Pipeline](21-ontology-knowledge-engineering-pipeline.md)
for the underlying kernel contracts.
