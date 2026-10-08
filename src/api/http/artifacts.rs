//! Claims-scoped, replayable coding artifacts and immutable input snapshots.
//!
//! Artifact bytes live below the tenant-minted blob prefix.  Their JSON index is
//! stored as an RDF literal in the claims-minted graph, so neither request data
//! nor an artifact identifier can select another tenant's storage namespace.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    isolation::IsolationClaims,
    knowledge_graph::{
        store::KnowledgeGraphStore,
        types::{RdfQuad, RdfValue},
    },
};

use super::{iam::UserIdentity, AppState};

pub const ARTIFACT_UPLOAD_MAX_BYTES: usize = 5 * 1024 * 1024;
const ARTIFACT_VOCAB: &str = "https://wild-agentos.dev/artifact/";
const ARTIFACT_METADATA_PREDICATE: &str = "https://wild-agentos.dev/artifact/metadata";

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Patch,
    RunTranscript,
    ReproduceScript,
    /// Immutable UTF-8 JSON input uploaded before an invocation exists, for
    /// example to be referenced through `input_ref`.
    InputSnapshot,
}

impl ArtifactKind {
    fn content_type(self) -> &'static str {
        match self {
            Self::Patch => "text/x-diff; charset=utf-8",
            Self::RunTranscript => "text/plain; charset=utf-8",
            Self::ReproduceScript => "text/x-shellscript; charset=utf-8",
            Self::InputSnapshot => "application/json",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::RunTranscript => "log",
            Self::ReproduceScript => "sh",
            Self::InputSnapshot => "json",
        }
    }

    /// Replay kinds describe one execution and must name it. Input snapshots
    /// are uploaded before any task exists, so their link is optional.
    fn requires_task_iri(self) -> bool {
        !matches!(self, Self::InputSnapshot)
    }

    fn parse(value: &str) -> Option<Self> {
        serde_json::from_value(Value::String(value.to_string())).ok()
    }
}

#[derive(Debug, Deserialize)]
pub struct ArtifactUploadRequest {
    pub kind: ArtifactKind,
    /// IRI of the task/checkpoint execution this artifact can replay. Required
    /// for replay kinds, optional for `input_snapshot`. It is only shape-checked
    /// and stored: it is not resolved, authorized against, or queried, so a
    /// caller may supply its own business IRI/URN.
    #[serde(default)]
    pub task_iri: Option<String>,
    /// Standard base64-encoded artifact bytes. This avoids storing request
    /// credentials in a multipart side channel and is bounded by the route.
    pub content_base64: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub id: String,
    pub kind: ArtifactKind,
    /// `null` when an `input_snapshot` was uploaded without a task link.
    /// Records written before this field became optional still deserialize.
    #[serde(default)]
    pub task_iri: Option<String>,
    pub blob_key: String,
    pub content_type: String,
    pub size_bytes: usize,
    pub sha256: String,
    pub created_at: String,
    pub created_by: String,
}

fn artifact_subject(id: &str) -> String {
    format!("{ARTIFACT_VOCAB}{id}")
}

fn artifact_key(id: &str, kind: ArtifactKind) -> String {
    format!("artifacts/{id}.{}", kind.extension())
}

fn valid_task_iri(task_iri: &str) -> bool {
    !task_iri.trim().is_empty()
        && task_iri.len() <= 2048
        && !task_iri.bytes().any(|byte| byte.is_ascii_control())
}

/// Stable machine-readable code for a secret-guard rejection. The matched
/// text and rule are never echoed.
const ARTIFACT_SECRET_ERROR_CODE: &str = "artifact_plaintext_secret";

/// Blocks recognizable credential values before they can be persisted; see
/// [`crate::utils::secret_scan`] for the shared real-format rules. Replay
/// inputs must reference credentials through environment variables or a
/// secret manager, never embed their values.
fn contains_plaintext_secret(bytes: &[u8]) -> bool {
    crate::utils::secret_scan::contains_plaintext_secret(&String::from_utf8_lossy(bytes))
}

/// Deepest accepted `input_snapshot` nesting (arrays/objects). This is what
/// `serde_json`'s default recursion limit (128) admits, so every accepted
/// snapshot can later be parsed into a `serde_json::Value` by the platform
/// or a caller.
pub(crate) const INPUT_SNAPSHOT_MAX_DEPTH: usize = 127;

/// Whether `bytes` is UTF-8 JSON nested at most [`INPUT_SNAPSHOT_MAX_DEPTH`]
/// levels that parses into a `serde_json::Value`. Parsing into `Value` (not
/// `IgnoredAny`, which skips string contents and is not depth-limited)
/// validates every string; the explicit depth pre-scan keeps the limit
/// independent of `serde_json` defaults and rejects deep input cheaply.
fn is_valid_snapshot_json(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        !json_nesting_exceeds(text.as_bytes(), INPUT_SNAPSHOT_MAX_DEPTH)
            && serde_json::from_str::<Value>(text).is_ok()
    })
}

/// True when array/object nesting outside string literals exceeds `max`.
fn json_nesting_exceeds(bytes: &[u8], max: usize) -> bool {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for &byte in bytes {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

fn require_claims(identity: &UserIdentity) -> Result<&IsolationClaims, (StatusCode, Json<Value>)> {
    identity.isolation_claims().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "verified isolation claims required for coding artifacts" })),
        )
    })
}

fn artifact_store(state: &AppState) -> Result<KnowledgeGraphStore, (StatusCode, Json<Value>)> {
    KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "artifact metadata store unavailable" })),
        )
    })
}

fn save_metadata(
    state: &AppState,
    claims: &IsolationClaims,
    metadata: &ArtifactMetadata,
) -> Result<(), (StatusCode, Json<Value>)> {
    let metadata_json = serde_json::to_vec(metadata).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "artifact metadata serialization failed" })),
        )
    })?;
    // Query helpers return lexical RDF literals rather than decoded literal
    // values. Base64 therefore preserves JSON quotes and escapes exactly.
    let metadata_index = STANDARD.encode(metadata_json);
    artifact_store(state)?
        .write_quads_for_claims(
            claims,
            &[RdfQuad {
                subject: artifact_subject(&metadata.id),
                predicate: ARTIFACT_METADATA_PREDICATE.to_string(),
                object: RdfValue::Literal(metadata_index),
                graph: None,
            }],
        )
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "artifact metadata persistence failed" })),
            )
        })
}

fn load_metadata(
    state: &AppState,
    claims: &IsolationClaims,
    id: Option<&str>,
) -> Result<Vec<ArtifactMetadata>, (StatusCode, Json<Value>)> {
    query_metadata(&state.kg_store, claims, id).map_err(|error| {
        let message = match error {
            ArtifactMetadataError::Store => "artifact metadata store unavailable",
            ArtifactMetadataError::Query => "artifact metadata query failed",
        };
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": message })),
        )
    })
}

/// Why an artifact metadata lookup failed (no detail is ever returned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactMetadataError {
    Store,
    Query,
}

/// Reads artifact metadata from the claims graph only. `id` must already be a
/// validated UUID when present (it is interpolated into the query).
fn query_metadata(
    kg_store: &Arc<oxigraph::store::Store>,
    claims: &IsolationClaims,
    id: Option<&str>,
) -> Result<Vec<ArtifactMetadata>, ArtifactMetadataError> {
    let query = match id {
        Some(id) => format!(
            "SELECT ?metadata WHERE {{ <{}> <{}> ?metadata }}",
            artifact_subject(id),
            ARTIFACT_METADATA_PREDICATE
        ),
        None => format!(
            "SELECT ?metadata WHERE {{ ?artifact <{}> ?metadata }} ORDER BY DESC(?metadata)",
            ARTIFACT_METADATA_PREDICATE
        ),
    };
    let rows = KnowledgeGraphStore::with_shared_store(kg_store.clone())
        .map_err(|_| ArtifactMetadataError::Store)?
        .query_sparql_for_claims(claims, &query)
        .map_err(|_| ArtifactMetadataError::Query)?;
    Ok(rows
        .iter()
        // KnowledgeGraphStore preserves SPARQL variable spelling, including
        // its leading `?` (for example, `?metadata`).
        .filter_map(|row| {
            row.get("?metadata")
                .or_else(|| row.get("metadata"))
                .and_then(Value::as_str)
        })
        .filter_map(|encoded| STANDARD.decode(encoded).ok())
        .filter_map(|raw| serde_json::from_slice(&raw).ok())
        .collect())
}

/// One artifact's metadata from the caller's claims graph (tenant + project),
/// for the built-in `input_ref` resolver. `Ok(None)` when the id is not a
/// UUID or has no record in this scope; `Err` when the store fails.
pub(crate) fn load_artifact_metadata(
    kg_store: &Arc<oxigraph::store::Store>,
    claims: &IsolationClaims,
    id: &str,
) -> Result<Option<ArtifactMetadata>, ArtifactMetadataError> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    Ok(query_metadata(kg_store, claims, Some(id))?
        .into_iter()
        .find(|record| record.id == id))
}

/// Whether `metadata.blob_key` is the key the upload route derives from the
/// id and kind, so a resolver never follows a key it did not mint.
pub(crate) fn is_canonical_blob_key(metadata: &ArtifactMetadata) -> bool {
    metadata.blob_key == artifact_key(&metadata.id, metadata.kind)
}

/// Test helper: writes `metadata` into the claims graph exactly as the upload
/// route does, without an `AppState`.
#[cfg(test)]
pub(crate) fn write_metadata_for_tests(
    kg_store: &Arc<oxigraph::store::Store>,
    claims: &IsolationClaims,
    metadata: &ArtifactMetadata,
) {
    let metadata_index = STANDARD.encode(serde_json::to_vec(metadata).unwrap());
    KnowledgeGraphStore::with_shared_store(kg_store.clone())
        .unwrap()
        .write_quads_for_claims(
            claims,
            &[RdfQuad {
                subject: artifact_subject(&metadata.id),
                predicate: ARTIFACT_METADATA_PREDICATE.to_string(),
                object: RdfValue::Literal(metadata_index),
                graph: None,
            }],
        )
        .unwrap();
}

/// Test helper: the blob key the upload route derives for `id` / `kind`.
#[cfg(test)]
pub(crate) fn artifact_key_for_tests(id: &str, kind: ArtifactKind) -> String {
    artifact_key(id, kind)
}

/// POST /api/v1/artifacts — persist one replayable coding artifact.
pub(crate) async fn upload_artifact_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<ArtifactUploadRequest>,
) -> Response {
    let claims = match require_claims(&identity) {
        Ok(claims) => claims,
        Err(response) => return response.into_response(),
    };
    match request.task_iri.as_deref() {
        Some(task_iri) if !valid_task_iri(task_iri) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "task_iri must be a non-empty control-character-free IRI" })),
            )
                .into_response();
        }
        None if request.kind.requires_task_iri() => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "task_iri is required for this artifact kind" })),
            )
                .into_response();
        }
        _ => {}
    }
    let bytes = match STANDARD.decode(&request.content_base64) {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "content_base64 is not valid base64" })),
            )
                .into_response()
        }
    };
    if bytes.is_empty() || bytes.len() > ARTIFACT_UPLOAD_MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "artifact content must be between 1 byte and 5 MiB" })),
        )
            .into_response();
    }
    if request.kind == ArtifactKind::InputSnapshot && !is_valid_snapshot_json(&bytes) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "input_snapshot content must be valid UTF-8 JSON" })),
        )
            .into_response();
    }
    if contains_plaintext_secret(&bytes) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "plaintext secrets are forbidden in coding artifacts",
                "code": ARTIFACT_SECRET_ERROR_CODE,
            })),
        )
            .into_response();
    }
    let blob = match &state.blob_store {
        Some(blob) => blob.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "artifact blob store unavailable" })),
            )
                .into_response()
        }
    };

    let id = uuid::Uuid::new_v4().hyphenated().to_string();
    let key = artifact_key(&id, request.kind);
    // Type is derived from the approved artifact kind, not request metadata.
    let content_type = request.kind.content_type().to_string();
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let metadata = ArtifactMetadata {
        id: id.clone(),
        kind: request.kind,
        task_iri: request.task_iri,
        blob_key: key.clone(),
        content_type: content_type.clone(),
        size_bytes: bytes.len(),
        sha256,
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by: claims.actor_id().to_string(),
    };
    if blob.put(claims, &key, &bytes, &content_type).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "artifact storage failed" })),
        )
            .into_response();
    }
    if let Err(response) = save_metadata(&state, claims, &metadata) {
        let _ = blob.delete(claims, &key).await;
        return response.into_response();
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "artifact": metadata,
            "download_url": format!("/api/v1/artifacts/{id}/download"),
        })),
    )
        .into_response()
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ListArtifactsQuery {
    /// Optional exact kind filter, e.g. `?kind=input_snapshot`.
    kind: Option<String>,
}

/// GET /api/v1/artifacts — list metadata in only the caller's claims graph.
pub(crate) async fn list_artifacts_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Query(query): Query<ListArtifactsQuery>,
) -> Response {
    let claims = match require_claims(&identity) {
        Ok(claims) => claims,
        Err(response) => return response.into_response(),
    };
    let kind = match query.kind.as_deref().map(ArtifactKind::parse) {
        None => None,
        Some(Some(kind)) => Some(kind),
        Some(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "unknown artifact kind" })),
            )
                .into_response()
        }
    };
    match load_metadata(&state, claims, None) {
        Ok(mut artifacts) => {
            if let Some(kind) = kind {
                artifacts.retain(|artifact| artifact.kind == kind);
            }
            (
                StatusCode::OK,
                Json(json!({ "count": artifacts.len(), "artifacts": artifacts })),
            )
                .into_response()
        }
        Err(response) => response.into_response(),
    }
}

/// GET /api/v1/artifacts/:id/download — retrieve one claims-scoped artifact.
pub(crate) async fn download_artifact_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(id): Path<String>,
) -> Response {
    let claims = match require_claims(&identity) {
        Ok(claims) => claims,
        Err(response) => return response.into_response(),
    };
    if uuid::Uuid::parse_str(&id).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid artifact id" })),
        )
            .into_response();
    }
    let metadata = match load_metadata(&state, claims, Some(&id)) {
        Ok(mut records) => match records.pop() {
            Some(record) => record,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "artifact not found" })),
                )
                    .into_response()
            }
        },
        Err(response) => return response.into_response(),
    };
    let blob = match &state.blob_store {
        Some(blob) => blob,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "artifact blob store unavailable" })),
            )
                .into_response()
        }
    };
    match blob.get(claims, &metadata.blob_key).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, metadata.content_type),
                (
                    header::CONTENT_DISPOSITION,
                    format!(
                        "attachment; filename=\"{}.{}\"",
                        metadata.id,
                        metadata.kind.extension()
                    ),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "artifact content not found" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod snapshot_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::http::{api_gov::ApiUsageState, SharedVectorStore},
        blob::LocalFsBlobStore,
        config::GatewaySettings,
        core::core_types::{CoreConfig, SemanticCore},
        gateway::unified_gateway::UnifiedGateway,
        tools::prompt_registry::PromptRegistry,
    };

    #[test]
    fn artifact_kinds_and_secret_guard_are_explicit() {
        assert_eq!(ArtifactKind::Patch.extension(), "patch");
        // Real shape, obviously fake value.
        let token = format!("export TOKEN=ghp_{}", "FAKE".repeat(9));
        assert!(contains_plaintext_secret(token.as_bytes()));
        assert!(!contains_plaintext_secret(
            b"export TOKEN=\"$TOKEN_FROM_ENV\""
        ));
    }

    #[test]
    fn task_iri_must_not_contain_control_characters() {
        assert!(valid_task_iri("iri://task/replayable"));
        assert!(!valid_task_iri(""));
        assert!(!valid_task_iri("iri://task/\ninvalid"));
    }

    pub(super) fn test_state(root: std::path::PathBuf) -> Arc<AppState> {
        let gateway = UnifiedGateway::new(&GatewaySettings {
            base_url: "http://localhost".to_string(),
            api_key: String::new(),
            default_model: "test".to_string(),
            timeout_seconds: 1,
            max_retries: 0,
            retry_base_ms: 1,
            use_responses_api: false,
            model_mapping: Default::default(),
        })
        .unwrap();
        let core = SemanticCore::new(CoreConfig {
            max_node_size: 1024,
            max_projection_size: 1024,
            l0_storage_path: root.join("l0").display().to_string(),
            event_buffer_size: 1,
            enable_metrics: false,
            eviction_config: None,
        })
        .unwrap();
        Arc::new(AppState {
            core: Arc::new(core),
            gateway: Arc::new(gateway),
            kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
            config_info: Arc::new(tokio::sync::RwLock::new(json!({}))),
            agents_info: json!({}),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![])),
            prompts: Arc::new(PromptRegistry::new()),
            kb_categories: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_bases: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_packs: Arc::new(tokio::sync::RwLock::new(vec![])),
            vector_store: Arc::new(arc_swap::ArcSwapOption::empty()) as SharedVectorStore,
            blob_store: Some(Arc::new(LocalFsBlobStore::new(root.join("blobs")))),
            task_executor: None,
            batch_manager: None,
            api_clients: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_keys: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_usage: Arc::new(ApiUsageState::default()),
            online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
            online_corpus_queue_capacity: 10,
            invocations: crate::api::http::invocations::InvocationsRuntime::unavailable(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    #[tokio::test]
    async fn claims_scoped_artifact_metadata_and_bytes_are_tenant_isolated() {
        let root = std::env::temp_dir().join(format!("artifact-test-{}", uuid::Uuid::new_v4()));
        let state = test_state(root.clone());
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project", "actor-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project", "actor-b").unwrap();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let metadata = ArtifactMetadata {
            id: id.clone(),
            kind: ArtifactKind::Patch,
            task_iri: Some("iri://task/94".to_string()),
            blob_key: artifact_key(&id, ArtifactKind::Patch),
            content_type: ArtifactKind::Patch.content_type().to_string(),
            size_bytes: 12,
            sha256: "test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: "actor-a".to_string(),
        };
        let blob = state.blob_store.as_ref().unwrap();
        blob.put(
            &tenant_a,
            &metadata.blob_key,
            b"diff --git\n",
            "text/x-diff",
        )
        .await
        .unwrap();
        save_metadata(&state, &tenant_a, &metadata).unwrap();

        assert_eq!(
            load_metadata(&state, &tenant_a, Some(&id)).unwrap(),
            vec![metadata.clone()]
        );
        assert!(load_metadata(&state, &tenant_b, Some(&id))
            .unwrap()
            .is_empty());
        assert!(blob.get(&tenant_b, &metadata.blob_key).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifacts_reject_missing_verified_claims() {
        assert!(require_claims(&UserIdentity::anonymous()).is_err());
    }

    async fn upload(
        state: &Arc<AppState>,
        claims: &IsolationClaims,
        content: &[u8],
    ) -> (StatusCode, Value) {
        let identity =
            crate::api::http::iam::test_identity_from_verified_claims(claims.clone(), vec![]);
        let response = upload_artifact_handler(
            State(state.clone()),
            identity,
            Json(ArtifactUploadRequest {
                kind: ArtifactKind::RunTranscript,
                task_iri: Some("iri://task/secret-guard".to_string()),
                content_base64: STANDARD.encode(content),
            }),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn upload_accepts_ordinary_words_and_rejects_real_tokens_without_persisting() {
        let root = std::env::temp_dir().join(format!("artifact-test-{}", uuid::Uuid::new_v4()));
        let state = test_state(root.clone());
        let claims = IsolationClaims::from_verified("tenant-a", "project", "actor-a").unwrap();

        let ordinary =
            br#"{"task-id":"task-1","risk-level":"low","disk-free":"9G","ask-user":true}"#;
        let (status, body) = upload(&state, &claims, ordinary).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let token = ["{\"key\":\"s", "k-proj-", &"FAKE".repeat(12), "\"}"].concat();
        let (status, body) = upload(&state, &claims, token.as_bytes()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({
                "error": "plaintext secrets are forbidden in coding artifacts",
                "code": ARTIFACT_SECRET_ERROR_CODE,
            }),
            "fixed error, no rule name or matched text"
        );

        // Only the ordinary upload was persisted.
        assert_eq!(load_metadata(&state, &claims, None).unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Handler-level regression: these payloads were rejected on `main`,
    /// accepted by the first real-format guard, and must be rejected again.
    #[tokio::test]
    async fn upload_rejects_review_regression_payloads_without_persisting() {
        let root = std::env::temp_dir().join(format!("artifact-test-{}", uuid::Uuid::new_v4()));
        let state = test_state(root.clone());
        let claims = IsolationClaims::from_verified("tenant-a", "project", "actor-a").unwrap();
        for (_, sample) in crate::utils::secret_scan::tests::review_regression_samples() {
            let (status, body) = upload(&state, &claims, sample.as_bytes()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "must block: {sample}");
            assert_eq!(body["code"], ARTIFACT_SECRET_ERROR_CODE);
        }
        assert!(load_metadata(&state, &claims, None).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
