//! Regression tests for issue #355 that go through the production executor
//! path (`HttpTaskExecutor::execute`) instead of a mock executor.
//!
//! redb locks `l0.redb` exclusively. Concurrent runs of the same tenant, in
//! the same or in different projects, must share one tenant L0 handle instead
//! of each opening the file (and failing on the lock).

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use crate::api::http::TaskExecutor;
use crate::core::event_bus::Event;
use crate::isolation::IsolationClaims;

use super::executor_l0_test_support::*;
use super::*;

fn lock_failures(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| {
            e.payload.contains("L0InitializationError")
                || e.payload.contains("already open")
                || e.payload.contains("Cannot acquire lock")
        })
        .map(|e| format!("{} {}: {}", e.task_iri, e.event_type, e.payload))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_runs_of_one_tenant_across_projects_share_the_tenant_l0() {
    // Every LLM reply is delayed so all runs hold their tenant L0 at once.
    let harness = production_harness(Duration::from_millis(400)).await;
    let runs = [
        ("tenant-a", "project-1"),
        ("tenant-a", "project-1"),
        ("tenant-a", "project-2"),
        ("tenant-b", "project-1"),
    ];
    let tasks: Vec<(String, IsolationClaims)> = runs
        .iter()
        .map(|(tenant, project)| {
            (
                format!("iri://task/{}", uuid::Uuid::new_v4()),
                claims(tenant, project),
            )
        })
        .collect();

    let mut rx = harness.event_bus.subscribe();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let mut joins = Vec::new();
    for (task_iri, claims) in tasks.clone() {
        let executor = harness.executor.clone();
        let in_flight = in_flight.clone();
        let max_in_flight = max_in_flight.clone();
        joins.push(tokio::spawn(async move {
            let now = in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            max_in_flight.fetch_max(now, AtomicOrdering::SeqCst);
            executor.execute(spec(&task_iri, &claims)).await;
            in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
        }));
    }
    for join in joins {
        join.await.unwrap();
    }

    let mut all = Vec::new();
    {
        use tokio::sync::broadcast::error::TryRecvError;
        loop {
            match rx.try_recv() {
                Ok(event) => all.push(event),
                Err(TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }

    assert!(
        max_in_flight.load(AtomicOrdering::SeqCst) > 1,
        "runs must overlap"
    );
    let failures = lock_failures(&all);
    assert!(
        failures.is_empty(),
        "same-tenant concurrent runs must not fight over the L0 lock: {failures:#?}"
    );
    for (task_iri, _) in &tasks {
        let events: Vec<Event> = all
            .iter()
            .filter(|e| &e.task_iri == task_iri)
            .cloned()
            .collect();
        let terminal = terminal_events(&events);
        assert!(
            terminal
                .iter()
                .any(|line| line.starts_with("TASK_COMPLETED"))
                && !terminal.iter().any(|line| line.starts_with("TASK_FAILED")),
            "every run must complete: {task_iri} {terminal:?}"
        );
    }
    // Each run reached the planner, i.e. got past L0 initialisation.
    assert!(harness.llm.requests.load(AtomicOrdering::SeqCst) >= tasks.len());
    // Once every run is over no tenant handle stays open.
    assert_eq!(harness.executor.tenant_l0.open_handles(), 0);
    assert_eq!(harness.legacy_l0.count().unwrap(), 0);
}

/// A tenant's L0 stays usable for later runs (and for a later process) after
/// its runs end: the registry closes the idle handle and the file lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tenant_l0_is_closed_after_the_last_run_and_can_be_reopened() {
    let harness = production_harness(Duration::from_millis(0)).await;
    let tenant = claims("tenant-a", "project-1");
    for _ in 0..2 {
        let task_iri = format!("iri://task/{}", uuid::Uuid::new_v4());
        let events = run_and_collect(&harness, &task_iri, &tenant).await;
        assert!(
            lock_failures(&events).is_empty(),
            "{:#?}",
            lock_failures(&events)
        );
        assert!(!terminal_events(&events).is_empty());
    }
    assert_eq!(harness.executor.tenant_l0.open_handles(), 0);
    drop(L0Store::open_for_claims(&harness.l0_root, &tenant).unwrap());
}
