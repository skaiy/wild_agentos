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
//! # Recovery
//!
//! [`InvocationStore::open`] moves every non-terminal record (`queued`,
//! `running`, `cancel_requested`) to `failed` with `error.code =
//! "interrupted"` and bumps its revision. Nothing is re-run automatically.
//! Recovery is the only write path that bypasses [`InvocationState::permits`];
//! it can only ever target `failed` from a non-terminal state.

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
/// Actor recorded in audit events written by the store itself.
pub(crate) const SYSTEM_ACTOR_ID: &str = "system";

const STORE_FILE_NAME: &str = "invocations.json";

/// Lifecycle state of an invocation (epic #313 §3.5).
///
/// ```text
/// queued ──► running ──► succeeded
///   │           ├──────► failed
///   │           └──► cancel_requested ──► cancelled
///   └──────────────────────────────────► cancelled
/// ```
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
    /// states have no outgoing edges and there are no self-loops.
    pub(crate) fn permits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::CancelRequested
                )
                | (Self::CancelRequested, Self::Cancelled)
        )
    }

    /// The state a cancel request moves to, or `None` when cancel is not an
    /// allowed edge from `self` (already cancelling or terminal).
    pub(crate) fn cancel_target(self) -> Option<Self> {
        match self {
            Self::Queued => Some(Self::Cancelled),
            Self::Running => Some(Self::CancelRequested),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub(crate) struct InvocationInput {
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
    pub input: InvocationInput,
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
    pub input: InvocationInput,
    pub task_iri: Option<String>,
    pub idempotency_key: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidIfMatch;

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
    records: RwLock<Vec<Invocation>>,
}

/// What [`InvocationStore::open`] did while recovering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RecoveryReport {
    pub loaded: usize,
    pub interrupted: usize,
}

impl InvocationStore {
    /// Default location: `<data_dir>/invocations.json`.
    pub(crate) fn default_path() -> PathBuf {
        super::data_dir().join(STORE_FILE_NAME)
    }

    /// Opens the store at [`Self::default_path`] and runs restart recovery.
    pub(crate) fn open_default() -> Result<(Self, RecoveryReport), InvocationStoreError> {
        Self::open(Self::default_path())
    }

    /// Loads `path` (missing file = empty store) and marks every non-terminal
    /// record as `failed/interrupted`. A corrupt file is an error rather than
    /// an empty store, so a later write can never silently discard records.
    pub(crate) fn open(
        path: impl Into<PathBuf>,
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
        if interrupted > 0 {
            persist(&path, &records)?;
        }
        let report = RecoveryReport {
            loaded: records.len(),
            interrupted,
        };
        Ok((
            Self {
                path,
                records: RwLock::new(records),
            },
            report,
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Creates a `queued` invocation at revision 1 in the claims' scope.
    pub(crate) async fn create_for_claims(
        &self,
        claims: &IsolationClaims,
        new: NewInvocation,
    ) -> Result<Invocation, InvocationStoreError> {
        let mut records = self.records.write().await;
        if records.len() >= MAX_STORED_INVOCATIONS {
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
            input: new.input,
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
        let mut next = records.clone();
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

    /// The single write entry point for lifecycle changes.
    ///
    /// Checks, in order and under one write lock: scope (`NotFound`),
    /// `expected_revision` (`RevisionConflict`), lifecycle edge
    /// (`IllegalTransition`). On success state, revision, timestamps, patch
    /// and the audit event are persisted in one atomic file replace before
    /// memory is updated. Who may request which transition (for example
    /// cancel only by the creating actor or a DA) is decided by the caller.
    pub(crate) async fn transition_for_claims(
        &self,
        claims: &IsolationClaims,
        id: &str,
        expected_revision: Option<u64>,
        next_state: InvocationState,
        patch: TransitionPatch,
    ) -> Result<Invocation, InvocationStoreError> {
        let mut records = self.records.write().await;
        let index = records
            .iter()
            .position(|record| record.id == id && record.is_in_scope(claims))
            .ok_or(InvocationStoreError::NotFound)?;
        let current = &records[index];
        if let Some(expected) = expected_revision {
            if expected != current.revision {
                return Err(InvocationStoreError::RevisionConflict {
                    current: current.revision,
                });
            }
        }
        let from = current.state;
        if from.is_terminal() || !from.permits(next_state) {
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
        Ok(updated)
    }
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
