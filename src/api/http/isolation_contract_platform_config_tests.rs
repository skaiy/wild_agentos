//! Global configuration is owned by the deployment, not by a tenant DA.

// Test-only lock held for the whole test by design (serializes process-global env/state);
// code under test never takes it, so holding it across `.await` cannot deadlock.
#![allow(clippy::await_holding_lock)]

use std::{path::Path, sync::Arc};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{Method, Request, StatusCode},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

use super::{
    control_plane_route_auth_tests::{app, test_state, EnvGuard},
    iam::{JwtClaims, PLATFORM_ADMIN_ROLE, PLATFORM_ADMIN_TENANT_ENV},
    AppState, TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
const PLATFORM_TENANT: &str = "tenant-platform";
const SUBJECT: &str = "test-config-actor";
const PROJECT: &str = "project-config";
const BASE_URL: &str = "https://changed.example.invalid";
const FAKE_KEY: &str = "fake-config-key-never-real";

fn setup(dir: &Path) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(SECRET.to_vec()).unwrap(),
        ),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
        ("AGENTOS_AUTH_STRICT", "true".into()),
        (PLATFORM_ADMIN_TENANT_ENV, PLATFORM_TENANT.into()),
    ])
}

fn jwt_for(tenant: &str, roles: &[&str], project: Option<&str>) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: SUBJECT.into(),
            tenant_id: tenant.into(),
            project_id: project.map(str::to_owned),
            roles: roles.iter().map(|role| (*role).into()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

async fn request(
    router: &Router,
    method: Method,
    uri: &str,
    body: Value,
    token: Option<&str>,
    x_identity: Option<&str>,
) -> (StatusCode, Bytes) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(x_identity) = x_identity {
        builder = builder.header("x-identity", x_identity);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
}

fn assert_denied(status: StatusCode, body: &Bytes, resource: &str, private_values: &[&str]) {
    assert_eq!(status, StatusCode::FORBIDDEN);
    let parsed: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        parsed,
        json!({
            "error": "platform_admin_required",
            "message": format!("platform administrator required for {resource}")
        })
    );
    let raw = std::str::from_utf8(body).unwrap();
    for value in private_values {
        assert!(
            !raw.contains(value),
            "authorization response leaked a private value"
        );
    }
}

async fn assert_unchanged(
    state: &Arc<AppState>,
    snapshot: &Value,
    path: &Path,
    original_file: Option<&[u8]>,
    original_model: &str,
    original_mapping: &str,
) {
    assert_eq!(*state.config_info.read().await, *snapshot);
    assert_eq!(state.gateway.default_model(), original_model);
    assert_eq!(state.gateway.get_model("default"), original_mapping);
    assert_eq!(std::fs::read(path).ok().as_deref(), original_file);
}

#[tokio::test]
async fn isolation_contract_platform_config_da_cannot_write_any_section() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let token = jwt_for("tenant-a", &["DA"], Some(PROJECT));
    let patches = [
        json!({"gateway": {"base_url": BASE_URL, "default_model": "changed-model"}}),
        json!({"gateway": {"api_key": FAKE_KEY}}),
        json!({"gateway": {"model_mapping": {"default": "changed-mapping"}}}),
        json!({"embedding": {"enabled": true, "oneapi": {"api_key": FAKE_KEY}}}),
        json!({"models": {"providers": [{"id": "changed-provider"}], "resources": []}}),
        json!({"admin_policies": {"sentinel": "changed-policy"}}),
    ];
    for initial_file in [None, Some(b"{ \"sentinel\": \"original\" }\n".as_slice())] {
        let state = test_state(dir.path());
        let router = app(state.clone());
        let path = dir.path().join("config_override.json");
        if let Some(bytes) = initial_file {
            std::fs::write(&path, bytes).unwrap();
        }
        let snapshot = state.config_info.read().await.clone();
        let model = state.gateway.default_model();
        let mapping = state.gateway.get_model("default");
        for patch in &patches {
            let (status, body) = request(
                &router,
                Method::PUT,
                "/api/v1/config",
                patch.clone(),
                Some(&token),
                None,
            )
            .await;
            assert_denied(
                status,
                &body,
                "configuration updates",
                &[
                    SUBJECT, "tenant-a", PROJECT, "DA", BASE_URL, FAKE_KEY, "changed-",
                ],
            );
            assert_unchanged(&state, &snapshot, &path, initial_file, &model, &mapping).await;
        }
        if initial_file.is_some() {
            std::fs::remove_file(&path).unwrap();
        }
    }
}

#[tokio::test]
async fn isolation_contract_platform_config_da_cannot_activate_embedding() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let path = dir.path().join("config_override.json");
    let original_file = br#"{"models":{"providers":[{"id":"provider-test","base_url":"http://127.0.0.1:9","api_key":"fake-provider-key"}],"resources":[{"id":"embedding-test","provider_id":"provider-test","model":"embed-test","modalities":["embedding"],"dimension":3}]}}"#;
    std::fs::write(&path, original_file).unwrap();
    let state = test_state(dir.path());
    let before = state.config_info.read().await.clone();
    let router = app(state.clone());
    let token = jwt_for("tenant-a", &["DA"], Some(PROJECT));
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/v1/embedding/activate",
        json!({"resource_id": "embedding-test"}),
        Some(&token),
        None,
    )
    .await;
    assert_denied(
        status,
        &body,
        "model operations",
        &[SUBJECT, "tenant-a", PROJECT, "DA", "embedding-test"],
    );
    assert_eq!(std::fs::read(&path).unwrap(), original_file);
    assert_eq!(*state.config_info.read().await, before);
    assert!(state.vector_store.load_full().is_none());
    assert!(state.knowledge_bases.read().await.is_empty());
    assert!(!dir.path().join("vector_store").exists());
}

#[tokio::test]
async fn isolation_contract_platform_config_platform_tenant_and_scope_fail_closed() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = test_state(dir.path());
    let router = app(state.clone());
    let path = dir.path().join("config_override.json");
    let before = state.config_info.read().await.clone();
    let model = state.gateway.default_model();
    let mapping = state.gateway.get_model("default");
    let patch = json!({"gateway": {"base_url": BASE_URL}});
    for (tenant, project, setting) in [
        ("tenant-other", Some(PROJECT), Some(PLATFORM_TENANT)),
        (PLATFORM_TENANT, None, Some(PLATFORM_TENANT)),
        (PLATFORM_TENANT, Some(""), Some(PLATFORM_TENANT)),
        (PLATFORM_TENANT, Some(PROJECT), None),
        (PLATFORM_TENANT, Some(PROJECT), Some("default")),
        ("default", Some(PROJECT), Some("default")),
        (PLATFORM_TENANT, Some(PROJECT), Some("  ")),
    ] {
        if let Some(setting) = setting {
            std::env::set_var(PLATFORM_ADMIN_TENANT_ENV, setting);
        } else {
            std::env::remove_var(PLATFORM_ADMIN_TENANT_ENV);
        }
        let token = jwt_for(tenant, &[PLATFORM_ADMIN_ROLE], project);
        let (status, body) = request(
            &router,
            Method::PUT,
            "/api/v1/config",
            patch.clone(),
            Some(&token),
            None,
        )
        .await;
        assert_denied(
            status,
            &body,
            "configuration updates",
            &[SUBJECT, tenant, PROJECT, PLATFORM_ADMIN_ROLE, BASE_URL],
        );
        assert_unchanged(&state, &before, &path, None, &model, &mapping).await;
    }
}

#[tokio::test]
async fn isolation_contract_platform_config_admin_can_update_without_da() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = test_state(dir.path());
    let router = app(state.clone());
    let token = jwt_for(PLATFORM_TENANT, &[PLATFORM_ADMIN_ROLE], Some(PROJECT));
    let (status, body) = request(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({"gateway": {
            "base_url": BASE_URL,
            "default_model": "new-default",
            "model_mapping": {"default": "mapped-model"},
            "api_key": FAKE_KEY
        }}),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(state.gateway.default_model(), "new-default");
    assert_eq!(state.gateway.get_model("default"), "mapped-model");
    let snapshot = state.config_info.read().await;
    assert_eq!(snapshot["gateway"]["base_url"], BASE_URL);
    assert_eq!(snapshot["gateway"]["api_key_configured"], true);
    assert!(snapshot["gateway"].get("api_key").is_none());
    assert_eq!(response["config"]["gateway"]["api_key_configured"], true);
    assert!(!std::str::from_utf8(&body).unwrap().contains(FAKE_KEY));
    let disk = std::fs::read_to_string(dir.path().join("config_override.json")).unwrap();
    assert!(disk.contains(BASE_URL));
    assert!(!disk.contains(FAKE_KEY));
    assert!(!disk.contains("\"api_key\""));
}

#[tokio::test]
async fn isolation_contract_platform_config_non_strict_anonymous_and_header_fail_closed() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    std::env::set_var("AGENTOS_AUTH_STRICT", "false");
    let state = test_state(dir.path());
    let router = app(state.clone());
    let before = state.config_info.read().await.clone();
    let path = dir.path().join("config_override.json");
    let model = state.gateway.default_model();
    let mapping = state.gateway.get_model("default");
    let header = STANDARD.encode(
        json!({"user_id": SUBJECT, "tenant_id": PLATFORM_TENANT, "roles": [PLATFORM_ADMIN_ROLE]})
            .to_string(),
    );
    for header in [None, Some(header.as_str())] {
        let (status, body) = request(
            &router,
            Method::PUT,
            "/api/v1/config",
            json!({"gateway": {"base_url": BASE_URL}}),
            None,
            header,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // The pre-existing PUT /api/v1/config 401 body (unchanged by #274).
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            "unauthorized"
        );
        let raw = std::str::from_utf8(&body).unwrap();
        assert!(
            !raw.contains(SUBJECT) && !raw.contains(PLATFORM_TENANT) && !raw.contains(BASE_URL)
        );
        assert_unchanged(&state, &before, &path, None, &model, &mapping).await;
    }
}

#[tokio::test]
async fn isolation_contract_platform_config_rejection_does_not_change_gateway_url() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let mock = Router::new().route(
        "/v1/models",
        get(move || {
            let hits = counted.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(json!({"data": []}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let state = test_state(dir.path());
    state.gateway.set_base_url(old_url);
    let router = app(state.clone());
    let token = jwt_for("tenant-a", &["DA"], Some(PROJECT));
    let (status, body) = request(
        &router,
        Method::PUT,
        "/api/v1/config",
        json!({"gateway": {"base_url": BASE_URL}}),
        Some(&token),
        None,
    )
    .await;
    assert_denied(
        status,
        &body,
        "configuration updates",
        &[SUBJECT, "tenant-a", PROJECT, "DA", BASE_URL],
    );
    assert!(state.gateway.health_check().await.unwrap());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}
