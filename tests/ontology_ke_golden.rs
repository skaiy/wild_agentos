//! Frozen, deterministic kernel-graph evaluation for ontology-constrained
//! extraction, canonicalization, and the #140 quality gate.
//!
//! This test only reads the fixture through `include_str!`; no extraction or
//! optimization path receives a fixture path or a write capability.

use std::collections::HashSet;

use serde::Deserialize;
use wild_agent_os_core::{
    isolation::IsolationClaims,
    knowledge_graph::{
        canonicalizer::{canonicalize, CanonicalizationStatus},
        ontology_layer::{OntologyDefinition, SparqlAskAssertion},
        quality_gate::{KgQualityGate, QualityCoverageArbitration, QualityGateRequest},
        rdf_mapper::RdfMapper,
        store::{ClaimsGraphUpdate, KnowledgeGraphStore},
        types::LLMExtractionOutput,
    },
};

#[derive(Debug, Deserialize)]
struct GoldenSuite {
    suite_version: String,
    minimum_case_count: usize,
    required_case_ids: Vec<String>,
    metrics: MetricThresholds,
    promoted_ontology: OntologyDefinition,
    cases: Vec<GoldenCase>,
}

#[derive(Debug, Deserialize)]
struct MetricThresholds {
    minimum_ontology_conformance_rate: f64,
    minimum_invalid_rejection_rate: f64,
    required_illegal_production_writes: usize,
}

#[derive(Debug, Deserialize)]
struct GoldenCase {
    id: String,
    text: String,
    candidates: LLMExtractionOutput,
    expected: ExpectedResult,
}

#[derive(Debug, Deserialize)]
struct ExpectedResult {
    staged_nodes: usize,
    staged_edges: usize,
    accepted_decisions: usize,
    rejected_decisions: usize,
    ask_query: String,
    deterministic_passed: bool,
    ask_violation: bool,
}

#[test]
fn golden_ontology_ke_extract_canonicalize_and_gate_multi_metric() {
    let suite: GoldenSuite =
        serde_json::from_str(include_str!("fixtures/ontology_ke_golden/golden.json"))
            .expect("frozen ontology KE golden fixture must be valid JSON");
    assert_eq!(suite.suite_version, "ontology-ke-golden/v1");
    assert!(
        suite.cases.len() >= suite.minimum_case_count,
        "fixture reduction requires an explicit frozen-suite revision"
    );

    let ids: HashSet<_> = suite.cases.iter().map(|case| case.id.as_str()).collect();
    for required in &suite.required_case_ids {
        assert!(
            ids.contains(required.as_str()),
            "required anti-decay case {required} is missing"
        );
    }

    let claims = IsolationClaims::from_verified("golden-tenant", "golden-project", "golden-eval")
        .expect("fixed test claims are valid");
    let kg = KnowledgeGraphStore::new().expect("in-memory graph store");
    let mut cases_matching_contract = 0usize;
    let mut expected_invalid_cases = 0usize;
    let mut rejected_invalid_cases = 0usize;
    let mut illegal_production_writes = 0usize;

    for case in &suite.cases {
        assert!(
            !case.text.trim().is_empty(),
            "{} needs source text",
            case.id
        );
        let canonical = canonicalize(&case.candidates, &suite.promoted_ontology);
        let accepted = canonical
            .decisions
            .iter()
            .filter(|decision| decision.status == CanonicalizationStatus::Accepted)
            .count();
        let rejected = canonical
            .decisions
            .iter()
            .filter(|decision| decision.status == CanonicalizationStatus::Rejected)
            .count();
        let matches_contract = canonical.extraction.nodes.len() == case.expected.staged_nodes
            && canonical.extraction.edges.len() == case.expected.staged_edges
            && accepted == case.expected.accepted_decisions
            && rejected == case.expected.rejected_decisions;
        assert!(matches_contract, "golden case {} changed behavior", case.id);
        if matches_contract {
            cases_matching_contract += 1;
        }
        if case.expected.rejected_decisions > 0 {
            expected_invalid_cases += 1;
            if rejected >= case.expected.rejected_decisions {
                rejected_invalid_cases += 1;
            }
        }

        let extraction_id = format!("golden-{}", case.id);
        let staging_graph = kg
            .staging_graph_iri_for_claims(&claims, &extraction_id)
            .expect("golden extraction id is valid");
        let mapped = RdfMapper::map_extraction(&canonical.extraction, &staging_graph);
        if !mapped.quads.is_empty() {
            kg.update_staging_for_claims(
                &claims,
                &extraction_id,
                &ClaimsGraphUpdate::insert_data(RdfMapper::quads_to_sparql_triples(&mapped.quads)),
            )
            .expect("golden data stages successfully");
        }

        let report = KgQualityGate::evaluate(
            &kg,
            &claims,
            &extraction_id,
            &QualityGateRequest {
                assertions: vec![SparqlAskAssertion {
                    code: "unpromoted_type_must_not_stage".into(),
                    query: case.expected.ask_query.clone(),
                }],
                policy_version: "ontology-ke-golden/v1".into(),
                arbitration: QualityCoverageArbitration::Compliance,
                pyshacl_shapes_ttl: None,
                judge: None,
            },
            None,
        )
        .expect("quality gate must evaluate the staged golden graph");
        assert_eq!(
            report.ask[0].violation, case.expected.ask_violation,
            "golden ASK result changed for {}",
            case.id
        );
        assert_eq!(
            report.deterministic_passed, case.expected.deterministic_passed,
            "golden deterministic gate status changed for {}",
            case.id
        );
        assert_eq!(report.production_write, false);
        if !kg
            .query_sparql_for_claims(&claims, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
            .expect("production graph query")
            .is_empty()
        {
            illegal_production_writes += 1;
        }
    }

    let ontology_conformance_rate = cases_matching_contract as f64 / suite.cases.len() as f64;
    let invalid_rejection_rate = rejected_invalid_cases as f64 / expected_invalid_cases as f64;
    assert!(
        ontology_conformance_rate >= suite.metrics.minimum_ontology_conformance_rate,
        "ontology conformance rate {ontology_conformance_rate} is below the frozen threshold"
    );
    assert!(
        invalid_rejection_rate >= suite.metrics.minimum_invalid_rejection_rate,
        "invalid rejection rate {invalid_rejection_rate} is below the frozen threshold"
    );
    assert_eq!(
        illegal_production_writes, suite.metrics.required_illegal_production_writes,
        "golden extraction/gate evaluation must never write production"
    );
}
