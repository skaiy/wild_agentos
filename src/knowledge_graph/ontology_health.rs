//! Read-only slow-loop evidence for ontology health and extraction goals.
//!
//! This module intentionally aggregates only claims-scoped, persisted evidence.
//! It neither creates drafts nor mutates staging, production, or ontology metadata.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::isolation::IsolationClaims;

use super::{
    ontology_layer::OntologyDefinition,
    quality_gate::QualityGateReport,
    store::{KnowledgeGraphStore, PendingExtractionReview, PendingTypeDraft},
};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

#[derive(Debug, Clone, Serialize)]
pub struct OntologyHealthReport {
    pub scope: HealthScope,
    pub generated_at: String,
    pub evidence: HealthEvidence,
    pub staging: StagingHealth,
    pub quality_gates: QualityGateHealth,
    pub sparse_object_types: Vec<SparseObjectType>,
    pub stale_type_drafts: StaleDraftHealth,
    pub production_write: bool,
    pub type_drafts_created: bool,
    pub promotion_performed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthScope {
    pub tenant_id: String,
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthEvidence {
    /// Only extractions with a persisted review/gate record can be enumerated.
    pub reviewed_extractions: usize,
    pub quality_gate_reports: usize,
    pub type_drafts_observed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StagingHealth {
    pub canonicalization_decisions: usize,
    pub accepted: usize,
    pub needs_review: usize,
    pub rejected: usize,
    /// Rejected decisions divided by all persisted canonicalization decisions.
    pub reject_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QualityGateHealth {
    pub passed: usize,
    pub failed: usize,
    pub failures_by_anchor: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SparseObjectType {
    pub id: String,
    pub iri: String,
    pub instance_count: u64,
    pub sparse: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleDraftHealth {
    pub stale_after_hours: i64,
    pub stale: Vec<StaleTypeDraft>,
    pub expired: Vec<StaleTypeDraft>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleTypeDraft {
    pub draft_id: String,
    pub source: String,
    pub created_at: String,
    pub expires_at: String,
    pub age_hours: i64,
}

pub fn report(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    ontology: &OntologyDefinition,
    sparse_threshold: u64,
    stale_after_hours: i64,
) -> Result<OntologyHealthReport, String> {
    let reviews = kg.list_extraction_reviews_for_claims(claims)?;
    let reports = reports_from_reviews(&reviews);
    let staging = staging_health(kg, claims, &reviews)?;
    let type_counts = production_type_counts(kg, claims)?;
    let drafts = kg.list_type_drafts_for_claims(claims)?;
    let stale_type_drafts = stale_drafts(drafts, stale_after_hours);

    let reviewed_extractions = reviews
        .iter()
        .map(|review| &review.extraction_id)
        .collect::<HashSet<_>>()
        .len();
    Ok(OntologyHealthReport {
        scope: HealthScope {
            tenant_id: claims.tenant_id().to_owned(),
            project_id: claims.project_id().to_owned(),
        },
        generated_at: Utc::now().to_rfc3339(),
        evidence: HealthEvidence {
            reviewed_extractions,
            quality_gate_reports: reports.len(),
            type_drafts_observed: stale_type_drafts.stale.len() + stale_type_drafts.expired.len(),
        },
        staging,
        quality_gates: quality_gate_health(&reports),
        sparse_object_types: ontology
            .object_types
            .iter()
            .map(|object| {
                let count = type_counts.get(&object.iri).copied().unwrap_or_default();
                SparseObjectType {
                    id: object.id.clone(),
                    iri: object.iri.clone(),
                    instance_count: count,
                    sparse: count <= sparse_threshold,
                }
            })
            .collect(),
        stale_type_drafts,
        production_write: false,
        type_drafts_created: false,
        promotion_performed: false,
    })
}

fn reports_from_reviews(reviews: &[PendingExtractionReview]) -> Vec<QualityGateReport> {
    reviews
        .iter()
        .filter_map(|review| serde_json::from_str(&review.report_json).ok())
        .collect()
}

fn staging_health(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    reviews: &[PendingExtractionReview],
) -> Result<StagingHealth, String> {
    let extraction_ids = reviews
        .iter()
        .map(|review| review.extraction_id.as_str())
        .collect::<HashSet<_>>();
    let mut health = StagingHealth {
        canonicalization_decisions: 0,
        accepted: 0,
        needs_review: 0,
        rejected: 0,
        reject_rate: 0.0,
    };
    for extraction_id in extraction_ids {
        let query = "SELECT ?json WHERE { \
            ?run <https://agentos.ontology/extraction/decision> ?decision . \
            ?decision <https://agentos.ontology/extraction/json> ?json \
        }";
        for row in kg.query_staging_for_claims(claims, extraction_id, query)? {
            let Some(raw) = row.get("?json").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(json) = decode_sparql_literal(raw) else {
                continue;
            };
            let Ok(decision) = serde_json::from_str::<serde_json::Value>(&json) else {
                continue;
            };
            let Some(status) = decision.get("status").and_then(serde_json::Value::as_str) else {
                continue;
            };
            health.canonicalization_decisions += 1;
            match status {
                "accepted" => health.accepted += 1,
                "needs_review" => health.needs_review += 1,
                "rejected" => health.rejected += 1,
                _ => health.canonicalization_decisions -= 1,
            }
        }
    }
    if health.canonicalization_decisions > 0 {
        health.reject_rate = health.rejected as f64 / health.canonicalization_decisions as f64;
    }
    Ok(health)
}

fn quality_gate_health(reports: &[QualityGateReport]) -> QualityGateHealth {
    let mut failures_by_anchor = BTreeMap::new();
    let mut passed = 0;
    for report in reports {
        if report.passed {
            passed += 1;
        }
        for result in &report.ask {
            if result.violation {
                *failures_by_anchor.entry(result.code.clone()).or_insert(0) += 1;
            }
        }
        if report
            .pyshacl
            .as_ref()
            .and_then(|value| value.get("conforms"))
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        {
            *failures_by_anchor.entry("pyshacl".to_owned()).or_insert(0) += 1;
        }
        if report.deterministic_passed && !report.passed {
            *failures_by_anchor.entry("judge".to_owned()).or_insert(0) += 1;
        }
    }
    QualityGateHealth {
        passed,
        failed: reports.len() - passed,
        failures_by_anchor,
    }
}

fn production_type_counts(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
) -> Result<BTreeMap<String, u64>, String> {
    let query = format!(
        "SELECT ?type (COUNT(DISTINCT ?subject) AS ?count) WHERE {{ \
         ?subject <{RDF_TYPE}> ?type }} GROUP BY ?type"
    );
    kg.query_sparql_for_claims(claims, &query).map(|rows| {
        rows.into_iter()
            .filter_map(|row| {
                let kind = row.get("?type")?.as_str()?.to_owned();
                let count = row.get("?count")?.as_str()?.parse().ok()?;
                Some((kind, count))
            })
            .collect()
    })
}

fn stale_drafts(drafts: Vec<PendingTypeDraft>, stale_after_hours: i64) -> StaleDraftHealth {
    let now = Utc::now();
    let cutoff = now - Duration::hours(stale_after_hours);
    let mut stale = Vec::new();
    let mut expired = Vec::new();
    for draft in drafts {
        let created = DateTime::parse_from_rfc3339(&draft.created_at).ok();
        let expires = DateTime::parse_from_rfc3339(&draft.expires_at).ok();
        let age_hours = created
            .map(|created| (now - created.with_timezone(&Utc)).num_hours())
            .unwrap_or_default();
        let view = StaleTypeDraft {
            draft_id: draft.draft_id,
            source: draft.source,
            created_at: draft.created_at,
            expires_at: draft.expires_at,
            age_hours,
        };
        if expires.map(|expires| expires <= now).unwrap_or(true) {
            expired.push(view);
        } else if created.map(|created| created <= cutoff).unwrap_or(true) {
            stale.push(view);
        }
    }
    StaleDraftHealth {
        stale_after_hours,
        stale,
        expired,
    }
}

fn decode_sparql_literal(value: &str) -> Option<String> {
    serde_json::from_str::<String>(&format!("\"{value}\"")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_graph::{ontology_layer::ev_repair_ontology, store::ClaimsGraphUpdate};

    #[test]
    fn report_is_claims_scoped_and_read_only() {
        let store = std::sync::Arc::new(oxigraph::store::Store::new().unwrap());
        let kg = KnowledgeGraphStore::with_shared_store(store).unwrap();
        let claims = IsolationClaims::from_verified("tenant", "project", "actor").unwrap();
        kg.update_for_claims(
            &claims,
            &ClaimsGraphUpdate::insert_data(
                "<urn:vehicle> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
                 <https://agentos.ontology/ev/Vehicle> .",
            ),
        )
        .unwrap();
        let result = report(&kg, &claims, &ev_repair_ontology(), 1, 24).unwrap();
        assert!(!result.production_write);
        assert!(!result.type_drafts_created);
        assert!(!result.promotion_performed);
        assert!(result
            .sparse_object_types
            .iter()
            .any(|item| item.id == "Vehicle" && item.instance_count == 1));
    }
}
