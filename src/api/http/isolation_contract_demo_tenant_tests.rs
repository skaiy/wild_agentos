//! Demo data is tenant-scoped; demo administration cannot change deployment config.

use std::{path::Path, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    routing::get,
    Json, Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{
    control_plane_route_auth_tests::{app, test_state, EnvGuard},
    iam::{JwtClaims, PLATFORM_ADMIN_ROLE, PLATFORM_ADMIN_TENANT_ENV},
    AppState, TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
const DEMO: &str = "demo";
const PLATFORM: &str = "platform";
const PROJECT: &str = "showcase";
const ACTOR: &str = "demo-user";
const CHANGE_URL: &str = "https://changed.example.invalid";

fn setup(dir: &Path) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(SECRET.to_vec()).unwrap(),
        ),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
        ("AGENTOS_AUTH_STRICT", "true".into()),
        (PLATFORM_ADMIN_TENANT_ENV, PLATFORM.into()),
    ])
}

fn jwt_for(tenant: &str, roles: &[&str]) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: ACTOR.into(),
            tenant_id: tenant.into(),
            project_id: Some(PROJECT.into()),
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
    token: &str,
) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn assert_platform_denied(result: &(StatusCode, Value), resource: &str) {
    assert_eq!(result.0, StatusCode::FORBIDDEN);
    assert_eq!(
        result.1,
        json!({
            "error": "platform_admin_required",
            "message": format!("platform administrator required for {resource}")
        })
    );
}

async fn assert_unchanged(
    state: &Arc<AppState>,
    before: &Value,
    path: &Path,
    file: Option<&[u8]>,
    model: &str,
    mapping: &str,
) {
    assert_eq!(*state.config_info.read().await, *before);
    assert_eq!(state.gateway.default_model(), model);
    assert_eq!(state.gateway.get_model("default"), mapping);
    assert!(state.gateway.health_check().await.unwrap());
    assert_eq!(std::fs::read(path).ok().as_deref(), file);
    assert!(state.vector_store.load_full().is_none());
}

#[tokio::test]
async fn isolation_contract_demo_tenant_da_cannot_write_global_config_with_or_without_platform_setting(
) {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let token = jwt_for(DEMO, &["DA", "mcp_invoke"]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let original_url = format!("http://{}", listener.local_addr().unwrap());
    let mock = Router::new().route("/v1/models", get(|| async { Json(json!({"data": []})) }));
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let patches = [
        json!({"gateway": {"base_url": CHANGE_URL, "default_model": "changed-model"}}),
        json!({"gateway": {"model_mapping": {"default": "changed-mapping"}}}),
        json!({"embedding": {"enabled": true}}),
        json!({"models": {"providers": [], "resources": []}}),
        json!({"admin_policies": {"sentinel": "changed"}}),
    ];
    for setting in [Some(PLATFORM), None] {
        match setting {
            Some(value) => std::env::set_var(PLATFORM_ADMIN_TENANT_ENV, value),
            None => std::env::remove_var(PLATFORM_ADMIN_TENANT_ENV),
        }
        for original_file in [None, Some(b"{ \"sentinel\": \"original\" }\n".as_slice())] {
            let state = test_state(dir.path());
            state.gateway.set_base_url(original_url.clone());
            let router = app(state.clone());
            let path = dir.path().join("config_override.json");
            if let Some(bytes) = original_file {
                std::fs::write(&path, bytes).unwrap();
            }
            let before = state.config_info.read().await.clone();
            let model = state.gateway.default_model();
            let mapping = state.gateway.get_model("default");
            for patch in &patches {
                let result = request(
                    &router,
                    Method::PUT,
                    "/api/v1/config",
                    patch.clone(),
                    &token,
                )
                .await;
                assert_platform_denied(&result, "configuration updates");
                assert_unchanged(&state, &before, &path, original_file, &model, &mapping).await;
            }
            let result = request(
                &router,
                Method::POST,
                "/api/v1/embedding/activate",
                json!({"resource_id": "missing"}),
                &token,
            )
            .await;
            assert_platform_denied(&result, "model operations");
            assert_unchanged(&state, &before, &path, original_file, &model, &mapping).await;
            if original_file.is_some() {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
    server.abort();
}

#[tokio::test]
async fn isolation_contract_demo_tenant_platform_admin_role_cannot_cross_platform_tenant() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = test_state(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let original_url = format!("http://{}", listener.local_addr().unwrap());
    let mock = Router::new().route("/v1/models", get(|| async { Json(json!({"data": []})) }));
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    state.gateway.set_base_url(original_url);
    let router = app(state.clone());
    let path = dir.path().join("config_override.json");
    let before = state.config_info.read().await.clone();
    let model = state.gateway.default_model();
    let mapping = state.gateway.get_model("default");
    let patch = json!({"gateway": {"base_url": CHANGE_URL}});
    let demo = jwt_for(DEMO, &[PLATFORM_ADMIN_ROLE]);
    let result = request(&router, Method::PUT, "/api/v1/config", patch.clone(), &demo).await;
    assert_platform_denied(&result, "configuration updates");
    assert_unchanged(&state, &before, &path, None, &model, &mapping).await;

    let platform = jwt_for(PLATFORM, &[PLATFORM_ADMIN_ROLE]);
    let (status, _) = request(&router, Method::PUT, "/api/v1/config", patch, &platform).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.config_info.read().await["gateway"]["base_url"],
        CHANGE_URL
    );
    assert!(path.exists());
    server.abort();
}

#[tokio::test]
async fn isolation_contract_demo_tenant_data_is_invisible_to_another_tenant() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let state = test_state(dir.path());
    let router = app(state.clone());
    let demo = jwt_for(DEMO, &["DA", "mcp_invoke"]);
    let other = jwt_for("demo-xcheck", &["DA"]);
    let (status, created) = request(
        &router,
        Method::POST,
        "/api/v1/api-clients",
        json!({"name": "Example Co. demo client"}),
        &demo,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["client"]["id"].as_str().unwrap();
    let uri = format!("/api/v1/api-clients/{id}");
    let (status, own_list) = request(
        &router,
        Method::GET,
        "/api/v1/api-clients",
        json!(null),
        &demo,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(own_list["clients"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == id));
    let (status, other_list) = request(
        &router,
        Method::GET,
        "/api/v1/api-clients",
        json!(null),
        &other,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(other_list["clients"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["id"] != id));

    let absent_dir = tempfile::tempdir().unwrap();
    let missing = app(test_state(absent_dir.path()));
    for (method, body) in [
        (Method::PUT, json!({"name": "changed"})),
        (Method::DELETE, json!(null)),
    ] {
        let actual = request(&router, method.clone(), &uri, body.clone(), &other).await;
        let absent = request(&missing, method, &uri, body, &other).await;
        assert_eq!(actual.0, StatusCode::NOT_FOUND);
        assert_eq!(actual, absent);
    }
    let (status, own_list) = request(
        &router,
        Method::GET,
        "/api/v1/api-clients",
        json!(null),
        &demo,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(own_list["clients"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == id));
}
