# 30. Durable execution

> *A Chinese version is available in [30-durable-execution.zh.md](30-durable-execution.zh.md).*

> **Status: design draft for issue [#367](https://github.com/skaiy/wild_agentos/issues/367).** It specifies a disabled-by-default durable-execution path. It does not change the current restart behaviour.

## 1. Decision and scope

The default single-node implementation is a small `DurableExecutor` boundary
backed by embedded [duroxide](https://github.com/microsoft/duroxide). It runs
inside this process's Tokio runtime and uses one SQLite file per tenant. The
file path is minted from verified `IsolationClaims`; without claims, no durable
database is opened. That is fail-closed.

`durable-temporal` is an off-by-default optional adapter for customers that
operate an external Temporal Server and need multi-node availability. The
server is never shipped in this image. The embedded path does not elect leaders
or promise multi-node scheduling.

Goals:

- an invocation can survive a normal restart or `kill -9` and reach a terminal
  outcome;
- recovery neither duplicates metered LLM usage nor blindly repeats an external
  side effect;
- claims, cancellation, deadline, budget, audit and approval rules remain
  effective after a restart.

Non-goals:

- exactly-once delivery to arbitrary external systems;
- making engine history an audit record or an authorization source;
- bundling Temporal, supporting an excluded engine, or changing the public
  Invocations HTTP API to expose engine concepts.

`invocations.resume_on_restart` remains `false` until every gate in §13 has
passed. `EXECUTION` remains off by default.

## 2. Current main baseline

The baseline was checked on this branch, not copied from the earlier strategy.

| Location | Observed current behaviour | Durable-execution consequence |
| --- | --- | --- |
| `src/core/checkpoint.rs` | `PdcaCheckpointEnvelope` is immutable, is written with verified claims at PDCA boundaries, and `read_pdca_resume` rejects absent or mismatched claims. `PdcaResumeState` reconstructs completed steps. | Step-level business checkpoints already exist. |
| `src/api/http/invocations_store.rs` | At startup, `InvocationStore::open_with_config` converts every non-terminal `queued`, `running`, or `cancel_requested` record to `failed/interrupted`, increments its revision, and audits the transition. It does not resume work. | Recovery must replace this privileged terminalisation only when the durable gates pass. |
| `src/api/http/invocations_execution.rs` | The bridge recreates verified claims from persisted invocation fields, applies in-process FIFO admission, starts the task, enforces deadline and budget, and has only an in-memory cancellation registry. | It has no durable instance history, replay or restart-safe cancellation signal. |
| `src/api/http/invocations_enforcement.rs` | Global/per-tenant/per-scope running caps and deadline watchers are process-local. | Recovery must reacquire a slot; counters cannot be trusted across a restart. |
| `src/memory/l0_store.rs` | `TenantL0Registry` shares one writable redb handle per tenant in a process; the key is minted from claims. redb's file lock prevents a second open. | The side-effect ledger must use this claims-scoped handle. |
| `src/tools/tool_executor/mod.rs` | `register` accepts name, description, JSON schema, handler and roles; `execute_guarded` applies advertised-tool, security and role-policy gates. | The tool registry has no per-tool replay-policy metadata yet. |
| `src/worker/task_queue.rs` | It uses `yaque` senders/receivers rooted at caller-selected filesystem paths. | The earlier claim that it is only in-memory is no longer accurate; it is not, however, the Invocation recovery authority or a PDCA replay log. |

There is no `resume_on_restart` or `resume_max_age` configuration today, and
no OpenTelemetry SDK/`traceparent` integration in `src/`. `Cargo.toml` uses
rustls-only `reqwest`; the distroless image deliberately has no OpenSSL runtime.

## 3. Architecture and ownership

The HTTP store remains the public lifecycle authority. The durable instance id
is the invocation id. WAO owns authorization, the side-effect ledger, audit
events, policy-set pinning, approval routing, and L0 checkpoint envelopes.
The engine owns workflow history, timers, activity delivery and instance locks.
Neither engine may write `InvocationStore` directly, and claims in an engine
history are never trusted.

```rust
#[async_trait]
pub trait DurableExecutor: Send + Sync {
    fn capabilities(&self) -> DurableCapabilities;
    async fn start(&self, request: DurableStart) -> Result<DurableHandle, DurableError>;
    async fn signal(&self, id: &InvocationId, signal: DurableSignal) -> Result<(), DurableError>;
    async fn query_status(&self, id: &InvocationId) -> Result<DurableStatus, DurableError>;
    async fn resume_all_for_tenant(&self, claims: &IsolationClaims) -> Result<ResumeReport, DurableError>;
    async fn purge(&self, claims: &IsolationClaims, before: DateTime<Utc>) -> Result<PurgeReport, DurableError>;
}

pub struct DurableCapabilities {
    pub durable_timers: bool,
    pub external_signals: bool,
    pub deterministic_replay: bool,
    pub multi_node: bool,
    pub instance_leases: bool,
    pub versioning: bool,
}
```

`DurableStart` carries an invocation id, an immutable claims snapshot digest,
definition/policy digests, deadline and serialized non-secret input reference.
It does not carry credentials. `DurableSignal` is `Cancel` or
`Approval { staging_id, outcome }`. A capability probe happens at startup; an
engine without deterministic replay, timers, signals and leases is rejected.

Each tenant gets a lazily-created, bounded runtime and
`<claims-derived-tenant-path>/durable.db`. The claims-derived path may not be
provided by HTTP input, engine history, or configuration interpolation. A
process epoch plus an exclusive SQLite/file lock prevents two local processes
from owning a tenant runtime. Engine instance leases prevent concurrent turns.
Lock failure is a fail-closed recovery error, not a retry loop that runs two
owners. Temporal advertises `multi_node = true`; embedded duroxide does not.

## 4. Engine mapping

| Concern | Duroxide default | Temporal adapter |
| --- | --- | --- |
| Invocation orchestration | One registered orchestration, `invocation_id` as instance id | One Workflow Execution, `invocation_id` as Workflow ID |
| PDCA Plan/Do/Check/Act | Deterministic orchestration transition; commit checkpoint activity after each completed step | Deterministic Workflow transition; checkpoint Activity after each completed step |
| LLM call | `llm_call` activity; result comes from history on replay | `llm_call` Activity; result comes from Event History on replay |
| Tool call | `tool_call` activity, preceded by ledger intent | `tool_call` Activity, preceded by ledger intent |
| Human review | External event and durable timer race | Signal and timer race |
| Deadline | Durable timer races work | Workflow timer races work |
| Cancel | Durable signal plus cooperative activity cancellation | Workflow signal/cancellation plus Activity cancellation |
| Child work | Sub-orchestration where needed | Child Workflow where needed |
| Versioning | Registered orchestration version kept for all live instances | Worker deployment/versioning and compatible Workflow code |

The mapping is deliberately behavioural rather than an API wrapper. Names in
the table are the proposed WAO activity names, not a promise that both vendor
APIs have identical type signatures.

## 5. Determinism, data retention, and checkpoints

Orchestrator code is a pure state machine: PDCA progression, policy-level
decision, cumulative budget comparison, signal handling and selection of the
next activity. It must not call I/O, `Utc::now()`, random generators, direct
Tokio concurrency primitives, or secrets. It uses engine time, generated IDs
and deterministic select/join facilities. LLMs, tools, blob reads/writes,
approval delivery and every other I/O action are activities.

Enforcement is a review checklist, a lint/forbidden-import rule for workflow
modules, and replay tests that execute each recorded history twice. A
non-determinism error fails closed and creates an audit event.

Engine history is execution evidence. `PdcaCheckpointEnvelope` and the L0
ledger remain business/audit evidence. On resume, the completed checkpoint
activity set in engine history must exactly equal the completed envelope set.
Any missing, extra, malformed, cross-claim or out-of-order record is
`needs_attention`, not an inferred success.

History stores bounded metadata and blob references only: a 64 KiB maximum
serialized activity input/output record, with larger LLM/tool material in
claims-scoped blobs. Default history retention is 30 days after terminal
completion; ledger records with unknown outcomes are retained for 90 days;
purge is claims-scoped, audited, and may not purge a non-terminal instance.
Exact operational defaults are configuration values below and must be tested
before release.

## 6. Activity delivery and the side-effect ledger

Activities are at-least-once. An activity implementation may start again after
an uncertain worker failure, so an idempotency key alone is insufficient for an
unknown external outcome.

The tool registration contract gains required `side_effect` metadata:

| Policy | Recovery rule |
| --- | --- |
| `replay_safe` | Replay is permitted (normally read-only). |
| `reconcile` | Call a tool-specific authoritative `reconcile` operation first; replay only when it proves the effect did not occur. |
| `never_replay` | Replay only when the ledger proves execution never began; otherwise route to existing staging/human review. |

An absent declaration is `never_replay`. Sandbox execution is
`never_replay`. The ledger is an append-only L0 record keyed by
`(invocation_id, PDCA step, attempt)`, written before invocation as an
immutable Intent. A small mutable lease record records owner epoch, activity
attempt and observed result. Unknown completion becomes `outcome_unknown` and
`needs_attention`; it extends retention. Redacted or omitted replay parameters
also force `needs_attention`. Secret redaction follows the repository's
secret-guard module at implementation time.

LLM requests use a digest over canonical non-secret request fields: invocation
id, activity id, selected model, prompt/input blob digest, tool-schema digest,
temperature and policy/definition digests. It excludes headers, bearer tokens,
API keys and raw secret values. A completed `llm_call` returns its recorded
result during replay, and its usage is committed exactly once. A successful
invocation still requires the existing complete usage/cost contract; missing
gateway or configured-price-table cost remains fail-closed rather than `0`.

## 7. Recovery state machine

```text
queued ──start──> running ──terminal──> succeeded | failed | cancelled
                       │
                       └─ restart ─> interrupted
interrupted ──eligible, audited resume──> queued ──start──> running
interrupted ──gate failure──────────────> needs_attention
```

`interrupted → queued` is a new audited transition with reason `resume`. It
is permitted only when all conditions hold: the switch is on; the instance
exists; claims snapshot, definition digest and policy digest validate; engine
and envelope histories agree; the record age is at most `resume_max_age`; and
a running slot and tenant lease are acquired. No condition may substitute a
new tenant, project or actor.

On restart, persisted cancel intent is signalled before scheduling work. A
past deadline resolves terminally without invoking an activity. Cumulative
usage and budget are rebuilt from recorded activity results, never reset.
Failure to acquire a running slot returns to durable queued state; it does not
start a second execution. Approval waits race their TTL timer; an expired
approval becomes terminal or `needs_attention` according to the pinned policy.

`resume_max_age` defaults to `24h`. An expired invocation is not retried and
becomes `failed/interrupted` with an audit reason. A missing/forged snapshot,
missing claims, wrong claims-derived database, or cross-scope resume is
fail-closed; this is the `isolation_contract_resume` negative suite.

## 8. Binary upgrades and engine replacement

Every instance records `workflow_name`, `workflow_compatibility_version`,
engine identity/version and WAO build/definition/policy digests. Compatible
changes keep the old workflow implementation registered until all its
instances reach a terminal state. A change is incompatible when it changes
the history command sequence, payload interpretation, or replay-policy
meaning. It routes to human review rather than silently failing or running new
code against old history.

The CI upgrade drill starts with build N, interrupts after every PDCA boundary,
then resumes under N+1. It must either finish using the compatible version or
reach `needs_attention` with an audit record. A separate engine-swap drill
runs the same contract suite against `MockDurableExecutor` and duroxide; an
adapter is not accepted merely because it compiles.

## 9. Configuration

| Setting | Default | Rule |
| --- | --- | --- |
| `invocations.resume_on_restart` | `false` | Enables qualified resume only after §13 gates. |
| `invocations.resume_max_age` | `24h` | Older interrupted records are not resumed. |
| `durable.engine` | `duroxide` | `temporal` requires feature `durable-temporal`. |
| `durable.max_tenant_runtimes` | `32` | Bounds lazy embedded runtimes; exhaustion fails closed. |
| `durable.history_retention` | `30d` | Terminal history retention; purge is audited. |
| `durable.unknown_outcome_retention` | `90d` | Minimum ledger retention for unknown effects. |
| `durable.max_history_record_bytes` | `65536` | Larger values use blob references. |
| `durable.temporal.endpoint` | unset | Required only for the optional external adapter. |

Environment-variable spelling and config schema are implementation work; this
table is the normative semantic contract.

## 10. Supply chain and images

At the time this design was researched, crates.io lists
[duroxide 0.1.32](https://crates.io/crates/duroxide) as MIT and preview; its
repository describes it as embedded Tokio execution with a built-in SQLite
provider. Its `sqlite` feature currently enables SQLx
`runtime-tokio-native-tls`, so it can reintroduce `native-tls`/OpenSSL despite
the repository's rustls-only posture. The implementation must exact-pin the
reviewed version (the initial proposal is `=0.1.32`), vendor it in the release
process, include its MIT notice and Microsoft copyright in
`THIRD_PARTY_NOTICES`, and pass cargo-deny issue #389.

It must also prove `cargo tree -i native-tls` is empty, or obtain/maintain a
minimal rustls/no-TLS upstream patch under the `third_party/` rules. Pulling
OpenSSL into the distroless runtime is rejected. An arm64 build and the
distroless health check are required evidence, not assumptions. Duroxide's
preview status and its release-policy/maintenance posture are therefore
release risks, not a claim of long-term support.

The optional adapter baseline is `temporalio-sdk = "=1.1.2"` under
`durable-temporal`; 1.1.2 is MIT in the upstream repository and requires Rust
1.92 according to its published manifest. This repository's Dockerfile uses
Rust 1.90.0, so the adapter is blocked until a toolchain compatibility decision
is accepted. Temporal's SDK connects workers to an external Temporal service;
no service binary belongs in this distribution.

## 11. Observability

This design aligns with issue #370 without making telemetry authoritative.
`durable.orchestration`, `durable.activity`, `durable.resume`,
`durable.signal`, `durable.lease` and `durable.reconcile` spans use the
invocation id and hashed tenant/project attributes. A resumed root span links
to, rather than overwrites, the interrupted trace. Metrics include resume
attempt/outcome, replay nondeterminism, history/envelope mismatch, activity
attempt, ledger unknown outcome, lease conflict and purge count. Prompts,
tool arguments, results, claims and secrets are not span attributes by
default. Audit remains the source of truth.

## 12. Public facts and excluded alternatives

- [duroxide README](https://github.com/microsoft/duroxide) and
  [docs.rs](https://docs.rs/duroxide) verify in-process Tokio operation, the
  `Provider`/SQLite model, deterministic orchestration and activities.
- [duroxide Cargo.toml](https://github.com/microsoft/duroxide/blob/main/Cargo.toml)
  verifies the current SQLite feature composition. The exact runtime
  consequences still require the proposed repository-local cargo-tree check.
- Temporal's [GA announcement](https://temporal.io/blog/build-durable-applications-rust-temporal-rust-sdk-now-generally-available),
  [SDK repository](https://github.com/temporalio/sdk-rust), and
  [crate page](https://crates.io/crates/temporalio-sdk) verify Rust Workflows,
  Activities, Workers, replay and external Temporal deployment.
- DBOS is watch-only. Restate and Golem (BSL), Obelisk (AGPL-only), and Hatchet
  (no verified official Rust SDK) are excluded. Dapr Workflow is not the
  default. These exclusions do not license or import their code.

## 13. Release gates and implementation issues

The v0.10.0 acceptance gate is all of:

1. CI `kill -9` matrix: every PDCA step × during LLM call × during tool call.
2. Mock-gateway usage is counted once after recovery.
3. `replay_safe` replays; `reconcile` replays only after proof of no effect;
   `never_replay` and undeclared tools never blindly replay.
4. `isolation_contract_resume` rejects missing/forged snapshots, scope changes
   and no-claims database creation.
5. Engine/envelope mismatch fails closed with audit evidence.
6. Cross-version upgrade drill and mock-versus-duroxide engine-swap drill pass.
7. Restarted cancel, deadline and cumulative budget work as specified.
8. `cargo deny check` passes; notices/vendor review pass; distroless image
   starts and passes `/health`; the native-TLS decision is proven.

Implementation issues (title — acceptance):

1. `feat(durable): DurableExecutor capability contract and mock engine` — contract suite passes.
2. `feat(durable): embedded duroxide tenant runtime and lease` — no claims means no SQLite file.
3. `feat(durable): activity-backed PDCA and LLM replay` — usage is once-only.
4. `feat(durable): side-effect ledger and tool replay metadata` — all three policies pass.
5. `feat(invocations): audited restart resume and consistency assertion` — mismatch fails closed.
6. `feat(durable): cancellation, deadline, budget and approval recovery` — persisted signals win races.
7. `test(durable): upgrade, kill -9 and engine-swap drills` — matrix is CI-gated.
8. `chore(durable): vendor, notices, native-TLS and distroless evidence` — supply-chain gate passes.
9. `feat(durable): optional Temporal adapter` — feature-off build unchanged; external-server conformance passes.

## 14. Open questions

1. What is the approved maximum number and total size of live tenant databases
   for private deployments?
2. Which existing staging terminal outcome is used for `needs_attention`, or
   should it be a separate non-terminal invocation state?
3. What retention and export policy satisfies each deployment's audit rules?
4. Can the selected duroxide version be built rustls/no-TLS without a local
   patch, and does it pass arm64/distroless validation?
5. Should the Temporal adapter wait for a repository-wide Rust toolchain uplift
   or use an explicitly supported compatibility strategy?
