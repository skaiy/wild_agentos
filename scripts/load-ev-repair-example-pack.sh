#!/usr/bin/env bash
# STUB: runs the filesystem admission gate only — same checks as CI
# (`check_ev_repair_example_pack.sh`). It does NOT import RDF into a tenant KB,
# mint claims-scoped graphs, or bind agents. Production load for the #416 demo
# (import → gate → bind) remains a follow-up.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "load-ev-repair-example-pack: stub — running pack gate only (no runtime import)." >&2
exec ./scripts/check_ev_repair_example_pack.sh
