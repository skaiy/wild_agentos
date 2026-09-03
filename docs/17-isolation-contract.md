# 17. Isolation Contract

`src/isolation/` is a kernel contract for scoped identity and future storage
names. It is not an identity provider (IdP), authentication implementation, or
storage migration.

## Trusted claims boundary

The only trusted construction path is:

```rust
IsolationClaims::from_verified(tenant_id, project_id, actor_id)
```

The caller of this API is responsible for verifying those values at an
authentication boundary. The isolation module does not read HTTP requests,
parse JSON bodies, inspect headers, or validate credentials.

`X-Identity` is a development simulation only. It is not a production tenant
identity source and must not be treated as one by new code or deployments.
Making the HTTP boundary fail closed is tracked separately in issue #24; this
contract does not change HTTP behavior.

This is deliberately not a Keycloak integration, a 17-state Temporal workflow,
or a StageExecutor feature.

## Naming contract, not a migration

Claims mint names for a future isolated layout:

| Mint | Contract |
| --- | --- |
| graph IRI | `graph://{tenant}/{project}` |
| object prefix | `{tenant}/` |
| vector namespace | `vector://{tenant}/{project}` |
| L0 path | `/data/l0/{tenant}` |

Minting validates the tenant and project as safe identifier segments and fails
closed for empty values, `.` / `..`, path separators, and other unsafe
characters. It does not create a directory or access a backend.

Minting is **not** a data migration. Do not remap or rewrite the existing live
key spaces:

- named graph: `graph:world`
- vector tag: `tenant:<id>`
- L0 storage: `./data/l0_store` with `l0.redb`
- blob key: `tenant:default/kb/...`

Storage wiring and migration require a separately scoped change.

## Graph interface

Wild AgentOS graph queries remain **SPARQL 1.1** queries against Oxigraph.
This contract does not introduce Cypher, replace Oxigraph, or alter the graph
query interface.
