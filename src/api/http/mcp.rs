//! MCP 服务器注册表（HTTP 管理面）。

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{data_dir, iam::UserIdentity, AppState};

/// MCP 服务器注册表的持久化文件路径。
fn mcp_servers_store_path() -> std::path::PathBuf {
    data_dir().join("mcp_servers.json")
}

/// 启动时从磁盘加载已注册的 MCP 服务器；文件不存在或解析失败时返回空列表。
pub(crate) fn load_mcp_servers() -> Vec<Value> {
    match std::fs::read_to_string(mcp_servers_store_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 将 MCP 服务器注册表持久化到磁盘（pretty JSON）。
pub(crate) fn save_mcp_servers(servers: &[Value]) -> std::io::Result<()> {
    let path = mcp_servers_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(servers).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

fn server_is_in_scope(server: &Value, claims: Option<&crate::isolation::IsolationClaims>) -> bool {
    let Some(claims) = claims else {
        return false;
    };

    server.get("tenantId").and_then(Value::as_str) == Some(claims.tenant_id())
        && server.get("projectId").and_then(Value::as_str) == Some(claims.project_id())
}

fn missing_isolation_claims() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "verified_isolation_claims_required",
            "message": "Verified IsolationClaims are required for MCP server catalog access",
        })),
    )
}

/// GET /api/v1/mcp/servers — 返回当前租户/项目已注册的 MCP 服务器。
///
/// The HTTP management catalog is fail-closed: public/anonymous identities
/// cannot enumerate registrations, and legacy unscoped records stay hidden.
pub(crate) async fn list_mcp_servers_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return missing_isolation_claims().into_response();
    }

    let servers: Vec<Value> = state
        .mcp_servers
        .read()
        .await
        .iter()
        .filter(|server| server_is_in_scope(server, identity.isolation_claims()))
        .cloned()
        .collect();
    Json(json!({ "count": servers.len(), "servers": servers })).into_response()
}

#[derive(Deserialize)]
pub struct McpServerRegisterRequest {
    pub name: String,
    pub description: Option<String>,
    pub endpoint: String,
    pub protocol: Option<String>,
}

/// POST /api/v1/mcp/servers — 注册新的 MCP 服务器
pub(crate) async fn register_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<McpServerRegisterRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return missing_isolation_claims().into_response();
    };
    let server = json!({
        "id": uuid::Uuid::new_v4().hyphenated().to_string(),
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "endpoint": req.endpoint,
        "protocol": req.protocol.unwrap_or_else(|| "sse".to_string()),
        "status": "active",
        "tenantId": claims.tenant_id(),
        "projectId": claims.project_id(),
    });
    let id = server["id"].as_str().unwrap_or("").to_string();
    let mut guard = state.mcp_servers.write().await;
    guard.push(server);
    let _ = save_mcp_servers(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": "registered" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isolation::IsolationClaims;

    #[test]
    fn mcp_server_catalog_is_scoped_to_verified_claims() {
        let server = json!({
            "name": "tenant-b-server",
            "tenantId": "tenant-b",
            "projectId": "project",
        });
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project", "test-actor").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project", "test-actor").unwrap();

        assert!(!server_is_in_scope(&server, Some(&tenant_a)));
        assert!(server_is_in_scope(&server, Some(&tenant_b)));
        assert!(!server_is_in_scope(&server, None));
    }
}
