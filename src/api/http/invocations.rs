//! `/v1/invocations` routes (issues #314 / #317): create, get, list, cancel
//! and events (SSE skeleton).
//!
//! Authentication and scope come only from verified JWT isolation claims
//! (no anonymous path, no `X-Identity`, no API-client keys, with or without
//! `AGENTOS_AUTH_STRICT`). Tenant, project and actor are never read from the
//! request. Persistence, the lifecycle state machine and the revision check
//! live in [`super::invocations_store`]. Cancellation registration and the
//! `succeeded` usage contract live in [`super::invocations_execution`].
//!
//! `Idempotency-Key` (#315): an optional create header of 1–255 visible
//! ASCII characters (`0x21..=0x7E`), scoped to `(tenant_id, project_id,
//! actor_id, key)` from the claims. The fingerprint is the SHA-256 of the
//! canonical JSON body (object keys sorted, no insignificant whitespace);
//! headers are not part of it. Same key and fingerprint → `200` with the
//! current resource and `Idempotent-Replayed: true`, nothing executed; a
//! different fingerprint → `409 idempotency_key_conflict` (no echo); a
//! concurrent duplicate → `409 idempotency_key_in_progress` with
//! `Retry-After`. The replay lookup runs before field validation and the
//! execution switch, so a retry whose `deadline` has since passed, or a retry
//! while execution is switched off, still replays the original.
//!
//! Gaps still tracked elsewhere:
//! - Agent definition revision registry / topology dispatch (follow-up). The
//!   agent store has no revisions, so a create carrying `agent_revision` is
//!   still `422 agent_revision_unsupported` (never silently ignored).
//! - FIFO running caps, deadline / budget / `input_ref` enforcement: #331
//!   (`invocations_enforcement` + execution bridge).
//!
//! Execution switch defaults to off (`AGENTOS_INVOCATION_EXECUTION_ENABLED`).
//! When on and a [`InvocationDispatcher`] is installed, create hands the
//! resource to the TaskExecutor bridge; otherwise create registers a cancel
//! token and leaves the resource `queued` (tests without a bridge).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use sha2::{Digest, Sha256};

use super::iam::UserIdentity;
use super::invocations_enforcement::InputRefRegistry;
use super::invocations_execution::InvocationCancellationRegistry;
use super::invocations_store::{
    invocation_not_found_response, parse_if_match, CreateOutcome, IdempotencyLookup,
    IdempotencyRegistration, Invocation, InvocationBudget, InvocationConfigError,
    InvocationInputRef, InvocationRequest, InvocationState, InvocationStore, InvocationStoreConfig,
    InvocationStoreError, NewInvocation, TransitionPatch,
};
use super::AppState;
use crate::isolation::{IsolationClaims, IsolationScopeProvenance};

/// Whole create body, in bytes.
pub(crate) const MAX_CREATE_BODY_BYTES: usize = 64 * 1024;
/// Inline `input`, as compact JSON.
pub(crate) const MAX_INPUT_BYTES: usize = 8192;
/// `metadata`, as compact JSON.
pub(crate) const MAX_METADATA_BYTES: usize = 16 * 1024;
/// Top-level `metadata` keys.
pub(crate) const MAX_METADATA_KEYS: usize = 64;
/// `agent_id` / `agent_revision` length.
const MAX_ID_BYTES: usize = 256;
/// `Idempotency-Key` length, in bytes (all visible ASCII).
pub(crate) const MAX_IDEMPOTENCY_KEY_BYTES: usize = 255;
/// Response header marking an idempotent replay.
pub(crate) const IDEMPOTENT_REPLAYED_HEADER: &str = "idempotent-replayed";
/// List page size.
pub(crate) const DEFAULT_LIST_LIMIT: usize = 20;
pub(crate) const MAX_LIST_LIMIT: usize = 100;

/// Scope and server-owned fields a caller may never send (`field_not_allowed`).
const FORBIDDEN_FIELDS: &[&str] = &[
    "tenant_id",
    "project_id",
    "actor_id",
    "id",
    "task_iri",
    "state",
    "revision",
];
/// Every create field the API accepts; anything else is `invalid_request`.
const CREATE_FIELDS: &[&str] = &[
    "prompt",
    "agent_id",
    "agent_revision",
    "input",
    "input_ref",
    "budget",
    "deadline",
    "metadata",
];
/// `agent_revision` values that would float; they are never resolved.
const FLOATING_REVISIONS: &[&str] = &["latest", "current", "head", "tip", "active", "default", "*"];

/// Receives each newly created invocation exactly once (never a replay).
/// The execution bridge (#317) implements this; tests use a counting mock.
pub(crate) trait InvocationDispatcher: Send + Sync {
    fn dispatch(&self, invocation: &Invocation);
}

/// Invocation runtime shared through `AppState`.
#[derive(Clone)]
pub(crate) struct InvocationsRuntime {
    /// `None` when the store could not be opened; every route then answers
    /// `503 invocation_store_unavailable` after authentication.
    store: Option<Arc<InvocationStore>>,
    /// Execution switch (#317). Off: create is `503 execution_disabled`.
    /// Production default is off (`AGENTOS_INVOCATION_EXECUTION_ENABLED`).
    execution_enabled: bool,
    /// Running / queued invocations that have a cancel token (#317).
    /// Keyed by invocation id only (no tenant/project dimension).
    cancellations: InvocationCancellationRegistry,
    /// Called after a successful, non-replayed create. `None` until the
    /// execution bridge (#317) installs one in production.
    dispatcher: Option<Arc<dyn InvocationDispatcher>>,
    /// Pluggable `input_ref` scheme → resolver registry (#331). Empty by
    /// default (create still `422 input_ref_unresolvable` until a deployment
    /// or test registers a scheme).
    input_refs: InputRefRegistry,
}

impl InvocationsRuntime {
    pub(crate) fn new(store: Option<Arc<InvocationStore>>, execution_enabled: bool) -> Self {
        Self {
            store,
            execution_enabled,
            cancellations: InvocationCancellationRegistry::new(),
            dispatcher: None,
            input_refs: InputRefRegistry::new(),
        }
    }

    /// Same runtime with `dispatcher` receiving new invocations.
    pub(crate) fn with_dispatcher(mut self, dispatcher: Arc<dyn InvocationDispatcher>) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    /// Shared store handle when open (`None` when unavailable).
    pub(crate) fn store(&self) -> Option<Arc<InvocationStore>> {
        self.store.clone()
    }

    /// Whether a production / test dispatcher is installed.
    #[allow(dead_code)]
    pub(crate) fn has_dispatcher(&self) -> bool {
        self.dispatcher.is_some()
    }

    /// No store and execution off. Used when the store fails to open and by
    /// test states that do not exercise invocations.
    pub(crate) fn unavailable() -> Self {
        Self::new(None, false)
    }

    /// Reads `AGENTOS_INVOCATION_EXECUTION_ENABLED` (default `false`).
    /// Accepted truthy values: `1`, `true`, `yes`, `on` (case-insensitive).
    pub(crate) fn execution_enabled_from_env() -> bool {
        std::env::var("AGENTOS_INVOCATION_EXECUTION_ENABLED")
            .ok()
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    }

    /// Opens the default store with its env config and runs restart recovery.
    /// The execution switch is read once at startup from
    /// `AGENTOS_INVOCATION_EXECUTION_ENABLED` (default off). Keep it off in
    /// production until the #317 execution bridge lands.
    ///
    /// # Panics
    ///
    /// On an invalid configuration (for example an idempotency TTL above the
    /// retention): startup fails closed instead of running with defaults.
    pub(crate) fn open_default() -> Self {
        let execution_enabled = Self::execution_enabled_from_env();
        if execution_enabled {
            tracing::warn!(
                "invocation execution enabled via AGENTOS_INVOCATION_EXECUTION_ENABLED; keep off in production unless the TaskExecutor bridge is intentionally enabled"
            );
        }
        let config = match InvocationStoreConfig::try_from_env() {
            Ok(config) => config,
            Err(InvocationConfigError(error)) => {
                panic!("invalid invocation configuration: {error}")
            }
        };
        match InvocationStore::open_with_config(InvocationStore::default_path(), config) {
            Ok((store, report)) => {
                if report.interrupted > 0 || report.swept > 0 || report.idempotency_expired > 0 {
                    tracing::info!(
                        loaded = report.loaded,
                        interrupted = report.interrupted,
                        swept = report.swept,
                        idempotency_expired = report.idempotency_expired,
                        "invocation store recovered"
                    );
                }
                Self::new(Some(Arc::new(store)), execution_enabled)
            }
            Err(error) => {
                // A corrupt file must not be replaced by an empty store.
                tracing::error!(error = %error, "invocation store unavailable");
                Self::unavailable()
            }
        }
    }

    pub(crate) fn cancellations(&self) -> &InvocationCancellationRegistry {
        &self.cancellations
    }

    /// Shared `input_ref` resolver registry (empty in production v0.12).
    pub(crate) fn input_refs(&self) -> &InputRefRegistry {
        &self.input_refs
    }

    /// Replaces the `input_ref` registry. Test-only until a deployment hook
    /// registers schemes (v0.12 ships no built-in resolver).
    #[cfg(test)]
    pub(crate) fn with_input_refs(mut self, input_refs: InputRefRegistry) -> Self {
        self.input_refs = input_refs;
        self
    }

    /// Registers a cancel token for a freshly created invocation and holds it
    /// until cancel or process shutdown (stub executor; no state drive yet).
    fn register_stub_execution(&self, id: &str, shutdown: CancellationToken) {
        let token = self.cancellations.register(id);
        let registry = self.cancellations.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = shutdown.cancelled() => {
                    token.cancel();
                }
            }
            registry.remove(&id);
        });
    }
}

/// Error body `{"error", "message"}` (plus `missing_field` for 403), kept
/// small so validation helpers can return it by value.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    missing_field: Option<&'static str>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            missing_field: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({"error": self.code, "message": self.message});
        if let Some(field) = self.missing_field {
            body["missing_field"] = json!(field);
        }
        (self.status, Json(body)).into_response()
    }
}

fn error_response(status: StatusCode, code: &'static str, message: &str) -> Response {
    ApiError::new(status, code, message).into_response()
}

fn invalid_request(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", message)
}

fn invalid_request_response(message: &str) -> Response {
    invalid_request(message).into_response()
}

fn payload_too_large(message: &str) -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", message)
}

/// Verified claims with an explicit tenant and project, or 401 / 403.
fn verified_scope(identity: &UserIdentity) -> Result<&IsolationClaims, ApiError> {
    let Some(claims) = identity.isolation_claims() else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "verified_isolation_claims_required",
            "verified isolation claims required for invocations",
        ));
    };
    if claims.provenance() != IsolationScopeProvenance::VerifiedExplicit {
        let missing = claims
            .missing_scope_field()
            .map(|field| field.as_str())
            .unwrap_or("project_id");
        let mut error = ApiError::new(
            StatusCode::FORBIDDEN,
            "claims_incomplete",
            "invocations require an explicit tenant and project in the token",
        );
        error.missing_field = Some(missing);
        return Err(error);
    }
    Ok(claims)
}

fn store_of(state: &AppState) -> Result<&Arc<InvocationStore>, ApiError> {
    state.invocations.store.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "invocation_store_unavailable",
            "invocation store is unavailable",
        )
    })
}

/// Public view of a record: everything except the internal audit trail and
/// the internal idempotency binding.
fn resource_view(invocation: &Invocation) -> Value {
    let mut value = serde_json::to_value(invocation).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.remove("audit_events");
        map.remove("idempotency");
    }
    value
}

fn resource_response(status: StatusCode, invocation: &Invocation) -> Response {
    let mut response = (status, Json(resource_view(invocation))).into_response();
    response
        .headers_mut()
        .insert(header::ETAG, invocation.etag());
    response
}

fn compact_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

fn optional<'a>(body: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    body.get(key).filter(|value| !value.is_null())
}

fn bounded_id(value: &Value, field: &str) -> Result<String, ApiError> {
    let Some(text) = value.as_str() else {
        return Err(invalid_request(format!("{field} must be a string")));
    };
    if text.trim().is_empty() || text.len() > MAX_ID_BYTES {
        return Err(invalid_request(format!(
            "{field} must be 1-{MAX_ID_BYTES} bytes"
        )));
    }
    Ok(text.to_string())
}

fn has_uri_scheme(uri: &str) -> bool {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !rest.is_empty()
}

fn parse_input_ref(value: &Value) -> Result<InvocationInputRef, ApiError> {
    let Some(object) = value.as_object() else {
        return Err(invalid_request("input_ref must be an object"));
    };
    if object.keys().any(|key| key != "uri" && key != "sha256") {
        return Err(invalid_request("input_ref accepts only uri and sha256"));
    }
    let uri = object.get("uri").and_then(Value::as_str);
    let sha256 = object.get("sha256").and_then(Value::as_str);
    let (Some(uri), Some(sha256)) = (uri, sha256) else {
        return Err(invalid_request("input_ref requires uri and sha256 strings"));
    };
    if !has_uri_scheme(uri) {
        return Err(invalid_request("input_ref.uri must be <scheme>://..."));
    }
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid_request(
            "input_ref.sha256 must be 64 lowercase hex characters",
        ));
    }
    Ok(InvocationInputRef {
        uri: uri.to_string(),
        sha256: sha256.to_string(),
    })
}

fn parse_budget(value: &Value) -> Result<InvocationBudget, ApiError> {
    let Some(object) = value.as_object() else {
        return Err(invalid_request("budget must be an object"));
    };
    let mut budget = InvocationBudget::default();
    for (key, member) in object {
        let slot = match key.as_str() {
            "max_tokens" => &mut budget.max_tokens,
            "max_tool_calls" => &mut budget.max_tool_calls,
            "max_cost" => &mut budget.max_cost,
            _ => return Err(invalid_request("budget has an unknown member")),
        };
        match member.as_u64().filter(|n| *n >= 1) {
            Some(n) => *slot = Some(n),
            None => return Err(invalid_request("budget members must be positive integers")),
        }
    }
    Ok(budget)
}

fn parse_deadline(value: &Value) -> Result<String, ApiError> {
    let Some(text) = value.as_str() else {
        return Err(invalid_request("deadline must be an RFC 3339 string"));
    };
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(text) else {
        return Err(invalid_request(
            "deadline must be RFC 3339 with a UTC offset",
        ));
    };
    if at.with_timezone(&chrono::Utc) <= chrono::Utc::now() {
        return Err(invalid_request("deadline must be in the future"));
    }
    Ok(text.to_string())
}

/// Validates a create body into the stored request. Pure: no I/O, no store.
/// Errors are `400` / `413` / `422`; the order is scope fields, unknown
/// fields, per-field rules, then cross-field rules.
pub(crate) fn parse_create_request(raw: &[u8]) -> Result<InvocationRequest, ApiError> {
    if raw.len() > MAX_CREATE_BODY_BYTES {
        return Err(payload_too_large("request body exceeds 64 KiB"));
    }
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(raw) else {
        return Err(invalid_request("request body must be a JSON object"));
    };
    if body
        .keys()
        .any(|key| FORBIDDEN_FIELDS.contains(&key.as_str()))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "field_not_allowed",
            "scope and server fields are taken from the token and the server",
        ));
    }
    if body
        .keys()
        .any(|key| !CREATE_FIELDS.contains(&key.as_str()))
    {
        return Err(invalid_request("request body has an unknown field"));
    }

    let mut request = InvocationRequest::default();
    if let Some(prompt) = optional(&body, "prompt") {
        let Some(text) = prompt.as_str() else {
            return Err(invalid_request("prompt must be a string"));
        };
        request.prompt = Some(text.to_string());
    }
    if let Some(agent_id) = optional(&body, "agent_id") {
        request.agent_id = Some(bounded_id(agent_id, "agent_id")?);
    }
    if let Some(revision) = optional(&body, "agent_revision") {
        let revision = bounded_id(revision, "agent_revision")?;
        if FLOATING_REVISIONS
            .iter()
            .any(|word| revision.trim().eq_ignore_ascii_case(word))
        {
            return Err(invalid_request(
                "agent_revision must be an exact revision, not a floating name",
            ));
        }
        if request.agent_id.is_none() {
            return Err(invalid_request("agent_revision requires agent_id"));
        }
        request.agent_revision = Some(revision);
    }
    if let Some(input) = optional(&body, "input") {
        if compact_len(input) > MAX_INPUT_BYTES {
            return Err(payload_too_large("input exceeds 8192 bytes"));
        }
        request.input = Some(input.clone());
    }
    if let Some(input_ref) = optional(&body, "input_ref") {
        if request.input.is_some() {
            return Err(invalid_request(
                "input and input_ref are mutually exclusive",
            ));
        }
        request.input_ref = Some(parse_input_ref(input_ref)?);
    }
    if let Some(budget) = optional(&body, "budget") {
        request.budget = Some(parse_budget(budget)?);
    }
    if let Some(deadline) = optional(&body, "deadline") {
        request.deadline = Some(parse_deadline(deadline)?);
    }
    if let Some(metadata) = optional(&body, "metadata") {
        let Some(map) = metadata.as_object() else {
            return Err(invalid_request("metadata must be an object"));
        };
        if map.len() > MAX_METADATA_KEYS {
            return Err(payload_too_large("metadata exceeds 64 top-level keys"));
        }
        if compact_len(metadata) > MAX_METADATA_BYTES {
            return Err(payload_too_large("metadata exceeds 16 KiB"));
        }
        request.metadata = map.clone();
    }
    let has_prompt = request
        .prompt
        .as_deref()
        .is_some_and(|prompt| !prompt.trim().is_empty());
    if !has_prompt && request.input.is_none() && request.input_ref.is_none() {
        return Err(invalid_request(
            "prompt is required unless input or input_ref is set",
        ));
    }
    // Scheme registration is checked at create time against the runtime
    // registry (#331); parse only validates shape here.
    Ok(request)
}

/// Optional `Idempotency-Key`: absent → `None`; present → 1–255 visible
/// ASCII characters (`0x21..=0x7E`), exactly one header. Anything else is
/// `400 invalid_idempotency_key`; the key is never echoed.
pub(crate) fn parse_idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_idempotency_key",
            "Idempotency-Key must be 1-255 visible ASCII characters",
        )
    };
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(invalid());
    }
    let raw = value.as_bytes();
    if raw.is_empty()
        || raw.len() > MAX_IDEMPOTENCY_KEY_BYTES
        || !raw.iter().all(|b| (0x21..=0x7e).contains(b))
    {
        return Err(invalid());
    }
    // Every byte is visible ASCII, so this is valid UTF-8.
    Ok(Some(String::from_utf8_lossy(raw).into_owned()))
}

/// Writes `value` as canonical JSON: object keys sorted by their UTF-8
/// bytes, no insignificant whitespace, scalars as `serde_json` prints them.
/// Independent of `serde_json`'s map ordering features.
fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (index, (key, member)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical_json(member, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Lowercase hex SHA-256 of the canonical create body, or `None` when the
/// body is too large or not JSON (validation rejects those anyway). Only
/// this digest is stored, never the body.
pub(crate) fn request_fingerprint(raw: &[u8]) -> Option<String> {
    if raw.len() > MAX_CREATE_BODY_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(raw).ok()?;
    let mut canonical = String::with_capacity(raw.len());
    write_canonical_json(&value, &mut canonical);
    Some(hex::encode(Sha256::digest(canonical.as_bytes())))
}

/// First 16 characters of a key, for debug logs only.
fn key_for_log(key: &str) -> &str {
    &key[..key.len().min(16)]
}

fn replay_response(invocation: &Invocation) -> Response {
    let mut response = resource_response(StatusCode::OK, invocation);
    insert_location(&mut response, invocation);
    response.headers_mut().insert(
        header::HeaderName::from_static(IDEMPOTENT_REPLAYED_HEADER),
        HeaderValue::from_static("true"),
    );
    response
}

fn insert_location(response: &mut Response, invocation: &Invocation) {
    if let Ok(location) = HeaderValue::from_str(&format!("/v1/invocations/{}", invocation.id)) {
        response.headers_mut().insert(header::LOCATION, location);
    }
}

/// `agent_id` must name a user agent definition in the caller's scope.
/// Unknown ids and other scopes get the same body.
async fn resolve_agent(
    state: &AppState,
    claims: &IsolationClaims,
    request: &InvocationRequest,
) -> Result<(), ApiError> {
    let Some(agent_id) = request.agent_id.as_deref() else {
        return Ok(());
    };
    let found = state.user_agents.read().await.iter().any(|agent| {
        agent.get("id").and_then(Value::as_str) == Some(agent_id)
            && agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
            && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
    });
    if !found {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_not_found",
            "agent_id does not name an agent definition in this scope",
        ));
    }
    if request.agent_revision.is_some() {
        // Agent definitions still carry no revision field in-tree (#331): a
        // pin can neither match nor be checked. Reject rather than ignore it
        // (never invent fake revision pinning).
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_revision_unsupported",
            "agent revisions are not available yet",
        ));
    }
    Ok(())
}

/// `POST /v1/invocations` with optional `Idempotency-Key`.
///
/// Order: claims (401/403) → key syntax (400) → replay lookup on the
/// fingerprint (200 replay / 409 conflict) → in-flight reservation (409 in
/// progress) → body validation (400/413/422) → agent (422) → store (503) →
/// execution switch (503) → atomic create of record + binding. Every
/// rejection writes nothing, so the same key can be used again afterwards.
pub(crate) async fn create_invocation_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let claims = match verified_scope(&identity) {
        Ok(claims) => claims,
        Err(error) => return error.into_response(),
    };
    let key = match parse_idempotency_key(&headers) {
        Ok(key) => key,
        Err(error) => return error.into_response(),
    };
    // With a key and a parsable body: replay before anything that can change
    // over time (deadline, agent registry, execution switch).
    let mut registration = None;
    let mut _reservation = None;
    if let (Some(key), Some(fingerprint)) = (key.as_deref(), request_fingerprint(&body)) {
        let store = match store_of(&state) {
            Ok(store) => store,
            Err(error) => return error.into_response(),
        };
        match store
            .find_idempotent_for_claims(claims, key, &fingerprint)
            .await
        {
            IdempotencyLookup::Replay(invocation) => {
                tracing::debug!(idempotency_key = key_for_log(key), "invocation replayed");
                return replay_response(&invocation);
            }
            IdempotencyLookup::Conflict => {
                tracing::debug!(idempotency_key = key_for_log(key), "idempotency conflict");
                return InvocationStoreError::IdempotencyKeyConflict.into_response();
            }
            IdempotencyLookup::Miss => {}
        }
        _reservation = match store.reserve_idempotency_key(claims, key) {
            Ok(reservation) => Some(reservation),
            Err(error) => {
                tracing::debug!(
                    idempotency_key = key_for_log(key),
                    "idempotency in progress"
                );
                return error.into_response();
            }
        };
        registration = Some(IdempotencyRegistration {
            key: key.to_string(),
            fingerprint,
        });
    }
    let request = match parse_create_request(&body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    if let Some(input_ref) = request.input_ref.as_ref() {
        let scheme = super::invocations_enforcement::InputRefRegistry::scheme_of(&input_ref.uri);
        let registered = scheme.is_some_and(|s| state.invocations.input_refs().has_scheme(s));
        if !registered {
            return ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "input_ref_unresolvable",
                "no resolver is registered for the input_ref scheme",
            )
            .into_response();
        }
    }
    if let Err(error) = resolve_agent(&state, claims, &request).await {
        return error.into_response();
    }
    let store = match store_of(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    if !state.invocations.execution_enabled {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "execution_disabled",
            "invocation execution is disabled on this server",
        );
    }
    let new = NewInvocation {
        request,
        task_iri: None,
        idempotency: registration,
    };
    match store.create_idempotent_for_claims(claims, new).await {
        Ok(CreateOutcome::Created(invocation)) => {
            // Real bridge (#317) owns the cancel token + state drive when a
            // dispatcher is installed. Otherwise keep the stub token so cancel
            // still signals something in tests without an executor.
            if let Some(dispatcher) = &state.invocations.dispatcher {
                dispatcher.dispatch(&invocation);
            } else {
                state
                    .invocations
                    .register_stub_execution(&invocation.id, state.shutdown.clone());
            }
            let mut response = resource_response(StatusCode::ACCEPTED, &invocation);
            insert_location(&mut response, &invocation);
            response
        }
        Ok(CreateOutcome::Replayed(invocation)) => replay_response(&invocation),
        Err(error) => error.into_response(),
    }
}

/// `GET /v1/invocations/:id`
pub(crate) async fn get_invocation_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(id): Path<String>,
) -> Response {
    let claims = match verified_scope(&identity) {
        Ok(claims) => claims,
        Err(error) => return error.into_response(),
    };
    let store = match store_of(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    match store.get_for_claims(claims, &id).await {
        Ok(invocation) => resource_response(StatusCode::OK, &invocation),
        Err(error) => error.into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    limit: Option<String>,
    after: Option<String>,
    state: Option<String>,
}

fn encode_cursor(invocation: &Invocation) -> String {
    let raw = json!([invocation.created_at, invocation.id]).to_string();
    URL_SAFE_NO_PAD.encode(raw)
}

fn decode_cursor(cursor: &str) -> Option<(String, String)> {
    let raw = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let (created_at, id): (String, String) = serde_json::from_slice(&raw).ok()?;
    Some((created_at, id))
}

/// `GET /v1/invocations?limit=&after=&state=`
pub(crate) async fn list_invocations_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    query: Option<Query<ListQuery>>,
) -> Response {
    let claims = match verified_scope(&identity) {
        Ok(claims) => claims,
        Err(error) => return error.into_response(),
    };
    let Some(Query(query)) = query else {
        return invalid_request_response("invalid query string");
    };
    let limit = match query.limit.as_deref() {
        None => DEFAULT_LIST_LIMIT,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=MAX_LIST_LIMIT).contains(&n) => n,
            _ => return invalid_request_response("limit must be an integer from 1 to 100"),
        },
    };
    let filter = match query.state.as_deref() {
        None => None,
        Some(raw) => match InvocationState::ALL.iter().find(|s| s.as_str() == raw) {
            Some(state) => Some(*state),
            None => return invalid_request_response("unknown state filter"),
        },
    };
    let after = match query.after.as_deref() {
        None => None,
        Some(raw) => match decode_cursor(raw) {
            Some(position) => Some(position),
            None => return invalid_request_response("invalid cursor"),
        },
    };
    let store = match store_of(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    // Newest first, `id` as tie-breaker (same order as the store).
    let listed = store.list_for_claims(claims, filter).await;
    let start = match &after {
        None => 0,
        Some((created_at, id)) => listed
            .iter()
            .position(|inv| (&inv.created_at, &inv.id) < (created_at, id))
            .unwrap_or(listed.len()),
    };
    let page: Vec<&Invocation> = listed.iter().skip(start).take(limit).collect();
    let has_more = start + page.len() < listed.len();
    let next_cursor = if has_more {
        page.last().map(|inv| encode_cursor(inv))
    } else {
        None
    };
    Json(json!({
        "object": "list",
        "data": page.iter().map(|inv| resource_view(inv)).collect::<Vec<_>>(),
        "has_more": has_more,
        "next_cursor": next_cursor,
    }))
    .into_response()
}

/// `POST /v1/invocations/:id/cancel` with optional `If-Match: "<revision>"`.
/// Any actor in the scope may read; only the creating actor or a DA may
/// cancel (`403 cancel_not_permitted`). `200` when the result is
/// `cancelled`, `202` when it is `cancel_requested`.
pub(crate) async fn cancel_invocation_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let claims = match verified_scope(&identity) {
        Ok(claims) => claims,
        Err(error) => return error.into_response(),
    };
    let expected_revision = match parse_if_match(&headers) {
        Ok(revision) => revision,
        Err(error) => return error.into_response(),
    };
    let store = match store_of(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    // A second attempt covers a state change between the read and the write
    // (for example queued -> running), which changes the cancel target.
    let mut attempts = 0;
    loop {
        attempts += 1;
        let current = match store.get_for_claims(claims, &id).await {
            Ok(invocation) => invocation,
            Err(error) => return error.into_response(),
        };
        if current.actor_id != claims.actor_id() && !identity.has_role("DA") {
            return error_response(
                StatusCode::FORBIDDEN,
                "cancel_not_permitted",
                "only the creating actor or a DA may cancel this invocation",
            );
        }
        let Some(target) = current.state.cancel_target() else {
            return InvocationStoreError::IllegalTransition {
                from: current.state,
                to: InvocationState::Cancelled,
            }
            .into_response();
        };
        match store
            .transition_outcome_for_claims(
                claims,
                &id,
                expected_revision,
                target,
                TransitionPatch::default(),
            )
            .await
        {
            Ok(outcome) => {
                // Signal the stub / future executor. Queued→cancelled also
                // drops the registry entry via the token's wait task.
                state.invocations.cancellations().cancel(&id);
                if outcome.invocation.state.is_terminal() {
                    state.invocations.cancellations().remove(&id);
                }
                let status = if outcome.invocation.state == InvocationState::Cancelled {
                    StatusCode::OK
                } else {
                    StatusCode::ACCEPTED
                };
                return resource_response(status, &outcome.invocation);
            }
            Err(InvocationStoreError::IllegalTransition { .. }) if attempts < 2 => continue,
            Err(InvocationStoreError::NotFound) => return invocation_not_found_response(),
            Err(error) => return error.into_response(),
        }
    }
}

/// `GET /v1/invocations/:id/events` — SSE (#317 / docs/29 §8.1).
///
/// Emits a `state` snapshot (with `snapshot: true` and the resource view),
/// then watches the store (and the event bus when `task_iri` is bound) until
/// the invocation is terminal (or shutdown). On `Lagged`, emits `resync` so
/// the client re-GETs. On terminal: optional `result` / `error`, then close.
pub(crate) async fn events_invocation_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(id): Path<String>,
) -> Response {
    let claims = match verified_scope(&identity) {
        Ok(claims) => claims.clone(),
        Err(error) => return error.into_response(),
    };
    let store = match store_of(&state) {
        Ok(store) => store.clone(),
        Err(error) => return error.into_response(),
    };
    let initial = match store.get_for_claims(&claims, &id).await {
        Ok(invocation) => invocation,
        Err(error) => return error.into_response(),
    };

    let shutdown = state.shutdown.clone();
    let event_bus = state.core.events.clone();
    let stream = async_stream::stream! {
        let mut seq: u64 = 0;
        let mut last_revision = initial.revision;
        let mut last_state = initial.state;
        let mut task_iri = initial.task_iri.clone();
        let mut rx = event_bus.subscribe();

        yield Ok::<Event, Infallible>(sse_state_event(
            &initial,
            None,
            true,
            &mut seq,
        ));

        if initial.state.is_terminal() {
            if let Some(event) = sse_terminal_payload(&initial, &mut seq) {
                yield Ok(event);
            }
            return;
        }

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }
                bus = rx.recv() => {
                    match bus {
                        Ok(event) => {
                            let Some(iri) = task_iri.as_deref() else { continue; };
                            if event.task_iri != iri {
                                continue;
                            }
                            // Progress is not persisted; surface a lightweight hint.
                            if event.event_type != "TASK_COMPLETED"
                                && event.event_type != "TASK_FAILED"
                            {
                                let data = json!({
                                    "invocation_id": id,
                                    "revision": last_revision,
                                    "at": chrono::Utc::now().to_rfc3339(),
                                    "message": event.event_type,
                                });
                                yield Ok(Event::default()
                                    .event("progress")
                                    .id(sse_event_id(last_revision, &mut seq))
                                    .data(data.to_string()));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let data = json!({
                                "invocation_id": id,
                                "revision": last_revision,
                                "at": chrono::Utc::now().to_rfc3339(),
                            });
                            yield Ok(Event::default()
                                .event("resync")
                                .id(sse_event_id(last_revision, &mut seq))
                                .data(data.to_string()));
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    let current = match store.get_for_claims(&claims, &id).await {
                        Ok(invocation) => invocation,
                        Err(_) => break,
                    };
                    if current.task_iri.is_some() {
                        task_iri = current.task_iri.clone();
                    }
                    if current.revision != last_revision || current.state != last_state {
                        let previous = last_state;
                        last_revision = current.revision;
                        last_state = current.state;
                        yield Ok(sse_state_event(
                            &current,
                            Some(previous),
                            false,
                            &mut seq,
                        ));
                        if current.state.is_terminal() {
                            if let Some(event) = sse_terminal_payload(&current, &mut seq) {
                                yield Ok(event);
                            }
                            break;
                        }
                    }
                }
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn sse_event_id(revision: u64, seq: &mut u64) -> String {
    let id = format!("{revision}.{seq}");
    *seq = seq.saturating_add(1);
    id
}

fn sse_state_event(
    invocation: &Invocation,
    previous_state: Option<InvocationState>,
    snapshot: bool,
    seq: &mut u64,
) -> Event {
    let mut data = json!({
        "invocation_id": invocation.id,
        "revision": invocation.revision,
        "at": invocation.updated_at,
        "state": invocation.state.as_str(),
        "previous_state": previous_state.map(|s| s.as_str()),
        "snapshot": snapshot,
    });
    if snapshot {
        data["invocation"] = resource_view(invocation);
    }
    Event::default()
        .event("state")
        .id(sse_event_id(invocation.revision, seq))
        .data(data.to_string())
}

fn sse_terminal_payload(invocation: &Invocation, seq: &mut u64) -> Option<Event> {
    match invocation.state {
        InvocationState::Succeeded => {
            let data = json!({
                "invocation_id": invocation.id,
                "revision": invocation.revision,
                "at": invocation.updated_at,
                "state": "succeeded",
                "result": invocation.result,
            });
            Some(
                Event::default()
                    .event("result")
                    .id(sse_event_id(invocation.revision, seq))
                    .data(data.to_string()),
            )
        }
        InvocationState::Failed => {
            let data = json!({
                "invocation_id": invocation.id,
                "revision": invocation.revision,
                "at": invocation.updated_at,
                "state": "failed",
                "error": invocation.error,
                "usage": invocation.result.as_ref().and_then(|r| r.usage.clone()),
            });
            Some(
                Event::default()
                    .event("error")
                    .id(sse_event_id(invocation.revision, seq))
                    .data(data.to_string()),
            )
        }
        InvocationState::Cancelled => None,
        _ => None,
    }
}

#[cfg(test)]
#[path = "invocations_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "invocations_idempotency_tests.rs"]
mod idempotency_tests;

#[cfg(test)]
#[path = "invocations_bridge_tests.rs"]
mod bridge_tests;

#[cfg(test)]
#[path = "invocations_enforcement_tests.rs"]
mod enforcement_tests;
