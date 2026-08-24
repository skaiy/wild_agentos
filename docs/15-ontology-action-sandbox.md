# 本体动作执行沙箱（数据沙箱已实现 / 计算沙箱待实现）

> 关联代码：`src/api/http/mod.rs`（`invoke_action_handler` / `commit_via_staging` /
> `sandbox_guardrail_violations` / `redirect_to_staging`）、`src/knowledge_graph/ontology_store.rs`。
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
   `graph:pack/ev-repair/staging/<uuid>`（`staging_graph_iri`）。
2. 把 side-effect 语句中的生产图 IRI 重定向到影子图并写入（`redirect_to_staging`）。
   生产图零改动；`DELETE WHERE` 在空影子图内为 no-op，符合「只暂存新增」的预期。
3. 对影子图跑 ASK/COUNT 护栏（`sandbox_guardrail_violations`）：
   - **三元组数上限**：`SANDBOX_MAX_TRIPLES = 5000`，防单次写回爆量。
   - **谓词命名空间白名单**：`SANDBOX_ALLOWED_PRED_PREFIXES`（ev 本体 / rdfs / rdf），
     存在任一越权谓词即违规。
4. 结果分派：
   - 通过 → `ADD SILENT GRAPH <staging> TO <prod>` 合并 + `DROP SILENT GRAPH <staging>`（提交）。
   - 违规 → `DROP SILENT GRAPH <staging>`（回滚），返回 **422** + `violations` 列表。

### 与 dry_run 的关系
- `dry_run=true`：只返回将执行的 SPARQL，不落任何图（预演）。
- `dry_run=false`：走上面的影子图提交流程（真写，但可回滚 + 有护栏）。

### 边界与后续可扩展
- 当前护栏为轻量 SPARQL ASK/COUNT，不是完整 SHACL。后续可挂 SHACL 形状约束
  （Oxigraph 不内置 SHACL，需自研或用 SPARQL 断言集）。
- 影子图目前每次即建即删；如需人工审批卡点，可保留影子图待审批后再合并。
- 护栏阈值/白名单为常量，后续可提为按域/按动作可配置。

### 测试（`ontology_action_tests`）
- `test_redirect_to_staging_rewrites_graph_iri`：图 IRI 重定向正确。
- `test_sandbox_commit_merges_to_production`：合法写回护栏通过 → 合并到生产图、影子图清理。
- `test_sandbox_rollback_on_foreign_predicate`：越权谓词 → 422 回滚，生产图零改动。

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
