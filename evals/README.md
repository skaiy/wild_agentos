# Golden evaluation suite

This directory contains deterministic regression cases for kernel behavior.
Run the focused suite from the repository root:

```sh
./scripts/test_golden.sh
# equivalent: cargo test --workspace golden --verbose
```

Current inventory:

- `golden/agent-plans.json`: heuristic Agent task classification and PDCA role plans.
- `golden/skill-markdown.json`: static Skill Markdown parsing, including required and optional parameters.
- `golden/action-invocation.json`: Action dry-run response and the guardrail ownership boundary.
- Isolation remains covered by the existing `isolation_contract` suite; run it with
  `cargo test --workspace isolation_contract --verbose`.

## Adding a case

1. Add a fixed JSON fixture under `evals/golden/` with a stable case `id`, input, and expected observable result.
2. Add or extend a test named `golden_*` that loads that fixture. Keep the assertion focused on the public contract: status, planned roles, parsed schema, generated SPARQL shape, or persisted state.
3. Run `./scripts/test_golden.sh` and the related focused test target.

Do not make live LLM output a hard assertion. LLM-dependent cases must use a mock response or assert deterministic heuristic/structural gates instead. Do not put credentials, network-only inputs, timestamps, UUIDs, or performance thresholds into expected output.
