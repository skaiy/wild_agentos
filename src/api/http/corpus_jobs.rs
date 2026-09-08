//! Claims-scoped persistence and HTTP contracts for online corpus jobs.
//!
//! This module deliberately stores orchestration metadata only. A job never
//! selects a graph and no transition writes production data. Future runners
//! must use [`transition_job_for_claims`] and the existing staged extraction
//! and human-confirmed materialization APIs.

use std::{path::PathBuf, sync::Arc};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    isolation::IsolationClaims,
    knowledge_graph::{
        canonicalizer::{canonicalize, CanonicalizationStatus},
        ontology_store::OntologyStore,
        quality_gate::{KgQualityGate, QualityGateRequest},
        rdf_mapper::RdfMapper,
        store::{ClaimsGraphUpdate, KnowledgeGraphStore, PendingExtractionReview},
        types::LLMExtractionOutput,
    },
};

use super::{
    iam::UserIdentity,
    ontology::{
        create_entity_resolution_suggestion_from_staging, persist_quality_gate_report,
        ConstrainedExtractionRequest, ConstrainedExtractionSource,
    },
    AppState,
};

const MAX_ERROR_METADATA_BYTES: usize = 1024;

pub(crate) type OnlineCorpusJobStore = Arc<tokio::sync::RwLock<Vec<OnlineCorpusJob>>>;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CorpusSource {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateOnlineCorpusJobRequest {
    pub source: CorpusSource,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnlineCorpusJobState {
    Queued,
    Running,
    AwaitingReview,
    Succeeded,
    Failed,
    Cancelled,
}

impl OnlineCorpusJobState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    fn permits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::AwaitingReview | Self::Succeeded | Self::Failed | Self::Cancelled
                )
                | (
                    Self::AwaitingReview,
                    Self::Succeeded | Self::Failed | Self::Cancelled
                )
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct CorpusJobAttemptSummary {
    pub attempts: u32,
    #[serde(default)]
    pub latest_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct CorpusJobLinks {
    #[serde(default)]
    pub staged_extraction_ids: Vec<String>,
    #[serde(default)]
    pub review_ids: Vec<String>,
    #[serde(default)]
    pub entity_resolution_suggestion_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct OnlineCorpusJob {
    pub id: String,
    pub source: CorpusSource,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub request_id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub actor_id: String,
    pub state: OnlineCorpusJobState,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    pub attempt_summary: CorpusJobAttemptSummary,
    pub links: CorpusJobLinks,
}

/// The runner accepts upstream extraction candidates but performs the trusted
/// canonicalization and staging itself. It does not select a graph, promote an
/// ontology type, or expose materialization.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunOnlineCorpusJobRequest {
    pub text: String,
    pub extractor: String,
    #[serde(default)]
    pub model: Option<String>,
    pub candidates: LLMExtractionOutput,
    pub quality_gate: QualityGateRequest,
}

pub(crate) fn load_online_corpus_jobs() -> Vec<OnlineCorpusJob> {
    std::fs::read_to_string(online_corpus_jobs_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn online_corpus_jobs_path() -> PathBuf {
    super::data_dir().join("online_corpus_jobs.json")
}

fn save_online_corpus_jobs(jobs: &[OnlineCorpusJob]) -> Result<(), String> {
    let path = online_corpus_jobs_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let serialized = serde_json::to_vec_pretty(jobs).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

fn claims_or_unauthorized(identity: &UserIdentity) -> Result<&IsolationClaims, Response> {
    identity.isolation_claims().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified JWT isolation claims are required"})),
        )
            .into_response()
    })
}

fn job_is_in_scope(job: &OnlineCorpusJob, claims: &IsolationClaims) -> bool {
    job.tenant_id == claims.tenant_id() && job.project_id == claims.project_id()
}

fn valid_nonempty(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} is required"))
    } else if value.len() > 512 {
        Err(format!("{field} exceeds 512 characters"))
    } else {
        Ok(())
    }
}

fn validate_create_request(request: &CreateOnlineCorpusJobRequest) -> Result<(), String> {
    valid_nonempty(&request.source.id, "source.id")?;
    valid_nonempty(&request.source.version, "source.version")?;
    if request
        .source
        .uri
        .as_ref()
        .is_some_and(|uri| uri.len() > 2048)
    {
        return Err("source.uri exceeds 2048 characters".to_string());
    }
    if let Some(key) = &request.idempotency_key {
        valid_nonempty(key, "idempotency_key")?;
    }
    Ok(())
}

fn bounded_error_metadata(error: &str) -> String {
    if error.len() <= MAX_ERROR_METADATA_BYTES {
        return error.to_owned();
    }
    let mut end = MAX_ERROR_METADATA_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &error[..end])
}

fn public_job(job: &OnlineCorpusJob, reused: bool) -> Value {
    json!({
        "job": job,
        "reused": reused,
        "production_write": false,
        "materialization_status": "not_materialized",
    })
}

/// POST /api/v1/online-corpus-jobs
///
/// Creates queued orchestration metadata only. Idempotency is scoped to the
/// verified tenant/project and conflicts if a key is reused for another source.
pub(crate) async fn create_online_corpus_job_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<CreateOnlineCorpusJobRequest>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    if let Err(error) = validate_create_request(&request) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
    }

    let mut jobs = state.online_corpus_jobs.write().await;
    if let Some(key) = request.idempotency_key.as_deref() {
        if let Some(existing) = jobs
            .iter()
            .find(|job| job_is_in_scope(job, claims) && job.idempotency_key.as_deref() == Some(key))
        {
            if existing.source == request.source {
                return (StatusCode::OK, Json(public_job(existing, true))).into_response();
            }
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "idempotency_key is already bound to another source in this scope"})),
            )
                .into_response();
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let job = OnlineCorpusJob {
        id: uuid::Uuid::new_v4().simple().to_string(),
        source: request.source,
        idempotency_key: request.idempotency_key,
        request_id: uuid::Uuid::new_v4().simple().to_string(),
        tenant_id: claims.tenant_id().to_owned(),
        project_id: claims.project_id().to_owned(),
        actor_id: claims.actor_id().to_owned(),
        state: OnlineCorpusJobState::Queued,
        created_at: now.clone(),
        updated_at: now,
        started_at: None,
        completed_at: None,
        attempt_summary: CorpusJobAttemptSummary::default(),
        links: CorpusJobLinks::default(),
    };
    let mut next = jobs.clone();
    next.push(job.clone());
    if let Err(error) = save_online_corpus_jobs(&next) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("persist online corpus job: {error}")})),
        )
            .into_response();
    }
    *jobs = next;
    (StatusCode::CREATED, Json(public_job(&job, false))).into_response()
}

/// GET /api/v1/online-corpus-jobs
pub(crate) async fn list_online_corpus_jobs_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let jobs = state.online_corpus_jobs.read().await;
    let jobs: Vec<_> = jobs
        .iter()
        .filter(|job| job_is_in_scope(job, claims))
        .collect();
    Json(json!({"count": jobs.len(), "jobs": jobs, "production_write": false})).into_response()
}

/// GET /api/v1/online-corpus-jobs/:id
pub(crate) async fn get_online_corpus_job_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let jobs = state.online_corpus_jobs.read().await;
    match jobs
        .iter()
        .find(|job| job.id == job_id && job_is_in_scope(job, claims))
    {
        Some(job) => Json(public_job(job, false)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "online corpus job not found"})),
        )
            .into_response(),
    }
}

/// POST /api/v1/online-corpus-jobs/:id/cancel
pub(crate) async fn cancel_online_corpus_job_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    match transition_job_for_claims(
        &state.online_corpus_jobs,
        claims,
        &job_id,
        OnlineCorpusJobState::Cancelled,
        None,
    )
    .await
    {
        Ok(job) => Json(public_job(&job, false)).into_response(),
        Err(JobTransitionError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "online corpus job not found"})),
        )
            .into_response(),
        Err(JobTransitionError::IllegalTransition) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "online corpus job is already terminal or cannot be cancelled"})),
        )
            .into_response(),
        Err(JobTransitionError::Persistence(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("persist online corpus job: {error}")})),
        )
            .into_response(),
    }
}

/// POST /api/v1/online-corpus-jobs/:id/run
///
/// Drives exactly one claims-scoped source version through the established KE
/// primitives. Completion means that staged evidence and approval-held review
/// work exist; it deliberately never materializes staging into production.
pub(crate) async fn run_online_corpus_job_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(job_id): Path<String>,
    Json(request): Json<RunOnlineCorpusJobRequest>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    if request.text.trim().is_empty() || request.extractor.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "text and extractor are required"})),
        )
            .into_response();
    }
    let job = {
        let jobs = state.online_corpus_jobs.read().await;
        match jobs
            .iter()
            .find(|job| job.id == job_id && job_is_in_scope(job, claims))
        {
            Some(job) => job.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "online corpus job not found"})),
                )
                    .into_response()
            }
        }
    };
    if job.state == OnlineCorpusJobState::AwaitingReview
        && !job.links.review_ids.is_empty()
        && !job.links.entity_resolution_suggestion_ids.is_empty()
    {
        return Json(json!({"job": job, "reused": true, "production_write": false, "materialization_status": "not_materialized"})).into_response();
    }
    if job.state.is_terminal() || matches!(job.state, OnlineCorpusJobState::AwaitingReview) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "online corpus job cannot be run from its current state"})),
        )
            .into_response();
    }
    if job.state == OnlineCorpusJobState::Queued {
        if let Err(error) = transition_job_for_claims(
            &state.online_corpus_jobs,
            claims,
            &job_id,
            OnlineCorpusJobState::Running,
            None,
        )
        .await
        {
            return job_transition_response(error);
        }
    }

    let execution = run_job_ke_primitives(&state, claims, &job, &request).await;
    match execution {
        Ok(links) => {
            if let Err(error) =
                record_job_links_for_claims(&state.online_corpus_jobs, claims, &job_id, links).await
            {
                return job_transition_response(error);
            }
            match transition_job_for_claims(
                &state.online_corpus_jobs,
                claims,
                &job_id,
                OnlineCorpusJobState::AwaitingReview,
                None,
            )
            .await
            {
                Ok(job) => Json(json!({
                    "job": job, "reused": false, "production_write": false,
                    "materialization_status": "not_materialized",
                    "status": "awaiting_review",
                }))
                .into_response(),
                Err(error) => job_transition_response(error),
            }
        }
        Err(error) => match transition_job_for_claims(
            &state.online_corpus_jobs,
            claims,
            &job_id,
            OnlineCorpusJobState::Failed,
            Some(&error),
        )
        .await
        {
            Ok(job) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "job": job, "error": error, "production_write": false,
                    "materialization_status": "not_materialized",
                })),
            )
                .into_response(),
            Err(transition_error) => job_transition_response(transition_error),
        },
    }
}

async fn run_job_ke_primitives(
    state: &AppState,
    claims: &IsolationClaims,
    job: &OnlineCorpusJob,
    request: &RunOnlineCorpusJobRequest,
) -> Result<CorpusJobLinks, String> {
    let extraction_id = job.id.clone();
    let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone())?;
    let ontology_store = OntologyStore::with_shared_store(state.kg_store.clone())?;
    ontology_store.ensure_seeded("ev-repair")?;
    let ontology = ontology_store.load_definition("ev-repair")?;
    let canonical = canonicalize(&request.candidates, &ontology);

    let staging_graph = kg.staging_graph_iri_for_claims(claims, &extraction_id)?;
    let mapped = RdfMapper::map_extraction(&canonical.extraction, &staging_graph);
    let decisions = serde_json::to_string(&canonical.decisions)
        .map_err(|error| format!("serialize canonicalization provenance: {error}"))?;
    let provenance = format!(
        "<https://agentos.ontology/extraction/run/{extraction_id}> \
         <https://agentos.ontology/extraction/blobId> \"{}\" ; \
         <https://agentos.ontology/extraction/blobVersion> \"{}\" ; \
         <https://agentos.ontology/extraction/extractor> \"{}\" ; \
         <https://agentos.ontology/extraction/sourceText> \"{}\" ; \
         <https://agentos.ontology/extraction/canonicalizationDecisions> \"{}\" .",
        sparql_literal(&job.source.id),
        sparql_literal(&job.source.version),
        sparql_literal(&request.extractor),
        sparql_literal(&request.text),
        sparql_literal(&decisions),
    );
    let mut triples = RdfMapper::quads_to_sparql_triples(&mapped.quads);
    if !triples.is_empty() {
        triples.push('\n');
    }
    triples.push_str(&provenance);
    kg.update_staging_for_claims(
        claims,
        &extraction_id,
        &ClaimsGraphUpdate::insert_data(triples),
    )?;

    let existing_review = kg
        .list_extraction_reviews_for_claims(claims)?
        .into_iter()
        .find(|review| review.extraction_id == extraction_id);
    let (review, gate_passed) = match existing_review {
        Some(review) => {
            let report = serde_json::from_str::<
                crate::knowledge_graph::quality_gate::QualityGateReport,
            >(&review.report_json)
            .map_err(|error| format!("stored quality gate report is invalid: {error}"))?;
            (review, report.passed)
        }
        None => {
            let report =
                KgQualityGate::evaluate(&kg, claims, &extraction_id, &request.quality_gate, None)?;
            persist_quality_gate_report(&kg, claims, &report)?;
            let review = PendingExtractionReview {
                review_id: uuid::Uuid::new_v4().simple().to_string(),
                extraction_id: extraction_id.clone(),
                staging_graph,
                gate_status: report.review_status.clone(),
                report_json: serde_json::to_string(&report)
                    .map_err(|error| format!("serialize gate report: {error}"))?,
                created_at: chrono::Utc::now().to_rfc3339(),
                decision: "pending".into(),
            };
            kg.create_extraction_review_for_claims(claims, &review)?;
            (review, report.passed)
        }
    };

    let mut suggestion_ids = Vec::new();
    for node in &canonical.extraction.nodes {
        let source_iri = format!("iri://entity/{}", RdfMapper::sanitize_id(&node.id));
        let existing = kg
            .list_action_approvals_for_claims(claims)?
            .into_iter()
            .find(|approval| {
                approval.action_id == "entity-resolution"
                    && approval
                        .anchor_query
                        .as_deref()
                        .is_some_and(|query| query.contains(&source_iri))
            })
            .map(|approval| approval.approval_id);
        let suggestion = match existing {
            Some(approval_id) => approval_id,
            None => create_entity_resolution_suggestion_from_staging(
                &kg,
                claims,
                &extraction_id,
                &source_iri,
                &node.label,
            )?
            .ok_or_else(|| format!("entity resolution uncertain for staged mention {}", node.id))?,
        };
        suggestion_ids.push(suggestion);
    }
    if suggestion_ids.is_empty() {
        return Err(
            "canonicalization produced no eligible entities for required entity resolution".into(),
        );
    }
    if !gate_passed {
        return Err("quality gate blocked the staged extraction".into());
    }
    if canonical
        .decisions
        .iter()
        .any(|d| d.status == CanonicalizationStatus::NeedsReview)
    {
        return Err("canonicalization ambiguity requires review".into());
    }
    let _ = state.kg_store.flush();
    Ok(CorpusJobLinks {
        staged_extraction_ids: vec![extraction_id],
        review_ids: vec![review.review_id],
        entity_resolution_suggestion_ids: suggestion_ids,
    })
}

fn sparql_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

async fn record_job_links_for_claims(
    store: &OnlineCorpusJobStore,
    claims: &IsolationClaims,
    job_id: &str,
    links: CorpusJobLinks,
) -> Result<(), JobTransitionError> {
    let mut jobs = store.write().await;
    let Some(index) = jobs
        .iter()
        .position(|job| job.id == job_id && job_is_in_scope(job, claims))
    else {
        return Err(JobTransitionError::NotFound);
    };
    let mut next = jobs.clone();
    next[index].links = links;
    next[index].updated_at = chrono::Utc::now().to_rfc3339();
    save_online_corpus_jobs(&next).map_err(JobTransitionError::Persistence)?;
    *jobs = next;
    Ok(())
}

fn job_transition_response(error: JobTransitionError) -> Response {
    match error {
        JobTransitionError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "online corpus job not found"})),
        )
            .into_response(),
        JobTransitionError::IllegalTransition => (
            StatusCode::CONFLICT,
            Json(json!({"error": "online corpus job cannot transition to requested state"})),
        )
            .into_response(),
        JobTransitionError::Persistence(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("persist online corpus job: {error}")})),
        )
            .into_response(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JobTransitionError {
    NotFound,
    IllegalTransition,
    Persistence(String),
}

/// Claims-scoped transition entry point for later runner and review code.
///
/// Completion is only job execution completion: callers must separately invoke
/// the existing explicit human-confirmed materialization flow.
pub(crate) async fn transition_job_for_claims(
    store: &OnlineCorpusJobStore,
    claims: &IsolationClaims,
    job_id: &str,
    next_state: OnlineCorpusJobState,
    latest_error: Option<&str>,
) -> Result<OnlineCorpusJob, JobTransitionError> {
    let mut jobs = store.write().await;
    let Some(index) = jobs
        .iter()
        .position(|job| job.id == job_id && job_is_in_scope(job, claims))
    else {
        return Err(JobTransitionError::NotFound);
    };
    let current = jobs[index].state;
    if current.is_terminal() || !current.permits(next_state) {
        return Err(JobTransitionError::IllegalTransition);
    }
    let mut next = jobs.clone();
    let now = chrono::Utc::now().to_rfc3339();
    {
        let job = &mut next[index];
        job.state = next_state;
        job.updated_at = now.clone();
        if next_state == OnlineCorpusJobState::Running {
            job.started_at = Some(now.clone());
            job.attempt_summary.attempts = job.attempt_summary.attempts.saturating_add(1);
        }
        if next_state.is_terminal() {
            job.completed_at = Some(now);
        }
        job.attempt_summary.latest_error = latest_error.map(bounded_error_metadata);
    }
    let updated = next[index].clone();
    save_online_corpus_jobs(&next).map_err(JobTransitionError::Persistence)?;
    *jobs = next;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        routing::{get, post},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use tower::ServiceExt;

    use crate::{
        api::http::{api_gov::ApiUsageState, iam::JwtClaims, AppState, TEST_ENV_LOCK},
        core::core_types::{CoreConfig, SemanticCore},
        gateway::unified_gateway::UnifiedGateway,
        tools::prompt_registry::PromptRegistry,
    };

    fn test_state() -> Arc<AppState> {
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: tempfile::tempdir().unwrap().path().display().to_string(),
                event_buffer_size: 10,
                enable_metrics: false,
                eviction_config: None,
            })
            .unwrap(),
        );
        let gateway = Arc::new(
            UnifiedGateway::new(&crate::config::GatewaySettings {
                base_url: "http://localhost".into(),
                api_key: String::new(),
                default_model: "test".into(),
                timeout_seconds: 1,
                max_retries: 1,
                retry_base_ms: 1,
                use_responses_api: false,
                model_mapping: Default::default(),
            })
            .unwrap(),
        );
        Arc::new(AppState {
            core,
            gateway,
            kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
            config_info: Arc::new(tokio::sync::RwLock::new(json!({}))),
            agents_info: json!({}),
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
        })
    }

    fn router(state: Arc<AppState>) -> Router {
        Router::new()
            .route(
                "/api/v1/online-corpus-jobs",
                post(create_online_corpus_job_handler).get(list_online_corpus_jobs_handler),
            )
            .route(
                "/api/v1/online-corpus-jobs/:id",
                get(get_online_corpus_job_handler),
            )
            .route(
                "/api/v1/online-corpus-jobs/:id/cancel",
                post(cancel_online_corpus_job_handler),
            )
            .route(
                "/api/v1/online-corpus-jobs/:id/run",
                post(run_online_corpus_job_handler),
            )
            .with_state(state)
    }

    fn token(tenant: &str, project: &str) -> String {
        encode(
            &Header::default(),
            &JwtClaims {
                sub: "service".into(),
                tenant_id: tenant.into(),
                project_id: Some(project.into()),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
        )
        .unwrap()
    }

    async fn request(
        router: &Router,
        method: &str,
        uri: &str,
        body: Value,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn corpus_jobs_fail_closed_and_are_claims_scoped() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        let router = router(test_state());
        let payload =
            json!({"source": {"id": "docs", "version": "v1"}, "idempotency_key": "first"});

        let (status, _) = request(
            &router,
            "POST",
            "/api/v1/online-corpus-jobs",
            payload.clone(),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = request(
            &router,
            "POST",
            "/api/v1/online-corpus-jobs",
            payload.clone(),
            Some("invalid"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            load_online_corpus_jobs().is_empty(),
            "unverified calls must not persist a job"
        );

        let tenant_a = token("tenant-a", "project-a");
        let tenant_b = token("tenant-b", "project-a");
        let (status, created) = request(
            &router,
            "POST",
            "/api/v1/online-corpus-jobs",
            payload.clone(),
            Some(&tenant_a),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = created["job"]["id"].as_str().unwrap();
        let (status, reused) = request(
            &router,
            "POST",
            "/api/v1/online-corpus-jobs",
            payload,
            Some(&tenant_a),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reused["job"]["id"], id);
        assert_eq!(load_online_corpus_jobs().len(), 1);

        let (status, listed) = request(
            &router,
            "GET",
            "/api/v1/online-corpus-jobs",
            json!({}),
            Some(&tenant_b),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["count"], 0);
        let (status, _) = request(
            &router,
            "GET",
            &format!("/api/v1/online-corpus-jobs/{id}"),
            json!({}),
            Some(&tenant_b),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/cancel"),
            json!({}),
            Some(&tenant_b),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, cancelled) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/cancel"),
            json!({}),
            Some(&tenant_a),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled["job"]["state"], "cancelled");
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/cancel"),
            json!({}),
            Some(&tenant_a),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        std::env::remove_var("AGENTOS_DATA_DIR");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
    }

    #[tokio::test]
    async fn state_transitions_are_scoped_persisted_and_bounded() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let other = IsolationClaims::from_verified("tenant-b", "project-a", "actor-b").unwrap();
        let job = OnlineCorpusJob {
            id: "job-1".into(),
            source: CorpusSource {
                id: "corpus".into(),
                version: "v1".into(),
                uri: None,
            },
            idempotency_key: Some("request-1".into()),
            request_id: "request-1".into(),
            tenant_id: claims.tenant_id().into(),
            project_id: claims.project_id().into(),
            actor_id: claims.actor_id().into(),
            state: OnlineCorpusJobState::Queued,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            started_at: None,
            completed_at: None,
            attempt_summary: CorpusJobAttemptSummary::default(),
            links: CorpusJobLinks::default(),
        };
        let store = Arc::new(tokio::sync::RwLock::new(vec![job]));

        assert_eq!(
            transition_job_for_claims(
                &store,
                &other,
                "job-1",
                OnlineCorpusJobState::Cancelled,
                None
            )
            .await,
            Err(JobTransitionError::NotFound)
        );
        let running = transition_job_for_claims(
            &store,
            &claims,
            "job-1",
            OnlineCorpusJobState::Running,
            None,
        )
        .await
        .unwrap();
        assert!(running.started_at.is_some());
        assert_eq!(running.attempt_summary.attempts, 1);
        let failed = transition_job_for_claims(
            &store,
            &claims,
            "job-1",
            OnlineCorpusJobState::Failed,
            Some(&"x".repeat(MAX_ERROR_METADATA_BYTES + 10)),
        )
        .await
        .unwrap();
        assert!(failed.completed_at.is_some());
        assert!(failed.attempt_summary.latest_error.unwrap().len() <= MAX_ERROR_METADATA_BYTES + 3);
        assert_eq!(
            load_online_corpus_jobs()[0].state,
            OnlineCorpusJobState::Failed
        );
        assert_eq!(
            transition_job_for_claims(
                &store,
                &claims,
                "job-1",
                OnlineCorpusJobState::Running,
                None
            )
            .await,
            Err(JobTransitionError::IllegalTransition)
        );
        std::env::remove_var("AGENTOS_DATA_DIR");
    }

    #[tokio::test]
    async fn runner_requires_er_suggestions_and_never_materializes_production() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        let sidecar = temp.path().join("er-sidecar.sh");
        std::fs::write(
            &sidecar,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"target_iri\":\"iri://entity/existing-alice\",\"score\":1.0,\"evidence\":[\"exact\"]}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("AGENTOS_KG_GLINKER_COMMAND", &sidecar);

        let state = test_state();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "service").unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        kg.update_for_claims(
            &claims,
            &ClaimsGraphUpdate::insert_data(
                "<iri://entity/existing-alice> <http://www.w3.org/2000/01/rdf-schema#label> \"Alice\" .",
            ),
        )
        .unwrap();
        let before = kg
            .query_sparql_for_claims(&claims, "SELECT ?s WHERE { ?s ?p ?o }")
            .unwrap()
            .len();
        let router = router(state.clone());
        let jwt = token("tenant-a", "project-a");
        let (status, created) = request(
            &router,
            "POST",
            "/api/v1/online-corpus-jobs",
            json!({"source": {"id": "docs", "version": "v1"}, "idempotency_key": "docs-v1"}),
            Some(&jwt),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = created["job"]["id"].as_str().unwrap();
        let run_payload = json!({
            "text": "Alice is a fault code",
            "extractor": "test-extractor",
            "candidates": {
                "nodes": [{"id": "new-alice", "node_type": "FaultCode", "label": "Alice", "properties": {}}],
                "edges": []
            },
            "quality_gate": {
                "assertions": [{"code": "no_violation", "query": "ASK { FILTER(false) }"}],
                "policy_version": "test/v1",
                "arbitration": "compliance"
            }
        });
        let (status, ran) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/run"),
            run_payload.clone(),
            Some(&jwt),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ran["job"]["state"], "awaiting_review");
        assert_eq!(
            ran["job"]["links"]["entity_resolution_suggestion_ids"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "required ER suggestion must be recorded before a job reaches review"
        );
        assert_eq!(
            kg.list_action_approvals_for_claims(&claims).unwrap().len(),
            1,
            "ER output must remain approval-held"
        );
        assert_eq!(
            kg.query_sparql_for_claims(&claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            before,
            "runner success must not write the production graph"
        );
        let (status, replay) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/run"),
            run_payload,
            Some(&jwt),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replay["reused"], true);
        assert_eq!(
            kg.list_action_approvals_for_claims(&claims).unwrap().len(),
            1,
            "idempotent replay must not create a second ER suggestion"
        );

        std::env::remove_var("AGENTOS_KG_GLINKER_COMMAND");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        std::env::remove_var("AGENTOS_DATA_DIR");
    }
}
