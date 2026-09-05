use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::isolation::IsolationClaims;
use crate::CoreError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MCPMessageType {
    Request,
    Response,
    Notification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<MCPError>,
}

impl MCPMessage {
    pub fn request(method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::String(uuid::Uuid::new_v4().hyphenated().to_string())),
            method: Some(method.to_string()),
            params,
            result: None,
            error: None,
        }
    }

    pub fn response(result: Value, id: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(code: i32, message: &str, id: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: None,
            params: None,
            result: None,
            error: Some(MCPError {
                code,
                message: message.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// A missing scope is shared by every authenticated tenant. The server
    /// still rejects requests without verified claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_scope: Option<MCPToolScope>,
    #[serde(default)]
    pub risk_level: MCPToolRiskLevel,
}

/// Tenant/project binding for an inbound MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MCPToolScope {
    pub tenant_id: String,
    pub project_id: String,
}

impl MCPToolScope {
    pub fn from_claims(claims: &IsolationClaims) -> Self {
        Self {
            tenant_id: claims.tenant_id().to_string(),
            project_id: claims.project_id().to_string(),
        }
    }

    fn matches(&self, claims: &IsolationClaims) -> bool {
        self.tenant_id == claims.tenant_id() && self.project_id == claims.project_id()
    }
}

/// Risk disclosed to MCP clients. High and critical tools are marked
/// destructive in the MCP annotations payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MCPToolRiskLevel {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

impl MCPTool {
    pub fn new(name: &str, description: &str, input_schema: Value) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
            output_schema: None,
            isolation_scope: None,
            risk_level: MCPToolRiskLevel::Normal,
        }
    }

    /// Bind this tool to the verified tenant/project scope.
    pub fn scoped_to(mut self, claims: &IsolationClaims) -> Self {
        self.isolation_scope = Some(MCPToolScope::from_claims(claims));
        self
    }

    pub fn with_risk_level(mut self, risk_level: MCPToolRiskLevel) -> Self {
        self.risk_level = risk_level;
        self
    }

    fn is_visible_to(&self, claims: &IsolationClaims) -> bool {
        self.isolation_scope
            .as_ref()
            .map(|scope| scope.matches(claims))
            .unwrap_or(true)
    }

    pub fn to_mcp_format(&self) -> Value {
        let destructive = matches!(
            self.risk_level,
            MCPToolRiskLevel::High | MCPToolRiskLevel::Critical
        );
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
            "annotations": {
                "destructiveHint": destructive,
                "x-agentos-risk-level": self.risk_level,
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPResource {
    pub uri: String,
    pub name: String,
    pub description: String,
    #[serde(default = "default_mime_type")]
    pub mime_type: String,
}

fn default_mime_type() -> String {
    "text/plain".to_string()
}

impl MCPResource {
    pub fn new(uri: &str, name: &str, description: &str) -> Self {
        Self {
            uri: uri.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            mime_type: "text/plain".to_string(),
        }
    }

    pub fn to_mcp_format(&self) -> Value {
        json!({
            "uri": self.uri,
            "name": self.name,
            "description": self.description,
            "mimeType": self.mime_type,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPPrompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<Value>,
}

impl MCPPrompt {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            arguments: Vec::new(),
        }
    }

    pub fn to_mcp_format(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "arguments": self.arguments,
        })
    }
}

#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn execute(&self, arguments: Value) -> Result<Value, CoreError>;
}

pub struct FunctionToolHandler<F>(pub F);

#[async_trait]
impl<F, Fut> ToolHandler for FunctionToolHandler<F>
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<Value, CoreError>> + Send,
{
    async fn execute(&self, arguments: Value) -> Result<Value, CoreError> {
        (self.0)(arguments).await
    }
}

pub struct MCPToolRegistry {
    tools: RwLock<HashMap<String, Vec<RegisteredMCPTool>>>,
}

struct RegisteredMCPTool {
    tool: MCPTool,
    handler: Arc<dyn ToolHandler>,
}

impl MCPToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    pub fn register<H: ToolHandler + 'static>(&self, tool: MCPTool, handler: H) {
        let name = tool.name.clone();
        let mut tools = self.tools.write();
        let registrations = tools.entry(name).or_default();
        // A registration is unique inside its tenant/project scope. Replacing
        // it leaves registrations for other scopes untouched.
        registrations.retain(|registered| registered.tool.isolation_scope != tool.isolation_scope);
        registrations.push(RegisteredMCPTool {
            tool,
            handler: Arc::new(handler),
        });
    }

    pub fn register_fn<F, Fut>(&self, tool: MCPTool, handler: F)
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value, CoreError>> + Send + 'static,
    {
        self.register(tool, FunctionToolHandler(handler));
    }

    pub fn list_tools(&self) -> Vec<MCPTool> {
        self.tools
            .read()
            .values()
            .flat_map(|registrations| {
                registrations
                    .iter()
                    .map(|registered| registered.tool.clone())
            })
            .collect()
    }

    /// Lists only tools that can be disclosed to verified claims. Requests
    /// without claims are intentionally fail-closed.
    pub fn list_tools_for_claims(&self, claims: Option<&IsolationClaims>) -> Vec<MCPTool> {
        let Some(claims) = claims else {
            return Vec::new();
        };

        self.tools
            .read()
            .values()
            .flat_map(|registrations| registrations.iter())
            .filter(|registered| registered.tool.is_visible_to(claims))
            .map(|registered| registered.tool.clone())
            .collect()
    }

    pub fn get_tool(&self, name: &str) -> Option<MCPTool> {
        self.tools
            .read()
            .get(name)
            .and_then(|registrations| registrations.first())
            .map(|registered| registered.tool.clone())
    }

    pub async fn execute(&self, name: &str, arguments: Value) -> Result<Value, CoreError> {
        let handler = self
            .tools
            .read()
            .get(name)
            .and_then(|registrations| registrations.first())
            .map(|registered| Arc::clone(&registered.handler));

        match handler {
            Some(h) => h.execute(arguments).await,
            None => Err(CoreError::Internal {
                message: format!("Tool not found: {}", name),
            }),
        }
    }

    pub async fn execute_for_claims(
        &self,
        name: &str,
        arguments: Value,
        claims: Option<&IsolationClaims>,
    ) -> Result<Value, CoreError> {
        let claims = claims.ok_or_else(|| CoreError::ValidationFailed {
            message: "Verified IsolationClaims are required for inbound MCP tools".to_string(),
        })?;
        let handler = self
            .tools
            .read()
            .get(name)
            .and_then(|registrations| {
                registrations
                    .iter()
                    .find(|registered| registered.tool.is_visible_to(claims))
            })
            .map(|registered| Arc::clone(&registered.handler));

        match handler {
            Some(handler) => handler.execute(arguments).await,
            None => Err(CoreError::ValidationFailed {
                // Do not disclose whether this name exists in another scope.
                message: "Tool is not available for this tenant/project".to_string(),
            }),
        }
    }
}

impl Default for MCPToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct MCPServer {
    #[allow(dead_code)]
    name: String,
    tools: Arc<MCPToolRegistry>,
    resources: RwLock<HashMap<String, MCPResource>>,
    prompts: RwLock<HashMap<String, MCPPrompt>>,
}

impl MCPServer {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            tools: Arc::new(MCPToolRegistry::new()),
            resources: RwLock::new(HashMap::new()),
            prompts: RwLock::new(HashMap::new()),
        }
    }

    pub fn register_tool<H: ToolHandler + 'static>(&self, tool: MCPTool, handler: H) {
        self.tools.register(tool, handler);
    }

    pub fn register_resource(&self, resource: MCPResource) {
        self.resources
            .write()
            .insert(resource.uri.clone(), resource);
    }

    pub fn register_prompt(&self, prompt: MCPPrompt) {
        self.prompts.write().insert(prompt.name.clone(), prompt);
    }

    pub fn tools(&self) -> &MCPToolRegistry {
        &self.tools
    }

    pub async fn handle_message(&self, message: MCPMessage) -> MCPMessage {
        self.handle_message_with_claims(message, None).await
    }

    /// Handles an inbound MCP request using claims minted by an authentication
    /// boundary. Catalog listing and invocation fail closed without claims.
    pub async fn handle_message_with_claims(
        &self,
        message: MCPMessage,
        claims: Option<&IsolationClaims>,
    ) -> MCPMessage {
        let method = match &message.method {
            Some(m) => m,
            None => return MCPMessage::error(-32600, "Invalid request", message.id.clone()),
        };

        match method.as_str() {
            "tools/list" => {
                if claims.is_none() {
                    return MCPMessage::error(
                        -32001,
                        "Verified IsolationClaims are required for inbound MCP tools",
                        message.id,
                    );
                }
                let tools: Vec<Value> = self
                    .tools
                    .list_tools_for_claims(claims)
                    .iter()
                    .map(|t| t.to_mcp_format())
                    .collect();
                MCPMessage::response(json!({"tools": tools}), message.id.unwrap_or(Value::Null))
            }

            "tools/call" => {
                if claims.is_none() {
                    return MCPMessage::error(
                        -32001,
                        "Verified IsolationClaims are required for inbound MCP tools",
                        message.id,
                    );
                }
                let params = message.params.clone().unwrap_or(Value::Null);
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

                match self
                    .tools
                    .execute_for_claims(tool_name, arguments, claims)
                    .await
                {
                    Ok(result) => MCPMessage::response(
                        json!({"content": [{"type": "text", "text": result.to_string()}]}),
                        message.id.unwrap_or(Value::Null),
                    ),
                    Err(e) => MCPMessage::error(-32603, &e.to_string(), message.id),
                }
            }

            "resources/list" => {
                let resources: Vec<Value> = self
                    .resources
                    .read()
                    .values()
                    .map(|r| r.to_mcp_format())
                    .collect();
                MCPMessage::response(
                    json!({"resources": resources}),
                    message.id.unwrap_or(Value::Null),
                )
            }

            "prompts/list" => {
                let prompts: Vec<Value> = self
                    .prompts
                    .read()
                    .values()
                    .map(|p| p.to_mcp_format())
                    .collect();
                MCPMessage::response(
                    json!({"prompts": prompts}),
                    message.id.unwrap_or(Value::Null),
                )
            }

            _ => MCPMessage::error(-32601, &format!("Method not found: {}", method), message.id),
        }
    }
}

pub struct MCPClient {
    server: Option<Arc<MCPServer>>,
}

impl MCPClient {
    pub fn new() -> Self {
        Self { server: None }
    }

    pub fn connect(&mut self, server: Arc<MCPServer>) {
        self.server = Some(server);
    }

    pub async fn list_tools(&self) -> Result<Vec<MCPTool>, CoreError> {
        self.list_tools_with_claims(None).await
    }

    pub async fn list_tools_with_claims(
        &self,
        claims: Option<&IsolationClaims>,
    ) -> Result<Vec<MCPTool>, CoreError> {
        let server = self.server.as_ref().ok_or_else(|| CoreError::Internal {
            message: "Not connected to server".to_string(),
        })?;

        let message = MCPMessage::request("tools/list", None);
        let response = server.handle_message_with_claims(message, claims).await;

        if let Some(error) = response.error {
            return Err(CoreError::Internal {
                message: error.message,
            });
        }

        let tools_data = response
            .result
            .and_then(|r| r.get("tools").cloned())
            .unwrap_or(Value::Array(Vec::new()));

        let tools: Vec<MCPTool> = serde_json::from_value(tools_data).unwrap_or_default();
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, CoreError> {
        self.call_tool_with_claims(name, arguments, None).await
    }

    pub async fn call_tool_with_claims(
        &self,
        name: &str,
        arguments: Value,
        claims: Option<&IsolationClaims>,
    ) -> Result<Value, CoreError> {
        let server = self.server.as_ref().ok_or_else(|| CoreError::Internal {
            message: "Not connected to server".to_string(),
        })?;

        let message = MCPMessage::request(
            "tools/call",
            Some(json!({
                "name": name,
                "arguments": arguments,
            })),
        );

        let response = server.handle_message_with_claims(message, claims).await;

        if let Some(error) = response.error {
            return Err(CoreError::Internal {
                message: error.message,
            });
        }

        let content = response
            .result
            .and_then(|r| r.get("content").cloned())
            .unwrap_or(Value::Array(Vec::new()));

        if let Some(first) = content.as_array().and_then(|a| a.first()) {
            if first.get("type").and_then(|t| t.as_str()) == Some("text") {
                let text = first.get("text").and_then(|t| t.as_str()).unwrap_or("");
                return serde_json::from_str(text).or_else(|_| Ok(Value::String(text.to_string())));
            }
        }

        Ok(content)
    }

    pub async fn list_resources(&self) -> Result<Vec<MCPResource>, CoreError> {
        let server = self.server.as_ref().ok_or_else(|| CoreError::Internal {
            message: "Not connected to server".to_string(),
        })?;

        let message = MCPMessage::request("resources/list", None);
        let response = server.handle_message(message).await;

        if let Some(error) = response.error {
            return Err(CoreError::Internal {
                message: error.message,
            });
        }

        let resources_data = response
            .result
            .and_then(|r| r.get("resources").cloned())
            .unwrap_or(Value::Array(Vec::new()));

        let resources: Vec<MCPResource> =
            serde_json::from_value(resources_data).unwrap_or_default();
        Ok(resources)
    }
}

impl Default for MCPClient {
    fn default() -> Self {
        Self::new()
    }
}

pub fn create_default_mcp_server() -> MCPServer {
    let server = MCPServer::new("agent-os");

    server.tools.register_fn(
        MCPTool::new(
            "file_read",
            "Read content from a file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to read"},
                    "encoding": {"type": "string", "default": "utf-8"},
                },
                "required": ["path"],
            }),
        )
        .with_risk_level(MCPToolRiskLevel::Low),
        |args| async move {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let encoding = args
                .get("encoding")
                .and_then(|v| v.as_str())
                .unwrap_or("utf-8");

            match std::fs::read_to_string(path) {
                Ok(content) => Ok(json!({"content": content, "path": path, "encoding": encoding})),
                Err(e) => Err(CoreError::Internal {
                    message: e.to_string(),
                }),
            }
        },
    );

    server.tools.register_fn(
        MCPTool::new(
            "file_write",
            "Write content to a file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to write"},
                    "content": {"type": "string", "description": "Content to write"},
                },
                "required": ["path", "content"],
            }),
        )
        .with_risk_level(MCPToolRiskLevel::High),
        |args| async move {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");

            match std::fs::write(path, content) {
                Ok(_) => Ok(json!({"success": true, "path": path})),
                Err(e) => Err(CoreError::Internal {
                    message: e.to_string(),
                }),
            }
        },
    );

    server.tools.register_fn(
        MCPTool::new(
            "http_request",
            "Make an HTTP request",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "URL to request"},
                    "method": {"type": "string", "default": "GET"},
                    "headers": {"type": "object"},
                    "body": {"type": "object"},
                    "timeout": {"type": "number", "default": 30},
                },
                "required": ["url"],
            }),
        )
        .with_risk_level(MCPToolRiskLevel::High),
        |args| async move {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("GET");

            Ok(json!({
                "url": url,
                "method": method,
                "status": "simulated",
                "note": "HTTP client requires reqwest integration"
            }))
        },
    );

    server
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(tenant: &str, project: &str) -> IsolationClaims {
        IsolationClaims::from_verified(tenant, project, "test-actor").unwrap()
    }

    #[tokio::test]
    async fn test_mcp_server() {
        let server = MCPServer::new("test");
        let tenant = claims("tenant-a", "project-a");

        server.tools.register_fn(
            MCPTool::new("echo", "Echo input", json!({"type": "object"})),
            |args| async move { Ok(args) },
        );

        let message = MCPMessage::request("tools/list", None);
        let response = server
            .handle_message_with_claims(message, Some(&tenant))
            .await;

        assert!(response.result.is_some());
        let result = response.result.as_ref().unwrap();
        let tools = result.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 1);
    }

    #[tokio::test]
    async fn test_mcp_client() {
        let server = Arc::new(create_default_mcp_server());
        let mut client = MCPClient::new();
        client.connect(server);
        let tenant = claims("tenant-a", "project-a");

        let tools = client.list_tools_with_claims(Some(&tenant)).await.unwrap();
        assert!(!tools.is_empty());
    }

    #[tokio::test]
    async fn inbound_catalog_is_scoped_to_verified_claims() {
        let server = MCPServer::new("test");
        let tenant_a = claims("tenant-a", "project");
        let tenant_b = claims("tenant-b", "project");

        server.tools.register_fn(
            MCPTool::new("tenant-a-tool", "Visible only to tenant A", json!({}))
                .scoped_to(&tenant_a),
            |_| async { Ok(json!({"tenant": "a"})) },
        );
        server.tools.register_fn(
            MCPTool::new("tenant-b-tool", "Visible only to tenant B", json!({}))
                .scoped_to(&tenant_b),
            |_| async { Ok(json!({"tenant": "b"})) },
        );
        server.tools.register_fn(
            MCPTool::new("shared-read", "Visible to authenticated tenants", json!({}))
                .with_risk_level(MCPToolRiskLevel::Low),
            |_| async { Ok(json!({"shared": true})) },
        );

        let listed_for_a = server
            .handle_message_with_claims(MCPMessage::request("tools/list", None), Some(&tenant_a))
            .await;
        let listed_tools = listed_for_a.result.unwrap();
        let names_for_a: Vec<String> = listed_tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .map(str::to_string)
            .collect();
        assert!(names_for_a.contains(&"tenant-a-tool".to_string()));
        assert!(names_for_a.contains(&"shared-read".to_string()));
        assert!(!names_for_a.contains(&"tenant-b-tool".to_string()));

        let cross_tenant_call = server
            .handle_message_with_claims(
                MCPMessage::request(
                    "tools/call",
                    Some(json!({"name": "tenant-b-tool", "arguments": {}})),
                ),
                Some(&tenant_a),
            )
            .await;
        assert_eq!(cross_tenant_call.error.unwrap().code, -32603);
    }

    #[tokio::test]
    async fn inbound_catalog_rejects_requests_without_claims() {
        let server = MCPServer::new("test");
        server.tools.register_fn(
            MCPTool::new("echo", "Echo input", json!({"type": "object"})),
            |args| async move { Ok(args) },
        );

        let list_response = server
            .handle_message(MCPMessage::request("tools/list", None))
            .await;
        assert_eq!(list_response.error.unwrap().code, -32001);

        let call_response = server
            .handle_message(MCPMessage::request(
                "tools/call",
                Some(json!({"name": "echo", "arguments": {}})),
            ))
            .await;
        assert_eq!(call_response.error.unwrap().code, -32001);
    }

    #[test]
    fn mcp_catalog_discloses_dangerous_tools() {
        let tool = MCPTool::new("file_write", "Writes a file", json!({}))
            .with_risk_level(MCPToolRiskLevel::High);
        let format = tool.to_mcp_format();

        assert_eq!(format["annotations"]["x-agentos-risk-level"], "high");
        assert_eq!(format["annotations"]["destructiveHint"], true);
    }

    #[test]
    fn test_mcp_message() {
        let req = MCPMessage::request("test/method", Some(json!({"arg": "value"})));
        assert_eq!(req.method, Some("test/method".to_string()));
        assert!(req.id.is_some());

        let resp = MCPMessage::response(json!({"result": "ok"}), Value::String("1".to_string()));
        assert!(resp.result.is_some());

        let err = MCPMessage::error(-32600, "Invalid request", None);
        assert!(err.error.is_some());
    }
}
