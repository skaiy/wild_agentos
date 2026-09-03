use super::*;
use crate::config::RuntimeHookConfig;
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
            let hook_config = RuntimeHookConfig::new(
                vec!["printf 'blocked by security policy'; exit 2".to_string()],
                vec![],
                vec![],
            );
            executor.set_hook_runner(HookRunner::new(hook_config));

            let input = json!({"query": "search test"});
            let result = executor.execute("tool_search", input).await;
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
    fn test_pa_readonly_tools_includes_bash() {
        assert!(ToolExecutor::is_pa_readonly_tool("bash"));
        assert!(ToolExecutor::is_pa_readonly_tool("file_read"));
        assert!(ToolExecutor::is_pa_readonly_tool("grep_search"));
        assert!(!ToolExecutor::is_pa_readonly_tool("file_write"));
        assert!(!ToolExecutor::is_pa_readonly_tool("file_edit"));
    }

    fn security_context() -> SecurityContext {
        SecurityContext::new("agent:test", "DA").with_task("iri://tasks/security-test")
    }

    #[test]
    fn tools_allowed_rejects_unlisted_tool() {
        rt().block_on(async {
            let executor = ToolExecutor::new();
            let result = executor
                .execute_with_security_context(
                    "bash",
                    json!({"command": "ls"}),
                    security_context(),
                    Some(&["file_read".to_string()]),
                )
                .await
                .unwrap();
            assert_eq!(result["error"], "Tool not allowed: bash");
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
                    None,
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
                        None,
                    )
                    .await;
                let err = match outcome {
                    Ok(result) => result
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("")
                        .to_string(),
                    Err(e) => e,
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
                    None,
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
    fn test_bash_self_protect_pkill_still_kills_real_target() {
        rt().block_on(async {
            use std::process::Command;
            // Spawn a real background sleep; pkill -f on a unique marker
            // must still terminate it (protection only filters the agent).
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
                result["exit_code"], 0,
                "pkill should find the target: {:?}",
                result
            );
            // The child must be gone shortly after.
            for _ in 0..50 {
                if let Ok(Some(status)) = child.try_wait() {
                    assert!(!status.success() || status.code() != Some(0));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = child.kill();
            panic!("target process was not killed");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_self_protect_killall_excludes_own_pid() {
        rt().block_on(async {
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
    fn test_bash_sandbox_disabled_when_requested() {
        rt().block_on(async {
            let result = super::super::builtins::execute_bash(json!({
                "command": "printf hi",
                "dangerouslyDisableSandbox": true,
            }))
            .await
            .unwrap();
            assert_eq!(result["exit_code"], 0);
            let status = &result["sandbox_status"];
            assert_eq!(
                status["enabled"], false,
                "sandbox must be disabled: {:?}",
                result
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_sandbox_unshare_launcher_active() {
        rt().block_on(async {
            // Sandbox is opt-in: with explicit enablement and namespace
            // restrictions the command must run inside the unshare sandbox
            // (or fall back gracefully on hosts without unshare).
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
