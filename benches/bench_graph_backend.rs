use criterion::{black_box, criterion_group, criterion_main, Criterion};

use std::sync::Arc;

use wild_agent_os_core::graph_backend::{GraphBackend, PetgraphBackend, SparqlBackend};
use wild_agent_os_core::knowledge_graph::store::KnowledgeGraphStore;
use wild_agent_os_core::knowledge_graph::types::{RdfQuad, RdfValue};
use wild_agent_os_core::skill_graph::graph_store::SkillGraphStore;
use wild_agent_os_core::skill_graph::types::SkillGraphNode;

// ── Helpers ──────────────────────────────────────────────────────────

fn make_quad(s: &str, p: &str, o: RdfValue, graph: &str) -> RdfQuad {
    RdfQuad {
        subject: s.to_string(),
        predicate: p.to_string(),
        object: o,
        graph: Some(graph.to_string()),
    }
}

fn build_petgraph_backend(node_count: usize) -> PetgraphBackend {
    let store = Arc::new(SkillGraphStore::new());
    for i in 0..node_count {
        let iri = format!("iri://skills/node_{}", i);
        let mut node = SkillGraphNode::new(&iri, &format!("Node {}", i), "");
        // Link to next node (chain topology)
        if i + 1 < node_count {
            let next = format!("iri://skills/node_{}", i + 1);
            node.add_prerequisite(&next, "chain");
        }
        store.register_skill(node).unwrap();
    }
    PetgraphBackend::new(store)
}

// Fixture setup writes the unscoped bench graph directly: the claims-scoped API would
// mint a tenant graph name that the backend under test does not query.
#[allow(deprecated)]
fn build_sparql_backend(node_count: usize) -> SparqlBackend {
    let kg = KnowledgeGraphStore::new().unwrap();
    let graph = "http://bench/graph";
    for i in 0..node_count {
        let s = format!("iri://skills/node_{}", i);
        let p = "iri://predicate/related";
        let o = RdfValue::Iri(format!("iri://skills/node_{}", (i + 1) % node_count));
        kg.write_quads(&[make_quad(&s, p, o, graph)], graph)
            .unwrap();
    }
    SparqlBackend::new(Arc::new(kg)).with_named_graph(graph)
}

// ── Benchmarks ───────────────────────────────────────────────────────

fn bench_petgraph_all_nodes(c: &mut Criterion) {
    let backend = build_petgraph_backend(200);
    c.bench_function("PetgraphBackend/all_nodes (200 nodes)", |b| {
        b.iter(|| black_box(backend.all_nodes()))
    });
}

fn bench_sparql_all_nodes(c: &mut Criterion) {
    let backend = build_sparql_backend(200);
    c.bench_function("SparqlBackend/all_nodes (200 nodes)", |b| {
        b.iter(|| black_box(backend.all_nodes()))
    });
}

fn bench_petgraph_all_edges(c: &mut Criterion) {
    let backend = build_petgraph_backend(200);
    c.bench_function("PetgraphBackend/all_edges (200 edges)", |b| {
        b.iter(|| black_box(backend.all_edges()))
    });
}

fn bench_sparql_all_edges(c: &mut Criterion) {
    let backend = build_sparql_backend(200);
    c.bench_function("SparqlBackend/all_edges (200 edges)", |b| {
        b.iter(|| black_box(backend.all_edges()))
    });
}

fn bench_petgraph_has_node_hit(c: &mut Criterion) {
    let backend = build_petgraph_backend(200);
    c.bench_function("PetgraphBackend/has_node (hit)", |b| {
        b.iter(|| black_box(backend.has_node("iri://skills/node_42")))
    });
}

fn bench_sparql_has_node_hit(c: &mut Criterion) {
    let backend = build_sparql_backend(200);
    c.bench_function("SparqlBackend/has_node (hit)", |b| {
        b.iter(|| black_box(backend.has_node("iri://skills/node_42")))
    });
}

fn bench_petgraph_has_node_miss(c: &mut Criterion) {
    let backend = build_petgraph_backend(200);
    c.bench_function("PetgraphBackend/has_node (miss)", |b| {
        b.iter(|| black_box(backend.has_node("iri://skills/nonexistent")))
    });
}

fn bench_sparql_has_node_miss(c: &mut Criterion) {
    let backend = build_sparql_backend(200);
    c.bench_function("SparqlBackend/has_node (miss)", |b| {
        b.iter(|| black_box(backend.has_node("iri://skills/nonexistent")))
    });
}

fn bench_petgraph_node_count(c: &mut Criterion) {
    let backend = build_petgraph_backend(200);
    c.bench_function("PetgraphBackend/node_count (200 nodes)", |b| {
        b.iter(|| black_box(backend.node_count()))
    });
}

fn bench_sparql_node_count(c: &mut Criterion) {
    let backend = build_sparql_backend(200);
    c.bench_function("SparqlBackend/node_count (200 nodes)", |b| {
        b.iter(|| black_box(backend.node_count()))
    });
}

criterion_group!(
    benches,
    bench_petgraph_all_nodes,
    bench_sparql_all_nodes,
    bench_petgraph_all_edges,
    bench_sparql_all_edges,
    bench_petgraph_has_node_hit,
    bench_sparql_has_node_hit,
    bench_petgraph_has_node_miss,
    bench_sparql_has_node_miss,
    bench_petgraph_node_count,
    bench_sparql_node_count,
);
criterion_main!(benches);
