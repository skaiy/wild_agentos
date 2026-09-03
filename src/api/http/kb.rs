//! 知识包 / 知识库分类 / 向量与图谱知识库 CRUD、摄取、检索与重建。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；RAG/Public/OpenAI 见 `chat.rs`；
//! kg_import/query、图片与模型/embedding 热切换处理器留在 `mod.rs`。

use std::sync::Arc;

use axum::{
    extract::{Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::knowledge_graph::store::KnowledgeGraphStore;
use crate::knowledge_graph::types::{RdfQuad, RdfValue};
use crate::memory::hyperspace_store::HybridSearchFilter;
use crate::isolation::IsolationClaims;

use super::iam::UserIdentity;
use super::{data_dir, expand_iri, AppState};

/// 知识库分类的持久化文件路径。
fn kb_categories_store_path() -> std::path::PathBuf {
    data_dir().join("kb_categories.json")
}

/// 启动时从磁盘加载知识库分类；文件不存在或解析失败时返回空列表。
pub(crate) fn load_kb_categories() -> Vec<Value> {
    match std::fs::read_to_string(kb_categories_store_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 将知识库分类持久化到磁盘（pretty JSON）。
fn save_kb_categories(categories: &[Value]) -> std::io::Result<()> {
    let path = kb_categories_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(categories).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

/// 知识库注册表的持久化文件路径。
fn knowledge_bases_store_path() -> std::path::PathBuf {
    data_dir().join("knowledge_bases.json")
}

/// 启动时从磁盘加载知识库；文件不存在或解析失败时返回空列表。
pub(crate) fn load_knowledge_bases() -> Vec<Value> {
    match std::fs::read_to_string(knowledge_bases_store_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 将知识库持久化到磁盘（pretty JSON）。
fn save_knowledge_bases(bases: &[Value]) -> std::io::Result<()> {
    let path = knowledge_bases_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(bases).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

/// 知识包注册表的持久化文件路径。
fn knowledge_packs_store_path() -> std::path::PathBuf {
    data_dir().join("knowledge_packs.json")
}

/// 启动时加载知识包；文件不存在时用内置包种子化并落盘（Decision B：内置包亦可编辑）。
pub(crate) fn load_knowledge_packs() -> Vec<Value> {
    match std::fs::read_to_string(knowledge_packs_store_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => {
            // 种子化：把内置静态知识包写入 JSON，之后完全由 JSON 驱动、可编辑。
            let seed: Vec<Value> = crate::knowledge_graph::ontology_layer::knowledge_packs()
                .iter()
                .filter_map(|p| serde_json::to_value(p).ok())
                .collect();
            let _ = save_knowledge_packs(&seed);
            seed
        }
    }
}

/// 将知识包持久化到磁盘（pretty JSON）。
pub(crate) fn save_knowledge_packs(packs: &[Value]) -> std::io::Result<()> {
    let path = knowledge_packs_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(packs).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

/// 遍历所有向量 KB，逐个后台重建索引（从 BlobStore 原文台账）。返回排队重建的 KB 数。
/// 无 BlobStore 或无原文台账的 KB 将被跳过（存量向量已作废，需重新上传）。
pub(crate) async fn spawn_reindex_all_vector_kbs(state: Arc<AppState>) -> usize {
    if state.blob_store.is_none() {
        tracing::warn!("BlobStore 未启用，跳过自动重建（存量向量已作废，需重新上传原文）");
        return 0;
    }
    tracing::warn!("自动重建缺少已验证 isolation claims，跳过 BlobStore 读取");
    0
}

// ─── 知识库分类管理 CRUD ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct KbCategoryCreateRequest {
    pub name: String,
    pub description: Option<String>,
}

/// GET /api/v1/knowledge-packs — 返回知识包清单（内置种子 + 用户创建，均持久化于 data/knowledge_packs.json）。
///
/// 每个知识包关联 N 个知识库分类 / N 个图知识库 / N 个向量知识库，可被 Agent 挂载。
pub(crate) async fn list_knowledge_packs_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let packs = state.knowledge_packs.read().await.clone();
    Json(json!({ "count": packs.len(), "knowledge_packs": packs }))
}

#[derive(Deserialize)]
pub struct KnowledgePackCreateRequest {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub icon: Option<String>,
    pub color: Option<String>,
    #[serde(default)]
    pub category_ids: Vec<String>,
    #[serde(default)]
    pub graph_kb_ids: Vec<String>,
    #[serde(default)]
    pub vector_kb_ids: Vec<String>,
}

/// 校验知识包关联的分类/图库/向量库 id 均存在且类型匹配；返回 Err(错误消息)。
async fn validate_pack_refs(
    state: &AppState,
    category_ids: &[String],
    graph_kb_ids: &[String],
    vector_kb_ids: &[String],
) -> Result<(), String> {
    {
        let cats = state.kb_categories.read().await;
        for cid in category_ids {
            if !cats
                .iter()
                .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(cid.as_str()))
            {
                return Err(format!("分类不存在: {cid}"));
            }
        }
    }
    let bases = state.knowledge_bases.read().await;
    for gid in graph_kb_ids {
        let ok = bases.iter().any(|b| {
            b.get("id").and_then(|v| v.as_str()) == Some(gid.as_str())
                && b.get("kb_type").and_then(|v| v.as_str()) == Some("graph")
        });
        if !ok {
            return Err(format!("图知识库不存在或类型不符: {gid}"));
        }
    }
    for vid in vector_kb_ids {
        let ok = bases.iter().any(|b| {
            b.get("id").and_then(|v| v.as_str()) == Some(vid.as_str())
                && b.get("kb_type").and_then(|v| v.as_str()) == Some("vector")
        });
        if !ok {
            return Err(format!("向量知识库不存在或类型不符: {vid}"));
        }
    }
    Ok(())
}

/// POST /api/v1/knowledge-packs — 创建知识包并持久化。
pub(crate) async fn create_knowledge_pack_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KnowledgePackCreateRequest>,
) -> impl IntoResponse {
    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "name 不能为空" })),
        );
    }
    if let Err(e) = validate_pack_refs(
        &state,
        &req.category_ids,
        &req.graph_kb_ids,
        &req.vector_kb_ids,
    )
    .await
    {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    }
    let id = uuid::Uuid::new_v4().hyphenated().to_string();
    let pack = json!({
        "id": id,
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "version": req.version.unwrap_or_else(|| "1.0.0".to_string()),
        "icon": req.icon.unwrap_or_else(|| "Package".to_string()),
        "color": req.color.unwrap_or_else(|| "sky".to_string()),
        "named_graph": "",
        "vector_namespace": "",
        "ontology_domain": "",
        "stats": { "object_types": 0, "link_types": 0, "action_types": 0, "functions": 0 },
        "category_ids": req.category_ids,
        "graph_kb_ids": req.graph_kb_ids,
        "vector_kb_ids": req.vector_kb_ids,
        "builtin": false,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    let mut guard = state.knowledge_packs.write().await;
    guard.push(pack.clone());
    let _ = save_knowledge_packs(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "id": pack["id"], "status": "created", "knowledge_pack": pack })),
    )
}

/// PUT /api/v1/knowledge-packs/:id — 更新知识包（合并 patch，校验关联引用）。
pub(crate) async fn update_knowledge_pack_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(patch): Json<Value>,
) -> impl IntoResponse {
    let extract_ids = |k: &str| -> Vec<String> {
        patch
            .get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let cat = extract_ids("category_ids");
    let gks = extract_ids("graph_kb_ids");
    let vks = extract_ids("vector_kb_ids");
    if let Err(e) = validate_pack_refs(&state, &cat, &gks, &vks).await {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    }
    let mut guard = state.knowledge_packs.write().await;
    let found = guard
        .iter_mut()
        .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));
    match found {
        Some(pack) => {
            if let (Some(obj), Some(patch_obj)) = (pack.as_object_mut(), patch.as_object()) {
                for (k, v) in patch_obj {
                    if k == "id" || k == "created_at" || k == "builtin" {
                        continue;
                    }
                    obj.insert(k.clone(), v.clone());
                }
                obj.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
            }
            let updated = pack.clone();
            let _ = save_knowledge_packs(&guard);
            (
                StatusCode::OK,
                Json(json!({ "status": "updated", "knowledge_pack": updated })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "knowledge pack not found", "id": id })),
        ),
    }
}

/// DELETE /api/v1/knowledge-packs/:id — 删除知识包并持久化。
pub(crate) async fn delete_knowledge_pack_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let mut guard = state.knowledge_packs.write().await;
    let before = guard.len();
    guard.retain(|p| p.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    if guard.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "knowledge pack not found", "id": id })),
        );
    }
    let _ = save_knowledge_packs(&guard);
    (
        StatusCode::OK,
        Json(json!({ "status": "deleted", "id": id })),
    )
}

/// GET /api/v1/kb/categories — 返回全部知识库分类
pub(crate) async fn list_kb_categories_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let categories = state.kb_categories.read().await.clone();
    Json(json!({ "count": categories.len(), "categories": categories }))
}

/// POST /api/v1/kb/categories — 创建知识库分类并持久化
pub(crate) async fn create_kb_category_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KbCategoryCreateRequest>,
) -> impl IntoResponse {
    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "name 不能为空" })),
        );
    }
    let category = json!({
        "id": uuid::Uuid::new_v4().hyphenated().to_string(),
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    let id = category["id"].as_str().unwrap_or("").to_string();
    let mut guard = state.kb_categories.write().await;
    guard.push(category.clone());
    let _ = save_kb_categories(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": "created", "category": category })),
    )
}

/// PUT /api/v1/kb/categories/:id — 更新知识库分类并持久化
pub(crate) async fn update_kb_category_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(patch): Json<Value>,
) -> impl IntoResponse {
    let mut guard = state.kb_categories.write().await;
    let found = guard
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));
    match found {
        Some(category) => {
            if let (Some(obj), Some(patch_obj)) = (category.as_object_mut(), patch.as_object()) {
                for (k, v) in patch_obj {
                    if k == "id" || k == "created_at" {
                        continue;
                    }
                    obj.insert(k.clone(), v.clone());
                }
                obj.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
            }
            let updated = category.clone();
            let _ = save_kb_categories(&guard);
            (
                StatusCode::OK,
                Json(json!({ "status": "updated", "category": updated })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "category not found", "id": id })),
        ),
    }
}

/// DELETE /api/v1/kb/categories/:id — 删除知识库分类并持久化
pub(crate) async fn delete_kb_category_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let mut guard = state.kb_categories.write().await;
    let before = guard.len();
    guard.retain(|c| c.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    if guard.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "category not found", "id": id })),
        );
    }
    let _ = save_kb_categories(&guard);
    (
        StatusCode::OK,
        Json(json!({ "status": "deleted", "id": id })),
    )
}

// ─── 知识库（向量/图）管理 ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct KnowledgeBaseCreateRequest {
    pub name: String,
    pub description: Option<String>,
    /// "vector" | "graph"
    pub kb_type: String,
    pub category_id: Option<String>,
}

/// Catalog entries are visible only within their verified tenant/project scope.
///
/// Records without this scope are historical and deliberately remain outside
/// the claims-scoped catalog rather than being migrated on read.
fn kb_belongs_to_claims(kb: &Value, claims: &IsolationClaims) -> bool {
    kb.get("tenant_id").and_then(Value::as_str) == Some(claims.tenant_id())
        && kb.get("project_id").and_then(Value::as_str) == Some(claims.project_id())
}

/// GET /api/v1/kb/bases — 返回全部知识库
pub(crate) async fn list_knowledge_bases_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "verified isolation claims required for KB catalog" }))),
    };
    let bases: Vec<Value> = state.knowledge_bases.read().await.iter()
        .filter(|kb| kb_belongs_to_claims(kb, claims))
        .cloned()
        .collect();
    (StatusCode::OK, Json(json!({ "count": bases.len(), "bases": bases })))
}

/// POST /api/v1/kb/bases — 创建知识库（向量/图），图类型在 oxigraph 落盘命名图元数据
pub(crate) async fn create_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<KnowledgeBaseCreateRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "verified isolation claims required for KB catalog" }))),
    };
    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "name 不能为空" })),
        );
    }
    if req.kb_type != "vector" && req.kb_type != "graph" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "kb_type 必须为 vector 或 graph" })),
        );
    }
    // 校验分类存在（若指定）
    if let Some(cat_id) = req.category_id.as_deref() {
        let exists = state
            .kb_categories
            .read()
            .await
            .iter()
            .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(cat_id));
        if !exists {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "category_id 不存在", "category_id": cat_id })),
            );
        }
    }

    let kb_id = uuid::Uuid::new_v4().hyphenated().to_string();
    // Graph names are minted from verified claims; request data cannot select
    // a catalog metadata write target.
    let graph_iri = if req.kb_type == "graph" {
        let iri = match claims.graph_iri() {
            Ok(iri) => iri,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("invalid verified graph scope: {e}") }))),
        };
        let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
            Ok(kg) => kg,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))),
        };
        if let Err(e) = kg.upsert_kb_catalog_metadata_for_claims(claims, &kb_id, &req.name, "graph") {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("命名图初始化失败: {e}") })),
            );
        }
        let _ = state.kg_store.flush();
        Some(iri)
    } else {
        None
    };

    // 向量类型：分配隔离命名空间，供运行时向量检索按 namespace 过滤。
    let vector_namespace = if req.kb_type == "vector" {
        match claims.vector_namespace() {
            Ok(namespace) => namespace,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("invalid verified vector namespace: {e}") }))),
        }
    } else {
        String::new()
    };
    let kb = json!({
        "id": kb_id,
        "name": req.name,
        "description": req.description.unwrap_or_default(),
        "kb_type": req.kb_type,
        "category_id": req.category_id.unwrap_or_default(),
        "graph": graph_iri.clone().unwrap_or_default(),
        "vector_namespace": vector_namespace,
        "tenant_id": claims.tenant_id(),
        "project_id": claims.project_id(),
        "created_by": claims.actor_id(),
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    let mut guard = state.knowledge_bases.write().await;
    guard.push(kb.clone());
    let _ = save_knowledge_bases(&guard);
    (
        StatusCode::CREATED,
        Json(json!({ "id": kb["id"], "status": "created", "base": kb })),
    )
}

/// DELETE /api/v1/kb/bases/:id — 删除 claims-scoped 知识库目录条目。
pub(crate) async fn delete_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "verified isolation claims required for KB catalog" }))),
    };
    let mut guard = state.knowledge_bases.write().await;
    let removed = guard
        .iter()
        .find(|b| {
            b.get("id").and_then(|v| v.as_str()) == Some(id.as_str())
                && kb_belongs_to_claims(b, claims)
        })
        .cloned();
    let removed = match removed {
        Some(removed) => removed,
        None => return (StatusCode::NOT_FOUND, Json(json!({ "error": "knowledge base not found", "id": id }))),
    };
    if removed.get("kb_type").and_then(Value::as_str) == Some("graph") {
        let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
            Ok(kg) => kg,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))),
        };
        if let Err(e) = kg.delete_kb_catalog_metadata_for_claims(claims, &id) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("KB catalog metadata delete failed: {e}") })),
            );
        }
        let _ = state.kg_store.flush();
    }
    guard.retain(|b| {
        !(b.get("id").and_then(|v| v.as_str()) == Some(id.as_str())
            && kb_belongs_to_claims(b, claims))
    });
    let _ = save_knowledge_bases(&guard);
    (
        StatusCode::OK,
        Json(json!({ "status": "deleted", "id": id })),
    )
}

#[derive(Deserialize)]
pub struct KnowledgeBaseUpdateRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub category_id: Option<String>,
}

/// PUT /api/v1/kb/bases/:id — 更新知识库可变元数据（name/description/category_id）。
/// 不改 kb_type/graph/vector_namespace/tenant；图类型改名时同步命名图 kbName 元三元组。
pub(crate) async fn update_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<KnowledgeBaseUpdateRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "verified isolation claims required for KB catalog" }))),
    };
    // 校验分类存在（若指定非空）
    if let Some(cat_id) = req.category_id.as_deref().filter(|s| !s.is_empty()) {
        let exists = state
            .kb_categories
            .read()
            .await
            .iter()
            .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(cat_id));
        if !exists {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "category_id 不存在", "category_id": cat_id })),
            );
        }
    }

    let (updated, is_graph, graph_iri, name_changed) = {
        let mut guard = state.knowledge_bases.write().await;
        let kb = match guard
            .iter_mut()
            .find(|b| {
                b.get("id").and_then(|v| v.as_str()) == Some(id.as_str())
                    && kb_belongs_to_claims(b, claims)
            })
        {
            Some(k) => k,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "knowledge base not found", "id": id })),
                )
            }
        };
        let mut name_changed: Option<String> = None;
        if let Some(name) = req.name {
            let name = name.trim().to_string();
            if name.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "name 不能为空" })),
                );
            }
            kb["name"] = json!(name);
            name_changed = Some(name);
        }
        if let Some(desc) = req.description {
            kb["description"] = json!(desc);
        }
        if let Some(cat) = req.category_id {
            kb["category_id"] = json!(cat);
        }
        kb["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
        let is_graph = kb.get("kb_type").and_then(|v| v.as_str()) == Some("graph");
        let graph_iri = kb
            .get("graph")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let updated = kb.clone();
        let _ = save_knowledge_bases(&guard);
        (updated, is_graph, graph_iri, name_changed)
    };

    // 图类型改名：同步命名图 kbName 元三元组
    if is_graph && !graph_iri.is_empty() {
        if let Some(new_name) = name_changed {
            let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
                Ok(kg) => kg,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))),
            };
            if let Err(e) = kg.upsert_kb_catalog_metadata_for_claims(claims, &id, &new_name, "graph") {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("KB catalog metadata update failed: {e}") })),
                );
            }
            let _ = state.kg_store.flush();
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "status": "updated", "id": id, "base": updated })),
    )
}

/// GET /api/v1/kb/bases/:id/stats — 单个知识库统计。
/// 图类型：命名图三元组精确计数（含 kbName/kbType 2 条元三元组）；
/// 向量类型：返回 namespace；chunks 暂无按命名空间枚举接口，返回 null 并附说明。
pub(crate) async fn knowledge_base_stats_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for graph storage" })),
            )
        }
    };
    let kb = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
    };
    let kb = match kb {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "knowledge base not found", "id": id })),
            )
        }
    };
    let kb_type = kb
        .get("kb_type")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mut stats = json!({
        "id": id,
        "name": kb.get("name").cloned().unwrap_or(Value::Null),
        "kb_type": kb_type,
        "category_id": kb.get("category_id").cloned().unwrap_or(Value::Null),
        "created_at": kb.get("created_at").cloned().unwrap_or(Value::Null),
        "updated_at": kb.get("updated_at").cloned().unwrap_or(Value::Null),
    });
    if kb_type == "graph" {
        let graph_iri = match claims.graph_iri() {
            Ok(graph_iri) => graph_iri,
            Err(e) => {
                tracing::warn!("KB stats invalid verified graph scope: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "invalid verified graph scope" })),
                );
            }
        };
        let triples = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
            Ok(kg) => {
                let q = "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }";
                match kg.query_sparql_for_claims(claims, q) {
                    Ok(rows) => rows
                        .first()
                        .and_then(|r| r.get("?c"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|n| json!(n))
                        .unwrap_or(json!(0)),
                    Err(e) => {
                        tracing::warn!("KB stats graph count failed: {}", e);
                        json!(null)
                    }
                }
            }
            Err(e) => {
                tracing::warn!("KB stats KG store failed: {}", e);
                json!(null)
            }
        };
        stats["graph"] = json!(graph_iri);
        stats["triples"] = triples;
    } else {
        stats["vector_namespace"] = kb.get("vector_namespace").cloned().unwrap_or(Value::Null);
        stats["chunks"] = json!(null);
        stats["note"] = json!("按命名空间的向量条目计数暂未开放枚举接口");
    }
    (StatusCode::OK, Json(stats))
}

#[derive(Deserialize)]
pub struct IngestRequest {
    #[serde(default)]
    pub texts: Vec<String>,
    pub text: Option<String>,
}

/// 简单按字符长度切块（按 char 切，避免破坏 UTF-8 边界；中文友好）。
fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        let t = text.trim().to_string();
        return if t.is_empty() { vec![] } else { vec![t] };
    }
    chars
        .chunks(max_chars)
        .map(|c| c.iter().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// POST /api/v1/kb/bases/:id/ingest — 向向量知识库写入文本（分块→embedding→写入向量库）。
pub(crate) async fn ingest_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<IngestRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for vector storage" })),
            )
        }
    };
    let kb = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
    };
    let kb = match kb {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "knowledge base not found", "id": id })),
            )
        }
    };
    if kb.get("kb_type").and_then(|v| v.as_str()) != Some("vector") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "仅向量知识库支持 ingest" })),
        );
    }
    let namespace = match claims.vector_namespace() {
        Ok(namespace) => namespace,
        Err(e) => {
            tracing::warn!("KB ingest invalid verified vector namespace: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "invalid verified vector namespace" })),
            );
        }
    };
    let store = match state.vector_store.load_full() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "向量库未启用（embedding 初始化失败）" })),
            )
        }
    };
    let mut texts: Vec<String> = req.texts;
    if let Some(t) = req.text {
        if !t.trim().is_empty() {
            texts.push(t);
        }
    }
    if texts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "texts/text 不能为空" })),
        );
    }
    let tags = Vec::new();
    let mut count = 0usize;
    for text in &texts {
        for chunk in chunk_text(text, 500) {
            let iri = format!("chunk/{}", uuid::Uuid::new_v4().hyphenated());
            match store.upsert_with_claims(claims, &iri, &chunk, &tags).await {
                Ok(_) => count += 1,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("写入失败: {e}") })),
                    )
                }
            }
        }
    }
    (
        StatusCode::OK,
        Json(json!({ "status": "ingested", "chunks": count, "namespace": namespace })),
    )
}

#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u64>,
}

/// POST /api/v1/kb/bases/:id/search — 对向量知识库做语义相似检索（供 admin/QA 直接验证召回）。
pub(crate) async fn search_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SearchRequest>,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for vector storage" })),
            )
        }
    };
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "query 不能为空" })),
        );
    }
    let kb = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
    };
    let kb = match kb {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "knowledge base not found", "id": id })),
            )
        }
    };
    if kb.get("kb_type").and_then(|v| v.as_str()) != Some("vector") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "仅向量知识库支持 search" })),
        );
    }
    let namespace = match claims.vector_namespace() {
        Ok(namespace) => namespace,
        Err(e) => {
            tracing::warn!("KB search invalid verified vector namespace: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "invalid verified vector namespace" })),
            );
        }
    };
    let store = match state.vector_store.load_full() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "向量库未启用（embedding 初始化失败）" })),
            )
        }
    };
    let limit = req.limit.unwrap_or(5).clamp(1, 20);
    let filter = HybridSearchFilter::new();
    match store
        .search_with_claims(claims, &query, &filter, limit)
        .await
    {
        Ok(hits) => {
            let results: Vec<Value> = hits
                .iter()
                .map(|h| json!({ "text": h.text, "score": h.score, "iri": h.iri, "tags": h.tags }))
                .collect();
            (
                StatusCode::OK,
                Json(json!({ "count": results.len(), "namespace": namespace, "results": results })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("检索失败: {e}") })),
        ),
    }
}

/// KB 上传/导入单文件体积上限（60MB，覆盖前端提示的 50MB/文件 + 编码开销）。
pub(crate) const KB_UPLOAD_MAX_BYTES: usize = 60 * 1024 * 1024;

/// 依扩展名判断向量库上传文件是否为当前可解析的纯文本类型。
/// 返回 Some(()) 表示直读文本；None 表示暂无解析器（PDF/Word 等），走诚实降级。
fn kb_text_ext(name: &str) -> Option<()> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".txt")
        || lower.ends_with(".md")
        || lower.ends_with(".markdown")
        || lower.ends_with(".csv")
        || lower.ends_with(".log")
        || lower.ends_with(".json")
        || lower.ends_with(".jsonl")
    {
        Some(())
    } else {
        None
    }
}

/// 依扩展名推断原文 Content-Type，用于对象存储写入（未知类型回退 octet-stream）。
fn kb_content_type(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let ct = if lower.ends_with(".txt") || lower.ends_with(".log") {
        "text/plain; charset=utf-8"
    } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
        "text/markdown; charset=utf-8"
    } else if lower.ends_with(".csv") {
        "text/csv; charset=utf-8"
    } else if lower.ends_with(".json") || lower.ends_with(".jsonl") {
        "application/json; charset=utf-8"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".docx") {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    } else if lower.ends_with(".doc") {
        "application/msword"
    } else {
        "application/octet-stream"
    };
    ct.to_string()
}

/// POST /api/v1/kb/bases/:id/upload — 向量库文件上传摄取（multipart）。
/// 字段：file（可多次，文件）、chunk_size、chunk_strategy、min_importance。
/// TXT/MD 等纯文本直解析→分块→embedding→写入；PDF/Word 暂无解析器，逐文件诚实标注 skipped。
pub(crate) async fn upload_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for blob storage" })),
            )
        }
    };
    let kb = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
    };
    let kb = match kb {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "knowledge base not found", "id": id })),
            )
        }
    };
    if kb.get("kb_type").and_then(|v| v.as_str()) != Some("vector") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "仅向量知识库支持文件上传" })),
        );
    }
    let namespace = match claims.vector_namespace() {
        Ok(namespace) => namespace,
        Err(e) => {
            tracing::warn!("KB upload invalid verified vector namespace: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "invalid verified vector namespace" })),
            );
        }
    };
    let store = match state.vector_store.load_full() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "向量库未启用（embedding 初始化失败）" })),
            )
        }
    };

    // 逐字段读取：文件累积到内存，参数落到局部变量。
    let mut chunk_size: usize = 500;
    let mut chunk_strategy = String::from("fixed");
    let mut min_importance: f32 = 0.5;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
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
        let fname = field.name().unwrap_or_default().to_string();
        let filename = field.file_name().map(|s| s.to_string());
        match fname.as_str() {
            "chunk_size" => {
                if let Ok(t) = field.text().await {
                    if let Ok(n) = t.trim().parse::<usize>() {
                        chunk_size = n.clamp(50, 4000);
                    }
                }
            }
            "chunk_strategy" => {
                if let Ok(t) = field.text().await {
                    chunk_strategy = t.trim().to_string();
                }
            }
            "min_importance" => {
                if let Ok(t) = field.text().await {
                    if let Ok(v) = t.trim().parse::<f32>() {
                        min_importance = v.clamp(0.0, 1.0);
                    }
                }
            }
            _ => {
                let name = filename.unwrap_or_else(|| fname.clone());
                match field.bytes().await {
                    Ok(b) => files.push((name, b.to_vec())),
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("读取文件失败: {e}") })),
                        )
                    }
                }
            }
        }
    }
    if files.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "未收到任何文件（字段名 file）" })),
        );
    }
    // 当前仅实现固定长度分块；其余策略降级为 fixed 并在响应标注。
    let applied_strategy = "fixed";

    let base_tags = Vec::new();
    let blob = state.blob_store.clone();
    let mut file_results: Vec<Value> = Vec::new();
    let mut ledger_entries: Vec<Value> = Vec::new();
    let mut total_chunks = 0usize;
    for (name, bytes) in files {
        // 内容寻址：doc_id = 原文 sha256，既用于去重也作为重建索引的稳定键。
        let doc_id = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hex::encode(hasher.finalize())
        };
        let content_type = kb_content_type(&name);
        let size = bytes.len();
        // ① 原文落盘：无论能否解析都持久化，为重建索引/预览/溯源留底。
        let blob_key = format!("kb/{id}/{doc_id}");
        let mut blob_ref = Value::Null;
        let mut persist_err: Option<String> = None;
        if let Some(b) = &blob {
            match b.put(claims, &blob_key, &bytes, &content_type).await {
                Ok(_) => blob_ref = json!({ "backend": b.backend(), "key": blob_key }),
                Err(e) => persist_err = Some(format!("原文落盘失败: {e}")),
            }
        } else {
            persist_err = Some("BlobStore 未启用，原文未持久化".to_string());
        }
        // ② 解析 + 分块 + 向量化（chunk 打 doc:<doc_id> 标签，chunk_iris 入台账供重建删除）。
        let parseable = kb_text_ext(&name).is_some();
        let mut file_chunks = 0usize;
        let mut chunk_iris: Vec<String> = Vec::new();
        let mut file_err: Option<String> = None;
        if parseable {
            let mut doc_tags = base_tags.clone();
            doc_tags.push(format!("doc:{}", doc_id));
            let text = String::from_utf8_lossy(&bytes).to_string();
            for chunk in chunk_text(&text, chunk_size) {
                let iri = format!("chunk/{}", uuid::Uuid::new_v4().hyphenated());
                match store
                    .upsert_with_claims(claims, &iri, &chunk, &doc_tags)
                    .await
                {
                    Ok(_) => {
                        file_chunks += 1;
                        total_chunks += 1;
                        chunk_iris.push(iri);
                    }
                    Err(e) => {
                        file_err = Some(format!("写入失败: {e}"));
                        break;
                    }
                }
            }
        } else {
            file_err = Some(
                "暂无该类型解析器（PDF/Word 等），原文已留底，接入解析器后可重建索引".to_string(),
            );
        }
        // ③ 台账状态：ready(已向量化) / stored(仅留底未向量化) / failed(向量化出错)。
        let status = if !parseable {
            "stored"
        } else if file_err.is_some() {
            "failed"
        } else {
            "ready"
        };
        let mut entry = json!({ "name": name, "chunks": file_chunks, "doc_id": doc_id });
        entry["persisted"] = json!(!blob_ref.is_null());
        if let Some(e) = &file_err {
            entry["skipped_reason"] = json!(e);
        }
        if let Some(e) = &persist_err {
            entry["persist_warning"] = json!(e);
        }
        file_results.push(entry);
        ledger_entries.push(json!({
            "doc_id": doc_id,
            "filename": name,
            "size": size,
            "content_type": content_type,
            "blob_ref": blob_ref,
            "status": status,
            "chunks": file_chunks,
            "chunk_iris": chunk_iris,
            "chunk_size": chunk_size,
            "chunk_strategy": applied_strategy,
            "min_importance": min_importance,
            "uploaded_by": identity.user_id,
            "uploaded_at": chrono::Utc::now().to_rfc3339(),
        }));
    }
    // 将台账合并进 KB.documents（按 doc_id 去重覆盖）并持久化。
    if !ledger_entries.is_empty() {
        let mut guard = state.knowledge_bases.write().await;
        if let Some(obj) = guard
            .iter_mut()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .and_then(|b| b.as_object_mut())
        {
            let mut docs: Vec<Value> = obj
                .get("documents")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for ne in &ledger_entries {
                let ndoc = ne.get("doc_id").and_then(|v| v.as_str());
                docs.retain(|d| d.get("doc_id").and_then(|v| v.as_str()) != ndoc);
                docs.push(ne.clone());
            }
            obj.insert("documents".into(), json!(docs));
            obj.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
        }
        let _ = save_knowledge_bases(&guard);
    }
    (
        StatusCode::OK,
        Json(json!({
            "status": "uploaded",
            "namespace": namespace,
            "total_chunks": total_chunks,
            "chunk_size": chunk_size,
            "chunk_strategy_requested": chunk_strategy,
            "chunk_strategy_applied": applied_strategy,
            "files": file_results,
        })),
    )
}

/// GET /api/v1/kb/bases/:id/documents — 返回该向量库的原文档台账（documents）。
pub(crate) async fn list_kb_documents_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let kb = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
    };
    match kb {
        Some(k) => {
            let docs = k.get("documents").cloned().unwrap_or_else(|| json!([]));
            let count = docs.as_array().map(|a| a.len()).unwrap_or(0);
            (
                StatusCode::OK,
                Json(json!({
                    "count": count,
                    "documents": docs,
                    "reindex_status": k.get("reindex_status").cloned().unwrap_or(Value::Null),
                    "reindexed_at": k.get("reindexed_at").cloned().unwrap_or(Value::Null),
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "knowledge base not found", "id": id })),
        ),
    }
}

/// RFC 5987 编码（Content-Disposition filename* 用），保留 A-Za-z0-9-._~，其余按 UTF-8 百分号编码。
fn rfc5987_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{:02X}", c));
        }
    }
    out
}

/// GET /api/v1/kb/bases/:id/documents/:doc_id/raw — 经 core 代理从 BlobStore 返回原文（不暴露 MinIO）。
pub(crate) async fn kb_document_raw_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path((id, doc_id)): axum::extract::Path<(String, String)>,
) -> Response {
    let doc = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .and_then(|k| k.get("documents").and_then(|v| v.as_array()).cloned())
            .and_then(|docs| {
                docs.into_iter()
                    .find(|d| d.get("doc_id").and_then(|v| v.as_str()) == Some(doc_id.as_str()))
            })
    };
    let doc = match doc {
        Some(d) => d,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "document not found", "doc_id": doc_id })),
            )
                .into_response()
        }
    };
    let has_blob_ref = doc
        .get("blob_ref")
        .and_then(|b| b.get("key"))
        .and_then(|v| v.as_str())
        .is_some_and(|key| !key.is_empty());
    if !has_blob_ref {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "该文档原文未持久化（BlobStore 未启用时上传）" })),
        )
            .into_response();
    }
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for blob storage" })),
            )
                .into_response()
        }
    };
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
    let key = format!("kb/{id}/{doc_id}");
    match blob.get(claims, &key).await {
        Ok(bytes) => {
            let ct = doc
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream")
                .to_string();
            let fname = doc
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let disp = format!("inline; filename*=UTF-8''{}", rfc5987_encode(fname));
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ct),
                    (header::CONTENT_DISPOSITION, disp),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("读取原文失败: {e}") })),
        )
            .into_response(),
    }
}

/// POST /api/v1/kb/bases/:id/reindex — 按当前 embedding/分块重建向量索引（异步）。
/// 从 documents 台账拉原文 → 删旧 chunk → 重新分块 embedding 写新 → 更新台账与状态。
pub(crate) async fn reindex_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let claims = match identity.isolation_claims() {
        Some(claims) => claims.clone(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for blob storage" })),
            )
                .into_response()
        }
    };
    let kb = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
    };
    let kb = match kb {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "knowledge base not found", "id": id })),
            )
                .into_response()
        }
    };
    if kb.get("kb_type").and_then(|v| v.as_str()) != Some("vector") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "仅向量知识库支持重建索引" })),
        )
            .into_response();
    }
    if state.vector_store.load().is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "向量库未启用（embedding 初始化失败）" })),
        )
            .into_response();
    }
    if state.blob_store.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "BlobStore 未启用，无原文可重建" })),
        )
            .into_response();
    }
    let docs: Vec<Value> = kb
        .get("documents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if docs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "无原文档台账，无法重建（请重新上传后再试）" })),
        )
            .into_response();
    }
    // 标记 reindexing 并落盘，避免并发重复触发。
    {
        let mut guard = state.knowledge_bases.write().await;
        if let Some(o) = guard
            .iter_mut()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .and_then(|b| b.as_object_mut())
        {
            o.insert("reindex_status".into(), json!("reindexing"));
            o.insert(
                "reindex_started_at".into(),
                json!(chrono::Utc::now().to_rfc3339()),
            );
        }
        let _ = save_knowledge_bases(&guard);
    }
    let doc_count = docs.len();
    let state2 = state.clone();
    let id2 = id.clone();
    tokio::spawn(async move {
        run_kb_reindex(state2, claims, id2, docs).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "reindexing", "id": id, "documents": doc_count })),
    )
        .into_response()
}

/// 后台重建任务：逐文档从 BlobStore 拉原文，删旧 chunk 后按当前 embedding 重新入库，回写台账。
async fn run_kb_reindex(
    state: Arc<AppState>,
    claims: crate::isolation::IsolationClaims,
    id: String,
    docs: Vec<Value>,
) {
    let store = match state.vector_store.load_full() {
        Some(s) => s,
        None => return,
    };
    let blob = match &state.blob_store {
        Some(b) => b.clone(),
        None => return,
    };
    let mut updated: Vec<Value> = Vec::new();
    let mut any_failed = false;
    for mut doc in docs {
        let doc_id = doc
            .get("doc_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let filename = doc
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let chunk_size = doc
            .get("chunk_size")
            .and_then(|v| v.as_u64())
            .unwrap_or(500) as usize;
        let has_blob_ref = doc
            .get("blob_ref")
            .and_then(|b| b.get("key"))
            .and_then(|v| v.as_str())
            .is_some_and(|key| !key.is_empty());
        // ① 删旧 chunk（幂等，忽略单条失败）。
        if let Some(arr) = doc.get("chunk_iris").and_then(|v| v.as_array()) {
            for it in arr {
                if let Some(iri) = it.as_str() {
                    let _ = store.delete_with_claims(&claims, iri).await;
                }
            }
        }
        // ② 无原文或非可解析类型：无法重建，保留留底状态。
        if !has_blob_ref || kb_text_ext(&filename).is_none() {
            if let Some(o) = doc.as_object_mut() {
                o.insert("chunks".into(), json!(0));
                o.insert("chunk_iris".into(), json!([]));
                if kb_text_ext(&filename).is_none() {
                    o.insert("status".into(), json!("stored"));
                } else {
                    any_failed = true;
                    o.insert("status".into(), json!("failed"));
                    o.insert("skipped_reason".into(), json!("原文缺失，无法重建"));
                }
            }
            updated.push(doc);
            continue;
        }
        let key = format!("kb/{id}/{doc_id}");
        let bytes = match blob.get(&claims, &key).await {
            Ok(b) => b,
            Err(e) => {
                any_failed = true;
                if let Some(o) = doc.as_object_mut() {
                    o.insert("status".into(), json!("failed"));
                    o.insert("skipped_reason".into(), json!(format!("原文读取失败: {e}")));
                    o.insert("chunks".into(), json!(0));
                    o.insert("chunk_iris".into(), json!([]));
                }
                updated.push(doc);
                continue;
            }
        };
        // ③ 重新分块 embedding 写入。
        let text = String::from_utf8_lossy(&bytes).to_string();
        let tags = vec![format!("doc:{}", doc_id)];
        let mut new_iris: Vec<String> = Vec::new();
        let mut err: Option<String> = None;
        for chunk in chunk_text(&text, chunk_size) {
            let iri = format!("chunk/{}", uuid::Uuid::new_v4().hyphenated());
            match store.upsert_with_claims(&claims, &iri, &chunk, &tags).await {
                Ok(_) => new_iris.push(iri),
                Err(e) => {
                    err = Some(format!("写入失败: {e}"));
                    break;
                }
            }
        }
        if let Some(o) = doc.as_object_mut() {
            o.insert("chunks".into(), json!(new_iris.len()));
            o.insert("chunk_iris".into(), json!(new_iris));
            if let Some(e) = &err {
                any_failed = true;
                o.insert("status".into(), json!("failed"));
                o.insert("skipped_reason".into(), json!(e));
            } else {
                o.insert("status".into(), json!("ready"));
                o.remove("skipped_reason");
            }
        }
        updated.push(doc);
    }
    // 回写台账与状态。
    {
        let mut guard = state.knowledge_bases.write().await;
        if let Some(o) = guard
            .iter_mut()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .and_then(|b| b.as_object_mut())
        {
            o.insert("documents".into(), json!(updated));
            o.insert(
                "reindex_status".into(),
                json!(if any_failed { "failed" } else { "ready" }),
            );
            o.insert(
                "reindexed_at".into(),
                json!(chrono::Utc::now().to_rfc3339()),
            );
            o.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
        }
        let _ = save_knowledge_bases(&guard);
    }
    tracing::info!(kb = %id, failed = any_failed, "KB reindex 完成");
}

/// 把非 IRI 的标识符转为可用作 IRI 局部名的安全串（非字母数字与 ._- 之外替换为 _）。
/// 为 SPARQL IRIREF 局部标识做最小转义：保留 Unicode（中文实体/关系名可读、无碰撞），
/// 仅对 IRIREF 语法禁止的字符（控制符、空格、<>"{}|\^`）按 UTF-8 逐字节百分号编码。
fn kb_sanitize_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control()
            || c == ' '
            || matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`')
        {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 将三元组导入中的主/谓项展开为 IRI：已是 http(s)/iri: 前缀则原样；命中已知前缀走 expand_iri；
/// 否则包装为 iri://entity/{sanitize}（主语）或调用方另行处理谓语。
fn kb_expand_iri_term(raw: &str, entity_prefix: &str) -> String {
    let t = raw.trim();
    if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("iri://") {
        return t.to_string();
    }
    let expanded = expand_iri(t);
    if expanded != t {
        expanded
    } else {
        format!("{}{}", entity_prefix, kb_sanitize_id(t))
    }
}

/// 依 object_type 与启发式构造对象 RdfValue：iri→IRI；literal→字面量；缺省时按是否像 IRI 判定。
fn kb_object_value(raw: &str, object_type: Option<&str>) -> RdfValue {
    let t = raw.trim();
    match object_type
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("iri") => RdfValue::Iri(kb_expand_iri_term(t, "iri://entity/")),
        Some("literal") => RdfValue::Literal(t.to_string()),
        _ => {
            if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("iri://") {
                RdfValue::Iri(t.to_string())
            } else {
                RdfValue::Literal(t.to_string())
            }
        }
    }
}

/// 从 CSV 文本构造三元组（列名不区分大小写匹配 subject/predicate/object[/object_type]，缺则按位置 0/1/2/3）。
fn kb_quads_from_csv(text: &str) -> Result<Vec<RdfQuad>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .has_headers(true)
        .from_reader(text.as_bytes());
    let headers = rdr
        .headers()
        .map_err(|e| format!("CSV 表头解析失败: {e}"))?
        .clone();
    let find = |names: &[&str]| -> Option<usize> {
        headers
            .iter()
            .position(|h| names.iter().any(|n| h.trim().eq_ignore_ascii_case(n)))
    };
    let (si, pi, oi) = (
        find(&["subject", "s"]).unwrap_or(0),
        find(&["predicate", "p", "relation", "rel"]).unwrap_or(1),
        find(&["object", "o"]).unwrap_or(2),
    );
    let ti = find(&["object_type", "otype", "type"]);
    let mut quads = Vec::new();
    for (idx, rec) in rdr.records().enumerate() {
        let rec = rec.map_err(|e| format!("CSV 第 {} 行解析失败: {e}", idx + 2))?;
        let s = rec.get(si).unwrap_or("").trim();
        let p = rec.get(pi).unwrap_or("").trim();
        let o = rec.get(oi).unwrap_or("").trim();
        if s.is_empty() || p.is_empty() || o.is_empty() {
            continue;
        }
        let otype = ti.and_then(|i| rec.get(i));
        quads.push(RdfQuad {
            subject: kb_expand_iri_term(s, "iri://entity/"),
            predicate: kb_expand_iri_term(p, "iri://relation/"),
            object: kb_object_value(o, otype),
            graph: None,
        });
    }
    Ok(quads)
}

/// 从 JSONL 文本构造三元组（每行一个对象，键 subject/s、predicate/p、object/o、object_type 可选）。
fn kb_quads_from_jsonl(text: &str) -> Result<Vec<RdfQuad>, String> {
    let mut quads = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .map_err(|e| format!("JSONL 第 {} 行解析失败: {e}", idx + 1))?;
        let pick = |keys: &[&str]| -> String {
            for k in keys {
                if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
                    return s.trim().to_string();
                }
            }
            String::new()
        };
        let s = pick(&["subject", "s"]);
        let p = pick(&["predicate", "p", "relation", "rel"]);
        let o = pick(&["object", "o"]);
        if s.is_empty() || p.is_empty() || o.is_empty() {
            continue;
        }
        let otype = v.get("object_type").and_then(|x| x.as_str());
        quads.push(RdfQuad {
            subject: kb_expand_iri_term(&s, "iri://entity/"),
            predicate: kb_expand_iri_term(&p, "iri://relation/"),
            object: kb_object_value(&o, otype),
            graph: None,
        });
    }
    Ok(quads)
}

/// 从简化 N-Triples 文本构造三元组：每行 `<s> <p> <o> .` 或 `<s> <p> "literal" .`。
fn kb_quads_from_triples(text: &str) -> Result<Vec<RdfQuad>, String> {
    let mut quads = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_end_matches('.').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // subject
        let rest = line
            .strip_prefix('<')
            .ok_or_else(|| format!("第 {} 行：主语需为 <IRI>", idx + 1))?;
        let (subj, rest) = rest
            .split_once('>')
            .ok_or_else(|| format!("第 {} 行：主语缺少 >", idx + 1))?;
        let rest = rest.trim_start();
        // predicate
        let rest = rest
            .strip_prefix('<')
            .ok_or_else(|| format!("第 {} 行：谓语需为 <IRI>", idx + 1))?;
        let (pred, rest) = rest
            .split_once('>')
            .ok_or_else(|| format!("第 {} 行：谓语缺少 >", idx + 1))?;
        let obj_raw = rest.trim();
        let object = if let Some(inner) =
            obj_raw.strip_prefix('<').and_then(|r| r.strip_suffix('>'))
        {
            RdfValue::Iri(inner.to_string())
        } else if let Some(inner) = obj_raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            RdfValue::Literal(inner.to_string())
        } else if obj_raw.is_empty() {
            return Err(format!("第 {} 行：缺少宾语", idx + 1));
        } else {
            RdfValue::Literal(obj_raw.to_string())
        };
        quads.push(RdfQuad {
            subject: subj.to_string(),
            predicate: pred.to_string(),
            object,
            graph: None,
        });
    }
    Ok(quads)
}

/// POST /api/v1/kb/bases/:id/import-graph — 图谱库文件导入（multipart）。
/// 字段：file（文件）、format（csv|jsonl|triples，缺省按扩展名推断）、schema（可选）、clear_before（可选）。
pub(crate) async fn import_graph_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "verified isolation claims required for graph storage" })),
            )
        }
    };
    let kb = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
    };
    let kb = match kb {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "knowledge base not found", "id": id })),
            )
        }
    };
    if kb.get("kb_type").and_then(|v| v.as_str()) != Some("graph") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "仅图谱知识库支持三元组导入" })),
        );
    }
    let graph_iri = match claims.graph_iri() {
        Ok(graph_iri) => graph_iri,
        Err(e) => {
            tracing::warn!("KB import invalid verified graph scope: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "invalid verified graph scope" })),
            );
        }
    };

    let mut format: Option<String> = None;
    let mut schema = String::new();
    let mut clear_before = false;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name = String::new();
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
        let fname = field.name().unwrap_or_default().to_string();
        let filename = field.file_name().map(|s| s.to_string());
        match fname.as_str() {
            "format" => {
                if let Ok(t) = field.text().await {
                    format = Some(t.trim().to_ascii_lowercase());
                }
            }
            "schema" => {
                if let Ok(t) = field.text().await {
                    schema = t.trim().to_string();
                }
            }
            "clear_before" => {
                if let Ok(t) = field.text().await {
                    clear_before = matches!(t.trim(), "true" | "1" | "yes");
                }
            }
            _ => {
                if let Some(n) = filename {
                    file_name = n;
                }
                match field.bytes().await {
                    Ok(b) => file_bytes = Some(b.to_vec()),
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("读取文件失败: {e}") })),
                        )
                    }
                }
            }
        }
    }

    // 推断格式：显式 format 优先，其次文件扩展名，默认 csv。
    let fmt = format.unwrap_or_else(|| {
        let lower = file_name.to_ascii_lowercase();
        if lower.ends_with(".jsonl") || lower.ends_with(".json") {
            "jsonl".into()
        } else if lower.ends_with(".nt") || lower.ends_with(".ttl") || lower.ends_with(".triples") {
            "triples".into()
        } else {
            "csv".into()
        }
    });

    let has_file = file_bytes.as_ref().map(|b| !b.is_empty()).unwrap_or(false);
    if !has_file && schema.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "未收到文件（字段名 file）或 schema" })),
        );
    }

    let mut quads: Vec<RdfQuad> = Vec::new();
    if has_file {
        let text = String::from_utf8_lossy(file_bytes.as_ref().unwrap()).to_string();
        let parsed = match fmt.as_str() {
            "csv" => kb_quads_from_csv(&text),
            "jsonl" => kb_quads_from_jsonl(&text),
            "triples" | "nt" | "ttl" => kb_quads_from_triples(&text),
            "cypher" => Err(
                "暂不支持执行 Cypher（Oxigraph 走 SPARQL），请改用 CSV/JSONL/triples".to_string(),
            ),
            other => Err(format!("不支持的 format: {other}")),
        };
        match parsed {
            Ok(q) => quads = q,
            Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
        }
    }

    // 统计不同主语/谓语（写入前，基于原始 quads）。
    let mut subjects = std::collections::HashSet::new();
    let mut predicates = std::collections::HashSet::new();
    for q in &quads {
        subjects.insert(q.subject.clone());
        predicates.insert(q.predicate.clone());
    }

    // 可选 schema：写为命名图元三元组，供后续写入时校验参考。
    let schema_saved = !schema.is_empty();
    if schema_saved {
        quads.push(RdfQuad {
            subject: graph_iri.clone(),
            predicate: "https://agentos.ontology/meta/kbSchema".to_string(),
            object: RdfValue::Literal(schema.clone()),
            graph: None,
        });
    }

    if clear_before {
        let clear = format!(
            "DELETE WHERE {{ GRAPH <{g}> {{ ?s ?p ?o . }} }}",
            g = graph_iri
        );
        if let Err(e) = state.kg_store.update(&clear) {
            tracing::warn!(graph = %graph_iri, "KB import clear skipped: {}", e);
        }
    }

    if quads.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "imported",
                "graph": graph_iri,
                "format": fmt,
                "triples_written": 0,
                "entities": 0,
                "relations": 0,
                "schema_saved": schema_saved,
                "note": "未解析出任何三元组",
            })),
        );
    }

    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
        }
    };
    match kg.write_quads_for_claims(claims, &quads) {
        Ok(()) => {
            let _ = state.kg_store.flush();
            (
                StatusCode::OK,
                Json(json!({
                    "status": "imported",
                    "graph": graph_iri,
                    "format": fmt,
                    "triples_written": quads.len(),
                    "entities": subjects.len(),
                    "relations": predicates.len(),
                    "schema_saved": schema_saved,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
/// §9 知识库图谱摄取回归单测：固化两处已修复缺陷——
///   1) 中文 IRI 保留（kb_sanitize_id 不再把非 ASCII 折叠成 `_`，避免碰撞/损坏）；
///   2) 图谱库 stats 三元组计数（Oxigraph 绑定键带 `?` 前缀，须用 `?c` 而非 `c`）。
#[cfg(test)]
mod kb_ingest_tests {
    use super::*;
    use crate::isolation::IsolationClaims;
    use crate::knowledge_graph::store::KnowledgeGraphStore;

    /// 回归：中文实体/关系名应原样保留 Unicode，仅对 IRIREF 禁用字符做百分号编码。
    #[test]
    fn test_kb_sanitize_id_preserves_unicode() {
        // 中文原样保留（旧实现会全部变成下划线）。
        assert_eq!(kb_sanitize_id("比亚迪"), "比亚迪");
        assert_eq!(kb_sanitize_id("车型:测试001"), "车型:测试001");
        // 不同中文实体不得坍缩到同一串（旧实现会碰撞）。
        assert_ne!(kb_sanitize_id("比亚迪"), kb_sanitize_id("特斯拉"));
        // IRIREF 语法禁用字符按 UTF-8 逐字节百分号编码。
        assert_eq!(kb_sanitize_id("a b"), "a%20b");
        let enc = kb_sanitize_id("x<y>\"z");
        assert!(
            enc.contains("%3C") && enc.contains("%3E") && enc.contains("%22"),
            "got {enc}"
        );
    }

    /// 回归：中文主/谓项展开为可读、无碰撞的 iri://entity|relation IRI。
    #[test]
    fn test_kb_expand_iri_term_chinese_no_collision() {
        let a = kb_expand_iri_term("车型:EV001", "iri://entity/");
        let b = kb_expand_iri_term("车型:EV002", "iri://entity/");
        assert_eq!(a, "iri://entity/车型:EV001");
        assert_ne!(a, b, "不同中文实体必须映射到不同 IRI");
        // 已是 IRI 前缀则原样透传。
        assert_eq!(
            kb_expand_iri_term("http://ex.org/x", "iri://entity/"),
            "http://ex.org/x"
        );
    }

    /// 回归：CSV 图谱导入保留中文、区分 iri/literal 宾语类型。
    #[test]
    fn test_kb_quads_from_csv_chinese() {
        let csv = "subject,predicate,object,object_type\n\
                   车型:测试001,属于品牌,品牌:比亚迪,iri\n\
                   车型:测试001,续航里程,605,literal\n";
        let quads = kb_quads_from_csv(csv).expect("csv parse");
        assert_eq!(quads.len(), 2);
        assert_eq!(quads[0].subject, "iri://entity/车型:测试001");
        assert_eq!(quads[0].predicate, "iri://relation/属于品牌");
        assert_eq!(
            quads[0].object,
            RdfValue::Iri("iri://entity/品牌:比亚迪".to_string())
        );
        assert_eq!(quads[1].object, RdfValue::Literal("605".to_string()));
    }

    /// 回归：写入命名图后，用 stats handler 同款 COUNT 查询验证——
    /// 绑定键为 `?c`（带 `?`），`c` 不存在；中文 IRI 精确计数。
    #[test]
    fn test_graph_stats_count_binding_key() {
        let kg = KnowledgeGraphStore::new().expect("in-mem store");
        let claims =
            IsolationClaims::from_verified("kb-test-tenant", "stats-project", "test-actor")
                .expect("verified claims");
        let csv = "subject,predicate,object,object_type\n\
                   车型:测试001,属于品牌,品牌:比亚迪,iri\n\
                   车型:测试001,续航里程,605,literal\n";
        let quads = kb_quads_from_csv(csv).expect("csv parse");
        kg.write_quads_for_claims(&claims, &quads)
            .expect("write quads");

        // Claims-scoped queries apply the named graph automatically.
        let rows = kg
            .query_sparql_for_claims(&claims, "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }")
            .expect("count query");
        let first = rows.first().expect("one row");
        // 关键回归：绑定键带 `?` 前缀。
        assert!(first.get("c").is_none(), "绑定键不应是 `c`");
        let count = first
            .get("?c")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .expect("?c parses to u64");
        assert_eq!(count, 2);
    }

    /// 回归：旧 knowledge_graph 已被某知识包（graph_kb_ids）覆盖时——
    /// 迁移只清空旧字段、不新建包，且幂等（二次运行无变更）。
    #[test]
    fn test_migrate_legacy_graph_already_covered() {
        let kb_uuid = "cbf58bb1-f09d-4256-a195-351f10172a90";
        let mut agents = vec![json!({
            "id": "a1",
            "name": "新能源车维修助手",
            "knowledge_graph": format!("tenant:default/tenant:default/kb/{}", kb_uuid),
            "knowledge_pack_ids": ["ev-repair-fault-kb"],
        })];
        let mut packs = vec![json!({
            "id": "ev-repair-fault-kb",
            "graph_kb_ids": [kb_uuid],
        })];
        let (a, p) = crate::api::http::agents::migrate_legacy_agent_graphs(&mut agents, &mut packs);
        assert!(a, "agent 应被迁移");
        assert!(!p, "已覆盖：不应新建知识包");
        assert_eq!(packs.len(), 1, "包数量不变");
        assert_eq!(agents[0]["knowledge_graph"], json!(""), "旧字段应清空");
        assert_eq!(
            agents[0]["knowledge_pack_ids"],
            json!(["ev-repair-fault-kb"])
        );
        // 幂等：二次运行无变更。
        let (a2, p2) =
            crate::api::http::agents::migrate_legacy_agent_graphs(&mut agents, &mut packs);
        assert!(!a2 && !p2, "幂等：清空后不再变更");
    }

    /// 回归：旧 knowledge_graph 未被任何包覆盖时——新建 graph_kb_ids 包并挂载。
    #[test]
    fn test_migrate_legacy_graph_creates_pack() {
        let kb_uuid = "11111111-2222-3333-4444-555555555555";
        let mut agents = vec![json!({
            "id": "a2",
            "name": "维修助手",
            "knowledge_graph": format!("tenant:default/kb/{}", kb_uuid),
            "knowledge_pack_ids": [],
        })];
        let mut packs: Vec<Value> = vec![];
        let (a, p) = crate::api::http::agents::migrate_legacy_agent_graphs(&mut agents, &mut packs);
        assert!(a && p, "应迁移并新建包");
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0]["graph_kb_ids"], json!([kb_uuid]));
        let new_pack_id = packs[0]["id"].as_str().unwrap();
        assert_eq!(agents[0]["knowledge_pack_ids"], json!([new_pack_id]));
        assert_eq!(agents[0]["knowledge_graph"], json!(""));
    }
}

#[cfg(test)]
mod kb_isolation_http_tests {
    use super::*;
    use crate::api::http::{api_gov::ApiUsageState, AppState, TEST_ENV_LOCK};
    use crate::core::core_types::{CoreConfig, SemanticCore};
    use crate::gateway::UnifiedGateway;
    use crate::memory::embedding_service::FallbackEmbeddingService;
    use crate::memory::hyperspace_store::HyperspaceStore;
    use crate::tools::prompt_registry::PromptRegistry;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::{get, post, put},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use tower::ServiceExt;

    fn test_state(tmp: &std::path::Path) -> Arc<AppState> {
        let l0 = tmp.join("l0");
        std::fs::create_dir_all(&l0).unwrap();
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: l0.to_str().unwrap().to_string(),
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
        let vector_store = HyperspaceStore::open(
            &tmp.join("vectors"),
            Arc::new(FallbackEmbeddingService::new()),
        )
        .unwrap();
        Arc::new(AppState {
            core,
            gateway,
            kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
            config_info: Arc::new(tokio::sync::RwLock::new(json!({}))),
            agents_info: json!({ "count": 0, "agents": [] }),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![])),
            prompts: Arc::new(PromptRegistry::new()),
            kb_categories: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_bases: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_packs: Arc::new(tokio::sync::RwLock::new(vec![])),
            vector_store: Arc::new(arc_swap::ArcSwapOption::from_pointee(vector_store)),
            blob_store: None,
            task_executor: None,
            batch_manager: None,
            api_clients: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_keys: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_usage: Arc::new(ApiUsageState::default()),
        })
    }

    fn jwt(tenant_id: &str) -> String {
        jwt_for_scope(tenant_id, None)
    }

    fn jwt_for_scope(tenant_id: &str, project_id: Option<&str>) -> String {
        encode(
            &Header::default(),
            &super::super::iam::JwtClaims {
                sub: format!("{tenant_id}-user"),
                tenant_id: tenant_id.to_string(),
                project_id: project_id.map(str::to_owned),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
        )
        .unwrap()
    }

    async fn response_json(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn kb_catalog_requires_claims_and_isolates_tenants() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let previous_data_dir = std::env::var_os("AGENTOS_DATA_DIR");
        std::env::set_var("AGENTOS_DATA_DIR", tmp.path());
        let state = test_state(tmp.path());
        let app = Router::new()
            .route("/kb/bases", get(list_knowledge_bases_handler).post(create_knowledge_base_handler))
            .route("/kb/bases/:id", put(update_knowledge_base_handler).delete(delete_knowledge_base_handler))
            .with_state(state.clone());

        let unauthenticated = Request::builder()
            .method("POST").uri("/kb/bases").header("content-type", "application/json")
            .body(Body::from(r#"{"name":"private","kb_type":"graph"}"#)).unwrap();
        assert_eq!(response_json(&app, unauthenticated).await.0, StatusCode::UNAUTHORIZED);

        // Unknown client graph/namespace inputs cannot override the claims mint.
        let create = Request::builder()
            .method("POST").uri("/kb/bases")
            .header("authorization", format!("Bearer {}", jwt("tenant-a")))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"tenant A catalog","kb_type":"graph","graph":"tenant:evil/kb/x","vector_namespace":"vector://evil/project"}"#)).unwrap();
        let (status, created) = response_json(&app, create).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["base"]["graph"], json!("graph://tenant-a/default"));
        let kb_id = created["id"].as_str().unwrap().to_string();

        let list = |tenant: &str| Request::builder().uri("/kb/bases")
            .header("authorization", format!("Bearer {}", jwt(tenant)))
            .body(Body::empty()).unwrap();
        assert_eq!(response_json(&app, list("tenant-a")).await.1["count"], json!(1));
        assert_eq!(response_json(&app, list("tenant-b")).await.1["count"], json!(0));

        let other_tenant_delete = Request::builder().method("DELETE").uri(format!("/kb/bases/{kb_id}"))
            .header("authorization", format!("Bearer {}", jwt("tenant-b"))).body(Body::empty()).unwrap();
        assert_eq!(response_json(&app, other_tenant_delete).await.0, StatusCode::NOT_FOUND);

        let other_tenant_update = Request::builder().method("PUT").uri(format!("/kb/bases/{kb_id}"))
            .header("authorization", format!("Bearer {}", jwt("tenant-b")))
            .header("content-type", "application/json").body(Body::from(r#"{"name":"must not update"}"#)).unwrap();
        assert_eq!(response_json(&app, other_tenant_update).await.0, StatusCode::NOT_FOUND);

        let other_project_list = Request::builder().uri("/kb/bases")
            .header("authorization", format!("Bearer {}", jwt_for_scope("tenant-a", Some("other-project"))))
            .body(Body::empty()).unwrap();
        assert_eq!(response_json(&app, other_project_list).await.1["count"], json!(0));

        for method in ["PUT", "DELETE"] {
            let mut builder = Request::builder().method(method).uri(format!("/kb/bases/{kb_id}"));
            if method == "PUT" {
                builder = builder.header("content-type", "application/json");
            }
            let body = if method == "PUT" { Body::from(r#"{"name":"must fail"}"#) } else { Body::empty() };
            assert_eq!(response_json(&app, builder.body(body).unwrap()).await.0, StatusCode::UNAUTHORIZED);
        }

        let metadata = state.kg_store.query(
            "SELECT ?s WHERE { GRAPH <graph://tenant-a/default> {
                ?s <https://agentos.ontology/meta/kbName> \"tenant A catalog\"
            }}",
        ).unwrap();
        let oxigraph::sparql::QueryResults::Solutions(metadata) = metadata else {
            panic!("expected SPARQL solutions");
        };
        assert_eq!(metadata.count(), 1, "catalog metadata must use claims graph");

        if let Some(previous_data_dir) = previous_data_dir {
            std::env::set_var("AGENTOS_DATA_DIR", previous_data_dir);
        } else {
            std::env::remove_var("AGENTOS_DATA_DIR");
        }
    }

    #[tokio::test]
    async fn graph_import_and_stats_require_claims_and_isolate_tenants() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        state.knowledge_bases.write().await.push(json!({
            "id": "graph-kb",
            "kb_type": "graph",
            "graph": "graph:world"
        }));
        let app = Router::new()
            .route("/kb/:id/import", post(import_graph_knowledge_base_handler))
            .route("/kb/:id/stats", get(knowledge_base_stats_handler))
            .with_state(state.clone());

        let unauthenticated = Request::builder()
            .method("POST")
            .uri("/kb/graph-kb/import")
            .header(
                "content-type",
                "multipart/form-data; boundary=claims-required",
            )
            .body(Body::from("--claims-required--\r\n"))
            .unwrap();
        assert_eq!(
            response_json(&app, unauthenticated).await.0,
            StatusCode::UNAUTHORIZED
        );
        let unauthenticated_stats = Request::builder()
            .uri("/kb/graph-kb/stats")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            response_json(&app, unauthenticated_stats).await.0,
            StatusCode::UNAUTHORIZED
        );

        let boundary = "kb-boundary";
        let csv =
            "subject,predicate,object\nhttp://example.test/a,http://example.test/p,tenant A\n";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.csv\"\r\n\r\n{csv}\r\n--{boundary}--\r\n"
        );
        let imported = Request::builder()
            .method("POST")
            .uri("/kb/graph-kb/import")
            .header("authorization", format!("Bearer {}", jwt("tenant-a")))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        assert_eq!(response_json(&app, imported).await.0, StatusCode::OK);

        let a_stats = Request::builder()
            .uri("/kb/graph-kb/stats")
            .header("authorization", format!("Bearer {}", jwt("tenant-a")))
            .body(Body::empty())
            .unwrap();
        assert_eq!(response_json(&app, a_stats).await.1["triples"], json!(1));
        let b_stats = Request::builder()
            .uri("/kb/graph-kb/stats")
            .header("authorization", format!("Bearer {}", jwt("tenant-b")))
            .body(Body::empty())
            .unwrap();
        assert_eq!(response_json(&app, b_stats).await.1["triples"], json!(0));

        let legacy = state
            .kg_store
            .query("SELECT ?s WHERE { GRAPH <graph:world> { ?s ?p ?o } }")
            .unwrap();
        let oxigraph::sparql::QueryResults::Solutions(legacy) = legacy else {
            panic!("expected SPARQL solutions");
        };
        assert_eq!(legacy.count(), 0);
    }

    #[tokio::test]
    async fn vector_ingest_and_search_require_claims_and_isolate_tenants() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        state.knowledge_bases.write().await.push(json!({
            "id": "vector-kb",
            "kb_type": "vector",
            "vector_namespace": "tenant:legacy"
        }));
        let app = Router::new()
            .route("/kb/:id/ingest", post(ingest_knowledge_base_handler))
            .route("/kb/:id/search", post(search_knowledge_base_handler))
            .with_state(state);

        let unauthenticated = Request::builder()
            .method("POST")
            .uri("/kb/vector-kb/ingest")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"text":"private battery instructions"}"#))
            .unwrap();
        assert_eq!(
            response_json(&app, unauthenticated).await.0,
            StatusCode::UNAUTHORIZED
        );
        let unauthenticated_search = Request::builder()
            .method("POST")
            .uri("/kb/vector-kb/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"battery instructions"}"#))
            .unwrap();
        assert_eq!(
            response_json(&app, unauthenticated_search).await.0,
            StatusCode::UNAUTHORIZED
        );

        let ingested = Request::builder()
            .method("POST")
            .uri("/kb/vector-kb/ingest")
            .header("authorization", format!("Bearer {}", jwt("tenant-a")))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"text":"private battery instructions"}"#))
            .unwrap();
        assert_eq!(response_json(&app, ingested).await.0, StatusCode::OK);

        let search = |tenant_id| {
            Request::builder()
                .method("POST")
                .uri("/kb/vector-kb/search")
                .header("authorization", format!("Bearer {}", jwt(tenant_id)))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"battery instructions"}"#))
                .unwrap()
        };
        assert_eq!(
            response_json(&app, search("tenant-a")).await.1["count"],
            json!(1)
        );
        assert_eq!(
            response_json(&app, search("tenant-b")).await.1["count"],
            json!(0)
        );
    }
}
