# 17. Isolation Matrix

This customer-facing matrix records the fail-closed isolation checks that run
in CI. It describes the current behavior of `main`; it is not a promise of an
identity-provider integration, storage migration, or billing system.

## Reproduce locally

Run the same golden suite as CI:

```bash
cargo test --workspace isolation_contract --verbose
```

## Verified behavior

| Area | Customer-visible boundary | CI golden test |
| --- | --- | --- |
| Knowledge-base catalog | Without a verified JWT, catalog create, update, and delete return `401`. With a JWT, catalog metadata is written to the minted `graph://{tenant}/{project}` target; client graph and vector namespace values do not select the target. A different tenant cannot read, update, or delete it. | `api::http::kb::isolation_contract::isolation_contract_kb_catalog_requires_claims_mints_targets_and_blocks_cross_tenant_writes` |
| Knowledge-base graph | Graph import and stats reject requests without verified claims (`401`). Imports go to the claims-minted graph rather than `graph:world`; a different tenant sees no imported triples. | `api::http::kb::isolation_contract::isolation_contract_graph_read_write_requires_claims_and_uses_minted_graph` |
| Knowledge-base vector | Vector ingest and search reject requests without verified claims (`401`). Ingest and search use the claims-minted vector namespace, and other tenants cannot retrieve the data. | `api::http::kb::isolation_contract::isolation_contract_vector_read_write_requires_claims_and_uses_minted_namespace` |
| Ontology | Ontology writes and action invocation reject requests without verified JWT claims (`401`). Action writes are visible in the verified tenant's graph only. | `api::http::ontology::ontology_crud_tests::isolation_contract_ontology_write_requires_jwt_and_uses_claims_scope`; `api::http::ontology::ontology_crud_tests::isolation_contract_ontology_actions_are_invisible_cross_tenant` |
| Runtime graph and vector tools | Calls without verified claims return an explicit error, rather than an empty successful result. Tool-supplied `graph`, `named_graph`, and `namespace` values are ignored; claims mint the target. | `tools::tool_executor::tests::isolation_contract_graph_and_vector_tools_fail_closed_without_claims`; `tools::tool_executor::tests::isolation_contract_graph_tools_use_claims_scope_and_ignore_tool_supplied_graphs` |
| Internal agent chat RAG | Chat without verified identity returns `401`, not an empty RAG success. Retrieval and agent access are constrained to the tenant/project minted from JWT claims; client retrieval targets are ignored. | `api::http::chat::tests::isolation_contract_chat_without_verified_identity_returns_unauthorized_not_empty_success`; `api::http::chat::tests::isolation_contract_chat_retrieval_isolates_tenants_and_ignores_client_targets`; `api::http::chat::tests::isolation_contract_chat_rejects_cross_tenant_agent_access` |
| Public API-key chat | Public API-key chat has no tenant claims and performs no tenant graph or vector RAG. | `api::http::chat::tests::isolation_contract_public_api_key_chat_performs_no_tenant_rag` |
| Development `X-Identity` | `X-Identity` can simulate a development identity but never creates `IsolationClaims`. Strict authentication rejects it. | `api::http::iam::tests::isolation_contract_x_identity_never_creates_isolation_claims` |
| Tool-call spend gate | When `AGENTOS_TENANT_TOOL_CALL_CAP` is unset, the gate does not require claims. When it is set to a valid cap, a metered call without verified claims is rejected. | `spend::tests::isolation_contract_spend_gate_allows_missing_claims_when_cap_is_unset`; `spend::tests::isolation_contract_spend_gate_requires_claims_when_cap_is_configured` |

## Historical data is not migrated

Claims mint safe names for new graph, vector, blob, and L0 operations. They do
not move, rewrite, or read through historical data. In particular,
`graph:world`, `tenant:<id>` vector tags, `./data/l0_store/l0.redb`, and
`tenant:default/kb/...` blob keys remain historical live key spaces. This
matrix does not claim that those keys are isolated or migrated. See the
[Isolation Contract](17-isolation-contract.md) for the full naming and
compatibility boundary.
