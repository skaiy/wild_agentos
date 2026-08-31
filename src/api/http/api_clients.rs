//! 管理面：调用方 & 密钥中心（需 DA 角色）。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；持久化模型在 `api_gov`。

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::api_gov::{self, ApiClient, ApiKey};
use super::iam::UserIdentity;
use super::AppState;

/// 密钥对外视图（绝不含 key_hash）。
fn key_public_view(k: &ApiKey) -> Value {
    json!({
        "id": k.id,
        "name": k.name,
        "client_id": k.client_id,
        "key_prefix": k.key_prefix,
        "status": k.status,
        "last_used_at": k.last_used_at,
        "expires_at": k.expires_at,
        "created_at": k.created_at,
    })
}

#[derive(Deserialize)]
pub struct CreateClientRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub granted_agent_ids: Vec<String>,
    pub rate_limit: Option<api_gov::RateLimit>,
    pub quota: Option<api_gov::Quota>,
}

#[derive(Deserialize)]
pub struct UpdateClientRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub granted_agent_ids: Option<Vec<String>>,
    pub status: Option<String>,
    pub rate_limit: Option<api_gov::RateLimit>,
    pub quota: Option<api_gov::Quota>,
}

#[derive(Deserialize)]
pub struct IssueKeyRequest {
    #[serde(default)]
    pub name: String,
    pub expires_at: Option<String>,
}

/// GET /api/v1/api-clients — 列出调用方（含密钥视图 + 实时用量快照）。
pub(crate) async fn list_api_clients_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let clients = state.api_clients.read().await;
    let keys = state.api_keys.read().await;
    let items: Vec<Value> = clients
        .iter()
        .map(|c| {
            let ckeys: Vec<Value> = keys
                .iter()
                .filter(|k| k.client_id == c.id)
                .map(key_public_view)
                .collect();
            json!({
                "id": c.id,
                "name": c.name,
                "description": c.description,
                "tenant_id": c.tenant_id,
                "owner": c.owner,
                "granted_agent_ids": c.granted_agent_ids,
                "status": c.status,
                "rate_limit": c.rate_limit,
                "quota": c.quota,
                "created_at": c.created_at,
                "updated_at": c.updated_at,
                "keys": ckeys,
                "usage": state.api_usage.snapshot(&c.id),
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "count": items.len(), "clients": items })),
    )
        .into_response()
}

/// POST /api/v1/api-clients — 创建调用方。
pub(crate) async fn create_api_client_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<CreateClientRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "name 不能为空" })),
        )
            .into_response();
    }
    let now = chrono::Utc::now().to_rfc3339();
    let client = ApiClient {
        id: uuid::Uuid::new_v4().hyphenated().to_string(),
        name: req.name.trim().to_string(),
        description: req.description,
        tenant_id: identity.tenant_id.clone(),
        owner: if req.owner.is_empty() {
            identity.user_id.clone()
        } else {
            req.owner
        },
        granted_agent_ids: req.granted_agent_ids,
        status: "active".to_string(),
        rate_limit: req.rate_limit.unwrap_or_default(),
        quota: req.quota.unwrap_or_default(),
        created_at: now.clone(),
        updated_at: now,
    };
    let mut guard = state.api_clients.write().await;
    guard.push(client.clone());
    let _ = api_gov::save_api_clients(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "status": "created", "client": client })),
    )
        .into_response()
}

/// PUT /api/v1/api-clients/:id — 更新调用方（改授权/限流/配额/启停）。
pub(crate) async fn update_api_client_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<UpdateClientRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let mut guard = state.api_clients.write().await;
    let client = match guard.iter_mut().find(|c| c.id == id) {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "client not found", "id": id })),
            )
                .into_response()
        }
    };
    if let Some(v) = req.name {
        client.name = v;
    }
    if let Some(v) = req.description {
        client.description = v;
    }
    if let Some(v) = req.owner {
        client.owner = v;
    }
    if let Some(v) = req.granted_agent_ids {
        client.granted_agent_ids = v;
    }
    if let Some(v) = req.status {
        client.status = v;
    }
    if let Some(v) = req.rate_limit {
        client.rate_limit = v;
    }
    if let Some(v) = req.quota {
        client.quota = v;
    }
    client.updated_at = chrono::Utc::now().to_rfc3339();
    let updated = client.clone();
    let _ = api_gov::save_api_clients(&guard);
    (
        StatusCode::OK,
        Json(json!({ "status": "updated", "client": updated })),
    )
        .into_response()
}

/// DELETE /api/v1/api-clients/:id — 删除调用方及其名下所有密钥。
pub(crate) async fn delete_api_client_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let mut clients = state.api_clients.write().await;
    let before = clients.len();
    clients.retain(|c| c.id != id);
    if clients.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "client not found", "id": id })),
        )
            .into_response();
    }
    let _ = api_gov::save_api_clients(&clients);
    let mut keys = state.api_keys.write().await;
    keys.retain(|k| k.client_id != id);
    let _ = api_gov::save_api_keys(&keys);
    (
        StatusCode::OK,
        Json(json!({ "status": "deleted", "id": id })),
    )
        .into_response()
}

/// POST /api/v1/api-clients/:id/keys — 为调用方签发新密钥（响应含明文，仅此一次）。
pub(crate) async fn issue_api_key_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<IssueKeyRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let tenant = {
        let clients = state.api_clients.read().await;
        match clients.iter().find(|c| c.id == id) {
            Some(c) => c.tenant_id.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "client not found", "id": id })),
                )
                    .into_response()
            }
        }
    };
    let (plaintext, prefix, hash) = api_gov::generate_key(&tenant);
    let key = ApiKey {
        id: uuid::Uuid::new_v4().hyphenated().to_string(),
        name: req.name,
        client_id: id.clone(),
        key_prefix: prefix,
        key_hash: hash,
        status: "active".to_string(),
        last_used_at: None,
        expires_at: req.expires_at,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let mut guard = state.api_keys.write().await;
    guard.push(key.clone());
    let _ = api_gov::save_api_keys(&guard);
    (
        StatusCode::CREATED,
        Json(json!({
            "status": "created",
            "key": key_public_view(&key),
            "api_key": plaintext,
            "warning": "该明文仅此一次返回，请立即妥善保存",
        })),
    )
        .into_response()
}

/// DELETE /api/v1/api-clients/:id/keys/:kid — 撤销某密钥。
pub(crate) async fn revoke_api_key_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path((id, kid)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let mut guard = state.api_keys.write().await;
    let key = guard.iter_mut().find(|k| k.id == kid && k.client_id == id);
    match key {
        Some(k) => {
            k.status = "revoked".to_string();
            let _ = api_gov::save_api_keys(&guard);
            (
                StatusCode::OK,
                Json(json!({ "status": "revoked", "id": kid })),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "key not found", "id": kid })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct AuditQuery {
    pub client_id: Option<String>,
    pub agent_id: Option<String>,
    pub limit: Option<usize>,
}

/// GET /api/v1/api-audit — 对外调用审计查询（按 client/agent 过滤，倒序）。
pub(crate) async fn list_api_audit_handler(
    identity: UserIdentity,
    Query(q): Query<AuditQuery>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let limit = q.limit.unwrap_or(200).min(1000);
    let items = api_gov::read_audit(q.client_id.as_deref(), q.agent_id.as_deref(), limit);
    (
        StatusCode::OK,
        Json(json!({ "count": items.len(), "records": items })),
    )
        .into_response()
}
