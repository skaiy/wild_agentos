use jsonwebtoken::{encode, EncodingKey, Header};
use tokio_stream::StreamExt;
use tonic::Code;

use super::seapp::se_kernel_service_server::SeKernelService;
use super::seapp::ExecuteTaskStreamRequest;
use super::AgentOSService;
use crate::api::grpc::auth::{jwt_interceptor, NOT_FOUND_MESSAGE};
use crate::api::http::iam::JwtClaims;
use crate::api::http::TEST_ENV_LOCK;
use crate::config::settings::Settings;

const TEST_JWT_SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";

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

#[tokio::test]
async fn isolation_contract_grpc_rejects_unauthenticated_and_cross_tenant_execute_task_stream() {
    let _guard = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
    let previous_jwt_secret = std::env::var_os("AGENTOS_JWT_SECRET");
    let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
    std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
    std::env::set_var(
        "AGENTOS_JWT_SECRET",
        "test-hs256-secret-at-least-32-bytes-long",
    );

    let missing_token = jwt_interceptor(tonic::Request::new(())).expect_err("interceptor");
    assert_eq!(missing_token.code(), Code::Unauthenticated);
    let mut invalid = tonic::Request::new(());
    invalid
        .metadata_mut()
        .insert("authorization", bearer("not-a-jwt"));
    let invalid = jwt_interceptor(invalid).expect_err("bad token");
    assert_eq!(invalid.code(), Code::Unauthenticated);

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("AGENTOS_DATA_DIR", tmp.path());
    let mut settings = Settings::default();
    settings.memory.l0.path = tmp.path().join("l0").display().to_string();
    settings.output.directory = tmp.path().join("out").display().to_string();
    std::fs::create_dir_all(&settings.memory.l0.path).unwrap();
    std::fs::create_dir_all(&settings.output.directory).unwrap();

    let service = AgentOSService::new(settings).expect("service");
    let task_iri = "iri://task/tenant-a-stream";
    let request_id = "approval_do_not_leak";
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
    service
        .event_bus
        .emit(
            task_iri,
            "HUMAN_APPROVAL_REQUIRED",
            "SA",
            &serde_json::json!({
                "request_id": request_id,
                "task_iri": task_iri,
            })
            .to_string(),
        )
        .await;

    let subscribers_before = service.event_bus.subscriber_count();

    let unauthenticated = tonic::Request::new(ExecuteTaskStreamRequest {
        task_iri: task_iri.to_string(),
        prompt: "start".into(),
        ..Default::default()
    });
    let unauthenticated = match service.execute_task_stream(unauthenticated).await {
        Err(status) => status,
        Ok(_) => panic!("missing token must be rejected"),
    };
    assert_eq!(unauthenticated.code(), Code::Unauthenticated);
    assert!(!unauthenticated.message().contains(request_id));

    let mut cross = tonic::Request::new(ExecuteTaskStreamRequest {
        task_iri: task_iri.to_string(),
        prompt: "start".into(),
        ..Default::default()
    });
    cross.metadata_mut().insert(
        "authorization",
        bearer(&jwt("tenant-b", "project-b", vec!["DA"])),
    );
    let cross = match service.execute_task_stream(cross).await {
        Err(status) => status,
        Ok(_) => panic!("cross-tenant ExecuteTaskStream must be rejected"),
    };

    let mut missing = tonic::Request::new(ExecuteTaskStreamRequest {
        task_iri: "iri://task/does-not-exist".into(),
        prompt: "start".into(),
        ..Default::default()
    });
    missing.metadata_mut().insert(
        "authorization",
        bearer(&jwt("tenant-b", "project-b", vec!["DA"])),
    );
    let missing = match service.execute_task_stream(missing).await {
        Err(status) => status,
        Ok(_) => panic!("missing task must be rejected"),
    };
    assert_eq!(cross.code(), Code::NotFound);
    assert_eq!(missing.code(), Code::NotFound);
    assert_eq!(cross.message(), missing.message());
    assert_eq!(cross.message(), NOT_FOUND_MESSAGE);
    assert!(!cross.message().contains(request_id));
    assert!(!service.execution_states.read().await.contains_key(task_iri));
    assert_eq!(
        service.event_bus.subscriber_count(),
        subscribers_before,
        "rejected ExecuteTaskStream must not subscribe to the bus"
    );

    service.shutdown.cancel();

    match previous_auth_mode {
        Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
        None => std::env::remove_var("AGENTOS_AUTH_MODE"),
    }
    match previous_jwt_secret {
        Some(value) => std::env::set_var("AGENTOS_JWT_SECRET", value),
        None => std::env::remove_var("AGENTOS_JWT_SECRET"),
    }
    match previous_data_dir {
        Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
        None => std::env::remove_var("AGENTOS_DATA_DIR"),
    }
}

#[tokio::test]
async fn isolation_contract_execute_task_stream_hides_other_task_and_tenant_events() {
    let _guard = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
    let previous_jwt_secret = std::env::var_os("AGENTOS_JWT_SECRET");
    let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
    std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
    std::env::set_var(
        "AGENTOS_JWT_SECRET",
        "test-hs256-secret-at-least-32-bytes-long",
    );

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("AGENTOS_DATA_DIR", tmp.path());
    let mut settings = Settings::default();
    settings.memory.l0.path = tmp.path().join("l0").display().to_string();
    settings.output.directory = tmp.path().join("out").display().to_string();
    std::fs::create_dir_all(&settings.memory.l0.path).unwrap();
    std::fs::create_dir_all(&settings.output.directory).unwrap();

    let service = AgentOSService::new(settings).expect("service");
    let task_iri = "iri://task/tenant-a-live-stream";
    let other_task_iri = "iri://task/other-task-live-stream";
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

    let mut request = tonic::Request::new(ExecuteTaskStreamRequest {
        task_iri: task_iri.to_string(),
        prompt: "observe".into(),
        ..Default::default()
    });
    request.metadata_mut().insert(
        "authorization",
        bearer(&jwt("tenant-a", "project-a", vec!["DA"])),
    );
    let response = match service.execute_task_stream(request).await {
        Ok(response) => response,
        Err(status) => panic!("in-scope stream must open: {status}"),
    };
    let mut stream = response.into_inner();
    tokio::task::yield_now().await;

    let own_marker = "own-scope-stream-marker";
    let other_task_marker = "other-task-stream-marker";
    let other_tenant_marker = "other-tenant-stream-marker";
    service
        .event_bus
        .emit(
            other_task_iri,
            "TASK_COMPLETED",
            "SA",
            &serde_json::json!({
                "tenant_id": "tenant-b",
                "project_id": "project-b",
                "summary": other_task_marker,
            })
            .to_string(),
        )
        .await;
    service
        .event_bus
        .emit(
            task_iri,
            "TASK_COMPLETED",
            "SA",
            &serde_json::json!({
                "tenant_id": "tenant-b",
                "project_id": "project-b",
                "summary": other_tenant_marker,
            })
            .to_string(),
        )
        .await;
    service
        .event_bus
        .emit(
            task_iri,
            "TASK_COMPLETED",
            "SA",
            &serde_json::json!({
                "tenant_id": "tenant-a",
                "project_id": "project-a",
                "summary": own_marker,
            })
            .to_string(),
        )
        .await;

    let mut saw_own = false;
    let mut leaked = Vec::new();
    let mut seen = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(event))) => {
                seen += 1;
                let summary = match &event.event {
                    Some(super::seapp::execution_event::Event::Completion(completion)) => {
                        completion.summary.clone()
                    }
                    _ => String::new(),
                };
                if event.task_iri != task_iri
                    || summary.contains(other_task_marker)
                    || summary.contains(other_tenant_marker)
                {
                    leaked.push(format!("{} {summary}", event.task_iri));
                }
                if summary.contains(own_marker) {
                    saw_own = true;
                    break;
                }
            }
            Ok(Some(Err(status))) => panic!("stream error: {status}"),
            Ok(None) | Err(_) => break,
        }
    }
    service.shutdown.cancel();

    assert!(
        leaked.is_empty(),
        "open stream forwarded an out-of-scope event: {leaked:?}"
    );
    assert!(
        saw_own,
        "in-scope marker was not delivered on the open stream (saw {seen} events)"
    );

    match previous_auth_mode {
        Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
        None => std::env::remove_var("AGENTOS_AUTH_MODE"),
    }
    match previous_jwt_secret {
        Some(value) => std::env::set_var("AGENTOS_JWT_SECRET", value),
        None => std::env::remove_var("AGENTOS_JWT_SECRET"),
    }
    match previous_data_dir {
        Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
        None => std::env::remove_var("AGENTOS_DATA_DIR"),
    }
}
