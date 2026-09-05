# Wild AgentOS 
<div align="center">

<img src="assets/logo_transparent.png" width="120" alt="Wild AgentOS Logo" />

**An Industrial-Grade AI Agent Operating System Built in Rust**  [![Star on GitHub](https://img.shields.io/github/stars/skaiy/wild_agentos?style=flat)](https://github.com/skaiy/wild_agentos)


[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![gRPC](https://img.shields.io/badge/gRPC-Protocol-green.svg)](https://grpc.io/)
[![Knowledge Graph](https://img.shields.io/badge/Knowledge%20Graph-Oxigraph-purple.svg)](https://oxigraph.org/)
[![Release](https://img.shields.io/badge/release-v0.2.0-blue)](https://github.com/skaiy/wild_agentos/releases)

---

[**English**](README.md) · [**中文**](README.zh.md) · [**Design Detail →**](docs/13-DESIGN_DETAIL.md)

<img src="assets/github-readme.png" alt="Wild AgentOS" width="100%" />

</div>

---

## 🎉 Release History & Changelog

Welcome to the release timeline of **Wild AgentOS**, featuring production-grade security gateway layers, partial tenant-scoped storage wiring, and an advanced cognitive operating system kernel. See the [Isolation Contract](docs/17-isolation-contract.md) for the current boundaries and unmigrated historical keys.

| Version | Release Date | Key Upgrades & Fused Features |
|---------|--------------|------------------------------|
| **v0.2.0** | **2026-09-05** | **Control Plane + Skill CI**<br>• Adds the Skill package format and CI gate: verification plus golden input/output checks, with the Judge hook disabled by default.<br>• Passing packages publish through the gated tenant channel; failing fixtures are blocked.<br>• Adds Rust-CI golden evaluations for Agent plans, Skill Markdown, and Action invocation via `scripts/test_golden.sh`.<br>• Companion Admin #16 delivered the separate-repository five-screen control-plane skeleton; this release does not change that repository's scope. |
| **v0.1.8** | **2026-09-04** | **Ontology Action HITL**<br>• Adds configurable `commit_strategy` for automatic or approval-held Action staging, with merge/discard APIs and TTL expiry.<br>• Adds configurable guardrails, SPARQL `ASK` assertions, and a `high_risk` approval hook.<br>• Publishes `ACTION_AUDIT` EventBus events for committed, pending, approved, rejected, and violated outcomes; see the [Ontology Action Data Sandbox](docs/15-ontology-action-sandbox.md). |
| **v0.1.7** | **2026-09-04** | **Isolation Proof & Eval**<br>• Adds the read-only `isolation-diagnose` CLI, customer-readable [Isolation Matrix](docs/17-isolation-matrix.md), and fail-closed `isolation_contract` CI coverage.<br>• Adds optional explicit `isolation-migrate`; it never uses a silent `UNION` and does not claim historical keys were migrated. |
| **v0.1.6** | **2026-09-04** | **Isolation & Hardening**<br>• Verified JWT tenant/project claims now mint graph, blob, vector, and L0 targets; HTTP KG, ontology, KB, chat RAG, and runtime graph/vector tools are claims-scoped and fail closed when claims are absent.<br>• New user agents receive verified scope; public API-key chat is non-tenant-RAG.<br>• Adds optional `AGENTOS_TENANT_TOOL_CALL_CAP`, MCP disclosure, bash environment sanitization, tool-schema enforcement, and PDCA L0 envelopes.<br>• See the [Isolation Contract](docs/17-isolation-contract.md); historical keys are not migrated. |
| **v0.1.5** | **2026-08-18** | **Cognitive Causal Engine & Advanced Graph Governance**<br>• **Causal Engine**: Standalone causal reasoning subsystem (`CausalEngine`, `FusionEngine`, `CausalStore`) to trace root causes and compute causal graphs of agent decisions.<br>• **Unified Graph Backend**: Consolidated fragmented graph operations into a single high-performance `GraphBackend`.<br>• **Graph Features**: Structural feature computation (PageRank, PageRank vector, centrality) and similarity scoring between cognitive snapshots.<br>• **Snapshot Timeline**: Temporal snapshot versioning with diff-based rollback and point-in-time state restoration.<br>• **Skill Center CRUD & Guard**: New client-side skill editing/deletion support, detail schema rendering, and strict **403 Forbidden** guards protecting system-level (`iri://`) builtins. |
| **v0.1.4** | **2026-07-06** | **Model Registry Center (3-in-1 Consolidation) & Dynamic Ingestion**<br>• **Consolidated Model Registry**: Merged gateway, embedding, and resource mapping settings into a unified "Model Registry Center".<br>• **Auto Model Discovery**: Automatic endpoint model schema discovery (`/v1/models`) and keyword-matching modaly pre-evaluation.<br>• **Vector Service Bridge**: Dynamic hot-swapping embedding models, triggering zero-downtime database rebuilds and background indexing. |
| **v0.1.3** | **2026-07-06** | **Multi-Modal Vision-Language (VL) Routing & Capability Slots**<br>• **Multi-Modal Gateway**: Automatic payload extraction (`ChatContent` parts) routing image payloads (Base64/URL) to VL models.<br>• **Agent Capability Slots**: Multi-model slot assignments per agent (e.g. Chat Slot → DeepSeek-V4, Vision Slot → Gemini-Pro). |
| **v0.1.2** | **2026-07-05** | **Multi-Tenant Knowledge Ingestion & Unified Knowledge Packages**<br>• **Two-Phase Ingestion**: Concurrent multi-file chunking upload to vector databases, and structural CSV/N-Triples graph imports to named graphs.<br>• **Knowledge Package Mounting**: Decoupled individual graph path binding, unifying knowledge resources into multi-pack `knowledge_pack_ids` for structured routing.<br>• **Isolation status**: this release entry does not mean current live keys were migrated; see the Isolation Contract. |
| **v0.1.1** | **2026-07-04** | **API Key Governance Center & One-Click Publishing**<br>• **API Key Governance**: Real-time client credentials management, quota limits enforcement, security audit logs, and access scopes.<br>• **OpenAI-Compatible Gateway**: One-click agent publishing with compatible endpoints (`/v1/chat/completions`) and SSE stream routes. |
| **v0.1.0** | **2026-07-01** | **Initial Release — Core OS Engine & Hyperspace Vector Storage**<br>• **HyperspaceEngine**: Embedded HNSW vector database with WAL and Poincaré/Lorentz metrics.<br>• **Skill Graph & Blackboard**: 5W2H semantic skill hypergraph, L0-L3 memory cache hierarchy with MESI coherence.<br>• **Workspace Monitor**: Real-time file system triggers and proactive perception engine. |

---

## What Is Wild AgentOS?

An **AI agent operating system** built in Rust that orchestrates multiple agents via the PDCA cycle, enabling coordinated, auditable, and self-improving systems.

> "We don't just build agents; we build the **infrastructure that harnesses their collective intelligence**."

### Core Architecture

| Layer | Technology | Role |
|-------|-----------|------|
| **Core Coordination** (Rust) | `PDCA cycle` · `5W2H ontology` · `EventBus` | Agent orchestration & lifecycle |
| **Skill Graph** | `RDF` · `6 link types` · `18 modules` | Dynamic cognitive network |
| **Memory System** | `L0 redb` · `L1 Session` · `L2 Blackboard` · `L3 Projection` · `MESI coherence` | Hierarchical memory with prefetch |
| **Knowledge Graph** | `Oxigraph RDF` · `SPARQL 1.1` · `Code AST` · `Named Graphs` | Cross-subsystem unified store |
| **HyperspaceEngine** | `HNSW ANN` · `WAL` · `Poincaré/Cosine/Euclidean` · `Hybrid search` | Embedded vector embeddings |
| **Wild Code TUI** | `ratatui` · `crossterm` · `MCP` · `checkpoint/resume` | Terminal AI coding assistant |
| **Data Bus** | `JSON-LD 1.1` · `@id/@type/@context` · `Named Graphs` | Universal interoperability |
| **Gateway** | `gRPC` · `HTTP (OpenAI-compatible)` · `MCP` | Production interface |
| **Perception Engine** | `10 triggers` · `Anomaly dedup` · `5W2H constraint check` | Proactive monitoring |
| **Agent Workflow** | `PA/DA/CA` · `Tool system` · `Checkpoint` · `Tracked actions` | Multi-agent execution |

---

## 🔧 Key Highlights

### 1. HyperspaceEngine — Embedded Vector Engine
Production-grade spatial memory engine with **runtime-switchable metrics** (Poincaré, Cosine, Euclidean, Lorentz). Features **HNSW approximate nearest neighbor search**, CRC32-verified **Write-Ahead Log (WAL)** with 3 sync modes, **tangent-space pruning** for Poincaré ball search, JSON-LD metadata index with RoaringBitmap filters, and dual-space **hybrid search** (text × structural). A self-contained crate with zero external vector database dependencies.

### 2. Skill Graph Cognitive Network
Dynamic in-memory cognitive network with **6 semantic link types** (Prerequisite, Composition, Related, Alternative, Extends, Generalization). Includes **Poincaré structural embedding** computation from graph topology (prerequisite depth, tag fingerprinting), **hypergraph composition** with first-class `Hyperedge` and `CompositionType` (Sequential, Parallel, Conditional, Optional, Fallback), **graph algorithms** (PageRank, betweenness centrality, label-propagation community detection, DFS prerequisite chains, Tarjan SCC cycle detection), **causal failure analysis** with root cause inference, **formal invariant verification** (6 checks: acyclicity, link existence, composite reachability, no deprecated prereqs, valid 5W2H, valid security levels), and **temporal versioning** with snapshot/rollback.

### 3. Generalized PDCA — 7-Level Adaptive Execution
Dynamically selects from 7 complexity levels (L0 instant → L5 recursive → L6 emergency) via 5W2H metadata. One engine handles everything from instant queries to multi-week projects — no rigid workflows. **PA/DA/CA agent roles** with template-driven prompt construction.

### 4. CPU Cache-Inspired Memory — 4 Layers + MESI Coherence
First-ever application of CPU cache coherence protocol to multi-agent memory. **L0** redb disk KV + HyperspaceEngine vectors → **L1** session context → **L2** Oxigraph RDF + Blackboard → **L3** SPARQL projection cache. Intelligent prefetch engine reduces perceived latency by 90%. Solves context explosion and shared memory inconsistency across concurrent agents.

### 5. JSON-LD Universal Data Bus — W3C-Standard Interoperability
`@context` duck-typing eliminates field name conflicts between skills. `@id` enables zero-cost cross-agent entity merging. `@graph` named graphs allow conflict-free parallel writes across subsystems. Turns interoperability hell into plug-and-play.

### 6. Self-Evolving Skill Graph — Autonomous Learning
AA agents create **knowledge fragments** and new semantic links after each task completion. `/learn` and `/reduce` mechanisms enable autonomous skill acquisition and consolidation. `BootstrapEngine` ingests markdown skills from the filesystem.

### 7. Universal Knowledge Graph — Unified Cognitive Backbone
All subsystems (skills, memories, tasks, code knowledge) share a single **Oxigraph RDF store** via named graphs, enabling cross-subsystem SPARQL joins. Code ASTs parsed by tree-sitter are automatically converted to RDF triples. **Bidirectional SPARQL sync** from `SkillGraphStore` keeps the cognitive graph in sync with the semantic store.

### 8. Semantic Skill Discovery Engine
`SkillDiscoveryEngine` wraps `HyperspaceStore` for vector-based semantic search across skills. `suggest_links()` falls back from Jaccard tag overlap to cosine similarity via embedding vectors. Includes BFS path finding (`find_skill_chain()`), composition tree construction (`get_skill_tree()`), and conflict detection.

### 9. 5W2H Dimension-Level Audit — Precision Rollback
CA audits each of the 7 dimensions independently. What/Why fail → re-analyze. How/Where fail → re-plan. When/HowMuch fail → conditional pass. No more black-box "PASS/FAIL" — you know exactly what went wrong.

### 10. Proactive Perception Engine
10 execution triggers with 60-second anomaly deduplication. Monitors deadline violations, budget overruns (>80% tokens), role mismatches, environment conflicts. **Workspace Monitor** detects file creations/modifications/deletions in real-time. Auto-escalates to human when needed.

### 11. Micro-Tool System — Tame Large Outputs
Results >8KB auto-generate conversational micro-tools (e.g., "search_in_results"). Transforms unwieldy 50KB+ outputs into interactive, queryable artifacts within the LLM context.

### 12. MCP Integration — One Protocol to Connect Them All
Standard **Model Context Protocol** connects GitHub, Slack, Jira, and any MCP-compatible server. Dynamic tool discovery at runtime. Supports both HTTP SSE and stdio transport modes with repeatable `--mcp-server` CLI flags.

#### Third-party MCP disclosure

An MCP server is a third party. It receives the JSON-RPC requests Wild AgentOS sends to it, including the selected tool name and arguments, and returns its tool results to the Agent. Only connect servers you trust with the task data you choose to send.

For `--mcp-server-stdio`, Wild AgentOS starts the server as a child process. That process inherits the Agent process environment, with any server-specific environment variables overlaid. Environment sanitization is tracked in [#26](https://github.com/skaiy/wild_agentos/issues/26) (implementation [#37](https://github.com/skaiy/wild_agentos/pull/37)); until that protection is applied to stdio MCP launches, do not put secrets in the Agent environment when starting a third-party server.

MCP failures are fail-loud: a failed connection is reported as `status=error:...`, clears any discovered tools, and returns an error to the caller. Failed tool calls are also returned as errors—they are not skipped and never replaced with a `simulated` result.

### 13. Checkpoint & Recovery — Crash-Proof Long-Running Tasks
Session state snapshots at critical points with full restoration on crash. Enables hour/day-long agent tasks and post-mortem replay debugging. `--resume <task_iri>` and `--list-checkpoints` commands for explicit session management.

### 14. Center + Edge Federation — Local Autonomy, Global Orchestration
Go Center handles workflow orchestration (Temporal), project management, agent registry. Rust Edge runs local LLM execution with Docker sandbox. VS Code Plugin provides real-time developer awareness. No single point of failure.

---

## 🖥️ Wild Code — The Terminal AI Assistant

**Wild Code** is a terminal-based AI coding assistant (`ratatui` TUI) that brings the power of Wild AgentOS's knowledge graph and agent orchestration directly into your command line — no IDE required.

**Features:**
- Interactive TUI with **Markdown rendering** (`tui-markdown`) and **mermaid diagram** support
- **MCP server integration** via `--mcp-server` and `--mcp-server-stdio` flags
- **Checkpoint/resume** with `--resume <task_iri>` and `--list-checkpoints`
- **Multi-model backends**: DeepSeek, OpenAI-compatible APIs
- **PDCA workflow execution** with plan/do/check/act cycles
- **Configurable** workspace, max iterations, max PDCA cycles, verbosity

![Wild Code Demo](assets/screenshot.gif)

![Knowledge Graph in Action](assets/wild_code_kg.JPG)
*Knowledge graph visualization — real-time entity relationships, code structure understanding, and cross-subsystem awareness powered by Oxigraph RDF*

![Completed Programming Task](assets/wild_code.JPG)
*Task completion interface — AI agent successfully analyzing and solving a programming task with full traceability*

---

## 🚀 Quick Start

### Download & Run — Wild Code

No dependencies required. Just download, extract, and run:

| Platform | Download |
|----------|----------|
| Linux (x86_64, musl) | [`wildcode-x86_64-unknown-linux-musl.tar.gz`](https://github.com/skaiy/wild_agentos/releases) (~15 MB) |
| Linux (aarch64, musl) | [`wildcode-aarch64-unknown-linux-musl.tar.gz`](https://github.com/skaiy/wild_agentos/releases) (~14 MB) |
| macOS (Apple Silicon) | [`wildcode-aarch64-apple-darwin.tar.gz`](https://github.com/skaiy/wild_agentos/releases) (~13 MB) |
| Windows (x86_64) | [`wildcode-x86_64-pc-windows-msvc.zip`](https://github.com/skaiy/wild_agentos/releases) (~12 MB) |

```bash
# Linux / macOS
tar xzf wildcode-*.tar.gz
./wildcode --help

# Windows (PowerShell)
Expand-Archive wildcode-x86_64-pc-windows-msvc.zip .
.\wildcode.exe --help
```

> All Linux builds are **fully statically linked** (musl) — no runtime dependencies required.

Set your API key and start using it:

```bash
export DEEPSEEK_API_KEY="sk-..."        # Linux / macOS
# or
set DEEPSEEK_API_KEY="sk-..."            # Windows (cmd)
# or
$env:DEEPSEEK_API_KEY="sk-..."           # Windows (PowerShell)

# Alternatively, use any OpenAI-compatible provider:
export AGENT_OS_GATEWAY_API_KEY="sk-..."
export AGENT_OS_GATEWAY_BASE_URL="https://your-endpoint/v1"

# Web search tool (powered by Exa):
# Get your free API key at https://exa.ai/docs/reference/team-management/get-api-key
# Falls back to DuckDuckGo (unreliable in China, not recommended for Chinese users)
export EXA_API_KEY="your-exa-api-key"

# Run an interactive session
./wildcode

# Or run a one-shot task
./wildcode "Explain how Rust's borrow checker works"

# With MCP server attached
./wildcode --mcp-server chrome=http://localhost:3000/sse

# Resume from checkpoint
./wildcode --resume task:abc123
```

### Build from Source

```bash
git clone https://github.com/skaiy/wild_agentos.git
cd Wild_AgentOS

# Build the wildcode binary (release, ~51 MB)
cargo build -p wild-code-cli --release
./target/release/wildcode --help
```

---

## 🗺️ Roadmap

Wild AgentOS is a **semantic-kernel AgentOS**: Rust PDCA orchestration with Oxigraph RDF/SPARQL, Hyperspace, the `IsolationClaims` naming contract, and an ontology Action **data** sandbox. It is not a bare-metal microkernel OS or a full Palantir clone; it does not replace Oxigraph with Nebula/Cypher, mix separate product/business repositories or product boundaries into this open-source tree, or confuse minting names with migrating historical data. See the [Evolution Roadmap](docs/18-evolution-roadmap.md) and [Isolation Contract](docs/17-isolation-contract.md).

- **v0.1.6 — done:** JWT `IsolationClaims` mint graph/blob/vector/L0 targets; relevant HTTP paths fail closed; historical keys are not migrated.
- **v0.1.7 — done:** read-only minted-vs-historical diagnose CLI, customer-readable isolation matrix, fail-closed golden CI, and optional explicit migration with no silent `UNION`.
- **v0.1.8 — done:** approval-held Action staging with merge/discard APIs and TTL, configurable guardrails plus SPARQL assertions, and `ACTION_AUDIT` event-bus audit.
- **v0.2.0 — done:** Skill package verification, golden checks, default-off Judge hook, and gated tenant publishing; Rust-CI Agent/Skill/Action golden evaluations; companion Admin #16 delivered the separate-repository five-screen control-plane skeleton.
- **[v0.2.1 Ontology Data + Protocols](https://github.com/skaiy/wild_agentos/milestone/4):** human-approved ObjectType/LinkType drafts, MCP inbound tenant catalog, Skill-as-MCP, and a thin outbound A2A adapter.
- **[v0.2.2 Artifacts + Sandbox + Bench](https://github.com/skaiy/wild_agentos/milestone/5):** claims-scoped coding artifacts, an external compute-sandbox adapter, and reproducible weak-compute benchmarks without fabricated speedups.
- **[v0.3.0 Markets + IdP + Emergent](https://github.com/skaiy/wild_agentos/milestone/6):** versioned Function/Skill market, OIDC/IdP with claims minted at the auth boundary, gated emergent tools, and optional default-off limited OWL/rules.

---

## 📊 Performance Targets

| Operation | Latency | Throughput |
|-----------|---------|-----------|
| L2 Node Write (Oxigraph) | ~2ms | 500 ops/sec |
| L3 SPARQL Projection | ~15ms | 66 ops/sec |
| L0 redb KV Read | ~1ms | 1000 ops/sec |
| Hyperspace HNSW Search (10K vectors) | ~1ms | 1000 qps |
| Poincaré Embedding (4D) | ~50µs | — |
| Agent ReAct Turn | 1-5s | 0.2-1 turns/sec |
| Idle Memory | ~200MB | scales with tasks |

---

## 📚 Documentation

- **Memory System** → [`docs/03-memory-system.md`](docs/03-memory-system.md) (L0 redb · HyperspaceEngine · Oxigraph SPARQL)
- **Isolation Contract** → [`docs/17-isolation-contract.md`](docs/17-isolation-contract.md) (verified claims and future naming; no storage migration)
- **Isolation Matrix** → [`docs/17-isolation-matrix.md`](docs/17-isolation-matrix.md) (CI-verified fail-closed behavior; historical keys are not migrated)
- **Evolution Roadmap** → [`docs/18-evolution-roadmap.md`](docs/18-evolution-roadmap.md) (post-v0.1.6 strategy and explicit non-goals)
- **Ontology Action Data Sandbox** → [`docs/15-ontology-action-sandbox.md`](docs/15-ontology-action-sandbox.md) (staging graph guardrails; not a compute sandbox)
- **Design Detail** → [`docs/13-DESIGN_DETAIL.md`](docs/13-DESIGN_DETAIL.md) · [`docs/13-DESIGN_DETAIL.zh.md`](docs/13-DESIGN_DETAIL.zh.md) (中文)
- **Core Design Philosophy** → [`docs/CORE_DESIGN_PHILOSOPHY.md`](docs/CORE_DESIGN_PHILOSOPHY.md) · [`docs/CORE_DESIGN_PHILOSOPHY.zh.md`](docs/CORE_DESIGN_PHILOSOPHY.zh.md) (中文)
- **gRPC Proto** → [`proto/pdca_core.proto`](proto/pdca_core.proto)

---

## 🤝 Contributing

We welcome contributions from the community!

- **🐛 Report bugs**: [GitHub Issues](https://github.com/skaiy/wild_agentos/issues)
- **💡 Propose ideas**: [GitHub Discussions](https://github.com/skaiy/wild_agentos/discussions)
- **🔀 Submit PRs**: Fork → feature branch → PR against `main`

```bash
git checkout -b feat/my-feature
# Make your changes
cargo fmt && cargo clippy  # Keep code clean
cargo test                 # Ensure nothing breaks
git commit -am 'Add my feature'
git push origin feat/my-feature
```

All contributors are expected to adhere to our [Code of Conduct](docs/CODE_OF_CONDUCT.md).

---

## 📄 License

Wild AgentOS is dual-licensed:

- **Community Edition** — [GNU AGPL v3.0](LICENSE) (see also [NOTICE](NOTICE)). If you run a modified version over a network, AGPLv3 Section 13 requires you to offer the complete corresponding source to its users.
- **Commercial Edition** — commercial use that cannot comply with the AGPLv3 requires a separate commercial license; see [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md).

For commercial licensing inquiries, contact **diaoguoliang@gmail.com**.

Contributions require signing our [Contributor License Agreement](CLA.md), handled automatically by CLA Assistant on your first pull request.

Copyright (c) 2026 skaiy (diaoguoliang@gmail.com).
