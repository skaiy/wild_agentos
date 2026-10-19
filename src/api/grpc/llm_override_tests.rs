//! gRPC LLM override security regression tests (#450).

use std::sync::{Arc, Mutex};

use axum::{http::HeaderMap, routing::post, Json, Router};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::json;
use tokio::net::TcpListener;
use tonic::{Code, Request};

use super::seapp::se_kernel_service_server::SeKernelService;
use super::seapp::{ChatStreamRequest, ExecuteStageRequest};
use super::AgentOSService;
use crate::api::grpc::llm_override::EXPLICIT_API_KEY_REQUIRED;
use crate::api::http::iam::JwtClaims;
use crate::api::http::TEST_ENV_LOCK;
use crate::config::settings::Settings;

const TEST_JWT_SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";
const SAVED_KEY: &str = "test-only-saved-gateway-key";

fn jwt(tenant_id: &str, project_id: &str, roles: Vec<&str>) -> String {
    encode(
        &Header::default(),
        &JwtClaims {
            sub: "test-user".into(),
            tenant_id: tenant_id.into(),
            project_id: Some(project_id.into()),
            roles: roles.into_iter().map(str::to_owned).collect(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        },
        &EncodingKey::from_secret(TEST_JWT_SECRET),
    )
    .unwrap()
}

fn bearer(token: &str) -> tonic::metadata::MetadataValue<tonic::metadata::Ascii> {
    format!("Bearer {token}").parse().unwrap()
}

struct Upstream {
    base: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl Upstream {
    async fn start() -> Self {
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let record = seen.clone();
        let mock = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap| {
                let record = record.clone();
                async move {
                    record.lock().unwrap().push(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                    );
                    Json(json!({
                        "choices": [{"message": {"content": "ok"}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        Self { base, seen }
    }

    fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }
}

fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

#[tokio::test]
async fn grpc_execute_stage_rejects_foreign_llm_base_without_caller_key() {
    let _guard = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
    let previous_jwt_secret = std::env::var_os("AGENTOS_JWT_SECRET");
    let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
    let previous_execution = std::env::var_os("AGENTOS_INVOCATION_EXECUTION_ENABLED");
    std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
    std::env::set_var(
        "AGENTOS_JWT_SECRET",
        "test-hs256-secret-at-least-32-bytes-long",
    );
    std::env::set_var("AGENTOS_INVOCATION_EXECUTION_ENABLED", "true");

    let saved = Upstream::start().await;
    let foreign = Upstream::start().await;
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("AGENTOS_DATA_DIR", tmp.path());
    let mut settings = Settings::default();
    settings.memory.l0.path = tmp.path().join("l0").display().to_string();
    settings.output.directory = tmp.path().join("out").display().to_string();
    settings.gateway.base_url = saved.base.clone();
    settings.gateway.api_key = SAVED_KEY.to_string();
    std::fs::create_dir_all(&settings.memory.l0.path).unwrap();
    std::fs::create_dir_all(&settings.output.directory).unwrap();

    let service = AgentOSService::new(settings).expect("service");
    let task_iri = "iri://task/grpc-llm-override";
    service
        .blackboard
        .write_node(
            task_iri,
            &serde_json::json!({
                "@id": task_iri,
                "@type": "Task",
                "tenant_id": "tenant-a",
                "project_id": "project-a",
            })
            .to_string(),
            &crate::core::core_types::CoreConfig {
                max_node_size: 1024,
                max_projection_size: 65536,
                l0_storage_path: tmp.path().join("l0").display().to_string(),
                event_buffer_size: 16,
                enable_metrics: false,
                eviction_config: None,
            },
        )
        .unwrap();

    let mut request = Request::new(ExecuteStageRequest {
        task_iri: task_iri.to_string(),
        prompt: "hello".into(),
        llm_base_url: foreign.base.clone(),
        llm_api_key: String::new(),
        ..Default::default()
    });
    request.metadata_mut().insert(
        "authorization",
        bearer(&jwt("tenant-a", "project-a", vec!["DA"])),
    );
    let status = service
        .execute_stage(request)
        .await
        .expect_err("must reject foreign base without key");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), EXPLICIT_API_KEY_REQUIRED);
    assert!(
        foreign.seen().is_empty(),
        "foreign upstream must not be contacted"
    );
    assert!(
        saved.seen().is_empty(),
        "saved upstream must not be contacted"
    );

    restore_env("AGENTOS_AUTH_MODE", previous_auth_mode);
    restore_env("AGENTOS_JWT_SECRET", previous_jwt_secret);
    restore_env("AGENTOS_DATA_DIR", previous_data_dir);
    restore_env("AGENTOS_INVOCATION_EXECUTION_ENABLED", previous_execution);
}

#[tokio::test]
async fn grpc_chat_stream_rejects_foreign_llm_base_without_caller_key() {
    let _guard = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
    let previous_jwt_secret = std::env::var_os("AGENTOS_JWT_SECRET");
    let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
    let previous_execution = std::env::var_os("AGENTOS_INVOCATION_EXECUTION_ENABLED");
    std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
    std::env::set_var(
        "AGENTOS_JWT_SECRET",
        "test-hs256-secret-at-least-32-bytes-long",
    );
    std::env::set_var("AGENTOS_INVOCATION_EXECUTION_ENABLED", "true");

    let saved = Upstream::start().await;
    let foreign = Upstream::start().await;
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("AGENTOS_DATA_DIR", tmp.path());
    let mut settings = Settings::default();
    settings.memory.l0.path = tmp.path().join("l0").display().to_string();
    settings.output.directory = tmp.path().join("out").display().to_string();
    settings.gateway.base_url = saved.base.clone();
    settings.gateway.api_key = SAVED_KEY.to_string();
    std::fs::create_dir_all(&settings.memory.l0.path).unwrap();
    std::fs::create_dir_all(&settings.output.directory).unwrap();

    let service = AgentOSService::new(settings).expect("service");
    let task_iri = "iri://task/grpc-chat-llm-override";
    service
        .blackboard
        .write_node(
            task_iri,
            &serde_json::json!({
                "@id": task_iri,
                "@type": "Task",
                "tenant_id": "tenant-a",
                "project_id": "project-a",
            })
            .to_string(),
            &crate::core::core_types::CoreConfig {
                max_node_size: 1024,
                max_projection_size: 65536,
                l0_storage_path: tmp.path().join("l0").display().to_string(),
                event_buffer_size: 16,
                enable_metrics: false,
                eviction_config: None,
            },
        )
        .unwrap();

    let mut request = Request::new(ChatStreamRequest {
        task_iri: task_iri.to_string(),
        prompt: "hello".into(),
        llm_base_url: foreign.base.clone(),
        llm_api_key: String::new(),
        ..Default::default()
    });
    request.metadata_mut().insert(
        "authorization",
        bearer(&jwt("tenant-a", "project-a", vec!["DA"])),
    );
    let status = match service.chat_stream(request).await {
        Err(status) => status,
        Ok(_) => panic!("must reject foreign base without key"),
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), EXPLICIT_API_KEY_REQUIRED);
    assert!(foreign.seen().is_empty());
    assert!(saved.seen().is_empty());

    restore_env("AGENTOS_AUTH_MODE", previous_auth_mode);
    restore_env("AGENTOS_JWT_SECRET", previous_jwt_secret);
    restore_env("AGENTOS_DATA_DIR", previous_data_dir);
    restore_env("AGENTOS_INVOCATION_EXECUTION_ENABLED", previous_execution);
}
