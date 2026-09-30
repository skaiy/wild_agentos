# 工具暴露

## 常驻工具和按需工具

每次 Agent 运行先加载该角色的常驻组。按需组不会进入函数 schema，直到
`tool_search` 返回匹配的工具名；这些名称从下一轮开始激活。

默认分组如下：

| 分组 | 用途 |
| --- | --- |
| Core | 文件读取 |
| Workspace | 工作区状态和前序 Agent 输出 |
| Write | 文件修改和 shell 命令 |
| Search | 文件与索引文档搜索 |
| Web | 公共网页搜索和读取 |
| KnowledgeRead | 只读知识库与图查询 |
| KnowledgePlan | Plan 安全的只读知识搜索 |
| KnowledgeWrite | 知识图更新和抽取 |
| Ingest | 文档索引和导入 |
| Skill | Skill 定义创建和转换 |
| Ontology | 本体注册、校验、比较和推理 |
| System | 工具发现 |

Plan 只获得只读组，其中包括 Web 按需组。Do 的常驻组包含可写工具；
Check 默认只读。角色可见性与执行策略是两道独立的门。

## 角色策略与执行

每次调用都会经过两道独立的门。首先，工具名必须存在于当前轮实际广告的 schema；
其次，运行时安全上下文必须满足角色策略。执行器仅从该上下文取得角色和 agent
身份，绝不从工具参数或模型输出取得。被拒绝的调用会返回结构化工具结果，不会调用
handler。

有效集合只能收窄：角色上限 ∩ 可信的每 agent 限制 ∩ 生效中的 supervisor 限制。
`tools_allowed` 等 Plan 元数据只是规划提示，不是执行控制。限制和按需激活状态只属于
一次运行，不会保存到共享执行器中。

| 角色 | 默认可见且可执行的工具 |
| --- | --- |
| Plan | `file_read`、`file_list`、`glob_search`、`grep_search`、`web_search`、`web_fetch`、`tool_search`、`rag_search`、`knowledge_list`、`knowledge_search`、`kg_search`、`knowledge_extract_code` |
| Do | 已配置角色分组中的每个已注册内置工具 |
| Check | `file_read`、`file_list`、`workspace_status`、`read_agent_output`、`glob_search`、`grep_search`、`rag_search`、`kg_search`、`web_search`、`web_fetch`、`tool_search`、`knowledge_list`、`knowledge_search`、`knowledge_extract_code`、`knowledge_query`、`knowledge_neighbors`、`kb_vector_search` |
| Act | `file_read`、`file_list`、`glob_search`、`grep_search`、`rag_search`、`kg_search`、`tool_search`、`knowledge_list`、`knowledge_search`、`knowledge_extract_code`、`knowledge_query`、`knowledge_neighbors`、`kb_vector_search` |

Plan、Check 与 Act 默认只读。只有在明确开启服务端
`token_optimization.tool_groups.check_bash_enabled` 时，Check 才能执行 `bash`。
此开关有风险：shell 命令可能执行不可信工作区内容并改变环境；除非 operator 接受该
风险，应保持默认值 `false`。审计 warning 只包含 agent、角色与工具名，不包含工具参数。

## 激活和缓存行为

激活状态只属于一次运行。schema 顺序固定为：按注册顺序的常驻工具、按首次
激活顺序的按需工具、动态 micro-tool。一次运行内已激活的工具不会被删除或
重排，因此常驻前缀可以保持稳定以利用 prompt cache。

一次搜索最多激活 `max_tools_per_activation` 个工具，一次运行最多激活
`max_activated_tools` 个工具，默认值分别为 5 和 20。搜索结果会返回
`activated`，超出限制时会返回 `activation_skipped`。

追加工具会保留常驻前缀，但追加点之后的内容需要重新缓存。因此应使用少量、
明确的搜索请求。

## 工具检索

`tool_search` 查询实时工具注册表，而不是固定目录。确定性的词法阶段会对工具名、
描述、参数名、参数描述和组名评分；精确工具名和前缀命中会额外加权，分数相同
时按工具名排序。默认最多返回 5 个结果，上限为 10。

返回的元数据包括工具名、组、单行描述、可见性状态（`resident`、`activated`
或 `on_demand`）和检索模式。目前模式为 `lexical`。未来可选的语义召回阶段
必须使用专用的服务端自有索引，并与词法分数融合；它绝不能写入或查询租户知识
命名空间。embedding 调用失败或返回无效向量时，检索必须保留词法结果并记录
回退，不能使用零向量继续搜索。

调用者角色只能由运行时提供；`tool_search` 不接受选择角色的参数。缺失或未知
的运行时角色会 fail closed。候选项会在排序前同时经过角色暴露分组和未改变的
执行白名单过滤，因此搜索结果既不会泄露该角色不能执行的工具，也不能激活它们。
激活仍然是追加式，并受现有的单次与单次运行上限约束。

## 配置和回滚

`token_optimization.tool_groups` 配置角色分组与激活上限。设置
`enabled: false` 会恢复旧的 fallback 工具暴露行为。网关会解析 OpenAI
兼容的 `prompt_tokens_details.cached_tokens` 或 `cache_read_input_tokens`，
并记录累计缓存命中率。
