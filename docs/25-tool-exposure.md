# Tool exposure

## Resident and on-demand tools

Each agent run starts with its role's resident groups. On-demand groups are
not included in the function schema until `tool_search` returns matching tool
names. Those names are activated for the next turn only.

The default groups are:

| Group | Purpose |
| --- | --- |
| Core | File reading |
| Workspace | Workspace status and prior-agent output |
| Write | File changes and shell commands |
| Search | File and indexed-document search |
| Web | Public web search and retrieval |
| KnowledgeRead | Read-only knowledge and graph queries |
| KnowledgePlan | Plan-safe read-only knowledge search |
| KnowledgeWrite | Knowledge graph updates and extraction |
| Ingest | Document indexing and import |
| Skill | Skill definition creation and conversion |
| Ontology | Ontology registration, validation, comparison, and reasoning |
| System | Tool discovery |

Plan receives only read-only groups, including its Web on-demand group. Do has
the write-capable resident group; Check is read-only by default. Role visibility
is separate from execution policy.

## Role policy and enforcement

Every call passes two independent gates. First, its name must be in the exact
schema advertised for the current turn. Second, the runtime security context
must satisfy the role policy. The executor obtains the role and agent identity
only from that context, never from tool arguments or model output. A denied
call returns a structured tool result and does not invoke its handler.

The effective set can only narrow: role cap ∩ trusted per-agent restriction ∩
active supervisor restriction. Plan metadata such as `tools_allowed` is a
planning hint, not an execution control. Restrictions and on-demand activation
belong to one run; they are not shared executor state.

| Role | Default visible and executable tools |
| --- | --- |
| Plan | `file_read`, `file_list`, `glob_search`, `grep_search`, `web_search`, `web_fetch`, `tool_search`, `rag_search`, `knowledge_list`, `knowledge_search`, `kg_search`, `knowledge_extract_code` |
| Do | Registered built-ins in the configured role groups except `knowledge_delete` and `ontology_register` |
| Check | `file_read`, `file_list`, `workspace_status`, `read_agent_output`, `glob_search`, `grep_search`, `rag_search`, `kg_search`, `web_search`, `web_fetch`, `tool_search`, `knowledge_list`, `knowledge_search`, `knowledge_extract_code`, `knowledge_query`, `knowledge_neighbors`, `kb_vector_search` |
| Act | `file_read`, `file_list`, `glob_search`, `grep_search`, `rag_search`, `kg_search`, `tool_search`, `knowledge_list`, `knowledge_search`, `knowledge_extract_code`, `knowledge_query`, `knowledge_neighbors`, `kb_vector_search` |

Plan, Check, and Act are read-only by default. Check may execute `bash` only
when the server-owned `token_optimization.tool_groups.check_bash_enabled`
switch is explicitly enabled. This is risky: shell commands can execute
untrusted workspace content and may change the environment, so keep the
default `false` unless an operator accepts that risk. When enabled, this adds
only `bash` to Check's advertised and executable set; it does not enable the
Write group or other shell/editing tools. Audit warnings include only the
agent, role, and tool name—never tool arguments.

## Activation and cache behavior

Activation state is local to one run. The schema order is always resident
tools in registration order, then activated tools in first-activation order,
then dynamic micro-tools. Activated tools are never removed or reordered in a
run. This preserves the serialized resident prefix for prompt caching.

One search can activate at most `max_tools_per_activation` tools, and a run can
activate at most `max_activated_tools` tools. Defaults are 5 and 20. A search
result reports `activated` and, when applicable, `activation_skipped`.

Appending tools preserves the resident prefix but changes the content after
the append point. That later prompt content must be cached again, so callers
should use a small number of focused searches.

## Tool retrieval

`tool_search` queries the live tool registry rather than a fixed catalog. Its
deterministic lexical stage scores tool names, descriptions, parameter names,
parameter descriptions, and group names. Exact names and prefixes receive an
additional boost; score ties are ordered by tool name. It returns at most five
results by default and ten at most.

The returned metadata includes the tool name, group, one-line description,
visibility status (`resident`, `activated`, or `on_demand`), and retrieval
mode. The current mode is `lexical`. A future optional semantic-recall stage
must use a dedicated server-owned index and fuse its candidates with lexical
scores; it must never write to or query tenant knowledge namespaces. If an
embedding operation fails or returns an invalid vector, retrieval must retain
the lexical result and record the fallback rather than searching with a zero
vector.

The runtime supplies the caller role; `tool_search` accepts no role-selection
parameter. Missing or unknown runtime roles fail closed. Candidates are
filtered by both the role's exposure groups and its unchanged execution
allowlist before ranking, so a search result cannot reveal or activate a tool
that role cannot execute. Activation remains append-only and subject to the
existing per-run limits.

## Configuration and rollback

`token_optimization.tool_groups` configures role groups and activation limits.
Setting `enabled: false` restores the legacy fallback exposure behavior. The
gateway records cached prompt tokens from OpenAI-compatible
`prompt_tokens_details.cached_tokens` or `cache_read_input_tokens` and logs
the cumulative cache hit rate.

## Tool description standard

The per-turn function schema is the single source of per-tool definitions.
System prompts do not repeat a text menu. They retain the platform note in
the environment section and, when applicable, a stable directory of
on-demand groups.

### Source and enforcement

Descriptions and parameter descriptions are hand-written literals at builtin
registration. No runtime normalization occurs. The builtin set is recorded
from registration, not a maintained name list. Run
`cargo test --lib tool_description_lint` to check the raw definitions.
External and generated registrations only log per-tool warnings at runtime;
they are never changed or blocked.

### Rules

- Names use lowercase snake case, are unique, and avoid numeric version
  suffixes and generic names.
- Descriptions are 80–600 bytes, with literal `Use when:` and `Not for:`
  markers.
- Confusable tools name a real same-family neighbor in `Not for:` and explain
  when to use that neighbor instead.
- Parameters are an object schema with descriptions for every property;
  `required` names exist in `properties`. Booleans state their actual default.
  Path, URL, regex, glob, and enum-like inputs state their format or an
  already-enforced schema constraint.
- Each role's resident serialized schema stays within 48,000 bytes. External
  descriptions are checked for excessive Jaccard similarity in tests.

### Confusable families

| Family | Tools |
| --- | --- |
| Search | `grep_search`, `glob_search`, `rag_search`, `kg_search`, `kb_vector_search`, `knowledge_search`, `knowledge_query`, `knowledge_list` |
| Write | `file_write`, `file_edit` |
| Shell | `bash`, `powershell` |
| Web | `web_search`, `web_fetch` |
| Import | `knowledge_import_file`, `knowledge_import_url`, `knowledge_import_directory`, `knowledge_import_json` |
| Ontology validation | `ontology_validate_turtle`, `ontology_lint_turtle`, `ontology_validate_shacl` |

### Exemptions and example

Generated micro-tools are exempt from the markers but still require parameter
descriptions. Any other exemption needs a reason in `LINT_EXEMPTIONS` and an
approved increase to `MAX_LINT_EXEMPTIONS` (currently zero); tests enforce both.

Before: `file_write` said `Write content to a file.`

After: `Write the complete supplied content to a file, creating or replacing
it. Use when: a whole file must be written. Not for: a targeted change in an
existing file; use file_edit.`
