# 4. Bus System

## 4.1 Module Overview

The bus system is Agent OS's communication infrastructure. It comprises an event bus and a memory bus. The event bus sends notifications between Agents using dynamic TypeMask bitmap routing, while the memory bus coordinates consistency between memory layers.

```mermaid
graph TB
    subgraph Event_bus
        EB["EventBus<br/>broadcast channel + dynamic TypeMask bitmap routing"]
        EF["EventFilter<br/>filter by task_iri / event_types / type_mask"]
        SUB["Subscription<br/>O(1) bitmap matching"]
    end

    subgraph Memory_bus
        MB["MemoryBus<br/>memory event notifications"]
        CE["ConsistencyEngine<br/>MESI consistency"]
    end

    subgraph Event_types
        ET1["Task lifecycle<br/>6 types"]
        ET2["PDCA phases<br/>8 types"]
        ET3["Agent events<br/>3 types"]
        ET4["Memory events<br/>4 types"]
        ET5["5W2H constraints<br/>2 types"]
        ET6["Human approval<br/>2 types"]
        ET7["System events<br/>3 types"]
    end

    EB --> EF --> SUB
    MB --> CE
    ET1 & ET2 & ET3 & ET4 & ET5 & ET6 & ET7 --> EB
```

## 4.2 EventBus — Event Bus

**File**: `src/core/event_bus.rs`
**Implementation status**: ✅ Complete

An efficient event bus based on a broadcast channel and dynamic TypeMask bitmap routing.

### Core Design

**Dynamic TypeMask bitmap routing**:

Each event type receives a unique bit when it is first registered. A HashMap maintains the type-to-bitmap mapping, and an AND operation performs O(1) matching.

```rust
pub struct TypeMask {
    masks: HashMap<String, u64>,  // type name → bitmap
    next_bit: u32,                // next available bit
}

impl TypeMask {
    pub fn get_or_create_mask(&mut self, type_name: &str) -> u64;
    pub fn combine_masks(&self, types: &[String]) -> u64;
    pub fn get_mask(&self, type_name: &str) -> Option<u64>;
}
```

TypeMask supports up to 64 event types (the width of `u64`).

### `EventType` Enum

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

### Priority Mechanism

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventPriority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}
```

| Priority | Value | Use case |
|--------|-----|---------|
| Low | 0 | Logging and statistics |
| Normal | 1 | Standard Agent events (default) |
| High | 2 | Task state changes and important data updates |
| Critical | 3 | System errors and urgent repair notifications |

### Core Structs

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

### Core Methods

| Method | Purpose |
|------|------|
| `new(capacity)` | Create an event bus |
| `emit(task_iri, type, source, payload)` | Publish a Normal-priority event |
| `emit_with_priority(task_iri, type, source, payload, priority)` | Publish an event at the specified priority |
| `subscribe()` | Subscribe to all events |
| `subscribe_with_filter(subscription)` | Subscribe with a filter |
| `register_type(type_name)` | Register a type for bitmap routing |
| `get_combined_mask(types)` | Get the combined bitmap for multiple types |
| `spawn_consumer(types, handler)` | Start a background asynchronous consumer |

### O(1) Event-Matching Flow

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

    Note over EB: DA matches: DA.type_mask & event.type_mask != 0
    Note over EB: PA does not match: PA.type_mask & event.type_mask == 0
    EB-->>DA: Notify
```

### Asynchronous Consumers

EventBus supports `spawn_consumer` to start a background Tokio task that processes events:

```rust
bus.spawn_consumer(
    vec!["PLAN_COMPLETED".to_string(), "DO_COMPLETED".to_string()],
    |event| async move {
        // Process the event asynchronously
    }
);
```

## 4.3 MemoryBus — Memory Event Bus

**File**: `src/memory/memory_bus.rs`
**Implementation status**: ✅ Complete

The memory event bus handles cross-layer memory-consistency notifications.

**Event types**:

| Event | Trigger | Action |
|------|---------|---------|
| `Invalidate(iri)` | L0 data is modified | Invalidate all L1 cache lines |
| `WriteBack(iri)` | Dirty L1 data must be written back | Write L1 data back to L0 |
| `Evict(iri)` | L1 exceeds its token budget | Evict low-priority cache lines |
| `Prefetch(iri)` | An upcoming access is predicted | Load into L2 in advance |
| `Sync(iri, layer)` | Inter-layer synchronization request | Synchronize data in the specified layer |

**Batch operations**:

| Method | Purpose |
|------|------|
| `publish_invalidate(iri, scope)` | Invalidate a single node cache |
| `publish_invalidate_batch(iris, scope)` | Batch cache invalidation (merged into one event) |
| `publish_with_priority(iri, scope, priority)` | Publish an event with priority |

**Consistency-guarantee flow**:

```mermaid
sequenceDiagram
    participant DA1 as DA-1
    participant L1_1 as L1 (DA-1)
    participant MB as MemoryBus
    participant CE as ConsistencyEngine
    participant L1_2 as L1 (DA-2)
    participant L0

    DA1->>L1_1: Modify data (M state)
    L1_1->>MB: Publish WriteBack(iri)
    MB->>CE: Process consistency
    CE->>L0: Write back data
    CE->>MB: Publish Invalidate(iri)
    MB->>L1_2: Invalidate cache line (I state)
    Note over L1_2: Reload from L0 on next access
```

## 4.4 ConsistencyEngine — MESI Consistency

**File**: `src/memory/consistency_engine.rs`
**Implementation status**: ✅ Complete

```mermaid
stateDiagram-v2
    [*] --> Invalid
    Invalid --> Shared: Read Hit
    Invalid --> Exclusive: Read Miss (exclusive load)
    Shared --> Modified: Write Hit
    Shared --> Invalid: Invalidate
    Exclusive --> Modified: Write Hit
    Exclusive --> Shared: Read by Other
    Modified --> Shared: Write Back + Share
    Modified --> Invalid: Invalidate
```

## 4.5 JSON-LD Semantic Layer

**File**: `src/jsonld/`
**Implementation status**: ✅ Complete

The JSON-LD semantic layer provides semantic interoperability for the data bus: it is the “unified data bus” connecting all modules.

**Core components**:

| Component | File | Purpose |
|------|------|------|
| Context | `jsonld/context.rs` | `@context` semantic mapping |
| Types | `jsonld/types.rs` | `@type` polymorphic definitions |
| Utils | `jsonld/utils.rs` | IRI utility functions |
| Framing | `jsonld/framing.rs` | On-demand projection trimming |
| TypeRouter | `jsonld/type_router.rs` | Type-routing decisions |

**Semantic bus architecture**:

```mermaid
graph TB
    subgraph Semantic_mapping
        CTX["@context<br/>field-to-IRI mapping"]
        TYPE["@type<br/>polymorphic discovery"]
        ID["@id<br/>entity alignment"]
    end

    subgraph Semantic_operations
        FRAME["Framing<br/>on-demand projection"]
        ROUTE["TypeRouter<br/>type routing"]
        MERGE["graph merging<br/>entity fusion"]
    end

    subgraph Consumers
        SA["SA scheduling"]
        L3["L3 projection"]
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
