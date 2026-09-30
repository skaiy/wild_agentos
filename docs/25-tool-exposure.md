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
the write-capable resident group; Check does not receive shell tools by
default. Role visibility is separate from execution policy.

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

## Configuration and rollback

`token_optimization.tool_groups` configures role groups and activation limits.
Setting `enabled: false` restores the legacy fallback exposure behavior. The
gateway records cached prompt tokens from OpenAI-compatible
`prompt_tokens_details.cached_tokens` or `cache_read_input_tokens` and logs
the cumulative cache hit rate.
