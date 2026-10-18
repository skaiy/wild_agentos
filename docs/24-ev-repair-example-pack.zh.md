# 24. EV 维修示例知识包

> *English version: [24-ev-repair-example-pack.md](24-ev-repair-example-pack.md).*

`ev-repair-fault-kb` 示例包将可选的 `ev-repair` 运行时资产从「仅 prompt」升级为
**文件系统示例包**：本体锚点、SHACL、可分发 Skill 包、冻结 golden I/O 与审计样例。
所有品牌、故障码与租户标识均为**虚构数据**，仅用于展示与 CI。

## 目录结构

| 路径 | 作用 |
|------|------|
| `example-packs/ev-repair-fault-kb/pack.yaml` | 包清单（`agentos.dev/knowledge-pack/v1`） |
| `ontology/anchors.json` | 必需的 `ObjectType` / `LinkType`，与种子 `ev-repair` 本体对齐 |
| `shacl/fault-code-shapes.ttl` | 虚构 `FaultCode` 的 SHACL 形状 |
| `rdf/fictional-sample.ttl` | 门控校验用的示例 RDF |
| `skills/fault-code-lookup/` | 带独立 golden 的租户 Skill 包 |
| `audit/samples.json` | 虚构隔离审计样例（`isolation_verified: true`） |
| `tests/golden-input.json` / `golden-output.json` | 包级门控冻结契约 |

内核内置种子（`ev-repair-fault-kb` / `ev-repair` 别名）仍是运行时绑定入口；本目录是
**可分发、可测试** 的制品。

## 包级 CI 门

CI 任务 `knowledge-pack-gate` 执行：

```bash
./scripts/check_ev_repair_example_pack.sh
```

本地：

```bash
cargo test --lib knowledge_pack_gate_ --verbose
```

门控会校验清单字段、SHA-256 冻结摘要、本体锚点与 `ev_repair_ontology()` 一致、
虚构样本 SHACL、内嵌 Skill 包清单、审计样例结构，以及与冻结 golden 输出一致。

## 可选加载（演示）

`./scripts/load-ev-repair-example-pack.sh` 当前与门控脚本相同，仅做校验。
纳入 #416 一条命令本体治理 demo（导入 → 门控 → Agent 绑定）的编排留作后续。

## Agent 绑定

在具备已验证 tenant/project claims 的 Agent 上，将 `knowledge_pack_ids` 设为
`ev-repair-fault-kb` 或 `ev-repair` 运行时别名。未绑定时代码路径保持领域中立。
