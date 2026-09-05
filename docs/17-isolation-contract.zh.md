> *本文是 [17-isolation-contract.md](17-isolation-contract.md) 的中文翻译。*

---

# 17. 隔离契约

`src/isolation/` 是作用域身份和存储名称的内核契约。它不是身份提供商（IdP）、OIDC
或 Keycloak 集成、17 状态 IAM 工作流，也不是存储迁移。本文件说明 2026-09-04 的
`main`：区分当前已接线的存储路径和仍在使用的历史路径；已 mint 名称不证明已有数据
已经迁移。

## 可信 claims 边界

唯一可信构造路径是：

```rust
IsolationClaims::from_verified(tenant_id, project_id, actor_id)
```

调用者在认证边界验证这些值；隔离模块不读取 HTTP 请求、不解析 JSON body、不检查
headers，也不校验 credentials。`X-Identity` 仅为开发模拟，不创建
`IsolationClaims`，不是生产租户身份来源。生产部署应使用 OIDC/JWKS 模式
（`AGENTOS_AUTH_MODE=oidc`），并配置 `AGENTOS_OIDC_JWKS_URL`、
`AGENTOS_OIDC_ISSUER` 和 `AGENTOS_OIDC_AUDIENCE`；issuer 与 audience 是必填，
且只接受非对称 OIDC 算法。JWKS 从配置 endpoint 获取，短时缓存，遇到未知 key ID
会刷新一次。默认 `hs256` 模式用 `AGENTOS_JWT_SECRET` 验证，保留给本地开发。
OIDC/JWKS 配置错误、缺少 key、签名无效、issuer/audience 不匹配都会 fail closed。
JWKS URL 必须使用 HTTPS；仅本地开发与测试 fixture 可以使用 loopback HTTP。

无 claims 的请求不能使用 claims-scoped graph/blob 路径。`JwtClaims.project_id` 是
带 serde default 的 `Option<String>`；缺失或为空时 mint `default` project。非空值
传入 `IsolationClaims::from_verified`；`.`、`..`、路径分隔符（如 `a/b`）等不安全
值会 fail closed，`verify_jwt` 不产生 identity。

## 命名契约，不是迁移

| Mint | 契约 |
| --- | --- |
| graph IRI | `graph://{tenant}/{project}` |
| object prefix | `{tenant}/` |
| vector namespace | `vector://{tenant}/{project}` |
| L0 path | `/data/l0/{tenant}` |

Minting 校验 tenant/project 为安全标识符段，拒绝空值、`.` / `..`、路径分隔符和其他
不安全字符；它不创建目录或访问后端。以下历史键空间仍在使用且未重映射、未改写：
`graph:world`、`tenant:<id>`、`./data/l0_store/l0.redb`、以及
`tenant:default/kb/...`。新隔离工作不得更改、删除或假定拥有这些路径；每项迁移须
单独定义范围。

## 诊断工具

`scripts/isolation-diagnose` 是只读本地文件系统诊断，不需要 JWT、HTTP endpoint 或
secret，也不会打开、创建、写入、合并、删除或迁移 graph、vector store、blob store
或 L0 database。

```bash
scripts/isolation-diagnose --data-root data
scripts/isolation-diagnose --data-root data --json
```

默认输出为隔离矩阵 Markdown（`路径 | 身份 | 期望 | 实测`），按 tenant/project 报告
`graph://{tenant}/{project}`、`vector://{tenant}/{project}` durable marker、
`blobs/{tenant}/` 下对象数、tenant L0 目录和历史键指标。JSON schema version 为 `1`；
`*_artifact_count` 只统计含 durable namespace marker 的常规存储文件，而非后端对象数。
超过 32 MiB 的文件不扫描 marker，并列入 `scan_warnings`。这是未来迁移的 inventory，
不是迁移工具；正的 mint 结果不证明迁移或完整多租户隔离。

公共 API-key chat 的无 tenant-RAG 不变量由
`public_api_key_chat_context_performs_no_tenant_rag` 覆盖：它没有 claims，绝不检索
tenant graph 或 vectors。

## 历史键迁移工具

`scripts/isolation-migrate` 是显式**离线**迁移工具，不是 HTTP endpoint，任何请求
hot path 都不能调用。它当前只实现 Oxigraph named-graph migration；vector
(`tenant:<id>`)、L0 和 blob 历史键仍未实现，生产查询不得把它们表述为已迁移或隔离。

其 JSON plan 的 schema version 为 `1`，把每个历史 source graph 映射到
claims-minted tenant/project target：

```json
{
  "schema_version": 1,
  "named_graphs": [{
    "source_graph": "graph:world",
    "target": { "tenant": "acme", "project": "research" }
  }]
}
```

先运行默认只读 plan：

```bash
scripts/isolation-migrate --data-root data --plan migration.json
```

审阅 source/target quad count 后，执行固定顺序 `plan → copy → verify`：

```bash
scripts/isolation-migrate --data-root data --plan migration.json --execute
```

工具在复制前拒绝不安全 target、重复 target、已有 quads 的 target 和无效 plan。它只
通过 Oxigraph SPARQL 复制，并验证每个 target count 等于记录的 source count。成功
real run 写入本地 `<data-root>/isolation-migrate-audit-<timestamp>.json`，仅记录
plan/verify 结果，不含 secret；source graph 默认保留。

删除已验证 source 是单独的破坏性操作，必须先备份和独立生产查询检查，并同时提供：

```bash
scripts/isolation-migrate --data-root data --plan migration.json --execute \
  --delete-source --confirm-delete-source
```

普通 copy 的 rollback 是停止使用新 target，并以人工审阅的 Oxigraph maintenance
action 清空它；原 source 仍可用。删除无法自动撤销，因而需要双重确认。刻意没有
来自 `graph:world` 的 `UNION`、fallback 或 read-through；vector、L0、blob 也无
同等行为。在每个历史 backend 被显式迁移和验证前，生产查询不得宣称隔离完成。

## 当前接线

### Spend gate

tenant tool-call spend gate 仅在 `AGENTOS_TENANT_TOOL_CALL_CAP` 为有效 cap 时启用。
未设置时调用者不需要 `IsolationClaims`；设置后每个 metered tool call 都需要 verified
claims，超过 per-tenant cap 会被拒绝。这是 process-local counter，不是 billing ledger。

### Graph 与运行时工具

新生产 graph write 通过 `IsolationClaims::from_verified` 和 `graph_iri()` 定位
`graph://{tenant}/{project}`。兼容 write/query/entity-search/neighbour/delete API 没有
verified claims 时拒绝调用，不会静默读写 `graph:world`。HTTP knowledge-base graph
import、stats、catalog CRUD 都要求 JWT-verified claims，客户端 graph/namespace 字段
不是 write target；无 claims 返回 `401`。图查询仍是 Oxigraph 上的 SPARQL 1.1。

SA execution 将 boundary-verified `IsolationClaims` 传给 graph/vector builtins。其
`graph`、`named_graph`、`namespace` 参数永不选择 storage target；无 claims 时返回
显式错误。runtime tools 没有 read-only public ontology exception，也不迁移或读穿
历史 `graph:world`。

### Blob、Vector、Chat RAG 与 L0

`BlobStore::put`、`get`、`delete`、`exists` 接受 `IsolationClaims` 和 relative key；
backend 用 `object_key_prefix()` mint `{tenant}/`，新对象形如
`{tenant}/kb/<kbid>/<sha256>`。HTTP upload、raw-document retrieval、rebuild 先要求
JWT-verified claims。`tenant:default/kb/...` 仍为历史对象，没有 read-through 或迁移。

claims-scoped vector upsert/search/delete 使用 `vector_namespace()`
(`vector://{tenant}/{project}`)，并作用于 stored IRI 和 JSON-LD named-graph metadata。
HTTP vector ingest/search/upload/reindex 要求 verified claims；无 claims 返回 `401`；
unscoped API fail closed。历史 `tenant:<id>` rows 未迁移，也不会由 claims-scoped
search 返回。

内部 Agent chat RAG 需要 JWT-verified `IsolationClaims`。图/向量检索目标由 claims
mint；客户端 `named_graph`、`vector_namespace` 和 agent knowledge-pack target
configuration 被忽略。请求的 Agent 必须具有相同 tenant/project；缺失或不匹配的
legacy unscoped Agent 不再隐式共享。新 user Agent 创建时写入 verified scope；检索错误
直接返回。Public API-key chat 没有 tenant/project claims，完全不做 tenant RAG。

`L0Store::open_for_claims` 只在给定 L0 root 下创建和写入 `l0_path()` mint 的 tenant
目录（`/data/l0/{tenant}`）。生产 HTTP task execution 有 JWT-verified claims 时调用
`open_for_claims`，PDCA persistence 使用该 tenant directory。无 claims 时保留由
`L0Store::new` read-only 打开的 startup L0 store；写历史共享数据库会 fail closed。
`./data/l0_store/l0.redb` 尚未迁移。

Coding 制品的上传、列出与下载同样要求 JWT-verified claims。制品字节使用 mint 的
`{tenant}/artifacts/` blob 前缀；用于重放的元数据写入 mint 的 claims graph。每个
patch、运行轨迹或复现脚本以 task IRI 关联 checkpoint 执行。参见
[Claims 作用域 Coding 制品](20-coding-artifacts.zh.md)。

## 图接口

Wild AgentOS 图查询仍是 Oxigraph 上的 **SPARQL 1.1**。本契约不引入 Cypher、不替换
Oxigraph，也不改变图查询接口。
