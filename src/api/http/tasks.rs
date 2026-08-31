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

use super::{AppState, TaskExecSpec};

#[derive(Deserialize)]
pub struct TaskRequest {
    pub user_input: String,
    /// 用户态标识，用于会话隔离（可选，缺省为匿名）。
    pub user_id: Option<String>,
    /// 会话标识，用于多轮上下文隔离（可选）。
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
pub struct StreamTaskRequest {
    pub prompt: String,
    pub task_iri: Option<String>,
    pub include_thought: Option<bool>,
    pub include_tool_calls: Option<bool>,
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
    Json(req): Json<TaskRequest>,
) -> impl IntoResponse {
    match state
        .core
        .init_task(
            &req.user_input,
            None,
            None,
            req.user_id.as_deref(),
            req.session_id.as_deref(),
        )
        .await
    {
        Ok(task_iri) => (
            StatusCode::CREATED,
            Json(json!({"task_iri": task_iri, "status": "created"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

pub(crate) async fn get_task_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.core.read_node(&task_iri).await {
        Ok(Some(node)) => Json(json!({
            "task_iri": task_iri,
            "found": true,
            "node": node,
        })),
        Ok(None) => Json(json!({
            "task_iri": task_iri,
            "found": false,
        })),
        Err(e) => Json(json!({
            "task_iri": task_iri,
            "error": e.to_string(),
        })),
    }
}

pub(crate) async fn stream_task_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StreamTaskRequest>,
) -> impl IntoResponse {
    let task_iri = req
        .task_iri
        .unwrap_or_else(|| format!("iri://stream/{}", uuid::Uuid::new_v4().hyphenated()));

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
            };
            tokio::spawn(async move {
                executor.execute(spec).await;
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
            match rx.recv().await {
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
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub(crate) async fn get_realtime_status_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    // Read the task node from L2 blackboard; return 404 when not found.
    match state.core.blackboard.read_node(&task_iri) {
        Ok(Some(node)) => {
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
            Json(json!({ "error": "task not found", "task_iri": task_iri })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_execution_details_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    // Read the task node from L2 blackboard; return 404 when not found.
    match state.core.blackboard.read_node(&task_iri) {
        Ok(Some(node)) => {
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
            Json(json!({ "error": "task not found", "task_iri": task_iri })),
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
    Query(q): Query<TaskTrendsQuery>,
) -> impl IntoResponse {
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

    Json(json!({ "days": days, "trends": trends }))
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
