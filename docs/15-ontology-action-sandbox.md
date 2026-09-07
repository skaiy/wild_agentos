# Ontology Action Execution Sandbox (data sandbox implemented / compute sandbox planned)

> *A Chinese version is available in [15-ontology-action-sandbox.zh.md](15-ontology-action-sandbox.zh.md).*
>
> Related code: `src/api/http/ontology.rs` (`invoke_action_handler` /
> `commit_via_staging`), `src/api/http/ontology_guardrails.rs`, and
> `src/knowledge_graph/ontology_store.rs`. See also
> [Knowledge Graph](07-knowledge-graph.md).

Ontology-layer `ActionType` makes the knowledge graph writable and executable.
Writing directly to a production named graph is irreversible, so this design
separates two risks and uses an independent mechanism for each:

| Category | Risk | Mechanism | Status |
|---|---|---|---|
| SPARQL write-back by declarative actions | corrupt graph data | **data sandbox** (named-graph isolation) | implemented |
| arbitrary user code / expression execution | unauthorized reads/writes or resource exhaustion | **compute sandbox** (process/namespace isolation) | planned |

## Data sandbox: staging-graph shadow execution

### Goal

Replace “write directly to production” with “write to an isolated shadow graph,
validate guardrails, then merge on success or roll back on failure.” This is a
rollback-capable transaction. It isolates data at the named-graph level only;
it does not isolate computation or processes.

### `commit_via_staging` flow

1. Each invocation receives a shadow graph IRI:
   `graph://{tenant}/{project}/staging/<uuid>` via
   `staging_graph_iri_for_claims`.
2. Side effects write directly to that claims-derived staging graph through
   `update_staging_for_claims`. The production graph remains unchanged.
   `DELETE WHERE` is a no-op on an empty shadow graph, as intended for
   add-only staging.
3. `ontology_guardrails` runs ASK/COUNT checks:
   - a triple-count cap: domain default or `ActionType` override, otherwise
     `5000`;
   - an allowed predicate-prefix list: domain default or `ActionType` override,
     otherwise the EV ontology, `rdfs`, and `rdf`; any other predicate violates
     the policy;
   - SPARQL `ASK` assertions: built-in ticket/FAQ examples require
     `rdfs:label`; domain and `ActionType` configuration can append readable
     `code` assertions. `ASK=true` is a violation.
4. The effective guardrail policy and invocation `commit_strategy` select the
   result:
   - a hard violation executes `DROP SILENT GRAPH <staging>`, returns **422**,
     and includes `violations`;
   - default `auto`, when the policy does not enlarge the default write surface,
     merges then deletes staging;
   - `require_approval`, or an action policy that increases the triple limit or
     expands predicate prefixes, retains staging and returns `approval_id` and
     `staging_graph`.

### `dry_run`

- `dry_run=true` returns only the SPARQL that would execute and writes no graph.
- `dry_run=false` uses the staging commit or approval path above.

### Audit events

After a durable decision, the action publishes structured JSON `ACTION_AUDIT`
to the existing `EventBus`. Tenant, project, and actor come only from verified
JWT isolation claims. Unauthenticated requests produce no event, and
best-effort event-publication failures do not alter the HTTP result.

| Decision | Trigger |
|---|---|
| `committed` | an `auto` staging graph merged |
| `pending` | approval is required or policy is high risk |
| `approved` | staging merged and cleared after approval |
| `rejected` | staging and approval cleared after rejection |
| `violated` | guardrail violation rolled staging back |

Every event carries `tenant_id`, `project_id`, `actor_id`, `action_id`,
`staging_id`, `decision`, `violations`, and `timestamp`.

### Approval API (verified JWT claims required)

The server mints tenant/project scope from a verified JWT; a request cannot
choose graph, tenant, or project. Approval records live in
`graph://{tenant}/{project}/action-approvals`; staging remains in
`graph://{tenant}/{project}/staging/{approval_id}`, so pending content never
appears in the production graph.

| Route | Behavior |
|---|---|
| `GET /api/v1/ontology/action-approvals` | List pending records in the caller’s tenant/project. |
| `POST /api/v1/ontology/action-approvals/:approval_id/approve` | Merge the associated staging graph and clear staging and approval. |
| `POST /api/v1/ontology/action-approvals/:approval_id/reject` (or `/discard`) | Discard the associated staging graph and approval. |

The approval schema is `approval_id`, `staging_id`, `staging_graph`,
`action_id`, `created_at`, and `expires_at`. The default TTL is 24 hours.
Reads or decisions lazily clear expired staging and metadata; approve/reject
then return **410 Gone**. Cross-tenant/project records are invisible and
cannot be approved or discarded; calls without verified JWT claims return
**401**.

### Guardrail configuration and scope

- Domain defaults use `PUT /api/v1/ontology/guardrails` and
  `GET /api/v1/ontology/guardrails`; both require verified JWT isolation
  claims.
- `ActionType.guardrails` is stored through the existing claims-authenticated
  CRUD. `max_triples` and `allowed_predicate_prefixes` override domain values
  item-by-item; `assertions` append in built-in → domain → `ActionType` order.
  Omitting a field always falls back to a safe default and never disables
  guardrails.
- Invocation requests do not accept `guardrails`; the server selects graph
  scope and effective policy from verified claims and stored configuration.

### Boundaries and tests

Guardrails are lightweight SPARQL ASK/COUNT checks, **not full SHACL**.
Oxigraph has no built-in SHACL; these auditable, configurable “near-SHACL”
assertions do not provide SHACL shapes, inference, report models, or complete
constraint semantics. Assertions cannot specify `GRAPH`; runtime binds each to
the claims-derived staging graph, preventing cross-tenant reads.

`ontology_action_tests` covers successful production merge, rollback for a
foreign predicate, same-scope approval and cross-tenant/JWT rejection,
whitelist overrides, and failed ASK assertions.

## Compute sandbox: external provider mount

Process isolation is needed only when custom actions/functions execute real
code or arbitrary expressions, rather than SPARQL templates with placeholder
allow-list replacement. Today custom actions have `executable=false` and
invoke returns **422**.

The kernel now provides `tools::sandbox_provider::SandboxProvider`, a mount
contract for external services such as OpenHands- or E2B-style gateways:

1. `submit(SandboxSubmitRequest)` accepts a caller-assigned `task_id`, verified
   `tenant_id`/`project_id`/`actor_id` claims, and JSON work descriptor.
2. `status(task_id)` reports `queued`, `running`, `succeeded`, `failed`, or
   `cancelled`.
3. `fetch_result(task_id)` returns a terminal `SandboxResult` JSON envelope:
   `task_id`, `status`, `exit_code`, structured `output`, and bounded
   `stdout`/`stderr`. A mismatched task ID or non-terminal result is rejected.

The default build includes `MockSandboxProvider`; it performs no code
execution and makes tests independent of external services. The generic
OpenHands/E2B-style HTTP gateway adapter (`POST /tasks`, `GET /tasks/{id}`,
`GET /tasks/{id}/result`) is only compiled with
`--features external-sandbox`. Credentials are intentionally not part of the
contract; deployment code must inject them at its transport boundary.

`AuditedSandboxProvider` emits best-effort `SANDBOX_AUDIT` events when a
terminal result is fetched. Payloads include the provider, task ID, verified
claims, final status, and exit code; result output is deliberately excluded
from audit events.

Reusable local-light-path foundations remain:

- `src/tools/tool_executor/builtins.rs` (`execute_bash`) and
  `src/tools/builtin/sandbox.rs` provide `FilesystemIsolationMode`,
  `isolateNetwork`, `namespaceRestrictions`, and
  `dangerouslyDisableSandbox`;
- `src/core/syscall_gate.rs` provides `WhitelistManager` / `SyscallGate` for
  a capability allow-list and signature-validation skeleton.

When requirements justify it, use a restricted DSL/SPARQL template layer
first. The external mount handles heavy execution; local bash namespace
isolation remains an explicit light-path opt-in. This scope does not add
container, gVisor, or WASM dependencies, does not create a fourth in-process
OS, and does not expand `BUILTIN_EXECUTABLE_ACTIONS` to arbitrary custom-action
code.

## Decision summary

- **Now:** declarative actions use a data sandbox—shadow graph, guardrails, and
  named-graph scope—with pure SPARQL, no new dependency, and rollback.
- **Heavy compute:** mount an external provider and accept only a structured
  result envelope; the HTTP adapter is feature-gated.
- **Local light path:** remains opt-in bash namespace isolation with the
  `syscall_gate` allow-list; arbitrary executable actions remain disabled.
