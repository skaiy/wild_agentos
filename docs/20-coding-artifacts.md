# 20. Claims-Scoped Coding Artifacts

Replayable outputs from coding agents are stored through the claims-scoped
artifact API. It supports `patch`, `run_transcript`, and `reproduce_script`,
plus `input_snapshot` for immutable JSON inputs uploaded before an invocation.
The built-in invocation `input_ref` resolver reads only that kind and stays off
by default (see [29-invocations-api.md](29-invocations-api.md)).
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
/api/v1/artifacts` lists only entries from the caller's claims graph;
`?kind=<kind>` narrows the list to one kind and an unknown kind returns `400`.
`GET /api/v1/artifacts/{id}/download` retrieves bytes only after the same check.

| `kind` | `Content-Type` | Extension | `task_iri` |
| --- | --- | --- | --- |
| `patch` | `text/x-diff; charset=utf-8` | `.patch` | required |
| `run_transcript` | `text/plain; charset=utf-8` | `.log` | required |
| `reproduce_script` | `text/x-shellscript; charset=utf-8` | `.sh` | required |
| `input_snapshot` | `application/json` | `.json` | optional |

`task_iri`, when present, must be non-empty, at most 2048 bytes, and free of
ASCII control characters. It is stored and echoed only: the server does not
check that it names an existing task, and it plays no part in authorization or
lookup (artifacts are read by id within the caller's claims). Callers may
therefore use their own business IRI or URN. Metadata reports `task_iri: null`
when it was omitted.

## Input snapshots

An `input_snapshot` is an immutable caller input, for example the large input
of a later invocation. Its content must be valid UTF-8 JSON (no byte-order
mark), otherwise upload returns `400` and stores nothing; size limit,
plaintext-secret guard, and claims scoping are the same as for other kinds.

```json
{
  "kind": "input_snapshot",
  "content_base64": "eyJzY2hlbWEiOiJzbmFwc2hvdC92MSJ9"
}
```

With the built-in `wao-artifact://` `input_ref` resolver (#341), an invocation
can reference it as `input_ref = { uri: "wao-artifact://<project>/<id>", sha256 }`,
where `sha256` is the returned `artifact.sha256` (the SHA-256 of the uploaded
bytes). Note that `input_ref`
applies its own, smaller content limit.

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

