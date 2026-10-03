# Separate demo and platform tenants

Demo tokens are shared widely. Global configuration affects every tenant, so demo access must not be platform administration. A tenant is created implicitly when a verified token carries its tenant ID; there is no tenant registration step.

## Convention

| Purpose | Tenant | Project | Actor | Roles |
| --- | --- | --- | --- | --- |
| Demo | `demo` | `showcase` | `demo-user` | `DA`, `mcp_invoke` |
| Platform administration | `platform` | Deployment-specific | Restricted administrator | `PLATFORM_ADMIN` |

The demo names can be changed with `DEMO_TENANT`, `DEMO_PROJECT`, and `DEMO_ACTOR`. Never give a demo token `PLATFORM_ADMIN`. The script rejects `default`, an overlap with the platform tenant, and `PLATFORM_ADMIN` in `DEMO_ROLES`. It compares against `AGENTOS_PLATFORM_ADMIN_TENANT` in the shell where it runs, so export the same value the server uses; if it is unset the script prints a note and cannot do that check.

## Prerequisites

Set `AGENTOS_JWT_SECRET` to the deployment's HS256 signing secret (at least 32 bytes). Set `AGENTOS_PLATFORM_ADMIN_TENANT=platform` on the server, using your actual platform tenant instead of `platform` if different. It must not be `default` or the demo tenant. Use a running local HTTP API with embedding and document storage enabled to seed the sample documents. Keep the signing secret and token files private.

Do not add `DA` to a live demo token until the platform-admin gate (#274) **and** the config-read gate (#290) are deployed. The script reports a skip for the config-read check on older kernels, but that is not approval for rollout.

## Mint, seed, verify

```sh
# Set AGENTOS_JWT_SECRET privately in your environment; do not paste it into commands.
export AGENTOS_PLATFORM_ADMIN_TENANT=platform
scripts/demo-tenant.sh mint --out ./demo-tenant.jwt --exp-days 7
DEMO_TOKEN_FILE=./demo-tenant.jwt BASE_URL=http://127.0.0.1:8080 scripts/demo-tenant.sh seed
DEMO_TOKEN_FILE=./demo-tenant.jwt BASE_URL=http://127.0.0.1:8080 scripts/demo-tenant.sh verify
```

`mint` writes a mode-600 token file and prints only the path and claim summary. The other commands read the token from that file and pass it to `curl` through a mode-600 header file, never on the command line. `seed` skips an existing demo agent, knowledge base, and documents by name; it uploads fictional samples from `scripts/examples/demo/`. `verify` checks demo reads, denies global configuration changes, and checks that another tenant cannot see or update the demo agent. Do not commit or share the token file.

## Rotation and existing data

Re-run `mint` with a new expiration to replace the file. Rotate `AGENTOS_JWT_SECRET` to revoke **all** tokens signed with the old secret, not only demo tokens. Existing demo data under `default` is **not** migrated automatically; review `scripts/isolation-migrate` before moving it.
