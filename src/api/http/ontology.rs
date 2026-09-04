//! 本体元模型 CRUD 与动力层 Action invoke。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；知识包/KB 见 `kb.rs`。

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    isolation::IsolationClaims,
    knowledge_graph::{
        ontology_layer::ActionGuardrailConfig,
        store::{ClaimsGraphUpdate, KnowledgeGraphStore, PendingActionApproval},
    },
};

use super::{iam::UserIdentity, ontology_guardrails, AppState};

const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

fn sparql_literal(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// GET /api/v1/ontology/types — 返回新能源车维修域本体定义（对象/链接/动作/函数）
///
/// 语义层（ObjectType/LinkType）+ 动力层（ActionType/FunctionDef）的完整元模型。
///
/// 数据源为 Oxigraph 元命名图（`graph:ontology/meta`）：首启由 `ensure_seeded` 幂等
/// 把硬编码 `ev_repair_ontology()` 写入图谱，之后读路径解析 `meta:json` 快照重建。
/// 存储不可用时回退硬编码定义，保证只读契约零回归。
pub(crate) async fn ontology_types_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    use crate::knowledge_graph::ontology_store::OntologyStore;
    let ont = (|| {
        let store = OntologyStore::with_shared_store(state.kg_store.clone()).ok()?;
        store.ensure_seeded("ev-repair").ok()?;
        store.load_definition("ev-repair").ok()
    })()
    .unwrap_or_else(crate::knowledge_graph::ontology_layer::ev_repair_ontology);
    Json(json!({
        "domain": ont.domain,
        "counts": {
            "object_types": ont.object_types.len(),
            "link_types": ont.link_types.len(),
            "action_types": ont.action_types.len(),
            "functions": ont.functions.len(),
        },
        "object_types": ont.object_types,
        "link_types": ont.link_types,
        "action_types": ont.action_types,
        "functions": ont.functions,
    }))
}

/// GET /api/v1/ontology/guardrails — 返回当前 claims 已认证调用方可读取的域默认护栏。
pub(crate) async fn domain_guardrails_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    match store.load_domain_guardrails(ONT_DOMAIN) {
        Ok(guardrails) => {
            Json(json!({ "domain": ONT_DOMAIN, "guardrails": guardrails })).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

/// PUT /api/v1/ontology/guardrails — 更新域默认护栏。
///
/// This endpoint is claims-authenticated. Invoke payloads cannot carry this configuration;
/// their graph scope and policy are selected server-side from the stored ActionType/domain.
pub(crate) async fn update_domain_guardrails_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(guardrails): Json<ActionGuardrailConfig>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    if let Err(error) = ontology_guardrails::validate_config(&guardrails) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(store) => store,
        Err(error) => return error.into_response(),
    };
    match store.upsert_domain_guardrails(ONT_DOMAIN, &guardrails) {
        Ok(()) => Json(json!({ "status": "ok", "domain": ONT_DOMAIN, "guardrails": guardrails }))
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

// ─── 阶段1：ObjectType + LinkType 在线 CRUD（存储驱动，写前备份 meta 图）───────
//
// 契约：
//   POST /api/v1/ontology/object-types          body=ObjectType   新建/更新（幂等）
//   PUT  /api/v1/ontology/object-types/:id       body=ObjectType   更新（id 以路径为准）
//   DELETE /api/v1/ontology/object-types/:id                       删除（被引用→409）
//   POST /api/v1/ontology/link-types            body=LinkType     新建/更新（source/target 校验）
//   PUT  /api/v1/ontology/link-types/:id         body=LinkType     更新
//   DELETE /api/v1/ontology/link-types/:id                        删除
// 本体域固定为 ev-repair（当前单域）；首启由 ensure_seeded 幂等 seed。

const ONT_DOMAIN: &str = "ev-repair";

/// 构造 OntologyStore 并确保已 seed（失败转 500 JSON）。
fn ontology_store_ready(
    state: &Arc<AppState>,
) -> Result<crate::knowledge_graph::ontology_store::OntologyStore, (StatusCode, Json<Value>)> {
    use crate::knowledge_graph::ontology_store::OntologyStore;
    let store = OntologyStore::with_shared_store(state.kg_store.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
    })?;
    store.ensure_seeded(ONT_DOMAIN).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
    })?;
    Ok(store)
}

/// POST /api/v1/ontology/object-types — 新建或更新对象类型（幂等 upsert）。
pub(crate) async fn upsert_object_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(obj): Json<crate::knowledge_graph::ontology_layer::ObjectType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_object_type(ONT_DOMAIN, &obj) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": obj.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/object-types/:id — 更新对象类型（id 以路径为准）。
pub(crate) async fn update_object_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut obj): Json<crate::knowledge_graph::ontology_layer::ObjectType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    obj.id = id;
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_object_type(ONT_DOMAIN, &obj) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": obj.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/object-types/:id — 删除对象类型；被引用返回 409。
pub(crate) async fn delete_object_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_object_type(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(refs) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "对象类型被引用，无法删除", "references": refs })),
        )
            .into_response(),
    }
}

/// POST /api/v1/ontology/link-types — 新建或更新链接类型（校验 source/target 存在）。
pub(crate) async fn upsert_link_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(link): Json<crate::knowledge_graph::ontology_layer::LinkType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_link_type(ONT_DOMAIN, &link) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": link.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/link-types/:id — 更新链接类型（id 以路径为准）。
pub(crate) async fn update_link_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut link): Json<crate::knowledge_graph::ontology_layer::LinkType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    link.id = id;
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_link_type(ONT_DOMAIN, &link) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": link.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/link-types/:id — 删除链接类型（无下游引用，直接删）。
pub(crate) async fn delete_link_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_link_type(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

// ─── 阶段2：ActionType + FunctionDef 声明式 CRUD（存储驱动，写前备份 meta 图）───
//
// 契约（与对象/链接一致的语义）：
//   POST /api/v1/ontology/action-types           body=ActionType   新建/更新（applies_to 校验）
//   PUT  /api/v1/ontology/action-types/:id        body=ActionType   更新
//   DELETE /api/v1/ontology/action-types/:id                        删除（动作为叶子，直接删）
//   POST /api/v1/ontology/function-defs          body=FunctionDef  新建/更新（applies_to 校验）
//   PUT  /api/v1/ontology/function-defs/:id       body=FunctionDef  更新
//   DELETE /api/v1/ontology/function-defs/:id                       删除
// 声明式：动作携带 parameters/preconditions/side_effects 声明，函数携带 returns/expression。
// 内置动作的执行仍由 invoke_action_handler 分派；自定义动作声明可存但暂不可执行（见 invoke）。

/// POST /api/v1/ontology/action-types — 新建或更新动作类型（幂等 upsert；applies_to 校验）。
pub(crate) async fn upsert_action_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(action): Json<crate::knowledge_graph::ontology_layer::ActionType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    if let Err(e) = ontology_guardrails::validate_config(&action.guardrails) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_action_type(ONT_DOMAIN, &action) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": action.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/action-types/:id — 更新动作类型（id 以路径为准）。
pub(crate) async fn update_action_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut action): Json<crate::knowledge_graph::ontology_layer::ActionType>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    action.id = id;
    if let Err(e) = ontology_guardrails::validate_config(&action.guardrails) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_action_type(ONT_DOMAIN, &action) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": action.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/action-types/:id — 删除动作类型（叶子元素，直接删）。
pub(crate) async fn delete_action_type_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_action_type(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// POST /api/v1/ontology/function-defs — 新建或更新函数（幂等 upsert；applies_to 校验）。
pub(crate) async fn upsert_function_def_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(func): Json<crate::knowledge_graph::ontology_layer::FunctionDef>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_function_def(ONT_DOMAIN, &func) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": func.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// PUT /api/v1/ontology/function-defs/:id — 更新函数（id 以路径为准）。
pub(crate) async fn update_function_def_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut func): Json<crate::knowledge_graph::ontology_layer::FunctionDef>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    func.id = id;
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.upsert_function_def(ONT_DOMAIN, &func) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "id": func.id })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// DELETE /api/v1/ontology/function-defs/:id — 删除函数（叶子元素，直接删）。
pub(crate) async fn delete_function_def_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if identity.isolation_claims().is_none() {
        return unauthorized_isolation_claims().into_response();
    }
    let store = match ontology_store_ready(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match store.delete_function_def(ONT_DOMAIN, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "id": id }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

// ─── 动力层执行器（ActionType invoke）──────────────────────────────────
//
// 让知识图谱从"只读"变为"可写可执行"：依据 ActionType 做参数校验 + 前置条件检查，
// 再把 side-effect 以 SPARQL 写回 JWT claims 铸造的命名图。
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn unauthorized_isolation_claims() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "verified JWT isolation claims are required" })),
    )
}

/// 本体实例 IRI：https://agentos.ontology/ev/{ObjectType}/{key}
fn ev_instance_iri(obj_type: &str, key: &str) -> String {
    format!("https://agentos.ontology/ev/{}/{}", obj_type, iri_safe(key))
}
/// 对象类型 / 链接类型 IRI（与 ontology_layer 的 ev() 一致）。
fn ev_term_iri(name: &str) -> String {
    format!("https://agentos.ontology/ev/{}", name)
}
/// 属性谓词 IRI。
fn ev_prop_iri(name: &str) -> String {
    format!("https://agentos.ontology/ev/prop/{}", name)
}
/// 主键值转 IRI 安全片段。
fn iri_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_whitespace()
                || matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\')
            {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// 文本字面量项（含转义引号）。
fn lit(s: &str) -> String {
    format!("\"{}\"", sparql_literal(s))
}
/// 十进制数值字面量项。
fn lit_decimal(n: f64) -> String {
    format!("\"{}\"^^<{}>", n, XSD_DECIMAL)
}

/// 属性 upsert：先删旧值再写新值（idempotent）。obj 为完整对象项。
fn upsert_prop_stmts(subject: &str, prop: &str, obj: &str) -> Vec<ClaimsGraphUpdate> {
    vec![
        ClaimsGraphUpdate::delete_where(format!("<{subject}> <{prop}> ?old")),
        ClaimsGraphUpdate::insert_data(format!("<{subject}> <{prop}> {obj}")),
    ]
}

/// 命名图内对象是否存在（前置条件检查）。
fn ev_object_exists(kg: &KnowledgeGraphStore, claims: &IsolationClaims, iri: &str) -> bool {
    let q = format!("SELECT ?o WHERE {{ <{iri}> ?p ?o }} LIMIT 1");
    kg.query_sparql_for_claims(claims, &q)
        .map(|r| !r.is_empty())
        .unwrap_or(false)
}

/// 对象存在性前置条件解析（知识/业务分流，MCP 向后兼容扩展位）。
///
/// - 知识对象（FaultCode / VehicleModel / FAQ…）：查询知识命名图。
/// - 业务对象（Vehicle / Battery / RepairOrder…）：业务数据不入图谱，未来经 MCP
///   对接业务库查询；当前 MCP 未接入，回退查询命名图以保持向后兼容——接入 MCP 后
///   只需替换 Business 分支，调用方（build_action_effects）无需改动。
fn resolve_object_exists(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    object_type: &str,
    key: &str,
) -> bool {
    use crate::knowledge_graph::ontology_layer::{object_kind_of, ObjectKind};
    let iri = ev_instance_iri(object_type, key);
    match object_kind_of(object_type) {
        ObjectKind::Knowledge => ev_object_exists(kg, claims, &iri),
        // TODO(MCP): 业务库接入后改为经 MCP 查询业务对象是否存在；当前回退命名图。
        ObjectKind::Business => ev_object_exists(kg, claims, &iri),
    }
}

fn p_str(params: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    match params.get(name) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}
fn p_num(params: &serde_json::Map<String, Value>, name: &str) -> Option<f64> {
    match params.get(name) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionInvokeRequest {
    /// applies_to 对象实例的主键值（动作作用的目标对象）。
    #[serde(default)]
    pub target: Option<String>,
    /// 动作参数（name → value）。
    #[serde(default)]
    pub params: serde_json::Map<String, Value>,
    /// 仅校验并返回将执行的 SPARQL，不真正写回。
    #[serde(default)]
    pub dry_run: bool,
    /// `auto` commits immediately unless a future guardrail marks the action
    /// high-risk. `require_approval` preserves the staging graph for HITL.
    #[serde(default)]
    pub commit_strategy: ActionCommitStrategy,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionCommitStrategy {
    #[default]
    Auto,
    RequireApproval,
}

/// POST /api/v1/ontology/actions/:id/invoke — 动力层执行器
/// 内置（已实现执行逻辑）的动作 id 白名单——只有这些动作可真正 invoke。
/// 自定义动作（阶段2 声明式 CRUD 新建的）当前 executable=false，声明可存但暂不可执行，
/// 待阶段3 通用执行器（SPARQL 模板 + 护栏）落地后开放。
const BUILTIN_EXECUTABLE_ACTIONS: &[&str] = &["GenerateRepairOrder"];

// ─── 数据沙箱（staging-graph 影子图执行 + 护栏后校验）────────────────────
//
// 让动作写回从"直接落生产图"升级为"先写隔离影子图 → 护栏校验 → 通过才合并、
// 失败即回滚"，等价一次可回滚事务。仅隔离**数据**（命名图级），不隔离计算/进程；
// 计算沙箱（任意代码执行）见 docs 记录，待有需要再实现。
//
//   1. 为本次 invoke 生成 JWT claims 图派生的 per-invocation 影子图
//   2. 把 side-effect 语句里的生产图 IRI 重定向到影子图，写入影子图（生产图零改动）
//   3. 对影子图跑 ASK 护栏（三元组数上限 / 谓词命名空间白名单），任一命中即视为违规
//   4. 通过 → ADD 影子图到生产图 + DROP 影子图（提交）；违规 → DROP 影子图（回滚）

#[derive(Debug, Default)]
struct SandboxGuardrailReport {
    violations: Vec<String>,
    /// A relaxed policy is held for approval even when its hard checks pass.
    high_risk: bool,
}

fn sandbox_guardrail_report(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    staging_id: &str,
    policy: &ontology_guardrails::EffectiveGuardrails,
) -> Result<SandboxGuardrailReport, String> {
    Ok(SandboxGuardrailReport {
        violations: ontology_guardrails::violations(kg, claims, staging_id, policy)?,
        high_risk: ontology_guardrails::is_high_risk(policy),
    })
}

const ACTION_APPROVAL_TTL_HOURS: i64 = 24;

#[derive(Debug)]
enum StagingCommitOutcome {
    Committed(Value),
    Pending(PendingActionApproval),
}

/// 经影子图提交一批写回语句。默认自动合并；HITL 策略或高风险护栏策略会保留影子图。
fn commit_via_staging(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    statements: &[ClaimsGraphUpdate],
    strategy: ActionCommitStrategy,
    action_id: &str,
    now: chrono::DateTime<chrono::Utc>,
    guardrails: &ontology_guardrails::EffectiveGuardrails,
) -> Result<StagingCommitOutcome, (StatusCode, String, Vec<String>)> {
    let staging_id = uuid::Uuid::new_v4().simple().to_string();
    let staging = kg
        .staging_graph_iri_for_claims(claims, &staging_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e, vec![]))?;

    // 1. 写入影子图（生产图零改动）。任一失败即清理并报错。
    for stmt in statements {
        if let Err(e) = kg.update_staging_for_claims(claims, &staging_id, stmt) {
            let _ = kg.drop_staging_for_claims(claims, &staging_id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("影子图写入失败: {e}"),
                vec![],
            ));
        }
    }

    // 2. 护栏后校验。违规即回滚（DROP 影子图），生产图不受影响。
    let guardrails = match sandbox_guardrail_report(kg, claims, &staging_id, guardrails) {
        Ok(report) => report,
        Err(e) => {
            let _ = kg.drop_staging_for_claims(claims, &staging_id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("护栏校验失败，已回滚: {e}"),
                vec![],
            ));
        }
    };
    if !guardrails.violations.is_empty() {
        let _ = kg.drop_staging_for_claims(claims, &staging_id);
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "护栏校验未通过，已回滚（生产图未改动）".to_string(),
            guardrails.violations,
        ));
    }

    if strategy == ActionCommitStrategy::RequireApproval || guardrails.high_risk {
        let approval = PendingActionApproval {
            approval_id: staging_id.clone(),
            staging_id,
            staging_graph: staging,
            action_id: action_id.to_string(),
            created_at: now.to_rfc3339(),
            expires_at: (now + chrono::Duration::hours(ACTION_APPROVAL_TTL_HOURS)).to_rfc3339(),
        };
        if let Err(e) = kg.create_action_approval_for_claims(claims, &approval) {
            let _ = kg.drop_staging_for_claims(claims, &approval.staging_id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("审批记录创建失败，已回滚: {e}"),
                vec![],
            ));
        }
        return Ok(StagingCommitOutcome::Pending(approval));
    }

    // 自动提交：合并影子图到生产图，再删除影子图。
    if let Err(e) = kg.commit_staging_for_claims(claims, &staging_id) {
        let _ = kg.drop_staging_for_claims(claims, &staging_id);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("影子图合并到生产图失败: {e}"),
            vec![],
        ));
    }
    let _ = kg.drop_staging_for_claims(claims, &staging_id);

    Ok(StagingCommitOutcome::Committed(json!({
        "sandbox": "staging_graph",
        "staging_graph": staging,
        "guardrails_passed": true,
    })))
}

fn approval_is_expired(approval: &PendingActionApproval) -> bool {
    chrono::DateTime::parse_from_rfc3339(&approval.expires_at)
        .map(|expires_at| expires_at <= chrono::Utc::now())
        .unwrap_or(true)
}

fn cleanup_action_approval(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    approval: &PendingActionApproval,
) {
    let _ = kg.drop_staging_for_claims(claims, &approval.staging_id);
    let _ = kg.delete_action_approval_for_claims(claims, &approval.approval_id);
}

/// GET /api/v1/ontology/action-approvals — pending approvals in the caller's
/// verified tenant/project scope. Expired approvals are lazily discarded.
pub(crate) async fn list_action_approvals_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
) -> impl IntoResponse {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let approvals = match kg.list_action_approvals_for_claims(claims) {
        Ok(approvals) => approvals,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let active: Vec<_> = approvals
        .into_iter()
        .filter(|approval| {
            if approval_is_expired(approval) {
                cleanup_action_approval(&kg, claims, approval);
                false
            } else {
                true
            }
        })
        .collect();
    let _ = state.kg_store.flush();
    (StatusCode::OK, Json(json!({ "approvals": active }))).into_response()
}

/// POST /api/v1/ontology/action-approvals/:approval_id/approve
pub(crate) async fn approve_action_approval_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(approval_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    resolve_action_approval(&state, identity, approval_id, true).await
}

/// POST /api/v1/ontology/action-approvals/:approval_id/reject
pub(crate) async fn reject_action_approval_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(approval_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    resolve_action_approval(&state, identity, approval_id, false).await
}

async fn resolve_action_approval(
    state: &Arc<AppState>,
    identity: UserIdentity,
    approval_id: String,
    approve: bool,
) -> axum::response::Response {
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims().into_response(),
    };
    let kg = match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
        Ok(kg) => kg,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let approval = match kg.list_action_approvals_for_claims(claims) {
        Ok(approvals) => approvals
            .into_iter()
            .find(|approval| approval.approval_id == approval_id),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    };
    let Some(approval) = approval else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "approval not found", "approval_id": approval_id })),
        )
            .into_response();
    };
    if approval_is_expired(&approval) {
        cleanup_action_approval(&kg, claims, &approval);
        let _ = state.kg_store.flush();
        return (
            StatusCode::GONE,
            Json(json!({ "error": "approval expired", "approval_id": approval_id })),
        )
            .into_response();
    }
    if approve {
        if let Err(e) = kg.commit_staging_for_claims(claims, &approval.staging_id) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("approval merge failed: {e}") })),
            )
                .into_response();
        }
    }
    if let Err(e) = kg.drop_staging_for_claims(claims, &approval.staging_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("approval staging cleanup failed: {e}") })),
        )
            .into_response();
    }
    if let Err(e) = kg.delete_action_approval_for_claims(claims, &approval.approval_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("approval metadata cleanup failed: {e}") })),
        )
            .into_response();
    }
    let _ = state.kg_store.flush();
    (
        StatusCode::OK,
        Json(json!({
            "status": if approve { "approved" } else { "rejected" },
            "approval_id": approval.approval_id,
            "staging_graph": approval.staging_graph,
        })),
    )
        .into_response()
}

pub(crate) async fn invoke_action_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(action_id): axum::extract::Path<String>,
    Json(req): Json<ActionInvokeRequest>,
) -> impl IntoResponse {
    use crate::knowledge_graph::ontology_store::OntologyStore;
    let claims = match identity.isolation_claims() {
        Some(claims) => claims,
        None => return unauthorized_isolation_claims(),
    };
    // 执行分派解耦：动作定义改从存储读取（首启幂等 seed），存储不可用时回退硬编码。
    let ont = (|| {
        let store = OntologyStore::with_shared_store(state.kg_store.clone()).ok()?;
        store.ensure_seeded(ONT_DOMAIN).ok()?;
        store.load_definition(ONT_DOMAIN).ok()
    })()
    .unwrap_or_else(crate::knowledge_graph::ontology_layer::ev_repair_ontology);
    let action = match ont.action_types.iter().find(|a| a.id == action_id) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("未知动作类型: {}", action_id) })),
            )
        }
    };

    // 自定义动作暂不可执行：声明已存于本体，但无内置执行逻辑（待阶段3 通用执行器）。
    if !BUILTIN_EXECUTABLE_ACTIONS.contains(&action_id.as_str()) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": format!("动作「{}」为自定义声明，暂不可执行", action.label),
                "executable": false,
                "reason": "custom_action_not_executable",
            })),
        );
    }

    // 1. 参数校验：必填项存在且非空。
    let missing: Vec<String> = action
        .parameters
        .iter()
        .filter(|p| p.required && p_str(&req.params, &p.name).is_none())
        .map(|p| p.name.clone())
        .collect();
    if !missing.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少必填参数", "missing": missing })),
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

    // 2. 前置条件 + 3. 组装 side-effect 写回 SPARQL（按动作分派）。
    let now = chrono::Utc::now();
    let (statements, result_meta) =
        match build_action_effects(&action_id, &req, &kg, claims, &now.to_rfc3339()) {
            Ok(v) => v,
            Err((code, msg)) => return (code, Json(json!({ "error": msg }))),
        };

    if req.dry_run {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "dry_run",
                "action": action_id,
                "graph": claims.graph_iri().expect("verified claims were validated"),
                "sparql": statements.iter().map(ClaimsGraphUpdate::sparql).collect::<Vec<_>>(),
                "result": result_meta,
            })),
        );
    }

    // 4. 数据沙箱写回：先写影子图 → 护栏后校验 → 通过才合并到生产图，失败即回滚。
    let guardrails = match ontology_guardrails::effective_config(&ont, &action) {
        Ok(guardrails) => guardrails,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    };
    let outcome = match commit_via_staging(
        &kg,
        claims,
        &statements,
        req.commit_strategy,
        &action_id,
        now,
        &guardrails,
    ) {
        Ok(outcome) => outcome,
        Err((code, msg, violations)) => {
            return (
                code,
                Json(json!({ "error": msg, "violations": violations })),
            )
        }
    };
    let (status, sandbox) = match outcome {
        StagingCommitOutcome::Committed(report) => ("ok", report),
        StagingCommitOutcome::Pending(approval) => (
            "pending_approval",
            json!({
                "sandbox": "staging_graph",
                "staging_graph": approval.staging_graph,
                "guardrails_passed": true,
                "approval_id": approval.approval_id,
                "expires_at": approval.expires_at,
            }),
        ),
    };
    let _ = state.kg_store.flush();

    (
        StatusCode::OK,
        Json(json!({
            "status": status,
            "action": action_id,
            "graph": claims.graph_iri().expect("verified claims were validated"),
            "applied": statements.len(),
            "result": result_meta,
            "sandbox": sandbox,
        })),
    )
}

/// 按动作类型组装前置条件校验 + side-effect 写回 SPARQL 语句序列。
fn build_action_effects(
    action_id: &str,
    req: &ActionInvokeRequest,
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    now: &str,
) -> Result<(Vec<ClaimsGraphUpdate>, Value), (StatusCode, String)> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    match action_id {
        // 依据已确诊故障码为车辆创建维修工单，并建立 forVehicle / diagnoses 链接。
        "GenerateRepairOrder" => {
            let fault_code = req
                .target
                .clone()
                .ok_or_else(|| bad("缺少 target（故障码主键）".into()))?;
            let vin = p_str(&req.params, "vehicle_vin").unwrap();
            let vehicle_iri = ev_instance_iri("Vehicle", &vin);
            // 车辆为业务对象：当前回退命名图校验，未来经 MCP 业务库校验（见 resolve_object_exists）。
            if !resolve_object_exists(kg, claims, "Vehicle", &vin) {
                return Err(bad(format!("前置条件不满足：车辆VIN不存在于图谱 ({vin})")));
            }
            let fault_iri = ev_instance_iri("FaultCode", &fault_code);
            if !resolve_object_exists(kg, claims, "FaultCode", &fault_code) {
                return Err(bad(format!(
                    "前置条件不满足：故障码未确诊/不存在 ({fault_code})"
                )));
            }
            let order_id = format!("RO-{}", uuid::Uuid::new_v4().hyphenated());
            let order_iri = ev_instance_iri("RepairOrder", &order_id);
            let mut triples = vec![
                format!(
                    "<{o}> a <{c}>",
                    o = order_iri,
                    c = ev_term_iri("RepairOrder")
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("order_id"),
                    v = lit(&order_id)
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("vehicle_vin"),
                    v = lit(&vin)
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("fault_code"),
                    v = lit(&fault_code)
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("status"),
                    v = lit("待处理")
                ),
                format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("created_at"),
                    v = lit(now)
                ),
                format!(
                    "<{o}> <{l}> <{veh}>",
                    o = order_iri,
                    l = ev_term_iri("forVehicle"),
                    veh = vehicle_iri
                ),
                format!(
                    "<{o}> <{l}> <{f}>",
                    o = order_iri,
                    l = ev_term_iri("diagnoses"),
                    f = fault_iri
                ),
                format!(
                    "<{o}> <{lbl}> {v}",
                    o = order_iri,
                    lbl = RDFS_LABEL,
                    v = lit(&order_id)
                ),
            ];
            if let Some(a) = p_str(&req.params, "assigned_to") {
                triples.push(format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("assigned_to"),
                    v = lit(&a)
                ));
            }
            if let Some(c) = p_num(&req.params, "estimated_cost") {
                triples.push(format!(
                    "<{o}> <{p}> {v}",
                    o = order_iri,
                    p = ev_prop_iri("estimated_cost"),
                    v = lit_decimal(c)
                ));
            }
            let stmt = ClaimsGraphUpdate::insert_data(format!("{} .", triples.join(" .\n")));
            Ok((
                vec![stmt],
                json!({ "order_id": order_id, "order_iri": order_iri, "vehicle": vehicle_iri, "fault_code": fault_iri }),
            ))
        }
        // 检测后写回电池 SOH（0-100），并记录更新时间。
        "UpdateBatterySoh" => {
            let battery_id = p_str(&req.params, "battery_id").unwrap();
            let soh = p_num(&req.params, "soh").ok_or_else(|| bad("soh 必须为数值".into()))?;
            if !(0.0..=100.0).contains(&soh) {
                return Err(bad("前置条件不满足：SOH 取值需在 0-100".into()));
            }
            let bat_iri = ev_instance_iri("Battery", &battery_id);
            // 电池为业务对象：当前回退命名图校验，未来经 MCP 业务库校验。
            if !resolve_object_exists(kg, claims, "Battery", &battery_id) {
                return Err(bad(format!(
                    "前置条件不满足：电池对象不存在 ({battery_id})"
                )));
            }
            let mut stmts = upsert_prop_stmts(&bat_iri, &ev_prop_iri("soh"), &lit_decimal(soh));
            stmts.extend(upsert_prop_stmts(
                &bat_iri,
                &ev_prop_iri("soh_updated_at"),
                &lit(now),
            ));
            Ok((stmts, json!({ "battery": bat_iri, "soh": soh })))
        }
        // 对存在批次性缺陷的车型打召回标记。
        "MarkRecall" => {
            let model_id = p_str(&req.params, "model_id").unwrap();
            let reason = p_str(&req.params, "recall_reason").unwrap();
            let model_iri = ev_instance_iri("VehicleModel", &model_id);
            if !resolve_object_exists(kg, claims, "VehicleModel", &model_id) {
                return Err(bad(format!("前置条件不满足：车型对象不存在 ({model_id})")));
            }
            let mut stmts = upsert_prop_stmts(&model_iri, &ev_prop_iri("recalled"), &lit("true"));
            stmts.extend(upsert_prop_stmts(
                &model_iri,
                &ev_prop_iri("recall_reason"),
                &lit(&reason),
            ));
            stmts.extend(upsert_prop_stmts(
                &model_iri,
                &ev_prop_iri("recall_marked_at"),
                &lit(now),
            ));
            Ok((
                stmts,
                json!({ "model": model_iri, "recalled": true, "recall_reason": reason }),
            ))
        }
        // 将一次诊断沉淀为 FAQ，挂接到对应故障码。
        "AppendFaq" => {
            let code = req
                .target
                .clone()
                .or_else(|| p_str(&req.params, "code"))
                .ok_or_else(|| bad("缺少 target/code（故障码主键）".into()))?;
            let question = p_str(&req.params, "question").unwrap();
            let answer = p_str(&req.params, "answer").unwrap();
            let fault_iri = ev_instance_iri("FaultCode", &code);
            if !resolve_object_exists(kg, claims, "FaultCode", &code) {
                return Err(bad(format!("前置条件不满足：故障码对象不存在 ({code})")));
            }
            let faq_id = format!("FAQ-{}", uuid::Uuid::new_v4().hyphenated());
            let faq_iri = ev_instance_iri("FAQ", &faq_id);
            let triples = [
                format!("<{f}> a <{c}>", f = faq_iri, c = ev_term_iri("FAQ")),
                format!(
                    "<{f}> <{p}> {o}",
                    f = faq_iri,
                    p = ev_prop_iri("faq_id"),
                    o = lit(&faq_id)
                ),
                format!(
                    "<{f}> <{p}> {o}",
                    f = faq_iri,
                    p = ev_prop_iri("question"),
                    o = lit(&question)
                ),
                format!(
                    "<{f}> <{p}> {o}",
                    f = faq_iri,
                    p = ev_prop_iri("answer"),
                    o = lit(&answer)
                ),
                format!(
                    "<{f}> <{lbl}> {o}",
                    f = faq_iri,
                    lbl = RDFS_LABEL,
                    o = lit(&question)
                ),
                format!(
                    "<{fc}> <{l}> <{f}>",
                    fc = fault_iri,
                    l = ev_term_iri("relatedFaq"),
                    f = faq_iri
                ),
            ];
            let stmt = ClaimsGraphUpdate::insert_data(format!("{} .", triples.join(" .\n")));
            Ok((
                vec![stmt],
                json!({ "faq_id": faq_id, "faq_iri": faq_iri, "fault_code": fault_iri }),
            ))
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            format!("动作 {action_id} 暂未实现执行器"),
        )),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ontology CRUD 集成测试（原与 skill_manifest_tests 混放，随 skills 拆分迁出后独立）
// ──────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod ontology_crud_tests {
    use super::*;
    use crate::core::core_types::{CoreConfig, SemanticCore};
    use crate::tools::prompt_registry::PromptRegistry;
    use axum::http::StatusCode;
    use axum::{
        routing::{get, post, put},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use tower::ServiceExt;

    use super::super::{api_gov::ApiUsageState, AppState, TEST_ENV_LOCK};

    fn make_state(tmp: &std::path::Path) -> Arc<AppState> {
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
        let kg_store = Arc::new(oxigraph::store::Store::new().unwrap());
        Arc::new(AppState {
            core,
            gateway,
            kg_store,
            config_info: Arc::new(tokio::sync::RwLock::new(serde_json::json!({}))),
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
        })
    }

    fn test_jwt(tenant: &str) -> String {
        encode(
            &Header::default(),
            &super::super::iam::JwtClaims {
                sub: "ontology-tester".to_string(),
                tenant_id: tenant.to_string(),
                project_id: Some("repair".to_string()),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
        )
        .unwrap()
    }

    /// 阶段0 无回归：GET /api/v1/ontology/types 改读 Oxigraph 元命名图后，
    /// 响应须与硬编码 ev_repair_ontology() 逐字段一致（首启由 ensure_seeded 幂等 seed）。
    #[tokio::test]
    async fn test_ontology_types_matches_hardcoded() {
        let tmp = std::env::temp_dir().join(format!("agentos_ont_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/v1/ontology/types")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        let ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let expected = json!({
            "domain": ont.domain,
            "counts": {
                "object_types": ont.object_types.len(),
                "link_types": ont.link_types.len(),
                "action_types": ont.action_types.len(),
                "functions": ont.functions.len(),
            },
            "object_types": ont.object_types,
            "link_types": ont.link_types,
            "action_types": ont.action_types,
            "functions": ont.functions,
        });
        assert_eq!(body, expected, "存储驱动的响应须与硬编码逐字段一致");

        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 阶段1 CRUD：新建对象 → GET 可见 → 删除被引用返回 409 → 删链接后可删对象。
    #[tokio::test]
    async fn isolation_contract_ontology_write_requires_jwt_and_uses_claims_scope() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_ontcrud_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .route(
                "/api/v1/ontology/object-types",
                post(upsert_object_type_handler),
            )
            .route(
                "/api/v1/ontology/object-types/:id",
                put(update_object_type_handler).delete(delete_object_type_handler),
            )
            .route(
                "/api/v1/ontology/link-types",
                post(upsert_link_type_handler),
            )
            .route(
                "/api/v1/ontology/link-types/:id",
                put(update_link_type_handler).delete(delete_link_type_handler),
            )
            .with_state(state);

        let post_json = |uri: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri.to_string())
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };
        let del = |uri: &str| {
            axum::http::Request::builder()
                .method("DELETE")
                .uri(uri.to_string())
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap()
        };

        // 1) 新建对象 Widget
        let obj = json!({
            "id": "Widget", "iri": "https://agentos.ontology/ev/Widget",
            "label": "小部件", "description": "测试", "icon": "Box", "color": "blue",
            "primary_key": "name", "title_property": "name", "properties": []
        });
        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/object-types")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(obj.to_string()))
            .unwrap();
        let r = app.clone().oneshot(unauthenticated).await.unwrap();
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "无 JWT 的 upsert 必须拒绝"
        );

        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/object-types", obj))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建对象应 200");

        // 2) GET 可见
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let has_widget = body["object_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == "Widget");
        assert!(has_widget, "新建对象应出现在 GET /types");

        // 3) 新建引用 Widget 的链接（Widget→Widget）
        let link = json!({
            "id": "WidgetSelf", "iri": "https://agentos.ontology/ev/WidgetSelf",
            "label": "自关联", "description": "", "source": "Widget", "target": "Widget",
            "cardinality": "one_to_many"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/link-types", link))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建链接应 200");

        // 4) 删对象被引用 → 409
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/object-types/Widget"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT, "被链接引用应返回 409");

        // 5) 删链接后可删对象
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/link-types/WidgetSelf"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/object-types/Widget"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "无引用后应可删");

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 阶段2 CRUD + 执行分派解耦：新建动作/函数 → GET 可见 → 自定义动作 invoke 返回 422
    /// （不可执行）→ 内置动作 dry_run 仍可执行 → 删除动作/函数回归。
    #[tokio::test]
    async fn test_ontology_action_function_crud() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_ontaf_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
        let token = test_jwt("tenant-a");
        let app = Router::new()
            .route("/api/v1/ontology/types", get(ontology_types_handler))
            .route(
                "/api/v1/ontology/action-types",
                post(upsert_action_type_handler),
            )
            .route(
                "/api/v1/ontology/action-types/:id",
                put(update_action_type_handler).delete(delete_action_type_handler),
            )
            .route(
                "/api/v1/ontology/function-defs",
                post(upsert_function_def_handler),
            )
            .route(
                "/api/v1/ontology/function-defs/:id",
                put(update_function_def_handler).delete(delete_function_def_handler),
            )
            .route(
                "/api/v1/ontology/actions/:id/invoke",
                post(invoke_action_handler),
            )
            .with_state(state);

        let post_json = |uri: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri.to_string())
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };
        let del = |uri: &str| {
            axum::http::Request::builder()
                .method("DELETE")
                .uri(uri.to_string())
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap()
        };

        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/actions/GenerateRepairOrder/invoke")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({}).to_string()))
            .unwrap();
        let r = app.clone().oneshot(unauthenticated).await.unwrap();
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "无 JWT 的 invoke 必须拒绝"
        );

        // 1) 新建自定义动作（applies_to=FaultCode，已 seed）
        let action = json!({
            "id": "TagFault", "iri": "https://agentos.ontology/ev/action/TagFault",
            "label": "标记故障", "description": "测试自定义动作", "applies_to": "FaultCode",
            "parameters": [], "preconditions": [], "side_effects": [], "icon": "Zap"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/action-types", action))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建动作应 200");

        // 2) 新建函数
        let func = json!({
            "id": "FaultScore", "label": "故障评分", "description": "测试函数",
            "applies_to": "FaultCode", "returns": "number", "expression": "1 + 1"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/function-defs", func))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "新建函数应 200");

        // 3) applies_to 不存在对象 → 400
        let bad = json!({
            "id": "Bad", "iri": "https://agentos.ontology/ev/action/Bad",
            "label": "坏动作", "description": "", "applies_to": "NoSuchObj",
            "parameters": [], "preconditions": [], "side_effects": [], "icon": "Zap"
        });
        let r = app
            .clone()
            .oneshot(post_json("/api/v1/ontology/action-types", bad))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "applies_to 不存在应 400"
        );

        // 4) GET 可见
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/types")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["action_types"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == "TagFault"),
            "新建动作应出现在 GET /types"
        );
        assert!(
            body["functions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "FaultScore"),
            "新建函数应出现在 GET /types"
        );

        // 5) 自定义动作 invoke → 422（不可执行）
        let r = app
            .clone()
            .oneshot(post_json(
                "/api/v1/ontology/actions/TagFault/invoke",
                json!({ "dry_run": true }),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "自定义动作应不可执行 422"
        );

        // 6) 内置动作 dry_run 仍可（缺必填参数 → 400，证明走到内置执行分派而非 422）
        let r = app
            .clone()
            .oneshot(post_json(
                "/api/v1/ontology/actions/GenerateRepairOrder/invoke",
                json!({ "dry_run": true }),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "内置动作缺必填参数应 400（证明未被 422 拦截）"
        );

        // 7) 删除动作/函数
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/action-types/TagFault"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "删动作应 200");
        let r = app
            .clone()
            .oneshot(del("/api/v1/ontology/function-defs/FaultScore"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "删函数应 200");

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn isolation_contract_ontology_actions_are_invisible_cross_tenant() {
        let tmp = std::env::temp_dir().join(format!("agentos_ontinvoke_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state = make_state(&tmp);
        let claims_a =
            crate::isolation::IsolationClaims::from_verified("tenant-a", "repair", "tester")
                .unwrap();
        let claims_b =
            crate::isolation::IsolationClaims::from_verified("tenant-b", "repair", "tester")
                .unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        let seed = format!(
            "<{}> a <{}> . <{}> a <{}> .",
            ev_instance_iri("Vehicle", "LVIN123"),
            ev_term_iri("Vehicle"),
            ev_instance_iri("FaultCode", "P0A80"),
            ev_term_iri("FaultCode"),
        );
        kg.update_for_claims(&claims_a, &ClaimsGraphUpdate::insert_data(seed))
            .unwrap();

        let app = Router::new()
            .route(
                "/api/v1/ontology/actions/:id/invoke",
                post(invoke_action_handler),
            )
            .with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/ontology/actions/GenerateRepairOrder/invoke")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", test_jwt("tenant-a")))
            .body(axum::body::Body::from(
                json!({"target": "P0A80", "params": {"vehicle_vin": "LVIN123"}}).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let query = format!(
            "SELECT ?o WHERE {{ ?o a <{}> }}",
            ev_term_iri("RepairOrder")
        );
        assert!(!kg
            .query_sparql_for_claims(&claims_a, &query)
            .unwrap()
            .is_empty());
        assert!(kg
            .query_sparql_for_claims(&claims_b, &query)
            .unwrap()
            .is_empty());

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn action_approval_keeps_staging_until_same_scope_approves() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_approval_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state = make_state(&tmp);
        let claims_a =
            crate::isolation::IsolationClaims::from_verified("tenant-a", "repair", "tester")
                .unwrap();
        let kg = KnowledgeGraphStore::with_shared_store(state.kg_store.clone()).unwrap();
        let seed = format!(
            "<{}> a <{}> . <{}> a <{}> .",
            ev_instance_iri("Vehicle", "LVIN123"),
            ev_term_iri("Vehicle"),
            ev_instance_iri("FaultCode", "P0A80"),
            ev_term_iri("FaultCode"),
        );
        kg.update_for_claims(&claims_a, &ClaimsGraphUpdate::insert_data(seed))
            .unwrap();

        let app = Router::new()
            .route(
                "/api/v1/ontology/actions/:id/invoke",
                post(invoke_action_handler),
            )
            .route(
                "/api/v1/ontology/action-approvals",
                get(list_action_approvals_handler),
            )
            .route(
                "/api/v1/ontology/action-approvals/:approval_id/approve",
                post(approve_action_approval_handler),
            )
            .route(
                "/api/v1/ontology/action-approvals/:approval_id/reject",
                post(reject_action_approval_handler),
            )
            .with_state(state);
        let post = |uri: String, token: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };

        let unauthenticated = axum::http::Request::builder()
            .uri("/api/v1/ontology/action-approvals")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let invoke = post(
            "/api/v1/ontology/actions/GenerateRepairOrder/invoke".to_string(),
            &test_jwt("tenant-a"),
            json!({
                "target": "P0A80",
                "params": {"vehicle_vin": "LVIN123"},
                "commit_strategy": "require_approval"
            }),
        );
        let response = app.clone().oneshot(invoke).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "pending_approval");
        let approval_id = body["sandbox"]["approval_id"].as_str().unwrap().to_string();

        let orders = format!(
            "SELECT ?o WHERE {{ ?o a <{}> }}",
            ev_term_iri("RepairOrder")
        );
        assert!(
            kg.query_sparql_for_claims(&claims_a, &orders)
                .unwrap()
                .is_empty(),
            "require_approval must not alter the production graph"
        );

        let list = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/ontology/action-approvals")
                    .header("authorization", format!("Bearer {}", test_jwt("tenant-a")))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let list_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(list.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(list_body["approvals"].as_array().unwrap().len(), 1);

        let cross_tenant = app
            .clone()
            .oneshot(post(
                format!("/api/v1/ontology/action-approvals/{approval_id}/approve"),
                &test_jwt("tenant-b"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);

        let approved = app
            .clone()
            .oneshot(post(
                format!("/api/v1/ontology/action-approvals/{approval_id}/approve"),
                &test_jwt("tenant-a"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(approved.status(), StatusCode::OK);
        assert!(
            !kg.query_sparql_for_claims(&claims_a, &orders)
                .unwrap()
                .is_empty(),
            "approval must merge the retained staging graph"
        );

        let reject_invoke = post(
            "/api/v1/ontology/actions/GenerateRepairOrder/invoke".to_string(),
            &test_jwt("tenant-a"),
            json!({
                "target": "P0A80",
                "params": {"vehicle_vin": "LVIN123"},
                "commit_strategy": "require_approval"
            }),
        );
        let response = app.clone().oneshot(reject_invoke).await.unwrap();
        let reject_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let reject_id = reject_body["sandbox"]["approval_id"].as_str().unwrap();
        let rejected = app
            .clone()
            .oneshot(post(
                format!("/api/v1/ontology/action-approvals/{reject_id}/reject"),
                &test_jwt("tenant-a"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::OK);
        assert_eq!(
            kg.query_sparql_for_claims(&claims_a, &orders)
                .unwrap()
                .len(),
            1,
            "reject must discard staged writes"
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}

/// 动力层执行器（ActionType invoke）单测：参数/前置条件校验 + SPARQL 组装。
#[cfg(test)]
mod ontology_action_tests {
    use super::*;
    use crate::isolation::IsolationClaims;
    use crate::knowledge_graph::store::KnowledgeGraphStore;
    use oxigraph::store::Store;

    fn test_claims(tenant: &str) -> IsolationClaims {
        IsolationClaims::from_verified(tenant, "repair", "tester").unwrap()
    }

    /// 预置车辆/故障码/电池/车型实例于调用方的 claims 图。
    fn seeded_kg(claims: &IsolationClaims) -> KnowledgeGraphStore {
        let store = Arc::new(Store::new().unwrap());
        let seed = format!(
            "\
             <{veh}> a <{vehc}> . \
             <{fault}> a <{faultc}> . \
             <{bat}> a <{batc}> . \
             <{model}> a <{modelc}> . \
             ",
            veh = ev_instance_iri("Vehicle", "LVIN123"),
            vehc = ev_term_iri("Vehicle"),
            fault = ev_instance_iri("FaultCode", "P0A80"),
            faultc = ev_term_iri("FaultCode"),
            bat = ev_instance_iri("Battery", "BAT-001"),
            batc = ev_term_iri("Battery"),
            model = ev_instance_iri("VehicleModel", "M-001"),
            modelc = ev_term_iri("VehicleModel"),
        );
        let kg = KnowledgeGraphStore::with_shared_store(store).unwrap();
        kg.update_for_claims(claims, &ClaimsGraphUpdate::insert_data(seed))
            .unwrap();
        kg
    }

    fn mk_req(target: Option<&str>, params: Value, dry_run: bool) -> ActionInvokeRequest {
        ActionInvokeRequest {
            target: target.map(|s| s.to_string()),
            params: params.as_object().cloned().unwrap_or_default(),
            dry_run,
            commit_strategy: ActionCommitStrategy::Auto,
        }
    }

    #[test]
    fn test_iri_safe_and_instance_iri() {
        assert_eq!(iri_safe("P0A80"), "P0A80");
        assert_eq!(iri_safe("a b"), "a_b");
        assert_eq!(
            ev_instance_iri("Vehicle", "X 1"),
            "https://agentos.ontology/ev/Vehicle/X_1"
        );
        assert_eq!(ev_prop_iri("soh"), "https://agentos.ontology/ev/prop/soh");
    }

    #[test]
    fn test_generate_repair_order_ok() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            Some("P0A80"),
            json!({"vehicle_vin": "LVIN123", "assigned_to": "张工", "estimated_cost": 1200}),
            false,
        );
        let (stmts, meta) = build_action_effects(
            "GenerateRepairOrder",
            &r,
            &kg,
            &claims,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert_eq!(stmts.len(), 1);
        let s = &stmts[0];
        assert!(s.sparql().contains("RepairOrder"));
        assert!(s.sparql().contains("forVehicle"));
        assert!(s.sparql().contains("diagnoses"));
        assert!(s.sparql().contains("张工"));
        assert!(s.sparql().contains("1200"));
        assert!(meta["order_id"].as_str().unwrap().starts_with("RO-"));
    }

    #[test]
    fn test_generate_repair_order_missing_vehicle_precondition() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(Some("P0A80"), json!({"vehicle_vin": "UNKNOWN"}), false);
        let err = build_action_effects("GenerateRepairOrder", &r, &kg, &claims, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("车辆VIN不存在"));
    }

    #[test]
    fn test_generate_repair_order_missing_target() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(None, json!({"vehicle_vin": "LVIN123"}), false);
        let err = build_action_effects("GenerateRepairOrder", &r, &kg, &claims, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    // ── 数据沙箱（staging-graph）单测 ──────────────────────────────────

    #[test]
    fn test_effects_do_not_select_a_graph() {
        let update = ClaimsGraphUpdate::insert_data("<a> <b> <c>");
        assert!(!update.sparql().to_uppercase().contains("GRAPH"));
    }

    /// 合法写回：经影子图护栏通过 → 合并到生产图；影子图删除、生产图可见新数据。
    #[test]
    fn test_sandbox_commit_merges_to_production() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(Some("P0A80"), json!({"vehicle_vin": "LVIN123"}), false);
        let (stmts, _meta) = build_action_effects(
            "GenerateRepairOrder",
            &r,
            &kg,
            &claims,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        let guardrails = ontology_guardrails::effective_config(&ont, action).unwrap();
        let outcome = commit_via_staging(
            &kg,
            &claims,
            &stmts,
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .expect("护栏应通过并提交");
        let StagingCommitOutcome::Committed(report) = outcome else {
            panic!("auto 策略必须直接提交");
        };
        assert_eq!(report["guardrails_passed"], json!(true));

        // claims 图应能查到新建的维修工单类型三元组。
        let q = format!(
            "SELECT ?o WHERE {{ ?o a <{}> }}",
            ev_term_iri("RepairOrder")
        );
        let rows = kg.query_sparql_for_claims(&claims, &q).unwrap();
        assert!(!rows.is_empty(), "生产图应可见已提交的维修工单");
        let tenant_b = test_claims("tenant-b");
        assert!(
            kg.query_sparql_for_claims(&tenant_b, &q)
                .unwrap()
                .is_empty(),
            "tenant B must not see tenant A invoke writes"
        );

        // 影子图应已删除（无残留）。
        let staging_id = report["staging_graph"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap();
        let sq = "SELECT ?s WHERE { ?s ?p ?o }";
        assert!(
            kg.query_staging_for_claims(&claims, staging_id, sq)
                .unwrap()
                .is_empty(),
            "影子图应已清理"
        );
    }

    /// 越权谓词：护栏应拦截并回滚（返回 422），生产图零改动。
    #[test]
    fn test_sandbox_rollback_on_foreign_predicate() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        // 统计 claims 图当前三元组数（回滚后应不变）。
        let count_q = "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }";
        let before = kg.query_sparql_for_claims(&claims, count_q).unwrap()[0]["?c"]
            .as_str()
            .unwrap()
            .to_string();

        // 构造带越权谓词（不在白名单命名空间）的语句。
        let foreign = ClaimsGraphUpdate::insert_data(
            "<https://agentos.ontology/ev/X/1> <http://evil.example/pwn> \"x\"",
        );
        let ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        let guardrails = ontology_guardrails::effective_config(&ont, action).unwrap();
        let err = commit_via_staging(
            &kg,
            &claims,
            &[foreign],
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err.2.iter().any(|v| v.contains("越权谓词")));

        // 生产图三元组数不变（已回滚）。
        let after = kg.query_sparql_for_claims(&claims, count_q).unwrap()[0]["?c"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(before, after, "回滚后生产图不应有任何改动");
    }

    #[test]
    fn test_action_whitelist_override_rejects_otherwise_allowed_predicate() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let mut ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter_mut()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        action.guardrails.allowed_predicate_prefixes =
            Some(vec!["https://agentos.ontology/ev/prop/".into()]);
        let action = action.clone();
        let guardrails = ontology_guardrails::effective_config(&ont, &action).unwrap();
        let permitted_by_default = ClaimsGraphUpdate::insert_data(
            "<https://agentos.ontology/ev/X/1> <https://agentos.ontology/ev/custom> \"x\"",
        );
        let err = commit_via_staging(
            &kg,
            &claims,
            &[permitted_by_default],
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err
            .2
            .iter()
            .any(|violation| violation.starts_with("predicate_whitelist:")));
    }

    #[test]
    fn test_assertion_failure_rolls_back_staging_graph() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let before = kg
            .query_sparql_for_claims(&claims, "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }")
            .unwrap()[0]["?c"]
            .clone();
        let mut ont = crate::knowledge_graph::ontology_layer::ev_repair_ontology();
        let action = ont
            .action_types
            .iter_mut()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        action.guardrails.assertions.push(
            crate::knowledge_graph::ontology_layer::SparqlAskAssertion {
                code: "no_staging_writes".into(),
                query: "ASK { ?s ?p ?o }".into(),
            },
        );
        let action = action.clone();
        let guardrails = ontology_guardrails::effective_config(&ont, &action).unwrap();
        let write = ClaimsGraphUpdate::insert_data(
            "<https://agentos.ontology/ev/X/1> <https://agentos.ontology/ev/prop/value> \"x\"",
        );
        let err = commit_via_staging(
            &kg,
            &claims,
            &[write],
            ActionCommitStrategy::Auto,
            "GenerateRepairOrder",
            chrono::Utc::now(),
            &guardrails,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err
            .2
            .iter()
            .any(|violation| violation.starts_with("assertion:no_staging_writes:")));
        let after = kg
            .query_sparql_for_claims(&claims, "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }")
            .unwrap()[0]["?c"]
            .clone();
        assert_eq!(before, after, "断言失败后生产图不应有任何改动");
    }

    #[test]
    fn test_invoke_payload_cannot_override_guardrails() {
        assert!(serde_json::from_value::<ActionInvokeRequest>(json!({
            "dry_run": true,
            "guardrails": { "max_triples": 0 }
        }))
        .is_err());
    }

    #[test]
    fn test_update_battery_soh_ok_and_range() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let ok = mk_req(None, json!({"battery_id": "BAT-001", "soh": 87.5}), false);
        let (stmts, meta) =
            build_action_effects("UpdateBatterySoh", &ok, &kg, &claims, "t").unwrap();
        assert_eq!(stmts.len(), 4); // soh upsert(2) + soh_updated_at upsert(2)
        assert!(stmts.iter().any(|s| s.sparql().contains("DELETE WHERE")));
        assert!(stmts.iter().any(|s| s.sparql().contains("87.5")));
        assert_eq!(meta["soh"], 87.5);

        let bad = mk_req(None, json!({"battery_id": "BAT-001", "soh": 150}), false);
        let err = build_action_effects("UpdateBatterySoh", &bad, &kg, &claims, "t").unwrap_err();
        assert!(err.1.contains("0-100"));
    }

    #[test]
    fn test_update_battery_soh_missing_battery() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(None, json!({"battery_id": "NOPE", "soh": 50}), false);
        let err = build_action_effects("UpdateBatterySoh", &r, &kg, &claims, "t").unwrap_err();
        assert!(err.1.contains("电池对象不存在"));
    }

    #[test]
    fn test_mark_recall_ok() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            None,
            json!({"model_id": "M-001", "recall_reason": "电池批次缺陷"}),
            false,
        );
        let (stmts, meta) = build_action_effects("MarkRecall", &r, &kg, &claims, "t").unwrap();
        assert_eq!(stmts.len(), 6); // 三个属性各 upsert(2)
        assert!(stmts.iter().any(|s| s.sparql().contains("recalled")));
        assert!(stmts.iter().any(|s| s.sparql().contains("电池批次缺陷")));
        assert_eq!(meta["recalled"], true);
    }

    #[test]
    fn test_append_faq_ok_and_links_fault() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            Some("P0A80"),
            json!({"question": "报警怎么办？", "answer": "请尽快检修"}),
            false,
        );
        let (stmts, meta) = build_action_effects("AppendFaq", &r, &kg, &claims, "t").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].sparql().contains("relatedFaq"));
        assert!(stmts[0].sparql().contains("报警怎么办"));
        assert!(meta["faq_id"].as_str().unwrap().starts_with("FAQ-"));
    }

    #[test]
    fn test_append_faq_missing_fault_precondition() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(
            Some("NON_EXIST"),
            json!({"question": "q", "answer": "a"}),
            false,
        );
        let err = build_action_effects("AppendFaq", &r, &kg, &claims, "t").unwrap_err();
        assert!(err.1.contains("故障码对象不存在"));
    }

    #[test]
    fn test_unknown_action() {
        let claims = test_claims("tenant-a");
        let kg = seeded_kg(&claims);
        let r = mk_req(None, json!({}), false);
        let err = build_action_effects("NoSuchAction", &r, &kg, &claims, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }
}
