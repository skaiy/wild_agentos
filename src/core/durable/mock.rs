//! In-process [`MockDurableExecutor`].
//!
//! History lives in this process only. There is no SQLite file, no tenant
//! runtime, and no call into the invocation store. Replay folds the recorded
//! events twice with one captured clock reading and commits the result only
//! when both folds agree. A divergence becomes `needs_attention` and an
//! error; the diverging projection is not stored.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;

use super::{
    claims_snapshot_digest, resume_history_digest, validate_durable_start, ApprovalOutcome,
    AttentionReason, ContentDigest, DurableCapabilities, DurableCapability, DurableError,
    DurableExecutor, DurableFailureReason, DurableHandle, DurablePhase, DurableSignal,
    DurableStart, DurableStatus, EngineIdentity, InvocationId, MissingCapabilities, PurgeReport,
    RecordedApproval, ResumeReport, MOCK_ENGINE_NAME, MOCK_ENGINE_VERSION,
};
use crate::isolation::IsolationClaims;
use crate::utils::secret_scan::contains_plaintext_secret;

/// Process-local stand-in for the durable contract. Cloning shares the same
/// history. `multi_node` cannot be advertised: this engine has no second node.
#[derive(Clone, Debug)]
pub struct MockDurableExecutor {
    capabilities: DurableCapabilities,
    inner: Arc<Mutex<MockInner>>,
}

#[derive(Debug)]
struct MockInner {
    instances: HashMap<InvocationId, Instance>,
    now: DateTime<Utc>,
    drift: bool,
    pause: Option<Arc<ResumePause>>,
    #[cfg(test)]
    resume_calls: u64,
}

#[derive(Debug)]
struct Instance {
    events: Vec<HistoryEvent>,
    lease_held: bool,
    attention: Option<AttentionReason>,
}

/// Test hook. The first waiter observes `entered` after the resume turn has
/// taken the instance lease and before it replays. `release` lets that turn
/// continue. A second turn that arrives in between fails with a lease conflict
/// instead of replaying.
#[derive(Debug)]
pub struct ResumePause {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(test)]
impl ResumePause {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct HistoryEvent {
    kind: EventKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
enum EventKind {
    Started {
        claims_snapshot_digest: ContentDigest,
        definition_digest: ContentDigest,
        policy_digest: ContentDigest,
        deadline: Option<DateTime<Utc>>,
        input_ref: Option<String>,
        workflow_name: String,
        workflow_compatibility_version: u32,
        engine: EngineIdentity,
        run_id: String,
        at: DateTime<Utc>,
    },
    Cancel {
        at: DateTime<Utc>,
    },
    Approval {
        staging_id: String,
        outcome: ApprovalOutcome,
        at: DateTime<Utc>,
    },
    DeadlineFired {
        at: DateTime<Utc>,
    },
    Completed {
        phase: RecordedTerminal,
        at: DateTime<Utc>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PurgeDecision {
    Purge,
    KeepTerminal,
    KeepNonTerminal,
    OtherScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum RecordedTerminal {
    Cancelled,
    DeadlineExceeded,
}

struct Fold {
    phase: DurablePhase,
    approvals: Vec<RecordedApproval>,
    deadline: Option<DateTime<Utc>>,
    claims_snapshot_digest: ContentDigest,
    definition_digest: ContentDigest,
    policy_digest: ContentDigest,
    workflow_name: String,
    workflow_compatibility_version: u32,
    engine: EngineIdentity,
    run_id: String,
    input_ref_digest: Option<ContentDigest>,
    completed_at: Option<DateTime<Utc>>,
    activity_count: u64,
}

impl MockDurableExecutor {
    /// Mock that honestly advertises the single-node required set.
    pub fn new() -> Self {
        Self::with_capabilities(DurableCapabilities::single_node())
            .expect("single-node mock capabilities are supported")
    }

    /// Builds a mock with an explicit advertisement.
    ///
    /// Advertising `multi_node` is [`DurableError::CapabilityUnsupported`].
    /// A missing required capability is accepted by this constructor so the
    /// probe can be tested; every operation then fails closed.
    pub fn with_capabilities(capabilities: DurableCapabilities) -> Result<Self, DurableError> {
        if capabilities.multi_node {
            return Err(DurableError::CapabilityUnsupported {
                capability: DurableCapability::MultiNode,
            });
        }
        Ok(Self {
            capabilities,
            inner: Arc::new(Mutex::new(MockInner {
                instances: HashMap::new(),
                now: Utc::now(),
                drift: false,
                pause: None,
                #[cfg(test)]
                resume_calls: 0,
            })),
        })
    }

    pub fn engine_identity() -> EngineIdentity {
        EngineIdentity {
            name: MOCK_ENGINE_NAME.to_string(),
            version: MOCK_ENGINE_VERSION.to_string(),
        }
    }

    /// Moves the clock the deadline check reads. Replay uses the value
    /// captured at the start of the call, not a second read.
    #[cfg(test)]
    pub fn set_time(&self, now: DateTime<Utc>) {
        self.inner.lock().now = now;
    }

    /// Clock reading used for a new decision. Folds of recorded history do
    /// not call this again.
    pub fn now(&self) -> DateTime<Utc> {
        self.inner.lock().now
    }

    #[cfg(test)]
    pub fn instance_count(&self) -> usize {
        self.inner.lock().instances.len()
    }

    #[cfg(test)]
    pub fn resume_calls(&self) -> u64 {
        self.inner.lock().resume_calls
    }

    /// The next resume of a non-empty tenant waits after taking leases.
    #[cfg(test)]
    pub fn pause_next_resume(&self) -> Arc<ResumePause> {
        let pause = ResumePause::new();
        self.inner.lock().pause = Some(Arc::clone(&pause));
        pause
    }

    /// The next replay's second fold disagrees with the first. The instance
    /// is marked `needs_attention` and the drifted events are discarded.
    #[cfg(test)]
    pub fn inject_replay_drift(&self) {
        self.inner.lock().drift = true;
    }

    fn ensure(&self, mutating: bool) -> Result<(), DurableError> {
        let missing = self.capabilities.missing_required();
        if !missing.is_empty() {
            return Err(DurableError::CapabilityUnavailable {
                missing: MissingCapabilities::new(missing),
            });
        }
        if mutating && !self.capabilities.versioning {
            return Err(DurableError::CapabilityUnavailable {
                missing: MissingCapabilities::new(vec![DurableCapability::Versioning]),
            });
        }
        Ok(())
    }
}

impl Default for MockDurableExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DurableExecutor for MockDurableExecutor {
    fn capabilities(&self) -> DurableCapabilities {
        self.capabilities
    }

    async fn start(&self, request: DurableStart) -> Result<DurableHandle, DurableError> {
        self.ensure(true)?;
        validate_durable_start(&request)?;
        let now = self.now();
        let mut guard = self.inner.lock();
        if guard.instances.contains_key(&request.invocation_id) {
            return Err(DurableError::AlreadyExists {
                id: request.invocation_id,
            });
        }
        let run_id = uuid::Uuid::new_v4().hyphenated().to_string();
        let started = started_event(&request, &run_id, now);
        let events = project(&[started], now, false);
        let id = request.invocation_id.clone();
        guard.instances.insert(
            id.clone(),
            Instance {
                events,
                lease_held: false,
                attention: None,
            },
        );
        Ok(DurableHandle {
            invocation_id: id,
            run_id,
            engine: Self::engine_identity(),
        })
    }

    async fn signal(&self, id: &InvocationId, signal: DurableSignal) -> Result<(), DurableError> {
        self.ensure(true)?;
        validate_signal(&signal)?;
        let now = self.now();
        let mut guard = self.inner.lock();
        let instance = guard
            .instances
            .get_mut(id)
            .ok_or_else(|| DurableError::NotFound { id: id.clone() })?;
        if instance.lease_held {
            return Err(DurableError::LeaseConflict { id: id.clone() });
        }
        if instance.attention.is_some() {
            return Err(DurableError::NondeterministicReplay { id: id.clone() });
        }
        let updated = apply_signal(&instance.events, &signal, now)
            .map_err(|_| DurableError::IllegalSignal { id: id.clone() })?;
        if updated != instance.events {
            instance.events = updated;
        }
        Ok(())
    }

    async fn query_status(&self, id: &InvocationId) -> Result<DurableStatus, DurableError> {
        self.ensure(false)?;
        let guard = self.inner.lock();
        let instance = guard
            .instances
            .get(id)
            .ok_or_else(|| DurableError::NotFound { id: id.clone() })?;
        Ok(status_of(id, instance))
    }

    async fn resume_all_for_tenant(
        &self,
        claims: &IsolationClaims,
    ) -> Result<ResumeReport, DurableError> {
        self.ensure(true)?;
        #[cfg(test)]
        {
            let mut guard = self.inner.lock();
            guard.resume_calls = guard.resume_calls.saturating_add(1);
        }
        let digest = claims_snapshot_digest(claims);
        let now = self.now();
        let (ids, pause) = {
            let mut guard = self.inner.lock();
            let ids = guard.ids_for_digest(&digest);
            if ids.is_empty() {
                return Ok(ResumeReport::empty());
            }
            if let Some(id) = ids
                .iter()
                .find(|id| {
                    guard
                        .instances
                        .get(*id)
                        .is_some_and(|instance| instance.attention.is_some())
                })
                .cloned()
            {
                return Err(DurableError::NondeterministicReplay { id });
            }
            if let Some(id) = ids
                .iter()
                .find(|id| {
                    guard
                        .instances
                        .get(*id)
                        .is_some_and(|instance| instance.lease_held)
                })
                .cloned()
            {
                return Err(DurableError::LeaseConflict { id });
            }
            for id in &ids {
                guard
                    .instances
                    .get_mut(id)
                    .expect("id just collected")
                    .lease_held = true;
            }
            let pause = guard.pause.take();
            (ids, pause)
        };
        let _clear = ClearLeases {
            inner: Arc::clone(&self.inner),
            ids: ids.clone(),
        };
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        let mut guard = self.inner.lock();
        guard.replay(&ids, now)
    }

    async fn purge(
        &self,
        claims: &IsolationClaims,
        before: DateTime<Utc>,
    ) -> Result<PurgeReport, DurableError> {
        self.ensure(true)?;
        let digest = claims_snapshot_digest(claims);
        let mut guard = self.inner.lock();
        let mut purged = Vec::new();
        let mut retained_non_terminal = 0;
        let mut retained_terminal = 0;
        let mut retained_other_scope = 0;
        let ids: Vec<InvocationId> = guard.instances.keys().cloned().collect();
        for id in ids {
            let decision = {
                let Some(instance) = guard.instances.get(&id) else {
                    continue;
                };
                let current = fold(&instance.events);
                if current.claims_snapshot_digest != digest {
                    PurgeDecision::OtherScope
                } else if instance.attention.is_none()
                    && current.phase.is_terminal()
                    && current
                        .completed_at
                        .is_some_and(|completed_at| completed_at < before)
                {
                    PurgeDecision::Purge
                } else if current.phase.is_terminal() && instance.attention.is_none() {
                    PurgeDecision::KeepTerminal
                } else {
                    PurgeDecision::KeepNonTerminal
                }
            };
            match decision {
                PurgeDecision::OtherScope => retained_other_scope += 1,
                PurgeDecision::KeepTerminal => retained_terminal += 1,
                PurgeDecision::KeepNonTerminal => retained_non_terminal += 1,
                PurgeDecision::Purge => {
                    purged.push(id.clone());
                    guard.instances.remove(&id);
                }
            }
        }
        purged.sort();
        Ok(PurgeReport {
            purged,
            retained_non_terminal,
            retained_terminal,
            retained_other_scope,
        })
    }
}

struct ClearLeases {
    inner: Arc<Mutex<MockInner>>,
    ids: Vec<InvocationId>,
}

impl Drop for ClearLeases {
    fn drop(&mut self) {
        let mut guard = self.inner.lock();
        for id in &self.ids {
            if let Some(instance) = guard.instances.get_mut(id) {
                instance.lease_held = false;
            }
        }
    }
}

impl MockInner {
    fn ids_for_digest(&self, digest: &ContentDigest) -> Vec<InvocationId> {
        let mut ids: Vec<InvocationId> = self
            .instances
            .iter()
            .filter(|(_, instance)| fold(&instance.events).claims_snapshot_digest == *digest)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    fn replay(
        &mut self,
        ids: &[InvocationId],
        now: DateTime<Utc>,
    ) -> Result<ResumeReport, DurableError> {
        let drift = std::mem::take(&mut self.drift);
        let mut plans = Vec::with_capacity(ids.len());
        let mut diverged = Vec::new();
        for id in ids {
            let instance = self.instances.get(id).expect("lease set on existing id");
            let before = fold(&instance.events);
            let first = project(&instance.events, now, false);
            let second = project(&instance.events, now, drift);
            if first != second {
                diverged.push(id.clone());
                continue;
            }
            let after = fold(&first);
            plans.push(ReplayPlan {
                id: id.clone(),
                events: first,
                already_terminal: before.phase.is_terminal(),
                timer_resolved: !before.phase.is_terminal() && after.phase.is_terminal(),
            });
        }
        if let Some(id) = diverged.first().cloned() {
            for bad in &diverged {
                if let Some(instance) = self.instances.get_mut(bad) {
                    instance.attention = Some(AttentionReason::NondeterministicReplay);
                }
            }
            // Matching plans are left untouched. The drifted projection is not stored.
            return Err(DurableError::NondeterministicReplay { id });
        }
        let mut pairs = Vec::with_capacity(plans.len());
        let mut already_terminal = Vec::new();
        let mut timer_resolved = Vec::new();
        let mut replayed = Vec::with_capacity(plans.len());
        for plan in plans {
            let instance = self
                .instances
                .get_mut(&plan.id)
                .expect("plan id still present");
            instance.events = plan.events;
            pairs.push((plan.id.clone(), history_digest(&instance.events)));
            if plan.already_terminal {
                already_terminal.push(plan.id.clone());
            }
            if plan.timer_resolved {
                timer_resolved.push(plan.id.clone());
            }
            replayed.push(plan.id);
        }
        Ok(ResumeReport {
            replayed,
            already_terminal,
            timer_resolved,
            history_digest: resume_history_digest(&pairs),
        })
    }
}

struct ReplayPlan {
    id: InvocationId,
    events: Vec<HistoryEvent>,
    already_terminal: bool,
    timer_resolved: bool,
}

fn started_event(request: &DurableStart, run_id: &str, at: DateTime<Utc>) -> HistoryEvent {
    HistoryEvent {
        kind: EventKind::Started {
            claims_snapshot_digest: request.claims_snapshot_digest.clone(),
            definition_digest: request.definition_digest.clone(),
            policy_digest: request.policy_digest.clone(),
            deadline: request.deadline,
            input_ref: request.input_ref.clone(),
            workflow_name: request.workflow_name.clone(),
            workflow_compatibility_version: request.workflow_compatibility_version,
            engine: MockDurableExecutor::engine_identity(),
            run_id: run_id.to_string(),
            at,
        },
    }
}

/// Pure projection. `drift` appends an extra event so a second call can
/// disagree; callers compare the two vectors and drop both when they differ.
fn project(events: &[HistoryEvent], now: DateTime<Utc>, drift: bool) -> Vec<HistoryEvent> {
    let mut projected = events.to_vec();
    let current = fold(&projected);
    if matches!(current.phase, DurablePhase::Running)
        && current.deadline.is_some_and(|deadline| deadline <= now)
    {
        projected.push(HistoryEvent {
            kind: EventKind::DeadlineFired { at: now },
        });
        projected.push(HistoryEvent {
            kind: EventKind::Completed {
                phase: RecordedTerminal::DeadlineExceeded,
                at: now,
            },
        });
    }
    if drift {
        projected.push(HistoryEvent {
            kind: EventKind::DeadlineFired { at: now },
        });
    }
    projected
}

fn apply_signal(
    events: &[HistoryEvent],
    signal: &DurableSignal,
    now: DateTime<Utc>,
) -> Result<Vec<HistoryEvent>, ()> {
    let current = fold(events);
    match signal {
        DurableSignal::Cancel => match current.phase {
            DurablePhase::Cancelled => Ok(events.to_vec()),
            DurablePhase::Running => {
                let mut updated = events.to_vec();
                updated.push(HistoryEvent {
                    kind: EventKind::Cancel { at: now },
                });
                updated.push(HistoryEvent {
                    kind: EventKind::Completed {
                        phase: RecordedTerminal::Cancelled,
                        at: now,
                    },
                });
                Ok(updated)
            }
            _ => Err(()),
        },
        DurableSignal::Approval {
            staging_id,
            outcome,
        } => {
            if !matches!(current.phase, DurablePhase::Running) {
                return Err(());
            }
            if current
                .approvals
                .iter()
                .any(|approval| approval.staging_id == *staging_id)
            {
                return Err(());
            }
            let mut updated = events.to_vec();
            updated.push(HistoryEvent {
                kind: EventKind::Approval {
                    staging_id: staging_id.clone(),
                    outcome: *outcome,
                    at: now,
                },
            });
            Ok(updated)
        }
    }
}

fn fold(events: &[HistoryEvent]) -> Fold {
    let HistoryEvent {
        kind:
            EventKind::Started {
                claims_snapshot_digest,
                definition_digest,
                policy_digest,
                deadline,
                input_ref,
                workflow_name,
                workflow_compatibility_version,
                engine,
                run_id,
                ..
            },
    } = events
        .first()
        .expect("instance history starts with Started")
    else {
        panic!("instance history starts with Started");
    };
    let mut fold = Fold {
        phase: DurablePhase::Running,
        approvals: Vec::new(),
        deadline: *deadline,
        claims_snapshot_digest: claims_snapshot_digest.clone(),
        definition_digest: definition_digest.clone(),
        policy_digest: policy_digest.clone(),
        workflow_name: workflow_name.clone(),
        workflow_compatibility_version: *workflow_compatibility_version,
        engine: engine.clone(),
        run_id: run_id.clone(),
        input_ref_digest: input_ref
            .as_ref()
            .map(|value| ContentDigest::sha256(value.as_bytes())),
        completed_at: None,
        activity_count: 0,
    };
    for event in events.iter().skip(1) {
        match &event.kind {
            EventKind::Started { .. } => {}
            EventKind::Cancel { .. } => {
                if matches!(fold.phase, DurablePhase::Running) {
                    fold.phase = DurablePhase::Cancelled;
                }
            }
            EventKind::Approval {
                staging_id,
                outcome,
                ..
            } => {
                if matches!(fold.phase, DurablePhase::Running) {
                    fold.approvals.push(RecordedApproval {
                        staging_id: staging_id.clone(),
                        outcome: *outcome,
                    });
                }
            }
            EventKind::DeadlineFired { .. } => {
                if matches!(fold.phase, DurablePhase::Running) {
                    fold.phase = DurablePhase::Failed {
                        reason: DurableFailureReason::DeadlineExceeded,
                    };
                }
            }
            EventKind::Completed { phase, at } => {
                fold.phase = match phase {
                    RecordedTerminal::Cancelled => DurablePhase::Cancelled,
                    RecordedTerminal::DeadlineExceeded => DurablePhase::Failed {
                        reason: DurableFailureReason::DeadlineExceeded,
                    },
                };
                fold.completed_at = Some(*at);
            }
        }
    }
    fold
}

fn status_of(id: &InvocationId, instance: &Instance) -> DurableStatus {
    let current = fold(&instance.events);
    let phase = match instance.attention {
        Some(reason) => DurablePhase::NeedsAttention { reason },
        None => current.phase,
    };
    DurableStatus {
        invocation_id: id.clone(),
        run_id: current.run_id,
        phase,
        claims_snapshot_digest: current.claims_snapshot_digest,
        definition_digest: current.definition_digest,
        policy_digest: current.policy_digest,
        workflow_name: current.workflow_name,
        workflow_compatibility_version: current.workflow_compatibility_version,
        engine: current.engine,
        deadline: current.deadline,
        approvals: current.approvals,
        history_digest: history_digest(&instance.events),
        activity_count: current.activity_count,
        lease_held: instance.lease_held,
        input_ref_digest: current.input_ref_digest,
    }
}

fn history_digest(events: &[HistoryEvent]) -> ContentDigest {
    let bytes = serde_json::to_vec(events).expect("history events are serializable");
    ContentDigest::sha256(&bytes)
}

fn validate_signal(signal: &DurableSignal) -> Result<(), DurableError> {
    match signal {
        DurableSignal::Cancel => Ok(()),
        DurableSignal::Approval { staging_id, .. } => validate_staging_id(staging_id),
    }
}

fn validate_staging_id(staging_id: &str) -> Result<(), DurableError> {
    if !(1..=128).contains(&staging_id.len())
        || !staging_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(DurableError::InvalidRequest {
            message: "approval staging id must be 1 to 128 characters of ascii letters, digits, '_' or '-'",
        });
    }
    if contains_plaintext_secret(staging_id) {
        return Err(DurableError::CredentialRejected);
    }
    Ok(())
}
