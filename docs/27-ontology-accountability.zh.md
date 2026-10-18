# 27. 本体问责（Ontology Accountability）

> *English version:
> [27-ontology-accountability.md](27-ontology-accountability.md).*

本文档承载本体问责相关能力（溯源、决策节点、有效期治理等）的共用说明。
当前里程碑仅实现**事实溯源**；其余章节为后续工作占位。

## 溯源（已实现）

- **伴生图**：内核根据已验证 `IsolationClaims` mint
  `graph://{tenant}/{project}/prov`。调用方不能命名该图；调用方 SPARQL 禁止
  `GRAPH`；`write_quads_for_claims` / `update_for_claims` 不能写入溯源图。
- **模型**：子图式 PROV-O + RDF 1.2 triple term。每次抽取暂存、物化、RML
  导入或 `ACTION_AUDIT` 决策对应一个 `prov:Activity`；产出三元组通过
  `https://wildagentos.org/ontology/prov#contains` 关联 triple term。来源链
  （文档 → 页 → chunk）使用
  `https://wildagentos.org/ontology/prov/` 下稳定 IRI。
- **只读 API**（JWT + claims）：`GET /api/v1/ontology/provenance?subject=<IRI>`
  与 `GET /api/v1/ontology/provenance/activities/{id}`。跨租户或未知
  activity id 返回字节一致的 `404` 响应体。
- **Oxigraph**：工作区依赖开启 `rdf-12` feature 以支持 triple term。

## 决策节点（规划中）

在溯源图中记录决策实体与因果边（单独 issue）。

## 有效期与取代（规划中）

按来源 DAG 记录撤回/取代审计。

## 完整性哈希链（规划中）

按租户的 Activity 哈希链与校验（PR-b 范围）。
