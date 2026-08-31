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
use crate::knowledge_graph::store::KnowledgeGraphStore;
use crate::memory::hyperspace_store::HybridSearchFilter;

use super::api_gov;
use super::{expand_iri, AppState};

#[derive(Deserialize)]
pub struct AgentChatRequest {
    pub message: String,
    #[serde(default)]
    pub images: Vec<String>,
}

/// 仅保留 ASCII 字母/数字/下划线组成、长度≥3 且包含数字或下划线（或长度≥4）的片段，
/// 作为故障码检索 token；可有效命中 APP_w009 / P0A80 等代码而排除普通停用词。
fn extract_code_tokens(message: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= 3 {
            let has_digit = cur.chars().any(|c| c.is_ascii_digit());
            if has_digit || cur.contains('_') || cur.len() >= 4 {
                out.push(cur.to_lowercase());
            }
        }
        cur.clear();
    };
    for ch in message.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            flush(&mut cur, &mut tokens);
        }
    }
    flush(&mut cur, &mut tokens);
    tokens.dedup();
    tokens
}

/// 将用户问题中的品牌别名映射为图谱中的品牌 label（如 特斯拉→Tesla）。
fn extract_brand_labels(message: &str) -> Vec<String> {
    let lower = message.to_lowercase();
    let mut out = Vec::new();
    let table: [(&[&str], &str); 6] = [
        (&["特斯拉", "tesla"], "Tesla"),
        (&["比亚迪", "byd"], "比亚迪"),
        (&["蔚来", "nio"], "蔚来"),
        (&["小鹏", "xpeng"], "小鹏"),
        (&["理想", "li auto", "lixiang"], "理想"),
        (&["问界", "aito"], "问界"),
    ];
    for (aliases, label) in table {
        if aliases
            .iter()
            .any(|a| message.contains(*a) || lower.contains(&a.to_lowercase()))
        {
            out.push(label.to_string());
        }
    }
    out
}

const ONT_FAULT: &str = "http://aps.local/ontology/FaultCode";
const ONT_BRAND_REL: &str = "http://aps.local/ontology/belongsToBrand";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const META: &str = "https://agentos.ontology/meta";

/// 构造检索 FaultCode 的 SPARQL；filter_expr 为已组装好的 FILTER 条件表达式。
fn build_fault_query(filter_expr: &str, limit: usize) -> String {
    format!(
        "SELECT ?code ?label ?meaning ?can_drive ?repair ?models ?brand WHERE {{ \
            ?n a <{f}> . \
            ?n <{m}/code> ?code . \
            OPTIONAL {{ ?n <{rl}> ?label }} \
            OPTIONAL {{ ?n <{m}/meaning> ?meaning }} \
            OPTIONAL {{ ?n <{m}/can_drive> ?can_drive }} \
            OPTIONAL {{ ?n <{m}/repair> ?repair }} \
            OPTIONAL {{ ?n <{m}/models> ?models }} \
            OPTIONAL {{ ?n <{br}> ?bn . ?bn <{rl}> ?brand }} \
            FILTER( {flt} ) \
        }} LIMIT {lim}",
        f = ONT_FAULT,
        m = META,
        rl = RDFS_LABEL,
        br = ONT_BRAND_REL,
        flt = filter_expr,
        lim = limit,
    )
}

fn trunc(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        t.to_string()
    } else {
        t.chars().take(n).collect::<String>() + "…"
    }
}

/// POST /api/v1/agents/:id/chat — 基于该 Agent 绑定知识图谱的检索增强问答（RAG）。
/// 流程：定位 Agent → 抽取故障码/品牌 token → SPARQL 检索 FaultCode 事实 →
/// 决策层（Phase 4）：将诊断出的故障码意图映射到适用的动力层 ActionType。
///
/// 取首个命中的故障码作为动作目标（applies_to=FaultCode 的动作），生成「建议动作」供前端
/// 渲染「诊断 → 建议 → 一键执行」。`requires_business_data=true` 的动作（如生成维修工单需车辆
/// VIN 等业务数据）当前工单系统尚未接入，前端仅弹窗占位，不直接落库。
fn build_action_suggestions(sources: &[Value]) -> Vec<Value> {
    let code = sources
        .first()
        .and_then(|s| s.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if code.is_empty() {
        return Vec::new();
    }
    vec![
        json!({
            "action": "GenerateRepairOrder",
            "label": "生成维修工单",
            "icon": "Wrench",
            "target": code,
            "requires_business_data": true,
            "note": "需车辆VIN等业务数据，工单系统对接中（规划中）",
            "reason": format!("针对诊断故障码 {code} 一键生成维修工单"),
        }),
        json!({
            "action": "AppendFaq",
            "label": "沉淀为常见问答",
            "icon": "MessageCirclePlus",
            "target": code,
            "requires_business_data": false,
            "reason": format!("将本次诊断沉淀为故障码 {code} 的 FAQ"),
        }),
    ]
}

/// POST /api/v1/agents/:id/chat — 内部单轮 RAG 问答（管理面，无入站鉴权）。
pub(crate) async fn agent_chat_handler(
    State(state): State<Arc<AppState>>,
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
    let (status, body) = run_agent_rag(&state, &id, &message, &req.images).await;
    (status, Json(body))
}

/// Agent RAG 检索上下文：检索完成、提示已组装，待（同步或流式）调用 LLM。
struct RagContext {
    messages: Vec<ChatMessage>,
    sources: Vec<Value>,
    retrieved: usize,
    vector_retrieved: usize,
    grounded: bool,
    /// 网关不可用时的图谱直出回退答案（有命中时才有）。
    fallback_answer: Option<String>,
    suggested_actions: Vec<Value>,
    /// 本次实际调用的真实型号名（按 model_mounts 选模型解析，回退旧 model/default）。
    model: String,
}

/// 解析 Agent 在指定能力槽上实际调用的真实型号名。
/// 依 `keys` 顺序读 `model_mounts[key]` → `config_info.models.resources[id].model`；
/// 均未命中时回退旧 `agent.model`（单模型），再回退 `gateway.default_model()`。
async fn resolve_agent_model(state: &Arc<AppState>, agent: &Value, keys: &[&str]) -> String {
    let mounts = agent.get("model_mounts");
    let resources = {
        let cfg = state.config_info.read().await;
        cfg.get("models")
            .and_then(|m| m.get("resources"))
            .and_then(|v| v.as_array())
            .cloned()
    };
    if let (Some(mounts), Some(resources)) = (mounts, resources.as_ref()) {
        for key in keys {
            let res_id = match mounts
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                Some(r) => r,
                None => continue,
            };
            if let Some(model) = resources
                .iter()
                .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(res_id))
                .and_then(|r| r.get("model").and_then(|v| v.as_str()))
                .filter(|s| !s.is_empty())
            {
                return model.to_string();
            }
        }
    }
    agent
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.gateway.default_model())
}

/// 单轮 RAG 检索与提示组装：定位 Agent → 图/向量检索 → 组装上下文与提示消息。
/// 返回可复用的 RagContext，供内部 chat、对外 public chat、SSE 流式与 OpenAI 兼容层共用。
/// `images` 为随消息透传的图片 URL 列表（非空即走 VL：选 vision 模型 + 组多部件 user 消息）。
async fn build_rag_context(
    state: &Arc<AppState>,
    id: &str,
    message: &str,
    images: &[String],
) -> Result<RagContext, (StatusCode, Value)> {
    let message = message.to_string();
    let has_image = !images.is_empty();

    // 1. 定位 Agent（用户态优先，其次批处理静态）。
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
    let agent_name = agent
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("维修助手")
        .to_string();
    // 选模型：有图走 vision 槽（回退 chat 槽），纯文本走 chat 槽；均未命中回退旧 model/default。
    let model_keys: &[&str] = if has_image {
        &["vision", "chat"]
    } else {
        &["chat"]
    };
    let selected_model = resolve_agent_model(state, &agent, model_keys).await;
    // 2a. 解析 Agent 的知识来源：展开知识包 → 命名图集合 + 向量命名空间集合。
    let mut graph_iris: Vec<String> = Vec::new();
    let mut vector_namespaces: Vec<String> = Vec::new();
    let pack_ids: Vec<String> = agent
        .get("knowledge_pack_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !pack_ids.is_empty() {
        let packs = state.knowledge_packs.read().await;
        let bases = state.knowledge_bases.read().await;
        let ids_of = |pack: &Value, key: &str| -> Vec<String> {
            pack.get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        for pid in &pack_ids {
            let pack = match packs
                .iter()
                .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(pid.as_str()))
            {
                Some(p) => p,
                None => continue,
            };
            if let Some(g) = pack
                .get("named_graph")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                graph_iris.push(expand_iri(g));
            }
            if let Some(ns) = pack
                .get("vector_namespace")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                vector_namespaces.push(ns.to_string());
            }
            for gid in ids_of(pack, "graph_kb_ids") {
                if let Some(kb) = bases
                    .iter()
                    .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(gid.as_str()))
                {
                    if let Some(g) = kb
                        .get("graph")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        graph_iris.push(expand_iri(g));
                    }
                }
            }
            for vid in ids_of(pack, "vector_kb_ids") {
                if let Some(kb) = bases
                    .iter()
                    .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(vid.as_str()))
                {
                    if let Some(ns) = kb
                        .get("vector_namespace")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        vector_namespaces.push(ns.to_string());
                    }
                }
            }
        }
    }
    graph_iris.sort();
    graph_iris.dedup();
    vector_namespaces.sort();
    vector_namespaces.dedup();

    // 2b. 图知识库检索：对每个命名图执行故障码/品牌召回（先按故障码，全空再按品牌）。
    let mut rows: Vec<Value> = Vec::new();
    {
        let kg = state.kg_store.clone();
        if let Ok(store) = KnowledgeGraphStore::with_shared_store(kg) {
            let codes = extract_code_tokens(&message);
            let brands = extract_brand_labels(&message);
            if !codes.is_empty() {
                let conds: Vec<String> = codes
                    .iter()
                    .map(|t| format!("CONTAINS(LCASE(STR(?code)), \"{}\")", t))
                    .collect();
                let q = build_fault_query(&conds.join(" || "), 6);
                for graph_iri in &graph_iris {
                    rows.extend(store.query_sparql(&q, Some(graph_iri)).unwrap_or_default());
                }
            }
            if rows.is_empty() && !brands.is_empty() {
                let conds: Vec<String> = brands
                    .iter()
                    .map(|b| format!("CONTAINS(STR(?brand), \"{}\")", b.replace('"', "")))
                    .collect();
                let q = format!(
                    "SELECT ?code ?label ?meaning ?can_drive ?repair ?models ?brand WHERE {{ \
                        ?n a <{f}> . \
                        ?n <{m}/code> ?code . \
                        ?n <{br}> ?bn . ?bn <{rl}> ?brand . \
                        OPTIONAL {{ ?n <{rl}> ?label }} \
                        OPTIONAL {{ ?n <{m}/meaning> ?meaning }} \
                        OPTIONAL {{ ?n <{m}/can_drive> ?can_drive }} \
                        OPTIONAL {{ ?n <{m}/repair> ?repair }} \
                        OPTIONAL {{ ?n <{m}/models> ?models }} \
                        FILTER( {flt} ) \
                    }} LIMIT 6",
                    f = ONT_FAULT,
                    m = META,
                    rl = RDFS_LABEL,
                    br = ONT_BRAND_REL,
                    flt = conds.join(" || "),
                );
                for graph_iri in &graph_iris {
                    rows.extend(store.query_sparql(&q, Some(graph_iri)).unwrap_or_default());
                }
            }
        }
    }

    // 2c. 向量知识库检索：对每个命名空间做语义相似检索（向量库启用时）。
    let mut vector_hits: Vec<(String, f32)> = Vec::new();
    if let Some(vstore) = state.vector_store.load_full() {
        for ns in &vector_namespaces {
            let filter = HybridSearchFilter::new().with_named_graph(ns.clone());
            if let Ok(hits) = vstore.search_with_filter(&message, &filter, 5).await {
                for h in hits {
                    vector_hits.push((h.text, h.score));
                }
            }
        }
    }

    // 3. 组装检索事实上下文（图知识库 + 向量知识库）。
    let get = |row: &Value, k: &str| {
        row.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut facts = String::new();
    let mut sources: Vec<Value> = Vec::new();
    for row in &rows {
        let code = get(row, "?code");
        let label = get(row, "?label");
        let brand = get(row, "?brand");
        facts.push_str(&format!(
            "- 故障码 {code}（{brand}）：{label}\n  含义：{}\n  能否行驶：{}\n  维修建议：{}\n  适用车型：{}\n",
            trunc(&get(row, "?meaning"), 300),
            trunc(&get(row, "?can_drive"), 200),
            trunc(&get(row, "?repair"), 300),
            trunc(&get(row, "?models"), 160),
        ));
        sources.push(json!({ "code": code, "label": label, "brand": brand }));
    }
    // 向量检索命中作为补充事实（不并入 sources，避免污染故障码来源与动作建议）。
    let mut vector_facts = String::new();
    for (text, score) in &vector_hits {
        vector_facts.push_str(&format!("- （相关度 {:.2}）{}\n", score, trunc(text, 400)));
    }
    let vector_retrieved = vector_hits.len();

    // 4. 构造提示并调用 LLM 网关。
    let sys = format!(
        "你是「{agent_name}」，一名专业的新能源汽车故障诊断与维修助手。请严格依据下方“知识库检索结果”，\
用简体中文回答用户问题：解释故障含义、是否可继续行驶、维修建议与适用车型。\
若检索结果为空或不足以支撑回答，请如实说明并给出通用排查建议，切勿编造具体故障码信息。\
回答需专业、严谨、条理清晰。"
    );
    let graph_section = if facts.is_empty() {
        "【知识图谱检索结果】\n（未检索到相关故障码记录）\n".to_string()
    } else {
        format!("【知识图谱检索结果】\n{facts}")
    };
    let vector_section = if vector_facts.is_empty() {
        String::new()
    } else {
        format!("\n【向量知识库检索结果】\n{vector_facts}")
    };
    let user_content = format!("{graph_section}{vector_section}\n【用户问题】\n{message}");
    // 有图时组多部件 user 消息（文本 + 各 image_url），否则退化为纯文本。
    let user_msg = if has_image {
        let mut parts = vec![ChatContent::part_text(user_content)];
        for u in images {
            parts.push(ChatContent::image(u.clone()));
        }
        ChatMessage {
            role: "user".into(),
            content: ChatContent::Parts(parts),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    } else {
        ChatMessage {
            role: "user".into(),
            content: user_content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    };
    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: sys.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        },
        user_msg,
    ];

    // 网关不可用时的图谱直出回退（有命中才提供）。
    let fallback_answer = rows.first().map(|row| {
        format!(
            "【基于知识图谱的检索结果】\n故障码 {}（{}）：{}\n含义：{}\n能否行驶：{}\n维修建议：{}\n适用车型：{}",
            get(row, "?code"), get(row, "?brand"), get(row, "?label"),
            get(row, "?meaning"), get(row, "?can_drive"), get(row, "?repair"), get(row, "?models"),
        )
    });
    Ok(RagContext {
        suggested_actions: build_action_suggestions(&sources),
        grounded: !rows.is_empty(),
        retrieved: rows.len(),
        vector_retrieved,
        fallback_answer,
        sources,
        messages,
        model: selected_model,
    })
}

/// 单轮 RAG（同步）：检索 → 调 LLM 网关生成简体中文回答。返回 (状态码, JSON 响应体)。
async fn run_agent_rag(
    state: &Arc<AppState>,
    id: &str,
    message: &str,
    images: &[String],
) -> (StatusCode, Value) {
    let rc = match build_rag_context(state, id, message, images).await {
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
            (
                StatusCode::OK,
                json!({
                    "status": "ok",
                    "answer": answer,
                    "grounded": rc.grounded,
                    "sources": rc.sources,
                    "retrieved": rc.retrieved,
                    "vector_retrieved": rc.vector_retrieved,
                    "model": rc.model,
                    "suggested_actions": rc.suggested_actions,
                }),
            )
        }
        Err(e) => {
            // 网关失败但已检索到事实时，回退为基于图谱的确定性回答，保证可用性。
            if let Some(fallback) = rc.fallback_answer {
                (
                    StatusCode::OK,
                    json!({
                        "status": "degraded",
                        "answer": fallback,
                        "grounded": true,
                        "sources": rc.sources,
                        "retrieved": rc.retrieved,
                        "vector_retrieved": rc.vector_retrieved,
                        "warning": format!("LLM 网关不可用，已回退为图谱直出：{}", e),
                        "suggested_actions": rc.suggested_actions,
                    }),
                )
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    json!({ "error": format!("LLM 网关调用失败：{}", e) }),
                )
            }
        }
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
    let (status, body) = run_agent_rag(&state, &id, &message, &[]).await;
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

/// 基于已完成检索的 RagContext，调用网关流式接口并逐 token 下发 SSE；
/// 尾部下发汇总（原生 done / OpenAI 结束 chunk + [DONE]），并在流结束后落审计。
/// `guard` 随流移动、于流结束时归还并发额度。
#[allow(clippy::too_many_arguments)]
fn build_sse_response(
    state: Arc<AppState>,
    ctx: api_gov::ApiCallerContext,
    id: String,
    endpoint: &'static str,
    started: std::time::Instant,
    rc: RagContext,
    guard: api_gov::ConcurrencyGuard,
    shape: StreamShape,
    report_model: String,
) -> axum::response::Response {
    let llm_model = rc.model.clone();
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
        // 流式失败或无产出且有图谱命中 → 回退图谱直出，保证可用性。
        if full.is_empty() {
            if let Some(fb) = &rc.fallback_answer {
                full = fb.clone();
                yield Ok(delta_event(shape, &chat_id, created, &report_model, fb));
            }
        }
        // 尾包。
        match shape {
            StreamShape::Native => {
                yield Ok(Event::default().event("done").data(
                    json!({
                        "answer": full,
                        "grounded": rc.grounded,
                        "sources": rc.sources,
                        "retrieved": rc.retrieved,
                        "vector_retrieved": rc.vector_retrieved,
                        "model": llm_model,
                        "suggested_actions": rc.suggested_actions,
                    })
                    .to_string(),
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
    let rc = match build_rag_context(&state, &id, &message, &[]).await {
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
fn openai_completion_json(model: &str, answer: &str) -> axum::response::Response {
    (
        StatusCode::OK,
        Json(json!({
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
        })),
    )
        .into_response()
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

/// POST /v1/chat/completions — OpenAI 兼容问答（model=agentId，取末条 user 内容做单轮 RAG）。
pub(crate) async fn openai_chat_completions_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<OpenAiChatRequest>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let id = req.model.trim().to_string();
    if id.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "model (agentId) 不能为空",
            "invalid_request_error",
        );
    }
    let endpoint: &'static str = if req.stream {
        "chat_completions_stream"
    } else {
        "chat_completions"
    };
    let (ctx, guard) = match public_gate(&state, &headers, &id, endpoint, started).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let last_user = req.messages.iter().rev().find(|m| m.role == "user");
    let message = last_user
        .map(|m| m.content.as_text().trim().to_string())
        .unwrap_or_default();
    // 提取末条 user 消息内的图片 URL(image_url 部件),供 VL 透传给 build_rag_context。
    let images: Vec<String> = last_user
        .map(|m| m.content.image_urls())
        .unwrap_or_default();
    if message.is_empty() {
        write_public_audit(&ctx, &id, endpoint, 400, started, "empty_message");
        return openai_error(
            StatusCode::BAD_REQUEST,
            "messages 中缺少非空 user 内容",
            "invalid_request_error",
        );
    }
    let rc = match build_rag_context(&state, &id, &message, &images).await {
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
    if req.stream {
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
            openai_completion_json(&id, &answer)
        }
        Err(e) => {
            if let Some(fb) = rc.fallback_answer {
                write_public_audit(&ctx, &id, endpoint, 200, started, "degraded");
                openai_completion_json(&id, &fb)
            } else {
                write_public_audit(&ctx, &id, endpoint, 502, started, "error");
                openai_error(
                    StatusCode::BAD_GATEWAY,
                    format!("LLM 网关调用失败：{}", e),
                    "api_error",
                )
            }
        }
    }
}
