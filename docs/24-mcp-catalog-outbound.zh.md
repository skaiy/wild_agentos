# Catalog MCP 出站调用

Catalog 可以代表一个租户/项目调用 HTTP MCP 工具。这个接口用于受控的
服务→MCP 调用。它不会开放任意工具：每个 Catalog 条目都可以使用精确的工具
allowlist，写类工具只有在明确启用后才能调用。

## 配置出站 JWT

运行 Core 的进程必须设置以下环境变量：

```sh
MCP_JWT_SECRET=...           # 必填的 HS256 签名密钥
MCP_JWT_ISSUER=wild-agentos-core  # 可选；此处为默认值
MCP_JWT_SUB=mcp-client            # 可选；此处为默认值
```

`MCP_JWT_SECRET` 为必填项。服务会为每一个出站请求签发一个短期（五分钟）
HS256 JWT。签发者和主体的默认值是中性示例，并且可通过各自的环境变量覆盖。
JWT 的 `aud` 默认是已登记的 Catalog server ID，因此不同 server 的凭据默认不会
共享同一个受众。可在条目中设置 `audience_env`，其值是保存该条目 audience 的环境
变量名称。调用时才读取该值，Catalog 中不会保存 audience 值；该变量缺失或为空时，
会在发送请求前失败。

JWT 的 `sub` 用于标识签名服务，默认值是 `wao-core`，可通过
`MCP_JWT_SUBJECT` 覆盖。

Catalog JSON 永不接收密钥。若要让一个 Catalog 条目使用此流程，请在注册时设置
`auth_kind: "bearer_jwt"`。持久化的 Catalog 记录只包含固定的环境变量引用
（`MCP_JWT_SECRET`、`MCP_JWT_ISSUER` 和 `MCP_JWT_SUB`）。

## 配置出站边界

Catalog HTTP endpoint 必须是没有用户凭据的绝对 `http` 或 `https` URL。登记时会
记录其 scheme、host 和 port，并在每次调用前再次检查。调用请求不能提供或替换
endpoint。

当部署需要显式网络 allowlist 时，可将
`MCP_OUTBOUND_ALLOWED_ORIGINS` 设置为以逗号分隔的允许 origin 列表：

```sh
MCP_OUTBOUND_ALLOWED_ORIGINS=https://mcp.example.test,http://127.0.0.1:8080
MCP_OUTBOUND_CONNECT_TIMEOUT_MS=5000
MCP_OUTBOUND_TIMEOUT_MS=15000
MCP_OUTBOUND_MAX_RESPONSE_BYTES=1048576
```

未设置 allowlist 时，只允许每个 Catalog 条目已登记的 origin。无效或已改变的
endpoint 会在发送任何出站请求前被拒绝。超时和响应大小设置均为可选的正整数；
上面的值即安全默认值。

当 `AGENTOS_AUTH_STRICT=true` 时，必须设置 `MCP_OUTBOUND_ALLOWED_ORIGINS`。
未设置或空值会阻止启动；Catalog 的登记和调用也会再次拒绝该配置，作为纵深防御。
HTTP redirect 永远不会被跟随。

条目还可以设置 `timeout_seconds`，取值为 1 至 300 的正整数。它是该条目的总出站
请求超时，但会被 `MCP_OUTBOUND_TIMEOUT_MS` 限制；未设置时使用全局设置或其默认值。

## 注册和调用

注册一个 HTTP MCP 端点：

```http
POST /api/v1/mcp/servers
Content-Type: application/json

{
  "name": "example-mcp-server",
  "description": "Example read tools",
  "endpoint": "http://host.docker.internal:5008/mcp",
  "protocol": "http",
  "auth_kind": "bearer_jwt",
  "audience_env": "EXAMPLE_MCP_AUDIENCE",
  "timeout_seconds": 15,
  "allowed_tools": ["health_check", "list_reports"],
  "write_tools_enabled": false
}
```

Catalog 管理和调用接口都要求经过验证的入站 `IsolationClaims`；Core 会将查找范围
限定为调用者所在的租户和项目。
登记 Catalog 条目要求专用的 `mcp_admin` 角色。只有普通 `DA` 角色的调用方会收到
`403`，且不会写入 Catalog 文件。
删除 Catalog 条目同样要求该角色和对应的租户/项目范围。

通过 Catalog MCP 的 `name` 调用一个已注册的工具；当名称有歧义时，请使用
`id`：

```http
POST /api/v1/mcp/servers/invoke
Content-Type: application/json

{
  "server": "example-mcp-server",
  "tool_name": "health_check",
  "arguments": {}
}
```

服务会签发专用 MCP JWT，向已注册端点发送
`Authorization: Bearer <minted JWT>`，并转发 JSON-RPC `tools/call` 请求。
成功响应的形式为 `{"result": ...}`。

为兼容 Streamable HTTP，出站请求会发送
`Accept: application/json, text/event-stream` 和 `Content-Type: application/json`。
Core 可以接收 JSON 响应，或从 SSE 响应中读取匹配的 JSON-RPC 消息。每次调用都是
无状态的，不执行 initialize/session 握手。

如果 Catalog 条目缺少 `auth_kind: "bearer_jwt"`、签名环境变量未配置、JWT 签发
失败，或远端服务拒绝令牌，调用会明确失败。服务不会伪造成功的工具结果。

## 工具策略 / Tool policy

`allowed_tools` 是可选的工具名列表，采用精确匹配并区分大小写。每个名称必须由
1–128 个 ASCII 字母、数字、`.`、`_`、`-` 或 `/` 组成。列表最多包含 64 项，
不能有重复名称。空列表表示不允许任何工具。

`write_tools_enabled` 默认值为 `false`。只有提供了非空的 `allowed_tools` 时才能
设为 `true`。写类工具必须同时在列表中，并且被明确启用。写类前缀完整列表为：

`create_`、`update_`、`delete_`、`generate_`、`execute_`、`add_`、`remove_`、
`apply_`、`duplicate_`、`restore_`、`save_`、`manage_`、`set_`、`write_`、
`insert_`、`drop_`、`upsert_`、`import_`、`publish_` 和 `send_`。

此前缀检查是纵深防御措施，真正的控制方式是 allowlist。服务不会根据远端
`tools/list` annotations 来批准调用。

被拒绝的调用会在签发 JWT 或发送远端请求之前返回 `403`：

- 工具不在 allowlist 中时返回 `mcp_tool_not_allowed`。
- 写类工具未明确启用时返回 `mcp_write_tool_blocked`。

没有 `allowed_tools` 的旧 Catalog 记录仍可调用非写类工具，但所有写类工具都会
被阻止。如需为旧条目添加 allowlist，请重新注册；Catalog 不提供更新接口。

## 与 `POST /mcp` 的边界

`POST /mcp` 是一个入站 Streamable HTTP 端点，用于向外部 MCP 客户端发布已明确
暴露的租户 Skill。它**不是** Catalog 注册项的代理，也不会路由 Catalog 工具。
其 Skill 暴露和写闸策略保持不变。

入站 `IsolationClaims` 只用于授权服务进行 Catalog 查找；它们绝不会作为 Bearer
凭据转发，但其中已验证的 `tenant_id` 和 `project_id` 会写入签发的出站 JWT。任一
值缺失时，服务会 fail-closed，且不会调用 endpoint。没有单独的管理员绕过路径。
对于出站调用，这两个范围 claim 都必须在入站 token 中明确出现；旧 token 的默认
project scope 不足以授权。明确命名为 `default` 的项目仍然有效。

MCP sidecar 在接受请求前必须验证 JWT 签名，并校验 `aud`、`tenant_id` 和
`project_id`。
