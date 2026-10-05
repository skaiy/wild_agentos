//! Route tests for `/v1/invocations` (#314).

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
    Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;
use crate::api::http::{
    control_plane_route_auth_tests::{test_state_with_invocations, EnvGuard},
    iam::JwtClaims,
    invocations_store::{InvocationStoreConfig, TransitionPatch},
    TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
const CANARY: &str = "canary-inv-meta-5d1e-do-not-leak";

pub(super) fn env(strict: bool) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(SECRET.to_vec()).unwrap(),
        ),
        ("AGENTOS_AUTH_STRICT", strict.to_string()),
    ])
}

pub(super) fn token(sub: &str, tenant: &str, project: Option<&str>, roles: &[&str]) -> String {
    token_exp(sub, tenant, project, roles, 3600)
}

fn token_exp(sub: &str, tenant: &str, project: Option<&str>, roles: &[&str], ttl: i64) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: sub.into(),
            tenant_id: tenant.into(),
            project_id: project.map(str::to_owned),
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::seconds(ttl)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

fn alice() -> String {
    token("alice", "tenant-a", Some("project-a"), &[])
}

struct Harness {
    router: Router,
    store: Arc<InvocationStore>,
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
}

fn harness_with(execution_enabled: bool, config: InvocationStoreConfig) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) =
        InvocationStore::open_with_config(dir.path().join("invocations.json"), config).unwrap();
    let store = Arc::new(store);
    let state = test_state_with_invocations(
        dir.path(),
        InvocationsRuntime::new(Some(store.clone()), execution_enabled),
    );
    Harness {
        router: router(state.clone()),
        store,
        state,
        _dir: dir,
    }
}

fn harness() -> Harness {
    harness_with(true, InvocationStoreConfig::default())
}

pub(super) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/v1/invocations",
            get(list_invocations_handler).post(create_invocation_handler),
        )
        .route("/v1/invocations/:id", get(get_invocation_handler))
        .route(
            "/v1/invocations/:id/cancel",
            post(cancel_invocation_handler),
        )
        .with_state(state)
}

pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) bytes: Bytes,
}

impl Reply {
    pub(super) fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
    pub(super) fn code(&self) -> String {
        self.json()["error"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

pub(super) async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<String>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
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

async fn create(h: &Harness, auth: &str, body: Value) -> Reply {
    call(
        &h.router,
        "POST",
        "/v1/invocations",
        Some(auth),
        &[],
        Some(body.to_string()),
    )
    .await
}

async fn create_ok(h: &Harness, auth: &str, body: Value) -> Value {
    let reply = create(h, auth, body).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.json());
    reply.json()
}

async fn stored_count(h: &Harness) -> usize {
    let raw = std::fs::read(h.store.path()).unwrap_or_default();
    if raw.is_empty() {
        return 0;
    }
    serde_json::from_slice::<Vec<Value>>(&raw).unwrap().len()
}

const ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/invocations"),
    ("GET", "/v1/invocations"),
    (
        "GET",
        "/v1/invocations/inv_00000000000000000000000000000000",
    ),
    (
        "POST",
        "/v1/invocations/inv_00000000000000000000000000000000/cancel",
    ),
];

#[tokio::test]
async fn invocations_routes_reject_unverified_callers_with_401() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for strict in [true, false] {
        let _env = env(strict);
        let h = harness();
        let expired = token_exp("alice", "tenant-a", Some("project-a"), &[], -3600);
        let x_identity = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            json!({"user_id": "alice", "tenant_id": "tenant-a", "roles": ["DA"]}).to_string(),
        );
        for (method, uri) in ROUTES {
            let body = (*method == "POST").then(|| json!({"prompt": "probe"}).to_string());
            let cases: Vec<(Option<&str>, Vec<(&str, &str)>)> = vec![
                (None, vec![]),
                (Some("not-a-jwt"), vec![]),
                (Some(expired.as_str()), vec![]),
                (None, vec![("x-identity", x_identity.as_str())]),
            ];
            for (auth, extra) in cases {
                let reply = call(&h.router, method, uri, auth, &extra, body.clone()).await;
                assert_eq!(
                    reply.status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {uri} strict={strict} auth={auth:?} extra={extra:?}"
                );
            }
        }
        assert_eq!(stored_count(&h).await, 0);
    }
}

#[tokio::test]
async fn invocations_routes_reject_defaulted_project_with_403() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let defaulted = token("alice", "tenant-a", None, &["DA"]);
    for (method, uri) in ROUTES {
        let body = (*method == "POST").then(|| json!({"prompt": "probe"}).to_string());
        let reply = call(&h.router, method, uri, Some(&defaulted), &[], body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.code(), "claims_incomplete");
        assert_eq!(reply.json()["missing_field"], "project_id");
    }
    assert_eq!(stored_count(&h).await, 0);
}

#[tokio::test]
async fn create_is_503_execution_disabled_by_default_and_persists_nothing() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness_with(false, InvocationStoreConfig::default());
    let reply = create(&h, &alice(), json!({"prompt": "hello"})).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "execution_disabled");
    assert_eq!(stored_count(&h).await, 0);
    let listed = call(
        &h.router,
        "GET",
        "/v1/invocations",
        Some(&alice()),
        &[],
        None,
    )
    .await;
    assert_eq!(listed.json()["data"], json!([]));

    // A store that failed to open: 503 after authentication, 401 before.
    let dir = tempfile::tempdir().unwrap();
    let router = router(test_state_with_invocations(
        dir.path(),
        InvocationsRuntime::unavailable(),
    ));
    let reply = call(&router, "GET", "/v1/invocations", Some(&alice()), &[], None).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "invocation_store_unavailable");
    let reply = call(&router, "GET", "/v1/invocations", None, &[], None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_returns_202_queued_and_echoes_request_verbatim() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let deadline = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let metadata = json!({"run": "r-1:create", "nested": {"list": [1, 2.5, null, {"k": true}]}});
    let body = json!({
        "prompt": "summarize",
        "input": {"rows": [1, 2, 3]},
        "budget": {"max_tokens": 4000, "max_cost": 2500000},
        "deadline": deadline,
        "metadata": metadata,
    });
    let reply = create(&h, &alice(), body).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.json());
    assert_eq!(reply.headers["etag"], "\"1\"");
    let created = reply.json();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("inv_"));
    assert_eq!(
        reply.headers["location"],
        format!("/v1/invocations/{id}").as_str()
    );
    assert_eq!(created["object"], "invocation");
    assert_eq!(created["state"], "queued");
    assert_eq!(created["revision"], 1);
    assert_eq!(created["tenant_id"], "tenant-a");
    assert_eq!(created["project_id"], "project-a");
    assert_eq!(created["actor_id"], "alice");
    assert!(created.get("audit_events").is_none());
    let request = &created["request"];
    assert_eq!(request["prompt"], "summarize");
    assert_eq!(request["input"], json!({"rows": [1, 2, 3]}));
    assert_eq!(
        request["budget"],
        json!({"max_tokens": 4000, "max_cost": 2500000})
    );
    assert_eq!(request["deadline"], deadline);
    assert_eq!(request["metadata"], metadata);

    let read = call(
        &h.router,
        "GET",
        &format!("/v1/invocations/{id}"),
        Some(&alice()),
        &[],
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.headers["etag"], "\"1\"");
    assert_eq!(read.json(), created);

    // Same-scope actors can read.
    let bob = token("bob", "tenant-a", Some("project-a"), &[]);
    let read = call(
        &h.router,
        "GET",
        &format!("/v1/invocations/{id}"),
        Some(&bob),
        &[],
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
}

#[tokio::test]
async fn create_validation_rejects_without_persisting() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    h.state.user_agents.write().await.extend([
        json!({"id": "agent-a", "tenant_id": "tenant-a", "project_id": "project-a"}),
        json!({"id": "agent-b", "tenant_id": "tenant-b", "project_id": "project-a"}),
    ]);
    let sha = "a".repeat(64);
    let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    let mut many_keys = serde_json::Map::new();
    for i in 0..65 {
        many_keys.insert(format!("k{i}"), json!(i));
    }
    let cases: Vec<(&str, Value, StatusCode, &str)> = vec![
        (
            "tenant_id",
            json!({"prompt": "p", "tenant_id": "t"}),
            StatusCode::BAD_REQUEST,
            "field_not_allowed",
        ),
        (
            "project_id",
            json!({"prompt": "p", "project_id": "x"}),
            StatusCode::BAD_REQUEST,
            "field_not_allowed",
        ),
        (
            "actor_id",
            json!({"prompt": "p", "actor_id": "x"}),
            StatusCode::BAD_REQUEST,
            "field_not_allowed",
        ),
        (
            "state",
            json!({"prompt": "p", "state": "succeeded"}),
            StatusCode::BAD_REQUEST,
            "field_not_allowed",
        ),
        (
            "revision",
            json!({"prompt": "p", "revision": 9}),
            StatusCode::BAD_REQUEST,
            "field_not_allowed",
        ),
        (
            "unknown",
            json!({"prompt": "p", "topology": "swarm"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "non-object",
            json!(["prompt"]),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "no prompt",
            json!({"metadata": {}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "blank prompt",
            json!({"prompt": "  "}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "prompt type",
            json!({"prompt": 3}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "input+ref",
            json!({"input": {}, "input_ref": {"uri": "s3://b/k", "sha256": sha}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "input 8193",
            json!({"input": "a".repeat(8191)}),
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        ),
        (
            "ref schemeless",
            json!({"input_ref": {"uri": "inputs/1/revisions/2", "sha256": sha}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "ref no sha",
            json!({"input_ref": {"uri": "s3://b/k"}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "ref upper sha",
            json!({"input_ref": {"uri": "s3://b/k", "sha256": "A".repeat(64)}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "ref extra",
            json!({"input_ref": {"uri": "s3://b/k", "sha256": sha, "x": 1}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "ref unresolvable",
            json!({"input_ref": {"uri": "s3://b/k", "sha256": sha}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "input_ref_unresolvable",
        ),
        (
            "revision w/o agent",
            json!({"prompt": "p", "agent_revision": "r1"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "floating",
            json!({"prompt": "p", "agent_id": "agent-a", "agent_revision": "LATEST"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "floating *",
            json!({"prompt": "p", "agent_id": "agent-a", "agent_revision": "*"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "budget 0",
            json!({"prompt": "p", "budget": {"max_tokens": 0}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "budget neg",
            json!({"prompt": "p", "budget": {"max_cost": -1}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "budget float",
            json!({"prompt": "p", "budget": {"max_tool_calls": 1.5}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "budget unknown",
            json!({"prompt": "p", "budget": {"max_minutes": 1}}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "deadline past",
            json!({"prompt": "p", "deadline": past}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "deadline no offset",
            json!({"prompt": "p", "deadline": "2999-01-01T00:00:00"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "deadline junk",
            json!({"prompt": "p", "deadline": "tomorrow"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "metadata keys",
            json!({"prompt": "p", "metadata": many_keys}),
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        ),
        (
            "metadata size",
            json!({"prompt": "p", "metadata": {"k": "m".repeat(16_377)}}),
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        ),
        (
            "metadata type",
            json!({"prompt": "p", "metadata": [1]}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "body size",
            json!({"prompt": "p".repeat(64 * 1024)}),
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        ),
        (
            "agent unknown",
            json!({"prompt": "p", "agent_id": "agent-x"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_not_found",
        ),
        (
            "agent other scope",
            json!({"prompt": "p", "agent_id": "agent-b"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_not_found",
        ),
        (
            "agent revision",
            json!({"prompt": "p", "agent_id": "agent-a", "agent_revision": "r1"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_revision_unsupported",
        ),
    ];
    let mut agent_not_found_bodies = Vec::new();
    for (name, body, status, code) in cases {
        let reply = create(&h, &alice(), body).await;
        assert_eq!(reply.status, status, "{name}: {}", reply.json());
        assert_eq!(reply.code(), code, "{name}");
        if code == "agent_not_found" {
            agent_not_found_bodies.push(reply.bytes.clone());
        }
        assert_eq!(stored_count(&h).await, 0, "{name} persisted");
    }
    assert_eq!(agent_not_found_bodies[0], agent_not_found_bodies[1]);

    // A malformed Idempotency-Key is rejected before anything is stored
    // (full coverage in invocations_idempotency_tests.rs, #315).
    let reply = call(
        &h.router,
        "POST",
        "/v1/invocations",
        Some(&alice()),
        &[("idempotency-key", "has space")],
        Some(json!({"prompt": "p"}).to_string()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "invalid_idempotency_key");
    assert_eq!(stored_count(&h).await, 0);

    // Boundaries pass: input 8192 bytes, metadata 16 KiB / 64 keys, in-scope agent.
    create_ok(&h, &alice(), json!({"input": "a".repeat(8190)})).await;
    create_ok(
        &h,
        &alice(),
        json!({"prompt": "p", "metadata": {"k": "m".repeat(16_376)}}),
    )
    .await;
    let mut keys = serde_json::Map::new();
    for i in 0..64 {
        keys.insert(format!("k{i}"), json!(i));
    }
    create_ok(&h, &alice(), json!({"prompt": "p", "metadata": keys})).await;
    let with_agent = create_ok(&h, &alice(), json!({"prompt": "p", "agent_id": "agent-a"})).await;
    assert_eq!(with_agent["request"]["agent_id"], "agent-a");
    assert_eq!(stored_count(&h).await, 4);
}

#[tokio::test]
async fn foreign_and_unknown_ids_get_byte_identical_404() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let created = create_ok(
        &h,
        &alice(),
        json!({"prompt": "p", "metadata": {"canary": CANARY}}),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let other_tenant = token("alice", "tenant-b", Some("project-a"), &["DA"]);
    let other_project = token("alice", "tenant-a", Some("project-b"), &["DA"]);
    let unknown = "inv_ffffffffffffffffffffffffffffffff";
    for suffix in ["", "/cancel"] {
        let method = if suffix.is_empty() { "GET" } else { "POST" };
        let mut replies = Vec::new();
        for (auth, target) in [
            (&other_tenant, id),
            (&other_project, id),
            (&alice(), unknown),
        ] {
            replies.push(
                call(
                    &h.router,
                    method,
                    &format!("/v1/invocations/{target}{suffix}"),
                    Some(auth),
                    &[],
                    None,
                )
                .await,
            );
        }
        for reply in &replies {
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
            assert!(!String::from_utf8_lossy(&reply.bytes).contains(CANARY));
        }
        for pair in [(0, 1), (0, 2), (1, 2)] {
            let (a, b) = (&replies[pair.0], &replies[pair.1]);
            assert_eq!(a.status, b.status);
            assert_eq!(a.headers, b.headers, "{method} {pair:?}");
            assert_eq!(a.bytes, b.bytes, "{method} {pair:?}");
        }
    }
    // The cross-scope cancels changed nothing.
    let read = h
        .store
        .get_for_claims(&scope("tenant-a", "project-a", "alice"), id)
        .await;
    assert_eq!(read.unwrap().revision, 1);
    // The canary never shows up in another scope's list.
    for auth in [&other_tenant, &other_project] {
        let listed = call(&h.router, "GET", "/v1/invocations", Some(auth), &[], None).await;
        assert_eq!(listed.status, StatusCode::OK);
        assert!(!String::from_utf8_lossy(&listed.bytes).contains(CANARY));
        assert_eq!(listed.json()["data"], json!([]));
    }
}

fn scope(tenant: &str, project: &str, actor: &str) -> IsolationClaims {
    IsolationClaims::from_verified(tenant, project, actor).unwrap()
}

#[tokio::test]
async fn cancel_rules_owner_da_if_match_and_lifecycle() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let bob = token("bob", "tenant-a", Some("project-a"), &[]);
    let dana = token("dana", "tenant-a", Some("project-a"), &["DA"]);
    let cancel = |id: &str| format!("/v1/invocations/{id}/cancel");

    // Another actor in the scope can read but not cancel.
    let first = create_ok(&h, &alice(), json!({"prompt": "p"})).await;
    let first_id = first["id"].as_str().unwrap().to_string();
    let reply = call(&h.router, "POST", &cancel(&first_id), Some(&bob), &[], None).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "cancel_not_permitted");

    // Malformed and stale If-Match.
    for bad in ["W/\"1\"", "1", "\"1\", \"2\"", "\"x\""] {
        let reply = call(
            &h.router,
            "POST",
            &cancel(&first_id),
            Some(&alice()),
            &[("if-match", bad)],
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(reply.code(), "invalid_if_match");
    }
    let reply = call(
        &h.router,
        "POST",
        &cancel(&first_id),
        Some(&alice()),
        &[("if-match", "\"7\"")],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "revision_conflict");
    assert_eq!(reply.headers["etag"], "\"1\"");

    // Creator cancels a queued invocation: 200 cancelled.
    let reply = call(
        &h.router,
        "POST",
        &cancel(&first_id),
        Some(&alice()),
        &[("if-match", "\"1\"")],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["state"], "cancelled");
    assert_eq!(reply.json()["revision"], 2);
    assert_eq!(reply.headers["etag"], "\"2\"");
    // Repeat with a stale If-Match: same 200, revision unchanged.
    let reply = call(
        &h.router,
        "POST",
        &cancel(&first_id),
        Some(&alice()),
        &[("if-match", "\"1\"")],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["revision"], 2);

    // A DA in the scope may cancel another actor's running invocation: 202.
    let second = create_ok(&h, &alice(), json!({"prompt": "p"})).await;
    let second_id = second["id"].as_str().unwrap().to_string();
    let claims = scope("tenant-a", "project-a", "worker");
    h.store
        .transition_for_claims(
            &claims,
            &second_id,
            None,
            InvocationState::Running,
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    let reply = call(
        &h.router,
        "POST",
        &cancel(&second_id),
        Some(&dana),
        &[],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.json()["state"], "cancel_requested");
    let reply = call(
        &h.router,
        "POST",
        &cancel(&second_id),
        Some(&alice()),
        &[],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.json()["revision"], 3);

    // Cancelling a succeeded invocation is 409 illegal_transition.
    let third = create_ok(&h, &alice(), json!({"prompt": "p"})).await;
    let third_id = third["id"].as_str().unwrap().to_string();
    for next in [InvocationState::Running, InvocationState::Succeeded] {
        h.store
            .transition_for_claims(&claims, &third_id, None, next, TransitionPatch::default())
            .await
            .unwrap();
    }
    let reply = call(
        &h.router,
        "POST",
        &cancel(&third_id),
        Some(&alice()),
        &[],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "illegal_transition");
}

#[tokio::test]
async fn create_over_active_limit_is_429_with_retry_after() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness_with(
        true,
        InvocationStoreConfig {
            max_active_per_scope: 2,
            ..InvocationStoreConfig::default()
        },
    );
    create_ok(&h, &alice(), json!({"prompt": "p"})).await;
    create_ok(&h, &alice(), json!({"prompt": "p"})).await;
    let reply = create(&h, &alice(), json!({"prompt": "p"})).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.code(), "too_many_active");
    assert_eq!(reply.headers["retry-after"], "5");
    assert_eq!(stored_count(&h).await, 2);
    // Another scope has its own limit.
    let other = token("alice", "tenant-b", Some("project-a"), &[]);
    create_ok(&h, &other, json!({"prompt": "p"})).await;
}

#[tokio::test]
async fn list_is_scoped_and_paginates_without_gaps_or_duplicates() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = env(true);
    let h = harness();
    let tenant_b = token("bob", "tenant-b", Some("project-a"), &[]);
    let mut a_ids = Vec::new();
    for _ in 0..5 {
        let created = create_ok(&h, &alice(), json!({"prompt": "p"})).await;
        a_ids.push(created["id"].as_str().unwrap().to_string());
    }
    let mut b_ids = Vec::new();
    for _ in 0..3 {
        let created = create_ok(&h, &tenant_b, json!({"prompt": "p"})).await;
        b_ids.push(created["id"].as_str().unwrap().to_string());
    }

    let mut seen = Vec::new();
    let mut uri = "/v1/invocations?limit=2".to_string();
    let mut pages = 0;
    loop {
        let page = call(&h.router, "GET", &uri, Some(&alice()), &[], None).await;
        assert_eq!(page.status, StatusCode::OK);
        let body = page.json();
        pages += 1;
        for item in body["data"].as_array().unwrap() {
            assert_eq!(item["tenant_id"], "tenant-a");
            assert!(item.get("audit_events").is_none());
            seen.push(item["id"].as_str().unwrap().to_string());
        }
        match body["next_cursor"].as_str() {
            Some(cursor) => {
                assert_eq!(body["has_more"], true);
                uri = format!("/v1/invocations?limit=2&after={cursor}");
            }
            None => {
                assert_eq!(body["has_more"], false);
                break;
            }
        }
    }
    assert_eq!(pages, 3);
    let mut sorted_seen = seen.clone();
    sorted_seen.sort();
    sorted_seen.dedup();
    assert_eq!(sorted_seen.len(), 5);
    let mut expected = a_ids.clone();
    expected.sort();
    assert_eq!(sorted_seen, expected);
    assert!(seen.iter().all(|id| !b_ids.contains(id)));

    let b_list = call(
        &h.router,
        "GET",
        "/v1/invocations",
        Some(&tenant_b),
        &[],
        None,
    )
    .await;
    assert_eq!(b_list.json()["data"].as_array().unwrap().len(), 3);

    // State filter.
    let claims = scope("tenant-a", "project-a", "alice");
    h.store
        .transition_for_claims(
            &claims,
            &a_ids[0],
            None,
            InvocationState::Cancelled,
            TransitionPatch::default(),
        )
        .await
        .unwrap();
    let filtered = call(
        &h.router,
        "GET",
        "/v1/invocations?state=cancelled",
        Some(&alice()),
        &[],
        None,
    )
    .await;
    let data = filtered.json()["data"].as_array().unwrap().clone();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["id"], a_ids[0].as_str());

    for bad in [
        "limit=0",
        "limit=101",
        "limit=x",
        "state=done",
        "after=%%%",
        "after=bm90LWpzb24",
    ] {
        let reply = call(
            &h.router,
            "GET",
            &format!("/v1/invocations?{bad}"),
            Some(&alice()),
            &[],
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(reply.code(), "invalid_request", "{bad}");
    }
}
