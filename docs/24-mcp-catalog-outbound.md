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
MCP_JWT_SUB=mcp-client            # optional; default shown
```

`MCP_JWT_SECRET` is required. The service mints a short-lived (five minute) HS256 JWT
for each outbound request. The issuer and subject defaults are neutral examples
and can be overridden with their respective environment variables. The JWT
`aud` defaults to the registered catalog server ID, so credentials for different
servers cannot share an audience by default. Set `audience_env` on an entry to
the name of an environment variable whose value is that entry's audience. The
value is read only when invoking and is never stored in the catalog. A missing
or empty named variable fails the invoke before it is sent.

The JWT `sub` identifies the signing service. It defaults to `wao-core` and can
be overridden with `MCP_JWT_SUBJECT`.

Secrets are never accepted in catalog JSON. To opt a catalog entry into this
flow, register it with `auth_kind: "bearer_jwt"`. The persisted catalog record
contains only the fixed environment-variable references (`MCP_JWT_SECRET`,
`MCP_JWT_ISSUER`, and `MCP_JWT_SUB`).

## Configure outbound boundaries

Catalog HTTP endpoints must be absolute `http` or `https` URLs without user
credentials. Their scheme, host, and port are recorded at registration and
checked again before every invoke. The invoke request cannot supply or replace
an endpoint.

Set `MCP_OUTBOUND_ALLOWED_ORIGINS` to a comma-separated list of permitted
origins when the deployment needs an explicit network allowlist:

```sh
MCP_OUTBOUND_ALLOWED_ORIGINS=https://mcp.example.test,http://127.0.0.1:8080
MCP_OUTBOUND_CONNECT_TIMEOUT_MS=5000
MCP_OUTBOUND_TIMEOUT_MS=15000
MCP_OUTBOUND_MAX_RESPONSE_BYTES=1048576
```

When the allowlist is unset, only each catalog entry's registered origin is
allowed. Invalid or changed endpoints are rejected before any outbound request.
The timeout and response-size settings are optional positive integers; the
values shown are the secure defaults.

When `AGENTOS_AUTH_STRICT=true`, `MCP_OUTBOUND_ALLOWED_ORIGINS` is required.
An unset or empty value prevents startup and catalog register/invoke requests
also reject it as defense in depth. HTTP redirects are never followed.

An entry may also set `timeout_seconds` to a positive value from 1 through 300.
It applies as that entry's total outbound request timeout, capped by
`MCP_OUTBOUND_TIMEOUT_MS`; when omitted, the global setting or its default is
used.

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
  "audience_env": "EXAMPLE_MCP_AUDIENCE",
  "timeout_seconds": 15,
  "allowed_tools": ["health_check", "list_reports"],
  "write_tools_enabled": false
}
```

The catalog management and invoke endpoints require verified inbound
`IsolationClaims`; Core scopes lookup to the caller's tenant and project.
Registering a catalog entry requires the dedicated `mcp_admin` role. A caller
with only the ordinary `DA` role receives `403`, and no catalog file is written.
Deleting a catalog entry requires the same role and tenant/project scope.

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
used as a bearer credential, but their verified `tenant_id` and `project_id`
are included as claims in the minted outbound JWT. If either value is absent,
the service fails closed and does not invoke the endpoint. There is no separate
administrator bypass path.
For outbound invocation, both scope claims must have been explicitly present
in the inbound token; a legacy defaulted project scope is not sufficient. A
project explicitly named `default` remains valid.

MCP sidecars must verify the JWT signature and validate `aud`, `tenant_id`, and
`project_id` before accepting a request.
