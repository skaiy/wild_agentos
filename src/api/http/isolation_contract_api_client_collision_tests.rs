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

fn is_unauthorized(result: Result<api_gov::ApiCallerContext, AuthError>) -> bool {
    matches!(result, Err(AuthError::Unauthorized))
}

#[test]
fn isolation_contract_resolve_bearer_token_rejects_any_duplicate_client_id() {
    // Explicit in-memory state that bypasses the load-time quarantine: both
    // clients are `active`, so only the resolver's duplicate check stands.
    let (key_a, plain_a) = issued("key-a", SHARED, "tenant-a");
    let (key_b, plain_b) = issued("key-b", SHARED, "tenant-b");
    let client_orders = [
        vec![client(SHARED, "tenant-a"), client(SHARED, "tenant-b")],
        vec![client(SHARED, "tenant-b"), client(SHARED, "tenant-a")],
    ];
    let key_orders = [
        vec![key_a.clone(), key_b.clone()],
        vec![key_b.clone(), key_a.clone()],
    ];
    for clients in &client_orders {
        for keys in &key_orders {
            for plaintext in [&plain_a, &plain_b] {
                assert!(
                    is_unauthorized(api_gov::resolve_bearer_token(plaintext, keys, clients)),
                    "duplicate client id must be 401 for every key"
                );
            }
        }
    }

    // Duplicates within one tenant are rejected too (never first match).
    let same_tenant = vec![client(SHARED, "tenant-a"), client(SHARED, "tenant-a")];
    assert!(is_unauthorized(api_gov::resolve_bearer_token(
        &plain_a,
        &key_orders[0],
        &same_tenant
    )));

    // A quarantined client is never usable, even as the only client left.
    let mut only_b = vec![client(SHARED, "tenant-b")];
    only_b[0].status = CLIENT_ID_CONFLICT_STATUS.into();
    assert!(is_unauthorized(api_gov::resolve_bearer_token(
        &plain_b,
        &key_orders[0],
        &only_b
    )));

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
async fn isolation_contract_id_conflict_client_status_update_is_409_and_auth_stays_401() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let (key_a, plain_a) = issued("key-a", SHARED, "tenant-a");
    let (key_b, plain_b) = issued("key-b", SHARED, "tenant-b");
    let mut clients = vec![client(SHARED, "tenant-a"), client(SHARED, "tenant-b")];
    api_gov::quarantine_cross_tenant_client_ids(&mut clients);
    let state = test_state(dir.path());
    *state.api_clients.write().await = clients;
    *state.api_keys.write().await = vec![key_a, key_b];
    let router = app(state.clone());

    for body in [json!({"status": "active"}), json!({"status": "disabled"})] {
        let (status, _) = send(
            &router,
            Method::PUT,
            &format!("/api/v1/api-clients/{SHARED}"),
            body,
            &da("tenant-a"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
    assert!(state
        .api_clients
        .read()
        .await
        .iter()
        .all(|c| c.status == CLIENT_ID_CONFLICT_STATUS));

    let chat = |plaintext: String, agent: &'static str| {
        let router = router.clone();
        async move {
            send(
                &router,
                Method::POST,
                &format!("/api/v1/public/agents/{agent}/chat"),
                json!({"message": "hi"}),
                &plaintext,
            )
            .await
            .0
        }
    };
    assert_eq!(
        chat(plain_a.clone(), "agent-tenant-a").await,
        StatusCode::UNAUTHORIZED
    );

    // Bypass the update API entirely: re-enable both clients in state. The
    // duplicate id alone still fails closed for both tenants' keys.
    for c in state.api_clients.write().await.iter_mut() {
        c.status = "active".into();
    }
    {
        let keys = state.api_keys.read().await;
        let clients = state.api_clients.read().await;
        for plaintext in [&plain_a, &plain_b] {
            assert!(is_unauthorized(api_gov::resolve_bearer_token(
                plaintext, &keys, &clients
            )));
        }
    }
    assert_eq!(
        chat(plain_a.clone(), "agent-tenant-a").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        chat(plain_b.clone(), "agent-tenant-b").await,
        StatusCode::UNAUTHORIZED
    );
}

/// In-memory and on-disk client/key snapshot (ids, tenants, statuses, key
/// hashes) used to prove a refused delete changed nothing.
async fn snapshot(state: &super::AppState) -> (Value, Value, Value, Value) {
    (
        serde_json::to_value(&*state.api_clients.read().await).unwrap(),
        serde_json::to_value(&*state.api_keys.read().await).unwrap(),
        serde_json::to_value(api_gov::load_api_clients()).unwrap(),
        serde_json::to_value(api_gov::load_api_keys()).unwrap(),
    )
}

/// Seeds state (and disk) with quarantined `clients` plus `keys`.
async fn seeded(
    dir: &Path,
    mut clients: Vec<ApiClient>,
    keys: Vec<ApiKey>,
) -> std::sync::Arc<super::AppState> {
    api_gov::quarantine_cross_tenant_client_ids(&mut clients);
    api_gov::save_api_clients(&clients).unwrap();
    api_gov::save_api_keys(&keys).unwrap();
    let state = test_state(dir);
    *state.api_clients.write().await = clients;
    *state.api_keys.write().await = keys;
    state
}

#[tokio::test]
async fn isolation_contract_delete_colliding_client_is_409_and_changes_nothing() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let (key_a1, _) = issued("key-a1", SHARED, "tenant-a");
    let (key_a2, _) = issued("key-a2", SHARED, "tenant-a");
    let (key_b1, _) = issued("key-b1", SHARED, "tenant-b");
    let (key_b2, _) = issued("key-b2", SHARED, "tenant-b");
    let cases = [
        // Distinct slugs and same slug (`Tenant-A` / `tenant-a`): both refused.
        vec![client(SHARED, "tenant-a"), client(SHARED, "tenant-b")],
        vec![client(SHARED, "Tenant-A"), client(SHARED, "tenant-a")],
    ];
    for clients in cases {
        let state = seeded(
            dir.path(),
            clients,
            vec![
                key_a1.clone(),
                key_b1.clone(),
                key_a2.clone(),
                key_b2.clone(),
            ],
        )
        .await;
        let router = app(state.clone());
        let before = snapshot(&state).await;
        assert_eq!(before.0.as_array().unwrap().len(), 2);
        assert_eq!(before.1.as_array().unwrap().len(), 4);

        let (status, body) = send(
            &router,
            Method::DELETE,
            &format!("/api/v1/api-clients/{SHARED}"),
            Value::Null,
            &da("tenant-a"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!body.to_string().contains("tenant-b"));
        assert_eq!(
            snapshot(&state).await,
            before,
            "refused delete must not touch clients or keys (memory or disk)"
        );
    }
}

#[tokio::test]
async fn isolation_contract_legacy_unprefixed_key_under_colliding_id_is_not_handed_over() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    // A legacy key under A's (colliding) client that carries no tenant prefix.
    let (mut legacy, _) = issued("key-legacy-a", SHARED, "tenant-a");
    legacy.key_prefix = "sk-legacy0".into();
    legacy.name = "legacy-a-key-name".into();
    let (key_b, _) = issued("key-b", SHARED, "tenant-b");
    let state = seeded(
        dir.path(),
        vec![client(SHARED, "tenant-a"), client(SHARED, "tenant-b")],
        vec![legacy.clone(), key_b],
    )
    .await;
    let router = app(state.clone());
    let before = snapshot(&state).await;

    let (status, _) = send(
        &router,
        Method::DELETE,
        &format!("/api/v1/api-clients/{SHARED}"),
        Value::Null,
        &da("tenant-a"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(snapshot(&state).await, before);

    let (status, body) = send(
        &router,
        Method::GET,
        "/api/v1/api-clients",
        Value::Null,
        &da("tenant-b"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = body.to_string();
    for hidden in [&legacy.id, &legacy.key_prefix, &legacy.name] {
        assert!(!text.contains(hidden.as_str()), "A's legacy key shown to B");
    }
    assert!(text.contains("key-b"));
}

#[tokio::test]
async fn isolation_contract_list_colliding_client_shows_only_own_keys() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let (key_a, _) = issued("key-a-visible", SHARED, "tenant-a");
    let (mut key_b, _) = issued("key-b-hidden", SHARED, "tenant-b");
    key_b.name = "tenant-b-secret-name".into();
    let b_id = key_b.id.clone();
    let b_prefix = key_b.key_prefix.clone();
    let b_name = key_b.name.clone();
    let mut clients = vec![client(SHARED, "tenant-a"), client(SHARED, "tenant-b")];
    api_gov::quarantine_cross_tenant_client_ids(&mut clients);
    let state = test_state(dir.path());
    *state.api_clients.write().await = clients;
    *state.api_keys.write().await = vec![key_b.clone(), key_a];
    let router = app(state.clone());

    let (status, body) = send(
        &router,
        Method::GET,
        "/api/v1/api-clients",
        Value::Null,
        &da("tenant-a"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = body.to_string();
    for hidden in [&b_id, &b_prefix, &b_name] {
        assert!(!text.contains(hidden.as_str()), "tenant B key leaked");
    }
    assert!(text.contains("key-a-visible"));

    // Same-slug tenants: ownership is ambiguous, so no keys are listed.
    *state.api_clients.write().await = vec![client(SHARED, "Tenant-A"), client(SHARED, "tenant-a")];
    let (status, body) = send(
        &router,
        Method::GET,
        "/api/v1/api-clients",
        Value::Null,
        &da("tenant-a"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = body.to_string();
    assert!(!text.contains("key-a-visible") && !text.contains(b_id.as_str()));
}

#[test]
fn isolation_contract_id_conflict_status_persists_across_save_and_reload() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = setup(dir.path());
    let (key_a, plain_a) = issued("key-a", SHARED, "tenant-a");
    api_gov::save_api_clients(&[client(SHARED, "tenant-a"), client(SHARED, "tenant-b")]).unwrap();
    api_gov::save_api_keys(&[key_a]).unwrap();

    let loaded = api_gov::load_api_clients();
    assert!(loaded.iter().all(|c| c.status == CLIENT_ID_CONFLICT_STATUS));
    api_gov::save_api_clients(&loaded).unwrap();

    // Even after the other tenant's client is gone (no collision left), the
    // persisted status keeps A's client unusable until an admin resets it.
    let only_a: Vec<ApiClient> = loaded
        .into_iter()
        .filter(|c| c.tenant_id == "tenant-a")
        .collect();
    api_gov::save_api_clients(&only_a).unwrap();
    let reloaded = api_gov::load_api_clients();
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].status, CLIENT_ID_CONFLICT_STATUS);
    assert!(is_unauthorized(api_gov::resolve_bearer_token(
        &plain_a,
        &api_gov::load_api_keys(),
        &reloaded
    )));
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
