# 17. Isolation Contract

`src/isolation/` is a kernel contract for scoped identity and storage names.
It is not an identity provider (IdP), an OIDC or Keycloak integration, a
17-state IAM workflow, or a storage migration.

This document describes the state of `main` as of 2026-09-04. It distinguishes
the storage paths that are wired today from historical paths that remain live.
Do not treat a minted name as proof that existing data has moved to it.

## Trusted claims boundary

The only trusted construction path is:

```rust
IsolationClaims::from_verified(tenant_id, project_id, actor_id)
```

The caller of this API is responsible for verifying those values at an
authentication boundary. The isolation module does not read HTTP requests,
parse JSON bodies, inspect headers, or validate credentials.

`X-Identity` is a development simulation only. It never creates
`IsolationClaims` and is not a production tenant identity source. JWT identities
verified by the current HTTP boundary do create claims; that boundary currently
validates HS256 JWTs using `AGENTOS_JWT_SECRET`, not OIDC or Keycloak. Unverified
requests have no claims and cannot use the claims-scoped graph or blob paths.
`JwtClaims.project_id` is an optional `Option<String>` field with a serde
default. A verified JWT without `project_id`, or with an empty `project_id`,
mints the `default` project. A non-empty value is passed to
`IsolationClaims::from_verified`; unsafe values such as `.`, `..`, or path
separators (for example `a/b`) fail closed there before graph, vector, L0, or
blob names are minted. In that case `verify_jwt` produces no identity.

This is deliberately not a Keycloak integration, a 17-state Temporal workflow,
or a StageExecutor feature.

## Naming contract, not a migration

Claims mint the following isolated layout:

| Mint | Contract |
| --- | --- |
| graph IRI | `graph://{tenant}/{project}` |
| object prefix | `{tenant}/` |
| vector namespace | `vector://{tenant}/{project}` |
| L0 path | `/data/l0/{tenant}` |

Minting validates the tenant and project as safe identifier segments and fails
closed for empty values, `.` / `..`, path separators, and other unsafe
characters. It does not create a directory or access a backend.

Minting is **not** a data migration. The following historical live key spaces
remain in place and have not been remapped or rewritten:

- named graph: `graph:world` (not migrated)
- vector tag: `tenant:<id>` (not migrated)
- L0 storage: `./data/l0_store/l0.redb` (not migrated)
- blob key: `tenant:default/kb/...` (not migrated)

Do not change, delete, or assume ownership of these historical paths as part of
new isolation work. Each migration requires a separately scoped change.

## 诊断工具

`scripts/isolation-diagnose` is a read-only, local filesystem diagnostic. It
does not require a JWT, HTTP endpoint, or secret, and it never opens, creates,
writes, merges, deletes, or migrates a graph, vector store, blob store, or L0
database.

```bash
# `data` is AGENTOS_DATA_DIR's default.
scripts/isolation-diagnose --data-root data

# Stable schema for a future migration inventory; Markdown remains the default.
scripts/isolation-diagnose --data-root data --json
```

The default output is an isolation-matrix Markdown snippet with `路径 | 身份 |
期望 | 实测` columns. It reports durable `graph://{tenant}/{project}` and
`vector://{tenant}/{project}` marker presence per tenant/project, local blob
object counts under `blobs/{tenant}/`, tenant L0 directory existence, and the
historical `graph:world`, `tenant:<id>`, `l0_store/l0.redb`, and
`tenant:<id>/kb/...` indicators.

The JSON output has schema version `1`:

```text
{
  "schema_version": 1,
  "data_root": "<path>",
  "read_only": true,
  "minted_namespaces": [{
    "tenant": "<safe tenant>", "project": "<safe project>",
    "graph_iri": "graph://<tenant>/<project>",
    "graph_artifact_count": 0,
    "vector_namespace": "vector://<tenant>/<project>",
    "vector_artifact_count": 0,
    "blob_prefix": "<tenant>/", "blob_object_count": 0,
    "l0_path": "l0/<tenant>", "l0_exists": false
  }],
  "historical": {
    "graph_world_present": false,
    "tenant_vector_tags": [],
    "shared_l0_redb_present": false,
    "legacy_blob_prefixes": []
  },
  "scan_warnings": []
}
```

`*_artifact_count` is a count of regular storage files that contain a durable
namespace marker, not a backend object count. This distinction makes the tool
safe even for backends whose open operation may initialize a WAL or take a
writer lock. Files larger than 32 MiB are not marker-scanned and appear in
`scan_warnings`; blob object counting still uses paths only. This is an
inventory for a future migration issue (such as #84), **not** a migration
tool: a positive minted result does not prove migration or complete
multi-tenant isolation.

The filesystem diagnostic does not make an HTTP request. API-key chat's
no-tenant-RAG invariant is instead covered by
`public_api_key_chat_context_performs_no_tenant_rag` in
`src/api/http/chat.rs`: public API-key requests have no claims and must
retrieve neither tenant graph nor tenant vectors.

## 历史键迁移工具

`scripts/isolation-migrate` is an explicit **offline** migration tool. It is
not an HTTP endpoint and no request hot path can invoke it. It currently
implements only Oxigraph named-graph migration; vector (`tenant:<id>`), L0,
and blob historical keys remain unimplemented and must not be represented as
migrated or isolated by production queries.

The plan is JSON with schema version `1`. It maps each historical source graph
to one claims-minted tenant/project target:

```json
{
  "schema_version": 1,
  "named_graphs": [{
    "source_graph": "graph:world",
    "target": { "tenant": "acme", "project": "research" }
  }]
}
```

Run a default, read-only plan first. It opens `<data-root>/kg` read-only and
writes neither a graph, a RocksDB/WAL artifact, nor an audit file:

```bash
scripts/isolation-migrate --data-root data --plan migration.json
```

After reviewing its source and target quad counts, execute the fixed sequence
`plan → copy → verify`:

```bash
scripts/isolation-migrate --data-root data --plan migration.json --execute
```

The tool rejects unsafe targets, duplicate targets, a target that already has
quads, and invalid plans before it copies anything. It copies via Oxigraph
SPARQL only and verifies that every target count equals its recorded source
count. A successful real run writes a local
`<data-root>/isolation-migrate-audit-<timestamp>.json`; the audit records the
plan/verify result but no secret. Source graphs are retained by default.

Deleting a verified source is a separate, destructive action requiring both
flags, and should only occur after a backup and an independent production
query check:

```bash
scripts/isolation-migrate --data-root data --plan migration.json --execute \
  --delete-source --confirm-delete-source
```

Rollback for a normal copy is to stop using the new target graph and clear it
with an operator-reviewed Oxigraph maintenance action; the original source
remains available. Deletion is not automatically reversible, which is why it
requires the two confirmations. There is deliberately no `UNION`, fallback,
or read-through from `graph:world` (and no equivalent behavior for vector,
L0, or blob keys). Until each historical backend is explicitly migrated and
verified, production queries must not claim that isolation is complete.

## Current wiring

### Spend gate

The tenant tool-call spend gate is active only when
`AGENTOS_TENANT_TOOL_CALL_CAP` is set to a valid cap. When it is unset, callers
do not need `IsolationClaims`. When it is set, each metered tool call requires
verified claims and calls over the per-tenant cap are rejected. This is a
process-local counter, not a billing ledger.

### Graph

New production graph writes use `IsolationClaims::from_verified` and
`graph_iri()` to target `graph://{tenant}/{project}`. Production compatibility
write, query, entity-search, neighbour, and delete APIs reject calls without
verified claims; they do not silently read or write `graph:world`. The HTTP
knowledge-base graph import, stats, and catalog CRUD handlers now require
JWT-verified claims and use that claims-scoped graph. Catalog metadata is written
through claims-scoped store APIs; client graph/namespace fields are ignored as
write targets. Requests without claims receive `401`; these handlers do not
migrate historical `tenant:` graphs or read through `graph:world`. Graph queries
remain SPARQL 1.1 against Oxigraph.

### Runtime tools

SA execution passes boundary-verified `IsolationClaims` to graph and vector
builtins. Their `graph`, `named_graph`, and `namespace` arguments never select
storage targets: reads and writes use the graph or vector namespace minted from
claims, and calls without claims return an explicit error. No read-only public
ontology exception is wired for runtime tools. Historical `graph:world` data is
not migrated or read through by this path.

### Blob

`BlobStore::put`, `get`, `delete`, and `exists` take `IsolationClaims` and only
accept a relative key. The backend mints `{tenant}/` with
`object_key_prefix()`, so new objects use keys such as
`{tenant}/kb/<kbid>/<sha256>`. HTTP upload, raw-document retrieval, and
rebuild paths require JWT-verified claims before using the BlobStore.

Existing `tenant:default/kb/...` objects are still historical objects. No
read-through compatibility mapping or migration has moved them to the new
prefix.

Coding artifact upload, listing, and download also require JWT-verified claims.
Artifact bytes use the minted `{tenant}/artifacts/` blob prefix and their
replay metadata is stored in the minted claims graph. A task IRI links each
patch, run transcript, or reproduction script to its checkpoint execution.
See [Claims-Scoped Coding Artifacts](20-coding-artifacts.md).

### Vector

Claims-scoped vector upsert, search, and delete use the namespace minted by
`vector_namespace()` (`vector://{tenant}/{project}`); it scopes both the stored
IRI and the JSON-LD named-graph metadata. HTTP knowledge-base vector ingest,
search, upload, and reindex now require JWT-verified claims and use this
namespace; requests without claims receive `401`. Production unscoped upsert,
search, filtered search, hybrid search, and delete APIs fail closed because they
lack verified claims. Historical `tenant:<id>` rows have not been migrated and
are not returned by claims-scoped search.

### Chat RAG

The internal agent chat RAG endpoint requires JWT-verified `IsolationClaims`.
Its graph and vector retrieval targets are minted from those claims; client
`named_graph` and `vector_namespace` fields and agent knowledge-pack target
configuration are ignored. The requested agent record must explicitly have the
same tenant and project as the verified claims; missing or mismatched agent
scope is rejected, so legacy unscoped agents are not shared implicitly.
New user agents are stamped with that verified tenant/project scope at creation.
Retrieval errors are returned rather than being silently converted into an
empty result. Public API-key chat does not carry
tenant/project claims and therefore performs no tenant RAG at all—it never
falls back to a tenant graph or vector namespace.

### L0

`L0Store::open_for_claims` creates and writes only the tenant directory minted
from `l0_path()` under its supplied L0 root (the `/data/l0/{tenant}` contract).
Production HTTP task execution opens `open_for_claims` when JWT-verified claims
are present, so PDCA persistence uses that tenant directory. Without claims it
keeps the startup L0 store, which `L0Store::new` opened read-only; writes through
that historical shared database fail closed. The historical
`./data/l0_store/l0.redb` database has not been migrated.

## Graph interface

Wild AgentOS graph queries remain **SPARQL 1.1** queries against Oxigraph.
This contract does not introduce Cypher, replace Oxigraph, or alter the graph
query interface.
