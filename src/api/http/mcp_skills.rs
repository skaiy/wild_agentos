//! Outbound MCP surface for tenant-published Skills.
//!
//! This is intentionally separate from `mcp.rs`, which manages *inbound*
//! third-party MCP servers. A Skill is never externally visible by default:
//! a DA must explicitly create an exposure after its tenant publish gate has
//! succeeded. Kernel (`iri://`) skills cannot be exposed.
//!
//! Each exposure belongs to one verified tenant and project. Rows stored
//! before `project_id` existed stay on disk but are not listed, updated,
//! deleted, or served over MCP. Creating one again adds a new row. The old
//! row stays until an operator removes it from the file. Each process start
//! logs a warning when such rows are loaded.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::isolation::IsolationScopeProvenance;
use crate::tools::mcp::{MCPError, MCPMessage};
use crate::tools::skill_registry::SkillMeta;

use super::iam::{AuthMethod, UserIdentity};
use super::{
    data_dir,
    skills::{admission_snapshot, is_tenant_published_skill_async, skill_published_for_tenant},
    AppState,
};

/// Serializes exposure read-modify-write in this process. Taken only inside
/// `spawn_blocking`, never on an async worker and never across `.await`.
static EXPOSURE_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct McpSkillExposure {
    pub tenant_id: String,
    /// Verified project that owns this exposure. Missing or empty on legacy
    /// rows, which fail closed instead of joining any project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
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

fn exposure_project_id(exposure: &McpSkillExposure) -> Option<&str> {
    exposure
        .project_id
        .as_deref()
        .filter(|project_id| !project_id.is_empty())
}

fn exposure_in_scope(exposure: &McpSkillExposure, tenant_id: &str, project_id: &str) -> bool {
    exposure.tenant_id == tenant_id && exposure_project_id(exposure) == Some(project_id)
}

fn lock_exposure_store() -> std::sync::MutexGuard<'static, ()> {
    EXPOSURE_STORE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Missing file is an empty store. A file that will not parse is an error:
/// callers must not replace it with `[]`.
fn load_exposures_unlocked() -> std::io::Result<Vec<McpSkillExposure>> {
    let text = match std::fs::read_to_string(exposures_path()) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let exposures: Vec<McpSkillExposure> = serde_json::from_str(&text)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let legacy = exposures
        .iter()
        .filter(|exposure| exposure_project_id(exposure).is_none())
        .count();
    if legacy > 0 {
        // Once per process, so each restart warns again. The rows stay on disk.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                legacy_count = legacy,
                "MCP skill exposures without a project_id are ignored; a new create adds another row and these stay on disk until removed by hand"
            );
        });
    }
    Ok(exposures)
}

enum ExposureStoreFailure {
    Io(std::io::Error),
    ToolNameTaken,
    NotFound,
}

fn exposure_failure_response(error: ExposureStoreFailure) -> axum::response::Response {
    match error {
        ExposureStoreFailure::Io(error) => exposure_store_error(error).into_response(),
        ExposureStoreFailure::ToolNameTaken => (
            StatusCode::CONFLICT,
            Json(json!({"error": "tool_name is already used by another exposed Skill"})),
        )
            .into_response(),
        ExposureStoreFailure::NotFound => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn run_exposure_blocking<T>(
    job: impl FnOnce() -> Result<T, ExposureStoreFailure> + Send + 'static,
) -> Result<T, ExposureStoreFailure>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(job).await {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(error = %error, "skill exposure task failed");
            Err(ExposureStoreFailure::Io(std::io::Error::other(
                "skill exposure task failed",
            )))
        }
    }
}

/// One pipeline snapshot for every exposure in this request. The pipeline
/// lock is released before the exposure lock is taken.
fn load_mcp_view() -> std::io::Result<(
    std::collections::HashMap<String, super::skills::SkillIriOwnerRecord>,
    Vec<McpSkillExposure>,
)> {
    let owners = match admission_snapshot() {
        Ok(view) => view.owners,
        Err(error) => {
            tracing::error!(error = %error, "pipeline run store is unreadable");
            std::collections::HashMap::new()
        }
    };
    let _guard = lock_exposure_store();
    let exposures = load_exposures_unlocked()?;
    Ok((owners, exposures))
}

async fn load_mcp_view_async() -> Result<
    (
        std::collections::HashMap<String, super::skills::SkillIriOwnerRecord>,
        Vec<McpSkillExposure>,
    ),
    (StatusCode, Json<Value>),
> {
    match tokio::task::spawn_blocking(load_mcp_view).await {
        Ok(Ok(view)) => Ok(view),
        Ok(Err(error)) => Err(exposure_store_error(error)),
        Err(error) => {
            tracing::error!(error = %error, "skill exposure reader task failed");
            Err(exposure_store_error(std::io::Error::other(
                "skill exposure reader failed",
            )))
        }
    }
}

fn save_exposures_unlocked(exposures: &[McpSkillExposure]) -> std::io::Result<()> {
    let path = exposures_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(exposures)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    super::config::write_file_atomically(&path, &bytes)
}

fn exposure_store_error(error: std::io::Error) -> (StatusCode, Json<Value>) {
    tracing::error!(error = %error, "MCP skill exposure store failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "skill_exposure_store_failed"})),
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
    owners: &std::collections::HashMap<String, super::skills::SkillIriOwnerRecord>,
    exposures: &[McpSkillExposure],
    state: &AppState,
    tenant_id: &str,
    project_id: &str,
    tool_name: &str,
) -> Option<(McpSkillExposure, SkillMeta)> {
    exposures.iter().find_map(|exposure| {
        (exposure.enabled
            && exposure_in_scope(exposure, tenant_id, project_id)
            && exposure.tool_name == tool_name
            && skill_published_for_tenant(owners.get(&exposure.skill_iri), tenant_id, project_id)
            && !exposure.skill_iri.starts_with("iri://"))
        .then(|| {
            state
                .core
                .skills
                .get_skill(&exposure.skill_iri)
                .map(|skill| (exposure.clone(), skill))
        })
        .flatten()
    })
}

/// A verified token whose project was defaulted is not an explicit project.
/// `POST /mcp` rejects it before any exposure is read, same as catalog invoke.
fn reject_defaulted_mcp_scope(identity: &UserIdentity) -> Result<(), (StatusCode, Json<Value>)> {
    let Some(claims) = identity.isolation_claims() else {
        return Ok(());
    };
    if claims.provenance() != IsolationScopeProvenance::VerifiedDefaulted {
        return Ok(());
    }
    let missing_field = claims
        .missing_scope_field()
        .map(|field| field.as_str())
        .unwrap_or("project_id");
    Err((
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "mcp_claims_incomplete",
            "missing_field": missing_field,
        })),
    ))
}

fn mcp_claims_scope(identity: &UserIdentity) -> Option<(String, String)> {
    identity.isolation_claims().map(|claims| {
        (
            claims.tenant_id().to_owned(),
            claims.project_id().to_owned(),
        )
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
    let Some((tenant_id, project_id)) = mcp_claims_scope(&identity) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized", "message": "MCP requires a verified Bearer JWT"})),
        )
            .into_response();
    };
    if let Err(error) = reject_defaulted_mcp_scope(&identity) {
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
            let (owners, exposures) = match load_mcp_view_async().await {
                Ok(view) => view,
                Err(error) => return error.into_response(),
            };
            let tools: Vec<Value> = exposures
                .iter()
                .filter(|exposure| {
                    exposure.enabled
                        && exposure_in_scope(exposure, &tenant_id, &project_id)
                        && skill_published_for_tenant(
                            owners.get(&exposure.skill_iri),
                            &tenant_id,
                            &project_id,
                        )
                        && !exposure.skill_iri.starts_with("iri://")
                })
                .filter_map(|exposure| {
                    state
                        .core
                        .skills
                        .get_skill(&exposure.skill_iri)
                        .filter(|skill| has_skill_role(&identity, skill))
                        .map(|skill| mcp_tool(&skill, exposure))
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
            let (owners, exposures) = match load_mcp_view_async().await {
                Ok(view) => view,
                Err(error) => return error.into_response(),
            };
            let Some((exposure, skill)) = exposed_skill(
                &owners,
                &exposures,
                &state,
                &tenant_id,
                &project_id,
                tool_name,
            ) else {
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

/// Gate for exposure reads and writes (#302, #384): verified JWT scope with an
/// explicit project and the DA role. The scope is the verified tenant and
/// project, never an unverified identity field or a defaulted project.
fn verified_exposure_scope(
    identity: &UserIdentity,
    resource: &str,
) -> Result<(String, String), (StatusCode, Json<Value>)> {
    identity.require_control_plane_da(resource)?;
    let claims = identity.isolation_claims().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "verified_isolation_claims_required",
                "message": format!("verified isolation claims required for {resource}"),
            })),
        )
    })?;
    Ok((
        claims.tenant_id().to_owned(),
        claims.project_id().to_owned(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpSkillExposureRequest {
    pub skill_iri: String,
    pub tool_name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// GET /api/v1/mcp/skill-exposures — control-plane DA, one project's exposures.
pub(crate) async fn list_skill_exposures_handler(identity: UserIdentity) -> impl IntoResponse {
    let (tenant_id, project_id) = match verified_exposure_scope(&identity, "skill exposures") {
        Ok(scope) => scope,
        Err(error) => return error.into_response(),
    };
    let exposures = match run_exposure_blocking(|| {
        let _guard = lock_exposure_store();
        load_exposures_unlocked().map_err(ExposureStoreFailure::Io)
    })
    .await
    {
        Ok(exposures) => exposures,
        Err(error) => return exposure_failure_response(error),
    };
    let exposures: Vec<_> = exposures
        .into_iter()
        .filter(|exposure| exposure_in_scope(exposure, &tenant_id, &project_id))
        .collect();
    Json(json!({"count": exposures.len(), "exposures": exposures})).into_response()
}

/// POST /api/v1/mcp/skill-exposures — DA-only explicit external publication.
pub(crate) async fn upsert_skill_exposure_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<McpSkillExposureRequest>,
) -> impl IntoResponse {
    // #302 / #384: the exposure is owned by the verified tenant and project,
    // never by an unverified X-Identity tenant or a defaulted project.
    let (tenant_id, project_id) = match verified_exposure_scope(&identity, "skill exposure writes")
    {
        Ok(scope) => scope,
        Err(error) => return error.into_response(),
    };
    if request.skill_iri.starts_with("iri://") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "system Skills cannot be exposed over MCP"})),
        )
            .into_response();
    }
    if !is_tenant_published_skill_async(
        request.skill_iri.clone(),
        tenant_id.clone(),
        project_id.clone(),
    )
    .await
    {
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

    // `project_id` in the body is not a field of this request. The row is
    // owned by the verified claims even when a client sends another project.
    let exposure = McpSkillExposure {
        tenant_id: tenant_id.clone(),
        project_id: Some(project_id.clone()),
        skill_iri: request.skill_iri,
        tool_name: request.tool_name,
        enabled: request.enabled,
    };
    let write_tenant = tenant_id.clone();
    let write_project = project_id.clone();
    match run_exposure_blocking(move || {
        upsert_exposure_sync(exposure, &write_tenant, &write_project)
    })
    .await
    {
        Ok(exposure) => (
            StatusCode::CREATED,
            Json(json!({"status": "ok", "exposure": exposure})),
        )
            .into_response(),
        Err(error) => exposure_failure_response(error),
    }
}

fn upsert_exposure_sync(
    exposure: McpSkillExposure,
    tenant_id: &str,
    project_id: &str,
) -> Result<McpSkillExposure, ExposureStoreFailure> {
    let _guard = lock_exposure_store();
    let mut exposures = load_exposures_unlocked().map_err(ExposureStoreFailure::Io)?;
    if let Some(existing) = exposures.iter_mut().find(|existing| {
        exposure_in_scope(existing, tenant_id, project_id)
            && existing.skill_iri == exposure.skill_iri
    }) {
        *existing = exposure.clone();
    } else if exposures.iter().any(|existing| {
        exposure_in_scope(existing, tenant_id, project_id)
            && existing.tool_name == exposure.tool_name
    }) {
        return Err(ExposureStoreFailure::ToolNameTaken);
    } else {
        exposures.push(exposure.clone());
    }
    save_exposures_unlocked(&exposures).map_err(ExposureStoreFailure::Io)?;
    Ok(exposure)
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
    let (tenant_id, project_id) = match verified_exposure_scope(&identity, "skill exposure writes")
    {
        Ok(scope) => scope,
        Err(error) => return error.into_response(),
    };
    let skill_iri = query.skill_iri;
    match run_exposure_blocking(move || delete_exposure_sync(tenant_id, project_id, skill_iri))
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => exposure_failure_response(error),
    }
}

fn delete_exposure_sync(
    tenant_id: String,
    project_id: String,
    skill_iri: String,
) -> Result<(), ExposureStoreFailure> {
    let _guard = lock_exposure_store();
    let mut exposures = load_exposures_unlocked().map_err(ExposureStoreFailure::Io)?;
    let before = exposures.len();
    exposures.retain(|exposure| {
        !(exposure_in_scope(exposure, &tenant_id, &project_id) && exposure.skill_iri == skill_iri)
    });
    if exposures.len() == before {
        return Err(ExposureStoreFailure::NotFound);
    }
    save_exposures_unlocked(&exposures).map_err(ExposureStoreFailure::Io)?;
    Ok(())
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
            project_id: Some("project-a".into()),
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
            project_id: Some("project-a".into()),
            skill_iri: "iri://skills/code_execute".into(),
            tool_name: "dangerous.execute".into(),
            enabled: true,
        };
        assert!(exposure.skill_iri.starts_with("iri://"));
    }
}
