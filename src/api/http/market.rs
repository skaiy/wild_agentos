//! Versioned Logic/Skill package market.
//!
//! This is a small kernel catalog, not a Foundry-style workshop: package
//! versions are immutable, tenant access is claims-scoped, and installation
//! records select an approved version for a tenant/project.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    knowledge_graph::ontology_layer::FunctionDef,
    tools::{
        skill_pipeline::{run_pipeline, PipelineContext, PipelineSource, SideEffectLevel},
        skill_registry::SkillMeta,
    },
};

use super::{iam::UserIdentity, skills::append_pipeline_run, AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PackageVisibility {
    /// Kernel bundles only. API callers cannot create, alter, or remove these.
    System,
    /// Visible to all projects in the publishing tenant.
    Tenant,
    /// Visible only to the publishing tenant and project.
    Private,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MarketPackage {
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub side_effect_level: SideEffectLevel,
    pub visibility: PackageVisibility,
    pub publisher_tenant_id: String,
    pub publisher_project_id: String,
    pub functions: Vec<FunctionDef>,
    pub skills: Vec<SkillMeta>,
    pub published_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Installation {
    name: String,
    version: String,
    #[serde(default)]
    previous_version: Option<String>,
    tenant_id: String,
    project_id: String,
    installed_at: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PublishPackageRequest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub side_effect_level: SideEffectLevel,
    #[serde(default = "default_visibility")]
    pub visibility: PackageVisibility,
    #[serde(default)]
    pub functions: Vec<FunctionDef>,
    #[serde(default)]
    pub skills: Vec<SkillMeta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InstallPackageRequest {
    pub version: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RollbackPackageRequest {}

#[derive(Debug, Deserialize)]
pub(crate) struct CatalogQuery {
    pub name: Option<String>,
}

fn default_visibility() -> PackageVisibility {
    PackageVisibility::Private
}

fn packages_path() -> std::path::PathBuf {
    super::data_dir().join("market_packages.json")
}

fn installations_path() -> std::path::PathBuf {
    super::data_dir().join("market_installations.json")
}

fn load_packages() -> Vec<MarketPackage> {
    std::fs::read_to_string(packages_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn load_installations() -> Vec<Installation> {
    std::fs::read_to_string(installations_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save<T: Serialize>(path: std::path::PathBuf, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Strict enough for published versions while avoiding a new dependency.
fn valid_semver(value: &str) -> bool {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(head, tail)| (head, Some(tail)));
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(head, tail)| (head, Some(tail)));
    let core_valid = core.split('.').count() == 3 && core.split('.').all(valid_numeric_identifier);
    core_valid
        && prerelease.is_none_or(valid_semver_suffix)
        && build.is_none_or(valid_semver_suffix)
}

fn valid_numeric_identifier(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|byte| byte.is_ascii_digit())
        && (part == "0" || !part.starts_with('0'))
}

fn valid_semver_suffix(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn compare_semver(left: &str, right: &str) -> std::cmp::Ordering {
    let split = |value: &str| {
        let value = value.split('+').next().unwrap_or(value);
        value
            .split_once('-')
            .map_or((value, None), |(core, pre)| (core, Some(pre)))
    };
    let (left_core, left_pre) = split(left);
    let (right_core, right_pre) = split(right);
    for (left, right) in left_core.split('.').zip(right_core.split('.')) {
        match left
            .parse::<u64>()
            .unwrap_or(0)
            .cmp(&right.parse::<u64>().unwrap_or(0))
        {
            std::cmp::Ordering::Equal => {}
            order => return order,
        }
    }
    match (left_pre, right_pre) {
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn claims_or_unauthorized(
    identity: &UserIdentity,
) -> Result<&crate::isolation::IsolationClaims, axum::response::Response> {
    identity.isolation_claims().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "verified JWT isolation claims are required"})),
        )
            .into_response()
    })
}

fn is_visible_to(package: &MarketPackage, claims: &crate::isolation::IsolationClaims) -> bool {
    match package.visibility {
        PackageVisibility::System => true,
        PackageVisibility::Tenant => package.publisher_tenant_id == claims.tenant_id(),
        PackageVisibility::Private => {
            package.publisher_tenant_id == claims.tenant_id()
                && package.publisher_project_id == claims.project_id()
        }
    }
}

fn has_published_version(
    packages: &[MarketPackage],
    name: &str,
    version: &str,
    claims: &crate::isolation::IsolationClaims,
) -> bool {
    packages.iter().any(|package| {
        package.name == name
            && package.version == version
            && package.publisher_tenant_id == claims.tenant_id()
            && package.publisher_project_id == claims.project_id()
    })
}

/// POST /api/v1/market/packages — publish an immutable version after Skill CI gates.
pub(crate) async fn publish_package_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<PublishPackageRequest>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    if let Err(error) = identity.require_role("DA") {
        return error.into_response();
    }
    if request.name.trim().is_empty() || !valid_semver(&request.version) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name must be non-empty and version must be SemVer (MAJOR.MINOR.PATCH)"})),
        ).into_response();
    }
    if request.visibility == PackageVisibility::System {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "system packages are kernel-owned and read-only"})),
        )
            .into_response();
    }
    for (label, schema) in [
        ("input_schema", &request.input_schema),
        ("output_schema", &request.output_schema),
    ] {
        if let Err(error) = jsonschema::JSONSchema::options().compile(schema) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("{label} is not a valid JSON Schema: {error}")})),
            )
                .into_response();
        }
    }

    let mut packages = load_packages();
    if has_published_version(&packages, &request.name, &request.version, claims) {
        return (StatusCode::CONFLICT, Json(json!({"error": "package version already exists; published versions are immutable"}))).into_response();
    }

    // Use the same admission gate as standalone Skills. Publication is atomic:
    // no package is persisted if any embedded Skill fails its gate.
    for skill in &request.skills {
        let mut ctx = PipelineContext::local(PipelineSource::Market, identity.user_id.clone());
        ctx.visibility = crate::tools::skill_pipeline::SkillVisibility::Tenant;
        let run = run_pipeline(
            &state.core.skills,
            skill,
            &ctx,
            Box::new(|_| Ok("market package admission".into())),
        );
        let permitted = run.gate_passed;
        let _ = append_pipeline_run(&run);
        if !permitted {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "embedded Skill failed admission gate", "pipeline_run": run})),
            )
                .into_response();
        }
    }

    let package = MarketPackage {
        name: request.name,
        version: request.version,
        description: request.description,
        input_schema: request.input_schema,
        output_schema: request.output_schema,
        side_effect_level: request.side_effect_level,
        visibility: request.visibility,
        publisher_tenant_id: claims.tenant_id().to_string(),
        publisher_project_id: claims.project_id().to_string(),
        functions: request.functions,
        skills: request.skills,
        published_at: chrono::Utc::now().to_rfc3339(),
    };
    packages.push(package.clone());
    match save(packages_path(), &packages) {
        Ok(()) => (
            StatusCode::CREATED,
            Json(json!({"status": "ok", "package": package})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

/// GET /api/v1/market/packages — list only packages visible to verified claims.
pub(crate) async fn list_packages_handler(
    identity: UserIdentity,
    Query(query): Query<CatalogQuery>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let packages: Vec<_> = load_packages()
        .into_iter()
        .filter(|package| {
            is_visible_to(package, claims)
                && query.name.as_ref().is_none_or(|name| &package.name == name)
        })
        .collect();
    Json(json!({"count": packages.len(), "packages": packages})).into_response()
}

fn select_visible_package(
    name: &str,
    version: &str,
    claims: &crate::isolation::IsolationClaims,
) -> Result<MarketPackage, axum::response::Response> {
    load_packages()
        .into_iter()
        .find(|package| package.name == name && package.version == version)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "package version not found"})),
            )
                .into_response()
        })
        .and_then(|package| {
            if is_visible_to(&package, claims) {
                Ok(package)
            } else {
                Err((
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "package is not visible to this tenant/project"})),
                )
                    .into_response())
            }
        })
}

/// POST /api/v1/market/packages/:name/install — install a visible version.
pub(crate) async fn install_package_handler(
    State(_state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(name): Path<String>,
    Json(request): Json<InstallPackageRequest>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(error) = identity.require_role("DA") {
        return error.into_response();
    }
    let package = match select_visible_package(&name, &request.version, claims) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Do not register package Skills in the process-global SkillRegistry here:
    // that registry has no tenant/project scope and would leak a private
    // package to other callers. The scoped installation record is the source
    // of truth for a future claims-aware package resolver.
    let mut installations = load_installations();
    if installations.iter().any(|entry| {
        entry.name == name
            && entry.tenant_id == claims.tenant_id()
            && entry.project_id == claims.project_id()
    }) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "package is already installed; use upgrade or rollback"})),
        )
            .into_response();
    }
    installations.retain(|entry| {
        !(entry.name == name
            && entry.tenant_id == claims.tenant_id()
            && entry.project_id == claims.project_id())
    });
    installations.push(Installation {
        name,
        version: package.version.clone(),
        previous_version: None,
        tenant_id: claims.tenant_id().into(),
        project_id: claims.project_id().into(),
        installed_at: chrono::Utc::now().to_rfc3339(),
    });
    match save(installations_path(), &installations) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"status": "ok", "installed": package})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

/// POST /api/v1/market/packages/:name/rollback — switch to a previously published visible version.
pub(crate) async fn rollback_package_handler(
    State(_state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(name): Path<String>,
    Json(_request): Json<RollbackPackageRequest>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    if let Err(error) = identity.require_role("DA") {
        return error.into_response();
    }
    let mut installations = load_installations();
    let Some(installation) = installations.iter_mut().find(|entry| {
        entry.name == name
            && entry.tenant_id == claims.tenant_id()
            && entry.project_id == claims.project_id()
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "package is not installed"})),
        )
            .into_response();
    };
    let Some(previous_version) = installation.previous_version.clone() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "no previous package version available for rollback"})),
        )
            .into_response();
    };
    let package = match select_visible_package(&name, &previous_version, claims) {
        Ok(package) => package,
        Err(response) => return response,
    };
    let current = std::mem::replace(&mut installation.version, previous_version);
    installation.previous_version = Some(current);
    installation.installed_at = chrono::Utc::now().to_rfc3339();
    match save(installations_path(), &installations) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"status": "ok", "rolled_back": package})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

/// POST /api/v1/market/packages/:name/upgrade — select a newer visible version.
/// Version ordering is deliberately caller-explicit; the server never chooses a
/// "latest" version because that would make upgrades non-deterministic.
pub(crate) async fn upgrade_package_handler(
    State(_state): State<Arc<AppState>>,
    identity: UserIdentity,
    Path(name): Path<String>,
    Json(request): Json<InstallPackageRequest>,
) -> impl IntoResponse {
    let claims = match claims_or_unauthorized(&identity) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    if let Err(error) = identity.require_role("DA") {
        return error.into_response();
    }
    let package = match select_visible_package(&name, &request.version, claims) {
        Ok(package) => package,
        Err(response) => return response,
    };
    let mut installations = load_installations();
    let Some(installation) = installations.iter_mut().find(|entry| {
        entry.name == name
            && entry.tenant_id == claims.tenant_id()
            && entry.project_id == claims.project_id()
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "package is not installed; use install first"})),
        )
            .into_response();
    };
    if compare_semver(&package.version, &installation.version) != std::cmp::Ordering::Greater {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "version_conflict",
                "installed": installation.version,
                "requested": package.version,
            })),
        )
            .into_response();
    }
    installation.previous_version = Some(installation.version.clone());
    installation.version = package.version.clone();
    installation.installed_at = chrono::Utc::now().to_rfc3339();
    match save(installations_path(), &installations) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"status": "ok", "upgraded": package})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_requires_three_numeric_core_components() {
        assert!(valid_semver("1.2.3"));
        assert!(valid_semver("1.2.3-rc.1"));
        assert!(!valid_semver("1.2"));
        assert!(!valid_semver("v1.2.3"));
        assert!(!valid_semver("01.2.3"));
        assert_eq!(
            compare_semver("1.1.0", "1.0.9"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_semver("1.0.0-rc.1", "1.0.0"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn cross_tenant_cannot_install_private_packages() {
        let package = MarketPackage {
            name: "private".into(),
            version: "1.0.0".into(),
            description: String::new(),
            input_schema: json!({}),
            output_schema: json!({}),
            side_effect_level: SideEffectLevel::None,
            visibility: PackageVisibility::Private,
            publisher_tenant_id: "a".into(),
            publisher_project_id: "p".into(),
            functions: vec![],
            skills: vec![],
            published_at: String::new(),
        };
        let owner = crate::isolation::IsolationClaims::from_verified("a", "p", "owner").unwrap();
        let other = crate::isolation::IsolationClaims::from_verified("b", "p", "other").unwrap();
        assert!(is_visible_to(&package, &owner));
        assert!(!is_visible_to(&package, &other));
    }

    #[test]
    fn published_versions_conflict_per_publisher_scope() {
        let package = MarketPackage {
            name: "logic".into(),
            version: "1.0.0".into(),
            description: String::new(),
            input_schema: json!({}),
            output_schema: json!({}),
            side_effect_level: SideEffectLevel::None,
            visibility: PackageVisibility::Private,
            publisher_tenant_id: "a".into(),
            publisher_project_id: "p".into(),
            functions: vec![],
            skills: vec![],
            published_at: String::new(),
        };
        let owner = crate::isolation::IsolationClaims::from_verified("a", "p", "owner").unwrap();
        assert!(has_published_version(&[package], "logic", "1.0.0", &owner));
        assert!(!has_published_version(&[], "logic", "1.0.0", &owner));
    }

    #[test]
    fn rollback_can_select_an_older_visible_version() {
        let _guard = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temporary = std::env::temp_dir().join(format!("market_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temporary).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &temporary);
        let package = |version: &str| MarketPackage {
            name: "logic".into(),
            version: version.into(),
            description: String::new(),
            input_schema: json!({}),
            output_schema: json!({}),
            side_effect_level: SideEffectLevel::Read,
            visibility: PackageVisibility::Tenant,
            publisher_tenant_id: "a".into(),
            publisher_project_id: "p".into(),
            functions: vec![],
            skills: vec![],
            published_at: String::new(),
        };
        save(packages_path(), &vec![package("1.0.0"), package("1.1.0")]).unwrap();
        let owner =
            crate::isolation::IsolationClaims::from_verified("a", "other", "owner").unwrap();
        let selected = select_visible_package("logic", "1.0.0", &owner).unwrap();
        assert_eq!(selected.version, "1.0.0");
        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(temporary);
    }
}
