use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::api::http::SharedVectorStore;
use crate::batch::manager::BatchAgentManager;
use crate::config::settings::Settings;
use crate::core::agent_runner::AgentRunner;
use crate::core::checkpoint::CheckpointManager;
use crate::core::event_bus::EventBus;
use crate::core::execution_event::ExecutionEventEmitter;
use crate::core::execution_event::ExecutionEventKind;
use crate::core::execution_event::ExecutionState;
use crate::core::sa::SupervisorAgent;
use crate::gateway::unified_gateway::UnifiedGateway;
use crate::memory::consistency_engine::ConsistencyEngine;
use crate::memory::l0_store::L0Store;
use crate::memory::l2_blackboard::Blackboard;
use crate::memory::l3_projection::ProjectionEngine;
use crate::memory::memory_bus::MemoryBus;
use crate::memory::memory_manager::MemoryManager;
use crate::memory::prefetch_engine::PrefetchEngine;
use crate::memory::scheduler::MemoryScheduler;
use crate::memory::unified_graph::UnifiedGraphStore;
use crate::skill_graph::graph_store::SkillGraphStore;
use crate::templates::template_engine::TemplateEngine;
use crate::tools::skill_registry::SkillRegistry;
use crate::tools::workspace_monitor::{WorkspaceMonitor, WorkspaceMonitorConfig};
use crate::CoreConfig;

pub mod seapp {
    tonic::include_proto!("seapp");
}

use seapp::*;

static TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct AgentOSService {
    settings: Settings,
    gateway: Arc<UnifiedGateway>,
    l0: Arc<L0Store>,
    blackboard: Arc<Blackboard>,
    projection: Arc<ProjectionEngine>,
    memory_manager: Arc<tokio::sync::Mutex<MemoryManager>>,
    skills: Arc<SkillRegistry>,
    templates: Arc<TemplateEngine>,
    event_bus: Arc<EventBus>,
    checkpoints: Arc<CheckpointManager>,
    scheduler: Arc<MemoryScheduler>,
    prefetch: Arc<PrefetchEngine>,
    unified_graph: Arc<UnifiedGraphStore>,
    /// 向量知识库（HyperspaceStore）：由 HTTP 路由、任务执行器与 SA 工具链共享的同一实例。
    vector_store: SharedVectorStore,
    execution_states: Arc<RwLock<HashMap<String, ExecutionState>>>,
    /// Batch Agent manager, post-new async initialization.
    /// Arc 包裹以便与 HTTP 路由共享同一实例（方案A 平台运维台）。
    batch_manager: Arc<tokio::sync::Mutex<Option<BatchAgentManager>>>,
    /// Skill graph store for background maintenance (archive + re-index)
    skill_graph: Option<Arc<SkillGraphStore>>,
}

impl AgentOSService {
    pub fn new(settings: Settings) -> Result<Self, String> {
        let gateway = Arc::new(
            UnifiedGateway::new(&settings.gateway)
                .map_err(|e| format!("Gateway init failed: {}", e))?,
        );

        let l0 = Arc::new(
            L0Store::new(&settings.memory.l0.path).map_err(|e| format!("L0 init failed: {}", e))?,
        );

        // KG 持久化目录：遵循 AGENTOS_DATA_DIR 约定（缺省 "data"），底层为 RocksDB。
        let kg_dir = std::env::var("AGENTOS_DATA_DIR").unwrap_or_else(|_| "data".to_string());
        let kg_path = std::path::Path::new(&kg_dir).join("kg");
        let unified_graph = Arc::new(
            UnifiedGraphStore::new_persistent(&kg_path)
                .map_err(|e| format!("UnifiedGraph init failed: {}", e))?,
        );

        let blackboard = Arc::new(
            Blackboard::with_store(unified_graph.store())
                .map_err(|e| format!("L2 init failed: {}", e))?,
        );
        let projection = Arc::new(ProjectionEngine::new(
            blackboard.clone(),
            settings.memory.l3.max_size,
        ));
        let skills = Arc::new(SkillRegistry::new());
        let templates_path = settings
            .agents
            .template_path
            .as_deref()
            .unwrap_or("src/templates/templates");
        let templates = Arc::new(
            TemplateEngine::new(std::path::Path::new(templates_path)).map_err(|e| {
                format!(
                    "Template engine init failed (path={}): {}",
                    templates_path, e
                )
            })?,
        );
        let event_bus = Arc::new(EventBus::new(settings.agents.event_bus_capacity));

        let memory_bus = Arc::new(MemoryBus::new(event_bus.clone()));
        let consistency = Arc::new(ConsistencyEngine::new(
            memory_bus.clone(),
            l0.clone(),
            blackboard.clone(),
            projection.clone(),
        ));
        let scheduler = Arc::new(MemoryScheduler::new(
            l0.clone(),
            blackboard.clone(),
            projection.clone(),
            consistency.clone(),
            memory_bus.clone(),
        ));
        let prefetch = Arc::new(PrefetchEngine::new(
            memory_bus.clone(),
            blackboard.clone(),
            projection.clone(),
        ));

        let memory_manager = Arc::new(tokio::sync::Mutex::new(MemoryManager::with_scheduler(
            l0.clone(),
            blackboard.clone(),
            projection.clone(),
            CoreConfig::default(),
            scheduler.clone(),
        )));

        let checkpoints = Arc::new(CheckpointManager::new());

        let eb_checkpoint = event_bus.clone();
        let cp_clone = checkpoints.clone();
        eb_checkpoint.spawn_consumer(
            vec!["CYCLE_STARTED".to_string(), "CYCLE_COMPLETED".to_string()],
            move |event| {
                let cp = cp_clone.clone();
                async move {
                    match event.event_type.as_str() {
                        "CYCLE_STARTED" => {
                            let id = cp.create(&event.task_iri, &format!("cycle:{}", event.task_iri), "{}", "{}", "{}", &[]);
                            tracing::debug!(checkpoint_id = ?id, "Checkpoint created for cycle start");
                        }
                        "CYCLE_COMPLETED" => {
                            let _ = cp.restore(&event.task_iri);
                            tracing::debug!("Checkpoint restored for cycle completion");
                        }
                        _ => {}
                    }
                }
            },
        );

        let eb_5w2h = event_bus.clone();
        eb_5w2h.spawn_consumer(
            vec![
                "DEADLINE_APPROACHING".to_string(),
                "BUDGET_EXCEEDED".to_string(),
            ],
            move |event| {
                let et = event.event_type.clone();
                async move {
                    tracing::warn!(
                        event_type = %et,
                        task_iri = %event.task_iri,
                        "5W2H constraint alert consumed: needs attention"
                    );
                }
            },
        );

        let eb_invalidate = event_bus.clone();
        let l0_inv = l0.clone();
        let bb_inv = blackboard.clone();
        eb_invalidate.spawn_consumer(
            vec![
                "MEMORY_INVALIDATE".to_string(),
                "CACHE_INVALIDATE".to_string(),
            ],
            move |event| {
                let bb = bb_inv.clone();
                let l0 = l0_inv.clone();
                async move {
                    tracing::info!(
                        event_type = %event.event_type,
                        task_iri = %event.task_iri,
                        "Cache invalidation event consumed"
                    );
                    let _ = (l0, bb);
                }
            },
        );

        let eb_prefetch = event_bus.clone();
        let bb_prefetch = blackboard.clone();
        let proj_prefetch = projection.clone();
        eb_prefetch.spawn_consumer(
            vec![
                "MEMORY_PREFETCH".to_string(),
                "PREFETCH_REQUEST".to_string(),
            ],
            move |event| {
                let bb = bb_prefetch.clone();
                let proj = proj_prefetch.clone();
                async move {
                    tracing::info!(
                        event_type = %event.event_type,
                        task_iri = %event.task_iri,
                        "Prefetch request event consumed"
                    );
                    let _ = (bb, proj);
                }
            },
        );

        let eb_tasks = event_bus.clone();
        eb_tasks.spawn_consumer(
            vec![
                "TASK_STARTED".to_string(),
                "TASK_COMPLETED".to_string(),
                "TASK_FAILED".to_string(),
                "AGENT_ERROR".to_string(),
            ],
            move |event| async move {
                match event.event_type.as_str() {
                    "TASK_FAILED" | "AGENT_ERROR" => {
                        tracing::warn!(
                            event_type = %event.event_type,
                            task_iri = %event.task_iri,
                            source = %event.source_agent_iri,
                            "Task failure event"
                        );
                    }
                    _ => {
                        tracing::info!(
                            event_type = %event.event_type,
                            task_iri = %event.task_iri,
                            source = %event.source_agent_iri,
                            "Task lifecycle event"
                        );
                    }
                }
            },
        );

        // ── BatchAgent manager (sync register, async start) ──
        let skill_graph = Arc::new(
            SkillGraphStore::new()
                .with_blackboard(blackboard.clone())
                .with_l0_store(l0.clone()),
        );
        let batch_mgr = {
            let mut mgr = BatchAgentManager::new()
                .with_event_bus(event_bus.clone())
                .with_graph_store(skill_graph.clone());

            let agent_settings = &settings.batch_agents.agents;
            if !agent_settings.is_empty() {
                let results = mgr.register_maintenance_agents(agent_settings);
                let ok = results.iter().filter(|r| r.is_ok()).count();
                let err = results.len() - ok;
                tracing::info!(
                    "BatchAgent registration complete: {} OK, {} failed, {} total configs",
                    ok,
                    err,
                    results.len()
                );
                for r in results.iter().filter_map(|r| r.as_ref().err()) {
                    tracing::warn!("BatchAgent registration failed: {:?}", r);
                }
            }
            mgr
        };

        // 向量库：与 HTTP 路由共用同一实例，避免对同一目录打开两个 HyperspaceStore 句柄。
        let vector_store = crate::api::http::open_vector_store(&settings.embedding);

        let s = Self {
            settings,
            gateway,
            l0,
            blackboard: blackboard.clone(),
            projection,
            memory_manager,
            skills,
            templates,
            event_bus: event_bus.clone(),
            checkpoints,
            scheduler,
            prefetch,
            unified_graph,
            vector_store,
            execution_states: Arc::new(RwLock::new(HashMap::new())),
            batch_manager: Arc::new(tokio::sync::Mutex::new(Some(batch_mgr))),
            skill_graph: Some(skill_graph),
        };

        Ok(s)
    }

    /// Assemble axum HTTP/SSE routes, reusing the service's runtime shared state (EventBus / Blackboard /
    /// SkillRegistry etc.), so HTTP `/api/v1/tasks/stream` and gRPC task execution share the same event bus.
    pub fn build_http_router(&self) -> axum::Router {
        use crate::core::core_types::SemanticCore;
        use crate::core::validation::ValidationEngine;

        let config = CoreConfig::default();
        let core = Arc::new(SemanticCore {
            blackboard: self.blackboard.clone(),
            l0_store: self.l0.clone(),
            projection: self.projection.clone(),
            skills: self.skills.clone(),
            events: self.event_bus.clone(),
            validation: Arc::new(ValidationEngine::new(config.max_node_size)),
            checkpoints: self.checkpoints.clone(),
            config,
        });
        let config_info = self.build_config_info();
        let agents_info = {
            let agents: Vec<serde_json::Value> = self
                .settings
                .batch_agents
                .agents
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "name": a.name,
                        "description": a.description,
                        "enabled": a.enabled,
                        "business_domain": a.business_domain,
                    })
                })
                .collect();
            serde_json::json!({ "count": agents.len(), "agents": agents })
        };
        // 注入任务执行器：复用服务的共享运行态，使 HTTP `/api/v1/tasks/stream` 能真正触发 SA 执行。
        let task_executor: Option<Arc<dyn crate::api::http::TaskExecutor>> =
            Some(Arc::new(HttpTaskExecutor {
                gateway: self.gateway.clone(),
                skills: self.skills.clone(),
                blackboard: self.blackboard.clone(),
                l0: self.l0.clone(),
                memory_manager: self.memory_manager.clone(),
                templates: self.templates.clone(),
                scheduler: self.scheduler.clone(),
                prefetch: self.prefetch.clone(),
                unified_graph: self.unified_graph.clone(),
                event_bus: self.event_bus.clone(),
                vector_store: self.vector_store.clone(),
                settings: self.settings.clone(),
            }));
        crate::api::http::build_router(
            core,
            self.gateway.clone(),
            self.unified_graph.store(),
            config_info,
            agents_info,
            self.vector_store.clone(),
            task_executor,
            Some(self.batch_manager.clone()),
        )
    }

    /// 构造已脱敏的运行期配置快照（不暴露 api_key 明文，仅暴露是否已配置）。
    /// 外部 LLM 网关字段与 GatewaySettings 对齐，供前端 Settings 页展示与对照。
    fn build_config_info(&self) -> serde_json::Value {
        let g = &self.settings.gateway;
        // api_key_configured: 优先取 settings 中的 key（来自 config.yaml / config_override.json），
        // 若为空则检查环境变量（AGENT_OS_GATEWAY_API_KEY）作为兜底，确保 ConfigMap/Secret 注入方式也能正确展示。
        let api_key_configured = !g.api_key.is_empty()
            || std::env::var("AGENT_OS_GATEWAY_API_KEY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
        // Embedding（向量化）：脱敏 oneapi.api_key；active_dimension 为当前生效维度（供前端提示重建）。
        let e = &self.settings.embedding;
        let active_dimension = if !e.enabled {
            e.fallback.dimension
        } else {
            match e.provider.as_str() {
                "ollama" => e.ollama.dimension,
                "oneapi" => e.oneapi.dimension,
                _ => e.fallback.dimension,
            }
        };
        // Models 注册表脱敏快照：provider.api_key → api_key_configured；resources 无密钥原样。
        let models = {
            let m = &self.settings.models;
            let providers: Vec<serde_json::Value> = m
                .providers
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "name": p.name,
                        "base_url": p.base_url,
                        "kind": p.kind,
                        "enabled": p.enabled,
                        "timeout_seconds": p.timeout_seconds,
                        "api_key_configured": !p.api_key.is_empty(),
                    })
                })
                .collect();
            let resources: Vec<serde_json::Value> = m
                .resources
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "name": r.name,
                        "provider_id": r.provider_id,
                        "model": r.model,
                        "modalities": r.modalities,
                        "enabled": r.enabled,
                        "context_window": r.context_window,
                        "dimension": r.dimension,
                        "supports_tools": r.supports_tools,
                        "supports_reasoning": r.supports_reasoning,
                        "supports_vision": r.supports_vision,
                    })
                })
                .collect();
            serde_json::json!({ "providers": providers, "resources": resources })
        };
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "models": models,
            "gateway": {
                "base_url": g.base_url,
                "default_model": g.default_model,
                "max_retries": g.max_retries,
                "timeout_seconds": g.timeout_seconds,
                "model_mapping": g.model_mapping,
                "api_key_configured": api_key_configured,
            },
            "embedding": {
                "enabled": e.enabled,
                "provider": e.provider,
                "active_dimension": active_dimension,
                "ollama": {
                    "base_url": e.ollama.base_url,
                    "model": e.ollama.model,
                    "dimension": e.ollama.dimension,
                },
                "oneapi": {
                    "base_url": e.oneapi.base_url,
                    "model": e.oneapi.model,
                    "dimension": e.oneapi.dimension,
                    "api_key_configured": !e.oneapi.api_key.is_empty(),
                },
                "fallback": {
                    "dimension": e.fallback.dimension,
                },
            },
            "api": {
                "grpc_addr": self.settings.api.grpc_addr,
                "http_addr": self.settings.api.http_addr,
                "metrics_port": self.settings.api.metrics_port,
            },
            "memory": {
                "l1_max_messages": self.settings.memory.l1.max_messages,
                "l2_max_node_size": self.settings.memory.l2.max_node_size,
            },
            "workspace": {
                "watch_enabled": self.settings.workspace.watch_enabled,
                "poll_interval_ms": self.settings.workspace.poll_interval_ms,
                "debounce_ms": self.settings.workspace.debounce_ms,
                "max_debounce_wait_ms": self.settings.workspace.max_debounce_wait_ms,
                "content_store_max_bytes": self.settings.workspace.content_store_max_bytes,
                "content_cache_capacity": self.settings.workspace.content_cache_capacity,
            },
            "sandbox": crate::tools::builtin::sandbox::sandbox_runtime_snapshot(),
            "verify_first": crate::core::sa::verify_first_runtime_snapshot(),
            "memory_scheduler": crate::memory::scheduler::MemoryScheduler::runtime_snapshot(),
            "embedding_health": crate::memory::embedding_health_snapshot(),
            "agents": {
                "max_iterations": self.settings.agents.max_iterations,
                "max_parallel_agents": self.settings.agents.max_parallel_agents,
            },
            "admin_policies": {
                "iam": {
                    "access_token_hours": self.settings.admin_policies.iam.access_token_hours,
                    "refresh_token_days": self.settings.admin_policies.iam.refresh_token_days,
                    "mfa_for_sensitive_actions": self.settings.admin_policies.iam.mfa_for_sensitive_actions,
                },
                "security": {
                    "prompt_injection_protection": self.settings.admin_policies.security.prompt_injection_protection,
                    "hallucination_threshold": self.settings.admin_policies.security.hallucination_threshold,
                    "max_tool_calls": self.settings.admin_policies.security.max_tool_calls,
                    "pii_redaction": self.settings.admin_policies.security.pii_redaction,
                },
                "storage": {
                    "task_retention_days": self.settings.admin_policies.storage.task_retention_days,
                    "session_retention_hours": self.settings.admin_policies.storage.session_retention_hours,
                    "audit_retention_days": self.settings.admin_policies.storage.audit_retention_days,
                },
            },
        })
    }

    /// Async start BatchAgent system + background maintenance tasks. Call before gRPC serve.
    pub async fn init_batch_system(&self) {
        let mut guard = self.batch_manager.lock().await;
        if let Some(ref mut mgr) = *guard {
            match mgr.start(None).await {
                Ok(()) => tracing::info!("BatchAgent system started"),
                Err(e) => tracing::warn!("BatchAgent partial startup failure: {:?}", e),
            }
        } else {
            tracing::info!("BatchAgent initialized or disabled");
        }
        drop(guard);

        // ── Background maintenance: archive + re-index every 30 minutes ──
        if let Some(ref sg) = self.skill_graph {
            let sg_clone = sg.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1800));
                loop {
                    interval.tick().await;

                    // Archive cold skills (L2→L0, last_used > 48 hours ago)
                    let cutoff = chrono::Utc::now() - chrono::Duration::hours(48);
                    match sg_clone.archive_cold_skills(&cutoff) {
                        Ok(archived) => {
                            if archived > 0 {
                                tracing::info!(
                                    archived = archived,
                                    "Maintenance: archived cold skills"
                                );
                            }
                        }
                        Err(e) => tracing::warn!("Maintenance: archive_cold_skills failed: {}", e),
                    }

                    // Re-index stale skills (updated_at > 4 hours ago)
                    let stale_age = chrono::Duration::hours(4);
                    let reindexed = sg_clone.reindex_stale_skills(&stale_age);
                    if reindexed > 0 {
                        tracing::info!(
                            reindexed = reindexed,
                            "Maintenance: re-indexed stale skills"
                        );
                    }
                }
            });
            tracing::info!(
                "Background maintenance task spawned (archive=48h, reindex=4h, interval=30min)"
            );
        }
    }

    fn create_sa(&self, settings: &Settings) -> SupervisorAgent {
        build_supervisor_agent(
            self.gateway.clone(),
            self.skills.clone(),
            self.blackboard.clone(),
            self.l0.clone(),
            self.memory_manager.clone(),
            self.templates.clone(),
            self.scheduler.clone(),
            self.prefetch.clone(),
            self.unified_graph.clone(),
            self.event_bus.clone(),
            self.vector_store.clone(),
            settings,
        )
    }

    fn apply_request_settings(&self, req: &impl RequestSettings) -> Settings {
        let mut settings = self.settings.clone();
        req.apply_to(&mut settings);
        settings
    }
}

/// 从共享运行态组件装配一个 SupervisorAgent（PDCA 管线）。
///
/// gRPC 的 `create_sa` 与 HTTP 的 `HttpTaskExecutor` 共用此函数，避免重复装配逻辑，
/// 并确保两条入口用的是**同一套**共享运行态（EventBus / Blackboard / 内存分层 / KG）。
#[allow(clippy::too_many_arguments)]
fn build_supervisor_agent(
    gateway: Arc<UnifiedGateway>,
    skills: Arc<SkillRegistry>,
    blackboard: Arc<Blackboard>,
    l0: Arc<L0Store>,
    memory_manager: Arc<tokio::sync::Mutex<MemoryManager>>,
    templates: Arc<TemplateEngine>,
    scheduler: Arc<MemoryScheduler>,
    prefetch: Arc<PrefetchEngine>,
    unified_graph: Arc<UnifiedGraphStore>,
    event_bus: Arc<EventBus>,
    vector_store: SharedVectorStore,
    settings: &Settings,
) -> SupervisorAgent {
    // initialize WorkspaceMonitor (if workspace root is configured)
    let workspace_root_path: Option<std::path::PathBuf> = settings
        .workspace
        .root
        .as_ref()
        .map(std::path::PathBuf::from);
    let workspace_monitor_opt: Option<Arc<WorkspaceMonitor>> =
        if let Some(ref ws_root) = workspace_root_path {
            let ws_config = WorkspaceMonitorConfig {
                workspace_root: ws_root.clone(),
                exclude_patterns: settings.workspace.exclude_patterns.clone(),
                watch_enabled: settings.workspace.watch_enabled,
                content_store_max_bytes: settings.workspace.content_store_max_bytes,
                ..Default::default()
            };
            match WorkspaceMonitor::initialize(
                ws_config,
                Some(blackboard.clone()),
                Some(event_bus.clone()),
            ) {
                Ok(ws) => {
                    tracing::info!(root = %ws_root.display(), "WorkspaceMonitor initialized");
                    Some(Arc::new(ws))
                }
                Err(e) => {
                    tracing::warn!(
                        "WorkspaceMonitor init failed: {}, using default workspace settings",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

    let mut runner_builder = AgentRunner::new(
        gateway.clone(),
        skills.clone(),
        blackboard.clone(),
        l0.clone(),
        memory_manager.clone(),
        templates.clone(),
        settings.agents.clone(),
    )
    .with_scheduler(scheduler.clone())
    .with_prefetch_engine(prefetch.clone())
    .with_unified_graph_store(unified_graph.store());
    if let Some(ref ws_root) = workspace_root_path {
        runner_builder = runner_builder.with_workspace_root(ws_root.clone());
    }

    let runner = Arc::new(runner_builder);

    {
        let ug_store = unified_graph.store();
        let mut executor = runner.tool_executor.write();
        executor.set_unified_kg_store(ug_store);
        // set workspace_monitor on ToolExecutor
        if let Some(ref wm) = workspace_monitor_opt {
            executor.set_workspace_monitor(wm.clone());
        }
        // 注入向量库，使 SA 工具链的 kb_vector_search 可做语义召回
        if let Some(vs) = vector_store.load_full() {
            executor.set_vector_store(vs);
        }
    }

    // register WorkspaceMonitor hooks into AgentRunner's hook_manager
    if let Some(ref wm) = workspace_monitor_opt {
        wm.register_hooks(&runner.hook_manager);
    }

    // complete AgentRunner init wiring: perception_store → WorkspaceMonitor
    runner.finalize_setup();

    let mut sa = SupervisorAgent::with_pdca_cycles(
        runner,
        templates.clone(),
        skills.clone(),
        event_bus.clone(),
        settings.agents.max_iterations,
        settings.agents.max_pdca_cycles,
    );

    sa = sa.with_memory(
        Some(blackboard.clone()),
        Some(prefetch.clone()),
        Some(scheduler.clone()),
    );
    sa
}

/// HTTP/SSE 任务执行器：持有已运行服务的共享运行态，在后台驱动 SA PDCA 管线，
/// 并把执行事件（phase / completion）发布到共享事件总线，供 SSE 流转发给前端。
pub struct HttpTaskExecutor {
    gateway: Arc<UnifiedGateway>,
    skills: Arc<SkillRegistry>,
    blackboard: Arc<Blackboard>,
    l0: Arc<L0Store>,
    memory_manager: Arc<tokio::sync::Mutex<MemoryManager>>,
    templates: Arc<TemplateEngine>,
    scheduler: Arc<MemoryScheduler>,
    prefetch: Arc<PrefetchEngine>,
    unified_graph: Arc<UnifiedGraphStore>,
    event_bus: Arc<EventBus>,
    vector_store: SharedVectorStore,
    settings: Settings,
}

#[async_trait::async_trait]
impl crate::api::http::TaskExecutor for HttpTaskExecutor {
    async fn execute(&self, spec: crate::api::http::TaskExecSpec) {
        let mut sa = build_supervisor_agent(
            self.gateway.clone(),
            self.skills.clone(),
            self.blackboard.clone(),
            self.l0.clone(),
            self.memory_manager.clone(),
            self.templates.clone(),
            self.scheduler.clone(),
            self.prefetch.clone(),
            self.unified_graph.clone(),
            self.event_bus.clone(),
            self.vector_store.clone(),
            &self.settings,
        );

        let emitter = ExecutionEventEmitter::with_options(
            &spec.task_iri,
            None,
            Some(self.event_bus.clone()),
            spec.include_thought,
            spec.include_tool_calls,
        );
        emitter.emit_phase_change("idle", "plan", "PA", "Task started");

        match sa.process_task(&spec.prompt, &spec.task_iri).await {
            Ok(result) => {
                emitter.emit_completion(&result.status, &result.summary, result.output.clone());
                // 显式在共享总线上推送终态，供 SSE 循环终止并转发 completion 给前端。
                let event_type = if result.status == "failed" {
                    "TASK_FAILED"
                } else {
                    "TASK_COMPLETED"
                };
                self.event_bus
                    .emit(
                        &spec.task_iri,
                        event_type,
                        "SA",
                        &serde_json::json!({"status": result.status, "summary": result.summary})
                            .to_string(),
                    )
                    .await;
            }
            Err(e) => {
                emitter.emit_error("ExecutionError", &e.to_string(), "SA", false);
                emitter.emit_completion("failed", &e.to_string(), None);
                self.event_bus
                    .emit(
                        &spec.task_iri,
                        "TASK_FAILED",
                        "SA",
                        &serde_json::json!({"status": "failed", "summary": e.to_string()})
                            .to_string(),
                    )
                    .await;
            }
        }
    }
}

trait RequestSettings {
    fn apply_to(&self, settings: &mut Settings);
}

impl AgentOSService {
    pub async fn send_supplementary_input(&self, task_iri: &str, content: &str) {
        tracing::info!(task_iri = %task_iri, "Received user supplementary input");
        self.event_bus
            .emit(task_iri, "USER_SUPPLEMENTARY_INPUT", "external", content)
            .await;
    }
}

impl RequestSettings for ExecuteStageRequest {
    fn apply_to(&self, settings: &mut Settings) {
        if !self.llm_api_key.is_empty() {
            settings.gateway.api_key = self.llm_api_key.clone();
        }
        if !self.llm_base_url.is_empty() {
            settings.gateway.base_url = self.llm_base_url.clone();
        }
        if !self.llm_model.is_empty() {
            settings.gateway.default_model = self.llm_model.clone();
            settings
                .gateway
                .model_mapping
                .insert("default".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("planning".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("execution".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("analysis".to_string(), self.llm_model.clone());
        }
    }
}

impl RequestSettings for ChatStreamRequest {
    fn apply_to(&self, settings: &mut Settings) {
        if !self.llm_api_key.is_empty() {
            settings.gateway.api_key = self.llm_api_key.clone();
        }
        if !self.llm_base_url.is_empty() {
            settings.gateway.base_url = self.llm_base_url.clone();
        }
        if !self.llm_model.is_empty() {
            settings.gateway.default_model = self.llm_model.clone();
            settings
                .gateway
                .model_mapping
                .insert("default".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("planning".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("execution".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("analysis".to_string(), self.llm_model.clone());
        }
    }
}

impl RequestSettings for ExecuteTaskStreamRequest {
    fn apply_to(&self, settings: &mut Settings) {
        if !self.llm_api_key.is_empty() {
            settings.gateway.api_key = self.llm_api_key.clone();
        }
        if !self.llm_base_url.is_empty() {
            settings.gateway.base_url = self.llm_base_url.clone();
        }
        if !self.llm_model.is_empty() {
            settings.gateway.default_model = self.llm_model.clone();
            settings
                .gateway
                .model_mapping
                .insert("default".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("planning".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("execution".to_string(), self.llm_model.clone());
            settings
                .gateway
                .model_mapping
                .insert("analysis".to_string(), self.llm_model.clone());
        }
    }
}

#[tonic::async_trait]
impl seapp::se_kernel_service_server::SeKernelService for AgentOSService {
    type ChatStreamStream = Pin<Box<dyn Stream<Item = Result<ChatStreamChunk, Status>> + Send>>;
    type ExecuteTaskStreamStream =
        Pin<Box<dyn Stream<Item = Result<seapp::ExecutionEvent, Status>> + Send>>;

    async fn execute_stage(
        &self,
        request: Request<ExecuteStageRequest>,
    ) -> Result<Response<ExecuteStageResponse>, Status> {
        let req = request.into_inner();
        let settings = self.apply_request_settings(&req);

        let mut sa = self.create_sa(&settings);

        let task_iri = if req.task_iri.is_empty() {
            format!("iri://stage/{}", req.stage_id)
        } else {
            req.task_iri
        };

        let result = sa
            .process_task(&req.prompt, &task_iri)
            .await
            .map_err(|e| Status::internal(format!("SA execution failed: {}", e)))?;

        let output_bytes = match &result.output {
            Some(v) => serde_json::to_vec(v).unwrap_or_default(),
            None => Vec::new(),
        };

        Ok(Response::new(ExecuteStageResponse {
            status: result.status.clone(),
            summary: result.summary.clone(),
            output_json: output_bytes,
            output_iri: task_iri,
            artifacts: vec![],
            errors: result.errors.clone(),
        }))
    }

    async fn chat_stream(
        &self,
        request: Request<ChatStreamRequest>,
    ) -> Result<Response<Self::ChatStreamStream>, Status> {
        let req = request.into_inner();
        let settings = self.apply_request_settings(&req);

        let (tx, rx) = mpsc::channel::<Result<ChatStreamChunk, Status>>(64);

        let mut sa = self.create_sa(&settings);

        let task_iri = if req.task_iri.is_empty() {
            format!("iri://chat/{}", uuid::Uuid::new_v4().hyphenated())
        } else {
            req.task_iri.clone()
        };

        let _ = tx
            .send(Ok(ChatStreamChunk {
                content: String::new(),
                done: false,
                status: "processing".to_string(),
            }))
            .await;

        match sa.process_task(&req.prompt, &task_iri).await {
            Ok(result) => {
                let content = extract_content(&result);

                let chunk_size = 20;
                let chars: Vec<char> = content.chars().collect();
                for chunk in chars.chunks(chunk_size) {
                    let chunk_str: String = chunk.iter().collect();
                    if tx
                        .send(Ok(ChatStreamChunk {
                            content: chunk_str,
                            done: false,
                            status: "streaming".to_string(),
                        }))
                        .await
                        .is_err()
                    {
                        return Ok(Response::new(Box::pin(
                            tokio_stream::wrappers::ReceiverStream::new(rx),
                        )));
                    }
                }

                let _ = tx
                    .send(Ok(ChatStreamChunk {
                        content: String::new(),
                        done: true,
                        status: result.status.clone(),
                    }))
                    .await;
            }
            Err(e) => {
                let _ = tx
                    .send(Ok(ChatStreamChunk {
                        content: format!("Error: {}", e),
                        done: true,
                        status: "error".to_string(),
                    }))
                    .await;
            }
        }

        let output = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output)))
    }

    async fn execute_task_stream(
        &self,
        request: Request<ExecuteTaskStreamRequest>,
    ) -> Result<Response<Self::ExecuteTaskStreamStream>, Status> {
        let req = request.into_inner();
        let settings = self.apply_request_settings(&req);

        let (tx, rx) = mpsc::channel::<Result<seapp::ExecutionEvent, Status>>(256);

        let task_iri = if req.task_iri.is_empty() {
            let seq = TASK_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            format!("iri://stream/{}", seq)
        } else {
            req.task_iri.clone()
        };

        let include_thought = req.include_thought;
        let include_tool_calls = req.include_tool_calls;

        {
            let mut states = self.execution_states.write().await;
            states.insert(task_iri.clone(), ExecutionState::new());
        }

        let event_bus = self.event_bus.clone();
        let states = self.execution_states.clone();
        let task_iri_clone = task_iri.clone();
        let mut event_rx = event_bus.subscribe();

        let tx_clone = tx.clone();
        let states_clone = states.clone();
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        if event.task_iri != task_iri_clone {
                            continue;
                        }

                        if let Some((core_event, proto_event)) = convert_event_bus_to_grpc(&event) {
                            let mut states = states_clone.write().await;
                            if let Some(state) = states.get_mut(&task_iri_clone) {
                                state.update_from_event(&core_event);
                            }
                            if tx_clone.send(Ok(proto_event)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        let settings_clone = settings.clone();
        let sa_settings = settings.clone();
        let prompt = req.prompt.clone();
        let task_iri_for_task = task_iri.clone();
        let _tx_for_task = tx.clone();
        let event_bus_for_task = self.event_bus.clone();

        tokio::spawn(async move {
            let mut sa = {
                let service =
                    AgentOSService::new(settings_clone).expect("AgentOSService::new failed");
                service.create_sa(&sa_settings)
            };

            let emitter = ExecutionEventEmitter::with_options(
                &task_iri_for_task,
                None,
                Some(event_bus_for_task),
                include_thought,
                include_tool_calls,
            );

            emitter.emit_phase_change("idle", "plan", "PA", "Task started");

            match sa.process_task(&prompt, &task_iri_for_task).await {
                Ok(result) => {
                    emitter.emit_completion(&result.status, &result.summary, result.output.clone());
                }
                Err(e) => {
                    emitter.emit_error("ExecutionError", &e.to_string(), "SA", false);
                    emitter.emit_completion("failed", &e.to_string(), None);
                }
            }

            {
                let mut states = states.write().await;
                states.remove(&task_iri_for_task);
            }
        });

        let output = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output)))
    }

    async fn get_execution_details(
        &self,
        request: Request<GetExecutionDetailsRequest>,
    ) -> Result<Response<ExecutionDetails>, Status> {
        let req = request.into_inner();
        let task_iri = req.task_iri;

        let states = self.execution_states.read().await;
        let state = states.get(&task_iri).cloned().unwrap_or_default();

        let details = ExecutionDetails {
            task_iri: task_iri.clone(),
            status: "running".to_string(),
            current_phase: state.current_phase.clone(),
            plan: None,
            steps: vec![],
            agent_sessions: vec![],
            stats: Some(ExecutionStats {
                total_turns: state.current_turn as i32,
                total_tool_calls: 0,
                total_tokens: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                error_count: 0,
                retry_count: 0,
            }),
            created_at: String::new(),
            updated_at: String::new(),
            duration_ms: 0,
        };

        Ok(Response::new(details))
    }

    async fn get_realtime_status(
        &self,
        request: Request<GetRealtimeStatusRequest>,
    ) -> Result<Response<RealtimeStatus>, Status> {
        let req = request.into_inner();
        let task_iri = req.task_iri;

        let states = self.execution_states.read().await;
        let state = states.get(&task_iri).cloned().unwrap_or_default();

        let status = RealtimeStatus {
            task_iri: task_iri.clone(),
            status: "running".to_string(),
            current_phase: state.current_phase.clone(),
            current_agent: Some(CurrentAgentInfo {
                id: state.current_agent_id.clone(),
                role: state.current_agent_role.clone(),
                status: "running".to_string(),
                turn: state.current_turn as i32,
            }),
            current_action: state.current_tool.as_ref().map(|t| CurrentActionInfo {
                r#type: "tool_call".to_string(),
                tool_name: t.clone(),
                started_at: String::new(),
            }),
            progress: Some(ExecutionProgress {
                completed_steps: state.completed_steps as i32,
                total_steps: state.total_steps as i32,
                percentage: (state.completed_steps * 100)
                    .checked_div(state.total_steps)
                    .unwrap_or(0) as i32,
                estimated_remaining_ms: 0,
            }),
            phase_history: state
                .phase_history
                .iter()
                .map(|p| PhaseHistoryEntry {
                    phase: p.phase.clone(),
                    agent_id: p.agent_id.clone(),
                    started_at: p.started_at,
                    completed_at: p.completed_at.unwrap_or(0),
                    status: p.status.clone(),
                })
                .collect(),
        };

        Ok(Response::new(status))
    }

    async fn validate_contract(
        &self,
        _request: Request<ValidateContractRequest>,
    ) -> Result<Response<ValidateContractResponse>, Status> {
        Ok(Response::new(ValidateContractResponse {
            valid: true,
            violations: vec![],
        }))
    }

    async fn flatten_to_frontend(
        &self,
        _request: Request<FlattenRequest>,
    ) -> Result<Response<FlattenResponse>, Status> {
        Ok(Response::new(FlattenResponse {
            frontend_json: "{}".to_string(),
        }))
    }

    async fn submit_human_approval(
        &self,
        _request: Request<SubmitApprovalRequest>,
    ) -> Result<Response<SubmitApprovalResponse>, Status> {
        Ok(Response::new(SubmitApprovalResponse {
            success: true,
            message: "ok".to_string(),
        }))
    }
}

fn convert_event_bus_to_grpc(
    event: &crate::core::event_bus::Event,
) -> Option<(
    crate::core::execution_event::ExecutionEvent,
    seapp::ExecutionEvent,
)> {
    use crate::core::event_bus::EventType;
    use crate::core::execution_event::ExecutionEvent as CoreExecutionEvent;

    let event_type = EventType::from_str(&event.event_type);
    let timestamp = event.timestamp.timestamp_millis();

    let kind = match event_type {
        EventType::PlanStarted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "idle".to_string(),
                to_phase: "plan".to_string(),
                agent_role: "PA".to_string(),
                reason: "Plan phase started".to_string(),
            })
        }
        EventType::PlanCompleted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "plan".to_string(),
                to_phase: "do".to_string(),
                agent_role: "PA".to_string(),
                reason: "Plan phase completed".to_string(),
            })
        }
        EventType::DoStarted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "plan".to_string(),
                to_phase: "do".to_string(),
                agent_role: "DA".to_string(),
                reason: "Do phase started".to_string(),
            })
        }
        EventType::DoCompleted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "do".to_string(),
                to_phase: "check".to_string(),
                agent_role: "DA".to_string(),
                reason: "Do phase completed".to_string(),
            })
        }
        EventType::CheckStarted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "do".to_string(),
                to_phase: "check".to_string(),
                agent_role: "CA".to_string(),
                reason: "Check phase started".to_string(),
            })
        }
        EventType::CheckCompleted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "check".to_string(),
                to_phase: "act".to_string(),
                agent_role: "CA".to_string(),
                reason: "Check phase completed".to_string(),
            })
        }
        EventType::ActStarted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "check".to_string(),
                to_phase: "act".to_string(),
                agent_role: "AA".to_string(),
                reason: "Act phase started".to_string(),
            })
        }
        EventType::ActCompleted => {
            ExecutionEventKind::PhaseChange(crate::core::execution_event::PhaseChange {
                from_phase: "act".to_string(),
                to_phase: "completed".to_string(),
                agent_role: "AA".to_string(),
                reason: "Act phase completed".to_string(),
            })
        }
        EventType::AgentStarted => {
            ExecutionEventKind::AgentStatus(crate::core::execution_event::AgentStatus {
                agent_id: event.source_agent_iri.clone(),
                role: "unknown".to_string(),
                status: "running".to_string(),
                turn: 0,
                iteration: 0,
                timestamp: None,
            })
        }
        EventType::AgentCompleted => {
            ExecutionEventKind::AgentStatus(crate::core::execution_event::AgentStatus {
                agent_id: event.source_agent_iri.clone(),
                role: "unknown".to_string(),
                status: "completed".to_string(),
                turn: 0,
                iteration: 0,
                timestamp: None,
            })
        }
        EventType::AgentError => ExecutionEventKind::Error(crate::core::execution_event::Error {
            error_type: "AgentError".to_string(),
            message: event.payload.clone(),
            agent_id: event.source_agent_iri.clone(),
            recoverable: false,
        }),
        EventType::TaskCompleted => {
            ExecutionEventKind::Completion(crate::core::execution_event::Completion {
                status: "success".to_string(),
                summary: event.payload.clone(),
                total_turns: 0,
                total_tool_calls: 0,
                total_tokens: 0,
                output_json: None,
            })
        }
        EventType::TaskFailed => {
            ExecutionEventKind::Completion(crate::core::execution_event::Completion {
                status: "failed".to_string(),
                summary: event.payload.clone(),
                total_turns: 0,
                total_tool_calls: 0,
                total_tokens: 0,
                output_json: None,
            })
        }
        _ => return None,
    };

    let core_event = CoreExecutionEvent {
        event_id: event.event_id.clone(),
        task_iri: event.task_iri.clone(),
        timestamp,
        event: kind.clone(),
    };

    let proto_event = seapp::ExecutionEvent {
        event_id: event.event_id.clone(),
        task_iri: event.task_iri.clone(),
        timestamp,
        event: Some(kind_to_proto_event(kind)),
    };

    Some((core_event, proto_event))
}

fn kind_to_proto_event(kind: ExecutionEventKind) -> seapp::execution_event::Event {
    use seapp::execution_event::Event;

    match kind {
        ExecutionEventKind::PhaseChange(pc) => Event::PhaseChange(PhaseChangeEvent {
            from_phase: pc.from_phase,
            to_phase: pc.to_phase,
            agent_role: pc.agent_role,
            reason: pc.reason,
        }),
        ExecutionEventKind::AgentStatus(as_) => Event::AgentStatus(AgentStatusEvent {
            agent_id: as_.agent_id,
            role: as_.role,
            status: as_.status,
            turn: as_.turn as i32,
            iteration: as_.iteration as i32,
        }),
        ExecutionEventKind::LlmContent(lc) => Event::LlmContent(LlmContentEvent {
            agent_id: lc.agent_id,
            role: lc.role,
            content_delta: lc.content_delta,
            is_reasoning: lc.is_reasoning,
            token_count: lc.token_count as i32,
        }),
        ExecutionEventKind::ToolCall(tc) => Event::ToolCall(ToolCallEvent {
            call_id: tc.call_id,
            tool_name: tc.tool_name,
            arguments_json: tc.arguments_json,
            agent_id: tc.agent_id,
            sequence: tc.sequence as i32,
        }),
        ExecutionEventKind::ToolResult(tr) => Event::ToolResult(ToolResultEvent {
            call_id: tr.call_id,
            tool_name: tr.tool_name,
            result: tr.result,
            success: tr.success,
            result_size_bytes: tr.result_size_bytes as i32,
            duration_ms: tr.duration_ms as i32,
        }),
        ExecutionEventKind::Thought(t) => Event::Thought(ThoughtEvent {
            agent_id: t.agent_id,
            thought: t.thought,
            action: t.action,
            emphasis: t.emphasis,
        }),
        ExecutionEventKind::TokenUsage(tu) => Event::TokenUsage(TokenUsageEvent {
            prompt_tokens: tu.prompt_tokens as i32,
            completion_tokens: tu.completion_tokens as i32,
            total_tokens: tu.total_tokens as i32,
            model: tu.model,
            turn: tu.turn as i32,
        }),
        ExecutionEventKind::Error(e) => Event::Error(ErrorEvent {
            error_type: e.error_type,
            message: e.message,
            agent_id: e.agent_id,
            recoverable: e.recoverable,
        }),
        ExecutionEventKind::Completion(c) => Event::Completion(CompletionEvent {
            status: c.status,
            summary: c.summary,
            total_turns: c.total_turns as i32,
            total_tool_calls: c.total_tool_calls as i32,
            total_tokens: c.total_tokens as i32,
            output_json: c
                .output_json
                .map(|v| serde_json::to_string(&v).unwrap_or_default())
                .unwrap_or_default(),
        }),
    }
}

fn extract_content(result: &crate::core::agent_runner::TaskResult) -> String {
    if let Some(ref output) = result.output {
        match output {
            serde_json::Value::String(s) => {
                let cleaned = clean_content(s);
                if !cleaned.is_empty() {
                    return cleaned;
                }
            }
            serde_json::Value::Object(map) => {
                if let Some(content) = map.get("content").and_then(|v| v.as_str()) {
                    let cleaned = clean_content(content);
                    if !cleaned.is_empty() {
                        return cleaned;
                    }
                }
                if let Some(summary) = map.get("summary").and_then(|v| v.as_str()) {
                    let cleaned = clean_content(summary);
                    if !cleaned.is_empty() {
                        return cleaned;
                    }
                }
            }
            _ => {}
        }
        if let Ok(formatted) = serde_json::to_string_pretty(output) {
            return formatted;
        }
    }

    if !result.summary.is_empty() {
        return clean_content(&result.summary);
    }

    "No content returned".to_string()
}

fn clean_content(text: &str) -> String {
    let re = regex::Regex::new(r#"\{[^}]*"thought"[^}]*\}"#).ok();
    let cleaned = re
        .map(|r| r.replace_all(text, "").to_string())
        .unwrap_or_else(|| text.to_string());
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        text.to_string()
    } else {
        cleaned
    }
}
