> *本文是 [18-evolution-roadmap.md](18-evolution-roadmap.md) 的中文版本。*

---

# 18. Wild AgentOS 演进路线图（v0.1.6 之后）

本文是 v0.1.6 之后的公开战略路线图。它说明产品要验证的能力边界，而不是对具体 API、发布日期或性能指标的承诺。

## 产品定位与边界

Wild AgentOS 是一个 **semantic-kernel AgentOS**：以 Rust PDCA 编排为核心，使用 Oxigraph RDF/SPARQL 作为语义图底座，配合 Hyperspace、`IsolationClaims` 命名契约，以及本体 Action 的**数据**沙箱。

- 它不是裸机微内核操作系统。
- 它不追求完整复刻某个专有平台。
- 图查询继续使用 Oxigraph 与 SPARQL；不以 Nebula/Cypher 替换它。
- 不混合 其他业务/产品仓库或产品边界。
- `IsolationClaims` 的 mint 是安全名称契约，**不等于**迁移已有数据。当前历史键状态见 [17. Isolation Contract](17-isolation-contract.zh.md)。

## 发布桶

### v0.1.6 — 已完成：隔离命名与 fail-closed 接线

经验证的 JWT `IsolationClaims` 已为 graph、blob、vector 与 L0 mint 目标；HTTP 路径在缺少 claims 时 fail closed。历史键没有迁移。详见 [17. Isolation Contract](17-isolation-contract.zh.md)。

### v0.1.7 — 已完成：Isolation Proof & Eval

已交付可审计的隔离证明包：

- 只读 `isolation-diagnose` CLI，区分 claims-minted 目标与历史键；
- 面向客户的[隔离矩阵](17-isolation-matrix.zh.md)，以及纳入 CI 的 fail-closed `isolation_contract` golden case；
- 可选、显式的 `isolation-migrate` 历史键迁移工具；默认只诊断，绝不静默 `UNION` 或把 mint 表述为迁移。

对应工作项：[R17 诊断 CLI](https://github.com/skaiy/wild_agentos/issues/82)、[CI 与隔离矩阵](https://github.com/skaiy/wild_agentos/issues/83)、[可选迁移](https://github.com/skaiy/wild_agentos/issues/84)。

### v0.1.8 — 已完成：Ontology Action HITL

已把本体 Action 数据沙箱扩展为人工审批闭环：

- 可通过 `commit_strategy` 保留 staging graph，供审批后 merge 或 discard，并支持 TTL 到期；
- 可配置护栏，并支持 SPARQL `ASK` 断言集与 `high_risk` hook；
- 通过 EventBus 发布 committed、pending、approved、rejected、violated 结果的 `ACTION_AUDIT` 审计事件。

这仍是数据沙箱，不承诺任意代码执行沙箱；当前实现边界见 [15. 本体动作执行沙箱](15-ontology-action-sandbox.zh.md)。对应工作项：[HITL](https://github.com/skaiy/wild_agentos/issues/85)、[护栏与断言](https://github.com/skaiy/wild_agentos/issues/86)、[事件审计](https://github.com/skaiy/wild_agentos/issues/87)。

### v0.2.0 — 已完成：控制平面 + Skill CI

- Skill package 格式已提供 CI gate，用于 package 验证和 golden input/output
  检查；可选 Judge hook 默认关闭。
- 通过的 package 可经受控 tenant 发布通道发布；失败 fixture 会被拦截。
- Rust CI 通过 `scripts/test_golden.sh` 执行 Agent 计划、Skill Markdown 和
  Action 调用的 golden evals。

对应工作项：[Skill CI 与发布](https://github.com/skaiy/wild_agentos/issues/88)、[黄金评测](https://github.com/skaiy/wild_agentos/issues/89)。

配套 Admin #16 已在独立仓库交付五屏控制平面骨架（Runs、Skills、KB · Ontology、Keys · Models、Isolation）。本路线图仅记录该配套变更，不改变该仓库的范围或实现。

### v0.2.1 — 已完成：本体数据 + 协议

- 可从 CSV 或 JSON Schema 生成 ObjectType / LinkType 草稿；它们按 claims
  隔离，提升前必须由获授权人员审批；
- 入站 MCP 工具目录由已验证的 `IsolationClaims` 过滤；
- 受 gate 的租户 Skill 可显式发布为由 claims 授权的 MCP 工具（默认拒绝；
  内核 Skill 被排除）；
- 薄型出站 A2A adapter 由默认关闭的 feature flag 保护；它以尽力而为方式
  工作，不提供入站 A2A server，也不重写本地任务生命周期。详见[出站 A2A
  适配器](19-a2a-outbound.md)。

对应工作项：[对象模型草稿](https://github.com/skaiy/wild_agentos/issues/90)、[MCP 目录](https://github.com/skaiy/wild_agentos/issues/91)、[Skill-as-MCP](https://github.com/skaiy/wild_agentos/issues/92)、[A2A adapter](https://github.com/skaiy/wild_agentos/issues/93)。

### [v0.2.2 Artifacts + Sandbox + Bench](https://github.com/skaiy/wild_agentos/milestone/5)

- claims 作用域的 coding artifacts store；
- 外部计算沙箱适配器（OpenHands/E2B 风格挂载），内核只接收结构化结果；
- 弱算力环境可复现基准，不编造速度提升。

对应工作项：[制品库](https://github.com/skaiy/wild_agentos/issues/94)、[外挂计算沙箱](https://github.com/skaiy/wild_agentos/issues/95)、[可复现基准](https://github.com/skaiy/wild_agentos/issues/96)。

### [v0.3.0 Markets + IdP + Emergent](https://github.com/skaiy/wild_agentos/milestone/6)

- 版本化 Function / Skill market；
- OIDC / IdP；claims 仍在认证边界 mint；
- 具备 gate 的 emergent tool pipeline；
- 有限 OWL / rules，作为可选功能且默认关闭。

对应工作项：[市场](https://github.com/skaiy/wild_agentos/issues/97)、[OIDC/IdP](https://github.com/skaiy/wild_agentos/issues/98)、[emergent tools](https://github.com/skaiy/wild_agentos/issues/99)、[有限 OWL/rules](https://github.com/skaiy/wild_agentos/issues/100)。

## 明确非目标

1. 不做第四类“微内核 OS”或裸机 OS。
2. 不追求 100% 复刻任何专有平台。
3. 不混入 其他业务/产品仓库或产品边界。
4. 不用 Nebula/Cypher 取代 Oxigraph/SPARQL。
5. 不把“已 mint 名称”宣称成“已完成历史数据迁移”。
