//! Deterministic, reviewable mapping from open extraction candidates to the
//! explicitly promoted ontology definition. This module never creates types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{
    ontology_layer::OntologyDefinition,
    types::{EdgeDef, LLMExtractionOutput, NodeDef},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalizationStatus {
    Accepted,
    NeedsReview,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalizationDecision {
    pub kind: String,
    pub candidate: String,
    pub status: CanonicalizationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_iri: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct CanonicalizationResult {
    pub extraction: LLMExtractionOutput,
    pub decisions: Vec<CanonicalizationDecision>,
}

/// Canonicalizes only exact (case-insensitive, whitespace-normalized) matches
/// against already-promoted ObjectType and LinkType identifiers, labels, or
/// IRIs. Ambiguous and unsupported candidates retain an explicit decision.
pub fn canonicalize(
    extracted: &LLMExtractionOutput,
    ontology: &OntologyDefinition,
) -> CanonicalizationResult {
    let object_index = index_terms(
        ontology
            .object_types
            .iter()
            .map(|term| (&term.id, &term.label, &term.iri)),
    );
    let link_index = index_terms(
        ontology
            .link_types
            .iter()
            .map(|term| (&term.id, &term.label, &term.iri)),
    );

    let mut decisions = Vec::new();
    let mut node_types = HashMap::new();
    let mut nodes = Vec::new();
    for node in &extracted.nodes {
        match resolve(&object_index, &node.node_type) {
            Resolution::Accepted(iri) => {
                node_types.insert(node.id.clone(), iri.clone());
                let mut canonical = node.clone();
                canonical.node_type = iri.clone();
                nodes.push(canonical);
                decisions.push(accepted("object_type", &node.node_type, iri));
            }
            Resolution::NeedsReview(candidates) => decisions.push(CanonicalizationDecision {
                kind: "object_type".into(),
                candidate: node.node_type.clone(),
                status: CanonicalizationStatus::NeedsReview,
                canonical_iri: None,
                candidates,
                reason: "multiple promoted ObjectTypes match this candidate".into(),
            }),
            Resolution::Rejected => decisions.push(CanonicalizationDecision {
                kind: "object_type".into(),
                candidate: node.node_type.clone(),
                status: CanonicalizationStatus::Rejected,
                canonical_iri: None,
                candidates: vec![],
                reason: "no promoted ObjectType matches this candidate".into(),
            }),
        }
    }

    let mut edges = Vec::new();
    for edge in &extracted.edges {
        let Some(source_type) = node_types.get(&edge.source) else {
            decisions.push(rejected_edge(
                edge,
                "source was not canonicalized to a promoted ObjectType",
            ));
            continue;
        };
        let Some(target_type) = node_types.get(&edge.target) else {
            decisions.push(rejected_edge(
                edge,
                "target was not canonicalized to a promoted ObjectType",
            ));
            continue;
        };
        match resolve(&link_index, &edge.relation) {
            Resolution::Accepted(iri) => {
                let link = ontology
                    .link_types
                    .iter()
                    .find(|link| link.iri == iri)
                    .expect("indexed link exists");
                let source_matches = ontology
                    .object_types
                    .iter()
                    .any(|object| object.id == link.source && object.iri == *source_type);
                let target_matches = ontology
                    .object_types
                    .iter()
                    .any(|object| object.id == link.target && object.iri == *target_type);
                if !source_matches || !target_matches {
                    decisions.push(rejected_edge(
                        edge,
                        "canonical LinkType source/target constraints do not match canonical node types",
                    ));
                    continue;
                }
                let mut canonical = edge.clone();
                canonical.relation = iri.clone();
                edges.push(canonical);
                decisions.push(accepted("link_type", &edge.relation, iri));
            }
            Resolution::NeedsReview(candidates) => decisions.push(CanonicalizationDecision {
                kind: "link_type".into(),
                candidate: edge.relation.clone(),
                status: CanonicalizationStatus::NeedsReview,
                canonical_iri: None,
                candidates,
                reason: "multiple promoted LinkTypes match this candidate".into(),
            }),
            Resolution::Rejected => decisions.push(rejected_edge(
                edge,
                "no promoted LinkType matches this candidate",
            )),
        }
    }

    CanonicalizationResult {
        extraction: LLMExtractionOutput { nodes, edges },
        decisions,
    }
}

enum Resolution {
    Accepted(String),
    NeedsReview(Vec<String>),
    Rejected,
}

fn index_terms<'a>(
    terms: impl Iterator<Item = (&'a String, &'a String, &'a String)>,
) -> HashMap<String, Vec<String>> {
    let mut index = HashMap::<String, Vec<String>>::new();
    for (id, label, iri) in terms {
        for key in [id, label, iri] {
            let values = index.entry(normalize(key)).or_default();
            if !values.contains(iri) {
                values.push(iri.clone());
            }
        }
    }
    index
}

fn resolve(index: &HashMap<String, Vec<String>>, candidate: &str) -> Resolution {
    match index.get(&normalize(candidate)) {
        Some(matches) if matches.len() == 1 => Resolution::Accepted(matches[0].clone()),
        Some(matches) => Resolution::NeedsReview(matches.clone()),
        None => Resolution::Rejected,
    }
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
}

fn accepted(kind: &str, candidate: &str, iri: String) -> CanonicalizationDecision {
    CanonicalizationDecision {
        kind: kind.into(),
        candidate: candidate.into(),
        status: CanonicalizationStatus::Accepted,
        canonical_iri: Some(iri),
        candidates: vec![],
        reason: "exact match to a promoted ontology term".into(),
    }
}

fn rejected_edge(edge: &EdgeDef, reason: &str) -> CanonicalizationDecision {
    CanonicalizationDecision {
        kind: "link_type".into(),
        candidate: edge.relation.clone(),
        status: CanonicalizationStatus::Rejected,
        canonical_iri: None,
        candidates: vec![],
        reason: format!("{reason}; edge {} -> {}", edge.source, edge.target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_graph::ontology_layer::ev_repair_ontology;
    use std::collections::HashMap;

    #[test]
    fn accepts_only_promoted_types_and_links() {
        let input = LLMExtractionOutput {
            nodes: vec![
                NodeDef {
                    id: "fault".into(),
                    node_type: "FaultCode".into(),
                    label: "P0A1".into(),
                    description: None,
                    properties: HashMap::new(),
                },
                NodeDef {
                    id: "system".into(),
                    node_type: "系统".into(),
                    label: "Battery".into(),
                    description: None,
                    properties: HashMap::new(),
                },
            ],
            edges: vec![EdgeDef {
                source: "fault".into(),
                target: "system".into(),
                relation: "影响系统".into(),
                properties: HashMap::new(),
            }],
        };
        let result = canonicalize(&input, &ev_repair_ontology());
        assert_eq!(result.extraction.nodes.len(), 2);
        assert_eq!(result.extraction.edges.len(), 1);
        assert_eq!(
            result.extraction.nodes[0].node_type,
            "https://agentos.ontology/ev/FaultCode"
        );
        assert_eq!(
            result.extraction.edges[0].relation,
            "https://agentos.ontology/ev/affectsSystem"
        );
    }

    #[test]
    fn retains_rejected_candidates_as_decisions_without_creating_types() {
        let input = LLMExtractionOutput {
            nodes: vec![NodeDef {
                id: "x".into(),
                node_type: "UnpromotedThing".into(),
                label: "X".into(),
                description: None,
                properties: HashMap::new(),
            }],
            edges: vec![],
        };
        let result = canonicalize(&input, &ev_repair_ontology());
        assert!(result.extraction.nodes.is_empty());
        assert_eq!(result.decisions[0].status, CanonicalizationStatus::Rejected);
    }
}
