# Contributing to Wild AgentOS

Welcome! Wild AgentOS is an open-source Agent Operating System written in Rust. We're excited that you want to contribute. This document will help you get set up, understand our development process, and land your first contribution.

## Table of Contents

1. [Code of Conduct](#code-of-conduct)
2. [Development Environment Setup](#development-environment-setup)
3. [Project Structure](#project-structure)
4. [Adding New Features](#adding-new-features)
   - [Extending the Skill Graph](#extending-the-skill-graph)
   - [Adding a New Batch Agent](#adding-a-new-batch-agent)
   - [Adding a New Tool](#adding-a-new-tool)
5. [Coding Conventions](#coding-conventions)
6. [Testing](#testing)
7. [Commit Guidelines](#commit-guidelines)
8. [Pull Request Process](#pull-request-process)
9. [Contributor License Agreement](#contributor-license-agreement)
10. [Documentation](#documentation)
11. [Getting Help](#getting-help)

## Code of Conduct

We expect all contributors to follow our [Code of Conduct](docs/CODE_OF_CONDUCT.md) ([中文](docs/CODE_OF_CONDUCT.zh.md)). Be respectful, inclusive, and constructive.

Security vulnerabilities must not be reported through public issues or pull requests — follow the process in [SECURITY.md](SECURITY.md) instead.

## Development Environment Setup

### Prerequisites

- **Rust** 1.78 or later (install via [rustup](https://rustup.rs))
- **Oxigraph** (compiled automatically as a Rust dependency; RDF graph store queried with SPARQL 1.1)
- **redb** (compiled automatically; L0 persistent key-value store in `src/memory/l0_store.rs`)
- **hyperspace-engine** (workspace crate; embedded HNSW vector index — no external vector database)
- **Docker** (optional, for sandboxed tool execution)

### Quick Start

```bash
# Clone the repository
git clone https://github.com/skaiy/wild_agentos.git
cd Wild_AgentOS

# Copy example environment file
cp .env.example .env

# Build the project (Oxigraph, redb, and hyperspace-engine compile as Rust deps)
cargo build --release

# Run the test suite
cargo test
```

### IDE Setup

We recommend using [VS Code](https://code.visualstudio.com/) with the [rust-analyzer](https://rust-analyzer.github.io/) extension. Open the project root, and rust-analyzer will pick up the configuration automatically.

## Project Structure

The project follows a layered architecture. Here are the key directories:

```
Wild_AgentOS/
├── api/                    # API definitions (gRPC / REST interfaces)
├── batch/                  # Batch Agent background maintenance system
├── config/                 # Configuration files (agent roles, templates, rules)
├── core/                   # Core engine (SA scheduler, AgentRunner, PDCA orchestration)
├── gateway/                # Syscall gate (SyscallGate / ToolGuard / StageGate)
├── jsonld/                 # JSON-LD semantic engine (context, framing, type routing)
├── llm/                    # LLM communication layer (SSE streaming, response parsing)
├── memory/                 # Four-level memory system (L0–L3, MESI consistency)
├── methodology/            # Engineering methodology (5W2H, Jikotei Kanketsu, TPS principles)
├── perception/             # Perception engines (situation, health, patterns, conflict)
├── root_cause/             # Root cause analysis engine (RootCauseEngine)
├── skill_graph/            # Skill graph (store, discovery, evolution, conflict detection)
├── templates/              # Prompt templates (agent roles, batch agents, design docs)
├── tools/                  # Tool system (ToolExecutor / MCP / Hooks / result router)
├── utils/                  # Common utility functions
├── worker/                 # Task workers and sandbox execution
├── lib.rs                  # Library root entry point
├── main.rs                 # Main program entry point
└── permissions.rs          # Permissions and role-based whitelist definitions
```

## Adding New Features

### Extending the Skill Graph

If you want to add a new link type, skill node type, or evolution rule:

1. Define new types in `src/skill_graph/types.rs`
2. If it’s a new link type, add it to the `SkillLinkType` enum and implement its logic in the relevant engine (e.g., discovery, conflict)
3. Update the JSON-LD representation in `src/skill_graph/graph_store.rs`
4. Add tests in `tests/skill_graph/`

### Adding a New Batch Agent

You can create a new background maintenance agent without modifying the framework:

1. Create a new template file in `templates/prompts/batch/` (e.g., `my_agent.md`)
2. Add a configuration entry in `config/batch_agents.yaml` with a unique name, trigger schedule, and the template name
3. Implement the result handler function in `src/batch/handlers.rs` and register it in the `dispatch_handler` function
4. Add tests to verify that the agent correctly reads data, calls the LLM, and writes results

No changes to the batch framework itself are required.

### Adding a New Tool

1. Implement the tool logic as an async function with the signature `Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>> + Send + Sync>`
2. Register it in `ToolExecutor::register()` in `src/tools/tool_executor.rs`
3. Define its JSON Schema and role permissions
4. If it’s an MCP tool, add the server configuration in `src/tools/mcp_client.rs`

## Coding Conventions

- **Rust Style**: Follow the official [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/). Run `cargo clippy` and `cargo fmt` before submitting.
- **Error Handling**: Use `anyhow` for application-level errors and custom error types for library crates. Avoid unwrap in production code; use proper error propagation.
- **Async/Await**: All I/O and tool calls must be async, using Tokio as the runtime.
- **JSON-LD**: Use `@id` and `@type` consistently. All new data types must have a registered IRI in the ontology.
- **Documentation**: Public items must have doc comments (`///`). Include examples where appropriate.
- **Naming**: Use descriptive, snake_case for functions and modules; CamelCase for types; IRIs in kebab-case.

## Testing

We rely heavily on automated tests:

- **Unit tests**: Run with `cargo test`. Place tests inside the module they test (e.g., `#[cfg(test)] mod tests { ... }`)
- **Integration tests**: Located in `tests/`. These test full workflows (e.g., a complete PDCA cycle).
- **Memory tests**: L0 uses redb (file-backed; tests typically use a temp directory). The graph layer is Oxigraph (SPARQL 1.1); in-memory stores can run in parallel without conflict. Use `Arc<Mutex<Store>>` for shared Oxigraph state. Vector search uses the in-repo `hyperspace-engine` crate.
- **LLM mocking**: For tests that involve LLM calls, we provide mock implementations in `src/testing/mock_llm.rs`. Use these to avoid network calls.

Before submitting, ensure:
```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## Commit Guidelines

We follow the [Conventional Commits](https://www.conventionalcommits.org/) specification:

```
feat(skill_graph): add new link type for composition
fix(memory): resolve MESI race condition on invalidate
docs(contributing): update setup instructions
```

Types include: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`, `perf`, `ci`.

## Pull Request Process

1. **Open an Issue**: Before writing significant code, open an issue to discuss your idea. This avoids wasted effort.
2. **Create a Branch**: Branch from `main` using a descriptive name (e.g., `feat/skill-graph-composition-links`).
3. **Keep Changes Focused**: Each PR should address a single concern. Don’t bundle unrelated changes.
4. **Update Documentation**: If your change affects user-facing behavior, update the relevant docs.
5. **Add Tests**: All new functionality must include tests. Bug fixes should include a regression test.
6. **Run CI Checks**: Ensure `cargo test`, `cargo clippy`, and `cargo fmt` pass.
7. **Sign the CLA**: On your first PR, the CLA Assistant bot will ask you to sign our [Contributor License Agreement](CLA.md). See below.
8. **Request Review**: Assign the PR to a maintainer. Describe what you did and why, linking to the issue.

## Contributor License Agreement

Wild AgentOS is dual-licensed: the Community Edition under [AGPLv3](LICENSE), and commercial use that cannot comply with the AGPLv3 under a [separate commercial license](LICENSE-COMMERCIAL.md). To keep this model workable, every contributor must sign our [Contributor License Agreement](CLA.md).

The process is automated — no paperwork:

1. Open a pull request.
2. The CLA Assistant bot comments on the PR if you have not signed yet.
3. Reply with the exact comment: `I have read the CLA Document and I hereby sign the CLA`
4. Your signature is recorded in `signatures/version1/cla.json` and the PR status check turns green.

You only need to do this once. If the check does not update, comment `recheck` to re-run it.

## Documentation

- **README.md** / **README.zh.md**: Project overview, quickstart, and architecture summary.
- **docs/03-memory-system.md**: Memory stack — L0 redb, HyperspaceEngine vectors, Oxigraph SPARQL graph store.
- **docs/**: Detailed guides for each subsystem (memory, skills, tools, etc.).
- **Code comments**: Explain the “why”, not the “what” (the code already shows what).

## Getting Help

- **GitHub Issues**: For bugs, feature requests, or questions.
- **Discussions**: For broader architecture discussions or brainstorming.
- **Email**: Reach out to the core team at [diaoguoliang@gmail.com](mailto:diaoguoliang@gmail.com).

We look forward to your contributions! Together, we can build the most reliable Agent Operating System.