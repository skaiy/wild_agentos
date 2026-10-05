//! Claims-scoped lifecycle store for `/v1/invocations` (issue #316).
//!
//! This module owns the invocation state machine, the `revision` optimistic
//! concurrency check (compare-and-set), persistence and restart recovery. It
//! deliberately has no HTTP routes: the create/get/list/cancel routes (#314),
//! `Idempotency-Key` handling (#315) and the execution bridge (#317) build on
//! the store interface exposed here.
//!
//! # Storage
//!
//! Records live in a `tokio::sync::RwLock<Vec<Invocation>>` mirrored to one
//! JSON file that is replaced by an atomic rename, following the existing
//! claims-scoped stores in this crate. Every write:
//!
//! 1. takes the single write lock (writes are serialized per process),
//! 2. applies the change to a cloned snapshot,
//! 3. persists the snapshot (`<file>.tmp` + rename), and
//! 4. only then publishes the snapshot to memory.
//!
//! A failed write therefore leaves both memory and disk at the previous state.
//! The whole file is rewritten per write, so the store is sized for at most
//! [`MAX_STORED_INVOCATIONS`] records per process; creates beyond that are
//! rejected with [`InvocationStoreError::StoreFull`] instead of degrading.
//!
//! # Retention
//!
//! Terminal records are kept for [`InvocationStoreConfig::retention`]
//! (default [`DEFAULT_RETENTION_DAYS`] days, env
//! [`RETENTION_DAYS_ENV`]), measured from `completed_at`. Expired terminal
//! records are swept on [`InvocationStore::open`] (after restart recovery),
//! opportunistically inside every create, and on demand through
//! [`InvocationStore::sweep_expired`]. Non-terminal records are never swept.
//! A sweep that removes nothing writes nothing, and a sweep that removes
//! records persists with the same atomic replace as any other write. The
//! [`MAX_STORED_INVOCATIONS`] cap counts the records left after the sweep.
//!
//! # Active limit
//!
//! A create is rejected with [`InvocationStoreError::TooManyActive`]
//! (`429 too_many_active`, `Retry-After: 5`) when the tenant/project scope
//! already has [`InvocationStoreConfig::max_active_per_scope`] non-terminal
//! invocations (default [`DEFAULT_MAX_ACTIVE_PER_SCOPE`], env
//! [`MAX_ACTIVE_ENV`]). The count and the insert happen under the same write
//! lock, and nothing is persisted for a rejected create.
//!
//! # Recovery
//!
//! [`InvocationStore::open`] moves every non-terminal record (`queued`,
//! `running`, `cancel_requested`) to `failed` with `error.code =
//! "interrupted"`, bumps its revision, records an audit event with actor
//! `system` and persists the result before the store is returned. Nothing is
//! re-run automatically.
//!
//! Restart recovery is the single privileged write path: it is the only code
//! that bypasses [`InvocationState::permits`] (for example `queued → failed`
//! is not an unconditional lifecycle edge), and it can only move a non-terminal record to
//! `failed/interrupted`. Terminal records are never touched. Every other write
//! goes through [`InvocationStore::transition_for_claims`].
//!
//! # Same-state repeats
//!
//! A transition whose target equals the current state (a duplicate worker
//! delivery, or a cancel retried on a `cancel_requested` / `cancelled`
//! invocation) is an idempotent success: nothing is written, the revision is
//! not bumped, no audit event is added, any patch is ignored and the current
//! record is returned. Following RFC 9110 §13.1.1, this holds even when the
//! caller's `If-Match` revision is stale, because the requested final state is
//! already reflected and a no-op cannot lose an update. Moving a terminal
//! record into a *different* state stays `IllegalTransition`.
//!
//! # `If-Match`
//!
//! [`parse_if_match`] accepts one strong tag `"<revision>"` or `*`. Anything
//! else (weak tags, lists, unquoted or non-numeric values) is
//! [`InvalidIfMatch`], which the routes map to `400 invalid_if_match`.

// The store is consumed by the routes and execution bridge that land in
// follow-up sub-issues of #313; until then only the unit tests exercise it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::RwLock;

use crate::isolation::IsolationClaims;

/// Upper bound on records kept by one store (whole-file rewrite per write).
pub(crate) const MAX_STORED_INVOCATIONS: usize = 10_000;
/// Upper bound on audit events kept per invocation; oldest are dropped.
pub(crate) const MAX_AUDIT_EVENTS: usize = 32;
/// Upper bound on a persisted error message, in bytes.
pub(crate) const MAX_ERROR_MESSAGE_BYTES: usize = 1024;
/// Error code recorded for invocations interrupted by a process restart.
pub(crate) const INTERRUPTED_ERROR_CODE: &str = "interrupted";
/// Error code for an invocation stopped by its `deadline`. It is also the
/// only reason that unlocks the `queued → failed` edge.
pub(crate) const DEADLINE_EXCEEDED_ERROR_CODE: &str = "deadline_exceeded";
/// Actor recorded in audit events written by the store itself.
pub(crate) const SYSTEM_ACTOR_ID: &str = "system";

/// Default retention of terminal records, in days.
pub(crate) const DEFAULT_RETENTION_DAYS: u64 = 7;
/// Env override for the terminal-record retention, in whole days (≥ 1).
pub(crate) const RETENTION_DAYS_ENV: &str = "AGENTOS_INVOCATION_RETENTION_DAYS";
/// Default limit of non-terminal invocations per tenant/project scope.
pub(crate) const DEFAULT_MAX_ACTIVE_PER_SCOPE: usize = 32;
/// Env override for the per-scope active limit (≥ 1).
pub(crate) const MAX_ACTIVE_ENV: &str = "AGENTOS_INVOCATION_MAX_ACTIVE";
/// `Retry-After` seconds sent with `429 too_many_active`.
pub(crate) const TOO_MANY_ACTIVE_RETRY_AFTER_SECS: u64 = 5;

const STORE_FILE_NAME: &str = "invocations.json";

/// Store limits. [`Self::from_env`] reads the documented env overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvocationStoreConfig {
    /// How long a terminal record is kept after `completed_at`.
    pub retention: chrono::Duration,
    /// Maximum non-terminal invocations per tenant/project scope.
    pub max_active_per_scope: usize,
}

impl Default for InvocationStoreConfig {
    fn default() -> Self {
        Self {
            retention: chrono::Duration::days(DEFAULT_RETENTION_DAYS as i64),
            max_active_per_scope: DEFAULT_MAX_ACTIVE_PER_SCOPE,
        }
    }
}

impl InvocationStoreConfig {
    /// Defaults overridden by [`RETENTION_DAYS_ENV`] and [`MAX_ACTIVE_ENV`].
    /// Missing, unparsable or zero values keep the default.
    pub(crate) fn from_env() -> Self {
        let positive = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .filter(|value| *value >= 1)
        };
        let mut config = Self::default();
        if let Some(days) = positive(RETENTION_DAYS_ENV) {
            config.retention = chrono::Duration::days(days.min(36_500) as i64);
        }
        if let Some(limit) = positive(MAX_ACTIVE_ENV) {
            config.max_active_per_scope = usize::try_from(limit).unwrap_or(usize::MAX);
        }
        config
    }
}

/// Lifecycle state of an invocation (epic #313 §3.5).
///
/// ```text
/// queued ──► running ──► succeeded
///   │  │        ├──────► failed
///   │  │        └──► cancel_requested ──► cancelled
///   │  │                     ├──────────► succeeded
///   │  │                     └──────────► failed
///   │  └──────────────────────────────► failed (deadline_exceeded only)
///   └──────────────────────────────────► cancelled
/// ```
///
/// `cancel_requested → succeeded | failed` records the real outcome when
/// execution finishes before the cancel request takes effect.
///
/// `queued → failed` is a *conditional* edge: it is not in [`Self::permits`]
/// and is accepted only when the transition patch carries
/// `error.code = "deadline_exceeded"` (see [`Self::permits_with`]). Any other
/// `queued → failed` request is `IllegalTransition`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InvocationState {
    Queued,
    Running,
    CancelRequested,
    Succeeded,
    Failed,
    Cancelled,
}

impl InvocationState {
    pub(crate) const ALL: [Self; 6] = [
        Self::Queued,
        Self::Running,
        Self::CancelRequested,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// Whether `self -> next` is an edge of the lifecycle graph. Terminal
    /// states have no outgoing edges and there are no self-loops; same-state
    /// repeats are handled as no-ops by the store, not as edges.
    pub(crate) fn permits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::CancelRequested
                )
                | (
                    Self::CancelRequested,
                    Self::Cancelled | Self::Succeeded | Self::Failed
                )
        )
    }

    /// [`Self::permits`] plus the conditional `queued → failed` edge, which
    /// only a deadline expiry (`error.code = "deadline_exceeded"`) may take.
    pub(crate) fn permits_with(self, next: Self, error: Option<&InvocationErrorInfo>) -> bool {
        self.permits(next)
            || (self == Self::Queued
                && next == Self::Failed
                && error.is_some_and(|e| e.code == DEADLINE_EXCEEDED_ERROR_CODE))
    }

    /// The state a cancel request moves to, or `None` when cancel would leave
    /// a terminal outcome (`succeeded` / `failed`). A repeated cancel targets
    /// the current state (`cancel_requested` / `cancelled`), which the store
    /// answers as an idempotent no-op.
    pub(crate) fn cancel_target(self) -> Option<Self> {
        match self {
            Self::Queued => Some(Self::Cancelled),
            Self::Running | Self::CancelRequested => Some(Self::CancelRequested),
            Self::Cancelled => Some(Self::Cancelled),
            Self::Succeeded | Self::Failed => None,
        }
    }
}

/// Caller-supplied create fields, echoed back unchanged as `request`.
/// The remaining create fields (`agent_revision`, `input`, `input_ref`,
/// `budget`, `deadline`) are added with the create route (#314).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct InvocationRequest {
    pub prompt: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct InvocationResult {
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<Value>,
    /// Metering for the run; the same numbers the server checks `budget`
    /// against. Absent when the execution path reported nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<InvocationUsage>,
}

/// Generic usage of one invocation (`result.usage`). Every field is optional;
/// `cost` is an integer in micro-USD (1 USD = 1_000_000).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct InvocationUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<InvocationToolCallUsage>>,
}

/// One tool call in [`InvocationUsage::tool_calls`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct InvocationToolCallUsage {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct InvocationErrorInfo {
    pub code: String,
    pub message: String,
}

impl InvocationErrorInfo {
    /// Builds a persisted error, truncating the message on a char boundary.
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_MESSAGE_BYTES {
            let mut end = MAX_ERROR_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self {
            code: code.into(),
            message,
        }
    }
}

/// One lifecycle transition. Never carries prompt, result or secret content.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct InvocationAuditEvent {
    pub at: String,
    pub from: Option<InvocationState>,
    pub to: InvocationState,
    pub revision: u64,
    pub actor_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub id: String,
    pub object: String,
    pub tenant_id: String,
    pub project_id: String,
    pub actor_id: String,
    pub state: InvocationState,
    pub revision: u64,
    pub request: InvocationRequest,
    #[serde(default)]
    pub task_iri: Option<String>,
    #[serde(default)]
    pub result: Option<InvocationResult>,
    #[serde(default)]
    pub error: Option<InvocationErrorInfo>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub audit_events: Vec<InvocationAuditEvent>,
}

impl Invocation {
    fn is_in_scope(&self, claims: &IsolationClaims) -> bool {
        self.tenant_id == claims.tenant_id() && self.project_id == claims.project_id()
    }

    fn push_audit(&mut self, event: InvocationAuditEvent) {
        self.audit_events.push(event);
        if self.audit_events.len() > MAX_AUDIT_EVENTS {
            let overflow = self.audit_events.len() - MAX_AUDIT_EVENTS;
            self.audit_events.drain(..overflow);
        }
    }

    /// Strong entity tag for this revision, e.g. `"3"`.
    pub(crate) fn etag(&self) -> HeaderValue {
        etag_for_revision(self.revision)
    }
}

/// Server-side fields for a new invocation. Scope and actor always come from
/// the verified claims passed to [`InvocationStore::create_for_claims`].
#[derive(Debug, Clone, Default)]
pub(crate) struct NewInvocation {
    pub request: InvocationRequest,
    pub task_iri: Option<String>,
    pub idempotency_key: Option<String>,
}

/// Result of [`InvocationStore::transition_outcome_for_claims`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransitionOutcome {
    /// The record after the call (unchanged for a same-state repeat).
    pub invocation: Invocation,
    /// `false` when the target equalled the current state and nothing was
    /// written (no revision bump, no audit event, patch ignored).
    pub changed: bool,
}

/// Optional fields written together with a transition.
#[derive(Debug, Clone, Default)]
pub(crate) struct TransitionPatch {
    pub result: Option<InvocationResult>,
    pub error: Option<InvocationErrorInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InvocationStoreError {
    /// Unknown id or outside the caller's tenant/project scope.
    NotFound,
    /// `expected_revision` did not match the stored revision.
    RevisionConflict { current: u64 },
    /// The lifecycle graph has no `from -> to` edge.
    IllegalTransition {
        from: InvocationState,
        to: InvocationState,
    },
    /// The store already holds [`MAX_STORED_INVOCATIONS`] records.
    StoreFull,
    /// The scope already has `max_active_per_scope` non-terminal records.
    TooManyActive,
    /// Persisting the snapshot failed; memory and disk are unchanged.
    Persistence(String),
}

impl std::fmt::Display for InvocationStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "invocation not found"),
            Self::RevisionConflict { current } => {
                write!(f, "revision conflict (current revision {current})")
            }
            Self::IllegalTransition { from, to } => {
                write!(f, "illegal transition {} -> {}", from.as_str(), to.as_str())
            }
            Self::StoreFull => write!(f, "invocation store is full"),
            Self::TooManyActive => write!(f, "too many active invocations in scope"),
            Self::Persistence(error) => write!(f, "persist invocations: {error}"),
        }
    }
}

impl std::error::Error for InvocationStoreError {}

/// The single not-found body for this resource: unknown ids and other scopes
/// must be indistinguishable.
pub(crate) fn invocation_not_found_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "not_found", "message": "invocation not found"})),
    )
        .into_response()
}

impl IntoResponse for InvocationStoreError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => invocation_not_found_response(),
            Self::RevisionConflict { current } => {
                let mut response = (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "revision_conflict",
                        "message": "the invocation was modified; re-read and retry",
                        "current_revision": current,
                    })),
                )
                    .into_response();
                response
                    .headers_mut()
                    .insert(header::ETAG, etag_for_revision(current));
                response
            }
            Self::IllegalTransition { from, to } => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "illegal_transition",
                    "message": format!(
                        "cannot move an invocation from {} to {}",
                        from.as_str(),
                        to.as_str()
                    ),
                })),
            )
                .into_response(),
            Self::StoreFull => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "invocation_store_full",
                    "message": "invocation store capacity reached",
                })),
            )
                .into_response(),
            Self::TooManyActive => {
                let mut response = (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "error": "too_many_active",
                        "message": "too many active invocations; retry later",
                    })),
                )
                    .into_response();
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    HeaderValue::from(TOO_MANY_ACTIVE_RETRY_AFTER_SECS),
                );
                response
            }
            Self::Persistence(error) => {
                // Keep file paths and OS detail out of the response body.
                tracing::error!(error = %error, "persist invocations failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "persistence_failed",
                        "message": "failed to persist invocation",
                    })),
                )
                    .into_response()
            }
        }
    }
}

/// Strong entity tag for `revision`, e.g. `"3"`.
pub(crate) fn etag_for_revision(revision: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{revision}\"")).expect("digits and quotes are valid")
}

/// Malformed `If-Match`; maps to `400 invalid_if_match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidIfMatch;

impl IntoResponse for InvalidIfMatch {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_if_match",
                "message": "If-Match must be a single strong tag \"<revision>\" or *",
            })),
        )
            .into_response()
    }
}

/// Parses an optional `If-Match: "<revision>"` header.
///
/// Absent header or `*` → `Ok(None)` (no revision check). Weak tags, lists,
/// unquoted or non-numeric values are rejected: CAS needs one strong tag.
pub(crate) fn parse_if_match(headers: &HeaderMap) -> Result<Option<u64>, InvalidIfMatch> {
    let mut values = headers.get_all(header::IF_MATCH).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(InvalidIfMatch);
    }
    let raw = value.to_str().map_err(|_| InvalidIfMatch)?.trim();
    if raw == "*" {
        return Ok(None);
    }
    let inner = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or(InvalidIfMatch)?;
    if inner.is_empty() || !inner.bytes().all(|b| b.is_ascii_digit()) {
        return Err(InvalidIfMatch);
    }
    inner.parse::<u64>().map(Some).map_err(|_| InvalidIfMatch)
}

/// Claims-scoped invocation store. Share it as `Arc<InvocationStore>`.
#[derive(Debug)]
pub(crate) struct InvocationStore {
    path: PathBuf,
    config: InvocationStoreConfig,
    records: RwLock<Vec<Invocation>>,
}

/// What [`InvocationStore::open`] did while recovering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RecoveryReport {
    pub loaded: usize,
    pub interrupted: usize,
    /// Expired terminal records removed by the startup sweep.
    pub swept: usize,
}

impl InvocationStore {
    /// Default location: `<data_dir>/invocations.json`.
    pub(crate) fn default_path() -> PathBuf {
        super::data_dir().join(STORE_FILE_NAME)
    }

    /// Opens the store at [`Self::default_path`] with
    /// [`InvocationStoreConfig::from_env`] and runs restart recovery.
    pub(crate) fn open_default() -> Result<(Self, RecoveryReport), InvocationStoreError> {
        Self::open_with_config(Self::default_path(), InvocationStoreConfig::from_env())
    }

    /// [`Self::open_with_config`] with the default limits.
    pub(crate) fn open(
        path: impl Into<PathBuf>,
    ) -> Result<(Self, RecoveryReport), InvocationStoreError> {
        Self::open_with_config(path, InvocationStoreConfig::default())
    }

    /// Loads `path` (missing file = empty store), marks every non-terminal
    /// record as `failed/interrupted`, then sweeps expired terminal records.
    /// A corrupt file is an error rather than an empty store, so a later
    /// write can never silently discard records.
    pub(crate) fn open_with_config(
        path: impl Into<PathBuf>,
        config: InvocationStoreConfig,
    ) -> Result<(Self, RecoveryReport), InvocationStoreError> {
        let path = path.into();
        let mut records: Vec<Invocation> = match std::fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|error| InvocationStoreError::Persistence(error.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(InvocationStoreError::Persistence(error.to_string())),
        };
        let now = now_rfc3339();
        let mut interrupted = 0;
        for record in records.iter_mut().filter(|r| !r.state.is_terminal()) {
            let from = record.state;
            record.state = InvocationState::Failed;
            record.revision = record.revision.saturating_add(1);
            record.error = Some(InvocationErrorInfo::new(
                INTERRUPTED_ERROR_CODE,
                "the process restarted before the invocation finished",
            ));
            record.updated_at = now.clone();
            record.completed_at = Some(now.clone());
            let revision = record.revision;
            record.push_audit(InvocationAuditEvent {
                at: now.clone(),
                from: Some(from),
                to: InvocationState::Failed,
                revision,
                actor_id: SYSTEM_ACTOR_ID.to_string(),
            });
            interrupted += 1;
        }
        let loaded = records.len();
        let swept = sweep_expired_records(&mut records, config.retention, chrono::Utc::now());
        if interrupted > 0 || swept > 0 {
            persist(&path, &records)?;
        }
        let report = RecoveryReport {
            loaded,
            interrupted,
            swept,
        };
        Ok((
            Self {
                path,
                config,
                records: RwLock::new(records),
            },
            report,
        ))
    }

    pub(crate) fn config(&self) -> InvocationStoreConfig {
        self.config
    }

    /// Removes terminal records whose `completed_at` is older than the
    /// retention. Returns how many were removed; removing none writes
    /// nothing. Non-terminal records are never removed.
    pub(crate) async fn sweep_expired(&self) -> Result<usize, InvocationStoreError> {
        self.sweep_expired_at(chrono::Utc::now()).await
    }

    /// [`Self::sweep_expired`] against an explicit clock (tests, schedulers).
    pub(crate) async fn sweep_expired_at(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<usize, InvocationStoreError> {
        let mut records = self.records.write().await;
        let mut next = records.clone();
        let swept = sweep_expired_records(&mut next, self.config.retention, now);
        if swept == 0 {
            return Ok(0);
        }
        persist(&self.path, &next)?;
        *records = next;
        Ok(swept)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Creates a `queued` invocation at revision 1 in the claims' scope.
    ///
    /// Under one write lock: sweeps expired terminal records (persisted with
    /// the new record), rejects with [`InvocationStoreError::TooManyActive`]
    /// when the scope is at its active limit and with
    /// [`InvocationStoreError::StoreFull`] when the swept store is at
    /// [`MAX_STORED_INVOCATIONS`]. A rejected create writes nothing.
    pub(crate) async fn create_for_claims(
        &self,
        claims: &IsolationClaims,
        new: NewInvocation,
    ) -> Result<Invocation, InvocationStoreError> {
        let mut records = self.records.write().await;
        let active_in_scope = records
            .iter()
            .filter(|record| !record.state.is_terminal() && record.is_in_scope(claims))
            .count();
        if active_in_scope >= self.config.max_active_per_scope {
            return Err(InvocationStoreError::TooManyActive);
        }
        let mut next = records.clone();
        sweep_expired_records(&mut next, self.config.retention, chrono::Utc::now());
        if next.len() >= MAX_STORED_INVOCATIONS {
            return Err(InvocationStoreError::StoreFull);
        }
        let now = now_rfc3339();
        let mut invocation = Invocation {
            id: format!("inv_{}", uuid::Uuid::new_v4().simple()),
            object: "invocation".to_string(),
            tenant_id: claims.tenant_id().to_string(),
            project_id: claims.project_id().to_string(),
            actor_id: claims.actor_id().to_string(),
            state: InvocationState::Queued,
            revision: 1,
            request: new.request,
            task_iri: new.task_iri,
            result: None,
            error: None,
            idempotency_key: new.idempotency_key,
            created_at: now.clone(),
            updated_at: now.clone(),
            started_at: None,
            completed_at: None,
            audit_events: Vec::new(),
        };
        invocation.push_audit(InvocationAuditEvent {
            at: now,
            from: None,
            to: InvocationState::Queued,
            revision: 1,
            actor_id: claims.actor_id().to_string(),
        });
        next.push(invocation.clone());
        persist(&self.path, &next)?;
        *records = next;
        Ok(invocation)
    }

    /// Reads one invocation in the claims' scope (any actor in the scope).
    pub(crate) async fn get_for_claims(
        &self,
        claims: &IsolationClaims,
        id: &str,
    ) -> Result<Invocation, InvocationStoreError> {
        self.records
            .read()
            .await
            .iter()
            .find(|record| record.id == id && record.is_in_scope(claims))
            .cloned()
            .ok_or(InvocationStoreError::NotFound)
    }

    /// Lists the claims' scope, newest first, optionally filtered by state.
    /// Cursor pagination is layered on top by the list route.
    pub(crate) async fn list_for_claims(
        &self,
        claims: &IsolationClaims,
        state: Option<InvocationState>,
    ) -> Vec<Invocation> {
        let records = self.records.read().await;
        let mut listed: Vec<Invocation> = records
            .iter()
            .filter(|record| record.is_in_scope(claims))
            .filter(|record| state.is_none_or(|wanted| record.state == wanted))
            .cloned()
            .collect();
        listed.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        listed
    }

    /// Lifecycle write that discards the `changed` flag; see
    /// [`Self::transition_outcome_for_claims`].
    pub(crate) async fn transition_for_claims(
        &self,
        claims: &IsolationClaims,
        id: &str,
        expected_revision: Option<u64>,
        next_state: InvocationState,
        patch: TransitionPatch,
    ) -> Result<Invocation, InvocationStoreError> {
        self.transition_outcome_for_claims(claims, id, expected_revision, next_state, patch)
            .await
            .map(|outcome| outcome.invocation)
    }

    /// The single write entry point for lifecycle changes.
    ///
    /// Checks, in order and under one write lock: scope (`NotFound`),
    /// same-state repeat (idempotent no-op, `changed = false`),
    /// `expected_revision` (`RevisionConflict`), lifecycle edge
    /// (`IllegalTransition`; `queued → failed` only with
    /// `error.code = "deadline_exceeded"`). On success state, revision, timestamps, patch
    /// and the audit event are persisted in one atomic file replace before
    /// memory is updated. Who may request which transition (for example
    /// cancel only by the creating actor or a DA) is decided by the caller.
    pub(crate) async fn transition_outcome_for_claims(
        &self,
        claims: &IsolationClaims,
        id: &str,
        expected_revision: Option<u64>,
        next_state: InvocationState,
        patch: TransitionPatch,
    ) -> Result<TransitionOutcome, InvocationStoreError> {
        let mut records = self.records.write().await;
        let index = records
            .iter()
            .position(|record| record.id == id && record.is_in_scope(claims))
            .ok_or(InvocationStoreError::NotFound)?;
        let current = &records[index];
        if current.state == next_state {
            // Already in the requested state: nothing to write (RFC 9110
            // §13.1.1 allows 2xx even if If-Match is stale).
            return Ok(TransitionOutcome {
                invocation: current.clone(),
                changed: false,
            });
        }
        if let Some(expected) = expected_revision {
            if expected != current.revision {
                return Err(InvocationStoreError::RevisionConflict {
                    current: current.revision,
                });
            }
        }
        let from = current.state;
        if from.is_terminal() || !from.permits_with(next_state, patch.error.as_ref()) {
            return Err(InvocationStoreError::IllegalTransition {
                from,
                to: next_state,
            });
        }

        let mut next = records.clone();
        let now = now_rfc3339();
        let record = &mut next[index];
        record.state = next_state;
        record.revision = record.revision.saturating_add(1);
        record.updated_at = now.clone();
        if next_state == InvocationState::Running && record.started_at.is_none() {
            record.started_at = Some(now.clone());
        }
        if next_state.is_terminal() {
            record.completed_at = Some(now.clone());
        }
        if let Some(result) = patch.result {
            record.result = Some(result);
        }
        if let Some(error) = patch.error {
            record.error = Some(InvocationErrorInfo::new(error.code, error.message));
        }
        let revision = record.revision;
        record.push_audit(InvocationAuditEvent {
            at: now,
            from: Some(from),
            to: next_state,
            revision,
            actor_id: claims.actor_id().to_string(),
        });
        let updated = record.clone();
        persist(&self.path, &next)?;
        *records = next;
        Ok(TransitionOutcome {
            invocation: updated,
            changed: true,
        })
    }
}

/// Drops terminal records completed more than `retention` before `now`.
/// A terminal record without a parsable `completed_at` falls back to
/// `updated_at`; if neither parses it is kept.
fn sweep_expired_records(
    records: &mut Vec<Invocation>,
    retention: chrono::Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> usize {
    let cutoff = now - retention;
    let before = records.len();
    records.retain(|record| {
        if !record.state.is_terminal() {
            return true;
        }
        let at = record
            .completed_at
            .as_deref()
            .unwrap_or(record.updated_at.as_str());
        let finished = chrono::DateTime::parse_from_rfc3339(at).ok();
        match finished {
            Some(at) => at.with_timezone(&chrono::Utc) > cutoff,
            None => true,
        }
    });
    before - records.len()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn persist(path: &Path, records: &[Invocation]) -> Result<(), InvocationStoreError> {
    let error = |e: std::io::Error| InvocationStoreError::Persistence(e.to_string());
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(error)?;
    }
    let serialized = serde_json::to_vec(records)
        .map_err(|e| InvocationStoreError::Persistence(e.to_string()))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized).map_err(error)?;
    std::fs::rename(&temporary, path).map_err(error)
}

#[cfg(test)]
#[path = "invocations_store_tests.rs"]
mod tests;
