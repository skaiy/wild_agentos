//! 本体元模型 CRUD 与动力层 Action invoke。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；知识包/KB 见 `kb.rs`。

use std::{
    process::{Command, Stdio},
    sync::Arc,
};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::error;

use crate::{
    isolation::IsolationClaims,
    knowledge_graph::{
        canonicalizer::{canonicalize, CanonicalizationDecision},
        extractor::KnowledgeExtractor,
        ontology_draft::{
            self, DraftLinkInput, InductionDocument, TypeDraftBundle, TypeDraftProvenance,
        },
        ontology_health,
        ontology_layer::ActionGuardrailConfig,
        quality_gate::{
            JudgeConfig, JudgeReport, JudgeVerdict, KgQualityGate, QualityGateReport,
            QualityGateRequest,
        },
        rdf_mapper::RdfMapper,
        store::{
            ClaimsGraphUpdate, KnowledgeGraphStore, MaterializationAnchor, PendingActionApproval,
            PendingExtractionReview, PendingTypeDraft,
        },
        types::LLMExtractionOutput,
    },
};

use super::{iam::UserIdentity, ontology_guardrails, AppState};

const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";
const ENTITY_RESOLUTION_PROVENANCE_NS: &str = "https://agentos.ontology/entity-resolution/";
const ENTITY_RESOLUTION_THRESHOLD: f32 = 0.98;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntityResolutionSuggestionRequest {
    pub source_iri: String,
    pub mention: String,
}

#[derive(Debug, Deserialize)]
struct EntityResolutionSidecarResponse {
    target_iri: String,
    score: f32,
    evidence: Vec<String>,
}

fn sparql_literal(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn normalized_entity_text(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn valid_iri(value: &str) -> bool {
    oxigraph::model::NamedNodeRef::new(value).is_ok()
}

fn run_entity_resolution_sidecar(
    source_iri: &str,
    mention: &str,
    candidates: &[(String, String)],
) -> Result<EntityResolutionSidecarResponse, String> {
    let command = std::env::var("AGENTOS_KG_GLINKER_COMMAND").map_err(|_| {
        "entity resolution requires AGENTOS_KG_GLINKER_COMMAND configured as one executable path"
            .to_string()
    })?;
    if command.trim().is_empty() || command.contains(char::is_whitespace) {
        return Err("AGENTOS_KG_GLINKER_COMMAND must be one executable path".into());
    }
    let input = json!({
        "source_iri": source_iri, "mention": mention,
        "candidates": candidates.iter().map(|(iri, label)| json!({"iri": iri, "label": label})).collect::<Vec<_>>(),
        "min_score": ENTITY_RESOLUTION_THRESHOLD,
    });
    let mut child = Command::new(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start entity-resolution sidecar: {error}"))?;
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .ok_or("entity-resolution sidecar stdin unavailable")?
        .write_all(input.to_string().as_bytes())
        .map_err(|error| format!("write entity-resolution sidecar input: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for entity-resolution sidecar: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "entity-resolution sidecar failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("entity-resolution sidecar returned invalid JSON: {error}"))
}

/// Creates an approval-held ER suggestion for an entity in a claims-derived
/// staging extraction. Candidate retrieval remains confined to the caller's
/// production graph; no suggestion is merged by this primitive.
pub(crate) fn create_entity_resolution_suggestion_from_staging(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    extraction_id: &str,
    source_iri: &str,
    mention: &str,
) -> Result<Option<String>, String> {
    if !valid_iri(source_iri) || mention.trim().is_empty() {
        return Err("source_iri must be a valid IRI and mention is required".into());
    }
    let source_exists = kg.query_staging_for_claims(
        claims,
        extraction_id,
        &format!("SELECT ?p WHERE {{ <{source_iri}> ?p ?o }} LIMIT 1"),
    )?;
    if source_exists.is_empty() {
        return Err("source entity is not present in the staged extraction".into());
    }
    let candidates = kg.query_sparql_for_claims(
        claims,
        &format!("SELECT DISTINCT ?candidate ?label WHERE {{ ?candidate <{RDFS_LABEL}> ?label . FILTER(?candidate != <{source_iri}>) }} LIMIT 50"),
    )
    .map_err(|error| error.to_string())?;
    let candidates: Vec<(String, String)> = candidates
        .into_iter()
        .filter_map(|row| {
            Some((
                row.get("?candidate")?.as_str()?.to_owned(),
                row.get("?label")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    let result = run_entity_resolution_sidecar(source_iri, mention, &candidates)?;
    let Some(target_label) = candidates
        .iter()
        .find(|(iri, _)| iri == &result.target_iri)
        .map(|(_, label)| label.clone())
    else {
        return Ok(None);
    };
    if result.score < ENTITY_RESOLUTION_THRESHOLD || !valid_iri(&result.target_iri) {
        return Ok(None);
    }
    let approval_id = uuid::Uuid::new_v4().simple().to_string();
    let evidence_iri = format!("{ENTITY_RESOLUTION_PROVENANCE_NS}suggestion/{approval_id}");
    let triples = format!(
        "<{source_iri}> <{OWL_SAME_AS}> <{}> . <{evidence_iri}> <{ENTITY_RESOLUTION_PROVENANCE_NS}sourceEntity> <{source_iri}> ; <{ENTITY_RESOLUTION_PROVENANCE_NS}targetEntity> <{}> ; <{ENTITY_RESOLUTION_PROVENANCE_NS}mention> \"{}\" ; <{ENTITY_RESOLUTION_PROVENANCE_NS}targetLabel> \"{}\" ; <{ENTITY_RESOLUTION_PROVENANCE_NS}score> \"{}\" ; <{ENTITY_RESOLUTION_PROVENANCE_NS}matcher> \"glinker-sidecar-v1\" .",
        result.target_iri, result.target_iri, sparql_literal(mention), sparql_literal(&target_label), result.score,
    );
    kg.update_staging_for_claims(
        claims,
        &approval_id,
        &ClaimsGraphUpdate::insert_data(triples),
    )?;
    kg.create_action_approval_for_claims(
        claims,
        &PendingActionApproval {
            approval_id: approval_id.clone(),
            staging_id: approval_id.clone(),
            staging_graph: kg.staging_graph_iri_for_claims(claims, &approval_id)?,
            action_id: "entity-resolution".into(),
            anchor_query: Some(format!(
                "SELECT ?same WHERE {{ <{source_iri}> <{OWL_SAME_AS}> <{}> }} LIMIT 1",
                result.target_iri
            )),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: (chrono::Utc::now() + chrono::Duration::hours(ACTION_APPROVAL_TTL_HOURS))
                .to_rfc3339(),
        },
    )?;
    Ok(Some(approval_id))
}

/// POST /api/v1/ontology/entity-resolution/suggestions.
///
/// A supervised, GLinker-inspired mention → retrieve → disambiguate path.
/// Retrieval is claims-scoped and the frozen matcher only accepts exact
/// normalized labels. Its output is always an approval-held suggestion.
pub(crate) async fn create_entity_resolution_suggestion_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<EntityResolutionSuggestionRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    if !valid_iri(&request.source_iri) || request.mention.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "source_iri must be a valid IRI and mention is required"})),
        )
            .into_response();
    }
    let mention = normalized_entity_text(&request.mention);
    if mention.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "mention must contain at least one letter or number"})),
        )
            .into_response();
    }
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let source_exists = match kg.query_sparql_for_claims(
        claims,
        &format!(
            "SELECT ?p WHERE {{ <{}> ?p ?o }} LIMIT 1",
            request.source_iri
        ),
    ) {
        Ok(rows) => !rows.is_empty(),
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    if !source_exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "source entity not found"})),
        )
            .into_response();
    }
    let candidates = match kg.query_sparql_for_claims(
        claims,
        &format!("SELECT DISTINCT ?candidate ?label WHERE {{ ?candidate <{RDFS_LABEL}> ?label . FILTER(?candidate != <{}>) }} LIMIT 50", request.source_iri),
    ) {
        Ok(rows) => rows,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response(),
    };
    let candidates: Vec<(String, String)> = candidates
        .into_iter()
        .filter_map(|row| {
            let candidate = row
                .get("?candidate")?
                .as_str()?
                .trim_matches(['<', '>'])
                .to_owned();
            let label = row.get("?label")?.as_str()?.to_owned();
            Some((candidate, label))
        })
        .collect();
    let result =
        match run_entity_resolution_sidecar(&request.source_iri, &request.mention, &candidates) {
            Ok(result) => result,
            Err(error) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": error, "production_write": false})),
                )
                    .into_response()
            }
        };
    let target_label = candidates
        .iter()
        .find(|(iri, _)| iri == &result.target_iri)
        .map(|(_, label)| label.clone());
    if result.score < ENTITY_RESOLUTION_THRESHOLD
        || target_label.is_none()
        || !valid_iri(&result.target_iri)
    {
        return (StatusCode::OK, Json(json!({
            "status": "not_presented", "reason": "no candidate met the frozen conservative threshold",
            "threshold": ENTITY_RESOLUTION_THRESHOLD, "production_write": false,
        }))).into_response();
    }
    let target_iri = result.target_iri;
    let target_label = target_label.expect("checked above");
    let approval_id = uuid::Uuid::new_v4().simple().to_string();
    let evidence_iri = format!("{ENTITY_RESOLUTION_PROVENANCE_NS}suggestion/{approval_id}");
    let triples = format!(
        "<{source}> <{same_as}> <{target}> . <{evidence}> <{ns}sourceEntity> <{source}> ; <{ns}targetEntity> <{target}> ; <{ns}mention> \"{mention}\" ; <{ns}targetLabel> \"{target_label}\" ; <{ns}score> \"{score}\" ; <{ns}matcher> \"glinker-sidecar-v1\" .",
        source = request.source_iri, target = target_iri, same_as = OWL_SAME_AS,
        evidence = evidence_iri, ns = ENTITY_RESOLUTION_PROVENANCE_NS,
        mention = sparql_literal(&request.mention), target_label = sparql_literal(&target_label),
        score = result.score,
    );
    if let Err(error) = kg.update_staging_for_claims(
        claims,
        &approval_id,
        &ClaimsGraphUpdate::insert_data(triples),
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response();
    }
    let approval = PendingActionApproval {
        approval_id: approval_id.clone(),
        staging_id: approval_id.clone(),
        staging_graph: kg
            .staging_graph_iri_for_claims(claims, &approval_id)
            .expect("generated identifier is valid"),
        action_id: "entity-resolution".into(),
        anchor_query: Some(format!(
            "SELECT ?same WHERE {{ <{}> <{}> <{}> }} LIMIT 1",
            request.source_iri, OWL_SAME_AS, target_iri
        )),
        created_at: chrono::Utc::now().to_rfc3339(),
        expires_at: (chrono::Utc::now() + chrono::Duration::hours(ACTION_APPROVAL_TTL_HOURS))
            .to_rfc3339(),
    };
    if let Err(error) = kg.create_action_approval_for_claims(claims, &approval) {
        let _ = kg.drop_staging_for_claims(claims, &approval_id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response();
    }
    let _ = state.kg_store.flush();
    emit_action_audit(
        &state,
        claims,
        "entity-resolution",
        &approval_id,
        "pending",
        &[],
    )
    .await;
    (StatusCode::OK, Json(json!({
        "status": "pending_approval", "approval_id": approval_id,
        "source_iri": request.source_iri, "target_iri": target_iri,
        "score": result.score, "threshold": ENTITY_RESOLUTION_THRESHOLD,
        "evidence": { "mention": request.mention, "target_label": target_label, "sidecar_evidence": result.evidence },
        "production_write": false,
    }))).into_response()
}

/// GET /api/v1/ontology/types — 返回新能源车维修域本体定义（对象/链接/动作/函数）
///
/// 语义层（ObjectType/LinkType）+ 动力层（ActionType/FunctionDef）的完整元模型。
///
/// 数据源为 Oxigraph 元命名图（`graph:ontology/meta`）：首启由 `ensure_seeded` 幂等
/// 把硬编码 `ev_repair_ontology()` 写入图谱，之后读路径解析 `meta:json` 快照重建。
/// 存储不可用时回退硬编码定义，保证只读契约零回归。
pub(crate) async fn ontology_types_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    use crate::knowledge_graph::ontology_store::OntologyStore;
    let ont = (|| {
        let store = OntologyStore::with_shared_store(state.kg_store.clone()).ok()?;
        store.ensure_seeded("ev-repair").ok()?;
        store.load_definition("ev-repair").ok()
    })()
    .unwrap_or_else(crate::knowledge_graph::ontology_layer::ev_repair_ontology);
    Json(json!({
        "domain": ont.domain,
        "counts": {
            "object_types": ont.object_types.len(),
            "link_types": ont.link_types.len(),
            "action_types": ont.action_types.len(),
            "functions": ont.functions.len(),
        },
        "object_types": ont.object_types,
        "link_types": ont.link_types,
        "action_types": ont.action_types,
        "functions": ont.functions,
    }))
}

/// GET /api/v1/ontology/guardrails — 返回当前 claims 已认证调用方可读取的域默认护栏。
pub(crate) async fn domain_guardrails_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    match store.load_domain_guardrails(ONT_DOMAIN) {
        Ok(guardrails) => {
            Json(json!({ "domain": ONT_DOMAIN, "guardrails": guardrails })).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

/// PUT /api/v1/ontology/guardrails — 更新域默认护栏。
///
/// This endpoint is claims-authenticated. Invoke payloads cannot carry this configuration;
/// their graph scope and policy are selected server-side from the stored ActionType/domain.
pub(crate) async fn update_domain_guardrails_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(guardrails): Json<ActionGuardrailConfig>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    if let Err(error) = ontology_guardrails::validate_config(&guardrails) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    match store.upsert_domain_guardrails(ONT_DOMAIN, &guardrails) {
        Ok(()) => Json(json!({ "status": "ok", "domain": ONT_DOMAIN, "guardrails": guardrails }))
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

// ─── 阶段1：ObjectType + LinkType 在线 CRUD（存储驱动，写前备份 meta 图）───────
//
// 契约：
//   POST /api/v1/ontology/object-types          body=ObjectType   新建/更新（幂等）
//   PUT  /api/v1/ontology/object-types/:id       body=ObjectType   更新（id 以路径为准）
//   DELETE /api/v1/ontology/object-types/:id                       删除（被引用→409）
//   POST /api/v1/ontology/link-types            body=LinkType     新建/更新（source/target 校验）
//   PUT  /api/v1/ontology/link-types/:id         body=LinkType     更新
//   DELETE /api/v1/ontology/link-types/:id                        删除
// 本体域固定为 ev-repair（当前单域）；首启由 ensure_seeded 幂等 seed。

const ONT_DOMAIN: &str = "ev-repair";
const TYPE_DRAFT_TTL_HOURS: i64 = 24;
const EXTRACTION_PROVENANCE_NS: &str = "https://agentos.ontology/extraction/";

/// A scenario's required ontology surface, supplied before the scenario is
/// attached to agents. IDs are matched exactly to promoted ontology IDs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OntologyReadinessRequest {
    pub required_object_types: Vec<String>,
    pub required_link_types: Vec<String>,
}

fn normalized_required_ids(ids: Vec<String>, kind: &str) -> Result<Vec<String>, String> {
    let mut unique = std::collections::BTreeSet::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(format!("{kind} IDs must not be empty"));
        }
        unique.insert(id.to_string());
    }
    Ok(unique.into_iter().collect())
}

fn readiness_items(
    required: &[String],
    promoted: &std::collections::HashSet<&str>,
    drafted: &std::collections::HashSet<&str>,
) -> Vec<Value> {
    required
        .iter()
        .map(|id| {
            let status = if promoted.contains(id.as_str()) {
                "promoted"
            } else if drafted.contains(id.as_str()) {
                "draft"
            } else {
                "missing"
            };
            json!({ "id": id, "status": status })
        })
        .collect()
}

/// POST /api/v1/ontology/readiness-report — read-only pre-attach domain
/// coverage report. It reads the promoted meta-model and caller-scoped open
/// drafts, but deliberately does not seed, promote, expire, or materialize.
pub(crate) async fn ontology_readiness_report_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<OntologyReadinessRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    let required_objects =
        match normalized_required_ids(request.required_object_types, "ObjectType") {
            Ok(ids) => ids,
            Err(error) => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
            }
        };
    let required_links = match normalized_required_ids(request.required_link_types, "LinkType") {
        Ok(ids) => ids,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
        }
    };

    // Do not call `ontology_store_ready`: its first-use seed is a write, which
    // is forbidden for this audit endpoint.
    use crate::knowledge_graph::ontology_store::OntologyStore;
    let ontology_store = match OntologyStore::with_shared_store(state.kg_store.clone()) {
        Ok(store) => store,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error })),
            )
                .into_response()
        }
    };
    let ontology = match ontology_store.load_definition(ONT_DOMAIN) {
        Ok(definition) => definition,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error })),
            )
                .into_response()
        }
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error })),
            )
                .into_response()
        }
    };
    // Unlike `active_type_drafts`, this is intentionally a pure read: expired
    // drafts are excluded in memory and never cleaned up by a report request.
    let open_drafts = match kg.list_type_drafts_for_claims(claims) {
        Ok(drafts) => drafts
            .into_iter()
            .filter(|draft| !type_draft_expired(draft))
            .collect::<Vec<_>>(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error })),
            )
                .into_response()
        }
    };
    let promoted_objects = ontology
        .object_types
        .iter()
        .map(|item| item.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let promoted_links = ontology
        .link_types
        .iter()
        .map(|item| item.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let drafted_objects = open_drafts
        .iter()
        .flat_map(|draft| {
            draft
                .bundle
                .object_types
                .iter()
                .map(|item| item.id.as_str())
        })
        .collect::<std::collections::HashSet<_>>();
    let drafted_links = open_drafts
        .iter()
        .flat_map(|draft| draft.bundle.link_types.iter().map(|item| item.id.as_str()))
        .collect::<std::collections::HashSet<_>>();
    let object_types = readiness_items(&required_objects, &promoted_objects, &drafted_objects);
    let link_types = readiness_items(&required_links, &promoted_links, &drafted_links);
    let required_total = object_types.len() + link_types.len();
    let promoted_total = object_types
        .iter()
        .chain(link_types.iter())
        .filter(|item| item["status"] == "promoted")
        .count();
    let draft_total = object_types
        .iter()
        .chain(link_types.iter())
        .filter(|item| item["status"] == "draft")
        .count();
    let missing = object_types
        .iter()
        .chain(link_types.iter())
        .filter(|item| item["status"] == "missing")
        .cloned()
        .collect::<Vec<_>>();
    let recommendations = missing
        .iter()
        .map(|item| {
            let type_id = item["id"].as_str().unwrap_or_default();
            json!({
                "type_id": item["id"],
                "kind": if required_objects.iter().any(|id| id == type_id) { "object_type" } else { "link_type" },
                "recommended_next_step": "provide a schema, glossary, DDL, or OpenAPI asset to create a reviewable type draft; explicit human promotion remains required",
            })
        })
        .collect::<Vec<_>>();

    (StatusCode::OK, Json(json!({
        "domain": ONT_DOMAIN,
        "read_only": true,
        "scenario_attach_ready": missing.is_empty() && draft_total == 0,
        "coverage": {
            "required": required_total,
            "promoted": promoted_total,
            "open_draft": draft_total,
            "missing": missing.len(),
            "promoted_percent": if required_total == 0 { 100.0 } else { (promoted_total as f64 / required_total as f64) * 100.0 },
        },
        "object_types": object_types,
        "link_types": link_types,
        "open_drafts": open_drafts,
        "gaps": missing,
        "recommended_draft_assets": recommendations,
    }))).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConstrainedExtractionSource {
    pub blob_id: String,
    pub version: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConstrainedExtractionRequest {
    /// The versioned source text used by an upstream extractor. This first
    /// slice accepts extraction candidates explicitly so LLM/sidecar selection
    /// stays outside the trusted graph-write boundary.
    pub text: String,
    pub source: ConstrainedExtractionSource,
    pub extractor: String,
    #[serde(default)]
    pub model: Option<String>,
    pub candidates: LLMExtractionOutput,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StagingQuery {
    pub sparql: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializeExtractionRequest {
    pub confirm: bool,
}

/// Optional thresholds for the claims-scoped, read-only slow health loop.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OntologyHealthQuery {
    /// Object types at or below this production instance count are sparse.
    #[serde(default = "default_sparse_type_threshold")]
    pub sparse_type_threshold: u64,
    /// A non-expired type draft at or above this age is considered stale.
    #[serde(default = "default_stale_draft_hours")]
    pub stale_draft_hours: i64,
}

fn default_sparse_type_threshold() -> u64 {
    1
}

fn default_stale_draft_hours() -> i64 {
    24
}

/// GET /api/v1/ontology/health
///
/// Slow-loop evidence for an authenticated claims scope: canonicalization
/// rejection rate, quality-gate failures, sparse promoted ObjectTypes, and
/// stale/expired type drafts. It is strictly read-only: it creates no draft,
/// never resolves a review, and cannot promote or materialize anything.
pub(crate) async fn ontology_health_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Query(query): Query<OntologyHealthQuery>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    if query.stale_draft_hours < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "stale_draft_hours must be non-negative"})),
        )
            .into_response();
    }
    let ontology_store = match ontology_store_ready(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    let ontology = match ontology_store.load_definition(ONT_DOMAIN) {
        Ok(ontology) => ontology,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    match ontology_health::report(
        &kg,
        claims,
        &ontology,
        query.sparse_type_threshold,
        query.stale_draft_hours,
    ) {
        Ok(report) => Json(report).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

fn extraction_provenance_triples(
    extraction_id: &str,
    request: &ConstrainedExtractionRequest,
    decisions: &[CanonicalizationDecision],
) -> Result<String, String> {
    let run = format!("{EXTRACTION_PROVENANCE_NS}run/{extraction_id}");
    let literal = |value: &str| format!("\"{}\"", sparql_literal(value));
    let mut triples = vec![format!(
        "<{run}> <{ns}blobId> {blob} ; <{ns}blobVersion> {version} ; \
         <{ns}extractor> {extractor} ; <{ns}sourceText> {text} .",
        ns = EXTRACTION_PROVENANCE_NS,
        blob = literal(&request.source.blob_id),
        version = literal(&request.source.version),
        extractor = literal(&request.extractor),
        text = literal(&request.text),
    )];
    if let Some(model) = &request.model {
        triples.push(format!(
            "<{run}> <{ns}model> {model} .",
            ns = EXTRACTION_PROVENANCE_NS,
            model = literal(model),
        ));
    }
    for (index, decision) in decisions.iter().enumerate() {
        let decision_iri = format!("{EXTRACTION_PROVENANCE_NS}decision/{extraction_id}/{index}");
        let serialized = serde_json::to_string(decision)
            .map_err(|error| format!("serialize canonicalization decision: {error}"))?;
        triples.push(format!(
            "<{run}> <{ns}decision> <{decision_iri}> . \
             <{decision_iri}> <{ns}json> {serialized} .",
            ns = EXTRACTION_PROVENANCE_NS,
            serialized = literal(&serialized),
        ));
    }
    Ok(triples.join("\n"))
}

/// POST /api/v1/ontology/constrained-extractions
///
/// Canonicalizes open extraction candidates solely against the current,
/// explicitly promoted ObjectType and LinkType definitions, then writes only
/// accepted candidates and complete provenance to a claims-minted staging
/// graph. This endpoint cannot select or write the production graph.
pub(crate) async fn constrained_extraction_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<ConstrainedExtractionRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    if request.text.trim().is_empty()
        || request.source.blob_id.trim().is_empty()
        || request.source.version.trim().is_empty()
        || request.extractor.trim().is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "text, source.blob_id, source.version, and extractor are required"})),
        )
            .into_response();
    }
    if let Err(error) = KnowledgeExtractor::validate_extraction(
        &serde_json::to_string(&request.candidates).unwrap_or_default(),
    ) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
    }
    let ontology_store = match ontology_store_ready(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    let ontology = match ontology_store.load_definition(ONT_DOMAIN) {
        Ok(ontology) => ontology,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let canonical = canonicalize(&request.candidates, &ontology);
    let extraction_id = uuid::Uuid::new_v4().simple().to_string();
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let staging_graph = match kg.staging_graph_iri_for_claims(claims, &extraction_id) {
        Ok(graph) => graph,
        Err(error) => {
            return (StatusCode::FORBIDDEN, Json(json!({"error": error}))).into_response()
        }
    };
    let mapped = RdfMapper::map_extraction(&canonical.extraction, &staging_graph);
    let mut triples = RdfMapper::quads_to_sparql_triples(&mapped.quads);
    let provenance =
        match extraction_provenance_triples(&extraction_id, &request, &canonical.decisions) {
            Ok(triples) => triples,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                )
                    .into_response()
            }
        };
    if !triples.is_empty() {
        triples.push('\n');
    }
    triples.push_str(&provenance);
    if let Err(error) = kg.update_staging_for_claims(
        claims,
        &extraction_id,
        &ClaimsGraphUpdate::insert_data(triples),
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response();
    }
    let _ = state.kg_store.flush();
    Json(json!({
        "status": "staged",
        "extraction_id": extraction_id,
        "staging_graph": staging_graph,
        "entities_staged": mapped.entity_count,
        "relations_staged": mapped.relation_count,
        "decisions": canonical.decisions,
        "production_write": false,
    }))
    .into_response()
}

/// GET /api/v1/ontology/constrained-extractions/:id? sparql=...
///
/// Reads only the caller's claims-derived staging graph; `GRAPH` clauses are
/// rejected by the store so callers cannot select another graph.
pub(crate) async fn constrained_extraction_query_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(extraction_id): Path<String>,
    Query(query): Query<StagingQuery>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    match kg.query_staging_for_claims(claims, &extraction_id, &query.sparql) {
        Ok(rows) => Json(json!({"rows": rows})).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response(),
    }
}

/// POST /api/v1/ontology/constrained-extractions/:id/quality-gate
///
/// Runs the medium-speed supervisory loop only against the caller's
/// claims-minted staging graph. It persists an immutable report on that same
/// extraction for review; it never materializes or promotes anything.
pub(crate) async fn quality_gate_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(extraction_id): Path<String>,
    Json(request): Json<QualityGateRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let mut report = match KgQualityGate::evaluate(&kg, claims, &extraction_id, &request, None) {
        Ok(report) => report,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    if report.deterministic_passed {
        if let Some(judge_config) = &request.judge {
            let judge =
                run_llm_quality_judge(&state, &kg, claims, &extraction_id, judge_config).await;
            report = KgQualityGate::apply_judge(report, judge);
        }
    }
    let review = PendingExtractionReview {
        review_id: uuid::Uuid::new_v4().simple().to_string(),
        extraction_id: extraction_id.clone(),
        staging_graph: match kg.staging_graph_iri_for_claims(claims, &extraction_id) {
            Ok(graph) => graph,
            Err(error) => {
                return (StatusCode::FORBIDDEN, Json(json!({"error": error}))).into_response()
            }
        },
        gate_status: report.review_status.clone(),
        report_json: serde_json::to_string(&report).unwrap_or_default(),
        created_at: chrono::Utc::now().to_rfc3339(),
        decision: "pending".into(),
    };
    if let Err(error) = persist_quality_gate_report(&kg, claims, &report) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response();
    }
    if let Err(error) = kg.create_extraction_review_for_claims(claims, &review) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response();
    }
    let _ = state.kg_store.flush();
    let status = if report.passed {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    (status, Json(json!({ "report": report, "review": review }))).into_response()
}

async fn run_llm_quality_judge(
    state: &AppState,
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    extraction_id: &str,
    config: &JudgeConfig,
) -> JudgeReport {
    use crate::gateway::unified_gateway::{ChatContent, ChatMessage};

    let evidence = match kg.staging_ntriples_for_claims(claims, extraction_id) {
        Ok(value) => value,
        Err(error) => return failed_judge_report(format!("read staging evidence: {error}")),
    };
    let message = ChatMessage {
        role: "user".into(),
        content: ChatContent::Text(format!(
            "You are a source-grounded KG quality reviewer. Review only this staging evidence. \
Return strict JSON with verdict (approve|needs_review|reject), rationale, and non-empty \
source_citations. Do not claim authority to override deterministic policy.\n\n{evidence}"
        )),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    };
    let default_model = state.gateway.default_model();
    let model = config.model.as_deref().unwrap_or(&default_model);
    let content = state
        .gateway
        .chat_with_model(model, vec![message])
        .await
        .ok()
        .and_then(|response| response.choices.into_iter().next())
        .and_then(|choice| choice.message.content);
    match content.and_then(|text| serde_json::from_str::<JudgeReport>(&text).ok()) {
        Some(report) if !report.source_citations.is_empty() => report,
        Some(_) => failed_judge_report("Judge returned no source citations".into()),
        None => failed_judge_report("Judge did not return valid source-grounded JSON".into()),
    }
}

fn failed_judge_report(rationale: String) -> JudgeReport {
    JudgeReport {
        verdict: JudgeVerdict::Reject,
        rationale,
        source_citations: Vec::new(),
    }
}

/// GET /api/v1/ontology/extraction-reviews — claims-scoped review queue.
pub(crate) async fn list_extraction_reviews_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    match kg.list_extraction_reviews_for_claims(claims) {
        Ok(reviews) => Json(json!({"reviews": reviews, "production_write": false})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

/// POST /api/v1/ontology/extraction-reviews/:id/{approve,reject}
///
/// Records external human judgment only. P1.5 owns any future materialization.
pub(crate) async fn resolve_extraction_review_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path((review_id, decision)): Path<(String, String)>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let decision = match decision.as_str() {
        "approve" => "approved",
        "reject" => "rejected",
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "unknown review action"})),
            )
                .into_response()
        }
    };
    match kg.resolve_extraction_review_for_claims(claims, &review_id, decision) {
        Ok(()) => Json(json!({
            "review_id": review_id, "decision": decision, "production_write": false
        }))
        .into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response(),
    }
}

/// GET /api/v1/ontology/constrained-extractions/:id/review
///
/// Returns reports attached to one extraction. Approval/rejection remains a
/// human/HITL operation; this endpoint deliberately cannot commit staging.
pub(crate) async fn constrained_extraction_review_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(extraction_id): Path<String>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let reports = match quality_gate_reports_for_extraction(&kg, claims, &extraction_id) {
        Ok(reports) => reports,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    Json(json!({
        "extraction_id": extraction_id,
        "quality_gate_reports": reports,
        "production_write": false,
    }))
    .into_response()
}

fn quality_gate_reports_for_extraction(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    extraction_id: &str,
) -> Result<Vec<QualityGateReport>, String> {
    let query = format!(
        "SELECT ?report WHERE {{ <{EXTRACTION_PROVENANCE_NS}run/{extraction_id}> \
         <{EXTRACTION_PROVENANCE_NS}qualityReport> ?report }}"
    );
    kg.query_staging_for_claims(claims, extraction_id, &query)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    row.get("?report")
                        .and_then(|value| value.as_str())
                        .and_then(decode_sparql_literal)
                        .and_then(|json| serde_json::from_str::<QualityGateReport>(&json).ok())
                })
                .collect()
        })
}

/// Oxigraph's display form keeps RDF literal escapes after the outer quotes
/// are removed by the generic query serializer. Decode that literal before
/// parsing the report JSON so persisted gate evidence can govern a later call.
fn decode_sparql_literal(value: &str) -> Option<String> {
    serde_json::from_str::<String>(&format!("\"{value}\"")).ok()
}

/// POST /api/v1/ontology/constrained-extractions/:id/materialize
///
/// This is the only staging-to-production path for constrained extractions.
/// It accepts no client-selected graph and treats the post-write claims-scoped
/// SPARQL re-read as the success anchor—not an upstream worker or LLM report.
pub(crate) async fn materialize_constrained_extraction_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(extraction_id): Path<String>,
    Json(request): Json<MaterializeExtractionRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return unauthorized_isolation_claims().into_response();
    };
    if !request.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "materialization requires confirm: true"})),
        )
            .into_response();
    }
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let reports = match quality_gate_reports_for_extraction(&kg, claims, &extraction_id) {
        Ok(reports) => reports,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    let reviews = match kg.list_extraction_reviews_for_claims(claims) {
        Ok(reviews) => reviews,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let approved_review = reviews
        .iter()
        .find(|review| review.extraction_id == extraction_id && review.decision == "approved");
    let gate_passed = reports.iter().any(|report| report.passed);
    let authority = if gate_passed {
        Some("quality_gate_passed")
    } else if approved_review.is_some() {
        Some("recorded_human_override")
    } else {
        None
    };
    let Some(authority) = authority else {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "materialization requires a passed quality gate or a recorded approved human override",
                "production_write": false,
            })),
        )
            .into_response();
    };

    let anchor = match kg.materialize_staging_with_anchor_for_claims(claims, &extraction_id) {
        Ok(anchor) => anchor,
        Err(error) => {
            emit_materialization_audit(
                &state,
                claims,
                &extraction_id,
                authority,
                approved_review.map(|review| review.review_id.as_str()),
                None,
                "materialize_failed",
                Some(&error),
            )
            .await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"status": "materialize_failed", "error": error, "production_write": true})),
            )
                .into_response();
        }
    };
    let status = if anchor.passed {
        "materialized"
    } else {
        "materialize_failed"
    };
    emit_materialization_audit(
        &state,
        claims,
        &extraction_id,
        authority,
        approved_review.map(|review| review.review_id.as_str()),
        Some(&anchor),
        status,
        (!anchor.passed).then_some("post-write SPARQL anchor failed"),
    )
    .await;
    let _ = state.kg_store.flush();
    let http_status = if anchor.passed {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        http_status,
        Json(json!({
            "status": status,
            "extraction_id": extraction_id,
            "authority": authority,
            "review_id": approved_review.map(|review| review.review_id.as_str()),
            "anchor": anchor,
            "production_write": true,
        })),
    )
        .into_response()
}

pub(crate) fn persist_quality_gate_report(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    report: &QualityGateReport,
) -> Result<(), String> {
    let serialized = serde_json::to_string(report)
        .map_err(|error| format!("serialize quality gate report: {error}"))?;
    let run = format!("{EXTRACTION_PROVENANCE_NS}run/{}", report.extraction_id);
    let triple = format!(
        "<{run}> <{EXTRACTION_PROVENANCE_NS}qualityReport> \"{}\" .",
        sparql_literal(&serialized)
    );
    kg.update_staging_for_claims(
        claims,
        &report.extraction_id,
        &ClaimsGraphUpdate::insert_data(triple),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CsvTypeDraftRequest {
    pub csv: String,
    #[serde(default)]
    pub object_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub primary_key: Option<String>,
    #[serde(default)]
    pub links: Vec<DraftLinkInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JsonSchemaTypeDraftRequest {
    pub schema: Value,
    #[serde(default)]
    pub object_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub links: Vec<DraftLinkInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenApiTypeDraftRequest {
    pub document: Value,
    #[serde(default)]
    pub links: Vec<DraftLinkInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SqlDdlTypeDraftRequest {
    pub ddl: String,
    #[serde(default)]
    pub links: Vec<DraftLinkInput>,
}

/// Pre-kernel schema induction input. Candidate terms may come from a human,
/// an LLM, or a rule engine; this endpoint only turns them into isolated drafts.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaInductionRequest {
    #[serde(default)]
    pub candidate_terms: Vec<String>,
    #[serde(default)]
    pub documents: Vec<InductionDocument>,
    #[serde(default)]
    pub model_version: Option<String>,
    pub rule_version: String,
    /// Required when document text is not included. Document bodies are never
    /// persisted; only these stable source identifiers are retained.
    #[serde(default)]
    pub source_document_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromoteTypeDraftRequest {
    pub confirm: bool,
    #[serde(default)]
    pub force_breaking: bool,
    #[serde(default)]
    pub audit: Option<BreakingPromotionAuditInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BreakingPromotionAuditInput {
    /// Human explanation for accepting an incompatible production schema change.
    pub reason: String,
    /// Stable external review/change identifier for later investigation.
    pub ticket: String,
}

#[derive(Debug, Serialize)]
struct TypePromotionAudit<'a> {
    audit_id: String,
    timestamp: String,
    draft_id: &'a str,
    actor_id: &'a str,
    tenant_id: &'a str,
    project_id: &'a str,
    force_breaking: bool,
    reason: Option<&'a str>,
    ticket: Option<&'a str>,
    compatibility_changes: &'a [ontology_draft::CompatibilityChange],
}

/// Creates a claims-scoped draft only. It never writes to the production
/// ontology meta graph and intentionally has no ActionType input/output.
async fn create_type_draft(
    state: &Arc<AppState>,
    claims: &IsolationClaims,
    source: &str,
    bundle: TypeDraftBundle,
) -> axum::response::Response {
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let now = chrono::Utc::now();
    let draft = PendingTypeDraft {
        draft_id: uuid::Uuid::new_v4().simple().to_string(),
        source: source.to_string(),
        bundle,
        created_at: now.to_rfc3339(),
        expires_at: (now + chrono::Duration::hours(TYPE_DRAFT_TTL_HOURS)).to_rfc3339(),
    };
    match kg.create_type_draft_for_claims(claims, &draft) {
        Ok(()) => {
            let _ = state.kg_store.flush();
            (
                StatusCode::CREATED,
                Json(json!({
                    "status": "draft",
                    "draft_id": draft.draft_id,
                    "source": draft.source,
                    "expires_at": draft.expires_at,
                    "preview": draft.bundle,
                    "actions_generated": false,
                })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

/// POST /api/v1/ontology/type-drafts/from-csv — infer a reviewable object
/// type from CSV headers. Every property is conservatively a string.
pub(crate) async fn create_csv_type_draft_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<CsvTypeDraftRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let bundle = match ontology_draft::from_csv_headers(
        &request.csv,
        request.object_id.as_deref(),
        request.label.as_deref(),
        request.primary_key.as_deref(),
        request.links,
    ) {
        Ok(bundle) => bundle,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    create_type_draft(&state, claims, "csv", bundle).await
}

/// POST /api/v1/ontology/type-drafts/from-json-schema — infer an ObjectType
/// from a JSON Schema. Links must be explicit request input; none are inferred.
pub(crate) async fn create_json_schema_type_draft_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<JsonSchemaTypeDraftRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let bundle = match ontology_draft::from_json_schema(
        &request.schema,
        request.object_id.as_deref(),
        request.label.as_deref(),
        request.links,
    ) {
        Ok(bundle) => bundle,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    create_type_draft(&state, claims, "json_schema", bundle).await
}

/// POST /api/v1/ontology/type-drafts/from-openapi — create reviewable types
/// from OpenAPI 3 component schemas. Links must be explicit annotations/input.
pub(crate) async fn create_openapi_type_draft_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<OpenApiTypeDraftRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let bundle = match ontology_draft::from_openapi(&request.document, request.links) {
        Ok(bundle) => bundle,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    create_type_draft(&state, claims, "openapi", bundle).await
}

/// POST /api/v1/ontology/type-drafts/from-sql-ddl — create reviewable types
/// from a supported CREATE TABLE DDL subset. Links require explicit FK evidence.
pub(crate) async fn create_sql_ddl_type_draft_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<SqlDdlTypeDraftRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let bundle = match ontology_draft::from_sql_ddl(&request.ddl, request.links) {
        Ok(bundle) => bundle,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    create_type_draft(&state, claims, "sql_ddl", bundle).await
}

/// POST /api/v1/ontology/type-drafts/from-induction — create pre-kernel,
/// reviewable schema candidates from terminology and/or corpus documents.
/// It is draft-only: neither promotion nor ActionType creation is possible.
pub(crate) async fn create_schema_induction_type_draft_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<SchemaInductionRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let mut source_document_ids = request.source_document_ids;
    source_document_ids.extend(request.documents.iter().map(|document| document.id.clone()));
    source_document_ids.sort();
    source_document_ids.dedup();
    if source_document_ids.iter().any(|id| id.trim().is_empty()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "source document ids must not be empty"})),
        )
            .into_response();
    }
    let provenance = TypeDraftProvenance {
        model_version: request
            .model_version
            .filter(|version| !version.trim().is_empty()),
        rule_version: request.rule_version,
        source_document_ids,
    };
    let bundle = match ontology_draft::induce_from_terms(
        request.candidate_terms,
        &request.documents,
        provenance,
    ) {
        Ok(bundle) => bundle,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    create_type_draft(&state, claims, "schema_induction", bundle).await
}

fn type_draft_expired(draft: &PendingTypeDraft) -> bool {
    chrono::DateTime::parse_from_rfc3339(&draft.expires_at)
        .map(|expires| expires <= chrono::Utc::now())
        .unwrap_or(true)
}

fn active_type_drafts(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
) -> Result<Vec<PendingTypeDraft>, String> {
    let drafts = kg.list_type_drafts_for_claims(claims)?;
    Ok(drafts
        .into_iter()
        .filter(|draft| {
            if type_draft_expired(draft) {
                let _ = kg.delete_type_draft_for_claims(claims, &draft.draft_id);
                false
            } else {
                true
            }
        })
        .collect())
}

/// GET /api/v1/ontology/type-drafts — caller-only active draft list.
pub(crate) async fn list_type_drafts_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    match active_type_drafts(&kg, claims) {
        Ok(drafts) => {
            let _ = state.kg_store.flush();
            (StatusCode::OK, Json(json!({"drafts": drafts}))).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

/// POST /api/v1/ontology/type-drafts/:draft_id/promote — the only draft path
/// that writes production metadata. Existing IDs are compatibility-gated before
/// being replaced, so production schemas cannot be silently broken.
pub(crate) async fn promote_type_draft_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(draft_id): axum::extract::Path<String>,
    Json(request): Json<PromoteTypeDraftRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    if !request.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "promotion requires explicit confirm: true"})),
        )
            .into_response();
    }
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let draft = match active_type_drafts(&kg, claims) {
        Ok(drafts) => drafts.into_iter().find(|draft| draft.draft_id == draft_id),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let Some(draft) = draft else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "type draft not found", "draft_id": draft_id})),
        )
            .into_response();
    };
    let store = match ontology_store_ready(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    let current = match store.load_definition(ONT_DOMAIN) {
        Ok(current) => current,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    };
    let existing_objects: std::collections::HashSet<_> = current
        .object_types
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    let mut draft_ids = std::collections::HashSet::new();
    let duplicate_id = draft
        .bundle
        .object_types
        .iter()
        .map(|item| item.id.as_str())
        .chain(draft.bundle.link_types.iter().map(|item| item.id.as_str()))
        .find(|id| !draft_ids.insert(*id));
    if let Some(id) = duplicate_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "draft contains duplicate type IDs", "id": id})),
        )
            .into_response();
    }
    let compatibility_changes = ontology_draft::compatibility_changes(&current, &draft.bundle);
    let breaking_changes = compatibility_changes
        .iter()
        .filter(|change| change.breaking)
        .cloned()
        .collect::<Vec<_>>();
    if !breaking_changes.is_empty() && !request.force_breaking {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "promotion contains breaking ontology changes; retry only with force_breaking: true and audit.reason/audit.ticket",
                "compatibility_changes": compatibility_changes,
            })),
        )
            .into_response();
    }
    let audit_input = request.audit.as_ref();
    if !breaking_changes.is_empty()
        && audit_input
            .is_none_or(|audit| audit.reason.trim().is_empty() || audit.ticket.trim().is_empty())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "breaking promotion requires non-empty audit.reason and audit.ticket",
                "compatibility_changes": compatibility_changes,
            })),
        )
            .into_response();
    }
    let draft_objects: std::collections::HashSet<_> = draft
        .bundle
        .object_types
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    if let Some(link) = draft.bundle.link_types.iter().find(|link| {
        !(existing_objects.contains(link.source.as_str())
            || draft_objects.contains(link.source.as_str()))
            || !(existing_objects.contains(link.target.as_str())
                || draft_objects.contains(link.target.as_str()))
    }) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "draft link references an unknown object type", "id": link.id})),
        )
            .into_response();
    }
    for object in &draft.bundle.object_types {
        if let Err(error) = store.upsert_object_type(ONT_DOMAIN, object) {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
        }
    }
    for link in &draft.bundle.link_types {
        if let Err(error) = store.upsert_link_type(ONT_DOMAIN, link) {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
        }
    }
    let audit = TypePromotionAudit {
        audit_id: uuid::Uuid::new_v4().simple().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        draft_id: &draft.draft_id,
        actor_id: claims.actor_id(),
        tenant_id: claims.tenant_id(),
        project_id: claims.project_id(),
        force_breaking: request.force_breaking,
        reason: audit_input.map(|input| input.reason.trim()),
        ticket: audit_input.map(|input| input.ticket.trim()),
        compatibility_changes: &compatibility_changes,
    };
    let audit_value = match serde_json::to_value(&audit) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("could not serialize promotion audit: {error}")})),
            )
                .into_response()
        }
    };
    if let Err(error) = store.record_type_promotion_audit(&audit.audit_id, &audit_value) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response();
    }
    if let Err(error) = kg.delete_type_draft_for_claims(claims, &draft.draft_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response();
    }
    let _ = state.kg_store.flush();
    (StatusCode::OK, Json(json!({"status": "promoted", "draft_id": draft.draft_id,
        "object_type_ids": draft.bundle.object_types.iter().map(|item| &item.id).collect::<Vec<_>>(),
        "link_type_ids": draft.bundle.link_types.iter().map(|item| &item.id).collect::<Vec<_>>(),
        "compatibility_changes": compatibility_changes,
        "audit": audit,
    }))).into_response()
}

/// 构造 OntologyStore 并确保已 seed（失败转 500 JSON）。
fn ontology_store_ready(
    state: &Arc<AppState>,
) -> Result<crate::knowledge_graph::ontology_store::OntologyStore, (StatusCode, Json<Value>)> {
    use crate::knowledge_graph::ontology_store::OntologyStore;
    let store = OntologyStore::with_shared_store(state.kg_store.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
    })?;
    store.ensure_seeded(ONT_DOMAIN).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
    })?;
    Ok(store)
}

/// POST /api/v1/ontology/object-types — 新建或更新对象类型（幂等 upsert）。
pub(crate) async fn upsert_object_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(obj): Json<crate::knowledge_graph::ontology_layer::ObjectType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_object_type(ONT_DOMAIN, &obj) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": obj.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/object-types/:id — 更新对象类型（id 以路径为准）。
pub(crate) async fn update_object_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut obj): Json<crate::knowledge_graph::ontology_layer::ObjectType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    obj.id = id;
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_object_type(ONT_DOMAIN, &obj) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": obj.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/object-types/:id — 删除对象类型；被引用返回 409。
pub(crate) async fn delete_object_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_object_type(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(refs) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "对象类型被引用，无法删除", "references": refs })),
        )
            .into_response(),
    }
}

/// POST /api/v1/ontology/link-types — 新建或更新链接类型（校验 source/target 存在）。
pub(crate) async fn upsert_link_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(link): Json<crate::knowledge_graph::ontology_layer::LinkType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_link_type(ONT_DOMAIN, &link) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": link.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/link-types/:id — 更新链接类型（id 以路径为准）。
pub(crate) async fn update_link_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut link): Json<crate::knowledge_graph::ontology_layer::LinkType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    link.id = id;
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_link_type(ONT_DOMAIN, &link) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": link.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/link-types/:id — 删除链接类型（无下游引用，直接删）。
pub(crate) async fn delete_link_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_link_type(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

// ─── 阶段2：ActionType + FunctionDef 声明式 CRUD（存储驱动，写前备份 meta 图）───
//
// 契约（与对象/链接一致的语义）：
//   POST /api/v1/ontology/action-types           body=ActionType   新建/更新（applies_to 校验）
//   PUT  /api/v1/ontology/action-types/:id        body=ActionType   更新
//   DELETE /api/v1/ontology/action-types/:id                        删除（动作为叶子，直接删）
//   POST /api/v1/ontology/function-defs          body=FunctionDef  新建/更新（applies_to 校验）
//   PUT  /api/v1/ontology/function-defs/:id       body=FunctionDef  更新
//   DELETE /api/v1/ontology/function-defs/:id                       删除
// 声明式：动作携带 parameters/preconditions/side_effects 声明，函数携带 returns/expression。
// 内置动作的执行仍由 invoke_action_handler 分派；自定义动作声明可存但暂不可执行（见 invoke）。

/// POST /api/v1/ontology/action-types — 新建或更新动作类型（幂等 upsert；applies_to 校验）。
pub(crate) async fn upsert_action_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(action): Json<crate::knowledge_graph::ontology_layer::ActionType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    if let Err(e) = ontology_guardrails::validate_config(&action.guardrails) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_action_type(ONT_DOMAIN, &action) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": action.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/action-types/:id — 更新动作类型（id 以路径为准）。
pub(crate) async fn update_action_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut action): Json<crate::knowledge_graph::ontology_layer::ActionType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    action.id = id;
    if let Err(e) = ontology_guardrails::validate_config(&action.guardrails) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_action_type(ONT_DOMAIN, &action) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": action.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/action-types/:id — 删除动作类型（叶子元素，直接删）。
pub(crate) async fn delete_action_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_action_type(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// POST /api/v1/ontology/function-defs — 新建或更新函数（幂等 upsert；applies_to 校验）。
pub(crate) async fn upsert_function_def_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(func): Json<crate::knowledge_graph::ontology_layer::FunctionDef>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_function_def(ONT_DOMAIN, &func) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": func.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/function-defs/:id — 更新函数（id 以路径为准）。
pub(crate) async fn update_function_def_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut func): Json<crate::knowledge_graph::ontology_layer::FunctionDef>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    func.id = id;
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_function_def(ONT_DOMAIN, &func) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": func.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/function-defs/:id — 删除函数（叶子元素，直接删）。
pub(crate) async fn delete_function_def_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_function_def(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

// ─── 动力层执行器（ActionType invoke）──────────────────────────────────
//
// 让知识图谱从"只读"变为"可写可执行"：依据 ActionType 做参数校验 + 前置条件检查，
// 再把 side-effect 以 SPARQL 写回 JWT claims 铸造的命名图。
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const ACTION_AUDIT_EVENT: &str = "ACTION_AUDIT";
const ACTION_AUDIT_SOURCE: &str = "ontology-action-api";

/// A tenant-scoped audit record emitted after an action reaches a durable
/// decision. Values are sourced exclusively from verified isolation claims.
#[derive(Debug, Serialize)]
struct ActionAuditEvent<'a> {
    tenant_id: &'a str,
    project_id: &'a str,
    actor_id: &'a str,
    action_id: &'a str,
    staging_id: &'a str,
    decision: &'a str,
    violations: &'a [String],
    timestamp: String,
}

#[derive(Debug, Serialize)]
struct MaterializationAuditEvent<'a> {
    tenant_id: &'a str,
    project_id: &'a str,
    actor_id: &'a str,
    extraction_id: &'a str,
    authority: &'a str,
    reviewer_id: &'a str,
    review_id: Option<&'a str>,
    anchor: Option<&'a MaterializationAnchor>,
    decision: &'a str,
    error: Option<&'a str>,
    timestamp: String,
}

#[allow(clippy::too_many_arguments)]
async fn emit_materialization_audit(
    state: &AppState,
    claims: &IsolationClaims,
    extraction_id: &str,
    authority: &str,
    review_id: Option<&str>,
    anchor: Option<&MaterializationAnchor>,
    decision: &str,
    error: Option<&str>,
) {
    let event = MaterializationAuditEvent {
        tenant_id: claims.tenant_id(),
        project_id: claims.project_id(),
        actor_id: claims.actor_id(),
        extraction_id,
        authority,
        // The caller who makes the explicit confirm is the accountable human
        // decision-maker for this write. A prior review id, if any, is linked
        // separately because legacy review records do not retain actor IDs.
        reviewer_id: claims.actor_id(),
        review_id,
        anchor,
        decision,
        error,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let payload = match serde_json::to_string(&event) {
        Ok(payload) => payload,
        Err(error) => {
            error!(
                extraction_id,
                decision,
                %error,
                "failed to serialize materialization audit event"
            );
            return;
        }
    };
    state
        .core
        .events
        .emit(
            &claims
                .graph_iri()
                .unwrap_or_else(|_| "graph://invalid".to_string()),
            ACTION_AUDIT_EVENT,
            "ontology-materialization-api",
            &payload,
        )
        .await;
}

async fn emit_action_audit(
    state: &AppState,
    claims: &IsolationClaims,
    action_id: &str,
    staging_id: &str,
    decision: &str,
    violations: &[String],
) {
    let event = ActionAuditEvent {
        tenant_id: claims.tenant_id(),
        project_id: claims.project_id(),
        actor_id: claims.actor_id(),
        action_id,
        staging_id,
        decision,
        violations,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let payload = match serde_json::to_string(&event) {
        Ok(payload) => payload,
        Err(error) => {
            error!(
                action_id,
                staging_id,
                decision,
                %error,
                "failed to serialize action audit event"
            );
            return;
        }
    };
    // EventBus emission is intentionally best-effort: its API does not expose
    // delivery failures, and audit publication must never change an action's
    // already-determined HTTP outcome.
    state
        .core
        .events
        .emit(
            &claims
                .graph_iri()
                .unwrap_or_else(|_| "graph://invalid".to_string()),
            ACTION_AUDIT_EVENT,
            ACTION_AUDIT_SOURCE,
            &payload,
        )
        .await;
}

fn unauthorized_isolation_claims() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "verified JWT isolation claims are required" })),
    )
}

/// 本体实例 IRI：https://agentos.ontology/ev/{ObjectType}/{key}
fn ev_instance_iri(obj_type: &str, key: &str) -> String {
    format!("https://agentos.ontology/ev/{}/{}", obj_type, iri_safe(key))
}
/// 对象类型 / 链接类型 IRI（与 ontology_layer 的 ev() 一致）。
fn ev_term_iri(name: &str) -> String {
    format!("https://agentos.ontology/ev/{}", name)
}
/// 属性谓词 IRI。
fn ev_prop_iri(name: &str) -> String {
    format!("https://agentos.ontology/ev/prop/{}", name)
}
/// 主键值转 IRI 安全片段。
fn iri_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_whitespace()
                || matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\')
            {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// 文本字面量项（含转义引号）。
fn lit(s: &str) -> String {
    format!("\"{}\"", sparql_literal(s))
}
/// 十进制数值字面量项。
fn lit_decimal(n: f64) -> String {
    format!("\"{}\"^^<{}>", n, XSD_DECIMAL)
}

/// 属性 upsert：先删旧值再写新值（idempotent）。obj 为完整对象项。
fn upsert_prop_stmts(subject: &str, prop: &str, obj: &str) -> Vec<ClaimsGraphUpdate> {
    vec![
        ClaimsGraphUpdate::delete_where(format!("<{subject}> <{prop}> ?old")),
        ClaimsGraphUpdate::insert_data(format!("<{subject}> <{prop}> {obj}")),
    ]
}

/// 命名图内对象是否存在（前置条件检查）。
fn ev_object_exists(kg: &KnowledgeGraphStore, claims: &IsolationClaims, iri: &str) -> bool {
    let q = format!("SELECT ?o WHERE {{ <{iri}> ?p ?o }} LIMIT 1");
    kg.query_sparql_for_claims(claims, &q)
        .map(|r| !r.is_empty())
        .unwrap_or(false)
}

/// 对象存在性前置条件解析（知识/业务分流，MCP 向后兼容扩展位）。
///
/// - 知识对象（FaultCode / VehicleModel / FAQ…）：查询知识命名图。
/// - 业务对象（Vehicle / Battery / RepairOrder…）：业务数据不入图谱，未来经 MCP
///   对接业务库查询；当前 MCP 未接入，回退查询命名图以保持向后兼容——接入 MCP 后
///   只需替换 Business 分支，调用方（build_action_effects）无需改动。
fn resolve_object_exists(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    object_type: &str,
    key: &str,
) -> bool {
    use crate::knowledge_graph::ontology_layer::{object_kind_of, ObjectKind};
    let iri = ev_instance_iri(object_type, key);
    match object_kind_of(object_type) {
        ObjectKind::Knowledge => ev_object_exists(kg, claims, &iri),
        // TODO(MCP): 业务库接入后改为经 MCP 查询业务对象是否存在；当前回退命名图。
        ObjectKind::Business => ev_object_exists(kg, claims, &iri),
    }
}

fn p_str(params: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    match params.get(name) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}
fn p_num(params: &serde_json::Map<String, Value>, name: &str) -> Option<f64> {
    match params.get(name) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionInvokeRequest {
    /// applies_to 对象实例的主键值（动作作用的目标对象）。
    #[serde(default)]
    pub target: Option<String>,
    /// 动作参数（name → value）。
    #[serde(default)]
    pub params: serde_json::Map<String, Value>,
    /// 仅校验并返回将执行的 SPARQL，不真正写回。
    #[serde(default)]
    pub dry_run: bool,
    /// `auto` commits immediately unless a future guardrail marks the action
    /// high-risk. `require_approval` preserves the staging graph for HITL.
    #[serde(default)]
    pub commit_strategy: ActionCommitStrategy,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionCommitStrategy {
    #[default]
    Auto,
    RequireApproval,
}

/// POST /api/v1/ontology/actions/:id/invoke — 动力层执行器
/// 内置（已实现执行逻辑）的动作 id 白名单——只有这些动作可真正 invoke。
/// 自定义动作（阶段2 声明式 CRUD 新建的）当前 executable=false，声明可存但暂不可执行，
/// 待阶段3 通用执行器（SPARQL 模板 + 护栏）落地后开放。
const BUILTIN_EXECUTABLE_ACTIONS: &[&str] = &["GenerateRepairOrder"];

// ─── 数据沙箱（staging-graph 影子图执行 + 护栏后校验）────────────────────
//
// 让动作写回从"直接落生产图"升级为"先写隔离影子图 → 护栏校验 → 通过才合并、
// 失败即回滚"，等价一次可回滚事务。仅隔离**数据**（命名图级），不隔离计算/进程；
// 计算沙箱（任意代码执行）见 docs 记录，待有需要再实现。
//
//   1. 为本次 invoke 生成 JWT claims 图派生的 per-invocation 影子图
//   2. 把 side-effect 语句里的生产图 IRI 重定向到影子图，写入影子图（生产图零改动）
//   3. 对影子图跑 ASK 护栏（三元组数上限 / 谓词命名空间白名单），任一命中即视为违规
//   4. 通过 → ADD 影子图到生产图 + DROP 影子图（提交）；违规 → DROP 影子图（回滚）

#[derive(Debug, Default)]
struct SandboxGuardrailReport {
    violations: Vec<String>,
    /// A relaxed policy is held for approval even when its hard checks pass.
    high_risk: bool,
}

fn sandbox_guardrail_report(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    staging_id: &str,
    policy: &ontology_guardrails::EffectiveGuardrails,
) -> Result<SandboxGuardrailReport, String> {
    Ok(SandboxGuardrailReport {
        violations: ontology_guardrails::violations(kg, claims, staging_id, policy)?,
        high_risk: ontology_guardrails::is_high_risk(policy),
    })
}

const ACTION_APPROVAL_TTL_HOURS: i64 = 24;

#[derive(Debug)]
enum StagingCommitOutcome {
    Committed(Value),
    Pending(PendingActionApproval),
}

/// 经影子图提交一批写回语句。默认自动合并；HITL 策略或高风险护栏策略会保留影子图。
fn commit_via_staging(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    statements: &[ClaimsGraphUpdate],
    strategy: ActionCommitStrategy,
    action_id: &str,
    now: chrono::DateTime<chrono::Utc>,
    guardrails: &ontology_guardrails::EffectiveGuardrails,
) -> Result<StagingCommitOutcome, (StatusCode, String, Vec<String>, Option<String>)> {
    let staging_id = uuid::Uuid::new_v4().simple().to_string();
    let staging = kg
        .staging_graph_iri_for_claims(claims, &staging_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e, vec![], None))?;

    // 1. 写入影子图（生产图零改动）。任一失败即清理并报错。
    for stmt in statements {
        if let Err(e) = kg.update_staging_for_claims(claims, &staging_id, stmt) {
            let _ = kg.drop_staging_for_claims(claims, &staging_id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("影子图写入失败: {e}"),
                vec![],
                Some(staging_id),
            ));
        }
    }

    // 2. 护栏后校验。违规即回滚（DROP 影子图），生产图不受影响。
    let guardrails = match sandbox_guardrail_report(kg, claims, &staging_id, guardrails) {
        Ok(report) => report,
        Err(e) => {
            let _ = kg.drop_staging_for_claims(claims, &staging_id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("护栏校验失败，已回滚: {e}"),
                vec![],
                Some(staging_id),
            ));
        }
    };
    if !guardrails.violations.is_empty() {
        let _ = kg.drop_staging_for_claims(claims, &staging_id);
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "护栏校验未通过，已回滚（生产图未改动）".to_string(),
            guardrails.violations,
            Some(staging_id),
        ));
    }

    if strategy == ActionCommitStrategy::RequireApproval || guardrails.high_risk {
        let approval = PendingActionApproval {
            approval_id: staging_id.clone(),
            staging_id,
            staging_graph: staging,
            action_id: action_id.to_string(),
            anchor_query: None,
            created_at: now.to_rfc3339(),
            expires_at: (now + chrono::Duration::hours(ACTION_APPROVAL_TTL_HOURS)).to_rfc3339(),
        };
        if let Err(e) = kg.create_action_approval_for_claims(claims, &approval) {
            let _ = kg.drop_staging_for_claims(claims, &approval.staging_id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("审批记录创建失败，已回滚: {e}"),
                vec![],
                Some(approval.staging_id.clone()),
            ));
        }
        return Ok(StagingCommitOutcome::Pending(approval));
    }

    // 自动提交：合并影子图到生产图，再删除影子图。
    if let Err(e) = kg.commit_staging_for_claims(claims, &staging_id) {
        let _ = kg.drop_staging_for_claims(claims, &staging_id);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("影子图合并到生产图失败: {e}"),
            vec![],
            Some(staging_id),
        ));
    }
    let _ = kg.drop_staging_for_claims(claims, &staging_id);

    Ok(StagingCommitOutcome::Committed(json!({
        "sandbox": "staging_graph",
        "staging_graph": staging,
        "guardrails_passed": true,
    })))
}

fn approval_is_expired(approval: &PendingActionApproval) -> bool {
    chrono::DateTime::parse_from_rfc3339(&approval.expires_at)
        .map(|expires_at| expires_at <= chrono::Utc::now())
        .unwrap_or(true)
}

fn cleanup_action_approval(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    approval: &PendingActionApproval,
) {
    let _ = kg.drop_staging_for_claims(claims, &approval.staging_id);
    let _ = kg.delete_action_approval_for_claims(claims, &approval.approval_id);
}

/// GET /api/v1/ontology/action-approvals — pending approvals in the caller's
/// verified tenant/project scope. Expired approvals are lazily discarded.
pub(crate) async fn list_action_approvals_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let approvals = match kg.list_action_approvals_for_claims(claims) {
        Ok(approvals) => approvals,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let active: Vec<_> = approvals
        .into_iter()
        .filter(|approval| {
            if approval_is_expired(approval) {
                cleanup_action_approval(&kg, claims, approval);
                false
            } else {
                true
            }
        })
        .collect();
    let _ = state.kg_store.flush();
    (StatusCode::OK, Json(json!({ "approvals": active }))).into_response()
}

/// POST /api/v1/ontology/action-approvals/:approval_id/approve
pub(crate) async fn approve_action_approval_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(approval_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    resolve_action_approval(&state, identity, approval_id, true).await
}

/// POST /api/v1/ontology/action-approvals/:approval_id/reject
pub(crate) async fn reject_action_approval_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(approval_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    resolve_action_approval(&state, identity, approval_id, false).await
}

async fn resolve_action_approval(
    state: &Arc<AppState>,
    identity: UserIdentity,
    approval_id: String,
    approve: bool,
) -> axum::response::Response {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let approval = match kg.list_action_approvals_for_claims(claims) {
        Ok(approvals) => approvals
            .into_iter()
            .find(|approval| approval.approval_id == approval_id),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let Some(approval) = approval else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "approval not found", "approval_id": approval_id })),
        )
            .into_response();
    };
    if approval_is_expired(&approval) {
        cleanup_action_approval(&kg, claims, &approval);
        let _ = state.kg_store.flush();
        return (
            StatusCode::GONE,
            Json(json!({ "error": "approval expired", "approval_id": approval_id })),
        )
            .into_response();
    }
    if approve {
        if let Err(e) = kg.commit_staging_for_claims(claims, &approval.staging_id) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("approval merge failed: {e}") })),
            )
                .into_response();
        }
        // Entity-resolution and other supervisory flows supply this query
        // server-side. A successful ADD is not evidence: re-read the
        // claims-minted production graph before declaring approval complete.
        if let Some(anchor_query) = &approval.anchor_query {
            match kg.query_sparql_for_claims(claims, anchor_query) {
                Ok(rows) if !rows.is_empty() => {}
                Ok(_) | Err(_) => {
                    let _ = state.kg_store.flush();
                    emit_action_audit(
                        state,
                        claims,
                        &approval.action_id,
                        &approval.staging_id,
                        "needs_repair",
                        &["post_merge_sparql_anchor_failed".into()],
                    )
                    .await;
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "status": "needs_repair",
                            "approval_id": approval.approval_id,
                            "error": "post-merge SPARQL anchor failed; approval retained for repair",
                        })),
                    )
                        .into_response();
                }
            }
        }
    }
    if let Err(e) = kg.drop_staging_for_claims(claims, &approval.staging_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("approval staging cleanup failed: {e}") })),
        )
            .into_response();
    }
    if let Err(e) = kg.delete_action_approval_for_claims(claims, &approval.approval_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("approval metadata cleanup failed: {e}") })),
        )
            .into_response();
    }
    let _ = state.kg_store.flush();
    emit_action_audit(
        state,
        claims,
        &approval.action_id,
        &approval.staging_id,
        if approve { "approved" } else { "rejected" },
        &[],
    )
    .await;
    (
        StatusCode::OK,
        Json(json!({
            "status": if approve { "approved" } else { "rejected" },
            "approval_id": approval.approval_id,
            "staging_graph": approval.staging_graph,
        })),
    )
        .into_response()
}

pub(crate) async fn invoke_action_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(action_id): axum::extract::Path<String>,
    Json(req): Json<ActionInvokeRequest>,
) -> impl IntoResponse {
    use crate::knowledge_graph::ontology_store::OntologyStore;
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims(),
    };
    // 执行分派解耦：动作定义改从存储读取（首启幂等 seed），存储不可用时回退硬编码。
    let ont = (|| {
        let store = OntologyStore::with_shared_store(state.kg_store.clone()).ok()?;
        store.ensure_seeded(ONT_DOMAIN).ok()?;
        store.load_definition(ONT_DOMAIN).ok()
    })()
    .unwrap_or_else(crate::knowledge_graph::ontology_layer::ev_repair_ontology);
    let action = match ont.action_types.iter().find(|a| a.id == action_id) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("未知动作类型: {}", action_id) })),
            )
        }
    };

    // 自定义动作暂不可执行：声明已存于本体，但无内置执行逻辑（待阶段3 通用执行器）。
    if !BUILTIN_EXECUTABLE_ACTIONS.contains(&action_id.as_str()) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": format!("动作「{}」为自定义声明，暂不可执行", action.label),
                "executable": false,
                "reason": "custom_action_not_executable",
            })),
        );
    }

    // 1. 参数校验：必填项存在且非空。
    let missing: Vec<String> = action
        .parameters
        .iter()
        .filter(|p| p.required && p_str(&req.params, &p.name).is_none())
        .map(|p| p.name.clone())
        .collect();
    if !missing.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少必填参数", "missing": missing })),
        );
    }

    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
        }
    };

    // 2. 前置条件 + 3. 组装 side-effect 写回 SPARQL（按动作分派）。
    let now = chrono::Utc::now();
    let (statements, result_meta) =
        match build_action_effects(&action_id, &req, &kg, claims, &now.to_rfc3339()) {
            Ok(v) => v,
            Err((code, msg)) => return (code, Json(json!({ "error": msg }))),
        };

    if req.dry_run {
        // dry runs deliberately have no staging graph and therefore no audit
        // event; they do not produce a state-changing decision to retain.
        return (
            StatusCode::OK,
            Json(json!({
                "status": "dry_run",
                "action": action_id,
                "graph": claims.graph_iri().expect("verified claims were validated"),
                "sparql": statements.iter().map(ClaimsGraphUpdate::sparql).collect::<Vec<_>>(),
                "result": result_meta,
            })),
        );
    }

    // 4. 数据沙箱写回：先写影子图 → 护栏后校验 → 通过才合并到生产图，失败即回滚。
    let guardrails = match ontology_guardrails::effective_config(&ont, &action) {
        Ok(guardrails) => guardrails,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    };
    let outcome = match commit_via_staging(
        &kg,
        claims,
        &statements,
        req.commit_strategy,
        &action_id,
        now,
        &guardrails,
    ) {
        Ok(outcome) => outcome,
        Err((code, msg, violations, staging_id)) => {
            if code == StatusCode::UNPROCESSABLE_ENTITY {
                if let Some(staging_id) = staging_id.as_deref() {
                    emit_action_audit(
                        &state,
                        claims,
                        &action_id,
                        staging_id,
                        "violated",
                        &violations,
                    )
                    .await;
                }
            }
            return (
                code,
                Json(json!({ "error": msg, "violations": violations })),
            );
        }
    };
    let (status, sandbox, decision, staging_id) = match outcome {
        StagingCommitOutcome::Committed(report) => {
            let staging_id = report["staging_graph"]
                .as_str()
                .and_then(|graph| graph.rsplit('/').next())
                .unwrap_or_default()
                .to_string();
            ("ok", report, "committed", staging_id)
        }
        StagingCommitOutcome::Pending(approval) => (
            "pending_approval",
            json!({
                "sandbox": "staging_graph",
                "staging_graph": approval.staging_graph,
                "guardrails_passed": true,
                "approval_id": approval.approval_id,
                "expires_at": approval.expires_at,
            }),
            "pending",
            approval.staging_id,
        ),
    };
    let _ = state.kg_store.flush();
    emit_action_audit(&state, claims, &action_id, &staging_id, decision, &[]).await;

    (
        StatusCode::OK,
        Json(json!({
            "status": status,
            "action": action_id,
            "graph": claims.graph_iri().expect("verified claims were validated"),
            "applied": statements.len(),
            "result": result_meta,
            "sandbox": sandbox,
        })),
    )
}

/// 按动作类型组装前置条件校验 + side-effect 写回 SPARQL 语句序列。
fn build_action_effects(
    action_id: &str,
    req: &ActionInvokeRequest,
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    now: &str,
) -> Result<(Vec<ClaimsGraphUpdate>, Value), (StatusCode, String)> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    match action_id {
        // 依据已确诊故障码为车辆创建维修工单，并建立 forVehicle / diagnoses 链接。
        "GenerateRepairOrder" => {
            let fault_code = req
                .target
                .clone()
                .ok_or_else(|| bad("缺少 target（故障码主键）".into()))?;
            let vin = p_str(&req.params, "vehicle_vin").unwrap();
            let vehicle_iri = ev_instance_iri("Vehicle", &vin);
            // 车辆为业务对象：当前回退命名图校验，未来经 MCP 业务库校验（见 resolve_object_exists）。
            if !resolve_object_exists(kg, claims, "Vehicle", &vin) {
                return Err(bad(format!("前置条件不满足：车辆VIN不存在于图谱 ({vin})")));
            }
            let fault_iri = ev_instance_iri("FaultCode", &fault_code);
            if !resolve_object_exists(kg, claims, "FaultCode", &fault_code) {
                return Err(bad(format!(
                    "前置条件不满足：故障码未确诊/不存在 ({fault_code})"
                )));
            }
            let order_id = format!("RO-{}", uuid::Uuid::new_v4().hyphenated());
            let order_iri = ev_instance_iri("RepairOrder", &order_id);
            let mut triples = vec![
                format!(
                    "<{o}> a <{c}>",
                    o = order_iri,
                    c = ev_term_iri("RepairOrder")
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("order_id"),
                    v = lit(&order_id)
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("vehicle_vin"),
                    v = lit(&vin)
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("fault_code"),
                    v = lit(&fault_code)
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("status"),
                    v = lit("待处理")
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("created_at"),
                    v = lit(now)
                ),
                format!(
                    "<{o}> <{l}> <{veh}>",
                    o = order_iri,
                    l = ev_term_iri("forVehicle"),
                    veh = vehicle_iri
                ),
                format!(
                    "<{o}> <{l}> <{f}>",
                    o = order_iri,
                    l = ev_term_iri("diagnoses"),
                    f = fault_iri
                ),
                format!(
                    "<{o}> <{lbl}> {v}",
                    o = order_iri,
                    lbl = RDFS_LABEL,
                    v = lit(&order_id)
                ),
            ];
            if let Some(a) = p_str(&req.params, "assigned_to") {
                triples.push(format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("assigned_to"),
                    v = lit(&a)
                ));
            }
            if let Some(c) = p_num(&req.params, "estimated_cost") {
                triples.push(format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("estimated_cost"),
                    v = lit_decimal(c)
                ));
            }
            let stmt = ClaimsGraphUpdate::insert_data(format!("{} .", triples.join(" .\n")));
            Ok((
                vec![stmt],
                json!({ "order_id": order_id, "order_iri": order_iri, "vehicle": vehicle_iri, "fault_code": fault_iri }),
            ))
        }
        // 检测后写回电池 SOH（0-100），并记录更新时间。
        "UpdateBatterySoh" => {
            let battery_id = p_str(&req.params, "battery_id").unwrap();
            let soh = p_num(&req.params, "soh").ok_or_else(|| bad("soh 必须为数值".into()))?;
            if !(0.0..=100.0).contains(&soh) {
                return Err(bad("前置条件不满足：SOH 取值需在 0-100".into()));
            }
            let bat_iri = ev_instance_iri("Battery", &battery_id);
            // 电池为业务对象：当前回退命名图校验，未来经 MCP 业务库校验。
            if !resolve_object_exists(kg, claims, "Battery", &battery_id) {
                return Err(bad(format!(
                    "前置条件不满足：电池对象不存在 ({battery_id})"
                )));
            }
            let mut stmts = upsert_prop_stmts(&bat_iri, &ev_prop_iri("soh"), &lit_decimal(soh));
            stmts.extend(upsert_prop_stmts(
                &bat_iri,
                &ev_prop_iri("soh_updated_at"),
                &lit(now),
            ));
            Ok((stmts, json!({ "battery": bat_iri, "soh": soh })))
        }
        // 对存在批次性缺陷的车型打召回标记。
        "MarkRecall" => {
            let model_id = p_str(&req.params, "model_id").unwrap();
            let reason = p_str(&req.params, "recall_reason").unwrap();
            let model_iri = ev_instance_iri("VehicleModel", &model_id);
            if !resolve_object_exists(kg, claims, "VehicleModel", &model_id) {
                return Err(bad(format!("前置条件不满足：车型对象不存在 ({model_id})")));
            }
            let mut stmts = upsert_prop_stmts(&model_iri, &ev_prop_iri("recalled"), &lit("true"));
            stmts.extend(upsert_prop_stmts(
                &model_iri,
                &ev_prop_iri("recall_reason"),
                &lit(&reason),
            ));
            stmts.extend(upsert_prop_stmts(
                &model_iri,
                &ev_prop_iri("recall_marked_at"),
                &lit(now),
            ));
            Ok((
                stmts,
                json!({ "model": model_iri, "recalled": true, "recall_reason": reason }),
            ))
        }
        // 将一次诊断沉淀为 FAQ，挂接到对应故障码。
        "AppendFaq" => {
            let code = req
                .target
                .clone()
                .or_else(|| p_str(&req.params, "code"))
                .ok_or_else(|| bad("缺少 target/code（故障码主键）".into()))?;
            let question = p_str(&req.params, "question").unwrap();
            let answer = p_str(&req.params, "answer").unwrap();
            let fault_iri = ev_instance_iri("FaultCode", &code);
            if !resolve_object_exists(kg, claims, "FaultCode", &code) {
                return Err(bad(format!("前置条件不满足：故障码对象不存在 ({code})")));
            }
            let faq_id = format!("FAQ-{}", uuid::Uuid::new_v4().hyphenated());
            let faq_iri = ev_instance_iri("FAQ", &faq_id);
            let triples = [
                format!("<{f}> a <{c}>", f = faq_iri, c = ev_term_iri("FAQ")),
                format!(
                    "<{f}> <{p}> {o}",
                    f = faq_iri,
                    p = ev_prop_iri("faq_id"),
                    o = lit(&faq_id)
                ),
                format!(
                    "<{f}> <{p}> {o}",
                    f = faq_iri,
                    p = ev_prop_iri("question"),
                    o = lit(&question)
                ),
                format!(
                    "<{f}> <{p}> {o}",
                    f = faq_iri,
                    p = ev_prop_iri("answer"),
                    o = lit(&answer)
                ),
                format!(
                    "<{f}> <{lbl}> {o}",
                    f = faq_iri,
                    lbl = RDFS_LABEL,
                    o = lit(&question)
                ),
                format!(
                    "<{fc}> <{l}> <{f}>",
                    fc = fault_iri,
                    l = ev_term_iri("relatedFaq"),
                    f = faq_iri
                ),
            ];
            let stmt = ClaimsGraphUpdate::insert_data(format!("{} .", triples.join(" .\n")));
            Ok((
                vec![stmt],
                json!({ "faq_id": faq_id, "faq_iri": faq_iri, "fault_code": fault_iri }),
            ))
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            format!("动作 {action_id} 暂未实现执行器"),
        )),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ontology CRUD 集成测试（原与 skill_manifest_tests 混放，随 skills 拆分迁出后独立）
// ──────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod ontology_crud_tests {
    use super::*;
    use crate::core::core_types::{CoreConfig, SemanticCore};
    use crate::tools::prompt_registry::PromptRegistry;
    use axum::http::StatusCode;
    use axum::{
        routing::{get, post, put},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use tower::ServiceExt;

    use super::super::{api_gov::ApiUsageState, AppState, TEST_ENV_LOCK};

    fn make_state(tmp: &std::path::Path) -> Arc<AppState> {
        let l0 = tmp.join("l0");
        std::fs::create_dir_all(&l0).unwrap();
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: l0.to_str().unwrap().to_string(),
                event_buffer_size: 10,
                enable_metrics: false,
                eviction_config: None,
            })
            .unwrap(),
        );
        let gateway = Arc::new(
            crate::gateway::UnifiedGateway::new(&crate::config::GatewaySettings {
                base_url: "http://localhost".into(),
                api_key: String::new(),
                default_model: "test-model".into(),
                timeout_seconds: 30,
                max_retries: 1,
                retry_base_ms: 500,
                use_responses_api: false,
                model_mapping: std::collections::HashMap::new(),
            })
            .unwrap(),
        );
        let kg_store = Arc::new(oxigraph::store::Store::new().unwrap());
        Arc::new(AppState {
            core,
            gateway,
            kg_store,
            config_info: Arc::new(tokio::sync::RwLock::new(serde_json::json!({}))),
            agents_info: serde_json::json!({ "count": 0, "agents": [] }),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![])),
            prompts: Arc::new(PromptRegistry::new()),
            kb_categories: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_bases: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_packs: Arc::new(tokio::sync::RwLock::new(vec![])),
            vector_store: Arc::new(arc_swap::ArcSwapOption::empty()),
            blob_store: None,
            task_executor: None,
            batch_manager: None,
            api_clients: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_keys: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_usage: Arc::new(ApiUsageState::default()),
            online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
            online_corpus_queue_capacity: 10,
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    fn test_jwt(tenant: &str) -> String {
        encode(
            &Header::default(),
            &super::super::iam::JwtClaims {
                sub: "ontology-tester".to_string(),
                tenant_id: tenant.to_string(),
                project_id: Some("repair".to_string()),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn readiness_report_is_claims_scoped_read_only_and_identifies_gaps() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_readiness_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let claims =
            IsolationClaims::from_verified("tenant-a", "repair", "ontology-tester").unwrap();
        let ontology_store =
            crate::knowledge_graph::ontology_store::OntologyStore::with_shared_store(
                state.kg_store.clone(),
            )
            .unwrap();
        ontology_store.ensure_seeded(ONT_DOMAIN).unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        kg.create_type_draft_for_claims(
            &claims,
            &PendingTypeDraft {
                draft_id: "proposed-asset".into(),
                source: "test".into(),
                bundle: TypeDraftBundle {
                    object_types: vec![crate::knowledge_graph::ontology_layer::ObjectType {
                        id: "ProposedAsset".into(),
                        iri: crate::knowledge_graph::ontology_layer::ev("ProposedAsset"),
                        label: "Proposed Asset".into(),
                        description: "draft".into(),
                        icon: "Box".into(),
                        color: "slate".into(),
                        primary_key: "id".into(),
                        title_property: "id".into(),
                        kind: Default::default(),
                        properties: vec![],
                    }],
                    link_types: vec![],
                    provenance: None,
                    suggested_links: vec![],
                    warnings: vec![],
                },
                created_at: chrono::Utc::now().to_rfc3339(),
                expires_at: (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
            },
        )
        .unwrap();
        let before = ontology_store.load_definition(ONT_DOMAIN).unwrap();
        let drafts_before = kg.list_type_drafts_for_claims(&claims).unwrap();
        let app = Router::new()
            .route(
                "/api/v1/ontology/readiness-report",
                post(ontology_readiness_report_handler),
            )
            .with_state(state.clone());
        let body = json!({
            "required_object_types": ["Vehicle", "ProposedAsset", "MissingAsset"],
            "required_link_types": ["triggers"]
        });
        let no_claims = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/readiness-report")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(no_claims).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let report = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/ontology/readiness-report")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", test_jwt("tenant-a")))
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(report.status(), StatusCode::OK);
        let report: Value = serde_json::from_slice(
            &axum::body::to_bytes(report.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(report["read_only"], true);
        assert_eq!(report["scenario_attach_ready"], false);
        assert_eq!(report["coverage"]["promoted"], 2);
        assert_eq!(report["coverage"]["open_draft"], 1);
        assert_eq!(report["coverage"]["missing"], 1);
        assert!(report["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["id"] == "MissingAsset"));
        assert_eq!(
            ontology_store
                .load_definition(ONT_DOMAIN)
                .unwrap()
                .object_types
                .len(),
            before.object_types.len()
        );
        assert_eq!(
            kg.list_type_drafts_for_claims(&claims).unwrap().len(),
            drafts_before.len()
        );
        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 阶段0 无回归：GET /api/v1/ontology/types 改读 Oxigraph 元命名图后，
    /// 响应须与硬编码 ev_repair_ontology() 逐字段一致（首启由 ensure_seeded 幂等 seed）。
    #[tokio::test]
    async fn test_ontology_types_matches_hardcoded() {
        let tmp = std::env::temp_dir().join(format!("agentos_ont_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/v1/ontology/types")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        let ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let expected = json!({
            "domain": ont.domain,
            "counts": {
                "object_types": ont.object_types.len(),
                "link_types": ont.link_types.len(),
                "action_types": ont.action_types.len(),
                "functions": ont.functions.len(),
            },
            "object_types": ont.object_types,
            "link_types": ont.link_types,
            "action_types": ont.action_types,
            "functions": ont.functions,
        });
        assert_eq!(body, expected, "存储驱动的响应须与硬编码逐字段一致");

        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 阶段1 CRUD：新建对象 → GET 可见 → 删除被引用返回 409 → 删链接后可删对象。
    #[tokio::test]
    async fn isolation_contract_ontology_write_requires_jwt_and_uses_claims_scope() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_ontcrud_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .route(
                "/api/v1/ontology/object-types",
                post(upsert_object_type_handler),
            )
            .route(
                "/api/v1/ontology/object-types/:id",
                put(update_object_type_handler).delete(delete_object_type_handler),
            )
            .route(
                "/api/v1/ontology/link-types",
                post(upsert_link_type_handler),
            )
            .route(
                "/api/v1/ontology/link-types/:id",
                put(update_link_type_handler).delete(delete_link_type_handler),
            )
            .with_state(state);

        let post_json = |uri: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri.to_string())
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };
        let del = |uri: &str| {
            axum::http::Request::builder()
                .method("DELETE")
                .uri(uri.to_string())
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap()
        };

        // 1) 新建对象 Widget
        let obj = json!({
            "id": "Widget", "iri": "https://agentos.ontology/ev/Widget",
            "label": "小部件", "description": "测试", "icon": "Box", "color": "blue",
            "primary_key": "name", "title_property": "name", "properties": []
        });
        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/object-types")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(obj.to_string()))
            .unwrap();
        let r = app.clone().oneshot(unauthenticated).await.unwrap();
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "无 JWT 的 upsert 必须拒绝"
        );

        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/object-types", obj))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建对象应 200");

        // 2) GET 可见
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let has_widget = body["object_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == "Widget");
        assert!(has_widget, "新建对象应出现在 GET /types");

        // 3) 新建引用 Widget 的链接（Widget→Widget）
        let link = json!({
            "id": "WidgetSelf", "iri": "https://agentos.ontology/ev/WidgetSelf",
            "label": "自关联", "description": "", "source": "Widget", "target": "Widget",
            "cardinality": "one_to_many"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/link-types", link))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建链接应 200");

        // 4) 删对象被引用 → 409
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/object-types/Widget"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT, "被链接引用应返回 409");

        // 5) 删链接后可删对象
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/link-types/WidgetSelf"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/object-types/Widget"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "无引用后应可删");

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn type_draft_csv_requires_claims_and_explicit_human_promotion() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_type_draft_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .route(
                "/api/v1/ontology/type-drafts",
                get(list_type_drafts_handler),
            )
            .route(
                "/api/v1/ontology/type-drafts/from-csv",
                post(create_csv_type_draft_handler),
            )
            .route(
                "/api/v1/ontology/type-drafts/:draft_id/promote",
                post(promote_type_draft_handler),
            )
            .with_state(state);
        let body = json!({
            "csv": "asset_id,Display Name,active\nA-1,Inverter,true\n",
            "object_id": "imported asset",
            "label": "Imported Asset"
        });
        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/type-drafts/from-csv")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let create = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/type-drafts/from-csv")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let created = app.clone().oneshot(create).await.unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(created.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(created["actions_generated"], false);
        assert!(created["preview"]["object_types"][0]["properties"]
            .as_array()
            .unwrap()
            .iter()
            .all(|property| property["prop_type"] == "string"));
        let draft_id = created["draft_id"].as_str().unwrap();

        let no_confirm = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/ontology/type-drafts/{draft_id}/promote"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(r#"{"confirm":false}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(no_confirm).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let promote = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/ontology/type-drafts/{draft_id}/promote"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(r#"{"confirm":true}"#))
            .unwrap();
        let promoted = app.clone().oneshot(promote).await.unwrap();
        let promoted_status = promoted.status();
        let promoted_body = axum::body::to_bytes(promoted.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            promoted_status,
            StatusCode::OK,
            "promotion response: {}",
            String::from_utf8_lossy(&promoted_body)
        );

        let types = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let types: Value = serde_json::from_slice(
            &axum::body::to_bytes(types.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(types["object_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["id"] == "ImportedAsset"));

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn breaking_type_draft_promotion_requires_force_and_audit_fields() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "agentos_breaking_type_draft_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route(
                "/api/v1/ontology/type-drafts/from-csv",
                post(create_csv_type_draft_handler),
            )
            .route(
                "/api/v1/ontology/type-drafts/:draft_id/promote",
                post(promote_type_draft_handler),
            )
            .with_state(state);
        // The promoted Brand type has country and logo_url. This draft omits
        // them, exercising the production compatibility gate.
        let create = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/type-drafts/from-csv")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(
                json!({"csv": "name\nAcme\n", "object_id": "Brand"}).to_string(),
            ))
            .unwrap();
        let created = app.clone().oneshot(create).await.unwrap();
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(created.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let draft_id = created["draft_id"].as_str().unwrap();

        let reject = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/ontology/type-drafts/{draft_id}/promote"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(r#"{"confirm":true}"#))
            .unwrap();
        let rejected = app.clone().oneshot(reject).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::CONFLICT);
        let rejected: Value = serde_json::from_slice(
            &axum::body::to_bytes(rejected.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(rejected["compatibility_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["kind"] == "property_removed"));

        let force = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/ontology/type-drafts/{draft_id}/promote"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(
                r#"{"confirm":true,"force_breaking":true,"audit":{"reason":"source schema retirement","ticket":"ENG-152"}}"#,
            ))
            .unwrap();
        let promoted = app.clone().oneshot(force).await.unwrap();
        assert_eq!(promoted.status(), StatusCode::OK);
        let promoted: Value = serde_json::from_slice(
            &axum::body::to_bytes(promoted.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(promoted["audit"]["actor_id"], "ontology-tester");
        assert_eq!(promoted["audit"]["ticket"], "ENG-152");
        assert_eq!(promoted["audit"]["force_breaking"], true);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn schema_induction_stays_out_of_types_until_explicitly_promoted() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp =
            std::env::temp_dir().join(format!("agentos_induction_draft_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .route(
                "/api/v1/ontology/type-drafts/from-induction",
                post(create_schema_induction_type_draft_handler),
            )
            .route(
                "/api/v1/ontology/type-drafts/:draft_id/promote",
                post(promote_type_draft_handler),
            )
            .with_state(state);
        let body = json!({
            "candidate_terms": ["Field Sensor", "record"],
            "documents": [{
                "id": "maintenance-handbook-v2",
                "text": "Field Sensor readings are collected. Field Sensor alerts are reviewed."
            }],
            "model_version": "terms-assistant-1",
            "rule_version": "terminology-v1"
        });
        let create = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/type-drafts/from-induction")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let created = app.clone().oneshot(create).await.unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(created.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(created["source"], "schema_induction");
        assert_eq!(created["actions_generated"], false);
        assert_eq!(
            created["preview"]["provenance"]["model_version"],
            "terms-assistant-1"
        );
        assert_eq!(
            created["preview"]["provenance"]["rule_version"],
            "terminology-v1"
        );
        assert_eq!(
            created["preview"]["provenance"]["source_document_ids"][0],
            "maintenance-handbook-v2"
        );
        assert!(created["preview"]["link_types"]
            .as_array()
            .unwrap()
            .is_empty());
        let draft_id = created["draft_id"].as_str().unwrap();

        let types = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let types: Value = serde_json::from_slice(
            &axum::body::to_bytes(types.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(types["object_types"]
            .as_array()
            .unwrap()
            .iter()
            .all(|object| object["id"] != "FieldSensor"));

        let promote = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/ontology/type-drafts/{draft_id}/promote"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(r#"{"confirm":true}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(promote).await.unwrap().status(),
            StatusCode::OK
        );

        let types = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let types: Value = serde_json::from_slice(
            &axum::body::to_bytes(types.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(types["object_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["id"] == "FieldSensor"));

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn openapi_type_draft_is_claims_scoped_and_promoted_only_after_confirmation() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "agentos_openapi_type_draft_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .route(
                "/api/v1/ontology/type-drafts/from-openapi",
                post(create_openapi_type_draft_handler),
            )
            .route(
                "/api/v1/ontology/type-drafts/:draft_id/promote",
                post(promote_type_draft_handler),
            )
            .with_state(state);
        let body = json!({
            "document": {
                "openapi": "3.0.3",
                "info": {"title": "Asset API", "version": "1"},
                "components": {
                    "schemas": {
                        "ImportedAsset": {
                            "type": "object",
                            "required": ["asset_id"],
                            "properties": {
                                "asset_id": {"type": "string"},
                                "active": {"type": "boolean"}
                            }
                        }
                    }
                }
            }
        });

        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/type-drafts/from-openapi")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let create = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/type-drafts/from-openapi")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let created = app.clone().oneshot(create).await.unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(created.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(created["source"], "openapi");
        assert_eq!(created["actions_generated"], false);
        let draft_id = created["draft_id"].as_str().unwrap();

        let no_confirm = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/ontology/type-drafts/{draft_id}/promote"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(r#"{"confirm":false}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(no_confirm).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let promote = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/ontology/type-drafts/{draft_id}/promote"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(r#"{"confirm":true}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(promote).await.unwrap().status(),
            StatusCode::OK
        );

        let types = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let types: Value = serde_json::from_slice(
            &axum::body::to_bytes(types.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(types["object_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["id"] == "ImportedAsset"));

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn constrained_extraction_requires_claims_and_writes_only_staging() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "agentos_constrained_extract_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route(
                "/api/v1/ontology/constrained-extractions",
                post(constrained_extraction_handler),
            )
            .with_state(state.clone());
        let body = json!({
            "text": "P0A1 affects the battery system.",
            "source": {"blob_id": "manual-p0a1", "version": "v1"},
            "extractor": "test-extractor",
            "model": "test-model",
            "candidates": {
                "nodes": [
                    {"id": "p0a1", "node_type": "FaultCode", "label": "P0A1", "properties": {}},
                    {"id": "battery", "node_type": "System", "label": "Battery system", "properties": {}}
                ],
                "edges": [
                    {"source": "p0a1", "target": "battery", "relation": "affectsSystem", "properties": {}}
                ]
            }
        });
        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/constrained-extractions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/constrained-extractions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["status"], "staged");
        assert_eq!(response["production_write"], false);
        let extraction_id = response["extraction_id"].as_str().unwrap();
        let claims = IsolationClaims::from_verified("tenant-a", "repair", "tester").unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        let staging = kg
            .query_staging_for_claims(&claims, extraction_id, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
            .unwrap();
        assert!(
            staging
                .iter()
                .any(|row| row["?o"] == "https://agentos.ontology/ev/FaultCode"),
            "canonical type triple must be present in staging"
        );
        assert!(
            staging
                .iter()
                .any(|row| row["?p"] == "https://agentos.ontology/extraction/blobId"),
            "source blob provenance must be present in staging"
        );
        assert!(
            kg.query_sparql_for_claims(&claims, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
                .unwrap()
                .is_empty(),
            "constrained extraction must not write the production graph"
        );
        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn isolation_contract_materialization_requires_claims_confirmation_gate_and_anchor() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "agentos_materialize_extraction_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let claims = IsolationClaims::from_verified("tenant-a", "repair", "tester").unwrap();
        let other_claims = IsolationClaims::from_verified("tenant-b", "repair", "tester").unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        let token = test_jwt("tenant-a");
        let other_token = test_jwt("tenant-b");
        let production_before = kg
            .query_sparql_for_claims(&claims, "SELECT ?s WHERE { ?s ?p ?o }")
            .unwrap()
            .len();
        let other_production_before = kg
            .query_sparql_for_claims(&other_claims, "SELECT ?s WHERE { ?s ?p ?o }")
            .unwrap()
            .len();
        let mut audits = state.core.events.subscribe();
        let app = Router::new()
            .route(
                "/api/v1/ontology/constrained-extractions/:id/quality-gate",
                post(quality_gate_handler),
            )
            .route(
                "/api/v1/ontology/constrained-extractions/:id/materialize",
                post(materialize_constrained_extraction_handler),
            )
            .route(
                "/api/v1/ontology/entity-resolution/suggestions",
                post(create_entity_resolution_suggestion_handler),
            )
            .route(
                "/api/v1/ontology/extraction-reviews",
                get(list_extraction_reviews_handler),
            )
            .route(
                "/api/v1/ontology/extraction-reviews/:id/:decision",
                post(resolve_extraction_review_handler),
            )
            .with_state(state.clone());
        let post = |uri: String, token: Option<&str>, body: Value| {
            let mut request = axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            request
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };

        // Missing verified claims and missing explicit confirmation both fail
        // before any production graph operation.
        let no_claims = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/no-claims/materialize".into(),
                None,
                json!({"confirm": true}),
            ))
            .await
            .unwrap();
        assert_eq!(no_claims.status(), StatusCode::UNAUTHORIZED);
        let invalid_claims = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/no-claims/materialize".into(),
                Some(&test_jwt("tenant/a")),
                json!({"confirm": true}),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_claims.status(), StatusCode::UNAUTHORIZED);
        for request in [
            post(
                "/api/v1/ontology/entity-resolution/suggestions".into(),
                None,
                json!({"source_iri": "urn:source", "mention": "Source"}),
            ),
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/ontology/extraction-reviews")
                .body(axum::body::Body::empty())
                .unwrap(),
            post(
                "/api/v1/ontology/extraction-reviews/review-1/approve".into(),
                None,
                json!({}),
            ),
        ] {
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "ER and review boundaries must reject missing claims before reading or writing"
            );
        }
        for request in [
            post(
                "/api/v1/ontology/entity-resolution/suggestions".into(),
                Some(&test_jwt("tenant/a")),
                json!({"source_iri": "urn:source", "mention": "Source"}),
            ),
            post(
                "/api/v1/ontology/extraction-reviews/review-1/approve".into(),
                Some(&test_jwt("tenant/a")),
                json!({}),
            ),
        ] {
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "ER and review boundaries must reject invalid claims"
            );
        }
        let no_confirm = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/no-confirm/materialize".into(),
                Some(&token),
                json!({"confirm": false}),
            ))
            .await
            .unwrap();
        assert_eq!(no_confirm.status(), StatusCode::BAD_REQUEST);

        kg.update_staging_for_claims(
            &claims,
            "blocked",
            &ClaimsGraphUpdate::insert_data("<urn:blocked> <urn:p> <urn:o> ."),
        )
        .unwrap();
        let failed_gate = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/blocked/quality-gate".into(),
                Some(&token),
                json!({"assertions": [{"code": "must_be_empty", "query": "ASK { ?s ?p ?o }"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(failed_gate.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let cross_scope = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/blocked/materialize".into(),
                Some(&other_token),
                json!({"confirm": true}),
            ))
            .await
            .unwrap();
        assert_eq!(cross_scope.status(), StatusCode::CONFLICT);
        let blocked = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/blocked/materialize".into(),
                Some(&token),
                json!({"confirm": true}),
            ))
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::CONFLICT);
        assert!(
            kg.query_sparql_for_claims(&claims, "SELECT ?s WHERE { <urn:blocked> ?p ?o }")
                .unwrap()
                .is_empty(),
            "a failed gate must never materialize staging"
        );
        assert_eq!(
            kg.query_sparql_for_claims(&claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            production_before,
            "missing, invalid, blocked, and cross-scope materialization must not write production"
        );
        assert_eq!(
            kg.query_sparql_for_claims(&other_claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            other_production_before,
            "a client path extraction id cannot materialize into another tenant graph"
        );
        kg.update_for_claims(
            &claims,
            &ClaimsGraphUpdate::insert_data(
                "<urn:cross-source> <http://www.w3.org/2000/01/rdf-schema#label> \"Source\" .",
            ),
        )
        .unwrap();
        let cross_scope_er = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/entity-resolution/suggestions".into(),
                Some(&other_token),
                json!({"source_iri": "urn:cross-source", "mention": "Source"}),
            ))
            .await
            .unwrap();
        assert_eq!(cross_scope_er.status(), StatusCode::NOT_FOUND);
        kg.create_extraction_review_for_claims(
            &claims,
            &PendingExtractionReview {
                review_id: "review-1".into(),
                extraction_id: "blocked".into(),
                staging_graph: kg.staging_graph_iri_for_claims(&claims, "blocked").unwrap(),
                gate_status: "blocked".into(),
                report_json: "{}".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
                decision: "pending".into(),
            },
        )
        .unwrap();
        let cross_scope_review = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/extraction-reviews/review-1/approve".into(),
                Some(&other_token),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(cross_scope_review.status(), StatusCode::BAD_REQUEST);

        kg.update_staging_for_claims(
            &claims,
            "approved",
            &ClaimsGraphUpdate::insert_data("<urn:approved> <urn:p> <urn:o> ."),
        )
        .unwrap();
        let passed_gate = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/approved/quality-gate".into(),
                Some(&token),
                json!({"assertions": []}),
            ))
            .await
            .unwrap();
        assert_eq!(passed_gate.status(), StatusCode::OK);
        assert_eq!(
            quality_gate_reports_for_extraction(&kg, &claims, "approved")
                .unwrap()
                .len(),
            1,
            "a passed gate report must be available to materialization"
        );
        let materialized = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/approved/materialize".into(),
                Some(&token),
                json!({"confirm": true}),
            ))
            .await
            .unwrap();
        let materialized_status = materialized.status();
        let materialized_body = axum::body::to_bytes(materialized.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            materialized_status,
            StatusCode::OK,
            "materialization response: {}",
            String::from_utf8_lossy(&materialized_body)
        );
        let body: Value = serde_json::from_slice(&materialized_body).unwrap();
        assert_eq!(body["status"], "materialized");
        assert_eq!(body["anchor"]["passed"], true);
        assert!(body["anchor"]["staging_triple_count"].as_u64().is_some());
        let audit = audits.recv().await.unwrap();
        assert_eq!(audit.event_type, ACTION_AUDIT_EVENT);
        let audit: Value = serde_json::from_str(&audit.payload).unwrap();
        assert_eq!(audit["extraction_id"], "approved");
        assert_eq!(audit["decision"], "materialized");
        assert_eq!(audit["anchor"]["passed"], true);
        assert!(
            !kg.query_sparql_for_claims(&claims, "SELECT ?s WHERE { <urn:approved> ?p ?o }")
                .unwrap()
                .is_empty(),
            "HTTP success requires production evidence from the SPARQL anchor"
        );
        kg.update_staging_for_claims(
            &claims,
            "human-override",
            &ClaimsGraphUpdate::insert_data("<urn:human-override> <urn:p> <urn:o> ."),
        )
        .unwrap();
        kg.create_extraction_review_for_claims(
            &claims,
            &PendingExtractionReview {
                review_id: "human-override-review".into(),
                extraction_id: "human-override".into(),
                staging_graph: kg
                    .staging_graph_iri_for_claims(&claims, "human-override")
                    .unwrap(),
                gate_status: "failed".into(),
                report_json: "{}".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
                decision: "approved".into(),
            },
        )
        .unwrap();
        let override_materialized = app
            .oneshot(post(
                "/api/v1/ontology/constrained-extractions/human-override/materialize".into(),
                Some(&token),
                json!({"confirm": true}),
            ))
            .await
            .unwrap();
        assert_eq!(override_materialized.status(), StatusCode::OK);
        let override_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(override_materialized.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(override_body["authority"], "recorded_human_override");
        assert_eq!(override_body["anchor"]["passed"], true);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 阶段2 CRUD + 执行分派解耦：新建动作/函数 → GET 可见 → 自定义动作 invoke 返回 422
    /// （不可执行）→ 内置动作 dry_run 仍可执行 → 删除动作/函数回归。
    #[tokio::test]
    async fn test_ontology_action_function_crud() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_ontaf_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .route(
                "/api/v1/ontology/action-types",
                post(upsert_action_type_handler),
            )
            .route(
                "/api/v1/ontology/action-types/:id",
                put(update_action_type_handler).delete(delete_action_type_handler),
            )
            .route(
                "/api/v1/ontology/function-defs",
                post(upsert_function_def_handler),
            )
            .route(
                "/api/v1/ontology/function-defs/:id",
                put(update_function_def_handler).delete(delete_function_def_handler),
            )
            .route(
                "/api/v1/ontology/actions/:id/invoke",
                post(invoke_action_handler),
            )
            .with_state(state);

        let post_json = |uri: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri.to_string())
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };
        let del = |uri: &str| {
            axum::http::Request::builder()
                .method("DELETE")
                .uri(uri.to_string())
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap()
        };

        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/actions/GenerateRepairOrder/invoke")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({}).to_string()))
            .unwrap();
        let r = app.clone().oneshot(unauthenticated).await.unwrap();
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "无 JWT 的 invoke 必须拒绝"
        );

        // 1) 新建自定义动作（applies_to=FaultCode，已 seed）
        let action = json!({
            "id": "TagFault", "iri": "https://agentos.ontology/ev/action/TagFault",
            "label": "标记故障", "description": "测试自定义动作", "applies_to": "FaultCode",
            "parameters": [], "preconditions": [], "side_effects": [], "icon": "Zap"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/action-types", action))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建动作应 200");

        // 2) 新建函数
        let func = json!({
            "id": "FaultScore", "label": "故障评分", "description": "测试函数",
            "applies_to": "FaultCode", "returns": "number", "expression": "1 + 1"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/function-defs", func))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建函数应 200");

        // 3) applies_to 不存在对象 → 400
        let bad = json!({
            "id": "Bad", "iri": "https://agentos.ontology/ev/action/Bad",
            "label": "坏动作", "description": "", "applies_to": "NoSuchObj",
            "parameters": [], "preconditions": [], "side_effects": [], "icon": "Zap"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/action-types", bad))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "applies_to 不存在应 400"
        );

        // 4) GET 可见
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["action_types"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == "TagFault"),
            "新建动作应出现在 GET /types"
        );
        assert!(
            body["functions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "FaultScore"),
            "新建函数应出现在 GET /types"
        );

        // 5) 自定义动作 invoke → 422（不可执行）
        let r = app
            .clone()
            .oneshot(post_json(
                "/api/v1/ontology/actions/TagFault/invoke",
                json!({ "dry_run": true }),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "自定义动作应不可执行 422"
        );

        // 6) 内置动作 dry_run 仍可（缺必填参数 → 400，证明走到内置执行分派而非 422）
        let r = app
            .clone()
            .oneshot(post_json(
                "/api/v1/ontology/actions/GenerateRepairOrder/invoke",
                json!({ "dry_run": true }),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "内置动作缺必填参数应 400（证明未被 422 拦截）"
        );

        // 7) 删除动作/函数
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/action-types/TagFault"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "删动作应 200");
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/function-defs/FaultScore"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "删函数应 200");

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn golden_action_dry_run_and_guardrail_contract() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fixture: Value =
            serde_json::from_str(include_str!("../../../evals/golden/action-invocation.json"))
                .expect("golden action fixture must be valid JSON");
        let dry_run = &fixture["dry_run"];
        let tmp = std::env::temp_dir().join(format!("agentos_golden_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let claims = IsolationClaims::from_verified("tenant-a", "repair", "tester").unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        kg.update_for_claims(
            &claims,
            &ClaimsGraphUpdate::insert_data(format!(
                "<{}> a <{}> . <{}> a <{}> .",
                ev_instance_iri("Vehicle", "LVIN123"),
                ev_term_iri("Vehicle"),
                ev_instance_iri("FaultCode", "P0A80"),
                ev_term_iri("FaultCode"),
            )),
        )
        .unwrap();

        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route(
                "/api/v1/ontology/actions/:id/invoke",
                post(invoke_action_handler),
            )
            .with_state(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/v1/ontology/actions/{}/invoke",
                        dry_run["action_id"].as_str().unwrap()
                    ))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::from(dry_run["request"].to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], dry_run["expected"]["status"]);
        assert_eq!(body["action"], dry_run["action_id"]);
        for expected in dry_run["expected"]["sparql_contains"].as_array().unwrap() {
            assert!(
                body["sparql"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|statement| statement
                        .as_str()
                        .unwrap()
                        .contains(expected.as_str().unwrap())),
                "dry run SPARQL is missing {expected}"
            );
        }
        for field in dry_run["expected"]["result_fields"].as_array().unwrap() {
            assert!(
                body["result"].get(field.as_str().unwrap()).is_some(),
                "dry run result is missing {field}"
            );
        }
        assert!(
            kg.query_sparql_for_claims(
                &claims,
                &format!(
                    "SELECT ?o WHERE {{ ?o a <{}> }}",
                    ev_term_iri("RepairOrder")
                ),
            )
            .unwrap()
            .is_empty(),
            "dry run must not commit a repair order"
        );
        assert!(
            serde_json::from_value::<ActionInvokeRequest>(fixture["guardrail"]["request"].clone())
                .is_err(),
            "callers must not override server-owned guardrails"
        );

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn isolation_contract_ontology_actions_are_invisible_cross_tenant() {
        let tmp = std::env::temp_dir().join(format!("agentos_ontinvoke_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state = make_state(&tmp);
        let claims_a =
            crate::isolation::IsolationClaims::from_verified("tenant-a", "repair", "tester")
                .unwrap();
        let claims_b =
            crate::isolation::IsolationClaims::from_verified("tenant-b", "repair", "tester")
                .unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        let seed = format!(
            "<{}> a <{}> . <{}> a <{}> .",
            ev_instance_iri("Vehicle", "LVIN123"),
            ev_term_iri("Vehicle"),
            ev_instance_iri("FaultCode", "P0A80"),
            ev_term_iri("FaultCode"),
        );
        kg.update_for_claims(&claims_a, &ClaimsGraphUpdate::insert_data(seed))
            .unwrap();

        let app = Router::new()
            .route(
                "/api/v1/ontology/actions/:id/invoke",
                post(invoke_action_handler),
            )
            .with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/actions/GenerateRepairOrder/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", test_jwt("tenant-a")))
            .body(axum::body::Body::from(
                json!({"target": "P0A80", "params": {"vehicle_vin": "LVIN123"}}).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let query = format!(
            "SELECT ?o WHERE {{ ?o a <{}> }}",
            ev_term_iri("RepairOrder")
        );
        assert!(!kg
            .query_sparql_for_claims(&claims_a, &query)
            .unwrap()
            .is_empty());
        assert!(kg
            .query_sparql_for_claims(&claims_b, &query)
            .unwrap()
            .is_empty());

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn action_approval_keeps_staging_until_same_scope_approves() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_approval_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state = make_state(&tmp);
        let claims_a =
            crate::isolation::IsolationClaims::from_verified("tenant-a", "repair", "tester")
                .unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        let seed = format!(
            "<{}> a <{}> . <{}> a <{}> .",
            ev_instance_iri("Vehicle", "LVIN123"),
            ev_term_iri("Vehicle"),
            ev_instance_iri("FaultCode", "P0A80"),
            ev_term_iri("FaultCode"),
        );
        kg.update_for_claims(&claims_a, &ClaimsGraphUpdate::insert_data(seed))
            .unwrap();

        let mut audit_events = state.core.events.subscribe();
        let app = Router::new()
            .route(
                "/api/v1/ontology/actions/:id/invoke",
                post(invoke_action_handler),
            )
            .route(
                "/api/v1/ontology/action-approvals",
                get(list_action_approvals_handler),
            )
            .route(
                "/api/v1/ontology/action-approvals/:approval_id/approve",
                post(approve_action_approval_handler),
            )
            .route(
                "/api/v1/ontology/action-approvals/:approval_id/reject",
                post(reject_action_approval_handler),
            )
            .with_state(state);
        let post = |uri: String, token: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };

        let unauthenticated = axum::http::Request::builder()
            .uri("/api/v1/ontology/action-approvals")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let unauthenticated_invoke = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/actions/GenerateRepairOrder/invoke")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({
                    "target": "P0A80",
                    "params": {"vehicle_vin": "LVIN123"}
                })
                .to_string(),
            ))
            .unwrap();
        assert_eq!(
            app.clone()
                .oneshot(unauthenticated_invoke)
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(
            matches!(
                audit_events.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "requests without verified claims must not forge an audit tenant"
        );

        let auto_commit = app
            .clone()
            .oneshot(post(
                "/api/v1/ontology/actions/GenerateRepairOrder/invoke".to_string(),
                &test_jwt("tenant-a"),
                json!({
                    "target": "P0A80",
                    "params": {"vehicle_vin": "LVIN123"}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(auto_commit.status(), StatusCode::OK);
        let committed = audit_events.recv().await.unwrap();
        assert_eq!(committed.event_type, ACTION_AUDIT_EVENT);
        let committed: Value = serde_json::from_str(&committed.payload).unwrap();
        assert_eq!(committed["tenant_id"], "tenant-a");
        assert_eq!(committed["project_id"], "repair");
        assert_eq!(committed["actor_id"], "ontology-tester");
        assert_eq!(committed["action_id"], "GenerateRepairOrder");
        assert_eq!(committed["decision"], "committed");
        assert!(committed["staging_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));
        assert_eq!(committed["violations"], json!([]));
        assert!(committed["timestamp"].as_str().is_some());

        let invoke = post(
            "/api/v1/ontology/actions/GenerateRepairOrder/invoke".to_string(),
            &test_jwt("tenant-a"),
            json!({
                "target": "P0A80",
                "params": {"vehicle_vin": "LVIN123"},
                "commit_strategy": "require_approval"
            }),
        );
        let response = app.clone().oneshot(invoke).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "pending_approval");
        let approval_id = body["sandbox"]["approval_id"].as_str().unwrap().to_string();
        let pending: Value =
            serde_json::from_str(&audit_events.recv().await.unwrap().payload).unwrap();
        assert_eq!(pending["decision"], "pending");
        assert_eq!(pending["staging_id"], approval_id);

        let orders = format!(
            "SELECT ?o WHERE {{ ?o a <{}> }}",
            ev_term_iri("RepairOrder")
        );
        assert_eq!(
            kg.query_sparql_for_claims(&claims_a, &orders)
                .unwrap()
                .len(),
            1,
            "require_approval must not alter the production graph"
        );

        let list = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/action-approvals")
                    .header("authorization", format!("Bearer {}", test_jwt("tenant-a")))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let list_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(list.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(list_body["approvals"].as_array().unwrap().len(), 1);

        let cross_tenant = app
            .clone()
            .oneshot(post(
                format!("/api/v1/ontology/action-approvals/{approval_id}/approve"),
                &test_jwt("tenant-b"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);

        let approved = app
            .clone()
            .oneshot(post(
                format!("/api/v1/ontology/action-approvals/{approval_id}/approve"),
                &test_jwt("tenant-a"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(approved.status(), StatusCode::OK);
        let approved: Value =
            serde_json::from_str(&audit_events.recv().await.unwrap().payload).unwrap();
        assert_eq!(approved["decision"], "approved");
        assert_eq!(approved["staging_id"], approval_id);
        assert!(
            !kg.query_sparql_for_claims(&claims_a, &orders)
                .unwrap()
                .is_empty(),
            "approval must merge the retained staging graph"
        );

        let reject_invoke = post(
            "/api/v1/ontology/actions/GenerateRepairOrder/invoke".to_string(),
            &test_jwt("tenant-a"),
            json!({
                "target": "P0A80",
                "params": {"vehicle_vin": "LVIN123"},
                "commit_strategy": "require_approval"
            }),
        );
        let response = app.clone().oneshot(reject_invoke).await.unwrap();
        let reject_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let reject_id = reject_body["sandbox"]["approval_id"].as_str().unwrap();
        let pending_reject: Value =
            serde_json::from_str(&audit_events.recv().await.unwrap().payload).unwrap();
        assert_eq!(pending_reject["decision"], "pending");
        assert_eq!(pending_reject["staging_id"], reject_id);
        let rejected = app
            .clone()
            .oneshot(post(
                format!("/api/v1/ontology/action-approvals/{reject_id}/reject"),
                &test_jwt("tenant-a"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::OK);
        let rejected: Value =
            serde_json::from_str(&audit_events.recv().await.unwrap().payload).unwrap();
        assert_eq!(rejected["decision"], "rejected");
        assert_eq!(rejected["staging_id"], reject_id);
        assert_eq!(
            kg.query_sparql_for_claims(&claims_a, &orders)
                .unwrap()
                .len(),
            2,
            "reject must discard staged writes"
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}

/// 动力层执行器（ActionType invoke）单测：参数/前置条件校验 + SPARQL 组装。
#[cfg(test)]
mod ontology_action_tests {
    use super::*;
    use crate::isolation::IsolationClaims;
    use crate::knowledge_graph::store::KnowledgeGraphStore;
    use oxigraph::store::Store;

    #[test]
    fn entity_resolution_matcher_is_exact_after_normalization() {
        assert_eq!(normalized_entity_text("ACME, Inc."), "acmeinc");
        assert_eq!(normalized_entity_text("ＡＣＭＥ"), "ａｃｍｅ");
        assert_ne!(
            normalized_entity_text("Acme Incorporated"),
            normalized_entity_text("Acme Inc.")
        );
    }

    fn test_claims(tenant: &str) -> IsolationClaims {
        IsolationClaims::from_verified(tenant, "repair", "tester").unwrap()
    }

    /// 预置车辆/故障码/电池/车型实例于调用方的 claims 图。
    fn seeded_kg(claims: &IsolationClaims) -> KnowledgeGraphStore {
        let store = Arc::new(Store::new().unwrap());
        let seed = format!(
            "\
             <{veh}> a <{vehc}> . \
             <{fault}> a <{faultc}> . \
             <{bat}> a <{batc}> . \
             <{model}> a <{modelc}> . \
             ",
            veh = ev_instance_iri("Vehicle", "LVIN123"),
            vehc = ev_term_iri("Vehicle"),
            fault = ev_instance_iri("FaultCode", "P0A80"),
            faultc = ev_term_iri("FaultCode"),
            bat = ev_instance_iri("Battery", "BAT-001"),
            batc = ev_term_iri("Battery"),
            model = ev_instance_iri("VehicleModel", "M-001"),
            modelc = ev_term_iri("VehicleModel"),
        );
        let kg = KnowledgeGraphStore::with_shared_store(store).unwrap();
        kg.update_for_claims(claims, &ClaimsGraphUpdate::insert_data(seed))
            .unwrap();
        kg
    }

    fn mk_req(target: Option<&str>, params: Value, dry_run: bool) -> ActionInvokeRequest {
        ActionInvokeRequest {
            target: target.map(|s| s.to_string()),
            params: params.as_object().cloned().unwrap_or_default(),
            dry_run,
            commit_strategy: ActionCommitStrategy::Auto,
        }
    }

    #[test]
    fn test_iri_safe_and_instance_iri() {
        assert_eq!(iri_safe("P0A80"), "P0A80");
        assert_eq!(iri_safe("a b"), "a_b");
        assert_eq!(
            ev_instance_iri("Vehicle", "X 1"),
            "https://agentos.ontology/ev/Vehicle/X_1"
        );
        assert_eq!(ev_prop_iri("soh"), "https://agentos.ontology/ev/prop/soh");
    }

    #[test]
    fn test_generate_repair_order_ok() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            Some("P0A80"),
            json!({"vehicle_vin": "LVIN123", "assigned_to": "张工", "estimated_cost": 1200}),
            false,
        );
        let (stmts, meta) = build_action_effects(
            "GenerateRepairOrder",
            &r,
            &kg,
            &claims,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert_eq!(stmts.len(), 1);
        let s = &stmts[0];
        assert!(s.sparql().contains("RepairOrder"));
        assert!(s.sparql().contains("forVehicle"));
        assert!(s.sparql().contains("diagnoses"));
        assert!(s.sparql().contains("张工"));
        assert!(s.sparql().contains("1200"));
        assert!(meta["order_id"].as_str().unwrap().starts_with("RO-"));
    }

    #[test]
    fn test_generate_repair_order_missing_vehicle_precondition() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(Some("P0A80"), json!({"vehicle_vin": "UNKNOWN"}), false);
        let err = build_action_effects("GenerateRepairOrder", &r, &kg, &claims, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("车辆VIN不存在"));
    }

    #[test]
    fn test_generate_repair_order_missing_target() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(None, json!({"vehicle_vin": "LVIN123"}), false);
        let err = build_action_effects("GenerateRepairOrder", &r, &kg, &claims, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    // ── 数据沙箱（staging-graph）单测 ──────────────────────────────────

    #[test]
    fn test_effects_do_not_select_a_graph() {
        let update = ClaimsGraphUpdate::insert_data("<a> <b> <c>");
        assert!(!update.sparql().to_uppercase().contains("GRAPH"));
    }

    /// 合法写回：经影子图护栏通过 → 合并到生产图；影子图删除、生产图可见新数据。
    #[test]
    fn test_sandbox_commit_merges_to_production() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(Some("P0A80"), json!({"vehicle_vin": "LVIN123"}), false);
        let (stmts, _meta) = build_action_effects(
            "GenerateRepairOrder",
            &r,
            &kg,
            &claims,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        let guardrails = ontology_guardrails::effective_config(&ont, action).unwrap();
        let outcome = commit_via_staging(
            &kg,
            &claims,
            &stmts,
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .expect("护栏应通过并提交");
        let StagingCommitOutcome::Committed(report) = outcome else {
            panic!("auto 策略必须直接提交");
        };
        assert_eq!(report["guardrails_passed"], json!(true));

        // claims 图应能查到新建的维修工单类型三元组。
        let q = format!(
            "SELECT ?o WHERE {{ ?o a <{}> }}",
            ev_term_iri("RepairOrder")
        );
        let rows = kg.query_sparql_for_claims(&claims, &q).unwrap();
        assert!(!rows.is_empty(), "生产图应可见已提交的维修工单");
        let tenant_b = test_claims("tenant-b");
        assert!(
            kg.query_sparql_for_claims(&tenant_b, &q)
                .unwrap()
                .is_empty(),
            "tenant B must not see tenant A invoke writes"
        );

        // 影子图应已删除（无残留）。
        let staging_id = report["staging_graph"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap();
        let sq = "SELECT ?s WHERE { ?s ?p ?o }";
        assert!(
            kg.query_staging_for_claims(&claims, staging_id, sq)
                .unwrap()
                .is_empty(),
            "影子图应已清理"
        );
    }

    /// 越权谓词：护栏应拦截并回滚（返回 422），生产图零改动。
    #[test]
    fn test_sandbox_rollback_on_foreign_predicate() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        // 统计 claims 图当前三元组数（回滚后应不变）。
        let count_q = "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }";
        let before = kg.query_sparql_for_claims(&claims, count_q).unwrap()[0]["?c"]
            .as_str()
            .unwrap()
            .to_string();

        // 构造带越权谓词（不在白名单命名空间）的语句。
        let foreign = ClaimsGraphUpdate::insert_data(
            "<https://agentos.ontology/ev/X/1> <http://evil.example/pwn> \"x\"",
        );
        let ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        let guardrails = ontology_guardrails::effective_config(&ont, action).unwrap();
        let err = commit_via_staging(
            &kg,
            &claims,
            &[foreign],
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err.2.iter().any(|v| v.contains("越权谓词")));

        // 生产图三元组数不变（已回滚）。
        let after = kg.query_sparql_for_claims(&claims, count_q).unwrap()[0]["?c"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(before, after, "回滚后生产图不应有任何改动");
    }

    #[test]
    fn test_action_whitelist_override_rejects_otherwise_allowed_predicate() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let mut ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter_mut()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        action.guardrails.allowed_predicate_prefixes =
            Some(vec!["https://agentos.ontology/ev/prop/".into()]);
        let action = action.clone();
        let guardrails = ontology_guardrails::effective_config(&ont, &action).unwrap();
        let permitted_by_default = ClaimsGraphUpdate::insert_data(
            "<https://agentos.ontology/ev/X/1> <https://agentos.ontology/ev/custom> \"x\"",
        );
        let err = commit_via_staging(
            &kg,
            &claims,
            &[permitted_by_default],
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err
            .2
            .iter()
            .any(|violation| violation.starts_with("predicate_whitelist:")));
    }

    #[test]
    fn test_assertion_failure_rolls_back_staging_graph() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let before = kg
            .query_sparql_for_claims(&claims, "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }")
            .unwrap()[0]["?c"]
            .clone();
        let mut ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter_mut()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        action.guardrails.assertions.push(
            crate::knowledge_graph::ontology_layer::SparqlAskAssertion {
                code: "no_staging_writes".into(),
                query: "ASK { ?s ?p ?o }".into(),
            },
        );
        let action = action.clone();
        let guardrails = ontology_guardrails::effective_config(&ont, &action).unwrap();
        let write = ClaimsGraphUpdate::insert_data(
            "<https://agentos.ontology/ev/X/1> <https://agentos.ontology/ev/prop/value> \"x\"",
        );
        let err = commit_via_staging(
            &kg,
            &claims,
            &[write],
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err
            .2
            .iter()
            .any(|violation| violation.starts_with("assertion:no_staging_writes:")));
        let after = kg
            .query_sparql_for_claims(&claims, "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }")
            .unwrap()[0]["?c"]
            .clone();
        assert_eq!(before, after, "断言失败后生产图不应有任何改动");
    }

    #[test]
    fn test_invoke_payload_cannot_override_guardrails() {
        assert!(serde_json::from_value::<ActionInvokeRequest>(json!({
            "dry_run": true,
            "guardrails": { "max_triples": 0 }
        }))
        .is_err());
    }

    #[test]
    fn test_update_battery_soh_ok_and_range() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let ok = mk_req(None, json!({"battery_id": "BAT-001", "soh": 87.5}), false);
        let (stmts, meta) =
            build_action_effects("UpdateBatterySoh", &ok, &kg, &claims, "t").unwrap();
        assert_eq!(stmts.len(), 4); // soh upsert(2) + soh_updated_at upsert(2)
        assert!(stmts.iter().any(|s| s.sparql().contains("DELETE WHERE")));
        assert!(stmts.iter().any(|s| s.sparql().contains("87.5")));
        assert_eq!(meta["soh"], 87.5);

        let bad = mk_req(None, json!({"battery_id": "BAT-001", "soh": 150}), false);
        let err = build_action_effects("UpdateBatterySoh", &bad, &kg, &claims, "t").unwrap_err();
        assert!(err.1.contains("0-100"));
    }

    #[test]
    fn test_update_battery_soh_missing_battery() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(None, json!({"battery_id": "NOPE", "soh": 50}), false);
        let err = build_action_effects("UpdateBatterySoh", &r, &kg, &claims, "t").unwrap_err();
        assert!(err.1.contains("电池对象不存在"));
    }

    #[test]
    fn test_mark_recall_ok() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            None,
            json!({"model_id": "M-001", "recall_reason": "电池批次缺陷"}),
            false,
        );
        let (stmts, meta) = build_action_effects("MarkRecall", &r, &kg, &claims, "t").unwrap();
        assert_eq!(stmts.len(), 6); // 三个属性各 upsert(2)
        assert!(stmts.iter().any(|s| s.sparql().contains("recalled")));
        assert!(stmts.iter().any(|s| s.sparql().contains("电池批次缺陷")));
        assert_eq!(meta["recalled"], true);
    }

    #[test]
    fn test_append_faq_ok_and_links_fault() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            Some("P0A80"),
            json!({"question": "报警怎么办？", "answer": "请尽快检修"}),
            false,
        );
        let (stmts, meta) = build_action_effects("AppendFaq", &r, &kg, &claims, "t").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].sparql().contains("relatedFaq"));
        assert!(stmts[0].sparql().contains("报警怎么办"));
        assert!(meta["faq_id"].as_str().unwrap().starts_with("FAQ-"));
    }

    #[test]
    fn test_append_faq_missing_fault_precondition() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            Some("NON_EXIST"),
            json!({"question": "q", "answer": "a"}),
            false,
        );
        let err = build_action_effects("AppendFaq", &r, &kg, &claims, "t").unwrap_err();
        assert!(err.1.contains("故障码对象不存在"));
    }

    #[test]
    fn test_unknown_action() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(None, json!({}), false);
        let err = build_action_effects("NoSuchAction", &r, &kg, &claims, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }
}
