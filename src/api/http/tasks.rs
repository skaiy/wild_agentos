//! 任务创建 / SSE 流 / 实时状态 / 执行详情 / 趋势。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装。`TaskExecSpec` / `TaskExecutor` 留在 `mod.rs`。

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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{core_ops::task_is_in_scope, iam::UserIdentity};
use super::{AppState, TaskExecSpec};

#[derive(Deserialize)]
pub struct TaskRequest {
    pub user_input: String,
    /// 用户态标识，用于会话隔离（可选，缺省为匿名）。
    pub user_id: Option<String>,
    /// 会话标识，用于多轮上下文隔离（可选）。
    pub session_id: Option<String>,
    /// Ignored for scoping: the task scope comes only from verified claims.
    /// A value that differs from the claims is rejected with 403.
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Deserialize)]
pub struct StreamTaskRequest {
    pub prompt: String,
    pub task_iri: Option<String>,
    pub include_thought: Option<bool>,
    pub include_tool_calls: Option<bool>,
    /// Same rule as `TaskRequest`: must match the verified claims if present.
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

/// Require verified claims for task creation/execution and reject a body
/// scope that differs from them. Task scope is never taken from the body.
#[allow(clippy::result_large_err)]
fn authorize_task_write<'a>(
    identity: &'a UserIdentity,
    body_tenant: Option<&str>,
    body_project: Option<&str>,
) -> Result<&'a crate::isolation::IsolationClaims, axum::response::Response> {
    let Some(claims) = identity.isolation_claims() else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims are required"})),
        )
            .into_response());
    };
    let mismatch = body_tenant.is_some_and(|tenant| tenant != claims.tenant_id())
        || body_project.is_some_and(|project| project != claims.project_id());
    if mismatch {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "task scope must come from verified claims"})),
        )
            .into_response());
    }
    Ok(claims)
}

#[derive(Deserialize)]
pub struct RealtimeStatusRequest {
    pub task_iri: String,
}

#[derive(Serialize)]
pub struct StreamEventResponse {
    pub event_type: String,
    pub data: Value,
}

pub(crate) async fn create_task_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<TaskRequest>,
) -> impl IntoResponse {
    let claims = match authorize_task_write(
        &identity,
        req.tenant_id.as_deref(),
        req.project_id.as_deref(),
    ) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let result = state
        .core
        .init_task_with_claims(
            &req.user_input,
            None,
            None,
            req.user_id.as_deref(),
            req.session_id.as_deref(),
            claims,
        )
        .await;
    match result {
        Ok(task_iri) => (
            StatusCode::CREATED,
            Json(json!({"task_iri": task_iri, "status": "created"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/v1/tasks — list only tasks carrying the caller's verified scope.
///
/// Task records without both scope fields are legacy/unverified and deliberately
/// excluded. This endpoint must never fall back to the platform task index.
pub(crate) async fn list_tasks_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims are required"})),
        )
            .into_response();
    };

    let mut tasks: Vec<Value> = state
        .core
        .blackboard
        .list_task_summaries()
        .into_iter()
        .filter_map(|summary| {
            let node = state.core.blackboard.read_node(&summary.task_iri).ok()??;
            let task: Value = serde_json::from_str(&node.json_ld).ok()?;
            let is_task = task.get("@type").is_some_and(|kind| {
                kind.as_str() == Some("Task")
                    || kind.as_array().is_some_and(|kinds| {
                        kinds.iter().any(|value| value.as_str() == Some("Task"))
                    })
            });
            let in_scope = task.get("tenant_id").and_then(Value::as_str)
                == Some(claims.tenant_id())
                && task.get("project_id").and_then(Value::as_str) == Some(claims.project_id());
            if !is_task || !in_scope {
                return None;
            }

            let created_at = task
                .get("created_at")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| node.created_at.to_rfc3339());
            Some(json!({
                "id": summary.task_iri,
                "iri": summary.task_iri,
                "task_iri": summary.task_iri,
                "status": task.get("status").and_then(Value::as_str).unwrap_or(&summary.status),
                "created_at": created_at,
                "interrupt_status": Value::Null,
                "approval_status": Value::Null,
            }))
        })
        .collect();
    tasks.sort_by(|left, right| {
        right["created_at"]
            .as_str()
            .cmp(&left["created_at"].as_str())
    });

    Json(json!({ "count": tasks.len(), "tasks": tasks })).into_response()
}

pub(crate) async fn get_task_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(task_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims are required"})),
        )
            .into_response();
    };
    match state.core.read_node(&task_iri).await {
        Ok(Some(node)) if task_is_in_scope(&node.json_ld, claims) => Json(json!({
            "task_iri": task_iri,
            "found": true,
            "node": node,
        }))
        .into_response(),
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%task_iri, "failed to read task scope: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to verify task scope"})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn stream_task_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<StreamTaskRequest>,
) -> axum::response::Response {
    // Authenticate and scope-check before subscribing or spawning anything,
    // so a rejected caller never gets an SSE stream or an execution.
    let claims = match authorize_task_write(
        &identity,
        req.tenant_id.as_deref(),
        req.project_id.as_deref(),
    ) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    if let Some(task_iri) = req.task_iri.as_deref() {
        // An existing task outside the caller's scope answers like a missing
        // one (no existence oracle); a supplied task_iri must be in scope.
        // A platform admin may stream any existing task.
        let platform_admin = identity.require_platform_admin("task streams").is_ok();
        let in_scope = matches!(
            state.core.read_node(task_iri).await,
            Ok(Some(node)) if platform_admin || task_is_in_scope(&node.json_ld, claims)
        );
        if !in_scope {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
    }
    // Without a supplied task_iri the server creates the task itself, so its
    // persisted scope is the caller's verified claims.
    let task_iri = match req.task_iri.clone() {
        Some(task_iri) => task_iri,
        None => match state
            .core
            .init_task_with_claims(&req.prompt, None, None, None, None, claims)
            .await
        {
            Ok(task_iri) => task_iri,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error.to_string()})),
                )
                    .into_response()
            }
        },
    };

    let event_bus = state.core.events.clone();
    let task_iri_clone = task_iri.clone();
    // 订阅必须早于触发执行，避免执行器早期推送的事件在订阅前丢失。
    let mut rx = event_bus.subscribe();

    // 触发实际执行：经注入的 TaskExecutor 在后台驱动 SA PDCA 管线，
    // 执行事件会发布到同一条共享事件总线，由下方 SSE 循环转发给前端。
    match state.task_executor.clone() {
        Some(executor) => {
            let spec = TaskExecSpec {
                prompt: req.prompt.clone(),
                task_iri: task_iri.clone(),
                include_thought: req.include_thought.unwrap_or(true),
                include_tool_calls: req.include_tool_calls.unwrap_or(true),
                cancellation: tokio_util::sync::CancellationToken::new(),
                isolation_claims: claims.clone(),
            };
            let task_iri = spec.task_iri.clone();
            let cancellation = spec.cancellation.clone();
            let task_events = event_bus.clone();
            let shutdown = state.shutdown.clone();
            tokio::spawn(async move {
                let execution = tokio::spawn(async move {
                    executor.execute(spec).await;
                });

                tokio::select! {
                    _ = shutdown.cancelled() => {
                        cancellation.cancel();
                        tracing::info!(task_iri = %task_iri, "HTTP task cancelled during shutdown");
                    }
                    result = execution => {
                        if let Err(error) = result {
                            cancellation.cancel();
                            tracing::error!(
                                task_iri = %task_iri,
                                cancelled = error.is_cancelled(),
                                panic = error.is_panic(),
                                error = %error,
                                "HTTP task executor terminated unexpectedly"
                            );
                            task_events
                                .emit(
                                    &task_iri,
                                    "TASK_FAILED",
                                    "http",
                                    &json!({
                                        "status": "failed",
                                        "summary": format!("task executor terminated unexpectedly: {error}"),
                                    })
                                    .to_string(),
                                )
                                .await;
                        }
                    }
                }
            });
        }
        None => {
            // 未注入执行器（仅测试态）：即时推送失败事件，避免前端卡在「启动中」。
            let bus = event_bus.clone();
            let ti = task_iri.clone();
            tokio::spawn(async move {
                bus.emit(
                    &ti,
                    "TASK_FAILED",
                    "http",
                    &json!({"status": "failed", "summary": "task executor not configured"})
                        .to_string(),
                )
                .await;
            });
        }
    }

    let stream = async_stream::stream! {
        yield Ok::<axum::response::sse::Event, std::convert::Infallible>(Event::default().event("task_started").data(json!({
            "task_iri": task_iri_clone,
            "status": "started"
        }).to_string()));

        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => {
                    tracing::info!(task_iri = %task_iri_clone, "SSE stream closed during shutdown");
                    break;
                }
                result = rx.recv() => match result {
                Ok(event) => {
                    if event.task_iri != task_iri_clone {
                        continue;
                    }

                    if let Some(sse_event) = convert_event_to_sse(&event) {
                        yield Ok(sse_event);
                    }

                    if event.event_type == "TASK_COMPLETED" || event.event_type == "TASK_FAILED" {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    continue;
                }
                },
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub(crate) async fn get_realtime_status_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(task_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims are required"})),
        )
            .into_response();
    };
    // Read the task node from L2 blackboard; return 404 when not found.
    match state.core.blackboard.read_node(&task_iri) {
        Ok(Some(node)) if task_is_in_scope(&node.json_ld, claims) => {
            // Parse json_ld to extract runtime status fields if present.
            let parsed: Value = serde_json::from_str(&node.json_ld).unwrap_or(Value::Null);
            let status = parsed
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("queued");
            let phase = parsed
                .get("current_phase")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let completed_steps = parsed
                .pointer("/progress/completed_steps")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let total_steps = parsed
                .pointer("/progress/total_steps")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let percentage = parsed
                .pointer("/progress/percentage")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| {
                    completed_steps
                        .saturating_mul(100)
                        .checked_div(total_steps)
                        .unwrap_or(0)
                });
            (
                axum::http::StatusCode::OK,
                Json(json!({
                    "task_iri": task_iri,
                    "status": status,
                    "current_phase": phase,
                    "node_type": node.node_type,
                    "tags": node.tags,
                    "created_at": node.created_at,
                    "dirty": node.dirty,
                    "current_agent": {
                        "id": parsed.pointer("/current_agent/id").and_then(|v| v.as_str()).unwrap_or(""),
                        "role": parsed.pointer("/current_agent/role").and_then(|v| v.as_str()).unwrap_or(""),
                        "status": parsed.pointer("/current_agent/status").and_then(|v| v.as_str()).unwrap_or(status),
                        "turn": parsed.pointer("/current_agent/turn").and_then(|v| v.as_u64()).unwrap_or(0),
                    },
                    "progress": {
                        "completed_steps": completed_steps,
                        "total_steps": total_steps,
                        "percentage": percentage.min(100),
                    },
                })),
            ).into_response()
        }
        _ => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "error": "task not found" })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_execution_details_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(task_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims are required"})),
        )
            .into_response();
    };
    // Read the task node from L2 blackboard; return 404 when not found.
    match state.core.blackboard.read_node(&task_iri) {
        Ok(Some(node)) if task_is_in_scope(&node.json_ld, claims) => {
            let parsed: Value = serde_json::from_str(&node.json_ld).unwrap_or(Value::Null);
            let status = parsed
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("queued");
            let phase = parsed
                .get("current_phase")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let turn = parsed.get("turn").and_then(|v| v.as_i64()).unwrap_or(0);
            // Collect child nodes for this task.
            let child_nodes = state.core.blackboard.get_task_nodes(&task_iri);
            (
                axum::http::StatusCode::OK,
                Json(json!({
                    "task_iri": task_iri,
                    "status": status,
                    "current_phase": phase,
                    "node_type": node.node_type,
                    "tags": node.tags,
                    "created_at": node.created_at,
                    "child_nodes": child_nodes,
                    "plan": parsed.get("plan").cloned().unwrap_or_else(|| json!({
                        "plan_id": "",
                        "description": "",
                        "steps": [],
                    })),
                    "steps": [],
                    "agent_sessions": [],
                    "stats": {
                        "total_turns": turn,
                        "total_tool_calls": 0,
                        "total_tokens": 0,
                    },
                })),
            )
                .into_response()
        }
        _ => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "error": "task not found" })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct TaskTrendsQuery {
    days: Option<i64>,
}

/// GET /api/v1/tasks/trends?days=N — 任务执行时序趋势（真实持久化数据）。
/// 扫描 L0Store 中 `iri://checkpoint/` 前缀的持久化检查点（跨进程/PVC 存活），按天聚合：
/// 活跃任务数（去重 task_iri）/ 检查点数（执行步）/ 完成阶段数（finish_/step_complete_）。
/// 预置最近 N 天的空桶以保证图表时间轴连续（默认 7 天，范围 1..=90）。
pub(crate) async fn list_task_trends_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Query(q): Query<TaskTrendsQuery>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified isolation claims are required"})),
        )
            .into_response();
    };
    let days = q.days.unwrap_or(7).clamp(1, 90);
    let today = chrono::Utc::now().date_naive();
    let start = today - chrono::Duration::days(days - 1);

    // 每桶：(去重任务集合, 检查点计数, 完成阶段计数)
    let mut buckets: std::collections::BTreeMap<
        chrono::NaiveDate,
        (std::collections::HashSet<String>, u64, u64),
    > = std::collections::BTreeMap::new();
    for i in 0..days {
        buckets.insert(
            start + chrono::Duration::days(i),
            (std::collections::HashSet::new(), 0, 0),
        );
    }

    if let Ok(entries) = state
        .core
        .l0_store
        .scan_iri_prefix("iri://checkpoint/", 5000)
    {
        for e in entries {
            let cp: crate::core::checkpoint::CheckpointData = match serde_json::from_str(&e.content)
            {
                Ok(v) => v,
                Err(_) => continue,
            };
            let in_scope = state
                .core
                .blackboard
                .read_node(&cp.task_iri)
                .ok()
                .flatten()
                .is_some_and(|task| task_is_in_scope(&task.json_ld, claims));
            if !in_scope {
                continue;
            }
            let d = cp.created_at.date_naive();
            if let Some(bucket) = buckets.get_mut(&d) {
                bucket.0.insert(cp.task_iri.clone());
                bucket.1 += 1;
                let phase = crate::core::checkpoint::parse_checkpoint_phase(&cp.name);
                if phase.starts_with("finish_") || phase.starts_with("step_complete_") {
                    bucket.2 += 1;
                }
            }
        }
    }

    let trends: Vec<Value> = buckets
        .into_iter()
        .map(|(date, (tasks, checkpoints, completed))| {
            json!({
                "date": date.format("%Y-%m-%d").to_string(),
                "tasks": tasks.len(),
                "checkpoints": checkpoints,
                "completed": completed,
            })
        })
        .collect();

    Json(json!({ "days": days, "trends": trends })).into_response()
}

/// 从序列化后的 ExecutionEvent payload 中取出内层某一 kind 的字段对象。
fn exec_event_inner(payload: &str, kind: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(payload).ok()?;
    v.get("event")?.get(kind).cloned()
}

fn convert_event_to_sse(event: &crate::core::event_bus::Event) -> Option<Event> {
    use crate::core::event_bus::EventType;

    // 富执行事件（由 AgentRunner 内联发布到总线，payload 为序列化后的 ExecutionEvent）：
    // 解析内层字段，映射为任务控制台可直接消费的干净 SSE 事件（思考/工具调用/逐字输出）。
    match event.event_type.as_str() {
        "THOUGHT" => {
            let inner = exec_event_inner(&event.payload, "Thought")?;
            return Some(
                Event::default().event("thought").data(
                    json!({
                        "agent_id": inner.get("agent_id"),
                        "thought": inner.get("thought"),
                        "action": inner.get("action"),
                        "emphasis": inner.get("emphasis"),
                    })
                    .to_string(),
                ),
            );
        }
        "TOOL_CALL" => {
            let inner = exec_event_inner(&event.payload, "ToolCall")?;
            let args_raw = inner
                .get("arguments_json")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = serde_json::from_str::<Value>(args_raw)
                .unwrap_or_else(|_| Value::String(args_raw.to_string()));
            return Some(
                Event::default().event("tool_call").data(
                    json!({
                        "call_id": inner.get("call_id"),
                        "tool_name": inner.get("tool_name"),
                        "arguments": arguments,
                        "agent_id": inner.get("agent_id"),
                        "sequence": inner.get("sequence"),
                    })
                    .to_string(),
                ),
            );
        }
        "TOOL_RESULT" => {
            let inner = exec_event_inner(&event.payload, "ToolResult")?;
            return Some(
                Event::default().event("tool_result").data(
                    json!({
                        "call_id": inner.get("call_id"),
                        "tool_name": inner.get("tool_name"),
                        "result": inner.get("result"),
                        "success": inner.get("success"),
                        "agent_id": inner.get("agent_id"),
                    })
                    .to_string(),
                ),
            );
        }
        "LLM_CONTENT" => {
            let inner = exec_event_inner(&event.payload, "LlmContent")?;
            return Some(
                Event::default().event("llm_content").data(
                    json!({
                        "agent_id": inner.get("agent_id"),
                        "role": inner.get("role"),
                        "delta": inner.get("content_delta"),
                        "is_reasoning": inner.get("is_reasoning"),
                    })
                    .to_string(),
                ),
            );
        }
        "PHASE_CHANGE" => {
            let inner = exec_event_inner(&event.payload, "PhaseChange")?;
            return Some(
                Event::default().event("phase_change").data(
                    json!({
                        "from_phase": inner.get("from_phase"),
                        "to_phase": inner.get("to_phase"),
                        "agent_role": inner.get("agent_role"),
                        "reason": inner.get("reason"),
                    })
                    .to_string(),
                ),
            );
        }
        "AGENT_STATUS" => {
            let inner = exec_event_inner(&event.payload, "AgentStatus")?;
            return Some(
                Event::default().event("agent_status").data(
                    json!({
                        "agent_id": inner.get("agent_id"),
                        "role": inner.get("role"),
                        "status": inner.get("status"),
                        "turn": inner.get("turn"),
                        "iteration": inner.get("iteration"),
                    })
                    .to_string(),
                ),
            );
        }
        "EXECUTION_ERROR" => {
            let inner = exec_event_inner(&event.payload, "Error")?;
            return Some(
                Event::default().event("error").data(
                    json!({
                        "error_type": inner.get("error_type"),
                        "message": inner.get("message"),
                        "agent_id": inner.get("agent_id"),
                    })
                    .to_string(),
                ),
            );
        }
        // SA 逐阶段派发事件（Debug 角色名，如 "Plan_STARTED"）→ 相位指示。
        "Plan_STARTED" | "Do_STARTED" | "Check_STARTED" | "Act_STARTED" => {
            let (to_phase, role) = match event.event_type.as_str() {
                "Plan_STARTED" => ("plan", "PA"),
                "Do_STARTED" => ("do", "DA"),
                "Check_STARTED" => ("check", "CA"),
                _ => ("act", "AA"),
            };
            return Some(
                Event::default().event("phase_change").data(
                    json!({
                        "to_phase": to_phase,
                        "agent_role": role,
                    })
                    .to_string(),
                ),
            );
        }
        _ => {}
    }

    let event_type = EventType::from_str(&event.event_type);
    let (event_name, data) = match event_type {
        EventType::PlanStarted => (
            "phase_change",
            json!({
                "from_phase": "idle",
                "to_phase": "plan",
                "agent_role": "PA"
            }),
        ),
        EventType::PlanCompleted => (
            "phase_change",
            json!({
                "from_phase": "plan",
                "to_phase": "do",
                "agent_role": "PA"
            }),
        ),
        EventType::DoStarted => (
            "phase_change",
            json!({
                "from_phase": "plan",
                "to_phase": "do",
                "agent_role": "DA"
            }),
        ),
        EventType::DoCompleted => (
            "phase_change",
            json!({
                "from_phase": "do",
                "to_phase": "check",
                "agent_role": "DA"
            }),
        ),
        EventType::CheckStarted => (
            "phase_change",
            json!({
                "from_phase": "do",
                "to_phase": "check",
                "agent_role": "CA"
            }),
        ),
        EventType::CheckCompleted => (
            "phase_change",
            json!({
                "from_phase": "check",
                "to_phase": "act",
                "agent_role": "CA"
            }),
        ),
        EventType::ActStarted => (
            "phase_change",
            json!({
                "from_phase": "check",
                "to_phase": "act",
                "agent_role": "AA"
            }),
        ),
        EventType::ActCompleted => (
            "phase_change",
            json!({
                "from_phase": "act",
                "to_phase": "completed",
                "agent_role": "AA"
            }),
        ),
        EventType::AgentStarted => (
            "agent_status",
            json!({
                "agent_id": event.source_agent_iri,
                "status": "running"
            }),
        ),
        EventType::AgentCompleted => (
            "agent_status",
            json!({
                "agent_id": event.source_agent_iri,
                "status": "completed"
            }),
        ),
        EventType::AgentError => (
            "error",
            json!({
                "agent_id": event.source_agent_iri,
                "message": event.payload
            }),
        ),
        EventType::TaskCompleted => (
            "completion",
            json!({
                "status": "success",
                "summary": event.payload
            }),
        ),
        EventType::TaskFailed => (
            "completion",
            json!({
                "status": "failed",
                "summary": event.payload
            }),
        ),
        _ => return None,
    };

    Some(Event::default().event(event_name).data(data.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        routing::get,
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        api::http::{api_gov::ApiUsageState, iam::JwtClaims, AppState, TEST_ENV_LOCK},
        config::GatewaySettings,
        core::core_types::{CoreConfig, SemanticCore},
        gateway::unified_gateway::UnifiedGateway,
        isolation::IsolationClaims,
        tools::prompt_registry::PromptRegistry,
    };

    fn test_state() -> Arc<AppState> {
        let dir = tempfile::tempdir().unwrap();
        let l0_dir = dir.keep();
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                l0_storage_path: l0_dir.to_string_lossy().into_owned(),
                enable_metrics: false,
                ..CoreConfig::default()
            })
            .unwrap(),
        );
        let gateway = Arc::new(
            UnifiedGateway::new(&GatewaySettings {
                base_url: "http://localhost".into(),
                api_key: String::new(),
                default_model: "test-model".into(),
                timeout_seconds: 30,
                max_retries: 1,
                retry_base_ms: 500,
                use_responses_api: false,
                model_mapping: std::collections::HashMap::new(),
            })
            .unwrap(),
        );
        Arc::new(AppState {
            core,
            gateway,
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

    fn jwt(tenant_id: &str, project_id: &str) -> String {
        encode(
            &Header::default(),
            &JwtClaims {
                sub: "test-user".into(),
                tenant_id: tenant_id.into(),
                project_id: Some(project_id.into()),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap()
    }

    async fn get_tasks(router: &Router, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().method("GET").uri("/api/v1/tasks");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn jwt_with_roles(tenant_id: &str, project_id: &str, roles: Vec<&str>) -> String {
        encode(
            &Header::default(),
            &JwtClaims {
                sub: format!("{tenant_id}-user"),
                tenant_id: tenant_id.into(),
                project_id: Some(project_id.into()),
                roles: roles.into_iter().map(str::to_owned).collect(),
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap()
    }

    async fn post_json(
        router: &Router,
        uri: &str,
        token: Option<&str>,
        body: Value,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    /// Fake executor that only counts invocations (stands in for the LLM run).
    struct CountingExecutor(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl crate::api::http::TaskExecutor for CountingExecutor {
        async fn execute(&self, _spec: crate::api::http::TaskExecSpec) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn counting_state() -> (Arc<AppState>, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut state = Arc::try_unwrap(test_state()).ok().expect("unique state");
        state.task_executor = Some(Arc::new(CountingExecutor(calls.clone())));
        (Arc::new(state), calls)
    }

    fn executor_calls(calls: &std::sync::atomic::AtomicUsize) -> usize {
        calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn task_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/api/v1/tasks", axum::routing::post(create_task_handler))
            .route(
                "/api/v1/tasks/stream",
                axum::routing::post(stream_task_handler),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn isolation_contract_task_create_requires_claims_and_creates_nothing() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = test_state();
        let router = task_router(state.clone());
        let before = state.core.blackboard.list_task_summaries().len();
        let response = post_json(
            &router,
            "/api/v1/tasks",
            None,
            json!({"user_input": "anonymous task"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(state.core.blackboard.list_task_summaries().len(), before);
    }

    #[tokio::test]
    async fn isolation_contract_task_create_rejects_body_scope_mismatch() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = test_state();
        let router = task_router(state.clone());
        let da_b = jwt_with_roles("tenant-b", "project-b", vec!["DA"]);
        let before = state.core.blackboard.list_task_summaries().len();
        for body in [
            json!({"user_input": "x", "tenant_id": "tenant-a"}),
            json!({"user_input": "x", "project_id": "project-a"}),
            json!({"user_input": "x", "tenant_id": "tenant-a", "project_id": "project-a"}),
        ] {
            let response = post_json(&router, "/api/v1/tasks", Some(&da_b), body).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(state.core.blackboard.list_task_summaries().len(), before);

        // Matching (or absent) body scope is accepted; the task lands in the
        // caller's verified scope.
        let response = post_json(
            &router,
            "/api/v1/tasks",
            Some(&da_b),
            json!({"user_input": "x", "tenant_id": "tenant-b", "project_id": "project-b"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let task_iri = serde_json::from_slice::<Value>(&body).unwrap()["task_iri"]
            .as_str()
            .unwrap()
            .to_string();
        let node = state.core.blackboard.read_node(&task_iri).unwrap().unwrap();
        let task: Value = serde_json::from_str(&node.json_ld).unwrap();
        assert_eq!(task["tenant_id"], "tenant-b");
        assert_eq!(task["project_id"], "project-b");
    }

    #[tokio::test]
    async fn isolation_contract_task_stream_rejects_before_stream() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (state, calls) = counting_state();
        let tasks_before = state.core.blackboard.list_task_summaries().len();
        let router = task_router(state.clone());
        let subscribers = state.core.events.subscriber_count();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            post_json(
                &router,
                "/api/v1/tasks/stream",
                None,
                json!({"prompt": "go"}),
            ),
        )
        .await
        .expect("anonymous stream must not hang");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(!content_type.contains("text/event-stream"));
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("data:"));
        assert_eq!(state.core.events.subscriber_count(), subscribers);
        assert_eq!(executor_calls(&calls), 0);
        assert_eq!(
            state.core.blackboard.list_task_summaries().len(),
            tasks_before
        );

        // A verified caller cannot stream into another scope's task, and a
        // body scope that differs from the claims is rejected.
        let user_a = jwt("tenant-a", "project-a");
        let response = post_json(
            &router,
            "/api/v1/tasks",
            Some(&user_a),
            json!({"user_input": "tenant a task"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let task_a = serde_json::from_slice::<Value>(&body).unwrap()["task_iri"]
            .as_str()
            .unwrap()
            .to_string();
        let da_b = jwt_with_roles("tenant-b", "project-b", vec!["DA"]);
        let cross = post_json(
            &router,
            "/api/v1/tasks/stream",
            Some(&da_b),
            json!({"prompt": "go", "task_iri": task_a}),
        )
        .await;
        assert_eq!(cross.status(), StatusCode::NOT_FOUND);
        let cross = to_bytes(cross.into_body(), 1024 * 1024).await.unwrap();
        let missing = post_json(
            &router,
            "/api/v1/tasks/stream",
            Some(&da_b),
            json!({"prompt": "go", "task_iri": "iri://task/missing"}),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let missing = to_bytes(missing.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(cross, missing);
        let mismatch = post_json(
            &router,
            "/api/v1/tasks/stream",
            Some(&da_b),
            json!({"prompt": "go", "tenant_id": "tenant-a"}),
        )
        .await;
        assert_eq!(mismatch.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.core.events.subscriber_count(), subscribers);
        assert_eq!(
            executor_calls(&calls),
            0,
            "rejected streams must not execute"
        );

        // The owner can stream its own task, and it executes once.
        let own = post_json(
            &router,
            "/api/v1/tasks/stream",
            Some(&user_a),
            json!({"prompt": "go", "task_iri": task_a}),
        )
        .await;
        assert_eq!(own.status(), StatusCode::OK);
        let content_type = own
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(content_type.contains("text/event-stream"));
        for _ in 0..50 {
            if executor_calls(&calls) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(executor_calls(&calls), 1);
        drop(own);

        // Without a task_iri the server creates a task owned by the caller.
        let fresh = post_json(
            &router,
            "/api/v1/tasks/stream",
            Some(&da_b),
            json!({"prompt": "new"}),
        )
        .await;
        assert_eq!(fresh.status(), StatusCode::OK);
        drop(fresh);
        let owned_by_b = state
            .core
            .blackboard
            .list_task_summaries()
            .into_iter()
            .filter_map(|summary| state.core.blackboard.read_node(&summary.task_iri).ok()?)
            .filter_map(|node| serde_json::from_str::<Value>(&node.json_ld).ok())
            .filter(|task| task["tenant_id"] == "tenant-b" && task["project_id"] == "project-b")
            .count();
        assert_eq!(owned_by_b, 1);
    }

    #[tokio::test]
    async fn task_list_requires_claims_and_never_exposes_other_scopes() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let saved_secret = std::env::var_os("AGENTOS_JWT_SECRET");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::set_var(
            "AGENTOS_JWT_SECRET",
            "test-hs256-secret-at-least-32-bytes-long",
        );

        let state = test_state();
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project-a", "actor-b").unwrap();
        let tenant_a_task = state
            .core
            .init_task_with_claims("task for tenant a", None, None, None, None, &tenant_a)
            .await
            .unwrap();
        let tenant_b_task = state
            .core
            .init_task_with_claims("task for tenant b", None, None, None, None, &tenant_b)
            .await
            .unwrap();
        // Legacy records without a full verified scope must not be enumerable.
        state
            .core
            .init_task_with_tenant("legacy task", None, None, None, None, Some("tenant-a"))
            .await
            .unwrap();

        let router = Router::new()
            .route("/api/v1/tasks", get(list_tasks_handler))
            .with_state(state);

        let (status, _) = get_tasks(&router, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, response) = get_tasks(&router, Some(&jwt("tenant-a", "project-a"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["count"], 1);
        let tasks = response["tasks"].as_array().unwrap();
        assert_eq!(tasks[0]["id"], tenant_a_task);
        assert_eq!(tasks[0]["iri"], tenant_a_task);
        assert_ne!(tasks[0]["iri"], tenant_b_task);
        assert!(tasks[0]["created_at"].is_string());
        assert!(tasks[0]["interrupt_status"].is_null());
        assert!(tasks[0]["approval_status"].is_null());
        assert!(
            !response.to_string().contains(&tenant_b_task),
            "cross-tenant task IRI leaked in response"
        );

        restore_env("AGENTOS_AUTH_MODE", saved_mode);
        restore_env("AGENTOS_JWT_SECRET", saved_secret);
    }

    #[tokio::test]
    async fn task_detail_and_trends_require_claims_and_never_expose_other_scopes() {
        let _guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let saved_secret = std::env::var_os("AGENTOS_JWT_SECRET");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::set_var(
            "AGENTOS_JWT_SECRET",
            "test-hs256-secret-at-least-32-bytes-long",
        );

        let state = test_state();
        let tenant_a_task = "task-a".to_string();
        let tenant_b_task = "task-b".to_string();
        for (task_iri, tenant_id) in [
            (tenant_a_task.as_str(), "tenant-a"),
            (tenant_b_task.as_str(), "tenant-b"),
        ] {
            state
                .core
                .blackboard
                .write_node(
                    task_iri,
                    &json!({
                        "@id": task_iri,
                        "@type": "Task",
                        "tenant_id": tenant_id,
                        "project_id": "project-a",
                        "status": "queued"
                    })
                    .to_string(),
                    &state.core.config,
                )
                .unwrap();
        }
        let checkpoint = |task_iri: &str| {
            json!({
                "checkpoint_iri": format!("iri://checkpoint/test/{}", uuid::Uuid::new_v4()),
                "task_iri": task_iri,
                "name": "step_complete_test",
                "node_count": 0,
                "total_size_bytes": 0,
                "created_at": chrono::Utc::now().to_rfc3339(),
                "tags": [],
                "nodes_json": "[]",
                "session_messages_json": "[]",
                "agent_state_json": "{}"
            })
            .to_string()
        };
        state
            .core
            .l0_store
            .store("iri://checkpoint/test/a", &checkpoint(&tenant_a_task))
            .unwrap();
        state
            .core
            .l0_store
            .store("iri://checkpoint/test/b", &checkpoint(&tenant_b_task))
            .unwrap();

        let router = Router::new()
            .route("/api/v1/tasks/trends", get(list_task_trends_handler))
            .route("/api/v1/tasks/:task_iri", get(get_task_handler))
            .route(
                "/api/v1/tasks/:task_iri/status",
                get(get_realtime_status_handler),
            )
            .route(
                "/api/v1/tasks/:task_iri/details",
                get(get_execution_details_handler),
            )
            .with_state(state);
        let tenant_a_token = jwt("tenant-a", "project-a");
        let request = |uri: String, token: Option<&str>| {
            let mut request = Request::builder().method("GET").uri(uri);
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            request.body(Body::empty()).unwrap()
        };

        for uri in [
            format!("/api/v1/tasks/{tenant_a_task}"),
            format!("/api/v1/tasks/{tenant_a_task}/status"),
            format!("/api/v1/tasks/{tenant_a_task}/details"),
            "/api/v1/tasks/trends".to_string(),
        ] {
            assert_eq!(
                router
                    .clone()
                    .oneshot(request(uri, None))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }

        for suffix in ["", "/status", "/details"] {
            let response = router
                .clone()
                .oneshot(request(
                    format!("/api/v1/tasks/{tenant_b_task}{suffix}"),
                    Some(&tenant_a_token),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(!String::from_utf8_lossy(&body).contains(&tenant_b_task));
        }

        let response = router
            .clone()
            .oneshot(request(
                format!("/api/v1/tasks/{tenant_a_task}/details"),
                Some(&tenant_a_token),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = router
            .oneshot(request(
                "/api/v1/tasks/trends".to_string(),
                Some(&tenant_a_token),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(&tenant_b_task));

        restore_env("AGENTOS_AUTH_MODE", saved_mode);
        restore_env("AGENTOS_JWT_SECRET", saved_secret);
    }

    fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
        if let Some(value) = previous {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
}
