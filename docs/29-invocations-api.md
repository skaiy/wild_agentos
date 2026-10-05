# 29. Invocations API (design draft)

> *A Chinese version is available in [29-invocations-api.zh.md](29-invocations-api.zh.md).*

> **Status: design draft, not implemented.** Tracking epic:
> [#313](https://github.com/skaiy/wild_agentos/issues/313), milestone v0.12.0.
> Field names and error codes may change before the sub-issues land. Nothing in
> this document describes behavior on current `main`.

## 1. Purpose

Downstream integrators need a durable, retryable and queryable call entry point:
submit one invocation, receive a persistent resource, poll or subscribe to its
state and result, and cancel it when needed. The existing `POST /api/v1/tasks`
only creates a task node, and `POST /api/v1/tasks/stream` ties results to one
HTTP connection. Neither has idempotency keys, an explicit lifecycle, cancel or
concurrent-write protection.

The Invocations API is built natively on the existing claims-only identity stack
(`IsolationClaims` minted from a verified IdP JWT Bearer). It adds no second
identity or token system.

## 2. Routes

| Method + path | Purpose |
| --- | --- |
| `POST /v1/invocations` | Create (optional `Idempotency-Key` header); returns `202` and the resource (`200` on an idempotent replay) |
| `GET /v1/invocations` | List the caller's scope; cursor pagination; optional `state` filter |
| `GET /v1/invocations/:id` | Read one invocation |
| `POST /v1/invocations/:id/cancel` | Request cancellation; optional `If-Match: "<revision>"`; `200` when the result is `cancelled`, `202` when it is `cancel_requested` |
| `GET /v1/invocations/:id/events` | Server-Sent Events: snapshot first, then live events, closes on a terminal state. **The route and its implementation belong to [#317](https://github.com/skaiy/wild_agentos/issues/317); #314 / #321 do not implement it. Until then, clients poll `GET /v1/invocations/:id`.** |

These routes live under `/v1/` but are **not** part of the OpenAI-compatible
layer (`/v1/models`, `/v1/chat/completions`), which authenticates with API-client
keys. Invocations accept verified JWT claims only.

## 3. Authentication and scope

- No verified claims (anonymous, invalid or expired JWT, `X-Identity`) → `401`,
  with and without `AGENTOS_AUTH_STRICT`.
- Claims whose project was defaulted rather than explicit → `403`.
- `tenant_id`, `project_id` and `actor_id` come only from claims. A request body
  containing them, or `id`, `task_iri`, `state`, `revision` → `400 field_not_allowed`.
- Reading or cancelling another tenant's or project's invocation → `404`,
  byte-identical to an unknown id.

## 4. Create request (draft)

All fields are optional except as noted. Unknown fields → `400 invalid_request`;
scope and server fields → `400 field_not_allowed` (§3).

```jsonc
{
  "prompt": "…",                 // required unless `input` or `input_ref` is set
  "agent_id": "…",               // optional server-side agent definition (§4.1)
  "agent_revision": "…",         // optional exact pin of that definition's revision; needs agent_id
  "input": { … },                // optional inline JSON, ≤ 8192 bytes serialized
  "input_ref": {                 // optional immutable reference; mutually exclusive with `input`
    "uri": "<scheme>://…",       // scheme selects a registered resolver (§4.2)
    "sha256": "<64 lowercase hex>"
  },
  "budget": {                    // optional; every present member is a positive integer
    "max_tokens": 40000,
    "max_tool_calls": 50,
    "max_cost": 2500000          // integer micro-USD (1 USD = 1_000_000), so 2.50 USD
  },
  "deadline": "2026-10-05T12:00:00Z",  // optional RFC 3339 with offset; must be in the future
  "metadata": {}                 // optional; ≤ 16 KiB serialized, ≤ 64 top-level keys
}
```

| Field | Rule | Error |
| --- | --- | --- |
| `prompt` / `input` / `input_ref` | At least one must be present | `400 invalid_request` |
| `agent_revision` | Must equal the current revision of `agent_id`; floating words (`latest`, `current`, `head`, `tip`, `active`, `default`, `*`, any case) are never resolved | Until agent definition revisions exist ([#317](https://github.com/skaiy/wild_agentos/issues/317)): any value → `422 agent_revision_unsupported`. After #317: mismatch → `409 agent_revision_mismatch`. Floating word or missing `agent_id` → `400 invalid_request` |
| `input` | Any JSON value, ≤ 8192 bytes as compact JSON | Over limit → `413 payload_too_large` |
| `input_ref` | Both `uri` and `sha256` required; `uri` is `<scheme>://…`; `sha256` is 64 lowercase hex; the scheme must have a registered resolver (§4.2) | Both `input` and `input_ref`, or a `uri` without `<scheme>://` → `400 invalid_request`; unregistered scheme → `422 input_ref_unresolvable` |
| `budget.*` | Positive integers (≥ 1); `max_cost` is micro-USD; unknown members rejected | `400 invalid_request` |
| `deadline` | RFC 3339 with offset, later than server time at create | `400 invalid_request` |
| `metadata` | JSON object, ≤ 16 KiB compact JSON, ≤ 64 top-level keys | `413 payload_too_large` |
| whole body | ≤ 64 KiB | `413 payload_too_large` |

- `agent_id` that does not resolve to a definition in the caller's scope →
  `422 agent_not_found`, identical for unknown ids and other scopes.
- Agent definitions on current `main` carry no revision. Until #317 adds
  definition revisions, any create carrying `agent_revision` is rejected with
  `422 agent_revision_unsupported` and nothing is persisted. The field is
  never silently ignored, so a caller can never believe it pinned a revision
  that the server did not check.
- `input_ref` content is fetched only by the execution bridge; if its SHA-256
  does not match, the invocation ends `failed` with
  `error.code = "input_digest_mismatch"`.
- When a budget limit is hit, execution stops and the invocation ends `failed`
  with `error.code = "budget_exceeded"`.
- When `deadline` passes, execution stops and the invocation ends `failed` with
  `error.code = "deadline_exceeded"`. This also applies to an invocation that
  is still `queued`: `queued → failed` is a conditional edge that only a
  deadline expiry (`error.code = "deadline_exceeded"`) may take; any other
  `queued → failed` request is `409 illegal_transition`.
- `metadata` and all other caller-supplied fields are echoed back unchanged in
  the resource's `request` object (`request.input`, `request.input_ref`,
  `request.metadata`, …): the server never adds, drops or rewrites keys
  or values. Every actor in the same tenant/project scope can read them, so do
  not put secrets there.
- Credentials, grants, token exchange, cross-area identity and revision-binding
  computation are not part of this API (§10). A caller that needs a binding
  computes it itself and pins it through `agent_revision`, `input_ref` and the
  digests above.

### 4.1 Agent targets and topology

- `agent_id` names a server-side agent definition in the caller's
  tenant/project scope. It may name an orchestrating definition, that is one
  executed by the Supervisor Agent as a multi-agent plan (decompose, run
  sub-agents, aggregate), not only a single agent.
- Topology (single agent or orchestrated plan, sub-agent limits, parallelism)
  is a property of the definition on the server. `agent_revision` pins the
  definition, and with it the topology; a caller cannot choose or override
  topology in the request. A `topology` or similar field is an unknown field →
  `400 invalid_request`.
- Without `agent_id` the server's default execution path is used.
- `agent_id` is the server-generated UUID returned by the agent registration
  endpoint. A caller cannot choose its own id at registration time.
- Registration fields and topology for orchestrating agents belong to
  [#317](https://github.com/skaiy/wild_agentos/issues/317).
- Making orchestrating definitions addressable by id and revision is part of
  the execution bridge
  ([#317](https://github.com/skaiy/wild_agentos/issues/317)). Until then,
  stored definitions have neither a revision nor a topology; see §11.

### 4.2 `input_ref` resolvers

- Resolvers are a pluggable registry keyed by URI scheme: `input_ref.uri` must
  be `<scheme>://…`, and the scheme selects the resolver.
- A `uri` without a scheme → `400 invalid_request`. A scheme with no
  registered resolver → `422 input_ref_unresolvable`. Both are checked at
  create, so nothing is persisted.
- v0.12.0 ships **no built-in resolver**, so every `input_ref` is `422` until a
  deployment registers one. First integrations should send inline `input`
  (≤ 8192 bytes).

## 5. Resource (draft)

```jsonc
{
  "id": "inv_…",                 // server-generated
  "object": "invocation",
  "tenant_id": "…", "project_id": "…", "actor_id": "…",
  "state": "queued",
  "revision": 1,
  "request": {                   // caller-supplied create fields, echoed unchanged
    "prompt": "…", "agent_id": null, "agent_revision": null,
    "input": null, "input_ref": null, "budget": null, "deadline": null,
    "metadata": {}
  },
  "task_iri": "iri://task_…",    // server-generated
  "result": null,                // see below once set
  "error": null,
  "idempotency_key": null,
  "created_at": "…", "updated_at": "…", "started_at": null, "completed_at": null
}
```

`result` once execution has finished:

```jsonc
{
  "summary": "…",
  "artifacts": [],
  "usage": {                     // optional; every member optional and omitted when unknown
    "provider": "…",
    "model": "…",
    "input_tokens": 1200,
    "output_tokens": 345,
    "cost": 18000,               // integer micro-USD, same unit as budget.max_cost
    "tool_calls": [{ "name": "…", "transport": "…" }]
  }
}
```

- `usage` reports the metering the server already does to enforce `budget`
  (`budget_exceeded`). It carries no partner or source attribution.
- A `failed` invocation can still carry `result` with `usage` (for example
  after `budget_exceeded`) and an empty `summary`.

## 6. Idempotency

- Scope: `(tenant_id, project_id, actor_id, Idempotency-Key)`.
- Key: 1–255 visible ASCII characters, so structured keys such as
  `<run-id>:create` are valid.
- Fingerprint: SHA-256 of the canonicalized request body, including every §4
  field (`agent_revision`, `input`, `input_ref`, `budget`, `deadline`,
  `metadata`). Headers such as `traceparent` are not part of it; a retry must
  resend the same body.
- Same key and fingerprint → `200` with the original resource and
  `Idempotent-Replayed: true`; no second execution.
- Same key, different fingerprint → `409 idempotency_key_conflict`.
- Concurrent duplicate while the first request is still being committed →
  `409 idempotency_key_in_progress` with `Retry-After`.
- Concurrent creates with one key produce exactly one resource and never a
  `5xx`.
- Registering the key and creating the resource happen in one atomic write
  before any side effect. A rejected, conflicting or invalid create leaves no
  resource, event, queue entry or execution behind.
- Idempotency records expire after a configurable TTL, default 24 h
  (`AGENTOS_INVOCATION_IDEMPOTENCY_TTL_HOURS`). After expiry the same key
  creates a new invocation.
- The TTL must not exceed the terminal-record retention (§7.1):
  `AGENTOS_INVOCATION_IDEMPOTENCY_TTL_HOURS` ≤
  `AGENTOS_INVOCATION_RETENTION_DAYS` × 24. Otherwise the server refuses to
  start (fail closed) with a configuration error naming both variables and
  their values. This guarantees a live idempotency record always points to a
  resource that still exists, so a replay never meets a swept invocation.

## 7. Lifecycle

```
queued ──► running ──► succeeded
  │  │        ├──────► failed
  │  │        └──► cancel_requested ──► cancelled
  │  │                     ├──────────► succeeded
  │  │                     └──────────► failed
  │  └──────────────────────────────► failed (deadline_exceeded only)
  └──────────────────────────────────► cancelled
```

- Terminal states: `succeeded`, `failed`, `cancelled`. Terminal results never
  change.
- `cancel_requested → succeeded | failed`: execution finished before the cancel
  took effect; the real outcome is recorded instead of being discarded.
- Every write increments `revision`. Responses carry `ETag: "<revision>"`.
- Stale `If-Match` → `409 revision_conflict`; a disallowed edge →
  `409 illegal_transition`. Malformed `If-Match` (weak tag, list, unquoted or
  non-numeric) → `400 invalid_if_match`. Absent `If-Match` or `*` skips the
  check.
- Same-state repeats are idempotent successes: a transition to the state the
  invocation is already in (a cancel on a `cancel_requested` or `cancelled`
  invocation, a duplicate worker delivery) returns the current resource with
  `2xx`, writes nothing, does not bump `revision` and ignores any new result.
  Per RFC 9110 §13.1.1 this holds even with a stale `If-Match`, because the
  requested state is already reflected.
- Moving a terminal invocation to a *different* state (for example cancelling
  a `succeeded` one) stays `409 illegal_transition`.
- On process restart, non-terminal invocations become `failed` with
  `error.code = "interrupted"`; they are not re-run automatically. Replaying
  the same `Idempotency-Key` returns that failed resource; resubmit with a new
  key.

`error.code` values on a failed invocation: `execution_failed`, `interrupted`,
`deadline_exceeded`, `budget_exceeded`, `input_digest_mismatch`.

### 7.1 Retention

- Terminal invocations are kept for a configurable retention measured from
  `completed_at`: default 7 days, `AGENTOS_INVOCATION_RETENTION_DAYS` (whole
  days, ≥ 1). After that they are removed and read as `404 not_found`.
  The retention must be at least the idempotency TTL (§6), or the server does
  not start.
- Expired terminal records are swept at startup (after restart recovery),
  inside every create, and on demand. Non-terminal invocations are never
  swept. A sweep that removes nothing writes nothing; a sweep that removes
  records uses the same atomic file replace as every other write.
- The per-process store capacity (10 000 records) counts the records left
  after the sweep. When it is still full, create → `503 invocation_store_full`.

### 7.2 Active limit

- At most 32 non-terminal invocations per tenant/project scope by default
  (`AGENTOS_INVOCATION_MAX_ACTIVE`, ≥ 1).
- Over the limit, create → `429 too_many_active` with `Retry-After: 5`, and
  nothing is persisted (no resource, no idempotency record). The count and the
  insert happen under one write lock.

## 8. Execution

- After a successful (non-replayed) create, the server creates a task with the
  caller's claims and runs it through the existing `TaskExecutor`. Execution is
  detached from the HTTP connection.
- Task events drive state transitions. A lagging SSE subscriber receives a
  `resync` event and should re-read the resource; the persisted state is
  authoritative.
- Usage is metered per run and written to `result.usage` with the terminal
  transition.
- Execution is behind a configuration switch that defaults to **off**. It must
  stay off in production until projection scoping
  ([#310](https://github.com/skaiy/wild_agentos/issues/310)) is merged.
- While the switch is off, a new create is rejected with
  `503 execution_disabled` and nothing is persisted (no resource, no
  idempotency record), so no invocation can stay non-terminal forever. A replay
  of a key registered before the switch was turned off still returns `200` and
  the existing resource. The switch is read at startup; turning it off needs a
  restart, which moves in-flight invocations to `failed/interrupted`.

### 8.1 Event stream

`GET /v1/invocations/:id/events` (`text/event-stream`). Each message has
`event:`, `id:` and one JSON `data:` line.

- `id` is `<revision>.<n>`: the resource revision when the event was emitted
  and a counter `n` within that revision, starting at 0. Order by revision,
  then `n`. Events are not replayed; on reconnect the stream starts again with
  a snapshot, so `Last-Event-ID` is ignored.
- Every `data` object carries `invocation_id`, `revision` and `at` (RFC 3339).

| `event` | When | Extra `data` members |
| --- | --- | --- |
| `state` | First event (snapshot), then every state transition | `state`, `previous_state` (null in the snapshot), `snapshot` (true only on the first event), `invocation` (full resource, snapshot only) |
| `progress` | Execution progress; not persisted, `revision` unchanged | `phase` (optional), `message` (optional, safe text) |
| `result` | Once, on `succeeded` | `state`, `result` (with `usage`) |
| `error` | Once, on `failed` | `state`, `error` (`{code, message}`), `usage` (optional) |
| `resync` | Subscriber lagged and events were dropped | none; re-read the resource |

A terminal transition sends its `state` event, then `result` (succeeded) or
`error` (failed); `cancelled` sends only `state`. The stream then closes.

```text
event: state
id: 3.0
data: {"invocation_id":"inv_…","revision":3,"at":"…","state":"succeeded","previous_state":"running","snapshot":false}

event: result
id: 3.1
data: {"invocation_id":"inv_…","revision":3,"at":"…","state":"succeeded","result":{"summary":"…","artifacts":[],"usage":{"input_tokens":1200,"output_tokens":345,"cost":18000}}}
```

## 9. Error codes (draft)

Error bodies are `{"error": "<code>", "message": "…"}`; messages are fixed,
safe texts and never echo tokens, inputs or the original request.

| Status | `error` | When |
| --- | --- | --- |
| 400 | `field_not_allowed`, `invalid_idempotency_key`, `invalid_request`, `invalid_if_match`, `idempotency_unsupported` | Scope or server fields in body; malformed key; invalid §4 field (including a schemeless `input_ref.uri`); malformed `If-Match`; `Idempotency-Key` sent before [#315](https://github.com/skaiy/wild_agentos/issues/315) (temporary code, §11) |
| 401 | `verified_isolation_claims_required` | No verified claims |
| 403 | `claims_incomplete`, `cancel_not_permitted` | Defaulted project (body may carry `missing_field`); cancel by an actor that is neither the creator nor a DA |
| 404 | `not_found` | Unknown id or another scope (identical body) |
| 409 | `idempotency_key_conflict`, `idempotency_key_in_progress`, `revision_conflict`, `illegal_transition`, `agent_revision_mismatch` | See §4, §6, §7; a `revision_conflict` body may carry `current_revision` |
| 413 | `payload_too_large` | Body > 64 KiB, `input` > 8192 bytes or `metadata` > 16 KiB / 64 keys |
| 422 | `input_ref_unresolvable`, `agent_not_found`, `agent_revision_unsupported` | `input_ref` scheme has no registered resolver; `agent_id` not found in scope; `agent_revision` sent before agent definition revisions exist (#317, §4) |
| 429 | `too_many_active` | Per-scope active limit reached (§7.2); `Retry-After: 5` |
| 500 | `persistence_failed` | Store write failed; nothing changed |
| 503 | `execution_disabled`, `invocation_store_full`, `invocation_store_unavailable` | Execution switch off (§8); store still full after the retention sweep (§7.1); invocation store not configured or unreachable |

Besides `error` and `message`, a 403 body may carry `missing_field` and a 409
body may carry `current_revision`. A successful create returns `202` with a
`Location: /v1/invocations/<id>` header; resource responses carry an `ETag`
with the current revision.

## 10. Non-goals

- No token exchange, delegation grants, cross-area or outbound identity features.
- No integrator-specific fields or compatibility keys in the public contract.
- API-client keys are not accepted as credentials in v0.12.0.
- Existing `/api/v1/tasks*` and OpenAI-compatible routes are unchanged.
- No built-in `input_ref` resolver in v0.12.0 (§4.2).
- Only the agent definition is pinned (`agent_revision`). Provider, model,
  tool, policy and context revisions are intentionally not pinned server-side
  in v0.12.0; pinning them is a possible follow-up.
- `usage` has no partner or source attribution.

## 11. Prerequisites for integrators

- **Exact agent pinning and orchestrating-agent targets require
  [#317](https://github.com/skaiy/wild_agentos/issues/317).** Both depend on
  agent definition revisions and a topology stored on the definition, which
  #317 adds. Before #317, `agent_revision` returns
  `422 agent_revision_unsupported` (§4) and an orchestrating plan cannot be
  addressed as a stored definition (§4.1). Integrations that depend on either
  should wait for #317 before switching over.
- **Execution switch.** Execution defaults to off and stays off in production
  until [#310](https://github.com/skaiy/wild_agentos/issues/310) is merged;
  until then creates return `503 execution_disabled` (§8).
- **Inputs.** v0.12.0 has no built-in `input_ref` resolver; send inline
  `input` (≤ 8192 bytes) (§4.2).
- **Idempotency requires [#315](https://github.com/skaiy/wild_agentos/issues/315).**
  Until #315 lands, a create that sends `Idempotency-Key` returns
  `400 idempotency_unsupported` (a temporary code) instead of silently
  ignoring the key. Integrations that rely on idempotent retries should wait
  for #315.
- **Switch-over prerequisites:** #315 + #317 + #310.
- **Agent ids.** Use the server-generated UUID returned by agent registration as
  `agent_id`; ids cannot be self-assigned at registration. Registration fields
  and topology for orchestrating agents come with #317 (§4.1).
- **Known inconsistency (agent registration).** `POST /api/v1/agents` still
  accepts a token whose project was filled in by default. An agent registered
  with such a token lands in the `default` project, and invocation calls with
  the same token return 403 / 422. Register agents with a token that carries
  an explicit project.
