# Ontology KE golden evaluation freeze policy

> 中文版：[22-ontology-ke-golden-freeze-policy.zh.md](22-ontology-ke-golden-freeze-policy.zh.md)

`tests/fixtures/ontology_ke_golden/` is a frozen kernel Graph Engineering
scorecard. It evaluates ontology-constrained extraction, canonicalization, and
the claims-scoped `KgQualityGate` introduced by [#140](https://github.com/skaiy/wild_agentos/issues/140).
It is not runtime data and must never be writable by an extractor, optimizer,
emergent loop, or deployment job.

## What is frozen

- `golden.json` contains source text, upstream candidates, a promoted miniature
  ontology, expected staging decisions, and deterministic `ASK` results.
- `SHA256SUMS` pins the fixture bytes. CI runs
  `scripts/check_ontology_ke_golden.sh` before the Rust suite.
- Required case IDs and the minimum case count prevent silent removal of hard
  negative cases.

The runtime and evaluation test load the fixture read-only with `include_str!`;
they have no fixture-writing path.

## Metrics and merge rule

`golden_ontology_ke_extract_canonicalize_and_gate_multi_metric` reports gates
for all of the following. No single accuracy score is a merge criterion:

1. ontology-conformance rate: expected canonical staging decisions match;
2. invalid-rejection rate: unpromoted types and invalid link domains are
   rejected; and
3. illegal production writes: exactly zero during extract/canonicalize/gate.

The quality gate runs only on claims-minted staging graphs. Its deterministic
`ASK` anchor is evaluated before optional Judge evidence, and no result in this
suite grants a production write.

## Changing the scorecard

Fixture changes are allowed only to correct a demonstrable defect or add a
harder, source-grounded case—not to remove difficult cases or lower the
thresholds. A pull request that changes this directory must:

1. check **Ontology KE golden fixture change reviewed** in the PR body;
2. add a concrete `Ontology KE golden fixture audit:` explanation describing
   the source-grounded reason, affected cases, and metric impact; and
3. receive review from two maintainers, including one owner of ontology/kernel
   governance.

CI rejects a fixture diff without the acknowledgement and explanation. The
maintainer review is deliberately a human control: a contributor who can edit
both a fixture and its checksum must not be able to self-authorize a simpler
scorecard.

## Slow measurement-decay audit

At least weekly, or before a release that changes extraction/canonicalization/
gate behavior, run:

```sh
./scripts/check_ontology_ke_golden.sh
cargo test --test ontology_ke_golden --verbose
```

Compare the fixture against `main`, inspect case count and required negative
cases, and record any fixture change under the PR audit field. The review asks
whether source provenance, ontology conformance, rejection behavior, and the
zero-production-write invariant are still measured. If a metric has become a
proxy for easy fixtures, add a harder case or revise the policy through the
same reviewed change process.
