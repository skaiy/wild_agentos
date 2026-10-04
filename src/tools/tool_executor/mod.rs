use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tracing::{debug, warn};

use crate::core::agent_instance::AgentRole;
use crate::core::tool_policy::ToolPolicy;
use crate::isolation::IsolationClaims;
use crate::knowledge_graph::store::KnowledgeGraphStore;
use crate::skill_graph::security::{SecurityContext, SecurityDecision, SecurityEngine};
use crate::tools::builtin::hooks::HookRunner;
use crate::tools::builtin::knowledge;
#[cfg(feature = "ontology")]
use crate::tools::builtin::ontology_tools;
use crate::tools::builtin::permissions::{PermissionMode, PermissionOutcome, PermissionPolicy};
use crate::tools::builtin::rag;
use crate::tools::skill_registry::SkillRegistry;
use crate::tools::tool_groups::{ActivatedTools, ToolGroupManager};
use crate::tools::workspace_monitor::{FileState, WorkspaceMonitor};

mod builtins;
pub(crate) mod tool_description_lint;

#[cfg(test)]
mod tests;

tokio::task_local! {
    /// Verified identity supplied by the runtime boundary for one tool call.
    ///
    /// This is task-local rather than executor state: a shared executor can
    /// serve concurrent tenants without one call's identity leaking into
    /// another call.
    static TOOL_ISOLATION_CLAIMS: Option<IsolationClaims>;
}

tokio::task_local! {
    /// The role is supplied exclusively by the runtime security boundary.
    ///
    /// Keeping it task-local prevents model-controlled tool arguments from
    /// selecting another role and keeps concurrent runs isolated.
    static TOOL_SEARCH_CALLER_ROLE: Option<String>;
}

tokio::task_local! {
    /// Runtime identity for legacy syscall-gate checks.
    static TOOL_CALLER_CONTEXT: Option<SecurityContext>;
}

tokio::task_local! {
    /// Caller-owned run policy used by tool discovery during one invocation.
    static TOOL_RUN_POLICY: Option<ToolPolicy>;
}

pub(super) fn require_isolation_claims() -> Result<IsolationClaims, String> {
    TOOL_ISOLATION_CLAIMS
        .try_with(|claims| claims.clone())
        .ok()
        .flatten()
        .ok_or_else(|| {
            "verified isolation claims are required for graph and vector tools".to_string()
        })
}

/// Tool input structs
#[derive(Debug, Deserialize)]
pub struct GlobSearchInput {
    pub pattern: String,
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GrepSearchInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub output_mode: Option<String>,
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub context: Option<usize>,
    pub line_numbers: Option<bool>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    pub multiline: Option<bool>,
    pub file_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebFetchInput {
    pub url: String,
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebSearchInput {
    pub query: String,
    pub allowed_domains: Option<Vec<String>>,
    pub blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct ToolSearchInput {
    pub query: String,
    pub max_results: Option<usize>,
}
type ToolFn =
    Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>> + Send + Sync>;

/// Stable error contract for callers of the tool-execution boundary.
///
/// Individual built-ins still supply their detailed failure text internally;
/// this type prevents those implementation details from becoming the public
/// error API and lets callers distinguish a missing tool from its failure.
#[derive(Debug, Error, PartialEq, Eq, serde::Serialize)]
pub enum ToolExecutionError {
    #[error("tool not found: {name}")]
    NotFound { name: String },

    #[error("tool '{name}' failed: {message}")]
    ExecutionFailed { name: String, message: String },
}

/// Wrap a synchronous tool function (takes &Value) as an async ToolFn
fn sync_tool_ref<F>(f: F) -> ToolFn
where
    F: Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
{
    let f = Arc::new(f);
    Arc::new(move |input| {
        let f = Arc::clone(&f);

        Box::pin(async move { f(&input) })
    })
}

/// Micro-tool context
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MicroToolContext {
    pub call_id: String,
    pub storage_key: String,
    pub tool_name: String,
    pub entity_types: Vec<String>,
    pub preview_size: usize,
}

/// Unified tool executor with built-in tools
#[derive(Clone)]
pub struct ToolExecutor {
    tools: HashMap<String, ToolFn>,
    tool_descriptions: Vec<ToolDescription>,
    description_indices: HashMap<String, usize>,
    micro_description_order: VecDeque<String>,
    builtin_tool_names: BTreeSet<String>,
    registering_builtins: bool,
    kg_store: Arc<std::sync::RwLock<KnowledgeGraphStore>>,
    projection_engine:
        Arc<parking_lot::RwLock<Option<Arc<crate::memory::l3_projection::ProjectionEngine>>>>,
    micro_tool_contexts: Arc<parking_lot::RwLock<HashMap<String, MicroToolContext>>>,
    micro_tool_data: Arc<parking_lot::RwLock<HashMap<String, serde_json::Value>>>,
    syscall_gate: Option<crate::core::syscall_gate::SyscallGate>,
    permission_policy: Option<PermissionPolicy>,
    hook_runner: Option<HookRunner>,
    tool_group_manager: Option<ToolGroupManager>,
    workspace_monitor: Arc<parking_lot::RwLock<Option<Arc<WorkspaceMonitor>>>>,
    /// 向量知识库（HyperspaceStore）：注入后 `kb_vector_search` 工具可做语义召回；None 时该工具返回空。
    vector_store: Arc<parking_lot::RwLock<Option<Arc<crate::memory::HyperspaceStore>>>>,
    /// 注入后 `execute_with_security_context` 会对每次调用做 SkillGraph 安全判定；None 时该层跳过。
    security_engine: Arc<parking_lot::RwLock<Option<Arc<SecurityEngine>>>>,
    /// 运行期技能注册表：把工具名解析为规范 skill IRI，供安全门查询图上策略。
    shared_skill_registry: Arc<parking_lot::RwLock<Option<Arc<SkillRegistry>>>>,
}

// Max micro-tool descriptions cap — removes oldest entries when exceeded.
// Prevents tool_descriptions from inflating indefinitely, avoiding thousands of token overhead per LLM request.
const MAX_MICRO_TOOL_DESCRIPTIONS: usize = 5;
const MICRO_TOOL_PREFIXES: &[&str] = &[
    "read_full_result_",
    "query_",
    "get_entity_details",
    "expand_relation",
];

/// Built-ins that are executor capabilities rather than independently
/// registered skills inherit a reviewed least-privilege SkillGraph policy.
/// Keep this table explicit: an unknown tool must still fail closed.
fn builtin_security_skill_iri(name: &str) -> Option<&'static str> {
    match name {
        "tool_search"
        | "glob_search"
        | "grep_search"
        | "file_read"
        | "file_list"
        | "workspace_status"
        | "rag_search"
        | "kg_search"
        | "kb_vector_search"
        | "knowledge_list"
        | "knowledge_search"
        | "knowledge_extract_code"
        | "knowledge_query"
        | "knowledge_neighbors"
        | "read_agent_output"
        | "read_full_result"
        | "get_entity_details"
        | "expand_relation"
        | "ontology_lint_turtle"
        | "ontology_diff_turtle"
        | "ontology_validate_turtle"
        | "ontology_validate_shacl"
        | "ontology_reason" => Some("iri://skills/file_read"),
        "bash"
        | "powershell"
        | "file_write"
        | "file_edit"
        | "rag_index"
        | "rag_chunk"
        | "knowledge_extract"
        | "knowledge_update"
        | "knowledge_delete"
        | "knowledge_bridge"
        | "knowledge_import_file"
        | "knowledge_import_directory"
        | "knowledge_import_json"
        | "ontology_register" => Some("iri://skills/file_write"),
        "web_search" | "web_fetch" | "http_request" | "knowledge_import_url" => {
            Some("iri://skills/http_request")
        }
        "llm_chat" => Some("iri://skills/llm_chat"),
        _ => None,
    }
}

/// Tool role filter: empty = all roles, "PA"/"DA"/"CA"/"AA" = role-specific only
#[derive(Clone)]
pub struct ToolDescription {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub allowed_roles: Vec<String>, // empty = all roles allowed
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolExecutor {
    pub fn new() -> Self {
        let kg_store = Arc::new(std::sync::RwLock::new(
            KnowledgeGraphStore::new().expect("Failed to create knowledge graph store"),
        ));
        let mut exe = Self {
            tools: HashMap::new(),
            tool_descriptions: Vec::new(),
            description_indices: HashMap::new(),
            micro_description_order: VecDeque::new(),
            builtin_tool_names: BTreeSet::new(),
            registering_builtins: false,
            kg_store,
            projection_engine: Arc::new(parking_lot::RwLock::new(None)),
            micro_tool_contexts: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            micro_tool_data: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            syscall_gate: None,
            permission_policy: None,
            hook_runner: None,
            tool_group_manager: None,
            workspace_monitor: Arc::new(parking_lot::RwLock::new(None)),
            vector_store: Arc::new(parking_lot::RwLock::new(None)),
            security_engine: Arc::new(parking_lot::RwLock::new(None)),
            shared_skill_registry: Arc::new(parking_lot::RwLock::new(None)),
        };
        exe.register_builtins();
        exe
    }

    pub fn set_projection_engine(
        &mut self,
        engine: Arc<crate::memory::l3_projection::ProjectionEngine>,
    ) {
        *self.projection_engine.write() = Some(engine);
    }

    pub fn set_tool_group_manager(&mut self, manager: ToolGroupManager) {
        self.tool_group_manager = Some(manager);
    }

    pub fn has_tool_group_manager(&self) -> bool {
        self.tool_group_manager.is_some()
    }

    pub fn activated_tools(&self) -> ActivatedTools {
        self.tool_group_manager
            .as_ref()
            .filter(|manager| manager.is_enabled())
            .map(ToolGroupManager::activated_tools)
            .unwrap_or_default()
    }

    pub fn registered_tool_names(&self) -> Vec<String> {
        self.tool_descriptions
            .iter()
            .map(|description| description.name.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn builtin_tool_names(&self) -> &BTreeSet<String> {
        &self.builtin_tool_names
    }

    #[cfg(test)]
    pub(crate) fn builtin_tool_descriptions(&self) -> impl Iterator<Item = &ToolDescription> {
        self.tool_descriptions
            .iter()
            .filter(|tool| self.builtin_tool_names.contains(&tool.name))
    }

    /// Return the live registry's resident names after the same role policy
    /// used for a first-turn schema. The result is sorted for prompt-only
    /// planning preferences; it never authorizes a call.
    pub fn visible_tool_names_for_role(
        &self,
        role: &str,
        agent_id: &str,
        activated: &ActivatedTools,
    ) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_definitions_for_turn_with_policy(role, agent_id, activated)
            .iter()
            .filter_map(|definition| definition["function"]["name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    pub fn build_tool_group_summary(&self, role: &str) -> String {
        self.tool_group_manager
            .as_ref()
            .map(|manager| manager.build_tool_summary(role, &self.registered_tool_names()))
            .unwrap_or_default()
    }

    /// Replace internal KnowledgeGraphStore with a unified Oxigraph Store
    pub fn set_unified_kg_store(&mut self, store: Arc<oxigraph::store::Store>) {
        self.kg_store = Arc::new(std::sync::RwLock::new(
            KnowledgeGraphStore::with_shared_store(store)
                .expect("Failed to create shared KG Store"),
        ));
    }

    pub fn set_syscall_gate(&mut self, gate: crate::core::syscall_gate::SyscallGate) {
        self.syscall_gate = Some(gate);
    }

    pub fn set_permission_policy(&mut self, policy: PermissionPolicy) {
        self.permission_policy = Some(policy);
    }

    pub fn set_hook_runner(&mut self, runner: HookRunner) {
        self.hook_runner = Some(runner);
    }

    pub fn set_workspace_monitor(&mut self, monitor: Arc<WorkspaceMonitor>) {
        *self.workspace_monitor.write() = Some(monitor);
    }

    pub fn get_workspace_monitor(&self) -> Option<Arc<WorkspaceMonitor>> {
        self.workspace_monitor.read().clone()
    }

    /// Get a reference to the internal KnowledgeGraphStore for shared use
    /// (e.g. by FusedRootCauseEngine for SPARQL semantic neighbor traversal).
    pub fn knowledge_graph_store(&self) -> Arc<std::sync::RwLock<KnowledgeGraphStore>> {
        self.kg_store.clone()
    }

    /// 注入向量知识库，使 `kb_vector_search` 工具可对向量库做语义检索。
    pub fn set_vector_store(&mut self, store: Arc<crate::memory::HyperspaceStore>) {
        *self.vector_store.write() = Some(store);
    }

    /// Inject the SkillGraph security engine consulted by
    /// `execute_with_security_context`.
    pub fn set_security_engine(&self, engine: Arc<SecurityEngine>) {
        *self.security_engine.write() = Some(engine);
    }

    /// Inject the live registry used by planning and execution so the security
    /// gate resolves tool names against the canonical skill IRI namespace.
    pub fn set_shared_skill_registry(&self, registry: Arc<SkillRegistry>) {
        *self.shared_skill_registry.write() = Some(registry);
    }

    /// Notify workspace_monitor that a file was read externally (e.g., via read_full_result).
    /// This helps the cache/diff system recognize the file as already-read on subsequent file_read.
    pub fn mark_file_external_read(&self, path: &str) {
        let guard = self.workspace_monitor.read();
        if let Some(ref wm) = *guard {
            wm.mark_file_read_external(path);
        }
    }

    /// Default tool requirements: bash/pwsh/code_exec→DangerFullAccess, file_write/edit→WorkspaceWrite, reads→ReadOnly
    pub fn set_default_permission_policy(&mut self) {
        let policy = PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_tool_requirement("powershell", PermissionMode::DangerFullAccess)
            .with_tool_requirement("code_execute", PermissionMode::DangerFullAccess)
            .with_tool_requirement("file_write", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("file_edit", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("file_read", PermissionMode::ReadOnly)
            .with_tool_requirement("grep_search", PermissionMode::ReadOnly)
            .with_tool_requirement("glob_search", PermissionMode::ReadOnly)
            .with_tool_requirement("web_search", PermissionMode::ReadOnly)
            .with_tool_requirement("web_fetch", PermissionMode::ReadOnly);
        self.permission_policy = Some(policy);
    }

    fn register_builtins(&mut self) {
        self.registering_builtins = true;
        // All tools open to all roles; LLM selects based on role description in agent.md
        let all: &[&str] = &[];
        self.register(
            "glob_search",
            "Find file paths matching a glob in a directory. Use when: locating files by name or extension. Not for: searching file contents; use grep_search.",
            json!({
                "type": "object",
                "properties": {"pattern": {"type":"string","description":"File-name glob pattern, such as **/*.rs."},"path": {"type":"string","description":"Directory path to search; defaults to the current directory."}},
                "required": ["pattern"]
            }),
            Arc::new(|input: Value| {
                Box::pin(async move { builtins::execute_glob_search(input).await })
            }),
            all,
        );
        self.register("grep_search", "Search file contents with a regular expression and optional filters. Use when: finding matching lines in files. Not for: locating paths by glob alone; use glob_search.", json!({
            "type": "object",
            "properties": {
                "pattern": {"type":"string","description":"Regular expression to match against file contents."},
                "path": {"type":"string","description":"Directory path to search; defaults to the current directory."},
                "glob": {"type":"string","description":"File-name glob filter, such as *.rs."},
                "output_mode": {"type":"string","description":"Output mode: files_with_matches | content | count"},
                "before": {"type":"integer","description":"Lines before match (-B)"},
                "after": {"type":"integer","description":"Lines after match (-A)"},
                "context": {"type":"integer","description":"Context lines around match (-C)"},
                "line_numbers": {"type":"boolean","description":"Show line numbers (default true)"},
                "head_limit": {"type":"integer","description":"Limit number of results (default 250)"},
                "offset": {"type":"integer","description":"Skip first N results"},
                "-i": {"type":"boolean","description":"Case-insensitive search; default false."},
                "multiline": {"type":"boolean","description":"Match across lines; default false."},
                "file_type": {"type":"string","description":"File type filter (rust, python, etc.)"}
            },
            "required": ["pattern"]
        }), Arc::new(|input: Value| Box::pin(async move { builtins::execute_grep_search(input).await })), all);
        self.register(
            "web_fetch",
            "Fetch and extract readable text from one web page. Use when: the URL is already known and its content is needed. Not for: finding URLs; use web_search.",
            json!({
                "type": "object",
                "properties": {"url": {"type":"string","description":"Absolute HTTP or HTTPS URL to fetch."},"prompt": {"type":"string","description":"Optional question to focus the extracted page text."}},
                "required": ["url"]
            }),
            Arc::new(|input: Value| {
                Box::pin(async move { builtins::execute_web_fetch(input).await })
            }),
            all,
        );
        self.register(
            "web_search",
            "Search public web pages by query and return matching results. Use when: finding sources without a known URL. Not for: reading a known page; use web_fetch.",
            json!({
                "type": "object",
                "properties": {"query": {"type":"string","minLength":2,"description":"Search phrase of at least two characters."}},
                "required": ["query"]
            }),
            Arc::new(|input: Value| {
                Box::pin(async move { builtins::execute_web_search(input).await })
            }),
            all,
        );
        self.register(
            "tool_search",
            "Find role-visible tools by name, description, parameter, or group; returns ranked matches for later turns. Use when: discovering an on-demand capability. Not for: executing tools or changing permissions.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type":"string", "description":"Task or capability to find; include operation and relevant parameter terms."},
                    "max_results": {"type":"integer", "description":"Optional result count from 0 to 10; defaults to 5."}
                },
                "required": ["query"]
            }),
            Arc::new(|_| {
                Box::pin(async {
                    Err("tool_search must be dispatched through the runtime security context"
                        .to_string())
                })
            }),
            all,
        );
        let ws_read = self.workspace_monitor.clone();
        self.register("file_read", "Read text file lines, with optional offsets and workspace diff/cache modes on repeat reads. Use when: inspecting file content. Not for: listing directory entries; use file_list.", json!({
            "type": "object",
            "properties": {
                "path": {"type":"string", "description": "Path of the text file to read."},
                "offset": {"type":"integer", "description": "Line offset to start from (0-indexed). Omit to read from beginning."},
                "limit": {"type":"integer", "description": "Number of lines to return. Omit to read all remaining lines from offset."},
                "mode": {"type":"string", "description": "Read mode: auto (default=use diff if previously read) | full | force_refresh | diff | changed_only"}
            },
            "required": ["path"]
        }), Arc::new(move |input: Value| {
            let ws = ws_read.clone();
            Box::pin(async move {
                let mode = input.get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("auto")
                    .to_string();
                // Extract offset/limit before input is moved into execute_file_read
                let has_offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) > 0;
                let has_limit = input.get("limit").is_some();
                let result = builtins::execute_file_read(input).await?;
                let guard = ws.read();
                if let Some(ref wm) = *guard {
                    if let Some(path) = result.get("path").and_then(|v| v.as_str()) {
                        let read_mode = match mode.as_str() {
                            "force_refresh" => crate::tools::workspace_monitor::ReadMode::ForceRefresh,
                            "diff" => crate::tools::workspace_monitor::ReadMode::Diff,
                            "changed_only" => crate::tools::workspace_monitor::ReadMode::ChangedOnly,
                            _ => {
                                // auto: use diff if file is already cached, else full
                                let inv = wm.inventory.read();
                                let entry = inv.get_entry(path);
                                match entry {
                                    Some(e) if e.read_count > 0 => crate::tools::workspace_monitor::ReadMode::Diff,
                                    _ => crate::tools::workspace_monitor::ReadMode::Full,
                                }
                            }
                        };
                        if let Ok(read_result) = wm.read_file(path, read_mode) {
                                let mut result = result;
                                if let Some(diff) = &read_result.unified_diff {
                                    if let Some(obj) = result.as_object_mut() { obj.insert("unified_diff".to_string(), Value::String(diff.clone())); }
                                }
                                if let Some(changed) = &read_result.changed_lines {
                                    if let Some(obj) = result.as_object_mut() { obj.insert("changed_lines".to_string(), Value::Array(
                                            changed.iter().map(|l| Value::String(l.clone())).collect()
                                        )); }
                                }
                                if !read_result.changed && read_result.from_cache {
                                    // Cache hit: file unchanged since last read.
                                    // Strip full content to avoid token waste on re-read.
                                    if !has_offset && !has_limit {
                                        if let Some(obj) = result.as_object_mut() {
                                            obj.remove("lines");
                                            obj.remove("returned");
                                            obj.insert("from_cache".to_string(), Value::Bool(true));
                                            obj.insert("message".to_string(), Value::String(
                                                "Cache hit: file unchanged since last read. Content already in your context from a previous read — skip re-reading and proceed with what you have.".to_string()
                                            ));
                                        }
                                    } else if let Some(obj) = result.as_object_mut() {
                                        obj.insert("from_cache".to_string(), Value::Bool(true));
                                        obj.insert("message".to_string(), Value::String(
                                            "Cache hit: file unchanged since last read. Content already in your context — skip re-reading.".to_string()
                                        ));
                                    }
                                }
                                return Ok(result);
                            }
                        }
                    }
                Ok(result)
            })
        }), all);
        let ws_write = self.workspace_monitor.clone();
        self.register(
            "file_write",
            "Write the complete supplied content to a file, creating or replacing it. Use when: a whole file must be written. Not for: a targeted change in an existing file; use file_edit.",
            json!({
                "type": "object",
                "properties": {"path": {"type":"string","description":"Path of the file to create or replace."},"content": {"type":"string","description":"Complete text content to write."}},
                "required": ["path","content"]
            }),
            Arc::new(move |input: Value| {
                let ws = ws_write.clone();
                Box::pin(async move {
                    let result = builtins::execute_file_write(input).await?;
                    if result.get("success") == Some(&Value::Bool(true)) {
                        let guard = ws.read();
                        if let Some(ref wm) = *guard {
                            if let Some(path) = result.get("path").and_then(|v| v.as_str()) {
                                wm.mark_file_written(path);
                            }
                        }
                    }
                    Ok(result)
                })
            }),
            all,
        );
        let ws_status = self.workspace_monitor.clone();
        self.register("workspace_status", "Summarize tracked workspace files by state and language, including stale and unread writes. Use when: checking tracked file status. Not for: listing directory contents; use file_list.", json!({
            "type": "object",
            "properties": {},
            "required": []
        }), Arc::new(move |_: Value| {
            let ws = ws_status.clone();
            Box::pin(async move {
                let guard = ws.read();
                if let Some(ref wm) = *guard {
                    let inv = wm.inventory.read();
                        let all = inv.list_all();
                        let total = all.len();

                        let stale = inv.list_by_state(FileState::ReadStale);
                        let written_unread = inv.list_by_state(FileState::WrittenUnread);
                        let discovered = inv.list_by_state(FileState::Discovered);
                        let fresh = inv.list_by_state(FileState::ReadFresh);

                        // Group by language
                        let mut lang_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                        for entry in &all {
                            *lang_map.entry(entry.language.clone()).or_insert(0) += 1;
                        }
                        let mut by_language: Vec<serde_json::Value> = lang_map.into_iter()
                            .map(|(lang, count)| json!({"language": lang, "count": count}))
                            .collect();
                        by_language.sort_by(|a, b| {
                            b["count"].as_u64().unwrap_or(0).cmp(&a["count"].as_u64().unwrap_or(0))
                        });

                        return Ok(json!({
                            "total_files": total,
                            "stale_count": stale.len(),
                            "stale_files": stale.iter().take(20).map(|e| json!(e.path)).collect::<Vec<_>>(),
                            "written_unread_count": written_unread.len(),
                            "written_unread_files": written_unread.iter().take(20).map(|e| json!(e.path)).collect::<Vec<_>>(),
                            "discovered_count": discovered.len(),
                            "fresh_count": fresh.len(),
                            "by_language": by_language,
                        }));
                    }
                // Fallback if no workspace_monitor available
                Ok(json!({"total_files": 0, "stale_count": 0, "written_unread_count": 0, "message": "Workspace monitor not available"}))
            })
        }), all);
        let ws_list = self.workspace_monitor.clone();
        self.register(
            "file_list",
            "List entries in a directory and include tracked file state when available. Use when: browsing directory contents. Not for: reading file text; use file_read.",
            json!({
                "type": "object",
                "properties": {"path": {"type":"string","description":"Directory path to list; defaults to the current directory."}},
                "required": []
            }),
            Arc::new(move |input: Value| {
                let ws = ws_list.clone();
                Box::pin(async move {
                    let mut result = builtins::execute_file_list(input).await?;
                    let guard = ws.read();
                    if let Some(ref wm) = *guard {
                        if let Some(entries) =
                            result.get_mut("entries").and_then(|e| e.as_array_mut())
                        {
                            for entry in entries.iter_mut() {
                                let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let inv = wm.inventory.read();
                                if let Some(file_entry) = inv.get_entry(name) {
                                    if let Some(obj) = entry.as_object_mut() {
                                        obj.insert(
                                            "state".to_string(),
                                            Value::String(file_entry.state.as_str().to_string()),
                                        );
                                        obj.insert(
                                            "language".to_string(),
                                            Value::String(file_entry.language.clone()),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Ok(result)
                })
            }),
            all,
        );
        let bash_desc = if cfg!(target_os = "windows") {
            "Run a shell command through PowerShell with bounded output and optional background execution. Use when: running general commands. Not for: a specifically PowerShell command; use powershell."
        } else {
            "Run a shell command with bounded output and optional background execution. Use when: running scripts, builds, or tests. Not for: PowerShell syntax; use powershell."
        };
        self.register("bash", bash_desc, json!({
            "type": "object",
            "properties": {
                "command": {"type":"string","description":"Shell command to run"},
                "description": {"type":"string","description":"What this command does"},
                "timeout": {"type":"integer","description":"Timeout in milliseconds"},
                "run_in_background": {"type":"boolean","description":"Spawn detached and return a task id immediately (default false)"},
                "dangerouslyDisableSandbox": {"type":"boolean","default":false,"description":"Unsupported; requests without an active sandbox are rejected"},
                "namespaceRestrictions": {"type":"boolean","description":"Enable user/mount/pid namespace isolation via unshare (default true when sandbox enabled)"},
                "isolateNetwork": {"type":"boolean","description":"Isolate network via a new network namespace (default false)"},
                "filesystemMode": {"type":"string","enum":["off","workspace-only","allow-list"],"description":"Filesystem isolation level (default workspace-only)"},
                "allowedMounts": {"type":"array","items":{"type":"string"},"description":"Additional paths allowed when filesystemMode is allow-list"}
            },
            "required": ["command"]
        }), Arc::new(|input: Value| Box::pin(async move { builtins::execute_bash(input).await })), all);
        let ws_edit = self.workspace_monitor.clone();
        self.register("file_edit", "Replace matching text in an existing file, optionally replacing all occurrences. Use when: making a targeted change. Not for: creating or replacing a whole file; use file_write.", json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Path of the existing file to edit."},
                "old_string": {"type":"string","description":"Text to find and replace"},
                "new_string": {"type":"string","description":"Replacement text"},
                "replace_all": {"type":"boolean","description":"Replace all occurrences (default: false)"}
            },
            "required": ["path","old_string","new_string"]
        }), Arc::new(move |input: Value| {
            let ws = ws_edit.clone();
            Box::pin(async move {
                let result = builtins::execute_file_edit(input).await?;
                if result.get("success") == Some(&Value::Bool(true)) {
                    let guard = ws.read();
                    if let Some(ref wm) = *guard {
                        if let Some(path) = result.get("path").and_then(|v| v.as_str()) {
                            wm.mark_file_written(path);
                        }
                    }
                }
                Ok(result)
            })
        }), all);
        self.register(
            "powershell",
            "Run a PowerShell command and return its bounded output. Use when: the command requires PowerShell syntax. Not for: general shell commands; use bash.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type":"string","description":"PowerShell command to run"},
                    "description": {"type":"string","description":"What this command does"},
                    "timeout": {"type":"integer","description":"Timeout in milliseconds"}
                },
                "required": ["command"]
            }),
            Arc::new(|input: Value| {
                Box::pin(async move { builtins::execute_powershell(input).await })
            }),
            all,
        );
        self.register("rag_search", "Retrieve semantically relevant chunks from the RAG index. Use when: searching indexed documents by meaning. Not for: matching file text by regex; use grep_search.", json!({
            "type": "object",
            "properties": {"query": {"type":"string","description":"Natural-language search query."},"limit": {"type":"integer","description":"Maximum number of matching chunks."}},
            "required": ["query"]
        }), sync_tool_ref(rag::execute_rag_search), all);
        self.register("rag_index", "Index supplied document text for later semantic retrieval. Use when: adding a document to RAG storage. Not for: splitting text without indexing; use rag_chunk.", json!({
            "type": "object",
            "properties": {"content": {"type":"string","description":"Document content to index"},"iri": {"type":"string","description":"Optional IRI identifier"},"tags": {"type":"array","items":{"type":"string"},"description":"Optional tags"}},
            "required": ["content"]
        }), sync_tool_ref(rag::execute_rag_index), all);
        self.register("rag_chunk", "Split supplied document text into overlapping chunks without indexing it. Use when: previewing segmentation. Not for: persisting searchable text; use rag_index.", json!({
            "type": "object",
            "properties": {"content": {"type":"string","description":"Document content to chunk"},"chunk_size": {"type":"integer","description":"Chunk size in characters (default 500)"},"overlap": {"type":"integer","description":"Overlap between chunks (default 50)"}},
            "required": ["content"]
        }), sync_tool_ref(rag::execute_rag_chunk), all);

        // ========== Knowledge Import Tools ==========
        self.register("knowledge_import_file", "Read a local file, chunk its content, and index it as knowledge. Use when: importing one file. Not for: importing a whole folder; use knowledge_import_directory.", json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Path of the local file to import."},
                "tags": {"type":"array","items":{"type":"string"},"description":"Tags for categorization"},
                "chunk_size": {"type":"integer","description":"Chunk size in characters (default 1000)"},
                "overlap": {"type":"integer","description":"Overlap between chunks (default 100)"},
                "auto_detect_title": {"type":"boolean","description":"Auto-detect title from content (default true)"}
            },
            "required": ["path"]
        }), Arc::new(|input: Value| Box::pin(async move { knowledge::execute_knowledge_import_file(input).await })), all);

        self.register("knowledge_import_url", "Fetch a web page, extract its text, and index it as knowledge. Use when: importing one URL. Not for: importing a local file; use knowledge_import_file.", json!({
            "type": "object",
            "properties": {
                "url": {"type":"string","description":"Absolute HTTP or HTTPS URL to fetch and import."},
                "tags": {"type":"array","items":{"type":"string"},"description":"Tags for categorization"},
                "chunk_size": {"type":"integer","description":"Chunk size in characters (default 1000)"},
                "overlap": {"type":"integer","description":"Overlap between chunks (default 100)"},
                "selector": {"type":"string","description":"CSS selector for page content to extract."}
            },
            "required": ["url"]
        }), Arc::new(|input: Value| Box::pin(async move { knowledge::execute_knowledge_import_url(input).await })), all);

        self.register("knowledge_import_directory", "Index matching files from a directory, optionally including subdirectories. Use when: importing many files. Not for: one file; use knowledge_import_file.", json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Directory path containing files to import."},
                "pattern": {"type":"string","description":"File-name glob pattern; default includes md, txt, html, and json."},
                "tags": {"type":"array","items":{"type":"string"},"description":"Tags for categorization"},
                "recursive": {"type":"boolean","description":"Recursively process subdirectories (default true)"},
                "chunk_size": {"type":"integer","description":"Chunk size in characters (default 1000)"},
                "overlap": {"type":"integer","description":"Overlap between chunks (default 100)"}
            },
            "required": ["path"]
        }), Arc::new(|input: Value| Box::pin(async move { knowledge::execute_knowledge_import_directory(input).await })), all);

        self.register("knowledge_list", "List indexed knowledge entries with optional source, tag, and pagination filters. Use when: browsing entries. Not for: relevance-ranked matches; use knowledge_search.", json!({
            "type": "object",
            "properties": {
                "tags": {"type":"array","items":{"type":"string"},"description":"Filter by tags"},
                "source_type": {"type":"string","description":"Filter by source type (file, url)"},
                "limit": {"type":"integer","description":"Max results (default 100)"},
                "offset": {"type":"integer","description":"Pagination offset"}
            }
        }), Arc::new(|input: Value| Box::pin(async move { knowledge::execute_knowledge_list(input).await })), all);

        self.register("knowledge_delete", "Delete indexed knowledge entries by IRI, tags, or all entries when explicitly requested. Use when: removing stored entries. Not for: changing content; use knowledge_update.", json!({
            "type": "object",
            "properties": {
                "iri": {"type":"string","description":"IRI of knowledge entry to delete"},
                "tags": {"type":"array","items":{"type":"string"},"description":"Delete all entries with these tags"},
                "all": {"type":"boolean","description":"Delete all knowledge entries; default false."}
            }
        }), Arc::new(|input: Value| Box::pin(async move { knowledge::execute_knowledge_delete(input).await })), all);

        self.register("knowledge_search", "Rank imported knowledge entries by keyword relevance, optionally filtered by tags. Use when: finding matching entries. Not for: browsing all entries; use knowledge_list.", json!({
            "type": "object",
            "properties": {
                "query": {"type":"string","description":"Search query"},
                "tags": {"type":"array","items":{"type":"string"},"description":"Filter by tags"},
                "limit": {"type":"integer","description":"Max results (default 10)"},
                "min_score": {"type":"number","description":"Minimum relevance score (default 0.1)"}
            },
            "required": ["query"]
        }), Arc::new(|input: Value| Box::pin(async move { knowledge::execute_knowledge_search(input).await })), all);

        self.register("knowledge_update", "Change the content or tags of an existing indexed knowledge entry. Use when: revising an entry by IRI. Not for: deleting entries; use knowledge_delete.", json!({
            "type": "object",
            "properties": {
                "iri": {"type":"string","description":"IRI of knowledge entry to update"},
                "content": {"type":"string","description":"New content"},
                "tags": {"type":"array","items":{"type":"string"},"description":"New or additional tags"},
                "append_tags": {"type":"boolean","description":"Append tags instead of replacing (default false)"}
            },
            "required": ["iri"]
        }), Arc::new(|input: Value| Box::pin(async move { knowledge::execute_knowledge_update(input).await })), all);

        // ========== Skill Creation Tools ==========
        self.register("create_skill", "Generate and register a skill definition from a natural-language request. Use when: creating a new skill from instructions. Not for: converting existing Markdown; use convert_skill.", json!({
            "type": "object",
            "properties": {
                "description": {"type":"string","description":"Natural language description of the skill to create"},
                "skill_name_hint": {"type":"string","description":"Suggested skill name (optional, lowercase with underscores)"},
                "category_hint": {"type":"string","description":"Suggested category (optional): file|network|ai|execution|validation|data|meta|system"},
                "security_level_override": {"type":"string","description":"Security level override (optional): low|normal|high|critical"}
            },
            "required": ["description"]
        }), Arc::new(|input: Value| Box::pin(async move { builtins::execute_create_skill(input).await })), &["DA"]);

        self.register("convert_skill", "Convert a Markdown skill description into a JSON-LD skill definition. Use when: structured Markdown is already available. Not for: generating from a request; use create_skill.", json!({
            "type": "object",
            "properties": {
                "markdown_content": {"type":"string","description":"Markdown content describing the skill"},
                "source_path": {"type":"string","description":"Optional path of the source Markdown file."}
            },
            "required": ["markdown_content"]
        }), Arc::new(|input: Value| Box::pin(async move { builtins::execute_convert_skill(input).await })), &["DA","CA"]);

        // ========== Knowledge Graph Tools ==========
        let kg_store_for_extract = self.kg_store.clone();
        self.register("knowledge_extract", "Extract entities and relations from supplied text into the knowledge graph. Use when: converting prose into graph facts. Not for: querying stored facts; use knowledge_query.", json!({
            "type": "object",
            "properties": {
                "text": {"type":"string","description":"Text content to extract from."},
                "domain": {"type":"string","description":"Optional extraction domain hint."}
            },
            "required": ["text"]
        }), Arc::new(move |input: Value| {
            let kg_store = kg_store_for_extract.clone();
            Box::pin(async move { builtins::execute_knowledge_extract(input, kg_store).await })
        }), all);

        let kg_store_for_query = self.kg_store.clone();
        self.register(
            "knowledge_query",
            "Run a SPARQL SELECT query over the knowledge graph and return bindings. Use when: querying graph triples precisely. Not for: fuzzy entity lookup; use kg_search.",
            json!({
                "type": "object",
                "properties": {
                    "sparql": {"type":"string","description":"SPARQL SELECT query statement."},
                    "named_graph": {"type":"string","description":"Named graph IRI (optional)."}
                },
                "required": ["sparql"]
            }),
            Arc::new(move |input: Value| {
                let kg_store = kg_store_for_query.clone();
                Box::pin(async move { builtins::execute_knowledge_query(input, kg_store).await })
            }),
            all,
        );

        let kg_store_for_search = self.kg_store.clone();
        self.register("kg_search", "Find knowledge-graph entities by fuzzy keyword and optional type. Use when: locating graph nodes. Not for: SPARQL triple patterns; use knowledge_query.", json!({
            "type": "object",
            "properties": {
                "keyword": {"type":"string","description":"Search keyword."},
                "entity_type": {"type":"string","description":"Entity type IRI filter (optional)."}
            },
            "required": ["keyword"]
        }), Arc::new(move |input: Value| {
            let kg_store = kg_store_for_search.clone();
            Box::pin(async move { builtins::execute_knowledge_search(input, kg_store).await })
        }), all);

        let vector_store_for_search = self.vector_store.clone();
        self.register("kb_vector_search", "Return vector-ranked text chunks from an ingested knowledge namespace. Use when: semantic document retrieval is needed. Not for: fuzzy graph entities; use kg_search.", json!({
            "type": "object",
            "properties": {
                "query": {"type":"string","description":"Natural-language query for semantic retrieval."},
                "namespace": {"type":"string","description":"Vector namespace to restrict search to a specific knowledge base (optional)."},
                "limit": {"type":"integer","description":"Max number of results (1-20, default 5)."}
            },
            "required": ["query"]
        }), Arc::new(move |input: Value| {
            let vstore = vector_store_for_search.read().clone();
            Box::pin(async move { builtins::execute_kb_vector_search(input, vstore).await })
        }), all);

        let kg_store_for_neighbors = self.kg_store.clone();
        self.register(
            "knowledge_neighbors",
            "Traverse neighboring graph entities and their relations up to the requested depth. Use when: exploring connections from an entity. Not for: finding an entity by keyword; use kg_search.",
            json!({
                "type": "object",
                "properties": {
                    "entity_id": {"type":"string","description":"Entity ID or IRI."},
                    "depth": {"type":"integer","description":"Traversal depth (1-3, default 1)."}
                },
                "required": ["entity_id"]
            }),
            Arc::new(move |input: Value| {
                let kg_store = kg_store_for_neighbors.clone();
                Box::pin(
                    async move { builtins::execute_knowledge_neighbors(input, kg_store).await },
                )
            }),
            all,
        );

        let kg_store_for_import = self.kg_store.clone();
        self.register("knowledge_import_json", "Map structured JSON objects into knowledge-graph nodes using a supplied mapping. Use when: importing structured data. Not for: indexing document text; use knowledge_import_file.", json!({
            "type": "object",
            "properties": {
                "json_data": {"type":"string","description":"JSON string containing an object or array of objects."},
                "mapping_config": {"type":"string","description":"JSON mapping string with id_field, type_field, label_field and optional relations."}
            },
            "required": ["json_data","mapping_config"]
        }), Arc::new(move |input: Value| {
            let kg_store = kg_store_for_import.clone();
            Box::pin(async move { builtins::execute_knowledge_import_json(input, kg_store).await })
        }), all);

        let kg_store_for_ontology = self.kg_store.clone();
        self.register("ontology_register", "Register ontology classes or properties in the knowledge graph. Use when: adding schema terms. Not for: checking RDF syntax; use ontology_validate_turtle.", json!({
            "type": "object",
            "properties": {
                "terms": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "iri": {"type":"string","description":"Ontology term IRI."},
                            "label": {"type":"string","description":"Term label."},
                            "description": {"type":"string","description":"Term description."},
                            "term_type": {"type":"string","description":"Type: Class | Property | Relation."}
                        },
                        "required": ["iri","label","description","term_type"]
                    },
                    "description": "Ontology term list."
                }
            },
            "required": ["terms"]
        }), Arc::new(move |input: Value| {
            let kg_store = kg_store_for_ontology.clone();
            Box::pin(async move { builtins::execute_ontology_register(input, kg_store).await })
        }), all);

        let kg_store_for_bridge = self.kg_store.clone();
        self.register("knowledge_bridge", "Link a knowledge-graph entity to a skill with a relation. Use when: associating stored entities and skills. Not for: creating graph entities from prose; use knowledge_extract.", json!({
            "type": "object",
            "properties": {
                "entity_id": {"type":"string","description":"Entity ID."},
                "skill_iri": {"type":"string","description":"Skill IRI."},
                "relation_type": {"type":"string","description":"Relation type: HasSkill | ApplicableIn | RelatedTo (default HasSkill)."}
            },
            "required": ["entity_id","skill_iri"]
        }), Arc::new(move |input: Value| {
            let kg_store = kg_store_for_bridge.clone();
            Box::pin(async move { builtins::execute_knowledge_bridge_with_store(input, kg_store).await })
        }), all);

        let kg_store_for_code = self.kg_store.clone();
        self.register("knowledge_extract_code", "Parse a code file into graph entities and relations, skipping unchanged files unless forced. Use when: indexing source structure. Not for: searching file text; use grep_search.", json!({
            "type": "object",
            "properties": {
                "file_path": {"type":"string","description":"Path of the source code file to parse."},
                "named_graph": {"type":"string","description":"Named graph IRI (optional, default graph:code)."},
                "force": {"type":"boolean","description":"Force full extraction, ignore cache (optional, default false)."}
            },
            "required": ["file_path"]
        }), Arc::new(move |input: Value| {
            let kg_store = kg_store_for_code.clone();
            Box::pin(async move { builtins::execute_knowledge_extract_code(input, kg_store).await })
        }), all);

        // ========== L3 Projection Query Tool ==========
        let proj_for_tool = self.projection_engine.clone();
        self.register("read_agent_output", "Read a previous agent's complete output from its projected task node. Use when: inspecting prior agent results by node IRI. Not for: reading a workspace file; use file_read.", json!({
            "type": "object",
            "properties": {
                "node_iri": {"type":"string","description":"L2 node IRI to read (e.g. iri://task/xxx/turn_3)."}
            },
            "required": ["node_iri"]
        }), Arc::new(move |input: Value| {
            let proj = proj_for_tool.clone();
            Box::pin(async move {
                let node_iri = input
                    .get("node_iri")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing node_iri parameter".to_string())?;
                let guard = proj.read();
                let engine = guard.as_ref()
                    .ok_or_else(|| "Projection engine not initialized".to_string())?;
                let result = engine.read_node(node_iri)
                    .map_err(|e| format!("Failed to read L2 node: {}", e))?;
                match result {
                    Some(node) => Ok(node),
                    None => Err(format!("Node not found: {}", node_iri)),
                }
            })
        }), all);

        // ========== Ontology Tools ==========
        #[cfg(feature = "ontology")]
        {
            self.register(
                "ontology_validate_turtle",
                "Parse Turtle RDF and report syntax validity and triple count. Use when: checking whether Turtle parses. Not for: style warnings; use ontology_lint_turtle.",
                json!({
                    "type": "object",
                    "properties": {
                        "ttl": {"type":"string","description":"Turtle content to validate"}
                    },
                    "required": ["ttl"]
                }),
                Arc::new(|input: Value| {
                    Box::pin(async move {
                        ontology_tools::execute_ontology_validate_turtle(input).await
                    })
                }),
                all,
            );

            self.register(
                "ontology_lint_turtle",
                "Check Turtle RDF for missing labels, comments, and domain or range guidance. Use when: reviewing ontology quality. Not for: syntax validation alone; use ontology_validate_turtle.",
                json!({
                    "type": "object",
                    "properties": {
                        "ttl": {"type":"string","description":"Turtle content to lint"}
                    },
                    "required": ["ttl"]
                }),
                Arc::new(|input: Value| {
                    Box::pin(
                        async move { ontology_tools::execute_ontology_lint_turtle(input).await },
                    )
                }),
                all,
            );

            self.register(
                "ontology_diff_turtle",
                "Compare two Turtle documents and report added and removed triples. Use when: reviewing RDF changes. Not for: syntax checking one document; use ontology_validate_turtle.",
                json!({
                    "type": "object",
                    "properties": {
                        "old_ttl": {"type":"string","description":"Original Turtle content"},
                        "new_ttl": {"type":"string","description":"New Turtle content"}
                    },
                    "required": ["old_ttl","new_ttl"]
                }),
                Arc::new(|input: Value| {
                    Box::pin(
                        async move { ontology_tools::execute_ontology_diff_turtle(input).await },
                    )
                }),
                all,
            );

            self.register("ontology_validate_shacl", "Validate RDF data against supplied SHACL shapes and report violations. Use when: checking shape constraints. Not for: Turtle syntax alone; use ontology_validate_turtle.", json!({
                "type": "object",
                "properties": {
                    "shapes_ttl": {"type":"string","description":"SHACL shapes in Turtle format"},
                    "data_ttl": {"type":"string","description":"Optional data Turtle to validate. If omitted, validates loaded store."}
                },
                "required": ["shapes_ttl"]
            }), Arc::new(|input: Value| Box::pin(async move { ontology_tools::execute_ontology_validate_shacl(input).await })), all);

            self.register("ontology_reason", "Infer triples from Turtle data with a selected RDFS or OWL reasoning profile. Use when: materializing logical consequences. Not for: validating shapes; use ontology_validate_shacl.", json!({
                "type": "object",
                "properties": {
                    "ttl": {"type":"string","description":"Turtle data to reason over"},
                    "profile": {"type":"string","description":"Reasoning profile: rdfs, owl-rl (default), owl-rl-ext, owl-dl"},
                    "materialize": {"type":"boolean","description":"Whether to materialize inferred triples (default: true)"}
                },
                "required": ["ttl"]
            }), Arc::new(|input: Value| Box::pin(async move { ontology_tools::execute_ontology_reason(input).await })), all);
        }
        self.registering_builtins = false;
    }

    /// Register a tool with role whitelist. Empty = all roles allowed.
    ///
    /// A tool registered here is never an internal micro-tool, even when its
    /// name uses a micro-tool prefix: registering it replaces any internal
    /// reader of the same name (#270-2).
    pub fn register(
        &mut self,
        name: &str,
        description: &str,
        parameters: Value,
        handler: ToolFn,
        allowed_roles: &[&str],
    ) {
        self.micro_tool_contexts.write().remove(name);
        self.register_handler(name, description, parameters, handler, allowed_roles);
    }

    fn register_handler(
        &mut self,
        name: &str,
        description: &str,
        parameters: Value,
        handler: ToolFn,
        allowed_roles: &[&str],
    ) {
        let roles: Vec<String> = allowed_roles.iter().map(|s| s.to_string()).collect();
        if self.registering_builtins {
            self.builtin_tool_names.insert(name.to_string());
        } else {
            let candidate = ToolDescription {
                name: name.to_string(),
                description: description.to_string(),
                parameters: parameters.clone(),
                allowed_roles: roles.clone(),
            };
            let violations = tool_description_lint::lint_tool(&candidate);
            if !violations.is_empty() {
                warn!(
                    tool = name,
                    violations = %violations.join("; "),
                    "registered tool does not meet description lint"
                );
            }
        }
        self.tools.insert(name.to_string(), handler);

        if let Some(&index) = self.description_indices.get(name) {
            let existing = &mut self.tool_descriptions[index];
            existing.description = description.to_string();
            existing.parameters = parameters;
            existing.allowed_roles = roles;
        } else {
            self.description_indices
                .insert(name.to_string(), self.tool_descriptions.len());
            self.tool_descriptions.push(ToolDescription {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
                allowed_roles: roles,
            });
            if Self::is_micro_tool_name(name) {
                self.micro_description_order.push_back(name.to_string());
                if self.micro_description_order.len() > MAX_MICRO_TOOL_DESCRIPTIONS {
                    if let Some(pos) = self
                        .micro_description_order
                        .pop_front()
                        .and_then(|oldest| self.description_indices.remove(&oldest))
                    {
                        self.tool_descriptions.remove(pos);
                        // Capped micro-tools are appended after resident tools.
                        // Repair only the shifted tail when retiring the oldest.
                        for (index, tool) in self.tool_descriptions.iter().enumerate().skip(pos) {
                            self.description_indices.insert(tool.name.clone(), index);
                        }
                    }
                }
            }
        }
    }

    fn is_micro_tool_name(name: &str) -> bool {
        MICRO_TOOL_PREFIXES.iter().any(|p| name.starts_with(p))
    }

    /// True only for result readers created by `register_micro_tool`, never
    /// for an external tool that merely shares a micro-tool name prefix.
    fn is_internal_micro_tool(&self, name: &str) -> bool {
        ToolPolicy::has_internal_micro_tool_prefix(name)
            && self.micro_tool_contexts.read().contains_key(name)
    }

    /// Run-local policy decision, with the prefix read-only rule applied only
    /// to internal micro-tools.
    fn policy_allows(
        &self,
        policy: &ToolPolicy,
        role: &AgentRole,
        agent_id: &str,
        name: &str,
    ) -> bool {
        policy.is_executable(role, agent_id, name)
            || (self.is_internal_micro_tool(name)
                && policy.is_internal_micro_tool_executable(role, agent_id, name))
    }

    /// Register micro-tool (dynamically generated tool for querying large tool results)
    pub fn register_micro_tool(&mut self, tool_name: &str, context: MicroToolContext) {
        let contexts = Arc::clone(&self.micro_tool_contexts);
        let data = Arc::clone(&self.micro_tool_data);
        let tool_name_owned = tool_name.to_string();

        contexts
            .write()
            .insert(tool_name.to_string(), context.clone());

        let description = if tool_name.starts_with("read_full_result_") {
            format!("Read full tool result. call_id: {}", context.call_id)
        } else if tool_name.starts_with("query_") {
            format!(
                "Query entity types: {:?}. call_id: {}",
                context.entity_types, context.call_id
            )
        } else if tool_name.starts_with("get_entity_details_") {
            format!("Get entity details. call_id: {}", context.call_id)
        } else {
            format!("Micro-tool: {}", tool_name)
        };

        let params = json!({
            "type": "object",
            "properties": {
                "offset": {"type": "integer", "description": "Starting offset"},
                "limit": {"type": "integer", "description": "Max results to return"}
            }
        });

        self.register_handler(
            tool_name,
            &description,
            params,
            Arc::new(move |input: Value| {
                let contexts = contexts.clone();
                let tool_name_owned = tool_name_owned.clone();
                let data = data.clone();
                Box::pin(async move {
                    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
                    let limit = input["limit"].as_u64().unwrap_or(100) as usize;

                    let ctx_guard = contexts.read();
                    let ctx = ctx_guard.get(&tool_name_owned).ok_or_else(|| {
                        format!("Micro-tool context not found: {}", tool_name_owned)
                    })?;

                    let data_guard = data.read();
                    let stored_data = data_guard
                        .get(&ctx.storage_key)
                        .ok_or_else(|| format!("Micro-tool data not found: {}", ctx.storage_key))?;

                    if tool_name_owned.starts_with("read_full_result_") {
                        if let Some(content) = stored_data.get("content").and_then(|v| v.as_str()) {
                            let lines: Vec<&str> = content.lines().collect();
                            let selected: Vec<String> = lines
                                .iter()
                                .skip(offset)
                                .take(limit)
                                .map(|l| l.to_string())
                                .collect();
                            return Ok(json!({
                                "content": selected.join("\n"),
                                "total_lines": lines.len(),
                                "offset": offset,
                                "returned": selected.len(),
                                "call_id": ctx.call_id,
                            }));
                        }
                    } else if tool_name_owned.starts_with("query_") {
                        if let Some(content) = stored_data.get("content").and_then(|v| v.as_str()) {
                            let query_type = input["entity_type"].as_str().unwrap_or("");
                            let keyword = input["keyword"].as_str().unwrap_or("");

                            let mut results = Vec::new();
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
                                if let Some(arr) = parsed.as_array() {
                                    for item in arr.iter().skip(offset).take(limit) {
                                        let type_match = query_type.is_empty()
                                            || item
                                                .get("type")
                                                .and_then(|v| v.as_str())
                                                .map(|t| t.contains(query_type))
                                                .unwrap_or(false);
                                        let keyword_match = keyword.is_empty()
                                            || item
                                                .to_string()
                                                .to_lowercase()
                                                .contains(&keyword.to_lowercase());
                                        if type_match && keyword_match {
                                            results.push(item.clone());
                                        }
                                    }
                                }
                            }
                            return Ok(json!({
                                "results": results,
                                "count": results.len(),
                                "call_id": ctx.call_id,
                            }));
                        }
                    } else if tool_name_owned.starts_with("get_entity_details_") {
                        let entity_id = input["entity_id"].as_str().unwrap_or("");
                        if let Some(content) = stored_data.get("content").and_then(|v| v.as_str()) {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
                                if let Some(arr) = parsed.as_array() {
                                    for item in arr {
                                        if item.get("id").and_then(|v| v.as_str())
                                            == Some(entity_id)
                                        {
                                            return Ok(json!({
                                                "entity": item,
                                                "call_id": ctx.call_id,
                                            }));
                                        }
                                    }
                                }
                            }
                        }
                        return Ok(json!({
                            "error": "Entity not found",
                            "entity_id": entity_id,
                            "call_id": ctx.call_id,
                        }));
                    }

                    Ok(json!({
                        "data": stored_data,
                        "call_id": ctx.call_id,
                    }))
                })
            }),
            &[],
        );
    }

    /// Store micro-tool data
    pub fn store_micro_tool_data(&self, storage_key: &str, data: serde_json::Value) {
        self.micro_tool_data
            .write()
            .insert(storage_key.to_string(), data);
    }

    /// Get list of registered micro-tools
    pub fn get_micro_tool_names(&self) -> Vec<String> {
        self.micro_tool_contexts.read().keys().cloned().collect()
    }

    pub async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolExecutionError> {
        self.execute_with_claims(name, input, None).await
    }

    async fn execute_with_claims(
        &self,
        name: &str,
        input: Value,
        claims: Option<IsolationClaims>,
    ) -> Result<Value, ToolExecutionError> {
        TOOL_ISOLATION_CLAIMS
            .scope(claims, self.execute_inner(name, input))
            .await
    }

    async fn execute_inner(&self, name: &str, input: Value) -> Result<Value, ToolExecutionError> {
        let input_str = input.to_string();

        if let Some(ref policy) = self.permission_policy {
            match policy.authorize(name, &input_str, None) {
                PermissionOutcome::Deny { reason } => {
                    return Ok(
                        json!({"error": format!("Permission denied: {}", reason), "denied_by": "permission_policy"}),
                    );
                }
                PermissionOutcome::Allow => {}
            }
        }

        if let Some(ref runner) = self.hook_runner {
            let hook_result = runner.run_pre_tool_use(name, &input_str);
            if hook_result.is_denied() {
                return Ok(
                    json!({"error": format!("Pre-tool hook denied: {}", hook_result.messages().join("; "))}),
                );
            }
            if hook_result.is_failed() {
                return Ok(
                    json!({"error": format!("Pre-tool hook failed: {}", hook_result.messages().join("; "))}),
                );
            }
            if hook_result.is_cancelled() {
                return Ok(json!({"error": "Pre-tool hook was cancelled"}));
            }
        }

        if let Some(ref gate) = self.syscall_gate {
            let context = TOOL_CALLER_CONTEXT
                .try_with(|context| context.clone())
                .ok()
                .flatten();
            let run_policy = TOOL_RUN_POLICY
                .try_with(|policy| policy.clone())
                .ok()
                .flatten();
            let (role, agent_id) = context
                .as_ref()
                .map(|context| (context.agent_role.as_str(), context.agent_id.as_str()))
                .unwrap_or(("", ""));
            // #270-1: the gate decides with the caller's run-local policy and
            // fails closed without a trusted role or run policy.
            let decision = gate.validate_tool_for_run(
                name,
                role,
                agent_id,
                run_policy.as_ref(),
                self.is_internal_micro_tool(name),
                None,
            );
            if let Err(e) = decision {
                return Ok(
                    json!({"error": format!("SyscallGate rejected: {}", e), "denied_by": "syscall_gate"}),
                );
            }
        }

        let result = if name == "tool_search" {
            let role = TOOL_SEARCH_CALLER_ROLE
                .try_with(|role| role.clone())
                .ok()
                .flatten()
                .ok_or_else(|| ToolExecutionError::ExecutionFailed {
                    name: name.to_string(),
                    message: "tool_search requires a verified runtime role".to_string(),
                })?;
            let policy = TOOL_RUN_POLICY
                .try_with(|policy| policy.clone())
                .ok()
                .flatten()
                .unwrap_or_default();
            let agent_id = TOOL_CALLER_CONTEXT
                .try_with(|context| context.as_ref().map(|context| context.agent_id.clone()))
                .ok()
                .flatten()
                .unwrap_or_default();
            self.search_tools_for_role_with_policy(&role, &agent_id, &policy, input)
                .map_err(|message| ToolExecutionError::ExecutionFailed {
                    name: name.to_string(),
                    message,
                })
        } else {
            // Use the fallback-aware lookup so routing every caller through the
            // permission/hook/syscall gates does not break micro-tool dispatch.
            let handler = match self.try_get_handler(name) {
                Some(h) => h,
                None => {
                    return Err(ToolExecutionError::NotFound {
                        name: name.to_string(),
                    })
                }
            };
            debug!(tool = %name, "Executing tool");
            handler(input)
                .await
                .map_err(|message| ToolExecutionError::ExecutionFailed {
                    name: name.to_string(),
                    message,
                })
        };

        // Post-tool-use hook
        if let Some(ref runner) = self.hook_runner {
            match &result {
                Ok(output) => {
                    let output_str = output.to_string();
                    let post_result =
                        runner.run_post_tool_use(name, &input_str, &output_str, false);
                    if post_result.is_denied() {
                        return Ok(
                            json!({"error": format!("Post-tool hook denied: {}", post_result.messages().join("; ")), "original_output": output}),
                        );
                    }
                }
                Err(e) => {
                    let _ = runner.run_post_tool_use_failure(name, &input_str, &e.to_string());
                }
            }
        }

        result
    }

    /// Execute a tool behind the SkillGraph security gate.
    ///
    /// Adds two checks on top of `execute`: the schemas advertised for this
    /// model turn and the graph-backed `SecurityEngine` decision for the resolved skill IRI.
    ///
    /// The advertised set is an execution boundary, not a plan preference:
    /// callers must capture it from the exact schema payload sent to the model.
    /// In particular, a PDCA step's `tools_allowed` metadata must not be used
    /// as a substitute for this per-turn boundary.
    /// A tool without a resolvable skill fails closed.
    pub async fn execute_with_security_context(
        &self,
        name: &str,
        input: Value,
        context: SecurityContext,
        advertised_tools: &[String],
        policy: &ToolPolicy,
    ) -> Result<Value, ToolExecutionError> {
        self.execute_with_security_context_and_claims_and_policy(
            name,
            input,
            context,
            advertised_tools,
            None,
            policy,
        )
        .await
    }

    /// Executes a tool under the caller-owned per-run role policy.
    pub async fn execute_with_security_context_and_claims_and_policy(
        &self,
        name: &str,
        input: Value,
        context: SecurityContext,
        advertised_tools: &[String],
        claims: Option<IsolationClaims>,
        policy: &ToolPolicy,
    ) -> Result<Value, ToolExecutionError> {
        if !advertised_tools.iter().any(|tool| tool == name) {
            return Ok(json!({
                "error": format!("Tool not advertised for this turn: {}", name),
                "tool": name,
                "denied_by": "advertised_gate",
            }));
        }
        let role = context.agent_role.parse::<AgentRole>().map_err(|_| {
            ToolExecutionError::ExecutionFailed {
                name: name.to_string(),
                message: "runtime security context has an unknown role".to_string(),
            }
        })?;
        let security_engine = { self.security_engine.read().clone() };
        if let Some(engine) = security_engine {
            let skill_iri = {
                let registry = self.shared_skill_registry.read();
                registry
                    .as_ref()
                    .and_then(|registry| registry.skill_iri_for_tool_name(name))
            }
            .or_else(|| builtin_security_skill_iri(name).map(str::to_string))
            // Generated result readers expose no independent side effect. They
            // inherit the least-privilege built-in read capability instead of
            // becoming an unregistered security bypass.
            .or_else(|| {
                self.is_internal_micro_tool(name)
                    .then(|| "iri://skills/file_read".to_string())
            });
            let Some(skill_iri) = skill_iri else {
                return Ok(
                    json!({"error": "Security denied: tool has no registered executable skill", "tool": name, "denied_by": "security_engine"}),
                );
            };
            match engine.check_execution(&skill_iri, &context).await {
                Ok(SecurityDecision::Allowed) => {}
                Ok(SecurityDecision::Denied { reasons }) => {
                    return Ok(
                        json!({"error": "Security denied", "tool": name, "skill_iri": skill_iri, "reasons": reasons, "denied_by": "security_engine"}),
                    );
                }
                Ok(SecurityDecision::RequiresApproval { approver, reason }) => {
                    return Ok(
                        json!({"error": "Security approval required", "tool": name, "skill_iri": skill_iri, "approver": approver, "reason": reason, "denied_by": "security_engine"}),
                    );
                }
                Err(error) => {
                    return Ok(
                        json!({"error": format!("Security denied: {error}"), "tool": name, "skill_iri": skill_iri, "denied_by": "security_engine"}),
                    );
                }
            }
        }

        if !self.policy_allows(policy, &role, &context.agent_id, name) {
            tracing::warn!(
                agent = %context.agent_id,
                role = %context.agent_role,
                tool = %name,
                "Role tool policy denied execution"
            );
            return Ok(json!({
                "error": "Tool not allowed for role",
                "tool": name,
                "role": context.agent_role,
                "denied_by": "role_policy",
            }));
        }

        let role = context.agent_role.clone();
        TOOL_CALLER_CONTEXT
            .scope(
                Some(context),
                TOOL_RUN_POLICY.scope(
                    Some(policy.clone()),
                    TOOL_SEARCH_CALLER_ROLE
                        .scope(Some(role), self.execute_with_claims(name, input, claims)),
                ),
            )
            .await
    }

    /// Get tool handler (avoid holding lock across await)
    pub fn get_handler(&self, name: &str) -> Option<ToolFn> {
        self.tools.get(name).cloned()
    }

    /// Get tool handler with micro-tool fallback.
    /// When normal lookup fails, dynamically build a handler from micro-tool data storage,
    /// preventing LLM from exhausting turns due to registry/handler inconsistency.
    pub fn try_get_handler(&self, name: &str) -> Option<ToolFn> {
        // 1. Try registered handler first
        if let Some(handler) = self.tools.get(name) {
            return Some(handler.clone());
        }
        // 2. Fallback: build dynamic handler from stored data for read_full_result_* micro-tools
        if name.starts_with("read_full_result_") {
            return self.make_micro_tool_fallback_handler(name);
        }
        None
    }

    /// Build a dynamic fallback handler for micro-tools (reads from micro_tool_data / micro_tool_contexts)
    fn make_micro_tool_fallback_handler(&self, name: &str) -> Option<ToolFn> {
        let ctx_guard = self.micro_tool_contexts.read();
        let ctx = ctx_guard.get(name)?.clone();
        let storage_key = ctx.storage_key.clone();
        let call_id = ctx.call_id.clone();
        drop(ctx_guard);

        let data_guard = self.micro_tool_data.read();
        let stored_data = data_guard.get(&storage_key)?.clone();
        drop(data_guard);

        Some(Arc::new(move |input: Value| {
            let _storage_key = storage_key.clone();
            let call_id = call_id.clone();
            let stored_data = stored_data.clone();

            Box::pin(async move {
                let offset = input["offset"].as_u64().unwrap_or(0) as usize;
                let limit = input["limit"].as_u64().unwrap_or(100) as usize;

                if let Some(content) = stored_data.get("content").and_then(|v| v.as_str()) {
                    let lines: Vec<&str> = content.lines().collect();
                    let selected: Vec<String> = lines
                        .iter()
                        .skip(offset)
                        .take(limit)
                        .map(|l| l.to_string())
                        .collect();
                    return Ok(serde_json::json!({
                        "content": selected.join("
                    "),
                        "total_lines": lines.len(),
                        "offset": offset,
                        "returned": selected.len(),
                        "call_id": call_id,
                    }));
                }

                Ok(serde_json::json!({
                    "data": stored_data,
                    "call_id": call_id,
                }))
            })
        }))
    }

    /// List all registered tools in deterministic name order.
    pub fn list_tools(&self, _role: &str) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// Return resident definitions without run-local activations.
    pub fn tool_definitions_for_role(&self, role: &str) -> Vec<Value> {
        self.tool_definitions_for_turn(role, &ActivatedTools::default())
    }

    /// Build the exact schema for one model turn. Resident definitions retain
    /// registration order; activated on-demand definitions are appended in the
    /// order they were activated; dynamic micro-tools always remain at the tail.
    pub fn tool_definitions_for_turn(&self, role: &str, activated: &ActivatedTools) -> Vec<Value> {
        self.tool_definitions_for_turn_with_policy(role, "", activated)
    }

    /// Build one turn's schema after applying the caller-owned run policy.
    pub fn tool_definitions_for_turn_with_policy(
        &self,
        role: &str,
        agent_id: &str,
        activated: &ActivatedTools,
    ) -> Vec<Value> {
        let role_name = match role {
            "PA" | "Plan" => "Plan",
            "DA" | "Do" => "Do",
            "CA" | "Check" => "Check",
            "AA" | "Act" => "Act",
            _ => role,
        };

        let (mut resident_tools, on_demand_tools) = if let Some(manager) = self
            .tool_group_manager
            .as_ref()
            .filter(|manager| manager.is_enabled())
        {
            manager.get_tool_names_for_role(role_name)
        } else {
            let is_pa = role == "Plan" || role == "PA";
            let is_aa = role == "Act" || role == "AA";
            if is_pa {
                let default: HashSet<String> = ToolPolicy::readonly_tools()
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                (default, HashSet::new())
            } else if is_aa {
                // Design: AA = Core(file_read,file_list) + System(tool_search) by default, Search+Knowledge on demand
                let aa_tools: HashSet<String> = [
                    "file_read",
                    "file_list",
                    "tool_search",
                    "grep_search",
                    "glob_search",
                    "rag_search",
                    "kg_search",
                    "codebase_search",
                    "knowledge_list",
                    "knowledge_search",
                    "knowledge_extract_code",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                (aa_tools, HashSet::new())
            } else {
                let all: HashSet<String> = self
                    .tool_descriptions
                    .iter()
                    .map(|td| td.name.clone())
                    .collect();
                (all, HashSet::new())
            }
        };

        let agent_role = role.parse::<AgentRole>().unwrap_or(AgentRole::Act);
        let policy_is_allowed =
            |name: &str| self.policy_allows(activated.policy(), &agent_role, agent_id, name);
        if agent_role == AgentRole::Check && activated.policy().check_bash_enabled() {
            resident_tools.insert("bash".to_string());
        }
        let role_is_allowed = |td: &ToolDescription| {
            td.allowed_roles.is_empty()
                || td.allowed_roles.iter().any(|allowed| {
                    allowed == role
                        || matches!(
                            (allowed.as_str(), role_name),
                            ("PA", "Plan") | ("DA", "Do") | ("CA", "Check") | ("AA", "Act")
                        )
                })
        };
        let definition = |td: &ToolDescription| {
            let mut params = td.parameters.clone();
            if params.get("type").is_none() {
                params["type"] = json!("object");
            }
            json!({
                "type": "function",
                "function": {
                    "name": td.name,
                    "description": td.description,
                    "parameters": params,
                }
            })
        };

        let mut result: Vec<Value> = self
            .tool_descriptions
            .iter()
            .filter(|td| !Self::is_micro_tool_name(&td.name))
            .filter(|td| {
                resident_tools.contains(&td.name)
                    && role_is_allowed(td)
                    && policy_is_allowed(&td.name)
            })
            .map(definition)
            .collect();

        for name in activated.names() {
            if !on_demand_tools.contains(name) {
                continue;
            }
            if let Some(description) = self.tool_descriptions.iter().find(|td| {
                td.name == *name
                    && !Self::is_micro_tool_name(&td.name)
                    && role_is_allowed(td)
                    && policy_is_allowed(&td.name)
            }) {
                result.push(definition(description));
            }
        }

        result.extend(
            self.tool_descriptions
                .iter()
                .filter(|td| {
                    Self::is_micro_tool_name(&td.name)
                        && role_is_allowed(td)
                        && policy_is_allowed(&td.name)
                })
                .map(definition),
        );

        let tool_names: Vec<&str> = result
            .iter()
            .filter_map(|v| v["function"]["name"].as_str())
            .collect();
        tracing::debug!(
            "[tool_definitions_for_role] role={}, filtered={}/{}, tools={:?}",
            role,
            result.len(),
            self.tool_descriptions.len(),
            tool_names
        );

        result
    }

    /// Activate names returned by tool_search for the current role and run.
    /// Search implementation is intentionally separate; this accepts the
    /// stable `matches[].name` contract consumed by the runner.
    pub fn activate_on_demand_from_search(
        &self,
        role: &str,
        activated: &mut ActivatedTools,
        result: &Value,
    ) -> crate::tools::tool_groups::ActivationResult {
        let role_name = match role {
            "PA" | "Plan" => "Plan",
            "DA" | "Do" => "Do",
            "CA" | "Check" => "Check",
            "AA" | "Act" => "Act",
            _ => role,
        };
        let candidates: Vec<String> = result["matches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["name"].as_str().map(str::to_owned))
            .collect();
        let on_demand = self
            .tool_group_manager
            .as_ref()
            .filter(|manager| manager.is_enabled())
            .map(|manager| manager.get_tool_names_for_role(role_name).1)
            .unwrap_or_default();
        activated.activate(&candidates, &on_demand)
    }

    pub fn pa_readonly_tools() -> &'static [&'static str] {
        ToolPolicy::plan_readonly_tools()
    }

    pub fn is_pa_readonly_tool(name: &str) -> bool {
        ToolPolicy::plan_readonly_tools().contains(&name)
    }

    /// Searches the live registry for tools visible to a verified runtime role.
    ///
    /// The lexical stage is deliberately deterministic. Vector retrieval is
    /// intentionally not enabled here: its index must be server-owned and
    /// separate from tenant vector stores before it can participate in fusion.
    pub fn search_tools_for_role(&self, role: &str, input: Value) -> Result<Value, String> {
        let manager = self
            .tool_group_manager
            .as_ref()
            .filter(|manager| manager.is_enabled())
            .cloned()
            .unwrap_or_else(|| ToolGroupManager::new(None));
        let policy = ToolPolicy::new().with_tool_group_manager(manager);
        self.search_tools_for_role_with_policy(role, "", &policy, input)
    }

    fn search_tools_for_role_with_policy(
        &self,
        role: &str,
        agent_id: &str,
        policy: &ToolPolicy,
        input: Value,
    ) -> Result<Value, String> {
        let params: ToolSearchInput =
            serde_json::from_value(input).map_err(|error| format!("Invalid input: {error}"))?;
        let role = parse_runtime_role(role)?;
        let role_name = canonical_role_name(role);
        let manager = self
            .tool_group_manager
            .as_ref()
            .filter(|manager| manager.is_enabled())
            .cloned()
            .unwrap_or_else(|| ToolGroupManager::new(None));
        let (resident, on_demand) = manager.get_tool_names_for_role(role_name);
        let exact_query = params.query.trim();
        if self
            .tool_descriptions
            .iter()
            .any(|tool| tool.name == exact_query)
            && !self.policy_allows(policy, &role, agent_id, exact_query)
        {
            return Ok(json!({
                "matches": [],
                "count": 0,
                "query": params.query,
            }));
        }
        let query_terms = search_terms(&params.query);
        let max_results = params.max_results.unwrap_or(5).min(10);

        let mut matches: Vec<(i64, String, Value)> = self
            .tool_descriptions
            .iter()
            .filter(|tool| !Self::is_micro_tool_name(&tool.name))
            .filter(|tool| resident.contains(&tool.name) || on_demand.contains(&tool.name))
            .filter(|tool| policy.is_executable(&role, agent_id, &tool.name))
            .filter_map(|tool| {
                let group = manager.group_for_tool(&tool.name)?;
                let score = lexical_score(tool, &group.to_string(), &query_terms);
                (score > 0).then(|| {
                    let status = if resident.contains(&tool.name) {
                        "resident"
                    } else {
                        "on_demand"
                    };
                    (
                        score,
                        tool.name.clone(),
                        json!({
                            "name": tool.name,
                            "group": group.to_string(),
                            "description": one_line_description(&tool.description),
                            "status": status,
                            "retrieval": "lexical",
                        }),
                    )
                })
            })
            .collect();
        matches.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let matches: Vec<Value> = matches
            .into_iter()
            .take(max_results)
            .map(|(_, _, value)| value)
            .collect();
        let count = matches.len();
        Ok(json!({
            "matches": matches,
            "count": count,
            "query": params.query,
        }))
    }

    /// Marks search results with the current per-run activation state. This is
    /// presentation-only; activation remains constrained by the group manager.
    pub fn mark_search_results_activation(&self, result: &mut Value, activated: &ActivatedTools) {
        let Some(matches) = result.get_mut("matches").and_then(Value::as_array_mut) else {
            return;
        };
        for tool in matches {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                if activated.names().iter().any(|active| active == name) {
                    tool["status"] = json!("activated");
                }
            }
        }
    }
}

fn parse_runtime_role(role: &str) -> Result<AgentRole, String> {
    role.parse::<AgentRole>()
        .map_err(|_| "tool_search requires a recognized runtime role".to_string())
}

fn canonical_role_name(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Plan => "Plan",
        AgentRole::Do => "Do",
        AgentRole::Check => "Check",
        AgentRole::Act => "Act",
    }
}

fn search_terms(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| term.to_lowercase())
        .collect()
}

fn field_score(field: &str, query_terms: &[String], weight: i64) -> i64 {
    let field_lower = field.to_lowercase();
    let terms = search_terms(field);
    query_terms
        .iter()
        .map(|query| {
            if terms.iter().any(|term| term == query) {
                weight
            } else if field_lower.contains(query) {
                weight / 2
            } else {
                0
            }
        })
        .sum()
}

fn lexical_score(tool: &ToolDescription, group: &str, query_terms: &[String]) -> i64 {
    if query_terms.is_empty() {
        return 0;
    }
    let name_lower = tool.name.to_lowercase();
    let query = query_terms.join("_");
    let mut score = field_score(&tool.name, query_terms, 20)
        + field_score(&tool.description, query_terms, 6)
        + field_score(group, query_terms, 3);
    if name_lower == query {
        score += 100;
    } else if name_lower.starts_with(&query) {
        score += 50;
    }
    if let Some(properties) = tool.parameters.get("properties").and_then(Value::as_object) {
        for (name, schema) in properties {
            score += field_score(name, query_terms, 12);
            if let Some(description) = schema.get("description").and_then(Value::as_str) {
                score += field_score(description, query_terms, 5);
            }
        }
    }
    score
}

fn one_line_description(description: &str) -> String {
    let line = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= 160 {
        line
    } else {
        format!("{}…", line.chars().take(159).collect::<String>())
    }
}
