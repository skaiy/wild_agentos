# 7. Knowledge Graph System

> *For the Chinese version, see [07-knowledge-graph.zh.md](07-knowledge-graph.zh.md).*

> An Oxigraph RDF-backed knowledge graph engine supporting LLM knowledge extraction, code AST extraction, SPARQL queries, and knowledge bridging.

> The trusted scope, naming, and historical key status for tenant data are governed by [17-isolation-contract.md](17-isolation-contract.md).

## Module Architecture

The input sources are unstructured text, code files in nine languages, and structured JSON. `KnowledgeExtractor` performs LLM extraction with three retries and JSON cleanup; `CodeAstExtractor` uses tree-sitter and SHA256-based incremental updates; `knowledge_import_json` maps JSON to RDF. `RdfMapper` converts `NodeDef`/`EdgeDef` to `RdfQuad` and property keys to IRIs, while `OntologyManager` provides 12 built-in ontology terms. `KnowledgeGraphStore` uses an Oxigraph Memory Store and offers SPARQL queries and 1–3 hop entity search. `KnowledgeBridge` bridges knowledge and skills through isolated named graphs, and `UnifiedGraphStore` shares the underlying `Arc<Store>`.

## Core Components

### KnowledgeGraphStore — Graph Storage

An RDF graph store based on Oxigraph Memory Store. `Arc<Mutex>` field injection eliminates global static dependencies.

```rust
pub struct KnowledgeGraphStore {
    store: Store,              // Oxigraph in-memory store
    default_graph: String,     // Historical/system default named graph "graph:world"
}
```

| Method | Description |
|------|------|
| `write_quads(quads, graph)` | Batch-write RDF Quads (SPARQL INSERT) |
| `delete_quads_for_source(source, graph)` | Delete related Quads by source file |
| `delete_quads_by_subject_prefix(prefix, graph)` | Batch-delete by subject IRI prefix |
| `query_sparql(sparql, named_graph)` | Execute a SPARQL SELECT query |
| `search_entities(keyword, entity_type)` | Fuzzy entity search (case-insensitive) |
| `get_neighbors(entity_id, depth)` | 1–3 hop neighbor traversal |

### RdfMapper — RDF Mapping Engine

Maps internal types to standard RDF Quads. `NodeDef {id, node_type, label, description, properties}` maps to `iri://entity/{id}`, with `rdf:type`, `rdfs:label`, and `ontology/meta/{key}` predicates. `EdgeDef {source, target, relation}` maps source and target to entity IRIs and uses the relation as predicate.

**Property-key IRI conversion rules**:
- Keys containing `/`, `#`, or `:` are used directly because they are complete IRIs.
- Other keys are prefixed with `https://agentos.ontology/meta/`.
- For example, `contentHash` becomes `https://agentos.ontology/meta/contentHash`.

### CodeAstExtractor — Code AST Extraction

tree-sitter-based code-structure extraction supports Rust (`fn`/`struct`/`enum`/`trait`/`impl`/`use`, calls/implements), Python (`def`/`class`/`import`, calls/inherits), JS/TS/TSX (function/class/method/import/interface/type, calls/inherits), Go (func/method/type/import, calls), Java (class/interface/method/import, calls/inherits/implements), and C/C++ (function/class/struct/include, calls).

Its three-layer incremental strategy is: L1 skips entirely when the SHA256 is unchanged; L2 deletes by subject prefix and rewrites a changed file; L3 passes `old_tree` to accelerate tree-sitter parsing.

```rust
pub enum IncrementalResult {
    Unchanged,
    Created { entity_count: usize, relation_count: usize, quad_count: usize },
    Updated { entity_count: usize, relation_count: usize, quad_count: usize, deleted_quads: usize },
}
```

### KnowledgeExtractor — LLM Knowledge Extraction

Extracts entities and relations from unstructured text through the LLM API. It retries three times because an LLM can fail to return valid JSON, removes Markdown code-fence markers and truncates overly long responses, validates the result using JSON Schema, and can restrict extraction with an optional `domain` parameter.

### OntologyManager — Ontology Management

The built-in classes are `ontology:Person`, `ontology:Organization`, `ontology:Concept`, `ontology:Event`, `ontology:Product`, and `ontology:Project`. The built-in properties are `ontology:worksFor`, `ontology:manages`, `ontology:dependsOn`, `ontology:hasSkill`, `ontology:applicableIn`, `ontology:bridge/hasSkill`, `ontology:bridge/applicableIn`, and `ontology:bridge/relatedTo`.

### KnowledgeBridge — Knowledge Bridging

It manages `HasSkill` (`ontology:bridge/hasSkill`), `ApplicableIn` (`ontology:bridge/applicableIn`), and `RelatedTo` (`ontology:bridge/relatedTo`) relations between knowledge-graph entities and skills.

## Named Graph Isolation Strategy

`graph:world` is the historical/system default graph, not the production tenant write target. Production tenant reads and writes are minted from verified `IsolationClaims` as `graph://{tenant}/{project}`. Compatibility APIs do not silently dual-write, read through, or migrate `graph:world`, and client graph selection does not override that target. Historical-key migration must be designed separately; see the [Isolation Contract](17-isolation-contract.md).

| Named graph | Purpose | Write source |
|--------|------|----------|
| `graph:world` | Historical/system-default general knowledge | `knowledge_extract` (historical path) |
| `graph:code` | Code-structure knowledge | `knowledge_extract_code` |
| `graph:skill` | Skill graph | `SkillGraphStore` |
| `graph:ontology` | Ontology definitions | `ontology_register` |
| `graph:bridge` | Knowledge-skill bridging | `knowledge_bridge` |

## Registered Tools

| Tool | Description | Available to PA |
|------|------|---------|
| `knowledge_extract` | LLM extraction of entities and relations from text | ✅ |
| `knowledge_query` | SPARQL SELECT query | ✅ |
| `kg_search` | Fuzzy entity search | ✅ |
| `knowledge_neighbors` | 1–3 hop neighbor traversal | ✅ |
| `knowledge_import_json` | Map JSON data to graph nodes | ❌ |
| `ontology_register` | Register a custom ontology term | ❌ |
| `knowledge_bridge` | Create a knowledge-skill bridge | ❌ |
| `knowledge_extract_code` | tree-sitter code AST extraction (incremental) | ✅ |

## Key Design Decisions

1. **OnceCell → Arc\<Mutex\> field injection**: `KnowledgeGraphStore` no longer uses a global static `OnceCell`; it is shared as a `ToolExecutor` field through `Arc<Mutex>`.
2. **Async ToolFn closures**: handlers change from synchronous function pointers to `Arc<dyn Fn(Value) -> Pin<Box<dyn Future<...>>>>`, supporting asynchronous work and closure captures.
3. **Property-key IRI normalization**: every property key becomes a complete IRI, ensuring valid SPARQL syntax.
4. **Incremental updates instead of full replacement**: changed code files delete old Quads before writing new ones, while SHA256 skips unchanged files.
5. **UnifiedGraphStore sharing**: the underlying Oxigraph Store is shared through `UnifiedGraphStore`, with named-graph isolation between modules.
