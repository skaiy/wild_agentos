use std::sync::Arc;

use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
    Router,
};
use serde_json::Value;

use crate::batch::manager::BatchAgentManager;
use crate::blob::BlobStore;
use crate::core::core_types::SemanticCore;
use crate::gateway::unified_gateway::UnifiedGateway;
use crate::memory::hyperspace_store::HyperspaceStore;
use crate::tools::prompt_registry::PromptRegistry;

/// Shared handle to the platform Batch Agent manager (Option<..> allows tests to omit it,
/// inner Option matches the gRPC server's take-on-shutdown lifecycle).
pub type SharedBatchManager = Arc<tokio::sync::Mutex<Option<BatchAgentManager>>>;

/// 可原子热替换的向量库句柄：HTTP 路由 / gRPC / 任务执行器 / SA 工具链共享同一容器。
/// 内层 `None` 表示向量检索禁用（embedding 初始化失败或尚未配置）。
/// embedding 配置变更时通过 `ArcSwapOption::store` 原子换入新维度的库，无需重启进程。
pub type SharedVectorStore = Arc<arc_swap::ArcSwapOption<HyperspaceStore>>;

pub mod iam;
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
pub mod config;
pub mod core_ops;
pub mod models;

use agents::{
    create_agent_handler, delete_agent_handler, list_agents_handler, load_user_agents,
    migrate_legacy_agent_graphs, save_user_agents, update_agent_handler,
};
use tasks::{
    create_task_handler, get_execution_details_handler, get_realtime_status_handler,
    get_task_handler, list_task_trends_handler, stream_task_handler,
};
use runtime::{
    health_handler, metrics_handler, unified_stats_handler,
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
    save_knowledge_packs, search_knowledge_base_handler, update_kb_category_handler,
    update_knowledge_base_handler, update_knowledge_pack_handler, upload_knowledge_base_handler,
    KB_UPLOAD_MAX_BYTES,
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
use config::{config_handler, hot_reload_models, update_config_handler};
use core_ops::{
    control_batch_agent_handler, emit_event_handler, get_projection_handler, kg_import_handler,
    kg_query_handler, list_batch_agents_handler, list_blackboard_nodes_handler,
    list_blackboard_tasks_handler, read_node_handler, stream_batch_events_handler,
    write_node_handler,
};
pub(crate) use core_ops::expand_iri;
use models::{
    activate_embedding_handler, image_raw_handler, provider_models_handler, test_model_handler,
    upload_image_handler, IMAGE_UPLOAD_MAX_BYTES,
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


#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::Router;
    use serde_json::json;
    use tower::ServiceExt; // oneshot

    use crate::gateway::unified_gateway::UnifiedGateway;
    use crate::tools::prompt_registry::PromptRegistry;

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


