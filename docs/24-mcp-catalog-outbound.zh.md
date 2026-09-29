# Catalog MCP 出站调用

Catalog 可以代表一个租户/项目调用 HTTP MCP 工具。这个接口用于受控的
Core→MCP 调用，例如 WODP B-07 健康检查；它不会开放 MCP 写工具，也不会执行
任意 Skill。

## 配置出站 JWT

运行 Core 的进程必须设置以下环境变量：

```sh
MCP_JWT_SECRET=...           # 必填的 HS256 签名密钥
MCP_JWT_ISSUER=wodp-demo     # 可选；此处为默认值
MCP_JWT_AUDIENCE=wodp-mcp    # 可选；此处为默认值
MCP_JWT_SUB=wodp_agent       # 可选；此处为默认值
```

`MCP_JWT_SECRET` 为必填项。Core 会为每一个出站请求签发一个短期（五分钟）
HS256 JWT。签发者、受众和主体的默认值与 WODP sidecar 契约一致，并且可通过
各自的环境变量覆盖。

Catalog JSON 永不接收密钥。若要让一个 Catalog 条目使用此流程，请在注册时设置
`auth_kind: "bearer_jwt"`。持久化的 Catalog 记录只包含固定的环境变量引用
（`MCP_JWT_SECRET`、`MCP_JWT_ISSUER`、`MCP_JWT_AUDIENCE` 和
`MCP_JWT_SUB`）。

## 注册和调用

注册一个 HTTP MCP 端点：

```http
POST /api/v1/mcp/servers
Content-Type: application/json

{
  "name": "superset-mcp",
  "description": "Superset read tools",
  "endpoint": "http://host.docker.internal:5008/mcp",
  "protocol": "http",
  "auth_kind": "bearer_jwt"
}
```

Catalog 管理和调用接口都要求经过验证的入站 `IsolationClaims`；Core 会将查找范围
限定为调用者所在的租户和项目。

通过 Catalog MCP 的 `name` 调用一个已注册的工具；当名称有歧义时，请使用
`id`：

```http
POST /api/v1/mcp/servers/invoke
Content-Type: application/json

{
  "server": "superset-mcp",
  "tool_name": "health_check",
  "arguments": {}
}
```

Core 会签发专用 MCP JWT，向已注册端点发送
`Authorization: Bearer <minted JWT>`，并转发 JSON-RPC `tools/call` 请求。
成功响应的形式为 `{"result": ...}`。

为兼容 Streamable HTTP，出站请求会发送
`Accept: application/json, text/event-stream` 和 `Content-Type: application/json`。
Core 可以接收 JSON 响应，或从 SSE 响应中读取匹配的 JSON-RPC 消息。每次调用都是
无状态的，不执行 initialize/session 握手。

如果 Catalog 条目缺少 `auth_kind: "bearer_jwt"`、签名环境变量未配置、JWT 签发
失败，或 sidecar 拒绝令牌，调用会明确失败。Core 不会伪造成功的工具结果。

## 与 `POST /mcp` 的边界

`POST /mcp` 是一个入站 Streamable HTTP 端点，用于向外部 MCP 客户端发布已明确
暴露的租户 Skill。它**不是** Catalog 注册项的代理，也不会路由 Catalog 工具。
其 Skill 暴露和写闸策略保持不变。

入站 `IsolationClaims` 只用于授权 Core 进行 Catalog 查找；它们绝不会被序列化
或作为出站 MCP Bearer 凭据转发。
