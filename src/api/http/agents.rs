//! 用户态 Agent CRUD 与持久化/图谱迁移。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；聊天/RAG 见 `chat.rs`。

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use super::iam::UserIdentity;
use super::{data_dir, AppState};

/// 用户态 Agent 的持久化文件路径。
fn agents_store_path() -> std::path::PathBuf {
    data_dir().join("agents.json")
}

/// 启动时从磁盘加载用户态 Agent；文件不存在或解析失败时返回空列表。
pub(crate) fn load_user_agents() -> Vec<Value> {
    match std::fs::read_to_string(agents_store_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 将用户态 Agent 持久化到磁盘（pretty JSON）。
pub(crate) fn save_user_agents(agents: &[Value]) -> std::io::Result<()> {
    let path = agents_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(agents).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

/// 从旧 knowledge_graph 值中解析知识库 uuid（形如 .../kb/{uuid}）。
fn extract_kb_uuid_from_graph(graph: &str) -> Option<String> {
    let idx = graph.rfind("/kb/")?;
    let candidate = graph[idx + 4..].split('/').next().unwrap_or_default();
    if candidate.len() == 36 && candidate.matches('-').count() == 4 {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// 一次性幂等迁移：将存量 agent.knowledge_graph（旧「绑定知识图谱」单值）迁入知识包体系。
/// 策略（对每个 knowledge_graph 非空的 agent）：
///
///   1) 能解析出 KB uuid 且已有知识包的 graph_kb_ids 覆盖它 → 确保该包挂载到 agent，清空旧字段；
///   2) 否则能解析出 KB uuid → 新建知识包 {graph_kb_ids:[uuid]}，挂载并清空；
///   3) 否则（原始命名图）→ 新建知识包 {named_graph: 原值}，挂载并清空。
///
/// 返回 (agents_changed, packs_changed)；清空后再次运行不再产生变更（幂等）。
pub(crate) fn migrate_legacy_agent_graphs(
    agents: &mut [Value],
    packs: &mut Vec<Value>,
) -> (bool, bool) {
    let mut agents_changed = false;
    let mut packs_changed = false;
    for agent in agents.iter_mut() {
        let kg = agent
            .get("knowledge_graph")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if kg.is_empty() {
            continue;
        }
        let agent_name = agent
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("agent")
            .to_string();
        let mut pack_ids: Vec<String> = agent
            .get("knowledge_pack_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let kb_uuid = extract_kb_uuid_from_graph(&kg);
        let covering_pack_id = kb_uuid.as_ref().and_then(|uuid| {
            packs
                .iter()
                .find(|p| {
                    p.get("graph_kb_ids")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().any(|x| x.as_str() == Some(uuid.as_str())))
                        .unwrap_or(false)
                })
                .and_then(|p| p.get("id").and_then(|v| v.as_str()).map(String::from))
        });

        let target_pack_id = match covering_pack_id {
            Some(pid) => pid,
            None => {
                let new_id = uuid::Uuid::new_v4().hyphenated().to_string();
                let mut pack = json!({
                    "id": new_id.clone(),
                    "name": format!("{}（图谱迁移）", agent_name),
                    "description": "由旧「绑定知识图谱」自动迁移生成",
                    "version": "1.0.0",
                    "icon": "Package",
                    "color": "amber",
                    "named_graph": "",
                    "vector_namespace": "",
                    "ontology_domain": "",
                    "stats": { "object_types": 0, "link_types": 0, "action_types": 0, "functions": 0 },
                    "category_ids": [],
                    "graph_kb_ids": [],
                    "vector_kb_ids": [],
                    "builtin": false,
                    "created_at": chrono::Utc::now().to_rfc3339(),
                });
                match &kb_uuid {
                    Some(uuid) => pack["graph_kb_ids"] = json!([uuid]),
                    None => pack["named_graph"] = json!(kg),
                }
                packs.push(pack);
                packs_changed = true;
                new_id
            }
        };

        if !pack_ids.contains(&target_pack_id) {
            pack_ids.push(target_pack_id.clone());
        }
        if let Some(obj) = agent.as_object_mut() {
            obj.insert("knowledge_pack_ids".into(), json!(pack_ids));
            obj.insert("knowledge_graph".into(), json!(""));
            obj.remove("knowledge_graph_description");
            obj.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
        }
        agents_changed = true;
        tracing::info!(
            "migrated legacy knowledge_graph for agent '{}' -> pack {}",
            agent_name,
            target_pack_id
        );
    }
    (agents_changed, packs_changed)
}

/// GET /api/v1/agents — 返回共享批处理目录与当前作用域的用户态 Agent。
pub(crate) async fn list_agents_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "verified isolation claims are required" })),
        )
            .into_response();
    };
    let mut agents: Vec<Value> = state
        .agents_info
        .get("agents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let batch_count = agents.len();
    let user_agents: Vec<Value> = state
        .user_agents
        .read()
        .await
        .iter()
        .filter(|agent| {
            agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
                && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
        })
        .cloned()
        .collect();
    let user_count = user_agents.len();
    agents.extend(user_agents);
    Json(json!({
        "count": agents.len(),
        "batch_count": batch_count,
        "user_count": user_count,
        "agents": agents,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct AgentCreateRequest {
    pub name: String,
    pub description: Option<String>,
    pub business_domain: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    /// 关联的知识包 id 列表（Agent → N 知识包）。
    #[serde(default)]
    pub knowledge_pack_ids: Vec<String>,
    pub enabled: Option<bool>,
    pub icon: Option<String>,
    pub color: Option<String>,
}

/// POST /api/v1/agents — 创建用户态 Agent 并持久化
pub(crate) async fn create_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<AgentCreateRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for agent creation" })),
            )
        }
    };
    let agent = json!({
        "id": uuid::Uuid::new_v4().hyphenated().to_string(),
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "business_domain": req.business_domain.unwrap_or_default(),
        "skills": req.skills,
        "knowledge_pack_ids": req.knowledge_pack_ids,
        "enabled": req.enabled.unwrap_or(true),
        "icon": req.icon.unwrap_or_else(|| "Bot".to_string()),
        "color": req.color.unwrap_or_else(|| "bg-blue-500".to_string()),
        "source": "user",
        "tenant_id": claims.tenant_id(),
        "project_id": claims.project_id(),
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    let id = agent["id"].as_str().unwrap_or("").to_string();
    let mut guard = state.user_agents.write().await;
    guard.push(agent.clone());
    let _ = save_user_agents(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": "created", "agent": agent })),
    )
}

/// PUT /api/v1/agents/:id — 更新用户态 Agent 并持久化
pub(crate) async fn update_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(patch): Json<Value>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "verified isolation claims are required" })),
        )
            .into_response();
    };
    let mut guard = state.user_agents.write().await;
    let found = guard.iter_mut().find(|agent| {
        agent.get("id").and_then(Value::as_str) == Some(id.as_str())
            && agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
            && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
    });
    match found {
        Some(agent) => {
            if let (Some(obj), Some(patch_obj)) = (agent.as_object_mut(), patch.as_object()) {
                for (k, v) in patch_obj {
                    if matches!(
                        k.as_str(),
                        "id" | "source" | "created_at" | "tenant_id" | "project_id"
                    ) {
                        continue;
                    }
                    obj.insert(k.clone(), v.clone());
                }
                obj.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
            }
            let updated = agent.clone();
            let _ = save_user_agents(&guard);
            (
                StatusCode::OK,
                Json(json!({ "status": "updated", "agent": updated })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found" })),
        ),
    }
    .into_response()
}

/// DELETE /api/v1/agents/:id — 删除用户态 Agent 并持久化
pub(crate) async fn delete_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "verified isolation claims are required" })),
        )
            .into_response();
    };
    let mut guard = state.user_agents.write().await;
    let before = guard.len();
    guard.retain(|agent| {
        !(agent.get("id").and_then(Value::as_str) == Some(id.as_str())
            && agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
            && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id()))
    });
    if guard.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found" })),
        )
            .into_response();
    }
    let _ = save_user_agents(&guard);
    (
        StatusCode::OK,
        Json(json!({ "status": "deleted", "id": id })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        routing::{get, put},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        api::http::{api_gov::ApiUsageState, iam::JwtClaims, AppState, TEST_ENV_LOCK},
        config::GatewaySettings,
        core::core_types::{CoreConfig, SemanticCore},
        gateway::unified_gateway::UnifiedGateway,
        tools::prompt_registry::PromptRegistry,
    };

    fn test_state(path: &std::path::Path) -> Arc<AppState> {
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                l0_storage_path: path.join("l0").display().to_string(),
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
                model_mapping: Default::default(),
            })
            .unwrap(),
        );
        Arc::new(AppState {
            core,
            gateway,
            kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
            config_info: Arc::new(tokio::sync::RwLock::new(json!({}))),
            agents_info: json!({"agents": [{"id": "platform-agent", "source": "platform"}]}),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![
                json!({"id": "agent-a", "name": "Agent A", "source": "user", "tenant_id": "tenant-a", "project_id": "project-a", "created_at": "2026-01-01T00:00:00Z"}),
                json!({"id": "agent-b", "name": "Agent B", "source": "user", "tenant_id": "tenant-b", "project_id": "project-a", "created_at": "2026-01-01T00:00:00Z"}),
                json!({"id": "legacy-agent", "name": "Legacy", "source": "user"}),
            ])),
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

    async fn request(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn agent_crud_requires_claims_and_never_exposes_other_scopes() {
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
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());

        let state = test_state(temp.path());
        let router = Router::new()
            .route("/api/v1/agents", get(list_agents_handler))
            .route(
                "/api/v1/agents/:id",
                put(update_agent_handler).delete(delete_agent_handler),
            )
            .with_state(state.clone());

        let (status, _) = request(
            &router,
            Request::builder()
                .method("GET")
                .uri("/api/v1/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let tenant_a_token = jwt("tenant-a", "project-a");
        let (status, listed) = request(
            &router,
            Request::builder()
                .method("GET")
                .uri("/api/v1/agents")
                .header("authorization", format!("Bearer {tenant_a_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["user_count"], 1);
        assert!(listed.to_string().contains("platform-agent"));
        assert!(listed.to_string().contains("agent-a"));
        assert!(!listed.to_string().contains("agent-b"));
        assert!(!listed.to_string().contains("legacy-agent"));

        for method in ["PUT", "DELETE"] {
            let mut builder = Request::builder()
                .method(method)
                .uri("/api/v1/agents/agent-b")
                .header("authorization", format!("Bearer {tenant_a_token}"));
            if method == "PUT" {
                builder = builder.header("content-type", "application/json");
            }
            let (status, body) = request(
                &router,
                builder
                    .body(if method == "PUT" {
                        Body::from(json!({"name": "attacker"}).to_string())
                    } else {
                        Body::empty()
                    })
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert!(!body.to_string().contains("agent-b"));
        }

        let (status, updated) = request(
            &router,
            Request::builder()
                .method("PUT")
                .uri("/api/v1/agents/agent-a")
                .header("authorization", format!("Bearer {tenant_a_token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"name": "Updated A", "tenant_id": "tenant-b", "project_id": "other", "id": "other"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["agent"]["name"], "Updated A");
        assert_eq!(updated["agent"]["tenant_id"], "tenant-a");
        assert_eq!(updated["agent"]["project_id"], "project-a");
        assert_eq!(updated["agent"]["id"], "agent-a");

        let (status, _) = request(
            &router,
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/agents/agent-a")
                .header("authorization", format!("Bearer {tenant_a_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        restore_env("AGENTOS_AUTH_MODE", previous_auth_mode);
        restore_env("AGENTOS_JWT_SECRET", previous_jwt_secret);
        restore_env("AGENTOS_DATA_DIR", previous_data_dir);
    }

    fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
        if let Some(value) = previous {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
}
