//! Agent 聊天 / RAG、对外 Public 门禁与 OpenAI 兼容层。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装。

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::gateway::unified_gateway::{ChatContent, ChatMessage};
use crate::isolation::IsolationClaims;
use crate::knowledge_graph::ontology_layer::{
    EV_REPAIR_ONT_FAULT, EV_REPAIR_PROMPT_TEMPLATE, EV_REPAIR_RUNTIME_ASSET_ID,
};
use crate::knowledge_graph::store::KnowledgeGraphStore;
use crate::memory::hyperspace_store::HybridSearchFilter;

use super::api_gov;
use super::iam::UserIdentity;
use super::AppState;

#[derive(Deserialize)]
pub struct AgentChatRequest {
    pub message: String,
    #[serde(default)]
    pub images: Vec<String>,
    /// Legacy client-directed retrieval targets are deliberately ignored.
    #[serde(default, rename = "named_graph")]
    _named_graph: Option<String>,
    /// Legacy client-directed retrieval targets are deliberately ignored.
    #[serde(default, rename = "vector_namespace")]
    _vector_namespace: Option<String>,
}

/// POST /api/v1/agents/:id/chat — 内部单轮聊天（必须携带验证过的 JWT claims）。
pub(crate) async fn agent_chat_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<AgentChatRequest>,
) -> impl IntoResponse {
    let message = req.message.trim().to_string();
    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "message 不能为空" })),
        );
    }
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for chat" })),
            )
        }
    };
    let messages = single_user_message(&message, &req.images);
    if let Err((status, body)) = validate_image_payload(&messages) {
        return (status, Json(body));
    }
    let (status, body) = run_agent_chat(&state, &id, messages, Some(claims)).await;
    (status, Json(body))
}

const DEFAULT_MAX_IMAGES: usize = 8;
const DEFAULT_MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Image input is URL/data-URI payload, so this limits the bytes supplied by
/// the caller. Remote image content is fetched by the configured model
/// provider and cannot be measured safely at this boundary.
fn image_payload_limits() -> (usize, usize) {
    let max_images = std::env::var("AGENTOS_MAX_IMAGES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_MAX_IMAGES);
    let max_bytes = std::env::var("AGENTOS_MAX_IMAGE_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_MAX_IMAGE_BYTES);
    (max_images, max_bytes)
}

fn image_urls(messages: &[ChatMessage]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| message.content.image_urls())
        .collect()
}

fn validate_image_payload(messages: &[ChatMessage]) -> Result<(), (StatusCode, Value)> {
    validate_image_payload_with_limits(messages, image_payload_limits())
}

fn validate_image_payload_with_limits(
    messages: &[ChatMessage],
    (max_images, max_bytes): (usize, usize),
) -> Result<(), (StatusCode, Value)> {
    let image_urls = image_urls(messages);
    if image_urls.len() > max_images {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({
                "error": "too_many_images",
                "max_images": max_images,
                "received_images": image_urls.len(),
            }),
        ));
    }
    let payload_bytes = image_urls.iter().map(String::len).sum::<usize>();
    if payload_bytes > max_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({
                "error": "image_payload_too_large",
                "max_image_bytes": max_bytes,
                "received_image_bytes": payload_bytes,
            }),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisionFallback {
    Error,
    Degrade,
}

fn vision_fallback_policy() -> VisionFallback {
    vision_fallback_policy_from(std::env::var("AGENTOS_VISION_FALLBACK").ok().as_deref())
}

fn vision_fallback_policy_from(value: Option<&str>) -> VisionFallback {
    if value == Some("degrade") {
        VisionFallback::Degrade
    } else {
        VisionFallback::Error
    }
}

/// Agent chat context ready for the gateway.
#[derive(Debug)]
struct ChatContext {
    messages: Vec<ChatMessage>,
    /// 本次实际调用的真实型号名（按 model_mounts 选模型解析，回退旧 model/default）。
    model: String,
    vision_mount_unavailable: bool,
}

/// Resolve the configured resource for one capability mount.
async fn mounted_model(state: &Arc<AppState>, agent: &Value, key: &str) -> Option<(String, Value)> {
    let res_id = agent
        .get("model_mounts")
        .and_then(|mounts| mounts.get(key))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let resources = {
        let cfg = state.config_info.read().await;
        cfg.get("models")
            .and_then(|m| m.get("resources"))
            .and_then(|v| v.as_array())
            .cloned()
    };
    resources?
        .into_iter()
        .find(|resource| resource.get("id").and_then(Value::as_str) == Some(res_id))
        .and_then(|resource| {
            let model = resource
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .map(str::to_owned);
            model.map(|model| (model, resource))
        })
}

/// Resolve the existing chat mount, then preserve legacy model/default fallback.
async fn resolve_chat_model(state: &Arc<AppState>, agent: &Value) -> String {
    if let Some((model, _)) = mounted_model(state, agent, "chat").await {
        return model;
    }
    agent
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.gateway.default_model())
}

fn resource_supports_vision(resource: &Value) -> bool {
    resource
        .get("supports_vision")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || resource
            .get("modalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| {
                modalities
                    .iter()
                    .any(|modality| modality.as_str() == Some("vision"))
            })
}

/// Claims-scoped chat may use only an agent explicitly owned by the same
/// tenant/project. Missing scope is denied rather than treated as a shared
/// agent, so a legacy record cannot accidentally expose its model.
fn agent_matches_claims(agent: &Value, claims: &IsolationClaims) -> bool {
    agent.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
        && agent.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
}

/// The only built-in chat runtime asset. The Agent's mount remains the
/// authorization point; the pack itself is only a capability declaration.
///
/// The check intentionally requires the seeded built-in identity and profile,
/// rather than trusting a caller-selected graph, a mutable prompt string, or
/// another tenant's similarly named pack.
async fn has_ev_repair_asset(state: &Arc<AppState>, agent: &Value) -> bool {
    let mounted = agent
        .get("knowledge_pack_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| {
            ids.iter()
                .any(|id| id.as_str() == Some(EV_REPAIR_RUNTIME_ASSET_ID))
        });
    if !mounted {
        return false;
    }
    state.knowledge_packs.read().await.iter().any(|pack| {
        pack.get("id").and_then(Value::as_str) == Some(EV_REPAIR_RUNTIME_ASSET_ID)
            && pack.get("builtin").and_then(Value::as_bool) == Some(true)
            && pack
                .get("runtime_asset")
                .and_then(|asset| asset.get("id"))
                .and_then(Value::as_str)
                == Some(EV_REPAIR_RUNTIME_ASSET_ID)
    })
}

fn extract_fault_code_tokens(message: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, tokens: &mut Vec<String>| {
        if current.len() >= 3
            && (current.chars().any(|c| c.is_ascii_digit())
                || current.contains('_')
                || current.len() >= 4)
        {
            tokens.push(current.to_lowercase());
        }
        current.clear();
    };
    for character in message.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            current.push(character);
        } else {
            flush(&mut current, &mut tokens);
        }
    }
    flush(&mut current, &mut tokens);
    tokens.dedup();
    tokens
}

fn ev_repair_retrieval_query(code_tokens: &[String]) -> Option<String> {
    if code_tokens.is_empty() {
        return None;
    }
    let filters = code_tokens
        .iter()
        .map(|token| format!("CONTAINS(LCASE(STR(?code)), \"{token}\")"))
        .collect::<Vec<_>>()
        .join(" || ");
    Some(format!(
        "SELECT ?code ?label ?meaning ?can_drive ?repair ?models ?brand WHERE {{ \
         ?n a <{EV_REPAIR_ONT_FAULT}> . \
         ?n <https://agentos.ontology/meta/code> ?code . \
         OPTIONAL {{ ?n <http://www.w3.org/2000/01/rdf-schema#label> ?label }} \
         OPTIONAL {{ ?n <https://agentos.ontology/meta/meaning> ?meaning }} \
         OPTIONAL {{ ?n <https://agentos.ontology/meta/can_drive> ?can_drive }} \
         OPTIONAL {{ ?n <https://agentos.ontology/meta/repair> ?repair }} \
         OPTIONAL {{ ?n <https://agentos.ontology/meta/models> ?models }} \
         OPTIONAL {{ ?n <http://aps.local/ontology/belongsToBrand> ?bn . ?bn <http://www.w3.org/2000/01/rdf-schema#label> ?brand }} \
         FILTER({filters}) }} LIMIT 6"
    ))
}

fn truncate_retrieval_fact(value: &str, maximum: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= maximum {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(maximum).collect::<String>())
    }
}

async fn ev_repair_context(
    state: &Arc<AppState>,
    claims: &IsolationClaims,
    messages: &[ChatMessage],
    agent_name: &str,
) -> Result<Vec<ChatMessage>, (StatusCode, Value)> {
    let question = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.as_text())
        .unwrap_or_default();
    let mut facts = String::new();
    if let Some(query) = ev_repair_retrieval_query(&extract_fault_code_tokens(&question)) {
        let graph =
            KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": format!("chat graph retrieval unavailable: {e}") }),
                )
            })?;
        let rows = graph.query_sparql_for_claims(claims, &query).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("chat graph retrieval failed: {e}") }),
            )
        })?;
        for row in rows {
            let value = |name| row.get(name).and_then(Value::as_str).unwrap_or_default();
            facts.push_str(&format!(
                "- 故障码 {}（{}）：{}\n  含义：{}\n  能否行驶：{}\n  维修建议：{}\n  适用车型：{}\n",
                value("?code"),
                value("?brand"),
                value("?label"),
                truncate_retrieval_fact(value("?meaning"), 300),
                truncate_retrieval_fact(value("?can_drive"), 200),
                truncate_retrieval_fact(value("?repair"), 300),
                truncate_retrieval_fact(value("?models"), 160),
            ));
        }
    }
    let mut retrieval = if facts.is_empty() {
        "【知识图谱检索结果】\n（未检索到相关故障码记录）".to_string()
    } else {
        format!("【知识图谱检索结果】\n{facts}")
    };
    if let Some(store) = state.vector_store.load_full() {
        let hits = store
            .search_with_claims(claims, &question, &HybridSearchFilter::new(), 5)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": format!("chat vector retrieval failed: {e}") }),
                )
            })?;
        if !hits.is_empty() {
            retrieval.push_str("\n【向量知识库检索结果】\n");
            for hit in hits {
                retrieval.push_str(&format!(
                    "- （相关度 {:.2}）{}\n",
                    hit.score,
                    truncate_retrieval_fact(&hit.text, 400)
                ));
            }
        }
    }
    Ok(vec![
        ChatMessage {
            role: "system".into(),
            content: EV_REPAIR_PROMPT_TEMPLATE
                .replace("{{agent_name}}", agent_name)
                .into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        },
        ChatMessage {
            role: "system".into(),
            content: retrieval.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        },
    ])
}

/// Single-turn native chat requests become a regular user message. The generic
/// HTTP paths deliberately add neither a default system message nor retrieval
/// context, so an Agent can be used by businesses other than EV repair.
fn single_user_message(message: &str, images: &[String]) -> Vec<ChatMessage> {
    let content = if images.is_empty() {
        ChatContent::text(message)
    } else {
        let mut parts = vec![ChatContent::part_text(message)];
        parts.extend(images.iter().cloned().map(ChatContent::image));
        ChatContent::Parts(parts)
    };
    vec![ChatMessage {
        role: "user".into(),
        content,
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }]
}

/// Validate Agent accessibility and choose its chat/vision model. Callers'
/// messages are forwarded unchanged unless this claims-scoped Agent explicitly
/// mounts the built-in `ev-repair` runtime asset.
async fn build_chat_context(
    state: &Arc<AppState>,
    id: &str,
    messages: Vec<ChatMessage>,
    claims: Option<&IsolationClaims>,
) -> Result<ChatContext, (StatusCode, Value)> {
    build_chat_context_with_fallback(state, id, messages, claims, vision_fallback_policy()).await
}

async fn build_chat_context_with_fallback(
    state: &Arc<AppState>,
    id: &str,
    messages: Vec<ChatMessage>,
    claims: Option<&IsolationClaims>,
    fallback: VisionFallback,
) -> Result<ChatContext, (StatusCode, Value)> {
    // Locate user-state Agent first, then the static batch configuration.
    let agent = {
        let guard = state.user_agents.read().await;
        guard
            .iter()
            .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
            .cloned()
            .or_else(|| {
                state
                    .agents_info
                    .get("agents")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| {
                        arr.iter()
                            .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
                            .cloned()
                    })
            })
    };
    let agent = match agent {
        Some(a) => a,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                json!({ "error": "agent not found", "id": id }),
            ))
        }
    };
    if let Some(claims) = claims {
        if !agent_matches_claims(&agent, claims) {
            return Err((
                StatusCode::FORBIDDEN,
                json!({
                    "error": "agent is not accessible in the verified tenant/project scope",
                    "id": id,
                }),
            ));
        }
    }
    let has_image = !image_urls(&messages).is_empty();
    let (selected_model, vision_mount_unavailable) = if has_image {
        match mounted_model(state, &agent, "vision").await {
            Some((model, resource)) if resource_supports_vision(&resource) => (model, false),
            _ => (resolve_chat_model(state, &agent).await, true),
        }
    } else {
        (resolve_chat_model(state, &agent).await, false)
    };
    if vision_mount_unavailable && fallback == VisionFallback::Error {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "error": "vision_mount_unavailable",
                "message": "images require a model_mounts.vision resource with vision capability",
            }),
        ));
    }
    let messages = if let Some(claims) = claims {
        if has_ev_repair_asset(state, &agent).await {
            let agent_name = agent
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("维修助手");
            let mut asset_messages =
                ev_repair_context(state, claims, &messages, agent_name).await?;
            asset_messages.extend(messages);
            asset_messages
        } else {
            messages
        }
    } else {
        messages
    };
    Ok(ChatContext {
        messages,
        model: selected_model,
        vision_mount_unavailable,
    })
}

/// Send generic chat context through the selected Agent model.
async fn run_agent_chat(
    state: &Arc<AppState>,
    id: &str,
    messages: Vec<ChatMessage>,
    claims: Option<&IsolationClaims>,
) -> (StatusCode, Value) {
    let rc = match build_chat_context(state, id, messages, claims).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match state.gateway.chat_with_model(&rc.model, rc.messages).await {
        Ok(resp) => {
            let answer = resp
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();
            let mut body = json!({
                "status": "ok",
                "answer": answer,
                "model": rc.model,
            });
            if rc.vision_mount_unavailable {
                body["degraded"] = json!(true);
                body["warning"] = json!("vision_mount_unavailable");
            }
            (StatusCode::OK, body)
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            json!({ "error": format!("LLM 网关调用失败：{}", e) }),
        ),
    }
}

// ─── 对外发布：Public API（入站密钥鉴权 + scope + 限流/配额 + 审计）──────────────

/// 从请求头解析入站密钥 → 调用方上下文；未命中/非法返回 401/403。
async fn authenticate_public(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
) -> Result<api_gov::ApiCallerContext, (StatusCode, Json<Value>)> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let token = match token {
        Some(t) => t,
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing bearer token" })),
            ))
        }
    };
    let keys = state.api_keys.read().await;
    let clients = state.api_clients.read().await;
    match api_gov::resolve_bearer_token(&token, &keys, &clients) {
        Ok(ctx) => Ok(ctx),
        Err(e) => {
            let code = match e {
                api_gov::AuthError::Unauthorized => StatusCode::UNAUTHORIZED,
                _ => StatusCode::FORBIDDEN,
            };
            Err((code, Json(json!({ "error": e.as_str() }))))
        }
    }
}

/// Agent 是否已发布（published=true）。
async fn agent_is_published(state: &Arc<AppState>, id: &str) -> bool {
    let guard = state.user_agents.read().await;
    guard
        .iter()
        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
        .and_then(|a| a.get("published").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

/// 更新命中密钥的 last_used_at 并落盘。
async fn touch_key_last_used(state: &Arc<AppState>, key_id: &str) {
    let mut keys = state.api_keys.write().await;
    if let Some(k) = keys.iter_mut().find(|k| k.id == key_id) {
        k.last_used_at = Some(chrono::Utc::now().to_rfc3339());
    }
    let _ = api_gov::save_api_keys(&keys);
}

/// 写一条对外调用审计（异步 fs 追加）。
fn write_public_audit(
    ctx: &api_gov::ApiCallerContext,
    agent_id: &str,
    endpoint: &str,
    status: u16,
    started: std::time::Instant,
    result: &str,
) {
    let entry = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "client_id": ctx.client_id,
        "key_prefix": ctx.key_prefix,
        "agent_id": agent_id,
        "endpoint": endpoint,
        "status": status,
        "result": result,
        "latency_ms": started.elapsed().as_millis() as u64,
        "tenant_id": ctx.tenant_id,
    });
    api_gov::append_audit(&entry);
}

/// 把限流/配额判定失败映射为 (状态码, 响应体, Retry-After 秒)。
fn usage_denied_response(d: &api_gov::UsageDenied) -> (StatusCode, Value, Option<u64>) {
    match d {
        api_gov::UsageDenied::RateLimited { retry_after } => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "rate_limited", "retry_after": retry_after }),
            Some(*retry_after),
        ),
        api_gov::UsageDenied::QuotaExceeded { scope } => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "quota_exceeded", "scope": scope }),
            None,
        ),
        api_gov::UsageDenied::Concurrency => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "concurrency_limit" }),
            None,
        ),
    }
}

/// 对外调用统一准入：鉴权 → scope(id ∈ granted && published) → 取 client → 限流/配额/并发。
/// 成功返回 (调用方上下文, 并发守卫)；失败返回可直接下发的响应（含审计与 Retry-After）。
async fn public_gate(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    id: &str,
    endpoint: &str,
    started: std::time::Instant,
) -> Result<(api_gov::ApiCallerContext, api_gov::ConcurrencyGuard), axum::response::Response> {
    let ctx = match authenticate_public(state, headers).await {
        Ok(c) => c,
        Err(resp) => return Err(resp.into_response()),
    };
    if !ctx.granted_agent_ids.iter().any(|a| a == id) {
        write_public_audit(&ctx, id, endpoint, 403, started, "not_in_scope");
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "agent not in scope", "id": id })),
        )
            .into_response());
    }
    if !agent_is_published(state, id).await {
        write_public_audit(&ctx, id, endpoint, 403, started, "not_published");
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "agent not published", "id": id })),
        )
            .into_response());
    }
    let client = {
        let clients = state.api_clients.read().await;
        clients.iter().find(|c| c.id == ctx.client_id).cloned()
    };
    let client = match client {
        Some(c) => c,
        None => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "client_disabled" })),
            )
                .into_response())
        }
    };
    let guard = match state.api_usage.try_acquire(&client) {
        Ok(g) => g,
        Err(denied) => {
            let (code, body, retry) = usage_denied_response(&denied);
            write_public_audit(&ctx, id, endpoint, code.as_u16(), started, "throttled");
            let mut resp = (code, Json(body)).into_response();
            if let Some(r) = retry {
                if let Ok(hv) = r.to_string().parse() {
                    resp.headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, hv);
                }
            }
            return Err(resp);
        }
    };
    Ok((ctx, guard))
}

/// POST /api/v1/public/agents/:id/chat — 对外单轮问答。
/// API keys do not carry verified tenant/project claims, so this endpoint
/// deliberately performs no tenant RAG and never falls back to another graph.
pub(crate) async fn public_agent_chat_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<AgentChatRequest>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let (ctx, _guard) = match public_gate(&state, &headers, &id, "chat", started).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let message = req.message.trim().to_string();
    if message.is_empty() {
        write_public_audit(&ctx, &id, "chat", 400, started, "empty_message");
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "message 不能为空" })),
        )
            .into_response();
    }
    let messages = single_user_message(&message, &req.images);
    if let Err((status, body)) = validate_image_payload(&messages) {
        write_public_audit(
            &ctx,
            &id,
            "chat",
            status.as_u16(),
            started,
            "image_payload_rejected",
        );
        return (status, Json(body)).into_response();
    }
    let (status, body) = run_agent_chat(&state, &id, messages, None).await;
    touch_key_last_used(&state, &ctx.key_id).await;
    write_public_audit(&ctx, &id, "chat", status.as_u16(), started, "ok");
    (status, Json(body)).into_response()
}

// ─── 流式：原生 SSE + OpenAI chunk（共用同一 token 流水线）────────────────────────

/// SSE 输出形态：原生（token/done 事件）或 OpenAI（chat.completion.chunk + [DONE]）。
#[derive(Clone, Copy)]
enum StreamShape {
    Native,
    OpenAI,
}

/// 把一段增量文本封装为对应形态的 SSE Event。
fn delta_event(shape: StreamShape, chat_id: &str, created: i64, model: &str, text: &str) -> Event {
    match shape {
        StreamShape::Native => Event::default()
            .event("token")
            .data(json!({ "delta": text }).to_string()),
        StreamShape::OpenAI => Event::default().data(
            json!({
                "id": chat_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }],
            })
            .to_string(),
        ),
    }
}

/// Stream a gateway chat context as native or OpenAI-compatible SSE.
/// The stream completion is audited after the concurrency guard is released.
/// `guard` 随流移动、于流结束时归还并发额度。
#[allow(clippy::too_many_arguments)]
fn build_sse_response(
    state: Arc<AppState>,
    ctx: api_gov::ApiCallerContext,
    id: String,
    endpoint: &'static str,
    started: std::time::Instant,
    rc: ChatContext,
    guard: api_gov::ConcurrencyGuard,
    shape: StreamShape,
    report_model: String,
) -> axum::response::Response {
    let llm_model = rc.model.clone();
    let vision_mount_unavailable = rc.vision_mount_unavailable;
    let chat_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let stream = async_stream::stream! {
        let _guard = guard; // 持有并发额度直至流结束
        let mut full = String::new();
        let mut ok = true;
        match state
            .gateway
            .stream_chat_with_params(&llm_model, rc.messages, None, None, None, None)
            .await
        {
            Ok(mut ms) => loop {
                match ms.next_event().await {
                    Ok(Some(ev)) => {
                        if let crate::llm::stream_types::StreamEvent::ContentBlockDelta(d) = &ev {
                            if let crate::llm::stream_types::ContentBlockDelta::TextDelta { text } = &d.delta {
                                if !text.is_empty() {
                                    full.push_str(text);
                                    yield Ok::<Event, Infallible>(delta_event(shape, &chat_id, created, &report_model, text));
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => { ok = false; break; }
                }
            },
            Err(_) => { ok = false; }
        }
        // 尾包。
        match shape {
            StreamShape::Native => {
                let mut done = json!({
                    "answer": full,
                    "model": llm_model,
                });
                if vision_mount_unavailable {
                    done["degraded"] = json!(true);
                    done["warning"] = json!("vision_mount_unavailable");
                }
                yield Ok(Event::default().event("done").data(
                    done.to_string(),
                ));
            }
            StreamShape::OpenAI => {
                yield Ok(Event::default().data(
                    json!({
                        "id": chat_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": report_model,
                        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                    })
                    .to_string(),
                ));
                yield Ok(Event::default().data("[DONE]"));
            }
        }
        let (status, result) = if !full.is_empty() {
            (200u16, if ok { "ok" } else { "degraded" })
        } else {
            (502u16, "error")
        };
        write_public_audit(&ctx, &id, endpoint, status, started, result);
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// POST /api/v1/public/agents/:id/chat/stream — 对外 SSE 流式问答（逐 token + done 尾包）。
pub(crate) async fn public_agent_chat_stream_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<AgentChatRequest>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let (ctx, guard) = match public_gate(&state, &headers, &id, "chat_stream", started).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let message = req.message.trim().to_string();
    if message.is_empty() {
        write_public_audit(&ctx, &id, "chat_stream", 400, started, "empty_message");
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "message 不能为空" })),
        )
            .into_response();
    }
    let messages = single_user_message(&message, &req.images);
    if let Err((status, body)) = validate_image_payload(&messages) {
        return (status, Json(body)).into_response();
    }
    let rc = match build_chat_context(&state, &id, messages, None).await {
        Ok(c) => c,
        Err((status, body)) => {
            write_public_audit(&ctx, &id, "chat_stream", status.as_u16(), started, "error");
            return (status, Json(body)).into_response();
        }
    };
    touch_key_last_used(&state, &ctx.key_id).await;
    let report_model = rc.model.clone();
    build_sse_response(
        state,
        ctx,
        id,
        "chat_stream",
        started,
        rc,
        guard,
        StreamShape::Native,
        report_model,
    )
}

// ─── OpenAI 兼容层：/v1/models、/v1/chat/completions（model = agentId）──────────────

#[derive(Deserialize)]
pub struct OpenAiMessage {
    #[serde(default)]
    pub role: String,
    /// 文本或多部件(含 image_url)内容;untagged 兼容旧 String 与新数组两种入参。
    #[serde(default)]
    pub content: ChatContent,
}

#[derive(Deserialize)]
pub struct OpenAiChatRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub stream: bool,
}

/// OpenAI 风格错误体。
fn openai_error(
    status: StatusCode,
    message: impl Into<String>,
    err_type: &str,
) -> axum::response::Response {
    (
        status,
        Json(json!({ "error": { "message": message.into(), "type": err_type } })),
    )
        .into_response()
}

/// 非流式 OpenAI chat.completion 响应（model 回显请求的 agentId）。
fn openai_completion_json(
    model: &str,
    answer: &str,
    vision_mount_unavailable: bool,
) -> axum::response::Response {
    let mut body = json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": answer },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
    });
    if vision_mount_unavailable {
        body["degraded"] = json!(true);
        body["warning"] = json!("vision_mount_unavailable");
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// GET /v1/models — 列出当前调用方 scope 内、且 published 的 Agent 作为 model。
pub(crate) async fn openai_list_models_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let ctx = match authenticate_public(&state, &headers).await {
        Ok(c) => c,
        Err(resp) => return resp.into_response(),
    };
    let created = chrono::Utc::now().timestamp();
    let agents = state.user_agents.read().await;
    let owner = if ctx.owner.is_empty() {
        "wild-agent-os".to_string()
    } else {
        ctx.owner.clone()
    };
    let data: Vec<Value> = ctx
        .granted_agent_ids
        .iter()
        .filter(|aid| {
            agents
                .iter()
                .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(aid.as_str()))
                .and_then(|a| a.get("published").and_then(|v| v.as_bool()))
                .unwrap_or(false)
        })
        .map(|aid| json!({ "id": aid, "object": "model", "created": created, "owned_by": owner }))
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "object": "list", "data": data })),
    )
        .into_response()
}

/// POST /v1/chat/completions — OpenAI-compatible chat (model = agentId).
pub(crate) async fn openai_chat_completions_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<OpenAiChatRequest>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let OpenAiChatRequest {
        model,
        messages,
        stream,
    } = req;
    let id = model.trim().to_string();
    if id.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "model (agentId) 不能为空",
            "invalid_request_error",
        );
    }
    let endpoint: &'static str = if stream {
        "chat_completions_stream"
    } else {
        "chat_completions"
    };
    let (ctx, guard) = match public_gate(&state, &headers, &id, endpoint, started).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if !messages
        .iter()
        .any(|message| message.role == "user" && !message.content.as_text().trim().is_empty())
    {
        write_public_audit(&ctx, &id, endpoint, 400, started, "empty_message");
        return openai_error(
            StatusCode::BAD_REQUEST,
            "messages 中缺少非空 user 内容",
            "invalid_request_error",
        );
    }
    // Preserve the full caller conversation, including caller-supplied system
    // messages and multimodal content. No default system prompt or RAG is added.
    let gateway_messages: Vec<ChatMessage> = messages
        .into_iter()
        .map(|message| ChatMessage {
            role: message.role,
            content: message.content,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        })
        .collect();
    if let Err((status, body)) = validate_image_payload(&gateway_messages) {
        return openai_error(
            status,
            body.get("error")
                .and_then(Value::as_str)
                .unwrap_or("invalid image payload"),
            "invalid_request_error",
        );
    }
    let rc = match build_chat_context(&state, &id, gateway_messages, None).await {
        Ok(c) => c,
        Err((status, body)) => {
            write_public_audit(&ctx, &id, endpoint, status.as_u16(), started, "error");
            let msg = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("agent not found")
                .to_string();
            return openai_error(status, msg, "invalid_request_error");
        }
    };
    touch_key_last_used(&state, &ctx.key_id).await;
    if stream {
        return build_sse_response(
            state,
            ctx,
            id.clone(),
            endpoint,
            started,
            rc,
            guard,
            StreamShape::OpenAI,
            id,
        );
    }
    match state.gateway.chat_with_model(&rc.model, rc.messages).await {
        Ok(resp) => {
            let answer = resp
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();
            write_public_audit(&ctx, &id, endpoint, 200, started, "ok");
            openai_completion_json(&id, &answer, rc.vision_mount_unavailable)
        }
        Err(e) => {
            write_public_audit(&ctx, &id, endpoint, 502, started, "error");
            openai_error(
                StatusCode::BAD_GATEWAY,
                format!("LLM 网关调用失败：{}", e),
                "api_error",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::core_types::{CoreConfig, SemanticCore};
    use crate::tools::prompt_registry::PromptRegistry;
    use axum::{body::Body, http::Request, routing::post, Router};
    use tower::ServiceExt;

    use super::super::api_gov::ApiUsageState;

    fn make_state() -> Arc<AppState> {
        let l0 = std::env::temp_dir().join(format!("chat_rag_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&l0).unwrap();
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: l0.to_string_lossy().into_owned(),
                event_buffer_size: 10,
                enable_metrics: false,
                eviction_config: None,
            })
            .unwrap(),
        );
        let gateway = Arc::new(
            crate::gateway::UnifiedGateway::new(&crate::config::GatewaySettings {
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
            agents_info: json!({ "count": 0, "agents": [] }),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![
                json!({
                    "id": "agent-a", "name": "Tenant A agent",
                    "tenant_id": "tenant-a", "project_id": "project-1",
                    "knowledge_pack_ids": ["attacker-pack"]
                }),
                json!({
                    "id": "agent-b", "name": "Tenant B agent",
                    "tenant_id": "tenant-b", "project_id": "project-1"
                }),
            ])),
            prompts: Arc::new(PromptRegistry::new()),
            kb_categories: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_bases: Arc::new(tokio::sync::RwLock::new(vec![])),
            // An attacker-controlled legacy pack target must not affect chat retrieval.
            knowledge_packs: Arc::new(tokio::sync::RwLock::new(vec![
                json!({
                    "id": "attacker-pack",
                    "named_graph": "graph://tenant-b/project-1",
                    "vector_namespace": "vector://tenant-b/project-1"
                }),
                json!({
                    "id": EV_REPAIR_RUNTIME_ASSET_ID,
                    "builtin": true,
                    "runtime_asset": { "id": EV_REPAIR_RUNTIME_ASSET_ID }
                }),
            ])),
            vector_store: Arc::new(arc_swap::ArcSwapOption::empty()),
            blob_store: None,
            task_executor: None,
            batch_manager: None,
            api_clients: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_keys: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_usage: Arc::new(ApiUsageState::default()),
            online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
            online_corpus_queue_capacity: 10,
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    #[tokio::test]
    async fn isolation_contract_chat_keeps_agent_access_scoped() {
        let state = make_state();
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-1", "actor-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project-1", "actor-b").unwrap();

        let a = build_chat_context(
            &state,
            "agent-a",
            single_user_message("P0A80", &[]),
            Some(&tenant_a),
        )
        .await
        .unwrap();
        assert_eq!(a.messages[0].content.as_text(), "P0A80");

        let b = build_chat_context(
            &state,
            "agent-b",
            single_user_message("P0A80", &[]),
            Some(&tenant_b),
        )
        .await
        .unwrap();
        assert_eq!(b.messages[0].content.as_text(), "P0A80");
    }

    #[tokio::test]
    async fn isolation_contract_chat_rejects_cross_tenant_agent_access() {
        let state = make_state();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project-1", "actor-b").unwrap();

        let err = match build_chat_context(
            &state,
            "agent-a",
            single_user_message("P0A80", &[]),
            Some(&tenant_b),
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("tenant B accessed tenant A's agent"),
        };
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn generic_chat_context_has_no_ev_repair_prompt_or_rag() {
        let state = make_state();
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-1", "actor-a").unwrap();

        let generic = build_chat_context(
            &state,
            "agent-a",
            single_user_message("Explain P0A80", &[]),
            Some(&tenant_a),
        )
        .await
        .unwrap();
        assert_eq!(generic.messages.len(), 1);
        assert_eq!(generic.messages[0].role, "user");
        assert_eq!(generic.messages[0].content.as_text(), "Explain P0A80");
        assert!(!generic.messages.iter().any(|message| {
            message
                .content
                .as_text()
                .contains("新能源汽车故障诊断与维修")
        }));
        assert!(!generic
            .messages
            .iter()
            .any(|message| message.content.as_text().contains("FaultCode")));
    }

    fn insert_fault(store: &oxigraph::store::Store, claims: &IsolationClaims, code: &str) {
        let graph = claims.graph_iri().unwrap();
        store
            .update(&format!(
                "INSERT DATA {{ GRAPH <{graph}> {{ \
                 <https://example.test/fault/{code}> a <{EV_REPAIR_ONT_FAULT}> ; \
                 <https://agentos.ontology/meta/code> \"{code}\" ; \
                 <http://www.w3.org/2000/01/rdf-schema#label> \"isolated fault\" . \
                 }} }}"
            ))
            .unwrap();
    }

    #[tokio::test]
    async fn ev_repair_asset_mount_adds_prompt_and_claims_scoped_fault_retrieval() {
        let state = make_state();
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-1", "actor-a").unwrap();
        insert_fault(&state.kg_store, &tenant_a, "P0A80");
        let mut agents = state.user_agents.write().await;
        let agent = agents
            .iter_mut()
            .find(|agent| agent["id"] == "agent-a")
            .unwrap();
        agent["knowledge_pack_ids"] = json!([EV_REPAIR_RUNTIME_ASSET_ID]);
        drop(agents);

        let context = build_chat_context(
            &state,
            "agent-a",
            single_user_message("Explain P0A80", &[]),
            Some(&tenant_a),
        )
        .await
        .unwrap();

        assert!(context.messages[0]
            .content
            .as_text()
            .contains("新能源汽车故障诊断与维修"));
        assert!(context.messages[1]
            .content
            .as_text()
            .contains("故障码 P0A80"));
        assert_eq!(context.messages[2].content.as_text(), "Explain P0A80");
    }

    #[tokio::test]
    async fn ev_repair_asset_does_not_cross_claims_bound_agents() {
        let state = make_state();
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-1", "actor-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project-1", "actor-b").unwrap();
        insert_fault(&state.kg_store, &tenant_a, "P0A80");
        let mut agents = state.user_agents.write().await;
        let agent = agents
            .iter_mut()
            .find(|agent| agent["id"] == "agent-a")
            .unwrap();
        agent["knowledge_pack_ids"] = json!([EV_REPAIR_RUNTIME_ASSET_ID]);
        drop(agents);

        let context = build_chat_context(
            &state,
            "agent-b",
            single_user_message("Explain P0A80", &[]),
            Some(&tenant_b),
        )
        .await
        .unwrap();

        assert_eq!(context.messages.len(), 1);
        assert!(!context.messages[0].content.as_text().contains("P0A80"));
    }

    #[tokio::test]
    async fn generic_chat_context_preserves_caller_messages() {
        let state = make_state();
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "Reply in English.".into(),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ChatMessage {
                role: "user".into(),
                content: "Summarize this release.".into(),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ];

        let generic = build_chat_context(&state, "agent-a", messages, None)
            .await
            .unwrap();
        assert_eq!(generic.messages.len(), 2);
        assert_eq!(generic.messages[0].content.as_text(), "Reply in English.");
        assert_eq!(
            generic.messages[1].content.as_text(),
            "Summarize this release."
        );
    }

    async fn configure_agent_a_model_mounts(
        state: &Arc<AppState>,
        vision_resource: Option<Value>,
        chat_resource: Value,
    ) {
        *state.config_info.write().await = json!({
            "models": { "resources": vision_resource.into_iter().chain(std::iter::once(chat_resource)).collect::<Vec<_>>() }
        });
        let mut agents = state.user_agents.write().await;
        let agent = agents
            .iter_mut()
            .find(|agent| agent.get("id").and_then(Value::as_str) == Some("agent-a"))
            .unwrap();
        agent["model_mounts"] = json!({ "vision": "vision", "chat": "chat" });
    }

    #[tokio::test]
    async fn image_request_uses_vision_mount_when_resource_supports_vision() {
        let state = make_state();
        configure_agent_a_model_mounts(
            &state,
            Some(json!({
                "id": "vision", "model": "vl-model", "modalities": ["chat", "vision"]
            })),
            json!({ "id": "chat", "model": "text-model", "modalities": ["chat"] }),
        )
        .await;

        let context = build_chat_context_with_fallback(
            &state,
            "agent-a",
            single_user_message("describe", &["https://example.test/image.png".into()]),
            None,
            VisionFallback::Error,
        )
        .await
        .unwrap();
        assert_eq!(context.model, "vl-model");
        assert!(!context.vision_mount_unavailable);
    }

    #[tokio::test]
    async fn image_request_degrades_only_when_explicitly_enabled() {
        let state = make_state();
        configure_agent_a_model_mounts(
            &state,
            Some(json!({ "id": "vision", "model": "text-vision-slot", "modalities": ["chat"] })),
            json!({ "id": "chat", "model": "text-model", "modalities": ["chat"] }),
        )
        .await;

        let context = build_chat_context_with_fallback(
            &state,
            "agent-a",
            single_user_message("describe", &["https://example.test/image.png".into()]),
            None,
            VisionFallback::Degrade,
        )
        .await
        .unwrap();
        assert_eq!(context.model, "text-model");
        assert!(context.vision_mount_unavailable);
    }

    #[tokio::test]
    async fn image_request_hard_fails_when_vision_mount_is_missing() {
        let state = make_state();
        let error = build_chat_context_with_fallback(
            &state,
            "agent-a",
            single_user_message("describe", &["https://example.test/image.png".into()]),
            None,
            VisionFallback::Error,
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.1["error"], "vision_mount_unavailable");
    }

    #[test]
    fn vision_fallback_degrade_mode_is_opt_in() {
        assert_eq!(vision_fallback_policy_from(None), VisionFallback::Error);
        assert_eq!(
            vision_fallback_policy_from(Some("degrade")),
            VisionFallback::Degrade
        );
        assert_eq!(
            vision_fallback_policy_from(Some("error")),
            VisionFallback::Error
        );
    }

    #[tokio::test]
    async fn text_only_request_does_not_require_a_vision_mount() {
        let state = make_state();
        let context = build_chat_context_with_fallback(
            &state,
            "agent-a",
            single_user_message("text only", &[]),
            None,
            VisionFallback::Error,
        )
        .await
        .unwrap();
        assert_eq!(context.model, "test-model");
        assert!(!context.vision_mount_unavailable);
    }

    #[test]
    fn image_payload_limits_reject_excess_count_and_bytes() {
        let too_many = single_user_message(
            "describe",
            &[
                "https://example.test/1.png".into(),
                "https://example.test/2.png".into(),
            ],
        );
        let count_error = validate_image_payload_with_limits(&too_many, (1, 1024)).unwrap_err();
        assert_eq!(count_error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(count_error.1["error"], "too_many_images");

        let too_large = single_user_message("describe", &["x".repeat(11)]);
        let bytes_error = validate_image_payload_with_limits(&too_large, (1, 10)).unwrap_err();
        assert_eq!(bytes_error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(bytes_error.1["error"], "image_payload_too_large");
    }

    #[tokio::test]
    async fn isolation_contract_chat_without_verified_identity_returns_unauthorized_not_empty_success(
    ) {
        let state = make_state();
        let router = Router::new()
            .route("/api/v1/agents/:id/chat", post(agent_chat_handler))
            .with_state(state);
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/agents/agent-a/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"message":"P0A80","images":["https://example.test/vehicle.png"],"named_graph":"graph://tenant-b/project-1","vector_namespace":"vector://tenant-b/project-1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
