//! #299: a saved provider credential is only ever sent to the endpoint it was
//! saved with. Caller-supplied endpoints never receive it, on any route that
//! can fall back to a saved key.
//!
//! #303: the probe routes are platform-admin only, and moving the gateway
//! `base_url` never carries the configured gateway key along.
//! #267: probe targets pass the provider outbound guard (allowlist / public
//! only, no metadata, pinned DNS, no redirects) before any connection.

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
    provider_outbound::PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV,
    TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
/// Fake saved credential; must never reach a foreign endpoint.
const SAVED_KEY: &str = "test-only-saved-provider-key";
/// Fake caller-supplied credential.
const CALLER_KEY: &str = "test-only-caller-supplied-key";

const ADMIN_TENANT: &str = "test-platform-tenant";

fn setup(dir: &Path) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(SECRET.to_vec()).unwrap(),
        ),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
        ("AGENTOS_PLATFORM_ADMIN_TENANT", ADMIN_TENANT.into()),
        ("AGENTOS_AUTH_STRICT", "false".into()),
        (PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV, String::new()),
    ])
}

/// Allow exactly these origins (the loopback mocks) for provider probes.
fn allow_origins(origins: &[&str]) -> EnvGuard {
    EnvGuard::set(&[(PROVIDER_OUTBOUND_ALLOWED_ORIGINS_ENV, origins.join(","))])
}

fn token(sub: &str, tenant: &str, roles: &[&str]) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: sub.into(),
            tenant_id: tenant.into(),
            project_id: Some("test-project".into()),
            roles: roles.iter().map(|role| role.to_string()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

fn admin_token() -> String {
    token("test-admin-user", ADMIN_TENANT, &["PLATFORM_ADMIN"])
}

fn tenant_da_token() -> String {
    token("test-da-user", "tenant-a", &["DA"])
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
    send(router, Method::POST, uri, body, Some(&admin_token())).await
}

async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
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
        "test-admin-user",
        ADMIN_TENANT,
        "test-project",
        "PLATFORM_ADMIN",
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
    let _allow = allow_origins(&[&saved.base, &foreign.base]);
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
    let _allow = allow_origins(&[&saved.base]);
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
    let _allow = allow_origins(&[&saved.base, &foreign.base]);
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
    let _allow = allow_origins(&["http://127.0.0.1:9"]);
    let router = app(test_state(dir.path()));

    // #267: tightened. URL credentials are refused before any connection
    // (previously a connection was attempted and reported as ok:false).
    let (status, text) = post_json(
        &router,
        "/api/v1/providers/models",
        json!({ "base_url": "http://user:pw@127.0.0.1:9", "api_key": CALLER_KEY }),
    )
    .await;
    assert_outbound_refused(status, &text);

    // Port 9 (discard) on loopback: connection refused, never a real upstream.
    let (status, text) = post_json(
        &router,
        "/api/v1/providers/models",
        json!({ "base_url": "http://127.0.0.1:9", "api_key": CALLER_KEY }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["ok"], json!(false));
    for leaked in ["user:pw", "127.0.0.1", CALLER_KEY] {
        assert!(!text.contains(leaked), "error body leaked {leaked:?}");
    }
}

fn assert_outbound_refused(status: StatusCode, body: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({
            "error": "provider_outbound_not_allowed",
            "message": "provider endpoint is not an allowed outbound destination",
        })
    );
    for leaked in [
        SAVED_KEY,
        CALLER_KEY,
        "127.0.0.1",
        "169.254",
        "user:pw",
        "provider-a",
    ] {
        assert!(!body.contains(leaked), "refusal body leaked {leaked:?}");
    }
}

#[tokio::test]
async fn isolation_contract_provider_probes_require_platform_admin() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let saved = Upstream::start().await;
    let _allow = allow_origins(&[&saved.base]);
    write_saved_provider(dir.path(), &saved.base);
    let router = app(test_state(dir.path()));

    let probes = [
        (
            "/api/v1/providers/models",
            json!({ "base_url": saved.base, "api_key": CALLER_KEY }),
        ),
        (
            "/api/v1/providers/models",
            json!({ "provider_id": "provider-a" }),
        ),
        ("/api/v1/models/test", json!({ "resource_id": "chat-a" })),
    ];
    let tenant_da = tenant_da_token();
    for (uri, body) in probes {
        let (status, _) = send(&router, Method::POST, uri, body.clone(), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} anonymous");
        let (status, text) = send(&router, Method::POST, uri, body.clone(), Some(&tenant_da)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} tenant DA: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["error"],
            json!("platform_admin_required")
        );
        assert!(!text.contains(SAVED_KEY) && !text.contains(CALLER_KEY));
    }
    assert!(
        saved.seen().is_empty(),
        "a rejected caller reached the upstream"
    );
}

#[tokio::test]
async fn isolation_contract_provider_probes_refuse_internal_targets_before_connecting() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let internal = Upstream::start().await;
    let other = Upstream::start().await;
    write_saved_provider(dir.path(), &internal.base);
    let router = app(test_state(dir.path()));

    let internal_port = internal.base.rsplit(':').next().unwrap().to_owned();
    let targets = [
        internal.base.clone(),
        format!("http://localhost:{internal_port}"),
        format!("http://[::ffff:127.0.0.1]:{internal_port}"),
        "http://169.254.169.254".to_owned(),
        "http://[fd00:ec2::254]".to_owned(),
        "http://0.0.0.0:9".to_owned(),
    ];
    // No allowlist: only public addresses.
    for base_url in &targets {
        let (status, text) = post_json(
            &router,
            "/api/v1/providers/models",
            json!({ "base_url": base_url, "api_key": CALLER_KEY }),
        )
        .await;
        assert_outbound_refused(status, &text);
    }
    // models/test vets the saved endpoint too.
    let (status, text) = post_json(
        &router,
        "/api/v1/models/test",
        json!({ "resource_id": "chat-a" }),
    )
    .await;
    assert_outbound_refused(status, &text);

    // An allowlist that names another origin does not admit this one, and a
    // listed metadata address stays refused.
    let _allow = allow_origins(&[&other.base, "http://169.254.169.254"]);
    for base_url in [internal.base.as_str(), "http://169.254.169.254"] {
        let (status, text) = post_json(
            &router,
            "/api/v1/providers/models",
            json!({ "base_url": base_url, "api_key": CALLER_KEY }),
        )
        .await;
        assert_outbound_refused(status, &text);
    }
    assert!(internal.seen().is_empty(), "refused target was contacted");
    assert!(other.seen().is_empty());
}

#[tokio::test]
async fn isolation_contract_provider_probes_do_not_follow_redirects() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let target = Upstream::start().await;
    let location = format!("{}/v1/models", target.base);
    let redirector = Router::new().route(
        "/v1/models",
        get(move || {
            let location = location.clone();
            async move {
                (
                    StatusCode::FOUND,
                    [(axum::http::header::LOCATION, location)],
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirector_base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, redirector).await.unwrap() });
    // Both origins are allowed; the redirect itself must still not be followed.
    let _allow = allow_origins(&[&redirector_base, &target.base]);
    let router = app(test_state(dir.path()));

    let (status, text) = post_json(
        &router,
        "/api/v1/providers/models",
        json!({ "base_url": redirector_base, "api_key": CALLER_KEY }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["http_status"], json!(302));
    assert!(target.seen().is_empty(), "redirect was followed");
}

#[tokio::test]
async fn isolation_contract_gateway_key_does_not_follow_new_base_url() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let saved = Upstream::start().await;
    let foreign = Upstream::start().await;
    let state = test_state(dir.path());
    state.gateway.set_base_url(saved.base.clone());
    state.gateway.set_api_key(SAVED_KEY.to_owned());
    let router = app(state.clone());
    let admin = admin_token();

    // Moving base_url without an explicit key: refused before save or apply.
    for patch in [
        json!({ "gateway": { "base_url": foreign.base } }),
        json!({ "gateway": { "base_url": format!("{}/v1/", foreign.base), "api_key": "" } }),
        json!({ "gateway": { "base_url": format!("{}/proxy", saved.base), "api_key": "  " } }),
    ] {
        let (status, text) =
            send(&router, Method::PUT, "/api/v1/config", patch, Some(&admin)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["error"],
            json!("explicit_api_key_required")
        );
        assert!(!text.contains(SAVED_KEY) && !text.contains(&foreign.base));
        assert_eq!(state.gateway.base_url(), saved.base);
        assert!(
            !dir.path().join("config_override.json").exists(),
            "a refused patch was persisted"
        );
    }
    let _ = state.gateway.health_check().await;
    assert!(foreign.seen().is_empty(), "foreign endpoint was contacted");
    assert_eq!(saved.seen(), vec![Some(format!("Bearer {SAVED_KEY}"))]);

    // Same endpoint (equivalent spelling) keeps the configured key.
    let (status, text) = send(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({ "gateway": { "base_url": format!("{}/v1/", saved.base) } }),
        Some(&admin),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");

    // A new endpoint with its own key: only that key reaches it.
    let (status, text) = send(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({ "gateway": { "base_url": foreign.base, "api_key": CALLER_KEY } }),
        Some(&admin),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    assert!(!text.contains(CALLER_KEY));
    let _ = state.gateway.health_check().await;
    assert_eq!(foreign.seen(), vec![Some(format!("Bearer {CALLER_KEY}"))]);
    assert!(foreign.never_saw_saved_key());
}

#[tokio::test]
async fn isolation_contract_models_test_only_contacts_saved_endpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let saved = Upstream::start().await;
    let foreign = Upstream::start().await;
    let _allow = allow_origins(&[&saved.base, &foreign.base]);
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

/// Loads configuration the way a restart does: `config.yaml` in `dir`, the
/// runtime override written by the API, and the given environment.
fn reload_after_restart(dir: &Path, env: &[(&str, &str)]) -> ::config::Config {
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    crate::config::settings::load_config_layers_for_test(
        dir.join("config").to_str().unwrap(),
        &dir.join("config_override.json"),
        &env,
    )
    .unwrap()
}

/// #303 review: a base URL persisted at runtime must not pick up the
/// deployment gateway key after a restart.
#[tokio::test]
async fn isolation_contract_gateway_deployment_key_does_not_follow_persisted_base_url() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    std::fs::write(
        dir.path().join("config.yaml"),
        "gateway:\n  base_url: https://deploy.invalid/v1\n  api_key: test-only-yaml-deploy-key\n",
    )
    .unwrap();
    let router = app(test_state(dir.path()));

    // A dummy key passes the runtime check and the base URL is persisted.
    let (status, text) = send(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({ "gateway": { "base_url": "https://attacker.invalid", "api_key": "dummy" } }),
        Some(&admin_token()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    assert!(read_override(dir.path()).contains("https://attacker.invalid"));
    assert!(!read_override(dir.path()).contains("dummy"));

    for env in [
        &[][..],
        &[("AGENT_OS_GATEWAY_API_KEY", "test-only-env-deploy-key")][..],
    ] {
        let config = reload_after_restart(dir.path(), env);
        assert_eq!(
            config.get_string("gateway.base_url").unwrap(),
            "https://attacker.invalid"
        );
        assert_eq!(config.get_string("gateway.api_key").unwrap(), "");
    }
}

/// Same rule for the oneapi embedding endpoint and its deployment key.
#[test]
fn isolation_contract_embedding_deployment_key_does_not_follow_persisted_base_url() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    std::fs::write(
        dir.path().join("config.yaml"),
        "embedding:\n  oneapi:\n    base_url: https://deploy-emb.invalid/v1\n",
    )
    .unwrap();
    save_config_override(&json!({ "embedding": { "oneapi": {
        "base_url": "https://attacker.invalid/v1"
    } } }))
    .unwrap();

    let config = reload_after_restart(
        dir.path(),
        &[("AGENT_OS_EMBEDDING_ONEAPI_API_KEY", "test-only-env-emb-key")],
    );
    assert_eq!(
        config.get_string("embedding.oneapi.base_url").unwrap(),
        "https://attacker.invalid/v1"
    );
    assert_eq!(config.get_string("embedding.oneapi.api_key").unwrap(), "");
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

/// Mock upstream that counts every request it receives, on any path.
async fn counting_upstream() -> (String, Arc<Mutex<usize>>) {
    let hits: Arc<Mutex<usize>> = Arc::default();
    let counter = hits.clone();
    let mock = Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            *counter.lock().unwrap() += 1;
            Json(json!({ "data": [{ "embedding": [0.0] }] }))
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    (format!("http://{address}/v1"), hits)
}

/// Differently cased spellings of `embedding.oneapi.base_url`. The
/// configuration loader lowercases keys, so each of these would move the
/// endpoint if it reached `config_override.json`.
fn cased_embedding_endpoint_patches(base: &str) -> Vec<Value> {
    vec![
        json!({ "embedding": { "provider": "oneapi", "oneapi": { "BASE_URL": base } } }),
        json!({ "embedding": { "provider": "oneapi", "OneApi": { "Base_Url": base } } }),
    ]
}

/// Cased spellings in the other embedding sub-tables (ollama, fallback).
fn cased_embedding_other_patches(base: &str) -> Vec<Value> {
    vec![
        json!({ "embedding": { "Ollama": { "base_url": base } } }),
        json!({ "embedding": { "ollama": { "Base_Url": base } } }),
        json!({ "embedding": { "Fallback": { "dimension": 8 } } }),
        json!({ "embedding": { "fallback": { "DIMENSION": 8 } } }),
    ]
}

/// #303 review (BLOCKER on #352): `PUT /api/v1/config` must reject a cased
/// spelling of the embedding endpoint with 422 before anything is saved or
/// hot-reloaded, so the deployment embedding key never reaches that endpoint.
#[tokio::test]
async fn isolation_contract_put_config_rejects_cased_embedding_endpoint_keys() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let _key = EnvGuard::set(&[(
        "AGENT_OS_EMBEDDING_ONEAPI_API_KEY",
        "test-only-env-emb-key".into(),
    )]);
    let (attacker, hits) = counting_upstream().await;
    let router = app(test_state(dir.path()));

    let mut patches = cased_embedding_endpoint_patches(&attacker);
    patches.extend(cased_embedding_other_patches(&attacker));
    for patch in patches {
        let before =
            std::fs::read_to_string(dir.path().join("config_override.json")).unwrap_or_default();
        let (status, text) = send(
            &router,
            Method::PUT,
            "/api/v1/config",
            patch.clone(),
            Some(&admin_token()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{patch}: {text}");
        assert!(!text.contains("test-only-env-emb-key"));
        let saved =
            std::fs::read_to_string(dir.path().join("config_override.json")).unwrap_or_default();
        assert!(!saved.contains(&attacker), "{patch} was persisted: {saved}");
        assert_eq!(saved, before, "{patch} changed the override file");
    }
    // Give any spawned reload/reindex a chance to run before counting.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(*hits.lock().unwrap(), 0, "attacker endpoint was contacted");

    // The canonical spelling the Admin UI sends is still accepted.
    let (status, text) = send(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({ "embedding": {
            "enabled": false, "provider": "fallback", "active_dimension": 128,
            "ollama": { "base_url": "http://localhost:11434", "model": "m", "dimension": 768 },
            "oneapi": { "base_url": "", "model": "m", "dimension": 1536, "api_key_configured": true },
            "fallback": { "dimension": 128 }
        } }),
        Some(&admin_token()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    let saved = read_override(dir.path());
    assert!(!saved.contains("active_dimension") && !saved.contains("api_key_configured"));
}

/// #303 review (BLOCKER on #352): an override that already holds a cased
/// spelling (e.g. written before the PUT schema was typed) must not pair the
/// deployment embedding key with that endpoint, after a restart or on the
/// hot reload path (`Settings::load_embedding` / `Settings::load`).
#[test]
fn isolation_contract_cased_override_embedding_endpoint_drops_deployment_key() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let _key = EnvGuard::set(&[(
        "AGENT_OS_EMBEDDING_ONEAPI_API_KEY",
        "test-only-env-emb-key".into(),
    )]);
    std::fs::write(
        dir.path().join("config.yaml"),
        "embedding:\n  oneapi:\n    base_url: https://deploy-emb.invalid/v1\n    api_key: test-only-yaml-emb-key\n",
    )
    .unwrap();
    let attacker = "https://attacker.invalid/v1";
    for patch in cased_embedding_endpoint_patches(attacker) {
        std::fs::write(dir.path().join("config_override.json"), patch.to_string()).unwrap();
        for env in [
            &[][..],
            &[("AGENT_OS_EMBEDDING_ONEAPI_API_KEY", "test-only-env-emb-key")][..],
        ] {
            let config = reload_after_restart(dir.path(), env);
            assert_eq!(
                config.get_string("embedding.oneapi.base_url").unwrap(),
                attacker,
                "{patch}"
            );
            assert_eq!(
                config.get_string("embedding.oneapi.api_key").unwrap(),
                "",
                "{patch}: deployment key followed the override endpoint"
            );
        }
        // Hot reload reads the process configuration the same way.
        let live = crate::config::settings::Settings::load_embedding();
        assert_eq!(live.oneapi.base_url, attacker, "{patch}");
        assert_eq!(live.oneapi.api_key, "", "{patch}");
    }
}

/// #303 re-review: concurrent embedding hot reloads (two PUTs touching
/// `embedding` at once) are serialized. Each reload reads the configuration
/// once, rotates the previous vector store to its own backup directory and
/// swaps in a new one; no two reload bodies ever run at the same time, and
/// back-to-back reloads within the same second do not collide.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolation_contract_concurrent_embedding_hot_reloads_are_serialized() {
    use std::sync::atomic::Ordering;

    use super::config::{embedding_reload_probe, hot_reload_embedding};

    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    std::fs::write(
        dir.path().join("config_override.json"),
        json!({ "embedding": { "enabled": false, "fallback": { "dimension": 8 } } }).to_string(),
    )
    .unwrap();
    let state = test_state(dir.path());
    embedding_reload_probe::MAX_IN_FLIGHT.store(0, Ordering::SeqCst);

    const RELOADS: usize = 8;
    let tasks: Vec<_> = (0..RELOADS)
        .map(|_| {
            let state = state.clone();
            tokio::spawn(async move { hot_reload_embedding(&state).await })
        })
        .collect();
    for task in tasks {
        let (_, new_dim, _, _) = task.await.unwrap().expect("hot reload failed");
        assert_eq!(new_dim, 8);
    }

    assert_eq!(
        embedding_reload_probe::MAX_IN_FLIGHT.load(Ordering::SeqCst),
        1,
        "embedding hot reloads overlapped"
    );
    assert_eq!(state.vector_store.load_full().unwrap().dimension(), 8);
    let mut stores = 0;
    let mut backups = 0;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name == "vector_store" {
            stores += 1;
        } else if name.starts_with("vector_store.bak-") {
            backups += 1;
        }
    }
    assert_eq!(stores, 1);
    // The first reload had no previous store to rotate.
    assert_eq!(backups, RELOADS - 1);
}

/// #303 re-review: a failed `PUT /api/v1/config` save answers 500 with a
/// fixed message and the I/O error kind only. The underlying error (the
/// temporary file error names the absolute data directory and the temporary
/// file) is logged on the server, never returned.
#[cfg(unix)]
#[tokio::test]
async fn isolation_contract_config_persist_failure_does_not_leak_paths() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = app(test_state(dir.path()));

    // A data directory the server cannot write to.
    let read_only = dir.path().join("read-only-data");
    std::fs::create_dir(&read_only).unwrap();
    std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o555)).unwrap();
    struct RestoreMode<'a>(&'a Path);
    impl Drop for RestoreMode<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = RestoreMode(&read_only);
    if std::fs::write(read_only.join("probe"), b"").is_ok() {
        // Privileged user: permissions are not enforced, nothing to observe.
        eprintln!("skipping: running with permission override");
        return;
    }
    let _data_dir =
        EnvGuard::set(&[("AGENTOS_DATA_DIR", read_only.to_string_lossy().into_owned())]);

    let (status, text) = send(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({ "gateway": { "default_model": "persist-failure-model" } }),
        Some(&admin_token()),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "body: {text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["persisted"], json!(false));
    assert_eq!(
        body["message"],
        json!(format!(
            "配置持久化失败：{}",
            std::io::ErrorKind::PermissionDenied
        ))
    );
    for leaked in [
        read_only.to_string_lossy().as_ref(),
        dir.path().to_string_lossy().as_ref(),
        "read-only-data",
        "config_override",
        ".tmp",
        "os error",
    ] {
        assert!(!text.contains(leaked), "500 body leaks {leaked:?}: {text}");
    }
    assert!(!read_only.join("config_override.json").exists());
}

/// #400: an embedding hot-reload failure is copied into the PUT config body.
/// The body carries a fixed phrase plus the I/O kind (or `open_failed`),
/// never the data-directory path, `/tmp`, or `os error`.
#[cfg(unix)]
#[tokio::test]
async fn isolation_contract_embedding_reload_failure_does_not_leak_paths() {
    use std::os::unix::fs::symlink;

    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = app(test_state(dir.path()));
    // A broken symlink: `create_dir_all` fails with an error whose Display
    // includes `os error` and, on some platforms, the absolute path.
    symlink(
        "/no/such/vector-store-target",
        dir.path().join("vector_store"),
    )
    .unwrap();

    let (status, text) = send(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({ "embedding": { "enabled": false, "fallback": { "dimension": 8 } } }),
        Some(&admin_token()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["persisted"], json!(true));
    assert_eq!(body["embedding_reloaded"], json!(false));
    assert_eq!(
        body["message"],
        json!(format!(
            "配置已持久化，但向量库热切换失败：{}（重启后仍会按新配置生效）",
            std::io::ErrorKind::AlreadyExists
        ))
    );
    for leaked in [
        dir.path().to_string_lossy().as_ref(),
        "/tmp",
        "os error",
        "vector_store",
        "no/such",
    ] {
        assert!(
            !text.contains(leaked),
            "reload body leaks {leaked:?}: {text}"
        );
    }
}
