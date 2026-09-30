# 14. 高级特性设计：根因分析、时间线版本控制、时序超图及 GNN

> 在 Wild AgentOS 上实现 4 个高级技能图谱特性的设计文档。
>
> **状态**：当前 `main` 已部分实现；本文保留未完成工作的设计细节。
> **已实现模块**：`src/skill_graph/`、`src/causal/`、`src/snapshots/` 和
> `src/graph_features/`
> **现有依赖**：petgraph 0.6、chrono、serde、uuid、sha2、hyperspace-engine

---

## 概述

本设计为现有的 `SkillGraphStore` 扩展了 4 个集成特性：

```
SkillGraphStore (现有)
  ├── CausalEngine (新增)    — 根因分析
  ├── TimelineStore (新增)   — 版本化图快照
  ├── TemporalHypergraph (新增) — 时间感知 N 元边
  └── GraphNeuralNet (新增)  — GNN 嵌入 + 推理
```

所有这 4 个特性共享相同的持久化层 (L0Store/redb)，并通过 EventBus 发送/接收事件。

---

## 1. 根因分析引擎 (Root Cause Analysis Engine)

### 1.1 设计目标

给定一组观测到的错误事件，在技能依赖图中向后追踪，以识别**最可能的根因技能**。

### 1.2 数据模型

```rust
// ── 扩展现有的 CausalChain / SkillCausalModel ──

/// 包含完整上下文的单次错误观测
pub struct CausalObservation {
    pub event_id: String,
    pub timestamp: DateTime<Utc>,
    pub skill_iri: String,
    pub error_class: String,
    pub error_signature: String,
    pub context: HashMap<String, String>,
    /// 传播到该错误的源错误（如果已知）
    pub propagation_from: Option<String>,
    /// 用于相似度匹配的 error_signature 嵌入向量
    pub signature_embedding: Option<EmbeddingVector>,
}

/// 归因引擎的推理结果
pub struct CausalInference {
    pub root_cause_iri: String,
    pub confidence: f32,
    pub propagation_path: Vec<CausalObservation>,
    pub supporting_evidence: Vec<String>,
    pub alternative_causes: Vec<(String, f32)>,  // (iri, confidence)
}

/// 持久化层优本化的存储型因果模型
pub struct CausalModelStore {
    // error_signature → [(skill_iri, count)]
    error_index: HashMap<String, Vec<(String, u32)>>,
    // skill_iri → error_signature → count
    error_profiles: HashMap<String, HashMap<String, u32>>,
    // (from, to) → propagation count
    propagation_edges: HashMap<(String, String), u32>,
    // skill_iri → prior probability (基于历史故障率的先验概率)
    prior_probability: HashMap<String, f32>,
}
```

### 1.3 算法

```
FUNCTION infer_root_cause(observed_errors: Vec<CausalObservation>)
    → Vec<CausalInference>

1. 从 SkillGraphStore 构建包含所有从 observed_errors 
   通过前置/扩展链接可达的技能的子图（反向遍历）

2. 为子图中的每个节点计算贝叶斯后验概率：
   P(node_is_root | observed) ∝ P(observed | node_is_root) × P(node)

   其中：
   - P(node) = 基于历史故障率的先验概率 (prior_probability)
   - P(observed | node) = ∏ P(error_i propagates_from node)
     通过追踪从 node 到每个观测到错误的路径计算得出
     （按 propagation_edge 计数加权）

3. 按后验概率对节点进行排序 → 返回前 K 个作为根因

4. 对于每个候选节点，通过从候选节点到每个观测到错误的广度优先搜索 (BFS)
   重构 propagation_path，遵循最可能的传播边
```

### 1.4 需要创建/修改的文件

| 文件 | 操作 | 说明 |
|------|--------|-------------|
| `src/causal/mod.rs` | 创建 | 模块入口，重新导出接口 |
| `src/causal/engine.rs` | 创建 | `CausalEngine` — 推理算法的具体实现 |
| `src/causal/store.rs` | 创建 | `CausalModelStore` — 持久化与查询 |
| `src/causal/types.rs` | 创建 | `CausalObservation`、`CausalInference` 等类型定义 |
| `src/skill_graph/types.rs` | 修改 | 在现有的 `CausalEvent` 中添加 `signature_embedding` 字段 |
| `src/skill_graph/graph_store.rs` | 修改 | 集成 `CausalEngine` 以自动记录故障 |

### 1.5 API

```rust
impl CausalEngine {
    pub fn new(store: Arc<SkillGraphStore>) -> Self;

    /// 记录一个观测到的错误事件
    pub fn record_observation(&self, obs: CausalObservation);

    /// 从一批观测结果中推理根因
    pub fn infer_root_cause(
        &self,
        observations: &[CausalObservation],
        top_k: usize,
    ) -> Vec<CausalInference>;

    /// 获取传播图作为 petgraph DiGraph，用于可视化
    pub fn propagation_graph(&self) -> DiGraph<String, f32>;
}
```

---

## 2. 技能图谱时间线版本控制 (Skill Graph Timeline Versioning)

### 2.1 设计目标

追踪技能图谱随时间发生的每一次变更，支持：
- 特定时间点查询（“在时间戳 T 时，技能图谱是什么样子的？”）
- 回滚到任何历史快照
- 两个快照之间的差异比对 (Diff)
- 所有变更的审计轨迹

### 2.2 数据模型

```rust
/// 图的单次变更记录
#[derive(Serialize, Deserialize)]
pub enum GraphMutation {
    SkillRegistered(SkillGraphNode),
    SkillUpdated { old: SkillGraphNode, new: SkillGraphNode },
    SkillRemoved(SkillGraphNode),
    LinkAdded { source: String, target: String, link_type: SkillLinkType },
    LinkRemoved { source: String, target: String, link_type: SkillLinkType },
    HyperedgeAdded(Hyperedge),
    HyperedgeRemoved(Hyperedge),
    MOCAdded(MOCNode),
    MOCChanged(MOCNode),
}

/// 完整的特定时间点图快照
#[derive(Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub snapshot_id: String,
    pub timestamp: DateTime<Utc>,
    pub label: String,
    pub skills: Vec<SkillGraphNode>,
    pub hyperedges: Vec<Hyperedge>,
    pub mocs: Vec<MOCNode>,
    pub fragments: Vec<KnowledgeFragment>,
    pub parent_snapshot_id: Option<String>,
    pub mutation: Option<GraphMutation>,  // 自父快照以来的变更
}

/// 两个快照之间的差异 (Diff)
pub struct GraphDiff {
    pub from_snapshot: String,
    pub to_snapshot: String,
    pub skills_added: Vec<SkillGraphNode>,
    pub skills_removed: Vec<SkillGraphNode>,
    pub skills_modified: Vec<(SkillGraphNode, SkillGraphNode)>,
    pub hyperedges_added: Vec<Hyperedge>,
    pub hyperedges_removed: Vec<Hyperedge>,
}
```

### 2.3 存储策略

使用**分块快照 + 写前日志 (chunked snapshot + write-ahead log)** 的方法：

```
TimelineStore
  ├── 全量快照 (Full snapshots)：每发生 N 次变更时生成（可配置，默认 100 次）
  │   在 L0 或 redb 中存储为序列化的 GraphSnapshot
  ├── 增量变更 (Incremental)：全量快照之间仅存储 GraphMutation
  │   在重建时基于最近的全量快照重新应用增量变更
  └── 索引：[snapshot_id → timestamp, parent_id, mutation_count]
```

快照采用**写时复制 (copy-on-write)**：仅在显式触发（例如在执行高风险操作之前）或定期自动触发时，快照才会克隆当前的图状态。这避免了每次变更时 O(n) 的开销。

### 2.4 需要创建/修改的文件

| 文件 | 操作 | 说明 |
|------|--------|-------------|
| `src/temporal/mod.rs` | 创建 | 模块入口 |
| `src/temporal/timeline.rs` | 创建 | `TimelineStore` — 快照创建、回滚与差异计算 |
| `src/temporal/types.rs` | 创建 | `GraphSnapshot`、`GraphMutation` 和 `GraphDiff` 的类型定义 |
| `src/skill_graph/graph_store.rs` | 修改 | 挂钩 (Hook) 所有变更方法以推送到 TimelineStore |
| `src/skill_graph/types.rs` | 修改 | 将 `SnapshotRecord` 保留为轻量级引用 |

### 2.5 API

```rust
impl TimelineStore {
    pub fn new(l0: Arc<L0Store>) -> Self;

    /// 记录一次变更（由 SkillGraphStore 钩子调用）
    pub fn record_mutation(&self, mutation: GraphMutation);

    /// 创建一个显式的全量快照
    pub fn create_snapshot(&self, store: &SkillGraphStore, label: &str) -> String;

    /// 列出所有快照
    pub fn list_snapshots(&self) -> Vec<GraphSnapshot>;

    /// 重建指定快照处的图状态
    pub fn reconstruct(&self, snapshot_id: &str) -> Option<GraphSnapshot>;

    /// 将 SkillGraphStore 回滚到指定快照
    pub fn rollback(&self, snapshot_id: &str, store: &SkillGraphStore) -> Result<(), Error>;

    /// 比对两个快照的差异
    pub fn diff(&self, from: &str, to: &str) -> Option<GraphDiff>;

    /// 获取两个快照之间的所有变更日志
    pub fn mutation_log(&self, from: &str, to: &str) -> Vec<GraphMutation>;
}
```

---

## 3. 时序超图 (Temporal Hypergraph)

### 3.1 设计目标

扩展现有的 `Hyperedge` 结构以支持时间感知的 N 元关系：
- 仅在特定时间窗口内有效的超边
- 时间范围查询（“在日期 X 时哪些超边是活跃的？”）
- 时序演进追踪（“该超边随时间发生了怎样的改变？”）
- 时序超边之间的因果约束推理

### 3.2 数据模型

```rust
/// 随时间存在/演进的超边
#[derive(Serialize, Deserialize)]
pub struct TemporalHyperedge {
    pub hyperedge_id: String,
    pub name: String,
    pub description: String,
    pub components: Vec<String>,
    pub target_composite: Option<String>,
    pub composition_type: CompositionType,
    pub weight: f32,
    pub metadata: HashMap<String, String>,

    // ── 时序扩展 ──
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,  // None 表示依然活跃
    pub intervals: Vec<TimeInterval>,        // 不连续的活跃时间段
    pub version: u32,
    pub supersedes: Option<String>,          // 上一版本的 hyperedge_id
    pub superseded_by: Option<String>,
}

/// 连续的时间区间
#[derive(Serialize, Deserialize)]
pub struct TimeInterval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub label: Option<String>,
}

/// 用于高效时间范围查询的时序索引
pub struct TemporalIndex {
    // 类似于 B-tree 的结构，以 (timestamp, hyperedge_id) 为键
    // 实现为通过二分查找排序的 Vec<(DateTime<Utc>, String)>
    // 生产环境：使用带复合键的 redb 表
    entries: Vec<(DateTime<Utc>, TemporalIndexEntry)>,
}

pub struct TemporalIndexEntry {
    pub hyperedge_id: String,
    pub event_type: TemporalEventType,  // Created, Modified, Activated, Deactivated
}

pub enum TemporalEventType {
    Created,
    Modified,    // 元数据/权重改变
    Activated,   // 进入 valid_from 期间
    Deactivated, // 超过 valid_until
    Superseded,  // 被更新的版本替代
}
```

### 3.3 查询 API

```rust
impl TemporalHypergraphStore {
    pub fn new(l0: Arc<L0Store>) -> Self;

    // ── CRUD ──
    pub fn register_hyperedge(&self, he: TemporalHyperedge);
    pub fn update_hyperedge(&self, he: TemporalHyperedge);  // 创建新版本
    pub fn deactivate(&self, hyperedge_id: &str, at: DateTime<Utc>);

    // ── 时间范围查询 ──
    pub fn active_at(&self, instant: DateTime<Utc>) -> Vec<TemporalHyperedge>;
    pub fn active_between(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<TemporalHyperedge>;
    pub fn history_of(&self, hyperedge_id: &str) -> Vec<TemporalHyperedge>;

    // ── 因果约束推理 ──
    /// 寻找满足 he1 的结束时间 ≤ he2 的开始时间的时间上有序超边对
    /// （潜在的因果链）
    pub fn find_causal_candidates(&self) -> Vec<(TemporalHyperedge, TemporalHyperedge)>;
}
```

### 3.4 需要创建/修改的文件

| 文件 | 操作 | 说明 |
|------|--------|-------------|
| `src/temporal/hypergraph.rs` | 创建 | `TemporalHypergraphStore` + `TemporalIndex` |
| `src/temporal/types.rs` | 创建 | `TemporalHyperedge`、`TimeInterval`、`TemporalEventType` |
| `src/temporal/mod.rs` | 修改 | 添加超图模块导入 |
| `src/skill_graph/graph_store.rs` | 修改 | 将超边的 CRUD 操作委托给 TemporalHypergraphStore |
| `src/skill_graph/types.rs` | 修改 | 给 `Hyperedge` 添加 `version` 字段 |

### 3.5 与 CausalEngine 的集成

时序超图为因果分析引擎提供数据输入：
- `TemporalHypergraphStore::find_causal_candidates()` 返回时间区间不重叠的超边对
- 这些对将成为 `CausalEngine.propagation_graph()` 中的先验边
- 如果超边 A 活跃在超边 B 之前，且两者共享组件，则 A 可能是 B 的因果前驱

---

## 4. 图神经网络 (GNN) 集成

### 4.1 设计目标

利用图神经网络计算技能图谱节点的**学习型嵌入 (learned embeddings)**，以支持：
- **节点分类**：从图结构预测技能类别/成熟度
- **链接预测**：推荐缺失的前置依赖链接或关联链接
- **异常检测**：标记那些嵌入特征偏离其邻居技能的节点
- **技能推荐**：通过学习到的相似度寻找与当前任务最相关的技能

### 4.2 架构设计

```
GraphNeuralNet
  ├── FeatureExtractor — 将 SkillGraphNode 转换为特征张量
  │   使用现有的 SkillGraphEmbedder (庞加莱结构化嵌入)
  │   + 技能属性（标签、成功率、成熟度、节点类型）
  │   + 邻域特征（度数、中心性、社区）
  ├── GNNModel — 图卷积层
  │   在纯 Rust 中实现简化的 GCN (图卷积网络)
  │   共 2 层: [输入 → 隐藏层(64) → 输出层(32)]
  │   基于 petgraph 邻接表进行手写矩阵运算
  ├── TrainingEngine — 根据图的变更进行在线学习
  │   链接预测目标：在已知节点嵌入时最大化 P(edge_exists)
  │   负采样 (Negative sampling)：采样无边节点对进行对比损失计算
  └── InferenceEngine — 提供嵌入与预测 API
```

### 4.3 GCN 实现（纯 Rust）

由于无法在 Rust 二进制中引入 PyTorch/TensorFlow，我们手动实现一个**极简 GCN**：

```rust
pub struct GraphConvolutionLayer {
    pub weight: Vec<Vec<f32>>,  // [input_dim × output_dim]
    pub bias: Vec<f32>,         // [output_dim]
}

impl GraphConvolutionLayer {
    pub fn forward(
        &self,
        features: &[Vec<f32>],      // [N × input_dim]
        adjacency: &[Vec<f32>],     // [N × N] 归一化的邻接矩阵
    ) -> Vec<Vec<f32>> {            // [N × output_dim]
        // H = σ(A · X · W + b)
        // 其中 A = D^(-1/2) · (A + I) · D^(-1/2) (对称归一化)
        let mut result = mat_mul(adjacency, features);  // [N × input_dim]
        result = mat_mul(&result, &self.weight);         // [N × output_dim]
        result = add_bias(result, &self.bias);           // [N × output_dim]
        result = relu(result);                            // [N × output_dim]
        result
     }
}

pub struct GNNModel {
    pub layer1: GraphConvolutionLayer,  // input_dim → hidden_dim
    pub layer2: GraphConvolutionLayer,  // hidden_dim → output_dim
    pub dropout: f32,
}
```

图神经网络在**运行时并不通过 SGD 进行训练** — 我们采用**几何嵌入对齐**的方法：
1. 使用现有的 `SkillGraphEmbedder` (庞加莱坐标) 提取结构化特征
2. 进行 2-3 轮**迭代邻域聚合 (iterative neighborhood aggregation)**（即对邻居节点嵌入求均值池化）
3. 计算出的 learned embedding 将融合图结构特征与节点自身属性

为了支持真正的权重训练，我们提供**离线训练模式**：
- 将技能图谱导出为 JSON 文件
- 在 Python 中使用标准的 GNN 框架进行训练
- 将学习到的权重重新导入回 `GraphConvolutionLayer` 结构体中

### 4.4 特征提取

```rust
pub struct NodeFeatures {
    // 结构特征 (来自现有的 SkillGraphEmbedder)
    pub poincare_coords: [f64; 4],
    // 图度量指标
    pub in_degree: f32,
    pub out_degree: f32,
    pub page_rank_score: f32,
    pub betweenness_score: f32,
    pub community_id: i32,
    // 技能属性（独热编码或归一化值）
    pub node_type: u8,           // Atomic=0, Composite=1, MOC=2 等
    pub maturity: f32,           // 实验性=0.0, 稳定=1.0
    pub success_rate: f32,
    pub security_level: u8,
    // 邻域特征
    pub avg_neighbor_success_rate: f32,
    pub prerequisite_depth: u32,
}
```

### 4.5 需要创建/修改的文件

| 文件 | 操作 | 说明 |
|------|--------|-------------|
| `src/gnn/mod.rs` | 创建 | 模块入口 |
| `src/gnn/layer.rs` | 创建 | `GraphConvolutionLayer` — 前向传播逻辑 |
| `src/gnn/model.rs` | 创建 | `GNNModel` — 两层图卷积网络 (GCN) |
| `src/gnn/features.rs` | 创建 | `FeatureExtractor` — 节点到特征张量的提取 |
| `src/gnn/predict.rs` | 创建 | `LinkPredictor`、`NodeClassifier` 实现 |
| `src/gnn/train.rs` | 创建 | `Trainer` — 离线训练接口（导出/导入权重） |
| `src/skill_graph/graph_store.rs` | 修改 | (可选) 在图发生变更后调用 `GNNModel.forward()` |

### 4.6 API

```rust
impl GNNModel {
    pub fn new(input_dim: usize, hidden_dim: usize, output_dim: usize) -> Self;

    /// 前向传播：计算图中所有节点的特征嵌入
    pub fn forward(&self, features: &NodeFeatures, adj: &[Vec<f32>]) -> Vec<Vec<f32>>;

    /// 从 Python 训练导出的 JSON 中加载权重
    pub fn load_weights(&mut self, path: &Path) -> Result<(), Error>;

    /// 导出当前权重以供离线训练
    pub fn export_weights(&self) -> Value;

    /// 计算两个节点之间的链接预测分数
    pub fn predict_link(&self, emb_i: &[f32], emb_j: &[f32]) -> f32;
}

impl FeatureExtractor {
    pub fn new(store: Arc<SkillGraphStore>, algorithms: Arc<SkillGraphAlgorithms>) -> Self;
    pub fn extract(&self, skill_iri: &str) -> Option<NodeFeatures>;
    pub fn extract_all(&self) -> HashMap<String, NodeFeatures>;
}
```

---

## 5. 跨特性集成

```
                    ┌─────────────────────────────────┐
                    │         EventBus                 │
                    │  发送: CAUSAL_EVENT,            │
                    │  GRAPH_MUTATION, TEMPORAL_EVENT  │
                    └──────┬──────────────┬────────────┘
                           │              │
               ┌───────────▼─────┐  ┌─────▼────────────┐
               │  CausalEngine   │  │  TimelineStore   │
               │  订阅错误事件   │  │  订阅图谱变更    │
               └────────▲────────┘  └─────▲────────────┘
                        │                 │
               ┌────────┴─────────────────┴──────────┐
               │         SkillGraphStore              │
               │  （所有的图变更动作都会钩住这两个引擎）│
               └────────┬────────────────────────────┘
                        │
               ┌────────▼──────────┐  ┌───────────────┐
               │TemporalHypergraph │  │    GNNModel    │
               │(时间感知边)       │  │(计算嵌入+预测)│
               │ 为因果引擎提供边先验│  └───────────────┘
               └───────────────────┘
```

### 5.1 事件流

1. **智能体执行失败** → `EventBus` 发送错误事件。
2. **CausalEngine** 监听到错误事件 → 记录一条 `CausalObservation`。
3. **SkillGraphStore 变更** (技能注册/更新) → 触发变更钩子：
   - 调用 `TimelineStore.record_mutation()` 记录版本变更。
   - 如果超边发生变化，调用 `TemporalHypergraph.update_hyperedge()`。
4. **按需推理**：`CausalEngine.infer_root_cause()` 查询 `TemporalHypergraphStore.find_causal_candidates()` 以获得时间顺序上的边先验。
5. **GNNModel** 定期拉取 `SkillGraphStore` 快照与 `SkillGraphAlgorithms` 计算出的度量指标，重新计算节点的图嵌入。

---

## 6. 持久化策略

| 组件 | 主要存储介质 | 备用存储/索引 |
|-----------|----------------|--------------|
| CausalModelStore | L0Store (redb) | 内存中的 HashMap（用于热路径） |
| TimelineStore | redb (变更日志) + L0 (全量快照) | — |
| TemporalHypergraphStore | L0Store | 内存中的 TemporalIndex |
| GNNModel | 权重 JSON 文件（按需加载） | 启动时基于图结构计算得出 |

---

## 7. 实施顺序

```
第一阶段：基础框架搭建
  ├── 1a. causal/types.rs + causal/mod.rs
  ├── 1b. temporal/types.rs + temporal/mod.rs
  └── 1c. temporal/timeline.rs (TimelineStore 骨架)

第二阶段：因果引擎开发
  ├── 2a. causal/store.rs (CausalModelStore 持久化)
  ├── 2b. causal/engine.rs (CausalEngine 贝叶斯推理实现)
  └── 2c. 挂钩 SkillGraphStore 变更动作

第三阶段：时序超图开发
  ├── 3a. temporal/hypergraph.rs (TemporalHypergraphStore)
  ├── 3b. TemporalIndex 索引实现
  └── 3c. 因果引擎集成 (添加 find_causal_candidates 先验)

第四阶段：GNN 模块开发
  ├── 4a. gnn/features.rs (特征提取器)
  ├── 4b. gnn/layer.rs (图卷积层计算)
  ├── 4c. gnn/model.rs + gnn/predict.rs (前向传播与预测)
  └── 4d. 挂钩 SkillGraphStore 运行时更新嵌入
```

---

## 8. 测试策略

| 特征模块 | 单元测试 | 集成测试 |
|---------|-----------|-------------------|
| CausalEngine | 使用合成图对推理算法进行准确度验证 | 观测异常 → 推理根因 → 验证故障树 的全链路测试 |
| TimelineStore | 快照的创建、回滚与 Diff 校验 | 执行回滚后，验证图的最终状态与期望版本一致 |
| TemporalHypergraph | 时间区间查询与版本变化追踪测试 | 时序候选链生成与排序测试 |
| GNN | 前向传播的数值正确性及特征提取模块测试 | 权重导出/导入的 JSON 格式兼容性测试 |
