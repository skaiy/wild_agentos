> *本文是 [23-admin-control-plane-api-matrix.md](23-admin-control-plane-api-matrix.md) 的中文翻译。*

---

# 23. Admin 控制面 ↔ 内核 API 对照矩阵

这是 Admin 控制面与内核 HTTP API 的当前 `main`（v0.7.0）对照矩阵。hash 路由标识
Admin 页面，不是内核路径。“所需 claims”只陈述当前内核行为，不会因为页面名称而推断
授权策略。

## 状态词汇

- **已有** — 所列路径已注册在当前 `main`。
- **不做** — 明确不在本矩阵范围内。

## 对照矩阵

| Admin 屏 | hash 路由 | 内核 method+path | 所需 claims | 状态 |
| --- | --- | --- | --- | --- |
| Runs | `#/runs` | `GET /api/v1/tasks` | 已验证的 tenant/project `IsolationClaims`；只列出调用方持久化作用域内的任务。 | **已有** |
| Runs — 任务详情 | `#/runs` | `GET /api/v1/tasks/:task_iri`、`GET /api/v1/tasks/:task_iri/status`、`GET /api/v1/tasks/:task_iri/details`、`GET /api/v1/tasks/trends` | 已验证的 tenant/project `IsolationClaims`；详情读取要求与 Runs 列表相同的持久化任务作用域，trends 仅聚合该作用域。 | **已有** |
| Agents | `#/agents` | `GET, POST /api/v1/agents`；`PUT, DELETE /api/v1/agents/:id`；`POST /api/v1/agents/:id/chat` | 已验证的 tenant/project `IsolationClaims`；用户 Agent 仅在调用方持久化作用域内列出和变更。无作用域的平台目录仍为共享运行期元数据。 | **已有** |
| Skills | `#/skills` | `GET, POST, DELETE /api/v1/skills`；`GET /api/v1/skills/manifest`；`POST /api/v1/skills/import-git`；`GET /api/v1/skills/pipeline-runs`；`POST /api/v1/skills/pipeline-rerun` | Skill 变更（`POST`/`DELETE /api/v1/skills`、`import-git`、`pipeline-rerun`）会改动全进程共享的技能注册表，要求 `require_platform_admin`（#302）；租户 `DA` 得到 `403 platform_admin_required`。读取没有统一的 `IsolationClaims` 门禁。 | **已有** |
| KB · Ontology | `#/kb-ontology` | `GET, POST /api/v1/kb/bases`；`GET, POST /api/v1/kb/categories`；`GET, POST /api/v1/knowledge-packs`；`GET /api/v1/ontology/types`；`GET /api/v1/ontology/health` | KB 图/向量摄取、目录 CRUD 和本体写入使用已验证的 tenant/project `IsolationClaims`；缺失 claims 会 fail closed。 | **已有** |
| Isolation | `#/isolation` | 没有 create-tenant HTTP 路径。本地只读诊断：`scripts/isolation-diagnose --data-root <path>` | JWT 验证 mint tenant/project claims。诊断 CLI 不需 JWT，仍可作为只读本地导入/盘点辅助；它不是 HTTP endpoint。 | **已有** — 没有 Admin 建租户表单 |
| Keys · Models | `#/keys-models` | `GET, POST /api/v1/api-clients`；`PUT, DELETE /api/v1/api-clients/:id`；`POST, DELETE /api/v1/api-clients/:id/keys[/:kid]`；`GET /api/v1/api-audit`；`GET, PUT /api/v1/config`；`POST /api/v1/models/test`；`POST /api/v1/providers/models`；`POST /api/v1/embedding/activate` | API client、key、audit 要求 verified explicit claims 加 `DA`，按已验证 tenant 隔离；跨租户变更与不存在的记录返回相同 404。`PUT /api/v1/config`（所有配置段）和 `POST /api/v1/embedding/activate` 要求 `require_platform_admin`：经验证的 JWT 带明确非空 tenant/project、精确的 `PLATFORM_ADMIN` 角色，tenant 与非 `default` 的 `AGENTOS_PLATFORM_ADMIN_TENANT` 一致（未配置时 fail closed）；无需 `DA`。`GET /api/v1/config` 要求经验证的 JWT，并满足 `require_control_plane_da`（verified explicit tenant/project + DA）或 `require_platform_admin` 之一；响应去掉疑似密钥字段（字段名先转小写并去掉 `_`/`-` 再匹配，如 `api_key`、`accessToken`、`client_secret`、`private_key`、`authorization`、`credentials`），凭据仅以 `*_configured` 布尔值表示。Admin 配置页需要包含 tenant 和 project 的 DA token，或平台管理员 token；`POST /api/v1/models/test` 和 `POST /api/v1/providers/models` 同样要求 `require_platform_admin`（#303），并经过 provider 出站守卫（#267），见下文“Provider 探测与网关密钥”。 | **已接线** |
| Memory · 黑板 | `#/memory`（也可深链至 `#/blackboard`） | `GET /api/v1/blackboard/tasks`；`GET /api/v1/blackboard/nodes?task_iri=…` | 已验证的 tenant/project `IsolationClaims`；没有持久化作用域的历史记录不得返回。 | **已有** |
| Ops | `#/ops` | `GET /api/v1/batch/agents`；`POST /api/v1/batch/agents/:name/control`；`GET /api/v1/guard/audit`；`GET /api/v1/guard/stats`；`GET /metrics` | Batch list/control 要求已验证的 isolation claims 加 `DA`。guard audit/stats 要求已验证 tenant/project claims，使用同一作用域集合，并会脱敏敏感值。`GET /metrics` 是挂在 **HTTP API 地址**（`api.http_addr`，演示常见 `:8080` / `:8081`）上的进程全局抓取端点，**不是** `api.metrics_port`（默认 9090）——该端口没有监听（#324）。 | **已接线** — batch claims + DA |
| 在线语料 | `#/online-corpus-jobs` | `GET, POST /api/v1/online-corpus-jobs`；`GET /api/v1/online-corpus-jobs/observability`；`GET /api/v1/online-corpus-jobs/:id`；`POST /api/v1/online-corpus-jobs/:id/cancel`；`POST /api/v1/online-corpus-jobs/:id/run` | 已验证的 tenant/project `IsolationClaims`；list、read、transition、runner 和 observability 数据都有作用域。 | **已有** |
| 本体设计台 | `#/ontology-studio` | `GET, POST /api/v1/ontology/type-drafts`；`POST /api/v1/ontology/type-drafts/from-{csv,json-schema,openapi,sql-ddl,induction}`；`POST /api/v1/ontology/type-drafts/:draft_id/promote`；`POST, PUT, DELETE /api/v1/ontology/{object-types,link-types,action-types,function-defs}` | type draft 与本体写入流程要求已验证 tenant/project `IsolationClaims`；提升仍需显式且可审计。 | **已有** |
| No-Code IDE | — | — | — | **不做** |
| 第二套 Grafana | — | — | — | **不做** |
| Admin 建租户表单 | — | — | tenant 作用域来自已验证 JWT claims，不来自 Admin tenant-creation API。 | **不做** |
| Invocations（集成方 API） | — | `POST /v1/invocations`；`GET /v1/invocations`；`GET /v1/invocations/:id`；`POST /v1/invocations/:id/cancel`；`GET /v1/invocations/:id/events` | 仅已校验 JWT `IsolationClaims`（不接受 API client key、不接受 `X-Identity`）。跨 scope 读/取消/事件 → 与不存在逐字节一致的 `404`。 | **规划中** — epic [#313](https://github.com/skaiy/wild_agentos/issues/313)；设计见 [`docs/29-invocations-api.zh.md`](29-invocations-api.zh.md)；契约测试 #318 |
| 业务编排 | — | — | — | **不做** |

## 解读与边界

`/v1/invocations` 是面向集成方的**内核 API**，不属于 Admin 控制面屏，也不属于 OpenAI 兼容层（`/v1/models`、`/v1/chat/completions`，用 API client key 鉴权）。没有对应的 Admin hash 路由；客户端用已校验的 IdP JWT 直接调内核路径。见 [`docs/29-invocations-api.zh.md`](29-invocations-api.zh.md) 与 epic [#313](https://github.com/skaiy/wild_agentos/issues/313)。

v0.7.0 已交付按 claims 作用域的 Runs 列表、脱敏且按 claims 作用域的 Guard
audit/statistics，以及按 claims 作用域的黑板任务和节点浏览（[#221](https://github.com/skaiy/wild_agentos/issues/221)、
[#222](https://github.com/skaiy/wild_agentos/issues/222)、
[#223](https://github.com/skaiy/wild_agentos/issues/223) 和
[#224](https://github.com/skaiy/wild_agentos/issues/224)）。

控制面写路由先鉴权、后读取请求体（#312）：`PUT /api/v1/config`、
`POST /api/v1/models/test`、`POST /api/v1/providers/models` 和
`POST /api/v1/embedding/activate` 对未认证的调用方返回 `401`，对未通过门禁的
调用方返回 `403`，无论请求体是什么，响应字节都相同。请求体错误（schema 错误
`422`、JSON 语法错误 `400`、非 JSON content type `415`）只返回给已授权的调用方，
字段名不会泄露给其他人。

隔离诊断无需 token 是刻意设计：它是本地、只读的文件系统工具，
既不创建 tenant，也不授予 HTTP 访问。

### 矩阵之外的写路由（#302）

下列内核写路由在上表中没有单独的 Admin 屏行，但遵循同样的两道门：

- **全进程共享 → `require_platform_admin`。** `POST /api/v1/prompts`、
  `POST /api/v1/prompts/:id/activate`、`PUT /api/v1/prompts/:id/canary`、
  `DELETE /api/v1/prompts/:id`（所有租户共用一个 Prompt 注册表和一个生效版本）。
  Prompt 读取不变。
- **按租户隔离 → `require_control_plane_da`**（已验证 JWT、显式 tenant 和 project、`DA`）：
  `POST /api/v1/market/packages` 及 `.../:name/{install,rollback,upgrade}`；
  `GET, POST, DELETE /api/v1/mcp/skill-exposures`（所属 tenant 和 project 取自已验证
  claims，不取自 `X-Identity`，也不取自回落的默认 project）；
  `POST /api/v1/kb/bases/:id/reindex`。
- 其他租户的知识库、技能暴露记录或市场私有包，与不存在时返回同样的 `404`。同一 tenant
  下其他 project 的技能暴露也是如此。技能暴露请求体里的 `project_id` 会被忽略。
  没有 `project_id`、或 `project_id` 为空的已存技能暴露不可列出、不可调用、不可删除。
  再次创建只会新增一行，旧行仍留在暴露文件里。进程每次启动并加载到这些行时都会记一条
  警告；操作者通过编辑该文件删除它们。`POST /mcp` 会拒绝没有 project claim 的已验证
  token（`403 mcp_claims_incomplete`）。租户准入运行记录发布者已验证的 tenant 和
project。只有该 tenant 能暴露该 Skill。没有发布者 tenant 的运行不授权任何暴露，
拒绝响应不含 Skill 的 description 和 input schema。

### API client id 冲突与恢复

如果同一个 API client id 在 `api_clients.json` 中出现在多个 tenant 下（例如手工导入后），
系统视为归属不明，并 fail closed：

- 加载时，所有使用该 id 的 client 都被标记为 `id_conflict`，并记录一条只含 id 与 tenant id 的警告。
- 对外 API 鉴权时，只要 key 的 `client_id` 被多个 client 共用，无论状态如何都返回 `401`；
  状态为 `id_conflict` 的 client 同样返回 `401`。
- 对 `id_conflict` client 的 `PUT /api/v1/api-clients/:id` 状态变更返回 `409`。对共用 id 的
  `DELETE /api/v1/api-clients/:id` 一律以 `409` 拒绝，不做任何改动（不删 client，也不删 key）：
  只删一方会让冲突消失，使另一方租户得到本不属于它的 key。共用 id 下，client 列表只展示带调用方
  tenant 前缀的 key（两个 tenant slug 相同时不展示）。共用 id 下不带 `tenant_id` 的旧审计记录永不返回。

恢复需人工处理：先停止服务（服务会用内存数据覆盖这些文件），管理员再修改
`api_clients.json`（以及相关 key 的 `api_keys.json`），使每个 client id 只属于一个 tenant，然后在文件中把保留的 client `status` 从 `id_conflict` 改回
`active`，然后启动服务。在此之前该状态会在保存和重新加载后一直保留。

同理，在 id 仍被共用或 client 处于 `id_conflict` 时，`POST /api/v1/api-clients/:id/keys`
也以 `409` 拒绝，不写入任何 key。恢复过程中删除 client 之前，运维人员必须先撤销该 client id
下的**全部** key；否则残留的 key（尤其是不带 tenant 前缀的旧 key）可能在该 id 重新只属于一个
tenant 后变更归属。当 client id 在多个 tenant 间冲突（`id_conflict`）时，所属方仍不能
删除 client（`409`），但可以在共用 id 下撤销自己的 key（`200`；只认带本 tenant 前缀的
key，其他 key 返回与不存在的 key 相同的 `404`）。只有调用方 tenant 的 slug 归属不明
（另一个冲突 tenant 的 slug 与之相同）时，撤销才返回 `409`，此时必须先由平台管理员解决
冲突。id 仍被共用时，其下处于有效状态的 key 鉴权返回 `401`；已撤销的 key 鉴权返回
`403` `key_revoked`，与是否共用 id 无关。各 tenant 应在冲突解除前撤销自己的 key——
否则冲突解除后，这些 key 会重新变得可用。

### Provider 探测与网关密钥（#267、#303）

`POST /api/v1/models/test` 和 `POST /api/v1/providers/models` 读取全局 provider
配置，并可能使用已保存的 provider 密钥，因此要求平台管理员；租户 `DA` 在任何出站
请求之前即得到 `403 platform_admin_required`。

每个探测目标（调用方提供的或已保存的）在建立连接前都要经过 provider 出站守卫：

- 绝对 `http`/`https` URL，且不带用户凭据；
- `PROVIDER_OUTBOUND_ALLOWED_ORIGINS`（逗号分隔的 origin，例如
  `https://llm.example.test,http://10.20.0.5:3000`）一旦设置即为精确白名单；
  白名单内的 origin 可以解析到私网或回环地址；任一条目格式错误则全部拒绝。
  **白名单里的主机名可以解析到内网地址**：谁控制该域名的 DNS，谁就决定探测发往
  哪里，因此只把自己控制的域名（或 IP 字面量）加入白名单；
- 未设置白名单时只允许公网地址（文档保留网段、Teredo 地址、以及通往非公网 IPv4
  的 6to4 地址都视为非公网；十进制/八进制/十六进制的 IPv4 写法会先规范化）；`AGENTOS_AUTH_STRICT=true` 时白名单为必填，
  未设置则所有探测都被拒绝；
- link-local / 云元数据、未指定地址、组播和广播地址永远不允许，即使 origin 在
  白名单内；
- 主机名只解析一次（解析超时 5 秒），请求固定到已校验的地址并禁用代理；
  不跟随重定向；响应体上限 1 MiB。

被拒绝的目标返回 `400 provider_outbound_not_allowed`，响应体固定，绝不回显 URL。
需要探测私网或回环地址上 provider（例如本地模型服务）的部署必须把其 origin
加入白名单。

`PUT /api/v1/config` 在已配置网关密钥的情况下把 `gateway.base_url` 改到另一个
端点时，必须同时提供非空的 `gateway.api_key`；否则返回
`400 explicit_api_key_required`，不保存也不生效。已配置的密钥绝不会被带到新端点。
保持同一端点（包括等价写法）、清空 base URL、或网关未配置密钥时不受影响。

重启后同样成立。`config_override.json` 从不保存网关密钥；启动时，如果 override 中的
`gateway.base_url` 与部署配置的端点不同，且未设置 `AGENT_OS_GATEWAY_BASE_URL`，
就丢弃来自部署（`config.yaml` 或 `AGENT_OS_GATEWAY_API_KEY`）的密钥并输出告警
（告警不含密钥）。只有 override 自带的密钥才会用于 override 的端点。
`embedding.oneapi.base_url` 与 `AGENT_OS_EMBEDDING_ONEAPI_API_KEY` 适用同一规则，
重启后和 embedding 变更热切换时都成立。
如需持久地更换网关端点，请在部署中同时设置 `AGENT_OS_GATEWAY_BASE_URL` 和
`AGENT_OS_GATEWAY_API_KEY`。

配置加载器不区分键名大小写，这项检查也按同样方式读取 override：`BASE_URL`、
`OneApi.Base_Url` 和 `base_url` 是同一个字段。`PUT /api/v1/config` 的 `gateway`
和 `embedding` 段是类型化的，只接受文档列出的小写字段名；其他字段（包括大小写
不同的写法）返回 `422`，不保存也不生效。

每次加载（启动或热切换）只读取一次 `config_override.json`，密钥检查用的就是这一次
读取的结果，因此检查的端点就是加载后配置实际使用的端点。在这两段类型化之前写入的
override 可能仍有其他写法：只要 `gateway` 或 `embedding` 下有任何非小写的键，或者
同一段出现了两种写法（`embedding` 与 `Embedding`，或 `embedding.oneapi`），这一段
就一律不使用部署密钥（无论它指向哪个端点），并输出一条告警，只写段名（不含密钥、
不含路径）。文件不会被自动改写。把这一段改成小写后，部署密钥即恢复。
`gateway.model_mapping` 下的模型名不受此限制。embedding 热切换逐个串行执行，
每次把旧向量库移到各自独立的 `vector_store.bak-<时间戳>-<序号>` 目录。
文件采用原子替换：先写一个仅属主可读写（`0600`）的新文件，再重命名覆盖旧文件。

内核底层契约参见[隔离契约](17-isolation-contract.zh.md)、
[隔离矩阵](17-isolation-matrix.zh.md)、
[知识摄取](16-knowledge-ingest-import-graph.zh.md)和
[本体知识工程流水线](21-ontology-knowledge-engineering-pipeline.zh.md)。
