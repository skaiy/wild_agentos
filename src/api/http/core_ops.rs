//! 核心语义操作：节点/投影/事件、黑板与批处理运维、KG import/query。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装。

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::knowledge_graph::rdf_mapper::RdfMapper;
use crate::knowledge_graph::store::{KnowledgeGraphError, KnowledgeGraphStore};
use crate::knowledge_graph::types::{EdgeDef, LLMExtractionOutput, NodeDef};
use crate::memory::l2_blackboard::QueryFilter;

use super::iam::{AuthMethod, UserIdentity};
use super::AppState;

#[derive(Deserialize)]
pub struct NodeWriteRequest {
    pub task_iri: String,
    pub json_ld: String,
    pub created_by: Option<String>,
}

#[derive(Deserialize)]
pub struct ProjectionRequest {
    pub task_iri: String,
    pub frame_name: Option<String>,
    pub params: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
pub struct KgImportRequest {
    pub nodes: Vec<NodeDef>,
    #[serde(default)]
    pub edges: Vec<EdgeDef>,
    pub graph: String,
    #[serde(default = "default_true")]
    pub clear_before: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
pub struct KgQueryRequest {
    pub sparql: String,
    pub named_graph: Option<String>,
}

pub(crate) async fn write_node_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<NodeWriteRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorize_core_write(&state, &identity, &req.task_iri).await {
        return response;
    }
    match state
        .core
        .write_node(&req.task_iri, &req.json_ld, None, req.created_by.as_deref())
        .await
    {
        Ok(node_iri) => (
            StatusCode::CREATED,
            Json(json!({"node_iri": node_iri, "accepted": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"accepted": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn get_projection_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ProjectionRequest>,
) -> impl IntoResponse {
    let frame = req
        .frame_name
        .unwrap_or_else(|| "reference_only".to_string());
    let params = req.params.unwrap_or_default();
    match state
        .core
        .projection
        .project(&req.task_iri, &frame, params)
        .await
    {
        Ok(projection) => Json(json!({
            "projection": serde_json::from_str::<Value>(&projection).ok(),
            "frame": frame,
            "task_iri": req.task_iri,
        })),
        Err(e) => Json(json!({"error": e.to_string(), "task_iri": req.task_iri})),
    }
}

pub(crate) async fn read_node_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(node_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.core.read_node(&node_iri).await {
        Ok(Some(node)) => Json(json!({
            "found": true,
            "json_ld": node.json_ld,
        })),
        Ok(None) => Json(json!({"found": false})),
        Err(e) => Json(json!({"found": false, "error": e.to_string()})),
    }
}

pub(crate) async fn emit_event_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let Some(task_iri) = payload
        .get("task_iri")
        .and_then(|v| v.as_str())
        .filter(|task_iri| !task_iri.trim().is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "task_iri is required"})),
        )
            .into_response();
    };
    if let Err(response) = authorize_core_write(&state, &identity, task_iri).await {
        return response;
    }
    let event_type = payload
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("CUSTOM");
    let source = payload
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("http_api");
    let event_id = state
        .core
        .emit_event(task_iri, event_type, source, &payload.to_string())
        .await;
    Json(json!({"event_id": event_id, "status": "emitted"})).into_response()
}

/// Authorize writes to a task-scoped blackboard/event stream.
///
/// A platform DA must still be a verified JWT identity. All other callers must
/// have verified isolation claims matching the task's persisted scope. Missing
/// or legacy task scope is deliberately denied rather than inferred from a
/// request body.
async fn authorize_core_write(
    state: &AppState,
    identity: &UserIdentity,
    task_iri: &str,
) -> Result<(), axum::response::Response> {
    if identity.auth_method == AuthMethod::Jwt && identity.has_role("DA") {
        return Ok(());
    }

    let Some(claims) = identity.isolation_claims() else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims required for core writes"})),
        )
            .into_response());
    };

    let task = match state.core.read_node(task_iri).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response())
        }
        Err(error) => {
            tracing::warn!(%task_iri, "failed to read task scope: {}", error);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to verify task scope"})),
            )
                .into_response());
        }
    };
    let task_scope: Value = match serde_json::from_str(&task.json_ld) {
        Ok(scope) => scope,
        Err(_) => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "task has no verified isolation scope"})),
            )
                .into_response())
        }
    };
    let in_scope = task_scope.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
        && task_scope.get("project_id").and_then(Value::as_str) == Some(claims.project_id());
    if !in_scope {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "task is outside the verified isolation scope"})),
        )
            .into_response());
    }
    Ok(())
}

pub(crate) async fn stream_batch_events_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let event_bus = state.core.events.clone();
    let mut rx = event_bus.subscribe();

    let stream = async_stream::stream! {
        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => {
                    tracing::info!("batch SSE stream closed during shutdown");
                    break;
                }
                result = rx.recv() => match result {
                Ok(event) => {
                    if !event.event_type.starts_with("BATCH_") {
                        continue;
                    }
                    let payload: Value =
                        serde_json::from_str(&event.payload).unwrap_or(Value::Null);
                    let data = json!({
                        "channel": "batch",
                        "event_type": event.event_type,
                        "source": event.source_agent_iri,
                        "task_iri": event.task_iri,
                        "timestamp": event.timestamp.to_rfc3339(),
                        "payload": payload,
                    });
                    yield Ok::<Event, Infallible>(
                        Event::default()
                            .event("batch")
                            .data(data.to_string()),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                },
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ============================================================
// 方案A 平台运维态：L2 黑板浏览器（只读）+ 批处理 Agent 运维台
// ============================================================

/// GET /api/v1/blackboard/tasks — 列出黑板上所有任务（平台/任务态，跨租户）。
pub(crate) async fn list_blackboard_tasks_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let tasks = state.core.blackboard.list_task_summaries();
    Json(json!({ "count": tasks.len(), "tasks": tasks }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct BlackboardNodesQuery {
    task_iri: String,
    role: Option<String>,
    node_type: Option<String>,
    cycle_id: Option<String>,
}

/// GET /api/v1/blackboard/nodes?task_iri=..&role=..&node_type=..&cycle_id=..
/// 读取指定任务下的节点（只读），支持角色/类型/周期多维过滤。task_iri 以查询参数传入以规避 IRI 内含斜杠。
pub(crate) async fn list_blackboard_nodes_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<BlackboardNodesQuery>,
) -> impl IntoResponse {
    let task_iri = q.task_iri.trim().to_string();
    if task_iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "task_iri 不能为空" })),
        );
    }
    let filter = QueryFilter {
        role: q.role.as_deref().and_then(|r| r.parse().ok()),
        cycle_id: q.cycle_id.clone().filter(|s| !s.is_empty()),
        node_type: q.node_type.clone().filter(|s| !s.is_empty()),
    };
    match state
        .core
        .blackboard
        .query_nodes_filtered(&task_iri, &filter)
    {
        Ok(nodes) => {
            let items: Vec<&crate::memory::l2_blackboard::Node> =
                nodes.iter().map(|n| n.as_ref()).collect();
            (
                StatusCode::OK,
                Json(json!({ "task_iri": task_iri, "count": items.len(), "nodes": items })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("读取节点失败: {e}") })),
        ),
    }
}

/// GET /api/v1/batch/agents — 列出所有批处理 Agent 及其状态/窗口/指标/配置摘要（平台运维态）。
pub(crate) async fn list_batch_agents_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mgr_arc = match &state.batch_manager {
        Some(m) => m.clone(),
        None => {
            return Json(json!({ "running": false, "count": 0, "agents": [] }));
        }
    };
    let guard = mgr_arc.lock().await;
    let mgr = match guard.as_ref() {
        Some(m) => m,
        None => return Json(json!({ "running": false, "count": 0, "agents": [] })),
    };
    let names: Vec<String> = mgr.list_agents().iter().map(|s| s.to_string()).collect();
    let agents: Vec<Value> = names
        .iter()
        .map(|name| {
            let status = mgr.get_status(name);
            let window = mgr.get_window_status(name);
            let metrics = mgr.get_metrics(name);
            let cfg = mgr.get_config(name).map(|c| {
                json!({
                    "description": c.description,
                    "enabled": c.enabled,
                    "business_domain": c.business_domain,
                    "model": c.model,
                })
            });
            json!({
                "name": name,
                "status": status,
                "window": window,
                "metrics": metrics,
                "config": cfg,
            })
        })
        .collect();
    Json(json!({ "running": mgr.is_running(), "count": agents.len(), "agents": agents }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchControlRequest {
    action: String,
}

/// POST /api/v1/batch/agents/:name/control — 启停指定批处理 Agent（action: start|stop）。
pub(crate) async fn control_batch_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(req): Json<BatchControlRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let mgr_arc = match &state.batch_manager {
        Some(m) => m.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "批处理系统未启用" })),
            )
                .into_response()
        }
    };
    let mut guard = mgr_arc.lock().await;
    let mgr = match guard.as_mut() {
        Some(m) => m,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "批处理系统未初始化" })),
            )
                .into_response()
        }
    };
    let result = match req.action.as_str() {
        "start" => mgr.start(Some(&name)).await,
        "stop" => mgr.stop(Some(&name)).await,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("不支持的操作: {other}（仅支持 start|stop）") })),
            )
                .into_response()
        }
    };
    match result {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "name": name, "action": req.action, "status": mgr.get_status(&name) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{:?}", e) })),
        )
            .into_response(),
    }
}

/// Expand short namespace prefixes to absolute IRIs for Oxigraph.
/// e.g. "aps:Bench" → "http://aps.local/ontology/Bench"
///      "graph:aps/benches" → "http://aps.local/graph/benches"
///      "rdfs:subClassOf" → "http://www.w3.org/2000/01/rdf-schema#subClassOf"
pub(crate) fn expand_iri(s: &str) -> String {
    if s.contains('/') && (s.starts_with("http://") || s.starts_with("https://")) {
        return s.to_string();
    }
    if let Some(rest) = s.strip_prefix("aps:") {
        format!("http://aps.local/ontology/{}", rest)
    } else if let Some(rest) = s.strip_prefix("graph:aps/") {
        format!("http://aps.local/graph/{}", rest)
    } else if let Some(rest) = s.strip_prefix("rdfs:") {
        format!("http://www.w3.org/2000/01/rdf-schema#{}", rest)
    } else if let Some(rest) = s.strip_prefix("rdf:") {
        format!("http://www.w3.org/1999/02/22-rdf-syntax-ns#{}", rest)
    } else {
        s.to_string()
    }
}

fn expand_extraction(mut extraction: LLMExtractionOutput) -> LLMExtractionOutput {
    for node in &mut extraction.nodes {
        node.node_type = expand_iri(&node.node_type);
    }
    for edge in &mut extraction.edges {
        edge.relation = expand_iri(&edge.relation);
    }
    extraction
}

pub(crate) async fn kg_import_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<KgImportRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "verified isolation claims required for graph storage"})),
            )
        }
    };
    let store = state.kg_store.clone();
    let graph_iri = match claims.graph_iri() {
        Ok(graph_iri) => graph_iri,
        Err(e) => {
            tracing::warn!("KG import invalid verified graph scope: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "invalid verified graph scope"})),
            );
        }
    };

    if req.clear_before {
        let clear = format!("DELETE WHERE {{ GRAPH <{}> {{ ?s ?p ?o . }} }}", graph_iri);
        if let Err(e) = store.update(&clear) {
            tracing::warn!(graph = %graph_iri, "KG clear skipped: {}", e);
        }
    }

    let extraction = expand_extraction(LLMExtractionOutput {
        nodes: req.nodes,
        edges: req.edges,
    });
    let result = RdfMapper::map_extraction(&extraction, &graph_iri);

    let kg = match KnowledgeGraphStore::with_shared_store(store) {
        Ok(kg) => kg,
        Err(e) => {
            tracing::error!(error = %e, "KG store initialization failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "knowledge graph storage unavailable"})),
            );
        }
    };

    match kg.write_quads_for_claims(claims, &result.quads) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "entity_count": result.entity_count,
                "relation_count": result.relation_count,
                "quad_count": result.quads.len(),
                "graph": graph_iri,
            })),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "KG import failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    }
}

pub(crate) async fn kg_query_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<KgQueryRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "verified isolation claims required for graph storage"})),
            )
        }
    };
    let store = state.kg_store.clone();
    let kg = match KnowledgeGraphStore::with_shared_store(store) {
        Ok(kg) => kg,
        Err(e) => {
            tracing::error!(error = %e, "KG store initialization failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "knowledge graph storage unavailable"})),
            );
        }
    };

    match kg.query_sparql_for_claims(claims, &req.sparql) {
        Ok(results) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "results": results,
                "count": results.len(),
            })),
        ),
        Err(KnowledgeGraphError::Query { message })
        | Err(KnowledgeGraphError::InvalidScope { message }) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": message})))
        }
        Err(e) => {
            tracing::error!(error = %e, "KG query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "knowledge graph query unavailable"})),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        routing::post,
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::{emit_event_handler, write_node_handler};
    use crate::{
        api::http::{iam::JwtClaims, ApiUsageState, AppState, TEST_ENV_LOCK},
        core::core_types::{CoreConfig, SemanticCore},
        gateway::unified_gateway::UnifiedGateway,
        tools::prompt_registry::PromptRegistry,
    };

    const TEST_JWT_SECRET: &[u8] = b"test-hs256-secret-at-least-32-bytes-long";

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

    fn test_state(tmp: &std::path::Path) -> Arc<AppState> {
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: tmp.join("l0").display().to_string(),
                event_buffer_size: 10,
                enable_metrics: false,
                eviction_config: None,
            })
            .unwrap(),
        );
        Arc::new(AppState {
            core,
            gateway: Arc::new(test_gateway()),
            kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
            config_info: Arc::new(tokio::sync::RwLock::new(json!({}))),
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
            api_clients: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_keys: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_usage: Arc::new(ApiUsageState::default()),
            online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
            online_corpus_queue_capacity: 10,
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

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

    async fn response_status(router: &Router, request: Request<Body>) -> StatusCode {
        router.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn core_write_routes_require_claims_and_reject_cross_scope() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let previous_jwt_secret = std::env::var_os("AGENTOS_JWT_SECRET");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::set_var(
            "AGENTOS_JWT_SECRET",
            "test-hs256-secret-at-least-32-bytes-long",
        );
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let task_iri = "iri://task/test-scope";
        state
            .core
            .blackboard
            .write_node(
                task_iri,
                &json!({
                    "@id": task_iri,
                    "@type": "Task",
                    "tenant_id": "tenant-a",
                    "project_id": "project-a",
                })
                .to_string(),
                &state.core.config,
            )
            .unwrap();
        let router = Router::new()
            .route("/api/v1/nodes", post(write_node_handler))
            .route("/api/v1/events", post(emit_event_handler))
            .with_state(state.clone());

        for (uri, body) in [
            (
                "/api/v1/nodes",
                json!({"task_iri": task_iri, "json_ld": "{\"@type\":\"Note\"}"}),
            ),
            (
                "/api/v1/events",
                json!({"task_iri": task_iri, "event_type": "CUSTOM"}),
            ),
        ] {
            let request = Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            assert_eq!(
                response_status(&router, request).await,
                StatusCode::UNAUTHORIZED
            );
        }

        let invalid_bearer = Request::builder()
            .method("POST")
            .uri("/api/v1/events")
            .header("authorization", "Bearer invalid-token")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"task_iri": task_iri, "event_type": "CUSTOM"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            response_status(&router, invalid_bearer).await,
            StatusCode::UNAUTHORIZED
        );

        for (tenant, project) in [("tenant-b", "project-a"), ("tenant-a", "project-b")] {
            for (uri, body) in [
                (
                    "/api/v1/nodes",
                    json!({"task_iri": task_iri, "json_ld": "{\"@type\":\"Note\"}"}),
                ),
                (
                    "/api/v1/events",
                    json!({"task_iri": task_iri, "event_type": "CUSTOM"}),
                ),
            ] {
                let request = Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(
                        "authorization",
                        format!("Bearer {}", jwt(tenant, project, vec![])),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                assert_eq!(
                    response_status(&router, request).await,
                    StatusCode::FORBIDDEN
                );
            }
        }

        let node_request = Request::builder()
            .method("POST")
            .uri("/api/v1/nodes")
            .header(
                "authorization",
                format!("Bearer {}", jwt("tenant-a", "project-a", vec![])),
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"task_iri": task_iri, "json_ld": "{\"@type\":\"Note\"}"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            response_status(&router, node_request).await,
            StatusCode::CREATED
        );

        let event_request = Request::builder()
            .method("POST")
            .uri("/api/v1/events")
            .header(
                "authorization",
                format!("Bearer {}", jwt("tenant-a", "project-a", vec![])),
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"task_iri": task_iri, "event_type": "CUSTOM"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            response_status(&router, event_request).await,
            StatusCode::OK
        );

        let da_request = Request::builder()
            .method("POST")
            .uri("/api/v1/events")
            .header(
                "authorization",
                format!("Bearer {}", jwt("tenant-b", "project-a", vec!["DA"])),
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"task_iri": task_iri, "event_type": "PLATFORM_AUDIT"}).to_string(),
            ))
            .unwrap();
        assert_eq!(response_status(&router, da_request).await, StatusCode::OK);

        let missing_task = Request::builder()
            .method("POST")
            .uri("/api/v1/events")
            .header(
                "authorization",
                format!("Bearer {}", jwt("tenant-a", "project-a", vec![])),
            )
            .header("content-type", "application/json")
            .body(Body::from(json!({"event_type": "CUSTOM"}).to_string()))
            .unwrap();
        let response = router.oneshot(missing_task).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"], "task_iri is required");

        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
        match previous_jwt_secret {
            Some(value) => std::env::set_var("AGENTOS_JWT_SECRET", value),
            None => std::env::remove_var("AGENTOS_JWT_SECRET"),
        }
    }
}
