> *本文是 [23-admin-control-plane-api-matrix.md](23-admin-control-plane-api-matrix.md) 的中文翻译。*

---

# 23. Admin 控制面 ↔ 内核 API 对照矩阵

这是 Admin 控制面与内核 HTTP API 的 v0.8 验收矩阵。hash 路由标识 Admin
页面，不是内核路径。“所需 claims”只陈述当前内核行为或已链接的进行中改动，不会因为
页面名称而推断授权策略。

## 状态词汇

- **已有** — 所列路径已注册在当前 `main`。
- **v0.8 本里程碑** — 所列路径或 claims 边界由链接的开放 v0.8 工作加入，尚未进入
  `main`。
- **不做** — 明确不在本矩阵和本里程碑范围内。

## 对照矩阵

| Admin 屏 | hash 路由 | 内核 method+path | 所需 claims | 状态 |
| --- | --- | --- | --- | --- |
| Runs | `#/runs` | `GET /api/v1/tasks` | 已验证的 tenant/project `IsolationClaims`（计划中）；只列出调用方持久化作用域内的任务。 | **v0.8 本里程碑** — [#221](https://github.com/skaiy/wild_agentos/issues/221)、[PR #225](https://github.com/skaiy/wild_agentos/pull/225) |
| Runs — 任务详情 | `#/runs` | `GET /api/v1/tasks/:task_iri`、`GET /api/v1/tasks/:task_iri/status`、`GET /api/v1/tasks/:task_iri/details`、`GET /api/v1/tasks/trends` | 当前 `main` 没有统一的 verified-claims 门禁；不能把这些详情/读取路径当作有作用域的 Admin 列表契约。 | **已有** |
| Agents | `#/agents` | `GET, POST /api/v1/agents`；`PUT, DELETE /api/v1/agents/:id`；`POST /api/v1/agents/:id/chat` | 创建和内部 chat 要求 verified `IsolationClaims`；当前 list/update/delete handler 没有统一要求或按 claims 过滤。 | **已有** — claims 覆盖不一致 |
| Skills | `#/skills` | `GET, POST, DELETE /api/v1/skills`；`GET /api/v1/skills/manifest`；`POST /api/v1/skills/import-git`；`GET /api/v1/skills/pipeline-runs`；`POST /api/v1/skills/pipeline-rerun` | Skill 变更要求 `DA`；读取没有统一的 `IsolationClaims` 门禁。 | **已有** |
| KB · Ontology | `#/kb-ontology` | `GET, POST /api/v1/kb/bases`；`GET, POST /api/v1/kb/categories`；`GET, POST /api/v1/knowledge-packs`；`GET /api/v1/ontology/types`；`GET /api/v1/ontology/health` | KB 图/向量摄取、目录 CRUD 和本体写入使用已验证的 tenant/project `IsolationClaims`；缺失 claims 会 fail closed。 | **已有** |
| Isolation | `#/isolation` | 没有 create-tenant HTTP 路径。本地只读诊断：`scripts/isolation-diagnose --data-root <path>` | JWT 验证 mint tenant/project claims。诊断 CLI 不需 JWT，仍可作为只读本地导入/盘点辅助；它不是 HTTP endpoint。 | **已有** — 没有 Admin 建租户表单 |
| Keys · Models | `#/keys-models` | `GET, POST /api/v1/api-clients`；`PUT, DELETE /api/v1/api-clients/:id`；`POST, DELETE /api/v1/api-clients/:id/keys[/:kid]`；`GET /api/v1/api-audit`；`GET, PUT /api/v1/config`；`POST /api/v1/models/test`；`POST /api/v1/providers/models`；`POST /api/v1/embedding/activate` | API client 和 audit 操作要求 `DA`；config 更新要求 verified JWT claims 加 `DA`。当前 `main` 的模型测试/发现/embedding 激活没有统一 claims 门禁。 | **已有** — claims 覆盖不一致 |
| Memory · 黑板 | `#/memory`（也可深链至 `#/blackboard`） | `GET /api/v1/blackboard/tasks`；`GET /api/v1/blackboard/nodes?task_iri=…` | 已验证的 tenant/project `IsolationClaims`（计划中）；没有持久化作用域的历史记录不得返回。 | **v0.8 本里程碑** — [#223](https://github.com/skaiy/wild_agentos/issues/223)、[PR #227](https://github.com/skaiy/wild_agentos/pull/227) |
| Ops | `#/ops`（计划中）；当前侧栏遗留：`#/overview`、`#/runtime`、`#/security` | `GET /api/v1/batch/agents`；`POST /api/v1/batch/agents/:name/control`；`GET /api/v1/guard/audit`；`GET /api/v1/guard/stats`；`GET /metrics` | Batch control 要求 `DA`；batch list 和 metrics 没有统一 claims 门禁。仅在链接改动合入后，guard audit/stats 才要求已验证 tenant/project claims，且两者使用同一作用域集合。 | batch/metrics **已有**；guard 为 **v0.8 本里程碑** — [#222](https://github.com/skaiy/wild_agentos/issues/222)、[PR #226](https://github.com/skaiy/wild_agentos/pull/226) |
| 在线语料 | `#/online-corpus-jobs` | `GET, POST /api/v1/online-corpus-jobs`；`GET /api/v1/online-corpus-jobs/observability`；`GET /api/v1/online-corpus-jobs/:id`；`POST /api/v1/online-corpus-jobs/:id/cancel`；`POST /api/v1/online-corpus-jobs/:id/run` | 已验证的 tenant/project `IsolationClaims`；list、read、transition、runner 和 observability 数据都有作用域。 | **已有** |
| 本体设计台 | `#/ontology-studio` | `GET, POST /api/v1/ontology/type-drafts`；`POST /api/v1/ontology/type-drafts/from-{csv,json-schema,openapi,sql-ddl,induction}`；`POST /api/v1/ontology/type-drafts/:draft_id/promote`；`POST, PUT, DELETE /api/v1/ontology/{object-types,link-types,action-types,function-defs}` | type draft 与本体写入流程要求已验证 tenant/project `IsolationClaims`；提升仍需显式且可审计。 | **已有** |
| No-Code IDE | — | — | — | **不做** |
| 第二套 Grafana | — | — | — | **不做** |
| Admin 建租户表单 | — | — | tenant 作用域来自已验证 JWT claims，不来自 Admin tenant-creation API。 | **不做** |
| 业务编排 | — | — | — | **不做** |

## 解读与边界

以下三项 v0.8 工作有意标为进行中：

1. [#221](https://github.com/skaiy/wild_agentos/issues/221) /
   [PR #225](https://github.com/skaiy/wild_agentos/pull/225) 提供按 claims 作用域的
   Runs 列表。
2. [#222](https://github.com/skaiy/wild_agentos/issues/222) /
   [PR #226](https://github.com/skaiy/wild_agentos/pull/226) 为 guard audit/stats
   加作用域并脱敏。
3. [#223](https://github.com/skaiy/wild_agentos/issues/223) /
   [PR #227](https://github.com/skaiy/wild_agentos/pull/227) 为黑板任务和节点浏览
   加作用域。

在这些 PR 合入前，Admin 消费方不得声称 Runs 列表、guard audit/stats 或黑板读取已拥有
上述 claims 边界。反之，隔离诊断无需 token 是刻意设计：它是本地、只读的文件系统工具，
既不创建 tenant，也不授予 HTTP 访问。

内核底层契约参见[隔离契约](17-isolation-contract.zh.md)、
[隔离矩阵](17-isolation-matrix.zh.md)、
[知识摄取](16-knowledge-ingest-import-graph.zh.md)和
[本体知识工程流水线](21-ontology-knowledge-engineering-pipeline.zh.md)。
