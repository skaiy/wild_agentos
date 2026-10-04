//! Tenant isolation contract for inbound API credentials and global configuration.

use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{Method, Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{
    api_gov::{self, ApiClient, ApiKey},
    control_plane_route_auth_tests::{app, test_state, EnvGuard},
    iam::JwtClaims,
    AppState, TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";

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

fn jwt_for(tenant: &str, roles: &[&str], project: Option<&str>) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: "test-da".into(),
            tenant_id: tenant.into(),
            project_id: project.map(str::to_string),
            roles: roles.iter().map(|role| (*role).into()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

async fn request_raw(
    router: &Router,
    method: Method,
    uri: &str,
    body: Value,
    token: Option<&str>,
) -> (StatusCode, Bytes) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
}

fn client(tenant: &str) -> ApiClient {
    ApiClient {
        id: format!("client-{tenant}"),
        name: format!("name-{tenant}"),
        description: String::new(),
        tenant_id: format!("tenant-{tenant}"),
        owner: "owner".into(),
        granted_agent_ids: vec![format!("agent-{tenant}")],
        status: "active".into(),
        rate_limit: Default::default(),
        quota: Default::default(),
        created_at: "2026-01-01".into(),
        updated_at: "2026-01-01".into(),
    }
}

fn key(tenant: &str) -> ApiKey {
    ApiKey {
        id: format!("key-{tenant}"),
        name: "test".into(),
        client_id: format!("client-{tenant}"),
        key_prefix: format!("sk-tenant-{tenant}-prefix"),
        key_hash: api_gov::hash_key(&format!("test-key-{tenant}")),
        status: "active".into(),
        last_used_at: None,
        expires_at: None,
        created_at: "2026-01-01".into(),
    }
}

async fn seeded(dir: &Path) -> Arc<AppState> {
    let state = test_state(dir);
    *state.api_clients.write().await = vec![client("a"), client("b")];
    *state.api_keys.write().await = vec![key("a"), key("b")];
    *state.user_agents.write().await = vec![
        json!({"id": "agent-a", "tenant_id": "tenant-a", "published": true}),
        json!({"id": "agent-b", "tenant_id": "tenant-b", "published": true}),
    ];
    api_gov::save_api_clients(&state.api_clients.read().await).unwrap();
    api_gov::save_api_keys(&state.api_keys.read().await).unwrap();
    state
}

async fn snapshot(state: &AppState, dir: &Path) -> (Value, Vec<u8>, Vec<u8>) {
    (
        json!({"clients": *state.api_clients.read().await, "keys": *state.api_keys.read().await}),
        std::fs::read(dir.join("api_clients.json")).unwrap(),
        std::fs::read(dir.join("api_keys.json")).unwrap(),
    )
}

#[tokio::test]
async fn cross_tenant_client_mutations_return_identical_not_found() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = seeded(dir.path()).await;
    let router = app(state.clone());
    let token = jwt_for("tenant-b", &["DA"], Some("project-b"));
    let before = snapshot(&state, dir.path()).await;
    let absent_dir = tempfile::tempdir().unwrap();
    let missing_state = test_state(absent_dir.path());
    let missing = app(missing_state);
    for (method, uri, body) in [
        (
            Method::PUT,
            "/api/v1/api-clients/client-a",
            json!({"name":"changed"}),
        ),
        (
            Method::PUT,
            "/api/v1/api-clients/client-a",
            json!({"granted_agent_ids":["agent-a"]}),
        ),
        (Method::DELETE, "/api/v1/api-clients/client-a", json!(null)),
        (
            Method::POST,
            "/api/v1/api-clients/client-a/keys",
            json!({"name":"new"}),
        ),
        (
            Method::DELETE,
            "/api/v1/api-clients/client-a/keys/key-a",
            json!(null),
        ),
    ] {
        let actual = request_raw(&router, method.clone(), uri, body.clone(), Some(&token)).await;
        let absent = request_raw(&missing, method, uri, body, Some(&token)).await;
        assert_eq!(actual.0, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(actual, absent, "{uri}");
        assert_eq!(
            snapshot(&state, dir.path()).await,
            before,
            "{uri} changed persisted state"
        );
    }
}

#[tokio::test]
async fn cross_tenant_revoke_via_own_client_is_not_found() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = seeded(dir.path()).await;
    let router = app(state.clone());
    let token = jwt_for("tenant-b", &["DA"], Some("project-b"));
    let before = snapshot(&state, dir.path()).await;
    let actual = request_raw(
        &router,
        Method::DELETE,
        "/api/v1/api-clients/client-b/keys/key-a",
        json!(null),
        Some(&token),
    )
    .await;
    assert_eq!(snapshot(&state, dir.path()).await, before);
    state.api_keys.write().await.retain(|key| key.id != "key-a");
    let missing = request_raw(
        &router,
        Method::DELETE,
        "/api/v1/api-clients/client-b/keys/key-a",
        json!(null),
        Some(&token),
    )
    .await;
    assert_eq!(actual.0, StatusCode::NOT_FOUND);
    assert_eq!(actual, missing);
    assert_eq!(
        std::fs::read(dir.path().join("api_keys.json")).unwrap(),
        before.2
    );
}

#[tokio::test]
async fn list_returns_only_verified_tenant_clients() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = app(seeded(dir.path()).await);
    for (tenant, own, other) in [("tenant-a", "a", "b"), ("tenant-b", "b", "a")] {
        let token = jwt_for(tenant, &["DA"], Some("project"));
        let (status, bytes) = request_raw(
            &router,
            Method::GET,
            "/api/v1/api-clients",
            json!(null),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains(&format!("client-{own}")));
        assert!(!body.contains(&format!("client-{other}")));
        assert!(!body.contains(&format!("key-{other}")));
        assert!(!body.contains(&format!("sk-tenant-{other}-prefix")));
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["count"], 1);
    }
}

#[tokio::test]
async fn audit_is_filtered_by_verified_tenant() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = app(seeded(dir.path()).await);
    for entry in [
        json!({"client_id":"client-a","tenant_id":"tenant-a","result":"a"}),
        json!({"client_id":"client-b","tenant_id":"tenant-b","result":"b"}),
        json!({"client_id":"client-a","result":"legacy-a"}),
        json!({"client_id":"orphan","result":"orphan"}),
    ] {
        api_gov::append_audit(&entry);
    }
    let a = jwt_for("tenant-a", &["DA"], Some("project-a"));
    let b = jwt_for("tenant-b", &["DA"], Some("project-b"));
    let read = |bytes: Bytes| -> Value { serde_json::from_slice(&bytes).unwrap() };
    let (_, bytes) = request_raw(
        &router,
        Method::GET,
        "/api/v1/api-audit",
        json!(null),
        Some(&b),
    )
    .await;
    assert_eq!(
        read(bytes)["records"],
        json!([{"client_id":"client-b","tenant_id":"tenant-b","result":"b"}])
    );
    let (_, bytes) = request_raw(
        &router,
        Method::GET,
        "/api/v1/api-audit?client_id=client-a",
        json!(null),
        Some(&b),
    )
    .await;
    assert_eq!(read(bytes)["count"], 0);
    let (_, bytes) = request_raw(
        &router,
        Method::GET,
        "/api/v1/api-audit",
        json!(null),
        Some(&a),
    )
    .await;
    assert_eq!(read(bytes)["count"], 2);
    for i in 0..30 {
        api_gov::append_audit(&json!({"client_id":"client-b","tenant_id":"tenant-b","seq":i}));
    }
    let (_, bytes) = request_raw(
        &router,
        Method::GET,
        "/api/v1/api-audit?limit=1",
        json!(null),
        Some(&a),
    )
    .await;
    assert_eq!(read(bytes)["records"][0]["result"], "legacy-a");
}

#[tokio::test]
async fn granted_agent_ids_rejects_cross_tenant_at_write_time() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = seeded(dir.path()).await;
    let router = app(state.clone());
    let token = jwt_for("tenant-b", &["DA"], Some("project-b"));
    let before = snapshot(&state, dir.path()).await;
    let bad = request_raw(
        &router,
        Method::POST,
        "/api/v1/api-clients",
        json!({"name":"new","granted_agent_ids":["agent-a"]}),
        Some(&token),
    )
    .await;
    let absent = request_raw(
        &router,
        Method::POST,
        "/api/v1/api-clients",
        json!({"name":"new","granted_agent_ids":["does-not-exist"]}),
        Some(&token),
    )
    .await;
    assert_eq!(bad.0, StatusCode::BAD_REQUEST);
    assert_eq!(bad, absent);
    assert_eq!(
        request_raw(
            &router,
            Method::PUT,
            "/api/v1/api-clients/client-b",
            json!({"granted_agent_ids":["agent-a"]}),
            Some(&token)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(snapshot(&state, dir.path()).await, before);
    assert_eq!(
        request_raw(
            &router,
            Method::PUT,
            "/api/v1/api-clients/client-b",
            json!({"granted_agent_ids":["agent-b"]}),
            Some(&token)
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn public_gate_rejects_cross_tenant_grant_from_legacy_data() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = seeded(dir.path()).await;
    state
        .api_clients
        .write()
        .await
        .iter_mut()
        .find(|c| c.id == "client-b")
        .unwrap()
        .granted_agent_ids = vec!["agent-a".into()];
    let (plaintext, prefix, hash) = api_gov::generate_key("tenant-b");
    state.api_keys.write().await.push(ApiKey {
        id: "real-key".into(),
        client_id: "client-b".into(),
        key_prefix: prefix,
        key_hash: hash,
        ..key("b")
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Json(json!({}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    state
        .gateway
        .set_base_url(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let router = app(state.clone());
    let bearer = format!("Bearer {plaintext}");
    let call = |id: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/v1/public/agents/{id}/chat"))
            .header("authorization", &bearer)
            .header("content-type", "application/json")
            .body(Body::from(json!({"message":"hello"}).to_string()))
            .unwrap()
    };
    let denied = router.clone().oneshot(call("agent-a")).await.unwrap();
    let audit: Value = serde_json::from_str(
        std::fs::read_to_string(api_gov::api_audit_path())
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(audit["result"], "not_in_scope");
    assert_eq!(audit["tenant_id"], "tenant-b");
    state
        .api_clients
        .write()
        .await
        .iter_mut()
        .find(|c| c.id == "client-b")
        .unwrap()
        .granted_agent_ids
        .clear();
    let missing = router.clone().oneshot(call("agent-a")).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        to_bytes(denied.into_body(), usize::MAX).await.unwrap(),
        to_bytes(missing.into_body(), usize::MAX).await.unwrap()
    );
    let response = router
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let models = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(!String::from_utf8_lossy(&models).contains("agent-a"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn public_gate_rejects_legacy_agent_without_tenant() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = seeded(dir.path()).await;
    state
        .user_agents
        .write()
        .await
        .push(json!({"id": "agent-legacy", "published": true}));
    state
        .api_clients
        .write()
        .await
        .iter_mut()
        .find(|c| c.id == "client-b")
        .unwrap()
        .granted_agent_ids = vec!["agent-legacy".into()];
    let (plaintext, prefix, hash) = api_gov::generate_key("tenant-b");
    state.api_keys.write().await.push(ApiKey {
        id: "legacy-key".into(),
        client_id: "client-b".into(),
        key_prefix: prefix,
        key_hash: hash,
        ..key("b")
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Json(json!({}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    state
        .gateway
        .set_base_url(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let router = app(state.clone());
    let bearer = format!("Bearer {plaintext}");
    let call = |id: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/v1/public/agents/{id}/chat"))
            .header("authorization", &bearer)
            .header("content-type", "application/json")
            .body(Body::from(json!({"message":"hello"}).to_string()))
            .unwrap()
    };
    let legacy = router.clone().oneshot(call("agent-legacy")).await.unwrap();
    let ungranted = router.clone().oneshot(call("agent-b")).await.unwrap();
    assert_eq!(legacy.status(), StatusCode::FORBIDDEN);
    assert_eq!(ungranted.status(), StatusCode::FORBIDDEN);
    let legacy_body = to_bytes(legacy.into_body(), usize::MAX).await.unwrap();
    let ungranted_body = to_bytes(ungranted.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&legacy_body).unwrap()["error"],
        serde_json::from_slice::<Value>(&ungranted_body).unwrap()["error"],
        "a legacy agent without tenant_id must be treated as out of scope"
    );
    let response = router
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let models = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(!String::from_utf8_lossy(&models).contains("agent-legacy"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn config_update_requires_control_plane_da() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let _platform_tenant = EnvGuard::set(&[("AGENTOS_PLATFORM_ADMIN_TENANT", "platform".into())]);
    let state = seeded(dir.path()).await;
    let router = app(state.clone());
    let old_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let make_mock = |calls: Arc<AtomicUsize>| {
        Router::new().route(
            "/v1/models",
            get(move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"data": []}))
                }
            }),
        )
    };
    let old_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old_url = format!("http://{}", old_listener.local_addr().unwrap());
    let new_url = format!("http://{}", new_listener.local_addr().unwrap());
    let old_counter = old_calls.clone();
    let new_counter = new_calls.clone();
    let old_server = tokio::spawn(async move {
        axum::serve(old_listener, make_mock(old_counter))
            .await
            .unwrap()
    });
    let new_server = tokio::spawn(async move {
        axum::serve(new_listener, make_mock(new_counter))
            .await
            .unwrap()
    });
    state.gateway.set_base_url(old_url);
    let patch = json!({"gateway":{"base_url":new_url,"default_model":"new-model"}});
    let before = state.config_info.read().await.clone();
    let original_model = state.gateway.default_model();
    let path = dir.path().join("config_override.json");
    for token in [None, Some(jwt_for("tenant-a", &["DA"], None))] {
        let (status, _) = request_raw(
            &router,
            Method::PUT,
            "/api/v1/config",
            patch.clone(),
            token.as_deref(),
        )
        .await;
        assert_eq!(
            status,
            if token.is_none() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert_eq!(*state.config_info.read().await, before);
        assert_eq!(state.gateway.default_model(), original_model);
        assert!(!path.exists());
        assert!(state.gateway.health_check().await.unwrap());
    }
    assert_eq!(old_calls.load(Ordering::SeqCst), 2);
    assert_eq!(new_calls.load(Ordering::SeqCst), 0);
    let da = jwt_for("tenant-a", &["DA"], Some("project-a"));
    // #274: tightened, an explicit tenant DA no longer writes global config (200 → 403).
    assert_eq!(
        request_raw(
            &router,
            Method::PUT,
            "/api/v1/config",
            patch.clone(),
            Some(&da)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(!path.exists());
    assert_eq!(state.gateway.default_model(), original_model);
    let admin = jwt_for("platform", &["PLATFORM_ADMIN"], Some("project-a"));
    assert_eq!(
        request_raw(&router, Method::PUT, "/api/v1/config", patch, Some(&admin))
            .await
            .0,
        // #274: tightened (success requires the platform-admin claim shape).
        StatusCode::OK
    );
    assert_eq!(state.gateway.default_model(), "new-model");
    assert!(state.gateway.health_check().await.unwrap());
    assert_eq!(old_calls.load(Ordering::SeqCst), 2);
    assert_eq!(new_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.config_info.read().await["gateway"]["base_url"],
        new_url
    );
    assert!(std::fs::read_to_string(path).unwrap().contains(&new_url));
    old_server.abort();
    new_server.abort();
}
