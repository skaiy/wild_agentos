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
            delete_skill_exposure_handler, list_skill_exposures_handler, skill_mcp_handler,
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
        .route("/mcp", post(skill_mcp_handler))
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
        project_id: Some("project-b".into()),
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

    // Listing uses the same control-plane DA gate as writes. A verified token
    // whose project was defaulted is not an explicit project scope.
    let list_uri = "/api/v1/mcp/skill-exposures";
    for caller in [Caller::Anonymous, Caller::XIdentity(&spoofed_b)] {
        let (status, _) = call(&app, Method::GET, list_uri, caller, Value::Null).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, error) = call(
        &app,
        Method::GET,
        list_uri,
        Caller::Bearer(&da_defaulted),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"], "control_plane_claims_incomplete");
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

fn publish_exposed_skill(state: &AppState, iri: &str, tenant_id: &str, project_id: &str) {
    publish_exposed_skill_meta(
        state,
        iri,
        tenant_id,
        project_id,
        "Read weather",
        json!({"type": "object"}),
    );
}

fn publish_exposed_skill_meta(
    state: &AppState,
    iri: &str,
    tenant_id: &str,
    project_id: &str,
    description: &str,
    input_schema: Value,
) {
    use crate::tools::skill_pipeline::{PipelineRun, PipelineSource, SkillVisibility};
    use crate::tools::skill_registry::SkillMeta;

    state.core.skills.register_skill(SkillMeta {
        skill_iri: iri.into(),
        name: "weather".into(),
        description: description.into(),
        version: "1.0.0".into(),
        category: "weather".into(),
        security_level: "normal".into(),
        allowed_roles: vec!["DA".into()],
        input_schema,
        output_schema: json!({"type": "object"}),
        compiled_template: "{}".into(),
        signature: None,
        signature_algorithm: None,
        input_mapping: Default::default(),
        output_mapping: Default::default(),
        skill_types: vec![],
    });
    super::skills::append_pipeline_run(&PipelineRun {
        run_id: format!("run-{iri}"),
        skill_iri: iri.into(),
        skill_name: "weather".into(),
        version: "1.0.0".into(),
        source: PipelineSource::Manual,
        visibility: SkillVisibility::Tenant,
        tenant_promotion_review: None,
        triggered_by: "da".into(),
        repo_url: None,
        started_at: "2026-01-01T00:00:00Z".into(),
        duration_ms: 1,
        stages: vec![],
        gate_passed: true,
        published: true,
        summary: "published".into(),
        publisher_tenant_id: Some(tenant_id.into()),
        publisher_project_id: Some(project_id.into()),
    })
    .unwrap();
}

fn exposure_rows(dir: &std::path::Path) -> Vec<Value> {
    let text = std::fs::read_to_string(dir.join("mcp_skill_exposures.json"))
        .unwrap_or_else(|_| "[]".into());
    serde_json::from_str(&text).unwrap_or_default()
}

/// #384: an exposure belongs to one verified project. Another project in the
/// same tenant cannot list, overwrite, delete, or call it. Removing the
/// project predicate from list, upsert, delete, or MCP lookup turns this red.
#[tokio::test]
async fn isolation_contract_skill_exposure_is_isolated_per_project() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://acme/weather";
    publish_exposed_skill(&state, iri, "tenant-a", "project-a");
    let app = router(state);
    let da_a = token("tenant-a", &["DA"], Some("project-a"));
    let da_b = token("tenant-a", &["DA"], Some("project-b"));
    let create = json!({"skill_iri": iri, "tool_name": "weather.lookup", "enabled": true});

    let (status, created) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_a),
        create,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["exposure"]["tenant_id"], "tenant-a");
    assert_eq!(created["exposure"]["project_id"], "project-a");
    assert_eq!(created["exposure"]["tool_name"], "weather.lookup");
    assert_eq!(created["exposure"]["enabled"], true);

    let (status, listed) = call(
        &app,
        Method::GET,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_b),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 0, "project B listed project A's exposure");
    assert!(listed["exposures"].as_array().unwrap().is_empty());

    let (status, listed) = call(
        &app,
        Method::GET,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_a),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["exposures"][0]["project_id"], "project-a");

    let list_rpc = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let call_rpc = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "weather.lookup", "arguments": {}}
    });
    let (status, tools) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&da_a),
        list_rpc.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["result"]["tools"][0]["name"], "weather.lookup");
    let (status, called) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&da_a),
        call_rpc.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{called}");
    assert_eq!(called["result"]["content"][0]["json"]["status"], "accepted");

    let (status, tools) = call(&app, Method::POST, "/mcp", Caller::Bearer(&da_b), list_rpc).await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(
        tools["result"]["tools"].as_array().unwrap().len(),
        0,
        "project B's MCP listed project A's tool"
    );
    let (status, missing_tool) =
        call(&app, Method::POST, "/mcp", Caller::Bearer(&da_b), call_rpc).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing_tool}");

    let delete_uri = format!("/api/v1/mcp/skill-exposures?skill_iri={iri}");
    let missing_uri = "/api/v1/mcp/skill-exposures?skill_iri=skill://acme/missing";
    let (foreign_status, foreign_body) = call(
        &app,
        Method::DELETE,
        &delete_uri,
        Caller::Bearer(&da_b),
        Value::Null,
    )
    .await;
    let (missing_status, missing_body) = call(
        &app,
        Method::DELETE,
        missing_uri,
        Caller::Bearer(&da_b),
        Value::Null,
    )
    .await;
    assert_eq!(foreign_status, missing_status);
    assert_eq!(foreign_body, missing_body);
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    let rows = exposure_rows(dir.path());
    assert_eq!(
        rows.len(),
        1,
        "project B's delete changed project A's exposure"
    );
    assert_eq!(rows[0]["project_id"], "project-a");
    assert_eq!(rows[0]["enabled"], true);
    assert_eq!(rows[0]["tool_name"], "weather.lookup");

    // Same skill IRI and tool name from project B must not replace A's row.
    let (status, upserted) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_b),
        json!({"skill_iri": iri, "tool_name": "weather.lookup", "enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upserted}");
    assert_eq!(upserted["exposure"]["project_id"], "project-b");
    assert_eq!(upserted["exposure"]["enabled"], false);
    let rows = exposure_rows(dir.path());
    let project_a = rows
        .iter()
        .find(|row| row["project_id"] == "project-a")
        .expect("project A's exposure is missing");
    assert_eq!(project_a["enabled"], true);
    assert_eq!(project_a["tool_name"], "weather.lookup");
    assert_eq!(project_a["skill_iri"], iri);
    let (status, listed) = call(
        &app,
        Method::GET,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_a),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["exposures"][0]["project_id"], "project-a");
    assert_eq!(listed["exposures"][0]["enabled"], true);
    let (status, listed) = call(
        &app,
        Method::GET,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_b),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["exposures"][0]["project_id"], "project-b");
    assert_eq!(listed["exposures"][0]["enabled"], false);
}

/// #384: rows written before project_id existed stay invisible and undeletable.
/// They are not assigned to the caller's project on read or on a later upsert.
#[tokio::test]
async fn isolation_contract_skill_exposure_legacy_rows_fail_closed() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://tenant-b/legacy";
    publish_exposed_skill(&state, iri, "tenant-b", "project-b");
    let path = dir.path().join("mcp_skill_exposures.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!([{
            "tenant_id": "tenant-b",
            "skill_iri": iri,
            "tool_name": "legacy.tool",
            "enabled": true
        }]))
        .unwrap(),
    )
    .unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    let app = router(state);
    let da = token("tenant-b", &["DA"], Some("project-b"));

    let (status, listed) = call(
        &app,
        Method::GET,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 0, "legacy exposure was visible");
    assert!(listed["exposures"].as_array().unwrap().is_empty());

    let (legacy_status, legacy_body) = call(
        &app,
        Method::DELETE,
        &format!("/api/v1/mcp/skill-exposures?skill_iri={iri}"),
        Caller::Bearer(&da),
        Value::Null,
    )
    .await;
    let (missing_status, missing_body) = call(
        &app,
        Method::DELETE,
        "/api/v1/mcp/skill-exposures?skill_iri=skill://tenant-b/missing",
        Caller::Bearer(&da),
        Value::Null,
    )
    .await;
    assert_eq!(legacy_status, missing_status);
    assert_eq!(legacy_body, missing_body);
    assert_eq!(legacy_status, StatusCode::NOT_FOUND);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

    let (status, tools) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&da),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 0);
    let (status, called) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&da),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "legacy.tool", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{called}");

    let (status, upserted) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da),
        json!({"skill_iri": iri, "tool_name": "legacy.tool", "enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upserted}");
    let rows = exposure_rows(dir.path());
    assert!(
        rows.iter().any(|row| {
            row["skill_iri"] == iri
                && row["enabled"] == true
                && row["tool_name"] == "legacy.tool"
                && row.get("project_id").and_then(Value::as_str).is_none()
        }),
        "legacy row was adopted into a project: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| {
            row["project_id"] == "project-b" && row["enabled"] == false && row["skill_iri"] == iri
        }),
        "project upsert did not create its own row: {rows:?}"
    );
}

/// #384 review: a token with no project claim is `VerifiedDefaulted` and would
/// otherwise match a same-tenant project whose id is the literal `default`.
#[tokio::test]
async fn isolation_contract_skill_exposure_mcp_rejects_defaulted_project() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://acme/default-project";
    publish_exposed_skill(&state, iri, "tenant-a", "default");
    let app = router(state);
    let explicit_default = token("tenant-a", &["DA"], Some("default"));
    let defaulted = token("tenant-a", &["DA"], None);

    let (status, created) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&explicit_default),
        json!({"skill_iri": iri, "tool_name": "default.lookup", "enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["exposure"]["project_id"], "default");

    let list_rpc = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let call_rpc = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "default.lookup", "arguments": {}}
    });
    for body in [list_rpc.clone(), call_rpc.clone()] {
        let (status, error) =
            call(&app, Method::POST, "/mcp", Caller::Bearer(&defaulted), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        assert_eq!(error["error"], "mcp_claims_incomplete");
        assert_eq!(error["missing_field"], "project_id");
    }

    let (status, tools) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&explicit_default),
        list_rpc,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["result"]["tools"][0]["name"], "default.lookup");
    let (status, called) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&explicit_default),
        call_rpc,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{called}");
    assert_eq!(called["result"]["content"][0]["json"]["status"], "accepted");
}

/// A client-supplied project_id does not choose the row's project.
#[tokio::test]
async fn isolation_contract_skill_exposure_ignores_body_project_id() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://acme/body-project";
    publish_exposed_skill(&state, iri, "tenant-a", "project-b");
    let app = router(state);
    let da_b = token("tenant-a", &["DA"], Some("project-b"));

    let (status, created) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_b),
        json!({
            "skill_iri": iri,
            "tool_name": "body.lookup",
            "enabled": true,
            "project_id": "project-a"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["exposure"]["project_id"], "project-b");
    let rows = exposure_rows(dir.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["project_id"], "project-b");
    assert_eq!(rows[0]["tenant_id"], "tenant-a");
    assert!(rows.iter().all(|row| row["project_id"] != "project-a"));
}

/// An empty `project_id` is the same fail-closed legacy row as a missing one.
#[tokio::test]
async fn isolation_contract_skill_exposure_blank_project_id_fails_closed() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://tenant-b/blank";
    publish_exposed_skill(&state, iri, "tenant-b", "project-b");
    let path = dir.path().join("mcp_skill_exposures.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!([{
            "tenant_id": "tenant-b",
            "project_id": "",
            "skill_iri": iri,
            "tool_name": "blank.tool",
            "enabled": true
        }]))
        .unwrap(),
    )
    .unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    let app = router(state);
    let da = token("tenant-b", &["DA"], Some("project-b"));

    let (status, listed) = call(
        &app,
        Method::GET,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], 0, "blank project_id was visible: {listed}");

    let (blank_status, blank_body) = call(
        &app,
        Method::DELETE,
        &format!("/api/v1/mcp/skill-exposures?skill_iri={iri}"),
        Caller::Bearer(&da),
        Value::Null,
    )
    .await;
    let (missing_status, missing_body) = call(
        &app,
        Method::DELETE,
        "/api/v1/mcp/skill-exposures?skill_iri=skill://tenant-b/missing",
        Caller::Bearer(&da),
        Value::Null,
    )
    .await;
    assert_eq!(blank_status, missing_status);
    assert_eq!(blank_body, missing_body);
    assert_eq!(blank_status, StatusCode::NOT_FOUND);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

    let (status, tools) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&da),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 0);

    let (status, upserted) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da),
        json!({"skill_iri": iri, "tool_name": "blank.tool", "enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upserted}");
    let rows = exposure_rows(dir.path());
    assert!(
        rows.iter().any(|row| {
            row["skill_iri"] == iri && row["project_id"] == "" && row["enabled"] == true
        }),
        "blank project_id row was adopted: {rows:?}"
    );
}

/// #430: concurrent creates must not drop rows.
#[tokio::test]
async fn isolation_contract_skill_exposure_concurrent_creates_keep_every_row() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    const N: usize = 40;
    for i in 0..N {
        publish_exposed_skill(
            &state,
            &format!("skill://acme/parallel-{i}"),
            "tenant-a",
            "project-a",
        );
    }
    let app = router(state);
    let da = token("tenant-a", &["DA"], Some("project-a"));
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..N {
        let app = app.clone();
        let da = da.clone();
        tasks.spawn(async move {
            call(
                &app,
                Method::POST,
                "/api/v1/mcp/skill-exposures",
                Caller::Bearer(&da),
                json!({
                    "skill_iri": format!("skill://acme/parallel-{i}"),
                    "tool_name": format!("tool.{i}"),
                    "enabled": true
                }),
            )
            .await
        });
    }
    let mut created = 0;
    while let Some(joined) = tasks.join_next().await {
        let (status, body) = joined.expect("create task");
        assert_eq!(status, StatusCode::CREATED, "{body}");
        created += 1;
    }
    assert_eq!(created, N);
    let rows = exposure_rows(dir.path());
    assert_eq!(rows.len(), N, "concurrent creates lost rows: {rows:?}");
    let names: std::collections::HashSet<_> = rows
        .iter()
        .map(|row| row["tool_name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names.len(), N);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("mcp_skill_exposures.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// #430: a file that will not parse is left untouched.
#[tokio::test]
async fn isolation_contract_skill_exposure_corrupt_file_is_not_rewritten() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://acme/corrupt";
    publish_exposed_skill(&state, iri, "tenant-a", "project-a");
    let path = dir.path().join("mcp_skill_exposures.json");
    let garbage = b"[{";
    std::fs::write(&path, garbage).unwrap();
    let app = router(state);
    let da = token("tenant-a", &["DA"], Some("project-a"));

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da),
        json!({"skill_iri": iri, "tool_name": "corrupt.tool", "enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"], "skill_exposure_store_failed");
    assert_eq!(std::fs::read(&path).unwrap(), garbage);

    let (status, body) = call(
        &app,
        Method::DELETE,
        &format!("/api/v1/mcp/skill-exposures?skill_iri={iri}"),
        Caller::Bearer(&da),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"], "skill_exposure_store_failed");
    assert_eq!(std::fs::read(&path).unwrap(), garbage);
}

/// #431: a skill published by tenant A cannot be exposed by tenant B. The
/// rejection and tenant B's tool list omit the skill description and input schema.
#[tokio::test]
async fn isolation_contract_skill_exposure_rejects_other_tenants_publish() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://tenant-a/secret-skill";
    let description = "tenant-a classified briefing";
    let schema = json!({"type": "object", "properties": {"classifiedCode": {"type": "string"}}});
    publish_exposed_skill_meta(&state, iri, "tenant-a", "project-a", description, schema);
    let app = router(state.clone());
    let da_b = token("tenant-b", &["DA"], Some("project-b"));
    let da_a_other_project = token("tenant-a", &["DA"], Some("project-b"));

    let (status, rejected) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_b),
        json!({"skill_iri": iri, "tool_name": "secret.lookup", "enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    assert_eq!(
        rejected["error"],
        "Skill must pass the tenant publish gate before MCP exposure"
    );
    let rejected_text = rejected.to_string();
    assert!(!rejected_text.contains(description), "{rejected}");
    assert!(!rejected_text.contains("classifiedCode"), "{rejected}");
    assert!(rejected.get("description").is_none());
    assert!(rejected.get("inputSchema").is_none());
    assert!(exposure_rows(dir.path()).is_empty());

    std::fs::write(
        dir.path().join("mcp_skill_exposures.json"),
        serde_json::to_string_pretty(&json!([{
            "tenant_id": "tenant-b",
            "project_id": "project-b",
            "skill_iri": iri,
            "tool_name": "secret.lookup",
            "enabled": true
        }]))
        .unwrap(),
    )
    .unwrap();
    let list_rpc = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let (status, tools) = call(&app, Method::POST, "/mcp", Caller::Bearer(&da_b), list_rpc).await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 0);
    let tools_text = tools.to_string();
    assert!(!tools_text.contains(description), "{tools}");
    assert!(!tools_text.contains("classifiedCode"), "{tools}");
    assert!(tools.get("description").is_none());
    assert!(tools.get("inputSchema").is_none());

    let (status, called) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&da_b),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "secret.lookup", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{called}");
    let called_text = called.to_string();
    assert!(!called_text.contains(description), "{called}");
    assert!(!called_text.contains("classifiedCode"), "{called}");

    let (status, created) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da_a_other_project),
        json!({"skill_iri": iri, "tool_name": "secret.lookup", "enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, owned) = call(
        &app,
        Method::POST,
        "/mcp",
        Caller::Bearer(&da_a_other_project),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{owned}");
    let owned_text = owned.to_string();
    assert!(owned_text.contains(description), "{owned}");
    assert!(owned_text.contains("classifiedCode"), "{owned}");
}

/// #431: an admission run with no publisher tenant authorizes no exposure.
#[tokio::test]
async fn isolation_contract_skill_exposure_legacy_publish_has_no_tenant() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = env(dir.path(), false);
    let state = test_state(dir.path());
    let iri = "skill://tenant-a/unscoped";
    publish_exposed_skill_meta(
        &state,
        iri,
        "tenant-a",
        "project-a",
        "unscoped briefing",
        json!({"type": "object", "properties": {"unscopedCode": {"type": "string"}}}),
    );
    let path = dir.path().join("pipeline_runs.json");
    let mut runs: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    runs[0]
        .as_object_mut()
        .unwrap()
        .remove("publisher_tenant_id");
    runs[0]
        .as_object_mut()
        .unwrap()
        .remove("publisher_project_id");
    std::fs::write(&path, serde_json::to_string_pretty(&runs).unwrap()).unwrap();

    let app = router(state);
    let da = token("tenant-a", &["DA"], Some("project-a"));
    let (status, rejected) = call(
        &app,
        Method::POST,
        "/api/v1/mcp/skill-exposures",
        Caller::Bearer(&da),
        json!({"skill_iri": iri, "tool_name": "unscoped.lookup", "enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    let text = rejected.to_string();
    assert!(!text.contains("unscoped briefing"), "{rejected}");
    assert!(!text.contains("unscopedCode"), "{rejected}");
    assert!(exposure_rows(dir.path()).is_empty());
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
