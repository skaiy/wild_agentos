use crate::core::agent_instance::AgentRole;
use crate::core::tool_policy::ToolPolicy;

/// At most this many tool names are echoed into errors / AGENT_ERROR.
pub(crate) const MAX_REPORTED_TOOLS: usize = 8;
/// Each echoed tool name is cut to this many bytes (on a char boundary).
pub(crate) const MAX_REPORTED_TOOL_NAME_BYTES: usize = 64;
/// Placeholder for a model-supplied name that is not a registered tool.
pub(crate) const UNREGISTERED_TOOL_NAME: &str = "<unregistered>";

/// Makes model-generated tool names safe to echo into errors and events
/// (which may be fanned out over SSE): registered names are kept, anything
/// else becomes `<unregistered>`, names are capped at 64 bytes, duplicates
/// of a kept name are folded and at most 8 are kept. Returns the kept names
/// and how many further names were dropped.
pub(crate) fn sanitize_reported_tool_names<'a>(
    tool_names: impl IntoIterator<Item = &'a str>,
    is_registered: impl Fn(&str) -> bool,
) -> (Vec<String>, usize) {
    let mut reported: Vec<String> = Vec::new();
    let mut omitted = 0;
    for name in tool_names {
        let name = if is_registered(name) {
            let mut end = name.len().min(MAX_REPORTED_TOOL_NAME_BYTES);
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            &name[..end]
        } else {
            UNREGISTERED_TOOL_NAME
        };
        if reported.iter().any(|seen| seen == name) {
            continue;
        }
        if reported.len() < MAX_REPORTED_TOOLS {
            reported.push(name.to_string());
        } else {
            omitted += 1;
        }
    }
    (reported, omitted)
}

/// Built-ins that perform a network request. Every other registered
/// in-process built-in is `local`.
pub(crate) fn recorded_builtin_transport(name: &str) -> &'static str {
    match name {
        "web_search"
        | "web_fetch"
        | "http_request"
        | "knowledge_import_url"
        | "knowledge_extract"
        | "create_skill"
        | "convert_skill"
        | "bash" => "http",
        _ => "local",
    }
}

/// Name and transport stored for one tool attempt that reached the tracker.
///
/// Unregistered names, policy refusals, and calls reported as not found
/// become [`UNREGISTERED_TOOL_NAME`] with transport `unknown`. The raw model
/// string is dropped: it can carry argument text. Registered names are
/// capped at [`MAX_REPORTED_TOOL_NAME_BYTES`].
pub(crate) fn classify_recorded_tool_call(
    name: &str,
    registered: bool,
    policy_denied: bool,
) -> (String, &'static str) {
    let trimmed = name.trim();
    if trimmed.is_empty() || !registered || policy_denied {
        return (UNREGISTERED_TOOL_NAME.to_string(), "unknown");
    }
    let mut end = trimmed.len().min(MAX_REPORTED_TOOL_NAME_BYTES);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    (
        trimmed[..end].to_string(),
        recorded_builtin_transport(trimmed),
    )
}

pub(crate) fn disallowed_pa_tools<'a>(
    role: &AgentRole,
    tool_names: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    if *role != AgentRole::Plan {
        return Vec::new();
    }
    tool_names
        .into_iter()
        .filter(|name| !crate::tools::tool_executor::ToolExecutor::is_pa_readonly_tool(name))
        .collect()
}

/// Role/tool queries over the default `ToolPolicy` (built-in groups, no
/// per-run narrowing, no view of the executor's internal micro-tools).
/// It is not on the runtime execution path: tool calls are authorised by
/// `ToolExecutor::execute_with_security_context_and_claims_and_policy` with
/// the caller's run-local policy, so this type can only answer "what does the
/// default cap allow", never widen a run.
#[derive(Clone)]
pub struct ToolController {
    policy: ToolPolicy,
}

impl ToolController {
    pub fn new() -> Self {
        Self {
            policy: ToolPolicy::new(),
        }
    }

    pub fn is_readonly_tool(&self, tool_name: &str) -> bool {
        ToolPolicy::is_readonly_tool(tool_name)
    }

    pub fn is_write_tool(&self, tool_name: &str) -> bool {
        !self.is_readonly_tool(tool_name)
    }

    pub fn is_tool_allowed_for_role(&self, tool_name: &str, role: &AgentRole) -> bool {
        self.policy.is_executable(role, "", tool_name)
    }

    pub fn list_available_tools(&self, role: &AgentRole) -> Vec<String> {
        self.policy.visible_tools(role, "")
    }

    pub fn should_force_finish(&self, tool_names: &[&str], role: &AgentRole) -> bool {
        !disallowed_pa_tools(role, tool_names.iter().copied()).is_empty()
    }
}

impl Default for ToolController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_readonly_tools() {
        let tc = ToolController::new();
        assert!(tc.is_readonly_tool("file_read"));
        assert!(tc.is_readonly_tool("grep_search"));
        assert!(!tc.is_readonly_tool("file_write"));
        assert!(!tc.is_readonly_tool("bash"));
    }

    #[test]
    fn test_write_tools() {
        let tc = ToolController::new();
        assert!(tc.is_write_tool("file_write"));
        assert!(tc.is_write_tool("bash"));
        assert!(!tc.is_write_tool("file_read"));
    }

    #[test]
    fn test_plan_allowlist_matches_pa_readonly_tools() {
        let tc = ToolController::new();

        for tool in crate::tools::tool_executor::ToolExecutor::pa_readonly_tools() {
            assert!(tc.is_tool_allowed_for_role(tool, &AgentRole::Plan));
        }
        assert!(!tc.is_tool_allowed_for_role("bash", &AgentRole::Plan));
        assert!(!tc.is_tool_allowed_for_role("file_write", &AgentRole::Plan));
    }

    #[test]
    fn role_execution_permissions_match_main_snapshot() {
        let executor = crate::tools::tool_executor::ToolExecutor::new();
        let registered = executor.registered_tool_names();
        let tc = ToolController::new();
        let plan_main = [
            "file_read",
            "file_list",
            "grep_search",
            "glob_search",
            "tool_search",
            "web_search",
            "web_fetch",
            "rag_search",
            "knowledge_list",
            "knowledge_search",
            "kg_search",
            "knowledge_extract_code",
        ];

        let allowed_for = |role: AgentRole| {
            registered
                .iter()
                .map(String::as_str)
                .filter(|tool| tc.is_tool_allowed_for_role(tool, &role))
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(
            allowed_for(AgentRole::Plan),
            plan_main.into_iter().collect()
        );
        // #270-2: the controller cannot see the executor's internal
        // micro-tool registry, so a micro-tool-like name is denied for every
        // role here; registered internal readers are allowed by the executor.
        for role in [
            AgentRole::Plan,
            AgentRole::Do,
            AgentRole::Check,
            AgentRole::Act,
        ] {
            assert!(!tc.is_tool_allowed_for_role("read_full_result_test", &role));
        }
        assert_eq!(
            allowed_for(AgentRole::Do),
            [
                "file_read",
                "file_list",
                "workspace_status",
                "read_agent_output",
                "file_write",
                "bash",
                "powershell",
                "file_edit",
                "grep_search",
                "glob_search",
                "rag_search",
                "kg_search",
                "web_search",
                "web_fetch",
                "knowledge_query",
                "knowledge_neighbors",
                "kb_vector_search",
                "knowledge_list",
                "knowledge_search",
                "knowledge_extract_code",
                "knowledge_update",
                "knowledge_extract",
                "knowledge_bridge",
                "rag_index",
                "rag_chunk",
                "knowledge_import_file",
                "knowledge_import_url",
                "knowledge_import_directory",
                "knowledge_import_json",
                "create_skill",
                "convert_skill",
                "ontology_validate_turtle",
                "ontology_lint_turtle",
                "ontology_diff_turtle",
                "ontology_validate_shacl",
                "ontology_reason",
                "tool_search",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            allowed_for(AgentRole::Check),
            [
                "file_read",
                "file_list",
                "workspace_status",
                "read_agent_output",
                "grep_search",
                "glob_search",
                "rag_search",
                "kg_search",
                "web_search",
                "web_fetch",
                "tool_search",
                "knowledge_list",
                "knowledge_search",
                "knowledge_extract_code",
                "knowledge_query",
                "knowledge_neighbors",
                "kb_vector_search",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            allowed_for(AgentRole::Act),
            [
                "file_read",
                "file_list",
                "grep_search",
                "glob_search",
                "rag_search",
                "kg_search",
                "tool_search",
                "knowledge_list",
                "knowledge_search",
                "knowledge_extract_code",
                "knowledge_query",
                "knowledge_neighbors",
                "kb_vector_search",
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn test_list_available_tools() {
        let tc = ToolController::new();
        let plan_tools = tc.list_available_tools(&AgentRole::Plan);
        assert!(plan_tools.contains(&"file_read".to_string()));
        assert!(!plan_tools.contains(&"file_write".to_string()));
        let do_tools = tc.list_available_tools(&AgentRole::Do);
        assert!(do_tools.contains(&"file_write".to_string()));
    }

    #[test]
    fn test_should_force_finish_plan() {
        let tc = ToolController::new();
        for name in [
            "file_write",
            "bash",
            "not_in_plan_allowlist",
            "read_full_result_x",
        ] {
            assert!(tc.should_force_finish(&[name], &AgentRole::Plan), "{name}");
        }
        assert!(!tc.should_force_finish(&["file_read"], &AgentRole::Plan));
        assert!(!tc.should_force_finish(&["file_write"], &AgentRole::Do));
        assert_eq!(
            disallowed_pa_tools(&AgentRole::Plan, ["file_read", "file_write", "bash"]),
            ["file_write", "bash"]
        );
    }

    #[test]
    fn reported_tool_names_are_registered_bounded_and_capped() {
        let long_registered = "r".repeat(100);
        let long_unregistered = "u".repeat(10 * 1024);
        let registered = |name: &str| {
            name == "file_write" || name == long_registered || name.starts_with("tool_")
        };
        let (names, omitted) = sanitize_reported_tool_names(
            [
                "file_write",
                long_unregistered.as_str(),
                long_registered.as_str(),
            ],
            registered,
        );
        assert_eq!(
            names,
            [
                "file_write".to_string(),
                UNREGISTERED_TOOL_NAME.to_string(),
                "r".repeat(MAX_REPORTED_TOOL_NAME_BYTES),
            ]
        );
        assert_eq!(omitted, 0);

        // Multi-byte names are cut on a char boundary.
        let wide = "é".repeat(40); // 80 bytes
        let (names, _) = sanitize_reported_tool_names([wide.as_str()], |_| true);
        assert!(names[0].len() <= MAX_REPORTED_TOOL_NAME_BYTES);
        assert!(wide.starts_with(&names[0]));

        // Duplicates fold; at most MAX_REPORTED_TOOLS are kept.
        let many: Vec<String> = (0..50).map(|i| format!("tool_{i}")).collect();
        let mut input: Vec<&str> = many.iter().map(String::as_str).collect();
        input.extend(["junk-a", "junk-b", "tool_0"]);
        let (names, omitted) = sanitize_reported_tool_names(input, registered);
        assert_eq!(names.len(), MAX_REPORTED_TOOLS);
        assert_eq!(names[0], "tool_0");
        // 42 more registered names + 2 unregistered ones; the repeated
        // tool_0 folds into the kept entry.
        assert_eq!(omitted, 50 - MAX_REPORTED_TOOLS + 2);
    }

    #[test]
    fn recorded_tool_call_drops_unregistered_payload_and_caps_names() {
        let secret = "secret-payload-9f3a {\"path\":\"/etc/passwd\"}";
        assert_eq!(
            classify_recorded_tool_call(secret, false, false),
            (UNREGISTERED_TOOL_NAME.to_string(), "unknown")
        );
        assert_eq!(
            classify_recorded_tool_call("bash", true, true),
            (UNREGISTERED_TOOL_NAME.to_string(), "unknown")
        );
        assert_eq!(
            classify_recorded_tool_call("bash", true, false),
            ("bash".to_string(), "http")
        );
        assert_eq!(
            classify_recorded_tool_call("file_read", true, false),
            ("file_read".to_string(), "local")
        );
        for name in [
            "web_search",
            "web_fetch",
            "http_request",
            "knowledge_import_url",
            "knowledge_extract",
            "create_skill",
            "convert_skill",
        ] {
            assert_eq!(recorded_builtin_transport(name), "http", "{name}");
        }
        let long = format!("file_read{}", "x".repeat(200));
        let (name, transport) = classify_recorded_tool_call(&long, true, false);
        assert_eq!(name.len(), MAX_REPORTED_TOOL_NAME_BYTES);
        assert!(!name.contains("secret"));
        assert_eq!(transport, "local");
        assert!(!classify_recorded_tool_call(secret, false, false)
            .0
            .contains("secret-payload"));
    }
}
