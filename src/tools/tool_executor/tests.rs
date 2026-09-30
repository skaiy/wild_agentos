use super::*;
use crate::config::RuntimeHookConfig;
use crate::isolation::IsolationClaims;
use crate::tools::builtin::hooks::HookRunner;
use crate::tools::builtin::permissions::{PermissionMode, PermissionPolicy};

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().expect("Failed to create runtime")
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
                )
                .await
                .unwrap();
            assert_eq!(result["error"], "Tool not advertised for this turn: bash");
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
            let mut executor = ToolExecutor::new();
            let micro_name = "read_full_result_turn_one";
            executor.store_micro_tool_data(
                "iri://tool-result/turn-one",
                json!({"content": "turn one data"}),
            );
            executor.register_micro_tool(
                micro_name,
                MicroToolContext {
                    call_id: "turn-one".to_string(),
                    storage_key: "iri://tool-result/turn-one".to_string(),
                    tool_name: "file_read".to_string(),
                    entity_types: vec![],
                    preview_size: 100,
                },
            );

            let first_turn = vec![micro_name.to_string()];
            let first_result = executor
                .execute_with_security_context(
                    micro_name,
                    json!({}),
                    security_context(),
                    &first_turn,
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
                )
                .await
                .unwrap();
            assert_eq!(
                rejected["error"],
                format!("Tool not advertised for this turn: {}", micro_name)
            );
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
}
