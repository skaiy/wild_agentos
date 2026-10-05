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
| `GET /v1/invocations/:id/events` | Server-Sent Events: snapshot first, then live events, closes on a terminal state |

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
  "agent_id": "…",               // optional
  "agent_revision": "…",         // optional exact pin of the agent definition revision; needs agent_id
  "input": { … },                // optional inline JSON, ≤ 8192 bytes serialized
  "input_ref": {                 // optional immutable reference; mutually exclusive with `input`
    "uri": "…",
    "sha256": "<64 lowercase hex>"
  },
  "budget": {                    // optional; every present member is a positive integer
    "max_tokens": 40000,
    "max_tool_calls": 50,
    "max_cost": 2500000          // smallest unit of the deployment's cost accounting (e.g. micro-USD)
  },
  "deadline": "2026-10-05T12:00:00Z",  // optional RFC 3339 with offset; must be in the future
  "metadata": {}                 // optional; ≤ 16 KiB serialized, ≤ 64 top-level keys
}
```

| Field | Rule | Error |
| --- | --- | --- |
| `prompt` / `input` / `input_ref` | At least one must be present | `400 invalid_request` |
| `agent_revision` | Must equal the current revision of `agent_id`; floating words (`latest`, `current`, `head`, `tip`, `active`, `default`, `*`, any case) are never resolved | Mismatch → `409 agent_revision_mismatch`; floating word or missing `agent_id` → `400 invalid_request` |
| `input` | Any JSON value, ≤ 8192 bytes as compact JSON | Over limit → `413 payload_too_large` |
| `input_ref` | Both `uri` and `sha256` required; `sha256` is 64 lowercase hex; scheme must have a configured resolver | Both `input` and `input_ref` → `400 invalid_request`; no resolver → `422 input_ref_unresolvable` |
| `budget.*` | Positive integers (≥ 1); unknown members rejected | `400 invalid_request` |
| `deadline` | RFC 3339 with offset, later than server time at create | `400 invalid_request` |
| `metadata` | JSON object, ≤ 16 KiB compact JSON, ≤ 64 top-level keys | `413 payload_too_large` |
| whole body | ≤ 64 KiB | `413 payload_too_large` |

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
  "result": null,
  "error": null,
  "idempotency_key": null,
  "created_at": "…", "updated_at": "…", "started_at": null, "completed_at": null
}
```

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
- Records expire after a configurable TTL (default 24 h).

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

## 8. Execution

- After a successful (non-replayed) create, the server creates a task with the
  caller's claims and runs it through the existing `TaskExecutor`. Execution is
  detached from the HTTP connection.
- Task events drive state transitions. A lagging SSE subscriber receives a
  `resync` event and should re-read the resource; the persisted state is
  authoritative.
- Execution is behind a configuration switch that defaults to **off**. It must
  stay off in production until projection scoping
  ([#310](https://github.com/skaiy/wild_agentos/issues/310)) is merged.
- While the switch is off, a new create is rejected with
  `503 execution_disabled` and nothing is persisted (no resource, no
  idempotency record), so no invocation can stay non-terminal forever. A replay
  of a key registered before the switch was turned off still returns `200` and
  the existing resource. The switch is read at startup; turning it off needs a
  restart, which moves in-flight invocations to `failed/interrupted`.

## 9. Error codes (draft)

Error bodies are `{"error": "<code>", "message": "…"}`; messages are fixed,
safe texts and never echo tokens, inputs or the original request.

| Status | `error` | When |
| --- | --- | --- |
| 400 | `field_not_allowed`, `invalid_idempotency_key`, `invalid_request`, `invalid_if_match` | Scope or server fields in body; malformed key; invalid §4 field; malformed `If-Match` |
| 401 | `verified_isolation_claims_required` | No verified claims |
| 403 | `claims_incomplete` | Defaulted project |
| 404 | `not_found` | Unknown id or another scope (identical body) |
| 409 | `idempotency_key_conflict`, `idempotency_key_in_progress`, `revision_conflict`, `illegal_transition`, `agent_revision_mismatch` | See §4, §6, §7 |
| 413 | `payload_too_large` | Body > 64 KiB, `input` > 8192 bytes or `metadata` > 16 KiB / 64 keys |
| 422 | `input_ref_unresolvable` | `input_ref` scheme has no configured resolver |
| 429 | `too_many_active_invocations` | Per-scope active limit reached |
| 500 | `persistence_failed` | Store write failed; nothing changed |
| 503 | `execution_disabled`, `invocation_store_full` | Execution switch off (§8); store capacity reached |

## 10. Non-goals

- No token exchange, delegation grants, cross-area or outbound identity features.
- No integrator-specific fields or compatibility keys in the public contract.
- API-client keys are not accepted as credentials in v0.12.0.
- Existing `/api/v1/tasks*` and OpenAI-compatible routes are unchanged.
