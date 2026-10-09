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

use super::iam::UserIdentity;
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
    identity: UserIdentity,
    Json(req): Json<ProjectionRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorize_core_read(&state, &identity, &req.task_iri).await {
        return response;
    }
    let frame = req
        .frame_name
        .unwrap_or_else(|| "reference_only".to_string());
    // Frames with a SPARQL template run a CONSTRUCT over the whole blackboard
    // with no task or tenant binding. Until templates are scope-bound, only a
    // platform admin may use them; everyone else gets the missing-node 404.
    let whole_graph = state
        .core
        .projection
        .get_frame(&frame)
        .is_some_and(|f| f.sparql_template.is_some());
    if whole_graph
        && identity
            .require_platform_admin("whole-graph projections")
            .is_err()
    {
        return core_read_not_found();
    }
    let params = req.params.unwrap_or_default();
    let result = if whole_graph {
        // Reached only after `require_platform_admin` above.
        state
            .core
            .projection
            .project_platform_wide(&req.task_iri, &frame, params)
            .await
    } else {
        match identity.isolation_claims() {
            Some(claims) => {
                state
                    .core
                    .projection
                    .project(&req.task_iri, &frame, params, claims)
                    .await
            }
            None => return core_read_not_found(),
        }
    };
    match result {
        Ok(projection) => Json(json!({
            "projection": serde_json::from_str::<Value>(&projection).ok(),
            "frame": frame,
            "task_iri": req.task_iri,
        }))
        .into_response(),
        Err(e) => Json(json!({"error": e.to_string(), "task_iri": req.task_iri})).into_response(),
    }
}

pub(crate) async fn read_node_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(node_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorize_core_read(&state, &identity, &node_iri).await {
        return response;
    }
    match state.core.read_node(&node_iri).await {
        Ok(Some(node)) => Json(json!({
            "found": true,
            "json_ld": node.json_ld,
        }))
        .into_response(),
        Ok(None) => Json(json!({"found": false})).into_response(),
        Err(e) => Json(json!({"found": false, "error": e.to_string()})).into_response(),
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
        .map(str::trim)
        .unwrap_or("CUSTOM");
    // Task lifecycle and terminal events (`TASK_*`) are published only by the
    // server itself; a caller cannot post them, whatever its role (#337).
    if is_reserved_event_type(event_type) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "reserved_event_type",
                "message": "TASK_* lifecycle events are published by the server only",
            })),
        )
            .into_response();
    }
    // Generic ingestion is an allowlist: CUSTOM and EXT_* only. Control-plane
    // and display types go through dedicated routes or the executor (#399).
    if !is_public_ingest_event_type(event_type) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "event_type_not_allowed",
                "message": "POST /api/v1/events accepts only CUSTOM and EXT_* event types",
            })),
        )
            .into_response();
    }
    // The source is set by the server from the verified caller; a `source`
    // member in the body is ignored, so no caller can speak as an executor.
    let source = external_event_source(&identity);
    // Drop the caller's `source` from the stored payload too, so no reader of
    // the payload can mistake it for the event's origin.
    let mut stored = payload.clone();
    if let Some(body) = stored.as_object_mut() {
        body.remove("source");
    }
    let event_id = state
        .core
        .emit_event(task_iri, event_type, &source, &stored.to_string())
        .await;
    Json(json!({"event_id": event_id, "status": "emitted"})).into_response()
}

/// `TASK_*` event types (case-insensitive) are the task lifecycle: created,
/// started, completed, failed, cancelled, archived. Only the server emits them.
pub(crate) fn is_reserved_event_type(event_type: &str) -> bool {
    event_type
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("TASK_"))
}

/// Types a caller may post to `POST /api/v1/events` (#399).
/// `CUSTOM` is exact; `EXT_*` is the `EXT_` prefix. Both are case-sensitive.
pub(crate) fn is_public_ingest_event_type(event_type: &str) -> bool {
    event_type == "CUSTOM" || event_type.starts_with("EXT_")
}

pub(crate) async fn emit_intervention_required_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    emit_control_event(&state, &identity, "INTERVENTION_REQUIRED", payload).await
}

pub(crate) async fn emit_user_supplementary_input_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    emit_control_event(&state, &identity, "USER_SUPPLEMENTARY_INPUT", payload).await
}

pub(crate) async fn emit_human_approval_result_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    emit_control_event(&state, &identity, "HUMAN_APPROVAL_RESULT", payload).await
}

pub(crate) async fn emit_threshold_exceeded_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    emit_control_event(&state, &identity, "THRESHOLD_EXCEEDED", payload).await
}

pub(crate) async fn emit_cycle_iteration_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    emit_control_event(&state, &identity, "CYCLE_ITERATION", payload).await
}

/// Dedicated run-control ingestion. The path selects the event type. The
/// caller must be the task's `user_id` or a DA in that task's tenant and
/// project (#399). Cross-scope answers like a missing task.
async fn emit_control_event(
    state: &AppState,
    identity: &UserIdentity,
    event_type: &str,
    mut payload: Value,
) -> axum::response::Response {
    if identity.isolation_claims().is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "verified isolation claims required for control events"
            })),
        )
            .into_response();
    }
    let Some(task_iri) = payload
        .get("task_iri")
        .and_then(|v| v.as_str())
        .filter(|task_iri| !task_iri.trim().is_empty())
        .map(str::to_string)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "task_iri is required"})),
        )
            .into_response();
    };
    let node = match state.core.read_node(&task_iri).await {
        Ok(Some(node)) => node,
        Ok(None) => return core_read_not_found(),
        Err(error) => {
            tracing::warn!(%task_iri, "failed to read task scope: {}", error);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to verify task scope"})),
            )
                .into_response();
        }
    };
    if let Err(response) = authorize_control_event(&node.json_ld, identity) {
        return *response;
    }
    let source = external_event_source(identity);
    if let Some(body) = payload.as_object_mut() {
        body.remove("source");
        body.insert("event_type".to_string(), json!(event_type));
    }
    let event_id = state
        .core
        .emit_event(&task_iri, event_type, &source, &payload.to_string())
        .await;
    Json(json!({
        "event_id": event_id,
        "status": "emitted",
        "event_type": event_type,
    }))
    .into_response()
}

fn authorize_control_event(
    task_json: &str,
    identity: &UserIdentity,
) -> Result<(), Box<axum::response::Response>> {
    let Some(claims) = identity.isolation_claims() else {
        return Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": "verified isolation claims required for control events"
                })),
            )
                .into_response(),
        ));
    };
    if !task_is_in_scope(task_json, claims) {
        return Err(Box::new(core_read_not_found()));
    }
    let task: Value = serde_json::from_str(task_json).unwrap_or(Value::Null);
    let is_owner = task
        .get("user_id")
        .and_then(Value::as_str)
        .is_some_and(|user_id| user_id == claims.actor_id());
    if is_owner || identity.has_role("DA") {
        return Ok(());
    }
    Err(Box::new(
        (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "control_event_forbidden",
                "message": "run owner or same-scope DA required",
            })),
        )
            .into_response(),
    ))
}

/// `source_agent_iri` of an event posted through `POST /api/v1/events`:
/// `external:http:<sub>`, from the verified caller.
pub(crate) fn external_event_source(identity: &UserIdentity) -> String {
    let sub = identity
        .isolation_claims()
        .map(|claims| claims.actor_id().to_string())
        .unwrap_or_else(|| identity.user_id.clone());
    format!("external:http:{sub}")
}

/// Body returned for both a missing node and a node outside the caller's
/// scope, so reads are not an existence oracle.
fn core_read_not_found() -> axum::response::Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
}

/// Authorize reads of a node or task projection.
///
/// Every caller needs verified claims matching the node's persisted tenant and
/// project. Only a platform admin (`require_platform_admin`) may read across
/// tenants. A node outside the scope answers exactly like a missing node.
async fn authorize_core_read(
    state: &AppState,
    identity: &UserIdentity,
    iri: &str,
) -> Result<(), axum::response::Response> {
    let Some(claims) = identity.isolation_claims() else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims required for core reads"})),
        )
            .into_response());
    };
    if identity.require_platform_admin("core reads").is_ok() {
        return Ok(());
    }
    let node = match state.core.read_node(iri).await {
        Ok(Some(node)) => node,
        Ok(None) => return Err(core_read_not_found()),
        Err(error) => {
            tracing::warn!(%iri, "failed to read node scope: {}", error);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to verify node scope"})),
            )
                .into_response());
        }
    };
    let scope: Value = serde_json::from_str(&node.json_ld).unwrap_or(Value::Null);
    let in_scope = scope.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
        && scope.get("project_id").and_then(Value::as_str) == Some(claims.project_id());
    if in_scope {
        Ok(())
    } else {
        Err(core_read_not_found())
    }
}

/// Authorize writes to a task-scoped blackboard/event stream.
///
/// Callers need verified isolation claims matching the task's persisted tenant
/// and project. A DA role does not cross tenants (#395). Only a platform admin
/// (`require_platform_admin`) may write across tenants. A missing task is 404
/// for every caller, including DA. Missing or legacy task scope is denied
/// rather than inferred from a request body.
async fn authorize_core_write(
    state: &AppState,
    identity: &UserIdentity,
    task_iri: &str,
) -> Result<(), axum::response::Response> {
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
    // The task exists. A platform admin may write it regardless of tenant.
    // Everyone else, including DA, must match the persisted scope.
    if identity.require_platform_admin("core writes").is_ok() {
        return Ok(());
    }
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
    identity: UserIdentity,
) -> impl IntoResponse {
    // Reject before subscribing so no SSE stream is opened for anonymous callers.
    if let Err(error) = identity.require_verified_isolation_claims("batch event stream") {
        return error.into_response();
    }
    let claims = identity
        .isolation_claims()
        .expect("verified isolation claims were required above")
        .clone();
    let event_bus = state.core.events.clone();
    let core = state.core.clone();
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
                    // A batch event is delivered only to subscribers in its
                    // tenant and project (#399). Unscoped events are dropped.
                    if !batch_event_visible(&core, &event, &claims).await {
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

/// A `BATCH_*` event is visible when its task node is in the subscriber's
/// tenant and project. If there is no task node, both `tenant_id` and
/// `project_id` on the payload must match. Anything else is hidden (#399).
async fn batch_event_visible(
    core: &crate::core::core_types::SemanticCore,
    event: &crate::core::event_bus::Event,
    claims: &crate::isolation::IsolationClaims,
) -> bool {
    match core.read_node(&event.task_iri).await {
        Ok(Some(node)) => task_is_in_scope(&node.json_ld, claims),
        Ok(None) => payload_scope_matches(&event.payload, claims),
        Err(_) => false,
    }
}

fn payload_scope_matches(payload: &str, claims: &crate::isolation::IsolationClaims) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return false;
    };
    value.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
        && value.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
}

// ============================================================
// L2 黑板浏览器（claims-scoped 只读）+ 批处理 Agent 运维台
// ============================================================

/// GET /api/v1/blackboard/tasks — list tasks in the verified tenant/project scope.
pub(crate) async fn list_blackboard_tasks_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims required for blackboard access"})),
        )
            .into_response();
    };
    let tasks: Vec<_> = state
        .core
        .blackboard
        .list_task_summaries()
        .into_iter()
        .filter(|summary| {
            state
                .core
                .blackboard
                .read_node(&summary.task_iri)
                .ok()
                .flatten()
                .is_some_and(|task| task_is_in_scope(&task.json_ld, claims))
        })
        .collect();
    Json(json!({ "count": tasks.len(), "tasks": tasks })).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct BlackboardNodesQuery {
    task_iri: String,
    role: Option<String>,
    node_type: Option<String>,
    cycle_id: Option<String>,
}

/// GET /api/v1/blackboard/nodes?task_iri=..&role=..&node_type=..&cycle_id=..
/// Read nodes in a verified task scope, with role/type/cycle filters.
/// `task_iri` remains a query parameter because IRIs may contain slashes.
pub(crate) async fn list_blackboard_nodes_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Query(q): Query<BlackboardNodesQuery>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims required for blackboard access"})),
        )
            .into_response();
    };
    let task_iri = q.task_iri.trim().to_string();
    if task_iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "task_iri 不能为空" })),
        )
            .into_response();
    }
    match state.core.blackboard.read_node(&task_iri) {
        Ok(Some(task)) if task_is_in_scope(&task.json_ld, claims) => {}
        Ok(Some(_)) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "task is outside the verified isolation scope"})),
            )
                .into_response()
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%task_iri, "failed to read task scope: {}", error);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to verify task scope"})),
            )
                .into_response();
        }
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
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("读取节点失败: {e}") })),
        )
            .into_response(),
    }
}

/// Validate a persisted task scope against verified claims.
///
/// This deliberately treats malformed, tenant-only, and otherwise legacy task
/// records as out of scope. The endpoint must not infer scope from task IRI or
/// child nodes.
pub(crate) fn task_is_in_scope(json_ld: &str, claims: &crate::isolation::IsolationClaims) -> bool {
    let Ok(task) = serde_json::from_str::<Value>(json_ld) else {
        return false;
    };
    let is_task = task.get("@type").is_some_and(|kind| {
        kind.as_str() == Some("Task")
            || kind
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|value| value.as_str() == Some("Task")))
    });
    is_task
        && task.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
        && task.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
}

/// GET /api/v1/batch/agents — 列出所有批处理 Agent 及其状态/窗口/指标/配置摘要（平台运维态）。
pub(crate) async fn list_batch_agents_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    if let Err(error) = identity.require_control_plane_da("batch agent operations") {
        return error.into_response();
    }
    let mgr_arc = match &state.batch_manager {
        Some(m) => m.clone(),
        None => {
            return Json(json!({ "running": false, "count": 0, "agents": [] })).into_response();
        }
    };
    let guard = mgr_arc.lock().await;
    let mgr = match guard.as_ref() {
        Some(m) => m,
        None => return Json(json!({ "running": false, "count": 0, "agents": [] })).into_response(),
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
        .into_response()
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
    if let Err(error) = identity.require_control_plane_da("batch agent operations") {
        return error.into_response();
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
// Test-only lock held for the whole test by design (serializes process-global env/state);
// code under test never takes it, so holding it across `.await` cannot deadlock.
#[allow(clippy::await_holding_lock)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        routing::{get, post},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::{
        emit_cycle_iteration_handler, emit_event_handler, emit_human_approval_result_handler,
        emit_intervention_required_handler, emit_threshold_exceeded_handler,
        emit_user_supplementary_input_handler, get_projection_handler,
        list_blackboard_nodes_handler, list_blackboard_tasks_handler, read_node_handler,
        stream_batch_events_handler, write_node_handler,
    };
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
                max_projection_size: 65536,
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
            invocations: crate::api::http::invocations::InvocationsRuntime::unavailable(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    fn jwt(tenant_id: &str, project_id: &str, roles: Vec<&str>) -> String {
        jwt_as("test-user", tenant_id, project_id, roles)
    }

    fn jwt_as(sub: &str, tenant_id: &str, project_id: &str, roles: Vec<&str>) -> String {
        encode(
            &Header::default(),
            &JwtClaims {
                sub: sub.into(),
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

    async fn response_json(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// #337 B1: `TASK_*` lifecycle events cannot be posted (any role, any
    /// spelling), and the source of a posted event is set by the server.
    #[tokio::test]
    async fn events_api_rejects_task_lifecycle_types_and_sets_the_source() {
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
        let task_iri = "iri://task/test-reserved";
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
            .route("/api/v1/events", post(emit_event_handler))
            .with_state(state.clone());
        let post_event = |token: String, body: Value| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/events")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let mut rx = state.core.events.subscribe();

        for token in [
            jwt("tenant-a", "project-a", vec![]),
            jwt("tenant-a", "project-a", vec!["DA"]),
            jwt("tenant-b", "project-z", vec!["DA"]),
        ] {
            for event_type in [
                "TASK_COMPLETED",
                "TASK_FAILED",
                "TASK_STARTED",
                "TASK_CREATED",
                "TASK_CANCELLED",
                "task_completed",
                " TASK_COMPLETED ",
                "Task_Failed",
            ] {
                let request = post_event(
                    token.clone(),
                    json!({
                        "task_iri": task_iri,
                        "event_type": event_type,
                        "source": "SA",
                        "status": "completed",
                    }),
                );
                assert_eq!(
                    response_status(&router, request).await,
                    StatusCode::FORBIDDEN,
                    "{event_type}"
                );
            }
        }
        assert!(rx.try_recv().is_err(), "no reserved event reached the bus");

        let request = post_event(
            jwt("tenant-a", "project-a", vec![]),
            json!({"task_iri": task_iri, "event_type": "CUSTOM", "source": "SA"}),
        );
        assert_eq!(response_status(&router, request).await, StatusCode::OK);
        let event = rx.try_recv().expect("custom event emitted");
        assert_eq!(event.event_type, "CUSTOM");
        assert_eq!(event.source_agent_iri, "external:http:test-user");
        let stored: Value = serde_json::from_str(&event.payload).unwrap();
        assert!(
            stored.get("source").is_none(),
            "caller-written source must not stay in the payload: {stored}"
        );
        assert_eq!(stored["task_iri"], task_iri);

        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
        match previous_jwt_secret {
            Some(value) => std::env::set_var("AGENTOS_JWT_SECRET", value),
            None => std::env::remove_var("AGENTOS_JWT_SECRET"),
        }
    }

    #[tokio::test]
    async fn core_write_routes_require_claims_and_reject_cross_scope() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let previous_jwt_secret = std::env::var_os("AGENTOS_JWT_SECRET");
        let previous_platform = std::env::var_os(crate::api::http::iam::PLATFORM_ADMIN_TENANT_ENV);
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::set_var(
            "AGENTOS_JWT_SECRET",
            "test-hs256-secret-at-least-32-bytes-long",
        );
        std::env::set_var(crate::api::http::iam::PLATFORM_ADMIN_TENANT_ENV, "platform");
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

        // Same-tenant DA may write. Cross-tenant DA is rejected (#395).
        let same_tenant_da = Request::builder()
            .method("POST")
            .uri("/api/v1/events")
            .header(
                "authorization",
                format!("Bearer {}", jwt("tenant-a", "project-a", vec!["DA"])),
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"task_iri": task_iri, "event_type": "CUSTOM"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            response_status(&router, same_tenant_da).await,
            StatusCode::OK
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
                        format!("Bearer {}", jwt(tenant, project, vec!["DA"])),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                assert_eq!(
                    response_status(&router, request).await,
                    StatusCode::FORBIDDEN,
                    "DA {tenant}/{project} {uri}"
                );
            }
        }

        let missing_for_da = Request::builder()
            .method("POST")
            .uri("/api/v1/events")
            .header(
                "authorization",
                format!("Bearer {}", jwt("tenant-a", "project-a", vec!["DA"])),
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"task_iri": "iri://task/missing", "event_type": "CUSTOM"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            response_status(&router, missing_for_da).await,
            StatusCode::NOT_FOUND
        );

        let admin_request = Request::builder()
            .method("POST")
            .uri("/api/v1/events")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    jwt(
                        "platform",
                        "ops",
                        vec![crate::api::http::iam::PLATFORM_ADMIN_ROLE]
                    )
                ),
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"task_iri": task_iri, "event_type": "EXT_ADMIN_NOTE"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            response_status(&router, admin_request).await,
            StatusCode::OK
        );

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
        match previous_platform {
            Some(value) => {
                std::env::set_var(crate::api::http::iam::PLATFORM_ADMIN_TENANT_ENV, value)
            }
            None => std::env::remove_var(crate::api::http::iam::PLATFORM_ADMIN_TENANT_ENV),
        }
    }

    fn scoped_task(state: &AppState, iri: &str, tenant: &str, project: &str, user_id: &str) {
        state
            .core
            .blackboard
            .write_node(
                iri,
                &json!({
                    "@id": iri,
                    "@type": "Task",
                    "tenant_id": tenant,
                    "project_id": project,
                    "user_id": user_id,
                })
                .to_string(),
                &state.core.config,
            )
            .unwrap();
    }

    struct Hs256Env {
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl Hs256Env {
        fn set() -> Self {
            let names = ["AGENTOS_AUTH_MODE", "AGENTOS_JWT_SECRET"];
            let previous = names
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect();
            std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
            std::env::set_var(
                "AGENTOS_JWT_SECRET",
                "test-hs256-secret-at-least-32-bytes-long",
            );
            Self { previous }
        }
    }

    impl Drop for Hs256Env {
        fn drop(&mut self) {
            for (name, value) in self.previous.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    /// #399: generic ingestion is CUSTOM / EXT_* only. Control types, BATCH_*
    /// and display types are rejected there; control types are accepted on
    /// their dedicated routes only for the run owner or a same-scope DA.
    #[tokio::test]
    async fn control_events_reject_non_owners_and_generic_ingest_is_an_allowlist() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = Hs256Env::set();
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let task_iri = "iri://task/control";
        scoped_task(&state, task_iri, "tenant-a", "project-a", "owner-user");
        let router = Router::new()
            .route("/api/v1/events", post(emit_event_handler))
            .route(
                "/api/v1/control-events/intervention-required",
                post(emit_intervention_required_handler),
            )
            .route(
                "/api/v1/control-events/user-supplementary-input",
                post(emit_user_supplementary_input_handler),
            )
            .route(
                "/api/v1/control-events/human-approval-result",
                post(emit_human_approval_result_handler),
            )
            .route(
                "/api/v1/control-events/threshold-exceeded",
                post(emit_threshold_exceeded_handler),
            )
            .route(
                "/api/v1/control-events/cycle-iteration",
                post(emit_cycle_iteration_handler),
            )
            .with_state(state.clone());
        let owner = jwt_as("owner-user", "tenant-a", "project-a", vec![]);
        let stranger = jwt_as("other-user", "tenant-a", "project-a", vec![]);
        let da = jwt_as("da-user", "tenant-a", "project-a", vec!["DA"]);
        let other_project_da = jwt_as("da-user", "tenant-a", "project-b", vec!["DA"]);
        let cross_da = jwt_as("da-user", "tenant-b", "project-a", vec!["DA"]);
        let post = |token: &str, uri: &str, event_type: &str| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"task_iri": task_iri, "event_type": event_type, "source": "SA"})
                        .to_string(),
                ))
                .unwrap()
        };
        let mut rx = state.core.events.subscribe();

        for (uri, event_type) in [
            (
                "/api/v1/control-events/intervention-required",
                "INTERVENTION_REQUIRED",
            ),
            (
                "/api/v1/control-events/user-supplementary-input",
                "USER_SUPPLEMENTARY_INPUT",
            ),
            (
                "/api/v1/control-events/human-approval-result",
                "HUMAN_APPROVAL_RESULT",
            ),
            (
                "/api/v1/control-events/threshold-exceeded",
                "THRESHOLD_EXCEEDED",
            ),
            ("/api/v1/control-events/cycle-iteration", "CYCLE_ITERATION"),
        ] {
            let (status, body) = response_json(&router, post(&stranger, uri, event_type)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{event_type} non-owner");
            assert_eq!(body["error"], "control_event_forbidden");

            let (status, _) =
                response_json(&router, post(&other_project_da, uri, event_type)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{event_type} other project");

            let (status, _) = response_json(&router, post(&cross_da, uri, event_type)).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{event_type} cross-tenant DA"
            );

            let (status, body) = response_json(&router, post(&owner, uri, event_type)).await;
            assert_eq!(status, StatusCode::OK, "{event_type} owner: {body}");
            let event = rx.try_recv().expect("owner control event");
            assert_eq!(event.event_type, event_type);
            assert_eq!(event.source_agent_iri, "external:http:owner-user");

            let (status, body) = response_json(&router, post(&da, uri, event_type)).await;
            assert_eq!(status, StatusCode::OK, "{event_type} DA: {body}");
            let event = rx.try_recv().expect("DA control event");
            assert_eq!(event.event_type, event_type);
            assert_eq!(event.source_agent_iri, "external:http:da-user");

            let (status, body) =
                response_json(&router, post(&owner, "/api/v1/events", event_type)).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "generic ingest must reject {event_type}: {body}"
            );
            assert_eq!(body["error"], "event_type_not_allowed");
        }

        for event_type in [
            "BATCH_AGENT_STARTED",
            "ACT_COMPLETED",
            "LLM_CONTENT",
            "custom",
        ] {
            let (status, body) =
                response_json(&router, post(&owner, "/api/v1/events", event_type)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{event_type}: {body}");
            assert_eq!(body["error"], "event_type_not_allowed");
        }
        let (status, _) = response_json(&router, post(&owner, "/api/v1/events", "EXT_NOTE")).await;
        assert_eq!(status, StatusCode::OK);
        let event = rx.try_recv().expect("EXT_NOTE");
        assert_eq!(event.event_type, "EXT_NOTE");
        assert!(
            rx.try_recv().is_err(),
            "rejected types must not reach the bus"
        );
    }

    async fn collect_sse(response: axum::response::Response, wait: std::time::Duration) -> String {
        use tokio_stream::StreamExt;
        let mut stream = response.into_body().into_data_stream();
        let mut buf = String::new();
        let deadline = tokio::time::Instant::now() + wait;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(chunk))) => buf.push_str(&String::from_utf8_lossy(&chunk)),
                _ => break,
            }
        }
        buf
    }

    /// #399: a BATCH event for tenant A is not delivered to tenant B.
    #[tokio::test]
    async fn batch_sse_is_not_delivered_across_tenants() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = Hs256Env::set();
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let task_a = "iri://task/batch-a";
        let task_b = "iri://task/batch-b";
        scoped_task(&state, task_a, "tenant-a", "project-a", "owner-a");
        scoped_task(&state, task_b, "tenant-b", "project-b", "owner-b");
        let router = Router::new()
            .route("/api/v1/batch/events", get(stream_batch_events_handler))
            .with_state(state.clone());
        let open = |token: &str| {
            Request::builder()
                .uri("/api/v1/batch/events")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap()
        };
        let token_a = jwt("tenant-a", "project-a", vec![]);
        let token_b = jwt("tenant-b", "project-b", vec![]);
        let response_a = router.clone().oneshot(open(&token_a)).await.unwrap();
        let response_b = router.clone().oneshot(open(&token_b)).await.unwrap();
        assert_eq!(response_a.status(), StatusCode::OK);
        assert_eq!(response_b.status(), StatusCode::OK);

        state
            .core
            .emit_event(
                task_a,
                "BATCH_AGENT_STARTED",
                "batch:manager",
                &json!({"marker": "only-tenant-a"}).to_string(),
            )
            .await;
        state
            .core
            .emit_event(
                task_b,
                "BATCH_AGENT_STARTED",
                "batch:manager",
                &json!({"marker": "only-tenant-b"}).to_string(),
            )
            .await;
        state
            .core
            .emit_event(
                "batch://unscoped",
                "BATCH_EXTRACTION_COMPLETED",
                "batch:manager",
                &json!({"marker": "unscoped-batch"}).to_string(),
            )
            .await;
        state
            .core
            .emit_event(
                "batch://payload-scope",
                "BATCH_ENTITY_DETECTED",
                "batch:manager",
                &json!({
                    "marker": "payload-tenant-a",
                    "tenant_id": "tenant-a",
                    "project_id": "project-a",
                })
                .to_string(),
            )
            .await;

        let wait = std::time::Duration::from_millis(800);
        let (body_a, body_b) =
            tokio::join!(collect_sse(response_a, wait), collect_sse(response_b, wait));
        assert!(
            body_a.contains("only-tenant-a"),
            "tenant A must see its own batch event: {body_a}"
        );
        assert!(
            !body_a.contains("only-tenant-b"),
            "tenant A must not see tenant B: {body_a}"
        );
        assert!(
            body_b.contains("only-tenant-b"),
            "tenant B must see its own batch event: {body_b}"
        );
        assert!(
            !body_b.contains("only-tenant-a"),
            "tenant B must not see tenant A: {body_b}"
        );
        assert!(!body_a.contains("unscoped-batch"), "{body_a}");
        assert!(!body_b.contains("unscoped-batch"), "{body_b}");
        assert!(body_a.contains("payload-tenant-a"), "{body_a}");
        assert!(!body_b.contains("payload-tenant-a"), "{body_b}");
    }

    async fn raw_response(router: &Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, body.to_vec())
    }

    fn node_read_request(iri: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(format!(
            "/api/v1/nodes/{}",
            iri.replace(':', "%3A").replace('/', "%2F")
        ));
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    fn projection_request(iri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/projections")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(json!({"task_iri": iri}).to_string()))
            .unwrap()
    }

    fn write_scoped_task(state: &AppState, iri: &str, tenant: &str, project: &str, extra: Value) {
        let mut node = json!({
            "@id": iri,
            "@type": "Task",
            "tenant_id": tenant,
            "project_id": project,
        });
        if let (Some(node), Some(extra)) = (node.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                node.insert(key.clone(), value.clone());
            }
        }
        state
            .core
            .blackboard
            .write_node(iri, &node.to_string(), &state.core.config)
            .unwrap();
    }

    fn projection_frame_request(iri: &str, frame: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/projections")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                json!({"task_iri": iri, "frame_name": frame}).to_string(),
            ))
            .unwrap()
    }

    struct ProjectionEnv {
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl ProjectionEnv {
        fn set() -> Self {
            let vars = [
                ("AGENTOS_AUTH_MODE", "hs256"),
                (
                    "AGENTOS_JWT_SECRET",
                    "test-hs256-secret-at-least-32-bytes-long",
                ),
                (crate::api::http::iam::PLATFORM_ADMIN_TENANT_ENV, "platform"),
            ];
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

    impl Drop for ProjectionEnv {
        fn drop(&mut self) {
            for (name, value) in self.previous.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    /// Every registered frame with a `sparql_template` runs a whole-blackboard
    /// CONSTRUCT, so only a platform admin may use it; everyone else gets the
    /// same 404 as a missing node, even on their own task.
    #[tokio::test]
    async fn isolation_contract_core_projection_sparql_frames_platform_admin_only() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = ProjectionEnv::set();
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let task_b = "iri://task/tenant-b-own";
        write_scoped_task(&state, task_b, "tenant-b", "project-b", json!({}));
        let router = Router::new()
            .route("/api/v1/projections", post(get_projection_handler))
            .with_state(state.clone());

        let mut sparql_frames: Vec<String> = state
            .core
            .projection
            .list_frames()
            .into_iter()
            .filter(|frame| frame.sparql_template.is_some())
            .map(|frame| frame.name.clone())
            .collect();
        sparql_frames.sort();
        assert!(
            sparql_frames.len() >= 7,
            "expected the SPARQL frames, got {sparql_frames:?}"
        );

        let not_found = br#"{"error":"not found"}"#.to_vec();
        let (missing_status, missing) = raw_response(
            &router,
            projection_frame_request(
                "iri://task/missing",
                "reference_only",
                &jwt("tenant-b", "project-b", vec![]),
            ),
        )
        .await;
        assert_eq!(missing_status, StatusCode::NOT_FOUND);
        assert_eq!(missing, not_found);

        let admin = jwt(
            "platform",
            "ops",
            vec![crate::api::http::iam::PLATFORM_ADMIN_ROLE],
        );
        for frame in &sparql_frames {
            for token in [
                jwt("tenant-b", "project-b", vec![]),
                jwt("tenant-b", "project-b", vec!["DA"]),
            ] {
                let (status, body) =
                    raw_response(&router, projection_frame_request(task_b, frame, &token)).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "frame {frame}");
                assert_eq!(body, missing, "frame {frame} must look like a missing node");
            }
            let (status, _) =
                raw_response(&router, projection_frame_request(task_b, frame, &admin)).await;
            assert_eq!(status, StatusCode::OK, "platform admin frame {frame}");
        }

        // Frames without a SPARQL template stay available on the caller's own task.
        let (status, _) = raw_response(
            &router,
            projection_frame_request(
                task_b,
                "reference_only",
                &jwt("tenant-b", "project-b", vec![]),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn isolation_contract_core_projection_never_leaks_other_tenant_task() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = ProjectionEnv::set();
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let canary = "tenant-a-canary-goal-7f3c";
        write_scoped_task(
            &state,
            "iri://task/tenant-a-secret",
            "tenant-a",
            "project-a",
            json!({
                "goal": canary,
                "constraints": [canary],
                "summary": canary,
                "status": "active",
            }),
        );
        // The SPARQL frames read `ex:` triples, which the JSON-LD node write
        // above does not produce. Seed them directly so the canary is really
        // reachable by a whole-graph CONSTRUCT.
        state
            .core
            .blackboard
            .sparql_update(&format!(
                r#"PREFIX ex: <https://wildagentos.org/ontology/>
                INSERT DATA {{
                    <iri://task/tenant-a-secret> a ex:Task ;
                        ex:summary "{canary}" ;
                        ex:goal "{canary}" ;
                        ex:constraints "{canary}" ;
                        ex:status "active" .
                }}"#
            ))
            .unwrap();
        let task_b = "iri://task/tenant-b-own";
        write_scoped_task(&state, task_b, "tenant-b", "project-b", json!({}));
        let router = Router::new()
            .route("/api/v1/projections", post(get_projection_handler))
            .with_state(state.clone());

        // Positive control: the whole-graph frame really returns the canary
        // (platform admin is allowed to run it), so the absence check below
        // is meaningful.
        let admin = jwt(
            "platform",
            "ops",
            vec![crate::api::http::iam::PLATFORM_ADMIN_ROLE],
        );
        let (status, body) =
            raw_response(&router, projection_frame_request(task_b, "pa_init", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&body).contains(canary),
            "seeded canary must be visible to the whole-graph pa_init frame: {}",
            String::from_utf8_lossy(&body)
        );

        let user_b = jwt("tenant-b", "project-b", vec![]);
        for frame in ["pa_init", "workspace_overview", "summary_only", "da_input"] {
            let (status, body) =
                raw_response(&router, projection_frame_request(task_b, frame, &user_b)).await;
            // Canary first, so a regression is reported as the leak it is.
            assert!(
                !String::from_utf8_lossy(&body).contains(canary),
                "frame {frame} leaked tenant A's task"
            );
            assert_eq!(status, StatusCode::NOT_FOUND, "frame {frame}");
        }
    }

    #[tokio::test]
    async fn isolation_contract_core_reads_cross_tenant_matches_missing_404() {
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
        let node_b = "iri://task/tenant-b-private";
        state
            .core
            .blackboard
            .write_node(
                node_b,
                &json!({
                    "@id": node_b,
                    "@type": "Task",
                    "tenant_id": "tenant-b",
                    "project_id": "project-b",
                    "note": "tenant-b-plan",
                })
                .to_string(),
                &state.core.config,
            )
            .unwrap();
        let router = Router::new()
            .route("/api/v1/nodes/:node_iri", get(read_node_handler))
            .route("/api/v1/projections", post(get_projection_handler))
            .with_state(state.clone());
        let missing = "iri://task/does-not-exist";
        let not_found = br#"{"error":"not found"}"#.to_vec();
        // Tenant A's DA, a plain tenant A user, and tenant B's DA in another
        // project all get the missing-node 404 for tenant B's node.
        for token in [
            jwt("tenant-a", "project-a", vec!["DA"]),
            jwt("tenant-a", "project-a", vec![]),
            jwt("tenant-b", "project-other", vec!["DA"]),
        ] {
            let (status, cross) =
                raw_response(&router, node_read_request(node_b, Some(&token))).await;
            let (missing_status, absent) =
                raw_response(&router, node_read_request(missing, Some(&token))).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(missing_status, StatusCode::NOT_FOUND);
            assert_eq!(cross, absent);
            assert_eq!(cross, not_found);

            let (status, cross) = raw_response(&router, projection_request(node_b, &token)).await;
            let (missing_status, absent) =
                raw_response(&router, projection_request(missing, &token)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(missing_status, StatusCode::NOT_FOUND);
            assert_eq!(cross, absent);
            assert_eq!(cross, not_found);
        }
        let (status, own) = raw_response(
            &router,
            node_read_request(node_b, Some(&jwt("tenant-b", "project-b", vec![]))),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&own).contains("tenant-b-plan"));
        assert_eq!(
            raw_response(&router, node_read_request(node_b, None))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );

        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
        match previous_jwt_secret {
            Some(value) => std::env::set_var("AGENTOS_JWT_SECRET", value),
            None => std::env::remove_var("AGENTOS_JWT_SECRET"),
        }
    }

    #[tokio::test]
    async fn blackboard_list_and_nodes_require_claims_and_are_scope_limited() {
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
        let tenant_a =
            crate::isolation::IsolationClaims::from_verified("tenant-a", "project-a", "actor-a")
                .unwrap();
        let tenant_b =
            crate::isolation::IsolationClaims::from_verified("tenant-b", "project-a", "actor-b")
                .unwrap();
        let tenant_a_task = state
            .core
            .init_task_with_claims("tenant a task", None, None, None, None, &tenant_a)
            .await
            .unwrap();
        let tenant_b_task = state
            .core
            .init_task_with_claims("tenant b task", None, None, None, None, &tenant_b)
            .await
            .unwrap();
        let legacy_task = state
            .core
            .init_task_with_tenant("legacy task", None, None, None, None, Some("tenant-a"))
            .await
            .unwrap();

        let router = Router::new()
            .route(
                "/api/v1/blackboard/tasks",
                get(list_blackboard_tasks_handler),
            )
            .route(
                "/api/v1/blackboard/nodes",
                get(list_blackboard_nodes_handler),
            )
            .with_state(state);

        let unauthenticated_uris = vec![
            "/api/v1/blackboard/tasks".to_string(),
            format!("/api/v1/blackboard/nodes?task_iri={tenant_a_task}"),
        ];
        for uri in unauthenticated_uris {
            let request = Request::builder()
                .method("GET")
                .uri(&uri)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                response_status(&router, request).await,
                StatusCode::UNAUTHORIZED
            );
        }

        let (status, tasks) = response_json(
            &router,
            Request::builder()
                .method("GET")
                .uri("/api/v1/blackboard/tasks")
                .header(
                    "authorization",
                    format!("Bearer {}", jwt("tenant-a", "project-a", vec![])),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(tasks["count"], 1);
        assert_eq!(tasks["tasks"][0]["task_iri"], tenant_a_task);
        assert!(!tasks.to_string().contains(&tenant_b_task));
        assert!(!tasks.to_string().contains(&legacy_task));

        let (status, nodes) = response_json(
            &router,
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/blackboard/nodes?task_iri={tenant_a_task}"))
                .header(
                    "authorization",
                    format!("Bearer {}", jwt("tenant-a", "project-a", vec![])),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(nodes["count"], 1);
        assert!(!nodes.to_string().contains(&tenant_b_task));

        for task_iri in [&tenant_b_task, &legacy_task] {
            let request = Request::builder()
                .method("GET")
                .uri(format!("/api/v1/blackboard/nodes?task_iri={task_iri}"))
                .header(
                    "authorization",
                    format!("Bearer {}", jwt("tenant-a", "project-a", vec![])),
                )
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                response_status(&router, request).await,
                StatusCode::FORBIDDEN
            );
        }

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
