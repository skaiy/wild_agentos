//! FIFO / deadline / budget / input_ref enforcement tests (#331).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::Router;
use serde_json::json;

use super::*;
use crate::api::http::control_plane_route_auth_tests::{test_state_with_invocations, EnvGuard};
use crate::api::http::invocations_enforcement::{
    sha256_hex, FifoScheduler, InputRefRegistry, InputRefResolver, InvocationRunningLimits,
    RunningScope, BUDGET_EXCEEDED_ERROR_CODE, INPUT_DIGEST_MISMATCH_ERROR_CODE,
};
use crate::api::http::invocations_execution::{InvocationExecutionBridge, ProjectionContextGate};
use crate::api::http::invocations_store::{
    InvocationState, InvocationStore, InvocationStoreConfig, DEADLINE_EXCEEDED_ERROR_CODE,
};
use crate::api::http::{TaskExecSpec, TaskExecutor, TEST_ENV_LOCK};
use crate::core::core_types::SemanticCore;
use crate::isolation::IsolationClaims;

use super::tests::{alice, call, env, router, token};

struct HoldExecutor {
    calls: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
    events: Arc<crate::core::event_bus::EventBus>,
    over_budget: bool,
}

#[async_trait]
impl TaskExecutor for HoldExecutor {
    async fn execute(&self, spec: TaskExecSpec) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
        let usage = if self.over_budget {
            json!({
                "model": "m",
                "input_tokens": 100,
                "output_tokens": 100,
                "cost": 9_999,
                "cost_source": "gateway"
            })
        } else {
            json!({
                "model": "m",
                "input_tokens": 1,
                "output_tokens": 1,
                "cost": 1,
                "cost_source": "gateway"
            })
        };
        self.events
            .emit(
                &spec.task_iri,
                "TASK_COMPLETED",
                crate::api::http::TASK_TERMINAL_SOURCE,
                &json!({"status": "succeeded", "summary": "held-ok", "usage": usage}).to_string(),
            )
            .await;
    }
}

struct MemResolver {
    body: Vec<u8>,
}

#[async_trait]
impl InputRefResolver for MemResolver {
    async fn resolve(&self, _uri: &str) -> Result<Vec<u8>, String> {
        Ok(self.body.clone())
    }
}

struct AlwaysOkGate;
#[async_trait]
impl ProjectionContextGate for AlwaysOkGate {
    async fn ensure_nonempty(
        &self,
        _core: &SemanticCore,
        _task_iri: &str,
        _claims: &IsolationClaims,
    ) -> Result<(), String> {
        Ok(())
    }
}

struct EnforcementHarness {
    router: Router,
    store: Arc<InvocationStore>,
    calls: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
    scheduler: Arc<FifoScheduler>,
    _dir: tempfile::TempDir,
    _env: EnvGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn make_harness(
    limits: InvocationRunningLimits,
    input_refs: InputRefRegistry,
    over_budget: bool,
) -> EnforcementHarness {
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
    let release = Arc::new(tokio::sync::Notify::new());
    let scheduler = Arc::new(FifoScheduler::new(limits));

    let bootstrap = test_state_with_invocations(
        dir.path(),
        InvocationsRuntime::new(Some(store.clone()), true),
    );
    let runtime =
        InvocationsRuntime::new(Some(store.clone()), true).with_input_refs(input_refs.clone());
    let bridge = Arc::new(InvocationExecutionBridge::new_with_enforcement(
        store.clone(),
        runtime.cancellations().clone(),
        bootstrap.core.clone(),
        Arc::new(HoldExecutor {
            calls: calls.clone(),
            release: release.clone(),
            events: bootstrap.core.events.clone(),
            over_budget,
        }),
        bootstrap.shutdown.clone(),
        Arc::new(AlwaysOkGate),
        scheduler.clone(),
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

    EnforcementHarness {
        router: router(state),
        store,
        calls,
        release,
        scheduler,
        _dir: dir,
        _env: env_guard,
        _lock: lock,
    }
}

fn alice_claims() -> IsolationClaims {
    IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap()
}

async fn create_prompt(
    h: &EnforcementHarness,
    auth: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let reply = call(
        &h.router,
        "POST",
        "/v1/invocations",
        Some(auth),
        &[],
        Some(body.to_string()),
    )
    .await;
    (reply.status, reply.json())
}

async fn wait_calls(calls: &Arc<AtomicUsize>, n: usize) {
    let start = std::time::Instant::now();
    while calls.load(Ordering::SeqCst) < n {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "executor calls stayed at {} (want {n})",
            calls.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_state(
    store: &InvocationStore,
    claims: &IsolationClaims,
    id: &str,
    want: InvocationState,
) {
    let start = std::time::Instant::now();
    loop {
        if let Ok(inv) = store.get_for_claims(claims, id).await {
            if inv.state == want {
                return;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "state for {id} did not become {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifo_over_cap_stays_queued_then_starts_in_order() {
    let h = make_harness(
        InvocationRunningLimits {
            global: 8,
            per_tenant: 8,
            per_scope: 1,
        },
        InputRefRegistry::new(),
        false,
    );
    let auth = alice();

    let (s1, b1) = create_prompt(&h, &auth, json!({"prompt": "first"})).await;
    assert_eq!(s1, StatusCode::ACCEPTED, "{b1}");
    let id1 = b1["id"].as_str().unwrap().to_string();
    wait_calls(&h.calls, 1).await;

    let (s2, b2) = create_prompt(&h, &auth, json!({"prompt": "second"})).await;
    assert_eq!(s2, StatusCode::ACCEPTED, "{b2}");
    let id2 = b2["id"].as_str().unwrap().to_string();

    tokio::time::sleep(Duration::from_millis(150)).await;
    let inv2 = h.store.get_for_claims(&alice_claims(), &id2).await.unwrap();
    assert_eq!(inv2.state, InvocationState::Queued);
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);

    h.release.notify_one();
    wait_calls(&h.calls, 2).await;
    wait_state(&h.store, &alice_claims(), &id1, InvocationState::Succeeded).await;

    h.release.notify_one();
    wait_state(&h.store, &alice_claims(), &id2, InvocationState::Succeeded).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifo_scopes_do_not_share_per_scope_quota() {
    let h = make_harness(
        InvocationRunningLimits {
            global: 8,
            per_tenant: 8,
            per_scope: 1,
        },
        InputRefRegistry::new(),
        false,
    );

    let (s1, _) = create_prompt(&h, &alice(), json!({"prompt": "a"})).await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    wait_calls(&h.calls, 1).await;

    let bob = token("bob", "tenant-b", Some("project-b"), &[]);
    let (s2, _) = create_prompt(&h, &bob, json!({"prompt": "b"})).await;
    assert_eq!(s2, StatusCode::ACCEPTED);
    wait_calls(&h.calls, 2).await;

    assert_eq!(
        h.scheduler
            .scope_running_count(&RunningScope {
                tenant_id: "tenant-a".into(),
                project_id: "project-a".into(),
            })
            .await,
        1
    );
    assert_eq!(
        h.scheduler
            .scope_running_count(&RunningScope {
                tenant_id: "tenant-b".into(),
                project_id: "project-b".into(),
            })
            .await,
        1
    );

    h.release.notify_waiters();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_tenant_cap_queues_other_projects_of_same_tenant() {
    let h = make_harness(
        InvocationRunningLimits {
            global: 8,
            per_tenant: 1,
            per_scope: 4,
        },
        InputRefRegistry::new(),
        false,
    );

    let (s1, b1) = create_prompt(&h, &alice(), json!({"prompt": "a"})).await;
    assert_eq!(s1, StatusCode::ACCEPTED, "{b1}");
    let id1 = b1["id"].as_str().unwrap().to_string();
    wait_calls(&h.calls, 1).await;

    // Same tenant, different project: blocked by the per-tenant cap, stays
    // queued with no new error code (HTTP contract unchanged).
    let alice_p2 = token("alice", "tenant-a", Some("project-b"), &[]);
    let alice_p2_claims = IsolationClaims::from_verified("tenant-a", "project-b", "alice").unwrap();
    let (s2, b2) = create_prompt(&h, &alice_p2, json!({"prompt": "a2"})).await;
    assert_eq!(s2, StatusCode::ACCEPTED, "{b2}");
    let id2 = b2["id"].as_str().unwrap().to_string();

    // Another tenant still starts.
    let bob = token("bob", "tenant-b", Some("project-b"), &[]);
    let (s3, b3) = create_prompt(&h, &bob, json!({"prompt": "b"})).await;
    assert_eq!(s3, StatusCode::ACCEPTED, "{b3}");
    wait_calls(&h.calls, 2).await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    let inv2 = h
        .store
        .get_for_claims(&alice_p2_claims, &id2)
        .await
        .unwrap();
    assert_eq!(inv2.state, InvocationState::Queued);
    assert_eq!(h.calls.load(Ordering::SeqCst), 2);
    assert_eq!(h.scheduler.tenant_running_count("tenant-a").await, 1);
    assert_eq!(h.scheduler.tenant_running_count("tenant-b").await, 1);

    // Finishing one tenant-a run lets the queued project-b invocation start.
    // `notify_waiters` releases both held runs; the queued one then starts.
    h.release.notify_waiters();
    wait_state(&h.store, &alice_claims(), &id1, InvocationState::Succeeded).await;
    wait_calls(&h.calls, 3).await;
    wait_state(&h.store, &alice_p2_claims, &id2, InvocationState::Running).await;
    assert_eq!(h.scheduler.tenant_running_count("tenant-a").await, 1);

    // `notify_one` keeps a permit if the executor has not parked yet.
    h.release.notify_one();
    wait_state(&h.store, &alice_p2_claims, &id2, InvocationState::Succeeded).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_while_queued_fails_with_deadline_exceeded() {
    let h = make_harness(
        InvocationRunningLimits {
            global: 8,
            per_tenant: 8,
            per_scope: 1,
        },
        InputRefRegistry::new(),
        false,
    );
    let auth = alice();

    let (s1, _) = create_prompt(&h, &auth, json!({"prompt": "blocker"})).await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    wait_calls(&h.calls, 1).await;

    let deadline = (chrono::Utc::now() + chrono::Duration::milliseconds(250)).to_rfc3339();
    let (s2, b2) = create_prompt(&h, &auth, json!({"prompt": "soon", "deadline": deadline})).await;
    assert_eq!(s2, StatusCode::ACCEPTED, "{b2}");
    let id2 = b2["id"].as_str().unwrap().to_string();

    wait_state(&h.store, &alice_claims(), &id2, InvocationState::Failed).await;
    let inv2 = h.store.get_for_claims(&alice_claims(), &id2).await.unwrap();
    assert_eq!(
        inv2.error.as_ref().map(|e| e.code.as_str()),
        Some(DEADLINE_EXCEEDED_ERROR_CODE)
    );
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);

    h.release.notify_one();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_exceeded_fails_with_usage() {
    let h = make_harness(
        InvocationRunningLimits::default(),
        InputRefRegistry::new(),
        true,
    );
    let (status, body) = create_prompt(
        &h,
        &alice(),
        json!({
            "prompt": "burn",
            "budget": {"max_cost": 10, "max_tokens": 5}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    wait_calls(&h.calls, 1).await;
    h.release.notify_one();

    wait_state(&h.store, &alice_claims(), &id, InvocationState::Failed).await;
    let inv = h.store.get_for_claims(&alice_claims(), &id).await.unwrap();
    assert_eq!(
        inv.error.as_ref().map(|e| e.code.as_str()),
        Some(BUDGET_EXCEEDED_ERROR_CODE)
    );
    assert!(inv.result.as_ref().and_then(|r| r.usage.as_ref()).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_ref_digest_mismatch_fails() {
    let body = b"payload-v1";
    let registry = InputRefRegistry::new();
    registry.register(
        "mem",
        Arc::new(MemResolver {
            body: body.to_vec(),
        }),
    );
    let h = make_harness(InvocationRunningLimits::default(), registry, false);
    let wrong = "0".repeat(64);
    let (status, resp) = create_prompt(
        &h,
        &alice(),
        json!({
            "prompt": "with-ref",
            "input_ref": {"uri": "mem://x", "sha256": wrong}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    let id = resp["id"].as_str().unwrap().to_string();

    wait_state(&h.store, &alice_claims(), &id, InvocationState::Failed).await;
    let inv = h.store.get_for_claims(&alice_claims(), &id).await.unwrap();
    assert_eq!(
        inv.error.as_ref().map(|e| e.code.as_str()),
        Some(INPUT_DIGEST_MISMATCH_ERROR_CODE)
    );
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_ref_unregistered_scheme_still_422() {
    let h = make_harness(
        InvocationRunningLimits::default(),
        InputRefRegistry::new(),
        false,
    );
    let (status, body) = create_prompt(
        &h,
        &alice(),
        json!({
            "input_ref": {
                "uri": "s3://bucket/key",
                "sha256": sha256_hex(b"x")
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "input_ref_unresolvable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_ref_matching_digest_runs() {
    let payload = b"ok-bytes";
    let registry = InputRefRegistry::new();
    registry.register(
        "mem",
        Arc::new(MemResolver {
            body: payload.to_vec(),
        }),
    );
    let h = make_harness(InvocationRunningLimits::default(), registry, false);
    let (status, resp) = create_prompt(
        &h,
        &alice(),
        json!({
            "prompt": "go",
            "input_ref": {"uri": "mem://x", "sha256": sha256_hex(payload)}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    let id = resp["id"].as_str().unwrap().to_string();
    wait_calls(&h.calls, 1).await;
    h.release.notify_one();
    wait_state(&h.store, &alice_claims(), &id, InvocationState::Succeeded).await;
}

/// Same-scope burst under per_scope>1 must admit concurrently (安野 #332 blocker).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifo_same_scope_burst_runs_under_per_scope_cap() {
    let h = make_harness(
        InvocationRunningLimits {
            global: 8,
            per_tenant: 8,
            per_scope: 4,
        },
        InputRefRegistry::new(),
        false,
    );
    let auth = alice();
    let (s1, _) = create_prompt(&h, &auth, json!({"prompt": "a"})).await;
    let (s2, _) = create_prompt(&h, &auth, json!({"prompt": "b"})).await;
    let (s3, _) = create_prompt(&h, &auth, json!({"prompt": "c"})).await;
    assert_eq!(
        (s1, s2, s3),
        (
            StatusCode::ACCEPTED,
            StatusCode::ACCEPTED,
            StatusCode::ACCEPTED
        )
    );
    wait_calls(&h.calls, 3).await;
    assert_eq!(
        h.scheduler
            .scope_running_count(&RunningScope {
                tenant_id: "tenant-a".into(),
                project_id: "project-a".into(),
            })
            .await,
        3
    );
    h.release.notify_waiters();
}
