# 如野智能体操作系统 Wild AgentOS
<div align="center">

<img src="assets/logo_transparent.png" width="120" alt="Wild AgentOS Logo" />

**面向受治理多智能体协作的操作系统**

[![Star on GitHub](https://img.shields.io/github/stars/skaiy/wild_agentos?style=flat)](https://github.com/skaiy/wild_agentos)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![gRPC](https://img.shields.io/badge/gRPC-Protocol-green.svg)](https://grpc.io/)
[![Knowledge Graph](https://img.shields.io/badge/Knowledge%20Graph-Oxigraph-purple.svg)](https://oxigraph.org/)
[![Release](https://img.shields.io/badge/release-v0.7.0-blue)](https://github.com/skaiy/wild_agentos/releases)

---

[**中文**](README.zh.md) · [**English**](README.md) · [**设计细节 →**](docs/13-DESIGN_DETAIL.zh.md)

<img src="assets/github-readme.png" alt="Wild AgentOS" width="100%" />

</div>

---

## 什么是 Wild AgentOS？

Wild AgentOS 是一个面向**多智能体协作治理**的操作系统，以 Rust（系统编程语言）构建。它用
PDCA（计划、执行、检查、改进）组织智能体工作，帮助团队共享知识，并保留工作过程和
决策的审计记录。

系统以租户和项目为安全边界。经过验证的 JWT（身份令牌）携带租户/项目隔离凭证
`IsolationClaims`，系统据此划定调用方可用的知识图谱、向量、文件和工作记录存储范围。
缺少或无效凭证即拒绝访问，调用方也不能指定其他租户的存储范围。此保证适用于已接入
隔离凭证的接口，并不代表每个历史 HTTP 接口都已隔离。系统划定新的存储名称，不等于
迁移历史数据；详见[隔离契约](docs/17-isolation-contract.zh.md)。

领域对话应放在客户端或可选示例包（如 `ev-repair`）中，核心中间件保持通用。

## 你能获得什么

- **协同执行：** 基于 Rust 的 PDCA 工作流、任务服务和事件审计，支持多个智能体围绕
  计划、执行、检查和改进协作。
- **共享知识与检索：** 使用 Oxigraph 知识图谱（结构化知识存储）和 SPARQL 查询语言，
  并提供内嵌向量检索，帮助找到相关信息。
- **受控的知识变更：** 可从 CSV、JSON Schema、OpenAPI 和 SQL DDL 生成草稿，经过
  检查、写入暂存区和人工审核后，才可按审批流程写入生产数据。草稿不会自动成为正式定义。
- **人工把关与可追溯性：** 高风险或待审批的操作必须由人决定；低风险操作也只能在其
  策略允许且通过护栏时自动提交。系统记录检查、审核、事件审计和回读证据。实体消歧
  （判断两条记录是否指向同一实体）只会给出暂存建议，绝不自动合并。
- **身份与集成：** 生产环境使用 OIDC/JWKS 进行身份校验；本地开发使用独立的开发用签名
  模式。gRPC 用于服务间通信；MCP（Model Context Protocol，模型上下文协议）可将
  经明确发布、按租户隔离的 Skill 作为工具提供。出站 A2A 适配器可选、默认关闭，且仅
  提供尽力而为的能力。
- **受治理的软件包：** Logic 和 Skill 软件包采用不可变版本、按租户可见，并记录安装、
  升级和回滚。正式上线需通过既定检查、审核证据和人工审批。
- **事实溯源：** 服务端 mint 的 PROV-O 伴生图记录暂存事实与治理决策的来源；按作用域
  提供的只读 API 返回链接或统一的“未找到”；扫描超预算时 fail-closed，不返回残缺结果。
- **示例知识包：** CI 门控校验文件系统知识包（锚点、SHACL、冻结 golden I/O、审计样例）。
  虚构的 `ev-repair-fault-kb` 展示包用于跑通门控，不写入生产数据。
- **服务端托管的智能体定义：** 按作用域的智能体可保存不可变定义修订（提示词、模型、
  可选输出结构）；在启用执行时，调用可在创建时固定某一修订版本。
- **持久输入与计量：** 制品库接受不可变 `input_snapshot` JSON，支持按 kind 过滤列表，
  并加强明文密钥检测（含 JSON 内伪装凭证）。生产调用路径在配置了成本来源时记录 token
  与费用；否则 fail-closed，不会在没有用量的情况下记为成功。
- **租户记忆边界：** 任务收尾写入该次运行自己的租户 L0，单进程有句柄上限与注册表上界；
  已完成任务记录在重启后仍可读取。
- **加固的服务边界：** gRPC 与 HTTP 一样要求已验证 JWT，并阻断会把已配置 LLM 网关
  API 密钥发往其他 `base_url` 的改向。MCP Skill 暴露、发布门、核心写路由、事件入站
  白名单与网关 SSE 路径均遵守租户/项目边界。

## v0.6：在线语料任务，始终由人把关

v0.6.0 新增了在同一租户/项目边界内运行的、已认证的在线语料任务。监视器默认开启，
但只检查已配置的数据源版本，并为每个新版本向队列加入一个任务。部署可关闭这项轮询
和入队，而不会删除已有任务、游标、审核或审计记录。监视器不会抓取来源 URL、计算内容
变化，也不会自行运行任务。

已认证的人员或服务可手动运行队列中的任务，并提供文本、提取方法、提取候选项和质量检查请求。
任务会规范化候选项，将相应证据放入暂存区，执行检查，生成一条待审批的实体消歧建议，然后
等待审核。

系统保留有上限的来源追溯和运行可观测记录，包括来源版本、内容摘要、检查结果、审核
状态、重试、容量压力和任务状态。临时的辅助服务故障最多重试三次；身份、校验或策略
失败会直接停止任务。CI 已验证失败、无效、跨边界或未认证的路径不能写入生产数据。

最重要的是，这些任务**不会**自动合并实体、将本体正式上线，或将暂存结果写入生产数据。
这些都必须经过明确的人工治理决策，并保留审计记录。

## 已交付能力

| 领域 | 业务说明 |
|---|---|
| **PDCA 协同** | 以 Rust 实现的 PDCA 工作流、任务服务和事件审计，支持多智能体按计划、执行、检查、改进协作。 |
| **租户/项目隔离** | 已验证的 JWT `IsolationClaims` 划定知识图谱、向量、文件和工作记录的存储范围；缺少或无效凭证即拒绝访问。 |
| **身份认证** | 生产环境使用 OIDC/JWKS 身份校验；本地开发使用独立的开发用签名模式。 |
| **知识工程** | 从 CSV、JSON Schema、OpenAPI 和 SQL DDL 生成按范围隔离的类型草稿；支持受限提取、规范化、质量检查、审核、带锚点的生产写入、冻结参考评测和只读健康证据。 |
| **人工审核与审计** | 低风险操作仅在策略允许且通过护栏时自动提交；高风险或待审批操作使用审批、合并/丢弃和到期处理。SPARQL `ASK` 检查、审批钩子和 `ACTION_AUDIT` 事件提供审计证据；实体消歧建议绝不自动合并。 |
| **在线语料运行** | 支持按隔离范围创建、查看、取消任务，以及手动暂存执行。默认开启的已配置版本轮询只负责入队，不执行任务；仓库默认配置没有监视器注册项，需由部署添加可信来源。包含重试、容量控制、有限来源追溯和按范围隔离的运行记录。 |
| **软件包与上线** | Logic 和 Skill 软件包版本不可变、按租户可见，并记录安装、升级和回滚。租户 Skill 和新兴工具正式上线需通过检查、参考 SHA 校验、规则审核、审计证据和人工审批。安装软件包尚不会将 Skill 注册到进程全局目录。 |
| **互操作性** | `IsolationClaims` 会过滤入站 MCP 工具目录；经过门控并明确发布的租户 Skill 可作为 MCP 工具。轻量出站 A2A 适配器默认关闭，且仅提供尽力而为的能力。 |
| **知识与检索** | Oxigraph RDF/SPARQL 知识图谱、按隔离范围的数据写入和检索，以及内嵌 Hyperspace HNSW 向量检索。可选的 RDFS 查询时扩展默认关闭，且不会写入推导结果。 |
| **制品与沙箱** | 按隔离范围保存的编程制品元数据与系统划定的文件前缀；外部 `SandboxProvider` HTTP 适配器默认关闭，当前仅定义对接契约，尚未接入智能体/工具运行时。 |
| **事实溯源** | 服务端 mint 的 PROV-O 伴生图与只读 HTTP API；读/写路径扫描有预算，超限 fail-closed。 |
| **知识包门控** | `knowledge-pack-gate` CI 与虚构 `ev-repair-fault-kb` 展示包（锚点、SHACL、golden I/O）。 |
| **智能体定义与调用** | 按作用域的不可变定义修订；启用执行时可固定修订；生产路径记录用量与成本（需配置成本来源）。 |
| **安全加固** | gRPC JWT、网关 `base_url` 改向拦截、MCP 暴露隔离、密钥扫描边界、写范围与事件 allowlist。 |

配套的 Admin control-plane 和 online-job-list surface 已独立交付，不属于本
仓库。

## 版本亮点

| 版本 | 日期 | 业务说明 |
|---|---:|---|
| **main（2026-10）** | — | PROV-O 事实溯源与 fail-closed 扫描预算；知识包 CI 门与 EV 维修展示包；运行时托管智能体定义；`input_snapshot` 制品与上传密钥守卫；租户 L0 配额与可读已完成任务；调用用量/成本计量；gRPC 鉴权与网关改向加固；MCP Skill 暴露隔离及更广安全门控。 |
| **v0.7.0** | 2026-09-13 | 核心对话保持通用，按需启用示例包才会加入领域帮助；团队还能只查看自己范围内的任务、护栏记录和黑板数据，护栏查看会隐藏敏感细节。 |
| **v0.6.1** | 2026-09-09 | 通用 Agent chat 与 completions 默认不再注入特定领域上下文；workload OIDC 现定义用于 `IsolationClaims` 的短期 JWT，且 BFF 不共享 `AGENTOS_JWT_SECRET`。 |
| **v0.6.0** | 2026-09-08 | 已认证的在线语料任务记录、手动暂存执行器、仅入队的监视器、有限的来源追溯和运行记录、重试与容量控制，以及由 CI 验证的生产写入隔离。 |
| **v0.5.0** | 2026-09-07 | 受治理的知识工程流程：暂存提取、质量检查和审核、待审批的实体消歧建议、审计证据、健康报告和兼容性门控的本体设计。 |
| **v0.3.0** | 2026-09-05 | 版本化的 Logic 和 Skill 软件包、生产身份校验、人工门控的新兴工具，以及可选的查询时知识扩展。 |
| **v0.2.0–v0.2.2** | 2026-09-05 | 软件包校验和发布控制、隔离的知识草稿与集成、制品存储、可选的外部沙箱适配器，以及可复现的部署基准。 |
| **v0.1.5–v0.1.8** | 2026-08-18–2026-09-04 | 因果分析、图谱和记忆基础能力、操作审核与审计控制，以及带诊断和迁移工具的租户/项目隔离。 |

完整版本记录请参阅 [changelog](CHANGELOG.md)。

## 治理如何落地

- **知识可解释：** Oxigraph 保存知识图谱，SPARQL 是查询它的语言；不同的知识图谱区域
  用于区分租户/项目数据。
- **变更需要正确的决策：** 候选内容会依据已批准的定义规范化、检查、暂存和审核；正式
  上线或写入生产数据必须经过明确的受治理步骤。
- **证据独立保存：** 确定性检查、冻结的参考用例、审计记录和写入后的回读，确保不会只
  因模型或工作进程声称“已完成”就视为成功。
- **访问范围可控：** 经验证的租户/项目隔离凭证决定存储范围，因此已接入该机制的接口
  不会跨边界写入数据。

## 从源码构建

```bash
git clone https://github.com/skaiy/wild_agentos.git
cd wild_agentos
# 构建前安装 Protocol Buffers 的 protoc compiler。
cargo build --workspace
cargo test --workspace
```

## 本地运行

默认 binary 启动 HTTP/SSE server（端口 `8080`）与 gRPC（`127.0.0.1:50051`）。
gRPC 要求与 HTTP 相同的已验证 JWT（`authorization: Bearer`）。监听其他地址请设置
`api.grpc_addr` 或 `AGENT_OS_API_GRPC_ADDR`；该覆盖是显式的。
Prometheus 指标通过该 HTTP 口的 `GET /metrics` 抓取（不是 `api.metrics_port` /
9090——该端口当前没有监听）。在 checkout 根目录使用提供的 `config.yaml` 启动：

```bash
cargo run --bin wild-agent-os-core
```

使用 LLM-backed feature 前需配置 LLM gateway。仓库内的 config 保留空的
gateway credential；deployment credential 应始终存放在 version control
之外。`gateway.api_key`（或按 model 解析到的 provider key）为空/仅空白时会在出网前短路，
不发起任何 LLM HTTP，也不重试。生产环境请设置 `AGENTOS_ENV=production`，并配置[隔离契约](docs/17-isolation-contract.zh.md)
所述必需 OIDC/JWKS environment value。server 会拒绝 production HS256
configuration。

## 文档

- [演进路线图](docs/18-evolution-roadmap.zh.md) — 已交付里程碑、边界与明确非目标
- [隔离契约](docs/17-isolation-contract.zh.md) — verified claims、fail-closed
  storage target 与 historical-key status
- [隔离矩阵](docs/17-isolation-matrix.zh.md) — 经 CI 验证的 isolation behavior
- [本体 KE 流水线](docs/21-ontology-knowledge-engineering-pipeline.zh.md) —
  online corpus job/watcher 与 KE governance boundary
- [本体 Action 数据沙箱](docs/15-ontology-action-sandbox.zh.md) — HITL staging、
  guardrail 与 audit
- [本体 KE Golden Freeze Policy](docs/22-ontology-ke-golden-freeze-policy.zh.md)
- [本体问责](docs/27-ontology-accountability.zh.md) — PROV-O 伴生图、只读 API 与扫描预算
- [EV 维修示例知识包](docs/24-ev-repair-example-pack.zh.md) — 虚构展示包、SHACL 与包级 CI 门
- [Invocations API](docs/29-invocations-api.zh.md) — 持久调用、用量/成本与定义修订固定（部分实现，见文档状态）
- [Admin control-plane API matrix](docs/23-admin-control-plane-api-matrix.zh.md) —
  Admin 与内核 API 的当前覆盖范围及访问边界
- [Outbound A2A adapter](docs/19-a2a-outbound.md)
- [Private-deployment benchmark](docs/19-private-deploy-benchmark.md)
- [设计细节](docs/13-DESIGN_DETAIL.zh.md)

## 参与贡献

- 在 [GitHub Issues](https://github.com/skaiy/wild_agentos/issues) 报告问题。
- 在 [GitHub Discussions](https://github.com/skaiy/wild_agentos/discussions) 提出建议。
- 向 `main` 提交 pull request。

提交前请运行相关检查：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

## 许可证

Wild AgentOS 采用双许可：

- **Community Edition** — [GNU AGPL v3.0](LICENSE)（参见 [NOTICE](NOTICE)）。
- **Commercial Edition** — 无法遵守 AGPLv3 的商业使用需要单独获取商业授权；
  详见 [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)。

商业授权请联系 **diaoguoliang@gmail.com**。参与贡献需签署
[Contributor License Agreement](CLA.md)。

版权所有 (c) 2026 skaiy (diaoguoliang@gmail.com)。
