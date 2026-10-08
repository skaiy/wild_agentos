//! Anonymous route sweep over the real `build_router`.
//!
//! Every `(method, path)` registered in `build_router` is parsed from the
//! router source (not a hand-maintained list) and called twice: with no
//! credentials and with a fake bearer key. Only 401 counts as protected.
//! `ANONYMOUS_ALLOWLIST` is intentionally public. `KNOWN_GAPS` (open routes)
//! and `INCONCLUSIVE` (extractor rejects before auth) are temporary, disjoint
//! lists that may only shrink; an entry whose status changes fails the sweep.

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
    ("GET", "/metrics", "issue: #324"),
    // Prompt reads stay open for now; #302 gated the prompt writes only.
    ("GET", "/api/v1/prompts", "issue: #302"),
    ("GET", "/api/v1/prompts/resolve", "issue: #302"),
];

/// TEMPORARY: routes whose anonymous probe is rejected by a request extractor
/// (400/413/415/422) before the handler runs, so the probe cannot show whether
/// the handler authenticates. Resolved later by a default-deny layer. May only
/// shrink: an entry that starts answering 401 fails the sweep, and an unlisted
/// route that answers 400/422 fails too.
const INCONCLUSIVE: &[(&str, &str)] = &[
    ("DELETE", "/api/v1/mcp/skill-exposures"),
    ("DELETE", "/api/v1/skills"),
    ("GET", "/api/v1/blackboard/nodes"),
    ("GET", "/api/v1/ontology/constrained-extractions/:id"),
    ("POST", "/api/v1/agents"),
    ("POST", "/api/v1/agents/:id/chat"),
    ("POST", "/api/v1/api-clients"),
    ("POST", "/api/v1/artifacts"),
    ("POST", "/api/v1/batch/agents/:name/control"),
    ("POST", "/api/v1/embedding/activate"),
    ("POST", "/api/v1/events"),
    ("POST", "/api/v1/images/upload"),
    ("POST", "/api/v1/kb/bases"),
    ("POST", "/api/v1/kb/bases/:id/import-graph"),
    ("POST", "/api/v1/kb/bases/:id/materialize-rml"),
    ("POST", "/api/v1/kb/bases/:id/search"),
    ("POST", "/api/v1/kb/bases/:id/upload"),
    ("POST", "/api/v1/kb/categories"),
    ("POST", "/api/v1/kg/import"),
    ("POST", "/api/v1/kg/query"),
    ("POST", "/api/v1/knowledge-packs"),
    ("POST", "/api/v1/market/packages"),
    ("POST", "/api/v1/market/packages/:name/install"),
    ("POST", "/api/v1/market/packages/:name/upgrade"),
    ("POST", "/api/v1/mcp/servers"),
    ("POST", "/api/v1/mcp/servers/invoke"),
    ("POST", "/api/v1/mcp/skill-exposures"),
    ("POST", "/api/v1/nodes"),
    ("POST", "/api/v1/online-corpus-jobs"),
    ("POST", "/api/v1/online-corpus-jobs/:id/run"),
    ("POST", "/api/v1/ontology/action-types"),
    ("POST", "/api/v1/ontology/constrained-extractions"),
    (
        "POST",
        "/api/v1/ontology/constrained-extractions/:id/materialize",
    ),
    ("POST", "/api/v1/ontology/entity-resolution/suggestions"),
    ("POST", "/api/v1/ontology/function-defs"),
    ("POST", "/api/v1/ontology/link-types"),
    ("POST", "/api/v1/ontology/object-types"),
    ("POST", "/api/v1/ontology/readiness-report"),
    ("POST", "/api/v1/ontology/type-drafts/:draft_id/promote"),
    ("POST", "/api/v1/ontology/type-drafts/from-csv"),
    ("POST", "/api/v1/ontology/type-drafts/from-induction"),
    ("POST", "/api/v1/ontology/type-drafts/from-json-schema"),
    ("POST", "/api/v1/ontology/type-drafts/from-openapi"),
    ("POST", "/api/v1/ontology/type-drafts/from-sql-ddl"),
    ("POST", "/api/v1/projections"),
    ("POST", "/api/v1/prompts"),
    ("POST", "/api/v1/public/agents/:id/chat"),
    ("POST", "/api/v1/public/agents/:id/chat/stream"),
    ("POST", "/api/v1/skills"),
    ("POST", "/api/v1/skills/import-git"),
    ("POST", "/api/v1/skills/pipeline-rerun"),
    ("POST", "/mcp"),
    ("POST", "/v1/chat/completions"),
    ("PUT", "/api/v1/ontology/action-types/:id"),
    ("PUT", "/api/v1/ontology/function-defs/:id"),
    ("PUT", "/api/v1/ontology/link-types/:id"),
    ("PUT", "/api/v1/ontology/object-types/:id"),
    ("PUT", "/api/v1/prompts/:id/canary"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeClass {
    /// Both probes answered 401.
    Protected,
    /// No probe was open, but at least one was an extractor rejection.
    Inconclusive,
    /// Any other answer, including 403 (only 401 counts as protected).
    Open,
}

fn classify(status: &str) -> ProbeClass {
    match status {
        "401" => ProbeClass::Protected,
        "400" | "413" | "415" | "422" => ProbeClass::Inconclusive,
        _ => ProbeClass::Open,
    }
}

/// Query strings that let a probe get past a required `Query` extractor, so the
/// sweep observes the handler instead of a 400.
const PROBE_QUERIES: &[(&str, &str)] = &[("/api/v1/skills/manifest", "iri=skill://sweep/probe")];

/// Minimal valid bodies so a probe gets past the JSON extractor and reaches
/// the handler's auth check (otherwise the route would be inconclusive).
const PROBE_BODIES: &[(&str, &str, &str)] = &[
    ("POST", "/api/v1/tasks", r#"{"user_input":"sweep probe"}"#),
    (
        "POST",
        "/api/v1/tasks/stream",
        r#"{"prompt":"sweep probe"}"#,
    ),
];

/// Routes that must stay protected (401 for both probes). Guards against a
/// regression being absorbed by a status change into another list.
const MUST_BE_PROTECTED: &[(&str, &str)] = &[
    ("POST", "/api/v1/tasks"),
    ("POST", "/api/v1/tasks/stream"),
    ("GET", "/api/v1/kb/bases/:id/documents"),
    ("GET", "/api/v1/batch/events"),
    ("POST", "/v1/invocations"),
    ("GET", "/v1/invocations"),
    ("GET", "/v1/invocations/:id"),
    ("POST", "/v1/invocations/:id/cancel"),
    ("GET", "/v1/invocations/:id/events"),
];

const ROUTER_SOURCE: &str = include_str!("mod.rs");
const FAKE_KEY: &str = "wao_fake_0000000000000000000000000000";

/// The `Router::new() ... .with_state(state)` chain of `build_router`, plus
/// whatever follows `.with_state(state)` up to the end of the function.
fn router_chain(source: &str) -> (&str, &str) {
    let start = source
        .find("pub fn build_router(")
        .expect("build_router present");
    let body = &source[start..];
    let chain_start = body
        .find("Router::new()")
        .expect("Router::new() in build_router");
    let body = &body[chain_start..];
    let end = body
        .find(".with_state(state)")
        .expect("build_router calls .with_state(state)");
    let after = &body[end + ".with_state(state)".len()..];
    (&body[..end], after)
}

fn has_call(chain: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    let bytes = chain.as_bytes();
    let mut from = 0;
    while let Some(pos) = chain[from..].find(&needle) {
        let at = from + pos;
        let prev = if at == 0 { b' ' } else { bytes[at - 1] };
        if !(prev.is_ascii_alphanumeric() || prev == b'_') {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// Constructs the route parser does not understand. Any of them would let a
/// route escape the sweep, so the guard fails instead.
fn router_source_violations(source: &str) -> Vec<String> {
    let (chain, after) = router_chain(source);
    let mut violations = Vec::new();
    for token in [".merge(", ".nest(", "route_service(", ".fallback("] {
        if chain.contains(token) {
            violations.push(format!("build_router uses {token}"));
        }
    }
    for name in ["any", "on"] {
        if has_call(chain, name) {
            violations.push(format!("build_router uses {name}(...)"));
        }
    }
    if chain.contains("_service(") {
        violations.push("build_router uses a *_service(...) call".to_string());
    }
    if after.trim_start().chars().next() != Some('}') {
        violations.push(".with_state(state) is not the last call in build_router".to_string());
    }
    violations
}

/// Parse `(METHOD, path)` pairs from the `.route(...)` calls in `build_router`.
/// Returns the routes and the paths whose `.route(` block had no method.
fn parse_routes(source: &str) -> (Vec<(String, String)>, Vec<String>) {
    let (chain, _) = router_chain(source);
    let mut routes = Vec::new();
    let mut unparsed = Vec::new();
    for chunk in chain.split(".route(").skip(1) {
        let open = chunk.find('"').expect("route path literal");
        let close = open + 1 + chunk[open + 1..].find('"').expect("route path end");
        let path = chunk[open + 1..close].to_string();
        let rest = &chunk[close + 1..];
        let mut found = false;
        for method in ["get", "post", "put", "delete", "patch"] {
            if has_call(rest, method) {
                routes.push((method.to_uppercase(), path.clone()));
                found = true;
            }
        }
        if !found {
            unparsed.push(path);
        }
    }
    routes.sort();
    routes.dedup();
    (routes, unparsed)
}

fn registered_routes() -> Vec<(String, String)> {
    let (routes, unparsed) = parse_routes(ROUTER_SOURCE);
    assert!(
        unparsed.is_empty(),
        "route blocks with no parsed method: {unparsed:?}"
    );
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
    } else if let Some((_, _, body)) = PROBE_BODIES
        .iter()
        .find(|(m, p, _)| *m == method && *p == path)
    {
        Body::from(*body)
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

/// Probes every non-allowlisted route anonymously and with a fake key.
/// Returns `(method, path, class, "anon=<status> fake_key=<status>")`.
async fn sweep(
    router: &Router,
    routes: &[(String, String)],
) -> Vec<(String, String, ProbeClass, String)> {
    let mut results = Vec::new();
    for (method, path) in routes {
        if ANONYMOUS_ALLOWLIST
            .iter()
            .any(|(m, p)| m == method && p == path)
        {
            continue;
        }
        let anonymous = status_for(router, method, path, None).await;
        let fake_key = status_for(router, method, path, Some(FAKE_KEY)).await;
        let classes = [classify(&anonymous), classify(&fake_key)];
        let class = if classes.contains(&ProbeClass::Open) {
            ProbeClass::Open
        } else if classes.contains(&ProbeClass::Inconclusive) {
            ProbeClass::Inconclusive
        } else {
            ProbeClass::Protected
        };
        results.push((
            method.clone(),
            path.clone(),
            class,
            format!("anon={anonymous} fake_key={fake_key}"),
        ));
    }
    results
}

fn listed(list: &[(&str, &str)], method: &str, path: &str) -> bool {
    list.iter().any(|(m, p)| *m == method && *p == path)
}

fn known_gap(method: &str, path: &str) -> bool {
    KNOWN_GAPS
        .iter()
        .any(|(m, p, _)| *m == method && *p == path)
}

#[test]
fn isolation_contract_anonymous_route_sweep_router_source_guard() {
    let violations = router_source_violations(ROUTER_SOURCE);
    assert!(violations.is_empty(), "{violations:?}");
    let (_, unparsed) = parse_routes(ROUTER_SOURCE);
    assert!(
        unparsed.is_empty(),
        "route blocks with no parsed method: {unparsed:?}"
    );
}

#[test]
fn isolation_contract_anonymous_route_sweep_guard_rejects_unparsed_constructs() {
    let (chain, _) = router_chain(ROUTER_SOURCE);
    let original = format!("{chain}.with_state(state)");
    for canary in [
        ".route(\"/api/v1/__canary_any\", any(health_handler))",
        ".merge(Router::new())",
        ".nest(\"/x\", Router::new())",
        ".route_service(\"/x\", svc)",
        ".fallback(health_handler)",
        ".route(\"/x\", on(MethodFilter::GET, health_handler))",
        ".route(\"/x\", get_service(svc))",
    ] {
        let mutated =
            ROUTER_SOURCE.replacen(&original, &format!("{chain}{canary}.with_state(state)"), 1);
        assert_ne!(mutated, ROUTER_SOURCE);
        assert!(
            !router_source_violations(&mutated).is_empty(),
            "guard missed {canary}"
        );
    }
    let trailing = ROUTER_SOURCE.replacen(
        &original,
        &format!("{original}.layer(tower::layer::util::Identity::new())"),
        1,
    );
    assert!(!router_source_violations(&trailing).is_empty());
    let unparsed = ROUTER_SOURCE.replacen(
        &original,
        &format!("{chain}.route(\"/api/v1/__canary_any\", any(health_handler)).with_state(state)"),
        1,
    );
    assert_eq!(
        parse_routes(&unparsed).1,
        vec!["/api/v1/__canary_any".to_string()]
    );
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
    let registered = |m: &str, p: &str| routes.iter().any(|(rm, rp)| rm == m && rp == p);
    for (method, path) in ANONYMOUS_ALLOWLIST {
        assert!(
            registered(method, path),
            "allowlisted {method} {path} not registered"
        );
    }
    for (method, path, _) in KNOWN_GAPS {
        assert!(
            registered(method, path),
            "KNOWN_GAPS {method} {path} not registered"
        );
        assert!(
            !listed(ANONYMOUS_ALLOWLIST, method, path),
            "{method} {path} in ANONYMOUS_ALLOWLIST and KNOWN_GAPS"
        );
    }
    for (method, path) in INCONCLUSIVE {
        assert!(
            registered(method, path),
            "INCONCLUSIVE {method} {path} not registered"
        );
        assert!(
            !listed(ANONYMOUS_ALLOWLIST, method, path) && !known_gap(method, path),
            "{method} {path} in INCONCLUSIVE and another list"
        );
    }

    for (method, path) in MUST_BE_PROTECTED {
        assert!(
            registered(method, path),
            "MUST_BE_PROTECTED {method} {path} not registered"
        );
        assert!(
            !listed(ANONYMOUS_ALLOWLIST, method, path)
                && !known_gap(method, path)
                && !listed(INCONCLUSIVE, method, path),
            "MUST_BE_PROTECTED {method} {path} is also in another list"
        );
    }

    let router = real_router(dir.path());
    let results = sweep(&router, &routes).await;
    for (method, path) in MUST_BE_PROTECTED {
        let result = results
            .iter()
            .find(|(m, p, _, _)| m == method && p == path)
            .expect("protected route was probed");
        assert_eq!(
            result.2,
            ProbeClass::Protected,
            "{method} {path} must answer 401: {}",
            result.3
        );
    }
    let count = |class: ProbeClass| results.iter().filter(|r| r.2 == class).count();
    eprintln!(
        "anonymous sweep: routes={} protected_401={} inconclusive={} open={}",
        routes.len(),
        count(ProbeClass::Protected),
        count(ProbeClass::Inconclusive),
        count(ProbeClass::Open)
    );

    let mut failures = Vec::new();
    for (method, path, class, statuses) in &results {
        let gap = known_gap(method, path);
        let inconclusive = listed(INCONCLUSIVE, method, path);
        match class {
            ProbeClass::Open if !gap => failures.push(format!(
                "open route not in KNOWN_GAPS: {method} {path} -> {statuses}"
            )),
            ProbeClass::Inconclusive if !inconclusive => failures.push(format!(
                "inconclusive route not in INCONCLUSIVE: {method} {path} -> {statuses}"
            )),
            ProbeClass::Protected if gap || inconclusive => failures.push(format!(
                "now protected, remove from KNOWN_GAPS/INCONCLUSIVE: {method} {path}"
            )),
            ProbeClass::Inconclusive if gap => failures.push(format!(
                "KNOWN_GAPS entry no longer open: {method} {path} -> {statuses}"
            )),
            _ => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
    let results = sweep(&router, &routes).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].2, ProbeClass::Open);
    assert_eq!(results[0].3, "anon=200 fake_key=200");
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
