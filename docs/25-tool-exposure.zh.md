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
| KnowledgeWrite | 知识图更新和抽取 |
| Ingest | 文档索引和导入 |
| Skill | Skill 定义创建和转换 |
| Ontology | 本体注册、校验、比较和推理 |
| System | 工具发现 |

Plan 只获得只读组，其中包括 Web 按需组。Do 的常驻组包含可写工具；
Check 默认不获得 shell 工具。角色可见性与执行策略是两道独立的门。

## 激活和缓存行为

激活状态只属于一次运行。schema 顺序固定为：按注册顺序的常驻工具、按首次
激活顺序的按需工具、动态 micro-tool。一次运行内已激活的工具不会被删除或
重排，因此常驻前缀可以保持稳定以利用 prompt cache。

一次搜索最多激活 `max_tools_per_activation` 个工具，一次运行最多激活
`max_activated_tools` 个工具，默认值分别为 5 和 20。搜索结果会返回
`activated`，超出限制时会返回 `activation_skipped`。

追加工具会保留常驻前缀，但追加点之后的内容需要重新缓存。因此应使用少量、
明确的搜索请求。

## 配置和回滚

`token_optimization.tool_groups` 配置角色分组与激活上限。设置
`enabled: false` 会恢复旧的 fallback 工具暴露行为。网关会解析 OpenAI
兼容的 `prompt_tokens_details.cached_tokens` 或 `cache_read_input_tokens`，
并记录累计缓存命中率。
