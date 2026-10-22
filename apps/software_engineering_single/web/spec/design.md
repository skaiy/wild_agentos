# Web 前端详细设计文档

## 版本历史

| 版本 | 日期 | 修改内容 | 作者 |
|------|------|---------|------|
| v1.0 | 2026-05-22 | 初版 | - |
| v1.1 | 2026-05-22 | 新增设置、Agent OS 状态监控、日志查看模块；聊天组件改用 chatscope | - |

---

## 一、项目概述

### 1.1 项目定位

本项目是"软件工程智能体应用"（Software Engineering Agent Application）的 Web 前端。它是一个面向开发者的**SDLC 全生命周期管理平台**，通过可视化界面与 Go 应用层交互，驱动 Rust Agent OS 内核执行需求分析、系统设计、编码实现、测试验证等软件工程阶段。

### 1.2 核心能力

| # | 能力 | 说明 |
|---|------|------|
| 1 | **管线编排** | 可视化的 SDLC 阶段工作流编辑器，支持自定义阶段配置 |
| 2 | **实时监控** | 通过 WebSocket 推送管线执行状态，实时展示阶段进度 |
| 3 | **智能对话** | 与 SA/DA Agent 的多轮对话，支持 Markdown、代码高亮、Diff 视图 |
| 4 | **知识图谱** | 项目 IRI 实体与 SHACL 关系的图可视化 |
| 5 | **人工审查** | 统一的人工审查入口，支持 Temporal Signal + Agent OS Hook 双层 HITL |
| 6 | **项目管理** | 项目、任务、阶段实例的 CRUD 与状态跟踪 |
| 7 | **系统设置** | 服务端配置、LLM API Key/URL/Model 配置、运行参数管理 |
| 8 | **状态监控** | Agent OS 内核状态、Temporal Worker 状态、资源使用监控 |
| 9 | **日志查看** | 系统日志、阶段执行日志、Agent OS 事件日志的实时查看与历史查询 |

### 1.3 架构关系

```
┌───────────────────────────────────────────┐
│           Web 前端 (React + TS)             │
│  ┌─────────┐ ┌────────┐ ┌──────────────┐  │
│  │ 管线编排  │ │ 实时监控│ │ 智能对话 / Chat│  │
│  │ (React   │ │ (WS    │ │ (Markdown +  │  │
│  │  Flow)   │ │ 推送)  │ │  Code Diff)  │  │
│  └────┬─────┘ └───┬────┘ └──────┬───────┘  │
│       │           │             │           │
│       └──────┬────┴─────┬──────┘           │
│              │ REST API │ WebSocket        │
└──────────────┼──────────┼──────────────────┘
               │          │
     ┌─────────▼──────────▼──────────┐
     │     Go 应用层 (Gin +          │
     │     gorilla/websocket)        │
     └─────────┬──────────┬──────────┘
               │ gRPC     │
     ┌─────────▼──────────▼──────────┐
     │     Rust Agent OS 内核         │
     └───────────────────────────────┘
```

---

## 二、技术选型

### 2.1 核心选型

| 类别 | 选型 | 版本 | 理由 |
|:---|:---|:---|:---|
| **核心框架** | React 18 + TypeScript | ^18.3 | Fiber 架构在处理大量节点重绘时性能更优，TS 保证前后端数据结构对齐 |
| **构建工具** | Vite | ^5.4 | 极速冷启动和 HMR，原生 ESM 支持 |
| **UI 组件库** | Ant Design 5 | ^5.21 | 企业级 B 端首选，表格/表单/布局/标签页组件齐全 |
| **路由** | React Router v6 | ^6.28 | 标准 SPA 路由方案，支持嵌套路由 |
| **状态管理** | Zustand | ^5.0 | 轻量、简单，适合管理全局 WebSocket 连接状态、当前工作流实例状态 |
| **HTTP 客户端** | ky / fetch | - | 轻量级 HTTP 请求，配合 Vite proxy 联调 |
| **WebSocket** | 原生 WebSocket | - | 直接使用浏览器 WebSocket API，无需额外库 |

### 2.2 可视化与交互选型

| 类别 | 选型 | 版本 | 理由 |
|:---|:---|:---|:---|
| **工作流画布** | @xyflow/react (React Flow) | ^12.3 | 最适合 SDLC 编排的 Node-Based UI 画布，拖拽/缩放/连线/Dagre 布局 |
| **聊天界面** | @chatscope/chat-ui-kit-react | ^1.10 | 专业聊天 UI 组件库，开箱即用的消息列表/输入框/头像/打字指示器，支持自定义消息渲染 |
| **代码高亮** | prism-react-renderer | ^2.4 | Vercel 出品，轻量灵活，按需引入语法包，支持复制按钮和行号 |
| **代码 Diff** | react-diff-viewer-continued | ^3.2 | 社区维护 Fork，并排/内联 Diff 视图，支持暗黑模式 |
| **终端日志** | xterm | ^5.3 | VS Code 内置终端的底层库，完美解析 ANSI 颜色，Canvas 渲染 |
| **Markdown 渲染** | react-markdown | ^9.0 | 标准 Markdown 渲染方案 |
| **图标** | @ant-design/icons | ^5.5 | 与 Ant Design 配套，图标统一 |

### 2.3 开发工具

| 工具 | 用途 |
|:----|:-----|
| ESLint | 代码质量检查 |
| Prettier | 代码格式化 |
| TypeScript | 类型检查 |
| npm / pnpm | 包管理 |

---

## 三、项目目录结构

```
web/
├── public/
│   └── favicon.ico
├── src/
│   ├── main.tsx                    # 应用入口
│   ├── App.tsx                     # 根组件 + 路由配置
│   ├── vite-env.d.ts              # Vite 环境变量类型
│   │
│   ├── api/                       # API 请求层
│   │   ├── client.ts              # HTTP 客户端 (ky 封装)
│   │   ├── project.ts             # 项目相关 API
│   │   ├── pipeline.ts            # 管线相关 API
│   │   ├── review.ts              # 审查相关 API
│   │   ├── graph.ts               # 图数据 API
│   │   ├── websocket.ts           # WebSocket 连接管理
│   │   ├── settings.ts            # 设置相关 API
│   │   ├── monitor.ts             # 监控相关 API
│   │   └── logs.ts                # 日志相关 API
│   │
│   ├── types/                     # TypeScript 类型定义
│   │   ├── project.ts             # 项目类型
│   │   ├── pipeline.ts            # 管线/阶段类型
│   │   ├── stage.ts               # 阶段类型
│   │   ├── review.ts              # 审查类型
│   │   ├── websocket.ts           # WebSocket 事件类型
│   │   ├── chat.ts                # 对话消息类型
│   │   ├── settings.ts            # 设置类型
│   │   ├── monitor.ts             # 监控类型
│   │   └── logs.ts                # 日志类型
│   │
│   ├── stores/                    # Zustand 状态管理
│   │   ├── projectStore.ts        # 项目列表/当前项目状态
│   │   ├── pipelineStore.ts       # 管线执行状态
│   │   ├── websocketStore.ts      # WebSocket 连接状态
│   │   ├── chatStore.ts           # 对话历史状态
│   │   ├── settingsStore.ts       # 设置状态
│   │   └── monitorStore.ts        # 监控状态
│   │
│   ├── pages/                     # 页面级组件
│   │   ├── Dashboard/             # 仪表盘首页
│   │   │   ├── index.tsx
│   │   │   └── ProjectList.tsx
│   │   ├── ProjectDetail/         # 项目详情页
│   │   │   ├── index.tsx
│   │   │   ├── PipelineWorkspace.tsx   # 管线工作区
│   │   │   ├── StageList.tsx           # 阶段列表
│   │   │   └── StageResult.tsx         # 阶段结果
│   │   ├── PipelineEditor/        # 管线编辑器页
│   │   │   ├── index.tsx
│   │   │   ├── WorkflowCanvas.tsx      # React Flow 画布
│   │   │   ├── NodePalette.tsx         # 节点面板
│   │   │   ├── PropertyPanel.tsx       # 属性配置面板
│   │   │   └── MiniMap.tsx             # 小地图
│   │   ├── Chat/                  # 智能对话页
│   │   │   ├── index.tsx
│   │   │   ├── MessageList.tsx         # 消息列表
│   │   │   ├── MessageInput.tsx        # 输入框
│   │   │   ├── CodeBlock.tsx           # 代码高亮块
│   │   │   ├── CodeDiff.tsx            # 代码差异对比
│   │   │   └── TerminalLog.tsx         # 终端日志展示
│   │   ├── Review/                # 人工审查页
│   │   │   ├── index.tsx
│   │   │   ├── PendingList.tsx         # 待审查列表
│   │   │   ├── ReviewDetail.tsx        # 审查详情
│   │   │   └── ReviewHistory.tsx       # 审查历史
│   │   ├── Graph/                 # 知识图谱页
│   │   │   ├── index.tsx
│   │   │   └── GraphView.tsx
│   │   ├── Settings/              # 系统设置页
│   │   │   ├── index.tsx
│   │   │   ├── ServerConfig.tsx       # 服务端配置
│   │   │   ├── LLMConfig.tsx          # LLM API 配置
│   │   │   └── RuntimeConfig.tsx      # 运行参数配置
│   │   ├── Monitor/               # 状态监控页
│   │   │   ├── index.tsx
│   │   │   ├── AgentOSStatus.tsx      # Agent OS 状态
│   │   │   ├── TemporalStatus.tsx     # Temporal Worker 状态
│   │   │   └── ResourceUsage.tsx      # 资源使用监控
│   │   └── Logs/                  # 日志查看页
│   │       ├── index.tsx
│   │       ├── SystemLogs.tsx         # 系统日志
│   │       ├── StageLogs.tsx          # 阶段执行日志
│   │       └── AgentOSLogs.tsx        # Agent OS 事件日志
│   │
│   ├── components/                # 通用组件
│   │   ├── Layout/
│   │   │   ├── AppLayout.tsx          # 主布局 (侧边栏 + 内容区)
│   │   │   ├── Sidebar.tsx            # 侧边栏导航
│   │   │   └── Header.tsx             # 顶部栏
│   │   ├── StageNode.tsx              # 阶段节点 (React Flow 自定义)
│   │   ├── StageStatusBadge.tsx       # 阶段状态标签
│   │   ├── PipelineTimeline.tsx       # 管线时间线
│   │   ├── WebSocketStatus.tsx        # WebSocket 连接状态指示器
│   │   └── ErrorBoundary.tsx          # 错误边界
│   │
│   ├── hooks/                     # 自定义 Hooks
│   │   ├── useWebSocket.ts            # WebSocket 连接 Hook
│   │   ├── usePipelineStatus.ts       # 管线状态轮询 Hook
│   │   └── useProjectList.ts          # 项目列表 Hook
│   │
│   ├── utils/                     # 工具函数
│   │   ├── format.ts                  # 格式化工具 (时间/状态/数字)
│   │   ├── constants.ts              # 常量定义
│   │   └── typeGuards.ts             # 类型守卫
│   │
│   └── styles/                    # 样式
│       ├── global.css                 # 全局样式
│       ├── theme.ts                   # Ant Design 主题配置
│       └── variables.css              # CSS 变量
│
├── index.html                    # HTML 入口
├── package.json
├── tsconfig.json
├── tsconfig.node.json
├── vite.config.ts
├── .eslintrc.cjs
└── .prettierrc
```

---

## 四、核心功能模块与子功能划分

### 模块一：仪表盘 Dashboard

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| D1-项目列表 | 展示所有项目，支持搜索/筛选/排序，显示项目状态和最近更新时间 | P0 |
| D2-快速启动管线 | 一键创建新项目并启动默认 SDLC 管线 | P0 |
| D3-待审批提醒 | 展示待人工审查的阶段数量，点击跳转审查页 | P0 |
| D4-最近活动 | 最近完成的管线执行记录 | P1 |

### 模块二：管线编辑器 PipelineEditor

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| E1-画布展示 | 使用 React Flow 展示 SDLC 阶段节点和依赖边，支持拖拽/缩放/框选 | P0 |
| E2-节点面板 | 左侧可拖拽的阶段类型面板（需求/设计/编码/测试/评审/CI/CD/部署） | P0 |
| E3-属性面板 | 右侧阶段配置面板，配置阶段参数（超时、重试策略、契约 Schema、AI/人工审查开关） | P0 |
| E4-节点状态 | 节点显示执行状态（Pending/Running/Succeeded/Failed/Reviewing），带颜色标识 | P0 |
| E5-自动布局 | 基于 Dagre 自动布局成水平或垂直 SDLC 流 | P0 |
| E6-保存/加载 | 管线配置的保存与加载 | P1 |
| E7-条件边 | 支持 `on_success` / `on_failure` 多条条件边 | P1 |

### 模块三：项目详情 ProjectDetail

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| P1-项目信息 | 展示项目元数据（名称、描述、创建时间、状态） | P0 |
| P2-管线运行 | 启动/停止/重试/回退管线执行 | P0 |
| P3-阶段列表 | 按顺序展示所有阶段，显示每个阶段的状态、耗时、重试次数 | P0 |
| P4-阶段详情 | 点击阶段查看详细执行结果（Summary、Output、IRI、Artifacts） | P0 |
| P5-实时状态 | 通过 WebSocket 实时更新阶段状态、进度 | P0 |
| P6-管线时间线 | 按时间轴展示管线执行过程中的关键事件 | P1 |

### 模块四：智能对话 Chat

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| C1-消息列表 | 展示 Agent 与用户的多轮对话消息流 | P0 |
| C2-消息输入 | 文本输入框 + 发送按钮，支持快捷键发送 | P0 |
| C3-Markdown 渲染 | 使用 react-markdown 解析 Agent 回复中的 Markdown 内容 | P0 |
| C4-代码高亮 | 使用 prism-react-renderer 渲染代码块，带语言标签和复制按钮 | P0 |
| C5-代码 Diff | 使用 react-diff-viewer-continued 展示 Agent 修改代码的红绿对比 | P0 |
| C6-终端日志 | 使用 xterm.js 渲染流式日志（带 ANSI 颜色） | P0 |
| C7-工具调用卡片 | Agent 调用工具的请求/结果展示卡片 | P1 |
| C8-消息历史 | 加载历史对话消息 | P1 |

### 模块五：人工审查 Review

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| R1-待审查列表 | 展示所有等待人工审查的阶段实例（来源项目/阶段名称/开始时间） | P0 |
| R2-审查详情 | 展示阶段产出的代码/文档详情，提供"通过/拒绝"操作 | P0 |
| R3-审查提交 | 提交审查结果（通过/拒绝 + 评论），触发 Temporal Signal | P0 |
| R4-审查历史 | 查询指定阶段的审查历史记录 | P1 |
| R5-批量审查 | 批量审批同项目下多个阶段 | P2 |

### 模块六：知识图谱 Graph

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| G1-图展示 | 使用 AntV G6 展示项目的 IRI 实体节点和 SHACL 关系边 | P1 |
| G2-节点详情 | 点击节点查看属性详情（类型、IRI、关联关系） | P1 |
| G3-图搜索 | 按 IRI 名称搜索节点 | P2 |

### 模块七：系统设置 Settings

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| S1-服务端配置 | 配置 Go 应用层服务地址、端口、Temporal Server 地址 | P0 |
| S2-LLM 配置 | 配置 LLM API Key、Base URL、Model 名称、Temperature 等参数 | P0 |
| S3-Agent OS 配置 | 配置 Rust Agent OS gRPC 地址、超时时间 | P0 |
| S4-运行参数 | 管线默认超时、重试策略、并发数等运行参数 | P1 |
| S5-配置持久化 | 配置保存到 localStorage 或后端，支持导入/导出 | P1 |
| S6-配置验证 | 保存前验证配置有效性（API Key 格式、URL 可达性） | P1 |

### 模块八：状态监控 Monitor

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| M1-Agent OS 状态 | 展示 Rust Agent OS 内核状态（运行中/停止）、gRPC 连接状态、版本信息 | P0 |
| M2-Temporal 状态 | 展示 Temporal Server 连接状态、Worker 数量、任务队列状态 | P0 |
| M3-活跃任务 | 当前正在执行的管线/阶段列表，实时进度展示 | P0 |
| M4-资源监控 | CPU、内存、磁盘使用情况（从后端获取） | P1 |
| M5-连接状态 | WebSocket 连接状态、gRPC 连接状态的可视化指示器 | P0 |
| M6-健康检查 | 一键检测各组件健康状态（Agent OS / Temporal / LLM API） | P1 |

### 模块九：日志查看 Logs

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| L1-系统日志 | Go 应用层的运行日志，支持按级别（INFO/WARN/ERROR）筛选 | P0 |
| L2-阶段日志 | 指定阶段的执行日志，包含 LLM 请求/响应、工具调用记录 | P0 |
| L3-Agent OS 日志 | Rust Agent OS 内核的事件日志（通过 gRPC 订阅） | P0 |
| L4-实时日志流 | 使用 xterm.js 实时展示日志流，支持 ANSI 颜色 | P0 |
| L5-日志搜索 | 按关键词、时间范围搜索历史日志 | P1 |
| L6-日志导出 | 导出日志为文件（JSON/TXT 格式） | P2 |

### 模块十：公共功能

| 子功能 | 功能概要 | 优先级 |
|--------|---------|--------|
| B1-布局框架 | 侧边栏导航 + 内容区布局，响应式适配 | P0 |
| B2-路由导航 | 基于 React Router v6 的 SPA 路由，支持 History 模式 | P0 |
| B3-WebSocket 连接 | 统一管理 WebSocket 连接，断线重连 | P0 |
| B4-全局状态 | Zustand store 管理全局状态（当前项目/管线/WebSocket） | P0 |
| B5-暗黑模式 | Ant Design 暗黑主题切换 | P1 |
| B6-错误处理 | 统一错误边界和异常处理 | P0 |
| B7-加载状态 | 全局 Loading、Skeleton 加载态 | P0 |

---

## 五、路由设计

```tsx
// 路由层级
<AppLayout>
  ├── /                          → Dashboard 仪表盘
  ├── /projects                   → ProjectList 项目列表
  ├── /projects/:id               → ProjectDetail 项目详情
  │   ├── (默认)                   → PipelineWorkspace 管线工作区
  │   ├── stages                  → StageList 阶段列表
  │   └── stages/:stageId         → StageResult 阶段结果
  ├── /projects/:id/editor        → PipelineEditor 管线编辑器
  ├── /chat/:projectId?           → Chat 智能对话
  ├── /reviews                    → Review 人工审查
  │   ├── (默认)                   → PendingList 待审查列表
  │   └── :stageId                → ReviewDetail 审查详情
  ├── /graph/:projectId?          → Graph 知识图谱
  ├── /settings                   → Settings 系统设置
  │   ├── (默认)                   → ServerConfig 服务端配置
  │   ├── llm                     → LLMConfig LLM 配置
  │   └── runtime                 → RuntimeConfig 运行参数
  ├── /monitor                    → Monitor 状态监控
  │   ├── (默认)                   → AgentOSStatus Agent OS 状态
  │   ├── temporal                → TemporalStatus Temporal 状态
  │   └── resources               → ResourceUsage 资源监控
  └── /logs                       → Logs 日志查看
      ├── (默认)                   → SystemLogs 系统日志
      ├── stage/:taskId/:stageId  → StageLogs 阶段日志
      └── agent-os                → AgentOSLogs Agent OS 日志
```

### 路由配置表

| 路径 | 页面 | 说明 |
|------|------|------|
| `/` | Dashboard | 首页仪表盘，重定向到 `/projects` |
| `/projects` | ProjectList | 项目列表页 |
| `/projects/:id` | ProjectDetail | 项目详情页（默认展示管线工作区） |
| `/projects/:id/editor` | PipelineEditor | 管线编辑器页 |
| `/projects/:id/stages` | StageList | 阶段列表页 |
| `/projects/:id/stages/:stageId` | StageResult | 阶段结果详情页 |
| `/chat/:projectId?` | Chat | 智能对话页，可指定项目上下文 |
| `/reviews` | Review | 人工审查页 |
| `/reviews/:stageId` | ReviewDetail | 审查详情页 |
| `/graph/:projectId?` | Graph | 知识图谱页 |
| `/settings` | Settings | 系统设置页（默认展示服务端配置） |
| `/settings/llm` | LLMConfig | LLM API 配置页 |
| `/settings/runtime` | RuntimeConfig | 运行参数配置页 |
| `/monitor` | Monitor | 状态监控页（默认展示 Agent OS 状态） |
| `/monitor/temporal` | TemporalStatus | Temporal 状态页 |
| `/monitor/resources` | ResourceUsage | 资源监控页 |
| `/logs` | Logs | 日志查看页（默认展示系统日志） |
| `/logs/stage/:taskId/:stageId` | StageLogs | 阶段执行日志页 |
| `/logs/agent-os` | AgentOSLogs | Agent OS 事件日志页 |

---

## 六、状态管理设计 (Zustand)

### 6.1 projectStore

```typescript
interface ProjectState {
  projects: ProjectMeta[];
  currentProject: ProjectMeta | null;
  loading: boolean;
  error: string | null;

  // Actions
  fetchProjects: () => Promise<void>;
  fetchProject: (id: string) => Promise<void>;
  createProject: (input: CreateProjectInput) => Promise<void>;
  deleteProject: (id: string) => Promise<void>;
}
```

### 6.2 pipelineStore

```typescript
interface PipelineState {
  currentPipeline: PipelineResult | null;
  stages: StageInstanceMeta[];
  taskMeta: TaskMeta | null;
  loading: boolean;

  // Actions
  startPipeline: (projectId: string, input: PipelineInput) => Promise<void>;
  fetchPipelineStatus: (projectId: string) => Promise<void>;
  fetchStageDetail: (taskId: string, stageId: string) => Promise<void>;
  retryTask: (taskId: string) => Promise<void>;
  rollbackTask: (taskId: string) => Promise<void>;
  updateStageFromWS: (event: WSEvent) => void;  // WebSocket 驱动的状态更新
}
```

### 6.3 websocketStore

```typescript
interface WebSocketState {
  connected: boolean;
  projectId: string | null;
  lastEvent: WSEvent | null;

  // Actions
  connect: (projectId: string) => void;
  disconnect: () => void;
  onMessage: (callback: (event: WSEvent) => void) => void;
}
```

### 6.4 chatStore

```typescript
interface ChatState {
  messages: ChatMessage[];
  streaming: boolean;
  currentMessage: string;

  // Actions
  sendMessage: (content: string) => Promise<void>;
  appendMessage: (msg: ChatMessage) => void;
  clearMessages: () => void;
  loadHistory: (projectId: string) => Promise<void>;
}
```

### 6.5 settingsStore

```typescript
interface SettingsState {
  server: ServerConfig;
  llm: LLMConfig;
  agentOS: AgentOSConfig;
  runtime: RuntimeConfig;

  // Actions
  loadSettings: () => void;                    // 从 localStorage 加载
  saveSettings: () => void;                    // 保存到 localStorage
  updateServerConfig: (config: Partial<ServerConfig>) => void;
  updateLLMConfig: (config: Partial<LLMConfig>) => void;
  updateAgentOSConfig: (config: Partial<AgentOSConfig>) => void;
  updateRuntimeConfig: (config: Partial<RuntimeConfig>) => void;
  validateConfig: () => Promise<ValidationResult>;
  exportConfig: () => string;                  // 导出为 JSON
  importConfig: (json: string) => void;        // 从 JSON 导入
}

interface ServerConfig {
  apiBaseUrl: string;        // Go 应用层 API 地址，如 "http://localhost:8080"
  wsBaseUrl: string;         // WebSocket 地址，如 "ws://localhost:8080"
  temporalHost: string;      // Temporal Server 地址，如 "temporal.example.com:7233"
}

interface LLMConfig {
  apiKey: string;            // LLM API Key（敏感信息，加密存储）
  baseUrl: string;           // LLM API Base URL
  model: string;             // 模型名称，如 "deepseek-chat"
  temperature: number;       // 温度参数
  maxTokens: number;         // 最大 Token 数
}

interface AgentOSConfig {
  grpcAddress: string;       // Rust Agent OS gRPC 地址
  grpcTimeout: number;       // gRPC 超时时间（秒）
}

interface RuntimeConfig {
  defaultTimeout: number;    // 默认阶段超时（秒）
  maxRetries: number;        // 默认最大重试次数
  maxConcurrency: number;    // 最大并发阶段数
}
```

### 6.6 monitorStore

```typescript
interface MonitorState {
  agentOSStatus: AgentOSStatus | null;
  temporalStatus: TemporalStatus | null;
  resourceUsage: ResourceUsage | null;
  activeTasks: ActiveTask[];
  loading: boolean;

  // Actions
  fetchAgentOSStatus: () => Promise<void>;
  fetchTemporalStatus: () => Promise<void>;
  fetchResourceUsage: () => Promise<void>;
  fetchActiveTasks: () => Promise<void>;
  healthCheck: () => Promise<HealthCheckResult>;
  subscribeToStatus: () => void;              // WebSocket 订阅状态更新
}

interface AgentOSStatus {
  running: boolean;
  version: string;
  grpcConnected: boolean;
  uptime: number;
  taskCount: number;
}

interface TemporalStatus {
  connected: boolean;
  namespace: string;
  workerCount: number;
  taskQueue: string;
  pendingWorkflows: number;
}

interface ResourceUsage {
  cpuPercent: number;
  memoryUsedMB: number;
  memoryTotalMB: number;
  diskUsedGB: number;
  diskTotalGB: number;
}
```

---

## 七、API 数据流与通信

### 7.1 REST API 契约

Web 前端通过 REST API 与 Go 应用层通信，所有 API 路径以 `/api/v1` 开头。

| 方法 | 路径 | 请求体 | 响应 | 说明 |
|------|------|--------|------|------|
| GET | `/api/v1/projects` | - | `ProjectMeta[]` | 获取项目列表 |
| POST | `/api/v1/projects` | `{name, description}` | `ProjectMeta` | 创建项目 |
| GET | `/api/v1/projects/:id` | - | `ProjectMeta` | 获取项目详情 |
| DELETE | `/api/v1/projects/:id` | - | - | 删除项目 |
| POST | `/api/v1/projects/:id/tasks` | `{pipeline_name}` | `TaskMeta` | 创建并启动任务 |
| GET | `/api/v1/projects/:id/tasks` | - | `TaskMeta[]` | 列出项目下所有任务 |
| GET | `/api/v1/tasks/:taskId` | - | `TaskMeta` | 获取任务详情 |
| POST | `/api/v1/tasks/:taskId/retry` | - | - | 重试任务 |
| POST | `/api/v1/tasks/:taskId/rollback` | - | - | 回退任务 |
| POST | `/api/v1/pipelines` | `PipelineInput` | `{project_id, task_id, workflow_id}` | 启动管线 |
| GET | `/api/v1/pipelines/:id` | - | `PipelineResult` | 获取管线结果 |
| GET | `/api/v1/tasks/:taskId/stages` | - | `StageInstanceMeta[]` | 阶段列表 |
| GET | `/api/v1/tasks/:taskId/stages/:stageId` | - | `StageInstanceMeta` | 阶段详情 |
| POST | `/api/v1/reviews/:stageId/submit` | `HumanReviewRequest` | `{status, approved}` | 提交审查 |
| GET | `/api/v1/reviews/pending` | - | `{reviews: PendingReview[]}` | 待审查列表 |
| GET | `/api/v1/reviews/:stageId/history` | - | `{reviews: ReviewRecord[]}` | 审查历史 |
| GET | `/api/v1/projects/:id/graph` | - | `GraphData` | 项目图数据 |
| GET | `/api/v1/projects/:id/snapshot` | - | `WorkflowSnapshot` | 项目快照 |
| GET | `/api/v1/system/status` | - | `SystemStatus` | 系统状态（Agent OS + Temporal） |
| GET | `/api/v1/system/health` | - | `HealthCheckResult` | 健康检查 |
| GET | `/api/v1/system/resources` | - | `ResourceUsage` | 资源使用情况 |
| GET | `/api/v1/system/active-tasks` | - | `ActiveTask[]` | 活跃任务列表 |
| GET | `/api/v1/logs/system` | `?level=&since=` | `LogEntry[]` | 系统日志 |
| GET | `/api/v1/logs/stage/:taskId/:stageId` | - | `LogEntry[]` | 阶段执行日志 |
| GET | `/api/v1/logs/agent-os` | `?since=` | `LogEntry[]` | Agent OS 事件日志 |
| GET | `/api/v1/logs/stream` | - | `WebSocket Stream` | 实时日志流 |
| POST | `/api/v1/config/llm` | `LLMConfig` | `{success}` | 保存 LLM 配置 |
| GET | `/api/v1/config/llm` | - | `LLMConfig` | 获取 LLM 配置（脱敏） |
| POST | `/api/v1/config/validate` | `ConfigValidationRequest` | `ValidationResult` | 验证配置有效性 |

### 7.2 WebSocket 事件协议

前端的 WebSocket 连接路径为 `/ws?project_id={projectId}`。

**服务端推送事件格式**：

```json
{
  "type": "event_type",
  "project_id": "proj_xxx",
  "payload": {},
  "timestamp": 1716000000000
}
```

| 事件类型 | Payload | 说明 |
|---------|---------|------|
| `pipeline_started` | `{project_id, task_id, workflow_id}` | 管线启动 |
| `stage_started` | `{stage_id, stage_type, name}` | 阶段开始 |
| `stage_completed` | `{stage_id, status, iri, duration_ms}` | 阶段完成 |
| `stage_failed` | `{stage_id, errors}` | 阶段失败 |
| `stage_ai_review` | `{stage_id, approved, score}` | AI 评审结果 |
| `stage_human_review_required` | `{stage_id, name}` | 需要人工审查 |
| `pipeline_completed` | `{status, summary, total_duration_ms}` | 管线完成 |
| `stage_progress` | `{stage_id, progress, message}` | 阶段进度更新 |
| `agent_os_event` | `{event_type, data}` | Agent OS 内部事件 |

### 7.3 前端通信架构

```
┌─────────────────┐      REST/HTTP       ┌──────────────┐
│   Web 前端       │ ──────────────────→  │  Go 应用层    │
│  (React + TS)   │ ←──────────────────  │  (Gin API)    │
│                 │      JSON Response   │              │
│                 │                       │              │
│  WebSocket 连接  │ ←───────────────────  │  WebSocket   │
│  (ws://...)     │   实时事件推送         │  Hub         │
└─────────────────┘                       └──────────────┘
```

---

## 八、关键组件设计

### 8.1 React Flow 自定义节点 (StageNode)

```typescript
// 自定义阶段节点属性
interface StageNodeData {
  id: string;
  name: string;
  stageType: StageType;
  status: StageStatus;         // pending / running / success / failed / reviewing
  order: number;
  hasAIReview: boolean;
  hasHumanReview: boolean;
  contractSchema?: string;
  timeoutSeconds: number;
  onFailure: FailurePolicy;
  onConfigure: (nodeId: string) => void;  // 打开属性面板
  onViewDetail: (nodeId: string) => void; // 查看阶段详情
}

// 节点颜色映射
const NODE_COLORS: Record<StageType, string> = {
  requirement: '#1890ff',  // 蓝色
  design: '#722ed1',       // 紫色
  coding: '#52c41a',       // 绿色
  testing: '#fa8c16',      // 橙色
  review: '#eb2f96',       // 粉色
  cicd: '#13c2c2',         // 青色
  deploy: '#fa541c',       // 红色
};
```

**每个节点 UI 组成**：
- 顶部：阶段类型图标 + 名称
- 中间：执行状态指示器（颜色 + 文字标签）
- 底部：配置快速入口（AI 评审/人工审查开关）
- 右侧：查看详情按钮

### 8.2 代码高亮组件 (CodeBlock)

```typescript
interface CodeBlockProps {
  code: string;
  language: string;
  showLineNumbers?: boolean;
  maxHeight?: number;
}

// 功能:
// 1. 语言标签（如 "go", "typescript", "rust"）
// 2. 复制到剪贴板按钮
// 3. 行号显示
// 4. 主题色（配合暗黑模式）
// 5. 可选的最大高度（超出滚动）
```

### 8.3 代码 Diff 组件 (CodeDiff)

```typescript
interface CodeDiffProps {
  oldCode: string;
  newCode: string;
  language?: string;
  splitView?: boolean;   // 默认并排对比
  leftTitle?: string;
  rightTitle?: string;
}

// 功能:
// 1. 红绿对比视图
// 2. 支持并排/合并模式切换
// 3. 暗黑模式
```

### 8.4 终端日志组件 (TerminalLog)

```typescript
interface TerminalLogProps {
  logStream: string;       // ANSI 日志文本
  autoScroll?: boolean;    // 自动滚动到底部
  maxLines?: number;       // 最大行数
}

// 功能:
// 1. 基于 xterm.js 渲染
// 2. 只读终端
// 3. ANSI 颜色解析
// 4. 自动滚动
```

### 8.5 布局框架 (AppLayout)

```
┌──────────────────────────────────────────────────┐
│  Header                                           │
│   logo | 项目名称 | WebSocket状态 | 用户信息       │
├────────┬─────────────────────────────────────────┤
│        │                                          │
│ Sidebar│  Content (React Router Outlet)            │
│        │                                          │
│ 📊 仪表盘│                                         │
│ 📁 项目  │                                         │
│ 💬 对话  │                                         │
│ ✅ 审查  │                                         │
│ 🕸️ 图谱  │                                         │
│ ─────── │                                         │
│ ⚙️ 设置  │                                         │
│ 📡 监控  │                                         │
│ 📜 日志  │                                         │
│        │                                          │
└────────┴──────────────────────────────────────────┘
```

### 8.6 设置页面组件 (Settings)

```typescript
// 设置页面使用 Tabs 切换不同配置项
// - 服务端配置：API 地址、WebSocket 地址、Temporal 地址
// - LLM 配置：API Key（密码输入框）、Base URL、Model 选择、Temperature 滑块
// - 运行参数：默认超时、重试策略、并发数

// 配置项示例
interface SettingsFormProps {
  initialValues: ServerConfig | LLMConfig | RuntimeConfig;
  onSave: (values: any) => Promise<void>;
  onValidate: (values: any) => Promise<ValidationResult>;
}
```

### 8.7 状态监控组件 (Monitor)

```typescript
// 监控页面展示各组件状态卡片
// - Agent OS 状态卡片：运行状态、版本、gRPC 连接、运行时长
// - Temporal 状态卡片：连接状态、Worker 数量、待处理工作流
// - 资源使用卡片：CPU/内存/磁盘使用率图表
// - 活跃任务列表：当前执行中的管线/阶段

// 状态指示器
type StatusIndicator = 'running' | 'stopped' | 'error' | 'unknown';
```

### 8.8 日志查看组件 (Logs)

```typescript
// 日志页面使用 xterm.js 展示实时日志流
// 支持按级别筛选（INFO/WARN/ERROR）
// 支持关键词搜索
// 支持日志导出

interface LogViewerProps {
  source: 'system' | 'stage' | 'agent-os';
  taskId?: string;
  stageId?: string;
  autoScroll?: boolean;
  maxLines?: number;
}
```

---

## 九、开发环境与构建

### 9.1 开发环境

前端开发时，Vite 开发服务器（默认 5173 端口）通过 proxy 代理调用 Go 后端：

```typescript
// vite.config.ts
export default defineConfig({
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://localhost:8080',
        changeOrigin: true,
      },
      '/ws': {
        target: 'ws://localhost:8080',
        ws: true,
      },
    },
  },
});
```

### 9.2 生产构建

```bash
cd web && npm install && npm run build
```

构建输出到 `web/dist/` 目录，由 Go 层的 `//go:embed` 指令嵌入到二进制文件中。

### 9.3 环境要求

| 工具 | 版本要求 |
|:----|:--------|
| Node.js | >= 18.x |
| npm | >= 9.x |
| 浏览器 | Chrome/Firefox/Safari 最新版本 |

---

## 十、开发计划 (TODOs)

### Phase 1: 项目初始化
- [ ] 1.1 使用 Vite 创建 React + TypeScript 项目
- [ ] 1.2 安装所有依赖（Ant Design, Zustand, React Flow, @chatscope/chat-ui-kit-react, prism-react-renderer 等）
- [ ] 1.3 配置 Vite proxy、TypeScript、ESLint、Prettier
- [ ] 1.4 实现全局样式、主题配置、CSS 变量

### Phase 2: 布局与路由
- [ ] 2.1 实现 AppLayout（Header + Sidebar + Content）
- [ ] 2.2 配置 React Router v6 路由
- [ ] 2.3 实现 Dashboard 页面（项目列表 + 快速启动）
- [ ] 2.4 实现 Sidebar 导航

### Phase 3: 类型与 API
- [ ] 3.1 定义所有 TypeScript 类型
- [ ] 3.2 实现 HTTP API 客户端
- [ ] 3.3 实现 Zustand stores（projectStore, pipelineStore）
- [ ] 3.4 实现 WebSocket 管理（websocketStore + useWebSocket hook）

### Phase 4: 管线编辑器
- [ ] 4.1 实现 React Flow 自定义 StageNode 组件
- [ ] 4.2 实现 NodePalette（左侧拖拽面板）
- [ ] 4.3 实现 PropertyPanel（右侧属性配置）
- [ ] 4.4 实现 Dagre 自动布局
- [ ] 4.5 实现节点状态颜色映射

### Phase 5: 项目详情与管线运行
- [ ] 5.1 实现 ProjectDetail 页面
- [ ] 5.2 实现 PipelineWorkspace（启动/停止/状态展示）
- [ ] 5.3 实现 StageList + StageStatusBadge
- [ ] 5.4 实现 PipelineTimeline
- [ ] 5.5 实现 WebSocket 实时状态更新

### Phase 6: 智能对话
- [ ] 6.1 实现 Chat 页面布局（使用 @chatscope/chat-ui-kit-react）
- [ ] 6.2 实现 CodeBlock 组件（prism-react-renderer）
- [ ] 6.3 实现 CodeDiff 组件（react-diff-viewer-continued）
- [ ] 6.4 实现 TerminalLog 组件（xterm.js）
- [ ] 6.5 实现 MessageList + MessageInput

### Phase 7: 人工审查
- [ ] 7.1 实现 Review 页面
- [ ] 7.2 实现 PendingList
- [ ] 7.3 实现 ReviewDetail + 提交功能
- [ ] 7.4 实现 ReviewHistory

### Phase 8: 知识图谱
- [ ] 8.1 实现 Graph 页面
- [ ] 8.2 实现 GraphView 组件

### Phase 9: 系统设置
- [ ] 9.1 实现 Settings 页面布局（Tabs 切换）
- [ ] 9.2 实现 ServerConfig 组件（服务端配置表单）
- [ ] 9.3 实现 LLMConfig 组件（LLM API 配置表单，含 API Key 密码输入）
- [ ] 9.4 实现 RuntimeConfig 组件（运行参数配置）
- [ ] 9.5 实现 settingsStore（配置状态管理）
- [ ] 9.6 实现配置持久化（localStorage）
- [ ] 9.7 实现配置验证功能

### Phase 10: 状态监控
- [ ] 10.1 实现 Monitor 页面布局
- [ ] 10.2 实现 AgentOSStatus 组件（Agent OS 状态卡片）
- [ ] 10.3 实现 TemporalStatus 组件（Temporal 状态卡片）
- [ ] 10.4 实现 ResourceUsage 组件（资源使用图表）
- [ ] 10.5 实现 ActiveTasks 组件（活跃任务列表）
- [ ] 10.6 实现 monitorStore（监控状态管理）
- [ ] 10.7 实现健康检查功能

### Phase 11: 日志查看
- [ ] 11.1 实现 Logs 页面布局
- [ ] 11.2 实现 SystemLogs 组件（系统日志查看）
- [ ] 11.3 实现 StageLogs 组件（阶段执行日志）
- [ ] 11.4 实现 AgentOSLogs 组件（Agent OS 事件日志）
- [ ] 11.5 实现实时日志流（WebSocket + xterm.js）
- [ ] 11.6 实现日志搜索功能
- [ ] 11.7 实现日志导出功能

### Phase 12: 完善与测试
- [ ] 12.1 实现暗黑模式切换
- [ ] 12.2 添加加载态/骨架屏
- [ ] 12.3 添加错误边界
- [ ] 12.4 基础功能自测