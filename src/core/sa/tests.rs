use super::*;

#[cfg(test)]
// Nested `tests` module inside `tests.rs` keeps existing test paths stable and avoids
// re-indenting the whole file; no other module shares this name.
#[allow(clippy::module_inception)]
mod tests {
    use super::*;
    use crate::core::agent_instance::AgentRole;
    use crate::core::agent_runner::AgentRunner;
    use crate::core::event_bus::EventBus;
    use crate::gateway::unified_gateway::UnifiedGateway;
    use crate::memory::memory_manager::MemoryManager;
    use crate::templates::template_engine::TemplateEngine;
    use crate::tools::skill_registry::SkillRegistry;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[derive(serde::Deserialize)]
    struct GoldenAgentCases {
        cases: Vec<GoldenAgentCase>,
    }

    #[derive(serde::Deserialize)]
    struct GoldenAgentCase {
        id: String,
        input: String,
        expected_complexity: String,
        expected_roles: Vec<String>,
    }

    fn make_sa_with_tempdir() -> (SupervisorAgent, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let l0 = Arc::new(
            crate::memory::l0_store::L0Store::new(dir.path().join("l0").to_string_lossy().as_ref())
                .unwrap(),
        );
        let l2 = Arc::new(crate::memory::l2_blackboard::Blackboard::new().unwrap());
        let proj = Arc::new(crate::memory::l3_projection::ProjectionEngine::new(
            l2.clone(),
            500,
        ));
        let mm = Arc::new(tokio::sync::Mutex::new(MemoryManager::new(
            l0.clone(),
            l2.clone(),
            proj.clone(),
            crate::CoreConfig::default(),
        )));
        let tmpl = Arc::new(TemplateEngine::new(std::path::Path::new("/nonexistent")).unwrap());
        let settings = crate::config::settings::GatewaySettings {
            base_url: "http://localhost:3000".to_string(),
            api_key: "sk-test".to_string(),
            default_model: "deepseek-v4-flash".to_string(),
            timeout_seconds: 30,
            max_retries: 3,
            retry_base_ms: 500,
            use_responses_api: false,
            model_mapping: HashMap::new(),
        };
        let gateway = Arc::new(UnifiedGateway::new(&settings).unwrap());
        let skills = Arc::new(SkillRegistry::new());
        let agent_settings = crate::config::settings::AgentSettings::default();
        let runner = Arc::new(AgentRunner::new(
            gateway,
            skills.clone(),
            l2.clone(),
            l0,
            mm,
            tmpl.clone(),
            agent_settings,
        ));
        let sa = SupervisorAgent::new(runner, tmpl, skills, Arc::new(EventBus::new(100)), 10)
            .with_memory(Some(l2), None, None);
        (sa, dir)
    }

    /// An owner of task B cannot satisfy task A's approval by reusing its
    /// request id. The wait accepts only a result whose task_iri matches.
    #[tokio::test]
    async fn approval_from_another_task_owner_is_ignored() {
        let (sa, _dir) = make_sa_with_tempdir();
        let bus = sa.event_bus.clone();
        let mut watch = bus.subscribe();
        let task_a = "iri://task/a";
        let task_b = "iri://task/b";
        let pending = tokio::spawn(async move {
            sa.request_human_approval_general("approve the budget?", "node-1", task_a)
                .await
        });

        let request_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let event = watch.recv().await.expect("event bus open");
                if event.event_type == "HUMAN_APPROVAL_REQUIRED" && event.task_iri == task_a {
                    let payload: serde_json::Value =
                        serde_json::from_str(&event.payload).expect("approval request payload");
                    return payload["request_id"]
                        .as_str()
                        .expect("request_id")
                        .to_string();
                }
            }
        })
        .await
        .expect("approval request");

        bus.emit(
            task_b,
            "HUMAN_APPROVAL_RESULT",
            "external:http:owner-b",
            &serde_json::json!({
                "request_id": request_id,
                "approved": true,
                "comment": "owner of task B",
            })
            .to_string(),
        )
        .await;
        bus.emit(
            task_a,
            "HUMAN_APPROVAL_RESULT",
            "external:http:owner-a",
            &serde_json::json!({
                "request_id": request_id,
                "approved": false,
                "comment": "owner of task A",
            })
            .to_string(),
        )
        .await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
            .await
            .expect("approval wait")
            .expect("join")
            .expect("approval result");
        assert!(!result.approved, "task B must not approve task A");
        assert_eq!(result.comment.as_deref(), Some("owner of task A"));
    }

    #[test]
    fn test_classify_simple() {
        let (sa, _dir) = make_sa_with_tempdir();
        assert_eq!(
            sa.classify_complexity("What is the weather?"),
            TaskComplexity::Simple
        );
        assert_eq!(
            sa.classify_complexity("Fix this bug in the code"),
            TaskComplexity::Emergency
        );
        assert_eq!(
            sa.classify_complexity("Build a web application with user authentication and database"),
            TaskComplexity::Recursive
        );
    }

    #[test]
    fn test_execution_plan_simple() {
        let (sa, _dir) = make_sa_with_tempdir();
        let plan = sa.analyze_task("Hello");
        assert_eq!(plan.agent_sequence.len(), 1);
        assert_eq!(plan.agent_sequence[0], AgentRole::Do);
    }

    #[test]
    fn golden_agent_plans_follow_heuristic_contract() {
        let cases: GoldenAgentCases =
            serde_json::from_str(include_str!("../../../evals/golden/agent-plans.json"))
                .expect("golden agent fixture must be valid JSON");
        let (sa, _dir) = make_sa_with_tempdir();

        for case in cases.cases {
            let plan = sa.analyze_task(&case.input);
            assert_eq!(
                format!("{:?}", plan.task_complexity).to_lowercase(),
                case.expected_complexity,
                "case {} classified unexpectedly",
                case.id
            );
            let roles: Vec<String> = plan
                .agent_sequence
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(
                roles, case.expected_roles,
                "case {} planned unexpectedly",
                case.id
            );
        }
    }

    #[test]
    fn test_execution_plan_emergency() {
        let (sa, _dir) = make_sa_with_tempdir();
        let plan = sa.analyze_task("Fix critical security vulnerability");
        assert_eq!(plan.agent_sequence.len(), 3);
        assert_eq!(plan.agent_sequence[0], AgentRole::Do);
        assert!(plan.agent_sequence.contains(&AgentRole::Act));
    }

    #[test]
    fn test_cleanup_expired_cycles() {
        let (mut sa, _dir) = make_sa_with_tempdir();
        sa.active_cycles.insert(
            "old_cycle".to_string(),
            CycleState {
                cycle_id: "old_cycle".to_string(),
                task_iri: "iri://task/1".to_string(),
                phase: CyclePhase::Completed,
                iteration: 1,
                max_iterations: 10,
                started_at: chrono::Utc::now() - chrono::Duration::hours(2),
                phase_history: vec![],
                task_completed: true,
                experience_hints: vec![],
            },
        );
        sa.cleanup_expired_cycles(3600);
        assert!(sa.active_cycles.is_empty());
    }

    #[test]
    fn test_verify_aa_needs_execution_parses_verdict() {
        use crate::core::agent_runner::{TaskResult, TaskVerdict};

        fn result_with(summary: &str, verdict: Option<TaskVerdict>) -> TaskResult {
            TaskResult {
                task_iri: "iri://task/verify".to_string(),
                status: "success".to_string(),
                verdict,
                summary: summary.to_string(),
                output: None,
                jsonld_output: None,
                artifacts: vec![],
                errors: vec![],
                turn_count: 1,
                tool_call_count: 0,
                five_w2h_updates: None,
                tracked_actions: Vec::new(),
                archive_iri: None,
            }
        }

        // Verify-first AA concluded full execution is needed (the regression:
        // the agent_runner's finish action hardcodes status "success", so this
        // verdict must be recovered from the summary to trigger fallback_steps).
        assert!(
            verify_aa_needs_execution(&result_with(
                "Final verdict: needs full execution. Existing workspace has no calculator.py — deliverable is absent.",
                None
            )),
            "explicit needs-execution verdict must require execution"
        );
        assert!(
            verify_aa_needs_execution(&result_with(
                "The existing code does NOT satisfy the task requirements. Missing: calculator.py, test_calculator.py.",
                None
            )),
            "missing deliverables must require execution"
        );
        assert!(
            verify_aa_needs_execution(&result_with("", None)),
            "empty verdict must conservatively require execution"
        );
        assert!(
            !verify_aa_needs_execution(&result_with(
                "Final verdict: task already done. Existing calculator.py passes all test cases.",
                None
            )),
            "task-already-done verdict must not require execution"
        );
        assert!(
            !verify_aa_needs_execution(&result_with(
                "VERIFIED-PASS: existing code satisfies the task requirements.",
                None
            )),
            "VERIFIED-PASS must not require execution"
        );
    }

    #[test]
    fn test_verify_aa_needs_execution_structured_verdict_priority() {
        use crate::core::agent_runner::{TaskResult, TaskVerdict};

        fn result_with(summary: &str, verdict: Option<TaskVerdict>) -> TaskResult {
            TaskResult {
                task_iri: "iri://task/verify".to_string(),
                status: "success".to_string(),
                verdict,
                summary: summary.to_string(),
                output: None,
                jsonld_output: None,
                artifacts: vec![],
                errors: vec![],
                turn_count: 1,
                tool_call_count: 0,
                five_w2h_updates: None,
                tracked_actions: Vec::new(),
                archive_iri: None,
            }
        }

        assert!(
            verify_aa_needs_execution(&result_with("", Some(TaskVerdict::Blocked))),
            "Blocked verdict must require execution"
        );
        assert!(
            verify_aa_needs_execution(&result_with("task already done", Some(TaskVerdict::Failed))),
            "Failed verdict must override a completion-looking summary"
        );
        assert!(
            verify_aa_needs_execution(&result_with("", Some(TaskVerdict::Timeout))),
            "Timeout verdict must require execution"
        );
        assert!(
            !verify_aa_needs_execution(&result_with(
                "VERIFIED-PASS: task already complete",
                Some(TaskVerdict::Success)
            )),
            "Success verdict + completion summary must not require execution"
        );
        assert!(
            verify_aa_needs_execution(&result_with(
                "deliverable is absent",
                Some(TaskVerdict::Success)
            )),
            "Success verdict + ambiguous summary must conservatively require execution"
        );
    }
}
