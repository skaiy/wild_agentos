# 20. Logic and Skill Package Market

The package market is a versioned kernel catalog for reusable `FunctionDef`
logic and Skills. It is intentionally not a Foundry-level Workshop: it does
not execute uploaded package code, create workflows, or provide a visual
authoring environment.

## Contract

`POST /api/v1/market/packages` publishes an immutable package version. A
package declares:

- `name` and SemVer `version` (`MAJOR.MINOR.PATCH`)
- JSON Schema `input_schema` and `output_schema`
- `side_effect_level`: `none`, `read`, `write`, or `execute`
- `visibility`: `private`, `tenant`, or `system`
- optional `functions` (`FunctionDef`) and `skills` (`SkillMeta`)

`system` is reserved for kernel-owned packages and cannot be published via the
HTTP API. Re-publishing an existing publisher/name/version returns `409`; a
new version must be used instead.

Embedded Skills run the same admission gate as standalone Skill publication.
Any failed embedded gate rejects the complete package with `422`, so no
partial package is published.

## Tenant access and installation

All catalog, install, and rollback endpoints require verified
`IsolationClaims` minted from JWT authentication and the `DA` role for
mutations. Client-provided tenant fields are never accepted.

- `private`: only the publishing tenant and project may view or install.
- `tenant`: any project in the publishing tenant may view or install.
- `system`: readable kernel package; its content is read-only.

`POST /api/v1/market/packages/:name/install` selects a visible version for the
caller’s tenant/project. `POST /api/v1/market/packages/:name/rollback` selects
an earlier visible version using the same version request body. Installation
records are scoped by tenant and project; a second version replaces that
scope's active selection.
