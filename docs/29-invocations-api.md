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
| `POST /v1/invocations` | Create (optional `Idempotency-Key` header); returns `202` and the resource |
| `GET /v1/invocations` | List the caller's scope; cursor pagination; optional `state` filter |
| `GET /v1/invocations/:id` | Read one invocation |
| `POST /v1/invocations/:id/cancel` | Request cancellation; optional `If-Match: "<revision>"` |
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

## 4. Resource (draft)

```jsonc
{
  "id": "inv_…",                 // server-generated
  "object": "invocation",
  "tenant_id": "…", "project_id": "…", "actor_id": "…",
  "state": "queued",
  "revision": 1,
  "input": { "prompt": "…", "agent_id": null, "metadata": {} },
  "task_iri": "iri://task_…",    // server-generated
  "result": null,
  "error": null,
  "idempotency_key": null,
  "created_at": "…", "updated_at": "…", "started_at": null, "completed_at": null
}
```

## 5. Idempotency

- Scope: `(tenant_id, project_id, actor_id, Idempotency-Key)`.
- Fingerprint: SHA-256 of the canonicalized request body.
- Same key and fingerprint → `200` with the original resource and
  `Idempotent-Replayed: true`; no second execution.
- Same key, different fingerprint → `409 idempotency_key_conflict`.
- Concurrent duplicate while the first request is still being committed →
  `409 idempotency_key_in_progress` with `Retry-After`.
- Records expire after a configurable TTL (default 24 h).

## 6. Lifecycle

```
queued ──► running ──► succeeded
  │           ├──────► failed
  │           └──► cancel_requested ──► cancelled
  └──────────────────────────────────► cancelled
```

- Terminal states: `succeeded`, `failed`, `cancelled`.
- Every write increments `revision`. Responses carry `ETag: "<revision>"`.
- Stale `If-Match` → `409 revision_conflict`; a disallowed edge →
  `409 illegal_transition`.
- On process restart, non-terminal invocations become `failed` with
  `error.code = "interrupted"`; they are not re-run automatically.

## 7. Execution

- After a successful (non-replayed) create, the server creates a task with the
  caller's claims and runs it through the existing `TaskExecutor`. Execution is
  detached from the HTTP connection.
- Task events drive state transitions. A lagging SSE subscriber receives a
  `resync` event and should re-read the resource; the persisted state is
  authoritative.
- Execution is behind a configuration switch that defaults to **off**. It must
  stay off in production until projection scoping
  ([#310](https://github.com/skaiy/wild_agentos/issues/310)) is merged.

## 8. Error codes (draft)

| Status | `error` | When |
| --- | --- | --- |
| 400 | `field_not_allowed`, `invalid_idempotency_key` | Scope or server fields in body; malformed key |
| 401 | `verified_isolation_claims_required` | No verified claims |
| 403 | `claims_incomplete` | Defaulted project |
| 404 | `not_found` | Unknown id or another scope (identical body) |
| 409 | `idempotency_key_conflict`, `idempotency_key_in_progress`, `revision_conflict`, `illegal_transition` | See §5, §6 |
| 413 | `payload_too_large` | Body or metadata over limit |
| 429 | `too_many_active_invocations` | Per-scope active limit reached |

## 9. Non-goals

- No token exchange, delegation grants, cross-area or outbound identity features.
- No integrator-specific fields or compatibility keys in the public contract.
- API-client keys are not accepted as credentials in v0.12.0.
- Existing `/api/v1/tasks*` and OpenAI-compatible routes are unchanged.
