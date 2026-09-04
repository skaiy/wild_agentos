# 本体动作执行沙箱（数据沙箱已实现 / 计算沙箱待实现）

> 关联代码：`src/api/http/ontology.rs`（`invoke_action_handler` / `commit_via_staging`）、
> `src/api/http/ontology_guardrails.rs`、`src/knowledge_graph/ontology_store.rs`。
> 关联本体文档：`07-knowledge-graph.md`。

本体「动力层」的 ActionType 让知识图谱从只读升级为可写可执行。写回一旦落生产命名图即不可
撤销，因此需要沙箱。本设计把「危险」拆成两类，用两种独立机制处理：

| 类别 | 风险本质 | 机制 | 状态 |
|---|---|---|---|
| 声明式动作的 SPARQL 写回 | 写坏图谱数据 | **数据沙箱**（命名图级隔离） | ✅ 已实现 |
| 任意用户代码 / 表达式执行 | 越权读写、资源耗尽 | **计算沙箱**（进程/命名空间隔离） | ⏸ 待实现（本文档记录设计） |

---

## 一、数据沙箱（已实现）：staging-graph 影子图执行

### 目标
把「直接写生产图」升级为「先写隔离影子图 → 护栏后校验 → 通过才合并、失败即回滚」，
等价一次可回滚事务。**仅隔离数据（命名图级），不隔离计算/进程。**

### 执行流程（`commit_via_staging`）
1. 为本次 invoke 生成 per-invocation 影子图 IRI：
   `graph://{tenant}/{project}/staging/<uuid>`（`staging_graph_iri_for_claims`）。
2. 通过 claims 派生的 staging 图直接写入 side-effect（`update_staging_for_claims`）。
   生产图零改动；`DELETE WHERE` 在空影子图内为 no-op，符合「只暂存新增」的预期。
3. 对影子图跑 ASK/COUNT 护栏（`ontology_guardrails`）：
   - **三元组数上限**：域默认或 ActionType 覆盖；两者都未配置时仍为 `5000`。
   - **谓词命名空间白名单**：域默认或 ActionType 覆盖；两者都未配置时仍为
     ev 本体 / rdfs / rdf，存在任一越权谓词即违规。
   - **SPARQL ASK 断言集**：内置工单/FAQ 必须带 `rdfs:label` 示例，并可在域或
     ActionType 配置中追加带可读 `code` 的 ASK 断言；ASK 返回 `true` 表示违规。
4. 结果分派（由 invoke 的 `commit_strategy` 和有效护栏策略决定）：
   - 任一硬护栏违规 → `DROP SILENT GRAPH <staging>`（回滚），返回 **422** + `violations` 列表。
   - `auto`（默认）且策略未扩大默认写入面 → 合并后删除影子图（提交）。
   - `require_approval`，或动作级策略提高三元组上限/扩大谓词前缀白名单 → 保留影子图，
     创建待审批对象并返回 `approval_id` 和 `staging_graph`。

### 与 dry_run 的关系
- `dry_run=true`：只返回将执行的 SPARQL，不落任何图（预演）。
- `dry_run=false`：走上面的影子图提交或待审批流程（真写，但可回滚 + 有护栏）。

### 人工审批 API（JWT claims 必需）

所有 API 从已验证 JWT 铸造租户/项目作用域；请求不能选择图、租户或项目。审批记录放在专用
命名图 `graph://{tenant}/{project}/action-approvals`，影子写入仍位于
`graph://{tenant}/{project}/staging/{approval_id}`，因此待审批内容不会出现在生产数据图。

| Route | 行为 |
|---|---|
| `GET /api/v1/ontology/action-approvals` | 列出本 tenant/project 的待审批记录。 |
| `POST /api/v1/ontology/action-approvals/:approval_id/approve` | 合并对应 staging 图，再清理 staging 和审批记录。 |
| `POST /api/v1/ontology/action-approvals/:approval_id/reject`（或 `/discard`） | 丢弃对应 staging 图和审批记录。 |

审批对象 schema 为 `approval_id`、`staging_id`、`staging_graph`、`action_id`、`created_at` 和
`expires_at`。默认 TTL 为 24 小时；读取或决议时发现过期记录会惰性清理 staging 图和元数据，
并且 approve/reject 返回 **410 Gone**。跨 tenant/project 查询不到该审批，不能批准或丢弃它；
没有 verified JWT claims 的接口调用返回 **401**。

### 护栏配置与 claims 作用域
- 域默认配置使用 `PUT /api/v1/ontology/guardrails`，读取使用
  `GET /api/v1/ontology/guardrails`；两者都要求已验证的 JWT isolation claims。
- ActionType 的 `guardrails` 字段通过既有 claims-authenticated ActionType CRUD 保存。
  `max_triples` 和 `allowed_predicate_prefixes` 逐项覆盖域默认值；`assertions` 则按
  内置 → 域 → ActionType 顺序追加。省略字段永远回退至安全默认值，不会关闭护栏。
- invoke 请求不接受 `guardrails` 字段，图作用域与有效策略始终由服务端从 verified
  claims 和已存配置选择。

### 边界与后续可扩展
- 当前护栏为轻量 SPARQL ASK/COUNT，**不是完整 SHACL**。Oxigraph 不内置 SHACL；
  这里的断言集只是可审计、可配置的“准 SHACL”替代，尚不支持 SHACL 的 shapes、
  推理、报告模型或完整约束语义。
- 默认策略下影子图每次即建即删；`require_approval` 或放宽默认策略时会保留至批准、
  拒绝或 TTL 清理。
- 断言查询不得指定 `GRAPH`，运行时会自动绑定至 claims 派生的影子图，避免跨租户
  读取。

### 测试（`ontology_action_tests`）
- `test_sandbox_commit_merges_to_production`：合法写回护栏通过 → 合并到生产图、影子图清理。
- `test_sandbox_rollback_on_foreign_predicate`：越权谓词 → 422 回滚，生产图零改动。
- `action_approval_keeps_staging_until_same_scope_approves`：待审批不改变生产图、同 scope 批准后
  才合并、跨租户不可批准、无 JWT 被拒绝、拒绝会丢弃影子图。
- `test_action_whitelist_override_rejects_otherwise_allowed_predicate`：ActionType 白名单
  覆盖会拒绝默认白名单本可接受的谓词。
- `test_assertion_failure_rolls_back_staging_graph`：ASK 断言失败 → 422 回滚，生产图零改动。

---

## 二、计算沙箱（待实现 — 记录，不落地）

### 何时才需要
仅当自定义动作/函数要执行**真正的代码或任意表达式**（而非占位符白名单替换的 SPARQL 模板）
时才需要进程级隔离。当前阶段自定义动作 `executable=false`（invoke 返回 422），不触发此需求。

### 已有可复用基础（无需从零造）
- `src/tools/tool_executor/builtins.rs` (`execute_bash`) plus `src/tools/builtin/sandbox.rs`：已具备 `FilesystemIsolationMode`、`isolateNetwork`、
  `namespaceRestrictions`、`dangerouslyDisableSandbox` 等 namespace/网络隔离能力。
- `src/core/syscall_gate.rs`：`WhitelistManager` / `SyscallGate` 提供能力白名单 + 签名校验骨架。

### 拟采用形态（待需求触发再定稿）
1. **表达式层**：优先把自定义函数限制为「受限 DSL / SPARQL 模板 + 占位符白名单替换」，
   能不引入代码执行就不引入。
2. **确需执行代码**：复用 bash.rs 的 namespace 隔离（只读文件系统 + 禁网 + 资源限额），
   经 `SyscallGate` 能力白名单收敛可用系统调用，输入输出走结构化通道。
3. **审计**：执行事件入事件总线留痕（与现有事件流一致）。

### 明确不做（本次范围外）
- 不引入容器/gVisor/WASM 运行时等新依赖。
- 不放开 `BUILTIN_EXECUTABLE_ACTIONS` 白名单去执行任意自定义动作代码。

---

## 决策小结
- **现在**：声明式动作 → 数据沙箱（影子图 + 护栏 + 命名图限定），纯 SPARQL 能力、零新依赖、可回滚。
- **将来（按需）**：任意代码执行 → 计算沙箱（复用 bash.rs namespace 隔离 + syscall_gate 白名单）。
