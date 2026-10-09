//! Unit tests for the invocation lifecycle store (#316).

use std::{collections::BTreeSet, sync::Arc};

use axum::body::to_bytes;

use super::*;

const CANARY_PROMPT: &str = "canary-prompt-7f3a-do-not-log";

fn claims(tenant: &str, project: &str, actor: &str) -> IsolationClaims {
    IsolationClaims::from_verified(tenant, project, actor).unwrap()
}

fn alice() -> IsolationClaims {
    claims("tenant-a", "project-a", "alice")
}

fn new_invocation() -> NewInvocation {
    NewInvocation {
        request: InvocationRequest {
            prompt: Some(CANARY_PROMPT.to_string()),
            agent_id: None,
            metadata: Map::new(),
            ..InvocationRequest::default()
        },
        task_iri: None,
        idempotency: None,
    }
}

fn open_store(dir: &tempfile::TempDir) -> InvocationStore {
    InvocationStore::open(dir.path().join("invocations.json"))
        .unwrap()
        .0
}

/// Allowed path from `queued` to each state.
fn path_to(state: InvocationState) -> Vec<InvocationState> {
    use InvocationState::*;
    match state {
        Queued => vec![],
        Running => vec![Running],
        CancelRequested => vec![Running, CancelRequested],
        Succeeded => vec![Running, Succeeded],
        Failed => vec![Running, Failed],
        Cancelled => vec![Cancelled],
    }
}

async fn invocation_in(
    store: &InvocationStore,
    claims: &IsolationClaims,
    state: InvocationState,
) -> Invocation {
    let mut invocation = store
        .create_for_claims(claims, new_invocation())
        .await
        .unwrap();
    for step in path_to(state) {
        invocation = store
            .transition_for_claims(
                claims,
                &invocation.id,
                Some(invocation.revision),
                step,
                TransitionPatch::default(),
            )
            .await
            .unwrap();
    }
    assert_eq!(invocation.state, state);
    invocation
}

fn disk_records(store: &InvocationStore) -> Vec<Invocation> {
    serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap()
}

async fn body_bytes(response: Response) -> (StatusCode, axum::body::Bytes) {
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 64 * 1024).await.unwrap(),
    )
}

#[test]
fn invocations_lifecycle_permits_matches_graph_exactly() {
    use InvocationState::*;
    let allowed: BTreeSet<(&str, &str)> = [
        (Queued, Running),
        (Queued, Cancelled),
        (Running, Succeeded),
        (Running, Failed),
        (Running, CancelRequested),
        (CancelRequested, Cancelled),
        (CancelRequested, Succeeded),
        (CancelRequested, Failed),
    ]
    .into_iter()
    .map(|(a, b)| (a.as_str(), b.as_str()))
    .collect();
    for from in InvocationState::ALL {
        for to in InvocationState::ALL {
            assert_eq!(
                from.permits(to),
                allowed.contains(&(from.as_str(), to.as_str())),
                "{from:?} -> {to:?}"
            );
        }
        if from.is_terminal() {
            assert!(InvocationState::ALL.iter().all(|to| !from.permits(*to)));
        }
    }
    for state in InvocationState::ALL {
        assert!(!state.permits(state), "no self-loop edge for {state:?}");
    }
    assert_eq!(Queued.cancel_target(), Some(Cancelled));
    assert_eq!(Running.cancel_target(), Some(CancelRequested));
    // Repeated cancels target the current state (idempotent no-op).
    assert_eq!(CancelRequested.cancel_target(), Some(CancelRequested));
    assert_eq!(Cancelled.cancel_target(), Some(Cancelled));
    for state in [Succeeded, Failed] {
        assert_eq!(state.cancel_target(), None);
    }
}

#[tokio::test]
async fn invocations_lifecycle_store_enforces_all_36_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    for from in InvocationState::ALL {
        for to in InvocationState::ALL {
            let invocation = invocation_in(&store, &claims, from).await;
            let outcome = store
                .transition_outcome_for_claims(
                    &claims,
                    &invocation.id,
                    Some(invocation.revision),
                    to,
                    TransitionPatch::default(),
                )
                .await;
            if from == to {
                // 6 diagonal pairs: idempotent success, nothing written.
                let outcome = outcome.unwrap_or_else(|e| panic!("{from:?}->{to:?}: {e}"));
                assert!(!outcome.changed, "{from:?} -> {to:?}");
                assert_eq!(outcome.invocation, invocation);
                let unchanged = store.get_for_claims(&claims, &invocation.id).await.unwrap();
                assert_eq!(unchanged, invocation);
            } else if from.permits(to) {
                let outcome = outcome.unwrap_or_else(|e| panic!("{from:?}->{to:?}: {e}"));
                assert!(outcome.changed);
                let updated = outcome.invocation;
                assert_eq!(updated.state, to);
                assert_eq!(updated.revision, invocation.revision + 1);
            } else {
                let outcome = outcome.map(|o| o.invocation);
                assert_eq!(
                    outcome,
                    Err(InvocationStoreError::IllegalTransition { from, to }),
                    "{from:?} -> {to:?}"
                );
                let unchanged = store.get_for_claims(&claims, &invocation.id).await.unwrap();
                assert_eq!(unchanged, invocation);
            }
        }
    }
}

#[tokio::test]
async fn invocations_lifecycle_revision_and_audit_track_each_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    let created = store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap();
    assert!(created.id.starts_with("inv_"));
    assert_eq!(created.object, "invocation");
    assert_eq!(created.state, InvocationState::Queued);
    assert_eq!(created.revision, 1);
    assert_eq!(
        (
            created.tenant_id.as_str(),
            created.project_id.as_str(),
            created.actor_id.as_str()
        ),
        ("tenant-a", "project-a", "alice")
    );
    assert_eq!(created.etag(), HeaderValue::from_static("\"1\""));

    let worker = claims_with_actor("worker");
    let running = store
        .transition_for_claims(
            &worker,
            &created.id,
            None,
            InvocationState::Running,
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    assert_eq!(running.revision, 2);
    assert!(running.started_at.is_some());
    assert!(running.completed_at.is_none());

    let done = store
        .transition_for_claims(
            &worker,
            &created.id,
            Some(2),
            InvocationState::Succeeded,
            TransitionPatch {
                result: Some(InvocationResult {
                    summary: "canary-result-summary".into(),
                    artifacts: vec![],
                    usage: None,
                }),
                error: None,
                task_iri: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(done.revision, 3);
    assert!(done.completed_at.is_some());
    assert_eq!(
        done.result.as_ref().unwrap().summary,
        "canary-result-summary"
    );
    assert_eq!(done.etag(), HeaderValue::from_static("\"3\""));

    let trail: Vec<_> = done
        .audit_events
        .iter()
        .map(|e| (e.from, e.to, e.revision, e.actor_id.as_str()))
        .collect();
    assert_eq!(
        trail,
        vec![
            (None, InvocationState::Queued, 1, "alice"),
            (
                Some(InvocationState::Queued),
                InvocationState::Running,
                2,
                "worker"
            ),
            (
                Some(InvocationState::Running),
                InvocationState::Succeeded,
                3,
                "worker"
            ),
        ]
    );
    let audit_json = serde_json::to_string(&done.audit_events).unwrap();
    assert!(!audit_json.contains(CANARY_PROMPT));
    assert!(!audit_json.contains("canary-result-summary"));

    assert_eq!(disk_records(&store), vec![done]);
}

fn claims_with_actor(actor: &str) -> IsolationClaims {
    claims("tenant-a", "project-a", actor)
}

#[test]
fn invocations_lifecycle_audit_events_are_capped() {
    let mut invocation = Invocation {
        id: "inv_x".into(),
        object: "invocation".into(),
        tenant_id: "t".into(),
        project_id: "p".into(),
        actor_id: "a".into(),
        state: InvocationState::Queued,
        revision: 1,
        request: InvocationRequest::default(),
        task_iri: None,
        result: None,
        error: None,
        idempotency_key: None,
        idempotency: None,
        created_at: String::new(),
        updated_at: String::new(),
        started_at: None,
        completed_at: None,
        audit_events: vec![],
    };
    for revision in 1..=(MAX_AUDIT_EVENTS as u64 + 10) {
        invocation.push_audit(InvocationAuditEvent {
            at: String::new(),
            from: None,
            to: InvocationState::Queued,
            revision,
            actor_id: "a".into(),
        });
    }
    assert_eq!(invocation.audit_events.len(), MAX_AUDIT_EVENTS);
    assert_eq!(invocation.audit_events[0].revision, 11);
    assert_eq!(
        invocation.audit_events.last().unwrap().revision,
        MAX_AUDIT_EVENTS as u64 + 10
    );
}

#[test]
fn invocations_lifecycle_error_message_is_truncated_on_char_boundary() {
    let long = "é".repeat(MAX_ERROR_MESSAGE_BYTES);
    let error = InvocationErrorInfo::new("x", long);
    assert!(error.message.len() <= MAX_ERROR_MESSAGE_BYTES);
    assert!(error.message.chars().all(|c| c == 'é'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invocations_lifecycle_cas_two_writers_same_revision_exactly_one_wins() {
    for _ in 0..50 {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(open_store(&dir));
        let claims = alice();
        let created = store
            .create_for_claims(&claims, new_invocation())
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for target in [InvocationState::Running, InvocationState::Cancelled] {
            let (store, claims, barrier, id) = (
                store.clone(),
                claims.clone(),
                barrier.clone(),
                created.id.clone(),
            );
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .transition_for_claims(
                        &claims,
                        &id,
                        Some(1),
                        target,
                        TransitionPatch::default(),
                    )
                    .await
            }));
        }
        let mut wins = 0;
        let mut conflicts = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(updated) => {
                    assert_eq!(updated.revision, 2);
                    wins += 1;
                }
                Err(InvocationStoreError::RevisionConflict { current }) => {
                    assert_eq!(current, 2);
                    conflicts += 1;
                }
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!((wins, conflicts), (1, 1));
        let stored = store.get_for_claims(&claims, &created.id).await.unwrap();
        assert_eq!(stored.revision, 2);
    }
}

/// Deterministic xorshift so the stress test needs no extra dependency.
fn next_random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn invocations_lifecycle_stress_16_writers_1000_random_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(open_store(&dir));
    let claims = alice();
    let created = store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap();
    let mut handles = Vec::new();
    for writer in 0..16u64 {
        let (store, claims, id) = (store.clone(), claims.clone(), created.id.clone());
        handles.push(tokio::spawn(async move {
            let mut seed = 0x9E37_79B9_7F4A_7C15 ^ (writer + 1);
            let mut won = Vec::new();
            for _ in 0..1000 {
                let roll = next_random(&mut seed);
                let target = InvocationState::ALL[(roll % 6) as usize];
                let expected = match (roll >> 8) % 3 {
                    0 => None,
                    1 => Some(store.get_for_claims(&claims, &id).await.unwrap().revision),
                    _ => Some((roll >> 16) % 5),
                };
                match store
                    .transition_outcome_for_claims(
                        &claims,
                        &id,
                        expected,
                        target,
                        TransitionPatch::default(),
                    )
                    .await
                {
                    Ok(outcome) if outcome.changed => won.push(outcome.invocation.revision),
                    Ok(_) => {}
                    Err(
                        InvocationStoreError::RevisionConflict { .. }
                        | InvocationStoreError::IllegalTransition { .. },
                    ) => {}
                    Err(other) => panic!("unexpected {other:?}"),
                }
                if roll % 7 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            won
        }));
    }
    let timeout = std::time::Duration::from_secs(120);
    let mut revisions = Vec::new();
    for handle in handles {
        revisions.extend(
            tokio::time::timeout(timeout, handle)
                .await
                .expect("no deadlock")
                .unwrap(),
        );
    }
    let successes = revisions.len() as u64;
    let unique: BTreeSet<u64> = revisions.iter().copied().collect();
    assert_eq!(
        unique.len() as u64,
        successes,
        "no two writers won one revision"
    );
    assert_eq!(unique, (2..=successes + 1).collect::<BTreeSet<_>>());
    let stored = store.get_for_claims(&claims, &created.id).await.unwrap();
    assert_eq!(stored.revision, successes + 1);
    assert_eq!(stored.audit_events.len() as u64, successes + 1);
    assert!(stored.state.is_terminal());
    assert_eq!(disk_records(&store), vec![stored]);
}

#[tokio::test]
async fn invocations_lifecycle_cancel_after_success_and_stale_if_match_are_409() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    let done = invocation_in(&store, &claims, InvocationState::Succeeded).await;
    assert_eq!(done.state.cancel_target(), None);
    let error = store
        .transition_for_claims(
            &claims,
            &done.id,
            Some(done.revision),
            InvocationState::Cancelled,
            TransitionPatch::default(),
        )
        .await
        .unwrap_err();
    let (status, body) = body_bytes(error.into_response()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "illegal_transition");

    let running = invocation_in(&store, &claims, InvocationState::Running).await;
    let mut headers = HeaderMap::new();
    headers.insert(header::IF_MATCH, HeaderValue::from_static("\"1\""));
    let expected = parse_if_match(&headers).unwrap();
    let error = store
        .transition_for_claims(
            &claims,
            &running.id,
            expected,
            running.state.cancel_target().unwrap(),
            TransitionPatch::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, InvocationStoreError::RevisionConflict { current: 2 });
    let response = error.into_response();
    assert_eq!(response.headers()[header::ETAG], "\"2\"");
    let (status, body) = body_bytes(response).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "revision_conflict");
    assert_eq!(body["current_revision"], 2);
}

#[tokio::test]
async fn invocations_lifecycle_cross_scope_is_byte_identical_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let owner = alice();
    let created = store
        .create_for_claims(&owner, new_invocation())
        .await
        .unwrap();

    let (_, unknown_body) = body_bytes(
        store
            .get_for_claims(&owner, "inv_does_not_exist")
            .await
            .unwrap_err()
            .into_response(),
    )
    .await;
    let unknown_transition = store
        .transition_for_claims(
            &owner,
            "inv_does_not_exist",
            None,
            InvocationState::Cancelled,
            TransitionPatch::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown_transition, InvocationStoreError::NotFound);

    for outsider in [
        claims("tenant-b", "project-a", "alice"),
        claims("tenant-a", "project-b", "alice"),
    ] {
        let read = store
            .get_for_claims(&outsider, &created.id)
            .await
            .unwrap_err();
        assert_eq!(read, InvocationStoreError::NotFound);
        let (status, body) = body_bytes(read.into_response()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, unknown_body);

        // A wrong revision must not leak the current revision across scopes.
        let write = store
            .transition_for_claims(
                &outsider,
                &created.id,
                Some(99),
                InvocationState::Cancelled,
                TransitionPatch::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(write, InvocationStoreError::NotFound);
        let (_, body) = body_bytes(write.into_response()).await;
        assert_eq!(body, unknown_body);
        assert!(store.list_for_claims(&outsider, None).await.is_empty());
    }

    // Same scope, different actor: may read (decision on #313).
    let colleague = claims_with_actor("bob");
    assert_eq!(
        store.get_for_claims(&colleague, &created.id).await.unwrap(),
        created
    );
    assert_eq!(
        store.list_for_claims(&colleague, None).await,
        vec![created.clone()]
    );
    assert!(store
        .list_for_claims(&colleague, Some(InvocationState::Running))
        .await
        .is_empty());
    let unchanged = store.get_for_claims(&owner, &created.id).await.unwrap();
    assert_eq!(unchanged, created);
}

#[cfg(unix)]
#[tokio::test]
async fn invocations_lifecycle_persist_failure_keeps_memory_and_disk() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let (store, _) = InvocationStore::open(store_dir.join("invocations.json")).unwrap();
    let claims = alice();
    let created = store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap();
    let before_disk = std::fs::read(store.path()).unwrap();

    std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let transition = store
        .transition_for_claims(
            &claims,
            &created.id,
            Some(1),
            InvocationState::Running,
            TransitionPatch::default(),
        )
        .await;
    let create = store.create_for_claims(&claims, new_invocation()).await;
    std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = transition.unwrap_err();
    assert!(matches!(error, InvocationStoreError::Persistence(_)));
    assert!(matches!(create, Err(InvocationStoreError::Persistence(_))));
    let (status, body) = body_bytes(error.into_response()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let body_text = String::from_utf8(body.to_vec()).unwrap();
    assert!(!body_text.contains(store_dir.to_str().unwrap()));

    assert_eq!(
        store.get_for_claims(&claims, &created.id).await.unwrap(),
        created
    );
    assert_eq!(store.list_for_claims(&claims, None).await.len(), 1);
    assert_eq!(std::fs::read(store.path()).unwrap(), before_disk);

    // The store recovers once the directory is writable again.
    let running = store
        .transition_for_claims(
            &claims,
            &created.id,
            Some(1),
            InvocationState::Running,
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    assert_eq!(running.revision, 2);
}

#[tokio::test]
async fn invocations_lifecycle_restart_marks_unfinished_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invocations.json");
    let claims = alice();
    let mut before = Vec::new();
    {
        let (store, report) = InvocationStore::open(&path).unwrap();
        assert_eq!(report, RecoveryReport::default());
        for state in InvocationState::ALL {
            before.push(invocation_in(&store, &claims, state).await);
        }
    }

    let (store, report) = InvocationStore::open(&path).unwrap();
    assert_eq!(
        report,
        RecoveryReport {
            loaded: 6,
            interrupted: 3,
            swept: 0,
            idempotency_expired: 0,
        }
    );
    for old in &before {
        let now = store.get_for_claims(&claims, &old.id).await.unwrap();
        if old.state.is_terminal() {
            assert_eq!(&now, old, "terminal records are untouched");
            continue;
        }
        assert_eq!(now.state, InvocationState::Failed);
        assert_eq!(now.revision, old.revision + 1);
        assert_eq!(now.error.as_ref().unwrap().code, INTERRUPTED_ERROR_CODE);
        assert!(now.completed_at.is_some());
        let last = now.audit_events.last().unwrap();
        assert_eq!(
            (last.from, last.to, last.revision, last.actor_id.as_str()),
            (
                Some(old.state),
                InvocationState::Failed,
                now.revision,
                SYSTEM_ACTOR_ID
            )
        );
        // Recovery is persisted: a second restart is a no-op.
        let error = store
            .transition_for_claims(
                &claims,
                &now.id,
                None,
                InvocationState::Running,
                TransitionPatch::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            InvocationStoreError::IllegalTransition { .. }
        ));
    }
    drop(store);
    let (_, report) = InvocationStore::open(&path).unwrap();
    assert_eq!(
        report,
        RecoveryReport {
            loaded: 6,
            interrupted: 0,
            swept: 0,
            idempotency_expired: 0,
        }
    );
}

#[test]
fn invocations_lifecycle_corrupt_store_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invocations.json");
    std::fs::write(&path, b"{not json").unwrap();
    assert!(matches!(
        InvocationStore::open(&path),
        Err(InvocationStoreError::Persistence(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
}

#[test]
fn invocations_lifecycle_if_match_parsing() {
    let parse = |values: &[&'static str]| {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::IF_MATCH, HeaderValue::from_static(value));
        }
        parse_if_match(&headers)
    };
    assert_eq!(parse(&[]), Ok(None));
    assert_eq!(parse(&["*"]), Ok(None));
    assert_eq!(parse(&["\"7\""]), Ok(Some(7)));
    assert_eq!(parse(&[" \"12\" "]), Ok(Some(12)));
    for bad in [
        "7",
        "W/\"7\"",
        "\"\"",
        "\"-1\"",
        "\"+1\"",
        "\"1\", \"2\"",
        "\"abc\"",
    ] {
        assert_eq!(parse(&[bad]), Err(InvalidIfMatch), "{bad}");
    }
    assert_eq!(parse(&["\"1\"", "\"2\""]), Err(InvalidIfMatch));
    assert_eq!(parse(&["\"99999999999999999999999\""]), Err(InvalidIfMatch));
    assert_eq!(etag_for_revision(42), HeaderValue::from_static("\"42\""));
}

#[tokio::test]
async fn invocations_lifecycle_invalid_if_match_is_400() {
    let (status, body) = body_bytes(InvalidIfMatch.into_response()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "invalid_if_match");
}

#[tokio::test]
async fn invocations_lifecycle_outcome_after_cancel_requested_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    for (outcome, patch) in [
        (
            InvocationState::Succeeded,
            TransitionPatch {
                result: Some(InvocationResult {
                    summary: "finished-before-cancel".into(),
                    artifacts: vec![],
                    usage: None,
                }),
                error: None,
                task_iri: None,
            },
        ),
        (
            InvocationState::Failed,
            TransitionPatch {
                result: None,
                error: Some(InvocationErrorInfo::new("execution_failed", "boom")),
                task_iri: None,
            },
        ),
    ] {
        let cancelling = invocation_in(&store, &claims, InvocationState::CancelRequested).await;
        let done = store
            .transition_for_claims(
                &claims,
                &cancelling.id,
                Some(cancelling.revision),
                outcome,
                patch,
            )
            .await
            .unwrap();
        assert_eq!(done.state, outcome);
        assert_eq!(done.revision, cancelling.revision + 1);
        assert!(done.completed_at.is_some());
        match outcome {
            InvocationState::Succeeded => {
                assert_eq!(done.result.unwrap().summary, "finished-before-cancel")
            }
            _ => assert_eq!(done.error.unwrap().code, "execution_failed"),
        }
    }
}

#[tokio::test]
async fn invocations_lifecycle_same_state_repeat_is_idempotent_success() {
    use InvocationState::*;
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();

    // Repeated cancel on a running invocation: first moves to
    // cancel_requested, second is a no-op even with a stale If-Match.
    let running = invocation_in(&store, &claims, Running).await;
    let first = store
        .transition_outcome_for_claims(
            &claims,
            &running.id,
            Some(running.revision),
            running.state.cancel_target().unwrap(),
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    assert!(first.changed);
    assert_eq!(first.invocation.state, CancelRequested);
    let disk_before = disk_records(&store);
    let again = store
        .transition_outcome_for_claims(
            &claims,
            &running.id,
            Some(running.revision),
            first.invocation.state.cancel_target().unwrap(),
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    assert!(!again.changed);
    assert_eq!(again.invocation, first.invocation);
    assert_eq!(disk_records(&store), disk_before);

    // Repeated cancel on a cancelled invocation.
    let cancelled = invocation_in(&store, &claims, Cancelled).await;
    let again = store
        .transition_outcome_for_claims(
            &claims,
            &cancelled.id,
            None,
            cancelled.state.cancel_target().unwrap(),
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    assert!(!again.changed);
    assert_eq!(again.invocation, cancelled);

    // Duplicate worker delivery of a terminal outcome: the stored result is
    // kept, the duplicate patch is ignored.
    let done = store
        .transition_for_claims(
            &claims,
            &invocation_in(&store, &claims, Running).await.id,
            None,
            Succeeded,
            TransitionPatch {
                result: Some(InvocationResult {
                    summary: "first-delivery".into(),
                    artifacts: vec![],
                    usage: None,
                }),
                error: None,
                task_iri: None,
            },
        )
        .await
        .unwrap();
    let disk_before = disk_records(&store);
    let duplicate = store
        .transition_outcome_for_claims(
            &claims,
            &done.id,
            Some(done.revision),
            Succeeded,
            TransitionPatch {
                result: Some(InvocationResult {
                    summary: "second-delivery".into(),
                    artifacts: vec![],
                    usage: None,
                }),
                error: None,
                task_iri: None,
            },
        )
        .await
        .unwrap();
    assert!(!duplicate.changed);
    assert_eq!(duplicate.invocation, done);
    assert_eq!(
        duplicate.invocation.result.unwrap().summary,
        "first-delivery"
    );
    assert_eq!(disk_records(&store), disk_before);

    // Leaving a terminal state for a different one is still 409.
    for (terminal, other) in [
        (Succeeded, Cancelled),
        (Failed, Succeeded),
        (Cancelled, Failed),
    ] {
        let record = invocation_in(&store, &claims, terminal).await;
        assert_eq!(
            store
                .transition_for_claims(&claims, &record.id, None, other, TransitionPatch::default())
                .await,
            Err(InvocationStoreError::IllegalTransition {
                from: terminal,
                to: other
            })
        );
    }

    // Scope is still checked first: a repeat from another scope is 404.
    let outsider = IsolationClaims::from_verified("tenant-b", "project-a", "alice").unwrap();
    assert_eq!(
        store
            .transition_for_claims(
                &outsider,
                &done.id,
                None,
                Succeeded,
                TransitionPatch::default()
            )
            .await,
        Err(InvocationStoreError::NotFound)
    );
}

#[test]
fn invocations_lifecycle_queued_to_failed_only_for_approved_pre_exec_codes() {
    use crate::api::http::invocations_store::{
        PROJECTION_CONTEXT_MISSING_ERROR_CODE, TASK_INIT_FAILED_ERROR_CODE,
    };
    use InvocationState::*;
    let approved = [
        InvocationErrorInfo::new(DEADLINE_EXCEEDED_ERROR_CODE, "deadline passed"),
        InvocationErrorInfo::new(PROJECTION_CONTEXT_MISSING_ERROR_CODE, "no projection"),
        InvocationErrorInfo::new(TASK_INIT_FAILED_ERROR_CODE, "init failed"),
    ];
    let other = InvocationErrorInfo::new("execution_failed", "boom");
    for from in InvocationState::ALL {
        for to in InvocationState::ALL {
            let conditional = from == Queued && to == Failed;
            assert_eq!(
                from.permits_with(to, None),
                from.permits(to),
                "{from:?}->{to:?}"
            );
            assert_eq!(
                from.permits_with(to, Some(&other)),
                from.permits(to),
                "{from:?}->{to:?}"
            );
            for code in &approved {
                assert_eq!(
                    from.permits_with(to, Some(code)),
                    from.permits(to) || conditional,
                    "{from:?}->{to:?} code={}",
                    code.code
                );
            }
        }
    }
    assert!(!Queued.permits(Failed));
}

#[tokio::test]
async fn invocations_lifecycle_store_queued_to_failed_requires_approved_reason() {
    use InvocationState::*;
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    let queued = invocation_in(&store, &claims, Queued).await;
    for patch in [
        TransitionPatch::default(),
        TransitionPatch {
            result: None,
            error: Some(InvocationErrorInfo::new("execution_failed", "boom")),
            task_iri: None,
        },
    ] {
        assert_eq!(
            store
                .transition_for_claims(&claims, &queued.id, Some(1), Failed, patch)
                .await,
            Err(InvocationStoreError::IllegalTransition {
                from: Queued,
                to: Failed
            })
        );
        assert_eq!(
            store.get_for_claims(&claims, &queued.id).await.unwrap(),
            queued
        );
    }
    let expired = store
        .transition_for_claims(
            &claims,
            &queued.id,
            Some(1),
            Failed,
            TransitionPatch {
                result: None,
                error: Some(InvocationErrorInfo::new(
                    DEADLINE_EXCEEDED_ERROR_CODE,
                    "deadline passed",
                )),
                task_iri: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(expired.state, Failed);
    assert_eq!(expired.revision, 2);
    assert!(expired.completed_at.is_some());
    assert!(expired.started_at.is_none());
    assert_eq!(expired.error.unwrap().code, DEADLINE_EXCEEDED_ERROR_CODE);
    assert_eq!(
        expired.audit_events.last().map(|e| (e.from, e.to)),
        Some((Some(Queued), Failed))
    );

    let queued2 = invocation_in(&store, &claims, Queued).await;
    let missing = store
        .transition_for_claims(
            &claims,
            &queued2.id,
            None,
            Failed,
            TransitionPatch {
                result: None,
                error: Some(InvocationErrorInfo::new(
                    crate::api::http::invocations_store::PROJECTION_CONTEXT_MISSING_ERROR_CODE,
                    "empty projection",
                )),
                task_iri: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(missing.state, Failed);
    assert_eq!(
        missing.error.unwrap().code,
        crate::api::http::invocations_store::PROJECTION_CONTEXT_MISSING_ERROR_CODE
    );
}

// --- Retention sweep, active limit and usage (#313 decisions) ---

fn open_store_with(dir: &tempfile::TempDir, config: InvocationStoreConfig) -> InvocationStore {
    InvocationStore::open_with_config(dir.path().join("invocations.json"), config)
        .unwrap()
        .0
}

#[tokio::test]
async fn invocations_lifecycle_sweep_removes_expired_terminal_keeps_non_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    let mut terminal = Vec::new();
    for state in [
        InvocationState::Succeeded,
        InvocationState::Failed,
        InvocationState::Cancelled,
    ] {
        terminal.push(invocation_in(&store, &claims, state).await);
    }
    let mut live = Vec::new();
    for state in [
        InvocationState::Queued,
        InvocationState::Running,
        InvocationState::CancelRequested,
    ] {
        live.push(invocation_in(&store, &claims, state).await);
    }

    // Inside the retention window nothing is swept and nothing is written.
    let before_disk = std::fs::read(store.path()).unwrap();
    assert_eq!(store.sweep_expired().await.unwrap(), 0);
    assert_eq!(std::fs::read(store.path()).unwrap(), before_disk);

    let later = chrono::Utc::now()
        + chrono::Duration::days(DEFAULT_RETENTION_DAYS as i64)
        + chrono::Duration::minutes(1);
    assert_eq!(store.sweep_expired_at(later).await.unwrap(), 3);
    for gone in &terminal {
        assert_eq!(
            store.get_for_claims(&claims, &gone.id).await.unwrap_err(),
            InvocationStoreError::NotFound
        );
    }
    let kept: BTreeSet<String> = live.iter().map(|r| r.id.clone()).collect();
    let listed: BTreeSet<String> = store
        .list_for_claims(&claims, None)
        .await
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(listed, kept, "non-terminal records are never swept");

    // Persisted atomically: disk matches memory and no temporary file is left.
    let on_disk: BTreeSet<String> = disk_records(&store).into_iter().map(|r| r.id).collect();
    assert_eq!(on_disk, kept);
    assert!(!store.path().with_extension("json.tmp").exists());

    // Re-sweeping is idempotent: nothing removed, file untouched.
    let swept_disk = std::fs::read(store.path()).unwrap();
    assert_eq!(store.sweep_expired_at(later).await.unwrap(), 0);
    assert_eq!(std::fs::read(store.path()).unwrap(), swept_disk);
}

#[tokio::test]
async fn invocations_lifecycle_sweep_runs_on_open_and_on_create() {
    let dir = tempfile::tempdir().unwrap();
    let claims = alice();
    let (done, queued) = {
        let store = open_store(&dir);
        (
            invocation_in(&store, &claims, InvocationState::Succeeded).await,
            invocation_in(&store, &claims, InvocationState::Queued).await,
        )
    };
    let zero = InvocationStoreConfig {
        retention: chrono::Duration::zero(),
        ..InvocationStoreConfig::default()
    };
    // Startup: recovery first (queued -> failed/interrupted, completed now),
    // then the sweep removes the already-expired terminal record.
    let (store, report) =
        InvocationStore::open_with_config(dir.path().join("invocations.json"), zero).unwrap();
    assert_eq!(report.loaded, 2);
    assert_eq!(report.interrupted, 1);
    assert!(report.swept >= 1);
    assert_eq!(
        store.get_for_claims(&claims, &done.id).await.unwrap_err(),
        InvocationStoreError::NotFound
    );
    assert!(!disk_records(&store).iter().any(|r| r.id == done.id));
    let _ = queued;

    // Create sweeps opportunistically and persists the sweep with the insert.
    let finished = invocation_in(&store, &claims, InvocationState::Cancelled).await;
    let fresh = store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap();
    let ids: Vec<String> = disk_records(&store).into_iter().map(|r| r.id).collect();
    assert!(ids.contains(&fresh.id));
    assert!(
        !ids.contains(&finished.id),
        "expired terminal swept on create"
    );
}

#[tokio::test]
async fn invocations_lifecycle_active_limit_is_429_and_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store_with(
        &dir,
        InvocationStoreConfig {
            max_active_per_scope: 2,
            ..InvocationStoreConfig::default()
        },
    );
    let claims = alice();
    let first = store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap();
    store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap();
    let before_disk = std::fs::read(store.path()).unwrap();
    let error = store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap_err();
    assert_eq!(error, InvocationStoreError::TooManyActive);
    assert_eq!(std::fs::read(store.path()).unwrap(), before_disk);
    assert_eq!(store.list_for_claims(&claims, None).await.len(), 2);

    let response = error.into_response();
    assert_eq!(
        response.headers().get(header::RETRY_AFTER).unwrap(),
        &TOO_MANY_ACTIVE_RETRY_AFTER_SECS.to_string()
    );
    let (status, body) = body_bytes(response).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "too_many_active");

    // The limit is per tenant/project scope.
    store
        .create_for_claims(
            &IsolationClaims::from_verified("tenant-a", "project-b", "alice").unwrap(),
            new_invocation(),
        )
        .await
        .unwrap();
    // Terminal records do not count.
    store
        .transition_for_claims(
            &claims,
            &first.id,
            None,
            InvocationState::Cancelled,
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap();
}

#[tokio::test]
async fn invocations_lifecycle_result_usage_round_trips_with_optional_fields() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    let running = invocation_in(&store, &claims, InvocationState::Running).await;
    let usage = InvocationUsage {
        provider: Some("provider-x".into()),
        model: Some("model-y".into()),
        input_tokens: Some(1200),
        output_tokens: Some(345),
        cost: Some(2_500_000),
        tool_calls: Some(vec![InvocationToolCallUsage {
            name: "search".into(),
            transport: Some("mcp".into()),
        }]),
    };
    let done = store
        .transition_for_claims(
            &claims,
            &running.id,
            None,
            InvocationState::Succeeded,
            TransitionPatch {
                result: Some(InvocationResult {
                    summary: "ok".into(),
                    artifacts: vec![],
                    usage: Some(usage.clone()),
                }),
                error: None,
                task_iri: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(done.result.as_ref().unwrap().usage.as_ref(), Some(&usage));
    let reloaded = disk_records(&store)
        .into_iter()
        .find(|r| r.id == done.id)
        .unwrap();
    assert_eq!(reloaded.result.unwrap().usage, Some(usage));

    // Absent usage members are omitted, not serialized as null.
    let sparse = serde_json::to_value(InvocationUsage {
        cost: Some(7),
        ..InvocationUsage::default()
    })
    .unwrap();
    assert_eq!(sparse, json!({"cost": 7}));
    let no_usage = serde_json::to_value(InvocationResult::default()).unwrap();
    assert!(no_usage.get("usage").is_none());
}

#[tokio::test]
async fn invocation_usage_rejects_unknown_members_and_invalid_transport_at_write_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    let running = invocation_in(&store, &claims, InvocationState::Running).await;
    let before = store.get_for_claims(&claims, &running.id).await.unwrap();
    let invalid = InvocationUsage {
        model: Some("model".into()),
        input_tokens: Some(1),
        output_tokens: Some(1),
        cost: Some(1),
        tool_calls: Some(vec![InvocationToolCallUsage {
            name: "tool".into(),
            transport: Some("smtp".into()),
        }]),
        ..InvocationUsage::default()
    };
    assert_eq!(
        store
            .transition_for_claims(
                &claims,
                &running.id,
                None,
                InvocationState::Succeeded,
                TransitionPatch {
                    result: Some(InvocationResult {
                        summary: "nope".into(),
                        artifacts: vec![],
                        usage: Some(invalid),
                    }),
                    ..TransitionPatch::default()
                },
            )
            .await,
        Err(InvocationStoreError::InvalidUsage(
            "tool_calls[].transport must be mcp, http, a2a, local, or unknown"
        ))
    );
    assert_eq!(
        store.get_for_claims(&claims, &running.id).await.unwrap(),
        before
    );

    let malformed = serde_json::json!({
        "model": "model",
        "input_tokens": 1,
        "output_tokens": 1,
        "cost": 1,
        "extra": true
    });
    assert!(serde_json::from_value::<InvocationUsage>(malformed).is_err());
}

/// Fills the store with terminal (`succeeded`) records, all inside the
/// retention window, until it holds exactly [`MAX_STORED_INVOCATIONS`].
/// Writes memory and disk directly: 10 000 single creates would rewrite the
/// whole file each time.
async fn fill_to_capacity(store: &InvocationStore, claims: &IsolationClaims) {
    let template = invocation_in(store, claims, InvocationState::Succeeded).await;
    let mut records = store.records.write().await;
    while records.len() < MAX_STORED_INVOCATIONS {
        let mut record = template.clone();
        record.id = format!("inv_{}", uuid::Uuid::new_v4().simple());
        records.push(record);
    }
    persist(store.path(), &records).unwrap();
}

#[tokio::test]
async fn invocations_lifecycle_store_full_nothing_expired_is_503_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    fill_to_capacity(&store, &claims).await;
    let before_disk = std::fs::read(store.path()).unwrap();

    let err = store
        .create_for_claims(&claims, new_invocation())
        .await
        .unwrap_err();
    assert_eq!(err, InvocationStoreError::StoreFull);
    let (status, body) = body_bytes(err.into_response()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "invocation_store_full");

    assert_eq!(store.records.read().await.len(), MAX_STORED_INVOCATIONS);
    assert_eq!(
        std::fs::read(store.path()).unwrap(),
        before_disk,
        "a rejected create writes nothing"
    );
}

#[tokio::test]
async fn invocations_lifecycle_store_full_sweep_frees_space_then_create_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir);
    let claims = alice();
    fill_to_capacity(&store, &claims).await;

    // Backdate one terminal record past the retention window.
    let expired_id = {
        let mut records = store.records.write().await;
        let expired = &mut records[0];
        let past = chrono::Utc::now()
            - chrono::Duration::days(DEFAULT_RETENTION_DAYS as i64)
            - chrono::Duration::minutes(1);
        expired.completed_at = Some(past.to_rfc3339());
        let id = expired.id.clone();
        persist(store.path(), &records).unwrap();
        id
    };

    let fresh = store
        .create_for_claims(&claims, new_invocation())
        .await
        .expect("the sweep inside create frees one slot");
    assert_eq!(fresh.state, InvocationState::Queued);
    assert_eq!(
        store
            .get_for_claims(&claims, &expired_id)
            .await
            .unwrap_err(),
        InvocationStoreError::NotFound
    );
    assert_eq!(store.records.read().await.len(), MAX_STORED_INVOCATIONS);

    // Sweep and insert were persisted together.
    let on_disk = disk_records(&store);
    assert_eq!(on_disk.len(), MAX_STORED_INVOCATIONS);
    assert!(on_disk.iter().any(|r| r.id == fresh.id));
    assert!(!on_disk.iter().any(|r| r.id == expired_id));

    // Full again with nothing expired: the next create is rejected.
    assert_eq!(
        store
            .create_for_claims(&claims, new_invocation())
            .await
            .unwrap_err(),
        InvocationStoreError::StoreFull
    );
}
