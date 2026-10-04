# 29. Invocations API（设计草案）

> *English version: [29-invocations-api.md](29-invocations-api.md).*

> **状态：设计草案，尚未实现。** 跟踪 epic：
> [#313](https://github.com/skaiy/wild_agentos/issues/313)，里程碑 v0.12.0。
> 字段名和错误码在子 issue 落地前可能调整。本文不描述当前 `main` 的任何行为。

## 1. 目的

下游集成方需要一个持久、可重试、可查询的调用入口：提交一次调用，拿到一个持久资源，轮询或订阅它的状态和结果，必要时取消。现有的 `POST /api/v1/tasks` 只建任务节点，`POST /api/v1/tasks/stream` 把结果绑定在一条 HTTP 连接上。两者都没有幂等键、显式生命周期、取消和并发写保护。

Invocations API 原生建立在现有 claims-only 身份栈上（由已校验的 IdP JWT Bearer 生成 `IsolationClaims`），不新增第二套身份或令牌体系。

## 2. 路由

| Method + path | 作用 |
| --- | --- |
| `POST /v1/invocations` | 创建（可带 `Idempotency-Key` 头），返回 `202` 和资源 |
| `GET /v1/invocations` | 列出调用方 scope 内的调用；游标分页；可按 `state` 过滤 |
| `GET /v1/invocations/:id` | 读取单个调用 |
| `POST /v1/invocations/:id/cancel` | 请求取消；可带 `If-Match: "<revision>"` |
| `GET /v1/invocations/:id/events` | SSE：先发快照，再发实时事件，到终态关闭 |

这些路由在 `/v1/` 下，但**不属于** OpenAI 兼容层（`/v1/models`、`/v1/chat/completions`，用 API client key 鉴权）。Invocations 只接受已校验的 JWT claims。

## 3. 鉴权与 scope

- 没有已校验 claims（匿名、JWT 无效或过期、`X-Identity`）→ `401`，`AGENTOS_AUTH_STRICT` 开关两种情况相同。
- project 是默认补出来、不是显式声明的 claims → `403`。
- `tenant_id`、`project_id`、`actor_id` 只来自 claims。请求体里出现这些字段，或 `id`、`task_iri`、`state`、`revision` → `400 field_not_allowed`。
- 读取或取消其他租户、其他项目的调用 → `404`，与不存在的 id 逐字节一致。

## 4. 资源（草案）

```jsonc
{
  "id": "inv_…",                 // 服务端生成
  "object": "invocation",
  "tenant_id": "…", "project_id": "…", "actor_id": "…",
  "state": "queued",
  "revision": 1,
  "input": { "prompt": "…", "agent_id": null, "metadata": {} },
  "task_iri": "iri://task_…",    // 服务端生成
  "result": null,
  "error": null,
  "idempotency_key": null,
  "created_at": "…", "updated_at": "…", "started_at": null, "completed_at": null
}
```

## 5. 幂等

- 作用域：`(tenant_id, project_id, actor_id, Idempotency-Key)`。
- 指纹：规范化请求体的 SHA-256。
- 同 key 同指纹 → `200` + 原资源 + `Idempotent-Replayed: true`，不再执行。
- 同 key 不同指纹 → `409 idempotency_key_conflict`。
- 第一个请求还在提交时来了并发重复请求 → `409 idempotency_key_in_progress`，带 `Retry-After`。
- 记录按可配置 TTL 过期（默认 24 小时）。

## 6. 生命周期

```
queued ──► running ──► succeeded
  │           ├──────► failed
  │           └──► cancel_requested ──► cancelled
  └──────────────────────────────────► cancelled
```

- 终态：`succeeded`、`failed`、`cancelled`。
- 每次写入 `revision` 加一，响应带 `ETag: "<revision>"`。
- `If-Match` 过期 → `409 revision_conflict`；不允许的迁移 → `409 illegal_transition`。
- 进程重启时，未到终态的调用改为 `failed`，`error.code = "interrupted"`，不自动重跑。

## 7. 执行

- 创建成功（且不是幂等重放）后，服务端用调用方 claims 建任务，交给现有 `TaskExecutor` 执行。执行与 HTTP 连接解耦。
- 任务事件驱动状态迁移。SSE 订阅者跟不上时收到 `resync` 事件，应重新读取资源；以持久化状态为准。
- 执行受配置开关控制，默认**关闭**。投影按 scope 绑定（[#310](https://github.com/skaiy/wild_agentos/issues/310)）合入前，生产必须保持关闭。

## 8. 错误码（草案）

| 状态码 | `error` | 场景 |
| --- | --- | --- |
| 400 | `field_not_allowed`、`invalid_idempotency_key` | 请求体带 scope 或服务端字段；key 格式非法 |
| 401 | `verified_isolation_claims_required` | 没有已校验 claims |
| 403 | `claims_incomplete` | project 为默认值 |
| 404 | `not_found` | id 不存在或属于其他 scope（body 相同） |
| 409 | `idempotency_key_conflict`、`idempotency_key_in_progress`、`revision_conflict`、`illegal_transition` | 见 §5、§6 |
| 413 | `payload_too_large` | 请求体或 metadata 超限 |
| 429 | `too_many_active_invocations` | 达到 scope 内活跃调用上限 |

## 9. 不在范围

- 不做 token exchange、委托授权、cross-area 或出站身份能力。
- 公开契约里不放集成方特有字段或兼容键。
- v0.12.0 不接受 API client key 作为凭证。
- 现有 `/api/v1/tasks*` 和 OpenAI 兼容路由不变。
