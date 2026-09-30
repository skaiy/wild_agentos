//! MCP server catalog and authenticated outbound invocation surface.

use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use futures::StreamExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{data_dir, iam::UserIdentity, AppState};
use crate::isolation::IsolationScopeProvenance;

const MAX_ALLOWED_TOOLS: usize = 64;
const MAX_TOOL_NAME_LENGTH: usize = 128;
const DEFAULT_OUTBOUND_MCP_CONNECT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_OUTBOUND_MCP_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_OUTBOUND_MCP_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MCP_CATALOG_ADMIN_ROLE: &str = "mcp_admin";
const WRITE_CLASS_TOOL_PREFIXES: &[&str] = &[
    "create_",
    "update_",
    "delete_",
    "generate_",
    "execute_",
    "add_",
    "remove_",
    "apply_",
    "duplicate_",
    "restore_",
    "save_",
    "manage_",
    "set_",
    "write_",
    "insert_",
    "drop_",
    "upsert_",
    "import_",
    "publish_",
    "send_",
];

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
    /// Optional environment variable holding this server's JWT audience.
    /// The catalog stores the variable name, never its value.
    pub audience_env: Option<String>,
    /// Optional per-server total outbound request timeout in seconds.
    pub timeout_seconds: Option<u64>,
    /// Exact, case-sensitive tool names allowed for this server. An empty
    /// array explicitly denies every tool.
    pub allowed_tools: Option<Vec<String>>,
    /// Write-class tools require both an allowlist entry and this explicit
    /// opt-in. Omitted values remain disabled.
    pub write_tools_enabled: Option<bool>,
}

fn is_valid_tool_name(tool_name: &str) -> bool {
    !tool_name.is_empty()
        && tool_name.len() <= MAX_TOOL_NAME_LENGTH
        && tool_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

fn is_valid_environment_variable_name(name: &str) -> bool {
    let mut characters = name.bytes();
    matches!(characters.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && characters.all(|character| character.is_ascii_alphanumeric() || character == b'_')
}

fn validate_tool_policy(
    allowed_tools: Option<&[String]>,
    write_tools_enabled: Option<bool>,
) -> Result<(), &'static str> {
    if write_tools_enabled == Some(true) && allowed_tools.is_none_or(|tools| tools.is_empty()) {
        return Err("write_tools_enabled requires a non-empty allowed_tools list");
    }
    let Some(allowed_tools) = allowed_tools else {
        return Ok(());
    };
    if allowed_tools.len() > MAX_ALLOWED_TOOLS {
        return Err("allowed_tools exceeds the maximum number of entries");
    }
    let mut seen = HashSet::with_capacity(allowed_tools.len());
    for tool_name in allowed_tools {
        if !is_valid_tool_name(tool_name) {
            return Err(
                "allowed_tools entries must be 1-128 ASCII letters, digits, '.', '_', '-' or '/'",
            );
        }
        if !seen.insert(tool_name) {
            return Err("allowed_tools entries must be unique");
        }
    }
    Ok(())
}

fn catalog_auth(kind: Option<&str>) -> Result<Value, &'static str> {
    match kind {
        Some("bearer_jwt") => Ok(json!({
            "kind": "bearer_jwt",
            "secret_env": "MCP_JWT_SECRET",
            "issuer_env": "MCP_JWT_ISSUER",
            "subject_env": "MCP_JWT_SUB",
        })),
        Some(_) => Err("auth_kind must be 'bearer_jwt' when supplied"),
        None => Ok(Value::Null),
    }
}

fn endpoint_origin(endpoint: &str) -> Result<String, &'static str> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| "HTTP MCP endpoint must be an absolute http(s) URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("HTTP MCP endpoint must be an absolute http(s) URL without user credentials");
    }
    Ok(url.origin().ascii_serialization())
}

fn configured_outbound_mcp_origins() -> Result<Option<HashSet<String>>, &'static str> {
    let Ok(configured) = std::env::var("MCP_OUTBOUND_ALLOWED_ORIGINS") else {
        return Ok(None);
    };
    let origins: HashSet<_> = configured
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(endpoint_origin)
        .collect::<Result<_, _>>()?;
    if origins.is_empty() {
        return Err("MCP_OUTBOUND_ALLOWED_ORIGINS must contain at least one http(s) origin");
    }
    Ok(Some(origins))
}

pub(crate) fn validate_strict_mcp_outbound_configuration() -> Result<(), &'static str> {
    let strict_mode = std::env::var("AGENTOS_AUTH_STRICT").as_deref() == Ok("true");
    if strict_mode && configured_outbound_mcp_origins()?.is_none() {
        return Err(
            "MCP_OUTBOUND_ALLOWED_ORIGINS must be configured when AGENTOS_AUTH_STRICT=true",
        );
    }
    Ok(())
}

fn validate_outbound_mcp_endpoint(server: &Value) -> Result<(), &'static str> {
    let endpoint = server
        .get("endpoint")
        .and_then(Value::as_str)
        .ok_or("catalog MCP endpoint is missing")?;
    let origin = endpoint_origin(endpoint)?;
    let registered_origin = server
        .get("endpoint_origin")
        .and_then(Value::as_str)
        .ok_or("catalog MCP endpoint is missing its registered origin")?;
    if origin != registered_origin {
        return Err("catalog MCP endpoint does not match its registered origin");
    }
    if let Some(allowed_origins) = configured_outbound_mcp_origins()? {
        if !allowed_origins.contains(&origin) {
            return Err("catalog MCP endpoint origin is not allowed");
        }
    }
    Ok(())
}

fn validate_outbound_timeout(timeout_seconds: Option<u64>) -> Result<(), &'static str> {
    if timeout_seconds.is_some_and(|timeout| timeout == 0 || timeout > 300) {
        return Err("timeout_seconds must be between 1 and 300");
    }
    Ok(())
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
    if let Err(error) = identity.require_role(MCP_CATALOG_ADMIN_ROLE) {
        return error.into_response();
    }
    if let Err(error) = validate_strict_mcp_outbound_configuration() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "mcp_outbound_allowlist_required", "message": error})),
        )
            .into_response();
    }
    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name must not be empty"})),
        )
            .into_response();
    }
    let protocol = req.protocol.unwrap_or_else(|| "sse".to_string());
    let registered_origin = if protocol == "http" {
        match endpoint_origin(&req.endpoint) {
            Ok(origin) => {
                if let Some(allowed_origins) = match configured_outbound_mcp_origins() {
                    Ok(origins) => origins,
                    Err(error) => {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": error})))
                            .into_response()
                    }
                } {
                    if !allowed_origins.contains(&origin) {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({"error": "catalog MCP endpoint origin is not allowed"})),
                        )
                            .into_response();
                    }
                }
                Some(origin)
            }
            Err(error) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
            }
        }
    } else {
        None
    };
    let auth = match catalog_auth(req.auth_kind.as_deref()) {
        Ok(auth) => auth,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response()
        }
    };
    if let Err(message) =
        validate_tool_policy(req.allowed_tools.as_deref(), req.write_tools_enabled)
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
    }
    if req
        .audience_env
        .as_deref()
        .is_some_and(|name| !is_valid_environment_variable_name(name))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "audience_env must be an environment variable name"})),
        )
            .into_response();
    }
    if let Err(message) = validate_outbound_timeout(req.timeout_seconds) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
    }
    let server = json!({
        "id": uuid::Uuid::new_v4().hyphenated().to_string(),
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "endpoint": req.endpoint,
        "endpoint_origin": registered_origin,
        "protocol": protocol,
        "auth": auth,
        "audience_env": req.audience_env,
        "timeout_seconds": req.timeout_seconds,
        "allowed_tools": req.allowed_tools,
        "write_tools_enabled": req.write_tools_enabled.unwrap_or(false),
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

/// DELETE /api/v1/mcp/servers/:id — remove a catalog MCP server.
pub(crate) async fn delete_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(claims) = identity.isolation_claims() else {
        return missing_isolation_claims().into_response();
    };
    if let Err(error) = identity.require_role(MCP_CATALOG_ADMIN_ROLE) {
        return error.into_response();
    }
    if let Err(error) = validate_strict_mcp_outbound_configuration() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "mcp_outbound_allowlist_required", "message": error})),
        )
            .into_response();
    }

    let mut servers = state.mcp_servers.write().await;
    let Some(position) = servers.iter().position(|server| {
        server_is_in_scope(server, Some(claims))
            && server.get("id").and_then(Value::as_str) == Some(id.as_str())
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "catalog MCP server not found"})),
        )
            .into_response();
    };
    servers.remove(position);
    if let Err(error) = save_mcp_servers(&servers) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to persist catalog MCP servers", "message": error.to_string()})),
        )
            .into_response();
    }
    StatusCode::NO_CONTENT.into_response()
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
    tenant_id: String,
    project_id: String,
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
fn mint_outbound_mcp_jwt(
    audience: &str,
    isolation_claims: &crate::isolation::IsolationClaims,
) -> Result<String, String> {
    let secret = mcp_env("MCP_JWT_SECRET", None)?;
    if isolation_claims.provenance() != IsolationScopeProvenance::VerifiedExplicit {
        return Err(
            "verified isolation claims must explicitly include tenant_id and project_id"
                .to_string(),
        );
    }
    let tenant_id = isolation_claims.tenant_id();
    let project_id = isolation_claims.project_id();
    if tenant_id.trim().is_empty() || project_id.trim().is_empty() {
        return Err("verified isolation claims must include tenant_id and project_id".to_string());
    }
    let now = chrono::Utc::now().timestamp() as usize;
    let claims = OutboundMcpJwtClaims {
        iss: mcp_env("MCP_JWT_ISSUER", Some("wild-agentos-core"))?,
        aud: audience.to_string(),
        sub: mcp_env("MCP_JWT_SUBJECT", Some("wao-core"))?,
        tenant_id: tenant_id.to_string(),
        project_id: project_id.to_string(),
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

fn outbound_mcp_audience(server: &Value) -> Result<String, &'static str> {
    match server.get("audience_env") {
        Some(Value::String(name)) => {
            if !is_valid_environment_variable_name(name) {
                return Err("catalog MCP audience_env is invalid");
            }
            mcp_env(name, None)
                .map_err(|_| "catalog MCP audience environment variable is not configured")
        }
        Some(Value::Null) | None => server
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or("catalog MCP server is missing its id"),
        Some(_) => Err("catalog MCP audience_env is invalid"),
    }
}

fn server_uses_bearer_jwt(server: &Value) -> bool {
    server
        .get("auth")
        .and_then(|auth| auth.get("kind"))
        .and_then(Value::as_str)
        == Some("bearer_jwt")
}

const OUTBOUND_MCP_REQUEST_ID: u64 = 1;

#[derive(Debug)]
enum InvokeHttpMcpError {
    Transport(String),
    JsonRpc(Value),
}

fn outbound_mcp_configured_positive_u64(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .ok()
            .filter(|value: &u64| *value > 0)
            .ok_or_else(|| format!("{name} must be a positive integer")),
        Err(_) => Ok(default),
    }
}

fn outbound_mcp_configured_positive_usize(name: &str, default: usize) -> Result<usize, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .ok()
            .filter(|value: &usize| *value > 0)
            .ok_or_else(|| format!("{name} must be a positive integer")),
        Err(_) => Ok(default),
    }
}

fn outbound_mcp_effective_timeout_ms(
    server_timeout_seconds: Option<u64>,
    global_timeout_ms: u64,
) -> Result<u64, String> {
    match server_timeout_seconds {
        Some(timeout_seconds) if (1..=300).contains(&timeout_seconds) => {
            Ok((timeout_seconds * 1_000).min(global_timeout_ms))
        }
        Some(_) => Err("catalog MCP timeout_seconds must be between 1 and 300".to_string()),
        None => Ok(global_timeout_ms),
    }
}

fn outbound_mcp_client(
    server_timeout_seconds: Option<u64>,
) -> Result<(reqwest::Client, usize), InvokeHttpMcpError> {
    let connect_timeout = outbound_mcp_configured_positive_u64(
        "MCP_OUTBOUND_CONNECT_TIMEOUT_MS",
        DEFAULT_OUTBOUND_MCP_CONNECT_TIMEOUT_MS,
    )
    .map_err(InvokeHttpMcpError::Transport)?;
    let global_timeout = outbound_mcp_configured_positive_u64(
        "MCP_OUTBOUND_TIMEOUT_MS",
        DEFAULT_OUTBOUND_MCP_TIMEOUT_MS,
    )
    .map_err(InvokeHttpMcpError::Transport)?;
    let timeout = outbound_mcp_effective_timeout_ms(server_timeout_seconds, global_timeout)
        .map_err(InvokeHttpMcpError::Transport)?;
    let max_response_bytes = outbound_mcp_configured_positive_usize(
        "MCP_OUTBOUND_MAX_RESPONSE_BYTES",
        DEFAULT_OUTBOUND_MCP_MAX_RESPONSE_BYTES,
    )
    .map_err(InvokeHttpMcpError::Transport)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(connect_timeout))
        .timeout(Duration::from_millis(timeout))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            InvokeHttpMcpError::Transport(format!("MCP HTTP client setup failed: {error}"))
        })?;
    Ok((client, max_response_bytes))
}

async fn read_bounded_response(
    response: reqwest::Response,
    max_response_bytes: usize,
) -> Result<Vec<u8>, InvokeHttpMcpError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            InvokeHttpMcpError::Transport(format!("MCP SSE response read failed: {error}"))
        })?;
        if body.len() + chunk.len() > max_response_bytes {
            return Err(InvokeHttpMcpError::Transport(format!(
                "MCP response exceeded {max_response_bytes} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn read_sse_json_rpc_response(body: &[u8], request_id: u64) -> Result<Value, InvokeHttpMcpError> {
    let body = std::str::from_utf8(body).map_err(|error| {
        InvokeHttpMcpError::Transport(format!("MCP SSE response was not valid UTF-8: {error}"))
    })?;
    let mut event_type = None;
    let mut data = Vec::new();

    for line in body.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty() && event_type.as_deref().is_none_or(|event| event == "message") {
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

fn is_write_class_tool(name: &str) -> bool {
    WRITE_CLASS_TOOL_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[derive(Debug, PartialEq, Eq)]
enum ToolPolicyDenied {
    ToolNotAllowed,
    WriteToolBlocked,
}

fn check_tool_policy(server: &Value, tool_name: &str) -> Result<(), ToolPolicyDenied> {
    match server.get("allowed_tools") {
        Some(Value::Array(allowed_tools)) => {
            let listed = allowed_tools
                .iter()
                .any(|allowed_tool| allowed_tool.as_str() == Some(tool_name));
            if !listed {
                return Err(ToolPolicyDenied::ToolNotAllowed);
            }
            if is_write_class_tool(tool_name)
                && server.get("write_tools_enabled").and_then(Value::as_bool) != Some(true)
            {
                return Err(ToolPolicyDenied::WriteToolBlocked);
            }
        }
        Some(Value::Null) | None => {
            if is_write_class_tool(tool_name) {
                return Err(ToolPolicyDenied::WriteToolBlocked);
            }
            tracing::warn!(
                server_id = server
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown"),
                "catalog MCP server has no allowed_tools policy; add an allowlist"
            );
        }
        Some(_) => return Err(ToolPolicyDenied::ToolNotAllowed),
    }
    Ok(())
}

fn tool_policy_denied_response(
    denied: ToolPolicyDenied,
    tool_name: &str,
) -> (StatusCode, Json<Value>) {
    let error = match denied {
        ToolPolicyDenied::ToolNotAllowed => "mcp_tool_not_allowed",
        ToolPolicyDenied::WriteToolBlocked => "mcp_write_tool_blocked",
    };
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error": error, "tool_name": tool_name})),
    )
}

async fn invoke_http_mcp(
    endpoint: &str,
    bearer: &str,
    tool_name: &str,
    arguments: Value,
    timeout_seconds: Option<u64>,
) -> Result<Value, InvokeHttpMcpError> {
    let (client, max_response_bytes) = outbound_mcp_client(timeout_seconds)?;
    let response = client
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
            if error.is_timeout() {
                InvokeHttpMcpError::Transport("MCP HTTP request timed out".to_string())
            } else {
                InvokeHttpMcpError::Transport(format!("MCP HTTP request failed: {error}"))
            }
        })?;
    let status = response.status();
    if status.is_redirection() {
        return Err(InvokeHttpMcpError::Transport(format!(
            "MCP HTTP request returned redirect status {status}"
        )));
    }
    let is_sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    let response = read_bounded_response(response, max_response_bytes).await?;
    let body = if is_sse {
        read_sse_json_rpc_response(&response, OUTBOUND_MCP_REQUEST_ID)?
    } else {
        serde_json::from_slice(&response).map_err(|error| {
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
            if error == "MCP HTTP request timed out" {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            },
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
    match claims.provenance() {
        IsolationScopeProvenance::VerifiedExplicit => {}
        IsolationScopeProvenance::VerifiedDefaulted => {
            let missing_field = claims
                .missing_scope_field()
                .map(|field| field.as_str())
                .unwrap_or("project_id");
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "mcp_claims_incomplete",
                    "missing_field": missing_field,
                })),
            )
                .into_response();
        }
        IsolationScopeProvenance::DeploymentConfig => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "mcp_claims_unverified"})),
            )
                .into_response()
        }
    }
    if let Err(error) = validate_strict_mcp_outbound_configuration() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "mcp_outbound_allowlist_required", "message": error})),
        )
            .into_response();
    }
    if request.tool_name.trim().is_empty() || !request.arguments.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "tool_name and object arguments are required"})),
        )
            .into_response();
    }
    let server = {
        let servers = state.mcp_servers.read().await;
        let matches: Vec<&Value> = servers
            .iter()
            .filter(|server| {
                server_is_in_scope(server, Some(claims))
                    && (server.get("id").and_then(Value::as_str) == Some(request.server.as_str())
                        || server.get("name").and_then(Value::as_str)
                            == Some(request.server.as_str()))
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
        matches[0].clone()
    };
    if server.get("protocol").and_then(Value::as_str) != Some("http") {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "catalog MCP must use protocol=http for tool invocation"})),
        )
            .into_response();
    }
    if !server_uses_bearer_jwt(&server) {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "catalog MCP is not configured with auth_kind=bearer_jwt"})),
        )
            .into_response();
    }
    if let Err(denied) = check_tool_policy(&server, &request.tool_name) {
        return tool_policy_denied_response(denied, &request.tool_name).into_response();
    }
    if let Err(error) = validate_outbound_mcp_endpoint(&server) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "mcp_endpoint_not_allowed", "message": error})),
        )
            .into_response();
    }
    let audience = match outbound_mcp_audience(&server) {
        Ok(audience) => audience,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "outbound_mcp_jwt_unavailable", "message": error})),
            )
                .into_response()
        }
    };
    let bearer = match mint_outbound_mcp_jwt(&audience, claims) {
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
    let timeout_seconds = server.get("timeout_seconds").and_then(Value::as_u64);
    match invoke_http_mcp(
        endpoint,
        &bearer,
        &request.tool_name,
        request.arguments,
        timeout_seconds,
    )
    .await
    {
        Ok(result) => (StatusCode::OK, Json(json!({"result": result}))).into_response(),
        Err(error) => outbound_mcp_failure_response(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isolation::IsolationClaims;
    use axum::{
        body::Body,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tower::ServiceExt;

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
        let claims =
            IsolationClaims::from_verified("test-tenant", "test-project", "test-actor").unwrap();
        assert!(mint_outbound_mcp_jwt("server-id", &claims).is_err());
        match previous {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
    }

    #[test]
    fn non_jwt_scope_claims_cannot_mint_outbound_mcp_credentials() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        let defaulted_scope =
            IsolationClaims::from_verified("test-tenant", "default", "test-actor").unwrap();
        let error = mint_outbound_mcp_jwt("server-id", &defaulted_scope).unwrap_err();
        assert!(error.contains("explicitly include tenant_id and project_id"));
        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
    }

    #[test]
    fn tool_policy_registration_validation_rejects_invalid_values() {
        assert!(validate_tool_policy(Some(&["read_status".into()]), None).is_ok());
        assert!(validate_tool_policy(None, None).is_ok());
        assert!(validate_tool_policy(Some(&[]), None).is_ok());
        assert!(validate_tool_policy(Some(&["".into()]), None).is_err());
        assert!(validate_tool_policy(Some(&["a".repeat(MAX_TOOL_NAME_LENGTH + 1)]), None).is_err());
        assert!(validate_tool_policy(Some(&["invalid tool".into()]), None).is_err());
        assert!(
            validate_tool_policy(Some(&["read_status".into(), "read_status".into()]), None)
                .is_err()
        );
        assert!(validate_tool_policy(
            Some(&vec!["read_status".into(); MAX_ALLOWED_TOOLS + 1]),
            None
        )
        .is_err());
        assert!(validate_tool_policy(None, Some(true)).is_err());
        assert!(validate_tool_policy(Some(&[]), Some(true)).is_err());
        assert!(validate_tool_policy(Some(&["create_report".into()]), Some(true)).is_ok());
    }

    #[test]
    fn tool_policy_enforces_allowlist_and_write_gate() {
        let allowlist = json!({
            "allowed_tools": ["read_status", "create_report"],
            "write_tools_enabled": false,
        });
        assert_eq!(check_tool_policy(&allowlist, "read_status"), Ok(()));
        assert_eq!(
            check_tool_policy(&allowlist, "not_listed"),
            Err(ToolPolicyDenied::ToolNotAllowed)
        );
        assert_eq!(
            check_tool_policy(&allowlist, "create_report"),
            Err(ToolPolicyDenied::WriteToolBlocked)
        );

        let write_enabled = json!({
            "allowed_tools": ["create_report"],
            "write_tools_enabled": true,
        });
        assert_eq!(check_tool_policy(&write_enabled, "create_report"), Ok(()));
        assert_eq!(
            check_tool_policy(&json!({}), "delete_report"),
            Err(ToolPolicyDenied::WriteToolBlocked)
        );
        assert_eq!(check_tool_policy(&json!({}), "read_status"), Ok(()));
        assert_eq!(
            check_tool_policy(&json!({"allowed_tools": []}), "read_status"),
            Err(ToolPolicyDenied::ToolNotAllowed)
        );
    }

    #[test]
    fn per_server_timeout_is_capped_by_global_timeout() {
        assert_eq!(outbound_mcp_effective_timeout_ms(None, 15_000), Ok(15_000));
        assert_eq!(
            outbound_mcp_effective_timeout_ms(Some(2), 15_000),
            Ok(2_000)
        );
        assert_eq!(
            outbound_mcp_effective_timeout_ms(Some(30), 5_000),
            Ok(5_000)
        );
        assert!(outbound_mcp_effective_timeout_ms(Some(0), 15_000).is_err());
    }

    #[test]
    fn strict_mode_requires_an_outbound_origin_allowlist() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_strict = std::env::var_os("AGENTOS_AUTH_STRICT");
        let previous_origins = std::env::var_os("MCP_OUTBOUND_ALLOWED_ORIGINS");
        std::env::remove_var("MCP_OUTBOUND_ALLOWED_ORIGINS");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        assert!(validate_strict_mcp_outbound_configuration().is_ok());
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        assert!(validate_strict_mcp_outbound_configuration().is_err());
        std::env::set_var("MCP_OUTBOUND_ALLOWED_ORIGINS", "");
        assert!(validate_strict_mcp_outbound_configuration().is_err());
        std::env::set_var("MCP_OUTBOUND_ALLOWED_ORIGINS", "https://mcp.example.test");
        assert!(validate_strict_mcp_outbound_configuration().is_ok());
        match previous_strict {
            Some(value) => std::env::set_var("AGENTOS_AUTH_STRICT", value),
            None => std::env::remove_var("AGENTOS_AUTH_STRICT"),
        }
        match previous_origins {
            Some(value) => std::env::set_var("MCP_OUTBOUND_ALLOWED_ORIGINS", value),
            None => std::env::remove_var("MCP_OUTBOUND_ALLOWED_ORIGINS"),
        }
    }

    #[tokio::test]
    async fn strict_mode_rejects_catalog_register_and_invoke_without_allowlist() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_strict = std::env::var_os("AGENTOS_AUTH_STRICT");
        let previous_origins = std::env::var_os("MCP_OUTBOUND_ALLOWED_ORIGINS");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        std::env::remove_var("MCP_OUTBOUND_ALLOWED_ORIGINS");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        let state = test_app_state(vec![json!({
            "id": "server-id",
            "name": "catalog-server",
            "endpoint": "http://127.0.0.1:8080/mcp",
            "endpoint_origin": "http://127.0.0.1:8080",
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"],
            "tenantId": "test-tenant",
            "projectId": "test-project",
        })]);
        let register = Router::new()
            .route("/servers", post(register_mcp_server_handler))
            .with_state(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::from(
                json!({
                    "name": "new-server",
                    "endpoint": "http://127.0.0.1:8081/mcp",
                    "protocol": "http",
                })
                .to_string(),
            ))
            .unwrap();
        assert_eq!(
            register.oneshot(request).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let invoke = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::from(
                json!({"server": "server-id", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        assert_eq!(
            invoke.oneshot(request).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        match previous_strict {
            Some(value) => std::env::set_var("AGENTOS_AUTH_STRICT", value),
            None => std::env::remove_var("AGENTOS_AUTH_STRICT"),
        }
        match previous_origins {
            Some(value) => std::env::set_var("MCP_OUTBOUND_ALLOWED_ORIGINS", value),
            None => std::env::remove_var("MCP_OUTBOUND_ALLOWED_ORIGINS"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    fn test_app_state(servers: Vec<Value>) -> Arc<AppState> {
        use crate::{
            core::core_types::{CoreConfig, SemanticCore},
            gateway::unified_gateway::UnifiedGateway,
            tools::prompt_registry::PromptRegistry,
        };

        let temp_dir = std::env::temp_dir().join(format!("mcp-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: temp_dir.join("l0").to_string_lossy().into_owned(),
                event_buffer_size: 10,
                enable_metrics: false,
                eviction_config: None,
            })
            .unwrap(),
        );
        let gateway = Arc::new(
            UnifiedGateway::new(&crate::config::GatewaySettings {
                base_url: "http://localhost".into(),
                api_key: String::new(),
                default_model: "test-model".into(),
                timeout_seconds: 30,
                max_retries: 1,
                retry_base_ms: 500,
                use_responses_api: false,
                model_mapping: std::collections::HashMap::new(),
            })
            .unwrap(),
        );
        Arc::new(AppState {
            core,
            gateway,
            kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
            config_info: Arc::new(tokio::sync::RwLock::new(json!({}))),
            agents_info: json!({}),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(servers)),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![])),
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
            api_usage: Arc::new(crate::api::http::api_gov::ApiUsageState::default()),
            online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
            online_corpus_queue_capacity: 1,
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    fn inbound_identity_token_with_roles(roles: Vec<&str>) -> String {
        encode(
            &Header::default(),
            &crate::api::http::iam::JwtClaims {
                sub: "test-user".into(),
                tenant_id: "test-tenant".into(),
                project_id: Some("test-project".into()),
                roles: roles.into_iter().map(str::to_owned).collect(),
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap()
    }

    fn raw_inbound_identity_token(claims: Value) -> String {
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap()
    }

    fn inbound_identity_token() -> String {
        inbound_identity_token_with_roles(vec![MCP_CATALOG_ADMIN_ROLE])
    }

    fn inbound_identity_token_with_project(project_id: Option<&str>) -> String {
        inbound_identity_token_with_claims("test-tenant", project_id)
    }

    fn inbound_identity_token_with_claims(tenant_id: &str, project_id: Option<&str>) -> String {
        encode(
            &Header::default(),
            &crate::api::http::iam::JwtClaims {
                sub: "test-user".into(),
                tenant_id: tenant_id.into(),
                project_id: project_id.map(str::to_owned),
                roles: vec!["DA".into()],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap()
    }

    fn test_isolation_claims() -> IsolationClaims {
        crate::api::http::iam::claims_identity(crate::api::http::iam::JwtClaims {
            sub: "test-actor".into(),
            tenant_id: "test-tenant".into(),
            project_id: Some("test-project".into()),
            roles: vec![],
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        })
        .unwrap()
        .isolation_claims()
        .unwrap()
        .clone()
    }

    #[tokio::test]
    async fn rejected_tools_do_not_mint_or_send_outbound_requests() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::remove_var("MCP_JWT_SECRET");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        async fn mock_handler(
            State(requests): State<Arc<AtomicUsize>>,
            Json(_body): Json<Value>,
        ) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let requests = Arc::new(AtomicUsize::new(0));
        let mock = Router::new()
            .route("/mcp", post(mock_handler))
            .with_state(requests.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let server = json!({
            "id": "server-id",
            "name": "catalog-server",
            "endpoint": format!("http://{address}/mcp"),
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"],
            "write_tools_enabled": false,
            "tenantId": "test-tenant",
            "projectId": "test-project",
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![server]));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token()),
            )
            .body(Body::from(
                json!({
                    "server": "catalog-server",
                    "tool_name": "delete_report",
                    "arguments": {"must_not_echo": "value"},
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({"error": "mcp_tool_not_allowed", "tool_name": "delete_report"})
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn allowed_tools_are_forwarded() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        async fn mock_handler(
            State(requests): State<Arc<AtomicUsize>>,
            Json(_body): Json<Value>,
        ) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let requests = Arc::new(AtomicUsize::new(0));
        let mock = Router::new()
            .route("/mcp", post(mock_handler))
            .with_state(requests.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let server = json!({
            "id": "server-id",
            "name": "catalog-server",
            "endpoint": format!("http://{address}/mcp"),
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"],
            "write_tools_enabled": false,
            "tenantId": "test-tenant",
            "projectId": "test-project",
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![server]));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token()),
            )
            .body(Body::from(
                json!({
                    "server": "catalog-server",
                    "tool_name": "read_status",
                    "arguments": {},
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"result": {"ok": true}}));
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn outbound_call_sends_minted_bearer_not_isolation_claims() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_subject = std::env::var_os("MCP_JWT_SUBJECT");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("MCP_JWT_SUBJECT", "catalog-mcp-signer");

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

        let isolation = test_isolation_claims();
        let bearer = mint_outbound_mcp_jwt("server-id", &isolation).unwrap();
        let result = invoke_http_mcp(
            &format!("http://{address}/mcp"),
            &bearer,
            "health_check",
            json!({}),
            None,
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
        assert_eq!(decoded.claims.sub, "catalog-mcp-signer");
        assert_ne!(decoded.claims.sub, isolation.actor_id());
        assert_eq!(decoded.claims.tenant_id, isolation.tenant_id());
        assert_eq!(decoded.claims.project_id, isolation.project_id());
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
            Some(value) => std::env::set_var("MCP_JWT_SUBJECT", value),
            None => std::env::remove_var("MCP_JWT_SUBJECT"),
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
            invoke_http_mcp(&endpoint, "test-token", "json_result", json!({}), None)
                .await
                .unwrap(),
            json!({"transport": "json"})
        );
        assert_eq!(
            invoke_http_mcp(&endpoint, "test-token", "sse_result", json!({}), None)
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

        let error = invoke_http_mcp(&endpoint, "test-token", "error_result", json!({}), None)
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

    #[tokio::test]
    async fn outbound_invoke_does_not_follow_redirects() {
        let redirected_requests = Arc::new(AtomicUsize::new(0));
        async fn redirected_handler(
            State(requests): State<Arc<AtomicUsize>>,
        ) -> axum::response::Response {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"unexpected": true}})).into_response()
        }

        let redirected = Router::new()
            .route("/mcp", post(redirected_handler))
            .with_state(redirected_requests.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirected_address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, redirected).await.unwrap() });

        async fn redirect_handler(State(location): State<String>) -> axum::response::Response {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(reqwest::header::LOCATION, location)],
            )
                .into_response()
        }

        let redirect = Router::new()
            .route("/mcp", post(redirect_handler))
            .with_state(format!("http://{redirected_address}/mcp"));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, redirect).await.unwrap() });

        let error = invoke_http_mcp(
            &format!("http://{redirect_address}/mcp"),
            "test-token",
            "read_status",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
        let InvokeHttpMcpError::Transport(error) = error else {
            panic!("redirect must be a transport failure");
        };
        assert!(error.contains("redirect status 307"));
        assert_eq!(redirected_requests.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn outbound_jwt_audience_is_bound_to_each_catalog_server() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");

        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        let first = decode::<OutboundMcpJwtClaims>(
            &mint_outbound_mcp_jwt("catalog-server-a", &test_isolation_claims()).unwrap(),
            &DecodingKey::from_secret(b"outbound-mcp-test-secret"),
            &validation,
        )
        .unwrap();
        let second = decode::<OutboundMcpJwtClaims>(
            &mint_outbound_mcp_jwt("catalog-server-b", &test_isolation_claims()).unwrap(),
            &DecodingKey::from_secret(b"outbound-mcp-test-secret"),
            &validation,
        )
        .unwrap();
        assert_eq!(first.claims.aud, "catalog-server-a");
        assert_eq!(second.claims.aud, "catalog-server-b");
        assert_ne!(first.claims.aud, second.claims.aud);
        let mut verifier = Validation::new(Algorithm::HS256);
        verifier.set_audience(&["catalog-server-b"]);
        assert!(decode::<OutboundMcpJwtClaims>(
            &mint_outbound_mcp_jwt("catalog-server-a", &test_isolation_claims()).unwrap(),
            &DecodingKey::from_secret(b"outbound-mcp-test-secret"),
            &verifier,
        )
        .is_err());

        let previous_audience = std::env::var_os("TEST_MCP_AUDIENCE");
        std::env::set_var("TEST_MCP_AUDIENCE", "configured-server-audience");
        let configured_server = json!({
            "id": "catalog-server",
            "audience_env": "TEST_MCP_AUDIENCE",
        });
        assert_eq!(
            outbound_mcp_audience(&configured_server).unwrap(),
            "configured-server-audience"
        );
        std::env::remove_var("TEST_MCP_AUDIENCE");
        assert!(outbound_mcp_audience(&configured_server).is_err());
        match previous_audience {
            Some(value) => std::env::set_var("TEST_MCP_AUDIENCE", value),
            None => std::env::remove_var("TEST_MCP_AUDIENCE"),
        }

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
    }

    #[tokio::test]
    async fn tampered_catalog_endpoint_is_rejected_before_outbound_request() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_strict = std::env::var_os("AGENTOS_AUTH_STRICT");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let mock = Router::new()
            .route("/mcp", post(mock_handler))
            .with_state(requests.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "server-id",
                "name": "catalog-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": "http://127.0.0.1:1",
                "protocol": "http",
                "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"],
                "write_tools_enabled": false,
                "tenantId": "test-tenant",
                "projectId": "test-project",
            })]));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token()),
            )
            .body(Body::from(
                json!({"server": "server-id", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        match previous_strict {
            Some(value) => std::env::set_var("AGENTOS_AUTH_STRICT", value),
            None => std::env::remove_var("AGENTOS_AUTH_STRICT"),
        }
    }

    #[tokio::test]
    async fn outbound_invoke_requires_explicit_inbound_scope_claims() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }
        let requests = Arc::new(AtomicUsize::new(0));
        let mock = Router::new()
            .route("/mcp", post(mock_handler))
            .with_state(requests.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let server = json!({
            "id": "server-id",
            "name": "catalog-server",
            "endpoint": format!("http://{address}/mcp"),
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"],
            "tenantId": "test-tenant",
            "projectId": "default",
        });

        let invoke = |token: String| {
            let app = Router::new()
                .route("/invoke", post(invoke_mcp_server_handler))
                .with_state(test_app_state(vec![server.clone()]));
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/invoke")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    json!({"server": "server-id", "tool_name": "read_status", "arguments": {}})
                        .to_string(),
                ))
                .unwrap();
            async move { app.oneshot(request).await.unwrap() }
        };

        let without_project = raw_inbound_identity_token(json!({
            "sub": "test-user",
            "tenant_id": "test-tenant",
            "roles": [],
            "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        }));
        assert_eq!(
            invoke(without_project).await.status(),
            StatusCode::FORBIDDEN
        );
        let without_tenant = raw_inbound_identity_token(json!({
            "sub": "test-user",
            "project_id": "default",
            "roles": [],
            "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        }));
        assert_eq!(
            invoke(without_tenant).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let empty_project = raw_inbound_identity_token(json!({
            "sub": "test-user",
            "tenant_id": "test-tenant",
            "project_id": "",
            "roles": [],
            "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        }));
        assert_eq!(invoke(empty_project).await.status(), StatusCode::FORBIDDEN);
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        let explicit_default = raw_inbound_identity_token(json!({
            "sub": "test-user",
            "tenant_id": "test-tenant",
            "project_id": "default",
            "roles": [],
            "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        }));
        assert_eq!(invoke(explicit_default).await.status(), StatusCode::OK);
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn outbound_response_limit_and_timeout_fail_explicitly() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_timeout = std::env::var_os("MCP_OUTBOUND_TIMEOUT_MS");
        let previous_limit = std::env::var_os("MCP_OUTBOUND_MAX_RESPONSE_BYTES");
        std::env::set_var("MCP_OUTBOUND_TIMEOUT_MS", "25");
        std::env::set_var("MCP_OUTBOUND_MAX_RESPONSE_BYTES", "32");

        async fn mock_handler(Json(body): Json<Value>) -> axum::response::Response {
            match body["params"]["name"].as_str() {
                Some("slow") => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})).into_response()
                }
                _ => Json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {"payload": "this response is intentionally too large"},
                }))
                .into_response(),
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp", post(mock_handler)))
                .await
                .unwrap()
        });
        let endpoint = format!("http://{address}/mcp");
        let oversized = invoke_http_mcp(&endpoint, "test-token", "large", json!({}), None)
            .await
            .unwrap_err();
        let timed_out = invoke_http_mcp(&endpoint, "test-token", "slow", json!({}), None)
            .await
            .unwrap_err();
        let InvokeHttpMcpError::Transport(oversized) = oversized else {
            panic!("oversized response must be a transport error");
        };
        let InvokeHttpMcpError::Transport(timed_out) = timed_out else {
            panic!("timeout must be a transport error");
        };
        assert!(oversized.contains("exceeded 32 byte limit"));
        assert_eq!(timed_out, "MCP HTTP request timed out");

        match previous_timeout {
            Some(value) => std::env::set_var("MCP_OUTBOUND_TIMEOUT_MS", value),
            None => std::env::remove_var("MCP_OUTBOUND_TIMEOUT_MS"),
        }
        match previous_limit {
            Some(value) => std::env::set_var("MCP_OUTBOUND_MAX_RESPONSE_BYTES", value),
            None => std::env::remove_var("MCP_OUTBOUND_MAX_RESPONSE_BYTES"),
        }
    }

    #[tokio::test]
    async fn non_administrator_cannot_register_or_persist_catalog_entry() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let data_dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", data_dir.path());
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        let state = test_app_state(vec![]);
        let app = Router::new()
            .route("/servers", post(register_mcp_server_handler))
            .with_state(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token_with_roles(vec!["DA"])),
            )
            .body(Body::from(
                json!({
                    "name": "catalog-server",
                    "endpoint": "http://127.0.0.1:8080/mcp",
                    "protocol": "http",
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.mcp_servers.read().await.is_empty());
        assert!(!data_dir.path().join("mcp_servers.json").exists());
        let app = Router::new()
            .route(
                "/servers/:id",
                axum::routing::delete(delete_mcp_server_handler),
            )
            .with_state(state);
        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/servers/server-id")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token_with_roles(vec!["DA"])),
            )
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        match previous_data_dir {
            Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
            None => std::env::remove_var("AGENTOS_DATA_DIR"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn slow_invoke_releases_catalog_lock_before_outbound_io() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        let data_dir = tempfile::tempdir().unwrap();
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::set_var("AGENTOS_DATA_DIR", data_dir.path());

        let requests = Arc::new(AtomicUsize::new(0));
        async fn slow_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(3)).await;
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let mock = Router::new()
            .route("/mcp", post(slow_handler))
            .with_state(requests.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let state = test_app_state(vec![
            json!({
                "id": "slow-server",
                "name": "slow-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http",
                "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"],
                "write_tools_enabled": false,
                "tenantId": "test-tenant",
                "projectId": "test-project",
            }),
            json!({
                "id": "delete-server",
                "name": "delete-server",
                "endpoint": "http://127.0.0.1:8081/mcp",
                "endpoint_origin": "http://127.0.0.1:8081",
                "protocol": "http",
                "tenantId": "test-tenant",
                "projectId": "test-project",
            }),
        ]);
        let invoke = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token()),
            )
            .body(Body::from(
                json!({"server": "slow-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        let invoke_task = tokio::spawn(invoke.oneshot(request));

        tokio::time::timeout(Duration::from_millis(200), async {
            while requests.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("slow endpoint did not receive the invoke");
        let register = Router::new()
            .route("/servers", post(register_mcp_server_handler))
            .with_state(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::from(
                json!({
                    "name": "new-server",
                    "endpoint": "http://127.0.0.1:8080/mcp",
                    "protocol": "http",
                })
                .to_string(),
            ))
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), register.oneshot(request))
            .await
            .expect("registration was blocked by a slow outbound invoke")
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let delete = Router::new()
            .route(
                "/servers/:id",
                axum::routing::delete(delete_mcp_server_handler),
            )
            .with_state(state);
        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/servers/delete-server")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::empty())
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), delete.oneshot(request))
            .await
            .expect("deletion was blocked by a slow outbound invoke")
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        invoke_task.abort();

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
        match previous_data_dir {
            Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
            None => std::env::remove_var("AGENTOS_DATA_DIR"),
        }
    }

    #[tokio::test]
    async fn audience_isolated_sidecars_reject_a_token_for_another_server() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_audience = std::env::var_os("MCP_TEST_AUDIENCE_A");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("MCP_TEST_AUDIENCE_A", "sidecar-a");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        async fn audience_handler(
            State(expected_audience): State<&'static str>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            let Some(token) = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
            else {
                return StatusCode::UNAUTHORIZED.into_response();
            };
            let mut validation = Validation::new(Algorithm::HS256);
            validation.set_audience(&[expected_audience]);
            if decode::<OutboundMcpJwtClaims>(
                token,
                &DecodingKey::from_secret(b"outbound-mcp-test-secret"),
                &validation,
            )
            .is_err()
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"jsonrpc": "2.0", "id": body["id"], "result": {}})),
                )
                    .into_response();
            }
            Json(json!({"jsonrpc": "2.0", "id": body["id"], "result": {"ok": true}}))
                .into_response()
        }

        async fn start_audience_sidecar(audience: &'static str) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new()
                .route("/mcp", post(audience_handler))
                .with_state(audience);
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://{address}/mcp")
        }

        let sidecar_a = start_audience_sidecar("sidecar-a").await;
        let sidecar_b = start_audience_sidecar("sidecar-b").await;
        let server_a = json!({
            "id": "catalog-a",
            "name": "catalog-a",
            "endpoint": sidecar_a,
            "endpoint_origin": endpoint_origin(&sidecar_a).unwrap(),
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "audience_env": "MCP_TEST_AUDIENCE_A",
            "allowed_tools": ["read_status"],
            "write_tools_enabled": false,
            "tenantId": "test-tenant",
            "projectId": "test-project",
        });
        let audience = outbound_mcp_audience(&server_a).unwrap();
        assert_eq!(audience, "sidecar-a");
        let bearer = mint_outbound_mcp_jwt(&audience, &test_isolation_claims()).unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        assert_eq!(
            decode::<OutboundMcpJwtClaims>(
                &bearer,
                &DecodingKey::from_secret(b"outbound-mcp-test-secret"),
                &validation,
            )
            .unwrap()
            .claims
            .aud,
            "sidecar-a"
        );
        assert!(
            invoke_http_mcp(&sidecar_a, &bearer, "read_status", json!({}), None)
                .await
                .is_ok()
        );
        let cross_audience = invoke_http_mcp(&sidecar_b, &bearer, "read_status", json!({}), None)
            .await
            .unwrap_err();
        assert!(
            matches!(cross_audience, InvokeHttpMcpError::Transport(message) if message.contains("401"))
        );

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_audience {
            Some(value) => std::env::set_var("MCP_TEST_AUDIENCE_A", value),
            None => std::env::remove_var("MCP_TEST_AUDIENCE_A"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn missing_or_empty_audience_environment_fails_closed_before_request() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_audience = std::env::var_os("MCP_TEST_REQUIRED_AUDIENCE");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let requests = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests_for_server = requests.clone();
        let endpoint = format!("http://{address}/mcp");
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(requests_for_server),
            )
            .await
            .unwrap()
        });
        let server = json!({
            "id": "catalog-server",
            "name": "catalog-server",
            "endpoint": endpoint,
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "audience_env": "MCP_TEST_REQUIRED_AUDIENCE",
            "allowed_tools": ["read_status"],
            "write_tools_enabled": false,
            "tenantId": "test-tenant",
            "projectId": "test-project",
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![server]));
        for value in [None, Some("")] {
            match value {
                Some(value) => std::env::set_var("MCP_TEST_REQUIRED_AUDIENCE", value),
                None => std::env::remove_var("MCP_TEST_REQUIRED_AUDIENCE"),
            }
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/invoke")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", inbound_identity_token()))
                .body(Body::from(
                    json!({"server": "catalog-server", "tool_name": "read_status", "arguments": {}})
                        .to_string(),
                ))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(requests.load(Ordering::SeqCst), 0);
        }

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_audience {
            Some(value) => std::env::set_var("MCP_TEST_REQUIRED_AUDIENCE", value),
            None => std::env::remove_var("MCP_TEST_REQUIRED_AUDIENCE"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[test]
    fn outbound_jwt_subject_defaults_and_is_overridable() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_subject = std::env::var_os("MCP_JWT_SUBJECT");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::remove_var("MCP_JWT_SUBJECT");
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        let decoding_key = DecodingKey::from_secret(b"outbound-mcp-test-secret");
        assert_eq!(
            decode::<OutboundMcpJwtClaims>(
                &mint_outbound_mcp_jwt("catalog-server", &test_isolation_claims()).unwrap(),
                &decoding_key,
                &validation,
            )
            .unwrap()
            .claims
            .sub,
            "wao-core"
        );
        std::env::set_var("MCP_JWT_SUBJECT", "custom-mcp-subject");
        assert_eq!(
            decode::<OutboundMcpJwtClaims>(
                &mint_outbound_mcp_jwt("catalog-server", &test_isolation_claims()).unwrap(),
                &decoding_key,
                &validation,
            )
            .unwrap()
            .claims
            .sub,
            "custom-mcp-subject"
        );

        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_subject {
            Some(value) => std::env::set_var("MCP_JWT_SUBJECT", value),
            None => std::env::remove_var("MCP_JWT_SUBJECT"),
        }
    }

    #[tokio::test]
    async fn oversized_json_response_returns_a_clear_bounded_error() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_limit = std::env::var_os("MCP_OUTBOUND_MAX_RESPONSE_BYTES");
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("MCP_OUTBOUND_MAX_RESPONSE_BYTES", "64");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");

        async fn large_response(Json(body): Json<Value>) -> Json<Value> {
            Json(
                json!({"jsonrpc": "2.0", "id": body["id"], "result": {"payload": "x".repeat(4096)}}),
            )
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp", post(large_response)))
                .await
                .unwrap()
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "large-server", "name": "large-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http", "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"], "write_tools_enabled": false,
                "tenantId": "test-tenant", "projectId": "test-project",
            })]));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token()),
            )
            .body(Body::from(
                json!({"server": "large-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("exceeded 64 byte limit"));

        match previous_limit {
            Some(value) => std::env::set_var("MCP_OUTBOUND_MAX_RESPONSE_BYTES", value),
            None => std::env::remove_var("MCP_OUTBOUND_MAX_RESPONSE_BYTES"),
        }
        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn da_service_token_cannot_mutate_mcp_catalog() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let data_dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", data_dir.path());
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let state = test_app_state(vec![json!({
            "id": "catalog-server",
            "name": "catalog-server",
            "endpoint": "http://127.0.0.1:8080/mcp",
            "endpoint_origin": "http://127.0.0.1:8080",
            "protocol": "http",
            "tenantId": "test-tenant",
            "projectId": "test-project",
        })]);
        let register = Router::new()
            .route("/servers", post(register_mcp_server_handler))
            .with_state(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", inbound_identity_token_with_roles(vec!["DA"])))
            .body(Body::from(
                json!({"name": "catalog-server", "endpoint": "http://127.0.0.1:8080/mcp", "protocol": "http"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            register.oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let delete = Router::new()
            .route(
                "/servers/:id",
                axum::routing::delete(delete_mcp_server_handler),
            )
            .with_state(state);
        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/servers/catalog-server")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token_with_roles(vec!["DA"])),
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            delete.oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        match previous_data_dir {
            Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
            None => std::env::remove_var("AGENTOS_DATA_DIR"),
        }
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn redirect_to_non_allowlisted_origin_is_not_followed() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = [
            "MCP_JWT_SECRET",
            "MCP_OUTBOUND_ALLOWED_ORIGINS",
            "AGENTOS_AUTH_MODE",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let redirected_requests = Arc::new(AtomicUsize::new(0));
        async fn destination(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }
        let destination_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_address = destination_listener.local_addr().unwrap();
        let redirected_requests_for_server = redirected_requests.clone();
        tokio::spawn(async move {
            axum::serve(
                destination_listener,
                Router::new()
                    .route("/mcp", post(destination))
                    .with_state(redirected_requests_for_server),
            )
            .await
            .unwrap()
        });
        let redirect_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_address = redirect_listener.local_addr().unwrap();
        let location = format!("http://{destination_address}/mcp");
        tokio::spawn(async move {
            axum::serve(
                redirect_listener,
                Router::new().route(
                    "/mcp",
                    post(move || {
                        let location = location.clone();
                        async move {
                            (
                                StatusCode::TEMPORARY_REDIRECT,
                                [(axum::http::header::LOCATION, location)],
                            )
                        }
                    }),
                ),
            )
            .await
            .unwrap()
        });
        std::env::set_var(
            "MCP_OUTBOUND_ALLOWED_ORIGINS",
            format!("http://{redirect_address}"),
        );
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "redirect-server", "name": "redirect-server",
                "endpoint": format!("http://{redirect_address}/mcp"),
                "endpoint_origin": format!("http://{redirect_address}"),
                "protocol": "http", "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"], "write_tools_enabled": false,
                "tenantId": "test-tenant", "projectId": "test-project",
            })]));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", inbound_identity_token()),
            )
            .body(Body::from(
                json!({"server": "redirect-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(redirected_requests.load(Ordering::SeqCst), 0);
        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[tokio::test]
    async fn local_development_allows_unset_outbound_origin_allowlist() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = [
            "AGENTOS_AUTH_STRICT",
            "MCP_OUTBOUND_ALLOWED_ORIGINS",
            "MCP_JWT_SECRET",
            "AGENTOS_AUTH_MODE",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        let data_dir = tempfile::tempdir().unwrap();
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        std::env::remove_var("MCP_OUTBOUND_ALLOWED_ORIGINS");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::set_var("AGENTOS_DATA_DIR", data_dir.path());
        assert_eq!(configured_outbound_mcp_origins().unwrap(), None);

        async fn mock_handler() -> Json<Value> {
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp", post(mock_handler)))
                .await
                .unwrap()
        });
        let state = test_app_state(vec![]);
        let register = Router::new()
            .route("/servers", post(register_mcp_server_handler))
            .with_state(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::from(
                json!({
                    "name": "local-server",
                    "endpoint": format!("http://{address}/mcp"),
                    "protocol": "http",
                    "auth_kind": "bearer_jwt",
                    "allowed_tools": ["read_status"],
                })
                .to_string(),
            ))
            .unwrap();
        assert_eq!(
            register.oneshot(request).await.unwrap().status(),
            StatusCode::CREATED
        );
        let invoke = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::from(
                json!({"server": "local-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        assert_eq!(
            invoke.oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );

        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        match previous_data_dir {
            Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
            None => std::env::remove_var("AGENTOS_DATA_DIR"),
        }
    }

    #[tokio::test]
    async fn strict_mode_requires_allowlist_and_refuses_catalog_operations_without_it() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = [
            "AGENTOS_AUTH_STRICT",
            "MCP_OUTBOUND_ALLOWED_ORIGINS",
            "MCP_JWT_SECRET",
            "AGENTOS_AUTH_MODE",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        let data_dir = tempfile::tempdir().unwrap();
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        std::env::remove_var("MCP_OUTBOUND_ALLOWED_ORIGINS");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::set_var("AGENTOS_DATA_DIR", data_dir.path());
        assert_eq!(
            validate_strict_mcp_outbound_configuration(),
            Err("MCP_OUTBOUND_ALLOWED_ORIGINS must be configured when AGENTOS_AUTH_STRICT=true")
        );

        async fn mock_handler() -> Json<Value> {
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp", post(mock_handler)))
                .await
                .unwrap()
        });
        let server = json!({
            "id": "strict-server", "name": "strict-server",
            "endpoint": format!("http://{address}/mcp"),
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http", "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"], "write_tools_enabled": false,
            "tenantId": "test-tenant", "projectId": "test-project",
        });
        let register = Router::new()
            .route("/servers", post(register_mcp_server_handler))
            .with_state(test_app_state(vec![]));
        let register_request = axum::http::Request::builder()
            .method("POST")
            .uri("/servers")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::from(
                json!({
                    "name": "strict-server",
                    "endpoint": format!("http://{address}/mcp"),
                    "protocol": "http",
                })
                .to_string(),
            ))
            .unwrap();
        let invoke = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![server]));
        let invoke_request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::from(
                json!({"server": "strict-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        let delete = Router::new()
            .route(
                "/servers/:id",
                axum::routing::delete(delete_mcp_server_handler),
            )
            .with_state(test_app_state(vec![json!({
                "id": "strict-server",
                "name": "strict-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http",
                "tenantId": "test-tenant",
                "projectId": "test-project",
            })]));
        let delete_request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/servers/strict-server")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    inbound_identity_token_with_roles(vec!["mcp_admin"])
                ),
            )
            .body(Body::empty())
            .unwrap();
        let register_status = register.oneshot(register_request).await.unwrap().status();
        let invoke_status = invoke.oneshot(invoke_request).await.unwrap().status();
        let delete_status = delete.oneshot(delete_request).await.unwrap().status();
        assert_eq!(register_status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(invoke_status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(delete_status, StatusCode::SERVICE_UNAVAILABLE);

        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        match previous_data_dir {
            Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
            None => std::env::remove_var("AGENTOS_DATA_DIR"),
        }
    }

    #[tokio::test]
    // #257 gap: IAM normalizes a missing project_id to "default", allowing minting and outbound I/O.
    async fn missing_inbound_project_id_does_not_mint_or_call_sidecar() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = [
            "MCP_JWT_SECRET",
            "AGENTOS_AUTH_MODE",
            "AGENTOS_AUTH_STRICT",
            "MCP_OUTBOUND_ALLOWED_ORIGINS",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        std::env::remove_var("MCP_OUTBOUND_ALLOWED_ORIGINS");

        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let requests = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(requests_for_server),
            )
            .await
            .unwrap()
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "default-project-server",
                "name": "default-project-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http",
                "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"],
                "write_tools_enabled": false,
                "tenantId": "test-tenant",
                "projectId": "default",
            })]));
        let token = encode(
            &Header::default(),
            &crate::api::http::iam::JwtClaims {
                sub: "test-user".into(),
                tenant_id: "test-tenant".into(),
                project_id: None,
                roles: vec!["DA".into()],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap();
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                json!({
                    "server": "default-project-server",
                    "tool_name": "read_status",
                    "arguments": {},
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("mcp_claims_incomplete"));
        assert!(body.contains("project_id"));
        assert!(!body.contains(&token));
        assert!(!body.contains("test-tenant"));
        assert!(!body.contains("default"));
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[tokio::test]
    async fn defaulted_project_jwt_provenance_is_rejected_by_mcp_invoke() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let claims = crate::api::http::iam::claims_identity(crate::api::http::iam::JwtClaims {
            sub: "test-user".into(),
            tenant_id: "test-tenant".into(),
            project_id: None,
            roles: vec!["DA".into()],
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        })
        .expect("verified JWT claims should produce an identity");
        assert_eq!(
            format!("{:?}", claims.isolation_claims().unwrap().provenance()),
            "VerifiedDefaulted"
        );

        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sidecar_requests = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(sidecar_requests),
            )
            .await
            .unwrap()
        });
        let response = invoke_mcp_server_handler(
            State(test_app_state(vec![json!({
                "id": "defaulted-project-server", "name": "defaulted-project-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http", "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"], "write_tools_enabled": false,
                "tenantId": "test-tenant", "projectId": "default",
            })])),
            claims,
            Json(McpCatalogInvokeRequest {
                server: "defaulted-project-server".into(),
                tool_name: "read_status".into(),
                arguments: json!({}),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("mcp_claims_incomplete"));
        assert!(body.contains("\"missing_field\":\"project_id\""));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_inbound_tenant_id_is_rejected_by_iam_before_sidecar() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(requests_for_server),
            )
            .await
            .unwrap()
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "tenant-server",
                "name": "tenant-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http",
                "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"],
                "write_tools_enabled": false,
                "tenantId": "test-tenant",
                "projectId": "test-project",
            })]));
        let token = encode(
            &Header::default(),
            &json!({
                "sub": "test-user",
                "project_id": "test-project",
                "roles": ["DA"],
                "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
            }),
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap();
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                json!({"server": "tenant-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn empty_inbound_tenant_id_is_rejected_by_iam_before_sidecar() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(requests_for_server),
            )
            .await
            .unwrap()
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "tenant-server",
                "name": "tenant-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http",
                "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"],
                "write_tools_enabled": false,
                "tenantId": "test-tenant",
                "projectId": "test-project",
            })]));
        let token = inbound_identity_token_with_claims("", Some("test-project"));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                json!({"server": "tenant-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        match previous_auth_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    // #257 gap: IAM normalizes an empty project_id to "default", allowing minting and outbound I/O.
    async fn empty_inbound_project_id_does_not_mint_or_call_sidecar() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = ["MCP_JWT_SECRET", "AGENTOS_AUTH_MODE", "AGENTOS_AUTH_STRICT"]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(requests_for_server),
            )
            .await
            .unwrap()
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "default-project-server",
                "name": "default-project-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http",
                "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"],
                "write_tools_enabled": false,
                "tenantId": "test-tenant",
                "projectId": "default",
            })]));
        let token = inbound_identity_token_with_claims("test-tenant", Some(""));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                json!({
                    "server": "default-project-server",
                    "tool_name": "read_status",
                    "arguments": {},
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("mcp_claims_incomplete"));
        assert!(body.contains("project_id"));
        assert!(!body.contains(&token));
        assert!(!body.contains("test-tenant"));
        assert!(!body.contains("default"));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    // #257 gap: from_verified cannot record that a default project originated outside a JWT claim.
    fn default_project_from_verified_does_not_mint() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_secret = std::env::var_os("MCP_JWT_SECRET");
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        let claims = IsolationClaims::from_verified("test-tenant", "default", "internal").unwrap();
        assert!(mint_outbound_mcp_jwt("catalog-server", &claims).is_err());
        match previous_secret {
            Some(value) => std::env::set_var("MCP_JWT_SECRET", value),
            None => std::env::remove_var("MCP_JWT_SECRET"),
        }
    }

    #[tokio::test]
    // #257 gap: watcher-built deploy-config claims cannot yet carry their non-token provenance.
    async fn watcher_style_default_claims_are_refused_before_mcp_outbound() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = ["MCP_JWT_SECRET", "AGENTOS_AUTH_STRICT"]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(requests_for_server),
            )
            .await
            .unwrap()
        });
        let state = test_app_state(vec![json!({
            "id": "watcher-server",
            "name": "watcher-server",
            "endpoint": format!("http://{address}/mcp"),
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"],
            "write_tools_enabled": false,
            "tenantId": "watcher-tenant",
            "projectId": "default",
        })]);
        let watcher_claims =
            IsolationClaims::from_verified("watcher-tenant", "default", "watcher-service").unwrap();
        let identity = crate::api::http::iam::test_identity_from_verified_claims(
            watcher_claims,
            vec!["DA".into()],
        );
        let response = invoke_mcp_server_handler(
            State(state),
            identity,
            Json(McpCatalogInvokeRequest {
                server: "watcher-server".into(),
                tool_name: "read_status".into(),
                arguments: json!({}),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("mcp_claims_unverified"));
        assert!(!body.contains("watcher-tenant"));
        assert!(!body.contains("default"));
        assert!(!body.contains("outbound-mcp-test-secret"));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[tokio::test]
    async fn missing_tenant_jwt_cannot_invoke_mcp_or_read_default_tenant_kb() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = ["MCP_JWT_SECRET", "AGENTOS_AUTH_MODE", "AGENTOS_AUTH_STRICT"]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(requests_for_server),
            )
            .await
            .unwrap()
        });
        let state = test_app_state(vec![json!({
            "id": "missing-tenant-server",
            "name": "missing-tenant-server",
            "endpoint": format!("http://{address}/mcp"),
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http",
            "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"],
            "write_tools_enabled": false,
            "tenantId": "default",
            "projectId": "test-project",
        })]);
        state.knowledge_bases.write().await.push(json!({
            "id": "default-tenant-kb",
            "name": "Default tenant record",
            "tenant_id": "default",
            "project_id": "test-project",
        }));
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .route(
                "/kb",
                axum::routing::get(crate::api::http::kb::list_knowledge_bases_handler),
            )
            .with_state(state);
        let token = encode(
            &Header::default(),
            &json!({
                "sub": "test-user",
                "project_id": "test-project",
                "roles": ["DA"],
                "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
            }),
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap();
        let invoke = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                json!({"server": "missing-tenant-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ))
            .unwrap();
        let invoke_response = app.clone().oneshot(invoke).await.unwrap();
        assert_eq!(invoke_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        let kb = axum::http::Request::builder()
            .method("GET")
            .uri("/kb")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let kb_response = app.oneshot(kb).await.unwrap();
        assert_eq!(kb_response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(kb_response.into_body(), 1024)
            .await
            .unwrap();
        assert!(!std::str::from_utf8(&body)
            .unwrap()
            .contains("default-tenant-kb"));
        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[tokio::test]
    async fn missing_subject_jwt_cannot_invoke_mcp_or_read_default_tenant_kb() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sidecar_requests = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(sidecar_requests),
            )
            .await
            .unwrap()
        });
        let state = test_app_state(vec![json!({
            "id": "subject-server", "name": "subject-server",
            "endpoint": format!("http://{address}/mcp"),
            "endpoint_origin": format!("http://{address}"),
            "protocol": "http", "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"], "write_tools_enabled": false,
            "tenantId": "default", "projectId": "test-project",
        })]);
        state.knowledge_bases.write().await.push(json!({
            "id": "default-subject-kb", "tenant_id": "default", "project_id": "test-project",
        }));
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .route(
                "/kb",
                axum::routing::get(crate::api::http::kb::list_knowledge_bases_handler),
            )
            .with_state(state);
        let token = encode(
            &Header::default(),
            &json!({
                "tenant_id": "default", "project_id": "test-project", "roles": ["DA"],
                "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
            }),
            &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
        )
        .unwrap();
        for (method, uri, body) in [
            (
                "POST",
                "/invoke",
                json!({"server": "subject-server", "tool_name": "read_status", "arguments": {}})
                    .to_string(),
            ),
            ("GET", "/kb", String::new()),
        ] {
            let request = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert!(!std::str::from_utf8(&body)
                .unwrap()
                .contains("default-subject-kb"));
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        match previous_mode {
            Some(value) => std::env::set_var("AGENTOS_AUTH_MODE", value),
            None => std::env::remove_var("AGENTOS_AUTH_MODE"),
        }
    }

    #[tokio::test]
    async fn oidc_wrong_issuer_or_audience_is_rejected_by_mcp_and_kb() {
        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = [
            "AGENTOS_AUTH_MODE",
            "AGENTOS_OIDC_JWKS_URL",
            "AGENTOS_OIDC_ISSUER",
            "AGENTOS_OIDC_AUDIENCE",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        let jwks_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let jwks_address = jwks_listener.local_addr().unwrap();
        let jwks = json!({"keys": [{
            "kty": "RSA", "kid": "test-rsa", "use": "sig", "alg": "RS256",
            "n": crate::api::http::iam::tests::TEST_RSA_N, "e": "AQAB"
        }]});
        tokio::spawn(async move {
            axum::serve(
                jwks_listener,
                Router::new().route(
                    "/jwks",
                    axum::routing::get(move || {
                        let jwks = jwks.clone();
                        async move { Json(jwks) }
                    }),
                ),
            )
            .await
            .unwrap()
        });
        std::env::set_var("AGENTOS_AUTH_MODE", "oidc");
        std::env::set_var(
            "AGENTOS_OIDC_JWKS_URL",
            format!("http://{jwks_address}/jwks"),
        );
        std::env::set_var("AGENTOS_OIDC_ISSUER", "https://issuer.example.test");
        std::env::set_var("AGENTOS_OIDC_AUDIENCE", "wild-agent-os");

        let requests = Arc::new(AtomicUsize::new(0));
        async fn mock_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
            requests.fetch_add(1, Ordering::SeqCst);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sidecar_requests = requests.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(sidecar_requests),
            )
            .await
            .unwrap()
        });
        let state = test_app_state(vec![json!({
            "id": "oidc-server", "name": "oidc-server",
            "endpoint": format!("http://{address}/mcp"), "endpoint_origin": format!("http://{address}"),
            "protocol": "http", "auth": {"kind": "bearer_jwt"},
            "allowed_tools": ["read_status"], "write_tools_enabled": false,
            "tenantId": "default", "projectId": "default",
        })]);
        state.knowledge_bases.write().await.push(json!({
            "id": "default-oidc-kb", "tenant_id": "default", "project_id": "default",
        }));
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .route(
                "/kb",
                axum::routing::get(crate::api::http::kb::list_knowledge_bases_handler),
            )
            .with_state(state);
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-rsa".into());
        for claims in [
            json!({"sub":"test","tenant_id":"default","project_id":"default","roles":["DA"],"iss":"https://wrong.example.test","aud":"wild-agent-os","exp":(chrono::Utc::now()+chrono::Duration::hours(1)).timestamp()}),
            json!({"sub":"test","tenant_id":"default","project_id":"default","roles":["DA"],"iss":"https://issuer.example.test","aud":"wrong-audience","exp":(chrono::Utc::now()+chrono::Duration::hours(1)).timestamp()}),
            json!({"sub":"test","tenant_id":"default","project_id":"default","roles":["DA"],"iss":"https://issuer.example.test","exp":(chrono::Utc::now()+chrono::Duration::hours(1)).timestamp()}),
        ] {
            let token = encode(
                &header,
                &claims,
                &EncodingKey::from_rsa_pem(
                    crate::api::http::iam::tests::TEST_RSA_PRIVATE_KEY.as_bytes(),
                )
                .unwrap(),
            )
            .unwrap();
            for (method, uri, body) in [
                (
                    "POST",
                    "/invoke",
                    json!({"server":"oidc-server","tool_name":"read_status","arguments":{}})
                        .to_string(),
                ),
                ("GET", "/kb", String::new()),
            ] {
                let request = axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert!(!std::str::from_utf8(&body)
                    .unwrap()
                    .contains("default-oidc-kb"));
            }
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[tokio::test]
    async fn explicit_default_project_id_mints_and_reaches_sidecar() {
        struct Seen {
            requests: AtomicUsize,
            project_id: std::sync::Mutex<Option<String>>,
        }

        let _guard = crate::api::http::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = ["MCP_JWT_SECRET", "AGENTOS_AUTH_MODE", "AGENTOS_AUTH_STRICT"]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        std::env::set_var("MCP_JWT_SECRET", "outbound-mcp-test-secret");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        async fn mock_handler(State(seen): State<Arc<Seen>>, headers: HeaderMap) -> Json<Value> {
            seen.requests.fetch_add(1, Ordering::SeqCst);
            let token = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap();
            let mut validation = Validation::new(Algorithm::HS256);
            validation.validate_aud = false;
            let claims = decode::<OutboundMcpJwtClaims>(
                token,
                &DecodingKey::from_secret(b"outbound-mcp-test-secret"),
                &validation,
            )
            .unwrap()
            .claims;
            *seen.project_id.lock().unwrap() = Some(claims.project_id);
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        }

        let seen = Arc::new(Seen {
            requests: AtomicUsize::new(0),
            project_id: std::sync::Mutex::new(None),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen_for_server = seen.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/mcp", post(mock_handler))
                    .with_state(seen_for_server),
            )
            .await
            .unwrap()
        });
        let app = Router::new()
            .route("/invoke", post(invoke_mcp_server_handler))
            .with_state(test_app_state(vec![json!({
                "id": "explicit-default-server",
                "name": "explicit-default-server",
                "endpoint": format!("http://{address}/mcp"),
                "endpoint_origin": format!("http://{address}"),
                "protocol": "http",
                "auth": {"kind": "bearer_jwt"},
                "allowed_tools": ["read_status"],
                "write_tools_enabled": false,
                "tenantId": "test-tenant",
                "projectId": "default",
            })]));
        let token = inbound_identity_token_with_project(Some("default"));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                json!({
                    "server": "explicit-default-server",
                    "tool_name": "read_status",
                    "arguments": {},
                })
                .to_string(),
            ))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
        assert_eq!(seen.requests.load(Ordering::SeqCst), 1);
        assert_eq!(seen.project_id.lock().unwrap().as_deref(), Some("default"));
        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
