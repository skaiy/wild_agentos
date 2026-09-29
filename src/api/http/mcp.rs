//! MCP server catalog and authenticated outbound invocation surface.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use futures::StreamExt;
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

const OUTBOUND_MCP_REQUEST_ID: u64 = 1;
const MAX_OUTBOUND_MCP_SSE_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
enum InvokeHttpMcpError {
    Transport(String),
    JsonRpc(Value),
}

async fn read_sse_json_rpc_response(
    response: reqwest::Response,
    request_id: u64,
) -> Result<Value, InvokeHttpMcpError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            InvokeHttpMcpError::Transport(format!("MCP SSE response read failed: {error}"))
        })?;
        if body.len() + chunk.len() > MAX_OUTBOUND_MCP_SSE_RESPONSE_BYTES {
            return Err(InvokeHttpMcpError::Transport(format!(
                "MCP SSE response exceeded {MAX_OUTBOUND_MCP_SSE_RESPONSE_BYTES} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    let body = std::str::from_utf8(&body).map_err(|error| {
        InvokeHttpMcpError::Transport(format!("MCP SSE response was not valid UTF-8: {error}"))
    })?;
    let mut event_type = None;
    let mut data = Vec::new();

    for line in body.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty()
                && event_type
                    .as_deref()
                    .map_or(true, |event| event == "message")
            {
                let message: Value = serde_json::from_str(&data.join("\n")).map_err(|error| {
                    InvokeHttpMcpError::Transport(format!(
                        "MCP SSE event data was not valid JSON-RPC JSON: {error}"
                    ))
                })?;
                if message.get("id") == Some(&json!(request_id)) {
                    return Ok(message);
                }
            }
            event_type = None;
            data.clear();
            continue;
        }
        if line.starts_with(':') {
            continue;
        }

        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event_type = Some(value.to_string()),
            "data" => data.push(value.to_string()),
            _ => {}
        }
    }

    Err(InvokeHttpMcpError::Transport(
        "MCP SSE response did not include a JSON-RPC message for the request".to_string(),
    ))
}

async fn invoke_http_mcp(
    endpoint: &str,
    bearer: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<Value, InvokeHttpMcpError> {
    let response = reqwest::Client::new()
        .post(endpoint)
        .bearer_auth(bearer)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": OUTBOUND_MCP_REQUEST_ID,
            "method": "tools/call",
            "params": {"name": tool_name, "arguments": arguments},
        }))
        .send()
        .await
        .map_err(|error| {
            InvokeHttpMcpError::Transport(format!("MCP HTTP request failed: {error}"))
        })?;
    let status = response.status();
    let is_sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    let body = if is_sse {
        read_sse_json_rpc_response(response, OUTBOUND_MCP_REQUEST_ID).await?
    } else {
        response.json().await.map_err(|error| {
            InvokeHttpMcpError::Transport(format!("MCP response parse failed: {error}"))
        })?
    };
    if let Some(error) = body.get("error") {
        return Err(InvokeHttpMcpError::JsonRpc(error.clone()));
    }
    if !status.is_success() {
        return Err(InvokeHttpMcpError::Transport(format!(
            "MCP HTTP request failed with status {status}"
        )));
    }
    body.get("result").cloned().ok_or_else(|| {
        InvokeHttpMcpError::Transport(
            "MCP tools/call response did not include a result".to_string(),
        )
    })
}

fn outbound_mcp_failure_response(error: InvokeHttpMcpError) -> axum::response::Response {
    match error {
        InvokeHttpMcpError::JsonRpc(error) => {
            (StatusCode::OK, Json(json!({"error": error}))).into_response()
        }
        InvokeHttpMcpError::Transport(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "outbound_mcp_call_failed", "message": error})),
        )
            .into_response(),
    }
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
        Err(error) => outbound_mcp_failure_response(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isolation::IsolationClaims;
    use axum::{
        extract::State,
        http::{
            header::{ACCEPT, CONTENT_TYPE},
            HeaderMap, StatusCode,
        },
        response::IntoResponse,
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

        #[derive(Default)]
        struct SeenHeaders {
            authorization: Option<String>,
            accept: Option<String>,
            content_type: Option<String>,
        }

        async fn mock_handler(
            State(seen): State<Arc<std::sync::Mutex<SeenHeaders>>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            let mut seen = seen.lock().unwrap();
            seen.authorization = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            seen.accept = headers
                .get(ACCEPT)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            seen.content_type = headers
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if !seen
                .accept
                .as_deref()
                .is_some_and(|value| value.contains("text/event-stream"))
            {
                return StatusCode::NOT_ACCEPTABLE.into_response();
            }
            (
                StatusCode::OK,
                Json(json!({"jsonrpc": "2.0", "id": body["id"], "result": {"ok": true}})),
            )
                .into_response()
        }

        let seen = Arc::new(std::sync::Mutex::new(SeenHeaders::default()));
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

        let seen = seen.lock().unwrap();
        let header = seen.authorization.as_deref().unwrap();
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
        let accept = seen.accept.as_deref().unwrap();
        assert!(accept.contains("application/json"));
        assert!(accept.contains("text/event-stream"));
        assert_eq!(seen.content_type.as_deref(), Some("application/json"));

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_subject {
            Some(value) => std::env::set_var("MCP_JWT_SUB", value),
            None => std::env::remove_var("MCP_JWT_SUB"),
        }
    }

    #[tokio::test]
    async fn outbound_call_accepts_json_and_sse_streamable_http_responses() {
        async fn mock_handler(
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            let accepts_streaming = headers
                .get(ACCEPT)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value.contains("application/json") && value.contains("text/event-stream")
                });
            if !accepts_streaming {
                return StatusCode::NOT_ACCEPTABLE.into_response();
            }

            let id = body["id"].clone();
            match body["params"]["name"].as_str() {
                Some("sse_result") => (
                    [(CONTENT_TYPE, "text/event-stream")],
                    format!(
                        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\ndata: \"id\":{id},\"result\":{{\"transport\":\"sse\"}}}}\n\n"
                    ),
                )
                    .into_response(),
                _ => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"transport": "json"},
                }))
                .into_response(),
            }
        }

        let app = Router::new().route("/mcp", post(mock_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = format!("http://{address}/mcp");

        assert_eq!(
            invoke_http_mcp(&endpoint, "test-token", "json_result", json!({}))
                .await
                .unwrap(),
            json!({"transport": "json"})
        );
        assert_eq!(
            invoke_http_mcp(&endpoint, "test-token", "sse_result", json!({}))
                .await
                .unwrap(),
            json!({"transport": "sse"})
        );
    }

    #[tokio::test]
    async fn outbound_call_surfaces_sse_json_rpc_errors() {
        async fn mock_handler(Json(body): Json<Value>) -> axum::response::Response {
            (
                [(CONTENT_TYPE, "text/event-stream")],
                format!(
                    "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":-32001,\"message\":\"upstream denied\"}}}}\n\n",
                    body["id"]
                ),
            )
                .into_response()
        }

        let app = Router::new().route("/mcp", post(mock_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = format!("http://{address}/mcp");
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = invoke_http_mcp(&endpoint, "test-token", "error_result", json!({}))
            .await
            .unwrap_err();
        let InvokeHttpMcpError::JsonRpc(error) = error else {
            panic!("SSE JSON-RPC error must not be converted to a transport failure");
        };
        assert_eq!(error["code"], -32001);
        assert_eq!(error["message"], "upstream denied");

        let response = outbound_mcp_failure_response(InvokeHttpMcpError::JsonRpc(error));
        assert_eq!(response.status(), StatusCode::OK);
    }
}
