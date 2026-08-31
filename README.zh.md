# 如野智能体操作系统 Wild AgentOS
<div align="center">

<img src="assets/logo_transparent.png" width="120" alt="Wild AgentOS Logo" />

**工业级 AI 智能体操作系统 · Rust 构建**  [![Star on GitHub](https://img.shields.io/github/stars/skaiy/wild_agentos?style=flat)](https://github.com/skaiy/wild_agentos)


[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![gRPC](https://img.shields.io/badge/gRPC-Protocol-green.svg)](https://grpc.io/)
[![Knowledge Graph](https://img.shields.io/badge/Knowledge%20Graph-Oxigraph-purple.svg)](https://oxigraph.org/)
[![Release](https://img.shields.io/badge/release-v0.1.5-blue)](https://github.com/skaiy/wild_agentos/releases)

---

[**中文**] · [**English**](README.md) · [**设计细节 →**](docs/13-DESIGN_DETAIL.zh.md)

<img src="assets/github-readme.png" alt="Wild AgentOS" width="100%" />

</div>

---

## 🎉 版本发布历史与说明

欢迎查阅 **Wild AgentOS** 的版本演进历史。平台提供生产级安全网关、多租户工作空间数据隔离，以及先进的智能体认知操作系统内核。

| 版本 | 发布日期 | 核心升级与融合特性 |
|------|----------|-------------------|
| **v0.1.5** | **2026-08-18** | **认知因果引擎与图治理升级**<br>• **Causal Engine 因果引擎**：新增独立因果分析子系统 `CausalEngine`、`FusionEngine`、`CausalStore` 和类型化的 `CausalFactor`。支持智能体运行的因果推理与多因素融合分析，用于根因识别、故障链传播以及决策因果图谱构建。<br>• **统一图存储后端 (Unified Graph)**：将原零散图读写接口收敛为单一、高性能的 `GraphBackend`。<br>• **图特征计算与相似度**：支持对认知快照进行度中心性、PageRank 等图特征向量计算，并比对计算认知相似度。<br>• **快照时间线 (Snapshot Timeline)**：会话级定点历史恢复与基于 diff 的版本差异回滚。<br>• **技能中心 CRUD 与系统守卫**：支持应用级（`skill://`）技能的新建、详情解析（含 Input/Output Schema）、编辑与删除；对系统级（`iri://`）内置技能执行严格的 **403 只读保护**。 |
| **v0.1.4** | **2026-07-06** | **统一模型注册中心与向量热桥接**<br>• **三合一收敛**：原「大模型网关」「向量/Embedding」「模型资源」Tab 合并收敛为单一的**模型注册中心**，实现统一管理。<br>• **自动拉取型号**：支持调用外部 `/v1/models` 并根据名称关键词自动对文本、VL、向量等型号进行模态预判。<br>• **向量服务热桥接**：在模型页面直接将型号「设为生效向量」，实现免重启热切换向量库并自动后台重建索引。 |
| **v0.1.3** | **2026-07-06** | **多模态 (VL) 支持与 Agent 多模型能力挂载**<br>• **多模态智能网关**：支持对 `ChatContent` 中包含的文本与图片（Base64/URL）进行解析，自动路由至多模态大模型。<br>• **能力槽多模型挂载**：支持在 Agent 中挂载不同的型号到对应的功能槽（例如 chat 槽挂载 DeepSeek，vision 槽挂载 Gemini）。 |
| **v0.1.2** | **2026-07-05** | **知识库多源摄取与统一知识包**<br>• **两阶段数据摄取**：支持向量库多文件上传自动分块与图谱库结构化三元组（CSV/N-Triples）上传到隔离命名图。<br>• **统一知识包挂载**：下线原单值图谱绑定，统一升级为多知识包挂载（`knowledge_pack_ids`），存量数据自动幂等迁移。 |
| **v0.1.1** | **2026-07-04** | **调用方 API 密钥治理中心与 Agent 一键发布**<br>• **密钥治理中心**：上线「调用方 Client + 密钥」管理面，支持配额限制、安全审计、Token 限制 and 权限 scope 控制。<br>• **OpenAI 兼容网关**：提供一键发布 Agent 按钮，直接生成兼容 OpenAI 格式的调用 URL (`/v1/chat/completions`) 与命令行示例，支持 SSE 流式返回。 |
| **v0.1.0** | **2026-07-01** | **首个版本发布 —— 核心系统引擎与 Hyperspace 向量存储**<br>• **HyperspaceEngine 向量引擎**：嵌入式 HNSW 向量数据库，支持 WAL 与 Poincaré/Lorentz 多维空间度量。<br>• **技能图谱与四层记忆**：5W2H 语义技能图谱，带 MESI 一致性协议的 L0-L3 分层记忆缓存。<br>• **工作区监控器**：实时文件监控与 10 种主动感知触发器。 |

---

## 什么是 Wild AgentOS？

一个 **基于 Rust 构建的 AI 智能体操作系统**，通过 PDCA 循环编排多智能体，实现协调、可审计和自我改进的系统。

> "我们不只构建智能体；我们构建**驾驭集体智能的基础设施**。"

### 核心技术栈

| 层级 | 技术 | 职责 |
|------|------|------|
| **核心编排** (Rust) | `PDCA 循环` · `5W2H 本体` · `事件总线` | 智能体编排与生命周期管理 |
| **技能图谱** | `RDF` · `6 种链接类型` · `18 模块` | 动态认知网络 |
| **记忆系统** | `L0 redb` · `L1 Session` · `L2 Blackboard` · `L3 Projection` · `MESI 一致性` | 带预取的分层记忆 |
| **知识图谱** | `Oxigraph RDF` · `SPARQL 1.1` · `代码 AST` · `命名图` | 跨子系统统一存储 |
| **HyperspaceEngine** | `HNSW ANN` · `WAL` · `Poincaré/Cosine/Euclidean` · `混合搜索` | 嵌入式向量嵌入引擎 |
| **Wild Code TUI** | `ratatui` · `crossterm` · `MCP` · `断点恢复` | 终端 AI 编程助手 |
| **数据总线** | `JSON-LD 1.1` · `@id/@type/@context` · `命名图` | 通用互操作层 |
| **网关** | `gRPC` · `HTTP (兼容 OpenAI)` · `MCP` | 生产级接口 |
| **感知引擎** | `10 种触发器` · `异常去重` · `5W2H 约束检查` | 主动监控 |
| **智能体工作流** | `PA/DA/CA` · `工具系统` · `检查点` · `追踪操作` | 多智能体执行 |

---

## 🔧 亮点速览

### 1. HyperspaceEngine — 嵌入式向量引擎
生产级空间记忆引擎，支持 **运行时可选度量空间**（Poincaré、Cosine、Euclidean、Lorentz）。内置 **HNSW 近似最近邻搜索**、CRC32 校验的**预写日志（WAL）**（3 种同步模式）、**切线空间剪枝**（优化 Poincaré 球搜索）、JSON-LD 元数据索引（RoaringBitmap 位图过滤器）以及双空间**混合搜索**（文本 × 结构）。独立 crate，零外部向量数据库依赖。

### 2. 技能图谱认知网络
动态内存认知网络，**6 种语义链接类型**（前置依赖、组合、关联、替代、扩展、泛化）。核心能力包括：基于图谱拓扑的 **Poincaré 结构嵌入**（前置依赖深度 + 标签域指纹）；**超图组合**——一等公民 `Hyperedge` 与 `CompositionType`（顺序、并行、条件、可选、回退）；**图算法**（PageRank、介数中心性、标签传播社区发现、DFS 前置链、Tarjan SCC 环检测）；**因果故障分析**与根因推断；**形式化不变式验证**（6 项检查：无环、链接可达、组合可达、无废弃前置依赖、5W2H 有效、安全等级有效）；**时序版本管理**与快照回滚。

### 3. 泛化 PDCA — 7 级自适应执行
通过 5W2H 元数据动态选择 7 级复杂度（L0 即时 → L5 递归 → L6 应急）。同一引擎同时处理即时查询与数周工程项目——无需僵硬的固定流程。**PA/DA/CA 智能体角色**，基于模板的提示词构建。

### 4. 语义技能发现引擎
`SkillDiscoveryEngine` 包装 `HyperspaceStore` 实现基于向量的语义技能搜索。`suggest_links()` 从 Jaccard 标签重叠优雅降级到余弦相似度搜索。内置 BFS 路径发现（`find_skill_chain()`）、组合树构建（`get_skill_tree()`）和冲突检测。

### 5. CPU 缓存记忆 — 4 层结构 + MESI 一致性
业界首创将 CPU 缓存一致性协议应用于多智能体记忆系统。**L0** redb 磁盘 KV + HyperspaceEngine 向量 → **L1** 会话上下文 → **L2** Oxigraph RDF + Blackboard → **L3** SPARQL 投影缓存。智能预取引擎降低 90% 感知延迟。解决上下文爆炸与并发智能体间的共享内存不一致问题。

### 6. JSON-LD 通用数据总线 — W3C 标准互操作
`@context` 鸭子类型消除技能间的字段名冲突。`@id` 实现零成本跨智能体实体合并。`@graph` 命名图支持跨子系统无锁并行写入。将互操作难题变为即插即用。

### 7. 自进化技能图谱 — 自主学习
AA 智能体每次任务完成后自动创建**知识片段**和新语义链接。`/learn`/`/reduce` 机制实现自主技能获取与归并。`BootstrapEngine` 从文件系统摄取 Markdown 格式技能。

### 8. 通用知识图谱 — 统一认知骨干
所有子系统（技能、记忆、任务、代码知识）共享同一 **Oxigraph RDF 存储**，通过命名图隔离，支持跨子系统 SPARQL 联合查询。tree-sitter 解析的代码 AST 自动转为 RDF 三元组。`SkillGraphStore` **双向 SPARQL 同步**确保认知图与语义存储实时一致。

### 9. 5W2H 维度级审计 — 精准回滚
CA 独立审计 7 个维度。What/Why 失败 → 重新分析。How/Where 失败 → 重新规划。When/HowMuch 失败 → 条件通过。告别黑盒"通过/不通过"——精确定位问题根因。

### 10. 主动感知引擎 — 防患于未然
10 种执行触发器，60 秒异常去重窗口。监控截止时间违规、预算超支（>80% Token）、角色不匹配、环境冲突。**工作区监控器**实时检测文件创建/修改/删除。必要时自动升级到人工处理。

### 11. 微工具系统 — 驾驭大型输出
结果 >8KB 时自动生成可对话的微工具（如"search_in_results"）。将 50KB+ 的笨重输出转变为 LLM 上下文中可交互、可查询的产物。

### 12. MCP 集成 — 一个协议连接一切
标准 **Model Context Protocol** 连接 GitHub、Slack、Jira 等任意 MCP 兼容服务器。运行时动态发现工具。支持 HTTP SSE 和 stdio 两种传输模式，通过可重复 `--mcp-server` CLI 标志配置。

### 13. 检查点与恢复 — 崩溃不丢上下文
关键执行点保存会话快照，崩溃后完整恢复上下文零丢失。`--resume <task_iri>` 和 `--list-checkpoints` 命令提供显式会话管理。支持数小时/数天的长任务执行及事后回放调试。

### 14. Center + Edge 联邦 — 本地自治，全局编排
Go Center 负责工作流编排（Temporal）、项目管理、智能体注册。Rust Edge 运行本地 LLM 执行与 Docker 沙箱。VS Code 插件提供实时开发者感知。无单点故障。

---

## 🖥️ Wild Code — 终端 AI 编程助手

**Wild Code** 是一款基于终端的 AI 编程助手（`ratatui` TUI），将如野智能体操作系统 Wild AgentOS的知识图谱与智能体编排能力直接带入命令行——无需 IDE。

**功能特性：**
- 交互式 TUI，支持 **Markdown 渲染**（`tui-markdown`）和 **Mermaid 图表**
- **MCP 服务器集成**，通过 `--mcp-server` 和 `--mcp-server-stdio` 标志
- **检查点恢复**：`--resume <task_iri>` 和 `--list-checkpoints`
- **多模型后端**：DeepSeek、兼容 OpenAI 的 API
- **PDCA 工作流执行**：规划/执行/检查/行动完整周期
- **可配置**：工作区、最大迭代次数、最大 PDCA 周期、日志级别

![Wild Code 演示](assets/screenshot.gif)

![知识图谱实战](assets/wild_code_kg.JPG)
*知识图谱可视化——实时实体关系、代码结构理解、基于 Oxigraph RDF 的跨子系统感知*

![编程任务完成](assets/wild_code.JPG)
*任务完成界面——AI 智能体成功分析并解决编程任务，全程可追溯*

---

## 🚀 快速开始

### 直接下载 — Wild Code

无需任何依赖。下载、解压、直接运行：

| 平台 | 下载 |
|------|------|
| Linux (x86_64, musl) | [`wildcode-x86_64-unknown-linux-musl.tar.gz`](https://github.com/skaiy/wild_agentos/releases) (~15 MB) |
| Linux (aarch64, musl) | [`wildcode-aarch64-unknown-linux-musl.tar.gz`](https://github.com/skaiy/wild_agentos/releases) (~14 MB) |
| macOS (Apple Silicon) | [`wildcode-aarch64-apple-darwin.tar.gz`](https://github.com/skaiy/wild_agentos/releases) (~13 MB) |
| Windows (x86_64) | [`wildcode-x86_64-pc-windows-msvc.zip`](https://github.com/skaiy/wild_agentos/releases) (~12 MB) |

```bash
# Linux / macOS
tar xzf wildcode-*.tar.gz
./wildcode --help

# Windows (PowerShell)
Expand-Archive wildcode-x86_64-pc-windows-msvc.zip .
.\wildcode.exe --help
```

> 所有 Linux 版本均为**全静态链接**（musl），无需任何运行时依赖。

设置 API 密钥后即可使用：

```bash
export DEEPSEEK_API_KEY="sk-..."        # Linux / macOS
# 或
set DEEPSEEK_API_KEY="sk-..."           # Windows (cmd)
# 或
$env:DEEPSEEK_API_KEY="sk-..."          # Windows (PowerShell)

# 也可使用任意兼容 OpenAI 的服务：
export AGENT_OS_GATEWAY_API_KEY="sk-..."
export AGENT_OS_GATEWAY_BASE_URL="https://your-endpoint/v1"

# Web search 工具（基于 Exa 搜索引擎）：
# 从 https://exa.ai/docs/reference/team-management/get-api-key 免费获取 API Key
# 未设置时自动降级为 DuckDuckGo 模式，但国内 DuckDuckGo 不好用，不推荐国内使用
export EXA_API_KEY="your-exa-api-key"

# 启动交互式会话
./wildcode

# 或单次执行任务
./wildcode "解释 Rust 的借用检查器工作原理"

# 附接 MCP 服务器
./wildcode --mcp-server chrome=http://localhost:3000/sse

# 从检查点恢复
./wildcode --resume task:abc123
```

### 从源码构建

```bash
git clone https://github.com/skaiy/wild_agentos.git
cd Wild_AgentOS

# 编译 wildcode 二进制（release，约 51 MB）
cargo build -p wild-code-cli --release
./target/release/wildcode --help
```

---

## 🗺️ 路线图

**v0.1.x 发布系列**（稳定化）：
- Linux/macOS/Windows 多平台二进制分发
- Linux musl 全静态编译（零依赖）
- MCP 工具生态扩展与文档完善
- 检查点恢复功能的测试与打磨

**v0.2.x 发布系列**（规划中）：
- 原生 Web 仪表盘（智能体监控与任务管理）
- Python/TypeScript SDK 简化集成
- 技能市场原型与社区插件注册表
- 多模型路由与成本感知调度

**v0.3.x+ 发布系列**（未来）：
- Kubernetes 部署算子，生产级弹性伸缩
- 跨 Edge 节点的分布式智能体网格
- 多模态智能体支持（视觉、音频）
- 多轮对话记忆压缩

---

## 📊 性能目标

| 操作 | 延迟 | 吞吐量 |
|------|------|--------|
| L2 节点写入 (Oxigraph) | ~2ms | 500 ops/sec |
| L3 SPARQL 投影 | ~15ms | 66 ops/sec |
| L0 redb KV 读取 | ~1ms | 1000 ops/sec |
| Hyperspace HNSW 搜索（万级向量） | ~1ms | 1000 qps |
| Poincaré 嵌入（4 维） | ~50µs | — |
| Agent ReAct 单轮 | 1-5s | 0.2-1 turns/sec |
| 空闲内存 | ~200MB | 随任务扩展 |

---

## 📚 文档

- **记忆系统** → [`docs/03-memory-system.md`](docs/03-memory-system.md)（L0 redb · HyperspaceEngine · Oxigraph SPARQL）
- **设计细节** → [`docs/13-DESIGN_DETAIL.zh.md`](docs/13-DESIGN_DETAIL.zh.md) · [`docs/13-DESIGN_DETAIL.md`](docs/13-DESIGN_DETAIL.md) (English)
- **核心设计理念** → [`docs/CORE_DESIGN_PHILOSOPHY.zh.md`](docs/CORE_DESIGN_PHILOSOPHY.zh.md) · [`docs/CORE_DESIGN_PHILOSOPHY.md`](docs/CORE_DESIGN_PHILOSOPHY.md) (English)
- **gRPC Proto** → [`proto/pdca_core.proto`](proto/pdca_core.proto)

---

## 🤝 参与贡献

欢迎社区贡献！

- **🐛 报告 Bug**：[GitHub Issues](https://github.com/skaiy/wild_agentos/issues)
- **💡 提出想法**：[GitHub Discussions](https://github.com/skaiy/wild_agentos/discussions)
- **🔀 提交 PR**：Fork → 功能分支 → PR 至 `main`

```bash
git checkout -b feat/my-feature
# 进行你的修改
cargo fmt && cargo clippy  # 保持代码整洁
cargo test                 # 确保一切正常
git commit -am '添加我的功能'
git push origin feat/my-feature
```

所有贡献者应遵守我们的[行为准则](docs/CODE_OF_CONDUCT.zh.md)。

---

## 📄 许可证

Wild AgentOS 采用双许可模式：

- **社区版** —— [GNU AGPL v3.0](LICENSE)（另见 [NOTICE](NOTICE)）。若通过网络对外提供修改后的版本，AGPLv3 第 13 条要求你向使用者提供完整的对应源代码。
- **商业版** —— 无法遵守 AGPLv3 的商业使用需单独获取商业授权，详见 [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)。

商业授权咨询请联系 **diaoguoliang@gmail.com**。

参与贡献需签署[贡献者许可协议（CLA）](CLA.md)，首次提交 PR 时由 CLA Assistant 自动引导完成。

版权所有 (c) 2026 skaiy (diaoguoliang@gmail.com)。
