//! Prompt / 模型灰度版本管理（G6′）。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；本模块只承载处理器与请求体。

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::tools::prompt_registry::PromptVersion;

use super::iam::UserIdentity;
use super::AppState;

/// GET /api/v1/prompts — 列举所有 Prompt 版本
pub(crate) async fn list_prompts_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let versions = state.prompts.list_versions();
    let active_id = state.prompts.active_id();
    Json(json!({
        "count": versions.len(),
        "active_id": active_id,
        "versions": versions,
    }))
}

/// POST /api/v1/prompts — 创建新版本（G7：仅 DA 角色）
#[derive(Deserialize)]
pub(crate) struct CreatePromptRequest {
    name: String,
    #[serde(default)]
    description: String,
    template: String,
    model: String,
    version: String,
}

pub(crate) async fn create_prompt_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(body): Json<CreatePromptRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if body.name.trim().is_empty()
        || body.template.trim().is_empty()
        || body.model.trim().is_empty()
        || body.version.trim().is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "error": "名称、模板、模型、版本号均为必填",
            })),
        )
            .into_response();
    }
    let prompt = PromptVersion::new(
        body.name.trim(),
        body.template,
        body.model.trim(),
        body.version.trim(),
        body.description.trim(),
    );
    let id = state.prompts.add_version(prompt);
    (
        StatusCode::CREATED,
        Json(json!({ "status": "created", "id": id })),
    )
        .into_response()
}

/// POST /api/v1/prompts/:id/activate — 激活指定版本
pub(crate) async fn activate_prompt_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if state.prompts.activate(&id) {
        Json(json!({ "status": "activated", "id": id })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "版本不存在", "id": id })),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
pub struct CanaryRequest {
    pub percent: u8,
    #[serde(default)]
    pub tenant_ids: Vec<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

/// PUT /api/v1/prompts/:id/canary — 设置灰度规则
pub(crate) async fn canary_prompt_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<CanaryRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if state
        .prompts
        .set_canary(&id, req.percent, req.tenant_ids, req.roles)
    {
        Json(json!({ "status": "ok", "id": id, "percent": req.percent })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "版本不存在", "id": id })),
        )
            .into_response()
    }
}

/// DELETE /api/v1/prompts/:id — 删除版本
pub(crate) async fn delete_prompt_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if state.prompts.delete_version(&id) {
        Json(json!({ "status": "deleted", "id": id })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "版本不存在", "id": id })),
        )
            .into_response()
    }
}

/// GET /api/v1/prompts/resolve?tenant_id=&user_id=&role= — 灰度路由决策
pub(crate) async fn resolve_prompt_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let tenant_id = params
        .get("tenant_id")
        .map(|s| s.as_str())
        .unwrap_or("default");
    let user_id = params
        .get("user_id")
        .map(|s| s.as_str())
        .unwrap_or("anonymous");
    let role = params.get("role").map(|s| s.as_str()).unwrap_or("");
    match state.prompts.resolve(tenant_id, user_id, role) {
        Some(resolved) => Json(json!({ "status": "ok", "resolved": resolved })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "无可用 Prompt 版本（请先激活一个版本）" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_create_prompt_request_accepts_frontend_payload() {
        let request: CreatePromptRequest = serde_json::from_value(json!({
            "name": "维修助手",
            "description": "维修场景 Prompt",
            "template": "你是 {{tenant_id}} 的维修助手",
            "model": "deepseek-chat",
            "version": "1.0.0"
        }))
        .unwrap();
        assert_eq!(request.name, "维修助手");
        assert_eq!(request.version, "1.0.0");
    }
}
