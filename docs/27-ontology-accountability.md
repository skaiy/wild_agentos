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
  and `GET /api/v1/ontology/provenance/activities/{id}`. Cross-tenant or unknown
  activity ids return identical `404` bodies.
- **Oxigraph**: workspace dependency enables feature `rdf-12` for triple terms.

## Decision nodes (planned)

Reserved for issue tracking decision entities and causal edges in the prov graph.

## Validity and supersede (planned)

Reserved for retract/supersede audit trails walking the source DAG.

## Integrity chain (planned)

Per-tenant hash chaining of activities and verification helpers (PR-b scope).
