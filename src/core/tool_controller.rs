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
        let all_registered = registered.iter().map(String::as_str).collect();

        assert_eq!(
            allowed_for(AgentRole::Plan),
            plan_main.into_iter().collect()
        );
        assert_eq!(allowed_for(AgentRole::Do), all_registered);
        assert!(allowed_for(AgentRole::Check).is_subset(&all_registered));
        assert!(allowed_for(AgentRole::Act).is_subset(&all_registered));
        assert!(!allowed_for(AgentRole::Check).contains("bash"));
        assert!(!allowed_for(AgentRole::Act).contains("file_write"));
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
}
