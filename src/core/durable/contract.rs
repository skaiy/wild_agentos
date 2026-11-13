//! Contract suite for [`super::DurableExecutor`].
//!
//! [`assert_durable_executor_contract`] is the engine-swap suite: a later
//! adapter has to pass the same function. The cases below it are specific to
//! the in-process mock (capability holes, lease conflict, clock, replay drift).

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use super::mock::MockDurableExecutor;
use super::{
    claims_snapshot_digest, probe_required, ApprovalOutcome, AttentionReason, ContentDigest,
    DurableCapabilities, DurableCapability, DurableError, DurableExecutor, DurableFailureReason,
    DurablePhase, DurableSignal, DurableStart, InstalledDurableExecutor, InvocationId,
    RecordedApproval, INVOCATION_WORKFLOW_COMPATIBILITY_VERSION, INVOCATION_WORKFLOW_NAME,
    MAX_HISTORY_RECORD_BYTES, MOCK_ENGINE_NAME, MOCK_ENGINE_VERSION,
};
use crate::isolation::IsolationClaims;

fn claims(actor: &str) -> IsolationClaims {
    IsolationClaims::from_verified("acme", "research_1", actor).unwrap()
}

fn past_deadline() -> DateTime<Utc> {
    Utc::now() - Duration::days(1)
}

fn future_deadline() -> DateTime<Utc> {
    Utc::now() + Duration::days(1)
}

fn request(id: &str, owner: &IsolationClaims, deadline: Option<DateTime<Utc>>) -> DurableStart {
    DurableStart {
        invocation_id: InvocationId::new(id).unwrap(),
        claims_snapshot_digest: claims_snapshot_digest(owner),
        definition_digest: ContentDigest::sha256(b"definition-v1"),
        policy_digest: ContentDigest::sha256(b"policy-v1"),
        deadline,
        input_ref: None,
        workflow_name: INVOCATION_WORKFLOW_NAME.to_string(),
        workflow_compatibility_version: INVOCATION_WORKFLOW_COMPATIBILITY_VERSION,
    }
}

fn ready(factory: &impl Fn() -> Arc<dyn DurableExecutor>) -> Arc<dyn DurableExecutor> {
    InstalledDurableExecutor::install(factory())
        .expect("contract suite refuses an executor that fails the capability probe")
        .executor()
}

fn plaintext_token(fill: char) -> String {
    format!("ghp_{}", fill.to_string().repeat(36))
}

/// Behavioral contract shared by the mock and any later engine.
///
/// A factory that fails [`probe_required`] aborts the suite. Cases are not
/// skipped and the executor is not replaced with a weaker one.
pub async fn assert_durable_executor_contract(factory: impl Fn() -> Arc<dyn DurableExecutor>) {
    accepts_an_engine_with_the_required_capabilities(&factory).await;
    start_query_round_trip(&factory).await;
    duplicate_start_does_not_open_a_second_run(&factory).await;
    unknown_instance_fails_closed(&factory).await;
    cancel_is_recorded_and_idempotent(&factory).await;
    approval_is_recorded_once_and_survives_replay(&factory).await;
    terminal_failure_rejects_a_later_signal(&factory).await;
    replay_of_a_running_instance_is_stable(&factory).await;
    missing_deadline_stays_running(&factory).await;
    resume_does_not_adopt_another_claims_digest(&factory).await;
    forged_snapshot_digest_is_not_resumed(&factory).await;
    purge_is_claims_scoped_and_keeps_non_terminal_instances(&factory).await;
    oversized_input_reference_is_rejected(&factory).await;
    plaintext_credential_is_rejected_without_being_stored(&factory).await;
    empty_or_control_input_reference_is_rejected(&factory).await;
    unregistered_workflow_is_rejected(&factory).await;
    malformed_approval_is_rejected(&factory).await;
}

async fn accepts_an_engine_with_the_required_capabilities(
    factory: &impl Fn() -> Arc<dyn DurableExecutor>,
) {
    let executor = factory();
    let before = executor.capabilities();
    assert!(before.satisfies_required());
    let installed = InstalledDurableExecutor::install(Arc::clone(&executor)).unwrap();
    assert_eq!(installed.capabilities(), before);
    assert!(installed.capabilities().deterministic_replay);
    assert!(installed.capabilities().durable_timers);
    assert!(installed.capabilities().external_signals);
    assert!(installed.capabilities().instance_leases);
    let probed = probe_required(installed.executor().as_ref()).unwrap();
    assert_eq!(probed, before);
    assert_eq!(executor.capabilities(), before);
}

async fn start_query_round_trip(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("starter");
    let reference = "blob://acme/research_1/input";
    let mut start = request("inv_round_trip", &owner, Some(future_deadline()));
    start.input_ref = Some(reference.to_string());
    let handle = executor.start(start.clone()).await.unwrap();
    assert_eq!(handle.invocation_id, start.invocation_id);
    assert!(!handle.run_id.is_empty());
    assert!(!handle.engine.name.is_empty());
    assert!(!handle.engine.version.is_empty());

    let status = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(status.phase, DurablePhase::Running);
    assert_eq!(status.run_id, handle.run_id);
    assert_eq!(status.engine, handle.engine);
    assert_eq!(status.claims_snapshot_digest, start.claims_snapshot_digest);
    assert_eq!(status.definition_digest, start.definition_digest);
    assert_eq!(status.policy_digest, start.policy_digest);
    assert_eq!(status.workflow_name, INVOCATION_WORKFLOW_NAME);
    assert_eq!(
        status.workflow_compatibility_version,
        INVOCATION_WORKFLOW_COMPATIBILITY_VERSION
    );
    assert_eq!(status.deadline, start.deadline);
    assert!(status.approvals.is_empty());
    assert_eq!(status.activity_count, 0);
    assert!(!status.lease_held);
    assert_eq!(
        status.input_ref_digest,
        Some(ContentDigest::sha256(reference.as_bytes()))
    );
}

async fn duplicate_start_does_not_open_a_second_run(
    factory: &impl Fn() -> Arc<dyn DurableExecutor>,
) {
    let executor = ready(factory);
    let owner = claims("starter");
    let start = request("inv_duplicate", &owner, Some(future_deadline()));
    let handle = executor.start(start.clone()).await.unwrap();
    let before = executor.query_status(&start.invocation_id).await.unwrap();
    let err = executor.start(start.clone()).await.unwrap_err();
    assert_eq!(
        err,
        DurableError::AlreadyExists {
            id: start.invocation_id.clone(),
        }
    );
    let after = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(after, before);
    assert_eq!(after.run_id, handle.run_id);
    assert_eq!(after.activity_count, 0);
}

async fn unknown_instance_fails_closed(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let id = InvocationId::new("inv_missing").unwrap();
    assert_eq!(
        executor.query_status(&id).await.unwrap_err(),
        DurableError::NotFound { id: id.clone() }
    );
    assert_eq!(
        executor
            .signal(&id, DurableSignal::Cancel)
            .await
            .unwrap_err(),
        DurableError::NotFound { id }
    );
}

async fn cancel_is_recorded_and_idempotent(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("canceller");
    let start = request("inv_cancel", &owner, Some(future_deadline()));
    executor.start(start.clone()).await.unwrap();
    executor
        .signal(&start.invocation_id, DurableSignal::Cancel)
        .await
        .unwrap();
    let cancelled = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(cancelled.phase, DurablePhase::Cancelled);
    assert_eq!(cancelled.activity_count, 0);
    let digest = cancelled.history_digest.clone();

    executor
        .signal(&start.invocation_id, DurableSignal::Cancel)
        .await
        .unwrap();
    let again = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(again.phase, DurablePhase::Cancelled);
    assert_eq!(again.history_digest, digest);

    let first = executor.resume_all_for_tenant(&owner).await.unwrap();
    let second = executor.resume_all_for_tenant(&owner).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.replayed, vec![start.invocation_id.clone()]);
    assert_eq!(first.already_terminal, vec![start.invocation_id.clone()]);
    assert!(first.timer_resolved.is_empty());
    let after = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(after.history_digest, digest);
    assert_eq!(after.activity_count, 0);
    assert_eq!(after.phase, DurablePhase::Cancelled);
}

async fn approval_is_recorded_once_and_survives_replay(
    factory: &impl Fn() -> Arc<dyn DurableExecutor>,
) {
    let executor = ready(factory);
    let owner = claims("reviewer");
    let start = request("inv_approval", &owner, Some(future_deadline()));
    executor.start(start.clone()).await.unwrap();
    executor
        .signal(
            &start.invocation_id,
            DurableSignal::Approval {
                staging_id: "stage_1".to_string(),
                outcome: ApprovalOutcome::Approved,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        executor
            .signal(
                &start.invocation_id,
                DurableSignal::Approval {
                    staging_id: "stage_1".to_string(),
                    outcome: ApprovalOutcome::Rejected,
                },
            )
            .await
            .unwrap_err(),
        DurableError::IllegalSignal {
            id: start.invocation_id.clone(),
        }
    );
    let before = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(before.phase, DurablePhase::Running);
    assert_eq!(
        before.approvals,
        vec![RecordedApproval {
            staging_id: "stage_1".to_string(),
            outcome: ApprovalOutcome::Approved,
        }]
    );
    let digest = before.history_digest.clone();
    let report = executor.resume_all_for_tenant(&owner).await.unwrap();
    assert_eq!(report.replayed, vec![start.invocation_id.clone()]);
    assert!(report.already_terminal.is_empty());
    assert!(report.timer_resolved.is_empty());
    let after = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(after.history_digest, digest);
    assert_eq!(after.approvals, before.approvals);
    assert_eq!(after.activity_count, 0);

    executor
        .signal(
            &start.invocation_id,
            DurableSignal::Approval {
                staging_id: "stage_2".to_string(),
                outcome: ApprovalOutcome::Rejected,
            },
        )
        .await
        .unwrap();
    executor
        .signal(&start.invocation_id, DurableSignal::Cancel)
        .await
        .unwrap();
    let done = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(done.phase, DurablePhase::Cancelled);
    assert_eq!(done.approvals.len(), 2);
    assert_eq!(done.activity_count, 0);
}

async fn terminal_failure_rejects_a_later_signal(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("deadline");
    let start = request("inv_deadline_signal", &owner, Some(past_deadline()));
    executor.start(start.clone()).await.unwrap();
    let failed = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(
        failed.phase,
        DurablePhase::Failed {
            reason: DurableFailureReason::DeadlineExceeded,
        }
    );
    assert_eq!(failed.activity_count, 0);
    assert_eq!(
        executor
            .signal(&start.invocation_id, DurableSignal::Cancel)
            .await
            .unwrap_err(),
        DurableError::IllegalSignal {
            id: start.invocation_id.clone(),
        }
    );
    assert_eq!(
        executor
            .signal(
                &start.invocation_id,
                DurableSignal::Approval {
                    staging_id: "stage_9".to_string(),
                    outcome: ApprovalOutcome::Rejected,
                },
            )
            .await
            .unwrap_err(),
        DurableError::IllegalSignal {
            id: start.invocation_id.clone(),
        }
    );
    assert_eq!(
        executor.query_status(&start.invocation_id).await.unwrap(),
        failed
    );
}

async fn replay_of_a_running_instance_is_stable(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("runner");
    let start = request("inv_running", &owner, Some(future_deadline()));
    executor.start(start.clone()).await.unwrap();
    let before = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(before.phase, DurablePhase::Running);
    let first = executor.resume_all_for_tenant(&owner).await.unwrap();
    let mid = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(mid.history_digest, before.history_digest);
    assert_eq!(mid.phase, DurablePhase::Running);
    assert_eq!(mid.activity_count, 0);
    let second = executor.resume_all_for_tenant(&owner).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.replayed, vec![start.invocation_id.clone()]);
    assert!(first.timer_resolved.is_empty());
    assert!(first.already_terminal.is_empty());
}

async fn missing_deadline_stays_running(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("owner");
    let start = request("inv_no_deadline", &owner, None);
    executor.start(start.clone()).await.unwrap();
    let status = executor.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(status.phase, DurablePhase::Running);
    assert_eq!(status.deadline, None);
    assert_eq!(status.activity_count, 0);
    let report = executor.resume_all_for_tenant(&owner).await.unwrap();
    assert!(report.timer_resolved.is_empty());
    assert_eq!(
        executor
            .query_status(&start.invocation_id)
            .await
            .unwrap()
            .phase,
        DurablePhase::Running
    );
}

async fn resume_does_not_adopt_another_claims_digest(
    factory: &impl Fn() -> Arc<dyn DurableExecutor>,
) {
    let executor = ready(factory);
    let owner = claims("owner");
    let other = claims("other");
    assert_ne!(
        claims_snapshot_digest(&owner),
        claims_snapshot_digest(&other)
    );
    let start = request("inv_scope", &owner, Some(future_deadline()));
    executor.start(start.clone()).await.unwrap();
    let before = executor.query_status(&start.invocation_id).await.unwrap();
    let skipped = executor.resume_all_for_tenant(&other).await.unwrap();
    assert!(skipped.replayed.is_empty());
    assert_eq!(
        executor.query_status(&start.invocation_id).await.unwrap(),
        before
    );
    let resumed = executor.resume_all_for_tenant(&owner).await.unwrap();
    assert_eq!(resumed.replayed, vec![start.invocation_id.clone()]);
    assert_eq!(
        executor
            .query_status(&start.invocation_id)
            .await
            .unwrap()
            .history_digest,
        before.history_digest
    );
}

async fn forged_snapshot_digest_is_not_resumed(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("owner");
    let mut start = request("inv_forged", &owner, Some(future_deadline()));
    start.claims_snapshot_digest = ContentDigest::sha256(b"forged-snapshot");
    assert_ne!(start.claims_snapshot_digest, claims_snapshot_digest(&owner));
    executor.start(start.clone()).await.unwrap();
    let before = executor.query_status(&start.invocation_id).await.unwrap();
    let report = executor.resume_all_for_tenant(&owner).await.unwrap();
    assert!(report.replayed.is_empty());
    assert_eq!(
        executor.query_status(&start.invocation_id).await.unwrap(),
        before
    );
}

async fn purge_is_claims_scoped_and_keeps_non_terminal_instances(
    factory: &impl Fn() -> Arc<dyn DurableExecutor>,
) {
    let executor = ready(factory);
    let owner = claims("owner");
    let other = claims("other");
    let running = request("inv_purge_running", &owner, Some(future_deadline()));
    let done = request("inv_purge_done", &owner, Some(past_deadline()));
    let foreign = request("inv_purge_foreign", &other, Some(past_deadline()));
    executor.start(running.clone()).await.unwrap();
    executor.start(done.clone()).await.unwrap();
    executor.start(foreign.clone()).await.unwrap();

    let too_early = executor
        .purge(&owner, Utc::now() - Duration::days(2))
        .await
        .unwrap();
    assert!(too_early.purged.is_empty());
    assert_eq!(too_early.retained_non_terminal, 1);
    assert_eq!(too_early.retained_terminal, 1);
    assert_eq!(too_early.retained_other_scope, 1);
    executor.query_status(&done.invocation_id).await.unwrap();

    let purged = executor
        .purge(&owner, Utc::now() + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(purged.purged, vec![done.invocation_id.clone()]);
    assert_eq!(purged.retained_non_terminal, 1);
    assert_eq!(purged.retained_terminal, 0);
    assert_eq!(purged.retained_other_scope, 1);
    assert!(matches!(
        executor.query_status(&done.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));
    assert_eq!(
        executor
            .query_status(&running.invocation_id)
            .await
            .unwrap()
            .phase,
        DurablePhase::Running
    );
    executor.query_status(&foreign.invocation_id).await.unwrap();

    let second = executor
        .purge(&owner, Utc::now() + Duration::hours(1))
        .await
        .unwrap();
    assert!(second.purged.is_empty());
    assert_eq!(second.retained_non_terminal, 1);

    let foreign_purge = executor
        .purge(&other, Utc::now() + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(foreign_purge.purged, vec![foreign.invocation_id]);
    assert_eq!(foreign_purge.retained_other_scope, 1);
}

async fn oversized_input_reference_is_rejected(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("owner");
    let mut start = request("inv_oversize", &owner, Some(future_deadline()));
    start.input_ref = Some("a".repeat(MAX_HISTORY_RECORD_BYTES + 1));
    assert_eq!(
        executor.start(start.clone()).await.unwrap_err(),
        DurableError::HistoryRecordTooLarge {
            size: MAX_HISTORY_RECORD_BYTES + 1,
            max: MAX_HISTORY_RECORD_BYTES,
        }
    );
    assert!(matches!(
        executor.query_status(&start.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));

    start.input_ref = Some("b".repeat(MAX_HISTORY_RECORD_BYTES));
    let reference = start.input_ref.clone().unwrap();
    executor.start(start.clone()).await.unwrap();
    assert_eq!(
        executor
            .query_status(&start.invocation_id)
            .await
            .unwrap()
            .input_ref_digest,
        Some(ContentDigest::sha256(reference.as_bytes()))
    );
}

async fn plaintext_credential_is_rejected_without_being_stored(
    factory: &impl Fn() -> Arc<dyn DurableExecutor>,
) {
    let executor = ready(factory);
    let owner = claims("owner");
    let secret = plaintext_token('a');
    let mut start = request("inv_secret", &owner, None);
    start.input_ref = Some(format!("blob://ref/{secret}"));
    let err = executor.start(start.clone()).await.unwrap_err();
    assert_eq!(err, DurableError::CredentialRejected);
    assert!(!err.to_string().contains("ghp_"));
    assert!(!format!("{err:?}").contains("ghp_"));
    assert!(matches!(
        executor.query_status(&start.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));
}

async fn empty_or_control_input_reference_is_rejected(
    factory: &impl Fn() -> Arc<dyn DurableExecutor>,
) {
    let executor = ready(factory);
    let owner = claims("owner");
    let mut empty = request("inv_empty_ref", &owner, None);
    empty.input_ref = Some(String::new());
    let err = executor.start(empty.clone()).await.unwrap_err();
    assert!(matches!(err, DurableError::InvalidRequest { .. }));
    assert!(matches!(
        executor.query_status(&empty.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));

    let mut control = request("inv_control_ref", &owner, None);
    control.input_ref = Some("blob://ref/\nmore".to_string());
    let err = executor.start(control.clone()).await.unwrap_err();
    assert!(matches!(err, DurableError::InvalidRequest { .. }));
    assert!(!err.to_string().contains('\n'));
    assert!(matches!(
        executor.query_status(&control.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));
}

async fn unregistered_workflow_is_rejected(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("owner");
    let mut version = request("inv_version", &owner, None);
    version.workflow_compatibility_version = 2;
    assert_eq!(
        executor.start(version.clone()).await.unwrap_err(),
        DurableError::IncompatibleWorkflow {
            workflow: INVOCATION_WORKFLOW_NAME.to_string(),
            version: 2,
        }
    );
    assert!(matches!(
        executor.query_status(&version.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));

    let mut name = request("inv_bad_name", &owner, None);
    name.workflow_name = "../other".to_string();
    let err = executor.start(name.clone()).await.unwrap_err();
    assert!(matches!(err, DurableError::InvalidRequest { .. }));
    assert!(!err.to_string().contains(".."));
    assert!(matches!(
        executor.query_status(&name.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));
}

async fn malformed_approval_is_rejected(factory: &impl Fn() -> Arc<dyn DurableExecutor>) {
    let executor = ready(factory);
    let owner = claims("owner");
    let start = request("inv_bad_stage", &owner, Some(future_deadline()));
    executor.start(start.clone()).await.unwrap();
    let before = executor.query_status(&start.invocation_id).await.unwrap();
    let err = executor
        .signal(
            &start.invocation_id,
            DurableSignal::Approval {
                staging_id: String::new(),
                outcome: ApprovalOutcome::Approved,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DurableError::InvalidRequest { .. }));

    let secret = plaintext_token('b');
    let err = executor
        .signal(
            &start.invocation_id,
            DurableSignal::Approval {
                staging_id: secret,
                outcome: ApprovalOutcome::Approved,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err, DurableError::CredentialRejected);
    assert!(!err.to_string().contains("ghp_"));
    assert_eq!(
        executor.query_status(&start.invocation_id).await.unwrap(),
        before
    );
}

#[test]
fn durable_start_has_no_credential_fields() {
    let owner = claims("owner");
    let start = request("inv_shape", &owner, None);
    let value = serde_json::to_value(&start).unwrap();
    let keys: BTreeSet<_> = value.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "invocation_id".to_string(),
            "claims_snapshot_digest".to_string(),
            "definition_digest".to_string(),
            "policy_digest".to_string(),
            "deadline".to_string(),
            "input_ref".to_string(),
            "workflow_name".to_string(),
            "workflow_compatibility_version".to_string(),
        ])
    );
}

#[test]
fn content_digest_and_invocation_id_fail_closed() {
    assert!(ContentDigest::parse("").is_err());
    assert!(ContentDigest::parse("abcd").is_err());
    assert!(ContentDigest::parse(&"g".repeat(64)).is_err());
    let parsed = ContentDigest::parse(&"AB".repeat(32)).unwrap();
    assert_eq!(parsed.as_str(), "ab".repeat(32));
    assert_eq!(ContentDigest::sha256(b"definition-v1").as_str().len(), 64);
    assert!(InvocationId::new("").is_err());
    assert!(InvocationId::new("../inv").is_err());
    let err = InvocationId::new("../inv").unwrap_err();
    assert!(!err.to_string().contains(".."));
    assert_eq!(InvocationId::new("inv_abc").unwrap().as_str(), "inv_abc");

    let owner = claims("owner");
    let other = claims("other");
    assert_ne!(
        claims_snapshot_digest(&owner),
        claims_snapshot_digest(&other)
    );
    assert_eq!(
        claims_snapshot_digest(&owner),
        claims_snapshot_digest(&owner)
    );
    assert!(DurableCapabilities::single_node().satisfies_required());
    assert!(!DurableCapabilities::single_node().multi_node);
    assert!(DurableCapabilities::single_node()
        .missing_required()
        .is_empty());
}

#[tokio::test]
async fn mock_engine_passes_the_durable_contract_suite() {
    assert_durable_executor_contract(|| {
        Arc::new(MockDurableExecutor::new()) as Arc<dyn DurableExecutor>
    })
    .await;
}

#[tokio::test]
async fn missing_required_capabilities_fail_closed() {
    let required = [
        DurableCapability::DeterministicReplay,
        DurableCapability::DurableTimers,
        DurableCapability::ExternalSignals,
        DurableCapability::InstanceLeases,
    ];
    for capability in required {
        let caps = capabilities_without(capability);
        assert!(!caps.satisfies_required());
        let exec = MockDurableExecutor::with_capabilities(caps).unwrap();
        let err = probe_required(&exec).unwrap_err();
        assert!(missing_of(err).contains(&capability));
        assert_eq!(exec.capabilities(), caps);

        let rejected = InstalledDurableExecutor::install(Arc::new(
            MockDurableExecutor::with_capabilities(caps).unwrap(),
        ) as Arc<dyn DurableExecutor>);
        assert!(matches!(
            rejected,
            Err(DurableError::CapabilityUnavailable { .. })
        ));

        assert_operations_fail_closed(&exec, capability).await;
        assert_eq!(exec.instance_count(), 0);
        assert_eq!(exec.capabilities(), caps);
    }

    let none = DurableCapabilities {
        durable_timers: false,
        external_signals: false,
        deterministic_replay: false,
        multi_node: false,
        instance_leases: false,
        versioning: false,
    };
    let exec = MockDurableExecutor::with_capabilities(none).unwrap();
    let err = probe_required(&exec).unwrap_err();
    assert_eq!(
        err.to_string(),
        "durable executor is missing capabilities: deterministic_replay, durable_timers, external_signals, instance_leases"
    );
    assert_eq!(missing_of(err), required);
}

#[tokio::test]
async fn versioning_gap_refuses_mutation_and_does_not_start() {
    let caps = capabilities_without(DurableCapability::Versioning);
    assert!(caps.satisfies_required());
    assert!(!caps.versioning);
    let exec = MockDurableExecutor::with_capabilities(caps).unwrap();
    assert!(probe_required(&exec).is_ok());
    let installed = InstalledDurableExecutor::install(Arc::new(
        MockDurableExecutor::with_capabilities(caps).unwrap(),
    ) as Arc<dyn DurableExecutor>)
    .unwrap();
    assert!(installed.capabilities().satisfies_required());
    assert!(!installed.capabilities().versioning);

    let owner = claims("owner");
    let start = request("inv_no_version", &owner, Some(past_deadline()));
    assert_eq!(
        missing_of(exec.start(start.clone()).await.unwrap_err()),
        vec![DurableCapability::Versioning]
    );
    assert_eq!(
        missing_of(
            exec.signal(&start.invocation_id, DurableSignal::Cancel)
                .await
                .unwrap_err()
        ),
        vec![DurableCapability::Versioning]
    );
    assert_eq!(
        missing_of(exec.resume_all_for_tenant(&owner).await.unwrap_err()),
        vec![DurableCapability::Versioning]
    );
    assert_eq!(
        missing_of(exec.purge(&owner, Utc::now()).await.unwrap_err()),
        vec![DurableCapability::Versioning]
    );
    assert!(matches!(
        exec.query_status(&start.invocation_id).await,
        Err(DurableError::NotFound { .. })
    ));
    assert_eq!(exec.instance_count(), 0);
    assert!(!exec.capabilities().versioning);
}

#[tokio::test]
async fn mock_refuses_to_advertise_multi_node() {
    let mut caps = DurableCapabilities::single_node();
    caps.multi_node = true;
    assert_eq!(
        MockDurableExecutor::with_capabilities(caps).unwrap_err(),
        DurableError::CapabilityUnsupported {
            capability: DurableCapability::MultiNode,
        }
    );
}

#[tokio::test]
async fn mock_reports_single_node_and_its_own_identity() {
    let exec = MockDurableExecutor::new();
    assert!(!exec.capabilities().multi_node);
    assert!(exec.capabilities().satisfies_required());
    let owner = claims("owner");
    let start = request("inv_identity", &owner, Some(future_deadline()));
    let handle = exec.start(start.clone()).await.unwrap();
    assert_eq!(handle.engine.name, MOCK_ENGINE_NAME);
    assert_eq!(handle.engine.version, MOCK_ENGINE_VERSION);
    let status = exec.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(status.engine, handle.engine);
    assert_eq!(status.phase, DurablePhase::Running);
}

#[tokio::test]
async fn install_does_not_start_or_resume() {
    let exec = Arc::new(MockDurableExecutor::new());
    let dyn_exec: Arc<dyn DurableExecutor> = exec.clone();
    let installed = InstalledDurableExecutor::install(dyn_exec).unwrap();
    assert!(installed.capabilities().satisfies_required());
    assert_eq!(exec.instance_count(), 0);
    assert_eq!(exec.resume_calls(), 0);
}

#[tokio::test]
async fn due_timer_resolves_once_on_resume_without_an_activity() {
    let exec = MockDurableExecutor::new();
    let start_at = exec.now();
    let owner = claims("owner");
    let start = request("inv_timer", &owner, Some(start_at + Duration::seconds(60)));
    exec.start(start.clone()).await.unwrap();
    assert_eq!(
        exec.query_status(&start.invocation_id).await.unwrap().phase,
        DurablePhase::Running
    );
    exec.set_time(start_at + Duration::seconds(61));
    assert_eq!(
        exec.query_status(&start.invocation_id).await.unwrap().phase,
        DurablePhase::Running
    );

    let report = exec.resume_all_for_tenant(&owner).await.unwrap();
    assert_eq!(report.timer_resolved, vec![start.invocation_id.clone()]);
    let failed = exec.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(
        failed.phase,
        DurablePhase::Failed {
            reason: DurableFailureReason::DeadlineExceeded,
        }
    );
    assert_eq!(failed.activity_count, 0);
    let digest = failed.history_digest.clone();

    let again = exec.resume_all_for_tenant(&owner).await.unwrap();
    assert_eq!(again.already_terminal, vec![start.invocation_id.clone()]);
    assert!(again.timer_resolved.is_empty());
    let after = exec.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(after.history_digest, digest);
    assert_eq!(after.activity_count, 0);
}

#[tokio::test]
async fn recorded_cancel_wins_over_a_later_deadline() {
    let exec = MockDurableExecutor::new();
    let start_at = exec.now();
    let owner = claims("owner");
    let start = request(
        "inv_cancel_deadline",
        &owner,
        Some(start_at + Duration::seconds(60)),
    );
    exec.start(start.clone()).await.unwrap();
    exec.signal(&start.invocation_id, DurableSignal::Cancel)
        .await
        .unwrap();
    let cancelled = exec.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(cancelled.phase, DurablePhase::Cancelled);
    let digest = cancelled.history_digest.clone();

    exec.set_time(start_at + Duration::seconds(120));
    let report = exec.resume_all_for_tenant(&owner).await.unwrap();
    assert_eq!(report.already_terminal, vec![start.invocation_id.clone()]);
    assert!(report.timer_resolved.is_empty());
    let after = exec.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(after.phase, DurablePhase::Cancelled);
    assert_eq!(after.history_digest, digest);
    assert_eq!(after.activity_count, 0);
}

#[tokio::test]
async fn purge_treats_before_as_exclusive() {
    let exec = MockDurableExecutor::new();
    let owner = claims("owner");
    let completed_at = exec.now();
    let start = request(
        "inv_boundary",
        &owner,
        Some(completed_at - Duration::seconds(1)),
    );
    exec.start(start.clone()).await.unwrap();
    let keep = exec.purge(&owner, completed_at).await.unwrap();
    assert!(keep.purged.is_empty());
    assert_eq!(keep.retained_terminal, 1);
    let dropped = exec
        .purge(&owner, completed_at + Duration::milliseconds(1))
        .await
        .unwrap();
    assert_eq!(dropped.purged, vec![start.invocation_id]);
}

#[tokio::test]
async fn nondeterministic_replay_fails_closed() {
    let exec = MockDurableExecutor::new();
    let owner = claims("owner");
    let start = request("inv_drift", &owner, Some(future_deadline()));
    exec.start(start.clone()).await.unwrap();
    let before = exec.query_status(&start.invocation_id).await.unwrap();
    exec.inject_replay_drift();
    assert_eq!(
        exec.resume_all_for_tenant(&owner).await.unwrap_err(),
        DurableError::NondeterministicReplay {
            id: start.invocation_id.clone(),
        }
    );
    let attention = exec.query_status(&start.invocation_id).await.unwrap();
    assert_eq!(
        attention.phase,
        DurablePhase::NeedsAttention {
            reason: AttentionReason::NondeterministicReplay,
        }
    );
    assert_eq!(attention.history_digest, before.history_digest);
    assert_eq!(attention.activity_count, 0);
    assert_eq!(
        exec.resume_all_for_tenant(&owner).await.unwrap_err(),
        DurableError::NondeterministicReplay {
            id: start.invocation_id.clone(),
        }
    );
    assert_eq!(
        exec.query_status(&start.invocation_id)
            .await
            .unwrap()
            .history_digest,
        before.history_digest
    );

    let purge = exec
        .purge(&owner, Utc::now() + Duration::hours(1))
        .await
        .unwrap();
    assert!(purge.purged.is_empty());
    assert_eq!(purge.retained_non_terminal, 1);
    assert!(exec.query_status(&start.invocation_id).await.is_ok());
}

#[tokio::test]
async fn instance_lease_conflict_does_not_run_a_second_owner() {
    let exec = MockDurableExecutor::new();
    let owner = claims("owner");
    let start = request("inv_lease", &owner, Some(future_deadline()));
    exec.start(start.clone()).await.unwrap();
    let digest = exec
        .query_status(&start.invocation_id)
        .await
        .unwrap()
        .history_digest;
    let pause = exec.pause_next_resume();
    let worker = exec.clone();
    let worker_claims = owner.clone();
    let task = tokio::spawn(async move { worker.resume_all_for_tenant(&worker_claims).await });

    pause.entered.notified().await;
    let held = exec.query_status(&start.invocation_id).await.unwrap();
    assert!(held.lease_held);
    assert_eq!(held.history_digest, digest);
    assert_eq!(
        exec.resume_all_for_tenant(&owner).await.unwrap_err(),
        DurableError::LeaseConflict {
            id: start.invocation_id.clone(),
        }
    );
    assert_eq!(
        exec.signal(&start.invocation_id, DurableSignal::Cancel)
            .await
            .unwrap_err(),
        DurableError::LeaseConflict {
            id: start.invocation_id.clone(),
        }
    );
    assert_eq!(
        exec.query_status(&start.invocation_id)
            .await
            .unwrap()
            .history_digest,
        digest
    );

    pause.release.notify_one();
    let report = task.await.unwrap().unwrap();
    assert_eq!(report.replayed, vec![start.invocation_id.clone()]);
    let after = exec.query_status(&start.invocation_id).await.unwrap();
    assert!(!after.lease_held);
    assert_eq!(after.phase, DurablePhase::Running);
    assert_eq!(after.history_digest, digest);
    assert_eq!(after.activity_count, 0);
}

fn capabilities_without(capability: DurableCapability) -> DurableCapabilities {
    let mut caps = DurableCapabilities::single_node();
    match capability {
        DurableCapability::DeterministicReplay => caps.deterministic_replay = false,
        DurableCapability::DurableTimers => caps.durable_timers = false,
        DurableCapability::ExternalSignals => caps.external_signals = false,
        DurableCapability::InstanceLeases => caps.instance_leases = false,
        DurableCapability::Versioning => caps.versioning = false,
        DurableCapability::MultiNode => caps.multi_node = true,
    }
    caps
}

fn missing_of(error: DurableError) -> Vec<DurableCapability> {
    match error {
        DurableError::CapabilityUnavailable { missing } => missing.into_vec(),
        other => panic!("expected a capability error, got {other}"),
    }
}

async fn assert_operations_fail_closed(exec: &MockDurableExecutor, needle: DurableCapability) {
    let owner = claims("owner");
    let start = request("inv_closed", &owner, Some(past_deadline()));
    let id = start.invocation_id.clone();
    let results = [
        exec.start(start).await.map(|_| ()),
        exec.signal(&id, DurableSignal::Cancel).await,
        exec.query_status(&id).await.map(|_| ()),
        exec.resume_all_for_tenant(&owner).await.map(|_| ()),
        exec.purge(&owner, future_deadline()).await.map(|_| ()),
    ];
    for result in results {
        let err = result.expect_err("missing capability returned success");
        match err {
            DurableError::CapabilityUnavailable { missing } => {
                assert!(
                    missing.as_slice().contains(&needle),
                    "{missing} does not name {needle}"
                );
            }
            other => panic!("expected a capability failure, got {other}"),
        }
    }
}
