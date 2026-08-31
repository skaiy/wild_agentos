use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::batch::manager::BatchAgentManager;
use crate::blob::BlobStore;
use crate::core::core_types::SemanticCore;
use crate::gateway::unified_gateway::UnifiedGateway;
use crate::knowledge_graph::rdf_mapper::RdfMapper;
use crate::knowledge_graph::store::KnowledgeGraphStore;
use crate::knowledge_graph::types::{EdgeDef, LLMExtractionOutput, NodeDef};
use crate::memory::hyperspace_store::HyperspaceStore;
use crate::memory::l2_blackboard::QueryFilter;
use crate::tools::prompt_registry::PromptRegistry;

/// Shared handle to the platform Batch Agent manager (Option<..> allows tests to omit it,
/// inner Option matches the gRPC server's take-on-shutdown lifecycle).
pub type SharedBatchManager = Arc<tokio::sync::Mutex<Option<BatchAgentManager>>>;

/// 可原子热替换的向量库句柄：HTTP 路由 / gRPC / 任务执行器 / SA 工具链共享同一容器。
/// 内层 `None` 表示向量检索禁用（embedding 初始化失败或尚未配置）。
/// embedding 配置变更时通过 `ArcSwapOption::store` 原子换入新维度的库，无需重启进程。
pub type SharedVectorStore = Arc<arc_swap::ArcSwapOption<HyperspaceStore>>;

pub mod iam;
use iam::UserIdentity;
pub mod api_gov;
use api_gov::{ApiClient, ApiKey, ApiUsageState};
pub mod api_clients;
pub mod prompts;
pub mod agents;
pub mod tasks;
pub mod runtime;
pub mod mcp;
pub mod guard;
pub mod skills;
pub mod ontology;
pub mod kb;
pub mod chat;

use agents::{
    create_agent_handler, delete_agent_handler, list_agents_handler, load_user_agents,
    migrate_legacy_agent_graphs, save_user_agents, update_agent_handler,
};
use tasks::{
    create_task_handler, get_execution_details_handler, get_realtime_status_handler,
    get_task_handler, list_task_trends_handler, stream_task_handler,
};
use runtime::{
    health_handler, live_runtime_hardening_fields, metrics_handler, unified_stats_handler,
};
use mcp::{
    list_mcp_servers_handler, load_mcp_servers, register_mcp_server_handler,
};
use guard::{guard_audit_handler, guard_stats_handler};
use skills::{
    delete_skill_handler, import_git_skill_handler, list_pipeline_runs_handler,
    list_skills_handler, load_user_skills, pipeline_rerun_handler, register_skill_handler,
    skill_manifest_handler,
};
use ontology::{
    delete_action_type_handler, delete_function_def_handler, delete_link_type_handler,
    delete_object_type_handler, invoke_action_handler, ontology_types_handler,
    update_action_type_handler, update_function_def_handler, update_link_type_handler,
    update_object_type_handler, upsert_action_type_handler, upsert_function_def_handler,
    upsert_link_type_handler, upsert_object_type_handler,
};
use kb::{
    create_kb_category_handler, create_knowledge_base_handler, create_knowledge_pack_handler,
    delete_kb_category_handler, delete_knowledge_base_handler, delete_knowledge_pack_handler,
    import_graph_knowledge_base_handler, ingest_knowledge_base_handler, kb_document_raw_handler,
    knowledge_base_stats_handler, list_kb_categories_handler, list_kb_documents_handler,
    list_knowledge_bases_handler, list_knowledge_packs_handler, load_kb_categories,
    load_knowledge_bases, load_knowledge_packs, reindex_knowledge_base_handler,
    save_knowledge_packs, search_knowledge_base_handler, spawn_reindex_all_vector_kbs,
    update_kb_category_handler, update_knowledge_base_handler, update_knowledge_pack_handler,
    upload_knowledge_base_handler, KB_UPLOAD_MAX_BYTES,
};
use chat::{
    agent_chat_handler, openai_chat_completions_handler, openai_list_models_handler,
    public_agent_chat_handler, public_agent_chat_stream_handler,
};

use api_clients::{
    create_api_client_handler, delete_api_client_handler, issue_api_key_handler,
    list_api_audit_handler, list_api_clients_handler, revoke_api_key_handler,
    update_api_client_handler,
};
use prompts::{
    activate_prompt_handler, canary_prompt_handler, create_prompt_handler, delete_prompt_handler,
    list_prompts_handler, resolve_prompt_handler,
};

pub struct AppState {
    pub core: Arc<SemanticCore>,
    pub gateway: Arc<UnifiedGateway>,
    pub kg_store: Arc<oxigraph::store::Store>,
    /// 已脱敏的运行期配置快照（不含 api_key 明文），支持前端 PUT 写回并持久化到
    /// data/config_override.json（重启后由 Settings::load() 读取生效）。
    pub config_info: Arc<tokio::sync::RwLock<Value>>,
    /// 批处理 Agent 列表（静态，来自启动配置）。
    pub agents_info: Value,
    /// MCP 服务器注册表（运行期动态写入）。
    pub mcp_servers: Arc<tokio::sync::RwLock<Vec<Value>>>,
    /// 用户态 Agent 注册表（运行期可增删改，持久化到 data/agents.json）。
    pub user_agents: Arc<tokio::sync::RwLock<Vec<Value>>>,
    /// Prompt/模型灰度版本注册表（G6'）。
    pub prompts: Arc<PromptRegistry>,
    /// 知识库分类注册表（运行期可增删改，持久化到 data/kb_categories.json）。
    pub kb_categories: Arc<tokio::sync::RwLock<Vec<Value>>>,
    /// 知识库注册表（向量/图，运行期可增删，持久化到 data/knowledge_bases.json）。
    pub knowledge_bases: Arc<tokio::sync::RwLock<Vec<Value>>>,
    /// 知识包注册表（运行期可增删改，持久化到 data/knowledge_packs.json；首启由内置包种子化）。
    pub knowledge_packs: Arc<tokio::sync::RwLock<Vec<Value>>>,
    /// 向量库（HyperspaceStore，按 embedding 配置初始化；内层 None 表示向量检索禁用）。
    /// 采用 `ArcSwapOption` 以支持 embedding 配置热切换时原子换库（见 `hot_reload_vector_store`）。
    pub vector_store: SharedVectorStore,
    /// 原文对象存储（BlobStore：MinIO 或 LocalFs 兜底）。为 None 时上传不落原文，仅向量化。
    pub blob_store: Option<Arc<dyn BlobStore>>,
    /// 任务执行器（productized 抽象）：由 build_http_router 注入，驱动 SA 跑 PDCA 管线并向共享事件总线推送执行事件。
    /// 为 None 时（仅测试态）流式任务不会真正执行，处理器会即时推送 TASK_FAILED 以避免前端卡在「启动中」。
    pub task_executor: Option<Arc<dyn TaskExecutor>>,
    /// 平台级批处理 Agent 管理器（方案A 运维态）：由 build_http_router 注入，None 表示测试态或未启用。
    pub batch_manager: Option<SharedBatchManager>,
    /// 入站调用方注册表（运行期可增删改，持久化到 data/api_clients.json）。
    pub api_clients: Arc<tokio::sync::RwLock<Vec<ApiClient>>>,
    /// 入站密钥注册表（仅存哈希，持久化到 data/api_keys.json）。
    pub api_keys: Arc<tokio::sync::RwLock<Vec<ApiKey>>>,
    /// 进程内限流/配额/并发用量状态（对外调用面）。
    pub api_usage: Arc<ApiUsageState>,
}

/// 流式任务执行规格：由 HTTP 流处理器构造并传入执行器。
#[derive(Clone)]
pub struct TaskExecSpec {
    pub prompt: String,
    pub task_iri: String,
    pub include_thought: bool,
    pub include_tool_calls: bool,
}

/// 任务执行器抽象：把「触发并驱动一次任务端到端执行」与 HTTP 传输层解耦。
///
/// 实现方（`api::grpc::server::HttpTaskExecutor`）持有已运行服务的共享运行态
/// （EventBus / Blackboard / Gateway / 内存分层等），在后台跑 SA 的 PDCA 管线，
/// 并把执行事件发布到**同一条**共享事件总线，供 `stream_task_handler` 的 SSE 循环转发给前端。
#[async_trait::async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(&self, spec: TaskExecSpec);
}

/// 持久化数据目录；可由 AGENTOS_DATA_DIR 覆盖（便于测试隔离），缺省为 "data"。
pub(crate) fn data_dir() -> std::path::PathBuf {
    std::env::var("AGENTOS_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("data"))
}

/// 序列化所有依赖进程级环境变量（AGENTOS_DATA_DIR / AGENTOS_AUTH_STRICT）的测试——
/// 避免并行执行时 env 被相互覆盖导致落盘/读取或鉴权模式错乱。各测试在函数入口取锁并持有到结束。
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());


/// Prompt 版本注册表的持久化文件路径。
fn prompts_store_path() -> std::path::PathBuf {
    data_dir().join("prompts.json")
}


/// 运行期配置覆盖文件路径；由 PUT /api/v1/config 写入，启动时被 Settings::load() 作为
/// 高于 config.yaml 的来源读取。路径与 Settings::load 中的 "data/config_override" 保持一致。
fn config_override_path() -> std::path::PathBuf {
    data_dir().join("config_override.json")
}

/// 将网关配置持久化到运行期覆盖文件，重启后由 Settings::load() 生效。
/// 将持久化所有字段（包括 api_key），保留覆盖文件其余段落。
fn save_config_override(patch: &Value) -> std::io::Result<()> {
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
        // 不持久化空 api_key，避免覆盖 config.yaml 中已配置的密钥。
        if clean
            .get("api_key")
            .and_then(|v| v.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(false)
        {
            clean.remove("api_key");
        }

        if let Some(obj) = root.as_object_mut() {
            let existing_gateway = obj.entry("gateway").or_insert(json!({}));
            if let Some(existing_gw_obj) = existing_gateway.as_object_mut() {
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
            json_deep_merge(existing, &clean);
        }
    }

    // Models 段:整体替换 providers/resources(集合语义,避免深合并残留已删项);
    // 空/缺失 provider.api_key 时回填 root 中同 id 的旧 key,避免误清空。
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
                        if let (Some(o), Some(ok)) = (p.as_object_mut(), old_p.get("api_key")) {
                            if ok.as_str().map(|s| !s.is_empty()).unwrap_or(false) {
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
fn json_deep_merge(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                json_deep_merge(d.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (d, s) => *d = s.clone(),
    }
}


#[derive(Deserialize)]
pub struct NodeWriteRequest {
    pub task_iri: String,
    pub json_ld: String,
    pub created_by: Option<String>,
}

#[derive(Deserialize)]
pub struct ProjectionRequest {
    pub task_iri: String,
    pub frame_name: Option<String>,
    pub params: Option<HashMap<String, String>>,
}


#[derive(Deserialize)]
pub struct KgImportRequest {
    pub nodes: Vec<NodeDef>,
    #[serde(default)]
    pub edges: Vec<EdgeDef>,
    pub graph: String,
    #[serde(default = "default_true")]
    pub clear_before: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
pub struct KgQueryRequest {
    pub sparql: String,
    pub named_graph: Option<String>,
}


/// 按 embedding 配置打开向量库（HyperspaceStore），包进可原子热替换的 `SharedVectorStore`。
/// 初始化失败则内层为 None（向量检索禁用，不影响图检索）。
/// 供 build_router 与 gRPC 服务共用，确保 HTTP 路由、任务执行器与 SA 工具链共享**同一个**可换库容器。
pub fn open_vector_store(
    embedding: &crate::config::settings::EmbeddingSettings,
) -> SharedVectorStore {
    let embed =
        crate::memory::embedding_service::create_embedding_service_from_config(embedding, 30);
    let vdir = data_dir().join("vector_store");
    match HyperspaceStore::open(&vdir, embed) {
        Ok(s) => Arc::new(arc_swap::ArcSwapOption::from_pointee(s)),
        Err(e) => {
            tracing::warn!("向量库初始化失败，向量检索禁用: {}", e);
            Arc::new(arc_swap::ArcSwapOption::empty())
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_router(
    core: Arc<SemanticCore>,
    gateway: Arc<UnifiedGateway>,
    kg_store: Arc<oxigraph::store::Store>,
    config_info: Value,
    agents_info: Value,
    vector_store: SharedVectorStore,
    task_executor: Option<Arc<dyn TaskExecutor>>,
    batch_manager: Option<SharedBatchManager>,
) -> Router {
    // 启动时加载用户态注册的技能并重新注册到内存技能表（默认技能由 SemanticCore 播种）。
    for skill in load_user_skills() {
        core.skills.register_skill(skill);
    }

    // 一次性幂等迁移：将存量 agent.knowledge_graph（旧「绑定知识图谱」）迁入知识包体系。
    let mut loaded_agents = load_user_agents();
    let mut loaded_packs = load_knowledge_packs();
    let (agents_migrated, packs_migrated) =
        migrate_legacy_agent_graphs(&mut loaded_agents, &mut loaded_packs);
    if agents_migrated {
        let _ = save_user_agents(&loaded_agents);
    }
    if packs_migrated {
        let _ = save_knowledge_packs(&loaded_packs);
    }

    let state = Arc::new(AppState {
        core,
        gateway,
        kg_store,
        config_info: Arc::new(tokio::sync::RwLock::new(config_info)),
        agents_info,
        mcp_servers: Arc::new(tokio::sync::RwLock::new(load_mcp_servers())),
        user_agents: Arc::new(tokio::sync::RwLock::new(loaded_agents)),
        prompts: Arc::new(PromptRegistry::load(prompts_store_path())),
        kb_categories: Arc::new(tokio::sync::RwLock::new(load_kb_categories())),
        knowledge_bases: Arc::new(tokio::sync::RwLock::new(load_knowledge_bases())),
        knowledge_packs: Arc::new(tokio::sync::RwLock::new(loaded_packs)),
        vector_store,
        blob_store: crate::blob::open_blob_store(),
        task_executor,
        batch_manager,
        api_clients: Arc::new(tokio::sync::RwLock::new(api_gov::load_api_clients())),
        api_keys: Arc::new(tokio::sync::RwLock::new(api_gov::load_api_keys())),
        api_usage: Arc::new(ApiUsageState::default()),
    });

    // 启动首灌：把持久化的 models 注册表灌入 gateway，使进程启动即按多 provider 生效。
    hot_reload_models(&state);

    Router::new()
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .route("/api/v1/memory/unified-stats", get(unified_stats_handler))
        .route(
            "/api/v1/config",
            get(config_handler).put(update_config_handler),
        )
        .route("/api/v1/tasks", post(create_task_handler))
        .route("/api/v1/tasks/:task_iri", get(get_task_handler))
        .route("/api/v1/tasks/stream", post(stream_task_handler))
        .route("/api/v1/tasks/trends", get(list_task_trends_handler))
        .route(
            "/api/v1/tasks/:task_iri/status",
            get(get_realtime_status_handler),
        )
        .route(
            "/api/v1/tasks/:task_iri/details",
            get(get_execution_details_handler),
        )
        .route("/api/v1/nodes", post(write_node_handler))
        .route("/api/v1/nodes/:node_iri", get(read_node_handler))
        .route("/api/v1/projections", post(get_projection_handler))
        .route("/api/v1/events", post(emit_event_handler))
        .route("/api/v1/batch/events", get(stream_batch_events_handler))
        // ── 方案A 平台运维态：L2 黑板浏览器（只读）+ 批处理 Agent 运维台 ──
        .route(
            "/api/v1/blackboard/tasks",
            get(list_blackboard_tasks_handler),
        )
        .route(
            "/api/v1/blackboard/nodes",
            get(list_blackboard_nodes_handler),
        )
        .route("/api/v1/batch/agents", get(list_batch_agents_handler))
        .route(
            "/api/v1/batch/agents/:name/control",
            post(control_batch_agent_handler),
        )
        .route(
            "/api/v1/skills",
            get(list_skills_handler)
                .post(register_skill_handler)
                .delete(delete_skill_handler),
        )
        .route("/api/v1/skills/manifest", get(skill_manifest_handler))
        .route("/api/v1/skills/import-git", post(import_git_skill_handler))
        .route(
            "/api/v1/skills/pipeline-runs",
            get(list_pipeline_runs_handler),
        )
        .route(
            "/api/v1/skills/pipeline-rerun",
            post(pipeline_rerun_handler),
        )
        .route("/api/v1/guard/audit", get(guard_audit_handler))
        .route("/api/v1/guard/stats", get(guard_stats_handler))
        .route("/api/v1/kg/import", post(kg_import_handler))
        .route("/api/v1/kg/query", post(kg_query_handler))
        // ── 本体层（Ontology Layer）：知识包 CRUD + 只读本体元数据 ──
        .route(
            "/api/v1/knowledge-packs",
            get(list_knowledge_packs_handler).post(create_knowledge_pack_handler),
        )
        .route(
            "/api/v1/knowledge-packs/:id",
            put(update_knowledge_pack_handler).delete(delete_knowledge_pack_handler),
        )
        .route("/api/v1/ontology/types", get(ontology_types_handler))
        // ── 本体元模型在线 CRUD（对象/链接）──
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
        // ── 本体元模型在线 CRUD（动作/函数）──
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
        // ── 知识库分类管理 CRUD ──
        .route(
            "/api/v1/kb/categories",
            get(list_kb_categories_handler).post(create_kb_category_handler),
        )
        .route(
            "/api/v1/kb/categories/:id",
            put(update_kb_category_handler).delete(delete_kb_category_handler),
        )
        // ── 知识库（向量/图）管理 ──
        .route(
            "/api/v1/kb/bases",
            get(list_knowledge_bases_handler).post(create_knowledge_base_handler),
        )
        .route(
            "/api/v1/kb/bases/:id",
            put(update_knowledge_base_handler).delete(delete_knowledge_base_handler),
        )
        .route(
            "/api/v1/kb/bases/:id/stats",
            get(knowledge_base_stats_handler),
        )
        .route(
            "/api/v1/kb/bases/:id/ingest",
            post(ingest_knowledge_base_handler),
        )
        .route(
            "/api/v1/kb/bases/:id/upload",
            post(upload_knowledge_base_handler).layer(DefaultBodyLimit::max(KB_UPLOAD_MAX_BYTES)),
        )
        .route(
            "/api/v1/kb/bases/:id/import-graph",
            post(import_graph_knowledge_base_handler)
                .layer(DefaultBodyLimit::max(KB_UPLOAD_MAX_BYTES)),
        )
        .route(
            "/api/v1/kb/bases/:id/reindex",
            post(reindex_knowledge_base_handler),
        )
        .route(
            "/api/v1/kb/bases/:id/documents",
            get(list_kb_documents_handler),
        )
        .route(
            "/api/v1/kb/bases/:id/documents/:doc_id/raw",
            get(kb_document_raw_handler),
        )
        .route(
            "/api/v1/kb/bases/:id/search",
            post(search_knowledge_base_handler),
        )
        // ── 图片入口（VL 多模态）：上传 + 只读代理 ──
        .route(
            "/api/v1/images/upload",
            post(upload_image_handler).layer(DefaultBodyLimit::max(IMAGE_UPLOAD_MAX_BYTES)),
        )
        .route("/api/v1/images/:image_id/raw", get(image_raw_handler))
        // ── 模型资源连通性测试 / 自动拉取型号 / 向量桥接（均不回显 api_key）──
        .route("/api/v1/models/test", post(test_model_handler))
        .route("/api/v1/providers/models", post(provider_models_handler))
        .route(
            "/api/v1/embedding/activate",
            post(activate_embedding_handler),
        )
        .route(
            "/api/v1/agents",
            get(list_agents_handler).post(create_agent_handler),
        )
        .route(
            "/api/v1/agents/:id",
            put(update_agent_handler).delete(delete_agent_handler),
        )
        .route("/api/v1/agents/:id/chat", post(agent_chat_handler))
        // ── 对外发布：Public API（入站密钥鉴权 + scope + 限流/配额 + 审计）──
        .route(
            "/api/v1/public/agents/:id/chat",
            post(public_agent_chat_handler),
        )
        .route(
            "/api/v1/public/agents/:id/chat/stream",
            post(public_agent_chat_stream_handler),
        )
        // ── OpenAI 兼容层（model = agentId，第三方 SDK 可直连）──
        .route("/v1/models", get(openai_list_models_handler))
        .route(
            "/v1/chat/completions",
            post(openai_chat_completions_handler),
        )
        // ── 调用方 & 密钥治理中心（管理面，需 DA 角色）──
        .route(
            "/api/v1/api-clients",
            get(list_api_clients_handler).post(create_api_client_handler),
        )
        .route(
            "/api/v1/api-clients/:id",
            put(update_api_client_handler).delete(delete_api_client_handler),
        )
        .route("/api/v1/api-clients/:id/keys", post(issue_api_key_handler))
        .route(
            "/api/v1/api-clients/:id/keys/:kid",
            delete(revoke_api_key_handler),
        )
        .route("/api/v1/api-audit", get(list_api_audit_handler))
        .route(
            "/api/v1/mcp/servers",
            get(list_mcp_servers_handler).post(register_mcp_server_handler),
        )
        // ── G6' Prompt/模型灰度版本管理 ──
        .route(
            "/api/v1/prompts",
            get(list_prompts_handler).post(create_prompt_handler),
        )
        .route("/api/v1/prompts/resolve", get(resolve_prompt_handler))
        .route(
            "/api/v1/prompts/:id/activate",
            post(activate_prompt_handler),
        )
        .route("/api/v1/prompts/:id/canary", put(canary_prompt_handler))
        .route("/api/v1/prompts/:id", delete(delete_prompt_handler))
        .with_state(state)
}


async fn config_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
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
    Json(info)
}

/// PUT /api/v1/config — 更新运行期配置并持久化到 data/config_override.json（重启后由 Settings 生效）
/// Body: { "gateway": { "base_url": "...", "api_key": "...", "default_model": "...", ... } }
async fn update_config_handler(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> impl IntoResponse {
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
                        if k == "api_key" {
                            // api_key_configured: 新 key 非空 OR 环境变量有配置（兜底）。
                            let new_key = v.as_str().unwrap_or("");
                            let env_key =
                                std::env::var("AGENT_OS_GATEWAY_API_KEY").unwrap_or_default();
                            gateway_obj.insert(
                                "api_key_configured".into(),
                                json!(!new_key.is_empty() || !env_key.is_empty()),
                            );
                        } else {
                            gateway_obj.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
        // Embedding 快照：深合并；oneapi.api_key 转为 api_key_configured，不回显明文。
        if let Some(emb_patch) = patch.get("embedding") {
            let mut clean = emb_patch.clone();
            if let Some(o) = clean.as_object_mut() {
                if let Some(oneapi) = o.get_mut("oneapi").and_then(|v| v.as_object_mut()) {
                    let key_now = oneapi.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
                    if oneapi.contains_key("api_key") {
                        oneapi.insert("api_key_configured".into(), json!(!key_now.is_empty()));
                        oneapi.remove("api_key");
                    }
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
            if let Some(provs) = clean.get_mut("providers").and_then(|v| v.as_array_mut()) {
                for p in provs.iter_mut() {
                    if let Some(o) = p.as_object_mut() {
                        let key_now = o
                            .get("api_key")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // 已配置：本次提供了非空 key，或此前已有同 id 的持久化 key。
                        let pid = o
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let prev_configured = info
                            .get("models")
                            .and_then(|m| m.get("providers"))
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter().any(|x| {
                                    x.get("id").and_then(|v| v.as_str()) == Some(&pid)
                                        && x.get("api_key_configured")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false)
                                })
                            })
                            .unwrap_or(false);
                        o.insert(
                            "api_key_configured".into(),
                            json!(!key_now.is_empty() || prev_configured),
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
fn hot_reload_models(state: &Arc<AppState>) {
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
async fn hot_reload_embedding(
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

async fn write_node_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<NodeWriteRequest>,
) -> impl IntoResponse {
    match state
        .core
        .write_node(&req.task_iri, &req.json_ld, None, req.created_by.as_deref())
        .await
    {
        Ok(node_iri) => (
            StatusCode::CREATED,
            Json(json!({"node_iri": node_iri, "accepted": true})),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"accepted": false, "error": e.to_string()})),
        ),
    }
}

async fn get_projection_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ProjectionRequest>,
) -> impl IntoResponse {
    let frame = req
        .frame_name
        .unwrap_or_else(|| "reference_only".to_string());
    let params = req.params.unwrap_or_default();
    match state
        .core
        .projection
        .project(&req.task_iri, &frame, params)
        .await
    {
        Ok(projection) => Json(json!({
            "projection": serde_json::from_str::<Value>(&projection).ok(),
            "frame": frame,
            "task_iri": req.task_iri,
        })),
        Err(e) => Json(json!({"error": e.to_string(), "task_iri": req.task_iri})),
    }
}

async fn read_node_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(node_iri): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.core.read_node(&node_iri).await {
        Ok(Some(node)) => Json(json!({
            "found": true,
            "json_ld": node.json_ld,
        })),
        Ok(None) => Json(json!({"found": false})),
        Err(e) => Json(json!({"found": false, "error": e.to_string()})),
    }
}

async fn emit_event_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let task_iri = payload
        .get("task_iri")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let event_type = payload
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("CUSTOM");
    let source = payload
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("http_api");
    let event_id = state
        .core
        .emit_event(task_iri, event_type, source, &payload.to_string())
        .await;
    Json(json!({"event_id": event_id, "status": "emitted"}))
}


async fn stream_batch_events_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let event_bus = state.core.events.clone();
    let mut rx = event_bus.subscribe();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !event.event_type.starts_with("BATCH_") {
                        continue;
                    }
                    let payload: Value =
                        serde_json::from_str(&event.payload).unwrap_or(Value::Null);
                    let data = json!({
                        "channel": "batch",
                        "event_type": event.event_type,
                        "source": event.source_agent_iri,
                        "task_iri": event.task_iri,
                        "timestamp": event.timestamp.to_rfc3339(),
                        "payload": payload,
                    });
                    yield Ok::<Event, Infallible>(
                        Event::default()
                            .event("batch")
                            .data(data.to_string()),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ============================================================
// 方案A 平台运维态：L2 黑板浏览器（只读）+ 批处理 Agent 运维台
// ============================================================


/// GET /api/v1/blackboard/tasks — 列出黑板上所有任务（平台/任务态，跨租户）。
async fn list_blackboard_tasks_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tasks = state.core.blackboard.list_task_summaries();
    Json(json!({ "count": tasks.len(), "tasks": tasks }))
}

#[derive(Debug, Deserialize)]
struct BlackboardNodesQuery {
    task_iri: String,
    role: Option<String>,
    node_type: Option<String>,
    cycle_id: Option<String>,
}

/// GET /api/v1/blackboard/nodes?task_iri=..&role=..&node_type=..&cycle_id=..
/// 读取指定任务下的节点（只读），支持角色/类型/周期多维过滤。task_iri 以查询参数传入以规避 IRI 内含斜杠。
async fn list_blackboard_nodes_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<BlackboardNodesQuery>,
) -> impl IntoResponse {
    let task_iri = q.task_iri.trim().to_string();
    if task_iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "task_iri 不能为空" })),
        );
    }
    let filter = QueryFilter {
        role: q.role.as_deref().and_then(|r| r.parse().ok()),
        cycle_id: q.cycle_id.clone().filter(|s| !s.is_empty()),
        node_type: q.node_type.clone().filter(|s| !s.is_empty()),
    };
    match state
        .core
        .blackboard
        .query_nodes_filtered(&task_iri, &filter)
    {
        Ok(nodes) => {
            let items: Vec<&crate::memory::l2_blackboard::Node> =
                nodes.iter().map(|n| n.as_ref()).collect();
            (
                StatusCode::OK,
                Json(json!({ "task_iri": task_iri, "count": items.len(), "nodes": items })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("读取节点失败: {e}") })),
        ),
    }
}

/// GET /api/v1/batch/agents — 列出所有批处理 Agent 及其状态/窗口/指标/配置摘要（平台运维态）。
async fn list_batch_agents_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mgr_arc = match &state.batch_manager {
        Some(m) => m.clone(),
        None => {
            return Json(json!({ "running": false, "count": 0, "agents": [] }));
        }
    };
    let guard = mgr_arc.lock().await;
    let mgr = match guard.as_ref() {
        Some(m) => m,
        None => return Json(json!({ "running": false, "count": 0, "agents": [] })),
    };
    let names: Vec<String> = mgr.list_agents().iter().map(|s| s.to_string()).collect();
    let agents: Vec<Value> = names
        .iter()
        .map(|name| {
            let status = mgr.get_status(name);
            let window = mgr.get_window_status(name);
            let metrics = mgr.get_metrics(name);
            let cfg = mgr.get_config(name).map(|c| {
                json!({
                    "description": c.description,
                    "enabled": c.enabled,
                    "business_domain": c.business_domain,
                    "model": c.model,
                })
            });
            json!({
                "name": name,
                "status": status,
                "window": window,
                "metrics": metrics,
                "config": cfg,
            })
        })
        .collect();
    Json(json!({ "running": mgr.is_running(), "count": agents.len(), "agents": agents }))
}

#[derive(Debug, Deserialize)]
struct BatchControlRequest {
    action: String,
}

/// POST /api/v1/batch/agents/:name/control — 启停指定批处理 Agent（action: start|stop）。
async fn control_batch_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(req): Json<BatchControlRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    let mgr_arc = match &state.batch_manager {
        Some(m) => m.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "批处理系统未启用" })),
            )
                .into_response()
        }
    };
    let mut guard = mgr_arc.lock().await;
    let mgr = match guard.as_mut() {
        Some(m) => m,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "批处理系统未初始化" })),
            )
                .into_response()
        }
    };
    let result = match req.action.as_str() {
        "start" => mgr.start(Some(&name)).await,
        "stop" => mgr.stop(Some(&name)).await,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("不支持的操作: {other}（仅支持 start|stop）") })),
            )
                .into_response()
        }
    };
    match result {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "name": name, "action": req.action, "status": mgr.get_status(&name) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{:?}", e) })),
        )
            .into_response(),
    }
}

/// Expand short namespace prefixes to absolute IRIs for Oxigraph.
/// e.g. "aps:Bench" → "http://aps.local/ontology/Bench"
///      "graph:aps/benches" → "http://aps.local/graph/benches"
///      "rdfs:subClassOf" → "http://www.w3.org/2000/01/rdf-schema#subClassOf"
pub(crate) fn expand_iri(s: &str) -> String {
    if s.contains('/') && (s.starts_with("http://") || s.starts_with("https://")) {
        return s.to_string();
    }
    if let Some(rest) = s.strip_prefix("aps:") {
        format!("http://aps.local/ontology/{}", rest)
    } else if let Some(rest) = s.strip_prefix("graph:aps/") {
        format!("http://aps.local/graph/{}", rest)
    } else if let Some(rest) = s.strip_prefix("rdfs:") {
        format!("http://www.w3.org/2000/01/rdf-schema#{}", rest)
    } else if let Some(rest) = s.strip_prefix("rdf:") {
        format!("http://www.w3.org/1999/02/22-rdf-syntax-ns#{}", rest)
    } else {
        s.to_string()
    }
}

fn expand_extraction(mut extraction: LLMExtractionOutput) -> LLMExtractionOutput {
    for node in &mut extraction.nodes {
        node.node_type = expand_iri(&node.node_type);
    }
    for edge in &mut extraction.edges {
        edge.relation = expand_iri(&edge.relation);
    }
    extraction
}

async fn kg_import_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KgImportRequest>,
) -> impl IntoResponse {
    let store = state.kg_store.clone();
    let graph_iri = expand_iri(&req.graph);

    if req.clear_before {
        let clear = format!("DELETE WHERE {{ GRAPH <{}> {{ ?s ?p ?o . }} }}", graph_iri);
        if let Err(e) = store.update(&clear) {
            tracing::warn!(graph = %graph_iri, "KG clear skipped: {}", e);
        }
    }

    let extraction = expand_extraction(LLMExtractionOutput {
        nodes: req.nodes,
        edges: req.edges,
    });
    let result = RdfMapper::map_extraction(&extraction, &graph_iri);

    let kg = match KnowledgeGraphStore::with_shared_store(store) {
        Ok(kg) => kg,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
    };

    match kg.write_quads(&result.quads, &graph_iri) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "entity_count": result.entity_count,
                "relation_count": result.relation_count,
                "quad_count": result.quads.len(),
                "graph": req.graph,
            })),
        ),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
    }
}

async fn kg_query_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KgQueryRequest>,
) -> impl IntoResponse {
    let store = state.kg_store.clone();
    let kg = match KnowledgeGraphStore::with_shared_store(store) {
        Ok(kg) => kg,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
    };

    let named_graph = req.named_graph.as_deref().map(expand_iri);
    match kg.query_sparql(&req.sparql, named_graph.as_deref()) {
        Ok(results) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "results": results,
                "count": results.len(),
            })),
        ),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))),
    }
}


/// 图片上传单文件体积上限（10MiB）。
const IMAGE_UPLOAD_MAX_BYTES: usize = 10 * 1024 * 1024;
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
async fn upload_image_handler(
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
async fn image_raw_handler(
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
struct ModelTestRequest {
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
async fn test_model_handler(
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
struct ProviderModelsRequest {
    #[serde(default)]
    provider_id: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
}

/// POST /api/v1/providers/models — 拉取 provider 的 /v1/models 型号列表（自动加载）。
/// 返回 { ok, http_status, models:[{id, owned_by}] }。绝不回显 api_key；错误仅取网络层原因。
async fn provider_models_handler(
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
struct EmbeddingActivateRequest {
    resource_id: String,
}

/// POST /api/v1/embedding/activate — 把某个 embedding 型号（resource）桥接为生效向量服务。
/// 用 resource 的 provider 端点/密钥 + resource.model/dimension 写入 embedding(oneapi) 段，
/// 热切换向量库并后台重建索引。绝不回显 api_key。
async fn activate_embedding_handler(
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


#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use tower::ServiceExt; // oneshot

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
        });

        // 构造 Router 并发起 GET /api/v1/config 请求
        let router = Router::new()
            .route("/api/v1/config", get(config_handler))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/v1/config")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = router.oneshot(req).await.unwrap();
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

        // 清理
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 端到端集成测试：电池维修助手 —— 导入(按租户隔离命名图) → 建体 → 多 Skill 注册(DA/匿名 403)
    /// → 绑定专用知识库 → 跨租户会话隔离查询 → 持久化落盘断言。
    #[tokio::test]
    async fn test_battery_assistant_e2e_tenant_isolation() {
        use crate::core::core_types::{CoreConfig, SemanticCore};
        use base64::{engine::general_purpose::STANDARD, Engine};

        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 隔离持久化目录 + 启用严格鉴权（验证匿名 403）
        let tmp = std::env::temp_dir().join(format!("agentos_e2e_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");

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
        let kg_store = Arc::new(oxigraph::store::Store::new().unwrap());
        let gateway = Arc::new(test_gateway());
        let state = Arc::new(AppState {
            core,
            gateway,
            kg_store,
            config_info: Arc::new(tokio::sync::RwLock::new(json!({}))),
            agents_info: json!({ "count": 0, "agents": [] }),
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
        });

        let router = Router::new()
            .route("/api/v1/kg/import", post(kg_import_handler))
            .route("/api/v1/kg/query", post(kg_query_handler))
            .route("/api/v1/agents", post(create_agent_handler))
            .route("/api/v1/skills", post(register_skill_handler))
            .with_state(state);

        async fn post_json(
            router: &Router,
            uri: &str,
            body: Value,
            ident: Option<&str>,
        ) -> (StatusCode, Value) {
            let mut b = axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json");
            if let Some(id) = ident {
                b = b.header("x-identity", id);
            }
            let req = b.body(axum::body::Body::from(body.to_string())).unwrap();
            let resp = router.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap();
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            (status, v)
        }

        let id_of = |user: &str, tenant: &str| -> String {
            STANDARD
                .encode(json!({"user_id": user, "tenant_id": tenant, "roles": ["DA"]}).to_string())
        };
        let id_a = id_of("svc-tesla", "t-tesla");
        let id_b = id_of("svc-byd", "t-byd");

        let fault_node = |tenant: &str, code: &str, label: &str| -> Value {
            json!({
                "id": format!("dtc:{}:{}", tenant, code),
                "node_type": "aps:FaultCode",
                "label": label,
                "properties": {"code": code}
            })
        };
        let g_tesla = "tenant:t-tesla/kb/fault-codes";
        let g_byd = "tenant:t-byd/kb/fault-codes";

        // [1] 按租户导入隔离命名图
        let (st, _) = post_json(
            &router,
            "/api/v1/kg/import",
            json!({
                "graph": g_tesla, "clear_before": true,
                "nodes": [fault_node("t-tesla", "BMS_a067", "BMS_a067 — 高压电池需要维修"),
                          fault_node("t-tesla", "BMS_a068", "BMS_a068 — 电池需要维修")],
                "edges": []
            }),
            Some(&id_a),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = post_json(
            &router,
            "/api/v1/kg/import",
            json!({
                "graph": g_byd, "clear_before": true,
                "nodes": [fault_node("t-byd", "P0A80", "P0A80 — 动力电池热管理系统故障"),
                          fault_node("t-byd", "P0A1F", "P0A1F — 电池包电压异常偏高")],
                "edges": []
            }),
            Some(&id_b),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // [2] 创建智能体
        let (st, agent) = post_json(
            &router,
            "/api/v1/agents",
            json!({
                "name": "新能源汽车电池维修助手",
                "description": "聚合多技能、绑定专用故障码知识库的工业级维修助手",
                "business_domain": "新能源汽车维修",
                "enabled": true
            }),
            Some(&id_a),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let agent_id = agent["id"].as_str().unwrap().to_string();
        assert!(!agent_id.is_empty());

        // [3] 注册多个 Skill（DA 角色）
        let skill = |iri: &str, name: &str| -> Value {
            json!({
                "skill_iri": iri, "name": name, "description": name,
                "version": "1.0.0", "category": "diagnostics", "security_level": "standard",
                "allowed_roles": ["DA"], "input_schema": {"type": "object"},
                "output_schema": {"type": "object"}, "compiled_template": "{{x}}"
            })
        };
        for (iri, name) in [
            ("skill://battery/fault-code-lookup", "故障码检索"),
            ("skill://battery/repair-order-gen", "维修工单生成"),
            ("skill://battery/severity-triage", "故障严重度分级"),
        ] {
            let (st, _) = post_json(&router, "/api/v1/skills", skill(iri, name), Some(&id_a)).await;
            assert_eq!(st, StatusCode::CREATED, "skill {} should register", iri);
        }
        // 负向：严格模式下匿名注册应 403
        let (st, _) = post_json(
            &router,
            "/api/v1/skills",
            skill("skill://battery/anon", "匿名技能"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        // [4] 跨租户会话隔离（图谱直查，租户命名图隔离）
        let list = |g: &str| {
            json!({"sparql": format!(
                "SELECT ?label WHERE {{ GRAPH <{}> {{ ?s a <http://aps.local/ontology/FaultCode> ; <http://www.w3.org/2000/01/rdf-schema#label> ?label }} }}", g)})
        };
        let find = |g: &str, code: &str| {
            json!({"sparql": format!(
                "SELECT ?label WHERE {{ GRAPH <{}> {{ ?s a <http://aps.local/ontology/FaultCode> ; <https://agentos.ontology/meta/code> \"{}\" }} }}", g, code)})
        };

        let (st, a) = post_json(&router, "/api/v1/kg/query", list(g_tesla), Some(&id_a)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(a["count"], 2);
        let (_, b) = post_json(&router, "/api/v1/kg/query", list(g_byd), Some(&id_b)).await;
        assert_eq!(b["count"], 2);

        // 隔离：租户A 图中查 BYD 专有码 → 0
        let (_, x) = post_json(
            &router,
            "/api/v1/kg/query",
            find(g_tesla, "P0A80"),
            Some(&id_a),
        )
        .await;
        assert_eq!(
            x["count"], 0,
            "cross-tenant leak: P0A80 must not appear in tesla graph"
        );
        // 对照：租户B 图中查同码 → 1
        let (_, y) = post_json(
            &router,
            "/api/v1/kg/query",
            find(g_byd, "P0A80"),
            Some(&id_b),
        )
        .await;
        assert_eq!(y["count"], 1);
        // 对照：租户A 图中查 Tesla 码 → 1
        let (_, z) = post_json(
            &router,
            "/api/v1/kg/query",
            find(g_tesla, "BMS_a067"),
            Some(&id_a),
        )
        .await;
        assert_eq!(z["count"], 1);

        // [6] 持久化落盘断言
        let agents_disk = std::fs::read_to_string(tmp.join("agents.json")).unwrap();
        assert!(agents_disk.contains("新能源汽车电池维修助手"));
        let skills_disk = std::fs::read_to_string(tmp.join("skills.json")).unwrap();
        assert!(skills_disk.contains("skill://battery/fault-code-lookup"));

        // 清理
        std::env::remove_var("AGENTOS_DATA_DIR");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let _ = std::fs::remove_dir_all(tmp);
    }
}


