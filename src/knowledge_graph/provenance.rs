//! Server-minted PROV-O companion graph (`graph://{tenant}/{project}/prov`).
//!
//! Callers never name this graph; only kernel paths in this module write to it.

use chrono::Utc;
use oxigraph::model::{
    GraphName, GraphNameRef, NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Quad,
    Term, Triple,
};
use oxigraph::store::Store;

use crate::isolation::IsolationClaims;

use super::rdf_mapper::RdfMapper;
use super::store::KnowledgeGraphStore;
use super::types::{RdfQuad, RdfValue};

pub const PROV_NS: &str = "http://www.w3.org/ns/prov#";
pub const WAO_PROV_VOCAB: &str = "https://wildagentos.org/ontology/prov#";
const WAO_PROV_RESOURCE: &str = "https://wildagentos.org/ontology/prov/";

/// Best-effort prov writes must not change the primary HTTP outcome; failures are logged only.
pub(crate) fn log_provenance_write_failure(activity_kind: &'static str, error: &str) {
    tracing::warn!(
        provenance_activity_kind = activity_kind,
        error = error,
        "failed to record provenance activity in server-minted prov graph"
    );
}

const EXTRACTION_STAGING_PROV_NS: &str = "https://agentos.ontology/extraction/";
const RML_META_NS: &str = "https://agentos.ontology/meta/";

/// Fail-closed cap on backing-store quads examined per prov read (full-store walk).
pub(crate) const PROV_READ_MAX_STORE_QUADS_SCANNED: usize = 2_097_152;
/// Fail-closed cap on companion-graph quads yielded per prov read.
pub(crate) const PROV_READ_MAX_COMPANION_GRAPH_QUADS: usize = 65_536;

const ERR_PROV_READ_STORE_SCAN_BUDGET: &str =
    "provenance read path exceeded store scan budget (fail-closed)";
const ERR_PROV_READ_GRAPH_QUAD_BUDGET: &str =
    "provenance read path exceeded companion graph quad budget (fail-closed)";

fn wao(pred: &str) -> String {
    format!("{WAO_PROV_VOCAB}{pred}")
}

fn prov(pred: &str) -> String {
    format!("{PROV_NS}{pred}")
}

/// Minted companion graph IRI; not exposed to caller-controlled writes.
pub fn prov_graph_iri_for_claims(claims: &IsolationClaims) -> Result<String, String> {
    let production = claims
        .graph_iri()
        .map_err(|e| format!("invalid verified graph scope: {e}"))?;
    Ok(format!("{production}/prov"))
}

pub fn agent_iri_for_claims(claims: &IsolationClaims) -> String {
    format!(
        "{WAO_PROV_RESOURCE}agent/{}",
        RdfMapper::sanitize_id(claims.actor_id())
    )
}

fn document_iri(blob_id: &str, version: &str) -> String {
    format!(
        "{WAO_PROV_RESOURCE}document/{}/{}",
        RdfMapper::sanitize_id(blob_id),
        RdfMapper::sanitize_id(version)
    )
}

fn page_iri(blob_id: &str, version: &str) -> String {
    format!(
        "{WAO_PROV_RESOURCE}page/{}/{}/1",
        RdfMapper::sanitize_id(blob_id),
        RdfMapper::sanitize_id(version)
    )
}

pub fn chunk_iri_for_extraction(extraction_id: &str) -> String {
    format!(
        "{WAO_PROV_RESOURCE}chunk/{}",
        RdfMapper::sanitize_id(extraction_id)
    )
}

pub fn activity_iri(local_id: &str) -> String {
    format!(
        "{WAO_PROV_RESOURCE}activity/{}",
        RdfMapper::sanitize_id(local_id)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceActivityKind {
    ExtractionStaging,
    Materialization,
    ActionAudit,
    RmlMaterialization,
}

impl ProvenanceActivityKind {
    fn label(self) -> &'static str {
        match self {
            Self::ExtractionStaging => "extraction_staging",
            Self::Materialization => "materialization",
            Self::ActionAudit => "action_audit",
            Self::RmlMaterialization => "rml_materialization",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceOutcome {
    Succeeded,
    Failed,
    Blocked,
}

impl ProvenanceOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

pub struct ExtractionSourceRef {
    pub blob_id: String,
    pub version: String,
    pub extractor: String,
    pub model: Option<String>,
}

impl KnowledgeGraphStore {
    pub fn prov_graph_iri_for_claims(claims: &IsolationClaims) -> Result<String, String> {
        prov_graph_iri_for_claims(claims)
    }

    /// Writes PROV-O records only to the server-minted prov graph.
    pub fn record_provenance_quads_for_claims(
        &self,
        claims: &IsolationClaims,
        quads: &[Quad],
    ) -> Result<(), String> {
        if quads.is_empty() {
            return Ok(());
        }
        let graph = prov_graph_iri_for_claims(claims)?;
        let graph_name =
            NamedNode::new(graph.as_str()).map_err(|e| format!("invalid prov graph IRI: {e}"))?;
        for quad in quads {
            let mut q = quad.clone();
            q.graph_name = GraphName::NamedNode(graph_name.clone());
            self.store_arc()
                .insert(&q)
                .map_err(|e| format!("prov graph insert failed: {e}"))?;
        }
        Ok(())
    }

    pub fn record_extraction_staging_provenance(
        &self,
        claims: &IsolationClaims,
        extraction_id: &str,
        source: &ExtractionSourceRef,
    ) -> Result<(), String> {
        let started = Utc::now().to_rfc3339();
        let local_id = format!("extraction/{extraction_id}");
        let activity = activity_iri(&local_id);
        let chunk = chunk_iri_for_extraction(extraction_id);
        let document = document_iri(&source.blob_id, &source.version);
        let page = page_iri(&source.blob_id, &source.version);
        let triple_terms = self.collect_staging_fact_triple_terms(claims, extraction_id)?;
        let quads = self.build_activity_bundle(
            claims,
            &activity,
            ProvenanceActivityKind::ExtractionStaging,
            ProvenanceOutcome::Succeeded,
            &started,
            &started,
            Some(&chunk),
            source.model.as_deref(),
            &triple_terms,
            |writer| {
                writer.entity_dag(&document, &page, &chunk);
                writer.literal(&wao("extractor"), &source.extractor);
                writer.literal(&wao("extractionId"), extraction_id);
            },
        )?;
        self.record_provenance_quads_for_claims(claims, &quads)
    }

    pub fn record_materialization_provenance(
        &self,
        claims: &IsolationClaims,
        extraction_id: &str,
        outcome: ProvenanceOutcome,
        model: Option<&str>,
    ) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let local_id = format!("materialize/{extraction_id}");
        let activity = activity_iri(&local_id);
        let chunk = chunk_iri_for_extraction(extraction_id);
        let triple_terms = if outcome == ProvenanceOutcome::Succeeded {
            self.collect_staging_fact_triple_terms(claims, extraction_id)?
        } else {
            Vec::new()
        };
        let quads = self.build_activity_bundle(
            claims,
            &activity,
            ProvenanceActivityKind::Materialization,
            outcome,
            &now,
            &now,
            Some(&chunk),
            model,
            &triple_terms,
            |writer| writer.literal(&wao("extractionId"), extraction_id),
        )?;
        self.record_provenance_quads_for_claims(claims, &quads)
    }

    pub fn record_rml_materialization_provenance(
        &self,
        claims: &IsolationClaims,
        run_iri: &str,
        mapping_checksum: &str,
        fact_triple_terms: Vec<Term>,
        outcome: ProvenanceOutcome,
    ) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let local_id = format!(
            "rml/{}",
            RdfMapper::sanitize_id(run_iri.rsplit('/').next().unwrap_or("run"))
        );
        let activity = activity_iri(&local_id);
        let fact_triple_terms = if outcome == ProvenanceOutcome::Succeeded {
            fact_triple_terms
        } else {
            Vec::new()
        };
        let quads = self.build_activity_bundle(
            claims,
            &activity,
            ProvenanceActivityKind::RmlMaterialization,
            outcome,
            &now,
            &now,
            None,
            None,
            &fact_triple_terms,
            |writer| {
                writer.literal(&wao("mappingChecksum"), mapping_checksum);
                writer.iri(&wao("rmlRun"), run_iri);
            },
        )?;
        self.record_provenance_quads_for_claims(claims, &quads)
    }

    pub fn record_action_audit_provenance(
        &self,
        claims: &IsolationClaims,
        action_id: &str,
        staging_id: &str,
        decision: &str,
        violations: &[String],
        timestamp: &str,
    ) -> Result<(), String> {
        let safe_timestamp = timestamp.replace(':', "-");
        let local_id = format!(
            "action-audit/{}/{}/{}",
            RdfMapper::sanitize_id(action_id),
            RdfMapper::sanitize_id(staging_id),
            RdfMapper::sanitize_id(&safe_timestamp)
        );
        let activity = activity_iri(&local_id);
        let quads = self.build_activity_bundle(
            claims,
            &activity,
            ProvenanceActivityKind::ActionAudit,
            ProvenanceOutcome::Succeeded,
            timestamp,
            timestamp,
            None,
            None,
            &[],
            |writer| {
                writer.literal(&wao("actionId"), action_id);
                writer.literal(&wao("stagingId"), staging_id);
                writer.literal(&wao("decision"), decision);
                writer.literal(&wao("violationCount"), violations.len().to_string());
            },
        )?;
        self.record_provenance_quads_for_claims(claims, &quads)
    }

    pub fn prov_activity_exists_for_claims(
        &self,
        claims: &IsolationClaims,
        activity_local_id: &str,
    ) -> Result<bool, String> {
        let graph = prov_graph_iri_for_claims(claims)?;
        let activity = activity_iri(activity_local_id);
        self.quad_exists_in_graph(&graph, &activity, &prov("type"), &prov("Activity"))
    }

    pub fn query_provenance_for_subject(
        &self,
        claims: &IsolationClaims,
        subject_iri: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        let graph = prov_graph_iri_for_claims(claims)?;
        let subject =
            NamedNodeRef::new(subject_iri).map_err(|e| format!("invalid subject IRI: {e}"))?;
        let contains = wao("contains");
        let mut hits = Vec::new();
        for_each_quad_in_named_graph(
            self.store_arc(),
            &graph,
            ProvReadScanLimits::production(),
            |quad| {
                if quad.predicate.as_str() != contains {
                    return Ok(());
                }
                if let Term::Triple(triple) = &quad.object {
                    if triple.subject.as_ref() == NamedOrBlankNodeRef::NamedNode(subject) {
                        hits.push(serde_json::json!({
                            "activity": quad.subject.to_string(),
                            "triple": {
                                "subject": triple.subject.to_string(),
                                "predicate": triple.predicate.to_string(),
                                "object": triple.object.to_string(),
                            }
                        }));
                    }
                }
                Ok(())
            },
        )?;
        Ok(hits)
    }

    pub fn get_provenance_activity(
        &self,
        claims: &IsolationClaims,
        activity_local_id: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let graph = prov_graph_iri_for_claims(claims)?;
        let activity = activity_iri(activity_local_id);
        let activity_node =
            NamedNode::new(activity.as_str()).map_err(|e| format!("invalid activity IRI: {e}"))?;
        let mut props = serde_json::Map::new();
        let mut contains = Vec::new();
        let mut found = false;
        let contains_pred = wao("contains");
        for_each_quad_in_named_graph(
            self.store_arc(),
            &graph,
            ProvReadScanLimits::production(),
            |quad| {
                if quad.subject.as_ref() != NamedOrBlankNodeRef::NamedNode(activity_node.as_ref()) {
                    return Ok(());
                }
                found = true;
                if quad.predicate.as_str() == contains_pred {
                    if let Term::Triple(triple) = &quad.object {
                        contains.push(serde_json::json!({
                            "subject": triple.subject.to_string(),
                            "predicate": triple.predicate.to_string(),
                            "object": triple.object.to_string(),
                        }));
                    }
                } else {
                    props.insert(
                        quad.predicate.to_string(),
                        serde_json::Value::String(quad.object.to_string()),
                    );
                }
                Ok(())
            },
        )?;
        if !found {
            return Ok(None);
        }
        Ok(Some(serde_json::json!({
            "activity": activity,
            "properties": props,
            "contains": contains,
        })))
    }

    fn collect_staging_fact_triple_terms(
        &self,
        claims: &IsolationClaims,
        staging_id: &str,
    ) -> Result<Vec<Term>, String> {
        let graph = self.staging_graph_iri_for_claims(claims, staging_id)?;
        self.collect_fact_triple_terms_in_graph(&graph)
    }

    /// Full-store walk on the provenance write path; bounded like read-path scans.
    fn collect_fact_triple_terms_in_graph(&self, graph: &str) -> Result<Vec<Term>, String> {
        let mut terms = Vec::new();
        let mut budget = ProvReadScanBudget::new(ProvReadScanLimits::production());
        for quad in self.store_arc().iter() {
            let quad = quad.map_err(|e| format!("read graph failed: {e}"))?;
            budget.record_store_quad()?;
            if !graph_matches(&quad, graph) {
                continue;
            }
            if is_provenance_or_meta_triple(&quad) {
                continue;
            }
            terms.push(quad_to_triple_term(&quad));
        }
        Ok(terms)
    }

    /// Counts `prov:Activity` nodes in the minted companion graph (test / contract aid).
    pub fn count_prov_activities_for_claims(
        &self,
        claims: &IsolationClaims,
    ) -> Result<usize, String> {
        let graph = prov_graph_iri_for_claims(claims)?;
        let prov_type = prov("type");
        let activity_type = prov("Activity");
        let mut count = 0usize;
        for_each_quad_in_named_graph(
            self.store_arc(),
            &graph,
            ProvReadScanLimits::production(),
            |quad| {
                if quad.predicate.as_str() == prov_type && quad.object.to_string() == activity_type
                {
                    count += 1;
                }
                Ok(())
            },
        )?;
        Ok(count)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_activity_bundle(
        &self,
        claims: &IsolationClaims,
        activity: &str,
        kind: ProvenanceActivityKind,
        outcome: ProvenanceOutcome,
        started: &str,
        ended: &str,
        derived_from_chunk: Option<&str>,
        model: Option<&str>,
        triple_terms: &[Term],
        extra: impl FnOnce(&mut ProvWriter),
    ) -> Result<Vec<Quad>, String> {
        let mut writer = ProvWriter::new(claims, activity);
        writer.iri(&prov("type"), &prov("Activity"));
        writer.literal(&wao("activityKind"), kind.label());
        writer.literal(&wao("activityOutcome"), outcome.label());
        writer.literal(&prov("startedAtTime"), started);
        writer.literal(&prov("endedAtTime"), ended);
        writer.iri(&prov("wasAssociatedWith"), &agent_iri_for_claims(claims));
        writer.literal(&wao("componentVersion"), env!("CARGO_PKG_VERSION"));
        writer.literal(&wao("ontologyVersion"), "promoted-runtime");
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            writer.literal(&wao("modelId"), model);
        }
        if let Some(chunk) = derived_from_chunk {
            writer.iri(&prov("wasDerivedFrom"), chunk);
        }
        extra(&mut writer);
        for term in triple_terms {
            writer.triple_term(&wao("contains"), term);
        }
        Ok(writer.finish())
    }

    fn quad_exists_in_graph(
        &self,
        graph: &str,
        subject: &str,
        predicate: &str,
        object: &str,
    ) -> Result<bool, String> {
        let mut found = false;
        for_each_quad_in_named_graph(
            self.store_arc(),
            graph,
            ProvReadScanLimits::production(),
            |quad| {
                if quad.subject.to_string() == subject
                    && quad.predicate.as_str() == predicate
                    && quad.object.to_string() == object
                {
                    found = true;
                }
                Ok(())
            },
        )?;
        Ok(found)
    }
}

struct ProvWriter {
    quads: Vec<Quad>,
    activity: NamedNode,
}

impl ProvWriter {
    fn new(claims: &IsolationClaims, activity: &str) -> Self {
        let _ = claims;
        Self {
            quads: Vec::new(),
            activity: NamedNode::new(activity).expect("activity IRI must be valid"),
        }
    }

    fn iri(&mut self, predicate: &str, object_iri: &str) {
        let predicate = NamedNode::new(predicate).expect("predicate IRI");
        let object = NamedNode::new(object_iri).expect("object IRI");
        self.quads.push(Quad {
            subject: self.activity.clone().into(),
            predicate,
            object: object.into(),
            graph_name: GraphName::DefaultGraph,
        });
    }

    fn literal(&mut self, predicate: &str, value: impl Into<String>) {
        let predicate = NamedNode::new(predicate).expect("predicate IRI");
        self.quads.push(Quad {
            subject: self.activity.clone().into(),
            predicate,
            object: Term::Literal(oxigraph::model::Literal::new_simple_literal(value.into())),
            graph_name: GraphName::DefaultGraph,
        });
    }

    fn triple_term(&mut self, predicate: &str, term: &Term) {
        let predicate = NamedNode::new(predicate).expect("predicate IRI");
        self.quads.push(Quad {
            subject: self.activity.clone().into(),
            predicate,
            object: term.clone(),
            graph_name: GraphName::DefaultGraph,
        });
    }

    fn entity_dag(&mut self, document: &str, page: &str, chunk: &str) {
        self.entity_node(document, "document");
        self.entity_node(page, "page");
        self.entity_node(chunk, "chunk");
        self.entity_link(page, document);
        self.entity_link(chunk, page);
    }

    fn entity_node(&mut self, iri: &str, kind: &str) {
        let node = NamedNode::new(iri).expect("entity IRI");
        self.quads.push(Quad {
            subject: node.clone().into(),
            predicate: NamedNode::new(prov("type")).expect("prov type"),
            object: NamedNode::new(prov("Entity")).expect("prov Entity").into(),
            graph_name: GraphName::DefaultGraph,
        });
        self.quads.push(Quad {
            subject: node.clone().into(),
            predicate: NamedNode::new(wao("entityKind")).expect("entity kind"),
            object: Term::Literal(oxigraph::model::Literal::new_simple_literal(kind)),
            graph_name: GraphName::DefaultGraph,
        });
    }

    fn entity_link(&mut self, child: &str, parent: &str) {
        let child = NamedNode::new(child).expect("child IRI");
        let parent = NamedNode::new(parent).expect("parent IRI");
        self.quads.push(Quad {
            subject: child.into(),
            predicate: NamedNode::new(prov("wasDerivedFrom")).expect("wasDerivedFrom"),
            object: parent.into(),
            graph_name: GraphName::DefaultGraph,
        });
    }

    fn finish(self) -> Vec<Quad> {
        self.quads
    }
}

fn graph_matches(quad: &Quad, graph: &str) -> bool {
    match quad.graph_name.as_ref() {
        GraphNameRef::NamedNode(name) => name.as_str() == graph,
        GraphNameRef::DefaultGraph | GraphNameRef::BlankNode(_) => false,
    }
}

struct ProvReadScanLimits {
    max_store_quads_scanned: usize,
    max_companion_graph_quads: usize,
}

impl ProvReadScanLimits {
    fn production() -> Self {
        Self {
            max_store_quads_scanned: PROV_READ_MAX_STORE_QUADS_SCANNED,
            max_companion_graph_quads: PROV_READ_MAX_COMPANION_GRAPH_QUADS,
        }
    }
}

struct ProvReadScanBudget {
    limits: ProvReadScanLimits,
    store_quads_scanned: usize,
    graph_quads_matched: usize,
}

impl ProvReadScanBudget {
    fn new(limits: ProvReadScanLimits) -> Self {
        Self {
            limits,
            store_quads_scanned: 0,
            graph_quads_matched: 0,
        }
    }

    fn record_store_quad(&mut self) -> Result<(), String> {
        self.store_quads_scanned += 1;
        if self.store_quads_scanned > self.limits.max_store_quads_scanned {
            return Err(ERR_PROV_READ_STORE_SCAN_BUDGET.to_string());
        }
        Ok(())
    }

    fn record_companion_graph_quad(&mut self) -> Result<(), String> {
        self.graph_quads_matched += 1;
        if self.graph_quads_matched > self.limits.max_companion_graph_quads {
            return Err(ERR_PROV_READ_GRAPH_QUAD_BUDGET.to_string());
        }
        Ok(())
    }
}

/// Yields only quads in one named graph. Still walks the backing store today; a
/// follow-up can switch to graph-native iterators when Oxigraph exposes them.
/// Scan cost is bounded; exceeding either budget fails closed with an error.
fn for_each_quad_in_named_graph<F>(
    store: &Store,
    graph: &str,
    limits: ProvReadScanLimits,
    mut visit: F,
) -> Result<(), String>
where
    F: FnMut(&Quad) -> Result<(), String>,
{
    let graph_name = NamedNodeRef::new(graph).map_err(|e| format!("invalid graph IRI: {e}"))?;
    let mut budget = ProvReadScanBudget::new(limits);
    for quad in store.iter() {
        let quad = quad.map_err(|e| format!("read graph failed: {e}"))?;
        budget.record_store_quad()?;
        if quad.graph_name.as_ref() == GraphNameRef::NamedNode(graph_name) {
            budget.record_companion_graph_quad()?;
            visit(&quad)?;
        }
    }
    Ok(())
}

fn is_provenance_or_meta_triple(quad: &Quad) -> bool {
    let subject = quad.subject.to_string();
    let predicate = quad.predicate.as_str();
    subject.starts_with(EXTRACTION_STAGING_PROV_NS)
        || predicate.starts_with(EXTRACTION_STAGING_PROV_NS)
        || predicate.starts_with(RML_META_NS)
        || predicate.starts_with(WAO_PROV_VOCAB)
        || predicate.starts_with(WAO_PROV_RESOURCE)
        || predicate.starts_with(PROV_NS)
}

pub fn triple_terms_from_rdf_quads(quads: &[RdfQuad]) -> Result<Vec<Term>, String> {
    quads
        .iter()
        .map(|quad| {
            let subject = NamedNode::new(quad.subject.as_str())
                .map_err(|e| format!("invalid subject IRI: {e}"))?;
            let predicate = NamedNode::new(quad.predicate.as_str())
                .map_err(|e| format!("invalid predicate IRI: {e}"))?;
            let object = rdf_value_to_term(&quad.object)?;
            Ok(Term::Triple(Box::new(Triple::new(
                NamedOrBlankNode::NamedNode(subject),
                predicate,
                object,
            ))))
        })
        .collect()
}

fn rdf_value_to_term(value: &RdfValue) -> Result<Term, String> {
    Ok(match value {
        RdfValue::Iri(iri) => NamedNode::new(iri.as_str())
            .map_err(|e| format!("invalid object IRI: {e}"))?
            .into(),
        RdfValue::Literal(text) => {
            Term::Literal(oxigraph::model::Literal::new_simple_literal(text.clone()))
        }
        RdfValue::TypedLiteral(text, datatype) => {
            Term::Literal(oxigraph::model::Literal::new_typed_literal(
                text.clone(),
                NamedNode::new(datatype.as_str())
                    .map_err(|e| format!("invalid datatype IRI: {e}"))?,
            ))
        }
    })
}

fn quad_to_triple_term(quad: &Quad) -> Term {
    Term::Triple(Box::new(Triple::new(
        quad.subject.clone(),
        quad.predicate.clone(),
        quad.object.clone(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isolation::IsolationClaims;

    fn claims() -> IsolationClaims {
        IsolationClaims::from_verified("tenant-a", "project-a", "actor-1").unwrap()
    }

    #[test]
    fn prov_graph_is_minted_from_claims() {
        let claims = claims();
        assert_eq!(
            prov_graph_iri_for_claims(&claims).unwrap(),
            "graph://tenant-a/project-a/prov"
        );
    }

    #[test]
    fn rdf12_triple_term_roundtrip_in_prov_graph() {
        let store = KnowledgeGraphStore::new().unwrap();
        let claims = claims();
        store
            .write_quads_for_claims(
                &claims,
                &[super::super::types::RdfQuad {
                    subject: "http://example.org/fact".into(),
                    predicate: "http://example.org/name".into(),
                    object: super::super::types::RdfValue::Literal("alpha".into()),
                    graph: None,
                }],
            )
            .unwrap();
        let terms = store
            .collect_fact_triple_terms_in_graph(&claims.graph_iri().unwrap())
            .unwrap();
        assert_eq!(terms.len(), 1);
        store
            .record_rml_materialization_provenance(
                &claims,
                "https://agentos.ontology/rml-materialization/test-run",
                "abc",
                terms,
                ProvenanceOutcome::Succeeded,
            )
            .expect("record_rml_materialization_provenance");
        let activity = store
            .get_provenance_activity(&claims, "rml/test-run")
            .unwrap()
            .expect("activity");
        assert_eq!(
            activity["contains"].as_array().unwrap().len(),
            1,
            "triple term link must survive insert and read-back"
        );
    }

    #[test]
    fn production_writes_do_not_populate_prov_graph() {
        let store = KnowledgeGraphStore::new().unwrap();
        let claims = claims();
        store
            .write_quads_for_claims(
                &claims,
                &[super::super::types::RdfQuad {
                    subject: "http://example.org/x".into(),
                    predicate: "http://example.org/y".into(),
                    object: super::super::types::RdfValue::Literal("z".into()),
                    graph: None,
                }],
            )
            .unwrap();
        assert_eq!(
            store.count_prov_activities_for_claims(&claims).unwrap(),
            0,
            "minted prov graph must stay empty until kernel provenance writers run"
        );
        assert!(
            store
                .query_provenance_for_subject(&claims, "http://example.org/x")
                .unwrap()
                .is_empty(),
            "subject lookup must not fabricate links from production-only triples"
        );
        assert!(
            store
                .get_provenance_activity(&claims, "extraction/test")
                .unwrap()
                .is_none(),
            "write_quads_for_claims must not write the prov companion graph"
        );
    }

    #[test]
    fn rml_failed_outcome_records_symmetric_activity_metadata() {
        let store = KnowledgeGraphStore::new().unwrap();
        let claims = claims();
        let subject = "https://example.test/asset/A-100";
        store
            .write_quads_for_claims(
                &claims,
                &[super::super::types::RdfQuad {
                    subject: subject.into(),
                    predicate: "https://example.test/ontology/name".into(),
                    object: super::super::types::RdfValue::Literal("Inverter".into()),
                    graph: None,
                }],
            )
            .unwrap();
        let terms = triple_terms_from_rdf_quads(&[super::super::types::RdfQuad {
            subject: subject.into(),
            predicate: "https://example.test/ontology/name".into(),
            object: super::super::types::RdfValue::Literal("Inverter".into()),
            graph: None,
        }])
        .unwrap();
        store
            .record_rml_materialization_provenance(
                &claims,
                "https://agentos.ontology/rml-materialization/failed-run",
                "checksum-deadbeef",
                terms,
                ProvenanceOutcome::Failed,
            )
            .expect("record_rml_materialization_provenance");
        let activity = store
            .get_provenance_activity(&claims, "rml/failed-run")
            .unwrap()
            .expect("failed RML run must still mint an activity");
        let props = activity["properties"]
            .as_object()
            .expect("activity properties");
        let literal = |key: &str| {
            props
                .get(&format!("<{}>", wao(key)))
                .and_then(|v| v.as_str())
                .map(|value| value.trim_matches('"'))
        };
        assert_eq!(literal("activityOutcome"), Some("failed"));
        assert_eq!(literal("activityKind"), Some("rml_materialization"));
        assert!(
            activity["contains"].as_array().unwrap().is_empty(),
            "failed materialization must not link fact triples via contains"
        );
    }

    #[test]
    fn prov_read_path_fail_closed_on_oversized_companion_graph() {
        use std::time::{Duration, Instant};

        let store = KnowledgeGraphStore::new().unwrap();
        let claims = claims();
        let over = PROV_READ_MAX_COMPANION_GRAPH_QUADS + 1;
        let batch = 1024usize;
        let seed_started = Instant::now();
        for start in (0..over).step_by(batch) {
            let end = (start + batch).min(over);
            let quads: Vec<Quad> = (start..end)
                .map(|i| Quad {
                    subject: NamedNode::new(format!("urn:prov-bloat:{i}"))
                        .expect("subject")
                        .into(),
                    predicate: NamedNode::new("urn:prov-bloat:p").expect("predicate"),
                    object: Term::Literal(oxigraph::model::Literal::new_simple_literal("padding")),
                    graph_name: GraphName::DefaultGraph,
                })
                .collect();
            store
                .record_provenance_quads_for_claims(&claims, &quads)
                .expect("seed companion graph");
        }
        let seed_elapsed = seed_started.elapsed();

        let read_started = Instant::now();
        let err = store
            .count_prov_activities_for_claims(&claims)
            .expect_err("oversized companion graph must fail closed on read");
        let read_elapsed = read_started.elapsed();

        assert!(
            err.contains("companion graph quad budget"),
            "unexpected error: {err}"
        );
        assert!(
            read_elapsed < Duration::from_secs(60),
            "read path must fail closed promptly (seed {seed_elapsed:?}, read {read_elapsed:?})"
        );
    }

    #[test]
    fn prov_read_path_fail_closed_on_oversized_store_scan() {
        let store = KnowledgeGraphStore::new().unwrap();
        let claims = claims();
        let over = 33usize;
        let limits = ProvReadScanLimits {
            max_store_quads_scanned: 32,
            max_companion_graph_quads: PROV_READ_MAX_COMPANION_GRAPH_QUADS,
        };
        let batch = 16usize;
        for start in (0..over).step_by(batch) {
            let end = (start + batch).min(over);
            let quads: Vec<super::super::types::RdfQuad> = (start..end)
                .map(|i| super::super::types::RdfQuad {
                    subject: format!("urn:store-bloat:{i}"),
                    predicate: "urn:store-bloat:p".into(),
                    object: super::super::types::RdfValue::Literal("padding".into()),
                    graph: None,
                })
                .collect();
            store.write_quads_for_claims(&claims, &quads).unwrap();
        }
        let graph = prov_graph_iri_for_claims(&claims).unwrap();
        let err = for_each_quad_in_named_graph(store.store_arc(), &graph, limits, |_| Ok(()))
            .expect_err("oversized backing store must fail closed on read scan");
        assert!(err.contains("store scan budget"), "unexpected error: {err}");
    }

    #[test]
    fn prov_read_path_ignores_quads_outside_minted_companion_graph() {
        let store = KnowledgeGraphStore::new().unwrap();
        let claims = claims();
        let subject = "https://example.test/only-tenant-b";
        let foreign_activity = activity_iri("rml/foreign");
        let foreign_contains = wao("contains");
        let triple = Term::Triple(Box::new(Triple::new(
            NamedOrBlankNode::NamedNode(NamedNode::new(subject).expect("subject")),
            NamedNode::new("https://example.test/p").expect("predicate"),
            NamedNode::new("https://example.test/o").expect("object"),
        )));
        store
            .record_provenance_quads_for_claims(
                &IsolationClaims::from_verified("tenant-b", "project-b", "actor-b").unwrap(),
                &[Quad {
                    subject: NamedNode::new(foreign_activity.as_str())
                        .expect("activity")
                        .into(),
                    predicate: NamedNode::new(foreign_contains).expect("contains"),
                    object: triple,
                    graph_name: GraphName::DefaultGraph,
                }],
            )
            .unwrap();
        assert_eq!(store.count_prov_activities_for_claims(&claims).unwrap(), 0);
        assert!(
            store
                .query_provenance_for_subject(&claims, subject)
                .unwrap()
                .is_empty(),
            "tenant-a read path must not surface tenant-b prov quads even though they share one store"
        );
    }
}
