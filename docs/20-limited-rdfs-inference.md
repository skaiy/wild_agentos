# 20. Limited RDFS Inference

Issue #100 adds an intentionally small, opt-in inference segment for
claims-scoped knowledge-graph reads. It keeps the graph interface as **SPARQL
1.1 on Oxigraph**. It does not add Nebula, Cypher, a second graph backend, or
an OWL DL reasoner.

## Enablement and behavior

The feature is disabled unless `AGENTOS_LIMITED_RDFS_INFERENCE` is exactly
`1` or `true` (case-insensitive):

```bash
AGENTOS_LIMITED_RDFS_INFERENCE=1
```

When disabled, graph reads execute against the durable Oxigraph named graph
exactly as before. No triples are added and query results are unchanged.

When enabled, each claims-scoped graph query is evaluated against a temporary
Oxigraph store containing only that caller's minted graph. The temporary store
adds only these finite RDFS consequences:

1. transitive `rdfs:subClassOf` closure, excluding reflexive self edges; and
2. inherited `rdf:type` membership along that subclass hierarchy.

The temporary store is discarded after the query. This release exposes
**query-time expansion only**: it does not persist or materialize inferred
triples in the source graph.

## Explicit limits

This is not full RDFS, OWL RL, OWL DL, or an enterprise rules engine. In
particular, it does not evaluate custom rule files, `rdfs:subPropertyOf`,
domain/range rules, OWL equivalence, restrictions, `sameAs`, property
characteristics, inconsistency detection, or non-monotonic rules.

Existing caller-supplied `GRAPH` clauses remain rejected for claims-scoped
queries. The inferred view uses the same claims-minted named graph, preserving
the isolation contract in [17-isolation-contract.md](17-isolation-contract.md).

## Performance and operations

Query-time expansion copies the requesting named graph and runs two SPARQL
updates before the requested query. Cost therefore grows with the graph's
quad count and subclass reachability; dense or cyclic hierarchies can produce
many derived type facts. It is appropriate only for bounded, small ontology
graphs. Keep it off for latency-sensitive or large graphs, and measure with
representative data before enabling it in a service.

Because inferred facts are ephemeral, source updates take effect on the next
query and no cleanup, migration, durable cache, or cross-tenant read-through
is required.
