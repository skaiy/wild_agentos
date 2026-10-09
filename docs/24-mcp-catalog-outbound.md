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
MCP_JWT_SUBJECT=wao-core          # optional; default shown
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
be overridden with `MCP_JWT_SUBJECT`. Subject values must be 1–64 ASCII
characters from `A-Z`, `a-z`, `0-9`, `.`, `_`, and `-`. In non-strict mode,
surrounding whitespace is trimmed; an empty or invalid value warns and falls
back to `wao-core`. In strict authentication, an empty, whitespace-padded, or
invalid value prevents startup. When the variable is unset in either mode, the
default remains `wao-core`.

Secrets are never accepted in catalog JSON. To opt a catalog entry into this
flow, register it with `auth_kind: "bearer_jwt"`. The persisted catalog record
contains only the fixed environment-variable references (`MCP_JWT_SECRET`,
`MCP_JWT_ISSUER`, and `MCP_JWT_SUBJECT`).

`MCP_JWT_SUB` is deprecated. Under strict authentication, startup refuses a
configuration that sets that legacy variable without a valid
`MCP_JWT_SUBJECT`, including when the legacy value is empty or whitespace-only.
If both variables are set, `MCP_JWT_SUBJECT` takes precedence.

## Configure outbound boundaries

Catalog HTTP endpoints must be absolute `http` or `https` URLs without user
credentials. Their scheme, host, and port are recorded at registration and
checked again before every invoke. The invoke request cannot supply or replace
an endpoint.

Set `MCP_OUTBOUND_ALLOWED_ORIGINS` to a comma-separated list of permitted
origins when the deployment needs an explicit network allowlist:

```sh
MCP_OUTBOUND_ALLOWED_ORIGINS=https://mcp.example.test,http://127.0.0.1:8080
MCP_OUTBOUND_ALLOWED_PRIVATE_CIDRS=10.20.0.0/16,fd00:1::/64
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

Before minting a JWT, each catalog invoke resolves a hostname exactly once,
rejects the entire answer if any address is not permitted, and pins all vetted
addresses to the request client. The URL retains its original hostname for
HTTPS certificate validation and SNI. Catalog outbound requests do not use
configured HTTP proxies, which could otherwise resolve the hostname again.
IP-literal endpoints do not require DNS resolution.

By default, loopback, link-local (including metadata), private IPv4 and IPv6,
unspecified, shared/CGNAT, `192.0.0.0/24`, benchmark, multicast, and reserved
addresses are blocked, including IPv4-mapped, IPv4-compatible, and NAT64
(`64:ff9b::/96`) IPv6 forms. Three explicit permission rules apply:

1. **Listed origin.** An endpoint (hostname or IP literal) whose exact origin
   (scheme, host, and port) appears in `MCP_OUTBOUND_ALLOWED_ORIGINS` may
   resolve to private, loopback, or other blocked addresses, such as a sidecar
   addressed by its container hostname. A hostname is still resolved only once;
   every vetted address is pinned and no proxy is used.
2. **Local development.** In non-strict local development with the origin
   allowlist **unset**, an IP-literal endpoint may use loopback or private
   addresses.
3. **Optional CIDR opt-in.** A hostname that is not covered by rule 1 may
   resolve into `MCP_OUTBOUND_ALLOWED_PRIVATE_CIDRS` (a comma-separated CIDR
   list, as shown above), which permits the whole segment. Every resolved
   address must be permitted.

A hostname that matches none of these rules and resolves to a blocked address
is rejected. In particular, with the origin allowlist unset and no CIDR
configured, a hostname resolving to a private or loopback address returns
`403`.

The following addresses are **never** permitted, even when the origin is listed
or a configured CIDR covers them: link-local (`169.254.0.0/16` and
`fe80::/10`, which include the `169.254.169.254` instance metadata address),
other well-known metadata addresses (`fd00:ec2::254`, `100.100.100.200`),
unspecified, multicast, and broadcast, in any IPv6-embedded form.

Rule 1 grants access to specific, operator-listed origins instead of whole
network segments. A deployment whose sidecar origin is already in
`MCP_OUTBOUND_ALLOWED_ORIGINS` needs no configuration change. The CIDR opt-in
remains available when a whole segment must be reachable by hostnames that are
not listed individually; it never acts as a boolean "allow private" switch.
Malformed CIDRs fail closed with `503 mcp_outbound_allowlist_required` on invoke
and prevent startup in strict mode. Rejected addresses return
`403 mcp_endpoint_not_allowed`; failed or empty DNS answers return
`502 outbound_mcp_call_failed`. Neither response includes resolved addresses.

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
Invoking a catalog tool requires either `DA` or the dedicated `mcp_invoke`
role. Missing or unrelated roles receive `403 {"error":"mcp_role_required"}`
before JWT signing, HTTP-client creation, or any outbound request.

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
catalog registrations and it does not route catalog tools. Skill exposures on
that endpoint are scoped to the caller's verified tenant and project. A
verified token with no project claim is rejected with `403` and
`mcp_claims_incomplete` before any exposure is read. This document does not
change the catalog outbound rules.

Inbound `IsolationClaims` authorize the service's catalog lookup only. They are never
used as a bearer credential, but their verified `tenant_id` and `project_id`
are included as claims in the minted outbound JWT. If either value is absent,
the service fails closed and does not invoke the endpoint. There is no separate
administrator bypass path.
For outbound invocation, both scope claims must have been explicitly present
in the inbound token; a legacy defaulted project scope is not sufficient. A
project explicitly named `default` remains valid. Incomplete verified scope
claims return `403` with `mcp_claims_incomplete` and only the missing field
name; no outbound request is sent.
JWTs without a tenant claim fail authentication with `401` before catalog
lookup. A missing or empty project claim keeps the existing default-scope
compatibility for other endpoints, but remains ineligible for outbound MCP.
Scopes originating from deployment configuration, including watcher and
migration processing, are not JWT-verified and return `403
mcp_claims_unverified` if used for outbound MCP.

MCP sidecars must verify the JWT signature and validate `aud`, `tenant_id`, and
`project_id` before accepting a request.
