use std::collections::HashMap;
use std::sync::Arc;

use std::path::Path;
use wild_agent_os_core::config::GatewaySettings;
use wild_agent_os_core::core::agent_instance::{AgentInstance, AgentRole, AgentStatus};
use wild_agent_os_core::core::event_bus::EventBus;
use wild_agent_os_core::core::sa::{SupervisorAgent, TaskComplexity};
use wild_agent_os_core::gateway::UnifiedGateway;
use wild_agent_os_core::memory::l0_store::L0Store;
use wild_agent_os_core::memory::l2_blackboard::Blackboard;
use wild_agent_os_core::memory::l3_projection::ProjectionEngine;
use wild_agent_os_core::memory::memory_manager::MemoryManager;
use wild_agent_os_core::templates::template_engine::TemplateEngine;
use wild_agent_os_core::tools::skill_registry::SkillRegistry;
use wild_agent_os_core::CoreConfig;

fn test_gateway_settings() -> GatewaySettings {
    GatewaySettings {
        base_url: "http://localhost:3000".to_string(),
        api_key: "sk-test".to_string(),
        default_model: "deepseek-v4-flash".to_string(),
        timeout_seconds: 30,
        max_retries: 3,
        retry_base_ms: 500,
        use_responses_api: false,
        model_mapping: HashMap::new(),
    }
}

fn make_sa() -> SupervisorAgent {
    let l2 = Arc::new(Blackboard::new().unwrap());
    let l0_dir = tempfile::tempdir().unwrap();
    let l0 = super::writable_l0(l0_dir.path());
    let proj = Arc::new(ProjectionEngine::new(l2.clone(), 500));
    let mm = Arc::new(tokio::sync::Mutex::new(MemoryManager::new(
        l0.clone(),
        l2.clone(),
        proj.clone(),
        CoreConfig::default(),
    )));
    let templates = Arc::new(
        TemplateEngine::new(Path::new("src/templates/templates"))
            .unwrap_or_else(|_| TemplateEngine::new(Path::new("/nonexistent")).unwrap()),
    );
    let gateway = Arc::new(UnifiedGateway::new(&test_gateway_settings()).unwrap());
    let skills = Arc::new(SkillRegistry::new());
    let runner = Arc::new(wild_agent_os_core::core::agent_runner::AgentRunner::new(
        gateway,
        skills.clone(),
        l2,
        l0,
        mm,
        templates.clone(),
        wild_agent_os_core::config::AgentSettings::default(),
    ));
    let event_bus = Arc::new(EventBus::new(100));

    SupervisorAgent::new(runner, templates, skills, event_bus, 10)
}

#[test]
fn test_sa_classify_complexity() {
    let sa = make_sa();
    assert_eq!(
        sa.analyze_task("Hello").task_complexity,
        TaskComplexity::Instant
    );
    assert_eq!(
        sa.analyze_task("What is the weather today?")
            .task_complexity,
        TaskComplexity::Simple
    );
    assert_eq!(
        sa.analyze_task("Build a web application with user authentication and database")
            .task_complexity,
        TaskComplexity::Recursive
    );
    assert_eq!(
        sa.analyze_task("Fix this critical bug").task_complexity,
        TaskComplexity::Emergency
    );
}

#[test]
fn test_sa_execution_plan_simple() {
    let sa = make_sa();
    let plan = sa.analyze_task("Hello");
    assert_eq!(plan.agent_sequence.len(), 1);
    assert_eq!(plan.agent_sequence[0], AgentRole::Do);
}

#[test]
fn test_sa_execution_plan_standard() {
    let sa = make_sa();
    let plan = sa.analyze_task(
        "Build a web application with user authentication and a PostgreSQL database backend",
    );
    assert_eq!(plan.agent_sequence.len(), 4);
    assert_eq!(plan.agent_sequence[0], AgentRole::Plan);
    assert_eq!(plan.agent_sequence[3], AgentRole::Act);
}

#[test]
fn test_sa_execution_plan_emergency() {
    let sa = make_sa();
    let plan = sa.analyze_task("Fix critical security vulnerability now");
    assert_eq!(plan.agent_sequence.len(), 3);
    assert_eq!(plan.agent_sequence[0], AgentRole::Do);
}

#[test]
fn test_agent_instance_lifecycle() {
    let agent = AgentInstance::new("agent_1".to_string(), AgentRole::Do);
    assert_eq!(agent.status, AgentStatus::Idle);
}
