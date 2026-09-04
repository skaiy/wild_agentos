# 4. 总线系统

> *本文是 [04-bus-system.md](04-bus-system.md) 的中文翻译。*

## 4.1 模块概览

总线系统是 Agent OS 的通信基础设施，包含事件总线和内存总线两个核心组件。事件总线负责 Agent 间的事件通知（动态 TypeMask 位图路由），内存总线负责记忆层间的一致性协调。

```mermaid
graph TB
    subgraph 事件总线
        EB["EventBus<br/>broadcast channel + 动态 TypeMask 位图路由"]
        EF["EventFilter<br/>按 task_iri / event_types / type_mask 过滤"]
        SUB["Subscription<br/>O(1) 位图匹配"]
    end

    subgraph 内存总线
        MB["MemoryBus<br/>内存事件通知"]
        CE["ConsistencyEngine<br/>MESI 一致性"]
    end

    subgraph 事件类型
        ET1["Task 生命周期<br/>6种"]
        ET2["PDCA 阶段<br/>8种"]
        ET3["Agent 事件<br/>3种"]
        ET4["Memory 事件<br/>4种"]
        ET5["5W2H 约束<br/>2种"]
        ET6["人工审批<br/>2种"]
        ET7["系统事件<br/>3种"]
    end

    EB --> EF --> SUB
    MB --> CE
    ET1 & ET2 & ET3 & ET4 & ET5 & ET6 & ET7 --> EB
```

## 4.2 EventBus — 事件总线

**文件**: `src/core/event_bus.rs`  
**实现状态**: ✅ 完整

基于 broadcast channel + 动态 TypeMask 位图路由的高效事件总线。

### 核心设计

**TypeMask 动态位图路由**:

每种事件类型在首次注册时被分配一个唯一的 bit 位，通过 HashMap 维护类型到位图的映射。匹配时通过 AND 运算实现 O(1) 过滤。

```rust
pub struct TypeMask {
    masks: HashMap<String, u64>,  // 类型名 → 位图
    next_bit: u32,                // 下一个可用 bit
}

impl TypeMask {
    pub fn get_or_create_mask(&mut self, type_name: &str) -> u64;
    pub fn combine_masks(&self, types: &[String]) -> u64;
    pub fn get_mask(&self, type_name: &str) -> Option<u64>;
}
```

TypeMask 支持最多 64 种事件类型（u64 位宽）。

### EventType 枚举

```rust
pub enum EventType {
    // Task lifecycle
    TaskCreated, TaskStarted, TaskCompleted, TaskFailed, TaskArchived,
    
    // PDCA phase events
    PlanStarted, PlanCompleted, DoStarted, DoCompleted,
    CheckStarted, CheckCompleted, ActStarted, ActCompleted,
    
    // Node events
    NodeCreated, NodeUpdated, NodeDeleted,
    
    // Agent events
    AgentStarted, AgentCompleted, AgentError,
    
    // System events
    CycleIteration, ThresholdExceeded, InterventionRequired,
    
    // Memory events
    MemoryInvalidate, MemoryWriteBack, MemoryPrefetch, MemoryLoad,
    
    // 5W2H constraint events
    DeadlineApproaching, BudgetExceeded,
    
    // Human approval events
    HumanApprovalRequired, HumanApprovalResult,
    
    // User supplementary input
    UserSupplementaryInput,
    
    // Custom
    Custom(String),
}
```

### 优先级机制

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventPriority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}
```

| 优先级 | 值 | 适用场景 |
|--------|-----|---------|
| Low | 0 | 日志、统计等 |
| Normal | 1 | 常规 Agent 事件（默认） |
| High | 2 | 任务状态变更、重要数据更新 |
| Critical | 3 | 系统错误、紧急修复通知 |

### 核心结构体

```rust
pub struct EventBus {
    sender: broadcast::Sender<Event>,
    event_count: AtomicU64,
    subscriber_count: AtomicU64,
    type_mask: std::sync::Mutex<TypeMask>,
}

pub struct Event {
    pub event_id: String,
    pub task_iri: String,
    pub event_type: String,
    pub source_agent_iri: String,
    pub payload: String,
    pub payload_json_ld: String,
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
    pub type_mask: u64,
    pub priority: EventPriority,
}

pub struct Subscription {
    pub subscriber_id: String,
    pub type_mask: u64,
    pub scope_iri: Option<String>,
    pub event_types: Vec<String>,
}

pub struct EventFilter {
    pub task_iri: Option<String>,
    pub event_types: Vec<String>,
    pub source_agent: Option<String>,
    pub type_mask: u64,
}
```

### 核心方法

| 方法 | 功能 |
|------|------|
| `new(capacity)` | 创建事件总线 |
| `emit(task_iri, type, source, payload)` | 发布 Normal 优先级事件 |
| `emit_with_priority(task_iri, type, source, payload, priority)` | 发布指定优先级事件 |
| `subscribe()` | 订阅所有事件 |
| `subscribe_with_filter(subscription)` | 带过滤的订阅 |
| `register_type(type_name)` | 注册类型到位图路由 |
| `get_combined_mask(types)` | 获取多个类型的组合位图 |
| `spawn_consumer(types, handler)` | 启动后台异步消费者 |

### 事件匹配 O(1) 流程

```mermaid
sequenceDiagram
    participant SA
    participant EB as EventBus
    participant PA
    participant DA

    SA->>EB: emit("PLAN_COMPLETED")
    Note over EB: type_mask = get_or_create_mask("PLAN_COMPLETED")
    Note over EB: Event.type_mask = 1 << 3

    PA->>EB: subscribe(AGENT_COMPLETED | TASK_COMPLETED)
    Note over EB: Subscription.type_mask = (1<<1) | (1<<5)

    DA->>EB: subscribe(PLAN_COMPLETED)
    Note over EB: Subscription.type_mask = 1 << 3

    Note over EB: DA 匹配: DA.type_mask & event.type_mask != 0
    Note over EB: PA 不匹配: PA.type_mask & event.type_mask == 0
    EB-->>DA: 通知
```

### 异步消费者

EventBus 支持 `spawn_consumer` 方法启动后台 tokio 任务处理事件：

```rust
bus.spawn_consumer(
    vec!["PLAN_COMPLETED".to_string(), "DO_COMPLETED".to_string()],
    |event| async move {
        // 异步处理事件
    }
);
```

## 4.3 MemoryBus — 内存事件总线

**文件**: `src/memory/memory_bus.rs`  
**实现状态**: ✅ 完整

内存事件总线，负责跨层内存一致性通知。

**事件类型**:

| 事件 | 触发条件 | 处理动作 |
|------|---------|---------|
| `Invalidate(iri)` | L0 数据被修改 | 使所有 L1 缓存行无效 |
| `WriteBack(iri)` | L1 脏数据需回写 | 将 L1 数据写回 L0 |
| `Evict(iri)` | L1 超出 Token 预算 | 淘汰低优先级缓存行 |
| `Prefetch(iri)` | 预测即将访问 | 提前加载到 L2 |
| `Sync(iri, layer)` | 层间同步请求 | 同步指定层的数据 |

**批量操作**:

| 方法 | 功能 |
|------|------|
| `publish_invalidate(iri, scope)` | 单节点缓存失效 |
| `publish_invalidate_batch(iris, scope)` | 批量缓存失效（合并为单次事件） |
| `publish_with_priority(iri, scope, priority)` | 带优先级的事件发布 |

**一致性保证流程**:

```mermaid
sequenceDiagram
    participant DA1 as DA-1
    participant L1_1 as L1 (DA-1)
    participant MB as MemoryBus
    participant CE as ConsistencyEngine
    participant L1_2 as L1 (DA-2)
    participant L0

    DA1->>L1_1: 修改数据 (M 状态)
    L1_1->>MB: 发布 WriteBack(iri)
    MB->>CE: 处理一致性
    CE->>L0: 回写数据
    CE->>MB: 发布 Invalidate(iri)
    MB->>L1_2: 使缓存行无效 (I 状态)
    Note over L1_2: 下次访问时从 L0 重新加载
```

## 4.4 ConsistencyEngine — MESI 一致性

**文件**: `src/memory/consistency_engine.rs`  
**实现状态**: ✅ 完整

```mermaid
stateDiagram-v2
    [*] --> Invalid
    Invalid --> Shared: Read Hit
    Invalid --> Exclusive: Read Miss (独占加载)
    Shared --> Modified: Write Hit
    Shared --> Invalid: Invalidate
    Exclusive --> Modified: Write Hit
    Exclusive --> Shared: Read by Other
    Modified --> Shared: Write Back + Share
    Modified --> Invalid: Invalidate
```

## 4.5 JSON-LD 语义层

**文件**: `src/jsonld/`  
**实现状态**: ✅ 完整

JSON-LD 语义层提供了数据总线的语义互操作能力，是连接所有模块的"统一数据总线"。

**核心组件**:

| 组件 | 文件 | 功能 |
|------|------|------|
| Context | `jsonld/context.rs` | @context 语义映射 |
| Types | `jsonld/types.rs` | @type 多态定义 |
| Utils | `jsonld/utils.rs` | IRI 工具函数 |
| Framing | `jsonld/framing.rs` | 按需投影裁剪 |
| TypeRouter | `jsonld/type_router.rs` | 类型路由决策 |

**语义总线架构**:

```mermaid
graph TB
    subgraph 语义映射
        CTX["@context<br/>字段→IRI映射"]
        TYPE["@type<br/>多态发现"]
        ID["@id<br/>实体对齐"]
    end

    subgraph 语义操作
        FRAME["Framing<br/>按需投影"]
        ROUTE["TypeRouter<br/>类型路由"]
        MERGE["图合并<br/>实体融合"]
    end

    subgraph 消费者
        SA["SA 调度"]
        L3["L3 投影"]
        SR["SkillRegistry"]
        AR["AgentRunner"]
    end

    CTX --> FRAME
    TYPE --> ROUTE
    ID --> MERGE
    FRAME --> SA & L3
    ROUTE --> SA & SR
    MERGE --> L3 & AR
```
