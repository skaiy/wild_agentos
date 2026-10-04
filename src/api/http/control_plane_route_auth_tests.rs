//! Route-level authorization coverage for the #241 control-plane claim gates.

use std::{
    ffi::OsString,
    sync::{atomic::AtomicUsize, Arc},
};

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    routing::{delete, get, post, put},
    Json, Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower::ServiceExt;

use super::{
    api_clients::{
        create_api_client_handler, delete_api_client_handler, issue_api_key_handler,
        list_api_audit_handler, list_api_clients_handler, revoke_api_key_handler,
        update_api_client_handler,
    },
    api_gov::{ApiClient, ApiKey, ApiUsageState, Quota, RateLimit},
    core_ops::{control_batch_agent_handler, list_batch_agents_handler},
    iam::JwtClaims,
    models::{activate_embedding_handler, provider_models_handler, test_model_handler},
    runtime::metrics_handler,
    AppState, TEST_ENV_LOCK,
};
use crate::{
    core::core_types::{CoreConfig, SemanticCore},
    gateway::unified_gateway::UnifiedGateway,
    tools::prompt_registry::PromptRegistry,
};

const TEST_JWT_SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";

pub(super) struct EnvGuard {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    pub(super) fn set(vars: &[(&'static str, String)]) -> Self {
        let previous = vars
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in vars {
            std::env::set_var(name, value);
        }
        Self { previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn test_gateway() -> UnifiedGateway {
    UnifiedGateway::new(&crate::config::GatewaySettings {
        base_url: "http://localhost".into(),
        api_key: String::new(),
        default_model: "test-model".into(),
        timeout_seconds: 30,
        max_retries: 1,
        retry_base_ms: 500,
        use_responses_api: false,
        model_mapping: Default::default(),
    })
    .unwrap()
}

pub(super) fn test_state(data_dir: &std::path::Path) -> Arc<AppState> {
    let core = Arc::new(
        SemanticCore::new(CoreConfig {
            l0_storage_path: data_dir.join("l0").to_string_lossy().into_owned(),
            enable_metrics: false,
            ..CoreConfig::default()
        })
        .unwrap(),
    );
    Arc::new(AppState {
        core,
        gateway: Arc::new(test_gateway()),
        kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
        config_info: Arc::new(tokio::sync::RwLock::new(json!({"before": true}))),
        agents_info: json!({}),
        mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
        user_agents: Arc::new(tokio::sync::RwLock::new(vec![])),
        prompts: Arc::new(PromptRegistry::new()),
        kb_categories: Arc::new(tokio::sync::RwLock::new(vec![])),
        knowledge_bases: Arc::new(tokio::sync::RwLock::new(vec![])),
        knowledge_packs: Arc::new(tokio::sync::RwLock::new(vec![])),
        vector_store: Arc::new(arc_swap::ArcSwapOption::empty()),
        blob_store: None,
        task_executor: None,
        batch_manager: None,
        api_clients: Arc::new(tokio::sync::RwLock::new(vec![seed_client()])),
        api_keys: Arc::new(tokio::sync::RwLock::new(vec![seed_key()])),
        api_usage: Arc::new(ApiUsageState::default()),
        online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
        online_corpus_queue_capacity: 1,
        shutdown: tokio_util::sync::CancellationToken::new(),
    })
}

fn seed_client() -> ApiClient {
    ApiClient {
        id: "client-seed".into(),
        name: "seed".into(),
        description: "state mutation sentinel".into(),
        tenant_id: "tenant-a".into(),
        owner: "seed-owner".into(),
        granted_agent_ids: vec![],
        status: "active".into(),
        rate_limit: RateLimit::default(),
        quota: Quota::default(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn seed_key() -> ApiKey {
    ApiKey {
        id: "key-seed".into(),
        name: "seed".into(),
        client_id: "client-seed".into(),
        key_prefix: "sk-tenant-a-seed".into(),
        key_hash: "hash".into(),
        status: "active".into(),
        last_used_at: None,
        expires_at: None,
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

pub(super) fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/config", put(super::config::update_config_handler))
        .route(
            "/api/v1/public/agents/:id/chat",
            post(super::chat::public_agent_chat_handler),
        )
        .route("/v1/models", get(super::chat::openai_list_models_handler))
        .route("/api/v1/batch/agents", get(list_batch_agents_handler))
        .route(
            "/api/v1/batch/agents/:name/control",
            post(control_batch_agent_handler),
        )
        .route("/api/v1/models/test", post(test_model_handler))
        .route("/api/v1/providers/models", post(provider_models_handler))
        .route(
            "/api/v1/embedding/activate",
            post(activate_embedding_handler),
        )
        .route(
            "/api/v1/api-clients",
            get(list_api_clients_handler).post(create_api_client_handler),
        )
        .route(
            "/api/v1/api-clients/:id",
            put(update_api_client_handler).delete(delete_api_client_handler),
        )
        .route("/api/v1/api-clients/:id/keys", post(issue_api_key_handler))
        .route(
            "/api/v1/api-clients/:id/keys/:kid",
            delete(revoke_api_key_handler),
        )
        .route("/api/v1/api-audit", get(list_api_audit_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

fn jwt(roles: &[&str], project_id: Option<&str>) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: "test-user".into(),
            tenant_id: "tenant-a".into(),
            project_id: project_id.map(str::to_owned),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(TEST_JWT_SECRET),
    )
    .unwrap()
}

fn jwt_without_tenant(roles: &[&str]) -> String {
    encode(
        &Header::default(),
        &json!({
            "sub": "test-user",
            "roles": roles,
            "project_id": "project-a",
            "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
        }),
        &EncodingKey::from_secret(TEST_JWT_SECRET),
    )
    .unwrap()
}

async fn request(
    router: &Router,
    method: Method,
    uri: &str,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&body)
            .unwrap_or(Value::String(String::from_utf8_lossy(&body).into_owned())),
    )
}

async fn api_state(state: &AppState) -> Value {
    json!({
        "clients": state.api_clients.read().await.clone(),
        "keys": state.api_keys.read().await.clone(),
    })
}

fn write_models_override(data_dir: &std::path::Path, base_url: &str) {
    std::fs::write(
        data_dir.join("config_override.json"),
        json!({
            "models": {
                "providers": [{
                    "id": "provider-a",
                    "base_url": base_url,
                    "api_key": "test-only-not-a-secret",
                    "timeout_seconds": 3
                }],
                "resources": [{
                    "id": "embedding-a",
                    "provider_id": "provider-a",
                    "model": "embedding-test",
                    "modalities": ["embedding"],
                    "dimension": 3
                }]
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn control_plane_routes_require_verified_claims_and_da() {
    let _lock = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let data_dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(TEST_JWT_SECRET.to_vec()).unwrap(),
        ),
        (
            "AGENTOS_DATA_DIR",
            data_dir.path().to_string_lossy().into_owned(),
        ),
        ("AGENTOS_PLATFORM_ADMIN_TENANT", "tenant-a".into()),
    ]);
    write_models_override(data_dir.path(), "http://127.0.0.1:9");
    let state = test_state(data_dir.path());
    let router = app(state.clone());
    let no_da = jwt(&[], Some("project-a"));
    let da = jwt(&["DA"], Some("project-a"));
    let platform_admin = jwt(&["PLATFORM_ADMIN"], Some("project-a"));

    let routes = [
        (Method::GET, "/api/v1/batch/agents", json!(null)),
        (
            Method::POST,
            "/api/v1/batch/agents/example/control",
            json!({"action": "start"}),
        ),
        (
            Method::POST,
            "/api/v1/models/test",
            json!({"resource_id": "missing"}),
        ),
        (
            Method::POST,
            "/api/v1/providers/models",
            json!({"base_url": "", "api_key": ""}),
        ),
        (
            Method::POST,
            "/api/v1/embedding/activate",
            json!({"resource_id": "embedding-a"}),
        ),
        (Method::GET, "/api/v1/api-clients", json!(null)),
        (
            Method::POST,
            "/api/v1/api-clients",
            json!({"name": "created"}),
        ),
        (
            Method::PUT,
            "/api/v1/api-clients/client-seed",
            json!({"name": "changed"}),
        ),
        (
            Method::DELETE,
            "/api/v1/api-clients/client-seed",
            json!(null),
        ),
        (
            Method::POST,
            "/api/v1/api-clients/client-seed/keys",
            json!({"name": "new-key"}),
        ),
        (
            Method::DELETE,
            "/api/v1/api-clients/client-seed/keys/key-seed",
            json!(null),
        ),
        (Method::GET, "/api/v1/api-audit", json!(null)),
    ];

    for (method, uri, body) in routes {
        let before = api_state(&state).await;
        assert_eq!(
            request(&router, method.clone(), uri, body.clone(), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{uri} accepted a request without verified claims"
        );
        assert_eq!(
            request(&router, method, uri, body, Some(&no_da)).await.0,
            StatusCode::FORBIDDEN,
            "{uri} accepted verified claims without DA"
        );
        assert_eq!(
            api_state(&state).await,
            before,
            "{uri} mutated API client/key state before passing its authorization gate"
        );
    }

    assert_eq!(
        request(
            &router,
            Method::GET,
            "/api/v1/batch/agents",
            json!(null),
            Some(&da)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/batch/agents/example/control",
            json!({"action": "start"}),
            Some(&da),
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/models/test",
            json!({"resource_id": "missing"}),
            Some(&da),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST,
        "the normal model validation result proves the DA request passed the gate"
    );
    // #274: tightened, explicit tenant DA no longer activates global embedding.
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/embedding/activate",
            json!({"resource_id": "embedding-a"}),
            Some(&da),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // #274: tightened (success now requires a platform-admin token).
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/embedding/activate",
            json!({"resource_id": "embedding-a"}),
            Some(&platform_admin),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            Method::GET,
            "/api/v1/api-clients",
            json!(null),
            Some(&da)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/api-clients",
            json!({"name": "created"}),
            Some(&da),
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        request(
            &router,
            Method::PUT,
            "/api/v1/api-clients/client-seed",
            json!({"name": "changed"}),
            Some(&da),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/api-clients/client-seed/keys",
            json!({"name": "new-key"}),
            Some(&da),
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        request(
            &router,
            Method::DELETE,
            "/api/v1/api-clients/client-seed/keys/key-seed",
            json!(null),
            Some(&da),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            Method::DELETE,
            "/api/v1/api-clients/client-seed",
            json!(null),
            Some(&da),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            Method::GET,
            "/api/v1/api-audit",
            json!(null),
            Some(&da)
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn provider_models_authorization_precedes_outbound_request() {
    let _lock = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let data_dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(TEST_JWT_SECRET.to_vec()).unwrap(),
        ),
        (
            "AGENTOS_DATA_DIR",
            data_dir.path().to_string_lossy().into_owned(),
        ),
    ]);
    let requests = Arc::new(AtomicUsize::new(0));
    let mock_requests = requests.clone();
    let mock = Router::new().route(
        "/v1/models",
        get(move || {
            let requests = mock_requests.clone();
            async move {
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Json(json!({"data": [{"id": "mock-model", "owned_by": "test"}]}))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let router = app(test_state(data_dir.path()));
    let body = json!({
        "base_url": format!("http://{address}"),
        "api_key": "test-only-not-a-secret"
    });
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/providers/models",
            body.clone(),
            None,
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);

    let no_da = jwt(&[], Some("project-a"));
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/providers/models",
            body.clone(),
            Some(&no_da),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);

    let da = jwt(&["DA"], Some("project-a"));
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/providers/models",
            body,
            Some(&da),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn embedding_activation_rejections_do_not_change_active_config_or_store() {
    let _lock = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let data_dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(TEST_JWT_SECRET.to_vec()).unwrap(),
        ),
        (
            "AGENTOS_DATA_DIR",
            data_dir.path().to_string_lossy().into_owned(),
        ),
    ]);
    write_models_override(data_dir.path(), "http://127.0.0.1:9");
    let state = test_state(data_dir.path());
    let router = app(state.clone());
    let original_config = state.config_info.read().await.clone();
    let body = json!({"resource_id": "embedding-a"});

    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/embedding/activate",
            body.clone(),
            None,
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let no_da = jwt(&[], Some("project-a"));
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/embedding/activate",
            body,
            Some(&no_da),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let da = jwt(&["DA"], Some("project-a"));
    // #274: tightened, explicit DA cannot activate embedding.
    assert_eq!(
        request(
            &router,
            Method::POST,
            "/api/v1/embedding/activate",
            json!({"resource_id": "embedding-a"}),
            Some(&da),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(*state.config_info.read().await, original_config);
    assert!(
        state.vector_store.load_full().is_none(),
        "a rejected request must not hot-swap the active vector store or queue reindexing"
    );
}

#[tokio::test]
async fn metrics_remains_unauthenticated() {
    let _lock = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let data_dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[(
        "AGENTOS_DATA_DIR",
        data_dir.path().to_string_lossy().into_owned(),
    )]);
    let router = app(test_state(data_dir.path()));
    let (status, body) = request(&router, Method::GET, "/metrics", json!(null), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("l2_nodes").is_some());
    assert!(body.get("embedding_health").is_some());
}

#[derive(Debug)]
struct DefaultedClaimsObservation {
    statuses: Vec<(&'static str, StatusCode)>,
    outbound_request_delta: usize,
    api_state_unchanged: bool,
    config_unchanged: bool,
    vector_store_unchanged: bool,
}

async fn observe_control_plane_requests(
    token: &str,
    data_dir: &std::path::Path,
    outbound_requests: &AtomicUsize,
    address: std::net::SocketAddr,
) -> DefaultedClaimsObservation {
    let state = test_state(data_dir);
    let router = app(state.clone());
    let original_api_state = api_state(&state).await;
    let original_config = state.config_info.read().await.clone();
    let outbound_before = outbound_requests.load(std::sync::atomic::Ordering::SeqCst);
    let mut statuses = Vec::new();
    for (method, uri, body) in [
        (Method::GET, "/api/v1/batch/agents", json!(null)),
        (
            Method::POST,
            "/api/v1/batch/agents/example/control",
            json!({"action": "start"}),
        ),
        (
            Method::POST,
            "/api/v1/models/test",
            json!({"resource_id": "missing"}),
        ),
        (
            Method::POST,
            "/api/v1/providers/models",
            json!({"base_url": format!("http://{address}")}),
        ),
        (
            Method::POST,
            "/api/v1/embedding/activate",
            json!({"resource_id": "embedding-a"}),
        ),
        (Method::GET, "/api/v1/api-clients", json!(null)),
        (
            Method::POST,
            "/api/v1/api-clients",
            json!({"name": "created"}),
        ),
        (
            Method::PUT,
            "/api/v1/api-clients/missing",
            json!({"name": "changed"}),
        ),
        (Method::DELETE, "/api/v1/api-clients/missing", json!(null)),
        (
            Method::POST,
            "/api/v1/api-clients/missing/keys",
            json!({"name": "new-key"}),
        ),
        (
            Method::DELETE,
            "/api/v1/api-clients/missing/keys/missing",
            json!(null),
        ),
        (Method::GET, "/api/v1/api-audit", json!(null)),
    ] {
        statuses.push((
            uri,
            request(&router, method, uri, body, Some(token)).await.0,
        ));
    }

    let observation = DefaultedClaimsObservation {
        statuses,
        outbound_request_delta: outbound_requests.load(std::sync::atomic::Ordering::SeqCst)
            - outbound_before,
        api_state_unchanged: api_state(&state).await == original_api_state,
        config_unchanged: *state.config_info.read().await == original_config,
        vector_store_unchanged: state.vector_store.load_full().is_none(),
    };
    observation
}

// A verified JWT that defaulted project_id must fail closed before every
// control-plane side effect, matching the established VerifiedDefaulted convention.
#[tokio::test]
async fn control_plane_routes_reject_defaulted_verified_claims() {
    let _lock = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let data_dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(TEST_JWT_SECRET.to_vec()).unwrap(),
        ),
        (
            "AGENTOS_DATA_DIR",
            data_dir.path().to_string_lossy().into_owned(),
        ),
    ]);
    let outbound_requests = Arc::new(AtomicUsize::new(0));
    let mock_requests = outbound_requests.clone();
    let mock = Router::new().route(
        "/v1/models",
        get(move || {
            let requests = mock_requests.clone();
            async move {
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Json(json!({"data": []}))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    write_models_override(data_dir.path(), &format!("http://{address}"));

    let observation = observe_control_plane_requests(
        &jwt(&["DA"], None),
        data_dir.path(),
        &outbound_requests,
        address,
    )
    .await;
    assert!(
        observation
            .statuses
            .iter()
            .all(|(_, status)| *status == StatusCode::FORBIDDEN),
        "a DA token with a defaulted project_id must be rejected: {:?}",
        observation.statuses
    );
    assert_eq!(observation.outbound_request_delta, 0);
    assert!(observation.api_state_unchanged);
    assert!(observation.config_unchanged);
    assert!(observation.vector_store_unchanged);
}

#[tokio::test]
async fn control_plane_routes_reject_missing_tenant_claims_before_side_effects() {
    let _lock = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let data_dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        ("AGENTOS_AUTH_MODE", "hs256".into()),
        (
            "AGENTOS_JWT_SECRET",
            String::from_utf8(TEST_JWT_SECRET.to_vec()).unwrap(),
        ),
        (
            "AGENTOS_DATA_DIR",
            data_dir.path().to_string_lossy().into_owned(),
        ),
    ]);
    let outbound_requests = Arc::new(AtomicUsize::new(0));
    let mock_requests = outbound_requests.clone();
    let mock = Router::new().route(
        "/v1/models",
        get(move || {
            let requests = mock_requests.clone();
            async move {
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Json(json!({"data": []}))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    write_models_override(data_dir.path(), &format!("http://{address}"));

    let observation = observe_control_plane_requests(
        &jwt_without_tenant(&["DA"]),
        data_dir.path(),
        &outbound_requests,
        address,
    )
    .await;
    assert!(
        observation
            .statuses
            .iter()
            .all(|(_, status)| *status == StatusCode::UNAUTHORIZED),
        "a JWT with no tenant_id must fail closed as 401 for every route: {:?}",
        observation.statuses
    );
    assert_eq!(observation.outbound_request_delta, 0);
    assert!(observation.api_state_unchanged);
    assert!(observation.config_unchanged);
    assert!(observation.vector_store_unchanged);
}
