# 24. EV-repair example knowledge pack

> *For the Chinese version, see [24-ev-repair-example-pack.zh.md](24-ev-repair-example-pack.zh.md).*

The `ev-repair-fault-kb` showcase upgrades the opt-in `ev-repair` runtime asset from
prompt-only behavior to a **filesystem example pack** with ontology anchors,
SHACL shapes, a distributable Skill package, frozen golden I/O, and audit samples.
All entities, brands, and fault codes are **fictional** and safe for CI.

## Layout

| Path | Purpose |
|------|---------|
| `example-packs/ev-repair-fault-kb/pack.yaml` | Pack manifest (`agentos.dev/knowledge-pack/v1`) |
| `ontology/anchors.json` | Required `ObjectType` / `LinkType` ids checked against the seeded `ev-repair` ontology |
| `shacl/fault-code-shapes.ttl` | SHACL shapes for fictional `FaultCode` instances |
| `rdf/fictional-sample.ttl` | Sample RDF triples validated by the gate |
| `skills/fault-code-lookup/` | Tenant Skill package with its own golden fixtures |
| `audit/samples.json` | Fictional isolation audit exemplars (`isolation_verified: true`) |
| `tests/golden-input.json` / `golden-output.json` | Frozen pack-level gate contract |

The built-in kernel seed (`ev-repair-fault-kb` / `ev-repair` alias) remains the
runtime binding surface; this directory is the **distributable, testable artifact**.

## Pack-level CI gate

CI job `knowledge-pack-gate` runs:

```bash
./scripts/check_ev_repair_example_pack.sh
```

Locally:

```bash
cargo test --lib knowledge_pack_gate_ --verbose
```

The gate verifies manifest fields (including `spec.named_graph` /
`spec.vector_namespace` aligned with the seeded `ev-repair-fault-kb` pack),
pack-relative path containment (no `..`), frozen SHA-256 digests, ontology
anchors against `ev_repair_ontology()`, SHACL conformance of fictional sample
data, embedded Skill package manifests, audit sample schema, and equality with
the frozen golden output.

SHACL in this showcase targets **only** `ev:FaultCode` instances
(`shacl/fault-code-shapes.ttl`); other triples in `rdf/fictional-sample.ttl`
(e.g. fictional `ev:Brand`) are not shape-validated at pack gate time.

When golden input sets `validate_shacl: false`, the gate report uses
`shacl_conforms: "skipped"` instead of implying conformance.

## Optional load for demos

`./scripts/load-ev-repair-example-pack.sh` is a **stub**: it prints a notice and
runs the same filesystem gate as CI. It does **not** import into a live KB or
bind agents. Wiring import → gate → bind for the one-command demo (#416) is a
follow-up.

## Agent binding

Bind `knowledge_pack_ids` to `ev-repair-fault-kb` or the `ev-repair` runtime alias
on an Agent scoped with verified tenant/project claims. Generic chat stays
domain-neutral when the pack is not bound.
