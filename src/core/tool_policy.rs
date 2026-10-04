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
            "workspace_status",
            "read_agent_output",
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
            "knowledge_query",
            "knowledge_neighbors",
            "kb_vector_search",
        ]
    }

    /// The immutable Plan cap retained from the main-branch execution policy.
    pub fn plan_readonly_tools() -> &'static [&'static str] {
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

    /// Static read-only built-ins. A name prefix alone never makes a tool
    /// read-only: an external or plugin tool may be called `query_*` and write.
    pub fn is_readonly_tool(name: &str) -> bool {
        Self::readonly_tools().contains(&name)
    }

    /// Name prefixes used by the executor's internal result-reader micro-tools.
    pub fn internal_micro_tool_prefixes() -> &'static [&'static str] {
        &[
            "read_full_result_",
            "query_",
            "get_entity_details_",
            "expand_relation_",
        ]
    }

    pub fn has_internal_micro_tool_prefix(name: &str) -> bool {
        Self::internal_micro_tool_prefixes()
            .iter()
            .any(|prefix| name.starts_with(prefix))
    }

    /// Read-only rule for internal micro-tools. The caller must already have
    /// confirmed that `name` is registered in the executor's internal
    /// micro-tool registry; this policy cannot see that registry, so
    /// `is_visible` / `is_executable` never grant a tool by prefix alone.
    /// Such readers inherit `file_read` and are never available to Plan.
    pub fn is_internal_micro_tool_executable(
        &self,
        role: &AgentRole,
        agent_id: &str,
        name: &str,
    ) -> bool {
        *role != AgentRole::Plan
            && Self::has_internal_micro_tool_prefix(name)
            && self
                .visible_tools(role, agent_id)
                .iter()
                .any(|tool| tool == "file_read")
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
            AgentRole::Plan => {
                cap.retain(|name| Self::plan_readonly_tools().contains(&name.as_str()));
            }
            AgentRole::Act => {
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
        // Destructive knowledge deletion and ontology registration are never
        // granted by the default PDCA execution policy.
        cap.remove("knowledge_delete");
        cap.remove("ontology_register");
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

    /// Tools in the role cap after per-run narrowing. Internal micro-tools are
    /// handled by `is_internal_micro_tool_executable`, never by name here.
    pub fn is_visible(&self, role: &AgentRole, agent_id: &str, name: &str) -> bool {
        self.visible_tools(role, agent_id)
            .iter()
            .any(|tool| tool == name)
    }

    pub fn is_executable(&self, role: &AgentRole, agent_id: &str, name: &str) -> bool {
        self.is_visible(role, agent_id, name)
    }

    pub fn check_bash_enabled(&self) -> bool {
        self.check_bash_enabled
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
            // #270-2: a prefix alone never grants execution.
            assert!(!policy.is_executable(&role, "agent", "read_full_result_test"));
            assert_eq!(
                policy.is_internal_micro_tool_executable(&role, "agent", "read_full_result_test"),
                role != AgentRole::Plan,
                "{role:?}"
            );
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

    #[test]
    fn plan_cap_ignores_extra_configured_groups() {
        let mut settings = crate::tools::tool_groups::ToolGroupSettings::default();
        settings
            .roles
            .get_mut("Plan")
            .unwrap()
            .default
            .push("Workspace".to_string());
        let policy =
            ToolPolicy::new().with_tool_group_manager(ToolGroupManager::new(Some(settings)));
        assert!(!policy.is_executable(&AgentRole::Plan, "agent", "workspace_status"));
        assert!(!policy.is_executable(&AgentRole::Plan, "agent", "read_full_result_test"));
        assert!(!policy.is_internal_micro_tool_executable(
            &AgentRole::Plan,
            "agent",
            "read_full_result_test"
        ));
    }

    /// #270-2: an external tool named like a micro-tool is not read-only.
    #[test]
    fn prefix_named_external_tools_are_not_readonly_for_check_or_act() {
        for name in [
            "query_orders",
            "read_full_result_x",
            "get_entity_details_x",
            "expand_relation_x",
        ] {
            assert!(!ToolPolicy::is_readonly_tool(name), "{name}");
        }
        // Before #270 these were visible to any role that could see file_read.
        let policy = ToolPolicy::new();
        for role in [AgentRole::Check, AgentRole::Act] {
            for name in ["query_orders", "read_full_result_x"] {
                assert!(!policy.is_visible(&role, "agent", name), "{role:?} {name}");
                assert!(
                    !policy.is_executable(&role, "agent", name),
                    "{role:?} {name}"
                );
            }
        }
    }

    /// The micro-tool rule still follows run-local narrowing: without
    /// `file_read` in the run there is no result reader either.
    #[test]
    fn internal_micro_tool_rule_follows_run_restriction() {
        let mut policy = ToolPolicy::new();
        assert!(policy.is_internal_micro_tool_executable(&AgentRole::Do, "agent", "query_person"));
        policy.restrict_tools("agent", ["file_list".to_string()]);
        assert!(!policy.is_internal_micro_tool_executable(&AgentRole::Do, "agent", "query_person"));
        assert!(!policy.is_internal_micro_tool_executable(&AgentRole::Do, "agent", "file_read"));
    }
}
