//! Regression tests for issue #356 that go through the production executor
//! path (`HttpTaskExecutor::execute`) instead of a mock executor.
//!
//! The shared startup L0 is read-only; a run's completion flush must land in
//! the run's own claims-verified tenant L0 and must not fail the run with
//! `PermissionDenied`.

use super::*;

// ── Production-path harness ────────────────────────────────────────────────
//
// Builds an `HttpTaskExecutor` wired the way `AgentOSService` wires it in
// production: the shared startup L0 is the legacy read-only store, and each run
// gets a claims-verified tenant L0 handle from the executor itself. The LLM is
// a local OpenAI-compatible stub, so no real model or network is involved.

use std::sync::atomic::AtomicUsize;
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
    pub(super) blackboard: Arc<Blackboard>,
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
        blackboard,
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

/// Concurrent runs of one tenant (same and different projects) share the
/// tenant's L0 handle: every run completes and every run's completion flush
/// lands in that tenant L0, with no redb lock failure on open or on flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_runs_of_one_tenant_all_complete_and_flush_into_the_shared_tenant_l0() {
    // Every LLM reply is delayed so all runs hold the tenant L0 at once.
    let harness = production_harness(Duration::from_millis(400)).await;
    let tenant_project_1 = claims("tenant-a", "project-1");
    let tenant_project_2 = claims("tenant-a", "project-2");
    let runs: Vec<(String, IsolationClaims)> =
        [&tenant_project_1, &tenant_project_1, &tenant_project_2]
            .into_iter()
            .map(|claims| {
                (
                    format!("iri://task/{}", uuid::Uuid::new_v4()),
                    claims.clone(),
                )
            })
            .collect();

    // A dirty node per run, as PDCA leaves it before completion.
    let config = CoreConfig::default();
    let seeds: Vec<String> = runs
        .iter()
        .map(|(task_iri, _)| format!("{task_iri}/seed"))
        .collect();
    for seed in &seeds {
        harness
            .blackboard
            .write_node(seed, r#"{"v":1}"#, &config)
            .unwrap();
        harness
            .blackboard
            .write_node(seed, r#"{"v":2}"#, &config)
            .unwrap();
    }

    let mut rx = harness.event_bus.subscribe();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let mut joins = Vec::new();
    for (task_iri, claims) in runs.clone() {
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

    assert!(
        max_in_flight.load(AtomicOrdering::SeqCst) > 1,
        "runs must overlap"
    );
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
    let lock_errors: Vec<String> = all
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
        lock_errors.is_empty(),
        "same-tenant concurrent runs must not fight over the L0 lock: {lock_errors:#?}"
    );
    for (task_iri, _) in &runs {
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
