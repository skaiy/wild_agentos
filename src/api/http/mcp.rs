//! MCP 服务器注册表（HTTP 管理面）。

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{data_dir, AppState};

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

/// GET /api/v1/mcp/servers — 返回已注册的 MCP 服务器
pub(crate) async fn list_mcp_servers_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let servers = state.mcp_servers.read().await.clone();
    Json(json!({ "count": servers.len(), "servers": servers }))
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
    Json(req): Json<McpServerRegisterRequest>,
) -> impl IntoResponse {
    let server = json!({
        "id": uuid::Uuid::new_v4().hyphenated().to_string(),
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "endpoint": req.endpoint,
        "protocol": req.protocol.unwrap_or_else(|| "sse".to_string()),
        "status": "active",
    });
    let id = server["id"].as_str().unwrap_or("").to_string();
    let mut guard = state.mcp_servers.write().await;
    guard.push(server);
    let _ = save_mcp_servers(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": "registered" })),
    )
}
