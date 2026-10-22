# 后端 RESTful API 接口文档

## 版本历史

| 版本 | 日期 | 修改内容 |
|------|------|---------|
| v1.0 | 2026-05-22 | 初版 |

---

## 一、概述

本文档为 Web 前端与 Go 后端之间的 RESTful API 契约。所有 API 路径以 `/api/v1` 为前缀，请求与响应均使用 JSON 格式。

### 1.1 基础约定

- **Base URL**: `http://localhost:8080/api/v1`
- **Content-Type**: `application/json`
- **认证**: `Authorization: Bearer <token>`（可选，由前端 localStorage 中的 `auth_token` 提供）
- **时间格式**: ISO 8601（如 `2026-05-22T10:30:00Z`）
- **时间戳**: Unix 毫秒（如 `1716000000000`）
- **成功响应**: HTTP 200（带 JSON body）或 HTTP 204（无 body）
- **错误响应**: HTTP 4xx/5xx，body 为 `{"message": "错误描述"}`

### 1.2 错误响应格式

```json
{
  "code": "VALIDATION_ERROR",
  "message": "具体的错误描述",
  "details": [
    { "field": "name", "message": "项目名称不能为空" }
  ]
}
```

可能的 `code` 值：`VALIDATION_ERROR`、`NOT_FOUND`、`INTERNAL_ERROR`、`UNAUTHORIZED`、`FORBIDDEN`

---

## 二、接口总览

### 2.1 接口清单

| # | 分组 | 路径 | 方法 | 说明 |
|---|------|------|------|------|
| 1 | 项目 | `/api/v1/projects` | GET | 获取项目列表 |
| 2 | 项目 | `/api/v1/projects` | POST | 创建项目 |
| 3 | 项目 | `/api/v1/projects/:id` | GET | 获取项目详情 |
| 4 | 项目 | `/api/v1/projects/:id` | DELETE | 删除项目 |
| 5 | 任务 | `/api/v1/projects/:id/tasks` | GET | 获取项目下所有任务 |
| 6 | 任务 | `/api/v1/projects/:id/tasks` | POST | 创建并启动任务 |
| 7 | 任务 | `/api/v1/tasks/:taskId` | GET | 获取任务详情 |
| 8 | 任务 | `/api/v1/tasks/:taskId/retry` | POST | 重试任务 |
| 9 | 任务 | `/api/v1/tasks/:taskId/rollback` | POST | 回退任务 |
| 10 | 管线 | `/api/v1/pipelines` | POST | 启动管线 |
| 11 | 管线 | `/api/v1/pipelines/:id` | GET | 获取管线结果 |
| 12 | 阶段 | `/api/v1/tasks/:taskId/stages` | GET | 获取阶段列表 |
| 13 | 阶段 | `/api/v1/tasks/:taskId/stages/:stageId` | GET | 获取阶段详情 |
| 14 | 审查 | `/api/v1/reviews/pending` | GET | 待审查列表 |
| 15 | 审查 | `/api/v1/reviews/:stageId` | GET | 审查详情 |
| 16 | 审查 | `/api/v1/reviews/:stageId/submit` | POST | 提交审查 |
| 17 | 审查 | `/api/v1/reviews/:stageId/history` | GET | 审查历史 |
| 18 | 对话 | `/api/v1/chat/send` | POST | 发送对话消息 |
| 19 | 对话 | `/api/v1/chat/history` | GET | 获取对话历史 |
| 20 | 配置 | `/api/v1/config/llm` | GET | 获取 LLM 配置 |
| 21 | 配置 | `/api/v1/config/llm` | POST | 保存 LLM 配置 |
| 22 | 配置 | `/api/v1/config/validate` | POST | 验证配置有效性 |
| 23 | 监控 | `/api/v1/system/status` | GET | 系统状态 |
| 24 | 监控 | `/api/v1/system/health` | GET | 健康检查 |
| 25 | 监控 | `/api/v1/system/resources` | GET | 资源使用 |
| 26 | 监控 | `/api/v1/system/active-tasks` | GET | 活跃任务列表 |
| 27 | 日志 | `/api/v1/logs/system` | GET | 系统日志 |
| 28 | 日志 | `/api/v1/logs/stage/:taskId/:stageId` | GET | 阶段执行日志 |
| 29 | 日志 | `/api/v1/logs/agent-os` | GET | Agent OS 事件日志 |
| 30 | 图谱 | `/api/v1/projects/:id/graph` | GET | 项目图数据 |
| 31 | 快照 | `/api/v1/projects/:id/snapshot` | GET | 项目工作流快照 |

### 2.2 WebSocket 接口

| 端点 | 说明 |
|------|------|
| `/ws?project_id={projectId}` | 实时事件推送 |

---

## 三、数据模型定义

### 3.1 通用枚举

#### ProjectStatus

```go
type ProjectStatus string
const (
    ProjectStatusActive   ProjectStatus = "active"
    ProjectStatusArchived ProjectStatus = "archived"
    ProjectStatusDeleted  ProjectStatus = "deleted"
)
```

#### TaskStatus

```go
type TaskStatus string
const (
    TaskStatusPending   TaskStatus = "pending"
    TaskStatusRunning   TaskStatus = "running"
    TaskStatusCompleted TaskStatus = "completed"
    TaskStatusFailed    TaskStatus = "failed"
    TaskStatusPaused    TaskStatus = "paused"
)
```

#### StageType

```go
type StageType string
const (
    StageTypeRequirement StageType = "requirement"
    StageTypeDesign      StageType = "design"
    StageTypeCoding      StageType = "coding"
    StageTypeTesting     StageType = "testing"
    StageTypeReview      StageType = "review"
    StageTypeCICD        StageType = "cicd"
    StageTypeDeploy      StageType = "deploy"
)
```

#### StageStatus

```go
type StageStatus string
const (
    StageStatusPending   StageStatus = "pending"
    StageStatusRunning   StageStatus = "running"
    StageStatusSuccess   StageStatus = "success"
    StageStatusFailed    StageStatus = "failed"
    StageStatusReviewing StageStatus = "reviewing"
    StageStatusSkipped   StageStatus = "skipped"
)
```

#### FailurePolicy

```go
type FailurePolicy string
const (
    FailurePolicyFail     FailurePolicy = "fail"
    FailurePolicyRetry    FailurePolicy = "retry"
    FailurePolicySkip     FailurePolicy = "skip"
    FailurePolicyRollback FailurePolicy = "rollback"
)
```

#### LogLevel

```go
type LogLevel string
const (
    LogLevelDEBUG LogLevel = "DEBUG"
    LogLevelINFO  LogLevel = "INFO"
    LogLevelWARN  LogLevel = "WARN"
    LogLevelERROR LogLevel = "ERROR"
)
```

#### MessageRole

```go
type MessageRole string
const (
    MessageRoleUser      MessageRole = "user"
    MessageRoleAssistant MessageRole = "assistant"
    MessageRoleSystem    MessageRole = "system"
)
```

#### MessageContentType

```go
type MessageContentType string
const (
    MessageContentTypeText     MessageContentType = "text"
    MessageContentTypeCode     MessageContentType = "code"
    MessageContentTypeDiff     MessageContentType = "diff"
    MessageContentTypeTerminal MessageContentType = "terminal"
    MessageContentTypeToolCall MessageContentType = "tool_call"
)
```

### 3.2 数据模型

#### ProjectMeta

```json
{
  "id": "proj_001",
  "name": "电商平台",
  "description": "电商平台项目",
  "status": "active",
  "createdAt": "2026-05-22T10:00:00Z",
  "updatedAt": "2026-05-22T10:00:00Z"
}
```

#### ProjectDetail

```json
{
  "id": "proj_001",
  "name": "电商平台",
  "description": "电商平台项目",
  "status": "active",
  "createdAt": "2026-05-22T10:00:00Z",
  "updatedAt": "2026-05-22T10:00:00Z",
  "taskCount": 3,
  "lastTaskAt": "2026-05-22T11:00:00Z"
}
```

#### TaskMeta

```json
{
  "id": "task_001",
  "projectId": "proj_001",
  "workflowId": "wf_abc123",
  "status": "running",
  "createdAt": "2026-05-22T10:00:00Z",
  "updatedAt": "2026-05-22T10:05:00Z",
  "startedAt": "2026-05-22T10:00:30Z",
  "completedAt": null,
  "totalDurationMs": null
}
```

#### StageInstanceMeta

```json
{
  "id": "stage_001",
  "taskId": "task_001",
  "name": "需求分析",
  "stageType": "requirement",
  "status": "success",
  "order": 1,
  "startedAt": "2026-05-22T10:00:30Z",
  "completedAt": "2026-05-22T10:05:30Z",
  "durationMs": 300000,
  "retryCount": 0,
  "hasAIReview": true,
  "hasHumanReview": false,
  "iri": "iri://proj_001/requirement"
}
```

#### StageDetail

```json
{
  "id": "stage_001",
  "taskId": "task_001",
  "name": "需求分析",
  "stageType": "requirement",
  "status": "success",
  "order": 1,
  "startedAt": "2026-05-22T10:00:30Z",
  "completedAt": "2026-05-22T10:05:30Z",
  "durationMs": 300000,
  "retryCount": 0,
  "hasAIReview": true,
  "hasHumanReview": false,
  "iri": "iri://proj_001/requirement",
  "summary": "完成用户登录、商品浏览两大核心需求分析",
  "output": {
    "requirements": [
      { "id": "REQ-001", "title": "用户登录", "description": "支持邮箱+密码登录" },
      { "id": "REQ-002", "title": "商品浏览", "description": "支持分类浏览和搜索" }
    ]
  },
  "artifacts": [
    { "name": "需求规格说明书", "type": "document", "path": "/artifacts/proj_001/req-spec.pdf", "size": 1024000 }
  ],
  "errors": [],
  "contractSchema": "{\"type\":\"object\",\"properties\":{\"requirements\":{\"type\":\"array\"}}}",
  "timeoutSeconds": 600,
  "onFailure": "retry"
}
```

#### Artifact

```json
{
  "name": "需求规格说明书",
  "type": "document",
  "path": "/artifacts/proj_001/req-spec.pdf",
  "size": 1024000
}
```

#### StageError

```json
{
  "code": "LLM_TIMEOUT",
  "message": "LLM 请求超时",
  "timestamp": "2026-05-22T10:06:00Z"
}
```

#### PipelineResult

```json
{
  "taskId": "task_001",
  "projectId": "proj_001",
  "workflowId": "wf_abc123",
  "status": "running",
  "stages": [
    { "id": "stage_001", "taskId": "task_001", "name": "需求分析", "stageType": "requirement", "status": "success", "order": 1, ... },
    { "id": "stage_002", "taskId": "task_001", "name": "系统设计", "stageType": "design", "status": "running", "order": 2, ... }
  ],
  "summary": null,
  "totalDurationMs": null
}
```

#### PipelineInput

```json
{
  "pipelineName": "default",
  "config": {
    "timeoutSeconds": 3600,
    "maxRetries": 3
  }
}
```

#### PendingReview

```json
{
  "stageId": "stage_005",
  "taskId": "task_001",
  "projectId": "proj_001",
  "projectName": "电商平台",
  "stageName": "代码审查",
  "stageType": "review",
  "startedAt": "2026-05-22T11:00:00Z",
  "summary": "需要人工审查代码变更"
}
```

#### ReviewRecord

```json
{
  "id": "review_001",
  "stageId": "stage_005",
  "reviewer": "admin",
  "approved": true,
  "comment": "代码质量良好，批准通过",
  "createdAt": "2026-05-22T11:30:00Z"
}
```

#### ReviewDetail

```json
{
  "stageId": "stage_005",
  "taskId": "task_001",
  "projectId": "proj_001",
  "projectName": "电商平台",
  "stageName": "代码审查",
  "stageType": "review",
  "startedAt": "2026-05-22T11:00:00Z",
  "summary": "需要人工审查代码变更",
  "output": {
    "filesChanged": 5,
    "linesAdded": 120,
    "linesRemoved": 30
  },
  "artifacts": [
    { "name": "auth.go", "type": "code", "content": "package main\n\nfunc main() {}\n", "path": "/artifacts/proj_001/auth.go" }
  ],
  "history": [
    { "id": "review_001", "stageId": "stage_005", "reviewer": "admin", "approved": true, "comment": "批准", "createdAt": "2026-05-22T11:30:00Z" }
  ]
}
```

#### HumanReviewRequest

```json
{
  "approved": true,
  "comment": "代码质量良好，批准通过"
}
```

#### ChatMessage

```json
{
  "id": "msg_001",
  "role": "assistant",
  "content": [
    {
      "type": "text",
      "data": "以下是用户登录模块的代码实现："
    },
    {
      "type": "code",
      "data": {
        "code": "func Login(w http.ResponseWriter, r *http.Request) {\n\t// ...\n}",
        "language": "go"
      }
    },
    {
      "type": "diff",
      "data": {
        "oldCode": "func old() {}",
        "newCode": "func new() {}",
        "language": "go"
      }
    },
    {
      "type": "terminal",
      "data": {
        "log": "\u001b[32m✓ Build successful\u001b[0m\n\u001b[31m✗ Test failed\u001b[0m"
      }
    },
    {
      "type": "tool_call",
      "data": {
        "toolName": "read_file",
        "arguments": { "path": "/src/main.go" },
        "result": { "content": "package main" },
        "status": "success"
      }
    }
  ],
  "createdAt": "2026-05-22T10:30:00Z",
  "projectId": "proj_001",
  "stageId": "stage_003"
}
```

#### SendMessageRequest

```json
{
  "content": "请帮我生成用户登录的Go代码",
  "projectId": "proj_001",
  "stageId": "stage_003"
}
```

#### ServerConfig / LLMConfig / AgentOSConfig / RuntimeConfig

```json
// ServerConfig
{
  "apiBaseUrl": "http://localhost:8080",
  "wsBaseUrl": "ws://localhost:8080",
  "temporalHost": "temporal.example.com:7233"
}

// LLMConfig
{
  "apiKey": "sk-xxx",
  "baseUrl": "https://api.deepseek.com",
  "model": "deepseek-chat",
  "temperature": 0.7,
  "maxTokens": 4096
}

// AgentOSConfig
{
  "grpcAddress": "localhost:50051",
  "grpcTimeout": 30
}

// RuntimeConfig
{
  "defaultTimeout": 3600,
  "maxRetries": 3,
  "maxConcurrency": 5
}
```

#### ConfigValidationRequest

```json
{
  "type": "llm",
  "config": {
    "apiKey": "sk-xxx",
    "baseUrl": "https://api.deepseek.com",
    "model": "deepseek-chat",
    "temperature": 0.7,
    "maxTokens": 4096
  }
}
```

#### ValidationResult

```json
{
  "valid": true,
  "errors": []
}
```

当验证失败时：

```json
{
  "valid": false,
  "errors": [
    { "field": "apiKey", "message": "API Key 格式不正确" },
    { "field": "baseUrl", "message": "URL 无法访问" }
  ]
}
```

#### AgentOSStatus

```json
{
  "running": true,
  "version": "1.0.0",
  "grpcConnected": true,
  "uptime": 3600,
  "taskCount": 2
}
```

#### TemporalStatus

```json
{
  "connected": true,
  "namespace": "default",
  "workerCount": 3,
  "taskQueue": "sdlc-tasks",
  "pendingWorkflows": 1
}
```

#### SystemStatus

```json
{
  "agentOS": {
    "running": true,
    "version": "1.0.0",
    "grpcConnected": true,
    "uptime": 3600,
    "taskCount": 2
  },
  "temporal": {
    "connected": true,
    "namespace": "default",
    "workerCount": 3,
    "taskQueue": "sdlc-tasks",
    "pendingWorkflows": 1
  }
}
```

#### HealthCheckResult

```json
{
  "agentOS": {
    "healthy": true,
    "message": "Agent OS gRPC 连接正常"
  },
  "temporal": {
    "healthy": true,
    "message": "Temporal Server 连接正常"
  },
  "llm": {
    "healthy": true,
    "message": "LLM API 调用正常"
  },
  "overall": true
}
```

#### ResourceUsage

```json
{
  "cpuPercent": 45.2,
  "memoryUsedMB": 2048,
  "memoryTotalMB": 8192,
  "diskUsedGB": 50,
  "diskTotalGB": 200
}
```

#### ActiveTask

```json
{
  "taskId": "task_001",
  "projectId": "proj_001",
  "projectName": "电商平台",
  "stageName": "编码实现",
  "stageId": "stage_003",
  "progress": 65,
  "startedAt": "2026-05-22T10:30:00Z"
}
```

#### LogEntry

```json
{
  "id": "log_001",
  "timestamp": "2026-05-22T10:00:00Z",
  "level": "INFO",
  "message": "管线启动成功",
  "source": "pipeline",
  "metadata": {
    "taskId": "task_001",
    "workflowId": "wf_abc123"
  }
}
```

#### GraphNode

```json
{
  "id": "req-001",
  "label": "需求: 用户登录",
  "type": "Requirement",
  "iri": "iri://proj_001/requirement/REQ-001",
  "properties": {
    "priority": "high",
    "status": "completed"
  }
}
```

#### GraphEdge

```json
{
  "id": "e1",
  "source": "proj_001",
  "target": "req-001",
  "label": "hasRequirement",
  "type": "has"
}
```

#### GraphData

```json
{
  "nodes": [
    { "id": "proj_001", "label": "项目: 电商平台", "type": "Project" },
    { "id": "req-001", "label": "需求: 用户登录", "type": "Requirement" }
  ],
  "edges": [
    { "id": "e1", "source": "proj_001", "target": "req-001", "label": "hasRequirement", "type": "has" }
  ]
}
```

#### WorkflowSnapshot

```json
{
  "projectId": "proj_001",
  "projectName": "电商平台",
  "currentTask": {
    "id": "task_001",
    "status": "running",
    "startedAt": "2026-05-22T10:00:30Z"
  },
  "stages": [
    {
      "id": "stage_001",
      "name": "需求分析",
      "stageType": "requirement",
      "status": "success",
      "order": 1,
      "durationMs": 300000,
      "iri": "iri://proj_001/requirement/REQ-001"
    }
  ],
  "reviews": [
    { "stageId": "stage_005", "status": "reviewing" }
  ]
}
```

---

## 四、接口详情

### 4.1 项目接口

---

#### [1] GET /api/v1/projects - 获取项目列表

**请求参数**：无

**响应**：HTTP 200

```json
[
  {
    "id": "proj_001",
    "name": "电商平台",
    "description": "电商平台项目",
    "status": "active",
    "createdAt": "2026-05-22T10:00:00Z",
    "updatedAt": "2026-05-22T10:00:00Z"
  }
]
```

**前端调用**：`projectApi.list()`
- `api.get<ProjectMeta[]>('projects')`

---

#### [2] POST /api/v1/projects - 创建项目

**请求体**：

```json
{
  "name": "电商平台",
  "description": "电商平台项目"
}
```

**响应**：HTTP 200

```json
{
  "id": "proj_001",
  "name": "电商平台",
  "description": "电商平台项目",
  "status": "active",
  "createdAt": "2026-05-22T10:00:00Z",
  "updatedAt": "2026-05-22T10:00:00Z"
}
```

**前端调用**：`projectApi.create(input)`
- `api.post<ProjectMeta>('projects', input)`

---

#### [3] GET /api/v1/projects/:id - 获取项目详情

**路径参数**：
- `id` (string) - 项目 ID

**响应**：HTTP 200

```json
{
  "id": "proj_001",
  "name": "电商平台",
  "description": "电商平台项目",
  "status": "active",
  "createdAt": "2026-05-22T10:00:00Z",
  "updatedAt": "2026-05-22T10:00:00Z",
  "taskCount": 3,
  "lastTaskAt": "2026-05-22T11:00:00Z"
}
```

**前端调用**：`projectApi.get(id)`
- `api.get<ProjectDetail>('projects/${id}')`

**页面使用**：`ProjectDetail` 页面在加载时调用此接口获取项目信息显示

---

#### [4] DELETE /api/v1/projects/:id - 删除项目

**路径参数**：
- `id` (string) - 项目 ID

**响应**：HTTP 204（无 body）

**前端调用**：`projectApi.delete(id)`
- `api.delete('projects/${id}')`

---

### 4.2 任务接口

---

#### [5] GET /api/v1/projects/:id/tasks - 获取项目下所有任务

**路径参数**：
- `id` (string) - 项目 ID

**响应**：HTTP 200

```json
[
  {
    "id": "task_001",
    "projectId": "proj_001",
    "workflowId": "wf_abc123",
    "status": "running",
    "createdAt": "2026-05-22T10:00:00Z",
    "updatedAt": "2026-05-22T10:05:00Z",
    "startedAt": "2026-05-22T10:00:30Z",
    "completedAt": null,
    "totalDurationMs": null
  }
]
```

**前端调用**：`projectApi.getTasks(id)`
- `api.get('projects/${id}/tasks')`

---

#### [6] POST /api/v1/projects/:id/tasks - 创建并启动任务

**路径参数**：
- `id` (string) - 项目 ID

**请求体**：

```json
{
  "pipeline_name": "default"
}
```

**响应**：HTTP 200

```json
{
  "id": "task_001",
  "projectId": "proj_001",
  "workflowId": "wf_abc123",
  "status": "running",
  "createdAt": "2026-05-22T10:00:00Z",
  "updatedAt": "2026-05-22T10:00:00Z",
  "startedAt": "2026-05-22T10:00:30Z",
  "completedAt": null,
  "totalDurationMs": null
}
```

**前端调用**：`projectApi.createTask(id, input)`
- `api.post('projects/${id}/tasks', input)`

---

#### [7] GET /api/v1/tasks/:taskId - 获取任务详情

**路径参数**：
- `taskId` (string) - 任务 ID

**响应**：HTTP 200

```json
{
  "id": "task_001",
  "projectId": "proj_001",
  "workflowId": "wf_abc123",
  "status": "running",
  "createdAt": "2026-05-22T10:00:00Z",
  "updatedAt": "2026-05-22T10:05:00Z",
  "startedAt": "2026-05-22T10:00:30Z",
  "completedAt": null,
  "totalDurationMs": null
}
```

**前端调用**：`pipelineApi.getTask(taskId)`
- `api.get<TaskMeta>('tasks/${taskId}')`

---

#### [8] POST /api/v1/tasks/:taskId/retry - 重试任务

**路径参数**：
- `taskId` (string) - 任务 ID

**请求体**：无

**响应**：HTTP 200

```json
{
  "id": "task_001",
  "status": "running"
}
```

**前端调用**：`pipelineApi.retryTask(taskId)`
- `api.post('tasks/${taskId}/retry')`

---

#### [9] POST /api/v1/tasks/:taskId/rollback - 回退任务

**路径参数**：
- `taskId` (string) - 任务 ID

**请求体**：无

**响应**：HTTP 200

```json
{
  "id": "task_001",
  "status": "pending"
}
```

**前端调用**：`pipelineApi.rollbackTask(taskId)`
- `api.post('tasks/${taskId}/rollback')`

---

### 4.3 管线接口

---

#### [10] POST /api/v1/pipelines - 启动管线

**请求体**：

```json
{
  "pipelineName": "default",
  "config": {
    "timeoutSeconds": 3600,
    "maxRetries": 3
  }
}
```

**响应**：HTTP 200

```json
{
  "project_id": "proj_001",
  "task_id": "task_001",
  "workflow_id": "wf_abc123"
}
```

**前端调用**：`pipelineApi.start(input)`
- `api.post<{project_id, task_id, workflow_id}>('pipelines', input)`

---

#### [11] GET /api/v1/pipelines/:id - 获取管线结果

**路径参数**：
- `id` (string) - 管线/任务 ID

**响应**：HTTP 200

```json
{
  "taskId": "task_001",
  "projectId": "proj_001",
  "workflowId": "wf_abc123",
  "status": "running",
  "stages": [...],
  "summary": null,
  "totalDurationMs": null
}
```

**前端调用**：`pipelineApi.get(id)`
- `api.get<PipelineResult>('pipelines/${id}')`

---

### 4.4 阶段接口

---

#### [12] GET /api/v1/tasks/:taskId/stages - 获取阶段列表

**路径参数**：
- `taskId` (string) - 任务 ID

**响应**：HTTP 200

```json
[
  {
    "id": "stage_001",
    "taskId": "task_001",
    "name": "需求分析",
    "stageType": "requirement",
    "status": "success",
    "order": 1,
    "startedAt": "2026-05-22T10:00:30Z",
    "completedAt": "2026-05-22T10:05:30Z",
    "durationMs": 300000,
    "retryCount": 0,
    "hasAIReview": true,
    "hasHumanReview": false,
    "iri": "iri://proj_001/requirement"
  }
]
```

**前端调用**：`pipelineApi.getStages(taskId)`
- `api.get<StageInstanceMeta[]>('tasks/${taskId}/stages')`

---

#### [13] GET /api/v1/tasks/:taskId/stages/:stageId - 获取阶段详情

**路径参数**：
- `taskId` (string) - 任务 ID
- `stageId` (string) - 阶段 ID

**响应**：HTTP 200

```json
{
  "id": "stage_001",
  "taskId": "task_001",
  "name": "需求分析",
  "stageType": "requirement",
  "status": "success",
  "order": 1,
  "startedAt": "2026-05-22T10:00:30Z",
  "completedAt": "2026-05-22T10:05:30Z",
  "durationMs": 300000,
  "retryCount": 0,
  "hasAIReview": true,
  "hasHumanReview": false,
  "iri": "iri://proj_001/requirement",
  "summary": "完成用户登录、商品浏览两大核心需求分析",
  "output": {
    "requirements": [
      { "id": "REQ-001", "title": "用户登录" }
    ]
  },
  "artifacts": [
    { "name": "需求规格说明书", "type": "document", "path": "/artifacts/proj_001/req-spec.pdf", "size": 1024000 }
  ],
  "errors": [],
  "contractSchema": "{\"type\":\"object\",\"properties\":{\"requirements\":{\"type\":\"array\"}}}",
  "timeoutSeconds": 600,
  "onFailure": "retry"
}
```

**前端调用**：`pipelineApi.getStage(taskId, stageId)`
- `api.get<StageDetail>('tasks/${taskId}/stages/${stageId}')`

---

### 4.5 审查接口

---

#### [14] GET /api/v1/reviews/pending - 待审查列表

**请求参数**：无

**响应**：HTTP 200

```json
{
  "reviews": [
    {
      "stageId": "stage_005",
      "taskId": "task_001",
      "projectId": "proj_001",
      "projectName": "电商平台",
      "stageName": "代码审查",
      "stageType": "review",
      "startedAt": "2026-05-22T11:00:00Z",
      "summary": "需要人工审查代码变更"
    }
  ]
}
```

**前端调用**：`reviewApi.getPending()`
- `api.get<{reviews: PendingReview[]}>('reviews/pending')`

---

#### [15] GET /api/v1/reviews/:stageId - 审查详情

**路径参数**：
- `stageId` (string) - 阶段 ID

**响应**：HTTP 200

```json
{
  "stageId": "stage_005",
  "taskId": "task_001",
  "projectId": "proj_001",
  "projectName": "电商平台",
  "stageName": "代码审查",
  "stageType": "review",
  "startedAt": "2026-05-22T11:00:00Z",
  "summary": "需要人工审查代码变更",
  "output": {
    "filesChanged": 5,
    "linesAdded": 120,
    "linesRemoved": 30
  },
  "artifacts": [
    { "name": "auth.go", "type": "code", "content": "package main\n\nfunc main() {}", "path": "/artifacts/proj_001/auth.go" }
  ],
  "history": []
}
```

**前端调用**：`reviewApi.getDetail(stageId)`
- `api.get<ReviewDetail>('reviews/${stageId}')`

---

#### [16] POST /api/v1/reviews/:stageId/submit - 提交审查

**路径参数**：
- `stageId` (string) - 阶段 ID

**请求体**：

```json
{
  "approved": true,
  "comment": "代码质量良好，批准通过"
}
```

**响应**：HTTP 200

```json
{
  "status": "success",
  "approved": true
}
```

**前端调用**：`reviewApi.submit(stageId, request)`
- `api.post<{status, approved}>('reviews/${stageId}/submit', request)`

**说明**：提交审查后，后端需要触发 Temporal Signal，通知 Agent OS 继续管线执行。如果 `approved = true`，则推进到下一阶段；如果 `approved = false`，则触发回退或重新生成。

---

#### [17] GET /api/v1/reviews/:stageId/history - 审查历史

**路径参数**：
- `stageId` (string) - 阶段 ID

**响应**：HTTP 200

```json
{
  "reviews": [
    {
      "id": "review_001",
      "stageId": "stage_005",
      "reviewer": "admin",
      "approved": true,
      "comment": "代码质量良好，批准通过",
      "createdAt": "2026-05-22T11:30:00Z"
    }
  ]
}
```

**前端调用**：`reviewApi.getHistory(stageId)`
- `api.get<{reviews: ReviewRecord[]}>('reviews/${stageId}/history')`

---

### 4.6 对话接口

---

#### [18] POST /api/v1/chat/send - 发送对话消息

**请求体**：

```json
{
  "content": "请帮我生成用户登录的Go代码",
  "projectId": "proj_001",
  "stageId": "stage_003"
}
```

**响应**：HTTP 200（SSE 流式响应）

响应采用 **Server-Sent Events (SSE)** 模式，支持流式输出。前端通过 EventSource 或 fetch streaming 消费。

```
data: {"type": "text", "data": "以下是用户登录模块的代码实现：\n\n"}

data: {"type": "code", "data": {"code": "func Login(w http.ResponseWriter, r *http.Request) {\n\t// ...\n}", "language": "go"}}

data: {"type": "tool_call", "data": {"toolName": "read_file", "arguments": {"path": "/src/main.go"}, "result": null, "status": "pending"}}

data: [DONE]
```

**Event 格式**：

每条 SSE data 是一个 JSON 对象，对应 `MessageContent` 结构：

| 字段 | 类型 | 说明 |
|------|------|------|
| `type` | string | `text`、`code`、`diff`、`terminal`、`tool_call` 之一 |
| `data` | any | 根据 type 不同，结构不同 |

前端收完所有事件（收到 `[DONE]` 标记）后，结束流式加载。

**前端调用**：`chatStore.sendMessage(input)`
- 使用 fetch + SSE 方式接收流式回复
- 每次收到一个 `data:` 行立即解析并追加到消息列表中
- 收到 `[DONE]` 后结束

---

#### [19] GET /api/v1/chat/history - 获取对话历史

**查询参数**：
- `projectId` (string, 可选) - 项目 ID，过滤该项目的对话
- `page` (number, 可选) - 页码，默认 1
- `pageSize` (number, 可选) - 每页条数，默认 50

**响应**：HTTP 200

```json
{
  "messages": [
    {
      "id": "msg_001",
      "role": "assistant",
      "content": [
        {
          "type": "text",
          "data": "以下是用户登录模块的代码实现："
        }
      ],
      "createdAt": "2026-05-22T10:30:00Z",
      "projectId": "proj_001",
      "stageId": "stage_003"
    }
  ],
  "total": 100,
  "page": 1,
  "pageSize": 50
}
```

**前端调用**：用于实现对话历史加载，当前 chatStore 中有 `loadHistory` 方法。

---

### 4.7 配置接口

---

#### [20] GET /api/v1/config/llm - 获取 LLM 配置

**请求参数**：无

**响应**：HTTP 200

```json
{
  "apiKey": "",
  "baseUrl": "https://api.deepseek.com",
  "model": "deepseek-chat",
  "temperature": 0.7,
  "maxTokens": 4096
}
```

**安全说明**：`apiKey` 字段在返回时必须脱敏处理（如返回空字符串或 `sk-***`）。

**前端调用**：`settingsApi.getLLMConfig()`
- `api.get<LLMConfig>('config/llm')`

---

#### [21] POST /api/v1/config/llm - 保存 LLM 配置

**请求体**：

```json
{
  "apiKey": "sk-xxx",
  "baseUrl": "https://api.deepseek.com",
  "model": "deepseek-chat",
  "temperature": 0.7,
  "maxTokens": 4096
}
```

**响应**：HTTP 200

```json
{
  "success": true
}
```

**说明**：后端收到后应将配置持久化（文件或数据库），并在运行时生效。

---

#### [22] POST /api/v1/config/validate - 验证配置有效性

**请求体**：

```json
{
  "type": "llm",
  "config": {
    "apiKey": "sk-xxx",
    "baseUrl": "https://api.deepseek.com",
    "model": "deepseek-chat",
    "temperature": 0.7,
    "maxTokens": 4096
  }
}
```

**`type` 可选值**：`server`、`llm`、`agentOS`、`runtime`

**响应**：HTTP 200

```json
{
  "valid": true,
  "errors": []
}
```

验证失败：

```json
{
  "valid": false,
  "errors": [
    { "field": "apiKey", "message": "API Key 格式不正确" }
  ]
}
```

**验证逻辑建议**：
- `type = server`：测试 Temporal 连接
- `type = llm`：测试 LLM API 调用
- `type = agentOS`：测试 gRPC 连接
- `type = runtime`：检查参数合理性

---

### 4.8 监控接口

---

#### [23] GET /api/v1/system/status - 系统状态

**请求参数**：无

**响应**：HTTP 200

```json
{
  "agentOS": {
    "running": true,
    "version": "1.0.0",
    "grpcConnected": true,
    "uptime": 3600,
    "taskCount": 2
  },
  "temporal": {
    "connected": true,
    "namespace": "default",
    "workerCount": 3,
    "taskQueue": "sdlc-tasks",
    "pendingWorkflows": 1
  }
}
```

**前端调用**：`monitorApi.getSystemStatus()`
- `api.get<SystemStatus>('system/status')`
- 前端 30 秒轮询一次该接口

---

#### [24] GET /api/v1/system/health - 健康检查

**请求参数**：无

**响应**：HTTP 200

```json
{
  "agentOS": {
    "healthy": true,
    "message": "Agent OS gRPC 连接正常"
  },
  "temporal": {
    "healthy": true,
    "message": "Temporal Server 连接正常"
  },
  "llm": {
    "healthy": true,
    "message": "LLM API 调用正常"
  },
  "overall": true
}
```

**前端调用**：`monitorApi.getHealth()`
- `api.get<HealthCheckResult>('system/health')`

---

#### [25] GET /api/v1/system/resources - 资源使用

**请求参数**：无

**响应**：HTTP 200

```json
{
  "cpuPercent": 45.2,
  "memoryUsedMB": 2048,
  "memoryTotalMB": 8192,
  "diskUsedGB": 50,
  "diskTotalGB": 200
}
```

**前端调用**：`monitorApi.getResources()`
- `api.get<ResourceUsage>('system/resources')`

---

#### [26] GET /api/v1/system/active-tasks - 活跃任务列表

**请求参数**：无

**响应**：HTTP 200

```json
[
  {
    "taskId": "task_001",
    "projectId": "proj_001",
    "projectName": "电商平台",
    "stageName": "编码实现",
    "stageId": "stage_003",
    "progress": 65,
    "startedAt": "2026-05-22T10:30:00Z"
  }
]
```

`progress` 字段取值范围 0-100，表示当前阶段的执行进度百分比。

**前端调用**：`monitorApi.getActiveTasks()`
- `api.get<ActiveTask[]>('system/active-tasks')`

---

### 4.9 日志接口

---

#### [27] GET /api/v1/logs/system - 系统日志

**查询参数**（均为可选）：
- `level` (string) - 日志级别：`DEBUG`、`INFO`、`WARN`、`ERROR`
- `since` (string) - 起始时间（ISO 8601）
- `keyword` (string) - 关键词搜索

**响应**：HTTP 200

```json
[
  {
    "id": "log_001",
    "timestamp": "2026-05-22T10:00:00Z",
    "level": "INFO",
    "message": "管线启动成功",
    "source": "pipeline",
    "metadata": {
      "taskId": "task_001"
    }
  }
]
```

**前端调用**：`logsApi.getSystemLogs(filter)`
- `api.get<LogEntry[]>('logs/system', params)`

---

#### [28] GET /api/v1/logs/stage/:taskId/:stageId - 阶段执行日志

**路径参数**：
- `taskId` (string) - 任务 ID
- `stageId` (string) - 阶段 ID

**响应**：HTTP 200

```json
[
  {
    "id": "log_010",
    "timestamp": "2026-05-22T10:02:00Z",
    "level": "INFO",
    "message": "开始执行需求分析阶段",
    "source": "stage_001",
    "metadata": {
      "taskId": "task_001",
      "stageId": "stage_001"
    }
  }
]
```

**前端调用**：`logsApi.getStageLogs(taskId, stageId)`
- `api.get<LogEntry[]>('logs/stage/${taskId}/${stageId}')`

---

#### [29] GET /api/v1/logs/agent-os - Agent OS 事件日志

**查询参数**（可选）：
- `since` (string) - 起始时间（ISO 8601）

**响应**：HTTP 200

```json
[
  {
    "id": "log_020",
    "timestamp": "2026-05-22T10:00:00Z",
    "level": "INFO",
    "message": "Agent OS 收到新的任务指令",
    "source": "agent_os",
    "metadata": {
      "eventType": "task_received",
      "taskId": "task_001"
    }
  }
]
```

**前端调用**：`logsApi.getAgentOSLogs(since)`
- `api.get<LogEntry[]>('logs/agent-os', params)`

---

### 4.10 图谱接口

---

#### [30] GET /api/v1/projects/:id/graph - 项目图数据

**路径参数**：
- `id` (string) - 项目 ID

**响应**：HTTP 200

```json
{
  "nodes": [
    { "id": "proj_001", "label": "项目: 电商平台", "type": "Project", "iri": "iri://project/1" },
    { "id": "req-001", "label": "需求: 用户登录", "type": "Requirement", "iri": "iri://req/1" },
    { "id": "design-001", "label": "设计: 登录模块", "type": "Design", "iri": "iri://design/1" }
  ],
  "edges": [
    { "id": "e1", "source": "proj_001", "target": "req-001", "label": "hasRequirement", "type": "has" },
    { "id": "e2", "source": "req-001", "target": "design-001", "label": "designBy", "type": "derives" }
  ]
}
```

**前端调用**：`graphApi.getProjectGraph(projectId)`
- `api.get<GraphData>('projects/${projectId}/graph')`

**前端 AntV G6 消费说明**：
- 每个 `node` 会映射为 G6 节点对象，`type` 字段用于区分节点颜色（Project=蓝、Requirement=紫、Design=青、Code=绿、Test=橙）
- `iri` 字段可选，用于展示节点 IRI 超链接
- `properties` 字段可选，用于在详情抽屉中展示附加属性

---

### 4.11 快照接口

---

#### [31] GET /api/v1/projects/:id/snapshot - 项目工作流快照

**路径参数**：
- `id` (string) - 项目 ID

**响应**：HTTP 200

```json
{
  "projectId": "proj_001",
  "projectName": "电商平台",
  "currentTask": {
    "id": "task_001",
    "status": "running",
    "startedAt": "2026-05-22T10:00:30Z"
  },
  "stages": [
    {
      "id": "stage_001",
      "name": "需求分析",
      "stageType": "requirement",
      "status": "success",
      "order": 1,
      "durationMs": 300000,
      "iri": "iri://proj_001/requirement/REQ-001"
    },
    {
      "id": "stage_002",
      "name": "系统设计",
      "stageType": "design",
      "status": "running",
      "order": 2,
      "durationMs": null,
      "iri": null
    }
  ],
  "reviews": [
    { "stageId": "stage_005", "status": "reviewing" }
  ]
}
```

**前端调用**：`projectApi.getSnapshot(id)`
- `api.get('projects/${id}/snapshot')`

---

## 五、WebSocket 事件协议

### 5.1 连接

```
ws://localhost:8080/ws?project_id=proj_001
```

前端 `WebSocketManager` 负责管理与后端的 WebSocket 连接，并支持自动断线重连（最多 5 次，指数退避）。

### 5.2 通用事件格式

所有推送事件采用统一 JSON 格式：

```json
{
  "type": "event_type",
  "projectId": "proj_001",
  "payload": {},
  "timestamp": 1716000000000
}
```

### 5.3 事件类型清单

| 事件类型 | Payload | 触发时机 | 前端处理 |
|---------|---------|---------|---------|
| `pipeline_started` | `{project_id, task_id, workflow_id}` | 管线启动时 | 更新任务状态为 running |
| `stage_started` | `{stage_id, stage_type, name}` | 阶段开始时 | 更新阶段状态为 running |
| `stage_completed` | `{stage_id, status, iri, duration_ms}` | 阶段完成时 | 更新阶段状态为 success/failed |
| `stage_failed` | `{stage_id, errors: [{code, message}]}` | 阶段失败时 | 更新阶段状态为 failed，显示错误信息 |
| `stage_ai_review` | `{stage_id, approved, score}` | AI 评审完成时 | 显示 AI 评审结果 |
| `stage_human_review_required` | `{stage_id, name}` | 需要人工审查时 | 更新阶段状态为 reviewing |
| `pipeline_completed` | `{status, summary, total_duration_ms}` | 管线完成时 | 更新管线状态为 completed/failed |
| `stage_progress` | `{stage_id, progress, message}` | 阶段进度更新 | 更新进度条 |
| `agent_os_event` | `{event_type, data}` | Agent OS 内部事件 | 展示事件日志 |

### 5.4 事件 Payload 详细格式

#### pipeline_started

```json
{
  "type": "pipeline_started",
  "projectId": "proj_001",
  "payload": {
    "project_id": "proj_001",
    "task_id": "task_001",
    "workflow_id": "wf_abc123"
  },
  "timestamp": 1716000000000
}
```

#### stage_started

```json
{
  "type": "stage_started",
  "projectId": "proj_001",
  "payload": {
    "stage_id": "stage_002",
    "stage_type": "design",
    "name": "系统设计"
  },
  "timestamp": 1716000300000
}
```

#### stage_completed

```json
{
  "type": "stage_completed",
  "projectId": "proj_001",
  "payload": {
    "stage_id": "stage_001",
    "status": "success",
    "iri": "iri://proj_001/requirement",
    "duration_ms": 300000
  },
  "timestamp": 1716000330000
}
```

#### stage_failed

```json
{
  "type": "stage_failed",
  "projectId": "proj_001",
  "payload": {
    "stage_id": "stage_003",
    "errors": [
      { "code": "COMPILATION_ERROR", "message": "代码编译失败：未定义的变量 'x'" }
    ]
  },
  "timestamp": 1716000400000
}
```

#### stage_ai_review

```json
{
  "type": "stage_ai_review",
  "projectId": "proj_001",
  "payload": {
    "stage_id": "stage_004",
    "approved": true,
    "score": 85
  },
  "timestamp": 1716000500000
}
```

#### stage_human_review_required

```json
{
  "type": "stage_human_review_required",
  "projectId": "proj_001",
  "payload": {
    "stage_id": "stage_005",
    "name": "代码审查"
  },
  "timestamp": 1716000600000
}
```

#### pipeline_completed

```json
{
  "type": "pipeline_completed",
  "projectId": "proj_001",
  "payload": {
    "status": "completed",
    "summary": "所有阶段执行完成",
    "total_duration_ms": 3600000
  },
  "timestamp": 1716003600000
}
```

#### stage_progress

```json
{
  "type": "stage_progress",
  "projectId": "proj_001",
  "payload": {
    "stage_id": "stage_003",
    "progress": 65,
    "message": "正在生成代码..."
  },
  "timestamp": 1716000350000
}
```

#### agent_os_event

```json
{
  "type": "agent_os_event",
  "projectId": "proj_001",
  "payload": {
    "event_type": "llm_request",
    "data": {
      "model": "deepseek-chat",
      "tokens": 1500,
      "duration_ms": 2500
    }
  },
  "timestamp": 1716000355000
}
```

---

## 六、Go 后端路由实现建议

### 6.1 路由注册（Gin 框架）

```go
package router

import (
    "github.com/gin-gonic/gin"
    "your-project/handler"
)

func SetupRouter() *gin.Engine {
    r := gin.Default()

    v1 := r.Group("/api/v1")
    {
        // 项目
        v1.GET("/projects", handler.ListProjects)
        v1.POST("/projects", handler.CreateProject)
        v1.GET("/projects/:id", handler.GetProject)
        v1.DELETE("/projects/:id", handler.DeleteProject)

        // 项目-任务
        v1.GET("/projects/:id/tasks", handler.ListTasks)
        v1.POST("/projects/:id/tasks", handler.CreateTask)

        // 项目-图谱/快照
        v1.GET("/projects/:id/graph", handler.GetProjectGraph)
        v1.GET("/projects/:id/snapshot", handler.GetProjectSnapshot)

        // 任务
        v1.GET("/tasks/:taskId", handler.GetTask)
        v1.POST("/tasks/:taskId/retry", handler.RetryTask)
        v1.POST("/tasks/:taskId/rollback", handler.RollbackTask)

        // 管线
        v1.POST("/pipelines", handler.StartPipeline)
        v1.GET("/pipelines/:id", handler.GetPipeline)

        // 阶段
        v1.GET("/tasks/:taskId/stages", handler.ListStages)
        v1.GET("/tasks/:taskId/stages/:stageId", handler.GetStage)

        // 审查
        v1.GET("/reviews/pending", handler.ListPendingReviews)
        v1.GET("/reviews/:stageId", handler.GetReviewDetail)
        v1.POST("/reviews/:stageId/submit", handler.SubmitReview)
        v1.GET("/reviews/:stageId/history", handler.GetReviewHistory)

        // 对话
        v1.POST("/chat/send", handler.SendChatMessage)
        v1.GET("/chat/history", handler.GetChatHistory)

        // 配置
        v1.GET("/config/llm", handler.GetLLMConfig)
        v1.POST("/config/llm", handler.SaveLLMConfig)
        v1.POST("/config/validate", handler.ValidateConfig)

        // 监控
        v1.GET("/system/status", handler.GetSystemStatus)
        v1.GET("/system/health", handler.HealthCheck)
        v1.GET("/system/resources", handler.GetResourceUsage)
        v1.GET("/system/active-tasks", handler.GetActiveTasks)

        // 日志
        v1.GET("/logs/system", handler.GetSystemLogs)
        v1.GET("/logs/stage/:taskId/:stageId", handler.GetStageLogs)
        v1.GET("/logs/agent-os", handler.GetAgentOSLogs)
    }

    // WebSocket
    r.GET("/ws", handler.WebSocketHandler)

    return r
}
```

### 6.2 WebSocket Hub 实现建议

```go
package ws

import (
    "sync"
    "github.com/gorilla/websocket"
)

type Client struct {
    ProjectID string
    Conn      *websocket.Conn
    Send      chan []byte
}

type Hub struct {
    clients    map[*Client]bool
    broadcast  chan []byte
    register   chan *Client
    unregister chan *Client
    mu         sync.RWMutex
}

func NewHub() *Hub {
    return &Hub{
        clients:    make(map[*Client]bool),
        broadcast:  make(chan []byte, 256),
        register:   make(chan *Client),
        unregister: make(chan *Client),
    }
}

// BroadcastToProject 发送事件到指定项目的所有客户端
func (h *Hub) BroadcastToProject(projectID string, event []byte) {
    h.mu.RLock()
    defer h.mu.RUnlock()
    for client := range h.clients {
        if client.ProjectID == projectID {
            select {
            case client.Send <- event:
            default:
                close(client.Send)
                delete(h.clients, client)
            }
        }
    }
}
```

### 6.3 跨域配置

```go
func SetupRouter() *gin.Engine {
    r := gin.Default()
    
    r.Use(cors.New(cors.Config{
        AllowOrigins:     []string{"http://localhost:5173"},
        AllowMethods:     []string{"GET", "POST", "PUT", "DELETE", "OPTIONS"},
        AllowHeaders:     []string{"Origin", "Content-Type", "Authorization"},
        AllowCredentials: true,
    }))
    
    // ...
}
```

### 6.4 SSE 对话实现建议

`POST /api/v1/chat/send` 接口需要使用 **SSE (Server-Sent Events)** 协议，前端使用 fetch streaming 消费。

```go
func SendChatMessage(c *gin.Context) {
    var req SendMessageRequest
    if err := c.ShouldBindJSON(&req); err != nil {
        c.JSON(400, gin.H{"message": err.Error()})
        return
    }

    c.Writer.Header().Set("Content-Type", "text/event-stream")
    c.Writer.Header().Set("Cache-Control", "no-cache")
    c.Writer.Header().Set("Connection", "keep-alive")
    c.Writer.Header().Set("Access-Control-Allow-Origin", "*")

    // 模拟流式输出
    for _, chunk := range generateResponse(req.Content) {
        data, _ := json.Marshal(chunk)
        fmt.Fprintf(c.Writer, "data: %s\n\n", data)
        c.Writer.Flush()
        time.Sleep(100 * time.Millisecond)
    }

    // 结束标记
    fmt.Fprintf(c.Writer, "data: [DONE]\n\n")
    c.Writer.Flush()
}
```

---

## 七、Go Go Struct 定义示例（供参考）

```go
package model

import "time"

// ============ 项目 ============

type ProjectStatus string

const (
    ProjectStatusActive   ProjectStatus = "active"
    ProjectStatusArchived ProjectStatus = "archived"
    ProjectStatusDeleted  ProjectStatus = "deleted"
)

type ProjectMeta struct {
    ID          string        `json:"id"`
    Name        string        `json:"name"`
    Description string        `json:"description"`
    Status      ProjectStatus `json:"status"`
    CreatedAt   time.Time     `json:"createdAt"`
    UpdatedAt   time.Time     `json:"updatedAt"`
}

type ProjectDetail struct {
    ProjectMeta
    TaskCount int        `json:"taskCount"`
    LastTaskAt *time.Time `json:"lastTaskAt,omitempty"`
}

type CreateProjectInput struct {
    Name        string `json:"name"`
    Description string `json:"description"`
}

// ============ 任务 ============

type TaskStatus string

const (
    TaskStatusPending   TaskStatus = "pending"
    TaskStatusRunning   TaskStatus = "running"
    TaskStatusCompleted TaskStatus = "completed"
    TaskStatusFailed    TaskStatus = "failed"
    TaskStatusPaused    TaskStatus = "paused"
)

type TaskMeta struct {
    ID              string     `json:"id"`
    ProjectID       string     `json:"projectId"`
    WorkflowID      string     `json:"workflowId"`
    Status          TaskStatus `json:"status"`
    CreatedAt       time.Time  `json:"createdAt"`
    UpdatedAt       time.Time  `json:"updatedAt"`
    StartedAt       *time.Time `json:"startedAt,omitempty"`
    CompletedAt     *time.Time `json:"completedAt,omitempty"`
    TotalDurationMs *int64     `json:"totalDurationMs,omitempty"`
}

// ============ 阶段 ============

type StageType string

const (
    StageTypeRequirement StageType = "requirement"
    StageTypeDesign      StageType = "design"
    StageTypeCoding      StageType = "coding"
    StageTypeTesting     StageType = "testing"
    StageTypeReview      StageType = "review"
    StageTypeCICD        StageType = "cicd"
    StageTypeDeploy      StageType = "deploy"
)

type StageStatus string

const (
    StageStatusPending   StageStatus = "pending"
    StageStatusRunning   StageStatus = "running"
    StageStatusSuccess   StageStatus = "success"
    StageStatusFailed    StageStatus = "failed"
    StageStatusReviewing StageStatus = "reviewing"
    StageStatusSkipped   StageStatus = "skipped"
)

type FailurePolicy string

const (
    FailurePolicyFail     FailurePolicy = "fail"
    FailurePolicyRetry    FailurePolicy = "retry"
    FailurePolicySkip     FailurePolicy = "skip"
    FailurePolicyRollback FailurePolicy = "rollback"
)

type StageInstanceMeta struct {
    ID             string       `json:"id"`
    TaskID         string       `json:"taskId"`
    Name           string       `json:"name"`
    StageType      StageType    `json:"stageType"`
    Status         StageStatus  `json:"status"`
    Order          int          `json:"order"`
    StartedAt      *time.Time   `json:"startedAt,omitempty"`
    CompletedAt    *time.Time   `json:"completedAt,omitempty"`
    DurationMs     *int64       `json:"durationMs,omitempty"`
    RetryCount     int          `json:"retryCount"`
    HasAIReview    bool         `json:"hasAIReview"`
    HasHumanReview bool         `json:"hasHumanReview"`
    IRI            string       `json:"iri,omitempty"`
}

type StageDetail struct {
    StageInstanceMeta
    Summary        string                 `json:"summary,omitempty"`
    Output         map[string]interface{} `json:"output,omitempty"`
    Artifacts      []Artifact             `json:"artifacts,omitempty"`
    Errors         []StageError           `json:"errors,omitempty"`
    ContractSchema string                 `json:"contractSchema,omitempty"`
    TimeoutSeconds int                    `json:"timeoutSeconds"`
    OnFailure      FailurePolicy          `json:"onFailure"`
}

type Artifact struct {
    Name string `json:"name"`
    Type string `json:"type"`
    Path string `json:"path"`
    Size int64  `json:"size,omitempty"`
}

type StageError struct {
    Code      string    `json:"code"`
    Message   string    `json:"message"`
    Timestamp time.Time `json:"timestamp"`
}

// ============ 管线 ============

type PipelineInput struct {
    PipelineName string                 `json:"pipelineName"`
    Config       map[string]interface{} `json:"config,omitempty"`
}

type PipelineResult struct {
    TaskID          string              `json:"taskId"`
    ProjectID       string              `json:"projectId"`
    WorkflowID      string              `json:"workflowId"`
    Status          TaskStatus          `json:"status"`
    Stages          []StageInstanceMeta `json:"stages"`
    Summary         string              `json:"summary,omitempty"`
    TotalDurationMs *int64              `json:"totalDurationMs,omitempty"`
}

// ============ 审查 ============

type PendingReview struct {
    StageID     string `json:"stageId"`
    TaskID      string `json:"taskId"`
    ProjectID   string `json:"projectId"`
    ProjectName string `json:"projectName"`
    StageName   string `json:"stageName"`
    StageType   string `json:"stageType"`
    StartedAt   string `json:"startedAt"`
    Summary     string `json:"summary,omitempty"`
}

type ReviewRecord struct {
    ID        string    `json:"id"`
    StageID   string    `json:"stageId"`
    Reviewer  string    `json:"reviewer,omitempty"`
    Approved  bool      `json:"approved"`
    Comment   string    `json:"comment,omitempty"`
    CreatedAt time.Time `json:"createdAt"`
}

type HumanReviewRequest struct {
    Approved bool   `json:"approved"`
    Comment  string `json:"comment,omitempty"`
}

type ReviewDetail struct {
    PendingReview
    Output    map[string]interface{} `json:"output,omitempty"`
    Artifacts []ReviewArtifact       `json:"artifacts,omitempty"`
    History   []ReviewRecord         `json:"history"`
}

type ReviewArtifact struct {
    Name    string `json:"name"`
    Type    string `json:"type"`
    Content string `json:"content,omitempty"`
    Path    string `json:"path"`
}

// ============ 对话 ============

type MessageRole string
type MessageContentType string

type SendMessageRequest struct {
    Content   string `json:"content"`
    ProjectID string `json:"projectId,omitempty"`
    StageID   string `json:"stageId,omitempty"`
}

type ChatMessage struct {
    ID        string           `json:"id"`
    Role      MessageRole      `json:"role"`
    Content   []MessageContent `json:"content"`
    CreatedAt time.Time        `json:"createdAt"`
    ProjectID string           `json:"projectId,omitempty"`
    StageID   string           `json:"stageId,omitempty"`
}

type MessageContent struct {
    Type MessageContentType `json:"type"`
    Data interface{}        `json:"data"`
}

type ChatHistoryResponse struct {
    Messages []ChatMessage `json:"messages"`
    Total    int           `json:"total"`
    Page     int           `json:"page"`
    PageSize int           `json:"pageSize"`
}

// ============ 配置 ============

type ServerConfig struct {
    ApiBaseUrl   string `json:"apiBaseUrl"`
    WsBaseUrl    string `json:"wsBaseUrl"`
    TemporalHost string `json:"temporalHost"`
}

type LLMConfig struct {
    ApiKey      string  `json:"apiKey"`
    BaseUrl     string  `json:"baseUrl"`
    Model       string  `json:"model"`
    Temperature float64 `json:"temperature"`
    MaxTokens   int     `json:"maxTokens"`
}

type AgentOSConfig struct {
    GrpcAddress string `json:"grpcAddress"`
    GrpcTimeout int    `json:"grpcTimeout"`
}

type RuntimeConfig struct {
    DefaultTimeout int `json:"defaultTimeout"`
    MaxRetries     int `json:"maxRetries"`
    MaxConcurrency int `json:"maxConcurrency"`
}

type ConfigValidationRequest struct {
    Type   string      `json:"type"`
    Config interface{} `json:"config"`
}

type ValidationResult struct {
    Valid  bool            `json:"valid"`
    Errors []FieldError    `json:"errors"`
}

type FieldError struct {
    Field   string `json:"field"`
    Message string `json:"message"`
}

// ============ 监控 ============

type AgentOSStatus struct {
    Running       bool   `json:"running"`
    Version       string `json:"version"`
    GrpcConnected bool   `json:"grpcConnected"`
    Uptime        int64  `json:"uptime"`
    TaskCount     int    `json:"taskCount"`
}

type TemporalStatus struct {
    Connected        bool   `json:"connected"`
    Namespace        string `json:"namespace"`
    WorkerCount      int    `json:"workerCount"`
    TaskQueue        string `json:"taskQueue"`
    PendingWorkflows int    `json:"pendingWorkflows"`
}

type SystemStatus struct {
    AgentOS  AgentOSStatus  `json:"agentOS"`
    Temporal TemporalStatus `json:"temporal"`
}

type HealthCheckResult struct {
    AgentOS  ComponentHealth `json:"agentOS"`
    Temporal ComponentHealth `json:"temporal"`
    LLM      ComponentHealth `json:"llm"`
    Overall  bool            `json:"overall"`
}

type ComponentHealth struct {
    Healthy bool   `json:"healthy"`
    Message string `json:"message"`
}

type ResourceUsage struct {
    CPUPercent    float64 `json:"cpuPercent"`
    MemoryUsedMB  int64   `json:"memoryUsedMB"`
    MemoryTotalMB int64   `json:"memoryTotalMB"`
    DiskUsedGB    int64   `json:"diskUsedGB"`
    DiskTotalGB   int64   `json:"diskTotalGB"`
}

type ActiveTask struct {
    TaskID      string  `json:"taskId"`
    ProjectID   string  `json:"projectId"`
    ProjectName string  `json:"projectName"`
    StageName   string  `json:"stageName"`
    StageID     string  `json:"stageId"`
    Progress    float64 `json:"progress"`
    StartedAt   string  `json:"startedAt"`
}

// ============ 日志 ============

type LogLevel string

type LogEntry struct {
    ID        string                 `json:"id"`
    Timestamp time.Time              `json:"timestamp"`
    Level     LogLevel               `json:"level"`
    Message   string                 `json:"message"`
    Source    string                 `json:"source"`
    Metadata  map[string]interface{} `json:"metadata,omitempty"`
}

// ============ 图谱 ============

type GraphNode struct {
    ID         string                 `json:"id"`
    Label      string                 `json:"label"`
    Type       string                 `json:"type"`
    IRI        string                 `json:"iri,omitempty"`
    Properties map[string]interface{} `json:"properties,omitempty"`
}

type GraphEdge struct {
    ID     string `json:"id"`
    Source string `json:"source"`
    Target string `json:"target"`
    Label  string `json:"label"`
    Type   string `json:"type"`
}

type GraphData struct {
    Nodes []GraphNode `json:"nodes"`
    Edges []GraphEdge `json:"edges"`
}

// ============ 快照 ============

type WorkflowSnapshot struct {
    ProjectID   string           `json:"projectId"`
    ProjectName string           `json:"projectName"`
    CurrentTask *SnapshotTask    `json:"currentTask,omitempty"`
    Stages      []SnapshotStage  `json:"stages"`
    Reviews     []SnapshotReview `json:"reviews"`
}

type SnapshotTask struct {
    ID        string    `json:"id"`
    Status    TaskStatus `json:"status"`
    StartedAt time.Time `json:"startedAt"`
}

type SnapshotStage struct {
    ID         string     `json:"id"`
    Name       string     `json:"name"`
    StageType  StageType  `json:"stageType"`
    Status     StageStatus `json:"status"`
    Order      int        `json:"order"`
    DurationMs *int64     `json:"durationMs,omitempty"`
    IRI        *string    `json:"iri"`
}

type SnapshotReview struct {
    StageID string      `json:"stageId"`
    Status  StageStatus `json:"status"`
}

// ============ WebSocket 事件 ============

type WSEvent struct {
    Type      string                 `json:"type"`
    ProjectID string                 `json:"projectId"`
    Payload   map[string]interface{} `json:"payload"`
    Timestamp int64                  `json:"timestamp"`
}
```

---

## 八、错误码对照表

| HTTP 状态码 | 说明 | 可能原因 |
|------------|------|---------|
| 200 | 请求成功 | - |
| 204 | 请求成功（无内容） | DELETE 操作 |
| 400 | 请求参数错误 | JSON 解析失败、字段校验失败 |
| 404 | 资源不存在 | 项目/任务/阶段 ID 不存在 |
| 500 | 服务器内部错误 | 数据库连接失败、Agent OS gRPC 调用失败 |

错误响应 body 格式：

```json
{
  "message": "项目不存在"
}
```

---

## 九、注意事项

1. **所有 ID 字段**：建议使用字符串类型，如 `proj_001`、`task_001`、`stage_001`，便于前端展示和调试
2. **分页**：列表接口（项目列表、对话历史等）应在后续版本支持 `page` 和 `pageSize` 参数
3. **SSE 流式响应**：`POST /api/v1/chat/send` 必须使用 SSE 协议，不能返回普通 JSON
4. **WebSocket 广播**：后端应在每个阶段状态变更时主动推送事件到对应项目的 WebSocket 连接
5. **配置脱敏**：`GET /api/v1/config/llm` 返回时，`apiKey` 必须脱敏
6. **审查触发**：`POST /api/v1/reviews/:stageId/submit` 通过后，后端必须触发 Temporal Signal 通知 Agent OS 继续执行
7. **图谱数据**：图谱节点类型 `type` 字段建议使用大写驼峰命名（`Project`、`Requirement`、`Design`、`Code`、`Test`），前端以此区分节点颜色
8. **时间格式**：所有时间字段使用 ISO 8601 格式（如 `2026-05-22T10:00:00Z`），WebSocket 事件中的 `timestamp` 使用 Unix 毫秒时间戳