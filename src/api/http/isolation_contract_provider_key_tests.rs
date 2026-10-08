//! #299: a saved provider credential is only ever sent to the endpoint it was
//! saved with. Caller-supplied endpoints never receive it, on any route that
//! can fall back to a saved key.

// Test-only lock held for the whole test by design (serializes process-global env/state);
// code under test never takes it, so holding it across `.await` cannot deadlock.
#![allow(clippy::await_holding_lock)]

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Method, Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower::ServiceExt;

use super::{
    config::save_config_override,
    control_plane_route_auth_tests::{app, test_state, EnvGuard},
    iam::JwtClaims,
    TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
/// Fake saved credential; must never reach a foreign endpoint.
const SAVED_KEY: &str = "test-only-saved-provider-key";
/// Fake caller-supplied credential.
const CALLER_KEY: &str = "test-only-caller-supplied-key";

fn setup(dir: &Path) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(SECRET.to_vec()).unwrap(),
        ),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
    ])
}

fn da_token() -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: "test-da-user".into(),
            tenant_id: "tenant-a".into(),
            project_id: Some("project-a".into()),
            roles: vec!["DA".into()],
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

/// Mock OpenAI-compatible upstream that records every `Authorization` header.
struct Upstream {
    base: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl Upstream {
    async fn start() -> Self {
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let record = |seen: Arc<Mutex<Vec<Option<String>>>>, headers: HeaderMap| {
            seen.lock().unwrap().push(
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            );
        };
        let (s1, s2) = (seen.clone(), seen.clone());
        let mock = Router::new()
            .route(
                "/v1/models",
                get(move |headers: HeaderMap| {
                    let seen = s1.clone();
                    async move {
                        record(seen, headers);
                        Json(json!({"data": [{"id": "mock-model", "owned_by": "test"}]}))
                    }
                }),
            )
            .route(
                "/v1/chat/completions",
                post(move |headers: HeaderMap| {
                    let seen = s2.clone();
                    async move {
                        record(seen, headers);
                        Json(json!({"choices": []}))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        Self {
            base: format!("http://{address}"),
            seen,
        }
    }

    fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }

    fn never_saw_saved_key(&self) -> bool {
        self.seen()
            .iter()
            .flatten()
            .all(|auth| !auth.contains(SAVED_KEY))
    }
}

fn write_saved_provider(dir: &Path, base_url: &str) {
    std::fs::write(
        dir.join("config_override.json"),
        json!({
            "models": {
                "providers": [{
                    "id": "provider-a",
                    "base_url": base_url,
                    "api_key": SAVED_KEY,
                    "timeout_seconds": 3
                }],
                "resources": [{
                    "id": "chat-a",
                    "provider_id": "provider-a",
                    "model": "chat-test",
                    "modalities": ["chat"]
                }]
            }
        })
        .to_string(),
    )
    .unwrap();
}

async fn post_json(router: &Router, uri: &str, body: Value) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", da_token()))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn expected_rejection() -> Value {
    json!({
        "error": "explicit_api_key_required",
        "message": "base_url differs from the saved provider endpoint; provide api_key explicitly",
    })
}

fn assert_generic_rejection(status: StatusCode, body: &str, foreign: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        expected_rejection()
    );
    for leaked in [
        SAVED_KEY,
        foreign,
        "127.0.0.1",
        "provider-a",
        "test-da-user",
        "tenant-a",
        "project-a",
        "\"DA\"",
        "user:pw",
    ] {
        assert!(!body.contains(leaked), "rejection body leaked {leaked:?}");
    }
}

#[tokio::test]
async fn isolation_contract_provider_models_never_sends_saved_key_to_foreign_base_url() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let saved = Upstream::start().await;
    let foreign = Upstream::start().await;
    write_saved_provider(dir.path(), &saved.base);
    let router = app(test_state(dir.path()));

    let foreign_host = foreign.base.trim_start_matches("http://").to_string();
    let attempts = [
        foreign.base.clone(),
        format!("{}/v1/", foreign.base),
        format!("http://user:pw@{foreign_host}"),
        // Same host, different path: still a different endpoint.
        format!("{}/proxy", saved.base),
    ];
    for base_url in attempts {
        for api_key in [json!(""), Value::Null] {
            let mut body = json!({ "provider_id": "provider-a", "base_url": base_url });
            if !api_key.is_null() {
                body["api_key"] = api_key;
            }
            let (status, text) = post_json(&router, "/api/v1/providers/models", body).await;
            assert_generic_rejection(status, &text, &base_url);
        }
    }
    assert!(foreign.seen().is_empty(), "foreign upstream was contacted");
    assert!(
        saved.seen().is_empty(),
        "rejected request reached an upstream"
    );
}

#[tokio::test]
async fn isolation_contract_provider_models_same_endpoint_reuses_saved_key() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let saved = Upstream::start().await;
    write_saved_provider(dir.path(), &saved.base);
    let router = app(test_state(dir.path()));

    // Saved endpoint, by omission and by an equivalent spelling (trailing /v1/).
    for body in [
        json!({ "provider_id": "provider-a" }),
        json!({ "provider_id": "provider-a", "base_url": format!("  {}/v1/ ", saved.base), "api_key": "" }),
    ] {
        let (status, text) = post_json(&router, "/api/v1/providers/models", body).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["models"][0]["id"], json!("mock-model"));
        assert!(!text.contains(SAVED_KEY));
    }
    assert_eq!(
        saved.seen(),
        vec![Some(format!("Bearer {SAVED_KEY}")); 2],
        "same-endpoint path must keep using the saved key"
    );
}

#[tokio::test]
async fn isolation_contract_provider_models_explicit_key_to_foreign_base_url_uses_only_that_key() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let saved = Upstream::start().await;
    let foreign = Upstream::start().await;
    write_saved_provider(dir.path(), &saved.base);
    let router = app(test_state(dir.path()));

    let (status, text) = post_json(
        &router,
        "/api/v1/providers/models",
        json!({ "provider_id": "provider-a", "base_url": foreign.base, "api_key": CALLER_KEY }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    assert!(!text.contains(CALLER_KEY) && !text.contains(SAVED_KEY));
    assert_eq!(foreign.seen(), vec![Some(format!("Bearer {CALLER_KEY}"))]);
    assert!(foreign.never_saw_saved_key());
    assert!(saved.seen().is_empty());
}

#[tokio::test]
async fn isolation_contract_provider_models_unreachable_error_does_not_echo_url() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = app(test_state(dir.path()));

    // Port 9 (discard) on loopback: connection refused, never a real upstream.
    let (status, text) = post_json(
        &router,
        "/api/v1/providers/models",
        json!({ "base_url": "http://user:pw@127.0.0.1:9", "api_key": CALLER_KEY }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["ok"], json!(false));
    for leaked in ["user:pw", "127.0.0.1", CALLER_KEY] {
        assert!(!text.contains(leaked), "error body leaked {leaked:?}");
    }
}

#[tokio::test]
async fn isolation_contract_models_test_only_contacts_saved_endpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let saved = Upstream::start().await;
    let foreign = Upstream::start().await;
    write_saved_provider(dir.path(), &saved.base);
    let router = app(test_state(dir.path()));

    // `/api/v1/models/test` takes no endpoint input; extra fields are ignored.
    let (status, text) = post_json(
        &router,
        "/api/v1/models/test",
        json!({ "resource_id": "chat-a", "base_url": foreign.base, "api_key": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    assert!(!text.contains(SAVED_KEY));
    assert!(foreign.seen().is_empty(), "foreign upstream was contacted");
    assert_eq!(saved.seen(), vec![Some(format!("Bearer {SAVED_KEY}"))]);
}

fn read_override(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("config_override.json")).unwrap()
}

#[test]
fn isolation_contract_saved_provider_key_is_dropped_when_endpoint_changes() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let provider = |base: &str| json!({ "models": { "providers": [{ "id": "provider-a", "base_url": base, "api_key": "" }], "resources": [] } });

    // Same endpoint (equivalent spelling): the saved key is kept, as before.
    write_saved_provider(dir.path(), "http://saved.invalid");
    save_config_override(&provider("http://saved.invalid/v1/")).unwrap();
    let kept: Value = serde_json::from_str(&read_override(dir.path())).unwrap();
    assert_eq!(kept["models"]["providers"][0]["api_key"], json!(SAVED_KEY));

    // Endpoint moved without a new key: the saved key must not follow it.
    for moved in ["http://foreign.invalid", "http://saved.invalid/proxy", ""] {
        write_saved_provider(dir.path(), "http://saved.invalid");
        save_config_override(&provider(moved)).unwrap();
        let text = read_override(dir.path());
        assert!(!text.contains(SAVED_KEY), "saved key followed {moved:?}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v["models"]["providers"][0].get("api_key").is_none());
    }
}

#[test]
fn isolation_contract_saved_embedding_key_is_dropped_when_endpoint_changes() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let write_saved = || {
        std::fs::write(
            dir.path().join("config_override.json"),
            json!({ "embedding": { "provider": "oneapi", "oneapi": {
                "base_url": "http://saved.invalid/v1", "api_key": SAVED_KEY, "model": "m"
            } } })
            .to_string(),
        )
        .unwrap();
    };
    let key_after = |patch: Value| -> Value {
        save_config_override(&json!({ "embedding": { "oneapi": patch } })).unwrap();
        let v: Value = serde_json::from_str(&read_override(dir.path())).unwrap();
        v["embedding"]["oneapi"]["api_key"].clone()
    };

    // Unrelated field, or an equivalent spelling of the same endpoint: kept.
    write_saved();
    assert_eq!(key_after(json!({ "model": "m2" })), json!(SAVED_KEY));
    write_saved();
    assert_eq!(
        key_after(json!({ "base_url": "http://saved.invalid/", "api_key": "" })),
        json!(SAVED_KEY)
    );
    // Endpoint moved without a new key: dropped.
    for moved in [
        json!({ "base_url": "http://foreign.invalid/v1" }),
        json!({ "base_url": "http://foreign.invalid/v1", "api_key": "" }),
    ] {
        write_saved();
        assert_eq!(key_after(moved), Value::Null);
        assert!(!read_override(dir.path()).contains(SAVED_KEY));
    }
    // Endpoint moved with an explicit new key: the new key is stored.
    write_saved();
    assert_eq!(
        key_after(json!({ "base_url": "http://foreign.invalid/v1", "api_key": CALLER_KEY })),
        json!(CALLER_KEY)
    );
}
