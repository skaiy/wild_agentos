//! Outbound MCP surface for tenant-published Skills.
//!
//! This is intentionally separate from `mcp.rs`, which manages *inbound*
//! third-party MCP servers. A Skill is never externally visible by default:
//! a DA must explicitly create an exposure after its tenant publish gate has
//! succeeded. Kernel (`iri://`) skills cannot be exposed.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::mcp::{MCPError, MCPMessage};
use crate::tools::skill_registry::SkillMeta;

use super::iam::{AuthMethod, UserIdentity};
use super::{data_dir, skills::is_tenant_published_skill, AppState};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct McpSkillExposure {
    pub tenant_id: String,
    pub skill_iri: String,
    pub tool_name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

fn exposures_path() -> std::path::PathBuf {
    data_dir().join("mcp_skill_exposures.json")
}

fn load_exposures() -> Vec<McpSkillExposure> {
    std::fs::read_to_string(exposures_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_exposures(exposures: &[McpSkillExposure]) -> std::io::Result<()> {
    let path = exposures_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(exposures).unwrap_or_else(|_| "[]".into()),
    )
}

/// External MCP clients must always use a verified JWT, including in
/// development mode. `X-Identity` is deliberately not an external boundary.
fn require_mcp_identity(identity: &UserIdentity) -> Result<(), (StatusCode, Json<Value>)> {
    if identity.auth_method == AuthMethod::Jwt && identity.isolation_claims().is_some() {
        return Ok(());
    }
    Err((
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized", "message": "MCP requires a verified Bearer JWT"})),
    ))
}

fn has_skill_role(identity: &UserIdentity, skill: &SkillMeta) -> bool {
    skill
        .allowed_roles
        .iter()
        .any(|role| identity.has_role(role))
}

fn mcp_error(code: i32, message: impl AsRef<str>, id: Option<Value>) -> MCPMessage {
    MCPMessage {
        jsonrpc: "2.0".into(),
        id,
        method: None,
        params: None,
        result: None,
        error: Some(MCPError {
            code,
            message: message.as_ref().into(),
        }),
    }
}

fn mcp_tool(skill: &SkillMeta, exposure: &McpSkillExposure) -> Value {
    json!({
        "name": exposure.tool_name,
        "description": skill.description,
        "inputSchema": skill.input_schema,
        "annotations": {
            "title": skill.name,
            "skillIri": skill.skill_iri,
            "version": skill.version,
        },
    })
}

fn exposed_skill(
    state: &AppState,
    identity: &UserIdentity,
    tool_name: &str,
) -> Option<(McpSkillExposure, SkillMeta)> {
    load_exposures().into_iter().find_map(|exposure| {
        (exposure.enabled
            && exposure.tenant_id == identity.tenant_id
            && exposure.tool_name == tool_name
            && is_tenant_published_skill(&exposure.skill_iri)
            && !exposure.skill_iri.starts_with("iri://"))
        .then(|| {
            state
                .core
                .skills
                .get_skill(&exposure.skill_iri)
                .map(|skill| (exposure, skill))
        })
        .flatten()
    })
}

/// POST /mcp — Streamable-HTTP-compatible JSON-RPC subset for tenant Skills.
pub(crate) async fn skill_mcp_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(message): Json<MCPMessage>,
) -> impl IntoResponse {
    if let Err(error) = require_mcp_identity(&identity) {
        return error.into_response();
    }

    let response = match message.method.as_deref() {
        Some("initialize") => MCPMessage::response(
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "wild-agentos-skills", "version": env!("CARGO_PKG_VERSION")},
            }),
            message.id.unwrap_or(Value::Null),
        ),
        Some("tools/list") => {
            let tools: Vec<Value> = load_exposures()
                .into_iter()
                .filter(|exposure| {
                    exposure.enabled
                        && exposure.tenant_id == identity.tenant_id
                        && is_tenant_published_skill(&exposure.skill_iri)
                        && !exposure.skill_iri.starts_with("iri://")
                })
                .filter_map(|exposure| {
                    state
                        .core
                        .skills
                        .get_skill(&exposure.skill_iri)
                        .filter(|skill| has_skill_role(&identity, skill))
                        .map(|skill| mcp_tool(&skill, &exposure))
                })
                .collect();
            MCPMessage::response(json!({"tools": tools}), message.id.unwrap_or(Value::Null))
        }
        Some("tools/call") => {
            let params = message.params.unwrap_or(Value::Null);
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let Some((exposure, skill)) = exposed_skill(&state, &identity, tool_name) else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(mcp_error(-32601, "Skill MCP tool not found", message.id)),
                )
                    .into_response();
            };
            if !has_skill_role(&identity, &skill) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(mcp_error(-32003, "Forbidden for this Skill", message.id)),
                )
                    .into_response();
            }
            if !arguments.is_object() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(mcp_error(
                        -32602,
                        "Tool arguments must be a JSON object",
                        message.id,
                    )),
                )
                    .into_response();
            }
            if let Err(error) = state
                .core
                .skills
                .validate_input(&skill.skill_iri, &arguments.to_string())
            {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(mcp_error(
                        -32602,
                        format!("Invalid tool arguments: {error}"),
                        message.id,
                    )),
                )
                    .into_response();
            }

            // Skill packages are metadata and contracts, not arbitrary executable code.
            // A successful call therefore dispatches a validated invocation envelope to
            // the configured runtime boundary; this endpoint never evaluates imported
            // package source. A runtime executor can consume this stable envelope later.
            MCPMessage::response(
                json!({"content": [{"type": "json", "json": {
                    "status": "accepted",
                    "skill_iri": skill.skill_iri,
                    "tool_name": exposure.tool_name,
                    "arguments": arguments,
                }}]}),
                message.id.unwrap_or(Value::Null),
            )
        }
        Some(method) => mcp_error(-32601, format!("Method not found: {method}"), message.id),
        None => mcp_error(-32600, "Invalid request", message.id),
    };
    (StatusCode::OK, Json(response)).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpSkillExposureRequest {
    pub skill_iri: String,
    pub tool_name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// GET /api/v1/mcp/skill-exposures — DA-only tenant-local exposure configuration.
pub(crate) async fn list_skill_exposures_handler(identity: UserIdentity) -> impl IntoResponse {
    if let Err(error) = identity.require_role("DA") {
        return error.into_response();
    }
    let exposures: Vec<_> = load_exposures()
        .into_iter()
        .filter(|exposure| exposure.tenant_id == identity.tenant_id)
        .collect();
    Json(json!({"count": exposures.len(), "exposures": exposures})).into_response()
}

/// POST /api/v1/mcp/skill-exposures — DA-only explicit external publication.
pub(crate) async fn upsert_skill_exposure_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<McpSkillExposureRequest>,
) -> impl IntoResponse {
    if let Err(error) = identity.require_role("DA") {
        return error.into_response();
    }
    if request.skill_iri.starts_with("iri://") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "system Skills cannot be exposed over MCP"})),
        )
            .into_response();
    }
    if !is_tenant_published_skill(&request.skill_iri) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "Skill must pass the tenant publish gate before MCP exposure"})),
        )
            .into_response();
    }
    if state.core.skills.get_skill(&request.skill_iri).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "published Skill is no longer registered"})),
        )
            .into_response();
    }
    if request.tool_name.is_empty()
        || request.tool_name.len() > 128
        || !request
            .tool_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": "tool_name must be 1-128 ASCII letters, digits, '.', '_' or '-'"}),
            ),
        )
            .into_response();
    }

    let exposure = McpSkillExposure {
        tenant_id: identity.tenant_id.clone(),
        skill_iri: request.skill_iri,
        tool_name: request.tool_name,
        enabled: request.enabled,
    };
    let mut exposures = load_exposures();
    if let Some(existing) = exposures.iter_mut().find(|existing| {
        existing.tenant_id == exposure.tenant_id && existing.skill_iri == exposure.skill_iri
    }) {
        *existing = exposure.clone();
    } else if exposures.iter().any(|existing| {
        existing.tenant_id == exposure.tenant_id && existing.tool_name == exposure.tool_name
    }) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "tool_name is already used by another exposed Skill"})),
        )
            .into_response();
    } else {
        exposures.push(exposure.clone());
    }
    match save_exposures(&exposures) {
        Ok(()) => (
            StatusCode::CREATED,
            Json(json!({"status": "ok", "exposure": exposure})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to persist MCP Skill exposure: {error}")})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpSkillExposureQuery {
    pub skill_iri: String,
}

/// DELETE /api/v1/mcp/skill-exposures?skill_iri=... — DA-only unpublish.
pub(crate) async fn delete_skill_exposure_handler(
    identity: UserIdentity,
    Query(query): Query<McpSkillExposureQuery>,
) -> impl IntoResponse {
    if let Err(error) = identity.require_role("DA") {
        return error.into_response();
    }
    let mut exposures = load_exposures();
    let before = exposures.len();
    exposures.retain(|exposure| {
        !(exposure.tenant_id == identity.tenant_id && exposure.skill_iri == query.skill_iri)
    });
    if exposures.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    match save_exposures(&exposures) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_skill() -> SkillMeta {
        SkillMeta {
            skill_iri: "skill://acme/weather".into(),
            name: "weather".into(),
            description: "Read weather".into(),
            version: "1.0.0".into(),
            category: "weather".into(),
            security_level: "normal".into(),
            allowed_roles: vec!["DA".into()],
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
            output_schema: json!({"type": "object"}),
            compiled_template: "{}".into(),
            signature: None,
            signature_algorithm: None,
            input_mapping: Default::default(),
            output_mapping: Default::default(),
            skill_types: vec![],
        }
    }

    #[test]
    fn advertised_tool_preserves_published_input_schema() {
        let skill = sample_skill();
        let exposure = McpSkillExposure {
            tenant_id: "acme".into(),
            skill_iri: skill.skill_iri.clone(),
            tool_name: "weather.lookup".into(),
            enabled: true,
        };
        let tool = mcp_tool(&skill, &exposure);
        assert_eq!(tool["name"], "weather.lookup");
        assert_eq!(tool["inputSchema"]["required"][0], "city");
        assert_eq!(tool["annotations"]["skillIri"], "skill://acme/weather");
    }

    #[test]
    fn system_iri_is_not_an_external_skill_candidate() {
        let exposure = McpSkillExposure {
            tenant_id: "acme".into(),
            skill_iri: "iri://skills/code_execute".into(),
            tool_name: "dangerous.execute".into(),
            enabled: true,
        };
        assert!(exposure.skill_iri.starts_with("iri://"));
    }
}
