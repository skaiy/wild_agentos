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
        l0_root: l0_root.clone(),
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
