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

Upload rejects content that embeds a credential *value* with `400` and a fixed
body `{"error": "plaintext secrets are forbidden in coding artifacts", "code":
"artifact_plaintext_secret"}`; neither the matched text nor the rule is echoed,
and nothing is stored. Rules match real credential formats, not bare
substrings, and are case-sensitive like the issued format. The same detector
(`utils::secret_scan`) drives the skill pipeline's file scan, where private
keys fail the gate and other formats warn.

| Rule | Matches |
| --- | --- |
| `private_key` | `-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----` or `… PRIVATE KEY BLOCK-----` (certificates and public keys pass) |
| `aws_access_key_id` | `AKIA` + 16 `[0-9A-Z]` |
| `aws_secret_access_key` | `aws_secret_access_key` (any case), optional quote, escaped (`\"`, `\'`) or unescaped (`"`, `'`), optional whitespace, separator `=>`, `:`, or `=`, optional whitespace, optional escaped or unescaped quote, then a 40-character `[A-Za-z0-9/+=]` value |
| `github_classic_pat` | `ghp_` + 36 `[A-Za-z0-9]` |
| `github_fine_grained_pat` | `github_pat_` + 82 `[A-Za-z0-9_]` |
| `slack_token` | `xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-` + at least 10 `[A-Za-z0-9-]` |
| `sk_api_key` | Left boundary is the start of text, a character outside `[A-Za-z0-9_-]`, an escaped `\n`, `\r`, `\t`, `\b`, or `\f`, a `\u` plus four hex digits, or `%` plus two hex digits. Then `sk-`, optional `proj-` or `ant-`, and at least 20 `[A-Za-z0-9_-]`. That match is a name, not a key, only when the text after `sk-` splits on `-` into at least three segments, at least two segments are one or more `[a-z]`, and every segment is one or more `[a-z]` except that the final segment may be `v` plus 1 to 4 digits. `sk-learn-classification-examples-v2` and `sk-my-long-running-service-name` are names. A digit in any other segment, a longer digit run, an uppercase letter, or `_` keeps the match a key. An all-letter key is let through; that is an accepted risk |

An all-letter key is let through: when every segment after `sk-` is `[a-z]+`
and the segment counts above are met, the match is treated as a name. That is
an accepted risk.

Ordinary words such as `task-`, `risk-`, `disk-`, `ask-` or `Slovakia` never
match, and referencing a credential by name
(`AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}`, `TOKEN="$TOKEN_FROM_ENV"`)
is allowed.

