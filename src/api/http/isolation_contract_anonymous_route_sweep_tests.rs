//! Anonymous route sweep over the real `build_router`.
//!
//! Every `(method, path)` registered in `build_router` is parsed from the
//! router source (not a hand-maintained list) and called twice: with no
//! credentials and with a fake bearer key. Each call must answer 401 unless
//! the route is in `ANONYMOUS_ALLOWLIST`. `KNOWN_GAPS` is a temporary,
//! separate list of routes that still answer something else; it may only
//! shrink, and an entry that starts answering 401 fails the sweep so the list
//! is kept honest.

use std::time::Duration;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    routing::get,
    Router,
};
use tower::ServiceExt;

use super::{
    control_plane_route_auth_tests::{test_state, EnvGuard},
    iam::PLATFORM_ADMIN_TENANT_ENV,
    TEST_ENV_LOCK,
};

/// Routes that are intentionally reachable without credentials.
pub(crate) const ANONYMOUS_ALLOWLIST: &[(&str, &str)] = &[("GET", "/health")];

/// TEMPORARY (P2): routes known to answer anonymous calls, deferred to the
/// referenced follow-up issue. Kept separate from `ANONYMOUS_ALLOWLIST`; an
/// entry that starts answering 401 fails the sweep so it must be removed.
/// Never add an entry to make a new route pass.
const KNOWN_GAPS: &[(&str, &str, &str)] = &[
    ("GET", "/api/v1/skills", "issue: TBD-P2-skills-meta"),
    (
        "GET",
        "/api/v1/skills/manifest",
        "issue: TBD-P2-skills-meta",
    ),
    (
        "GET",
        "/api/v1/skills/pipeline-runs",
        "issue: TBD-P2-skills-meta",
    ),
    (
        "GET",
        "/api/v1/memory/unified-stats",
        "issue: TBD-P2-runtime-stats",
    ),
    (
        "GET",
        "/api/v1/ontology/types",
        "issue: TBD-P2-ontology-types",
    ),
    ("GET", "/metrics", "issue: TBD-P2-metrics"),
    // Prompt routes are tracked by #302 and deliberately untouched here.
    ("GET", "/api/v1/prompts", "issue: #302"),
    ("GET", "/api/v1/prompts/resolve", "issue: #302"),
];

/// A probe is "open" unless it was denied (401/403) or rejected by a request
/// extractor before the handler ran (400/413/415/422). Extractor rejections
/// are inconclusive for auth and are counted separately; see the PR notes.
fn is_open(status: &str) -> bool {
    !matches!(status, "401" | "403" | "400" | "413" | "415" | "422")
}

/// Query strings that let a probe get past a required `Query` extractor, so the
/// sweep observes the handler instead of a 400.
const PROBE_QUERIES: &[(&str, &str)] = &[("/api/v1/skills/manifest", "iri=skill://sweep/probe")];

const ROUTER_SOURCE: &str = include_str!("mod.rs");
const FAKE_KEY: &str = "wao_fake_0000000000000000000000000000";

/// Parse `(METHOD, path)` pairs from the `.route(...)` calls in `build_router`.
fn registered_routes() -> Vec<(String, String)> {
    let start = ROUTER_SOURCE
        .find("pub fn build_router(")
        .expect("build_router present");
    let body = &ROUTER_SOURCE[start..];
    let end = body
        .find(".with_state(state)")
        .expect("build_router ends with state");
    let body = &body[..end];
    let mut routes = Vec::new();
    let chunks: Vec<&str> = body.split(".route(").skip(1).collect();
    for chunk in chunks {
        let open = chunk.find('"').expect("route path literal");
        let close = open + 1 + chunk[open + 1..].find('"').expect("route path end");
        let path = chunk[open + 1..close].to_string();
        let rest = &chunk[close + 1..];
        let bytes = rest.as_bytes();
        for method in ["get", "post", "put", "delete", "patch"] {
            let needle = format!("{method}(");
            let mut from = 0;
            while let Some(pos) = rest[from..].find(&needle) {
                let at = from + pos;
                let prev = if at == 0 { b' ' } else { bytes[at - 1] };
                if !(prev.is_ascii_alphanumeric() || prev == b'_') {
                    routes.push((method.to_uppercase(), path.clone()));
                    break;
                }
                from = at + needle.len();
            }
        }
    }
    routes.sort();
    routes.dedup();
    routes
}

fn concrete_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with(':') || segment.starts_with('*') {
                uuid::Uuid::new_v4().hyphenated().to_string()
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn probe_uri(path: &str) -> String {
    let uri = concrete_path(path);
    match PROBE_QUERIES.iter().find(|(p, _)| *p == path) {
        Some((_, query)) => format!("{uri}?{query}"),
        None => uri,
    }
}

fn setup(dir: &std::path::Path) -> EnvGuard {
    EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        ("AGENTOS_AUTH_STRICT", "true".into()),
        (
            "AGENTOS_JWT_SECRET",
            "test-hs256-secret-at-least-32-bytes-long".into(),
        ),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
        ("MCP_JWT_SUBJECT", "route-sweep-test".into()),
        (PLATFORM_ADMIN_TENANT_ENV, "platform".into()),
        (
            "MCP_OUTBOUND_ALLOWED_ORIGINS",
            "https://example.invalid".into(),
        ),
    ])
}

fn real_router(dir: &std::path::Path) -> Router {
    let state = test_state(dir);
    super::build_router(
        state.core.clone(),
        state.gateway.clone(),
        state.kg_store.clone(),
        serde_json::json!({}),
        state.agents_info.clone(),
        state.vector_store.clone(),
        None,
        None,
        crate::config::OnlineCorpusWatcherSettings::default(),
        state.shutdown.clone(),
    )
}

async fn status_for(router: &Router, method: &str, path: &str, key: Option<&str>) -> String {
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(probe_uri(path))
        .header("content-type", "application/json");
    if let Some(key) = key {
        builder = builder
            .header("authorization", format!("Bearer {key}"))
            .header("x-api-key", key);
    }
    let body = if method == "GET" {
        Body::empty()
    } else {
        Body::from("{}")
    };
    match tokio::time::timeout(
        Duration::from_secs(10),
        router.clone().oneshot(builder.body(body).unwrap()),
    )
    .await
    {
        Ok(Ok(response)) => response.status().as_u16().to_string(),
        Ok(Err(error)) => format!("error:{error}"),
        Err(_) => "timeout".to_string(),
    }
}

/// Returns `METHOD path -> anon=<status> fake_key=<status>` for every
/// non-allowlisted route that does not answer 401 to both calls.
async fn sweep(router: &Router, routes: &[(String, String)]) -> Vec<(String, String, String)> {
    let mut unguarded = Vec::new();
    for (method, path) in routes {
        if ANONYMOUS_ALLOWLIST
            .iter()
            .any(|(m, p)| m == method && p == path)
        {
            continue;
        }
        let anonymous = status_for(router, method, path, None).await;
        let fake_key = status_for(router, method, path, Some(FAKE_KEY)).await;
        if is_open(&anonymous) || is_open(&fake_key) {
            unguarded.push((
                method.clone(),
                path.clone(),
                format!("anon={anonymous} fake_key={fake_key}"),
            ));
        }
    }
    unguarded
}

#[tokio::test]
async fn isolation_contract_anonymous_route_sweep() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let routes = registered_routes();
    assert!(
        routes.len() >= 100,
        "route parser found only {} routes",
        routes.len()
    );
    for (method, path, _) in KNOWN_GAPS {
        assert!(
            !ANONYMOUS_ALLOWLIST
                .iter()
                .any(|(m, p)| m == method && p == path),
            "{method} {path} is in both ANONYMOUS_ALLOWLIST and KNOWN_GAPS"
        );
        assert!(
            routes.iter().any(|(m, p)| m == method && p == path),
            "KNOWN_GAPS route {method} {path} is not registered"
        );
    }
    for (method, path) in ANONYMOUS_ALLOWLIST {
        assert!(
            routes.iter().any(|(m, p)| m == method && p == path),
            "allowlisted route {method} {path} is not registered"
        );
    }
    let router = real_router(dir.path());
    let unguarded = sweep(&router, &routes).await;
    let mut exact_401 = 0;
    for (method, path) in &routes {
        if status_for(&router, method, path, None).await == "401" {
            exact_401 += 1;
        }
    }
    eprintln!(
        "anonymous sweep: routes={} exact_401={} open={}",
        routes.len(),
        exact_401,
        unguarded.len()
    );

    let unexpected: Vec<String> = unguarded
        .iter()
        .filter(|(m, p, _)| !KNOWN_GAPS.iter().any(|(gm, gp, _)| gm == m && gp == p))
        .map(|(m, p, s)| format!("{m} {p} -> {s}"))
        .collect();
    let stale: Vec<String> = KNOWN_GAPS
        .iter()
        .filter(|(gm, gp, _)| !unguarded.iter().any(|(m, p, _)| gm == m && gp == p))
        .map(|(m, p, _)| format!("{m} {p}"))
        .collect();
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "routes={} unguarded routes not in the allowlist:\n{}\nKNOWN_GAPS entries that now answer 401 (remove them):\n{}",
        routes.len(),
        unexpected.join("\n"),
        stale.join("\n")
    );
}

/// Negative control: an unauthenticated route added to the real router must
/// be reported by the sweep.
#[tokio::test]
async fn isolation_contract_anonymous_route_sweep_catches_unguarded_route() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = real_router(dir.path()).route(
        "/api/v1/__sweep_canary/:id",
        get(|| async { StatusCode::OK }),
    );
    let routes = vec![("GET".to_string(), "/api/v1/__sweep_canary/:id".to_string())];
    let unguarded = sweep(&router, &routes).await;
    assert_eq!(unguarded.len(), 1);
    assert_eq!(unguarded[0].2, "anon=200 fake_key=200");
}

#[test]
fn isolation_contract_anonymous_route_sweep_parses_kb_documents_route() {
    let routes = registered_routes();
    for expected in [
        ("GET", "/api/v1/kb/bases/:id/documents"),
        ("GET", "/api/v1/knowledge-packs"),
        ("DELETE", "/api/v1/kb/categories/:id"),
        ("PUT", "/api/v1/kb/categories/:id"),
        ("POST", "/api/v1/public/agents/:id/chat"),
    ] {
        assert!(
            routes
                .iter()
                .any(|(m, p)| m == expected.0 && p == expected.1),
            "parser missed {expected:?}"
        );
    }
}

fn jwt_for(tenant: &str, roles: &[&str], project: &str) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &super::iam::JwtClaims {
            sub: format!("{tenant}-sweep-user"),
            tenant_id: tenant.into(),
            project_id: Some(project.into()),
            roles: roles.iter().map(|role| (*role).into()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &jsonwebtoken::EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
    )
    .unwrap()
}

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
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
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Knowledge packs and KB categories are platform-level shared objects:
/// tenant DA may list but not write; only a platform admin may write.
#[tokio::test]
async fn isolation_contract_kb_shared_registries_require_platform_admin_writes() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = real_router(dir.path());
    let admin = jwt_for("platform", &[super::iam::PLATFORM_ADMIN_ROLE], "ops");
    let tenant_da = jwt_for("tenant-a", &["DA"], "project-a");

    for (collection, list_key, item_key) in [
        ("/api/v1/kb/categories", "categories", "category"),
        (
            "/api/v1/knowledge-packs",
            "knowledge_packs",
            "knowledge_pack",
        ),
    ] {
        let (status, _) = call(
            &router,
            Method::POST,
            collection,
            &tenant_da,
            serde_json::json!({"name": "tenant-made"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{collection} DA create");

        let (status, created) = call(
            &router,
            Method::POST,
            collection,
            &admin,
            serde_json::json!({"name": "shared"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{collection} PA create");
        let id = created["id"].as_str().unwrap().to_string();
        let item = format!("{collection}/{id}");

        let (status, _) = call(
            &router,
            Method::PUT,
            &item,
            &tenant_da,
            serde_json::json!({"name": "hijacked"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{item} DA update");
        let (status, _) = call(
            &router,
            Method::DELETE,
            &item,
            &tenant_da,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{item} DA delete");

        // Tenant DA may read; data is unchanged.
        let (status, listed) = call(
            &router,
            Method::GET,
            collection,
            &tenant_da,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{collection} DA list");
        let entry = listed[list_key]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id.as_str())
            .cloned()
            .expect("entry survives DA delete");
        assert_eq!(entry["name"], "shared");

        let (status, updated) = call(
            &router,
            Method::PUT,
            &item,
            &admin,
            serde_json::json!({"name": "renamed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{item} PA update");
        assert_eq!(updated[item_key]["name"], "renamed");
        let (status, _) = call(
            &router,
            Method::DELETE,
            &item,
            &admin,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{item} PA delete");
    }
}

/// The batch SSE endpoint rejects anonymous callers before opening a stream.
#[tokio::test]
async fn isolation_contract_batch_events_sse_rejects_before_stream() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let router = real_router(dir.path());
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        router.clone().oneshot(
            Request::builder()
                .uri("/api/v1/batch/events")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("anonymous SSE must not hang")
    .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(!content_type.contains("text/event-stream"));
    let body = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("body is finite")
    .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("data:"));
}
