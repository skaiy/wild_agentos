#!/usr/bin/env bash
# Validate the frozen ontology-KG golden suite and enforce its PR change policy.
set -euo pipefail

cd "$(dirname "$0")/.."
fixture_dir="tests/fixtures/ontology_ke_golden"

(cd "$fixture_dir" && sha256sum --check SHA256SUMS)

python3 - "$fixture_dir/golden.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as fixture:
    suite = json.load(fixture)

cases = suite.get("cases", [])
ids = {case.get("id") for case in cases}
required = set(suite.get("required_case_ids", []))
metrics = suite.get("metrics", {})

if suite.get("suite_version") != "ontology-ke-golden/v1":
    raise SystemExit("unsupported or missing ontology KE golden suite version")
if len(cases) < suite.get("minimum_case_count", 0):
    raise SystemExit("golden suite was reduced below its frozen minimum case count")
if missing := required - ids:
    raise SystemExit(f"golden suite is missing required anti-decay cases: {sorted(missing)}")
if not all(case.get("text", "").strip() and case.get("candidates") and case.get("expected")
           for case in cases):
    raise SystemExit("every golden case requires source text, candidates, and expected decisions")
if metrics.get("required_illegal_production_writes") != 0:
    raise SystemExit("frozen golden policy requires zero illegal production writes")
if metrics.get("minimum_ontology_conformance_rate", 0) <= 0:
    raise SystemExit("ontology conformance must remain a positive metric")
if metrics.get("minimum_invalid_rejection_rate", 0) <= 0:
    raise SystemExit("invalid rejection must remain a positive metric")
PY

# Fixture edits need a deliberate PR-body acknowledgement. This is intentionally
# evaluated only for PRs: maintainers may update the baseline on main after the
# documented two-reviewer process, while ordinary PRs must disclose the change.
if [[ "${GITHUB_EVENT_NAME:-}" == "pull_request" ]]; then
  base_ref="${ONTOLOGY_KE_GOLDEN_BASE_REF:?set ONTOLOGY_KE_GOLDEN_BASE_REF for pull requests}"
  changed="$(git diff --name-only "${base_ref}...HEAD" -- "$fixture_dir")"
  if [[ -n "$changed" ]]; then
    python3 - <<'PY'
import os
import re

body = os.environ.get("ONTOLOGY_KE_GOLDEN_PR_BODY", "")
checked = re.search(
    r"- \[[xX]\] Ontology KE golden fixture change reviewed", body
)
audit = re.search(
    r"Ontology KE golden fixture audit:\s*(?!N/?A\b|TODO\b|<!--)(\S.+)", body,
    re.IGNORECASE,
)
if not checked or not audit:
    raise SystemExit(
        "ontology KE golden fixtures changed: PR body must check "
        "'Ontology KE golden fixture change reviewed' and provide a non-placeholder "
        "'Ontology KE golden fixture audit:' explanation"
    )
PY
  fi
fi
