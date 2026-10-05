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
| `GET /v1/invocations/:id/events` | SSE：先发快照，再发实时事件，到终态关闭。**该路由及其实现归 [#317](https://github.com/skaiy/wild_agentos/issues/317)；#314 / #321 未实现。在那之前客户端轮询 `GET /v1/invocations/:id`。** |

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
  "agent_id": "…",               // 可选，服务端的 agent 定义（§4.1）
  "agent_revision": "…",         // 可选，精确钉住该定义的修订；需同时带 agent_id
  "input": { … },                // 可选，内联 JSON，序列化后 ≤ 8192 字节
  "input_ref": {                 // 可选，不可变引用；与 `input` 互斥
    "uri": "<scheme>://…",       // scheme 选择已注册的解析器（§4.2）
    "sha256": "<64 位小写十六进制>"
  },
  "budget": {                    // 可选；出现的成员必须是正整数
    "max_tokens": 40000,
    "max_tool_calls": 50,
    "max_cost": 2500000          // 整数，单位微美元（1 USD = 1_000_000），即 2.50 美元
  },
  "deadline": "2026-10-05T12:00:00Z",  // 可选，带时区偏移的 RFC 3339，必须晚于当前时间
  "metadata": {}                 // 可选；序列化后 ≤ 16 KiB，顶层键 ≤ 64 个
}
```

| 字段 | 规则 | 错误 |
| --- | --- | --- |
| `prompt` / `input` / `input_ref` | 至少出现一个 | `400 invalid_request` |
| `agent_revision` | 必须等于 `agent_id` 当前修订；浮动词（`latest`、`current`、`head`、`tip`、`active`、`default`、`*`，不分大小写）一律不解析 | agent 定义修订（[#317](https://github.com/skaiy/wild_agentos/issues/317)）落地之前：带任何值 → `422 agent_revision_unsupported`。#317 之后：不一致 → `409 agent_revision_mismatch`。浮动词或缺 `agent_id` → `400 invalid_request` |
| `input` | 任意 JSON 值，紧凑序列化 ≤ 8192 字节 | 超限 → `413 payload_too_large` |
| `input_ref` | `uri` 与 `sha256` 都必填；`uri` 形如 `<scheme>://…`；`sha256` 为 64 位小写十六进制；scheme 必须有已注册的解析器（§4.2） | 同时带 `input` 和 `input_ref`，或 `uri` 没有 `<scheme>://` → `400 invalid_request`；scheme 未注册 → `422 input_ref_unresolvable` |
| `budget.*` | 正整数（≥ 1）；`max_cost` 单位为微美元；未知成员拒绝 | `400 invalid_request` |
| `deadline` | 带时区偏移的 RFC 3339，晚于创建时的服务端时间 | `400 invalid_request` |
| `metadata` | JSON 对象，紧凑序列化 ≤ 16 KiB，顶层键 ≤ 64 个 | `413 payload_too_large` |
| 整个请求体 | ≤ 64 KiB | `413 payload_too_large` |

- `agent_id` 在调用方 scope 内找不到对应定义 → `422 agent_not_found`，不存在的 id 与其他 scope 的 id 返回相同。
- 当前 `main` 上的 agent 定义没有修订号。在 #317 补上定义修订之前，凡是带 `agent_revision` 的创建一律返回 `422 agent_revision_unsupported`，不持久化任何东西。这个字段绝不会被静默忽略，调用方不会误以为钉住了一个服务端根本没校验的修订。
- `input_ref` 的内容只由执行桥拉取；SHA-256 不一致时调用以 `failed` 结束，`error.code = "input_digest_mismatch"`。
- 触达预算上限时停止执行，调用以 `failed` 结束，`error.code = "budget_exceeded"`。
- 到达 `deadline` 时停止执行，调用以 `failed` 结束，`error.code = "deadline_exceeded"`。仍在 `queued` 的调用同样处理：`queued → failed` 是条件边，只有到期（`error.code = "deadline_exceeded"`）才能走；其他原因的 `queued → failed` 请求一律 `409 illegal_transition`。
- `metadata` 和其他调用方提供的字段都在资源的 `request` 对象里原样回显（`request.input`、`request.input_ref`、`request.metadata` 等）：服务端不增、不删、不改任何键或值。同一 tenant/project scope 内的所有 actor 都能读到，不要放敏感内容。
- 凭证、授权（grant）、token exchange、cross-area 身份和修订绑定（revision binding）计算都不属于本 API（§10）。需要绑定的调用方自行计算，并通过 `agent_revision`、`input_ref` 和上面的摘要钉住。

### 4.1 Agent 目标与拓扑

- `agent_id` 指向调用方 tenant/project scope 内的一个服务端 agent 定义。它可以是编排型定义，即由 Supervisor Agent 按多 agent 计划执行（拆解、运行子 agent、汇总）的定义，不限于单个 agent。
- 拓扑（单 agent 还是编排计划、子 agent 上限、是否并行）是服务端定义的属性。`agent_revision` 钉住定义，也就钉住了拓扑；调用方不能在请求里选择或覆盖拓扑。请求体里的 `topology` 等字段属于未知字段 → `400 invalid_request`。
- 不带 `agent_id` 时走服务端默认执行路径。
- `agent_id` 填 agent 注册接口返回的服务端生成 UUID；注册时调用方不能自己指定 id。
- 编排型 agent 的注册字段和拓扑归 [#317](https://github.com/skaiy/wild_agentos/issues/317)。
- 让编排型定义可以按 id 和修订寻址，属于执行桥（[#317](https://github.com/skaiy/wild_agentos/issues/317)）的工作。在那之前，存储的定义既没有修订也没有拓扑，见 §11。

### 4.2 `input_ref` 解析器

- 解析器是按 URI scheme 注册的可插拔注册表：`input_ref.uri` 必须是 `<scheme>://…`，由 scheme 选择解析器。
- `uri` 没有 scheme → `400 invalid_request`；scheme 没有已注册的解析器 → `422 input_ref_unresolvable`。两者都在创建时检查，什么都不落盘。
- v0.12.0 **不内置任何解析器**，部署方注册之前所有 `input_ref` 都返回 `422`。首批集成应使用内联 `input`（≤ 8192 字节）。

## 5. 资源（草案）

```jsonc
{
  "id": "inv_…",                 // 服务端生成
  "object": "invocation",
  "tenant_id": "…", "project_id": "…", "actor_id": "…",
  "state": "queued",
  "revision": 1,
  "request": {                   // 调用方提供的创建字段，原样回显
    "prompt": "…", "agent_id": null, "agent_revision": null,
    "input": null, "input_ref": null, "budget": null, "deadline": null,
    "metadata": {}
  },
  "task_iri": "iri://task_…",    // 服务端生成
  "result": null,                // 写入后的形状见下
  "error": null,
  "idempotency_key": null,
  "created_at": "…", "updated_at": "…", "started_at": null, "completed_at": null
}
```

执行结束后的 `result`：

```jsonc
{
  "summary": "…",
  "artifacts": [],
  "usage": {
    "provider": "…",             // 可选
    "model": "…",                // status = succeeded 时必填
    "input_tokens": 1200,        // status = succeeded 时必填
    "output_tokens": 345,        // status = succeeded 时必填
    "cost": 18000,               // status = succeeded 时必填；整数微美元
    "tool_calls": [{ "name": "…", "transport": "mcp" }]  // 可选；transport 见下
  }
}
```

- **成功态 usage（VAL-016 / VAL-017）。** 当 `status` 为 `succeeded` 时，`result.usage` **必须**存在，且包含非空 `model`、`input_tokens`、`output_tokens` 以及整数 `cost`（微美元，与 `budget.max_cost` 同单位）。缺任一项就**不得**记为 `succeeded`（失败关闭：迁到 `failed` 且 `error.code = "incomplete_usage"`，或拒绝该终态写入）。`provider` 与 `tool_calls` 在所有终态上仍可选。
- 在 `failed` / `cancelled` / `interrupted` 路径上，`usage` 可选；若带了，结构仍须合法（未知成员拒绝；有 `cost` 时必须是整数）。`failed` 的调用也可以带 `result`，其中只有 `usage`（例如 `budget_exceeded` 之后），`summary` 为空。
- `usage` 报告的是服务端为执行 `budget`（`budget_exceeded`）本来就在做的计量，不含任何合作方或来源归因。
- **`tool_calls[].transport` 词表（闭集）。** 每条 tool-call 可带 `transport`，取值只能是：
  `mcp` | `http` | `a2a` | `local` | `unknown`。
  - `mcp` — 经 MCP 服务绑定调用的工具。
  - `http` — 直连 HTTP 工具调用（**不是** A2A）。
  - `a2a` — 走 A2A 出站路径的工具调用；**不要用 `http` 兼指 A2A**。A2A 调用使用独立取值 `transport = "a2a"`。
  - `local` — 进程内 / 内置工具，无网络跳转。
  - `unknown` — 无法判定时使用；已知时优先写明确取值。闭集以外的值在写入时拒绝。

### 5.1 列表响应

`GET /v1/invocations` 返回调用方 tenant/project scope 内的游标分页（该 scope 内任意 actor 可读；按 `created_at` 新到旧，`id` 作并列键）：

```jsonc
{
  "object": "list",
  "data": [ /* 与 §5 相同的 Invocation 资源；不含 audit_events */ ],
  "has_more": true,
  "next_cursor": "…"             // 不透明；has_more 为 false 时省略 / null
}
```

| 查询参数 | 规则 |
| --- | --- |
| `limit` | 可选整数 1–100；默认 **20** |
| `state` | 可选，精确匹配生命周期状态（`queued`、`running` 等）；未知 → `400 invalid_request` |
| `after` | 上一页 `next_cursor` 的不透明游标；格式错误 → `400 invalid_request` |

跨 scope 的行不会出现。匿名 → `401`；project 为默认补全 → `403`。

## 6. 幂等

- 作用域：`(tenant_id, project_id, actor_id, Idempotency-Key)`。
- key：1–255 个可见 ASCII 字符，`<run-id>:create` 这类带冒号的结构化 key 合法。
- 指纹：规范化请求体的 SHA-256，包含 §4 的全部字段（`agent_revision`、`input`、`input_ref`、`budget`、`deadline`、`metadata`）。`traceparent` 等请求头不参与；重试必须原样重发同一个请求体。
- 同 key 同指纹 → `200` + 原资源 + `Idempotent-Replayed: true`，不再执行。
- 同 key 不同指纹 → `409 idempotency_key_conflict`。
- 第一个请求还在提交时来了并发重复请求 → `409 idempotency_key_in_progress`，带 `Retry-After`。
- 同一个 key 的并发创建只产生一个资源，不出现 `5xx`。
- 登记 key 与创建资源在同一次原子写入里完成，且先于任何副作用。被拒绝、冲突或非法的创建不留下任何资源、事件、队列项或执行。
- 幂等记录按可配置 TTL 过期，默认 24 小时（`AGENTOS_INVOCATION_IDEMPOTENCY_TTL_HOURS`）。过期后同一个 key 会创建新的调用。
- TTL 不得超过终态记录的保留期（§7.1）：`AGENTOS_INVOCATION_IDEMPOTENCY_TTL_HOURS` ≤ `AGENTOS_INVOCATION_RETENTION_DAYS` × 24。否则服务端拒绝启动（fail closed），并给出写明两个变量及其取值的配置错误。这样仍有效的幂等记录指向的资源一定还在，重放不会碰到已被清理的调用。

## 7. 生命周期

```
queued ──► running ──► succeeded
  │  │        ├──────► failed
  │  │        └──► cancel_requested ──► cancelled
  │  │                     ├──────────► succeeded
  │  │                     └──────────► failed
  │  └──────────────────────────────► failed（仅 deadline_exceeded）
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

### 7.1 保留与清理

- 终态调用从 `completed_at` 起按可配置的保留期保存：默认 7 天，`AGENTOS_INVOCATION_RETENTION_DAYS`（整天数，≥ 1）。过期后删除，再读返回 `404 not_found`。保留期必须不短于幂等 TTL（§6），否则服务端不启动。
- 过期的终态记录在启动时（重启恢复之后）、每次创建时以及按需清理。非终态调用永不清理。没有删掉任何记录的清理不写盘；删掉记录的清理与其他写入一样走原子文件替换。
- 每进程存储容量（10 000 条）按清理后剩下的记录计算。清理后仍满时，创建 → `503 invocation_store_full`。

### 7.2 活跃上限

- 每个 tenant/project scope 默认最多 32 个非终态调用（`AGENTOS_INVOCATION_MAX_ACTIVE`，≥ 1）。
- 超限时创建 → `429 too_many_active`，带 `Retry-After: 5`，什么都不落盘（没有资源，也没有幂等记录）。计数和写入在同一把写锁里完成。

## 8. 执行

- 创建成功（且不是幂等重放）后，服务端用调用方 claims 建任务，交给现有 `TaskExecutor` 执行。执行与 HTTP 连接解耦。
- 任务事件驱动状态迁移。SSE 订阅者跟不上时收到 `resync` 事件，应重新读取资源；以持久化状态为准。
- 每次运行都计量，用量随终态迁移写入 `result.usage`。落到 `succeeded` 时必须满足 §5 的完整 usage（VAL-016 / VAL-017）；不完整的 usage 不得落成 `succeeded`。
- 执行受配置开关控制，默认**关闭**。投影按 scope 绑定（[#310](https://github.com/skaiy/wild_agentos/issues/310)）合入前，生产必须保持关闭。
- 开关关闭时，新的创建请求返回 `503 execution_disabled`，什么都不落盘（没有资源，也没有幂等记录），因此不会有调用永远停在非终态。开关关闭前已登记的 key 重放仍返回 `200` 和原资源。开关在启动时读取；关闭开关需要重启，重启会把执行中的调用改为 `failed/interrupted`。

### 8.1 事件流

`GET /v1/invocations/:id/events`（`text/event-stream`）。每条消息有 `event:`、`id:` 和一行 JSON `data:`。

- `id` 为 `<revision>.<n>`：发出事件时资源的 revision，加上该 revision 内从 0 开始的计数 `n`。先按 revision、再按 `n` 排序。事件不重放；重连后从快照重新开始，因此忽略 `Last-Event-ID`。
- 每个 `data` 对象都带 `invocation_id`、`revision` 和 `at`（RFC 3339）。

| `event` | 何时 | `data` 额外成员 |
| --- | --- | --- |
| `state` | 第一条（快照），之后每次状态迁移 | `state`、`previous_state`（快照中为 null）、`snapshot`（仅第一条为 true）、`invocation`（完整资源，仅快照） |
| `progress` | 执行进度；不落盘，`revision` 不变 | `phase`（可选）、`message`（可选，安全文案） |
| `result` | 到达 `succeeded` 时发一次 | `state`、`result`（含 `usage`） |
| `error` | 到达 `failed` 时发一次 | `state`、`error`（`{code, message}`）、`usage`（可选） |
| `resync` | 订阅者跟不上，事件被丢弃 | 无；请重新读取资源 |

终态迁移先发 `state`，再发 `result`（succeeded）或 `error`（failed）；`cancelled` 只发 `state`。之后流关闭。

```text
event: state
id: 3.0
data: {"invocation_id":"inv_…","revision":3,"at":"…","state":"succeeded","previous_state":"running","snapshot":false}

event: result
id: 3.1
data: {"invocation_id":"inv_…","revision":3,"at":"…","state":"succeeded","result":{"summary":"…","artifacts":[],"usage":{"input_tokens":1200,"output_tokens":345,"cost":18000}}}
```

## 9. 错误码（草案）

错误体为 `{"error": "<code>", "message": "…"}`；message 是固定的安全文案，不回显令牌、输入或原请求。

| 状态码 | `error` | 场景 |
| --- | --- | --- |
| 400 | `field_not_allowed`、`invalid_idempotency_key`、`invalid_request`、`invalid_if_match`、`idempotency_unsupported` | 请求体带 scope 或服务端字段；key 格式非法；§4 字段非法（包括没有 scheme 的 `input_ref.uri`）；`If-Match` 格式错误；[#315](https://github.com/skaiy/wild_agentos/issues/315) 之前带了 `Idempotency-Key`（临时码，见 §11） |
| 401 | `verified_isolation_claims_required` | 没有已校验 claims |
| 403 | `claims_incomplete`、`cancel_not_permitted` | project 为默认值（body 可带 `missing_field`）；既不是创建者也不是 DA 的 actor 发起取消 |
| 404 | `not_found` | id 不存在或属于其他 scope（body 相同） |
| 409 | `idempotency_key_conflict`、`idempotency_key_in_progress`、`revision_conflict`、`illegal_transition`、`agent_revision_mismatch` | 见 §4、§6、§7；`revision_conflict` 的 body 可带 `current_revision` |
| 413 | `payload_too_large` | 请求体 > 64 KiB、`input` > 8192 字节或 `metadata` > 16 KiB / 64 个键 |
| 422 | `input_ref_unresolvable`、`agent_not_found`、`agent_revision_unsupported` | `input_ref` 的 scheme 没有已注册的解析器；`agent_id` 在 scope 内不存在；agent 定义修订（#317）落地前请求带了 `agent_revision`（§4） |
| 429 | `too_many_active` | 达到 scope 内活跃调用上限（§7.2）；带 `Retry-After: 5` |
| 500 | `persistence_failed` | 存储写入失败，状态未改变 |
| 503 | `execution_disabled`、`invocation_store_full`、`invocation_store_unavailable` | 执行开关关闭（§8）；保留期清理后存储仍满（§7.1）；调用存储未配置或不可用 |

除 `error` 和 `message` 外，403 的 body 可带 `missing_field`，409 `revision_conflict` 的 body 可带 `current_revision`。**标识当前资源的 409 响应同时带 `ETag: "<revision>"`**（至少 `revision_conflict`，与 `current_revision` 一致）。创建成功返回 `202` 并带 `Location: /v1/invocations/<id>` 头；2xx 资源响应带当前修订对应的 `ETag`。

## 10. 不在范围

- 不做 token exchange、委托授权、cross-area 或出站身份能力。
- 公开契约里不放集成方特有字段或兼容键。
- v0.12.0 不接受 API client key 作为凭证。
- 现有 `/api/v1/tasks*` 和 OpenAI 兼容路由不变。
- v0.12.0 不内置 `input_ref` 解析器（§4.2）。
- 只钉住 agent 定义（`agent_revision`）。provider、model、工具、策略和上下文的修订在 v0.12.0 有意不在服务端钉住，以后可另开 follow-up。
- `usage` 不含合作方或来源归因。

## 11. 集成方前提

- **精确钉住 agent 和以编排型 agent 为目标，都依赖 [#317](https://github.com/skaiy/wild_agentos/issues/317)。** 两者都需要 agent 定义修订和存储在定义上的拓扑，这由 #317 补上。#317 之前，`agent_revision` 返回 `422 agent_revision_unsupported`（§4），编排计划也不能作为存储的定义来寻址（§4.1）。依赖其中任一项的集成应等 #317 合入后再切换。
- **执行开关。** 执行默认关闭，在 [#310](https://github.com/skaiy/wild_agentos/issues/310) 合入前生产环境保持关闭；在此之前创建返回 `503 execution_disabled`（§8）。
- **输入。** v0.12.0 不内置 `input_ref` 解析器，请使用内联 `input`（≤ 8192 字节）（§4.2）。
- **幂等依赖 [#315](https://github.com/skaiy/wild_agentos/issues/315)。** #315 之前，带 `Idempotency-Key` 的创建请求返回 `400 idempotency_unsupported`（临时码），而不是静默忽略该 key。依赖幂等重试的集成应等 #315 合入。
- **切换前提：** #315 + #317 + #310。
- **Agent id。** `agent_id` 使用 agent 注册接口返回的服务端生成 UUID，注册时不能自指定 id。编排型 agent 的注册字段和拓扑随 #317 提供（§4.1）。
- **已知不一致（agent 注册）。** `POST /api/v1/agents` 仍接受 project 为默认补全值的令牌。用这种令牌注册的 agent 会落到 `default` project。**同一张 defaulted 令牌**访问任意 `/v1/invocations` 路由一律返回 **`403 claims_incomplete`**（body 可带 `missing_field: "project_id"`）。换成带显式 project 的令牌后再查那个落在 `default` 的 agent，会得到 **`422 agent_not_found`**。注册 agent 时请直接使用带显式 project 的令牌。

## 12. 文档与矩阵收尾（#318）

- `docs/23` 矩阵已补 Invocations 行，并在「解读与边界」写明：本资源不属于 Admin 屏、不属于 OpenAI 兼容层。
- `docs/17` 隔离矩阵已补跨 scope 404 / 匿名 401 行。
- 路由级契约测试索引见 `src/api/http/isolation_contract_invocations_tests.rs`（随 #314/#317 栈落地）。
