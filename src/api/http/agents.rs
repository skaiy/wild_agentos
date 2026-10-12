//! 用户态 Agent CRUD 与持久化/图谱迁移。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；聊天/RAG 见 `chat.rs`。

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::iam::UserIdentity;
use super::{data_dir, AppState};

const MAX_AGENT_ID_BYTES: usize = 64;

/// An immutable, executable snapshot of an agent definition.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct AgentDefinitionRevision {
    pub revision: String,
    pub system_prompt: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentDefinitionLookupError {
    Missing,
    OutOfScope,
}

/// Resolve one immutable definition snapshot. The caller maps `OutOfScope` to
/// 404 so an id in another tenant or project is never observable.
pub(crate) fn resolve_definition_revision(
    agents: &[Value],
    tenant_id: &str,
    project_id: &str,
    agent_id: &str,
    requested_revision: Option<&str>,
) -> Result<AgentDefinitionRevision, AgentDefinitionLookupError> {
    let Some(agent) = agents
        .iter()
        .find(|agent| agent.get("id").and_then(Value::as_str) == Some(agent_id))
    else {
        return Err(AgentDefinitionLookupError::Missing);
    };
    if agent.get("tenant_id").and_then(Value::as_str) != Some(tenant_id)
        || agent.get("project_id").and_then(Value::as_str) != Some(project_id)
    {
        return Err(AgentDefinitionLookupError::OutOfScope);
    }
    let revisions = agent
        .get("revisions")
        .and_then(Value::as_array)
        .ok_or(AgentDefinitionLookupError::Missing)?;
    let revision = requested_revision
        .map(str::to_owned)
        .or_else(|| {
            agent
                .get("active_revision")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or(AgentDefinitionLookupError::Missing)?;
    revisions
        .iter()
        .find(|candidate| candidate.get("revision").and_then(Value::as_str) == Some(&revision))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or(AgentDefinitionLookupError::Missing)
}

fn valid_agent_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_AGENT_ID_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn definition_request_error(message: &str) -> impl IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "invalid_agent_definition", "message": message })),
    )
}

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

/// 将用户态 Agent 持久化到磁盘（pretty JSON），原子替换。
pub(crate) fn save_user_agents(agents: &[Value]) -> std::io::Result<()> {
    let content = serde_json::to_vec_pretty(agents).map_err(std::io::Error::other)?;
    write_file_atomically(&agents_store_path(), &content)
}

/// Replaces `path` with `bytes` so a reader (or the next startup) sees either
/// the old file or the new one, never a torn mix.
///
/// The bytes go to a uniquely named temporary file in the same directory
/// (so the rename stays on one filesystem and concurrent writers never share
/// a temporary path), are `fsync`ed, then renamed over `path`; on Unix the
/// directory is `fsync`ed too so the rename itself survives a crash. On any
/// error the temporary file is removed and `path` is left untouched.
fn write_file_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => parent.to_path_buf(),
        None => std::path::PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("agents.json");
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{file_name}."))
        .suffix(".tmp")
        .tempfile_in(&parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_directory(&parent)
}

#[cfg(unix)]
fn sync_directory(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_dir: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Persists the agent list after a mutation. A failed write is logged rather
/// than surfaced so the HTTP contract of the CRUD routes stays unchanged; the
/// previous file on disk is intact because the write is atomic.
fn persist_user_agents(agents: &[Value]) {
    if let Err(error) = save_user_agents(agents) {
        tracing::error!(error = %error, "failed to persist user agents");
    }
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
    /// A tenant/project-local identifier. Omitted only for legacy user agents.
    pub id: Option<String>,
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
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    /// Stored verbatim. Terminal output validation is deliberately outside this
    /// definition-layer contract.
    pub output_schema: Option<Value>,
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
    let is_definition = req.id.is_some()
        || req.system_prompt.is_some()
        || req.model.is_some()
        || req.output_schema.is_some();
    let id = req
        .id
        .unwrap_or_else(|| uuid::Uuid::new_v4().hyphenated().to_string());
    if is_definition && !valid_agent_id(&id) {
        return definition_request_error(
            "agent id must start with a lowercase letter and contain only lowercase letters, digits, hyphens, or underscores (maximum 64 bytes)",
        )
        .into_response();
    }
    let definition = match (req.system_prompt, req.model) {
        (Some(system_prompt), Some(model))
            if !system_prompt.trim().is_empty() && !model.trim().is_empty() =>
        {
            Some(AgentDefinitionRevision {
                revision: "1".into(),
                system_prompt,
                model,
                output_schema: req.output_schema,
                created_at: chrono::Utc::now().to_rfc3339(),
            })
        }
        (None, None) if !is_definition => None,
        _ => {
            return definition_request_error(
                "agent definitions require non-empty system_prompt and model",
            )
            .into_response()
        }
    };
    let mut agent = json!({
        "id": id,
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
    let mut guard = state.user_agents.write().await;
    if guard.iter().any(|existing| {
        existing.get("id").and_then(Value::as_str) == agent.get("id").and_then(Value::as_str)
            && existing.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
            && existing.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
    }) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "agent_id_conflict", "message": "agent id already exists in this tenant and project" })),
        )
            .into_response();
    }
    if let Some(definition) = definition {
        let definition = serde_json::to_value(definition).expect("definition serializes");
        agent["active_revision"] = json!("1");
        agent["revisions"] = json!([definition]);
    }
    let id = agent["id"].as_str().unwrap_or("").to_string();
    guard.push(agent.clone());
    persist_user_agents(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": "created", "agent": agent })),
    )
}

#[derive(Deserialize)]
pub(crate) struct AgentRevisionCreateRequest {
    pub system_prompt: String,
    pub model: String,
    pub output_schema: Option<Value>,
}

/// GET /api/v1/agents/:id/revisions — list immutable definition snapshots.
pub(crate) async fn list_agent_revisions_handler(
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
    let guard = state.user_agents.read().await;
    let Some(agent) = guard.iter().find(|agent| {
        agent.get("id").and_then(Value::as_str) == Some(id.as_str())
            && agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
            && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found" })),
        )
            .into_response();
    };
    let Some(revisions) = agent.get("revisions").and_then(Value::as_array) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found" })),
        )
            .into_response();
    };
    Json(json!({
        "agent_id": id,
        "active_revision": agent.get("active_revision").cloned().unwrap_or(Value::Null),
        "revisions": revisions,
    }))
    .into_response()
}

/// POST /api/v1/agents/:id/revisions — append an immutable definition snapshot.
pub(crate) async fn create_agent_revision_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<AgentRevisionCreateRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "verified isolation claims are required" })),
        )
            .into_response();
    };
    if req.system_prompt.trim().is_empty() || req.model.trim().is_empty() {
        return definition_request_error(
            "agent definitions require non-empty system_prompt and model",
        )
        .into_response();
    }
    let mut guard = state.user_agents.write().await;
    let Some(agent) = guard.iter_mut().find(|agent| {
        agent.get("id").and_then(Value::as_str) == Some(id.as_str())
            && agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
            && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found" })),
        )
            .into_response();
    };
    let Some(revisions) = agent.get_mut("revisions").and_then(Value::as_array_mut) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found" })),
        )
            .into_response();
    };
    let next = revisions
        .iter()
        .filter_map(|revision| revision.get("revision").and_then(Value::as_str))
        .filter_map(|revision| revision.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let revision = AgentDefinitionRevision {
        revision: next.to_string(),
        system_prompt: req.system_prompt,
        model: req.model,
        output_schema: req.output_schema,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let revision_value = serde_json::to_value(&revision).expect("definition serializes");
    revisions.push(revision_value);
    if let Some(object) = agent.as_object_mut() {
        object.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
    }
    persist_user_agents(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "agent_id": id, "revision": revision })),
    )
        .into_response()
}

/// POST /api/v1/agents/:id/revisions/:revision/activate — select a previous
/// immutable snapshot as the active revision.
pub(crate) async fn activate_agent_revision_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path((id, revision)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "verified isolation claims are required" })),
        )
            .into_response();
    };
    let mut guard = state.user_agents.write().await;
    let Some(agent) = guard.iter_mut().find(|agent| {
        agent.get("id").and_then(Value::as_str) == Some(id.as_str())
            && agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
            && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent not found" })),
        )
            .into_response();
    };
    let found = agent
        .get("revisions")
        .and_then(Value::as_array)
        .is_some_and(|revisions| {
            revisions.iter().any(|candidate| {
                candidate.get("revision").and_then(Value::as_str) == Some(revision.as_str())
            })
        });
    if !found {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent revision not found" })),
        )
            .into_response();
    }
    if let Some(object) = agent.as_object_mut() {
        object.insert("active_revision".into(), json!(revision));
        object.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
    }
    let active_revision = agent["active_revision"].clone();
    persist_user_agents(&guard);
    Json(json!({ "agent_id": id, "active_revision": active_revision })).into_response()
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
                    if matches!(
                        k.as_str(),
                        "active_revision" | "revisions" | "system_prompt" | "model" | "output_schema"
                    ) {
                        return definition_request_error(
                            "agent revisions are immutable; create a revision or activate an existing revision",
                        )
                        .into_response();
                    }
                    obj.insert(k.clone(), v.clone());
                }
                obj.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
            }
            let updated = agent.clone();
            persist_user_agents(&guard);
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
    persist_user_agents(&guard);
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
        routing::{get, post, put},
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
            invocations: crate::api::http::invocations::InvocationsRuntime::unavailable(),
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

    #[tokio::test]
    async fn definitions_are_scoped_immutable_and_can_roll_back() {
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
            .route("/api/v1/agents", post(create_agent_handler))
            .route("/api/v1/agents/:id", put(update_agent_handler))
            .route(
                "/api/v1/agents/:id/revisions",
                get(list_agent_revisions_handler).post(create_agent_revision_handler),
            )
            .route(
                "/api/v1/agents/:id/revisions/:revision/activate",
                post(activate_agent_revision_handler),
            )
            .with_state(state.clone());
        let tenant_a = jwt("tenant-a", "project-a");
        let tenant_b = jwt("tenant-b", "project-a");

        let (status, created) = request(
            &router,
            Request::builder()
                .method("POST")
                .uri("/api/v1/agents")
                .header("authorization", format!("Bearer {tenant_a}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "id": "research_agent",
                        "name": "Research",
                        "system_prompt": "Use concise answers.",
                        "model": "model-one",
                        "output_schema": {"$id": "result-v1"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["agent"]["active_revision"], "1");

        let (status, revised) = request(
            &router,
            Request::builder()
                .method("POST")
                .uri("/api/v1/agents/research_agent/revisions")
                .header("authorization", format!("Bearer {tenant_a}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"system_prompt": "Use detailed answers.", "model": "model-two"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(revised["revision"]["revision"], "2");

        let (status, _) = request(
            &router,
            Request::builder()
                .method("PUT")
                .uri("/api/v1/agents/research_agent")
                .header("authorization", format!("Bearer {tenant_a}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"model": "changed"}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, rolled_back) = request(
            &router,
            Request::builder()
                .method("POST")
                .uri("/api/v1/agents/research_agent/revisions/1/activate")
                .header("authorization", format!("Bearer {tenant_a}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rolled_back["active_revision"], "1");

        let (status, _) = request(
            &router,
            Request::builder()
                .method("GET")
                .uri("/api/v1/agents/research_agent/revisions")
                .header("authorization", format!("Bearer {tenant_b}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let definitions = state.user_agents.read().await;
        let revision = resolve_definition_revision(
            &definitions,
            "tenant-a",
            "project-a",
            "research_agent",
            None,
        )
        .unwrap();
        assert_eq!(revision.revision, "1");
        assert_eq!(revision.model, "model-one");

        restore_env("AGENTOS_AUTH_MODE", previous_auth_mode);
        restore_env("AGENTOS_JWT_SECRET", previous_jwt_secret);
        restore_env("AGENTOS_DATA_DIR", previous_data_dir);
    }

    fn leftover_temporaries(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn atomic_write_replaces_file_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("agents.json");

        write_file_atomically(&path, b"[1]").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[1]");

        write_file_atomically(&path, b"[1,2]").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[1,2]");
        assert!(leftover_temporaries(path.parent().unwrap()).is_empty());
    }

    #[test]
    fn atomic_write_failure_keeps_target_and_cleans_temporary() {
        let dir = tempfile::tempdir().unwrap();
        // A non-empty directory at the target path makes the final rename fail.
        let path = dir.path().join("agents.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"original").unwrap();

        assert!(write_file_atomically(&path, b"[]").is_err());
        assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"original");
        assert!(leftover_temporaries(dir.path()).is_empty());
    }

    #[test]
    fn save_user_agents_round_trips_through_atomic_write() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        std::env::set_var("AGENTOS_DATA_DIR", dir.path());

        let agents = vec![json!({"id": "agent-a", "name": "Agent A"})];
        save_user_agents(&agents).unwrap();
        assert_eq!(load_user_agents(), agents);
        save_user_agents(&[]).unwrap();
        assert!(load_user_agents().is_empty());
        assert!(leftover_temporaries(dir.path()).is_empty());

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
