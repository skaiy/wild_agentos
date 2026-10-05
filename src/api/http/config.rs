//! 运行期配置：GET/PUT /api/v1/config、覆盖文件持久化、models/embedding 热切换。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装。

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::memory::hyperspace_store::HyperspaceStore;

use super::iam::{AuthMethod, UserIdentity};
use super::kb::spawn_reindex_all_vector_kbs;
use super::runtime::live_runtime_hardening_fields;
use super::{data_dir, AppState};

/// Validated request schema for the configuration write surface.
///
/// The settings document remains extensible for the non-gateway sections, but
/// gateway is typed because it controls the credentials used by the shared LLM
/// gateway.  This prevents a typo or arbitrary top-level JSON from silently
/// becoming persistent configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigUpdateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway: Option<GatewayConfigPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admin_policies: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GatewayConfigPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Accepted for runtime hot update only. It is deliberately omitted from
    /// config_override.json; configure a durable key through the deployment
    /// secret/environment instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_base_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_responses_api: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_mapping: Option<HashMap<String, String>>,
    /// UI-only state, ignored when persisting or applying the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_configured: Option<bool>,
}

impl ConfigUpdateRequest {
    fn into_patch(self) -> Value {
        serde_json::to_value(self).expect("configuration DTO is serializable")
    }
}

/// 运行期配置覆盖文件路径；由 PUT /api/v1/config 写入，启动时被 Settings::load() 作为
/// 高于 config.yaml 的来源读取。路径与 Settings::load 中的 "data/config_override" 保持一致。
fn config_override_path() -> std::path::PathBuf {
    data_dir().join("config_override.json")
}

/// Whether two OpenAI-compatible base URLs name the same endpoint (#299).
///
/// A saved provider credential may only be reused for the endpoint it was saved
/// with. Both sides are normalized with `normalize_api_base` (trim, trailing
/// `/`, trailing `/v1`) and then compared exactly; an empty side never matches.
pub(crate) fn same_provider_endpoint(a: &str, b: &str) -> bool {
    let a = crate::config::settings::normalize_api_base(a);
    let b = crate::config::settings::normalize_api_base(b);
    !a.is_empty() && a == b
}

/// 将网关配置持久化到运行期覆盖文件，重启后由 Settings::load() 生效。
/// Gateway API keys are runtime-only and never written to this file.
pub(crate) fn save_config_override(patch: &Value) -> std::io::Result<()> {
    let path = config_override_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .unwrap_or_else(|| json!({}));

    if let Some(gateway_patch) = patch.get("gateway").and_then(|v| v.as_object()) {
        let mut clean = gateway_patch.clone();
        // api_key_configured 仅用于前端展示，不是 GatewaySettings 字段。
        clean.remove("api_key_configured");
        // Credentials are supplied through the process secret/environment.
        // Never keep a new key, or a legacy key from a prior override, on disk.
        clean.remove("api_key");

        if let Some(obj) = root.as_object_mut() {
            let existing_gateway = obj.entry("gateway").or_insert(json!({}));
            if let Some(existing_gw_obj) = existing_gateway.as_object_mut() {
                existing_gw_obj.remove("api_key");
                for (k, v) in clean {
                    existing_gw_obj.insert(k, v);
                }
            }
        }
    }

    // Embedding（向量化）段：深合并，清理 UI 辅助字段与空 oneapi.api_key。
    if let Some(emb_patch) = patch.get("embedding") {
        let mut clean = emb_patch.clone();
        if let Some(o) = clean.as_object_mut() {
            o.remove("active_dimension");
            if let Some(oneapi) = o.get_mut("oneapi").and_then(|v| v.as_object_mut()) {
                oneapi.remove("api_key_configured");
                if oneapi
                    .get("api_key")
                    .and_then(|v| v.as_str())
                    .map(|s| s.is_empty())
                    .unwrap_or(false)
                {
                    oneapi.remove("api_key");
                }
            }
        }
        if let Some(obj) = root.as_object_mut() {
            let existing = obj.entry("embedding").or_insert(json!({}));
            // #299: a saved oneapi key is only kept for the endpoint it was saved
            // with. A patch that moves `oneapi.base_url` without supplying a new
            // key drops the saved key instead of carrying it to the new endpoint.
            let new_base = clean
                .get("oneapi")
                .filter(|o| o.get("api_key").is_none())
                .and_then(|o| o.get("base_url"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let (Some(new_base), Some(old_oneapi)) = (
                new_base,
                existing.get_mut("oneapi").and_then(|v| v.as_object_mut()),
            ) {
                let old_base = old_oneapi
                    .get("base_url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !same_provider_endpoint(&new_base, old_base) {
                    old_oneapi.remove("api_key");
                }
            }
            json_deep_merge(existing, &clean);
        }
    }

    // Models 段:整体替换 providers/resources(集合语义,避免深合并残留已删项);
    // 空/缺失 provider.api_key 时回填 root 中同 id 的旧 key,避免误清空。
    // #299: 仅当该 provider 的 base_url(归一化后)未变时才回填;端点变了则丢弃旧 key,
    // 绝不把已保存密钥带到新端点。
    if let Some(models_patch) = patch.get("models") {
        let mut clean = models_patch.clone();
        if let Some(provs) = clean.get_mut("providers").and_then(|v| v.as_array_mut()) {
            let old = root
                .get("models")
                .and_then(|m| m.get("providers"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for p in provs.iter_mut() {
                let pid = p
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let k = p
                    .get("api_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(o) = p.as_object_mut() {
                    o.remove("api_key_configured");
                }
                if k.is_empty() {
                    if let Some(old_p) = old
                        .iter()
                        .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(&pid))
                    {
                        let new_base = p
                            .get("base_url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let old_base = old_p.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
                        let same_endpoint = same_provider_endpoint(&new_base, old_base);
                        if let (Some(o), Some(ok)) = (p.as_object_mut(), old_p.get("api_key")) {
                            if same_endpoint && ok.as_str().map(|s| !s.is_empty()).unwrap_or(false)
                            {
                                o.insert("api_key".into(), ok.clone());
                            } else {
                                o.remove("api_key");
                            }
                        }
                    } else if let Some(o) = p.as_object_mut() {
                        o.remove("api_key");
                    }
                }
            }
        }
        if let Some(obj) = root.as_object_mut() {
            obj.insert("models".into(), clean);
        }
    }

    let content = serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&path, content)
}

/// 递归深合并 src 到 dst（对象逐键合并，其余类型直接覆盖）。
pub(crate) fn json_deep_merge(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                json_deep_merge(d.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (d, s) => *d = s.clone(),
    }
}

/// Shared first step of both config routes: a verified JWT with isolation
/// claims. Each route then applies its own role gate (#290 / #274).
fn require_verified_jwt(identity: &UserIdentity, action: &str) -> Option<Response> {
    if identity.auth_method != AuthMethod::Jwt || identity.isolation_claims().is_none() {
        return Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": "unauthorized",
                    "message": format!("a verified JWT is required to {action} configuration"),
                })),
            )
                .into_response(),
        );
    }
    None
}

/// GET /api/v1/config gate: verified JWT, then either a control-plane DA
/// (explicit tenant/project + DA) or a platform administrator (#274). On
/// rejection the DA gate's error is returned unchanged.
fn require_config_reader(identity: &UserIdentity) -> Option<Response> {
    if let Some(error) = require_verified_jwt(identity, "read") {
        return Some(error);
    }
    let da = identity.require_control_plane_da("configuration reads");
    if da.is_ok()
        || identity
            .require_platform_admin("configuration reads")
            .is_ok()
    {
        return None;
    }
    da.err().map(IntoResponse::into_response)
}

/// Secret-looking field names, compared after `normalize_field_name`.
const SECRET_FIELD_NAMES: &[&str] = &[
    "apikey",
    "secret",
    "token",
    "password",
    "secretkey",
    "privatekey",
    "accesskey",
    "authorization",
    "credential",
    "credentials",
];

/// Lowercase and drop `_` / `-`, so `accessToken`, `access_token` and
/// `ACCESS-TOKEN` all normalize to `accesstoken`.
fn normalize_field_name(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Blacklist check used by `scrub_secret_fields`: a field is secret when its
/// normalized name equals or ends with one of `SECRET_FIELD_NAMES`. Display
/// flags ending in `configured` (e.g. `api_key_configured`) are kept.
fn is_secret_field_name(key: &str) -> bool {
    let name = normalize_field_name(key);
    if name.ends_with("configured") {
        return false;
    }
    SECRET_FIELD_NAMES
        .iter()
        .any(|secret| name.ends_with(secret))
}

pub(crate) fn scrub_secret_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|key, _| !is_secret_field_name(key));
            for value in fields.values_mut() {
                scrub_secret_fields(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_secret_fields(item);
            }
        }
        _ => {}
    }
}

pub(crate) async fn config_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    if let Some(error) = require_config_reader(&identity) {
        return error;
    }
    let mut info = state.config_info.read().await.clone();
    if let Some(obj) = info.as_object_mut() {
        let live = live_runtime_hardening_fields();
        if let Some(map) = live.as_object() {
            for (k, v) in map {
                obj.insert(k.clone(), v.clone());
            }
        }
        // Workspace watch flags live in the startup snapshot when built by
        // AgentOSService; if a test/minimal snapshot omitted them, still expose
        // defaults so Admin always has a stable schema.
        if !obj.contains_key("workspace") {
            let ws = crate::config::settings::WorkspaceSettings::default();
            obj.insert(
                "workspace".to_string(),
                json!({
                    "watch_enabled": ws.watch_enabled,
                    "poll_interval_ms": ws.poll_interval_ms,
                    "debounce_ms": ws.debounce_ms,
                    "max_debounce_wait_ms": ws.max_debounce_wait_ms,
                    "content_store_max_bytes": ws.content_store_max_bytes,
                    "content_cache_capacity": ws.content_cache_capacity,
                }),
            );
        }
    }
    scrub_secret_fields(&mut info);
    Json(info).into_response()
}

/// PUT /api/v1/config — 更新运行期配置并持久化到 data/config_override.json（重启后由 Settings 生效）
/// Body: { "gateway": { "base_url": "...", "api_key": "...", "default_model": "...", ... } }
pub(crate) async fn update_config_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(request): Json<ConfigUpdateRequest>,
) -> impl IntoResponse {
    if let Some(error) = require_verified_jwt(&identity, "update") {
        return error;
    }
    // #274: process-global writes stay on the platform-admin gate; DA is not enough.
    if let Err(error) = identity.require_platform_admin("configuration updates") {
        return error.into_response();
    }
    let patch = request.into_patch();

    if let Err(error) = save_config_override(&patch) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "message": format!("配置持久化失败：{error}"),
                "persisted": false,
            })),
        )
            .into_response();
    }

    // 1. 运行时更新 Gateway 服务
    if let Some(gw_patch) = patch.get("gateway").and_then(|v| v.as_object()) {
        if let Some(base_url) = gw_patch.get("base_url").and_then(|v| v.as_str()) {
            state.gateway.set_base_url(base_url.to_string());
        }
        // 仅当用户明确提供了非空 api_key 时才更新运行时网关（避免覆盖 config.yaml 的密钥）。
        if let Some(api_key) = gw_patch
            .get("api_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            state.gateway.set_api_key(api_key.to_string());
        }
        if let Some(default_model) = gw_patch.get("default_model").and_then(|v| v.as_str()) {
            state.gateway.set_default_model(default_model.to_string());
        }
        if let Some(mapping) = gw_patch.get("model_mapping").and_then(|v| v.as_object()) {
            for (k, v) in mapping {
                if let Some(m) = v.as_str() {
                    state.gateway.set_model_mapping(k.clone(), m.to_string());
                }
            }
        }
    }

    let persisted = true;

    // 3. 更新已脱敏的运行期快照供前端展示
    {
        let mut info = state.config_info.write().await;
        if let Some(gw_patch) = patch.get("gateway").and_then(|v| v.as_object()) {
            if let Some(obj) = info.as_object_mut() {
                let gateway = obj.entry("gateway").or_insert(json!({}));
                if let Some(gateway_obj) = gateway.as_object_mut() {
                    for (k, v) in gw_patch {
                        if k != "api_key" {
                            gateway_obj.insert(k.clone(), v.clone());
                        }
                    }
                    gateway_obj.insert(
                        "api_key_configured".into(),
                        json!(state.gateway.api_key_configured()),
                    );
                }
            }
        }
        // Embedding 快照：深合并；oneapi.api_key 转为 api_key_configured，不回显明文。
        if let Some(emb_patch) = patch.get("embedding") {
            let mut clean = emb_patch.clone();
            if let Some(o) = clean.as_object_mut() {
                if let Some(oneapi) = o.get_mut("oneapi").and_then(|v| v.as_object_mut()) {
                    oneapi.remove("api_key");
                    oneapi.insert(
                        "api_key_configured".into(),
                        json!(!crate::config::settings::Settings::load_embedding()
                            .oneapi
                            .api_key
                            .is_empty()),
                    );
                }
            }
            if let Some(obj) = info.as_object_mut() {
                let existing = obj.entry("embedding").or_insert(json!({}));
                json_deep_merge(existing, &clean);
            }
        }
        // Models 快照：整体替换；每个 provider 的 api_key 转为 api_key_configured，不回显明文。
        if let Some(models_patch) = patch.get("models") {
            let mut clean = models_patch.clone();
            let effective = crate::config::settings::Settings::load_models();
            if let Some(provs) = clean.get_mut("providers").and_then(|v| v.as_array_mut()) {
                for p in provs.iter_mut() {
                    if let Some(o) = p.as_object_mut() {
                        let pid = o.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        o.insert(
                            "api_key_configured".into(),
                            json!(
                                effective
                                    .providers
                                    .iter()
                                    .any(|provider| provider.id == pid
                                        && !provider.api_key.is_empty())
                            ),
                        );
                        o.remove("api_key");
                    }
                }
            }
            if let Some(obj) = info.as_object_mut() {
                obj.insert("models".into(), clean);
            }
        }
        if let Some(admin_policies_patch) = patch.get("admin_policies") {
            if let Some(obj) = info.as_object_mut() {
                let existing = obj.entry("admin_policies").or_insert(json!({}));
                json_deep_merge(existing, admin_policies_patch);
            }
        }
    }

    // 4. Embedding 变更：按新配置热切换向量库并后台重建所有向量 KB 索引（免重启即时生效）。
    let mut embedding_reloaded = false;
    let mut reindex_queued = 0usize;
    let mut dim_note = String::new();
    let mut reload_err: Option<String> = None;
    if patch.get("embedding").is_some() {
        match hot_reload_embedding(&state).await {
            Ok((old_dim, new_dim, dim_changed, kbs)) => {
                embedding_reloaded = true;
                reindex_queued = kbs;
                dim_note = if dim_changed {
                    format!("向量维度 {old_dim} → {new_dim}")
                } else {
                    format!("维度 {new_dim} 不变")
                };
                // 同步脱敏快照的 active_dimension，使前端反显即时反映新生效维度。
                let mut info = state.config_info.write().await;
                if let Some(emb) = info.get_mut("embedding").and_then(|v| v.as_object_mut()) {
                    emb.insert("active_dimension".into(), json!(new_dim));
                }
            }
            Err(e) => reload_err = Some(e),
        }
    }

    // 4b. Models 变更：把最新注册表灌入 gateway（provider 端点 + model→provider 映射），
    //     增量热更、无需重启；未命中 model 时 gateway 自动回退单网关。
    if patch.get("models").is_some() {
        hot_reload_models(&state);
    }

    let final_info = state.config_info.read().await.clone();
    let message = if let Some(e) = &reload_err {
        format!("配置已持久化，但向量库热切换失败：{e}（重启后仍会按新配置生效）")
    } else if embedding_reloaded {
        format!(
            "配置已更新并即时生效（Embedding 已热切换，{dim_note}；已排队重建 {reindex_queued} 个向量库索引）。"
        )
    } else if persisted {
        "配置已更新并持久化生效。".to_string()
    } else {
        "配置已在运行时更新，但持久化失败。".to_string()
    };
    Json(json!({
        "status": "ok",
        "message": message,
        "persisted": persisted,
        "embedding_reloaded": embedding_reloaded,
        "reindex_queued": reindex_queued,
        "config": final_info,
    }))
    .into_response()
}

/// Models 注册表热更新：按最新持久化配置把启用的 provider 端点与 model→provider 映射
/// 灌入 gateway。整体替换、无需重启；移除 models 段后调用即回退单网关。
pub(crate) fn hot_reload_models(state: &Arc<AppState>) {
    let m = crate::config::settings::Settings::load_models();
    let mut provs: HashMap<String, crate::gateway::unified_gateway::ProviderRuntime> =
        HashMap::new();
    for p in m.providers.iter().filter(|p| p.enabled) {
        provs.insert(
            p.id.clone(),
            crate::gateway::unified_gateway::ProviderRuntime {
                base_url: p.base_url.clone(),
                api_key: p.api_key.clone(),
                timeout_seconds: p.timeout_seconds,
            },
        );
    }
    let mut mp: HashMap<String, String> = HashMap::new();
    for r in m.resources.iter().filter(|r| r.enabled) {
        if m.providers
            .iter()
            .any(|p| p.id == r.provider_id && p.enabled)
        {
            mp.insert(r.model.clone(), r.provider_id.clone());
        }
    }
    let provider_count = provs.len();
    let model_count = mp.len();
    state.gateway.set_provider_registry(provs);
    state.gateway.set_model_provider_mapping(mp);
    tracing::info!(
        provider_count,
        model_count,
        "models 注册表已热更新灌入 gateway"
    );
}

/// Embedding 配置热切换：按最新持久化配置重建 embedding 服务，原子换入新维度向量库，
/// 并后台重建所有向量 KB 索引（从原文台账重嵌入）。免进程重启即时生效。
/// 返回 (old_dim, new_dim, dim_changed, reindex_queued)。
pub(crate) async fn hot_reload_embedding(
    state: &Arc<AppState>,
) -> Result<(usize, usize, bool, usize), String> {
    let settings = crate::config::settings::Settings::load().unwrap_or_default();
    let embedding = settings.embedding.clone();
    let timeout = settings.agents.embedding_timeout_secs;
    let new_embed =
        crate::memory::embedding_service::create_embedding_service_from_config(&embedding, timeout);
    let new_dim = new_embed.dimension();
    let old_dim = state.vector_store.load_full().map(|s| s.dimension());
    let dim_changed = old_dim != Some(new_dim);
    let vdir = data_dir().join("vector_store");
    // 任何 embedding 变更都需换库重建（旧向量来自旧模型，语义不可混用；维度变更更是结构不兼容）。
    // 用全新目录打开，旧库整体移为 .bak-<ts> 便于回滚，同时避免与仍被引用的旧句柄争用同一文件。
    if vdir.exists() {
        let bak = data_dir().join(format!(
            "vector_store.bak-{}",
            chrono::Utc::now().format("%Y%m%d%H%M%S")
        ));
        std::fs::rename(&vdir, &bak).map_err(|e| format!("轮换旧向量目录失败: {e}"))?;
        tracing::info!("embedding 热切换：旧向量库已移至 {}", bak.display());
    }
    std::fs::create_dir_all(&vdir).map_err(|e| format!("创建向量目录失败: {e}"))?;
    let new_store =
        HyperspaceStore::open(&vdir, new_embed).map_err(|e| format!("打开新向量库失败: {e}"))?;
    state.vector_store.store(Some(Arc::new(new_store)));
    tracing::info!(old_dim = ?old_dim, new_dim, dim_changed, "embedding 已热切换，向量库原子换入");
    let reindex_queued = spawn_reindex_all_vector_kbs(state.clone()).await;
    Ok((old_dim.unwrap_or(0), new_dim, dim_changed, reindex_queued))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use tower::ServiceExt; // oneshot

    use super::super::api_gov::ApiUsageState;
    use super::super::iam::JwtClaims;
    use crate::api::http::TEST_ENV_LOCK;
    use crate::gateway::unified_gateway::UnifiedGateway;
    use crate::tools::prompt_registry::PromptRegistry;

    const FAKE_KEY: &str = "test-fake-gateway-key-278";

    /// 构造一个最小可用的 UnifiedGateway（不触网，仅满足 AppState 依赖）。
    fn test_gateway() -> UnifiedGateway {
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
        .unwrap()
    }

    #[tokio::test]
    async fn test_config_handler_returns_sanitized_config() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _platform_tenant = super::super::control_plane_route_auth_tests::EnvGuard::set(&[(
            super::super::iam::PLATFORM_ADMIN_TENANT_ENV,
            "test-tenant".into(),
        )]);
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        let previous_auth_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        let previous_gateway_key = std::env::var_os("AGENT_OS_GATEWAY_API_KEY");
        // 构造一个包含 api_key 的测试配置
        let test_config = json!({
            "version": "0.1.0-test",
            "gateway": {
                "base_url": "https://api.example.com",
                "default_model": "test-model",
                "max_retries": 5,
                "timeout_seconds": 120,
                "model_mapping": {"default": "test-model"},
                "api_key_configured": true
            },
            "api": {
                "http_addr": "0.0.0.0:8080",
                "grpc_addr": "0.0.0.0:50051",
                "metrics_port": 9090
            },
            "memory": {"l1_max_messages": 50, "l2_max_node_size": 1024},
            "agents": {"max_iterations": 20, "max_parallel_agents": 5}
        });

        // 构造测试用 AppState (最小化依赖)
        use crate::core::core_types::{CoreConfig, SemanticCore};

        let tmp = std::env::temp_dir().join(format!("agentos_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let test_core_config = CoreConfig {
            max_node_size: 1024,
            max_projection_size: 2048,
            l0_storage_path: tmp.to_str().unwrap().to_string(),
            event_buffer_size: 10,
            enable_metrics: false,
            eviction_config: None,
        };
        let core = Arc::new(SemanticCore::new(test_core_config).unwrap());
        let kg_store = Arc::new(oxigraph::store::Store::new().unwrap());
        let gateway = Arc::new(test_gateway());

        let state = Arc::new(AppState {
            core,
            gateway,
            kg_store,
            config_info: Arc::new(tokio::sync::RwLock::new(test_config.clone())),
            agents_info: serde_json::json!({ "count": 0, "agents": [] }),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
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
            api_usage: Arc::new(ApiUsageState::default()),
            online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
            online_corpus_queue_capacity: 10,
            invocations: crate::api::http::invocations::InvocationsRuntime::unavailable(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        });

        // 构造 Router 并发起 GET /api/v1/config 请求
        let router = Router::new()
            .route(
                "/api/v1/config",
                get(config_handler).put(update_config_handler),
            )
            .with_state(state.clone());

        let req = axum::http::Request::builder()
            .uri("/api/v1/config")
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    encode(
                        &Header::default(),
                        &JwtClaims {
                            sub: "config-test".to_string(),
                            tenant_id: "test-tenant".to_string(),
                            project_id: Some("test-project".to_string()),
                            roles: vec!["DA".to_string()],
                            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp()
                                as usize,
                        },
                        &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
                    )
                    .unwrap()
                ),
            )
            .body(axum::body::Body::empty())
            .unwrap();

        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 读取 body 并解析 JSON
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let config_res: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // 验证关键字段存在（且无明文 api_key）
        assert_eq!(config_res["version"], "0.1.0-test");
        assert_eq!(config_res["gateway"]["base_url"], "https://api.example.com");
        assert_eq!(config_res["gateway"]["default_model"], "test-model");
        assert_eq!(config_res["gateway"]["api_key_configured"], true);
        assert!(
            config_res["gateway"]["api_key"].is_null()
                || !config_res["gateway"]
                    .as_object()
                    .unwrap()
                    .contains_key("api_key")
        );
        assert!(config_res["sandbox"]["enabled"].is_boolean());
        assert!(config_res["sandbox"]["unshare_supported"].is_boolean());
        assert!(config_res["workspace"]["watch_enabled"].is_boolean());
        assert!(config_res["verify_first"]["enabled"].as_bool().unwrap());
        assert!(config_res["memory_scheduler"]["wired"].is_boolean());
        assert!(config_res["embedding_health"]["provider"].is_string());

        let put_config = |token: Option<String>, body: Value| {
            let mut request = axum::http::Request::builder()
                .method("PUT")
                .uri("/api/v1/config")
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            request
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };

        let anonymous = router
            .clone()
            .oneshot(put_config(
                None,
                json!({"gateway": {"base_url": "https://blocked.example"}}),
            ))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let token_for = |roles: Vec<&str>, project: Option<&str>| {
            encode(
                &Header::default(),
                &JwtClaims {
                    sub: "config-test".to_string(),
                    tenant_id: "test-tenant".to_string(),
                    project_id: project.map(str::to_string),
                    roles: roles.into_iter().map(str::to_string).collect(),
                    exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
                },
                &EncodingKey::from_secret(b"test-hs256-secret-at-least-32-bytes-long"),
            )
            .unwrap()
        };
        let unknown_field = router
            .clone()
            .oneshot(put_config(
                Some(token_for(vec!["DA"], Some("test-project"))),
                json!({"gateway": {"base_url": "https://blocked.example"}, "unexpected": true}),
            ))
            .await
            .unwrap();
        assert_eq!(unknown_field.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let non_da = router
            .clone()
            .oneshot(put_config(
                Some(token_for(vec!["PA"], Some("test-project"))),
                json!({"gateway": {"base_url": "https://blocked.example"}}),
            ))
            .await
            .unwrap();
        assert_eq!(non_da.status(), StatusCode::FORBIDDEN);
        let da = router
            .clone()
            .oneshot(put_config(
                Some(token_for(vec!["DA"], None)),
                json!({"gateway": {"base_url": "https://blocked.example"}}),
            ))
            .await
            .unwrap();
        assert_eq!(da.status(), StatusCode::FORBIDDEN);
        // A defaulted project must fail before touching either the runtime or disk.
        assert!(!tmp.join("config_override.json").exists());

        let rejected = router
            .clone()
            .oneshot(put_config(
                Some(token_for(vec!["DA"], Some("test-project"))),
                json!({"gateway": {"base_url": "https://blocked.example"}}),
            ))
            .await
            .unwrap();
        // #274: tightened
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert!(!tmp.join("config_override.json").exists());

        // An environment key not used to initialize this gateway is not effective.
        std::env::set_var("AGENT_OS_GATEWAY_API_KEY", FAKE_KEY);
        let empty = router
            .clone()
            .oneshot(put_config(
                // #274: platform admin required for config writes
                Some(token_for(vec!["PLATFORM_ADMIN"], Some("test-project"))),
                json!({"gateway": {"api_key": ""}}),
            ))
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::OK);
        assert!(!state.gateway.api_key_configured());
        let body = axum::body::to_bytes(empty.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let snapshot: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(snapshot["config"]["gateway"]["api_key_configured"], false);
        assert!(!String::from_utf8_lossy(&body).contains(FAKE_KEY));

        let updated = router
            .clone()
            .oneshot(put_config(
                Some(token_for(vec!["PLATFORM_ADMIN"], Some("test-project"))),
                json!({
                    "gateway": {
                        "base_url": "https://configured.example",
                        "api_key": FAKE_KEY
                    }
                }),
            ))
            .await
            .unwrap();
        // #274: tightened (success now requires a platform-admin token)
        assert_eq!(updated.status(), StatusCode::OK);
        assert!(state.gateway.api_key_configured());
        let body = axum::body::to_bytes(updated.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let snapshot: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(snapshot["config"]["gateway"]["api_key_configured"], true);
        assert!(!String::from_utf8_lossy(&body).contains(FAKE_KEY));
        let unchanged = router
            .clone()
            .oneshot(put_config(
                Some(token_for(vec!["PLATFORM_ADMIN"], Some("test-project"))),
                json!({"gateway": {"api_key": ""}}),
            ))
            .await
            .unwrap();
        assert_eq!(unchanged.status(), StatusCode::OK);
        let body = axum::body::to_bytes(unchanged.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let snapshot: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(snapshot["config"]["gateway"]["api_key_configured"], true);
        assert!(state.gateway.api_key_configured());
        assert!(!String::from_utf8_lossy(&body).contains(FAKE_KEY));
        let get = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/config")
                    .header(
                        "authorization",
                        format!("Bearer {}", token_for(vec!["DA"], Some("test-project"))),
                    )
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // #295: GET requires DA or platform admin; tightened to assert success.
        assert_eq!(get.status(), StatusCode::OK);
        let body = axum::body::to_bytes(get.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(FAKE_KEY));

        // Pre-existing guarantee, checked before any embedding/models key is
        // written below: no api_key at all in config_override.json.
        let override_contents = std::fs::read_to_string(tmp.join("config_override.json")).unwrap();
        assert!(!override_contents.contains(FAKE_KEY));
        assert!(
            !override_contents.contains("\"api_key\""),
            "gateway api_key must never persist in config_override.json"
        );

        // Empty provider and embedding keys preserve the persisted effective
        // keys, rather than replacing their snapshot flags with false.
        for (patch, path) in [
            (
                json!({"models": {"providers": [{"id": "test-provider", "base_url": "https://provider.example", "api_key": FAKE_KEY}], "resources": []}}),
                "models",
            ),
            (
                json!({"models": {"providers": [{"id": "test-provider", "base_url": "https://provider.example", "api_key": ""}], "resources": []}}),
                "models",
            ),
            (
                json!({"embedding": {"enabled": false, "oneapi": {"api_key": FAKE_KEY}}}),
                "embedding",
            ),
            (
                json!({"embedding": {"enabled": false, "oneapi": {"api_key": ""}}}),
                "embedding",
            ),
        ] {
            let response = router
                .clone()
                .oneshot(put_config(
                    Some(token_for(vec!["PLATFORM_ADMIN"], Some("test-project"))),
                    patch,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let snapshot: Value = serde_json::from_slice(&body).unwrap();
            let configured = if path == "models" {
                &snapshot["config"]["models"]["providers"][0]["api_key_configured"]
            } else {
                &snapshot["config"]["embedding"]["oneapi"]["api_key_configured"]
            };
            assert_eq!(configured, &json!(true));
            assert!(!String::from_utf8_lossy(&body).contains(FAKE_KEY));
        }

        let override_contents = std::fs::read_to_string(tmp.join("config_override.json")).unwrap();
        let persisted: Value = serde_json::from_str(&override_contents).unwrap();
        assert!(!persisted["gateway"].to_string().contains(FAKE_KEY));
        assert!(
            persisted["gateway"].get("api_key").is_none(),
            "gateway api_key must never persist in config_override.json"
        );

        // 清理
        if let Some(value) = previous_data_dir {
            std::env::set_var("AGENTOS_DATA_DIR", value);
        } else {
            std::env::remove_var("AGENTOS_DATA_DIR");
        }
        if let Some(value) = previous_auth_mode {
            std::env::set_var("AGENTOS_AUTH_MODE", value);
        } else {
            std::env::remove_var("AGENTOS_AUTH_MODE");
        }
        if let Some(value) = previous_gateway_key {
            std::env::set_var("AGENT_OS_GATEWAY_API_KEY", value);
        } else {
            std::env::remove_var("AGENT_OS_GATEWAY_API_KEY");
        }
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn scrub_secret_fields_recurses_and_preserves_non_secrets() {
        let mut value = json!({
            "API_KEY": "hidden",
            "nested": {
                "Secret": "hidden",
                "access_token": "hidden",
                "refresh_token": "hidden",
                "ApiKey": "hidden",
                "api_key_configured": true,
                "max_tokens": 100,
                "token_budget": 200,
                "base_url": "https://example.invalid"
            },
            "items": [
                {"password": "hidden", "client_secret": "hidden", "token_configured": false},
                {"custom_api_key": "hidden", "service_token": "hidden", "name": "kept"}
            ]
        });
        scrub_secret_fields(&mut value);
        assert_eq!(
            value,
            json!({
                "nested": {
                    "api_key_configured": true,
                    "max_tokens": 100,
                    "token_budget": 200,
                    "base_url": "https://example.invalid"
                },
                "items": [
                    {"token_configured": false},
                    {"name": "kept"}
                ]
            })
        );
    }

    #[test]
    fn scrub_secret_fields_normalizes_case_and_separators() {
        let mut value = json!({
            "accessToken": "hidden",
            "ACCESS-TOKEN": "hidden",
            "apiKey": "hidden",
            "api-key": "hidden",
            "clientSecret": "hidden",
            "secret_key": "hidden",
            "secretKey": "hidden",
            "private_key": "hidden",
            "PrivateKey": "hidden",
            "aws_access_key": "hidden",
            "accessKey": "hidden",
            "Authorization": "hidden",
            "proxy-authorization": "hidden",
            "credential": "hidden",
            "Credentials": {"user": "hidden", "pass": "hidden"},
            "service_credentials": ["hidden"],
            "bearerToken": "hidden",
            "apiKeyConfigured": true,
            "access-token-configured": false,
            "maxTokens": 100,
            "token_budget": 200,
            "tokenizer": "kept",
            "keyspace": "kept",
            "authorized": true,
            "baseUrl": "https://example.invalid"
        });
        scrub_secret_fields(&mut value);
        assert_eq!(
            value,
            json!({
                "apiKeyConfigured": true,
                "access-token-configured": false,
                "maxTokens": 100,
                "token_budget": 200,
                "tokenizer": "kept",
                "keyspace": "kept",
                "authorized": true,
                "baseUrl": "https://example.invalid"
            })
        );
    }
}
