# 30. 耐久执行

> *English version: [30-durable-execution.md](30-durable-execution.md).*

> **状态：[#367](https://github.com/skaiy/wild_agentos/issues/367) 的设计草案。** 本文只定义默认关闭的耐久执行路径，不改变当前重启行为。

## 1. 决策与范围

默认单节点路径采用小型 `DurableExecutor` 边界，默认引擎是嵌入式
[duroxide](https://github.com/microsoft/duroxide)。它在当前进程的 Tokio
runtime 内运行。每个租户一个 SQLite 文件。路径只由已验证的
`IsolationClaims` 生成。没有 claims 就不开库。失败关闭。

`durable-temporal` 是可选、默认关闭的 Temporal 适配器。它面向需要多节点
可用性的客户。客户自行运行外部 Temporal Server。镜像不分发该服务。嵌入式
路径不做选主，也不承诺多节点调度。

目标：

- invocation 经正常重启或 `kill -9` 后仍可到达终态；
- 恢复不重复计量 LLM 用量，也不盲目重复外部副作用；
- 重启后 claims、取消、deadline、预算、审计和审批仍有效。

非目标：

- 对任意外部系统提供 exactly-once；
- 把引擎历史当审计事实或鉴权来源；
- 打包 Temporal、支持被排除的引擎，或在公开 Invocations HTTP API 暴露引擎概念。

全部 §13 验收门通过前，`invocations.resume_on_restart` 保持 `false`。
`EXECUTION` 也保持默认关闭。

## 2. 当前 main 基线

本表来自当前分支代码核验，不沿用旧策略文档。

| 位置 | 已核实的当前行为 | 对耐久执行的含义 |
| --- | --- | --- |
| `src/core/checkpoint.rs` | `PdcaCheckpointEnvelope` 不可变；在 PDCA 边界用已验证 claims 写入；`read_pdca_resume` 拒绝缺失或不匹配 claims；`PdcaResumeState` 重建已完成步骤。 | 已有步骤级业务 checkpoint。 |
| `src/api/http/invocations_store.rs` | 启动时，`InvocationStore::open_with_config` 将全部非终态 `queued`、`running`、`cancel_requested` 改为 `failed/interrupted`，递增 revision 并写审计；不会续跑。 | 仅在耐久门通过时，才能替换这条特权终止路径。 |
| `src/api/http/invocations_execution.rs` | 执行桥从持久化 invocation 字段重建已验证 claims；使用进程内 FIFO 准入；启动任务；执行 deadline、预算；取消注册表只在内存。 | 没有持久实例历史、回放或重启安全的取消信号。 |
| `src/api/http/invocations_enforcement.rs` | 全局、租户、scope 的运行上限和 deadline watcher 都在进程内。 | 恢复必须重新申请槽位；重启后不能信任旧计数。 |
| `src/memory/l0_store.rs` | `TenantL0Registry` 每进程、每租户共享一个可写 redb handle；键由 claims 生成。redb 文件锁阻止第二次打开。 | 副作用账本必须复用此 claims-scoped handle。 |
| `src/tools/tool_executor/mod.rs` | `register` 接收名称、描述、JSON schema、handler、角色；`execute_guarded` 执行 advertised-tool、安全、角色策略门。 | 工具注册表目前没有逐工具重放策略元数据。 |
| `src/worker/task_queue.rs` | 使用以调用方传入文件路径为根的 `yaque` sender/receiver。 | 旧文档称它“只在内存”已不准确；但它仍不是 Invocation 恢复权威或 PDCA 回放日志。 |

当前没有 `resume_on_restart` 或 `resume_max_age` 配置。`src/` 里也没有
OpenTelemetry SDK/`traceparent` 集成。`Cargo.toml` 的 `reqwest` 是
rustls-only；distroless 镜像刻意不带 OpenSSL runtime。

## 3. 架构与职责

HTTP store 仍是公开生命周期的权威。耐久实例 id 等于 invocation id。WAO
负责鉴权、副作用账本、审计事件、策略集钉住、审批路由和 L0 checkpoint
envelope。引擎负责 workflow history、timer、activity 投递和实例锁。
两个引擎都不得直接写 `InvocationStore`。也绝不信任引擎 history 内的 claims。

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

`DurableStart` 带 invocation id、不可变 claims snapshot digest、定义/策略
digest、deadline 与非敏感输入引用。它不带凭证。`DurableSignal` 只有 `Cancel`
或 `Approval { staging_id, outcome }`。启动时做能力探测。缺少确定性回放、
timer、signal、lease 的引擎一律拒绝。

每个租户延迟创建一个有上限的 runtime 和
`<claims-derived-tenant-path>/durable.db`。路径不能来自 HTTP 输入、引擎
history 或配置插值。进程 epoch 加 SQLite/文件独占锁防止两个本地进程取得
同一租户 runtime。引擎实例 lease 防止并发 turn。拿锁失败即恢复失败关闭，
不能靠重试跑出两个 owner。Temporal 报告 `multi_node = true`；嵌入式
duroxide 不报告。

## 4. 引擎映射

| 关注点 | 默认 duroxide | Temporal 适配器 |
| --- | --- | --- |
| Invocation 编排 | 已注册 orchestration，`invocation_id` 是实例 id | 一个 Workflow Execution，`invocation_id` 是 Workflow ID |
| PDCA Plan/Do/Check/Act | 确定性 orchestration 转换；每步完成后跑 checkpoint activity | 确定性 Workflow 转换；每步完成后跑 checkpoint Activity |
| LLM 调用 | `llm_call` activity；回放从 history 取结果 | `llm_call` Activity；回放从 Event History 取结果 |
| 工具调用 | `tool_call` activity，先写 ledger intent | `tool_call` Activity，先写 ledger intent |
| 人审 | 外部事件与 durable timer 竞争 | Signal 与 timer 竞争 |
| Deadline | durable timer 与工作竞争 | Workflow timer 与工作竞争 |
| 取消 | durable signal 加 cooperative activity cancellation | Workflow signal/cancellation 加 Activity cancellation |
| 子工作 | 需要时使用 sub-orchestration | 使用 Child Workflow |
| 版本化 | live instance 保留其注册 orchestration version | Worker deployment/versioning 与兼容 Workflow code |

这里是行为映射，不是 API 包装。表中的名称是拟定的 WAO activity 名，不保证
两家 API 的类型签名相同。

## 5. 确定性、数据保留与 checkpoint

编排器只能是纯状态机：PDCA 推进、策略级决策、累计预算比较、signal 处理和下
一个 activity 选择。它不得调用 I/O、`Utc::now()`、随机数、直接 Tokio
并发原语或密钥。它只使用引擎时间、引擎生成 id 和确定性 select/join。LLM、
工具、blob 读写、审批投递及全部其他 I/O 都是 activity。

防回归手段：workflow 模块的 lint/禁用 import、代码审查清单、同一 history
执行两次的 replay 测试。发现非确定性即失败关闭并写审计。

引擎 history 是执行证据。`PdcaCheckpointEnvelope` 与 L0 ledger 仍是业务和
审计证据。恢复时，引擎 history 中完成的 checkpoint activity 集合必须与
envelope 集合完全相同。任何缺失、额外、损坏、跨 claims、乱序记录都进入
`needs_attention`，不能猜成成功。

history 只保存有界元数据和 blob 引用：单条 activity input/output 序列化后最多
64 KiB；更大的 LLM/工具材料进 claims-scoped blob。终态后 history 默认保留
30 天；未知结果 ledger 保留 90 天；清理按 claims、写审计，不得清理非终态实例。
具体运维默认值见下表，发版前必须有测试。

## 6. Activity 投递与副作用账本

activity 是至少一次。worker 不确定失败后 activity 可能再次开始。仅有
idempotency key 不足以处理未知外部结果。

工具注册契约增加必填 `side_effect` 元数据：

| 策略 | 恢复规则 |
| --- | --- |
| `replay_safe` | 允许重放，通常是只读工具。 |
| `reconcile` | 先调用工具专属、权威的 `reconcile`；只有证明副作用未发生才可重放。 |
| `never_replay` | 只有 ledger 能证明从未开始执行才可重放；否则进入现有 staging/人工审查。 |

未声明按 `never_replay`。沙箱执行按 `never_replay`。账本是 L0 内的只追加记录，
键为 `(invocation_id, PDCA step, attempt)`。调用前写不可变 Intent。小型可变
lease 记录 owner epoch、activity attempt 和观察到的结果。未知完成标为
`outcome_unknown`、`needs_attention`，并延长保留。参数被脱敏或省略也必须进入
`needs_attention`。实施时与仓库 secret guard 模块一致。

LLM 请求 digest 包含规范化的非敏感字段：invocation id、activity id、所选模型、
prompt/input blob digest、工具 schema digest、temperature、策略/定义 digest。
它不包含 header、bearer token、API key、原始 secret。已完成的 `llm_call` 在
回放时直接返回记录结果，用量只提交一次。成功 invocation 仍须满足既有完整
usage/cost 契约。没有 gateway 或运营方 price table 成本时失败关闭，不能填 `0`。

## 7. 恢复状态机

```text
queued ──start──> running ──terminal──> succeeded | failed | cancelled
                       │
                       └─ restart ─> interrupted
interrupted ──eligible, audited resume──> queued ──start──> running
interrupted ──gate failure──────────────> needs_attention
```

`interrupted → queued` 是新增、带 `resume` 原因的审计迁移。只有所有条件都满足
才允许：开关已开；实例存在；claims snapshot、定义 digest、策略 digest 有效；
engine 与 envelope history 一致；记录未超 `resume_max_age`；已取得运行槽和租户
lease。不得用新 tenant、project 或 actor 替代任何条件。

重启时先投递持久化 cancel intent，再调度工作。deadline 已过时直接到终态，不调用
activity。累计 usage 和预算从已记录 activity 结果重建，绝不归零。拿不到运行槽
时保持 durable queued，不得启动第二份执行。审批等待与 TTL timer 竞争；过期审批
按钉住策略进入终态或 `needs_attention`。

`resume_max_age` 默认 `24h`。过期 invocation 不恢复，改为
`failed/interrupted` 并审计。snapshot 缺失/伪造、claims 缺失、claims 派生数据库
错误、跨 scope 恢复都失败关闭；它们进入 `isolation_contract_resume` 负向套件。

## 8. 二进制升级与引擎替换

每个实例记录 `workflow_name`、`workflow_compatibility_version`、引擎身份/版本，
以及 WAO build/definition/policy digest。兼容升级保留旧 workflow 实现，直到全部
旧实例进入终态。不兼容指 history command 序列、payload 解释或重放策略语义改变。
这种变更进入人工审查，不能静默失败，也不能让新代码读取旧 history。

CI 升级演练从 build N 启动，在每个 PDCA 边界中断，再在 N+1 恢复。它必须使用兼容
版本完成，或带审计进入 `needs_attention`。另一条引擎替换演练用相同 contract suite
运行 `MockDurableExecutor` 和 duroxide；适配器能编译不等于验收。

## 9. 配置

| 设置 | 默认值 | 规则 |
| --- | --- | --- |
| `invocations.resume_on_restart` | `false` | 仅在 §13 门通过后启用合格恢复。 |
| `invocations.resume_max_age` | `24h` | 更早的 interrupted 记录不恢复。 |
| `durable.engine` | `duroxide` | `temporal` 需要 `durable-temporal` feature。 |
| `durable.max_tenant_runtimes` | `32` | 限制延迟创建的嵌入式 runtime；耗尽即失败关闭。 |
| `durable.history_retention` | `30d` | 终态 history 保留期；清理有审计。 |
| `durable.unknown_outcome_retention` | `90d` | 未知副作用的 ledger 最短保留期。 |
| `durable.max_history_record_bytes` | `65536` | 超出部分使用 blob 引用。 |
| `durable.temporal.endpoint` | 未设置 | 仅可选外部适配器需要。 |

环境变量写法和配置 schema 属于实施工作；此表规定语义。

## 10. 供应链与镜像

本文调研时，[crates.io 的 duroxide 0.1.32](https://crates.io/crates/duroxide)
标为 MIT、preview；仓库说明它是嵌入式 Tokio 执行，内置 SQLite provider。它的
`sqlite` feature 当前启用 SQLx `runtime-tokio-native-tls`，可能把
`native-tls`/OpenSSL 带回依赖树，和仓库 rustls-only 立场冲突。实施必须精确 pin
已审版本（初始提案为 `=0.1.32`），在发版流程 vendor，向
`THIRD_PARTY_NOTICES` 写入 MIT notice 与 Microsoft copyright，并通过
cargo-deny #389。

还必须证明 `cargo tree -i native-tls` 为空；否则应取得并维护最小 rustls/no-TLS
上游补丁，按 `third_party/` 规则处理。拒绝把 OpenSSL 拉进 distroless runtime。
arm64 构建和 distroless health check 必须是证据，不得假设通过。duroxide 的 preview
状态及其 release-policy/维护方式是发版风险，不是长期支持承诺。

可选适配器的基线是 feature `durable-temporal` 下
`temporalio-sdk = "=1.1.2"`。上游仓库标为 MIT，发布 manifest 标明 Rust 1.92。
本仓库 Dockerfile 使用 Rust 1.90.0，所以工具链兼容决策前，适配器被阻塞。
Temporal SDK 的 worker 连接外部 Temporal 服务；本发行物不含服务端二进制。

## 11. 可观测性

本文与 #370 对齐，但不让遥测成为权威。`durable.orchestration`、
`durable.activity`、`durable.resume`、`durable.signal`、`durable.lease`、
`durable.reconcile` span 带 invocation id 和哈希后的 tenant/project 属性。恢复后的
root span 以 link 关联中断前 trace，不覆盖它。指标包括恢复尝试/结果、回放非确定性、
history/envelope 不匹配、activity attempt、ledger 未知结果、lease 冲突、清理数量。
默认不导出 prompt、工具参数、结果、claims、secret。审计仍是事实来源。

## 12. 公开事实与排除项

- [duroxide README](https://github.com/microsoft/duroxide) 和
  [docs.rs](https://docs.rs/duroxide) 核实了进程内 Tokio、`Provider`/SQLite、
  确定性 orchestration 和 activity。
- [duroxide Cargo.toml](https://github.com/microsoft/duroxide/blob/main/Cargo.toml)
  核实当前 SQLite feature 组成。对本仓库的精确 runtime 影响仍须以
  cargo-tree 检查为准。
- Temporal 的 [GA 公告](https://temporal.io/blog/build-durable-applications-rust-temporal-rust-sdk-now-generally-available)、
  [SDK 仓库](https://github.com/temporalio/sdk-rust) 和
  [crate 页面](https://crates.io/crates/temporalio-sdk) 核实了 Rust Workflow、
  Activity、Worker、回放和外部 Temporal 部署。
- DBOS 只观察。Restate、Golem（BSL），Obelisk（AGPL-only）和 Hatchet（未核实到
  官方 Rust SDK）排除。Dapr Workflow 不作默认。这些排除不导入也不许可其代码。

## 13. 发版门与实现 issue

v0.10.0 验收门必须全部通过：

1. CI `kill -9` 矩阵：每个 PDCA 步骤 × LLM 调用中 × 工具调用中。
2. mock gateway 证明恢复后 usage 只计一次。
3. `replay_safe` 可重放；`reconcile` 仅在证明无副作用后重放；
   `never_replay` 和未声明工具绝不盲目重放。
4. `isolation_contract_resume` 拒绝缺失/伪造 snapshot、scope 变更和无 claims 建库。
5. engine/envelope 不匹配失败关闭并保留审计。
6. 跨版本升级演练、mock 对 duroxide 的引擎替换演练通过。
7. 重启后的 cancel、deadline、累计 budget 符合本文。
8. `cargo deny check` 通过；notice/vendor 审查通过；distroless 镜像可启动并通过
   `/health`；native-TLS 结论已证明。

实现 issue（标题 — 验收）：

1. `feat(durable): DurableExecutor capability contract and mock engine` — contract suite 通过。
2. `feat(durable): embedded duroxide tenant runtime and lease` — 无 claims 不创建 SQLite 文件。
3. `feat(durable): activity-backed PDCA and LLM replay` — usage 只计一次。
4. `feat(durable): side-effect ledger and tool replay metadata` — 三种策略全部通过。
5. `feat(invocations): audited restart resume and consistency assertion` — 不一致失败关闭。
6. `feat(durable): cancellation, deadline, budget and approval recovery` — 持久信号赢得竞争。
7. `test(durable): upgrade, kill -9 and engine-swap drills` — 矩阵是 CI gate。
8. `chore(durable): vendor, notices, native-TLS and distroless evidence` — 供应链 gate 通过。
9. `feat(durable): optional Temporal adapter` — feature-off build 不变；外部服务一致性通过。

## 14. 开放问题

1. 私有化部署允许的 live tenant 数据库数量和总大小上限是什么？
2. `needs_attention` 应复用现有 staging 终态，还是成为独立的非终态 invocation 状态？
3. 各部署的审计规则要求怎样的保留和导出策略？
4. 选定 duroxide 版本能否在无本地补丁下以 rustls/no-TLS 构建，并通过
   arm64/distroless 验证？
5. Temporal 适配器应等待仓库统一升级 Rust 工具链，还是采用已批准的兼容方案？
