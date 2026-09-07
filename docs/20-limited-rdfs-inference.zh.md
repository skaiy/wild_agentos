> *本文是 [20-limited-rdfs-inference.md](20-limited-rdfs-inference.md) 的中文翻译。*

---

# 20. 有限 RDFS 推理

Issue #100 为 claims-scoped 知识图谱读取加入一个有意保持很小、显式启用的推理片段。
图接口仍是 **Oxigraph 上的 SPARQL 1.1**；不引入 Nebula、Cypher、第二套图后端或 OWL
DL 推理器。

## 启用与行为

除非将 `AGENTOS_LIMITED_RDFS_INFERENCE` 设为 `1` 或 `true`（大小写不敏感），否则功能
保持关闭：

```bash
AGENTOS_LIMITED_RDFS_INFERENCE=1
```

关闭时，图读取与此前完全一致：直接查询持久化 Oxigraph 命名图，不增加三元组，也不改变
结果。

开启时，每次 claims-scoped 图查询都会在仅包含当前调用方 mint 图的临时 Oxigraph store
上执行。临时 store 只增加两类有限 RDFS 结论：

1. `rdfs:subClassOf` 的传递闭包（不产生自反 self edge）；以及
2. 沿该子类层级继承的 `rdf:type` 成员关系。

查询结束后临时 store 即被丢弃。本版本只提供**查询时扩展**：不会向源图持久化或
materialize 推导三元组。

## 明确边界

这不是完整 RDFS、OWL RL、OWL DL 或企业规则推理引擎。它尤其不会执行自定义规则文件、
`rdfs:subPropertyOf`、domain/range 规则、OWL 等价/限制、`sameAs`、属性特征、不一致性
检测或非单调规则。

claims-scoped 查询仍拒绝调用方提供的 `GRAPH` 子句。推理视图使用同一个 claims-minted
命名图，保持 [17-isolation-contract.zh.md](17-isolation-contract.zh.md) 中的隔离契约。

## 性能与运维

查询时扩展会复制当前命名图，并在执行目标查询前运行两条 SPARQL update。因此成本随图的
quad 数量和子类可达性增长；稠密或循环层级会产生大量派生 type facts。它只适合有界、
规模较小的本体图。对延迟敏感或大型图应保持关闭；服务启用前应使用代表性数据测量。

因为推导事实是临时的，源图更新会在下一次查询生效，不需要清理、迁移、持久化缓存或
跨租户 read-through。
