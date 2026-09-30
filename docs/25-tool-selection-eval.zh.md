# 工具选择黄金评估

此评估衡量 Plan、Do 和 Check 角色的第一轮工具选择响应，不会执行模型选择的工具。

用例位于 `eval/tool_selection/cases.json`，与产品夹具分开。当前共有 60 个用例，
每个角色 20 个；合计包括正确选工具 15 个、按需发现 9 个、MCP/技能使用 6 个、
不使用工具 9 个、提示注入防护 12 个、角色禁止工具请求 9 个。

## 运行

离线模式使用每个用例中记录的工具调用，可确定性地验证用例结构、指标和报告生成；
它不需要网络或凭据：

```bash
./scripts/run_tool_selection_eval.sh
# 或
cargo run --bin tool-selection-eval -- --offline
```

JSON 报告和简短 Markdown 摘要会写入 `target/tool-selection-eval/`。CI 会运行离线命令。

要生成在线基线，请只通过环境变量提供配置：

```bash
export TOOL_SELECTION_EVAL_PROVIDER="provider-name"
export TOOL_SELECTION_EVAL_MODEL="model-name"
export TOOL_SELECTION_EVAL_BASE_URL="https://example.invalid/v1/chat/completions"
export TOOL_SELECTION_EVAL_API_KEY="..."
cargo run --bin tool-selection-eval -- --output target/tool-selection-live
```

仓库不提交模型、端点或密钥。在线模式固定 temperature 为 `0.0`、seed 为 `2710`，
并在报告中记录这些值和当前提交。请按提交记录基线产物，但将其放在源码树之外或团队
批准的结果存储中。

## 指标

- **Top-1 正确工具率**：第一个工具是预期工具；对于“不用工具”用例，无调用即正确。
- **必需工具召回率 / 漏掉的必需工具**：响应中出现的预期工具数除以预期工具总数。
- **过度调用率**：不用工具的用例中发生任何调用，或调用数多于预期。
- **错误工具率**：每个用例中非预期工具调用的数量。
- **禁止工具尝试数**：命中用例禁止列表的调用，按角色报告。
- **tool_search hit@3**：有预期搜索结果的用例中，目标是否位于记录的前三项。
- **工具定义和菜单 token 数**：序列化函数定义与可读工具菜单的确定性字符估算 token 数。
  它不依赖提供商 tokenizer，也能显示描述去重带来的变化。
- **工具数组前缀稳定性**：同一角色连续轮次中，完整暴露工具名数组相同的比例。
- **提示缓存命中代理**：同一角色连续轮次中，定义和菜单文本都相同的比例。这是代理指标，
  不是提供商缓存遥测。

## 添加用例

在 `eval/tool_selection/cases.json` 添加对象，包含唯一 `id`、`Plan`、`Do` 或 `Check`
角色、类别、任务、可选上下文和注入工具结果、`expected_tools`、`forbidden_tools` 与
`no_tool_correct`。

`expected_tools` 与 `no_tool_correct` 必须恰好有一个有效。为离线模式添加
`recorded_tool_calls`；按需发现用例添加 `tool_search_top3`。注入用例应保留足以测试
防护的不可信返回文本，且不得包含凭据或敏感内容。

运行器在执行时从内核读取实际工具定义和可读菜单。因此，工具暴露改变后，同一用例会
显示工具表面成本和稳定性的变化。TODO：该单轮运行器尚不执行多轮按需加载和完整注册表
搜索；当这些运行时行为在下一轮暴露不同工具表面时，指标会测量其影响。
