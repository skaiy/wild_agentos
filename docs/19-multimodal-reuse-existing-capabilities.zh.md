# WAO 底座详细设计：多模态（图文）— **复用现有能力优先**

| 项 | 内容 |
|---|---|
| 文档版本 | 0.2.0（修订：强调对齐既有设计与已落地契约） |
| 受众 | 如野 · 运野 · 测野 · 安野 |
| 触发方 | StructCapture 演示线（业务仓只消费契约） |
| 上位设计 | [docs/13-DESIGN_DETAIL.zh.md](https://github.com/skaiy/wild_agentos/blob/main/docs/13-DESIGN_DETAIL.zh.md)（**主依据，勿平行造轮**） |
| 关联 | 姊妹文档《业务层-图文语音联合结构化》；隔离见 `17-isolation-contract`；工具见 `05-tool-system`；摄取见 `16-knowledge-ingest-import-graph` |
| 参考现网 | WAO `core:v0.6.1` · OIDC · Agent `structcapture-organizer` · gateway 临时文本 MiniMax |

---

## 0. 给开源侧的硬性原则（请置顶）

> **不要为 StructCapture「另起一套多模态中台」。**  
> 先吃透并**接线/配置/补文档/补缺口**既有能力；缺口只做最小增量，并写回 DESIGN_DETAIL / 对应分册，而不是在业务仓或旁路里长出第二套语义。

| 原则 | 含义 |
|---|---|
| **复用优先** | 编排、记忆、JSON-LD、5W2H、技能图、工具、gateway、隔离合同——全部沿用 `13-DESIGN_DETAIL` 已定义模型 |
| **契约已有则对齐** | Agent chat 已接受 `images[]`；有图走 `model_mounts.vision`→`chat`；业务对齐该形状，不另发明字段名 |
| **配置先于代码** | 演示阻塞常是「vision 槽未挂 VL / 文档未写清 / 错误语义不清」，不是缺一整条新 pipeline |
| **业务无关** | 领域 prompt、HITL、枚举字典留在 Capture；底座保持通用 |
| **禁止静默丢图** | 若 vision 槽落到非 VL 或上游拒图，须可观测错误或明确降级策略；禁止「收了 images 却当纯文本且不告知」 |

业务旁路 VL 仅作**临时演示兜底**；正道是原版 WAO 上述路径跑通。

---

## 1. 对照 `13-DESIGN_DETAIL`：已有能力如何承接「图文结构化」

下列映射说明：**多模态是既有架构上的输入形态扩展**，不是新子系统。

| DESIGN_DETAIL 能力 | 多模态场景中的用法（复用） | 忌讳 |
|---|---|---|
| **§1 通用化 PDCA + 复杂度分级** | 「整理 shots→结构化」对业务多为 **L0/L1**（单轮/单次 PDCA）；不必默认拉满 L3+ 多智能体。SA 按 5W2H 选模式即可 | 为识图硬编码「永远跑完整 PDCA 团队」 |
| **§2 五层记忆** | 图证据进 L0/L2 用 **@id / IRI**；L1 只留摘要+指针。勿把整段 base64 灌进上下文窗口 | 平行造「图片缓存服务」绕开记忆层级 |
| **§3 JSON-LD 语义总线** | 每张 shot / 每个抽取字段作带 `@id`/`@type` 的节点；Framing 控制投影深度；冲突用命名图溯源 | 另搞一套业务专用非 RDF 证据仓作为「正式」通路 |
| **§4 5W2H** | **Where** = 图像/URL 证据源；**What/Why** = 结构化目标与成功标准；CA 可按维度审计「图文是否对齐」 | 另写一套与 5W2H 无关的「vision meta」标准 |
| **§5 技能图谱** | 识图/OCR/VL 补全视为 **AtomicSkill 或 MCP 封装**；文本抽取用 `AlternativeLink` 降级；经验用 KnowledgeFragment | 在 Core 写死 StructCapture 包装识字逻辑 |
| **§6 主动感知** | CycleTimeout / QualityDegradation 覆盖 VL 慢与胡抽；沿用去重窗口 | 单独做一套 vision 告警总线 |
| **§7 工具 + MCP** | 大图结果走结果路由/微工具；外部 VL 优先经 **已有 LLMClient / MCP**，不另开私协议 | 业务直连绕过 SyscallGate/隔离 |
| **§8–§9 检查点 / 任务队列** | 长耗时识图可异步+checkpoint；批量导入图证据可走 worker | 演示同步路径强行改成重队列（非必须） |
| **§10 模板 + JSON Schema「一次往返双重收获」** | 结构化字段应用 **Schema 校验 → 转 JSON-LD → 写黑板**；与 think/content/summary 模式一致 | 只靠自由文本 JSON 无校验 |
| **§11 组件：Gateway / LLMClient** | 统一走 gateway；有图选 vision 槽 | 业务侧再维护第二套「WAO 外专用 gateway」当作正道 |

**结论给如野**：排期时应写成「**接通 / 配置 /  hardening / 文档化既有 chat+vision+隔离+记忆**」，而不是「从零设计多模态子系统」。

---

## 2. 代码侧已落地契约（请直接消费，勿平行发明）

以下摘自主仓现状（`src/api/http/chat.rs` 等），业务层已按此假设对接：

### 2.1 Agent Chat 请求

```http
POST /api/v1/agents/{agent_id}/chat
Authorization: Bearer <OIDC，须含 verified isolation claims>
Content-Type: application/json

{
  "message": "必填文本",
  "images": ["data:image/jpeg;base64,...", "https://..."]
}
```

- `images` 默认空 = 纯文本，行为与今日一致。  
- 有图时组装 `ChatContent::Parts`（text part + `ChatContent::image`）。  
- **同一路径**校验 isolation claims；无 claims → `verified isolation claims required for chat`（多模态不得绕过）。

### 2.2 模型槽（已有）

有图时解析顺序：`model_mounts["vision"]` → `model_mounts["chat"]` → 旧 `agent.model` → `gateway.default_model()`。  
无图：仅 `chat` 槽。

**演示现网缺口（配置，非架构）**：`structcapture-organizer` 的 **vision 槽未挂真实 VL**（或挂了文本型号），gateway 亦多为文本 MiniMax → 表现为「能传 images，但识图质量/能力不足」。优先 **配模型资源 + mounts**，再谈改代码。

### 2.3 与 DESIGN_DETAIL 的衔接点

| 已有代码行为 | 应对齐的设计章节 |
|---|---|
| 通用 chat、不默认塞车修 system/RAG | §11 业务无关；BizAgent 隔离执行 |
| claims 作用域 Agent | `17-isolation-contract` |
| gateway `chat_with_model` | §11 LLMClient / Gateway |
| （后续）抽取结果入库 | §2–§3 记忆 + JSON-LD；§10 Schema |

---

## 3. 目标与非目标（修订）

### 3.1 目标

- **W1** **文档化并冻结**已有 `images` + vision/chat mounts 契约（OpenAPI/示例/上限/错误）。  
- **W2** 演示 Agent **正确挂载 VL**；文本与视觉模型并存（配置面）。  
- **W3** 失败可区分：未配置 VL、上游拒图、413、超时——业务可降级；**默认禁止静默丢图**。  
- **W4** 与记忆/JSON-LD/5W2H/技能图的衔接说明写清（图证据如何落 @id，而非只当一次性 base64）。  
- **W5** OIDC/隔离与带图请求同路径（已基本具备，补测例）。

### 3.2 非目标

- 不在 WAO 内做 StructCapture 领域 OCR/枚举/HITL。  
- 不新造平行「Vision Service」替代 gateway。  
- 不强制所有 Agent 开 vision。  
- 不把旁路商业 VL SDK 写进 Core。

---

## 4. 缺口清单：在「已有能力」上的最小增量

### P0 — 演示正道（优先配置 + 契约硬化）

| ID | 事项 | 类型 | 验收 |
|---|---|---|---|
| P0-1 | 正式文档：`images`、mounts `vision`/`chat`、上限、错误码；链到 DESIGN_DETAIL §7/§11 | 文档 | curl 示例可复制；与代码一致 |
| P0-2 | 演示/样例：`models.resources` 增加 VL；Agent `model_mounts.vision` 指向它 | **配置** | 带 1 张图 chat，回复能引用图上文字/物体 |
| P0-3 | 当 vision 槽缺失且回退到纯文本模型时：**可观测**（响应头/正文字段/`warning` 或 4xx 策略二选一，产品定一种并写死） | 小改/策略 | 测例证明不「假识图」 |
| P0-4 | 体积上限与 413/业务码（可配置） | 硬化 | 超限可测 |
| P0-5 | OIDC + isolation + 带图：正/负例 CI | 测试 | 无 claims 仍 401 |

### P1 — 对齐记忆与技能（仍复用 DESIGN_DETAIL）

| ID | 事项 | 复用 |
|---|---|---|
| P1-1 | 可选：图证据写入 L2/L0 为 `mem:`/`exec:` 节点，L1 仅 IRI | §2 §3 |
| P1-2 | 审计日志：`image_count`、字节数、选用 model id；**不落 base64** | §6 / 安野 |
| P1-3 | https 图 URL：SSRF 防护（allowlist）；优先预签名 | §7 网络工具同类约束 |
| P1-4 | 结构化输出走 Schema 校验 + JSON-LD（一次往返双重收获） | §10 |
| P1-5 | 可选 AtomicSkill/MCP「vision.describe」供 PDCA Do 调用；chat 直传 images 仍保留 | §5 §7 |

### P2 — 增强

| ID | 事项 |
|---|---|
| P2-1 | 多图与 message 内显式引用对齐（`[image:n]`） |
| P2-2 | 流式 chat + 多模态（若文本流式已有则扩展） |
| P2-3 | VL 超时纳入 ProactiveEngine CycleTimeout 调参样例 |
| P2-4 | 内容安全钩子（文档占位即可） |

---

## 5. 与业务层协作（不变职责，正道定义收紧）

```
业务层（栈野）                              原版 WAO（开源线）
────────────────                            ────────────────
压缩/限张/开关/HITL/枚举
POST message + images[]  ─────────────►  已有 chat 契约
                         ◄─────────────  reply / 可区分错误
临时旁路 VL  ◄── 仅当 P0 未闭环（vision 未挂或策略未定）
```

**关闭业务旁路的条件：**

1. P0-1～P0-5 完成（文档+演示 Agent vision 挂 VL+可观测策略+测例）  
2. 演示 VPS 冒烟：带图 organize 优于纯文本  
3. 运野 env 样例；测野清单；安野确认日志无图内容  

---

## 6. 建议里程碑表述（给如野排期用语）

请避免：「新建多模态子系统 / 新 Vision API」。  
推荐：「**Agent chat 多模态接线验收**（既有 `images` + `model_mounts.vision`）+ DESIGN_DETAIL 交叉引用 + 演示配置」。

| 建议切片 | 内容 |
|---|---|
| 热修 / 小版 | P0：文档、VL 挂载、静默丢图治理、413、CI |
| 下一小版 | P1：证据节点、审计、SSRF、Schema→JSON-LD |
| 后续 | P2 |

版本号由开源线定；业务只认「可引用的 tag + 文档段落」。

---

## 7. 运维要点（运野）— 配置面优先

| 项 | 建议 |
|---|---|
| 模型资源 | 在 `models.resources` 登记 VL（号池 Qwen3-VL 等） |
| Agent | `model_mounts.chat` = 文本；`model_mounts.vision` = VL |
| 回滚 | 去掉 vision mount 或改回文本 = 立即退回文本路径；业务关 `INCLUDE_IMAGES` |
| 健康 | 可选 vision probe（1×1 或 fixture 图） |
| 配额 | WildPool/MiniMax 账单先确认再切 VL |

---

## 8. 测试要点（测野）

1. 无 `images`：纯文本回归。  
2. 有图 + vision 挂 VL：fixture 包装图，回复含可见品牌/规格线索。  
3. 有图但 vision 未挂：断言 **可观测策略**（非静默成功假象）。  
4. 超大 payload。  
5. 无 OIDC / 缺 isolation claims。  
6. （联调）StructCapture organize E2E 由业务仓测，底座提供 staging 说明。

---

## 9. 安全（安野）

1. 鉴权强度与纯文本相同（已有 claims 门禁，勿为带图降级）。  
2. 日志禁止 data URL / 原图。  
3. https 拉图防 SSRF。  
4. 跨租户命名图/向量命名空间仍走 isolation mint（见 `16`/`17`）。  
5. 禁止 HS256 共享密钥「演示捷径」。

---

## 10. 明确不归底座

| 事项 | 归属 |
|---|---|
| home-inventory 字段、枚举、HITL UI、ASR UX | StructCapture / 栈野 |
| 客户 Basic Auth、Caddy、kaiy.ai | 演示运维 / 栈野 VPS |
| 选哪家 VL | 联合建议；落地为 **models.resources 配置** |

---

## 11. 给如野的一页摘要（可转发）

> 请对照 [13-DESIGN_DETAIL.zh.md](https://github.com/skaiy/wild_agentos/blob/main/docs/13-DESIGN_DETAIL.zh.md)：**多模态不是新中台**，而是把图证据接进已有 PDCA/记忆/JSON-LD/5W2H/技能/工具/Gateway。  
> 代码已有 `POST .../chat` 的 `images[]` 与 `model_mounts.vision→chat`。请优先 **挂 VL、写清契约与错误语义、补 CI**，禁止静默丢图。  
> StructCapture 只消费该契约；领域与 HITL 仍在业务仓。旁路 VL 仅临时。请回告目标版本号与 P0 排期。

---

## 12. 开放问题（开源线拍板）

1. ~~vision 回退到 chat（文本模型）时：硬错误 vs 带 `warning` 的软降级？~~ **已定**：默认软降级 + `warning: vision_mount_unavailable` / `degraded`；`AGENTOS_VISION_FALLBACK=error` 可硬 4xx。  
2. 单请求 `max_images` / `max_bytes` 官方默认？  
3. 图证据是否在 P1 强制落 L2 @id，还是 chat 无状态透传即可？  
4. ~~目标版本：`0.6.x` 热修还是 `0.7.0`？~~ **已定**：**v0.6.2**（milestone 热修切片）。

---

## 13. 修订记录

| 日期 | 版本 | 说明 |
|---|---|---|
| 2026-09-09 | 0.1.0 | 初稿能力清单 |
| 2026-09-09 | 0.2.0 | **修订**：强制对齐 `13-DESIGN_DETAIL`；标明已有 `images`/vision mounts；P0 改为配置+契约硬化优先，反对平行造轮 |
| 2026-09-09 | 0.2.0 | 记入开源线定案：v0.6.2；vision 回退默认软降级+warning；配 EN 文件 |
