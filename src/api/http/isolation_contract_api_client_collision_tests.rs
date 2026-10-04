//! Fail closed when one API client id appears under more than one tenant
//! (legacy or imported `api_clients.json`): no first-match attribution in
//! auth, and no ambiguous legacy audit records.

use std::path::Path;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{
    api_gov::{self, ApiClient, ApiKey, AuthError, Quota, RateLimit, CLIENT_ID_CONFLICT_STATUS},
    control_plane_route_auth_tests::{app, test_state, EnvGuard},
    iam::JwtClaims,
    TEST_ENV_LOCK,
};

const SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
const SHARED: &str = "client-shared";

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

fn da(tenant: &str) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: "test-da".into(),
            tenant_id: tenant.into(),
            project_id: Some("project".into()),
            roles: vec!["DA".into()],
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

fn client(id: &str, tenant: &str) -> ApiClient {
    ApiClient {
        id: id.into(),
        name: format!("{tenant}-client"),
        description: String::new(),
        tenant_id: tenant.into(),
        owner: "owner".into(),
        granted_agent_ids: vec![format!("agent-{tenant}")],
        status: "active".into(),
        rate_limit: RateLimit::default(),
        quota: Quota::default(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// A key generated exactly like `issue_api_key_handler` does; returns the
/// stored record and its one-time plaintext (test-only, never printed).
fn issued(kid: &str, client_id: &str, tenant: &str) -> (ApiKey, String) {
    let (plaintext, key_prefix, key_hash) = api_gov::generate_key(tenant);
    (
        ApiKey {
            id: kid.into(),
            name: kid.into(),
            client_id: client_id.into(),
            key_prefix,
            key_hash,
            status: "active".into(),
            last_used_at: None,
            expires_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        },
        plaintext,
    )
}

async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    body: Value,
    bearer: &str,
) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn isolation_contract_client_id_collision_at_load_disables_all_and_auth_is_401() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let (key_a, plain_a) = issued("key-a", SHARED, "tenant-a");
    let (key_b, plain_b) = issued("key-b", SHARED, "tenant-b");
    let (key_c, plain_c) = issued("key-c", "client-c", "tenant-c");
    api_gov::save_api_clients(&[
        client(SHARED, "tenant-a"),
        client(SHARED, "tenant-b"),
        client("client-c", "tenant-c"),
    ])
    .unwrap();
    api_gov::save_api_keys(&[key_a, key_b, key_c]).unwrap();

    let clients = api_gov::load_api_clients();
    let status_of = |tenant: &str| {
        clients
            .iter()
            .find(|c| c.tenant_id == tenant)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(status_of("tenant-a"), CLIENT_ID_CONFLICT_STATUS);
    assert_eq!(status_of("tenant-b"), CLIENT_ID_CONFLICT_STATUS);
    assert_eq!(status_of("tenant-c"), "active");

    let keys = api_gov::load_api_keys();
    for plaintext in [&plain_a, &plain_b] {
        assert!(matches!(
            api_gov::resolve_bearer_token(plaintext, &keys, &clients),
            Err(AuthError::Unauthorized)
        ));
    }
    assert_eq!(
        api_gov::resolve_bearer_token(&plain_c, &keys, &clients)
            .unwrap()
            .tenant_id,
        "tenant-c"
    );

    // Route level: the public gate answers 401 for both colliding keys, while a
    // non-colliding key gets past authentication.
    let state = test_state(dir.path());
    *state.api_clients.write().await = clients;
    *state.api_keys.write().await = keys;
    let router = app(state);
    for (plaintext, agent) in [(&plain_a, "agent-tenant-a"), (&plain_b, "agent-tenant-b")] {
        let (status, body) = send(
            &router,
            Method::POST,
            &format!("/api/v1/public/agents/{agent}/chat"),
            json!({"message": "hi"}),
            plaintext,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
    let (status, _) = send(
        &router,
        Method::POST,
        "/api/v1/public/agents/agent-tenant-c/chat",
        json!({"message": "hi"}),
        &plain_c,
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

#[test]
fn isolation_contract_resolve_bearer_token_selects_colliding_client_by_key_tenant() {
    // Explicit in-memory state that bypasses the load-time quarantine, so the
    // resolver's own selection is exercised.
    let clients = vec![client(SHARED, "tenant-a"), client(SHARED, "tenant-b")];
    let (key_a, plain_a) = issued("key-a", SHARED, "tenant-a");
    let (key_b, plain_b) = issued("key-b", SHARED, "tenant-b");
    let (key_x, plain_x) = issued("key-x", SHARED, "tenant-x");
    let keys = vec![key_a, key_b, key_x];

    // B's key maps to tenant B even though tenant A's client comes first.
    let b = api_gov::resolve_bearer_token(&plain_b, &keys, &clients).unwrap();
    assert_eq!(b.tenant_id, "tenant-b");
    assert_eq!(b.granted_agent_ids, vec!["agent-tenant-b".to_string()]);
    let a = api_gov::resolve_bearer_token(&plain_a, &keys, &clients).unwrap();
    assert_eq!(a.tenant_id, "tenant-a");

    // A key whose prefix matches none of the colliding tenants: 401.
    assert!(matches!(
        api_gov::resolve_bearer_token(&plain_x, &keys, &clients),
        Err(AuthError::Unauthorized)
    ));

    // Two colliding tenants with the same slug (`Tenant-A` / `tenant-a`): the
    // prefix matches both, so 401 rather than either one.
    let same_slug = vec![client(SHARED, "Tenant-A"), client(SHARED, "tenant-a")];
    assert!(matches!(
        api_gov::resolve_bearer_token(&plain_a, &keys, &same_slug),
        Err(AuthError::Unauthorized)
    ));

    // A quarantined client is never usable, even when it is the only match.
    let mut quarantined = clients.clone();
    api_gov::quarantine_cross_tenant_client_ids(&mut quarantined);
    let only_b: Vec<ApiClient> = quarantined
        .into_iter()
        .filter(|c| c.tenant_id == "tenant-b")
        .collect();
    assert!(matches!(
        api_gov::resolve_bearer_token(&plain_b, &keys, &only_b),
        Err(AuthError::Unauthorized)
    ));

    // Unique ids keep today's behavior.
    let unique = vec![client("client-b", "tenant-b")];
    let (key_u, plain_u) = issued("key-u", "client-b", "tenant-b");
    assert_eq!(
        api_gov::resolve_bearer_token(&plain_u, &[key_u], &unique)
            .unwrap()
            .tenant_id,
        "tenant-b"
    );
}

#[tokio::test]
async fn isolation_contract_legacy_audit_with_colliding_client_id_is_hidden() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let lines = [
        // Legacy, colliding id: ambiguous, must never be returned.
        json!({"client_id": SHARED, "agent_id": "agent-x", "seq": 1}),
        // Legacy, A's own unique id: still visible to A.
        json!({"client_id": "client-a-only", "agent_id": "agent-a", "seq": 2}),
        // Tenant-tagged records: attributed by tenant_id, unchanged.
        json!({"client_id": SHARED, "tenant_id": "tenant-a", "seq": 3}),
        json!({"client_id": SHARED, "tenant_id": "tenant-b", "seq": 4}),
    ];
    let mut content = String::new();
    for line in &lines {
        content.push_str(&line.to_string());
        content.push('\n');
    }
    std::fs::write(api_gov::api_audit_path(), content).unwrap();

    let colliding = vec![
        client(SHARED, "tenant-a"),
        client(SHARED, "tenant-b"),
        client("client-a-only", "tenant-a"),
    ];
    // Both raw collisions (in memory) and load-time quarantine hide it; the
    // quarantine status keeps hiding it even after the other tenant's client
    // is gone.
    let mut quarantined = colliding.clone();
    api_gov::quarantine_cross_tenant_client_ids(&mut quarantined);
    let quarantined_a_only: Vec<ApiClient> = quarantined
        .iter()
        .filter(|c| c.tenant_id == "tenant-a")
        .cloned()
        .collect();
    for clients in [colliding, quarantined, quarantined_a_only] {
        let state = test_state(dir.path());
        *state.api_clients.write().await = clients;
        let router = app(state);
        for query in ["", &format!("?client_id={SHARED}")] {
            let (status, body) = send(
                &router,
                Method::GET,
                &format!("/api/v1/api-audit{query}"),
                Value::Null,
                &da("tenant-a"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let records = body["records"].as_array().unwrap();
            assert!(
                records.iter().all(|r| r["seq"] != 1 && r["seq"] != 4),
                "ambiguous or foreign record returned"
            );
            assert!(records.iter().any(|r| r["seq"] == 3));
            if query.is_empty() {
                assert!(records.iter().any(|r| r["seq"] == 2));
            }
        }
    }
}
