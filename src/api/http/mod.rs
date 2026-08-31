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
use crate::gateway::unified_gateway::{ChatContent, ChatMessage, UnifiedGateway};
use crate::knowledge_graph::rdf_mapper::RdfMapper;
use crate::knowledge_graph::store::KnowledgeGraphStore;
use crate::knowledge_graph::types::{EdgeDef, LLMExtractionOutput, NodeDef, RdfQuad, RdfValue};
use crate::memory::hyperspace_store::{HybridSearchFilter, HyperspaceStore};
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

/// 知识库分类的持久化文件路径。
fn kb_categories_store_path() -> std::path::PathBuf {
    data_dir().join("kb_categories.json")
}

/// 启动时从磁盘加载知识库分类；文件不存在或解析失败时返回空列表。
fn load_kb_categories() -> Vec<Value> {
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
fn load_knowledge_bases() -> Vec<Value> {
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
fn load_knowledge_packs() -> Vec<Value> {
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
fn save_knowledge_packs(packs: &[Value]) -> std::io::Result<()> {
    let path = knowledge_packs_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(packs).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
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

/// 遍历所有向量 KB，逐个后台重建索引（从 BlobStore 原文台账）。返回排队重建的 KB 数。
/// 无 BlobStore 或无原文台账的 KB 将被跳过（存量向量已作废，需重新上传）。
async fn spawn_reindex_all_vector_kbs(state: Arc<AppState>) -> usize {
    if state.blob_store.is_none() {
        tracing::warn!("BlobStore 未启用，跳过自动重建（存量向量已作废，需重新上传原文）");
        return 0;
    }
    let targets: Vec<(String, String, String, Vec<Value>)> = {
        let guard = state.knowledge_bases.read().await;
        guard
            .iter()
            .filter_map(|kb| {
                if kb.get("kb_type").and_then(|v| v.as_str()) != Some("vector") {
                    return None;
                }
                let id = kb.get("id").and_then(|v| v.as_str())?.to_string();
                let namespace = kb
                    .get("vector_namespace")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if namespace.is_empty() {
                    return None;
                }
                let tenant = kb
                    .get("tenant_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default")
                    .to_string();
                let docs = kb
                    .get("documents")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if docs.is_empty() {
                    return None;
                }
                Some((id, namespace, tenant, docs))
            })
            .collect()
    };
    let count = targets.len();
    for (id, namespace, tenant, docs) in targets {
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
        let st = state.clone();
        tokio::spawn(async move {
            run_kb_reindex(st, id, namespace, tenant, docs).await;
        });
    }
    count
}


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
async fn agent_chat_handler(
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
async fn public_agent_chat_handler(
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
async fn public_agent_chat_stream_handler(
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
async fn openai_list_models_handler(
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
async fn openai_chat_completions_handler(
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
fn expand_iri(s: &str) -> String {
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

// ─── 知识库分类管理 CRUD ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct KbCategoryCreateRequest {
    pub name: String,
    pub description: Option<String>,
}

/// GET /api/v1/knowledge-packs — 返回知识包清单（内置种子 + 用户创建，均持久化于 data/knowledge_packs.json）。
///
/// 每个知识包关联 N 个知识库分类 / N 个图知识库 / N 个向量知识库，可被 Agent 挂载。
async fn list_knowledge_packs_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
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
async fn create_knowledge_pack_handler(
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
async fn update_knowledge_pack_handler(
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
async fn delete_knowledge_pack_handler(
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

/// GET /api/v1/ontology/types — 返回新能源车维修域本体定义（对象/链接/动作/函数）
///
/// 语义层（ObjectType/LinkType）+ 动力层（ActionType/FunctionDef）的完整元模型。
///
/// 数据源为 Oxigraph 元命名图（`graph:ontology/meta`）：首启由 `ensure_seeded` 幂等
/// 把硬编码 `ev_repair_ontology()` 写入图谱，之后读路径解析 `meta:json` 快照重建。
/// 存储不可用时回退硬编码定义，保证只读契约零回归。
async fn ontology_types_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
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
async fn upsert_object_type_handler(
    State(state): State<Arc<AppState>>,
    Json(obj): Json<crate::knowledge_graph::ontology_layer::ObjectType>,
) -> impl IntoResponse {
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
async fn update_object_type_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut obj): Json<crate::knowledge_graph::ontology_layer::ObjectType>,
) -> impl IntoResponse {
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
async fn delete_object_type_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
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
async fn upsert_link_type_handler(
    State(state): State<Arc<AppState>>,
    Json(link): Json<crate::knowledge_graph::ontology_layer::LinkType>,
) -> impl IntoResponse {
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
async fn update_link_type_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut link): Json<crate::knowledge_graph::ontology_layer::LinkType>,
) -> impl IntoResponse {
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
async fn delete_link_type_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
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
async fn upsert_action_type_handler(
    State(state): State<Arc<AppState>>,
    Json(action): Json<crate::knowledge_graph::ontology_layer::ActionType>,
) -> impl IntoResponse {
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
async fn update_action_type_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut action): Json<crate::knowledge_graph::ontology_layer::ActionType>,
) -> impl IntoResponse {
    action.id = id;
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
async fn delete_action_type_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
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
async fn upsert_function_def_handler(
    State(state): State<Arc<AppState>>,
    Json(func): Json<crate::knowledge_graph::ontology_layer::FunctionDef>,
) -> impl IntoResponse {
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
async fn update_function_def_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut func): Json<crate::knowledge_graph::ontology_layer::FunctionDef>,
) -> impl IntoResponse {
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
async fn delete_function_def_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
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
// 再把 side-effect 以 SPARQL 写回新能源车维修知识包的命名图（graph:pack/ev-repair）。

/// 新能源车维修知识包的命名图（写回隔离单元）。
const EV_PACK_GRAPH: &str = "graph:pack/ev-repair";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

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
fn upsert_prop_stmts(subject: &str, prop: &str, obj: &str) -> Vec<String> {
    vec![
        format!(
            "DELETE WHERE {{ GRAPH <{g}> {{ <{s}> <{p}> ?old }} }}",
            g = EV_PACK_GRAPH,
            s = subject,
            p = prop
        ),
        format!(
            "INSERT DATA {{ GRAPH <{g}> {{ <{s}> <{p}> {o} }} }}",
            g = EV_PACK_GRAPH,
            s = subject,
            p = prop,
            o = obj
        ),
    ]
}

/// 命名图内对象是否存在（前置条件检查）。
fn ev_object_exists(kg: &KnowledgeGraphStore, iri: &str) -> bool {
    let q = format!(
        "SELECT ?o WHERE {{ GRAPH <{g}> {{ <{iri}> ?p ?o }} }} LIMIT 1",
        g = EV_PACK_GRAPH,
        iri = iri,
    );
    kg.query_sparql(&q, None)
        .map(|r| !r.is_empty())
        .unwrap_or(false)
}

/// 对象存在性前置条件解析（知识/业务分流，MCP 向后兼容扩展位）。
///
/// - 知识对象（FaultCode / VehicleModel / FAQ…）：查询知识命名图。
/// - 业务对象（Vehicle / Battery / RepairOrder…）：业务数据不入图谱，未来经 MCP
///   对接业务库查询；当前 MCP 未接入，回退查询命名图以保持向后兼容——接入 MCP 后
///   只需替换 Business 分支，调用方（build_action_effects）无需改动。
fn resolve_object_exists(kg: &KnowledgeGraphStore, object_type: &str, key: &str) -> bool {
    use crate::knowledge_graph::ontology_layer::{object_kind_of, ObjectKind};
    let iri = ev_instance_iri(object_type, key);
    match object_kind_of(object_type) {
        ObjectKind::Knowledge => ev_object_exists(kg, &iri),
        // TODO(MCP): 业务库接入后改为经 MCP 查询业务对象是否存在；当前回退命名图。
        ObjectKind::Business => ev_object_exists(kg, &iri),
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
pub struct ActionInvokeRequest {
    /// applies_to 对象实例的主键值（动作作用的目标对象）。
    #[serde(default)]
    pub target: Option<String>,
    /// 动作参数（name → value）。
    #[serde(default)]
    pub params: serde_json::Map<String, Value>,
    /// 仅校验并返回将执行的 SPARQL，不真正写回。
    #[serde(default)]
    pub dry_run: bool,
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
//   1. 为本次 invoke 生成 per-invocation 影子图 IRI（graph:pack/ev-repair/staging/<uuid>）
//   2. 把 side-effect 语句里的生产图 IRI 重定向到影子图，写入影子图（生产图零改动）
//   3. 对影子图跑 ASK 护栏（三元组数上限 / 谓词命名空间白名单），任一命中即视为违规
//   4. 通过 → ADD 影子图到生产图 + DROP 影子图（提交）；违规 → DROP 影子图（回滚）

/// 影子图三元组数上限（防单次写回爆量）。
const SANDBOX_MAX_TRIPLES: usize = 5000;
/// 允许写回的谓词命名空间前缀白名单（防越权写入无关命名空间）。
const SANDBOX_ALLOWED_PRED_PREFIXES: &[&str] = &[
    "https://agentos.ontology/ev/",
    "http://www.w3.org/2000/01/rdf-schema#",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
];

/// 生成本次 invoke 的影子图 IRI。
fn staging_graph_iri() -> String {
    format!(
        "{}/staging/{}",
        EV_PACK_GRAPH,
        uuid::Uuid::new_v4().simple()
    )
}

/// 把 side-effect 语句中出现的生产图 IRI 重定向到影子图。
/// 语句均由 build_action_effects 以 `GRAPH <EV_PACK_GRAPH>` 形式硬编码构造，做定向替换即可。
fn redirect_to_staging(stmt: &str, staging: &str) -> String {
    stmt.replace(
        &format!("GRAPH <{}>", EV_PACK_GRAPH),
        &format!("GRAPH <{}>", staging),
    )
}

/// 护栏后校验：对影子图跑 ASK，返回违规项列表（空=通过）。
fn sandbox_guardrail_violations(kg: &KnowledgeGraphStore, staging: &str) -> Vec<String> {
    let mut violations = Vec::new();

    // 护栏1：三元组数上限。
    let count_q = format!(
        "SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}",
        g = staging
    );
    let n = kg
        .query_sparql(&count_q, None)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get("?c").and_then(|v| v.as_str().map(String::from)))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    if n > SANDBOX_MAX_TRIPLES {
        violations.push(format!(
            "写回三元组数 {} 超过上限 {}",
            n, SANDBOX_MAX_TRIPLES
        ));
    }

    // 护栏2：谓词命名空间白名单——存在任一不在白名单前缀内的谓词即违规。
    let filters: Vec<String> = SANDBOX_ALLOWED_PRED_PREFIXES
        .iter()
        .map(|p| format!("STRSTARTS(STR(?p), \"{}\")", p))
        .collect();
    let ask_q = format!(
        "ASK {{ GRAPH <{g}> {{ ?s ?p ?o . FILTER(!({allow})) }} }}",
        g = staging,
        allow = filters.join(" || ")
    );
    let has_foreign = kg
        .query_sparql(&ask_q, None)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get("result").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    if has_foreign {
        violations.push("存在越权谓词（不在允许的命名空间白名单内）".to_string());
    }

    violations
}

/// 经影子图提交一批写回语句：写影子图 → 护栏 → 提交/回滚。
/// 返回 Ok(护栏报告 JSON) 表示已提交；Err((状态码, 消息, 违规列表)) 表示回滚。
fn commit_via_staging(
    kg: &KnowledgeGraphStore,
    statements: &[String],
) -> Result<Value, (StatusCode, String, Vec<String>)> {
    let staging = staging_graph_iri();

    // 1. 写入影子图（生产图零改动）。任一失败即清理并报错。
    for stmt in statements {
        let staged = redirect_to_staging(stmt, &staging);
        if let Err(e) = kg.store_arc().update(&staged) {
            let _ = kg
                .store_arc()
                .update(&format!("DROP SILENT GRAPH <{}>", staging));
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("影子图写入失败: {e}"),
                vec![],
            ));
        }
    }

    // 2. 护栏后校验。违规即回滚（DROP 影子图），生产图不受影响。
    let violations = sandbox_guardrail_violations(kg, &staging);
    if !violations.is_empty() {
        let _ = kg
            .store_arc()
            .update(&format!("DROP SILENT GRAPH <{}>", staging));
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "护栏校验未通过，已回滚（生产图未改动）".to_string(),
            violations,
        ));
    }

    // 3. 提交：合并影子图到生产图，再删除影子图。
    let merge = format!(
        "ADD SILENT GRAPH <{s}> TO <{p}>",
        s = staging,
        p = EV_PACK_GRAPH
    );
    if let Err(e) = kg.store_arc().update(&merge) {
        let _ = kg
            .store_arc()
            .update(&format!("DROP SILENT GRAPH <{}>", staging));
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("影子图合并到生产图失败: {e}"),
            vec![],
        ));
    }
    let _ = kg
        .store_arc()
        .update(&format!("DROP SILENT GRAPH <{}>", staging));

    Ok(json!({
        "sandbox": "staging_graph",
        "staging_graph": staging,
        "guardrails_passed": true,
    }))
}

async fn invoke_action_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(action_id): axum::extract::Path<String>,
    Json(req): Json<ActionInvokeRequest>,
) -> impl IntoResponse {
    use crate::knowledge_graph::ontology_store::OntologyStore;
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
    let now = chrono::Utc::now().to_rfc3339();
    let (statements, result_meta) = match build_action_effects(&action_id, &req, &kg, &now) {
        Ok(v) => v,
        Err((code, msg)) => return (code, Json(json!({ "error": msg }))),
    };

    if req.dry_run {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "dry_run",
                "action": action_id,
                "graph": EV_PACK_GRAPH,
                "sparql": statements,
                "result": result_meta,
            })),
        );
    }

    // 4. 数据沙箱写回：先写影子图 → 护栏后校验 → 通过才合并到生产图，失败即回滚。
    let sandbox = match commit_via_staging(&kg, &statements) {
        Ok(report) => report,
        Err((code, msg, violations)) => {
            return (
                code,
                Json(json!({ "error": msg, "violations": violations })),
            )
        }
    };
    let _ = state.kg_store.flush();

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "action": action_id,
            "graph": EV_PACK_GRAPH,
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
    now: &str,
) -> Result<(Vec<String>, Value), (StatusCode, String)> {
    let g = EV_PACK_GRAPH;
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
            if !resolve_object_exists(kg, "Vehicle", &vin) {
                return Err(bad(format!("前置条件不满足：车辆VIN不存在于图谱 ({vin})")));
            }
            let fault_iri = ev_instance_iri("FaultCode", &fault_code);
            if !resolve_object_exists(kg, "FaultCode", &fault_code) {
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
            let stmt = format!(
                "INSERT DATA {{ GRAPH <{g}> {{ {t} . }} }}",
                g = g,
                t = triples.join(" .\n")
            );
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
            if !resolve_object_exists(kg, "Battery", &battery_id) {
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
            if !resolve_object_exists(kg, "VehicleModel", &model_id) {
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
            if !resolve_object_exists(kg, "FaultCode", &code) {
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
            let stmt = format!(
                "INSERT DATA {{ GRAPH <{g}> {{ {t} . }} }}",
                g = g,
                t = triples.join(" .\n")
            );
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

/// GET /api/v1/kb/categories — 返回全部知识库分类
async fn list_kb_categories_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let categories = state.kb_categories.read().await.clone();
    Json(json!({ "count": categories.len(), "categories": categories }))
}

/// POST /api/v1/kb/categories — 创建知识库分类并持久化
async fn create_kb_category_handler(
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
async fn update_kb_category_handler(
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
async fn delete_kb_category_handler(
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

/// SPARQL 字面量转义。
fn sparql_literal(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// GET /api/v1/kb/bases — 返回全部知识库
async fn list_knowledge_bases_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let bases = state.knowledge_bases.read().await.clone();
    Json(json!({ "count": bases.len(), "bases": bases }))
}

/// POST /api/v1/kb/bases — 创建知识库（向量/图），图类型在 oxigraph 落盘命名图元数据
async fn create_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<KnowledgeBaseCreateRequest>,
) -> impl IntoResponse {
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
    // 图类型：租户隔离命名图 + 落盘元数据三元组（Wild AgentOS 底座）
    let graph_iri = if req.kb_type == "graph" {
        let iri = format!("tenant:{}/kb/{}", identity.tenant_id, kb_id);
        let insert = format!(
            "INSERT DATA {{ GRAPH <{g}> {{ <{g}> <https://agentos.ontology/meta/kbName> \"{n}\" . <{g}> <https://agentos.ontology/meta/kbType> \"graph\" }} }}",
            g = iri,
            n = sparql_literal(&req.name),
        );
        if let Err(e) = state.kg_store.update(&insert) {
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
        format!("vec:tenant/{}/kb/{}", identity.tenant_id, kb_id)
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
        "tenant_id": identity.tenant_id,
        "created_by": identity.user_id,
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

/// DELETE /api/v1/kb/bases/:id — 删除知识库并持久化（图类型同时清空命名图）
async fn delete_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let mut guard = state.knowledge_bases.write().await;
    let removed = guard
        .iter()
        .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        .cloned();
    let before = guard.len();
    guard.retain(|b| b.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    if guard.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "knowledge base not found", "id": id })),
        );
    }
    let _ = save_knowledge_bases(&guard);
    // 图类型：清空命名图三元组
    if let Some(b) = removed {
        if let Some(g) = b
            .get("graph")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            let clear = format!("DELETE WHERE {{ GRAPH <{g}> {{ ?s ?p ?o . }} }}");
            if let Err(e) = state.kg_store.update(&clear) {
                tracing::warn!(graph = %g, "KB graph clear skipped: {}", e);
            }
            let _ = state.kg_store.flush();
        }
    }
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
async fn update_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<KnowledgeBaseUpdateRequest>,
) -> impl IntoResponse {
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
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
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
            let sparql = format!(
                "DELETE {{ GRAPH <{g}> {{ <{g}> <https://agentos.ontology/meta/kbName> ?o }} }} \
                 INSERT {{ GRAPH <{g}> {{ <{g}> <https://agentos.ontology/meta/kbName> \"{n}\" }} }} \
                 WHERE {{ OPTIONAL {{ GRAPH <{g}> {{ <{g}> <https://agentos.ontology/meta/kbName> ?o }} }} }}",
                g = graph_iri,
                n = sparql_literal(&new_name),
            );
            if let Err(e) = state.kg_store.update(&sparql) {
                tracing::warn!(graph = %graph_iri, "KB rename meta sync skipped: {}", e);
            } else {
                let _ = state.kg_store.flush();
            }
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
async fn knowledge_base_stats_handler(
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
        let graph_iri = kb
            .get("graph")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let triples = if graph_iri.is_empty() {
            json!(0)
        } else {
            match KnowledgeGraphStore::with_shared_store(state.kg_store.clone()) {
                Ok(kg) => {
                    let q = format!(
                        "SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}",
                        g = graph_iri
                    );
                    match kg.query_sparql(&q, None) {
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
async fn ingest_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<IngestRequest>,
) -> impl IntoResponse {
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
    let namespace = kb
        .get("vector_namespace")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if namespace.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "该向量库缺少 vector_namespace" })),
        );
    }
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
    let tags = vec![namespace.clone(), format!("tenant:{}", identity.tenant_id)];
    let mut count = 0usize;
    for text in &texts {
        for chunk in chunk_text(text, 500) {
            let iri = format!("{}#chunk/{}", namespace, uuid::Uuid::new_v4().hyphenated());
            match store
                .upsert_with_metadata(
                    &iri,
                    &chunk,
                    &tags,
                    Some(0.5),
                    None,
                    Some(namespace.as_str()),
                )
                .await
            {
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
async fn search_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<SearchRequest>,
) -> impl IntoResponse {
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
    let namespace = kb
        .get("vector_namespace")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if namespace.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "该向量库缺少 vector_namespace" })),
        );
    }
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
    let filter = HybridSearchFilter::new().with_named_graph(namespace.clone());
    match store.search_with_filter(&query, &filter, limit).await {
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
const KB_UPLOAD_MAX_BYTES: usize = 60 * 1024 * 1024;

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
async fn upload_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
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
    let namespace = kb
        .get("vector_namespace")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if namespace.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "该向量库缺少 vector_namespace" })),
        );
    }
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

    let base_tags = vec![namespace.clone(), format!("tenant:{}", identity.tenant_id)];
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
        let blob_key = format!("tenant:{}/kb/{}/{}", identity.tenant_id, id, doc_id);
        let mut blob_ref = Value::Null;
        let mut persist_err: Option<String> = None;
        if let Some(b) = &blob {
            match b.put(&blob_key, &bytes, &content_type).await {
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
                let iri = format!("{}#chunk/{}", namespace, uuid::Uuid::new_v4().hyphenated());
                match store
                    .upsert_with_metadata(
                        &iri,
                        &chunk,
                        &doc_tags,
                        Some(min_importance),
                        None,
                        Some(namespace.as_str()),
                    )
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
async fn list_kb_documents_handler(
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
async fn kb_document_raw_handler(
    State(state): State<Arc<AppState>>,
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
    let key = doc
        .get("blob_ref")
        .and_then(|b| b.get("key"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let key = match key {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "该文档原文未持久化（BlobStore 未启用时上传）" })),
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
    match blob.get(&key).await {
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

/// POST /api/v1/kb/bases/:id/reindex — 按当前 embedding/分块重建向量索引（异步）。
/// 从 documents 台账拉原文 → 删旧 chunk → 重新分块 embedding 写新 → 更新台账与状态。
async fn reindex_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
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
    let namespace = kb
        .get("vector_namespace")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if namespace.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "该向量库缺少 vector_namespace" })),
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
    let tenant = kb
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
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
        run_kb_reindex(state2, id2, namespace, tenant, docs).await;
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
    id: String,
    namespace: String,
    tenant: String,
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
        let min_importance = doc
            .get("min_importance")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5) as f32;
        let key = doc
            .get("blob_ref")
            .and_then(|b| b.get("key"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        // ① 删旧 chunk（幂等，忽略单条失败）。
        if let Some(arr) = doc.get("chunk_iris").and_then(|v| v.as_array()) {
            for it in arr {
                if let Some(iri) = it.as_str() {
                    let _ = store.delete(iri).await;
                }
            }
        }
        // ② 无原文或非可解析类型：无法重建，保留留底状态。
        if key.is_none() || kb_text_ext(&filename).is_none() {
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
        let key = key.unwrap();
        let bytes = match blob.get(&key).await {
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
        let tags = vec![
            namespace.clone(),
            format!("tenant:{}", tenant),
            format!("doc:{}", doc_id),
        ];
        let mut new_iris: Vec<String> = Vec::new();
        let mut err: Option<String> = None;
        for chunk in chunk_text(&text, chunk_size) {
            let iri = format!("{}#chunk/{}", namespace, uuid::Uuid::new_v4().hyphenated());
            match store
                .upsert_with_metadata(
                    &iri,
                    &chunk,
                    &tags,
                    Some(min_importance),
                    None,
                    Some(namespace.as_str()),
                )
                .await
            {
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
async fn import_graph_knowledge_base_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
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
    let graph_iri = kb
        .get("graph")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if graph_iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "该图谱库缺少命名图 graph" })),
        );
    }

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
    match kg.write_quads(&quads, &graph_iri) {
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


// ──────────────────────────────────────────────────────────────────────────────
// ontology CRUD 集成测试（原与 skill_manifest_tests 混放，随 skills 拆分迁出后独立）
// ──────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod ontology_crud_tests {
    use super::*;
    use crate::core::core_types::{CoreConfig, SemanticCore};
    use axum::http::StatusCode;
    use tower::ServiceExt;

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
    async fn test_ontology_object_link_crud() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("agentos_ontcrud_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let state = make_state(&tmp);
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
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };
        let del = |uri: &str| {
            axum::http::Request::builder()
                .method("DELETE")
                .uri(uri.to_string())
                .body(axum::body::Body::empty())
                .unwrap()
        };

        // 1) 新建对象 Widget
        let obj = json!({
            "id": "Widget", "iri": "https://agentos.ontology/ev/Widget",
            "label": "小部件", "description": "测试", "icon": "Box", "color": "blue",
            "primary_key": "name", "title_property": "name", "properties": []
        });
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
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };
        let del = |uri: &str| {
            axum::http::Request::builder()
                .method("DELETE")
                .uri(uri.to_string())
                .body(axum::body::Body::empty())
                .unwrap()
        };

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
}


/// 动力层执行器（ActionType invoke）单测：参数/前置条件校验 + SPARQL 组装。
#[cfg(test)]
mod ontology_action_tests {
    use super::*;
    use oxigraph::store::Store;

    /// 预置车辆/故障码/电池/车型实例于 graph:pack/ev-repair。
    fn seeded_kg() -> KnowledgeGraphStore {
        let store = Arc::new(Store::new().unwrap());
        let seed = format!(
            "INSERT DATA {{ GRAPH <{g}> {{ \
             <{veh}> a <{vehc}> . \
             <{fault}> a <{faultc}> . \
             <{bat}> a <{batc}> . \
             <{model}> a <{modelc}> . \
             }} }}",
            g = EV_PACK_GRAPH,
            veh = ev_instance_iri("Vehicle", "LVIN123"),
            vehc = ev_term_iri("Vehicle"),
            fault = ev_instance_iri("FaultCode", "P0A80"),
            faultc = ev_term_iri("FaultCode"),
            bat = ev_instance_iri("Battery", "BAT-001"),
            batc = ev_term_iri("Battery"),
            model = ev_instance_iri("VehicleModel", "M-001"),
            modelc = ev_term_iri("VehicleModel"),
        );
        store.update(&seed).unwrap();
        KnowledgeGraphStore::with_shared_store(store).unwrap()
    }

    fn mk_req(target: Option<&str>, params: Value, dry_run: bool) -> ActionInvokeRequest {
        ActionInvokeRequest {
            target: target.map(|s| s.to_string()),
            params: params.as_object().cloned().unwrap_or_default(),
            dry_run,
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
        let kg = seeded_kg();
        let r = mk_req(
            Some("P0A80"),
            json!({"vehicle_vin": "LVIN123", "assigned_to": "张工", "estimated_cost": 1200}),
            false,
        );
        let (stmts, meta) =
            build_action_effects("GenerateRepairOrder", &r, &kg, "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(stmts.len(), 1);
        let s = &stmts[0];
        assert!(s.contains("RepairOrder"));
        assert!(s.contains("forVehicle"));
        assert!(s.contains("diagnoses"));
        assert!(s.contains("张工"));
        assert!(s.contains("1200"));
        assert!(meta["order_id"].as_str().unwrap().starts_with("RO-"));
    }

    #[test]
    fn test_generate_repair_order_missing_vehicle_precondition() {
        let kg = seeded_kg();
        let r = mk_req(Some("P0A80"), json!({"vehicle_vin": "UNKNOWN"}), false);
        let err = build_action_effects("GenerateRepairOrder", &r, &kg, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("车辆VIN不存在"));
    }

    #[test]
    fn test_generate_repair_order_missing_target() {
        let kg = seeded_kg();
        let r = mk_req(None, json!({"vehicle_vin": "LVIN123"}), false);
        let err = build_action_effects("GenerateRepairOrder", &r, &kg, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    // ── 数据沙箱（staging-graph）单测 ──────────────────────────────────

    #[test]
    fn test_redirect_to_staging_rewrites_graph_iri() {
        let stmt = format!(
            "INSERT DATA {{ GRAPH <{}> {{ <a> <b> <c> }} }}",
            EV_PACK_GRAPH
        );
        let staging = "graph:pack/ev-repair/staging/abc";
        let out = redirect_to_staging(&stmt, staging);
        assert!(out.contains(&format!("GRAPH <{}>", staging)));
        assert!(!out.contains(&format!("GRAPH <{}> {{", EV_PACK_GRAPH)));
    }

    /// 合法写回：经影子图护栏通过 → 合并到生产图；影子图删除、生产图可见新数据。
    #[test]
    fn test_sandbox_commit_merges_to_production() {
        let kg = seeded_kg();
        let r = mk_req(Some("P0A80"), json!({"vehicle_vin": "LVIN123"}), false);
        let (stmts, _meta) =
            build_action_effects("GenerateRepairOrder", &r, &kg, "2026-01-01T00:00:00Z").unwrap();

        let report = commit_via_staging(&kg, &stmts).expect("护栏应通过并提交");
        assert_eq!(report["guardrails_passed"], json!(true));

        // 生产图应能查到新建的维修工单类型三元组。
        let q = format!(
            "SELECT ?o WHERE {{ GRAPH <{g}> {{ ?o a <{c}> }} }}",
            g = EV_PACK_GRAPH,
            c = ev_term_iri("RepairOrder")
        );
        let rows = kg.query_sparql(&q, None).unwrap();
        assert!(!rows.is_empty(), "生产图应可见已提交的维修工单");

        // 影子图应已删除（无残留）。
        let staging = report["staging_graph"].as_str().unwrap();
        let sq = format!("SELECT ?s WHERE {{ GRAPH <{}> {{ ?s ?p ?o }} }}", staging);
        assert!(
            kg.query_sparql(&sq, None).unwrap().is_empty(),
            "影子图应已清理"
        );
    }

    /// 越权谓词：护栏应拦截并回滚（返回 422），生产图零改动。
    #[test]
    fn test_sandbox_rollback_on_foreign_predicate() {
        let kg = seeded_kg();
        // 统计生产图当前三元组数（回滚后应不变）。
        let count_q = format!(
            "SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{}> {{ ?s ?p ?o }} }}",
            EV_PACK_GRAPH
        );
        let before = kg.query_sparql(&count_q, None).unwrap()[0]["?c"]
            .as_str()
            .unwrap()
            .to_string();

        // 构造带越权谓词（不在白名单命名空间）的语句。
        let foreign = format!(
            "INSERT DATA {{ GRAPH <{g}> {{ <https://agentos.ontology/ev/X/1> <http://evil.example/pwn> \"x\" }} }}",
            g = EV_PACK_GRAPH
        );
        let err = commit_via_staging(&kg, &[foreign]).unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err.2.iter().any(|v| v.contains("越权谓词")));

        // 生产图三元组数不变（已回滚）。
        let after = kg.query_sparql(&count_q, None).unwrap()[0]["?c"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(before, after, "回滚后生产图不应有任何改动");
    }

    #[test]
    fn test_update_battery_soh_ok_and_range() {
        let kg = seeded_kg();
        let ok = mk_req(None, json!({"battery_id": "BAT-001", "soh": 87.5}), false);
        let (stmts, meta) = build_action_effects("UpdateBatterySoh", &ok, &kg, "t").unwrap();
        assert_eq!(stmts.len(), 4); // soh upsert(2) + soh_updated_at upsert(2)
        assert!(stmts.iter().any(|s| s.contains("DELETE WHERE")));
        assert!(stmts.iter().any(|s| s.contains("87.5")));
        assert_eq!(meta["soh"], 87.5);

        let bad = mk_req(None, json!({"battery_id": "BAT-001", "soh": 150}), false);
        let err = build_action_effects("UpdateBatterySoh", &bad, &kg, "t").unwrap_err();
        assert!(err.1.contains("0-100"));
    }

    #[test]
    fn test_update_battery_soh_missing_battery() {
        let kg = seeded_kg();
        let r = mk_req(None, json!({"battery_id": "NOPE", "soh": 50}), false);
        let err = build_action_effects("UpdateBatterySoh", &r, &kg, "t").unwrap_err();
        assert!(err.1.contains("电池对象不存在"));
    }

    #[test]
    fn test_mark_recall_ok() {
        let kg = seeded_kg();
        let r = mk_req(
            None,
            json!({"model_id": "M-001", "recall_reason": "电池批次缺陷"}),
            false,
        );
        let (stmts, meta) = build_action_effects("MarkRecall", &r, &kg, "t").unwrap();
        assert_eq!(stmts.len(), 6); // 三个属性各 upsert(2)
        assert!(stmts.iter().any(|s| s.contains("recalled")));
        assert!(stmts.iter().any(|s| s.contains("电池批次缺陷")));
        assert_eq!(meta["recalled"], true);
    }

    #[test]
    fn test_append_faq_ok_and_links_fault() {
        let kg = seeded_kg();
        let r = mk_req(
            Some("P0A80"),
            json!({"question": "报警怎么办？", "answer": "请尽快检修"}),
            false,
        );
        let (stmts, meta) = build_action_effects("AppendFaq", &r, &kg, "t").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("relatedFaq"));
        assert!(stmts[0].contains("报警怎么办"));
        assert!(meta["faq_id"].as_str().unwrap().starts_with("FAQ-"));
    }

    #[test]
    fn test_append_faq_missing_fault_precondition() {
        let kg = seeded_kg();
        let r = mk_req(
            Some("NON_EXIST"),
            json!({"question": "q", "answer": "a"}),
            false,
        );
        let err = build_action_effects("AppendFaq", &r, &kg, "t").unwrap_err();
        assert!(err.1.contains("故障码对象不存在"));
    }

    #[test]
    fn test_unknown_action() {
        let kg = seeded_kg();
        let r = mk_req(None, json!({}), false);
        let err = build_action_effects("NoSuchAction", &r, &kg, "t").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
/// §9 知识库图谱摄取回归单测：固化两处已修复缺陷——
///   1) 中文 IRI 保留（kb_sanitize_id 不再把非 ASCII 折叠成 `_`，避免碰撞/损坏）；
///   2) 图谱库 stats 三元组计数（Oxigraph 绑定键带 `?` 前缀，须用 `?c` 而非 `c`）。
#[cfg(test)]
mod kb_ingest_tests {
    use super::*;

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
        let graph = "iri://kb/test-cn-stats";
        let csv = "subject,predicate,object,object_type\n\
                   车型:测试001,属于品牌,品牌:比亚迪,iri\n\
                   车型:测试001,续航里程,605,literal\n";
        let quads = kb_quads_from_csv(csv).expect("csv parse");
        kg.write_quads(&quads, graph).expect("write quads");

        // 与 knowledge_base_stats_handler 完全一致的计数查询。
        let q = format!(
            "SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}",
            g = graph
        );
        let rows = kg.query_sparql(&q, None).expect("count query");
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
        let (a2, p2) = crate::api::http::agents::migrate_legacy_agent_graphs(&mut agents, &mut packs);
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
