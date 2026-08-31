//! 图片上传/代理与模型资源：连通性测试、provider 型号拉取、embedding 桥接激活。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装。

use std::sync::Arc;

use axum::{
    extract::{Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};

use super::config::{hot_reload_embedding, json_deep_merge, save_config_override};
use super::iam::UserIdentity;
use super::AppState;

/// 图片上传单文件体积上限（10MiB）。
pub(crate) const IMAGE_UPLOAD_MAX_BYTES: usize = 10 * 1024 * 1024;
/// 内联 data URI 阈值：仅小图（≤256KiB）随上传响应返回 data_uri，便于无回源出网场景。
const IMAGE_DATA_URI_MAX_BYTES: usize = 256 * 1024;

/// content_type → 受支持图片扩展名；None 表示非受支持图片类型（拒绝上传）。
fn image_ext_from_ct(ct: &str) -> Option<&'static str> {
    match ct.split(';').next().unwrap_or("").trim() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// 扩展名 → content_type（raw 代理回填响应头）。
fn image_ct_from_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

/// 按魔数嗅探图片扩展名（content_type 缺失/不可信时兜底）。
fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        Some("png")
    } else if bytes.len() >= 3 && &bytes[0..3] == b"\xff\xd8\xff" {
        Some("jpg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
        Some("gif")
    } else {
        None
    }
}


/// POST /api/v1/images/upload — 图片上传（multipart，复用 BlobStore）。
/// 字段：file（单个图片）。校验类型 ∈ {png,jpeg,webp,gif} 且 ≤10MiB。
/// 返回 { image_id, url, content_type, size, data_uri? }，url 供 image_url 直接引用。
pub(crate) async fn upload_image_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let blob = match &state.blob_store {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "BlobStore 未启用" })),
            )
        }
    };
    // 读取单个 file 字段（累积到内存）。
    let mut file: Option<(String, Vec<u8>)> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("multipart 解析失败: {e}") })),
                )
            }
        };
        let declared_ct = field.content_type().map(|s| s.to_string());
        match field.bytes().await {
            Ok(b) => file = Some((declared_ct.unwrap_or_default(), b.to_vec())),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("读取文件失败: {e}") })),
                )
            }
        }
    }
    let (declared_ct, bytes) = match file {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "未收到图片（字段名 file）" })),
            )
        }
    };
    if bytes.len() > IMAGE_UPLOAD_MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "图片超过 10MiB 上限" })),
        );
    }
    // 优先信任声明的 content_type；缺省时按内容嗅探。
    let ext = match image_ext_from_ct(&declared_ct).or_else(|| sniff_image_ext(&bytes)) {
        Some(e) => e,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "仅支持 png/jpeg/webp/gif 图片" })),
            )
        }
    };
    let ct = image_ct_from_ext(ext).to_string();
    let tenant = identity.tenant_id.clone();
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let key = format!("images/tenant:{tenant}/{uuid}.{ext}");
    if let Err(e) = blob.put(&key, &bytes, &ct).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("图片落盘失败: {e}") })),
        );
    }
    // image_id 编码 tenant 与文件名（tenant__uuid.ext），raw 代理据此还原受控 key。
    let image_id = format!("{tenant}__{uuid}.{ext}");
    let raw_url = format!("/api/v1/images/{image_id}/raw");
    let data_uri = if bytes.len() <= IMAGE_DATA_URI_MAX_BYTES {
        Some(format!("data:{};base64,{}", ct, STANDARD.encode(&bytes)))
    } else {
        None
    };
    (
        StatusCode::OK,
        Json(json!({
            "image_id": image_id,
            "url": raw_url,
            "content_type": ct,
            "size": bytes.len(),
            "data_uri": data_uri,
        })),
    )
}

/// GET /api/v1/images/:image_id/raw — 经 core 代理从 BlobStore 返回图片（不暴露 MinIO）。
/// image_id 形如 `<tenant>__<uuid>.<ext>`，还原受控 key `images/tenant:<tenant>/<uuid>.<ext>`。
pub(crate) async fn image_raw_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(image_id): axum::extract::Path<String>,
) -> Response {
    let (tenant, fname) = match image_id.split_once("__") {
        Some((t, f)) if !t.is_empty() && !f.is_empty() => (t, f),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "非法 image_id" })),
            )
                .into_response()
        }
    };
    // 防路径穿越：文件名段不得含分隔符或相对路径片段。
    if tenant.contains('/') || fname.contains('/') || fname.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "非法 image_id" })),
        )
            .into_response();
    }
    let ext = fname.rsplit('.').next().unwrap_or("");
    let ct = image_ct_from_ext(ext).to_string();
    let key = format!("images/tenant:{tenant}/{fname}");
    let blob = match &state.blob_store {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "BlobStore 未启用" })),
            )
                .into_response()
        }
    };
    match blob.get(&key).await {
        Ok(bytes) => (StatusCode::OK, [(header::CONTENT_TYPE, ct)], bytes).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "图片不存在" })),
        )
            .into_response(),
    }
}

/// 模型连通性测试请求体：resource_id 定位型号(+其 provider);provider_id 可显式覆盖。
#[derive(Deserialize)]
pub(crate) struct ModelTestRequest {
    #[serde(default)]
    provider_id: String,
    #[serde(default)]
    resource_id: String,
    /// chat|vision|embedding;缺省按 resource.modalities 首项或 "chat"。
    #[serde(default)]
    modality: String,
}

/// 32x32 纯白 PNG(base64),vision 连通性测试的最小图片载荷。
/// 注:部分 VL 模型(如 Qwen3-VL)要求图片每边 > 28px,且校验 PNG 完整性,故用合法 32x32 而非 1x1。
const TEST_PIXEL_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAJklEQVR42u3NMQ0AAAwDoPo33arYsQQMkB6LQCAQCAQCgUAg+BIMi1X0ptsIcT0AAAAASUVORK5CYII=";

/// POST /api/v1/models/test — provider/resource 连通性测试。
/// Body: { provider_id?, resource_id, modality? }。返回 { ok, http_status, latency_ms, dimension? }。
/// 绝不回显 api_key;错误信息不含 Authorization。
pub(crate) async fn test_model_handler(
    State(_state): State<Arc<AppState>>,
    Json(req): Json<ModelTestRequest>,
) -> impl IntoResponse {
    let m = crate::config::settings::Settings::load_models();
    let resource = m
        .resources
        .iter()
        .find(|r| r.id == req.resource_id)
        .cloned();
    let provider_id = if !req.provider_id.is_empty() {
        req.provider_id.clone()
    } else {
        resource
            .as_ref()
            .map(|r| r.provider_id.clone())
            .unwrap_or_default()
    };
    let provider = match m.providers.iter().find(|p| p.id == provider_id) {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "provider 未找到（检查 provider_id/resource_id）" })),
            )
        }
    };
    if provider.base_url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "provider.base_url 未配置" })),
        );
    }
    let model = resource
        .as_ref()
        .map(|r| r.model.clone())
        .unwrap_or_default();
    // modality 优先 body → resource.modalities 首项 → chat。
    let modality = if !req.modality.is_empty() {
        req.modality.clone()
    } else {
        resource
            .as_ref()
            .and_then(|r| r.modalities.first().cloned())
            .unwrap_or_else(|| "chat".to_string())
    };
    if model.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "resource.model 为空，无法测试" })),
        );
    }
    let base = crate::config::settings::normalize_api_base(&provider.base_url);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            provider.timeout_seconds.clamp(3, 60),
        ))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("HTTP 客户端构造失败: {e}") })),
            )
        }
    };
    let started = std::time::Instant::now();
    let (url, body) = match modality.as_str() {
        "embedding" => (
            format!("{base}/v1/embeddings"),
            json!({ "model": model, "input": "ping" }),
        ),
        "vision" => (
            format!("{base}/v1/chat/completions"),
            json!({
                "model": model,
                "max_tokens": 1,
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "ping" },
                        { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{TEST_PIXEL_PNG_B64}") } }
                    ]
                }]
            }),
        ),
        _ => (
            format!("{base}/v1/chat/completions"),
            json!({ "model": model, "max_tokens": 1, "messages": [{ "role": "user", "content": "ping" }] }),
        ),
    };
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => {
            let http_status = r.status().as_u16();
            let ok = r.status().is_success();
            let mut out = json!({ "ok": ok, "http_status": http_status, "latency_ms": latency_ms });
            // embedding 成功时回传维度;其余 modality 不解析 body。
            if ok && modality == "embedding" {
                if let Ok(v) = r.json::<Value>().await {
                    if let Some(dim) = v
                        .get("data")
                        .and_then(|d| d.as_array())
                        .and_then(|a| a.first())
                        .and_then(|e| e.get("embedding"))
                        .and_then(|e| e.as_array())
                        .map(|a| a.len())
                    {
                        out["dimension"] = json!(dim);
                    }
                }
            }
            (StatusCode::OK, Json(out))
        }
        // 错误信息仅取网络层原因(不含 Authorization/请求头)。
        Err(e) => (
            StatusCode::OK,
            Json(
                json!({ "ok": false, "http_status": 0, "latency_ms": latency_ms, "error": e.to_string() }),
            ),
        ),
    }
}

/// 自动拉取型号请求体：provider_id 命中已保存 provider（用其持久化端点/密钥）；
/// 也可内联 base_url/api_key（用于新增尚未保存的 provider）。
#[derive(Deserialize)]
pub(crate) struct ProviderModelsRequest {
    #[serde(default)]
    provider_id: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
}

/// POST /api/v1/providers/models — 拉取 provider 的 /v1/models 型号列表（自动加载）。
/// 返回 { ok, http_status, models:[{id, owned_by}] }。绝不回显 api_key；错误仅取网络层原因。
pub(crate) async fn provider_models_handler(
    State(_state): State<Arc<AppState>>,
    Json(req): Json<ProviderModelsRequest>,
) -> impl IntoResponse {
    // 端点/密钥解析：内联优先，缺省按 provider_id 回填持久化值。
    let (mut base_url, mut api_key, mut timeout) =
        (req.base_url.trim().to_string(), req.api_key.clone(), 60u64);
    if base_url.is_empty() || api_key.is_empty() {
        let m = crate::config::settings::Settings::load_models();
        if let Some(p) = m.providers.iter().find(|p| p.id == req.provider_id) {
            if base_url.is_empty() {
                base_url = p.base_url.clone();
            }
            if api_key.is_empty() {
                api_key = p.api_key.clone();
            }
            timeout = p.timeout_seconds;
        }
    }
    if base_url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "base_url 未配置（提供 base_url 或已保存的 provider_id）" })),
        );
    }
    let base = crate::config::settings::normalize_api_base(&base_url);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout.clamp(3, 60)))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("HTTP 客户端构造失败: {e}") })),
            )
        }
    };
    let url = format!("{base}/v1/models");
    let mut rb = client.get(&url).header("Content-Type", "application/json");
    if !api_key.is_empty() {
        rb = rb.header("Authorization", format!("Bearer {api_key}"));
    }
    match rb.send().await {
        Ok(r) => {
            let http_status = r.status().as_u16();
            let ok = r.status().is_success();
            let mut models: Vec<Value> = vec![];
            if ok {
                if let Ok(v) = r.json::<Value>().await {
                    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                        for item in arr {
                            if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                                models.push(json!({
                                    "id": id,
                                    "owned_by": item.get("owned_by").and_then(|x| x.as_str()).unwrap_or(""),
                                }));
                            }
                        }
                    }
                }
            }
            (
                StatusCode::OK,
                Json(json!({ "ok": ok, "http_status": http_status, "models": models })),
            )
        }
        Err(e) => (
            StatusCode::OK,
            Json(json!({ "ok": false, "http_status": 0, "models": [], "error": e.to_string() })),
        ),
    }
}

/// 向量桥接请求体：将 resource_id 指向的 embedding 型号设为生效向量服务。
#[derive(Deserialize)]
pub(crate) struct EmbeddingActivateRequest {
    resource_id: String,
}

/// POST /api/v1/embedding/activate — 把某个 embedding 型号（resource）桥接为生效向量服务。
/// 用 resource 的 provider 端点/密钥 + resource.model/dimension 写入 embedding(oneapi) 段，
/// 热切换向量库并后台重建索引。绝不回显 api_key。
pub(crate) async fn activate_embedding_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmbeddingActivateRequest>,
) -> impl IntoResponse {
    let m = crate::config::settings::Settings::load_models();
    let resource = match m.resources.iter().find(|r| r.id == req.resource_id) {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "resource 未找到" })),
            )
        }
    };
    if !resource.modalities.iter().any(|x| x == "embedding") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "该型号未标注 embedding 模态" })),
        );
    }
    let dimension = match resource.dimension {
        Some(d) if d > 0 => d,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "该向量型号未设置 dimension（维度）" })),
            )
        }
    };
    let provider = match m.providers.iter().find(|p| p.id == resource.provider_id) {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "provider 未找到" })),
            )
        }
    };
    if provider.base_url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "provider.base_url 未配置" })),
        );
    }
    if provider.api_key.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "provider 未配置 api_key，无法作为 OpenAI 兼容向量服务生效" })),
        );
    }
    // embedding 补丁(oneapi)：base_url/api_key 来自 provider，model/dimension 来自 resource。
    let patch = json!({
        "embedding": {
            "enabled": true,
            "provider": "oneapi",
            "oneapi": {
                "base_url": crate::config::settings::normalize_api_base(&provider.base_url),
                "api_key": provider.api_key,
                "model": resource.model,
                "dimension": dimension,
            }
        }
    });
    let persisted = save_config_override(&patch).is_ok();
    // 更新脱敏快照（去明文 key，转 api_key_configured）。
    {
        let mut info = state.config_info.write().await;
        if let Some(obj) = info.as_object_mut() {
            let mut clean = patch.get("embedding").cloned().unwrap_or_else(|| json!({}));
            if let Some(oneapi) = clean.get_mut("oneapi").and_then(|v| v.as_object_mut()) {
                let has = oneapi
                    .get("api_key")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                oneapi.insert("api_key_configured".into(), json!(has));
                oneapi.remove("api_key");
            }
            let existing = obj.entry("embedding").or_insert_with(|| json!({}));
            json_deep_merge(existing, &clean);
        }
    }
    // 热切换向量库 + 后台重建索引。
    let (message, embedding_reloaded, reindex_queued) = match hot_reload_embedding(&state).await {
        Ok((old_dim, new_dim, dim_changed, kbs)) => {
            {
                let mut info = state.config_info.write().await;
                if let Some(emb) = info.get_mut("embedding").and_then(|v| v.as_object_mut()) {
                    emb.insert("active_dimension".into(), json!(new_dim));
                }
            }
            let note = if dim_changed {
                format!("向量维度 {old_dim} → {new_dim}")
            } else {
                format!("维度 {new_dim} 不变")
            };
            (
                format!("已设为生效向量型号并热切换（{note}；已排队重建 {kbs} 个向量库索引）。"),
                true,
                kbs,
            )
        }
        Err(e) => (
            format!("配置已持久化，但向量库热切换失败：{e}"),
            false,
            0usize,
        ),
    };
    let final_info = state.config_info.read().await.clone();
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "message": message,
            "persisted": persisted,
            "embedding_reloaded": embedding_reloaded,
            "reindex_queued": reindex_queued,
            "config": final_info,
        })),
    )
}

