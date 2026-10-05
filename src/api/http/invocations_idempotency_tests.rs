//! `Idempotency-Key` tests for `POST /v1/invocations` (#315).

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    body::{to_bytes, Body},
    http::{HeaderValue, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::tests::{call, env, router, token, Reply};
use super::*;
use crate::api::http::{
    control_plane_route_auth_tests::{test_state_with_invocations, EnvGuard},
    invocations_store::{
        IdempotencyLookup, IdempotencyRegistration, InvocationStoreConfig, IDEMPOTENCY_TTL_ENV,
        INTERRUPTED_ERROR_CODE, RETENTION_DAYS_ENV,
    },
    TEST_ENV_LOCK,
};
use crate::isolation::IsolationClaims;

const PROMPT_CANARY: &str = "canary-idem-prompt-81c4-do-not-echo";
const META_CANARY: &str = "canary-idem-meta-2b9e-do-not-echo";
const KEY: &str = "run-7f3a:create";

#[derive(Default)]
struct CountingDispatcher(AtomicUsize);

impl InvocationDispatcher for CountingDispatcher {
    fn dispatch(&self, _invocation: &Invocation) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Harness {
    router: Router,
    store: Arc<InvocationStore>,
    dispatched: Arc<CountingDispatcher>,
    /// Store directory (survives a simulated restart).
    dir: tempfile::TempDir,
    /// App-state directory; each state needs its own (it opens a database).
    _state_dir: tempfile::TempDir,
}

impl Harness {
    fn dispatched(&self) -> usize {
        self.dispatched.0.load(Ordering::SeqCst)
    }

    /// Records on disk (the source of truth after a restart).
    fn disk(&self) -> Vec<Value> {
        let raw = std::fs::read(self.store.path()).unwrap_or_default();
        if raw.is_empty() {
            return Vec::new();
        }
        serde_json::from_slice(&raw).unwrap()
    }

    /// Router over the same store with a different execution switch. Keep
    /// the returned directory alive as long as the router.
    fn router_with_switch(&self, execution_enabled: bool) -> (Router, tempfile::TempDir) {
        let state_dir = tempfile::tempdir().unwrap();
        let router = router(test_state_with_invocations(
            state_dir.path(),
            InvocationsRuntime::new(Some(self.store.clone()), execution_enabled)
                .with_dispatcher(self.dispatched.clone()),
        ));
        (router, state_dir)
    }
}

fn harness_in(dir: tempfile::TempDir, config: InvocationStoreConfig, enabled: bool) -> Harness {
    let (store, _) =
        InvocationStore::open_with_config(dir.path().join("invocations.json"), config).unwrap();
    let store = Arc::new(store);
    let dispatched = Arc::new(CountingDispatcher::default());
    let state_dir = tempfile::tempdir().unwrap();
    let state = test_state_with_invocations(
        state_dir.path(),
        InvocationsRuntime::new(Some(store.clone()), enabled).with_dispatcher(dispatched.clone()),
    );
    Harness {
        router: router(state),
        store,
        dispatched,
        dir,
        _state_dir: state_dir,
    }
}

fn harness_with(config: InvocationStoreConfig) -> Harness {
    harness_in(tempfile::tempdir().unwrap(), config, true)
}

fn harness() -> Harness {
    harness_with(InvocationStoreConfig::default())
}

fn alice() -> String {
    token("alice", "tenant-a", Some("project-a"), &[])
}

fn deadline() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(2))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn base_body(deadline: &str) -> Value {
    json!({
        "prompt": PROMPT_CANARY,
        "input": {"rows": [1, 2, 3], "label": "in-7c1d"},
        "budget": {"max_tokens": 4321},
        "deadline": deadline,
        "metadata": {"trace": META_CANARY, "n": 7},
    })
}

async fn post(router: &Router, auth: &str, key: Option<&str>, body: &str) -> Reply {
    let extra: Vec<(&str, &str)> = key.map(|k| ("idempotency-key", k)).into_iter().collect();
    call(
        router,
        "POST",
        "/v1/invocations",
        Some(auth),
        &extra,
        Some(body.to_string()),
    )
    .await
}

/// Sends raw header bytes that `&str` cannot express (control chars, obs-text).
async fn post_raw_key(router: &Router, auth: &str, key: &[u8], body: &str) -> Reply {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/invocations")
        .header("authorization", format!("Bearer {auth}"))
        .header("content-type", "application/json")
        .header("idempotency-key", HeaderValue::from_bytes(key).unwrap())
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Reply {
        status,
        headers,
        bytes,
    }
}

fn keyed_records(h: &Harness, key: &str) -> usize {
    h.disk()
        .iter()
        .filter(|r| r["idempotency_key"] == key)
        .count()
}

fn claims(tenant: &str, project: &str, actor: &str) -> IsolationClaims {
    IsolationClaims::from_verified(tenant, project, actor).unwrap()
}

#[tokio::test]
async fn idempotent_replay_returns_200_same_id_and_dispatches_once() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = base_body(&deadline()).to_string();

    let first = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.json());
    assert!(first.headers.get("idempotent-replayed").is_none());
    let created = first.json();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["idempotency_key"], KEY);
    assert!(created.get("idempotency").is_none(), "binding is internal");
    assert!(!first.bytes.windows(11).any(|w| w == b"fingerprint"));

    for attempt in 0..2 {
        let replay = post(&h.router, &alice(), Some(KEY), &body).await;
        assert_eq!(replay.status, StatusCode::OK, "attempt {attempt}");
        assert_eq!(replay.headers["idempotent-replayed"], "true");
        assert_eq!(replay.headers["etag"], "\"1\"");
        assert_eq!(
            replay.headers["location"],
            format!("/v1/invocations/{id}").as_str()
        );
        assert_eq!(replay.json(), created);
    }
    assert_eq!(h.disk().len(), 1);
    assert_eq!(h.dispatched(), 1, "executor called exactly once");

    // Headers are not part of the fingerprint; key order and whitespace are not either.
    let reply = call(
        &h.router,
        "POST",
        "/v1/invocations",
        Some(&alice()),
        &[
            ("idempotency-key", KEY),
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
        ],
        Some(body.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["id"], id.as_str());
    let reordered: Value = serde_json::from_str(&body).unwrap();
    let mut pretty = String::from("{\n");
    let map = reordered.as_object().unwrap();
    let mut keys: Vec<&String> = map.keys().collect();
    keys.reverse();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            pretty.push_str(",\n");
        }
        pretty.push_str(&format!("  {} :  {}", json!(k), map[*k]));
    }
    pretty.push_str("\n}");
    let reply = post(&h.router, &alice(), Some(KEY), &pretty).await;
    assert_eq!(reply.status, StatusCode::OK, "{pretty}");
    assert_eq!(reply.json()["id"], id.as_str());

    // State changes are visible on replay: the current view, not a snapshot.
    let cancelled = call(
        &h.router,
        "POST",
        &format!("/v1/invocations/{id}/cancel"),
        Some(&alice()),
        &[],
        None,
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK);
    let replay = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json()["state"], "cancelled");
    assert_eq!(replay.headers["etag"], "\"2\"");
    assert_eq!(h.dispatched(), 1);
}

#[tokio::test]
async fn idempotent_replay_survives_a_deadline_that_has_since_passed() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    // Deadline two seconds out: valid at create, in the past for the retry.
    let soon = (chrono::Utc::now() + chrono::Duration::seconds(2))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let body = json!({"prompt": "p", "deadline": soon}).to_string();
    let first = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.json());
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
    let without_key = post(&h.router, &alice(), None, &body).await;
    assert_eq!(
        without_key.status,
        StatusCode::BAD_REQUEST,
        "deadline passed"
    );
    let replay = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json()["id"], first.json()["id"]);
}

#[tokio::test]
async fn idempotency_conflict_on_any_changed_field_is_409_without_echo() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let deadline = deadline();
    let base = base_body(&deadline);
    let first = post(&h.router, &alice(), Some(KEY), &base.to_string()).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.json());

    let later = (chrono::Utc::now() + chrono::Duration::hours(3))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut variants: Vec<(&str, Value)> = Vec::new();
    let mut v = base.clone();
    v["prompt"] = json!("another prompt");
    variants.push(("prompt", v));
    let mut v = base.clone();
    v["agent_id"] = json!("agent-z");
    v["agent_revision"] = json!("rev-9");
    variants.push(("agent_revision", v));
    let mut v = base.clone();
    v["input"]["rows"][2] = json!(4);
    variants.push(("input", v));
    let mut v = base.clone();
    v.as_object_mut().unwrap().remove("input");
    v["input_ref"] = json!({"uri": "s3://b/k", "sha256": "b".repeat(64)});
    variants.push(("input_ref", v));
    let mut v = base.clone();
    v["budget"]["max_tokens"] = json!(4322);
    variants.push(("budget.max_tokens", v));
    let mut v = base.clone();
    v["deadline"] = json!(later);
    variants.push(("deadline", v));
    let mut v = base.clone();
    v["metadata"]["n"] = json!(8);
    variants.push(("metadata", v));
    let mut v = base.clone();
    v["metadata"]["extra"] = Value::Null;
    variants.push(("metadata added null", v));
    // Invalid bodies conflict too (the key is taken), and still persist nothing.
    let mut v = base.clone();
    v["tenant_id"] = json!("tenant-b");
    variants.push(("forbidden field", v));

    let forbidden = [
        PROMPT_CANARY,
        META_CANARY,
        "in-7c1d",
        "4321",
        deadline.as_str(),
        "another prompt",
        "agent-z",
        "rev-9",
        "4322",
        later.as_str(),
        KEY,
    ];
    for (name, body) in variants {
        let reply = post(&h.router, &alice(), Some(KEY), &body.to_string()).await;
        assert_eq!(
            reply.status,
            StatusCode::CONFLICT,
            "{name}: {}",
            reply.json()
        );
        assert_eq!(reply.code(), "idempotency_key_conflict", "{name}");
        let text = String::from_utf8(reply.bytes.to_vec()).unwrap();
        for canary in forbidden {
            assert!(!text.contains(canary), "{name}: response leaks {canary}");
        }
        let body = reply.json();
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["error", "message"], "{name}");
        assert!(reply.headers.get("idempotent-replayed").is_none());
        assert_eq!(h.disk().len(), 1, "{name}");
    }
    assert_eq!(h.dispatched(), 1);
}

#[tokio::test]
async fn idempotency_key_syntax_is_1_to_255_visible_ascii() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = json!({"prompt": "p"}).to_string();
    let max = "~".repeat(255);
    for key in ["run-1:create", max.as_str(), "!", "a/b?c=d#e"] {
        let reply = post(&h.router, &alice(), Some(key), &body).await;
        assert_eq!(
            reply.status,
            StatusCode::ACCEPTED,
            "{key}: {}",
            reply.json()
        );
    }
    let created = h.disk().len();
    let too_long = "k".repeat(256);
    let bad: Vec<Vec<u8>> = vec![
        too_long.into_bytes(),
        b"".to_vec(),
        b"has space".to_vec(),
        b"tab\there".to_vec(),
        b"obs\xfftext".to_vec(),
        "ключ".as_bytes().to_vec(),
    ];
    for key in bad {
        let reply = post_raw_key(&h.router, &alice(), &key, &body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{key:?}");
        assert_eq!(reply.code(), "invalid_idempotency_key", "{key:?}");
        assert_eq!(h.disk().len(), created, "{key:?} persisted");
    }
    // Two Idempotency-Key headers are ambiguous.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/invocations")
        .header("authorization", format!("Bearer {}", alice()))
        .header("idempotency-key", "a")
        .header("idempotency-key", "b")
        .body(Body::from(body.clone()))
        .unwrap();
    let response = h.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.disk().len(), created);
    assert_eq!(h.dispatched(), created);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn idempotency_50_concurrent_creates_make_exactly_one_resource() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = base_body(&deadline()).to_string();
    let auth = alice();
    let barrier = Arc::new(tokio::sync::Barrier::new(50));
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let router = h.router.clone();
        let body = body.clone();
        let auth = auth.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            post(&router, &auth, Some(KEY), &body).await
        }));
    }
    let mut created = Vec::new();
    let mut replayed = Vec::new();
    let mut in_progress = 0;
    for task in tasks {
        let reply = task.await.unwrap();
        match reply.status {
            StatusCode::ACCEPTED => created.push(reply.json()["id"].clone()),
            StatusCode::OK => {
                assert_eq!(reply.headers["idempotent-replayed"], "true");
                replayed.push(reply.json()["id"].clone());
            }
            StatusCode::CONFLICT => {
                assert_eq!(reply.code(), "idempotency_key_in_progress");
                assert_eq!(reply.headers["retry-after"], "1");
                in_progress += 1;
            }
            other => panic!("unexpected {other}: {}", reply.json()),
        }
    }
    assert_eq!(created.len(), 1, "exactly one 202");
    assert!(replayed.iter().all(|id| *id == created[0]));
    assert_eq!(1 + replayed.len() + in_progress, 50);
    assert_eq!(h.disk().len(), 1);
    assert_eq!(h.dispatched(), 1);
    // Once the first commit is done every retry replays.
    let reply = post(&h.router, &auth, Some(KEY), &body).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn idempotency_key_in_progress_while_reserved_and_free_afterwards() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = json!({"prompt": "p"}).to_string();
    let reservation = h
        .store
        .reserve_idempotency_key(&claims("tenant-a", "project-a", "alice"), KEY)
        .unwrap();
    let reply = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "idempotency_key_in_progress");
    assert_eq!(reply.headers["retry-after"], "1");
    // Other scopes are not blocked by alice's reservation.
    let bob = token("bob", "tenant-a", Some("project-a"), &[]);
    assert_eq!(
        post(&h.router, &bob, Some(KEY), &body).await.status,
        StatusCode::ACCEPTED
    );
    assert_eq!(h.disk().len(), 1);
    drop(reservation);
    assert_eq!(
        post(&h.router, &alice(), Some(KEY), &body).await.status,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn idempotency_scope_isolates_tenants_and_actors() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = base_body(&deadline()).to_string();
    let other = json!({"prompt": "different"}).to_string();
    let a = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(a.status, StatusCode::ACCEPTED);
    let bob = token("bob", "tenant-a", Some("project-a"), &[]);
    let carol = token("carol", "tenant-b", Some("project-a"), &[]);
    let alice_p2 = token("alice", "tenant-a", Some("project-b"), &[]);
    let alice_t2 = token("alice", "tenant-b", Some("project-a"), &[]);
    let mut ids = vec![a.json()["id"].clone()];
    for (who, auth) in [
        ("bob same body", &bob),
        ("carol same body", &carol),
        ("alice other project", &alice_p2),
        ("alice other tenant", &alice_t2),
    ] {
        let reply = post(&h.router, auth, Some(KEY), &body).await;
        assert_eq!(
            reply.status,
            StatusCode::ACCEPTED,
            "{who}: {}",
            reply.json()
        );
        assert!(reply.headers.get("idempotent-replayed").is_none(), "{who}");
        ids.push(reply.json()["id"].clone());
    }
    // A different body under the same key in another scope is not a conflict.
    let dave = token("dave", "tenant-a", Some("project-a"), &[]);
    let reply = post(&h.router, &dave, Some(KEY), &other).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.json());
    ids.push(reply.json()["id"].clone());
    ids.sort_by_key(|v| v.to_string());
    ids.dedup();
    assert_eq!(ids.len(), 6, "every scope created its own resource");
    // And each scope replays its own resource.
    let replay = post(&h.router, &bob, Some(KEY), &body).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_ne!(replay.json()["id"], a.json()["id"]);
    let replay = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(replay.json()["id"], a.json()["id"]);
    assert_eq!(h.dispatched(), 6);
}

#[tokio::test]
async fn idempotency_binding_expires_after_ttl_and_the_key_creates_again() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness_with(InvocationStoreConfig {
        idempotency_ttl: chrono::Duration::milliseconds(800),
        ..InvocationStoreConfig::default()
    });
    let body = json!({"prompt": "p"}).to_string();
    let first = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    let first_id = first.json()["id"].clone();
    assert_eq!(
        post(&h.router, &alice(), Some(KEY), &body).await.status,
        StatusCode::OK
    );
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    // Expired: a different body is a new request, not a conflict.
    let changed = json!({"prompt": "changed"}).to_string();
    let second = post(&h.router, &alice(), Some(KEY), &changed).await;
    assert_eq!(second.status, StatusCode::ACCEPTED, "{}", second.json());
    assert_ne!(second.json()["id"], first_id);
    // Lazy cleanup ran inside that create: the old binding is gone from disk,
    // the old record keeps its public idempotency_key.
    let disk = h.disk();
    let old = disk.iter().find(|r| r["id"] == first_id).unwrap();
    assert_eq!(old["idempotency_key"], KEY);
    assert!(old.get("idempotency").is_none(), "{old}");
    let replay = post(&h.router, &alice(), Some(KEY), &changed).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json()["id"], second.json()["id"]);
    assert_eq!(h.dispatched(), 2);
}

#[tokio::test]
async fn idempotency_ttl_uses_the_injected_clock() {
    let dir = tempfile::tempdir().unwrap();
    let config = InvocationStoreConfig {
        idempotency_ttl: chrono::Duration::hours(24),
        ..InvocationStoreConfig::default()
    };
    let (store, _) =
        InvocationStore::open_with_config(dir.path().join("invocations.json"), config).unwrap();
    let claims = claims("tenant-a", "project-a", "alice");
    let created = store
        .create_idempotent_for_claims(&claims, registered("fp-1"))
        .await
        .unwrap();
    let CreateOutcome::Created(created) = created else {
        panic!("expected a new record");
    };
    let now = chrono::Utc::now();
    let live = store
        .find_idempotent_at(&claims, KEY, "fp-1", now + chrono::Duration::hours(23))
        .await;
    assert_eq!(live, IdempotencyLookup::Replay(created.clone()));
    assert_eq!(
        store
            .find_idempotent_at(&claims, KEY, "fp-2", now + chrono::Duration::hours(23))
            .await,
        IdempotencyLookup::Conflict
    );
    assert_eq!(
        store
            .find_idempotent_at(&claims, KEY, "fp-1", now + chrono::Duration::hours(25))
            .await,
        IdempotencyLookup::Miss
    );
}

fn registered(fingerprint: &str) -> NewInvocation {
    NewInvocation {
        request: InvocationRequest {
            prompt: Some("p".into()),
            ..InvocationRequest::default()
        },
        task_iri: None,
        idempotency: Some(IdempotencyRegistration {
            key: KEY.into(),
            fingerprint: fingerprint.into(),
        }),
    }
}

#[tokio::test]
async fn idempotency_store_create_checks_fingerprint_and_actor_under_the_write_lock() {
    // The route's read-only lookup is an early exit; the store must enforce
    // the same rules on its own (no lookup before it here).
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = InvocationStore::open(dir.path().join("invocations.json")).unwrap();
    let alice = claims("tenant-a", "project-a", "alice");
    let bob = claims("tenant-a", "project-a", "bob");
    let CreateOutcome::Created(first) = store
        .create_idempotent_for_claims(&alice, registered("fp-1"))
        .await
        .unwrap()
    else {
        panic!("expected a new record");
    };
    assert_eq!(
        store
            .create_idempotent_for_claims(&alice, registered("fp-1"))
            .await
            .unwrap(),
        CreateOutcome::Replayed(first.clone())
    );
    assert_eq!(
        store
            .create_idempotent_for_claims(&alice, registered("fp-2"))
            .await
            .unwrap_err(),
        InvocationStoreError::IdempotencyKeyConflict
    );
    let CreateOutcome::Created(other) = store
        .create_idempotent_for_claims(&bob, registered("fp-2"))
        .await
        .unwrap()
    else {
        panic!("bob must get his own record");
    };
    assert_ne!(other.id, first.id);
    assert_eq!(store.list_for_claims(&alice, None).await.len(), 2);
}

#[tokio::test]
async fn idempotency_records_survive_restart_and_replay_the_interrupted_resource() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = base_body(&deadline()).to_string();
    let first = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    let id = first.json()["id"].clone();
    let Harness { dir, .. } = h;

    // Restart: recovery marks the queued invocation failed/interrupted.
    let h = harness_in(dir, InvocationStoreConfig::default(), true);
    let replay = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.headers["idempotent-replayed"], "true");
    let view = replay.json();
    assert_eq!(view["id"], id);
    assert_eq!(view["state"], "failed");
    assert_eq!(view["error"]["code"], INTERRUPTED_ERROR_CODE);
    let changed = json!({"prompt": "resubmit"}).to_string();
    let conflict = post(&h.router, &alice(), Some(KEY), &changed).await;
    assert_eq!(conflict.code(), "idempotency_key_conflict");
    // Resubmitting needs a new key.
    let fresh = post(&h.router, &alice(), Some("run-7f3a:create:2"), &body).await;
    assert_eq!(fresh.status, StatusCode::ACCEPTED);
    assert_eq!(h.dispatched(), 1);
}

#[tokio::test]
async fn idempotency_startup_drops_expired_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invocations.json");
    let config = InvocationStoreConfig {
        idempotency_ttl: chrono::Duration::milliseconds(1),
        ..InvocationStoreConfig::default()
    };
    let (store, _) = InvocationStore::open_with_config(&path, config).unwrap();
    let claims = claims("tenant-a", "project-a", "alice");
    store
        .create_idempotent_for_claims(&claims, registered("fp-1"))
        .await
        .unwrap();
    drop(store);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (store, report) = InvocationStore::open_with_config(&path, config).unwrap();
    assert_eq!(report.idempotency_expired, 1);
    let disk: Vec<Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(disk[0].get("idempotency").is_none(), "persisted at startup");
    assert_eq!(disk[0]["idempotency_key"], KEY);
    assert_eq!(
        store.find_idempotent_for_claims(&claims, KEY, "fp-1").await,
        IdempotencyLookup::Miss
    );
}

#[tokio::test]
async fn idempotency_rejected_creates_leave_no_trace_and_free_the_key() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness_with(InvocationStoreConfig {
        max_active_per_scope: 1,
        ..InvocationStoreConfig::default()
    });
    let rejected: Vec<(&str, String, StatusCode)> = vec![
        (
            "400 field",
            json!({"prompt": "p", "topology": "x"}).to_string(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "400 not json",
            "{\"prompt\": ".to_string(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "413 body",
            json!({"prompt": "p".repeat(64 * 1024)}).to_string(),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "413 metadata",
            json!({"prompt": "p", "metadata": {"k": "m".repeat(16_377)}}).to_string(),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "422 agent",
            json!({"prompt": "p", "agent_id": "agent-x"}).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "422 input_ref",
            json!({"input_ref": {"uri": "s3://b/k", "sha256": "c".repeat(64)}}).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    for (name, body, status) in &rejected {
        let reply = post(&h.router, &alice(), Some(KEY), body).await;
        assert_eq!(reply.status, *status, "{name}: {}", reply.json());
        assert_eq!(h.disk().len(), 0, "{name}");
    }

    // 503 execution_disabled: same store, switch off.
    let (off, _off_dir) = h.router_with_switch(false);
    let body = json!({"prompt": "valid"}).to_string();
    let reply = post(&off, &alice(), Some(KEY), &body).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "execution_disabled");
    assert_eq!(h.disk().len(), 0);

    // Persistence failure: 500, no record, no binding in memory or on disk.
    h.store.fail_writes_after(Some(0));
    let reply = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.code(), "persistence_failed");
    h.store.fail_writes_after(None);
    assert_eq!(h.disk().len(), 0);
    assert_eq!(h.dispatched(), 0);

    // 429 too_many_active with a fresh key while another invocation is active.
    let blocker = post(&h.router, &alice(), None, &body).await;
    assert_eq!(blocker.status, StatusCode::ACCEPTED);
    let reply = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(keyed_records(&h, KEY), 0);
    let blocker_id = blocker.json()["id"].as_str().unwrap().to_string();
    let cancel = call(
        &h.router,
        "POST",
        &format!("/v1/invocations/{blocker_id}/cancel"),
        Some(&alice()),
        &[],
        None,
    )
    .await;
    assert_eq!(cancel.status, StatusCode::OK);

    // The key is still free: a valid request now creates.
    let reply = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.json());
    assert_eq!(keyed_records(&h, KEY), 1);
    assert_eq!(h.dispatched(), 2);
}

#[tokio::test]
async fn idempotency_record_and_resource_are_one_write() {
    // Mutation guard: registering the key in a write separate from the
    // record would leave one without the other when the second write fails.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = json!({"prompt": "p"}).to_string();

    // Exactly one write is allowed: the whole create must fit in it.
    h.store.fail_writes_after(Some(1));
    let first = post(&h.router, &alice(), Some(KEY), &body).await;
    h.store.fail_writes_after(None);
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.json());
    let disk = h.disk();
    assert_eq!(disk.len(), 1);
    assert!(
        disk[0].get("idempotency").is_some(),
        "binding written with the record"
    );

    // A retry replays instead of creating a duplicate.
    let retry = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.json()["id"], first.json()["id"]);
    assert_eq!(h.disk().len(), 1);

    // A failing single write leaves nothing; the retry then creates once.
    let other_key = "run-7f3a:create:2";
    h.store.fail_writes_after(Some(0));
    let failed = post(&h.router, &alice(), Some(other_key), &body).await;
    h.store.fail_writes_after(None);
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(keyed_records(&h, other_key), 0);
    let retry = post(&h.router, &alice(), Some(other_key), &body).await;
    assert_eq!(retry.status, StatusCode::ACCEPTED);
    let replay = post(&h.router, &alice(), Some(other_key), &body).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(keyed_records(&h, other_key), 1);
    assert_eq!(h.dispatched(), 2);
}

#[tokio::test]
async fn idempotency_replays_while_execution_is_switched_off() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let body = json!({"prompt": "p"}).to_string();
    let first = post(&h.router, &alice(), Some(KEY), &body).await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    let (off, _off_dir) = h.router_with_switch(false);
    let replay = post(&off, &alice(), Some(KEY), &body).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.headers["idempotent-replayed"], "true");
    assert_eq!(replay.json()["id"], first.json()["id"]);
    let new_key = post(&off, &alice(), Some("run-7f3a:create:2"), &body).await;
    assert_eq!(new_key.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(new_key.code(), "execution_disabled");
    assert_eq!(keyed_records(&h, "run-7f3a:create:2"), 0);
    assert_eq!(h.disk().len(), 1);
    assert_eq!(h.dispatched(), 1);
}

#[test]
fn idempotency_fingerprint_is_canonical_and_covers_every_field() {
    let fp = |v: Value| request_fingerprint(v.to_string().as_bytes()).unwrap();
    let base = json!({
        "prompt": "p",
        "agent_id": "a",
        "agent_revision": "r1",
        "input_ref": {"uri": "s3://b/k", "sha256": "a".repeat(64)},
        "budget": {"max_tokens": 10, "max_cost": 5},
        "deadline": "2999-01-01T00:00:00Z",
        "metadata": {"z": 1, "a": [1, {"y": 2, "x": 3}]},
    });
    let reference = fp(base.clone());
    assert_eq!(reference.len(), 64);
    // Whitespace and key order do not matter.
    let spaced = "{ \"metadata\" : {\"a\":[1,{\"x\":3,\"y\":2}],\"z\":1},\n\"deadline\":\"2999-01-01T00:00:00Z\", \"budget\":{\"max_cost\":5,\"max_tokens\":10},\"input_ref\":{\"sha256\":\"".to_string()
        + &"a".repeat(64)
        + "\",\"uri\":\"s3://b/k\"},\"agent_revision\":\"r1\",\"agent_id\":\"a\",\"prompt\":\"p\" }";
    assert_eq!(request_fingerprint(spaced.as_bytes()).unwrap(), reference);
    let changes: Vec<(&str, Value)> = vec![
        ("agent_revision", json!("r2")),
        (
            "input_ref",
            json!({"uri": "s3://b/k", "sha256": "b".repeat(64)}),
        ),
        ("budget", json!({"max_tokens": 11, "max_cost": 5})),
        ("deadline", json!("2999-01-01T00:00:01Z")),
        ("metadata", json!({"z": 2, "a": [1, {"y": 2, "x": 3}]})),
        ("prompt", json!("q")),
        ("agent_id", json!("b")),
    ];
    for (field, value) in changes {
        let mut changed = base.clone();
        changed[field] = value;
        assert_ne!(fp(changed), reference, "{field}");
    }
    let mut with_input = base.clone();
    with_input.as_object_mut().unwrap().remove("input_ref");
    with_input["input"] = json!({"k": 1});
    let mut other_input = with_input.clone();
    other_input["input"] = json!({"k": 2});
    assert_ne!(fp(with_input), fp(other_input), "input");
    assert!(request_fingerprint(b"not json").is_none());
    assert!(request_fingerprint(&vec![b' '; MAX_CREATE_BODY_BYTES + 1]).is_none());
}

#[test]
fn idempotency_ttl_above_retention_fails_config() {
    let vars = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    };
    let defaults = InvocationStoreConfig::try_from_vars(vars(&[])).unwrap();
    assert_eq!(defaults.idempotency_ttl, chrono::Duration::hours(24));
    assert_eq!(defaults.retention, chrono::Duration::days(7));

    let equal = InvocationStoreConfig::try_from_vars(vars(&[
        (RETENTION_DAYS_ENV, "7"),
        (IDEMPOTENCY_TTL_ENV, "168"),
    ]))
    .unwrap();
    assert_eq!(equal.idempotency_ttl, chrono::Duration::hours(168));

    let error = InvocationStoreConfig::try_from_vars(vars(&[
        (RETENTION_DAYS_ENV, "7"),
        (IDEMPOTENCY_TTL_ENV, "169"),
    ]))
    .unwrap_err();
    for needle in [IDEMPOTENCY_TTL_ENV, RETENTION_DAYS_ENV, "=169", "=7", "168"] {
        assert!(error.0.contains(needle), "{needle} missing in {error}");
    }
    // Default retention (7 d) applies when only the TTL is set.
    assert!(InvocationStoreConfig::try_from_vars(vars(&[(IDEMPOTENCY_TTL_ENV, "169")])).is_err());
    assert!(InvocationStoreConfig::try_from_vars(vars(&[(RETENTION_DAYS_ENV, "1")])).is_ok());
    assert!(InvocationStoreConfig::try_from_vars(vars(&[
        (RETENTION_DAYS_ENV, "1"),
        (IDEMPOTENCY_TTL_ENV, "25"),
    ]))
    .is_err());
    // A set but invalid TTL never falls back to the default.
    for bad in ["0", "abc", "-1", "1.5", ""] {
        let leaked: &'static str = Box::leak(bad.to_string().into_boxed_str());
        let pairs: &'static [(&'static str, &'static str)] =
            Box::leak(vec![(IDEMPOTENCY_TTL_ENV, leaked)].into_boxed_slice());
        let error = InvocationStoreConfig::try_from_vars(vars(pairs)).unwrap_err();
        assert!(error.0.contains(IDEMPOTENCY_TTL_ENV), "{bad}: {error}");
    }
}

#[test]
#[should_panic(expected = "AGENTOS_INVOCATION_IDEMPOTENCY_TTL_HOURS=169 exceeds")]
fn idempotency_ttl_above_retention_refuses_startup() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        (RETENTION_DAYS_ENV, "7".into()),
        (IDEMPOTENCY_TTL_ENV, "169".into()),
        (
            "AGENTOS_DATA_DIR",
            dir.path().to_string_lossy().into_owned(),
        ),
    ]);
    let _ = InvocationsRuntime::open_default();
}

#[test]
fn idempotency_ttl_equal_to_retention_starts() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        (RETENTION_DAYS_ENV, "7".into()),
        (IDEMPOTENCY_TTL_ENV, "168".into()),
        (
            "AGENTOS_DATA_DIR",
            dir.path().to_string_lossy().into_owned(),
        ),
    ]);
    // Must not panic: open_default validates TTL ≤ retention × 24.
    let _ = InvocationsRuntime::open_default();
    let (store, _) = InvocationStore::open_with_config(
        dir.path().join("invocations.json"),
        InvocationStoreConfig::try_from_env().unwrap(),
    )
    .unwrap();
    assert_eq!(store.config().idempotency_ttl, chrono::Duration::hours(168));
}
