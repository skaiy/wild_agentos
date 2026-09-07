# 9. 技能图谱系统

> *本文是 [09-skill-graph.md](09-skill-graph.md) 的中文翻译。*

> 基于 JSON-LD 的技能知识图谱，支持 5W2H 描述、技能发现、进化、冲突检测和自举学习

## 模块架构

**源文件目录**: `src/skill_graph/`（15 个模块）

```mermaid
graph TB
    subgraph 输入源
        MD["Markdown Skill"]
        LLM_IN["LLM 自然语言"]
        MCP_IN["MCP 工具"]
        BOOT_IN["任务执行/错误恢复/<br/>用户反馈/代码审查"]
    end

    subgraph 创建层
        SC["SkillCreator<br/>LLM 技能创建<br/>MD→JSON-LD 转换"]
        MCP_INT["MCPIntegration<br/>MCP 工具同步"]
        BSE["BootstrapEngine<br/>自举学习"]
    end

    subgraph 存储层
        SGS["SkillGraphStore<br/>技能图谱存储<br/>L0+L2 双层"]
        IDX["PreAggregatedIndex<br/>预聚合索引"]
    end

    subgraph 运行时引擎
        SDE["SkillDiscoveryEngine<br/>5W2H 匹配 + 向量检索"]
        SEE["SkillEvolutionEngine<br/>使用追踪 + 进化建议"]
        SEC["SecurityEngine<br/>信任等级 + 签名校验"]
        CFE["ConflictDetectionEngine<br/>6种冲突检测"]
        QE["QueryEngine<br/>查询模板"]
    end

    MD --> SC
    LLM_IN --> SC
    MCP_IN --> MCP_INT
    BOOT_IN --> BSE
    SC --> SGS
    MCP_INT --> SGS
    BSE --> SGS
    SGS --> IDX
    IDX --> SDE
    SGS --> SEE
    SGS --> SEC
    SGS --> CFE
    SGS --> QE
```

## 模块文件清单

| 文件 | 组件 | 说明 |
|------|------|------|
| `types.rs` | SkillGraphNode, SkillNodeType, SkillLink, Skill5W2H | 核心类型定义 |
| `graph_store.rs` | SkillGraphStore | 技能图谱存储（L0 + L2） |
| `index.rs` | PreAggregatedIndex | 预聚合索引 |
| `discovery.rs` | SkillDiscoveryEngine | 5W2H 技能发现引擎 |
| `evolution.rs` | SkillEvolutionEngine | 技能进化引擎 |
| `conflict.rs` | ConflictDetectionEngine | 冲突检测引擎 |
| `security.rs` | SecurityEngine | 安全引擎 |
| `skill_creator.rs` | SkillCreator | LLM 技能创建 |
| `bootstrap.rs` | BootstrapEngine | 自举学习 |
| `mcp_integration.rs` | MCPIntegration | MCP 工具同步 |
| `query_templates.rs` | QueryEngine | 查询模板 |
| `embedding.rs` | SkillEmbeddingEngine | 庞加莱结构嵌入计算 |
| `graph_algorithms.rs` | GraphAlgorithmEngine | 图算法（PageRank/介数中心性/社区发现） |
| `verification.rs` | InvariantVerifier | 形式化不变式验证（6 种检查） |

## 核心类型

### SkillGraphNode — 技能节点

```mermaid
classDiagram
    class SkillGraphNode {
        +String skill_iri
        +String name
        +String description
        +String version
        +SkillNodeType node_type
        +Skill5W2H w2h
        +Vec~SkillLink~ links
        +SkillGraphMeta graph_meta
        +Option~SkillContent~ content
        +Option~SkillSecurityInfo~ security_info
        +StorageTier storage_tier
        +to_json_ld() Value
    }

    class SkillNodeType {
        <<enumeration>>
        Atomic
        Composite
        MOC
        KnowledgeFragment
        MCPTool
        Bootstrap
    }

    class Skill5W2H {
        +String what
        +String why
        +String who
        +String when
        +String where
        +String how
        +Option~String~ how_much
    }

    class SkillLink {
        +String target_iri
        +SkillLinkType link_type
        +Option~String~ description
        +f64 confidence
    }

    class SkillLinkType {
        <<enumeration>>
        Prerequisite
        Composition
        Related
        Alternative
        Extends
        Generalization
    }

    SkillGraphNode --> SkillNodeType
    SkillGraphNode --> Skill5W2H
    SkillGraphNode --> SkillLink
    SkillLink --> SkillLinkType
```

### 存储层级

| 层级 | 类型 | 说明 |
|------|------|------|
| `L0Permanent` | redb | 永久存储，核心技能 |
| `L1Session` | 内存 | 会话级临时技能 |
| `L2Blackboard` | Oxigraph | 共享黑板，跨 Agent 可见 |
| `L3Projection` | SPARQL | 按需投影 |

## 引擎详解

### SkillDiscoveryEngine — 技能发现

基于 5W2H 维度匹配和向量检索的技能发现：

```mermaid
flowchart LR
    TASK["任务描述"] --> W2H["5W2H 解析"]
    W2H --> MATCH["5W2H 维度匹配<br/>what/why/who/when/where"]
    W2H --> VEC["向量相似度检索"]
    MATCH --> MERGE["结果融合"]
    VEC --> MERGE
    MERGE --> RANK["置信度排序"]
    RANK --> TOP["返回 Top-K 技能"]
```

**文件**: `src/skill_graph/discovery.rs`

### SkillEvolutionEngine — 技能进化

追踪技能使用情况，生成进化建议：

**文件**: `src/skill_graph/evolution.rs`

| 进化建议类型 | 说明 |
|-------------|------|
| `AddLink` | 添加新的技能关联 |
| `UpdateSuccessRate` | 更新成功率 |
| `CreateFragment` | 创建知识碎片 |
| `Deprecate` | 标记废弃 |
| `Merge` | 合并相似技能 |
| `Split` | 拆分过大的技能 |

### ConflictDetectionEngine — 冲突检测

**文件**: `src/skill_graph/conflict.rs`

6 种冲突类型：

| 冲突类型 | 说明 |
|---------|------|
| `Resource` | 资源竞争冲突 |
| `Dependency` | 依赖版本冲突 |
| `Permission` | 权限冲突 |
| `Semantic` | 语义定义冲突 |
| `Temporal` | 时序冲突 |
| `Version` | 版本冲突 |

### SecurityEngine — 安全引擎

**文件**: `src/skill_graph/security.rs`

```mermaid
flowchart TD
    CALL["技能调用请求"] --> TRUST["检查信任等级"]
    TRUST --> PERM["检查权限列表"]
    PERM --> SIG["校验数字签名<br/>(Ed25519)"]
    SIG --> RISK["评估风险分数"]
    RISK --> DECISION{"安全决策"}
    DECISION -->|"Allow"| EXEC["执行技能"]
    DECISION -->|"Deny"| REJECT["拒绝执行"]
    DECISION -->|"AskUser"| PROMPT["提示用户确认"]
```

### SkillCreator — LLM 技能创建

**文件**: `src/skill_graph/skill_creator.rs`

支持两种创建模式：

1. **自然语言创建**：用户描述需求 → LLM 生成 JSON-LD Skill 定义
2. **Markdown 转换**：读取 skill.md → LLM 转换为 JSON-LD 格式

### BootstrapEngine — 自举学习

**文件**: `src/skill_graph/bootstrap.rs`

从运行时经验中自动学习新技能：

| 学习来源 | 说明 |
|---------|------|
| 任务执行 | 成功执行的任务模式 |
| 错误恢复 | 修复错误的策略 |
| 用户反馈 | 用户显式指导 |
| 代码审查 | 代码改进建议 |
| 知识抽取 | 从文档中提取 |

**操作类型**：
- `Learn` — 创建新技能或增强现有技能
- `Reduce` — 简化过于复杂的技能

### MCPIntegration — MCP 工具同步

**文件**: `src/skill_graph/mcp_integration.rs`

将 MCP 工具自动同步为技能图谱中的技能节点。

## MOC 导航

MOC（Map of Content）节点作为技能图谱的导航入口：

```mermaid
graph TD
    MOC["MOC: 编程技能"] --> S1["Rust 开发"]
    MOC --> S2["Python 开发"]
    MOC --> S3["Web 开发"]
    S1 --> S1_1["Rust 异步编程"]
    S1 --> S1_2["Rust 宏编程"]
    S2 --> S2_1["Python 数据分析"]
    S3 --> S3_1["React 开发"]
    S3 --> S3_2["Node.js 开发"]
    S1_1 -.->|"Related"| S3_2
```

## 新增高级特性

### 庞加莱结构嵌入 (SkillEmbeddingEngine)

**文件**: `src/skill_graph/embedding.rs`

从图谱拓扑结构自动计算技能节点的几何嵌入向量。嵌入依据：
- 先决条件深度（Prerequisite depth）
- 标签指纹（Tag fingerprinting）
- 链接拓扑模式

嵌入向量用于庞加莱球空间中的语义相似度搜索和结构聚类。

### 图算法引擎 (GraphAlgorithmEngine)

**文件**: `src/skill_graph/graph_algorithms.rs`

内置图算法引擎提供丰富的图谱分析能力：

| 算法 | 用途 |
|------|------|
| PageRank | 技能重要性排序，识别核心技能 |
| 介数中心性 (Betweenness) | 关键路径瓶颈检测 |
| 标签传播社区发现 | 自动技能聚类 |
| DFS 先决条件链 | 发现完整技能依赖链 |
| Tarjan SCC | 循环依赖检测 |

### 形式化不变式验证 (InvariantVerifier)

**文件**: `src/skill_graph/verification.rs`

对技能图谱执行 6 种不变式检查，确保图结构完整性：

| 检查 | 说明 |
|------|------|
| 无环性 | 无循环依赖 |
| 链接存在性 | 无悬空引用 |
| 组合可达性 | 所有子技能可访问 |
| 无废弃先决条件 | 无已废弃技能被引用 |
| 有效 5W2H | 所有技能 5W2H 元数据完整 |
| 有效安全等级 | 无越权链接 |

违反检查的操作在提交前被拒绝，并返回具体错误原因。

### Skill 包、门禁与发布

可分发的 Skill 是一个目录，包含 `skill.yaml`、`SKILL.md` 入口文件，以及
`tests/golden-input.json` 和 `tests/golden-output.json`。`skill.yaml` 的 `package`
段使用 `schema: agentos.dev/skill-package/v1` 进行版本化，并且必须声明：

```yaml
package:
  schema: agentos.dev/skill-package/v1
  side_effect_level: none # none | read | write | execute
  visibility: tenant      # system | tenant | session
  entrypoint: SKILL.md
  golden_input: tests/golden-input.json
  golden_output: tests/golden-output.json
  judge_rules: judge.rules # 可选的确定性 JSON-pointer 规则
```

Git 导入是租户级发布通道。它会在持久化或注册 Skill 前验证包清单、既有的
Schema/签名/安全检查、golden 输入输出契约和可选的确定性 Judge 规则。声明
`system` 的包会被拒绝；系统级 Skill 只能由内核维护。会话级创作保持隔离，不能
静默升级为租户级。LLM Judge 默认关闭，内核门禁也绝不会执行导入包中的脚本。

CI 中的 `skill-package-gate` job 同时执行一个通过和一个故意失败的 fixture。
本地运行：

```bash
cargo test --lib skill_package_gate_ --verbose
```

### 超图组合 (Hypergraph)

技能图谱支持第一类超图组合，通过 `Hyperedge` 类型和 `CompositionType` 枚举定义复杂工作流：

| 组合类型 | 语义 |
|---------|------|
| Sequential | 顺序执行 |
| Parallel | 并行执行 |
| Conditional | 条件分支 |
| Optional | 可选步骤 |
| Fallback | 回退策略 |

### 时间版本控制 (Temporal Versioning)

技能图谱支持快照和回滚机制：
- 每次图操作前自动创建快照
- 验证失败时自动回滚
- 手动创建基线快照用于发布版本

### Oxigraph SPARQL 桥接

实现 `SkillGraphStore` 与统一 Oxigraph RDF 存储之间的实时双向同步：
- 图写入时通过 SPARQL INSERT 同步到 `system:skills` 命名图
- 外部 SPARQL 更新时自动反同步回内存图
- 命名图隔离防止跨子系统数据污染

### 语义技能发现增强

`SkillDiscoveryEngine` 已集成 HyperspaceEngine 向量存储：
- `suggest_links()` 从 Jaccard 标签重叠降级为 HyperspaceStore 余弦相似度搜索
- `find_skill_chain()` BFS 路径搜索
- `get_skill_tree()` 组合树构建
- 混合文本×结构搜索（向量×图拓扑）

## 与知识图谱的关系

技能图谱和知识图谱是互补的双层架构：

```mermaid
graph LR
    subgraph 技能图谱
        SKILL["技能节点<br/>How/Process"]
    end

    subgraph 知识图谱
        ENTITY["知识实体<br/>What/Concept"]
    end

    SKILL -->|"HasSkill"| ENTITY
    ENTITY -->|"ApplicableIn"| SKILL
    SKILL -->|"RelatedTo"| ENTITY
```

| 维度 | 技能图谱 | 知识图谱 |
|------|---------|---------|
| 存储 | L0 redb + L2 Oxigraph | Oxigraph Memory（`Arc<Mutex>`） |
| 命名图 | `graph:skill` | `graph:world` / `graph:code` |
| 描述 | 5W2H 结构化 | RDF Quads |
| 发现 | 5W2H 匹配 + HyperspaceEngine 向量检索 + 图算法 | SPARQL + 模糊搜索 |
| 进化 | 使用追踪 + 进化建议 + 形式化验证 | 增量更新（SHA256） |
| 超图 | Hyperedge + CompositionType（顺序/并行/条件/可选/回退） | — |
| 不变式验证 | 6 种验证（无环/可达性/无废弃/安全/5W2H） | — |
| 版本控制 | 快照 + 回滚 | — |
