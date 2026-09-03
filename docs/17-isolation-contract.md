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
