# 20. Claims-Scoped Coding Artifacts

Replayable outputs from coding agents are stored through the claims-scoped
artifact API. It supports `patch`, `run_transcript`, and `reproduce_script`,
plus `input_snapshot` for immutable JSON inputs uploaded before an invocation.
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
variables or a secret manager. Upload rejects recognizable plaintext private
keys and common access-token forms; secrets are never returned in metadata.
Historical blob objects are neither read nor migrated by this API.
