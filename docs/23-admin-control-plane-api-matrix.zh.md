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
| Skills | `#/skills` | `GET, POST, DELETE /api/v1/skills`；`GET /api/v1/skills/manifest`；`POST /api/v1/skills/import-git`；`GET /api/v1/skills/pipeline-runs`；`POST /api/v1/skills/pipeline-rerun` | Skill 变更要求 `DA`；读取没有统一的 `IsolationClaims` 门禁。 | **已有** |
| KB · Ontology | `#/kb-ontology` | `GET, POST /api/v1/kb/bases`；`GET, POST /api/v1/kb/categories`；`GET, POST /api/v1/knowledge-packs`；`GET /api/v1/ontology/types`；`GET /api/v1/ontology/health` | KB 图/向量摄取、目录 CRUD 和本体写入使用已验证的 tenant/project `IsolationClaims`；缺失 claims 会 fail closed。 | **已有** |
| Isolation | `#/isolation` | 没有 create-tenant HTTP 路径。本地只读诊断：`scripts/isolation-diagnose --data-root <path>` | JWT 验证 mint tenant/project claims。诊断 CLI 不需 JWT，仍可作为只读本地导入/盘点辅助；它不是 HTTP endpoint。 | **已有** — 没有 Admin 建租户表单 |
| Keys · Models | `#/keys-models` | `GET, POST /api/v1/api-clients`；`PUT, DELETE /api/v1/api-clients/:id`；`POST, DELETE /api/v1/api-clients/:id/keys[/:kid]`；`GET /api/v1/api-audit`；`GET, PUT /api/v1/config`；`POST /api/v1/models/test`；`POST /api/v1/providers/models`；`POST /api/v1/embedding/activate` | API client、audit、config、模型测试/发现和 embedding 激活均要求已验证的 isolation claims 加 `DA`。 | **已接线** — claims + DA |
| Memory · 黑板 | `#/memory`（也可深链至 `#/blackboard`） | `GET /api/v1/blackboard/tasks`；`GET /api/v1/blackboard/nodes?task_iri=…` | 已验证的 tenant/project `IsolationClaims`；没有持久化作用域的历史记录不得返回。 | **已有** |
| Ops | `#/ops` | `GET /api/v1/batch/agents`；`POST /api/v1/batch/agents/:name/control`；`GET /api/v1/guard/audit`；`GET /api/v1/guard/stats`；`GET /metrics` | Batch list/control 要求已验证的 isolation claims 加 `DA`。guard audit/stats 要求已验证 tenant/project claims，使用同一作用域集合，并会脱敏敏感值。`/metrics` 保持进程全局抓取端点。 | **已接线** — batch claims + DA |
| 在线语料 | `#/online-corpus-jobs` | `GET, POST /api/v1/online-corpus-jobs`；`GET /api/v1/online-corpus-jobs/observability`；`GET /api/v1/online-corpus-jobs/:id`；`POST /api/v1/online-corpus-jobs/:id/cancel`；`POST /api/v1/online-corpus-jobs/:id/run` | 已验证的 tenant/project `IsolationClaims`；list、read、transition、runner 和 observability 数据都有作用域。 | **已有** |
| 本体设计台 | `#/ontology-studio` | `GET, POST /api/v1/ontology/type-drafts`；`POST /api/v1/ontology/type-drafts/from-{csv,json-schema,openapi,sql-ddl,induction}`；`POST /api/v1/ontology/type-drafts/:draft_id/promote`；`POST, PUT, DELETE /api/v1/ontology/{object-types,link-types,action-types,function-defs}` | type draft 与本体写入流程要求已验证 tenant/project `IsolationClaims`；提升仍需显式且可审计。 | **已有** |
| No-Code IDE | — | — | — | **不做** |
| 第二套 Grafana | — | — | — | **不做** |
| Admin 建租户表单 | — | — | tenant 作用域来自已验证 JWT claims，不来自 Admin tenant-creation API。 | **不做** |
| 业务编排 | — | — | — | **不做** |

## 解读与边界

v0.7.0 已交付按 claims 作用域的 Runs 列表、脱敏且按 claims 作用域的 Guard
audit/statistics，以及按 claims 作用域的黑板任务和节点浏览（[#221](https://github.com/skaiy/wild_agentos/issues/221)、
[#222](https://github.com/skaiy/wild_agentos/issues/222)、
[#223](https://github.com/skaiy/wild_agentos/issues/223) 和
[#224](https://github.com/skaiy/wild_agentos/issues/224)）。

隔离诊断无需 token 是刻意设计：它是本地、只读的文件系统工具，
既不创建 tenant，也不授予 HTTP 访问。

内核底层契约参见[隔离契约](17-isolation-contract.zh.md)、
[隔离矩阵](17-isolation-matrix.zh.md)、
[知识摄取](16-knowledge-ingest-import-graph.zh.md)和
[本体知识工程流水线](21-ontology-knowledge-engineering-pipeline.zh.md)。
