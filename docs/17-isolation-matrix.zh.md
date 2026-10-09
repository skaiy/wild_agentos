> *本文是 [17-isolation-matrix.md](17-isolation-matrix.md) 的中文翻译。*

---

# 17. 隔离矩阵

本面向客户的矩阵记录 CI 执行的 fail-closed 隔离检查。它描述 `main` 当前行为，
不承诺身份提供商集成、存储迁移或计费系统。

## 本地复现

执行与 CI 相同的 golden suite：

```bash
cargo test --workspace isolation_contract --verbose
```

## 已验证的行为

| 区域 | 客户可见边界 | CI golden test |
| --- | --- | --- |
| 知识库目录 | 无已验证 JWT 时，创建、更新、删除返回 `401`。有 JWT 时元数据写入 mint 的 `graph://{tenant}/{project}`；客户端 graph/vector namespace 不选择目标。其他租户不能读写。 | `api::http::kb::isolation_contract::isolation_contract_kb_catalog_requires_claims_mints_targets_and_blocks_cross_tenant_writes` |
| 知识库图 | 无 verified claims 的导入和统计返回 `401`。导入进入 claims-minted 图而非 `graph:world`；其他租户看不到三元组。 | `api::http::kb::isolation_contract::isolation_contract_graph_read_write_requires_claims_and_uses_minted_graph` |
| 知识库向量 | 无 verified claims 的摄取和搜索返回 `401`。摄取/搜索使用 claims-minted vector namespace，其他租户无法检索。 | `api::http::kb::isolation_contract::isolation_contract_vector_read_write_requires_claims_and_uses_minted_namespace` |
| Coding 制品 | Patch、运行轨迹和复现脚本的 upload/list/download 都要求 verified claims。字节位于 tenant blob prefix；元数据仅在调用者的 claims graph 可见。 | `api::http::artifacts::tests::claims_scoped_artifact_metadata_and_bytes_are_tenant_isolated`；`api::http::artifacts::tests::artifacts_reject_missing_verified_claims` |
| 本体 | 本体写入和 Action 调用无 verified JWT claims 时返回 `401`；Action 写入只在该租户图中可见。 | `api::http::ontology::ontology_crud_tests::isolation_contract_ontology_write_requires_jwt_and_uses_claims_scope`; `api::http::ontology::ontology_crud_tests::isolation_contract_ontology_actions_are_invisible_cross_tenant` |
| 运行时图/向量工具 | 没有 verified claims 的调用返回显式错误，不能空成功；工具给出的 `graph`、`named_graph`、`namespace` 会被忽略。 | `tools::tool_executor::tests::isolation_contract_graph_and_vector_tools_fail_closed_without_claims`; `tools::tool_executor::tests::isolation_contract_graph_tools_use_claims_scope_and_ignore_tool_supplied_graphs` |
| 内部 Agent chat RAG | 无 verified identity 的 chat 返回 `401`，而非空 RAG 成功。JWT claims 约束检索和 Agent 访问；客户端检索目标被忽略。 | `api::http::chat::tests::isolation_contract_chat_without_verified_identity_returns_unauthorized_not_empty_success`; `api::http::chat::tests::isolation_contract_chat_retrieval_isolates_tenants_and_ignores_client_targets`; `api::http::chat::tests::isolation_contract_chat_rejects_cross_tenant_agent_access` |
| 公共 API-key chat | 公共 API-key chat 没有 tenant claims，不执行 tenant graph 或 vector RAG。 | `api::http::chat::tests::isolation_contract_public_api_key_chat_performs_no_tenant_rag` |
| 开发 `X-Identity` | `X-Identity` 只能模拟开发身份，绝不创建 `IsolationClaims`；严格认证拒绝它。 | `api::http::iam::tests::isolation_contract_x_identity_never_creates_isolation_claims` |
| 工具调用额度门 | `AGENTOS_TENANT_TOOL_CALL_CAP` 未设置时不要求 claims；设置有效 cap 后，没有 verified claims 的计量调用被拒绝。 | `spend::tests::isolation_contract_spend_gate_allows_missing_claims_when_cap_is_unset`; `spend::tests::isolation_contract_spend_gate_requires_claims_when_cap_is_configured` |
| Invocations API（`/v1/invocations`） | 匿名/未校验调用方在全部 5 条路由上得到 `401 verified_isolation_claims_required`（STRICT 开/关相同）。跨租户、跨项目与未知 id 的 get/cancel/events 返回逐字节一致的 `404 not_found`（状态码、头、体），且不回显外租户 metadata。 | `api::http::invocations::tests::invocations_routes_reject_unverified_callers_with_401`；`api::http::invocations::tests::events_cross_scope_is_byte_identical_404` （同模块另有 get/cancel 孪生用例）；契约索引 `api::http::isolation_contract_invocations_tests`（#318） |
| 原仅 DA 的写路由（#302） | 全进程共享的注册表（Prompt 创建/激活/灰度/删除；技能注册/删除/import-git/pipeline-rerun）要求 `require_platform_admin`：租户 `DA` 得到 `403 platform_admin_required`，没有已验证 claims 的调用方（包括非严格模式的 `X-Identity` 旁路）得到 `401`。按租户隔离的写操作（市场 publish/install/rollback/upgrade、MCP 技能暴露的列出/新增/删除、KB 重建索引）要求 `require_control_plane_da`（已验证 JWT、显式 project、`DA`）。MCP 技能暴露按已验证的 tenant 和 project 保存。其他租户的 KB、暴露记录或私有包，与不存在时返回同样的 `404`。同一 tenant 的其他 project 删除时也得到同样的响应，其列表和 `POST /mcp` 工具查找都不会看到该暴露。没有 `project_id`、或 `project_id` 为空的旧记录不可列出、不可调用、不可删除。再次创建只会新增一行，旧行留在磁盘上；进程每次启动都会警告，旧行须由操作者手工删除。`POST /mcp` 对 project 被回落成默认值的已验证 token 返回 `403 mcp_claims_incomplete`。 | `api::http::isolation_contract_da_write_routes_tests`（`isolation_contract_skill_exposure_is_isolated_per_project`、`isolation_contract_skill_exposure_legacy_rows_fail_closed`） |

## 历史数据需要离线迁移

Claims 为新图、向量、blob 和 L0 操作 mint 安全名称，但不移动、改写或读穿历史数据。
`scripts/isolation-migrate` v2 提供 operator-reviewed 的离线 copy 与 verify，覆盖
named graph、checkpoint 的本地 Hyperspace `tenant:<id>` vector、共享本地 L0，以及
本地文件系统 `tenant:<id>/kb/...` blob。远程对象存储 blob 仍为历史键，本工具不处理。
在 operator 验证每个适用迁移前，本矩阵不宣称生产隔离完成。完整 plan 和兼容性边界见
[隔离契约](17-isolation-contract.zh.md)。
