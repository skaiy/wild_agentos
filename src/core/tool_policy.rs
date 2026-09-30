use std::collections::{HashMap, HashSet};

use crate::core::agent_instance::AgentRole;
use crate::tools::tool_groups::ToolGroupManager;

/// Server-owned execution policy for built-in tools.
///
/// The policy is instantiated for each run. Agent restrictions only intersect
/// with a role's cap; they can never add a capability.
#[derive(Debug, Clone)]
pub struct ToolPolicy {
    groups: ToolGroupManager,
    check_bash_enabled: bool,
    agent_restrictions: HashMap<String, HashSet<String>>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPolicy {
    pub fn new() -> Self {
        Self {
            groups: ToolGroupManager::new(None),
            check_bash_enabled: false,
            agent_restrictions: HashMap::new(),
        }
    }

    pub fn with_tool_group_manager(mut self, groups: ToolGroupManager) -> Self {
        self.check_bash_enabled = groups.check_bash_enabled();
        self.groups = groups;
        self
    }

    /// This opt-in is intentionally server configuration, never model input.
    pub fn with_check_bash_enabled(mut self, enabled: bool) -> Self {
        self.check_bash_enabled = enabled;
        self
    }

    pub fn readonly_tools() -> &'static [&'static str] {
        &[
            "file_read",
            "file_list",
            "glob_search",
            "grep_search",
            "web_search",
            "web_fetch",
            "tool_search",
            "rag_search",
            "knowledge_list",
            "knowledge_search",
            "kg_search",
            "knowledge_extract_code",
        ]
    }

    pub fn is_readonly_tool(name: &str) -> bool {
        Self::readonly_tools().contains(&name)
            || name.starts_with("read_full_result_")
            || name.starts_with("query_")
            || name.starts_with("get_entity_details_")
            || name.starts_with("expand_relation_")
    }

    fn role_name(role: &AgentRole) -> &'static str {
        match role {
            AgentRole::Plan => "Plan",
            AgentRole::Do => "Do",
            AgentRole::Check => "Check",
            AgentRole::Act => "Act",
        }
    }

    fn role_cap(&self, role: &AgentRole) -> HashSet<String> {
        let (resident, on_demand) = self.groups.get_tool_names_for_role(Self::role_name(role));
        let mut cap: HashSet<String> = resident.union(&on_demand).cloned().collect();

        match role {
            AgentRole::Plan | AgentRole::Act => {
                cap.retain(|name| Self::is_readonly_tool(name));
            }
            AgentRole::Check => {
                cap.retain(|name| Self::is_readonly_tool(name));
                if self.check_bash_enabled {
                    cap.insert("bash".to_string());
                }
            }
            AgentRole::Do => {}
        }
        cap
    }

    /// Tools which may be shown for the runtime role before per-turn
    /// advertisement. This applies the same trusted narrowing as execution.
    pub fn visible_tools(&self, role: &AgentRole, agent_id: &str) -> Vec<String> {
        let mut tools = self.role_cap(role);
        if let Some(restriction) = self.agent_restrictions.get(agent_id) {
            tools.retain(|name| restriction.contains(name));
        }
        let mut tools: Vec<String> = tools.into_iter().collect();
        tools.sort();
        tools
    }

    pub fn is_visible(&self, role: &AgentRole, agent_id: &str, name: &str) -> bool {
        if name.starts_with("read_full_result_")
            || name.starts_with("query_")
            || name.starts_with("get_entity_details_")
            || name.starts_with("expand_relation_")
        {
            return self
                .visible_tools(role, agent_id)
                .iter()
                .any(|tool| tool == "file_read");
        }
        self.visible_tools(role, agent_id)
            .iter()
            .any(|tool| tool == name)
    }

    pub fn is_executable(&self, role: &AgentRole, agent_id: &str, name: &str) -> bool {
        self.is_visible(role, agent_id, name)
    }

    /// Apply a trusted restriction for this run. Values outside the role cap
    /// are retained as data but cannot become visible or executable.
    pub fn restrict_tools(
        &mut self,
        agent_id: impl Into<String>,
        tools: impl IntoIterator<Item = String>,
    ) {
        let agent_id = agent_id.into();
        let requested: HashSet<String> = tools.into_iter().collect();
        self.agent_restrictions
            .entry(agent_id)
            .and_modify(|current| current.retain(|name| requested.contains(name)))
            .or_insert(requested);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_policy_truth_table() {
        let policy = ToolPolicy::new();
        for role in [
            AgentRole::Plan,
            AgentRole::Do,
            AgentRole::Check,
            AgentRole::Act,
        ] {
            let can_write = policy.is_executable(&role, "agent", "file_write");
            assert_eq!(can_write, role == AgentRole::Do, "{role:?}");
            assert!(!policy.is_executable(&role, "agent", "knowledge_delete"));
            assert!(!policy.is_executable(&role, "agent", "ontology_register"));
            assert!(!policy.is_executable(&role, "agent", "not_registered"));
            assert!(policy.is_executable(&role, "agent", "read_full_result_test"));
        }
        assert!(!policy.is_executable(&AgentRole::Check, "agent", "bash"));
        assert!(ToolPolicy::new()
            .with_check_bash_enabled(true)
            .is_executable(&AgentRole::Check, "agent", "bash"));
    }

    #[test]
    fn agent_restrictions_only_narrow() {
        let mut policy = ToolPolicy::new();
        policy.restrict_tools("agent", ["file_read".to_string(), "bash".to_string()]);
        assert!(policy.is_executable(&AgentRole::Do, "agent", "file_read"));
        assert!(!policy.is_executable(&AgentRole::Do, "agent", "file_write"));
        assert!(!policy.is_executable(&AgentRole::Plan, "agent", "bash"));
    }
}
