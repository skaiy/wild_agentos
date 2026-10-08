# 20. Claims-Scoped Coding Artifacts

Replayable outputs from coding agents are stored through the claims-scoped
artifact API. It supports `patch`, `run_transcript`, and `reproduce_script`.
Each upload requires JWT-verified `IsolationClaims`; development `X-Identity`,
API keys, and anonymous requests receive `401`.

## API

`POST /api/v1/artifacts` accepts JSON:

```json
{
  "kind": "patch",
  "task_iri": "iri://task/123",
  "content_base64": "ZGlmZiAtLWdpdA=="
}
```

The response includes immutable metadata and a download URL. `GET
/api/v1/artifacts` lists only entries from the caller's claims graph. `GET
/api/v1/artifacts/{id}/download` retrieves bytes only after the same check.

Artifact bytes use the existing BlobStore with the server-minted
`{tenant}/artifacts/` prefix. Metadata is an RDF literal in the caller's
`graph://{tenant}/{project}` graph and includes `task_iri`, content hash,
creator, time, and blob key. Client data never selects either storage target.

## Replay and safety

`task_iri` links the patch, transcript, or script to its checkpoint/task
execution. Reproduction scripts must obtain credentials from environment
variables or a secret manager; secrets are never returned in metadata.
Historical blob objects are neither read nor migrated by this API.

### Plaintext-secret guard

Upload rejects content that embeds a credential *value* with `400` and
`matched_rules` (rule names only, never the matched text); nothing is stored.
Rules match real credential formats, not bare substrings: token prefixes must
start at an ASCII word boundary (start of input or a character outside
`[A-Za-z0-9_]`), must be followed by enough token characters, and are
case-sensitive like the real format.

| Rule | Matches |
| --- | --- |
| `pem_armor_header` | any `-----BEGIN <UPPERCASE LABEL>-----` line |
| `pem_private_key_marker` | `PRIVATE KEY-----` / `PRIVATE KEY BLOCK-----` |
| `aws_secret_access_key` | `aws_secret_access_key`, `aws-secret-access-key` or `SecretAccessKey` (any case), optional quote, `:`/`=`, then 40 `[A-Za-z0-9/+=]` |
| `aws_access_key_id` | `AKIA`/`ASIA` + 16 `[0-9A-Z]`, bounded on both sides |
| `github_fine_grained_pat` | `github_pat_` + at least 22 `[A-Za-z0-9_]` |
| `github_token` | `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_` + at least 36 `[A-Za-z0-9]` |
| `slack_token` | `xoxa-`/`xoxb-`/`xoxp-`/`xoxo-`/`xoxs-`/`xoxr-` + at least 10 `[A-Za-z0-9-]` |
| `sk_api_key` | `sk-` + at least 20 `[A-Za-z0-9_-]` (covers `sk-proj-…`, `sk-ant-…`) |

Ordinary words such as `task-`, `risk-`, `disk-` or `ask-` never match, and
referencing a credential by name (`AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}`,
`TOKEN="$TOKEN_FROM_ENV"`) is allowed.

