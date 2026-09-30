# Catalog MCP outbound invocation

The catalog can invoke an HTTP MCP tool on behalf of a tenant/project. This
surface is for controlled service-to-MCP calls. It does not expose arbitrary
tools: each catalog entry can use an exact tool allowlist, and write-like tools
are blocked unless explicitly enabled.

## Configure outbound JWT

The process that runs Core must set the following environment variables:

```sh
MCP_JWT_SECRET=...           # required HS256 signing secret
MCP_JWT_ISSUER=wild-agentos-core  # optional; default shown
MCP_JWT_AUDIENCE=example-mcp      # optional; default shown
MCP_JWT_SUB=mcp-client            # optional; default shown
```

`MCP_JWT_SECRET` is required. The service mints a short-lived (five minute) HS256 JWT
for each outbound request. The issuer, audience, and subject defaults are
neutral examples and can be overridden with their respective environment
variables.

Secrets are never accepted in catalog JSON. To opt a catalog entry into this
flow, register it with `auth_kind: "bearer_jwt"`. The persisted catalog record
contains only the fixed environment-variable references (`MCP_JWT_SECRET`,
`MCP_JWT_ISSUER`, `MCP_JWT_AUDIENCE`, and `MCP_JWT_SUB`).

## Register and invoke

Register an HTTP MCP endpoint:

```http
POST /api/v1/mcp/servers
Content-Type: application/json

{
  "name": "example-mcp-server",
  "description": "Example read tools",
  "endpoint": "http://host.docker.internal:5008/mcp",
  "protocol": "http",
  "auth_kind": "bearer_jwt",
  "allowed_tools": ["health_check", "list_reports"],
  "write_tools_enabled": false
}
```

The catalog management and invoke endpoints require verified inbound
`IsolationClaims`; Core scopes lookup to the caller's tenant and project.

Invoke a registered tool by its catalog `name` (or `id` when names are
ambiguous):

```http
POST /api/v1/mcp/servers/invoke
Content-Type: application/json

{
  "server": "example-mcp-server",
  "tool_name": "health_check",
  "arguments": {}
}
```

The service mints the dedicated MCP JWT, sends
`Authorization: Bearer <minted JWT>` to the registered endpoint, and forwards
a JSON-RPC `tools/call` request. A successful response is returned as
`{"result": ...}`.

For Streamable HTTP compatibility, the outbound request sends
`Accept: application/json, text/event-stream` and `Content-Type: application/json`.
Core accepts a JSON response or the matching JSON-RPC message from an SSE
response. Each invoke is stateless and does not perform an initialize/session
handshake.

If the catalog entry lacks `auth_kind: "bearer_jwt"`, the signing environment
is absent, JWT minting fails, or the remote server rejects the token, invocation
fails explicitly. The service does not synthesize a successful tool result.

## Tool policy

`allowed_tools` is an optional, exact-match and case-sensitive list of tool
names. Each name must be 1–128 ASCII letters, digits, `.`, `_`, `-`, or `/`.
The maximum list size is 64 and duplicate names are rejected. An empty list
means no tools are allowed.

`write_tools_enabled` defaults to `false`. It can be set to `true` only when
`allowed_tools` is present and non-empty. A write-like tool must be both listed
and explicitly enabled. The write-like prefix list is:

`create_`, `update_`, `delete_`, `generate_`, `execute_`, `add_`, `remove_`,
`apply_`, `duplicate_`, `restore_`, `save_`, `manage_`, `set_`, `write_`,
`insert_`, `drop_`, `upsert_`, `import_`, `publish_`, and `send_`.

This prefix check is defense in depth. The allowlist is the actual control;
the service does not use remote `tools/list` annotations to approve a call.

Rejected calls return `403` before a JWT is minted or a remote request is sent:

- `mcp_tool_not_allowed` when the tool is absent from an allowlist.
- `mcp_write_tool_blocked` when a write-like tool has not been explicitly enabled.

Older catalog records without `allowed_tools` remain usable for non-write-like
tools, but write-like tools are blocked. Re-register an older entry to add an
allowlist; the catalog has no update endpoint.

## Boundary with `POST /mcp`

`POST /mcp` is the inbound Streamable HTTP endpoint that publishes explicitly
exposed tenant Skills to external MCP clients. It is **not** a proxy for
catalog registrations and it does not route catalog tools. Its Skill exposure
and write-gate policies are unchanged.

Inbound `IsolationClaims` authorize the service's catalog lookup only. They are never
serialized or forwarded as an outbound MCP Bearer credential.
