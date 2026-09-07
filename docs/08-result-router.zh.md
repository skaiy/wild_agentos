# 8. 工具结果智能路由

> *本文是 [08-result-router.md](08-result-router.md) 的中文翻译。*

> 当工具返回大结果时，自动选择最优处理策略，避免 Token 浪费

## 问题背景

LLM Agent 执行工具调用时，工具可能返回大量数据（如目录列表、搜索结果、代码文件内容）。直接将大结果塞入 LLM 上下文会导致：
- Token 消耗剧增
- 关键信息被淹没
- API 调用可能超限

## 路由决策流程

```mermaid
flowchart TD
    INPUT["工具返回结果"] --> META["分析结果元数据<br/>ToolResultMeta"]
    META --> ROUTER["ResultRouter.route()"]
    ROUTER --> SIZE{"结果大小?"}

    SIZE -->|"< 4KB"| PASS["PassThrough<br/>直接透传"]
    SIZE -->|"4KB-50KB"| STRUCT{"是否 JSON?"}

    STRUCT -->|"是 JSON"| TRUNC_J["Truncate<br/>JSON 智能截断<br/>保留前N个+标记"]
    STRUCT -->|"非 JSON"| TRUNC_T["Truncate<br/>文本智能截断<br/>按行截断+统计"]

    SIZE -->|"> 50KB"| LARGE_STRUCT{"是否结构化 JSON?"}
    LARGE_STRUCT -->|"是"| GRAPHIFY["Graphify<br/>图谱化存储<br/>+ 微工具注入"]
    LARGE_STRUCT -->|"否"| SUMMARIZE["Summarize<br/>预览+末尾预览<br/>+ read_full_result"]

    GRAPHIFY --> MT["生成微工具<br/>query_{EntityType}<br/>get_entity_details<br/>expand_relation"]
    SUMMARIZE --> STORE["完整结果存储<br/>注入 read_full_result"]
```

## 核心组件

### ResultRouter — 路由决策引擎

```rust
pub struct ResultRouter {
    settings: ToolResultRouterSettings,
}

pub enum RouteDecision {
    PassThrough,
    Truncate { max_chars: usize },
    Graphify { call_id: String, graph_name: String },
    Summarize { call_id: String, preview_size: usize },
}
```

### ToolResultRouterSettings

**配置文件**: `config.yaml` 中 `tool_result_router` 段

```yaml
tool_result_router:
  enabled: true
  threshold_small: 2048          # 小结果阈值（字节），小于此值直接透传
  threshold_large: 8192          # 大结果阈值（字节），超过此值考虑图谱化
  preview_size: 2000             # 摘要预览大小
  max_graph_entities: 500        # 图谱化最大实体数
  max_micro_tools: 5             # 最大微工具数
  sparql_query_timeout_ms: 100   # SPARQL 查询超时
  auto_cleanup: true             # 自动清理过期图谱
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `threshold_small` | 2048 | 透传阈值（字节） |
| `threshold_large` | 8192 | 截断/图谱化阈值（字节） |
| `preview_size` | 2000 | 摘要预览大小 |
| `max_graph_entities` | 500 | 图谱化最大实体数 |
| `max_micro_tools` | 5 | 最大微工具数 |

### 智能截断策略

**JSON 截断**（`smart_truncate_json`）：
- 识别 JSON 数组 → 保留前 N 个元素 + `[截断: 共 M 个, 保留 N 个]`
- 识别 JSON 对象 → 保留前 N 个 key + 截断标记
- 非 JSON → 退回文本截断

**文本截断**（`smart_truncate_text`）：
- 按行截断，保留完整行
- 统计总行数和保留行数
- UTF-8 字符边界安全处理

### GraphifyEngine — 图谱化引擎

将 JSON 工具结果递归解析为知识图谱节点：

```mermaid
graph TD
    JSON["JSON 工具结果"] --> PARSE["递归解析"]
    PARSE --> OBJ["对象 → NodeDef<br/>id=路径, type=对象类型"]
    PARSE --> ARR["数组 → 批量 NodeDef<br/>id=路径[i]"]
    PARSE --> PRIM["基本类型 → 对象属性"]

    OBJ --> EDGE["父→子 EdgeDef<br/>relation=字段名"]
    ARR --> EDGE

    OBJ --> ANALYSIS["SchemaAnalysis<br/>实体类型分布<br/>关系类型统计"]
    ANALYSIS --> SUMMARY["数据摘要<br/>实体数/关系数/类型分布"]
    ANALYSIS --> MICRO["微工具生成"]
```

**SchemaAnalysis** 输出：
- `entity_types: Vec<(String, usize)>` — 实体类型及计数
- `relation_types: Vec<String>` — 关系类型列表
- `total_entities / total_relations` — 总计

### MicroToolGenerator — 微工具生成

根据图谱化结果动态生成查询工具，注入 LLM 上下文：

| 微工具类型 | 名称模式 | 说明 |
|-----------|---------|------|
| EntityTypeQuery | `query_{EntityType}` | 按实体类型查询 |
| EntityDetails | `get_entity_details` | 获取实体详情 |
| RelationTraversal | `expand_relation` | 遍历关系 |
| FullTextRead | `read_full_result` | 读取完整存储结果 |

```rust
pub enum MicroToolType {
    EntityTypeQuery { entity_type: String, graph_name: String },
    EntityDetails { graph_name: String },
    RelationTraversal { graph_name: String },
    FullTextRead { storage_key: String },
}
```

## 集成到 AgentRunner

工具结果路由在 `AgentRunner.route_tool_result()` 中自动执行：

```mermaid
sequenceDiagram
    participant AR as AgentRunner
    participant TE as ToolExecutor
    participant RR as ResultRouter
    participant KGS as KnowledgeGraphStore
    participant LLM as LLM API

    AR->>TE: execute_tool(name, input)
    TE-->>AR: tool_result (可能很大)
    AR->>RR: route(result, tool_name, call_id)
    RR-->>AR: RouteDecision

    alt PassThrough
        AR->>LLM: 直接透传结果
    else Truncate
        AR->>AR: smart_truncate(result)
        AR->>LLM: 截断后的结果
    else Graphify
        AR->>KGS: write_quads(graphified)
        AR->>LLM: 摘要 + 微工具定义
    else Summarize
        AR->>LLM: 预览 + read_full_result 工具
    end
```

## UTF-8 安全处理

所有截断操作都确保在字符边界进行：

```rust
fn safe_slice(s: &str, max_len: usize) -> &str {
    if max_len >= s.len() { return s; }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
```
