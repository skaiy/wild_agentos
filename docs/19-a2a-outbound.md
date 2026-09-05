# Outbound A2A adapter

Issue: [#93](https://github.com/skaiy/wild_agentos/issues/93)

## Scope and target version

This is a thin, outbound-only adapter. It does not add an inbound A2A server,
Agent Card endpoint, remote task store, streaming endpoint, or a second task
lifecycle to the kernel.

Target compatibility is A2A **1.0** (the `Major.Minor` protocol version). The
latest published specification patch reviewed for this change is v1.0.1; A2A
requires clients to negotiate with `1.0`, not a patch version.

- Specification: <https://a2a-protocol.org/v1.0.0/specification/>
- Releases: <https://github.com/a2aproject/A2A/releases>

The adapter uses the HTTP+JSON binding's minimal `SendMessage` operation:

1. `POST {endpoint}/message:send` with the local prompt as a `ROLE_USER`
   message and `returnImmediately: true`.
2. A second `POST /message:send` when the local task reaches a terminal state,
   with the local summary/output as a `ROLE_USER` follow-up. When the first
   response includes `task.id`, the follow-up includes it as `message.taskId`.

No `GetTask`, streaming, cancellation, push-notification, or Agent Card
discovery calls are made.

## Sequence and timing

```text
HTTP/SSE caller -> Wild Agent OS -> local SupervisorAgent
                       |                 |
                       |-- SendMessage --> remote A2A agent  (before local work; 15s default timeout)
                       |<-- task.id ------|                 (optional)
                       |                 |
                       |<-- local result -|                 (local lifecycle remains authoritative)
                       |-- SendMessage --> remote A2A agent  (terminal result; 15s default timeout)
```

Both remote calls are best-effort. A timeout, transport failure, or non-2xx
remote response is logged without its response body and never changes the
local task's result or SSE terminal event. This avoids making an external
agent a kernel dependency.

## Configuration and security

The flag is off by default:

```yaml
a2a:
  outbound:
    enabled: false
    endpoint: "https://partner.example/a2a"
    bearer_token: "" # deployment secret; do not commit
    timeout_seconds: 15
```

The client sends `Content-Type`/`Accept: application/a2a+json` and
`A2A-Version: 1.0`. `bearer_token`, when configured, is a remote
service-to-service credential. It is not an end-user credential.

For each request, verified local identity is copied into
`metadata.wildAgentOs.claims` as `tenantId`, `projectId`, and `actorId`.
Raw JWTs and inbound `Authorization` headers are never forwarded. The remote
agent must treat these metadata values as attributed context rather than
independently verified authorization claims; a production deployment should
use its own remote authorization policy and HTTPS endpoint.

## Follow-up

Inbound A2A platform support (Agent Cards, `/message:send`, streaming,
remote task storage, and authentication negotiation) remains explicitly out
of scope for this adapter.
