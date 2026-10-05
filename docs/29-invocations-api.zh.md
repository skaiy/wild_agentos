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
| `POST /v1/invocations` | 创建（可带 `Idempotency-Key` 头），返回 `202` 和资源（幂等重放返回 `200`） |
| `GET /v1/invocations` | 列出调用方 scope 内的调用；游标分页；可按 `state` 过滤 |
| `GET /v1/invocations/:id` | 读取单个调用 |
| `POST /v1/invocations/:id/cancel` | 请求取消；可带 `If-Match: "<revision>"`；结果为 `cancelled` 时返回 `200`，为 `cancel_requested` 时返回 `202` |
| `GET /v1/invocations/:id/events` | SSE：先发快照，再发实时事件，到终态关闭 |

这些路由在 `/v1/` 下，但**不属于** OpenAI 兼容层（`/v1/models`、`/v1/chat/completions`，用 API client key 鉴权）。Invocations 只接受已校验的 JWT claims。

## 3. 鉴权与 scope

- 没有已校验 claims（匿名、JWT 无效或过期、`X-Identity`）→ `401`，`AGENTOS_AUTH_STRICT` 开关两种情况相同。
- project 是默认补出来、不是显式声明的 claims → `403`。
- `tenant_id`、`project_id`、`actor_id` 只来自 claims。请求体里出现这些字段，或 `id`、`task_iri`、`state`、`revision` → `400 field_not_allowed`。
- 读取或取消其他租户、其他项目的调用 → `404`，与不存在的 id 逐字节一致。

## 4. 创建请求（草案）

除注明外都是可选字段。未知字段 → `400 invalid_request`；scope 或服务端字段 → `400 field_not_allowed`（§3）。

```jsonc
{
  "prompt": "…",                 // 没有 `input` 和 `input_ref` 时必填
  "agent_id": "…",               // 可选
  "agent_revision": "…",         // 可选，精确钉住 agent 定义的修订；需同时带 agent_id
  "input": { … },                // 可选，内联 JSON，序列化后 ≤ 8192 字节
  "input_ref": {                 // 可选，不可变引用；与 `input` 互斥
    "uri": "…",
    "sha256": "<64 位小写十六进制>"
  },
  "budget": {                    // 可选；出现的成员必须是正整数
    "max_tokens": 40000,
    "max_tool_calls": 50,
    "max_cost": 2500000          // 部署成本核算的最小单位（如微美元）
  },
  "deadline": "2026-10-05T12:00:00Z",  // 可选，带时区偏移的 RFC 3339，必须晚于当前时间
  "metadata": {}                 // 可选；序列化后 ≤ 16 KiB，顶层键 ≤ 64 个
}
```

| 字段 | 规则 | 错误 |
| --- | --- | --- |
| `prompt` / `input` / `input_ref` | 至少出现一个 | `400 invalid_request` |
| `agent_revision` | 必须等于 `agent_id` 当前修订；浮动词（`latest`、`current`、`head`、`tip`、`active`、`default`、`*`，不分大小写）一律不解析 | 不一致 → `409 agent_revision_mismatch`；浮动词或缺 `agent_id` → `400 invalid_request` |
| `input` | 任意 JSON 值，紧凑序列化 ≤ 8192 字节 | 超限 → `413 payload_too_large` |
| `input_ref` | `uri` 与 `sha256` 都必填；`sha256` 为 64 位小写十六进制；scheme 必须有已配置的解析器 | 同时带 `input` 和 `input_ref` → `400 invalid_request`；没有解析器 → `422 input_ref_unresolvable` |
| `budget.*` | 正整数（≥ 1）；未知成员拒绝 | `400 invalid_request` |
| `deadline` | 带时区偏移的 RFC 3339，晚于创建时的服务端时间 | `400 invalid_request` |
| `metadata` | JSON 对象，紧凑序列化 ≤ 16 KiB，顶层键 ≤ 64 个 | `413 payload_too_large` |
| 整个请求体 | ≤ 64 KiB | `413 payload_too_large` |

- `input_ref` 的内容只由执行桥拉取；SHA-256 不一致时调用以 `failed` 结束，`error.code = "input_digest_mismatch"`。
- 触达预算上限时停止执行，调用以 `failed` 结束，`error.code = "budget_exceeded"`。
- 到达 `deadline` 时停止执行，调用以 `failed` 结束，`error.code = "deadline_exceeded"`。仍在 `queued` 的调用同样处理（`queued → failed` 这条边随执行桥加入）。
- `metadata` 和其他调用方提供的字段都在资源的 `input` 对象里原样回显：服务端不增、不删、不改任何键或值。同一 tenant/project scope 内的所有 actor 都能读到，不要放敏感内容。
- 凭证、授权（grant）、token exchange、cross-area 身份和修订绑定（revision binding）计算都不属于本 API（§10）。需要绑定的调用方自行计算，并通过 `agent_revision`、`input_ref` 和上面的摘要钉住。

## 5. 资源（草案）

```jsonc
{
  "id": "inv_…",                 // 服务端生成
  "object": "invocation",
  "tenant_id": "…", "project_id": "…", "actor_id": "…",
  "state": "queued",
  "revision": 1,
  "input": {                     // 调用方提供的创建字段，原样回显
    "prompt": "…", "agent_id": null, "agent_revision": null,
    "input": null, "input_ref": null, "budget": null, "deadline": null,
    "metadata": {}
  },
  "task_iri": "iri://task_…",    // 服务端生成
  "result": null,
  "error": null,
  "idempotency_key": null,
  "created_at": "…", "updated_at": "…", "started_at": null, "completed_at": null
}
```

## 6. 幂等

- 作用域：`(tenant_id, project_id, actor_id, Idempotency-Key)`。
- key：1–255 个可见 ASCII 字符，`<run-id>:create` 这类带冒号的结构化 key 合法。
- 指纹：规范化请求体的 SHA-256，包含 §4 的全部字段（`agent_revision`、`input`、`input_ref`、`budget`、`deadline`、`metadata`）。`traceparent` 等请求头不参与；重试必须原样重发同一个请求体。
- 同 key 同指纹 → `200` + 原资源 + `Idempotent-Replayed: true`，不再执行。
- 同 key 不同指纹 → `409 idempotency_key_conflict`。
- 第一个请求还在提交时来了并发重复请求 → `409 idempotency_key_in_progress`，带 `Retry-After`。
- 同一个 key 的并发创建只产生一个资源，不出现 `5xx`。
- 登记 key 与创建资源在同一次原子写入里完成，且先于任何副作用。被拒绝、冲突或非法的创建不留下任何资源、事件、队列项或执行。
- 记录按可配置 TTL 过期（默认 24 小时）。

## 7. 生命周期

```
queued ──► running ──► succeeded
  │  │        ├──────► failed
  │  │        └──► cancel_requested ──► cancelled
  │  │                     ├──────────► succeeded
  │  │                     └──────────► failed
  │  └──────────────────────────────► failed（仅 deadline）
  └──────────────────────────────────► cancelled
```

- 终态：`succeeded`、`failed`、`cancelled`。终态结果不再改变。
- `cancel_requested → succeeded | failed`：取消生效前执行已结束，记录真实结果，不丢弃。
- 每次写入 `revision` 加一，响应带 `ETag: "<revision>"`。
- `If-Match` 过期 → `409 revision_conflict`；不允许的迁移 → `409 illegal_transition`。`If-Match` 格式错误（弱标签、列表、无引号或非数字）→ `400 invalid_if_match`。不带 `If-Match` 或为 `*` 时不检查。
- 同态重复迁移是幂等成功：目标状态等于当前状态（对 `cancel_requested` 或 `cancelled` 的调用再次取消、worker 重复投递）时，返回当前资源和 `2xx`，不写入、不增加 `revision`，忽略新带的结果。按 RFC 9110 §13.1.1，即使 `If-Match` 已过期也如此，因为请求的状态已经达成。
- 终态迁到*另一个*状态（例如取消已 `succeeded` 的调用）仍是 `409 illegal_transition`。
- 进程重启时，未到终态的调用改为 `failed`，`error.code = "interrupted"`，不自动重跑。用同一个 `Idempotency-Key` 重放只会拿到这个 failed 资源；要重新提交请换新 key。

失败调用的 `error.code` 取值：`execution_failed`、`interrupted`、`deadline_exceeded`、`budget_exceeded`、`input_digest_mismatch`。

## 8. 执行

- 创建成功（且不是幂等重放）后，服务端用调用方 claims 建任务，交给现有 `TaskExecutor` 执行。执行与 HTTP 连接解耦。
- 任务事件驱动状态迁移。SSE 订阅者跟不上时收到 `resync` 事件，应重新读取资源；以持久化状态为准。
- 执行受配置开关控制，默认**关闭**。投影按 scope 绑定（[#310](https://github.com/skaiy/wild_agentos/issues/310)）合入前，生产必须保持关闭。
- 开关关闭时，新的创建请求返回 `503 execution_disabled`，什么都不落盘（没有资源，也没有幂等记录），因此不会有调用永远停在非终态。开关关闭前已登记的 key 重放仍返回 `200` 和原资源。开关在启动时读取；关闭开关需要重启，重启会把执行中的调用改为 `failed/interrupted`。

## 9. 错误码（草案）

错误体为 `{"error": "<code>", "message": "…"}`；message 是固定的安全文案，不回显令牌、输入或原请求。

| 状态码 | `error` | 场景 |
| --- | --- | --- |
| 400 | `field_not_allowed`、`invalid_idempotency_key`、`invalid_request`、`invalid_if_match` | 请求体带 scope 或服务端字段；key 格式非法；§4 字段非法；`If-Match` 格式错误 |
| 401 | `verified_isolation_claims_required` | 没有已校验 claims |
| 403 | `claims_incomplete` | project 为默认值 |
| 404 | `not_found` | id 不存在或属于其他 scope（body 相同） |
| 409 | `idempotency_key_conflict`、`idempotency_key_in_progress`、`revision_conflict`、`illegal_transition`、`agent_revision_mismatch` | 见 §4、§6、§7 |
| 413 | `payload_too_large` | 请求体 > 64 KiB、`input` > 8192 字节或 `metadata` > 16 KiB / 64 个键 |
| 422 | `input_ref_unresolvable` | `input_ref` 的 scheme 没有已配置的解析器 |
| 429 | `too_many_active_invocations` | 达到 scope 内活跃调用上限 |
| 500 | `persistence_failed` | 存储写入失败，状态未改变 |
| 503 | `execution_disabled`、`invocation_store_full` | 执行开关关闭（§8）；存储容量已满 |

## 10. 不在范围

- 不做 token exchange、委托授权、cross-area 或出站身份能力。
- 公开契约里不放集成方特有字段或兼容键。
- v0.12.0 不接受 API client key 作为凭证。
- 现有 `/api/v1/tasks*` 和 OpenAI 兼容路由不变。
