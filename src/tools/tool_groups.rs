use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolGroup {
    Core,
    Workspace,
    Write,
    Search,
    Web,
    KnowledgeRead,
    KnowledgePlan,
    KnowledgeWrite,
    Ingest,
    Skill,
    Ontology,
    System,
}

impl std::fmt::Display for ToolGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolGroup::Core => write!(f, "Core"),
            ToolGroup::Workspace => write!(f, "Workspace"),
            ToolGroup::Write => write!(f, "Write"),
            ToolGroup::Search => write!(f, "Search"),
            ToolGroup::Web => write!(f, "Web"),
            ToolGroup::KnowledgeRead => write!(f, "KnowledgeRead"),
            ToolGroup::KnowledgePlan => write!(f, "KnowledgePlan"),
            ToolGroup::KnowledgeWrite => write!(f, "KnowledgeWrite"),
            ToolGroup::Ingest => write!(f, "Ingest"),
            ToolGroup::Skill => write!(f, "Skill"),
            ToolGroup::Ontology => write!(f, "Ontology"),
            ToolGroup::System => write!(f, "System"),
        }
    }
}

impl std::str::FromStr for ToolGroup {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "core" => Ok(ToolGroup::Core),
            "workspace" => Ok(ToolGroup::Workspace),
            "write" => Ok(ToolGroup::Write),
            "search" => Ok(ToolGroup::Search),
            "web" => Ok(ToolGroup::Web),
            "knowledgeread" | "knowledge_read" => Ok(ToolGroup::KnowledgeRead),
            "knowledgeplan" | "knowledge_plan" => Ok(ToolGroup::KnowledgePlan),
            "knowledgewrite" | "knowledge_write" => Ok(ToolGroup::KnowledgeWrite),
            "ingest" => Ok(ToolGroup::Ingest),
            "skill" => Ok(ToolGroup::Skill),
            "ontology" => Ok(ToolGroup::Ontology),
            "system" => Ok(ToolGroup::System),
            _ => Err(format!("Unknown tool group: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoleToolConfig {
    #[serde(default)]
    pub default: Vec<String>,
    #[serde(default)]
    pub on_demand: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGroupSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub roles: HashMap<String, RoleToolConfig>,
    #[serde(default = "default_max_tools_per_activation")]
    pub max_tools_per_activation: usize,
    #[serde(default = "default_max_activated_tools")]
    pub max_activated_tools: usize,
}

fn default_true() -> bool {
    true
}

fn default_max_tools_per_activation() -> usize {
    5
}

fn default_max_activated_tools() -> usize {
    20
}

impl Default for ToolGroupSettings {
    fn default() -> Self {
        let mut roles = HashMap::new();

        roles.insert(
            "Plan".to_string(),
            RoleToolConfig {
                default: vec![
                    "Core".to_string(),
                    "Search".to_string(),
                    "KnowledgePlan".to_string(),
                    "System".to_string(),
                ],
                on_demand: vec!["Web".to_string()],
            },
        );

        roles.insert(
            "Do".to_string(),
            RoleToolConfig {
                default: vec![
                    "Core".to_string(),
                    "Workspace".to_string(),
                    "Write".to_string(),
                    "Search".to_string(),
                    "System".to_string(),
                ],
                on_demand: vec![
                    "Web".to_string(),
                    "KnowledgePlan".to_string(),
                    "KnowledgeRead".to_string(),
                    "KnowledgeWrite".to_string(),
                    "Ingest".to_string(),
                    "Skill".to_string(),
                    "Ontology".to_string(),
                ],
            },
        );

        roles.insert(
            "Check".to_string(),
            RoleToolConfig {
                default: vec![
                    "Core".to_string(),
                    "Search".to_string(),
                    "System".to_string(),
                ],
                on_demand: vec![
                    "Workspace".to_string(),
                    "Write".to_string(),
                    "Web".to_string(),
                    "KnowledgePlan".to_string(),
                    "KnowledgeRead".to_string(),
                    "KnowledgeWrite".to_string(),
                    "Ingest".to_string(),
                    "Skill".to_string(),
                    "Ontology".to_string(),
                ],
            },
        );

        roles.insert(
            "Act".to_string(),
            RoleToolConfig {
                default: vec!["Core".to_string(), "System".to_string()],
                on_demand: vec![
                    "Search".to_string(),
                    "KnowledgePlan".to_string(),
                    "KnowledgeRead".to_string(),
                ],
            },
        );

        Self {
            enabled: true,
            roles,
            max_tools_per_activation: default_max_tools_per_activation(),
            max_activated_tools: default_max_activated_tools(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolGroupManager {
    settings: ToolGroupSettings,
    group_tools: HashMap<ToolGroup, HashSet<String>>,
}

/// Per-run, append-only on-demand exposure state. It is deliberately owned by
/// an AgentRunner loop and never by the shared ToolExecutor.
#[derive(Debug, Clone, Default)]
pub struct ActivatedTools {
    tools: Vec<String>,
    max_per_activation: usize,
    max_total: usize,
    activation_events: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationResult {
    pub activated: Vec<String>,
    pub skipped: Vec<String>,
}

impl ActivatedTools {
    pub fn new(max_per_activation: usize, max_total: usize) -> Self {
        Self {
            tools: Vec::new(),
            max_per_activation,
            max_total,
            activation_events: 0,
        }
    }

    pub fn activate(
        &mut self,
        candidates: &[String],
        allowed: &HashSet<String>,
    ) -> ActivationResult {
        let mut activated = Vec::new();
        let mut skipped = Vec::new();
        for name in candidates {
            if !allowed.contains(name) || self.tools.contains(name) {
                continue;
            }
            if activated.len() == self.max_per_activation || self.tools.len() == self.max_total {
                skipped.push(name.clone());
                continue;
            }
            self.tools.push(name.clone());
            activated.push(name.clone());
        }
        if !activated.is_empty() {
            self.activation_events += 1;
        }
        ActivationResult { activated, skipped }
    }

    pub fn names(&self) -> &[String] {
        &self.tools
    }

    pub fn activation_events(&self) -> usize {
        self.activation_events
    }
}

impl ToolGroupManager {
    pub fn new(settings: Option<ToolGroupSettings>) -> Self {
        let settings = settings.unwrap_or_default();
        let group_tools = Self::build_group_tools();

        Self {
            settings,
            group_tools,
        }
    }

    fn build_group_tools() -> HashMap<ToolGroup, HashSet<String>> {
        let mut map = HashMap::new();

        map.insert(
            ToolGroup::Core,
            HashSet::from(["file_read".to_string(), "file_list".to_string()]),
        );
        map.insert(
            ToolGroup::Workspace,
            HashSet::from([
                "workspace_status".to_string(),
                "read_agent_output".to_string(),
            ]),
        );

        map.insert(
            ToolGroup::Write,
            HashSet::from([
                "file_write".to_string(),
                "bash".to_string(),
                "powershell".to_string(),
                "file_edit".to_string(),
            ]),
        );

        map.insert(
            ToolGroup::Web,
            HashSet::from(["web_search".to_string(), "web_fetch".to_string()]),
        );

        map.insert(
            ToolGroup::Search,
            HashSet::from([
                "grep_search".to_string(),
                "glob_search".to_string(),
                "rag_search".to_string(),
                "kg_search".to_string(),
            ]),
        );

        map.insert(
            ToolGroup::KnowledgeRead,
            HashSet::from([
                "knowledge_query".to_string(),
                "knowledge_neighbors".to_string(),
                "kb_vector_search".to_string(),
            ]),
        );
        map.insert(
            ToolGroup::KnowledgePlan,
            HashSet::from([
                "knowledge_list".to_string(),
                "knowledge_search".to_string(),
                "knowledge_extract_code".to_string(),
            ]),
        );

        map.insert(
            ToolGroup::KnowledgeWrite,
            HashSet::from([
                "knowledge_update".to_string(),
                "knowledge_delete".to_string(),
                "knowledge_extract".to_string(),
                "knowledge_bridge".to_string(),
            ]),
        );
        map.insert(
            ToolGroup::Ingest,
            HashSet::from([
                "rag_index".to_string(),
                "rag_chunk".to_string(),
                "knowledge_import_file".to_string(),
                "knowledge_import_url".to_string(),
                "knowledge_import_directory".to_string(),
                "knowledge_import_json".to_string(),
            ]),
        );

        map.insert(
            ToolGroup::Skill,
            HashSet::from(["create_skill".to_string(), "convert_skill".to_string()]),
        );

        map.insert(
            ToolGroup::System,
            HashSet::from(["tool_search".to_string()]),
        );
        let mut ontology = HashSet::from([
            "ontology_register".to_string(),
            "ontology_validate_turtle".to_string(),
            "ontology_lint_turtle".to_string(),
            "ontology_diff_turtle".to_string(),
            "ontology_validate_shacl".to_string(),
            "ontology_reason".to_string(),
        ]);
        if !cfg!(feature = "ontology") {
            ontology.retain(|name| name == "ontology_register");
        }
        map.insert(ToolGroup::Ontology, ontology);

        map
    }

    pub fn get_groups_for_role(&self, role: &str) -> (Vec<ToolGroup>, Vec<ToolGroup>) {
        let role_config = self.settings.roles.get(role);

        match role_config {
            Some(config) => {
                let default: Vec<ToolGroup> = config
                    .default
                    .iter()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let on_demand: Vec<ToolGroup> = config
                    .on_demand
                    .iter()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                (default, on_demand)
            }
            None => (vec![ToolGroup::Core, ToolGroup::System], vec![]),
        }
    }

    pub fn get_tools_for_groups(&self, groups: &[ToolGroup]) -> HashSet<String> {
        let mut tools = HashSet::new();
        for group in groups {
            if let Some(group_tools) = self.group_tools.get(group) {
                tools.extend(group_tools.clone());
            }
        }
        tools
    }

    pub fn grouped_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .group_tools
            .values()
            .flat_map(|tools| tools.iter().cloned())
            .collect();
        names.sort();
        names
    }

    pub fn get_tool_names_for_role(&self, role: &str) -> (HashSet<String>, HashSet<String>) {
        let (default_groups, on_demand_groups) = self.get_groups_for_role(role);
        let default_tools = self.get_tools_for_groups(&default_groups);
        let on_demand_tools = self.get_tools_for_groups(&on_demand_groups);
        (default_tools, on_demand_tools)
    }

    pub fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    pub fn activated_tools(&self) -> ActivatedTools {
        ActivatedTools::new(
            self.settings.max_tools_per_activation,
            self.settings.max_activated_tools,
        )
    }

    pub fn build_tool_summary(&self, role: &str, registered_tools: &[String]) -> String {
        if !self.settings.enabled {
            return String::new();
        }
        let (_, on_demand) = self.get_groups_for_role(role);
        let descriptions = [
            (ToolGroup::Web, "find or fetch public web content"),
            (
                ToolGroup::KnowledgeRead,
                "query imported knowledge and graph data",
            ),
            (
                ToolGroup::KnowledgeWrite,
                "update or enrich knowledge graph data",
            ),
            (
                ToolGroup::Ingest,
                "index or import documents and structured data",
            ),
            (ToolGroup::Skill, "create or convert skill definitions"),
            (
                ToolGroup::Ontology,
                "register, validate, compare, or reason over ontology data",
            ),
            (ToolGroup::Search, "search files and indexed documents"),
        ];
        let available: HashSet<&str> = registered_tools.iter().map(String::as_str).collect();
        let entries: Vec<String> = descriptions
            .iter()
            .filter(|(group, _)| on_demand.contains(group))
            .filter(|(group, _)| {
                self.get_tools_for_groups(&[*group])
                    .iter()
                    .any(|name| available.contains(name.as_str()))
            })
            .map(|(group, description)| {
                format!("- {group}: {description}; use tool_search to load relevant tools.")
            })
            .collect();
        if entries.is_empty() {
            String::new()
        } else {
            format!("## On-demand tool groups\n{}", entries.join("\n"))
        }
    }

    pub fn is_tool_available_for_role(&self, role: &str, tool_name: &str) -> bool {
        let (default_tools, on_demand_tools) = self.get_tool_names_for_role(role);
        default_tools.contains(tool_name) || on_demand_tools.contains(tool_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_tool_group_from_str() {
        assert_eq!(ToolGroup::from_str("Core").unwrap(), ToolGroup::Core);
        assert_eq!(ToolGroup::from_str("core").unwrap(), ToolGroup::Core);
        assert_eq!(ToolGroup::from_str("SEARCH").unwrap(), ToolGroup::Search);
        assert!(ToolGroup::from_str("Unknown").is_err());
    }

    #[test]
    fn test_default_settings() {
        let settings = ToolGroupSettings::default();
        assert!(settings.enabled);
        assert!(settings.roles.contains_key("Plan"));
        assert!(settings.roles.contains_key("Do"));
        assert!(settings.roles.contains_key("Check"));
        assert!(settings.roles.contains_key("Act"));
    }

    #[test]
    fn test_get_groups_for_plan() {
        let manager = ToolGroupManager::new(None);
        let (default, on_demand) = manager.get_groups_for_role("Plan");

        assert!(default.contains(&ToolGroup::Core));
        assert!(default.contains(&ToolGroup::Search));
        assert!(default.contains(&ToolGroup::KnowledgePlan));
        assert!(default.contains(&ToolGroup::System));
        assert!(!default.contains(&ToolGroup::Web));

        assert!(on_demand.contains(&ToolGroup::Web));
        assert!(!on_demand.contains(&ToolGroup::KnowledgeWrite));
    }

    #[test]
    fn test_get_tools_for_groups() {
        let manager = ToolGroupManager::new(None);
        let tools = manager.get_tools_for_groups(&[ToolGroup::Core, ToolGroup::Web]);

        assert!(tools.contains("file_read"));
        assert!(!tools.contains("file_write")); // file_write is in Write group
        assert!(tools.contains("web_search"));
        assert!(tools.contains("web_fetch"));
        assert!(!tools.contains("knowledge_query"));
    }

    #[test]
    fn test_write_group() {
        let manager = ToolGroupManager::new(None);
        let tools = manager.get_tools_for_groups(&[ToolGroup::Write]);

        assert!(tools.contains("file_write"));
        assert!(tools.contains("bash"));
        assert!(tools.contains("powershell"));
        assert!(!tools.contains("file_read"));
    }

    #[test]
    fn test_is_tool_available_for_role() {
        let manager = ToolGroupManager::new(None);

        assert!(manager.is_tool_available_for_role("Plan", "file_read"));
        assert!(manager.is_tool_available_for_role("Plan", "web_search"));
        assert!(!manager.is_tool_available_for_role("Plan", "bash"));

        assert!(manager.is_tool_available_for_role("Do", "bash"));
        assert!(manager.is_tool_available_for_role("Do", "web_search"));
    }

    #[test]
    fn builtin_group_table_matches_registered_tools_exactly_once() {
        let manager = ToolGroupManager::new(None);
        let executor = crate::tools::tool_executor::ToolExecutor::new();
        let mut registered = executor.registered_tool_names();
        registered.sort();
        assert_eq!(manager.grouped_tool_names(), registered);
    }

    #[test]
    fn plan_groups_are_read_only() {
        let manager = ToolGroupManager::new(None);
        let (resident, on_demand) = manager.get_tool_names_for_role("Plan");
        for name in resident.union(&on_demand) {
            assert!(
                crate::tools::tool_executor::ToolExecutor::is_pa_readonly_tool(name),
                "{name} must not be visible to Plan"
            );
        }
    }

    #[test]
    fn plan_can_reach_exactly_main_readonly_tools() {
        let manager = ToolGroupManager::new(None);
        let (resident, on_demand) = manager.get_tool_names_for_role("Plan");
        let mut visible: Vec<String> = resident.union(&on_demand).cloned().collect();
        visible.sort();
        let mut expected = crate::tools::tool_executor::ToolExecutor::pa_readonly_tools()
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(visible, expected);
    }

    #[test]
    fn role_groups_preserve_main_fallback_reachability() {
        let manager = ToolGroupManager::new(None);
        let executor = crate::tools::tool_executor::ToolExecutor::new();
        let registered = executor.registered_tool_names();
        for role in ["Plan", "Do", "Check", "Act"] {
            let (resident, on_demand) = manager.get_tool_names_for_role(role);
            let reachable = resident.union(&on_demand).cloned().collect::<HashSet<_>>();
            let expected: HashSet<String> = match role {
                "Plan" => crate::tools::tool_executor::ToolExecutor::pa_readonly_tools()
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
                "Act" => [
                    "file_read",
                    "file_list",
                    "tool_search",
                    "grep_search",
                    "glob_search",
                    "rag_search",
                    "kg_search",
                    "knowledge_list",
                    "knowledge_search",
                    "knowledge_extract_code",
                ]
                .iter()
                .map(|name| name.to_string())
                .collect(),
                "Do" | "Check" => registered.iter().cloned().collect(),
                _ => unreachable!(),
            };
            assert!(
                expected.is_subset(&reachable),
                "{role} lost fallback reachability"
            );
        }
    }

    #[test]
    fn activation_is_ordered_and_limited() {
        let mut activated = ActivatedTools::new(2, 3);
        let allowed = HashSet::from([
            "web_fetch".to_string(),
            "web_search".to_string(),
            "knowledge_search".to_string(),
        ]);
        assert_eq!(
            activated
                .activate(
                    &["web_fetch".to_string(), "web_search".to_string()],
                    &allowed
                )
                .activated,
            vec!["web_fetch", "web_search"]
        );
        assert_eq!(
            activated
                .activate(
                    &["web_fetch".to_string(), "knowledge_search".to_string()],
                    &allowed
                )
                .activated,
            vec!["knowledge_search"]
        );
        assert_eq!(
            activated.names(),
            &["web_fetch", "web_search", "knowledge_search"]
        );
    }
}
