# 20. Claims-Scoped Coding Artifacts

Replayable outputs from coding agents are stored through the claims-scoped
artifact API. It supports `patch`, `run_transcript`, and `reproduce_script`.
`input_snapshot` is the kind the built-in invocation `input_ref` resolver
reads; `POST /api/v1/artifacts` rejects that kind until #378, and the
resolver stays off by default (see
[29-invocations-api.md](29-invocations-api.md)).
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

Upload rejects content that embeds a credential *value* with `400` and a fixed
body `{"error": "plaintext secrets are forbidden in coding artifacts", "code":
"artifact_plaintext_secret"}`; neither the matched text nor the rule is echoed,
and nothing is stored. Rules match real credential formats, not bare
substrings, and are case-sensitive like the issued format. The same detector
(`utils::secret_scan`) drives the skill pipeline's file scan, where private
keys fail the gate and other formats warn.

| Rule | Matches |
| --- | --- |
| `private_key` | `-----BEGIN [A-Z ]*PRIVATE KEY-----` or `… PRIVATE KEY BLOCK-----` (certificates and public keys pass) |
| `aws_access_key_id` | `AKIA` + 16 `[0-9A-Z]` |
| `aws_secret_access_key` | `aws_secret_access_key` (any case), optional quote, `:`/`=`, then a 40-character `[A-Za-z0-9/+=]` value |
| `github_classic_pat` | `ghp_` + 36 `[A-Za-z0-9]` |
| `github_fine_grained_pat` | `github_pat_` + 82 `[A-Za-z0-9_]` |
| `slack_token` | `xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-` + at least 10 `[A-Za-z0-9-]` |
| `sk_api_key` | `sk-` not preceded by `[A-Za-z0-9_-]`, + at least 20 `[A-Za-z0-9_-]` (covers `sk-proj-…`, `sk-ant-…`, `sk-<slug>-<hex>`) |

Ordinary words such as `task-`, `risk-`, `disk-`, `ask-` or `Slovakia` never
match, and referencing a credential by name
(`AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}`, `TOKEN="$TOKEN_FROM_ENV"`)
is allowed.

