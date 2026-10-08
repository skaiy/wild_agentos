//! #302: write routes that used to check only `require_role("DA")`.
//!
//! Process-global state (prompt registry, skill registry) needs a platform
//! administrator. Tenant-scoped state (market installations, MCP skill
//! exposures, KB reindex) needs a control-plane DA: verified JWT claims with
//! an explicit project and the DA role. Another tenant's object answers 404.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    routing::{delete, get, post, put},
    Router,
};
use base64::Engine;
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{
    control_plane_route_auth_tests::{test_state, EnvGuard},
    iam::{JwtClaims, PLATFORM_ADMIN_ROLE, PLATFORM_ADMIN_TENANT_ENV},
    AppState, TEST_ENV_LOCK,
};
use crate::tools::prompt_registry::PromptVersion;

const SECRET: &str = "test-hs256-secret-at-least-32-bytes-long";

fn env(dir: &std::path::Path, strict: bool) -> EnvGuard {
    let mut vars = vec![
        ("AGENTOS_AUTH_MODE", "hs256".to_string()),
        ("AGENTOS_JWT_SECRET", SECRET.to_string()),
        ("AGENTOS_DATA_DIR", dir.to_string_lossy().into_owned()),
        (PLATFORM_ADMIN_TENANT_ENV, "platform".to_string()),
    ];
    if strict {
        vars.push(("AGENTOS_AUTH_STRICT", "true".to_string()));
    } else {
        vars.push(("AGENTOS_AUTH_STRICT", "false".to_string()));
    }
    EnvGuard::set(&vars)
}

fn token(tenant: &str, roles: &[&str], project: Option<&str>) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &JwtClaims {
            sub: format!("{tenant}-user"),
            tenant_id: tenant.into(),
            project_id: project.map(str::to_owned),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

/// Dev-only `X-Identity` header (non-strict mode): unverified, no claims.
fn x_identity(tenant: &str, roles: &[&str]) -> String {
    base64::engine::general_purpose::STANDARD.encode(
        json!({"user_id": format!("{tenant}-spoof"), "tenant_id": tenant, "roles": roles})
            .to_string(),
    )
}

enum Caller<'a> {
    Anonymous,
    Bearer(&'a str),
    XIdentity(&'a str),
}

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    caller: Caller<'_>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    match caller {
        Caller::Anonymous => {}
        Caller::Bearer(token) => {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        Caller::XIdentity(identity) => builder = builder.header("x-identity", identity),
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}

fn router(state: Arc<AppState>) -> Router {
    use super::{
        kb::reindex_knowledge_base_handler,
        market::{
            install_package_handler, publish_package_handler, rollback_package_handler,
            upgrade_package_handler,
        },
        mcp_skills::{
            delete_skill_exposure_handler, list_skill_exposures_handler,
            upsert_skill_exposure_handler,
        },
        prompts::{
            activate_prompt_handler, canary_prompt_handler, create_prompt_handler,
            delete_prompt_handler,
        },
        skills::{
            delete_skill_handler, import_git_skill_handler, pipeline_rerun_handler,
            register_skill_handler,
        },
    };
    Router::new()
        .route("/api/v1/prompts", post(create_prompt_handler))
        .route(
            "/api/v1/prompts/:id/activate",
            post(activate_prompt_handler),
        )
        .route("/api/v1/prompts/:id/canary", put(canary_prompt_handler))
        .route("/api/v1/prompts/:id", delete(delete_prompt_handler))
        .route(
            "/api/v1/skills",
            post(register_skill_handler).delete(delete_skill_handler),
        )
        .route("/api/v1/skills/import-git", post(import_git_skill_handler))
        .route(
            "/api/v1/skills/pipeline-rerun",
            post(pipeline_rerun_handler),
        )
        .route("/api/v1/market/packages", post(publish_package_handler))
        .route(
            "/api/v1/market/packages/:name/install",
            post(install_package_handler),
        )
        .route(
            "/api/v1/market/packages/:name/rollback",
            post(rollback_package_handler),
        )
        .route(
            "/api/v1/market/packages/:name/upgrade",
            post(upgrade_package_handler),
        )
        .route(
            "/api/v1/mcp/skill-exposures",
            get(list_skill_exposures_handler)
                .post(upsert_skill_exposure_handler)
                .delete(delete_skill_exposure_handler),
        )
        .route(
            "/api/v1/kb/bases/:id/reindex",
            post(reindex_knowledge_base_handler),
        )
        .with_state(state)
}

fn prompt_body() -> Value {
    json!({"name": "p", "template": "hello {{tenant_id}}", "model": "m", "version": "1.0.0"})
}

fn skill_body(iri: &str) -> Value {
    json!({
        "skill_iri": iri, "name": "s", "description": "d", "version": "1.0.0",
        "category": "test", "security_level": "standard", "allowed_roles": ["DA"],
        "input_schema": {"type": "object"}, "output_schema": {"type": "object"},
        "compiled_template": "{{x}}"
    })
}

/// Prompt and skill registries are process-global: tenant DA (verified,
/// explicit project) gets 403 `platform_admin_required`, the dev X-Identity
/// DA gets 401, anonymous gets 401, and none of them change the registry.
#[tokio::test]
async fn isolation_contract_global_registry_writes_require_platform_admin() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let seeded = state
        .prompts
        .add_version(PromptVersion::new("seed", "t", "m", "0.0.1", ""));
    let app = router(state.clone());

    let tenant_da = token("tenant-a", &["DA"], Some("project-a"));
    let spoofed_da = x_identity("tenant-a", &["DA"]);
    let platform_admin = token("platform", &[PLATFORM_ADMIN_ROLE], Some("ops"));

    let routes = [
        (Method::POST, "/api/v1/prompts".to_string(), prompt_body()),
        (
            Method::POST,
            format!("/api/v1/prompts/{seeded}/activate"),
            Value::Null,
        ),
        (
            Method::PUT,
            format!("/api/v1/prompts/{seeded}/canary"),
            json!({"percent": 50}),
        ),
        (
            Method::DELETE,
            format!("/api/v1/prompts/{seeded}"),
            Value::Null,
        ),
        (
            Method::POST,
            "/api/v1/skills".to_string(),
            skill_body("skill://tenant/write"),
        ),
        (
            Method::DELETE,
            "/api/v1/skills?iri=skill://tenant/write".to_string(),
            Value::Null,
        ),
        (
            Method::POST,
            "/api/v1/skills/import-git".to_string(),
            json!({"repo_url": ""}),
        ),
        (
            Method::POST,
            "/api/v1/skills/pipeline-rerun".to_string(),
            json!({"skill_iri": "skill://tenant/write"}),
        ),
    ];
    let snapshot = |state: &AppState| {
        (
            serde_json::to_value(state.prompts.list_versions()).unwrap(),
            state.prompts.active_id(),
            state.core.skills.skill_count(),
        )
    };
    let before = snapshot(&state);
    for (method, uri, body) in &routes {
        let (status, _) = call(&app, method.clone(), uri, Caller::Anonymous, body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous {method} {uri}");
        let (status, _) = call(
            &app,
            method.clone(),
            uri,
            Caller::XIdentity(&spoofed_da),
            body.clone(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "X-Identity DA {method} {uri}"
        );
        let (status, error) = call(
            &app,
            method.clone(),
            uri,
            Caller::Bearer(&tenant_da),
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "tenant DA {method} {uri}");
        assert_eq!(error["error"], "platform_admin_required", "{method} {uri}");
        assert_eq!(snapshot(&state), before, "{method} {uri} changed state");
    }

    // Positive control: the platform administrator passes every gate.
    let (status, created) = call(
        &app,
        Method::POST,
        "/api/v1/prompts",
        Caller::Bearer(&platform_admin),
        prompt_body(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created = created["id"].as_str().unwrap().to_string();
    for (method, uri, body, expected) in [
        (
            Method::POST,
            format!("/api/v1/prompts/{created}/activate"),
            Value::Null,
            StatusCode::OK,
        ),
        (
            Method::PUT,
            format!("/api/v1/prompts/{created}/canary"),
            json!({"percent": 10}),
            StatusCode::OK,
        ),
        (
            Method::DELETE,
            format!("/api/v1/prompts/{seeded}"),
            Value::Null,
            StatusCode::OK,
        ),
        (
            Method::POST,
            "/api/v1/skills".to_string(),
            skill_body("skill://platform/ok"),
            StatusCode::CREATED,
        ),
        (
            Method::POST,
            "/api/v1/skills/pipeline-rerun".to_string(),
            json!({"skill_iri": "skill://platform/ok"}),
            StatusCode::OK,
        ),
        (
            Method::POST,
            "/api/v1/skills/import-git".to_string(),
            json!({"repo_url": ""}),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::DELETE,
            "/api/v1/skills?iri=skill://platform/ok".to_string(),
            Value::Null,
            StatusCode::OK,
        ),
    ] {
        let (status, body) = call(
            &app,
            method.clone(),
            &uri,
            Caller::Bearer(&platform_admin),
            body,
        )
        .await;
        assert_eq!(status, expected, "platform admin {method} {uri}: {body}");
    }
}

fn publish_body(name: &str, version: &str) -> Value {
    json!({
        "name": name, "version": version, "input_schema": {"type": "object"},
        "output_schema": {"type": "object"}, "side_effect_level": "none",
        "visibility": "private"
    })
}

/// Market writes are tenant/project scoped: no claims → 401, DA without an
/// explicit project → 403, verified user without DA → 403, another tenant's
/// private package → 404 (same body as a missing version).
#[tokio::test]
async fn isolation_contract_market_writes_require_control_plane_da() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let app = router(test_state(dir.path()));

    let owner = token("tenant-b", &["DA"], Some("project-b"));
    let da_a = token("tenant-a", &["DA"], Some("project-a"));
    let da_defaulted = token("tenant-a", &["DA"], None);
    let no_da = token("tenant-a", &[], Some("project-a"));
    let spoofed_da = x_identity("tenant-b", &["DA"]);

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/v1/market/packages",
        Caller::Bearer(&owner),
        publish_body("logic", "1.0.0"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let routes = [
        (
            "/api/v1/market/packages".to_string(),
            publish_body("logic", "2.0.0"),
        ),
        (
            "/api/v1/market/packages/logic/install".to_string(),
            json!({"version": "1.0.0"}),
        ),
        (
            "/api/v1/market/packages/logic/rollback".to_string(),
            json!({}),
        ),
        (
            "/api/v1/market/packages/logic/upgrade".to_string(),
            json!({"version": "1.0.0"}),
        ),
    ];
    for (uri, body) in &routes {
        for caller in [Caller::Anonymous, Caller::XIdentity(&spoofed_da)] {
            let (status, _) = call(&app, Method::POST, uri, caller, body.clone()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        }
        let (status, error) = call(
            &app,
            Method::POST,
            uri,
            Caller::Bearer(&da_defaulted),
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "defaulted project {uri}");
        assert_eq!(error["error"], "control_plane_claims_incomplete", "{uri}");
        let (status, _) = call(
            &app,
            Method::POST,
            uri,
            Caller::Bearer(&no_da),
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "no DA {uri}");
    }

    // Cross-tenant install of a private package: same 404 as a missing one.
    let (foreign_status, foreign) = call(
        &app,
        Method::POST,
        "/api/v1/market/packages/logic/install",
        Caller::Bearer(&da_a),
        json!({"version": "1.0.0"}),
    )
    .await;
    let (missing_status, missing) = call(
        &app,
        Method::POST,
        "/api/v1/market/packages/logic/install",
        Caller::Bearer(&da_a),
        json!({"version": "9.9.9"}),
    )
    .await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign, missing);

    // Positive control: the owner installs its own package.
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/v1/market/packages/logic/install",
        Caller::Bearer(&owner),
        json!({"version": "1.0.0"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// MCP skill exposures belong to the verified tenant. In non-strict mode the
/// old role-only check let an unverified X-Identity header pick any tenant.
#[tokio::test]
async fn isolation_contract_skill_exposure_writes_use_verified_tenant() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let app = router(test_state(dir.path()));
    let exposure = super::mcp_skills::McpSkillExposure {
        tenant_id: "tenant-b".into(),
        skill_iri: "skill://b/weather".into(),
        tool_name: "weather".into(),
        enabled: true,
    };
    let path = dir.path().join("mcp_skill_exposures.json");
    std::fs::write(&path, serde_json::to_string(&vec![exposure]).unwrap()).unwrap();
    let stored = || std::fs::read_to_string(&path).unwrap();
    let before = stored();

    let delete_uri = "/api/v1/mcp/skill-exposures?skill_iri=skill://b/weather";
    let upsert_body = json!({"skill_iri": "skill://b/weather", "tool_name": "hijack"});
    let spoofed_b = x_identity("tenant-b", &["DA"]);
    let da_defaulted = token("tenant-b", &["DA"], None);
    let da_a = token("tenant-a", &["DA"], Some("project-a"));

    for (method, body) in [
        (Method::DELETE, Value::Null),
        (Method::POST, upsert_body.clone()),
    ] {
        let uri = if method == Method::DELETE {
            delete_uri
        } else {
            "/api/v1/mcp/skill-exposures"
        };
        for caller in [Caller::Anonymous, Caller::XIdentity(&spoofed_b)] {
            let (status, _) = call(&app, method.clone(), uri, caller, body.clone()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
        let (status, error) = call(
            &app,
            method.clone(),
            uri,
            Caller::Bearer(&da_defaulted),
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(error["error"], "control_plane_claims_incomplete");
        assert_eq!(stored(), before, "{method} {uri} changed exposures");
    }

    // Listing is scoped to the verified tenant.
    let list_uri = "/api/v1/mcp/skill-exposures";
    for caller in [Caller::Anonymous, Caller::XIdentity(&spoofed_b)] {
        let (status, _) = call(&app, Method::GET, list_uri, caller, Value::Null).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, listed) = call(
        &app,
        Method::GET,
        list_uri,
        Caller::Bearer(&da_a),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 0, "tenant-a saw tenant-b's exposure");
    let owner_list = token("tenant-b", &["DA"], Some("project-b"));
    let (status, listed) = call(
        &app,
        Method::GET,
        list_uri,
        Caller::Bearer(&owner_list),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 1);

    // Another tenant's DA cannot remove tenant-b's exposure: 404, unchanged.
    let (status, _) = call(
        &app,
        Method::DELETE,
        delete_uri,
        Caller::Bearer(&da_a),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(stored(), before);

    // Positive control: tenant-b's own control-plane DA removes it.
    let owner = token("tenant-b", &["DA"], Some("project-b"));
    let (status, _) = call(
        &app,
        Method::DELETE,
        delete_uri,
        Caller::Bearer(&owner),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_ne!(stored(), before);
}

/// KB reindex: another tenant's knowledge base answers 404 (as if missing)
/// and is never marked as reindexing.
#[tokio::test]
async fn isolation_contract_kb_reindex_requires_owner_control_plane_da() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let kb = json!({
        "id": "kb-b", "tenant_id": "tenant-b", "project_id": "project-b", "kb_type": "vector",
        "documents": [{"doc_id": "d1", "filename": "a.txt"}]
    });
    state.knowledge_bases.write().await.push(kb.clone());
    let app = router(state.clone());
    let uri = "/api/v1/kb/bases/kb-b/reindex";

    let spoofed_b = x_identity("tenant-b", &["DA"]);
    for caller in [Caller::Anonymous, Caller::XIdentity(&spoofed_b)] {
        let (status, _) = call(&app, Method::POST, uri, caller, Value::Null).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, error) = call(
        &app,
        Method::POST,
        uri,
        Caller::Bearer(&token("tenant-b", &["DA"], None)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"], "control_plane_claims_incomplete");

    let da_a = token("tenant-a", &["DA"], Some("project-a"));
    let (foreign_status, foreign) =
        call(&app, Method::POST, uri, Caller::Bearer(&da_a), Value::Null).await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    let (missing_status, missing) = call(
        &app,
        Method::POST,
        "/api/v1/kb/bases/kb-missing/reindex",
        Caller::Bearer(&da_a),
        Value::Null,
    )
    .await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign["error"], missing["error"]);
    assert_eq!(state.knowledge_bases.read().await[0], kb, "KB was touched");

    // Same tenant, another project: also 404 as if missing, KB untouched.
    let other_project = token("tenant-b", &["DA"], Some("project-other"));
    let (status, same_tenant) = call(
        &app,
        Method::POST,
        uri,
        Caller::Bearer(&other_project),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(same_tenant["error"], missing["error"]);
    assert_eq!(state.knowledge_bases.read().await[0], kb, "KB was touched");

    // Positive control: the owner reaches the handler body (no vector store
    // in the test state → 503), proving the gate admits it.
    let owner = token("tenant-b", &["DA"], Some("project-b"));
    let (status, _) = call(&app, Method::POST, uri, Caller::Bearer(&owner), Value::Null).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
