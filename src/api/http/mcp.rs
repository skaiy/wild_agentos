//! MCP server catalog and authenticated outbound invocation surface.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
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
#[serde(deny_unknown_fields)]
pub struct McpServerRegisterRequest {
    pub name: String,
    pub description: Option<String>,
    pub endpoint: String,
    pub protocol: Option<String>,
    /// `bearer_jwt` opts this server into an outbound JWT minted from process
    /// environment. The catalog stores only the fixed environment references,
    /// never credential values.
    pub auth_kind: Option<String>,
}

fn catalog_auth(kind: Option<&str>) -> Result<Value, &'static str> {
    match kind {
        Some("bearer_jwt") => Ok(json!({
            "kind": "bearer_jwt",
            "secret_env": "MCP_JWT_SECRET",
            "issuer_env": "MCP_JWT_ISSUER",
            "audience_env": "MCP_JWT_AUDIENCE",
            "subject_env": "MCP_JWT_SUB",
        })),
        Some(_) => Err("auth_kind must be 'bearer_jwt' when supplied"),
        None => Ok(Value::Null),
    }
}

/// POST /api/v1/mcp/servers — register a catalog MCP server.
pub(crate) async fn register_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<McpServerRegisterRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return missing_isolation_claims().into_response();
    };
    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name must not be empty"})),
        )
            .into_response();
    }
    let protocol = req.protocol.unwrap_or_else(|| "sse".to_string());
    let valid_http_endpoint = reqwest::Url::parse(&req.endpoint)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false);
    if protocol == "http" && !valid_http_endpoint {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "HTTP MCP endpoint must be an absolute http(s) URL"})),
        )
            .into_response();
    }
    let auth = match catalog_auth(req.auth_kind.as_deref()) {
        Ok(auth) => auth,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response()
        }
    };
    let server = json!({
        "id": uuid::Uuid::new_v4().hyphenated().to_string(),
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "endpoint": req.endpoint,
        "protocol": protocol,
        "auth": auth,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpCatalogInvokeRequest {
    /// A catalog MCP `name` or `id`, resolved only within the caller's
    /// verified tenant/project scope.
    pub server: String,
    pub tool_name: String,
    #[serde(default = "empty_object")]
    pub arguments: Value,
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Debug, Serialize, Deserialize)]
struct OutboundMcpJwtClaims {
    iss: String,
    aud: String,
    sub: String,
    iat: usize,
    exp: usize,
}

fn mcp_env(name: &str, default: Option<&str>) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| default.map(str::to_owned))
        .ok_or_else(|| format!("{name} is not configured"))
}

/// Mints the credential for an *outbound* MCP request. It deliberately takes
/// no `IsolationClaims`: inbound isolation determines catalog visibility, not
/// the bearer credential trusted by the remote MCP server.
fn mint_outbound_mcp_jwt() -> Result<String, String> {
    let secret = mcp_env("MCP_JWT_SECRET", None)?;
    let now = chrono::Utc::now().timestamp() as usize;
    let claims = OutboundMcpJwtClaims {
        iss: mcp_env("MCP_JWT_ISSUER", Some("wodp-demo"))?,
        aud: mcp_env("MCP_JWT_AUDIENCE", Some("wodp-mcp"))?,
        sub: mcp_env("MCP_JWT_SUB", Some("wodp_agent"))?,
        iat: now,
        exp: now + 300,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|error| format!("failed to mint outbound MCP JWT: {error}"))
}

fn server_uses_bearer_jwt(server: &Value) -> bool {
    server
        .get("auth")
        .and_then(|auth| auth.get("kind"))
        .and_then(Value::as_str)
        == Some("bearer_jwt")
}

async fn invoke_http_mcp(
    endpoint: &str,
    bearer: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let response = reqwest::Client::new()
        .post(endpoint)
        .bearer_auth(bearer)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool_name, "arguments": arguments},
        }))
        .send()
        .await
        .map_err(|error| format!("MCP HTTP request failed: {error}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("MCP response parse failed: {error}"))?;
    if !status.is_success() {
        return Err(format!("MCP HTTP request failed with status {status}"));
    }
    if let Some(error) = body.get("error") {
        return Err(format!("MCP tools/call returned error: {error}"));
    }
    body.get("result")
        .cloned()
        .ok_or_else(|| "MCP tools/call response did not include a result".to_string())
}

/// POST /api/v1/mcp/servers/invoke — invoke a tool on a catalog HTTP MCP.
///
/// This is an outbound catalog proxy. It is distinct from `POST /mcp`, which
/// publishes tenant Skills to external MCP clients.
pub(crate) async fn invoke_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<McpCatalogInvokeRequest>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return missing_isolation_claims().into_response();
    };
    if request.tool_name.trim().is_empty() || !request.arguments.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "tool_name and object arguments are required"})),
        )
            .into_response();
    }
    let servers = state.mcp_servers.read().await;
    let matches: Vec<&Value> = servers
        .iter()
        .filter(|server| {
            server_is_in_scope(server, Some(claims))
                && (server.get("id").and_then(Value::as_str) == Some(request.server.as_str())
                    || server.get("name").and_then(Value::as_str) == Some(request.server.as_str()))
        })
        .collect();
    if matches.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "catalog MCP server not found"})),
        )
            .into_response();
    }
    if matches.len() > 1 {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "catalog MCP name is ambiguous; invoke by id"})),
        )
            .into_response();
    }
    let server = matches[0];
    if server.get("protocol").and_then(Value::as_str) != Some("http") {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "catalog MCP must use protocol=http for tool invocation"})),
        )
            .into_response();
    }
    if !server_uses_bearer_jwt(server) {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "catalog MCP is not configured with auth_kind=bearer_jwt"})),
        )
            .into_response();
    }
    let bearer = match mint_outbound_mcp_jwt() {
        Ok(token) => token,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "outbound_mcp_jwt_unavailable", "message": error})),
            )
                .into_response()
        }
    };
    let endpoint = server["endpoint"].as_str().unwrap_or_default();
    match invoke_http_mcp(endpoint, &bearer, &request.tool_name, request.arguments).await {
        Ok(result) => (StatusCode::OK, Json(json!({"result": result}))).into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "outbound_mcp_call_failed", "message": error})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isolation::IsolationClaims;
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use jsonwebtoken::{decode, DecodingKey, Validation};
    use tokio::net::TcpListener;

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

    #[test]
    fn catalog_bearer_auth_stores_only_environment_references() {
        let auth = catalog_auth(Some("bearer_jwt")).unwrap();
        assert_eq!(auth["secret_env"], "MCP_JWT_SECRET");
        assert!(auth.get("secret").is_none());
    }

    #[test]
    fn missing_mcp_jwt_secret_fails_closed() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("MCP_JWT_SECRET");
        std::env::remove_var("MCP_JWT_SECRET");
        assert!(mint_outbound_mcp_jwt().is_err());
        match previous {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
    }

    #[tokio::test]
    async fn outbound_call_sends_minted_bearer_not_isolation_claims() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_subject = std::env::var_os("MCP_JWT_SUB");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("MCP_JWT_SUB", "catalog-mcp-client");

        async fn mock_handler(
            State(seen): State<Arc<std::sync::Mutex<Option<String>>>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            *seen.lock().unwrap() = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            (
                StatusCode::OK,
                Json(json!({"jsonrpc": "2.0", "id": body["id"], "result": {"ok": true}})),
            )
        }

        let seen = Arc::new(std::sync::Mutex::new(None));
        let app = Router::new()
            .route("/mcp", post(mock_handler))
            .with_state(seen.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let bearer = mint_outbound_mcp_jwt().unwrap();
        let isolation =
            IsolationClaims::from_verified("tenant-not-mcp", "project", "actor").unwrap();
        let result = invoke_http_mcp(
            &format!("http://{address}/mcp"),
            &bearer,
            "health_check",
            json!({}),
        )
        .await
        .unwrap();
        assert_eq!(result, json!({"ok": true}));

        let header = seen.lock().unwrap().clone().unwrap();
        let token = header.strip_prefix("Bearer ").unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        let decoded = decode::<OutboundMcpJwtClaims>(
            token,
            &DecodingKey::from_secret(b"outbound-mcp-test-secret"),
            &validation,
        )
        .unwrap();
        assert_eq!(decoded.claims.sub, "catalog-mcp-client");
        assert_ne!(decoded.claims.sub, isolation.actor_id());
        assert!(!token.contains(isolation.tenant_id()));

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_subject {
            Some(value) => std::env::set_var("MCP_JWT_SUB", value),
            None => std::env::remove_var("MCP_JWT_SUB"),
        }
    }
}
