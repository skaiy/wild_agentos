# Catalog MCP outbound invocation

The catalog can invoke an HTTP MCP tool on behalf of a tenant/project. This
surface is for controlled Core-to-MCP calls such as WODP B-07 health checks; it
does not expose MCP write tools or execute arbitrary Skills.

## Configure outbound JWT

The process that runs Core must set the following environment variables:

```sh
MCP_JWT_SECRET=...           # required HS256 signing secret
MCP_JWT_ISSUER=wodp-demo     # optional; default shown
MCP_JWT_AUDIENCE=wodp-mcp    # optional; default shown
MCP_JWT_SUB=wodp_agent       # optional; default shown
```

`MCP_JWT_SECRET` is required. Core mints a short-lived (five minute) HS256 JWT
for each outbound request. The issuer, audience, and subject defaults are
provided for the WODP sidecar contract and can be overridden with their
respective environment variables.

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
  "name": "superset-mcp",
  "description": "Superset read tools",
  "endpoint": "http://host.docker.internal:5008/mcp",
  "protocol": "http",
  "auth_kind": "bearer_jwt"
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
  "server": "superset-mcp",
  "tool_name": "health_check",
  "arguments": {}
}
```

Core mints the dedicated MCP JWT, sends
`Authorization: Bearer <minted JWT>` to the registered endpoint, and forwards
a JSON-RPC `tools/call` request. A successful response is returned as
`{"result": ...}`.

If the catalog entry lacks `auth_kind: "bearer_jwt"`, the signing environment
is absent, JWT minting fails, or the sidecar rejects the token, invocation
fails explicitly. Core does not synthesize a successful tool result.

## Boundary with `POST /mcp`

`POST /mcp` is the inbound Streamable HTTP endpoint that publishes explicitly
exposed tenant Skills to external MCP clients. It is **not** a proxy for
catalog registrations and it does not route catalog tools. Its Skill exposure
and write-gate policies are unchanged.

Inbound `IsolationClaims` authorize Core's catalog lookup only. They are never
serialized or forwarded as an outbound MCP Bearer credential.
