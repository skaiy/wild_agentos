//! Route-level isolation contract for reading configuration.

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{Method, Request, StatusCode},
    Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{
    control_plane_route_auth_tests::{test_state, EnvGuard},
    iam::{JwtClaims, PLATFORM_ADMIN_ROLE, PLATFORM_ADMIN_TENANT_ENV},
    TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
const MARKER: &str = "sk-test-marker-290";

fn setup(dir: &std::path::Path) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        ("AGENTOS_AUTH_STRICT", "true".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(SECRET.to_vec()).unwrap(),
        ),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
        ("MCP_JWT_SUBJECT", "config-read-test".into()),
        (PLATFORM_ADMIN_TENANT_ENV, "platform".into()),
        (
            "MCP_OUTBOUND_ALLOWED_ORIGINS",
            "https://example.invalid".into(),
        ),
    ])
}

fn jwt(roles: &[&str], project: Option<&str>) -> String {
    jwt_for("tenant-a", roles, project)
}

fn jwt_for(tenant: &str, roles: &[&str], project: Option<&str>) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: "config-reader".into(),
            tenant_id: tenant.into(),
            project_id: project.map(str::to_owned),
            roles: roles.iter().map(|role| (*role).into()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

fn router(dir: &std::path::Path, snapshot: Value) -> Router {
    let state = test_state(dir);
    super::build_router(
        state.core.clone(),
        state.gateway.clone(),
        state.kg_store.clone(),
        snapshot,
        state.agents_info.clone(),
        state.vector_store.clone(),
        None,
        None,
        crate::config::OnlineCorpusWatcherSettings::default(),
        state.shutdown.clone(),
    )
}

async fn request(
    router: &Router,
    method: Method,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, Bytes) {
    let mut builder = Request::builder()
        .method(method)
        .uri("/api/v1/config")
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
    (
        status,
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
}

fn error(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap()
}

async fn post(router: &Router, uri: &str, body: Value, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
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

/// Independent of the production blacklist on purpose: normalize like the
/// contract says (lowercase, drop `_` / `-`) and reject any secret-looking name.
fn assert_no_secret_keys(value: &Value) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                let name: String = key
                    .to_lowercase()
                    .chars()
                    .filter(|c| *c != '_' && *c != '-')
                    .collect();
                let forbidden = !name.ends_with("configured")
                    && [
                        "apikey",
                        "secret",
                        "token",
                        "password",
                        "secretkey",
                        "privatekey",
                        "accesskey",
                        "authorization",
                        "credential",
                        "credentials",
                    ]
                    .iter()
                    .any(|suffix| name.ends_with(suffix));
                assert!(!forbidden, "secret key leaked");
                assert_no_secret_keys(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_no_secret_keys(item);
            }
        }
        _ => {}
    }
}

fn assert_no_marker(body: &Bytes) {
    assert!(!body
        .windows(MARKER.len())
        .any(|window| window == MARKER.as_bytes()));
}

#[tokio::test]
async fn isolation_contract_config_read_rejects_anonymous() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = router(dir.path(), json!({"before": true}));
    let (status, body) = request(&router, Method::GET, Value::Null, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error(&body)["error"], "unauthorized");
    assert_eq!(
        error(&body)["message"],
        "a verified JWT is required to read configuration"
    );
}

#[tokio::test]
async fn isolation_contract_config_read_rejects_public_api_key() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = router(dir.path(), json!({"before": true}));
    let da = jwt(&["DA"], Some("project-a"));
    let (status, client) = post(
        &router,
        "/api/v1/api-clients",
        json!({"name": "config-read-test"}),
        &da,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = client["client"]["id"].as_str().unwrap();
    let (status, issued) = post(
        &router,
        &format!("/api/v1/api-clients/{id}/keys"),
        json!({"name": "read-test"}),
        &da,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let public_key = issued["api_key"].as_str().unwrap();
    let (status, _) = request(&router, Method::GET, Value::Null, Some(public_key)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn isolation_contract_config_read_rejects_defaulted_project() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = router(dir.path(), json!({"before": true}));
    let (status, body) =
        request(&router, Method::GET, Value::Null, Some(&jwt(&["DA"], None))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error(&body)["error"], "control_plane_claims_incomplete");
}

#[tokio::test]
async fn isolation_contract_config_read_rejects_non_da() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = router(dir.path(), json!({"before": true}));
    let (status, body) = request(
        &router,
        Method::GET,
        Value::Null,
        Some(&jwt(&["PA"], Some("project-a"))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error(&body)["error"], "forbidden");
    // #287 will remove user_id/roles from the shared require_role error.
}

#[tokio::test]
async fn isolation_contract_config_read_scrubs_nested_snapshot() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let snapshot = json!({
        "gateway": {"api_key_configured": true, "api_key": MARKER},
        "nested": [{"access_token": MARKER, "safe": {"client_secret": MARKER}}],
        "camel": {"accessToken": MARKER, "privateKey": MARKER, "secret-key": MARKER},
        "auth": {"Authorization": MARKER, "accessKey": MARKER, "credentials": {"v": MARKER}, "credential": MARKER},
        "base_url": "https://example.invalid",
        "max_tokens": 100
    });
    let router = router(dir.path(), snapshot);
    let (status, body) = request(
        &router,
        Method::GET,
        Value::Null,
        Some(&jwt(&["DA"], Some("project-a"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body
        .windows(MARKER.len())
        .any(|window| window == MARKER.as_bytes()));
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_no_secret_keys(&response);
    assert_eq!(response["gateway"]["api_key_configured"], true);
    assert_eq!(response["base_url"], "https://example.invalid");
    assert_eq!(response["max_tokens"], 100);
}

#[tokio::test]
async fn isolation_contract_config_read_hides_gateway_key_after_put() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = router(
        dir.path(),
        json!({"gateway": {"api_key_configured": false}}),
    );
    let da = jwt(&["DA"], Some("project-a"));
    // #274: tightened. A tenant DA can read but not write global configuration.
    let (status, body) = request(
        &router,
        Method::PUT,
        json!({"gateway": {"api_key": MARKER}}),
        Some(&da),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error(&body)["error"], "platform_admin_required");
    assert!(!dir.path().join("config_override.json").exists());
    let admin = jwt_for("platform", &[PLATFORM_ADMIN_ROLE], Some("ops"));
    let (status, _) = request(
        &router,
        Method::PUT,
        json!({"gateway": {"api_key": MARKER}}),
        Some(&admin),
    )
    .await;
    // #274: tightened (the write now succeeds only with a platform-admin token).
    assert_eq!(status, StatusCode::OK);
    let (status, body) = request(&router, Method::GET, Value::Null, Some(&da)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body
        .windows(MARKER.len())
        .any(|window| window == MARKER.as_bytes()));
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_no_secret_keys(&response);
    assert_eq!(response["gateway"]["api_key_configured"], true);
    assert!(
        !std::fs::read_to_string(dir.path().join("config_override.json"))
            .unwrap()
            .contains(MARKER)
    );
}

#[tokio::test]
async fn isolation_contract_config_read_allows_platform_admin_without_da() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let snapshot = json!({
        "gateway": {"api_key_configured": true, "apiKey": MARKER, "base_url": "https://example.invalid"},
        "models": {"providers": [{"id": "p", "access_token": MARKER, "api_key_configured": true}]}
    });
    let router = router(dir.path(), snapshot);

    let admin = jwt_for("platform", &[PLATFORM_ADMIN_ROLE], Some("ops"));
    let (status, body) = request(&router, Method::GET, Value::Null, Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert_no_marker(&body);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_no_secret_keys(&response);
    assert_eq!(response["gateway"]["api_key_configured"], true);
    assert_eq!(response["gateway"]["base_url"], "https://example.invalid");
    assert_eq!(
        response["models"]["providers"][0],
        json!({"id": "p", "api_key_configured": true})
    );
}

#[tokio::test]
async fn isolation_contract_config_read_platform_admin_role_needs_platform_scope() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = router(dir.path(), json!({"gateway": {"api_key": MARKER}}));

    // PLATFORM_ADMIN outside the platform tenant, or without an explicit
    // project, is not a reader unless it is also a control-plane DA.
    for token in [
        jwt_for("tenant-a", &[PLATFORM_ADMIN_ROLE], Some("project-a")),
        jwt_for("platform", &[PLATFORM_ADMIN_ROLE], None),
        jwt_for("platform", &["platform_admin"], Some("ops")),
    ] {
        let (status, body) = request(&router, Method::GET, Value::Null, Some(&token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_no_marker(&body);
    }
    // The platform tenant setting fails closed for reads too.
    std::env::set_var(PLATFORM_ADMIN_TENANT_ENV, "default");
    let (status, body) = request(
        &router,
        Method::GET,
        Value::Null,
        Some(&jwt_for("default", &[PLATFORM_ADMIN_ROLE], Some("ops"))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_no_marker(&body);
}
