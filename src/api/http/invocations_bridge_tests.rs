//! Execution bridge tests for #317 (mock TaskExecutor + VAL-PROJ-CTX / usage).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;
use crate::api::http::control_plane_route_auth_tests::{test_state_with_invocations, EnvGuard};
use crate::api::http::invocations_enforcement::{
    FifoScheduler, InputRefRegistry, BUDGET_EXCEEDED_ERROR_CODE,
};
use crate::api::http::invocations_execution::{
    claims_from_invocation, projection_context_is_nonempty, InvocationExecutionBridge,
    ProjectionContextGate, ScopedProjectionGate, PROJECTION_GATE_FRAME,
};
use crate::api::http::invocations_store::{
    InvocationStoreConfig, PROJECTION_CONTEXT_MISSING_ERROR_CODE,
};
use crate::api::http::{TaskExecSpec, TaskExecutor, TEST_ENV_LOCK};
use crate::core::core_types::SemanticCore;
use crate::isolation::IsolationClaims;

use super::tests::{alice, call, env, router};

struct MockExecutor {
    mode: MockMode,
    calls: Arc<AtomicUsize>,
    events: Arc<crate::core::event_bus::EventBus>,
    seen_claims: Arc<std::sync::Mutex<Option<IsolationClaims>>>,
}

#[derive(Clone, Copy)]
enum MockMode {
    SucceedWithUsage,
    SucceedWithWebSearch,
    SucceedWithTwoWebSearches,
    SucceedWithoutUsage,
    Fail,
    HangUntilCancel,
}

#[async_trait]
impl TaskExecutor for MockExecutor {
    async fn execute(&self, spec: TaskExecSpec) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.seen_claims.lock().unwrap() = Some(spec.isolation_claims.clone());
        match self.mode {
            MockMode::SucceedWithUsage => {
                self.events
                    .emit(
                        &spec.task_iri,
                        "TASK_COMPLETED",
                        "mock",
                        &json!({
                            "status": "succeeded",
                            "summary": "mock-ok",
                            "usage": {
                                "model": "mock-model",
                                "input_tokens": 11,
                                "output_tokens": 7,
                                "cost": 42
                            }
                        })
                        .to_string(),
                    )
                    .await;
            }
            MockMode::SucceedWithWebSearch => {
                self.events
                    .emit(
                        &spec.task_iri,
                        "TOOL_CALL",
                        "mock",
                        &json!({
                            "event": {
                                "ToolCall": {
                                    "tool_name": "web_search",
                                    "arguments_json": {"query": "never persist this"}
                                }
                            }
                        })
                        .to_string(),
                    )
                    .await;
                self.events
                    .emit(
                        &spec.task_iri,
                        "TASK_COMPLETED",
                        "mock",
                        &json!({
                            "status": "succeeded",
                            "summary": "mock-ok",
                            "usage": {
                                "model": "mock-model",
                                "input_tokens": 11,
                                "output_tokens": 7,
                                "cost": 42
                            }
                        })
                        .to_string(),
                    )
                    .await;
            }
            MockMode::SucceedWithTwoWebSearches => {
                for query in ["first", "second"] {
                    self.events
                        .emit(
                            &spec.task_iri,
                            "TOOL_CALL",
                            "mock",
                            &json!({
                                "event": {
                                    "ToolCall": {
                                        "tool_name": "web_search",
                                        "arguments_json": {"query": query}
                                    }
                                }
                            })
                            .to_string(),
                        )
                        .await;
                }
                self.events
                    .emit(
                        &spec.task_iri,
                        "TASK_COMPLETED",
                        "mock",
                        &json!({
                            "status": "succeeded",
                            "summary": "mock-ok",
                            "usage": {
                                "model": "mock-model",
                                "input_tokens": 11,
                                "output_tokens": 7,
                                "cost": 42
                            }
                        })
                        .to_string(),
                    )
                    .await;
            }
            MockMode::SucceedWithoutUsage => {
                self.events
                    .emit(
                        &spec.task_iri,
                        "TASK_COMPLETED",
                        "mock",
                        &json!({"status": "succeeded", "summary": "missing-usage"}).to_string(),
                    )
                    .await;
            }
            MockMode::Fail => {
                self.events
                    .emit(
                        &spec.task_iri,
                        "TASK_FAILED",
                        "mock",
                        &json!({"status": "failed", "summary": "mock-boom"}).to_string(),
                    )
                    .await;
            }
            MockMode::HangUntilCancel => {
                spec.cancellation.cancelled().await;
                self.events
                    .emit(
                        &spec.task_iri,
                        "TASK_FAILED",
                        "mock",
                        &json!({"status": "failed", "summary": "cancelled-by-token"}).to_string(),
                    )
                    .await;
            }
        }
    }
}

struct EmptyProjectionGate;

#[async_trait]
impl ProjectionContextGate for EmptyProjectionGate {
    async fn ensure_nonempty(
        &self,
        _core: &SemanticCore,
        _task_iri: &str,
        _claims: &IsolationClaims,
    ) -> Result<(), String> {
        Err("forced empty projection".into())
    }
}

struct BridgeHarness {
    router: Router,
    store: Arc<InvocationStore>,
    state: Arc<AppState>,
    calls: Arc<AtomicUsize>,
    seen_claims: Arc<std::sync::Mutex<Option<IsolationClaims>>>,
    _dir: tempfile::TempDir,
    _env: EnvGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn make_bridge_harness(mode: MockMode, gate: Arc<dyn ProjectionContextGate>) -> BridgeHarness {
    let lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env_guard = env(true);
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = InvocationStore::open_with_config(
        dir.path().join("invocations.json"),
        InvocationStoreConfig::default(),
    )
    .unwrap();
    let store = Arc::new(store);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_claims = Arc::new(std::sync::Mutex::new(None));

    let bootstrap = test_state_with_invocations(
        dir.path(),
        InvocationsRuntime::new(Some(store.clone()), true),
    );
    let input_refs = InputRefRegistry::new();
    let runtime =
        InvocationsRuntime::new(Some(store.clone()), true).with_input_refs(input_refs.clone());
    let bridge = Arc::new(InvocationExecutionBridge::new_with_enforcement(
        store.clone(),
        runtime.cancellations().clone(),
        bootstrap.core.clone(),
        Arc::new(MockExecutor {
            mode,
            calls: calls.clone(),
            events: bootstrap.core.events.clone(),
            seen_claims: seen_claims.clone(),
        }),
        bootstrap.shutdown.clone(),
        gate,
        Arc::new(FifoScheduler::with_defaults()),
        input_refs,
    ));
    let runtime = runtime.with_dispatcher(bridge);

    let state = Arc::new(AppState {
        core: bootstrap.core.clone(),
        gateway: bootstrap.gateway.clone(),
        kg_store: bootstrap.kg_store.clone(),
        config_info: bootstrap.config_info.clone(),
        agents_info: bootstrap.agents_info.clone(),
        mcp_servers: bootstrap.mcp_servers.clone(),
        user_agents: bootstrap.user_agents.clone(),
        prompts: bootstrap.prompts.clone(),
        kb_categories: bootstrap.kb_categories.clone(),
        knowledge_bases: bootstrap.knowledge_bases.clone(),
        knowledge_packs: bootstrap.knowledge_packs.clone(),
        vector_store: bootstrap.vector_store.clone(),
        blob_store: None,
        task_executor: None,
        batch_manager: None,
        api_clients: bootstrap.api_clients.clone(),
        api_keys: bootstrap.api_keys.clone(),
        api_usage: bootstrap.api_usage.clone(),
        online_corpus_jobs: bootstrap.online_corpus_jobs.clone(),
        online_corpus_queue_capacity: bootstrap.online_corpus_queue_capacity,
        invocations: runtime,
        shutdown: bootstrap.shutdown.clone(),
    });

    BridgeHarness {
        router: router(state.clone()),
        store,
        state,
        calls,
        seen_claims,
        _dir: dir,
        _env: env_guard,
        _lock: lock,
    }
}

async fn create_inv(router: &Router, auth: &str, body: Value) -> super::tests::Reply {
    call(
        router,
        "POST",
        "/v1/invocations",
        Some(auth),
        &[],
        Some(body.to_string()),
    )
    .await
}

async fn wait_terminal(store: &InvocationStore, id: &str, claims: &IsolationClaims) -> Invocation {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let inv = store.get_for_claims(claims, id).await.unwrap();
        if inv.state.is_terminal() {
            return inv;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for terminal state; last={:?}", inv.state);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn body() -> Value {
    json!({"prompt": "bridge-test-prompt"})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_succeeds_with_usage_and_task_iri_and_claims() {
    let h = make_bridge_harness(MockMode::SucceedWithUsage, Arc::new(ScopedProjectionGate));
    let created = create_inv(&h.router, &alice(), body()).await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.json());
    let id = created.json()["id"].as_str().unwrap().to_string();
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert_eq!(inv.state, InvocationState::Succeeded);
    assert!(inv
        .task_iri
        .as_ref()
        .is_some_and(|t| t.starts_with("iri://task_")));
    let usage = inv.result.as_ref().unwrap().usage.as_ref().unwrap();
    assert_eq!(usage.model.as_deref(), Some("mock-model"));
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(usage.cost, Some(42));
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);
    let seen = h.seen_claims.lock().unwrap().clone().unwrap();
    assert_eq!(seen.tenant_id(), "tenant-a");
    assert_eq!(seen.project_id(), "project-a");
    assert_eq!(seen.actor_id(), "alice");
    let projected = h
        .state
        .core
        .projection
        .project(
            inv.task_iri.as_ref().unwrap(),
            PROJECTION_GATE_FRAME,
            Default::default(),
            &claims,
        )
        .await
        .unwrap();
    assert!(
        projection_context_is_nonempty(&projected),
        "expected non-empty projection, got {projected}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_incomplete_usage_fails_closed() {
    let h = make_bridge_harness(
        MockMode::SucceedWithoutUsage,
        Arc::new(ScopedProjectionGate),
    );
    let created = create_inv(&h.router, &alice(), body()).await;
    let id = created.json()["id"].as_str().unwrap().to_string();
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert_eq!(inv.state, InvocationState::Failed);
    assert_eq!(
        inv.error.as_ref().map(|e| e.code.as_str()),
        Some("incomplete_usage")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_records_builtin_tool_calls_without_arguments_or_results() {
    let h = make_bridge_harness(
        MockMode::SucceedWithWebSearch,
        Arc::new(ScopedProjectionGate),
    );
    let created = create_inv(&h.router, &alice(), body()).await;
    let id = created.json()["id"].as_str().unwrap().to_string();
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert_eq!(inv.state, InvocationState::Succeeded);
    let tool_calls = inv
        .result
        .as_ref()
        .and_then(|result| result.usage.as_ref())
        .and_then(|usage| usage.tool_calls.as_ref())
        .expect("built-in tool call usage");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].name, "web_search");
    assert_eq!(tool_calls[0].transport.as_deref(), Some("http"));
    let persisted = serde_json::to_string(&tool_calls).unwrap();
    assert!(!persisted.contains("arguments_json"));
    assert!(!persisted.contains("never persist this"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_max_tool_calls_fails_after_recorded_builtin_calls() {
    let h = make_bridge_harness(
        MockMode::SucceedWithTwoWebSearches,
        Arc::new(ScopedProjectionGate),
    );
    let created = create_inv(
        &h.router,
        &alice(),
        json!({"prompt": "search", "budget": {"max_tool_calls": 1}}),
    )
    .await;
    let id = created.json()["id"].as_str().unwrap().to_string();
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert_eq!(inv.state, InvocationState::Failed);
    assert_eq!(
        inv.error.as_ref().map(|error| error.code.as_str()),
        Some(BUDGET_EXCEEDED_ERROR_CODE)
    );
    assert_eq!(
        inv.result
            .as_ref()
            .and_then(|result| result.usage.as_ref())
            .and_then(|usage| usage.tool_calls.as_ref())
            .map(Vec::len),
        Some(2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_task_failed_path() {
    let h = make_bridge_harness(MockMode::Fail, Arc::new(ScopedProjectionGate));
    let created = create_inv(&h.router, &alice(), body()).await;
    let id = created.json()["id"].as_str().unwrap().to_string();
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert_eq!(inv.state, InvocationState::Failed);
    assert_eq!(
        inv.error.as_ref().map(|e| e.code.as_str()),
        Some("task_failed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_projection_missing_fail_closed_without_executor() {
    let h = make_bridge_harness(MockMode::SucceedWithUsage, Arc::new(EmptyProjectionGate));
    let created = create_inv(&h.router, &alice(), body()).await;
    let id = created.json()["id"].as_str().unwrap().to_string();
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert_eq!(inv.state, InvocationState::Failed);
    assert_eq!(
        inv.error.as_ref().map(|e| e.code.as_str()),
        Some(PROJECTION_CONTEXT_MISSING_ERROR_CODE)
    );
    assert_eq!(h.calls.load(Ordering::SeqCst), 0, "executor must not run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_cancel_running_signals_token() {
    let h = make_bridge_harness(MockMode::HangUntilCancel, Arc::new(ScopedProjectionGate));
    let created = create_inv(&h.router, &alice(), body()).await;
    let id = created.json()["id"].as_str().unwrap().to_string();
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let inv = h.store.get_for_claims(&claims, &id).await.unwrap();
        if inv.state == InvocationState::Running || inv.state.is_terminal() {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("never reached running");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let cancel = call(
        &h.router,
        "POST",
        &format!("/v1/invocations/{id}/cancel"),
        Some(&alice()),
        &[],
        None,
    )
    .await;
    assert!(
        cancel.status == StatusCode::ACCEPTED || cancel.status == StatusCode::OK,
        "cancel status {} body {}",
        cancel.status,
        cancel.json()
    );
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert!(
        matches!(
            inv.state,
            InvocationState::Cancelled | InvocationState::Failed
        ),
        "got {:?}",
        inv.state
    );
    assert!(h.calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_sse_disconnect_does_not_stop_execution() {
    let h = make_bridge_harness(MockMode::SucceedWithUsage, Arc::new(ScopedProjectionGate));
    let created = create_inv(&h.router, &alice(), body()).await;
    let id = created.json()["id"].as_str().unwrap().to_string();
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/invocations/{id}/events"))
        .header("Authorization", format!("Bearer {}", alice()))
        .body(Body::empty())
        .unwrap();
    let response = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    let claims = IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap();
    let inv = wait_terminal(&h.store, &id, &claims).await;
    assert_eq!(inv.state, InvocationState::Succeeded);
}

#[test]
fn claims_from_invocation_round_trips_scope() {
    let inv = Invocation {
        id: "inv_x".into(),
        object: "invocation".into(),
        tenant_id: "tenant-a".into(),
        project_id: "project-a".into(),
        actor_id: "alice".into(),
        state: InvocationState::Queued,
        revision: 1,
        request: Default::default(),
        task_iri: None,
        result: None,
        error: None,
        idempotency_key: None,
        idempotency: None,
        created_at: "t".into(),
        updated_at: "t".into(),
        started_at: None,
        completed_at: None,
        audit_events: vec![],
    };
    let claims = claims_from_invocation(&inv).unwrap();
    assert_eq!(claims.tenant_id(), "tenant-a");
    assert_eq!(claims.actor_id(), "alice");
}
