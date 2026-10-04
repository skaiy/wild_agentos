//! 管理面：调用方 & 密钥中心（需 DA 角色）。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；持久化模型在 `api_gov`。

use std::{collections::HashSet, sync::Arc};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
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

fn client_not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "client not found", "id": id })),
    )
        .into_response()
}

fn client_id_conflict(id: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "client id conflict",
            "id": id,
            "message": "client id is quarantined; an administrator must fix the data first",
        })),
    )
        .into_response()
}

fn key_not_found(kid: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "key not found", "id": kid })),
    )
        .into_response()
}

async fn valid_grants(state: &AppState, tenant: &str, ids: &[String]) -> bool {
    let agents = state.user_agents.read().await;
    ids.iter().all(|id| {
        agents.iter().any(|agent| {
            agent.get("id").and_then(Value::as_str) == Some(id)
                && agent.get("tenant_id").and_then(Value::as_str) == Some(tenant)
        })
    })
}

fn invalid_grants() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "invalid_granted_agent_ids" })),
    )
        .into_response()
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
    if let Err(error) = identity.require_control_plane_da("API client operations") {
        return error.into_response();
    }
    let tenant = identity
        .isolation_claims()
        .expect("DA claims required")
        .tenant_id();
    let clients = state.api_clients.read().await;
    let keys = state.api_keys.read().await;
    // Under a client id shared with another tenant, only keys carrying this
    // tenant's prefix are shown (none if the slugs are ambiguous).
    let collisions = api_gov::cross_tenant_client_id_collisions(&clients);
    let items: Vec<Value> = clients
        .iter()
        .filter(|c| c.tenant_id == tenant)
        .map(|c| {
            let ckeys: Vec<Value> = keys
                .iter()
                .filter(|k| k.client_id == c.id)
                .filter(|k| api_gov::tenant_may_manage_key(&collisions, k, tenant))
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
    if let Err(error) = identity.require_control_plane_da("API client operations") {
        return error.into_response();
    }
    let tenant = identity
        .isolation_claims()
        .expect("DA claims required")
        .tenant_id();
    if !valid_grants(&state, tenant, &req.granted_agent_ids).await {
        return invalid_grants();
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
        tenant_id: tenant.to_string(),
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
    if let Err(error) = identity.require_control_plane_da("API client operations") {
        return error.into_response();
    }
    let tenant = identity
        .isolation_claims()
        .expect("DA claims required")
        .tenant_id();
    // Resolve ownership first so an invalid grant cannot distinguish a foreign
    // client from a missing client. Recheck under the write lock below.
    if !state
        .api_clients
        .read()
        .await
        .iter()
        .any(|c| c.id == id && c.tenant_id == tenant)
    {
        return client_not_found(&id);
    }
    if let Some(ids) = &req.granted_agent_ids {
        if !valid_grants(&state, tenant, ids).await {
            return invalid_grants();
        }
    }
    let mut guard = state.api_clients.write().await;
    let client = match guard
        .iter_mut()
        .find(|c| c.id == id && c.tenant_id == tenant)
    {
        Some(c) => c,
        None => return client_not_found(&id),
    };
    // A quarantined (colliding) client must not be re-enabled through the API;
    // the collision has to be fixed in the data first. Second layer: auth also
    // rejects any duplicate id regardless of status.
    if req.status.is_some() && client.status == api_gov::CLIENT_ID_CONFLICT_STATUS {
        return client_id_conflict(&id);
    }
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
    if let Err(error) = identity.require_control_plane_da("API client operations") {
        return error.into_response();
    }
    let tenant = identity
        .isolation_claims()
        .expect("DA claims required")
        .tenant_id();
    let mut clients = state.api_clients.write().await;
    if !clients.iter().any(|c| c.id == id && c.tenant_id == tenant) {
        return client_not_found(&id);
    }
    // When another tenant shares this id, keys are removed only if they carry
    // this tenant's prefix; same-slug tenants cannot be told apart -> 409.
    let collisions = api_gov::cross_tenant_client_id_collisions(&clients);
    if collisions
        .get(&id)
        .is_some_and(|tenants| api_gov::colliding_tenant_slug_is_ambiguous(tenants, tenant))
    {
        return client_id_conflict(&id);
    }
    clients.retain(|c| c.id != id || c.tenant_id != tenant);
    let _ = api_gov::save_api_clients(&clients);
    let mut keys = state.api_keys.write().await;
    keys.retain(|k| k.client_id != id || !api_gov::tenant_may_manage_key(&collisions, k, tenant));
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
    if let Err(error) = identity.require_control_plane_da("API client operations") {
        return error.into_response();
    }
    let tenant = identity
        .isolation_claims()
        .expect("DA claims required")
        .tenant_id();
    {
        let clients = state.api_clients.read().await;
        if !clients.iter().any(|c| c.id == id && c.tenant_id == tenant) {
            return client_not_found(&id);
        }
    }
    let (plaintext, prefix, hash) = api_gov::generate_key(tenant);
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
    if let Err(error) = identity.require_control_plane_da("API client operations") {
        return error.into_response();
    }
    let tenant = identity
        .isolation_claims()
        .expect("DA claims required")
        .tenant_id();
    // Release the client lock before taking the key lock: `authenticate_public`
    // acquires keys then clients, so holding both here in the opposite order
    // could deadlock behind a queued writer.
    let owns_client = state
        .api_clients
        .read()
        .await
        .iter()
        .any(|c| c.id == id && c.tenant_id == tenant);
    if !owns_client {
        return key_not_found(&kid);
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
        None => key_not_found(&kid),
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
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Query(q): Query<AuditQuery>,
) -> impl IntoResponse {
    if let Err(error) = identity.require_control_plane_da("API audit access") {
        return error.into_response();
    }
    let tenant = identity
        .isolation_claims()
        .expect("DA claims required")
        .tenant_id();
    // Legacy records without tenant_id are attributed by client_id only, so
    // ids shared with another tenant (or quarantined at load) are excluded.
    let tenant_client_ids: HashSet<String> =
        api_gov::legacy_audit_client_ids(&state.api_clients.read().await, tenant);
    let limit = q.limit.unwrap_or(200).min(1000);
    let items = api_gov::read_audit_for_tenant(
        tenant,
        &tenant_client_ids,
        q.client_id.as_deref(),
        q.agent_id.as_deref(),
        limit,
    );
    (
        StatusCode::OK,
        Json(json!({ "count": items.len(), "records": items })),
    )
        .into_response()
}
