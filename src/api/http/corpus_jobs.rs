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
use sha2::{Digest, Sha256};

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
const MAX_AUDIT_EVENTS: usize = 64;
const MAX_RETRY_ATTEMPTS: u32 = 3;

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
    /// Trusted scheduler-only provenance. HTTP input cannot set this field.
    #[serde(default, skip_deserializing)]
    pub watcher_id: Option<String>,
}

/// Result of a trusted enqueue operation shared by the authenticated HTTP
/// handler and the deployment-configured watcher scheduler.
pub(crate) struct EnqueuedOnlineCorpusJob {
    pub job: OnlineCorpusJob,
    pub reused: bool,
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
                    Self::Queued
                        | Self::AwaitingReview
                        | Self::Succeeded
                        | Self::Failed
                        | Self::Cancelled
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
    pub retries: u32,
    #[serde(default)]
    pub max_attempts: u32,
    #[serde(default)]
    pub latest_error: Option<String>,
    #[serde(default)]
    pub latest_error_classification: Option<CorpusJobErrorClassification>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CorpusJobErrorClassification {
    Transient,
    Validation,
    Authorization,
    Policy,
    Internal,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CorpusJobAuditEvent {
    pub at: String,
    pub event: String,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub error_classification: Option<CorpusJobErrorClassification>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct CorpusJobLinks {
    #[serde(default)]
    pub source_content_sha256: Option<String>,
    #[serde(default)]
    pub extractor: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub canonicalization_decision_count: usize,
    #[serde(default)]
    pub quality_gate_report_persisted: bool,
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
    #[serde(default)]
    pub audit_events: Vec<CorpusJobAuditEvent>,
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
    if request.source.uri.as_ref().is_some_and(|uri| {
        uri.len() > 2048
            || uri
                .split_once("://")
                .is_some_and(|(_, rest)| rest.contains('@'))
    }) {
        return Err("source.uri exceeds 2048 characters or includes credentials".to_string());
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

fn sanitize_metadata(value: &str) -> String {
    let without_lines = value.replace(['\n', '\r'], " ");
    let redacted = without_lines
        .split_whitespace()
        .map(|part| {
            if part.contains("://")
                && part
                    .split_once("://")
                    .is_some_and(|(_, rest)| rest.contains('@'))
            {
                "<redacted-url>"
            } else if part.contains("token=")
                || part.contains("api_key=")
                || part.contains("password=")
            {
                "<redacted>"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    bounded_error_metadata(&redacted)
}

fn append_audit_event(
    job: &mut OnlineCorpusJob,
    event: impl Into<String>,
    detail: Option<&str>,
    error_classification: Option<CorpusJobErrorClassification>,
) {
    job.audit_events.push(CorpusJobAuditEvent {
        at: chrono::Utc::now().to_rfc3339(),
        event: event.into(),
        attempt: job.attempt_summary.attempts,
        error_classification,
        detail: detail.map(sanitize_metadata),
    });
    if job.audit_events.len() > MAX_AUDIT_EVENTS {
        job.audit_events
            .drain(..job.audit_events.len() - MAX_AUDIT_EVENTS);
    }
}

fn classify_job_error(error: &str) -> CorpusJobErrorClassification {
    if error.starts_with("entity resolution requires AGENTOS_KG_GLINKER_COMMAND")
        || error.starts_with("start entity-resolution sidecar")
        || error.starts_with("wait for entity-resolution sidecar")
        || error.starts_with("entity-resolution sidecar failed")
    {
        CorpusJobErrorClassification::Transient
    } else if error.contains("quality gate blocked")
        || error.contains("canonicalization ambiguity")
        || error.contains("entity resolution uncertain")
    {
        CorpusJobErrorClassification::Policy
    } else if error.contains("required") || error.contains("must be") || error.contains("invalid") {
        CorpusJobErrorClassification::Validation
    } else if error.contains("unauthorized") || error.contains("verified JWT") {
        CorpusJobErrorClassification::Authorization
    } else {
        CorpusJobErrorClassification::Internal
    }
}

fn is_retryable(classification: CorpusJobErrorClassification) -> bool {
    classification == CorpusJobErrorClassification::Transient
}

fn public_job(job: &OnlineCorpusJob, reused: bool) -> Value {
    json!({
        "job": job,
        "reused": reused,
        "production_write": false,
        "materialization_status": "not_materialized",
    })
}

/// Persist one claims-scoped job or return its existing idempotent equivalent.
///
/// The caller is responsible for establishing `claims`: HTTP derives them from
/// a verified JWT, while watchers derive them only from an explicit deployment
/// registration. This function never selects a graph or writes production data.
pub(crate) async fn enqueue_online_corpus_job(
    store: &OnlineCorpusJobStore,
    claims: &IsolationClaims,
    request: CreateOnlineCorpusJobRequest,
) -> Result<EnqueuedOnlineCorpusJob, String> {
    validate_create_request(&request)?;
    let mut jobs = store.write().await;
    if let Some(key) = request.idempotency_key.as_deref() {
        if let Some(existing) = jobs
            .iter()
            .find(|job| job_is_in_scope(job, claims) && job.idempotency_key.as_deref() == Some(key))
        {
            if existing.source == request.source {
                return Ok(EnqueuedOnlineCorpusJob {
                    job: existing.clone(),
                    reused: true,
                });
            }
            return Err(
                "idempotency_key is already bound to another source in this scope".to_string(),
            );
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
        updated_at: now.clone(),
        started_at: None,
        completed_at: None,
        attempt_summary: CorpusJobAttemptSummary {
            max_attempts: MAX_RETRY_ATTEMPTS,
            ..Default::default()
        },
        links: CorpusJobLinks::default(),
        audit_events: vec![CorpusJobAuditEvent {
            at: now,
            event: if request.watcher_id.is_some() {
                "watcher_enqueued".into()
            } else {
                "queued".into()
            },
            attempt: 0,
            error_classification: None,
            detail: request
                .watcher_id
                .as_deref()
                .map(|watcher_id| format!("watcher_id={watcher_id}; cursor=source_version"))
                .or_else(|| Some("job accepted".into())),
        }],
    };
    let mut next = jobs.clone();
    next.push(job.clone());
    save_online_corpus_jobs(&next)?;
    *jobs = next;
    Ok(EnqueuedOnlineCorpusJob { job, reused: false })
}

/// GET /api/v1/online-corpus-jobs/observability
///
/// Claims-scoped operational counters. Corpus payloads and other scopes are
/// never returned; queue capacity is supplied by the process configuration.
pub(crate) async fn online_corpus_job_observability_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let jobs = state.online_corpus_jobs.read().await;
    let scoped: Vec<_> = jobs
        .iter()
        .filter(|job| job_is_in_scope(job, claims))
        .collect();
    let queued = scoped
        .iter()
        .filter(|job| job.state == OnlineCorpusJobState::Queued)
        .count();
    let active = scoped
        .iter()
        .filter(|job| job.state == OnlineCorpusJobState::Running)
        .count();
    let retries: u32 = scoped.iter().map(|job| job.attempt_summary.retries).sum();
    let failed = scoped
        .iter()
        .filter(|job| job.state == OnlineCorpusJobState::Failed)
        .count();
    Json(json!({
        "queue_depth": queued,
        "active_work": active,
        "retry_count": retries,
        "terminal_failures": failed,
        "queue_capacity": state.online_corpus_queue_capacity,
        "saturated": queued >= state.online_corpus_queue_capacity,
        "oldest_queued_at": scoped.iter()
            .filter(|job| job.state == OnlineCorpusJobState::Queued)
            .map(|job| job.created_at.as_str()).min(),
        "production_write": false,
    }))
    .into_response()
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
    match enqueue_online_corpus_job(&state.online_corpus_jobs, claims, request).await {
        Ok(enqueued) if enqueued.reused => {
            (StatusCode::OK, Json(public_job(&enqueued.job, true))).into_response()
        }
        Ok(enqueued) => {
            (StatusCode::CREATED, Json(public_job(&enqueued.job, false))).into_response()
        }
        Err(error) if error.contains("idempotency_key is already bound") => {
            (StatusCode::CONFLICT, Json(json!({"error": error}))).into_response()
        }
        Err(error) if error.contains("required") || error.contains("exceeds") => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("persist online corpus job: {error}")})),
        )
            .into_response(),
    }
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
        Err(error) => {
            let classification = classify_job_error(&error);
            // `job` is the pre-run snapshot, so include the attempt that just
            // transitioned to Running before deciding whether it may be queued.
            let retryable = is_retryable(classification)
                && job.attempt_summary.attempts.saturating_add(1) < MAX_RETRY_ATTEMPTS;
            let next_state = if retryable {
                OnlineCorpusJobState::Queued
            } else {
                OnlineCorpusJobState::Failed
            };
            match transition_job_for_claims(
                &state.online_corpus_jobs,
                claims,
                &job_id,
                next_state,
                Some(&error),
            )
            .await
            {
                Ok(job) => (
                    if retryable {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::UNPROCESSABLE_ENTITY
                    },
                    Json(json!({
                        "job": job, "error": sanitize_metadata(&error),
                        "error_classification": classification,
                        "retry_scheduled": retryable,
                        "production_write": false,
                        "materialization_status": "not_materialized",
                    })),
                )
                    .into_response(),
                Err(transition_error) => job_transition_response(transition_error),
            }
        }
    }
}

async fn run_job_ke_primitives(
    state: &AppState,
    claims: &IsolationClaims,
    job: &OnlineCorpusJob,
    request: &RunOnlineCorpusJobRequest,
) -> Result<CorpusJobLinks, String> {
    let extraction_id = job.id.clone();
    let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone())
        .map_err(|error| error.to_string())?;
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
         <https://agentos.ontology/extraction/model> \"{}\" ; \
         <https://agentos.ontology/extraction/sourceContentSha256> \"{}\" ; \
         <https://agentos.ontology/extraction/canonicalizationDecisions> \"{}\" .",
        sparql_literal(&job.source.id),
        sparql_literal(&job.source.version),
        sparql_literal(&request.extractor),
        sparql_literal(request.model.as_deref().unwrap_or("unspecified")),
        sha256_hex(&request.text),
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
            let report = match serde_json::from_str::<
                crate::knowledge_graph::quality_gate::QualityGateReport,
            >(&review.report_json)
            {
                Ok(report) => report,
                Err(first_error) => {
                    let Some(decoded) = decode_sparql_literal(&review.report_json) else {
                        return Err(format!(
                            "stored quality gate report is invalid: {first_error}"
                        ));
                    };
                    serde_json::from_str(&decoded).map_err(|error| {
                        format!("stored quality gate report is invalid: {error}")
                    })?
                }
            };
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
        // Entity-resolution evidence is tied to this staged extraction.
        // Reusing an approval from a prior job would bypass the sidecar for
        // this run, including when the sidecar is unavailable. Require the
        // sidecar to produce a fresh, approval-held suggestion instead.
        let suggestion = create_entity_resolution_suggestion_from_staging(
            &kg,
            claims,
            &extraction_id,
            &source_iri,
            &node.label,
        )?
        .ok_or_else(|| format!("entity resolution uncertain for staged mention {}", node.id))?;
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
        source_content_sha256: Some(sha256_hex(&request.text)),
        extractor: Some(request.extractor.clone()),
        model: request.model.clone(),
        canonicalization_decision_count: canonical.decisions.len(),
        quality_gate_report_persisted: true,
        staged_extraction_ids: vec![extraction_id],
        review_ids: vec![review.review_id],
        entity_resolution_suggestion_ids: suggestion_ids,
    })
}

fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn sparql_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn decode_sparql_literal(value: &str) -> Option<String> {
    serde_json::from_str::<String>(&format!("\"{value}\"")).ok()
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
    append_audit_event(
        &mut next[index],
        "provenance_recorded",
        Some("canonicalization, quality gate, ER suggestions, and staging linked"),
        None,
    );
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
        job.attempt_summary.latest_error = latest_error.map(sanitize_metadata);
        job.attempt_summary.latest_error_classification = latest_error.map(classify_job_error);
        if next_state == OnlineCorpusJobState::Queued && latest_error.is_some() {
            job.attempt_summary.retries = job.attempt_summary.retries.saturating_add(1);
        }
        append_audit_event(
            job,
            match next_state {
                OnlineCorpusJobState::Queued if latest_error.is_some() => "retry_queued",
                OnlineCorpusJobState::Queued => "queued",
                OnlineCorpusJobState::Running => "running",
                OnlineCorpusJobState::AwaitingReview => "awaiting_review",
                OnlineCorpusJobState::Succeeded => "succeeded",
                OnlineCorpusJobState::Failed => "failed",
                OnlineCorpusJobState::Cancelled => "cancelled",
            },
            latest_error,
            latest_error.map(classify_job_error),
        );
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
            online_corpus_queue_capacity: 10,
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
                "/api/v1/online-corpus-jobs/observability",
                get(online_corpus_job_observability_handler),
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
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn isolation_contract_online_corpus_jobs_fail_closed_and_are_claims_scoped() {
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
        let invalid_scope = token("tenant/a", "project-a");
        let (status, _) = request(
            &router,
            "POST",
            "/api/v1/online-corpus-jobs",
            payload.clone(),
            Some(&invalid_scope),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            load_online_corpus_jobs().is_empty(),
            "missing or invalid claims must not persist a job"
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
        for (method, uri) in [
            ("GET", "/api/v1/online-corpus-jobs".to_string()),
            ("GET", format!("/api/v1/online-corpus-jobs/{id}")),
            ("POST", format!("/api/v1/online-corpus-jobs/{id}/cancel")),
        ] {
            let (status, _) = request(&router, method, &uri, json!({}), None).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must require verified claims"
            );
        }
        assert_eq!(
            load_online_corpus_jobs()[0].state,
            OnlineCorpusJobState::Queued,
            "unauthenticated reads and cancellation must not transition a job"
        );
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
        let (status, hidden_observability) = request(
            &router,
            "GET",
            "/api/v1/online-corpus-jobs/observability",
            json!({}),
            Some(&tenant_b),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hidden_observability["queue_depth"], 0);
        let (status, observability) = request(
            &router,
            "GET",
            "/api/v1/online-corpus-jobs/observability",
            json!({}),
            Some(&tenant_a),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(observability["queue_depth"], 1);
        assert_eq!(observability["queue_capacity"], 10);
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
            audit_events: vec![],
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
    async fn transient_failures_have_bounded_retries_and_auditable_attempts() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let store = Arc::new(tokio::sync::RwLock::new(vec![OnlineCorpusJob {
            id: "retry-job".into(),
            source: CorpusSource {
                id: "corpus".into(),
                version: "v1".into(),
                uri: None,
            },
            idempotency_key: None,
            request_id: "request".into(),
            tenant_id: claims.tenant_id().into(),
            project_id: claims.project_id().into(),
            actor_id: claims.actor_id().into(),
            state: OnlineCorpusJobState::Queued,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            started_at: None,
            completed_at: None,
            attempt_summary: CorpusJobAttemptSummary {
                max_attempts: MAX_RETRY_ATTEMPTS,
                ..Default::default()
            },
            links: CorpusJobLinks::default(),
            audit_events: vec![],
        }]));

        for attempt in 1..=MAX_RETRY_ATTEMPTS {
            let running = transition_job_for_claims(
                &store,
                &claims,
                "retry-job",
                OnlineCorpusJobState::Running,
                None,
            )
            .await
            .unwrap();
            assert_eq!(running.attempt_summary.attempts, attempt);
            let next = if attempt < MAX_RETRY_ATTEMPTS {
                OnlineCorpusJobState::Queued
            } else {
                OnlineCorpusJobState::Failed
            };
            transition_job_for_claims(
                &store,
                &claims,
                "retry-job",
                next,
                Some("start entity-resolution sidecar: temporary outage token=secret"),
            )
            .await
            .unwrap();
        }
        let job = store.read().await[0].clone();
        assert_eq!(job.state, OnlineCorpusJobState::Failed);
        assert_eq!(job.attempt_summary.retries, MAX_RETRY_ATTEMPTS - 1);
        assert_eq!(
            job.attempt_summary.latest_error_classification,
            Some(CorpusJobErrorClassification::Transient)
        );
        let latest_error = job.attempt_summary.latest_error.unwrap();
        assert!(!latest_error.contains("secret"));
        assert!(latest_error.contains("<redacted>"));
        assert_eq!(
            classify_job_error("quality gate blocked the staged extraction"),
            CorpusJobErrorClassification::Policy
        );
        assert_eq!(
            classify_job_error(
                "entity resolution requires AGENTOS_KG_GLINKER_COMMAND configured as one executable path"
            ),
            CorpusJobErrorClassification::Transient
        );
        std::env::remove_var("AGENTOS_DATA_DIR");
    }

    #[test]
    fn provenance_metadata_redacts_payloads_and_credentials() {
        assert_eq!(sha256_hex("raw corpus text").len(), 64);
        assert!(!sanitize_metadata("raw corpus text token=do-not-store").contains("do-not-store"));
        assert_eq!(
            sanitize_metadata("fetch https://user:password@example.test/corpus"),
            "fetch <redacted-url>"
        );
    }

    #[tokio::test]
    async fn isolation_contract_online_corpus_runner_requires_claims_and_never_materializes_production(
    ) {
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
        let other_claims =
            IsolationClaims::from_verified("tenant-b", "project-a", "service").unwrap();
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
        let other_before = kg
            .query_sparql_for_claims(&other_claims, "SELECT ?s WHERE { ?s ?p ?o }")
            .unwrap()
            .len();
        let router = router(state.clone());
        let jwt = token("tenant-a", "project-a");
        let other_jwt = token("tenant-b", "project-a");
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
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/run"),
            run_payload.clone(),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/run"),
            run_payload.clone(),
            Some("invalid"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/run"),
            run_payload.clone(),
            Some(&other_jwt),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let mut crafted_payload = run_payload.clone();
        crafted_payload["production_graph"] = json!("graph://tenant-b/project-a");
        let (status, _) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/run"),
            crafted_payload,
            Some(&jwt),
        )
        .await;
        assert!(
            status.is_client_error(),
            "client graph targets must be rejected rather than accepted"
        );
        assert_eq!(
            kg.query_sparql_for_claims(&claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            before,
            "failed runner calls must fail closed without production writes"
        );
        assert_eq!(
            kg.query_sparql_for_claims(&other_claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            other_before,
            "cross-scope runner calls and crafted payloads must not redirect writes"
        );
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
            ran["job"]["links"]["source_content_sha256"]
                .as_str()
                .unwrap()
                .len(),
            64,
            "provenance retains a content digest rather than raw corpus text"
        );
        assert!(ran["job"]["audit_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "provenance_recorded"));
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
        assert_eq!(
            kg.query_sparql_for_claims(&other_claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            other_before,
            "runner success must not write another tenant's production graph"
        );
        let (status, replay) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{id}/run"),
            run_payload.clone(),
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

        // A transient runner failure is retried from queued state; it must not
        // make a production write or turn the retry into a privileged success.
        let (status, retry_job) = request(
            &router,
            "POST",
            "/api/v1/online-corpus-jobs",
            json!({"source": {"id": "retry-docs", "version": "v1"}}),
            Some(&jwt),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let retry_id = retry_job["job"]["id"].as_str().unwrap();
        std::env::remove_var("AGENTOS_KG_GLINKER_COMMAND");
        let (status, retry) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{retry_id}/run"),
            run_payload.clone(),
            Some(&jwt),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry["job"]["state"], "queued");
        assert_eq!(retry["retry_scheduled"], true);
        assert_eq!(
            kg.query_sparql_for_claims(&claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            before,
            "a failed runner attempt must not write the production graph"
        );
        std::env::set_var("AGENTOS_KG_GLINKER_COMMAND", &sidecar);
        let (status, retried) = request(
            &router,
            "POST",
            &format!("/api/v1/online-corpus-jobs/{retry_id}/run"),
            run_payload,
            Some(&jwt),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(retried["job"]["state"], "awaiting_review");
        assert_eq!(
            kg.query_sparql_for_claims(&claims, "SELECT ?s WHERE { ?s ?p ?o }")
                .unwrap()
                .len(),
            before,
            "a retried runner must still stage and await approval rather than materialize"
        );

        std::env::remove_var("AGENTOS_KG_GLINKER_COMMAND");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        std::env::remove_var("AGENTOS_DATA_DIR");
    }
}
