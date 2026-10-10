//! #312: control-plane config and model routes authorize before they parse
//! the JSON body. Callers that fail the gate get the gate's 401/403, byte for
//! byte the same for valid and invalid bodies; only authorized callers see
//! the body rejections (422 schema, 400 syntax, 415 content type).

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{Method, Request, StatusCode},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::json;
use tower::ServiceExt;

use super::{
    config::{config_handler, update_config_handler},
    control_plane_route_auth_tests::{test_state, EnvGuard},
    iam::{JwtClaims, PLATFORM_ADMIN_ROLE, PLATFORM_ADMIN_TENANT_ENV},
    models::{activate_embedding_handler, provider_models_handler, test_model_handler},
    TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
const PLATFORM_TENANT: &str = "tenant-platform";

/// Nothing about the request schema may reach a caller that failed the gate.
const SCHEMA_LEAKS: &[&str] = &[
    "gateway",
    "api_key",
    "base_url",
    "resource_id",
    "provider_id",
    "modality",
    "expected one of",
    "missing field",
    "unknown field",
    "invalid type",
    "expected struct",
    "GatewayConfigPatch",
    "ConfigUpdateRequest",
    "ModelTestRequest",
    "ProviderModelsRequest",
    "EmbeddingActivateRequest",
];

const JSON: &str = "application/json";
const TEXT: &str = "text/plain";

/// One invalid body and the status an authorized caller gets for it.
struct Invalid {
    label: &'static str,
    content_type: &'static str,
    body: &'static str,
    authorized_status: StatusCode,
}

const fn invalid(
    label: &'static str,
    content_type: &'static str,
    body: &'static str,
    authorized_status: StatusCode,
) -> Invalid {
    Invalid {
        label,
        content_type,
        body,
        authorized_status,
    }
}

struct Route {
    method: Method,
    uri: &'static str,
    valid: &'static str,
    invalid: Vec<Invalid>,
    /// `true`: gate is `require_platform_admin`; `false`: control-plane DA.
    platform_admin: bool,
}

fn routes() -> Vec<Route> {
    use StatusCode as S;
    vec![
        Route {
            method: Method::PUT,
            uri: "/api/v1/config",
            valid: "{}",
            invalid: vec![
                invalid("unknown field", JSON, r#"{"x":1}"#, S::UNPROCESSABLE_ENTITY),
                invalid(
                    "nested unknown field",
                    JSON,
                    r#"{"gateway":{"x":1}}"#,
                    S::UNPROCESSABLE_ENTITY,
                ),
                invalid(
                    "wrong type",
                    JSON,
                    r#"{"gateway":1}"#,
                    S::UNPROCESSABLE_ENTITY,
                ),
                invalid("syntax", JSON, r#"{"gateway":"#, S::BAD_REQUEST),
                invalid("content type", TEXT, "{}", S::UNSUPPORTED_MEDIA_TYPE),
            ],
            platform_admin: true,
        },
        Route {
            method: Method::POST,
            uri: "/api/v1/embedding/activate",
            valid: r#"{"resource_id":"embedding-a"}"#,
            invalid: vec![
                invalid("unknown field", JSON, r#"{"x":1}"#, S::UNPROCESSABLE_ENTITY),
                invalid("missing field", JSON, "{}", S::UNPROCESSABLE_ENTITY),
                invalid(
                    "wrong type",
                    JSON,
                    r#"{"resource_id":1}"#,
                    S::UNPROCESSABLE_ENTITY,
                ),
                invalid("syntax", JSON, r#"{"resource_id":"#, S::BAD_REQUEST),
                invalid(
                    "content type",
                    TEXT,
                    r#"{"resource_id":"embedding-a"}"#,
                    S::UNSUPPORTED_MEDIA_TYPE,
                ),
            ],
            platform_admin: true,
        },
        Route {
            method: Method::POST,
            uri: "/api/v1/models/test",
            valid: r#"{"resource_id":"missing"}"#,
            invalid: vec![
                invalid(
                    "wrong type",
                    JSON,
                    r#"{"resource_id":1}"#,
                    S::UNPROCESSABLE_ENTITY,
                ),
                invalid(
                    "wrong type (list)",
                    JSON,
                    r#"{"provider_id":[]}"#,
                    S::UNPROCESSABLE_ENTITY,
                ),
                invalid("not an object", JSON, r#""x""#, S::UNPROCESSABLE_ENTITY),
                invalid("syntax", JSON, r#"{"resource_id":"#, S::BAD_REQUEST),
                invalid(
                    "content type",
                    TEXT,
                    r#"{"resource_id":"missing"}"#,
                    S::UNSUPPORTED_MEDIA_TYPE,
                ),
            ],
            // #303 (#352): the provider probes are platform-admin only.
            platform_admin: true,
        },
        Route {
            method: Method::POST,
            uri: "/api/v1/providers/models",
            valid: r#"{"base_url":"","api_key":""}"#,
            invalid: vec![
                invalid(
                    "wrong type",
                    JSON,
                    r#"{"base_url":1}"#,
                    S::UNPROCESSABLE_ENTITY,
                ),
                invalid(
                    "wrong type (object)",
                    JSON,
                    r#"{"api_key":{}}"#,
                    S::UNPROCESSABLE_ENTITY,
                ),
                invalid("not an object", JSON, r#""x""#, S::UNPROCESSABLE_ENTITY),
                invalid("syntax", JSON, r#"{"base_url":"#, S::BAD_REQUEST),
                invalid(
                    "content type",
                    TEXT,
                    r#"{"base_url":"","api_key":""}"#,
                    S::UNSUPPORTED_MEDIA_TYPE,
                ),
            ],
            // #303 (#352): the provider probes are platform-admin only.
            platform_admin: true,
        },
    ]
}

fn env(dir: &std::path::Path) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        ("AGENTOS_AUTH_STRICT", "false".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(SECRET.to_vec()).unwrap(),
        ),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
        (PLATFORM_ADMIN_TENANT_ENV, PLATFORM_TENANT.into()),
    ])
}

fn router(dir: &std::path::Path) -> Router {
    Router::new()
        .route(
            "/api/v1/config",
            get(config_handler).put(update_config_handler),
        )
        .route("/api/v1/models/test", post(test_model_handler))
        .route("/api/v1/providers/models", post(provider_models_handler))
        .route(
            "/api/v1/embedding/activate",
            post(activate_embedding_handler),
        )
        .with_state(test_state(dir))
}

fn token(tenant: &str, roles: &[&str]) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: "body-order-user".into(),
            tenant_id: tenant.into(),
            project_id: Some("project-a".into()),
            roles: roles.iter().map(|role| role.to_string()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

#[derive(Clone, Copy)]
enum Caller<'a> {
    Anonymous,
    XIdentity(&'a str),
    Bearer(&'a str),
}

async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    content_type: &str,
    body: &str,
    caller: Caller<'_>,
) -> (StatusCode, Bytes) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", content_type);
    builder = match caller {
        Caller::Anonymous => builder,
        Caller::XIdentity(value) => builder.header("x-identity", value),
        Caller::Bearer(token) => builder.header("authorization", format!("Bearer {token}")),
    };
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
}

fn assert_no_schema_leak(body: &[u8], context: &str) {
    let text = String::from_utf8_lossy(body);
    for leak in SCHEMA_LEAKS {
        assert!(
            !text.contains(leak),
            "{context}: rejection body leaked {leak:?}: {text}"
        );
    }
}

/// Relative path plus bytes for every file under `dir`. A rejected request
/// must not create `config_override.json` or any other file.
fn files_under(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let path = entry.path();
            if path.is_dir() {
                walk(&path, &rel, out);
            } else {
                out.push((rel, std::fs::read(&path).expect("read data dir file")));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out.sort();
    out
}

fn assert_data_dir_unchanged(dir: &std::path::Path, before: &[(String, Vec<u8>)], context: &str) {
    let override_path = dir.join("config_override.json");
    match before
        .iter()
        .find(|(name, _)| name == "config_override.json")
    {
        None => assert!(
            !override_path.exists(),
            "{context}: config_override.json was created"
        ),
        Some((_, bytes)) => {
            let now = std::fs::read(&override_path).unwrap_or_else(|error| {
                panic!("{context}: config_override.json disappeared: {error}")
            });
            assert_eq!(&now, bytes, "{context}: config_override.json bytes changed");
        }
    }
    assert_eq!(
        files_under(dir),
        before,
        "{context}: data dir changed after a rejected request"
    );
}

#[tokio::test]
async fn isolation_contract_unauthenticated_invalid_bodies_get_401_without_schema() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path());
    let router = router(dir.path());
    let before = files_under(dir.path());
    let x_identity_admin = STANDARD.encode(
        json!({"user_id": "x", "tenant_id": PLATFORM_TENANT, "roles": ["DA", PLATFORM_ADMIN_ROLE]})
            .to_string(),
    );
    let callers = [
        ("anonymous", Caller::Anonymous),
        ("x-identity", Caller::XIdentity(&x_identity_admin)),
        (
            "bad signature",
            Caller::Bearer("eyJhbGciOiJIUzI1NiJ9.e30.invalid"),
        ),
    ];

    for route in routes() {
        for (who, caller) in callers {
            let mut bodies = vec![(JSON, route.valid, "valid")];
            bodies.extend(
                route
                    .invalid
                    .iter()
                    .map(|i| (i.content_type, i.body, i.label)),
            );
            for (content_type, body, label) in bodies {
                let context = format!("{} {} as {who} with {label} body", route.method, route.uri);
                let (status, response) = send(
                    &router,
                    route.method.clone(),
                    route.uri,
                    content_type,
                    body,
                    caller,
                )
                .await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{context}");
                assert_no_schema_leak(&response, &context);
                assert_data_dir_unchanged(dir.path(), &before, &context);
            }
        }
    }
}

#[tokio::test]
async fn isolation_contract_forbidden_response_does_not_depend_on_body() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path());
    let router = router(dir.path());
    let seeded = b"seeded-override-must-stay-byte-identical";
    std::fs::write(dir.path().join("config_override.json"), seeded).unwrap();
    let before = files_under(dir.path());
    // Not a platform admin: a DA of an ordinary tenant.
    let tenant_da = token("tenant-a", &["DA"]);
    // Verified, but without DA.
    let no_da = token("tenant-a", &[]);

    for route in routes() {
        let caller = if route.platform_admin {
            &tenant_da
        } else {
            &no_da
        };
        let (status, expected) = send(
            &router,
            route.method.clone(),
            route.uri,
            JSON,
            route.valid,
            Caller::Bearer(caller),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{} valid body", route.uri);
        assert_no_schema_leak(&expected, route.uri);
        assert_data_dir_unchanged(dir.path(), &before, route.uri);
        for invalid in &route.invalid {
            let (status, body) = send(
                &router,
                route.method.clone(),
                route.uri,
                invalid.content_type,
                invalid.body,
                Caller::Bearer(caller),
            )
            .await;
            assert_eq!(
                (status, &body),
                (StatusCode::FORBIDDEN, &expected),
                "{} {}: 403 must be byte-identical to the valid-body 403",
                route.uri,
                invalid.label
            );
            assert_data_dir_unchanged(
                dir.path(),
                &before,
                &format!("{} {}", route.uri, invalid.label),
            );
        }
    }
}

#[tokio::test]
async fn isolation_contract_authorized_invalid_bodies_keep_json_rejections() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path());
    let router = router(dir.path());
    let platform_admin = token(PLATFORM_TENANT, &[PLATFORM_ADMIN_ROLE]);
    let da = token("tenant-a", &["DA"]);

    for route in routes() {
        let caller = if route.platform_admin {
            &platform_admin
        } else {
            &da
        };
        for invalid in &route.invalid {
            let (status, _) = send(
                &router,
                route.method.clone(),
                route.uri,
                invalid.content_type,
                invalid.body,
                Caller::Bearer(caller),
            )
            .await;
            assert_eq!(
                status, invalid.authorized_status,
                "{} {} as an authorized caller",
                route.uri, invalid.label
            );
        }
    }

    // A schema error is still descriptive for an authorized caller.
    let (status, body) = send(
        &router,
        Method::PUT,
        "/api/v1/config",
        JSON,
        r#"{"x":1}"#,
        Caller::Bearer(&platform_admin),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(String::from_utf8_lossy(&body).contains("unknown field"));

    // Valid bodies still reach the handler: unknown resource → 400.
    let (status, _) = send(
        &router,
        Method::POST,
        "/api/v1/models/test",
        JSON,
        r#"{"resource_id":"missing"}"#,
        Caller::Bearer(&platform_admin),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn isolation_contract_config_read_still_requires_verified_reader() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path());
    let router = router(dir.path());

    let (status, _) = send(
        &router,
        Method::GET,
        "/api/v1/config",
        JSON,
        "",
        Caller::Anonymous,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for reader in [
        token("tenant-a", &["DA"]),
        token(PLATFORM_TENANT, &[PLATFORM_ADMIN_ROLE]),
    ] {
        let (status, _) = send(
            &router,
            Method::GET,
            "/api/v1/config",
            JSON,
            "",
            Caller::Bearer(&reader),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}
