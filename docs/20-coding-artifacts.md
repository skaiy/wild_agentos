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
variables or a secret manager. Upload rejects recognizable plaintext private
keys and common access-token forms; secrets are never returned in metadata.
Historical blob objects are neither read nor migrated by this API.
