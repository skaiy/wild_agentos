# 27. Ontology Accountability

> *A Chinese version is available in
> [27-ontology-accountability.zh.md](27-ontology-accountability.zh.md).*

This document is the shared home for ontology accountability capabilities
(provenance, decision nodes, validity governance, and related audit surfaces).
This milestone implements **fact provenance** only; other sections are reserved
for follow-on work.

## Provenance (implemented)

- **Companion graph**: the kernel mints `graph://{tenant}/{project}/prov` from
  verified `IsolationClaims`. Callers never name this graph; caller SPARQL must
  not use `GRAPH`, and `write_quads_for_claims` / `update_for_claims` cannot
  target the prov graph.
- **Model**: subgraph-style PROV-O with RDF 1.2 triple terms. Each extraction
  staging batch, materialization, RML import, or `ACTION_AUDIT` decision yields
  one `prov:Activity`. Produced triples link via
  `https://wildagentos.org/ontology/prov#contains` with an RDF-star triple term
  object. Source chain nodes (document → page → chunk) use stable IRIs under
  `https://wildagentos.org/ontology/prov/`.
- **Read API** (JWT + claims): `GET /api/v1/ontology/provenance?subject=<IRI>`
  and `GET /api/v1/ontology/provenance/activities/{id}`.
- **HTTP response semantics** (do not confuse with information leakage):
  - **Unauthorized (`401`)**: no verified JWT / isolation claims on either route.
  - **Subject lookup (`200`)**: always returns `{ "subject", "links" }`. An empty
    `links` array means “no provenance in this tenant/project,” including
    cross-tenant cases (byte-identical to an in-scope miss).
  - **Activity lookup (`404`)**: unknown id, cross-tenant id, invalid path segment
    (residual `%` after decoding), or disallowed characters — all share the same
    JSON body `{"error":"provenance activity not found"}`. Malformed *path*
    segments are rejected **before** claims are checked, so unauthenticated
    callers also receive this `404` (not `401`).
- **RML provenance**: a `prov:Activity` with `activityOutcome=failed` is recorded
  when claims-scoped production writes fail after a worker run; worker/parse
  failures before that point do not mint an activity.
- **Read-path scanning**: prov reads filter to the minted companion graph via a
  shared helper; the backing store may still be walked until graph-native
  iterators land. Each read is fail-closed with explicit scan budgets
  (`PROV_READ_MAX_STORE_QUADS_SCANNED`, `PROV_READ_MAX_COMPANION_GRAPH_QUADS` in
  `knowledge_graph::provenance`).
- **Write-path scanning**: staging and materialization paths that collect fact
  triple terms for provenance use the same bounded full-store walk. Exceeding the
  store scan budget fails closed instead of recording a partial activity.
- **Activity existence checks**: `prov_activity_exists_for_claims` uses the same
  bounded scan when deciding whether an activity id is present in the minted
  companion graph, so cross-tenant probes cannot distinguish “missing” from
  “over budget” through a different code path.
- **Oxigraph**: workspace dependency enables feature `rdf-12` for triple terms.

## Decision nodes (planned)

Reserved for issue tracking decision entities and causal edges in the prov graph.

## Validity and supersede (planned)

Reserved for retract/supersede audit trails walking the source DAG.

## Integrity chain (planned)

Per-tenant hash chaining of activities and verification helpers (PR-b scope).
