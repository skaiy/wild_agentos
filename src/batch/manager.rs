use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tracing::{debug, info, warn};

use crate::batch::emitter::BatchEventEmitter;
use crate::batch::error::BatchError;
use crate::batch::handlers;
use crate::batch::trigger::TriggerSystem;
use crate::batch::types::{BatchAgentConfig, BatchAgentStatus, BatchMetrics, ExtractionResult};
use crate::batch::window::SlidingWindow;
use crate::batch::window::WindowConfig;
use crate::config::settings::BatchAgentSettings;
use crate::core::event_bus::EventBus;
use crate::skill_graph::graph_store::SkillGraphStore;

/// Holds the runtime state for a single Batch Agent instance.
pub struct BatchAgentInstance {
    pub name: String,
    pub config: BatchAgentConfig,
    pub status: BatchAgentStatus,
    pub window: Arc<RwLock<SlidingWindow>>,
    pub trigger_system: TriggerSystem,
    pub metrics: BatchMetrics,
}

pub struct BatchAgentManager {
    agents: HashMap<String, BatchAgentInstance>,
    event_bus: Option<Arc<EventBus>>,
    emitter: Option<BatchEventEmitter>,
    event_scope: Option<(String, String)>,
    graph_store: Option<Arc<SkillGraphStore>>,
    running: bool,
}

impl BatchAgentManager {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            event_bus: None,
            emitter: None,
            event_scope: None,
            graph_store: None,
            running: false,
        }
    }

    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        let mut emitter = BatchEventEmitter::new(event_bus.clone());
        if let Some((tenant_id, project_id)) = &self.event_scope {
            emitter.set_scope(tenant_id.clone(), project_id.clone());
        }
        self.event_bus = Some(event_bus);
        self.emitter = Some(emitter);
        self
    }

    /// Scope stamped on batch events. Call before or after `with_event_bus`.
    pub fn with_event_scope(
        mut self,
        tenant_id: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        let scope = (tenant_id.into(), project_id.into());
        if let Some(emitter) = self.emitter.as_mut() {
            emitter.set_scope(scope.0.clone(), scope.1.clone());
        }
        self.event_scope = Some(scope);
        self
    }

    pub fn with_graph_store(mut self, graph_store: Arc<SkillGraphStore>) -> Self {
        self.graph_store = Some(graph_store);
        self
    }

    pub fn register(&mut self, config: BatchAgentConfig) -> Result<(), BatchError> {
        let name = config.name.clone();

        if self.agents.contains_key(&name) {
            return Err(BatchError::AgentAlreadyExists { name });
        }

        let window_config = match &config.window_type {
            crate::batch::types::WindowType::MessageCount(n) => WindowConfig {
                max_entries: *n,
                min_entries: 1,
                time_window_secs: u64::MAX,
                intent_shift_threshold: 0.6,
            },
            crate::batch::types::WindowType::TimeWindow(secs) => WindowConfig {
                max_entries: usize::MAX,
                min_entries: 1,
                time_window_secs: *secs,
                intent_shift_threshold: 0.6,
            },
            crate::batch::types::WindowType::Hybrid {
                max_messages,
                max_seconds,
            } => WindowConfig {
                max_entries: *max_messages,
                min_entries: 1,
                time_window_secs: *max_seconds,
                intent_shift_threshold: 0.6,
            },
            crate::batch::types::WindowType::Manual => WindowConfig {
                max_entries: usize::MAX,
                min_entries: 0,
                time_window_secs: u64::MAX,
                intent_shift_threshold: 0.6,
            },
        };

        let window = Arc::new(RwLock::new(SlidingWindow::new(window_config)));
        let mut trigger_system = TriggerSystem::new(config.triggers.clone(), window.clone());

        // Auto-register event types
        if let Some(ref mut emitter) = self.emitter {
            emitter.set_agent_config(&name, config.emit_on.clone());
        }
        if let Some(ref event_bus) = self.event_bus {
            trigger_system.listen_to(event_bus, vec![]);
        }

        let instance = BatchAgentInstance {
            name: name.clone(),
            config: config.clone(),
            status: BatchAgentStatus::Registered,
            window,
            trigger_system,
            metrics: BatchMetrics::default(),
        };

        self.agents.insert(name.clone(), instance);
        info!(agent = %name, domain = %config.business_domain, "Batch Agent registered");
        Ok(())
    }

    pub fn unregister(&mut self, name: &str) -> Result<(), BatchError> {
        if !self.agents.contains_key(name) {
            return Err(BatchError::AgentNotFound { name: name.into() });
        }
        self.agents.remove(name);
        debug!(agent = %name, "Batch Agent unregistered");
        Ok(())
    }

    pub async fn start(&mut self, name: Option<&str>) -> Result<(), BatchError> {
        if let Some(n) = name {
            let instance = self
                .agents
                .get_mut(n)
                .ok_or_else(|| BatchError::AgentNotFound { name: n.into() })?;
            if instance.status == BatchAgentStatus::Running {
                return Err(BatchError::AgentAlreadyRunning { name: n.into() });
            }
            instance.status = BatchAgentStatus::Running;
            if let Some(ref emitter) = self.emitter {
                emitter.emit_agent_started(n).await;
            }
            info!(agent = %n, "Batch Agent started");
        } else {
            let names: Vec<String> = self.agents.keys().cloned().collect();
            for n in names {
                if let Some(instance) = self.agents.get_mut(&n) {
                    if instance.status != BatchAgentStatus::Running {
                        instance.status = BatchAgentStatus::Running;
                        if let Some(ref emitter) = self.emitter {
                            emitter.emit_agent_started(&n).await;
                        }
                    }
                }
            }
            self.running = true;
            info!("All batch agents started");
        }
        Ok(())
    }

    pub async fn stop(&mut self, name: Option<&str>) -> Result<(), BatchError> {
        if let Some(n) = name {
            let instance = self
                .agents
                .get_mut(n)
                .ok_or_else(|| BatchError::AgentNotFound { name: n.into() })?;
            instance.status = BatchAgentStatus::Stopped;
            if let Some(ref emitter) = self.emitter {
                emitter.emit_agent_stopped(n, "manual_stop").await;
            }
        } else {
            let names: Vec<String> = self.agents.keys().cloned().collect();
            for n in names {
                if let Some(instance) = self.agents.get_mut(&n) {
                    instance.status = BatchAgentStatus::Stopped;
                    if let Some(ref emitter) = self.emitter {
                        emitter.emit_agent_stopped(&n, "manual_stop").await;
                    }
                }
            }
            self.running = false;
        }
        Ok(())
    }

    pub fn get_status(&self, name: &str) -> Option<BatchAgentStatus> {
        self.agents.get(name).map(|a| a.status.clone())
    }

    pub fn get_window_status(&self, name: &str) -> Option<crate::batch::types::WindowStatus> {
        self.agents.get(name).map(|a| a.window.read().status())
    }

    pub fn get_metrics(&self, name: &str) -> Option<BatchMetrics> {
        self.agents.get(name).map(|a| a.metrics.clone())
    }

    pub fn get_config(&self, name: &str) -> Option<&BatchAgentConfig> {
        self.agents.get(name).map(|a| &a.config)
    }

    pub fn push_message(
        &mut self,
        agent_name: &str,
        entry: crate::batch::types::WindowEntry,
    ) -> Result<(), BatchError> {
        let instance =
            self.agents
                .get_mut(agent_name)
                .ok_or_else(|| BatchError::AgentNotFound {
                    name: agent_name.into(),
                })?;
        instance.window.write().push(entry)
    }

    pub fn evaluate_triggers(&self, agent_name: &str) -> Vec<crate::batch::types::TriggerReason> {
        self.agents
            .get(agent_name)
            .map(|a| {
                let rt = tokio::runtime::Handle::try_current();
                match rt {
                    Ok(handle) => handle.block_on(async { a.trigger_system.evaluate().await }),
                    Err(_) => vec![],
                }
            })
            .unwrap_or_default()
    }

    pub fn drain_window(
        &mut self,
        agent_name: &str,
    ) -> Option<Vec<crate::batch::types::WindowEntry>> {
        self.agents
            .get_mut(agent_name)
            .map(|a| a.window.write().drain())
    }

    pub fn list_agents(&self) -> Vec<&str> {
        self.agents.keys().map(|s| s.as_str()).collect()
    }

    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn emitter(&self) -> Option<&BatchEventEmitter> {
        self.emitter.as_ref()
    }

    /// Register the 8 standard maintenance agents from settings.
    pub fn register_maintenance_agents(
        &mut self,
        settings: &[BatchAgentSettings],
    ) -> Vec<Result<(), BatchError>> {
        let mut results = Vec::new();
        for s in settings {
            if !s.enabled {
                continue;
            }
            let window_type = s.window_type.as_deref().unwrap_or("manual");
            let config = BatchAgentConfig {
                name: s.name.clone(),
                description: s.description.clone(),
                enabled: true,
                window_type: match window_type.to_lowercase().as_str() {
                    "manual" => crate::batch::types::WindowType::Manual,
                    "hybrid" => crate::batch::types::WindowType::Hybrid {
                        max_messages: s.window_max_messages.unwrap_or(5),
                        max_seconds: s.window_max_seconds.unwrap_or(3600),
                    },
                    _ => crate::batch::types::WindowType::MessageCount(
                        s.window_max_messages.unwrap_or(5),
                    ),
                },
                triggers: s
                    .triggers
                    .iter()
                    .map(|t| {
                        let trigger_type = match t.trigger_type.to_lowercase().as_str() {
                            s if s.starts_with("cron") => {
                                crate::batch::types::TriggerType::CronSchedule(
                                    t.params.get("schedule").cloned().unwrap_or_default(),
                                )
                            }
                            "intent_shift" | "intent" => {
                                crate::batch::types::TriggerType::IntentShift
                            }
                            s if s.starts_with("message") => {
                                crate::batch::types::TriggerType::MessageThreshold(
                                    t.params
                                        .get("threshold")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(5),
                                )
                            }
                            s if s.starts_with("custom") => {
                                crate::batch::types::TriggerType::CustomEvent(
                                    t.params.get("event").cloned().unwrap_or_default(),
                                )
                            }
                            _ => crate::batch::types::TriggerType::WindowFull,
                        };
                        crate::batch::types::TriggerConfig {
                            trigger_type,
                            params: t.params.clone(),
                        }
                    })
                    .collect(),
                prompt_source: crate::batch::types::PromptSource::HybridWithTemplate,
                prompt_template_path: s.prompt_template_path.clone(),
                prompt_template_name: s
                    .prompt_template_name
                    .clone()
                    .or_else(|| Some(s.name.clone())),
                prompt_params: std::collections::HashMap::new(),
                business_domain: s.business_domain.clone(),
                entity_types: crate::batch::vocabulary::EntityTypeConfig {
                    vocabulary: s.entity_types.clone(),
                    custom: vec![],
                },
                relation_types: crate::batch::vocabulary::RelationTypeConfig {
                    vocabulary: s.relation_types.clone(),
                    custom: vec![],
                },
                intent_types: crate::batch::vocabulary::IntentTypeConfig {
                    vocabulary: s.intent_types.clone(),
                    custom: vec![],
                },
                model: s.model.clone(),
                temperature: s.temperature,
                max_retries: s.max_retries.unwrap_or(3),
                timeout_seconds: s.timeout_seconds.unwrap_or(120),
                emit_on: s
                    .emit_on
                    .iter()
                    .map(|e| match e.to_lowercase().as_str() {
                        "new_relation" => crate::batch::types::EmitCondition::NewRelation,
                        "intent_detected" => {
                            crate::batch::types::EmitCondition::IntentDetected(vec![])
                        }
                        "confidence_above" => {
                            crate::batch::types::EmitCondition::ConfidenceAbove(0.8)
                        }
                        _ => crate::batch::types::EmitCondition::Always,
                    })
                    .collect(),
                inject_user_reminders: s.inject_user_reminders,
                inject_context_summary: s.inject_context_summary,
                inject_related_entities: true,
            };
            results.push(self.register(config));
        }
        results
    }

    /// Run the post-extraction handler for a maintenance agent.
    /// This should be called after `ExtractorPipeline` produces a result for a maintenance agent.
    pub fn run_maintenance_handler(
        &self,
        agent_name: &str,
        result: &ExtractionResult,
    ) -> handlers::HandlerOutcome {
        match self.graph_store {
            Some(ref graph) => handlers::run_handler(agent_name, result, graph.as_ref()),
            None => {
                warn!(
                    "No SkillGraphStore available — maintenance handler skipped for {}",
                    agent_name
                );
                handlers::HandlerOutcome {
                    handler_name: agent_name.to_string(),
                    actions_taken: vec!["No graph store available".to_string()],
                    ..Default::default()
                }
            }
        }
    }
}

impl Default for BatchAgentManager {
    fn default() -> Self {
        Self::new()
    }
}
