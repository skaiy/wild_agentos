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
  与 `GET /api/v1/ontology/provenance/activities/{id}`。
- **HTTP 响应约定**（避免把格式错误当成信息泄露）：
  - **未授权（`401`）**：两条路由均无已验证 JWT / isolation claims。
  - **按 subject 查询（`200`）**：固定返回 `{ "subject", "links" }`；`links`
    为空表示本 tenant/project 无溯源（含跨租户），与域内查无结果时响应体逐字节一致。
  - **按 activity id 查询（`404`）**：未知、跨租户、路径段非法（解码后仍含 `%`）
    或字符集不允许 — 共用同一 JSON 体
    `{"error":"provenance activity not found"}`。路径格式错误在 **claims 检查之前**
    即返回该 `404`（未认证亦然，不是 `401`）。
- **RML 溯源**：worker 跑通后若 claims 作用域内的生产写入失败，会记录
  `activityOutcome=failed` 的 `prov:Activity`；更早的 worker/解析失败不 mint activity。
- **读路径扫描**：溯源读路径经共享 helper 只处理 mint 的伴生图；在 Oxigraph
  提供按图迭代前，底层仍可能全库遍历。每次读取有显式扫描预算，超限 fail-closed
  （见 `knowledge_graph::provenance` 中 `PROV_READ_MAX_STORE_QUADS_SCANNED`、
  `PROV_READ_MAX_COMPANION_GRAPH_QUADS`）。
- **Oxigraph**：工作区依赖开启 `rdf-12` feature 以支持 triple term。

## 决策节点（规划中）

在溯源图中记录决策实体与因果边（单独 issue）。

## 有效期与取代（规划中）

按来源 DAG 记录撤回/取代审计。

## 完整性哈希链（规划中）

按租户的 Activity 哈希链与校验（PR-b 范围）。
