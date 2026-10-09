//! Regression tests for issue #355 that go through the production executor
//! path (`HttpTaskExecutor::execute`) instead of a mock executor.
//!
//! redb locks `l0.redb` exclusively. Concurrent runs of the same tenant, in
//! the same or in different projects, must share one tenant L0 handle instead
//! of each opening the file (and failing on the lock).

use super::*;

// ── Production-path harness ────────────────────────────────────────────────
//
// Builds an `HttpTaskExecutor` wired the way `AgentOSService` wires it in
// production: the shared startup L0 is the legacy read-only store, and each run
// gets a claims-verified tenant L0 handle from the executor itself. The LLM is
// a local OpenAI-compatible stub, so no real model or network is involved.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::Duration;

use crate::api::http::{TaskExecSpec, TaskExecutor};
use crate::core::event_bus::Event;
use crate::isolation::IsolationClaims;

pub(super) struct StubLlm {
    pub(super) base_url: String,
    pub(super) requests: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for StubLlm {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn stub_reply(body: &serde_json::Value) -> String {
    let text = body["messages"].to_string();
    if text.contains("task planning expert") {
        serde_json::json!({
            "complexity": "simple",
            "description": "stub plan",
            "steps": [{
                "step_id": "step_1",
                "role": "Do",
                "objective": "Answer the request",
                "expected_output": "A short answer",
                "dependencies": [],
                "tools_allowed": [],
                "success_criteria": "An answer is given"
            }],
            "success_metrics": ["answered"]
        })
        .to_string()
    } else {
        "Done.".to_string()
    }
}

/// Start an OpenAI-compatible chat stub that waits `delay` before every reply,
/// so concurrent runs overlap for real.
pub(super) async fn spawn_stub_llm(delay: Duration) -> StubLlm {
    use axum::response::IntoResponse;

    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let handler = move |axum::Json(body): axum::Json<serde_json::Value>| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, AtomicOrdering::SeqCst);
            tokio::time::sleep(delay).await;
            let content = stub_reply(&body);
            if body["stream"].as_bool() == Some(true) {
                let chunk = serde_json::json!({
                    "id": "stub",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": null}]
                });
                let done = serde_json::json!({
                    "id": "stub",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                });
                let sse = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    sse,
                )
                    .into_response()
            } else {
                axum::Json(serde_json::json!({
                    "id": "stub",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": content},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
                .into_response()
            }
        }
    };
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(handler.clone()))
        .route("/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    StubLlm {
        base_url: format!("http://{addr}"),
        requests,
        shutdown: Some(tx),
    }
}

pub(super) struct Harness {
    pub(super) executor: Arc<HttpTaskExecutor>,
    pub(super) event_bus: Arc<EventBus>,
    pub(super) legacy_l0: Arc<L0Store>,
    pub(super) l0_root: std::path::PathBuf,
    pub(super) llm: StubLlm,
    _dir: tempfile::TempDir,
}

pub(super) async fn production_harness(llm_delay: Duration) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let llm = spawn_stub_llm(llm_delay).await;
    let l0_root = dir.path().join("l0");

    let mut settings = Settings::default();
    settings.gateway.base_url = llm.base_url.clone();
    settings.gateway.api_key = "stub-key-for-tests".to_string();
    settings.gateway.default_model = "stub-model".to_string();
    settings.gateway.max_retries = 0;
    settings.gateway.timeout_seconds = 20;
    for model in settings.gateway.model_mapping.values_mut() {
        *model = "stub-model".to_string();
    }
    settings.memory.l0.path = l0_root.to_string_lossy().into_owned();
    settings.output.directory = dir.path().join("output").to_string_lossy().into_owned();
    settings.workspace.root = None;
    settings.agents.timeout_seconds = 120;

    let gateway = Arc::new(UnifiedGateway::new(&settings.gateway).unwrap());
    // Same as production startup (`L0Store::new` outside tests): the shared
    // legacy store is read-only; a fresh install has no l0.redb at all.
    let legacy_l0 = Arc::new(L0Store::open_legacy_readonly(&settings.memory.l0.path).unwrap());
    let unified_graph = Arc::new(UnifiedGraphStore::new().unwrap());
    let blackboard = Arc::new(Blackboard::with_store(unified_graph.store()).unwrap());
    let projection = Arc::new(ProjectionEngine::new(blackboard.clone(), 500));
    let skills = Arc::new(SkillRegistry::new());
    let templates =
        Arc::new(TemplateEngine::new(std::path::Path::new("src/templates/templates")).unwrap());
    let event_bus = Arc::new(EventBus::new(16_384));
    let memory_bus = Arc::new(MemoryBus::new(event_bus.clone()));
    let consistency = Arc::new(ConsistencyEngine::new(
        memory_bus.clone(),
        legacy_l0.clone(),
        blackboard.clone(),
        projection.clone(),
    ));
    let scheduler = Arc::new(MemoryScheduler::new(
        legacy_l0.clone(),
        blackboard.clone(),
        projection.clone(),
        consistency,
        memory_bus.clone(),
    ));
    let prefetch = Arc::new(PrefetchEngine::new(
        memory_bus,
        blackboard.clone(),
        projection.clone(),
    ));
    let memory_manager = Arc::new(tokio::sync::Mutex::new(MemoryManager::with_scheduler(
        legacy_l0.clone(),
        blackboard.clone(),
        projection,
        CoreConfig::default(),
        scheduler.clone(),
    )));
    let vector_store: SharedVectorStore = Arc::new(arc_swap::ArcSwapOption::empty());

    let executor = Arc::new(HttpTaskExecutor {
        gateway,
        skills,
        blackboard: blackboard.clone(),
        tenant_l0: Arc::new(TenantL0Registry::new(&l0_root)),
        memory_manager,
        templates,
        scheduler,
        prefetch,
        unified_graph,
        event_bus: event_bus.clone(),
        vector_store,
        settings,
    });

    Harness {
        executor,
        event_bus,
        legacy_l0,
        l0_root,
        llm,
        _dir: dir,
    }
}

pub(super) fn spec(task_iri: &str, claims: &IsolationClaims) -> TaskExecSpec {
    TaskExecSpec {
        prompt: "Say hello.".to_string(),
        task_iri: task_iri.to_string(),
        include_thought: false,
        include_tool_calls: false,
        cancellation: CancellationToken::new(),
        isolation_claims: claims.clone(),
    }
}

pub(super) fn claims(tenant: &str, project: &str) -> IsolationClaims {
    IsolationClaims::from_verified(tenant, project, "actor").unwrap()
}

/// Run `execute` while recording every event published for `task_iri`.
pub(super) async fn run_and_collect(
    harness: &Harness,
    task_iri: &str,
    claims: &IsolationClaims,
) -> Vec<Event> {
    let mut rx = harness.event_bus.subscribe();
    harness.executor.execute(spec(task_iri, claims)).await;
    drain(&mut rx, task_iri)
}

pub(super) fn drain(
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
    task_iri: &str,
) -> Vec<Event> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) if event.task_iri == task_iri => events.push(event),
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    events
}

/// The terminal `TASK_COMPLETED` / `TASK_FAILED` payloads of a run.
pub(super) fn terminal_events(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event_type == "TASK_COMPLETED" || e.event_type == "TASK_FAILED")
        .map(|e| format!("{}: {}", e.event_type, e.payload))
        .collect()
}

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
        assert!(
            !terminal_events(&events).is_empty(),
            "every run must reach a terminal event: {task_iri}"
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
