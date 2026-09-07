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

/// GET /api/v1/agents — 返回批处理 Agent（静态）与用户态 Agent（持久化）合并列表
pub(crate) async fn list_agents_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut agents: Vec<Value> = state
        .agents_info
        .get("agents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let batch_count = agents.len();
    let user_agents = state.user_agents.read().await.clone();
    let user_count = user_agents.len();
    agents.extend(user_agents);
    Json(json!({
        "count": agents.len(),
        "batch_count": batch_count,
        "user_count": user_count,
        "agents": agents,
    }))
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
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(patch): Json<Value>,
) -> impl IntoResponse {
    let mut guard = state.user_agents.write().await;
    let found = guard
        .iter_mut()
        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));
    match found {
        Some(agent) => {
            if let (Some(obj), Some(patch_obj)) = (agent.as_object_mut(), patch.as_object()) {
                for (k, v) in patch_obj {
                    if k == "id" || k == "source" || k == "created_at" {
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
            Json(json!({ "error": "agent not found", "id": id })),
        ),
    }
}

/// DELETE /api/v1/agents/:id — 删除用户态 Agent 并持久化
pub(crate) async fn delete_agent_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let mut guard = state.user_agents.write().await;
    let before = guard.len();
    guard.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    if guard.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found", "id": id })),
        );
    }
    let _ = save_user_agents(&guard);
    (
        StatusCode::OK,
        Json(json!({ "status": "deleted", "id": id })),
    )
}
