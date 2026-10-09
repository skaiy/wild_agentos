//! Regression tests for issue #356 that go through the production executor
//! path (`HttpTaskExecutor::execute`) instead of a mock executor.
//!
//! The shared startup L0 is read-only; a run's completion flush must land in
//! the run's own claims-verified tenant L0 and must not fail the run with
//! `PermissionDenied`.

use std::time::Duration;

use crate::api::http::TaskExecutor;
use crate::core::event_bus::Event;
use crate::isolation::IsolationClaims;

use super::executor_l0_test_support::*;
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_run_flushes_into_its_tenant_l0_not_the_read_only_shared_l0() {
    let harness = production_harness(Duration::from_millis(0)).await;
    let tenant = claims("tenant-a", "project-1");
    let task_iri = format!("iri://task/{}", uuid::Uuid::new_v4());

    // A node of this task that PDCA updated (dirty) before completion, and a
    // dirty node of another tenant's concurrent run on the shared blackboard.
    let config = CoreConfig::default();
    let own = format!("{task_iri}/seed");
    let foreign = format!("iri://task/{}/result", uuid::Uuid::new_v4());
    for iri in [&own, &foreign] {
        harness
            .blackboard
            .write_node(iri, r#"{"v":1}"#, &config)
            .unwrap();
        harness
            .blackboard
            .write_node(iri, r#"{"v":2}"#, &config)
            .unwrap();
    }

    let events = run_and_collect(&harness, &task_iri, &tenant).await;
    let terminal = terminal_events(&events);

    assert!(
        harness.llm.requests.load(AtomicOrdering::SeqCst) > 0,
        "the run must reach the LLM stub: {terminal:?}"
    );
    assert!(!terminal.is_empty(), "the run must reach a terminal event");
    for line in &terminal {
        assert!(
            !line.contains("cannot write L0 data"),
            "completion flush hit the read-only shared L0: {line}"
        );
    }
    assert!(
        terminal.iter().any(|l| l.starts_with("TASK_COMPLETED")),
        "run must complete: {terminal:?}"
    );

    let tenant_l0 = L0Store::open_for_claims(&harness.l0_root, &tenant).unwrap();
    assert!(
        tenant_l0.retrieve(&own).unwrap().is_some(),
        "the task's dirty node must be flushed into the run's tenant L0"
    );
    assert!(
        tenant_l0.retrieve(&foreign).unwrap().is_none(),
        "another run's dirty node must not be flushed into this tenant's L0"
    );
    assert!(
        harness
            .blackboard
            .read_node(&foreign)
            .unwrap()
            .unwrap()
            .dirty
    );
    assert_eq!(harness.legacy_l0.count().unwrap(), 0);
}

/// One dirty L2 node `{task_iri}/seed` per run, as PDCA leaves it before
/// completion.
fn seed_dirty_nodes(harness: &Harness, runs: &[(String, IsolationClaims)]) -> Vec<String> {
    let config = CoreConfig::default();
    runs.iter()
        .map(|(task_iri, _)| {
            let seed = format!("{task_iri}/seed");
            for v in [r#"{"v":1}"#, r#"{"v":2}"#] {
                harness.blackboard.write_node(&seed, v, &config).unwrap();
            }
            seed
        })
        .collect()
}

fn new_runs(claims: &[&IsolationClaims]) -> Vec<(String, IsolationClaims)> {
    claims
        .iter()
        .map(|claims| {
            (
                format!("iri://task/{}", uuid::Uuid::new_v4()),
                (*claims).clone(),
            )
        })
        .collect()
}

/// Start every run at once through `HttpTaskExecutor::execute`.
fn spawn_runs(
    harness: &Harness,
    runs: &[(String, IsolationClaims)],
) -> Vec<tokio::task::JoinHandle<()>> {
    runs.iter()
        .cloned()
        .map(|(task_iri, claims)| {
            let executor = harness.executor.clone();
            tokio::spawn(async move {
                let _ = executor.execute(tagged_spec(&task_iri, &claims)).await;
            })
        })
        .collect()
}

fn drain_all(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<Event> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut all = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => all.push(event),
            Err(TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    all
}

/// Every run ended `TASK_COMPLETED` and no event reports an L0 lock failure or
/// a write into the read-only shared L0.
fn assert_all_completed_without_l0_errors(all: &[Event], runs: &[(String, IsolationClaims)]) {
    let l0_errors: Vec<String> = all
        .iter()
        .filter(|e| {
            e.payload.contains("L0InitializationError")
                || e.payload.contains("already open")
                || e.payload.contains("Cannot acquire lock")
                || e.payload.contains("cannot write L0 data")
        })
        .map(|e| format!("{} {}: {}", e.task_iri, e.event_type, e.payload))
        .collect();
    assert!(
        l0_errors.is_empty(),
        "concurrent runs must not fight over the L0 lock: {l0_errors:#?}"
    );
    for (task_iri, _) in runs {
        let events: Vec<Event> = all
            .iter()
            .filter(|e| &e.task_iri == task_iri)
            .cloned()
            .collect();
        let terminal = terminal_events(&events);
        assert!(
            terminal.iter().any(|l| l.starts_with("TASK_COMPLETED"))
                && !terminal.iter().any(|l| l.starts_with("TASK_FAILED")),
            "every run must complete: {task_iri} {terminal:?}"
        );
    }
}

/// Concurrent runs of one tenant (same and different projects) share the
/// tenant's L0 handle: every run completes and every run's completion flush
/// lands in that tenant L0, with no redb lock failure on open or on flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_runs_of_one_tenant_all_complete_and_flush_into_the_shared_tenant_l0() {
    // Every LLM reply is delayed so all runs hold the tenant L0 at once.
    let harness = production_harness(Duration::from_millis(400)).await;
    let tenant_project_1 = claims("tenant-a", "project-1");
    let tenant_project_2 = claims("tenant-a", "project-2");
    let runs = new_runs(&[&tenant_project_1, &tenant_project_1, &tenant_project_2]);
    let seeds = seed_dirty_nodes(&harness, &runs);

    let mut rx = harness.event_bus.subscribe();
    let joins = spawn_runs(&harness, &runs);

    // While at least two runs are inside a model call (so both are past L0
    // acquisition and before they drop their handle), sample how many clones
    // of the tenant handle the runs hold: registry + this probe + runs.
    let registry = harness.executor.tenant_l0.clone();
    let in_llm = harness.llm.in_llm.clone();
    let probe_claims = tenant_project_1.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut overlapping_samples = 0usize;
            let mut max_held_by_runs = 0usize;
            while !stop.load(AtomicOrdering::SeqCst) {
                if runs_in_llm(&in_llm) >= 2 {
                    if let Ok(handle) = registry.get_or_open(&probe_claims) {
                        let held_by_runs = Arc::strong_count(&handle).saturating_sub(2);
                        max_held_by_runs = max_held_by_runs.max(held_by_runs);
                        overlapping_samples += 1;
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            (overlapping_samples, max_held_by_runs)
        })
    };
    for join in joins {
        join.await.unwrap();
    }
    stop.store(true, AtomicOrdering::SeqCst);
    let (overlapping_samples, max_held_by_runs) = sampler.await.unwrap();
    // A late probe can keep the handle past the last run's release.
    harness.executor.tenant_l0.release_idle();

    assert!(
        overlapping_samples > 0,
        "at least two runs must be inside a model call at the same time"
    );
    assert!(
        max_held_by_runs >= 2,
        "overlapping runs must hold the one shared tenant handle at the same time \
         (held by runs: {max_held_by_runs})"
    );
    assert_eq!(harness.llm.runs_in_llm(), 0);

    let all = drain_all(&mut rx);
    assert_all_completed_without_l0_errors(&all, &runs);
    assert!(harness.llm.requests.load(AtomicOrdering::SeqCst) >= runs.len());

    // Every run's completion flush succeeded: no run left its node dirty in
    // L2 (completion may also release it), and the node is in the tenant L0
    // the runs shared.
    for seed in &seeds {
        assert!(
            harness
                .blackboard
                .read_node(seed)
                .unwrap()
                .is_none_or(|node| !node.dirty),
            "completion flush must not leave the node dirty: {seed}"
        );
    }
    assert_eq!(harness.executor.tenant_l0.open_handles(), 0);
    let tenant_l0 = L0Store::open_for_claims(&harness.l0_root, &tenant_project_1).unwrap();
    for seed in &seeds {
        assert!(
            tenant_l0.retrieve(seed).unwrap().is_some(),
            "each run's dirty node must be flushed into the shared tenant L0: {seed}"
        );
    }
    assert_eq!(harness.legacy_l0.count().unwrap(), 0);
}

/// Two tenants with two concurrent runs each: both tenant handles are open at
/// the same time, every run completes, and each run's dirty node is flushed
/// into its own tenant's L0 only, never into the other tenant's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_runs_of_two_tenants_flush_only_into_their_own_tenant_l0() {
    let harness = production_harness(Duration::from_millis(400)).await;
    let tenant_a = claims("tenant-a", "project-1");
    let tenant_b = claims("tenant-b", "project-1");
    let runs = new_runs(&[&tenant_a, &tenant_a, &tenant_b, &tenant_b]);
    let seeds = seed_dirty_nodes(&harness, &runs);

    let mut rx = harness.event_bus.subscribe();
    let joins = spawn_runs(&harness, &runs);

    // `open_handles` only reads the registry, so sampling it does not keep a
    // handle open.
    let registry = harness.executor.tenant_l0.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut max_open = 0usize;
            while !stop.load(AtomicOrdering::SeqCst) {
                max_open = max_open.max(registry.open_handles());
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            max_open
        })
    };
    for join in joins {
        join.await.unwrap();
    }
    stop.store(true, AtomicOrdering::SeqCst);
    let max_open = sampler.await.unwrap();

    assert_eq!(
        max_open, 2,
        "both tenant handles must be open at the same time, and only those two"
    );
    let all = drain_all(&mut rx);
    assert_all_completed_without_l0_errors(&all, &runs);
    assert_eq!(harness.executor.tenant_l0.open_handles(), 0);

    let l0_a = L0Store::open_for_claims(&harness.l0_root, &tenant_a).unwrap();
    let l0_b = L0Store::open_for_claims(&harness.l0_root, &tenant_b).unwrap();
    for ((task_iri, claims), seed) in runs.iter().zip(&seeds) {
        let (own, other) = if claims.tenant_id() == tenant_a.tenant_id() {
            (&l0_a, &l0_b)
        } else {
            (&l0_b, &l0_a)
        };
        assert!(
            own.retrieve(seed).unwrap().is_some(),
            "run {task_iri} must flush its node into its own tenant L0"
        );
        assert!(
            other.retrieve(seed).unwrap().is_none(),
            "run {task_iri} must not flush its node into the other tenant's L0"
        );
    }
    assert_eq!(harness.legacy_l0.count().unwrap(), 0);
}

/// Archive (session summary) and the consistency flush of a dirty task node
/// both land in this run's tenant L0. Another tenant cannot read them, and the
/// shared startup L0 stays empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_and_consistency_land_in_the_tenant_l0_only() {
    let harness = production_harness(Duration::from_millis(0)).await;
    let tenant = claims("tenant-a", "project-1");
    let other = claims("tenant-b", "project-1");
    let task_iri = format!("iri://task/{}", uuid::Uuid::new_v4());
    let seed = format!("{task_iri}/seed");
    let config = CoreConfig::default();
    harness
        .blackboard
        .write_node(&seed, r#"{"v":1}"#, &config)
        .unwrap();
    harness
        .blackboard
        .write_node(&seed, r#"{"v":2}"#, &config)
        .unwrap();

    let events = run_and_collect(&harness, &task_iri, &tenant).await;
    let terminal = terminal_events(&events);
    assert!(
        terminal
            .iter()
            .any(|line| line.starts_with("TASK_COMPLETED")),
        "run must complete: {terminal:?}"
    );

    let tenant_l0 = L0Store::open_for_claims(&harness.l0_root, &tenant).unwrap();
    assert!(
        tenant_l0.retrieve(&seed).unwrap().is_some(),
        "the dirty task node must be flushed into this tenant's L0"
    );
    let archives = tenant_l0.scan_iri_prefix("iri://archive/", 50).unwrap();
    assert!(
        !archives.is_empty(),
        "session or turn archive must land in this tenant's L0"
    );

    let other_l0 = L0Store::open_for_claims(&harness.l0_root, &other).unwrap();
    assert!(other_l0.retrieve(&seed).unwrap().is_none());
    assert!(other_l0
        .scan_iri_prefix("iri://archive/", 50)
        .unwrap()
        .is_empty());
    assert_eq!(harness.legacy_l0.count().unwrap(), 0);
}

/// A cancellation drops `process_task` before its completion hook. The dirty
/// subtree is still flushed into the tenant L0, and the shared store stays empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_run_still_flushes_its_dirty_subtree_into_the_tenant_l0() {
    let harness = production_harness(Duration::from_millis(0)).await;
    let tenant = claims("tenant-a", "project-1");
    let task_iri = format!("iri://task/{}", uuid::Uuid::new_v4());
    let seed = format!("{task_iri}/seed");
    let config = CoreConfig::default();
    harness
        .blackboard
        .write_node(&seed, r#"{"v":1}"#, &config)
        .unwrap();
    harness
        .blackboard
        .write_node(&seed, r#"{"v":2}"#, &config)
        .unwrap();

    let spec = spec(&task_iri, &tenant);
    spec.cancellation.cancel();
    let outcome = harness.executor.execute(spec).await;
    assert_eq!(outcome.status, "cancelled");

    let tenant_l0 = L0Store::open_for_claims(&harness.l0_root, &tenant).unwrap();
    assert!(
        tenant_l0.retrieve(&seed).unwrap().is_some(),
        "a cancelled run must still flush its dirty subtree"
    );
    assert_eq!(harness.legacy_l0.count().unwrap(), 0);
}
