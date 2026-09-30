use crate::core::agent_instance::AgentRole;
use crate::core::tool_policy::ToolPolicy;

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
        *role == AgentRole::Plan
            && tool_names
                .iter()
                .any(|name| !crate::tools::tool_executor::ToolExecutor::is_pa_readonly_tool(name))
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
        assert!(!tc.is_tool_allowed_for_role("read_full_result_test", &AgentRole::Plan));
        for role in [AgentRole::Do, AgentRole::Check, AgentRole::Act] {
            assert!(tc.is_tool_allowed_for_role("read_full_result_test", &role));
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
    }
}
