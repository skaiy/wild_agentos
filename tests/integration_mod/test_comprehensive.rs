use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use wild_agent_os_core::config::settings::LoggingSettings;
use wild_agent_os_core::config::GatewaySettings;
use wild_agent_os_core::core::agent_instance::AgentRole;
use wild_agent_os_core::core::event_bus::EventBus;
use wild_agent_os_core::core::sa::{SupervisorAgent, TaskComplexity};
use wild_agent_os_core::gateway::UnifiedGateway;
use wild_agent_os_core::memory::l0_store::L0Store;
use wild_agent_os_core::memory::l1_session::L1Session;
use wild_agent_os_core::memory::l2_blackboard::Blackboard;
use wild_agent_os_core::memory::l3_projection::ProjectionEngine;
use wild_agent_os_core::memory::memory_manager::MemoryManager;
use wild_agent_os_core::templates::template_engine::TemplateEngine;
use wild_agent_os_core::tools::skill_registry::SkillRegistry;
use wild_agent_os_core::utils::init_logging;
use wild_agent_os_core::CoreConfig;

static LOGGING_INITIALIZED: std::sync::Once = std::sync::Once::new();

fn init_test_logging() {
    LOGGING_INITIALIZED.call_once(|| {
        let logging_settings = LoggingSettings {
            level: "debug".to_string(),
            format: "text".to_string(),
            console_output: true,
            file_output: wild_agent_os_core::config::settings::FileOutputSettings {
                enabled: true,
                path: "./logs".to_string(),
                prefix: "test_agent_os".to_string(),
                rotation: "daily".to_string(),
                max_files: 10,
            },
            filters: vec![
                wild_agent_os_core::config::settings::LogFilter {
                    module: "wild_agent_os_core::core".to_string(),
                    level: "debug".to_string(),
                },
                wild_agent_os_core::config::settings::LogFilter {
                    module: "wild_agent_os_core::gateway".to_string(),
                    level: "debug".to_string(),
                },
            ],
            sensitive_fields: vec!["api_key".to_string(), "password".to_string()],
        };
        let _guard = init_logging(&logging_settings);
        std::mem::forget(_guard);
    });
}

fn test_gateway_settings() -> GatewaySettings {
    GatewaySettings {
        base_url: "http://localhost:3000".to_string(),
        api_key: "sk-test".to_string(),
        default_model: "deepseek-v4-flash".to_string(),
        timeout_seconds: 30,
        max_retries: 3,
        retry_base_ms: 500,
        use_responses_api: false,
        model_mapping: Default::default(),
    }
}

#[allow(dead_code)]
struct TestInfra {
    _l0_dir: tempfile::TempDir,
    l0: Arc<L0Store>,
    l2: Arc<Blackboard>,
    proj: Arc<ProjectionEngine>,
    mm: Arc<tokio::sync::Mutex<MemoryManager>>,
    skills: Arc<SkillRegistry>,
    templates: Arc<TemplateEngine>,
    gateway: Arc<UnifiedGateway>,
    runner: Arc<wild_agent_os_core::core::agent_runner::AgentRunner>,
}

fn setup_infra() -> TestInfra {
    let l0_dir = tempfile::tempdir().unwrap();
    let l0_path = l0_dir.path().join("l0");
    let l0 = super::writable_l0(l0_path);
    let l2 = Arc::new(Blackboard::new().unwrap());
    let proj = Arc::new(ProjectionEngine::new(l2.clone(), 500));
    let mm = Arc::new(tokio::sync::Mutex::new(MemoryManager::new(
        l0.clone(),
        l2.clone(),
        proj.clone(),
        CoreConfig::default(),
    )));
    let skills = Arc::new(SkillRegistry::new());
    let tmpl = Arc::new(
        TemplateEngine::new(Path::new("src/templates/templates"))
            .unwrap_or_else(|_| TemplateEngine::new(Path::new("/nonexistent")).unwrap()),
    );
    let gateway = Arc::new(UnifiedGateway::new(&test_gateway_settings()).unwrap());
    let runner = Arc::new(wild_agent_os_core::core::agent_runner::AgentRunner::new(
        gateway.clone(),
        skills.clone(),
        l2.clone(),
        l0.clone(),
        mm.clone(),
        tmpl.clone(),
        wild_agent_os_core::config::AgentSettings::default(),
    ));
    TestInfra {
        _l0_dir: l0_dir,
        l0,
        l2,
        proj,
        mm,
        skills,
        templates: tmpl,
        gateway,
        runner,
    }
}

#[test]
fn test_sa_complexity_classification() {
    init_test_logging();

    let infra = setup_infra();
    let sa = SupervisorAgent::new(
        infra.runner.clone(),
        infra.templates.clone(),
        infra.skills.clone(),
        Arc::new(EventBus::new(100)),
        10,
    );

    let plan = sa.analyze_task("What is the capital of France?");
    assert_eq!(plan.task_complexity, TaskComplexity::Simple);
    assert_eq!(plan.agent_sequence.len(), 1);
    assert_eq!(plan.agent_sequence[0], AgentRole::Do);

    let plan = sa.analyze_task(
        "Build a web application with user authentication and a PostgreSQL database backend",
    );
    assert_eq!(plan.task_complexity, TaskComplexity::Recursive);
    assert_eq!(plan.agent_sequence.len(), 4);

    let plan = sa.analyze_task("Fix critical security vulnerability in the authentication module");
    assert_eq!(plan.task_complexity, TaskComplexity::Emergency);
    assert_eq!(plan.agent_sequence.len(), 3);

    let plan = sa.analyze_task("Research and compare different database solutions for e-commerce");
    assert_eq!(plan.task_complexity, TaskComplexity::Exploratory);
    assert!(!plan.parallel_groups.is_empty());
}

#[test]
fn test_memory_full_pipeline() {
    let infra = setup_infra();

    let mut l1 = L1Session::new("agent_1", "DA", "iri://task/test_mem");
    l1.add_summary("assistant", "Found the bug in auth.rs", None);
    l1.add_summary("assistant", "Applied the fix and verified", None);
    assert_eq!(l1.turn_count(), 2);

    let config = wild_agent_os_core::CoreConfig::default();
    infra
        .l2
        .write_node(
            "iri://task/test_mem/node_1",
            r#"{"@id":"iri://task/test_mem/node_1","@type":"TestNode","summary":"test"}"#,
            &config,
        )
        .unwrap();
    let node = infra
        .l2
        .read_node("iri://task/test_mem/node_1")
        .unwrap()
        .unwrap();
    assert_eq!(node.node_type.as_ref().unwrap(), "TestNode");

    infra.l2.flush_oxigraph();
    let sparql_results = infra
        .l2
        .query("SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 5")
        .unwrap();
    assert!(
        !sparql_results.is_empty(),
        "SPARQL should return results after write_node"
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let projection = infra
            .proj
            .project("iri://task/test_mem", "reference_only", HashMap::new())
            .await
            .unwrap();
        assert!(projection.contains("task_iri"));
    });
}

#[test]
fn test_l0_store_crud() {
    let infra = setup_infra();

    infra
        .l0
        .store("iri://test/doc1", "Rust is a systems programming language")
        .unwrap();
    infra
        .l0
        .store("iri://test/doc2", "Python is an interpreted language")
        .unwrap();

    let entry = infra.l0.retrieve("iri://test/doc1").unwrap();
    assert!(entry.is_some());

    let results = infra.l0.search("Rust", 10).unwrap();
    assert!(!results.is_empty());

    assert!(infra.l0.count().unwrap_or(0) >= 2);
}
