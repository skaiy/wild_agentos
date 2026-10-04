use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tracing::{debug, warn};

use crate::core::agent_instance::AgentRole;
use crate::core::validation::{JsonLdValidator, SignatureVerifier};
use crate::memory::l2_blackboard::Blackboard;
use crate::tools::skill_registry::SkillRegistry;
use crate::CoreError;

/// Static role whitelist built from the default `ToolPolicy`. The runtime
/// executor does not use it for role decisions: `SyscallGate::validate_tool_for_run`
/// takes the caller's run-local policy and fails closed without one.
#[derive(Clone)]
pub struct WhitelistManager {
    role_whitelist: HashMap<AgentRole, HashSet<String>>,
    custom_whitelist: HashMap<String, HashSet<String>>,
}

impl Default for WhitelistManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WhitelistManager {
    pub fn new() -> Self {
        let policy = crate::core::tool_policy::ToolPolicy::new();
        let mut map = HashMap::new();
        for role in [
            AgentRole::Plan,
            AgentRole::Do,
            AgentRole::Check,
            AgentRole::Act,
        ] {
            map.insert(role, policy.visible_tools(&role, "").into_iter().collect());
        }
        Self {
            role_whitelist: map,
            custom_whitelist: HashMap::new(),
        }
    }

    pub fn check_permission(&self, role: &AgentRole, tool_name: &str) -> bool {
        if let Some(whitelist) = self.role_whitelist.get(role) {
            if whitelist.contains(tool_name) {
                return true;
            }
        }
        false
    }

    pub fn check_permission_for_agent(
        &self,
        _agent_id: &str,
        role: &AgentRole,
        tool_name: &str,
    ) -> bool {
        // The legacy custom list must not widen a role's policy. Per-run
        // narrowing is enforced by ToolPolicy in the AgentRunner.
        self.check_permission(role, tool_name)
    }

    pub fn add_tool(&mut self, role: AgentRole, tool_name: &str) {
        if crate::core::tool_policy::ToolPolicy::new().is_executable(&role, "", tool_name) {
            self.role_whitelist
                .entry(role)
                .or_default()
                .insert(tool_name.to_string());
        } else {
            warn!(role = %role, tool = %tool_name, "Ignoring attempt to widen role whitelist");
        }
    }

    pub fn remove_tool(&mut self, role: &AgentRole, tool_name: &str) {
        if let Some(whitelist) = self.role_whitelist.get_mut(role) {
            whitelist.remove(tool_name);
        }
    }

    pub fn add_custom_whitelist(&mut self, agent_id: &str, tools: Vec<String>) {
        let set: HashSet<String> = tools.into_iter().collect();
        self.custom_whitelist.insert(agent_id.to_string(), set);
    }

    pub fn list_allowed_tools(&self, role: &AgentRole) -> Vec<String> {
        let mut tools = Vec::new();
        if let Some(whitelist) = self.role_whitelist.get(role) {
            tools = whitelist.iter().cloned().collect();
        }
        tools.sort();
        tools
    }
}

#[derive(Clone)]
pub struct SyscallGate {
    validator: JsonLdValidator,
    signature_verifier: SignatureVerifier,
    skills: Arc<SkillRegistry>,
    agent_whitelist: HashMap<String, Vec<String>>,
    whitelist_manager: WhitelistManager,
}

impl SyscallGate {
    pub fn new(skills: Arc<SkillRegistry>, max_size: usize) -> Self {
        Self {
            validator: JsonLdValidator::new(max_size, true),
            signature_verifier: SignatureVerifier::new(),
            skills,
            agent_whitelist: HashMap::new(),
            whitelist_manager: WhitelistManager::new(),
        }
    }

    pub fn with_whitelist_manager(mut self, manager: WhitelistManager) -> Self {
        self.whitelist_manager = manager;
        self
    }

    pub fn whitelist_manager(&self) -> &WhitelistManager {
        &self.whitelist_manager
    }

    pub fn whitelist_manager_mut(&mut self) -> &mut WhitelistManager {
        &mut self.whitelist_manager
    }

    pub fn validate_call_with_role(
        &self,
        agent_id: &str,
        skill_iri: &str,
        input_json: &str,
        role: &AgentRole,
    ) -> Result<Value, CoreError> {
        let tool_name = skill_iri.trim_start_matches("iri://skills/").to_string();

        if !self
            .whitelist_manager
            .check_permission_for_agent(agent_id, role, &tool_name)
        {
            warn!(agent = %agent_id, role = %role, tool = %tool_name, "Role whitelist denied");
            return Err(CoreError::ValidationFailed {
                message: format!(
                    "Agent {} (role {:?}) not authorized to call tool {}",
                    agent_id, role, tool_name
                ),
            });
        }

        self.validate_call(agent_id, skill_iri, input_json)
    }

    pub fn check_5w2h_constraints(
        &self,
        tool_name: &str,
        five_w2h_snapshot: Option<&crate::core::five_w2h::Task5W2H>,
    ) -> Result<(), crate::CoreError> {
        let snapshot = match five_w2h_snapshot {
            Some(s) => s,
            None => return Ok(()),
        };

        if let Some(ref how) = snapshot.how {
            if how
                .forbidden_tools
                .iter()
                .any(|t| t.eq_ignore_ascii_case(tool_name))
            {
                return Err(crate::CoreError::Internal {
                    message: format!("Tool {} is in 5W2H forbiddenTools list, denied", tool_name),
                });
            }
        }

        if let Some(ref who) = snapshot.who {
            if let Some(ref access_level) = who.access_level {
                let write_tools = ["file_write", "bash", "code_execute", "file_delete"];
                if *access_level == crate::core::five_w2h::AccessLevel::Read
                    && write_tools
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(tool_name))
                {
                    return Err(crate::CoreError::Internal {
                        message: format!(
                            "5W2H accessLevel is Read, write tool {} denied",
                            tool_name
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    /// Validates against the static role whitelist. An empty or unknown role
    /// fails closed: without a trusted caller role there is nothing to check
    /// the whitelist against (#270-1).
    pub fn validate_tool_with_5w2h(
        &self,
        tool_name: &str,
        agent_role: &str,
        five_w2h_snapshot: Option<&crate::core::five_w2h::Task5W2H>,
    ) -> Result<(), crate::CoreError> {
        let role = Self::trusted_role(tool_name, agent_role)?;
        if !self.whitelist_manager.check_permission(&role, tool_name) {
            return Err(Self::role_denied(agent_role, tool_name));
        }
        self.check_5w2h_constraints(tool_name, five_w2h_snapshot)
    }

    /// Runtime gate used by the tool executor. The role decision comes from
    /// the caller's run-local `ToolPolicy` (server tool groups plus per-run
    /// narrowing), not from the default policy behind `WhitelistManager`.
    /// It fails closed without a trusted role or without a run-local policy.
    /// `internal_micro_tool` must only be true for readers the executor
    /// registered itself.
    pub fn validate_tool_for_run(
        &self,
        tool_name: &str,
        agent_role: &str,
        agent_id: &str,
        run_policy: Option<&crate::core::tool_policy::ToolPolicy>,
        internal_micro_tool: bool,
        five_w2h_snapshot: Option<&crate::core::five_w2h::Task5W2H>,
    ) -> Result<(), crate::CoreError> {
        let role = Self::trusted_role(tool_name, agent_role)?;
        let Some(policy) = run_policy else {
            warn!(role = %agent_role, tool = %tool_name, "SyscallGate denied: no run-local tool policy");
            return Err(crate::CoreError::Internal {
                message: format!(
                    "Tool '{}' denied: no run-local tool policy for this caller",
                    tool_name
                ),
            });
        };
        let allowed = policy.is_executable(&role, agent_id, tool_name)
            || (internal_micro_tool
                && policy.is_internal_micro_tool_executable(&role, agent_id, tool_name));
        if !allowed {
            return Err(Self::role_denied(agent_role, tool_name));
        }
        self.check_5w2h_constraints(tool_name, five_w2h_snapshot)
    }

    fn trusted_role(tool_name: &str, agent_role: &str) -> Result<AgentRole, crate::CoreError> {
        agent_role.parse::<AgentRole>().map_err(|_| {
            warn!(role = %agent_role, tool = %tool_name, "SyscallGate denied: no trusted caller role");
            crate::CoreError::Internal {
                message: format!(
                    "Tool '{}' denied: no trusted caller role (got '{}')",
                    tool_name, agent_role
                ),
            }
        })
    }

    fn role_denied(agent_role: &str, tool_name: &str) -> crate::CoreError {
        warn!(role = %agent_role, tool = %tool_name, "Role-based whitelist denied");
        crate::CoreError::Internal {
            message: format!(
                "Role '{}' is not allowed to use tool '{}'",
                agent_role, tool_name
            ),
        }
    }

    pub fn set_agent_whitelist(&mut self, agent_id: &str, allowed_iris: Vec<String>) {
        self.agent_whitelist
            .insert(agent_id.to_string(), allowed_iris);
    }

    pub fn add_to_whitelist(&mut self, agent_id: &str, skill_iri: &str) {
        self.agent_whitelist
            .entry(agent_id.to_string())
            .or_default()
            .push(skill_iri.to_string());
    }

    pub fn validate_call(
        &self,
        agent_id: &str,
        skill_iri: &str,
        input_json: &str,
    ) -> Result<Value, CoreError> {
        let validated = self.skills.validate_input(skill_iri, input_json)?;

        if !self.skills.check_signature(skill_iri) {
            warn!(skill = %skill_iri, "Skill signature verification failed");
            return Err(CoreError::ValidationFailed {
                message: format!("Skill {} signature invalid", skill_iri),
            });
        }

        if !self.check_whitelist(agent_id, skill_iri) {
            warn!(agent = %agent_id, skill = %skill_iri, "Agent not in whitelist");
            return Err(CoreError::ValidationFailed {
                message: format!("Agent {} not authorized for skill {}", agent_id, skill_iri),
            });
        }

        debug!(agent = %agent_id, skill = %skill_iri, "SyscallGate: passed");
        Ok(validated)
    }

    fn check_whitelist(&self, agent_id: &str, skill_iri: &str) -> bool {
        self.agent_whitelist
            .get(agent_id)
            .map(|list| list.iter().any(|iri| iri == skill_iri))
            .unwrap_or(false)
    }

    pub fn sync_whitelist_to_oxigraph(
        &self,
        blackboard: &Blackboard,
        agent_id: &str,
    ) -> Result<usize, CoreError> {
        let graph = format!("iri://whitelist/{}", agent_id);
        let mut count = 0;
        if let Some(iris) = self.agent_whitelist.get(agent_id) {
            for iri in iris {
                let sparql = format!(
                    "INSERT DATA {{ GRAPH <{graph}> {{ <{iri}> <https://wildagentos.org/ontology/skill#accessibleTool> \"true\" . }} }}",
                    graph = graph, iri = iri
                );
                blackboard.sparql_update(&sparql)?;
                count += 1;
            }
        }
        debug!(agent = %agent_id, count = count, "Whitelist synced to oxigraph");
        Ok(count)
    }

    pub fn query_agent_tools(
        &self,
        blackboard: &Blackboard,
        agent_id: &str,
    ) -> Result<Vec<String>, CoreError> {
        let graph = format!("iri://whitelist/{}", agent_id);
        let sparql = format!(
            "SELECT ?iri WHERE {{ GRAPH <{graph}> {{ ?iri a <https://wildagentos.org/ontology/skill#AccessibleTool> }} }}",
            graph = graph
        );
        let results = blackboard.query(&sparql)?;
        let mut tools = Vec::new();
        for row in &results {
            if let Some(iri) = row.get("iri").and_then(|v| v.as_str()) {
                tools.push(iri.to_string());
            }
        }
        Ok(tools)
    }

    pub fn validate_json_ld(&self, json_ld: &str) -> Result<(), CoreError> {
        self.validator.validate(json_ld);
        Ok(())
    }

    pub fn verify_signature(&self, data: &str, signature: &str) -> Result<bool, CoreError> {
        self.signature_verifier.verify(data, signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whitelist() {
        let skills = Arc::new(SkillRegistry::new());
        let mut gate = SyscallGate::new(skills, 2048);
        gate.add_to_whitelist("agent_da_1", "iri://skills/file_read");
        gate.add_to_whitelist("agent_da_1", "iri://skills/file_write");

        let valid = gate.check_whitelist("agent_da_1", "iri://skills/file_read");
        assert!(valid);
        let denied = gate.check_whitelist("agent_da_1", "iri://skills/llm_chat");
        assert!(!denied);
    }

    #[test]
    fn test_unknown_agent_not_authorized() {
        let skills = Arc::new(SkillRegistry::new());
        let gate = SyscallGate::new(skills, 2048);
        assert!(!gate.check_whitelist("unknown", "iri://skills/file_read"));
    }

    #[test]
    fn test_whitelist_manager_default() {
        let wm = WhitelistManager::new();

        assert!(wm.check_permission(&AgentRole::Plan, "file_read"));
        assert!(wm.check_permission(&AgentRole::Plan, "grep_search"));
        assert!(!wm.check_permission(&AgentRole::Plan, "file_write"));
        assert!(!wm.check_permission(&AgentRole::Plan, "bash"));

        assert!(wm.check_permission(&AgentRole::Do, "file_write"));
        assert!(wm.check_permission(&AgentRole::Do, "bash"));
        assert!(wm.check_permission(&AgentRole::Do, "rag_search"));

        assert!(!wm.check_permission(&AgentRole::Check, "bash"));
        assert!(!wm.check_permission(&AgentRole::Check, "file_write"));

        assert!(!wm.check_permission(&AgentRole::Act, "file_write"));
        assert!(!wm.check_permission(&AgentRole::Act, "bash"));
    }

    #[test]
    fn test_whitelist_manager_custom() {
        let mut wm = WhitelistManager::new();
        wm.add_custom_whitelist("special_agent", vec!["custom_tool".to_string()]);

        assert!(!wm.check_permission_for_agent("special_agent", &AgentRole::Plan, "custom_tool"));
        assert!(wm.check_permission_for_agent("special_agent", &AgentRole::Plan, "file_read"));
        assert!(!wm.check_permission_for_agent("normal_agent", &AgentRole::Plan, "custom_tool"));
    }

    #[test]
    fn test_whitelist_manager_add_remove() {
        let mut wm = WhitelistManager::new();
        wm.remove_tool(&AgentRole::Plan, "file_read");
        assert!(!wm.check_permission(&AgentRole::Plan, "file_read"));
        wm.add_tool(AgentRole::Plan, "file_read");
        assert!(wm.check_permission(&AgentRole::Plan, "file_read"));
        wm.add_tool(AgentRole::Plan, "custom_new_tool");
        assert!(!wm.check_permission(&AgentRole::Plan, "custom_new_tool"));
    }

    #[test]
    fn test_list_allowed_tools() {
        let wm = WhitelistManager::new();
        let plan_tools = wm.list_allowed_tools(&AgentRole::Plan);
        assert!(plan_tools.contains(&"file_read".to_string()));
        assert!(!plan_tools.contains(&"file_write".to_string()));
    }
}

#[cfg(test)]
mod tests_5w2h {
    use super::*;
    use crate::core::five_w2h::*;

    fn make_gate() -> SyscallGate {
        SyscallGate::new(Arc::new(SkillRegistry::new()), 2048)
    }

    #[test]
    fn test_5w2h_forbidden_tools_constraint() {
        let gate = make_gate();
        let w2h = Task5W2H::new("Restricted task", "Test constraints").with_how(HowDetail {
            plan_iri: None,
            preferred_skills: vec![],
            forbidden_tools: vec!["bash".to_string(), "file_delete".to_string()],
            required_steps: None,
            dependencies: vec![],
        });
        assert!(gate.check_5w2h_constraints("file_read", Some(&w2h)).is_ok());
        assert!(gate.check_5w2h_constraints("bash", Some(&w2h)).is_err());
        assert!(gate.check_5w2h_constraints("bash", Some(&w2h)).is_err());
        assert!(gate
            .check_5w2h_constraints("file_delete", Some(&w2h))
            .is_err());
    }

    #[test]
    fn test_5w2h_access_level_read_constraint() {
        let gate = make_gate();
        let w2h = Task5W2H::new("Read-only task", "Test access control").with_who(WhoDetail {
            requestor: None,
            assignees: vec![],
            stakeholders: vec![],
            required_role: None,
            access_level: Some(AccessLevel::Read),
        });
        assert!(gate.check_5w2h_constraints("file_read", Some(&w2h)).is_ok());
        assert!(gate
            .check_5w2h_constraints("file_write", Some(&w2h))
            .is_err());
        assert!(gate.check_5w2h_constraints("bash", Some(&w2h)).is_err());
        assert!(gate
            .check_5w2h_constraints("code_execute", Some(&w2h))
            .is_err());
    }

    #[test]
    fn test_5w2h_access_level_write_allowed() {
        let gate = make_gate();
        let w2h = Task5W2H::new("Write task", "Test write permission").with_who(WhoDetail {
            requestor: None,
            assignees: vec![],
            stakeholders: vec![],
            required_role: None,
            access_level: Some(AccessLevel::Write),
        });
        assert!(gate.check_5w2h_constraints("file_read", Some(&w2h)).is_ok());
        assert!(gate
            .check_5w2h_constraints("file_write", Some(&w2h))
            .is_ok());
    }

    /// #270-1: an empty or unknown role no longer skips the role whitelist.
    #[test]
    fn syscall_gate_fails_closed_without_trusted_role() {
        let gate = make_gate();
        for role in ["", "unknown", "system"] {
            assert!(
                gate.validate_tool_with_5w2h("file_read", role, None)
                    .is_err(),
                "{role:?}"
            );
            assert!(
                gate.validate_tool_with_5w2h("bash", role, None).is_err(),
                "{role:?}"
            );
            let policy = crate::core::tool_policy::ToolPolicy::new();
            assert!(
                gate.validate_tool_for_run("file_read", role, "agent", Some(&policy), false, None)
                    .is_err(),
                "{role:?}"
            );
        }
        assert!(gate
            .validate_tool_with_5w2h("file_read", "Plan", None)
            .is_ok());
        assert!(gate.validate_tool_with_5w2h("bash", "Plan", None).is_err());
    }

    /// The runtime gate uses the caller's run-local policy and fails closed
    /// when there is none (安野's #272 note on the default `ToolPolicy::new()`).
    #[test]
    fn syscall_gate_uses_run_local_policy_and_fails_closed_without_it() {
        let gate = make_gate();
        assert!(gate
            .validate_tool_for_run("file_read", "Do", "agent", None, false, None)
            .is_err());

        let mut policy = crate::core::tool_policy::ToolPolicy::new();
        assert!(gate
            .validate_tool_for_run("file_write", "Do", "agent", Some(&policy), false, None)
            .is_ok());
        policy.restrict_tools("agent", ["file_read".to_string()]);
        // The default whitelist would allow Do file_write; the narrowed run must not.
        assert!(gate
            .validate_tool_with_5w2h("file_write", "Do", None)
            .is_ok());
        assert!(gate
            .validate_tool_for_run("file_write", "Do", "agent", Some(&policy), false, None)
            .is_err());
        assert!(gate
            .validate_tool_for_run("file_read", "Do", "agent", Some(&policy), false, None)
            .is_ok());
        // Another agent in the same run is not affected by this restriction.
        assert!(gate
            .validate_tool_for_run("file_write", "Do", "other", Some(&policy), false, None)
            .is_ok());

        // Server opt-in carried by the run policy (Check bash) is honoured,
        // which the default whitelist cannot express.
        let check_bash = crate::core::tool_policy::ToolPolicy::new().with_check_bash_enabled(true);
        assert!(gate.validate_tool_with_5w2h("bash", "Check", None).is_err());
        assert!(gate
            .validate_tool_for_run("bash", "Check", "agent", Some(&check_bash), false, None)
            .is_ok());
    }

    /// #270-2 at the gate: a micro-tool name passes only when the executor
    /// vouches that it is an internal reader.
    #[test]
    fn syscall_gate_prefix_rule_only_for_internal_micro_tools() {
        let gate = make_gate();
        let policy = crate::core::tool_policy::ToolPolicy::new();
        for role in ["Check", "Act"] {
            assert!(
                gate.validate_tool_for_run(
                    "query_orders",
                    role,
                    "agent",
                    Some(&policy),
                    false,
                    None
                )
                .is_err(),
                "{role}"
            );
            assert!(
                gate.validate_tool_for_run(
                    "query_orders",
                    role,
                    "agent",
                    Some(&policy),
                    true,
                    None
                )
                .is_ok(),
                "{role}"
            );
        }
        assert!(gate
            .validate_tool_for_run("query_orders", "Plan", "agent", Some(&policy), true, None)
            .is_err());
    }

    #[test]
    fn test_5w2h_no_snapshot_passes() {
        let gate = make_gate();
        assert!(gate.check_5w2h_constraints("bash", None).is_ok());
        assert!(gate.check_5w2h_constraints("file_write", None).is_ok());
    }
}
