//! Durable execution boundary (design: `docs/30-durable-execution.md`).
//!
//! [`DurableExecutor`] is the engine-facing contract. The HTTP invocation
//! store stays the public lifecycle authority: an engine must not write it,
//! and claims found in engine history are never trusted. This module is the
//! capability contract and the in-process [`MockDurableExecutor`]. It does
//! not open a tenant database, record a side-effect ledger, or resume work
//! when the process starts.
//!
//! `invocations.resume_on_restart` is intentionally not read here. Startup
//! still marks non-terminal invocations `failed/interrupted` and does not
//! call [`DurableExecutor::resume_all_for_tenant`].
//!
//! # Capability probe
//!
//! [`probe_required`] reads [`DurableExecutor::capabilities`] and rejects an
//! engine that lacks deterministic replay, durable timers, external signals,
//! or instance leases. The rejection is an error. The probe does not turn the
//! missing flags on, and it does not return a weaker executor. [`InstalledDurableExecutor::install`]
//! is the registration point; process startup does not call it.
//!
//! An operation that needs a capability the engine did not advertise fails
//! with the same class of error. A `multi_node = false` advertisement is the
//! single-node posture, not a missing required capability. Versioning is not
//! in the startup reject list; a start or other mutating call still refuses
//! to run when the engine cannot record a workflow compatibility version.

mod mock;

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::isolation::IsolationClaims;
use crate::utils::secret_scan::contains_plaintext_secret;

pub use mock::MockDurableExecutor;

/// Registered orchestration accepted by this contract. Engines keep this
/// implementation for live instances; a different name or compatibility
/// version is rejected instead of being executed.
pub const INVOCATION_WORKFLOW_NAME: &str = "invocation";
/// Compatibility version stored with [`INVOCATION_WORKFLOW_NAME`].
pub const INVOCATION_WORKFLOW_COMPATIBILITY_VERSION: u32 = 1;
/// Maximum serialized activity input or output kept in engine history.
/// Larger payloads belong in claims-scoped blobs. This slice enforces the
/// bound on the non-secret input reference carried by [`DurableStart`].
pub const MAX_HISTORY_RECORD_BYTES: usize = 65_536;

/// In-process mock engine identity. Embedded duroxide does not use this name.
pub const MOCK_ENGINE_NAME: &str = "mock";
/// Version recorded on instances started by [`MockDurableExecutor`].
pub const MOCK_ENGINE_VERSION: &str = "1";

/// Stable names for [`DurableCapabilities`] flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum DurableCapability {
    /// Deterministic replay of recorded history.
    DeterministicReplay,
    /// Durable timers that race work and survive the in-process schedule.
    DurableTimers,
    /// External cancel and approval signals recorded in history.
    ExternalSignals,
    /// A lease that stops two owners running one instance.
    InstanceLeases,
    /// Records and honors `workflow_compatibility_version`.
    Versioning,
    /// Multi-node scheduling. The embedded path advertises `false`.
    MultiNode,
}

impl DurableCapability {
    /// Stable probe and error spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeterministicReplay => "deterministic_replay",
            Self::DurableTimers => "durable_timers",
            Self::ExternalSignals => "external_signals",
            Self::InstanceLeases => "instance_leases",
            Self::Versioning => "versioning",
            Self::MultiNode => "multi_node",
        }
    }
}

impl fmt::Display for DurableCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an engine actually implements. Flags are advertisements, not wishes:
/// [`probe_required`] rejects a hole in the required set, and the mock refuses
/// to advertise [`DurableCapability::MultiNode`] because it cannot provide it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableCapabilities {
    pub durable_timers: bool,
    pub external_signals: bool,
    pub deterministic_replay: bool,
    pub multi_node: bool,
    pub instance_leases: bool,
    pub versioning: bool,
}

impl DurableCapabilities {
    /// Required single-node set. `multi_node` stays false.
    pub const fn single_node() -> Self {
        Self {
            durable_timers: true,
            external_signals: true,
            deterministic_replay: true,
            multi_node: false,
            instance_leases: true,
            versioning: true,
        }
    }

    /// Required capabilities that are absent, in probe order: deterministic
    /// replay, timers, signals, leases. Versioning and `multi_node` are not
    /// in this list.
    pub fn missing_required(self) -> Vec<DurableCapability> {
        let mut missing = Vec::with_capacity(4);
        if !self.deterministic_replay {
            missing.push(DurableCapability::DeterministicReplay);
        }
        if !self.durable_timers {
            missing.push(DurableCapability::DurableTimers);
        }
        if !self.external_signals {
            missing.push(DurableCapability::ExternalSignals);
        }
        if !self.instance_leases {
            missing.push(DurableCapability::InstanceLeases);
        }
        missing
    }

    /// Whether [`probe_required`] would accept this advertisement.
    pub fn satisfies_required(self) -> bool {
        self.missing_required().is_empty()
    }
}

/// Capabilities named by a failed probe or a fail-closed operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingCapabilities {
    capabilities: Vec<DurableCapability>,
}

impl MissingCapabilities {
    pub fn new(capabilities: Vec<DurableCapability>) -> Self {
        Self { capabilities }
    }

    pub fn as_slice(&self) -> &[DurableCapability] {
        &self.capabilities
    }

    pub fn into_vec(self) -> Vec<DurableCapability> {
        self.capabilities
    }
}

impl fmt::Display for MissingCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, capability) in self.capabilities.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(capability.as_str())?;
        }
        Ok(())
    }
}

/// Lowercase SHA-256 hex digest. Constructors reject any other shape.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct ContentDigest(String);

impl ContentDigest {
    /// Digest of `bytes` as 64 lowercase hex characters.
    pub fn sha256(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    /// Accepts 64 hexadecimal characters and stores the lowercase form.
    pub fn parse(value: &str) -> Result<Self, DurableError> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DurableError::InvalidRequest {
                message: "content digest must be 64 hexadecimal characters",
            });
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonical digest of a verified claims snapshot.
///
/// The engine stores this digest and compares it. It does not store the
/// tenant, project, or actor, and it cannot turn the digest back into claims.
pub fn claims_snapshot_digest(claims: &IsolationClaims) -> ContentDigest {
    let canonical = format!(
        "wao-claims-snapshot-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        claims.tenant_id().len(),
        claims.tenant_id(),
        claims.project_id().len(),
        claims.project_id(),
        claims.actor_id().len(),
        claims.actor_id(),
    );
    ContentDigest::sha256(canonical.as_bytes())
}

/// Public invocation id. The durable instance id is this value.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct InvocationId(String);

impl InvocationId {
    /// Rejects an empty, oversized, or non-identifier id. The invalid value
    /// is not copied into the error.
    pub fn new(value: impl Into<String>) -> Result<Self, DurableError> {
        let value = value.into();
        if !(1..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(DurableError::InvalidRequest {
                message:
                    "invocation id must be 1 to 128 characters of ascii letters, digits, '_' or '-'",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Engine name and version recorded for an instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EngineIdentity {
    pub name: String,
    pub version: String,
}

/// Start request. It carries digests and a non-secret input reference, never
/// credentials, tokens, or a claims object the engine could treat as authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DurableStart {
    pub invocation_id: InvocationId,
    pub claims_snapshot_digest: ContentDigest,
    pub definition_digest: ContentDigest,
    pub policy_digest: ContentDigest,
    pub deadline: Option<DateTime<Utc>>,
    pub input_ref: Option<String>,
    pub workflow_name: String,
    pub workflow_compatibility_version: u32,
}

/// Shape checks shared by engines. A failed check is an error and does not
/// describe a degraded start.
pub fn validate_durable_start(request: &DurableStart) -> Result<(), DurableError> {
    if !is_workflow_name(&request.workflow_name) {
        return Err(DurableError::InvalidRequest {
            message:
                "workflow name must be 1 to 64 characters of ascii letters, digits, '_' or '-'",
        });
    }
    if request.workflow_name != INVOCATION_WORKFLOW_NAME
        || request.workflow_compatibility_version != INVOCATION_WORKFLOW_COMPATIBILITY_VERSION
    {
        return Err(DurableError::IncompatibleWorkflow {
            workflow: request.workflow_name.clone(),
            version: request.workflow_compatibility_version,
        });
    }
    if let Some(input_ref) = &request.input_ref {
        if input_ref.is_empty() {
            return Err(DurableError::InvalidRequest {
                message: "durable input reference must not be empty",
            });
        }
        if input_ref.len() > MAX_HISTORY_RECORD_BYTES {
            return Err(DurableError::HistoryRecordTooLarge {
                size: input_ref.len(),
                max: MAX_HISTORY_RECORD_BYTES,
            });
        }
        if input_ref.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err(DurableError::InvalidRequest {
                message: "durable input reference must not contain control characters",
            });
        }
        if contains_plaintext_secret(input_ref) {
            return Err(DurableError::CredentialRejected);
        }
    }
    Ok(())
}

fn is_workflow_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// External signal delivered to a running instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum DurableSignal {
    Cancel,
    Approval {
        staging_id: String,
        outcome: ApprovalOutcome,
    },
}

/// Human-review outcome carried by [`DurableSignal::Approval`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ApprovalOutcome {
    Approved,
    Rejected,
}

impl ApprovalOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

impl fmt::Display for ApprovalOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Handle returned by a successful start. A rejected start does not mint one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableHandle {
    pub invocation_id: InvocationId,
    pub run_id: String,
    pub engine: EngineIdentity,
}

/// Why a terminal failure was recorded. This slice only produces a deadline
/// failure; the variant stays specific so a later activity failure cannot be
/// confused with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DurableFailureReason {
    DeadlineExceeded,
}

impl DurableFailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }
}

/// Why an instance cannot be resumed safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum AttentionReason {
    NondeterministicReplay,
}

impl AttentionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NondeterministicReplay => "nondeterministic_replay",
        }
    }
}

/// Engine-level phase. `NeedsAttention` is not terminal: purge must leave it
/// in place, and resume must not report it as a successful replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum DurablePhase {
    Running,
    Succeeded,
    Failed { reason: DurableFailureReason },
    Cancelled,
    NeedsAttention { reason: AttentionReason },
}

impl DurablePhase {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed { .. } | Self::Cancelled
        )
    }
}

/// Approval recorded in history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecordedApproval {
    pub staging_id: String,
    pub outcome: ApprovalOutcome,
}

/// Point-in-time view of one instance. `activity_count` counts executed
/// activities. This slice has none, so a deadline or cancel leaves it at zero.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableStatus {
    pub invocation_id: InvocationId,
    pub run_id: String,
    pub phase: DurablePhase,
    pub claims_snapshot_digest: ContentDigest,
    pub definition_digest: ContentDigest,
    pub policy_digest: ContentDigest,
    pub workflow_name: String,
    pub workflow_compatibility_version: u32,
    pub engine: EngineIdentity,
    pub deadline: Option<DateTime<Utc>>,
    pub approvals: Vec<RecordedApproval>,
    pub history_digest: ContentDigest,
    pub activity_count: u64,
    pub lease_held: bool,
    pub input_ref_digest: Option<ContentDigest>,
}

/// Clean replay of every in-scope instance. A lease conflict or a diverging
/// replay is a [`DurableError`], not a field the caller can skip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeReport {
    pub replayed: Vec<InvocationId>,
    pub already_terminal: Vec<InvocationId>,
    pub timer_resolved: Vec<InvocationId>,
    pub history_digest: ContentDigest,
}

impl ResumeReport {
    fn empty() -> Self {
        Self {
            replayed: Vec::new(),
            already_terminal: Vec::new(),
            timer_resolved: Vec::new(),
            history_digest: resume_history_digest(&[]),
        }
    }
}

pub(crate) fn resume_history_digest(pairs: &[(InvocationId, ContentDigest)]) -> ContentDigest {
    let mut lines: Vec<String> = pairs
        .iter()
        .map(|(id, digest)| format!("{} {}", id.as_str(), digest.as_str()))
        .collect();
    lines.sort();
    let mut body = String::from("wao-resume-v1\n");
    for line in lines {
        body.push_str(&line);
        body.push('\n');
    }
    ContentDigest::sha256(body.as_bytes())
}

/// Claims-scoped purge. Other scopes are counted, not identified.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgeReport {
    pub purged: Vec<InvocationId>,
    /// In-scope instances that are non-terminal, `needs_attention`, or have
    /// no completion time. Purge never removes them.
    pub retained_non_terminal: usize,
    /// In-scope terminal instances whose completion time is not strictly
    /// before the caller's bound.
    pub retained_terminal: usize,
    pub retained_other_scope: usize,
}

/// Failures of the durable boundary. Credential and request errors do not
/// echo the rejected value.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum DurableError {
    #[error("durable executor is missing capabilities: {missing}")]
    CapabilityUnavailable { missing: MissingCapabilities },
    #[error("durable executor cannot advertise the {capability} capability")]
    CapabilityUnsupported { capability: DurableCapability },
    #[error("{message}")]
    InvalidRequest { message: &'static str },
    #[error("workflow {workflow} compatibility version {version} is not registered")]
    IncompatibleWorkflow { workflow: String, version: u32 },
    #[error("durable start rejected a plaintext credential")]
    CredentialRejected,
    #[error("durable history record is {size} bytes; the maximum is {max}")]
    HistoryRecordTooLarge { size: usize, max: usize },
    #[error("durable instance {id} already exists")]
    AlreadyExists { id: InvocationId },
    #[error("durable instance {id} was not found")]
    NotFound { id: InvocationId },
    #[error("durable signal is not valid for invocation {id}")]
    IllegalSignal { id: InvocationId },
    #[error("durable instance lease is already held for {id}")]
    LeaseConflict { id: InvocationId },
    #[error("durable replay diverged for {id}")]
    NondeterministicReplay { id: InvocationId },
}

/// Engine boundary. Object-safe so a registration point can hold
/// `Arc<dyn DurableExecutor>` after the capability probe.
#[async_trait]
pub trait DurableExecutor: Send + Sync {
    fn capabilities(&self) -> DurableCapabilities;
    async fn start(&self, request: DurableStart) -> Result<DurableHandle, DurableError>;
    async fn signal(&self, id: &InvocationId, signal: DurableSignal) -> Result<(), DurableError>;
    async fn query_status(&self, id: &InvocationId) -> Result<DurableStatus, DurableError>;
    async fn resume_all_for_tenant(
        &self,
        claims: &IsolationClaims,
    ) -> Result<ResumeReport, DurableError>;
    async fn purge(
        &self,
        claims: &IsolationClaims,
        before: DateTime<Utc>,
    ) -> Result<PurgeReport, DurableError>;
}

/// Reads capabilities and rejects the engine when a required one is absent.
///
/// The returned error lists every missing required capability. Flags are not
/// rewritten, and this function does not build a fallback executor.
pub fn probe_required(executor: &dyn DurableExecutor) -> Result<DurableCapabilities, DurableError> {
    let capabilities = executor.capabilities();
    let missing = capabilities.missing_required();
    if missing.is_empty() {
        return Ok(capabilities);
    }
    let missing = MissingCapabilities::new(missing);
    tracing::warn!(
        missing = %missing,
        "durable executor rejected by capability probe"
    );
    Err(DurableError::CapabilityUnavailable { missing })
}

/// An executor that has passed [`probe_required`].
///
/// [`Self::install`] is the construction point. It does not start instances
/// and it does not resume tenants. Process startup does not construct this
/// type, so the invocation store's restart path is unchanged.
#[derive(Clone)]
pub struct InstalledDurableExecutor {
    executor: std::sync::Arc<dyn DurableExecutor>,
    capabilities: DurableCapabilities,
}

impl InstalledDurableExecutor {
    pub fn install(executor: std::sync::Arc<dyn DurableExecutor>) -> Result<Self, DurableError> {
        let capabilities = probe_required(executor.as_ref())?;
        debug_assert!(capabilities.satisfies_required());
        Ok(Self {
            executor,
            capabilities,
        })
    }

    pub fn executor(&self) -> std::sync::Arc<dyn DurableExecutor> {
        std::sync::Arc::clone(&self.executor)
    }

    pub fn capabilities(&self) -> DurableCapabilities {
        self.capabilities
    }
}

#[cfg(test)]
mod contract;
#[cfg(test)]
pub use contract::assert_durable_executor_contract;
