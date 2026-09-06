# 本体 KE 黄金评测冻结策略

> English: [22-ontology-ke-golden-freeze-policy.md](22-ontology-ke-golden-freeze-policy.md)

`tests/fixtures/ontology_ke_golden/` 是冻结的内核 Graph Engineering
计分卡。它评测本体约束 extraction、canonicalization，以及
[#140](https://github.com/skaiy/wild_agentos/issues/140) 引入的 claims-scoped
`KgQualityGate`。它不是运行时数据；extractor、optimizer、emergent loop 和部署任务
都绝不能拥有写入权限。

## 冻结内容

- `golden.json` 包含源文本、上游 candidate、已 promote 的微型 ontology、预期 staging
  decision 与确定性 `ASK` 结果。
- `SHA256SUMS` 固定 fixture 字节；CI 会先运行
  `scripts/check_ontology_ke_golden.sh`，再运行 Rust suite。
- 必需 case ID 与最小 case 数量防止困难的 negative case 被静默删除。

runtime 与评测测试都以 `include_str!` 只读加载 fixture；不存在 fixture 写入路径。

## 指标与合并规则

`golden_ontology_ke_extract_canonicalize_and_gate_multi_metric` 会对以下全部指标
执行门禁；禁止把单一 accuracy 当作合并条件：

1. **ontology-conformance rate：** 预期 canonical staging decision 必须匹配；
2. **invalid-rejection rate：** 未 promote type 与不合法的 link domain 必须被拒绝；
3. **illegal production writes：** extract/canonicalize/gate 阶段必须严格为零。

quality gate 只在 claims-minted staging graph 上运行。确定性的 `ASK` anchor 先于可选
Judge evidence 执行，且本 suite 的任何结果都不授予 production write。

## 如何变更计分卡

只允许为修复可证明的缺陷，或增加更困难、以 source 为依据的 case 而变更 fixture；不得
删除困难 case 或降低阈值。变更此目录的 pull request 必须：

1. 在 PR body 勾选 **Ontology KE golden fixture change reviewed**；
2. 填写具体的 `Ontology KE golden fixture audit:` 说明，描述 source-grounded 原因、
   受影响 case 与 metric 影响；
3. 获得两名 maintainer 审阅，其中一名为 ontology/kernel governance owner。

没有 acknowledgement 与说明的 fixture diff 会被 CI 拒绝。maintainer review 是刻意保留的
人工控制：能同时改 fixture 与 checksum 的贡献者不得自行授权简化计分卡。

## 慢速 measurement-decay 审计

至少每周一次，或在改变 extraction/canonicalization/gate 行为的发布前，运行：

```sh
./scripts/check_ontology_ke_golden.sh
cargo test --test ontology_ke_golden --verbose
```

将 fixture 与 `main` 对比，检查 case 数量和必需的 negative case，并在 PR audit 字段记录
所有 fixture 变更。审阅必须判断 source provenance、ontology conformance、rejection
behavior 与 zero-production-write invariant 是否仍在被衡量。如果某个指标已退化为容易
fixture 的代理，应增加困难 case，或通过同样的审阅流程修订策略。
