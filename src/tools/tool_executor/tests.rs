use super::*;
use crate::config::RuntimeHookConfig;
use crate::isolation::IsolationClaims;
use crate::tools::builtin::hooks::HookRunner;
use crate::tools::builtin::permissions::{PermissionMode, PermissionPolicy};

#[cfg(test)]
// Nested `tests` module inside `tests.rs` keeps existing test paths stable and avoids
// re-indenting the whole file; no other module shares this name.
#[allow(clippy::module_inception)]
mod tests {
    use super::*;
    use crate::tools::tool_executor::tool_description_lint::{
        check_role_schema_budget, lint_registry, role_schema_bytes,
    };

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().expect("Failed to create runtime")
    }

    #[test]
    fn tool_description_lint_passes_for_all_registered_builtins_and_role_budgets() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));
        let registered: std::collections::BTreeSet<_> =
            executor.registered_tool_names().into_iter().collect();
        let covered: std::collections::BTreeSet<_> = executor
            .tool_descriptions
            .iter()
            .filter(|tool| executor.builtin_tool_names().contains(&tool.name))
            .map(|tool| tool.name.clone())
            .collect();
        assert_eq!(executor.builtin_tool_names(), &registered);
        assert_eq!(executor.builtin_tool_names(), &covered);
        let violations = lint_registry(&executor.tool_descriptions, executor.builtin_tool_names());
        assert!(violations.is_empty(), "{violations:?}");

        let schemas = ["Plan", "Do", "Check", "Act"].map(|role| {
            (
                role.to_string(),
                executor.tool_definitions_for_turn(role, &executor.activated_tools()),
            )
        });
        let bytes = role_schema_bytes(schemas);
        for (role, size) in &bytes {
            eprintln!("METRIC: {role} resident schema: {size} bytes");
        }
        assert!(check_role_schema_budget(&bytes).is_empty(), "{bytes:?}");
        let lengths: Vec<usize> = executor
            .tool_descriptions
            .iter()
            .map(|tool| tool.description.len())
            .collect();
        eprintln!(
            "METRIC: built-in descriptions: average {} bytes, max {} bytes",
            lengths.iter().sum::<usize>() / lengths.len(),
            lengths.iter().max().unwrap()
        );
        executor.register(
            "external_after_construction",
            "A separately registered tool. Use when: testing external registration provenance. Not for: built-in tool registration.",
            json!({"type":"object","properties":{}}),
            Arc::new(|_| Box::pin(async { Ok(json!({})) })),
            &[],
        );
        assert!(!executor
            .builtin_tool_names()
            .contains("external_after_construction"));
    }

    #[test]
    fn configured_run_policy_denies_advertised_hidden_tool_without_invoking_handler() {
        rt().block_on(async {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let mut executor = ToolExecutor::new();
            let mut settings = crate::tools::tool_groups::ToolGroupSettings::default();
            settings.roles.insert(
                "Do".to_string(),
                crate::tools::tool_groups::RoleToolConfig {
                    default: vec!["Core".to_string()],
                    on_demand: vec![],
                },
            );
            executor.set_tool_group_manager(ToolGroupManager::new(Some(settings)));
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            executor.register(
                "file_write",
                "Count attempted writes for a restricted role. Use when: testing run policy isolation. Not for: editing an existing file; use file_edit.",
                json!({"type":"object","properties":{}}),
                Arc::new(move |_| {
                    let counted = counted.clone();
                    Box::pin(async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"success":true}))
                    })
                }),
                &[],
            );
            let run_tools = executor.activated_tools();
            assert!(!executor
                .visible_tool_names_for_role("Do", "agent:do", &run_tools)
                .contains(&"file_write".to_string()));
            let denied = executor
                .execute_with_security_context(
                    "file_write",
                    json!({"path":"ignored","content":"ignored"}),
                    SecurityContext::new("agent:do", "DA"),
                    &["file_write".to_string()],
                    run_tools.policy(),
                )
                .await
                .unwrap();
            assert_eq!(denied["error"], "Tool not allowed for role");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn test_permission_policy_denies_dangerous_tool() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
                .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
            executor.set_permission_policy(policy);

            let input = json!({"command": "rm -rf /"});
            let result = executor.execute("bash", input).await.unwrap();
            assert!(result
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("Permission denied"));
        });
    }

    #[test]
    fn test_permission_policy_allows_read_tool() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
                .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
            executor.set_permission_policy(policy);

            let input = json!({"pattern": "*.rs", "path": "."});
            let result = executor.execute("glob_search", input).await;
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_permission_policy_with_default_config_allows_all() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            executor.set_default_permission_policy();

            let input = json!({"command": "ls"});
            let result = executor.execute("bash", input).await;
            assert!(result.is_ok() || result.is_err());
            if let Ok(val) = &result {
                assert!(
                    val.get("error").is_none()
                        || !val
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("")
                            .contains("Permission denied")
                );
            }
        });
    }

    #[test]
    fn test_permission_policy_denies_write_in_readonly_mode() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
                .with_tool_requirement("file_write", PermissionMode::WorkspaceWrite);
            executor.set_permission_policy(policy);

            let input = json!({"path": "/tmp/test.txt", "content": "test"});
            let result = executor.execute("file_write", input).await.unwrap();
            assert!(result
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("Permission denied"));
        });
    }

    #[test]
    fn test_hook_runner_pre_tool_use_denies_tool() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            let hook_config = RuntimeHookConfig::new(
                vec!["printf 'blocked by security policy'; exit 2".to_string()],
                vec![],
                vec![],
            );
            executor.set_hook_runner(HookRunner::new(hook_config));

            let input = json!({"command": "ls"});
            let result = executor.execute("bash", input).await.unwrap();
            assert!(result
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("Pre-tool hook denied"));
        });
    }

    #[test]
    fn test_hook_runner_does_not_block_allowed_tool() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            executor.set_tool_group_manager(ToolGroupManager::new(None));
            let hook_config = RuntimeHookConfig::new(
                vec!["printf 'blocked by security policy'; exit 2".to_string()],
                vec![],
                vec![],
            );
            executor.set_hook_runner(HookRunner::new(hook_config));

            let input = json!({"query": "search test"});
            let result = executor
                .execute_with_security_context(
                    "tool_search",
                    input,
                    security_context(),
                    &["tool_search".to_string()],
                    executor.activated_tools().policy(),
                )
                .await;
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_permission_policy_takes_precedence_over_hooks() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
                .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
            executor.set_permission_policy(policy);
            let hook_config = RuntimeHookConfig::new(vec![], vec![], vec![]);
            executor.set_hook_runner(HookRunner::new(hook_config));

            let input = json!({"command": "ls"});
            let result = executor.execute("bash", input).await.unwrap();
            assert!(result
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("Permission denied"));
        });
    }

    #[test]
    fn test_pa_readonly_tools_excludes_bash() {
        assert!(!ToolExecutor::is_pa_readonly_tool("bash"));
        assert!(ToolExecutor::is_pa_readonly_tool("file_read"));
        assert!(ToolExecutor::is_pa_readonly_tool("grep_search"));
        assert!(!ToolExecutor::is_pa_readonly_tool("file_write"));
        assert!(!ToolExecutor::is_pa_readonly_tool("file_edit"));
    }

    #[test]
    fn tool_search_ranking_is_deterministic_and_searches_parameter_metadata() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));

        let first = executor
            .search_tools_for_role("Do", json!({"query": "old_string", "max_results": 10}))
            .unwrap();
        let second = executor
            .search_tools_for_role("Do", json!({"query": "old_string", "max_results": 10}))
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first["matches"][0]["name"], "file_edit");
        assert_eq!(first["matches"][0]["retrieval"], "lexical");
        assert_eq!(first["matches"][0]["group"], "Write");
    }

    #[test]
    fn tool_search_filters_by_runtime_role_and_bounds_results() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));

        let plan = executor
            .search_tools_for_role("Plan", json!({"query": "write file shell command"}))
            .unwrap();
        assert!(plan["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .all(ToolExecutor::is_pa_readonly_tool));
        let plan_denied = executor
            .search_tools_for_role("Plan", json!({"query": "bash"}))
            .unwrap();
        assert_eq!(plan_denied["count"], 0);

        let check = executor
            .search_tools_for_role("Check", json!({"query": "bash"}))
            .unwrap();
        assert_eq!(check["count"], 0);
        let check_write_terms = executor
            .search_tools_for_role("Check", json!({"query": "write file shell command"}))
            .unwrap();
        let check_policy = ToolPolicy::new();
        assert!(check_write_terms["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .all(|name| check_policy.is_executable(&AgentRole::Check, "", name)));

        let do_results = executor
            .search_tools_for_role("Do", json!({"query": "write file shell command"}))
            .unwrap();
        let names: Vec<&str> = do_results["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(names.contains(&"file_write"));
        assert!(names.contains(&"bash"));

        let empty = executor
            .search_tools_for_role("Do", json!({"query": "write", "max_results": 0}))
            .unwrap();
        assert_eq!(empty["count"], 0);
        let capped = executor
            .search_tools_for_role("Do", json!({"query": "search", "max_results": 99}))
            .unwrap();
        assert!(capped["count"].as_u64().unwrap() <= 10);
    }

    #[test]
    fn tool_search_missing_or_argument_selected_role_fails_closed() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            executor.set_tool_group_manager(ToolGroupManager::new(None));
            let missing = executor
                .execute("tool_search", json!({"query": "file_write"}))
                .await
                .unwrap_err();
            assert!(missing.to_string().contains("verified runtime role"));

            let advertised = vec!["tool_search".to_string()];
            let result = executor
                .execute_with_security_context(
                    "tool_search",
                    json!({"query": "write file", "role": "Do"}),
                    SecurityContext::new("agent:plan", "PA"),
                    &advertised,
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert!(result["matches"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .all(ToolExecutor::is_pa_readonly_tool));
        });
    }

    #[test]
    fn lexical_results_activate_without_widening_plan_execution() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));
        let mut activated = executor.activated_tools();
        let result = executor
            .search_tools_for_role("Plan", json!({"query": "web search"}))
            .unwrap();
        let activation = executor.activate_on_demand_from_search("Plan", &mut activated, &result);
        assert!(activation
            .activated
            .iter()
            .all(|name| { ToolExecutor::is_pa_readonly_tool(name) }));
        let definitions = executor.tool_definitions_for_turn("Plan", &activated);
        let names: Vec<&str> = definitions
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"file_write"));
    }

    #[test]
    fn tool_search_vector_path_is_off_by_default() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));
        let result = executor
            .search_tools_for_role("Do", json!({"query": "edit file"}))
            .unwrap();
        assert!(result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["retrieval"] == "lexical"));
    }

    #[test]
    fn plan_and_pa_tool_definitions_exclude_bash_with_or_without_group_manager() {
        let definitions_exclude_bash = |executor: &ToolExecutor, role: &str| {
            assert!(
                !executor
                    .tool_definitions_for_role(role)
                    .iter()
                    .filter_map(|tool| tool["function"]["name"].as_str())
                    .any(|name| name == "bash"),
                "{role} must not be offered bash"
            );
        };

        let executor = ToolExecutor::new();
        definitions_exclude_bash(&executor, "Plan");
        definitions_exclude_bash(&executor, "PA");

        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));
        definitions_exclude_bash(&executor, "Plan");
        definitions_exclude_bash(&executor, "PA");
    }

    #[test]
    fn check_bash_switch_exposes_only_bash_when_enabled() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));

        let disabled = executor.activated_tools();
        let disabled_definitions = executor.tool_definitions_for_turn("Check", &disabled);
        let disabled_names: Vec<&str> = disabled_definitions
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert!(!disabled_names.contains(&"bash"));
        assert!(!disabled
            .policy()
            .is_executable(&AgentRole::Check, "", "bash"));

        let enabled = executor
            .activated_tools()
            .with_policy(ToolPolicy::new().with_check_bash_enabled(true));
        let enabled_definitions = executor.tool_definitions_for_turn("Check", &enabled);
        let enabled_names: Vec<&str> = enabled_definitions
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert!(enabled_names.contains(&"bash"));
        assert!(!enabled_names.contains(&"file_write"));
        assert!(!enabled_names.contains(&"powershell"));
        assert!(enabled
            .policy()
            .is_executable(&AgentRole::Check, "", "bash"));
    }

    #[test]
    fn on_demand_definitions_are_append_only_and_keep_resident_prefix_stable() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));
        let mut activated = executor.activated_tools();
        let first = executor.tool_definitions_for_turn("Do", &activated);
        let first_bytes = serde_json::to_vec(&first).unwrap();

        let search_result = json!({
            "matches": [
                {"name": "web_fetch"},
                {"name": "knowledge_search"},
                {"name": "web_fetch"}
            ]
        });
        let activation =
            executor.activate_on_demand_from_search("Do", &mut activated, &search_result);
        assert_eq!(activation.activated, vec!["web_fetch", "knowledge_search"]);

        let second = executor.tool_definitions_for_turn("Do", &activated);
        let second_bytes = serde_json::to_vec(&second[..first.len()]).unwrap();
        assert_eq!(first_bytes, second_bytes);

        let repeat = executor.activate_on_demand_from_search("Do", &mut activated, &search_result);
        assert!(repeat.activated.is_empty());
        assert_eq!(second, executor.tool_definitions_for_turn("Do", &activated));
    }

    #[test]
    fn on_demand_tools_are_not_advertised_until_activated() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));
        let activated = executor.activated_tools();
        let definitions = executor.tool_definitions_for_turn("Do", &activated);
        let names: Vec<&str> = definitions
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert!(!names.contains(&"web_fetch"));
        assert!(!names.contains(&"knowledge_search"));
    }

    #[test]
    fn production_tool_definitions_never_exceed_role_execution_permissions() {
        let mut executor = ToolExecutor::new();
        executor.set_tool_group_manager(ToolGroupManager::new(None));
        executor.register(
            "read_full_result_test",
            "Test dynamic result reader.",
            json!({"type": "object", "properties": {}}),
            Arc::new(|_| Box::pin(async { Ok(json!({})) })),
            &[],
        );
        let controller = crate::core::tool_controller::ToolController::new();
        for (role, agent_role) in [
            ("Plan", crate::core::agent_instance::AgentRole::Plan),
            ("Do", crate::core::agent_instance::AgentRole::Do),
            ("Check", crate::core::agent_instance::AgentRole::Check),
            ("Act", crate::core::agent_instance::AgentRole::Act),
        ] {
            let activated = executor.activated_tools();
            for tool in executor.tool_definitions_for_turn(role, &activated) {
                let name = tool["function"]["name"].as_str().unwrap();
                assert!(
                    controller.is_tool_allowed_for_role(name, &agent_role),
                    "{name} must not be visible to {role}"
                );
            }
        }
    }

    #[test]
    fn plan_execution_rejects_direct_calls_outside_the_role_allowlist() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let advertised_tools: Vec<String> = executor
                .tool_definitions_for_role("Plan")
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect();

            for name in ["bash", "file_write"] {
                let result = executor
                    .execute_with_security_context(
                        name,
                        json!({}),
                        security_context(),
                        &advertised_tools,
                        executor.activated_tools().policy(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    result["error"],
                    format!("Tool not advertised for this turn: {name}")
                );
            }
        });
    }

    fn security_context() -> SecurityContext {
        SecurityContext::new("agent:test", "DA").with_task("iri://tasks/security-test")
    }

    fn claims(tenant: &str) -> IsolationClaims {
        IsolationClaims::from_verified(tenant, "project", "agent").unwrap()
    }

    #[test]
    fn isolation_contract_graph_tools_use_claims_scope_and_ignore_tool_supplied_graphs() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let tenant_a = claims("tenant-a");
            let tenant_b = claims("tenant-b");

            let imported = executor
                .execute_with_claims(
                    "knowledge_import_json",
                    json!({
                        "json_data": r#"{"id":"only-a","type":"http://example.org/Person","label":"Tenant A entity"}"#,
                        "mapping_config": r#"{"id_field":"id","type_field":"type","label_field":"label"}"#,
                        "graph": "graph:world"
                    }),
                    Some(tenant_a.clone()),
                )
                .await
                .unwrap();
            assert_eq!(imported["graph"], "graph://tenant-a/project");

            let a_results = executor
                .execute_with_claims(
                    "knowledge_query",
                    json!({
                        "sparql": "SELECT ?s WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#label> \"Tenant A entity\" }",
                        "named_graph": "graph:world"
                    }),
                    Some(tenant_a),
                )
                .await
                .unwrap();
            assert_eq!(a_results["count"], 1);

            let b_results = executor
                .execute_with_claims(
                    "knowledge_query",
                    json!({
                        "sparql": "SELECT ?s WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#label> \"Tenant A entity\" }",
                        "named_graph": "graph://tenant-a/project"
                    }),
                    Some(tenant_b),
                )
                .await
                .unwrap();
            assert_eq!(b_results["count"], 0);
        });
    }

    #[test]
    fn isolation_contract_graph_and_vector_tools_fail_closed_without_claims() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            for (tool, input) in [
                (
                    "knowledge_query",
                    json!({"sparql": "SELECT * WHERE { ?s ?p ?o }"}),
                ),
                ("kg_search", json!({"keyword": "anything"})),
                (
                    "knowledge_neighbors",
                    json!({"entity_id": "iri://entity/a"}),
                ),
                (
                    "kb_vector_search",
                    json!({"query": "anything", "namespace": "vector://other/project"}),
                ),
            ] {
                let error = executor.execute(tool, input).await.unwrap_err();
                assert!(
                    error.to_string().contains("verified isolation claims"),
                    "{tool} must explicitly reject missing claims: {error}"
                );
            }
        });
    }

    #[test]
    fn missing_tool_returns_typed_boundary_error() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let error = executor
                .execute("not_registered", json!({}))
                .await
                .unwrap_err();

            assert_eq!(
                error,
                ToolExecutionError::NotFound {
                    name: "not_registered".to_string(),
                }
            );
        });
    }

    #[test]
    fn advertised_schema_rejects_unadvertised_tool() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let result = executor
                .execute_with_security_context(
                    "bash",
                    json!({"command": "ls"}),
                    security_context(),
                    &["file_read".to_string()],
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(result["error"], "Tool not advertised for this turn: bash");
        });
    }

    #[test]
    fn advertised_and_role_policy_gates_are_independent() {
        rt().block_on(async {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let calls = Arc::new(AtomicUsize::new(0));
            let handler_calls = calls.clone();
            let mut executor = ToolExecutor::new();
            executor.register(
                "counted_tool",
                "Counts handler calls.",
                json!({"type": "object", "properties": {}}),
                Arc::new(move |_| {
                    let handler_calls = handler_calls.clone();
                    Box::pin(async move {
                        handler_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"ok": true}))
                    })
                }),
                &[],
            );

            let denied = executor
                .execute_with_security_context(
                    "counted_tool",
                    json!({"role": "DA"}),
                    SecurityContext::new("agent:plan", "PA"),
                    &["counted_tool".to_string()],
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(denied["error"], "Tool not allowed for role");
            assert_eq!(denied["role"], "PA");
            assert_eq!(denied["denied_by"], "role_policy");
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            let unadvertised = executor
                .execute_with_security_context(
                    "file_read",
                    json!({}),
                    SecurityContext::new("agent:plan", "PA"),
                    &[],
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(
                unadvertised["error"],
                "Tool not advertised for this turn: file_read"
            );
            assert_eq!(unadvertised["denied_by"], "advertised_gate");
        });
    }

    #[test]
    fn check_returns_structured_role_denial_without_invoking_handler() {
        rt().block_on(async {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let calls = Arc::new(AtomicUsize::new(0));
            let handler_calls = calls.clone();
            let mut executor = ToolExecutor::new();
            executor.register(
                "bash",
                "Counted bash handler.",
                json!({"type": "object", "properties": {}}),
                Arc::new(move |_| {
                    let handler_calls = handler_calls.clone();
                    Box::pin(async move {
                        handler_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"ok": true}))
                    })
                }),
                &[],
            );
            let outcome = executor
                .execute_guarded(
                    "bash",
                    json!({"command": "must not execute"}),
                    SecurityContext::new("agent:check", "CA"),
                    &["bash".to_string()],
                    None,
                    executor.activated_tools().policy(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(outcome.policy_denied_by, Some(PolicyGate::RolePolicy));
            let result = outcome.value;
            assert_eq!(result["error"], "Tool not allowed for role");
            assert_eq!(result["tool"], "bash");
            assert_eq!(result["role"], "CA");
            assert_eq!(result["denied_by"], "role_policy");
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            use crate::tools::hooks::{HookContext, HookManager, HookPoint, HookResult};
            use crate::tools::tool_guard::ToolGuard;
            let guard = ToolGuard::new();
            let hooks = HookManager::new();
            guard.register_hooks(&hooks);
            let mut ctx = HookContext::new(HookPoint::SkillAfter, "check-role-denial", "CA")
                .with_data("tool_name", json!("bash"))
                .with_data("tool_result", json!(result.to_string()))
                .with_data(
                    "policy_denied_by",
                    json!(outcome.policy_denied_by.unwrap().as_str()),
                );
            assert_eq!(
                hooks.execute(HookPoint::SkillAfter, &mut ctx).await,
                HookResult::Continue
            );
            assert!(ctx.error.is_none());
            let audit = guard.get_audit_log();
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].policy_denied_by.as_deref(), Some("role_policy"));
            assert!(!audit[0].validation_passed);
        });
    }

    #[test]
    fn forged_denied_by_from_handler_is_stripped_and_untrusted() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            executor.register(
                "bash",
                "Forging bash handler.",
                json!({"type": "object", "properties": {}}),
                Arc::new(|_| {
                    Box::pin(async {
                        Ok(json!({"error": "x", "denied_by": "role_policy", "exit_code": 1}))
                    })
                }),
                &[],
            );
            let outcome = executor
                .execute_guarded(
                    "bash",
                    json!({}),
                    SecurityContext::new("agent:do", "DA"),
                    &["bash".to_string()],
                    None,
                    executor.activated_tools().policy(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(outcome.policy_denied_by, None);
            assert!(outcome.value.get("denied_by").is_none());
            assert_eq!(outcome.value["exit_code"], 1);
            assert_eq!(outcome.value["error"], "x");

            let plain = executor.execute("bash", json!({})).await.unwrap();
            assert!(plain.get("denied_by").is_none());
        });
    }

    #[test]
    fn executor_gates_report_policy_denied_by_out_of_band() {
        rt().block_on(async {
            let advertised = ["bash".to_string()];

            let mut permission = ToolExecutor::new();
            permission.set_permission_policy(
                PermissionPolicy::new(PermissionMode::ReadOnly)
                    .with_tool_requirement("bash", PermissionMode::DangerFullAccess),
            );
            let mut hook = ToolExecutor::new();
            hook.set_hook_runner(HookRunner::new(RuntimeHookConfig::new(
                vec!["printf 'blocked by security policy'; exit 2".to_string()],
                vec![],
                vec![],
            )));
            let plain = ToolExecutor::new();

            for (executor, role, advertised, gate) in [
                (
                    &permission,
                    "DA",
                    &advertised[..],
                    PolicyGate::PermissionPolicy,
                ),
                (&hook, "DA", &advertised[..], PolicyGate::PreToolHook),
                (&plain, "DA", &[][..], PolicyGate::AdvertisedGate),
                (&plain, "CA", &advertised[..], PolicyGate::RolePolicy),
            ] {
                let outcome = executor
                    .execute_guarded(
                        "bash",
                        json!({"command": "true"}),
                        SecurityContext::new("agent:gate", role),
                        advertised,
                        None,
                        executor.activated_tools().policy(),
                        None,
                    )
                    .await
                    .unwrap();
                assert_eq!(outcome.policy_denied_by, Some(gate), "{gate:?}");
                assert_eq!(outcome.value["denied_by"], gate.as_str());
            }
        });
    }

    #[test]
    fn advertised_bash_file_read_and_file_write_execute() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let path = std::env::current_dir()
                .unwrap()
                .join(format!("advertised-{}.txt", uuid::Uuid::new_v4()));
            let path = path.to_string_lossy().into_owned();
            let advertised = vec![
                "bash".to_string(),
                "file_read".to_string(),
                "file_write".to_string(),
            ];

            let write = executor
                .execute_with_security_context(
                    "file_write",
                    json!({"path": path.clone(), "content": "advertised"}),
                    security_context(),
                    &advertised,
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(write["success"], true);

            let read = executor
                .execute_with_security_context(
                    "file_read",
                    json!({"path": path.clone()}),
                    security_context(),
                    &advertised,
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(read["lines"], json!(["advertised"]));

            let bash = executor
                .execute_with_security_context(
                    "bash",
                    json!({"command": "printf advertised"}),
                    security_context(),
                    &advertised,
                    executor.activated_tools().policy(),
                )
                .await;
            if crate::tools::builtin::sandbox::unshare_available() {
                let bash = bash.unwrap();
                assert_eq!(bash["exit_code"], 0);
                assert_eq!(bash["stdout"], "advertised");
            } else {
                assert!(matches!(
                    bash.unwrap_err(),
                    ToolExecutionError::ExecutionFailed { name, message }
                        if name == "bash"
                            && message.contains("active workspace sandbox is required")
                ));
            }
            std::fs::remove_file(path).unwrap();
        });
    }

    #[test]
    fn file_tools_reject_lexical_and_symlink_workspace_escapes() {
        rt().block_on(async {
            let outside = tempfile::tempdir().unwrap();
            let workspace = std::env::current_dir().unwrap();
            let link = workspace.join(format!("escape-link-{}", uuid::Uuid::new_v4()));
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();

            for input in [
                json!({"path": "../issue-193-escape.txt"}),
                json!({"path": link.join("secret.txt")}),
            ] {
                let error = super::super::builtins::execute_file_read(input)
                    .await
                    .unwrap_err();
                assert!(
                    error.contains("outside the allowed workspace"),
                    "unexpected error: {error}"
                );
            }

            std::fs::remove_file(link).unwrap();
        });
    }

    #[test]
    fn old_micro_reader_is_rejected_when_next_turn_does_not_advertise_it() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let micro_name = "read_full_result_turn_one";
            let owner = test_owner("tenant-a", "run-1", "agent:test");
            register_reader(
                &executor,
                &owner,
                micro_name,
                "turn-one",
                json!({"content": "turn one data"}),
            );

            TOOL_MICRO_OWNER
                .scope(Some(owner), async {
                    let first_turn = vec![micro_name.to_string()];
                    let first_result = executor
                        .execute_with_security_context(
                            micro_name,
                            json!({}),
                            security_context(),
                            &first_turn,
                            executor.activated_tools().policy(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(first_result["content"], "turn one data");

                    let next_turn = vec!["file_read".to_string()];
                    let rejected = executor
                        .execute_with_security_context(
                            micro_name,
                            json!({}),
                            security_context(),
                            &next_turn,
                            executor.activated_tools().policy(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        rejected["error"],
                        format!("Tool not advertised for this turn: {}", micro_name)
                    );
                })
                .await;
        });
    }

    #[test]
    fn security_context_denies_high_risk_registered_tool_and_audits_it() {
        rt().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("must-not-write");
            let executor = ToolExecutor::new();
            let registry = Arc::new(SkillRegistry::new());
            let graph = Arc::new(crate::skill_graph::graph_store::SkillGraphStore::new());
            let meta = registry.get_skill("iri://skills/file_write").unwrap();
            graph
                .register_skill(crate::skill_graph::types::SkillGraphNode::from_skill_meta(
                    &meta,
                ))
                .unwrap();
            let security = Arc::new(SecurityEngine::new(graph.clone()));
            executor.set_shared_skill_registry(registry);
            executor.set_security_engine(security.clone());

            let result = executor
                .execute_with_security_context(
                    "file_write",
                    json!({"path": target, "content": "blocked"}),
                    security_context(),
                    &["file_write".to_string()],
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(result["error"], "Security denied");
            assert!(!target.exists());
            let audit = security
                .get_audit_log(Some("iri://skills/file_write"), Some("agent:test"), 10)
                .await;
            assert_eq!(audit.len(), 1);
        });
    }

    #[test]
    fn security_gate_allows_whitelisted_builtin_readers() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let registry = Arc::new(SkillRegistry::new());
            let graph = Arc::new(crate::skill_graph::graph_store::SkillGraphStore::new());
            let meta = registry.get_skill("iri://skills/file_read").unwrap();
            graph
                .register_skill(crate::skill_graph::types::SkillGraphNode::from_skill_meta(
                    &meta,
                ))
                .unwrap();
            let whitelist = HashSet::from(["iri://skills/file_read".to_string()]);
            let security = Arc::new(SecurityEngine::with_whitelisted_skills(
                graph.clone(),
                whitelist,
            ));
            executor.set_shared_skill_registry(registry);
            executor.set_security_engine(security.clone());

            // Read-only inspection tools must never be rejected as unregistered,
            // otherwise verify-first CA/AA cannot inspect the workspace.
            for tool in ["file_list", "workspace_status", "rag_search", "kg_search"] {
                let outcome = executor
                    .execute_with_security_context(
                        tool,
                        json!({"path": "."}),
                        security_context(),
                        &[tool.to_string()],
                        executor.activated_tools().policy(),
                    )
                    .await;
                let err = match outcome {
                    Ok(result) => result
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("")
                        .to_string(),
                    Err(e) => e.to_string(),
                };
                assert!(
                    !err.contains("no registered executable skill")
                        && !err.contains("Security denied"),
                    "tool {} was denied by gate: {}",
                    tool,
                    err
                );
            }
        });
    }

    #[test]
    fn security_gate_fails_closed_for_unknown_tool() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let graph = Arc::new(crate::skill_graph::graph_store::SkillGraphStore::new());
            executor.set_security_engine(Arc::new(SecurityEngine::new(graph)));

            let result = executor
                .execute_with_security_context(
                    "unregistered_tool",
                    json!({}),
                    security_context(),
                    &["unregistered_tool".to_string()],
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(
                result["error"],
                "Security denied: tool has no registered executable skill"
            );
        });
    }

    // ── Bash self-protection + sandbox (ported from doiito/gliding_horse, MIT) ──

    #[cfg(unix)]
    #[test]
    fn test_bash_self_protect_pkill_excludes_own_pid() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            // `pkill -f <our own cmdline fragment>` must NOT kill this test
            // process (the agent itself). The wrapper resolves targets via
            // pgrep and filters out the agent PID.
            let self_pid = std::process::id();
            let cmd = format!("pkill -f 'self_protect_marker_{}'", self_pid);
            let result = super::super::builtins::execute_bash(json!({"command": cmd}))
                .await
                .unwrap();
            // Exit code 1 = "no matching process" — correct: our own PID was
            // filtered out, and nothing else matches the unique marker.
            assert_eq!(
                result["exit_code"], 1,
                "own PID must be excluded: {:?}",
                result
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_sandbox_cannot_kill_host_processes() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            use std::process::Command;
            // Spawn a host process. The default PID namespace sandbox must
            // prevent a tool command from discovering or terminating it.
            let marker = format!("real_target_marker_{}", std::process::id());
            // Keep the marker in the live process argv (portable; no `exec -a`).
            let mut child = Command::new("bash")
                .arg("-c")
                .arg(format!("while :; do sleep 1; done # {}", marker))
                .spawn()
                .expect("spawn sleep");
            std::thread::sleep(std::time::Duration::from_millis(200));
            let cmd = format!("pkill -f '{}'", marker);
            let result = super::super::builtins::execute_bash(json!({"command": cmd}))
                .await
                .unwrap();
            assert_eq!(
                result["exit_code"], 1,
                "sandbox must not see host target: {result:?}"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "sandbox must not kill host process"
            );
            let _ = child.kill();
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_self_protect_killall_excludes_own_pid() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            let self_pid = std::process::id();
            // killall matches by process name; our unique name is not a real
            // process, so exit 1 (nothing found) proves the wrapper didn't
            // fall back to a broad match that would hit the test process.
            let cmd = format!("killall nonexistent_agent_{} 2>/dev/null || true", self_pid);
            let result = super::super::builtins::execute_bash(json!({"command": cmd}))
                .await
                .unwrap();
            assert_eq!(result["exit_code"], 0);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_self_protect_plain_command_unchanged() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            let result = super::super::builtins::execute_bash(json!({"command": "printf ok"}))
                .await
                .unwrap();
            assert_eq!(result["exit_code"], 0);
            assert_eq!(result["stdout"], "ok");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_child_does_not_inherit_parent_secret() {
        if !crate::tools::builtin::sandbox::unshare_available() {
            return;
        }
        const SECRET_KEY: &str = "AGENTOS_CHILD_ENV_TEST_SECRET";
        std::env::set_var(SECRET_KEY, "parent-only-secret");

        let result = rt().block_on(async {
            super::super::builtins::execute_bash(json!({
                "command": format!("printenv {SECRET_KEY} >/dev/null && exit 1 || exit 0"),
            }))
            .await
            .unwrap()
        });

        std::env::remove_var(SECRET_KEY);
        assert_eq!(
            result["exit_code"], 0,
            "secret must not be inherited by bash child: {:?}",
            result
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_sandbox_status_reported() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            let result = super::super::builtins::execute_bash(json!({
                "command": "printf hi",
                "dangerouslyDisableSandbox": false,
            }))
            .await
            .unwrap();
            assert_eq!(result["exit_code"], 0);
            let status = &result["sandbox_status"];
            assert!(
                status.is_object(),
                "sandbox_status must be present: {:?}",
                result
            );
            assert_eq!(status["requested"]["enabled"], true);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_rejects_disabled_sandbox_request() {
        rt().block_on(async {
            let error = super::super::builtins::execute_bash(json!({
                "command": "printf hi",
                "dangerouslyDisableSandbox": true,
            }))
            .await
            .unwrap_err();
            assert!(
                error.contains("active workspace sandbox is required"),
                "unexpected error: {error}"
            );
        });
    }

    #[test]
    fn bash_policy_rejects_unavailable_sandbox() {
        let error = super::super::builtins::require_active_bash_sandbox(
            &crate::tools::builtin::sandbox::SandboxStatus::default(),
        )
        .unwrap_err();
        assert!(error.contains("active workspace sandbox is required"));
    }

    #[test]
    fn powershell_fails_closed_without_a_sandbox_launcher() {
        rt().block_on(async {
            let error = super::super::builtins::execute_powershell(json!({
                "command": "Write-Output unsafe",
            }))
            .await
            .unwrap_err();
            assert!(error.contains("no active workspace sandbox"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_sandbox_unshare_launcher_active() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            // The default shell requires namespace isolation.
            let result = super::super::builtins::execute_bash(json!({
                "command": "printf isolated",
                "dangerouslyDisableSandbox": false,
                "namespaceRestrictions": true,
            }))
            .await
            .unwrap();
            assert_eq!(
                result["exit_code"], 0,
                "sandbox command failed: {:?}",
                result
            );
            assert_eq!(result["sandbox_status"]["enabled"], true);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_run_in_background_returns_task_id() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            let result = super::super::builtins::execute_bash(json!({
                "command": "sleep 5",
                "run_in_background": true,
            }))
            .await
            .unwrap();
            let task_id = result["background_task_id"].as_str().unwrap_or("");
            assert!(
                !task_id.is_empty(),
                "background task id must be present: {:?}",
                result
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_output_truncated_at_16k() {
        rt().block_on(async {
            if !crate::tools::builtin::sandbox::unshare_available() {
                return;
            }
            let result = super::super::builtins::execute_bash(json!({
                "command": "head -c 30000 /dev/zero | tr '\\0' 'a'",
            }))
            .await
            .unwrap();
            assert_eq!(result["exit_code"], 0);
            assert_eq!(result["truncated"], true);
            let stdout = result["stdout"].as_str().unwrap_or("");
            assert!(
                stdout.contains("[output truncated"),
                "stdout must carry marker: {:?}",
                result
            );
            assert!(
                stdout.len() < 20_000,
                "stdout must be capped: {}",
                stdout.len()
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_truncate_output_short_unchanged() {
        let (out, truncated) = super::super::builtins::truncate_output("hello");
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_truncate_output_exact_boundary() {
        let (out, truncated) = super::super::builtins::truncate_output(&"a".repeat(16_384));
        assert_eq!(out.len(), 16_384);
        assert!(!truncated);
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_truncate_output_one_over() {
        let (out, truncated) = super::super::builtins::truncate_output(&"a".repeat(16_385));
        assert!(truncated);
        assert!(out.contains("[output truncated"));
    }

    fn test_owner(tenant: &str, run: &str, agent: &str) -> MicroToolOwner {
        MicroToolOwner {
            tenant_id: tenant.to_string(),
            project_id: "project".to_string(),
            run_id: run.to_string(),
            agent_id: agent.to_string(),
        }
    }

    /// Store `data` and register `reader` for `owner`, as the router does.
    fn register_reader(
        executor: &ToolExecutor,
        owner: &MicroToolOwner,
        reader: &str,
        call_id: &str,
        data: Value,
    ) {
        let storage_key = owner.storage_key(call_id);
        executor.store_micro_tool_data(owner, &storage_key, data);
        executor.register_micro_tool(
            reader,
            MicroToolContext {
                call_id: call_id.to_string(),
                storage_key,
                tool_name: "file_read".to_string(),
                entity_types: vec![],
                preview_size: 100,
                owner: owner.clone(),
            },
        );
    }

    fn check_context() -> SecurityContext {
        SecurityContext::new("agent:check", "CA").with_task("iri://tasks/security-test")
    }

    fn marker_tool(hit: Arc<std::sync::atomic::AtomicUsize>) -> ToolFn {
        Arc::new(move |_| {
            let hit = hit.clone();
            Box::pin(async move {
                hit.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(json!({"written": true}))
            })
        })
    }

    /// #270-2: an external tool whose name merely starts with `query_` is not
    /// read-only, so Check cannot see or execute it; an internal reader is.
    #[test]
    fn prefix_named_external_tool_is_not_executable_by_check() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            executor.set_tool_group_manager(ToolGroupManager::new(None));
            let hit = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            executor.register(
                "query_orders",
                "Update order rows in an external system. Use when: testing. Not for: reading.",
                json!({"type": "object", "properties": {}}),
                marker_tool(hit.clone()),
                &[],
            );
            let owner = test_owner("tenant-a", "run-1", "agent:check");
            register_reader(
                &executor,
                &owner,
                "read_full_result_c1",
                "c1",
                json!({"content": "row"}),
            );

            let activated = executor.activated_tools();
            let names: Vec<String> = executor
                .tool_definitions_for_run("Check", "", &activated, Some(&owner))
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                .collect();
            assert!(!names.contains(&"query_orders".to_string()));
            assert!(names.contains(&"read_full_result_c1".to_string()));

            let advertised = vec![
                "query_orders".to_string(),
                "read_full_result_c1".to_string(),
            ];
            let denied = executor
                .execute_with_security_context(
                    "query_orders",
                    json!({}),
                    check_context(),
                    &advertised,
                    activated.policy(),
                )
                .await
                .unwrap();
            assert_eq!(denied["error"], "Tool not allowed for role");
            assert_eq!(hit.load(std::sync::atomic::Ordering::SeqCst), 0);

            let allowed = executor
                .execute_guarded(
                    "read_full_result_c1",
                    json!({}),
                    check_context(),
                    &advertised,
                    None,
                    activated.policy(),
                    Some(&owner),
                )
                .await
                .unwrap()
                .value;
            assert_eq!(allowed["content"], "row");
        });
    }

    /// Re-registering a micro-tool name as an ordinary tool removes its
    /// internal status, so the prefix rule no longer applies to it.
    #[test]
    fn external_registration_replaces_internal_micro_tool_status() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            executor.set_tool_group_manager(ToolGroupManager::new(None));
            let owner = test_owner("tenant-a", "run-1", "agent:check");
            register_reader(
                &executor,
                &owner,
                "query_person",
                "c2",
                json!({"content": "row"}),
            );
            let hit = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            executor.register(
                "query_person",
                "Write person rows in an external system. Use when: testing. Not for: reading.",
                json!({"type": "object", "properties": {}}),
                marker_tool(hit.clone()),
                &[],
            );
            let advertised = vec!["query_person".to_string()];
            let denied = executor
                .execute_with_security_context(
                    "query_person",
                    json!({}),
                    check_context(),
                    &advertised,
                    executor.activated_tools().policy(),
                )
                .await
                .unwrap();
            assert_eq!(denied["error"], "Tool not allowed for role");
            assert_eq!(hit.load(std::sync::atomic::Ordering::SeqCst), 0);
        });
    }

    /// #270-1: with a SyscallGate installed, a call without a trusted caller
    /// context (no role, no run-local policy) is rejected before the handler.
    #[test]
    fn syscall_gate_rejects_calls_without_trusted_context() {
        rt().block_on(async {
            let mut executor = ToolExecutor::new();
            executor.set_tool_group_manager(ToolGroupManager::new(None));
            let hit = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            executor.register(
                "file_list",
                "Test override of file_list. Use when: testing. Not for: anything else.",
                json!({"type": "object", "properties": {}}),
                marker_tool(hit.clone()),
                &[],
            );
            executor.set_syscall_gate(crate::core::syscall_gate::SyscallGate::new(
                Arc::new(SkillRegistry::new()),
                2048,
            ));

            let raw = executor.execute("file_list", json!({})).await.unwrap();
            let error = raw["error"].as_str().unwrap_or_default();
            assert!(error.starts_with("SyscallGate rejected"), "{raw}");
            assert!(error.contains("no trusted caller role"), "{raw}");
            assert_eq!(hit.load(std::sync::atomic::Ordering::SeqCst), 0);

            let activated = executor.activated_tools();
            let advertised = vec!["file_list".to_string()];
            let ok = executor
                .execute_with_security_context(
                    "file_list",
                    json!({}),
                    security_context(),
                    &advertised,
                    activated.policy(),
                )
                .await
                .unwrap();
            assert_eq!(ok["written"], true, "{ok}");
            assert_eq!(hit.load(std::sync::atomic::Ordering::SeqCst), 1);

            // The gate follows the run-local narrowing, not the default policy.
            let mut narrowed = activated.policy().clone();
            narrowed.restrict_tools("agent:test", ["file_read".to_string()]);
            let denied = executor
                .execute_with_security_context(
                    "file_list",
                    json!({}),
                    security_context(),
                    &advertised,
                    &narrowed,
                )
                .await
                .unwrap();
            assert_eq!(denied["error"], "Tool not allowed for role");
            assert_eq!(hit.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    // ── #311: generated result readers are owned by run/agent/tenant ──

    /// Calls `name` the way the runner does, as `owner`, and returns the
    /// serialized outcome so responses can be compared byte for byte.
    async fn call_reader_as(
        executor: &ToolExecutor,
        owner: &MicroToolOwner,
        name: &str,
        input: Value,
    ) -> String {
        let advertised = vec![name.to_string()];
        let outcome = executor
            .execute_guarded(
                name,
                input,
                security_context(),
                &advertised,
                None,
                executor.activated_tools().policy(),
                Some(owner),
            )
            .await
            .map(|outcome| outcome.value)
            .map_err(|error| error.to_string());
        serde_json::to_string(&outcome.map_err(Value::String)).unwrap()
    }

    /// What any caller gets for `name` when no reader was ever registered.
    async fn never_registered_response(owner: &MicroToolOwner, name: &str) -> String {
        call_reader_as(&ToolExecutor::new(), owner, name, json!({})).await
    }

    fn canary(label: &str) -> Value {
        json!({"content": format!("CANARY-{label}-line-1\nCANARY-{label}-line-2")})
    }

    #[test]
    fn isolation_contract_micro_reader_is_absent_for_another_run() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let run_a = test_owner("tenant-a", "run-a", "agent:test");
            let run_b = test_owner("tenant-a", "run-b", "agent:test");
            register_reader(&executor, &run_a, "read_full_result_c1", "c1", canary("A"));

            let own = call_reader_as(&executor, &run_a, "read_full_result_c1", json!({})).await;
            assert!(own.contains("CANARY-A"), "{own}");

            let other = call_reader_as(&executor, &run_b, "read_full_result_c1", json!({})).await;
            assert!(!other.contains("CANARY-A"), "{other}");
            assert_eq!(
                other,
                never_registered_response(&run_b, "read_full_result_c1").await
            );
            // Outside any runtime owner scope the reader does not exist either.
            assert!(executor.try_get_handler("read_full_result_c1").is_none());
        });
    }

    #[test]
    fn isolation_contract_micro_reader_is_absent_for_another_agent_in_same_run() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let agent_a = test_owner("tenant-a", "run-1", "agent:a");
            let agent_b = test_owner("tenant-a", "run-1", "agent:b");
            register_reader(
                &executor,
                &agent_a,
                "read_full_result_c1",
                "c1",
                canary("A"),
            );

            let other = call_reader_as(&executor, &agent_b, "read_full_result_c1", json!({})).await;
            assert!(!other.contains("CANARY-A"), "{other}");
            assert_eq!(
                other,
                never_registered_response(&agent_b, "read_full_result_c1").await
            );
        });
    }

    #[test]
    fn isolation_contract_micro_reader_same_call_id_across_tenants_stays_separate() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            // Provider-supplied call ids are not secret and can repeat.
            let tenant_a = test_owner("tenant-a", "run-1", "agent:test");
            let tenant_b = test_owner("tenant-b", "run-1", "agent:test");
            assert_ne!(
                tenant_a.storage_key("call_1"),
                tenant_b.storage_key("call_1")
            );
            register_reader(
                &executor,
                &tenant_a,
                "read_full_result_call_1",
                "call_1",
                canary("A"),
            );

            let probe =
                call_reader_as(&executor, &tenant_b, "read_full_result_call_1", json!({})).await;
            assert!(!probe.contains("CANARY-A"), "{probe}");
            assert_eq!(
                probe,
                never_registered_response(&tenant_b, "read_full_result_call_1").await
            );

            register_reader(
                &executor,
                &tenant_b,
                "read_full_result_call_1",
                "call_1",
                canary("B"),
            );
            let a =
                call_reader_as(&executor, &tenant_a, "read_full_result_call_1", json!({})).await;
            let b =
                call_reader_as(&executor, &tenant_b, "read_full_result_call_1", json!({})).await;
            assert!(a.contains("CANARY-A") && !a.contains("CANARY-B"), "{a}");
            assert!(b.contains("CANARY-B") && !b.contains("CANARY-A"), "{b}");
        });
    }

    #[test]
    fn isolation_contract_graphify_reader_names_do_not_cross_runs() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let run_a = test_owner("tenant-a", "run-a", "agent:test");
            let run_b = test_owner("tenant-a", "run-b", "agent:test");
            // Graphify readers carry no call id: both runs get `query_person`.
            let rows = |label: &str| {
                json!({"content": json!([{"id": format!("{label}-1"), "type": "person", "name": format!("CANARY-{label}")}]).to_string()})
            };
            // Same provider call id on both sides, too.
            register_reader(&executor, &run_a, "query_person", "g1", rows("A"));
            register_reader(&executor, &run_b, "query_person", "g1", rows("B"));

            let a = call_reader_as(&executor, &run_a, "query_person", json!({})).await;
            let b = call_reader_as(&executor, &run_b, "query_person", json!({})).await;
            assert!(a.contains("CANARY-A") && !a.contains("CANARY-B"), "{a}");
            assert!(b.contains("CANARY-B") && !b.contains("CANARY-A"), "{b}");
        });
    }

    #[test]
    fn isolation_contract_later_registration_with_same_call_id_does_not_overwrite() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let run_a = test_owner("tenant-a", "run-a", "agent:test");
            let run_b = test_owner("tenant-a", "run-b", "agent:test");
            register_reader(
                &executor,
                &run_a,
                "read_full_result_dup",
                "dup",
                canary("A"),
            );
            register_reader(
                &executor,
                &run_b,
                "read_full_result_dup",
                "dup",
                canary("B"),
            );

            let a = call_reader_as(&executor, &run_a, "read_full_result_dup", json!({})).await;
            assert!(a.contains("CANARY-A") && !a.contains("CANARY-B"), "{a}");
        });
    }

    #[test]
    fn isolation_contract_turn_schema_lists_only_own_readers() {
        let executor = ToolExecutor::new();
        let run_a = test_owner("tenant-a", "run-a", "agent:test");
        let run_b = test_owner("tenant-a", "run-b", "agent:test");
        register_reader(&executor, &run_a, "read_full_result_a1", "a1", canary("A"));
        let names = |owner: Option<&MicroToolOwner>| -> Vec<String> {
            executor
                .tool_definitions_for_run("DA", "agent:test", &executor.activated_tools(), owner)
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                .collect()
        };
        assert!(names(Some(&run_a)).contains(&"read_full_result_a1".to_string()));
        assert!(!names(Some(&run_b)).contains(&"read_full_result_a1".to_string()));
        assert!(!names(None).contains(&"read_full_result_a1".to_string()));
        assert!(!executor
            .registered_tool_names()
            .contains(&"read_full_result_a1".to_string()));
    }

    #[test]
    fn micro_reader_owner_reads_full_result_with_paging() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let owner = test_owner("tenant-a", "run-1", "agent:test");
            let content: Vec<String> = (0..10).map(|i| format!("line-{i}")).collect();
            register_reader(
                &executor,
                &owner,
                "read_full_result_page",
                "page",
                json!({"content": content.join("\n")}),
            );
            let out = call_reader_as(
                &executor,
                &owner,
                "read_full_result_page",
                json!({"offset": 2, "limit": 3}),
            )
            .await;
            let out: Value = serde_json::from_str(&out).unwrap();
            let ok = &out["Ok"];
            assert_eq!(ok["content"], "line-2\nline-3\nline-4");
            assert_eq!(ok["total_lines"], 10);
            assert_eq!(ok["offset"], 2);
            assert_eq!(ok["returned"], 3);
            assert_eq!(ok["call_id"], "page");
        });
    }

    #[test]
    fn isolation_contract_run_end_removes_readers_and_results() {
        let executor = ToolExecutor::new();
        let run_a = test_owner("tenant-a", "run-a", "agent:test");
        let run_b = test_owner("tenant-a", "run-b", "agent:test");
        register_reader(&executor, &run_a, "read_full_result_a1", "a1", canary("A"));
        register_reader(&executor, &run_a, "query_person", "a1", canary("A"));
        register_reader(&executor, &run_b, "read_full_result_b1", "b1", canary("B"));
        let store = executor.micro_tool_store();
        assert_eq!(store.counts(), (3, 2));

        store.remove_run("run-a");
        assert_eq!(store.counts(), (1, 1));
        assert!(!executor.has_micro_reader(&run_a, "read_full_result_a1"));
        assert!(executor.has_micro_reader(&run_b, "read_full_result_b1"));
        store.remove_run("run-b");
        assert_eq!(store.counts(), (0, 0));
    }

    #[test]
    fn isolation_contract_expired_readers_are_gone_and_pruned() {
        let executor = ToolExecutor::new();
        let owner = test_owner("tenant-a", "run-a", "agent:test");
        register_reader(
            &executor,
            &owner,
            "read_full_result_old",
            "old",
            canary("A"),
        );
        executor
            .micro_tools
            .0
            .write()
            .advance_clock_for_test(micro_store::DEFAULT_MICRO_TOOL_TTL * 2);
        assert!(!executor.has_micro_reader(&owner, "read_full_result_old"));

        // The next write prunes expired entries.
        register_reader(
            &executor,
            &owner,
            "read_full_result_new",
            "new",
            canary("B"),
        );
        assert_eq!(executor.micro_tool_store().counts(), (1, 1));
        assert!(executor.has_micro_reader(&owner, "read_full_result_new"));
    }

    #[test]
    fn isolation_contract_micro_store_is_bounded_across_runs() {
        let executor = ToolExecutor::new();
        for run in 0..(micro_store::MAX_MICRO_TOOL_ENTRIES + 500) {
            let owner = test_owner("tenant-a", &format!("run-{run}"), "agent:test");
            register_reader(&executor, &owner, "read_full_result_c1", "c1", canary("X"));
            register_reader(&executor, &owner, "query_person", "c1", canary("X"));
        }
        let (readers, data) = executor.micro_tool_store().counts();
        assert!(readers <= micro_store::MAX_MICRO_TOOL_ENTRIES, "{readers}");
        assert!(data <= micro_store::MAX_MICRO_TOOL_ENTRIES, "{data}");
        // Newest entries survive eviction.
        let newest = test_owner(
            "tenant-a",
            &format!("run-{}", micro_store::MAX_MICRO_TOOL_ENTRIES + 499),
            "agent:test",
        );
        assert!(executor.has_micro_reader(&newest, "query_person"));
    }

    #[test]
    fn isolation_contract_tenant_quota_evicts_only_that_tenant() {
        let executor = ToolExecutor::new();
        executor.micro_tool_store().set_tenant_quota_for_test(2);
        let tenant_b = test_owner("tenant-b", "run-b", "agent:test");
        register_reader(
            &executor,
            &tenant_b,
            "read_full_result_b",
            "cb",
            canary("B"),
        );
        for i in 0..4 {
            let tenant_a = test_owner("tenant-a", &format!("run-{i}"), "agent:test");
            register_reader(
                &executor,
                &tenant_a,
                &format!("read_full_result_{i}"),
                &format!("c{i}"),
                canary("A"),
            );
        }
        assert!(executor.has_micro_reader(&tenant_b, "read_full_result_b"));
        let oldest = test_owner("tenant-a", "run-0", "agent:test");
        let newest = test_owner("tenant-a", "run-3", "agent:test");
        assert!(!executor.has_micro_reader(&oldest, "read_full_result_0"));
        assert!(executor.has_micro_reader(&newest, "read_full_result_3"));
        let (readers, data) = executor.micro_tool_store().counts();
        assert!(readers <= 3, "{readers}");
        assert!(data <= 3, "{data}");
        assert!(readers <= micro_store::MAX_MICRO_TOOL_ENTRIES);
    }

    #[test]
    fn isolation_contract_storage_key_segments_cannot_collide() {
        let a = MicroToolOwner {
            tenant_id: "t/a".to_string(),
            project_id: "p".to_string(),
            run_id: "r".to_string(),
            agent_id: "g".to_string(),
        };
        let b = MicroToolOwner {
            tenant_id: "t".to_string(),
            project_id: "a/p".to_string(),
            run_id: "r".to_string(),
            agent_id: "g".to_string(),
        };
        assert_ne!(a.storage_key("c"), b.storage_key("c"));
        assert_eq!(
            a.storage_key("c"),
            "iri://tool-result/t%2Fa/p/r/g/c".to_string()
        );
    }
}
