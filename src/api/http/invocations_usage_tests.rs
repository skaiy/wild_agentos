//! #337 regression tests on the production path: `/v1/invocations` → bridge →
//! the real `HttpTaskExecutor` (not a mock executor) → a local
//! OpenAI-compatible stub LLM that reports `usage`. No real model or network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::response::IntoResponse;
use axum::Router;
use serde_json::{json, Value};

use super::tests::{alice, call, env, router, token};
use super::*;
use crate::api::grpc::server::HttpTaskExecutor;
use crate::api::http::control_plane_route_auth_tests::{test_state_with_invocations, EnvGuard};
use crate::api::http::invocations_enforcement::{FifoScheduler, InputRefRegistry};
use crate::api::http::invocations_execution::{InvocationExecutionBridge, ScopedProjectionGate};
use crate::api::http::invocations_store::{CostSource, InvocationStoreConfig, InvocationUsage};
use crate::api::http::TEST_ENV_LOCK;
use crate::config::settings::{ModelPrice, Settings};
use crate::isolation::IsolationClaims;

const STUB_MODEL: &str = "stub-model";
const PROMPT_TOKENS: u64 = 120;
const COMPLETION_TOKENS: u64 = 30;
/// Gateway-reported cost of one call: 0.000123 USD = 123 micro-USD.
const CALL_COST_USD: f64 = 0.000123;
const CALL_COST_MICRO_USD: u64 = 123;
const PROMPT_CANARY: &str = "canary-usage-prompt-7f3a-do-not-leak";
const KEY_CANARY: &str = "canary-usage-key-91c2-do-not-leak";

// ── Stub LLM ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct StubConfig {
    /// Report a gateway cost (`usage.cost`) on every call.
    report_cost: bool,
    /// Honour `stream_options.include_usage` on streaming calls.
    stream_usage: bool,
    /// Delay before every reply, so concurrent runs overlap.
    delay_ms: u64,
    /// Answer `400` to streaming calls that carry `stream_options`, like an
    /// upstream that does not know the option.
    reject_stream_options: bool,
}

#[derive(Default)]
struct StubStats {
    plain_calls: AtomicUsize,
    stream_calls: AtomicUsize,
    stream_calls_asking_usage: AtomicUsize,
    rejected_stream_calls: AtomicUsize,
}

impl StubStats {
    fn total(&self) -> u64 {
        (self.plain_calls.load(Ordering::SeqCst) + self.stream_calls.load(Ordering::SeqCst)) as u64
    }
}

fn stub_usage(config: StubConfig) -> Value {
    let mut usage = json!({
        "prompt_tokens": PROMPT_TOKENS,
        "completion_tokens": COMPLETION_TOKENS,
        "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS,
    });
    if config.report_cost {
        usage["cost"] = json!(CALL_COST_USD);
    }
    usage
}

fn stub_content(body: &Value) -> String {
    if body["messages"]
        .to_string()
        .contains("task planning expert")
    {
        json!({
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

async fn spawn_stub_llm(config: StubConfig) -> (String, Arc<StubStats>) {
    let stats = Arc::new(StubStats::default());
    let handler_stats = stats.clone();
    let handler = move |axum::Json(body): axum::Json<Value>| {
        let stats = handler_stats.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(config.delay_ms)).await;
            let content = stub_content(&body);
            if body["stream"].as_bool() == Some(true) {
                if config.reject_stream_options && body.get("stream_options").is_some() {
                    stats.rejected_stream_calls.fetch_add(1, Ordering::SeqCst);
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"error": {"message": "unknown field stream_options"}})),
                    )
                        .into_response();
                }
                stats.stream_calls.fetch_add(1, Ordering::SeqCst);
                let asked = body["stream_options"]["include_usage"].as_bool() == Some(true);
                if asked {
                    stats
                        .stream_calls_asking_usage
                        .fetch_add(1, Ordering::SeqCst);
                }
                let mut sse = String::new();
                // Like OpenAI, every chunk before the trailing one carries
                // `"usage": null` when usage was asked for.
                for chunk in [
                    json!({"id": "s", "model": STUB_MODEL, "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": null}], "usage": null}),
                    json!({"id": "s", "model": STUB_MODEL, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": null}),
                ] {
                    sse.push_str(&format!("data: {chunk}\n\n"));
                }
                // OpenAI-compatible upstreams send usage only when asked, on a
                // trailing chunk with empty `choices`.
                if asked && config.stream_usage {
                    let chunk = json!({"id": "s", "model": STUB_MODEL, "choices": [], "usage": stub_usage(config)});
                    sse.push_str(&format!("data: {chunk}\n\n"));
                }
                sse.push_str("data: [DONE]\n\n");
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    sse,
                )
                    .into_response()
            } else {
                stats.plain_calls.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({
                    "id": "c",
                    "model": STUB_MODEL,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
                    "usage": stub_usage(config),
                }))
                .into_response()
            }
        }
    };
    let app = Router::new().route("/v1/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), stats)
}

// ── Production harness ────────────────────────────────────────────────────

struct UsageHarness {
    router: Router,
    store: Arc<InvocationStore>,
    stats: Arc<StubStats>,
    _dir: tempfile::TempDir,
    _env: EnvGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn settings_for(base_url: &str, dir: &std::path::Path, price_table: bool) -> Settings {
    let mut settings = Settings::default();
    settings.gateway.base_url = base_url.to_string();
    settings.gateway.api_key = KEY_CANARY.to_string();
    settings.gateway.default_model = STUB_MODEL.to_string();
    settings.gateway.max_retries = 0;
    settings.gateway.timeout_seconds = 20;
    for model in settings.gateway.model_mapping.values_mut() {
        *model = STUB_MODEL.to_string();
    }
    // Separate from the test core's own L0 file.
    settings.memory.l0.path = dir.join("exec-l0").to_string_lossy().into_owned();
    settings.output.directory = dir.join("output").to_string_lossy().into_owned();
    settings.workspace.root = None;
    settings.agents.timeout_seconds = 120;
    if price_table {
        settings.pricing.models.insert(
            STUB_MODEL.to_string(),
            ModelPrice {
                input_usd_per_million_tokens: 2.0,
                output_usd_per_million_tokens: 10.0,
            },
        );
    }
    settings
}

async fn usage_harness(config: StubConfig, price_table: bool) -> UsageHarness {
    let lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env_guard = env(true);
    let dir = tempfile::tempdir().unwrap();
    let (base_url, stats) = spawn_stub_llm(config).await;
    let (store, _) = InvocationStore::open_with_config(
        dir.path().join("invocations.json"),
        InvocationStoreConfig::default(),
    )
    .unwrap();
    let store = Arc::new(store);
    let bootstrap = test_state_with_invocations(
        dir.path(),
        InvocationsRuntime::new(Some(store.clone()), true),
    );
    let executor = Arc::new(HttpTaskExecutor::for_tests(
        &bootstrap.core,
        settings_for(&base_url, dir.path(), price_table),
    ));
    let input_refs = InputRefRegistry::new();
    let runtime =
        InvocationsRuntime::new(Some(store.clone()), true).with_input_refs(input_refs.clone());
    let bridge = Arc::new(InvocationExecutionBridge::new_with_enforcement(
        store.clone(),
        runtime.cancellations().clone(),
        bootstrap.core.clone(),
        executor,
        bootstrap.shutdown.clone(),
        Arc::new(ScopedProjectionGate),
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
    UsageHarness {
        router: router(state),
        store,
        stats,
        _dir: dir,
        _env: env_guard,
        _lock: lock,
    }
}

fn bob() -> String {
    token("bob", "tenant-b", Some("project-b"), &[])
}

fn carol() -> String {
    token("carol", "tenant-a", Some("project-c"), &[])
}

fn carol_claims() -> IsolationClaims {
    IsolationClaims::from_verified("tenant-a", "project-c", "carol").unwrap()
}

fn alice_claims() -> IsolationClaims {
    IsolationClaims::from_verified("tenant-a", "project-a", "alice").unwrap()
}

fn bob_claims() -> IsolationClaims {
    IsolationClaims::from_verified("tenant-b", "project-b", "bob").unwrap()
}

async fn create(h: &UsageHarness, auth: &str, body: Value) -> String {
    let reply = call(
        &h.router,
        "POST",
        "/v1/invocations",
        Some(auth),
        &[],
        Some(body.to_string()),
    )
    .await;
    assert_eq!(
        reply.status,
        axum::http::StatusCode::ACCEPTED,
        "{}",
        reply.json()
    );
    reply.json()["id"].as_str().unwrap().to_string()
}

async fn wait_terminal(h: &UsageHarness, id: &str, claims: &IsolationClaims) -> Invocation {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let inv = h.store.get_for_claims(claims, id).await.unwrap();
        if inv.state.is_terminal() {
            return inv;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out; last={:?}",
            inv.state
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn prompt_body() -> Value {
    json!({"prompt": format!("Say hello. {PROMPT_CANARY}")})
}

fn usage_of(inv: &Invocation) -> &InvocationUsage {
    inv.result
        .as_ref()
        .and_then(|r| r.usage.as_ref())
        .unwrap_or_else(|| panic!("no result.usage: {inv:?}"))
}

/// Nothing persisted in result / error may carry the prompt or the key.
fn assert_no_canary(inv: &Invocation) {
    let result = serde_json::to_string(&inv.result).unwrap();
    let error = serde_json::to_string(&inv.error).unwrap();
    for text in [&result, &error] {
        assert!(!text.contains(PROMPT_CANARY), "prompt leaked: {text}");
        assert!(!text.contains(KEY_CANARY), "key leaked: {text}");
    }
    assert!(!serde_json::to_string(inv).unwrap().contains(KEY_CANARY));
}

const COST_AND_USAGE: StubConfig = StubConfig {
    report_cost: true,
    stream_usage: true,
    delay_ms: 0,
    reject_stream_options: false,
};
const USAGE_NO_COST: StubConfig = StubConfig {
    report_cost: false,
    stream_usage: true,
    delay_ms: 0,
    reject_stream_options: false,
};

// ── Tests ─────────────────────────────────────────────────────────────────

/// Gateway reports usage and cost on streaming and non-streaming calls: the
/// invocation succeeds with the exact sums, `cost_source = gateway`, and the
/// executor's summary (not the scheduler's empty `TASK_COMPLETED`, which
/// reaches the bus first).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_run_succeeds_with_gateway_usage_and_cost() {
    let h = usage_harness(COST_AND_USAGE, false).await;
    let id = create(&h, &alice(), prompt_body()).await;
    let inv = wait_terminal(&h, &id, &alice_claims()).await;

    let calls = h.stats.total();
    assert!(
        h.stats.plain_calls.load(Ordering::SeqCst) > 0,
        "run must make a non-streaming call"
    );
    assert!(
        h.stats.stream_calls.load(Ordering::SeqCst) > 0,
        "run must make a streaming call"
    );
    assert_eq!(
        h.stats.stream_calls_asking_usage.load(Ordering::SeqCst),
        h.stats.stream_calls.load(Ordering::SeqCst),
        "every streaming call must ask for usage"
    );
    assert_eq!(
        inv.state,
        InvocationState::Succeeded,
        "error={:?}",
        inv.error
    );
    let usage = usage_of(&inv);
    assert_eq!(usage.model.as_deref(), Some(STUB_MODEL));
    assert_eq!(usage.input_tokens, Some(calls * PROMPT_TOKENS));
    assert_eq!(usage.output_tokens, Some(calls * COMPLETION_TOKENS));
    assert_eq!(usage.cost, Some(calls * CALL_COST_MICRO_USD));
    assert_eq!(usage.cost_source, Some(CostSource::Gateway));
    assert!(usage.cost.unwrap() > 0);
    assert!(
        !inv.result.as_ref().unwrap().summary.is_empty(),
        "summary must come from the executor's terminal event"
    );
    assert_no_canary(&inv);
}

/// No gateway cost, operator price table configured → priced from the table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_run_prices_from_operator_table_without_gateway_cost() {
    let h = usage_harness(USAGE_NO_COST, true).await;
    let id = create(&h, &alice(), prompt_body()).await;
    let inv = wait_terminal(&h, &id, &alice_claims()).await;
    let calls = h.stats.total();
    assert_eq!(
        inv.state,
        InvocationState::Succeeded,
        "error={:?}",
        inv.error
    );
    let usage = usage_of(&inv);
    let (input, output) = (calls * PROMPT_TOKENS, calls * COMPLETION_TOKENS);
    assert_eq!(usage.input_tokens, Some(input));
    assert_eq!(usage.output_tokens, Some(output));
    // USD per million tokens == micro-USD per token: 2 * in + 10 * out.
    assert_eq!(usage.cost, Some(2 * input + 10 * output));
    assert_eq!(usage.cost_source, Some(CostSource::ConfigPriceTable));
}

/// Neither a gateway cost nor a price table: the run must not succeed, and
/// `cost` is never filled in with zero or an estimate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_run_without_cost_source_fails_incomplete_usage() {
    let h = usage_harness(USAGE_NO_COST, false).await;
    let id = create(&h, &alice(), prompt_body()).await;
    let inv = wait_terminal(&h, &id, &alice_claims()).await;
    assert_eq!(inv.state, InvocationState::Failed);
    let error = inv.error.as_ref().unwrap();
    assert_eq!(error.code, "incomplete_usage");
    assert!(
        error.message.contains("no cost source"),
        "{}",
        error.message
    );
    let usage = usage_of(&inv);
    assert_eq!(usage.cost, None, "cost must not be zero-filled");
    assert_eq!(usage.cost_source, None);
    let calls = h.stats.total();
    assert_eq!(usage.input_tokens, Some(calls * PROMPT_TOKENS));
    assert_no_canary(&inv);
}

/// An upstream that ignores `include_usage` leaves streaming calls without
/// usage: tokens are unknown, so the run fails closed instead of reporting an
/// undercount.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_run_with_missing_stream_usage_fails_closed() {
    let h = usage_harness(
        StubConfig {
            report_cost: true,
            stream_usage: false,
            delay_ms: 0,
            reject_stream_options: false,
        },
        true,
    )
    .await;
    let id = create(&h, &alice(), prompt_body()).await;
    let inv = wait_terminal(&h, &id, &alice_claims()).await;
    assert!(h.stats.stream_calls.load(Ordering::SeqCst) > 0);
    assert_eq!(inv.state, InvocationState::Failed);
    assert_eq!(inv.error.as_ref().unwrap().code, "incomplete_usage");
    let usage = usage_of(&inv);
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.cost, None);
}

/// `max_tokens` / `max_cost` now act on the production path and the failed
/// invocation carries the actual usage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_budget_limits_fail_with_actual_usage() {
    let h = usage_harness(COST_AND_USAGE, false).await;
    for budget in [json!({"max_tokens": 1}), json!({"max_cost": 1})] {
        let before = h.stats.total();
        let id = create(
            &h,
            &alice(),
            json!({"prompt": "Say hello.", "budget": budget}),
        )
        .await;
        let inv = wait_terminal(&h, &id, &alice_claims()).await;
        let calls = h.stats.total() - before;
        assert_eq!(inv.state, InvocationState::Failed, "{budget}");
        assert_eq!(inv.error.as_ref().unwrap().code, "budget_exceeded");
        let usage = usage_of(&inv);
        assert_eq!(usage.input_tokens, Some(calls * PROMPT_TOKENS));
        assert_eq!(usage.cost, Some(calls * CALL_COST_MICRO_USD));
    }
}

/// Three runs at the same time (two projects of one tenant, plus another
/// tenant) each report only their own calls: together they account for
/// exactly the calls the stub served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_concurrent_runs_do_not_mix_usage() {
    let h = usage_harness(
        StubConfig {
            report_cost: true,
            stream_usage: true,
            delay_ms: 150,
            reject_stream_options: false,
        },
        false,
    )
    .await;
    let a = create(&h, &alice(), prompt_body()).await;
    let b = create(&h, &bob(), prompt_body()).await;
    let c = create(&h, &carol(), prompt_body()).await;
    let (claims_a, claims_b, claims_c) = (alice_claims(), bob_claims(), carol_claims());
    let (inv_a, inv_b, inv_c) = tokio::join!(
        wait_terminal(&h, &a, &claims_a),
        wait_terminal(&h, &b, &claims_b),
        wait_terminal(&h, &c, &claims_c)
    );
    for inv in [&inv_a, &inv_b, &inv_c] {
        assert_eq!(
            inv.state,
            InvocationState::Succeeded,
            "error={:?}",
            inv.error
        );
    }
    let usages = [usage_of(&inv_a), usage_of(&inv_b), usage_of(&inv_c)];
    let total = h.stats.total();
    assert!(usages.iter().all(|u| u.input_tokens.unwrap() > 0));
    assert_eq!(
        usages.iter().map(|u| u.input_tokens.unwrap()).sum::<u64>(),
        total * PROMPT_TOKENS,
        "per-run counts must add up to exactly the calls served"
    );
    assert_eq!(
        usages.iter().map(|u| u.cost.unwrap()).sum::<u64>(),
        total * CALL_COST_MICRO_USD
    );
}

/// An upstream that rejects `stream_options` gets the streaming call once more
/// without it; the run still completes, but without stream usage it fails
/// closed as `incomplete_usage` instead of `execution_failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_run_retries_without_stream_options_then_fails_closed() {
    let h = usage_harness(
        StubConfig {
            report_cost: true,
            stream_usage: true,
            delay_ms: 0,
            reject_stream_options: true,
        },
        true,
    )
    .await;
    let id = create(&h, &alice(), prompt_body()).await;
    let inv = wait_terminal(&h, &id, &alice_claims()).await;
    let rejected = h.stats.rejected_stream_calls.load(Ordering::SeqCst);
    assert!(rejected > 0, "the stub must have rejected stream_options");
    assert_eq!(
        h.stats.stream_calls.load(Ordering::SeqCst),
        rejected,
        "every rejected streaming call is retried once without stream_options"
    );
    assert_eq!(h.stats.stream_calls_asking_usage.load(Ordering::SeqCst), 0);
    assert_eq!(inv.state, InvocationState::Failed);
    assert_eq!(inv.error.as_ref().unwrap().code, "incomplete_usage");
    assert_eq!(usage_of(&inv).input_tokens, None);
}
