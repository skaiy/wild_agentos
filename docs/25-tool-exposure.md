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
the write-capable resident group; Check receives shell tools on demand rather
than as resident tools. Role visibility is separate from execution policy.

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
